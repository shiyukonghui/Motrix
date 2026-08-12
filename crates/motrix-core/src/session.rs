//! 会话持久化模块（Phase 2 Task 12）
//!
//! 负责两类会话数据的读写与恢复（参考 MIGRATION-TAURI.md 5.4 / 7.1 / 7.3 节与
//! tasks.md Task 12）：
//!
//! 1. **Electron 版旧会话 `download.session` 导入**（仅首次迁移使用）：
//!    aria2 会话文本，每行一条任务：`gid status 路径 urls...`（空格分隔）。
//!    `*.aria2` 私有控制文件**不解析**（aria2 私有二进制格式，不实现解析器），
//!    因此旧会话中的未完成任务无法续传，统一标记为 error + 提示重新下载；
//!    已完成 / 已移除任务直接进入历史列表。
//!
//! 2. **自有 checkpoint（`checkpoint.json`，JSON 数组）**：记录每个任务的
//!    gid / 状态 / 目录 / URL / 已下载字节 / 进度 / 错误信息 / 创建时间，
//!    应用退出时保存（等价 aria2.saveSession 语义）、下次启动时恢复；
//!    未完成任务以 waiting 状态恢复，用户恢复下载时基于已下载字节（Range）续传。
//!
//! 启动恢复流程 [`restore_session`]：
//!   - 优先读取 checkpoint（文件存在即视为"自有会话已接管"，**哪怕为空也不重复
//!     迁移旧会话**，避免用户在清空任务退出后旧 download.session 被反复导入）；
//!   - 无 checkpoint 时读取 `download.session` 解析导入，并在迁移完成后写一份
//!     checkpoint.json（后续启动直接走 checkpoint 路径）。

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::task::{Task, TaskRepository, TaskStatus};

/// 旧会话未完成任务统一写入的错误提示
/// （`*.aria2` 控制文件不再解析，无法断点续传，提示用户重新下载）
const UNFINISHED_MIGRATE_HINT: &str = "旧会话未完成任务，请重新下载（*.aria2 控制文件不再解析）";

/// 从 Electron 版 download.session 导入的任务（文本行：`gid status 路径 urls...`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTask {
    /// 任务 gid（16 位 hex，原样保留）
    pub gid: String,
    /// aria2 状态串（active/waiting/paused/error/complete/removed 等）
    pub status: String,
    /// 任务文件路径（download.session 第三字段）
    pub path: String,
    /// 下载源 URL 列表（空格分隔的剩余字段）
    pub urls: Vec<String>,
}

/// 自有 checkpoint 任务（`checkpoint.json` 数组元素，JSON 序列化）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointTask {
    /// 任务 gid
    pub gid: String,
    /// 任务状态串（aria2 状态）
    pub status: String,
    /// 保存目录
    pub dir: String,
    /// 下载源 URL 列表（从 files[].uris 收集；BT 任务为磁力链接 / base64 .torrent 源）
    pub urls: Vec<String>,
    /// 保存文件名（dir 下的相对路径，恢复时重建保存路径）
    pub out: Option<String>,
    /// 总长度（字节）
    pub total_length: u64,
    /// 已下载长度（字节，断点续传基础）
    pub completed_length: u64,
    /// 错误信息（error 任务保留）
    pub error_message: Option<String>,
    /// 创建时间（unix 秒）
    pub created_at: u64,
    // ------------------------------------------------------------------
    // BT 任务字段（Phase 3，librqbit 集成；`#[serde(default)]` 保证旧 checkpoint
    // 无这些键时也能正常解析，向后兼容）
    // ------------------------------------------------------------------
    /// 是否 BT 任务（恢复时按 BT 语义重建 bittorrent 信息）
    #[serde(default)]
    pub is_bt: bool,
    /// BT 信息哈希（40 位 hex；恢复时填充 bittorrent.info_hash）
    #[serde(default)]
    pub info_hash: Option<String>,
    /// 降采样 bitfield（checkpoint 保存时的进度位图；恢复时填充 task.bitfield）
    #[serde(default)]
    pub piece_bitmap: Option<String>,
}

// ---------------------------------------------------------------------------
// aria2 旧会话解析（download.session）
// ---------------------------------------------------------------------------

/// 解析 Electron 版 aria2 会话文本为任务列表
///
/// 每行格式：`gid status 路径 urls...`（空格分隔），其中：
/// - 第 1 字段：gid（原样保留）
/// - 第 2 字段：aria2 状态串（active/waiting/paused/error/complete/removed 等）
/// - 第 3 字段：任务文件路径
/// - 其余字段：空格分隔的下载源 URL（可空）
///
/// 容错策略（**跳过错行**）：空行、字段数不足 3 的行、状态串非法的行一律跳过，
/// 不 panic；URL 为空的任务也保留（如已移除的无源任务）。
pub fn parse_aria2_session(content: &str) -> Vec<ImportedTask> {
    let mut tasks = Vec::new();
    for line in content.lines() {
        // 去首尾空白后切分；全空白行（空行）跳过
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 3 {
            continue;
        }
        // 状态串必须是合法 aria2 状态（TaskStatus::from_str 同时接受 seeding）
        if TaskStatus::from_str(tokens[1]).is_none() {
            continue;
        }
        // 第 3 字段为路径，其余为 URL
        let urls: Vec<String> = tokens[3..].iter().map(|s| s.to_string()).collect();
        tasks.push(ImportedTask {
            gid: tokens[0].to_string(),
            status: tokens[1].to_string(),
            path: tokens[2].to_string(),
            urls,
        });
    }
    tasks
}

// ---------------------------------------------------------------------------
// 自有 checkpoint（checkpoint.json）
// ---------------------------------------------------------------------------

/// 从文件路径 + 保存目录推导 checkpoint 的 out 字段
///
/// 优先取 dir 下的相对路径（保留子目录结构，如 "sub/file.bin"），
/// 路径不在 dir 下或 dir 为空时退化为文件名；均不可得时返回 None。
fn out_from_path(path: &str, dir: &str) -> Option<String> {
    let p = Path::new(path);
    if !dir.is_empty() {
        if let Ok(rel) = p.strip_prefix(dir) {
            if !rel.as_os_str().is_empty() {
                return Some(rel.to_string_lossy().to_string());
            }
        }
    }
    p.file_name().map(|n| n.to_string_lossy().to_string())
}

/// 从任务仓库导出 checkpoint 任务列表
///
/// 覆盖仓库全部在册任务（`repo.all()`，不含 removed 历史），
/// URL 从 `files[].uris` 收集（多文件任务平铺全部源）、dir 取 `task.dir`、
/// 状态串取 `TaskStatus::as_str`，进度 / 错误信息 / 创建时间原样保留。
pub fn checkpoint_tasks(repo: &TaskRepository) -> Vec<CheckpointTask> {
    repo.all()
        .iter()
        .map(|t| {
            // 任务 URL：从 files[].uris 收集（每个元素为 (uri, status)）
            let urls: Vec<String> = t
                .files
                .iter()
                .flat_map(|f| f.uris.iter().map(|(uri, _)| uri.clone()))
                .collect();
            // out：dir 下的相对路径（退化文件名），供恢复时重建保存路径
            let out = t.files.first().and_then(|f| out_from_path(&f.path, &t.dir));
            CheckpointTask {
                gid: t.gid.clone(),
                status: t.status.as_str().to_string(),
                dir: t.dir.clone(),
                urls,
                out,
                total_length: t.total_length,
                completed_length: t.completed_length,
                error_message: t.error_message.clone(),
                created_at: t.created_at,
                // BT 字段（Phase 3）：bittorrent 存在即为 BT 任务；info_hash 与
                // 降采样 bitfield 一并导出（恢复时重建 bittorrent / 进度位图）
                is_bt: t.bittorrent.is_some(),
                info_hash: t.bittorrent.as_ref().and_then(|b| b.info_hash.clone()),
                piece_bitmap: (!t.bitfield.is_empty()).then(|| t.bitfield.clone()),
            }
        })
        .collect()
}

/// 保存 checkpoint 到 `<data_dir>/checkpoint.json`（JSON 数组，等价 aria2.saveSession 语义）
///
/// 目录不存在时自动创建；写盘失败返回错误字符串（调用方记 warn 日志）。
pub fn save_checkpoint(data_dir: &Path, repo: &TaskRepository) -> Result<(), String> {
    let tasks = checkpoint_tasks(repo);
    // 序列化为 JSON 数组（Value 形式，便于后续扩展键）
    let value = serde_json::to_value(&tasks).map_err(|e| format!("序列化 checkpoint 失败: {e}"))?;
    let content =
        serde_json::to_string_pretty(&value).map_err(|e| format!("格式化 checkpoint JSON 失败: {e}"))?;
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("创建数据目录失败（{}）: {e}", data_dir.display()))?;
    let path = data_dir.join("checkpoint.json");
    fs::write(&path, content)
        .map_err(|e| format!("写入 checkpoint 失败（{}）: {e}", path.display()))?;
    info!(
        "[Motrix] 已保存会话 checkpoint（{} 个任务）: {}",
        tasks.len(),
        path.display()
    );
    Ok(())
}

/// 读取 `<data_dir>/checkpoint.json`
///
/// 文件不存在 / 读取失败 / 解析失败均返回空 vec（不 panic），解析失败记 warn 日志。
pub fn load_checkpoint(data_dir: &Path) -> Vec<CheckpointTask> {
    let path = data_dir.join("checkpoint.json");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        // 文件不存在（首次启动 / 尚未迁移）：按空处理
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Vec<CheckpointTask>>(&content) {
        Ok(tasks) => tasks,
        Err(e) => {
            warn!("[Motrix] checkpoint 解析失败（按空处理）: {e}");
            Vec::new()
        }
    }
}

/// 把 checkpoint 任务恢复进任务仓库
///
/// 状态映射（构造后直接赋值，绕过状态机，属"导入恢复"而非运行期迁移）：
/// - complete 保持 complete（历史列表）
/// - active / waiting 恢复为 waiting（下次用户 / 启动时可继续；BT 任务 resume 时
///   经 engine::ensure_bt_registered 从源重新加入 librqbit 续传）
/// - paused 恢复为 paused
/// - error 保持 error（保留错误信息）
/// - removed 保持 removed（出现在 stopped 历史）
/// - seeding 保持 seeding（BT 做种任务）
/// - 未知状态串回退 waiting（防御）
///
/// HTTP 任务用 `Task::new_http_task` 重建（gid / urls / dir / out），随后赋值
/// completed_length / total_length / error_message / created_at 等字段；
/// **BT 任务**（`is_bt`）用 `Task::new_bt_task` 重建（bittorrent 信息 + 源），
/// 恢复 bitfield（降采样位图）与 info_hash；metadata 阶段任务（info_name 未保存）
/// 保持 info_name=None，resume 后由 metadata 就绪回调补齐。
pub fn restore_checkpoint(repo: &mut TaskRepository, tasks: Vec<CheckpointTask>) {
    for ct in tasks {
        // BT 任务：按 BT 语义重建（bittorrent 存在、源写入 files[0].uris[0]）
        if ct.is_bt {
            // 源：checkpoint urls 保存了磁力 / base64 .torrent（BT 任务 uris 即源）
            let source = ct.urls.first().cloned().unwrap_or_default();
            let mut task =
                Task::new_bt_task(ct.gid.clone(), &source, ct.dir.clone(), ct.info_hash.clone());
            // 恢复进度 / 错误信息 / 创建时间 / bitfield（保真）
            task.total_length = ct.total_length;
            task.completed_length = ct.completed_length;
            task.error_message = ct.error_message.clone();
            task.created_at = ct.created_at;
            task.bitfield = ct.piece_bitmap.clone().unwrap_or_default();
            // 状态映射（见函数注释；active/waiting → waiting 供用户手动 resume）
            task.status = match ct.status.as_str() {
                "complete" => TaskStatus::Complete,
                "paused" => TaskStatus::Paused,
                "error" => TaskStatus::Error,
                "active" | "waiting" => TaskStatus::Waiting,
                "removed" => TaskStatus::Removed,
                "seeding" => TaskStatus::Seeding,
                _ => TaskStatus::Waiting,
            };
            repo.add(task);
            continue;
        }
        // out 缺省时回退 "download"（与 engine.rs 的 file_name_from_uri 兜底一致）
        let out = ct.out.clone().unwrap_or_else(|| "download".to_string());
        let mut task = Task::new_http_task(ct.gid.clone(), &ct.urls, ct.dir.clone(), out);
        // 恢复进度 / 错误信息 / 创建时间（保真）
        task.total_length = ct.total_length;
        task.completed_length = ct.completed_length;
        task.error_message = ct.error_message.clone();
        task.created_at = ct.created_at;
        // 状态映射（见函数注释）
        task.status = match ct.status.as_str() {
            "complete" => TaskStatus::Complete,
            "paused" => TaskStatus::Paused,
            "error" => TaskStatus::Error,
            // 运行中 / 等待中任务恢复为 waiting：不自动启动下载，
            // 由用户在 UI 中手动恢复（KGet 基于已下载字节 Range 续传）
            "active" | "waiting" => TaskStatus::Waiting,
            "removed" => TaskStatus::Removed,
            "seeding" => TaskStatus::Seeding,
            _ => TaskStatus::Waiting,
        };
        repo.add(task);
    }
}

// ---------------------------------------------------------------------------
// 启动恢复流程
// ---------------------------------------------------------------------------

/// 启动时恢复会话（在 RPC 服务与广播循环启动之前调用，保证首帧快照含历史任务）
///
/// 流程：
/// 1. 优先读取 checkpoint：文件存在（**哪怕为空**）即视为"自有会话已接管"，
///    非空则 [`restore_checkpoint`] 恢复，空则跳过（不重复迁移旧会话）；
/// 2. 无 checkpoint 时读取 `<data_dir>/download.session`（Electron 版旧会话）
///    用 [`parse_aria2_session`] 解析导入：
///    - complete / removed 状态任务直接进仓库（历史列表，不自动下载）；
///    - 其余未完成任务以 status=error + 错误提示进仓库。
///    取舍说明：不置 waiting——waiting 表示可恢复下载，但旧任务未注册进引擎的
///    task_uris / task_options 内部表，无法真正续传（`*.aria2` 控制文件不解析），
///    error 状态让前端明确提示"重新下载"更诚实；
/// 3. 迁移完成后写一份 checkpoint.json（后续启动直接走 checkpoint 路径）。
pub fn restore_session(data_dir: &Path, repo: &mut TaskRepository) {
    let checkpoint_path = data_dir.join("checkpoint.json");

    // 1. 自有 checkpoint 优先（存在即视为已接管会话）
    if checkpoint_path.exists() {
        let checkpoint = load_checkpoint(data_dir);
        if checkpoint.is_empty() {
            // 空 checkpoint：上次退出时仓库无任务，无需恢复，也不再迁移旧会话
            info!("[Motrix] checkpoint 为空（上次退出时无任务），跳过会话恢复");
        } else {
            info!("[Motrix] 从 checkpoint 恢复 {} 个任务", checkpoint.len());
            restore_checkpoint(repo, checkpoint);
        }
        return;
    }

    // 2. 无 checkpoint：尝试解析 Electron 版旧会话 download.session
    let session_path = data_dir.join("download.session");
    let content = match fs::read_to_string(&session_path) {
        Ok(content) => content,
        // 旧会话不存在（全新安装 / 已被重置）：无可恢复内容
        Err(_) => return,
    };
    let imported = parse_aria2_session(&content);
    if imported.is_empty() {
        // 文件存在但无有效行：写一份空 checkpoint 标记迁移完成，避免下次重复解析
        info!("[Motrix] download.session 无可导入任务，标记迁移完成");
        if let Err(e) = save_checkpoint(data_dir, repo) {
            warn!("[Motrix] 迁移后写 checkpoint 失败: {e}");
        }
        return;
    }
    info!(
        "[Motrix] 解析到旧会话 download.session（{} 个任务），开始迁移",
        imported.len()
    );

    for imported_task in imported {
        // 从路径拆分保存目录与文件名（恢复保存路径用）
        let dir = Path::new(&imported_task.path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let out = Path::new(&imported_task.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());

        // 状态映射：complete / removed 进历史列表；其余未完成任务标记 error + 提示
        let (status, error_message) = match imported_task.status.as_str() {
            "complete" => (TaskStatus::Complete, None),
            "removed" => (TaskStatus::Removed, None),
            _ => (TaskStatus::Error, Some(UNFINISHED_MIGRATE_HINT.to_string())),
        };
        let mut task = Task::new_http_task(imported_task.gid, &imported_task.urls, dir, out);
        task.status = status; // 导入场景直接赋值（绕过状态机）
        if let Some(message) = error_message {
            task.error_code = Some(1);
            task.error_message = Some(message);
        }
        repo.add(task);
    }

    // 3. 迁移完成：写一份 checkpoint.json（后续启动直接走 checkpoint 路径）
    if let Err(e) = save_checkpoint(data_dir, repo) {
        warn!("[Motrix] 迁移后写 checkpoint 失败: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 创建带唯一后缀的临时目录（测试后清理）
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "motrix-session-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 清理临时目录
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    // ------------------------------------------------------------------
    // 测试 1：parse_aria2_session 多行样本解析（含错行容错）
    // ------------------------------------------------------------------
    #[test]
    fn parse_aria2_session_parses_lines_and_skips_bad() {
        // 样本：完整行 / 多 URL 行 / 无 URL 行 / 空行 / token 不足错行 / 非法状态错行
        let content = "0123456789abcdef complete /downloads/a.zip https://example.com/a.zip\n\
fedcba9876543210 waiting /downloads/b.bin https://example.com/b.bin https://mirror.example.com/b.bin\n\
aabbccddeeff0011 paused /downloads/c.mp4\n\
\n\
abc123\n\
deadbeefdeadbeef unknown /tmp/x.zip https://example.com/x.zip\n";
        let tasks = parse_aria2_session(content);
        assert_eq!(tasks.len(), 3);

        // 完整行：gid / status / path / 单 URL
        assert_eq!(tasks[0].gid, "0123456789abcdef");
        assert_eq!(tasks[0].status, "complete");
        assert_eq!(tasks[0].path, "/downloads/a.zip");
        assert_eq!(tasks[0].urls, vec!["https://example.com/a.zip"]);

        // 多 URL 行：全部保留
        assert_eq!(tasks[1].status, "waiting");
        assert_eq!(
            tasks[1].urls,
            vec![
                "https://example.com/b.bin".to_string(),
                "https://mirror.example.com/b.bin".to_string()
            ]
        );

        // 无 URL 行（gid/status/path 三字段）也可解析，urls 为空
        assert_eq!(tasks[2].status, "paused");
        assert!(tasks[2].urls.is_empty());
    }

    #[test]
    fn parse_aria2_session_tolerates_crlf_and_whitespace() {
        // Windows 换行（\r\n）与行首多余空白也应正确解析
        let content = "  0123456789abcdef complete /downloads/a.zip https://example.com/a.zip  \r\n\
fedcba9876543210 waiting /downloads/b.bin https://example.com/b.bin\r\n";
        let tasks = parse_aria2_session(content);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].gid, "0123456789abcdef");
        assert_eq!(tasks[0].urls, vec!["https://example.com/a.zip"]);
        assert_eq!(tasks[1].status, "waiting");
    }

    // ------------------------------------------------------------------
    // 测试 2：checkpoint 导出 → 保存 → 加载 → 恢复 往返（状态/进度/URL 保真）
    // ------------------------------------------------------------------
    #[test]
    fn checkpoint_roundtrip_preserves_state_progress_and_urls() {
        let dir = temp_dir();

        // 构造 3 个不同状态的任务：complete / waiting（带进度）/ error（带错误信息）
        let mut repo = TaskRepository::new();

        let urls_a = vec!["https://example.com/a.zip".to_string()];
        let mut a = Task::new_http_task("aaaaaaaaaaaaaaaa", &urls_a, "/downloads", "a.zip");
        a.status = TaskStatus::Complete; // 构造后直接赋值（导入恢复场景绕过状态机）
        a.total_length = 1000;
        a.completed_length = 1000;
        a.created_at = 111;

        let urls_b = vec![
            "https://example.com/b.bin".to_string(),
            "https://mirror.example.com/b.bin".to_string(),
        ];
        let mut b = Task::new_http_task("bbbbbbbbbbbbbbbb", &urls_b, "/downloads", "b.bin");
        b.total_length = 2000;
        b.completed_length = 500; // 部分进度（断点续传基础）
        b.created_at = 222;

        let urls_c = vec!["https://example.com/c.mp4".to_string()];
        let mut c = Task::new_http_task("cccccccccccccccc", &urls_c, "/downloads", "c.mp4");
        c.status = TaskStatus::Error;
        c.error_message = Some("下载失败: timeout".to_string());
        c.total_length = 3000;
        c.completed_length = 100;
        c.created_at = 333;

        repo.add(a);
        repo.add(b);
        repo.add(c);

        // 导出 → 保存 → 加载
        let exported = checkpoint_tasks(&repo);
        assert_eq!(exported.len(), 3);
        save_checkpoint(&dir, &repo).expect("保存 checkpoint 应成功");
        assert!(dir.join("checkpoint.json").exists(), "checkpoint.json 应已写出");
        let loaded = load_checkpoint(&dir);
        assert_eq!(loaded.len(), 3);

        // 恢复进全新仓库：状态 / 进度 / URL / 错误信息 / 创建时间保真
        let mut repo2 = TaskRepository::new();
        restore_checkpoint(&mut repo2, loaded);
        assert_eq!(repo2.all().len(), 3);

        let a2 = repo2.get("aaaaaaaaaaaaaaaa").expect("complete 任务应恢复");
        assert_eq!(a2.status, TaskStatus::Complete);
        assert_eq!(a2.total_length, 1000);
        assert_eq!(a2.completed_length, 1000);
        assert_eq!(a2.created_at, 111);
        assert_eq!(a2.files[0].uris[0].0, "https://example.com/a.zip");
        assert_eq!(a2.files[0].path, "/downloads/a.zip");

        let b2 = repo2.get("bbbbbbbbbbbbbbbb").expect("waiting 任务应恢复");
        assert_eq!(b2.status, TaskStatus::Waiting);
        assert_eq!(b2.total_length, 2000);
        assert_eq!(b2.completed_length, 500);
        assert_eq!(b2.created_at, 222);
        assert_eq!(b2.files[0].uris.len(), 2);
        assert_eq!(b2.files[0].uris[1].0, "https://mirror.example.com/b.bin");

        let c2 = repo2.get("cccccccccccccccc").expect("error 任务应恢复");
        assert_eq!(c2.status, TaskStatus::Error);
        assert_eq!(c2.total_length, 3000);
        assert_eq!(c2.completed_length, 100);
        assert_eq!(c2.error_message.as_deref(), Some("下载失败: timeout"));
        assert_eq!(c2.created_at, 333);

        cleanup(&dir);
    }

    // ------------------------------------------------------------------
    // 测试 2.5：BT checkpoint 往返（Phase 3）：is_bt / info_hash / piece_bitmap
    //          导出 → 恢复，未完成任务以 Waiting 恢复（resume 后重新加入引擎续传）
    // ------------------------------------------------------------------
    #[test]
    fn checkpoint_roundtrip_preserves_bt_fields() {
        let dir = temp_dir();
        // 40 位 info_hash（6 组 c0ffee + abcd）
        let info_hash = "c0ffeec0ffeec0ffeec0ffeec0ffeec0ffeeabcd";
        let magnet = format!("magnet:?xt=urn:btih:{info_hash}");

        // 构造两个 BT 任务：未完成任务（waiting + 进度）与做种任务（seeding）
        let mut repo = TaskRepository::new();
        let mut bt1 = Task::new_bt_task(
            "bt00000000000001",
            &magnet,
            "/downloads",
            Some(info_hash.to_string()),
        );
        bt1.total_length = 1000;
        bt1.completed_length = 400; // 部分进度（断点续传基础）
        bt1.bitfield = "05f".to_string(); // 降采样位图占位
        bt1.created_at = 111;

        let mut bt2 = Task::new_bt_task(
            "bt00000000000002",
            &magnet,
            "/downloads",
            Some(info_hash.to_string()),
        );
        bt2.total_length = 1000;
        bt2.completed_length = 1000;
        bt2.status = TaskStatus::Seeding; // 做种中
        bt2.created_at = 222;

        repo.add(bt1);
        repo.add(bt2);

        // 导出 → 保存 → 加载
        let exported = checkpoint_tasks(&repo);
        assert_eq!(exported.len(), 2);
        assert!(exported.iter().all(|c| c.is_bt));
        assert!(exported
            .iter()
            .all(|c| c.info_hash.as_deref() == Some(info_hash)));
        assert_eq!(exported[0].piece_bitmap.as_deref(), Some("05f"));
        // BT 任务的 urls 保存了源（磁力链接）
        assert_eq!(exported[0].urls, vec![magnet.clone()]);

        save_checkpoint(&dir, &repo).expect("保存 checkpoint 应成功");
        let loaded = load_checkpoint(&dir);
        assert_eq!(loaded.len(), 2);

        // 恢复进全新仓库：BT 字段保真
        let mut repo2 = TaskRepository::new();
        restore_checkpoint(&mut repo2, loaded);
        assert_eq!(repo2.all().len(), 2);

        // 未完成任务：Waiting 恢复、bitfield / info_hash / 源保真
        let r1 = repo2.get("bt00000000000001").expect("BT 任务应恢复");
        assert_eq!(r1.status, TaskStatus::Waiting);
        assert_eq!(r1.total_length, 1000);
        assert_eq!(r1.completed_length, 400);
        assert_eq!(r1.bitfield, "05f");
        assert_eq!(r1.created_at, 111);
        assert!(r1.bittorrent.is_some(), "BT 任务应重建 bittorrent 信息");
        assert_eq!(
            r1.bittorrent.as_ref().unwrap().info_hash.as_deref(),
            Some(info_hash)
        );
        // 磁力 metadata 阶段：info_name=None（前端 isMagnetTask）
        assert!(r1.bittorrent.as_ref().unwrap().info_name.is_none());
        // 源保留在 files[0].uris[0]（resume 时重新加入引擎取源）
        assert_eq!(r1.files[0].uris[0].0, magnet);

        // 做种任务：Seeding 保持
        let r2 = repo2.get("bt00000000000002").expect("做种任务应恢复");
        assert_eq!(r2.status, TaskStatus::Seeding);

        // 向后兼容：旧 checkpoint（无 is_bt 等键）应正常解析为 HTTP 任务语义
        let legacy = r#"[
            {"gid":"aaaaaaaaaaaaaaaa","status":"waiting","dir":"/d","urls":["https://a/x"],
             "out":"x.bin","total_length":10,"completed_length":5,
             "error_message":null,"created_at":1}
        ]"#;
        fs::write(dir.join("checkpoint.json"), legacy).unwrap();
        let loaded_legacy = load_checkpoint(&dir);
        assert_eq!(loaded_legacy.len(), 1);
        assert!(!loaded_legacy[0].is_bt, "旧 checkpoint 默认非 BT 任务");
        let mut repo3 = TaskRepository::new();
        restore_checkpoint(&mut repo3, loaded_legacy);
        let r3 = repo3.get("aaaaaaaaaaaaaaaa").expect("旧 checkpoint 任务应恢复");
        assert!(r3.bittorrent.is_none(), "旧 checkpoint 任务应为 HTTP 语义");

        cleanup(&dir);
    }

    // ------------------------------------------------------------------
    // 测试 3：restore_session 无 checkpoint 时从 download.session 导入，
    //         未完成任务被标记 error，迁移后写出 checkpoint
    // ------------------------------------------------------------------
    #[test]
    fn restore_session_imports_old_session_and_marks_unfinished() {
        let dir = temp_dir();
        // 构造 Electron 版旧会话 download.session（已完成 / 未完成 / 已移除任务）
        let session = "0123456789abcdef complete /downloads/a.zip https://example.com/a.zip\n\
fedcba9876543210 active /downloads/b.bin https://example.com/b.bin\n\
aabbccddeeff0011 paused /downloads/c.mp4 https://example.com/c.mp4\n\
deadbeefdeadbeef removed /downloads/d.torrent\n";
        fs::write(dir.join("download.session"), session).expect("写旧会话文件失败");

        let mut repo = TaskRepository::new();
        restore_session(&dir, &mut repo);

        // complete 任务直接进历史（保持 complete，不自动下载）
        let a = repo.get("0123456789abcdef").expect("complete 任务应导入");
        assert_eq!(a.status, TaskStatus::Complete);
        assert_eq!(a.files[0].uris[0].0, "https://example.com/a.zip");
        assert_eq!(a.files[0].path, "/downloads/a.zip");

        // removed 任务直接进历史（保持 removed，出现在 stopped 列表）
        let d = repo.get("deadbeefdeadbeef").expect("removed 任务应导入");
        assert_eq!(d.status, TaskStatus::Removed);

        // 未完成任务（active / paused）被标记 error + 提示重新下载
        let b = repo.get("fedcba9876543210").expect("active 任务应导入");
        assert_eq!(b.status, TaskStatus::Error);
        assert!(
            b.error_message.as_deref().unwrap().contains("请重新下载"),
            "未完成任务应带重新下载提示"
        );
        let c = repo.get("aabbccddeeff0011").expect("paused 任务应导入");
        assert_eq!(c.status, TaskStatus::Error);

        // 迁移完成后应写出一份 checkpoint.json（后续启动直接走 checkpoint 路径）
        assert!(dir.join("checkpoint.json").exists(), "迁移后应写出 checkpoint");

        // 再次恢复：走 checkpoint 路径（不再重复迁移旧会话），未完成任务保持 error 标记
        let mut repo2 = TaskRepository::new();
        restore_session(&dir, &mut repo2);
        assert_eq!(repo2.all().len(), 4);
        assert_eq!(repo2.get("0123456789abcdef").unwrap().status, TaskStatus::Complete);
        assert_eq!(repo2.get("fedcba9876543210").unwrap().status, TaskStatus::Error);
        assert_eq!(repo2.get("aabbccddeeff0011").unwrap().status, TaskStatus::Error);

        // 空 checkpoint 场景：用户清空全部任务后退出，不应重复导入旧会话
        let mut empty_repo = TaskRepository::new();
        save_checkpoint(&dir, &empty_repo).expect("保存空 checkpoint 应成功");
        restore_session(&dir, &mut empty_repo);
        assert!(
            empty_repo.all().is_empty(),
            "空 checkpoint 不应重新导入旧会话"
        );

        cleanup(&dir);
    }

    // ------------------------------------------------------------------
    // 测试 4：load_checkpoint 对缺失 / 损坏文件不 panic，返回空
    // ------------------------------------------------------------------
    #[test]
    fn load_checkpoint_is_robust_to_missing_and_corrupt_files() {
        let dir = temp_dir();
        // 文件不存在：返回空
        assert!(load_checkpoint(&dir).is_empty());
        // 文件内容损坏：返回空且不 panic
        fs::write(dir.join("checkpoint.json"), "not-a-json").unwrap();
        assert!(load_checkpoint(&dir).is_empty());
        cleanup(&dir);
    }
}
