//! P3 4.1—4.5 的漂移检查。

use std::collections::BTreeSet;

const MODULE: &str = include_str!("../src/collaboration_plan.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

#[test]
fn module_is_exported_and_session_key_has_six_parts() {
    assert!(LIB.contains("pub mod collaboration_plan;"), "模块必须导出");
    for field in [
        "pub company: String",
        "pub project: String",
        "pub target: String",
        "pub work_kind: String",
        "pub execution_identity: String",
        "pub delegation_identity: String",
    ] {
        assert!(
            MODULE.contains(field),
            "SessionKey 缺分量 `{field}`：少一项就可能串会话"
        );
    }
}

#[test]
fn tasks_4_1_to_4_5_are_checked() {
    let mut checked = BTreeSet::new();
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("- [x] ") else {
            continue;
        };
        if let Some(id) = rest.split_whitespace().next() {
            checked.insert(id.to_string());
        }
    }
    for id in ["4.1", "4.2", "4.3", "4.4", "4.5"] {
        assert!(checked.contains(id), "任务 {id} 应已勾选");
    }
}
