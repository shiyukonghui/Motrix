# Phase 0-2 遗留问题修复 Spec

## Why

对 `migrate-to-tauri-phase2`（Phase 0-2）逐项审查后发现 4 类遗留问题：HTTPS 任务进度恒为 0（P1）、原生 shell 能力 command 未注册导致"在文件夹中显示/打开文件/删除"失效（P2）、KGet 进度事件速度恒为 0 导致速度计失效（P3）、Windows 构建因 openssl vendored 需要 perl 而阻塞（环境）。本变更修复这些问题，使 Phase 0-2 功能完整可用、构建可复现。

## What Changes

- **HTTPS/HTTP 总长探测**：`motrix-core/kget.rs` 的 `probe_content_length` 由"仅 http 手写 TCP 探测"改为基于 **reqwest**（HEAD → Content-Length，HEAD 不支持时 Range GET 兜底），支持 `https://`；reqwest 已在依赖树中（KGet 传递依赖），直接声明即可，不新增编译成本。
- **完成时进度回填**：`motrix-core/engine.rs` `handle_finished` 在 `total_length == 0`（探测失败）时读取磁盘输出文件大小回填 `total_length` / `completed_length`，保证最终进度=100%。
- **原生 shell 能力 command**：`src-tauri/commands.rs` 新增并注册 `show_item_in_folder` / `open_path` / `trash_item`（`std::process::Command` 平台命令 + `trash` crate），对齐 `src/shims/remote-shell.js` 已有调用，解除"显示在文件夹/打开文件/移到废纸篓"失效。
- **下载速度统计**：`motrix-core/engine.rs` 进度回调按"进度差值 / 时间差"自算瞬时 `download_speed`（KGet 回调 speed 恒 0 的库限制），驱动速度计与 `engine:global-stat`。
- **构建可复现**：记录 openssl vendored 构建需 perl 的前置条件（Windows 开发机安装 Strawberry Perl），验证 `cargo check` / `cargo test --workspace` 全绿。
- **非目标**：FTP/SFTP 集成测试、JSON-RPC WS 对外通知推送（notify.rs 接 WS）、addUri 多 URL 镜像语义（实测与原 Electron 版逐 URL 独立任务行为一致，非缺陷）——均不在本次范围。

## Impact

- Affected specs：`MIGRATION-TAURI.md` 第 5.3（选项映射/探测）、5.4（断点续传与会话）、5.8（状态事件）、6.4（原生能力 shim）、7（数据迁移）及 Phase 2 验收项；延续 `migrate-to-tauri-phase2` spec 的 Phase 0-2 范围。
- Affected code（新增/修改 Rust）：
  - `crates/motrix-core/src/kget.rs`（probe 改造，含模块注释与映射文档更新）
  - `crates/motrix-core/src/engine.rs`（完成回填 + 速度计算）
  - `crates/motrix-core/Cargo.toml`（声明 `reqwest`）
  - `src-tauri/src/commands.rs` / `src-tauri/src/lib.rs`（shell command 实现与注册）
  - `src-tauri/Cargo.toml`（声明 `trash`）
  - 测试：`crates/motrix-core/tests/*`、`src-tauri` 相关单元测试
- 项目约定：**所有 Rust 代码须含中文注释**。
- 不修改：前端视觉组件、shims 调用签名（`remote-shell.js` 保持现有 invoke 名）。

## ADDED Requirements

### Requirement: HTTPS/HTTP 任务总长探测正确（P1）

系统 SHALL 使 `probe_content_length` 支持 `http://` 与 `https://`：经 reqwest 发送 `HEAD` 请求解析 `Content-Length`；服务器不支持 HEAD（非 200/206）或未返回总长时，以 `Range: bytes=0-0` 的 GET 从 `Content-Range` 兜底；任何失败返回 `Ok(0)` 且不阻塞任务添加，连接超时 ≤ 5 秒。

#### Scenario: HTTPS 任务添加即得总长
- **WHEN** 经 `add_uri` 添加 `https://` URL 任务
- **THEN** 任务 `totalLength` 为真实总长（非 0），下载过程 `completedLength` 按 percent 换算递增

#### Scenario: 探测失败时完成回填
- **WHEN** 探测失败（`totalLength == 0`）的任务下载完成
- **THEN** `handle_finished` 读取磁盘文件大小回填 `totalLength` / `completedLength`（两者相等），进度最终为 100%

### Requirement: 原生 shell 能力 command（P2）

系统 SHALL 在 `src-tauri` 实现并注册 `show_item_in_folder` / `open_path` / `trash_item` 三个 command：`show_item_in_folder` 按平台定位文件（Windows `explorer /select,`、macOS `open -R`、Linux 打开父目录）；`open_path` 用平台默认打开（Windows `explorer`、macOS `open`、Linux `xdg-open`）；`trash_item` 用 `trash` crate 移到回收站。失败返回带错误信息的 Err，成功返回 OK。

#### Scenario: 前端调用不报错
- **WHEN** 前端 `invoke('show_item_in_folder'|'open_path'|'trash_item', { path })`
- **THEN** 返回成功结果，文件管理器定位 / 默认程序打开 / 文件进入回收站，shim 不再 console.warn

### Requirement: 下载速度统计（P3）

系统 SHALL 在 `motrix-core/engine.rs` 按进度差值计算瞬时速度：进度回调记录 `(completed_length, Instant)` 快照，`download_speed = Δcompleted / Δt`；任务 `downloadSpeed`、`engine:snapshot` / `engine:global-stat` 均反映该速度。

#### Scenario: 速度计显示非零
- **WHEN** HTTP(S) 任务下载中（`totalLength > 0`）
- **THEN** `get_global_stat` 的 `downloadSpeed` / 快照任务 `downloadSpeed` 大于 0 且与下载速率一致

### Requirement: 构建可复现（环境）

系统 SHALL 在标准 Windows 开发环境下可复现 `cargo check` 与 `cargo test --workspace` 通过：openssl vendored（`ssh2` → `libssh2-sys` → `openssl-sys`，KGet 无条件依赖）构建需 perl，README 构建前置说明须记录；本机安装 Strawberry Perl 后全量测试通过。

#### Scenario: 干净环境构建
- **WHEN** 在已安装 perl（或等价提供 vendored OpenSSL 构建能力）的机器执行 `cargo test --workspace`
- **THEN** 全部 crate 编译通过、全部用例绿色（含既有契约/集成测试）

## MODIFIED Requirements

### Requirement: probe_content_length（原仅 http 手写 TCP）
原实现仅支持 `http://` 且为手写 TCP 探测，导致 `https://` 任务 `totalLength` 恒为 0、进度与速度显示失效。改为 reqwest 双协议探测（详见上文"HTTPS/HTTP 任务总长探测正确"）。`EngineOptions` 的 UA / 代理配置在探测中继续生效。

## REMOVED Requirements

无（本次不删除既有能力；仅修复与增强）。
