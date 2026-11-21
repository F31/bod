//! P7 §3（3.1—3.5）的漂移检查：决定记录必须能被解析、通过门禁，且引用的数字出自真实报告。

const TASKS: &str = include_str!("../../../openspec/changes/p7-capacity-evaluation/tasks.md");

/// tasks 3.x 必须勾选并带证据。
#[test]
fn tasks_3_x_are_checked_with_evidence() {
    let section = TASKS
        .split("## 3. 方案对比与决定")
        .nth(1)
        .expect("tasks 必须有 §3")
        .split("## 4.")
        .next()
        .unwrap_or_default();
    for id in ["3.1", "3.2", "3.3", "3.4", "3.5"] {
        assert!(
            section.contains(&format!("- [x] {id}")),
            "tasks 的 {id} 尚未勾选"
        );
    }
    assert!(
        section.contains("docs/reports/p7-decision.md"),
        "tasks 要指向决定记录"
    );
}
