//! 系统托盘 + 动态速度计（等价 Electron 版 ui/TrayManager.js）
//!
//! - 托盘图标使用 src-tauri/icons 下的应用图标（icon.png/icon.ico，经 default_window_icon 加载）
//! - 菜单项按 src/main/menus/tray.json 构建（id 保持原命名），点击经 menu.rs 分发
//! - 左键单击切换主窗口显隐；鼠标按下/弹起向前端发 application:update-tray-focused
//!   （前端 DynamicTray.vue 据此切换托盘图标主题）
//! - 动态速度计：前端 Canvas 绘制 → `application:update-tray`（{width,height,data}）→
//!   Rust 端解码（PNG 或原始 RGBA）→ `tray.set_icon`，节流 ≤1s 防托盘卡顿

use std::time::Instant;

use serde_json::Value;
use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use crate::AppState;

/// 托盘图标 id（后续经 app.tray_by_id 获取以更新图标）
const TRAY_ID: &str = "main-tray";
/// 托盘图标更新节流间隔：前端 Canvas 以秒级频率重绘（速度变化驱动），
/// 过度频繁 set_icon 会导致 Windows 托盘卡顿，这里限制 ≤1s 一次
const TRAY_UPDATE_THROTTLE: std::time::Duration = std::time::Duration::from_secs(1);
/// 速度计画布目标尺寸（前端 TRAY_CANVAS_CONFIG 66×16 × scale 2 = 132×32，
/// 见 src/shared/constants.js；原始 RGBA 载荷的回退尺寸）
const TRAY_WIDTH: u32 = 132;
const TRAY_HEIGHT: u32 = 32;

/// setup 阶段调用：创建托盘图标 + 菜单 + 事件绑定
pub fn setup(app: &AppHandle) {
    // 托盘菜单（对齐 src/main/menus/tray.json，id 保持原命名）
    let menu = match build_tray_menu(app) {
        Ok(menu) => menu,
        Err(e) => {
            warn!("[Motrix] 构建托盘菜单失败: {e}");
            return;
        }
    };

    // 托盘图标：优先使用应用默认图标（tauri.conf.json 的 icons/icon.png），
    // 极端情况下退化为 1×1 透明占位图避免 panic
    let icon = app.default_window_icon().cloned().unwrap_or_else(|| {
        Image::new_owned(vec![0u8; 4], 1, 1)
    });

    // show_menu_on_left_click(false)：Windows/macOS 下左键走 Click 事件（切换窗口），
    // 右键弹出上下文菜单（与 Electron TrayManager 的 click/right-click 行为一致）。
    // 注：托盘菜单点击不在此注册 on_menu_event——Builder 的全局 on_menu_event 会
    // 同时捕获应用菜单与托盘菜单（按菜单项 id 分发，避免双重分发）
    let tray_result = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip("Motrix")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state,
                ..
            } = event
            {
                match button_state {
                    // 鼠标弹起：切换主窗口显隐（等价 Electron handleTrayClick → toggle）
                    MouseButtonState::Up => {
                        toggle_main_window(tray.app_handle());
                        // 恢复非聚焦托盘主题（等价 Electron mouse-up 事件）
                        let _ = crate::emit_command_dispatch(
                            tray.app_handle(),
                            "application:update-tray-focused",
                            &[Value::from(serde_json::json!({ "focused": false }))],
                        );
                    }
                    // 鼠标按下：托盘图标切换为反色主题（等价 Electron mouse-down 事件）
                    MouseButtonState::Down => {
                        let _ = crate::emit_command_dispatch(
                            tray.app_handle(),
                            "application:update-tray-focused",
                            &[Value::from(serde_json::json!({ "focused": true }))],
                        );
                    }
                }
            }
        })
        .build(app);

    match tray_result {
        Ok(_tray) => info!("[Motrix] 系统托盘已创建"),
        Err(e) => warn!("[Motrix] 创建系统托盘失败: {e}"),
    }
}

/// 构建托盘菜单（结构对齐 src/main/menus/tray.json）
///
/// 菜单文本暂用英文硬编码（菜单 id 保持原命名；i18n 本地化后续可按 locale 重建，
/// 等价 Electron 版 handleLocaleChange → setupMenu）
fn build_tray_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};

    let new_task = MenuItem::with_id(app, "task.new-task", "New Task", true, None::<&str>)?;
    let new_bt_task =
        MenuItem::with_id(app, "task.new-bt-task", "New BT Task", true, None::<&str>)?;
    let open_file =
        MenuItem::with_id(app, "task.open-file", "Open Torrent File...", true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let show = MenuItem::with_id(app, "app.show", "Show Motrix", true, None::<&str>)?;
    let manual = MenuItem::with_id(app, "help.manual", "Manual", true, None::<&str>)?;
    let check_updates =
        MenuItem::with_id(app, "app.check-for-updates", "Check for Updates...", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let task_list = MenuItem::with_id(app, "app.task-list", "Task List", true, None::<&str>)?;
    let preferences = MenuItem::with_id(app, "app.preferences", "Preferences", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "app.quit", "Quit", true, None::<&str>)?;

    Menu::with_items(
        app,
        &[
            &new_task,
            &new_bt_task,
            &open_file,
            &separator1,
            &show,
            &manual,
            &check_updates,
            &separator2,
            &task_list,
            &preferences,
            &quit,
        ],
    )
}

/// 更新托盘图标（application:update-tray 命令入口）
///
/// 载荷为 args[0]：`{ width, height, data: [u8...] }`（前端已把 ArrayBuffer 转字节数组，
/// 尺寸 132×32 = TRAY_CANVAS_CONFIG 66×16 × scale 2），或回退为纯字节数组。
/// 字节为 PNG 编码（前端 convertToBlob 输出）时自动解码为 RGBA，否则按原始 RGBA 处理。
/// 节流 ≤1s：间隔内丢弃（防托盘卡顿）。
pub fn update_tray_image(app: &AppHandle, args: &[Value]) {
    // —— 节流：1s 内只更新一次（等价 MIGRATION-TAURI.md 11 节"降低上传频率"对策）——
    let state = app.state::<AppState>();
    let mut last = match state.tray_last_update.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    let now = Instant::now();
    if let Some(t) = *last {
        if now.duration_since(t) < TRAY_UPDATE_THROTTLE {
            return;
        }
    }
    *last = Some(now);
    drop(last);

    let Some(image) = parse_tray_payload(args.first()) else {
        warn!("[Motrix] application:update-tray 载荷解析失败（数据为空或格式不符）");
        return;
    };
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        warn!("[Motrix] 未找到托盘实例（TRAY_ID={TRAY_ID}），忽略更新");
        return;
    };
    if let Err(e) = tray.set_icon(Some(image)) {
        warn!("[Motrix] 更新托盘图标失败: {e}");
    }
}

/// 解析托盘速度计载荷为 tauri 图像
///
/// 优先按 PNG 解码（前端 Canvas convertToBlob 默认输出 PNG 编码，尺寸信息在 PNG 头中）；
/// 解码失败（载荷为原始 RGBA）时使用载荷提供的宽高（约定 132×32），
/// 与"前端只改桥接、Rust 端按 132×32 约定"的契约保持一致。
fn parse_tray_payload(payload: Option<&Value>) -> Option<Image<'static>> {
    let (data, width, height) = match payload {
        // 结构化载荷：{ width, height, data: [...] }
        Some(Value::Object(map)) => {
            let data: Vec<u8> = map
                .get("data")?
                .as_array()?
                .iter()
                .filter_map(|x| x.as_u64())
                .map(|x| x as u8)
                .collect();
            let width = map
                .get("width")
                .and_then(|v| v.as_u64())
                .unwrap_or(TRAY_WIDTH as u64) as u32;
            let height = map
                .get("height")
                .and_then(|v| v.as_u64())
                .unwrap_or(TRAY_HEIGHT as u64) as u32;
            (data, width, height)
        }
        // 回退：纯字节数组（旧桥接 / shim 自动转换场景）
        Some(Value::Array(arr)) => {
            let data: Vec<u8> = arr
                .iter()
                .filter_map(|x| x.as_u64())
                .map(|x| x as u8)
                .collect();
            (data, TRAY_WIDTH, TRAY_HEIGHT)
        }
        _ => return None,
    };
    if data.is_empty() {
        return None;
    }

    // 1) 尝试 PNG 解码（真实宽高取自解码结果）
    if let Ok(decoded) = image::load_from_memory(&data) {
        let rgba = decoded.to_rgba8();
        let (w, h) = rgba.dimensions();
        if w > 0 && h > 0 {
            return Some(Image::new_owned(rgba.into_raw(), w, h));
        }
    }

    // 2) 原始 RGBA 回退：校验长度 = 宽 × 高 × 4 后直接构造
    if width > 0 && height > 0 && data.len() == (width as usize) * (height as usize) * 4 {
        return Some(Image::new_owned(data, width, height));
    }
    None
}

/// 左键单击托盘图标：切换主窗口显隐（等价 Electron handleTrayClick → application.toggle）
pub fn toggle_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let visible = win.is_visible().unwrap_or(false);
        if visible {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    }
}
