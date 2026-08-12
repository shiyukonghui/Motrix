# Tasks

## Phase 3：BT 支持（librqbit 集成）

- [x] Task 1：librqbit 集成 POC 与 bt.rs 骨架
  - [x] 1.1 锁定 librqbit 版本（**=8.1.1**，crates.io 最新稳定版；POC 验证 API 形态：`Session::new_with_opts` / `add_torrent`(AddTorrent::Url/TorrentFileBytes) / `ManagedTorrent` stats / pause/unpause/delete/update_only_files；Windows 编译无阻塞性前置）
  - [x] 1.2 新建 `crates/motrix-core/src/bt.rs`：封装 librqbit Session + torrent handle（进度轮询、pause/start/remove、文件选择），参考 rqbit 仓库 Tauri 桌面示例；自持 tokio Runtime 对外提供同步 API；所有代码含中文注释
  - [x] 1.3 单元测试：magnet 解析、.torrent 解析、downsample_bitfield、进度换算纯函数（9 例）；`cargo check` 通过

- [x] Task 2：add_torrent 真实实现（magnet + .torrent）+ metadata 迁移
  - [x] 2.1 `engine.rs::add_torrent`：magnet 创建 metadata 任务（`bittorrent.info` 省略、`totalLength=0`，前端 `isMagnetTask` 可复用——`BittorrentInfo.info_name` 改 `Option<String>`，None 时 to_aria2 省略 info 键）；`.torrent` base64 解码 → 解析 → 建任务
  - [x] 2.2 metadata 获取成功后转完整 BT 任务（files/totalLength/announceList 填充），gid 保持稳定；后台 std 线程 + 30s 超时兜底（librqbit 8.1.1 的 add_torrent 对 magnet 会阻塞等待 metadata）
  - [x] 2.3 更新占位测试（add_torrent 改为断言真实行为：任务创建成功 + metadata 阶段字段）

- [x] Task 3：BT 进度/状态整合 + 状态机 + 暂停恢复
  - [x] 3.1 librqbit stats → 任务字段（completedLength/downloadSpeed/uploadSpeed/numSeeders/seeder），接入 `bt_update_progress` 管线（进度轮询 500ms）
  - [x] 3.2 状态机：BT 完成 → Seeding（`keep-seeding` 开启）或 Complete；做种结束发 `bt-complete` 事件（TaskEvent 已预留）
  - [x] 3.3 BT 暂停 = session.pause（停止 torrent + 保留 piece 状态）；恢复 = unpause 重加入续传（已确认语义）；remove = session.delete
  - [x] 3.4 集成测试：magnet/.torrent 任务创建、BT 暂停/恢复/remove 分流（离线断言，不依赖真实 BT 网络）

- [x] Task 4：bitfield 降采样 + peers 分页（硬约束）
  - [x] 4.1 降采样函数 `downsample_bitfield`：piece 标志 → ≤240 字符十六进制串（每字符 0~15，前端 `Math.floor(parseInt(ch,16)/4)` 得 0~3 状态）；roundtrip 单测（长度≤240、状态与桶内完成度一致）
  - [x] 4.2 `commands.rs::get_task_detail` 确认接通降采样 bitfield（BT 任务）；`engine:snapshot` 不含 bitfield 原串（bt.rs 轮询时已降采样存储于 Task.bitfield）
  - [x] 4.3 `get_peers` 真实实现：返回 aria2 兼容字段（ip/port/peerId/bitfield/uploadSpeed/downloadSpeed）≤100 条分页；**上游限制：librqbit 8.1.1 未公开 per-peer 明细，当前返回空数组，契约与分页结构保留，待上游提供 per-peer stats 后填充**（代码注释说明）
  - [x] 4.4 单元测试：降采样 roundtrip、长度 ≤ 240、全完成/全未完成；peers 字段契约测试

- [x] Task 5：BT 选项与会话（select-file / seeding / bt-tracker / checkpoint）
  - [x] 5.1 `select-file`：`change_option` 设置选中文件索引 → `update_only_files` 更新 librqbit 已选文件集合
  - [x] 5.2 `keep-seeding`/`seed-ratio`/`seed-time` 映射 EngineOptions（librqbit 无内建做种比率/时长控制，keep-seeding 已生效，ratio/time 保留供上层调度，代码注释说明）
  - [x] 5.3 `bt-tracker` 同步（P1）：`bt_trackers` 选项映射到 EngineOptions，应用于新 BT 任务（auto-sync-tracker 拉取逻辑在 Phase 4 占位）
  - [x] 5.4 BT 任务会话持久化（P1）：`CheckpointTask` 增 `is_bt/info_hash/piece_bitmap`（`#[serde(default)]` 向后兼容），导出/恢复 BT 任务
  - [x] 5.5 契约测试：BT 任务字段 Golden-Data 对拍 + `to_aria2` BT 分支单测（magnet 阶段省略 info 键）

## Phase 4：平台能力补齐

- [x] Task 6：托盘 + 菜单 + 动态速度计
  - [x] 6.1 新建 `src-tauri/src/app/tray.rs`：`TrayIconBuilder` 托盘，菜单项按原 tray.json（id 保持原命名），点击显隐主窗口 + 左键切换 + 焦点状态通知
  - [x] 6.2 新建 `src-tauri/src/app/menu.rs`：按平台构建应用菜单（win32 file/task/edit/window/help 五组，darwin app 分组），id 保持原命名，`on_menu_event` 统一分发（原生命令 Rust 处理 / 渲染命令转发 command:dispatch）
  - [x] 6.3 动态速度计：前端 main.js updateTray 修正为 `{width:132, height:32, data:[...]}`（66×16×scale2，桥接层改动）；Rust 端 `application:update-tray` 解码 PNG/RGBA → `Image::new_owned` → 更新托盘图标；**上传频率 ≤1s 节流**（Rust 端记录上次更新时间）

- [x] Task 7：命令补全 + 事件处理 + 配置联动
  - [x] 7.1 补齐 `COMMAND_NAMES`（+17 项渲染命令：new-task/new-bt-task/task-list/preferences 等）与 `handle_command` 实现（relaunch/quit/show/hide/reset-session/factory-reset/change-theme/change-locale/auto-hide-window/open-external/reveal-in-folder/open-file/clear-recent-tasks/help:* 等全部实现）
  - [x] 7.2 4 类事件（speed-change/download-status-change/progress-change/task-download-complete）驱动：任务栏进度条（ProgressBarState）、完成系统通知、防休眠状态跟踪（`app/energy.rs`）
  - [x] 7.3 8 类配置变化即时联动（open-at-login→autostart、proxy→引擎选项（已有）、run-mode→启动隐藏、locale/theme→转发前端、show-progress-bar→任务栏进度、auto-sync-tracker→占位日志）；save-preference 前后对比触发联动

- [x] Task 8：平台插件装配（单实例 / 自启 / 深链 / 通知 / 窗口状态 / 最近文档）
  - [x] 8.1 单实例：`tauri-plugin-single-instance`（2.4.3），二次启动聚焦已有窗口
  - [x] 8.2 开机自启：`tauri-plugin-autostart`（2.5.1），`open-at-login` 联动即时生效（启动参数 `--opened-at-login=1` 对齐 Electron）
  - [x] 8.3 深链：`app/protocol.rs` 自实现（Windows `reg add` HKCU 注册 mo/motrix/magnet，dev 跳过注册，打包产物注册；macOS/Linux 打包期注册）；深链 URL 分发：magnet→add_torrent、http(s)/ftp→add_uri、mo:/motrix:→命令分发（未用 tauri-plugin-deep-link——Windows 运行时注册依赖打包机制，取舍已注释）
  - [x] 8.4 任务完成系统通知：`tauri-plugin-notification`（2.3.3），TaskEvent 'complete' 订阅 + `task-notification` 配置控制
  - [x] 8.5 窗口状态保存：`app/window.rs` 自实现（window-state.json，500ms 节流，`keep-window-state` 控制，重启恢复）
  - [x] 8.6 最近文档：Windows SHAddToRecentDocs 需 windows crate 过重，降级为日志占位（注释说明接入点）

- [x] Task 9：P1 能力（UPnP / 自动更新 / 防休眠）
  - [x] 9.1 UPnP：`app/upnp.rs` 用 `igd`（0.12.1），`enable-upnp` 联动 listen-port/dht-listen-port 映射（spawn_blocking 异步，失败仅 warn）
  - [x] 9.2 自动更新：`app/updater.rs` 返回 `{available:false, reason:"update-source-not-configured"}` 优雅降级（已确认策略；发布流程就绪后接入 tauri-plugin-updater，取舍已注释）
  - [x] 9.3 电源防休眠：`app/energy.rs` 状态跟踪 + 日志 + `is_downloading()` 接入点（避免 windows crate 重依赖，取舍已注释）

## Phase 5：CLI 命令行工具

- [x] Task 10：motrix-rpc 客户端 + motrix-cli 骨架
  - [x] 10.1 `motrix-rpc/src/client.rs`：轻量 JSON-RPC 客户端 `JsonRpcClient`（reqwest 0.13 blocking + rustls、token 认证、`ClientError` 错误映射，含 5 个单元测试）
  - [x] 10.2 新建 `crates/motrix-cli`（clap derive，加入根 workspace members）：`main.rs` 参数解析 + 远程/daemon 双模式入口
  - [x] 10.3 全局参数 `--rpc-url`/`--rpc-secret`/`--json`/`--quiet`；secret 解析顺序：参数 → 环境变量 `MOTRIX_RPC_SECRET` → system.json
  - [x] 10.4 `cargo build -p motrix-cli` 通过，`--help` 完整

- [x] Task 11：远程模式命令
  - [x] 11.1 `add`（多 URL，aria2.addUri + options）/ `add-torrent`（magnet/base64/文件路径 → aria2.addTorrent）/ `list [active|waiting|stopped]` / `status <gid>`
  - [x] 11.2 `pause` / `pause-all` / `resume` / `resume-all` / `remove [-f] [--delete-files]` / `purge`
  - [x] 11.3 `config get/set`：systemKeys → `aria2.changeGlobalOption`；userKeys → `motrix.saveUserConfig`（服务端已实现）
  - [x] 11.4 `engine status`（system.multicall 一次取 getVersion+getGlobalStat）；`--json` 直接透传 RPC result（to_string_pretty）；默认人类可读；`--quiet` 抑制非错误输出
  - [x] 11.5 集成测试：client.rs 5 例 + e2e.rs 全流程（见 Task 12）

- [x] Task 12：saveUserConfig 服务端 + daemon 模式 + 端到端验收
  - [x] 12.1 `motrix.saveUserConfig`/`motrix.getUserConfig` 实现（经 ConfigManager 写 user.json userKeys；`CoreRpcBackend` 迁至 `motrix-core/src/rpc_backend.rs`，src-tauri 用 re-export shim 复用——GUI 与 daemon 共享同一份后端）
  - [x] 12.2 `motrix-cli daemon`：复用 `motrix-core` 引擎（ConfigManager+TaskManager+restore_session）+ JsonRpcServer；端口占用检测 → 报错退出（非零退出码）提示先关闭 GUI（已确认策略）；Ctrl+C 退出保存 checkpoint
  - [x] 12.3 端到端验收（`crates/motrix-cli/tests/e2e.rs`）：进程内 daemon + 外部客户端——getVersion/getGlobalStat → addUri → tellStatus 轮询至 complete（字段与文件一致、数值为字符串、与 tellStatus 契约一致）→ pause/unpause → remove → purge → saveUserConfig/getUserConfig 落盘验证；`cargo test -p motrix-cli` 通过

# Task Dependencies

- [Task 1] 依赖 [Phase 2 完成]（task.rs / engine.rs / broadcaster.rs 已就绪）
- [Task 2] 依赖 [Task 1]
- [Task 3] 依赖 [Task 2]
- [Task 4] 依赖 [Task 3]（进度数据源）
- [Task 5] 依赖 [Task 3]、[Task 4]；与 [Task 4] 部分并行（5.1/5.2 依赖 3，5.4 依赖 3）
- [Task 6] 依赖 [Phase 0-2 完成]；与 [Task 1]~[Task 5] 无依赖，可并行
- [Task 7] 依赖 [Task 6]；与 [Task 1]~[Task 5] 无依赖，可并行
- [Task 8] 依赖 [Task 7]、[Task 2]（magnet 深链 add_torrent）
- [Task 9] 依赖 [Task 8]；9.1 依赖 Phase 3 的 listen-port
- [Task 10] 依赖 [Phase 2 完成]；与 [Task 1]~[Task 9] 无依赖，可并行
- [Task 11] 依赖 [Task 10]、[Task 12]（config userKeys 部分）
- [Task 12] 依赖 [Task 10]、[Task 2]（daemon 下 BT 能力）

**实际执行情况**：
- Wave 1 并行：Agent A（Task 1-5 Phase 3）+ Agent B（Task 10-11 CLI 骨架/命令）
- Wave 2 并行：Agent C（Task 6-9 Phase 4）+ Agent E（Task 12 daemon/后端迁移/端到端）
- 最终验证：`cargo check --workspace` 通过、`cargo test --workspace` 全绿（见 checklist.md）
