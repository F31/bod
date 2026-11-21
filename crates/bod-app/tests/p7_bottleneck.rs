//! P7 §2（2.1—2.5）的漂移检查：归因口径、实测项与手册必须一致。

use std::collections::BTreeSet;

const MODULE: &str = include_str!("../src/bottleneck.rs");
const TASKS: &str = include_str!("../../../openspec/changes/p7-capacity-evaluation/tasks.md");

use bod_app::bottleneck::{
    ContentionSample, ContentionVerdict, Pressure, RecoveryPhase, RecoveryReading, RecoveryVerdict,
    SlotReading, SlotVerdict, VolumeReading, classify_contention, classify_recovery,
    classify_slots, classify_volume, signals_from,
};
use bod_app::capacity_evaluation::{Graduation, GraduationSignal, graduation};

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
