//! 唤醒受理编排（P0 D2 任务 3.1—3.2）。
//!
//! 把一个逻辑唤醒在**同一** UnitOfWork 内完成：查回执 → 纯决策 → 登记回执 +
//! 推进 work_queue 的 wanted_generation。事件事实假定已持久（由调用方先写入 events），
//! 本模块只引用其摘要，不保留“最后一个 payload”。重复回执不递增代数、不建第二派发。
//!
//! 时间由调用方注入（宿主时钟），本模块不读系统时钟、不做 IO；网络/Agent 派发在事务外。

pub use crate::domain::queue::{
    QueueError, QueueState, WakeOutcome, WakeReceipt, WakeRequest, WorkQueueEntry,
    apply_accepted_wake, evaluate_wake,
};
use crate::store::{SqliteStore, StoreError, UnitOfWork};

#[derive(Debug)]
pub enum WakeError {
    Store(StoreError),
    Queue(QueueError),
}

impl From<StoreError> for WakeError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<QueueError> for WakeError {
    fn from(value: QueueError) -> Self {
        Self::Queue(value)
    }
}

/// 在给定写事务内受理一次唤醒，返回决策结果。调用方负责 commit/rollback。
///
/// 顺序严格遵循 §33.15：先查回执并比对摘要，再决定是否递增代数；
/// 不存在时先创建/更新 work_queue 行，再登记回执，二者在同一事务。
pub fn accept_in(
    uow: &UnitOfWork<'_>,
    req: &WakeRequest,
    now_ms: i64,
    retain_window_ms: i64,
) -> Result<WakeOutcome, WakeError> {
    let existing = uow.get_wake_receipt(
        req.company_id.as_str(),
        req.producer_id.as_str(),
        &req.idempotency_key,
    )?;
    let queue = uow.get_queue(req.company_id.as_str(), &req.queue_key)?;
    let outcome = evaluate_wake(
        existing.as_ref(),
        queue.as_ref(),
        req,
        now_ms,
        retain_window_ms,
    )?;

    if let WakeOutcome::Accepted {
        receipt,
        generation,
    } = &outcome
    {
        let next = apply_accepted_wake(queue, req, *generation, now_ms);
        // 先写队列行（回执有外键引用它），再写回执；同事务原子提交。
        uow.put_queue(&next)?;
        uow.put_wake_receipt(receipt)?;
    }
    Ok(outcome)
}

/// 便捷入口：打开一个写事务受理唤醒并提交。
pub fn accept(
    store: &mut SqliteStore,
    req: &WakeRequest,
    now_ms: i64,
    retain_window_ms: i64,
) -> Result<WakeOutcome, WakeError> {
    let uow = store.transaction()?;
    let outcome = accept_in(&uow, req, now_ms, retain_window_ms)?;
    uow.commit()?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::DomainId;
    use crate::store::SqliteStore;

    fn config() -> BodConfig {
        BodConfig::default()
    }

    fn store() -> SqliteStore {
        SqliteStore::in_memory(&config()).expect("内存库应可打开")
    }

    fn req(key: &str, digest: &str) -> WakeRequest {
        WakeRequest {
            company_id: DomainId::new("company-default"),
            producer_id: DomainId::new("producer-1"),
            idempotency_key: key.to_string(),
            request_digest: digest.to_string(),
            queue_key: "issue-1".to_string(),
            schedule_id: None,
            occurrence_id: None,
        }
    }

    fn retain_ms() -> i64 {
        168 * 3_600_000
    }

    #[test]
    fn i_68_repeated_command_returns_original_receipt_and_no_second_generation() {
        let mut store = store();
        let first = accept(&mut store, &req("k1", "d1"), 1_000, retain_ms()).unwrap();
        let generation = match first {
            WakeOutcome::Accepted { generation, .. } => generation,
            _ => panic!("首次应受理"),
        };
        assert_eq!(generation, 1);

        let second = accept(&mut store, &req("k1", "d1"), 2_000, retain_ms()).unwrap();
        match second {
            WakeOutcome::Existing { receipt } => {
                assert_eq!(receipt.generation, 1);
                assert_eq!(receipt.accepted_at_ms, 1_000);
            }
            _ => panic!("重复命令应返回原回执"),
        }
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(entry.wanted_generation, 1, "重复回执不得递增代数");
    }

    #[test]
    fn i_69_same_key_changed_digest_is_rejected() {
        let mut store = store();
        accept(&mut store, &req("k1", "d1"), 1_000, retain_ms()).unwrap();
        let err = accept(&mut store, &req("k1", "d2"), 2_000, retain_ms()).unwrap_err();
        assert!(matches!(
            err,
            WakeError::Queue(QueueError::KeyConflict { .. })
        ));
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(entry.wanted_generation, 1, "冲突请求不得推进代数");
    }

    #[test]
    fn i_70_new_logical_wake_while_running_bumps_generation_without_second_consumer() {
        let mut store = store();
        accept(&mut store, &req("k1", "d1"), 1_000, retain_ms()).unwrap();
        {
            let uow = store.transaction().unwrap();
            let mut entry = uow
                .get_queue("company-default", "issue-1")
                .unwrap()
                .unwrap();
            entry.state = QueueState::Running;
            entry.lease_owner = Some("consumer-1".to_string());
            entry.lease_token = 1;
            entry.claimed_generation = 1;
            uow.put_queue(&entry).unwrap();
            uow.commit().unwrap();
        }
        let second = accept(&mut store, &req("k2", "d2"), 1_500, retain_ms()).unwrap();
        assert!(matches!(
            second,
            WakeOutcome::Accepted { generation: 2, .. }
        ));
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.state,
            QueueState::Running,
            "运行中新增工作仍保持单一消费者"
        );
        assert_eq!(entry.wanted_generation, 2);
        assert_eq!(entry.claimed_generation, 1);
        assert_eq!(entry.lease_owner.as_deref(), Some("consumer-1"));
    }

    #[test]
    fn scheduled_occurrence_is_bound_and_cannot_be_masked_as_retry() {
        let mut store = store();
        let mut first = req("sched-1", "d1");
        first.schedule_id = Some("sched-1".to_string());
        first.occurrence_id = Some("occ-1".to_string());
        accept(&mut store, &first, 1_000, retain_ms()).unwrap();

        // 同键改 occurrence：拒绝（不生成伪装重试）。
        let mut changed = first.clone();
        changed.occurrence_id = Some("occ-2".to_string());
        let err = accept(&mut store, &changed, 1_100, retain_ms()).unwrap_err();
        assert!(matches!(
            err,
            WakeError::Queue(QueueError::KeyConflict { .. })
        ));

        // 同键同 occurrence 重放：返回原回执。
        let replay = accept(&mut store, &first, 1_200, retain_ms()).unwrap();
        assert!(matches!(replay, WakeOutcome::Existing { .. }));
    }

    #[test]
    fn duplicate_wake_never_creates_a_second_outbox_intent() {
        use crate::domain::queue::OutboxDelivery;
        use crate::domain::{DomainId, EffectStatus};

        let mut store = store();
        accept(&mut store, &req("k1", "d1"), 1_000, retain_ms()).unwrap();
        accept(&mut store, &req("k1", "d1"), 2_000, retain_ms()).unwrap();
        // 派发意图以稳定逻辑键为唯一约束：重试不会覆盖出第二条。
        let delivery = OutboxDelivery {
            event_or_command_id: "act-1".to_string(),
            destination_id: "executor-1".to_string(),
            company_id: DomainId::new("company-default"),
            queue_key: "issue-1".to_string(),
            request_digest: "d1".to_string(),
            status: EffectStatus::Prepared,
            attempts: 0,
            next_due_at_ms: 1_000,
            result_ref: None,
            created_at_ms: 1_000,
        };
        {
            let uow = store.transaction().unwrap();
            uow.put_outbox(&delivery).unwrap();
            uow.put_outbox(&delivery).unwrap();
            uow.commit().unwrap();
        }
        let delivered = store.list_outbox_by_status(EffectStatus::Prepared).unwrap();
        assert_eq!(delivered.len(), 1, "同一逻辑键只保留一条派发意图");
    }
}
