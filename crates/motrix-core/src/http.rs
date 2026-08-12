//! 顺序 HTTP 下载引擎（胶水层，支持可靠的断点续传）
//!
//! ## 为什么需要本模块（替代 KGet AdvancedDownloader 的 HTTP 路径）
//!
//! KGet 的 [`kget::AdvancedDownloader`] 采用"预分配整文件（`set_len(total)`）+
//! rayon 并行分块偏移写入"的模型，其断点续传依赖 **文件大小 = 已下载的连续前缀**
//! （`calculate_chunks(total, existing_size)` 从文件大小处继续分块）。但并行分块
//! 在**任意中断**后磁盘上并不是连续前缀（低编号 chunk 可能未完成、高编号 chunk
//! 反而先写满），此时：
//! - 直接恢复：KGet 认为 `[0, existing_size)` 有效而跳过 → 空洞被保留 → **文件损坏**；
//! - 删除重下（Phase 2 的兜底）：恢复从 0 开始 → **暂停/继续等于重下**（用户可见问题）。
//!
//! 本模块提供**单连接顺序下载**：数据始终从字节 0 连续写入磁盘。因此：
//! - 暂停时按已完成字节数截断文件（[`truncate_to`]），文件即有效的连续前缀；
//! - 恢复时发送 `Range: bytes={已有}-` 追加续传 → **任意暂停/恢复均正确**，
//!   满足迁移文档"断点续传有效"的验收要求。
//!
//! ## 取舍说明
//!
//! - 放弃了 KGet 的分块并行（单连接带宽利用），换取可靠的续传与文件正确性；
//!   限速 / 代理 / 自定义头 / UA / 重试语义与 KGet 路径保持一致；
//! - FTP / SFTP / WebDAV / Metalink 等非 HTTP(S) 协议仍走 KGet（本模块仅 HTTP(S)）；
//! - 服务器忽略 Range（返回 200）时回退为从头下载（先截断文件再顺序写入）。
//!
//! ## 线程模型
//!
//! 与 [`crate::kget::spawn_download`] 一致：reqwest blocking 客户端在**独立 std 线程**
//! 内运行（reqwest blocking 内部创建 tokio runtime，在 Tauri command / JSON-RPC 的
//! tokio 异步上下文中创建 / drop runtime 会 panic，故引擎构造与下载循环整体放入
//! std 线程）；事件经 `on_event` 回调（引擎线程内同步调用，须快速返回）。

use std::io::{Read, Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::kget::KgetEvent;
use crate::options::EngineOptions;

/// 顺序 HTTP 下载句柄：提供中止能力（暂停 / 删除用）
pub struct HttpDownloadHandle {
    /// 引擎线程句柄
    join: Option<thread::JoinHandle<()>>,
    /// 取消令牌：`abort()` 置位后下载在下个读取检查点停止
    cancel: Arc<AtomicBool>,
    /// 引擎线程是否已退出（供 pause/remove 做"有限等待"）
    finished: Arc<AtomicBool>,
}

impl HttpDownloadHandle {
    /// 中止下载（暂停 / 删除用）：置位取消令牌，读取循环尽快停止
    pub fn abort(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 是否已请求中止
    pub fn is_aborted(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 有限等待引擎线程退出（最多等待 `timeout`，期间每 10ms 轮询一次）
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

    /// 阻塞等待引擎线程结束（上层清理用）
    pub fn join(mut self) -> thread::Result<()> {
        if let Some(handle) = self.join.take() {
            handle.join()
        } else {
            Ok(())
        }
    }
}

/// 启动顺序 HTTP 下载（支持断点续传）
///
/// # 参数
/// - `gid`: 任务 gid（透传给 `on_event`）
/// - `url`: HTTP(S) 下载地址
/// - `options`: 引擎配置（UA / 代理 / 自定义头 / 重试 / 限速）
/// - `existing_bytes`: 已有字节数（暂停 / checkpoint 恢复后 >= 0；>0 时发
///   `Range: bytes={existing_bytes}-` 续传并追加写）
/// - `on_event`: 事件回调（Started → Progress* → Finished / Failed）
///
/// # 返回
/// - `Ok(HttpDownloadHandle)`：下载已在后台线程启动，可 `abort()` / `join()`
/// - `Err(String)`：初始化失败（输出目录无法创建等），未启动线程
pub fn spawn_http_download(
    gid: String,
    url: &str,
    options: &EngineOptions,
    existing_bytes: u64,
    on_event: impl Fn(&str, KgetEvent) + Send + 'static,
) -> Result<HttpDownloadHandle, String> {
    // ── 1. 解析输出路径（dir/out；out 缺失时从 URL 推断文件名）──
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
    // 预先创建输出目录（init 期错误同步返回）
    if let Some(parent) = std::path::Path::new(&output).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建输出目录失败: {e}"))?;
    }

    // ── 2. 准备线程数据（客户端构造移入引擎线程：reqwest blocking 内部创建
    //        tokio runtime，避免在 Tauri/JSON-RPC 的 tokio 异步上下文中创建）──
    let url = url.to_string();
    let output_thread = output;
    let cancel = Arc::new(AtomicBool::new(false));
    // 自定义头（含 User-Agent；服务器侧 UA 一致）
    let mut headers: Vec<(String, String)> = options.headers.clone();
    if let Some(ua) = &options.user_agent {
        headers.push(("User-Agent".to_string(), ua.clone()));
    }
    // 外层重试参数（与 KGet 路径一致：max-tries / retry-wait 由胶水层实现）
    let max_tries = options.normalized_max_tries().max(1);
    let retry_wait = options.retry_wait;
    let speed_limit = options.effective_speed_limit();
    let all_proxy = options.all_proxy.clone();
    let proxy_user = options.all_proxy_user.clone();
    let proxy_pass = options.all_proxy_passwd.clone();

    // ── 3. 事件回调桥接（Fn + Send，用 Mutex 包装保证 Sync）──
    let cb: Arc<std::sync::Mutex<Box<dyn Fn(&str, KgetEvent) + Send>>> =
        Arc::new(std::sync::Mutex::new(Box::new(on_event)));
    let cb_thread = cb.clone();
    let gid_thread = gid.clone();

    // ── 4. 启动引擎线程 ──
    let finished = Arc::new(AtomicBool::new(false));
    let finished_thread = finished.clone();
    let cancel_engine = cancel.clone();

    let handle = thread::Builder::new()
        .name(format!("http-{gid}"))
        .spawn(move || {
            let emit = |event: KgetEvent| {
                if let Ok(guard) = cb_thread.lock() {
                    guard(&gid_thread, event);
                }
            };

            // 构造 reqwest blocking 客户端（超时 / UA / 代理；禁止压缩以保持
            // Content-Length 与落盘字节一致）
            let mut builder = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(300))
                .connect_timeout(Duration::from_secs(20))
                .no_gzip()
                .no_deflate();
            if let Some(proxy_url) = all_proxy.as_deref() {
                if !proxy_url.trim().is_empty() {
                    let mut proxy = match reqwest::Proxy::all(proxy_url) {
                        Ok(p) => p,
                        Err(e) => {
                            emit(KgetEvent::Failed {
                                message: format!("构造代理配置失败: {e}"),
                            });
                            finished_thread.store(true, Ordering::Relaxed);
                            return;
                        }
                    };
                    if let (Some(u), Some(p)) = (proxy_user.as_deref(), proxy_pass.as_deref()) {
                        proxy = proxy.basic_auth(u, p);
                    }
                    builder = builder.proxy(proxy);
                }
            }
            let client = match builder.build() {
                Ok(c) => c,
                Err(e) => {
                    emit(KgetEvent::Failed {
                        message: format!("构建 HTTP 客户端失败: {e}"),
                    });
                    finished_thread.store(true, Ordering::Relaxed);
                    return;
                }
            };

            emit(KgetEvent::Started);

            // ── 5. 外层重试循环（aria2 max-tries / retry-wait 语义）──
            // 续传起点随每次尝试后的实际文件大小更新：若某次尝试下载了部分数据
            // 后失败（连接中断），下次重试从新的文件大小处 Range 续传，避免重复写。
            let mut current_existing = existing_bytes;
            let mut attempt = 0u32;
            loop {
                match run_once(
                    &client,
                    &url,
                    &output_thread,
                    &headers,
                    current_existing,
                    speed_limit,
                    &cancel_engine,
                    &emit,
                ) {
                    Ok(()) => {
                        emit(KgetEvent::Finished {
                            path: Some(output_thread.clone()),
                        });
                        break;
                    }
                    Err(e) => {
                        // 更新续传起点：以磁盘实际文件大小为准（本次尝试可能已写入部分数据）
                        current_existing = std::fs::metadata(&output_thread)
                            .map(|m| m.len())
                            .unwrap_or(current_existing);
                        attempt += 1;
                        // 主动中止（暂停/删除）：保留部分文件，不视为失败
                        if cancel_engine.load(Ordering::Relaxed) {
                            break;
                        }
                        // 不可恢复错误（404 等）直接失败，避免无意义重试
                        if is_fatal_error(&e) || attempt >= max_tries {
                            emit(KgetEvent::Failed { message: e });
                            break;
                        }
                        if retry_wait > 0 {
                            thread::sleep(Duration::from_secs(retry_wait));
                        }
                    }
                }
            }
            finished_thread.store(true, Ordering::Relaxed);
        })
        .map_err(|e| format!("启动 HTTP 下载线程失败: {e}"))?;

    Ok(HttpDownloadHandle {
        join: Some(handle),
        cancel,
        finished,
    })
}

/// 单次下载尝试：Range 续传（或从头）→ 顺序追加写 → 进度回调
fn run_once(
    client: &reqwest::blocking::Client,
    url: &str,
    output: &str,
    headers: &[(String, String)],
    existing_bytes: u64,
    speed_limit: Option<u64>,
    cancel: &Arc<AtomicBool>,
    emit: &dyn Fn(KgetEvent),
) -> Result<(), String> {
    // 发送请求前检查取消（暂停后恢复前的旧引擎线程可能尚未退出）
    if cancel.load(Ordering::Relaxed) {
        return Err("下载已取消".to_string());
    }
    // 1. 确定续传起点与写入模式（**先校验文件状态**，避免信任不可靠的 completed）：
    //    - existing_bytes > 0 且磁盘文件大小 == existing_bytes（顺序引擎暂停已截断）
    //      → 视为有效连续前缀，续传（Range 从 existing，追加写）；
    //    - existing_bytes > 0 但文件缺失 / 大小不一致（旧版删文件兜底、手动删除、
    //      截断失败等）→ 前缀不可信，从头下载（绝不 set_len 扩展出零字节假前缀）；
    //    - existing_bytes == 0 → 从头下载。
    let disk_size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    let resume = existing_bytes > 0 && disk_size == existing_bytes;
    let start_offset = if resume { existing_bytes } else { 0u64 };

    // 2. 发起请求：续传时带 Range；服务器忽略 Range（200）则从头重下
    let mut req = client.get(url);
    for (name, value) in headers {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            req = req.header(n, v);
        }
    }
    if start_offset > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={start_offset}-"));
    }
    let resp = req
        .send()
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!("HTTP 错误: {status}"));
    }

    // 3. 确认最终写入起点与总长
    //    206：服务端接受续传，起点 = start_offset，追加写
    //    200：服务端忽略 Range → 截断文件从头下载（起点 = 0）
    let (start_offset, append_mode) = if status == reqwest::StatusCode::PARTIAL_CONTENT {
        (start_offset, true)
    } else {
        // 200：Range 被忽略（服务器不支持/忽略），先截断旧数据避免混写
        let _ = std::fs::File::create(output)
            .map_err(|e| format!("创建输出文件失败: {e}"))?;
        (0u64, false)
    };
    // 总长：优先 Content-Range 的 total；其次 Content-Length + 起点
    let content_range_total = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split('/').nth(1))
        .and_then(|s| s.trim().parse::<u64>().ok());
    let response_len = resp.content_length().unwrap_or(0);
    let total = content_range_total.unwrap_or(start_offset.saturating_add(response_len));

    // 4. 打开输出文件（续传追加 / 从头创建）
    //    续传路径先把文件**对齐到续传起点**：暂停时上层已截断到 completed，
    //    但引擎线程可能残留写入（abort 后读检查点间隔内多写了几块）或截断失败，
    //    这里强制 set_len(start_offset) 再 seek 到末尾追加，避免追加错位 / 空洞。
    let mut file = if append_mode {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(output)
            .map_err(|e| format!("打开输出文件失败: {e}"))?;
        if f.metadata().map_err(|e| format!("读取文件大小失败: {e}"))?.len() != start_offset {
            f.set_len(start_offset)
                .map_err(|e| format!("对齐续传起点失败: {e}"))?;
        }
        f.seek(std::io::SeekFrom::End(0))
            .map_err(|e| format!("定位文件末尾失败: {e}"))?;
        f
    } else {
        std::fs::File::create(output).map_err(|e| format!("创建输出文件失败: {e}"))?
    };

    // 4. 顺序流式写入（16KB 缓冲，与 KGet 一致的内存友好读取；
    //    进度回调节流：每 ≥256KB 或本次会话结束才上报，避免高频事件压垮上层）
    let mut reader = resp;
    let mut buffer = [0u8; 16384];
    let mut written: u64 = 0;
    let mut last_emit: u64 = 0;
    let started_at = std::time::Instant::now();
    loop {
        // 每轮读取前检查取消（暂停/删除即时生效）
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let n = reader
            .read(&mut buffer)
            .map_err(|e| format!("读取响应流失败: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])
            .map_err(|e| format!("写入文件失败: {e}"))?;
        written += n as u64;

        // 限速：按"已写入字节 / 限速"的目标耗时 sleep
        if let Some(limit) = speed_limit {
            if limit > 0 {
                let expected = Duration::from_secs_f64(written as f64 / limit as f64);
                let actual = started_at.elapsed();
                if expected > actual {
                    thread::sleep(expected - actual);
                }
            }
        }

        // 进度回调（节流 ≥256KB）：绝对进度（含续传部分），与任务 totalLength 对齐
        if written.saturating_sub(last_emit) >= 256 * 1024 {
            let absolute = start_offset.saturating_add(written);
            let percent = if total > 0 {
                (absolute as f64 / total as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
            emit(KgetEvent::Progress {
                percent,
                speed: 0,
            });
            last_emit = written;
        }
    }
    // 本次会话最后再上报一次进度（保证暂停 / 结束时进度字段准确）
    {
        let absolute = start_offset.saturating_add(written);
        let percent = if total > 0 {
            (absolute as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        emit(KgetEvent::Progress {
            percent,
            speed: 0,
        });
    }
    // 取消时：**返回错误**（重试循环据此静默退出，绝不发 Finished）。
    // 关键：若返回 Ok，重试循环会 emit Finished → 上层 handle_finished 把
    // total 当 completed 直接标 Complete（未下载任何字节也"完成"）。
    if cancel.load(Ordering::Relaxed) {
        return Err("下载已取消".to_string());
    }
    // 未取消但响应提前结束（服务器断开）→ 视为失败由外层重试（已下载数据保留）
    if total > 0 && start_offset.saturating_add(written) < total {
        return Err("连接中断（已下载数据保留，可续传）".to_string());
    }
    Ok(())
}

/// 把输出文件截断到指定字节数（顺序下载的连续前缀，安全）
///
/// 供上层暂停时调用：KGet 预分配"假文件"问题在顺序引擎下不存在，
/// 文件即真实已下载的连续前缀，截断到 `bytes` 后恢复可 Range 续传。
pub fn truncate_to(path: &str, bytes: u64) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(bytes)
}

/// 判断错误是否"不可恢复"（404 等，重试无意义）
fn is_fatal_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("404")
        || lower.contains("not found")
        || lower.contains("checksum")
}

/// 从 URL 提取保存文件名（去掉 scheme 与 query：`scheme://host/path?q` → 取 path 的
/// 最后一个非空段；无路径段（仅 host / 以 / 结尾）时回退默认名）
pub(crate) fn file_name_from_url_or_default(url: &str, default: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    // 去掉 query（? 之后）与 fragment（# 之后），只保留路径部分
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    match path.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_extraction_works() {
        assert_eq!(
            file_name_from_url_or_default("https://a.com/f.iso", "d"),
            "f.iso"
        );
        assert_eq!(
            file_name_from_url_or_default("https://a.com/dir/f.iso?x=1", "d"),
            "f.iso"
        );
        // 仅 host（无路径段）或路径以 / 结尾 → 默认名
        assert_eq!(file_name_from_url_or_default("https://a.com", "d"), "d");
        assert_eq!(file_name_from_url_or_default("https://a.com/", "d"), "d");
    }
}
