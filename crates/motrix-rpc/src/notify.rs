//! 通知骨架：向订阅者推送 aria2 兼容的通知（onDownloadStart / onDownloadComplete 等）。
//!
//! Phase 2 由 motrix-core 引擎状态驱动真实推送；本任务仅提供：
//! - [build_notification]：构建标准 JSON-RPC 通知对象
//! - [Notifier]：broadcast 通道骨架（subscribe 订阅 / notify 广播）

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// 构建 JSON-RPC 通知对象：
/// `{"jsonrpc":"2.0","method":"aria2.onDownloadStart","params":[...]}`
///
/// 注意：通知没有 `id` 字段（区别于请求/响应）。
pub fn build_notification(method: &str, params: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params
    })
}

/// 通知器：持有 broadcast 发送端，可被多处 [Notifier::subscribe] 订阅。
///
/// Phase 2 中 WS 连接建立时可订阅本通道，把引擎事件转发给外部客户端。
#[derive(Clone)]
pub struct Notifier {
    /// 通知广播通道（发送端）
    tx: tokio::sync::broadcast::Sender<Value>,
}

impl Notifier {
    /// 创建通知器（内部 channel 容量 64，慢订阅者会丢消息——外部客户端场景可接受）
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self { tx }
    }

    /// 订阅通知流（接收端；注意 broadcast 会回放最近消息）
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.tx.subscribe()
    }

    /// 广播一条通知（method 形如 `aria2.onDownloadStart`；无订阅者时静默丢弃）
    pub fn notify(&self, method: &str, params: Vec<Value>) {
        let _ = self.tx.send(build_notification(method, params));
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 通知对象结构正确() {
        let notifier = Notifier::new();
        let mut rx = notifier.subscribe();
        // 广播一条 onDownloadStart 通知
        notifier.notify("aria2.onDownloadStart", vec![json!({ "gid": "2089b05ecca3d829" })]);
        // 订阅者能收到同一对象
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["method"], "aria2.onDownloadStart");
        assert_eq!(msg["params"][0]["gid"], "2089b05ecca3d829");
        // 通知无 id 字段
        assert!(msg.get("id").is_none());
    }

    #[test]
    fn build_notification辅助函数() {
        let n = build_notification("aria2.onDownloadComplete", vec![json!({ "gid": "abc" })]);
        assert_eq!(n["jsonrpc"], "2.0");
        assert_eq!(n["method"], "aria2.onDownloadComplete");
        assert_eq!(n["params"][0]["gid"], "abc");
    }
}
