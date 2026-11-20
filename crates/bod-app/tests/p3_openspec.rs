//! P3 任务 1.6 的漂移检查：规格说明、实现与任务进度三者不能各说各话。
//!
//! 三类检查：
//! 1. **可追溯**：delta 里每条 requirement 的标题都要在 spec note 里出现（1.6 的验收条件）；
//! 2. **用例名不漂**：note「命名用例」表里列的每个名字，必须真是模块里的测试函数；
//! 3. **口径不松**：note 必须写明「勾选不是验收证据」「不按行号或显示编号」这两条硬规则。

use std::collections::BTreeSet;

const DELTA: &str = include_str!(
    "../../../openspec/changes/p3-change-collaboration/specs/openspec-integration/spec.md"
);
const NOTE: &str = include_str!("../../../docs/spec-notes/p3-openspec.md");
const MODULE: &str = include_str!("../src/openspec_integration.rs");
const LIB: &str = include_str!("../src/lib.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p3-change-collaboration/tasks.md");

/// 取出 delta 里所有 `### Requirement: <title>` 的标题。
fn requirement_titles(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("### Requirement: "))
        .map(|title| title.trim().to_string())
        .collect()
}

#[test]
fn spec_note_covers_every_requirement_in_the_delta() {
    let titles = requirement_titles(DELTA);
    assert_eq!(titles.len(), 5, "openspec-integration delta 应有 5 条需求");
    for title in &titles {
        assert!(
            NOTE.contains(title.as_str()),
            "spec note 未覆盖需求 `{title}`；1.6 要求每个映射字段可追溯到本 Change 的 requirement"
        );
    }
}

#[test]
fn every_named_case_in_the_note_is_a_real_test() {
    // 只读 §1 的「命名用例」表：从 `### 命名用例` 到下一个二级标题之间。
    let section = NOTE
        .split("### 命名用例")
        .nth(1)
        .expect("spec note 必须有「命名用例」小节")
        .split("\n## ")
        .next()
        .unwrap_or_default();
    // 每行只取第一个反引号片段（表格首列）；右侧解释里出现的字段名不算用例名。
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
    assert!(
        names.len() >= 5,
        "命名用例表应至少列 5 条，实际 {} 条",
        names.len()
    );
    for name in names {
        assert!(
            MODULE.contains(&format!("fn {name}(")),
            "spec note 列的用例 `{name}` 在模块里不存在——改名后没同步"
        );
    }
}

#[test]
fn note_keeps_the_two_hard_rules() {
    assert!(
        NOTE.contains("不按行号") || NOTE.contains("不是行号"),
        "note 必须写明任务身份不按行号绑定"
    );
    assert!(
        NOTE.contains("进度投影") && NOTE.contains("验收"),
        "note 必须写明勾选是进度投影、不是验收证据"
    );
    assert!(
        NOTE.contains("parallel_safe"),
        "note 必须写明同 requirement 多 Change 时不能并行归档"
    );
    assert!(
        NOTE.contains("MechanicalSplit"),
        "note 必须写明机械均分被拒绝"
    );
}

#[test]
fn module_is_exported_and_documented() {
    assert!(
        LIB.contains("pub mod openspec_integration;"),
        "模块必须在 lib.rs 导出"
    );
    assert!(
        MODULE.contains("I-10") || MODULE.contains("invariant_i_10"),
        "模块文档要点明 I-10 的落点"
    );
    // 单包上限必须与 note 写的一致，避免"文档说 12、代码是别的数"。
    assert!(MODULE.contains("MAX_FILES_PER_PACKAGE: u32 = 12"));
    assert!(NOTE.contains("12"));
}

#[test]
fn tasks_1_1_to_1_6_are_checked_only_with_the_note_in_place() {
    let mut checked = BTreeSet::new();
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("- [") else {
            continue;
        };
        let is_checked = rest.starts_with('x');
        let body = rest.get(3..).unwrap_or_default().trim();
        if is_checked && let Some(id) = body.split_whitespace().next() {
            checked.insert(id.to_string());
        }
    }
    for id in ["1.1", "1.2", "1.3", "1.4", "1.5", "1.6"] {
        assert!(
            checked.contains(id),
            "任务 {id} 应已勾选；若确认未完成请撤销勾选，不能只在文档里写完成"
        );
    }
    // 勾选必须有落地：note 与模块都在。
    assert!(NOTE.contains("## 7. 验证"));
}
