//! P6 1.1—1.6 的漂移检查。

use std::collections::BTreeSet;

const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p6-open-extension/tasks.md");

#[test]
fn module_is_registered_in_the_app_layer() {
    assert!(
        LIB.contains("pub mod extension_sdk;"),
        "extension_sdk 未在 lib.rs 注册"
    );
}

#[test]
fn tasks_1_1_to_1_6_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["1.1", "1.2", "1.3", "1.4", "1.5", "1.6"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("1."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "1.1—1.6 勾选状态应与已落地范围一致");
}
