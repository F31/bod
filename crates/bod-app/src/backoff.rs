//! 有界退避、稳定 Blocked、dead 与队列观测编排（P0 D2 任务 3.5）。
//!
//! 纯退避决策在 bod-core::queue::evaluate_retry；本模块把它落到队列状态：
//! 瞬时错误 → 有界退避重排；权限/预算/输入/外部未知 → 稳定 blocked（无热重试）；
//! 超预算 → dead 并保留原因。doctor 通过指标查询列出深度、最老未处理与 dead/Unknown。

use crate::domain::DenialReason;
pub use crate::domain::queue::{
    ErrorClass, QueueMetrics, QueueState, RetryDecision, RetryPolicy, WorkQueueEntry,
    evaluate_retry, summarize_metrics,
};
use crate::store::{SqliteStore, StoreError, UnitOfWork};

#[derive(Debug)]
pub enum BackoffError {
    Store(StoreError),
    /// 租约不匹配：只有当前租约持有者能提交退避/阻塞裁决。
    LeaseLost,
}

impl From<StoreError> for BackoffError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

/// 把一次失败按退避决策落到队列行。调用方须持当前租约（owner/token）。
/// 返回写回后的下一状态供上层记录。
#[allow(clippy::too_many_arguments)]
pub fn apply_failure_in(
    uow: &UnitOfWork<'_>,
    company_id: &str,
    queue_key: &str,
    owner: &str,
    token: u64,
    class: ErrorClass,
    policy: RetryPolicy,
    now_ms: i64,
) -> Result<NextAction, BackoffError> {
    let decision = evaluate_retry(class, attempts_of(uow, company_id, queue_key)?, policy);
    match decision {
        RetryDecision::Backoff { delay_ms } => {
            let due = now_ms + delay_ms;
            let written = uow.complete_queue_if_leased(
                company_id,
                queue_key,
                owner,
                token,
                0,
                QueueState::Pending,
                due,
                Some("瞬时错误，有界退避后重排"),
                None,
            )?;
            if !written {
                return Err(BackoffError::LeaseLost);
            }
            Ok(NextAction::Requeue { due_ms: due })
        }
        RetryDecision::StableBlock { reason } => {
            let written = uow.complete_queue_if_leased(
                company_id,
                queue_key,
                owner,
                token,
                0,
                QueueState::Blocked,
                i64::MAX,
                Some("稳定阻塞，等待显式解除条件"),
                Some(reason),
            )?;
            if !written {
                return Err(BackoffError::LeaseLost);
            }
            Ok(NextAction::Block { reason })
        }
        RetryDecision::Dead { reason } => {
            let written = uow.complete_queue_if_leased(
                company_id,
                queue_key,
                owner,
                token,
                0,
                QueueState::Dead,
                i64::MAX,
                Some("超过重试预算，转人工处理"),
                Some(reason),
            )?;
            if !written {
                return Err(BackoffError::LeaseLost);
            }
            Ok(NextAction::Dead { reason })
        }
    }
}

fn attempts_of(uow: &UnitOfWork<'_>, company_id: &str, queue_key: &str) -> Result<u32, StoreError> {
    Ok(uow
        .get_queue(company_id, queue_key)?
        .map(|entry| entry.attempts)
        .unwrap_or(0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextAction {
    Requeue { due_ms: i64 },
    Block { reason: DenialReason },
    Dead { reason: DenialReason },
}

/// doctor 队列健康：聚合深度、最老到期/未处理、代数滞后、租约过期、dead、重试与投递延迟。
pub fn doctor_metrics(
    store: &SqliteStore,
    company_id: &str,
    now_ms: i64,
) -> Result<QueueMetrics, StoreError> {
    let entries = store.list_queue(company_id)?;
    let prepared = store.list_outbox_by_status(crate::domain::EffectStatus::Prepared)?;
    let dispatched = store.list_outbox_by_status(crate::domain::EffectStatus::Dispatched)?;
    let deliveries: Vec<_> = prepared.into_iter().chain(dispatched).collect();
    Ok(summarize_metrics(&entries, &deliveries, now_ms))
}

/// 列出需要人工接管的 Unresolved 项（dead + ExternalOutcomeUnknown 阻塞），供 doctor 单列。
pub fn unresolved(
    store: &SqliteStore,
    company_id: &str,
) -> Result<Vec<WorkQueueEntry>, StoreError> {
    Ok(store
        .list_queue(company_id)?
        .into_iter()
        .filter(|entry| {
            entry.state == QueueState::Dead
                || entry.blocked_reason == Some(DenialReason::ExternalOutcomeUnknown)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim_concurrency;
    use crate::config::BodConfig;
    use crate::domain::{DomainId, EffectStatus, WakeRequest};
    use crate::store::SqliteStore;
    use crate::wakeup;

    fn wake(key: &str) -> WakeRequest {
        WakeRequest {
            company_id: DomainId::new("company-default"),
            producer_id: DomainId::new("producer-1"),
            idempotency_key: key.to_string(),
            request_digest: "d1".to_string(),
            queue_key: "issue-1".to_string(),
            schedule_id: None,
            occurrence_id: None,
        }
    }

    fn seeded() -> SqliteStore {
        let mut store = SqliteStore::in_memory(&BodConfig::default()).unwrap();
        wakeup::accept(&mut store, &wake("k1"), 1_000, 168 * 3_600_000).unwrap();
        {
            let uow = store.transaction().unwrap();
            claim_concurrency::claim_in(
                &uow,
                "company-default",
                "issue-1",
                "consumer",
                1_000,
                30_000,
            )
            .unwrap();
            uow.commit().unwrap();
        }
        store
    }

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
            total_deadline_ms: 600_000,
            jitter_ms: 37,
        }
    }

    #[test]
    fn budget_block_is_stable_no_next_due_hot_retry() {
        let mut store = seeded();
        {
            let uow = store.transaction().unwrap();
            let action = apply_failure_in(
                &uow,
                "company-default",
                "issue-1",
                "consumer",
                1,
                ErrorClass::Budget,
                policy(),
                1_100,
            )
            .unwrap();
            assert_eq!(
                action,
                NextAction::Block {
                    reason: DenialReason::InsufficientBudget
                }
            );
            uow.commit().unwrap();
        }
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, QueueState::Blocked);
        assert_eq!(
            entry.next_due_at_ms,
            i64::MAX,
            "稳定阻塞不设置近到期，禁止热重试"
        );
        let metrics = doctor_metrics(&store, "company-default", 1_100).unwrap();
        assert_eq!(metrics.blocked_count, 1);
    }

    #[test]
    fn transient_error_requeues_with_bounded_delay() {
        let mut store = seeded();
        {
            let uow = store.transaction().unwrap();
            let action = apply_failure_in(
                &uow,
                "company-default",
                "issue-1",
                "consumer",
                1,
                ErrorClass::Transient,
                policy(),
                1_100,
            )
            .unwrap();
            match action {
                NextAction::Requeue { due_ms } => {
                    assert!(due_ms > 1_100);
                    assert!(due_ms <= 1_100 + policy().max_delay_ms);
                }
                _ => panic!("瞬时错误应有界退避重排"),
            }
            uow.commit().unwrap();
        }
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, QueueState::Pending);
    }

    #[test]
    fn exceeding_retry_budget_goes_dead_and_doctor_lists_it() {
        let mut store = seeded();
        {
            let uow = store.transaction().unwrap();
            let action = apply_failure_in(
                &uow,
                "company-default",
                "issue-1",
                "consumer",
                1,
                ErrorClass::Transient,
                RetryPolicy {
                    max_attempts: 0,
                    ..policy()
                },
                1_100,
            )
            .unwrap();
            assert_eq!(
                action,
                NextAction::Dead {
                    reason: DenialReason::InsufficientBudget
                }
            );
            uow.commit().unwrap();
        }
        let metrics = doctor_metrics(&store, "company-default", 1_100).unwrap();
        assert_eq!(metrics.dead_count, 1);
        assert_eq!(metrics.depth, 1);
        assert_eq!(metrics.oldest_queue_key.as_deref(), Some("issue-1"));
        let unresolved = unresolved(&store, "company-default").unwrap();
        assert_eq!(unresolved.len(), 1);
    }

    #[test]
    fn external_unknown_is_listed_as_unresolved() {
        let mut store = seeded();
        {
            let uow = store.transaction().unwrap();
            apply_failure_in(
                &uow,
                "company-default",
                "issue-1",
                "consumer",
                1,
                ErrorClass::ExternalUnknown,
                policy(),
                1_100,
            )
            .unwrap();
            uow.commit().unwrap();
        }
        let unresolved = unresolved(&store, "company-default").unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(
            unresolved[0].blocked_reason,
            Some(DenialReason::ExternalOutcomeUnknown)
        );
    }

    #[test]
    fn doctor_reports_oldest_unprocessed_time_and_delivery_latency() {
        let mut store = seeded();
        {
            let uow = store.transaction().unwrap();
            uow.put_outbox(&crate::domain::OutboxDelivery {
                event_or_command_id: "act-1".to_string(),
                destination_id: "executor-1".to_string(),
                company_id: DomainId::new("company-default"),
                queue_key: "issue-1".to_string(),
                request_digest: "d1".to_string(),
                status: EffectStatus::Prepared,
                attempts: 0,
                next_due_at_ms: 1_000,
                result_ref: None,
                created_at_ms: 500,
            })
            .unwrap();
            uow.commit().unwrap();
        }
        let metrics = doctor_metrics(&store, "company-default", 2_000).unwrap();
        assert_eq!(metrics.depth, 1);
        assert_eq!(metrics.oldest_delivery_latency_ms, Some(1_500));
        assert_eq!(metrics.generation_lag_total, 1);
    }
}
