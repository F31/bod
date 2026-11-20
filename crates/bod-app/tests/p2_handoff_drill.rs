//! 真实演练取样（4.4 人工耗时 / 5.2 对照）的漂移检查。
//!
//! 这里**不**重跑演练（那是人手按秒表的事），只保证「文档写的口径」与「代码强制的口径」
//! 以及「任务表的状态」没有分叉：
//!
//! - CLI 必须暴露 `handoff drill` 与 `--drill`，且不允许 `--drill` 与 `--human-minutes` 混用；
//! - `HumanTime` 四态与「口述值不算证据」的规则必须写在 spec note 里；
//! - 秒表两端必须是 `last_action` / `handoff_done`，且读数写成「算出」而不是「填入」；
//! - 5.2 的四项指标名必须与实现一致；
//! - 读数未取得时，`tasks.md` 的 4.4 / 5.2 必须是 `- [ ]`，出口结论必须是 `blocked`——
//!   谁把冒烟当证据去勾任务，这里先挂。

const CLI: &str = include_str!("../../bod/src/main.rs");
const SPEC_NOTE: &str = include_str!("../../../docs/spec-notes/p2-handoff-drill.md");
const SAMPLES_REPORT: &str = include_str!("../../../docs/reports/p2-handoff-samples.md");
const EXIT_EVIDENCE: &str = include_str!("../../../docs/reports/p2-exit-evidence.md");
const TASKS: &str = include_str!("../../../openspec/changes/p2-work-continuation/tasks.md");
const DRILL_MODULE: &str = include_str!("../src/handoff_drill.rs");
const DRILL_CARD: &str = include_str!("../../../docs/reports/p2-drill-card.md");

fn task_line<'a>(text: &'a str, id: &str) -> Option<&'a str> {
    let needle = format!("{id} ");
    text.lines().find(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("- [") && trimmed.contains(&needle)
    })
}

#[test]
fn cli_exposes_the_drill_family_and_its_evidence_flag() {
    for needle in [
        "handoff\" && sub == \"drill\"",
        "--drill",
        "handoff drill start",
        "handoff drill mark",
        "handoff drill finish",
        "handoff drill arm",
    ] {
        assert!(
            CLI.contains(needle),
            "CLI 使用说明或分发里缺 {needle}：演练入口必须可发现"
        );
    }
    assert!(
        CLI.contains("不能同时给"),
        "CLI 必须拒绝 --drill 与 --human-minutes 混用，否则分不清哪个读数进了报告"
    );
}

#[test]
fn stopwatch_readings_are_computed_not_supplied() {
    // 读数的唯一来源是两端档位之差。
    assert!(
        DRILL_MODULE.contains("end_ms - start_ms"),
        "handoff_ms 必须由 handoff_done 与 last_action 之差算出"
    );
    assert!(
        DRILL_MODULE.contains("DrillStep::LastAction")
            && DRILL_MODULE.contains("DrillStep::HandoffDone"),
        "秒表两端必须是 last_action 与 handoff_done"
    );
    assert!(
        SPEC_NOTE.contains("算出"),
        "spec note 必须说明读数由时间戳算出，而不是填进去"
    );
    assert!(
        !CLI.contains("HumanTime::Measured"),
        "旧的裸数字变体不该再出现在 CLI 里：口述值走 Claimed"
    );
}

#[test]
fn four_human_time_states_are_documented_with_the_evidence_rule() {
    for state in ["`Scripted`", "`Drilled`", "`Claimed`", "`Unrecorded`"] {
        assert!(
            SPEC_NOTE.contains(state),
            "spec note 的四态表缺 {state}：口述值与实测值必须能分辨"
        );
    }
    assert!(
        SPEC_NOTE.contains("is_evidenced"),
        "spec note 必须写明 is_evidenced 这条判定线"
    );
    assert!(
        SPEC_NOTE.contains("口述值") && SPEC_NOTE.contains("不计入"),
        "spec note 必须写明口述值不计入 4.4 的证据计数"
    );
}

#[test]
fn four_comparison_metrics_match_the_implementation() {
    for metric in ["接管就绪", "员工中断数", "重复执行数", "人工补充"] {
        assert!(
            SAMPLES_REPORT.contains(metric),
            "样本报告 §6 缺指标 {metric}：5.2 的四项必须逐项对上"
        );
        assert!(
            DRILL_MODULE.contains(metric) || DRILL_MODULE.contains("ready_seconds"),
            "实现里缺 {metric} 的落点"
        );
    }
    assert!(
        SAMPLES_REPORT.contains("不写「提升 X%」") || SAMPLES_REPORT.contains("不预设提升比例"),
        "报告必须写明不预设提升比例"
    );
}

#[test]
fn drill_report_names_incomplete_arms_instead_of_leaving_blanks() {
    assert!(
        DRILL_MODULE.contains("未完成项"),
        "对照报告必须有无完成项一栏"
    );
    assert!(
        DRILL_MODULE.contains("臂没有样本"),
        "某臂零样本时必须点名，不能留空白被读成 0"
    );
    assert!(
        SAMPLES_REPORT.contains("样本数"),
        "报告必须列样本数：单样本的差值不构成结论"
    );
}

#[test]
fn tasks_stay_unchecked_while_the_readings_are_absent() {
    let line_44 = task_line(TASKS, "4.4").expect("tasks.md 必须有 4.4 一行");
    let line_52 = task_line(TASKS, "5.2").expect("tasks.md 必须有 5.2 一行");
    assert!(
        line_44.starts_with("- [ ]"),
        "4.4 仍未取得真实演练读数，不能勾选：{line_44}"
    );
    assert!(
        line_52.starts_with("- [ ]"),
        "5.2 仍未取得人工对照读数，不能勾选：{line_52}"
    );
    assert!(
        SAMPLES_REPORT.contains("真实读数：**未取得**"),
        "样本报告必须如实写读数未取得"
    );
    assert!(
        EXIT_EVIDENCE.contains("出口结论: blocked"),
        "出口结论在读数到位前必须是 blocked"
    );
}

/// 执行卡要照着敲，所以里面的档位名必须真被 `DrillStep::parse` 接受；
/// 同时读数位必须是占位符——卡里出现具体秒数，早晚会被人当读数抄进报告。
#[test]
fn drill_card_only_uses_real_steps_and_placeholders() {
    let known = [
        "task_ready",
        "last_action",
        "handoff_done",
        "verify_begin",
        "verify_end",
    ];
    for line in DRILL_CARD.lines() {
        if let Some(position) = line.find("--step ") {
            let rest = &line[position + "--step ".len()..];
            let token = rest.split_whitespace().next().unwrap_or_default();
            assert!(
                known.contains(&token),
                "执行卡里的档位 {token} 不是 DrillStep 支持的取值"
            );
        }
        // `--arm` 的两臂同理。
        if line.contains("--arm manual") || line.contains("--arm bod") {
            assert!(
                line.contains("<") || line.contains("|"),
                "执行卡的 --arm 示例必须是占位或说明行，不能带具体读数：{line}"
            );
        }
    }
    assert!(
        DRILL_CARD.contains("数不清就不记这一臂"),
        "执行卡必须写明「数不清就不记」，否则 0 会冒充「没发生」"
    );
    assert!(
        DRILL_CARD.contains("学习效应"),
        "执行卡必须登记两臂顺序带来的学习效应"
    );
    assert!(
        DRILL_CARD.contains("不要手改") || DRILL_CARD.contains("不要手改 `drill.timeline`"),
        "执行卡必须禁止手改时间线：手改会让时间戳与系统时钟脱钩"
    );
}
