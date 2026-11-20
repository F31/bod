//! P4 任务 3.1—3.5：环境引用、发布审批、部署状态、回滚边界与发布门禁（`release-governance`）。
//!
//! 发布这条线上最容易出的错，都是"把愿望当事实"：期望版本当成已部署、请求发出当成已成功、
//! 回滚声明当成已恢复、预算解释当成可花。本模块把这四件事各自拆成两条轴：
//! 1. **期望 vs 实际**（3.1）：`EnvironmentRef` 分开记 desired/observed；观测缺失就是 `Unknown`，
//!    不一致就是 `Drifted`，只有相等才叫已部署。密钥不进 Manifest 明文。
//! 2. **审批 vs 目标**（3.2）：审批绑 artifacts digest、环境、参数与有效期；任何一项变了，
//!    原批准失效，必须重新批准（AZ05 同类）。
//! 3. **请求 vs 结果**（3.3）：部署状态机只在验证通过后到 `Succeeded`；回包丢失进 `Unknown`，
//!    先按 invocation/业务键查询，查询带证据才落定，不盲重发。
//! 4. **回滚 vs 前向修复**（3.4）：只恢复已声明可回退的资源；不可逆迁移必须给前向修复方案。
//! 5. **预算解释 vs 门禁**（3.5）：可用额度 = 限额 − 已结算 − 有效预留 − 未知风险占用；
//!    不足就阻止新发布，只读解释不动验收与历史账本。
//!
//! 时间是固定格式的 UTC 串（`2026-11-21T09:00:00Z`），字典序即时间序；本模块不解析时区。

use crate::stage_manifest::looks_like_credential;
use std::fmt;

// ---------------------------------------------------------------------------
// 3.1 环境引用与状态投影
// ---------------------------------------------------------------------------

/// 环境引用：平台管资源，bod 管"要哪个版本、谁能操作、结果是否满足验收"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentRef {
    pub environment_id: String,
    pub provider: String,
    pub external_id: String,
    pub purpose: String,
    /// 允许谁操作（角色或主体），与平台侧权限分开记录。
    pub access_scope: String,
    pub lifecycle: String,
    pub desired_revision: String,
    pub observed_revision: Option<String>,
    pub observed_at: Option<String>,
}

impl EnvironmentRef {
    pub fn new(environment_id: &str, provider: &str, external_id: &str, purpose: &str) -> Self {
        Self {
            environment_id: environment_id.to_string(),
            provider: provider.to_string(),
            external_id: external_id.to_string(),
            purpose: purpose.to_string(),
            access_scope: String::new(),
            lifecycle: String::new(),
            desired_revision: String::new(),
            observed_revision: None,
            observed_at: None,
        }
    }

    pub fn with_access_scope(mut self, scope: &str) -> Self {
        self.access_scope = scope.to_string();
        self
    }

    pub fn with_lifecycle(mut self, lifecycle: &str) -> Self {
        self.lifecycle = lifecycle.to_string();
        self
    }

    pub fn with_desired(mut self, revision: &str) -> Self {
        self.desired_revision = revision.to_string();
        self
    }

    /// 记录一次实际观测。`revision` 为 `None` 表示观测不到（不是"没变化"）。
    pub fn observe(&mut self, revision: Option<&str>, at: &str) {
        self.observed_revision = revision.map(|item| item.to_string());
        self.observed_at = Some(at.to_string());
    }

    pub fn drift(&self) -> Drift {
        match (&self.observed_revision, self.desired_revision.is_empty()) {
            (None, _) => Drift::Unknown,
            (Some(_), true) => Drift::Unknown,
            (Some(observed), false) if *observed == self.desired_revision => Drift::None,
            (Some(observed), false) => Drift::Detected {
                desired: self.desired_revision.clone(),
                observed: observed.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    None,
    Detected {
        desired: String,
        observed: String,
    },
    /// 没观测到，就不能说"没有漂移"。
    Unknown,
}

impl Drift {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Detected { .. } => "detected",
            Self::Unknown => "unknown",
        }
    }
}

/// 部署状态投影。期望状态永远不投影成"已部署"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentProjection {
    Deployed {
        revision: String,
        observed_at: String,
    },
    Drifted {
        desired: String,
        observed: String,
        observed_at: String,
    },
    Unknown {
        reason: String,
    },
}

impl DeploymentProjection {
    pub fn is_deployed(&self) -> bool {
        matches!(self, Self::Deployed { .. })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Deployed { .. } => "deployed",
            Self::Drifted { .. } => "drifted",
            Self::Unknown { .. } => "unknown",
        }
    }
}

impl EnvironmentRef {
    pub fn projection(&self) -> DeploymentProjection {
        match (&self.observed_revision, &self.observed_at) {
            (Some(observed), Some(at)) if *observed == self.desired_revision => {
                DeploymentProjection::Deployed {
                    revision: observed.clone(),
                    observed_at: at.clone(),
                }
            }
            (Some(observed), Some(at)) => DeploymentProjection::Drifted {
                desired: self.desired_revision.clone(),
                observed: observed.clone(),
                observed_at: at.clone(),
            },
            _ => DeploymentProjection::Unknown {
                reason: "没有实际版本观测".to_string(),
            },
        }
    }
}

/// Manifest 明文里的密钥泄漏。名字和值都查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretLeak {
    pub field: String,
    pub why: String,
}

/// 校验要写进 Manifest 的环境条目。密钥必须走平台或凭据引用，不进明文。
pub fn check_manifest_env(fields: &[(&str, &str)]) -> Result<(), SecretLeak> {
    for (name, value) in fields {
        let lowered = name.to_ascii_lowercase();
        let suspicious_name = ["secret", "password", "token", "private_key", "credential"]
            .iter()
            .any(|needle| lowered.contains(needle));
        if suspicious_name {
            return Err(SecretLeak {
                field: (*name).to_string(),
                why: "字段名像密钥，Manifest 只能放凭据引用".to_string(),
            });
        }
        if looks_like_credential(value) {
            return Err(SecretLeak {
                field: (*name).to_string(),
                why: "字段值形似凭据".to_string(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3.2 发布计划与审批
// ---------------------------------------------------------------------------

/// 迁移声明。`reversible = false` 的迁移会直接决定回滚边界（3.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub name: String,
    pub reversible: bool,
    /// 不可逆时的前向修复方案。
    pub forward_fix: String,
}

impl Migration {
    pub fn reversible(name: &str) -> Self {
        Self {
            name: name.to_string(),
            reversible: true,
            forward_fix: String::new(),
        }
    }

    pub fn irreversible(name: &str, forward_fix: &str) -> Self {
        Self {
            name: name.to_string(),
            reversible: false,
            forward_fix: forward_fix.to_string(),
        }
    }
}

/// 发布计划：审批看的对象就是它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePlan {
    pub plan_id: String,
    pub manifest_id: String,
    pub revision: String,
    pub artifact_digest: String,
    pub environment_id: String,
    pub deployment_method: String,
    pub parameters_digest: String,
    pub migrations: Vec<Migration>,
    pub verification: Vec<String>,
    pub stop_conditions: Vec<String>,
    pub rollback_basis: Vec<String>,
}

impl ReleasePlan {
    pub fn new(
        plan_id: &str,
        manifest_id: &str,
        revision: &str,
        artifact_digest: &str,
        environment_id: &str,
    ) -> Self {
        Self {
            plan_id: plan_id.to_string(),
            manifest_id: manifest_id.to_string(),
            revision: revision.to_string(),
            artifact_digest: artifact_digest.to_string(),
            environment_id: environment_id.to_string(),
            deployment_method: String::new(),
            parameters_digest: String::new(),
            migrations: Vec::new(),
            verification: Vec::new(),
            stop_conditions: Vec::new(),
            rollback_basis: Vec::new(),
        }
    }

    pub fn with_method(mut self, method: &str) -> Self {
        self.deployment_method = method.to_string();
        self
    }

    pub fn with_parameters(mut self, digest: &str) -> Self {
        self.parameters_digest = digest.to_string();
        self
    }
}

/// 审批：五个绑定字段缺一不可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub approval_id: String,
    pub artifact_digest: String,
    pub environment_id: String,
    pub parameters_digest: String,
    pub valid_until: String,
    pub approved_by: String,
    pub approved_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalCheck {
    Valid,
    Expired { valid_until: String, now: String },
    TargetChanged { approved: String, planned: String },
    DigestChanged { approved: String, planned: String },
    ParametersChanged { approved: String, planned: String },
}

impl ApprovalCheck {
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Valid => "审批有效".to_string(),
            Self::Expired { valid_until, now } => {
                format!("审批已于 {valid_until} 过期（当前 {now}）")
            }
            Self::TargetChanged { approved, planned } => {
                format!("审批目标 {approved} 与计划目标 {planned} 不一致")
            }
            Self::DigestChanged { approved, planned } => {
                format!("审批制品 {approved} 与计划制品 {planned} 不一致")
            }
            Self::ParametersChanged { approved, planned } => {
                format!("审批参数 {approved} 与计划参数 {planned} 不一致")
            }
        }
    }
}

/// 审批核对。改目标、改 digest、改参数、过期——任一不成立就作废（AZ05 同类）。
pub fn check_approval(plan: &ReleasePlan, approval: &Approval, now: &str) -> ApprovalCheck {
    if approval.environment_id != plan.environment_id {
        return ApprovalCheck::TargetChanged {
            approved: approval.environment_id.clone(),
            planned: plan.environment_id.clone(),
        };
    }
    if approval.artifact_digest != plan.artifact_digest {
        return ApprovalCheck::DigestChanged {
            approved: approval.artifact_digest.clone(),
            planned: plan.artifact_digest.clone(),
        };
    }
    if approval.parameters_digest != plan.parameters_digest {
        return ApprovalCheck::ParametersChanged {
            approved: approval.parameters_digest.clone(),
            planned: plan.parameters_digest.clone(),
        };
    }
    if now > approval.valid_until.as_str() {
        return ApprovalCheck::Expired {
            valid_until: approval.valid_until.clone(),
            now: now.to_string(),
        };
    }
    ApprovalCheck::Valid
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseAdmission {
    Admitted,
    Refused {
        reason: ReleaseBlockReason,
        detail: String,
    },
}

impl ReleaseAdmission {
    pub fn admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }
}

/// 拒绝发布的原因。`unblock` 是给用户看的"怎么解"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseBlockReason {
    ApprovalInvalid,
    BudgetExhausted,
    DeploymentUnknown { deployment_id: String },
}

impl ReleaseBlockReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ApprovalInvalid => "approval-invalid",
            Self::BudgetExhausted => "budget-exhausted",
            Self::DeploymentUnknown { .. } => "deployment-unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// 3.3 部署状态机
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentState {
    Pending,
    Running,
    Verifying,
    Succeeded,
    Failed,
    Unknown,
}

impl DeploymentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Verifying => "verifying",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentEvent {
    /// 提交之后由 Effect Gate 派发。
    DispatchSent {
        at: String,
    },
    /// 连接器接受了请求。这不等于成功。
    Accepted {
        at: String,
    },
    Rejected {
        at: String,
        reason: String,
    },
    /// 回包丢失：只能先查。
    AckLost {
        at: String,
    },
    VerificationStarted {
        at: String,
    },
    VerificationPassed {
        at: String,
    },
    VerificationFailed {
        at: String,
        reason: String,
    },
    /// 查询后落定，必须带证据。
    QueryResolved {
        at: String,
        state: DeploymentState,
        evidence: String,
    },
}

impl DeploymentEvent {
    fn at(&self) -> &str {
        match self {
            Self::DispatchSent { at }
            | Self::Accepted { at }
            | Self::Rejected { at, .. }
            | Self::AckLost { at }
            | Self::VerificationStarted { at }
            | Self::VerificationPassed { at }
            | Self::VerificationFailed { at, .. }
            | Self::QueryResolved { at, .. } => at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    BeforeCommit,
    WrongState {
        state: DeploymentState,
        event: &'static str,
    },
    ResolveWithoutEvidence,
    ResolveToUnknown,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeCommit => write!(formatter, "外部动作必须在提交后派发"),
            Self::WrongState { state, event } => {
                write!(formatter, "状态 {} 不接受事件 {event}", state.as_str())
            }
            Self::ResolveWithoutEvidence => write!(formatter, "查询落定必须带证据"),
            Self::ResolveToUnknown => write!(formatter, "查询结果不能仍是 Unknown"),
        }
    }
}

impl std::error::Error for TransitionError {}

/// 一次部署。`invocation_id` 与业务键就是回包丢失后查询用的钥匙。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub deployment_id: String,
    pub plan_id: String,
    pub invocation_id: String,
    pub business_key: String,
    pub state: DeploymentState,
    pub committed: bool,
    pub requested_at: Option<String>,
    pub resolved_at: Option<String>,
    pub query_attempts: u32,
    pub evidence: Vec<String>,
}

impl Deployment {
    pub fn new(
        deployment_id: &str,
        plan_id: &str,
        invocation_id: &str,
        business_key: &str,
    ) -> Self {
        Self {
            deployment_id: deployment_id.to_string(),
            plan_id: plan_id.to_string(),
            invocation_id: invocation_id.to_string(),
            business_key: business_key.to_string(),
            state: DeploymentState::Pending,
            committed: false,
            requested_at: None,
            resolved_at: None,
            query_attempts: 0,
            evidence: Vec::new(),
        }
    }

    /// 状态落库提交之后才允许派发外部动作。
    pub fn mark_committed(&mut self) {
        self.committed = true;
    }

    /// 能否重发。Unknown 状态一律不能：先查。
    pub fn can_resend(&self) -> bool {
        matches!(
            self.state,
            DeploymentState::Pending | DeploymentState::Failed
        )
    }

    pub fn unknown_query_key(&self) -> Option<&str> {
        if self.state == DeploymentState::Unknown {
            Some(self.invocation_id.as_str())
        } else {
            None
        }
    }
}

/// 推进部署状态。返回新状态或拒绝原因，调用方据此决定是否落库。
pub fn apply(
    deployment: &mut Deployment,
    event: DeploymentEvent,
) -> Result<DeploymentState, TransitionError> {
    let at = event.at().to_string();
    match event {
        DeploymentEvent::DispatchSent { .. } => {
            if !deployment.committed {
                return Err(TransitionError::BeforeCommit);
            }
            if deployment.state != DeploymentState::Pending {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "dispatch-sent",
                });
            }
            deployment.requested_at = Some(at);
            deployment.state = DeploymentState::Running;
        }
        DeploymentEvent::Accepted { .. } => {
            // 接受请求只是"收到了"，状态仍是 Running。
            if deployment.state != DeploymentState::Running {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "accepted",
                });
            }
        }
        DeploymentEvent::Rejected { reason, .. } => {
            if deployment.state != DeploymentState::Running {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "rejected",
                });
            }
            deployment.evidence.push(format!("连接器拒绝：{reason}"));
            deployment.state = DeploymentState::Failed;
        }
        DeploymentEvent::AckLost { .. } => {
            if !matches!(
                deployment.state,
                DeploymentState::Running | DeploymentState::Verifying
            ) {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "ack-lost",
                });
            }
            deployment.state = DeploymentState::Unknown;
            deployment.evidence.push("回包丢失，未确认结果".to_string());
        }
        DeploymentEvent::VerificationStarted { .. } => {
            if deployment.state != DeploymentState::Running {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "verification-started",
                });
            }
            deployment.state = DeploymentState::Verifying;
        }
        DeploymentEvent::VerificationPassed { .. } => {
            if deployment.state != DeploymentState::Verifying {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "verification-passed",
                });
            }
            deployment.state = DeploymentState::Succeeded;
            deployment.resolved_at = Some(at);
        }
        DeploymentEvent::VerificationFailed { reason, .. } => {
            if deployment.state != DeploymentState::Verifying {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "verification-failed",
                });
            }
            deployment.evidence.push(format!("验证失败：{reason}"));
            deployment.state = DeploymentState::Failed;
            deployment.resolved_at = Some(at);
        }
        DeploymentEvent::QueryResolved {
            state, evidence, ..
        } => {
            if deployment.state != DeploymentState::Unknown {
                return Err(TransitionError::WrongState {
                    state: deployment.state,
                    event: "query-resolved",
                });
            }
            if evidence.trim().is_empty() {
                return Err(TransitionError::ResolveWithoutEvidence);
            }
            if state == DeploymentState::Unknown {
                return Err(TransitionError::ResolveToUnknown);
            }
            deployment.query_attempts += 1;
            deployment.evidence.push(format!("查询证据：{evidence}"));
            deployment.state = state;
            deployment.resolved_at = Some(at);
        }
    }
    Ok(deployment.state)
}

// ---------------------------------------------------------------------------
// 3.4 回滚边界
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    ApplicationImage,
    Configuration,
    DatabaseMigration,
    ExternalMessage,
    DataWrite,
}

impl ResourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ApplicationImage => "application-image",
            Self::Configuration => "configuration",
            Self::DatabaseMigration => "database-migration",
            Self::ExternalMessage => "external-message",
            Self::DataWrite => "data-write",
        }
    }

    /// 外部消息与数据写入天然不可撤回：发出去就是发出去了。
    pub fn inherently_irreversible(&self) -> bool {
        matches!(self, Self::ExternalMessage | Self::DataWrite)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedResource {
    pub name: String,
    pub kind: ResourceKind,
    pub reversible: bool,
    pub forward_fix: String,
}

impl ReleasedResource {
    pub fn reversible(name: &str, kind: ResourceKind) -> Self {
        Self {
            name: name.to_string(),
            kind,
            reversible: true,
            forward_fix: String::new(),
        }
    }

    pub fn irreversible(name: &str, kind: ResourceKind, forward_fix: &str) -> Self {
        Self {
            name: name.to_string(),
            kind,
            reversible: false,
            forward_fix: forward_fix.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackPlan {
    pub deployment_id: String,
    pub resources: Vec<ReleasedResource>,
    pub steps: Vec<String>,
}

impl RollbackPlan {
    pub fn new(deployment_id: &str) -> Self {
        Self {
            deployment_id: deployment_id.to_string(),
            resources: Vec::new(),
            steps: Vec::new(),
        }
    }

    pub fn push(&mut self, resource: ReleasedResource) -> &mut Self {
        self.resources.push(resource);
        self
    }

    /// 回滚结论。不可逆资源进 `forward_fix_required`，绝不进 `restored`。
    pub fn verdict(&self) -> RollbackVerdict {
        let mut restorable = Vec::new();
        let mut forward_fix_required = Vec::new();
        let mut undeclared = Vec::new();
        for resource in &self.resources {
            let cannot_reverse = !resource.reversible || resource.kind.inherently_irreversible();
            if cannot_reverse {
                if resource.forward_fix.trim().is_empty() {
                    undeclared.push(resource.name.clone());
                } else {
                    forward_fix_required.push(ForwardFix {
                        resource: resource.name.clone(),
                        kind: resource.kind,
                        fix: resource.forward_fix.clone(),
                    });
                }
            } else {
                restorable.push(resource.name.clone());
            }
        }
        let claim = if forward_fix_required.is_empty() && undeclared.is_empty() {
            RestoreClaim::Full
        } else {
            RestoreClaim::Partial
        };
        RollbackVerdict {
            restorable,
            forward_fix_required,
            undeclared,
            claim,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardFix {
    pub resource: String,
    pub kind: ResourceKind,
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreClaim {
    /// 全部资源都声明可回退且能执行。
    Full,
    /// 至少一项不能回退：只能说"部分恢复 + 前向修复"。
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackVerdict {
    pub restorable: Vec<String>,
    pub forward_fix_required: Vec<ForwardFix>,
    pub undeclared: Vec<String>,
    pub claim: RestoreClaim,
}

impl RollbackVerdict {
    pub fn claims_full_restore(&self) -> bool {
        self.claim == RestoreClaim::Full
    }

    pub fn summary(&self) -> String {
        match self.claim {
            RestoreClaim::Full => format!("可恢复 {} 项资源", self.restorable.len()),
            RestoreClaim::Partial => format!(
                "可恢复 {} 项；{} 项需前向修复；{} 项未声明处置",
                self.restorable.len(),
                self.forward_fix_required.len(),
                self.undeclared.len()
            ),
        }
    }
}

/// 自动回滚策略。必须是"验证过且授权过"的，不能靠默认开启。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackPolicy {
    pub auto: bool,
    pub verified: bool,
    pub authorized: bool,
    pub authorized_by: Option<String>,
}

impl RollbackPolicy {
    pub fn manual() -> Self {
        Self {
            auto: false,
            verified: false,
            authorized: false,
            authorized_by: None,
        }
    }
}

pub fn may_auto_rollback(policy: &RollbackPolicy) -> bool {
    policy.auto && policy.verified && policy.authorized
}

// ---------------------------------------------------------------------------
// 3.5 预算解释与发布门禁
// ---------------------------------------------------------------------------

/// 计量来源。`Invocation` 之外的来源都算覆盖缺口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeteringSource {
    Invocation,
    Estimate,
    Aggregated,
}

impl MeteringSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Invocation => "invocation",
            Self::Estimate => "estimate",
            Self::Aggregated => "aggregated",
        }
    }
}

/// 预算解释。可用额度 = 限额 − 已结算 − 有效预留 − 未知风险占用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExplanation {
    pub limit_minor: i64,
    pub settled_minor: i64,
    pub reserved_minor: i64,
    pub unknown_risk_minor: i64,
    pub metering_source: MeteringSource,
    pub coverage_gaps: Vec<String>,
}

impl BudgetExplanation {
    pub fn available(&self) -> i64 {
        self.limit_minor - self.settled_minor - self.reserved_minor - self.unknown_risk_minor
    }

    pub fn gaps(&self) -> Vec<String> {
        let mut gaps = self.coverage_gaps.clone();
        if self.metering_source != MeteringSource::Invocation {
            gaps.push(format!(
                "计量来源为 {}，未按实际 invocation 记账",
                self.metering_source.as_str()
            ));
        }
        gaps
    }

    /// 解释本身要自洽：可用额度为负说明已经超支，不能显示成"还有额度"。
    pub fn consistent(&self) -> bool {
        self.available() >= 0
    }
}

/// 一次发布的准入：审批 + 预算 + 在途 Unknown 三处都要过。
pub fn admit_release(
    plan: &ReleasePlan,
    approval: &Approval,
    now: &str,
    budget: &BudgetExplanation,
    open_deployments: &[Deployment],
) -> ReleaseAdmission {
    let check = check_approval(plan, approval, now);
    if !check.is_valid() {
        return ReleaseAdmission::Refused {
            reason: ReleaseBlockReason::ApprovalInvalid,
            detail: check.describe(),
        };
    }
    for deployment in open_deployments {
        if deployment.state == DeploymentState::Unknown && deployment.plan_id == plan.plan_id {
            return ReleaseAdmission::Refused {
                reason: ReleaseBlockReason::DeploymentUnknown {
                    deployment_id: deployment.deployment_id.clone(),
                },
                detail: format!(
                    "上一次部署 {} 结果未知，先按 invocation {} 查询",
                    deployment.deployment_id, deployment.invocation_id
                ),
            };
        }
    }
    if budget.available() <= 0 {
        return ReleaseAdmission::Refused {
            reason: ReleaseBlockReason::BudgetExhausted,
            detail: format!(
                "可用额度 {} 分（限额 {} − 已结算 {} − 预留 {} − 未知风险 {}），需提额或结清",
                budget.available(),
                budget.limit_minor,
                budget.settled_minor,
                budget.reserved_minor,
                budget.unknown_risk_minor
            ),
        };
    }
    ReleaseAdmission::Admitted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::artifact::sha256_hex;

    fn dg(seed: &str) -> String {
        format!("sha256:{}", sha256_hex(seed.as_bytes()))
    }

    fn env() -> EnvironmentRef {
        EnvironmentRef::new("env-staging", "k8s", "cluster-a/ns-b", "预发布")
            .with_access_scope("release-ops")
            .with_lifecycle("persistent")
            .with_desired("build-42")
    }

    fn plan() -> ReleasePlan {
        ReleasePlan::new("plan-1", "stage-1", "rev-2", &dg("pkg"), "env-staging")
            .with_method("rolling")
            .with_parameters("params-1")
    }

    fn approval(plan: &ReleasePlan) -> Approval {
        Approval {
            approval_id: "apr-1".to_string(),
            artifact_digest: plan.artifact_digest.clone(),
            environment_id: plan.environment_id.clone(),
            parameters_digest: plan.parameters_digest.clone(),
            valid_until: "2026-11-22T00:00:00Z".to_string(),
            approved_by: "lead-1".to_string(),
            approved_at: "2026-11-21T09:00:00Z".to_string(),
        }
    }

    /// 造一份"还剩 `available_minor` 分"的解释：已结算 4000、预留 2000、未知风险 1000，
    /// 限额随可用额度上调，保证四者自洽。
    fn budget(available_minor: i64) -> BudgetExplanation {
        let settled = 4_000;
        let reserved = 2_000;
        let unknown = 1_000;
        BudgetExplanation {
            limit_minor: settled + reserved + unknown + available_minor,
            settled_minor: settled,
            reserved_minor: reserved,
            unknown_risk_minor: unknown,
            metering_source: MeteringSource::Invocation,
            coverage_gaps: Vec::new(),
        }
    }

    /// 3.1：实际版本与期望不一致 → 记 drift，不显示为已部署。
    #[test]
    fn drifted_environment_is_not_shown_as_deployed() {
        let mut env = env();
        assert_eq!(env.drift(), Drift::Unknown, "没观测就是未知，不是无漂移");
        assert_eq!(env.projection().as_str(), "unknown");

        env.observe(Some("build-39"), "2026-11-21T09:10:00Z");
        assert_eq!(
            env.drift(),
            Drift::Detected {
                desired: "build-42".to_string(),
                observed: "build-39".to_string()
            }
        );
        match env.projection() {
            DeploymentProjection::Drifted {
                desired,
                observed,
                observed_at,
            } => {
                assert_eq!(desired, "build-42");
                assert_eq!(observed, "build-39");
                assert_eq!(observed_at, "2026-11-21T09:10:00Z");
            }
            other => panic!("漂移不得显示为已部署，实际 {other:?}"),
        }
        assert!(!env.projection().is_deployed());

        env.observe(Some("build-42"), "2026-11-21T09:20:00Z");
        assert_eq!(env.drift(), Drift::None);
        assert!(env.projection().is_deployed());
        assert_eq!(env.observed_revision.as_deref(), Some("build-42"));
    }

    /// 3.1：观测不到版本时保持 Unknown，期望状态不能顶替实际状态。
    #[test]
    fn unknown_environment_is_not_shown_as_healthy() {
        let mut env = env();
        env.observe(None, "2026-11-21T09:30:00Z");
        assert_eq!(env.drift(), Drift::Unknown);
        match env.projection() {
            DeploymentProjection::Unknown { reason } => {
                assert!(reason.contains("没有实际版本观测"));
            }
            other => panic!("观测不到应保持 Unknown，实际 {other:?}"),
        }

        let mut undeclared = EnvironmentRef::new("env-x", "vm", "vm-1", "临时");
        undeclared.observe(Some("build-1"), "2026-11-21T09:30:00Z");
        assert_eq!(undeclared.drift(), Drift::Unknown, "没有期望版本也是未知");
        assert!(!undeclared.projection().is_deployed());
    }

    /// 3.1：密钥不进 Manifest 明文，字段名与字段值都查。
    #[test]
    fn manifest_env_rejects_secret_fields() {
        assert_eq!(
            check_manifest_env(&[("revision", "build-42"), ("namespace", "prod-a")]),
            Ok(())
        );
        assert_eq!(
            check_manifest_env(&[("db_password", "hunter2")]),
            Err(SecretLeak {
                field: "db_password".to_string(),
                why: "字段名像密钥，Manifest 只能放凭据引用".to_string()
            })
        );
        match check_manifest_env(&[("endpoint", "https://api.example.com/?api_key=abcd1234")]) {
            Err(leak) => assert!(leak.why.contains("形似凭据")),
            Ok(()) => panic!("值里的凭据必须被拦住"),
        }
    }

    /// 3.2 / AZ05：审批后改目标 → 原批准失效，必须重新批准。
    #[test]
    fn approval_after_target_change_becomes_invalid() {
        let plan = plan();
        let approval = approval(&plan);
        assert_eq!(
            check_approval(&plan, &approval, "2026-11-21T10:00:00Z"),
            ApprovalCheck::Valid
        );

        let mut retargeted = plan.clone();
        retargeted.environment_id = "env-prod".to_string();
        let check = check_approval(&retargeted, &approval, "2026-11-21T10:00:00Z");
        assert_eq!(
            check,
            ApprovalCheck::TargetChanged {
                approved: "env-staging".to_string(),
                planned: "env-prod".to_string()
            }
        );
        let admission = admit_release(
            &retargeted,
            &approval,
            "2026-11-21T10:00:00Z",
            &budget(3_000),
            &[],
        );
        assert!(!admission.admitted(), "改目标后不得放行");
        match admission {
            ReleaseAdmission::Refused { reason, detail } => {
                assert_eq!(reason, ReleaseBlockReason::ApprovalInvalid);
                assert!(detail.contains("不一致"));
            }
            other => panic!("应拒绝，实际 {other:?}"),
        }
    }

    /// 3.2：审批同时绑 digest、参数与有效期；改任一项都不再有效。
    #[test]
    fn approval_binds_digest_parameters_and_expiry() {
        let plan = plan();
        let mut approval = approval(&plan);

        let mut new_digest = plan.clone();
        new_digest.artifact_digest = dg("pkg-v2");
        assert!(matches!(
            check_approval(&new_digest, &approval, "2026-11-21T10:00:00Z"),
            ApprovalCheck::DigestChanged { .. }
        ));

        let mut new_params = plan.clone();
        new_params.parameters_digest = "params-2".to_string();
        assert!(matches!(
            check_approval(&new_params, &approval, "2026-11-21T10:00:00Z"),
            ApprovalCheck::ParametersChanged { .. }
        ));

        approval.valid_until = "2026-11-21T09:30:00Z".to_string();
        match check_approval(&plan, &approval, "2026-11-21T10:00:00Z") {
            ApprovalCheck::Expired { valid_until, now } => {
                assert_eq!(valid_until, "2026-11-21T09:30:00Z");
                assert_eq!(now, "2026-11-21T10:00:00Z");
            }
            other => panic!("过期审批必须失效，实际 {other:?}"),
        }
    }

    /// 3.2：过期审批直接拒绝发布，并给出可读原因。
    #[test]
    fn expired_approval_refuses_the_release() {
        let plan = plan();
        let mut approval = approval(&plan);
        approval.valid_until = "2026-11-21T09:30:00Z".to_string();
        let admission = admit_release(
            &plan,
            &approval,
            "2026-11-21T10:00:00Z",
            &budget(5_000),
            &[],
        );
        match admission {
            ReleaseAdmission::Refused { reason, detail } => {
                assert_eq!(reason, ReleaseBlockReason::ApprovalInvalid);
                assert!(detail.contains("过期"));
            }
            other => panic!("应拒绝，实际 {other:?}"),
        }
    }

    /// 3.3：状态没提交就派发外部动作 → 拒绝。
    #[test]
    fn dispatch_before_commit_is_refused() {
        let mut deployment = Deployment::new("dep-1", "plan-1", "inv-1", "quota@prod");
        let err = apply(
            &mut deployment,
            DeploymentEvent::DispatchSent {
                at: "2026-11-21T10:00:00Z".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(err, TransitionError::BeforeCommit);
        assert_eq!(deployment.state, DeploymentState::Pending);

        deployment.mark_committed();
        assert_eq!(
            apply(
                &mut deployment,
                DeploymentEvent::DispatchSent {
                    at: "2026-11-21T10:00:00Z".to_string(),
                }
            ),
            Ok(DeploymentState::Running)
        );
        assert_eq!(
            deployment.requested_at.as_deref(),
            Some("2026-11-21T10:00:00Z")
        );
    }

    /// 3.3：回包丢失 → Unknown，保留占用，禁止盲重发，只能按 invocation 查。
    #[test]
    fn lost_acknowledgement_becomes_unknown_and_blocks_resend() {
        let mut deployment = Deployment::new("dep-1", "plan-1", "inv-1", "quota@prod");
        deployment.mark_committed();
        apply(
            &mut deployment,
            DeploymentEvent::DispatchSent {
                at: "2026-11-21T10:00:00Z".to_string(),
            },
        )
        .unwrap();
        apply(
            &mut deployment,
            DeploymentEvent::AckLost {
                at: "2026-11-21T10:01:00Z".to_string(),
            },
        )
        .unwrap();

        assert_eq!(deployment.state, DeploymentState::Unknown);
        assert!(!deployment.can_resend(), "Unknown 时不得重发");
        assert_eq!(deployment.unknown_query_key(), Some("inv-1"));

        let admission = admit_release(
            &plan(),
            &approval(&plan()),
            "2026-11-21T10:05:00Z",
            &budget(5_000),
            std::slice::from_ref(&deployment),
        );
        match admission {
            ReleaseAdmission::Refused { reason, detail } => {
                assert_eq!(
                    reason,
                    ReleaseBlockReason::DeploymentUnknown {
                        deployment_id: "dep-1".to_string()
                    }
                );
                assert!(detail.contains("inv-1"), "要按 invocation 查");
            }
            other => panic!("在途 Unknown 应阻止新发布，实际 {other:?}"),
        }
    }

    /// 3.3：Unknown 只能靠带证据的查询落定，查询结果不能还是 Unknown。
    #[test]
    fn unknown_is_only_resolved_by_a_query_with_evidence() {
        let mut deployment = Deployment::new("dep-1", "plan-1", "inv-1", "quota@prod");
        deployment.mark_committed();
        apply(
            &mut deployment,
            DeploymentEvent::DispatchSent {
                at: "2026-11-21T10:00:00Z".to_string(),
            },
        )
        .unwrap();
        apply(
            &mut deployment,
            DeploymentEvent::AckLost {
                at: "2026-11-21T10:01:00Z".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            apply(
                &mut deployment,
                DeploymentEvent::QueryResolved {
                    at: "2026-11-21T10:02:00Z".to_string(),
                    state: DeploymentState::Succeeded,
                    evidence: "  ".to_string(),
                }
            ),
            Err(TransitionError::ResolveWithoutEvidence)
        );
        assert_eq!(
            apply(
                &mut deployment,
                DeploymentEvent::QueryResolved {
                    at: "2026-11-21T10:02:00Z".to_string(),
                    state: DeploymentState::Unknown,
                    evidence: "接口超时".to_string(),
                }
            ),
            Err(TransitionError::ResolveToUnknown)
        );

        assert_eq!(
            apply(
                &mut deployment,
                DeploymentEvent::QueryResolved {
                    at: "2026-11-21T10:03:00Z".to_string(),
                    state: DeploymentState::Succeeded,
                    evidence: "connector readback: status=applied".to_string(),
                }
            ),
            Ok(DeploymentState::Succeeded)
        );
        assert_eq!(deployment.query_attempts, 1);
        assert!(!deployment.can_resend(), "已落定成功，不需要重发");
        assert_eq!(deployment.unknown_query_key(), None);
        assert!(
            deployment
                .evidence
                .iter()
                .any(|item| item.contains("connector readback"))
        );
        assert_eq!(
            deployment.resolved_at.as_deref(),
            Some("2026-11-21T10:03:00Z")
        );
    }

    /// 3.3：连接器接受请求不等于成功，必须走完验证才到 Succeeded。
    #[test]
    fn connector_acceptance_is_not_success() {
        let mut deployment = Deployment::new("dep-1", "plan-1", "inv-1", "quota@prod");
        deployment.mark_committed();
        apply(
            &mut deployment,
            DeploymentEvent::DispatchSent {
                at: "2026-11-21T10:00:00Z".to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            apply(
                &mut deployment,
                DeploymentEvent::Accepted {
                    at: "2026-11-21T10:00:30Z".to_string(),
                }
            ),
            Ok(DeploymentState::Running),
            "收到请求仍只是 Running"
        );
        apply(
            &mut deployment,
            DeploymentEvent::VerificationStarted {
                at: "2026-11-21T10:02:00Z".to_string(),
            },
        )
        .unwrap();
        assert_eq!(deployment.state, DeploymentState::Verifying);
        assert!(!deployment.state.is_terminal());
        apply(
            &mut deployment,
            DeploymentEvent::VerificationPassed {
                at: "2026-11-21T10:05:00Z".to_string(),
            },
        )
        .unwrap();
        assert_eq!(deployment.state, DeploymentState::Succeeded);
        assert!(deployment.state.is_terminal());
    }

    /// 3.4：不可逆迁移不标为已回退，必须给前向修复方案。
    #[test]
    fn irreversible_migration_is_not_marked_rolled_back() {
        let mut plan = RollbackPlan::new("dep-1");
        plan.push(ReleasedResource::reversible(
            "api-image",
            ResourceKind::ApplicationImage,
        ));
        plan.push(ReleasedResource::irreversible(
            "migration-014",
            ResourceKind::DatabaseMigration,
            "写一份补偿迁移回填默认值，再发布新修订",
        ));
        plan.steps.push("回滚镜像到上一版".to_string());

        let verdict = plan.verdict();
        assert_eq!(verdict.restorable, vec!["api-image".to_string()]);
        assert_eq!(verdict.forward_fix_required.len(), 1);
        assert_eq!(verdict.forward_fix_required[0].resource, "migration-014");
        assert!(verdict.forward_fix_required[0].fix.contains("补偿迁移"));
        assert!(!verdict.claims_full_restore());
        assert_eq!(verdict.claim, RestoreClaim::Partial);
        assert!(verdict.summary().contains("需前向修复"));
        assert!(
            !verdict.restorable.contains(&"migration-014".to_string()),
            "不得声称已恢复不可逆资源"
        );
    }

    /// 3.4：外部消息与数据写入天生不可撤回；未声明处置也算不能回退。
    #[test]
    fn undeclared_resource_blocks_a_full_restore_claim() {
        let mut plan = RollbackPlan::new("dep-2");
        plan.push(ReleasedResource::reversible(
            "api-image",
            ResourceKind::ApplicationImage,
        ));
        plan.push(ReleasedResource::reversible(
            "notify-customer",
            ResourceKind::ExternalMessage,
        ));
        let verdict = plan.verdict();
        assert_eq!(verdict.undeclared, vec!["notify-customer".to_string()]);
        assert!(!verdict.claims_full_restore());

        let mut declared = RollbackPlan::new("dep-3");
        declared.push(ReleasedResource::irreversible(
            "notify-customer",
            ResourceKind::ExternalMessage,
            "向客户补发更正通知并记录更正单",
        ));
        let verdict = declared.verdict();
        assert!(verdict.undeclared.is_empty());
        assert_eq!(verdict.forward_fix_required.len(), 1);
        assert!(!verdict.claims_full_restore(), "补发通知不等于撤回消息");
    }

    /// 3.4：自动回滚必须经验证与授权，默认关闭。
    #[test]
    fn auto_rollback_requires_verified_and_authorized_policy() {
        let manual = RollbackPolicy::manual();
        assert!(!may_auto_rollback(&manual));

        let unverified = RollbackPolicy {
            auto: true,
            verified: false,
            authorized: true,
            authorized_by: Some("lead-1".to_string()),
        };
        assert!(!may_auto_rollback(&unverified));

        let unauthorized = RollbackPolicy {
            auto: true,
            verified: true,
            authorized: false,
            authorized_by: None,
        };
        assert!(!may_auto_rollback(&unauthorized));

        let ok = RollbackPolicy {
            auto: true,
            verified: true,
            authorized: true,
            authorized_by: Some("lead-1".to_string()),
        };
        assert!(may_auto_rollback(&ok));
    }

    /// 3.5：预算解释按 限额 − 已结算 − 预留 − 未知风险 算可用额度。
    #[test]
    fn budget_explanation_subtracts_reserved_and_unknown_risk() {
        let explanation = BudgetExplanation {
            limit_minor: 10_000,
            settled_minor: 4_000,
            reserved_minor: 4_000,
            unknown_risk_minor: 1_500,
            metering_source: MeteringSource::Invocation,
            coverage_gaps: Vec::new(),
        };
        assert_eq!(explanation.available(), 500);
        assert!(explanation.consistent());
        assert!(explanation.gaps().is_empty());

        let estimated = BudgetExplanation {
            metering_source: MeteringSource::Estimate,
            ..explanation.clone()
        };
        assert!(estimated.gaps()[0].contains("未按实际 invocation 记账"));

        let over = BudgetExplanation {
            unknown_risk_minor: 2_000,
            ..explanation
        };
        assert_eq!(over.available(), 0);
        assert!(over.consistent(), "为 0 仍自洽，只是没钱了");
    }

    /// 3.5：预算耗尽阻止新发布，解释只读，不动已结算与账本。
    #[test]
    fn exhausted_budget_blocks_release_without_touching_ledger() {
        let plan = plan();
        let approval = approval(&plan);
        let exhausted = BudgetExplanation {
            limit_minor: 10_000,
            settled_minor: 6_000,
            reserved_minor: 3_000,
            unknown_risk_minor: 1_000,
            metering_source: MeteringSource::Invocation,
            coverage_gaps: vec!["工具费用未计量".to_string()],
        };
        assert_eq!(exhausted.available(), 0);
        let before = exhausted.clone();

        match admit_release(&plan, &approval, "2026-11-21T10:00:00Z", &exhausted, &[]) {
            ReleaseAdmission::Refused { reason, detail } => {
                assert_eq!(reason, ReleaseBlockReason::BudgetExhausted);
                assert!(detail.contains("可用额度 0"));
                assert!(detail.contains("提额"), "要给出批准入口");
            }
            other => panic!("预算耗尽应拒绝，实际 {other:?}"),
        }
        assert_eq!(exhausted, before, "解释只读");
        assert_eq!(exhausted.settled_minor, 6_000, "历史账本不变");
        assert!(
            exhausted.gaps()[0].contains("工具费用未计量"),
            "覆盖缺口照实显示"
        );
    }
}
