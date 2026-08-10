//! axum 服务器：`127.0.0.1:{port}/jsonrpc` 的 WS + HTTP POST 双通道。
//!
//! - `POST /jsonrpc`：body 为 JSON-RPC 请求文本
//! - `GET /jsonrpc`：升级为 WebSocket，逐条处理文本消息并回写响应
//!
//! 认证（token:{secret}）与协议处理位于 [crate::methods]，本模块只负责传输层。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

use crate::methods;
use crate::RpcBackend;

/// 路由共享状态：后端实现 + 认证密钥
#[derive(Clone)]
struct RpcState {
    /// 后端引擎实现（RpcBackend 抽象，解耦 motrix-core）
    backend: Arc<dyn RpcBackend>,
    /// RPC 认证密钥（空串表示不要求认证）
    secret: String,
}

/// JSON-RPC 服务器：自持 listener 持续服务。
///
/// - port 默认 16800（构造时传入，传 0 则由系统分配端口，供测试用）
/// - secret 为空串表示不要求认证
pub struct JsonRpcServer {
    backend: Arc<dyn RpcBackend>,
    port: u16,
    secret: String,
}

impl JsonRpcServer {
    /// 创建服务实例
    pub fn new(backend: Arc<dyn RpcBackend>, port: u16, secret: String) -> Self {
        Self {
            backend,
            port,
            secret,
        }
    }

    /// 构建 axum Router（供测试 oneshot 直接调用，或 [Self::serve] 内部复用）。
    ///
    /// 路由：`GET /jsonrpc` → WS 升级；`POST /jsonrpc` → JSON body 处理。
    pub fn router(backend: Arc<dyn RpcBackend>, secret: String) -> Router {
        Router::new()
            .route("/jsonrpc", get(ws_handler).post(post_handler))
            .with_state(RpcState { backend, secret })
    }

    /// 绑定 `127.0.0.1:{port}` 并持续服务，直到 listener 关闭。
    pub async fn serve(self) -> Result<(), anyhow::Error> {
        // 仅监听回环地址，避免暴露到局域网
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        tracing::info!(%local, "JSON-RPC 服务已启动（WS + HTTP POST 双通道）");
        axum::serve(listener, Self::router(self.backend, self.secret)).await?;
        Ok(())
    }
}

/// `GET /jsonrpc`：WebSocket 升级处理器
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<RpcState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_session(socket, state))
}

/// WS 会话：逐条接收文本消息 → 协议层处理 → 回写响应文本
async fn ws_session(mut socket: WebSocket, state: RpcState) {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(text) => {
                let resp = methods::process_request_str(text.as_str(), &*state.backend, &state.secret);
                // 对端断开（发送失败）则结束会话
                if socket.send(Message::Text(resp.into())).await.is_err() {
                    break;
                }
            }
            // 客户端关闭连接
            Message::Close(_) => break,
            // 二进制帧 / 乒乓帧等忽略
            _ => {}
        }
    }
}

/// `POST /jsonrpc`：处理 JSON body
async fn post_handler(State(state): State<RpcState>, body: String) -> Response<Body> {
    let resp = methods::process_request_str(&body, &*state.backend, &state.secret);
    // 显式声明 application/json（JSON-RPC 响应）
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    (headers, resp).into_response()
}
