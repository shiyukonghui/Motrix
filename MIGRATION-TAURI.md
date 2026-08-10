# Motrix 迁移文档：Electron + aria2 → Tauri 2 + Rust

> 目标：将 Motrix 从 `Electron + aria2c 外部进程` 架构迁移为 `Tauri 2 + Rust 原生下载引擎` 架构，
> **前端 UI 保持不变**，下载功能使用 **Rust 完整迁移并做性能优化**。

---

## 1. 文档目的与范围

- **目的**：给出从 Electron 版 Motrix 迁移到 Tauri + Rust 的完整方案，供开发团队评审与实施。
- **范围**：
  - 现状架构分析与数据流梳理；
  - 目标架构设计（Rust 下载引擎 + aria2 兼容 JSON-RPC 服务 + Tauri 集成层）；
  - 前端"零 UI 改动"的兼容策略；
  - 配置/会话数据的迁移；
  - 分阶段实施计划与风险对策。
- **不在范围内**：本次不讨论 UI 交互细节改动（要求 UI 不变）；不讨论 Android/iOS 移动端。

---

## 2. 现状分析

### 2.1 技术栈

| 层次 | 技术 | 说明 |
| --- | --- | --- |
| 壳 | Electron 22 | 主进程 + 渲染进程 |
| 前端框架 | Vue 2.7 + Vuex + Vue Router + element-ui 2 | 组件化 UI |
| 构建 | webpack 5（electron-vue 脚手架） | `.electron-vue/*.config.js` |
| 下载引擎 | **aria2c 外部进程** | 通过 JSON-RPC(WebSocket) 通信 |
| 配置存储 | electron-store | `user.json` / `system.json` |
| 更新 | electron-updater | |
| 渲染进程原生能力 | @electron/remote | `shell`、`nativeTheme` 等 |
| 国际化 | i18next | 30+ 语言 |

### 2.2 进程与模块结构

```
┌─────────────────────────────────────────────────────────┐
│ 主进程 (src/main)                                        │
│  Application.js —— 事件总线 / IPC 分发 / 生命周期          │
│  ├── Engine.js        启动/停止 aria2c 子进程              │
│  ├── EngineClient.js  JSON-RPC 客户端（改全局选项/关闭）    │
│  ├── ConfigManager.js user.json + system.json            │
│  ├── UPnPManager / AutoLaunchManager / UpdateManager     │
│  ├── EnergyManager / ProtocolManager / ExceptionHandler  │
│  └── ui/: WindowManager / MenuManager / TrayManager /    │
│           DockManager / TouchBarManager / ThemeManager    │
└─────────────────────────────────────────────────────────┘
                        │  IPC: 'command' / 'event' / invoke('get-app-config')
┌─────────────────────────────────────────────────────────┐
│ 渲染进程 (src/renderer, Vue)                              │
│  ├── api/Api.js       包装 aria2 JSON-RPC 调用            │
│  ├── components/Native/EngineClient.vue 轮询 + 事件绑定   │
│  ├── components/Native/Ipc.vue        主进程命令分发      │
│  ├── components/Native/DynamicTray.vue 托盘速度计(Canvas) │
│  ├── components/Native/TitleBar.vue   自绘标题栏          │
│  └── store/           Vuex（app / task / preference）    │
└─────────────────────────────────────────────────────────┘
                        │  WebSocket 直连 (127.0.0.1:16800/jsonrpc)
┌─────────────────────────────────────────────────────────┐
│ aria2c 子进程 (extra/<platform>/<arch>/engine/aria2c)     │
└─────────────────────────────────────────────────────────┘
```

### 2.3 下载引擎数据流（现状）

1. **启动**：`Engine.start()` 用 `--conf-path` + `--save-session` + 动态 `--key=value` 参数 `spawn` aria2c；
2. **通信**：渲染进程 `Api.js` 通过 **WebSocket** 直连 `ws://127.0.0.1:16800/jsonrpc`，主进程的 `EngineClient` 也用同一协议；
3. **任务操作**：`addUri` / `addTorrent` / `pause` / `unpause` / `remove` / `changeOption` 等 → aria2 JSON-RPC；
4. **状态获取**：渲染进程轮询（间隔 500ms~6s 自适应）`tellActive` / `tellWaiting` / `tellStopped` / `getGlobalStat` / `tellStatus`；
5. **事件推送**：aria2 通过 JSON-RPC 通知推送 `onDownloadStart` / `onDownloadComplete` / `onDownloadError` / `onBtDownloadComplete` 等；
6. **断点续传**：aria2 控制文件 `*.aria2` + 会话文件 `download.session`。

### 2.4 渲染进程依赖的 Electron 原生 API（迁移必须兼容）

| 文件 | 使用的 API |
| --- | --- |
| `api/Api.js` | `ipcRenderer.invoke('get-app-config')` |
| `utils/native.js` | `@electron/remote` 的 `shell.showItemInFolder` / `shell.openPath` / `shell.trashItem`、`nativeTheme` |
| `pages/index/main.js` | `vue-electron`（`$electron` 挂载）、`ipcRenderer.send('command', …)` |
| `components/Native/*` | `$electron.ipcRenderer.send` / `.on` / `.removeListener` |
| 全局 | `electron-is`（`is.renderer()` / `is.macOS()` / `is.windows()` / `is.linux()` / `is.dev()`） |

> **关键结论**：**视觉层**（业务 `.vue` 组件、样式、Vuex、路由、i18n）**不依赖 Electron**；
> 仅 `components/Native/*`（EngineClient/Ipc/TitleBar/DynamicTray 等桥接组件）与 `api/Api.js`、`utils/native.js` 依赖平台 API。
> 所以"保持 UI 不变"= 视觉组件与状态层不变 + 替换桥接/数据管线（详见 5.8 与第 6 章）。

### 2.5 配置与数据

- `user.json`：应用级配置（主题、语言、代理、运行模式、托盘速度计、开机自启等），键见 `src/shared/configKeys.js` 的 `userKeys`；
- `system.json`：aria2 全局选项（`dir`、`split`、`max-concurrent-downloads`、`bt-tracker`、`rpc-listen-port`、`rpc-secret` 等），键见 `systemKeys`；
- `download.session`：aria2 会话文本（每行一条任务），用于重启后恢复；
- 数据目录：`app.getPath('userData')`（非便携版）；`PORTABLE_EXECUTABLE_DIR`（便携版）。

---

## 3. 迁移目标与总体策略

### 3.1 目标

1. 壳与 UI：Tauri 2 + WebView，**UI 与交互零改动**；
2. 下载：**完全去掉 aria2c 子进程**，由 Rust 原生下载引擎实现 HTTP / FTP / BitTorrent 下载；
3. 性能：下载并发、内存占用、启动速度、二进制体积全面优于或持平 Electron 版（见 5.7 对比表）；
4. 兼容：历史 `user.json` / `system.json` / `download.session` 可平滑迁移；
5. 平台：Windows / macOS / Linux 三端一致。

### 3.2 总体策略：**"契约保持、引擎替换"**

前端与后端之间建立**两条职责分离的通道**；aria2 兼容 JSON-RPC **仅作为对外接口保留**，前端不依赖它。因此：

1. **前端 → 后端（操作）走 Tauri command**：`addUri` / `pause` / `resume` / `remove` / `changeOption` 等任务操作映射为 `invoke('add_uri')` / `invoke('pause_task')` 等 command，由 Tauri capability 做权限控制；
2. **后端 → 前端（状态）走 Tauri 事件**（见 5.8）：Rust 端 `engine:*` 节流推送精简快照，前端增量更新，解决 Electron 版"轮询 + 大 bitfield/peers 载荷"白屏问题；
3. **JSON-RPC 服务（aria2 兼容，127.0.0.1:16800）保留为"对外接口"**：供 `motrix-cli` 远程模式（第 9 章）、外部 aria2 兼容客户端（AriaNg 等）与向后兼容使用；**前端 UI 不走 JSON-RPC**（操作与状态分别收敛到 Tauri command / 事件，通道不重复）；
4. **上述通道背后挂 Rust 原生下载引擎**（KGet + librqbit，见第 5 章），替代 aria2c；
5. **原 Electron IPC**（`command` / `event` / `get-app-config`）映射为 Tauri command + event，前端只需替换 Electron shim；
6. **配置格式沿用** `user.json` / `system.json`（JSON），Rust 侧读写，保证升级无缝。

### 3.3 为什么前端 UI 能"不动"

- 所有**视觉组件**（`.vue`）只消费 Vuex 数据，不感知数据来自轮询、事件推送还是 invoke，因此 UI 零改动；
- 需替换的仅是平台桥接点（第 2.4 节）+ 数据管线：`EngineClient.vue`（轮询 → 事件订阅）、`Api.js`（操作/查询方法 → Tauri command 与 on-demand invoke，见 5.8 与第 8 章），统一收敛在 `shims/` 目录（详见第 6 章）。

---

## 4. 目标架构设计

### 4.1 目标目录结构

> 说明：Rust 端采用 **cargo workspace**，引擎 / RPC / CLI 拆为独立 crate，GUI 与 CLI 共享同一份引擎代码（CLI 设计见第 9 章）。

```
motrix/
├── Cargo.toml                       # workspace 根（members: src-tauri, crates/*）
├── src/                             # 前端（保留现有，仅加 shim）
│   ├── renderer/                    # 现有渲染进程代码（不变）
│   ├── shims/                       # ★ 新增：electron 兼容层
│   │   ├── electron-is.js           #   is.renderer()/is.macOS()… 平台判断
│   │   ├── ipcRenderer.js           #   send/on/invoke → Tauri event/invoke
│   │   ├── events.js                #   listen('engine:*') → Vuex 增量 mutation（见 5.8）
│   │   ├── remote-shell.js          #   shell.showItemInFolder/openPath/trashItem
│   │   ├── remote-nativeTheme.js    #   nativeTheme.shouldUseDarkColors
│   │   └── index.js                 #   统一注入 $electron / __shims__
│   └── index.html                   # Tauri 入口页
├── src-tauri/                       # Tauri 应用 crate（GUI 壳）
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   ├── icons/
│   └── src/
│       ├── main.rs                  # 入口：单实例、启动流程
│       ├── lib.rs                   # tauri::Builder 装配
│       ├── commands.rs              # Tauri commands（窗口/配置/任务操作/详情拉取/目录选择…）
│       └── app/                     # 应用级 Manager 的 Rust 实现
│           ├── window.rs            # 窗口管理（原 WindowManager）
│           ├── tray.rs              # 系统托盘 + 速度计图像
│           ├── menu.rs              # 应用菜单
│           ├── autostart.rs         # 开机自启（tauri-plugin-autostart）
│           ├── protocol.rs          # 深链协议 mo:/motrix:/magnet:
│           ├── updater.rs           # 更新（tauri-plugin-updater）
│           └── energy.rs            # 电源管理（防休眠）
├── crates/
│   ├── motrix-core/                 # ★ 共享引擎核心（GUI 与 CLI 共用）
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── task.rs              # 任务模型/状态机（与 aria2 字段对齐）
│   │   │   ├── kget.rs              # KGet 封装：builder/AdvancedDownloader 配置映射
│   │   │   ├── bt.rs                # librqbit 封装：magnet/.torrent/peers/seeding
│   │   │   ├── session.rs           # 会话持久化（download.session 导入 + checkpoint）
│   │   │   ├── options.rs           # 全局/任务选项（映射 systemKeys）
│   │   │   ├── config.rs            # user.json / system.json 读写 + 变更事件
│   │   │   └── broadcaster.rs       # StateBroadcaster：节流合并 → engine:* 事件（见 5.8）
│   │   └── Cargo.toml
│   ├── motrix-rpc/                  # ★ aria2 兼容 JSON-RPC（服务端 + 客户端）
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── server.rs            #   axum: WS + HTTP POST 双通道
│   │   │   ├── methods.rs           #   aria2.* / system.* / motrix.* 方法实现
│   │   │   ├── notify.rs            #   通知推送（onDownloadStart 等）
│   │   │   └── client.rs            #   轻量 JSON-RPC 客户端（CLI 复用，见第 9 章）
│   │   └── Cargo.toml
│   └── motrix-cli/                  # ★ CLI 二进制（见第 9 章）
│       ├── src/
│       │   └── main.rs              # clap 命令解析 + 远程/daemon 双模式
│       └── Cargo.toml
└── extra/                           # 删除（不再需要 aria2c）
```

### 4.2 Rust 依赖选型（优先采用成熟库，禁止造轮子）

> 选型原则：**下载引擎层全部采用社区成熟 crate**；Motrix 自研部分仅限"任务编排 + aria2 契约适配"这类胶水层。

| 用途 | 采用的成熟库 | 成熟度 / 许可 | 说明 |
| --- | --- | --- | --- |
| 下载引擎核心（HTTP/HTTPS/FTP/SFTP/WebDAV/Metalink） | **`Kget` v1.7** | 9 个版本、持续迭代 / MIT | 下载管理器**库**：`kget::builder(url)` 链式 API（连接数、限速、代理、校验和、重试），`AdvancedDownloader` 多线程并行分段下载，`.spawn()` 返回进度 `Receiver`；内存优化（16KB 流式读写 + 2MB 缓冲） |
| BitTorrent（磁力/.torrent） | **`librqbit` v7.x** | 2000+ commits、活跃维护 / Apache-2.0 | 纯 Rust BT 库：DHT、PEX、ut_metadata、uTP(BEP29)、Web Seeds(BEP19)、LPD；rqbit 同仓库含 Tauri 桌面示例；**KGet 的 BT 功能同样基于 librqbit，组合已被验证** |
| JSON-RPC 服务（aria2 兼容适配层） | `axum` + `tungstenite` | 成熟 | 自研薄适配层：把 KGet/librqbit 状态映射为 aria2 协议（属胶水，非下载引擎轮子）；`motrix-rpc` 同时提供轻量 JSON-RPC **客户端**（供 CLI） |
| CLI 参数解析 | `clap`（derive） | 成熟 | `motrix-cli` 命令解析 |
| CLI 进度显示 | `indicatif` | 成熟 | `list`/`status` 人类可读进度条/表格 |
| 异步运行时 | `tokio` | 成熟 | |
| 序列化 | `serde` + `serde_json` | 成熟 | |
| 配置 | `serde_json` 直接读写 `user.json`/`system.json` | — | 保持 Electron 版键名与默认值 |
| UPnP | `igd` | 成熟 | BT 监听端口映射 |
| 深链协议 | `tauri-plugin-deep-link` | Tauri 官方 | `mo:` / `motrix:` / `magnet:` |
| 托盘 | `tauri::tray` | Tauri 内置 | |
| 自动更新 | `tauri-plugin-updater` | Tauri 官方 | |
| 开机自启 | `tauri-plugin-autostart` | Tauri 官方 | |
| 打开文件管理器/文件 | `tauri-plugin-shell` + `tauri-plugin-opener` | Tauri 官方 | |
| 单实例 | `tauri-plugin-single-instance` | Tauri 官方 | |

**明确的否决项（调研结论）**：

- **`aria2-core` / `aria2-rpc`（aria2-rust，balovess）**：功能上是"aria2 的 Rust 替身"（34 个 RPC 方法、WS 7 事件、.aria2 控制文件、会话兼容），本可让前端与协议零改动——但 **GPL-2.0-or-later 许可与 Motrix 的 MIT 协议冲突**，且项目仅 v0.2.x（2026-05 起步、119 commits、下载量极低），未达生产级成熟度 → **否决**，仅作为协议行为参考实现；
- **`durl` / `dlm` / `stormdl` / `hydra-dl` / `tur-rs` / `furl-cli`**：HTTP 分段下载器，但成熟度或维护状态不足（单版本、下载量个位数、无稳定库 API 或仍处开发期）→ **否决**；
- **基于 `reqwest` 自研分段下载器**：违反"禁止造轮子"原则 → **否决**。

> 注：所有 Rust 代码须包含中文注释（项目约定）。

### 4.3 进程模型对比

| | Electron 版 | Tauri 版 |
| --- | --- | --- |
| 壳进程 | Electron main（Node） | Tauri main（Rust） |
| 渲染进程 | Chromium renderer | 系统 WebView |
| 下载引擎 | aria2c 子进程（外部二进制） | **Rust 主进程内异步任务**（tokio） |
| 通信 | WS→aria2 + IPC→main | Tauri event/invoke（前端）+ WS→Rust JSON-RPC（对外接口） |
| 进程数 | 2+ 个 Node 进程 + aria2c | 1 个 Rust 进程 + WebView |

---

## 5. 下载引擎集成设计（核心：基于成熟库，禁止造轮子）

### 5.1 总体结构（成熟库组合）

下载引擎层**不复用 aria2c、不自研下载算法**，而是组合两个成熟 Rust 库；Motrix 自研部分仅为"任务编排 + aria2 契约适配 + 状态广播"胶水层：

> 胶水层除上图 JSON-RPC 适配外，还包含 **StateBroadcaster**（`motrix-core/broadcaster.rs`）：把任务状态节流合并后经 `tauri::emit` 推送 `engine:*` 事件给前端（详见 5.8），与 JSON-RPC 通道并存、职责分离。

```
┌──────────────────────────────────────────────────────────────┐
│ Motrix 自研胶水层（业务逻辑，非下载引擎轮子）                    │
│  任务仓库 + 状态机（字段与 aria2 对齐）→ 暂停/恢复/删除/队列/会话 │
│  JSON-RPC 适配层（motrix-rpc/server.rs）→ aria2 协议 ←→ KGet/librqbit │
└──────────────┬─────────────────────────────────┬──────────────┘
               │                                 │
        ┌──────▼───────┐                 ┌───────▼───────┐
        │ KGet (引擎)   │                 │ librqbit      │
        │ HTTP/HTTPS   │                 │ BitTorrent    │
        │ FTP/SFTP/    │                 │ DHT/PEX/      │
        │ WebDAV/Metalink │               │ uTP/磁力/种子 │
        └──────────────┘                 └───────────────┘
```

### 5.2 任务模型（胶水层）

任务字段与 aria2 `tellStatus` 返回结构对齐，保证前端 `TaskItem.vue` / `TaskProgress.vue` 等组件零改动；任务状态机与"引擎调用"解耦，由适配层在 KGet/librqbit 的进度事件与任务字段间做转换：

```rust
// engine/task.rs（示意）
pub struct Task {
    pub gid: String,                // 同 aria2 gid（16 位 hex）
    pub status: TaskStatus,         // active/waiting/paused/error/complete/removed/seeding
    pub total_length: u64,
    pub completed_length: u64,
    pub upload_length: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    pub dir: PathBuf,
    pub files: Vec<TaskFile>,       // path/uris/length/completedLength/selected
    pub bittorrent: Option<BittorrentInfo>, // info/announceList/mode
    pub info_hash: Option<String>,
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
    pub connections: u32,           // 当前连接数
    pub gid_of_progress: ...,
}
```

### 5.3 HTTP/HTTPS/FTP 下载 → KGet

- **入口 API**：`kget::builder(url)`（单文件链式）与 `AdvancedDownloader`（多线程并行分段下载），`.spawn()` 返回进度 `Receiver` —— 适配层订阅后转为 `tellStatus` 字段与统计；
- **aria2 选项 → KGet 配置映射**（胶水层职责）：

  | aria2 选项（`systemKeys`） | KGet 对应能力 |
  | --- | --- |
  | `max-connection-per-server` / `split` | `builder.connections(n)`（并行分段连接数） |
  | `max-download-limit` / `max-overall-download-limit` | 速度限制（`kget` 内置限速器） |
  | `all-proxy` / `no-proxy` / 代理认证 | `ProxyConfig`（HTTP/SOCKS5） |
  | `checksum` | `DownloadOptions` 校验和验证（SHA-256/512、SHA-1、MD5、BLAKE3） |
  | `max-tries` / `retry-wait` | 重试配置 |
  | `header` / `cookie` | 自定义 HTTP 头 / Cookie |
  | `user-agent` | UA 设置 |

- FTP/SFTP/WebDAV/Metalink 由 KGet 一并覆盖，能力差异按"协议支持矩阵"在实施时逐项勾选（见第 11 章风险与对策）。

### 5.4 断点续传与会话（胶水层）

- **断点续传**：KGet 基于 HTTP Range 自带断点续传（`.spawn()` 的 `Receiver` 可拿到已下载字节），胶水层据此恢复进度；
- **旧 `*.aria2` 控制文件**：为 aria2 私有二进制格式，**不做解析**（避免实现 aria2 私有格式解析器）。迁移策略：从 `download.session` 重建任务列表（URL/目录/文件名），未完成任务提示用户重新下载，已完成任务不受影响；
- **会话 `download.session`**：解析现有文本格式（`gid status path urls…`）导入任务；此后由胶水层维护自有 checkpoint（JSON，记录每任务已下载字节与进度），退出时 `will-exit` 保存、下次启动恢复。

### 5.5 BitTorrent → librqbit

- **磁力链接 / .torrent**：磁力先以 metadata 任务呈现（`bittorrent.info` 为空），获取 metadata 后再转为完整 BT 任务 —— 与 aria2 行为一致，前端 `isMagnetTask` 判断可复用；
- **能力**：DHT / PEX / ut_metadata / uTP(BEP29) / Web Seeds(BEP19) / LPD / 文件选择，均由 librqbit 提供；
- **字段暴露**：`infoHash`、`announceList`、`peers`（`getPeers`）、`seeder`、`numSeeders`、`bitfield`（进度条走 librqbit piece 统计 → bitfield → percent）；
- **种子上传**：`keep-seeding` / `seed-ratio` / `seed-time` 映射到 librqbit 的 seeding 控制；
- rqbit 仓库自带 HTTP API 与 **Tauri 桌面示例**，集成方式可直接参考其代码（Apache-2.0，兼容）。

### 5.6 JSON-RPC 适配层（对外接口，aria2 兼容契约）

> **定位**：本适配层**仅服务外部客户端**——`motrix-cli` 远程模式（第 9 章）、第三方 aria2 兼容客户端（AriaNg 等）与向后兼容；**前端 UI 不依赖它**（前端状态走 `engine:*` 事件、操作走 Tauri command，见 5.8）。端口仍默认 `127.0.0.1:16800`。

- 协议：`ws://127.0.0.1:{rpc-listen-port}/jsonrpc` 与 `http://…/jsonrpc` 双通道（外部客户端 `JSONRPCClient._send` 在 WS 断开时自动降级 HTTP，适配层两端都实现）；
- 认证：`token:{secret}` 首参数（`rpc-secret`）；
- 方法清单（对应 aria2 标准接口，供 CLI / 外部客户端）：

```
// 任务
aria2.addUri / aria2.addTorrent / aria2.addMetalink
aria2.tellStatus / aria2.tellActive / aria2.tellWaiting / aria2.tellStopped
aria2.getPeers / aria2.getGlobalStat / aria2.getVersion
aria2.pause / aria2.pauseAll / aria2.forcePause / aria2.forcePauseAll
aria2.unpause / aria2.unpauseAll / aria2.remove / aria2.forceRemove
aria2.changeOption / aria2.changeGlobalOption
aria2.getOption / aria2.getGlobalOption
aria2.saveSession / aria2.purgeDownloadResult / aria2.removeDownloadResult
// 系统
system.multicall / system.listNotifications / system.listMethods
```

> 注：`aria2.addMetalink` 前端无实际触发入口（`commands.js` 中为 TODO），一期可返回占位错误（见 8.1），其余方法全部实现。

- **通知推送**（对应原 aria2 通知事件）：
  `onDownloadStart` / `onDownloadStop` / `onDownloadPause` / `onDownloadComplete` / `onDownloadError` / `onBtDownloadComplete`；
  > 职责边界：上述 RPC 通知保留给**外部 aria2 客户端**与兼容路径；UI 侧任务事件改为订阅 `engine:task-event`（见 5.8），同一引擎状态经两通道按需输出；
- **参数格式**：所有数值返回字符串（aria2 惯例），前端 `Number()` 转换处不变。

### 5.7 与 aria2 的性能对比（优化项清单）

| 维度 | Electron + aria2c | Tauri + Rust 引擎 | 收益 |
| --- | --- | --- | --- |
| 进程 | Node×2 + aria2c 子进程 | 单 Rust 进程 | 内存 / 启动更快 |
| 并行下载 | aria2 多连接逻辑 | KGet `AdvancedDownloader` 并行分段 + librqbit 并行 piece | 带宽利用率等同甚至更优 |
| 并发 | `max-concurrent-downloads` 任务级 | 任务级编排 + 库级并行 | 更充分的带宽利用 |
| 体积 | Electron + Chromium（≈100MB+） | WebView + Rust（≈10~20MB） | 安装包大幅减小 |
| 内存 | V8 常驻 + aria2 | 无 V8（仅 WebView 渲染 UI） | 常驻内存更低 |
| 断点续传 | aria2 私有控制文件 | KGet Range 续传 + 自有 checkpoint | 新任务无缝，旧任务重下 |
| 维护 | 外部二进制供应链 | 仅 `crates.io` 依赖 | 供应链简化 |
| 许可 | aria2 GPL-2.0（二进制分发） | KGet(MIT) + librqbit(Apache-2.0) | 与 Motrix MIT 兼容 |

### 5.8 前端状态同步：轮询 → Tauri 事件推送（解决长时下载白屏）

**现状问题（白屏根因，已在 Electron 版复现）**：

1. [EngineClient.vue](src/renderer/components/Native/EngineClient.vue) 的轮询循环（500ms~6s 自适应）在 BT 详情开启且启用 peers 时，叠加调用 `fetchItemWithPeers`（`tellStatus` + `getPeers`）；
2. `tellStatus` 每次返回**完整 `bitfield` 十六进制串**（每 1 字符 = 4 个 piece 的下载状态位，大种子可达数万~数十万字符），`getPeers` 返回全部 peer 列表 —— WebSocket 载荷达数 MB 且被高频 `JSON.parse`；
3. [TaskGraphic/Index.vue](src/renderer/components/TaskGraphic/Index.vue) 的 `atoms` computed **将 bitfield 的每个字符生成一个 SVG DOM 节点**，且每次轮询 prop 变化即全量重建（`TaskActivity.vue` 直接绑定 `task.bitfield`）；
4. Vuex 每次 `UPDATE_CURRENT_TASK_ITEM` 全量覆盖，触发所有依赖组件重渲染。

四者叠加使渲染进程单线程饱和 → **WebView 白屏 / 无响应**（长时 BT 下载时必现）。

**Tauri 事件机制改造（Rust 推，前端收，根治白屏）**：

```
┌─ Rust 主进程 ────────────────────────────────────────────┐
│  引擎状态仓库（KGet/librqbit 进度回调更新）                 │
│  StateBroadcaster 状态广播器：                            │
│   ├─ 压缩快照（不含 bitfield 原始串 / peers 明细）          │
│   ├─ 节流合并（默认 1s，按 numActive 自适应 0.5s~6s）      │
│   └─ 订阅者定向推送（index 窗口 vs 详情窗口）               │
└────────────────────────┬─────────────────────────────────┘
                         │ tauri::Emitter::emit("engine:snapshot", …)
┌────────────────────────▼─────────────────────────────────┐
│ WebView（Vue）                                           │
│  shims/events.js: listen("engine:snapshot")              │
│   → Vuex 增量 mutation（diff 变更任务，非全量覆盖）         │
│   → invoke("get_task_detail") 仅在详情面板打开时调用        │
└──────────────────────────────────────────────────────────┘
```

**事件与载荷设计**：

| 事件 | 载荷（均为精简字段） | 触发时机 |
| --- | --- | --- |
| `engine:snapshot` | 全局统计 + 任务列表精简字段（`gid/status/percent/speed/completedLength/totalLength/…`），**不含 bitfield 原串** | 节流合并后周期性推送 |
| `engine:task-event` | `gid` + 事件类型（对应原 `onDownloadStart` 等通知） | 任务状态变化时立即推送 |
| `engine:global-stat` | `downloadSpeed/uploadSpeed/numActive/numWaiting/numStopped` | 周期推送（托盘/标题速度用） |
| `invoke('get_task_detail')` | 单任务全字段（**bitfield 已在 Rust 端降采样**到目标宽度，如 240 atom） | 仅详情面板打开时调用 |
| `invoke('get_peers')` | peer 列表（**分页**，默认 100 条） | 详情面板 Peers 标签切换时调用 |

**对前端的影响（UI 视觉不变，仅数据层改造）**：

- 视觉组件 `TaskItem.vue` / `TaskGraphic` / `TaskPeers.vue` **全部保持不变**（它们只消费 Vuex 数据）；
- 修改点集中在数据管线：`EngineClient.vue`（轮询 → 事件订阅）、`Api.js`（操作/查询方法 → Tauri command 与 on-demand invoke）、新增 `shims/events.js`；
- `TaskGraphic` 因收到的是 Rust 降采样后的 bitfield（≤ 目标宽度字符数），DOM 节点数从"数十万"降为"≤ 240"，彻底消除白屏；
- **前端不再走 JSON-RPC**：任务操作 → Tauri command，状态 → `engine:*` 事件；JSON-RPC 适配层仅保留为**对外接口**（CLI 远程模式、外部 aria2 兼容客户端，见 3.2 / 5.6）。

> 白屏问题同时是 **Phase 2/3 的验收标准之一**：长时 BT 下载 ≥ 4 小时、任务列表 + 详情图保持流畅不白屏。

---

## 6. 前端兼容层设计（UI 不变的关键）

### 6.1 统一替换入口

在 `src/index.html` / `main.js` 顶部引入 `src/shims/index.js`，将 `$electron`、`electron-is`、`@electron/remote` 全部替换为 shim 实现。

**改动边界（精确表述）**：

- **视觉组件与 Vuex store 不做改动**（`TaskItem.vue` / `TaskGraphic` / `TaskPeers.vue` / `components/Task/*` / `store/` 等只消费数据，不感知来源）；
- **数据管线组件需替换**：`components/Native/EngineClient.vue`（轮询 → 事件订阅）、`components/Native/Ipc.vue` / `TitleBar.vue` / `DynamicTray.vue`（Electron API → shim）、`api/Api.js`（轮询/操作方法 → 事件订阅 + Tauri command + on-demand invoke，见 5.8 与 8.2）——这些属于平台桥接/数据层，非视觉 UI；
- 全部改动统一收敛在 `shims/` 目录与上述 Native 组件内（详见附录 A 逐文件清单）。

### 6.2 平台判断 shim（替代 electron-is）

```js
// src/shims/electron-is.js
// 平台判断：Tauri 下通过 navigator.userAgent / window.__TAURI_INTERNALS__ 推断
const ua = navigator.userAgent.toLowerCase()
const platform = window.__TAURI_OS_PLATFORM__ // 由 Rust 端注入（hostname 或 command）

export const is = {
  renderer: () => true,               // Tauri 前端即渲染端
  main: () => false,
  macOS: () => platform === 'darwin',
  windows: () => platform === 'win32',
  linux: () => platform === 'linux',
  dev: () => import.meta.env.DEV || location.port === '1420' // Tauri dev 端口
}
```

> 在 `main.rs` 中 `webview` 注入 `window.__TAURI_OS_PLATFORM__`，一次注入全局生效。

### 6.3 IPC shim（替代 ipcRenderer）

```js
// src/shims/ipcRenderer.js
import { invoke } from '@tauri-apps/api/core'
import { listen, emit } from '@tauri-apps/api/event'

export const ipcRenderer = {
  // 原主进程监听 'command'，把 command+args 分发给各模块
  send (channel, ...args) {
    if (channel === 'command') {
      const [command, ...rest] = args
      return emit('command', { command, args: rest })
    }
    if (channel === 'event') {
      const [eventName, ...rest] = args
      return emit('event', { eventName, args: rest })
    }
  },
  // 主进程 → 渲染进程的 'command' 下发
  on (channel, listener) {
    if (channel === 'command') {
      return listen('command:dispatch', (e) => {
        listener(e, e.payload.command, ...e.payload.args)
      })
    }
    return listen(channel, listener)
  },
  removeListener (channel, listener) { /* 见实现细节 */ },
  invoke (channel, ...args) {
    if (channel === 'get-app-config') return invoke('get_app_config')
  }
}
```

Rust 端对应：

```rust
// src-tauri/src/commands.rs（示意）
#[tauri::command]
async fn get_app_config(state: State<AppState>) -> Result<ConfigBundle, String> {
    // 合并 userConfig + systemConfig + context（platform/arch/log-path/session-path…）
}

// 主进程接收 'command'/'event' 事件并分发
app.handle().listen("command", |e| {
    let (command, args) = parse(e.payload);
    dispatch_command(&command, args);       // 原 Application.handleCommands 的等价物
});
app.handle().listen("event", |e| {
    let (event_name, args) = parse(e.payload);
    dispatch_event(&event_name, args);      // speed-change / download-status-change…
});
```

> 任务操作（原 `Api.js` 的 JSON-RPC 调用）不经过 shim 的 `send('command')`，而是直接调用 Tauri command（`invoke('add_uri')` / `invoke('pause_task')` …，见 8.2）；`shims/ipcRenderer.js` 仅承载原 Electron IPC 的 `command` / `event` / `get-app-config` 通道。

### 6.4 原生能力 shim（替代 @electron/remote）

| 原 API | Tauri 实现 |
| --- | --- |
| `shell.showItemInFolder(path)` | Tauri command `show_item_in_folder`（Windows `explorer /select,`、macOS `open -R`、Linux `dbus`/`xdg`） |
| `shell.openPath(path)` | `tauri-plugin-opener` 的 `openPath` |
| `shell.trashItem(path)` | `tauri-plugin-shell` + 平台命令，或 `trash` crate（Rust 端 command） |
| `nativeTheme.shouldUseDarkColors` | Rust 端读取系统主题返回，或 `window.matchMedia('(prefers-color-scheme: dark)')` |

### 6.5 直接可用的 WebView 能力（无需改动）

> 说明：此处指"**传输能力**"可直接复用（协议层），不等于业务方法不改。UI 主路径的状态获取已改为 Tauri 事件（5.8），JSON-RPC 仅保留给外部客户端（CLI / 第三方 aria2 兼容客户端）与向后兼容。

- **WebSocket 传输**：WebView 原生支持 `WebSocket`；共享客户端库 `JSONRPCClient._send` 在 WS 断开时自动降级 HTTP——该客户端现由 `motrix-cli` / 外部客户端使用（前端数据路径不再连接 JSON-RPC）；
- `Notification` 系统通知（HTML5 Notification，Windows/macOS WebView 均支持）；
- 拖拽、剪贴板、`FileReader`（torrent 文件读取）、`<canvas>`（托盘速度计绘制）；
- `tray.worker.js`（web worker）绘制托盘图 → 经 `command` → Rust 更新托盘图标（二进制 ArrayBuffer → `Vec<u8>`）。

### 6.6 前端构建

- **一期**：沿用现有 webpack 配置，仅将 `target` 从 electron-renderer 改为 web，产物给 Tauri 的 `frontendDist`；
- **二期（可选）**：迁移 Vite，收益为构建速度与 dev 热更新体验（不影响 UI）。

---

## 7. 配置与数据迁移

### 7.1 配置文件

| 文件 | 现状（electron-store） | 迁移方案 |
| --- | --- | --- |
| `user.json` | JSON | Rust `motrix-core/src/config.rs` 直接读写同名 JSON，**键名与默认值完全保留** |
| `system.json` | JSON | 同上；`rpc-listen-port`/`rpc-secret` 仍作为 JSON-RPC 服务配置 |
| `download.session` | aria2 文本 | Rust 解析导入任务（URL/目录/文件名），恢复后由自有 checkpoint 持久化 |
| `*.aria2` 控制文件 | aria2 私有格式 | **不解析**（aria2 私有二进制格式）；旧未完成任务提示重下，已完成任务不受影响；新任务使用自有 checkpoint |
| `dht.dat` / `dht6.dat` | aria2 DHT 路由表 | librqbit 若格式不同则重建，可接受 |

### 7.2 数据目录

- 非便携版：沿用系统用户数据目录（Windows `%APPDATA%\motrix`、macOS `~/Library/Application Support/motrix`、Linux `~/.config/motrix`）。Tauri 的 `app_data_dir` 由 `bundle.identifier` 决定（默认 `com.tauri.dev`），需将 `tauri.conf.json` 的 `bundle.identifier` 与 `app_data_dir` 对齐到 Electron 版的 `motrix` 目录名，保证配置/会话无缝迁移；
- 便携版：保留 `PORTABLE_EXECUTABLE_DIR` 逻辑（Rust 端从当前 exe 目录推断）。

### 7.3 旧会话恢复流程

1. 启动时读取 `user.json`/`system.json`（不存在则写默认值）；
2. 解析 `download.session` → 重建任务列表（status、gid、URL、目录）；
3. 未完成任务（无 `.aria2` 控制文件续传能力）提示用户重新下载；已完成任务直接进入历史列表；
4. 迁移完成后可在设置中触发"重置下载会话"（等价 `application:reset-session`）。

---

## 8. IPC / RPC 契约对照表

### 8.1 aria2 RPC 方法（motrix-rpc 对外接口）

> 调用方：**`motrix-cli`（第 9 章）与外部 aria2 兼容客户端**；前端 UI 已改走 Tauri command / 事件（见 8.2），不再经此通道。

| 调用方 / 场景 | 方法 | 实现位置 |
| --- | --- | --- |
| CLI `engine status` / 外部 | `aria2.getVersion` | `motrix-rpc/methods.rs` |
| CLI `engine status` / 外部 | `aria2.getGlobalStat` | 引擎统计聚合 |
| CLI `add` / 外部 | `aria2.addUri`（multicall） | 经 `motrix-core` 创建任务 |
| CLI `add-torrent` / 外部 | `aria2.addTorrent` | 经 `motrix-core`（bt.rs） |
| CLI / 外部 | `aria2.addMetalink` | 一期返回占位错误（前端无触发入口） |
| CLI `list` / 外部 | `aria2.tellActive` / `tellWaiting` / `tellStopped` | 任务仓库查询 |
| CLI `status` / 外部 | `aria2.tellStatus` | 任务仓库查询 |
| CLI / 外部 | `aria2.getPeers` | `motrix-core`（bt.rs） |
| CLI `pause`/`resume`/`remove` / 外部 | `aria2.pause` / `unpause` / `remove` / `forceRemove` / `pauseAll` / `unpauseAll` | 任务状态机 |
| CLI / 外部 | `system.multicall` | 批量分发 |
| CLI `config set`（systemKeys）/ 外部 | `aria2.changeOption` / `changeGlobalOption` | `motrix-core/options.rs` |
| CLI / 外部 | `aria2.saveSession` | `motrix-core/session.rs` |
| CLI `config set`（userKeys） | `motrix.saveUserConfig`（`motrix.*` 扩展） | 见第 9 章 |

> 前端对应通道：上述方法在前端 UI 中由 **Tauri command** 等价替代（`invoke('add_uri')` / `invoke('pause_task')`…，见 8.2 与 5.8），操作语义与 RPC 方法一一对应。

### 8.2 主进程事件频道

| 方向 | 频道 | 载荷 | 说明 |
| --- | --- | --- | --- |
| 渲染→主 | `command` + `application:save-preference` | `{user, system}` | 保存配置 + 同步引擎选项 |
| 渲染→主 | `command` + `application:open-file` / `application:reveal-in-folder` / `application:quit` … | 见 `Application.handleCommands` | 全部 40+ 命令逐一映射为 Rust 端分发函数 |
| 渲染→主 | `event` + `speed-change` / `download-status-change` / `progress-change` / `task-download-complete` | 速度/下载中/进度条/完成 | Rust 端更新托盘/能量/最近文档 |
| 主→渲染 | `command`（`webContents.send`） | `application:new-task` / `application:update-theme` / … | Rust 端 `window.emit('command:dispatch', …)` |
| 渲染↔主 | `invoke('get-app-config')` | 合并配置对象 | Rust command `get_app_config` |
| **主→渲染（引擎状态，见 5.8）** | `engine:snapshot` / `engine:task-event` / `engine:global-stat` | 精简任务/统计快照（**无 bitfield 原串**） | 节流推送，替代高频轮询 |
| 渲染→主（任务操作，替代原 JSON-RPC） | `invoke('add_uri')` / `invoke('add_torrent')` / `invoke('pause_task')` / `invoke('resume_task')` / `invoke('remove_task')` / `invoke('change_option')` … | 任务操作参数（对应 §8.1 各 aria2 方法） | Tauri command，capability 权限控制 |
| 渲染→主（详情按需） | `invoke('get_task_detail')` / `invoke('get_peers')` | 降采样 bitfield / 分页 peers | 详情面板打开时调用 |

> 迁移时建议维护一张 `COMMAND_NAMES` 表（Rust 枚举），缺失命令直接报"未实现"，防止静默丢消息。

### 8.3 菜单 / 托盘 / 深链映射

| Electron 版 | Tauri 版 |
| --- | --- |
| `MenuManager`（menus/*.json） | `tauri::menu` 按平台构建；菜单项 id 保持 `app.check-for-updates` 等原命名 |
| `TrayManager` + `DynamicTray.vue`(Canvas) | `tauri::tray::TrayIcon`；动态图像由渲染进程绘制后 command 上传 |
| `ProtocolManager`（mo:/motrix:/magnet:/thunder:） | `tauri-plugin-deep-link`（Windows 注册表 / macOS Info.plist / Linux .desktop） |
| `AutoLaunchManager` | `tauri-plugin-autostart` |
| `UpdateManager`（electron-updater） | `tauri-plugin-updater`（需 GitHub Releases 元数据） |
| `UPnPManager`（@motrix/nat-api） | `igd` crate |
| `EnergyManager`（powerSaveBlocker） | Tauri `power` 插件 / 平台 API |
| 单实例锁 | `tauri-plugin-single-instance` |
| 窗口状态保存（window-state） | 自实现（Rust 持久化 bounds）或 `tauri-plugin-window-state` |

---

## 9. CLI 命令行工具设计

### 9.1 目标与使用场景

- **脚本 / CI / 自动化**：无需打开 GUI 即可添加、暂停、恢复、查询下载任务；
- **与深链协议互补**：深链（`mo:` / `magnet:`）面向浏览器与系统集成，CLI 面向终端与脚本；
- **复用 aria2 兼容 JSON-RPC 适配层**：CLI 只是 JSON-RPC 的**另一个客户端**，后端协议零新增；
- **无头模式**：`motrix-cli daemon` 可直接驱动引擎（无 GUI），供服务器 / CI 场景独立运行。

### 9.2 架构与运行模式

```
┌──────────────────────────────────────────────────────────┐
│ motrix-cli（独立二进制，clap 解析参数）                     │
│  ├─ 远程模式（默认）                                      │
│  │    └─ motrix-rpc 客户端 → JSON-RPC → 运行中的 Motrix   │
│  │        后端（GUI 或 daemon，127.0.0.1:16800）          │
│  └─ daemon 模式                                          │
│       └─ 直接驱动 motrix-core 引擎 + 启动 JSON-RPC 服务    │
│          （等同"无窗口的 Motrix"，不依赖 GUI 进程）         │
└──────────────────────────────────────────────────────────┘
```

- **远程模式（默认）**：连接运行中的 Motrix 后端，调用 `aria2.*` 方法（与外部 aria2 兼容客户端同一契约，见 5.6）；
- **daemon 模式**：`motrix-cli daemon` 在后台拉起 `motrix-core` 引擎与 `motrix-rpc` 服务，供远程模式 / 外部 RPC 客户端连接；GUI（`src-tauri`）与 daemon（`motrix-cli`）**直接依赖同一个 `motrix-core` crate**，引擎代码天然共享、无行为分叉（见 4.1）。

### 9.3 命令一览（与底层 RPC 的映射）

| CLI 命令 | 说明 | 底层 RPC / 模块 |
| --- | --- | --- |
| `add <url>…` | 添加 HTTP/FTP 下载（多 URL=多源镜像） | `aria2.addUri`（multicall） |
| `add-torrent <file.torrent\|magnet:…>` | 添加种子 / 磁力任务 | `aria2.addTorrent` |
| `list [active\|waiting\|stopped]` | 列出任务（默认 active） | `aria2.tellActive/tellWaiting/tellStopped` |
| `status <gid>` | 查看单任务进度 / 详情 | `aria2.tellStatus` |
| `pause <gid>…` / `pause-all` | 暂停 | `aria2.pause` / `aria2.pauseAll` |
| `resume <gid>…` / `resume-all` | 恢复 | `aria2.unpause` / `aria2.unpauseAll` |
| `remove <gid>…` `[-f] [--delete-files]` | 删除任务（可选强制 / 连带删除文件） | `aria2.remove` / `aria2.forceRemove` + 文件回收 |
| `purge` | 清除已完成 / 错误记录 | `aria2.purgeDownloadResult` |
| `config get/set <key> <value>` | 查看 / 修改全局选项（systemKeys → 引擎；userKeys → 用户配置） | `aria2.getGlobalOption/changeGlobalOption` + `motrix.saveUserConfig`（扩展方法，见 9.4） |
| `engine status` | 引擎版本 / 全局统计 | `aria2.getVersion` + `aria2.getGlobalStat` |
| `daemon` | 无头模式启动引擎 + RPC 服务 | motrix-core + motrix-rpc |
| 全局参数 | `--rpc-url`、`--rpc-secret`、`--json`、`--quiet` | — |

**使用示例**：

```
# 添加下载（16 连接，指定目录与限速）
motrix-cli add "https://example.com/file.zip" -d ~/Downloads -x 16 -l 2M

# 添加磁力 / 种子
motrix-cli add-torrent "magnet:?xt=urn:btih:..."
motrix-cli add-torrent ./ubuntu.torrent

# 查询（机器可读输出）
motrix-cli list active --json
motrix-cli status <gid> --json

# 任务控制
motrix-cli pause <gid>
motrix-cli resume <gid>
motrix-cli remove <gid> --delete-files

# 配置与引擎
motrix-cli config set max-concurrent-downloads 10
motrix-cli engine status

# 无头模式（服务器 / CI）
motrix-cli daemon
```

### 9.4 认证与配置

- **认证**：`rpc-secret` 解析顺序 `--rpc-secret` 参数 → 环境变量 `MOTRIX_RPC_SECRET` → `system.json` 中的 `rpc-secret`；`--rpc-url` 默认 `127.0.0.1:16800/jsonrpc`（WS 优先，HTTP POST 兜底，与共享客户端 `JSONRPCClient` 行为一致）；
- **user 配置写入**：`config set` 对 `systemKeys` 走 `aria2.changeGlobalOption`；对 `userKeys` 走**非标准扩展方法** `motrix.saveUserConfig`（仅 CLI / 内部使用，不破坏 aria2 兼容性，方法与 `system.listMethods` 中可见的 `motrix.*` 前缀区分）。

### 9.5 输出格式

- 默认：人类可读文本（进度条 / 表格，复用 `indicatif`）；
- `--json`：结构化输出（`gid`/`status`/`percent`/`downloadSpeed`/`completedLength`/`totalLength`/…，字段与 `tellStatus` 一致），供脚本解析。

### 9.6 实现要点

- `clap`（derive）解析参数；`motrix-rpc` 同时提供**服务端**（axum WS+HTTP）与**客户端**（轻量 JSON-RPC over HTTP POST，约百余行），CLI 直接复用客户端；
- daemon 模式与 GUI 共用 `motrix-core`（引擎 / 任务模型 / 会话），**同一份引擎代码**，避免行为分叉；
- CLI 采用**拉取式**（`tellStatus` 轮询/单次查询），不进 5.8 的事件推送通道（事件通道是 GUI 渲染专用）；
- 构建：`cargo build -p motrix-cli` 产出独立 `motrix-cli` 可执行文件，可随安装包分发或在 PATH 中使用。

---

## 10. 分阶段实施计划

### Phase 0：脚手架与可行性验证（最小可运行）

- 新建 `src-tauri`，跑通 Tauri dev（Vue 前端原样加载）；
- shim 三件套（is / ipcRenderer / remote）就位，窗口与路由正常；
- Rust 实现 `get_app_config`，前端 `fetchPreference` 打通。

**验收**：Tauri 壳下 UI 完整显示、主题/语言切换生效。

### Phase 1：JSON-RPC 服务（对外）+ Tauri 通道 + 状态占位

- 实现 `motrix-rpc/server.rs`（axum，WS+HTTP）、`system.multicall`、`getVersion`/`getGlobalStat`；
- 空实现 `tellActive/tellWaiting/tellStopped`（返回空列表），前端列表页正常渲染空态；
- **打通 Tauri 通道**：任务操作 command 占位（`invoke('add_uri')` 等先返回占位结果）+ `shims/events.js` 订阅 `engine:snapshot` → Vuex，验证"操作走 command、状态走事件"替代 JSON-RPC 轮询。

**验收**：RPC 服务可被外部客户端（`motrix-rpc` 客户端 / curl）调用 `getVersion`/`getGlobalStat`；前端经 Tauri command + 事件在 dev 下可操作占位任务并更新 Vuex。真实任务操作（`add_uri` 等接入引擎）在 Phase 2 进行。

### Phase 2：HTTP/FTP 下载引擎（核心，KGet 集成）

- 任务模型 + 状态机（胶水层）；KGet `AdvancedDownloader` 集成与 aria2 选项映射；
- **StateBroadcaster**：Rust 端节流合并状态 → `engine:snapshot` 事件推送，前端增量 mutation（不再轮询）；
- 进度 `Receiver` → 精简快照字段；通知推送（`engine:task-event`，对应 onDownloadStart/onDownloadComplete/onDownloadError）；
- 限速、代理、`changeOption`/`changeGlobalOption`；
- 会话导入导出（download.session）与 checkpoint。

**验收**：新增 HTTP 任务可下载/暂停/恢复/删除，进度条与速度计正常；断点续传有效；**事件推送流畅、无轮询**。

### Phase 3：BT 支持（librqbit 集成）

- 集成 **librqbit**：magnet + .torrent、metadata 获取、文件选择（`select-file`）、`getPeers`、seeding 控制、`bt-tracker` 同步；
- **bitfield 降采样**：Rust 端按目标宽度降采样后经 `invoke('get_task_detail')` 下发；peers 分页（`invoke('get_peers')`）；
- 参考 rqbit 仓库自带的 Tauri 桌面示例代码接入方式。

**验收**：磁力链与种子任务全流程可用，Peers/Trackers 面板正常。

### Phase 4：平台能力补齐

- 托盘（含速度计）、菜单、深链、开机自启、UPnP、更新、单实例、窗口状态、最近文档、任务完成系统通知。

**验收**：对照 `Application.js` 的 `handleCommands` / `handleEvents` 清单逐一勾选。

### Phase 5：CLI 命令行工具

- 新建 `crates/motrix-cli`（clap）；`motrix-rpc` 补充轻量 JSON-RPC 客户端；
- 远程模式：`add / list / status / pause / resume / remove / purge / config / engine` 全命令连通运行中的后端；
- `motrix-cli daemon` 无头模式：直接复用 `motrix-core` 引擎 + 启动 RPC 服务（与 GUI 共享同一 crate，见 4.1）；
- 扩展方法 `motrix.saveUserConfig`（userKeys 配置写入）。

**验收**：脚本可完成"添加→查询→暂停→恢复→删除"全流程；daemon 模式下 `motrix-cli add` 与 RPC 外部客户端均可操作；`--json` 输出字段与 `tellStatus` 契约一致。

### Phase 6：优化与收尾

- 吞吐/内存/CPU 基准对比 Electron 版（大文件 10GB+、多任务并发）、协议支持矩阵差异项补齐；
- 配置/会话迁移工具 + 一键迁移入口；
- 打包（`tauri build`）三平台产物（含 `motrix-cli` 随包分发），替换 electron-builder 脚本。

**验收**：与 Electron 版功能对等，性能指标达成（见 5.7）。

---

## 11. 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| **BT 大 bitfield / peers 载荷导致渲染白屏**（Electron 版已现） | 长时下载 UI 不可用 | **Tauri 事件推送 + 降采样 bitfield + 分页 peers**（见 5.8），Phase 2/3 验收含 ≥4h 长时压测 |
| **KGet 为个人维护项目**（v1.7、MIT） | 长期维护不确定性 | 锁版本 + 关键功能 POC 先行；`cargo vendor`/vendored 依赖固化；API 变动影响有限（仅 kget.rs 一个文件） |
| **aria2-rust 虽功能最贴合但 GPL-2.0 + 未成熟** | 许可传染 / 不稳定 | 已否决；跟踪其进展，若未来改许可且成熟度提升可作为备选替换方案 |
| librqbit 能力缺口（如 MSE/PE 加密、某些 DHT 场景） | BT 兼容性下降 | 能力矩阵逐项 POC 验证；缺口项评估 librqbit 上游或社区方案，必要时提交上游 issue |
| KGet/librqbit 不支持的能力（如 aria2 `bt-metadata-only` 等冷门选项） | 个别设置无效 | 迁移前建立"选项→能力"映射表，明确不支持项在 UI 中禁用或提示 |
| aria2 RPC 返回字段细节差异 | 外部客户端 / 事件快照字段不一致 | 建立 **契约测试**：录制 Electron 版真实返回 JSON，`motrix-rpc` 与事件快照逐字段对拍 |
| `*.aria2` 控制文件不解析 | 旧任务无法续传 | 迁移期从 `download.session` 重建任务并提示重下；新任务用自有 checkpoint |
| 系统 WebView 差异（Windows WebView2 版本） | 偶发渲染/通知差异 | 指定最低 WebView2 运行时；Tauri 2 自动注入 polyfill |
| 深链/单实例在不同平台细节多 | 调试成本高 | 先用 tauri 官方插件，回归测试三平台 |
| 托盘动态图标（Canvas→Rust）跨进程开销 | 托盘卡顿 | 降低上传频率（如 1s），或 Rust 端直接绘制进度 |
| 前端 shim 遗漏 electron 调用 | 运行时白屏 | 全局搜索 `electron`/`$electron`/`@electron/remote` 穷举替换（见附录 A） |

---

## 12. 验证与测试策略

1. **契约测试**：Rust 侧为每个 RPC 方法写 Golden-Data 测试（与 Electron 版真实返回对拍）；
2. **下载集成测试**：本地 HTTP 服务器 + 大/小/断点/限速/代理场景自动化（`tokio::test`）；
3. **UI 回归**：录制 Electron 版截图，Tauri 版同场景截图 diff；
4. **性能基准**：同一网络/文件下对比双版本吞吐、内存峰值、启动时间、包体积；
5. **数据迁移演练**：拷贝旧 `userData` 目录 → 新版本启动 → 检查配置与会话恢复。

---

## 13. 附录 A：electron 依赖穷举清单（前端需替换点）

| 文件 | 替换内容 |
| --- | --- |
| `src/renderer/api/Api.js` | `ipcRenderer.invoke('get-app-config')` → shim invoke；`fetchTaskList/fetchTaskItem/getGlobalStat` 轮询方法 → `engine:*` 事件数据；`addUri/pause/resume/remove/changeOption` 等操作方法 → Tauri command（`invoke`）；`fetchTaskItem/getPeers` → on-demand invoke（详见 5.8 / 8.2） |
| `src/renderer/utils/native.js` | `@electron/remote` shell/nativeTheme → shim |
| `src/renderer/pages/index/main.js` | `vue-electron` / `is.renderer()` → shim |
| `src/renderer/pages/index/App.vue` | `is.macOS()/is.renderer()` → shim |
| `src/renderer/components/Native/TitleBar.vue` | `$electron.ipcRenderer` → shim |
| `src/renderer/components/Native/Ipc.vue` | `$electron.ipcRenderer.on('command')` → shim |
| `src/renderer/components/Native/EngineClient.vue` | `$electron.ipcRenderer.send('event', …)` → shim；**轮询逻辑 → 订阅 `engine:snapshot` 事件 + on-demand invoke（见 5.8）** |
| `src/renderer/components/Native/DynamicTray.vue` | `ipcRenderer.send('command', 'application:update-tray', ab)` → shim |
| `src/renderer/components/Native/SelectDirectory.vue` | 目录选择对话框 → `tauri-plugin-dialog` |
| `src/renderer/components/Native/ShowInFolder.vue` | `showItemInFolder` → shim |
| `src/renderer/components/About/*` | 版本信息来自 `get-app-config`（Rust 注入 version） |
| 新增 `src/shims/events.js` | 封装 `listen('engine:*')` → Vuex 增量 mutation（diff 变更任务） |

---

## 14. 附录 B：README / 文档更新项

- `README-CN.md` / `README.md`：技术栈与架构描述改为 Tauri + Rust；
- `electron-builder.json` → 删除，替换 `tauri.conf.json`（含 `bundle`、`icons`、`plugins`）；
- CI（`.github/workflows/release.yml`）改用 `tauri-action`；
- `app-update.yml` → 改用 `tauri-plugin-updater` 的 `latest.json` 发布格式。

---

*文档版本：v1.6（2026-08-09）*
*v1.5：通道职责收敛——前端 UI 完全走 Tauri（操作 → command，状态 → `engine:*` 事件），JSON-RPC 明确定位为对外接口。*
*v1.6：一致性复审——修正 §9.2/Phase 5 误引的 `motrix-app`（实为 `src-tauri` + 共享 `motrix-core`）；统一 crate 路径（§7.1 `config/store.rs`→`motrix-core/config.rs`、§5.1/6.3 示例路径）；§4.3 通信行、§5.6/9.4/11 措辞对齐"前端不走 JSON-RPC"；附录 A 合并 Api.js 重复行。*
*下一步：评审通过后从 Phase 0 启动实施。*
