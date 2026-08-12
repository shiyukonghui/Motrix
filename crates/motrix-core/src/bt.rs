//! BitTorrent 引擎封装（Phase 3：librqbit 集成）
//!
//! 本模块把 librqbit（rqbit 客户端主库，Apache-2.0）封装为**同步 API**，
//! 供任务编排层 [`crate::engine::TaskManager`] 调用，不自研 BT 协议：
//!
//! - **线程模型**：内部自持一个多线程 tokio [`Runtime`]，librqbit 的
//!   [`Session`]（DHT / TCP 监听 / tracker 通信）运行在其上；对外方法一律
//!   同步（内部 `block_on`），与 TaskManager 的同步结构天然衔接；
//! - **进度轮询**：Runtime 上运行一个约 500ms 的轮询任务，周期读取各 torrent
//!   的 [`TorrentStats`]，经回调（捕获 `Arc<TaskManager>`）更新任务仓库字段；
//! - **磁力 metadata 阶段**：`add_magnet` 立即返回磁力预解析信息（info_hash /
//!   dn），TaskManager 据此创建"metadata 任务"（`bittorrent.info_name=None`，
//!   前端 `isMagnetTask` 判断依据）；轮询检测到 metadata 就绪后回调
//!   [`BtEvent::MetadataReady`] 补齐文件 / 总长 / announce / 名称；
//! - **暂停语义**：`session.pause` 停止 torrent 但保留 piece 状态，
//!   `session.unpause` 重新 start 续传（符合 Motrix 确认的暂停语义）；
//! - **peers 限制说明**：librqbit 8.1.1 未在公开 API 暴露 per-peer 明细
//!   （peer_id / per-peer bitfield / 瞬时速度），因此 [`BtEngine::get_peers`]
//!   保留 aria2 兼容字段结构与分页契约但当前返回空列表；聚合的 peer 数量
//!   经 [`BtProgress::num_seeders`]（live peers 数近似）上报。
//!
//! 接入参考：https://github.com/ikatson/rqbit 的 desktop/src-tauri/src/main.rs
//! （Apache-2.0，与本项目 MIT 兼容），版本锁定 `librqbit =8.1.1`（见 Cargo.toml）。

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, ManagedTorrent, Session, SessionOptions, TorrentMetadata,
    TorrentStats,
};
use tracing::warn;

/// bitfield 降采样目标宽度（字符数）：前端 TaskGraphic 按每字符生成一个
/// SVG atom（≤240 个 DOM 节点，根治 Electron 版大 bitfield 白屏，见 5.8 节）
pub const DEFAULT_BITFIELD_CHARS: usize = 240;

/// 进度轮询间隔（毫秒）
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 磁力后台添加的超时（librqbit resolve_magnet 等待 metadata 的上限；
/// 无 DHT / tracker 响应时兜底，避免后台线程永久挂起）
const MAGNET_ADD_TIMEOUT: Duration = Duration::from_secs(30);

/// BT 引擎会话级配置（由 EngineOptions 映射而来）
#[derive(Debug, Clone)]
pub struct BtEngineConfig {
    /// TCP 监听端口范围（`listen-port` 起始；范围取 100 端口避免多实例 / 测试并行冲突）
    pub listen_port_range: Option<std::ops::Range<u16>>,
}

/// 单个 BT 任务的添加选项（TaskManager::add_torrent 组装）
#[derive(Debug, Clone, Default)]
pub struct BtAddOptions {
    /// 保存目录
    pub dir: String,
    /// 附加 tracker 列表（`bt-tracker`）
    pub trackers: Vec<String>,
    /// 文件选择（0 起始索引，`select-file` 已转换）
    pub only_files: Option<Vec<usize>>,
    /// 添加后是否立即暂停
    pub paused: bool,
}

/// 磁力链接预解析信息（metadata 阶段用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetMeta {
    /// 40 位小写十六进制信息哈希
    pub info_hash: String,
    /// 磁力 display name（dn 参数，URL 解码后；无则 None）
    pub name: Option<String>,
}

/// 种子文件元信息（.torrent 解析 / magnet metadata 就绪后）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentMeta {
    /// 40 位小写十六进制信息哈希
    pub info_hash: String,
    /// 种子名称
    pub name: Option<String>,
    /// 下载模式：single（单文件）/ multi（多文件）
    pub mode: String,
    /// 总长度（字节）
    pub total_length: u64,
    /// 文件列表（相对路径，"/" 分隔）
    pub files: Vec<TorrentFileMeta>,
    /// announce 服务器列表（种子自带 + bt-tracker 合并后的实际生效列表）
    pub announce_list: Vec<String>,
}

/// 种子文件元信息（相对路径 + 长度）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFileMeta {
    /// 文件索引（从 1 开始，与 aria2 files[].index 对齐）
    pub index: usize,
    /// 相对路径（如 "sub/file.bin"）
    pub path: String,
    /// 文件长度（字节）
    pub length: u64,
}

/// BT 进度快照（轮询回调 → TaskManager 更新任务仓库字段）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtProgress {
    /// 已完成字节数
    pub completed: u64,
    /// 总字节数
    pub total: u64,
    /// 累计上传字节数（librqbit stats.uploaded_bytes）
    pub uploaded_bytes: u64,
    /// 下载速度（字节/秒）
    pub download_speed: u64,
    /// 上传速度（字节/秒）
    pub upload_speed: u64,
    /// 做种者数量（librqbit 不区分 seed/leech，用 live peers 数近似）
    pub num_seeders: u32,
    /// 是否做种中（下载已全部完成）
    pub seeder: bool,
    /// 降采样 bitfield（≤[`DEFAULT_BITFIELD_CHARS`] 个十六进制字符）
    pub bitfield: String,
    /// 每文件已完成字节（与任务 files 列表一一对应，同步 files[].completedLength）
    pub file_progress: Vec<u64>,
    /// 是否已下载完成（全部 piece 校验通过）
    pub finished: bool,
}

/// BT 引擎事件（经回调传给 TaskManager）
#[derive(Debug, Clone)]
pub enum BtEvent {
    /// metadata 就绪（磁力任务专用；.torrent 任务添加时即完整，不触发）
    MetadataReady(TorrentMeta),
    /// 进度更新
    Progress(BtProgress),
    /// 磁力添加失败（后台解析 metadata 失败 / magnet 无效；TaskManager 标记任务错误）
    AddFailed(String),
}

/// aria2 getPeers 兼容的 peer 信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// IP 地址
    pub ip: String,
    /// 端口
    pub port: u16,
    /// peer id（librqbit 8.1.1 未暴露，恒为空串）
    pub peer_id: String,
    /// peer 的 have bitfield（librqbit 8.1.1 未暴露，恒为空串）
    pub bitfield: String,
    /// 下载速度（字节/秒）
    pub download_speed: u64,
    /// 上传速度（字节/秒）
    pub upload_speed: u64,
}

/// 引擎内登记的一个 torrent（gid ↔ librqbit handle 映射）
struct TorrentEntry {
    /// 任务 gid（TaskManager 侧 id）
    gid: String,
    /// librqbit 托管 torrent 句柄
    handle: Arc<ManagedTorrent>,
    /// 事件回调（捕获 Arc<TaskManager>，轮询线程内调用）
    on_event: Arc<dyn Fn(BtEvent) + Send + Sync>,
    /// 磁力 metadata 是否已通知（只通知一次）
    metadata_notified: bool,
}

/// librqbit torrent 内部 id（usize 别名，见 librqbit session.rs `pub type TorrentId = usize`；
/// 未 re-export 到 crate 根，这里用原始 usize 类型）
type TorrentId = usize;

/// BitTorrent 引擎：内部自持 tokio Runtime + librqbit Session
///
/// 所有对外方法均为**同步**（内部 `block_on`）；轮询任务在 Runtime 上运行，
/// 通过 `Weak<BtEngine>` 访问本引擎（Arc 释放后轮询自动退出）。
pub struct BtEngine {
    /// 自持 tokio 运行时（librqbit 会话与轮询任务运行于此）
    rt: tokio::runtime::Runtime,
    /// librqbit 会话（DHT / TCP 监听 / tracker 通信）
    session: Arc<Session>,
    /// torrent 登记表（librqbit TorrentId → 条目）
    torrents: Mutex<HashMap<TorrentId, TorrentEntry>>,
}

impl BtEngine {
    /// 创建 BT 引擎（自持 Runtime + 启动 librqbit Session，并拉起进度轮询任务）
    ///
    /// `default_dir` 为会话级默认下载目录（每个 torrent 可用 `output_folder` 覆盖）；
    /// 创建失败（端口占用 / 配置非法）返回错误字符串，由 TaskManager 懒初始化并上报。
    pub fn new(config: BtEngineConfig, default_dir: impl Into<PathBuf>) -> Result<Arc<Self>, String> {
        // 1. 自持多线程 tokio Runtime（librqbit 为异步库，TaskManager 为同步结构，
        //    内部 block_on 桥接；2 个工作线程对 BT 下载足够，其余交给 librqbit 自身调度）
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("创建 BT 引擎运行时失败: {e}"))?;
        // 2. 初始化 librqbit Session：自定义 TCP 监听端口范围（listen-port），
        //    其余走默认（DHT 开启、会话级 trackers 为空——tracker 按任务级传递）
        let default_dir = default_dir.into();
        let session = rt
            .block_on(async {
                let opts = SessionOptions {
                    listen_port_range: config.listen_port_range,
                    ..Default::default()
                };
                Session::new_with_opts(default_dir, opts).await
            })
            .map_err(|e| format!("初始化 librqbit 会话失败: {e:#}"))?;
        let engine = Arc::new(Self {
            rt,
            session,
            torrents: Mutex::new(HashMap::new()),
        });
        // 3. 拉起进度轮询任务（约 500ms 一次，见 poll_once）
        engine.start_poll_loop();
        Ok(engine)
    }

    /// 在 Runtime 上启动进度轮询循环（持有 Weak 自引用，引擎释放后自动退出）
    fn start_poll_loop(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let _ = self.rt.spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let Some(this) = weak.upgrade() else {
                    // 引擎已被释放：结束轮询
                    return;
                };
                this.poll_once();
            }
        });
    }

    /// 添加磁力任务：**立即返回**磁力预解析信息（metadata 阶段用）
    ///
    /// 关键设计：librqbit 8.1.1 的 `Session::add_torrent` 对 magnet 会
    /// `resolve_magnet().await` **阻塞等待 metadata 获取成功**（DHT / tracker
    /// 无响应时可能长时间不返回）。因此这里把 librqbit 的 add 放到**后台 std 线程**
    /// 执行（`Runtime::Handle::block_on`），本方法同步返回预解析信息，
    /// TaskManager 先创建 metadata 任务（`bittorrent.info_name=None`）；
    /// metadata 就绪后由轮询回调 [`BtEvent::MetadataReady`] 补齐，失败回调
    /// [`BtEvent::AddFailed`]（TaskManager 标记任务错误）。
    pub fn add_magnet(
        self: &Arc<Self>,
        magnet: &str,
        opts: &BtAddOptions,
        gid: &str,
        on_event: impl Fn(BtEvent) + Send + Sync + 'static,
    ) -> Result<MagnetMeta, String> {
        // 预解析磁力链接（info_hash / dn 等）：同步、不依赖网络
        let parsed = parse_magnet(magnet).ok_or_else(|| format!("无效的磁力链接: {magnet}"))?;
        let add_opts = make_add_options(opts);
        // 后台线程执行 librqbit add（阻塞等待 metadata 期间不阻塞调用方）
        self.spawn_magnet_add(magnet.to_string(), add_opts, gid.to_string(), on_event);
        Ok(MagnetMeta {
            info_hash: parsed.info_hash,
            name: parsed.display_name,
        })
    }

    /// 在后台 std 线程执行 `session.add_torrent(AddTorrent::Url(magnet))`
    ///
    /// 完成（或失败）后经 `on_event` 回调；成功时把句柄登记进引擎（轮询随即接管
    /// metadata 就绪检测与进度更新）。线程持有 `Runtime::Handle`（非阻塞），
    /// 引擎 Drop 后 `block_on` 因 runtime 关闭立即返回错误，线程安全退出。
    /// **metadata 超时保护**：resolve_magnet 在无 DHT / tracker 响应的环境下可能
    /// 长期不返回，用 30 秒超时兜底（超时按添加失败回调，线程退出）。
    fn spawn_magnet_add(
        self: &Arc<Self>,
        magnet: String,
        add_opts: AddTorrentOptions,
        gid: String,
        on_event: impl Fn(BtEvent) + Send + Sync + 'static,
    ) {
        let this = self.clone();
        let session = self.session.clone();
        let handle = self.rt.handle().clone();
        std::thread::spawn(move || {
            let result = handle.block_on(async move {
                tokio::time::timeout(
                    MAGNET_ADD_TIMEOUT,
                    session.add_torrent(AddTorrent::Url(Cow::Owned(magnet.into())), Some(add_opts)),
                )
                .await
            });
            match result {
                Ok(Ok(resp)) => {
                    if let Some(torrent_handle) = resp.into_handle() {
                        this.register(&gid, torrent_handle, on_event);
                    } else {
                        on_event(BtEvent::AddFailed("磁力任务未返回托管句柄".to_string()));
                    }
                }
                Ok(Err(e)) => on_event(BtEvent::AddFailed(format!("添加磁力失败: {e:#}"))),
                Err(_) => on_event(BtEvent::AddFailed("磁力 metadata 获取超时".to_string())),
            }
        });
    }

    /// 添加 .torrent 内容（原始字节，TaskManager 侧已完成 base64 解码）：
    /// 立即返回完整元信息（文件 / 总长 / announce）
    pub fn add_torrent_file(
        &self,
        bytes: &[u8],
        opts: &BtAddOptions,
        gid: &str,
        on_event: impl Fn(BtEvent) + Send + Sync + 'static,
    ) -> Result<TorrentMeta, String> {
        let add_opts = make_add_options(opts);
        let handle = self.add_torrent_inner(AddTorrent::TorrentFileBytes(bytes.to_vec().into()), add_opts)?;
        // .torrent 自带完整 metadata：立即构造元信息返回（TaskManager 直接建完整任务）
        let meta = handle
            .with_metadata(|m| build_torrent_meta(&handle, m))
            .map_err(|e| format!("解析种子元信息失败: {e:#}"))?;
        // 注册时标记 metadata 已通知（.torrent 无需 MetadataReady 阶段）
        self.register_with_meta_notified(gid, handle, on_event);
        Ok(meta)
    }

    /// session.add_torrent 的公共封装（block_on + 错误映射）
    fn add_torrent_inner(
        &self,
        add: AddTorrent<'_>,
        opts: AddTorrentOptions,
    ) -> Result<Arc<ManagedTorrent>, String> {
        let session = self.session.clone();
        self.rt
            .block_on(async move {
                session
                    .add_torrent(add, Some(opts))
                    .await
                    .map_err(|e| format!("添加 BT 任务失败: {e:#}"))?
                    .into_handle()
                    .ok_or_else(|| "BT 任务未返回托管句柄".to_string())
            })
    }

    /// 登记 torrent（magnet：metadata 未通知；同一 TorrentId 已登记时以新 gid 接管）
    fn register(
        &self,
        gid: &str,
        handle: Arc<ManagedTorrent>,
        on_event: impl Fn(BtEvent) + Send + Sync + 'static,
    ) {
        let id = handle.id();
        let mut guard = self.torrents.lock().unwrap();
        if let Some(existing) = guard.get(&id) {
            if existing.gid != gid {
                warn!(
                    "[Motrix] BT 任务重复添加同一种子：gid {old} 的引擎登记由新任务 {new} 接管（info_hash 相同）",
                    old = existing.gid,
                    new = gid
                );
            }
        }
        guard.insert(
            id,
            TorrentEntry {
                gid: gid.to_string(),
                handle,
                on_event: Arc::new(on_event),
                metadata_notified: false,
            },
        );
    }

    /// 登记 torrent（.torrent：metadata 已就绪，无需 MetadataReady 通知）
    fn register_with_meta_notified(
        &self,
        gid: &str,
        handle: Arc<ManagedTorrent>,
        on_event: impl Fn(BtEvent) + Send + Sync + 'static,
    ) {
        let id = handle.id();
        let mut guard = self.torrents.lock().unwrap();
        if let Some(existing) = guard.get(&id) {
            if existing.gid != gid {
                warn!(
                    "[Motrix] BT 任务重复添加同一种子：gid {old} 的引擎登记由新任务 {new} 接管（info_hash 相同）",
                    old = existing.gid,
                    new = gid
                );
            }
        }
        guard.insert(
            id,
            TorrentEntry {
                gid: gid.to_string(),
                handle,
                on_event: Arc::new(on_event),
                metadata_notified: true,
            },
        );
    }

    /// 引擎中是否存在指定 gid 的 torrent（区分"暂停后恢复"与"checkpoint 恢复重加入"）
    pub fn contains(&self, gid: &str) -> bool {
        self.torrents
            .lock()
            .map(|g| g.values().any(|e| e.gid == gid))
            .unwrap_or(false)
    }

    /// 按 gid 查句柄
    fn handle_by_gid(&self, gid: &str) -> Result<Arc<ManagedTorrent>, String> {
        self.torrents
            .lock()
            .map_err(|e| format!("获取 BT 登记表锁失败: {e}"))?
            .values()
            .find(|e| e.gid == gid)
            .map(|e| e.handle.clone())
            .ok_or_else(|| format!("BT 任务不在引擎中: {gid}"))
    }

    /// 按 gid 查 librqbit TorrentId
    fn id_by_gid(&self, gid: &str) -> Result<TorrentId, String> {
        self.torrents
            .lock()
            .map_err(|e| format!("获取 BT 登记表锁失败: {e}"))?
            .values()
            .find(|e| e.gid == gid)
            .map(|e| e.handle.id())
            .ok_or_else(|| format!("BT 任务不在引擎中: {gid}"))
    }

    /// 暂停 BT 任务（librqbit session.pause：停止 torrent 但保留 piece 状态）
    pub fn pause(&self, gid: &str) -> Result<(), String> {
        let handle = self.handle_by_gid(gid)?;
        let session = self.session.clone();
        self.rt
            .block_on(async move {
                session
                    .pause(&handle)
                    .await
                    .map_err(|e| format!("暂停 BT 任务失败: {e:#}"))
            })
    }

    /// 恢复 BT 任务（librqbit session.unpause：重新 start，基于保留的 piece 续传）
    pub fn resume(&self, gid: &str) -> Result<(), String> {
        let handle = self.handle_by_gid(gid)?;
        let session = self.session.clone();
        self.rt
            .block_on(async move {
                session
                    .unpause(&handle)
                    .await
                    .map_err(|e| format!("恢复 BT 任务失败: {e:#}"))
            })
    }

    /// 移除 BT 任务（librqbit session.delete：**保留已下载文件**，与 aria2 remove 一致）；
    /// 同时从登记表移除（轮询不再更新该任务）
    pub fn remove(&self, gid: &str) -> Result<(), String> {
        let id = self.id_by_gid(gid)?;
        let session = self.session.clone();
        self.rt
            .block_on(async move {
                session
                    .delete(TorrentIdOrHash::Id(id), false)
                    .await
                    .map_err(|e| format!("移除 BT 任务失败: {e:#}"))
            })?;
        self.torrents
            .lock()
            .map_err(|e| format!("获取 BT 登记表锁失败: {e}"))?
            .remove(&id);
        Ok(())
    }

    /// 文件选择（aria2 select-file）：只下载指定索引（0 起始）的文件
    pub fn only_files(&self, gid: &str, indices: &[usize]) -> Result<(), String> {
        let handle = self.handle_by_gid(gid)?;
        let session = self.session.clone();
        let set: std::collections::HashSet<usize> = indices.iter().copied().collect();
        self.rt
            .block_on(async move {
                session
                    .update_only_files(&handle, &set)
                    .await
                    .map_err(|e| format!("更新文件选择失败: {e:#}"))
            })
    }

    /// 获取任务 peers（aria2.getPeers 兼容，limit 分页）
    ///
    /// **librqbit 8.1.1 公开 API 限制**：未暴露 per-peer 明细（peer_id / per-peer
    /// bitfield / 瞬时速度），无法枚举连接的 peers，故返回空列表（契约 / 分页逻辑保留）。
    /// 待 librqbit 上游提供 per-peer stats 或升级后在此填充真实数据。
    pub fn get_peers(&self, gid: &str, _limit: usize) -> Vec<PeerInfo> {
        let _ = gid;
        Vec::new()
    }

    /// 停止会话（退出前调用：优雅停止 DHT / 监听 / 各 torrent 任务）
    pub fn shutdown(&self) {
        let session = self.session.clone();
        self.rt.block_on(async move {
            session.stop().await;
        });
    }

    /// 单次轮询：遍历全部登记的 torrent，读取 stats → 构造事件 → 回调
    fn poll_once(&self) {
        // 先取快照（不持锁调用 librqbit，避免长时间持锁阻塞 add/remove）
        let entries: Vec<(TorrentId, String, Arc<ManagedTorrent>, Arc<dyn Fn(BtEvent) + Send + Sync>, bool)> =
            match self.torrents.lock() {
                Ok(guard) => guard
                    .iter()
                    .map(|(id, e)| {
                        (
                            *id,
                            e.gid.clone(),
                            e.handle.clone(),
                            e.on_event.clone(),
                            e.metadata_notified,
                        )
                    })
                    .collect(),
                Err(e) => {
                    warn!("[Motrix] BT 轮询获取登记表锁失败: {e}");
                    return;
                }
            };
        for (id, _gid, handle, on_event, metadata_notified) in entries {
            // 1) metadata 阶段（磁力任务）：就绪后回调一次 MetadataReady
            if !metadata_notified {
                if let Ok(meta) = handle.with_metadata(|m| build_torrent_meta(&handle, m)) {
                    on_event(BtEvent::MetadataReady(meta));
                    if let Ok(mut guard) = self.torrents.lock() {
                        if let Some(e) = guard.get_mut(&id) {
                            e.metadata_notified = true;
                        }
                    }
                }
            }
            // 2) 进度阶段：读取 stats 构造 BtProgress 并回调
            let stats = handle.stats();
            let progress = build_progress(&handle, &stats);
            if let Some(p) = progress {
                on_event(BtEvent::Progress(p));
            }
        }
    }
}

impl Drop for BtEngine {
    fn drop(&mut self) {
        // 优雅停止 librqbit 会话（DHT / 监听 / torrent 任务）；Runtime 随本结构释放
        let session = self.session.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.rt.block_on(async move {
                session.stop().await;
            });
        }));
    }
}

// ======================================================================
// 纯函数（便于单元测试，不依赖 librqbit 运行环境）
// ======================================================================

/// 把任务级选项组装为 librqbit 的 AddTorrentOptions
fn make_add_options(opts: &BtAddOptions) -> AddTorrentOptions {
    AddTorrentOptions {
        paused: opts.paused,
        output_folder: Some(opts.dir.clone()),
        only_files: opts.only_files.clone(),
        // bt-tracker 列表（种子自带 announce 之外追加的 tracker）
        trackers: if opts.trackers.is_empty() {
            None
        } else {
            Some(opts.trackers.clone())
        },
        // 续传（checkpoint 恢复 / 暂停恢复）需要允许写入已存在文件（librqbit 文档建议设置）
        overwrite: true,
        ..Default::default()
    }
}

/// 从 librqbit TorrentMetadata 构造 motrix 侧种子元信息（info_hash / 文件 / announce）
fn build_torrent_meta(handle: &Arc<ManagedTorrent>, meta: &TorrentMetadata) -> TorrentMeta {
    // 单文件 / 多文件模式：由文件数判定（对应 aria2 bittorrent.mode）
    let mode = if meta.file_infos.len() > 1 { "multi" } else { "single" };
    let files = meta
        .file_infos
        .iter()
        .enumerate()
        .map(|(i, f)| TorrentFileMeta {
            index: i + 1,
            path: f.relative_filename.to_string_lossy().to_string(),
            length: f.len,
        })
        .collect();
    // announce：取实际生效的 tracker 列表（种子自带 announce + 任务级 bt-tracker 合并，
    // librqbit 在 ManagedTorrentShared.trackers 中归一化保存）
    let announce_list: Vec<String> = handle
        .shared()
        .trackers
        .iter()
        .map(|t| t.to_string())
        .collect();
    TorrentMeta {
        info_hash: handle.info_hash().as_string(),
        name: meta.name.clone(),
        mode: mode.to_string(),
        total_length: meta.lengths.total_length(),
        files,
        announce_list,
    }
}

/// 读取 torrent 统计并构造进度快照（live 为 None 时速度 / peers 归零）
fn build_progress(handle: &Arc<ManagedTorrent>, stats: &TorrentStats) -> Option<BtProgress> {
    // 速度：librqbit 的 Speed 字段为 MiB/s（bps/1024/1024），换算回字节/秒
    let (download_speed, upload_speed) = stats
        .live
        .as_ref()
        .map(|l| {
            (
                (l.download_speed.mbps * 1024.0 * 1024.0) as u64,
                (l.upload_speed.mbps * 1024.0 * 1024.0) as u64,
            )
        })
        .unwrap_or((0, 0));
    // 做种者数：librqbit 不区分 seed/leech，用 live peers 数近似（aria2 numSeeders 语义近似）
    let num_seeders = stats
        .live
        .as_ref()
        .map(|l| l.snapshot.peer_stats.live as u32)
        .unwrap_or(0);
    // bitfield：按 metadata 的 piece 长度 / 文件区间与每文件已下载字节估算 piece 完成标志，
    // 再降采样为 ≤240 字符的十六进制串（前端 TaskGraphic 按字符渲染进度）
    let bitfield = handle
        .with_metadata(|m| {
            let flags = compute_piece_flags_from_meta(m, &stats.file_progress);
            downsample_bitfield(&flags, DEFAULT_BITFIELD_CHARS)
        })
        .unwrap_or_default();
    Some(BtProgress {
        completed: stats.progress_bytes,
        total: stats.total_bytes,
        uploaded_bytes: stats.uploaded_bytes,
        download_speed,
        upload_speed,
        num_seeders,
        seeder: stats.finished,
        bitfield,
        file_progress: stats.file_progress.clone(),
        finished: stats.finished,
    })
}

/// 从种子元信息 + 每文件进度估算 piece 完成标志（build_progress 用）
fn compute_piece_flags_from_meta(meta: &TorrentMetadata, file_progress: &[u64]) -> Vec<bool> {
    let total_pieces = meta.lengths.total_pieces() as usize;
    let piece_len = meta.lengths.default_piece_length() as u64;
    // 文件区间：(offset_in_torrent, len)
    let ranges: Vec<(u64, u64)> = meta
        .file_infos
        .iter()
        .map(|f| (f.offset_in_torrent, f.len))
        .collect();
    compute_piece_flags(&ranges, file_progress, total_pieces, piece_len)
}

/// 根据每个文件的（起始偏移, 长度）与已下载字节数，估算每个 piece 的完成标志
///
/// 判定规则：piece 覆盖的每个文件的对应字节区间都已下载（`file_progress[fi]` 达到
/// 该区间终点相对文件起点的偏移），则该 piece 视为完成。piece 跨越文件边界时
/// 要求所有覆盖文件的对应字节都完成（保守近似）。
pub fn compute_piece_flags(
    file_ranges: &[(u64, u64)],
    file_progress: &[u64],
    total_pieces: usize,
    piece_len: u64,
) -> Vec<bool> {
    let mut flags = Vec::with_capacity(total_pieces);
    for p in 0..total_pieces {
        let p_start = p as u64 * piece_len;
        let p_end = p_start + piece_len;
        let mut done = true;
        for (fi, &(f_start, f_len)) in file_ranges.iter().enumerate() {
            let f_end = f_start + f_len;
            // 该文件不覆盖此 piece：跳过
            if f_end <= p_start || f_start >= p_end {
                continue;
            }
            // 该文件在此 piece 内的区间终点（相对文件起点 = 需要完成的字节数）
            let covered_end = f_end.min(p_end);
            let need = covered_end - f_start;
            if file_progress.get(fi).copied().unwrap_or(0) < need {
                done = false;
                break;
            }
        }
        flags.push(done);
    }
    flags
}

/// 把每 piece 完成标志聚合为 ≤`target_chars`（默认 240）个十六进制字符的 bitfield
///
/// 映射规则：piece 按目标宽度均分成桶，桶内完成比例 `r` → 字符值 `min(15, floor(r*16))`；
/// 前端 `Math.floor(parseInt(ch,16)/4)` 还原 0~3 的完成状态（0=0%, 4=25%, 8=50%,
/// 12=75%, 15=100%），即状态 ≈ `floor(r*4)`。全完成 → 全 `f`（状态 3）、
/// 全未完成 → 全 `0`（状态 0）。**硬约束：输出长度 ≤ target_chars**。
pub fn downsample_bitfield(piece_flags: &[bool], target_chars: usize) -> String {
    if piece_flags.is_empty() || target_chars == 0 {
        return String::new();
    }
    let buckets = target_chars.min(piece_flags.len());
    let n = piece_flags.len();
    let mut out = String::with_capacity(buckets);
    for b in 0..buckets {
        let start = b * n / buckets;
        let end = (b + 1) * n / buckets;
        // 桶边界安全：start < end 恒成立（buckets ≤ n），防御性处理
        let value = if start < end {
            let done = piece_flags[start..end].iter().filter(|f| **f).count();
            let ratio = done as f64 / (end - start) as f64;
            (ratio * 16.0).floor().min(15.0) as u32
        } else {
            0
        };
        // char::from_digit(15, 16) = 'f'（小写十六进制，aria2 惯例小写）
        out.push(char::from_digit(value, 16).unwrap_or('0'));
    }
    out
}

/// 磁力链接预解析（`magnet:?xt=urn:btih:<40hex>&dn=...&tr=...`）
///
/// 提取：40 位十六进制 info_hash（小写）、`dn` 显示名（percent 解码）、
/// `tr` tracker 列表（percent 解码）。解析失败返回 None（非磁力 / 缺 info_hash）。
pub fn parse_magnet(link: &str) -> Option<MagnetInfo> {
    let rest = link.strip_prefix("magnet:?")?;
    let mut info_hash: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut trackers: Vec<String> = Vec::new();
    for param in rest.split('&') {
        let (key, value) = param.split_once('=').unwrap_or((param, ""));
        match key {
            "xt" => {
                // 仅接受 btih（BitTorrent Info Hash）；40 位十六进制
                if let Some(hash) = value.strip_prefix("urn:btih:") {
                    if hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                        info_hash = Some(hash.to_ascii_lowercase());
                    }
                }
            }
            "dn" => display_name = Some(percent_decode(value)),
            "tr" => {
                let decoded = percent_decode(value);
                if !decoded.is_empty() {
                    trackers.push(decoded);
                }
            }
            _ => {}
        }
    }
    Some(MagnetInfo {
        info_hash: info_hash?,
        display_name: display_name.filter(|s| !s.is_empty()),
        trackers,
    })
}

/// 磁力链接解析结果（parse_magnet 返回）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetInfo {
    /// 40 位小写十六进制 info_hash
    pub info_hash: String,
    /// dn 参数（显示名，percent 解码后）
    pub display_name: Option<String>,
    /// tr 参数（tracker 列表，percent 解码后）
    pub trackers: Vec<String>,
}

/// 简易 percent 解码（`%XX` → 字节；`+` 保持原样不转空格，与 URI 语义一致）
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 单个十六进制字符 → 数值（0~15；非法字符返回 None）
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// base64 解码 .torrent 内容（aria2.addTorrent 的 params[0] 契约：标准 base64）
pub fn decode_torrent_base64(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let input = input.trim();
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|e| format!("base64 解码种子内容失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    // ------------------------------------------------------------------
    // downsample_bitfield：长度约束 / 还原状态 / 全完成 / 全未完成
    // ------------------------------------------------------------------

    /// 把 hex 字符还原为 0~3 的完成状态（等价前端 Math.floor(parseInt(ch,16)/4)）
    fn char_state(ch: char) -> u32 {
        ch.to_digit(16).unwrap() / 4
    }

    #[test]
    fn downsample_bitfield_length_bounded() {
        // 大量 piece（10 万）→ 输出长度不超过 240
        let flags: Vec<bool> = (0..100_000).map(|i| i % 3 == 0).collect();
        let s = downsample_bitfield(&flags, DEFAULT_BITFIELD_CHARS);
        assert!(s.len() <= DEFAULT_BITFIELD_CHARS, "输出长度 {} 超过 240", s.len());
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn downsample_bitfield_all_done_and_all_pending() {
        // 全完成：每桶比例 1.0 → 字符值 15 → 还原状态 3
        let done = vec![true; 1000];
        let s = downsample_bitfield(&done, 240);
        assert!(s.chars().all(|c| char_state(c) == 3), "全完成应为全 3 状态: {s}");
        assert!(s.chars().all(|c| c == 'f'), "全完成应为全 f: {s}");
        // 全未完成：每桶比例 0 → 字符值 0 → 还原状态 0
        let pending = vec![false; 1000];
        let s = downsample_bitfield(&pending, 240);
        assert!(s.chars().all(|c| char_state(c) == 0), "全未完成应为全 0 状态: {s}");
        assert!(s.chars().all(|c| c == '0'), "全未完成应为全 0: {s}");
    }

    #[test]
    fn downsample_bitfield_state_matches_bucket_ratio() {
        // roundtrip：每桶还原的 0~3 状态应与桶内完成度 floor(r*4) 一致
        // 构造 240 pieces、80 桶（3 pieces/桶），桶内完成数 0..=3
        let n = 240usize;
        let buckets = 80usize;
        let flags: Vec<bool> = (0..n).map(|i| {
            // 每 3 个 piece 一组，组内完成数 = 组号 % 4
            let group = i / 3;
            (i % 3) < (group % 4)
        })
        .collect();
        let s = downsample_bitfield(&flags, buckets);
        assert_eq!(s.len(), buckets);
        for (b, ch) in s.chars().enumerate() {
            // 桶内完成数 = b % 4（见 flags 构造），比例 r = (b%4)/3
            let r = (b % 4) as f64 / 3.0;
            let expected_state = (r * 4.0).floor().min(3.0) as u32;
            assert_eq!(char_state(ch), expected_state, "桶 {b} 状态不匹配");
        }
    }

    #[test]
    fn downsample_bitfield_empty_inputs() {
        assert_eq!(downsample_bitfield(&[], 240), "");
        assert_eq!(downsample_bitfield(&[true; 10], 0), "");
    }

    // ------------------------------------------------------------------
    // compute_piece_flags：按文件区间 + 已完成字节估算 piece 完成标志
    // ------------------------------------------------------------------

    #[test]
    fn compute_piece_flags_marks_completed_pieces() {
        // 两个文件：文件 0 占 [0, 60)，文件 1 占 [60, 100)；piece 长 20 → 5 个 piece
        let ranges = [(0u64, 60u64), (60u64, 40u64)];
        // 文件 0 完成 60 字节、文件 1 完成 40 字节 → 全部完成
        let flags = compute_piece_flags(&ranges, &[60, 40], 5, 20);
        assert_eq!(flags, vec![true, true, true, true, true]);
        // 文件 0 只完成 50 字节（piece 2 = [40,60) 需要文件 0 到 60，未满）；
        // piece 3/4 由文件 1（[60,100)，已完成 40）覆盖 → 完成
        let flags = compute_piece_flags(&ranges, &[50, 40], 5, 20);
        assert_eq!(flags, vec![true, true, false, true, true]);
        // 文件 1 只完成 10 字节（piece 3 需要 20、piece 4 需要 40，均未满）→ 只有前 3 个完成
        let flags = compute_piece_flags(&ranges, &[60, 10], 5, 20);
        assert_eq!(flags, vec![true, true, true, false, false]);
    }

    // ------------------------------------------------------------------
    // parse_magnet：info_hash / dn / tr 提取
    // ------------------------------------------------------------------

    #[test]
    fn parse_magnet_extracts_fields() {
        // 40 位 info_hash（6 组 C0FFEE + ABCD；大写输入应转为小写）
        let hash_upper = "C0FFEEC0FFEEC0FFEEC0FFEEC0FFEEC0FFEEABCD";
        assert_eq!(hash_upper.len(), 40, "测试用 info_hash 应为 40 位");
        let m = parse_magnet(&format!(
            "magnet:?xt=urn:btih:{hash_upper}&dn=ubuntu-24.04&tr=udp%3A%2F%2Ftracker1%3A80%2Fannounce"
        ))
        .expect("合法磁力应解析成功");
        assert_eq!(
            m.info_hash,
            "c0ffeec0ffeec0ffeec0ffeec0ffeec0ffeeabcd" // 6 组 c0ffee + abcd（40 字符）
        );
        assert_eq!(m.display_name.as_deref(), Some("ubuntu-24.04"));
        // percent 解码（%3A → ':'、%2F → '/'）
        assert_eq!(
            m.trackers,
            vec!["udp://tracker1:80/announce".to_string()]
        );
    }

    #[test]
    fn parse_magnet_rejects_invalid() {
        // 非磁力 / 缺 xt / info_hash 长度不对 → None
        assert!(parse_magnet("https://example.com").is_none());
        assert!(parse_magnet("magnet:?dn=no-hash").is_none());
        assert!(parse_magnet("magnet:?xt=urn:btih:short").is_none());
        // 非法 hex 字符
        assert!(parse_magnet("magnet:?xt=urn:btih:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_none());
    }

    // ------------------------------------------------------------------
    // decode_torrent_base64：合法 / 非法输入
    // ------------------------------------------------------------------

    #[test]
    fn decode_torrent_base64_roundtrip() {
        let raw = b"d8:announce0:e".to_vec(); // 最小可解析的 bencode 占位（仅验证 base64 层）
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let decoded = decode_torrent_base64(&encoded).expect("base64 解码应成功");
        assert_eq!(decoded, raw);
        // 非法 base64 → 错误（base64 库对空串返回 Ok(空)，故仅断言非法字符报错）
        assert!(decode_torrent_base64("not base64!!!").is_err());
        assert_eq!(
            decode_torrent_base64("").expect("空串解码应成功"),
            Vec::<u8>::new()
        );
    }

    // ------------------------------------------------------------------
    // percent_decode 辅助
    // ------------------------------------------------------------------

    #[test]
    fn percent_decode_handles_hex_and_plain() {
        assert_eq!(percent_decode("udp%3A%2F%2Fhost%3A80"), "udp://host:80");
        assert_eq!(percent_decode("plain-text"), "plain-text");
        assert_eq!(percent_decode("100%"), "100%"); // 非法的 % 序列原样保留
    }
}
