//! 轻量 JSON-RPC 客户端：同步 HTTP POST 调用（供 motrix-cli 远程模式复用）。
//!
//! 设计取舍：
//! - **HTTP 栈选型**：采用 `reqwest` blocking 而非 `ureq`。理由：workspace 内
//!   motrix-core 已通过 KGet 解析了 reqwest 0.13（blocking + rustls，见
//!   motrix-core/Cargo.toml 注释），此处复用同一依赖树与 rustls TLS 栈，不会
//!   为 CLI 再引入一套 HTTP 实现（ureq 虽更轻，但需新增 rustls 依赖并编译，收益不大）；
//! - **同步（blocking）调用**：CLI 命令一次请求即得结果，无需异步运行时，代码更简单
//!   （daemon 模式由后续任务引入 tokio）；
//! - **认证**：secret 非空时在 params 首元素注入 `token:{secret}`，与服务端
//!   [crate::methods] 的认证约定一致（与 aria2 兼容，code=1 表示认证失败）。

use serde_json::{json, Value};
use thiserror::Error;

/// 客户端错误：连接失败 / HTTP 非 2xx / 响应不合法 / 服务端返回 error 对象
#[derive(Debug, Error)]
pub enum ClientError {
    /// 无法建立连接或请求发送失败（服务未启动、地址错误等）
    #[error("无法连接到 RPC 服务: {0}")]
    Connection(String),
    /// HTTP 状态码非 2xx
    #[error("RPC 服务返回 HTTP 状态码 {0}")]
    HttpStatus(u16),
    /// 响应体不是合法 JSON
    #[error("RPC 响应解析失败: {0}")]
    InvalidResponse(String),
    /// 响应既无 result 也无 error（不符合 JSON-RPC 契约）
    #[error("RPC 响应缺少 result 且缺少 error")]
    MissingResult,
    /// 服务端返回 error 对象（透传 code 与 message，如 "Authorization failed"）
    #[error("{message}")]
    ServerError { code: i64, message: String },
}

/// 轻量 JSON-RPC 客户端（同步 HTTP POST，复用连接池）
pub struct JsonRpcClient {
    /// 服务端 URL，如 `http://127.0.0.1:16800/jsonrpc`
    url: String,
    /// 认证密钥（空串表示不传 token，即免认证模式）
    secret: String,
    /// reqwest blocking 客户端（内部自带线程池与连接复用）
    http: reqwest::blocking::Client,
}

impl JsonRpcClient {
    /// 创建客户端：`secret` 非空时自动在每次请求的 params 首元素注入 token 认证参数
    pub fn new(url: String, secret: String) -> Self {
        Self {
            url,
            secret,
            http: reqwest::blocking::Client::new(),
        }
    }

    /// 调用一个 JSON-RPC 方法并返回 result。
    ///
    /// 请求形如 `{"jsonrpc":"2.0","id":1,"method":...,"params":["token:{secret}", ...]}`，
    /// 与服务端 [crate::methods] 的解析契约一致（id 固定为 1，CLI 为单请求模式）。
    pub fn call(&self, method: &str, params: Vec<Value>) -> Result<Value, ClientError> {
        // 认证：secret 非空时首参数注入 token:{secret}，其余参数原样透传
        let mut full_params = Vec::with_capacity(params.len() + 1);
        if !self.secret.is_empty() {
            full_params.push(json!(format!("token:{}", self.secret)));
        }
        full_params.extend(params);

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": full_params,
        });

        // 发送 POST 请求（连接失败 / 超时等统一归为 Connection 错误）
        let resp = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .map_err(|e| ClientError::Connection(e.to_string()))?;

        // HTTP 非 2xx：透传状态码（正常服务端恒返回 200）
        if !resp.status().is_success() {
            return Err(ClientError::HttpStatus(resp.status().as_u16()));
        }

        // 响应体解析为 JSON
        let text = resp
            .text()
            .map_err(|e| ClientError::InvalidResponse(e.to_string()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| ClientError::InvalidResponse(e.to_string()))?;

        // error 字段 → ServerError（message 透传，如 "Authorization failed" / "Method not found"）
        if let Some(err) = value.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string();
            return Err(ClientError::ServerError { code, message });
        }

        // result 字段 → 成功返回
        value.get("result").cloned().ok_or(ClientError::MissingResult)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{JsonRpcServer, RpcBackend, RpcError};

    /// 测试用假后端（与 tests/jsonrpc.rs 的最小方法集一致）
    struct FakeBackend;

    impl RpcBackend for FakeBackend {
        fn call(&self, method: &str, _params: &[Value]) -> Result<Value, RpcError> {
            match method {
                // getVersion：固定版本信息
                "aria2.getVersion" => Ok(json!({ "version": "1.36.0" })),
                // 未注册方法：由后端返回 MethodNotFound（-32601）
                _ => Err(RpcError::MethodNotFound(format!("方法未实现: {method}"))),
            }
        }
    }

    /// 启动本地真实 axum 测试服务，返回 `http://127.0.0.1:{port}/jsonrpc` 与运行时。
    ///
    /// 说明：测试用普通 `#[test]` 而非 `#[tokio::test]` —— reqwest blocking 客户端的
    /// drop 会关闭内部线程池，在 tokio 异步上下文（如 `#[tokio::test]`）中执行会触发
    /// "Cannot drop a runtime in a context where blocking is not allowed" panic；
    /// 故这里用 multi-thread 运行时在后台驱动 axum 服务，测试主体留在普通线程。
    fn start_test_server(secret: &str) -> (String, tokio::runtime::Runtime) {
        // multi-thread 运行时：block_on 返回后后台 worker 仍持续驱动 spawn 的服务器任务
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = rt.block_on(async {
            let app = JsonRpcServer::router(Arc::new(FakeBackend), secret.to_string());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            format!("http://{addr}/jsonrpc")
        });
        (url, rt)
    }

    #[test]
    fn 正确secret调用成功并返回result() {
        let (url, rt) = start_test_server("secret");
        let client = JsonRpcClient::new(url, "secret".to_string());
        let result = client.call("aria2.getVersion", vec![]).unwrap();
        // 契约：getVersion 返回 version 字段（字符串）
        assert_eq!(result["version"], "1.36.0");
        drop(rt);
    }

    #[test]
    fn 错误secret返回认证失败() {
        let (url, rt) = start_test_server("secret");
        let client = JsonRpcClient::new(url, "wrong".to_string());
        let err = client.call("aria2.getVersion", vec![]).unwrap_err();
        // 认证失败：code=1、message="Authorization failed"（aria2 约定）
        match err {
            ClientError::ServerError { code, message } => {
                assert_eq!(code, 1);
                assert_eq!(message, "Authorization failed");
            }
            other => panic!("期望认证失败错误，实际: {other}"),
        }
        drop(rt);
    }

    #[test]
    fn 空secret免认证() {
        let (url, rt) = start_test_server("");
        let client = JsonRpcClient::new(url, String::new());
        // 不传 token 也能成功（服务端空 secret 免认证）
        let result = client.call("aria2.getVersion", vec![]).unwrap();
        assert_eq!(result["version"], "1.36.0");
        drop(rt);
    }

    #[test]
    fn 方法未实现返回服务端错误() {
        let (url, rt) = start_test_server("secret");
        let client = JsonRpcClient::new(url, "secret".to_string());
        let err = client.call("aria2.noSuchMethod", vec![]).unwrap_err();
        // 方法不存在：code=-32601，message 透传
        match err {
            ClientError::ServerError { code, message } => {
                assert_eq!(code, -32601);
                assert!(message.contains("未实现"));
            }
            other => panic!("期望方法未实现错误，实际: {other}"),
        }
        drop(rt);
    }

    #[test]
    fn 连接失败返回连接错误() {
        // 端口 1 无服务监听，连接必然失败 → Connection 错误（友好提示"服务未启动"）
        let client = JsonRpcClient::new("http://127.0.0.1:1/jsonrpc".to_string(), String::new());
        let err = client.call("aria2.getVersion", vec![]).unwrap_err();
        assert!(matches!(err, ClientError::Connection(_)));
    }
}
