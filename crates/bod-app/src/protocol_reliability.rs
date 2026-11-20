//! 观察信封的持久接收、缺口 ACK 与业务处理边界（P0 D4 任务 5.3）。
//!
//! 顺序不可颠倒：**先**把观察写入 inbox 并推进游标提交，**之后**才返回 ACK。
//! ACK 只表示"已持久接收"，不代表工具成功、业务处理完成或成果已接受（§15.5）。
//! 收到 19 但缺 18 时 ACK 只能到 17；超出保留窗口要求快照，不静默跳到最新。

use crate::domain::execution::{
    ExecutionState, ObservationCursor, ObservationEnvelope, ObservationType, ReceiveOutcome,
};
use crate::invocation::{InvocationError, StateUpdate, advance_invocation_state};
use crate::store::SqliteStore;

/// 单条观察持久化上限：超大日志截断并标记，关键业务流另有编号与保留策略。
pub const MAX_OBSERVATION_PAYLOAD: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationReceipt {
    /// contiguous / duplicate / gap_buffered / beyond_retention / buffer_limit / invalid_sequence
    pub disposition: &'static str,
    /// 只在持久提交成功后给出；重复与缺口都不会推进。
    pub ack_upto: Option<u64>,
    pub persisted: bool,
    pub requires_snapshot: bool,
    /// 缺口起点与缺失条数。
    pub missing: Option<(u64, u64)>,
    /// 截断时的原始字节数，如实记录而不假装完整。
    pub truncated_from: Option<usize>,
    /// 落库前被脱敏的敏感字段个数；0 表示没有命中。
    pub redactions: usize,
    /// 观察若携带状态变化，返回宿主侧状态推进结果。
    pub state_update: Option<StateUpdate>,
}

fn truncate_at_boundary(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 接收一条观察。返回的 ACK 已在同一事务内与 inbox 记录一起提交。
pub fn receive_observation(
    store: &mut SqliteStore,
    envelope: &ObservationEnvelope,
    retained_floor: Option<u64>,
    now_ms: i64,
) -> Result<ObservationReceipt, InvocationError> {
    let cursor = {
        let uow = store.transaction()?;
        uow.get_observation_cursor(
            &envelope.invocation_id,
            envelope.source_epoch,
            &envelope.stream_id,
        )?
    };
    let mut cursor = cursor.unwrap_or_else(|| ObservationCursor::new(None, retained_floor));
    let outcome = cursor.receive(envelope.source_sequence);

    if outcome == ReceiveOutcome::Invalid {
        return Ok(ObservationReceipt {
            disposition: outcome.disposition(),
            ack_upto: None,
            persisted: false,
            requires_snapshot: false,
            missing: None,
            truncated_from: None,
            redactions: 0,
            state_update: None,
        });
    }
    if outcome.requires_snapshot() {
        // 不写入保留窗口外的观察：返回缺口并要求快照/对账，绝不拼接序列。
        return Ok(ObservationReceipt {
            disposition: outcome.disposition(),
            ack_upto: outcome.ack_upto(),
            persisted: false,
            requires_snapshot: true,
            missing: match &outcome {
                ReceiveOutcome::BeyondRetention { retained_floor, .. } => {
                    Some((*retained_floor, 1))
                }
                _ => None,
            },
            truncated_from: None,
            redactions: 0,
            state_update: None,
        });
    }

    let (payload, truncated_from) = if envelope.payload.len() > MAX_OBSERVATION_PAYLOAD {
        (
            truncate_at_boundary(&envelope.payload, MAX_OBSERVATION_PAYLOAD).to_string(),
            Some(envelope.payload.len()),
        )
    } else {
        (envelope.payload.clone(), None)
    };
    // 敏感字段过滤必须在落库前完成：脱敏后的文本才进 inbox，
    // 否则凭据会经查询/导出/日志三个出口泄漏（4.2）。
    let (payload, redaction) = crate::observation_redaction::redact(&payload);
    let stored = ObservationEnvelope {
        payload,
        ..envelope.clone()
    };

    let (persisted, ack_upto) = {
        let uow = store.transaction()?;
        // 先落 inbox，再推进游标：中途崩溃只会重传，不会丢事件。
        let first_time = uow.insert_observation(&stored, truncated_from.is_some(), now_ms)?;
        uow.put_observation_cursor(
            &envelope.invocation_id,
            envelope.source_epoch,
            &envelope.stream_id,
            &cursor,
            now_ms,
        )?;
        uow.commit()?;
        (first_time, cursor.ack_upto())
    };

    let mut state_update = None;
    if persisted
        && envelope.kind == ObservationType::StateChange
        && let Some(next) = ExecutionState::parse(&stored.payload)
    {
        state_update = Some(advance_invocation_state(
            store,
            &envelope.invocation_id,
            next,
            crate::domain::execution::TransitionAuthority::Observed,
            now_ms,
        )?);
    }

    let missing = match &outcome {
        ReceiveOutcome::Buffered {
            missing_from,
            missing_count,
            ..
        } => Some((*missing_from, *missing_count)),
        _ => None,
    };

    Ok(ObservationReceipt {
        disposition: outcome.disposition(),
        ack_upto: if persisted {
            ack_upto
        } else {
            outcome.ack_upto()
        },
        persisted,
        requires_snapshot: false,
        missing,
        truncated_from,
        redactions: redaction.replaced,
        state_update,
    })
}

/// 取未处理的观察（ACK 已确认但业务处理未完成的仍要消费）。
pub fn pending_observations(
    store: &mut SqliteStore,
    invocation_id: &str,
) -> Result<Vec<ObservationEnvelope>, InvocationError> {
    let uow = store.transaction()?;
    Ok(uow.list_pending_observations(invocation_id)?)
}

/// 标记业务处理完成。与 ACK 分离：ACK 早于业务处理是正常的。
pub fn mark_processed(
    store: &mut SqliteStore,
    envelope: &ObservationEnvelope,
) -> Result<bool, InvocationError> {
    let uow = store.transaction()?;
    let updated = uow.mark_observation_processed(
        &envelope.invocation_id,
        envelope.source_epoch,
        &envelope.stream_id,
        envelope.source_sequence,
    )?;
    uow.commit()?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(sequence: u64, kind: ObservationType, payload: &str) -> ObservationEnvelope {
        ObservationEnvelope {
            invocation_id: "inv-1".to_string(),
            source_epoch: 1,
            stream_id: "business".to_string(),
            source_sequence: sequence,
            observed_at_ms: 1_000 + sequence as i64,
            kind,
            payload: payload.to_string(),
            origin: "executor".to_string(),
        }
    }

    fn store() -> SqliteStore {
        SqliteStore::in_memory(&crate::config::BodConfig::default()).expect("内存库可用")
    }

    /// 状态推进要求宿主已受理该 invocation：先落记录，再收观察。
    fn seed_invocation(store: &mut SqliteStore) {
        use crate::domain::execution::{InvocationRecord, StartRequest};
        let descriptor = crate::descriptor_schema::parse_descriptor(
            crate::descriptor_schema::descriptor_fixture(),
        )
        .expect("fixture 可用");
        let request = StartRequest {
            invocation_id: "inv-1".to_string(),
            run_id: "run-1".to_string(),
            work_package_revision: "wp-1".to_string(),
            attempt_id: "attempt-1".to_string(),
            binding_id: "binding-1".to_string(),
            fencing_token: 12,
            agent_ref: "agent-1".to_string(),
            runtime_config_revision: "rt-1".to_string(),
            context_bundle_ref: "ctx-1".to_string(),
            workspace_ref: "ws-1".to_string(),
            grant_ref: "grant-1".to_string(),
            reservation_refs: vec![],
            deadline_at_ms: None,
        };
        let record = InvocationRecord::pending(&request, "company-default", &descriptor, 1);
        let uow = store.transaction().expect("事务可用");
        uow.insert_invocation(&record).expect("写入接收记录");
        uow.commit().expect("提交");
    }

    #[test]
    fn ack_only_after_persist_and_stops_at_gap() {
        let mut store = store();
        let first = receive_observation(
            &mut store,
            &envelope(1, ObservationType::Progress, "a"),
            None,
            10,
        )
        .unwrap();
        assert_eq!(first.disposition, "contiguous");
        assert_eq!(first.ack_upto, Some(1));
        assert!(first.persisted);

        // 收到 3 但缺 2：ACK 停在 1，但仍持久化 3 以便补齐后连续推进。
        let gap = receive_observation(
            &mut store,
            &envelope(3, ObservationType::Progress, "c"),
            None,
            11,
        )
        .unwrap();
        assert_eq!(gap.disposition, "gap_buffered");
        assert_eq!(gap.ack_upto, Some(1));
        assert_eq!(gap.missing, Some((2, 1)));
        assert!(gap.persisted);

        let fill = receive_observation(
            &mut store,
            &envelope(2, ObservationType::Progress, "b"),
            None,
            12,
        )
        .unwrap();
        assert_eq!(fill.disposition, "contiguous");
        assert_eq!(fill.ack_upto, Some(3));
        assert_eq!(store.count_observations("inv-1").unwrap(), 3);
    }

    #[test]
    fn duplicate_observation_is_persisted_once_and_not_reprocessed() {
        let mut store = store();
        let e = envelope(1, ObservationType::Progress, "a");
        receive_observation(&mut store, &e, None, 10).unwrap();
        let again = receive_observation(&mut store, &e, None, 11).unwrap();
        assert_eq!(again.disposition, "duplicate");
        assert!(!again.persisted);
        assert_eq!(again.ack_upto, Some(1));
        assert_eq!(store.count_observations("inv-1").unwrap(), 1);
    }

    #[test]
    fn beyond_retention_requires_snapshot_instead_of_jumping() {
        let mut store = store();
        let receipt = receive_observation(
            &mut store,
            &envelope(3, ObservationType::Log, "old"),
            Some(5),
            10,
        )
        .unwrap();
        assert_eq!(receipt.disposition, "beyond_retention");
        assert!(receipt.requires_snapshot);
        assert!(!receipt.persisted);
        assert_eq!(store.count_observations("inv-1").unwrap(), 0);
    }

    #[test]
    fn huge_log_is_truncated_and_marked() {
        let mut store = store();
        let payload = "x".repeat(MAX_OBSERVATION_PAYLOAD * 2 + 7);
        let receipt = receive_observation(
            &mut store,
            &envelope(1, ObservationType::Log, &payload),
            None,
            10,
        )
        .unwrap();
        assert_eq!(receipt.truncated_from, Some(payload.len()));
        let pending = pending_observations(&mut store, "inv-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload.len(), MAX_OBSERVATION_PAYLOAD);
    }

    #[test]
    fn ack_does_not_mean_processed() {
        let mut store = store();
        let e = envelope(1, ObservationType::FinalResult, "done");
        let receipt = receive_observation(&mut store, &e, None, 10).unwrap();
        assert_eq!(receipt.ack_upto, Some(1));
        // ACK 已给出，但业务处理未完成：仍在待处理列表里。
        assert_eq!(pending_observations(&mut store, "inv-1").unwrap().len(), 1);
        assert!(mark_processed(&mut store, &e).unwrap());
        assert!(
            pending_observations(&mut store, "inv-1")
                .unwrap()
                .is_empty()
        );
        // 重复标记不再是新处理。
        assert!(!mark_processed(&mut store, &e).unwrap());
    }

    #[test]
    fn state_change_observation_advances_host_state_only_once() {
        let mut store = store();
        seed_invocation(&mut store);
        let e = envelope(1, ObservationType::StateChange, "Running");
        let receipt = receive_observation(&mut store, &e, None, 10).unwrap();
        assert!(matches!(
            receipt.state_update,
            Some(StateUpdate::Applied(ExecutionState::Running))
        ));
        let duplicate = receive_observation(&mut store, &e, None, 11).unwrap();
        assert!(duplicate.state_update.is_none(), "重复观察不得重复推进状态");
    }
}
