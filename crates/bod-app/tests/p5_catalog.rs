//! P5 1.1—1.6 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p5-team-reuse/specs/team-catalog/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p5-catalog.md");
const MODULE: &str = include_str!("../src/catalog_upgrade.rs");
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
        LIB.contains("pub mod catalog_upgrade;"),
        "catalog_upgrade 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 5, "team-catalog delta 应有 5 条需求");
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
        14,
        "命名用例表应列 14 条，实际 {}",
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
fn note_states_the_install_and_upgrade_rules() {
    for needle in [
        "联网下载不占数据库写事务",
        "候选资源正式启用前不执行",
        "不含展示名",
        "没有任何一格是\"静默覆盖本地\"",
        "恒 `false`",
        "结构上不读",
    ] {
        assert!(NOTE.contains(needle), "note 必须写明 `{needle}`");
    }
    assert!(
        MODULE.contains("安装包不携带真实密钥"),
        "模块文档必须声明安装包不携带真实密钥"
    );
    assert!(
        MODULE.contains("不联网、不读写包文件、不落 DB"),
        "模块文档必须声明边界"
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
