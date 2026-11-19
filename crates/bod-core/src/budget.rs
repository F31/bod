//! P0 D3 预算纯规则：多维约束、定点金额、分层准入、C/R/U 互斥、超额与账本分录。
//! 纯值类型 + 确定性决策，不依赖 DB/Web/Tauri（G-1），不读时钟、不用浮点累计。
//!
//! 核心等式（§11.3、§11.10）：可用额度 = 限额 − 已确认 C − 有效预留 R − 待对账风险 U。
//! 同一未结算金额只能处于 R 或 U 之一；结算增加 C 的同时减少对应未结算占用。

use crate::DomainId;
use crate::model::DenialReason;

/// 计量维度。不同单位不得直接相加（§11.1）；金额带币种，token 按模型口径分列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dimension {
    /// 金额，整数最小计价单位（如 micro-CNY），必须带币种。
    MoneyMicros,
    Tokens,
    Calls,
    DurationMs,
    DiskBytes,
    Concurrency,
}

impl Dimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MoneyMicros => "money_micros",
            Self::Tokens => "tokens",
            Self::Calls => "calls",
            Self::DurationMs => "duration_ms",
            Self::DiskBytes => "disk_bytes",
            Self::Concurrency => "concurrency",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "money_micros" => Some(Self::MoneyMicros),
            "tokens" => Some(Self::Tokens),
            "calls" => Some(Self::Calls),
            "duration_ms" => Some(Self::DurationMs),
            "disk_bytes" => Some(Self::DiskBytes),
            "concurrency" => Some(Self::Concurrency),
            _ => None,
        }
    }

    /// 是否需要币种；非金额维度不得参与现金求和。
    pub fn requires_currency(self) -> bool {
        matches!(self, Self::MoneyMicros)
    }
}

/// 一次计量：维度 + 整数值 + 可选币种。金额必须是整数最小计价单位（§11.10）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measure {
    pub dimension: Dimension,
    pub value: i64,
    pub currency: Option<String>,
}

impl Measure {
    pub fn money_micros(value: i64, currency: impl Into<String>) -> Self {
        Self {
            dimension: Dimension::MoneyMicros,
            value,
            currency: Some(currency.into()),
        }
    }

    pub fn tokens(value: i64) -> Self {
        Self {
            dimension: Dimension::Tokens,
            value,
            currency: None,
        }
    }

    pub fn calls(value: i64) -> Self {
        Self {
            dimension: Dimension::Calls,
            value,
            currency: None,
        }
    }

    /// 金额必须带币种、非金额不得带币种；负值一律拒绝（预留在别处表达方向）。
    pub fn validate(&self) -> Result<(), BudgetError> {
        if self.value < 0 {
            return Err(BudgetError::NegativeMeasure);
        }
        match (self.dimension.requires_currency(), &self.currency) {
            (true, Some(currency)) if !currency.trim().is_empty() => Ok(()),
            (true, _) => Err(BudgetError::MissingCurrency),
            (false, None) => Ok(()),
            (false, Some(_)) => Err(BudgetError::UnexpectedCurrency),
        }
    }
}

/// 预算层级；准入按此固定顺序复核（§11.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BudgetScopeKind {
    Company,
    Project,
    Agent,
    Run,
    WorkPackage,
}

impl BudgetScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Company => "company",
            Self::Project => "project",
            Self::Agent => "agent",
            Self::Run => "run",
            Self::WorkPackage => "work_package",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "company" => Some(Self::Company),
            "project" => Some(Self::Project),
            "agent" => Some(Self::Agent),
            "run" => Some(Self::Run),
            "work_package" => Some(Self::WorkPackage),
            _ => None,
        }
    }

    /// 固定准入次序，供测试与文档对齐。
    pub const ADMISSION_ORDER: [Self; 5] = [
        Self::Company,
        Self::Project,
        Self::Agent,
        Self::Run,
        Self::WorkPackage,
    ];
}

/// 某一层级的限额。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetLimit {
    pub company_id: DomainId,
    pub scope_kind: BudgetScopeKind,
    pub scope_id: DomainId,
    pub measure: Measure,
    pub limit: i64,
    pub version: u64,
}

/// 一次逻辑支出的预留/结算状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReservationState {
    /// 有效预留 R。
    Active,
    /// 已结算：金额计入 C，不再占 R/U。
    Consumed,
    /// 已取消且确认未发生：全部释放。
    Cancelled,
    /// 过期或取消但结果未知：剩余占用转 U。
    Risk,
}

impl ReservationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Consumed => "consumed",
            Self::Cancelled => "cancelled",
            Self::Risk => "risk",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "consumed" => Some(Self::Consumed),
            "cancelled" => Some(Self::Cancelled),
            "risk" => Some(Self::Risk),
            _ => None,
        }
    }
}

/// 一条逻辑支出：C/R/U 三个量互斥不重叠（I-71）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub reservation_id: String,
    pub company_id: DomainId,
    pub project_id: Option<DomainId>,
    pub agent_id: Option<DomainId>,
    pub run_id: Option<DomainId>,
    pub work_package_id: Option<DomainId>,
    pub measure: Measure,
    /// 有效预留 R。
    pub reserved: i64,
    /// 已确认消费 C。
    pub confirmed: i64,
    /// 待对账风险占用 U。
    pub risk: i64,
    pub state: ReservationState,
    /// 绑定稳定逻辑键，便于 effect/outbox 关联与去重。
    pub logical_key: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Reservation {
    pub fn new(
        reservation_id: impl Into<String>,
        company_id: DomainId,
        measure: Measure,
        reserved: i64,
        now_ms: i64,
    ) -> Self {
        Self {
            reservation_id: reservation_id.into(),
            company_id,
            project_id: None,
            agent_id: None,
            run_id: None,
            work_package_id: None,
            measure,
            reserved,
            confirmed: 0,
            risk: 0,
            state: ReservationState::Active,
            logical_key: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }

    /// 未结算占用：R + U。二者互斥，不得重复计入同一金额。
    pub fn unsettled(&self) -> i64 {
        self.reserved + self.risk
    }

    /// 该预留对某一层级的 C/R/U 贡献。
    pub fn contribution(&self) -> (i64, i64, i64) {
        (self.confirmed, self.reserved, self.risk)
    }
}

/// 某层级某维度的当前占用快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetUsage {
    pub confirmed: i64,
    pub reserved: i64,
    pub risk: i64,
}

impl BudgetUsage {
    pub fn available(&self, limit: i64) -> i64 {
        limit - self.confirmed - self.reserved - self.risk
    }

    pub fn add(&mut self, other: BudgetUsage) {
        self.confirmed += other.confirmed;
        self.reserved += other.reserved;
        self.risk += other.risk;
    }
}

/// 账本分录类型；只追加、不可改写（§11.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LedgerEntryKind {
    /// 建立预留（R 增）。
    Reserve,
    /// 释放预留（R 减）。
    Release,
    /// 结算确认（C 增、R 减）。
    Commit,
    /// 转风险占用（R 减、U 增）。
    RiskTransfer,
    /// 超额调整（有来源，不截断 actual）。
    Adjustment,
}

impl LedgerEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserve => "reserve",
            Self::Release => "release",
            Self::Commit => "commit",
            Self::RiskTransfer => "risk_transfer",
            Self::Adjustment => "adjustment",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reserve" => Some(Self::Reserve),
            "release" => Some(Self::Release),
            "commit" => Some(Self::Commit),
            "risk_transfer" => Some(Self::RiskTransfer),
            "adjustment" => Some(Self::Adjustment),
            _ => None,
        }
    }
}

/// 不可变账本分录。`amount` 为有符号整数：入账为正、释放为负；跨维度不得相加。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub entry_id: String,
    pub company_id: DomainId,
    pub project_id: Option<DomainId>,
    pub agent_id: Option<DomainId>,
    pub work_package_id: Option<DomainId>,
    pub reservation_id: Option<String>,
    /// 外部计量唯一键；同一 usage_key 只允许一条实际费用分录（I-35）。
    pub usage_key: Option<String>,
    pub kind: LedgerEntryKind,
    pub measure: Measure,
    pub amount: i64,
    pub source: String,
    pub reason: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetError {
    /// 负数计量。
    NegativeMeasure,
    /// 金额缺币种。
    MissingCurrency,
    /// 非金额维度带了币种。
    UnexpectedCurrency,
    /// 可用额度不足；记录触发的层级与缺口（稳定拒绝原因）。
    InsufficientBudget {
        scope_kind: BudgetScopeKind,
        dimension: Dimension,
        requested: i64,
        available: i64,
    },
    /// 没有任何该层级的限额配置：默认无隐式付费授权（§11.5）。
    NoBudgetAuthorization { scope_kind: BudgetScopeKind },
    /// 已超额：新的派发一律拒绝，直到有来源的调整分录解除。
    OverBudget { scope_kind: BudgetScopeKind },
    /// 同 usage_key 重复上报：按 I-35 去重，不重复扣费。
    DuplicateUsageKey { usage_key: String },
    /// 结算金额不得为负。
    InvalidSettlement,
    /// 预留已结算或已取消，不能再转换。
    ReservationNotActive { state: ReservationState },
    /// 释放金额超过当前预留。
    ReleaseExceedsReserved { reserved: i64, requested: i64 },
}

impl BudgetError {
    /// 稳定拒绝原因码，供控制 API 与预算解释复用。
    pub fn reason(&self) -> DenialReason {
        match self {
            Self::InsufficientBudget { .. }
            | Self::NoBudgetAuthorization { .. }
            | Self::OverBudget { .. } => DenialReason::InsufficientBudget,
            _ => DenialReason::InsufficientBudget,
        }
    }

    /// 允许的下一步（人读），不把未知占用显示为可用余额。
    pub fn next_step(&self) -> String {
        match self {
            Self::InsufficientBudget {
                scope_kind,
                dimension,
                requested,
                available,
            } => format!(
                "在 {} 层级为 {} 增加限额或降低请求量（请求 {requested}，可用 {available}）；\
                 调整后重新唤醒该工作包",
                scope_kind.as_str(),
                dimension.as_str()
            ),
            Self::NoBudgetAuthorization { scope_kind } => format!(
                "当前没有 {} 层级的付费授权；请显式设置金额上限，或改用免费/观察模式",
                scope_kind.as_str()
            ),
            Self::OverBudget { scope_kind } => format!(
                "{} 已超额；先补记有来源的调整分录或提高限额，再允许新派发",
                scope_kind.as_str()
            ),
            Self::DuplicateUsageKey { usage_key } => {
                format!("usage_key {usage_key} 已入账；重复上报被去重，不追加费用")
            }
            Self::ReservationNotActive { state } => {
                format!("该预留已是 {} 状态，不能再转换", state.as_str())
            }
            Self::ReleaseExceedsReserved { .. } | Self::InvalidSettlement => {
                "核对预留在途金额后重新提交结算或释放".to_string()
            }
            Self::NegativeMeasure | Self::MissingCurrency | Self::UnexpectedCurrency => {
                "按维度使用非负整数计量；金额必须带币种，非金额不得带币种".to_string()
            }
        }
    }
}

/// 一次准入需要检查的层级限额与当前占用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeBudget {
    pub scope_kind: BudgetScopeKind,
    pub scope_id: DomainId,
    pub limit: Option<i64>,
    pub usage: BudgetUsage,
}

/// 准入结果：通过的层级顺序 + 请求量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub requested: Measure,
    pub approved_scopes: Vec<BudgetScopeKind>,
}

/// 固定顺序分层准入（I-05）。同一事务内对每个层级复核可用额；任一层不足即拒绝，
/// 调用方必须整体回滚，不得部分占用。缺少限额视为无授权（默认不授予消费额度）。
pub fn evaluate_admission(
    scopes: &[ScopeBudget],
    requested: &Measure,
) -> Result<Admission, BudgetError> {
    requested.validate()?;
    let mut approved = Vec::new();
    for kind in BudgetScopeKind::ADMISSION_ORDER {
        let scope = scopes
            .iter()
            .find(|candidate| candidate.scope_kind == kind)
            .ok_or(BudgetError::NoBudgetAuthorization { scope_kind: kind })?;
        let limit = scope
            .limit
            .ok_or(BudgetError::NoBudgetAuthorization { scope_kind: kind })?;
        let available = scope.usage.available(limit);
        if available < requested.value {
            return Err(BudgetError::InsufficientBudget {
                scope_kind: kind,
                dimension: requested.dimension,
                requested: requested.value,
                available,
            });
        }
        approved.push(kind);
    }
    Ok(Admission {
        requested: requested.clone(),
        approved_scopes: approved,
    })
}

/// 结算决策：实际费用可能高于或低于预留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    /// 计入 C 的实际金额。
    pub actual: i64,
    /// 释回的预留（actual 小于 reserved 时）。
    pub released: i64,
    /// 超出预留、需如实追加的超额部分。
    pub overage: i64,
    /// 结算后该预留的下一状态。
    pub next_state: ReservationState,
}

/// 全量结算（§11.3）：不截断 actual 来维持 settle ≤ reserve 的假平衡。
pub fn evaluate_settlement(
    reservation: &Reservation,
    actual: i64,
) -> Result<Settlement, BudgetError> {
    if reservation.state != ReservationState::Active && reservation.state != ReservationState::Risk
    {
        return Err(BudgetError::ReservationNotActive {
            state: reservation.state,
        });
    }
    if actual < 0 {
        return Err(BudgetError::InvalidSettlement);
    }
    let remaining = reservation.reserved + reservation.risk;
    let released = (remaining - actual).max(0);
    let overage = (actual - remaining).max(0);
    Ok(Settlement {
        actual,
        released,
        overage,
        next_state: ReservationState::Consumed,
    })
}

/// 取消/过期但无法确认是否发生：R→U 一减一增，总量不变（I-71）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskTransfer {
    pub from_reserved: i64,
    pub to_risk: i64,
}

pub fn evaluate_risk_transfer(reservation: &Reservation) -> Result<RiskTransfer, BudgetError> {
    if reservation.state != ReservationState::Active {
        return Err(BudgetError::ReservationNotActive {
            state: reservation.state,
        });
    }
    Ok(RiskTransfer {
        from_reserved: reservation.reserved,
        to_risk: reservation.reserved,
    })
}

/// 确认从未发生的取消：释放全部有效预留，不转 U。
pub fn evaluate_cancellation(reservation: &Reservation) -> Result<i64, BudgetError> {
    if reservation.state != ReservationState::Active {
        return Err(BudgetError::ReservationNotActive {
            state: reservation.state,
        });
    }
    Ok(reservation.reserved)
}

/// 计费模式（§11.5）：没有显式付费授权时不得自动授予可消费额度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BillingMode {
    /// 未配置任何付费额度：只能免费/离线运行，界面必须明确标识。
    Unpaid,
    /// 用户显式设置额度：允许受管付费派发，仍受 C/R/U 约束。
    Authorized,
    /// 观察模式：只读取/记录，不派发付费动作。
    ObserveOnly,
}

impl BillingMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unpaid => "未授权付费（免费/离线模式）",
            Self::Authorized => "已显式授权付费额度",
            Self::ObserveOnly => "观察模式（不派发付费动作）",
        }
    }

    /// 是否允许发起新的付费派发；Unpaid/ObserveOnly 一律不允许。
    pub fn allows_paid_dispatch(self) -> bool {
        matches!(self, Self::Authorized)
    }
}

/// 默认预算档案：刻意是 Unpaid，不自动创建默认可消费额度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetProfile {
    pub mode: BillingMode,
    pub currency: Option<String>,
    pub limit: Option<i64>,
}

impl Default for BudgetProfile {
    fn default() -> Self {
        Self {
            mode: BillingMode::Unpaid,
            currency: None,
            limit: None,
        }
    }
}

impl BudgetProfile {
    /// 用户显式授权后才进入 Authorized；额度必须为正且带币种。
    pub fn authorize(currency: impl Into<String>, limit: i64) -> Result<Self, BudgetError> {
        if limit <= 0 {
            return Err(BudgetError::InvalidSettlement);
        }
        Ok(Self {
            mode: BillingMode::Authorized,
            currency: Some(currency.into()),
            limit: Some(limit),
        })
    }
}

/// 预算解释（§11.9）：分维度展示 C/R/U/可用与来源，拒绝时给出触发层级与下一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExplanation {
    pub dimension: Dimension,
    pub currency: Option<String>,
    pub limit: i64,
    pub confirmed: i64,
    pub reserved: i64,
    pub risk: i64,
    pub available: i64,
    pub source: String,
    /// 覆盖缺口：未配置的层级。
    pub coverage_gaps: Vec<BudgetScopeKind>,
}

pub fn explain_budget(
    scope_kind: BudgetScopeKind,
    dimension: Dimension,
    currency: Option<&str>,
    limit: Option<i64>,
    usage: BudgetUsage,
    source: impl Into<String>,
) -> BudgetExplanation {
    let effective_limit = limit.unwrap_or(0);
    BudgetExplanation {
        dimension,
        currency: currency.map(str::to_string),
        limit: effective_limit,
        confirmed: usage.confirmed,
        reserved: usage.reserved,
        risk: usage.risk,
        available: usage.available(effective_limit),
        source: source.into(),
        coverage_gaps: if limit.is_none() {
            vec![scope_kind]
        } else {
            Vec::new()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn company() -> DomainId {
        DomainId::new("company-default")
    }

    fn scopes(limit: i64, used: i64) -> Vec<ScopeBudget> {
        BudgetScopeKind::ADMISSION_ORDER
            .iter()
            .enumerate()
            .map(|(index, kind)| ScopeBudget {
                scope_kind: *kind,
                scope_id: DomainId::new(format!("scope-{index}")),
                limit: Some(limit),
                usage: BudgetUsage {
                    confirmed: 0,
                    reserved: used,
                    risk: 0,
                },
            })
            .collect()
    }

    mod admission {
        use super::*;

        #[test]
        fn i_05_checks_all_five_levels_in_fixed_order() {
            let admission =
                evaluate_admission(&scopes(1_000, 0), &Measure::money_micros(500, "CNY"))
                    .expect("五层都有额度应通过");
            assert_eq!(
                admission.approved_scopes,
                BudgetScopeKind::ADMISSION_ORDER.to_vec()
            );
        }

        #[test]
        fn i_05_any_level_shortage_rejects_with_triggering_scope() {
            let mut budgets = scopes(1_000, 0);
            // 只让 Agent 层不足。
            budgets[2].usage.reserved = 950;
            let err = evaluate_admission(&budgets, &Measure::money_micros(100, "CNY")).unwrap_err();
            assert_eq!(
                err,
                BudgetError::InsufficientBudget {
                    scope_kind: BudgetScopeKind::Agent,
                    dimension: Dimension::MoneyMicros,
                    requested: 100,
                    available: 50,
                }
            );
            assert_eq!(err.reason(), DenialReason::InsufficientBudget);
            assert!(err.next_step().contains("agent"));
        }

        #[test]
        fn missing_limit_means_no_implicit_paid_authorization() {
            let mut budgets = scopes(1_000, 0);
            budgets[0].limit = None;
            assert_eq!(
                evaluate_admission(&budgets, &Measure::money_micros(1, "CNY")).unwrap_err(),
                BudgetError::NoBudgetAuthorization {
                    scope_kind: BudgetScopeKind::Company
                }
            );
        }

        #[test]
        fn available_equals_limit_minus_c_r_u() {
            let usage = BudgetUsage {
                confirmed: 100,
                reserved: 200,
                risk: 300,
            };
            assert_eq!(usage.available(1_000), 400);
            assert_eq!(usage.available(500), -100, "超额不显示为可用");
        }
    }

    mod settlement_and_risk {
        use super::*;

        fn reservation(reserved: i64) -> Reservation {
            Reservation::new(
                "res-1",
                company(),
                Measure::money_micros(reserved, "CNY"),
                reserved,
                100,
            )
        }

        #[test]
        fn i_06_settlement_above_reserve_keeps_full_actual() {
            let settlement = evaluate_settlement(&reservation(500), 800).unwrap();
            assert_eq!(settlement.actual, 800, "不得截断 actual");
            assert_eq!(settlement.overage, 300);
            assert_eq!(settlement.released, 0);
            assert_eq!(settlement.next_state, ReservationState::Consumed);
        }

        #[test]
        fn settlement_below_reserve_releases_remainder() {
            let settlement = evaluate_settlement(&reservation(500), 300).unwrap();
            assert_eq!(settlement.released, 200);
            assert_eq!(settlement.overage, 0);
        }

        #[test]
        fn i_71_risk_transfer_moves_r_to_u_one_for_one() {
            let r = reservation(400);
            let transfer = evaluate_risk_transfer(&r).unwrap();
            assert_eq!(transfer.from_reserved, 400);
            assert_eq!(transfer.to_risk, 400);
            // 总量守恒：R+U 不变，且不重复计入。
            let before = r.reserved + r.risk;
            let after = (r.reserved - transfer.from_reserved) + (r.risk + transfer.to_risk);
            assert_eq!(before, after);
        }

        #[test]
        fn consumed_reservation_cannot_transfer_again() {
            let mut r = reservation(400);
            r.state = ReservationState::Consumed;
            assert!(matches!(
                evaluate_risk_transfer(&r).unwrap_err(),
                BudgetError::ReservationNotActive { .. }
            ));
        }
    }

    mod measures {
        use super::*;

        #[test]
        fn money_requires_currency_and_tokens_must_not_carry_one() {
            assert_eq!(
                Measure {
                    dimension: Dimension::MoneyMicros,
                    value: 1,
                    currency: None,
                }
                .validate()
                .unwrap_err(),
                BudgetError::MissingCurrency
            );
            assert_eq!(
                Measure {
                    dimension: Dimension::Tokens,
                    value: 1,
                    currency: Some("CNY".to_string()),
                }
                .validate()
                .unwrap_err(),
                BudgetError::UnexpectedCurrency
            );
        }

        #[test]
        fn token_amounts_from_different_models_are_not_summed_as_money() {
            // 两个 token 计量没有币种，不能参与现金求和；explain 也分维度输出。
            let a = Measure::tokens(1_000);
            let b = Measure::tokens(2_000);
            assert!(a.currency.is_none() && b.currency.is_none());
            assert_ne!(a.dimension, Dimension::MoneyMicros);
            let explained = explain_budget(
                BudgetScopeKind::Project,
                Dimension::Tokens,
                None,
                Some(10_000),
                BudgetUsage::default(),
                "provider:usage_reports",
            );
            assert_eq!(explained.currency, None);
            assert_eq!(explained.available, 10_000);
        }

        #[test]
        fn explanation_shows_gap_and_never_hides_unknown_risk() {
            let explained = explain_budget(
                BudgetScopeKind::Project,
                Dimension::MoneyMicros,
                Some("CNY"),
                Some(1_000),
                BudgetUsage {
                    confirmed: 300,
                    reserved: 200,
                    risk: 100,
                },
                "ledger_entries",
            );
            assert_eq!(explained.available, 400);
            assert_eq!(explained.risk, 100, "未知占用单列，不从可用额中隐去");
            assert!(explained.coverage_gaps.is_empty());

            let missing = explain_budget(
                BudgetScopeKind::Project,
                Dimension::MoneyMicros,
                Some("CNY"),
                None,
                BudgetUsage::default(),
                "none",
            );
            assert_eq!(missing.coverage_gaps, vec![BudgetScopeKind::Project]);
        }
    }

    mod billing {
        use super::*;

        #[test]
        fn default_profile_grants_no_paid_quota() {
            let profile = BudgetProfile::default();
            assert_eq!(profile.mode, BillingMode::Unpaid);
            assert!(!profile.mode.allows_paid_dispatch());
            assert!(profile.limit.is_none());
            assert!(profile.mode.label().contains("未授权付费"));
        }

        #[test]
        fn explicit_authorization_enables_paid_dispatch() {
            let profile = BudgetProfile::authorize("CNY", 500_000_000).unwrap();
            assert_eq!(profile.mode, BillingMode::Authorized);
            assert!(profile.mode.allows_paid_dispatch());
            assert_eq!(profile.limit, Some(500_000_000));
        }

        #[test]
        fn observe_mode_is_labelled_and_never_dispatches_paid_actions() {
            let mode = BillingMode::ObserveOnly;
            assert!(!mode.allows_paid_dispatch());
            assert!(mode.label().contains("观察模式"));
        }
    }
}
