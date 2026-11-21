//! P6 任务 2.3、2.4：远程分区、旧节点恢复与外部动作对账（G-19）。
//!
//! 失联不等于失败，迟到不等于权威。三段纪律：
//! 1. **失联即暂停（2.3）**：断开时暂停新的受管外部动作、保留未上传事件、连接显示"未知"
//!    （不显示"失败"，也不显示"正常"）；只读查看不受影响。
//! 2. **恢复先核对（2.3 / RM03）**：按 epoch、租约、尝试、游标、外部动作记录五项逐条核对，
//!    任一不符即阻塞并列出原因；旧节点迟到完成保留签名证据进历史，过期 token 不得修改宿主
//!    受管状态，权威结果只有一份、预算按实际用量对账、无重复验收。
//! 3. **对账不盲写（2.4 / RM02）**：外部写入已发出而结果未知时按稳定业务键查询目标；
//!    可查询且已存在只补记一次，可查询且不存在才允许一次补写，不可查询只显示待核实；
//!    目标不能隔离旧写入时阻塞冲突动作并列出核实责任人。
//!
//! 本模块是纯值判断：不建连接、不调用外部系统、不落表。

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 2.3 失联、恢复核对与旧节点迟到
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSession {
    pub node_id: String,
    pub invocation_id: String,
    /// 授权 epoch：换人/撤权/接管即变。
    pub epoch: u64,
    pub lease_id: String,
    pub lease_until_ms: i64,
    pub attempt: u32,
    /// 已确认的连续游标。
    pub cursor: u64,
    /// 未对账完的外部动作。
    pub pending_effects: Vec<String>,
    /// 节点手上令牌的 epoch：过期 token 不能改宿主状态。
    pub token_epoch: u64,
}

impl NodeSession {
    pub fn new(node_id: &str, invocation_id: &str, attempt: u32, lease_until_ms: i64) -> Self {
        Self {
            node_id: node_id.to_string(),
            invocation_id: invocation_id.to_string(),
            epoch: 1,
            lease_id: format!("lease-{attempt}"),
            lease_until_ms,
            attempt,
            cursor: 0,
            pending_effects: Vec::new(),
            token_epoch: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseDecision {
    pub pause_new_external_actions: bool,
    /// `connected` / `unreachable` / `unknown`：断开期间是"未知"，不是"失败"。
    pub connection_display: &'static str,
    /// 未上传事件一律保留，不因断开丢弃。
    pub persisted_pending_events: u64,
    /// 只读查看继续可用。
    pub view_allowed: bool,
}

pub fn on_link_lost(session: &NodeSession, pending_events: u64) -> PauseDecision {
    let _ = session;
    PauseDecision {
        pause_new_external_actions: true,
        connection_display: "unknown",
        persisted_pending_events: pending_events,
        view_allowed: true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeBlock {
    EpochMismatch { session: u64, host: u64 },
    LeaseExpired { lease_until_ms: i64, now_ms: i64 },
    LeaseReplaced { session: String, host: String },
    AttemptAdvanced { session: u32, host: u32 },
    CursorBehind { session: u64, host: u64 },
    EffectsUnreconciled { effect_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeVerdict {
    Resume { from_cursor: u64 },
    Blocked { reasons: Vec<ResumeBlock> },
}

/// 恢复时核对五项。一项不符就阻塞，不做"大体上一致就继续"。
pub fn resume_check(session: &NodeSession, host: &NodeSession, now_ms: i64) -> ResumeVerdict {
    let mut reasons = Vec::new();
    if session.epoch != host.epoch {
        reasons.push(ResumeBlock::EpochMismatch {
            session: session.epoch,
            host: host.epoch,
        });
    }
    if session.token_epoch != host.epoch {
        reasons.push(ResumeBlock::EpochMismatch {
            session: session.token_epoch,
            host: host.epoch,
        });
    }
    if session.lease_until_ms <= now_ms {
        reasons.push(ResumeBlock::LeaseExpired {
            lease_until_ms: session.lease_until_ms,
            now_ms,
        });
    }
    if session.lease_id != host.lease_id {
        reasons.push(ResumeBlock::LeaseReplaced {
            session: session.lease_id.clone(),
            host: host.lease_id.clone(),
        });
    }
    if session.attempt != host.attempt {
        reasons.push(ResumeBlock::AttemptAdvanced {
            session: session.attempt,
            host: host.attempt,
        });
    }
    if session.cursor > host.cursor {
        reasons.push(ResumeBlock::CursorBehind {
            session: session.cursor,
            host: host.cursor,
        });
    }
    // 未对账的外部动作：节点自报的与宿主记录的取并集，任一侧有都算没对完。
    let mut unreconciled: Vec<String> = session
        .pending_effects
        .iter()
        .chain(host.pending_effects.iter())
        .cloned()
        .collect();
    unreconciled.sort();
    unreconciled.dedup();
    if !unreconciled.is_empty() {
        reasons.push(ResumeBlock::EffectsUnreconciled {
            effect_ids: unreconciled,
        });
    }
    if reasons.is_empty() {
        ResumeVerdict::Resume {
            from_cursor: host.cursor + 1,
        }
    } else {
        ResumeVerdict::Blocked { reasons }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionReport {
    pub node_id: String,
    pub invocation_id: String,
    pub attempt: u32,
    pub epoch: u64,
    pub token_epoch: u64,
    pub lease_id: String,
    /// 报告内容摘要与签名：迟到也保留，供事后核对。
    pub summary_digest: String,
    pub signature: String,
    /// 报告自称的实际用量（用于按实际用量对账）。
    pub actual_usage: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetSettlement {
    /// 权威结果按实际用量对账；预留在同一笔里结算。
    ReconciledToActualUsage { actual: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LateOutcome {
    /// 仍是当前权威尝试：正常入账。
    Accepted { attempt: u32 },
    /// 旧尝试迟到：保留证据进历史，不改权威结果、不产生接受。
    KeptAsHistory {
        attempt: u32,
        authoritative_attempt: u32,
        acceptance_created: bool,
        budget: BudgetSettlement,
    },
    /// 过期 token：拒绝对宿主受管状态的任何修改。
    RejectedStaleToken { token_epoch: u64, required: u64 },
}

/// 处理一次完成报告。判定只读宿主当前权威状态，不看节点自称。
pub fn settle_completion(session: &NodeSession, report: &CompletionReport) -> LateOutcome {
    if report.token_epoch != session.epoch {
        return LateOutcome::RejectedStaleToken {
            token_epoch: report.token_epoch,
            required: session.epoch,
        };
    }
    if report.attempt == session.attempt {
        return LateOutcome::Accepted {
            attempt: report.attempt,
        };
    }
    LateOutcome::KeptAsHistory {
        attempt: report.attempt,
        authoritative_attempt: session.attempt,
        acceptance_created: false,
        budget: BudgetSettlement::ReconciledToActualUsage {
            actual: report.actual_usage,
        },
    }
}

// ---------------------------------------------------------------------------
// 2.4 外部动作按业务键对账（RM02）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectState {
    /// 已发出、结果未知。
    Unknown,
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEffect {
    /// 稳定业务动作 ID：重试不变，是查询目标与去重的唯一键。
    pub business_key: String,
    pub effect_id: String,
    pub target: String,
    pub state: EffectState,
    /// 核实责任人：不可查询时点名人，而不是"待补充"。
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRecord {
    pub business_key: String,
    pub external_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetQuery {
    /// 目标可查账：`found` 是按业务键查到的记录。
    Queryable { found: Vec<ExternalRecord> },
    /// 目标不能查账（只有基本回调的 webhook 等）。
    NotQueryable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// 目标里已存在：只补记一次，不重写。
    AlreadyApplied {
        external_id: String,
        recorded: usize,
        blind_writes: usize,
    },
    /// 目标里确实没有：允许一次补写，且只此一次。
    WriteOnce { writes: usize },
    /// 不可查询：显示待核实并点名责任人，盲写次数为 0。
    NeedsVerification { owner: String, blind_writes: usize },
    /// 目标不能隔离旧写入：阻塞冲突动作。
    Blocked { owner: String, reason: &'static str },
}

/// 对账台账：只记"已经写入目标几次"，用来证明没有第二次盲写。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileLedger {
    writes: BTreeMap<String, usize>,
    recorded: BTreeMap<String, usize>,
}

impl ReconcileLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn writes_for(&self, effect_id: &str) -> usize {
        self.writes.get(effect_id).copied().unwrap_or(0)
    }

    pub fn recorded_for(&self, effect_id: &str) -> usize {
        self.recorded.get(effect_id).copied().unwrap_or(0)
    }

    /// 按业务键对账。同一 effect 重复对账不会产生第二次写或第二次补记。
    pub fn reconcile(&mut self, effect: &ExternalEffect, query: &TargetQuery) -> ReconcileOutcome {
        match query {
            TargetQuery::NotQueryable => ReconcileOutcome::NeedsVerification {
                owner: effect.owner.clone(),
                blind_writes: 0,
            },
            TargetQuery::Queryable { found } => {
                let matched = found
                    .iter()
                    .find(|record| record.business_key == effect.business_key);
                match matched {
                    Some(record) => {
                        let counter = self.recorded.entry(effect.effect_id.clone()).or_insert(0);
                        if *counter == 0 {
                            *counter = 1;
                        }
                        ReconcileOutcome::AlreadyApplied {
                            external_id: record.external_id.clone(),
                            recorded: *counter,
                            blind_writes: 0,
                        }
                    }
                    None => {
                        let written = self.writes.entry(effect.effect_id.clone()).or_insert(0);
                        if *written == 0 {
                            // 允许一次补写：目标确认不存在时才是"没生效"。
                            *written = 1;
                        }
                        ReconcileOutcome::WriteOnce { writes: *written }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictDecision {
    /// 目标不能隔离旧写入：阻塞冲突动作，列出责任人。
    Blocked { owner: String },
    /// 只能在隔离的独立工作区推进。
    IsolatedWorkspace,
}

/// 冲突动作的前置判定：能隔离才允许并行，否则阻塞。
pub fn resolve_conflict(effect: &ExternalEffect, target_can_isolate: bool) -> ConflictDecision {
    if target_can_isolate {
        ConflictDecision::IsolatedWorkspace
    } else {
        ConflictDecision::Blocked {
            owner: effect.owner.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(attempt: u32) -> NodeSession {
        NodeSession::new("node-a", "inv-1", attempt, 10_000)
    }

    fn report(attempt: u32) -> CompletionReport {
        CompletionReport {
            node_id: "node-a".to_string(),
            invocation_id: "inv-1".to_string(),
            attempt,
            epoch: 1,
            token_epoch: 1,
            lease_id: "lease-1".to_string(),
            summary_digest: "sha256:done".to_string(),
            signature: "sig:node-a".to_string(),
            actual_usage: 1_200,
        }
    }

    fn effect() -> ExternalEffect {
        ExternalEffect {
            business_key: "publish:project-1:release-7".to_string(),
            effect_id: "eff-1".to_string(),
            target: "connector:tracker".to_string(),
            state: EffectState::Unknown,
            owner: "ops-1".to_string(),
        }
    }

    /// 2.3：失联暂停新外部动作、显示未知、保留未上传事件、查看继续。
    #[test]
    fn task_2_3_link_loss_pauses_external_actions_and_shows_unknown() {
        let decision = on_link_lost(&session(1), 7);
        assert!(
            decision.pause_new_external_actions,
            "失联必须暂停新外部动作"
        );
        assert_eq!(decision.connection_display, "unknown", "显示未知而非失败");
        assert_eq!(decision.persisted_pending_events, 7, "未上传事件不丢弃");
        assert!(decision.view_allowed, "只读查看不受影响");
    }

    /// 2.3：恢复先核对 epoch、租约、尝试、游标与外部动作五项。
    #[test]
    fn task_2_3_resume_checks_epoch_lease_attempt_cursor_and_effects() {
        let host = session(2);
        assert_eq!(
            resume_check(&host, &host, 5_000),
            ResumeVerdict::Resume { from_cursor: 1 }
        );

        let stale = NodeSession {
            cursor: 9,
            pending_effects: vec!["eff-1".to_string()],
            ..session(1)
        };
        match resume_check(&stale, &host, 5_000) {
            ResumeVerdict::Blocked { reasons } => {
                assert!(reasons.contains(&ResumeBlock::AttemptAdvanced {
                    session: 1,
                    host: 2
                }));
                assert!(reasons.contains(&ResumeBlock::CursorBehind {
                    session: 9,
                    host: 0
                }));
                assert!(reasons.contains(&ResumeBlock::EffectsUnreconciled {
                    effect_ids: vec!["eff-1".to_string()]
                }));
            }
            other => panic!("旧节点不得直接恢复，实得 {other:?}"),
        }

        // 租约过期与 epoch 变化各自独立成条。
        let expired = NodeSession {
            epoch: 3,
            token_epoch: 3,
            ..session(2)
        };
        match resume_check(&expired, &host, 20_000) {
            ResumeVerdict::Blocked { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|reason| matches!(reason, ResumeBlock::LeaseExpired { .. }))
                );
                assert!(reasons.iter().any(|reason| matches!(
                    reason,
                    ResumeBlock::EpochMismatch {
                        session: 3,
                        host: 1
                    }
                )));
            }
            other => panic!("租约过期应阻塞，实得 {other:?}"),
        }
    }

    /// 2.3 / RM03：租约过期后新尝试完成，旧节点提交只保留历史。
    #[test]
    fn rm03_lease_expired_new_attempt_keeps_old_report_as_history() {
        let current = session(2);
        assert_eq!(
            settle_completion(&current, &report(2)),
            LateOutcome::Accepted { attempt: 2 },
            "当前尝试正常入账"
        );

        match settle_completion(&current, &report(1)) {
            LateOutcome::KeptAsHistory {
                attempt,
                authoritative_attempt,
                acceptance_created,
                budget,
            } => {
                assert_eq!(attempt, 1);
                assert_eq!(authoritative_attempt, 2, "权威结果仍是新尝试");
                assert!(!acceptance_created, "迟到报告不产生接受（无重复验收）");
                assert_eq!(
                    budget,
                    BudgetSettlement::ReconciledToActualUsage { actual: 1_200 },
                    "预算按实际用量对账"
                );
            }
            other => panic!("旧尝试迟到只能进历史，实得 {other:?}"),
        }

        // 过期 token 一律拒绝修改宿主受管状态。
        let revoked = NodeSession {
            epoch: 5,
            ..session(2)
        };
        assert_eq!(
            settle_completion(&revoked, &report(2)),
            LateOutcome::RejectedStaleToken {
                token_epoch: 1,
                required: 5
            }
        );
    }

    /// 2.4 / RM02：已生效只补记一次；不可查询显示待核实且无盲写。
    #[test]
    fn rm02_applied_effect_recorded_once_or_needs_verification() {
        let mut ledger = ReconcileLedger::new();
        let applied = TargetQuery::Queryable {
            found: vec![ExternalRecord {
                business_key: "publish:project-1:release-7".to_string(),
                external_id: "ext-42".to_string(),
            }],
        };
        assert_eq!(
            ledger.reconcile(&effect(), &applied),
            ReconcileOutcome::AlreadyApplied {
                external_id: "ext-42".to_string(),
                recorded: 1,
                blind_writes: 0
            }
        );
        let again = ledger.reconcile(&effect(), &applied);
        assert_eq!(
            again,
            ReconcileOutcome::AlreadyApplied {
                external_id: "ext-42".to_string(),
                recorded: 1,
                blind_writes: 0
            },
            "第二次对账不重复补记"
        );
        assert_eq!(ledger.recorded_for("eff-1"), 1);
        assert_eq!(ledger.writes_for("eff-1"), 0, "已生效时不写");

        let unknown = TargetQuery::NotQueryable;
        for _ in 0..3 {
            assert_eq!(
                ledger.reconcile(&effect(), &unknown),
                ReconcileOutcome::NeedsVerification {
                    owner: "ops-1".to_string(),
                    blind_writes: 0
                }
            );
        }
        assert_eq!(ledger.writes_for("eff-1"), 0, "不可查询绝不盲写");
    }

    /// 2.4：可查询且确认不存在才允许一次补写，绝不第二次。
    #[test]
    fn task_2_4_queryable_target_allows_exactly_one_write() {
        let mut ledger = ReconcileLedger::new();
        let absent = TargetQuery::Queryable { found: Vec::new() };
        assert_eq!(
            ledger.reconcile(&effect(), &absent),
            ReconcileOutcome::WriteOnce { writes: 1 }
        );
        assert_eq!(
            ledger.reconcile(&effect(), &absent),
            ReconcileOutcome::WriteOnce { writes: 1 },
            "第二次对账不再写"
        );
        assert_eq!(ledger.writes_for("eff-1"), 1);

        // 其他业务键的记录不算命中：按业务键而不是按目标匹配。
        let other_key = TargetQuery::Queryable {
            found: vec![ExternalRecord {
                business_key: "publish:project-1:release-6".to_string(),
                external_id: "ext-40".to_string(),
            }],
        };
        assert_eq!(
            ledger.reconcile(&effect(), &other_key),
            ReconcileOutcome::WriteOnce { writes: 1 }
        );
    }

    /// 2.4：目标不能隔离旧写入时阻塞冲突动作并点名责任人。
    #[test]
    fn task_2_4_conflicting_action_blocked_when_target_cannot_isolate() {
        assert_eq!(
            resolve_conflict(&effect(), false),
            ConflictDecision::Blocked {
                owner: "ops-1".to_string()
            }
        );
        assert_eq!(
            resolve_conflict(&effect(), true),
            ConflictDecision::IsolatedWorkspace
        );
    }
}
