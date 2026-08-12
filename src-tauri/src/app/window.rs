//! 窗口状态保存与恢复（等价 Electron 版 ui/WindowManager.js 的窗口状态能力）
//!
//! - 主窗口移动 / 缩放时把边界（x/y/width/height）写入数据目录 `window-state.json`（节流 500ms）
//! - 启动 setup 时按 `keep-window-state`（user.json，默认关闭）恢复上次窗口边界
//! - `run-mode=TRAY` 或 `auto-hide-window` 时启动后隐藏窗口（等价 Electron showPage hidden）

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Window, WindowEvent};
use tracing::{info, warn};

use crate::AppState;

/// 窗口状态 JSON 文件名（位于数据目录下）
const WINDOW_STATE_FILE: &str = "window-state.json";
/// 窗口状态落盘节流间隔：移动 / 缩放事件高频触发，500ms 内合并为一次写盘
const WRITE_THROTTLE: Duration = Duration::from_millis(500);
/// 窗口最小尺寸下限（防止状态文件损坏导致窗口跑到屏幕外 / 尺寸为 0）
const MIN_WIDTH: u32 = 400;
const MIN_HEIGHT: u32 = 300;

/// 窗口边界（物理像素）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct WindowBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// 最近一次写盘时间（进程级节流；单实例应用，静态变量足够）
static LAST_WRITE: Mutex<Option<Instant>> = Mutex::new(None);

/// 应用运行模式（与 src/shared/constants.js 的 APP_RUN_MODE 对齐）
const APP_RUN_MODE_TRAY: u32 = 2;

/// setup 阶段调用：恢复窗口状态 + 按运行模式隐藏窗口
pub fn setup(app: &AppHandle) {
    let state = app.state::<AppState>();
    let (keep_window_state, run_mode, auto_hide_window) = match state.config_manager.lock() {
        Ok(config_manager) => {
            let user = config_manager.user_config();
            (
                user.keep_window_state,
                user.run_mode,
                user.auto_hide_window,
            )
        }
        Err(e) => {
            warn!("[Motrix] 读取窗口相关配置失败（获取配置锁失败）: {e}");
            (false, 1, false)
        }
    };
    let Some(win) = app.get_webview_window("main") else {
        warn!("[Motrix] 未找到主窗口，跳过窗口状态恢复");
        return;
    };

    // 恢复上次窗口边界（keep-window-state 为 false 时保持 tauri.conf.json 的默认尺寸）
    if keep_window_state {
        if let Some(bounds) = read_window_state(&state.data_dir) {
            let _ = win.set_position(tauri::PhysicalPosition::new(bounds.x, bounds.y));
            let _ = win.set_size(tauri::PhysicalSize::new(bounds.width, bounds.height));
            info!(
                "[Motrix] 已恢复窗口状态: x={} y={} {}x{}",
                bounds.x, bounds.y, bounds.width, bounds.height
            );
        }
    } else {
        info!("[Motrix] keep-window-state=false，使用默认窗口尺寸");
    }

    // run-mode=TRAY（托盘模式）或 auto-hide-window：启动后隐藏主窗口
    if run_mode == APP_RUN_MODE_TRAY || auto_hide_window {
        let _ = win.hide();
        info!("[Motrix] run-mode=TRAY 或 auto-hide-window=true，启动后隐藏主窗口");
    }
}

/// Builder.on_window_event 回调：移动 / 缩放时保存窗口状态（节流 500ms）；
/// 失焦时若开启 auto-hide-window 则隐藏窗口（等价 Electron handleWindowBlur）
pub fn on_window_event(window: &Window, event: &WindowEvent) {
    let app = window.app_handle();

    // 自动隐藏：窗口失焦且用户开启 auto-hide-window 时隐藏（等价 Electron windowBlur）
    if let WindowEvent::Focused(false) = event {
        let state = app.state::<AppState>();
        if state.auto_hide_window.load(std::sync::atomic::Ordering::SeqCst) {
            info!("[Motrix] 窗口失焦且 auto-hide-window 开启，隐藏主窗口");
            let _ = window.hide();
        }
        return;
    }

    // 仅处理移动 / 缩放
    let (pos, size) = match event {
        WindowEvent::Moved(p) => (Some(*p), None),
        WindowEvent::Resized(s) => (None, Some(*s)),
        _ => return,
    };

    // 是否保存窗口状态（keep-window-state 配置控制，等价 Electron storeWindowState）
    let keep = match app.state::<AppState>().config_manager.lock() {
        Ok(config_manager) => config_manager.user_config().keep_window_state,
        Err(e) => {
            warn!("[Motrix] 读取 keep-window-state 配置失败: {e}");
            return;
        }
    };
    if !keep {
        return;
    }

    // 节流：500ms 内只写一次盘（避免拖动窗口时高频磁盘 IO）
    let mut last = match LAST_WRITE.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    let now = Instant::now();
    if let Some(t) = *last {
        if now.duration_since(t) < WRITE_THROTTLE {
            return;
        }
    }
    *last = Some(now);
    drop(last);

    // 移动与缩放事件通常成对到达；只收到其一时代取当前值补全
    let p = pos.unwrap_or_else(|| {
        window
            .outer_position()
            .unwrap_or(tauri::PhysicalPosition::new(0, 0))
    });
    let s = size.unwrap_or_else(|| {
        window
            .outer_size()
            .unwrap_or(tauri::PhysicalSize::new(1000, 700))
    });
    let bounds = WindowBounds {
        x: p.x,
        y: p.y,
        width: s.width,
        height: s.height,
    };
    let path = app.state::<AppState>().data_dir.join(WINDOW_STATE_FILE);
    match serde_json::to_string_pretty(&bounds) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                warn!("[Motrix] 保存窗口状态失败: {e}");
            }
        }
        Err(e) => warn!("[Motrix] 序列化窗口状态失败: {e}"),
    }
}

/// 读取上次保存的窗口边界；文件不存在 / 损坏 / 尺寸非法时返回 None
fn read_window_state(data_dir: &Path) -> Option<WindowBounds> {
    let path = data_dir.join(WINDOW_STATE_FILE);
    let content = std::fs::read_to_string(path).ok()?;
    let bounds: WindowBounds = serde_json::from_str(&content).ok()?;
    // 防止状态文件异常（多显示器变更等）导致窗口不可见
    if bounds.width < MIN_WIDTH || bounds.height < MIN_HEIGHT {
        warn!(
            "[Motrix] 窗口状态尺寸异常（{}x{}），忽略恢复",
            bounds.width, bounds.height
        );
        return None;
    }
    Some(bounds)
}
