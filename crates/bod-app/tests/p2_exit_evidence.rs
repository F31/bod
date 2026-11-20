//! P2 出口证据报告（任务 5.3）的漂移检查。
//!
//! 这里**不**重跑被验收的逻辑（那是 5.1 与 4.x 的职责），只保证「报告里点名的」
//! 与「仓库里存在的」没有分叉：
//!
//! - 报告点名的每个故障 case 必须真在 `p2_fault_acceptance.rs` 里；
//! - 报告引用的 `--test` 目标必须真存在于 `tests/`；
//! - 支持矩阵 §8 的 P2 行必须保持 `Planned`/`Unverified`，`Planned` 行不得带证据；
//! - 出口结论仍是 `blocked` 时，任何 P2 门禁都不得被登记成 `implemented`；
//! - 报告必须点名 4.4/5.2 与四项对照指标，防止把「未取得」悄悄改写成「已就绪」。

const REPORT: &str = include_str!("../../../docs/reports/p2-exit-evidence.md");
const MATRIX: &str = include_str!("../../../docs/spec-notes/p0-support-matrix.md");
const GATES: &str = include_str!("../../../verification/gates.yaml");
const FAULT_SUITE: &str = include_str!("p2_fault_acceptance.rs");

/// 从文本里抽取所有 `case_xxx` 形式的标识符（去重，保持出现顺序）。
fn cited_cases(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found: Vec<&str> = Vec::new();
    let mut i = 0usize;
    while i + 5 <= bytes.len() {
        if &bytes[i..i + 5] != b"case_" {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 5;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let name = &text[start..end];
        if name.len() > 5 && !found.contains(&name) {
            found.push(name);
        }
        i = end.max(i + 1);
    }
    found
}

/// 抽取报告里 `--test <name>` 的测试目标名。
fn cited_test_targets(text: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    let marker = "--test ";
    let mut rest = text;
    while let Some(pos) = rest.find(marker) {
        let after = &rest[pos + marker.len()..];
        let end = after
            .find(|ch: char| ch.is_whitespace() || ch == '`')
            .unwrap_or(after.len());
        let name = &after[..end];
        if !name.is_empty() && !found.contains(&name) {
            found.push(name);
        }
        rest = &after[end..];
    }
    found
}

fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text
        .find(heading)
        .unwrap_or_else(|| panic!("缺少小节 {heading}"));
    let rest = &text[start..];
    match rest[heading.len()..].find("\n## ") {
        Some(offset) => &rest[..heading.len() + offset],
        None => rest,
    }
}

fn has_report_line<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find(|line| line.trim_start().starts_with(prefix))
        .map(str::trim)
}

#[test]
fn report_states_blocked_and_names_the_two_open_tasks() {
    let verdict = has_report_line(REPORT, "出口结论:").expect("报告必须写明出口结论");
    assert_eq!(
        verdict, "出口结论: blocked",
        "4.4 与 5.2 未完成时出口结论只能是 blocked"
    );
    let blockers = has_report_line(REPORT, "阻塞项:").expect("报告必须列出阻塞项");
    assert!(blockers.contains("4.4"), "阻塞项必须点名 4.4：{blockers}");
    assert!(blockers.contains("5.2"), "阻塞项必须点名 5.2：{blockers}");
}

#[test]
fn every_cited_case_exists_in_the_fault_suite() {
    let cases = cited_cases(REPORT);
    assert!(
        cases.len() >= 6,
        "报告应点名 5.1 的六个命名 case，实际 {}",
        cases.len()
    );
    for name in cases {
        let needle = format!("fn {name}(");
        assert!(
            FAULT_SUITE.contains(&needle),
            "报告引用了不存在的 case：{name}"
        );
    }
}

#[test]
fn cited_test_targets_exist_on_disk() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let targets = cited_test_targets(REPORT);
    assert!(!targets.is_empty(), "报告应至少引用一个 --test 目标");
    for name in targets {
        let path = tests_dir.join(format!("{name}.rs"));
        assert!(path.is_file(), "报告引用了不存在的测试目标：{name}");
    }
}

#[test]
fn p2_matrix_rows_stay_unupgraded() {
    let block = section(MATRIX, "## 8. P2");
    let mut planned = 0usize;
    let mut rows = 0usize;
    for line in block.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 5 || cells[1].is_empty() || cells[1] == "能力" {
            continue;
        }
        if cells[1].starts_with("---") || cells[1].starts_with(":--") {
            continue;
        }
        rows += 1;
        let status = cells[2];
        let evidence = cells[3];
        assert!(
            !status.starts_with("Supported"),
            "P2 未归档且 4.4/5.2 未完成，不得标 Supported：{line}"
        );
        if status.contains("Planned") {
            planned += 1;
            assert!(
                evidence == "—" || evidence == "-" || evidence.is_empty(),
                "Planned 行不得带证据：{line}"
            );
        }
    }
    assert!(rows >= 4, "支持矩阵 §8 应至少 4 行，实际 {rows}");
    assert!(
        planned >= 2,
        "接续与对照指标应保持 Planned，实际只有 {planned} 行"
    );
    assert!(block.contains("人工耗时"), "§8 必须写明缺人工耗时");
}

#[test]
fn no_p2_gate_is_claimed_implemented_while_exit_is_blocked() {
    for id in ["G-28", "G-38", "G-39", "G-40", "G-41", "G-42"] {
        let marker = format!("- id: {id}\n");
        let Some(start) = GATES.find(&marker) else {
            continue; // 未登记，符合「P2 绑定保持 planned/不登记」的现状
        };
        let rest = &GATES[start + marker.len()..];
        let end = rest.find("\n- id: ").unwrap_or(rest.len());
        let block = &rest[..end];
        assert!(
            !block.contains("status: implemented"),
            "出口结论仍为 blocked，{id} 不应被登记成 implemented：{block}"
        );
    }
}

#[test]
fn report_keeps_comparison_metrics_and_unblocking_conditions() {
    for needle in [
        "接管就绪耗时",
        "员工中断数",
        "重复执行数",
        "人工补充时间",
        "人工耗时",
        "解除条件",
    ] {
        assert!(REPORT.contains(needle), "报告缺少「{needle}」");
    }
    assert!(
        REPORT.contains("不预设提升比例") || REPORT.contains("没有样本就没有比例"),
        "5.2 明确要求不预设提升比例，报告必须写明"
    );
}
