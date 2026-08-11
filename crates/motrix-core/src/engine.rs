//! 任务编排胶水层（Phase 2 Task 10）
//!
//! [`TaskManager`] 是"任务编排 + aria2 契约适配"胶水层（**非下载算法**，
//! 下载由 KGet 引擎完成，见 kget.rs）：持有任务仓库（[`TaskRepository`]）与
//! KGet 引擎句柄，负责：
//!
//! - **添加任务**（[`TaskManager::add_uri`]）：建任务 → 探总长（尽力而为）→
//!   受 `max-concurrent-downloads` 限制直接启动或入 waiting 队列；
//! - **并发队列**：完成任务 / 失败 / 暂停后自动 promote 最早等待任务
//!   （[`TaskManager::promote_waiting`]）；
//! - **暂停 / 恢复**：暂停 = abort 引擎句柄 + 标 Paused（KGet 保留部分文件）；
//!   恢复 = 重新 spawn（KGet 基于 HTTP Range 断点续传）；
//! - **删除 / 选项**：删除 = abort + 移除仓库记录；任务级 / 全局选项即时更新，
//!   全局选项持久化到 system.json；
//! - **进度换算**：KGet 事件只有 `percent`（0~100，无字节数，speed 恒 0），
//!   按 `completed = total * percent / 100` 换算 completedLength；total 未知
//!   （探测失败）时仅维持速度统计、不写 completed。
//!
//! 参考 MIGRATION-TAURI.md 5.1 / 5.3 / 5.4 / 5.8 节与 tasks.md Task 10。
//!
//! ## 线程模型
//!
//! 所有公开方法均为 `&self`，内部用 Mutex 保护各自状态，任何时刻最多持有一个
//! 锁（避免死锁）；KGet 引擎在独立 std 线程中运行，通过 `on_event` 回调回到
//! 本管理器（回调捕获 `Arc<TaskManager>` 自引用，构造后须调用
//! [`TaskManager::set_self`] 注入）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tracing::warn;

use crate::config::{ConfigManager, SystemConfig};
use crate::kget::{self, KgetEvent, KgetHandle};
use crate::options::{parse_size, EngineOptions};
use crate::task::{generate_gid, GlobalStat, Task, TaskRepository, TaskStatus};

/// 任务状态变化事件（engine:task-event 载荷来源，Task 11）
///
/// `event` 取值与前端 EngineClient.vue 的事件分发一一对应：
/// - `start`：下载开始（add_uri 启动 / resume 恢复）
/// - `pause`：任务暂停
/// - `stop`：任务被移除
/// - `complete`：下载完成
/// - `error`：下载失败
/// - `bt-complete`：BT 做种完成（Phase 3 提供，本阶段不发送）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskEvent {
    /// 任务 gid
    pub gid: String,
    /// 事件类型（start/stop/pause/complete/error/bt-complete）
    pub event: String,
}

/// 任务管理器（任务编排胶水层）
pub struct TaskManager {
    /// 任务仓库（与 Tauri AppState / 广播循环 / JSON-RPC 共享同一份）
    pub repo: Arc<Mutex<TaskRepository>>,
    /// 配置管理器（change_global_option 时把全局选项持久化到 system.json）
    config_manager: Arc<Mutex<ConfigManager>>,
    /// 全局选项（启动时从 system.json 初始化，change_global_option 更新；
    /// 新任务以此为基础克隆后叠加任务级 options）
    global: Mutex<EngineOptions>,
    /// 运行中任务 gid -> 引擎句柄（暂停 / 删除时 abort；任务结束/失败时移除）
    active: Mutex<HashMap<String, KgetHandle>>,
    /// gid -> 源 URL 列表（恢复 / 重试用，spawn 时取 uris[0] 发起下载）
    task_uris: Mutex<HashMap<String, Vec<String>>>,
    /// gid -> 任务级引擎选项（change_option 更新，spawn 时使用）
    task_options: Mutex<HashMap<String, EngineOptions>>,
    /// gid -> 上次速度采样的 (已完成字节, 采样时刻)
    ///
    /// KGet 的 Progress 事件不带实时速度（speed 恒 0），由 handle_progress
    /// 按"进度差值 / 时间差"自算瞬时速度（每 100ms 采样一次）。暂停 /
    /// 失败 / 完成 / 移除任务时移除对应记录，保证恢复后重新采样。
    speed_samples: Mutex<HashMap<String, (u64, std::time::Instant)>>,
    /// 最大并发下载数（对应 max-concurrent-downloads，最小 1）
    max_concurrent: Mutex<u32>,
    /// 自引用（Weak）：引擎线程回调需要升级为 Arc<TaskManager> 再调用方法
    self_arc: Mutex<Option<Weak<TaskManager>>>,
    /// 任务状态变化事件通道（tokio broadcast：send 不 await，同步上下文可直接发送；
    /// 容量 256 保留最近历史，迟到的订阅者可追到最近事件；无订阅者时 send 失败静默忽略）
    events: tokio::sync::broadcast::Sender<TaskEvent>,
}

impl TaskManager {
    /// 构造任务管理器
    ///
    /// - `system`：系统配置（system.json），初始化全局选项与最大并发数
    /// - `config_manager`：配置管理器（change_global_option 持久化用）
    ///
    /// 构造后须调用 [`TaskManager::set_self`] 注入自引用（`Arc::downgrade`），
    /// 否则引擎线程回调升级 Arc 时会 panic。
    pub fn new(system: &SystemConfig, config_manager: Arc<Mutex<ConfigManager>>) -> Self {
        let global = EngineOptions::from_system_config(system);
        // 最大并发数至少为 1（aria2 语义：0 视为未设置，取默认 1 保证可下载）
        let max_concurrent = system.max_concurrent_downloads.max(1);
        // 事件通道：容量 256（保留最近 256 条事件历史，订阅者启动稍晚也能追到近期事件）
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            repo: Arc::new(Mutex::new(TaskRepository::new())),
            config_manager,
            global: Mutex::new(global),
            active: Mutex::new(HashMap::new()),
            task_uris: Mutex::new(HashMap::new()),
            task_options: Mutex::new(HashMap::new()),
            speed_samples: Mutex::new(HashMap::new()),
            max_concurrent: Mutex::new(max_concurrent),
            self_arc: Mutex::new(None),
            events,
        }
    }

    /// 注入自引用（构造后由持有方调用一次：`task_manager.set_self(Arc::downgrade(&task_manager))`）
    pub fn set_self(&self, this: Weak<TaskManager>) {
        if let Ok(mut guard) = self.self_arc.lock() {
            *guard = Some(this);
        }
    }

    /// 订阅任务状态变化事件（返回 broadcast Receiver，由 src-tauri 的 async 循环
    /// 接收并转发为 `engine:task-event` 事件给前端）
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }

    /// 发送任务状态变化事件（broadcast send 不 await，同步上下文可直接调用；
    /// 无订阅者时 send 返回 Err，静默忽略，不影响任务操作）
    fn emit_event(&self, gid: &str, event: &str) {
        let _ = self.events.send(TaskEvent {
            gid: gid.to_string(),
            event: event.to_string(),
        });
    }

    /// 升级自引用为 Arc<TaskManager>（引擎线程回调使用；未 set_self 时 panic）
    fn arc_this(&self) -> Arc<TaskManager> {
        self.self_arc
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("TaskManager 自引用未设置（请先调用 set_self）")
    }

    // ==================================================================
    // 任务操作（公开 API，供 Tauri commands / JSON-RPC 后端调用）
    // ==================================================================

    /// 添加 HTTP/FTP URL 任务（aria2.addUri 语义）
    ///
    /// 每个 URL 创建一个独立任务并返回对应 gid 列表：
    /// 1. 以全局选项为基准克隆 + 叠加任务级 `options`（dir/out 取任务级或全局）；
    /// 2. HTTP(S) URL 尽力探测 Content-Length 填充 `total_length`
    ///    （探测失败返回 0，不阻塞任务，进度由 UI 按 percent 换算）；
    /// 3. 加入任务仓库并记录 task_uris / task_options；
    /// 4. 若当前运行数 < max-concurrent-downloads 则置 Active 并启动引擎，
    ///    否则保持 Waiting 入队（完成任务 / 失败后自动 promote）。
    pub fn add_uri(&self, uris: &[String], options: &Value) -> Result<Vec<String>, String> {
        if uris.is_empty() {
            return Err("add_uri 需要一个非空的 URL 数组".to_string());
        }
        // 基准全局选项（克隆；任务级 options 在其上叠加）
        let base = self
            .global
            .lock()
            .map_err(|e| format!("获取全局选项锁失败: {e}"))?
            .clone();
        let mut gids: Vec<String> = Vec::with_capacity(uris.len());
        for uri in uris {
            let uri = uri.trim();
            if uri.is_empty() {
                continue;
            }
            // 任务级引擎选项：全局 + 任务级覆盖
            let mut task_opts = base.clone();
            task_opts.apply_task_options(options);
            // 保存目录 / 文件名：优先任务级 options，否则用全局 dir / URL 推断
            let dir = task_opts.dir.clone();
            let out = task_opts
                .out
                .clone()
                .unwrap_or_else(|| file_name_from_uri(uri));
            // 探总长（仅 HTTP(S)，尽力而为；失败返回 0 不阻塞）
            let total = if uri.starts_with("http://") || uri.starts_with("https://") {
                crate::kget::probe_content_length(uri, &task_opts).unwrap_or(0)
            } else {
                0
            };
            // 建任务（status 默认 Waiting）并填充探测到的总长
            let gid = generate_gid();
            let mut task = Task::new_http_task(gid.clone(), &[uri.to_string()], dir, out);
            task.total_length = total;
            if let Some(file) = task.files.first_mut() {
                file.length = total;
            }
            // 加入仓库 + 记录源 URL / 任务级选项
            self.repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?
                .add(task);
            self.task_uris
                .lock()
                .map_err(|e| format!("获取源 URL 表锁失败: {e}"))?
                .insert(gid.clone(), vec![uri.to_string()]);
            self.task_options
                .lock()
                .map_err(|e| format!("获取任务选项锁失败: {e}"))?
                .insert(gid.clone(), task_opts);
            gids.push(gid);
        }
        if gids.is_empty() {
            return Err("add_uri 的 URL 列表均为空".to_string());
        }
        // 统一调度：槽位有空则启动最早 Waiting（含刚添加的任务）
        self.promote_waiting();
        Ok(gids)
    }

    /// 暂停任务（aria2.pause）：abort 引擎句柄（若在运行）+ 置 Paused
    ///
    /// 暂停语义：KGet 的 abort 置位取消令牌，下载在下一个读写检查点停止，
    /// 已下载的部分文件保留（供恢复时 Range 续传）；等待引擎线程退出后任务置
    /// Paused。对 waiting / 已 paused 任务幂等返回 "OK"；complete / error 等
    /// 终态任务不可暂停，返回错误。
    pub fn pause(&self, gid: &str) -> Result<String, String> {
        self.pause_inner(gid)
    }

    /// 强制暂停（aria2.forcePause）：与 [`TaskManager::pause`] 行为一致
    /// （KGet 的 abort 本身就是强停，无需区分普通 / 强制）
    pub fn force_pause(&self, gid: &str) -> Result<String, String> {
        self.pause_inner(gid)
    }

    /// pause / force_pause 共用实现
    fn pause_inner(&self, gid: &str) -> Result<String, String> {
        // 1. abort 并"有限等待"引擎线程退出（KGet 引擎可能阻塞在网络读取，
        //    不能无限 join（reqwest 内部超时 300s）；超时后直接继续，
        //    引擎线程稍后自行退出，迟到的 Finished/Failed 事件因状态已变会被忽略）
        if let Some(handle) = self
            .active
            .lock()
            .map_err(|e| format!("获取运行集合锁失败: {e}"))?
            .remove(gid)
        {
            handle.abort();
            let _ = handle.wait_exit(std::time::Duration::from_millis(2000));
        }
        // 2. 清理 KGet 预分配可能留下的"假文件"（见 cleanup_partial_file 注释），
        //    保证恢复时不会误判为已下载完成
        self.cleanup_partial_file(gid);
        // 2.5 清理速度采样快照（引擎已停止，暂停期间速度无意义；独立短锁，
        //    恢复时从头重新采样）
        if let Ok(mut samples) = self.speed_samples.lock() {
            samples.remove(gid);
        }
        // 3. 状态迁移：active / waiting → paused；paused 幂等
        {
            let mut repo = self
                .repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
            let status = repo
                .get(gid)
                .map(|t| t.status)
                .ok_or_else(|| format!("任务不存在: {gid}"))?;
            match status {
                TaskStatus::Active | TaskStatus::Waiting => {
                    repo.set_status(gid, TaskStatus::Paused)
                        .map_err(|_| format!("任务不存在: {gid}"))?;
                    // Task 11：暂停事件（前端 onDownloadPause 触发暂停 toast）
                    self.emit_event(gid, "pause");
                }
                // 已暂停：幂等
                TaskStatus::Paused => {}
                // 终态任务不可暂停
                _ => return Err(format!("无法暂停任务（当前状态不支持）: gid={gid}")),
            }
        }
        // 4. 暂停可能释放了并发槽位，唤醒下一个等待任务
        self.promote_waiting();
        Ok("OK".to_string())
    }

    /// 清理中断下载留下的部分文件（KGet 预分配陷阱处理）
    ///
    /// KGet 的 `AdvancedDownloader` 在服务器支持 Range 时会先把输出文件
    /// `set_len(total)` 预分配为完整大小（见 KGet 1.7 advanced_download.rs），
    /// 因此**暂停中断后文件大小恒等于任务 total**，恢复时 KGet 会误判
    /// "已下载完成"而直接跳过，留下全 0 / 空洞文件。
    ///
    /// 处理策略（正确性优先）：
    /// - 文件大小 >= total 且任务并未真正完成（completed < total）→ 这是
    ///   预分配产生的"假文件"，删除它，恢复时从头下载（KGet 1.7 并行分段
    ///   预分配使其正可靠续传不可行，详见 kget.rs 模块注释）；
    /// - 文件大小 < total → 真实的部分文件（服务器不支持 Range 时 KGet
    ///   不预分配），保留，恢复时由 KGet 尝试续传。
    fn cleanup_partial_file(&self, gid: &str) {
        // 读取任务信息（克隆，避免持锁调用文件系统）
        let (path, total, completed) = {
            let repo = match self.repo.lock() {
                Ok(repo) => repo,
                Err(_) => return,
            };
            let Some(task) = repo.get(gid) else { return };
            let path = task
                .files
                .first()
                .map(|f| f.path.clone())
                .unwrap_or_default();
            (path, task.total_length, task.completed_length)
        };
        // total 未知（探测失败）或文件不存在：无需处理
        if total == 0 || path.is_empty() {
            return;
        }
        let Ok(meta) = std::fs::metadata(&path) else { return };
        // 文件大小 >= total 且未真正下载完成 → 预分配"假文件"，删除从头下载
        if meta.len() >= total && completed < total {
            // Windows 下引擎线程可能仍持有文件句柄，删除失败时忽略
            // （恢复时若仍误判完成，任务会直接进入 Complete，属 KGet 库缺陷的兜底场景）
            if std::fs::remove_file(&path).is_ok() {
                warn!("[Motrix] 清理 KGet 预分配残留文件（暂停后恢复将重新下载）: {path}");
            }
        }
    }

    /// 恢复任务（aria2.unpause）：置 Active 并重新 spawn（KGet 基于 Range 续传）
    ///
    /// 仍受 `max-concurrent-downloads` 限制：并发槽位已满时回到 waiting 队列；
    /// 对已运行 / 排队任务幂等返回 "OK"；complete / error 等终态不可恢复。
    pub fn resume(&self, gid: &str) -> Result<String, String> {
        // 1. 校验任务状态
        let status = {
            let repo = self
                .repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
            repo.get(gid)
                .map(|t| t.status)
                .ok_or_else(|| format!("任务不存在: {gid}"))?
        };
        match status {
            // 已运行 / 排队：幂等
            TaskStatus::Active | TaskStatus::Waiting => return Ok("OK".to_string()),
            // 已暂停 → 恢复
            TaskStatus::Paused => {}
            // 终态任务不可恢复
            _ => return Err(format!("无法恢复任务（当前状态不支持）: gid={gid}")),
        }
        // 2. 检查并发槽位：满则回到 waiting 队列（等待其它任务完成后自动 promote）
        let max = *self
            .max_concurrent
            .lock()
            .map_err(|e| format!("获取并发配置锁失败: {e}"))?;
        let num_active = self
            .active
            .lock()
            .map_err(|e| format!("获取运行集合锁失败: {e}"))?
            .len();
        if num_active >= max as usize {
            let mut repo = self
                .repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
            repo.set_status(gid, TaskStatus::Waiting)
                .map_err(|_| format!("任务不存在: {gid}"))?;
            return Ok("OK".to_string());
        }
        // 3. 置 Active 并重新 spawn（KGet 检测已有部分文件，Range 续传）
        {
            let mut repo = self
                .repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
            repo.set_status(gid, TaskStatus::Active)
                .map_err(|_| format!("任务不存在: {gid}"))?;
        }
        if let Err(e) = self.spawn_one(gid) {
            // 启动失败（如输出目录不可写）：标记错误并返回
            if let Ok(mut repo) = self.repo.lock() {
                let _ = repo.mark_error(gid, 1, e.clone());
            }
            return Err(e);
        }
        Ok("OK".to_string())
    }

    /// 移除任务（aria2.remove / forceRemove）：abort 引擎句柄 + 移除仓库记录
    ///
    /// 已下载的部分文件保留（是否删除文件由上层决定）；移除后释放并发槽位，
    /// 自动 promote 下一个等待任务。任务不存在返回错误。
    pub fn remove(&self, gid: &str) -> Result<String, String> {
        // 1. abort 并"有限等待"引擎线程退出（避免移除后引擎仍在写文件；
        //    卡在网络读取时超时后继续，线程稍后自行退出）
        if let Some(handle) = self
            .active
            .lock()
            .map_err(|e| format!("获取运行集合锁失败: {e}"))?
            .remove(gid)
        {
            handle.abort();
            let _ = handle.wait_exit(std::time::Duration::from_millis(2000));
        }
        // 2. 清理任务级记录
        self.task_uris
            .lock()
            .map_err(|e| format!("获取源 URL 表锁失败: {e}"))?
            .remove(gid);
        self.task_options
            .lock()
            .map_err(|e| format!("获取任务选项锁失败: {e}"))?
            .remove(gid);
        // 清理速度采样快照（独立短锁，不嵌套 repo 锁）
        if let Ok(mut samples) = self.speed_samples.lock() {
            samples.remove(gid);
        }
        // 3. 从仓库移除（进入 removed 历史，供 stopped 列表展示）
        {
            let mut repo = self
                .repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
            if repo.remove(gid).is_none() {
                return Err(format!("任务不存在: {gid}"));
            }
            // Task 11：移除事件（前端 onDownloadStop 触发移除 toast）
            self.emit_event(gid, "stop");
        }
        // 4. 移除可能释放了并发槽位，唤醒下一个等待任务
        self.promote_waiting();
        Ok("OK".to_string())
    }

    /// 修改任务级选项（aria2.changeOption）
    ///
    /// 说明：运行中任务的选项在**下次恢复（resume / 重试）时生效**；
    /// 已暂停任务更新后**不会自动重启**（保持暂停状态），符合 aria2 语义。
    pub fn change_option(&self, gid: &str, options: &Value) -> Result<String, String> {
        let mut task_options = self
            .task_options
            .lock()
            .map_err(|e| format!("获取任务选项锁失败: {e}"))?;
        let mut opts = task_options
            .get(gid)
            .cloned()
            .ok_or_else(|| format!("任务不存在: {gid}"))?;
        // 任务级 options 覆盖（与 add_uri 同一套映射）
        opts.apply_task_options(options);
        task_options.insert(gid.to_string(), opts);
        Ok("OK".to_string())
    }

    /// 修改全局选项（aria2.changeGlobalOption）
    ///
    /// - 更新内存全局选项（新任务以此为基准）与最大并发数；
    /// - 持久化到 system.json（经配置管理器；数值键字符串 → 数字保证类型匹配）；
    /// - 已运行 / 等待中的任务不受影响（各自持有任务级选项快照）。
    pub fn change_global_option(&self, options: &Value) -> Result<String, String> {
        // 1. 更新内存全局选项（新任务以此为基准克隆）
        {
            let mut global = self
                .global
                .lock()
                .map_err(|e| format!("获取全局选项锁失败: {e}"))?;
            global.apply_task_options(options);
        }
        // 2. max-concurrent-downloads 单独处理（不在 apply_task_options 映射表内）
        if let Some(v) = options.get("max-concurrent-downloads") {
            let n = v
                .as_u64()
                .or_else(|| v.as_str().and_then(parse_size));
            if let Some(n) = n {
                *self
                    .max_concurrent
                    .lock()
                    .map_err(|e| format!("获取并发配置锁失败: {e}"))? = n.max(1) as u32;
            }
        }
        // 3. 持久化到 system.json（失败仅告警，内存配置已生效）
        let patch = normalize_system_patch(options);
        match self.config_manager.lock() {
            Ok(mut cm) => {
                if let Err(e) = cm.apply_preference(None, Some(&patch)) {
                    warn!("[Motrix] change_global_option 持久化失败（内存配置已生效）: {e}");
                }
            }
            Err(e) => warn!("[Motrix] change_global_option 获取配置管理器锁失败，跳过持久化: {e}"),
        }
        Ok("OK".to_string())
    }

    /// 添加 BitTorrent 种子 / 磁力任务（aria2.addTorrent）
    ///
    /// BT 下载支持属 **Phase 3**（librqbit 集成），本次不创建任务、不启动引擎，
    /// 明确返回占位错误，避免前端误以为任务已建立。
    pub fn add_torrent(&self, _torrent: &str, _options: &Value) -> Result<String, String> {
        Err("BitTorrent 支持将在 Phase 3 提供".to_string())
    }

    /// 按 gid 查询任务（克隆返回，避免调用方持锁）
    pub fn get(&self, gid: &str) -> Option<Task> {
        self.repo
            .lock()
            .ok()
            .and_then(|repo| repo.get(gid).cloned())
    }

    /// 清空已移除任务历史（等价 aria2.purgeDownloadResult）
    ///
    /// 仅清理 removed 历史（stopped 列表），在册的 active/waiting/complete 等任务不受影响。
    pub fn purge(&self) {
        if let Ok(mut repo) = self.repo.lock() {
            repo.purge_removed();
        }
    }

    /// 全局统计（aria2.getGlobalStat）：active / waiting / stopped 数量 + 总速度
    pub fn global_stat(&self) -> GlobalStat {
        self.repo
            .lock()
            .map(|repo| repo.global_stat())
            .unwrap_or_else(|_| GlobalStat {
                download_speed: 0,
                upload_speed: 0,
                num_active: 0,
                num_waiting: 0,
                num_stopped: 0,
            })
    }

    // ==================================================================
    // 内部实现：并发队列调度与引擎事件处理
    // ==================================================================

    /// 在 max-concurrent-downloads 限制内，把最早 Waiting 任务置 Active 并启动引擎
    ///
    /// 幂等：槽位已满 / 无等待任务时立即返回。每次循环只处理一个任务，
    /// 依次获取锁（不嵌套），spawn 失败的任务标记 Error 后继续尝试下一个。
    pub fn promote_waiting(&self) {
        loop {
            // 1. 槽位检查（max_concurrent 与 active 均为短锁，先读后释放）
            let max = match self.max_concurrent.lock() {
                Ok(guard) => *guard,
                Err(e) => {
                    warn!("[Motrix] promote_waiting 获取并发配置锁失败: {e}");
                    return;
                }
            };
            let num_active = match self.active.lock() {
                Ok(guard) => guard.len(),
                Err(e) => {
                    warn!("[Motrix] promote_waiting 获取运行集合锁失败: {e}");
                    return;
                }
            };
            if num_active >= max.max(1) as usize {
                return;
            }
            // 2. 取最早 Waiting 任务
            let next = {
                let repo = match self.repo.lock() {
                    Ok(repo) => repo,
                    Err(e) => {
                        warn!("[Motrix] promote_waiting 获取任务仓库锁失败: {e}");
                        return;
                    }
                };
                repo.waiting().first().map(|t| t.gid.clone())
            };
            let Some(gid) = next else { return };
            // 3. 置 Active（状态机校验：waiting -> active 合法）
            {
                let mut repo = match self.repo.lock() {
                    Ok(repo) => repo,
                    Err(e) => {
                        warn!("[Motrix] promote_waiting 获取任务仓库锁失败: {e}");
                        return;
                    }
                };
                if repo.set_status(&gid, TaskStatus::Active).is_err() {
                    // 理论上不会发生（waiting -> active 恒合法）；防御性退出
                    return;
                }
            }
            // 4. 启动引擎；失败则标记 Error 并继续尝试下一个等待任务
            if let Err(e) = self.spawn_one(&gid) {
                if let Ok(mut repo) = self.repo.lock() {
                    let _ = repo.mark_error(&gid, 1, e);
                }
                continue;
            }
        }
    }

    /// 启动单个任务的 KGet 引擎（任务须已置 Active）
    ///
    /// 取源 URL（uris[0]）+ 任务级选项调用 `kget::spawn_download`，
    /// 事件回调捕获 `Arc<TaskManager>`（经 self_arc 升级），在引擎线程内同步调用。
    ///
    /// **checkpoint 恢复任务兜底**（Task 13 续传边界修复）：Task 12 经
    /// `session.rs::restore_checkpoint` 恢复的任务只进了任务仓库，**没有**登记进
    /// `task_uris` / `task_options` 内部表（与 `add_uri` 的任务不同），因此这里在
    /// 内部表缺失时兜底：
    /// - 源 URL 从仓库任务 `files[0].uris` 取第一个 uri（.0），恢复任务走 files
    ///   兜底 URL 以支持 checkpoint 续传；files 也为空时返回明确错误；
    /// - 引擎选项以全局选项为基准克隆，并把 dir / out 覆盖为任务的保存路径
    ///   （保证下载写回 checkpoint 记录的目录 / 文件名，Range 续传基于已有文件）。
    fn spawn_one(&self, gid: &str) -> Result<(), String> {
        // 1. 取源 URL 与任务级选项（先查内部表，缺失时走仓库任务 / 全局选项兜底；
        //    各锁分开持有，先释放再启动）
        let (url, options) = {
            let uris = self
                .task_uris
                .lock()
                .map_err(|e| format!("获取源 URL 表锁失败: {e}"))?;
            let opts = self
                .task_options
                .lock()
                .map_err(|e| format!("获取任务选项锁失败: {e}"))?;
            let url = uris.get(gid).and_then(|list| list.first()).cloned();
            let options = opts.get(gid).cloned();
            // 内部表两者都命中（add_uri / 常规 resume 路径）：直接使用
            // （is_some 判断避免 move，unwrap 在确定 Some 后执行）
            if url.is_some() && options.is_some() {
                (url.unwrap(), options.unwrap())
            } else {
                // 内部表缺失（checkpoint 恢复任务）：从仓库任务 + 全局选项兜底
                let repo = self
                    .repo
                    .lock()
                    .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
                let task = repo
                    .get(gid)
                    .ok_or_else(|| format!("任务不存在: {gid}"))?;
                // URL 兜底：取 files[0].uris 的第一个 uri；files 为空返回明确错误
                let url = url
                    .or_else(|| {
                        task.files
                            .first()
                            .and_then(|f| f.uris.first())
                            .map(|(uri, _)| uri.clone())
                    })
                    .ok_or_else(|| {
                        format!("任务 {gid} 缺少源 URL（task_uris 未登记且任务 files 为空）")
                    })?;
                // 引擎选项兜底：全局选项为基准，dir / out 覆盖为任务的保存路径
                let options = options.unwrap_or_else(|| {
                    let mut base = self
                        .global
                        .lock()
                        .map(|g| g.clone())
                        .unwrap_or_else(|e| {
                            warn!("[Motrix] spawn_one 获取全局选项锁失败，使用默认选项: {e}");
                            EngineOptions::default()
                        });
                    base.dir = task.dir.clone();
                    // out：从保存路径取文件名（"dir/out" 最后一个分隔符之后）
                    base.out = task
                        .files
                        .first()
                        .and_then(|f| f.path.rsplit(['/', '\\']).next())
                        .filter(|name| !name.is_empty())
                        .map(str::to_string);
                    base
                });
                (url, options)
            }
        };
        // 2. 启动引擎（事件回调捕获 Arc<TaskManager> 自引用）
        let this = self.arc_this();
        let on_event = move |gid: &str, event: KgetEvent| {
            this.on_engine_event(gid, event);
        };
        let handle = kget::spawn_download(gid.to_string(), &url, &options, on_event)
            .map_err(|e| format!("启动下载失败: {e}"))?;
        // 3. 登记运行句柄
        self.active
            .lock()
            .map_err(|e| format!("获取运行集合锁失败: {e}"))?
            .insert(gid.to_string(), handle);
        // Task 11：下载开始事件（add_uri 启动 / resume 恢复均走此处；
        // 前端 onDownloadStart 触发记录历史目录 + 开始 toast）
        self.emit_event(gid, "start");
        Ok(())
    }

    /// 引擎事件回调入口（引擎线程内调用，须快速返回）
    fn on_engine_event(&self, gid: &str, event: KgetEvent) {
        match event {
            KgetEvent::Started => self.handle_started(gid),
            KgetEvent::Progress { percent, speed } => self.handle_progress(gid, percent, speed),
            KgetEvent::Finished { .. } => self.handle_finished(gid),
            KgetEvent::Failed { message } => self.handle_failed(gid, &message),
        }
    }

    /// Started：更新连接数显示为 1（KGet 引擎已启动）
    fn handle_started(&self, gid: &str) {
        let mut repo = match self.repo.lock() {
            Ok(repo) => repo,
            Err(e) => {
                warn!("[Motrix] Started 回调获取任务仓库锁失败: {e}");
                return;
            }
        };
        let Some(task) = repo.get(gid) else { return };
        if !matches!(task.status, TaskStatus::Active | TaskStatus::Waiting) {
            return;
        }
        // 保持其它字段不变，仅更新 connections=1
        let (completed, total, d_speed, u_speed) = (
            task.completed_length,
            task.total_length,
            task.download_speed,
            task.upload_speed,
        );
        let _ = repo.update_progress(gid, completed, total, d_speed, u_speed, 1);
    }

    /// Progress：按 percent 换算 completed = total * percent / 100，并自算瞬时速度
    ///
    /// KGet 事件只有 percent（0~100）且 speed 恒为 0，无字节数：
    /// - `total > 0`：按"进度差值 / 时间差"自算速度（每 100ms 采样一次，未到
    ///   采样间隔沿用上一次计算值），保证前端速度计 / engine:global-stat 的
    ///   downloadSpeed 不为 0；
    /// - `total == 0`（探测失败）：仅维持 completed 不变，不计算速度（保持现状）。
    fn handle_progress(&self, gid: &str, percent: f64, speed: u64) {
        let mut repo = match self.repo.lock() {
            Ok(repo) => repo,
            Err(e) => {
                warn!("[Motrix] Progress 回调获取任务仓库锁失败: {e}");
                return;
            }
        };
        let Some(task) = repo.get(gid) else { return };
        // 仅运行 / 排队中任务更新进度（已暂停 / 移除 / 完成的迟到事件忽略）
        if !matches!(task.status, TaskStatus::Active | TaskStatus::Waiting) {
            return;
        }
        let total = task.total_length;
        let connections = task.connections.max(1);
        let completed = if total > 0 {
            // percent 可能因服务器差异略超 100，夹紧到 total
            ((total as f64) * percent / 100.0).round().min(total as f64) as u64
        } else {
            // total 未知：不写 completed（保持 0，避免进度虚高）
            task.completed_length
        };
        // 瞬时速度自算（锁顺序：先锁 repo（上文已持有），再锁 speed_samples，
        // 全程保持该顺序、不反向嵌套，避免死锁）
        let computed_speed = if total > 0 {
            let now = std::time::Instant::now();
            let mut samples = match self.speed_samples.lock() {
                Ok(s) => s,
                Err(e) => {
                    warn!("[Motrix] Progress 回调获取速度采样锁失败: {e}");
                    // 锁失败：退化为 KGet 传入值（恒 0）更新进度后返回
                    let _ = repo.update_progress(gid, completed, total, speed, 0, connections);
                    return;
                }
            };
            match samples.get_mut(gid) {
                // 已有快照：达到 100ms 采样间隔才计算并更新，否则沿用上一次速度
                Some((last_completed, last_at)) => {
                    let dt_ms = now.duration_since(*last_at).as_millis() as u64;
                    if dt_ms >= 100 {
                        // Δcompleted 用 saturating_sub 防御 percent 换算的微小回退
                        let delta = completed.saturating_sub(*last_completed);
                        let new_speed = delta * 1000 / dt_ms;
                        *last_completed = completed;
                        *last_at = now;
                        new_speed
                    } else {
                        // 未到采样间隔：保留上一次计算出的速度
                        task.download_speed
                    }
                }
                // 首次见到该 gid：只建立快照不产出速度（首个采样点）
                None => {
                    samples.insert(gid.to_string(), (completed, now));
                    task.download_speed
                }
            }
        } else {
            // total 未知：不计算速度（保持现状，KGet speed 恒 0）
            speed
        };
        let _ = repo.update_progress(gid, completed, total, computed_speed, 0, connections);
    }

    /// Finished：置 Complete + completed=total + 速度归零 + 唤醒下一个等待任务
    fn handle_finished(&self, gid: &str) {
        {
            let mut repo = match self.repo.lock() {
                Ok(repo) => repo,
                Err(e) => {
                    warn!("[Motrix] Finished 回调获取任务仓库锁失败: {e}");
                    return;
                }
            };
            let Some(task) = repo.get(gid) else { return };
            // 已暂停 / 已移除 / 已出错的任务忽略迟到的完成事件
            if !matches!(task.status, TaskStatus::Active | TaskStatus::Waiting) {
                return;
            }
            // 完成字节数：total>0 直接用 total；total==0（探测失败，如 https 任务）
            // 时读取磁盘文件大小回填，保证 completedLength/totalLength 不为 0
            // （进度=100%，见 resolve_finished_bytes）
            let (completed, total) = resolve_finished_bytes(task);
            let connections = task.connections.max(1);
            // 完成时 completed = total（与 percent 换算结果对齐），下载/上传速度归零
            let _ = repo.update_progress(gid, completed, total, 0, 0, connections);
            let _ = repo.set_status(gid, TaskStatus::Complete);
            // Task 11：完成事件（前端 onDownloadComplete 触发完成通知）
            self.emit_event(gid, "complete");
        }
        // 释放运行集合中的句柄（引擎线程已结束）
        if let Ok(mut active) = self.active.lock() {
            active.remove(gid);
        }
        // 清理速度采样快照（repo / active 锁均已释放，独立短锁，避免锁嵌套）
        if let Ok(mut samples) = self.speed_samples.lock() {
            samples.remove(gid);
        }
        // 唤醒下一个等待任务（受 max-concurrent 限制）
        self.promote_waiting();
    }

    /// Failed：标记错误（code=1）+ 唤醒下一个等待任务
    fn handle_failed(&self, gid: &str, message: &str) {
        {
            let mut repo = match self.repo.lock() {
                Ok(repo) => repo,
                Err(e) => {
                    warn!("[Motrix] Failed 回调获取任务仓库锁失败: {e}");
                    return;
                }
            };
            let Some(task) = repo.get(gid) else { return };
            // 已暂停 / 已移除的任务忽略迟到的失败事件（如用户暂停与引擎失败竞态）
            if !matches!(task.status, TaskStatus::Active | TaskStatus::Waiting) {
                return;
            }
            let _ = repo.mark_error(gid, 1, message.to_string());
            // Task 11：错误事件（前端 onDownloadError 触发失败提示）
            self.emit_event(gid, "error");
        }
        // 释放运行集合中的句柄
        if let Ok(mut active) = self.active.lock() {
            active.remove(gid);
        }
        // 清理速度采样快照（独立短锁，避免与 repo 锁嵌套）
        if let Ok(mut samples) = self.speed_samples.lock() {
            samples.remove(gid);
        }
        // 唤醒下一个等待任务
        self.promote_waiting();
    }
}

// ======================================================================
// 辅助函数
// ======================================================================

/// 解析任务完成时的 (completed, total) 字节数（handle_finished 回填用）
///
/// - `total_length > 0`：直接返回 `(total, total)`（完成时已下载字节 = 总长）；
/// - `total_length == 0`（探测失败，如 https 任务）：读取 `files[0].path`
///   的磁盘文件大小回填（两者相等，进度 = 100%）；文件不存在（异常场景）
///   返回 `(0, 0)`，避免虚报进度。
fn resolve_finished_bytes(task: &Task) -> (u64, u64) {
    if task.total_length > 0 {
        return (task.total_length, task.total_length);
    }
    // total 未知：以磁盘实际文件大小为准（下载完成时文件即完整内容）
    let size = task
        .files
        .first()
        .and_then(|f| std::fs::metadata(&f.path).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    (size, size)
}

/// 从 URL 提取保存文件名（`scheme://` 之后路径部分的最后一个 '/' 之后内容），
/// 无路径（仅 host）或以 '/' 结尾（目录）时回退 "download"
fn file_name_from_uri(uri: &str) -> String {
    // 去掉 scheme:// 前缀：文件名只可能出现在 "://" 之后的路径部分
    // （query 等后缀原样保留，如 "a.zip?x=1" → "a.zip?x=1"）
    let rest = uri.split_once("://").map(|(_, r)| r).unwrap_or(uri);
    match rest.rsplit_once('/') {
        // 有路径段：取最后一个 '/' 之后的非空段作为文件名
        Some((_, name)) if !name.is_empty() => name.to_string(),
        // 仅 host（无 '/'）或以 '/' 结尾（目录）：回退默认名
        _ => "download".to_string(),
    }
}

/// system.json 中数值类型的键（值须为 JSON 数字；持久化前把字符串数值 /
/// 带后缀大小转成数字，避免与 SystemConfig 字段类型不匹配）
const SYSTEM_NUMERIC_KEYS: &[&str] = &[
    "max-concurrent-downloads",
    "max-connection-per-server",
    "split",
    "max-download-limit",
    "max-overall-download-limit",
    "max-overall-upload-limit",
    "rpc-listen-port",
    "dht-listen-port",
    "listen-port",
    "seed-time",
    "seed-ratio",
    "max-tries",
    "retry-wait",
];

/// 把 options 补丁中数值键的字符串值转为数字（parse_size 支持 "1M" 后缀），
/// 保证 apply_preference 写回 system.json 时与 SystemConfig 字段类型匹配。
fn normalize_system_patch(options: &Value) -> Value {
    let Some(map) = options.as_object() else {
        return options.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, value) in map {
        if SYSTEM_NUMERIC_KEYS.contains(&key.as_str()) {
            let v = if let Some(n) = value.as_u64() {
                Value::from(n)
            } else if let Some(f) = value.as_f64() {
                Value::from(f)
            } else if let Some(s) = value.as_str() {
                // 按优先级解析字符串数值：
                // 1) 标准十进制整数（如 "3"，保持整数类型与 SystemConfig 字段匹配）；
                // 2) 带小数的浮点（如 seed-ratio "2.5"，parse_size 会截断成整数 2）；
                // 3) 带 K/M/G 后缀的大小串（如 "1M" → 1048576）。
                if let Ok(n) = s.parse::<u64>() {
                    Value::from(n)
                } else if let Ok(f) = s.parse::<f64>() {
                    Value::from(f)
                } else if let Some(n) = parse_size(s) {
                    Value::from(n)
                } else {
                    value.clone()
                }
            } else {
                value.clone()
            };
            out.insert(key.clone(), v);
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant};

    // ------------------------------------------------------------------
    // 测试基础设施：极简本地 HTTP 服务器 + 临时目录 + 等待辅助
    // ------------------------------------------------------------------

    /// 测试临时目录（Drop 时整体清理）
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "motrix-engine-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("创建测试临时目录失败");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 极简本地 HTTP 服务器（测试用）：支持 HEAD / GET 与 Range 分段下载
    ///
    /// 仅响应路径以 `/file` 开头的请求；HEAD 响应声明 `Accept-Ranges: bytes`
    /// （让 KGet 走并行分段路径，该路径有取消检查，abort 后引擎线程能快速退出）。
    /// `start_with_delay` 可放慢发送速度，用于模拟长时下载（测试暂停 / 并发队列）。
    struct TestHttpServer {
        addr: SocketAddr,
        _thread: Option<thread::JoinHandle<()>>,
    }

    impl TestHttpServer {
        /// 启动服务器（可选每块发送延迟）
        fn start_with_delay(content: Vec<u8>, per_chunk_delay: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定测试端口失败");
            // 非阻塞 accept：Drop 时线程随进程退出自然终止，不阻塞测试
            listener
                .set_nonblocking(true)
                .expect("设置非阻塞失败");
            let addr = listener.local_addr().unwrap();
            let thread = thread::spawn(move || loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // 每个连接独立线程处理：避免慢速发送阻塞后续连接的 accept，
                        // 也避免连接生命周期竞态（Windows 下易产生 WSAECONNABORTED）
                        let content = content.clone();
                        let delay = per_chunk_delay;
                        thread::spawn(move || {
                            let _ = handle_one_connection(stream, &content, delay);
                        });
                    }
                    // 非阻塞 accept：无连接时休眠后继续轮询
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            });
            Self {
                addr,
                _thread: Some(thread),
            }
        }

        /// 启动服务器（无发送延迟）
        fn start(content: Vec<u8>) -> Self {
            Self::start_with_delay(content, Duration::ZERO)
        }

        /// 生成指向服务器的 URL
        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{}", self.addr.port(), path)
        }
    }

    /// 处理单个连接：读取请求头 → 响应 → 显式关闭写端（避免 RST 竞态）
    fn handle_one_connection(
        mut stream: TcpStream,
        content: &[u8],
        delay: Duration,
    ) -> std::io::Result<()> {
        use std::net::Shutdown;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        // 读取请求头（直到空行）
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let req = String::from_utf8_lossy(&buf);
        let result = serve_request(&mut stream, &req, content, delay);
        // 显式关闭写端：让客户端（reqwest / probe）读到 EOF，避免连接复用竞态
        let _ = stream.shutdown(Shutdown::Both);
        result.map(|_| ())
    }

    /// 处理单个 HTTP 请求：HEAD / GET（含 Range: bytes=start- 分段）
    fn serve_request(
        stream: &mut TcpStream,
        req: &str,
        content: &[u8],
        delay: Duration,
    ) -> std::io::Result<()> {
        let request_line = req.lines().next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("GET");
        let path = parts.next().unwrap_or("/");
        let range = req
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
            .map(str::to_string);
        let total = content.len() as u64;

        let head: String;
        let body: &[u8];
        let is_target = path.starts_with("/file");
        if !is_target {
            head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string();
            body = &[];
        } else if method == "HEAD" {
            // 声明 Accept-Ranges: bytes：KGet 判定服务器支持 Range，走并行分段路径
            // （该路径有取消检查，abort 后引擎线程能快速退出，pause/resume 测试稳定）
            head = format!(
                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
            );
            body = &[];
        } else if let Some(range) = range {
            // 解析 Range: bytes=start- 或 bytes=start-end（仅取 start）
            let start: u64 = range
                .split("bytes=")
                .nth(1)
                .and_then(|r| r.split('-').next())
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
                .min(total);
            body = &content[start as usize..];
            let end = total.saturating_sub(1);
            head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
        } else {
            head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
            body = content;
        }

        // 写入响应（body 按块发送，块间可插入延迟以模拟慢速下载）
        stream.write_all(head.as_bytes())?;
        for chunk in body.chunks(64 * 1024) {
            stream.write_all(chunk)?;
            if !delay.is_zero() {
                thread::sleep(delay);
            }
        }
        stream.flush()
    }

    /// 构造测试用 TaskManager（max_concurrent 可配置；下载目录在临时目录下）
    fn test_manager(max_concurrent: u32) -> (Arc<TaskManager>, Arc<Mutex<TaskRepository>>, TempDir) {
        let temp = TempDir::new();
        let dl_dir = temp.path().join("dl");
        std::fs::create_dir_all(&dl_dir).expect("创建测试下载目录失败");
        let config_manager = Arc::new(Mutex::new(ConfigManager::new(temp.path().to_path_buf())));
        let mut system = SystemConfig::defaults(temp.path());
        system.max_concurrent_downloads = max_concurrent;
        system.dir = dl_dir.to_string_lossy().to_string();
        let tm = Arc::new(TaskManager::new(&system, config_manager));
        tm.set_self(Arc::downgrade(&tm));
        let repo = tm.repo.clone();
        (tm, repo, temp)
    }

    /// 轮询等待任务到达指定状态（带超时，避免测试卡死）
    fn wait_status(repo: &Arc<Mutex<TaskRepository>>, gid: &str, status: TaskStatus) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(guard) = repo.lock() {
                if guard.get(gid).map(|t| t.status) == Some(status) {
                    return true;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// 等待任务完成并断言：totalLength / completedLength 正确 + 输出文件内容一致
    fn assert_completed_with_file(
        repo: &Arc<Mutex<TaskRepository>>,
        gid: &str,
        expected: &[u8],
    ) {
        assert!(
            wait_status(repo, gid, TaskStatus::Complete),
            "任务 {gid} 未在超时内完成"
        );
        let (path, total, completed) = {
            let guard = repo.lock().expect("获取任务仓库锁失败");
            let task = guard.get(gid).expect("任务应存在于仓库");
            (
                task.files[0].path.clone(),
                task.total_length,
                task.completed_length,
            )
        };
        // HTTP 场景探总长成功：totalLength / completedLength 应与文件一致
        assert_eq!(total, expected.len() as u64);
        assert_eq!(completed, expected.len() as u64);
        let saved = std::fs::read(&path).expect("输出文件应存在");
        assert_eq!(saved, expected, "输出文件内容与源不一致");
    }

    // ------------------------------------------------------------------
    // 测试 1：add_uri 后任务经 KGet 引擎下载完成（状态 Complete、文件存在）
    // ------------------------------------------------------------------
    #[test]
    fn add_uri_downloads_to_complete() {
        // 100KB 伪随机内容（保证与服务器返回一致）
        let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let server = TestHttpServer::start(content.clone());
        let (tm, repo, _temp) = test_manager(2);

        // 添加任务（单连接，避免并发分段干扰测试断言）
        let urls = vec![server.url("/file")];
        let gids = tm
            .add_uri(&urls, &json!({"out": "a.bin", "connections": 1}))
            .expect("add_uri 应成功");
        let gid = &gids[0];
        // gid 为 16 位小写 hex（与 aria2 一致）
        assert_eq!(gid.len(), 16);

        // 任务最终 Complete 且文件内容一致
        assert_completed_with_file(&repo, gid, &content);

        // 探总长生效：添加后 totalLength 已被 HEAD 探测填充
        let t = repo.lock().unwrap().get(gid).unwrap().clone();
        assert_eq!(t.total_length, content.len() as u64);
        assert_eq!(t.completed_length, content.len() as u64);
        assert_eq!(t.connections, 1);
    }

    // ------------------------------------------------------------------
    // 测试 2：暂停后 abort，恢复后继续（Range 续传）直至完成
    // ------------------------------------------------------------------
    #[test]
    fn pause_resume_resumes_to_complete() {
        // 2MB 内容 + 每块 2ms 发送延迟：保证下载过程可被暂停打断
        let content = vec![7u8; 2 * 1024 * 1024];
        let server = TestHttpServer::start_with_delay(content.clone(), Duration::from_millis(2));
        let (tm, repo, _temp) = test_manager(1);

        let gids = tm
            .add_uri(
                &[server.url("/file")],
                &json!({"out": "p.bin", "connections": 1}),
            )
            .expect("add_uri 应成功");
        let gid = &gids[0];
        // 等待下载开始
        assert!(wait_status(&repo, gid, TaskStatus::Active));

        // 暂停：abort 引擎句柄 + 置 Paused
        tm.pause(gid).expect("暂停应成功");
        assert_eq!(
            repo.lock().unwrap().get(gid).unwrap().status,
            TaskStatus::Paused
        );
        // 已暂停任务再次暂停：幂等
        tm.pause(gid).expect("重复暂停应幂等");

        // 恢复：置 Active 并重新 spawn（KGet 检测已有部分文件，Range 续传）
        tm.resume(gid).expect("恢复应成功");
        assert!(wait_status(&repo, gid, TaskStatus::Complete));

        // 文件完整（续传未损坏内容）
        assert_completed_with_file(&repo, gid, &content);
    }

    // ------------------------------------------------------------------
    // 测试 3：max-concurrent-downloads 并发队列（第二个 Waiting，完成后 promote）
    // ------------------------------------------------------------------
    #[test]
    fn max_concurrent_limits_and_promotes() {
        // 慢速服务器确保第一个任务不会瞬间完成（便于观察排队状态）
        let content = vec![42u8; 512 * 1024];
        let server = TestHttpServer::start_with_delay(content.clone(), Duration::from_millis(1));
        // 最大并发 1：第二个任务必须排队
        let (tm, repo, _temp) = test_manager(1);

        let gid1 = tm
            .add_uri(&[server.url("/file")], &json!({"out": "one.bin", "connections": 1}))
            .expect("add_uri 应成功")[0]
            .clone();
        let gid2 = tm
            .add_uri(&[server.url("/file")], &json!({"out": "two.bin", "connections": 1}))
            .expect("add_uri 应成功")[0]
            .clone();

        // 第一个立即 Active；第二个因并发限制保持 Waiting
        assert_eq!(
            repo.lock().unwrap().get(&gid1).unwrap().status,
            TaskStatus::Active
        );
        assert_eq!(
            repo.lock().unwrap().get(&gid2).unwrap().status,
            TaskStatus::Waiting
        );

        // 第一个完成后，第二个被自动 promote 为 Active 并最终完成
        assert!(wait_status(&repo, &gid1, TaskStatus::Complete));
        assert!(wait_status(&repo, &gid2, TaskStatus::Complete));
        assert_completed_with_file(&repo, &gid1, &content);
        assert_completed_with_file(&repo, &gid2, &content);
    }

    // ------------------------------------------------------------------
    // 测试 4：probe_content_length 真实 HTTP 探测（HEAD → Content-Length）
    // ------------------------------------------------------------------
    #[test]
    fn probe_content_length_via_test_server() {
        let content = vec![9u8; 123_456];
        let server = TestHttpServer::start(content.clone());
        let opts = EngineOptions::default();
        // HEAD 探测：Content-Length 与内容长度一致
        let len = crate::kget::probe_content_length(&server.url("/file"), &opts)
            .expect("http 探测应成功");
        assert_eq!(len, 123_456);
        // 非 /file 路径 → 404 → Ok(0)
        assert_eq!(
            crate::kget::probe_content_length(&server.url("/nope"), &opts).unwrap(),
            0
        );
    }

    // ------------------------------------------------------------------
    // 测试：resolve_finished_bytes 完成字节回填（total==0 时读磁盘文件大小）
    // ------------------------------------------------------------------
    #[test]
    fn resolve_finished_bytes_handles_unknown_total() {
        let temp = TempDir::new();

        // 场景 1：total_length=0，files[0].path 指向已存在的文件 → (文件大小, 文件大小)
        // （https 探测失败场景：完成后以磁盘实际大小回填 total 与 completed）
        let done_path = temp.path().join("done.bin");
        std::fs::write(&done_path, vec![9u8; 42]).expect("写入测试文件失败");
        let mut task = Task::new_http_task(
            "a".to_string(),
            &["http://127.0.0.1:9/file".to_string()],
            temp.path().to_string_lossy().to_string(),
            "done.bin",
        );
        assert_eq!(task.total_length, 0);
        assert_eq!(resolve_finished_bytes(&task), (42, 42));

        // 场景 2：total_length > 0 → (total, total)，不读文件
        task.total_length = 1234;
        assert_eq!(resolve_finished_bytes(&task), (1234, 1234));

        // 场景 3：total_length=0 且文件不存在（异常场景）→ (0, 0)，不虚报进度
        let missing = Task::new_http_task(
            "b".to_string(),
            &["http://127.0.0.1:9/file".to_string()],
            temp.path().to_string_lossy().to_string(),
            "nope.bin",
        );
        assert_eq!(resolve_finished_bytes(&missing), (0, 0));
    }

    // ------------------------------------------------------------------
    // 测试 5：change_global_option 更新并发数并持久化到 system.json
    // ------------------------------------------------------------------
    #[test]
    fn change_global_option_updates_max_concurrent_and_persists() {
        let (tm, _repo, temp) = test_manager(1);
        // 修改并发数为 3（字符串形态，模拟前端/aria2 参数），并修改全局 dir
        tm.change_global_option(&json!({
            "max-concurrent-downloads": "3",
            "max-download-limit": "1M",
            "dir": temp.path().join("dl2").to_string_lossy(),
        }))
        .expect("change_global_option 应成功");

        // 内存生效：max_concurrent = 3（内部读取校验）
        assert_eq!(*tm.max_concurrent.lock().unwrap(), 3);
        // 全局选项生效：新任务 dir 使用新值、限速 1M
        let global = tm.global.lock().unwrap();
        assert_eq!(global.max_download_limit, Some(1024 * 1024));
        assert_eq!(global.dir, temp.path().join("dl2").to_string_lossy());
        drop(global);

        // 持久化生效：system.json 中 max-concurrent-downloads 为数字 3（类型匹配）
        let system = crate::config::read_system_config(temp.path()).expect("读取 system.json 应成功");
        assert_eq!(system.max_concurrent_downloads, 3);
    }

    // ------------------------------------------------------------------
    // 测试 6：file_name_from_uri / normalize_system_patch 纯函数
    // ------------------------------------------------------------------
    #[test]
    fn file_name_from_uri_extracts_name() {
        assert_eq!(file_name_from_uri("https://example.com/a.zip"), "a.zip");
        assert_eq!(file_name_from_uri("https://example.com/a.zip?x=1"), "a.zip?x=1");
        assert_eq!(file_name_from_uri("https://example.com/"), "download");
        assert_eq!(file_name_from_uri("https://example.com"), "download");
    }

    #[test]
    fn normalize_system_patch_converts_numeric_strings() {
        let patch = json!({
            "max-concurrent-downloads": "3",
            "max-download-limit": "1M",
            "seed-ratio": "2.5",
            "dir": "/downloads",
            "user-agent": "UA",
        });
        let normalized = normalize_system_patch(&patch);
        assert_eq!(normalized["max-concurrent-downloads"], 3);
        assert_eq!(normalized["max-download-limit"], 1024 * 1024);
        assert_eq!(normalized["seed-ratio"], 2.5);
        // 非数值键保持原样
        assert_eq!(normalized["dir"], "/downloads");
        assert_eq!(normalized["user-agent"], "UA");
    }

    // ------------------------------------------------------------------
    // 测试 7：add_torrent 占位错误 / get 查询 / purge 清空 removed 历史
    // ------------------------------------------------------------------
    #[test]
    fn add_torrent_is_placeholder_and_purge_clears_removed() {
        let (tm, repo, _temp) = test_manager(1);
        // add_torrent：BT 属 Phase 3，返回明确占位错误，不创建任务、不启动引擎
        let err = tm
            .add_torrent("magnet:?xt=urn:btih:0123456789abcdef", &json!({}))
            .expect_err("add_torrent 应返回占位错误");
        assert!(err.contains("Phase 3"), "占位错误信息应提及 Phase 3: {err}");
        assert!(
            repo.lock().unwrap().all().is_empty(),
            "add_torrent 不应创建任务"
        );

        // get：不存在返回 None；存在返回任务克隆
        assert!(tm.get("0123456789abcdef").is_none());
        let gid = "abcdef0123456789";
        let task = Task::new_http_task(gid, &["http://127.0.0.1:1/x".to_string()], "/tmp", "x.bin");
        repo.lock().unwrap().add(task);
        assert_eq!(tm.get(gid).unwrap().gid, gid);

        // purge：仅清空 removed 历史（stopped 列表），在册任务不受影响
        repo.lock().unwrap().remove(gid);
        assert!(!repo.lock().unwrap().stopped().is_empty());
        tm.purge();
        assert!(repo.lock().unwrap().stopped().is_empty());
        // 在册任务（active/waiting 等）不受 purge 影响——上面已移除，此处仓库应为空
        assert!(repo.lock().unwrap().all().is_empty());
    }

    // ------------------------------------------------------------------
    // 测试 8：checkpoint 恢复任务兜底（Task 13 续传边界修复）
    // 构造 Task（files[0].uris 有 URL、Paused）直接进仓库、不登记 task_uris /
    // task_options（与 session.rs::restore_checkpoint 现状一致），resume 应能
    // 经 files 兜底 URL + 全局选项兜底启动引擎并下载完成。
    // ------------------------------------------------------------------
    #[test]
    fn resume_restored_task_falls_back_to_files_uris() {
        let content: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let server = TestHttpServer::start(content.clone());
        let (tm, repo, temp) = test_manager(1);
        let dl_dir = temp.path().join("dl");

        // 模拟 checkpoint 恢复：任务直接进仓库（Paused 状态），
        // 未登记 task_uris / task_options 内部表
        let gid = "fedcba9876543210".to_string();
        let mut task = Task::new_http_task(
            gid.clone(),
            &[server.url("/file")],
            dl_dir.to_string_lossy().to_string(),
            "restored.bin",
        );
        task.status = TaskStatus::Paused; // 导入恢复场景直接赋值（绕过状态机）
        task.total_length = content.len() as u64; // checkpoint 记录了总长（探总长结果）
        repo.lock().unwrap().add(task);

        // 恢复：内部表缺失时应从 files[0].uris 兜底 URL 并启动引擎
        tm.resume(&gid).expect("恢复 checkpoint 任务应成功");
        assert!(wait_status(&repo, &gid, TaskStatus::Complete), "兜底恢复的任务未完成");

        // 下载写回了任务的保存路径且内容与服务器一致
        assert_completed_with_file(&repo, &gid, &content);
    }

    // ------------------------------------------------------------------
    // 测试 9：瞬时下载速度自算（Task 4：KGet Progress 事件不带实时速度）
    // 慢速下载（1MB、每 64KB 块 20ms 延迟 ≈ 320ms 总时长），轮询断言
    // 下载过程中 download_speed 出现 > 0 的采样值（首个速度样本在 ~100ms
    // 产生），完成后速度归 0。
    // ------------------------------------------------------------------
    #[test]
    fn progress_reports_computed_download_speed() {
        // 1MB 内容 + 每块 20ms 发送延迟：总时长约 320ms（16 块 × 20ms），
        // 期间能产生约 2~3 个速度采样点（每 100ms 一次），速度 > 0 的窗口
        // 足够轮询捕获；connections=1 走 AdvancedDownloader 单段路径
        let content = vec![5u8; 1024 * 1024];
        let server = TestHttpServer::start_with_delay(content.clone(), Duration::from_millis(20));
        let (tm, repo, _temp) = test_manager(1);

        let gids = tm
            .add_uri(
                &[server.url("/file")],
                &json!({"out": "speed.bin", "connections": 1}),
            )
            .expect("add_uri 应成功");
        let gid = &gids[0];

        // 阶段 1：等待任务开始下载（Active / 已完成均可，且 completedLength > 0，
        // 即首个 Progress 回调已到达）
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (status, completed) = {
                let guard = repo.lock().expect("获取任务仓库锁失败");
                let task = guard.get(gid).expect("任务应存在于仓库");
                (task.status, task.completed_length)
            };
            if completed > 0 && matches!(status, TaskStatus::Active | TaskStatus::Complete) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "任务未在 10s 超时内开始下载（completedLength 一直为 0）"
            );
            thread::sleep(Duration::from_millis(50));
        }

        // 阶段 2：轮询（最长 10s）断言瞬时速度 > 0。首个速度样本在首个
        // Progress 回调后 ~100ms 产生，下载 ~320ms 完成，速度 > 0 的状态有
        // 约 200ms 窗口可捕获（50ms 轮询间隔足够稳定命中）
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_speed = false;
        while Instant::now() < deadline {
            let speed = repo
                .lock()
                .expect("获取任务仓库锁失败")
                .get(gid)
                .expect("任务应存在于仓库")
                .download_speed;
            if speed > 0 {
                saw_speed = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(saw_speed, "下载过程中应自算出 > 0 的瞬时速度");

        // 阶段 3：任务完成且文件完整，完成后速度归 0（handle_finished 置零
        // 并清理速度采样快照）
        assert_completed_with_file(&repo, gid, &content);
        assert_eq!(
            repo.lock().unwrap().get(gid).unwrap().download_speed,
            0,
            "完成后下载速度应归零"
        );
    }
}
