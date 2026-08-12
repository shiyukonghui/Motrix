//! 端到端测试：进程内模拟「daemon 模式 + 外部客户端」全流程（Phase 5 Task 12）
//!
//! 验证验收标准（对应任务描述）：
//! - 复用 motrix-core 引擎（ConfigManager / TaskManager）与 motrix-rpc 服务
//!   （JsonRpcServer），daemon 与 GUI 无行为分叉；
//! - 用 `motrix_rpc::client::JsonRpcClient`（外部客户端视角）依次执行：
//!   getVersion / getGlobalStat → addUri → pause / unpause → tellStatus（契约字段）→
//!   remove → purgeDownloadResult → motrix.saveUserConfig / motrix.getUserConfig。
//!
//! 线程模型说明（与 motrix-rpc/client.rs 测试一致）：`JsonRpcClient` 基于 reqwest
//! blocking 客户端，其 drop 在 tokio 异步上下文（如 `#[tokio::test]`）会触发
//! "Cannot drop a runtime in a context where blocking is not allowed" panic，
//! 故测试主体用普通 `#[test]`，仅用后台 multi-thread runtime 驱动 axum 服务。

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use motrix_core::config::ConfigManager;
use motrix_core::engine::TaskManager;
use motrix_core::CoreRpcBackend;
use motrix_rpc::client::JsonRpcClient;
use motrix_rpc::{JsonRpcServer, RpcBackend};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 测试辅助：临时目录 / 本地 HTTP 服务器
// ---------------------------------------------------------------------------

/// 临时目录（Drop 时递归清理，防止测试残留污染系统）
struct TempDir(PathBuf);

impl TempDir {
    /// 创建带唯一后缀的临时目录
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "motrix-cli-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 极简本地 HTTP 测试服务器（参考 motrix-core/src/engine.rs 测试的 TestHttpServer
/// 简化版）：仅响应 `/file`，支持 HEAD（声明 Accept-Ranges 供 KGet 探总长）与
/// GET（含 Range 分段）；块间可插入延迟以模拟慢速下载（保证 pause 命中 active）。
struct TestHttpServer {
    addr: SocketAddr,
    _thread: Option<thread::JoinHandle<()>>,
}

impl TestHttpServer {
    /// 启动服务器（body 按 64KB 分块发送，块间休眠 delay 模拟慢速下载）
    fn start(content: Vec<u8>, per_chunk_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定测试端口失败");
        // 非阻塞 accept：Drop 时线程随进程退出自然终止，不阻塞测试
        listener
            .set_nonblocking(true)
            .expect("设置非阻塞失败");
        let addr = listener.local_addr().unwrap();
        let thread = thread::spawn(move || loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // 每个连接独立线程处理：避免慢速发送阻塞后续 accept
                    let content = content.clone();
                    let delay = per_chunk_delay;
                    thread::spawn(move || {
                        let _ = handle_one_connection(stream, &content, delay);
                    });
                }
                // 非阻塞 accept：无连接时休眠后继续轮询
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        });
        Self {
            addr,
            _thread: Some(thread),
        }
    }

    /// 生成指向服务器的 URL
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.addr.port(), path)
    }
}

/// 处理单个连接：读取请求头 → 响应 → 显式关闭写端
fn handle_one_connection(
    mut stream: TcpStream,
    content: &[u8],
    delay: Duration,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    // 读取请求头（直到空行）
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let result = serve_request(&mut stream, &req, content, delay);
    // 显式关闭写端：让客户端读到 EOF，避免连接复用竞态
    let _ = stream.shutdown(Shutdown::Both);
    result
}

/// 处理单个 HTTP 请求：HEAD / GET（含 Range: bytes=start- 分段）
fn serve_request(
    stream: &mut TcpStream,
    req: &str,
    content: &[u8],
    delay: Duration,
) -> std::io::Result<()> {
    let request_line = req.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");
    let range = req
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .map(str::to_string);
    let total = content.len() as u64;

    let head: String;
    let body: &[u8];
    let is_target = path.starts_with("/file");
    if !is_target {
        head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string();
        body = &[];
    } else if method == "HEAD" {
        // 声明 Accept-Ranges: bytes：KGet 判定服务器支持 Range，走并行分段路径
        head = format!(
            "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
        );
        body = &[];
    } else if let Some(range) = range {
        // 解析 Range: bytes=start- 或 bytes=start-end（仅取 start）
        let start: u64 = range
            .split("bytes=")
            .nth(1)
            .and_then(|r| r.split('-').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
            .min(total);
        body = &content[start as usize..];
        let end = total.saturating_sub(1);
        head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
    } else {
        head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
        body = content;
    }

    // 写入响应（body 按 64KB 分块发送，块间插入延迟模拟慢速下载）
    stream.write_all(head.as_bytes())?;
    for chunk in body.chunks(64 * 1024) {
        stream.write_all(chunk)?;
        if !delay.is_zero() {
            thread::sleep(delay);
        }
    }
    stream.flush()
}

// ---------------------------------------------------------------------------
// 测试辅助：RPC 轮询
// ---------------------------------------------------------------------------

/// 轮询 aria2.tellStatus 直到任务到达指定状态（带超时，避免测试卡死）
///
/// 返回最终 tellStatus 结果；超时 / 任务不存在 / RPC 错误时 panic。
fn wait_status(client: &JsonRpcClient, gid: &str, status: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let r = client
            .call("aria2.tellStatus", vec![json!(gid)])
            .expect("tellStatus 应成功");
        if r.get("status").and_then(Value::as_str) == Some(status) {
            return r;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("任务 {gid} 未在超时内到达状态 {status}");
}

// ---------------------------------------------------------------------------
// 端到端主流程
// ---------------------------------------------------------------------------

/// 进程内「daemon + 外部客户端」全流程验收
#[test]
fn e2e_daemon_rpc_full_flow() {
    // 1. 启动慢速 HTTP 服务器：1MB 内容、单连接 64KB/块 × 120ms ≈ 1.9s 下载，
    //    保证 addUri 后立即 pause 时任务仍处于 active（而非瞬间 complete）
    let content: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let http_server = TestHttpServer::start(content.clone(), Duration::from_millis(120));

    // 2. 临时数据目录 + 下载目录
    let temp = TempDir::new("daemon");
    let dl_dir = temp.0.join("dl");
    std::fs::create_dir_all(&dl_dir).expect("创建下载目录失败");

    // 3. 后台 tokio runtime：仅用于驱动 axum RPC 服务（测试主体留在普通线程，
    //    原因见文件头注释：reqwest blocking client 不能在 tokio 上下文 drop）
    let rt = tokio::runtime::Runtime::new().expect("创建测试 runtime 失败");

    // 4. 初始化引擎（与 daemon 完全同构：ConfigManager → TaskManager → set_self）
    let config_manager = Arc::new(Mutex::new(ConfigManager::new(temp.0.clone())));
    // 全局下载目录指向临时目录（与任务级 options.dir 双保险）
    {
        let mut cm = config_manager.lock().expect("获取配置锁失败");
        let patch = json!({ "dir": dl_dir.to_string_lossy().to_string() });
        cm.update_system_config(patch.as_object().unwrap())
            .expect("更新全局下载目录失败");
    }
    let system = config_manager.lock().expect("获取配置锁失败").system_config().clone();
    let task_manager = Arc::new(TaskManager::new(&system, config_manager.clone()));
    task_manager.set_self(Arc::downgrade(&task_manager));

    // 5. 构造 aria2 兼容后端 + 启动 JSON-RPC 服务（port 0 = 系统分配）
    let backend: Arc<dyn RpcBackend> =
        Arc::new(CoreRpcBackend::new(task_manager.clone(), config_manager.clone()));
    let url = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定测试端口失败");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, JsonRpcServer::router(backend, "testsecret".to_string()))
                .await
                .expect("RPC 服务异常退出");
        });
        format!("http://{addr}/jsonrpc")
    });

    // 6. 外部客户端（带认证，与 motrix-cli 远程模式同一客户端）
    let client = JsonRpcClient::new(url, "testsecret".to_string());

    // 6.1 版本 / 统计：getVersion 版本串含 "motrix-engine"；getGlobalStat 字段齐全
    let version = client.call("aria2.getVersion", vec![]).expect("getVersion 应成功");
    assert!(
        version["version"].as_str().unwrap_or("").contains("motrix-engine"),
        "getVersion 应包含 motrix-engine 标识，实际: {version}"
    );
    let stat = client.call("aria2.getGlobalStat", vec![]).expect("getGlobalStat 应成功");
    for key in ["downloadSpeed", "uploadSpeed", "numActive", "numWaiting", "numStopped"] {
        assert!(
            stat[key].is_string(),
            "getGlobalStat.{key} 应为字符串（aria2 契约），实际: {}",
            stat[key]
        );
    }

    // 6.2 添加任务：addUri（本地 HTTP URL + options.dir 指向临时下载目录）→ gid
    let url = http_server.url("/file");
    let add_result = client
        .call(
            "aria2.addUri",
            vec![
                json!([url]),
                json!({ "dir": dl_dir.to_string_lossy().to_string(), "connections": 1 }),
            ],
        )
        .expect("addUri 应成功");
    let gids = add_result.as_array().expect("addUri 应返回 gid 数组");
    assert_eq!(gids.len(), 1, "单 URL 应产生一个任务");
    let gid = gids[0].as_str().expect("gid 应为字符串").to_string();

    // 6.3 暂停 / 恢复：pause / unpause 返回 "OK"（aria2 惯例；慢速下载保证
    //     pause 命中 active 而非终态）
    let paused = client
        .call("aria2.pause", vec![json!(gid)])
        .expect("pause 应成功");
    assert_eq!(paused.as_str(), Some("OK"), "pause 应返回 OK");
    let unpaused = client
        .call("aria2.unpause", vec![json!(gid)])
        .expect("unpause 应成功");
    assert_eq!(unpaused.as_str(), Some("OK"), "unpause 应返回 OK");

    // 6.4 等待完成并校验 tellStatus 契约字段：
    //     status=complete、totalLength/completedLength 与文件一致且数值均为字符串
    let status = wait_status(&client, &gid, "complete");
    let total = status["totalLength"].as_str().expect("totalLength 应为字符串");
    let completed = status["completedLength"].as_str().expect("completedLength 应为字符串");
    assert_eq!(total, content.len().to_string(), "totalLength 与服务器文件一致");
    assert_eq!(completed, content.len().to_string(), "completedLength 与服务器文件一致");
    // 下载文件落地校验：内容与源一致
    let saved = std::fs::read(dl_dir.join("file")).expect("输出文件应存在");
    assert_eq!(saved, content, "下载文件内容与源不一致");

    // 6.5 删除 / 清除：remove 返回 gid；purgeDownloadResult 返回 OK
    let removed = client
        .call("aria2.remove", vec![json!(gid)])
        .expect("remove 应成功");
    assert_eq!(removed.as_str(), Some(gid.as_str()), "remove 应返回 gid");
    let purged = client
        .call("aria2.purgeDownloadResult", vec![])
        .expect("purgeDownloadResult 应成功");
    assert_eq!(purged.as_str(), Some("OK"), "purgeDownloadResult 应返回 OK");

    // 6.6 用户配置扩展方法：saveUserConfig 写 user.json 的 user 分区 → getUserConfig 读回
    let saved_cfg = client
        .call("motrix.saveUserConfig", vec![json!({ "locale": "zh-CN" })])
        .expect("saveUserConfig 应成功");
    assert_eq!(saved_cfg.as_str(), Some("OK"), "saveUserConfig 应返回 OK");
    // 读文件验证 user.json 已落盘（locale 键）
    let user_file: Value = serde_json::from_str(
        &std::fs::read_to_string(temp.0.join("user.json")).expect("user.json 应已写出"),
    )
    .expect("user.json 应可解析");
    assert_eq!(user_file["locale"], "zh-CN", "user.json 应包含更新后的 locale");
    // 经 RPC 读回：motrix.getUserConfig 返回完整 user 配置
    let user_cfg = client
        .call("motrix.getUserConfig", vec![])
        .expect("getUserConfig 应成功");
    assert_eq!(
        user_cfg["locale"].as_str(),
        Some("zh-CN"),
        "getUserConfig 应返回更新后的 locale"
    );

    // 7. 清理：停止后台 runtime（服务器 / 引擎线程随进程结束自然回收）
    drop(rt);
}
