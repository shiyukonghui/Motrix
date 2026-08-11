# Tasks

## 前置：构建环境准备（解锁全部后续任务的编译/测试）

- [x] Task 1：构建环境准备与验证
  - [x] 1.1 确认阻塞根因：KGet → `ssh2`（默认特性 `vendored-openssl`）→ `libssh2-sys` → `openssl-sys`，vendored 构建 openssl 需 perl；在 README 构建前置说明（或 MIGRATION-TAURI.md 附录）记录 Windows 需安装 Strawberry Perl（或等价提供 vendored OpenSSL 构建能力）
  - [x] 1.2 安装/确认 perl 可用后执行 `cargo check`（全 workspace）通过
  - [x] 1.3 执行 `cargo test --workspace` 记录基线用例数（预期 ≥ 75 全绿）

## 修复实现（Task 2/3/4 相互独立，均依赖 Task 1 的可编译环境）

- [x] Task 2：HTTPS/HTTP 总长探测与完成回填（P1）
  - [x] 2.1 `crates/motrix-core/Cargo.toml` 声明 `reqwest`（默认特性，复用 KGet 已编译依赖，不新增编译成本）
  - [x] 2.2 重写 `kget.rs::probe_content_length`：reqwest `HEAD` → Content-Length；HEAD 不可用（非 200/206 或无 Content-Length）时 `Range: bytes=0-0` GET 从 Content-Range 取总长；UA/代理读 `EngineOptions`；超时 5s；失败返回 `Ok(0)`；更新模块文档与映射表注释（含 https 支持说明）
  - [x] 2.3 `engine.rs::handle_finished` 兜底：`total_length == 0` 时 `std::fs::metadata` 读输出文件大小，回填 `total_length` / `completed_length`（单文件任务同步 `files[0]`）
  - [x] 2.4 测试：
    - [x] 单元/集成：本地 HTTP 服务器验证 reqwest 探测结果不变（HEAD 成功 / 仅 Range 兜底 / 404 → 0）；纯函数测试保持不变
    - [x] 完成回填单测：构造 `total_length=0` 的任务 + 临时文件，断言完成回调后 total/completed = 文件大小
    - [x] 新增 https 探测用例：探测函数对 https URL 返回成功或至少不阻塞（`Ok` 或 `Ok(0)`），不再因协议被拒

- [x] Task 3：原生 shell 能力 command（P2）
  - [x] 3.1 `src-tauri/Cargo.toml` 声明 `trash` crate（平台回收站删除）
  - [x] 3.2 `commands.rs` 实现 `show_item_in_folder(path)`：Windows `explorer /select,<path>`、macOS `open -R <path>`、Linux 打开父目录（`xdg-open <parent>`）；std::process::Command，失败返回 Err 带信息
  - [x] 3.3 实现 `open_path(path)`：Windows `explorer <path>`、macOS `open`、Linux `xdg-open`（std::process::Command）
  - [x] 3.4 实现 `trash_item(path)`：`trash::delete`，失败返回 Err
  - [x] 3.5 `lib.rs` invoke_handler 注册三个 command；确认 capability 无需新增（自定义 command 默认可用）
  - [x] 3.6 测试：平台命令构造的纯函数单测（Windows 分支断言命令串）；`trash` 删除临时文件往返；`cargo check -p motrix-tauri` 通过

- [x] Task 4：下载速度统计（P3）
  - [x] 4.1 `engine.rs`：进度回调维护 `(completed, Instant)` 快照（Map<gid, 快照>），`download_speed = Δcompleted / Δt`（Δt 过小时沿用上一值；total 未知时不改速度），`update_progress` 写入 `download_speed`
  - [x] 4.2 完成/暂停/失败时清除该 gid 快照并归零速度（与现状一致）
  - [x] 4.3 集成测试：本地慢速 HTTP 服务器下载中轮询 `download_speed > 0`（复用现有 TestHttpServer）

## 收尾

- [x] Task 5：全量验证与回归
  - [x] 5.1 `cargo test --workspace` 全绿（含新增用例），契约测试（contract.rs / jsonrpc.rs）无回归
  - [x] 5.2 `npm run build:web` 成功（前端无改动，仅回归确认）
  - [x] 5.3 对照 `fix-phase2-issues/checklist.md` 逐项勾选；更新 `migrate-to-tauri-phase2/checklist.md` 中受影响验收项（速度计/进度条）标注已修复

# Task Dependencies

- [Task 1] 无依赖（环境准备，先行解锁编译/测试）
- [Task 2] 依赖 [Task 1]
- [Task 3] 依赖 [Task 1]
- [Task 4] 依赖 [Task 1]、[Task 2]（速度计算依赖 total 正确换算字节）
- [Task 5] 依赖 [Task 2]、[Task 3]、[Task 4]

**可并行项**：Task 2 与 Task 3 可并行；Task 1 完成即可启动 Task 2/3。
