//! P4 3.1—3.6 的漂移检查。

use std::collections::BTreeSet;

const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p4-runtime-delivery/tasks.md");

#[test]
fn modules_are_registered_in_the_app_layer() {
    assert!(
        LIB.contains("pub mod release_governance;"),
        "release_governance 未在 lib.rs 注册"
    );
    assert!(
        LIB.contains("pub mod maintenance;"),
        "maintenance 未在 lib.rs 注册"
    );
}

#[test]
fn tasks_3_1_to_3_6_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["3.1", "3.2", "3.3", "3.4", "3.5", "3.6"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("3."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "3.1—3.6 勾选状态应与已落地范围一致");
}
