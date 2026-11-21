//! P6 3.1—3.5 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p6-open-extension/specs/readonly-adapter/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p6-readonly.md");
const MODULE: &str = include_str!("../src/readonly_api.rs");
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
        LIB.contains("pub mod readonly_api;"),
        "readonly_api 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 5, "readonly-adapter delta 应有 5 条需求");
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
    assert_eq!(names.len(), 8, "命名用例表应列 8 条，实际 {}", names.len());
    for name in names {
        assert!(
            MODULE.contains(&format!("    fn {name}(")),
            "note 列的用例 `{name}` 在模块里不存在"
        );
    }
}

#[test]
fn note_states_the_readonly_boundaries() {
    for needle in [
        "**按钮提示不是授权。**",
        "**等于 `visible.len()`**",
        "不可见与不存在给同一个",
        "**先鉴权再补拉**",
        "**永不可淘汰**",
        "旧结果不会因为\"还没重算完\"而继续被当成当前结果展示",
        "审计不保存令牌或业务密文明文",
    ] {
        assert!(NOTE.contains(needle), "note 必须写明 `{needle}`");
    }
    assert!(
        MODULE.contains("纯值判断：不建连接、不读外部索引、不落表、不启动任何执行体"),
        "模块文档必须声明边界"
    );
}

#[test]
fn note_admits_what_is_not_yet_evidenced() {
    for needle in [
        "没有真实跨端查询",
        "G-34 绑定保持 planned",
        "`ProjectionCache` 是内存 map，进程重启即空",
    ] {
        assert!(NOTE.contains(needle), "note 必须如实写明 `{needle}`");
    }
    assert!(
        !NOTE.contains("已完成真实跨端查询验证"),
        "不得声称已完成真实跨端验证"
    );
}

#[test]
fn tasks_3_1_to_3_5_are_checked_only_with_the_note_in_place() {
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
    let expected: BTreeSet<String> = ["3.1", "3.2", "3.3", "3.4", "3.5"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let actual: BTreeSet<String> = checked
        .iter()
        .filter(|id| id.starts_with("3."))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "3.1—3.5 勾选状态应与已落地范围一致");
}
