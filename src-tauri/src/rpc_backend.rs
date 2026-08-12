//! rpc_backend 重导出 shim（Phase 5，Task 12：RPC 后端迁移到 motrix-core）
//!
//! 迁移原因：`motrix-cli daemon` 不能依赖 src-tauri（会拉进 tauri 全套），
//! 而 daemon 需要与 GUI 共享同一份 aria2 兼容 RPC 后端（无行为分叉）。
//! 因此 `CoreRpcBackend` 已整体迁至 `crates/motrix-core/src/rpc_backend.rs`，
//! 本文件仅保留一行重导出，保证 `src-tauri/src/lib.rs` 的
//! `use rpc_backend::CoreRpcBackend;` 无需改动即可继续编译。

pub use motrix_core::rpc_backend::CoreRpcBackend;
