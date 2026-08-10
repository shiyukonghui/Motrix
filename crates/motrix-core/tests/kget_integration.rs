//! KGet 引擎集成测试（Task 9.4）
//!
//! 用 `std::net::TcpListener` 手写极简 HTTP 测试服务器（不引重型依赖）：
//! - 支持 HEAD / GET，返回固定字节内容；
//! - 支持 `Range: bytes=` 部分响应（206 + Content-Range），以验证断点续传；
//! - 记录每个请求的方法 / 路径 / Range 头，供断言"续传请求从已有字节开始"。
//!
//! 用例覆盖：
//! - a. 小文件（64KB）下载：事件序列 Started → Finished，文件存在且内容一致；
//! - b. 断点续传：先手动写一半文件再下载，断言最终文件完整、服务器收到
//!     从已有字节开始的 Range 请求、进度为绝对进度（非从 0）；
//! - c. 进度事件：至少收到一次 Progress 且 percent 在 0~100 之间（speed 如实为 0）。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use motrix_core::{EngineOptions, KgetEvent, KgetHandle, spawn_download};

/// 单个请求的日志（供续传断言：是否收到从已有字节开始的 Range 请求）
#[derive(Debug, Clone)]
struct RequestLog {
    range: Option<String>,
}

/// 极简 HTTP 测试服务器
struct TestServer {
    addr: SocketAddr,
    logs: Arc<Mutex<Vec<RequestLog>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    /// 启动服务器：监听 127.0.0.1 随机端口，返回固定字节内容
    fn spawn(data: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定测试服务器端口失败");
        let addr = listener.local_addr().expect("获取测试服务器地址失败");
        // 非阻塞 accept + 轮询，便于 stop 后线程及时退出
        listener
            .set_nonblocking(true)
            .expect("设置非阻塞失败");

        let data = Arc::new(data);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let data_t = data.clone();
        let logs_t = logs.clone();
        let stop_t = stop.clone();
        let thread = thread::spawn(move || {
            loop {
                if stop_t.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // 每连接一个处理线程（reqwest 每请求新连接，避免阻塞 accept 循环）
                        let data = data_t.clone();
                        let logs = logs_t.clone();
                        thread::spawn(move || handle_connection(stream, &data, &logs));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            logs,
            stop,
            thread: Some(thread),
        }
    }

    /// 已收到的请求日志
    fn logs(&self) -> Vec<RequestLog> {
        self.logs.lock().expect("请求日志锁中毒").clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // 停止 accept 循环并等待服务器线程退出
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 处理单个 HTTP 连接：解析请求行与头，按 HEAD / GET(+Range) 返回对应响应
fn handle_connection(mut stream: TcpStream, data: &[u8], logs: &Mutex<Vec<RequestLog>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("克隆流失败"));
    // 请求行：`METHOD PATH HTTP/1.1`
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    // path 不参与业务逻辑（测试服务器对任意路径返回同一内容），无需解析

    // 头字段（读到空行结束），提取 Range
    let mut range: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" {
            break;
        }
        if line.to_ascii_lowercase().starts_with("range:") {
            let idx = line.find(':').expect("range 头含冒号");
            range = Some(line[idx + 1..].trim().to_string());
        }
    }
    logs.lock()
        .expect("请求日志锁中毒")
        .push(RequestLog { range: range.clone() });

    let total = data.len() as u64;
    // HEAD：只返回头（AdvancedDownloader 用它探测大小与 Range 支持）
    if method == "HEAD" {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.flush();
        return;
    }

    // GET（可带 Range）
    match parse_range(range.as_deref(), total) {
        Some((start, end)) => {
            let body = &data[start as usize..=end as usize];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        }
        None => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(data);
        }
    }
    let _ = stream.flush();
}

/// 解析 `Range: bytes=a-b` / `bytes=a-` 为闭区间 [start, end]
fn parse_range(range: Option<&str>, total: u64) -> Option<(u64, u64)> {
    let spec = range?.strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let start: u64 = s.trim().parse().ok()?;
    let end: u64 = if e.trim().is_empty() {
        total.saturating_sub(1)
    } else {
        e.trim().parse().ok()?
    };
    if start >= total || start > end {
        return None;
    }
    Some((start, end.min(total - 1)))
}

/// 确定性伪随机字节（避免引入 rand 依赖），用于内容一致性比较
fn random_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i.wrapping_mul(31) ^ (i >> 3)) % 251) as u8)
        .collect()
}

/// 创建本次测试独立临时目录（带名称与进程 id，避免并行测试冲突）
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "motrix-kget-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir
}

/// 构造测试用引擎选项（保存目录 + 文件名 + 2 连接）
fn test_options(dir: &PathBuf, out: &str) -> EngineOptions {
    let mut options = EngineOptions::default();
    options.connections = 2;
    options.dir = dir.to_string_lossy().to_string();
    options.out = Some(out.to_string());
    options
}

/// 执行一次下载并收集全部事件，阻塞直到 Finished / Failed（或超时）
fn run_download(gid: &str, url: &str, options: &EngineOptions) -> Vec<KgetEvent> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_cb = events.clone();
    let handle: KgetHandle = spawn_download(gid.to_string(), url, options, move |_gid, event| {
        events_cb.lock().expect("事件锁中毒").push(event);
    })
    .expect("spawn_download 初始化失败");

    // 轮询事件列表直到出现结束事件（Finished / Failed），30 秒超时
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let done = {
            let guard = events.lock().expect("事件锁中毒");
            guard.iter().any(|e| {
                matches!(e, KgetEvent::Finished { .. } | KgetEvent::Failed { .. })
            })
        };
        if done {
            break;
        }
        if Instant::now() > deadline {
            handle.abort();
            panic!(
                "下载超时（30s），已收到事件: {:?}",
                *events.lock().expect("事件锁中毒")
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = handle.join();
    // 末尾表达式避免 MutexGuard 临时值跨 block 存活（E0597）
    let snapshot = events.lock().expect("事件锁中毒").clone();
    snapshot
}

// ---------------------------------------------------------------------------
// 用例 a：小文件（64KB）下载，事件序列与文件内容
// ---------------------------------------------------------------------------
#[test]
fn downloads_small_file_with_finished_event() {
    let data = random_bytes(64 * 1024);
    let server = TestServer::spawn(data.clone());
    let dir = temp_dir("small");
    let options = test_options(&dir, "small.bin");
    let url = format!("http://{}/small.bin", server.addr);

    let events = run_download("test-a", &url, &options);

    // 事件序列：首事件 Started、末事件 Finished（path 为输出文件）
    assert_eq!(events.first(), Some(&KgetEvent::Started), "首事件应为 Started: {events:?}");
    let last = events.last().expect("应至少收到事件");
    let finished_path = match last {
        KgetEvent::Finished { path } => path.clone(),
        other => panic!("末事件应为 Finished，实际: {other:?}"),
    };
    let finished_path = finished_path.expect("Finished 应携带输出路径");

    // 目标文件存在且内容与服务器字节完全一致
    let file = dir.join("small.bin");
    assert!(file.exists(), "目标文件应存在: {}", file.display());
    assert_eq!(fs::read(&file).expect("读取下载文件失败"), data);
    // Finished 携带的路径指向同一文件（Windows 上 KGet 返回 `/` 而
    // to_string_lossy 返回 `\`，故不做字符串比较，用内容一致性验证）
    assert_eq!(fs::read(&finished_path).expect("读取 Finished 路径失败"), data);
}

// ---------------------------------------------------------------------------
// 用例 b：断点续传——先手动写一半文件，再下载应从已有字节继续
// ---------------------------------------------------------------------------
#[test]
fn resumes_from_existing_partial_file() {
    let data = random_bytes(256 * 1024);
    let server = TestServer::spawn(data.clone());
    let dir = temp_dir("resume");
    let options = test_options(&dir, "resume.bin");
    let url = format!("http://{}/resume.bin", server.addr);

    // 模拟第一次下载中断：手动写入前一半字节（KGet 续传依赖已有文件大小）
    let half = data.len() / 2;
    let file_path = dir.join("resume.bin");
    fs::write(&file_path, &data[..half]).expect("写入部分文件失败");

    let events = run_download("test-b", &url, &options);

    // 1. 最终文件完整且内容一致（续传补全后半部分）
    assert_eq!(fs::read(&file_path).expect("读取下载文件失败"), data);

    // 2. 服务器收到从已有字节开始的 Range 请求（而非重新从 0 下载）
    let resumed = server.logs().iter().any(|log| {
        log.range
            .as_deref()
            .is_some_and(|r| r.starts_with(&format!("bytes={half}-")))
    });
    assert!(
        resumed,
        "应收到从已有字节 {half} 开始的 Range 请求，实际请求日志: {:?}",
        server.logs()
    );

    // 3. 进度为绝对进度：存在明显大于 0 的中间进度（续传从 ~50% 开始，
    //    而非从 0 重新累计）
    let has_mid_progress = events.iter().any(|e| {
        matches!(e, KgetEvent::Progress { percent, .. } if *percent > 5.0 && *percent < 99.0)
    });
    assert!(
        has_mid_progress,
        "应收到 5%~99% 之间的中间进度事件（绝对进度），实际事件: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// 用例 c：进度事件——至少一次 Progress 且 percent / speed 合理
// ---------------------------------------------------------------------------
#[test]
fn emits_progress_events_with_reasonable_percent() {
    let data = random_bytes(256 * 1024);
    let server = TestServer::spawn(data.clone());
    let dir = temp_dir("progress");
    let options = test_options(&dir, "progress.bin");
    let url = format!("http://{}/progress.bin", server.addr);

    let events = run_download("test-c", &url, &options);

    // 至少收到一次 Progress 事件
    let progresses: Vec<&KgetEvent> = events
        .iter()
        .filter(|e| matches!(e, KgetEvent::Progress { .. }))
        .collect();
    assert!(
        !progresses.is_empty(),
        "应至少收到一次进度事件，实际事件: {events:?}"
    );

    // 每个 Progress 的 percent 均在 0~100 区间
    for event in &progresses {
        if let KgetEvent::Progress { percent, .. } = event {
            assert!(
                (0.0..=100.0).contains(percent),
                "percent 超出 0~100: {percent}"
            );
        }
    }

    // speed 如实为 0（KGet 1.7 进度回调不提供实时速度，属库限制）
    for event in &events {
        if let KgetEvent::Progress { speed, .. } = event {
            assert_eq!(*speed, 0, "KGet 1.7 不提供实时速度，speed 应恒为 0");
        }
    }
}
