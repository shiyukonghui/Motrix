//! 应用菜单（等价 Electron 版 ui/MenuManager.js，按平台构建）
//!
//! - Windows/Linux：file / task / edit / window / help 分组（参考 src/main/menus/win32.json）
//! - macOS：app / task / edit / window / help 分组（参考 darwin.json）
//! - 菜单项 id 保持 JSON 模板中的原命名（如 app.quit / task.new-task / help.manual）
//! - 菜单点击分发：原生命令 → Rust 直接处理；渲染进程命令 → `command:dispatch` 转发前端
//!   （Tauri 简化版：Electron 版菜单命令发往 renderer 由 Ipc.vue 再回发 main，此处直接分流）
//! - edit.* / window.* 系统角色项使用 PredefinedMenuItem（原生行为 + 自定义 id 触发事件）

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Manager};
use tracing::info;

use crate::AppState;

/// setup 阶段调用：构建并安装应用菜单
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_app_menu(app)?;
    // 设置到 AppHandle：Windows/Linux 挂到主窗口菜单栏，macOS 为全局菜单栏
    app.set_menu(menu)?;
    Ok(())
}

/// Builder.on_menu_event 回调：菜单点击分发入口（应用菜单 + 托盘菜单共用）
pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        // 特殊角色项：非 JSON 模板中的 command 命令，直接处理
        "window.reload" => {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.eval("window.location.reload()");
            }
        }
        // Electron role: zoom（Windows 上等价最大化/还原切换）
        "window.zoom" => {
            if let Some(win) = app.get_webview_window("main") {
                if win.is_maximized().unwrap_or(false) {
                    let _ = win.unmaximize();
                } else {
                    let _ = win.maximize();
                }
            }
        }
        // Electron role: front（仅 macOS 有意义，聚焦全部窗口）
        "window.front" => {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_focus();
            }
        }
        "help.toggle-dev-tools" => {
            if let Some(win) = app.get_webview_window("main") {
                win.open_devtools();
            }
        }
        id => dispatch_menu_command(app, id),
    }
}

/// 分发菜单点击：菜单项 id → command（与 src/main/menus/*.json 的 command 字段对应）
pub fn dispatch_menu_command(app: &AppHandle, id: &str) {
    // 需先显示主窗口的菜单项（对应 JSON 模板中 command-before / command-after 的
    // application:show?page=index 语义：新建任务 / 打开文件 / 任务列表 / 设置 / 关于）
    const SHOW_WINDOW_FIRST: &[&str] = &[
        "task.new-task",
        "task.new-bt-task",
        "task.open-file",
        "app.task-list",
        "app.preferences",
        "app.about",
    ];
    if SHOW_WINDOW_FIRST.contains(&id) {
        crate::show_main_window(app);
    }

    let Some(command) = menu_id_to_command(id) else {
        // edit.* / window.* 系统角色项由 PredefinedMenuItem 原生处理，无需分发
        info!("[Motrix] 菜单项 {id} 无映射命令（由系统原生处理）");
        return;
    };

    // 原生命令与渲染进程命令统一交给 handle_command 分流
    // （渲染进程命令在 handle_command 中经 command:dispatch 转发前端）
    let state = app.state::<AppState>();
    crate::handle_command(app, &state, command, &[]);
}

/// 菜单项 id → 命令映射（对齐 src/main/menus/tray.json / win32.json / darwin.json / linux.json）
fn menu_id_to_command(id: &str) -> Option<&'static str> {
    match id {
        // —— 任务 ——
        "task.new-task" => Some("application:new-task"),
        "task.new-bt-task" => Some("application:new-bt-task"),
        "task.open-file" => Some("application:open-file"),
        "task.pause-task" => Some("application:pause-task"),
        "task.resume-task" => Some("application:resume-task"),
        "task.delete-task" => Some("application:delete-task"),
        "task.move-task-up" => Some("application:move-task-up"),
        "task.move-task-down" => Some("application:move-task-down"),
        "task.pause-all-task" => Some("application:pause-all-task"),
        "task.resume-all-task" => Some("application:resume-all-task"),
        "task.select-all-task" => Some("application:select-all-task"),
        "task.clear-recent-tasks" => Some("application:clear-recent-tasks"),
        // —— 应用 ——
        "app.about" => Some("application:about"),
        "app.preferences" => Some("application:preferences"),
        "app.check-for-updates" => Some("application:check-for-updates"),
        "app.show" => Some("application:show"),
        "app.task-list" => Some("application:task-list"),
        "app.quit" => Some("application:quit"),
        // —— 帮助 ——
        "help.official-website" => Some("help:official-website"),
        "help.manual" => Some("help:manual"),
        "help.release-notes" => Some("help:release-notes"),
        "help.report-problem" => Some("help:report-problem"),
        _ => None,
    }
}

/// 构建应用菜单（按平台选择文件 / 应用分组，其余分组三平台一致）
fn build_app_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    #[cfg(target_os = "macos")]
    let first_menu = build_app_menu_macos(app)?;
    #[cfg(not(target_os = "macos"))]
    let first_menu = build_file_menu(app)?;

    let task_menu = build_task_menu(app)?;
    let edit_menu = build_edit_menu(app)?;
    let window_menu = build_window_menu(app)?;
    let help_menu = build_help_menu(app)?;

    Menu::with_items(
        app,
        &[&first_menu, &task_menu, &edit_menu, &window_menu, &help_menu],
    )
}

/// macOS 应用菜单（对应 darwin.json 的 menu.app 分组）
#[cfg(target_os = "macos")]
fn build_app_menu_macos(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let about = MenuItem::with_id(app, "app.about", "About Motrix", true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let preferences = MenuItem::with_id(app, "app.preferences", "Preferences…", true, None::<&str>)?;
    let check_updates =
        MenuItem::with_id(app, "app.check-for-updates", "Check for Updates...", true, None::<&str>)?;
    let hide = PredefinedMenuItem::hide(app, Some("app.hide"))?;
    let hide_others = PredefinedMenuItem::hide_others(app, Some("app.hide-others"))?;
    let show_all = PredefinedMenuItem::show_all(app, Some("app.unhide"))?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let quit = PredefinedMenuItem::quit(app, Some("app.quit"))?;
    Submenu::with_items(
        app,
        "Motrix",
        true,
        &[
            &about,
            &separator1,
            &preferences,
            &check_updates,
            &hide,
            &hide_others,
            &show_all,
            &separator2,
            &quit,
        ],
    )
}

/// 文件菜单（Windows / Linux，对应 win32.json / linux.json 的 menu.file 分组）
#[cfg(not(target_os = "macos"))]
fn build_file_menu(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let about = MenuItem::with_id(app, "app.about", "About Motrix", true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let preferences = MenuItem::with_id(app, "app.preferences", "Preferences", true, None::<&str>)?;
    let check_updates =
        MenuItem::with_id(app, "app.check-for-updates", "Check for Updates...", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "app.show", "Show Motrix", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let quit = PredefinedMenuItem::quit(app, Some("app.quit"))?;
    Submenu::with_items(
        app,
        "File",
        true,
        &[
            &about,
            &separator1,
            &preferences,
            &check_updates,
            &show,
            &separator2,
            &quit,
        ],
    )
}

/// 任务菜单（win32.json / darwin.json / linux.json 的 menu.task 分组一致）
fn build_task_menu(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let new_task = MenuItem::with_id(app, "task.new-task", "New Task", true, None::<&str>)?;
    let new_bt_task =
        MenuItem::with_id(app, "task.new-bt-task", "New BT Task", true, None::<&str>)?;
    let open_file =
        MenuItem::with_id(app, "task.open-file", "Open Torrent File...", true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let task_list = MenuItem::with_id(app, "app.task-list", "Task List", true, None::<&str>)?;
    let pause_task = MenuItem::with_id(app, "task.pause-task", "Pause Task", true, None::<&str>)?;
    let resume_task =
        MenuItem::with_id(app, "task.resume-task", "Resume Task", true, None::<&str>)?;
    let delete_task =
        MenuItem::with_id(app, "task.delete-task", "Delete Task", true, None::<&str>)?;
    let move_task_up =
        MenuItem::with_id(app, "task.move-task-up", "Move Task Up", true, None::<&str>)?;
    let move_task_down =
        MenuItem::with_id(app, "task.move-task-down", "Move Task Down", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let pause_all =
        MenuItem::with_id(app, "task.pause-all-task", "Pause All Tasks", true, None::<&str>)?;
    let resume_all =
        MenuItem::with_id(app, "task.resume-all-task", "Resume All Tasks", true, None::<&str>)?;
    let select_all =
        MenuItem::with_id(app, "task.select-all-task", "Select All Tasks", true, None::<&str>)?;
    let separator3 = PredefinedMenuItem::separator(app)?;
    // 注：linux.json 无 clear-recent-tasks 项；Windows 保留（win32.json 含该项）
    let clear_recent =
        MenuItem::with_id(app, "task.clear-recent-tasks", "Clear Recent Tasks", true, None::<&str>)?;
    Submenu::with_items(
        app,
        "Task",
        true,
        &[
            &new_task,
            &new_bt_task,
            &open_file,
            &separator1,
            &task_list,
            &pause_task,
            &resume_task,
            &delete_task,
            &move_task_up,
            &move_task_down,
            &separator2,
            &pause_all,
            &resume_all,
            &select_all,
            &separator3,
            &clear_recent,
        ],
    )
}

/// 编辑菜单（win32.json / darwin.json / linux.json 的 menu.edit 分组一致）
fn build_edit_menu(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let undo = PredefinedMenuItem::undo(app, Some("edit.undo"))?;
    let redo = PredefinedMenuItem::redo(app, Some("edit.redo"))?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let cut = PredefinedMenuItem::cut(app, Some("edit.cut"))?;
    let copy = PredefinedMenuItem::copy(app, Some("edit.copy"))?;
    let paste = PredefinedMenuItem::paste(app, Some("edit.paste"))?;
    // 注：tauri 无 "delete" 预定义项（Electron role: delete），用普通项占位，
    // 删除文本由 WebView 内前端快捷键处理（点击事件无原生行为）
    let delete = MenuItem::with_id(app, "edit.delete", "Delete", true, None::<&str>)?;
    let select_all = PredefinedMenuItem::select_all(app, Some("edit.select-all"))?;
    Submenu::with_items(
        app,
        "Edit",
        true,
        &[&undo, &redo, &separator1, &cut, &copy, &paste, &delete, &select_all],
    )
}

/// 窗口菜单（对应 win32.json / darwin.json / linux.json 的 menu.window 分组）
fn build_window_menu(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    // reload 无 tauri 预定义项，用普通项 + on_menu_event 特殊处理（location.reload）
    let reload = MenuItem::with_id(app, "window.reload", "Reload", true, None::<&str>)?;
    let close = PredefinedMenuItem::close_window(app, Some("window.close"))?;
    let minimize = PredefinedMenuItem::minimize(app, Some("window.minimize"))?;
    // zoom 无 tauri 预定义项（Electron role: zoom），用普通项 + on_menu_event 处理（最大化切换）
    let zoom = MenuItem::with_id(app, "window.zoom", "Zoom", true, None::<&str>)?;
    let toggle_fullscreen = PredefinedMenuItem::fullscreen(app, Some("window.toggle-fullscreen"))?;
    let separator = PredefinedMenuItem::separator(app)?;
    // window.front 仅 macOS 有意义（对应 role: front），Windows/Linux 忽略
    let front = MenuItem::with_id(app, "window.front", "Bring All to Front", true, None::<&str>)?;
    Submenu::with_items(
        app,
        "Window",
        true,
        &[&reload, &close, &minimize, &zoom, &toggle_fullscreen, &separator, &front],
    )
}

/// 帮助菜单（win32.json / darwin.json / linux.json 的 menu.help 分组一致）
fn build_help_menu(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let official_website =
        MenuItem::with_id(app, "help.official-website", "Official Website", true, None::<&str>)?;
    let manual = MenuItem::with_id(app, "help.manual", "Manual", true, None::<&str>)?;
    let release_notes =
        MenuItem::with_id(app, "help.release-notes", "Release Notes", true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let report_problem =
        MenuItem::with_id(app, "help.report-problem", "Report a Problem", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let toggle_dev_tools =
        MenuItem::with_id(app, "help.toggle-dev-tools", "Toggle Developer Tools", true, None::<&str>)?;
    Submenu::with_items(
        app,
        "Help",
        true,
        &[
            &official_website,
            &manual,
            &release_notes,
            &separator1,
            &report_problem,
            &separator2,
            &toggle_dev_tools,
        ],
    )
}
