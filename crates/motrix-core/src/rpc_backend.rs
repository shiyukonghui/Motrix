//! CoreRpcBackend：把任务管理器 / 配置管理器适配为 motrix-rpc 的 RpcBackend
//!
//! 定位：为 motrix-rpc 的 `JsonRpcServer`（WS + HTTP POST 双通道，对外接口）提供
//! aria2 兼容方法实现（参考 MIGRATION-TAURI.md 5.6 / 8.1 节）。
//! **本模块是 GUI 与 CLI daemon 共享的 aria2 兼容 RPC 后端**（Phase 5 迁移，
//! 见 MIGRATION-TAURI.md 4.1 crate 结构）：motrix-core 增加对 motrix-rpc 的
//! 依赖（仅用于 `RpcBackend` trait 与 `RpcError`，无循环依赖——motrix-rpc 不
//! 依赖 motrix-core），src-tauri 与 motrix-cli daemon 各自用它构造 `Arc<dyn RpcBackend>`，
//! 前端 UI 不走此通道（操作走 Tauri command、状态走 engine:* 事件）。
//!
//! Phase 2 实现（Task 10：任务操作接通真实引擎）：
//! - 查询：getVersion / getGlobalStat / tellActive / tellWaiting / tellStopped / tellStatus
//! - 控制：pause / forcePause / unpause（单任务）、pauseAll / unpauseAll（批量）、
//!   remove / forceRemove、addUri（经 TaskManager 启动 KGet 真实引擎）
//! - 选项：getOption / getGlobalOption（system 配置基础子集，字段为字符串）、
//!   changeGlobalOption / changeOption（经 TaskManager 更新并持久化）
//! - 会话：saveSession（占位 "OK"）、purgeDownloadResult（清空 removed 历史）
//!
//! Phase 3 补充（BT 支持，见 MIGRATION-TAURI.md 5.5 / 5.6）：
//! - aria2.addTorrent（params[0]=base64 .torrent，params[1]=options）→ 经 TaskManager
//!   创建 BT 任务并返回 gid；
//! - aria2.getPeers（params[0]=gid，params[1]=limit 可选）→ aria2 兼容 peer JSON 数组
//!   （librqbit 8.1.1 未暴露 per-peer 明细，当前返回空数组，契约保留）；
//! - motrix.saveUserConfig / motrix.getUserConfig（Phase 5 用户配置读写，实现在本文件）：
//!   经 ConfigManager 写 / 读 user.json 的 user 分区。
//!
//! 说明：`RpcError` 无 NotFound 变体，任务不存在统一返回
//! `RpcError::Other { code: 1, message: "gid 不存在" }`（aria2 惯例错误码 1）。

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tracing::debug;

use crate::{ConfigManager, Task, TaskManager, TaskStatus};
use motrix_rpc::{RpcBackend, RpcError};

/// aria2 兼容 RPC 后端：持有任务管理器与配置管理器（Arc<Mutex> 以便跨任务共享）
pub struct CoreRpcBackend {
    /// 任务管理器（任务编排 + 任务仓库，与 Tauri AppState / daemon / 广播循环共享同一份）
    pub task_manager: Arc<TaskManager>,
    /// 配置管理器（读取 system.json 的 dir / 并发等全局选项）
    pub config_manager: Arc<Mutex<ConfigManager>>,
}

impl CoreRpcBackend {
    /// 构造 aria2 兼容后端（从共享的任务管理器 / 配置管理器接入，GUI 与 CLI daemon 通用）
    pub fn new(
        task_manager: Arc<TaskManager>,
        config_manager: Arc<Mutex<ConfigManager>>,
    ) -> Self {
        Self {
            task_manager,
            config_manager,
        }
    }

    /// 读取 system 配置的全局选项基础子集（getOption / getGlobalOption 共用）
    ///
    /// 返回 aria2 全局选项的常见键（kebab-case），数值一律转字符串（aria2 惯例）；
    /// 键名与 `systemKeys`（src/shared/configKeys.js）保持一致。
    fn engine_options(&self) -> Result<Value, RpcError> {
        let config_manager = self
            .config_manager
            .lock()
            .map_err(|e| RpcError::Internal(format!("获取配置管理器锁失败: {e}")))?;
        let system = config_manager.system_config();
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
        }))
    }

    /// 收集满足指定状态的任务 gid 列表（批量暂停 / 恢复用）
    fn collect_gids(&self, predicate: impl Fn(TaskStatus) -> bool) -> Result<Vec<String>, RpcError> {
        let repo = self
            .task_manager
            .repo
            .lock()
            .map_err(|e| RpcError::Internal(format!("获取任务仓库锁失败: {e}")))?;
        Ok(repo
            .all()
            .iter()
            .filter(|t| predicate(t.status))
            .map(|t| t.gid.clone())
            .collect())
    }
}

/// 任务不存在错误（aria2 惯例：gid 不存在/无效返回错误码 1）
fn gid_not_found(gid: &str) -> RpcError {
    RpcError::Other {
        code: 1,
        message: format!("gid 不存在: {gid}"),
    }
}

/// 按 offset / num 截取任务列表（aria2.tellWaiting / tellStopped 的 params 语义）
///
/// - params[0]：offset（默认 0）
/// - params[1]：num（默认 50）
fn paginate_tasks(tasks: &[Task], params: &[Value]) -> Value {
    let offset = params
        .first()
        .and_then(|p| p.as_u64())
        .unwrap_or(0) as usize;
    let num = params
        .get(1)
        .and_then(|p| p.as_u64())
        .unwrap_or(50) as usize;
    json!(
        tasks
            .iter()
            .skip(offset)
            .take(num)
            .map(|t| t.to_aria2())
            .collect::<Vec<Value>>()
    )
}

impl RpcBackend for CoreRpcBackend {
    /// 方法分发：params 已由协议层剥离认证 token
    fn call(&self, method: &str, params: &[Value]) -> Result<Value, RpcError> {
        debug!("[Motrix] JSON-RPC 调用: {method}, params={params:?}");
        match method {
            // ---------------- 版本 / 统计 ----------------
            "aria2.getVersion" => Ok(json!({
                "version": crate::ENGINE_VERSION,
                "enabledFeatures": [],
            })),
            "aria2.getGlobalStat" => Ok(self.task_manager.global_stat().to_aria2_json()),

            // ---------------- 任务列表查询 ----------------
            // tellActive：返回全部运行中任务（无分页参数）
            "aria2.tellActive" => {
                let repo = self
                    .task_manager
                    .repo
                    .lock()
                    .map_err(|e| RpcError::Internal(format!("获取任务仓库锁失败: {e}")))?;
                let tasks: Vec<Value> = repo.active().iter().map(|t| t.to_aria2()).collect();
                Ok(json!(tasks))
            }
            // tellWaiting / tellStopped：支持 offset / num 分页
            "aria2.tellWaiting" => {
                let repo = self
                    .task_manager
                    .repo
                    .lock()
                    .map_err(|e| RpcError::Internal(format!("获取任务仓库锁失败: {e}")))?;
                Ok(paginate_tasks(&repo.waiting(), params))
            }
            "aria2.tellStopped" => {
                let repo = self
                    .task_manager
                    .repo
                    .lock()
                    .map_err(|e| RpcError::Internal(format!("获取任务仓库锁失败: {e}")))?;
                Ok(paginate_tasks(&repo.stopped(), params))
            }
            // tellStatus：params[0] = gid；不存在返回错误
            "aria2.tellStatus" => {
                let gid = params
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        RpcError::InvalidParams("aria2.tellStatus 需要 gid 参数".into())
                    })?;
                let repo = self
                    .task_manager
                    .repo
                    .lock()
                    .map_err(|e| RpcError::Internal(format!("获取任务仓库锁失败: {e}")))?;
                repo.get(gid)
                    .map(|t| t.to_aria2())
                    .ok_or_else(|| gid_not_found(gid))
            }

            // ---------------- 暂停 / 恢复 ----------------
            // 单任务暂停 / 强制暂停（KGet abort 本身就是强停，行为一致）
            "aria2.pause" | "aria2.forcePause" => {
                let gid = params
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        RpcError::InvalidParams(format!("{method} 需要 gid 参数"))
                    })?;
                self.task_manager
                    .pause(gid)
                    .map(Value::String)
                    .map_err(|e| RpcError::Other { code: 1, message: e })
            }
            // 恢复（aria2.unpause）：经 TaskManager 重新 spawn（Range 续传）
            "aria2.unpause" => {
                let gid = params
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| RpcError::InvalidParams("aria2.unpause 需要 gid 参数".into()))?;
                self.task_manager
                    .resume(gid)
                    .map(Value::String)
                    .map_err(|e| RpcError::Other { code: 1, message: e })
            }
            // 批量暂停：所有 active / waiting 任务 → paused
            "aria2.pauseAll" | "aria2.forcePauseAll" => {
                let gids = self.collect_gids(|s| matches!(s, TaskStatus::Active | TaskStatus::Waiting))?;
                for gid in &gids {
                    let _ = self.task_manager.pause(gid);
                }
                Ok(json!("OK"))
            }
            // 批量恢复：所有 paused 任务 → active
            "aria2.unpauseAll" => {
                let gids = self.collect_gids(|s| s == TaskStatus::Paused)?;
                for gid in &gids {
                    let _ = self.task_manager.resume(gid);
                }
                Ok(json!("OK"))
            }

            // ---------------- 删除 / 添加 ----------------
            // 移除任务（经 TaskManager：abort 引擎 + 移除仓库记录）
            "aria2.remove" | "aria2.forceRemove" => {
                let gid = params
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        RpcError::InvalidParams(format!("{method} 需要 gid 参数"))
                    })?;
                // aria2 语义：remove 返回被移除任务的 gid
                self.task_manager
                    .remove(gid)
                    .map(|_| json!(gid))
                    .map_err(|e| RpcError::Other { code: 1, message: e })
            }
            // 添加 URL 任务（真实引擎）：params[0] = uris 数组，params[1] = options（可空）
            "aria2.addUri" => {
                let uris: Vec<String> = params
                    .first()
                    .and_then(|p| p.as_array())
                    .ok_or_else(|| {
                        RpcError::InvalidParams("aria2.addUri 需要一个 URL 数组作为第一个参数".into())
                    })?
                    .iter()
                    .filter_map(|u| u.as_str().map(str::to_string))
                    .collect();
                let options = params.get(1).unwrap_or(&Value::Null);
                // add_uri 返回 gid 列表，转为 JSON 字符串数组（aria2 惯例）
                self.task_manager
                    .add_uri(&uris, options)
                    .map(|gids| Value::Array(gids.into_iter().map(Value::String).collect()))
                    .map_err(|e| RpcError::Other { code: 1, message: e })
            }
            // 添加 BT 种子任务（aria2.addTorrent）：params[0] = base64 编码的 .torrent 内容，
            // params[1] = options（可空）。经 TaskManager 创建 BT 任务并返回 gid（字符串）。
            "aria2.addTorrent" => {
                let torrent = params
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        RpcError::InvalidParams(
                            "aria2.addTorrent 需要 base64 编码的种子内容作为第一个参数".into(),
                        )
                    })?;
                let options = params.get(1).unwrap_or(&Value::Null);
                self.task_manager
                    .add_torrent(torrent, options)
                    .map(Value::String)
                    .map_err(|e| RpcError::Other { code: 1, message: e })
            }
            // 获取任务 peers（aria2.getPeers）：params[0] = gid，
            // params[1] = limit（可选，默认 100，分页硬约束）。
            // 返回 aria2 兼容 peer JSON 数组（数值为字符串）。
            // 注：librqbit 8.1.1 未暴露 per-peer 明细，当前返回空数组（契约保留）。
            "aria2.getPeers" => {
                let gid = params
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        RpcError::InvalidParams("aria2.getPeers 需要 gid 参数".into())
                    })?;
                let limit = params.get(1).and_then(|p| p.as_u64()).unwrap_or(100) as usize;
                let peers = self.task_manager.get_peers(gid, limit);
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
            // addMetalink：占位错误（前端无触发入口，见 MIGRATION-TAURI.md 5.6）
            "aria2.addMetalink" => Err(RpcError::Other {
                code: 1,
                message: "aria2.addMetalink 尚未实现（Phase 2/3 提供 Metalink 支持）".into(),
            }),

            // ---------------- 选项 / 会话 ----------------
            // getOption / getGlobalOption：返回 system 配置基础子集
            "aria2.getOption" | "aria2.getGlobalOption" => self.engine_options(),
            // changeGlobalOption：经 TaskManager 更新内存 + 持久化 system.json
            "aria2.changeGlobalOption" => {
                let options = params
                    .first()
                    .ok_or_else(|| {
                        RpcError::InvalidParams("aria2.changeGlobalOption 需要 options 参数".into())
                    })?;
                self.task_manager
                    .change_global_option(options)
                    .map(Value::String)
                    .map_err(|e| RpcError::Other { code: 1, message: e })
            }
            // changeOption：params[0] = gid，params[1] = options
            "aria2.changeOption" => {
                let gid = params
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        RpcError::InvalidParams("aria2.changeOption 需要 gid 参数".into())
                    })?;
                let options = params
                    .get(1)
                    .ok_or_else(|| {
                        RpcError::InvalidParams("aria2.changeOption 需要 options 参数".into())
                    })?;
                self.task_manager
                    .change_option(gid, options)
                    .map(Value::String)
                    .map_err(|e| RpcError::Other { code: 1, message: e })
            }
            // saveSession：占位 "OK"（真实会话持久化属 Phase 2/12，GUI / daemon 退出时
            // 由持有方直接调用 session::save_checkpoint，等价 aria2.saveSession 语义）
            "aria2.saveSession" => Ok(json!("OK")),
            // purgeDownloadResult：经 TaskManager 清空 removed 历史后返回 "OK"
            "aria2.purgeDownloadResult" => {
                self.task_manager.purge();
                Ok(json!("OK"))
            }

            // ---------------- Motrix 扩展方法（Phase 5 用户配置，实现在本文件） ----------------
            // motrix.saveUserConfig：params[0] = userKeys 对象（kebab-case 键），
            // 经 ConfigManager 仅写 user.json 的 user 分区（未建模键保留在 extra），返回 "OK"
            "motrix.saveUserConfig" => {
                let patch = params
                    .first()
                    .and_then(|p| p.as_object())
                    .ok_or_else(|| {
                        RpcError::InvalidParams(
                            "motrix.saveUserConfig 需要 userKeys 对象作为第一个参数".into(),
                        )
                    })?;
                let mut config_manager = self.config_manager.lock().map_err(|e| {
                    RpcError::Internal(format!("获取配置管理器锁失败: {e}"))
                })?;
                config_manager
                    .update_user_config(patch)
                    .map_err(|e| {
                        RpcError::Other {
                            code: 1,
                            message: format!("保存用户配置失败: {e}"),
                        }
                    })?;
                Ok(json!("OK"))
            }
            // motrix.getUserConfig：返回当前 user.json 内容（供 motrix-cli config get 使用）
            "motrix.getUserConfig" => {
                let config_manager = self.config_manager.lock().map_err(|e| {
                    RpcError::Internal(format!("获取配置管理器锁失败: {e}"))
                })?;
                Ok(config_manager.user_config_json())
            }

            // ---------------- 其余方法：明确报"未实现" ----------------
            other => Err(RpcError::MethodNotFound(format!("方法未实现: {other}"))),
        }
    }
}
