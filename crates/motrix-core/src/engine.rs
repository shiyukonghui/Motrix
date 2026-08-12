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

use crate::bt::{BtAddOptions, BtEngine, BtEngineConfig, BtEvent, BtProgress, TorrentMeta};
use crate::config::{ConfigManager, SystemConfig};
use crate::fastdown::FastDownHandle;
use crate::http::HttpDownloadHandle;
use crate::kget::{self, KgetEvent, KgetHandle};
use crate::options::{parse_size, EngineOptions};
use crate::task::{generate_gid, GlobalStat, Task, TaskFile, TaskRepository, TaskStatus};

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

/// 下载引擎句柄（统一并行 HTTP / 顺序 HTTP / KGet 三种实现）
///
/// - [`FastDownHandle`]：HTTP(S) 并行下载（fastdown.rs，fast-down 5.0.2，
///   多连接 + 侧车状态字节级续传，首选）；
/// - [`HttpDownloadHandle`]：HTTP(S) 顺序下载（http.rs，可靠续传，fast-down
///   初始化失败 / 代理认证等场景的回退）；
/// - [`KgetHandle`]：KGet 引擎（非 HTTP 协议 / 既有路径）。
enum EngineHandle {
    /// 并行 HTTP 下载（fastdown.rs，首选）
    FastDown(FastDownHandle),
    /// 顺序 HTTP 下载（http.rs，回退）
    Http(HttpDownloadHandle),
    /// KGet 引擎（kget.rs）
    Kget(KgetHandle),
}

impl EngineHandle {
    /// 中止下载（暂停 / 删除用）
    fn abort(&self) {
        match self {
            EngineHandle::FastDown(h) => h.abort(),
            EngineHandle::Http(h) => h.abort(),
            EngineHandle::Kget(h) => h.abort(),
        }
    }

    /// 有限等待引擎线程退出
    fn wait_exit(&self, timeout: std::time::Duration) -> bool {
        match self {
            EngineHandle::FastDown(h) => h.wait_exit(timeout),
            EngineHandle::Http(h) => h.wait_exit(timeout),
            EngineHandle::Kget(h) => h.wait_exit(timeout),
        }
    }
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
    active: Mutex<HashMap<String, EngineHandle>>,
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
    /// BT 引擎（Phase 3，librqbit 集成）：**懒初始化**（首次 add_torrent 时创建，
    /// 避免应用无 BT 任务时也拉起 DHT / 监听端口）；内部自持 tokio Runtime 与
    /// librqbit Session，进度经轮询回调 `on_bt_event` 回到本管理器
    bt: Mutex<Option<Arc<BtEngine>>>,
    /// 运行中 BT 任务 gid 集合（并发槽位计数用：`active.len() + bt_active.len()`
    /// 为实际运行数；暂停 / 移除 / 下载完成（不做种）时移除）
    bt_active: Mutex<std::collections::HashSet<String>>,
    /// 已发过"下载完成"通知的 BT 任务 gid（防止轮询重复触发 complete / bt-complete）
    bt_done: Mutex<std::collections::HashSet<String>>,
    /// BT 任务进入做种的时刻（gid → Instant；做种上限 seed-time / seed-ratio 检查用；
    /// 做种结束 / 暂停 / 移除时清理对应记录，避免内存泄漏与过期时刻误判）
    bt_seed_started: Mutex<HashMap<String, std::time::Instant>>,
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
            // BT 引擎懒初始化（None），首次 add_torrent 时创建
            bt: Mutex::new(None),
            bt_active: Mutex::new(std::collections::HashSet::new()),
            bt_done: Mutex::new(std::collections::HashSet::new()),
            bt_seed_started: Mutex::new(HashMap::new()),
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

    /// 添加 URL 任务（aria2.addUri 语义）
    ///
    /// 每个 URL 创建一个独立任务并返回对应 gid 列表：
    /// 1. 以全局选项为基准克隆 + 叠加任务级 `options`（dir/out 取任务级或全局）；
    /// 2. **magnet 链接按 aria2 语义（addUri 接受 magnet:）路由到 BT 引擎**
    ///    （经 [`TaskManager::add_torrent`]，前端 AddTask 对话框的 URI 标签页
    ///    会把磁力链接走 addUri 通道，若按 HTTP 处理会报 builder error）；
    /// 3. HTTP(S) URL 尽力探测 Content-Length 填充 `total_length`
    ///    （探测失败返回 0，不阻塞任务，进度由 UI 按 percent 换算）；
    /// 4. 加入任务仓库并记录 task_uris / task_options；
    /// 5. 若当前运行数 < max-concurrent-downloads 则置 Active 并启动引擎，
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
            // magnet: 链接 → BT 引擎（librqbit），返回单 gid；与 aria2 行为一致
            if uri.starts_with("magnet:") {
                gids.push(self.add_torrent(uri, options)?);
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

    /// BT 任务暂停（pause / force_pause 的 BT 分支）
    ///
    /// 暂停语义（项目已确认）：**停止 torrent 并保留 piece 状态**（librqbit
    /// session.pause），恢复时重新 start 基于已有 piece 续传，不重新校验。
    /// - Active / Waiting → Paused：引擎 pause + 从 bt_active 移除（释放并发槽位）；
    /// - 已暂停任务幂等；终态（complete/error/seeding）不可暂停（做种任务的
    ///   终止走 remove，与 aria2 语义一致）。
    fn pause_bt(&self, gid: &str) -> Result<String, String> {
        // 1. 停止引擎（仅当任务已登记进 librqbit Session；checkpoint 恢复后未
        //    resume 的任务不在引擎中，跳过引擎操作直接状态迁移）。
        //    磁力 metadata 解析中（未登记进 torrents）的任务：取消后台重试循环
        if let Ok(guard) = self.bt.lock() {
            if let Some(engine) = guard.as_ref() {
                engine.cancel_pending_add(gid);
                if engine.contains(gid) {
                    engine.pause(gid)?;
                }
            }
        }
        // 2. 状态迁移：active / waiting → paused；paused 幂等
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
                    self.emit_event(gid, "pause");
                }
                // 已暂停：幂等
                TaskStatus::Paused => {}
                // 终态任务不可暂停（含做种中任务：做种终止走 remove）
                _ => return Err(format!("无法暂停任务（当前状态不支持）: gid={gid}")),
            }
        }
        // 3. 释放并发槽位（暂停可能唤醒下一个等待任务）
        if let Ok(mut active) = self.bt_active.lock() {
            active.remove(gid);
        }
        // 4. 清理做种起始时刻记录（暂停后做种计时作废，恢复重新开始）
        if let Ok(mut started) = self.bt_seed_started.lock() {
            started.remove(gid);
        }
        self.promote_waiting();
        Ok("OK".to_string())
    }

    /// pause / force_pause 共用实现
    fn pause_inner(&self, gid: &str) -> Result<String, String> {
        // 0. BT 任务分流（Phase 3）：停止 librqbit torrent 并保留 piece 状态
        let is_bt = self
            .repo
            .lock()
            .map(|repo| repo.get(gid).map(|t| t.bittorrent.is_some()).unwrap_or(false))
            .unwrap_or(false);
        if is_bt {
            return self.pause_bt(gid);
        }
        // 1. abort 并"有限等待"引擎线程退出（引擎可能阻塞在网络读取，不能无限
        //    join；超时后直接继续，引擎线程稍后自行退出，迟到的 Finished/Failed
        //    事件因状态已变会被忽略）
        if let Some(handle) = self
            .active
            .lock()
            .map_err(|e| format!("获取运行集合锁失败: {e}"))?
            .remove(gid)
        {
            handle.abort();
            let _ = handle.wait_exit(std::time::Duration::from_millis(2000));
            // 2. 暂停后部分文件处理（按引擎类型）：
            //    - FastDown（并行 HTTP，fastdown.rs）：引擎线程在退出前已自行
            //      **截断文件到最大已写端点 + 持久化 clean 侧车状态**（恢复时
            //      据此 invert 剩余区间字节级续传），此处无需额外处理；
            //    - Http（顺序 HTTP，http.rs）：文件即已下载的连续前缀，截断到
            //      已完成字节（truncate_partial_file），恢复时经 Range 续传追加；
            //    - Kget（并行非 HTTP / 旧路径）：沿用原"删除预分配假文件"兜底
            //      （见 cleanup_partial_file 注释），避免恢复误判完成。
            match &handle {
                EngineHandle::FastDown(_) => {}
                EngineHandle::Http(_) => self.truncate_partial_file(gid),
                EngineHandle::Kget(_) => self.cleanup_partial_file(gid),
            }
        }
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

    /// 暂停时把部分文件截断到已完成字节（顺序 HTTP 下载专用）
    ///
    /// http.rs 顺序引擎保证文件 = 已下载的**连续前缀**（单连接从 0 顺序写，
    /// 不预分配整文件），因此截断到 `completed_length` 是安全的：恢复时经
    /// `Range: bytes={completed}-` 追加续传，不再从头重下（对比 KGet 并行
    /// 下载因预分配/空洞只能"删文件重下"，见 [`Self::cleanup_partial_file`]）。
    fn truncate_partial_file(&self, gid: &str) {
        // 读取任务信息（克隆，避免持锁调用文件系统）
        let (path, completed) = {
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
            (path, task.completed_length)
        };
        // 无进度 / 路径为空：无需处理（恢复从头下载）
        if path.is_empty() || completed == 0 {
            return;
        }
        if let Err(e) = crate::http::truncate_to(&path, completed) {
            // Windows 下引擎线程可能仍持有文件句柄；失败仅告警，
            // 恢复时若文件大小仍 > completed，顺序引擎会从头（Range 被忽略）重下
            warn!("[Motrix] 暂停截断部分文件失败（恢复将从头下载）: {path}: {e}");
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
        // 1.5 BT 任务分流（Phase 3）：重新 start（librqbit unpause，基于保留 piece 续传）
        let is_bt = self
            .repo
            .lock()
            .map(|repo| repo.get(gid).map(|t| t.bittorrent.is_some()).unwrap_or(false))
            .unwrap_or(false);
        if is_bt {
            return self.resume_bt(gid);
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

    /// BT 任务恢复（resume 的 BT 分支）
    ///
    /// - checkpoint 恢复的任务不在 librqbit Session 中：先经
    ///   [`TaskManager::ensure_bt_registered`] 从 `files[0].uris[0]` 的源重新加入
    ///   （磁力 / base64 .torrent），再 unpause 续传；
    /// - 常规暂停后恢复：引擎中已有登记，直接 unpause（librqbit 基于保留的
    ///   piece 状态继续下载 / 做种）。
    fn resume_bt(&self, gid: &str) -> Result<String, String> {
        // 1. 并发槽位检查：满则回到 waiting 队列
        let max = *self
            .max_concurrent
            .lock()
            .map_err(|e| format!("获取并发配置锁失败: {e}"))?;
        let num_active = self
            .active
            .lock()
            .map_err(|e| format!("获取运行集合锁失败: {e}"))?
            .len()
            + self
                .bt_active
                .lock()
                .map_err(|e| format!("获取 BT 运行集合锁失败: {e}"))?
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
        // 2. 置 Active 并确保引擎登记 + 恢复运行
        {
            let mut repo = self
                .repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
            repo.set_status(gid, TaskStatus::Active)
                .map_err(|_| format!("任务不存在: {gid}"))?;
        }
        // 获取（必要时懒创建）BT 引擎：重启后恢复的 BT 任务 resume 时引擎
        // 可能尚未初始化（bt_engine_for 内部按任务级选项懒创建）
        let engine = self.bt_engine_for(gid)?;
        // checkpoint 恢复的任务：重新加入引擎（已登记则 no-op）；
        // 重新加入的任务在 librqbit 中已自动运行（live），对 live torrent 调
        // unpause 会报错（librqbit "already live"），故仅在"暂停后恢复"场景 unpause。
        let was_registered = engine.contains(gid);
        self.ensure_bt_registered(gid)?;
        if was_registered {
            engine.resume(gid)?;
        }
        // 3. 登记运行集合 + 下载开始事件（与 KGet resume 对齐）
        if let Ok(mut active) = self.bt_active.lock() {
            active.insert(gid.to_string());
        }
        self.emit_event(gid, "start");
        Ok("OK".to_string())
    }

    /// 移除任务（aria2.remove / forceRemove）：abort 引擎句柄 + 移除仓库记录
    ///
    /// 已下载的部分文件保留（是否删除文件由上层决定）；移除后释放并发槽位，
    /// 自动 promote 下一个等待任务。任务不存在返回错误。
    pub fn remove(&self, gid: &str) -> Result<String, String> {
        // 0. BT 任务分流（Phase 3）：librqbit session.delete（保留已下载文件）
        let is_bt = self
            .repo
            .lock()
            .map(|repo| repo.get(gid).map(|t| t.bittorrent.is_some()).unwrap_or(false))
            .unwrap_or(false);
        if is_bt {
            return self.remove_bt(gid);
        }
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
        // 1.5 删除 fast-down 侧车状态文件（任务已移除，断点续传状态失去意义；
        //     引擎线程在退出前可能已写 clean 状态，需在 wait_exit 后删除。
        //     路径 = 输出文件 + ".fd.json"）
        if let Some(path) = self
            .repo
            .lock()
            .ok()
            .and_then(|repo| repo.get(gid).cloned())
            .and_then(|t| t.files.first().map(|f| f.path.clone()))
        {
            let _ = std::fs::remove_file(crate::fastdown::state_path(&path));
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

    /// BT 任务移除（remove / forceRemove 的 BT 分支）
    ///
    /// librqbit session.delete（`delete_files=false`）**保留已下载文件**
    /// （与 aria2 remove 语义一致；是否删除文件由上层决定）；从引擎登记表与
    /// 任务仓库移除，释放并发槽位后唤醒下一个等待任务。
    fn remove_bt(&self, gid: &str) -> Result<String, String> {
        // 1. 从引擎移除（仅当已登记；checkpoint 恢复后未 resume 的任务跳过）
        if let Ok(guard) = self.bt.lock() {
            if let Some(engine) = guard.as_ref() {
                // 磁力 metadata 解析中（未登记进 torrents）的任务：取消后台重试循环
                engine.cancel_pending_add(gid);
                if engine.contains(gid) {
                    engine.remove(gid)?;
                }
            }
        }
        // 2. 清理任务级记录（源 / 选项 / 运行集合 / 完成标记）
        self.task_uris
            .lock()
            .map_err(|e| format!("获取源 URL 表锁失败: {e}"))?
            .remove(gid);
        self.task_options
            .lock()
            .map_err(|e| format!("获取任务选项锁失败: {e}"))?
            .remove(gid);
        if let Ok(mut active) = self.bt_active.lock() {
            active.remove(gid);
        }
        if let Ok(mut done) = self.bt_done.lock() {
            done.remove(gid);
        }
        // 清理做种起始时刻记录（移除任务后做种计时无意义，避免内存泄漏）
        if let Ok(mut started) = self.bt_seed_started.lock() {
            started.remove(gid);
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
            self.emit_event(gid, "stop");
        }
        // 4. 释放并发槽位，唤醒下一个等待任务
        self.promote_waiting();
        Ok("OK".to_string())
    }

    /// 修改任务级选项（aria2.changeOption）
    ///
    /// 说明：运行中任务的选项在**下次恢复（resume / 重试）时生效**；
    /// 已暂停任务更新后**不会自动重启**（保持暂停状态），符合 aria2 语义。
    /// 例外：`select-file`（BT 多文件任务的已选文件集合）**即时下发**到
    /// librqbit（`update_only_files`），无需重启任务即生效。
    pub fn change_option(&self, gid: &str, options: &Value) -> Result<String, String> {
        // 1. 更新任务级选项（与 add_uri 同一套映射；先更新、释放锁后再做 BT 下发，
        //    避免持 task_options 锁期间嵌套其它锁）
        let updated = {
            let mut task_options = self
                .task_options
                .lock()
                .map_err(|e| format!("获取任务选项锁失败: {e}"))?;
            let mut opts = task_options
                .get(gid)
                .cloned()
                .ok_or_else(|| format!("任务不存在: {gid}"))?;
            opts.apply_task_options(options);
            task_options.insert(gid.to_string(), opts.clone());
            opts
        };
        // 2. BT 任务 select-file 即时生效：把解析后的 0 起始索引（apply_task_options
        //    已转换，见 options.rs parse_select_file）下发给 librqbit 更新已选文件集合
        if options.get("select-file").is_some() {
            let is_bt = self
                .repo
                .lock()
                .map(|repo| repo.get(gid).map(|t| t.bittorrent.is_some()).unwrap_or(false))
                .unwrap_or(false);
            if is_bt {
                // 仅下发解析结果；未指定合法索引（空串等）时下发空集合（与选项一致）
                let indices = updated.only_files.clone().unwrap_or_default();
                let engine = self
                    .bt
                    .lock()
                    .map_err(|e| format!("获取 BT 引擎锁失败: {e}"))?
                    .clone()
                    .ok_or_else(|| "BT 引擎未初始化".to_string())?;
                engine.only_files(gid, &indices)?;
            }
        }
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
    /// `torrent` 为 magnet 链接（`magnet:` 前缀）或 base64 编码的 .torrent 内容：
    /// - **磁力**：预解析 info_hash 创建"metadata 任务"（`bittorrent` 存在但
    ///   `info_name=None`、totalLength=0，前端 `isMagnetTask` 据此判断）；
    ///   metadata 获取成功后轮询回调 [`TaskManager::bt_metadata_ready`] 在同一 gid
    ///   上补齐文件 / 总长 / announce / 名称；
    /// - **.torrent**：解码解析后直接创建完整 BT 任务（files / totalLength /
    ///   announceList / info_name 齐备）。
    ///
    /// 任务加入 librqbit Session 即开始运行（受 `max-concurrent-downloads` 并发队列
    /// 限制：槽位不足时保持 Waiting，promote 时置 Active）。返回 16 位 hex gid。
    pub fn add_torrent(&self, torrent: &str, options: &Value) -> Result<String, String> {
        let torrent = torrent.trim();
        if torrent.is_empty() {
            return Err("add_torrent 需要一个 magnet 链接或 base64 编码的 .torrent 内容".to_string());
        }
        // 1. 任务级选项：全局为基准 + 任务级覆盖（含 bt-tracker / select-file 等 BT 键）
        let mut task_opts = self
            .global
            .lock()
            .map_err(|e| format!("获取全局选项锁失败: {e}"))?
            .clone();
        task_opts.apply_task_options(options);
        // keep-seeding 未在任务级指定时，取 user.json 的配置（默认关闭）
        if options.get("keep-seeding").is_none() {
            if let Ok(cm) = self.config_manager.lock() {
                task_opts.keep_seeding = cm.user_config().keep_seeding;
            }
        }
        // 2. 组装 BT 添加选项（dir / trackers / select-file）
        let bt_opts = BtAddOptions {
            dir: task_opts.dir.clone(),
            trackers: task_opts.bt_trackers.clone(),
            only_files: task_opts.only_files.clone(),
            paused: false,
        };
        // 3. 懒创建 BT 引擎（首次 add_torrent 时初始化 librqbit Session）
        let engine = self.bt_engine(&task_opts)?;
        // 4. 生成 gid，注册事件回调。**回调捕获 Weak<TaskManager>**：TaskManager
        //    持有 BtEngine（强引用），若回调再持有 Arc<TaskManager> 会形成循环引用
        //    导致永不释放；Weak 升级失败（管理器已释放）时静默忽略迟到事件。
        let gid = generate_gid();
        let callback = {
            let weak = self
                .self_arc
                .lock()
                .map_err(|e| format!("获取自引用锁失败: {e}"))?
                .clone()
                .ok_or_else(|| "TaskManager 自引用未设置（请先调用 set_self）".to_string())?;
            let gid = gid.clone();
            move |ev: BtEvent| {
                if let Some(this) = weak.upgrade() {
                    this.on_bt_event(&gid, ev);
                }
            }
        };
        // 5. 分流：磁力 → metadata 任务；base64 .torrent → 完整任务
        if torrent.starts_with("magnet:") {
            let meta = engine.add_magnet(torrent, &bt_opts, &gid, callback)?;
            // metadata 任务：bittorrent 存在但 info_name=None、totalLength=0
            let mut task =
                Task::new_bt_task(&gid, torrent, &task_opts.dir, Some(meta.info_hash.clone()));
            task.total_length = 0;
            self.repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?
                .add(task);
        } else {
            // base64 解码 .torrent 内容 → librqbit 解析 → 完整任务
            let bytes = crate::bt::decode_torrent_base64(torrent)?;
            let meta = engine.add_torrent_file(&bytes, &bt_opts, &gid, callback)?;
            let mut task =
                Task::new_bt_task(&gid, torrent, &task_opts.dir, Some(meta.info_hash.clone()));
            // 填充完整元信息（files / totalLength / announce / info_name / mode）
            apply_torrent_meta(&mut task, &meta, task_opts.only_files.as_deref());
            self.repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?
                .add(task);
        }
        // 6. 登记源 / 任务级选项（恢复与续传取源）
        self.task_uris
            .lock()
            .map_err(|e| format!("获取源 URL 表锁失败: {e}"))?
            .insert(gid.clone(), vec![torrent.to_string()]);
        self.task_options
            .lock()
            .map_err(|e| format!("获取任务选项锁失败: {e}"))?
            .insert(gid.clone(), task_opts);
        // 7. 统一调度：槽位有空则启动最早 Waiting（含刚添加的 BT 任务）
        self.promote_waiting();
        Ok(gid)
    }

    /// 获取 BT 任务 peers（aria2.getPeers 兼容，limit 默认 100 分页；
    /// 当前 librqbit 8.1.1 未暴露 per-peer 明细，返回空数组，契约保留）
    pub fn get_peers(&self, gid: &str, limit: usize) -> Vec<crate::bt::PeerInfo> {
        match self.bt.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(engine) if engine.contains(gid) => engine.get_peers(gid, limit),
                _ => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    }

    /// 懒创建（或复用）BT 引擎
    fn bt_engine(&self, opts: &EngineOptions) -> Result<Arc<BtEngine>, String> {
        let mut guard = self
            .bt
            .lock()
            .map_err(|e| format!("获取 BT 引擎锁失败: {e}"))?;
        if let Some(engine) = guard.as_ref() {
            return Ok(engine.clone());
        }
        // listen-port 起始的 100 端口范围（避免多实例 / 并行测试端口冲突）
        let config = BtEngineConfig {
            listen_port_range: opts.bt_listen_port.map(|p| p..p.saturating_add(100)),
            // 禁用 DHT 持久化（迁移文档 7.1：librqbit 路由表重建可接受）：
            // 避免共享 dht.dat 复用端口导致 GUI/daemon 多实例或测试并行时的绑定冲突
            disable_dht_persistence: true,
        };
        let engine = BtEngine::new(config, opts.dir.clone())?;
        *guard = Some(engine.clone());
        Ok(engine)
    }

    /// 获取 BT 引擎；未初始化时按任务级选项懒创建
    ///
    /// 场景：应用重启后 checkpoint 恢复的 BT 任务 resume / promote 时，引擎
    /// （懒初始化）可能尚未建立（此前没有新的 add_torrent 调用），若直接取
    /// `self.bt` 会得到 None 而报"BT 引擎未初始化"，导致恢复的任务无法续传。
    fn bt_engine_for(&self, gid: &str) -> Result<Arc<BtEngine>, String> {
        // 已初始化：直接复用（短锁，命中即返回）
        {
            let guard = self
                .bt
                .lock()
                .map_err(|e| format!("获取 BT 引擎锁失败: {e}"))?;
            if let Some(engine) = guard.as_ref() {
                return Ok(engine.clone());
            }
        }
        // 未初始化：用任务级选项（缺省全局选项）创建；bt_engine 内部持锁创建，
        // 此处先释放上面的锁避免死锁
        let opts = self
            .task_options
            .lock()
            .map_err(|e| format!("获取任务选项锁失败: {e}"))?
            .get(gid)
            .cloned()
            .unwrap_or_else(|| {
                self.global
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_else(|e| {
                        warn!("[Motrix] 懒创建 BT 引擎获取全局选项锁失败，使用默认选项: {e}");
                        EngineOptions::default()
                    })
            });
        self.bt_engine(&opts)
    }

    /// 确保 BT 任务已登记进引擎（checkpoint 恢复的任务在重启后未加入 librqbit
    /// Session，resume / promote 时据此从 `files[0].uris[0]` 的源重新加入续传）
    fn ensure_bt_registered(&self, gid: &str) -> Result<(), String> {
        // 获取（必要时懒创建）BT 引擎：重启后恢复的 BT 任务首次 resume /
        // promote 时引擎可能尚未初始化（bt_engine_for 内部处理）
        let engine = self.bt_engine_for(gid)?;
        if engine.contains(gid) {
            return Ok(());
        }
        // 从仓库任务 + 任务级选项取源与 BT 选项
        let (source, bt_opts) = {
            let repo = self
                .repo
                .lock()
                .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
            let task = repo
                .get(gid)
                .ok_or_else(|| format!("任务不存在: {gid}"))?;
            let source = task
                .files
                .first()
                .and_then(|f| f.uris.first())
                .map(|(uri, _)| uri.clone())
                .ok_or_else(|| format!("BT 任务 {gid} 缺少源（files 为空）"))?;
            let opts = self
                .task_options
                .lock()
                .map_err(|e| format!("获取任务选项锁失败: {e}"))?
                .get(gid)
                .cloned()
                .unwrap_or_default();
            (
                source,
                BtAddOptions {
                    dir: task.dir.clone(),
                    trackers: opts.bt_trackers.clone(),
                    only_files: opts.only_files.clone(),
                    paused: false,
                },
            )
        };
        let callback = {
            // 与 add_torrent 一致：捕获 Weak<TaskManager>，避免循环引用
            let weak = match self.self_arc.lock() {
                Ok(guard) => guard.clone(),
                Err(e) => return Err(format!("获取自引用锁失败: {e}")),
            };
            let gid = gid.to_string();
            move |ev: BtEvent| {
                if let Some(this) = weak.as_ref().and_then(|w| w.upgrade()) {
                    this.on_bt_event(&gid, ev);
                }
            }
        };
        if source.starts_with("magnet:") {
            engine.add_magnet(&source, &bt_opts, gid, callback)?;
        } else {
            let bytes = crate::bt::decode_torrent_base64(&source)?;
            let meta = engine.add_torrent_file(&bytes, &bt_opts, gid, callback)?;
            // 重新加入时若任务仍处于 metadata 阶段（info_name=None），补齐元信息
            self.apply_bt_meta_if_pending(gid, &meta);
        }
        Ok(())
    }

    /// BT 引擎事件分发入口（轮询线程内调用，须快速返回）
    fn on_bt_event(&self, gid: &str, ev: BtEvent) {
        match ev {
            BtEvent::MetadataReady(meta) => self.bt_metadata_ready(gid, meta),
            BtEvent::Progress(progress) => self.bt_update_progress(gid, &progress),
            // 磁力后台添加失败：标记任务错误（如 magnet 无效 / metadata 解析失败）
            BtEvent::AddFailed(message) => {
                if let Ok(mut repo) = self.repo.lock() {
                    let _ = repo.mark_error(gid, 1, message);
                }
                self.emit_event(gid, "error");
            }
        }
    }

    /// 磁力 metadata 就绪：在同一 gid 上补齐完整 BT 元信息
    ///
    /// （磁力任务创建时 info_name=None / totalLength=0，前端 isMagnetTask；
    /// 就绪后填充 files / totalLength / announceList / info_name / mode）
    fn bt_metadata_ready(&self, gid: &str, meta: TorrentMeta) {
        // 任务级 only_files（文件选择）一并生效
        let only_files = self
            .task_options
            .lock()
            .map_err(|e| warn!("[Motrix] metadata 就绪获取任务选项失败: {e}"))
            .ok()
            .and_then(|g| g.get(gid).map(|o| o.only_files.clone()))
            .flatten();
        let mut repo = match self.repo.lock() {
            Ok(repo) => repo,
            Err(e) => {
                warn!("[Motrix] metadata 就绪获取任务仓库锁失败: {e}");
                return;
            }
        };
        let Some(mut task) = repo.get(gid).cloned() else { return };
        apply_torrent_meta(&mut task, &meta, only_files.as_deref());
        repo.add(task);
        drop(repo);
        // metadata 就绪可能让等待中的任务具备启动条件，唤醒调度
        self.promote_waiting();
    }

    /// 重新加入引擎时若任务仍处于 metadata 阶段，补齐元信息（ensure_bt_registered 用）
    fn apply_bt_meta_if_pending(&self, gid: &str, meta: &TorrentMeta) {
        let only_files = self
            .task_options
            .lock()
            .ok()
            .and_then(|g| g.get(gid).map(|o| o.only_files.clone()))
            .flatten();
        if let Ok(mut repo) = self.repo.lock() {
            if let Some(mut task) = repo.get(gid).cloned() {
                // 仅补齐 metadata 阶段的任务（info_name 仍为 None）
                let pending = task
                    .bittorrent
                    .as_ref()
                    .map(|b| b.info_name.is_none())
                    .unwrap_or(false);
                if pending {
                    apply_torrent_meta(&mut task, meta, only_files.as_deref());
                    repo.add(task);
                }
            }
        }
    }

    /// BT 进度回调：更新任务仓库字段 + 处理"下载完成 → Seeding / Complete"状态迁移
    ///
    /// - 下载完成（`progress.finished` 首次为 true）：
    ///   - `keep-seeding` 开启 → Seeding（保留做种，速度 / peers 继续上报）；
    ///   - 否则 → Complete，并连发 `complete`（下载完成）+ `bt-complete`（做种结束，
    ///     下载完成即做种结束）；
    /// - 做种中任务由用户暂停 / 移除终止（pause 对 Seeding 状态返回错误，见 pause_inner）。
    pub fn bt_update_progress(&self, gid: &str, p: &BtProgress) {
        let keep_seeding = self
            .task_options
            .lock()
            .map_err(|e| warn!("[Motrix] BT 进度回调获取任务选项失败: {e}"))
            .ok()
            .and_then(|g| g.get(gid).map(|o| o.keep_seeding))
            .unwrap_or(false);
        let mut repo = match self.repo.lock() {
            Ok(repo) => repo,
            Err(e) => {
                warn!("[Motrix] BT 进度回调获取任务仓库锁失败: {e}");
                return;
            }
        };
        let Some(task) = repo.get(gid) else { return };
        // 已暂停 / 已移除 / 已出错任务忽略迟到的进度事件
        if !matches!(
            task.status,
            TaskStatus::Active | TaskStatus::Waiting | TaskStatus::Seeding
        ) {
            return;
        }
        // 进度 / 速度 / 做种字段
        let _ = repo.update_progress(
            gid,
            p.completed,
            p.total,
            p.download_speed,
            p.upload_speed,
            // connections：librqbit 无连接数概念，用 live peers 数近似
            p.num_seeders,
        );
        if let Some(task) = repo.get_mut(gid) {
            task.upload_length = p.uploaded_bytes;
            task.num_seeders = p.num_seeders;
            task.seeder = p.seeder;
            task.bitfield = p.bitfield.clone();
        }
        // 文件级进度同步（librqbit stats.file_progress 与文件列表一一对应）
        if let Some(task) = repo.get_mut(gid) {
            if task.files.len() == p.file_progress.len() {
                for (f, done) in task.files.iter_mut().zip(&p.file_progress) {
                    f.completed_length = *done;
                    f.length = f.length.max(*done);
                }
            }
        }
        // 做种上限检查：keep-seeding 进入做种后，每轮 500ms 回调都会走到这里；
        // 满足 seed-time（做种时长）/ seed-ratio（上传 / 下载比率）任一上限即结束
        // 做种（置 Complete + bt-complete + 释放并发槽位），随后由用户暂停 / 移除
        // 终止的语义不再需要（spec：达到上限自动触发 bt-complete）
        let is_seeding = repo
            .get(gid)
            .map(|t| t.status == TaskStatus::Seeding)
            .unwrap_or(false);
        if is_seeding {
            // 读取做种上限配置（缺省视为不限；与 task_options 短锁交互，读后即释放）
            let (seed_ratio, seed_time) = self
                .task_options
                .lock()
                .map_err(|e| warn!("[Motrix] BT 进度回调获取任务选项失败: {e}"))
                .ok()
                .and_then(|g| g.get(gid).map(|o| (o.seed_ratio, o.seed_time)))
                .unwrap_or((None, None));
            // 做种起始时刻：无记录（异常路径）视为刚进入做种，elapsed=0 不误判
            let elapsed_secs = self
                .bt_seed_started
                .lock()
                .ok()
                .and_then(|m| m.get(gid).copied())
                .map(|start| start.elapsed().as_secs())
                .unwrap_or(0);
            if seeding_limit_reached(seed_ratio, seed_time, p.uploaded_bytes, p.total, elapsed_secs) {
                // 结束做种：置 Complete + bt-complete 事件 + 从运行集合移除（释放并发槽位）
                let _ = repo.set_status(gid, TaskStatus::Complete);
                self.emit_event(gid, "bt-complete");
                if let Ok(mut active) = self.bt_active.lock() {
                    active.remove(gid);
                }
                // 清理做种起始时刻（避免内存泄漏与后续过期时刻误判）
                if let Ok(mut started) = self.bt_seed_started.lock() {
                    started.remove(gid);
                }
                drop(repo);
                self.promote_waiting();
                return;
            }
        }
        // 下载完成 → 状态迁移（只触发一次）
        let already_done = self
            .bt_done
            .lock()
            .map(|g| g.contains(gid))
            .unwrap_or(true);
        if p.finished && !already_done {
            if keep_seeding {
                // 进入做种：保留并发槽位（继续上传），通知"下载完成"；
                // 记录做种起始时刻（seed-time / seed-ratio 上限检查用；
                // or_insert 保证重复回调不重置已注入的时刻）
                let _ = repo.set_status(gid, TaskStatus::Seeding);
                if let Ok(mut started) = self.bt_seed_started.lock() {
                    started
                        .entry(gid.to_string())
                        .or_insert_with(std::time::Instant::now);
                }
                self.emit_event(gid, "complete");
            } else {
                // 不做种：直接完成，下载完成即做种结束（bt-complete）
                let _ = repo.set_status(gid, TaskStatus::Complete);
                self.emit_event(gid, "complete");
                self.emit_event(gid, "bt-complete");
                // 释放并发槽位（唤醒下一个等待任务）
                if let Ok(mut active) = self.bt_active.lock() {
                    active.remove(gid);
                }
                drop(repo);
                self.promote_waiting();
            }
            if let Ok(mut done) = self.bt_done.lock() {
                done.insert(gid.to_string());
            }
        }
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
            // BT 任务同样占用并发槽位（active 为 KGet 句柄、bt_active 为 BT 任务集合）
            let num_bt_active = match self.bt_active.lock() {
                Ok(guard) => guard.len(),
                Err(e) => {
                    warn!("[Motrix] promote_waiting 获取 BT 运行集合锁失败: {e}");
                    return;
                }
            };
            if num_active + num_bt_active >= max.max(1) as usize {
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
            //    （BT 任务：确保已登记进 librqbit Session 后即视为已启动；
            //    HTTP 任务：KGet spawn）
            let is_bt = {
                let repo = match self.repo.lock() {
                    Ok(repo) => repo,
                    Err(e) => {
                        warn!("[Motrix] promote_waiting 获取任务仓库锁失败: {e}");
                        return;
                    }
                };
                repo.get(&gid)
                    .map(|t| t.bittorrent.is_some())
                    .unwrap_or(false)
            };
            if is_bt {
                // BT 任务启动 = 登记运行集合（librqbit 在 add_torrent / ensure_bt_registered
                // 时已开始下载；checkpoint 恢复的任务在此补登记）
                if let Err(e) = self.ensure_bt_registered(&gid) {
                    if let Ok(mut repo) = self.repo.lock() {
                        let _ = repo.mark_error(&gid, 1, e);
                    }
                    continue;
                }
                if let Ok(mut active) = self.bt_active.lock() {
                    active.insert(gid.clone());
                }
                // Task 11：下载开始事件（与 KGet spawn_one 对齐）
                self.emit_event(&gid, "start");
            } else if let Err(e) = self.spawn_one(&gid) {
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
        // HTTP(S) 首选 fast-down 并行引擎（fastdown.rs）：多连接 + 侧车状态
        // 字节级续传（暂停/恢复不再从头）。初始化失败（代理认证不支持 / 输出
        // 目录不可写等）回退顺序引擎（http.rs）——已有字节（暂停 / checkpoint
        // 恢复后的 completed_length）经 Range 续传追加写；非 HTTP（FTP/WebDAV/
        // Metalink 等）走 KGet（既有路径）。
        let handle = if url.starts_with("http://") || url.starts_with("https://") {
            match crate::fastdown::spawn_fast_down_download(
                gid.to_string(),
                &url,
                &options,
                on_event.clone(),
            ) {
                Ok(h) => EngineHandle::FastDown(h),
                Err(e) => {
                    warn!("[Motrix] fast-down 启动失败，回退顺序引擎: {e}");
                    // 已有字节：取任务已完成长度（顺序下载时文件即连续前缀，暂停已截断）
                    let existing = self
                        .repo
                        .lock()
                        .ok()
                        .and_then(|repo| repo.get(gid).map(|t| t.completed_length))
                        .unwrap_or(0);
                    EngineHandle::Http(
                        crate::http::spawn_http_download(
                            gid.to_string(),
                            &url,
                            &options,
                            existing,
                            on_event,
                        )
                        .map_err(|e| format!("启动下载失败: {e}"))?,
                    )
                }
            }
        } else {
            EngineHandle::Kget(
                kget::spawn_download(gid.to_string(), &url, &options, on_event)
                    .map_err(|e| format!("启动下载失败: {e}"))?,
            )
        };
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

/// 判定 BT 做种上限是否已达到（seed-ratio / seed-time 任一满足即视为达到）
///
/// - `seed_time`：做种时长上限（秒），`None` 表示不限时；
/// - `seed_ratio`：做种比率上限（上传 / 下载，如 2.0 = 上传达下载量的 200%），
///   `None` 表示不限比；`total == 0` 时比率无法计算，跳过该判定项；
/// - 两者均未设置 → 恒为 `false`（keep-seeding 下无限做种，由用户暂停 / 移除终止）。
fn seeding_limit_reached(
    seed_ratio: Option<f64>,
    seed_time: Option<u32>,
    uploaded: u64,
    total: u64,
    elapsed_secs: u64,
) -> bool {
    // 做种时长达到上限（elapsed_secs 为已做种秒数）
    if let Some(t) = seed_time {
        if elapsed_secs >= t as u64 {
            return true;
        }
    }
    // 做种比率达到上限（total > 0 才可计算；total == 0 跳过此项）
    if let Some(r) = seed_ratio {
        if total > 0 && (uploaded as f64 / total as f64) >= r {
            return true;
        }
    }
    false
}

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

/// 把种子元信息应用到任务（add_torrent / metadata 就绪回调共用，Phase 3）
///
/// 填充：`bittorrent`（info_name / mode / announce_list / info_hash）、
/// `totalLength`、`files`（绝对路径 = dir + 相对路径，`selected` 按 select-file；
/// 多文件任务 files[].completedLength 由进度回调同步）。**保留源**
/// （add_torrent 时写入 `files[0].uris[0]`）供 checkpoint 恢复 / 续传取源。
fn apply_torrent_meta(task: &mut Task, meta: &TorrentMeta, only_files: Option<&[usize]>) {
    task.total_length = meta.total_length;
    if let Some(bt) = task.bittorrent.as_mut() {
        bt.info_name = meta.name.clone();
        bt.mode = meta.mode.clone();
        // announce_list：aria2 为二维数组（每层为同源 tracker 组），平铺后单元素组
        bt.announce_list = meta.announce_list.iter().map(|t| vec![t.clone()]).collect();
        bt.info_hash = Some(meta.info_hash.clone());
    }
    // 保留源（metadata 就绪重建 files 时不能丢）
    let source = task
        .files
        .first()
        .and_then(|f| f.uris.first())
        .map(|(uri, status)| (uri.clone(), status.clone()));
    // 选中列表：select-file 未指定 → 全选
    let selected: Vec<bool> = match only_files {
        Some(indices) => (0..meta.files.len()).map(|i| indices.contains(&i)).collect(),
        None => vec![true; meta.files.len()],
    };
    task.files = meta
        .files
        .iter()
        .zip(selected)
        .map(|(f, sel)| TaskFile {
            // TaskFile.index 为 u32（aria2 惯例），TorrentFileMeta.index 为 usize
            index: f.index as u32,
            path: join_save_path(&task.dir, &f.path),
            length: f.length,
            completed_length: 0,
            selected: sel,
            uris: Vec::new(),
        })
        .collect();
    // 源写回 files[0].uris（BT 任务的文件列表以 metadata 为准，源仅用于恢复取源）
    if let Some((uri, status)) = source {
        if let Some(first) = task.files.first_mut() {
            first.uris.push((uri, status));
        }
    }
}

/// 拼接保存目录与种子内相对路径（`dir + "/" + rel`，兼容 dir 尾部分隔符）
fn join_save_path(dir: &str, rel: &str) -> String {
    let dir = dir.trim_end_matches(['/', '\\']);
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
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

    // BT 测试串行锁：定义与说明见 crate::bt::BT_TEST_LOCK（bt.rs 与 engine.rs
    // 的 BT 会话测试共用同一把锁，避免 librqbit DHT 端口并行冲突）
    use crate::bt::BT_TEST_LOCK;

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
            // 解析 Range: bytes=start-end（闭区间）或 bytes=start-（省略 end = 文件末尾）；
            // **必须按请求区间精确返回**：fast-down 的 Range 探测发 `bytes=0-0` 并要求
            // Content-Range 以 `bytes 0-0/` 开头，若像旧实现那样返回整个尾部会导致
            // fast-down 误判服务器不支持 Range（走单连接 + 不可续传路径）。
            let spec = range
                .split("bytes=")
                .nth(1)
                .unwrap_or("")
                .trim()
                .to_string();
            let (s, e) = match spec.split_once('-') {
                Some((s, e)) => (s.trim().to_string(), e.trim().to_string()),
                None => (spec.clone(), String::new()),
            };
            let last = total.saturating_sub(1);
            let start: u64 = s.parse().unwrap_or(0).min(last);
            let end: u64 = if e.is_empty() {
                last
            } else {
                e.parse().unwrap_or(last).min(last)
            };
            if start <= end {
                body = &content[start as usize..=end as usize];
            } else {
                body = &[];
            }
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
    // 测试 2：暂停后 abort，恢复后从断点继续（http.rs 顺序引擎 Range 续传）直至完成
    // ------------------------------------------------------------------
    #[test]
    fn pause_resume_resumes_to_complete() {
        // 8MB 内容 + 每块 4ms 发送延迟（总时长约 512ms）：保证下载过程可被暂停
        // 打断（2MB/2ms 下顺序引擎完成太快，pause 可能追不上 → 状态已 Complete）
        let content = vec![7u8; 8 * 1024 * 1024];
        let server = TestHttpServer::start_with_delay(content.clone(), Duration::from_millis(4));
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
        // 等待出现实际进度（顺序引擎启动极快，Active 可能早于首个进度事件；
        // 确保暂停发生在下载中段，验证"暂停保留进度 + 截断 + 续传"）
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let completed = repo
                    .lock()
                    .unwrap()
                    .get(gid)
                    .map(|t| t.completed_length)
                    .unwrap_or(0);
                if completed > 0 {
                    break;
                }
                assert!(Instant::now() < deadline, "下载应在 10s 内产生进度");
                thread::sleep(Duration::from_millis(20));
            }
        }

        // 暂停：abort 引擎句柄 + 置 Paused + 把部分文件截断到已完成字节
        tm.pause(gid).expect("暂停应成功");
        assert_eq!(
            repo.lock().unwrap().get(gid).unwrap().status,
            TaskStatus::Paused
        );
        // 已暂停任务再次暂停：幂等
        tm.pause(gid).expect("重复暂停应幂等");

        // 暂停后应保留了部分进度（顺序引擎真实续传的基础：completed > 0，
        // 且磁盘文件被截断到 completed，恢复时 Range 续传而非从头重下）
        let paused = repo.lock().unwrap().get(gid).unwrap().clone();
        assert!(
            paused.completed_length > 0 && paused.completed_length < content.len() as u64,
            "暂停时应保留部分进度，实际 completed={}",
            paused.completed_length
        );
        let disk_size = std::fs::metadata(&paused.files[0].path)
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(
            disk_size, paused.completed_length,
            "暂停后部分文件应被截断到已完成字节（供恢复续传）"
        );

        // 恢复：置 Active 并重新 spawn（顺序引擎基于已有字节 Range 续传）
        tm.resume(gid).expect("恢复应成功");
        assert!(wait_status(&repo, gid, TaskStatus::Complete));

        // 文件完整（续传未损坏内容）
        assert_completed_with_file(&repo, gid, &content);
    }

    // ------------------------------------------------------------------
    // 测试 2b：并行下载（connections=4）暂停 → 恢复 → 字节级续传直至完成
    // fast-down 并行分块 + 侧车状态（*.fd.json）：暂停后文件截断到最大已写端点、
    // 侧车记录已完成区间（clean=true）；恢复时校验 FileId 后 invert 求剩余区间
    // 并行续传，最终文件完整（本测试覆盖"并行 + 精确恢复"的核心价值路径）。
    // ------------------------------------------------------------------
    #[test]
    fn parallel_pause_resume_resumes_to_complete() {
        // 8MB + 每块 20ms 发送延迟：4 连接并行下总时长约 8MB/(4×3.2MB/s)≈625ms，
        // 可被暂停稳定打断（对比测试 2 的单连接顺序路径）
        let content = vec![11u8; 8 * 1024 * 1024];
        let server = TestHttpServer::start_with_delay(content.clone(), Duration::from_millis(20));
        let (tm, repo, _temp) = test_manager(1);

        let gids = tm
            .add_uri(
                &[server.url("/file")],
                &json!({"out": "pp.bin", "connections": 4}),
            )
            .expect("add_uri 应成功");
        let gid = &gids[0];
        // 等待下载开始
        assert!(wait_status(&repo, gid, TaskStatus::Active));
        // 等待出现实际进度
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let completed = repo
                    .lock()
                    .unwrap()
                    .get(gid)
                    .map(|t| t.completed_length)
                    .unwrap_or(0);
                if completed > 0 {
                    break;
                }
                assert!(Instant::now() < deadline, "下载应在 10s 内产生进度");
                thread::sleep(Duration::from_millis(20));
            }
        }

        // 暂停：置 Paused；fast-down 引擎线程退出前已截断文件 + 持久化 clean 侧车
        tm.pause(gid).expect("暂停应成功");
        assert_eq!(
            repo.lock().unwrap().get(gid).unwrap().status,
            TaskStatus::Paused
        );
        let paused = repo.lock().unwrap().get(gid).unwrap().clone();
        assert!(
            paused.completed_length > 0 && paused.completed_length < content.len() as u64,
            "暂停时应保留部分进度，实际 completed={}",
            paused.completed_length
        );
        // 侧车状态文件已写入且标记 clean（恢复的唯一权威依据）
        let sidecar = format!("{}.fd.json", paused.files[0].path);
        assert!(Path::new(&sidecar).exists(), "暂停后应写入侧车状态文件");
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap())
                .expect("侧车状态应为合法 JSON");
        assert_eq!(
            state["clean"].as_bool(),
            Some(true),
            "正常暂停后侧车应标记 clean（可安全续传）"
        );

        // 恢复：fast-down 读侧车 → invert 剩余区间 → 并行续传 → 完成
        tm.resume(gid).expect("恢复应成功");
        assert!(wait_status(&repo, gid, TaskStatus::Complete));

        // 文件完整（并行分块 + 字节级续传未损坏内容）
        assert_completed_with_file(&repo, gid, &content);
        // 完成后侧车已清理（任务完结，无需恢复依据）
        assert!(
            !Path::new(&sidecar).exists(),
            "下载完成后应删除侧车状态文件"
        );
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
    // 测试 7：add_torrent 真实实现（BT 任务创建 + metadata 阶段字段）/ get 查询 /
    //         purge 清空 removed 历史
    // ------------------------------------------------------------------
    #[test]
    fn add_torrent_creates_bt_task_and_purge_clears_removed() {
        // 持 BT 测试串行锁（见 BT_TEST_LOCK 注释：librqbit DHT 共享 dht.dat 端口）
        let _guard = BT_TEST_LOCK.lock().unwrap();
        let (tm, repo, _temp) = test_manager(1);
        // add_torrent：真实实现（magnet → metadata 任务，引擎懒初始化 librqbit Session）
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";
        let gid = tm
            .add_torrent(magnet, &json!({}))
            .expect("add_torrent 应创建 BT 任务");
        // gid 为 16 位小写 hex（与 aria2 一致）
        assert_eq!(gid.len(), 16);
        assert!(
            gid.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "gid 应为小写 hex: {gid}"
        );

        // 任务存在于仓库：bittorrent 已填充、处于磁力 metadata 阶段（info_name=None）
        let task = repo
            .lock()
            .unwrap()
            .get(&gid)
            .expect("BT 任务应存在于仓库")
            .clone();
        let bt = task.bittorrent.as_ref().expect("BT 任务应带 bittorrent 信息");
        assert_eq!(
            bt.info_hash.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert!(
            bt.info_name.is_none(),
            "磁力 metadata 阶段 info_name 应为 None（前端 isMagnetTask 依赖）"
        );
        // totalLength=0（metadata 未就绪），源已写入 files[0].uris[0]
        assert_eq!(task.total_length, 0);
        assert_eq!(task.files[0].uris[0].0, magnet);
        // 再次添加同一磁力：返回新 gid（引擎去重由 librqbit 处理，任务各自独立）
        let gid3 = tm
            .add_torrent(magnet, &json!({}))
            .expect("再次添加磁力应成功");
        assert_ne!(gid3, gid, "每次 add_torrent 应生成不同 gid");

        // get：不存在返回 None；存在返回任务克隆
        assert!(tm.get("0123456789abcdef").is_none());
        let gid2 = "abcdef0123456789";
        let task = Task::new_http_task(gid2, &["http://127.0.0.1:1/x".to_string()], "/tmp", "x.bin");
        repo.lock().unwrap().add(task);
        assert_eq!(tm.get(gid2).unwrap().gid, gid2);

        // purge：仅清空 removed 历史（stopped 列表），在册任务不受影响
        repo.lock().unwrap().remove(gid2);
        assert!(!repo.lock().unwrap().stopped().is_empty());
        tm.purge();
        assert!(repo.lock().unwrap().stopped().is_empty());
        // 在册任务（BT 任务）不受 purge 影响
        assert!(repo.lock().unwrap().contains(&gid));
    }

    // ------------------------------------------------------------------
    // 测试 7b：add_uri 对 magnet: 链接按 aria2 语义路由到 BT 引擎
    // （前端 AddTask 的 URI 标签页把磁力走 addUri 通道；若按 HTTP 处理会
    //  报 builder error，本测试断言返回 gid 且任务为磁力 metadata 阶段）
    // ------------------------------------------------------------------
    #[test]
    fn add_uri_routes_magnet_to_bt_engine() {
        // 持 BT 测试串行锁（见 BT_TEST_LOCK 注释：librqbit DHT 共享 dht.dat 端口）
        let _guard = BT_TEST_LOCK.lock().unwrap();
        let (tm, repo, _temp) = test_manager(1);
        let magnet = "magnet:?xt=urn:btih:deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let gids = tm
            .add_uri(&[magnet.to_string()], &json!({}))
            .expect("add_uri 对 magnet 应路由到 BT 引擎并成功");
        assert_eq!(gids.len(), 1);
        let gid = &gids[0];
        assert_eq!(gid.len(), 16);

        // 任务为磁力 metadata 阶段（bittorrent 存在、info_name=None、totalLength=0）
        let task = repo
            .lock()
            .unwrap()
            .get(gid)
            .expect("magnet 路由应创建任务")
            .clone();
        let bt = task.bittorrent.as_ref().expect("应为 BT 任务");
        assert!(bt.info_name.is_none(), "磁力 metadata 阶段应省略 info");
        assert_eq!(task.total_length, 0);
        // 混合场景：magnet + HTTP URL 一起添加，各自创建任务
        let gids2 = tm
            .add_uri(
                &[magnet.to_string(), "http://127.0.0.1:9/file".to_string()],
                &json!({}),
            )
            .expect("混合 magnet + http 应成功");
        assert_eq!(gids2.len(), 2);
        let bt_task = repo.lock().unwrap().get(&gids2[0]).unwrap().clone();
        assert!(bt_task.bittorrent.is_some(), "第一个应为 BT 任务");
    }

    // ------------------------------------------------------------------
    // 测试 7c：重启后恢复的 BT 任务可 resume（懒创建 BT 引擎）
    // 模拟 checkpoint 恢复：任务直接进仓库（Paused、带 bittorrent），且
    // `tm.bt` 引擎未初始化（本进程没有 add_torrent 调用）——resume 应懒创建
    // 引擎并成功（此前会因"BT 引擎未初始化"失败，导致暂停后重启无法续传）。
    // ------------------------------------------------------------------
    #[test]
    fn resume_bt_task_after_restart_lazily_creates_engine() {
        // 持 BT 测试串行锁（见 BT_TEST_LOCK 注释）
        let _guard = BT_TEST_LOCK.lock().unwrap();
        let (tm, repo, _temp) = test_manager(1);
        // 模拟 checkpoint 恢复：BT 任务直接进仓库（Paused），未登记 task_options
        let gid = "aaaa1111bbbb2222".to_string();
        let mut task = Task::new_bt_task(
            &gid,
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            "/tmp/dl",
            Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        );
        task.status = TaskStatus::Paused; // 导入恢复场景直接赋值（绕过状态机）
        repo.lock().unwrap().add(task);
        // 引擎尚未初始化（模拟重启后首启）
        assert!(
            tm.bt.lock().unwrap().is_none(),
            "测试前置：BT 引擎应尚未初始化"
        );

        // 恢复：应懒创建 BT 引擎并成功置 Active
        tm.resume(&gid).expect("重启后恢复 BT 任务应成功");
        assert!(
            tm.bt.lock().unwrap().is_some(),
            "resume 后 BT 引擎应已懒创建"
        );
        let restored = repo.lock().unwrap().get(&gid).unwrap().clone();
        assert_eq!(restored.status, TaskStatus::Active);
        // 已登记运行集合（占用并发槽位）
        assert!(tm.bt_active.lock().unwrap().contains(&gid));
    }

    // ------------------------------------------------------------------
    // 测试 8：checkpoint 恢复任务兜底（Task 13 续传边界修复）
    // 构造 Task（files[0].uris 有 URL、Paused）直接进仓库、不登记 task_uris /
    // task_options（与 session.rs::restore_checkpoint 现状一致），resume 应能
    // 经 files 兜底 URL + 全局选项兜底启动引擎并下载完成。
    // **边界**：任务记录了 completed_length > 0（checkpoint 进度）但磁盘文件缺失
    // （旧版暂停删文件兜底 / 手动删除 / 数据清理）——顺序引擎必须"从头下载"，
    // 绝不能把零字节文件 set_len 扩展成假前缀导致损坏。
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
        // checkpoint 记录了部分进度，但**磁盘上没有该文件**（旧版删文件兜底场景）
        task.completed_length = 10_000;
        let out_path = task.files[0].path.clone();
        repo.lock().unwrap().add(task);
        assert!(
            !std::path::Path::new(&out_path).exists(),
            "测试前置：文件应不存在（模拟旧版删文件兜底）"
        );

        // 恢复：内部表缺失时应从 files[0].uris 兜底 URL 并启动引擎；
        // 文件缺失 + completed>0 → 顺序引擎从头下载（不产生零前缀假文件）
        tm.resume(&gid).expect("恢复 checkpoint 任务应成功");
        assert!(wait_status(&repo, &gid, TaskStatus::Complete), "兜底恢复的任务未完成");

        // 下载写回了任务的保存路径且内容与服务器一致（从头下载，无零字节假前缀）
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

    // ------------------------------------------------------------------
    // 测试 10：change_option 的 select-file 即时下发（Phase 3 修复 1）
    // ------------------------------------------------------------------

    /// 便捷构造一个带 bittorrent 元信息的 BT 任务并注入仓库 + task_options
    fn inject_bt_task(tm: &Arc<TaskManager>, gid: &str) {
        let task = Task::new_bt_task(
            gid,
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            "/tmp/dl",
            Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        );
        tm.repo.lock().unwrap().add(task);
        tm.task_options
            .lock()
            .unwrap()
            .insert(gid.to_string(), EngineOptions::default());
    }

    /// 便捷构造 Seeding 状态 BT 任务（keep-seeding 开启，做种上限可配置）
    fn inject_seeding_task(
        tm: &Arc<TaskManager>,
        gid: &str,
        seed_ratio: Option<f64>,
        seed_time: Option<u32>,
        started_secs_ago: u64,
        total: u64,
        uploaded: u64,
    ) {
        let mut task = Task::new_bt_task(
            gid,
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            "/tmp/dl",
            Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        );
        task.status = TaskStatus::Seeding;
        task.total_length = total;
        task.completed_length = total;
        tm.repo.lock().unwrap().add(task);
        let mut opts = EngineOptions::default();
        opts.keep_seeding = true;
        opts.seed_ratio = seed_ratio;
        opts.seed_time = seed_time;
        tm.task_options.lock().unwrap().insert(gid.to_string(), opts);
        // 做种起始时刻注入为 started_secs_ago 秒前（模拟已做种一段时间）
        tm.bt_seed_started
            .lock()
            .unwrap()
            .insert(gid.to_string(), Instant::now() - Duration::from_secs(started_secs_ago));
        // 模拟运行中任务（占用并发槽位）与已发过"下载完成"通知
        tm.bt_active.lock().unwrap().insert(gid.to_string());
        tm.bt_done.lock().unwrap().insert(gid.to_string());
    }

    /// 便捷构造一轮做种中的进度回调（finished=true）
    fn seeding_progress(total: u64, uploaded: u64) -> BtProgress {
        BtProgress {
            completed: total,
            total,
            uploaded_bytes: uploaded,
            download_speed: 0,
            upload_speed: 0,
            num_seeders: 1,
            seeder: true,
            bitfield: String::new(),
            file_progress: vec![total],
            finished: true,
        }
    }

    #[test]
    fn change_option_select_file_dispatches_to_bt() {
        let (tm, _repo, _temp) = test_manager(1);
        let gid = "0123456789abcdef".to_string();
        inject_bt_task(&tm, &gid);

        // BT 任务 + select-file：先更新 task_options，再尝试下发引擎。
        // 引擎未初始化（测试环境不拉起 librqbit）→ 返回明确错误，而非静默忽略。
        let err = tm
            .change_option(&gid, &json!({"select-file": "1,3"}))
            .expect_err("BT 引擎未初始化时 select-file 下发应返回错误");
        assert!(
            err.contains("BT 引擎未初始化"),
            "错误信息应提示引擎未初始化: {err}"
        );
        // 但 task_options 已更新：select-file 1 起始索引 → 0 起始（1→0、3→2）
        let opts = tm.task_options.lock().unwrap();
        assert_eq!(
            opts.get(&gid).unwrap().only_files,
            Some(vec![0, 2]),
            "select-file 的 1 起始索引应转换为 0 起始存入 task_options"
        );
    }

    #[test]
    fn change_option_select_file_ignored_for_http() {
        let (tm, repo, _temp) = test_manager(1);
        // 注入 HTTP 任务（离线：不经过 add_uri 的真实引擎启动）
        let gid = "fedcba9876543210".to_string();
        let task = Task::new_http_task(
            gid.clone(),
            &["http://127.0.0.1:9/x".to_string()],
            "/tmp",
            "x.bin",
        );
        repo.lock().unwrap().add(task);
        tm.task_options
            .lock()
            .unwrap()
            .insert(gid.clone(), EngineOptions::default());

        // 非 BT 任务：select-file 只更新 task_options，不触发引擎下发（返回 OK）
        tm.change_option(&gid, &json!({"select-file": "1,2"}))
            .expect("非 BT 任务应忽略 select-file 下发并返回 OK");
        let opts = tm.task_options.lock().unwrap();
        assert_eq!(
            opts.get(&gid).unwrap().only_files,
            Some(vec![0, 1]),
            "HTTP 任务的 select-file 也应解析并存入 task_options"
        );
    }

    // ------------------------------------------------------------------
    // 测试 11：seeding_limit_reached 纯函数（做种上限判定）
    // ------------------------------------------------------------------
    #[test]
    fn seeding_limit_reached_pure_logic() {
        // 未设置任何上限 → 恒 false（keep-seeding 无限做种）
        assert!(!seeding_limit_reached(None, None, 999, 1000, 99999));
        // seed-time 上限：elapsed >= seed_time 秒即满足
        assert!(!seeding_limit_reached(None, Some(10), 0, 1000, 9));
        assert!(seeding_limit_reached(None, Some(10), 0, 1000, 10));
        assert!(seeding_limit_reached(None, Some(10), 0, 1000, 11));
        // seed-ratio 上限：uploaded / total >= ratio（total > 0 才计算）
        assert!(!seeding_limit_reached(Some(2.0), None, 1999, 1000, 0));
        assert!(seeding_limit_reached(Some(2.0), None, 2000, 1000, 0));
        assert!(seeding_limit_reached(Some(2.0), None, 3000, 1000, 0));
        // total == 0：跳过 seed-ratio 项（仅 seed-time 生效）
        assert!(!seeding_limit_reached(Some(2.0), None, 999, 0, 0));
        assert!(!seeding_limit_reached(Some(2.0), None, 999, 0, 5));
        assert!(seeding_limit_reached(Some(2.0), Some(5), 999, 0, 5));
        // 任一满足即达到（ratio 已超、time 未到）
        assert!(seeding_limit_reached(Some(1.5), Some(999), 1500, 1000, 1));
        assert!(seeding_limit_reached(Some(999.0), Some(60), 0, 1000, 60));
    }

    // ------------------------------------------------------------------
    // 测试 12：bt_update_progress 做种上限结束做种（Phase 3 修复 2）
    // ------------------------------------------------------------------

    /// seed-time 超上限：Seeding → Complete + bt-complete 事件 + 释放并发槽位
    #[test]
    fn bt_update_progress_ends_seeding_when_time_limit_reached() {
        let (tm, repo, _temp) = test_manager(1);
        let gid = "0123456789abcdef".to_string();
        // 已做种 2 秒、seed-time=1 秒 → 上限已超
        inject_seeding_task(&tm, &gid, None, Some(1), 2, 1000, 0);
        let mut rx = tm.subscribe_events();

        tm.bt_update_progress(&gid, &seeding_progress(1000, 0));

        // 状态转 Complete + bt-complete 事件
        assert_eq!(
            repo.lock().unwrap().get(&gid).unwrap().status,
            TaskStatus::Complete,
            "做种时长达到 seed-time 上限后应结束做种"
        );
        let ev = rx.try_recv().expect("应收到 bt-complete 事件");
        assert_eq!(ev.event, "bt-complete", "做种结束应发出 bt-complete 事件");
        // 释放并发槽位 + 清理做种起始时刻
        assert!(
            !tm.bt_active.lock().unwrap().contains(&gid),
            "做种结束应从运行集合移除（释放并发槽位）"
        );
        assert!(
            !tm.bt_seed_started.lock().unwrap().contains_key(&gid),
            "做种结束应清理做种起始时刻记录"
        );
    }

    /// seed-ratio 超上限：同样结束做种（上传 / 下载比率达标）
    #[test]
    fn bt_update_progress_ends_seeding_when_ratio_limit_reached() {
        let (tm, repo, _temp) = test_manager(1);
        let gid = "0123456789abcdef".to_string();
        // seed-ratio=2.0：上传 2000 字节 / 总量 1000 → 比率 2.0 达标
        inject_seeding_task(&tm, &gid, Some(2.0), None, 0, 1000, 2000);
        let mut rx = tm.subscribe_events();

        tm.bt_update_progress(&gid, &seeding_progress(1000, 2000));

        assert_eq!(
            repo.lock().unwrap().get(&gid).unwrap().status,
            TaskStatus::Complete,
            "上传比率达到 seed-ratio 上限后应结束做种"
        );
        let ev = rx.try_recv().expect("应收到 bt-complete 事件");
        assert_eq!(ev.event, "bt-complete");
    }

    /// 未达上限：保持 Seeding，不发 bt-complete，不释放槽位
    #[test]
    fn bt_update_progress_keeps_seeding_before_limit() {
        let (tm, repo, _temp) = test_manager(1);
        let gid = "0123456789abcdef".to_string();
        // 已做种 1 秒但 seed-time=60、seed-ratio=10：均未达上限
        inject_seeding_task(&tm, &gid, Some(10.0), Some(60), 1, 1000, 100);
        let mut rx = tm.subscribe_events();

        tm.bt_update_progress(&gid, &seeding_progress(1000, 100));

        // 状态保持 Seeding、做种起始时刻保留、并发槽位保留、无 bt-complete 事件
        assert_eq!(
            repo.lock().unwrap().get(&gid).unwrap().status,
            TaskStatus::Seeding,
            "未达做种上限时应保持 Seeding"
        );
        assert!(tm.bt_seed_started.lock().unwrap().contains_key(&gid));
        assert!(tm.bt_active.lock().unwrap().contains(&gid));
        assert!(
            matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
            "未达上限时不应发出任何事件"
        );
    }

    /// 无限做种（seed-ratio / seed-time 均未设置）：保持 Seeding（由用户终止）
    #[test]
    fn bt_update_progress_keeps_seeding_without_limits() {
        let (tm, repo, _temp) = test_manager(1);
        let gid = "0123456789abcdef".to_string();
        // 已做种 30 秒（超过任何常见上限时长），但 seed-ratio / seed-time 均未设置
        inject_seeding_task(&tm, &gid, None, None, 30, 1000, 99999);

        tm.bt_update_progress(&gid, &seeding_progress(1000, 99999));

        assert_eq!(
            repo.lock().unwrap().get(&gid).unwrap().status,
            TaskStatus::Seeding,
            "未设置做种上限时应无限做种（由用户暂停 / 移除终止）"
        );
    }

    /// 首次下载完成的 keep-seeding 分支：置 Seeding + 记录做种起始时刻
    #[test]
    fn bt_update_progress_enters_seeding_records_start_time() {
        let (tm, repo, _temp) = test_manager(1);
        let gid = "0123456789abcdef".to_string();
        // Active 下载中任务（keep-seeding 开启、finished 首轮为 true）
        let mut task = Task::new_bt_task(
            &gid,
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            "/tmp/dl",
            Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        );
        task.status = TaskStatus::Active;
        task.total_length = 1000;
        task.completed_length = 1000;
        repo.lock().unwrap().add(task);
        let mut opts = EngineOptions::default();
        opts.keep_seeding = true;
        tm.task_options.lock().unwrap().insert(gid.clone(), opts);
        tm.bt_active.lock().unwrap().insert(gid.clone());

        tm.bt_update_progress(&gid, &seeding_progress(1000, 0));

        // 状态转 Seeding + 做种起始时刻已记录（供后续上限检查）
        assert_eq!(
            repo.lock().unwrap().get(&gid).unwrap().status,
            TaskStatus::Seeding,
            "keep-seeding 开启且下载完成应进入做种"
        );
        assert!(
            tm.bt_seed_started.lock().unwrap().contains_key(&gid),
            "进入做种时应记录做种起始时刻"
        );
        // 做种保留并发槽位（bt_active 不移除）
        assert!(tm.bt_active.lock().unwrap().contains(&gid));
    }
}
