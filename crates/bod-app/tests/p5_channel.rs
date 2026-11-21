//! P5 4.1—4.5 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p5-team-reuse/specs/notification-channel/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p5-channel.md");
const MODULE: &str = include_str!("../src/notification_channel.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p5-team-reuse/tasks.md");

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
        LIB.contains("pub mod notification_channel;"),
        "notification_channel 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 5, "notification-channel delta 应有 5 条需求");
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
    let names = first_column_ids(note_section("## 6. 命名用例"));
    assert_eq!(
        names.len(),
        12,
        "命名用例表应列 12 条，实际 {}",
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
fn note_keeps_the_receipt_and_boundary_rules() {
    for needle in [
        "渠道接受不等于已送达，更不等于已读",
        "不做乐观成功",
        "从不读 `message.text`",
        "标待核实，不假定没发出去",
        "覆盖变化、回执新修订与 outbox 意图同一次写入",
    ] {
        assert!(NOTE.contains(needle), "note 必须写明 `{needle}`");
    }
    assert!(
        MODULE.contains("它**不是**第二个业务系统"),
        "模块文档必须声明通道不是第二个业务系统"
    );
}

#[test]
fn tasks_4_1_to_4_5_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["4.1", "4.2", "4.3", "4.4", "4.5"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("4."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "4.1—4.5 勾选状态应与已落地范围一致");
}
