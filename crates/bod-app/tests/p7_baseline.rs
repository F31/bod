//! P7 §1（1.1—1.5）的漂移检查：实测项清单、报告渲染、手册与规格必须一致。

use bod_app::capacity_evaluation::{
    BaselineReport, EnvironmentRecord, FormFactor, MeasureItem, MeasureStatus, render_markdown,
};

fn environment() -> EnvironmentRecord {
    EnvironmentRecord {
        note: "linux / x86_64 / rustc test".to_string(),
        os: "linux x86_64".to_string(),
        cpu: "8 逻辑核".to_string(),
        rustc: "rustc test".to_string(),
        build_params: "dev profile / debug_assertions=true".to_string(),
        sqlite_sync: "WAL + synchronous=FULL + BEGIN IMMEDIATE".to_string(),
        data_scale: "测试规模".to_string(),
        cache_state: "冷".to_string(),
    }
}

#[test]
fn task_1_4_state_machine_is_three_valued_and_blocks_unmeasured_items() {
    let target = MeasureItem::new("event_throughput_batched", FormFactor::Headless, "≥ 5000/s");
    assert_eq!(target.status, MeasureStatus::Target);
    assert!(!target.usable_for_decision());

    let unverified = MeasureItem::new("state_to_web_visible_p95", FormFactor::Web, "≤ 2 s")
        .unverified("无 Web 宿主", "cargo run -p xtask -- bench");
    assert_eq!(unverified.status, MeasureStatus::Unverified);
    assert!(unverified.failure_reason.is_some());

    let measured = MeasureItem::new(
        "key_transaction_latency",
        FormFactor::Headless,
        "p99 ≤ 10 ms",
    )
    .measured("p99 1.6 ms", 200, "cargo run -p xtask -- bench", Some(true));
    assert_eq!(measured.status, MeasureStatus::Measured);
    assert!(measured.usable_for_decision());

    let report = BaselineReport {
        environment: environment(),
        commit: "test".to_string(),
        items: vec![target, unverified, measured],
    };
    assert!(
        !report.usable_for_decision(),
        "含 Target 项的报告不得进入容量决策"
    );
    assert_eq!(report.target_count(), 1);
    assert_eq!(report.unverified_count(), 1);
    assert_eq!(report.measured_count(), 1);
}

#[test]
fn task_1_3_rendered_report_carries_command_samples_and_status_per_item() {
    let report = BaselineReport {
        environment: environment(),
        commit: "abc1234".to_string(),
        items: vec![
            MeasureItem::new(
                "key_transaction_latency",
                FormFactor::Headless,
                "p99 ≤ 10 ms",
            )
            .with_method("200 次单条写事务")
            .with_note("串行单写口径")
            .measured(
                "p50 1.0 ms / p99 1.6 ms",
                200,
                "cargo run -p xtask -- bench",
                Some(true),
            ),
            MeasureItem::new("state_to_web_visible_p95", FormFactor::Web, "≤ 2 s")
                .with_method("需要 Web 宿主")
                .unverified("无 Web 宿主", "cargo run -p xtask -- bench"),
        ],
    };
    let rendered = render_markdown(&report, &[]);

    assert!(rendered.contains("- commit：abc1234"));
    assert!(rendered.contains("- 结果：Measured 1 / Unverified 1 / Target 0"));
    assert!(rendered.contains("与上一轮差异项：无"));
    for item in &report.items {
        assert!(
            rendered.contains(&format!("\n## {}\n", item.name)),
            "报告缺少 {} 的小节",
            item.name
        );
        assert!(
            rendered.contains(&format!("- 命令：`{}`", item.command)),
            "{} 必须写出复现命令",
            item.name
        );
        assert!(
            rendered.contains(&format!("- 样本数：{}", item.samples)),
            "{} 必须写出样本数",
            item.name
        );
        assert!(
            rendered.contains(&format!("- 状态：{}", item.status.as_str())),
            "{} 必须写出状态",
            item.name
        );
        assert!(
            rendered.contains(&format!("- 形态：{}", item.form_factor.as_str())),
            "{} 必须写出形态",
            item.name
        );
    }
    assert!(
        rendered.contains("- 未测原因：无 Web 宿主"),
        "未测项必须带原因"
    );
    // 表格里三项状态齐全，未测的显示"未测"而不是空白或 0。
    assert!(
        rendered.contains("| `state_to_web_visible_p95` | web | ≤ 2 s | 未测 | 0 | Unverified |")
    );

    // 有差异项时必须写出来，不能默认说"条件一致"。
    let with_diff = render_markdown(&report, &["sqlite_sync"]);
    assert!(with_diff.contains("与上一轮差异项：sqlite_sync"));
    assert!(!with_diff.contains("无（条件一致，可比较）"));
}
