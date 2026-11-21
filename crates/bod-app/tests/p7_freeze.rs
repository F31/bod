//! P7 §4（4.1—4.2）的漂移检查：两阶段对比只把「都测到 + 在容差内」的项转 Measured，
//! 冻结预算必须先定位超标项的默认依赖与结构，且不得靠降低同步/事务/授权/门禁换数字。

use bod_app::capacity_evaluation::{
    BaselineReport, BudgetScope, DriftAttribution, DriftCause, EngineChange, EnvironmentRecord,
    FormFactor, MeasureItem, MeasureStatus, PHASE_TOLERANCE_PCT, PhaseComparison, compare_phases,
    freeze_budget, freeze_with_attribution, render_frozen_budget, render_phase_comparison,
};

/// 环境一致（除 `data_scale` 外全部固定），用于判定「可比」。
fn environment() -> EnvironmentRecord {
    EnvironmentRecord {
        note: "windows / x86_64 / rustc 1.96.0".to_string(),
        os: "windows".to_string(),
        cpu: "16 逻辑核".to_string(),
        rustc: "rustc 1.96.0".to_string(),
        build_params: "dev profile, debug=0".to_string(),
        sqlite_sync: "WAL + synchronous=FULL + BEGIN IMMEDIATE".to_string(),
        data_scale: "200 单条事务 + 5000 批量事件 + 100000 事件重启".to_string(),
        cache_state: "冷".to_string(),
    }
}

/// 一条已测项（有样本、有命令、有标量）。
fn measured_item(name: &'static str, value: f64) -> MeasureItem {
    MeasureItem::new(name, FormFactor::Headless, "≤ 10 ms")
        .measured(format!("{value:.3}"), 3, "xtask bench", Some(value <= 10.0))
        .with_numeric(value)
}

/// 一条只有目标的项（无样本、无命令）。
fn target_item(name: &'static str) -> MeasureItem {
    MeasureItem::new(name, FormFactor::Headless, "≤ 60 s")
}

fn report(items: Vec<MeasureItem>, commit: &str) -> BaselineReport {
    BaselineReport {
        environment: environment(),
        commit: commit.to_string(),
        items,
    }
}

fn two_phases(first: Vec<MeasureItem>, second: Vec<MeasureItem>) -> PhaseComparison {
    compare_phases(
        &report(first, "phase-1"),
        &report(second, "phase-2"),
        PHASE_TOLERANCE_PCT,
    )
}

fn scope() -> BudgetScope {
    BudgetScope::new(
        "p7-budget-v1",
        FormFactor::Headless,
        "windows / x86_64 / rustc 1.96.0",
        environment().data_scale,
        PHASE_TOLERANCE_PCT,
    )
}

fn located(name: &'static str) -> DriftAttribution {
    DriftAttribution::new(
        name,
        vec![DriftCause::Structure {
            name: "逐条 fsync".to_string(),
            evidence: "event_throughput_durable 1167 vs batched 61618 events/s".to_string(),
        }],
    )
}

/// 4.1：只有两阶段都测到且在容差内的项才转 Measured。
#[test]
fn task_4_1_only_stable_both_phase_items_become_measured() {
    let comparison = two_phases(
        vec![
            measured_item("key_transaction_latency", 1.741),
            measured_item("recovery_rto_100k_events", 60.785),
        ],
        vec![
            measured_item("key_transaction_latency", 1.730),
            measured_item("recovery_rto_100k_events", 102.527),
        ],
    );

    assert_eq!(
        comparison.promotable(),
        vec!["key_transaction_latency"],
        "漂移 -0.6% 的项可转 Measured"
    );
    assert_eq!(
        comparison.unresolved(),
        vec!["recovery_rto_100k_events"],
        "漂移 +68.7% 的项必须保留原状态"
    );
    assert_eq!(
        comparison.stage(),
        bod_app::capacity_evaluation::BudgetStage::TwoComparablePhases {
            diff: vec!["recovery_rto_100k_events"]
        }
    );
    assert!(
        freeze_budget(&comparison.stage()).is_err(),
        "存在超容差项时不得冻结"
    );
}

/// 4.1：容差边界是闭区间；刚好 25% 算稳，25.1% 不算。
/// 取 4.0 → 5.0 是因为这个比值在二进制浮点里**恰好**是 0.25（×100 得 25.0），
/// 换成 0.100 → 0.125 会得到 24.999…%，测不出「开区间」这种错。
#[test]
fn task_4_1_tolerance_boundary_is_inclusive() {
    let at_edge = two_phases(
        vec![measured_item("idle_tick_cost", 4.0)],
        vec![measured_item("idle_tick_cost", 5.0)],
    );
    assert_eq!(at_edge.promotable(), vec!["idle_tick_cost"]);
    assert_eq!(at_edge.unresolved(), Vec::<&'static str>::new());

    let over_edge = two_phases(
        vec![measured_item("idle_tick_cost", 4.0)],
        vec![measured_item("idle_tick_cost", 5.004)],
    );
    assert!(over_edge.promotable().is_empty());
    assert_eq!(over_edge.unresolved(), vec!["idle_tick_cost"]);
}

/// 4.1：环境或数据规模不同的两轮不可比，任何项都不得转 Measured。
#[test]
fn task_4_1_incomparable_phases_promote_nothing() {
    let first = report(
        vec![measured_item("key_transaction_latency", 1.741)],
        "phase-1",
    );
    let mut other = environment();
    other.data_scale = "1 万事件".to_string();
    let second = BaselineReport {
        environment: other,
        commit: "phase-2".to_string(),
        items: vec![measured_item("key_transaction_latency", 1.730)],
    };

    let comparison = compare_phases(&first, &second, PHASE_TOLERANCE_PCT);
    assert!(matches!(comparison, PhaseComparison::Incomparable { .. }));
    assert!(
        comparison.promotable().is_empty(),
        "不可比的两轮不能把任何项转 Measured"
    );
    assert!(freeze_budget(&comparison.stage()).is_err());

    let rendered = render_phase_comparison(&comparison, &first, &second, PHASE_TOLERANCE_PCT);
    assert!(rendered.contains("不可比，不得冻结"), "{rendered}");
    assert!(rendered.contains("data_scale"), "{rendered}");
}

/// 4.1：只在一阶段出现或没有标量的项，不参与漂移，也不得转 Measured。
#[test]
fn task_4_1_single_phase_or_scalarless_items_are_never_promoted() {
    let comparison = two_phases(
        vec![
            measured_item("key_transaction_latency", 1.741),
            target_item("cold_start_to_api_ready"),
        ],
        vec![
            measured_item("key_transaction_latency", 1.730),
            target_item("cold_start_to_api_ready"),
            measured_item("executor_slot_capacity", 12.0),
        ],
    );

    assert_eq!(comparison.promotable(), vec!["key_transaction_latency"]);
    let unresolved = comparison.unresolved();
    assert!(
        unresolved.contains(&"cold_start_to_api_ready"),
        "{unresolved:?}"
    );
    assert!(
        unresolved.contains(&"executor_slot_capacity"),
        "{unresolved:?}"
    );

    let PhaseComparison::Comparable { single_phase, .. } = &comparison else {
        panic!("同环境同规模必须判可比")
    };
    assert!(single_phase.contains(&"cold_start_to_api_ready"));
    assert!(single_phase.contains(&"executor_slot_capacity"));
}

/// 4.1：数据为零的项不能算出 ±∞ 漂移后当成「在容差内」。
#[test]
fn task_4_1_zero_baseline_drift_is_not_within_tolerance() {
    let comparison = two_phases(
        vec![measured_item("idle_tick_cost", 0.0)],
        vec![measured_item("idle_tick_cost", 0.094)],
    );
    assert!(comparison.promotable().is_empty());
    assert_eq!(comparison.unresolved(), vec!["idle_tick_cost"]);
}

/// 4.2：每个未进预算的项都必须先定位默认依赖或结构，否则不许冻结。
#[test]
fn task_4_2_freezing_requires_located_causes_for_every_excluded_item() {
    let first = report(
        vec![
            measured_item("key_transaction_latency", 1.741),
            measured_item("recovery_rto_100k_events", 60.785),
        ],
        "phase-1",
    );
    let second = report(
        vec![
            measured_item("key_transaction_latency", 1.730),
            measured_item("recovery_rto_100k_events", 102.527),
        ],
        "phase-2",
    );
    let comparison = compare_phases(&first, &second, PHASE_TOLERANCE_PCT);

    let missing = freeze_with_attribution(&comparison, &second, scope(), &[], &[]);
    let reason = missing.expect_err("未定位时不得冻结");
    assert!(reason.contains("recovery_rto_100k_events"), "{reason}");
    assert!(reason.contains("定位"), "{reason}");

    let unknown_only = vec![DriftAttribution::new(
        "recovery_rto_100k_events",
        vec![DriftCause::Unknown],
    )];
    assert!(
        freeze_with_attribution(&comparison, &second, scope(), &unknown_only, &[]).is_err(),
        "`Unknown` 不算定位"
    );

    let budget = freeze_with_attribution(
        &comparison,
        &second,
        scope(),
        &[located("recovery_rto_100k_events")],
        &[],
    )
    .expect("定位后可以冻结稳定项");
    assert_eq!(budget.items.len(), 1);
    assert_eq!(budget.items[0].name, "key_transaction_latency");
    assert_eq!(budget.excluded, vec!["recovery_rto_100k_events"]);
    assert_eq!(budget.attributions.len(), 1, "排除项必须带定位结论");
}

/// 4.2：冻结靠降低 SQLite 同步 / 放宽事务 / 跳授权或门禁换来的数字，一律阻断。
#[test]
fn task_4_2_relaxing_sync_or_gates_blocks_the_freeze() {
    let first = report(
        vec![measured_item("key_transaction_latency", 1.741)],
        "phase-1",
    );
    let second = report(
        vec![measured_item("key_transaction_latency", 1.730)],
        "phase-2",
    );
    let comparison = compare_phases(&first, &second, PHASE_TOLERANCE_PCT);

    for change in [
        EngineChange::LowerSyncLevel {
            level: "synchronous=NORMAL".to_string(),
        },
        EngineChange::WidenTransaction,
        EngineChange::SkipGate {
            gate: "G-26 支持矩阵".to_string(),
        },
        EngineChange::SwitchEngine {
            target: "postgres".to_string(),
        },
    ] {
        let result = freeze_with_attribution(
            &comparison,
            &second,
            scope(),
            &[],
            std::slice::from_ref(&change),
        );
        let reason = result.expect_err("降低正确性的路径不得冻结");
        assert!(reason.contains("阻断"), "{change:?} → {reason}");
    }
}

/// 4.2：冻结记录必须写明版本、形态、环境、规模与排除项的复测条件。
#[test]
fn task_4_2_frozen_record_carries_version_scope_and_retest_conditions() {
    let first = report(
        vec![
            measured_item("key_transaction_latency", 1.741),
            measured_item("queue_selection_1000", 2.137),
        ],
        "phase-1",
    );
    let second = report(
        vec![
            measured_item("key_transaction_latency", 1.730),
            measured_item("queue_selection_1000", 3.700),
        ],
        "phase-2",
    );
    let comparison = compare_phases(&first, &second, PHASE_TOLERANCE_PCT);

    let budget = freeze_with_attribution(
        &comparison,
        &second,
        scope(),
        &[DriftAttribution::new(
            "queue_selection_1000",
            vec![
                DriftCause::DefaultDependency {
                    dependency: "busy_timeout".to_string(),
                    evidence: "BodConfig::default() 取 500 ms".to_string(),
                },
                DriftCause::Structure {
                    name: "就绪候选全表扫".to_string(),
                    evidence: "queue_selection_1000 无覆盖索引".to_string(),
                },
            ],
        )],
        &[],
    )
    .expect("稳定项可冻结");

    let rendered = render_frozen_budget(&budget);
    for expected in [
        "p7-budget-v1",
        "headless",
        "windows / x86_64 / rustc 1.96.0",
        "200 单条事务 + 5000 批量事件 + 100000 事件重启",
        "±25%",
        "`key_transaction_latency`",
        "`queue_selection_1000`",
        "默认依赖 `busy_timeout`",
        "结构「就绪候选全表扫」",
        "## 排除项与复测条件",
        "同形态同规模连跑两轮",
    ] {
        assert!(rendered.contains(expected), "缺少 {expected}：\n{rendered}");
    }
    assert!(
        budget.item("queue_selection_1000").is_none(),
        "超容差项不得进预算"
    );
    assert_eq!(
        budget
            .item("key_transaction_latency")
            .map(|item| item.value),
        Some(1.730),
        "冻结值取第二阶段实测"
    );
}

/// 4.2：两轮全部超容差时没有预算可冻结，不得产出「空预算 + 已冻结」。
#[test]
fn task_4_2_no_stable_item_means_no_budget() {
    let first = report(vec![measured_item("idle_tick_cost", 0.178)], "phase-1");
    let second = report(vec![measured_item("idle_tick_cost", 0.094)], "phase-2");
    let comparison = compare_phases(&first, &second, PHASE_TOLERANCE_PCT);

    let reason = freeze_with_attribution(
        &comparison,
        &second,
        scope(),
        &[located("idle_tick_cost")],
        &[],
    )
    .expect_err("没有稳定项时无预算可冻结");
    assert!(reason.contains("无预算可冻结"), "{reason}");
}

/// 4.1/4.2：未测项（Target）不得因两阶段都「存在」而被当成稳定项。
#[test]
fn task_4_1_target_items_never_become_measured_by_repetition() {
    let item = target_item("cold_start_to_api_ready");
    assert_eq!(item.status, MeasureStatus::Target);

    let comparison = two_phases(
        vec![measured_item("key_transaction_latency", 1.741)],
        vec![measured_item("key_transaction_latency", 1.730)],
    );
    assert!(!comparison.promotable().contains(&"cold_start_to_api_ready"));
}
