//! P7 §2（2.1—2.5）的漂移检查：归因口径、实测项与手册必须一致。

use std::collections::BTreeSet;

const DELTA: &str = include_str!(
    "../../../openspec/changes/p7-capacity-evaluation/specs/capacity-governance/spec.md"
);
const NOTE: &str = include_str!("../../../docs/spec-notes/p7-measurement.md");
const PROBE: &str = include_str!("../src/perf_probe.rs");
const MODULE: &str = include_str!("../src/bottleneck.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p7-capacity-evaluation/tasks.md");

use bod_app::bottleneck::{
    ContentionSample, ContentionVerdict, Pressure, RecoveryPhase, RecoveryReading, RecoveryVerdict,
    SlotReading, SlotVerdict, VolumeReading, classify_contention, classify_recovery,
    classify_slots, classify_volume, signals_from,
};
use bod_app::capacity_evaluation::{Graduation, GraduationSignal, graduation};

fn note_section(heading: &str) -> &'static str {
    NOTE.split(heading)
        .nth(1)
        .unwrap_or_else(|| panic!("手册必须有 `{heading}` 小节"))
        .split("\n## ")
        .next()
        .unwrap_or_default()
}

#[test]
fn task_2_1_recovery_items_expose_external_and_internal_phases() {
    assert!(
        PROBE.contains("recovery_rto_100k_events"),
        "要有独立的恢复 RTO 测量项"
    );
    assert!(
        PROBE.contains("external: true") && PROBE.contains("external: false"),
        "恢复阶段必须区分外部与内部"
    );
    assert!(
        PROBE.contains("quick_check"),
        "恢复后必须有完整性校验，否则 RTO 只算到打开文件"
    );
    let section = note_section("### 4.2 恢复 RTO 必须拆成外部与内部");
    assert!(section.contains("Unverified"), "手册要写明缺数据时的结论");
    assert!(section.contains("外部") && section.contains("内部"));
}

#[test]
fn task_2_2_contention_item_separates_busy_from_queueing() {
    assert!(PROBE.contains("write_contention_p99_growth"));
    assert!(
        PROBE.contains("DatabaseBusy"),
        "要真的数 Busy，而不是看时延猜"
    );
    assert!(
        PROBE.contains("busy_timeout"),
        "要写出 busy_timeout 口径，否则 Busy=0 无法解释"
    );
    let section = note_section("### 4.3 单写争用要同时看 Busy 与时延");
    for must in [
        "WriteContention",
        "QueueingWithoutBusy",
        "NoContention",
        "Unknown",
    ] {
        assert!(section.contains(must), "手册要写清 {must} 的判定");
    }
    assert!(
        section.contains("不能证明没有争用"),
        "手册必须写明 Busy=0 不等于无争用"
    );
}

#[test]
fn task_2_3_volume_and_index_space_have_separate_items() {
    for item in [
        "table_index_volume_100k_events",
        "index_rebuild_cost_100k_events",
        "indexed_vs_unindexed_lookup",
    ] {
        assert!(PROBE.contains(item), "缺少体积/索引测量项 {item}");
        assert!(NOTE.contains(item), "手册缺少 {item} 的口径");
    }
    assert!(
        PROBE.contains("REINDEX"),
        "重建成本必须真的执行重建，不能拿估算代替"
    );
    assert!(
        PROBE.contains("dbstat"),
        "表/索引分别占用要取 dbstat，拿不到就记 unknown"
    );
    // aggregate_id 有 UNIQUE 自动索引：不能拿它当"未索引"对照。
    assert!(
        PROBE.contains("UNIQUE(aggregate_id, aggregate_version)") || PROBE.contains("自动索引"),
        "实现里要说明为什么不能用 aggregate_id 做未索引对照"
    );
}

#[test]
fn task_2_4_slot_items_and_guards_exist() {
    assert!(PROBE.contains("executor_slot_capacity"));
    assert!(PROBE.contains("agent_slot_occupancy"));
    assert!(
        PROBE.contains("agent_processes_observed: None"),
        "没有真实 Agent 进程时必须如实留 None"
    );
    assert!(MODULE.contains("pub fn agent_slot_claim"));
    assert!(MODULE.contains("pub fn guard_replicate_side_effect_package"));
    assert!(note_section("### 4.5").contains("复制有副作用工作包"));
}

#[test]
fn task_2_5_four_signals_are_aggregated_and_event_count_is_not_a_signal() {
    assert!(MODULE.contains("pub fn signals_from"));
    // 四类信号必须齐备且顺序稳定。
    let reading = VolumeReading {
        db_bytes: 15_000_000,
        event_rows: 100_000,
        index_bytes: Some(1_000_000),
        rebuild_ms: 300.0,
        unindexed_lookup_us: 15_000,
        indexed_lookup_us: 7,
    };
    let assessment = classify_volume(&reading, 256 * 1024 * 1024, 10_000.0);
    let signals = signals_from(
        &ContentionVerdict::NoContention {
            baseline_p99_us: 1_000,
            worst_p99_us: 1_050,
            growth: 0.05,
        },
        &RecoveryVerdict::WithinTarget {
            total_ms: 80.0,
            external_ms: 20.0,
            internal_ms: 60.0,
        },
        &reading,
        &assessment,
        &SlotVerdict::WritePathBound { gain: 0.1 },
    );
    assert_eq!(
        signals
            .iter()
            .map(|signal| signal.signal)
            .collect::<Vec<_>>(),
        vec![
            GraduationSignal::RecoveryRto,
            GraduationSignal::WriteContention,
            GraduationSignal::VolumePressure,
            GraduationSignal::MultiHostDemand,
        ]
    );
    assert_eq!(
        signals[3].met, None,
        "多机写入需求在单机 bench 里没有观测来源，必须 unknown"
    );

    match graduation(&signals, Some(5_000_000)) {
        Graduation::NoMigration { reasons, .. } => {
            assert!(reasons.iter().any(|line| line.contains("日均事件数")));
        }
        other => panic!("三绿一 unknown 不应迁移，实得 {other:?}"),
    }
}

#[test]
fn task_2_1_to_2_4_missing_samples_never_produce_a_healthy_verdict() {
    // 恢复：只有合并数字 → Unverified。
    assert!(matches!(
        classify_recovery(
            &RecoveryReading::new(vec![RecoveryPhase {
                name: "total",
                millis: 100.0,
                external: false,
            }]),
            60_000.0
        ),
        RecoveryVerdict::Unverified { .. }
    ));

    // 争用：只有单写样本 → Unknown。
    assert!(matches!(
        classify_contention(&[ContentionSample {
            writers: 1,
            transactions: 100,
            p99_us: 1_000,
            busy_errors: 0,
        }]),
        ContentionVerdict::Unknown { .. }
    ));

    // 体积：全缺 → 三个判断都是 Unknown。
    let empty = VolumeReading {
        db_bytes: 0,
        event_rows: 0,
        index_bytes: None,
        rebuild_ms: 0.0,
        unindexed_lookup_us: 0,
        indexed_lookup_us: 0,
    };
    let assessment = classify_volume(&empty, 256 * 1024 * 1024, 10_000.0);
    assert!(matches!(assessment.size, Pressure::Unknown { .. }));
    assert!(matches!(assessment.rebuild, Pressure::Unknown { .. }));

    // 槽位：没有吞吐样本 → Unknown。
    assert!(matches!(
        classify_slots(&SlotReading {
            single_writer_per_sec: 0.0,
            saturated_per_sec: 0.0,
            agent_processes_observed: None,
        }),
        SlotVerdict::Unknown { .. }
    ));
}

#[test]
fn governance_delta_requirements_are_addressed() {
    let mut requirements = Vec::new();
    for line in DELTA.lines() {
        if let Some(rest) = line.strip_prefix("### Requirement: ") {
            requirements.push(rest.trim().to_string());
        }
    }
    assert_eq!(requirements.len(), 6, "治理规格应有 6 条 Requirement");
    for requirement in &requirements {
        assert!(
            NOTE.contains(requirement.as_str()),
            "手册未覆盖治理需求 `{requirement}`"
        );
    }
}

#[test]
fn tasks_2_x_are_checked_with_evidence() {
    let section = TASKS
        .split("## 2. 瓶颈定位")
        .nth(1)
        .expect("tasks 必须有 §2 瓶颈定位")
        .split("## 3.")
        .next()
        .unwrap_or_default();
    let checked: BTreeSet<String> = section
        .lines()
        .filter(|line| line.trim_start().starts_with("- [x]"))
        .filter_map(|line| line.split_whitespace().nth(2).map(|item| item.to_string()))
        .collect();
    for id in ["2.1", "2.2", "2.3", "2.4", "2.5"] {
        assert!(checked.contains(id), "tasks 的 {id} 尚未勾选");
    }
    assert!(
        section.contains("docs/spec-notes/p7-measurement.md") || section.contains("bottleneck"),
        "tasks 的 2.x 要指向实现与手册"
    );
}
