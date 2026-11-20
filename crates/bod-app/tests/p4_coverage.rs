//! P4 4.1—4.6 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p4-runtime-delivery/specs/coverage-receipt/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p4-coverage.md");
const MODULE: &str = include_str!("../src/coverage_receipt.rs");
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
fn module_is_registered_in_the_app_layer() {
    assert!(
        LIB.contains("pub mod coverage_receipt;"),
        "coverage_receipt 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 6, "coverage-receipt delta 应有 6 条需求");
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
fn note_states_the_fixed_reply_and_isolation_rules() {
    assert!(
        NOTE.contains("修复版本已提供，客户环境待核实"),
        "note 必须写明只有制品时的答复原文"
    );
    assert!(
        NOTE.contains("不给\"还有几条别人的\""),
        "note 必须写明计数不泄露他人"
    );
    assert!(
        NOTE.contains("只写 `seen_at`，不动响应状态"),
        "note 必须写明访问不计确认"
    );
    assert!(
        NOTE.contains("历史确认保留原版本"),
        "note 必须写明回滚更正保留历史版本"
    );
    assert!(
        MODULE.contains("不发送任何通知"),
        "模块文档必须声明不发通知、不存库"
    );
}

#[test]
fn tasks_4_1_to_4_6_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["4.1", "4.2", "4.3", "4.4", "4.5", "4.6"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("4."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "4.1—4.6 勾选状态应与已落地范围一致");
}
