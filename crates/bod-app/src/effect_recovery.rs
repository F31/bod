//! Effect Gate 提交后派发与重启恢复编排（P0 D2 任务 3.4）。
//!
//! 关键边界（§10.3—10.5、§33.13、effect-gate spec）：
//! - 意图在写事务内持久（Prepared），网络/进程动作只在事务**提交后**由 outbox 派发；
//! - 事件回放只恢复事实投影与待处理 outbox，**不调用外部系统**；
//! - 重启后扫描未终结资源与 outbox，Dispatched/Unknown 先 inspect/reconcile 再决定，
//!   已写 Dispatched 但未发送时不得盲重发，未取得证据不外报 Confirmed。

pub use crate::domain::queue::{
    EffectStatus, ExternalObservation, OutboxDelivery, RecoveryAction, classify_recovery,
};
use crate::store::{SqliteStore, StoreError, UnitOfWork};

#[derive(Debug)]
pub enum RecoveryError {
    Store(StoreError),
    /// 逻辑键冲突：同 ID 不同参数，拒绝按新动作执行。
    KeyConflict {
        event_or_command_id: String,
    },
}

impl From<StoreError> for RecoveryError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

/// 在写事务内登记派发意图。同逻辑键同摘要返回已有意图（稳定逻辑键不重建），
/// 同键不同摘要拒绝（effect-gate 幂等逻辑键）。
pub fn prepare_in(
    uow: &UnitOfWork<'_>,
    delivery: &OutboxDelivery,
) -> Result<PreparedIntent, RecoveryError> {
    if let Some(existing) =
        uow.get_outbox(&delivery.event_or_command_id, &delivery.destination_id)?
    {
        if existing.request_digest != delivery.request_digest {
            return Err(RecoveryError::KeyConflict {
                event_or_command_id: delivery.event_or_command_id.clone(),
            });
        }
        return Ok(PreparedIntent {
            delivery: existing,
            created: false,
        });
    }
    uow.put_outbox(delivery)?;
    Ok(PreparedIntent {
        delivery: delivery.clone(),
        created: true,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedIntent {
    pub delivery: OutboxDelivery,
    pub created: bool,
}

/// 重启恢复扫描：读取所有 Dispatched/Unknown 意图，按外部观测分类。
/// 本函数只读库、不做派发；`inspect` 由宿主在事务外完成后把结果传入 `decide`。
pub fn pending_recovery(store: &SqliteStore) -> Result<Vec<OutboxDelivery>, RecoveryError> {
    let mut out = Vec::new();
    for status in [
        EffectStatus::Prepared,
        EffectStatus::Dispatched,
        EffectStatus::Unknown,
    ] {
        out.extend(store.list_outbox_by_status(status)?);
    }
    Ok(out)
}

/// 对单条意图做恢复决策。observed 是宿主 inspect 的结果；未查询时为 None → InspectFirst。
pub fn decide(
    delivery: &OutboxDelivery,
    observed: Option<ExternalObservation>,
    now_ms: i64,
) -> RecoveryAction {
    classify_recovery(delivery, observed, now_ms)
}

/// 把恢复裁决写回 outbox：Confirmed 记录结果引用；重派发则标记 Dispatched 并递增尝试。
pub fn apply_recovery_in(
    uow: &UnitOfWork<'_>,
    delivery: &OutboxDelivery,
    action: &RecoveryAction,
    now_ms: i64,
) -> Result<(), RecoveryError> {
    match action {
        RecoveryAction::MarkConfirmed { result_ref } => {
            uow.set_outbox_status(
                &delivery.event_or_command_id,
                &delivery.destination_id,
                EffectStatus::Confirmed,
                delivery.attempts,
                now_ms,
                result_ref.as_deref(),
            )?;
        }
        RecoveryAction::RedispatchGuarded => {
            // 确认外部未发生后允许一次受幂等键保护的重派发；标记 Dispatched 等待结果回执。
            uow.set_outbox_status(
                &delivery.event_or_command_id,
                &delivery.destination_id,
                EffectStatus::Dispatched,
                delivery.attempts + 1,
                now_ms,
                None,
            )?;
        }
        RecoveryAction::Dispatch | RecoveryAction::InspectFirst | RecoveryAction::Skip => {}
    }
    Ok(())
}

/// 事件回放：只从持久化 outbox 恢复待处理意图，**不执行副作用**（effect-gate 提交后派发）。
/// 这里返回 Prepared 意图清单供宿主在提交后派发；不接触网络/Agent。
pub fn replay_outbox(store: &SqliteStore) -> Result<Vec<OutboxDelivery>, RecoveryError> {
    Ok(store
        .list_outbox_by_status(EffectStatus::Prepared)?
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::DomainId;
    use crate::store::SqliteStore;

    fn delivery(id: &str, digest: &str, status: EffectStatus) -> OutboxDelivery {
        OutboxDelivery {
            event_or_command_id: id.to_string(),
            destination_id: "executor-1".to_string(),
            company_id: DomainId::new("company-default"),
            queue_key: "issue-1".to_string(),
            request_digest: digest.to_string(),
            status,
            attempts: 0,
            next_due_at_ms: 1_000,
            result_ref: None,
            created_at_ms: 900,
        }
    }

    fn store() -> SqliteStore {
        SqliteStore::in_memory(&BodConfig::default()).unwrap()
    }

    #[test]
    fn intent_is_persisted_and_same_key_same_digest_returns_original() {
        let mut store = store();
        {
            let uow = store.transaction().unwrap();
            let first = prepare_in(&uow, &delivery("act-1", "d1", EffectStatus::Prepared)).unwrap();
            assert!(first.created);
            let again = prepare_in(&uow, &delivery("act-1", "d1", EffectStatus::Prepared)).unwrap();
            assert!(!again.created, "同逻辑键保持原意图，不重建");
            uow.commit().unwrap();
        }
        let pending = pending_recovery(&store).unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn same_key_changed_digest_is_rejected() {
        let mut store = store();
        let uow = store.transaction().unwrap();
        prepare_in(&uow, &delivery("act-1", "d1", EffectStatus::Prepared)).unwrap();
        let err = prepare_in(&uow, &delivery("act-1", "d2", EffectStatus::Prepared)).unwrap_err();
        assert!(matches!(err, RecoveryError::KeyConflict { .. }));
        uow.rollback().unwrap();
    }

    #[test]
    fn i_04_replay_only_restores_pending_intent_without_side_effect() {
        let mut store = store();
        {
            let uow = store.transaction().unwrap();
            prepare_in(&uow, &delivery("act-1", "d1", EffectStatus::Prepared)).unwrap();
            uow.commit().unwrap();
        }
        // 回放：拿到 Prepared 意图，但状态仍是 Prepared（没有派发副作用把状态改成 Dispatched）。
        let replayed = replay_outbox(&store).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].status, EffectStatus::Prepared);
        let still = store
            .transaction()
            .unwrap()
            .get_outbox("act-1", "executor-1")
            .unwrap()
            .unwrap();
        assert_eq!(still.status, EffectStatus::Prepared);
    }

    #[test]
    fn i_11_dispatched_without_evidence_inspects_first_then_marks_confirmed() {
        let mut store = store();
        {
            let uow = store.transaction().unwrap();
            prepare_in(&uow, &delivery("act-1", "d1", EffectStatus::Dispatched)).unwrap();
            uow.commit().unwrap();
        }
        let pending = pending_recovery(&store).unwrap();
        let item = &pending[0];
        // 未查询：先 inspect，禁止盲重发。
        assert_eq!(decide(item, None, 2_000), RecoveryAction::InspectFirst);
        // 查询确认已发生：只标记 Confirmed，不重发。
        let action = decide(
            item,
            Some(ExternalObservation::Confirmed {
                result_ref: Some("inv-123".to_string()),
            }),
            2_000,
        );
        assert!(matches!(action, RecoveryAction::MarkConfirmed { .. }));
        {
            let uow = store.transaction().unwrap();
            apply_recovery_in(&uow, item, &action, 2_000).unwrap();
            uow.commit().unwrap();
        }
        let after = store
            .transaction()
            .unwrap()
            .get_outbox("act-1", "executor-1")
            .unwrap()
            .unwrap();
        assert_eq!(after.status, EffectStatus::Confirmed);
        assert_eq!(after.result_ref.as_deref(), Some("inv-123"));
    }

    #[test]
    fn committed_then_crash_recovers_intent_and_confirms_via_external_state() {
        let mut store = store();
        // 提交后崩溃：意图已落盘为 Dispatched（已写 Dispatched 但未发送窗口）。
        {
            let uow = store.transaction().unwrap();
            prepare_in(&uow, &delivery("act-1", "d1", EffectStatus::Dispatched)).unwrap();
            uow.commit().unwrap();
        }
        drop(store);
        // 重启：从 DB 恢复意图，先 inspect；确认外部已发生则标记 Confirmed，不重复扣减/派发。
        let store = store_reopen_from(&delivery("act-1", "d1", EffectStatus::Dispatched));
        let pending = pending_recovery(&store).unwrap();
        assert_eq!(pending.len(), 1);
        let action = decide(
            &pending[0],
            Some(ExternalObservation::Confirmed { result_ref: None }),
            3_000,
        );
        assert!(matches!(action, RecoveryAction::MarkConfirmed { .. }));
    }

    // 用一份新的内存库模拟“重启后从持久化记录恢复”，保留同样的逻辑键与意图。
    fn store_reopen_from(d: &OutboxDelivery) -> SqliteStore {
        let mut store = store();
        {
            let uow = store.transaction().unwrap();
            prepare_in(&uow, d).unwrap();
            uow.commit().unwrap();
        }
        store
    }
}
