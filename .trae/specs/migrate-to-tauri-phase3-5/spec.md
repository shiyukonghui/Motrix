# Motrix Tauri 迁移（Phase 3~5）Spec

## Why

Motrix 迁移已完成后端引擎与事件管线（Phase 0~2：Tauri 壳、JSON-RPC 服务、KGet HTTP/FTP 引擎、StateBroadcaster 事件推送、会话持久化）。本次依据 `MIGRATION-TAURI.md` 完成 **Phase 3（BitTorrent 支持）、Phase 4（平台能力补齐）、Phase 5（CLI 命令行工具）**，达到"功能与 Electron 版对等"的中间里程碑。

核心动机：
- **Phase 3**：补上 BT 能力（磁力/.torrent），并以"bitfield 降采样 + peers 分页"根治长时 BT 下载 WebView 白屏（文档 5.8 硬指标：≥4h 压测不白屏）。
- **Phase 4**：把 Electron 主进程的桌面集成胶水（托盘/菜单/深链/自启/单实例/通知等）等价迁移到 Tauri 插件体系，保持前端命令契约不变。
- **Phase 5**：把引擎从"GUI 内嵌"扩展为"进程外可脚本驱动"，CLI 复用 JSON-RPC 契约成为第二个客户端，daemon 无头模式解锁服务器/CI 场景。

## What Changes

### Phase 3：BitTorrent 支持（librqbit 集成）
- 新建 `crates/motrix-core/src/bt.rs`：封装 librqbit session（spawn/stop）+ torrent handle，参考 rqbit 仓库 Tauri 桌面示例接入方式。
- `engine.rs::add_torrent` 由占位改为真实实现：magnet（先呈现 metadata 任务，`bittorrent.info` 为空、`isMagnetTask` 可复用，获取 metadata 后转为完整 BT 任务、gid 保持稳定）与 `.torrent`（base64 解码 → 解析 → 建任务）。
- BT 进度/peer/seeding 事件 → 任务字段（`completedLength` / `downloadSpeed` / `uploadSpeed` / `numSeeders` / `seeder`），接入现有 `update_progress` 回调管线与 StateBroadcaster。
- 状态机整合：BT 下载完成 → `Seeding` 或 `Complete`（由 `keep-seeding` 决定，`Seeding` 状态已预留）；BT 任务支持 pause/resume/remove。
- **BT 暂停语义（用户已确认）**：暂停 = 停止 torrent 并保存 session 状态（含 piece 位图），恢复 = 重新加入 torrent 续传。
- **bitfield 降采样（硬约束）**：`get_task_detail` 返回的 bitfield 字符串长度 ≤ 240 字符，每字符表示 0~3 的 piece 完成状态，与前端 `TaskGraphic/Index.vue` 的 `buildAtom` 语义对齐（`Math.floor(parseInt(ch,16)/4)`）；`engine:snapshot` 仍不含 bitfield 原串。
- **peers 分页（硬约束）**：`get_peers` 返回 peer 列表默认 100 条，字段与 aria2 兼容（`ip/port/peerId/bitfield/uploadSpeed/downloadSpeed`），前端 `TaskPeers.vue` 零改动（分页仅在 Rust 端限流，不做 UI 翻页）。
- `bittorrent` 字段填充：`infoHash` / `announceList` / `mode` / `info.name` 正确输出（`Task::to_aria2` 已支持 BT 分支）。
- `select-file` 文件选择：BT 多文件任务按 `select-file` 选项选中/取消文件，映射 librqbit 已选文件集合。
- seeding 控制：`keep-seeding` / `seed-ratio` / `seed-time` 映射 librqbit 做种行为；做种结束触发 `bt-complete` 事件（`TaskEvent` 已预留）。
- P1：`bt-tracker` 同步（`auto-sync-tracker` 配置驱动 tracker 列表拉取 → 写入 `system.json` 的 `bt-tracker` → 应用于新 BT 任务）；BT 任务会话持久化（信息哈希/piece 位图进入 checkpoint，重启恢复）。
- 契约测试：BT 任务字段 Golden-Data 对拍 + `to_aria2` BT 分支单测。

### Phase 4：平台能力补齐
- **托盘**：`tauri::tray` 静态托盘（菜单项按原 `tray.json`，id 保持原命名）+ **动态速度计（用户已确认：沿用 Canvas 绘制 + command 上传）**——`DynamicTray.vue` Canvas 绘制 → `application:update-tray`（二进制 ArrayBuffer）→ Rust 更新托盘图标，上传频率降至 1s。
- **应用菜单**：按平台构建（win32 五组），菜单项 id 保持原命名，点击触发现有 `handle_command` 分发。
- **命令分发补全**：`COMMAND_NAMES` 清单中未实现的约 20 项逐一实现（`application:quit/show/hide/relaunch/reset-session/factory-reset/change-theme/change-locale/auto-hide-window/change-menu-states/open-external/reveal-in-folder/open-file/clear-recent-tasks/save-preference`、`help:*` 外链、`app.check-for-updates` 等）。
- **事件处理**：`EVENT_NAMES` 4 类事件（`speed-change` / `download-status-change` / `progress-change` / `task-download-complete`）驱动托盘速度计、电源防休眠、窗口任务栏进度、最近文档。
- **单实例**：`tauri-plugin-single-instance`，二次启动聚焦已有窗口。
- **开机自启**：`tauri-plugin-autostart`，`open-at-login` 配置联动，配置变化即时生效。
- **深链协议**：`tauri-plugin-deep-link` 注册 `mo:` / `motrix:` / `magnet:`；深链 URL → 添加任务；dev 模式跳过注册（对齐 Electron 版 `is.dev()` 判断），打包产物中验收；magnet 深链依赖 Phase 3 的 `add_torrent`。
- **任务完成系统通知**：`engine:task-event` 的 `complete` → 系统通知（`task-notification` 配置控制）。
- **窗口状态保存**：`keep-window-state` 配置下窗口位置/大小持久化，重启恢复（Rust 自实现 bounds 持久化）。
- **最近文档**：`task-download-complete` → 添加系统最近文档。
- **配置监听联动**：8 类配置变化即时生效（`open-at-login` / `protocols` / `run-mode` / `proxy` / `locale` / `theme` / `show-progress-bar` / `auto-sync-tracker`），`proxy` 联动引擎 `all-proxy`/`no-proxy`。
- P1：UPnP（`igd` crate，`enable-upnp` 联动 `listen-port`/`dht-listen-port`）、自动更新（**用户已确认：接入 `tauri-plugin-updater` 命令链路，无 latest.json 元数据时返回"不可用"优雅降级**，验收降级为命令链路通）、电源管理防休眠、`run-mode=tray` 启动隐藏。

### Phase 5：CLI 命令行工具
- 新建 `crates/motrix-cli`（clap derive，加入 workspace）；`motrix-rpc` 新增轻量 JSON-RPC 客户端 `client.rs`（HTTP POST，约百余行，token 认证、错误映射）。
- 全局参数：`--rpc-url`（默认 `127.0.0.1:16800/jsonrpc`）/ `--rpc-secret` / `--json` / `--quiet`；secret 解析顺序：参数 → 环境变量 `MOTRIX_RPC_SECRET` → `system.json` 的 `rpc-secret`。
- 远程模式命令：`add`（多 URL） / `add-torrent`（依赖 Phase 3） / `list [active|waiting|stopped]` / `status <gid>` / `pause` / `pause-all` / `resume` / `resume-all` / `remove [-f] [--delete-files]` / `purge` / `config get/set`（systemKeys → `aria2.*`，userKeys → `motrix.saveUserConfig` 扩展方法） / `engine status`。
- **daemon 无头模式**：复用 `motrix-core` 引擎 + 启动 JSON-RPC 服务，无 GUI 独立运行；**端口冲突策略（用户已确认）**：检测到 `127.0.0.1:16800` 已占用时报错退出（非零退出码），提示先关闭 GUI/其他后端。
- `--json` 输出直接透传 RPC 原始 result（避免二次序列化造成契约漂移）；默认人类可读输出（indicatif 进度条/表格）；`--quiet` 抑制非错误输出。
- 端到端验收：脚本可完成"添加→查询→暂停→恢复→删除"全流程；daemon 模式下 CLI 与外部 RPC 客户端（curl）均可操作；`--json` 字段与 `tellStatus` 契约一致（复用 Phase 2 契约测试方法论）。

## Impact

- Affected specs：覆盖 `MIGRATION-TAURI.md` 第 5.5（BT）、5.8（bitfield 降采样/peers 分页）、6.5（托盘通道）、8.2~8.3（事件频道/平台映射）、9（CLI）、10.3~10.5（Phase 3~5 实施）、11（风险对策）章节。
- Affected code（新增 Rust）：
  - `crates/motrix-core/src/bt.rs`（新增）、`crates/motrix-core/src/engine.rs`（add_torrent 真实实现、BT 状态分支）、`crates/motrix-core/src/task.rs`（BT 字段）、`crates/motrix-core/src/options.rs`（bt-tracker/seed-ratio/seed-time/select-file 映射）、`crates/motrix-core/src/session.rs`（BT checkpoint）
  - `crates/motrix-rpc/src/client.rs`（新增）、`crates/motrix-rpc/src/methods.rs`（addTorrent/getPeers/saveUserConfig 真实实现）、`crates/motrix-rpc/src/server.rs`
  - `crates/motrix-cli/`（新增 crate）
  - `src-tauri/src/app/`（新增：window.rs / tray.rs / menu.rs / autostart.rs / protocol.rs / updater.rs / energy.rs）、`src-tauri/src/commands.rs`（get_task_detail 降采样 / get_peers 分页 / 平台命令）、`src-tauri/src/lib.rs`（插件装配、命令补全）、`src-tauri/src/rpc_backend.rs`（saveUserConfig）
- Affected code（前端，仅桥接/数据管线，视觉组件不动）：
  - `src/renderer/components/Native/DynamicTray.vue`（上传频率控制，可选）、`src/shims/events.js`（bt 事件订阅，可选）
  - 视觉组件 `TaskItem.vue` / `TaskGraphic` / `TaskPeers.vue` / `store/` **保持不变**
- 项目约定：**所有 Rust 代码须含中文注释**；前端 UI 视觉层零改动；禁止造轮子（BT 用 librqbit、平台能力用 tauri 官方插件、UPnP 用 igd）。
- 新增依赖：`librqbit`（Apache-2.0 兼容）、`clap`（derive）、`indicatif`、`tauri-plugin-deep-link`、`tauri-plugin-autostart`、`tauri-plugin-updater`、`tauri-plugin-single-instance`、`igd`。

## ADDED Requirements

### Requirement: BT 引擎集成与任务添加（Phase 3）

系统 SHALL 提供 `motrix-core/src/bt.rs`，封装 librqbit session（spawn/stop）与 torrent handle；`add_torrent` 支持 magnet 与 `.torrent` 两种输入，返回 aria2 兼容 gid。

#### Scenario: magnet 链接添加
- **WHEN** 调用 `add_torrent("magnet:?xt=urn:btih:...")`
- **THEN** 创建 metadata 任务（status=active/waiting，`bittorrent.info` 为空、`totalLength`=0），前端 `isMagnetTask` 判断可复用；metadata 获取成功后转为完整 BT 任务（`files`/`totalLength`/`announceList` 填充），gid 保持稳定

#### Scenario: .torrent 文件添加
- **WHEN** 调用 `add_torrent` 传入 base64 编码的 .torrent 内容
- **THEN** 解码解析并创建 BT 任务，可直接开始下载

### Requirement: BT 进度与状态机整合（Phase 3）

系统 SHALL 将 librqbit 的 piece/peer 统计映射为任务字段（`completedLength` / `downloadSpeed` / `uploadSpeed` / `numSeeders` / `seeder`），接入现有 `update_progress` 管线并经 `engine:snapshot` 推送；BT 任务下载完成进入 `Seeding`（由 `keep-seeding` 决定）或 `Complete`。

#### Scenario: BT 下载与完成
- **WHEN** BT 任务下载进行中
- **THEN** 进度/速度/做种数随 librqbit 事件持续更新，状态机按 `Active→Seeding→Complete` 或 `Active→Complete` 单次迁移，做种结束触发 `bt-complete` 事件

#### Scenario: BT 暂停/恢复
- **WHEN** 对 BT 任务执行 pause 再 resume
- **THEN** 暂停 = 停止 torrent 并保存 session 状态（含 piece 位图），恢复 = 重新加入 torrent 从已下载 piece 续传（已确认语义）

### Requirement: bitfield 降采样（Phase 3 硬约束）

系统 SHALL 在 Rust 端对 bitfield 按目标宽度降采样：`get_task_detail` 返回的 bitfield 字符串长度 ≤ 240 字符，每字符为十六进制数字表示 0~3 的 piece 完成状态，与前端 `TaskGraphic` 的 `buildAtom` 语义兼容；`engine:snapshot` 不含 bitfield 原串。

#### Scenario: 详情面板打开
- **WHEN** 前端 `invoke('get_task_detail', { gid })` 获取 BT 任务详情
- **THEN** 返回 bitfield 串长度 ≤ 240，前端 TaskGraphic 渲染 DOM 节点数 ≤ 240，长时 BT 下载任务列表 + 详情图保持流畅不白屏（≥4h 压测验收）

#### Scenario: 降采样语义正确
- **WHEN** 运行降采样单元测试
- **THEN** 输出字符串每个字符经 `Math.floor(parseInt(ch,16)/4)` 还原的 0~3 状态与原始 piece 位图在该区间平均完成度一致（roundtrip 测试通过）

### Requirement: peers 分页（Phase 3 硬约束）

系统 SHALL 让 `get_peers` 返回分页 peer 列表（默认 100 条），字段与 aria2 兼容（`ip` / `port` / `peerId` / `bitfield` / `uploadSpeed` / `downloadSpeed`）；前端 `TaskPeers.vue` 零改动（Rust 端限流，无 UI 翻页）。

#### Scenario: Peers 面板
- **WHEN** 前端 `invoke('get_peers', { gid })` 获取 BT 任务 peer 列表
- **THEN** 返回 ≤ 100 条 peer 记录，字段与 aria2 `getPeers` 契约一致，前端直接渲染

### Requirement: BT 选项支持（Phase 3）

系统 SHALL 支持 `select-file`（文件选择）、`keep-seeding` / `seed-ratio` / `seed-time`（做种控制）、`bt-tracker`（tracker 同步，P1）、BT 任务会话持久化（P1）。

#### Scenario: 文件选择
- **WHEN** 对 BT 多文件任务执行 `change_option` 设置 `select-file` 索引
- **THEN** librqbit 已选文件集合随之更新，仅下载选中文件

#### Scenario: 做种控制
- **WHEN** BT 任务下载完成且 `keep-seeding` 开启
- **THEN** 进入做种状态直至满足 `seed-ratio` / `seed-time` 上限，随后触发 `bt-complete`

### Requirement: 托盘与菜单（Phase 4）

系统 SHALL 提供 `tauri::tray` 托盘（菜单项 id 保持原 `tray.json` 命名，点击托盘显隐主窗口）+ 动态速度计（`DynamicTray.vue` Canvas 绘制 → `application:update-tray` 上传二进制 → Rust 更新托盘图标，上传频率 ≤ 1s）+ 按平台构建的应用菜单（菜单项 id 保持原命名，点击触发 `handle_command` 分发）。

#### Scenario: 托盘速度计
- **WHEN** 下载进行中且 `tray-speedometer` 配置开启
- **THEN** 托盘图标按 `engine:global-stat` 的下载速度周期性更新为 Canvas 绘制的速度计图像

### Requirement: 平台能力与命令补全（Phase 4）

系统 SHALL 补全 `COMMAND_NAMES` 未实现命令（退出/显隐/重置会话/恢复出厂/主题/语言/自动隐藏/菜单状态/外链/显示在文件夹/打开文件/清除最近任务/保存偏好/检查更新等约 20 项），并实现单实例（`tauri-plugin-single-instance`）、开机自启（`tauri-plugin-autostart` + `open-at-login` 联动）、深链协议（`mo:` / `motrix:` / `magnet:`，dev 模式跳过注册）、任务完成系统通知（`task-notification` 配置控制）、窗口状态保存（`keep-window-state`）、最近文档、4 类事件处理（速度/下载中/进度/完成）、8 类配置变化即时联动。

#### Scenario: 命令链路完整
- **WHEN** 前端触发 `COMMAND_NAMES` 中任意命令
- **THEN** Rust 端对应实现生效，不再出现"已列入清单但未实现"的日志；菜单/托盘/深链行为与 Electron 版等价

#### Scenario: 深链添加任务
- **WHEN** 系统将 `magnet:?xt=...` URL 交由 Motrix 处理（打包产物中）
- **THEN** 应用启动/聚焦并创建 BT 下载任务（依赖 Phase 3 `add_torrent`）

#### Scenario: 自动更新优雅降级
- **WHEN** `check-for-updates` 被触发且不存在发布元数据（latest.json）
- **THEN** 返回"不可用"结果而非报错，命令链路可验证

### Requirement: motrix-rpc 客户端与 CLI 骨架（Phase 5）

系统 SHALL 提供 `motrix-rpc/src/client.rs` 轻量 JSON-RPC 客户端（HTTP POST、token 认证、错误映射）与 `crates/motrix-cli`（clap derive，加入 workspace，`cargo build -p motrix-cli` 产出独立二进制）；全局参数 `--rpc-url` / `--rpc-secret` / `--json` / `--quiet`，secret 解析顺序为参数 → 环境变量 `MOTRIX_RPC_SECRET` → `system.json`。

#### Scenario: CLI 编译与帮助
- **WHEN** 执行 `cargo build -p motrix-cli` 与 `motrix-cli --help`
- **THEN** 产出独立二进制，帮助文本覆盖全部子命令与全局参数

### Requirement: CLI 远程模式命令（Phase 5）

系统 SHALL 实现远程模式全命令：`add`（多 URL，multicall）/ `add-torrent` / `list` / `status` / `pause` / `pause-all` / `resume` / `resume-all` / `remove [-f] [--delete-files]` / `purge` / `config get/set`（systemKeys → `aria2.*`，userKeys → `motrix.saveUserConfig`）/ `engine status`；`--json` 输出直接透传 RPC 原始 result；默认人类可读输出（indicatif）。

#### Scenario: 脚本化全流程
- **WHEN** 脚本依次执行 `add` → `list` → `status` → `pause` → `resume` → `remove`
- **THEN** 全部命令对运行中的后端生效，`--json` 输出字段与 `tellStatus` 契约一致，退出码正确

### Requirement: daemon 无头模式（Phase 5）

系统 SHALL 提供 `motrix-cli daemon`：复用 `motrix-core` 引擎 + 启动 JSON-RPC 服务（与 GUI 共享同一 crate，无行为分叉）；检测到 16800 端口已占用时报错退出（非零退出码）并提示先关闭 GUI/其他后端；退出时保存 checkpoint。

#### Scenario: daemon 独立运行
- **WHEN** 启动 `motrix-cli daemon` 后（无 GUI）
- **THEN** JSON-RPC 服务监听 16800，`motrix-cli add` 与 curl 外部客户端均可操作引擎

#### Scenario: 端口冲突
- **WHEN** GUI（或另一 daemon）已占用 16800 时启动 daemon
- **THEN** 报错退出（非零退出码），提示先关闭 GUI/其他后端（已确认策略）

### Requirement: motrix.saveUserConfig 扩展方法（Phase 5）

系统 SHALL 在 `motrix-rpc` 服务端实现 `motrix.saveUserConfig`（`motrix.*` 前缀，仅 CLI/内部使用，不破坏 aria2 兼容性），写入 `user.json` 的 userKeys；`config get/set` 对 userKeys 全流程可用。

#### Scenario: userKeys 配置写入
- **WHEN** 执行 `motrix-cli config set <userKey> <value>`
- **THEN** 经 `motrix.saveUserConfig` 写入 `user.json`，`config get <userKey>` 可读回

## MODIFIED Requirements

### Requirement: add_torrent / get_peers 占位 → 真实实现
Phase 1/2 中 `engine.rs::add_torrent`（返回占位错误）与 `commands.rs::get_peers`（返回空列表）在 Phase 3 替换为真实实现；对应占位测试同步更新。

### Requirement: TaskEvent bt-complete
`TaskEvent` 类型已预留 `bt-complete`（Phase 3 提供），本阶段实现 BT 做种完成时发出该事件。

### Requirement: DynamicTray.vue 上传频率
托盘动态图像上传频率从 Electron 版轮询驱动调整为 ≤ 1s（防托盘卡顿，文档 11 章对策）。

## REMOVED Requirements

### Requirement: add_torrent 占位错误
**Reason**：BT 引擎在 Phase 3 落地，占位错误不再需要。
**Migration**：`add_torrent` 真实实现支持 magnet 与 .torrent；占位测试用例改为断言真实行为。

## 非目标（本次范围外）

- Phase 6：性能基准、配置/会话一键迁移工具、`tauri build` 三平台打包、CI（`.github/workflows`）、README 更新。
- aria2 冷门 BT 选项完整支持矩阵（`bt-metadata-only` / `bt-force-encryption` 等 librqbit 缺口项）：按文档 11 章策略在 UI 禁用或提示，不强制实现。
- DHT 路由表格式迁移（`dht.dat` 重建可接受）。
- macOS Dock/TouchBar 专属能力（标记 P2 可选，非 mac 平台跳过）。
- `remove --delete-files` 的文件物理删除（P2，沿用现有 `remove_task` 行为）。
