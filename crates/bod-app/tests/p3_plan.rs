//! P3 4.1—4.5 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str = include_str!(
    "../../../openspec/changes/p3-change-collaboration/specs/collaboration-planning/spec.md"
);
const NOTE: &str = include_str!("../../../docs/spec-notes/p3-plan.md");
const MODULE: &str = include_str!("../src/collaboration_plan.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(
        titles.len(),
        6,
        "collaboration-planning delta 应有 6 条需求"
    );
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
        .split("## 7. 命名用例")
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
    assert_eq!(
        names.len(),
        15,
        "命名用例表应列 15 条，实际 {}",
        names.len()
    );
    for name in names {
        assert!(
            MODULE.contains(&format!("fn {name}(")),
            "note 列的用例 `{name}` 在模块里不存在"
        );
    }
}

#[test]
fn note_keeps_the_no_side_effect_and_unknown_relation_rules() {
    assert!(
        NOTE.contains("不派发") && NOTE.contains("不改验收"),
        "note 必须写明答疑零副作用"
    );
    assert!(
        NOTE.contains("RelationState::Unknown"),
        "note 必须写明未确认关系不能进图"
    );
    assert!(
        NOTE.contains("在途 Run") && NOTE.contains("新开 Run"),
        "note 必须写明在途 Run 的保护路径"
    );
    assert!(
        NOTE.contains("global_per_agent"),
        "note 必须写明共享 Agent 的全局并发是另一道约束"
    );
}

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
