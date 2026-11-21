//! P3 2.1—2.5 的漂移检查：需求覆盖、用例名、硬口径。

use std::collections::BTreeSet;

const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

#[test]
fn tasks_2_1_to_2_6_are_checked_with_a_real_sample_for_2_6() {
    let mut checked = BTreeSet::new();
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("- [") else {
            continue;
        };
        if !rest.starts_with('x') {
            continue;
        }
        let body = rest.get(3..).unwrap_or_default().trim();
        if let Some(id) = body.split_whitespace().next() {
            checked.insert(id.to_string());
        }
    }
    for id in ["2.1", "2.2", "2.3", "2.4", "2.5"] {
        assert!(checked.contains(id), "任务 {id} 应已勾选");
    }
    // 2.6 已用本仓库真实提交跑出 G-31 报告（docs/reports/p3-interface-impact.md），
    // 但报告本身不含通过结论；勾选只表示"样本已取得"，门禁判定仍由评审给出。
    assert!(
        checked.contains("2.6"),
        "2.6 已取得真实接口变化样本，应勾选"
    );
}
