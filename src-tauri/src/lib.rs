//! motrix-tauri：Tauri 2 应用壳（GUI 层）
//!
//! - 初始化数据目录与配置（motrix-core::config）
//! - 注册配置读写 command（get_app_config / save_app_config）
//! - 监听前端 shims 发来的 'command' / 'event' 事件并分发（对齐 Electron 版 IPC）
//! - 向 Webview 注入平台标识 `window.__TAURI_OS_PLATFORM__`

mod commands;
mod rpc_backend;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use motrix_core::{ConfigManager, TaskManager};
use motrix_rpc::{JsonRpcServer, RpcBackend};
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{Emitter, Listener, Manager};
use tracing::{info, warn};

use rpc_backend::CoreRpcBackend;

/// 应用全局状态：持有配置管理器与任务管理器（Arc 以便在多个 command / 事件回调间共享）
///
/// 任务管理器内部持有任务仓库（`task_manager.repo`），广播循环与 JSON-RPC 后端
/// 均经它访问同一份仓库。
pub struct AppState {
    pub config_manager: Arc<Mutex<ConfigManager>>,
    /// 任务管理器（Task 10：任务编排 + 任务仓库，JSON-RPC 后端与 engine:* 广播循环共享）
    pub task_manager: Arc<TaskManager>,
    /// 数据目录（checkpoint.json / download.session 所在目录；启动恢复与退出保存使用）
    pub data_dir: PathBuf,
}

impl AppState {
    /// 初始化全局状态：定位数据目录、加载（或创建）user.json / system.json，
    /// 并以 system 配置初始化任务管理器（全局选项 / 最大并发数）
    pub fn new() -> Self {
        // 数据目录定位失败（极端环境）时回退到当前目录，避免应用无法启动
        let data_dir = motrix_core::config::data_dir().unwrap_or_else(|e| {
            warn!("[Motrix] 无法定位数据目录，回退到当前目录: {e}");
            PathBuf::from(".")
        });
        let config_manager = Arc::new(Mutex::new(ConfigManager::new(data_dir.clone())));
        // 从 system.json 读取引擎初始配置（失败回退默认值）
        let system = match config_manager.lock() {
            Ok(cm) => cm.system_config().clone(),
            Err(e) => {
                warn!("[Motrix] 读取系统配置失败，使用默认配置: {e}");
                motrix_core::config::SystemConfig::defaults(&data_dir)
            }
        };
        let task_manager = Arc::new(TaskManager::new(&system, config_manager.clone()));
        // 注入自引用（Weak）：引擎线程事件回调需要升级为 Arc 再回调任务管理器
        task_manager.set_self(Arc::downgrade(&task_manager));
        // —— 启动恢复（Task 12）：在 JSON-RPC 服务与广播循环启动之前，
        //    把历史 / 未完成任务从 checkpoint（或 Electron 版旧 download.session）
        //    恢复进任务仓库，保证首帧快照即含历史任务 ——
        match task_manager.repo.lock() {
            Ok(mut repo) => motrix_core::session::restore_session(&data_dir, &mut repo),
            Err(e) => warn!("[Motrix] 会话恢复失败（获取任务仓库锁失败）: {e}"),
        }
        Self {
            config_manager,
            task_manager,
            data_dir,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// 'command' 事件载荷（前端经 shims/ipcRenderer.js `emit('command', { command, args })` 发送）
#[derive(Debug, Deserialize)]
struct CommandEvent {
    command: String,
    #[serde(default)]
    args: Vec<Value>,
}

/// 'event' 事件载荷（前端经 shims `emit('event', { eventName, args })` 发送）
#[derive(Debug, Deserialize)]
struct AppEvent {
    event_name: String,
    #[serde(default)]
    args: Vec<Value>,
}

/// 已知命令清单（对齐 Electron 版 Application.handleCommands，防止静默丢消息）
pub const COMMAND_NAMES: &[&str] = &[
    "application:save-preference",
    "application:update-tray",
    "application:relaunch",
    "application:quit",
    "application:show",
    "application:hide",
    "application:reset-session",
    "application:factory-reset",
    "application:check-for-updates",
    "application:change-theme",
    "application:change-locale",
    "application:toggle-dock",
    "application:auto-hide-window",
    "application:change-menu-states",
    "application:open-file",
    "application:clear-recent-tasks",
    "application:setup-protocols-client",
    "application:open-external",
    "application:reveal-in-folder",
    "help:official-website",
    "help:manual",
    "help:release-notes",
    "help:report-problem",
];

/// 已知事件清单（对齐 Electron 版 Application.handleEvents 中前端发送的事件）
pub const EVENT_NAMES: &[&str] = &[
    "speed-change",
    "download-status-change",
    "progress-change",
    "task-download-complete",
];

/// 将 Rust 平台名映射为前端 shims 期望的平台标识（win32 / darwin / linux）
pub(crate) fn tauri_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        _ => "linux",
    }
}

/// 分发 'command' 事件：等价 Electron 版 Application.handleCommands
fn handle_command(state: &AppState, command: &str, args: &[Value]) {
    match command {
        // 保存偏好设置：解析 args[0] 的 { user, system } 并分区写回配置
        "application:save-preference" => {
            let Some(payload) = args.first() else {
                warn!("[Motrix] application:save-preference 缺少 payload 参数");
                return;
            };
            let user = payload.get("user");
            let system = payload.get("system");
            match state.config_manager.lock() {
                Ok(mut config_manager) => {
                    if let Err(e) = config_manager.apply_preference(user, system) {
                        warn!("[Motrix] 保存偏好设置失败: {e}");
                    } else {
                        info!("[Motrix] 已保存偏好设置");
                    }
                }
                Err(e) => warn!("[Motrix] 获取配置锁失败: {e}"),
            }
            // Task 10.2：system 分区变更同步引擎全局选项（等价 changeGlobalOption，
            // 使限速 / 代理 / 并发等对后续任务即时生效；键类型与 change_global_option 一致）
            if let Some(system_patch) = system {
                if system_patch.is_object() {
                    if let Err(e) = state.task_manager.change_global_option(system_patch) {
                        warn!("[Motrix] 同步引擎全局选项失败: {e}");
                    }
                }
            }
        }
        // 其余命令：Phase 4 逐步实现，未实现时记录日志而非静默丢弃
        _ => {
            if COMMAND_NAMES.contains(&command) {
                warn!("[Motrix] 命令 {command} 已列入 COMMAND_NAMES 但尚未实现（将在后续阶段处理）");
            } else {
                warn!("[Motrix] 收到未知命令: {command}");
            }
        }
    }
}

/// Tauri 应用入口
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::get_app_config,
            commands::save_app_config,
            // —— 原生 shell 能力（Task 3）：showItemInFolder / openPath / trashItem ——
            commands::show_item_in_folder,
            commands::open_path,
            commands::trash_item,
            // —— 任务操作占位命令（Phase 1，Task 6）——
            commands::add_uri,
            commands::add_torrent,
            commands::pause_task,
            commands::resume_task,
            commands::remove_task,
            commands::change_option,
            commands::change_global_option,
            commands::get_global_stat,
            commands::get_global_option,
            commands::get_engine_info,
            commands::get_tasks,
            commands::get_task_detail,
            commands::get_peers,
            commands::save_session,
            commands::purge,
        ])
        .setup(|app| {
            // —— 平台标识注入：前端 shims/electron-is.js 读取 window.__TAURI_OS_PLATFORM__ ——
            let platform = tauri_platform();
            let script = format!("window.__TAURI_OS_PLATFORM__ = '{platform}';");
            for (label, webview_window) in app.webview_windows() {
                if let Err(e) = webview_window.eval(&script) {
                    warn!("[Motrix] 向窗口 {label} 注入平台标识失败: {e}");
                }
            }
            info!("[Motrix] 已注入平台标识: {platform}");

            // —— 监听前端 'command' 事件（原 Electron ipcMain.on('command')）——
            let app_handle = app.handle().clone();
            app_handle.clone().listen("command", move |event| {
                let payload = event.payload();
                match serde_json::from_str::<CommandEvent>(payload) {
                    Ok(CommandEvent { command, args }) => {
                        info!("[Motrix] ipc receive command: {command}");
                        let state = app_handle.state::<AppState>();
                        handle_command(&state, &command, &args);
                    }
                    Err(e) => warn!("[Motrix] 解析 command 事件载荷失败: {e}"),
                }
            });

            // —— 监听前端 'event' 事件（原 Electron ipcMain.on('event')）——
            let app_handle = app.handle().clone();
            app_handle.clone().listen("event", move |event| {
                let payload = event.payload();
                match serde_json::from_str::<AppEvent>(payload) {
                    Ok(AppEvent { event_name, args }) => {
                        if EVENT_NAMES.contains(&event_name.as_str()) {
                            // Phase 0 先记录日志；速度/进度等事件在 Phase 4 处理
                            info!("[Motrix] ipc receive event: {event_name}, args: {args:?}");
                        } else {
                            warn!("[Motrix] 收到未知事件: {event_name}");
                        }
                    }
                    Err(e) => warn!("[Motrix] 解析 event 事件载荷失败: {e}"),
                }
            });

            // —— 启动 JSON-RPC 服务（对外接口，127.0.0.1:{rpc-listen-port}/jsonrpc，
            //     WS + HTTP POST 双通道，供 motrix-cli / 外部 aria2 兼容客户端使用）——
            {
                let state = app.state::<AppState>();
                // 从 system.json 读取监听端口与密钥（默认 16800；空密钥表示免认证）
                let (rpc_port, rpc_secret) = match state.config_manager.lock() {
                    Ok(config_manager) => (
                        config_manager.rpc_listen_port(),
                        config_manager.rpc_secret().to_string(),
                    ),
                    Err(e) => {
                        warn!("[Motrix] 读取 RPC 配置失败（获取配置锁失败）: {e}");
                        (16800u16, String::new())
                    }
                };
                // 用共享的任务管理器 / 配置管理器构造 aria2 兼容后端
                let backend: Arc<dyn RpcBackend> = Arc::new(CoreRpcBackend::new(
                    state.task_manager.clone(),
                    state.config_manager.clone(),
                ));
                tauri::async_runtime::spawn(async move {
                    // serve() 为异步且持续服务；端口被占用等启动失败仅记 warn，不阻塞应用
                    if let Err(e) = JsonRpcServer::new(backend, rpc_port, rpc_secret)
                        .serve()
                        .await
                    {
                        warn!("[Motrix] JSON-RPC 服务启动失败（端口 {rpc_port}）: {e}");
                    }
                });
            }

            // —— 任务状态变化事件转发（Task 11）：订阅 TaskManager 的 TaskEvent 通道，
            //    收到后立即 emit `engine:task-event`（{gid, event}）给前端，
            //    对应原 aria2 的 onDownloadStart/Stop/Pause/Complete/Error 通知 ——
            {
                let app_handle = app.handle().clone();
                let state = app.state::<AppState>();
                let mut rx = state.task_manager.subscribe_events();
                tauri::async_runtime::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                let payload = json!({ "gid": ev.gid, "event": ev.event });
                                // 推送失败忽略（前端未订阅 / WebView 未就绪时丢弃）
                                let _ = app_handle.emit("engine:task-event", &payload);
                            }
                            // 订阅较晚落后于发送方（广播容量内的历史被跳过）：继续接收最新事件
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            // 通道关闭（TaskManager 被释放）：退出转发循环
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
            }

            // —— 启动状态广播循环：按 numActive 自适应间隔（0.5s~6s）推送 engine:* 事件 ——
            {
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        // 依据当前运行中任务数计算下一次推送间隔（任务越多推送越密）
                        let interval = {
                            let state = app_handle.state::<AppState>();
                            let num_active = match state.task_manager.repo.lock() {
                                Ok(repo) => repo.global_stat().num_active as u32,
                                Err(e) => {
                                    warn!("[Motrix] 广播循环获取任务仓库锁失败: {e}");
                                    0
                                }
                            };
                            motrix_core::broadcaster::effective_interval(num_active)
                        };
                        tokio::time::sleep(interval).await;

                        // 构建全局统计与任务快照（同一次锁内读取，保证一致性）
                        let (stat, snapshot) = {
                            let state = app_handle.state::<AppState>();
                            let repo = match state.task_manager.repo.lock() {
                                Ok(repo) => repo,
                                Err(e) => {
                                    warn!("[Motrix] 广播循环获取任务仓库锁失败: {e}");
                                    continue;
                                }
                            };
                            (
                                repo.global_stat().to_aria2_json(),
                                motrix_core::broadcaster::build_snapshot(&repo),
                            )
                        };
                        // 推送事件：失败忽略（前端未订阅或 WebView 未就绪时下次重试）
                        let _ = app_handle.emit("engine:global-stat", &stat);
                        let _ = app_handle.emit("engine:snapshot", &snapshot);
                    }
                });
            }

            Ok(())
        })
        // Task 12：由 .run(context) 改为 .build(context) + .run(callback)，
        // 以便在 RunEvent 回调中执行退出保存（等价 Electron 版 will-exit 保存会话）
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // —— 退出保存（Task 12）：等价 aria2.saveSession 语义，
            //    把任务仓库写入 checkpoint.json，下次启动时经 restore_session 恢复
            //    （未完成任务基于已下载字节 Range 续传）——
            if let tauri::RunEvent::ExitRequested { .. } = event {
                let state = app_handle.state::<AppState>();
                let repo = match state.task_manager.repo.lock() {
                    Ok(repo) => repo,
                    Err(e) => {
                        warn!("[Motrix] 退出保存 checkpoint 失败（获取任务仓库锁失败）: {e}");
                        return;
                    }
                };
                if let Err(e) = motrix_core::session::save_checkpoint(&state.data_dir, &repo) {
                    warn!("[Motrix] 退出保存 checkpoint 失败: {e}");
                } else {
                    info!("[Motrix] 退出时已保存会话 checkpoint");
                }
            }
        });
}
