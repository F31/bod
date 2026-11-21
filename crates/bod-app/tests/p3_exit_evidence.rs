//! P3 5.3 的漂移检查：出口报告、支持矩阵与任务勾选三处不能各说各话。

use std::collections::BTreeSet;

const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

fn task_state(id: &str) -> Option<bool> {
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let (checked, rest) = if let Some(rest) = trimmed.strip_prefix("- [x] ") {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix("- [ ] ") {
            (false, rest)
        } else {
            continue;
        };
        if rest.split_whitespace().next() == Some(id) {
            return Some(checked);
        }
    }
    None
}

#[test]
fn open_tasks_stay_unchecked_while_the_sample_is_absent() {
    assert_eq!(task_state("2.6"), Some(false), "2.6 需真实样本，不得勾选");
    assert_eq!(task_state("5.2"), Some(false), "5.2 需真实演示，不得勾选");
    for id in ["1.1", "1.6", "2.5", "3.6", "4.5", "5.1", "5.3"] {
        assert_eq!(task_state(id), Some(true), "任务 {id} 应已勾选");
    }
}

#[test]
fn every_cited_test_target_exists_on_disk() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for target in ["p3_openspec", "p3_impact", "p3_merge", "p3_plan"] {
        let path = root.join("tests").join(format!("{target}.rs"));
        assert!(path.is_file(), "报告引用的测试目标 {target} 不存在");
    }
    for module in [
        "openspec_integration",
        "impact_revalidation",
        "merge_queue",
        "collaboration_plan",
        "coding_integration",
        "workspace_safety",
    ] {
        let path = root.join("src").join(format!("{module}.rs"));
        assert!(path.is_file(), "报告引用的模块 {module} 不存在");
    }
}

#[test]
fn checked_task_ids_are_unique() {
    let mut seen = BTreeSet::new();
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("- [x] ")
            .or_else(|| trimmed.strip_prefix("- [ ] "))
        else {
            continue;
        };
        let Some(id) = rest.split_whitespace().next() else {
            continue;
        };
        if !id.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            continue;
        }
        assert!(seen.insert(id.to_string()), "任务编号 {id} 重复");
    }
}
