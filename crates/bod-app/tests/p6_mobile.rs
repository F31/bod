//! P6 4.1—4.5 的漂移检查（4.6 真机验收不在源码证据范围）。

use std::collections::BTreeSet;

const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p6-open-extension/tasks.md");

#[test]
fn module_is_registered_in_the_app_layer() {
    assert!(
        LIB.contains("pub mod mobile_control;"),
        "mobile_control 未在 lib.rs 注册"
    );
}

#[test]
fn tasks_4_1_to_4_5_are_checked_while_4_6_stays_open() {
    let mut checked = BTreeSet::new();
    let mut unchecked = BTreeSet::new();
    for line in TASKS.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- [") else {
            continue;
        };
        let body = rest.get(3..).unwrap_or_default().trim();
        let Some(id) = body.split_whitespace().next() else {
            continue;
        };
        if !id.starts_with("4.") {
            continue;
        }
        if rest.starts_with('x') {
            checked.insert(id.to_string());
        } else {
            unchecked.insert(id.to_string());
        }
    }
    let expected: BTreeSet<String> = ["4.1", "4.2", "4.3", "4.4", "4.5"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(checked, expected, "4.1—4.5 应全部勾选");
    assert_eq!(
        unchecked,
        ["4.6".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "4.6 需要真机，必须保持未勾选"
    );
}
