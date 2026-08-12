# Checklist

## 全局约定
- [x] 所有新增 Rust 代码包含中文注释
- [x] 前端视觉组件（TaskItem / TaskGraphic / TaskPeers / store / router / i18n）未做视觉改动，仅桥接/数据管线被替换
- [x] 未造轮子：BT 用 librqbit、平台能力用 tauri 官方插件、UPnP 用 igd

## Phase 3：BT 支持
- [x] `crates/motrix-core/src/bt.rs` 封装 librqbit session/torrent handle，`cargo check` 通过，Windows 编译无新增阻塞性前置
- [x] `add_torrent` 真实实现：magnet 创建 metadata 任务（`bittorrent.info` 为空），metadata 获取后转完整 BT 任务且 gid 稳定；`.torrent` base64 可添加
- [x] BT 进度/速度/做种数经 `engine:snapshot` 推送；完成进入 Seeding（`keep-seeding`）或 Complete；做种结束发 `bt-complete`
- [x] BT 暂停 = 停止+保存状态，恢复 = 重加入续传（已确认语义）
- [x] `get_task_detail` 返回 bitfield 字符串长度 ≤ 240（每字符 0~3 状态），降采样 roundtrip 单测通过；`engine:snapshot` 不含 bitfield 原串
- [x] `get_peers` 返回 ≤ 100 条 aria2 兼容 peer 字段（ip/port/peerId/bitfield/uploadSpeed/downloadSpeed）
- [x] `select-file` 文件选择、`keep-seeding`/`seed-ratio`/`seed-time` 做种控制生效（含运行中 change_option 即时下发 + seed 上限结束做种发 bt-complete，7 个新增测试覆盖）
- [x] `bt-tracker` 同步与 BT 任务 checkpoint 恢复（P1）生效
- [x] BT 契约测试通过（Golden-Data 对拍 + `to_aria2` BT 分支）
- [x] `cargo test --workspace` 全绿（含新增 BT 用例，Phase 2 用例无回归）

## Phase 4：平台能力补齐
- [x] 托盘图标显示，菜单项按原 `tray.json`（id 保持原命名），点击显隐主窗口
- [x] 动态速度计：`application:update-tray` 上传 Canvas 图像 → 托盘图标更新，上传频率 ≤ 1s
- [x] 应用菜单按平台构建（win32 五组），id 保持原命名，点击触发命令分发
- [x] `COMMAND_NAMES` 未实现命令补齐（退出/显隐/重置会话/恢复出厂/主题/语言/外链/检查更新等），无"已列入但未实现"日志
- [x] 4 类事件驱动托盘/防休眠/窗口进度/最近文档；8 类配置变化即时联动（`proxy` 联动引擎选项）
- [x] 单实例：二次启动聚焦已有窗口
- [x] 开机自启：`open-at-login` 联动 `tauri-plugin-autostart`，配置变化即时生效
- [x] 深链 `mo:`/`motrix:`/`magnet:` 注册（打包产物中），dev 模式跳过注册；magnet 深链可添加任务
- [x] 任务完成系统通知（`task-notification` 控制）
- [x] 窗口状态保存（`keep-window-state`）重启恢复；最近文档添加
- [x] UPnP 映射、自动更新命令链路（无元数据优雅降级）、电源防休眠（P1）就绪
- [x] `cargo check`（含 tauri 插件）通过

## Phase 5：CLI
- [x] `crates/motrix-cli` 加入 workspace，`cargo build -p motrix-cli` 产出独立二进制，`--help` 完整
- [x] `motrix-rpc/src/client.rs` 轻量 JSON-RPC 客户端（HTTP POST + token 认证 + 错误映射）
- [x] 全局参数 `--rpc-url` / `--rpc-secret` / `--json` / `--quiet`；secret 解析顺序：参数 → 环境变量 → system.json
- [x] 远程模式全命令可用：add / add-torrent / list / status / pause(-all) / resume(-all) / remove / purge / config get/set / engine status
- [x] `motrix.saveUserConfig` 服务端实现，`config get/set` 对 userKeys 全流程可用
- [x] `motrix-cli daemon` 无头模式：复用 motrix-core 引擎 + JSON-RPC 服务；端口占用报错退出（非零退出码）并提示；退出保存 checkpoint
- [x] 端到端：脚本完成"添加→查询→暂停→恢复→删除"全流程；daemon 下 CLI 与 curl 均可操作；`--json` 字段与 `tellStatus` 契约一致
- [x] CLI 集成测试通过（命令执行断言输出与退出码）

## 整体
- [x] `cargo test --workspace` 全绿
- [ ] `tauri dev` 手工验收（需人工在 GUI 环境执行，本环境无法自动化）：BT 任务列表/详情图/Peers/Trackers 面板正常；托盘/菜单/自启/通知可用；CLI 对运行中应用全流程可用。**已完成的可自动化替代验证**：`cargo test --workspace` 全绿（81 core + rpc/cli/tauri）、`cargo build -p motrix-cli` 成功、`npm run build:web` 成功、daemon 手动启动/端口冲突/远程连接实测通过
