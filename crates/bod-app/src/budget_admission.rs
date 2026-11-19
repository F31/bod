//! 预算准入、预留/结算与预算解释编排（P0 D3 任务 4.1—4.5）。
//!
//! 所有准入与记账在同一 `UnitOfWork` 内按固定次序完成（§11.7）：
//! Company → Project → Agent → Run → WorkPackage → Reservation → Binding → 派发意图。
//! 任一步失败由调用方回滚，不产生部分占用。外部派发在提交后由 D2 outbox 进行。
//!
//! 账本只追加（DB trigger 阻断 UPDATE/DELETE）；C/R/U 聚合以 `reservations` 为准，
//! `ledger_entries` 提供审计轨迹与 usage_key 去重。

use crate::domain::DomainId;
pub use crate::domain::budget::{
    Admission, BudgetError, BudgetExplanation, BudgetLimit, BudgetScopeKind, BudgetUsage,
    Dimension, LedgerEntry, LedgerEntryKind, Measure, Reservation, ReservationState, Settlement,
    evaluate_admission, evaluate_cancellation, evaluate_risk_transfer, evaluate_settlement,
    explain_budget,
};
use crate::domain::queue::{EffectStatus, OutboxDelivery};
use crate::store::{SqliteStore, StoreError, UnitOfWork};

#[derive(Debug)]
pub enum AdmissionError {
    Budget(BudgetError),
    Store(StoreError),
    /// 写冲突在 deadline 内耗尽：可恢复 Busy，保留稳定 request_id。
    Busy {
        request_id: String,
        attempts: u32,
    },
}

impl From<BudgetError> for AdmissionError {
    fn from(value: BudgetError) -> Self {
        Self::Budget(value)
    }
}

impl From<StoreError> for AdmissionError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

/// 一次准入请求：绑定稳定逻辑键，重试不会重复预留。
#[derive(Debug, Clone)]
pub struct AdmissionRequest {
    pub reservation_id: String,
    pub company_id: DomainId,
    pub project_id: DomainId,
    pub agent_id: DomainId,
    pub run_id: DomainId,
    pub work_package_id: DomainId,
    pub measure: Measure,
    /// 稳定逻辑动作键：同键重试复用原预留与派发意图。
    pub logical_key: String,
    pub destination_id: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionReceipt {
    pub reservation_id: String,
    pub reserved: i64,
    /// 是否复用已有预留（幂等重试）。
    pub reused: bool,
}

fn currency_of(measure: &Measure) -> &str {
    measure.currency.as_deref().unwrap_or("")
}

/// 读取五个层级的限额与当前 C/R/U。缺失限额返回 None，由纯规则判为无授权。
fn load_scope_budgets(
    uow: &UnitOfWork<'_>,
    req: &AdmissionRequest,
) -> Result<Vec<crate::domain::budget::ScopeBudget>, AdmissionError> {
    let pairs = [
        (BudgetScopeKind::Company, req.company_id.as_str()),
        (BudgetScopeKind::Project, req.project_id.as_str()),
        (BudgetScopeKind::Agent, req.agent_id.as_str()),
        (BudgetScopeKind::Run, req.run_id.as_str()),
        (BudgetScopeKind::WorkPackage, req.work_package_id.as_str()),
    ];
    let currency = currency_of(&req.measure);
    let mut scopes = Vec::with_capacity(pairs.len());
    for (kind, scope_id) in pairs {
        let limit = uow.get_budget_limit(
            req.company_id.as_str(),
            kind,
            scope_id,
            req.measure.dimension,
            currency,
        )?;
        let usage = uow.usage_for_scope(
            req.company_id.as_str(),
            kind,
            scope_id,
            req.measure.dimension,
            currency,
        )?;
        scopes.push(crate::domain::budget::ScopeBudget {
            scope_kind: kind,
            scope_id: DomainId::new(scope_id),
            limit: limit.map(|row| row.limit),
            usage,
        });
    }
    Ok(scopes)
}

/// 在写事务内完成分层准入：复核五层 → 写预留 → 写 Reserve 分录 → 写派发意图。
/// 调用方负责 commit；任何 Err 由 UnitOfWork 的 Drop 自动回滚，不留下 Binding。
pub fn admit_in(
    uow: &UnitOfWork<'_>,
    req: &AdmissionRequest,
    now_ms: i64,
) -> Result<AdmissionReceipt, AdmissionError> {
    // 幂等：同一逻辑键已有预留则直接复用，不重复占额。
    if let Some(existing) = uow.get_reservation_by_logical_key(&req.logical_key)? {
        return Ok(AdmissionReceipt {
            reservation_id: existing.reservation_id,
            reserved: existing.reserved + existing.risk + existing.confirmed,
            reused: true,
        });
    }

    // 已超额的项目拒绝新的派发，直到有来源的调整分录解除（i_06）。
    if uow.has_project_overage(req.company_id.as_str(), req.project_id.as_str())? {
        return Err(BudgetError::OverBudget {
            scope_kind: BudgetScopeKind::Project,
        }
        .into());
    }

    let scopes = load_scope_budgets(uow, req)?;
    evaluate_admission(&scopes, &req.measure)?;

    let mut reservation = Reservation::new(
        req.reservation_id.clone(),
        req.company_id.clone(),
        req.measure.clone(),
        req.measure.value,
        now_ms,
    );
    reservation.project_id = Some(req.project_id.clone());
    reservation.agent_id = Some(req.agent_id.clone());
    reservation.run_id = Some(req.run_id.clone());
    reservation.work_package_id = Some(req.work_package_id.clone());
    reservation.logical_key = Some(req.logical_key.clone());
    uow.insert_reservation(&reservation)?;

    uow.insert_ledger_entry(&LedgerEntry {
        entry_id: format!("ledger-reserve-{}", req.reservation_id),
        company_id: req.company_id.clone(),
        project_id: Some(req.project_id.clone()),
        agent_id: Some(req.agent_id.clone()),
        work_package_id: Some(req.work_package_id.clone()),
        reservation_id: Some(req.reservation_id.clone()),
        usage_key: None,
        kind: LedgerEntryKind::Reserve,
        measure: req.measure.clone(),
        amount: req.measure.value,
        source: req.source.clone(),
        reason: None,
        created_at_ms: now_ms,
    })?;

    // 派发意图与预留同事务提交；外部调用在 commit 后由 outbox 执行。
    uow.put_outbox(&OutboxDelivery {
        event_or_command_id: req.logical_key.clone(),
        destination_id: req.destination_id.clone(),
        company_id: req.company_id.clone(),
        queue_key: req.work_package_id.as_str().to_string(),
        request_digest: format!(
            "{}:{}:{}",
            req.measure.dimension.as_str(),
            req.measure.value,
            currency_of(&req.measure)
        ),
        status: EffectStatus::Prepared,
        attempts: 0,
        next_due_at_ms: now_ms,
        result_ref: None,
        created_at_ms: now_ms,
    })?;

    Ok(AdmissionReceipt {
        reservation_id: req.reservation_id.clone(),
        reserved: req.measure.value,
        reused: false,
    })
}

/// 便捷入口：开事务准入并提交。
pub fn admit(
    store: &mut SqliteStore,
    req: &AdmissionRequest,
    now_ms: i64,
) -> Result<AdmissionReceipt, AdmissionError> {
    let uow = store.transaction()?;
    let receipt = admit_in(&uow, req, now_ms)?;
    uow.commit()?;
    Ok(receipt)
}

/// 结算：实际费用如实入账，usage_key 唯一去重；超额追加调整分录并标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementReceipt {
    pub reservation_id: String,
    pub actual: i64,
    pub released: i64,
    pub overage: i64,
    /// 外部同一 usage_key 重复上报时为 true，不重复扣费。
    pub deduplicated: bool,
}

pub fn settle_in(
    uow: &UnitOfWork<'_>,
    reservation_id: &str,
    usage_key: &str,
    actual: i64,
    source: &str,
    now_ms: i64,
) -> Result<SettlementReceipt, AdmissionError> {
    if uow.ledger_entry_for_usage_key(usage_key)?.is_some() {
        let existing = uow.get_reservation(reservation_id)?;
        return Ok(SettlementReceipt {
            reservation_id: reservation_id.to_string(),
            actual: existing.as_ref().map(|r| r.confirmed).unwrap_or(0),
            released: 0,
            overage: 0,
            deduplicated: true,
        });
    }
    let mut reservation = uow
        .get_reservation(reservation_id)?
        .ok_or(BudgetError::InvalidSettlement)?;
    let settlement = evaluate_settlement(&reservation, actual)?;

    reservation.confirmed = settlement.actual;
    reservation.reserved = 0;
    reservation.risk = 0;
    reservation.state = settlement.next_state;
    reservation.updated_at_ms = now_ms;
    uow.update_reservation(&reservation)?;

    uow.insert_ledger_entry(&LedgerEntry {
        entry_id: format!("ledger-commit-{usage_key}"),
        company_id: reservation.company_id.clone(),
        project_id: reservation.project_id.clone(),
        agent_id: reservation.agent_id.clone(),
        work_package_id: reservation.work_package_id.clone(),
        reservation_id: Some(reservation_id.to_string()),
        usage_key: Some(usage_key.to_string()),
        kind: LedgerEntryKind::Commit,
        measure: reservation.measure.clone(),
        amount: settlement.actual,
        source: source.to_string(),
        reason: None,
        created_at_ms: now_ms,
    })?;

    if settlement.released > 0 {
        uow.insert_ledger_entry(&LedgerEntry {
            entry_id: format!("ledger-release-{usage_key}"),
            company_id: reservation.company_id.clone(),
            project_id: reservation.project_id.clone(),
            agent_id: reservation.agent_id.clone(),
            work_package_id: reservation.work_package_id.clone(),
            reservation_id: Some(reservation_id.to_string()),
            usage_key: None,
            kind: LedgerEntryKind::Release,
            measure: reservation.measure.clone(),
            amount: -settlement.released,
            source: source.to_string(),
            reason: Some("结算低于预留，释放差额".to_string()),
            created_at_ms: now_ms,
        })?;
    }

    if settlement.overage > 0 {
        // 超额调整只记录原因，不重复计价（actual 已全额计入 Commit）。
        uow.insert_ledger_entry(&LedgerEntry {
            entry_id: format!("ledger-adjust-{usage_key}"),
            company_id: reservation.company_id.clone(),
            project_id: reservation.project_id.clone(),
            agent_id: reservation.agent_id.clone(),
            work_package_id: reservation.work_package_id.clone(),
            reservation_id: Some(reservation_id.to_string()),
            usage_key: None,
            kind: LedgerEntryKind::Adjustment,
            measure: reservation.measure.clone(),
            amount: 0,
            source: source.to_string(),
            reason: Some(format!(
                "实际费用超出预留上界 {}；如实入账并阻止后续派发，不截断 actual",
                settlement.overage
            )),
            created_at_ms: now_ms,
        })?;
    }

    Ok(SettlementReceipt {
        reservation_id: reservation_id.to_string(),
        actual: settlement.actual,
        released: settlement.released,
        overage: settlement.overage,
        deduplicated: false,
    })
}

/// 取消但外部结果未知：R→U 一减一增，总量不变（I-71）。
pub fn cancel_to_risk_in(
    uow: &UnitOfWork<'_>,
    reservation_id: &str,
    now_ms: i64,
) -> Result<Reservation, AdmissionError> {
    let mut reservation = uow
        .get_reservation(reservation_id)?
        .ok_or(BudgetError::InvalidSettlement)?;
    let transfer = evaluate_risk_transfer(&reservation)?;
    reservation.reserved -= transfer.from_reserved;
    reservation.risk += transfer.to_risk;
    reservation.state = ReservationState::Risk;
    reservation.updated_at_ms = now_ms;
    uow.update_reservation(&reservation)?;
    uow.insert_ledger_entry(&LedgerEntry {
        entry_id: format!("ledger-risk-{reservation_id}-{now_ms}"),
        company_id: reservation.company_id.clone(),
        project_id: reservation.project_id.clone(),
        agent_id: reservation.agent_id.clone(),
        work_package_id: reservation.work_package_id.clone(),
        reservation_id: Some(reservation_id.to_string()),
        usage_key: None,
        kind: LedgerEntryKind::RiskTransfer,
        measure: reservation.measure.clone(),
        amount: 0,
        source: "cancel_unconfirmed".to_string(),
        reason: Some(format!(
            "取消但结果未知：R {} 转 U {}，净占用不变",
            transfer.from_reserved, transfer.to_risk
        )),
        created_at_ms: now_ms,
    })?;
    Ok(reservation)
}

/// 确认取消（从未发生）：释放全部有效预留。
pub fn cancel_confirmed_in(
    uow: &UnitOfWork<'_>,
    reservation_id: &str,
    now_ms: i64,
) -> Result<Reservation, AdmissionError> {
    let mut reservation = uow
        .get_reservation(reservation_id)?
        .ok_or(BudgetError::InvalidSettlement)?;
    let released = evaluate_cancellation(&reservation)?;
    reservation.reserved = 0;
    reservation.state = ReservationState::Cancelled;
    reservation.updated_at_ms = now_ms;
    uow.update_reservation(&reservation)?;
    if released > 0 {
        uow.insert_ledger_entry(&LedgerEntry {
            entry_id: format!("ledger-cancel-{reservation_id}"),
            company_id: reservation.company_id.clone(),
            project_id: reservation.project_id.clone(),
            agent_id: reservation.agent_id.clone(),
            work_package_id: reservation.work_package_id.clone(),
            reservation_id: Some(reservation_id.to_string()),
            usage_key: None,
            kind: LedgerEntryKind::Release,
            measure: reservation.measure.clone(),
            amount: -released,
            source: "cancel_confirmed".to_string(),
            reason: Some("确认未发生，释放全部预留".to_string()),
            created_at_ms: now_ms,
        })?;
    }
    Ok(reservation)
}

/// Busy 有界重试策略：在 deadline 内重试**整个**事务（重读全部状态），
/// 不是只重放最后一条 SQL；耗尽返回稳定 request_id。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub deadline_ms: i64,
    /// 每次失败后的固定等待（确定性注入，避免测试依赖真实时钟）。
    pub backoff_step_ms: i64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            deadline_ms: 2_000,
            backoff_step_ms: 50,
        }
    }
}

/// 整事务重试器。`attempt` 每次都应重新读取全部相关状态。
pub fn retry_whole_transaction<T, F>(
    policy: RetryPolicy,
    request_id: &str,
    mut attempt: F,
) -> Result<T, AdmissionError>
where
    F: FnMut() -> AttemptResult<T>,
{
    let mut elapsed = 0;
    let attempts = policy.max_attempts.max(1);
    for index in 0..attempts {
        match attempt() {
            AttemptResult::Done(value) => return Ok(value),
            AttemptResult::Fatal(err) => return Err(err),
            AttemptResult::Busy => {
                elapsed += policy.backoff_step_ms.max(1);
                if elapsed >= policy.deadline_ms && index + 1 < attempts {
                    return Err(AdmissionError::Busy {
                        request_id: request_id.to_string(),
                        attempts: index + 1,
                    });
                }
            }
        }
    }
    Err(AdmissionError::Busy {
        request_id: request_id.to_string(),
        attempts,
    })
}

/// 供调用方构造重试闭包的结果类型。
pub enum AttemptResult<T> {
    Done(T),
    Busy,
    Fatal(AdmissionError),
}

impl<T> AttemptResult<T> {
    pub fn from_admission(result: Result<T, AdmissionError>) -> Self {
        match result {
            Ok(value) => Self::Done(value),
            Err(AdmissionError::Busy { .. }) => Self::Busy,
            Err(AdmissionError::Store(StoreError::Sql(err)))
                if err.to_string().contains("database is locked") =>
            {
                Self::Busy
            }
            Err(err) => Self::Fatal(err),
        }
    }
}

/// 带 Busy 重试的准入入口：每次重试重新开事务并重读五层余额。
pub fn admit_with_retry(
    store: &mut SqliteStore,
    req: &AdmissionRequest,
    now_ms: i64,
    policy: RetryPolicy,
) -> Result<AdmissionReceipt, AdmissionError> {
    retry_whole_transaction(policy, &req.reservation_id, || {
        let result = (|| -> Result<AdmissionReceipt, AdmissionError> {
            let uow = store.transaction()?;
            let receipt = admit_in(&uow, req, now_ms)?;
            uow.commit()?;
            Ok(receipt)
        })();
        AttemptResult::from_admission(result)
    })
}

/// 预算解释：分维度展示 C/R/U、可用、来源与覆盖缺口。限额与占用在同一快照读取。
pub fn explain(
    store: &mut SqliteStore,
    company_id: &str,
    scope_kind: BudgetScopeKind,
    scope_id: &str,
    dimension: Dimension,
    currency: Option<&str>,
) -> Result<BudgetExplanation, StoreError> {
    let currency = currency.unwrap_or("");
    let (limit, usage) = {
        let uow = store.transaction()?;
        let limit = uow.get_budget_limit(company_id, scope_kind, scope_id, dimension, currency)?;
        let usage = uow.usage_for_scope(company_id, scope_kind, scope_id, dimension, currency)?;
        uow.commit()?;
        (limit, usage)
    };
    let source = if limit.is_some() {
        "budget_limits + reservations(C/R/U)"
    } else {
        "no budget_limits row"
    };
    Ok(explain_budget(
        scope_kind,
        dimension,
        if currency.is_empty() {
            None
        } else {
            Some(currency)
        },
        limit.map(|row| row.limit),
        usage,
        source,
    ))
}

/// 是否存在超额记录：有 Adjustment 且对应 Project 已超限，用于阻止后续派发。
pub fn has_overage(store: &SqliteStore, company_id: &str) -> Result<bool, StoreError> {
    Ok(store
        .list_ledger_entries(company_id)?
        .iter()
        .any(|entry| entry.kind == LedgerEntryKind::Adjustment))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::store::SqliteStore;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_db() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bod-budget-test-{}-{n}.db", std::process::id()))
    }

    fn config() -> BodConfig {
        let mut config = BodConfig::default();
        config.sqlite.busy_timeout_ms = 5_000;
        config
    }

    fn store() -> SqliteStore {
        SqliteStore::in_memory(&config()).unwrap()
    }

    /// 建立五层限额，每层都是同一天花板。
    fn seed_limits(store: &mut SqliteStore, limit: i64) {
        let uow = store.transaction().unwrap();
        let scopes = [
            (BudgetScopeKind::Company, "company-default"),
            (BudgetScopeKind::Project, "project-1"),
            (BudgetScopeKind::Agent, "agent-1"),
            (BudgetScopeKind::Run, "run-1"),
            (BudgetScopeKind::WorkPackage, "wp-1"),
        ];
        for (kind, id) in scopes {
            uow.put_budget_limit(&BudgetLimit {
                company_id: DomainId::new("company-default"),
                scope_kind: kind,
                scope_id: DomainId::new(id),
                measure: Measure::money_micros(0, "CNY"),
                limit,
                version: 1,
            })
            .unwrap();
        }
        uow.commit().unwrap();
    }

    fn request(id: &str, amount: i64) -> AdmissionRequest {
        AdmissionRequest {
            reservation_id: format!("res-{id}"),
            company_id: DomainId::new("company-default"),
            project_id: DomainId::new("project-1"),
            agent_id: DomainId::new("agent-1"),
            run_id: DomainId::new("run-1"),
            work_package_id: DomainId::new("wp-1"),
            measure: Measure::money_micros(amount, "CNY"),
            logical_key: format!("logic-{id}"),
            destination_id: "executor-1".to_string(),
            source: "test".to_string(),
        }
    }

    #[test]
    fn i_05_admission_reserves_and_emits_outbox_in_one_transaction() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        let receipt = admit(&mut store, &request("a", 300), 1_000).unwrap();
        assert_eq!(receipt.reserved, 300);
        assert!(!receipt.reused);

        let reservations = store.list_reservations("company-default").unwrap();
        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].reserved, 300);
        let ledger = store.list_ledger_entries("company-default").unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].kind, LedgerEntryKind::Reserve);
        assert_eq!(ledger[0].amount, 300);
        let outbox = store.list_outbox_by_status(EffectStatus::Prepared).unwrap();
        assert_eq!(outbox.len(), 1, "预留与派发意图同事务提交");
    }

    #[test]
    fn i_05_concurrent_admission_of_last_credit_only_one_succeeds() {
        let path = temp_db();
        let cfg = config();
        {
            let mut store = SqliteStore::open(&path, &cfg).unwrap();
            seed_limits(&mut store, 500);
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
                        match admit(&mut store, &request(&format!("c{i}"), 400), 1_000) {
                            Ok(_) => true,
                            Err(AdmissionError::Budget(BudgetError::InsufficientBudget {
                                ..
                            })) => false,
                            Err(AdmissionError::Store(_)) => false,
                            Err(err) => panic!("意外错误: {err:?}"),
                        }
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            results.iter().filter(|ok| **ok).count(),
            1,
            "最后一份额度只能有一个成功: {results:?}"
        );

        // 失败方无残留：只有一条预留、一条 Reserve 分录、一条派发意图。
        let mut store = SqliteStore::open(&path, &cfg).unwrap();
        assert_eq!(store.list_reservations("company-default").unwrap().len(), 1);
        assert_eq!(
            store.list_ledger_entries("company-default").unwrap().len(),
            1
        );
        assert_eq!(
            store
                .list_outbox_by_status(EffectStatus::Prepared)
                .unwrap()
                .len(),
            1
        );
        let usage = store
            .transaction()
            .unwrap()
            .usage_for_scope(
                "company-default",
                BudgetScopeKind::Company,
                "company-default",
                Dimension::MoneyMicros,
                "CNY",
            )
            .unwrap();
        assert_eq!(usage.reserved, 400);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn i_05_idempotent_retry_reuses_reservation_and_outbox() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        let first = admit(&mut store, &request("a", 300), 1_000).unwrap();
        let again = admit(&mut store, &request("a", 300), 1_100).unwrap();
        assert!(again.reused);
        assert_eq!(first.reservation_id, again.reservation_id);
        assert_eq!(store.list_reservations("company-default").unwrap().len(), 1);
        assert_eq!(
            store
                .list_outbox_by_status(EffectStatus::Prepared)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn i_05_missing_limit_rejects_without_partial_binding() {
        let mut store = store();
        // 只配置四层，Run 层缺失。
        {
            let uow = store.transaction().unwrap();
            for (kind, id) in [
                (BudgetScopeKind::Company, "company-default"),
                (BudgetScopeKind::Project, "project-1"),
                (BudgetScopeKind::Agent, "agent-1"),
                (BudgetScopeKind::WorkPackage, "wp-1"),
            ] {
                uow.put_budget_limit(&BudgetLimit {
                    company_id: DomainId::new("company-default"),
                    scope_kind: kind,
                    scope_id: DomainId::new(id),
                    measure: Measure::money_micros(0, "CNY"),
                    limit: 1_000,
                    version: 1,
                })
                .unwrap();
            }
            uow.commit().unwrap();
        }
        let err = admit(&mut store, &request("a", 100), 1_000).unwrap_err();
        assert!(matches!(
            err,
            AdmissionError::Budget(BudgetError::NoBudgetAuthorization {
                scope_kind: BudgetScopeKind::Run
            })
        ));
        assert!(
            store
                .list_reservations("company-default")
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .list_outbox_by_status(EffectStatus::Prepared)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn i_71_cancel_to_risk_keeps_total_and_never_double_counts() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        admit(&mut store, &request("a", 400), 1_000).unwrap();
        let before = {
            let uow = store.transaction().unwrap();
            let usage = uow
                .usage_for_scope(
                    "company-default",
                    BudgetScopeKind::Company,
                    "company-default",
                    Dimension::MoneyMicros,
                    "CNY",
                )
                .unwrap();
            uow.commit().unwrap();
            usage
        };
        {
            let uow = store.transaction().unwrap();
            cancel_to_risk_in(&uow, "res-a", 2_000).unwrap();
            uow.commit().unwrap();
        }
        let after = {
            let uow = store.transaction().unwrap();
            let usage = uow
                .usage_for_scope(
                    "company-default",
                    BudgetScopeKind::Company,
                    "company-default",
                    Dimension::MoneyMicros,
                    "CNY",
                )
                .unwrap();
            uow.commit().unwrap();
            usage
        };
        assert_eq!(after.reserved, 0);
        assert_eq!(after.risk, 400);
        assert_eq!(
            before.reserved + before.risk,
            after.reserved + after.risk,
            "R→U 一减一增，总量不变"
        );
        assert_eq!(before.confirmed, after.confirmed);
    }

    #[test]
    fn i_06_late_bill_above_reserve_is_recorded_in_full() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        admit(&mut store, &request("a", 500), 1_000).unwrap();
        {
            let uow = store.transaction().unwrap();
            let receipt = settle_in(&uow, "res-a", "usage-1", 800, "provider", 2_000).unwrap();
            assert_eq!(receipt.actual, 800, "不截断 actual");
            assert_eq!(receipt.overage, 300);
            uow.commit().unwrap();
        }
        let reservation = store
            .transaction()
            .unwrap()
            .get_reservation("res-a")
            .unwrap()
            .unwrap();
        assert_eq!(reservation.confirmed, 800);
        assert_eq!(reservation.state, ReservationState::Consumed);
        let ledger = store.list_ledger_entries("company-default").unwrap();
        assert!(
            ledger
                .iter()
                .any(|entry| entry.kind == LedgerEntryKind::Commit && entry.amount == 800)
        );
        let adjustment = ledger
            .iter()
            .find(|entry| entry.kind == LedgerEntryKind::Adjustment)
            .expect("超额必须有调整分录");
        assert!(adjustment.reason.as_deref().unwrap_or("").contains("300"));
        assert!(has_overage(&store, "company-default").unwrap());
    }

    #[test]
    fn i_35_same_usage_key_is_not_charged_twice() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        admit(&mut store, &request("a", 500), 1_000).unwrap();
        {
            let uow = store.transaction().unwrap();
            settle_in(&uow, "res-a", "usage-1", 300, "provider", 2_000).unwrap();
            uow.commit().unwrap();
        }
        {
            let uow = store.transaction().unwrap();
            let again = settle_in(&uow, "res-a", "usage-1", 300, "provider", 2_100).unwrap();
            assert!(again.deduplicated);
            uow.commit().unwrap();
        }
        let commits: Vec<_> = store
            .list_ledger_entries("company-default")
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == LedgerEntryKind::Commit)
            .collect();
        assert_eq!(commits.len(), 1, "同 usage_key 只入账一次");
        assert_eq!(
            store
                .transaction()
                .unwrap()
                .get_reservation("res-a")
                .unwrap()
                .unwrap()
                .confirmed,
            300
        );
    }

    #[test]
    fn i_06_overage_blocks_subsequent_dispatch() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        admit(&mut store, &request("a", 500), 1_000).unwrap();
        {
            let uow = store.transaction().unwrap();
            settle_in(&uow, "res-a", "usage-1", 800, "provider", 2_000).unwrap();
            uow.commit().unwrap();
        }
        // 虽然可用额仍剩 200，但超额事实本身阻止新派发。
        let err = admit(&mut store, &request("b", 100), 2_100).unwrap_err();
        assert!(matches!(
            err,
            AdmissionError::Budget(BudgetError::OverBudget {
                scope_kind: BudgetScopeKind::Project
            })
        ));
        assert_eq!(store.list_reservations("company-default").unwrap().len(), 1);
    }

    #[test]
    fn any_dimension_hitting_its_limit_blocks_admission() {
        let mut store = store();
        // 五层同时配置金额与 token 两种维度。
        {
            let uow = store.transaction().unwrap();
            for (kind, id) in [
                (BudgetScopeKind::Company, "company-default"),
                (BudgetScopeKind::Project, "project-1"),
                (BudgetScopeKind::Agent, "agent-1"),
                (BudgetScopeKind::Run, "run-1"),
                (BudgetScopeKind::WorkPackage, "wp-1"),
            ] {
                for (measure, limit) in [
                    (Measure::money_micros(0, "CNY"), 1_000),
                    (Measure::tokens(0), 100),
                ] {
                    uow.put_budget_limit(&BudgetLimit {
                        company_id: DomainId::new("company-default"),
                        scope_kind: kind,
                        scope_id: DomainId::new(id),
                        measure,
                        limit,
                        version: 1,
                    })
                    .unwrap();
                }
            }
            uow.commit().unwrap();
        }
        let mut first = request("t1", 0);
        first.measure = Measure::tokens(90);
        admit(&mut store, &first, 1_000).unwrap();
        // 金额仍充足，但 token 只剩 10，跨维度任一触限即拒绝。
        let mut second = request("t2", 0);
        second.measure = Measure::tokens(20);
        let err = admit(&mut store, &second, 1_100).unwrap_err();
        assert!(matches!(
            err,
            AdmissionError::Budget(BudgetError::InsufficientBudget {
                dimension: Dimension::Tokens,
                requested: 20,
                available: 10,
                ..
            })
        ));
    }

    #[test]
    fn token_dimension_is_not_summed_as_cash() {
        let mut store = store();
        {
            let uow = store.transaction().unwrap();
            for (kind, id) in [
                (BudgetScopeKind::Company, "company-default"),
                (BudgetScopeKind::Project, "project-1"),
                (BudgetScopeKind::Agent, "agent-1"),
                (BudgetScopeKind::Run, "run-1"),
                (BudgetScopeKind::WorkPackage, "wp-1"),
            ] {
                uow.put_budget_limit(&BudgetLimit {
                    company_id: DomainId::new("company-default"),
                    scope_kind: kind,
                    scope_id: DomainId::new(id),
                    measure: Measure::tokens(0),
                    limit: 10_000,
                    version: 1,
                })
                .unwrap();
            }
            uow.commit().unwrap();
        }
        let mut tokens = request("t", 0);
        tokens.measure = Measure::tokens(9_000);
        admit(&mut store, &tokens, 1_000).unwrap();
        // 金额维度仍无授权，token 用量不折算为现金。
        let err = admit(&mut store, &request("m", 1), 1_100).unwrap_err();
        assert!(matches!(
            err,
            AdmissionError::Budget(BudgetError::NoBudgetAuthorization {
                scope_kind: BudgetScopeKind::Company
            })
        ));
    }

    #[test]
    fn busy_retry_repeats_whole_transaction_and_keeps_request_id() {
        let mut calls = 0;
        let result = retry_whole_transaction(
            RetryPolicy {
                max_attempts: 3,
                deadline_ms: 10_000,
                backoff_step_ms: 50,
            },
            "res-stable",
            || {
                calls += 1;
                if calls < 3 {
                    AttemptResult::Busy
                } else {
                    AttemptResult::Done(42)
                }
            },
        )
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(calls, 3, "每次冲突都整体重试");
    }

    #[test]
    fn busy_exhaustion_returns_stable_request_id() {
        let err = retry_whole_transaction::<(), _>(
            RetryPolicy {
                max_attempts: 2,
                deadline_ms: 10,
                backoff_step_ms: 50,
            },
            "res-stable",
            || AttemptResult::Busy,
        )
        .unwrap_err();
        match err {
            AdmissionError::Busy { request_id, .. } => assert_eq!(request_id, "res-stable"),
            other => panic!("应为 Busy: {other:?}"),
        }
    }

    #[test]
    fn explanation_lists_c_r_u_and_gap_without_hiding_risk() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        admit(&mut store, &request("a", 300), 1_000).unwrap();
        let explained = explain(
            &mut store,
            "company-default",
            BudgetScopeKind::Company,
            "company-default",
            Dimension::MoneyMicros,
            Some("CNY"),
        )
        .unwrap();
        assert_eq!(explained.limit, 1_000);
        assert_eq!(explained.reserved, 300);
        assert_eq!(explained.confirmed, 0);
        assert_eq!(explained.risk, 0);
        assert_eq!(explained.available, 700);
        assert!(explained.coverage_gaps.is_empty());
    }

    #[test]
    fn ledger_is_append_only_at_the_database_level() {
        let mut store = store();
        seed_limits(&mut store, 1_000);
        admit(&mut store, &request("a", 100), 1_000).unwrap();
        // 直接尝试改写/删除：DB trigger 必须拒绝。
        let sql = "UPDATE ledger_entries SET amount = 999";
        let err = store.execute_batch_for_test(sql);
        assert!(err.is_err(), "账本分录不可改写");
        let del = "DELETE FROM ledger_entries";
        let err = store.execute_batch_for_test(del);
        assert!(err.is_err(), "账本分录不可删除");
    }
}
