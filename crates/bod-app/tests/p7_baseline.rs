//! P7 §1（1.1—1.5）的漂移检查：实测项清单、报告渲染、手册与规格必须一致。

use std::collections::BTreeSet;

const DELTA: &str = include_str!(
    "../../../openspec/changes/p7-capacity-evaluation/specs/performance-baseline/spec.md"
);
const NOTE: &str = include_str!("../../../docs/spec-notes/p7-measurement.md");
const PROBE: &str = include_str!("../src/perf_probe.rs");
const MODULE: &str = include_str!("../src/capacity_evaluation.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p7-capacity-evaluation/tasks.md");
const AGENTS: &str = include_str!("../../../AGENTS.md");

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

/// 手册 §3 表格里列出的测量项名。
fn note_item_names() -> Vec<String> {
    let section = NOTE
        .split("## 3. 测量项与口径")
        .nth(1)
        .expect("手册必须有 §3 测量项与口径")
        .split("\n## ")
        .next()
        .unwrap_or_default();
    section
        .lines()
        .filter_map(|line| {
            let cell = line.trim_start().strip_prefix("| `")?;
            let end = cell.find('`')?;
            Some(cell[..end].to_string())
        })
        .collect()
}

/// `perf_probe.rs` 里作为测量项登记的名字。
fn probe_item_names() -> BTreeSet<String> {
    PROBE
        .match_indices("MeasureItem::new(")
        .filter_map(|(index, _)| {
            let rest = &PROBE[index..];
            let start = rest.find('"')? + 1;
            let end = rest[start..].find('"')?;
            Some(rest[start..start + end].to_string())
        })
        .collect()
}

/// 手册 §3 表格每一行的目标单元格。
fn note_target_cells() -> Vec<String> {
    let section = NOTE
        .split("## 3. 测量项与口径")
        .nth(1)
        .expect("手册必须有 §3")
        .split("\n## ")
        .next()
        .unwrap_or_default();
    section
        .lines()
        .filter(|line| line.trim_start().starts_with("| `"))
        .filter_map(|line| line.split('|').nth(3).map(|cell| cell.trim().to_string()))
        .collect()
}

#[test]
fn task_1_1_measurement_conditions_cover_every_required_field() {
    // 规格要求的六类条件都要有对应字段，否则复现对照做不出来。
    for field in [
        "OS",
        "CPU",
        "rustc",
        "构建参数",
        "SQLite 同步策略",
        "数据规模",
        "缓存冷热",
    ] {
        assert!(
            NOTE.contains(field),
            "测量手册必须记录 {field}，否则报告不可复现"
        );
    }
    for field in [
        "pub os: String",
        "pub cpu: String",
        "pub rustc: String",
        "pub build_params: String",
        "pub sqlite_sync: String",
        "pub data_scale: String",
        "pub cache_state: String",
    ] {
        assert!(MODULE.contains(field), "EnvironmentRecord 缺字段：{field}");
    }
    assert!(
        MODULE.contains("pub fn diff(&self, other: &Self) -> Vec<&'static str>"),
        "必须有逐字段差异对照",
    );
    assert!(
        NOTE.contains("差异项会写进 Markdown 报告抬头"),
        "手册要说明差异项落到哪里"
    );
}

#[test]
fn task_1_2_manual_declares_per_form_factor_reporting() {
    for form in FormFactor::ALL {
        assert!(
            NOTE.contains(form.as_str()),
            "手册必须给出 {} 形态的说明",
            form.as_str()
        );
    }
    assert!(
        NOTE.contains("不得用 headless 空闲 RSS 代替桌面总内存")
            || NOTE.contains("不得**用 headless 空闲 RSS 代替桌面总内存"),
        "手册必须写明 headless 与桌面不可互替"
    );
    assert!(
        MODULE.contains("pub fn guard_form_factor"),
        "代码层面要有形态守卫"
    );
    assert!(
        MODULE.contains("pub fn may_claim_form_factor"),
        "按形态宣称达标要有专门入口"
    );
}

#[test]
fn task_1_3_manual_inventory_matches_probe_inventory() {
    let note: BTreeSet<String> = note_item_names().into_iter().collect();
    let probe = probe_item_names();
    assert!(!note.is_empty(), "手册 §3 必须列出测量项");
    assert!(!probe.is_empty(), "perf_probe 必须登记测量项");

    let missing_in_manual: Vec<&String> = probe.difference(&note).collect();
    let missing_in_probe: Vec<&String> = note.difference(&probe).collect();
    assert!(
        missing_in_manual.is_empty(),
        "代码里有但手册没写的测量项：{missing_in_manual:?}"
    );
    assert!(
        missing_in_probe.is_empty(),
        "手册里写了但代码没实现的测量项：{missing_in_probe:?}"
    );

    // 规格点名的八类口径都必须有对应项。
    for required in [
        "idle_rss_headless",
        "cold_start_store_ready",
        "key_transaction_latency",
        "event_throughput_batched",
        "queue_selection_1000",
        "state_to_web_visible_p95",
        "restart_after_100k_events",
        "package_size_headless",
    ] {
        assert!(probe.contains(required), "基线缺 {required}");
    }

    // 吞吐与尾延迟必须是两个口径，并有专门说明。
    assert!(probe.contains("event_throughput_durable"));
    assert!(
        NOTE.contains("吞吐与尾延迟分别结论"),
        "手册要写明吞吐与尾延迟分开结论"
    );
}

#[test]
fn task_1_4_every_target_in_manual_is_a_pending_measurement_not_a_promise() {
    let cells = note_target_cells();
    assert!(!cells.is_empty(), "手册 §3 目标列不能为空");
    for cell in &cells {
        let numeric = cell.contains('≤') || cell.contains('≥');
        let none_target = cell.contains("无冻结目标") || cell.contains("无冻结数值");
        assert!(
            numeric || none_target,
            "目标单元格 `{cell}` 既没有数值目标也没有声明「无冻结目标」"
        );
        assert!(
            !cell.contains("已达标") && !cell.contains("承诺"),
            "手册里的目标不得写成已达成的承诺：{cell}"
        );
    }
    // 表格上方必须声明目标列全部是 Target/待实测。
    assert!(
        NOTE.contains("Target（待实测确认）"),
        "手册要把目标列整体标为 Target/待实测"
    );
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

#[test]
fn task_1_5_manual_is_linked_from_tasks_and_agents() {
    assert!(
        TASKS.contains("docs/spec-notes/p7-measurement.md"),
        "tasks 的 1.5 必须指向本手册"
    );
    assert!(
        AGENTS.contains("p7-measurement.md"),
        "AGENTS.md 要登记 P7 测量手册的位置"
    );
    for heading in [
        "## 1. 命令",
        "## 2. 测量条件",
        "## 3. 测量项与口径",
        "## 4. 瓶颈归因口径",
        "## 5. 分形态报告",
        "## 6. 报告归档",
        "## 7. 未取得的部分",
    ] {
        assert!(
            NOTE.contains(heading),
            "手册缺少小节 `{heading}`：复现者会找不到口径"
        );
    }
}

#[test]
fn spec_delta_requirements_are_all_addressed_in_manual() {
    // 规格里的 6 条 Requirement 都要在手册里有落点，不能只实现一半。
    let mut requirements = Vec::new();
    for line in DELTA.lines() {
        if let Some(rest) = line.strip_prefix("### Requirement: ") {
            requirements.push(rest.trim().to_string());
        }
    }
    assert_eq!(requirements.len(), 6, "规格应有 6 条 Requirement");
    for requirement in &requirements {
        assert!(
            NOTE.contains(requirement.as_str()),
            "手册未覆盖需求 `{requirement}`"
        );
    }
}
