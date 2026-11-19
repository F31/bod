//! P0 D6 删除请求、清理阻塞、Abandon 与外部共享资源边界（任务 7.3）。
//!
//! 规格：`recovery-cleanup` 删除请求与清理阻塞 / 外部共享资源边界；
//! `resource-state` 删除请求与清理边界；V2.9.6 §8.5、§21.4、§33.9。
//!
//! 三条硬约束：
//! 1. 先标记 `deletionRequestedAt` 并停止新派发，再逐项判定四项条件；
//!    任一项不明显示 `CleanupBlocked`，**不得**静默清空记录。
//! 2. 外部仓库、生产环境、外部共享资源与正式 Manifest 引用的成果**永不**被自动销毁；
//!    OwnerReference 只对宿主管理的依赖资源做受控级联。
//! 3. 清理经过 Effect Gate：意图在写事务内成为 outbox 记录（Prepared），
//!    完成时改 Confirmed；同一 `request_id` 重复调用是幂等的。
//!
//! 证据边界：本模块只判定与登记，不删除 CAS 对象。未固定草稿的物理回收仍由
//! `cas::gc`（保留窗口 + 外部引用排除）执行，避免把"记录清理"与"对象回收"混为一谈。

use crate::domain::DomainId;
use crate::domain::budget::ReservationState;
use crate::domain::execution::ExecutionState;
use crate::domain::queue::{EffectStatus, OutboxDelivery};
use crate::effect_recovery::{self, PreparedIntent};
use crate::recovery_models::{
    CleanupAbandon, CleanupRequest, CleanupState, CleanupTargetKind, RevisionPin,
};
use crate::store::{SqliteStore, StoreError};
use std::collections::HashSet;

/// outbox 目的地：清理动作的 Effect Gate 通道。
pub const CLEANUP_DESTINATION: &str = "cleanup";

/// 阻塞码：与文档、CLI 输出一致，调用方不解析人类消息。
pub const BLOCK_SCOPE_UNKNOWN: &str = "scope_unknown";
pub const BLOCK_STOP_UNCONFIRMED: &str = "stop_unconfirmed";
pub const BLOCK_BUDGET_UNSETTLED: &str = "budget_unsettled";
pub const BLOCK_ARTIFACT_UNPINNED: &str = "artifact_unpinned";
pub const BLOCK_TEMP_ENV_UNKNOWN: &str = "temp_env_unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionState {
    Satisfied,
    /// 已知未完成：有在途、未结算或未授权项。
    Pending,
    /// 无法判定：缺事实来源，按阻塞处理。
    Unknown,
}

impl ConditionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::Pending => "pending",
            Self::Unknown => "unknown",
        }
    }

    pub fn blocks(self) -> bool {
        !matches!(self, Self::Satisfied)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupCondition {
    pub name: &'static str,
    pub state: ConditionState,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupBlocker {
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupVerdict {
    pub conditions: Vec<CleanupCondition>,
    pub blockers: Vec<CleanupBlocker>,
}

impl CleanupVerdict {
    pub fn is_ready(&self) -> bool {
        self.blockers.is_empty()
    }

    pub fn blocked_codes(&self) -> Vec<&'static str> {
        self.blockers.iter().map(|item| item.code).collect()
    }

    pub fn condition(&self, name: &str) -> Option<&CleanupCondition> {
        self.conditions.iter().find(|item| item.name == name)
    }
}

/// 执行停止事实：`Cancelling` 表示取消已请求但未确认，仍算未停止。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StopFacts {
    pub in_flight: usize,
    pub cancelling: usize,
    pub unknown: usize,
    /// 无法归属到项目的执行：事实不明，按 Unknown 处理（不能当"无在途"）。
    pub unattributed: usize,
    pub running_issues: i64,
}

impl StopFacts {
    pub fn from_states(states: &[String], running_issues: i64, unattributed: usize) -> Self {
        let mut facts = Self {
            running_issues,
            unattributed,
            ..Self::default()
        };
        for raw in states {
            match ExecutionState::parse(raw) {
                Some(ExecutionState::Cancelling) => facts.cancelling += 1,
                Some(ExecutionState::Unknown) => facts.unknown += 1,
                Some(state) if !state.is_terminal() => facts.in_flight += 1,
                _ => {}
            }
        }
        facts
    }

    fn stopped(&self) -> bool {
        self.in_flight == 0
            && self.cancelling == 0
            && self.unknown == 0
            && self.unattributed == 0
            && self.running_issues == 0
    }

    /// 有任何未确认停止即视为可能仍在外部产生副作用。
    pub fn blocks_stop(&self) -> bool {
        !self.stopped()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetFacts {
    /// 仍占 R 的预留：费用未结算。
    pub active_reservations: usize,
    /// 占用 U 但没有 open 对账案件：结果未知，不能当已结算。
    pub risk_without_case: usize,
    /// 占用 U 且已移交对账：允许清理，占用保留在案件中。
    pub risk_with_case: usize,
}

impl BudgetFacts {
    fn settled(&self) -> bool {
        self.active_reservations == 0 && self.risk_without_case == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArtifactFacts {
    /// 已按策略固定：接受指针、正式 Manifest 引用或外部权威存储。
    pub preserved: usize,
    /// 未固定的宿主管理草稿：策略未授权时阻塞。
    pub cascade_candidates: usize,
    pub external_refs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupFacts {
    /// 目标能否解析到项目范围；解析失败时其余事实不可信。
    pub scope_known: bool,
    pub stop: StopFacts,
    pub budget: BudgetFacts,
    pub artifacts: ArtifactFacts,
    pub temp_envs: ConditionState,
    pub temp_env_detail: String,
    /// 项目内修订的固定线索，供级联计划与外部引用清单一并输出。
    pub pins: Vec<RevisionPin>,
}

impl CleanupFacts {
    /// 无法解析目标范围时的保守事实：每项都是 Unknown。
    pub fn unknown_scope(detail: &str) -> Self {
        Self {
            scope_known: false,
            stop: StopFacts::default(),
            budget: BudgetFacts::default(),
            artifacts: ArtifactFacts::default(),
            temp_envs: ConditionState::Unknown,
            temp_env_detail: detail.to_string(),
            pins: Vec::new(),
        }
    }
}

/// 四项条件逐项判定：停止已确认 → 费用已结算或移交对账 → 产物已按策略固定 → 临时环境已处置。
pub fn evaluate_cleanup(facts: &CleanupFacts, policy: &CleanupPolicy) -> CleanupVerdict {
    let mut conditions = Vec::with_capacity(4);
    let mut blockers = Vec::new();

    if !facts.scope_known {
        conditions.push(CleanupCondition {
            name: "scope",
            state: ConditionState::Unknown,
            detail: "无法解析清理目标所属项目，停止与费用事实不可判定".to_string(),
        });
        blockers.push(CleanupBlocker {
            code: BLOCK_SCOPE_UNKNOWN,
            detail: "目标不存在或缺少项目归属，先修复引用再请求删除".to_string(),
        });
    }

    let (stop_state, stop_detail) = if facts.stop.stopped() {
        (
            ConditionState::Satisfied,
            "无在途执行、无未确认取消、无运行中 Issue".to_string(),
        )
    } else if facts.stop.unknown > 0 || facts.stop.unattributed > 0 {
        (
            ConditionState::Unknown,
            format!(
                "{} 个执行处于 Unknown，{} 个执行无法归属到项目，取消是否生效未知",
                facts.stop.unknown, facts.stop.unattributed
            ),
        )
    } else {
        (
            ConditionState::Pending,
            format!(
                "在途 {} / 取消未确认 {} / 运行中 Issue {}",
                facts.stop.in_flight, facts.stop.cancelling, facts.stop.running_issues
            ),
        )
    };
    conditions.push(CleanupCondition {
        name: "stop_confirmed",
        state: stop_state,
        detail: stop_detail.clone(),
    });
    if stop_state.blocks() {
        blockers.push(CleanupBlocker {
            code: BLOCK_STOP_UNCONFIRMED,
            detail: stop_detail,
        });
    }

    let (budget_state, budget_detail) = if facts.budget.settled() {
        (
            ConditionState::Satisfied,
            if facts.budget.risk_with_case > 0 {
                format!(
                    "费用已结算；{} 笔风险占用保留在对账案件中",
                    facts.budget.risk_with_case
                )
            } else {
                "无未结算预留".to_string()
            },
        )
    } else if facts.budget.risk_without_case > 0 {
        (
            ConditionState::Unknown,
            format!(
                "{} 笔风险占用没有对账案件，实际费用未知",
                facts.budget.risk_without_case
            ),
        )
    } else {
        (
            ConditionState::Pending,
            format!("{} 笔预留仍在占用 R", facts.budget.active_reservations),
        )
    };
    conditions.push(CleanupCondition {
        name: "budget_settled",
        state: budget_state,
        detail: budget_detail.clone(),
    });
    if budget_state.blocks() {
        blockers.push(CleanupBlocker {
            code: BLOCK_BUDGET_UNSETTLED,
            detail: budget_detail,
        });
    }

    let (artifact_state, artifact_detail) = if facts.artifacts.cascade_candidates == 0 {
        (
            ConditionState::Satisfied,
            format!(
                "{} 个修订已固定（接受/Manifest/外部），{} 个外部引用保留",
                facts.artifacts.preserved, facts.artifacts.external_refs
            ),
        )
    } else if policy.cascade_authorized {
        (
            ConditionState::Satisfied,
            format!(
                "{} 个未固定草稿已获授权随项目清理；{} 个修订固定保留",
                facts.artifacts.cascade_candidates, facts.artifacts.preserved
            ),
        )
    } else {
        (
            ConditionState::Pending,
            format!(
                "{} 个未固定草稿：需保留或显式授权删除",
                facts.artifacts.cascade_candidates
            ),
        )
    };
    conditions.push(CleanupCondition {
        name: "artifacts_pinned",
        state: artifact_state,
        detail: artifact_detail.clone(),
    });
    if artifact_state.blocks() {
        blockers.push(CleanupBlocker {
            code: BLOCK_ARTIFACT_UNPINNED,
            detail: artifact_detail,
        });
    }

    conditions.push(CleanupCondition {
        name: "temp_env_disposed",
        state: facts.temp_envs,
        detail: facts.temp_env_detail.clone(),
    });
    if facts.temp_envs.blocks() {
        blockers.push(CleanupBlocker {
            code: BLOCK_TEMP_ENV_UNKNOWN,
            detail: facts.temp_env_detail.clone(),
        });
    }

    CleanupVerdict {
        conditions,
        blockers,
    }
}

/// 调用方对本次清理的策略声明。默认值即最保守：临时环境不明、未授权删除草稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPolicy {
    pub temp_envs: ConditionState,
    pub temp_env_detail: String,
    pub cascade_authorized: bool,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            temp_envs: ConditionState::Unknown,
            temp_env_detail: "宿主未提供临时环境清单，按未处置处理".to_string(),
            cascade_authorized: false,
        }
    }
}

impl CleanupPolicy {
    pub fn disposed(detail: &str) -> Self {
        Self {
            temp_envs: ConditionState::Satisfied,
            temp_env_detail: detail.to_string(),
            cascade_authorized: false,
        }
    }

    pub fn authorizing_cascade(mut self) -> Self {
        self.cascade_authorized = true;
        self
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalReason {
    /// 用户 Git 仓库（repo/commit/path）。
    Repository,
    /// 生产环境（部署目标）。
    ProductionEnvironment,
    /// 外部共享资源（他人所有或跨项目共用）。
    SharedResource,
    /// 被正式 Manifest 引用的交付成果。
    FormalManifest,
    /// 当前接受指针：已交付，不随项目删除消失。
    AcceptedDelivery,
}

impl ExternalReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "external_repository",
            Self::ProductionEnvironment => "production_environment",
            Self::SharedResource => "shared_resource",
            Self::FormalManifest => "formal_manifest",
            Self::AcceptedDelivery => "accepted_delivery",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Repository => "外部代码仓库",
            Self::ProductionEnvironment => "生产环境",
            Self::SharedResource => "外部共享资源",
            Self::FormalManifest => "正式 Manifest 引用",
            Self::AcceptedDelivery => "已接受交付",
        }
    }

    /// 外部存储 locator 的显式前缀声明；未声明的一律按仓库处理（最保守）。
    pub fn classify_locator(locator: &str) -> Self {
        if locator.starts_with("env:") || locator.starts_with("prod:") {
            Self::ProductionEnvironment
        } else if locator.starts_with("shared:") {
            Self::SharedResource
        } else {
            Self::Repository
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalReference {
    pub revision_id: String,
    pub reference: String,
    pub reason: ExternalReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CascadePlan {
    /// 宿主管理且未被固定的修订：可随项目清理。
    pub managed: Vec<String>,
    /// 必须保留的引用：外部仓库、生产环境、共享资源、Manifest、已接受交付。
    pub preserved: Vec<ExternalReference>,
    /// 需要人工跟进的说明：清理不会替用户解除这些引用。
    pub manual_followups: Vec<String>,
}

/// 受控级联：只把"宿主管理且未被固定"的修订列为可清理，其余一律保留并说明原因。
pub fn plan_cascade(pins: &[RevisionPin]) -> CascadePlan {
    let mut plan = CascadePlan::default();
    for pin in pins {
        if pin.is_external() {
            let reason = ExternalReason::classify_locator(pin.storage.as_str());
            plan.preserved.push(ExternalReference {
                revision_id: pin.revision_id.clone(),
                reference: pin.storage.as_str().to_string(),
                reason,
            });
            plan.manual_followups.push(format!(
                "修订 {} 位于{}（{}），宿主只清理自身记录，请人工确认后续归属",
                pin.revision_id,
                reason.label(),
                pin.storage.as_str()
            ));
            continue;
        }
        if pin.accepted {
            plan.preserved.push(ExternalReference {
                revision_id: pin.revision_id.clone(),
                reference: pin.storage.as_str().to_string(),
                reason: ExternalReason::AcceptedDelivery,
            });
            continue;
        }
        if !pin.manifest_refs.is_empty() {
            plan.preserved.push(ExternalReference {
                revision_id: pin.revision_id.clone(),
                reference: pin.storage.as_str().to_string(),
                reason: ExternalReason::FormalManifest,
            });
            plan.manual_followups.push(format!(
                "修订 {} 被正式 Manifest {} 引用，需先解除交付关系再清理",
                pin.revision_id,
                pin.manifest_refs.join("、")
            ));
            continue;
        }
        plan.managed.push(pin.revision_id.clone());
    }
    plan
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupScope {
    pub company_id: String,
    pub kind: CleanupTargetKind,
    pub target_id: String,
}

impl CleanupScope {
    pub fn new(company_id: &str, kind: CleanupTargetKind, target_id: &str) -> Self {
        Self {
            company_id: company_id.to_string(),
            kind,
            target_id: target_id.to_string(),
        }
    }

    pub fn project(company_id: &str, project_id: &str) -> Self {
        Self::new(company_id, CleanupTargetKind::Project, project_id)
    }

    pub fn describe(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.target_id)
    }
}

#[derive(Debug)]
pub enum CleanupError {
    Store(StoreError),
    Recovery(effect_recovery::RecoveryError),
    /// 条件未满足：不得完成清理。
    Blocked {
        blockers: Vec<CleanupBlocker>,
    },
    NotRequested,
    AlreadyAbandoned,
    MissingRequester,
    MissingOwner,
    MissingReconciliationEntry,
}

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::Recovery(err) => write!(f, "{err:?}"),
            Self::Blocked { blockers } => {
                let codes: Vec<&str> = blockers.iter().map(|item| item.code).collect();
                write!(f, "CleanupBlocked: {}", codes.join(","))
            }
            Self::NotRequested => write!(f, "该目标没有清理请求，先登记删除请求"),
            Self::AlreadyAbandoned => write!(f, "该清理请求已 Abandon，不能标记为完成"),
            Self::MissingRequester => write!(f, "清理请求必须记录发起人"),
            Self::MissingOwner => write!(f, "Abandon 必须记录责任人"),
            Self::MissingReconciliationEntry => write!(f, "Abandon 必须记录后续对账入口"),
        }
    }
}

impl std::error::Error for CleanupError {}

impl From<StoreError> for CleanupError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<effect_recovery::RecoveryError> for CleanupError {
    fn from(value: effect_recovery::RecoveryError) -> Self {
        Self::Recovery(value)
    }
}

/// 目标所属项目：Project 直接成立，Issue/Artifact 反查，Run 不按项目聚合。
fn resolve_project(
    store: &SqliteStore,
    scope: &CleanupScope,
) -> Result<Option<String>, CleanupError> {
    match scope.kind {
        CleanupTargetKind::Project => Ok(Some(scope.target_id.clone())),
        CleanupTargetKind::Issue => {
            Ok(store.issue_project_id(&scope.company_id, &scope.target_id)?)
        }
        CleanupTargetKind::Artifact => {
            Ok(store.artifact_project_id(&scope.company_id, &scope.target_id)?)
        }
        CleanupTargetKind::Run => Ok(None),
    }
}

fn scope_run_ids(
    store: &SqliteStore,
    scope: &CleanupScope,
    project: Option<&str>,
) -> Result<Vec<String>, CleanupError> {
    match scope.kind {
        CleanupTargetKind::Run => Ok(vec![scope.target_id.clone()]),
        _ => match project {
            Some(project_id) => Ok(store.list_project_run_ids(&scope.company_id, project_id)?),
            None => Ok(Vec::new()),
        },
    }
}

/// 收集四项条件的证据。只读：不写库、不改派发、不动外部系统。
pub fn collect_facts(
    store: &SqliteStore,
    scope: &CleanupScope,
    policy: &CleanupPolicy,
) -> Result<CleanupFacts, CleanupError> {
    let project = resolve_project(store, scope)?;
    let scope_known = match scope.kind {
        // Run 级清理不按项目聚合，但必须确认 Run 存在，否则停止事实无从判断。
        CleanupTargetKind::Run => store.run_exists(&scope.company_id, &scope.target_id)?,
        _ => project.is_some(),
    };
    if !scope_known {
        return Ok(CleanupFacts {
            temp_envs: policy.temp_envs,
            temp_env_detail: policy.temp_env_detail.clone(),
            ..CleanupFacts::unknown_scope("无法解析清理目标所属项目")
        });
    }

    let (states, running_issues, unattributed) = match scope.kind {
        CleanupTargetKind::Run => (
            store.list_run_invocation_states(&scope.company_id, &scope.target_id)?,
            0,
            0,
        ),
        _ => {
            let project_id = project.as_deref().unwrap_or_default();
            (
                store.list_project_invocation_states(&scope.company_id, project_id)?,
                store.count_running_issues(&scope.company_id, project_id)?,
                store.count_unattributed_invocations(&scope.company_id)?,
            )
        }
    };
    let stop = StopFacts::from_states(&states, running_issues, unattributed as usize);

    let run_ids: HashSet<String> = scope_run_ids(store, scope, project.as_deref())?
        .into_iter()
        .collect();
    let open_cases = store.list_reconciliation_cases(&scope.company_id, true)?;
    let covered: HashSet<String> = open_cases
        .iter()
        .flat_map(|case| case.risk_reservation_refs.iter().cloned())
        .collect();

    let mut budget = BudgetFacts::default();
    for reservation in store.list_reservations(&scope.company_id)? {
        let in_scope = match scope.kind {
            CleanupTargetKind::Run => {
                reservation.run_id.as_ref().map(|id| id.as_str()) == Some(scope.target_id.as_str())
            }
            _ => {
                reservation.project_id.as_ref().map(|id| id.as_str())
                    == Some(project.as_deref().unwrap_or_default())
                    || reservation
                        .run_id
                        .as_ref()
                        .map(|id| run_ids.contains(id.as_str()))
                        .unwrap_or(false)
            }
        };
        if !in_scope {
            continue;
        }
        match reservation.state {
            ReservationState::Active => budget.active_reservations += 1,
            ReservationState::Risk => {
                if covered.contains(&reservation.reservation_id) {
                    budget.risk_with_case += 1;
                } else {
                    budget.risk_without_case += 1;
                }
            }
            ReservationState::Consumed | ReservationState::Cancelled => {}
        }
    }

    let pins = match (scope.kind, project.as_deref()) {
        (CleanupTargetKind::Run, _) => Vec::new(),
        (_, Some(project_id)) => store.list_project_revision_pins(&scope.company_id, project_id)?,
        (_, None) => Vec::new(),
    };
    let plan = plan_cascade(&pins);
    let artifacts = ArtifactFacts {
        preserved: plan.preserved.len(),
        cascade_candidates: plan.managed.len(),
        external_refs: plan
            .preserved
            .iter()
            .filter(|item| {
                matches!(
                    item.reason,
                    ExternalReason::Repository
                        | ExternalReason::ProductionEnvironment
                        | ExternalReason::SharedResource
                )
            })
            .count(),
    };

    Ok(CleanupFacts {
        scope_known: true,
        stop,
        budget,
        artifacts,
        temp_envs: policy.temp_envs,
        temp_env_detail: policy.temp_env_detail.clone(),
        pins,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupOutcome {
    pub request: CleanupRequest,
    pub verdict: CleanupVerdict,
    pub cascade: CascadePlan,
    pub effect: Option<PreparedIntent>,
    /// true 表示本次真的登记了删除请求；false 表示幂等命中已有记录。
    pub created: bool,
}

fn cleanup_delivery(scope: &CleanupScope, request_id: &str, now_ms: i64) -> OutboxDelivery {
    OutboxDelivery {
        event_or_command_id: request_id.to_string(),
        destination_id: CLEANUP_DESTINATION.to_string(),
        company_id: DomainId::new(&scope.company_id),
        queue_key: format!("cleanup:{}", scope.describe()),
        request_digest: format!("{}|{}", scope.describe(), request_id),
        status: EffectStatus::Prepared,
        attempts: 0,
        next_due_at_ms: now_ms,
        result_ref: None,
        created_at_ms: now_ms,
    }
}

/// 登记删除请求：写事务内标记 `deletionRequestedAt` + 建清理记录 + 落 Effect Gate 意图，
/// 提交后才判定四项条件（判定读的是提交后的事实）。
pub fn request_cleanup(
    store: &mut SqliteStore,
    scope: &CleanupScope,
    requested_by: &str,
    policy: &CleanupPolicy,
    request_id: &str,
    now_ms: i64,
) -> Result<CleanupOutcome, CleanupError> {
    if requested_by.trim().is_empty() {
        return Err(CleanupError::MissingRequester);
    }
    if let Some(existing) =
        store.get_cleanup_request(&scope.company_id, scope.kind, &scope.target_id)?
    {
        // 幂等：同一目标不新建第二条记录，也不重复落 Effect Gate 意图。
        let facts = collect_facts(store, scope, policy)?;
        return Ok(CleanupOutcome {
            verdict: evaluate_cleanup(&facts, policy),
            cascade: plan_cascade(&facts.pins),
            request: existing,
            effect: None,
            created: false,
        });
    }

    let request = CleanupRequest {
        request_id: request_id.to_string(),
        company_id: DomainId::new(&scope.company_id),
        target_kind: scope.kind,
        target_id: scope.target_id.clone(),
        state: CleanupState::Requested,
        blockers: Vec::new(),
        requested_by: requested_by.to_string(),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    let delivery = cleanup_delivery(scope, request_id, now_ms);
    let effect = {
        let uow = store.transaction()?;
        match scope.kind {
            CleanupTargetKind::Project => {
                uow.mark_project_deletion_requested(&scope.company_id, &scope.target_id, now_ms)?;
            }
            CleanupTargetKind::Issue => {
                uow.mark_issue_deletion_requested(&scope.company_id, &scope.target_id, now_ms)?;
            }
            CleanupTargetKind::Run | CleanupTargetKind::Artifact => {}
        }
        uow.put_cleanup_request(&request)?;
        let prepared = effect_recovery::prepare_in(&uow, &delivery)?;
        uow.commit()?;
        prepared
    };

    let facts = collect_facts(store, scope, policy)?;
    let verdict = evaluate_cleanup(&facts, policy);
    let cascade = plan_cascade(&facts.pins);
    if !verdict.is_ready() {
        let uow = store.transaction()?;
        uow.update_cleanup_state(
            request_id,
            CleanupState::Blocked,
            &verdict
                .blockers
                .iter()
                .map(|item| item.code.to_string())
                .collect::<Vec<_>>(),
            now_ms,
        )?;
        uow.commit()?;
    }

    let request = store
        .get_cleanup_request(&scope.company_id, scope.kind, &scope.target_id)?
        .unwrap_or(request);
    Ok(CleanupOutcome {
        request,
        verdict,
        cascade,
        effect: Some(effect),
        created: true,
    })
}

/// 重新判定并写回状态（条件可能随对账/结算变化）。
pub fn refresh_cleanup(
    store: &mut SqliteStore,
    scope: &CleanupScope,
    policy: &CleanupPolicy,
    now_ms: i64,
) -> Result<CleanupOutcome, CleanupError> {
    let request = store
        .get_cleanup_request(&scope.company_id, scope.kind, &scope.target_id)?
        .ok_or(CleanupError::NotRequested)?;
    let facts = collect_facts(store, scope, policy)?;
    let verdict = evaluate_cleanup(&facts, policy);
    let cascade = plan_cascade(&facts.pins);
    if matches!(
        request.state,
        CleanupState::Completed | CleanupState::Abandoned
    ) {
        return Ok(CleanupOutcome {
            request,
            verdict,
            cascade,
            effect: None,
            created: false,
        });
    }
    let next_state = if verdict.is_ready() {
        CleanupState::Requested
    } else {
        CleanupState::Blocked
    };
    let uow = store.transaction()?;
    uow.update_cleanup_state(
        &request.request_id,
        next_state,
        &verdict
            .blockers
            .iter()
            .map(|item| item.code.to_string())
            .collect::<Vec<_>>(),
        now_ms,
    )?;
    uow.commit()?;
    let request = store
        .get_cleanup_request(&scope.company_id, scope.kind, &scope.target_id)?
        .ok_or(CleanupError::NotRequested)?;
    Ok(CleanupOutcome {
        request,
        verdict,
        cascade,
        effect: None,
        created: false,
    })
}

/// 完成清理：四项条件必须全部满足，且必须经过 Effect Gate。
/// 重复调用返回幂等结果，不产生第二次删除。
pub fn complete_cleanup(
    store: &mut SqliteStore,
    scope: &CleanupScope,
    policy: &CleanupPolicy,
    now_ms: i64,
) -> Result<CleanupOutcome, CleanupError> {
    let request = store
        .get_cleanup_request(&scope.company_id, scope.kind, &scope.target_id)?
        .ok_or(CleanupError::NotRequested)?;
    let facts = collect_facts(store, scope, policy)?;
    let verdict = evaluate_cleanup(&facts, policy);
    let cascade = plan_cascade(&facts.pins);

    if request.state == CleanupState::Abandoned {
        return Err(CleanupError::AlreadyAbandoned);
    }
    if request.state == CleanupState::Completed {
        return Ok(CleanupOutcome {
            request,
            verdict,
            cascade,
            effect: None,
            created: false,
        });
    }
    if !verdict.is_ready() {
        let uow = store.transaction()?;
        uow.update_cleanup_state(
            &request.request_id,
            CleanupState::Blocked,
            &verdict
                .blockers
                .iter()
                .map(|item| item.code.to_string())
                .collect::<Vec<_>>(),
            now_ms,
        )?;
        uow.commit()?;
        return Err(CleanupError::Blocked {
            blockers: verdict.blockers.clone(),
        });
    }

    let delivery = cleanup_delivery(scope, &request.request_id, now_ms);
    let effect = {
        let uow = store.transaction()?;
        // 同一逻辑键的意图必须已存在（请求阶段写入）；缺失时按同摘要补齐，仍是幂等语义。
        let prepared = effect_recovery::prepare_in(&uow, &delivery)?;
        uow.update_cleanup_state(&request.request_id, CleanupState::Completed, &[], now_ms)?;
        uow.set_outbox_status(
            &delivery.event_or_command_id,
            &delivery.destination_id,
            EffectStatus::Confirmed,
            prepared.delivery.attempts,
            now_ms,
            Some(&request.request_id),
        )?;
        uow.commit()?;
        prepared
    };

    let request = store
        .get_cleanup_request(&scope.company_id, scope.kind, &scope.target_id)?
        .ok_or(CleanupError::NotRequested)?;
    Ok(CleanupOutcome {
        request,
        verdict,
        cascade,
        effect: Some(effect),
        created: false,
    })
}

#[derive(Debug, Clone)]
pub struct AbandonInput<'a> {
    pub owner: &'a str,
    pub reconciliation_entry: &'a str,
    pub known_evidence: &'a str,
    pub abandon_id: &'a str,
}

/// Abandon：记录尚存外部资源、风险、责任人与后续对账入口。
/// 不标记删除成功，不删除任何记录或对象。
pub fn abandon_cleanup(
    store: &mut SqliteStore,
    scope: &CleanupScope,
    policy: &CleanupPolicy,
    input: &AbandonInput<'_>,
    now_ms: i64,
) -> Result<CleanupAbandon, CleanupError> {
    if input.owner.trim().is_empty() {
        return Err(CleanupError::MissingOwner);
    }
    if input.reconciliation_entry.trim().is_empty() {
        return Err(CleanupError::MissingReconciliationEntry);
    }
    let request = store
        .get_cleanup_request(&scope.company_id, scope.kind, &scope.target_id)?
        .ok_or(CleanupError::NotRequested)?;
    if request.state == CleanupState::Abandoned {
        return Err(CleanupError::AlreadyAbandoned);
    }

    let facts = collect_facts(store, scope, policy)?;
    let plan = plan_cascade(&facts.pins);
    let mut residual: Vec<String> = plan
        .preserved
        .iter()
        .map(|item| {
            format!(
                "{}|{}|{}",
                item.reason.as_str(),
                item.revision_id,
                item.reference
            )
        })
        .collect();
    residual.extend(
        plan.manual_followups
            .iter()
            .map(|item| format!("followup|{item}")),
    );
    if facts.budget.risk_without_case > 0 {
        residual.push(format!(
            "risk_reservation|{}|未建对账案件",
            facts.budget.risk_without_case
        ));
    }
    if facts.temp_envs.blocks() {
        residual.push(format!(
            "temp_env|{}|{}",
            facts.temp_envs.as_str(),
            facts.temp_env_detail
        ));
    }

    let mut risks = Vec::new();
    if !plan.preserved.is_empty() {
        risks.push(format!(
            "{} 个交付引用仍指向外部权威存储，删除项目记录后需人工确认归属",
            plan.preserved.len()
        ));
    }
    if facts.stop.blocks_stop() {
        risks.push("存在未确认停止的执行，可能在外部继续产生副作用".to_string());
    }
    if facts.budget.risk_without_case > 0 {
        risks.push("风险占用未建对账案件，费用敞口未知".to_string());
    }
    if facts.temp_envs.blocks() {
        risks.push("临时环境未确认处置，可能占用宿主资源".to_string());
    }
    if risks.is_empty() {
        risks.push("无残留外部资源；本记录仅为审计留痕".to_string());
    }

    let abandon = CleanupAbandon {
        abandon_id: input.abandon_id.to_string(),
        company_id: DomainId::new(&scope.company_id),
        request_id: request.request_id.clone(),
        residual_resources: residual,
        risks,
        owner: input.owner.to_string(),
        reconciliation_entry: input.reconciliation_entry.to_string(),
        created_at_ms: now_ms,
    };

    let uow = store.transaction()?;
    uow.put_cleanup_abandon(&abandon)?;
    let mut blockers = request
        .blockers
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>();
    blockers.push(format!("abandoned:{}", abandon.abandon_id));
    uow.update_cleanup_state(
        &request.request_id,
        CleanupState::Abandoned,
        &blockers,
        now_ms,
    )?;
    uow.commit()?;
    Ok(abandon)
}

/// 当前处于 requested/blocked 的清理请求（供 CLI 与 doctor 展示未决项）。
pub fn blocking_requests(
    store: &SqliteStore,
    company_id: &str,
) -> Result<Vec<CleanupRequest>, CleanupError> {
    store
        .list_open_cleanup_requests(company_id)
        .map_err(CleanupError::Store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_acceptance;
    use crate::cas::CasStore;
    use crate::config::BodConfig;
    use crate::domain::Version;
    use crate::domain::artifact::{Artifact, ArtifactKind, ArtifactVisibility, Digest};
    use crate::domain::budget::{Measure, Reservation};
    use crate::domain::execution::InvocationRecord;
    use crate::domain::model::{
        AcceptanceCriteria, Issue, IssueKind, IssueSpec, Phase, ResourceMeta, ResourceStatus,
    };
    use crate::lifecycle_recovery::{CaseRequest, open_case};
    use std::path::PathBuf;

    const NOW: i64 = 1_763_500_000_000;
    const COMPANY: &str = "company-default";
    const PROJECT: &str = "project-1";

    struct Fixture {
        store: SqliteStore,
        cas: CasStore,
        _root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let seq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            let root =
                std::env::temp_dir().join(format!("bod-cleanup-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let store = SqliteStore::open(&root.join("bod.sqlite"), &BodConfig::default()).unwrap();
            let cas = CasStore::open(root.join("cas")).unwrap();
            Self {
                store,
                cas,
                _root: root,
            }
        }

        fn with_issue(&mut self) -> &mut Self {
            let issue = Issue {
                meta: ResourceMeta {
                    uid: DomainId::new("issue-1"),
                    company_id: DomainId::new(COMPANY),
                    project_id: Some(DomainId::new(PROJECT)),
                    resource_version: Version::INITIAL,
                    generation: 1,
                    deletion_requested_at: None,
                },
                kind: IssueKind::General,
                repository: None,
                spec: IssueSpec {
                    title: "整理发布说明".to_string(),
                    assignment: None,
                    acceptance: AcceptanceCriteria {
                        criteria: vec!["评审通过".to_string()],
                    },
                },
                status: ResourceStatus {
                    phase: Phase::Pending,
                    observed_generation: 1,
                    conditions: Vec::new(),
                },
            };
            let uow = self.store.transaction().unwrap();
            uow.insert_issue(&issue).unwrap();
            uow.commit().unwrap();
            self
        }

        /// P0 切片还没有 runs 写入者；测试用最小行建立"执行归属项目"的链路。
        fn run_row(&mut self, run_id: &str) {
            self.store
                .execute_batch_for_test(&format!(
                    "INSERT INTO runs (uid, company_id, issue_uid, plan_revision, desired_state)
                     VALUES ('{run_id}', '{COMPANY}', 'issue-1', 'plan-1', 'Run')"
                ))
                .unwrap();
        }

        fn invocation(&mut self, run_id: &str, state: ExecutionState) {
            let record = InvocationRecord {
                invocation_id: format!("inv-{run_id}"),
                company_id: COMPANY.to_string(),
                run_id: run_id.to_string(),
                work_package_revision: "wp-r1".to_string(),
                attempt_id: "attempt-1".to_string(),
                binding_id: "binding-1".to_string(),
                fencing_token: 1,
                adapter_id: "fake".to_string(),
                adapter_version: "1".to_string(),
                params_canonical: "params".to_string(),
                params_digest: "digest".to_string(),
                execution_id: None,
                state,
                created_at_ms: NOW,
                accepted_at_ms: None,
                last_observed_at_ms: None,
                retention_until_ms: None,
            };
            let uow = self.store.transaction().unwrap();
            uow.insert_invocation(&record).unwrap();
            uow.commit().unwrap();
        }

        fn reservation(&mut self, id: &str, state: ReservationState, run_id: Option<&str>) {
            let mut reservation = Reservation::new(
                id,
                DomainId::new(COMPANY),
                Measure::money_micros(1_000, "CNY"),
                1_000,
                NOW,
            );
            reservation.project_id = Some(DomainId::new(PROJECT));
            reservation.run_id = run_id.map(DomainId::new);
            reservation.state = state;
            match state {
                ReservationState::Active => {}
                ReservationState::Risk => {
                    reservation.reserved = 0;
                    reservation.risk = 1_000;
                }
                ReservationState::Consumed => {
                    reservation.reserved = 0;
                    reservation.confirmed = 1_000;
                }
                ReservationState::Cancelled => {
                    reservation.reserved = 0;
                }
            }
            let uow = self.store.transaction().unwrap();
            uow.insert_reservation(&reservation).unwrap();
            uow.commit().unwrap();
        }

        fn artifact(&mut self, artifact_id: &str) {
            artifact_acceptance::ensure_artifact(
                &mut self.store,
                &Artifact {
                    id: DomainId::new(artifact_id),
                    company_id: DomainId::new(COMPANY),
                    project_id: DomainId::new(PROJECT),
                    kind: ArtifactKind::Report,
                    title: artifact_id.to_string(),
                    owner: DomainId::new("coder-1"),
                    visibility: ArtifactVisibility::Internal,
                    accepted_revision: None,
                },
                NOW,
            )
            .unwrap();
        }

        fn cas_revision(&mut self, artifact_id: &str, revision_id: &str, body: &[u8]) {
            self.artifact(artifact_id);
            artifact_acceptance::submit_candidate(
                &mut self.store,
                &self.cas,
                COMPANY,
                &DomainId::new(artifact_id),
                &DomainId::new(revision_id),
                None,
                body,
                "text/markdown",
                &DomainId::new("coder-1"),
                &DomainId::new("rules-v1"),
                &DomainId::new("env-local"),
                NOW,
            )
            .unwrap();
        }

        fn external_revision(&mut self, artifact_id: &str, revision_id: &str, locator: &str) {
            self.artifact(artifact_id);
            artifact_acceptance::register_external_revision(
                &mut self.store,
                COMPANY,
                &DomainId::new(artifact_id),
                &DomainId::new(revision_id),
                None,
                locator,
                Digest::of_bytes(locator.as_bytes()),
                locator.len() as i64,
                "text/plain",
                &DomainId::new("coder-1"),
                "外部仓库提交定位，算法 sha256",
                NOW,
            )
            .unwrap();
        }

        /// 接受指针的语义由 artifact_acceptance 用例覆盖；此处只放一个已固定的指针。
        fn accept_pointer(&mut self, artifact_id: &str, revision_id: &str) {
            self.store
                .execute_batch_for_test(&format!(
                    "UPDATE artifacts SET accepted_revision = '{revision_id}'
                     WHERE uid = '{artifact_id}'"
                ))
                .unwrap();
        }

        fn manifest(&mut self, manifest_id: &str, revision_id: &str) {
            self.store
                .put_manifest(
                    COMPANY,
                    manifest_id,
                    PROJECT,
                    1,
                    &[revision_id.to_string()],
                    &[],
                    NOW,
                )
                .unwrap();
        }

        fn revision_count(&self) -> i64 {
            self.store.core_row_counts().unwrap().revisions
        }
    }

    fn scope() -> CleanupScope {
        CleanupScope::project(COMPANY, PROJECT)
    }

    fn codes(outcome: &CleanupOutcome) -> Vec<&'static str> {
        outcome.verdict.blocked_codes()
    }

    fn dispose_only() -> CleanupPolicy {
        CleanupPolicy::disposed("临时工作区与构建目录已删除")
    }

    #[test]
    fn cancelling_invocation_blocks_cleanup_and_keeps_records() {
        let mut fx = Fixture::new();
        fx.with_issue();
        fx.run_row("run-1");
        fx.invocation("run-1", ExecutionState::Cancelling);

        let outcome = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-1",
            NOW,
        )
        .unwrap();

        assert!(outcome.created);
        assert!(!outcome.verdict.is_ready());
        assert!(codes(&outcome).contains(&BLOCK_STOP_UNCONFIRMED));
        assert_eq!(outcome.request.state, CleanupState::Blocked);
        assert_eq!(
            outcome.request.blockers,
            vec![BLOCK_STOP_UNCONFIRMED.to_string()]
        );

        // 先标记删除请求：Issue 上删除标记已写，但记录没有被静默清空。
        assert_eq!(
            fx.store
                .issue_deletion_requested_at(COMPANY, "issue-1")
                .unwrap()
                .as_deref(),
            Some(NOW.to_string().as_str())
        );

        // 未满足条件时完成清理必须被拒绝，且不改变状态。
        let blocked =
            complete_cleanup(&mut fx.store, &scope(), &dispose_only(), NOW + 1).unwrap_err();
        assert!(matches!(blocked, CleanupError::Blocked { .. }));
        assert_eq!(
            fx.store
                .get_cleanup_request(COMPANY, CleanupTargetKind::Project, PROJECT)
                .unwrap()
                .unwrap()
                .state,
            CleanupState::Blocked
        );
    }

    #[test]
    fn unknown_execution_is_unknown_once_run_row_is_missing() {
        let mut fx = Fixture::new();
        fx.with_issue();
        // 有 invocation 但没有 runs 行：归属不可判定，必须按 Unknown 而不是"无在途"。
        fx.invocation("run-orphan", ExecutionState::Running);

        let outcome = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-2",
            NOW,
        )
        .unwrap();
        assert!(codes(&outcome).contains(&BLOCK_STOP_UNCONFIRMED));
        let condition = outcome.verdict.condition("stop_confirmed").unwrap();
        assert_eq!(condition.state, ConditionState::Unknown);
        assert!(condition.detail.contains("无法归属"));
    }

    #[test]
    fn active_reservation_blocks_and_risk_handed_to_case_releases() {
        // 未结算预留：阻塞。
        let mut fx = Fixture::new();
        fx.with_issue();
        fx.reservation("res-active", ReservationState::Active, None);
        let outcome = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-3",
            NOW,
        )
        .unwrap();
        assert!(codes(&outcome).contains(&BLOCK_BUDGET_UNSETTLED));

        // 已移交对账：风险占用保留在案件中，费用条件视为已处置。
        let mut fx = Fixture::new();
        fx.with_issue();
        fx.reservation("res-risk", ReservationState::Risk, None);
        let before = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-4",
            NOW,
        )
        .unwrap();
        assert!(
            codes(&before).contains(&BLOCK_BUDGET_UNSETTLED),
            "风险占用没有案件时必须阻塞"
        );

        open_case(
            &mut fx.store,
            &CaseRequest {
                company_id: COMPANY,
                case_id: "case-1",
                invocation_id: None,
                logical_action_key: "charge:res-risk",
                impact_scope: PROJECT,
                risk_reservation_refs: vec!["res-risk".to_string()],
                known_evidence: "外部账单未回",
                assignee: "user-1",
                next_step: "向供应商核对实际用量",
            },
            NOW + 1,
        )
        .unwrap();

        let after = refresh_cleanup(&mut fx.store, &scope(), &dispose_only(), NOW + 2).unwrap();
        assert_eq!(
            after.verdict.condition("budget_settled").unwrap().state,
            ConditionState::Satisfied
        );
        assert!(
            after
                .verdict
                .condition("budget_settled")
                .unwrap()
                .detail
                .contains("对账案件")
        );
        assert_eq!(after.request.state, CleanupState::Requested);
    }

    #[test]
    fn external_repo_and_formal_manifest_survive_project_delete() {
        let mut fx = Fixture::new();
        fx.with_issue();
        fx.external_revision("art-repo", "rev-repo", "repo:acme/site@abc123:README.md");
        fx.cas_revision("art-doc", "rev-accepted", b"accepted body");
        fx.accept_pointer("art-doc", "rev-accepted");
        fx.cas_revision("art-manifest", "rev-manifest", b"manifest body");
        fx.manifest("mf-1", "rev-manifest");
        fx.cas_revision("art-draft", "rev-draft", b"draft body");
        let revisions_before = fx.revision_count();
        assert_eq!(revisions_before, 4);

        // 未固定草稿存在且未授权 → 阻塞，不得静默丢弃。
        let blocked = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-5",
            NOW,
        )
        .unwrap();
        assert!(codes(&blocked).contains(&BLOCK_ARTIFACT_UNPINNED));
        assert_eq!(blocked.cascade.managed, vec!["rev-draft".to_string()]);
        let preserved: Vec<ExternalReason> = blocked
            .cascade
            .preserved
            .iter()
            .map(|item| item.reason)
            .collect();
        assert!(preserved.contains(&ExternalReason::Repository));
        assert!(preserved.contains(&ExternalReason::AcceptedDelivery));
        assert!(preserved.contains(&ExternalReason::FormalManifest));
        assert!(
            blocked
                .cascade
                .manual_followups
                .iter()
                .any(|item| item.contains("repo:acme/site@abc123:README.md"))
        );

        // 显式授权后才允许级联未固定草稿，且外部与正式引用仍保留。
        let policy = dispose_only().authorizing_cascade();
        let done = complete_cleanup(&mut fx.store, &scope(), &policy, NOW + 1).unwrap();
        assert!(done.verdict.is_ready());
        assert_eq!(done.request.state, CleanupState::Completed);
        assert_eq!(
            fx.revision_count(),
            revisions_before,
            "宿主不销毁外部仓库、正式交付与不可变修订"
        );
        assert_eq!(fx.store.list_artifacts(COMPANY, PROJECT).unwrap().len(), 4);
        assert!(
            done.cascade
                .preserved
                .iter()
                .any(|item| item.reference == "repo:acme/site@abc123:README.md")
        );
    }

    #[test]
    fn abandon_records_residuals_and_never_claims_success() {
        let mut fx = Fixture::new();
        fx.with_issue();
        fx.external_revision("art-prod", "rev-prod", "env:prod-cluster");
        let revisions_before = fx.revision_count();

        request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-6",
            NOW,
        )
        .unwrap();

        let abandon = abandon_cleanup(
            &mut fx.store,
            &scope(),
            &dispose_only(),
            &AbandonInput {
                owner: "user-1",
                reconciliation_entry: "doctor/reconcile:project-1",
                known_evidence: "外部环境仍在，用户放弃本次清理",
                abandon_id: "abn-1",
            },
            NOW + 1,
        )
        .unwrap();

        assert!(
            abandon
                .residual_resources
                .iter()
                .any(|item| item.contains("production_environment|rev-prod|env:prod-cluster"))
        );
        assert!(!abandon.risks.is_empty());
        assert_eq!(abandon.owner, "user-1");
        assert_eq!(abandon.reconciliation_entry, "doctor/reconcile:project-1");

        let request = fx
            .store
            .get_cleanup_request(COMPANY, CleanupTargetKind::Project, PROJECT)
            .unwrap()
            .unwrap();
        assert_eq!(request.state, CleanupState::Abandoned);
        assert!(
            request
                .blockers
                .iter()
                .any(|item| item.starts_with("abandoned:"))
        );

        // Abandon 之后不能改标为删除成功。
        assert!(matches!(
            complete_cleanup(&mut fx.store, &scope(), &dispose_only(), NOW + 2).unwrap_err(),
            CleanupError::AlreadyAbandoned
        ));
        assert_eq!(fx.revision_count(), revisions_before);
        assert_eq!(fx.store.list_artifacts(COMPANY, PROJECT).unwrap().len(), 1);
    }

    #[test]
    fn request_is_idempotent_and_cleanup_passes_effect_gate_once() {
        let mut fx = Fixture::new();
        fx.with_issue();

        let first = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-7",
            NOW,
        )
        .unwrap();
        assert!(first.created);
        assert!(first.verdict.is_ready());
        let effect = first.effect.clone().unwrap();
        assert!(effect.created);
        assert_eq!(effect.delivery.destination_id, CLEANUP_DESTINATION);

        let second = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &dispose_only(),
            "clr-7",
            NOW + 1,
        )
        .unwrap();
        assert!(!second.created, "同一目标重复请求不新建记录");
        assert_eq!(second.request.request_id, first.request.request_id);
        assert_eq!(
            fx.store
                .list_outbox_by_status(EffectStatus::Prepared)
                .unwrap()
                .len(),
            1,
            "Effect Gate 意图只有一条"
        );
        assert_eq!(
            fx.store.list_open_cleanup_requests(COMPANY).unwrap().len(),
            1
        );

        let done = complete_cleanup(&mut fx.store, &scope(), &dispose_only(), NOW + 2).unwrap();
        assert_eq!(done.request.state, CleanupState::Completed);
        let confirmed = fx
            .store
            .list_outbox_by_status(EffectStatus::Confirmed)
            .unwrap();
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].result_ref.as_deref(), Some("clr-7"));

        let again = complete_cleanup(&mut fx.store, &scope(), &dispose_only(), NOW + 3).unwrap();
        assert!(!again.created);
        assert_eq!(again.request.state, CleanupState::Completed);
        assert_eq!(
            fx.store
                .list_outbox_by_status(EffectStatus::Confirmed)
                .unwrap()
                .len(),
            1,
            "重复完成不产生第二个副作用"
        );
    }

    #[test]
    fn default_policy_treats_unknown_temp_env_as_blocked() {
        let mut fx = Fixture::new();
        fx.with_issue();
        let outcome = request_cleanup(
            &mut fx.store,
            &scope(),
            "user-1",
            &CleanupPolicy::default(),
            "clr-8",
            NOW,
        )
        .unwrap();
        assert_eq!(
            outcome
                .verdict
                .condition("temp_env_disposed")
                .unwrap()
                .state,
            ConditionState::Unknown
        );
        assert!(codes(&outcome).contains(&BLOCK_TEMP_ENV_UNKNOWN));
        assert_eq!(outcome.request.state, CleanupState::Blocked);
    }

    #[test]
    fn unknown_target_reports_scope_unknown_instead_of_success() {
        let mut fx = Fixture::new();
        let missing = CleanupScope::new(COMPANY, CleanupTargetKind::Artifact, "art-missing");
        let outcome = request_cleanup(
            &mut fx.store,
            &missing,
            "user-1",
            &dispose_only(),
            "clr-9",
            NOW,
        )
        .unwrap();
        assert!(codes(&outcome).contains(&BLOCK_SCOPE_UNKNOWN));
        assert!(!outcome.verdict.is_ready());
    }
}
