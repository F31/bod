//! P3 2.1—2.5 的漂移检查：需求覆盖、用例名、硬口径。

use std::collections::BTreeSet;

const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

#[test]
fn tasks_2_1_to_2_5_are_checked_while_2_6_waits_for_the_real_sample() {
    let mut checked = BTreeSet::new();
    let mut unchecked = BTreeSet::new();
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("- [") else {
            continue;
        };
        let is_checked = rest.starts_with('x');
        let body = rest.get(3..).unwrap_or_default().trim();
        let Some(id) = body.split_whitespace().next() else {
            continue;
        };
        if is_checked {
            checked.insert(id.to_string());
        } else {
            unchecked.insert(id.to_string());
        }
    }
    for id in ["2.1", "2.2", "2.3", "2.4", "2.5"] {
        assert!(checked.contains(id), "任务 {id} 应已勾选");
    }
    assert!(
        unchecked.contains("2.6"),
        "2.6 需要真实接口变化样本跑 G-31，未取得前不得勾选"
    );
}
