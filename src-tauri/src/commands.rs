//! Tauri commands：前端经 invoke() 调用的命令实现
//!
//! 提供两类命令：
//! - 配置读写（Phase 0）：get_app_config / save_app_config
//! - 任务操作（Phase 2，Task 10）：add_uri / add_torrent / pause_task / resume_task /
//!   remove_task / change_option / change_global_option / get_global_stat / get_engine_info /
//!   get_tasks / get_task_detail / get_peers / save_session / purge
//!
//! 任务操作命令已接入 motrix-core 真实引擎（TaskManager）：添加 / 暂停 / 恢复 / 删除
//! 均由 KGet 引擎实际执行；add_torrent（BT）与 get_peers 属 Phase 3，返回明确占位。

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

    Ok(json!({
        "user": user,
        "system": system,
        "context": context,
    }))
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

/// 添加种子任务：BT 下载支持属 Phase 3（librqbit），经 TaskManager 返回明确占位错误
#[tauri::command]
pub fn add_torrent(
    state: State<'_, AppState>,
    torrent: String,
    options: Option<Value>,
) -> Result<String, String> {
    debug!("[Motrix] add_torrent 调用（BT 属 Phase 3，暂未支持）");
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
#[tauri::command]
pub fn get_task_detail(state: State<'_, AppState>, gid: String) -> Result<Value, String> {
    let repo = state
        .task_manager
        .repo
        .lock()
        .map_err(|e| format!("获取任务仓库锁失败: {e}"))?;
    Ok(repo.get(&gid).map(|t| t.to_aria2()).unwrap_or(Value::Null))
}

/// 获取任务 peers 列表（占位）：返回空数组（BT peers 支持属 Phase 3）
#[tauri::command]
pub fn get_peers(gid: String) -> Result<Value, String> {
    let _ = gid;
    debug!("[Motrix] get_peers 调用（BT 属 Phase 3，返回空列表）");
    Ok(json!([]))
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
