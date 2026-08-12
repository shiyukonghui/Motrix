//! motrix-tauri：Tauri 2 应用壳（GUI 层）
//!
//! - 初始化数据目录与配置（motrix-core::config）
//! - 注册配置读写 command（get_app_config / save_app_config）
//! - 监听前端 shims 发来的 'command' / 'event' 事件并分发（对齐 Electron 版 IPC）
//! - 向 Webview 注入平台标识 `window.__TAURI_OS_PLATFORM__`
//! - Phase 4：托盘 / 菜单 / 单实例 / 开机自启 / 深链 / 系统通知 / 窗口状态 /
//!   UPnP / 自动更新 / 防休眠（等价 Electron 版 src/main 下的桌面集成胶水）

mod app;
mod commands;
mod rpc_backend;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use motrix_core::{ConfigManager, TaskManager};
use motrix_rpc::{JsonRpcServer, RpcBackend};
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Listener, Manager};
use tauri::window::{ProgressBarState, ProgressBarStatus};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tracing::{info, warn};

use app::window as window_manager;
use app::{autostart, energy, menu, protocol, tray, updater, upnp};
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
    /// 托盘图标最近更新时间（application:update-tray 节流 ≤1s 用，防托盘卡顿）
    pub tray_last_update: Mutex<Option<Instant>>,
    /// 自动隐藏窗口开关（application:auto-hide-window 写入；窗口失焦时据此隐藏窗口）
    pub auto_hide_window: AtomicBool,
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
            tray_last_update: Mutex::new(None),
            auto_hide_window: AtomicBool::new(false),
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

/// 已知命令清单（对齐 Electron 版 Application.handleCommands + 菜单 JSON 中的命令，
/// 防止静默丢消息；缺失命令直接报"未知命令"）
pub const COMMAND_NAMES: &[&str] = &[
    // —— 原生命令（Rust 直接处理）——
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
    // —— 渲染进程命令（菜单 / 托盘 / 深链 → command:dispatch 转发前端）——
    // 注：以下命令出现在 tray.json / win32.json 等菜单定义中（Electron 版由
    // 主进程 sendCommandToAll 发往 renderer，Ipc.vue → commands.js 分发），
    // 补齐进清单避免"未知命令"日志
    "application:new-task",
    "application:new-bt-task",
    "application:new-bt-task-with-file",
    "application:task-list",
    "application:preferences",
    "application:about",
    "application:pause-task",
    "application:resume-task",
    "application:delete-task",
    "application:move-task-up",
    "application:move-task-down",
    "application:pause-all-task",
    "application:resume-all-task",
    "application:select-all-task",
    "application:show-task-detail",
    "application:update-preference-config",
    "application:update-system-theme",
    "application:update-theme",
    "application:update-locale",
    "application:update-tray-focused",
];

/// 渲染进程命令：收到后经 `command:dispatch` 转发前端（commands.js 已注册对应处理器）
const RENDERER_COMMANDS: &[&str] = &[
    "application:new-task",
    "application:new-bt-task",
    "application:new-bt-task-with-file",
    "application:task-list",
    "application:preferences",
    "application:about",
    "application:pause-task",
    "application:resume-task",
    "application:delete-task",
    "application:move-task-up",
    "application:move-task-down",
    "application:pause-all-task",
    "application:resume-all-task",
    "application:select-all-task",
    "application:show-task-detail",
    "application:open-file",
    "application:update-preference-config",
    "application:update-system-theme",
    "application:update-theme",
    "application:update-locale",
    "application:update-tray-focused",
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

/// 配置联动所需的用户配置子集（save-preference 前后对比用，对齐 watchXxxChange 系列）
#[derive(Default, Clone)]
struct LinkedUserConfig {
    open_at_login: bool,
    run_mode: u32,
    locale: String,
    theme: String,
    show_progress_bar: bool,
    auto_sync_tracker: bool,
    enable_upnp: bool,
}

/// 读取当前用户配置中参与联动的键（供保存前后对比）
fn read_linked_user_config(state: &AppState) -> Option<LinkedUserConfig> {
    state.config_manager.lock().ok().map(|cm| {
        let u = cm.user_config();
        LinkedUserConfig {
            open_at_login: u.open_at_login,
            run_mode: u.run_mode,
            locale: u.locale.clone(),
            theme: u.theme.clone(),
            show_progress_bar: u.show_progress_bar,
            auto_sync_tracker: u.auto_sync_tracker,
            enable_upnp: u.enable_upnp,
        }
    })
}

/// 配置变更联动：对比 save-preference 前后的值，触发对应平台行为
/// （等价 Electron 版 watchOpenAtLoginChange / watchRunModeChange / watchLocaleChange /
///  watchThemeChange / watchShowProgressBarChange / watchUPnPEnabledChange 等）
fn apply_config_linkage(
    app: &AppHandle,
    state: &AppState,
    old: LinkedUserConfig,
    new: LinkedUserConfig,
) {
    // open-at-login → 开机自启即时生效
    if old.open_at_login != new.open_at_login {
        autostart::apply(app, new.open_at_login);
    }
    // run-mode → 托盘模式启动隐藏窗口（等价 watchRunModeChange；隐藏动作在下次启动的
    // window_manager::setup 中执行，Tauri 无 Dock 概念，当前进程不做额外处理）
    if old.run_mode != new.run_mode {
        info!(
            "[Motrix] run-mode 变化: {} -> {}（TRAY 模式下窗口将在下次启动时隐藏）",
            old.run_mode, new.run_mode
        );
    }
    // locale → 转发前端应用（等价 watchLocaleChange → sendCommandToAll update-locale）
    if old.locale != new.locale {
        emit_command_dispatch(app, "application:update-locale", &[json!({ "locale": new.locale })]);
    }
    // theme → 转发前端应用（等价 watchThemeChange → sendCommandToAll update-theme）
    if old.theme != new.theme {
        emit_command_dispatch(app, "application:update-theme", &[json!({ "theme": new.theme })]);
    }
    // show-progress-bar → 关闭时清除任务栏进度条（等价 watchShowProgressBarChange → unbind）
    if old.show_progress_bar != new.show_progress_bar && !new.show_progress_bar {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.set_progress_bar(ProgressBarState {
                status: Some(ProgressBarStatus::None),
                progress: None,
            });
        }
    }
    // auto-sync-tracker → tracker 自动同步（Task 9/P1 占位：完整抓取-合并逻辑后续接入，
    // 本阶段仅记录变更；对应 Electron autoSyncTrackers / syncTrackers）
    if old.auto_sync_tracker != new.auto_sync_tracker {
        info!(
            "[Motrix] auto-sync-tracker 变化: {} -> {}（tracker 自动同步将在后续阶段接入）",
            old.auto_sync_tracker, new.auto_sync_tracker
        );
    }
    // enable-upnp → 即时建立 / 移除端口映射（等价 watchUPnPEnabledChange）
    if old.enable_upnp != new.enable_upnp {
        if new.enable_upnp {
            upnp::start_mapping(state);
        } else {
            upnp::stop_mapping(state);
        }
    }
}

/// 向主窗口转发命令（原 Electron sendCommandToAll → webContents.send('command')，
/// Tauri 简化：emit `command:dispatch`，前端 shims/ipcRenderer.js on('command') 监听分发）
pub(crate) fn emit_command_dispatch(app: &AppHandle, command: &str, args: &[Value]) {
    let payload = json!({ "command": command, "args": args });
    if let Err(e) = app.emit("command:dispatch", &payload) {
        warn!("[Motrix] 转发命令 {command} 到前端失败: {e}");
    }
}

/// 显示并聚焦主窗口（等价 Electron application.show）
pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 隐藏主窗口（等价 Electron application.hide）
pub(crate) fn hide_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
}

/// 用系统默认浏览器打开外部链接（等价 Electron shell.openExternal，tauri-plugin-opener）
fn open_external(app: &AppHandle, url: &str) {
    if url.is_empty() {
        return;
    }
    if let Err(e) = app.opener().open_url(url, None::<&str>) {
        warn!("[Motrix] 打开外部链接失败: {url}: {e}");
    }
}

/// 分发 'command' 事件：等价 Electron 版 Application.handleCommands
///
/// - 原生命令（quit/show/hide/update-tray 等）由 Rust 直接处理
/// - 渲染进程命令（new-task/preferences/task-list 等）经 command:dispatch 转发前端
fn handle_command(app: &AppHandle, state: &AppState, command: &str, args: &[Value]) {
    match command {
        // 保存偏好设置：解析 args[0] 的 { user, system } 并分区写回配置，随后做配置联动
        "application:save-preference" => {
            // 保存前快照旧值（供保存后对比触发联动）
            let old = read_linked_user_config(state);
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
            // —— 配置联动：对比变更前后，触发平台行为（Task 7.3）——
            if let (Some(old), Some(new)) = (old, read_linked_user_config(state)) {
                apply_config_linkage(app, state, old, new);
            }
        }
        // 动态托盘速度计：前端 Canvas 绘制 → 更新托盘图标（节流 ≤1s）
        "application:update-tray" => {
            tray::update_tray_image(app, args);
        }
        // 重启应用（等价 Electron relaunch：stopAllSettled + app.relaunch + app.exit）
        "application:relaunch" => {
            info!("[Motrix] 应用重启中...");
            app.restart();
        }
        // 退出应用
        "application:quit" => {
            info!("[Motrix] 应用退出");
            app.exit(0);
        }
        // 显示主窗口（可选带 ?page= 参数；单窗口应用仅记录 page）
        "application:show" => {
            if let Some(page) = args
                .first()
                .and_then(|v| v.get("page"))
                .and_then(|v| v.as_str())
            {
                info!("[Motrix] application:show page={page}");
            }
            show_main_window(app);
        }
        // 隐藏主窗口
        "application:hide" => {
            hide_main_window(app);
        }
        // 重置下载会话：清空任务仓库 + 写空 checkpoint（等价 Electron resetSession）
        "application:reset-session" => {
            let gids: Vec<String> = {
                let repo = match state.task_manager.repo.lock() {
                    Ok(repo) => repo,
                    Err(e) => {
                        warn!("[Motrix] 重置会话失败（获取任务仓库锁失败）: {e}");
                        return;
                    }
                };
                repo.all().iter().map(|t| t.gid.clone()).collect()
            };
            // 逐条移除仓库中的任务（TaskRepository 无整表清空接口）
            if let Ok(mut repo) = state.task_manager.repo.lock() {
                for gid in &gids {
                    let _ = repo.remove(gid);
                }
            }
            // 写入空 checkpoint，下次启动即为全新会话
            if let Ok(repo) = state.task_manager.repo.lock() {
                if let Err(e) = motrix_core::session::save_checkpoint(&state.data_dir, &repo) {
                    warn!("[Motrix] 重置会话后写入 checkpoint 失败: {e}");
                }
            }
            info!("[Motrix] 会话已重置（任务仓库清空 + 空 checkpoint 写入）");
        }
        // 恢复出厂设置：删除 user.json / system.json 后重启重建默认配置
        // （等价 Electron factoryReset：offConfigListeners + configManager.reset + relaunch）
        "application:factory-reset" => {
            info!("[Motrix] 执行恢复出厂设置：删除配置文件并重启");
            let _ = std::fs::remove_file(state.data_dir.join("user.json"));
            let _ = std::fs::remove_file(state.data_dir.join("system.json"));
            app.restart();
        }
        // 检查更新：发布流程未就绪，返回"不可用"结果（app/updater.rs 说明取舍）
        "application:check-for-updates" => {
            let result = updater::check_for_updates(app);
            info!("[Motrix] 检查更新结果: {result}");
        }
        // 切换主题：转发前端应用（等价 Electron change-theme → sendCommandToAll update-theme）
        "application:change-theme" => {
            if let Some(theme) = args.first().and_then(|v| v.as_str()) {
                emit_command_dispatch(app, "application:update-theme", &[json!({ "theme": theme })]);
            }
        }
        // 切换语言：转发前端应用（等价 Electron change-locale）
        "application:change-locale" => {
            if let Some(locale) = args.first().and_then(|v| v.as_str()) {
                emit_command_dispatch(
                    app,
                    "application:update-locale",
                    &[json!({ "locale": locale })],
                );
            }
        }
        // 切换 Dock 显示（macOS 专属；Windows/Linux 无 Dock，仅记录）
        "application:toggle-dock" => {
            info!("[Motrix] application:toggle-dock（仅 macOS 有效，当前平台忽略）");
        }
        // 窗口失焦自动隐藏开关（等价 Electron auto-hide-window）
        "application:auto-hide-window" => {
            let hide = args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            state.auto_hide_window.store(hide, Ordering::SeqCst);
            info!(
                "[Motrix] auto-hide-window 已{}",
                if hide { "开启" } else { "关闭" }
            );
        }
        // 更新菜单 / 托盘菜单状态（等价 Electron change-menu-states；
        // Tauri 菜单项状态动态更新未完整实现，本阶段仅记录）
        "application:change-menu-states" => {
            info!("[Motrix] application:change-menu-states（菜单状态动态更新后续阶段接入）");
        }
        // 打开文件选择对话框（等价 Electron open-file）；本阶段转发前端处理
        "application:open-file" => {
            emit_command_dispatch(app, "application:open-file", args);
        }
        // 清除最近文档（等价 Electron clearRecentDocuments；最近文档能力降级，见 on_task_complete）
        "application:clear-recent-tasks" => {
            info!("[Motrix] application:clear-recent-tasks（最近文档能力未接入，忽略）");
        }
        // 注册深链协议客户端（dev 模式跳过，打包产物注册；等价 Electron setup-protocols-client）
        "application:setup-protocols-client" => {
            protocol::setup_protocols_client(app, args.first());
        }
        // 打开外部链接（args[0] = url）
        "application:open-external" => {
            let Some(url) = args.first().and_then(|v| v.as_str()) else {
                warn!("[Motrix] application:open-external 缺少 url 参数");
                return;
            };
            open_external(app, url);
        }
        // 在文件管理器中定位文件 + 展示任务详情（等价 Electron reveal-in-folder）
        "application:reveal-in-folder" => {
            let payload = args.first();
            if let Some(path) = payload
                .and_then(|p| p.get("path"))
                .and_then(|v| v.as_str())
            {
                info!("[Motrix] application:reveal-in-folder===> {path}");
                // 复用 commands.rs 的原生定位能力（explorer /select, 等平台命令）
                let _ = crate::commands::show_item_in_folder(path.to_string());
            }
            if let Some(gid) = payload
                .and_then(|p| p.get("gid"))
                .and_then(|v| v.as_str())
            {
                emit_command_dispatch(
                    app,
                    "application:show-task-detail",
                    &[json!({ "gid": gid })],
                );
            }
        }
        // —— 帮助类命令：打开对应官网链接（等价 Electron handleCommands 中 help:* 系列）——
        "help:official-website" => open_external(app, "https://motrix.app/"),
        "help:manual" => open_external(app, "https://motrix.app/manual"),
        "help:release-notes" => open_external(app, "https://motrix.app/release"),
        "help:report-problem" => open_external(app, "https://motrix.app/report"),
        // 渲染进程命令：转发 command:dispatch 由前端分发（对应 commands.js 注册的命令）
        _ => {
            if RENDERER_COMMANDS.contains(&command) {
                emit_command_dispatch(app, command, args);
            } else if COMMAND_NAMES.contains(&command) {
                warn!("[Motrix] 命令 {command} 已列入 COMMAND_NAMES 但尚未实现（将在后续阶段处理）");
            } else {
                warn!("[Motrix] 收到未知命令: {command}");
            }
        }
    }
}

/// 任务栏进度条（等价 Electron bindProgressChange → setProgressBar）
///
/// Electron 语义：0-1（2=macOS 不确定模式）；Tauri `set_progress_bar` 取 0-100，
/// 故乘以 100 并对齐边界；`show-progress-bar` 配置关闭时不显示。
fn apply_progress_bar(app: &AppHandle, progress: f64) {
    let enabled = match app.state::<AppState>().config_manager.lock() {
        Ok(cm) => cm.user_config().show_progress_bar,
        Err(e) => {
            warn!("[Motrix] 读取 show-progress-bar 配置失败: {e}");
            return;
        }
    };
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    if !enabled {
        let _ = win.set_progress_bar(ProgressBarState {
            status: Some(ProgressBarStatus::None),
            progress: None,
        });
        return;
    }
    let p = progress.clamp(0.0, 1.0);
    if p <= 0.0 {
        // 无进度时清除任务栏进度（Electron 为 0 时显示空进度条，此处简化为清除）
        let _ = win.set_progress_bar(ProgressBarState {
            status: Some(ProgressBarStatus::None),
            progress: None,
        });
    } else {
        // Tauri 的进度状态为 Normal(0-100)，对应 Electron 的 0-1 值乘以 100
        let _ = win.set_progress_bar(ProgressBarState {
            status: Some(ProgressBarStatus::Normal),
            progress: Some((p * 100.0) as u64),
        });
    }
}

/// 任务完成处理：系统通知（task-notification 配置，默认开启）+ 最近文档
fn on_task_complete(app: &AppHandle, path: Option<&str>) {
    // 系统通知：tauri-plugin-notification（Windows toast / macOS / Linux 通知）
    let enabled = match app.state::<AppState>().config_manager.lock() {
        Ok(cm) => cm.user_config().task_notification,
        Err(e) => {
            warn!("[Motrix] 读取 task-notification 配置失败（获取配置锁失败）: {e}");
            true
        }
    };
    if enabled {
        let body = path
            .map(|p| format!("下载完成: {p}"))
            .unwrap_or_else(|| "下载任务已完成".to_string());
        // 失败仅记 warn（Linux 无通知服务 / Windows 通知被系统限制等场景不阻塞应用）
        if let Err(e) = app.notification().builder().title("Motrix").body(body).show() {
            warn!("[Motrix] 发送任务完成通知失败: {e}");
        }
    }

    // 最近文档：Windows SHAddToRecentDocs 需引入 windows crate（依赖较重），
    // 且暂无轻量跨平台方案，本阶段记录日志作为接入点（后续可接 open-recent 类方案）
    match path {
        Some(p) => info!("[Motrix] 任务完成（最近文档接入点，跳过）: {p}"),
        None => info!("[Motrix] 任务完成（最近文档接入点，跳过）"),
    }
}

/// 分发 'event' 事件：等价 Electron 版 Application.handleEvents
fn handle_event(app: &AppHandle, event_name: &str, args: &[Value]) {
    match event_name {
        // 速度变化：托盘速度计数据已由前端 Canvas 直接驱动（application:update-tray），
        // 此处仅记录（等价 Electron speed-change 触发 dock/tray 刷新的入口）
        "speed-change" => {
            info!("[Motrix] 收到 speed-change 事件（速度计由前端直接驱动托盘图标）");
        }
        // 下载状态变化：驱动防休眠（等价 Electron download-status-change → EnergyManager）
        "download-status-change" => {
            let downloading = args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            energy::handle_download_status_change(downloading);
        }
        // 进度变化：窗口任务栏进度条（等价 Electron bindProgressChange → setProgressBar）
        "progress-change" => {
            let progress = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            apply_progress_bar(app, progress);
        }
        // 任务完成：最近文档（系统通知走 TaskEvent 'complete' 订阅，避免重复）
        "task-download-complete" => {
            let path = args.get(1).and_then(|v| v.as_str()).map(str::to_string);
            on_task_complete(app, path.as_deref());
        }
        _ => {
            if EVENT_NAMES.contains(&event_name) {
                info!("[Motrix] 事件 {event_name} 已列入 EVENT_NAMES 但未单独处理");
            } else {
                warn!("[Motrix] 收到未知事件: {event_name}");
            }
        }
    }
}

/// Tauri 应用入口
pub fn run() {
    tauri::Builder::default()
        // —— 单实例插件：二次启动时聚焦已有窗口并处理深链参数 ——
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 聚焦主窗口（等价 Electron 单实例锁 + 二次启动 show）
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
            // 深链 URL 由系统作为命令行参数传给二次启动进程，转发给协议处理
            protocol::handle_args(app, &argv);
        }))
        // —— 开机自启插件（Windows 注册表 / macOS LaunchAgent / Linux autostart）——
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // 与 Electron 版 LOGIN_SETTING_OPTIONS.args 一致，标记本次为开机启动
            Some(vec!["--opened-at-login=1"]),
        ))
        // —— 系统通知插件（任务完成通知）——
        .plugin(tauri_plugin_notification::init())
        // —— 打开外部链接插件（help:* / application:open-external）——
        .plugin(tauri_plugin_opener::init())
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
                        handle_command(&app_handle, state.inner(), &command, &args);
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
                            handle_event(&app_handle, &event_name, &args);
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
            //    对应原 aria2 的 onDownloadStart/Stop/Pause/Complete/Error 通知；
            //    并在此订阅任务完成（complete）发送系统通知（Task 8.4）——
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
                                // 任务完成 → 系统通知（读 user.json 的 task-notification 配置；
                                // 注：前端 EngineClient 另有 HTML5 通知，后续可统一收敛到此处）
                                if payload["event"] == "complete" {
                                    on_task_complete(&app_handle, None);
                                }
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

            // ==================== Phase 4：平台能力初始化 ====================

            // 窗口状态恢复 + run-mode=TRAY / auto-hide-window 启动隐藏
            window_manager::setup(app.handle());

            // 应用菜单（按平台构建：win32 用 file/task/edit/window/help 分组）
            if let Err(e) = menu::setup(app.handle()) {
                warn!("[Motrix] 安装应用菜单失败: {e}");
            }

            // 系统托盘（含动态速度计）
            tray::setup(app.handle());

            // 开机自启：按 user.json 的 open-at-login 配置应用
            autostart::apply_configured(app.handle());

            // 深链协议：处理启动参数中的 URL + 打包产物注册协议（dev 模式跳过）
            {
                let args: Vec<String> = std::env::args().skip(1).collect();
                protocol::handle_args(app.handle(), &args);
                // 按 user.json 的 protocols 配置注册协议客户端（等价 initProtocolManager + setup）
                let protocols = match app.state::<AppState>().config_manager.lock() {
                    Ok(cm) => serde_json::to_value(cm.user_config().protocols.clone()).ok(),
                    Err(e) => {
                        warn!("[Motrix] 读取 protocols 配置失败: {e}");
                        None
                    }
                };
                protocol::setup_protocols_client(app.handle(), protocols.as_ref());
            }

            // UPnP 端口映射（enable-upnp 开启时异步执行，失败仅记 warn）
            upnp::maybe_setup_upnp(app.handle());

            Ok(())
        })
        // —— 窗口事件：移动/缩放保存状态、失焦自动隐藏（window.rs）——
        .on_window_event(|window, event| {
            window_manager::on_window_event(window, event);
        })
        // —— 菜单点击分发：应用菜单（menu.rs；托盘菜单在 TrayIconBuilder 内分发）——
        .on_menu_event(|app, event| {
            menu::on_menu_event(app, event);
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
