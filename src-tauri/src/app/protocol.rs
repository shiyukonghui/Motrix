//! 深链协议（等价 Electron 版 core/ProtocolManager.js：mo: / motrix: / magnet: / http(s)）
//!
//! 实现取舍：tauri-plugin-deep-link 在 Windows 上依赖打包期注册机制且 dev 模式不生效，
//! 为保证启动稳定与三平台行为一致，本模块采用轻量方案（注释说明取舍）：
//! - **Windows**：打包产物（非 dev）启动时用 `reg add` 写 HKCU 注册表注册协议
//!   （URL 以命令行参数 %1 传给本进程）；dev 模式跳过注册（与 Electron is.dev() 一致）
//! - **macOS / Linux**：协议注册依赖打包期 Info.plist / .desktop 文件，运行时不做注册
//! - **URL 处理**（系统以命令行参数传入）：magnet: → add_torrent；http(s)/ftp: →
//!   add_uri；mo:/motrix: → 解析 host 为命令并分发（渲染命令转发前端 / 原生命令自处理）

use std::path::Path;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use crate::AppState;

/// 判断命令行参数是否为 Motrix 关注的协议 URL
fn is_protocol_url(arg: &str) -> bool {
    let lower = arg.to_lowercase();
    lower.starts_with("mo:")
        || lower.starts_with("motrix:")
        || lower.starts_with("magnet:")
        || lower.starts_with("http:")
        || lower.starts_with("https:")
        || lower.starts_with("ftp:")
}

/// 处理启动参数中的深链 URL（setup 与单实例插件回调均调用）
pub fn handle_args(app: &AppHandle, args: &[String]) {
    for arg in args {
        if is_protocol_url(arg) {
            handle_protocol_url(app, arg);
        }
    }
}

/// 处理单个协议 URL（等价 Electron ProtocolManager.handle）
pub fn handle_protocol_url(app: &AppHandle, url: &str) {
    info!("[Motrix] 收到协议 URL: {url}");
    let lower = url.to_lowercase();

    // 资源类协议：直接创建下载任务（等价 handleResourceProtocol）
    if lower.starts_with("magnet:") {
        let state = app.state::<AppState>();
        // 先显示窗口（等价 Electron handleProtocol 先 show 再处理）
        crate::show_main_window(app);
        match state.task_manager.add_torrent(url, &json!({})) {
            Ok(gid) => info!("[Motrix] 深链添加 BT 任务成功: {gid}"),
            Err(e) => warn!("[Motrix] 深链添加 BT 任务失败: {e}"),
        }
        return;
    }
    if lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("ftp:") {
        let state = app.state::<AppState>();
        crate::show_main_window(app);
        match state.task_manager.add_uri(&[url.to_string()], &json!({})) {
            Ok(gids) => info!("[Motrix] 深链添加下载任务成功: {gids:?}"),
            Err(e) => warn!("[Motrix] 深链添加下载任务失败: {e}"),
        }
        return;
    }

    // mo:/motrix: 内部命令协议（等价 handleMoProtocol）
    if lower.starts_with("mo:") || lower.starts_with("motrix:") {
        handle_mo_protocol(app, url);
    }
}

/// 解析 mo:/motrix: 协议：host 映射命令，query 作为命令参数（等价 src/main/configs/protocol.js）
fn handle_mo_protocol(app: &AppHandle, url: &str) {
    // 手工解析（避免引入 url 解析依赖）：去掉 scheme 后取 host 与 query
    let rest = url.split_once(':').map(|(_, r)| r).unwrap_or("");
    let rest = rest.trim_start_matches('/');
    let (host, query) = match rest.find(['?', '/']) {
        Some(idx) => (&rest[..idx], rest[idx..].strip_prefix(['?', '/'])),
        None => (rest, None),
    };
    let Some(command) = protocol_host_to_command(host) else {
        warn!("[Motrix] 未知 mo: 协议命令: {host}（URL: {url}）");
        return;
    };

    // 把 query 字符串解析为参数对象（等价 querystring.parse）
    let mut args_map = serde_json::Map::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                args_map.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
    }
    let args: Vec<Value> = if args_map.is_empty() {
        Vec::new()
    } else {
        vec![Value::Object(args_map)]
    };

    info!("[Motrix] mo: 协议分发命令: {command}");
    let state = app.state::<AppState>();
    crate::handle_command(app, &state, command, &args);
}

/// host → 命令映射（与 src/main/configs/protocol.js 一致）
fn protocol_host_to_command(host: &str) -> Option<&'static str> {
    match host {
        "task-list" => Some("application:task-list"),
        "new-task" => Some("application:new-task"),
        "new-bt-task" => Some("application:new-bt-task"),
        "pause-all-task" => Some("application:pause-all-task"),
        "resume-all-task" => Some("application:resume-all-task"),
        "reveal-in-folder" => Some("application:reveal-in-folder"),
        "preferences" => Some("application:preferences"),
        "about" => Some("application:about"),
        _ => None,
    }
}

/// 注册协议客户端（application:setup-protocols-client 命令入口 / setup 启动时调用）
///
/// - dev 模式（debug_assertions）跳过注册（等价 Electron is.dev()）
/// - protocols 为 user.json 的 protocols 对象（{ magnet, thunder }，mo/motrix 恒注册）
pub fn setup_protocols_client(_app: &AppHandle, protocols: Option<&Value>) {
    if cfg!(debug_assertions) {
        info!("[Motrix] dev 模式跳过协议注册（打包产物才会注册）");
        return;
    }

    // 决定要注册的 scheme：mo / motrix 恒注册；magnet 按配置（默认开启）
    let mut schemes: Vec<&str> = vec!["mo", "motrix"];
    let magnet_enabled = protocols
        .and_then(|p| p.get("magnet"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if magnet_enabled {
        schemes.push("magnet");
    }

    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            warn!("[Motrix] 无法定位当前可执行文件，跳过协议注册: {e}");
            return;
        }
    };

    #[cfg(target_os = "windows")]
    {
        register_windows(&exe, &schemes);
    }
    #[cfg(not(target_os = "windows"))]
    {
        // macOS/Linux：协议注册依赖打包期 Info.plist / .desktop 文件，
        // 运行时不做注册（与 Electron 版在 is.dev() 外走 setAsDefaultProtocolClient 的差异，
        // 以保持三平台行为一致，详见模块头注释）
        info!("[Motrix] 非 Windows 平台协议注册由打包期配置完成（Info.plist / .desktop），运行时跳过");
        let _ = _app;
    }
}

/// Windows：向 HKCU 注册表写入协议关联（reg add，等价 app.setAsDefaultProtocolClient）
#[cfg(target_os = "windows")]
fn register_windows(exe: &Path, schemes: &[&str]) {
    use std::process::Command;

    let exe_str = exe.display().to_string();
    for scheme in schemes {
        // 协议根键（含描述）
        let _ = Command::new("reg")
            .args([
                "add",
                &format!(r"HKCU\Software\Classes\{scheme}"),
                "/ve",
                "/d",
                &format!("URL:Motrix {scheme} Protocol"),
                "/f",
            ])
            .status();
        // 打开命令：指向本 exe，参数 %1 为完整 URL
        let _ = Command::new("reg")
            .args([
                "add",
                &format!(r"HKCU\Software\Classes\{scheme}\shell\open\command"),
                "/ve",
                "/d",
                &format!("\"{exe_str}\" \"%1\""),
                "/f",
            ])
            .status();
        // 默认图标（可选）
        let _ = Command::new("reg")
            .args([
                "add",
                &format!(r"HKCU\Software\Classes\{scheme}\DefaultIcon"),
                "/ve",
                "/d",
                &format!("\"{exe_str}\",0"),
                "/f",
            ])
            .status();
    }
    info!("[Motrix] Windows 协议注册完成: {schemes:?}");
}
