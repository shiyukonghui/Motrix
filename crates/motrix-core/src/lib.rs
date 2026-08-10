//! motrix-core：Motrix 下载管理器的核心库（GUI 与 CLI 共享）
//!
//! 本 crate 不依赖 Tauri，供 src-tauri（GUI）与后续 motrix-cli 复用同一套引擎代码。
//! 模块规划（按迁移文档，后续任务逐步实现）：
//! - config:      user.json / system.json 配置读写（本阶段已实现）
//! - task:        任务模型与状态机、任务仓库（Phase 2 Task 8，已实现）
//! - kget:        KGet 下载引擎封装（Phase 2 Task 9，已实现）
//! - options:     aria2 选项 → 引擎配置映射（Phase 2 Task 9，已实现）
//! - engine:      任务编排胶水层 TaskManager（Phase 2 Task 10，已实现）
//! - session:     会话持久化（Phase 2）
//! - broadcaster: 状态广播器（Phase 1/2）

pub mod broadcaster;
pub mod config;
pub mod engine;
pub mod kget;
pub mod options;
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
// re-export 任务模块关键类型，供 src-tauri（commands/broadcaster）与 motrix-rpc 直接使用
pub use task::{
    generate_gid, BittorrentInfo, GlobalStat, Task, TaskError, TaskFile, TaskRepository,
    TaskStatus,
};

/// aria2 兼容引擎版本串（`aria2.getVersion` / `get_engine_info` 输出统一来源）
///
/// 编译期拼接：前半段为模拟的 aria2 版本号（对齐 Electron 版 Motrix 主版本），
/// 括号内为 motrix-core 的实际 crate 版本（随构建动态获取，避免魔法串）。
pub const ENGINE_VERSION: &str = concat!("1.8.19 (motrix-engine ", env!("CARGO_PKG_VERSION"), ")");
