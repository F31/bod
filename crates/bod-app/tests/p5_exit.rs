//! P5 5.3 的漂移检查：出口报告、支持矩阵与任务勾选三处不能各说各话。

use std::collections::BTreeSet;

const TASKS: &str = include_str!("../../../openspec/changes/p5-team-reuse/tasks.md");

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
fn open_task_stays_unchecked_while_the_real_acceptance_is_absent() {
    assert_eq!(
        task_state("5.2"),
        Some(false),
        "5.2 需真实多主体授权与隔离验收，不得勾选"
    );
    for id in ["5.1", "5.3"] {
        assert_eq!(task_state(id), Some(true), "任务 {id} 应已勾选");
    }
    for id in ["1.1", "1.6", "2.1", "2.7", "3.1", "3.7", "4.1", "4.5"] {
        assert_eq!(task_state(id), Some(true), "任务 {id} 应已勾选");
    }
}

#[test]
fn every_cited_target_exists_on_disk() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for target in [
        "p5_catalog",
        "p5_permissions",
        "p5_knowledge",
        "p5_channel",
        "p5_gates",
    ] {
        let path = root.join("tests").join(format!("{target}.rs"));
        assert!(path.is_file(), "报告引用的测试目标 {target} 不存在");
    }
    for module in [
        "catalog_upgrade",
        "authorization",
        "feedback_knowledge",
        "notification_channel",
        "maintenance",
    ] {
        let path = root.join("src").join(format!("{module}.rs"));
        assert!(path.is_file(), "报告引用的模块 {module} 不存在");
    }
    for note in ["p5-catalog", "p5-permissions", "p5-knowledge", "p5-channel"] {
        let path = root
            .join("..")
            .join("..")
            .join("docs")
            .join("spec-notes")
            .join(format!("{note}.md"));
        assert!(path.is_file(), "报告引用的 spec note {note} 不存在");
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
