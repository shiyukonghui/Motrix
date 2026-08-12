//! motrix-core：Motrix 下载管理器的核心库（GUI 与 CLI 共享）
//!
//! 本 crate 不依赖 Tauri，供 src-tauri（GUI）与后续 motrix-cli 复用同一套引擎代码。
//! 模块规划（按迁移文档，后续任务逐步实现）：
//! - config:      user.json / system.json 配置读写（本阶段已实现）
//! - task:        任务模型与状态机、任务仓库（Phase 2 Task 8，已实现）
//! - kget:        KGet 下载引擎封装（Phase 2 Task 9，已实现）
//! - options:     aria2 选项 → 引擎配置映射（Phase 2 Task 9，已实现）
//! - engine:      任务编排胶水层 TaskManager（Phase 2 Task 10，已实现）
//! - bt:          BitTorrent 引擎封装（Phase 3：librqbit 集成，见 MIGRATION-TAURI.md 5.5）
//! - session:     会话持久化（Phase 2）
//! - broadcaster: 状态广播器（Phase 1/2）
//! - rpc_backend: aria2 兼容 JSON-RPC 后端（Phase 5，Task 12：从 src-tauri 迁入，
//!   GUI 与 motrix-cli daemon 共享，见 MIGRATION-TAURI.md 4.1 crate 结构）

pub mod broadcaster;
pub mod bt;
pub mod config;
pub mod engine;
pub mod fastdown;
pub mod http;
pub mod kget;
pub mod options;
pub mod rpc_backend;
pub mod session;
pub mod task;

pub use config::ConfigManager;
// re-export 广播器关键 API，供 src-tauri（lib.rs 广播循环）与 motrix-rpc 直接使用
pub use broadcaster::{build_snapshot, effective_interval, BroadcasterConfig};
// re-export 任务编排胶水层，供 src-tauri（commands/rpc_backend）与 motrix-rpc 直接使用
pub use engine::TaskManager;
// re-export KGet 引擎封装关键类型，供 src-tauri（commands/broadcaster）与 motrix-rpc 直接使用
pub use kget::{probe_content_length, spawn_download, KgetEvent, KgetHandle, KgetInitError};
// re-export 引擎配置映射类型，供 src-tauri / motrix-rpc 构造下载配置
pub use options::{parse_headers, parse_size, EngineOptions};
// re-export BT 引擎封装关键类型，供 src-tauri（commands/rpc_backend）构造 BT 任务
pub use bt::{BtAddOptions, BtEngine, BtEngineConfig, BtEvent, BtProgress, PeerInfo};
// re-export 任务模块关键类型，供 src-tauri（commands/broadcaster）与 motrix-rpc 直接使用
pub use task::{
    generate_gid, BittorrentInfo, GlobalStat, Task, TaskError, TaskFile, TaskRepository,
    TaskStatus,
};
// re-export aria2 兼容 RPC 后端（Phase 5，Task 12）：供 src-tauri（rpc_backend shim）
// 与 motrix-cli daemon 直接使用，避免两处各自 import rpc_backend 模块路径
pub use rpc_backend::CoreRpcBackend;

/// aria2 兼容引擎版本串（`aria2.getVersion` / `get_engine_info` 输出统一来源）
///
/// 编译期拼接：前半段为模拟的 aria2 版本号（对齐 Electron 版 Motrix 主版本），
/// 括号内为 motrix-core 的实际 crate 版本（随构建动态获取，避免魔法串）。
pub const ENGINE_VERSION: &str = concat!("1.8.19 (motrix-engine ", env!("CARGO_PKG_VERSION"), ")");
