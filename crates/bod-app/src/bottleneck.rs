//! P7 §2：瓶颈定位与归因（`capacity-governance` 的「瓶颈定位」与「毕业信号」）。
//!
//! 这一层只做**归因判断**：把实测到的数字翻译成「哪一类毕业信号被触发」，并拦住三类
//! 常见误判：
//! 1. **p99 升高不等于单写争用**：没有 Busy、也没有写等待上升时，只能记 unknown，
//!    不能归因为写锁争用（2.2）。
//! 2. **总 RTO 不等于内部恢复能力**：外部复制/搬运耗时必须单独列出，否则 RTO 数字
//!    换块盘就变，无法比较（2.1）。
//! 3. **体积压力不等于索引可优化**：前者看库大小与重建成本，后者看有/无索引的查询
//!    差值，两件事分开给数据（2.3）。
//!
//! 另外两条不许越界：执行槽位扩容不得复制有副作用的工作包（2.4）；任何结论都不能只靠
//! 日均事件数或单一 RTO 指标推出（2.5，判定本身在 `capacity_evaluation::graduation`）。

use crate::capacity_evaluation::{GraduationSignal, SignalReading};

// ---------------------------------------------------------------------------
// 2.1 恢复 RTO：区分外部恢复与内部恢复
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryPhase {
    pub name: &'static str,
    pub millis: f64,
    /// `true` 表示这一步花在宿主之外（复制备份、搬运介质、等待外部存储）。
    pub external: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecoveryReading {
    pub phases: Vec<RecoveryPhase>,
}

impl RecoveryReading {
    pub fn new(phases: Vec<RecoveryPhase>) -> Self {
        Self { phases }
    }

    pub fn total_ms(&self) -> f64 {
        self.phases.iter().map(|phase| phase.millis).sum()
    }

    pub fn external_ms(&self) -> f64 {
        self.phases
            .iter()
            .filter(|phase| phase.external)
            .map(|phase| phase.millis)
            .sum()
    }

    pub fn internal_ms(&self) -> f64 {
        self.phases
            .iter()
            .filter(|phase| !phase.external)
            .map(|phase| phase.millis)
            .sum()
    }

    /// 外部耗时占比：换盘/换机器后波动最大的部分。
    pub fn external_share(&self) -> f64 {
        let total = self.total_ms();
        if total <= 0.0 {
            0.0
        } else {
            self.external_ms() / total
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryVerdict {
    WithinTarget {
        total_ms: f64,
        external_ms: f64,
        internal_ms: f64,
    },
    Exceeded {
        total_ms: f64,
        external_ms: f64,
        internal_ms: f64,
        over_ms: f64,
    },
    /// 没有把外部与内部分开，或没有样本：不给结论。
    Unverified { reason: &'static str },
}

/// 判 RTO。**没有外部阶段或没有内部阶段一律 Unverified**：合并数字不能拿来比较。
pub fn classify_recovery(reading: &RecoveryReading, target_ms: f64) -> RecoveryVerdict {
    if reading.phases.is_empty() {
        return RecoveryVerdict::Unverified {
            reason: "没有任何恢复阶段样本",
        };
    }
    if reading.external_ms() <= 0.0 {
        return RecoveryVerdict::Unverified {
            reason: "未计入外部恢复耗时：RTO 无法与换盘/换机后的结果比较",
        };
    }
    if reading.internal_ms() <= 0.0 {
        return RecoveryVerdict::Unverified {
            reason: "未计入内部恢复耗时：无法判断是宿主慢还是搬运慢",
        };
    }
    let total_ms = reading.total_ms();
    let external_ms = reading.external_ms();
    let internal_ms = reading.internal_ms();
    if total_ms <= target_ms {
        RecoveryVerdict::WithinTarget {
            total_ms,
            external_ms,
            internal_ms,
        }
    } else {
        RecoveryVerdict::Exceeded {
            total_ms,
            external_ms,
            internal_ms,
            over_ms: total_ms - target_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// 2.2 单写争用：Busy 与写等待必须一起看
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ContentionSample {
    /// 并发写者数（1 = 单写基线）。
    pub writers: usize,
    pub transactions: usize,
    pub p99_us: u128,
    /// 因拿不到写锁而失败的事务数（Busy / SQLITE_BUSY）。
    pub busy_errors: u64,
}

/// p99 增长在此比例内视为噪声，不算争用。
pub const NOISE_GROWTH: f64 = 0.25;

#[derive(Debug, Clone, PartialEq)]
pub enum ContentionVerdict {
    WriteContention {
        baseline_p99_us: u128,
        worst_p99_us: u128,
        busy_errors: u64,
        growth: f64,
    },
    /// p99 涨了但一个 Busy 都没有：不能归因为写锁争用。
    QueueingWithoutBusy {
        baseline_p99_us: u128,
        worst_p99_us: u128,
        growth: f64,
        unknown_areas: Vec<&'static str>,
    },
    NoContention {
        baseline_p99_us: u128,
        worst_p99_us: u128,
        growth: f64,
    },
    Unknown {
        reason: &'static str,
    },
}

pub fn classify_contention(samples: &[ContentionSample]) -> ContentionVerdict {
    let Some(baseline) = samples.iter().find(|sample| sample.writers == 1) else {
        return ContentionVerdict::Unknown {
            reason: "缺单写基线：没有 writers=1 的样本",
        };
    };
    let Some(worst) = samples
        .iter()
        .max_by_key(|sample| sample.writers)
        .filter(|sample| sample.writers > 1)
    else {
        return ContentionVerdict::Unknown {
            reason: "缺多写样本：只有单写基线",
        };
    };
    if baseline.p99_us == 0 || baseline.transactions == 0 {
        return ContentionVerdict::Unknown {
            reason: "单写基线没有有效样本",
        };
    }
    let growth = worst.p99_us as f64 / baseline.p99_us as f64 - 1.0;
    let busy_errors: u64 = samples.iter().map(|sample| sample.busy_errors).sum();

    if busy_errors > 0 {
        return ContentionVerdict::WriteContention {
            baseline_p99_us: baseline.p99_us,
            worst_p99_us: worst.p99_us,
            busy_errors,
            growth,
        };
    }
    if growth <= NOISE_GROWTH {
        return ContentionVerdict::NoContention {
            baseline_p99_us: baseline.p99_us,
            worst_p99_us: worst.p99_us,
            growth,
        };
    }
    ContentionVerdict::QueueingWithoutBusy {
        baseline_p99_us: baseline.p99_us,
        worst_p99_us: worst.p99_us,
        growth,
        unknown_areas: vec![
            "fsync 串行化：每次提交都要等落盘",
            "单写者排队：等待写锁的时间未与事务处理时间分离",
            "CPU 调度与页缓存抖动",
        ],
    }
}

// ---------------------------------------------------------------------------
// 2.3 体积与重建成本 / 索引优化空间
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Pressure {
    Over,
    /// 已到预算的 90% 以上：下次复测前必须给出处理方案。
    Near,
    Under,
    Unknown {
        reason: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndexSpace {
    /// 有索引与无索引的查询差了一个数量级以上：还有优化空间。
    Exploitable {
        unindexed_us: u128,
        indexed_us: u128,
        factor: f64,
    },
    NoGap {
        factor: f64,
    },
    Unknown {
        reason: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct VolumeReading {
    pub db_bytes: u64,
    pub event_rows: i64,
    /// 表/索引分别占用；`dbstat` 不可用时为 `None`。
    pub index_bytes: Option<u64>,
    pub rebuild_ms: f64,
    /// 未索引列上的全表扫描耗时。
    pub unindexed_lookup_us: u128,
    /// 走索引的等值查询耗时。
    pub indexed_lookup_us: u128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VolumeAssessment {
    pub size: Pressure,
    pub rebuild: Pressure,
    pub index_space: IndexSpace,
}

/// 体积压力（库大小 + 重建成本）与索引优化空间**分开评**。
pub fn classify_volume(
    reading: &VolumeReading,
    budget_bytes: u64,
    rebuild_budget_ms: f64,
) -> VolumeAssessment {
    let size = if reading.db_bytes == 0 {
        Pressure::Unknown {
            reason: "没有库大小样本",
        }
    } else if reading.db_bytes > budget_bytes {
        Pressure::Over
    } else if reading.db_bytes as f64 > budget_bytes as f64 * 0.9 {
        Pressure::Near
    } else {
        Pressure::Under
    };

    let rebuild = if reading.rebuild_ms <= 0.0 {
        Pressure::Unknown {
            reason: "没有重建耗时样本",
        }
    } else if reading.rebuild_ms > rebuild_budget_ms {
        Pressure::Over
    } else if reading.rebuild_ms > rebuild_budget_ms * 0.9 {
        Pressure::Near
    } else {
        Pressure::Under
    };

    let index_space = if reading.index_space_unknown() {
        IndexSpace::Unknown {
            reason: "索引/表分别占用或查询对比样本缺失",
        }
    } else if reading.indexed_lookup_us == 0 {
        IndexSpace::Unknown {
            reason: "有索引查询没有有效样本",
        }
    } else {
        let factor = reading.unindexed_lookup_us as f64 / reading.indexed_lookup_us as f64;
        if factor >= 10.0 {
            IndexSpace::Exploitable {
                unindexed_us: reading.unindexed_lookup_us,
                indexed_us: reading.indexed_lookup_us,
                factor,
            }
        } else {
            IndexSpace::NoGap { factor }
        }
    };

    VolumeAssessment {
        size,
        rebuild,
        index_space,
    }
}

impl VolumeReading {
    /// 表/索引分别占用是体积压力的必要证据；缺了只能记 unknown。
    pub fn index_space_unknown(&self) -> bool {
        self.index_bytes.is_none() || self.event_rows <= 0
    }
}

// ---------------------------------------------------------------------------
// 2.4 执行槽位与并发容量
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SlotReading {
    /// 单写者时的吞吐（events/s 或 txn/s）。
    pub single_writer_per_sec: f64,
    /// 加到最多写者时的吞吐。
    pub saturated_per_sec: f64,
    /// 观测到的真实 Agent 进程数；bench 里没有 Agent 时应为 `None`。
    pub agent_processes_observed: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SlotVerdict {
    /// 加并发写没有收益：瓶颈在写路径，先优化存储而不是加执行节点。
    WritePathBound {
        gain: f64,
    },
    /// 加并发写有明确收益：执行节点扩展能拿到吞吐。
    ScaleExecutors {
        gain: f64,
    },
    Unknown {
        reason: &'static str,
    },
}

/// 扩容收益阈值：吞吐翻倍以上才算能靠加节点解决。
pub const SCALE_GAIN_THRESHOLD: f64 = 1.0;

pub fn classify_slots(reading: &SlotReading) -> SlotVerdict {
    if reading.single_writer_per_sec <= 0.0 || reading.saturated_per_sec <= 0.0 {
        return SlotVerdict::Unknown {
            reason: "缺单写或并发吞吐样本",
        };
    }
    let gain = reading.saturated_per_sec / reading.single_writer_per_sec - 1.0;
    if gain < SCALE_GAIN_THRESHOLD {
        SlotVerdict::WritePathBound { gain }
    } else {
        SlotVerdict::ScaleExecutors { gain }
    }
}

/// 没有观测到真实 Agent 进程时，不得宣称 Agent 槽位容量。
pub fn agent_slot_claim(observed: Option<usize>) -> Result<usize, &'static str> {
    match observed {
        Some(count) if count > 0 => Ok(count),
        _ => Err("未观测到真实 Agent 进程：不得宣称 Agent 槽位容量或并发上限"),
    }
}

/// 提高吞吐不得靠复制有副作用的工作包：这条永远拒绝。
pub fn guard_replicate_side_effect_package() -> Result<(), &'static str> {
    Err("拒绝复制有副作用的工作包来提吞吐：会二次产生副作用（重复提交、重复扣费）")
}

// ---------------------------------------------------------------------------
// 2.5 汇总四类毕业信号
// ---------------------------------------------------------------------------

/// 把四项归因结果翻成四类毕业信号；缺数据的一律记 unknown（`None`）。
pub fn signals_from(
    contention: &ContentionVerdict,
    recovery: &RecoveryVerdict,
    volume: &VolumeReading,
    assessment: &VolumeAssessment,
    slots: &SlotVerdict,
) -> Vec<SignalReading> {
    let rto = match recovery {
        RecoveryVerdict::Exceeded { total_ms, .. } => SignalReading {
            signal: GraduationSignal::RecoveryRto,
            observed: format!("总 RTO {total_ms:.0} ms（含外部恢复）"),
            threshold: "恢复 RTO 目标值".to_string(),
            met: Some(true),
        },
        RecoveryVerdict::WithinTarget { total_ms, .. } => SignalReading {
            signal: GraduationSignal::RecoveryRto,
            observed: format!("总 RTO {total_ms:.0} ms（含外部恢复）"),
            threshold: "恢复 RTO 目标值".to_string(),
            met: Some(false),
        },
        RecoveryVerdict::Unverified { reason } => SignalReading {
            signal: GraduationSignal::RecoveryRto,
            observed: (*reason).to_string(),
            threshold: "恢复 RTO 目标值".to_string(),
            met: None,
        },
    };

    let write = match contention {
        ContentionVerdict::WriteContention {
            busy_errors,
            growth,
            ..
        } => SignalReading {
            signal: GraduationSignal::WriteContention,
            observed: format!("Busy {busy_errors} 次，p99 增长 {:.0}%", growth * 100.0),
            threshold: "Busy > 0 且 p99 随写者数上升".to_string(),
            met: Some(true),
        },
        ContentionVerdict::NoContention { growth, .. } => SignalReading {
            signal: GraduationSignal::WriteContention,
            observed: format!("无 Busy，p99 增长 {:.0}%（噪声内）", growth * 100.0),
            threshold: "Busy > 0 且 p99 随写者数上升".to_string(),
            met: Some(false),
        },
        ContentionVerdict::QueueingWithoutBusy { growth, .. } => SignalReading {
            signal: GraduationSignal::WriteContention,
            observed: format!(
                "无 Busy，p99 增长 {:.0}%：写者排队（等锁时间未与处理时间分离），不归因为 Busy 争用",
                growth * 100.0
            ),
            threshold: "Busy > 0 且 p99 随写者数上升".to_string(),
            met: None,
        },
        ContentionVerdict::Unknown { reason } => SignalReading {
            signal: GraduationSignal::WriteContention,
            observed: (*reason).to_string(),
            threshold: "Busy > 0 且 p99 随写者数上升".to_string(),
            met: None,
        },
    };

    let volume_signal = {
        let over = matches!(assessment.size, Pressure::Over)
            || matches!(assessment.rebuild, Pressure::Over);
        let unknown = matches!(assessment.size, Pressure::Unknown { .. })
            || matches!(assessment.rebuild, Pressure::Unknown { .. })
            || matches!(assessment.index_space, IndexSpace::Unknown { .. });
        let observed = match (
            &assessment.size,
            &assessment.rebuild,
            &assessment.index_space,
        ) {
            (Pressure::Unknown { reason }, _, _) => (*reason).to_string(),
            (_, Pressure::Unknown { reason }, _) => (*reason).to_string(),
            (_, _, IndexSpace::Unknown { reason }) => (*reason).to_string(),
            _ => format!(
                "库 {} 字节，重建 {:.0} ms，索引空间 {:?}",
                volume.db_bytes, volume.rebuild_ms, assessment.index_space
            ),
        };
        SignalReading {
            signal: GraduationSignal::VolumePressure,
            observed,
            threshold: "超出体积预算或重建预算".to_string(),
            // 越界优先；没有越界但有 unknown 时不下"健康"结论。
            met: if over {
                Some(true)
            } else if unknown {
                None
            } else {
                Some(false)
            },
        }
    };

    let multi_host = SignalReading {
        signal: GraduationSignal::MultiHostDemand,
        observed: match slots {
            SlotVerdict::ScaleExecutors { gain } => {
                format!("多写者吞吐收益 {:.0}%，但多机写入需求未取得", gain * 100.0)
            }
            SlotVerdict::WritePathBound { gain } => {
                format!("多写者吞吐收益 {:.0}%：扩容不能解决写路径", gain * 100.0)
            }
            SlotVerdict::Unknown { reason } => (*reason).to_string(),
        },
        threshold: "确有第二写入者或容灾要求".to_string(),
        met: None,
    };

    vec![rto, write, volume_signal, multi_host]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capacity_evaluation::{Graduation, graduation};

    fn volume_ok() -> VolumeReading {
        VolumeReading {
            db_bytes: 15_060_992,
            event_rows: 100_000,
            index_bytes: Some(1_000_000),
            rebuild_ms: 120.0,
            unindexed_lookup_us: 4_200,
            indexed_lookup_us: 12,
        }
    }

    /// 2.1：总 RTO 必须把外部恢复单独列出，否则不给结论。
    #[test]
    fn task_2_1_recovery_separates_external_from_internal() {
        let reading = RecoveryReading::new(vec![
            RecoveryPhase {
                name: "restore_copy",
                millis: 780.0,
                external: true,
            },
            RecoveryPhase {
                name: "reopen",
                millis: 5.0,
                external: false,
            },
            RecoveryPhase {
                name: "integrity_check",
                millis: 120.0,
                external: false,
            },
        ]);
        assert_eq!(reading.total_ms(), 905.0);
        assert_eq!(reading.external_ms(), 780.0);
        assert_eq!(reading.internal_ms(), 125.0);
        assert!(reading.external_share() > 0.8);

        match classify_recovery(&reading, 3_000.0) {
            RecoveryVerdict::WithinTarget {
                total_ms,
                external_ms,
                internal_ms,
            } => {
                assert_eq!(total_ms, 905.0);
                assert_eq!(external_ms, 780.0);
                assert_eq!(internal_ms, 125.0);
            }
            other => panic!("应达标，实得 {other:?}"),
        }

        // 超标时也要把外部与内部分开写出来。
        let mut slow = reading.clone();
        slow.phases[0].millis = 5_000.0;
        match classify_recovery(&slow, 3_000.0) {
            RecoveryVerdict::Exceeded {
                over_ms,
                external_ms,
                ..
            } => {
                assert!(over_ms > 0.0);
                assert_eq!(external_ms, 5_000.0, "超标的是外部搬运，不是宿主恢复");
            }
            other => panic!("应超标，实得 {other:?}"),
        }

        // 只有合并数字（没有外部/内部区分）→ 不给结论。
        let merged = RecoveryReading::new(vec![RecoveryPhase {
            name: "total",
            millis: 900.0,
            external: false,
        }]);
        assert!(matches!(
            classify_recovery(&merged, 3_000.0),
            RecoveryVerdict::Unverified { .. }
        ));
        assert!(matches!(
            classify_recovery(&RecoveryReading::default(), 3_000.0),
            RecoveryVerdict::Unverified { .. }
        ));

        // 只测了外部搬运、没测内部恢复：同样不给结论。
        let only_external = RecoveryReading::new(vec![RecoveryPhase {
            name: "restore_copy",
            millis: 800.0,
            external: true,
        }]);
        assert!(
            matches!(
                classify_recovery(&only_external, 3_000.0),
                RecoveryVerdict::Unverified { .. }
            ),
            "只有外部耗时时无法区分是宿主慢还是搬运慢"
        );

        // 只测了内部恢复、没测外部搬运：换盘后不可比，也不给结论。
        let only_internal = RecoveryReading::new(vec![RecoveryPhase {
            name: "reopen",
            millis: 5.0,
            external: false,
        }]);
        assert!(matches!(
            classify_recovery(&only_internal, 3_000.0),
            RecoveryVerdict::Unverified { .. }
        ));
    }

    /// 2.2：p99 升高但无 Busy → 不得归因为单写争用。
    #[test]
    fn task_2_2_tail_latency_without_busy_is_not_write_lock_contention() {
        let samples = vec![
            ContentionSample {
                writers: 1,
                transactions: 200,
                p99_us: 1_600,
                busy_errors: 0,
            },
            ContentionSample {
                writers: 4,
                transactions: 200,
                p99_us: 6_400,
                busy_errors: 0,
            },
        ];
        match classify_contention(&samples) {
            ContentionVerdict::QueueingWithoutBusy {
                growth,
                unknown_areas,
                ..
            } => {
                assert!(growth >= 3.0);
                assert!(
                    unknown_areas.iter().any(|area| area.contains("排队")),
                    "要写明哪块还是 unknown"
                );
            }
            other => panic!("无 Busy 不得判写锁争用，实得 {other:?}"),
        }

        // 有 Busy 才是写锁争用。
        let with_busy = vec![
            samples[0].clone(),
            ContentionSample {
                busy_errors: 7,
                ..samples[1].clone()
            },
        ];
        match classify_contention(&with_busy) {
            ContentionVerdict::WriteContention { busy_errors, .. } => assert_eq!(busy_errors, 7),
            other => panic!("有 Busy 应判写争用，实得 {other:?}"),
        }

        // p99 不动：没有争用。
        let flat = vec![
            samples[0].clone(),
            ContentionSample {
                writers: 4,
                transactions: 200,
                p99_us: 1_700,
                busy_errors: 0,
            },
        ];
        assert!(matches!(
            classify_contention(&flat),
            ContentionVerdict::NoContention { .. }
        ));

        // 缺基线或缺多写样本：unknown，不下结论。
        assert!(matches!(
            classify_contention(&samples[1..]),
            ContentionVerdict::Unknown { .. }
        ));
        assert!(matches!(
            classify_contention(&samples[..1]),
            ContentionVerdict::Unknown { .. }
        ));
    }

    /// 2.3：体积压力与索引优化空间分别给数据。
    #[test]
    fn task_2_3_volume_pressure_and_index_headroom_are_separate() {
        let reading = volume_ok();
        let assessment = classify_volume(&reading, 64 * 1024 * 1024, 1_000.0);
        assert_eq!(assessment.size, Pressure::Under);
        assert_eq!(assessment.rebuild, Pressure::Under);
        match assessment.index_space {
            IndexSpace::Exploitable { factor, .. } => assert!(factor >= 10.0),
            other => panic!("无索引扫描慢 350 倍应算有优化空间，实得 {other:?}"),
        }

        // 体积越界、索引没空间：两件事分开报。
        let mut big = reading.clone();
        big.db_bytes = 80 * 1024 * 1024;
        big.rebuild_ms = 2_000.0;
        big.unindexed_lookup_us = 30;
        let oversized = classify_volume(&big, 64 * 1024 * 1024, 1_000.0);
        assert_eq!(oversized.size, Pressure::Over);
        assert_eq!(oversized.rebuild, Pressure::Over);
        assert!(
            matches!(oversized.index_space, IndexSpace::NoGap { .. }),
            "索引已够快时不得再报优化空间"
        );

        // 缺表/索引分别占用：体积记 unknown，不给"健康"结论。
        let mut no_dbstat = reading.clone();
        no_dbstat.index_bytes = None;
        let unknown = classify_volume(&no_dbstat, 64 * 1024 * 1024, 1_000.0);
        assert!(matches!(unknown.index_space, IndexSpace::Unknown { .. }));
        assert!(unknown.size == Pressure::Under, "库大小还是有的");

        // 90%—100% 之间是 Near（要给出处理方案），不能被当成已越界。
        let mut near = reading.clone();
        near.db_bytes = 60 * 1024 * 1024;
        near.rebuild_ms = 950.0;
        let nearing = classify_volume(&near, 64 * 1024 * 1024, 1_000.0);
        assert_eq!(nearing.size, Pressure::Near);
        assert_eq!(nearing.rebuild, Pressure::Near);
    }

    /// 2.4：扩容收益与执行节点需求分别判断；不得复制副作用工作包。
    #[test]
    fn task_2_4_scale_out_is_judged_separately_from_executor_demand() {
        // 加写者基本没收益 → 写路径受限，加执行节点解决不了。
        let write_bound = SlotReading {
            single_writer_per_sec: 1_200.0,
            saturated_per_sec: 1_300.0,
            agent_processes_observed: None,
        };
        match classify_slots(&write_bound) {
            SlotVerdict::WritePathBound { gain } => assert!(gain < SCALE_GAIN_THRESHOLD),
            other => panic!("收益不足应判写路径受限，实得 {other:?}"),
        }

        let scalable = SlotReading {
            single_writer_per_sec: 1_000.0,
            saturated_per_sec: 3_200.0,
            agent_processes_observed: Some(2),
        };
        match classify_slots(&scalable) {
            SlotVerdict::ScaleExecutors { gain } => assert!(gain >= SCALE_GAIN_THRESHOLD),
            other => panic!("吞吐翻两倍应判可扩执行节点，实得 {other:?}"),
        }

        // Agent 槽位：没观测到真实进程就不许宣称容量。
        assert!(agent_slot_claim(None).is_err());
        assert!(agent_slot_claim(Some(0)).is_err());
        assert_eq!(agent_slot_claim(Some(3)), Ok(3));

        assert!(guard_replicate_side_effect_package().is_err());
        assert!(matches!(
            classify_slots(&SlotReading {
                single_writer_per_sec: 0.0,
                saturated_per_sec: 0.0,
                agent_processes_observed: None,
            }),
            SlotVerdict::Unknown { .. }
        ));
    }

    /// 2.5：四类信号汇总后，日均事件数仍不构成迁移理由。
    #[test]
    fn task_2_5_no_migration_from_event_count_or_single_rto() {
        let contention = ContentionVerdict::NoContention {
            baseline_p99_us: 1_600,
            worst_p99_us: 1_700,
            growth: 0.06,
        };
        let recovery = RecoveryVerdict::WithinTarget {
            total_ms: 900.0,
            external_ms: 780.0,
            internal_ms: 120.0,
        };
        let reading = volume_ok();
        let assessment = classify_volume(&reading, 64 * 1024 * 1024, 1_000.0);
        let slots = SlotVerdict::WritePathBound { gain: 0.08 };

        let signals = signals_from(&contention, &recovery, &reading, &assessment, &slots);
        assert_eq!(signals.len(), 4);
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
        // 多机写入需求在本仓库无观测来源，必须记 unknown。
        assert_eq!(signals[3].met, None);

        match graduation(&signals, Some(1_000_000)) {
            Graduation::NoMigration { unknown, reasons } => {
                assert_eq!(unknown, vec![GraduationSignal::MultiHostDemand]);
                assert!(reasons.iter().any(|line| line.contains("日均事件数")));
            }
            other => panic!("三绿一 unknown 不应触发迁移，实得 {other:?}"),
        }

        // 单一 RTO 越界也不足以推出迁移：其它三项没有越界，仍需进入对比而非直接决定。
        let exceeded = RecoveryVerdict::Exceeded {
            total_ms: 9_000.0,
            external_ms: 8_000.0,
            internal_ms: 1_000.0,
            over_ms: 6_000.0,
        };
        let only_rto = signals_from(&contention, &exceeded, &reading, &assessment, &slots);
        match graduation(&only_rto, None) {
            Graduation::Candidate { signals, .. } => {
                assert_eq!(signals, vec![GraduationSignal::RecoveryRto]);
            }
            other => panic!("RTO 越界应进入方案对比，实得 {other:?}"),
        }

        // 排队但无 Busy：时延涨了也不能记成"写锁争用已越界"。
        let queueing = signals_from(
            &ContentionVerdict::QueueingWithoutBusy {
                baseline_p99_us: 2_000,
                worst_p99_us: 300_000,
                growth: 149.0,
                unknown_areas: vec!["单写者排队"],
            },
            &recovery,
            &reading,
            &assessment,
            &slots,
        );
        assert_eq!(
            queueing[1].met, None,
            "无 Busy 的排队必须记 unknown，不能当成争用已成立"
        );
    }

    /// 2.5：缺观测的信号一律 unknown，不把无告警当健康。
    #[test]
    fn task_2_5_missing_observations_stay_unknown() {
        let empty = VolumeReading {
            db_bytes: 0,
            event_rows: 0,
            index_bytes: None,
            rebuild_ms: 0.0,
            unindexed_lookup_us: 0,
            indexed_lookup_us: 0,
        };
        let assessment = classify_volume(&empty, 64 * 1024 * 1024, 1_000.0);
        assert!(matches!(assessment.size, Pressure::Unknown { .. }));
        assert!(matches!(assessment.rebuild, Pressure::Unknown { .. }));
        assert!(matches!(assessment.index_space, IndexSpace::Unknown { .. }));

        let signals = signals_from(
            &ContentionVerdict::Unknown {
                reason: "缺多写样本",
            },
            &RecoveryVerdict::Unverified {
                reason: "缺外部阶段",
            },
            &empty,
            &assessment,
            &SlotVerdict::Unknown {
                reason: "缺吞吐样本",
            },
        );
        assert!(
            signals.iter().all(|signal| signal.met.is_none()),
            "四项都缺数据时必须全部 unknown"
        );
        match graduation(&signals, None) {
            Graduation::NoMigration { unknown, .. } => assert_eq!(unknown.len(), 4),
            other => panic!("全 unknown 时不得给迁移结论，实得 {other:?}"),
        }
    }
}
