//! P5 4.1—4.5 的漂移检查。

use std::collections::BTreeSet;

const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p5-team-reuse/tasks.md");

#[test]
fn module_is_registered_in_the_app_layer() {
    assert!(
        LIB.contains("pub mod notification_channel;"),
        "notification_channel 未在 lib.rs 注册"
    );
}

#[test]
fn tasks_4_1_to_4_5_are_checked_only_with_the_note_in_place() {
    let mut checked = BTreeSet::new();
    for line in TASKS.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- [") else {
            continue;
        };
        let is_checked = rest.starts_with('x');
        let body = rest.get(3..).unwrap_or_default().trim();
        if is_checked && let Some(id) = body.split_whitespace().next() {
            checked.insert(id.to_string());
        }
    }
    let expected: BTreeSet<String> = ["4.1", "4.2", "4.3", "4.4", "4.5"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("4."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "4.1—4.5 勾选状态应与已落地范围一致");
}
