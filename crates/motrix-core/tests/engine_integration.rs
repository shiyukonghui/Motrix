//! TaskManager 集成测试（Task 13.2，对应 MIGRATION-TAURI.md 12 节「下载集成测试」）
//!
//! 复用 tests/kget_integration.rs 的本地 HTTP 服务器写法（`std::net::TcpListener`，
//! 支持 GET + `Range: bytes=` 206 + 记录请求日志），验证任务编排胶水层
//! （TaskManager）的完整行为：
//!
//! - a. 全生命周期：add_uri → 下载完成（文件与服务器一致）→ remove → 仓库无该任务
//! - b. 暂停 / 恢复：大文件任务 pause → Paused（active 句柄清空释放并发槽位）→
//!      resume → complete（KGet 预分配缺陷由 engine.rs 的 cleanup_partial_file 兜底，
//!      以现状行为为准断言最终完成 + 文件完整）
//! - c. 并发队列：max_concurrent=1 下第二个任务初始 Waiting，第一个完成后被 promote
//!      为 Active 并最终 Complete
//! - d. 事件流：subscribe_events 收到 start / pause / complete 且顺序合理
//!
//! 说明：所有轮询均有 30s 超时上限，避免测试挂死；下载目录用
//! `std::env::temp_dir()` 下的唯一临时目录（用完删除）。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use motrix_core::config::{ConfigManager, SystemConfig};
use motrix_core::{TaskManager, TaskRepository, TaskStatus};
use serde_json::json;

/// 所有轮询共用的超时上限（避免测试挂死）
const TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// 极简本地 HTTP 测试服务器（支持 HEAD / GET + Range 206 + 每块发送延迟）
// ---------------------------------------------------------------------------

/// 单个请求日志（记录 Range 头，供续传场景断言；当前用例未直接读取，保留记录能力）
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct RequestLog {
    range: Option<String>,
}

/// 极简本地 HTTP 测试服务器
struct TestServer {
    addr: SocketAddr,
    /// 请求日志（当前用例未直接读取，保留记录能力供续传断言）
    #[allow(dead_code)]
    logs: Arc<Mutex<Vec<RequestLog>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    /// 启动服务器：监听 127.0.0.1 随机端口，返回固定字节内容；
    /// `per_chunk_delay` 可放慢发送速度，用于模拟长时下载（暂停 / 并发队列测试）
    fn spawn(data: Vec<u8>, per_chunk_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定测试服务器端口失败");
        let addr = listener.local_addr().expect("获取测试服务器地址失败");
        // 非阻塞 accept + 轮询，便于 stop 后线程及时退出
        listener.set_nonblocking(true).expect("设置非阻塞失败");

        let data = Arc::new(data);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (data_t, logs_t, stop_t) = (data.clone(), logs.clone(), stop.clone());
        let thread = thread::spawn(move || loop {
            if stop_t.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    // 每连接一个处理线程（reqwest 每请求新连接，避免阻塞 accept 循环）
                    let data = data_t.clone();
                    let logs = logs_t.clone();
                    thread::spawn(move || {
                        let _ = handle_connection(stream, &data, &logs, per_chunk_delay);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        });

        Self {
            addr,
            logs,
            stop,
            thread: Some(thread),
        }
    }

    /// 生成指向服务器的 URL
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.addr.port(), path)
    }

    /// 已收到的请求日志（当前用例未直接读取，保留记录能力供续传断言）
    #[allow(dead_code)]
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

/// 处理单个 HTTP 连接：解析请求行与头，按 HEAD / GET(+Range) 返回对应响应；
/// 响应体按块发送，块间可插入延迟
fn handle_connection(
    mut stream: TcpStream,
    data: &[u8],
    logs: &Mutex<Vec<RequestLog>>,
    delay: Duration,
) -> std::io::Result<()> {
    use std::net::Shutdown;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone().expect("克隆流失败"));

    // 请求行：`METHOD PATH HTTP/1.1`
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return Ok(());
    }
    let method = request_line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

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
    // HEAD：声明 Accept-Ranges（KGet 判定服务器支持 Range，走并行分段路径，
    // 该路径有取消检查，abort 后引擎线程能快速退出，pause/resume 测试稳定）
    if method == "HEAD" {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes())?;
        return stream.flush();
    }

    // GET（可带 Range）：按块发送并插入延迟
    let (status_head, body) = match parse_range(range.as_deref(), total) {
        Some((start, end)) => {
            let body = &data[start as usize..=end as usize];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                body.len()
            );
            (head, body)
        }
        None => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            );
            (head, data)
        }
    };
    stream.write_all(status_head.as_bytes())?;
    for chunk in body.chunks(64 * 1024) {
        stream.write_all(chunk)?;
        if !delay.is_zero() {
            thread::sleep(delay);
        }
    }
    // 显式关闭写端：让客户端读到 EOF，避免连接复用竞态
    let _ = stream.shutdown(Shutdown::Both);
    stream.flush()
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

// ---------------------------------------------------------------------------
// 测试基础设施：临时目录 + 测试管理器 + 等待辅助
// ---------------------------------------------------------------------------

/// 测试临时目录（Drop 时整体清理）
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "motrix-engine-int-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("创建临时目录失败");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 构造测试用 TaskManager（max_concurrent 可配置；全局 dir 指向临时下载目录）
fn test_manager(
    max_concurrent: u32,
) -> (Arc<TaskManager>, Arc<Mutex<TaskRepository>>, TempDir) {
    let temp = TempDir::new("mgr");
    let dl_dir = temp.path().join("dl");
    fs::create_dir_all(&dl_dir).expect("创建下载目录失败");
    let config_manager = Arc::new(Mutex::new(ConfigManager::new(temp.path().to_path_buf())));
    let mut system = SystemConfig::defaults(temp.path());
    system.max_concurrent_downloads = max_concurrent;
    system.dir = dl_dir.to_string_lossy().to_string();
    let tm = Arc::new(TaskManager::new(&system, config_manager));
    // 注入自引用（Weak）：引擎线程事件回调需要升级为 Arc 再回调任务管理器
    tm.set_self(Arc::downgrade(&tm));
    let repo = tm.repo.clone();
    (tm, repo, temp)
}

/// 确定性伪随机字节（避免引入 rand 依赖），用于内容一致性比较
fn random_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i.wrapping_mul(31) ^ (i >> 3)) % 251) as u8)
        .collect()
}

/// 轮询等待任务到达指定状态（带超时，避免测试卡死）
fn wait_status(
    repo: &Arc<Mutex<TaskRepository>>,
    gid: &str,
    status: TaskStatus,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(guard) = repo.lock() {
            if guard.get(gid).map(|t| t.status) == Some(status) {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 等待任务被 promote（Active 或已快速完成均视为通过，用于并发队列用例）
fn wait_promoted(repo: &Arc<Mutex<TaskRepository>>, gid: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(guard) = repo.lock() {
            if let Some(task) = guard.get(gid) {
                if matches!(task.status, TaskStatus::Active | TaskStatus::Complete) {
                    return true;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 等待任务出现实际进度（completed > 0；超时不报错，pause 测试中用于确保下载已开始）
fn wait_progress(repo: &Arc<Mutex<TaskRepository>>, gid: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let has_progress = repo
            .lock()
            .map(|g| g.get(gid).map(|t| t.completed_length > 0))
            .unwrap_or(Some(false));
        if has_progress == Some(true) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// 等待任务完成并断言：totalLength / completedLength 正确 + 输出文件内容一致
fn assert_completed_with_file(
    repo: &Arc<Mutex<TaskRepository>>,
    gid: &str,
    expected: &[u8],
) {
    assert!(
        wait_status(repo, gid, TaskStatus::Complete, TIMEOUT),
        "任务 {gid} 未在超时内完成"
    );
    let (path, total, completed) = {
        let guard = repo.lock().expect("获取任务仓库锁失败");
        let task = guard.get(gid).expect("任务应存在于仓库");
        (
            task.files[0].path.clone(),
            task.total_length,
            task.completed_length,
        )
    };
    // HTTP 场景探总长成功：totalLength / completedLength 应与文件一致
    assert_eq!(total, expected.len() as u64);
    assert_eq!(completed, expected.len() as u64);
    let saved = fs::read(&path).expect("输出文件应存在");
    assert_eq!(saved, expected, "输出文件内容与源不一致");
}

// ---------------------------------------------------------------------------
// 用例 a：全生命周期——add_uri → 下载完成 → remove
// ---------------------------------------------------------------------------
#[test]
fn lifecycle_add_download_complete_remove() {
    let content = random_bytes(256 * 1024);
    let server = TestServer::spawn(content.clone(), Duration::ZERO);
    let (tm, repo, _temp) = test_manager(1);

    // add_uri：单 URL → gid 列表（每个 URL 一个独立任务）
    let gids = tm
        .add_uri(&[server.url("/life.bin")], &json!({"out": "life.bin", "connections": 1}))
        .expect("add_uri 应成功");
    assert_eq!(gids.len(), 1);
    let gid = &gids[0];
    // gid 为 16 位小写 hex（与 aria2 一致）
    assert_eq!(gid.len(), 16);

    // 轮询直到 complete：文件已写入且内容与服务器一致
    assert_completed_with_file(&repo, gid, &content);

    // remove：仓库不再有该任务（进入 removed 历史，stopped 列表可见）
    tm.remove(gid).expect("remove 应成功");
    assert!(
        !repo.lock().unwrap().contains(gid),
        "移除后仓库不应再有该任务"
    );
    assert!(
        repo.lock()
            .unwrap()
            .stopped()
            .iter()
            .any(|t| t.gid == *gid && t.status == TaskStatus::Removed),
        "被移除任务应出现在 stopped 历史（removed 状态）"
    );
}

// ---------------------------------------------------------------------------
// 用例 b：暂停 / 恢复——大文件任务 pause → Paused（槽位释放）→ resume → complete
// ---------------------------------------------------------------------------
#[test]
fn pause_releases_slot_and_resume_completes() {
    // 4MB + 每块 2ms 发送延迟：保证下载过程可被暂停打断
    let content = vec![7u8; 4 * 1024 * 1024];
    let server = TestServer::spawn(content.clone(), Duration::from_millis(2));
    let (tm, repo, _temp) = test_manager(1);

    let gids = tm
        .add_uri(&[server.url("/pause.bin")], &json!({"out": "pause.bin", "connections": 1}))
        .expect("add_uri 应成功");
    let gid = &gids[0];

    // 等待下载开始且有实际进度
    assert!(
        wait_status(&repo, gid, TaskStatus::Active, TIMEOUT),
        "任务未进入 active"
    );
    wait_progress(&repo, gid, TIMEOUT);

    // 暂停：状态 Paused
    tm.pause(gid).expect("暂停应成功");
    assert_eq!(
        repo.lock().unwrap().get(gid).unwrap().status,
        TaskStatus::Paused
    );

    // active 句柄清空（释放并发槽位）：max_concurrent=1 下新任务应立即 Active
    let gid2 = tm
        .add_uri(&[server.url("/b.bin")], &json!({"out": "b.bin", "connections": 1}))
        .expect("add_uri 应成功")[0]
        .clone();
    assert!(
        wait_status(&repo, &gid2, TaskStatus::Active, TIMEOUT),
        "暂停后并发槽位未释放，第二个任务未立即 active"
    );
    // 清理第二个任务（避免影响后续断言）
    tm.remove(&gid2).expect("移除第二个任务应成功");

    // 恢复：Range 续传 → 最终 complete（KGet 预分配缺陷由 cleanup_partial_file 兜底，
    // 以现状行为为准断言最终完成 + 文件完整，而非"部分字节续传"）
    tm.resume(gid).expect("恢复应成功");
    assert_completed_with_file(&repo, gid, &content);
}

// ---------------------------------------------------------------------------
// 用例 c：并发队列——max_concurrent=1 下第二个任务排队，完成后 promote
// ---------------------------------------------------------------------------
#[test]
fn max_concurrent_queue_promotes_second_task() {
    // 慢速服务器确保第一个任务不会瞬间完成（便于观察排队状态）
    let content = vec![42u8; 2 * 1024 * 1024];
    let server = TestServer::spawn(content.clone(), Duration::from_millis(2));
    // 最大并发 1：第二个任务必须排队
    let (tm, repo, _temp) = test_manager(1);

    let gid1 = tm
        .add_uri(&[server.url("/one.bin")], &json!({"out": "one.bin", "connections": 1}))
        .expect("add_uri 应成功")[0]
        .clone();
    let gid2 = tm
        .add_uri(&[server.url("/two.bin")], &json!({"out": "two.bin", "connections": 1}))
        .expect("add_uri 应成功")[0]
        .clone();

    // 第一个立即 Active；第二个因并发限制保持 Waiting
    assert!(
        wait_status(&repo, &gid1, TaskStatus::Active, TIMEOUT),
        "第一个任务未进入 active"
    );
    assert_eq!(
        repo.lock().unwrap().get(&gid2).unwrap().status,
        TaskStatus::Waiting,
        "并发限制下第二个任务应为 Waiting"
    );

    // 第一个完成后，第二个被自动 promote 为 Active 并最终 Complete
    assert_completed_with_file(&repo, &gid1, &content);
    assert!(
        wait_promoted(&repo, &gid2, TIMEOUT),
        "第二个任务未被 promote 为 active"
    );
    assert_completed_with_file(&repo, &gid2, &content);
}

// ---------------------------------------------------------------------------
// 用例 d：事件流——subscribe_events 收到 start / pause / complete 且顺序合理
// ---------------------------------------------------------------------------
#[test]
fn event_stream_has_start_pause_and_complete_in_order() {
    use tokio::sync::broadcast::error::TryRecvError;

    // 慢速任务：下载期间暂停，验证 pause 事件
    let content = vec![9u8; 4 * 1024 * 1024];
    let server = TestServer::spawn(content.clone(), Duration::from_millis(2));
    let (tm, repo, _temp) = test_manager(1);
    // 先订阅（早于任务添加，避免错过事件）
    let mut rx = tm.subscribe_events();

    let gids = tm
        .add_uri(&[server.url("/ev.bin")], &json!({"out": "ev.bin", "connections": 1}))
        .expect("add_uri 应成功");
    let gid = &gids[0];

    // 等待下载开始并暂停（应产生 start → pause 事件）
    assert!(
        wait_status(&repo, gid, TaskStatus::Active, TIMEOUT),
        "任务未进入 active"
    );
    wait_progress(&repo, gid, TIMEOUT);
    tm.pause(gid).expect("暂停应成功");
    assert!(
        wait_status(&repo, gid, TaskStatus::Paused, TIMEOUT),
        "任务未进入 paused"
    );

    // 恢复 → 最终 complete（resume 再次产生 start 事件）
    tm.resume(gid).expect("恢复应成功");
    assert_completed_with_file(&repo, gid, &content);

    // 收集全部事件（broadcast 保留最近历史；顺序由发送顺序保证）
    let mut events: Vec<String> = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(ev) => events.push(ev.event),
            // 订阅落后于发送方（容量内历史被跳过）：继续接收
            Err(TryRecvError::Lagged(_)) => continue,
            // Empty / Closed：事件已全部收完
            Err(_) => break,
        }
    }
    assert!(
        events.contains(&"start".to_string()),
        "事件流应包含 start，实际: {events:?}"
    );
    assert!(
        events.contains(&"pause".to_string()),
        "事件流应包含 pause，实际: {events:?}"
    );
    assert!(
        events.contains(&"complete".to_string()),
        "事件流应包含 complete，实际: {events:?}"
    );
    // 顺序合理：首次 start 早于 pause，pause 早于最终 complete
    let pos_start = events.iter().position(|e| e == "start").expect("start 存在");
    let pos_pause = events.iter().position(|e| e == "pause").expect("pause 存在");
    let pos_complete = events
        .iter()
        .position(|e| e == "complete")
        .expect("complete 存在");
    assert!(
        pos_start < pos_pause && pos_pause < pos_complete,
        "事件顺序应 start < pause < complete，实际: {events:?}"
    );
}
