//! motrix-rpc：aria2 兼容 JSON-RPC 服务（对外接口）
//!
//! 提供 `127.0.0.1:{rpc-listen-port}/jsonrpc` 的 WS + HTTP POST 双通道，
//! 供 motrix-cli 远程模式与外部 aria2 兼容客户端（如 AriaNg）使用。
//! 前端 UI 不走此通道（操作走 Tauri command、状态走 engine:* 事件）。
//!
//! 模块划分：
//! - [server]：  axum WS + HTTP POST 双通道服务器（[JsonRpcServer]）
//! - [methods]： JSON-RPC 协议层（请求解析 / token 认证 / system.* 方法 / 响应包装）
//! - [notify]：  通知骨架（onDownloadStart 等广播，Phase 2 接入真实推送）
//! - client：    轻量 JSON-RPC 客户端（供 motrix-cli 复用，Phase 5 实现）

pub mod methods;
pub mod notify;
pub mod server;

use serde_json::Value;
use thiserror::Error;

/// 后端抽象：把 JSON-RPC 方法调用转发到具体引擎实现（Phase 2 由 motrix-core 提供实现）。
///
/// 协议层负责 token 认证、system.* 方法、结果包装；此处只关心"方法名 + 参数 → 结果"，
/// 因此本 crate 不依赖 motrix-core，仅依赖该 trait，天然解耦。
pub trait RpcBackend: Send + Sync {
    /// 调用一个引擎方法（params 已剥离认证 token）
    fn call(&self, method: &str, params: &[Value]) -> Result<Value, RpcError>;
}

/// JSON-RPC 错误类型。
///
/// - 认证失败（aria2 约定）：code = 1, message = "Authorization failed"
/// - 协议错误码：-32600 无效请求、-32601 方法不存在、-32602 无效参数、-32000 服务器内部错误
#[derive(Debug, Clone, Error)]
pub enum RpcError {
    /// 认证失败（aria2 约定的 code=1）
    #[error("Authorization failed")]
    AuthorizationFailed,
    /// JSON 解析失败（-32700）
    #[error("Parse error: {0}")]
    Parse(String),
    /// 无效请求（-32600）：请求结构不合法 / jsonrpc 版本错误
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    /// 方法不存在（-32601）
    #[error("Method not found: {0}")]
    MethodNotFound(String),
    /// 无效参数（-32602）
    #[error("Invalid params: {0}")]
    InvalidParams(String),
    /// 服务器内部错误（-32000）
    #[error("Internal error: {0}")]
    Internal(String),
    /// 其他业务错误（携带自定义 code/message，例如 aria2 任务错误码）
    #[error("{message}")]
    Other { code: i64, message: String },
}

impl RpcError {
    /// 转换为 JSON-RPC 错误对象 `{"code": <int>, "message": <string>}`
    pub fn to_jsonrpc(&self) -> Value {
        let (code, message) = match self {
            // aria2 认证失败约定：code=1
            RpcError::AuthorizationFailed => (1_i64, "Authorization failed".to_string()),
            RpcError::Parse(m) => (-32700, m.clone()),
            RpcError::InvalidRequest(m) => (-32600, m.clone()),
            RpcError::MethodNotFound(m) => (-32601, m.clone()),
            RpcError::InvalidParams(m) => (-32602, m.clone()),
            RpcError::Internal(m) => (-32000, m.clone()),
            RpcError::Other { code, message } => (*code, message.clone()),
        };
        serde_json::json!({ "code": code, "message": message })
    }
}

// 便捷导出：让外部以 `motrix_rpc::JsonRpcServer` 使用服务端
pub use server::JsonRpcServer;
