//! P5 2.1—2.7 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p5-team-reuse/specs/team-permissions/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p5-permissions.md");
const MODULE: &str = include_str!("../src/authorization.rs");
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
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 6, "team-permissions delta 应有 6 条需求");
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
fn note_keeps_the_no_derivation_and_no_side_effect_rules() {
    for needle in [
        "跨段不派生",
        "只等于可见条数",
        "被拦时调用数为零",
        "历史归属不改写",
        "没有任何值字段",
    ] {
        assert!(NOTE.contains(needle), "note 必须写明 `{needle}`");
    }
    assert!(
        MODULE.contains("凭据边界（2.3）"),
        "模块文档必须声明凭据边界"
    );
}

#[test]
fn tasks_2_1_to_2_7_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["2.1", "2.2", "2.3", "2.4", "2.5", "2.6", "2.7"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("2."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "2.1—2.7 勾选状态应与已落地范围一致");
}
