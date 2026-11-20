//! P2 任务 3.2：员工独立继续与执行/管理身份分离。
//!
//! 权威语义：V2.9.6 §19.9（切换前已提交的合法分工与派发意图继续；旧经理的执行结果可在其仍有效的
//! 执行身份下登记，撤任不会自动使员工反馈认证失败）、§19.10（撤销管理权不等于终止进程；旧经理只做
//! 纯管理时不必因换人杀掉工作进程，兼任编码时可显式保留其执行 Assignment 而只移除管理能力；运行时
//! 无法分离凭据/工具权限时停止该会话并按现场交接重建）、§31.10（管理凭据不能作为员工执行凭据，
//! 执行凭据独立绑定 invocation/Assignment）、§41.8（真实项目 A→B 切换：员工进度继续、已接受成果
//! 不重做、费用风险保留）、V15-D05（换经理默认不换员工）。
//!
//! 本模块的边界：
//! - 只做「切换时现有执行与历史记录的处置投影」，不实现切换事务（3.3）、不改历史接受与费用主体
//!   （3.4）、不做专项委任边界（3.5）。这里没有停止执行、重发命令、改派、重做或写接受的代码路径。
//! - `Directive` 里没有 `Cancel`/`Restart`/`Redo` 变体：切换本身不能停止员工工作、不能重跑已运行
//!   的执行、不能重做已接受的成果。这是类型事实，不是运行时开关或配置。
//! - 只读投影，不建表（沿用 2.5 的做法）：迁移仍 10 条、表仍 44 张，事实来源仍是 issues / runs /
//!   invocations / artifacts / artifact_revisions / acceptances。
//! - 只支持项目作用域。专项子树的展开需要 Issue 层级，属 3.5；越界时显式拒绝而不是近似成项目。
//! - 「可分离」只能由宿主声明（`SeatSeparability`），默认 `Undeclared` 且**不**当作可分离；模块里
//!   没有任何从 Agent 自述或角色称号推导可分离性的路径。

use std::collections::BTreeMap;
use std::fmt;

use crate::domain::execution::{ExecutionState, InvocationRecord};
use crate::domain::model::Issue;
use crate::manager_transfer::{
    AppointmentStatus, ManagementAppointment, ManagementScope, ManagementTicket,
};
use crate::store::{SqliteStore, StoreError};

/// 投影 schema：与委任、交接包、接管意图各自版本化。
pub const CONTINUATION_SCHEMA: &str = "bod.staff-continuation/v1";

// ---------------------------------------------------------------------------
// 身份：执行 / 管理
// ---------------------------------------------------------------------------

/// 一个 Agent 在某个时刻以哪种身份行动。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActingRole {
    Execution,
    Management,
}

impl ActingRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::Management => "management",
        }
    }
}

/// 执行座位的来源。**只是来源记录**：撤任不会改座位，也没有「按来源判定有效性」的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatOrigin {
    /// 直接指派，与任何委任无关。
    Standalone,
    /// 上任时由该委任安排（仅作来源；委任撤任后座位照旧有效）。
    InheritedFromAppointment {
        appointment_id: String,
        authority_epoch: u64,
    },
}

impl SeatOrigin {
    pub fn describe(&self) -> String {
        match self {
            Self::Standalone => "直接指派".to_string(),
            Self::InheritedFromAppointment {
                appointment_id,
                authority_epoch,
            } => format!("由委任 {appointment_id}（epoch {authority_epoch}）安排"),
        }
    }
}

/// 运行时能否把「管理权限」与「执行权限」分开（§19.10）。
///
/// 只有宿主显式声明 `Separable` 才认为可分离；`Undeclared` 与 `NotSeparable` 一律按不可分离处理，
/// 因为「没声明」不能当成「能分开」——否则等于拿缺省值冒充能力证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatSeparability {
    Separable,
    Undeclared,
    NotSeparable,
}

impl SeatSeparability {
    pub const fn is_separable(self) -> bool {
        matches!(self, Self::Separable)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Separable => "separable",
            Self::Undeclared => "undeclared",
            Self::NotSeparable => "not_separable",
        }
    }

    pub const fn describe(self) -> &'static str {
        match self {
            Self::Separable => "可分离：保留其执行 Assignment，只移除管理能力",
            Self::Undeclared => "未声明可分离：不得据切换保留其执行会话",
            Self::NotSeparable => "不可分离：停止该会话，按现场交接重建",
        }
    }
}

/// 执行座位：某 Agent 在某 Issue 上的执行归属（§31.10「执行凭据独立绑定 invocation/Assignment」）。
///
/// 唯一构造入口是 [`ExecutionSeat::from_issue`]，签名里没有 Appointment 参数——座位不能从管理
/// 委任「推导」出来，只能从 Issue 的 Assignment 读出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionSeat {
    pub seat_id: String,
    pub agent_id: String,
    pub issue_uid: String,
    pub scope_key: String,
    pub grant_revision: u64,
    pub origin: SeatOrigin,
    pub separability: SeatSeparability,
}

impl ExecutionSeat {
    /// 从 Issue 的 Assignment 读出执行座位；未指派时返回 `None`（不是错误，也不是空座位）。
    pub fn from_issue(
        scope: &ManagementScope,
        issue: &Issue,
    ) -> Result<Option<Self>, ContinuityError> {
        let project_id = scope.project_id();
        let issue_project = issue
            .meta
            .project_id
            .as_ref()
            .map(|value| value.as_str().to_string());
        if issue_project.as_deref() != Some(project_id.as_str()) {
            return Err(ContinuityError::ScopeMismatch {
                issue_uid: issue.meta.uid.as_str().to_string(),
                expected_project: project_id,
            });
        }
        let Some(assignment) = issue.spec.assignment.as_ref() else {
            return Ok(None);
        };
        let agent_id = assignment.owner.id.as_str().to_string();
        if agent_id.trim().is_empty() {
            return Err(ContinuityError::EmptyField("assignment.owner.id"));
        }
        Ok(Some(Self {
            seat_id: format!("seat:{}#{agent_id}", issue.meta.uid.as_str()),
            agent_id,
            issue_uid: issue.meta.uid.as_str().to_string(),
            scope_key: scope.scope_key(),
            grant_revision: assignment.grant_revision,
            origin: SeatOrigin::Standalone,
            separability: SeatSeparability::Undeclared,
        }))
    }

    /// 只登记来源：把座位标成「由该委任安排」，不改变任何有效性判定。
    pub fn with_inherited_origin(mut self, appointment: &ManagementAppointment) -> Self {
        self.origin = SeatOrigin::InheritedFromAppointment {
            appointment_id: appointment.appointment_id.clone(),
            authority_epoch: appointment.authority_epoch,
        };
        self
    }

    /// 宿主声明该座位的运行时是否可分离（§19.10）。Agent 无从设置这个字段。
    pub fn with_separability(mut self, separability: SeatSeparability) -> Self {
        self.separability = separability;
        self
    }
}

/// 宿主对某个座位的声明（可分离性、来源）。
///
/// 声明不来自数据库里的座位行（那样会变成「谁写库谁能给自己发能力」），而由宿主在装载现场时
/// 按适配器能力登记传入；没有声明就是 `Undeclared`，不当作可分离。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatDeclaration {
    pub issue_uid: String,
    pub agent_id: String,
    pub separability: SeatSeparability,
    pub inherited_from: Option<(String, u64)>,
}

impl SeatDeclaration {
    pub fn new(
        issue_uid: impl Into<String>,
        agent_id: impl Into<String>,
        separability: SeatSeparability,
    ) -> Self {
        Self {
            issue_uid: issue_uid.into(),
            agent_id: agent_id.into(),
            separability,
            inherited_from: None,
        }
    }

    /// 只登记来源，不改变有效性判定。
    pub fn inherited_from(mut self, appointment_id: impl Into<String>, epoch: u64) -> Self {
        self.inherited_from = Some((appointment_id.into(), epoch));
        self
    }
}

/// 执行凭据：绑定 invocation/Assignment 的短时凭据（§31.10）。
///
/// 只有 [`ExecutionCredential::for_invocation`] 一个构造入口，它要求真实的 seat + invocation 记录；
/// 管理票证没有任何路径能变成执行凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionCredential {
    pub seat_id: String,
    pub agent_id: String,
    pub company_id: String,
    pub issue_uid: String,
    pub attempt_id: String,
    pub fencing_token: u64,
    pub issued_at: String,
    pub not_after: String,
}

impl ExecutionCredential {
    /// 由座位与已持久化的 invocation 记录签发（fencing token 取自记录，不由调用方指定）。
    pub fn for_invocation(
        seat: &ExecutionSeat,
        invocation: &InvocationRecord,
        issued_at: &str,
        not_after: &str,
    ) -> Result<Self, ContinuityError> {
        if invocation.attempt_id.trim().is_empty() {
            return Err(ContinuityError::EmptyField("invocation.attempt_id"));
        }
        if invocation.company_id.trim().is_empty() {
            return Err(ContinuityError::EmptyField("invocation.company_id"));
        }
        let credential = Self {
            seat_id: seat.seat_id.clone(),
            agent_id: seat.agent_id.clone(),
            company_id: invocation.company_id.clone(),
            issue_uid: seat.issue_uid.clone(),
            attempt_id: invocation.attempt_id.clone(),
            fencing_token: invocation.fencing_token,
            issued_at: issued_at.to_string(),
            not_after: not_after.to_string(),
        };
        credential.validate()?;
        Ok(credential)
    }

    pub fn validate(&self) -> Result<(), ContinuityError> {
        for (field, value) in [
            ("credential.seat_id", self.seat_id.as_str()),
            ("credential.agent_id", self.agent_id.as_str()),
            ("credential.company_id", self.company_id.as_str()),
            ("credential.issue_uid", self.issue_uid.as_str()),
            ("credential.attempt_id", self.attempt_id.as_str()),
            ("credential.issued_at", self.issued_at.as_str()),
            ("credential.not_after", self.not_after.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ContinuityError::EmptyField(field));
            }
        }
        if !earlier(&self.issued_at, &self.not_after) {
            return Err(ContinuityError::CredentialNotPositive {
                issued_at: self.issued_at.clone(),
                not_after: self.not_after.clone(),
            });
        }
        Ok(())
    }

    pub const fn role(&self) -> ActingRole {
        ActingRole::Execution
    }

    /// 执行凭据不带管理命令集，也没有 epoch/audience 字段：它不能发管理命令。
    pub const fn can_issue_management_command(&self) -> bool {
        false
    }

    /// 反向顶替同样不存在：执行凭据构造不出管理票证。
    pub fn as_management_ticket(&self) -> Result<ManagementTicket, ContinuityError> {
        Err(ContinuityError::ExecutionIsNotManagement {
            seat_id: self.seat_id.clone(),
        })
    }
}

/// 管理票证不能当执行凭据（§31.10）。这里没有分支能拼出执行凭据，直接如实拒绝。
pub fn management_ticket_as_execution(
    ticket: &ManagementTicket,
) -> Result<ExecutionCredential, ContinuityError> {
    Err(ContinuityError::ManagementIsNotExecution {
        appointment_id: ticket.appointment_id.clone(),
        authority_epoch: ticket.authority_epoch,
        can_act_as_execution: ticket.can_act_as_execution(),
    })
}

// ---------------------------------------------------------------------------
// 现场投影
// ---------------------------------------------------------------------------

/// 运行中的工作：按执行事实（invocation）列出，不按「谁安排的」列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningWork {
    pub run_id: String,
    pub invocation_id: String,
    pub issue_uid: String,
    pub executor: Option<String>,
    pub state: ExecutionState,
}

/// 需要先处理、切换不能代劳的工作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionWork {
    pub run_id: String,
    pub invocation_id: String,
    pub issue_uid: String,
    pub state: ExecutionState,
    pub remedy: Remedy,
}

/// 切换前必须由谁处理的补救方向；指向已有模块，不在这里重新实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// Unknown 副作用：先按 invocation 对账（2.3 / P0 对账）。
    ReconcileEffect,
    /// 停止中：等停止确认（2.2），不能凭切换当作已停。
    ConfirmStop,
}

impl Remedy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReconcileEffect => "reconcile_effect",
            Self::ConfirmStop => "confirm_stop",
        }
    }
}

/// 已接受成果：保留、不重做。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedWork {
    pub artifact_id: String,
    pub revision_id: String,
    pub acceptance_id: String,
    pub decided_by: String,
    pub accepted_at_ms: i64,
}

/// 执行贡献：某修订由谁以执行身份产出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution {
    pub revision_id: String,
    pub artifact_id: String,
    pub produced_by: String,
}

/// 切换时的现场投影：全部来自已持久化事实，构造后不再读库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuityState {
    pub company_id: String,
    pub scope_key: String,
    pub seats: Vec<ExecutionSeat>,
    pub running: Vec<RunningWork>,
    pub attention: Vec<AttentionWork>,
    pub accepted: Vec<AcceptedWork>,
    pub contributions: Vec<Contribution>,
    /// 有接受记录但不批准当前修订的（已被取代 / 待人工 / 指针已移走）：单列，不并入保留也不丢弃。
    pub stale_acceptances: Vec<String>,
}

impl ContinuityState {
    /// 从存储读出现场，不带任何座位声明（全部座位按 `Undeclared` 处理）。
    pub fn load(
        store: &SqliteStore,
        company_id: &str,
        scope: &ManagementScope,
    ) -> Result<Self, ContinuityError> {
        Self::load_with_declarations(store, company_id, scope, &[])
    }

    /// 从存储读出现场，并叠加宿主声明的座位能力。
    ///
    /// 声明必须命中真实存在的座位（否则 `UnknownSeatDeclaration`），也不允许同一座位声明两次
    /// （`DuplicateSeatDeclaration`）——不静默忽略，避免「写错了等于没声明」和「后写覆盖前写」。
    pub fn load_with_declarations(
        store: &SqliteStore,
        company_id: &str,
        scope: &ManagementScope,
        declarations: &[SeatDeclaration],
    ) -> Result<Self, ContinuityError> {
        if company_id.trim().is_empty() {
            return Err(ContinuityError::EmptyField("company_id"));
        }
        let project_id = match scope {
            ManagementScope::Project { project_id } => project_id.clone(),
            ManagementScope::IssueSubtree { root_issue_id, .. } => {
                return Err(ContinuityError::UnsupportedScope {
                    scope_key: scope.scope_key(),
                    detail: format!(
                        "专项子树 {root_issue_id} 的展开需要 Issue 层级，属 3.5 边界，本切片不做近似"
                    ),
                });
            }
        };

        let issues = store.list_issues_for_project(company_id, &project_id)?;
        let mut owners: BTreeMap<String, String> = BTreeMap::new();
        let mut seats = Vec::new();
        for issue in &issues {
            let issue_uid = issue.meta.uid.as_str().to_string();
            if let Some(assignment) = issue.spec.assignment.as_ref() {
                owners.insert(issue_uid.clone(), assignment.owner.id.as_str().to_string());
            }
            if let Some(seat) = ExecutionSeat::from_issue(scope, issue)? {
                seats.push(seat);
            }
        }
        seats.sort_by(|left, right| left.seat_id.cmp(&right.seat_id));
        apply_declarations(&mut seats, declarations)?;

        let run_issue: BTreeMap<String, String> = store
            .list_project_run_issues(company_id, &project_id)?
            .into_iter()
            .collect();

        let mut running = Vec::new();
        let mut attention = Vec::new();
        for invocation in store.list_invocations(company_id)? {
            let Some(issue_uid) = run_issue.get(&invocation.run_id) else {
                continue; // 不属于本项目：不并入本作用域投影。
            };
            let executor = owners.get(issue_uid).cloned();
            match classify(invocation.state) {
                WorkClass::InFlight => running.push(RunningWork {
                    run_id: invocation.run_id.clone(),
                    invocation_id: invocation.invocation_id.clone(),
                    issue_uid: issue_uid.clone(),
                    executor,
                    state: invocation.state,
                }),
                WorkClass::Stopping => attention.push(AttentionWork {
                    run_id: invocation.run_id.clone(),
                    invocation_id: invocation.invocation_id.clone(),
                    issue_uid: issue_uid.clone(),
                    state: invocation.state,
                    remedy: Remedy::ConfirmStop,
                }),
                WorkClass::Unreconciled => attention.push(AttentionWork {
                    run_id: invocation.run_id.clone(),
                    invocation_id: invocation.invocation_id.clone(),
                    issue_uid: issue_uid.clone(),
                    state: invocation.state,
                    remedy: Remedy::ReconcileEffect,
                }),
                WorkClass::Settled => {}
            }
        }
        running.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        attention.sort_by(|left, right| left.run_id.cmp(&right.run_id));

        let artifacts: BTreeMap<String, Option<String>> = store
            .list_artifacts(company_id, &project_id)?
            .into_iter()
            .map(|artifact| {
                (
                    artifact.id.as_str().to_string(),
                    artifact
                        .accepted_revision
                        .as_ref()
                        .map(|value| value.as_str().to_string()),
                )
            })
            .collect();

        let mut accepted = Vec::new();
        let mut stale_acceptances = Vec::new();
        for record in store.list_acceptances_for_company(company_id)? {
            let artifact_id = record.artifact_id.as_str().to_string();
            let Some(accepted_revision) = artifacts.get(&artifact_id) else {
                continue; // 不属于本项目。
            };
            let revision_id = record.binding.artifact_revision.as_str().to_string();
            let approves_current = accepted_revision.as_deref() == Some(revision_id.as_str());
            if record.superseded_by.is_some() || !record.decision.is_accepted() || !approves_current
            {
                stale_acceptances.push(record.acceptance_id.as_str().to_string());
                continue;
            }
            accepted.push(AcceptedWork {
                artifact_id,
                revision_id,
                acceptance_id: record.acceptance_id.as_str().to_string(),
                decided_by: record.actor.as_str().to_string(),
                accepted_at_ms: record.accepted_at_ms,
            });
        }
        accepted.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        stale_acceptances.sort();

        let producers: BTreeMap<String, (String, String)> = store
            .list_project_revision_producers(company_id, &project_id)?
            .into_iter()
            .map(|(revision_id, artifact_id, produced_by)| {
                (revision_id, (artifact_id, produced_by))
            })
            .collect();

        let mut contributions = BTreeMap::new();
        for item in &accepted {
            let Some((artifact_id, produced_by)) = producers.get(&item.revision_id) else {
                return Err(ContinuityError::CorruptAcceptance {
                    acceptance_id: item.acceptance_id.clone(),
                    revision_id: item.revision_id.clone(),
                });
            };
            contributions.insert(
                item.revision_id.clone(),
                Contribution {
                    revision_id: item.revision_id.clone(),
                    artifact_id: artifact_id.clone(),
                    produced_by: produced_by.clone(),
                },
            );
        }

        Ok(Self {
            company_id: company_id.to_string(),
            scope_key: scope.scope_key(),
            seats,
            running,
            attention,
            accepted,
            contributions: contributions.into_values().collect(),
            stale_acceptances,
        })
    }

    /// 某 Agent 当前持有哪些身份。管理身份只看当前委任，执行身份只看执行事实。
    pub fn role_of(
        &self,
        agent_id: &str,
        appointment: Option<&ManagementAppointment>,
    ) -> RoleAssignment {
        let execution_seats = self
            .seats
            .iter()
            .filter(|seat| seat.agent_id == agent_id)
            .map(|seat| seat.seat_id.clone())
            .collect();
        let management = appointment
            .filter(|item| item.manager_agent_id == agent_id)
            .map(|item| ManagementRole {
                appointment_id: item.appointment_id.clone(),
                authority_epoch: item.authority_epoch,
                status: item.status,
            });
        RoleAssignment {
            agent_id: agent_id.to_string(),
            execution_seats,
            management,
        }
    }

    fn seat_of(&self, issue_uid: &str, agent_id: &str) -> Option<&ExecutionSeat> {
        self.seats
            .iter()
            .find(|seat| seat.issue_uid == issue_uid && seat.agent_id == agent_id)
    }
}

/// 一个 Agent 的身份集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAssignment {
    pub agent_id: String,
    pub execution_seats: Vec<String>,
    pub management: Option<ManagementRole>,
}

/// 管理身份：绑定当前委任与 epoch。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementRole {
    pub appointment_id: String,
    pub authority_epoch: u64,
    pub status: AppointmentStatus,
}

impl RoleAssignment {
    pub fn holds_execution(&self) -> bool {
        !self.execution_seats.is_empty()
    }

    /// 只有 `Active` 算持有在任管理权：暂停/撤任/过期都不算（3.1 每条命令重查状态）。
    pub fn holds_management(&self) -> bool {
        self.management
            .as_ref()
            .is_some_and(|role| role.status == AppointmentStatus::Active)
    }
}

// ---------------------------------------------------------------------------
// 切换指令
// ---------------------------------------------------------------------------

/// 切换对该项工作的处置。没有 `Cancel`/`Restart`/`Redo`：切换不停止、不重跑、不重做。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Directive {
    /// 继续执行：沿用原 Attempt 与 invocation，不重启、不重新派发。
    Continue,
    /// 保留已接受成果：不重做，不要求重验（重验须走 3.4 的有理由提案）。
    Preserve,
    /// 保留历史贡献：撤任不删除旧经理的代码、报告与决定。
    Retain,
    /// 旧经理执行座不可分离：停止该会话并按现场交接重建（2.2 隔离 + 2.4 预检 + 2.5 卡片）。
    RebuildViaHandoff,
    /// 切换不能代劳：先对账或等停止确认。
    ResolveBeforeSwitch,
}

impl Directive {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Preserve => "preserve",
            Self::Retain => "retain",
            Self::RebuildViaHandoff => "rebuild_via_handoff",
            Self::ResolveBeforeSwitch => "resolve_before_switch",
        }
    }

    /// 全量变体：用例用它固定「没有停止/重做这一类指令」。
    pub const ALL: [Self; 5] = [
        Self::Continue,
        Self::Preserve,
        Self::Retain,
        Self::RebuildViaHandoff,
        Self::ResolveBeforeSwitch,
    ];
}

/// 指令归属：员工（换谁走都不变）、旧经理本人、历史记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanParty {
    Employee,
    OutgoingManager,
    History,
}

impl PlanParty {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Employee => "employee",
            Self::OutgoingManager => "outgoing_manager",
            Self::History => "history",
        }
    }
}

/// 指令对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedSubject {
    Running {
        run_id: String,
        invocation_id: String,
        issue_uid: String,
    },
    Accepted {
        artifact_id: String,
        revision_id: String,
        acceptance_id: String,
    },
    Contribution {
        revision_id: String,
        artifact_id: String,
    },
    Attention {
        run_id: String,
        invocation_id: String,
        state: ExecutionState,
    },
}

impl PlannedSubject {
    pub fn sort_key(&self) -> String {
        match self {
            Self::Running { run_id, .. } => format!("a:{run_id}"),
            Self::Attention { run_id, .. } => format!("b:{run_id}"),
            Self::Accepted { artifact_id, .. } => format!("c:{artifact_id}"),
            Self::Contribution { revision_id, .. } => format!("d:{revision_id}"),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Running {
                run_id,
                invocation_id,
                issue_uid,
            } => format!("运行中的 {run_id}（invocation {invocation_id}，Issue {issue_uid}）"),
            Self::Accepted {
                artifact_id,
                revision_id,
                acceptance_id,
            } => format!("已接受 {artifact_id}@{revision_id}（接受 {acceptance_id}）"),
            Self::Contribution {
                revision_id,
                artifact_id,
            } => format!("历史贡献 {artifact_id}@{revision_id}"),
            Self::Attention {
                run_id,
                invocation_id,
                state,
            } => format!(
                "待人处理 {run_id}（invocation {invocation_id}，状态 {})",
                state.as_str()
            ),
        }
    }
}

/// 一条指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedItem {
    pub party: PlanParty,
    pub subject: PlannedSubject,
    pub directive: Directive,
    pub note: String,
}

/// 切换指令集合：`plan()` 唯一产出，没有「停止」「重发」「重做」的构造入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchPlan {
    pub items: Vec<PlannedItem>,
}

impl SwitchPlan {
    /// 对运行中的 Run 取指令。
    pub fn directive_for_run(&self, run_id: &str) -> Option<Directive> {
        self.items.iter().find_map(|item| match &item.subject {
            PlannedSubject::Running { run_id: id, .. }
            | PlannedSubject::Attention { run_id: id, .. }
                if id == run_id =>
            {
                Some(item.directive)
            }
            _ => None,
        })
    }

    /// 对某个 Artifact 取指令。
    pub fn directive_for_artifact(&self, artifact_id: &str) -> Option<Directive> {
        self.items.iter().find_map(|item| match &item.subject {
            PlannedSubject::Accepted {
                artifact_id: id, ..
            } if id == artifact_id => Some(item.directive),
            _ => None,
        })
    }

    /// 只出现在员工名下的指令；换谁走都不受这些项影响。
    pub fn employee_items(&self) -> Vec<&PlannedItem> {
        self.items
            .iter()
            .filter(|item| item.party == PlanParty::Employee)
            .collect()
    }

    pub fn counts(&self) -> PlanCounts {
        let mut counts = PlanCounts::default();
        for item in &self.items {
            match item.directive {
                Directive::Continue => counts.continue_runs += 1,
                Directive::Preserve => counts.preserved += 1,
                Directive::Retain => counts.retained += 1,
                Directive::RebuildViaHandoff => counts.rebuild_via_handoff += 1,
                Directive::ResolveBeforeSwitch => counts.attention += 1,
            }
        }
        counts
    }

    /// 切换期请求重做已接受成果：已接受就不许（换人不是重做已接受成果的理由）。
    pub fn may_redo(&self, artifact_id: &str) -> Result<(), ContinuityError> {
        for item in &self.items {
            if let PlannedSubject::Accepted {
                artifact_id: id,
                revision_id,
                acceptance_id,
            } = &item.subject
                && id == artifact_id
            {
                return Err(ContinuityError::RedoAccepted {
                    artifact_id: id.clone(),
                    revision_id: revision_id.clone(),
                    acceptance_id: acceptance_id.clone(),
                });
            }
        }
        Ok(())
    }

    /// 切换期请求重启：运行中与未知副作用都不许（后者要先对账，否则可能双重执行）。
    pub fn may_restart(&self, run_id: &str) -> Result<(), ContinuityError> {
        for item in &self.items {
            match &item.subject {
                PlannedSubject::Running {
                    run_id: id,
                    invocation_id,
                    ..
                } if id == run_id => {
                    return Err(ContinuityError::RestartRunning {
                        run_id: id.clone(),
                        invocation_id: invocation_id.clone(),
                    });
                }
                PlannedSubject::Attention {
                    run_id: id,
                    invocation_id,
                    state,
                } if id == run_id => {
                    return Err(ContinuityError::RestartBlocked {
                        run_id: id.clone(),
                        invocation_id: invocation_id.clone(),
                        state: *state,
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 切换期改派执行者：运行中不许（改派等于停掉在跑的执行）。
    pub fn may_change_executor(&self, issue_uid: &str) -> Result<(), ContinuityError> {
        for item in &self.items {
            if let PlannedSubject::Running {
                issue_uid: id,
                run_id,
                ..
            } = &item.subject
                && id == issue_uid
            {
                return Err(ContinuityError::ExecutorLockedWhileRunning {
                    issue_uid: id.clone(),
                    run_id: run_id.clone(),
                });
            }
        }
        Ok(())
    }

    /// 交接报告：`from`/`to` 只写进报告头，不参与任何指令判定（判定只看现场事实）。
    pub fn handover(&self, from: &str, to: &str) -> Result<HandoverReport, ContinuityError> {
        if from.trim().is_empty() {
            return Err(ContinuityError::EmptyField("handover.from"));
        }
        if to.trim().is_empty() {
            return Err(ContinuityError::EmptyField("handover.to"));
        }
        if from == to {
            return Err(ContinuityError::SameManager {
                agent_id: from.to_string(),
            });
        }
        Ok(HandoverReport {
            from: from.to_string(),
            to: to.to_string(),
            counts: self.counts(),
            items: self.items.len(),
        })
    }
}

/// 计数摘要。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanCounts {
    pub continue_runs: usize,
    pub preserved: usize,
    pub retained: usize,
    pub rebuild_via_handoff: usize,
    pub attention: usize,
}

/// 报告头。只含计数与对照身份，不含任何“已停止/已重做”字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoverReport {
    pub from: String,
    pub to: String,
    pub counts: PlanCounts,
    pub items: usize,
}

/// 生成切换指令。
///
/// 规则只看现场事实 + 宿主声明的可分离性；`outgoing_manager` 只用于判断「这一项是不是旧经理
/// 本人以执行身份在跑」。员工名下的执行不因换人改变指令。
pub fn plan(
    state: &ContinuityState,
    outgoing_manager: &str,
) -> Result<SwitchPlan, ContinuityError> {
    if outgoing_manager.trim().is_empty() {
        return Err(ContinuityError::EmptyField("outgoing_manager"));
    }
    let mut items = Vec::new();

    for work in &state.running {
        let is_outgoing = work.executor.as_deref() == Some(outgoing_manager);
        let (party, directive, note) = if !is_outgoing {
            (
                PlanParty::Employee,
                Directive::Continue,
                match &work.executor {
                    Some(executor) => format!(
                        "员工 {executor} 的执行沿用原 Attempt 与 invocation，切换不停、不重派"
                    ),
                    None => "执行者未登记：按执行事实继续，不凭缺少登记停止他人的工作".to_string(),
                },
            )
        } else {
            match state.seat_of(&work.issue_uid, outgoing_manager) {
                Some(seat) if seat.separability.is_separable() => (
                    PlanParty::OutgoingManager,
                    Directive::Continue,
                    format!(
                        "座位 {} 声明可分离：保留执行 Assignment（{}），只移除管理能力",
                        seat.seat_id,
                        seat.separability.describe()
                    ),
                ),
                Some(seat) => (
                    PlanParty::OutgoingManager,
                    Directive::RebuildViaHandoff,
                    format!(
                        "旧经理以执行身份在跑（座位 {}，{}）：停止该会话并按现场交接重建，不做原地继续",
                        seat.seat_id,
                        seat.separability.describe()
                    ),
                ),
                None => (
                    PlanParty::OutgoingManager,
                    Directive::RebuildViaHandoff,
                    "旧经理以执行身份在跑但座位未登记：不得据切换保留，按现场交接重建".to_string(),
                ),
            }
        };
        items.push(PlannedItem {
            party,
            subject: PlannedSubject::Running {
                run_id: work.run_id.clone(),
                invocation_id: work.invocation_id.clone(),
                issue_uid: work.issue_uid.clone(),
            },
            directive,
            note,
        });
    }

    for work in &state.attention {
        items.push(PlannedItem {
            party: PlanParty::Employee,
            subject: PlannedSubject::Attention {
                run_id: work.run_id.clone(),
                invocation_id: work.invocation_id.clone(),
                state: work.state,
            },
            directive: Directive::ResolveBeforeSwitch,
            note: format!(
                "状态 {} 不能由切换代劳：先{}（{}），不自动重发或重跑",
                work.state.as_str(),
                match work.remedy {
                    Remedy::ReconcileEffect => "按 invocation 对账",
                    Remedy::ConfirmStop => "等停止确认",
                },
                work.remedy.as_str()
            ),
        });
    }

    for work in &state.accepted {
        items.push(PlannedItem {
            party: PlanParty::History,
            subject: PlannedSubject::Accepted {
                artifact_id: work.artifact_id.clone(),
                revision_id: work.revision_id.clone(),
                acceptance_id: work.acceptance_id.clone(),
            },
            directive: Directive::Preserve,
            note: format!(
                "由 {} 接受的 {}@{} 保留：不重做，不因换人自动重验",
                work.decided_by, work.artifact_id, work.revision_id
            ),
        });
    }

    for contribution in &state.contributions {
        // 旧经理本人的历史贡献单独标出：撤任不删除其代码、报告与决定。
        let party = if contribution.produced_by == outgoing_manager {
            PlanParty::OutgoingManager
        } else {
            PlanParty::History
        };
        items.push(PlannedItem {
            party,
            subject: PlannedSubject::Contribution {
                revision_id: contribution.revision_id.clone(),
                artifact_id: contribution.artifact_id.clone(),
            },
            directive: Directive::Retain,
            note: format!(
                "{} 以执行身份产出的 {}@{} 保留（撤任不撤销其执行身份登记）",
                contribution.produced_by, contribution.artifact_id, contribution.revision_id
            ),
        });
    }

    items.sort_by(|left, right| {
        (left.party, left.subject.sort_key()).cmp(&(right.party, right.subject.sort_key()))
    });
    Ok(SwitchPlan { items })
}

// ---------------------------------------------------------------------------
// 状态分类
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkClass {
    InFlight,
    Stopping,
    Unreconciled,
    Settled,
}

/// 执行状态 → 切换时的处置类别。11 个变体全部有归属，没有兜底分支。
fn classify(state: ExecutionState) -> WorkClass {
    match state {
        ExecutionState::Accepted
        | ExecutionState::Starting
        | ExecutionState::Running
        | ExecutionState::NeedsInput
        | ExecutionState::NeedsAuth
        | ExecutionState::Suspended => WorkClass::InFlight,
        ExecutionState::Cancelling => WorkClass::Stopping,
        ExecutionState::Unknown => WorkClass::Unreconciled,
        ExecutionState::Completed | ExecutionState::Failed | ExecutionState::Cancelled => {
            WorkClass::Settled
        }
    }
}

/// 把宿主声明叠加到已读出的座位上：未命中或重复都报错，不静默忽略。
fn apply_declarations(
    seats: &mut [ExecutionSeat],
    declarations: &[SeatDeclaration],
) -> Result<(), ContinuityError> {
    let mut by_seat: BTreeMap<(&str, &str), &SeatDeclaration> = BTreeMap::new();
    for declaration in declarations {
        let key = (
            declaration.issue_uid.as_str(),
            declaration.agent_id.as_str(),
        );
        if by_seat.insert(key, declaration).is_some() {
            return Err(ContinuityError::DuplicateSeatDeclaration {
                issue_uid: declaration.issue_uid.clone(),
                agent_id: declaration.agent_id.clone(),
            });
        }
        let known = seats.iter().any(|seat| {
            seat.issue_uid == declaration.issue_uid && seat.agent_id == declaration.agent_id
        });
        if !known {
            return Err(ContinuityError::UnknownSeatDeclaration {
                issue_uid: declaration.issue_uid.clone(),
                agent_id: declaration.agent_id.clone(),
            });
        }
    }
    for seat in seats.iter_mut() {
        let key = (seat.issue_uid.as_str(), seat.agent_id.as_str());
        let Some(declaration) = by_seat.get(&key) else {
            continue;
        };
        seat.separability = declaration.separability;
        if let Some((appointment_id, epoch)) = &declaration.inherited_from {
            seat.origin = SeatOrigin::InheritedFromAppointment {
                appointment_id: appointment_id.clone(),
                authority_epoch: *epoch,
            };
        }
    }
    Ok(())
}

/// 规范时间戳可直接按字典序比较（与 3.1 同口径）。
fn earlier(left: &str, right: &str) -> bool {
    left < right
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ContinuityError {
    EmptyField(&'static str),
    UnsupportedScope {
        scope_key: String,
        detail: String,
    },
    ScopeMismatch {
        issue_uid: String,
        expected_project: String,
    },
    CredentialNotPositive {
        issued_at: String,
        not_after: String,
    },
    ManagementIsNotExecution {
        appointment_id: String,
        authority_epoch: u64,
        can_act_as_execution: bool,
    },
    ExecutionIsNotManagement {
        seat_id: String,
    },
    CorruptAcceptance {
        acceptance_id: String,
        revision_id: String,
    },
    UnknownSeatDeclaration {
        issue_uid: String,
        agent_id: String,
    },
    DuplicateSeatDeclaration {
        issue_uid: String,
        agent_id: String,
    },
    RedoAccepted {
        artifact_id: String,
        revision_id: String,
        acceptance_id: String,
    },
    RestartRunning {
        run_id: String,
        invocation_id: String,
    },
    RestartBlocked {
        run_id: String,
        invocation_id: String,
        state: ExecutionState,
    },
    ExecutorLockedWhileRunning {
        issue_uid: String,
        run_id: String,
    },
    SameManager {
        agent_id: String,
    },
    Store(StoreError),
}

impl fmt::Display for ContinuityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "{field} 不能为空"),
            Self::UnsupportedScope { scope_key, detail } => {
                write!(f, "不支持的作用域 {scope_key}：{detail}")
            }
            Self::ScopeMismatch {
                issue_uid,
                expected_project,
            } => write!(f, "Issue {issue_uid} 不属于项目 {expected_project}"),
            Self::CredentialNotPositive {
                issued_at,
                not_after,
            } => write!(f, "执行凭据有效期非正：{issued_at} → {not_after}"),
            Self::ManagementIsNotExecution {
                appointment_id,
                authority_epoch,
                can_act_as_execution,
            } => write!(
                f,
                "管理票证（委任 {appointment_id}，epoch {authority_epoch}）不能作为执行凭据：can_act_as_execution={can_act_as_execution}"
            ),
            Self::ExecutionIsNotManagement { seat_id } => {
                write!(f, "执行凭据（座位 {seat_id}）不能作为管理凭据")
            }
            Self::CorruptAcceptance {
                acceptance_id,
                revision_id,
            } => write!(
                f,
                "接受 {acceptance_id} 指向的修订 {revision_id} 不存在：现场投影不可信"
            ),
            Self::UnknownSeatDeclaration {
                issue_uid,
                agent_id,
            } => write!(
                f,
                "声明命不中座位（Issue {issue_uid} / Agent {agent_id}）：不静默忽略"
            ),
            Self::DuplicateSeatDeclaration {
                issue_uid,
                agent_id,
            } => write!(
                f,
                "座位（Issue {issue_uid} / Agent {agent_id}）被声明两次：不按后写覆盖前写"
            ),
            Self::RedoAccepted {
                artifact_id,
                revision_id,
                acceptance_id,
            } => write!(
                f,
                "{artifact_id}@{revision_id} 已被接受（{acceptance_id}）：切换不能重做已接受成果"
            ),
            Self::RestartRunning {
                run_id,
                invocation_id,
            } => write!(
                f,
                "{run_id}（invocation {invocation_id}）仍在运行：切换不能重启，只能继续"
            ),
            Self::RestartBlocked {
                run_id,
                invocation_id,
                state,
            } => write!(
                f,
                "{run_id}（invocation {invocation_id}）处于 {}：先对账或等停止确认，不能重发",
                state.as_str()
            ),
            Self::ExecutorLockedWhileRunning { issue_uid, run_id } => write!(
                f,
                "Issue {issue_uid} 的 {run_id} 仍在运行：切换期改派等于停掉在跑的执行"
            ),
            Self::SameManager { agent_id } => {
                write!(f, "交接双方都是 {agent_id}：同一人不构成负责人切换")
            }
            Self::Store(err) => write!(f, "存储错误：{err}"),
        }
    }
}

impl std::error::Error for ContinuityError {}

impl From<StoreError> for ContinuityError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

// ---------------------------------------------------------------------------
// 作用域辅助
// ---------------------------------------------------------------------------

impl ManagementScope {
    /// 项目 id：专项也挂在项目下，但本模块只用它做筛选，不展开子树。
    pub fn project_id(&self) -> String {
        match self {
            Self::Project { project_id } => project_id.clone(),
            Self::IssueSubtree { project_id, .. } => project_id.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::artifact::{
        AcceptanceBinding, AcceptanceDecision, AcceptanceRecord, Artifact, ArtifactKind,
        ArtifactRevision, ArtifactVisibility, Digest, DigestAlgorithm, StorageRef,
    };
    use crate::domain::model::{
        AcceptanceCriteria, Assignment, IssueKind, IssueSpec, Owner, OwnerKind, Phase,
        ResourceMeta, ResourceStatus, Scope,
    };
    use crate::domain::{DomainId, Version};
    use crate::manager_transfer::{
        AppointRequest, ManagementCommand, ManagementCommandSet, ManagementGate,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const COMPANY: &str = "company-1";
    const PROJECT: &str = "project-1";
    const AUDIENCE: &str = "host:local";
    const NOW_MS: i64 = 1_764_100_000_000;
    const APPOINTED_AT: &str = "2026-11-20T12:00:00Z";

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-continuity-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
    }

    fn scope() -> ManagementScope {
        ManagementScope::project(PROJECT)
    }

    fn commands() -> ManagementCommandSet {
        ManagementCommandSet::new(vec![
            ManagementCommand::PlanRevise,
            ManagementCommand::AssignTask,
            ManagementCommand::ReadStatus,
        ])
        .unwrap()
    }

    fn appoint_request(manager: &str, at: &str) -> AppointRequest {
        AppointRequest::new(
            COMPANY,
            scope(),
            manager,
            "grant:manager-r2",
            "user-owner",
            at,
            Some("2027-01-01T00:00:00Z".to_string()),
            AUDIENCE,
            commands(),
        )
    }

    fn add_issue(store: &mut SqliteStore, uid: &str, owner: Option<&str>, phase: Phase) {
        let assignment = owner.map(|agent| Assignment {
            owner: Owner {
                kind: OwnerKind::Agent,
                id: DomainId::new(agent),
            },
            scope: Scope {
                company_id: DomainId::new(COMPANY),
                project_id: Some(DomainId::new(PROJECT)),
                issue_id: None,
            },
            grant_revision: 1,
        });
        let issue = Issue {
            meta: ResourceMeta {
                uid: DomainId::new(uid),
                company_id: DomainId::new(COMPANY),
                project_id: Some(DomainId::new(PROJECT)),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: IssueKind::General,
            repository: None,
            spec: IssueSpec {
                title: format!("{uid} 的标题"),
                assignment,
                acceptance: AcceptanceCriteria {
                    criteria: vec!["测试通过".to_string()],
                },
            },
            status: ResourceStatus {
                phase,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        };
        let uow = store.transaction().unwrap();
        uow.insert_issue(&issue).unwrap();
        uow.commit().unwrap();
    }

    fn add_run(store: &mut SqliteStore, run_id: &str, issue_uid: &str) {
        let uow = store.transaction().unwrap();
        uow.insert_run(run_id, COMPANY, issue_uid, "plan-1", "Running")
            .unwrap();
        uow.commit().unwrap();
    }

    fn add_invocation(
        store: &mut SqliteStore,
        invocation_id: &str,
        run_id: &str,
        state: ExecutionState,
    ) {
        let record = InvocationRecord {
            invocation_id: invocation_id.to_string(),
            company_id: COMPANY.to_string(),
            run_id: run_id.to_string(),
            work_package_revision: "wp-1".to_string(),
            attempt_id: format!("attempt-{run_id}"),
            binding_id: "binding-1".to_string(),
            fencing_token: 7,
            adapter_id: "adapter-1".to_string(),
            adapter_version: "1.0".to_string(),
            params_canonical: "{}".to_string(),
            params_digest: "sha256:00".to_string(),
            execution_id: Some(format!("exec-{run_id}")),
            state,
            created_at_ms: NOW_MS,
            accepted_at_ms: Some(NOW_MS),
            last_observed_at_ms: Some(NOW_MS),
            retention_until_ms: None,
        };
        let uow = store.transaction().unwrap();
        uow.insert_invocation(&record).unwrap();
        uow.commit().unwrap();
    }

    fn hex64(seed: &str) -> String {
        let mut digest = String::new();
        for byte in seed.bytes() {
            digest.push_str(&format!("{byte:02x}"));
            if digest.len() >= 64 {
                break;
            }
        }
        while digest.len() < 64 {
            digest.push('0');
        }
        digest.truncate(64);
        digest
    }

    fn add_artifact(store: &mut SqliteStore, artifact_id: &str) {
        let artifact = Artifact {
            id: DomainId::new(artifact_id),
            company_id: DomainId::new(COMPANY),
            project_id: DomainId::new(PROJECT),
            kind: ArtifactKind::Report,
            title: format!("{artifact_id} 报告"),
            owner: DomainId::new("agent-w1"),
            visibility: ArtifactVisibility::Internal,
            accepted_revision: None,
        };
        let uow = store.transaction().unwrap();
        uow.insert_artifact(&artifact, NOW_MS).unwrap();
        uow.commit().unwrap();
    }

    fn add_revision(
        store: &mut SqliteStore,
        revision_id: &str,
        artifact_id: &str,
        produced_by: &str,
    ) {
        let digest = hex64(revision_id);
        let revision = ArtifactRevision {
            revision_id: DomainId::new(revision_id),
            artifact_id: DomainId::new(artifact_id),
            base_revision: None,
            storage: StorageRef::Cas(digest.clone()),
            digest: Digest {
                algorithm: DigestAlgorithm::Sha256,
                value: digest,
            },
            size_bytes: 128,
            mime: "text/markdown".to_string(),
            produced_by: DomainId::new(produced_by),
            source_note: "测试样本".to_string(),
            depends_on: Vec::new(),
            created_at_ms: NOW_MS,
        };
        let uow = store.transaction().unwrap();
        uow.insert_artifact_revision(COMPANY, &revision).unwrap();
        uow.commit().unwrap();
    }

    /// 写一条接受并把 Artifact 的接受指针指向该修订（§21.1 的接受指针）。
    fn accept(
        store: &mut SqliteStore,
        acceptance_id: &str,
        artifact_id: &str,
        revision_id: &str,
        decision: AcceptanceDecision,
    ) {
        let record = AcceptanceRecord {
            acceptance_id: DomainId::new(acceptance_id),
            artifact_id: DomainId::new(artifact_id),
            binding: AcceptanceBinding {
                work_package_revision: DomainId::new("wp-1"),
                input_baseline: DomainId::new("input-1"),
                artifact_revision: DomainId::new(revision_id),
                verification_rules: DomainId::new("rules-1"),
                target_environment: DomainId::new("env-1"),
                merge_combination: None,
            },
            decision,
            actor: DomainId::new("user-owner"),
            reason: "样本接受".to_string(),
            evidence_refs: vec!["test:evidence-1".to_string()],
            accepted_at_ms: NOW_MS,
            superseded_by: None,
        };
        let uow = store.transaction().unwrap();
        uow.insert_acceptance(COMPANY, &record).unwrap();
        if decision.is_accepted() {
            assert!(
                uow.cas_accepted_revision(artifact_id, None, revision_id)
                    .unwrap(),
                "接受指针应更新"
            );
        }
        uow.commit().unwrap();
    }

    fn read_issue(store: &mut SqliteStore, uid: &str) -> Issue {
        store
            .transaction()
            .unwrap()
            .get_issue(uid)
            .unwrap()
            .unwrap()
    }

    fn read_invocation(store: &mut SqliteStore, invocation_id: &str) -> InvocationRecord {
        store
            .transaction()
            .unwrap()
            .get_invocation(invocation_id)
            .unwrap()
            .unwrap()
    }

    /// 一个员工在跑、一个员工已交付、成果由旧经理以执行身份产出并被接受。
    fn seeded(root: &Path) -> (SqliteStore, ManagementAppointment) {
        let mut store = open_store(root);
        add_issue(&mut store, "issue-run", Some("agent-w1"), Phase::Running);
        add_issue(&mut store, "issue-done", Some("agent-w1"), Phase::Succeeded);
        add_run(&mut store, "run-run", "issue-run");
        add_run(&mut store, "run-done", "issue-done");
        add_invocation(&mut store, "inv-run", "run-run", ExecutionState::Running);
        add_invocation(
            &mut store,
            "inv-done",
            "run-done",
            ExecutionState::Completed,
        );
        add_artifact(&mut store, "art-report");
        add_revision(&mut store, "rev-1", "art-report", "manager-a");
        accept(
            &mut store,
            "acc-1",
            "art-report",
            "rev-1",
            AcceptanceDecision::Accepted,
        );
        let appointment = ManagementAppointment::appoint(
            &store,
            appoint_request("manager-a", APPOINTED_AT),
            NOW_MS,
        )
        .unwrap();
        (store, appointment)
    }

    fn declarations() -> Vec<SeatDeclaration> {
        vec![
            SeatDeclaration::new("issue-run", "agent-w1", SeatSeparability::Separable),
            SeatDeclaration::new("issue-done", "agent-w1", SeatSeparability::Separable),
        ]
    }

    #[test]
    fn switch_keeps_running_work_accepted_results_and_rejects_old_commands() {
        let root = temp_root("switch");
        let (mut store, mut appointment_a) = seeded(&root);
        let scope = scope();
        let declarations = declarations();

        let before =
            ContinuityState::load_with_declarations(&store, COMPANY, &scope, &declarations)
                .unwrap();
        assert_eq!(before.running.len(), 1, "只有 inv-run 在跑");
        assert_eq!(before.running[0].invocation_id, "inv-run");
        assert_eq!(before.accepted.len(), 1);
        assert_eq!(before.accepted[0].artifact_id, "art-report");
        assert_eq!(before.contributions.len(), 1);
        assert_eq!(before.contributions[0].produced_by, "manager-a");
        assert!(before.stale_acceptances.is_empty());

        let plan_before = plan(&before, "manager-a").unwrap();
        assert_eq!(
            plan_before.directive_for_run("run-run"),
            Some(Directive::Continue)
        );
        assert_eq!(
            plan_before.directive_for_artifact("art-report"),
            Some(Directive::Preserve)
        );
        assert!(plan_before.items.iter().any(|item| {
            item.party == PlanParty::OutgoingManager
                && item.directive == Directive::Retain
                && matches!(&item.subject, PlannedSubject::Contribution { revision_id, .. } if revision_id == "rev-1")
        }));
        assert_eq!(
            plan_before.counts(),
            PlanCounts {
                continue_runs: 1,
                preserved: 1,
                retained: 1,
                rebuild_via_handoff: 0,
                attention: 0,
            }
        );

        // 切换：撤任 A（epoch 1 → 2，旧票当场失效），再任命 B（epoch 3）。
        let version_a = appointment_a.version;
        appointment_a
            .revoke(&store, "2026-11-20T12:05:00Z", version_a, NOW_MS)
            .unwrap();
        let appointment_b = ManagementAppointment::appoint(
            &store,
            appoint_request("manager-b", "2026-11-20T12:06:00Z"),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(appointment_b.authority_epoch, 3);

        let after = ContinuityState::load_with_declarations(&store, COMPANY, &scope, &declarations)
            .unwrap();
        assert_eq!(after.running, before.running, "运行中的执行照旧");
        assert_eq!(after.accepted, before.accepted, "已接受成果不重做");
        assert_eq!(after.contributions, before.contributions, "贡献保留");
        assert_eq!(
            plan(&after, "manager-a").unwrap().items,
            plan_before.items,
            "换人后同一现场的指令逐条不变"
        );

        // 执行事实没有被切换改写。
        let invocation = read_invocation(&mut store, "inv-run");
        assert_eq!(invocation.state, ExecutionState::Running);
        assert_eq!(
            store.list_acceptances_for_company(COMPANY).unwrap().len(),
            1,
            "接受记录未被删改"
        );

        // 身份分离：A 只剩执行身份，B 只有管理身份。
        let role_a = after.role_of("manager-a", Some(&appointment_a));
        assert!(!role_a.holds_management(), "A 的管理权已随撤任失效");
        assert!(
            !role_a.holds_execution(),
            "本场景 A 没有执行座位：执行身份来自 Assignment，不来自管理权"
        );
        let role_b = after.role_of("manager-b", Some(&appointment_b));
        assert!(!role_b.holds_execution());
        assert!(role_b.holds_management());
        assert_eq!(role_b.management.as_ref().unwrap().authority_epoch, 3);

        // 旧管理命令被拒（3.1 的复核），员工执行不受影响。
        let old_ticket = ManagementTicket::issue(
            &appointment_a,
            ManagementCommand::AssignTask,
            "2026-11-20T12:06:00Z",
            "2026-11-20T12:20:00Z",
        )
        .unwrap();
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let error = gate
            .submit(&old_ticket, "cmd-old-1", "2026-11-20T12:06:00Z", NOW_MS)
            .unwrap_err();
        match error {
            crate::manager_transfer::TransferError::Rejected { code, .. } => assert_eq!(
                code,
                crate::manager_transfer::CommandRejectionCode::StaleAppointment
            ),
            other => panic!("期望旧 epoch 被拒，实际 {other:?}"),
        }
        let after_reject =
            ContinuityState::load_with_declarations(&store, COMPANY, &scope, &declarations)
                .unwrap();
        assert_eq!(after_reject.running, before.running);
    }

    #[test]
    fn employee_instruction_does_not_depend_on_who_leaves() {
        let root = temp_root("invariance");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-w1", Some("agent-w1"), Phase::Running);
        add_issue(&mut store, "issue-mgr", Some("manager-a"), Phase::Running);
        add_run(&mut store, "run-w1", "issue-w1");
        add_run(&mut store, "run-mgr", "issue-mgr");
        add_invocation(&mut store, "inv-w1", "run-w1", ExecutionState::Running);
        add_invocation(&mut store, "inv-mgr", "run-mgr", ExecutionState::Running);
        let declarations = [
            SeatDeclaration::new("issue-w1", "agent-w1", SeatSeparability::Separable),
            SeatDeclaration::new("issue-mgr", "manager-a", SeatSeparability::Separable),
        ];
        let state =
            ContinuityState::load_with_declarations(&store, COMPANY, &scope(), &declarations)
                .unwrap();

        let from_a = plan(&state, "manager-a").unwrap();
        let from_elsewhere = plan(&state, "manager-z").unwrap();

        let employee = |plan: &SwitchPlan| {
            plan.items
                .iter()
                .find(|item| {
                    matches!(&item.subject, PlannedSubject::Running { run_id, .. } if run_id == "run-w1")
                })
                .cloned()
                .unwrap()
        };
        assert_eq!(employee(&from_a), employee(&from_elsewhere));
        assert_eq!(employee(&from_a).party, PlanParty::Employee);
        assert_eq!(employee(&from_a).directive, Directive::Continue);

        // 旧经理自己那一项才因「谁走」变化。
        let manager_item = |plan: &SwitchPlan| {
            plan.items
                .iter()
                .find(|item| {
                    matches!(&item.subject, PlannedSubject::Running { run_id, .. } if run_id == "run-mgr")
                })
                .cloned()
                .unwrap()
        };
        assert_eq!(manager_item(&from_a).party, PlanParty::OutgoingManager);
        assert_eq!(
            manager_item(&from_a).directive,
            Directive::Continue,
            "可分离时保留执行"
        );
        assert_eq!(manager_item(&from_elsewhere).party, PlanParty::Employee);
    }

    #[test]
    fn outgoing_manager_execution_without_declaration_requires_handoff() {
        let root = temp_root("rebuild");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-mgr", Some("manager-a"), Phase::Running);
        add_run(&mut store, "run-mgr", "issue-mgr");
        add_invocation(&mut store, "inv-mgr", "run-mgr", ExecutionState::Running);
        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();

        // 未声明：不当作可分离（缺省值不是能力证据）。
        let undeclared = plan(&state, "manager-a").unwrap();
        assert_eq!(
            undeclared.directive_for_run("run-mgr"),
            Some(Directive::RebuildViaHandoff)
        );
        assert_eq!(undeclared.counts().rebuild_via_handoff, 1);
        let note = undeclared.items[0].note.clone();
        assert!(note.contains("未声明可分离"), "应回显未声明状态：{note}");

        let declarations = [SeatDeclaration::new(
            "issue-mgr",
            "manager-a",
            SeatSeparability::NotSeparable,
        )];
        let not_separable =
            ContinuityState::load_with_declarations(&store, COMPANY, &scope(), &declarations)
                .unwrap();
        assert_eq!(
            plan(&not_separable, "manager-a")
                .unwrap()
                .directive_for_run("run-mgr"),
            Some(Directive::RebuildViaHandoff)
        );

        let declarations = [SeatDeclaration::new(
            "issue-mgr",
            "manager-a",
            SeatSeparability::Separable,
        )];
        let separable =
            ContinuityState::load_with_declarations(&store, COMPANY, &scope(), &declarations)
                .unwrap();
        let kept = plan(&separable, "manager-a").unwrap();
        assert_eq!(kept.directive_for_run("run-mgr"), Some(Directive::Continue));
        assert_eq!(separable.seats[0].separability, SeatSeparability::Separable);
    }

    #[test]
    fn seat_declarations_must_hit_reality_and_appear_once() {
        let root = temp_root("declarations");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-run", Some("agent-w1"), Phase::Running);

        let unknown = [SeatDeclaration::new(
            "issue-ghost",
            "agent-w1",
            SeatSeparability::Separable,
        )];
        let error = ContinuityState::load_with_declarations(&store, COMPANY, &scope(), &unknown)
            .unwrap_err();
        assert!(matches!(
            error,
            ContinuityError::UnknownSeatDeclaration { .. }
        ));

        let duplicated = [
            SeatDeclaration::new("issue-run", "agent-w1", SeatSeparability::Separable),
            SeatDeclaration::new("issue-run", "agent-w1", SeatSeparability::NotSeparable),
        ];
        let error = ContinuityState::load_with_declarations(&store, COMPANY, &scope(), &duplicated)
            .unwrap_err();
        assert!(matches!(
            error,
            ContinuityError::DuplicateSeatDeclaration { .. }
        ));

        // 声明来源只作记录：座位仍来自 Issue 指派。
        let with_origin =
            [
                SeatDeclaration::new("issue-run", "agent-w1", SeatSeparability::Separable)
                    .inherited_from("appointment:project:project-1#1", 1),
            ];
        let state =
            ContinuityState::load_with_declarations(&store, COMPANY, &scope(), &with_origin)
                .unwrap();
        assert_eq!(
            state.seats[0].origin,
            SeatOrigin::InheritedFromAppointment {
                appointment_id: "appointment:project:project-1#1".to_string(),
                authority_epoch: 1,
            }
        );
    }

    #[test]
    fn every_execution_state_is_classified_for_switch() {
        use ExecutionState::*;
        for state in [
            Accepted, Starting, Running, NeedsInput, NeedsAuth, Suspended,
        ] {
            assert_eq!(classify(state), WorkClass::InFlight, "{}", state.as_str());
        }
        assert_eq!(classify(Cancelling), WorkClass::Stopping);
        assert_eq!(classify(Unknown), WorkClass::Unreconciled);
        for state in [Completed, Failed, Cancelled] {
            assert_eq!(classify(state), WorkClass::Settled, "{}", state.as_str());
        }
    }

    #[test]
    fn cancelling_and_unknown_wait_for_people_not_for_the_switch() {
        let root = temp_root("attention");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-stop", Some("agent-w1"), Phase::Running);
        add_issue(
            &mut store,
            "issue-unknown",
            Some("agent-w1"),
            Phase::Blocked,
        );
        add_run(&mut store, "run-stop", "issue-stop");
        add_run(&mut store, "run-unknown", "issue-unknown");
        add_invocation(
            &mut store,
            "inv-stop",
            "run-stop",
            ExecutionState::Cancelling,
        );
        add_invocation(
            &mut store,
            "inv-unknown",
            "run-unknown",
            ExecutionState::Unknown,
        );

        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        assert!(state.running.is_empty(), "取消中/未知都不算继续项");
        assert_eq!(state.attention.len(), 2);
        let stop = state
            .attention
            .iter()
            .find(|item| item.run_id == "run-stop")
            .unwrap();
        assert_eq!(stop.remedy, Remedy::ConfirmStop);
        let unknown = state
            .attention
            .iter()
            .find(|item| item.run_id == "run-unknown")
            .unwrap();
        assert_eq!(unknown.remedy, Remedy::ReconcileEffect);

        let switch_plan = plan(&state, "manager-a").unwrap();
        assert_eq!(
            switch_plan.directive_for_run("run-stop"),
            Some(Directive::ResolveBeforeSwitch)
        );
        assert_eq!(
            switch_plan.directive_for_run("run-unknown"),
            Some(Directive::ResolveBeforeSwitch)
        );
        // 未知副作用不能靠重发绕过（否则可能双重执行）。
        match switch_plan.may_restart("run-unknown").unwrap_err() {
            ContinuityError::RestartBlocked { state, .. } => {
                assert_eq!(state, ExecutionState::Unknown)
            }
            other => panic!("期望 RestartBlocked，实际 {other:?}"),
        }
        assert!(matches!(
            switch_plan.may_restart("run-stop").unwrap_err(),
            ContinuityError::RestartBlocked { .. }
        ));
    }

    #[test]
    fn accepted_artifact_cannot_be_redone_but_open_work_can() {
        let root = temp_root("redo");
        let (mut store, _appointment) = seeded(&root);
        add_artifact(&mut store, "art-draft");
        add_revision(&mut store, "rev-draft", "art-draft", "agent-w1");
        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        let switch_plan = plan(&state, "manager-a").unwrap();

        match switch_plan.may_redo("art-report").unwrap_err() {
            ContinuityError::RedoAccepted {
                artifact_id,
                revision_id,
                acceptance_id,
            } => {
                assert_eq!(artifact_id, "art-report");
                assert_eq!(revision_id, "rev-1");
                assert_eq!(acceptance_id, "acc-1");
            }
            other => panic!("期望 RedoAccepted，实际 {other:?}"),
        }
        // 未接受的草稿不受保护：它本来就没通过验收。
        assert!(switch_plan.may_redo("art-draft").is_ok());
        assert_eq!(
            switch_plan.directive_for_artifact("art-draft"),
            None,
            "未接受成果不进保留清单"
        );
    }

    #[test]
    fn running_run_cannot_be_restarted_and_finished_run_can() {
        let root = temp_root("restart");
        let (store, _appointment) = seeded(&root);
        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        let switch_plan = plan(&state, "manager-a").unwrap();

        match switch_plan.may_restart("run-run").unwrap_err() {
            ContinuityError::RestartRunning {
                run_id,
                invocation_id,
            } => {
                assert_eq!(run_id, "run-run");
                assert_eq!(invocation_id, "inv-run");
            }
            other => panic!("期望 RestartRunning，实际 {other:?}"),
        }
        assert!(
            switch_plan.may_restart("run-done").is_ok(),
            "已终结的 Run 可正常重新调度"
        );
    }

    #[test]
    fn executor_cannot_be_replaced_while_issue_is_running() {
        let root = temp_root("executor");
        let (store, _appointment) = seeded(&root);
        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        let switch_plan = plan(&state, "manager-a").unwrap();

        match switch_plan.may_change_executor("issue-run").unwrap_err() {
            ContinuityError::ExecutorLockedWhileRunning { issue_uid, run_id } => {
                assert_eq!(issue_uid, "issue-run");
                assert_eq!(run_id, "run-run");
            }
            other => panic!("期望 ExecutorLockedWhileRunning，实际 {other:?}"),
        }
        assert!(
            switch_plan.may_change_executor("issue-done").is_ok(),
            "没有在跑执行的 Issue 可以正常改派"
        );
    }

    #[test]
    fn superseded_and_requires_human_acceptances_are_not_preserved() {
        let root = temp_root("stale");
        let mut store = open_store(&root);
        add_artifact(&mut store, "art-1");
        add_artifact(&mut store, "art-2");
        add_revision(&mut store, "rev-1", "art-1", "agent-w1");
        add_revision(&mut store, "rev-2", "art-2", "agent-w1");
        accept(
            &mut store,
            "acc-old",
            "art-1",
            "rev-1",
            AcceptanceDecision::Accepted,
        );
        let uow = store.transaction().unwrap();
        uow.supersede_acceptances("art-1", "acc-new").unwrap();
        uow.commit().unwrap();
        accept(
            &mut store,
            "acc-human",
            "art-2",
            "rev-2",
            AcceptanceDecision::RequiresHuman,
        );

        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        assert!(state.accepted.is_empty(), "被取代与待人工都不算已接受");
        assert_eq!(
            state.stale_acceptances,
            vec!["acc-human".to_string(), "acc-old".to_string()]
        );
        assert!(
            state.contributions.is_empty(),
            "没有保留的接受就没有保留的贡献"
        );
        let switch_plan = plan(&state, "manager-a").unwrap();
        assert!(switch_plan.items.is_empty());
        assert!(
            switch_plan.may_redo("art-1").is_ok(),
            "未生效的接受不保护重做"
        );
    }

    #[test]
    fn acceptance_of_a_stale_revision_is_not_current() {
        let root = temp_root("pointer");
        let mut store = open_store(&root);
        add_artifact(&mut store, "art-1");
        add_revision(&mut store, "rev-1", "art-1", "agent-w1");
        add_revision(&mut store, "rev-2", "art-1", "agent-w1");
        accept(
            &mut store,
            "acc-1",
            "art-1",
            "rev-1",
            AcceptanceDecision::Accepted,
        );
        // 接受指针前进到 rev-2：rev-1 的接受保留在历史里，但不再是「当前已接受」。
        let uow = store.transaction().unwrap();
        assert!(
            uow.cas_accepted_revision("art-1", Some("rev-1"), "rev-2")
                .unwrap()
        );
        uow.commit().unwrap();

        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        assert!(state.accepted.is_empty());
        assert_eq!(state.stale_acceptances, vec!["acc-1".to_string()]);
    }

    #[test]
    fn contribution_of_the_outgoing_manager_is_retained() {
        let root = temp_root("contribution");
        let (store, _appointment) = seeded(&root);
        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();

        let from_a = plan(&state, "manager-a").unwrap();
        let item = from_a
            .items
            .iter()
            .find(|item| matches!(&item.subject, PlannedSubject::Contribution { .. }))
            .expect("应有贡献项");
        assert_eq!(item.directive, Directive::Retain);
        assert_eq!(item.party, PlanParty::OutgoingManager);
        assert!(item.note.contains("manager-a"));

        // 同一份贡献在别人走时只是普通历史，不被删除也不被改写。
        let from_z = plan(&state, "manager-z").unwrap();
        let item = from_z
            .items
            .iter()
            .find(|item| matches!(&item.subject, PlannedSubject::Contribution { .. }))
            .unwrap();
        assert_eq!(item.directive, Directive::Retain);
        assert_eq!(item.party, PlanParty::History);
        assert_eq!(
            from_a.counts().retained,
            from_z.counts().retained,
            "换谁走都不改变贡献条数"
        );
    }

    #[test]
    fn roles_separate_management_from_execution() {
        let root = temp_root("roles");
        let (mut store, mut appointment_a) = seeded(&root);
        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();

        let before = state.role_of("manager-a", Some(&appointment_a));
        assert!(before.holds_management());
        assert!(
            !before.holds_execution(),
            "manager-a 在 seeded 现场里没有执行座位（成果产出者不等于座位持有者）"
        );

        let version = appointment_a.version;
        appointment_a
            .revoke(&store, "2026-11-20T12:05:00Z", version, NOW_MS)
            .unwrap();
        let after = state.role_of("manager-a", Some(&appointment_a));
        assert!(!after.holds_management());
        assert_eq!(
            after.management.as_ref().unwrap().status,
            AppointmentStatus::Revoked
        );

        // 兼做执行的旧经理：撤任后执行座位照旧（§19.10）。
        add_issue(&mut store, "issue-mgr", Some("manager-a"), Phase::Running);
        let with_seat = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        let mixed = with_seat.role_of("manager-a", Some(&appointment_a));
        assert!(mixed.holds_execution());
        assert!(!mixed.holds_management());
        assert_eq!(mixed.execution_seats.len(), 1);
    }

    #[test]
    fn management_ticket_is_not_an_execution_credential() {
        let root = temp_root("cross-use");
        let (mut store, appointment) = seeded(&root);
        let ticket = ManagementTicket::issue(
            &appointment,
            ManagementCommand::AssignTask,
            "2026-11-20T12:01:00Z",
            "2026-11-20T12:10:00Z",
        )
        .unwrap();

        assert!(!ticket.can_act_as_execution());
        let error = management_ticket_as_execution(&ticket).unwrap_err();
        match error {
            ContinuityError::ManagementIsNotExecution {
                appointment_id,
                authority_epoch,
                can_act_as_execution,
            } => {
                assert_eq!(appointment_id, appointment.appointment_id);
                assert_eq!(authority_epoch, 1);
                assert!(!can_act_as_execution);
            }
            other => panic!("期望 ManagementIsNotExecution，实际 {other:?}"),
        }

        // 反向同样不存在。
        let seat = ExecutionSeat::from_issue(&scope(), &read_issue(&mut store, "issue-run"))
            .unwrap()
            .unwrap();
        let invocation = read_invocation(&mut store, "inv-run");
        let credential = ExecutionCredential::for_invocation(
            &seat,
            &invocation,
            "2026-11-20T12:01:00Z",
            "2026-11-20T12:10:00Z",
        )
        .unwrap();
        assert_eq!(credential.role(), ActingRole::Execution);
        assert!(!credential.can_issue_management_command());
        assert_eq!(credential.fencing_token, invocation.fencing_token);
        assert_eq!(credential.attempt_id, invocation.attempt_id);
        assert!(matches!(
            credential.as_management_ticket().unwrap_err(),
            ContinuityError::ExecutionIsNotManagement { .. }
        ));
    }

    #[test]
    fn execution_credential_needs_a_positive_window_and_real_attempt() {
        let root = temp_root("credential");
        let (mut store, _appointment) = seeded(&root);
        let seat = ExecutionSeat::from_issue(&scope(), &read_issue(&mut store, "issue-run"))
            .unwrap()
            .unwrap();
        let mut invocation = read_invocation(&mut store, "inv-run");

        let error = ExecutionCredential::for_invocation(
            &seat,
            &invocation,
            "2026-11-20T12:10:00Z",
            "2026-11-20T12:10:00Z",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ContinuityError::CredentialNotPositive { .. }
        ));

        invocation.attempt_id = "  ".to_string();
        let error = ExecutionCredential::for_invocation(
            &seat,
            &invocation,
            "2026-11-20T12:00:00Z",
            "2026-11-20T12:10:00Z",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ContinuityError::EmptyField("invocation.attempt_id")
        ));
    }

    #[test]
    fn handover_report_labels_but_does_not_decide() {
        let root = temp_root("report");
        let (store, _appointment) = seeded(&root);
        let state = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        let switch_plan = plan(&state, "manager-a").unwrap();

        let report = switch_plan.handover("manager-a", "manager-b").unwrap();
        assert_eq!(report.from, "manager-a");
        assert_eq!(report.to, "manager-b");
        assert_eq!(report.items, switch_plan.items.len());
        assert_eq!(report.counts.continue_runs, 1);

        assert!(matches!(
            switch_plan.handover("manager-a", "manager-a").unwrap_err(),
            ContinuityError::SameManager { .. }
        ));
        assert!(matches!(
            switch_plan.handover("  ", "manager-b").unwrap_err(),
            ContinuityError::EmptyField("handover.from")
        ));
        assert!(matches!(
            plan(&state, " ").unwrap_err(),
            ContinuityError::EmptyField("outgoing_manager")
        ));
    }

    #[test]
    fn directive_set_has_no_stop_or_redo_variant() {
        // 类型事实的回归保护：新增指令必须先想清楚它是否等于「切换能停人」。
        let names: Vec<&str> = Directive::ALL.iter().map(|item| item.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "continue",
                "preserve",
                "retain",
                "rebuild_via_handoff",
                "resolve_before_switch"
            ]
        );
        for forbidden in ["cancel", "stop", "restart", "redo"] {
            assert!(
                !names.contains(&forbidden),
                "切换指令集合不得包含 {forbidden}"
            );
        }
    }

    #[test]
    fn issue_subtree_scope_is_refused_rather_than_approximated() {
        let root = temp_root("subtree");
        let store = open_store(&root);
        let error = ContinuityState::load(
            &store,
            COMPANY,
            &ManagementScope::issue_subtree(PROJECT, "issue-root"),
        )
        .unwrap_err();
        match error {
            ContinuityError::UnsupportedScope { scope_key, detail } => {
                assert_eq!(scope_key, "issue:project-1/issue-root");
                assert!(detail.contains("3.5"));
            }
            other => panic!("期望 UnsupportedScope，实际 {other:?}"),
        }
    }

    #[test]
    fn projection_survives_reopen_and_does_not_write() {
        let root = temp_root("reopen");
        let (store, _appointment) = seeded(&root);
        let before = ContinuityState::load(&store, COMPANY, &scope()).unwrap();
        let counts = store.core_row_counts().unwrap();
        drop(store);

        let reopened = open_store(&root);
        let after = ContinuityState::load(&reopened, COMPANY, &scope()).unwrap();
        assert_eq!(after, before, "只读投影不依赖进程内状态");
        assert_eq!(
            reopened.core_row_counts().unwrap(),
            counts,
            "装载投影不写任何一行"
        );
    }
}
