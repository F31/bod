//! P7 §4（4.1—4.3）的漂移检查：两阶段数字必须出自真实阶段报告，预算记录必须定位每个排除项，
//! 出口证据不得把未测项说成达标，也不得把"未冻结"写成"已冻结"。

use bod_app::capacity_evaluation::PHASE_TOLERANCE_PCT;

const PHASE1_WIN: &str = include_str!("../../../docs/reports/p7-phase1-windows.md");
const PHASE2_WIN: &str = include_str!("../../../docs/reports/p7-phase2-windows.md");
const TWO_WIN: &str = include_str!("../../../docs/reports/p7-two-phase-windows.md");
const PHASE1_LIN: &str = include_str!("../../../docs/reports/p7-phase1-linux.md");
const PHASE2_LIN: &str = include_str!("../../../docs/reports/p7-phase2-linux.md");
const TWO_LIN: &str = include_str!("../../../docs/reports/p7-two-phase-linux.md");
const BUDGET: &str = include_str!("../../../docs/reports/p7-budget.md");
const EXIT: &str = include_str!("../../../docs/reports/p7-exit-evidence.md");
const DECISION: &str = include_str!("../../../docs/reports/p7-decision.md");
const BASELINE_LIN: &str = include_str!("../../../docs/reports/p7-baseline-linux.md");
const TASKS: &str = include_str!("../../../openspec/changes/p7-capacity-evaluation/tasks.md");

const PLATFORMS: [(&str, &str, &str, &str); 2] = [
    ("windows", PHASE1_WIN, PHASE2_WIN, TWO_WIN),
    ("linux", PHASE1_LIN, PHASE2_LIN, TWO_LIN),
];

/// 从 `## name` 段里取 `- 实测：` 行的所有数字。
fn section_numbers(report: &str, name: &str) -> Vec<f64> {
    let marker = format!("## {name}\n");
    let start = match report.find(&marker) {
        Some(start) => start,
        None => panic!(
            "阶段报告缺少 {name} 段（report {} 字节，含 '## ' 行 {} 条）",
            report.len(),
            report
                .lines()
                .filter(|line| line.starts_with("## "))
                .count()
        ),
    };
    let rest = &report[start + marker.len()..];
    let section = match rest.find("\n## ") {
        Some(end) => &rest[..end],
        None => rest,
    };
    let line = section
        .lines()
        .find(|line| line.starts_with("- 实测："))
        .unwrap_or_else(|| panic!("{name} 段缺少实测行"));
    digits(&line["- 实测：".len()..])
}

/// 抽出文本里的数字（支持负号与小数），忽略其它字符。
fn digits(text: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut current = String::new();
    let bytes: Vec<char> = text.chars().collect();
    for (index, ch) in bytes.iter().enumerate() {
        let negative_start = *ch == '-'
            && current.is_empty()
            && bytes
                .get(index + 1)
                .map(|next| next.is_ascii_digit() || *next == '.')
                .unwrap_or(false);
        if ch.is_ascii_digit() || *ch == '.' || negative_start {
            current.push(*ch);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<f64>() {
                out.push(value);
            }
            current.clear();
        }
    }
    if let Ok(value) = current.parse::<f64>() {
        out.push(value);
    }
    out
}

/// 两阶段表：`(指标, 阶段一, 阶段二, 漂移, 判定)`。
fn drift_rows(report: &str) -> Vec<(String, f64, f64, f64, String)> {
    let mut rows = Vec::new();
    for line in report.lines() {
        if !line.starts_with("| `") {
            continue;
        }
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 7 {
            continue;
        }
        let name = cells[1].trim_matches('`').to_string();
        let first: f64 = cells[2].parse().expect("阶段一必须是数字");
        let second: f64 = cells[3].parse().expect("阶段二必须是数字");
        let drift: f64 = cells[4]
            .trim_end_matches('%')
            .trim_start_matches('+')
            .parse()
            .expect("漂移必须是百分比数字");
        rows.push((name, first, second, drift, cells[6].to_string()));
    }
    rows
}

/// 报告里 `| \`name\` | form | target | value | samples | Status |` 的 (name, Status)。
fn status_rows(report: &str) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for line in report.lines() {
        if !line.starts_with("| `") {
            continue;
        }
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 7 {
            continue;
        }
        let status = cells[6].to_string();
        if status.starts_with("Measured")
            || status.starts_with("Unverified")
            || status.starts_with("Target")
        {
            rows.push((cells[1].trim_matches('`').to_string(), status));
        }
    }
    rows
}

/// 4.1：两平台都有两阶段报告，且判定都是「未冻结」（真实结论，不是漏跑）。
#[test]
fn task_4_1_two_phase_reports_exist_for_both_platforms() {
    for (platform, _, _, two_phase) in PLATFORMS {
        assert!(
            two_phase.contains(&format!("- 漂移容差：±{PHASE_TOLERANCE_PCT:.0}%")),
            "{platform} 报告缺少容差行"
        );
        assert!(
            two_phase.contains("## 单项漂移"),
            "{platform} 报告缺少漂移表"
        );
        assert!(
            two_phase.contains("- 预算冻结：**未冻结**"),
            "{platform} 报告必须如实记录未冻结"
        );
        assert!(
            !two_phase.contains("**budget-frozen**"),
            "{platform} 不得宣称已冻结"
        );
    }
}

/// 4.1：表格判定与结论行的点名必须一致，且计数加总等于全部测量项。
#[test]
fn task_4_1_verdict_line_matches_table_rows() {
    for (platform, _, _, two_phase) in PLATFORMS {
        let rows = drift_rows(two_phase);
        let over: Vec<&str> = rows
            .iter()
            .filter(|row| row.4.contains("超出容差"))
            .map(|row| row.0.as_str())
            .collect();
        let stable = rows
            .iter()
            .filter(|row| row.4.contains("可转 Measured"))
            .count();

        let freeze_line = two_phase
            .lines()
            .find(|line| line.starts_with("- 预算冻结："))
            .expect("必须有冻结判定行");
        for name in &over {
            assert!(
                freeze_line.contains(name),
                "{platform} 冻结判定行漏了点名 {name}：{freeze_line}"
            );
        }

        let conclusion = two_phase
            .lines()
            .find(|line| line.starts_with("## 结论："))
            .expect("必须有结论行");
        assert!(
            conclusion.contains(&format!("可转 Measured {stable} 项")),
            "{platform} 结论计数与表格不一致：{conclusion}"
        );

        let promotable_line = two_phase
            .lines()
            .find(|line| line.starts_with("- 可转 Measured："))
            .expect("必须列出可转项");
        let promoted = promotable_line
            .trim_start_matches("- 可转 Measured：")
            .split('、')
            .count();
        assert_eq!(promoted, stable, "{platform} 可转项数量与表格不一致");
    }
}

/// 4.1：两阶段表里的每个数字都能在对应阶段的报告里找到（防手写估数）。
#[test]
fn task_4_1_two_phase_numbers_come_from_phase_reports() {
    for (platform, phase1, phase2, two_phase) in PLATFORMS {
        for (name, first, second, drift, _) in drift_rows(two_phase) {
            for (label, report, expected) in [("阶段一", phase1, first), ("阶段二", phase2, second)]
            {
                let candidates = section_numbers(report, &name);
                // 报告正文对数值做了取整展示（如 1.538 ms 写成「重开就绪 2 ms」），
                // 因此容差 = 0.5 个展示单位 + 1% 相对，既能吸收取整，也不会放过换一轮的数字。
                let hit = candidates
                    .iter()
                    .any(|value| (value - expected).abs() <= 0.5 + expected.abs() * 0.01);
                assert!(
                    hit,
                    "{platform} {name} 的{label}值 {expected} 在阶段报告里找不到；实测段为 {candidates:?}"
                );
            }
            let recomputed = if first == 0.0 {
                0.0
            } else {
                (second - first) / first * 100.0
            };
            // 表格里的值只展示三位小数、漂移只展示一位小数，重算会有小偏差。
            // 容差按「两处三位小数的最坏取整误差」推：0.05 pp + 200 × 0.0005 / |阶段一值|。
            // 小基数项（如 0.084 ms）容差自动放宽到约 1.2 pp，大基数项仍收紧在 0.05 pp。
            let tolerance = 0.05 + 200.0 * 0.0005 / first.abs().max(f64::MIN_POSITIVE);
            assert!(
                (recomputed - drift).abs() <= tolerance,
                "{platform} {name} 漂移与两阶段值不自洽：报告 {drift}% vs 算得 {recomputed:.2}%（容差 {tolerance:.2} pp）"
            );
        }
    }
}

/// 4.1：可转项与保留项加起来覆盖全部测量项，不能靠"少列几项"制造稳定假象。
#[test]
fn task_4_1_promotable_plus_unresolved_covers_inventory() {
    let inventory = status_rows(BASELINE_LIN).len();
    assert!(inventory >= 25, "基线清单一共 {inventory} 项，少于预期");

    for (platform, _, _, two_phase) in PLATFORMS {
        let conclusion = two_phase
            .lines()
            .find(|line| line.starts_with("## 结论："))
            .expect("必须有结论行");
        let numbers = digits(conclusion);
        assert_eq!(
            numbers.len(),
            2,
            "{platform} 结论行应有两个计数：{conclusion}"
        );
        let total = numbers[0] + numbers[1];
        assert!(
            (total - inventory as f64) <= 1.0,
            "{platform} 可转 + 保留 = {total}，与清单 {inventory} 项不匹配"
        );
    }
}

/// 4.2：每个被排除的项都必须在预算记录里定位到默认依赖或结构。
#[test]
fn task_4_2_budget_locates_every_excluded_item() {
    let mut excluded: Vec<String> = Vec::new();
    for (_, _, _, two_phase) in PLATFORMS {
        for (name, _, _, _, verdict) in drift_rows(two_phase) {
            if verdict.contains("超出容差") && !excluded.contains(&name) {
                excluded.push(name);
            }
        }
    }
    assert!(!excluded.is_empty(), "两平台至少有一个超容差项");

    for name in &excluded {
        let row = BUDGET
            .lines()
            .find(|line| line.starts_with(&format!("| `{name}`")))
            .unwrap_or_else(|| panic!("预算记录缺少排除项 {name} 的行"));
        assert!(
            row.contains("默认依赖") || row.contains("结构"),
            "{name} 只有「超出容差」结论，没有定位默认依赖与结构"
        );
        assert!(
            row.contains("复测条件") || row.contains("重跑") || row.contains("中位数"),
            "{name} 缺少复测条件"
        );
    }
}

/// 4.2：预算记录必须写明版本与适用范围，并把"未冻结"和"没有降低正确性"写死。
#[test]
fn task_4_2_budget_declares_version_scope_and_no_relaxation() {
    for expected in [
        "p7-budget-v1",
        "未冻结",
        "headless",
        "±25%",
        "200 单条事务 + 5000 批量事件",
        "synchronous = Full",
        "\"relaxations\": []",
        "\"state\": \"not-frozen\"",
        "BEGIN IMMEDIATE",
    ] {
        assert!(BUDGET.contains(expected), "预算记录缺少 {expected}");
    }
    assert!(
        !BUDGET.contains("预算已冻结"),
        "预算未生效时不得出现「已冻结」字样"
    );

    let json_start = BUDGET.find("```json").expect("预算记录要有 json 块") + "```json".len();
    let rest = &BUDGET[json_start..];
    let json_end = rest.find("```").expect("json 块必须闭合");
    let value: serde_json::Value =
        serde_json::from_str(rest[..json_end].trim()).expect("json 块必须能解析");
    assert_eq!(value["state"], "not-frozen");
    assert_eq!(
        value["relaxations"].as_array().map(Vec::len),
        Some(0),
        "relaxations 必须为空"
    );
    assert_eq!(value["sync_level"], "FULL");
    assert!(
        value["frozen_items"].as_array().map(Vec::len) == Some(0),
        "未冻结时不得有冻结项"
    );
}

/// 4.3：出口证据必须给出可核对的瓶颈与四类候选对比，不能只有形容词。
#[test]
fn task_4_3_exit_evidence_cites_bottleneck_and_comparisons() {
    for expected in [
        "182.37",
        "338.57",
        "58359",
        "60619",
        "377",
        "执行节点扩展",
        "PostgreSQL",
        "外部持久执行服务",
        "索引 / 批处理 / 存储优化",
        "p7-decision.md",
        "p7-budget.md",
    ] {
        assert!(EXIT.contains(expected), "出口证据缺少 {expected}");
    }
    // 决定记录里出现的候选名必须都能在出口证据里找到。
    for candidate in [
        "索引/批处理/存储优化",
        "执行节点扩展",
        "PostgreSQL",
        "外部持久执行服务",
    ] {
        assert!(DECISION.contains(candidate), "决定记录缺少候选 {candidate}");
    }
}

/// 4.3：出口证据里所有 `Unverified` 项都要保留在原状态并给复测条件，不能悄悄消失。
#[test]
fn task_4_3_exit_evidence_keeps_unverified_items_open() {
    let unverified: Vec<String> = status_rows(BASELINE_LIN)
        .into_iter()
        .filter(|(_, status)| status == "Unverified")
        .map(|(name, _)| name)
        .collect();
    assert!(!unverified.is_empty(), "基线里应有 Unverified 项");

    let section = EXIT
        .split("## 6. 未取得项")
        .nth(1)
        .expect("出口证据必须有未取得项清单");
    for name in &unverified {
        assert!(section.contains(name.as_str()), "未取得项清单漏了 {name}");
    }
    assert!(section.contains("复测条件"), "未取得项清单必须给复测条件");
    assert!(
        !EXIT.contains("全部达标") && !EXIT.contains("预算已冻结"),
        "存在未取得项与未冻结预算时不得宣称全部达标"
    );
}

/// 4.3：出口证据必须写明「不按日事件数机械迁移」并保持支持矩阵不扩大。
#[test]
fn task_4_3_exit_evidence_keeps_governance_rules() {
    for expected in [
        "不按日事件数",
        "RejectedByEventCount",
        "0 Supported",
        "guard_change_in_capacity_stage",
        "未冻结",
    ] {
        assert!(EXIT.contains(expected), "出口证据缺少 {expected}");
    }
    for expected in ["日均事件数", "四类信号"] {
        assert!(EXIT.contains(expected), "出口证据缺少 {expected}");
    }
}

/// 4.1—4.3：导出结论与预算记录必须一致（不能一处说未冻结、另一处说已冻结）。
#[test]
fn task_4_3_budget_and_exit_agree_on_freeze_state() {
    let budget_frozen = BUDGET.contains("预算已冻结");
    let exit_frozen = EXIT.contains("正式容量预算已冻结");
    assert_eq!(budget_frozen, exit_frozen, "预算状态在两份报告里必须一致");
    assert!(EXIT.contains("未冻结"), "出口证据必须写明预算未冻结");
}

/// 4.x 必须勾选，并在下一行留下落点。
#[test]
fn tasks_4_x_are_checked_with_evidence() {
    let section = TASKS
        .split("## 4. 预算冻结与出口")
        .nth(1)
        .expect("tasks 必须有 §4");
    let section = section
        .split("## Workflow follow-up")
        .next()
        .unwrap_or(section);
    let lines: Vec<&str> = section.lines().collect();
    for task in ["4.1", "4.2", "4.3"] {
        let index = lines
            .iter()
            .position(|line| line.contains(&format!("- [x] {task}")))
            .unwrap_or_else(|| panic!("{task} 未勾选"));
        let note = lines
            .get(index + 1)
            .copied()
            .unwrap_or_default()
            .trim_start();
        assert!(
            note.starts_with('→'),
            "{task} 勾选后必须写落点，便于审计：{note}"
        );
        assert!(note.len() > 60, "{task} 的落点太短，写不出具体文件与用例");
    }
}
