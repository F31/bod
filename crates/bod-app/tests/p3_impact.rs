//! P3 2.1—2.5 的漂移检查：需求覆盖、用例名、硬口径。

use std::collections::BTreeSet;

const DELTA: &str =
    include_str!("../../../openspec/changes/p3-change-collaboration/specs/impact-analysis/spec.md");
const NOTE: &str = include_str!("../../../docs/spec-notes/p3-impact.md");
const MODULE: &str = include_str!("../src/impact_revalidation.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

fn requirement_titles(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .map(|title| title.trim().to_string())
        .collect()
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles = requirement_titles(DELTA);
    assert_eq!(titles.len(), 5, "impact-analysis delta 应有 5 条需求");
    for title in &titles {
        assert!(
            NOTE.contains(title.as_str()),
            "spec note 未覆盖需求 `{title}`"
        );
    }
}

#[test]
fn every_named_case_in_the_note_is_a_real_test() {
    let section = NOTE
        .split("## 6. 命名用例")
        .nth(1)
        .expect("note 必须有命名用例小节")
        .split("\n## ")
        .next()
        .unwrap_or_default();
    let names: Vec<String> = section
        .lines()
        .filter_map(|line| {
            let cell = line.trim_start().strip_prefix("| `")?;
            let end = cell.find('`')?;
            Some(cell[..end].to_string())
        })
        .filter(|item| {
            item.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
        .collect();
    assert!(names.len() >= 10, "至少列 10 条用例，实际 {}", names.len());
    for name in names {
        assert!(
            MODULE.contains(&format!("fn {name}(")),
            "note 列的用例 `{name}` 在模块里不存在"
        );
    }
}

#[test]
fn note_keeps_the_three_hard_rules() {
    assert!(
        NOTE.contains("不受影响") && NOTE.contains("缺少关系"),
        "note 必须写明「未找到依赖 ≠ 不受影响」"
    );
    assert!(
        NOTE.contains("不据此判定失效"),
        "note 必须写明推测边不判定下游失效"
    );
    assert!(
        NOTE.contains("没有告警当健康"),
        "note 必须写明「没有告警不等于健康」"
    );
}

#[test]
fn module_is_exported_and_bounds_match_constants() {
    assert!(
        LIB.contains("pub mod impact_revalidation;"),
        "模块必须在 lib.rs 导出"
    );
    assert!(MODULE.contains("MAX_TARGETS_PER_PROPOSAL: usize = 8"));
    assert!(MODULE.contains("MAX_PROPOSALS_PER_TRIGGER: usize = 4"));
    assert!(NOTE.contains("（8）") && NOTE.contains("（4）"));
}

#[test]
fn tasks_2_1_to_2_5_are_checked_while_2_6_waits_for_the_real_sample() {
    let mut checked = BTreeSet::new();
    let mut unchecked = BTreeSet::new();
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("- [") else {
            continue;
        };
        let is_checked = rest.starts_with('x');
        let body = rest.get(3..).unwrap_or_default().trim();
        let Some(id) = body.split_whitespace().next() else {
            continue;
        };
        if is_checked {
            checked.insert(id.to_string());
        } else {
            unchecked.insert(id.to_string());
        }
    }
    for id in ["2.1", "2.2", "2.3", "2.4", "2.5"] {
        assert!(checked.contains(id), "任务 {id} 应已勾选");
    }
    assert!(
        unchecked.contains("2.6"),
        "2.6 需要真实接口变化样本跑 G-31，未取得前不得勾选"
    );
}
