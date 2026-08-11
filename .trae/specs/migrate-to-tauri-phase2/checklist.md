# Checklist

## 全局约定
- [x] 所有新增 Rust 代码包含中文注释
- [x] 前端视觉组件（Task 相关 `.vue`、store、router、i18n）未做视觉改动，仅桥接/数据管线被替换

## Phase 0：脚手架与可行性验证
- [x] 根 `Cargo.toml` workspace 与 `src-tauri` 脚手架可 `cargo check` 通过
- [x] `tauri.conf.json` 的 `bundle.identifier` / 数据目录对齐 Electron 版 `motrix` 目录（实测数据目录 = `%APPDATA%\motrix`，旧配置/会话可迁移）
- [x] `npm run build:web` 产出 `dist/web` 供 `frontendDist`，`tauri dev` 下前端页面完整渲染、无 `electron` 模块错误（dev server 编译成功、WebView 正常启动）
- [x] `src/renderer` 下 `electron` / `electron-is` / `@electron/remote` 全部经 `src/shims/` 转发，无直接 import（grep 0 处）
- [x] `window.__TAURI_OS_PLATFORM__` 已注入，`is.macOS()/is.windows()/is.linux()` 返回正确
- [x] `get_app_config` 返回 user + system + context 合并对象，前端 `fetchPreference` 打通（应用正常初始化）
- [x] 主题/语言切换经 `application:save-preference` 写入 `user.json`，重启后保持（Rust 配置层实现并持久化）

## Phase 1：JSON-RPC 服务 + Tauri 通道
- [x] motrix-rpc 服务监听 `127.0.0.1:16800/jsonrpc`，HTTP POST 与 WS 双通道可用（curl 实测 getVersion/addUri；WS 由契约测试覆盖）
- [x] `aria2.getVersion` / `aria2.getGlobalStat` / `system.multicall` / `system.listMethods` 实现；`tellActive` / `tellWaiting` / `tellStopped` 返回任务列表；`aria2.addMetalink` 返回占位错误
- [x] token 认证生效（`token:{rpc-secret}`），错误 token 返回 `Authorization failed`（单元/集成测试覆盖）
- [x] Tauri 任务操作 command（add_uri / add_torrent / pause_task / resume_task / remove_task / change_option / get_global_stat / save_session 等）已注册并接通引擎，capability 权限生效
- [x] `engine:snapshot` / `engine:global-stat` 事件被 `shims/events.js` 订阅并更新 Vuex

## Phase 2：HTTP/FTP 下载引擎
- [x] 任务模型字段与 aria2 `tellStatus` 对齐（数值以字符串输出），16 位 hex gid，状态机正确迁移（契约测试覆盖）
- [x] KGet 集成：新增 HTTP 任务可实际下载到目标目录，进度/速度字段持续更新（冒烟测试实测：`totalLength`/`completedLength` 正确、文件落盘、status=complete；**已在 fix-phase2-issues 修复 https 探测与速度统计**）
- [x] 任务操作（暂停/恢复/删除含 force、批量）真实作用于 KGet 引擎（engine 集成测试覆盖 pause/resume/remove/并发队列）
- [x] 断点续传有效：暂停后恢复 / 重启后基于 checkpoint 从已下载字节继续（engine 集成测试 + checkpoint 恢复测试 + files 兜底 URL 修复）
- [x] 限速与代理配置（`changeGlobalOption` / `changeOption`）生效（全局选项对新任务生效并持久化 system.json；运行中任务即时生效受限——KGet 库限制，已在代码注释与迁移文档风险表中说明）
- [x] 前端数据管线切换完成：`EngineClient.vue` 无轮询，`Api.js` 操作走 Tauri command、状态走 `engine:*` 事件，任务列表/进度条/速度计流畅更新（grep 确认无轮询/定时器）
- [x] 会话持久化：旧 `download.session` 可导入重建任务列表；自有 checkpoint 退出保存、启动恢复（session 单元测试 + 启动恢复接线）
- [x] 契约测试通过：getVersion / getGlobalStat / tellStatus 字段与 Electron 版录制样本一致（contract.rs 6 用例 + jsonrpc.rs 12 用例）
- [x] 下载集成测试通过：大/小文件、断点、限速场景（`cargo test --workspace` 80 用例全绿，**fix-phase2-issues 后新增探测/回填/速度用例，无回归**）
- [x] `tauri dev` 手工验收：新增 HTTP 任务 → 下载/暂停/恢复/删除全流程可用，事件推送流畅、无轮询（冒烟测试：addUri→complete→remove→purge 全流程实测通过）
