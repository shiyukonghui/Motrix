# Checklist

## 全局约定
- [x] 所有新增/修改的 Rust 代码包含中文注释
- [x] 前端视觉组件与 shims 调用签名（`remote-shell.js` invoke 名）未改动
- [x] README 构建前置说明记录了 Windows 下 perl（vendored OpenSSL）要求

## 构建环境（Task 1）
- [x] `cargo check` 全 workspace 通过
- [x] `cargo test --workspace` 全绿（含既有 75 用例基线，无回归）

## HTTPS/HTTP 探测与完成回填（Task 2）
- [x] `probe_content_length` 基于 reqwest 支持 http/https；HEAD → Content-Length、Range GET 兜底；失败返回 Ok(0) 不阻塞
- [x] 探测请求显式发送 `Accept-Encoding: identity`，避免服务器返回压缩后大小（实测 jsdelivr/Cloudflare 的 Content-Range 是压缩缓存大小，需以真实大小为准）
- [x] 本地 HTTP 服务器验证探测结果正确（HEAD 成功 / Range 兜底 / 404 → 0）
- [x] `handle_finished` 在 total==0 时读取文件大小回填 total/completed（单文件同步 files[0]）
- [x] https URL 添加任务：totalLength 为真实总长；完成时 completedLength == totalLength（进度 100%）
- [x] 新增探测/回填用例全部通过

## 原生 shell 能力（Task 3）
- [x] `show_item_in_folder` / `open_path` / `trash_item` 三个 command 已实现并注册到 invoke_handler
- [x] `src-tauri/Cargo.toml` 声明 `trash`；`cargo check -p motrix-tauri` 通过
- [x] 平台命令构造有单元测试覆盖（Windows/macOS/Linux 分支）
- [x] 前端 `remote-shell.js` 调用不再因"command not found"失败（成功路径返回 OK）

## 下载速度统计（Task 4）
- [x] 下载中任务 `downloadSpeed` > 0（engine 集成测试断言）
- [x] 完成/暂停/失败后速度归零，快照与 global-stat 一致

## 收尾（Task 5）
- [x] `cargo test --workspace` 全绿（含新增用例，共 80 用例）
- [x] `npm run build:web` 成功
- [x] 已更新 `migrate-to-tauri-phase2/checklist.md` 受影响验收项
