//! 并行 HTTP 下载引擎（fast-down 5.0.2 集成）
//!
//! ## 背景
//!
//! KGet 的 [`crate::kget`] `AdvancedDownloader` 并行分块 + 整文件预分配
//! （`set_len(total)`），其断点续传依赖"文件大小 = 已下载的连续前缀"；并行下载
//! **任意中断**后该假设不成立（低编号 chunk 未完成、高编号 chunk 反而先写满）→
//! 恢复时跳过空洞 → **文件损坏**，或删文件重下 → **暂停/继续等于重下**（用户可见
//! 问题）。顺序引擎（[`crate::http`]）用单连接换取可靠续传，但放弃多连接带宽。
//!
//! 本模块以 fast-down 5.0.2（fast-pull 工作窃取并发核心 + reqwest 异步 HTTP）实现
//! **并行下载 + 字节级精确续传**：
//!
//! - [`fast_down::download_multi`] 的 `download_chunks`（待下载区间）由调用方提供，
//!   库本身不跟踪已完成块；
//! - 本模块消费事件链的 `Event::PushProgress`，用 [`fast_down::Merge`] 累积
//!   **已交给推送线程的区间**（`CacheSeqPusher` 顺序写 + abort 后 flush 保证区间
//!   真实落盘），并定期持久化到**侧车状态文件** `{输出}.fd.json`；
//! - 暂停（abort）：等推送线程 drain + flush 完成后，以 `clean` 标记持久化并截断
//!   文件到最大已写端点（省磁盘，空洞留在"剩余区间"中由恢复重下）；
//! - 恢复：读取侧车状态，校验 FileId（etag / last-modified）与文件长度一致后，
//!   [`fast_down::invert`] 求剩余区间重新并行下载；不一致（文件被删 / 内容变化 /
//!   崩溃残留的 `clean=false` 快照）则从头下载。
//!
//! 不预分配整文件（增长式写入器），避免 KGet `set_len(total)` 的磁盘瞬时占用与
//! 低磁盘空间启动即失败问题。
//!
//! ## 取舍
//!
//! - fast-down 不暴露 `max-tries`：分块拉取失败由其内部按 `retry_gap` 无限重试
//!   （等价 aria2 `max-tries=0`）；整段会话全灭（链关闭但区间未覆盖全量）报失败；
//! - 代理基本认证（`all-proxy-user` / `all-proxy-passwd`）fast-down 的
//!   `build_client` 不支持 → 启动即返回 Err，由上层回退顺序引擎（[`crate::http`]）；
//! - 服务器不支持 Range / 长度未知（chunked）：单连接顺序下载且不可续传（暂停
//!   后恢复从头），与 KGet 对该场景的行为一致。
//!
//! ## 线程模型
//!
//! 与 [`crate::http`] 一致：tokio 多线程 runtime 在**独立 std 线程**内创建并
//! `block_on`（fast-pull 的推送线程用 `spawn_blocking`，须多线程 runtime；在 Tauri
//! command / JSON-RPC 的 tokio 异步上下文中创建 / drop runtime 会 panic）；事件经
//! `on_event` 回调（runtime 内异步任务中调用，须快速返回）。

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use fast_down::{
    CacheSeqPusher, Event, FileId, Merge, ProgressEntry, Proxy, Pusher, invert,
};
use fast_down::fast_puller::{FastDownPuller, FastDownPullerOptions, build_client};
use fast_down::http::Prefetch;
use fast_down::multi::{DownloadOptions, download_multi};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

use crate::http::file_name_from_url_or_default;
use crate::kget::KgetEvent;
use crate::options::EngineOptions;

/// 侧车状态文件扩展名（追加在输出文件名后，如 `a.bin.fd.json`）
pub const SIDECAR_EXT: &str = ".fd.json";

/// 侧车状态：已完成区间 + 一致性校验信息（恢复的唯一权威依据）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FdState {
    /// 源 URL（校验：恢复时 URL 变化视为新任务）
    url: String,
    /// 总长度（校验：服务器长度变化视为内容变化）
    size: u64,
    /// 服务端 etag（`FileId` 一致性校验）
    etag: Option<String>,
    /// 服务端 last-modified（同上）
    last_modified: Option<String>,
    /// 已完成区间（`[start, end)`，已合并排序）
    completed: Vec<[u64; 2]>,
    /// 暂停时截断后的文件大小（恢复时校验磁盘文件长度）
    file_len: u64,
    /// 是否经"abort + flush + 截断"的正常暂停持久化；
    /// `false` 表示下载中的周期快照（崩溃残留时**不可信** → 从头下载）
    clean: bool,
}

/// fast-down 并行下载句柄：提供中止能力（暂停 / 删除用）
pub struct FastDownHandle {
    /// 引擎线程句柄
    join: Option<thread::JoinHandle<()>>,
    /// 取消令牌：`abort()` 置位
    cancel: Arc<AtomicBool>,
    /// 取消通知（引擎线程的 tokio 事件消费循环据此快速 abort download_multi）
    notify: Arc<tokio::sync::Notify>,
    /// 引擎线程是否已退出（供 pause/remove 做"有限等待"）
    finished: Arc<AtomicBool>,
}

impl FastDownHandle {
    /// 中止下载（暂停 / 删除用）：置位取消令牌并通知引擎线程
    pub fn abort(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.notify.notify_one();
    }

    /// 是否已请求中止
    pub fn is_aborted(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 有限等待引擎线程退出（最多等待 `timeout`，期间每 10ms 轮询一次）
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.finished.load(Ordering::Relaxed) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.finished.load(Ordering::Relaxed)
    }

    /// 阻塞等待引擎线程结束（上层清理用）
    pub fn join(mut self) -> thread::Result<()> {
        if let Some(handle) = self.join.take() {
            handle.join()
        } else {
            Ok(())
        }
    }
}

/// 启动 fast-down 并行下载（支持字节级断点续传）
///
/// # 参数
/// - `gid`: 任务 gid（透传给 `on_event`）
/// - `url`: HTTP(S) 下载地址
/// - `options`: 引擎配置（UA / 代理 / 自定义头 / 重试 / 限速 / 连接数）
/// - `on_event`: 事件回调（Started → Progress* → Finished / Failed）
///
/// # 返回
/// - `Ok(FastDownHandle)`：下载已在后台线程启动，可 `abort()` / `join()`
/// - `Err(String)`：初始化失败（输出目录无法创建 / 代理认证不支持等），
///   未启动线程；上层据此回退顺序引擎（[`crate::http`]）
pub fn spawn_fast_down_download(
    gid: String,
    url: &str,
    options: &EngineOptions,
    on_event: impl Fn(&str, KgetEvent) + Send + 'static,
) -> Result<FastDownHandle, String> {
    // ── 1. 解析输出路径（dir/out；out 缺失时从 URL 推断文件名，与 http.rs 一致）──
    let filename = options
        .out
        .clone()
        .unwrap_or_else(|| file_name_from_url_or_default(url, "download"));
    let output = if options.dir.is_empty() {
        filename
    } else if options.dir.ends_with('/') || options.dir.ends_with('\\') {
        format!("{}{}", options.dir, filename)
    } else {
        format!("{}/{}", options.dir, filename)
    };
    // 预先创建输出目录（init 期错误同步返回，供上层回退顺序引擎）
    if let Some(parent) = Path::new(&output).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建输出目录失败: {e}"))?;
    }
    // fast-down 的 build_client 不支持代理基本认证（reqwest Proxy 无注入点）：
    // 带认证代理的任务返回错误 → 上层回退顺序引擎（http.rs 支持 basic_auth）
    if options.all_proxy_user.is_some() || options.all_proxy_passwd.is_some() {
        return Err("fast-down 不支持代理认证（回退顺序引擎）".to_string());
    }

    // ── 2. 准备线程数据（客户端构造移入引擎线程内：fast-down 的 tokio runtime
    //        须在独立 std 线程创建，避免在 Tauri/JSON-RPC 异步上下文中创建/drop）──
    let url_thread = url.to_string();
    let output_thread = output;
    // 自定义头（含 User-Agent；服务器侧 UA 一致）
    let mut headers_thread = options.headers.clone();
    if let Some(ua) = &options.user_agent {
        headers_thread.push(("User-Agent".to_string(), ua.clone()));
    }
    let proxy_thread = options.all_proxy.clone();
    let speed_limit = options.effective_speed_limit();
    let connections = options.connections.clamp(1, 32) as usize;
    let retry_wait = options.retry_wait.max(1);

    // ── 3. 事件回调桥接（Fn + Send，用 Mutex 包装保证 Sync）──
    let cb: Arc<std::sync::Mutex<Box<dyn Fn(&str, KgetEvent) + Send>>> =
        Arc::new(std::sync::Mutex::new(Box::new(on_event)));
    let cb_thread = cb.clone();
    let gid_thread = gid.clone();

    // ── 4. 启动引擎线程 ──
    let cancel = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let cancel_engine = cancel.clone();
    let notify_engine = notify.clone();
    let finished_engine = finished.clone();

    let handle = thread::Builder::new()
        .name(format!("fd-{gid}"))
        .spawn(move || {
            let emit = |event: KgetEvent| {
                if let Ok(guard) = cb_thread.lock() {
                    guard(&gid_thread, event);
                }
            };
            // 多线程 runtime：fast-pull 的推送线程用 spawn_blocking，须多线程 runtime
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    emit(KgetEvent::Failed {
                        message: format!("创建 tokio runtime 失败: {e}"),
                    });
                    finished_engine.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let result = runtime.block_on(async_main(
                &url_thread,
                &output_thread,
                &headers_thread,
                proxy_thread.as_deref(),
                speed_limit,
                connections,
                retry_wait,
                &notify_engine,
                &emit,
            ));
            if let Err(e) = result {
                // 主动中止（暂停/删除）时静默退出（不视为失败，避免误发 Failed 事件；
                // 上层 pause/remove 已把状态迁移为 Paused / 移除）
                if !cancel_engine.load(Ordering::Relaxed) {
                    emit(KgetEvent::Failed { message: e });
                }
            }
            finished_engine.store(true, Ordering::Relaxed);
        })
        .map_err(|e| format!("启动 fast-down 下载线程失败: {e}"))?;

    Ok(FastDownHandle {
        join: Some(handle),
        cancel,
        notify,
        finished,
    })
}

/// 引擎线程主流程（把同步闭包形式的回调桥接到异步下载循环）
async fn async_main(
    url: &str,
    output: &str,
    headers: &[(String, String)],
    proxy_url: Option<&str>,
    speed_limit: Option<u64>,
    connections: usize,
    retry_wait: u64,
    notify: &Arc<tokio::sync::Notify>,
    emit: &dyn Fn(KgetEvent),
) -> Result<(), String> {
    run_download(
        url,
        output,
        headers,
        proxy_url,
        speed_limit,
        connections,
        retry_wait,
        notify,
        emit,
    )
    .await
}

/// 单次下载会话：prefetch → 侧车状态 → download_multi → 事件消费 → 收尾
async fn run_download(
    url: &str,
    output: &str,
    headers: &[(String, String)],
    proxy_url: Option<&str>,
    speed_limit: Option<u64>,
    connections: usize,
    retry_wait: u64,
    notify: &Arc<tokio::sync::Notify>,
    emit: &dyn Fn(KgetEvent),
) -> Result<(), String> {
    // ── 1. 构造请求头（自定义头 + UA；与 http.rs / KGet 对齐）──
    let mut header_map = HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            header_map.insert(n, v);
        }
    }

    // ── 2. prefetch：探测大小 / Range 支持 / FileId（etag、last-modified）──
    let parsed_url = Url::parse(url).map_err(|e| format!("解析 URL 失败: {e}"))?;
    let proxy: Proxy<&str> = match proxy_url {
        Some(p) if !p.trim().is_empty() => Proxy::Custom(p),
        _ => Proxy::System,
    };
    let client = build_client(&header_map, proxy, false, false, None)
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
    let (info, resp) = client
        .prefetch(parsed_url.clone())
        .await
        .map_err(|(e, _)| format!("探测 URL 失败: {e:?}"))?;
    // 服务器未提供长度（chunked 等）：不支持分块与续传，走单连接顺序下载
    let size = info.size;
    let supports_range = info.supports_range && size > 0;

    // ── 3. 侧车状态路径 ──
    let sidecar = state_path(output);

    // ── 4. 加载并校验侧车状态（决定是否续传及剩余区间）──
    let loaded = load_state(&sidecar);
    let resumeable = loaded.as_ref().is_some_and(|s| {
        s.clean
            && s.size == size
            && s.url == url
            && file_id_matches(s, &info.file_id)
            // 磁盘文件长度必须等于暂停时截断长度（文件被删 / 手动截断 → 状态无效）
            && std::fs::metadata(output)
                .map(|m| m.len() == s.file_len)
                .unwrap_or(false)
    });
    let (mut merged, remaining): (Vec<ProgressEntry>, Vec<ProgressEntry>) = if resumeable {
        // 续传：已完成区间还原 + invert 求剩余区间（window=0 精确到字节）
        let st = loaded.expect("已校验侧车状态存在");
        let completed: Vec<ProgressEntry> =
            st.completed.iter().map(|[s, e]| *s..*e).collect();
        let remaining: Vec<ProgressEntry> =
            invert(completed.iter().cloned(), size, 0).collect();
        (completed, remaining)
    } else {
        // 从头下载：清掉旧侧车（旧状态不可信）；文件截断到 0 在打开文件时做
        if loaded.is_some() {
            let _ = tokio::fs::remove_file(&sidecar).await;
        }
        (Vec::new(), vec![0..size])
    };

    // ── 5. 打开输出文件（续传：保留既有内容偏移写；从头：truncate 清空）──
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(!resumeable)
        .open(output)
        .await
        .map_err(|e| format!("打开输出文件失败: {e}"))?;

    // ── 6. 构建 Pusher：增长式顺序写入 + 限速 ──
    //     CacheSeqPusher 只把"从 0 开始的连续前缀"落盘，其余块在内存缓冲
    //     （high/low watermark 之间逐批 flush），保证磁盘内容与侧车区间一致；
    //     增长式写入器不预分配整文件（见模块文档）；
    //     限速在推送线程内 sleep 形成背压（推送队列满 → 执行器阻塞 → 网络停）。
    let growth = GrowthFilePusher::new(file, 64 * 1024)
        .await
        .map_err(|e| format!("初始化文件写入器失败: {e}"))?;
    let rate = RateLimitedPusher {
        inner: growth,
        limit: speed_limit,
        written: 0,
        start: Instant::now(),
    };
    let pusher = CacheSeqPusher::new(rate, 1024 * 1024, 512 * 1024);

    // ── 7. 构建 Puller（复用 prefetch 响应流，首块从 0 开始时不二次请求）──
    let puller = FastDownPuller::new(FastDownPullerOptions {
        url: parsed_url.clone(),
        headers: Arc::new(header_map),
        proxy: match proxy_url {
            Some(p) if !p.trim().is_empty() => Proxy::Custom(p),
            _ => Proxy::System,
        },
        accept_invalid_certs: false,
        accept_invalid_hostnames: false,
        file_id: info.file_id.clone(),
        resp: Some(Arc::new(parking_lot::Mutex::new(Some(resp)))),
        available_ips: Arc::new([]),
    })
    .map_err(|e| format!("初始化 fast-down puller 失败: {e}"))?;

    // ── 8. 启动并行下载（服务器不支持 Range → 单连接；支持 → 多连接分块）──
    let concurrent = if supports_range { connections.max(1) } else { 1 };
    emit(KgetEvent::Started);
    let result = download_multi(
        puller,
        pusher,
        DownloadOptions {
            download_chunks: remaining.into_iter(),
            concurrent,
            retry_gap: Duration::from_secs(retry_wait),
            pull_timeout: Duration::from_secs(30),
            push_queue_cap: 64,
            min_chunk_size: 256 * 1024,
            max_speculative: 4,
        },
    );

    // ── 9. 事件消费循环：累积已落盘区间 + 进度回调 + 周期持久化 + 取消检测 ──
    let chain = result.event_chain.clone();
    let mut last_persist = Instant::now();
    let mut last_emit = 0u64;
    let mut cancelled = false;
    loop {
        // 取消（暂停/删除）：abort 下载 + 释放 executor/队列（Drop 触发 abort），
        // 随后推送线程 drain 剩余队列并 flush 落盘，事件链随之关闭。
        let closed = tokio::select! {
            biased;
            _ = notify.notified() => {
                cancelled = true;
                false
            }
            ev = chain.recv() => match ev {
                Ok(Event::PullProgress(_, r)) | Ok(Event::PushProgress(_, r)) => {
                    // 区间合并（PushProgress 表示数据已交给推送线程，flush 后真实落盘）
                    merged.merge_progress(r);
                    false
                }
                Ok(_) => false,
                // 事件链关闭：正常完成 / abort 后推送线程已退出 / 全块失败
                Err(_) => true,
            },
        };
        if cancelled {
            // 暂停/删除：释放 DownloadResult（内部 abort 所有任务 + 释放队列），
            // 再排空事件链收集最终 PushProgress 并等待 flush 完成
            drop(result);
            break;
        }
        if closed {
            break;
        }
        // 进度回调（节流 ≥256KB；engine.rs 据此换算 completedLength 与速度）
        let downloaded = merged_total(&merged);
        let percent = if size > 0 {
            (downloaded as f64 / size as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        if downloaded.saturating_sub(last_emit) >= 256 * 1024 {
            emit(KgetEvent::Progress { percent, speed: 0 });
            last_emit = downloaded;
        }
        // 周期持久化侧车（每 2s；`clean=false` 快照仅作崩溃后"从头下载"的依据，
        // 不用于续传——未 flush 的数据无法保证在磁盘上）
        if last_persist.elapsed() >= Duration::from_secs(2) {
            let _ = persist_state(
                &sidecar,
                &make_state(url, size, &info.file_id, &merged, max_end(&merged), false),
            )
            .await;
            last_persist = Instant::now();
        }
    }

    // ── 10. 收尾 ──
    let downloaded = merged_total(&merged);
    if cancelled {
        // 暂停/删除：排空事件链收集最终区间 + 等待推送线程 flush 落盘
        // （带总时限兜底：推送线程在 flush 无限重试（如磁盘满）时不能永久阻塞）
        let mut chain_closed = false;
        let drain_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < drain_deadline {
            match tokio::time::timeout(Duration::from_millis(500), chain.recv()).await {
                Ok(Ok(Event::PullProgress(_, r))) | Ok(Ok(Event::PushProgress(_, r))) => {
                    merged.merge_progress(r);
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) => {
                    chain_closed = true;
                    break;
                }
                Err(_) => {}
            }
        }
        let end = max_end(&merged);
        // 最后上报一次进度（暂停后 UI 显示准确的 completedLength）
        let percent = if size > 0 {
            (merged_total(&merged) as f64 / size as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        emit(KgetEvent::Progress { percent, speed: 0 });
        if supports_range {
            // 可续传：截断文件到最大已写端点（省磁盘；空洞属剩余区间，恢复重下）
            if let Ok(f) = tokio::fs::OpenOptions::new().write(true).open(output).await {
                let _ = f.set_len(end).await;
            }
            // 仅当事件链确认关闭（flush 完成）才写 clean 标记；
            // 否则恢复时视为不可信 → 从头下载（不损坏文件）
            let _ = persist_state(
                &sidecar,
                &make_state(url, size, &info.file_id, &merged, end, chain_closed),
            )
            .await;
        } else {
            // 服务器不支持 Range：无法续传，删侧车避免恢复误判；保留部分文件
            let _ = tokio::fs::remove_file(&sidecar).await;
        }
        // 静默退出（上层 pause/remove 已处理状态迁移，不发 Finished/Failed）
        return Ok(());
    }

    // 未取消：事件链关闭即会话结束
    let complete = if size > 0 { downloaded >= size } else { true };
    if complete {
        // 下载完成：清理侧车（任务已完结，无需恢复依据）
        let _ = tokio::fs::remove_file(&sidecar).await;
        emit(KgetEvent::Finished {
            path: Some(output.to_string()),
        });
        Ok(())
    } else {
        // 链关闭但区间未覆盖全量：全部块拉取失败 / 拉取被拒，报失败
        Err("下载中断（部分分块失败，重试已耗尽）".to_string())
    }
}

// ======================================================================
// Pusher 实现
// ======================================================================

/// 增长式文件写入器：按区间偏移写入，文件随写入自然增长
///
/// 与 KGet 的 `StdFilePusher`（`set_len(total)` 预分配整文件）不同，本写入器
/// **不预分配**：既避免磁盘瞬时全量占用，也避免低磁盘空间时启动即失败；
/// 配合 `CacheSeqPusher` 的顺序写，文件大小 ≈ 已下载的连续前缀。
struct GrowthFilePusher {
    buf: std::io::BufWriter<std::fs::File>,
    /// 当前写位置（命中时跳过 seek，提升顺序写性能）
    pos: u64,
}

impl GrowthFilePusher {
    async fn new(file: tokio::fs::File, buffer_size: usize) -> std::io::Result<Self> {
        Ok(Self {
            buf: std::io::BufWriter::with_capacity(buffer_size, file.into_std().await),
            pos: 0,
        })
    }
}

impl Pusher for GrowthFilePusher {
    type Error = std::io::Error;

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }
        let start = range.start;
        if self.pos != start {
            if let Err(e) = self.buf.seek(SeekFrom::Start(start)) {
                self.pos = u64::MAX;
                return Err((e, bytes));
            }
            self.pos = start;
        }
        let mut remaining = &bytes[..];
        while !remaining.is_empty() {
            match self.buf.write(remaining) {
                Ok(0) => {
                    return Err((
                        std::io::Error::new(std::io::ErrorKind::WriteZero, "写入 0 字节"),
                        bytes,
                    ));
                }
                Ok(n) => {
                    self.pos += n as u64;
                    remaining = &remaining[n..];
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.pos = u64::MAX;
                    return Err((e, bytes));
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.buf.flush()
    }
}

/// 限速包装：在推送线程内按"已写字节 / 限速"目标时长 sleep 形成背压
///
/// 推送队列填满（`push_queue_cap`）后执行器阻塞在 `tx_push.send().await` → 网络
/// 读取停，实现平滑限速（与顺序引擎 http.rs 的限速语义一致）。
struct RateLimitedPusher<P: Pusher> {
    inner: P,
    /// 限速（字节/秒；`None` 或 0 表示不限速）
    limit: Option<u64>,
    /// 本次会话累计写入字节（限速计时基准，续传时从 0 重新累计）
    written: u64,
    /// 本次会话开始时刻
    start: Instant,
}

impl<P: Pusher> Pusher for RateLimitedPusher<P> {
    type Error = P::Error;

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        let len = bytes.len() as u64;
        self.inner.push(range, bytes)?;
        self.written += len;
        if let Some(limit) = self.limit {
            if limit > 0 {
                let expected = Duration::from_secs_f64(self.written as f64 / limit as f64);
                let actual = self.start.elapsed();
                if expected > actual {
                    thread::sleep(expected - actual);
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

// ======================================================================
// 侧车状态：读写 + 一致性校验
// ======================================================================

/// 侧车状态文件路径（`{输出}.fd.json`）
pub fn state_path(output: &str) -> String {
    format!("{output}{SIDECAR_EXT}")
}

/// 原子写侧车状态（临时文件 + rename，避免崩溃留下半写状态）
async fn persist_state(path: &str, state: &FdState) -> std::io::Result<()> {
    let json = serde_json::to_vec(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = format!("{path}.tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// 读取侧车状态（文件缺失 / 解析失败 → None）
fn load_state(path: &str) -> Option<FdState> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// FileId 一致性校验：etag 与 last-modified 均一致才认为服务端内容未变
fn file_id_matches(state: &FdState, file_id: &FileId) -> bool {
    state.etag.as_deref() == file_id.etag.as_deref()
        && state.last_modified.as_deref() == file_id.last_modified.as_deref()
}

/// 由合并区间构造侧车状态
fn make_state(
    url: &str,
    size: u64,
    file_id: &FileId,
    merged: &[ProgressEntry],
    file_len: u64,
    clean: bool,
) -> FdState {
    FdState {
        url: url.to_string(),
        size,
        etag: file_id.etag.as_deref().map(str::to_string),
        last_modified: file_id.last_modified.as_deref().map(str::to_string),
        completed: merged.iter().map(|r| [r.start, r.end]).collect(),
        file_len,
        clean,
    }
}

/// 合并区间总字节数（= 已下载字节数，用于进度百分比）
fn merged_total(merged: &[ProgressEntry]) -> u64 {
    merged
        .iter()
        .map(|r| r.end.saturating_sub(r.start))
        .sum()
}

/// 合并区间最大端点（暂停时截断文件到该位置；`merged` 已排序无重叠）
fn max_end(merged: &[ProgressEntry]) -> u64 {
    merged.last().map(|r| r.end).unwrap_or(0)
}

// ======================================================================
// 单元测试（区间合并 / invert / 侧车校验纯逻辑，不依赖网络）
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// fast_down::invert：已完成前缀 → 剩余区间
    #[test]
    fn invert_returns_remaining_ranges() {
        // 连续前缀 [0..100) → 剩余 [100..300)
        let completed: Vec<ProgressEntry> = vec![0..100];
        let remaining: Vec<ProgressEntry> = invert(completed.iter().cloned(), 300, 0).collect();
        assert_eq!(remaining, vec![100..300]);

        // 多个区间（含空洞）→ 精确求剩余（window=0 不吸收小区间）
        let completed: Vec<ProgressEntry> = vec![0..10, 20..30, 40..50];
        let remaining: Vec<ProgressEntry> = invert(completed.iter().cloned(), 100, 0).collect();
        assert_eq!(remaining, vec![10..20, 30..40, 50..100]);

        // 已全部完成 → 空剩余
        let completed: Vec<ProgressEntry> = vec![0..300];
        let remaining: Vec<ProgressEntry> = invert(completed.iter().cloned(), 300, 0).collect();
        assert!(remaining.is_empty());
    }

    /// merge_progress：乱序 / 相邻 / 重叠区间合并为排序无重叠列表
    #[test]
    fn merge_progress_merges_ranges() {
        let mut merged: Vec<ProgressEntry> = Vec::new();
        merged.merge_progress(20..30);
        merged.merge_progress(0..10);
        merged.merge_progress(10..20);
        merged.merge_progress(25..40); // 与 [20..30) 重叠合并
        assert_eq!(merged, vec![0..40]);
        merged.merge_progress(40..60);
        merged.merge_progress(80..90);
        assert_eq!(merged, vec![0..60, 80..90]);
        // 总字节数 = 区间长度之和
        assert_eq!(merged_total(&merged), 70);
        // 最大端点 = 最后一个区间 end
        assert_eq!(max_end(&merged), 90);
    }

    /// file_id_matches：etag / last-modified 一致才匹配
    #[test]
    fn file_id_matches_requires_same_etag_and_last_modified() {
        let state = FdState {
            url: "http://x/a.bin".into(),
            size: 100,
            etag: Some("\"abc\"".into()),
            last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".into()),
            completed: vec![[0, 50]],
            file_len: 50,
            clean: true,
        };
        // 完全一致 → 匹配
        assert!(file_id_matches(
            &state,
            &FileId::new(Some("\"abc\""), Some("Mon, 01 Jan 2024 00:00:00 GMT"))
        ));
        // etag 变化 → 不匹配
        assert!(!file_id_matches(
            &state,
            &FileId::new(Some("\"def\""), Some("Mon, 01 Jan 2024 00:00:00 GMT"))
        ));
        // last-modified 缺失（服务器不再下发）→ 不匹配
        assert!(!file_id_matches(&state, &FileId::new(Some("\"abc\""), None)));
    }

    /// 侧车状态序列化 / 反序列化往返（恢复路径的持久化基础）
    #[test]
    fn fd_state_roundtrip() {
        let state = FdState {
            url: "http://x/a.bin".into(),
            size: 1024 * 1024,
            etag: None,
            last_modified: None,
            completed: vec![[0, 1024], [2048, 4096]],
            file_len: 4096,
            clean: true,
        };
        let json = serde_json::to_vec(&state).expect("序列化失败");
        let back: FdState = serde_json::from_slice(&json).expect("反序列化失败");
        assert_eq!(back.url, state.url);
        assert_eq!(back.size, state.size);
        assert_eq!(back.completed, state.completed);
        assert_eq!(back.file_len, state.file_len);
        assert!(back.clean);
    }
}
