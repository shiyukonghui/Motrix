//! JSON-RPC 协议层：请求解析、token 认证、system.* 方法实现、结果统一包装。
//!
//! 职责边界：
//! - 解析请求文本（jsonrpc/2.0、id、method、params）→ [RpcRequest]
//! - 认证：params 第一个参数必须是 `token:{secret}`（secret 为空串时不要求认证）
//! - `system.multicall` / `system.listMethods` / `system.listNotifications` 在协议层实现
//! - 其余方法透传给 [crate::RpcBackend::call]，并对返回做统一 result / error 包装

use crate::{RpcBackend, RpcError};
use serde::Deserialize;
use serde_json::{json, Value};

/// JSON-RPC 请求体（宽松解析：jsonrpc 版本可选、params 缺省为空）
#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    /// 协议版本标识（应为 "2.0"）
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// 请求 id（可为数字/字符串/null；通知场景可缺省）
    pub id: Option<Value>,
    /// 方法名，如 `aria2.getVersion`
    pub method: String,
    /// 位置参数数组（认证时第一个参数为 `token:{secret}`）
    #[serde(default)]
    pub params: Vec<Value>,
}

/// `system.listMethods` 固定清单：aria2 标准方法 + motrix.* 扩展占位
pub const LIST_METHODS: &[&str] = &[
    // 任务操作
    "aria2.addUri",
    "aria2.addTorrent",
    "aria2.addMetalink",
    // 状态查询
    "aria2.tellStatus",
    "aria2.tellActive",
    "aria2.tellWaiting",
    "aria2.tellStopped",
    "aria2.getPeers",
    "aria2.getGlobalStat",
    "aria2.getVersion",
    // 暂停 / 恢复 / 删除
    "aria2.pause",
    "aria2.pauseAll",
    "aria2.forcePause",
    "aria2.forcePauseAll",
    "aria2.unpause",
    "aria2.unpauseAll",
    "aria2.remove",
    "aria2.forceRemove",
    // 选项与会话
    "aria2.changeOption",
    "aria2.changeGlobalOption",
    "aria2.getOption",
    "aria2.getGlobalOption",
    "aria2.saveSession",
    "aria2.purgeDownloadResult",
    "aria2.removeDownloadResult",
    // 系统方法
    "system.multicall",
    "system.listNotifications",
    "system.listMethods",
    // motrix.* 扩展（Phase 5 motrix-cli 使用，占位声明）
    "motrix.saveUserConfig",
    "motrix.getUserConfig",
];

/// `system.listNotifications` 固定清单（对应原 aria2 通知事件）
pub const LIST_NOTIFICATIONS: &[&str] = &[
    "aria2.onDownloadStart",
    "aria2.onDownloadPause",
    "aria2.onDownloadStop",
    "aria2.onDownloadComplete",
    "aria2.onDownloadError",
    "aria2.onBtDownloadComplete",
];

/// 处理一段请求文本（HTTP POST body 或 WS 消息），返回响应文本。
///
/// JSON 解析失败时返回 JSON-RPC `Parse error`（-32700）响应。
pub fn process_request_str(body: &str, backend: &dyn RpcBackend, secret: &str) -> String {
    match serde_json::from_str::<RpcRequest>(body) {
        Ok(req) => handle_request(&req, backend, secret).to_string(),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": RpcError::Parse(e.to_string()).to_jsonrpc()
        })
        .to_string(),
    }
}

/// 核心分发：校验协议版本 → token 认证 → system.* / backend.call → 统一包装。
pub(crate) fn handle_request(req: &RpcRequest, backend: &dyn RpcBackend, secret: &str) -> Value {
    // 1) 协议版本校验：jsonrpc 字段存在且不为 "2.0" 时视为无效请求（-32600）
    if let Some(v) = &req.jsonrpc {
        if v != "2.0" {
            return error_response(req, RpcError::InvalidRequest(format!("不支持的 jsonrpc 版本: {v}")));
        }
    }

    // 2) token 认证：params 第一个参数必须是 "token:{secret}"（secret 为空则免认证）
    let (auth_ok, params) = extract_params(&req.params, secret);
    if !auth_ok {
        return error_response(req, RpcError::AuthorizationFailed);
    }

    // 3) 方法分发：system.* 在协议层实现，其余透传后端
    match req.method.as_str() {
        "system.multicall" => handle_multicall(req, backend, &params, secret),
        "system.listMethods" => result_response(req, json!(LIST_METHODS)),
        "system.listNotifications" => result_response(req, json!(LIST_NOTIFICATIONS)),
        _ => match backend.call(&req.method, &params) {
            Ok(result) => result_response(req, result),
            // 方法不存在等错误由后端返回，此处统一包装为 error 对象
            Err(e) => error_response(req, e),
        },
    }
}

/// 校验 token 并剥离认证参数。
///
/// 返回 `(是否通过认证, 去除 token 后的参数列表)`：
/// - secret 为空串：不要求认证，参数原样传递；
/// - 否则要求 params[0] 精确等于 `token:{secret}`。
fn extract_params(params: &[Value], secret: &str) -> (bool, Vec<Value>) {
    if secret.is_empty() {
        // 免认证模式：全部参数透传
        return (true, params.to_vec());
    }
    let expect = format!("token:{secret}");
    match params.first() {
        Some(Value::String(s)) if s == &expect => (true, params[1..].to_vec()),
        _ => (false, Vec::new()),
    }
}

/// `system.multicall`：批量调用。
///
/// 参数为 `[ [method, params...], ... ]` 数组（aria2 惯例内层每项也带 token）。
/// 按序执行，返回结果数组；对应项失败时在数组同位置放入 error 对象。
fn handle_multicall(req: &RpcRequest, backend: &dyn RpcBackend, params: &[Value], secret: &str) -> Value {
    // 调用列表：取 params 中第一个数组参数（已剥离外层 token）
    let Some(calls) = params.iter().find_map(|p| p.as_array()) else {
        return error_response(
            req,
            RpcError::InvalidParams("system.multicall 需要一个 [method, params] 数组作为参数".into()),
        );
    };

    let mut results: Vec<Value> = Vec::with_capacity(calls.len());
    for call in calls {
        // 每个元素形如 ["aria2.getVersion", []] 或 ["aria2.addUri", "token:x", "url", {...}]
        let Some(method) = call.as_array().and_then(|c| c.first()).and_then(|m| m.as_str()) else {
            results.push(RpcError::InvalidParams("multicall 元素需为 [method, params...] 数组".into()).to_jsonrpc());
            continue;
        };
        let raw_params: Vec<Value> = if let Some(Value::Array(a)) = call.as_array().and_then(|c| c.get(1)) {
            // 标准形式：第二元素为参数数组
            a.clone()
        } else {
            // 兼容扁平形式：["method", "arg1", "arg2", ...]
            call.as_array().map(|c| c.iter().skip(1).cloned().collect()).unwrap_or_default()
        };
        // 内层调用同样剥离 token（aria2 惯例：multicall 每项都带 token）
        let (auth_ok, call_params) = extract_params(&raw_params, secret);
        if !auth_ok {
            results.push(RpcError::AuthorizationFailed.to_jsonrpc());
            continue;
        }
        match backend.call(method, &call_params) {
            Ok(r) => results.push(r),
            Err(e) => results.push(e.to_jsonrpc()),
        }
    }
    result_response(req, Value::Array(results))
}

/// 统一包装成功响应：`{"jsonrpc":"2.0","id":...,"result":...}`
fn result_response(req: &RpcRequest, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": req.id, "result": result })
}

/// 统一包装错误响应：`{"jsonrpc":"2.0","id":...,"error":{code,message}}`
fn error_response(req: &RpcRequest, err: RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": req.id, "error": err.to_jsonrpc() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用假后端：getVersion 返回固定版本信息
    struct StubBackend;

    impl RpcBackend for StubBackend {
        fn call(&self, method: &str, _params: &[Value]) -> Result<Value, RpcError> {
            match method {
                "aria2.getVersion" => Ok(json!({ "version": "1.36.0" })),
                _ => Err(RpcError::MethodNotFound(format!("方法未实现: {method}"))),
            }
        }
    }

    #[test]
    fn 解析请求与认证剥离() {
        let req = serde_json::from_str::<RpcRequest>(
            r#"{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":["token:secret"]}"#,
        )
        .unwrap();
        assert_eq!(req.method, "aria2.getVersion");
        // 认证通过且剥离 token 后参数为空
        let (ok, params) = extract_params(&req.params, "secret");
        assert!(ok);
        assert!(params.is_empty());
    }

    #[test]
    fn 认证失败返回code1() {
        let req: RpcRequest = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":["token:wrong"]}"#,
        )
        .unwrap();
        let resp = handle_request(&req, &StubBackend, "secret");
        assert_eq!(resp["error"]["code"], 1);
        assert_eq!(resp["error"]["message"], "Authorization failed");
    }

    #[test]
    fn 空secret免认证() {
        let req: RpcRequest = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":[]}"#,
        )
        .unwrap();
        let resp = handle_request(&req, &StubBackend, "");
        assert_eq!(resp["result"]["version"], "1.36.0");
    }

    #[test]
    fn multicall返回结果数组且失败项为错误对象() {
        // aria2 惯例：multicall 内层每个调用也携带 token
        let req: RpcRequest = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":2,"method":"system.multicall",
                "params":["token:secret",[["aria2.getVersion",["token:secret"]],["aria2.noSuch",["token:secret"]]]]}"#,
        )
        .unwrap();
        let resp = handle_request(&req, &StubBackend, "secret");
        let arr = resp["result"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["version"], "1.36.0");
        assert_eq!(arr[1]["code"], -32601);
    }
}
