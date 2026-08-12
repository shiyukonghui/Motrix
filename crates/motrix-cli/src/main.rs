//! motrix-cli：Motrix 命令行工具（Phase 5：CLI 骨架 + 远程模式命令 + daemon 无头模式）
//!
//! 架构（见 MIGRATION-TAURI.md 第 9 章）：
//! - CLI 是 JSON-RPC 的**另一个客户端**：除 `daemon` 外，所有命令均通过
//!   [motrix_rpc::client] 的轻量客户端调用运行中的后端（GUI 或 daemon），
//!   后端协议零新增；
//! - `daemon`（无头模式，Phase 5 Task 12）：复用 motrix-core 引擎（ConfigManager /
//!   TaskManager / session / rpc_backend）启动本进程内的 JSON-RPC 服务，
//!   与 GUI 共享同一份引擎代码（无行为分叉，见 MIGRATION-TAURI.md 4.1）；
//! - 认证：`rpc-secret` 解析顺序 `--rpc-secret` 参数 → 环境变量
//!   `MOTRIX_RPC_SECRET` → `system.json` 中的 `rpc-secret`；
//! - 输出：`--json` 直接透传 RPC result（与 tellStatus 契约一致，不做二次组装）；
//!   默认人类可读文本；`--quiet` 抑制成功输出；错误打印到 stderr 且退出码非零。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use motrix_core::{ConfigManager, TaskManager};
use motrix_rpc::client::{ClientError, JsonRpcClient};
use motrix_rpc::{JsonRpcServer, RpcBackend};
use serde_json::{json, Map, Value};

/// Motrix 下载引擎命令行工具（JSON-RPC 远程控制）
///
/// 使用示例：
///   motrix-cli add "https://example.com/file.zip" -x 16 -l 2M
///   motrix-cli add-torrent ./ubuntu.torrent
///   motrix-cli list active --json
///   motrix-cli status <gid>
///   motrix-cli engine status
#[derive(Parser)]
#[command(
    name = "motrix-cli",
    version,
    about = "Motrix 下载引擎命令行工具（JSON-RPC 远程控制）",
    disable_help_subcommand = true
)]
struct Cli {
    /// JSON-RPC 服务地址（默认连接本机 Motrix 后端）
    #[arg(long, global = true, default_value = "http://127.0.0.1:16800/jsonrpc")]
    rpc_url: String,

    /// RPC 密钥（优先级：本参数 > 环境变量 MOTRIX_RPC_SECRET > system.json 的 rpc-secret）
    #[arg(long, global = true)]
    rpc_secret: Option<String>,

    /// 结构化输出：直接透传 RPC result（JSON，供脚本解析）
    #[arg(long, global = true)]
    json: bool,

    /// 抑制成功时的正常输出（错误信息仍打印到 stderr）
    #[arg(long, global = true)]
    quiet: bool,

    /// 子命令
    #[command(subcommand)]
    command: Command,
}

/// 子命令集合（全部映射到 aria2.* 方法，见 MIGRATION-TAURI.md 9.3 表格）
#[derive(Subcommand)]
enum Command {
    /// 添加 HTTP/FTP 下载任务（多 URL = 多源镜像，aria2.addUri）
    Add {
        /// 下载 URL 列表
        #[arg(required = true)]
        urls: Vec<String>,
        /// 下载目录（任务级 options.dir）
        #[arg(short, long)]
        dir: Option<String>,
        /// 每服务器连接数（max-connection-per-server）
        #[arg(short, long)]
        connections: Option<u32>,
        /// 下载速度上限（max-download-limit，如 "2M"）
        #[arg(short, long)]
        limit: Option<String>,
    },
    /// 添加种子 / 磁力任务（aria2.addTorrent）
    ///
    /// 入参支持三种形式：`magnet:` 磁力链接（原样传递）、`.torrent` 本地文件路径
    /// （读取后转 base64）、其余视为 base64 编码的种子内容。
    AddTorrent {
        /// .torrent 文件路径、base64 内容或 magnet 链接
        torrent: String,
    },
    /// 列出任务（aria2.tellActive / tellWaiting / tellStopped）
    List {
        /// 任务类型：active / waiting / stopped（默认 active）
        #[arg(default_value = "active")]
        kind: String,
    },
    /// 查看单个任务详情（aria2.tellStatus）
    Status {
        /// 任务 gid
        gid: String,
    },
    /// 暂停任务（aria2.pause）
    Pause {
        /// 任务 gid 列表
        #[arg(required = true)]
        gids: Vec<String>,
    },
    /// 暂停全部任务（aria2.pauseAll）
    PauseAll,
    /// 恢复任务（aria2.unpause）
    Resume {
        /// 任务 gid 列表
        #[arg(required = true)]
        gids: Vec<String>,
    },
    /// 恢复全部任务（aria2.unpauseAll）
    ResumeAll,
    /// 删除任务（aria2.remove / forceRemove）
    Remove {
        /// 任务 gid 列表
        #[arg(required = true)]
        gids: Vec<String>,
        /// 强制删除（走 forceRemove，跳过暂停等待）
        #[arg(short, long)]
        force: bool,
        /// 连带删除文件（本阶段仅作请求标记，后端删除能力由后续阶段实现）
        #[arg(long)]
        delete_files: bool,
    },
    /// 清除已完成 / 出错的任务记录（aria2.purgeDownloadResult）
    Purge,
    /// 查看 / 修改全局配置（aria2.getGlobalOption / changeGlobalOption + motrix.saveUserConfig）
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// 引擎状态（aria2.getVersion + aria2.getGlobalStat）
    Engine {
        #[command(subcommand)]
        action: EngineAction,
    },
    /// 无头模式启动引擎 + RPC 服务（Phase 5 Task 12：复用 motrix-core 引擎 +
    /// motrix-rpc 的 JsonRpcServer，与 GUI 共享同一份实现）
    Daemon,
}

/// config 子命令：get / set
#[derive(Subcommand)]
enum ConfigAction {
    /// 查看全局选项（无 key 输出全部，有 key 输出单值）
    Get {
        /// 选项键名（缺省输出全部，如 max-concurrent-downloads）
        key: Option<String>,
    },
    /// 修改全局选项
    ///
    /// systemKeys 走 aria2.changeGlobalOption（值按字符串传入）；
    /// userKeys 走非标准扩展方法 motrix.saveUserConfig（见 MIGRATION-TAURI.md 9.4）。
    Set {
        /// 选项键名（userKeys / systemKeys 清单见 src/shared/configKeys.js）
        key: String,
        /// 选项值（按字符串传入）
        value: String,
    },
}

/// engine 子命令：status
#[derive(Subcommand)]
enum EngineAction {
    /// 引擎版本与全局统计（getVersion + getGlobalStat，经 system.multicall 一次获取）
    Status,
}

/// userKeys 键名清单（与 src/shared/configKeys.js 一致，只读参考勿修改前端）。
/// config set 对 userKeys 走非标准扩展方法 motrix.saveUserConfig。
const USER_KEYS: &[&str] = &[
    "auto-check-update",
    "auto-hide-window",
    "auto-sync-tracker",
    "cookie",
    "enable-upnp",
    "engine-bin-path",
    "engine-max-connection-per-server",
    "favorite-directories",
    "hide-app-menu",
    "history-directories",
    "keep-seeding",
    "keep-window-state",
    "last-check-update-time",
    "last-sync-tracker-time",
    "locale",
    "log-level",
    "new-task-show-downloading",
    "no-confirm-before-delete-task",
    "open-at-login",
    "protocols",
    "proxy",
    "resume-all-when-app-launched",
    "run-mode",
    "show-progress-bar",
    "task-notification",
    "theme",
    "tracker-source",
    "tray-speedometer",
];

/// systemKeys 键名清单（与 src/shared/configKeys.js 一致，只读参考勿修改前端）。
/// config set 对 systemKeys 走 aria2.changeGlobalOption。
const SYSTEM_KEYS: &[&str] = &[
    "all-proxy-passwd",
    "all-proxy-user",
    "all-proxy",
    "allow-overwrite",
    "allow-piece-length-change",
    "always-resume",
    "async-dns",
    "auto-file-renaming",
    "bt-enable-hook-after-hash-check",
    "bt-enable-lpd",
    "bt-exclude-tracker",
    "bt-external-ip",
    "bt-force-encryption",
    "bt-hash-check-seed",
    "bt-load-saved-metadata",
    "bt-max-peers",
    "bt-metadata-only",
    "bt-min-crypto-level",
    "bt-prioritize-piece",
    "bt-remove-unselected-file",
    "bt-request-peer-speed-limit",
    "bt-require-crypto",
    "bt-save-metadata",
    "bt-seed-unverified",
    "bt-stop-timeout",
    "bt-tracker-connect-timeout",
    "bt-tracker-interval",
    "bt-tracker-timeout",
    "bt-tracker",
    "check-integrity",
    "checksum",
    "conditional-get",
    "connect-timeout",
    "content-disposition-default-utf8",
    "continue",
    "dht-file-path",
    "dht-file-path6",
    "dht-listen-port",
    "dir",
    "dry-run",
    "enable-http-keep-alive",
    "enable-http-pipelining",
    "enable-mmap",
    "enable-peer-exchange",
    "file-allocation",
    "follow-metalink",
    "follow-torrent",
    "force-save",
    "force-sequential",
    "ftp-passwd",
    "ftp-pasv",
    "ftp-proxy-passwd",
    "ftp-proxy-user",
    "ftp-proxy",
    "ftp-reuse-connection",
    "ftp-type",
    "ftp-user",
    "gid",
    "hash-check-only",
    "header",
    "http-accept-gzip",
    "http-auth-challenge",
    "http-no-cache",
    "http-passwd",
    "http-proxy-passwd",
    "http-proxy-user",
    "http-proxy",
    "http-user",
    "https-proxy-passwd",
    "https-proxy-user",
    "https-proxy",
    "index-out",
    "listen-port",
    "lowest-speed-limit",
    "max-concurrent-downloads",
    "max-connection-per-server",
    "max-download-limit",
    "max-file-not-found",
    "max-mmap-limit",
    "max-overall-download-limit",
    "max-overall-upload-limit",
    "max-resume-failure-tries",
    "max-tries",
    "max-upload-limit",
    "metalink-base-uri",
    "metalink-enable-unique-protocol",
    "metalink-language",
    "metalink-location",
    "metalink-os",
    "metalink-preferred-protocol",
    "metalink-version",
    "min-split-size",
    "no-file-allocation-limit",
    "no-netrc",
    "no-proxy",
    "no-want-digest-header",
    "out",
    "parameterized-uri",
    "pause-metadata",
    "pause",
    "piece-length",
    "proxy-method",
    "realtime-chunk-checksum",
    "referer",
    "remote-time",
    "remove-control-file",
    "retry-wait",
    "reuse-uri",
    "rpc-listen-port",
    "rpc-save-upload-metadata",
    "rpc-secret",
    "seed-ratio",
    "seed-time",
    "select-file",
    "split",
    "ssh-host-key-md",
    "stream-piece-selector",
    "timeout",
    "uri-selector",
    "use-head",
    "user-agent",
];

fn main() -> ExitCode {
    // 参数解析失败（未知子命令 / 缺参数）时 clap 自动输出帮助并以非零码退出
    let cli = Cli::parse();
    // daemon 无头模式：进入 tokio runtime 运行（需异步 RPC 服务 / 信号监听），
    // 与远程模式（同步 JSON-RPC 客户端）分开处理
    if matches!(&cli.command, Command::Daemon) {
        return daemon_main();
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        // 业务 / 连接 / 认证等错误：stderr 中文友好提示 + 退出码 1
        Err(msg) => {
            eprintln!("错误: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// daemon 无头模式入口：创建 tokio runtime 并运行 daemon 主循环（直到 Ctrl+C 退出）
///
/// 结构说明：main 保持同步签名（远程模式为同步 JSON-RPC 调用，无需 runtime），
/// 仅 daemon 进入 tokio runtime（对应 GUI 的 tauri::async_runtime）。
fn daemon_main() -> ExitCode {
    // multi-thread runtime：RPC 服务连接处理与 Ctrl+C 信号监听并行驱动
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("错误: 创建异步运行时失败: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(daemon_run())
}

/// daemon 主循环（async）：初始化引擎 → 端口冲突检测 → 启动 RPC 服务 →
/// Ctrl+C 优雅退出（保存会话 checkpoint）
async fn daemon_run() -> ExitCode {
    // 1. 定位数据目录（与 GUI 共用 motrix-core 的统一实现；定位失败回退当前目录）
    let data_dir = motrix_core::config::data_dir().unwrap_or_else(|e| {
        eprintln!("警告: 无法定位数据目录，回退到当前目录: {e}");
        PathBuf::from(".")
    });

    // 2. 加载配置（首次运行自动创建 user.json / system.json）并初始化任务管理器。
    //    与 GUI（src-tauri/src/lib.rs AppState::new）完全同构：TaskManager 用
    //    system.json 初始化全局选项 / 并发数，注入自引用供引擎线程回调
    let config_manager = Arc::new(Mutex::new(ConfigManager::new(data_dir.clone())));
    let system = match config_manager.lock() {
        Ok(cm) => cm.system_config().clone(),
        Err(e) => {
            eprintln!("错误: 读取系统配置失败: {e}");
            return ExitCode::FAILURE;
        }
    };
    let task_manager = Arc::new(TaskManager::new(&system, config_manager.clone()));
    // 注入自引用（Weak）：引擎线程事件回调需要升级为 Arc 再回调任务管理器
    task_manager.set_self(Arc::downgrade(&task_manager));

    // 3. 启动时恢复会话（checkpoint / Electron 版旧 download.session），
    //    与 GUI 启动恢复流程一致（在 RPC 服务启动之前调用）
    match task_manager.repo.lock() {
        Ok(mut repo) => motrix_core::session::restore_session(&data_dir, &mut repo),
        Err(e) => eprintln!("警告: 会话恢复失败（获取任务仓库锁失败）: {e}"),
    }

    // 4. 读取 RPC 监听端口与密钥（system.json 的 rpc-listen-port / rpc-secret）
    let (port, secret) = match config_manager.lock() {
        Ok(cm) => (cm.rpc_listen_port(), cm.rpc_secret().to_string()),
        Err(e) => {
            eprintln!("错误: 读取 RPC 配置失败: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 5. 端口冲突检测：先尝试绑定 127.0.0.1:{port}，失败说明端口已被占用
    //    （通常是与 GUI 或其他后端同时运行）→ 报错并以非零退出码退出
    //    （已确认策略：daemon 需要独立端口）
    match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(probe) => drop(probe), // 探测用 listener 随即释放，随后 serve() 重新绑定
        Err(e) => {
            eprintln!(
                "错误: 端口 {port} 已被占用，请先关闭 Motrix GUI 或其他后端（daemon 模式需要独立端口）: {e}"
            );
            return ExitCode::FAILURE;
        }
    }

    // 6. 构造 aria2 兼容 RPC 后端（与 GUI 共享同一份 motrix-core 引擎实现，
    //    无行为分叉）并组装 JSON-RPC 服务
    let backend: Arc<dyn RpcBackend> = Arc::new(motrix_core::CoreRpcBackend::new(
        task_manager.clone(),
        config_manager.clone(),
    ));
    let server = JsonRpcServer::new(backend, port, secret);
    println!("daemon 已启动，监听 127.0.0.1:{port}/jsonrpc");

    // 7. 持续运行 RPC 服务，同时监听 Ctrl+C 实现优雅退出
    tokio::select! {
        // RPC 服务（WS + HTTP POST 双通道）持续运行；异常退出则报错返回
        serve_result = server.serve() => match serve_result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("错误: JSON-RPC 服务异常退出: {e}");
                ExitCode::FAILURE
            }
        },
        // Ctrl+C：保存会话 checkpoint（等价 aria2.saveSession / GUI 退出保存，
        // 未完成任务基于已下载字节在下次启动时 Range 续传）后退出
        ctrl_c = tokio::signal::ctrl_c() => {
            if let Err(e) = ctrl_c {
                eprintln!("警告: 监听 Ctrl+C 失败: {e}");
            }
            let saved = match task_manager.repo.lock() {
                Ok(repo) => motrix_core::session::save_checkpoint(&data_dir, &repo),
                Err(e) => Err(format!("获取任务仓库锁失败: {e}")),
            };
            match saved {
                Ok(()) => println!("daemon 已停止（会话 checkpoint 已保存）"),
                Err(e) => eprintln!("警告: 退出保存 checkpoint 失败: {e}"),
            }
            ExitCode::SUCCESS
        }
    }
}

/// 顶层流程（远程模式）：解析密钥 → 创建客户端 → 分发子命令
/// （daemon 已在 main() 中拦截进入 tokio runtime，不会走到这里）
fn run(cli: &Cli) -> Result<(), String> {
    // 解析密钥：--rpc-secret 参数 → 环境变量 MOTRIX_RPC_SECRET → system.json
    let secret = resolve_secret(cli.rpc_secret.as_deref());
    let client = JsonRpcClient::new(cli.rpc_url.clone(), secret);
    dispatch(cli, &client)
}

/// 将客户端错误转换为用户可读的中文提示（供顶层 `Result<(), String>` 的 `?` 使用）。
/// 对连接失败与认证失败附加常见排查建议，其余错误原样透传。
fn rpc_err(e: ClientError) -> String {
    match &e {
        ClientError::Connection(_) => {
            format!("{e}（请确认 Motrix 后端已启动，且 --rpc-url 指向正确）")
        }
        ClientError::ServerError { code: 1, .. } => format!(
            "{e}（请检查 --rpc-secret / 环境变量 MOTRIX_RPC_SECRET / system.json 中的 rpc-secret）"
        ),
        _ => e.to_string(),
    }
}

/// 子命令分发：除 daemon 外全部走 JSON-RPC（CLI 只是 JSON-RPC 的另一个客户端）
fn dispatch(cli: &Cli, client: &JsonRpcClient) -> Result<(), String> {
    match &cli.command {
        Command::Add { urls, dir, connections, limit } => {
            // 任务级 options（aria2 契约：选项值均为字符串）
            let mut options = Map::new();
            if let Some(d) = dir {
                options.insert("dir".to_string(), json!(d));
            }
            if let Some(x) = connections {
                options.insert("max-connection-per-server".to_string(), json!(x.to_string()));
            }
            if let Some(l) = limit {
                options.insert("max-download-limit".to_string(), json!(l));
            }
            // aria2.addUri：params = [uris 数组, options 对象]，返回 gid 数组
            let result = client
                .call("aria2.addUri", vec![json!(urls), json!(options)])
                .map_err(rpc_err)?;
            emit(cli, &result, |r| {
                // 人类可读：逐行打印返回的 gid
                match r.as_array() {
                    Some(gids) => gids
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|g| format!("任务已添加: {g}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    None => format!("任务已添加: {r}"),
                }
            });
            Ok(())
        }
        Command::AddTorrent { torrent } => {
            // 入参归一化：magnet 原样 / .torrent 文件转 base64 / 其余视为 base64
            let payload = prepare_torrent(torrent)?;
            // aria2.addTorrent：params = [base64 种子 或 magnet, options]，返回 gid
            let result = client
                .call("aria2.addTorrent", vec![json!(payload)])
                .map_err(rpc_err)?;
            emit(cli, &result, |r| format!("任务已添加: {r}"));
            Ok(())
        }
        Command::List { kind } => {
            // 按类型选择 tell 方法（waiting/stopped 走 offset/num 分页，aria2 契约）
            let result = match kind.as_str() {
                "active" => client.call("aria2.tellActive", vec![]).map_err(rpc_err)?,
                "waiting" => {
                    client
                        .call("aria2.tellWaiting", vec![json!(0), json!(100)])
                        .map_err(rpc_err)?
                }
                "stopped" => {
                    client
                        .call("aria2.tellStopped", vec![json!(0), json!(100)])
                        .map_err(rpc_err)?
                }
                other => {
                    return Err(format!(
                        "未知任务类型 `{other}`（可选: active | waiting | stopped）"
                    ))
                }
            };
            emit(cli, &result, |r| format_task_table(r));
            Ok(())
        }
        Command::Status { gid } => {
            let result = client
                .call("aria2.tellStatus", vec![json!(gid)])
                .map_err(rpc_err)?;
            emit(cli, &result, |r| format_task_detail(r));
            Ok(())
        }
        Command::Pause { gids } => call_each(client, "aria2.pause", gids, "已暂停", cli),
        Command::PauseAll => {
            let result = client.call("aria2.pauseAll", vec![]).map_err(rpc_err)?;
            emit(cli, &result, |_| "已暂停全部任务".to_string());
            Ok(())
        }
        Command::Resume { gids } => call_each(client, "aria2.unpause", gids, "已恢复", cli),
        Command::ResumeAll => {
            let result = client.call("aria2.unpauseAll", vec![]).map_err(rpc_err)?;
            emit(cli, &result, |_| "已恢复全部任务".to_string());
            Ok(())
        }
        Command::Remove { gids, force, delete_files } => {
            // force 走 forceRemove（跳过等待直接删除），否则走 remove
            let method = if *force { "aria2.forceRemove" } else { "aria2.remove" };
            call_each(client, method, gids, "已删除", cli)?;
            // --delete-files：本阶段仅作请求标记（aria2.remove/forceRemove 无此参数），
            // 后端删除文件能力由后续阶段通过扩展方法实现
            if *delete_files {
                eprintln!("提示: --delete-files 请求已记录，后端删除文件能力将在后续阶段实现（本阶段仅删除任务记录）");
            }
            Ok(())
        }
        Command::Purge => {
            let result = client.call("aria2.purgeDownloadResult", vec![]).map_err(rpc_err)?;
            emit(cli, &result, |_| "已清除下载记录".to_string());
            Ok(())
        }
        Command::Config { action } => handle_config(cli, client, action),
        Command::Engine { action } => match action {
            EngineAction::Status => {
                // 经 system.multicall 一次获取 getVersion + getGlobalStat
                // （--json 直接透传 multicall 结果数组，与后端契约一致）
                let result = client
                    .call(
                        "system.multicall",
                        vec![json!([["aria2.getVersion", []], ["aria2.getGlobalStat", []]])],
                    )
                    .map_err(rpc_err)?;
                emit(cli, &result, |r| format_engine_status(r));
                Ok(())
            }
        },
        // daemon 已在 main() 中拦截进入 tokio runtime，此处仅为穷尽枚举
        Command::Daemon => unreachable!("daemon 已在 main() 中处理"),
    }
}

/// 批量调用同一方法（每个 gid 一次调用），逐项输出结果
fn call_each(
    client: &JsonRpcClient,
    method: &str,
    gids: &[String],
    action: &str,
    cli: &Cli,
) -> Result<(), String> {
    if gids.is_empty() {
        return Err("至少需要一个 gid 参数".to_string());
    }
    for gid in gids {
        // 方法结果：成功时为对应 gid 字符串（aria2 契约），失败即整体报错
        let result = client.call(method, vec![json!(gid)]).map_err(rpc_err)?;
        emit(cli, &result, |r| format!("{action}: {r}"));
    }
    Ok(())
}

/// config 子命令：get（getGlobalOption）/ set（changeGlobalOption 或 motrix.saveUserConfig）
fn handle_config(cli: &Cli, client: &JsonRpcClient, action: &ConfigAction) -> Result<(), String> {
    match action {
        ConfigAction::Get { key } => {
            let result = client.call("aria2.getGlobalOption", vec![]).map_err(rpc_err)?;
            match key {
                Some(k) => {
                    // 单键查询：直接取对应值输出（缺键视为未找到）
                    let v = result.get(k).cloned().unwrap_or(Value::Null);
                    if v.is_null() {
                        return Err(format!("未找到配置键: {k}"));
                    }
                    emit(cli, &v, |r| {
                        r.as_str().map(str::to_string).unwrap_or_else(|| r.to_string())
                    });
                }
                None => emit(cli, &result, |r| format_kv_pairs(r)),
            }
            Ok(())
        }
        ConfigAction::Set { key, value } => {
            // userKeys → 非标准扩展方法 motrix.saveUserConfig（params[0] = userKeys 对象）；
            if USER_KEYS.contains(&key.as_str()) {
                let result = client.call("motrix.saveUserConfig", vec![json!({ key: value })]);
                match result {
                    Ok(r) => {
                        emit(cli, &r, |_| format!("已保存用户配置: {key} = {value}"));
                        Ok(())
                    }
                    // 后端尚未实现该扩展方法（-32601 方法未实现）时给出友好提示
                    Err(ClientError::ServerError { code, message })
                        if code == -32601 || message.contains("未实现") =>
                    {
                        Err(format!(
                            "后端暂未实现 motrix.saveUserConfig（{message}），用户配置写入将在后续阶段上线"
                        ))
                    }
                    Err(e) => Err(e.to_string()),
                }
            // systemKeys → aria2.changeGlobalOption（值按字符串传入）
            } else if SYSTEM_KEYS.contains(&key.as_str()) {
                let result = client
                    .call("aria2.changeGlobalOption", vec![json!({ key: value })])
                    .map_err(rpc_err)?;
                emit(cli, &result, |_| format!("已设置全局选项: {key} = {value}"));
                Ok(())
            } else {
                Err(format!(
                    "未知配置键: {key}（键名清单见 src/shared/configKeys.js 的 userKeys / systemKeys）"
                ))
            }
        }
    }
}

/// 解析 RPC 密钥：`--rpc-secret` 参数 → 环境变量 `MOTRIX_RPC_SECRET` → `system.json` 的
/// `rpc-secret`（解析顺序与 MIGRATION-TAURI.md 9.4 一致；全部缺失时为空串，即免认证）
fn resolve_secret(arg: Option<&str>) -> String {
    if let Some(s) = arg {
        return s.to_string();
    }
    if let Ok(s) = std::env::var("MOTRIX_RPC_SECRET") {
        if !s.is_empty() {
            return s;
        }
    }
    // 数据目录统一走 motrix-core::config::data_dir()（见 read_system_json 注释）；
    // 文件不存在或缺键则回退空串（免认证）
    match read_system_json() {
        Some(v) => v
            .get("rpc-secret")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// 读取数据目录下的 system.json（文件不存在 / 解析失败返回 None，调用方回退空密钥）
///
/// 数据目录统一走 motrix-core::config::data_dir()（与 GUI / daemon 同一实现，
/// 见 motrix-core/config.rs 的目录对齐说明），不再用 dirs 自实现。
fn read_system_json() -> Option<Value> {
    let base = motrix_core::config::data_dir().ok()?;
    let path = base.join("system.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// add-torrent 入参归一化：
/// - `magnet:` 前缀 → 原样返回（磁力链接直接传给后端）
/// - `.torrent` 结尾的本地文件路径 → 读取二进制并转 base64（aria2.addTorrent 契约）
/// - 其余 → 视为 base64 编码的种子内容，原样传递
fn prepare_torrent(input: &str) -> Result<String, String> {
    if input.starts_with("magnet:") {
        return Ok(input.to_string());
    }
    // .torrent 本地文件：读取后 base64 编码
    if input.ends_with(".torrent") {
        let data =
            std::fs::read(input).map_err(|e| format!("读取种子文件 `{input}` 失败: {e}"))?;
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        return Ok(b64);
    }
    // 其余视为 base64 内容
    Ok(input.to_string())
}

// ---------------------------------------------------------------------------
// 输出辅助
// ---------------------------------------------------------------------------

/// 输出 RPC 结果：
/// - `--json`：直接透传 result（pretty 打印），保证与后端契约（如 tellStatus）一致；
/// - `--quiet`：抑制成功输出（错误仍由调用方走 stderr）；
/// - 默认：调用 human 闭包生成人类可读文本打印到 stdout。
fn emit(cli: &Cli, result: &Value, human: impl FnOnce(&Value) -> String) {
    if cli.quiet {
        return;
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(result).unwrap_or_default());
    } else {
        println!("{}", human(result));
    }
}

/// 人类可读：任务列表表格（GID / 状态 / 进度 / 速度）
fn format_task_table(tasks: &Value) -> String {
    let Some(arr) = tasks.as_array() else {
        return "（无任务）".to_string();
    };
    if arr.is_empty() {
        return "（无任务）".to_string();
    }
    let rows: Vec<String> = arr
        .iter()
        .map(|t| {
            let gid = t.get("gid").and_then(Value::as_str).unwrap_or("-");
            let status = status_label(t.get("status").and_then(Value::as_str).unwrap_or("-"));
            let total = parse_u64(t.get("totalLength")).unwrap_or(0);
            let done = parse_u64(t.get("completedLength")).unwrap_or(0);
            // 进度百分比：总长度未知（0）时显示 "-"
            let percent = if total > 0 {
                format!("{:.1}%", done as f64 * 100.0 / total as f64)
            } else {
                "-".to_string()
            };
            let speed = format_speed(parse_u64(t.get("downloadSpeed")).unwrap_or(0));
            format!("{gid:<16} {status:<8} {percent:>7} {speed:>10}")
        })
        .collect();
    let mut out = format!("{:<16} {:<8} {:>7} {:>10}\n", "GID", "状态", "进度", "速度");
    out.push_str(&"─".repeat(46));
    out.push('\n');
    out.push_str(&rows.join("\n"));
    out
}

/// 人类可读：单任务详情（逐字段打印 + 文件列表）
fn format_task_detail(t: &Value) -> String {
    let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("-").to_string();
    let mut out = String::new();
    out.push_str(&format!("gid:            {}\n", s("gid")));
    out.push_str(&format!("状态:           {}\n", status_label(&s("status"))));
    // 进度：completedLength / totalLength（aria2 数值字段均为字符串）
    let done = parse_u64(t.get("completedLength")).unwrap_or(0);
    let total = parse_u64(t.get("totalLength")).unwrap_or(0);
    if total > 0 {
        out.push_str(&format!(
            "进度:           {:.1}% （{} / {}）\n",
            done as f64 * 100.0 / total as f64,
            format_bytes(done),
            format_bytes(total)
        ));
    } else {
        out.push_str(&format!("已下载:         {}\n", format_bytes(done)));
    }
    out.push_str(&format!(
        "下载速度:       {}\n",
        format_speed(parse_u64(t.get("downloadSpeed")).unwrap_or(0))
    ));
    out.push_str(&format!(
        "上传速度:       {}\n",
        format_speed(parse_u64(t.get("uploadSpeed")).unwrap_or(0))
    ));
    // 错误信息（仅出错任务有）
    if let Some(code) = t.get("errorCode") {
        out.push_str(&format!("错误码:         {code}\n"));
    }
    if let Some(msg) = t.get("errorMessage").and_then(Value::as_str) {
        if !msg.is_empty() {
            out.push_str(&format!("错误信息:       {msg}\n"));
        }
    }
    if let Some(dir) = t.get("dir").and_then(Value::as_str) {
        if !dir.is_empty() {
            out.push_str(&format!("目录:           {dir}\n"));
        }
    }
    // 文件列表（aria2 files: [{path, length, completedLength}]）
    if let Some(files) = t.get("files").and_then(Value::as_array) {
        if !files.is_empty() {
            out.push_str("文件:\n");
            for f in files {
                let path = f.get("path").and_then(Value::as_str).unwrap_or("-");
                let len = format_bytes(parse_u64(f.get("length")).unwrap_or(0));
                out.push_str(&format!("  - {path} （{len}）\n"));
            }
        }
    }
    out
}

/// 人类可读：引擎版本 + 全局统计（解析 system.multicall 结果数组
/// [getVersion 结果, getGlobalStat 结果]）
fn format_engine_status(r: &Value) -> String {
    let arr = r.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let version = arr
        .first()
        .and_then(|v| v.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("未知");
    let stat = arr.get(1).cloned().unwrap_or(Value::Null);
    let mut out = format!("引擎版本: {version}\n");
    // getGlobalStat 字段（aria2 惯例：数值均为字符串）
    for (label, key) in [
        ("下载速度", "downloadSpeed"),
        ("上传速度", "uploadSpeed"),
        ("活动任务", "numActive"),
        ("等待任务", "numWaiting"),
        ("已停止任务", "numStopped"),
        ("累计停止", "numStoppedTotal"),
    ] {
        let v = stat.get(key).and_then(Value::as_str).unwrap_or("-");
        out.push_str(&format!("{label}: {v}\n"));
    }
    out
}

/// 人类可读：键值对列表（config get 无 key 时的全部输出，按键名排序保证稳定）
fn format_kv_pairs(obj: &Value) -> String {
    let Some(map) = obj.as_object() else {
        return obj.to_string();
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys.iter()
        .map(|k| format!("{k} = {}", map[*k]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// aria2 状态码 → 中文标签（仅人类可读输出使用，--json 保持原值）
fn status_label(s: &str) -> &str {
    match s {
        "active" => "下载中",
        "waiting" => "等待中",
        "paused" => "已暂停",
        "error" => "出错",
        "complete" => "已完成",
        "removed" => "已移除",
        other => other,
    }
}

/// 解析 aria2 的字符串数值字段（"123" → 123；缺失 / 非法 → None）
fn parse_u64(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_str).and_then(|s| s.parse().ok())
}

/// 字节/秒 → 人类可读速度（如 "1.2 MiB/s"）
fn format_speed(bytes: u64) -> String {
    if bytes == 0 {
        return "-".to_string();
    }
    const UNITS: &[&str] = &["B/s", "KiB/s", "MiB/s", "GiB/s"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// 字节数 → 人类可读大小（如 "1.5 MiB"）
fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}
