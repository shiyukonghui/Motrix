//! 开机自启（等价 Electron 版 core/AutoLaunchManager.js，基于 tauri-plugin-autostart）
//!
//! - 启动时按 user.json 的 `open-at-login` 配置应用自启
//! - 配置变化时（save-preference 联动）即时启用 / 关闭

use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;
use tracing::{info, warn};

/// 启动时按 user.json 的 open-at-login 配置应用开机自启
pub fn apply_configured(app: &AppHandle) {
    let enabled = match app.state::<crate::AppState>().config_manager.lock() {
        Ok(config_manager) => config_manager.user_config().open_at_login,
        Err(e) => {
            warn!("[Motrix] 读取 open-at-login 配置失败（获取配置锁失败）: {e}");
            return;
        }
    };
    apply(app, enabled);
}

/// 启用 / 关闭开机自启（open-at-login 配置变化时即时调用）
pub fn apply(app: &AppHandle, enabled: bool) {
    let result = if enabled {
        app.autolaunch().enable()
    } else {
        app.autolaunch().disable()
    };
    match result {
        Ok(()) => info!(
            "[Motrix] 开机自启已{}",
            if enabled { "开启" } else { "关闭" }
        ),
        Err(e) => warn!("[Motrix] 设置开机自启失败: {e}"),
    }
}
