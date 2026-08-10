//! 状态广播器（StateBroadcaster）骨架（Phase 1，Task 7）
//!
//! 职责：把任务仓库的状态节流合并为精简快照，供上层（src-tauri）经
//! `engine:snapshot` / `engine:global-stat` 事件推送给前端（参考
//! MIGRATION-TAURI.md 5.8 节）。本模块只提供**纯函数与配置**：
//!
//! - [`BroadcasterConfig`]：节流推送的间隔配置（默认基准 1s，自适应区间 0.5s~6s）
//! - [`effective_interval`]：按运行中任务数（numActive）计算自适应推送间隔，
//!   numActive 越大间隔越小（任务越多状态变化越频繁，推送越密）
//! - [`build_snapshot`]：从任务仓库构建 `{"globalStat": ..., "tasks": [...]}` 快照
//!
//! 本 crate 不依赖 Tauri，实际 emit 由 src-tauri 完成；模块保持纯函数以便
//! GUI 与未来的 motrix-cli daemon 模式共享同一套节流逻辑。

use std::time::Duration;

use serde_json::{json, Value};

use crate::task::TaskRepository;

/// 节流推送间隔配置
///
/// 字段语义与前端 `store/modules/app.js` 的常量保持一致：
/// - base_interval：默认（无活动任务时）推送间隔，默认 1s
/// - min_interval：自适应下限，默认 500ms（任务极多时不再更快）
/// - max_interval：自适应上限，默认 6s
#[derive(Debug, Clone, Copy)]
pub struct BroadcasterConfig {
    /// 基准间隔（num_active <= 0 时使用），默认 1s
    pub base_interval: Duration,
    /// 最小间隔，默认 500ms
    pub min_interval: Duration,
    /// 最大间隔，默认 6s
    pub max_interval: Duration,
}

impl Default for BroadcasterConfig {
    fn default() -> Self {
        Self {
            base_interval: Duration::from_millis(1000),
            min_interval: Duration::from_millis(500),
            max_interval: Duration::from_millis(6000),
        }
    }
}

impl BroadcasterConfig {
    /// 按运行中任务数（numActive）计算自适应推送间隔
    ///
    /// 规则（与前端 `UPDATE_INTERVAL` 的 clamp 逻辑一致）：
    /// - `num_active == 0`：使用基准间隔（默认 1s），避免空闲时高频空推
    /// - `num_active > 0`：每多 1 个活动任务间隔减小 100ms，
    ///   即 `base - 100ms * num_active`，并夹在 [min_interval, max_interval] 内
    /// - 结果恒落在 [500ms, 6s] 区间，且随 num_active 增大单调不增
    pub fn effective_interval(&self, num_active: u32) -> Duration {
        if num_active == 0 {
            return self.base_interval;
        }
        // 每多一个活动任务间隔减少 100ms（与前端 PER_INTERVAL=100 一致）
        let step = Duration::from_millis(100);
        let reduced = self
            .base_interval
            .checked_sub(step.saturating_mul(num_active))
            .unwrap_or(Duration::ZERO);
        // 夹在 [min, max] 区间（reduced 不可能超过 max，仅需保证不低于 min）
        reduced.clamp(self.min_interval, self.max_interval)
    }
}

/// 使用默认节流配置计算有效推送间隔（自由函数，方便 re-export 供 src-tauri 调用）
pub fn effective_interval(num_active: u32) -> Duration {
    BroadcasterConfig::default().effective_interval(num_active)
}

/// 从任务仓库构建状态快照：`{"globalStat": {...}, "tasks": [aria2 任务 JSON...]}`
///
/// - `globalStat`：调用 [`TaskRepository::global_stat`] 后转 aria2 兼容 JSON
///   （downloadSpeed/uploadSpeed/numActive/numWaiting/numStopped，数值均为字符串）
/// - `tasks`：仓库全部在册任务（active/waiting/paused/error/complete/seeding）的
///   `Task::to_aria2()` 全量字段 JSON（Phase 1 可接受全量；字段均为字符串，
///   前端 `Number()` 转换处不变）
pub fn build_snapshot(repo: &TaskRepository) -> Value {
    let global_stat = repo.global_stat().to_aria2_json();
    // 全量在册任务（不含 removed 历史；历史任务由 stopped 列表单独展示）
    let tasks: Vec<Value> = repo.all().iter().map(|task| task.to_aria2()).collect();
    json!({
        "globalStat": global_stat,
        "tasks": tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Task, TaskStatus};

    /// 便捷构造单 URL 测试任务（gid 用固定串便于断言）
    fn task_with_gid(gid: &str) -> Task {
        let urls = vec!["https://example.com/a.zip".to_string()];
        Task::new_http_task(gid.to_string(), &urls, "/downloads", "a.zip")
    }

    // ------------------------------------------------------------------
    // effective_interval：0/1/5/大量 active 均落在 [500ms, 6s] 且单调
    // ------------------------------------------------------------------
    #[test]
    fn effective_interval_falls_in_range() {
        // 0 个活动任务：使用基准间隔 1s（在 [500ms, 6s] 范围内）
        assert_eq!(effective_interval(0), Duration::from_millis(1000));
        // 1 个活动任务：每多一个任务间隔减 100ms
        assert_eq!(effective_interval(1), Duration::from_millis(900));
        // 5 个活动任务：到达最小间隔 500ms
        assert_eq!(effective_interval(5), Duration::from_millis(500));
        // 大量活动任务（如 100）：不低于最小间隔（clamp 生效）
        assert_eq!(effective_interval(100), Duration::from_millis(500));

        // 所有取值均落在 [500ms, 6s] 区间内
        for n in [0u32, 1, 2, 3, 5, 10, 100, u32::MAX] {
            let interval = effective_interval(n);
            assert!(
                interval >= Duration::from_millis(500),
                "num_active={n} 间隔低于 500ms: {interval:?}"
            );
            assert!(
                interval <= Duration::from_millis(6000),
                "num_active={n} 间隔高于 6s: {interval:?}"
            );
        }
    }

    #[test]
    fn effective_interval_is_monotonic() {
        // 单调不增：活动任务越多，推送间隔越小（或相等）
        let mut prev = Duration::from_millis(6000);
        for n in 0..=50u32 {
            let interval = effective_interval(n);
            assert!(
                interval <= prev,
                "间隔应随 num_active 单调不增: n={n}, prev={prev:?}, cur={interval:?}"
            );
            prev = interval;
        }
    }

    // ------------------------------------------------------------------
    // build_snapshot：顶层结构 + globalStat/tasks 字段
    // ------------------------------------------------------------------
    #[test]
    fn build_snapshot_shape() {
        let mut repo = TaskRepository::new();
        // active 任务（带进度与速度）
        let mut active = task_with_gid("aaaaaaaaaaaaaaaa");
        active.transition(TaskStatus::Active).unwrap();
        active.completed_length = 50;
        active.total_length = 100;
        active.download_speed = 512;
        repo.add(active);
        // waiting 任务（默认状态）
        repo.add(task_with_gid("bbbbbbbbbbbbbbbb"));

        let snapshot = build_snapshot(&repo);
        let obj = snapshot.as_object().expect("快照应为 JSON 对象");
        // 顶层结构：globalStat + tasks
        assert!(obj.contains_key("globalStat"), "缺少 globalStat 键");
        assert!(obj.contains_key("tasks"), "缺少 tasks 键");

        // globalStat：数值均为字符串（aria2 惯例）
        assert_eq!(snapshot["globalStat"]["numActive"], "1");
        assert_eq!(snapshot["globalStat"]["numWaiting"], "1");
        assert_eq!(snapshot["globalStat"]["downloadSpeed"], "512");
        assert_eq!(snapshot["globalStat"]["numStopped"], "0");

        // tasks：aria2 全量字段 JSON 数组，字段均为字符串
        let tasks = snapshot["tasks"].as_array().expect("tasks 应为数组");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["gid"], "aaaaaaaaaaaaaaaa");
        assert_eq!(tasks[0]["status"], "active");
        assert_eq!(tasks[0]["totalLength"], "100");
        assert_eq!(tasks[0]["completedLength"], "50");
        assert_eq!(tasks[0]["downloadSpeed"], "512");
        assert_eq!(tasks[1]["gid"], "bbbbbbbbbbbbbbbb");
        assert_eq!(tasks[1]["status"], "waiting");
        assert_eq!(tasks[1]["totalLength"], "0");
    }

    #[test]
    fn build_snapshot_empty_repo() {
        // 空仓库：globalStat 全 0、tasks 为空数组
        let repo = TaskRepository::new();
        let snapshot = build_snapshot(&repo);
        assert_eq!(snapshot["globalStat"]["numActive"], "0");
        assert_eq!(snapshot["tasks"].as_array().map(Vec::len), Some(0));
    }
}
