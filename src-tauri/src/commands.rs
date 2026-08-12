//! Tauri commands：前端经 invoke() 调用的命令实现
//!
//! 提供三类命令：
//! - 配置读写（Phase 0）：get_app_config / save_app_config
//! - 任务操作（Phase 2，Task 10）：add_uri / add_torrent / pause_task / resume_task /
//!   remove_task / change_option / change_global_option / get_global_stat / get_engine_info /
//!   get_tasks / get_task_detail / get_peers / save_session / purge
//! - 原生 shell 能力（Task 3）：show_item_in_folder / open_path / trash_item
//!   （等价 Electron 版 shell.showItemInFolder / shell.openPath / shell.trashItem）
//!
//! 任务操作命令已接入 motrix-core 真实引擎（TaskManager）：添加 / 暂停 / 恢复 / 删除
//! 均由 KGet / librqbit 引擎实际执行；add_torrent（BT）与 get_peers 属 Phase 3，
//! 已实现真实行为（BT 任务创建 / peers 查询，见 MIGRATION-TAURI.md 5.5）。

use serde_json::{json, Value};
use tauri::State;
use tracing::debug;

use crate::AppState;

// ==================== 配置读写命令（Phase 0） ====================

/// 获取应用完整配置：`{ user, system, context }`
///
/// - user / system：user.json / system.json 内容（kebab-case 键，与 Electron 版一致）
/// - context：平台 / 架构 / 版本 / 日志路径 / 会话路径 / 数据目录
#[tauri::command]
pub fn get_app_config(state: State<'_, AppState>) -> Result<Value, String> {
    let config_manager = state
        .config_manager
        .lock()
        .map_err(|e| format!("获取配置锁失败: {e}"))?;

    let data_dir = config_manager.data_dir().to_path_buf();
    let user = config_manager.user_config_json();
    let system = config_manager.system_config_json();
    drop(config_manager);

    // context 键名（kebab-case）与 Electron 版 Context.js 对齐，并补充版本信息
    let context = json!({
        "platform": crate::tauri_platform(),
        "arch": std::env::consts::ARCH,
        "version": env!("CARGO_PKG_VERSION"),
        "log-path": data_dir.join("logs").join("motrix.log").to_string_lossy().to_string(),
        "session-path": data_dir.join("download.session").to_string_lossy().to_string(),
        "user-data-path": data_dir.to_string_lossy().to_string(),
    });

    // 返回**扁平合并对象**（与 Electron 版 get-app-config 一致：`{...system, ...user, ...context}`）：
    // 前端 Api.js::loadConfig 对结果做 changeKeysToCamelCase 后整体作为 preference.config，
    // 视觉组件直接读取顶层键（如 config.locale / config.theme / config.maxConcurrentDownloads）。
    // 若按 user/system/context 三层嵌套返回，config.locale 等顶层键将缺失，
    // 导致设置页等组件 data() 抛 "Cannot read properties of undefined (reading 'startsWith')"。
    let mut merged = serde_json::Map::new();
    // 合并顺序与 Electron 一致：system 为底 → user 覆盖 → context 覆盖（同键后者胜）
    for (k, v) in system.as_object().into_iter().flatten() {
        merged.insert(k.clone(), v.clone());
    }
    for (k, v) in user.as_object().into_iter().flatten() {
        merged.insert(k.clone(), v.clone());
    }
    for (k, v) in context.as_object().into_iter().flatten() {
        merged.insert(k.clone(), v.clone());
    }
    Ok(Value::Object(merged))
}

/// 保存应用配置：payload 形如 `{ "user": {...}, "system": {...} }`，分区写回对应 JSON 文件
#[tauri::command]
pub fn save_app_config(state: State<'_, AppState>, payload: Value) -> Result<(), String> {
    let user = payload.get("user");
    let system = payload.get("system");

    let mut config_manager = state
        .config_manager
        .lock()
        .map_err(|e| format!("获取配置锁失败: {e}"))?;
    config_manager
        .apply_preference(user, system)
        .map_err(|e| format!("保存配置失败: {e}"))?;
    drop(config_manager);

    // Task 10.2：system 分区变更同步引擎全局选项（等价 changeGlobalOption，
    // 使限速 / 代理 / 并发等对后续任务即时生效）
    if let Some(system_patch) = system {
        if system_patch.is_object() {
            state
                .task_manager
                .change_global_option(system_patch)
                .map_err(|e| format!("同步引擎全局选项失败: {e}"))?;
        }
    }
    Ok(())
}

// ==================== 任务操作命令（Phase 2，Task 10） ====================

/// 添加 URL 任务（真实引擎）：接入 motrix-core TaskManager
///
/// 每个 URL 创建独立任务并返回对应 gid 列表；options 为任务级选项
/// （dir/out/connections/限速/代理等，kebab-case 键），多 URL 各自成任务。
#[tauri::command]
pub fn add_uri(
    state: State<'_, AppState>,
    uris: Vec<String>,
    options: Option<Value>,
) -> Result<Vec<String>, String> {
    debug!("[Motrix] add_uri 调用：{} 个 URL", uris.len());
    let opts = options.unwrap_or(Value::Null);
    state.task_manager.add_uri(&uris, &opts)
}

/// 添加种子任务（真实引擎）：BT 经 TaskManager → librqbit（Phase 3）
#[tauri::command]
pub fn add_torrent(
    state: State<'_, AppState>,
    torrent: String,
    options: Option<Value>,
) -> Result<String, String> {
    debug!("[Motrix] add_torrent 调用（magnet / base64 .torrent → librqbit）");
    let opts = options.unwrap_or(Value::Null);
    state.task_manager.add_torrent(&torrent, &opts)
}

/// 暂停任务：abort 引擎句柄 + 置 Paused（KGet 保留部分文件供续传）
#[tauri::command]
pub fn pause_task(state: State<'_, AppState>, gid: String) -> Result<String, String> {
    debug!("[Motrix] pause_task 调用：gid={gid}");
    state.task_manager.pause(&gid)
}

/// 恢复任务：置 Active 并重新 spawn（KGet 基于 Range 断点续传）
#[tauri::command]
pub fn resume_task(state: State<'_, AppState>, gid: String) -> Result<String, String> {
    debug!("[Motrix] resume_task 调用：gid={gid}");
    state.task_manager.resume(&gid)
}

/// 移除任务：abort 引擎句柄 + 移除仓库记录
///
/// - `force`：是否强制移除（可选，默认 false；KGet 的 abort 本身就是强停，
///   与普通移除行为一致，参数保留以兼容 aria2 调用语义）
/// - `delete_files`：是否连带删除已下载文件（可选，默认 false；**本次忽略**，
///   文件删除能力在后续阶段提供，仅记录参数）
#[tauri::command]
pub fn remove_task(
    state: State<'_, AppState>,
    gid: String,
    force: Option<bool>,
    delete_files: Option<bool>,
) -> Result<String, String> {
    debug!("[Motrix] remove_task 调用：gid={gid}, force={force:?}, delete_files={delete_files:?}");
    let _ = (force, delete_files);
    state.task_manager.remove(&gid)
}

/// 修改任务选项：更新任务级选项（运行中任务下次恢复生效；已暂停任务不自动重启）
#[tauri::command]
pub fn change_option(
    state: State<'_, AppState>,
    gid: String,
    options: Value,
) -> Result<String, String> {
    debug!("[Motrix] change_option 调用：gid={gid}");
    state.task_manager.change_option(&gid, &options)
}

/// 修改全局选项：更新内存全局选项 + max_concurrent，并持久化到 system.json
#[tauri::command]
pub fn change_global_option(
    state: State<'_, AppState>,
    options: Value,
) -> Result<String, String> {
    debug!("[Motrix] change_global_option 调用");
    state.task_manager.change_global_option(&options)
}

/// 获取全局统计：经 TaskManager 聚合任务仓库（active/waiting/stopped 数量 + 总速度）
#[tauri::command]
pub fn get_global_stat(state: State<'_, AppState>) -> Result<Value, String> {
    Ok(state.task_manager.global_stat().to_aria2_json())
}

/// 获取全局选项（aria2.getGlobalOption 兼容基础子集）
///
/// 从 system.json（经 ConfigManager）读取 dir / 并发 / 连接数 / UA / 限速 /
/// 代理等常见键，数值一律转字符串（aria2 惯例）；返回键为 kebab-case，
/// 前端 Api.js 会再做 changeKeysToCamelCase 转换（与旧 JSON-RPC 行为一致）。
/// `header` 为 SystemConfig 未建模键（存于 extra），按原值透出（数组 / 字符串）。
#[tauri::command]
pub fn get_global_option(state: State<'_, AppState>) -> Result<Value, String> {
    let config_manager = state
        .config_manager
        .lock()
        .map_err(|e| format!("获取配置锁失败: {e}"))?;
    let system = config_manager.system_config();
    let header = system.extra.get("header").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "dir": system.dir,
        "max-concurrent-downloads": system.max_concurrent_downloads.to_string(),
        "max-connection-per-server": system.max_connection_per_server.to_string(),
        "split": system.split.to_string(),
        "user-agent": system.user_agent,
        "max-download-limit": system.max_download_limit.to_string(),
        "max-overall-download-limit": system.max_overall_download_limit.to_string(),
        "max-overall-upload-limit": system.max_overall_upload_limit.to_string(),
        "all-proxy": system.all_proxy,
        "no-proxy": system.no_proxy,
        "continue": system.r#continue.to_string(),
        "header": header,
    }))
}

/// 获取引擎信息：真实版本串与功能列表（与 motrix-rpc 的 aria2.getVersion 保持一致，
/// 版本串统一取自 motrix-core::ENGINE_VERSION，随构建动态生成）
#[tauri::command]
pub fn get_engine_info() -> Result<Value, String> {
    Ok(json!({
        "version": motrix_core::ENGINE_VERSION,
        "enabledFeatures": [],
    }))
}

/// 获取任务列表：任务仓库全部在册任务的 aria2 兼容 JSON 列表
#[tauri::command]
pub fn get_tasks(state: State<'_, AppState>) -> Result<Value, String> {
    let repo = state
        .task_manager
        .repo
        .lock()
        .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
    let tasks: Vec<Value> = repo.all().iter().map(|t| t.to_aria2()).collect();
    Ok(json!(tasks))
}

/// 获取任务详情：按 gid 返回单任务 aria2 兼容 JSON；不存在返回 null
///
/// BT 任务的 `bitfield` 字段已在 bt.rs 的进度轮询时**降采样**为 ≤240 个十六进制
/// 字符并存储于 Task.bitfield（见 MIGRATION-TAURI.md 5.8：根治 Electron 版大
/// bitfield 白屏），此处 to_aria2() 直接输出该降采样值，无需额外处理。
#[tauri::command]
pub fn get_task_detail(state: State<'_, AppState>, gid: String) -> Result<Value, String> {
    let repo = state
        .task_manager
        .repo
        .lock()
        .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
    Ok(repo.get(&gid).map(|t| t.to_aria2()).unwrap_or(Value::Null))
}

/// 获取任务 peers 列表（aria2.getPeers 兼容）：经 TaskManager 查询 BT 引擎
///
/// 返回字段（数值为字符串，aria2 惯例）：ip / port / peerId / bitfield /
/// downloadSpeed / uploadSpeed；默认最多 100 条（分页硬约束）。
/// 注：librqbit 8.1.1 未在公开 API 暴露 per-peer 明细，当前返回空数组
/// （契约 / 分页逻辑保留，待 librqbit 上游提供 per-peer stats 后填充）。
#[tauri::command]
pub fn get_peers(state: State<'_, AppState>, gid: String) -> Result<Value, String> {
    debug!("[Motrix] get_peers 调用：gid={gid}");
    let peers = state.task_manager.get_peers(&gid, 100);
    let list: Vec<Value> = peers
        .iter()
        .map(|p| {
            json!({
                "ip": p.ip,
                "port": p.port.to_string(),
                "peerId": p.peer_id,
                "bitfield": p.bitfield,
                "downloadSpeed": p.download_speed.to_string(),
                "uploadSpeed": p.upload_speed.to_string(),
            })
        })
        .collect();
    Ok(json!(list))
}

/// 保存会话（等价 aria2.saveSession）：把任务仓库写入 checkpoint.json
///
/// checkpoint 记录每个任务的状态 / URL / 已下载字节 / 进度，
/// 退出时（RunEvent::ExitRequested）与手动触发时保存，下次启动经
/// motrix_core::session::restore_session 恢复，未完成任务基于已下载字节续传。
#[tauri::command]
pub fn save_session(state: State<'_, AppState>) -> Result<String, String> {
    debug!("[Motrix] save_session 调用（写入 checkpoint.json）");
    let repo = state
        .task_manager
        .repo
        .lock()
        .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
    motrix_core::session::save_checkpoint(&state.data_dir, &repo)?;
    Ok("OK".to_string())
}

/// 清空已完成/已移除任务记录：经 TaskManager 清空 removed 历史（等价 aria2.purgeDownloadResult）
#[tauri::command]
pub fn purge(state: State<'_, AppState>) -> Result<String, String> {
    debug!("[Motrix] purge 调用（清空 removed 历史）");
    state.task_manager.purge();
    Ok("OK".to_string())
}

// ==================== 原生 shell 能力命令（Task 3） ====================
// 等价 Electron 版 shell.showItemInFolder / openPath / trashItem，
// 前端 src/shims/remote-shell.js 经 invoke() 调用；平台命令直接用
// std::process::Command spawn 系统程序（explorer / open / xdg-open），
// 不引入 tauri-plugin，避免额外权限与依赖。

/// 平台命令动作：定位文件 / 打开文件目录
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellAction {
    /// 在系统文件管理器中定位并选中文件
    Reveal,
    /// 用系统默认程序打开文件 / 目录
    Open,
}

/// 构造平台 shell 命令，返回 `[程序名, 参数...]` 列表
///
/// - Windows：`explorer /select,<path>`（`/select,` 需与路径拼成单个参数串）；
///   打开用 `explorer <path>`
/// - macOS：定位用 `open -R <path>`；打开用 `open <path>`
/// - Linux：`xdg-open`（打开 / 定位均用它）；Linux 文件管理器没有可靠的标准
///   "定位文件"命令，故 Reveal 退化为打开文件所在父目录（此限制在注释中说明）
fn build_shell_command(action: ShellAction, path: &str) -> Vec<String> {
    match action {
        ShellAction::Reveal => {
            #[cfg(target_os = "windows")]
            {
                vec!["explorer".to_string(), format!("/select,{path}")]
            }
            #[cfg(target_os = "macos")]
            {
                vec!["open".to_string(), "-R".to_string(), path.to_string()]
            }
            #[cfg(target_os = "linux")]
            {
                // Linux 无"定位并选中文件"的标准命令，打开父目录作为近似
                let parent = std::path::Path::new(path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string());
                vec!["xdg-open".to_string(), parent]
            }
        }
        ShellAction::Open => {
            #[cfg(target_os = "windows")]
            {
                vec!["explorer".to_string(), path.to_string()]
            }
            #[cfg(target_os = "macos")]
            {
                vec!["open".to_string(), path.to_string()]
            }
            #[cfg(target_os = "linux")]
            {
                vec!["xdg-open".to_string(), path.to_string()]
            }
        }
    }
}

/// spawn 平台命令（分离执行，不等待子进程结束），失败返回原因
fn run_spawned(program: &str, args: &[String]) -> Result<(), String> {
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 {program} 失败: {e}"))
}

/// 在系统文件管理器中定位并选中该文件（等价 Electron shell.showItemInFolder）
///
/// - Windows：`explorer /select,<path>`；macOS：`open -R <path>`
/// - Linux：xdg-open 打开文件所在父目录（文件管理器无可靠"定位文件"标准命令）
#[tauri::command]
pub fn show_item_in_folder(path: String) -> Result<String, String> {
    let cmd = build_shell_command(ShellAction::Reveal, &path);
    // build_shell_command 保证列表非空：首元素为程序名，其余为参数
    run_spawned(&cmd[0], &cmd[1..])?;
    Ok("OK".to_string())
}

/// 用系统默认程序打开文件 / 目录（等价 Electron shell.openPath）
///
/// Windows：`explorer <path>`；macOS：`open <path>`；Linux：`xdg-open <path>`
#[tauri::command]
pub fn open_path(path: String) -> Result<String, String> {
    let cmd = build_shell_command(ShellAction::Open, &path);
    run_spawned(&cmd[0], &cmd[1..])?;
    Ok("OK".to_string())
}

/// 将文件 / 目录移到回收站（等价 Electron shell.trashItem）
///
/// 使用 `trash` crate 跨平台实现（Windows 回收站 / macOS Trash / Linux trash）。
/// 失败返回 Err（含原因），成功返回 "OK"。
#[tauri::command]
pub fn trash_item(path: String) -> Result<String, String> {
    trash::delete(&path).map_err(|e| format!("移入回收站失败: {e}"))?;
    Ok("OK".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// build_shell_command 的平台分支断言：仅编译当前目标平台对应的用例
    #[test]
    fn build_shell_command_reveal() {
        #[cfg(target_os = "windows")]
        assert_eq!(
            build_shell_command(ShellAction::Reveal, r"C:\Users\test\file.txt"),
            vec![
                "explorer".to_string(),
                r"/select,C:\Users\test\file.txt".to_string()
            ]
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            build_shell_command(ShellAction::Reveal, "/tmp/file.txt"),
            vec![
                "open".to_string(),
                "-R".to_string(),
                "/tmp/file.txt".to_string()
            ]
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            build_shell_command(ShellAction::Reveal, "/tmp/file.txt"),
            vec!["xdg-open".to_string(), "/tmp".to_string()]
        );
    }

    /// build_shell_command 打开分支断言：仅编译当前目标平台对应的用例
    #[test]
    fn build_shell_command_open() {
        #[cfg(target_os = "windows")]
        assert_eq!(
            build_shell_command(ShellAction::Open, r"C:\Users\test\file.txt"),
            vec!["explorer".to_string(), r"C:\Users\test\file.txt".to_string()]
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            build_shell_command(ShellAction::Open, "/tmp/file.txt"),
            vec!["open".to_string(), "/tmp/file.txt".to_string()]
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            build_shell_command(ShellAction::Open, "/tmp/file.txt"),
            vec!["xdg-open".to_string(), "/tmp/file.txt".to_string()]
        );
    }

    /// trash_item 往返测试：临时文件被移入回收站后原路径不再存在
    ///
    /// 注意：回收站操作依赖系统桌面环境，CI / 无桌面沙箱可能受限；
    /// 若环境不支持可将本测试标注 #[ignore] 跳过（本机 Windows 应可跑通）。
    #[test]
    fn trash_item_moves_file_to_trash() {
        // 用"进程 id + 纳秒时间戳"拼唯一临时文件名，避免并发测试冲突
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间应晚于 UNIX 纪元")
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("motrix_trash_test_{}_{}.tmp", std::process::id(), nanos));

        std::fs::write(&path, b"trash test payload").expect("写临时文件失败");
        assert!(path.exists(), "临时文件应已创建");

        let result = trash_item(path.to_string_lossy().to_string());
        assert!(result.is_ok(), "trash_item 应成功，实际: {:?}", result);
        assert!(
            !path.exists(),
            "文件移入回收站后原路径不应再存在（已移入回收站）"
        );
    }
}
