# Tasks

## Phase 0：脚手架与可行性验证

- [x] Task 1：搭建 Cargo workspace 与 src-tauri 脚手架
  - [ ] 1.1 创建根 `Cargo.toml`（workspace，members: `src-tauri`, `crates/motrix-core`, `crates/motrix-rpc`），并创建空 `crates/motrix-core`、`crates/motrix-rpc` 库骨架
  - [ ] 1.2 创建 `src-tauri/`（`Cargo.toml` 依赖 tauri 2 + 必要插件、`tauri.conf.json`、`capabilities/default.json`、图标、`src/main.rs`、`src/lib.rs`、`src/commands.rs`）
  - [ ] 1.3 在 `tauri.conf.json` 配置 `bundle.identifier` 使数据目录对齐 Electron 版 `motrix`（Windows `%APPDATA%\motrix` 等），`frontendDist` 指向 `../dist/web`
  - [ ] 1.4 验证：`cargo check` 全 workspace 通过

- [x] Task 2：前端 web 构建产物接入 Tauri
  - [ ] 2.1 复用/调整 `.electron-vue/webpack.web.config.js` 产出 `dist/web`（`target: web`），确保前端不再直接依赖 `electron` 模块可打包
  - [ ] 2.2 `package.json` 增加 `@tauri-apps/api`（前端运行时）与 `@tauri-apps/cli`（开发依赖），新增 `dev:web` / `build:web` 脚本与 `tauri dev` / `tauri build` 脚本
  - [ ] 2.3 验证：`npm run build:web` 生成 `dist/web/index.html` 可被 WebView 加载（`tauri dev` 页面渲染，无 `electron` 运行时错误）

- [x] Task 3：shim 兼容层 + 平台注入
  - [ ] 3.1 新建 `src/shims/electron-is.js`：`is.renderer()` 恒 true、`is.macOS()/windows()/linux()/dev()` 读 `window.__TAURI_OS_PLATFORM__` 与 dev 端口
  - [ ] 3.2 新建 `src/shims/ipcRenderer.js`：`send('command'|'event')` → Tauri `emit`；`on/removeListener` → `listen`；`invoke('get-app-config')` → `invoke('get_app_config')`
  - [ ] 3.3 新建 `src/shims/remote-shell.js`（`showItemInFolder` / `openPath` / `trashItem`，先映射到对应 Tauri command 或返回占位）与 `src/shims/remote-nativeTheme.js`（`shouldUseDarkColors` → `prefers-color-scheme`）
  - [ ] 3.4 新建 `src/shims/index.js` 统一注入 `$electron`（`Vue.prototype.$electron`）并接入 `pages/index/main.js` 顶部；替换 `main.js` / `utils/native.js` 中对 `electron` / `electron-is` / `@electron/remote` 的直接引用
  - [ ] 3.5 `src-tauri` 的 `main.rs` 在 webview 初始化时注入 `window.__TAURI_OS_PLATFORM__`（win32/darwin/linux）
  - [ ] 3.6 验证：`src/renderer` 下全局搜索 electron import 均走 shim；平台判断返回正确

- [x] Task 4：Rust 配置层 + get_app_config + 配置保存通道
  - [ ] 4.1 `motrix-core/src/config.rs`：`user.json` / `system.json` 读写（键名/默认值与 `src/shared/configKeys.js` 的 `userKeys` / `systemKeys` 一致），数据目录按 Task 1.3 对齐；不存在时写默认值
  - [ ] 4.2 `src-tauri/src/commands.rs` 实现 `get_app_config`：合并 user + system + context（platform/arch/version/log-path/session-path 等）
  - [ ] 4.3 `lib.rs` 监听 `command` / `event` Tauri 事件并分发；实现 `application:save-preference`（user/system 分区写回）+ 首版 `COMMAND_NAMES` 清单（未实现命令报"未实现"）
  - [ ] 4.4 验证：前端 `fetchPreference` 返回完整配置；主题/语言切换经 `application:save-preference` 写入 `user.json`，重启后保持

## Phase 1：JSON-RPC 服务（对外）+ Tauri 通道 + 状态占位

- [x] Task 5：motrix-rpc JSON-RPC 服务
  - [ ] 5.1 `crates/motrix-rpc`：`server.rs`（axum，`127.0.0.1:{rpc-listen-port}/jsonrpc` 的 WS + HTTP POST 双通道）、`methods.rs`、`notify.rs`（通知骨架）
  - [ ] 5.2 方法实现：`system.multicall` / `system.listMethods` / `system.listNotifications` / `aria2.getVersion` / `aria2.getGlobalStat`；`aria2.tellActive` / `tellWaiting` / `tellStopped` 返回空列表；`aria2.addMetalink` 返回占位错误
  - [ ] 5.3 token 认证：首个参数 `token:{rpc-secret}`（读 `system.json`），错误返回 `Authorization failed`
  - [ ] 5.4 契约测试：HTTP POST 与 WS 客户端调用 getVersion / getGlobalStat / system.multicall / tellActive，断言 JSON-RPC 响应结构与字段
  - [ ] 5.5 验证：RPC 服务随应用启动监听 16800，curl 与 WS 客户端均可调用

- [x] Task 6：Tauri 任务操作 command 占位 + capability 权限
  - [ ] 6.1 `commands.rs` 注册 `add_uri` / `add_torrent` / `pause_task` / `resume_task` / `remove_task` / `change_option` / `get_global_stat` / `save_session` 占位实现（返回占位 gid / 空结果，错误语义明确）
  - [ ] 6.2 `capabilities/default.json` 配置 `core:default` 及事件/命令所需 permissions
  - [ ] 6.3 `lib.rs` `invoke_handler` 注册全部 command
  - [ ] 6.4 验证：`tauri dev` 下前端 `invoke('add_uri')` 等调用不报错并返回占位结果

- [x] Task 7：engine:* 事件通道 + shims/events.js
  - [ ] 7.1 `motrix-core/src/broadcaster.rs` 骨架：周期推送空任务快照 + 全局统计（`engine:snapshot` / `engine:global-stat`），经 `tauri::emit` 输出
  - [ ] 7.2 新建 `src/shims/events.js`：`listen('engine:snapshot' | 'engine:task-event' | 'engine:global-stat')` → Vuex 增量 mutation（`app/UPDATE_GLOBAL_STAT` 与任务列表 diff 更新）
  - [ ] 7.3 `EngineClient.vue` / `Api.js` 接入事件订阅，验证占位任务经 Tauri command + 事件驱动 Vuex（"操作走 command、状态走事件"通道打通）
  - [ ] 7.4 验证：dev 下订阅事件后 Vuex 统计/任务数据随 Rust 端推送更新

## Phase 2：HTTP/FTP 下载引擎（核心，KGet 集成）

- [x] Task 8：motrix-core 任务模型与状态机
  - [ ] 8.1 `task.rs`：`Task` / `TaskStatus` / `TaskFile`（字段与 aria2 `tellStatus` 对齐，数值以字符串输出）、16 位 hex `gid` 生成、状态机迁移（active/waiting/paused/error/complete/removed）
  - [ ] 8.2 任务仓库：active / waiting / stopped 列表维护与查询，`changeOption` / 暂停 / 恢复 / 删除语义
  - [ ] 8.3 单元测试：gid 生成、状态迁移、字段序列化

- [x] Task 9：KGet 集成（kget.rs + options.rs）
  - [ ] 9.1 锁定 KGet 版本依赖（`kget = "1.7"`，验证 builder API：`kget::builder(url).connections(n).max_speed(..).proxy(..).spawn()` 与 `AdvancedDownloader`），以 POC 用例验证 API 可用
  - [ ] 9.2 `options.rs`：systemKeys → KGet 配置映射（`max-connection-per-server`/`split` → connections、`max-download-limit`/`max-overall-download-limit` → 限速、代理 `all-proxy`/`no-proxy` → ProxyConfig、`header`/`user-agent`/`cookie` → 自定义头、`max-tries`/`retry-wait` → 重试、`checksum` → 校验和、`dir`/`out` → 输出路径）
  - [ ] 9.3 `kget.rs`：封装 builder 调用，`.spawn()` 的进度 `Receiver` 订阅 → 更新任务 `completedLength` / `downloadSpeed` / `totalLength` / `connections`；错误映射 `errorCode` / `errorMessage`；暂停/恢复/取消语义与 KGet 能力对齐
  - [ ] 9.4 集成测试：本地 HTTP 服务器小/大文件下载、进度字段正确

- [x] Task 10：任务操作 command 接通引擎 + 全局选项
  - [ ] 10.1 `add_uri`（多 URL = 多源镜像）、`add_torrent` 占位、`pause_task` / `resume_task` / `remove_task`（含 force、批量）、`change_option` / `change_global_option`（限速/代理/并发即时生效）、`get_global_stat`、`save_session`、`purge` 全部接通 `motrix-core`
  - [ ] 10.2 `application:save-preference` 同步引擎全局选项（等价 `changeGlobalOption`）
  - [ ] 10.3 验证：HTTP 任务可下载/暂停/恢复/删除；限速与代理设置对运行中/新任务生效

- [x] Task 11：StateBroadcaster 状态推送 + 前端数据管线切换
  - [ ] 11.1 `broadcaster.rs` 完整实现：节流合并（默认 1s，按 `numActive` 自适应 0.5s~6s）→ `engine:snapshot`（精简字段快照）/ `engine:task-event`（onDownloadStart/Stop/Pause/Complete/Error 对应事件）/ `engine:global-stat`；订阅引擎仓库状态变更
  - [ ] 11.2 前端 `EngineClient.vue`：轮询逻辑 → 事件订阅（`events.js`），保留任务详情 on-demand `invoke('get_task_detail')`
  - [ ] 11.3 前端 `Api.js`：`fetchTaskList`/`getGlobalStat`/`fetchProgress` 改为消费事件数据；操作类（addUri/pause/resume/remove/changeOption 等）改为 Tauri command；移除 JSON-RPC 轮询路径
  - [ ] 11.4 `store/modules/task.js` 增量 mutation（按 gid diff 更新，非全量覆盖）
  - [ ] 11.5 验证：任务列表/进度条/速度计随 `engine:*` 事件流畅更新，无轮询请求

- [x] Task 12：会话持久化与断点续传
  - [ ] 12.1 `session.rs`：解析 Electron 版 `download.session`（`gid status path urls…`）导入任务；`*.aria2` 控制文件不解析，未完成任务标记为需重新下载
  - [ ] 12.2 自有 checkpoint（JSON，记录每任务已下载字节/进度/状态）：退出 `will-exit` 保存、启动恢复，未完成任务基于 KGet Range 续传
  - [ ] 12.3 数据迁移：旧 `user.json` / `system.json` 加载（Task 4 已覆盖），数据目录对齐验证
  - [ ] 12.4 验证：重启后已完成任务进历史、未完成任务继续下载；构造 `download.session` 样本验证导入

- [x] Task 13：契约测试与整体验收准备
  - [ ] 13.1 录制 Electron 版真实返回 JSON（getVersion / getGlobalStat / tellStatus 典型样本）作为 Golden-Data，`motrix-rpc` 契约测试逐字段对拍
  - [ ] 13.2 `motrix-core` 下载集成测试：断点续传、限速、多连接场景
  - [ ] 13.3 全量 `cargo test` 与 `tauri dev` 手工验收通过，对照 `checklist.md`

# Task Dependencies

- [Task 2] 不依赖 Task 1（前端构建独立），但 `tauri dev` 联调依赖 Task 1 完成
- [Task 3] 依赖 [Task 1]（平台注入在 main.rs）、[Task 2]（前端可构建运行）
- [Task 4] 依赖 [Task 1]、[Task 3]（`get-app-config` 经 shim 打通）
- [Task 5] 依赖 [Task 1]（workspace）、[Task 4]（读 `system.json` 取 rpc-secret/rpc-listen-port）
- [Task 6] 依赖 [Task 4]（共享配置/仓库骨架）
- [Task 7] 依赖 [Task 5]（事件语义参考 RPC 通知）、[Task 6]（command 占位联调）
- [Task 8] 依赖 [Task 4]（仓库/配置骨架）；可与 [Task 5] 并行
- [Task 9] 依赖 [Task 8]（任务模型）；与 [Task 10] 紧耦合（先 9 后 10）
- [Task 10] 依赖 [Task 9]、[Task 6]（替换占位）
- [Task 11] 依赖 [Task 10]、[Task 7]（事件通道复用）
- [Task 12] 依赖 [Task 8]（任务仓库）、[Task 10]（引擎可恢复）
- [Task 13] 依赖 [Task 5]~[Task 12]

**可并行项**：Task 1 → Task 2 可先并行；Task 5 与 Task 8 可并行；Task 9 完成后 Task 10、Task 12 可部分并行。
