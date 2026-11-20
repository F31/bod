//! P4 2.1—2.6 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p4-runtime-delivery/specs/stage-manifest/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p4-evidence.md");
const MODULE: &str = include_str!("../src/stage_manifest.rs");
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
        LIB.contains("pub mod stage_manifest;"),
        "stage_manifest 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 6, "stage-manifest delta 应有 6 条需求");
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
        16,
        "命名用例表应列 16 条，实际 {}",
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
fn note_states_the_disclosure_and_identity_rules() {
    assert!(
        NOTE.contains("凭据在任何视图都不出现"),
        "note 必须写明凭据不出现在任何视图"
    );
    assert!(
        NOTE.contains("客户视图只报被扣数量"),
        "note 必须写明客户视图只报计数"
    );
    assert!(
        NOTE.contains("自签包或嵌入公钥"),
        "note 必须写明自签/嵌入公钥不建立外部身份信任"
    );
    assert!(
        NOTE.contains("`Fixed` 只在「已确认投递 + 验收通过」出现"),
        "note 必须写明已修复结论的成立条件"
    );
    assert!(
        MODULE.contains("不读写文件系统"),
        "模块文档必须声明不碰文件系统与打包编码"
    );
}

#[test]
fn tasks_2_1_to_2_6_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["2.1", "2.2", "2.3", "2.4", "2.5", "2.6"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("2."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "2.1—2.6 勾选状态应与已落地范围一致");
}
