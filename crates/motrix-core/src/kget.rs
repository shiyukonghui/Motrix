//! KGet 下载引擎封装（Phase 2 Task 9）
//!
//! 基于 KGet v1.7（crates.io 包名 `Kget`，库名 `kget`，MIT）封装 Motrix 的下载引擎，
//! 对应迁移文档 5.1 / 5.3 / 5.4 节（MIGRATION-TAURI.md）。
//!
//! ## KGet 1.7 真实 API 调研结论（docs.rs 1.7.0 + 本地源码，写于实现期）
//!
//! - 推荐入口 `kget::builder(url)` 返回 `DownloadBuilder`，链式方法：
//!   `output(path)` / `connections(usize)`（内部 clamp 1~32）/ `speed_limit(bytes/s)` /
//!   `proxy(url)` / `proxy_auth(user, pass)` / `sha256|sha512|sha1|md5|blake3(hash)` /
//!   `header(name, value)`（可多次）/ `retry(RetryConfig)` / `range(start, end)` /
//!   `quiet(bool)` / `download()` / `download_to_bytes()`。
//! - `.spawn()` 返回 `(std::thread::JoinHandle<Result<DownloadResult, KgetError>>,
//!   std::sync::mpsc::Receiver<DownloadEvent>)`；`DownloadEvent` 枚举：
//!   `Progress { percent: f64 /* 0~100 */, speed_bps: u64, eta_secs: Option<u64> }` /
//!   `Status(String)` / `Completed { path, sha256 }` / `Error(String)`。
//!   注意：**进度事件不提供已下载/总字节数，且 builder 传入的 speed_bps 恒为 0**。
//! - `connections > 1` 时 builder 内部走 `AdvancedDownloader`（rayon 并行分段 +
//!   FileExt 偏移写入），**自带断点续传**：检测输出文件已存在大小 `existing_size`，
//!   从该位置继续分段（`calculate_chunks(total, existing)`），进度回调为绝对进度
//!   （`(已有 + 本次新增) / 总大小`）。`connections == 1` 时走单连接 `http_download`。
//! - `AdvancedDownloader` 提供 `set_cancel_token(Arc<AtomicBool>)` **真取消**、
//!   `set_resume_policy(ResumePolicy)`（库调用必须设为 `AlwaysResume`，否则续传时
//!   会阻塞 stdin 询问）、`set_extra_headers`、`set_progress_callback(f32)`。
//! - features：`default = []`（无默认特性）；HTTP/HTTPS/FTP/SFTP/WebDAV 均为内置
//!   非 optional 能力；`gui` / `torrent-native` / `torrent-transmission` / `async`
//!   为可选特性。故依赖声明 `default-features = false` 即为最精简。
//!
//! ## 封装取舍（与任务原型的差异，均因真实 API 调整）
//!
//! - 任务原型要求"封装 `builder` + `.spawn()` 的 Receiver"。但 `builder.spawn()`
//!   返回 `std::thread::JoinHandle`（**无 abort 方法**），无法实现暂停/删除所需的
//!   中止能力；`builder` 也不暴露取消令牌。因此本封装**直接使用
//!   `AdvancedDownloader`**（builder 多连接路径的底层引擎）：通过 `set_cancel_token`
//!   实现真实 `abort()`，通过 `AlwaysResume` + 已有文件实现断点续传，行为与
//!   builder 多连接路径完全一致。
//! - `KgetEvent::Progress` 与真实事件对齐：仅 `percent`（0~100）与 `speed`
//!   （KGet 1.7 进度回调不提供实时速度，恒为 0）；无已下载/总字节数字段。
//! - 重试：`max-tries` / `retry-wait` 由本封装在引擎线程外层实现（胶水层逻辑，
//!   非下载算法）；KGet 内部另有每 chunk 3 次重试。
//! - User-Agent：KGet 1.7 无独立 UA 方法且 `AdvancedDownloader` 内置 UA 为
//!   `KGet/<版本>`，自定义 UA 经 `extra_headers` 注入（reqwest 可能追加而非覆盖，
//!   属 KGet 限制，如实注释）。
//!
//! ## 暂停 / 恢复 / 删除语义
//!
//! KGet 基于 HTTP Range 自带断点续传（已有部分文件时从该位置继续）。
//! - 暂停 = `KgetHandle::abort()`（置位取消令牌，下载在下个检查点停止，部分文件保留）
//!   + 上层将任务标记为 `Paused`；
//! - 恢复 = 以相同 URL / 输出路径重新调用 [`spawn_download`]，KGet 检测已有文件
//!   自动续传（进度从已有比例继续）；
//! - 删除 = `abort()` + 上层移除任务记录（文件删除由上层决定）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use kget::KgetError;

use crate::options::EngineOptions;

/// 引擎事件（适配层统一事件，供上层更新任务进度字段与状态机）
#[derive(Debug, Clone, PartialEq)]
pub enum KgetEvent {
    /// 下载已启动（引擎线程开始执行）
    Started,
    /// 进度事件：`percent` 为完成百分比（0.0~100.0，**绝对进度**：
    /// 断点续传时从已有比例开始，而非从 0）；`speed` 为瞬时速度（字节/秒，
    /// **KGet 1.7 进度回调不提供实时速度，恒为 0**）
    Progress {
        percent: f64,
        speed: u64,
    },
    /// 下载成功完成（`path` 为输出文件路径）
    Finished {
        path: Option<String>,
    },
    /// 下载失败（`message` 为错误描述，见 [`map_kget_error`]）
    Failed {
        message: String,
    },
}

/// 引擎初始化错误（`spawn_download` 同步返回，不会启动后台线程）
#[derive(Debug, thiserror::Error)]
pub enum KgetInitError {
    /// KGet 引擎初始化失败（如代理配置 / HTTP 客户端构建失败）
    #[error("KGet 引擎初始化失败: {0}")]
    Init(String),
    /// 本地文件系统错误（输出目录创建失败等）
    #[error("本地文件系统错误: {0}")]
    Io(String),
}

/// 下载句柄：持有引擎线程与取消令牌，提供中止能力（暂停/删除用）
pub struct KgetHandle {
    /// 引擎线程句柄（`join` 等待线程结束，线程内不再向外抛错）
    join: Option<thread::JoinHandle<()>>,
    /// 取消令牌：`abort()` 置位后 KGet 在下一个读写检查点停止下载
    cancel: Arc<AtomicBool>,
    /// 引擎线程是否已退出（供 pause/remove 做"有限等待"，避免长时间阻塞）
    finished: Arc<AtomicBool>,
}

impl KgetHandle {
    /// 中止下载（暂停 / 删除用）
    ///
    /// 置位取消令牌，KGet 的 `AdvancedDownloader` 在读写循环中周期性检查，
    /// 尽快停止并保留已下载的部分文件（供恢复时 Range 续传）。
    /// 注意：引擎线程可能阻塞在网络读取（KGet 内部 reqwest 超时 300s），
    /// abort 后线程未必立即退出，请配合 [`KgetHandle::wait_exit`] 使用。
    pub fn abort(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 是否已请求中止
    pub fn is_aborted(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 有限等待引擎线程退出（最多等待 `timeout`，期间每 10ms 轮询一次）
    ///
    /// 引擎线程正常完成 / 失败 / 被中止后均会自行退出并置位 finished；
    /// 若线程卡在网络读取（如服务器迟迟不关闭连接），超时返回 `false`，
    /// 上层不应再 `join`（否则可能阻塞到 reqwest 的 300s 超时）。
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.finished.load(Ordering::Relaxed) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.finished.load(Ordering::Relaxed)
    }

    /// 阻塞等待引擎线程结束（上层清理用；被中止时线程也会自行退出，
    /// 但若线程卡在网络读取可能等待较久，建议优先使用 [`KgetHandle::wait_exit`]）
    pub fn join(mut self) -> thread::Result<()> {
        if let Some(handle) = self.join.take() {
            handle.join()
        } else {
            Ok(())
        }
    }
}

/// 启动一个下载任务
///
/// # 参数
/// - `task_gid`: 任务 gid（原样透传给 `on_event` 回调，供上层关联任务）
/// - `url`: 下载地址（HTTP/HTTPS/FTP 等 KGet 支持的协议）
/// - `options`: 引擎配置（见 [`EngineOptions`]，由 options.rs 从 aria2 选项映射）
/// - `on_event`: 事件回调，引擎线程内同步调用（线程安全；**回调需快速返回**，
///   避免阻塞下载进度）。事件序列：`Started` → `Progress`* → `Finished` 或 `Failed`
///
/// # 返回
/// - `Ok(KgetHandle)`：下载已在后台线程启动，可调用 `abort()` 中止、`join()` 等待
/// - `Err(KgetInitError)`：引擎初始化失败（输出目录无法创建、代理配置非法等），
///   此时未启动任何线程
pub fn spawn_download(
    task_gid: String,
    url: &str,
    options: &EngineOptions,
    on_event: impl Fn(&str, KgetEvent) + Send + 'static,
) -> Result<KgetHandle, KgetInitError> {
    // ── 1. 解析输出路径（dir/out；out 缺失时由 KGet 从 URL 推断文件名）──
    let filename = options
        .out
        .clone()
        .unwrap_or_else(|| kget::get_filename_from_url_or_default(url, "download"));
    let output = join_path(&options.dir, &filename);
    // KGet 的 AdvancedDownloader 不自动创建父目录，这里预先创建（init 期错误同步返回）
    if let Some(parent) = std::path::Path::new(&output).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| KgetInitError::Io(format!("创建输出目录失败: {e}")))?;
    }

    // ── 2. 构造代理 / 优化器配置 ──
    let proxy = make_proxy(options);
    let optimizer = make_optimizer(options);

    // ── 3. 准备引擎线程所需数据（纯函数 / 非 tokio 操作，在调用线程完成）──
    // 说明：KGet 的 AdvancedDownloader 构造（new + set_* 方法）会**内部创建 tokio
    // runtime**，而 Tauri command / JSON-RPC handler 均运行在 tokio 异步上下文中，
    // 若在此同步构造，KGet 在异步上下文中 drop runtime 会触发 panic
    // （tokio blocking/shutdown.rs: "Cannot drop a runtime in a context where
    // blocking is not allowed"），因此引擎构造整体移入引擎 std 线程（见 5. 注释）。
    let url = url.to_string();
    let output_for_thread = output.clone();
    // 取消令牌：abort() 置位后下载在下个检查点停止（句柄与引擎线程共享）
    let cancel = Arc::new(AtomicBool::new(false));
    // 自定义头（含 User-Agent 注入，KGet 1.7 无独立 UA 方法）
    let mut headers = options.headers.clone();
    if let Some(ua) = &options.user_agent {
        // 追加在自定义头之后；KGet 内置 UA 可能与其并存（reqwest append 行为），属库限制
        headers.push(("User-Agent".to_string(), ua.clone()));
    }
    // 外层重试参数（从 EngineOptions 提取，move 进线程）
    let max_tries = options.normalized_max_tries().max(1);
    let retry_wait = options.retry_wait;

    // ── 4. 事件回调桥接（on_event 为 Fn + Send，用 Mutex 包装保证 Sync）──
    let cb: Arc<Mutex<Box<dyn Fn(&str, KgetEvent) + Send>>> =
        Arc::new(Mutex::new(Box::new(on_event)));
    let cb_thread = cb.clone();
    let gid_for_thread = task_gid.clone();

    // ── 5. 启动引擎线程 ──
    // 关键：KGet 的 AdvancedDownloader 构造（new + set_* 方法）会**内部创建 tokio
    // runtime**，而 Tauri command / JSON-RPC handler 均运行在 tokio 异步上下文中，
    // 若在调用线程同步构造，KGet 在异步上下文中 drop runtime 会触发 panic
    // （tokio blocking/shutdown.rs: "Cannot drop a runtime in a context where
    // blocking is not allowed"）。因此把引擎构造与下载循环**全部放进引擎 std 线程**
    // （std 线程非 tokio 异步上下文，创建 / drop runtime 安全）。
    let finished = Arc::new(AtomicBool::new(false));
    let finished_thread = finished.clone();
    let cancel_engine = cancel.clone();

    let handle = thread::Builder::new()
        .name(format!("kget-{task_gid}"))
        .spawn(move || {
            let emit = |event: KgetEvent| {
                if let Ok(guard) = cb_thread.lock() {
                    guard(&gid_for_thread, event);
                }
            };

            // 构造引擎（init 失败经 Failed 事件上报，不 panic）
            let mut downloader = match kget::AdvancedDownloader::new(
                url,
                output_for_thread.clone(),
                true, // quiet：引擎自身不向 stdout 打印，进度统一经回调上报
                proxy,
                optimizer,
            ) {
                Ok(d) => d,
                Err(e) => {
                    emit(KgetEvent::Failed {
                        message: format!("KGet 引擎初始化失败: {e}"),
                    });
                    finished_thread.store(true, Ordering::Relaxed);
                    return;
                }
            };
            // 库调用必须显式设置 AlwaysResume：否则检测到已有部分文件时会阻塞 stdin 询问
            downloader.set_resume_policy(kget::ResumePolicy::AlwaysResume);
            downloader.set_cancel_token(cancel_engine.clone());
            downloader.set_extra_headers(headers);
            // 进度回调：p 为 0.0~1.0 绝对进度（含续传部分），转为 0.0~100.0
            let cb_progress = cb_thread.clone();
            let gid_for_progress = gid_for_thread.clone();
            downloader.set_progress_callback(move |p: f32| {
                let event = KgetEvent::Progress {
                    percent: p as f64 * 100.0,
                    speed: 0,
                };
                if let Ok(guard) = cb_progress.lock() {
                    guard(&gid_for_progress, event);
                }
            });
            // 设置 status 回调（消息本身忽略），以触发 AdvancedDownloader 下载完成后的
            // 文件大小完整性校验（metadata.len() != total_size 时报错）
            downloader.set_status_callback(|_msg: String| {
                // 状态消息（"Connecting…" 等）对上层无价值，忽略
            });

            emit(KgetEvent::Started);

            // 外层重试循环：KGet 内部已有每 chunk 3 次重试，
            // 这里按 aria2 max-tries / retry-wait 再兜底（胶水层逻辑）
            let mut attempt = 0u32;
            loop {
                match downloader.download() {
                    Ok(()) => {
                        emit(KgetEvent::Finished {
                            path: Some(output_for_thread.clone()),
                        });
                        break;
                    }
                    Err(e) => {
                        attempt += 1;
                        // 主动中止（暂停/删除）：保留部分文件，不视为失败
                        if cancel_engine.load(Ordering::Relaxed) {
                            break;
                        }
                        let msg = e.to_string();
                        // 不可恢复错误（404 / 校验和失败等）直接失败，避免无意义重试；
                        // 达到最大尝试次数也失败
                        if is_fatal_engine_error(&msg) || attempt >= max_tries {
                            emit(KgetEvent::Failed { message: msg });
                            break;
                        }
                        // 等待 retry-wait 秒后重试
                        if retry_wait > 0 {
                            thread::sleep(Duration::from_secs(retry_wait));
                        }
                    }
                }
            }
            // 线程即将退出：通知等待方（pause / remove 的有限等待）
            finished_thread.store(true, Ordering::Relaxed);
        })
        .map_err(|e| KgetInitError::Io(format!("启动引擎线程失败: {e}")))?;

    Ok(KgetHandle {
        join: Some(handle),
        cancel,
        finished,
    })
}

/// 探测 URL 的 Content-Length（仅用于任务元数据 totalLength，非下载算法）
///
/// 用 `std::net::TcpStream` 手写最小 HTTP 探测：
/// - **仅支持 http://**：https 需要 TLS 握手（本函数不引入 TLS 依赖），
///   直接返回 `Err`，由调用方 `unwrap_or(0)` 兜底为 0（任务仍可正常下载，
///   进度由 UI 按 percent 换算）；
/// - 流程：先发 `HEAD` 请求解析 `Content-Length`；若服务器不支持 HEAD
///   （405/403/501 等）或未返回 Content-Length，**重新建立连接**发
///   `Range: bytes=0-0` 的 GET，从 `206` 响应的 `Content-Range: bytes 0-0/{total}`
///   取总长度（不复用连接：服务器可能已关闭前一连接）；
/// - 连接 / 读写均设置 5 秒超时，任何网络异常返回 `Err`，绝不阻塞任务添加；
/// - 非 200/206 响应返回 `Ok(0)`（表示"未知"，不视为错误）。
pub fn probe_content_length(url: &str, options: &EngineOptions) -> Result<u64, String> {
    use std::io::Write;

    // 仅支持 http://（https 需要 TLS，直接返回错误由上层兜底为 0）
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        format!("probe_content_length 仅支持 http://（{url} 为 https，返回 0）")
    })?;
    // 解析 host[:port] 与 path（无 '/' 时 path 为 "/"）
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(idx) if authority[idx + 1..].parse::<u16>().is_ok() => {
            (&authority[..idx], authority[idx + 1..].parse::<u16>().unwrap())
        }
        _ => (authority, 80),
    };

    // 用户代理：优先 options.user_agent，否则使用默认 UA
    let ua = options
        .user_agent
        .clone()
        .unwrap_or_else(|| crate::config::CHROME_UA.to_string());

    // 第一步：新建连接发 HEAD 请求
    let mut stream = connect_http(host, port)?;
    let head = format!(
        "HEAD {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {ua}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("发送 HEAD 请求失败: {e}"))?;
    let response = read_response_head(&mut stream)?;
    let status = parse_status_code(&response);
    let mut length = parse_content_length(&response);

    // 第二步：HEAD 不支持（非 200/206）或未返回 Content-Length →
    // 重新连接发 Range GET 探测（服务器可能已关闭 HEAD 连接，必须新建连接）
    if !matches!(status, 200 | 206) || length.is_none() {
        let mut stream = connect_http(host, port)?;
        let get = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {ua}\r\nRange: bytes=0-0\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(get.as_bytes())
            .map_err(|e| format!("发送 GET 探测请求失败: {e}"))?;
        let response = read_response_head(&mut stream)?;
        let status = parse_status_code(&response);
        if status == 206 {
            // 206：从 Content-Range: bytes 0-0/{total} 取总长度
            length = parse_content_range_total(&response).or(length);
        } else if status == 200 {
            length = parse_content_length(&response).or(length);
        } else {
            // 其它状态（404 等）：未知，返回 0（不阻塞任务）
            return Ok(0);
        }
    }
    Ok(length.unwrap_or(0))
}

/// 建立到目标主机的 TCP 连接（5 秒连接 / 读写超时；直连，代理场景探活失败返回 Err）
fn connect_http(host: &str, port: u16) -> Result<std::net::TcpStream, String> {
    use std::net::{ToSocketAddrs, TcpStream};
    use std::time::Duration;
    // connect_timeout 需要 SocketAddr，先经 ToSocketAddrs 解析主机名
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("解析 {host}:{port} 失败: {e}"))?
        .next()
        .ok_or_else(|| format!("解析 {host}:{port} 无结果"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("连接 {host}:{port} 失败: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    Ok(stream)
}

/// 读取响应头（直到空行 `\r\n\r\n`，上限 64KB）
fn read_response_head(stream: &mut std::net::TcpStream) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // 头部结束（HTTP 头与正文以空行分隔）
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        // 防御：头部异常膨胀时放弃
        if buf.len() > 64 * 1024 {
            return Err("响应头超过 64KB".to_string());
        }
    }
    Ok(buf)
}

/// 解析响应状态码（"HTTP/1.1 200 OK" → 200；解析失败返回 0）
fn parse_status_code(response: &[u8]) -> u16 {
    let text = String::from_utf8_lossy(response);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let _ = parts.next(); // HTTP/1.1
    parts.next().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// 解析 Content-Length 头（未找到返回 None）
fn parse_content_length(response: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(response);
    text.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

/// 解析 Content-Range 的总长度（"bytes 0-0/12345" → 12345；未找到返回 None）
fn parse_content_range_total(response: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(response);
    text.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-range:") {
            // 形如 "bytes 0-0/12345"，取 '/' 之后的总长度
            let total = value.split('/').nth(1)?.trim();
            total.parse().ok()
        } else {
            None
        }
    })
}

/// 拼接输出路径（dir 与文件名，兼容尾部带不带分隔符，统一用正斜杠）
fn join_path(dir: &str, filename: &str) -> String {
    if dir.is_empty() {
        return filename.to_string();
    }
    if dir.ends_with('/') || dir.ends_with('\\') {
        format!("{dir}{filename}")
    } else {
        format!("{dir}/{filename}")
    }
}

/// 由 EngineOptions 构造 KGet 代理配置
///
/// `all-proxy` 支持 HTTP/HTTPS/SOCKS5（按 URL scheme 识别，与 KGet builder 的
/// `make_proxy` 判定一致）；代理认证透传 `all-proxy-user` / `all-proxy-passwd`。
fn make_proxy(options: &EngineOptions) -> kget::ProxyConfig {
    match options.all_proxy.as_deref() {
        Some(url) if !url.trim().is_empty() => kget::ProxyConfig {
            enabled: true,
            url: Some(url.to_string()),
            username: options.all_proxy_user.clone(),
            password: options.all_proxy_passwd.clone(),
            proxy_type: if url.starts_with("socks5://") || url.starts_with("socks5h://") {
                kget::ProxyType::Socks5
            } else if url.starts_with("https://") {
                kget::ProxyType::Https
            } else {
                kget::ProxyType::Http
            },
        },
        _ => kget::ProxyConfig::default(),
    }
}

/// 由 EngineOptions 构造 KGet 优化器（并行连接数 + 限速）
///
/// 限速取 `max-download-limit` / `max-overall-download-limit` 中非零较小值
/// （KGet 只有任务级 TokenBucket 聚合限速，无独立全局池）。
fn make_optimizer(options: &EngineOptions) -> kget::Optimizer {
    let mut cfg = kget::Config::default().optimization;
    cfg.speed_limit = options.effective_speed_limit();
    cfg.max_connections = (options.connections.max(1) as usize).clamp(1, 32);
    kget::Optimizer::from_config(cfg)
}

/// 判断引擎错误消息是否"不可恢复"（重试也无意义）
///
/// `AdvancedDownloader::download()` 返回 `Box<dyn Error>`，内部包裹的 KGet 错误
/// （`KgetError::NotFound` / `ChecksumMismatch` 等）只能经 Display 字符串识别，
/// 因此用消息启发式判断：404 / not found / checksum 均属服务器或数据本身问题。
fn is_fatal_engine_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("404")
        || lower.contains("not found")
        || lower.contains("checksum")
        || lower.contains("does not support range")
}

/// 错误映射：KGet 错误 → 人类可读错误消息（供 `KgetEvent::Failed` / 任务 errorMessage）
///
/// 语义尽量对齐 aria2 错误信息风格，便于前端与 RPC 契约层复用。
/// 说明：引擎线程的错误类型为 `Box<dyn Error>`（`AdvancedDownloader::download`
/// 的签名），实际运行时错误消息取 `e.to_string()`；本函数保留为 KgetError
/// 的语义化映射，供单元测试与后续需要结构化错误码的路径使用。
#[cfg_attr(not(test), allow(dead_code))]
fn map_kget_error(e: &KgetError) -> String {
    match e {
        KgetError::Network(msg) => format!("网络错误: {msg}"),
        KgetError::Io(err) => format!("本地 IO 错误: {err}"),
        KgetError::ChecksumMismatch {
            algorithm,
            expected,
            got,
        } => format!(
            "校验和不匹配（{algorithm}）: 期望 {expected}，实际 {got}"
        ),
        KgetError::Protocol(msg) => format!("协议错误: {msg}"),
        KgetError::Cancelled => "下载已取消".to_string(),
        KgetError::NotFound(url) => format!("资源不存在（HTTP 404）: {url}"),
        KgetError::SidecarError(msg) => format!("校验和文件错误: {msg}"),
        KgetError::Other(msg) => format!("下载失败: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // join_path / make_proxy / make_optimizer / map_kget_error 纯函数测试
    // ------------------------------------------------------------------
    #[test]
    fn join_path_handles_dir_separator() {
        assert_eq!(join_path("/downloads", "a.zip"), "/downloads/a.zip");
        assert_eq!(join_path("/downloads/", "a.zip"), "/downloads/a.zip");
        assert_eq!(join_path("C:\\Downloads", "a.zip"), "C:\\Downloads/a.zip");
        assert_eq!(join_path("", "a.zip"), "a.zip");
    }

    #[test]
    fn make_proxy_detects_scheme_and_auth() {
        let mut opts = EngineOptions::default();
        // 未配置代理 → 默认禁用
        let proxy = make_proxy(&opts);
        assert!(!proxy.enabled);

        opts.all_proxy = Some("socks5://127.0.0.1:1080".to_string());
        opts.all_proxy_user = Some("u".to_string());
        opts.all_proxy_passwd = Some("p".to_string());
        let proxy = make_proxy(&opts);
        assert!(proxy.enabled);
        assert!(matches!(proxy.proxy_type, kget::ProxyType::Socks5));
        assert_eq!(proxy.username.as_deref(), Some("u"));
        assert_eq!(proxy.password.as_deref(), Some("p"));

        // https 前缀 → Https；其它 → Http
        opts.all_proxy = Some("https://proxy:8443".to_string());
        assert!(matches!(
            make_proxy(&opts).proxy_type,
            kget::ProxyType::Https
        ));
        opts.all_proxy = Some("http://proxy:8080".to_string());
        assert!(matches!(make_proxy(&opts).proxy_type, kget::ProxyType::Http));
    }

    #[test]
    fn make_optimizer_applies_connections_and_speed_limit() {
        let mut opts = EngineOptions::default();
        opts.connections = 64; // KGet 内部 clamp 1~32
        opts.max_download_limit = Some(1024 * 1024);
        opts.max_overall_download_limit = Some(512 * 1024); // 取较小值
        let optimizer = make_optimizer(&opts);
        assert_eq!(optimizer.max_connections(), 32);
        assert_eq!(optimizer.speed_limit, Some(512 * 1024));

        // 无限速 → None
        opts.max_download_limit = None;
        opts.max_overall_download_limit = None;
        assert_eq!(make_optimizer(&opts).speed_limit, None);
    }

    #[test]
    fn map_kget_error_produces_readable_message() {
        assert_eq!(
            map_kget_error(&KgetError::NotFound("https://x/404".to_string())),
            "资源不存在（HTTP 404）: https://x/404"
        );
        assert_eq!(
            map_kget_error(&KgetError::Network("connection refused".to_string())),
            "网络错误: connection refused"
        );
        assert_eq!(map_kget_error(&KgetError::Cancelled), "下载已取消");
    }

    // ------------------------------------------------------------------
    // probe_content_length 的响应解析纯函数：状态码 / Content-Length / Content-Range
    // （真实 HTTP 探测的集成测试见 engine.rs 的测试模块）
    // ------------------------------------------------------------------
    #[test]
    fn parse_status_code_extracts_code() {
        assert_eq!(parse_status_code(b"HTTP/1.1 200 OK\r\n\r\n"), 200);
        assert_eq!(parse_status_code(b"HTTP/1.1 206 Partial Content\r\n\r\n"), 206);
        assert_eq!(parse_status_code(b"HTTP/1.1 404 Not Found\r\n\r\n"), 404);
        // 畸形响应 → 0
        assert_eq!(parse_status_code(b"garbage"), 0);
    }

    #[test]
    fn parse_content_length_extracts_value() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 12345\r\nConnection: close\r\n\r\n";
        assert_eq!(parse_content_length(resp), Some(12345));
        // 大小写不敏感 / 缺失返回 None
        assert_eq!(parse_content_length(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n"), Some(7));
        assert_eq!(parse_content_length(b"HTTP/1.1 200 OK\r\n\r\n"), None);
    }

    #[test]
    fn parse_content_range_total_extracts_total() {
        let resp = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/12345\r\n\r\n";
        assert_eq!(parse_content_range_total(resp), Some(12345));
        // 缺失返回 None
        assert_eq!(parse_content_range_total(b"HTTP/1.1 200 OK\r\n\r\n"), None);
    }

    #[test]
    fn probe_content_length_rejects_https() {
        // https 需要 TLS，本函数不支持 → 返回 Err（调用方 unwrap_or(0) 兜底）
        let opts = EngineOptions::default();
        assert!(probe_content_length("https://example.com/a.zip", &opts).is_err());
    }
}
