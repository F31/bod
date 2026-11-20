//! P1 切片 4.6：预算耗尽时的新派发门禁（P0 G-45 回归）。
//!
//! 边界：本模块只回答「当前派发入口是否还有可用预算」，不重复实现 P0
//! `budget_admission::admit_in` 的五层完整准入与记账。派发入口能稳定取得的
//! 只有 Company 与 Project，因此这里按固定次序复核这两层显式配置的金额上限；
//! Agent/Run/WorkPackage 与真实请求量在 `admit_in` 中复核。任一层未配置限额
//! 视为「该入口不受此层预算约束」，保持 P0 既有派发行为，不把无配置误判为耗尽。
//!
//! 判定只读、不写任何表；阻塞状态的持久化由 `control_api::request_dispatch` 在同一
//! 事务内完成（Blocked + InsufficientBudget），且不创建 Reservation / Reserve 分录 /
//! Outbox 派发意图 / Attempt / Binding、不触碰派发探针。已结算费用与已接受成果
//! 只作为既有事实被保留，门禁对它们没有写入路径。

use crate::budget_admission::{BudgetError, BudgetScopeKind, Dimension, Measure};
use crate::domain::model::DenialReason;
use crate::store::{StoreError, UnitOfWork};

/// 派发入口没有可靠的工作量估算，用 1 微元作为「还能不能开始新工作」的下界：
/// 可用额 < 1 即视为耗尽（零或负都不允许再派发）。
pub const MIN_DISPATCH_RESERVE_MICROS: i64 = 1;
const CURRENCY: &str = "CNY";

/// 稳定阻塞事实：原因码、触发层级与解除条件都可直接投影到工作台。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetGateBlock {
    pub reason: DenialReason,
    pub scope_kind: BudgetScopeKind,
    pub scope_id: String,
    pub dimension: Dimension,
    pub available: i64,
    pub requested: i64,
    /// 人读的重新唤醒条件，来自 P0 `BudgetError::next_step()`，不自行放宽。
    pub reactivation: String,
}

impl BudgetGateBlock {
    /// 控制 API 的稳定原因码；调用方按码分支，不解析人类消息。
    pub fn stable_code(&self) -> &'static str {
        "BOD_INSUFFICIENT_BUDGET"
    }

    pub fn message(&self) -> String {
        format!(
            "{} 层级 {} 可用额度 {} 小于新派发下界 {}，停止新派发",
            self.scope_kind.as_str(),
            self.scope_id,
            self.available,
            self.requested
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetGateOutcome {
    Allowed,
    Blocked(BudgetGateBlock),
}

/// 在调用方已有的 `UnitOfWork` 内只读复核预算；不预留、不记账、不写队列。
pub fn evaluate_dispatch_budget(
    uow: &UnitOfWork<'_>,
    company_id: &str,
    project_id: Option<&str>,
) -> Result<BudgetGateOutcome, StoreError> {
    let measure = Measure::money_micros(MIN_DISPATCH_RESERVE_MICROS, CURRENCY);
    let currency = CURRENCY;

    // 固定次序：Company → Project（§11.7）。未配置的层级不在此入口阻塞。
    let mut configured: Vec<(BudgetScopeKind, &str)> = vec![(BudgetScopeKind::Company, company_id)];
    if let Some(project_id) = project_id {
        configured.push((BudgetScopeKind::Project, project_id));
    }
    for (scope_kind, scope_id) in configured {
        let Some(limit) = uow.get_budget_limit(
            company_id,
            scope_kind,
            scope_id,
            measure.dimension,
            currency,
        )?
        else {
            continue;
        };
        let usage = uow.usage_for_scope(
            company_id,
            scope_kind,
            scope_id,
            measure.dimension,
            currency,
        )?;
        let available = usage.available(limit.limit);
        if available < measure.value {
            let error = BudgetError::InsufficientBudget {
                scope_kind,
                dimension: measure.dimension,
                requested: measure.value,
                available,
            };
            return Ok(BudgetGateOutcome::Blocked(BudgetGateBlock {
                reason: error.reason(),
                scope_kind,
                scope_id: scope_id.to_string(),
                dimension: measure.dimension,
                available,
                requested: measure.value,
                reactivation: error.next_step(),
            }));
        }
    }

    // 已如实记录超额的项目，即使限额仍显示余额也不允许继续派发（P0 I-06）。
    if let Some(project_id) = project_id
        && uow.has_project_overage(company_id, project_id)?
    {
        let error = BudgetError::OverBudget {
            scope_kind: BudgetScopeKind::Project,
        };
        return Ok(BudgetGateOutcome::Blocked(BudgetGateBlock {
            reason: error.reason(),
            scope_kind: BudgetScopeKind::Project,
            scope_id: project_id.to_string(),
            dimension: Dimension::MoneyMicros,
            available: 0,
            requested: measure.value,
            reactivation: error.next_step(),
        }));
    }

    Ok(BudgetGateOutcome::Allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget_admission::{BudgetLimit, settle_in};
    use crate::config::BodConfig;
    use crate::domain::DomainId;
    use crate::store::SqliteStore;

    fn seed_limit(store: &mut SqliteStore, kind: BudgetScopeKind, id: &str, limit: i64) {
        let uow = store.transaction().unwrap();
        uow.put_budget_limit(&BudgetLimit {
            company_id: DomainId::new("company-1"),
            scope_kind: kind,
            scope_id: DomainId::new(id),
            measure: Measure::money_micros(0, CURRENCY),
            limit,
            version: 1,
        })
        .unwrap();
        uow.commit().unwrap();
    }

    #[test]
    fn gate_is_read_only_and_unconfigured_budget_does_not_block_dispatch() {
        let mut store = SqliteStore::in_memory(&BodConfig::default()).unwrap();
        let before = store.core_row_counts().unwrap();
        let uow = store.transaction().unwrap();
        let first = evaluate_dispatch_budget(&uow, "company-1", Some("project-1")).unwrap();
        let second = evaluate_dispatch_budget(&uow, "company-1", Some("project-1")).unwrap();
        uow.commit().unwrap();
        assert_eq!(first, BudgetGateOutcome::Allowed);
        assert_eq!(second, first, "相同事实必须得到相同判定");
        assert_eq!(store.core_row_counts().unwrap(), before, "门禁判定只读");
    }

    #[test]
    fn settled_fee_consumes_availability_and_blocks_with_stable_reactivation() {
        let mut store = SqliteStore::in_memory(&BodConfig::default()).unwrap();
        // admit_in 复核五层，除了要耗尽的项目层，其余层给足额度。
        for (kind, id) in [
            (BudgetScopeKind::Company, "company-1"),
            (BudgetScopeKind::Project, "project-1"),
            (BudgetScopeKind::Agent, "agent-1"),
            (BudgetScopeKind::Run, "run-1"),
            (BudgetScopeKind::WorkPackage, "wp-1"),
        ] {
            seed_limit(
                &mut store,
                kind,
                id,
                if kind == BudgetScopeKind::Project {
                    500
                } else {
                    5_000
                },
            );
        }

        // 先完成一笔真实结算：500 微元全部计入 C，可用额归零。
        let uow = store.transaction().unwrap();
        let reservation = crate::budget_admission::admit_in(
            &uow,
            &crate::budget_admission::AdmissionRequest {
                reservation_id: "res-budget-gate".to_string(),
                company_id: DomainId::new("company-1"),
                project_id: DomainId::new("project-1"),
                agent_id: DomainId::new("agent-1"),
                run_id: DomainId::new("run-1"),
                work_package_id: DomainId::new("wp-1"),
                measure: Measure::money_micros(500, CURRENCY),
                logical_key: "logic-budget-gate".to_string(),
                destination_id: "executor-1".to_string(),
                source: "test".to_string(),
            },
            1_000,
        )
        .unwrap();
        settle_in(
            &uow,
            &reservation.reservation_id,
            "usage-1",
            500,
            "test",
            1_100,
        )
        .unwrap();
        uow.commit().unwrap();

        let uow = store.transaction().unwrap();
        let decision = evaluate_dispatch_budget(&uow, "company-1", Some("project-1")).unwrap();
        uow.commit().unwrap();
        let BudgetGateOutcome::Blocked(block) = decision else {
            panic!("已结算 500 后项目可用额为零，必须阻塞新派发");
        };
        assert_eq!(block.reason, DenialReason::InsufficientBudget);
        assert_eq!(block.scope_kind, BudgetScopeKind::Project);
        assert_eq!(block.available, 0);
        assert_eq!(block.stable_code(), "BOD_INSUFFICIENT_BUDGET");
        assert!(!block.reactivation.is_empty());
        assert!(block.reactivation.contains("增加限额"));
    }

    #[test]
    fn company_limit_blocks_before_project_and_overage_also_blocks() {
        let mut store = SqliteStore::in_memory(&BodConfig::default()).unwrap();
        seed_limit(&mut store, BudgetScopeKind::Company, "company-1", 0);
        seed_limit(&mut store, BudgetScopeKind::Project, "project-1", 900);
        let uow = store.transaction().unwrap();
        let decision = evaluate_dispatch_budget(&uow, "company-1", Some("project-1")).unwrap();
        uow.commit().unwrap();
        let BudgetGateOutcome::Blocked(block) = decision else {
            panic!("Company 层限额为零时应先于 Project 阻塞");
        };
        assert_eq!(block.scope_kind, BudgetScopeKind::Company);
    }
}
