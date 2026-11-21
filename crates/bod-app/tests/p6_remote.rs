//! P6 2.1—2.7 的漂移检查。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p6-open-extension/specs/remote-execution/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p6-remote.md");
const DELEGATION: &str = include_str!("../src/remote_delegation.rs");
const PARTITION: &str = include_str!("../src/remote_partition.rs");
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
fn modules_are_registered_in_the_app_layer() {
    for needle in ["pub mod remote_delegation;", "pub mod remote_partition;"] {
        assert!(LIB.contains(needle), "lib.rs 缺少 `{needle}`");
    }
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles: Vec<&str> = DELTA
        .lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .collect();
    assert_eq!(titles.len(), 6, "remote-execution delta 应有 6 条需求");
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
        16,
        "命名用例表应列 16 条，实际 {}",
        names.len()
    );
    for name in names {
        let declared = format!("    fn {name}(");
        assert!(
            DELEGATION.contains(&declared) || PARTITION.contains(&declared),
            "note 列的用例 `{name}` 在两个模块里都不存在"
        );
    }
}

#[test]
fn note_states_the_remote_boundaries() {
    for needle in [
        "节点与 webhook **共用同一份**",
        "**验签通过不等于业务通过**",
        "断开是\"未知\"，不是\"失败\"",
        "**不产生接受**（无重复验收）",
        "业务键匹配而不是目标匹配",
        "只说明设备可识别",
        "整段释放缓冲",
        "没有权威回执就显示待核实",
        "同一 `decision_key` 至多一次有效业务决定",
    ] {
        assert!(NOTE.contains(needle), "note 必须写明 `{needle}`");
    }
    for module in [DELEGATION, PARTITION] {
        assert!(
            module.contains("纯值判断") && module.contains("不建连接") && module.contains("不落表"),
            "模块文档必须声明纯值判断与边界"
        );
    }
}

#[test]
fn note_admits_what_is_not_yet_evidenced() {
    for needle in [
        "没有真实双机环境",
        "2.8（§43.10 双机示例）未勾选",
        "G-19 绑定保持 planned",
        "不含 HTTP/SSE 实现与 QUIC 对照",
    ] {
        assert!(NOTE.contains(needle), "note 必须如实写明 `{needle}`");
    }
    assert!(!NOTE.contains("已通过双机演练"), "不得声称已通过双机演练");
}

#[test]
fn tasks_2_1_to_2_7_are_checked_while_2_8_stays_open() {
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
        if !id.starts_with("2.") {
            continue;
        }
        if rest.starts_with('x') {
            checked.insert(id.to_string());
        } else {
            unchecked.insert(id.to_string());
        }
    }
    let expected: BTreeSet<String> = ["2.1", "2.2", "2.3", "2.4", "2.5", "2.6", "2.7"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(checked, expected, "2.1—2.7 应全部勾选");
    assert_eq!(
        unchecked,
        ["2.8".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "2.8 需要真实双机，必须保持未勾选"
    );
}
