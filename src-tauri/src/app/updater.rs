//! 自动更新（等价 Electron 版 core/UpdateManager.js，electron-updater）
//!
//! 取舍说明：正式接入需 `tauri-plugin-updater` 并在 tauri.conf.json 配置
//! `plugins.updater.endpoints` 与 `pubkey`（GitHub Releases 的 latest.json 发布元数据）。
//! 当前发布流程未就绪（无 latest.json），直接接入插件会因缺少 pubkey 在启动时 panic，
//! 故本阶段 `application:check-for-updates` 返回"不可用"结果并记录日志，
//! 待发布流程就绪后替换为插件调用（`app.updater().check()`）即可，命令契约不变。

use tauri::AppHandle;
use serde_json::{json, Value};
use tracing::info;

/// 检查更新（application:check-for-updates 命令入口）
///
/// 返回值为 JSON：`{ available: false, reason, message }`——
/// 前端收到后按"无可用更新"处理（不报错），符合"无发布元数据时优雅降级"的策略。
pub fn check_for_updates(app: &AppHandle) -> Value {
    info!("[Motrix] 检查更新被调用（更新源未配置，返回不可用）");
    let _ = app;
    json!({
        "available": false,
        "reason": "update-source-not-configured",
        "message": "更新源未配置（latest.json 发布元数据就绪后将接入 tauri-plugin-updater）",
    })
}
