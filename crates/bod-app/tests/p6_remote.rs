//! P6 2.1—2.7 的漂移检查。

use std::collections::BTreeSet;

const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p6-open-extension/tasks.md");

#[test]
fn modules_are_registered_in_the_app_layer() {
    for needle in ["pub mod remote_delegation;", "pub mod remote_partition;"] {
        assert!(LIB.contains(needle), "lib.rs 缺少 `{needle}`");
    }
}

#[test]
fn tasks_2_1_to_2_7_are_checked_while_2_8_stays_open() {
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
        if !id.starts_with("2.") {
            continue;
        }
        if rest.starts_with('x') {
            checked.insert(id.to_string());
        } else {
            unchecked.insert(id.to_string());
        }
    }
    let expected: BTreeSet<String> = ["2.1", "2.2", "2.3", "2.4", "2.5", "2.6", "2.7"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(checked, expected, "2.1—2.7 应全部勾选");
    assert_eq!(
        unchecked,
        ["2.8".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "2.8 需要真实双机，必须保持未勾选"
    );
}
