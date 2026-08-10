# Motrix Tauri 迁移（Phase 0~2）Spec

## Why

Motrix 目前是 Electron 22 + 外部 aria2c 子进程架构。依据 `MIGRATION-TAURI.md`，需迁移为 **Tauri 2 + Rust 原生下载引擎** 架构，目标：去掉 aria2c 子进程、降低内存与包体积、解决长时 BT 下载 WebView 白屏问题、**前端 UI 视觉层零改动**。

本次 Spec 覆盖文档 **Phase 0（脚手架与可行性验证）、Phase 1（JSON-RPC 对外服务 + Tauri 通道 + 状态占位）、Phase 2（HTTP/FTP 下载引擎核心，KGet 集成）**。Phase 3~6（BT、平台能力、CLI、优化收尾）不在本次范围。

## What Changes

- **Phase 0**：新建 Cargo workspace（`src-tauri` + `crates/*`）与 Tauri 2 壳；前端 webpack 改为 `target: web` 产出 `dist/web` 供 `frontendDist`；新增 `src/shims/` electron 兼容层（`electron-is` / `ipcRenderer` / `remote-shell` / `remote-nativeTheme`）；Rust 实现 `get_app_config` 与 `application:save-preference` 配置读写通道，打通 `fetchPreference`。
- **Phase 1**：新建 `crates/motrix-rpc`（axum WS + HTTP POST 双通道 JSON-RPC，`127.0.0.1:16800`），实现 `system.multicall` / `system.listMethods` / `aria2.getVersion` / `aria2.getGlobalStat` / `tell*`（空列表）与 token 认证；Tauri 任务操作 command 占位（`add_uri` / `pause_task` 等）+ capability 权限；`engine:snapshot` / `engine:global-stat` 事件通道与 `shims/events.js` 订阅 → Vuex。
- **Phase 2**：新建 `crates/motrix-core` 任务模型/状态机（字段与 aria2 `tellStatus` 对齐）+ 任务仓库；KGet v1.7 集成（`kget::builder` / `AdvancedDownloader`，`.spawn()` Receiver → 进度字段；`options.rs` 完成 systemKeys → KGet 配置映射）；任务操作 command 接通真实引擎（增删改查/限速/代理/全局选项）；StateBroadcaster 节流合并推送 `engine:*` 事件，前端数据管线由轮询切换为事件订阅（增量 mutation）；会话导入导出（`download.session` 解析 + 自有 checkpoint JSON 断点续传）。
- **删除**：`extra/` 下 aria2c 二进制不再由 Tauri 版使用（本次不物理删除目录，仅不再依赖）。

## Impact

- Affected specs：覆盖 `MIGRATION-TAURI.md` 第 4（目标架构）、5（下载引擎集成 5.1~5.4、5.6~5.8）、6（前端兼容层 6.1~6.6）、7（配置迁移 7.1~7.3）、8（IPC/RPC 契约 8.1~8.2）章节的 Phase 0~2 部分。
- Affected code（前端，仅桥接/数据管线，视觉组件不动）：
  - `src/renderer/api/Api.js`、`src/renderer/pages/index/main.js`、`src/renderer/components/Native/EngineClient.vue`、`src/renderer/components/Native/Ipc.vue`、`src/renderer/components/Native/TitleBar.vue`、`src/renderer/components/Native/DynamicTray.vue`、`src/renderer/utils/native.js`
  - 新增 `src/shims/*`
  - `package.json`（新增 `@tauri-apps/api`、`@tauri-apps/cli`）、`.electron-vue/*`（web 构建目标）
- Affected code（新增 Rust）：
  - `src-tauri/`（main.rs / lib.rs / commands.rs / tauri.conf.json / capabilities）、`crates/motrix-core/`（task.rs / kget.rs / options.rs / session.rs / config.rs / broadcaster.rs）、`crates/motrix-rpc/`（server.rs / methods.rs / notify.rs）
- 项目约定：**所有 Rust 代码须含中文注释**。

## ADDED Requirements

### Requirement: Cargo workspace 与 Tauri 2 脚手架

系统 SHALL 提供可由 `cargo check` 通过的 workspace 根 `Cargo.toml`（members: `src-tauri`, `crates/motrix-core`, `crates/motrix-rpc`）与可启动的 `src-tauri` crate（`tauri.conf.json`、`capabilities/default.json`、图标、`main.rs`/`lib.rs`/`commands.rs`）。

#### Scenario: 脚手架可编译
- **WHEN** 在项目根执行 `cargo check`
- **THEN** workspace 全部 crate 编译通过，无致命错误

#### Scenario: Tauri 配置对齐数据目录
- **WHEN** 检查 `tauri.conf.json` 的 `bundle.identifier` 与 `app_data_dir`
- **THEN** 数据目录对齐 Electron 版 `motrix` 目录（Windows `%APPDATA%\motrix` 等），保证旧配置/会话可迁移

### Requirement: 前端 web 构建产物接入 Tauri

系统 SHALL 产出 web 目标构建产物（`dist/web`）供 `tauri.conf.json` 的 `frontendDist` 使用，使 Tauri WebView 能加载现有 Vue2 前端。

#### Scenario: Tauri dev 加载前端
- **WHEN** 执行 `tauri dev`（dev 阶段指向 webpack dev server 或 `dist/web`）
- **THEN** 前端页面正常渲染，无 `electron` 模块运行时错误

### Requirement: shim 兼容层（替换 electron 依赖）

系统 SHALL 提供 `src/shims/` 兼容层替换前端对 `electron` / `electron-is` / `@electron/remote` 的直接依赖：`electron-is.js`（`is.renderer()` 恒为 true，平台判断读 Rust 注入的 `window.__TAURI_OS_PLATFORM__`）、`ipcRenderer.js`（`send('command'/'event')` → Tauri `emit`，`invoke('get-app-config')` → `invoke('get_app_config')`，`on` → `listen`）、`remote-shell.js`（`showItemInFolder`/`openPath`/`trashItem` 占位）、`remote-nativeTheme.js`（`shouldUseDarkColors` 走 `prefers-color-scheme`）、`index.js`（统一注入 `$electron` 与 shim）。

#### Scenario: 全局替换
- **WHEN** 在 `src/renderer` 下全局搜索 `electron` / `electron-is` / `@electron/remote` 的 import
- **THEN** 全部经由 `src/shims/` 转发，视觉组件（`.vue` / store）不出现平台 API 直接调用

#### Scenario: 平台注入
- **WHEN** Tauri WebView 加载完成
- **THEN** `window.__TAURI_OS_PLATFORM__` 已注入（`win32` / `darwin` / `linux`），`is.macOS()/is.windows()/is.linux()` 返回正确值

### Requirement: Rust 配置层与 get_app_config

系统 SHALL 在 `motrix-core/src/config.rs` 直接读写 Electron 版同名 JSON（`user.json` / `system.json`），键名与默认值与 `src/shared/configKeys.js` 的 `userKeys` / `systemKeys` 完全一致；`get_app_config` command 返回 user + system + context（platform/arch/version/log-path/session-path 等）合并对象。

#### Scenario: 首次启动
- **WHEN** 数据目录不存在 `user.json` / `system.json`
- **THEN** 按默认值创建两个文件，前端 `fetchPreference` 返回完整配置

#### Scenario: 主题/语言切换持久化
- **WHEN** 前端 `savePreference` 经 `command` 通道发送 `application:save-preference`
- **THEN** Rust 端将 `{user, system}` 分区写回对应 JSON 文件，主题/语言切换重启后保持

### Requirement: motrix-rpc JSON-RPC 服务（对外接口）

系统 SHALL 提供 `crates/motrix-rpc`：axum 实现 `127.0.0.1:{rpc-listen-port}/jsonrpc` 的 **WS + HTTP POST 双通道**；首个参数 `token:{rpc-secret}` 认证；实现 `system.multicall` / `system.listMethods` / `aria2.getVersion` / `aria2.getGlobalStat`，`aria2.tellActive` / `tellWaiting` / `tellStopped` 返回空列表（Phase 1 占位），`aria2.addMetalink` 返回占位错误。

#### Scenario: HTTP POST 调用
- **WHEN** 用 curl POST `{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":["token:xxx"]}` 到 `/jsonrpc`
- **THEN** 返回合法 JSON-RPC 响应，含版本信息与 enabledFeatures

#### Scenario: WS 调用与认证
- **WHEN** 外部 WS 客户端连接并调用 `system.multicall`
- **THEN** 批量方法按序执行并返回对应结果；无 token 或 token 错误时返回 `Authorization failed` 错误

### Requirement: Tauri 任务操作 command 占位与 capability 权限

系统 SHALL 在 `src-tauri/src/commands.rs` 注册任务操作 command（`add_uri` / `add_torrent` / `pause_task` / `resume_task` / `remove_task` / `change_option` / `get_global_stat` / `save_session` 等），Phase 1 返回占位结果；`capabilities/default.json` 配置对应权限。

#### Scenario: 占位任务操作
- **WHEN** 前端 `invoke('add_uri', { uris, options })`
- **THEN** 返回占位 gid（可成功/可失败占位），前端任务流（AddTask 弹窗 → Vuex）在 dev 下不报错

### Requirement: engine:* 事件通道与前端订阅

系统 SHALL 建立 `engine:snapshot` / `engine:task-event` / `engine:global-stat` 事件通道（`StateBroadcaster` 骨架，Phase 1 推送空任务占位快照 + 全局统计）；`src/shims/events.js` 订阅事件并映射为 Vuex 增量 mutation（`app/UPDATE_GLOBAL_STAT`、`task` 列表 diff 更新）。

#### Scenario: 状态事件驱动 Vuex
- **WHEN** Rust 端 emit `engine:snapshot` / `engine:global-stat`
- **THEN** 前端 Vuex 中全局统计与任务列表随之更新，视觉组件无需改动即显示最新数据

### Requirement: motrix-core 任务模型与状态机（Phase 2）

系统 SHALL 提供任务模型 `Task`（`gid`(16 位 hex) / `status` / `totalLength` / `completedLength` / `downloadSpeed` / `uploadSpeed` / `dir` / `files` / `errorCode` / `errorMessage` / `connections` 等，与 aria2 `tellStatus` 字段对齐）与状态机（active / waiting / paused / error / complete / removed）；任务仓库维护 active / waiting / stopped 列表并提供查询/变更操作；所有数值字段按 aria2 惯例以**字符串**输出。

#### Scenario: 任务全生命周期
- **WHEN** 依次执行 add_uri → 下载 → pause → resume → remove
- **THEN** 任务状态按 状态机 正确迁移，`tellStatus` / 快照字段与 aria2 契约一致

### Requirement: KGet 下载引擎集成（Phase 2）

系统 SHALL 在 `motrix-core/src/kget.rs` 封装 KGet v1.7（`kget::builder(url)` / `AdvancedDownloader`），`.spawn()` 返回的进度 `Receiver` 驱动任务进度字段；`options.rs` 完成 aria2 选项 → KGet 配置映射（`max-connection-per-server`/`split` → `connections`、限速 → 速度限制、代理 → ProxyConfig、`header`/`user-agent`/`cookie` → 自定义头、`max-tries`/`retry-wait` → 重试、`checksum` → 校验和）。

#### Scenario: HTTP 下载
- **WHEN** 通过 `add_uri` 添加一个 HTTP URL 任务并允许运行
- **THEN** KGet 引擎实际下载文件到 `dir`，进度 Receiver 持续更新 `completedLength` / `downloadSpeed`，任务状态随事件流转

#### Scenario: 断点续传
- **WHEN** 下载中途暂停后再恢复（或应用重启后恢复）
- **THEN** 基于 HTTP Range / 自有 checkpoint 从已下载字节继续，而非重新下载

### Requirement: StateBroadcaster 状态推送（Phase 2）

系统 SHALL 实现 `motrix-core/src/broadcaster.rs`：节流合并任务快照（默认 1s，按 `numActive` 自适应 0.5s~6s），经 `tauri::emit` 推送 `engine:snapshot`（精简字段，不含 bitfield 原串）/ `engine:task-event` / `engine:global-stat`；前端 `EngineClient.vue` 由轮询改为事件订阅，`Api.js` 的 `fetch*` 方法改为消费事件数据 / on-demand `invoke('get_task_detail')`，操作类方法改为 Tauri command。

#### Scenario: 无轮询状态同步
- **WHEN** 观察网络面板与代码
- **THEN** 前端不再对 JSON-RPC 做高频轮询，任务列表/全局统计均由 `engine:*` 事件驱动

### Requirement: 会话持久化（Phase 2）

系统 SHALL 在 `motrix-core/src/session.rs` 解析 Electron 版 `download.session` 文本（`gid status path urls…`）导入任务列表；此后维护自有 checkpoint（JSON，记录每任务已下载字节与进度），退出时保存、启动时恢复。

#### Scenario: 旧会话导入
- **WHEN** 数据目录存在旧 `download.session` 且启动新版本
- **THEN** 任务列表按文本重建（status / gid / URL / 目录），已完成任务进入历史列表

#### Scenario: checkpoint 恢复
- **WHEN** 应用正常退出后再次启动，存在未完成任务
- **THEN** 从 checkpoint 恢复已下载字节并继续下载

### Requirement: 契约测试与集成测试（Phase 2）

系统 SHALL 提供测试：每个 RPC 方法 Golden-Data 契约测试（与 Electron 版真实返回对拍）、本地 HTTP 服务器集成测试（大/小文件、断点、限速、代理场景）。

#### Scenario: 契约一致
- **WHEN** 运行 `cargo test -p motrix-rpc` 与 `cargo test -p motrix-core`
- **THEN** getVersion / getGlobalStat / tellStatus 等返回字段与 Electron 版录制样本一致，下载集成用例全部通过

## MODIFIED Requirements

### Requirement: 前端数据管线（Api.js / EngineClient.vue）
原轮询 + WebSocket JSON-RPC 管线被 Tauri command + `engine:*` 事件替代（详见上文事件通道与状态推送两条）。视觉组件与 Vuex store 语义不变。

## REMOVED Requirements

### Requirement: aria2c 子进程依赖
**Reason**：迁移目标为"契约保持、引擎替换"，下载由 Rust 原生引擎（KGet）实现，aria2c 不再被 Tauri 版启动。
**Migration**：`extra/` 目录保留不动（不删除），Tauri 版不再引用；`download.session` 由 `session.rs` 解析导入，`*.aria2` 私有控制文件不做解析，未完成任务提示重新下载。

## 非目标（本次范围外）

- Phase 3：BitTorrent（librqbit）、bitfield 降采样、peers 分页
- Phase 4：托盘速度计、深链、开机自启、UPnP、更新、单实例
- Phase 5：motrix-cli 命令行工具
- Phase 6：性能基准、打包发布、CI
