//! P6 1.1—1.6 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p6-open-extension/specs/sdk-extension/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p6-sdk.md");
const MODULE: &str = include_str!("../src/extension_sdk.rs");
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
        LIB.contains("pub mod extension_sdk;"),
        "extension_sdk 未在 lib.rs 注册"
    );
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 5, "sdk-extension delta 应有 5 条需求");
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
        11,
        "命名用例表应列 11 条，实际 {}",
        names.len()
    );
    for name in names {
        assert!(
            MODULE.contains(&format!("fn {name}(")),
            "note 列的用例 `{name}` 在模块里不存在"
        );
        assert!(
            MODULE.contains(&format!("    fn {name}(")),
            "`{name}` 必须是模块内用例，不是文档里的字样"
        );
    }
}

#[test]
fn note_states_the_extension_boundaries() {
    for needle in [
        "拒绝只作用于新扩展任务",
        "只有 `Supported` 的 `in_scheduling_pool()`",
        "**不看**自报的 `Degraded`",
        "安装时缺 fixture 即",
        "接收端零次收到材料",
        "不会被读成契约证据",
        "安装包本身不携带真实密钥",
    ] {
        assert!(NOTE.contains(needle), "note 必须写明 `{needle}`");
    }
    assert!(
        MODULE.contains("不下载、不执行外部程序、不落 DB"),
        "模块文档必须声明边界"
    );
}

#[test]
fn note_admits_what_is_not_yet_evidenced() {
    for needle in [
        "没有真实第三方适配器接入过",
        "不代表支持矩阵已升 Supported",
        "`RealTask` 需要真实项目跑过才有",
    ] {
        assert!(NOTE.contains(needle), "note 必须如实写明 `{needle}`");
    }
    assert!(
        !NOTE.contains("已接入真实第三方适配器"),
        "不得声称已接入真实第三方"
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
