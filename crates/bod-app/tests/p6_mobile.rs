//! P6 4.1—4.5 的漂移检查（4.6 真机验收不在源码证据范围）。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p6-open-extension/specs/mobile-control/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p6-mobile.md");
const MODULE: &str = include_str!("../src/mobile_control.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p6-open-extension/tasks.md");

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
        LIB.contains("pub mod mobile_control;"),
        "mobile_control 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 6, "mobile-control delta 应有 6 条需求");
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
        10,
        "命名用例表应列 10 条，实际 {}",
        names.len()
    );
    for name in names {
        assert!(
            MODULE.contains(&format!("    fn {name}(")),
            "note 列的用例 `{name}` 在模块里不存在"
        );
    }
}

#[test]
fn note_states_the_mobile_boundaries() {
    for needle in [
        "**模拟器不算真机**",
        "**旧签名不能接受新成果**",
        "**恢复不等于生效**",
        "**打开链接只记访问**",
        "**只记一次决定**",
        "催办不自动执行",
    ] {
        assert!(NOTE.contains(needle), "note 必须写明 `{needle}`");
    }
    assert!(
        MODULE.contains("纯值判断：不起进程、不发推送、不挂载工作区、不落表"),
        "模块文档必须声明边界"
    );
}

#[test]
fn note_admits_that_real_device_evidence_is_absent() {
    for needle in ["**真机证据为零**", "4.6 未勾选", "不是两台真实设备的并发"] {
        assert!(NOTE.contains(needle), "note 必须如实写明 `{needle}`");
    }
    assert!(!NOTE.contains("已完成真机验收"), "不得声称已完成真机验收");
}

#[test]
fn tasks_4_1_to_4_5_are_checked_while_4_6_stays_open() {
    let mut checked = BTreeSet::new();
    let mut unchecked = BTreeSet::new();
    for line in TASKS.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- [") else {
            continue;
        };
        let body = rest.get(3..).unwrap_or_default().trim();
        let Some(id) = body.split_whitespace().next() else {
            continue;
        };
        if !id.starts_with("4.") {
            continue;
        }
        if rest.starts_with('x') {
            checked.insert(id.to_string());
        } else {
            unchecked.insert(id.to_string());
        }
    }
    let expected: BTreeSet<String> = ["4.1", "4.2", "4.3", "4.4", "4.5"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(checked, expected, "4.1—4.5 应全部勾选");
    assert_eq!(
        unchecked,
        ["4.6".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "4.6 需要真机，必须保持未勾选"
    );
}
