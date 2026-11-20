//! 接续样本报告（任务 4.4）的漂移检查。
//!
//! 这里**不**重跑样本（那是 `handoff_sample` 库内 10 条用例与 `bod handoff sample` 的职责，
//! 每次要起真实子进程），只保证「报告里写的」与「代码/任务表里的」没有分叉：
//!
//! - 报告的 schema 与样本 ID 必须与常量同源，不能手抄出一个新版本；
//! - 结论行的计数必须与「已证运行时」列出的个数对得上，且满足规格要求的运行时数；
//! - `local-process` 在 L2 上的 `unsupported` 必须写在报告里——把它写成 pass 就是夸大；
//! - 人工耗时必须是「脚本化 0」这种实测值，不能出现「未记录却仍下结论」；
//! - 报告点名的 `case_*` 必须真在故障验收文件里（防引用幽灵用例）；
//! - 报告说「保持未勾选」时，`tasks.md` 的 4.4 就必须仍是 `- [ ]`。

use bod_app::handoff_sample::{REQUIRED_RUNTIMES, SAMPLE_SCHEMA, TASK_SAMPLE_ID};

const REPORT: &str = include_str!("../../../docs/reports/p2-handoff-samples.md");
const TASKS: &str = include_str!("../../../openspec/changes/p2-work-continuation/tasks.md");
const FAULT_SUITE: &str = include_str!("p2_fault_acceptance.rs");
const CLI: &str = include_str!("../../bod/src/main.rs");

fn line_starting_with<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines().find(|line| line.starts_with(prefix))
}

/// 从文本里抽取所有 `case_xxx` 标识符（去重，保持出现顺序）。
fn cited_cases(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found: Vec<&str> = Vec::new();
    let mut index = 0usize;
    while index + 5 <= bytes.len() {
        if &bytes[index..index + 5] != b"case_" {
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index + 5;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let name = &text[start..end];
        if name.len() > 5 && !found.contains(&name) {
            found.push(name);
        }
        index = end.max(index + 1);
    }
    found
}

/// 取 `start` 之后、`end` 之前的一段；两端都不含。
fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text
        .find(start)
        .map(|index| index + start.len())
        .unwrap_or(0);
    let rest = &text[from..];
    let to = rest.find(end).unwrap_or(rest.len());
    &rest[..to]
}

/// 某一段里 `adapter.local-process` 的所有行。
fn local_rows(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains("adapter.local-process"))
        .collect()
}

#[test]
fn report_uses_the_same_schema_and_sample_id_as_the_code() {
    assert!(
        REPORT.contains(SAMPLE_SCHEMA),
        "报告必须写明 schema {SAMPLE_SCHEMA}"
    );
    assert!(
        REPORT.contains(TASK_SAMPLE_ID),
        "报告必须写明样本 {TASK_SAMPLE_ID}"
    );
    // 报告里的两个常量不是手抄的：抽出来逐字比。
    assert!(REPORT.contains(&format!("- schema: {SAMPLE_SCHEMA}")));
    assert!(REPORT.contains(&format!("- 样本: {TASK_SAMPLE_ID}")));
}

#[test]
fn claim_line_agrees_with_the_listed_runtimes() {
    let claim_line = line_starting_with(REPORT, "接续证据: ").expect("报告必须有结论行");
    assert!(
        claim_line.starts_with(&format!("接续证据: {}", "evidenced"))
            || claim_line.starts_with("接续证据: insufficient"),
        "结论行只允许 evidenced / insufficient，实际 {claim_line}"
    );
    let numbers = claim_line
        .split('(')
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("结论行必须带 (n/m)");
    let mut parts = numbers.split('/');
    let passed: usize = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let required: usize = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
    assert_eq!(
        required, REQUIRED_RUNTIMES,
        "报告要求的分母必须与代码常量一致"
    );

    if claim_line.contains("evidenced") {
        let listed = line_starting_with(REPORT, "已证运行时: ").expect("evidenced 必须列出运行时");
        let names: Vec<&str> = listed
            .trim_start_matches("已证运行时: ")
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .collect();
        assert_eq!(
            names.len(),
            passed,
            "结论说 {passed} 个运行时，列表里有 {}",
            names.len()
        );
        assert!(passed >= REQUIRED_RUNTIMES);
    }
}

#[test]
fn local_process_limitation_is_stated_not_hidden() {
    let l2 = section(REPORT, "## 2. L2 实测", "## 3. L1 对照");
    let l1 = section(REPORT, "## 3. L1 对照", "## 4. 负责人切换样本");

    let l2_rows = local_rows(l2);
    assert!(
        l2_rows.len() >= 2,
        "L2 段必须列出 local-process 的结论，实际 {} 行",
        l2_rows.len()
    );
    for row in &l2_rows {
        assert!(
            row.contains("unsupported(reliable_observation)"),
            "local-process 在 L2 上只能记 unsupported，不能写成通过：{row}"
        );
    }

    // 对照：L1 段里同一个适配器应当通过。两段不一致才是能力边界，全文都写 pass 就是夸大。
    let l1_rows = local_rows(l1);
    assert!(!l1_rows.is_empty(), "L1 段缺少 local-process 的对照行");
    for row in &l1_rows {
        assert!(
            row.contains("pass（"),
            "L1 段应显示 local-process 通过（对照 L2 的 unsupported）：{row}"
        );
    }
    assert!(REPORT.contains("重启后无法重放"));
}

#[test]
fn human_time_is_an_observed_zero_not_an_omission() {
    assert!(
        REPORT.contains("脚本化，无人工介入"),
        "人工耗时必须写明是脚本化实测的 0"
    );
    assert!(
        !REPORT.contains("人工耗时: 未记录"),
        "既有结论就不能把未记录的人工耗时说成 0"
    );
    // 报告必须承认这一点，而不是含糊过去。
    assert!(REPORT.contains("--human-minutes"));
    assert!(REPORT.contains("保持未勾选"));
}

#[test]
fn cited_cases_exist_in_the_fault_suite() {
    let cases = cited_cases(REPORT);
    assert!(!cases.is_empty(), "报告应点名负责人切换的命名 case");
    for name in cases {
        assert!(
            FAULT_SUITE.contains(name),
            "报告引用了不存在的 case：{name}"
        );
        assert!(
            FAULT_SUITE.contains(&format!("fn {name}(")),
            "case {name} 必须真是一个测试函数"
        );
    }
}

#[test]
fn open_task_stays_unchecked_while_the_report_lists_gaps() {
    let line = TASKS
        .lines()
        .find(|line| line.starts_with("- [ ] 4.4") || line.starts_with("- [x] 4.4"))
        .expect("tasks.md 必须有 4.4 的勾选行");
    if REPORT.contains("保持未勾选") {
        assert!(
            line.starts_with("- [ ]"),
            "报告说 4.4 未勾选，tasks.md 却已勾上：{line}"
        );
    }
    // 报告的边界段必须点明真实人工演练仍缺，否则「机器能产的都产了」会被读成「全部完成」。
    assert!(REPORT.contains("真实人工演练"));
    assert!(REPORT.contains("未具备"));
}

#[test]
fn cli_exposes_the_sample_command_and_its_flags() {
    assert!(CLI.contains("bod handoff sample"), "USAGE 必须列出采样命令");
    assert!(
        CLI.contains("cmd == \"handoff\" && sub == \"sample\""),
        "必须真有命令分支，不能只在帮助里写"
    );
    for flag in ["--level", "--root", "--json", "--human-minutes"] {
        assert!(CLI.contains(flag), "缺参数 {flag}");
    }
}
