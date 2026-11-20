//! P3 3.1—3.6 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str = include_str!(
    "../../../openspec/changes/p3-change-collaboration/specs/multi-agent-workspace/spec.md"
);
const NOTE: &str = include_str!("../../../docs/spec-notes/p3-merge.md");
const MODULE: &str = include_str!("../src/merge_queue.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 5, "multi-agent-workspace delta 应有 5 条需求");
    for title in titles {
        assert!(
            NOTE.contains(title.trim()),
            "spec note 未覆盖需求 `{}`",
            title.trim()
        );
    }
}

#[test]
fn every_named_case_in_the_note_is_a_real_test() {
    let section = NOTE
        .split("## 5. 命名用例")
        .nth(1)
        .expect("note 必须有命名用例小节")
        .split("\n## ")
        .next()
        .unwrap_or_default();
    let names: Vec<String> = section
        .lines()
        .filter_map(|line| {
            let cell = line.trim_start().strip_prefix("| `")?;
            let end = cell.find('`')?;
            Some(cell[..end].to_string())
        })
        .filter(|item| {
            item.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
        .collect();
    assert_eq!(names.len(), 7, "命名用例表应列 7 条，实际 {}", names.len());
    for name in names {
        assert!(
            MODULE.contains(&format!("fn {name}(")),
            "note 列的用例 `{name}` 在模块里不存在"
        );
    }
}

#[test]
fn note_states_the_three_gates_and_the_isolation_downgrade() {
    for needle in ["TargetMoved", "EvidenceMismatch", "NotAtHead"] {
        assert!(NOTE.contains(needle), "note 必须写明门禁错误 `{needle}`");
    }
    assert!(
        NOTE.contains("Collaborative") && NOTE.contains("G-29"),
        "note 必须写明 worktree 只能算协作隔离（G-29）"
    );
    assert!(
        NOTE.contains("每个 Attempt"),
        "note 必须写明每 Attempt 独立 worktree"
    );
}

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
