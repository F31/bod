//! 原子认领、租约 fencing 与完成确认编排（P0 D2 任务 3.3）。
//!
//! 纯决策在 bod-core::queue，原子性由 store 的条件更新（UPDATE ... WHERE token/state）+ 单写事务保证。
//! 完成确认必须匹配 lease_owner 与 lease_token；wanted > claimed 时转 pending（发现新代数），
//! 否则 idle。租约过期只恢复处理权，是否重派发由 effect_recovery 先 inspect/reconcile 决定。

pub use crate::domain::queue::{
    ClaimGrant, CompletionDecision, QueueError, QueueState, WorkQueueEntry, evaluate_claim,
    evaluate_completion, recover_expired_lease,
};
use crate::store::{StoreError, UnitOfWork};

pub const CLAIM_OWNER: &str = "consumer";

#[derive(Debug)]
pub enum ClaimError {
    Store(StoreError),
    Queue(QueueError),
    /// 竞争失败：另一个消费者已持有租约，本方必须退出，不得启动执行。
    LeaseLost,
}

impl From<StoreError> for ClaimError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<QueueError> for ClaimError {
    fn from(value: QueueError) -> Self {
        Self::Queue(value)
    }
}

/// 在事务内原子认领：先读当前状态做纯决策，再用期望 token 做条件更新。
/// 两个消费者并发时，先提交者递增 token，后者的 CAS 因 token 不匹配失败（i_01）。
pub fn claim_in(
    uow: &UnitOfWork<'_>,
    company_id: &str,
    queue_key: &str,
    owner: &str,
    now_ms: i64,
    lease_ms: i64,
) -> Result<ClaimGrant, ClaimError> {
    let entry = uow
        .get_queue(company_id, queue_key)?
        .ok_or(ClaimError::Queue(QueueError::NotReady {
            state: QueueState::Cancelled,
        }))?;
    let grant = evaluate_claim(&entry, owner, now_ms, lease_ms)?;
    let won = uow.claim_queue_if_ready(
        company_id,
        queue_key,
        entry.lease_token,
        owner,
        now_ms,
        grant.lease_expires_at_ms,
    )?;
    if !won {
        return Err(ClaimError::LeaseLost);
    }
    Ok(grant)
}

/// 完成确认：用 owner/token CAS，成功后按 wanted/claimed 决定 pending 或 idle。
pub fn complete_in(
    uow: &UnitOfWork<'_>,
    company_id: &str,
    queue_key: &str,
    owner: &str,
    token: u64,
    now_ms: i64,
) -> Result<CompletionDecision, ClaimError> {
    let entry = uow
        .get_queue(company_id, queue_key)?
        .ok_or(ClaimError::Queue(QueueError::NotReady {
            state: QueueState::Cancelled,
        }))?;
    let decision = evaluate_completion(&entry, owner, token)?;
    let written = uow.complete_queue_if_leased(
        company_id,
        queue_key,
        owner,
        token,
        decision.completed_generation,
        decision.next_state,
        now_ms,
        None,
        None,
    )?;
    if !written {
        // 读取期间租约被更晚的 token 覆盖：fencing 生效，拒绝旧持有者提交。
        return Err(ClaimError::LeaseLost);
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::DomainId;
    use crate::domain::queue::{WakeOutcome, WakeRequest};
    use crate::store::SqliteStore;
    use crate::wakeup;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_db() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bod-claim-test-{}-{n}.db", std::process::id()))
    }

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

    fn seeded_memory_store() -> SqliteStore {
        let mut store = SqliteStore::in_memory(&BodConfig::default()).unwrap();
        wakeup::accept(&mut store, &wake("k1"), 1_000, 168 * 3_600_000).unwrap();
        store
    }

    #[test]
    fn i_01_claim_sets_claimed_generation_and_increments_token() {
        let mut store = seeded_memory_store();
        let grant = {
            let uow = store.transaction().unwrap();
            let grant = claim_in(
                &uow,
                "company-default",
                "issue-1",
                CLAIM_OWNER,
                1_000,
                30_000,
            )
            .expect("到期 pending 可认领");
            uow.commit().unwrap();
            grant
        };
        assert_eq!(grant.claimed_generation, 1);
        assert_eq!(grant.lease_token, 1);
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, QueueState::Running);
        assert_eq!(entry.claimed_generation, 1);
        assert_eq!(entry.lease_token, 1);
        assert_eq!(entry.lease_owner.as_deref(), Some(CLAIM_OWNER));
    }

    #[test]
    fn i_01_g_2_concurrent_claim_only_one_succeeds() {
        let path = temp_db();
        let cfg = BodConfig::default();
        {
            let mut store = SqliteStore::open(&path, &cfg).unwrap();
            wakeup::accept(&mut store, &wake("k1"), 1_000, 168 * 3_600_000).unwrap();
        }
        let barrier = Arc::new(Barrier::new(2));
        let results: Vec<bool> = thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|i| {
                    let path = path.clone();
                    let cfg = cfg.clone();
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        let mut store = match SqliteStore::open(&path, &cfg) {
                            Ok(s) => s,
                            Err(_) => return false,
                        };
                        let uow = match store.transaction() {
                            Ok(u) => u,
                            // 写锁竞争失败是合法的非成功结果。
                            Err(_) => return false,
                        };
                        let owner = format!("consumer-{i}");
                        let won =
                            claim_in(&uow, "company-default", "issue-1", &owner, 1_000, 30_000)
                                .is_ok();
                        if won {
                            uow.commit().is_ok()
                        } else {
                            let _ = uow.rollback();
                            false
                        }
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            results.iter().filter(|ok| **ok).count(),
            1,
            "并发认领只能有一个成功: {results:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn i_02_stale_token_cannot_confirm_completion() {
        let mut store = seeded_memory_store();
        {
            let uow = store.transaction().unwrap();
            claim_in(
                &uow,
                "company-default",
                "issue-1",
                CLAIM_OWNER,
                1_000,
                30_000,
            )
            .unwrap();
            uow.commit().unwrap();
        }
        // 旧 token 0 完成：被 fencing 拒绝。
        {
            let uow = store.transaction().unwrap();
            let err =
                complete_in(&uow, "company-default", "issue-1", CLAIM_OWNER, 0, 1_100).unwrap_err();
            assert!(matches!(err, ClaimError::Queue(QueueError::LeaseMismatch)));
            uow.rollback().unwrap();
        }
        // 正确 token 1 完成：成功转 idle。
        {
            let uow = store.transaction().unwrap();
            let decision = complete_in(&uow, "company-default", "issue-1", CLAIM_OWNER, 1, 1_100)
                .expect("当前 token 应可确认");
            assert_eq!(decision.next_state, QueueState::Idle);
            uow.commit().unwrap();
        }
    }

    #[test]
    fn i_03_completion_with_newer_generation_goes_pending_not_idle() {
        let mut store = seeded_memory_store();
        {
            let uow = store.transaction().unwrap();
            claim_in(
                &uow,
                "company-default",
                "issue-1",
                CLAIM_OWNER,
                1_000,
                30_000,
            )
            .unwrap();
            uow.commit().unwrap();
        }
        // 运行中新唤醒把 wanted 递增到 2。
        wakeup::accept(&mut store, &wake("k2"), 1_050, 168 * 3_600_000).unwrap();
        let decision = {
            let uow = store.transaction().unwrap();
            let decision = complete_in(&uow, "company-default", "issue-1", CLAIM_OWNER, 1, 1_100)
                .expect("当前 token 可确认");
            uow.commit().unwrap();
            decision
        };
        assert!(decision.newer_work_pending);
        assert_eq!(decision.next_state, QueueState::Pending);
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, QueueState::Pending);
        assert_eq!(entry.wanted_generation, 2);
        assert_eq!(entry.completed_generation, 1);
    }

    #[test]
    fn expired_lease_recovers_processing_right_only() {
        let mut store = seeded_memory_store();
        {
            let uow = store.transaction().unwrap();
            claim_in(
                &uow,
                "company-default",
                "issue-1",
                CLAIM_OWNER,
                1_000,
                5_000,
            )
            .unwrap();
            uow.commit().unwrap();
        }
        {
            let uow = store.transaction().unwrap();
            assert!(
                uow.expire_lease_if_due("company-default", "issue-1", 6_000)
                    .unwrap()
            );
            uow.commit().unwrap();
        }
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.state,
            QueueState::Pending,
            "过期只恢复处理权，派发另由对账决定"
        );
        assert_eq!(entry.lease_token, 1, "fencing token 不回退");
        let _ = recover_expired_lease(&entry, 7_000);
    }

    #[test]
    fn duplicate_receipt_returns_original_lease_not_a_new_one() {
        let mut store = seeded_memory_store();
        {
            let uow = store.transaction().unwrap();
            claim_in(
                &uow,
                "company-default",
                "issue-1",
                CLAIM_OWNER,
                1_000,
                30_000,
            )
            .unwrap();
            uow.commit().unwrap();
        }
        // 重复同一个唤醒逻辑键：原回执不推进代数，租约仍是同一个。
        let outcome = wakeup::accept(&mut store, &wake("k1"), 1_100, 168 * 3_600_000).unwrap();
        assert!(matches!(outcome, WakeOutcome::Existing { .. }));
        let entry = store
            .transaction()
            .unwrap()
            .get_queue("company-default", "issue-1")
            .unwrap()
            .unwrap();
        assert_eq!(entry.lease_token, 1);
        assert_eq!(entry.wanted_generation, 1);
    }
}
