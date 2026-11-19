//! P0 D1 领域值类型：Company/Project/Agent/Issue、资源外壳、权限与稳定原因码。
//! 纯数据 + 不变量校验，不依赖 DB/Web/Tauri（G-1）。

use crate::{DomainId, Version};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    pub id: DomainId,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryContext {
    pub provider: String,
    pub repository_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: DomainId,
    pub company_id: DomainId,
    pub name: String,
    pub repository: Option<RepositoryContext>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: DomainId,
    pub company_id: DomainId,
    pub display_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind {
    Coding,
    General,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub company_id: DomainId,
    pub project_id: Option<DomainId>,
    pub issue_id: Option<DomainId>,
}

impl Scope {
    /// 请求范围必须被授权范围覆盖；未指定的层级视为更宽范围。
    pub fn contains(&self, requested: &Scope) -> bool {
        if self.company_id != requested.company_id {
            return false;
        }
        let project_ok = match (&self.project_id, &requested.project_id) {
            (Some(_), Some(r)) => self.project_id.as_ref() == Some(r),
            (None, Some(_)) => true,
            (_, None) => true,
        };
        let issue_ok = match (&self.issue_id, &requested.issue_id) {
            (Some(_), Some(r)) => self.issue_id.as_ref() == Some(r),
            (None, Some(_)) => true,
            (_, None) => true,
        };
        project_ok && issue_ok
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    User,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub id: DomainId,
    pub kind: PrincipalKind,
    /// 自报角色称号仅用于展示，永远不作为授权依据。
    pub claimed_roles: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    Dispatch,
    ManageBudget,
    Accept,
    ManageScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub principal_id: DomainId,
    pub scope: Scope,
    pub permission: Permission,
    pub revision: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnerKind {
    Agent,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub kind: OwnerKind,
    pub id: DomainId,
}

/// 任务级唯一 accountable owner；贡献者不改变责任归属。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub owner: Owner,
    pub scope: Scope,
    pub grant_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcceptanceCriteria {
    pub criteria: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSpec {
    pub title: String,
    pub assignment: Option<Assignment>,
    pub acceptance: AcceptanceCriteria,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    Pending,
    Running,
    Blocked,
    Succeeded,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConditionReason {
    AwaitingApproval,
    NoEligibleAgent,
    InsufficientBudget,
    DependencyNotAccepted,
    ExternalOutcomeUnknown,
    Progressing,
    Recovered,
}

impl ConditionReason {
    /// 合法稳定停止原因：不产生热重试，等待显式解除条件。
    pub fn is_stable_stop(self) -> bool {
        matches!(
            self,
            Self::AwaitingApproval
                | Self::NoEligibleAgent
                | Self::InsufficientBudget
                | Self::DependencyNotAccepted
                | Self::ExternalOutcomeUnknown
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub reason: ConditionReason,
    pub status: ConditionStatus,
    /// 面向人读；权限与调度逻辑不得解析此字段。
    pub message: String,
    pub observed_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceStatus {
    pub phase: Phase,
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceMeta {
    pub uid: DomainId,
    pub company_id: DomainId,
    pub project_id: Option<DomainId>,
    pub resource_version: Version,
    pub generation: u64,
    pub deletion_requested_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub meta: ResourceMeta,
    pub kind: IssueKind,
    pub repository: Option<RepositoryContext>,
    pub spec: IssueSpec,
    pub status: ResourceStatus,
}

impl Issue {
    /// Coding Issue 必须具备项目与仓库上下文（§identity-access 默认 Company 与归属）。
    pub fn context_error(&self) -> Option<DomainError> {
        if self.kind == IssueKind::Coding
            && (self.meta.project_id.is_none() || self.repository.is_none())
        {
            Some(DomainError::MissingProjectContext)
        } else {
            None
        }
    }

    pub fn scope(&self) -> Scope {
        Scope {
            company_id: self.meta.company_id.clone(),
            project_id: self.meta.project_id.clone(),
            issue_id: Some(self.meta.uid.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriterKind {
    DomainCommand,
    Scheduler,
    RunController,
    Verifier,
    Acceptance,
    Budget,
    Executor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    MissingProjectContext,
    MissingAssignment,
    InsufficientPermission,
    VersionConflict { current: Version },
    ImmutableDuringRun,
    WrongWriter,
    ScopeMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DenialReason {
    InsufficientPermission,
    MissingProjectContext,
    MissingAssignment,
    VersionConflict,
    ImmutableDuringRun,
    WrongWriter,
    ScopeMismatch,
    /// 预算不足：稳定阻塞，不得周期性热重试（§9.12、D2）。
    InsufficientBudget,
    /// 外部动作结果未知：冻结盲重试，先对账（§10.4、D2）。
    ExternalOutcomeUnknown,
}

/// 稳定拒绝原因 + 审计字段：实际主体、依据修订、重新激活条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub reason: DenialReason,
    pub actor: DomainId,
    /// Box 避免 Result 的 Err 分支过大（clippy::result_large_err）。
    pub scope: Box<Scope>,
    pub grant_revision: Option<u64>,
    pub reactivation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub actor: DomainId,
    pub scope: Scope,
    pub grant_revision: u64,
}

/// 执行端观察信封：只写 status，不直接改变 spec 或 Attempt 状态权威。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub phase: Phase,
    pub reason: ConditionReason,
    pub message: String,
    pub source_version: String,
    pub observed_at: String,
}

/// 控制字段变更：只允许 desiredState / 有授权的资源限制 / 注释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlChange {
    pub title: Option<String>,
    pub acceptance: Option<AcceptanceCriteria>,
}

impl ControlChange {
    pub fn touches_immutable_spec(&self) -> bool {
        self.acceptance.is_some()
    }
}
