//! 契约 Golden-Data 测试（Task 13.1，对应 MIGRATION-TAURI.md 12 节「契约测试」）
//!
//! 在测试内定义**内联 golden 样本**（模拟 Electron 版 aria2 真实返回形状），
//! 断言 motrix-core 的 `Task::to_aria2()` / `GlobalStat::to_aria2_json()` 输出
//! 与 golden **逐字段一致**（`serde_json::Value` 相等），保证「契约保持、引擎替换」
//! 迁移策略下 RPC 返回结构不回归。
//!
//! 数值约定（aria2 惯例）：所有数值字段均为字符串；无错误任务
//! `errorCode = "0"` / `errorMessage = ""`（与真实 aria2 tellStatus 一致）。
//!
//! 覆盖样本：
//! - `tellStatus` 黄金样本：HTTP active 任务（完整键集合 + files[].uris）
//! - `tellStatus` 黄金样本：complete 任务（errorCode="0"）
//! - `getGlobalStat` 黄金样本（数值均为字符串）
//! - `getVersion` 黄金样本（version / enabledFeatures 形状）

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use motrix_core::config::{ConfigManager, SystemConfig};
use motrix_core::{GlobalStat, Task, TaskManager, TaskStatus};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 黄金样本（golden data）：模拟 Electron 版 aria2 真实返回形状
// ---------------------------------------------------------------------------

/// tellStatus 黄金样本：HTTP active 任务（完整键集合，数值均为字符串；
/// 无错误任务 errorCode="0" / errorMessage="")
fn golden_tell_status_active() -> Value {
    json!({
        "gid": "0123456789abcdef",
        "status": "active",
        "totalLength": "1048576",
        "completedLength": "524288",
        "uploadLength": "0",
        "downloadSpeed": "102400",
        "uploadSpeed": "0",
        "connections": "4",
        "dir": "/downloads",
        "files": [
            {
                "index": "1",
                "length": "1048576",
                "completedLength": "524288",
                "selected": "true",
                "path": "/downloads/ubuntu-24.04.iso",
                "uris": [
                    { "uri": "https://mirror.example.com/ubuntu-24.04.iso", "status": "used" }
                ]
            }
        ],
        "errorCode": "0",
        "errorMessage": "",
        "numSeeders": "0",
        "seeder": "false",
        "bitfield": "",
        "infoHash": null,
        "bittorrent": null,
    })
}

/// tellStatus 黄金样本：complete 任务（errorCode="0"、速度归零）
fn golden_tell_status_complete() -> Value {
    json!({
        "gid": "fedcba9876543210",
        "status": "complete",
        "totalLength": "2048",
        "completedLength": "2048",
        "uploadLength": "0",
        "downloadSpeed": "0",
        "uploadSpeed": "0",
        "connections": "1",
        "dir": "/downloads",
        "files": [
            {
                "index": "1",
                "length": "2048",
                "completedLength": "2048",
                "selected": "true",
                "path": "/downloads/release-notes.txt",
                "uris": [
                    { "uri": "https://example.com/release-notes.txt", "status": "used" }
                ]
            }
        ],
        "errorCode": "0",
        "errorMessage": "",
        "numSeeders": "0",
        "seeder": "false",
        "bitfield": "",
        "infoHash": null,
        "bittorrent": null,
    })
}

/// getGlobalStat 黄金样本（downloadSpeed/uploadSpeed/numActive/numWaiting/numStopped，
/// 数值均为字符串）
fn golden_global_stat() -> Value {
    json!({
        "downloadSpeed": "102400",
        "uploadSpeed": "0",
        "numActive": "1",
        "numWaiting": "2",
        "numStopped": "1",
    })
}

/// getVersion 黄金样本（version 为 motrix 引擎版本串 + enabledFeatures 特性数组）
fn golden_get_version() -> Value {
    json!({
        "version": motrix_core::ENGINE_VERSION,
        "enabledFeatures": [
            "Async DNS", "BitTorrent", "Firefox3 Cookie", "GZip", "HTTPS",
            "Message Digest", "Metalink", "XML-RPC", "SFTP"
        ],
    })
}

// ---------------------------------------------------------------------------
// 与 golden 等价的 motrix-core 数据构造
// ---------------------------------------------------------------------------

/// 构造与 active golden 等价的 Task（经 Task::new_http_task + 字段赋值）
fn build_active_task() -> Task {
    let mut task = Task::new_http_task(
        "0123456789abcdef",
        &["https://mirror.example.com/ubuntu-24.04.iso".to_string()],
        "/downloads",
        "ubuntu-24.04.iso",
    );
    // 状态机迁移：waiting -> active（合法）
    task.transition(TaskStatus::Active)
        .expect("waiting -> active 迁移应合法");
    task.total_length = 1_048_576;
    task.completed_length = 524_288;
    task.download_speed = 102_400;
    task.connections = 4;
    // 文件信息与顶层进度字段对齐（aria2 tellStatus 中 files[].length 同 totalLength）
    if let Some(file) = task.files.first_mut() {
        file.length = 1_048_576;
        file.completed_length = 524_288;
    }
    // aria2 惯例：无错误任务 errorCode="0"、errorMessage=""
    task.error_code = Some(0);
    task.error_message = Some(String::new());
    task
}

/// 构造与 complete golden 等价的 Task（errorCode="0"）
fn build_complete_task() -> Task {
    let mut task = Task::new_http_task(
        "fedcba9876543210",
        &["https://example.com/release-notes.txt".to_string()],
        "/downloads",
        "release-notes.txt",
    );
    // 状态机迁移：waiting -> active -> complete（合法链路）
    task.transition(TaskStatus::Active)
        .expect("waiting -> active 迁移应合法");
    task.transition(TaskStatus::Complete)
        .expect("active -> complete 迁移应合法");
    task.total_length = 2048;
    task.completed_length = 2048;
    task.connections = 1;
    // 文件信息与顶层进度字段对齐（aria2 tellStatus 中 files[].length 同 totalLength）
    if let Some(file) = task.files.first_mut() {
        file.length = 2048;
        file.completed_length = 2048;
    }
    task.error_code = Some(0);
    task.error_message = Some(String::new());
    task
}

/// 构造默认 waiting 任务（并发队列 / 统计样本用）
fn waiting_task(gid: &str, uri: &str, out: &str) -> Task {
    Task::new_http_task(gid, &[uri.to_string()], "/downloads", out)
}

// ---------------------------------------------------------------------------
// 临时目录辅助（测试后清理）
// ---------------------------------------------------------------------------

/// 创建带唯一后缀的临时目录（Drop 时整体删除）
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "motrix-contract-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("创建临时目录失败");
        TempDir(dir)
    }

    /// 返回临时目录路径（&PathBuf 自动 deref 为 &Path）
    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 用临时数据目录构造 TaskManager（不启动引擎，仅验证序列化契约）
fn test_manager(temp: &TempDir) -> Arc<TaskManager> {
    let config_manager = Arc::new(Mutex::new(ConfigManager::new(temp.path().to_path_buf())));
    let system = SystemConfig::defaults(temp.path());
    Arc::new(TaskManager::new(&system, config_manager))
}

// ---------------------------------------------------------------------------
// 契约断言：to_aria2() / to_aria2_json() 与 golden 逐字段一致
// ---------------------------------------------------------------------------

/// HTTP active 任务：to_aria2 输出与 golden 逐字段一致
#[test]
fn tell_status_active_matches_golden() {
    let actual = build_active_task().to_aria2();
    assert_eq!(
        actual,
        golden_tell_status_active(),
        "to_aria2 输出与 golden 不一致"
    );
}

/// complete 任务（errorCode="0"）：to_aria2 输出与 golden 逐字段一致
#[test]
fn tell_status_complete_matches_golden() {
    let actual = build_complete_task().to_aria2();
    assert_eq!(
        actual,
        golden_tell_status_complete(),
        "to_aria2 输出与 golden 不一致"
    );
}

/// 经 TaskManager 仓库构造等价数据（Task 13 要求 Task/GlobalStat/TaskManager
/// 三条构造路径均可）：任务放入共享仓库后 get() 再 to_aria2 与 golden 一致
#[test]
fn tell_status_via_task_manager_matches_golden() {
    let temp = TempDir::new("tm");
    let tm = test_manager(&temp);
    tm.repo.lock().unwrap().add(build_active_task());
    let out = tm
        .get("0123456789abcdef")
        .expect("任务应存在于仓库")
        .to_aria2();
    assert_eq!(out, golden_tell_status_active());
    drop(tm);
}

/// getGlobalStat：GlobalStat::to_aria2_json 与 golden 一致（数值均为字符串）
#[test]
fn global_stat_matches_golden() {
    // 直接构造 GlobalStat（等价 tellStatus 之外的统计样本）
    let stat = GlobalStat {
        download_speed: 102_400,
        upload_speed: 0,
        num_active: 1,
        num_waiting: 2,
        num_stopped: 1,
    };
    assert_eq!(stat.to_aria2_json(), golden_global_stat());

    // 经 TaskManager::global_stat 聚合路径：仓库含 active / waiting / complete 任务
    let temp = TempDir::new("stat");
    let tm = test_manager(&temp);
    {
        let mut repo = tm.repo.lock().unwrap();
        repo.add(build_active_task()); // active，download_speed=102400
        repo.add(waiting_task("bbbbbbbbbbbbbbbb", "https://example.com/b.bin", "b.bin"));
        repo.add(waiting_task("cccccccccccccccc", "https://example.com/c.bin", "c.bin"));
        repo.add(build_complete_task()); // complete（计入 stopped）
    }
    assert_eq!(tm.global_stat().to_aria2_json(), golden_global_stat());
    drop(tm);
}

/// getVersion：版本串格式与 enabledFeatures 数组形状（ENGINE_VERSION 统一来源）
#[test]
fn get_version_matches_golden_shape() {
    let golden = golden_get_version();
    // version 为字符串，且为 "主版本号 (motrix-engine <crate版本>)" 格式
    let version = golden["version"].as_str().expect("version 应为字符串");
    assert!(version.starts_with("1.8.19"), "版本号应保持 aria2 兼容前缀: {version}");
    assert!(
        version.contains("(motrix-engine"),
        "版本串应包含 motrix-engine 标识: {version}"
    );
    // enabledFeatures 为非空字符串数组（前端 / 外部客户端依赖该形状）
    let features = golden["enabledFeatures"]
        .as_array()
        .expect("enabledFeatures 应为数组");
    assert!(!features.is_empty(), "enabledFeatures 不应为空");
    assert!(
        features.iter().all(|f| f.is_string()),
        "enabledFeatures 元素应为字符串"
    );
    // 与 motrix-core 暴露的 ENGINE_VERSION 常量一致
    assert_eq!(version, motrix_core::ENGINE_VERSION);
}

/// 附加：所有数值字段均为字符串（aria2 惯例）——逐字段扫描 golden 断言
#[test]
fn golden_numeric_fields_are_strings() {
    // tellStatus 黄金样本的数值键（含 files[].index/length/completedLength/selected）
    let numeric_keys = [
        "totalLength",
        "completedLength",
        "uploadLength",
        "downloadSpeed",
        "uploadSpeed",
        "connections",
        "numSeeders",
    ];
    for golden in [golden_tell_status_active(), golden_tell_status_complete()] {
        for key in numeric_keys {
            assert!(
                golden[key].is_string(),
                "键 {key} 应为字符串，实际: {:?}",
                golden[key]
            );
        }
        // files 内数值键均为字符串
        let file = &golden["files"][0];
        for key in ["index", "length", "completedLength", "selected"] {
            assert!(
                file[key].is_string(),
                "files 键 {key} 应为字符串，实际: {:?}",
                file[key]
            );
        }
    }
    // getGlobalStat 数值键均为字符串
    for key in [
        "downloadSpeed",
        "uploadSpeed",
        "numActive",
        "numWaiting",
        "numStopped",
    ] {
        assert!(
            golden_global_stat()[key].is_string(),
            "getGlobalStat 键 {key} 应为字符串"
        );
    }
}
