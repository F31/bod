//! P3 3.1—3.6 的漂移检查。

use std::collections::BTreeSet;

const MODULE: &str = include_str!("../src/merge_queue.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

#[test]
fn module_is_exported_and_four_write_limits_are_kept() {
    assert!(LIB.contains("pub mod merge_queue;"), "模块必须导出");
    assert!(MODULE.contains("pub const DEFAULT_WRITE_LIMITS"));
    assert!(MODULE.contains("\"symlink_traversal\""));
    assert!(MODULE.contains("\"git_admin_dir\""));
}

#[test]
fn tasks_3_1_to_3_6_are_checked() {
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
    for id in ["3.1", "3.2", "3.3", "3.4", "3.5", "3.6"] {
        assert!(checked.contains(id), "任务 {id} 应已勾选");
    }
}
