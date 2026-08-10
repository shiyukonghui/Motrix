//! motrix-rpc 集成测试：HTTP POST（oneshot 直调 Router）与真实 WS 服务器双通道。
//!
//! FakeBackend 实现 [RpcBackend]：getVersion 返回固定版本、tellActive 返回空列表、
//! getGlobalStat 返回全 0 统计（数值字段为字符串，aria2 惯例）。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use motrix_rpc::{JsonRpcServer, RpcBackend, RpcError};
use serde_json::{json, Value};
use tower::ServiceExt;

/// 测试用假后端：模拟 motrix-core 引擎的最小方法集
struct FakeBackend;

impl RpcBackend for FakeBackend {
    fn call(&self, method: &str, _params: &[Value]) -> Result<Value, RpcError> {
        match method {
            // getVersion：固定版本信息 + enabledFeatures
            "aria2.getVersion" => Ok(json!({
                "version": "1.36.0",
                "enabledFeatures": [
                    "Async DNS", "BitTorrent", "HTTPS",
                    "Message Digest", "Metalink", "XML-RPC", "SFTP"
                ]
            })),
            // tellActive：Phase 1 占位返回空列表
            "aria2.tellActive" => Ok(Value::Array(vec![])),
            // getGlobalStat：全 0 统计（数值字段按 aria2 惯例输出字符串）
            "aria2.getGlobalStat" => Ok(json!({
                "downloadSpeed": "0",
                "uploadSpeed": "0",
                "numActive": "0",
                "numWaiting": "0",
                "numStopped": "0",
                "numStoppedTotal": "0"
            })),
            // addUri：返回占位 gid
            "aria2.addUri" => Ok(json!({ "gid": "2089b05ecca3d829" })),
            // 未注册方法：由后端返回 MethodNotFound（-32601）
            _ => Err(RpcError::MethodNotFound(format!("方法未实现: {method}"))),
        }
    }
}

/// 便捷：构造 JSON-RPC 请求文本
fn rpc_body(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

/// 便捷：POST /jsonrpc 并取回解析后的 JSON 响应
async fn post(app: axum::Router, body: String) -> Value {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jsonrpc")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// 带 secret 的 Router（测试用）
fn app_with_secret() -> axum::Router {
    JsonRpcServer::router(Arc::new(FakeBackend), "secret".to_string())
}

// ---------------------------------------------------------------------------
// HTTP POST 契约测试
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_post_get_version_返回result含版本字符串() {
    let app = app_with_secret();
    let v = post(app, rpc_body(1, "aria2.getVersion", json!(["token:secret"]))).await;
    // 响应结构：jsonrpc/2.0 + id 回显 + result
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], json!(1));
    assert_eq!(v["result"]["version"], "1.36.0");
    assert!(v["result"]["version"].is_string());
    // enabledFeatures 为数组
    assert!(v["result"]["enabledFeatures"].is_array());
}

#[tokio::test]
async fn http_post_无token返回认证失败() {
    let app = app_with_secret();
    let v = post(app, rpc_body(1, "aria2.getVersion", json!([]))).await;
    assert_eq!(v["error"]["code"], json!(1));
    assert_eq!(v["error"]["message"], "Authorization failed");
}

#[tokio::test]
async fn http_post_错误token返回认证失败() {
    let app = app_with_secret();
    let v = post(app, rpc_body(1, "aria2.getVersion", json!(["token:wrong"]))).await;
    assert_eq!(v["error"]["code"], json!(1));
    assert_eq!(v["error"]["message"], "Authorization failed");
}

#[tokio::test]
async fn http_post_tell_active_返回空列表() {
    let app = app_with_secret();
    let v = post(app, rpc_body(2, "aria2.tellActive", json!(["token:secret"]))).await;
    assert_eq!(v["result"], json!([]));
}

#[tokio::test]
async fn http_post_system_multicall_批量调用两个方法() {
    let app = app_with_secret();
    // aria2 惯例：multicall 内层每个调用也携带 token
    let params = json!([
        "token:secret",
        [["aria2.getVersion", ["token:secret"]], ["aria2.getGlobalStat", ["token:secret"]]]
    ]);
    let v = post(app, rpc_body(3, "system.multicall", params)).await;
    let arr = v["result"].as_array().expect("multicall 结果应为数组");
    assert_eq!(arr.len(), 2);
    // 第一个结果：getVersion
    assert_eq!(arr[0]["version"], "1.36.0");
    // 第二个结果：getGlobalStat
    assert_eq!(arr[1]["downloadSpeed"], "0");
}

#[tokio::test]
async fn http_post_system_multicall_单项失败返回错误对象() {
    let app = app_with_secret();
    let params = json!([
        "token:secret",
        [["aria2.getVersion", ["token:secret"]], ["aria2.noSuchMethod", ["token:secret"]]]
    ]);
    let v = post(app, rpc_body(4, "system.multicall", params)).await;
    let arr = v["result"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // 成功项正常返回
    assert_eq!(arr[0]["version"], "1.36.0");
    // 失败项在数组同位置放入 error 对象（-32601 方法不存在）
    assert_eq!(arr[1]["code"], json!(-32601));
    assert!(arr[1]["message"].is_string());
}

#[tokio::test]
async fn http_post_未注册方法返回方法不存在错误() {
    let app = app_with_secret();
    let v = post(app, rpc_body(5, "aria2.unknownMethod", json!(["token:secret"]))).await;
    // 方法是否存在由后端判定（契约：method 未实现 → 由 backend 返回）
    assert_eq!(v["error"]["code"], json!(-32601));
}

#[tokio::test]
async fn http_post_空secret不要求认证() {
    let app = JsonRpcServer::router(Arc::new(FakeBackend), String::new());
    // 不传 token 也能成功调用
    let v = post(app, rpc_body(1, "aria2.getVersion", json!([]))).await;
    assert_eq!(v["result"]["version"], "1.36.0");
}

#[tokio::test]
async fn http_post_get_global_stat_数值字段为字符串() {
    let app = app_with_secret();
    let v = post(app, rpc_body(6, "aria2.getGlobalStat", json!(["token:secret"]))).await;
    // aria2 惯例：所有数值字段以字符串输出（前端 Number() 转换处不变）
    assert_eq!(v["result"]["downloadSpeed"], "0");
    assert_eq!(v["result"]["uploadSpeed"], "0");
    assert_eq!(v["result"]["numActive"], "0");
    assert_eq!(v["result"]["numWaiting"], "0");
    assert_eq!(v["result"]["numStopped"], "0");
    assert!(v["result"]["downloadSpeed"].is_string());
}

#[tokio::test]
async fn http_post_system_list_methods_返回方法清单() {
    let app = app_with_secret();
    let v = post(app, rpc_body(7, "system.listMethods", json!(["token:secret"]))).await;
    let arr = v["result"].as_array().unwrap();
    // 包含 aria2 方法与 motrix.* 扩展占位
    assert!(arr.iter().any(|m| m == "aria2.getVersion"));
    assert!(arr.iter().any(|m| m == "system.multicall"));
    assert!(arr.iter().any(|m| m == "motrix.saveUserConfig"));
}

// ---------------------------------------------------------------------------
// WS 契约测试：绑定 127.0.0.1:0 启动真实服务器
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_get_version_真实服务器返回正确响应() {
    // 绑定 127.0.0.1:0（系统分配端口）并启动真实 axum 服务
    let app = JsonRpcServer::router(Arc::new(FakeBackend), "secret".to_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // tokio-tungstenite 客户端连接 WS
    let url = format!("ws://{addr}/jsonrpc");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();

    // 发送 getVersion 请求（带 token）
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        rpc_body(1, "aria2.getVersion", json!(["token:secret"])).into(),
    ))
    .await
    .unwrap();

    // 断言收到正确响应
    let msg = ws.next().await.unwrap().expect("WS 消息读取失败");
    match msg {
        tokio_tungstenite::tungstenite::Message::Text(text) => {
            let v: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["jsonrpc"], "2.0");
            assert_eq!(v["id"], json!(1));
            assert_eq!(v["result"]["version"], "1.36.0");
        }
        other => panic!("期望文本消息，实际收到: {other:?}"),
    }
}

#[tokio::test]
async fn ws_错误token返回认证失败() {
    let app = JsonRpcServer::router(Arc::new(FakeBackend), "secret".to_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("ws://{addr}/jsonrpc");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();

    // 错误 token
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        rpc_body(1, "aria2.getVersion", json!(["token:bad"])).into(),
    ))
    .await
    .unwrap();

    let msg = ws.next().await.unwrap().expect("WS 消息读取失败");
    if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"]["code"], json!(1));
        assert_eq!(v["error"]["message"], "Authorization failed");
    } else {
        panic!("期望文本消息");
    }
}
