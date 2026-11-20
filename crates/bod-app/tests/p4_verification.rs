//! P4 1.1—1.6 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str = include_str!(
    "../../../openspec/changes/p4-runtime-delivery/specs/runtime-verification/spec.md"
);
const NOTE: &str = include_str!("../../../docs/spec-notes/p4-verification.md");
const MODULE: &str = include_str!("../src/runtime_verification.rs");
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

/// 从表格第一列取反引号里的标识符。
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
        LIB.contains("pub mod runtime_verification;"),
        "runtime_verification 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 6, "runtime-verification delta 应有 6 条需求");
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
        17,
        "命名用例表应列 17 条，实际 {}",
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
fn template_fields_exist_in_the_module() {
    let fields = first_column_ids(note_section("## 8. 证据模板与字段追溯"));
    assert_eq!(
        fields.len(),
        25,
        "模板字段表应列 25 项，实际 {}",
        fields.len()
    );
    for field in fields {
        assert!(
            MODULE.contains(&format!("pub {field}:")),
            "模板字段 `{field}` 在模块里没有对应定义"
        );
    }
}

#[test]
fn note_states_the_no_default_pass_rule_and_the_rebuild_rule() {
    assert!(
        NOTE.contains("未运行不是 Pass"),
        "note 必须写明未运行不默认 Pass"
    );
    assert!(
        NOTE.contains("`NotApplicable` 需要豁免"),
        "note 必须写明 NotApplicable 需要豁免"
    );
    assert!(
        NOTE.contains("平台变了 → `RebuildRequired{PlatformDiffers}`"),
        "note 必须写明平台变化触发重建"
    );
    assert!(
        MODULE.contains("分支绿灯不算证据"),
        "模块文档必须点明分支绿灯不算证据"
    );
}

#[test]
fn tasks_1_1_to_1_6_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["1.1", "1.2", "1.3", "1.4", "1.5", "1.6"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("1."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "1.1—1.6 勾选状态应与已落地范围一致");
}
