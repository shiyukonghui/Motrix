//! UPnP 端口映射（等价 Electron 版 core/UPnPManager.js，@motrix/nat-api → igd crate）
//!
//! - user.json 的 `enable-upnp` 开启时，把 system.json 的 `listen-port` / `dht-listen-port`
//!   映射到路由器（等价 Electron startUPnPMapping）
//! - 异步执行（spawn_blocking），失败仅记 warn，不阻塞启动
//! - `enable-upnp` 配置变化 / 端口变化时即时重建映射（save-preference 联动调用）

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::Manager;
use tracing::{info, warn};

use crate::AppState;

/// 是否已建立端口映射（内存标记，等价 Electron UPnPManager 的 mappingStatus）
static MAPPED: AtomicBool = AtomicBool::new(false);

/// setup 阶段调用：enable-upnp 开启时异步建立端口映射（失败不阻塞启动）
pub fn maybe_setup_upnp(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let enabled = match state.config_manager.lock() {
        Ok(config_manager) => config_manager.user_config().enable_upnp,
        Err(e) => {
            warn!("[Motrix] 读取 enable-upnp 配置失败（获取配置锁失败）: {e}");
            return;
        }
    };
    if !enabled {
        info!("[Motrix] enable-upnp=false，跳过 UPnP 端口映射");
        return;
    }
    start_mapping(&state);
}

/// 建立端口映射（异步，不阻塞主线程）
///
/// igd 的网关发现 / 映射请求为同步 IO，放到 spawn_blocking 中执行，
/// 避免阻塞 Tauri 事件循环（失败仅记 warn，不影响启动与下载）。
pub fn start_mapping(state: &AppState) {
    if MAPPED.load(Ordering::SeqCst) {
        return;
    }
    // 读取待映射端口（BT 监听端口 + DHT 监听端口）
    let ports = match state.config_manager.lock() {
        Ok(config_manager) => {
            let system = config_manager.system_config();
            vec![system.listen_port, system.dht_listen_port]
        }
        Err(e) => {
            warn!("[Motrix] 读取 UPnP 端口配置失败: {e}");
            return;
        }
    };
    tauri::async_runtime::spawn_blocking(move || {
        match igd::search_gateway(igd::SearchOptions::default()) {
            Ok(gateway) => {
                // 本机内网 IP：向公网地址建立 UDP 连接读取本地地址（std 技巧，避免额外依赖）
                let Some(local_ip) = local_ip() else {
                    warn!("[Motrix] UPnP 无法确定本机内网 IP，跳过映射");
                    return;
                };
                for port in ports {
                    // TCP 端口映射（IGD 标准 AddPortMapping；igd 0.12 的 add_port 参数为
                    // (协议, 外部端口, 内网 SocketAddrV4, 租期, 描述)，lease=0 表示永久）
                    let client = SocketAddrV4::new(local_ip, port);
                    match gateway.add_port(igd::PortMappingProtocol::TCP, port, client, 0, "Motrix") {
                        Ok(()) => {
                            info!("[Motrix] UPnP 端口映射成功: {port} -> {local_ip}:{port}")
                        }
                        Err(e) => warn!("[Motrix] UPnP 映射端口 {port} 失败: {e}"),
                    }
                }
                MAPPED.store(true, Ordering::SeqCst);
            }
            Err(e) => warn!(
                "[Motrix] UPnP 网关发现失败（路由器未开启 UPnP 或不在同一局域网）: {e}"
            ),
        }
    });
}

/// 移除端口映射（enable-upnp 关闭时调用；等价 Electron stopUPnPMapping）
pub fn stop_mapping(state: &AppState) {
    if !MAPPED.load(Ordering::SeqCst) {
        return;
    }
    let ports = match state.config_manager.lock() {
        Ok(config_manager) => {
            let system = config_manager.system_config();
            vec![system.listen_port, system.dht_listen_port]
        }
        Err(e) => {
            warn!("[Motrix] 读取 UPnP 端口配置失败: {e}");
            return;
        }
    };
    tauri::async_runtime::spawn_blocking(move || {
        if let Ok(gateway) = igd::search_gateway(igd::SearchOptions::default()) {
            for port in ports {
                // igd 0.12 的 remove_port 只需 (协议, 外部端口)
                match gateway.remove_port(igd::PortMappingProtocol::TCP, port) {
                    Ok(()) => info!("[Motrix] UPnP 端口映射已移除: {port}"),
                    Err(e) => warn!("[Motrix] UPnP 移除映射 {port} 失败: {e}"),
                }
            }
        }
        MAPPED.store(false, Ordering::SeqCst);
    });
}

/// 获取本机内网 IPv4 地址（std 技巧：向公网 DNS 地址建立 UDP 连接后读取本地地址，
/// 无需引入 local-ip-address 等额外依赖）
fn local_ip() -> Option<Ipv4Addr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().and_then(|addr| match addr.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    })
}
