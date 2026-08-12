//! 防休眠（等价 Electron 版 core/EnergyManager.js，powerSaveBlocker）
//!
//! 由 `download-status-change` 事件驱动：有活动下载时保持系统不睡眠，空闲时恢复。
//!
//! 实现取舍：Windows 原生防休眠需调用 `SetThreadExecutionState(ES_CONTINUOUS |
//! ES_SYSTEM_REQUIRED)`（需 windows crate，依赖较重）；Tauri 目前无官方 power 插件。
//! 本阶段实现"状态跟踪 + 日志 + 后续接入点"，避免引入重型依赖——
//! 后续可在 `handle_download_status_change` 中接入平台 API（windows crate /
//! macOS IOKit / Linux systemd-inhibit）实现真正的防休眠。

use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

/// 是否有活动下载（内存状态，等价 Electron EnergyManager 的 psbId 标记）
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

/// 处理 download-status-change 事件：更新状态并记录日志（防休眠接入点）
pub fn handle_download_status_change(downloading: bool) {
    let old = DOWNLOADING.swap(downloading, Ordering::SeqCst);
    if old != downloading {
        info!(
            "[Motrix] 下载状态变化: downloading={downloading}（防休眠接入点：{}）",
            if downloading {
                "保持系统不睡眠"
            } else {
                "恢复系统睡眠策略"
            }
        );
    }
}

/// 当前是否有活动下载（供后续防休眠 / 托盘状态使用；当前为预留接入点，允许暂未引用）
#[allow(dead_code)]
pub fn is_downloading() -> bool {
    DOWNLOADING.load(Ordering::SeqCst)
}
