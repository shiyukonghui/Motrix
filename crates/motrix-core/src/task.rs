//! 任务模型与状态机模块（Phase 2 Task 8）
//!
//! 任务字段与 aria2 `tellStatus` 返回结构对齐，保证前端 `TaskItem.vue` / `TaskProgress.vue`
//! 等组件零改动；任务状态机与"引擎调用"解耦，由适配层在 KGet / librqbit 的进度事件与
//! 任务字段之间做转换（参考 MIGRATION-TAURI.md 5.2 节）。
//!
//! 本模块同时提供任务仓库 `TaskRepository`：维护 active / waiting / stopped 分组，
//! 供后续 Task 9（KGet 引擎）、Task 10（任务操作 command）、Task 12（会话持久化）对接。
//! 仓库为普通同步结构，外部以 `Arc<Mutex<TaskRepository>>` 包裹共享。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// 任务状态（与 aria2 状态机一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// 正在下载 / 上传（aria2: active）
    Active,
    /// 等待队列中（aria2: waiting）
    Waiting,
    /// 已暂停（aria2: paused）
    Paused,
    /// 出错（aria2: error）
    Error,
    /// 已完成（aria2: complete）
    Complete,
    /// 已移除（aria2: removed，记录在 stopped 历史）
    Removed,
    /// 做种中（aria2: seeding，BT 任务，Phase 3 使用）
    Seeding,
}

impl TaskStatus {
    /// 转为 aria2 状态串（tellStatus 的 status 字段）
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Active => "active",
            TaskStatus::Waiting => "waiting",
            TaskStatus::Paused => "paused",
            TaskStatus::Error => "error",
            TaskStatus::Complete => "complete",
            TaskStatus::Removed => "removed",
            TaskStatus::Seeding => "seeding",
        }
    }

    /// 从 aria2 状态串解析（未知串返回 None）
    pub fn from_str(s: &str) -> Option<TaskStatus> {
        match s {
            "active" => Some(TaskStatus::Active),
            "waiting" => Some(TaskStatus::Waiting),
            "paused" => Some(TaskStatus::Paused),
            "error" => Some(TaskStatus::Error),
            "complete" => Some(TaskStatus::Complete),
            "removed" => Some(TaskStatus::Removed),
            "seeding" => Some(TaskStatus::Seeding),
            _ => None,
        }
    }
}

/// 任务模块错误
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    /// 非法状态迁移（from -> to 不在状态机允许集合内）
    #[error("非法状态迁移: {from:?} -> {to:?}")]
    InvalidTransition { from: TaskStatus, to: TaskStatus },
    /// 任务不存在（gid 未注册）
    #[error("任务不存在: gid={gid}")]
    NotFound { gid: String },
}

/// 任务文件信息（对应 aria2 tellStatus 的 files[] 元素）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFile {
    /// 文件索引（从 1 开始）
    pub index: u32,
    /// 文件绝对路径
    pub path: String,
    /// 文件总长度（字节）
    pub length: u64,
    /// 已完成长度（字节）
    pub completed_length: u64,
    /// 是否选中下载（BT 多文件任务用）
    pub selected: bool,
    /// 下载源列表：(uri, 状态 used/waiting)
    pub uris: Vec<(String, String)>,
}

/// BT 元信息（对应 aria2 tellStatus 的 bittorrent 对象，Phase 3 由 librqbit 填充）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BittorrentInfo {
    /// 下载模式：single（单文件）/ multi（多文件）
    pub mode: String,
    /// 种子名称（bittorrent.info.name）。
    ///
    /// 磁力链接任务在 **metadata 获取前为 None**：此时 to_aria2() 省略 `info` 键，
    /// 前端 `isMagnetTask = (task) => bittorrent && !bittorrent.info` 据此判断
    /// （对应 MIGRATION-TAURI.md 5.5 节"磁力先以 metadata 任务呈现"）。
    pub info_name: Option<String>,
    /// announce 服务器列表（对应 announceList）
    pub announce_list: Vec<Vec<String>>,
    /// 信息哈希（对应顶层 infoHash 字段，可选）
    pub info_hash: Option<String>,
}

/// 下载任务（字段与 aria2 tellStatus 对齐）
#[derive(Debug, Clone)]
pub struct Task {
    /// 任务 id（16 位小写 hex，与 aria2 gid 一致）
    pub gid: String,
    /// 任务状态
    pub status: TaskStatus,
    /// 总长度（字节）
    pub total_length: u64,
    /// 已完成长度（字节）
    pub completed_length: u64,
    /// 已上传长度（字节，BT 任务用）
    pub upload_length: u64,
    /// 下载速度（字节/秒）
    pub download_speed: u64,
    /// 上传速度（字节/秒）
    pub upload_speed: u64,
    /// 保存目录
    pub dir: String,
    /// 文件列表
    pub files: Vec<TaskFile>,
    /// BT 元信息（HTTP 任务为 None）
    pub bittorrent: Option<BittorrentInfo>,
    /// 错误码（对应 aria2 errorCode）
    pub error_code: Option<i32>,
    /// 错误信息（对应 aria2 errorMessage）
    pub error_message: Option<String>,
    /// 当前连接数
    pub connections: u32,
    /// bitfield 十六进制串（HTTP 任务为空串；BT 由 Phase 3 填充分段 bitfield）
    pub bitfield: String,
    /// 做种者数量（BT 任务用）
    pub num_seeders: u32,
    /// 是否做种中（BT 任务用）
    pub seeder: bool,
    /// 创建时间（unix 秒，会话排序用）
    pub created_at: u64,
}

impl Task {
    /// 构造 HTTP 任务（total_length=0、status=Waiting，单文件）
    ///
    /// - `gid`: 任务 id（通常由 [`generate_gid`] 生成）
    /// - `uris`: 下载源列表（多 URL 即多源镜像）
    /// - `dir`: 保存目录
    /// - `out`: 保存文件名
    pub fn new_http_task(
        gid: impl Into<String>,
        uris: &[String],
        dir: impl Into<String>,
        out: impl Into<String>,
    ) -> Self {
        let dir = dir.into();
        let out = out.into();
        // 拼接保存路径：dir/out（兼容 dir 尾部带不带分隔符，统一用正斜杠）
        let path = if dir.ends_with('/') || dir.ends_with('\\') {
            format!("{dir}{out}")
        } else {
            format!("{dir}/{out}")
        };
        // 下载源初始状态均为 "used"（aria2 惯例：当前正在使用的 uri）
        let task_uris: Vec<(String, String)> = uris
            .iter()
            .map(|uri| (uri.clone(), "used".to_string()))
            .collect();

        Self {
            gid: gid.into(),
            status: TaskStatus::Waiting,
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            dir,
            files: vec![TaskFile {
                index: 1,
                path,
                length: 0,
                completed_length: 0,
                selected: true,
                uris: task_uris,
            }],
            bittorrent: None,
            error_code: None,
            error_message: None,
            connections: 0,
            bitfield: String::new(),
            num_seeders: 0,
            seeder: false,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    /// 构造 BT 任务（Phase 3：磁力 / 种子任务）
    ///
    /// - `gid`: 任务 id（通常由 [`generate_gid`] 生成）
    /// - `source`: BT 源（magnet 链接 或 base64 编码的 .torrent 内容），
    ///   存入 `files[0].uris[0]`，供 BT 引擎重新加入（checkpoint 恢复 / 续传）时取源；
    /// - `dir`: 保存目录
    /// - `info_hash`: 40 位十六进制信息哈希（磁力预解析 / 种子解析结果；未知时传 None，
    ///   由 metadata 就绪回调补填）
    ///
    /// 构造后 `bittorrent` 存在但 `info_name` 为 None（磁力 metadata 阶段前端据此
    /// 判定 isMagnetTask），total_length=0，由后续 metadata / 进度回调填充。
    pub fn new_bt_task(
        gid: impl Into<String>,
        source: &str,
        dir: impl Into<String>,
        info_hash: Option<String>,
    ) -> Self {
        let gid = gid.into();
        let dir = dir.into();
        let task = Self {
            gid: gid.clone(),
            status: TaskStatus::Waiting,
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            dir: dir.clone(),
            files: vec![TaskFile {
                index: 1,
                // BT 任务保存路径占位（metadata 就绪后按文件列表重建）；源写入 uris[0]
                path: format!("{dir}/"),
                length: 0,
                completed_length: 0,
                selected: true,
                uris: vec![(source.to_string(), "used".to_string())],
            }],
            bittorrent: Some(BittorrentInfo {
                // 占位 mode（metadata 就绪后按文件数更新为 single / multi）
                mode: "single".to_string(),
                info_name: None,
                announce_list: Vec::new(),
                info_hash,
            }),
            error_code: None,
            error_message: None,
            connections: 0,
            bitfield: String::new(),
            num_seeders: 0,
            seeder: false,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        task
    }

    /// 任务进度百分比（0.0 ~ 100.0；total_length 为 0 时返回 0）
    pub fn percent(&self) -> f64 {
        if self.total_length == 0 {
            return 0.0;
        }
        self.completed_length as f64 / self.total_length as f64 * 100.0
    }

    /// 状态机迁移合法性判定
    ///
    /// 允许的迁移集合（覆盖 HTTP 与 BT 全生命周期）：
    /// - active <-> waiting（运行 / 排队）
    /// - active <-> paused、waiting <-> paused（暂停 / 恢复）
    /// - active/waiting/paused -> complete / error（结束）
    /// - 任意非 removed 状态 -> removed（删除）
    /// - BT 做种相关：active -> seeding、complete -> seeding、seeding -> complete / removed
    pub fn can_transition(from: TaskStatus, to: TaskStatus) -> bool {
        use TaskStatus::*;
        matches!(
            (from, to),
            // 运行 <-> 排队
            (Active, Waiting) | (Waiting, Active)
            // 运行 <-> 暂停
            | (Active, Paused) | (Paused, Active)
            // 排队 <-> 暂停
            | (Waiting, Paused) | (Paused, Waiting)
            // 下载中任务结束：完成 / 出错（暂停的任务只能恢复或移除，不会直接结束）
            | (Active, Complete) | (Waiting, Complete)
            | (Active, Error) | (Waiting, Error)
            // 任意状态 -> 移除
            | (Active, Removed) | (Waiting, Removed) | (Paused, Removed)
            | (Complete, Removed) | (Error, Removed) | (Seeding, Removed)
            // BT 做种相关（Phase 3）：下载完成转做种、做种结束转完成
            | (Active, Seeding) | (Complete, Seeding) | (Seeding, Complete)
        )
    }

    /// 状态迁移（非法迁移返回 [`TaskError::InvalidTransition`]）
    pub fn transition(&mut self, new: TaskStatus) -> Result<(), TaskError> {
        if !Self::can_transition(self.status, new) {
            return Err(TaskError::InvalidTransition {
                from: self.status,
                to: new,
            });
        }
        self.status = new;
        Ok(())
    }

    /// 输出 aria2 `tellStatus` 兼容 JSON
    ///
    /// 键名保持 aria2 驼峰格式（gid/status/totalLength/…），**所有数值转成字符串**
    /// （aria2 惯例，前端 `Number()` 转换处不变）；files 元素字段
    /// index/length/completedLength/selected/path/uris 均为字符串。
    pub fn to_aria2(&self) -> Value {
        // 文件数组
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|f| {
                json!({
                    "index": f.index.to_string(),
                    "length": f.length.to_string(),
                    "completedLength": f.completed_length.to_string(),
                    "selected": f.selected.to_string(),
                    "path": f.path,
                    "uris": f.uris.iter().map(|(uri, status)| json!({
                        "uri": uri,
                        "status": status,
                    })).collect::<Vec<Value>>(),
                })
            })
            .collect();

        // BT 元信息（可选）：announceList / mode / info.name。
        // metadata 获取前（磁力任务）info_name 为 None → **省略 info 键**，
        // 前端 isMagnetTask 依据 `bittorrent && !bittorrent.info` 判断。
        let bittorrent = self.bittorrent.as_ref().map(|b| {
            let mut obj = serde_json::Map::new();
            obj.insert("announceList".to_string(), json!(b.announce_list));
            obj.insert("mode".to_string(), json!(b.mode));
            if let Some(name) = &b.info_name {
                obj.insert("info".to_string(), json!({ "name": name }));
            }
            Value::Object(obj)
        });
        // 错误信息：无错误时为 null，有错误时为字符串
        let error_code = self.error_code.map(|c| c.to_string());
        let error_message = self.error_message.clone();
        // 顶层 infoHash：仅 BT 任务存在（bittorrent.info_hash）
        let info_hash = self.bittorrent.as_ref().and_then(|b| b.info_hash.clone());

        json!({
            "gid": self.gid,
            "status": self.status.as_str(),
            "totalLength": self.total_length.to_string(),
            "completedLength": self.completed_length.to_string(),
            "uploadLength": self.upload_length.to_string(),
            "downloadSpeed": self.download_speed.to_string(),
            "uploadSpeed": self.upload_speed.to_string(),
            "connections": self.connections.to_string(),
            "dir": self.dir,
            "files": files,
            "errorCode": error_code,
            "errorMessage": error_message,
            "numSeeders": self.num_seeders.to_string(),
            "seeder": self.seeder.to_string(),
            "bitfield": self.bitfield,
            "infoHash": info_hash,
            "bittorrent": bittorrent,
        })
    }
}

/// gid 自增计数器（进程内保证每次生成不同）
static GID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成 16 位小写 hex gid（与 aria2 gid 格式一致）
///
/// 为避免新增依赖（rand），使用「时间戳微秒 + 进程内自增计数器 + 进程 id」
/// 经 FNV-1a 风格散列扩散后取 64 位格式化，保证同一进程内两次生成不同。
pub fn generate_gid() -> String {
    // 当前时间（微秒）
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    // 进程内自增计数（即使时间戳相同也能保证不同）
    let counter = GID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    // FNV-1a 散列扩散，避免低 16 位过于相似
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for v in [micros as u64, (micros >> 32) as u64, counter, pid] {
        hash ^= v;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", hash)
}

/// 移除历史保留上限（等价 aria2 内部 stopped 记录条数）
const MAX_REMOVED: usize = 100;

/// 任务仓库：维护任务全集 + 插入顺序 + 最近移除历史
///
/// 普通同步结构，外部以 `Arc<Mutex<TaskRepository>>` 包裹共享。
/// - `tasks` / `order`：在册任务（含 active/waiting/paused/error/complete/seeding）
/// - `removed`：最近移除的任务，供 stopped 列表展示（保留 [`MAX_REMOVED`] 条）
#[derive(Debug)]
pub struct TaskRepository {
    /// gid -> 任务
    tasks: HashMap<String, Task>,
    /// 任务插入顺序（gid 列表，用于 active/waiting/stopped 有序输出）
    order: Vec<String>,
    /// 最近移除的任务（供 stopped 列表）
    removed: Vec<Task>,
}

impl TaskRepository {
    /// 创建空仓库
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            order: Vec::new(),
            removed: Vec::new(),
        }
    }

    /// 添加任务（已存在则覆盖，并保持原有顺序）
    pub fn add(&mut self, task: Task) {
        let gid = task.gid.clone();
        if !self.order.contains(&gid) {
            self.order.push(gid.clone());
        }
        self.tasks.insert(gid, task);
    }

    /// 按 gid 查询任务
    pub fn get(&self, gid: &str) -> Option<&Task> {
        self.tasks.get(gid)
    }

    /// 按 gid 查询任务（可变引用；BT 进度回调 / metadata 就绪更新字段用）
    pub fn get_mut(&mut self, gid: &str) -> Option<&mut Task> {
        self.tasks.get_mut(gid)
    }

    /// 是否包含指定 gid 的任务
    pub fn contains(&self, gid: &str) -> bool {
        self.tasks.contains_key(gid)
    }

    /// 运行中任务列表（按插入顺序）
    pub fn active(&self) -> Vec<Task> {
        self.order
            .iter()
            .filter_map(|gid| self.tasks.get(gid))
            .filter(|t| t.status == TaskStatus::Active)
            .cloned()
            .collect()
    }

    /// 等待中任务列表（按插入顺序）
    pub fn waiting(&self) -> Vec<Task> {
        self.order
            .iter()
            .filter_map(|gid| self.tasks.get(gid))
            .filter(|t| t.status == TaskStatus::Waiting)
            .cloned()
            .collect()
    }

    /// 已停止任务列表（含 complete / error / removed 状态，按插入顺序；
    /// 最近移除的历史任务追加在末尾）
    pub fn stopped(&self) -> Vec<Task> {
        let mut result: Vec<Task> = self
            .order
            .iter()
            .filter_map(|gid| self.tasks.get(gid))
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Complete | TaskStatus::Error | TaskStatus::Removed
                )
            })
            .cloned()
            .collect();
        result.extend(self.removed.iter().cloned());
        result
    }

    /// 全部在册任务（不含 removed 历史；removed 历史经 [`Self::stopped`] 获取）
    pub fn all(&self) -> Vec<Task> {
        self.order
            .iter()
            .filter_map(|gid| self.tasks.get(gid))
            .cloned()
            .collect()
    }

    /// 设置任务状态（经状态机校验，非法迁移返回错误）
    pub fn set_status(&mut self, gid: &str, status: TaskStatus) -> Result<(), TaskError> {
        let task = self
            .tasks
            .get_mut(gid)
            .ok_or_else(|| TaskError::NotFound { gid: gid.to_string() })?;
        task.transition(status)
    }

    /// 更新进度与速度字段（引擎进度回调使用）
    pub fn update_progress(
        &mut self,
        gid: &str,
        completed: u64,
        total: u64,
        d_speed: u64,
        u_speed: u64,
        connections: u32,
    ) -> Result<(), TaskError> {
        let task = self
            .tasks
            .get_mut(gid)
            .ok_or_else(|| TaskError::NotFound { gid: gid.to_string() })?;
        task.completed_length = completed;
        task.total_length = total;
        task.download_speed = d_speed;
        task.upload_speed = u_speed;
        task.connections = connections;
        // 单文件任务：同步文件级进度，保证 aria2 tellStatus 的 files[].completedLength
        // 与任务级一致（多文件任务由各文件独立更新，此处不动）
        if task.files.len() == 1 {
            if let Some(file) = task.files.first_mut() {
                file.completed_length = completed;
                if total > 0 {
                    file.length = total;
                }
            }
        }
        Ok(())
    }

    /// 标记任务出错：状态按状态机迁移到 Error 并记录错误码 / 错误信息
    /// （已处于 Error 状态时仅更新错误信息，幂等）
    pub fn mark_error(&mut self, gid: &str, code: i32, msg: String) -> Result<(), TaskError> {
        let task = self
            .tasks
            .get_mut(gid)
            .ok_or_else(|| TaskError::NotFound { gid: gid.to_string() })?;
        if task.status != TaskStatus::Error {
            task.transition(TaskStatus::Error)?;
        }
        task.error_code = Some(code);
        task.error_message = Some(msg);
        Ok(())
    }

    /// 移除任务：移出 tasks 并加入 removed 历史（保留最近 [`MAX_REMOVED`] 条）
    ///
    /// 返回被移除的任务（保持原状态）；removed 历史中的副本状态标记为
    /// [`TaskStatus::Removed`]，供 stopped 列表展示。
    pub fn remove(&mut self, gid: &str) -> Option<Task> {
        let task = self.tasks.remove(gid)?;
        self.order.retain(|g| g != gid);
        // 记录移除历史：状态标记为 Removed
        let mut removed_task = task.clone();
        removed_task.status = TaskStatus::Removed;
        self.removed.push(removed_task);
        if self.removed.len() > MAX_REMOVED {
            self.removed.remove(0);
        }
        Some(task)
    }

    /// 清空 stopped 历史（等价 aria2.purgeDownloadResult；仅清理 removed 历史，
    /// 在册的 complete/error 任务由 [`Self::remove`] 显式移除后再进入历史）
    pub fn purge_removed(&mut self) {
        self.removed.clear();
    }

    /// 全局统计：active/waiting/stopped 数量 + 总下载/上传速度（仅统计运行中任务）
    pub fn global_stat(&self) -> GlobalStat {
        let mut download_speed = 0u64;
        let mut upload_speed = 0u64;
        let mut num_active = 0usize;
        let mut num_waiting = 0usize;
        for task in self.tasks.values() {
            match task.status {
                TaskStatus::Active => {
                    num_active += 1;
                    download_speed += task.download_speed;
                    upload_speed += task.upload_speed;
                }
                TaskStatus::Waiting => num_waiting += 1,
                _ => {}
            }
        }
        GlobalStat {
            download_speed,
            upload_speed,
            num_active,
            num_waiting,
            num_stopped: self.stopped().len(),
        }
    }
}

impl Default for TaskRepository {
    fn default() -> Self {
        Self::new()
    }
}

/// 全局统计（对应 aria2.getGlobalStat 返回，供 broadcaster / 托盘速度计使用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalStat {
    /// 全局下载速度（字节/秒）
    pub download_speed: u64,
    /// 全局上传速度（字节/秒）
    pub upload_speed: u64,
    /// 运行中任务数
    pub num_active: usize,
    /// 等待中任务数
    pub num_waiting: usize,
    /// 已停止任务数（含 complete / error / removed）
    pub num_stopped: usize,
}

impl GlobalStat {
    /// 输出 aria2 getGlobalStat 兼容 JSON（数值均为字符串）
    pub fn to_aria2_json(&self) -> Value {
        json!({
            "downloadSpeed": self.download_speed.to_string(),
            "uploadSpeed": self.upload_speed.to_string(),
            "numActive": self.num_active.to_string(),
            "numWaiting": self.num_waiting.to_string(),
            "numStopped": self.num_stopped.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 便捷构造单 URL 测试任务（gid 用固定串便于断言）
    fn task_with_gid(gid: &str) -> Task {
        let urls = vec!["https://example.com/a.zip".to_string()];
        Task::new_http_task(gid.to_string(), &urls, "/downloads", "a.zip")
    }

    // ------------------------------------------------------------------
    // 8.3.1 gid 生成：长度 16、小写 hex、两次生成不同
    // ------------------------------------------------------------------
    #[test]
    fn gid_is_16_hex_and_unique() {
        let a = generate_gid();
        let b = generate_gid();
        // 长度为 16
        assert_eq!(a.len(), 16);
        assert_eq!(b.len(), 16);
        // 全部为小写 hex 字符
        for gid in [&a, &b] {
            assert!(
                gid.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                "gid 包含非法字符: {gid}"
            );
        }
        // 两次生成不同
        assert_ne!(a, b);
    }

    // ------------------------------------------------------------------
    // 8.3.2 状态迁移：合法 / 非法迁移
    // ------------------------------------------------------------------
    #[test]
    fn status_transition_valid_and_invalid() {
        let mut task = task_with_gid("t1");
        assert_eq!(task.status, TaskStatus::Waiting);

        // 合法链路：waiting -> active -> paused -> active -> complete
        task.transition(TaskStatus::Active).unwrap();
        task.transition(TaskStatus::Paused).unwrap();
        task.transition(TaskStatus::Active).unwrap();
        task.transition(TaskStatus::Complete).unwrap();
        assert_eq!(task.status, TaskStatus::Complete);

        // 非法迁移：complete -> active 不允许
        let err = task.transition(TaskStatus::Active).unwrap_err();
        assert_eq!(
            err,
            TaskError::InvalidTransition {
                from: TaskStatus::Complete,
                to: TaskStatus::Active,
            }
        );
        // 迁移失败后状态保持不变
        assert_eq!(task.status, TaskStatus::Complete);

        // can_transition 对称断言
        assert!(Task::can_transition(TaskStatus::Active, TaskStatus::Paused));
        assert!(Task::can_transition(TaskStatus::Paused, TaskStatus::Active));
        assert!(Task::can_transition(TaskStatus::Waiting, TaskStatus::Paused));
        assert!(Task::can_transition(TaskStatus::Active, TaskStatus::Removed));
        assert!(Task::can_transition(TaskStatus::Complete, TaskStatus::Removed));
        assert!(Task::can_transition(TaskStatus::Error, TaskStatus::Removed));
        assert!(Task::can_transition(TaskStatus::Active, TaskStatus::Seeding));
        assert!(Task::can_transition(TaskStatus::Seeding, TaskStatus::Complete));
        assert!(!Task::can_transition(TaskStatus::Complete, TaskStatus::Active));
        assert!(!Task::can_transition(TaskStatus::Removed, TaskStatus::Active));
        assert!(!Task::can_transition(TaskStatus::Paused, TaskStatus::Complete));
    }

    #[test]
    fn task_status_str_roundtrip() {
        // as_str / from_str 互逆
        for status in [
            TaskStatus::Active,
            TaskStatus::Waiting,
            TaskStatus::Paused,
            TaskStatus::Error,
            TaskStatus::Complete,
            TaskStatus::Removed,
            TaskStatus::Seeding,
        ] {
            assert_eq!(TaskStatus::from_str(status.as_str()), Some(status));
        }
        // 未知串返回 None
        assert_eq!(TaskStatus::from_str("unknown"), None);
    }

    // ------------------------------------------------------------------
    // 8.3.3 to_aria2：JSON 形状（键名驼峰、数值为字符串、files/uris 结构）
    // ------------------------------------------------------------------
    #[test]
    fn to_aria2_shape_with_http_task() {
        let mut task = task_with_gid("0123456789abcdef");
        task.transition(TaskStatus::Active).unwrap();
        // 填充进度 / 速度 / 错误字段
        task.completed_length = 123;
        task.total_length = 456;
        task.upload_length = 999;
        task.download_speed = 1024;
        task.upload_speed = 2048;
        task.connections = 4;
        task.error_code = Some(3);
        task.error_message = Some("file not found".to_string());

        let v = task.to_aria2();
        let obj = v.as_object().expect("to_aria2 应输出 JSON 对象");

        // 键名齐全（aria2 驼峰格式）
        for key in [
            "gid",
            "status",
            "totalLength",
            "completedLength",
            "uploadLength",
            "downloadSpeed",
            "uploadSpeed",
            "connections",
            "dir",
            "files",
            "errorCode",
            "errorMessage",
            "numSeeders",
            "seeder",
            "bitfield",
            "infoHash",
            "bittorrent",
        ] {
            assert!(obj.contains_key(key), "缺少键: {key}");
        }

        // 标量字段：数值一律为字符串
        assert_eq!(obj["gid"], "0123456789abcdef");
        assert_eq!(obj["status"], "active");
        assert_eq!(obj["totalLength"], "456");
        assert_eq!(obj["completedLength"], "123");
        assert_eq!(obj["uploadLength"], "999");
        assert_eq!(obj["downloadSpeed"], "1024");
        assert_eq!(obj["uploadSpeed"], "2048");
        assert_eq!(obj["connections"], "4");
        assert_eq!(obj["dir"], "/downloads");
        assert_eq!(obj["errorCode"], "3");
        assert_eq!(obj["errorMessage"], "file not found");
        assert_eq!(obj["numSeeders"], "0");
        assert_eq!(obj["seeder"], "false");
        assert_eq!(obj["bitfield"], "");
        // HTTP 任务：无 BT 信息
        assert!(obj["infoHash"].is_null());
        assert!(obj["bittorrent"].is_null());

        // files 数组结构
        let files = obj["files"].as_array().expect("files 应为数组");
        assert_eq!(files.len(), 1);
        let f0 = &files[0];
        assert_eq!(f0["index"], "1");
        assert_eq!(f0["length"], "0");
        assert_eq!(f0["completedLength"], "0");
        assert_eq!(f0["selected"], "true");
        assert_eq!(f0["path"], "/downloads/a.zip");
        let uris = f0["uris"].as_array().expect("uris 应为数组");
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0]["uri"], "https://example.com/a.zip");
        assert_eq!(uris[0]["status"], "used");
    }

    #[test]
    fn to_aria2_with_bittorrent() {
        let mut task = task_with_gid("fedcba9876543210");
        // 下载完成后进入做种（waiting -> active -> seeding）
        task.transition(TaskStatus::Active).unwrap();
        task.transition(TaskStatus::Seeding).unwrap();
        task.bittorrent = Some(BittorrentInfo {
            mode: "multi".to_string(),
            info_name: Some("ubuntu-24.04".to_string()),
            announce_list: vec![
                vec!["udp://tracker1:80".to_string()],
                vec!["udp://tracker2:80".to_string()],
            ],
            info_hash: Some("aabbccddeeff00112233".to_string()),
        });
        task.num_seeders = 7;
        task.seeder = true;

        let v = task.to_aria2();
        assert_eq!(v["status"], "seeding");
        assert_eq!(v["numSeeders"], "7");
        assert_eq!(v["seeder"], "true");
        assert_eq!(v["infoHash"], "aabbccddeeff00112233");
        let bt = v["bittorrent"].as_object().expect("bittorrent 应为对象");
        assert_eq!(bt["mode"], "multi");
        assert_eq!(bt["info"]["name"], "ubuntu-24.04");
        assert_eq!(
            bt["announceList"],
            json!([["udp://tracker1:80"], ["udp://tracker2:80"]])
        );
    }

    #[test]
    fn to_aria2_magnet_metadata_pending_omits_info_key() {
        // 磁力 metadata 阶段（Phase 3）：bittorrent 存在但 info_name 为 None，
        // to_aria2 必须**省略 info 键**（前端 isMagnetTask 据此判断）
        let mut task = task_with_gid("magnet0000000001");
        task.transition(TaskStatus::Active).unwrap();
        task.bittorrent = Some(BittorrentInfo {
            mode: "single".to_string(),
            info_name: None,
            announce_list: Vec::new(),
            info_hash: Some("cafebabecafebabecafebabecafebabecafebabe".to_string()),
        });

        let v = task.to_aria2();
        let bt = v["bittorrent"].as_object().expect("bittorrent 应为对象");
        assert_eq!(bt["mode"], "single");
        assert_eq!(
            bt["announceList"],
            json!([]),
            "metadata 阶段 announceList 为空数组"
        );
        assert!(
            !bt.contains_key("info"),
            "metadata 阶段不应输出 info 键（前端 isMagnetTask 依赖）"
        );
        // 顶层 infoHash 仍可用（磁力链接预解析得到）
        assert_eq!(v["infoHash"], "cafebabecafebabecafebabecafebabecafebabe");
        // info 键补齐后（metadata 就绪）恢复输出
        task.bittorrent.as_mut().unwrap().info_name = Some("debian-12".to_string());
        let v2 = task.to_aria2();
        assert_eq!(v2["bittorrent"]["info"]["name"], "debian-12");
    }

    #[test]
    fn percent_helper() {
        let mut task = task_with_gid("p1");
        // total 为 0 时返回 0
        assert_eq!(task.percent(), 0.0);
        task.total_length = 100;
        task.completed_length = 25;
        assert_eq!(task.percent(), 25.0);
    }

    // ------------------------------------------------------------------
    // 8.3.4 任务仓库：add/get/active/waiting/stopped 分组、
    //         remove -> removed、global_stat 计数
    // ------------------------------------------------------------------
    #[test]
    fn repository_grouping_remove_and_stat() {
        let mut repo = TaskRepository::new();

        let gid_active = "aaaaaaaaaaaaaaaa";
        let gid_waiting = "bbbbbbbbbbbbbbbb";
        let gid_complete = "cccccccccccccccc";
        let gid_to_remove = "dddddddddddddddd";

        // 4 个任务：active、waiting、complete、active(待移除)
        let mut a = task_with_gid(gid_active);
        a.transition(TaskStatus::Active).unwrap();
        a.download_speed = 100;
        a.upload_speed = 10;

        let w = task_with_gid(gid_waiting); // 默认 waiting

        let mut c = task_with_gid(gid_complete);
        c.transition(TaskStatus::Complete).unwrap();

        let mut r = task_with_gid(gid_to_remove);
        r.transition(TaskStatus::Active).unwrap();
        r.download_speed = 200;

        repo.add(a);
        repo.add(w);
        repo.add(c);
        repo.add(r);

        // get / contains
        assert!(repo.contains(gid_active));
        assert_eq!(repo.get(gid_active).unwrap().status, TaskStatus::Active);
        assert!(repo.get("no-such-gid").is_none());

        // 分组：active 2 个（按插入顺序）、waiting 1 个、stopped 1 个（仅 complete）
        assert_eq!(repo.active().len(), 2);
        assert_eq!(repo.active()[0].gid, gid_active);
        assert_eq!(repo.active()[1].gid, gid_to_remove);
        assert_eq!(repo.waiting().len(), 1);
        assert_eq!(repo.waiting()[0].gid, gid_waiting);
        assert_eq!(repo.stopped().len(), 1);
        assert_eq!(repo.stopped()[0].gid, gid_complete);
        assert_eq!(repo.all().len(), 4);

        // set_status：waiting -> paused（合法）；complete -> active（非法）
        repo.set_status(gid_waiting, TaskStatus::Paused).unwrap();
        assert_eq!(
            repo.get(gid_waiting).unwrap().status,
            TaskStatus::Paused
        );
        assert!(repo.set_status(gid_complete, TaskStatus::Active).is_err());
        // 不存在的任务
        assert_eq!(
            repo.set_status("no-such-gid", TaskStatus::Active).unwrap_err(),
            TaskError::NotFound {
                gid: "no-such-gid".to_string()
            }
        );

        // update_progress
        repo.update_progress(gid_active, 50, 100, 512, 0, 4).unwrap();
        let t = repo.get(gid_active).unwrap();
        assert_eq!(t.completed_length, 50);
        assert_eq!(t.total_length, 100);
        assert_eq!(t.download_speed, 512);
        assert_eq!(t.connections, 4);

        // mark_error：先恢复为 active，再标记错误（paused 任务只会恢复或移除，不会直接出错）
        repo.set_status(gid_waiting, TaskStatus::Active).unwrap();
        repo.mark_error(gid_waiting, 1, "timeout".to_string()).unwrap();
        let t = repo.get(gid_waiting).unwrap();
        assert_eq!(t.status, TaskStatus::Error);
        assert_eq!(t.error_code, Some(1));
        assert_eq!(t.error_message.as_deref(), Some("timeout"));

        // remove：移出 tasks、进入 removed 历史、stopped 追加
        let removed_task = repo.remove(gid_to_remove).unwrap();
        assert_eq!(removed_task.gid, gid_to_remove);
        assert!(!repo.contains(gid_to_remove));
        assert_eq!(repo.active().len(), 1);
        // stopped = 在册 complete(1) + 在册 error(1) + removed 历史(1)
        assert_eq!(repo.stopped().len(), 3);
        assert!(
            repo.stopped()
                .iter()
                .any(|t| t.gid == gid_to_remove && t.status == TaskStatus::Removed)
        );
        // 重复 remove 返回 None
        assert!(repo.remove(gid_to_remove).is_none());

        // global_stat 计数与速度
        let stat = repo.global_stat();
        assert_eq!(stat.num_active, 1);
        assert_eq!(stat.num_waiting, 0);
        assert_eq!(stat.num_stopped, 3);
        assert_eq!(stat.download_speed, 512);
        assert_eq!(stat.upload_speed, 0);

        // purge_removed：清空 removed 历史后 stopped 不再含已移除任务
        repo.purge_removed();
        assert_eq!(repo.stopped().len(), 2);
        assert!(!repo.stopped().iter().any(|t| t.gid == gid_to_remove));
    }

    #[test]
    fn global_stat_to_aria2_json() {
        let stat = GlobalStat {
            download_speed: 100,
            upload_speed: 50,
            num_active: 2,
            num_waiting: 3,
            num_stopped: 4,
        };
        let v = stat.to_aria2_json();
        assert_eq!(v["downloadSpeed"], "100");
        assert_eq!(v["uploadSpeed"], "50");
        assert_eq!(v["numActive"], "2");
        assert_eq!(v["numWaiting"], "3");
        assert_eq!(v["numStopped"], "4");
    }
}
