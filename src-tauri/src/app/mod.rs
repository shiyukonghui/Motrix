//! Phase 4：平台能力模块（等价 Electron 版 src/main 下的 Manager / core 模块）
//!
//! - `window.rs`   窗口状态保存与恢复（原 WindowManager）
//! - `tray.rs`     系统托盘 + 动态速度计（原 TrayManager）
//! - `menu.rs`     应用菜单（原 MenuManager，按平台构建）
//! - `autostart.rs` 开机自启（原 AutoLaunchManager，tauri-plugin-autostart）
//! - `protocol.rs` 深链协议 mo:/motrix:/magnet:（原 ProtocolManager）
//! - `updater.rs`  自动更新（原 UpdateManager；当前发布流程未就绪，降级为占位）
//! - `energy.rs`   防休眠（原 EnergyManager；轻量实现 + 后续接入点）
//! - `upnp.rs`     UPnP 端口映射（原 UPnPManager，igd crate）

pub mod autostart;
pub mod energy;
pub mod menu;
pub mod protocol;
pub mod tray;
pub mod updater;
pub mod upnp;
pub mod window;
