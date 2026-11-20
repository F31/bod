//! P4 3.1—3.6 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p4-runtime-delivery/specs/release-governance/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p4-release.md");
const MODULE: &str = include_str!("../src/release_governance.rs");
const MAINTENANCE: &str = include_str!("../src/maintenance.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p4-runtime-delivery/tasks.md");

fn note_section(heading: &str) -> &'static str {
    NOTE.split(heading)
        .nth(1)
        .unwrap_or_else(|| panic!("note 必须有 `{heading}` 小节"))
        .split("\n## ")
        .next()
        .unwrap_or_default()
}

fn first_column_ids(section: &str) -> Vec<String> {
    section
        .lines()
        .filter_map(|line| {
            let cell = line.trim_start().strip_prefix("| `")?;
            let end = cell.find('`')?;
            Some(cell[..end].to_string())
        })
        .filter(|item| {
            item.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
        })
        .collect()
}

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
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 6, "release-governance delta 应有 6 条需求");
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
    let names = first_column_ids(note_section("## 7. 命名用例"));
    assert_eq!(
        names.len(),
        20,
        "命名用例表应列 20 条，实际 {}",
        names.len()
    );
    for name in names {
        let exists =
            MODULE.contains(&format!("fn {name}(")) || MAINTENANCE.contains(&format!("fn {name}("));
        assert!(exists, "note 列的用例 `{name}` 在模块里不存在");
    }
}

#[test]
fn note_states_the_post_commit_and_no_blind_resend_rules() {
    assert!(
        NOTE.contains("提交后派发"),
        "note 必须写明外部动作在提交后派发"
    );
    assert!(NOTE.contains("不盲重发"), "note 必须写明 Unknown 不盲重发");
    assert!(
        NOTE.contains("可用额度 = 限额 − 已结算(C) − 有效预留(R) − 未知风险占用(U)"),
        "note 必须写明预算解释的算式"
    );
    assert!(
        NOTE.contains("维护不能自己改生产"),
        "note 必须写明维护不自动升级/发布"
    );
    assert!(
        MODULE.contains("期望版本当成已部署"),
        "模块文档必须点明不得把期望当事实"
    );
    assert!(
        MAINTENANCE.contains("G-35"),
        "maintenance 模块必须点名 G-35"
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
