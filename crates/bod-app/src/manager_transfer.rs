//! P2 任务 3.1：管理委任、`authority_epoch` 与命令复核。
//!
//! 权威语义：V2.9.6 §5.9（ManagementAppointment 字段与四种状态）、§19.3（epoch 与 Active 由宿主
//! 生成，Agent 不能自选）、§19.8（每个管理 scope 最多一个当前 Active 主委任；epoch 单调递增且不回退；
//! 命令进入队列时校验不够，应用前再核对）、§31.10（管理凭据绑定 Appointment / epoch / scope /
//! 命令集合 / 时效 / audience，不能作为员工执行凭据或长期密钥）。
//!
//! 本模块的边界：
//! - 只管「谁现在是这个 scope 的管理者、凭据还能不能用」，不实现切换流程（3.3）、不管员工执行
//!   （3.2）、不改历史成果与费用（3.4）。这里没有任何写预算、计划、派发或成果的代码路径。
//! - 拒绝一律落 `management_commands` 审计行（原因码 + 说明），可读回；不写「已拒绝」到别处。
//! - 缓存只在「头指针版本未变」时可用；每次提交仍读一次权威头行，所以换 epoch、暂停、撤任之后
//!   暖缓存不会变成放行凭据。

use std::collections::BTreeMap;

use crate::canonical_json;
use crate::domain::DomainId;
use crate::store::{SqliteStore, StoreError};
use std::fmt;

/// 委任 schema：与交接包、接管意图各自版本化。
pub const APPOINTMENT_SCHEMA: &str = "bod.appointment/v1";

/// 管理凭据的最长有效期（秒）。§31.10：管理凭据是短时凭据，不能当长期密钥下发。
pub const MAX_TICKET_SECONDS: i64 = 900;

/// 缓存条目上限。超过即整体清空（有界即可，不做 LRU：缓存的正确性不依赖淘汰策略）。
pub const MAX_CACHED_SCOPES: usize = 16;

// ---------------------------------------------------------------------------
// 作用域
// ---------------------------------------------------------------------------

/// 管理作用域：项目，或显式的根 Issue 子树（§5.9）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManagementScope {
    Project {
        project_id: String,
    },
    IssueSubtree {
        project_id: String,
        root_issue_id: String,
    },
}

impl ManagementScope {
    pub fn project(project_id: impl Into<String>) -> Self {
        Self::Project {
            project_id: project_id.into(),
        }
    }

    pub fn issue_subtree(project_id: impl Into<String>, root_issue_id: impl Into<String>) -> Self {
        Self::IssueSubtree {
            project_id: project_id.into(),
            root_issue_id: root_issue_id.into(),
        }
    }

    /// 持久化键。项目与专项各自独立，专项不吞掉项目默认委任。
    pub fn scope_key(&self) -> String {
        match self {
            Self::Project { project_id } => format!("project:{project_id}"),
            Self::IssueSubtree {
                project_id,
                root_issue_id,
            } => format!("issue:{project_id}/{root_issue_id}"),
        }
    }

    pub fn parse_key(text: &str) -> Result<Self, TransferError> {
        let trimmed = text.trim();
        if let Some(project) = trimmed.strip_prefix("project:") {
            if project.is_empty() {
                return Err(TransferError::EmptyField("scope.project_id"));
            }
            return Ok(Self::project(project));
        }
        if let Some(rest) = trimmed.strip_prefix("issue:") {
            let (project, root) = rest
                .split_once('/')
                .ok_or_else(|| TransferError::UnknownScopeKey(text.to_string()))?;
            if project.is_empty() || root.is_empty() {
                return Err(TransferError::UnknownScopeKey(text.to_string()));
            }
            return Ok(Self::issue_subtree(project, root));
        }
        Err(TransferError::UnknownScopeKey(text.to_string()))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Project { project_id } => format!("项目 {project_id}"),
            Self::IssueSubtree {
                project_id,
                root_issue_id,
            } => format!("项目 {project_id} 的专项 {root_issue_id} 子树"),
        }
    }

    fn validate(&self) -> Result<(), TransferError> {
        match self {
            Self::Project { project_id } => {
                if project_id.trim().is_empty() {
                    return Err(TransferError::EmptyField("scope.project_id"));
                }
            }
            Self::IssueSubtree {
                project_id,
                root_issue_id,
            } => {
                if project_id.trim().is_empty() {
                    return Err(TransferError::EmptyField("scope.project_id"));
                }
                if root_issue_id.trim().is_empty() {
                    return Err(TransferError::EmptyField("scope.root_issue_id"));
                }
            }
        }
        Ok(())
    }
}

impl fmt::Display for ManagementScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.scope_key())
    }
}

// ---------------------------------------------------------------------------
// 状态与命令集
// ---------------------------------------------------------------------------

/// 委任状态（§5.9）。`Revoked` 是终态：撤销的委任不恢复，重新委任生成新的 epoch。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppointmentStatus {
    Active,
    Suspended,
    Revoked,
    Expired,
}

impl AppointmentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "revoked" => Some(Self::Revoked),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Revoked | Self::Expired)
    }

    pub const fn describe(self) -> &'static str {
        match self {
            Self::Active => "在任",
            Self::Suspended => "已暂停",
            Self::Revoked => "已撤销",
            Self::Expired => "已到期",
        }
    }
}

/// 管理命令类别（§19.4）。前六类可在授权内直接落实，后两类是治理命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManagementCommand {
    /// 计划/拆分：新建有限子任务、修订依赖与安排。
    PlanRevise,
    /// 分工：指派已有合法 Agent、调整优先级。
    AssignTask,
    /// 监督：读取项目状态、提出澄清。
    ReadStatus,
    /// 修复：有界重试、返工、提出接续。
    RequestRepair,
    /// 成果：验证后接受授权类别的中间成果。
    AcceptIntermediate,
    /// 交付：提交阶段/最终交付与风险报告。
    SubmitDelivery,
    /// 治理：任命管理负责人。负责人不能给自己或他人任命。
    AppointManager,
    /// 治理：撤任管理负责人。
    RevokeAppointment,
}

impl ManagementCommand {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlanRevise => "plan_revise",
            Self::AssignTask => "assign_task",
            Self::ReadStatus => "read_status",
            Self::RequestRepair => "request_repair",
            Self::AcceptIntermediate => "accept_intermediate",
            Self::SubmitDelivery => "submit_delivery",
            Self::AppointManager => "appoint_manager",
            Self::RevokeAppointment => "revoke_appointment",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "plan_revise" => Some(Self::PlanRevise),
            "assign_task" => Some(Self::AssignTask),
            "read_status" => Some(Self::ReadStatus),
            "request_repair" => Some(Self::RequestRepair),
            "accept_intermediate" => Some(Self::AcceptIntermediate),
            "submit_delivery" => Some(Self::SubmitDelivery),
            "appoint_manager" => Some(Self::AppointManager),
            "revoke_appointment" => Some(Self::RevokeAppointment),
            _ => None,
        }
    }

    /// 治理命令：只能由治理主体发出，不属于委任可授予的命令集。
    pub const fn is_governance(self) -> bool {
        matches!(self, Self::AppointManager | Self::RevokeAppointment)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::PlanRevise => "计划/拆分",
            Self::AssignTask => "分工",
            Self::ReadStatus => "监督",
            Self::RequestRepair => "修复",
            Self::AcceptIntermediate => "接受中间成果",
            Self::SubmitDelivery => "提交交付",
            Self::AppointManager => "任命负责人",
            Self::RevokeAppointment => "撤任负责人",
        }
    }
}

/// 委任可授予的管理命令集合。
///
/// 治理命令在构造时被拒：类型上不存在「带着任命权下发」的委任命令集，因此调用方无从绕过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementCommandSet {
    commands: Vec<ManagementCommand>,
}

impl ManagementCommandSet {
    pub fn new(commands: Vec<ManagementCommand>) -> Result<Self, TransferError> {
        if commands.is_empty() {
            return Err(TransferError::EmptyCommandSet);
        }
        for command in &commands {
            if command.is_governance() {
                return Err(TransferError::GovernanceCommandNotGrantable(*command));
            }
        }
        let mut sorted = commands;
        sorted.sort();
        let mut deduped: Vec<ManagementCommand> = Vec::with_capacity(sorted.len());
        for command in sorted {
            if deduped.contains(&command) {
                return Err(TransferError::DuplicateCommand(command));
            }
            deduped.push(command);
        }
        Ok(Self { commands: deduped })
    }

    pub fn allows(&self, command: ManagementCommand) -> bool {
        self.commands.contains(&command)
    }

    pub fn commands(&self) -> &[ManagementCommand] {
        &self.commands
    }

    pub fn describe(&self) -> String {
        self.commands
            .iter()
            .map(|command| command.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    fn to_json(&self) -> String {
        serde_json::Value::Array(
            self.commands
                .iter()
                .map(|command| serde_json::Value::String(command.as_str().to_string()))
                .collect(),
        )
        .to_string()
    }

    fn from_json(text: &str) -> Result<Self, TransferError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|_| TransferError::CorruptAppointment {
                detail: "commands_json 不是合法 JSON".to_string(),
            })?;
        let items = value
            .as_array()
            .ok_or_else(|| TransferError::CorruptAppointment {
                detail: "commands_json 不是数组".to_string(),
            })?;
        let mut commands = Vec::with_capacity(items.len());
        for item in items {
            let text = item
                .as_str()
                .ok_or_else(|| TransferError::CorruptAppointment {
                    detail: "命令不是字符串".to_string(),
                })?;
            let command =
                ManagementCommand::parse(text).ok_or_else(|| TransferError::UnknownCommand {
                    command: text.to_string(),
                })?;
            commands.push(command);
        }
        Self::new(commands)
    }
}

// ---------------------------------------------------------------------------
// 时间比较
// ---------------------------------------------------------------------------

/// 规范时间（`YYYY-MM-DDTHH:MM:SSZ`）转 Unix 秒。
///
/// 只用于比较两个规范时间与计算凭据时长；调用前必须已通过
/// [`canonical_json::is_canonical_timestamp`]。不引入时区/闰秒库，也不做日历精确校验
/// （`2026-02-31` 会落到 3 月，形状合法但日期不存在属调用方输入问题）。
fn unix_seconds(text: &str) -> i64 {
    let number = |range: std::ops::Range<usize>| -> i64 { text[range].parse().unwrap_or(0) };
    let year = number(0..4);
    let month = number(5..7);
    let day = number(8..10);
    let hour = number(11..13);
    let minute = number(14..16);
    let second = number(17..19);
    days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
}

/// Howard Hinnant 的 `days_from_civil`：公历日期 → 1970-01-01 起的天数。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 规范时间比较：早于返回 true。两个输入都必须是规范时间。
fn earlier(left: &str, right: &str) -> bool {
    left < right
}

// ---------------------------------------------------------------------------
// 委任
// ---------------------------------------------------------------------------

/// 任命请求。**没有 epoch 字段**：epoch 由宿主按当前头指针 +1 分配，调用方无从自选（§19.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppointRequest {
    pub company_id: DomainId,
    pub scope: ManagementScope,
    pub manager_agent_id: String,
    pub grant_ref: String,
    pub appointed_by: String,
    pub appointed_at: String,
    pub expires_at: Option<String>,
    pub audience: String,
    pub commands: ManagementCommandSet,
}

impl AppointRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        company_id: impl Into<String>,
        scope: ManagementScope,
        manager_agent_id: impl Into<String>,
        grant_ref: impl Into<String>,
        appointed_by: impl Into<String>,
        appointed_at: impl Into<String>,
        expires_at: Option<String>,
        audience: impl Into<String>,
        commands: ManagementCommandSet,
    ) -> Self {
        Self {
            company_id: DomainId::new(company_id),
            scope,
            manager_agent_id: manager_agent_id.into(),
            grant_ref: grant_ref.into(),
            appointed_by: appointed_by.into(),
            appointed_at: appointed_at.into(),
            expires_at,
            audience: audience.into(),
            commands,
        }
    }

    fn validate(&self) -> Result<(), TransferError> {
        if self.company_id.as_str().trim().is_empty() {
            return Err(TransferError::EmptyField("company_id"));
        }
        self.scope.validate()?;
        if self.manager_agent_id.trim().is_empty() {
            return Err(TransferError::EmptyField("manager_agent_id"));
        }
        if self.grant_ref.trim().is_empty() {
            return Err(TransferError::EmptyField("grant_ref"));
        }
        if self.appointed_by.trim().is_empty() {
            return Err(TransferError::EmptyField("appointed_by"));
        }
        if self.audience.trim().is_empty() {
            return Err(TransferError::EmptyField("audience"));
        }
        if !canonical_json::is_canonical_timestamp(&self.appointed_at) {
            return Err(TransferError::NonCanonicalTimestamp(
                self.appointed_at.clone(),
            ));
        }
        if let Some(expires_at) = &self.expires_at {
            if !canonical_json::is_canonical_timestamp(expires_at) {
                return Err(TransferError::NonCanonicalTimestamp(expires_at.clone()));
            }
            if !earlier(&self.appointed_at, expires_at) {
                return Err(TransferError::AppointmentValidityNotPositive {
                    appointed_at: self.appointed_at.clone(),
                    expires_at: expires_at.clone(),
                });
            }
        }
        Ok(())
    }
}

/// 管理委任（§5.9）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementAppointment {
    pub appointment_id: String,
    pub company_id: DomainId,
    pub scope: ManagementScope,
    pub manager_agent_id: String,
    pub grant_ref: String,
    pub authority_epoch: u64,
    pub status: AppointmentStatus,
    pub appointed_by: String,
    pub appointed_at: String,
    pub expires_at: Option<String>,
    pub audience: String,
    pub commands: ManagementCommandSet,
    pub version: u64,
}

impl ManagementAppointment {
    /// 宿主分配的行 id：由 scope 与 epoch 决定，重试落回同一行（不做第二份委任）。
    pub fn id_for(scope: &ManagementScope, epoch: u64) -> String {
        format!("appointment:{}#{epoch}", scope.scope_key())
    }

    /// 任命。要求该 scope 当前没有 Active/Suspended 的主委任（I-58）。
    ///
    /// 写入顺序：先插委任行（获得 `(scope, epoch)` 唯一性检查），再用条件写更新头指针。
    /// 头指针才是权威——行存在但没人指向它时不可用，所以中途崩溃留下的是惰性行而不是第二主。
    pub fn appoint(
        store: &SqliteStore,
        request: AppointRequest,
        recorded_at_ms: i64,
    ) -> Result<Self, TransferError> {
        request.validate()?;
        let company = request.company_id.as_str().to_string();
        let scope_key = request.scope.scope_key();
        let head = store.get_scope_head(&company, &scope_key)?;

        let mut expected_head_version = None;
        let epoch;
        match &head {
            None => {
                epoch = 1;
            }
            Some(row) => {
                let current = store
                    .get_appointment(&company, &row.current_appointment_id)?
                    .ok_or_else(|| TransferError::CorruptScopeHead {
                        scope_key: scope_key.clone(),
                        detail: "头指针指向的委任行不存在".to_string(),
                    })?
                    .to_appointment()?;
                let effective = current.effective_status(&request.appointed_at);
                match effective {
                    AppointmentStatus::Active | AppointmentStatus::Suspended => {
                        return Err(TransferError::ScopeAlreadyManaged {
                            current_appointment_id: current.appointment_id.clone(),
                            authority_epoch: current.authority_epoch,
                        });
                    }
                    AppointmentStatus::Revoked | AppointmentStatus::Expired => {}
                }
                if current.status == AppointmentStatus::Active {
                    // 已过期但库里仍标 Active：先落 Expired，释放「一个 Active」的唯一索引。
                    let mut expired = current.clone();
                    expired.status = AppointmentStatus::Expired;
                    expired.version = current.version + 1;
                    if !store.update_appointment_cas(
                        &AppointmentRow::from_appointment(&expired, recorded_at_ms)?,
                        current.version,
                    )? {
                        return Err(TransferError::AppointmentConflict {
                            appointment_id: current.appointment_id.clone(),
                        });
                    }
                }
                epoch = next_epoch(row.authority_epoch, current.authority_epoch)?;
                expected_head_version = Some(row.version);
            }
        }

        let appointment = Self {
            appointment_id: Self::id_for(&request.scope, epoch),
            company_id: request.company_id.clone(),
            scope: request.scope.clone(),
            manager_agent_id: request.manager_agent_id.clone(),
            grant_ref: request.grant_ref.clone(),
            authority_epoch: epoch,
            status: AppointmentStatus::Active,
            appointed_by: request.appointed_by.clone(),
            appointed_at: request.appointed_at.clone(),
            expires_at: request.expires_at.clone(),
            audience: request.audience.clone(),
            commands: request.commands.clone(),
            version: 1,
        };

        let row = AppointmentRow::from_appointment(&appointment, recorded_at_ms)?;
        if !store.insert_appointment(&row)? {
            // 同 (scope, epoch) 已有一行：只接受内容完全一致的「上次崩溃留下的预约行」。
            let existing = store
                .get_appointment(&company, &appointment.appointment_id)?
                .ok_or_else(|| TransferError::AppointmentConflict {
                    appointment_id: appointment.appointment_id.clone(),
                })?;
            if existing != row {
                return Err(TransferError::AppointmentConflict {
                    appointment_id: appointment.appointment_id.clone(),
                });
            }
        }

        let head_row = ScopeHeadRow {
            company_id: company.clone(),
            scope_key: scope_key.clone(),
            current_appointment_id: appointment.appointment_id.clone(),
            authority_epoch: epoch,
            version: expected_head_version.map(|value| value + 1).unwrap_or(1),
            updated_at: request.appointed_at.clone(),
            recorded_at_ms,
        };
        let written = match expected_head_version {
            None => store.insert_scope_head(&head_row)?,
            Some(version) => store.update_scope_head_cas(&head_row, version)?,
        };
        if !written {
            // 并发任命或撤销抢先：本次委任行保持惰性，不成为当前主委任。
            return Err(TransferError::ScopeHeadConflict {
                scope_key: scope_key.clone(),
            });
        }
        Ok(appointment)
    }

    pub fn load(
        store: &SqliteStore,
        company_id: &str,
        appointment_id: &str,
    ) -> Result<Option<Self>, TransferError> {
        match store.get_appointment(company_id, appointment_id)? {
            Some(row) => Ok(Some(row.to_appointment()?)),
            None => Ok(None),
        }
    }

    /// 列出某 scope 的历史委任（epoch 升序），供核对「epoch 不重用」。
    pub fn list_for_scope(
        store: &SqliteStore,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<Self>, TransferError> {
        store
            .list_appointments(company_id, scope_key)?
            .iter()
            .map(AppointmentRow::to_appointment)
            .collect()
    }

    /// 生效状态：`expires_at` 已过时按 Expired 算，不依赖是否有人定时翻状态。
    ///
    /// 暂停中的委任同样会到期：到期即终态，不能靠「恢复」重新生效。
    pub fn effective_status(&self, at: &str) -> AppointmentStatus {
        let expirable = matches!(
            self.status,
            AppointmentStatus::Active | AppointmentStatus::Suspended
        );
        if expirable
            && let Some(expires_at) = &self.expires_at
            && !earlier(at, expires_at)
        {
            return AppointmentStatus::Expired;
        }
        self.status
    }

    pub fn allows(&self, command: ManagementCommand) -> bool {
        self.commands.allows(command)
    }

    /// 暂停：新管理命令停用，委任仍是当前主委任（不产生第二主）。
    ///
    /// 先写头指针（版本 +1 使缓存失效），再改状态行：即使第二步失败，缓存也已经不能再拿旧状态放行。
    pub fn suspend(
        &mut self,
        store: &SqliteStore,
        at: &str,
        expected_version: u64,
        recorded_at_ms: i64,
    ) -> Result<(), TransferError> {
        self.change_status(
            store,
            AppointmentStatus::Suspended,
            at,
            expected_version,
            recorded_at_ms,
            false,
        )
    }

    /// 恢复：只允许从 Suspended 回到 Active。撤销与到期是终态，不能恢复。
    pub fn resume(
        &mut self,
        store: &SqliteStore,
        at: &str,
        expected_version: u64,
        recorded_at_ms: i64,
    ) -> Result<(), TransferError> {
        if self.status == AppointmentStatus::Revoked {
            return Err(TransferError::RevokedIsTerminal);
        }
        if self.status == AppointmentStatus::Expired
            || self.effective_status(at) == AppointmentStatus::Expired
        {
            return Err(TransferError::AppointmentNotActive {
                status: AppointmentStatus::Expired,
            });
        }
        self.change_status(
            store,
            AppointmentStatus::Active,
            at,
            expected_version,
            recorded_at_ms,
            false,
        )
    }

    /// 撤任：终态，且**递增 epoch**，让撤任前签发的管理凭据当场失效（§19.8）。
    pub fn revoke(
        &mut self,
        store: &SqliteStore,
        at: &str,
        expected_version: u64,
        recorded_at_ms: i64,
    ) -> Result<(), TransferError> {
        if self.status == AppointmentStatus::Revoked {
            return Err(TransferError::RevokedIsTerminal);
        }
        self.change_status(
            store,
            AppointmentStatus::Revoked,
            at,
            expected_version,
            recorded_at_ms,
            true,
        )
    }

    fn change_status(
        &mut self,
        store: &SqliteStore,
        status: AppointmentStatus,
        at: &str,
        expected_version: u64,
        recorded_at_ms: i64,
        bump_epoch: bool,
    ) -> Result<(), TransferError> {
        if !canonical_json::is_canonical_timestamp(at) {
            return Err(TransferError::NonCanonicalTimestamp(at.to_string()));
        }
        if expected_version != self.version {
            return Err(TransferError::VersionConflict {
                expected: expected_version,
                actual: self.version,
            });
        }
        if self.status == AppointmentStatus::Revoked {
            return Err(TransferError::RevokedIsTerminal);
        }
        let company = self.company_id.as_str().to_string();
        let scope_key = self.scope.scope_key();
        let head = store.get_scope_head(&company, &scope_key)?.ok_or_else(|| {
            TransferError::CorruptScopeHead {
                scope_key: scope_key.clone(),
                detail: "委任存在但作用域头不存在".to_string(),
            }
        })?;

        // 头指针先行：版本必增，epoch 在撤任时也增。缓存以头版本为准，所以此后不会命中旧状态。
        let next_head = ScopeHeadRow {
            company_id: company.clone(),
            scope_key: scope_key.clone(),
            current_appointment_id: head.current_appointment_id.clone(),
            authority_epoch: if bump_epoch {
                next_epoch(head.authority_epoch, self.authority_epoch)?
            } else {
                head.authority_epoch
            },
            version: head.version + 1,
            updated_at: at.to_string(),
            recorded_at_ms,
        };
        if !store.update_scope_head_cas(&next_head, head.version)? {
            return Err(TransferError::VersionConflict {
                expected: head.version,
                actual: head.version + 1,
            });
        }

        let mut next = self.clone();
        next.status = status;
        next.version = self.version + 1;
        if !store.update_appointment_cas(
            &AppointmentRow::from_appointment(&next, recorded_at_ms)?,
            expected_version,
        )? {
            return Err(TransferError::VersionConflict {
                expected: expected_version,
                actual: self.version,
            });
        }
        *self = next;
        Ok(())
    }
}

/// 下一个 epoch：严格大于头指针与旧委任两者，绝不回退、绝不复用。
fn next_epoch(head_epoch: u64, appointment_epoch: u64) -> Result<u64, TransferError> {
    let base = head_epoch.max(appointment_epoch);
    let proposed = base
        .checked_add(1)
        .ok_or(TransferError::EpochNotMonotonic {
            previous: base,
            proposed: base,
        })?;
    Ok(proposed)
}

// ---------------------------------------------------------------------------
// 管理凭据（票证）
// ---------------------------------------------------------------------------

/// 凭据用途。管理票证恒为 `Management`，模块里不存在把它变成 `Execution` 的转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPurpose {
    Management,
    Execution,
}

/// 管理票证：绑定 Appointment / epoch / scope / 命令 / 时效 / audience（§31.10）。
///
/// 没有 `invocation_id`、`assignment_id` 字段——它不是员工执行凭据；执行凭据由执行协议按
/// invocation 签发，两者不能互相顶替。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementTicket {
    pub ticket_id: String,
    pub appointment_id: String,
    pub authority_epoch: u64,
    pub scope: ManagementScope,
    pub command: ManagementCommand,
    pub audience: String,
    pub issued_at: String,
    pub not_after: String,
}

impl ManagementTicket {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ticket_id: impl Into<String>,
        appointment_id: impl Into<String>,
        authority_epoch: u64,
        scope: ManagementScope,
        command: ManagementCommand,
        audience: impl Into<String>,
        issued_at: impl Into<String>,
        not_after: impl Into<String>,
    ) -> Self {
        Self {
            ticket_id: ticket_id.into(),
            appointment_id: appointment_id.into(),
            authority_epoch,
            scope,
            command,
            audience: audience.into(),
            issued_at: issued_at.into(),
            not_after: not_after.into(),
        }
    }

    /// 由已任命的委任签发一张短时票证：epoch 与 scope 直接取自委任，调用方无从改写。
    pub fn issue(
        appointment: &ManagementAppointment,
        command: ManagementCommand,
        issued_at: &str,
        not_after: &str,
    ) -> Result<Self, TransferError> {
        let ticket = Self::new(
            format!("ticket:{}#{}", appointment.appointment_id, command.as_str()),
            appointment.appointment_id.clone(),
            appointment.authority_epoch,
            appointment.scope.clone(),
            command,
            appointment.audience.clone(),
            issued_at,
            not_after,
        );
        ticket.validate()?;
        Ok(ticket)
    }

    pub fn validate(&self) -> Result<(), TransferError> {
        if self.ticket_id.trim().is_empty() {
            return Err(TransferError::EmptyField("ticket_id"));
        }
        if self.appointment_id.trim().is_empty() {
            return Err(TransferError::EmptyField("appointment_id"));
        }
        if self.audience.trim().is_empty() {
            return Err(TransferError::EmptyField("ticket.audience"));
        }
        if self.authority_epoch == 0 {
            return Err(TransferError::EmptyField("authority_epoch"));
        }
        self.scope.validate()?;
        if self.command.is_governance() {
            return Err(TransferError::GovernanceCommand(self.command));
        }
        if !canonical_json::is_canonical_timestamp(&self.issued_at) {
            return Err(TransferError::NonCanonicalTimestamp(self.issued_at.clone()));
        }
        if !canonical_json::is_canonical_timestamp(&self.not_after) {
            return Err(TransferError::NonCanonicalTimestamp(self.not_after.clone()));
        }
        if !earlier(&self.issued_at, &self.not_after) {
            return Err(TransferError::CredentialNotPositive {
                issued_at: self.issued_at.clone(),
                not_after: self.not_after.clone(),
            });
        }
        let ttl = unix_seconds(&self.not_after) - unix_seconds(&self.issued_at);
        if ttl > MAX_TICKET_SECONDS {
            return Err(TransferError::LongLivedCredential { seconds: ttl });
        }
        Ok(())
    }

    pub const fn purpose(&self) -> CredentialPurpose {
        CredentialPurpose::Management
    }

    /// 管理票证没有执行绑定，所以不能作为员工执行凭据使用。
    ///
    /// 这不是运行时开关，而是类型事实：本结构没有 invocation/assignment 字段，
    /// 执行路径需要的凭据无法从这里构造出来。
    pub const fn can_act_as_execution(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// 复核结果与缓存
// ---------------------------------------------------------------------------

/// 一次复核的审计结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    Accepted,
    Rejected,
}

impl CommandOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "accepted" => Some(Self::Accepted),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// 拒绝原因码。写进审计行，重放时据此还原同一个错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRejectionCode {
    /// 旧 epoch：切换或撤任之后仍用原管理凭据。
    StaleAppointment,
    /// epoch 比当前更高：宿主不签发未来 epoch，按伪造处理。
    UnknownEpoch,
    /// 委任非 Active（暂停/撤销/到期）。
    AppointmentInactive,
    /// 票证 scope 与委任 scope 不同。
    ScopeMismatch,
    /// 命令不在该委任可发集合内。
    CommandNotGranted,
    /// 票证受众与委任受众不同。
    AudienceMismatch,
    /// 凭据已过期。
    CredentialExpired,
    /// 凭据尚未生效。
    CredentialNotYetValid,
    /// 凭据有效期超过委任有效期。
    CredentialOutlivesAppointment,
    /// 作用域还没有委任。
    ScopeUnmanaged,
    /// 头指针与委任行不一致。
    CorruptScopeHead,
}

impl CommandRejectionCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaleAppointment => "stale_appointment",
            Self::UnknownEpoch => "unknown_epoch",
            Self::AppointmentInactive => "appointment_inactive",
            Self::ScopeMismatch => "scope_mismatch",
            Self::CommandNotGranted => "command_not_granted",
            Self::AudienceMismatch => "audience_mismatch",
            Self::CredentialExpired => "credential_expired",
            Self::CredentialNotYetValid => "credential_not_yet_valid",
            Self::CredentialOutlivesAppointment => "credential_outlives_appointment",
            Self::ScopeUnmanaged => "scope_unmanaged",
            Self::CorruptScopeHead => "corrupt_scope_head",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "stale_appointment" => Some(Self::StaleAppointment),
            "unknown_epoch" => Some(Self::UnknownEpoch),
            "appointment_inactive" => Some(Self::AppointmentInactive),
            "scope_mismatch" => Some(Self::ScopeMismatch),
            "command_not_granted" => Some(Self::CommandNotGranted),
            "audience_mismatch" => Some(Self::AudienceMismatch),
            "credential_expired" => Some(Self::CredentialExpired),
            "credential_not_yet_valid" => Some(Self::CredentialNotYetValid),
            "credential_outlives_appointment" => Some(Self::CredentialOutlivesAppointment),
            "scope_unmanaged" => Some(Self::ScopeUnmanaged),
            "corrupt_scope_head" => Some(Self::CorruptScopeHead),
            _ => None,
        }
    }
}

/// 复核通过的结果。**不携带任何执行绑定**，也不产生 Grant/Assignment。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCommand {
    pub command_id: String,
    pub appointment_id: String,
    pub authority_epoch: u64,
    pub scope: ManagementScope,
    pub command: ManagementCommand,
    pub audience: String,
    pub purpose: CredentialPurpose,
    pub verified_at: String,
    /// 同键同内容重放：没有产生第二次业务动作。
    pub replayed: bool,
    pub cache_state: CacheState,
}

impl VerifiedCommand {
    pub const fn can_act_as_execution(&self) -> bool {
        false
    }
}

/// 本次复核走了哪条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    /// 未启用缓存，每次都读库。
    Disabled,
    /// 命中缓存（仍然读了权威头行）。
    Hit,
    /// 未命中或头版本已变，重新读委任行。
    Reloaded,
}

/// 缓存统计，供测试与运维核对「缓存是否真失效」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub reloads: u64,
    pub invalidations: u64,
    pub flushes: u64,
    /// 每次提交都读一次权威头行的次数：缓存不能让宿主跳过当前委任查询。
    pub head_reads: u64,
    pub entries: usize,
}

#[derive(Debug, Clone)]
struct CachedAppointment {
    /// 缓存所对应的头指针版本。头版本一变，条目即不可用。
    head_version: u64,
    appointment: ManagementAppointment,
}

/// 有界的委任缓存。
#[derive(Debug, Default)]
pub struct AppointmentCache {
    entries: BTreeMap<String, CachedAppointment>,
    stats: CacheStats,
}

impl AppointmentCache {
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.len(),
            ..self.stats
        }
    }

    /// 显式失效某个作用域。返回是否真的删了条目。
    pub fn invalidate(&mut self, scope: &ManagementScope) -> bool {
        let removed = self.entries.remove(&scope.scope_key()).is_some();
        self.stats.invalidations += 1;
        removed
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ---------------------------------------------------------------------------
// 复核入口
// ---------------------------------------------------------------------------

/// 管理命令复核入口。持有 Company 与宿主受众；可选用缓存。
pub struct ManagementGate<'a> {
    store: &'a SqliteStore,
    company_id: DomainId,
    audience: String,
    cache: Option<AppointmentCache>,
}

impl<'a> ManagementGate<'a> {
    /// 不开缓存：每次提交读头行 + 读委任行。
    pub fn new(
        store: &'a SqliteStore,
        company_id: impl Into<String>,
        audience: impl Into<String>,
    ) -> Self {
        Self {
            store,
            company_id: DomainId::new(company_id),
            audience: audience.into(),
            cache: None,
        }
    }

    /// 开缓存：头版本未变时复用委任行，但每次仍读一次头行。
    pub fn with_cache(
        store: &'a SqliteStore,
        company_id: impl Into<String>,
        audience: impl Into<String>,
    ) -> Self {
        Self {
            cache: Some(AppointmentCache::default()),
            ..Self::new(store, company_id, audience)
        }
    }

    pub fn cache_stats(&self) -> Option<CacheStats> {
        self.cache.as_ref().map(AppointmentCache::stats)
    }

    pub fn invalidate(&mut self, scope: &ManagementScope) -> bool {
        match self.cache.as_mut() {
            Some(cache) => cache.invalidate(scope),
            None => false,
        }
    }

    /// 提交一条管理命令并当场复核当前委任。
    ///
    /// 顺序固定：幂等键比对 → 读权威头行 → 取委任（可用缓存）→ epoch/状态/scope/受众/时效/命令集。
    /// 任何拒绝都写审计行，然后返回同一个错误。
    pub fn submit(
        &mut self,
        ticket: &ManagementTicket,
        command_id: &str,
        at: &str,
        recorded_at_ms: i64,
    ) -> Result<VerifiedCommand, TransferError> {
        ticket.validate()?;
        if command_id.trim().is_empty() {
            return Err(TransferError::EmptyField("command_id"));
        }
        if !canonical_json::is_canonical_timestamp(at) {
            return Err(TransferError::NonCanonicalTimestamp(at.to_string()));
        }
        let company = self.company_id.as_str().to_string();
        let scope_key = ticket.scope.scope_key();

        // 幂等：同键同语义返回原结果；同键不同参数直接拒绝，不写第二条审计。
        if let Some(prior) = self.store.get_management_command(&company, command_id)? {
            let same = prior.appointment_id.as_deref() == Some(ticket.appointment_id.as_str())
                && prior.authority_epoch == Some(ticket.authority_epoch)
                && prior.command == ticket.command.as_str()
                && prior.scope_key == scope_key;
            if !same {
                return Err(TransferError::CommandKeyReuse {
                    command_id: command_id.to_string(),
                });
            }
            return match prior.outcome.as_str() {
                "accepted" => Ok(VerifiedCommand {
                    command_id: command_id.to_string(),
                    appointment_id: ticket.appointment_id.clone(),
                    authority_epoch: ticket.authority_epoch,
                    scope: ticket.scope.clone(),
                    command: ticket.command,
                    audience: ticket.audience.clone(),
                    purpose: CredentialPurpose::Management,
                    verified_at: prior.at.clone(),
                    replayed: true,
                    cache_state: CacheState::Disabled,
                }),
                "rejected" => {
                    let code = prior
                        .reason_code
                        .as_deref()
                        .and_then(CommandRejectionCode::parse)
                        .ok_or_else(|| TransferError::CorruptCommandAudit {
                            command_id: command_id.to_string(),
                        })?;
                    Err(TransferError::Rejected {
                        code,
                        detail: prior.reason.unwrap_or_default(),
                    })
                }
                other => Err(TransferError::CorruptCommandAudit {
                    command_id: format!("{command_id}（未知结论 {other}）"),
                }),
            };
        }

        // 缓存不能免掉这一步：当前委任必须每次查。
        let head = match self.store.get_scope_head(&company, &scope_key)? {
            Some(head) => head,
            None => {
                return Err(self.reject(
                    &scope_key,
                    command_id,
                    ticket,
                    CommandRejectionCode::ScopeUnmanaged,
                    format!("{} 尚无管理委任", ticket.scope.describe()),
                    at,
                    recorded_at_ms,
                )?);
            }
        };
        if let Some(cache) = self.cache.as_mut() {
            cache.stats.head_reads += 1;
        }

        let (appointment, cache_state) = self.resolve(&head, &scope_key)?;

        if ticket.authority_epoch < head.authority_epoch {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::StaleAppointment,
                format!(
                    "管理凭据 epoch {} 已失效，当前 epoch {}",
                    ticket.authority_epoch, head.authority_epoch
                ),
                at,
                recorded_at_ms,
            )?);
        }
        if ticket.authority_epoch > head.authority_epoch {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::UnknownEpoch,
                format!(
                    "epoch {} 高于当前 epoch {}；宿主不签发未来 epoch",
                    ticket.authority_epoch, head.authority_epoch
                ),
                at,
                recorded_at_ms,
            )?);
        }
        if ticket.appointment_id != head.current_appointment_id
            || appointment.appointment_id != head.current_appointment_id
        {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::StaleAppointment,
                format!(
                    "委任 {} 已不是当前主委任（当前 {}）",
                    ticket.appointment_id, head.current_appointment_id
                ),
                at,
                recorded_at_ms,
            )?);
        }

        let effective = appointment.effective_status(at);
        if effective != AppointmentStatus::Active {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::AppointmentInactive,
                format!("委任{}，不能发管理命令", effective.describe()),
                at,
                recorded_at_ms,
            )?);
        }
        if ticket.scope != appointment.scope {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::ScopeMismatch,
                format!(
                    "票证 scope {} 与委任 scope {} 不同",
                    ticket.scope.scope_key(),
                    appointment.scope.scope_key()
                ),
                at,
                recorded_at_ms,
            )?);
        }
        if self.audience != appointment.audience {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::AudienceMismatch,
                format!(
                    "宿主受众 {} 与委任受众 {} 不同",
                    self.audience, appointment.audience
                ),
                at,
                recorded_at_ms,
            )?);
        }
        if ticket.audience != appointment.audience {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::AudienceMismatch,
                format!(
                    "票证受众 {} 与委任受众 {} 不同",
                    ticket.audience, appointment.audience
                ),
                at,
                recorded_at_ms,
            )?);
        }
        if earlier(at, &ticket.issued_at) {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::CredentialNotYetValid,
                format!("凭据 {} 尚未生效", ticket.issued_at),
                at,
                recorded_at_ms,
            )?);
        }
        if !earlier(at, &ticket.not_after) {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::CredentialExpired,
                format!("凭据已于 {} 过期", ticket.not_after),
                at,
                recorded_at_ms,
            )?);
        }
        // 凭据有效期不得超过委任到期：否则撤任/到期之后这张票还能继续用。
        if let Some(expires_at) = &appointment.expires_at
            && earlier(expires_at, &ticket.not_after)
        {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::CredentialOutlivesAppointment,
                format!("凭据有效至 {}，超过委任到期 {expires_at}", ticket.not_after),
                at,
                recorded_at_ms,
            )?);
        }
        if !appointment.allows(ticket.command) {
            return Err(self.reject(
                &scope_key,
                command_id,
                ticket,
                CommandRejectionCode::CommandNotGranted,
                format!(
                    "命令 {} 不在委任可发集合（{}）",
                    ticket.command.as_str(),
                    appointment.commands.describe()
                ),
                at,
                recorded_at_ms,
            )?);
        }

        self.record(
            &scope_key,
            command_id,
            Some(&ticket.appointment_id),
            Some(ticket.authority_epoch),
            ticket.command,
            CommandOutcome::Accepted,
            None,
            Some(&format!(
                "命令 {} 复核通过（{}）",
                ticket.command.as_str(),
                ticket.scope.scope_key()
            )),
            at,
            recorded_at_ms,
        )?;
        Ok(VerifiedCommand {
            command_id: command_id.to_string(),
            appointment_id: ticket.appointment_id.clone(),
            authority_epoch: ticket.authority_epoch,
            scope: ticket.scope.clone(),
            command: ticket.command,
            audience: ticket.audience.clone(),
            purpose: CredentialPurpose::Management,
            verified_at: at.to_string(),
            replayed: false,
            cache_state,
        })
    }

    /// 取当前委任：缓存条目仅在头版本未变时可用。
    fn resolve(
        &mut self,
        head: &ScopeHeadRow,
        scope_key: &str,
    ) -> Result<(ManagementAppointment, CacheState), TransferError> {
        let company = self.company_id.as_str().to_string();
        if self.cache.is_none() {
            let appointment = self.load_current(head, &company, scope_key)?;
            return Ok((appointment, CacheState::Disabled));
        }
        // 命中判断只借不可变引用，判断结束再决定是否读库，避免与 `as_mut` 冲突。
        let hit = self.cache.as_ref().and_then(|cache| {
            cache
                .entries
                .get(scope_key)
                .filter(|entry| entry.head_version == head.version)
                .map(|entry| entry.appointment.clone())
        });
        if let Some(appointment) = hit {
            if let Some(cache) = self.cache.as_mut() {
                cache.stats.hits += 1;
            }
            return Ok((appointment, CacheState::Hit));
        }
        let appointment = self.load_current(head, &company, scope_key)?;
        if let Some(cache) = self.cache.as_mut() {
            cache.stats.misses += 1;
            cache.stats.reloads += 1;
            if cache.entries.len() >= MAX_CACHED_SCOPES && !cache.entries.contains_key(scope_key) {
                cache.entries.clear();
                cache.stats.flushes += 1;
            }
            cache.entries.insert(
                scope_key.to_string(),
                CachedAppointment {
                    head_version: head.version,
                    appointment: appointment.clone(),
                },
            );
        }
        Ok((appointment, CacheState::Reloaded))
    }

    fn load_current(
        &self,
        head: &ScopeHeadRow,
        company: &str,
        scope_key: &str,
    ) -> Result<ManagementAppointment, TransferError> {
        let appointment = self
            .store
            .get_appointment(company, &head.current_appointment_id)?
            .ok_or_else(|| TransferError::CorruptScopeHead {
                scope_key: scope_key.to_string(),
                detail: "头指针指向的委任行不存在".to_string(),
            })?;
        let appointment = appointment.to_appointment()?;
        // 撤任会先推进头 epoch 让旧票当场失效，此时头 epoch 比委任行大 1 是**正常**状态；
        // 只有头落后于委任（指针被回滚）才是损坏。
        if appointment.authority_epoch > head.authority_epoch
            || appointment.scope.scope_key() != scope_key
        {
            return Err(TransferError::CorruptScopeHead {
                scope_key: scope_key.to_string(),
                detail: format!(
                    "头 epoch {} 落后于委任 epoch {}（或 scope 不一致）",
                    head.authority_epoch, appointment.authority_epoch
                ),
            });
        }
        Ok(appointment)
    }

    #[allow(clippy::too_many_arguments)]
    fn reject(
        &self,
        scope_key: &str,
        command_id: &str,
        ticket: &ManagementTicket,
        code: CommandRejectionCode,
        detail: String,
        at: &str,
        recorded_at_ms: i64,
    ) -> Result<TransferError, TransferError> {
        self.record(
            scope_key,
            command_id,
            Some(&ticket.appointment_id),
            Some(ticket.authority_epoch),
            ticket.command,
            CommandOutcome::Rejected,
            Some(code),
            Some(&detail),
            at,
            recorded_at_ms,
        )?;
        Ok(TransferError::Rejected { code, detail })
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        scope_key: &str,
        command_id: &str,
        appointment_id: Option<&str>,
        epoch: Option<u64>,
        command: ManagementCommand,
        outcome: CommandOutcome,
        code: Option<CommandRejectionCode>,
        reason: Option<&str>,
        at: &str,
        recorded_at_ms: i64,
    ) -> Result<(), TransferError> {
        let row = ManagementCommandRow {
            company_id: self.company_id.as_str().to_string(),
            command_id: command_id.to_string(),
            scope_key: scope_key.to_string(),
            appointment_id: appointment_id.map(|value| value.to_string()),
            authority_epoch: epoch,
            command: command.as_str().to_string(),
            outcome: outcome.as_str().to_string(),
            reason_code: code.map(|value| value.as_str().to_string()),
            reason: reason.map(|value| value.to_string()),
            at: at.to_string(),
            recorded_at_ms,
        };
        if !self.store.insert_management_command(&row)? {
            // 并发同键：另一写入者已落审计，读回比对由上层幂等分支处理。
            return Err(TransferError::CommandKeyReuse {
                command_id: command_id.to_string(),
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 持久化行
// ---------------------------------------------------------------------------

/// `management_appointments` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppointmentRow {
    pub company_id: String,
    pub appointment_id: String,
    pub scope_key: String,
    pub manager_agent_id: String,
    pub grant_ref: String,
    pub authority_epoch: u64,
    pub status: String,
    pub appointed_by: String,
    pub appointed_at: String,
    pub expires_at: Option<String>,
    pub audience: String,
    pub commands_json: String,
    pub version: u64,
    pub recorded_at_ms: i64,
}

impl AppointmentRow {
    pub fn from_appointment(
        appointment: &ManagementAppointment,
        recorded_at_ms: i64,
    ) -> Result<Self, TransferError> {
        Ok(Self {
            company_id: appointment.company_id.as_str().to_string(),
            appointment_id: appointment.appointment_id.clone(),
            scope_key: appointment.scope.scope_key(),
            manager_agent_id: appointment.manager_agent_id.clone(),
            grant_ref: appointment.grant_ref.clone(),
            authority_epoch: appointment.authority_epoch,
            status: appointment.status.as_str().to_string(),
            appointed_by: appointment.appointed_by.clone(),
            appointed_at: appointment.appointed_at.clone(),
            expires_at: appointment.expires_at.clone(),
            audience: appointment.audience.clone(),
            commands_json: appointment.commands.to_json(),
            version: appointment.version,
            recorded_at_ms,
        })
    }

    pub fn to_appointment(&self) -> Result<ManagementAppointment, TransferError> {
        let status = AppointmentStatus::parse(&self.status).ok_or_else(|| {
            TransferError::CorruptAppointment {
                detail: format!("未知状态 {}", self.status),
            }
        })?;
        Ok(ManagementAppointment {
            appointment_id: self.appointment_id.clone(),
            company_id: DomainId::new(self.company_id.clone()),
            scope: ManagementScope::parse_key(&self.scope_key)?,
            manager_agent_id: self.manager_agent_id.clone(),
            grant_ref: self.grant_ref.clone(),
            authority_epoch: self.authority_epoch,
            status,
            appointed_by: self.appointed_by.clone(),
            appointed_at: self.appointed_at.clone(),
            expires_at: self.expires_at.clone(),
            audience: self.audience.clone(),
            commands: ManagementCommandSet::from_json(&self.commands_json)?,
            version: self.version,
        })
    }
}

/// `management_scope_heads` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeHeadRow {
    pub company_id: String,
    pub scope_key: String,
    pub current_appointment_id: String,
    pub authority_epoch: u64,
    pub version: u64,
    pub updated_at: String,
    pub recorded_at_ms: i64,
}

/// `management_commands` 行（命令审计）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementCommandRow {
    pub company_id: String,
    pub command_id: String,
    pub scope_key: String,
    pub appointment_id: Option<String>,
    pub authority_epoch: Option<u64>,
    pub command: String,
    pub outcome: String,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
    pub at: String,
    pub recorded_at_ms: i64,
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum TransferError {
    EmptyField(&'static str),
    NonCanonicalTimestamp(String),
    UnknownScopeKey(String),
    UnknownCommand {
        command: String,
    },
    EmptyCommandSet,
    DuplicateCommand(ManagementCommand),
    /// 治理命令不能写进委任命令集。
    GovernanceCommandNotGrantable(ManagementCommand),
    /// 管理票证不能声称治理命令。
    GovernanceCommand(ManagementCommand),
    ScopeAlreadyManaged {
        current_appointment_id: String,
        authority_epoch: u64,
    },
    EpochNotMonotonic {
        previous: u64,
        proposed: u64,
    },
    ScopeHeadConflict {
        scope_key: String,
    },
    AppointmentConflict {
        appointment_id: String,
    },
    AppointmentValidityNotPositive {
        appointed_at: String,
        expires_at: String,
    },
    CredentialNotPositive {
        issued_at: String,
        not_after: String,
    },
    LongLivedCredential {
        seconds: i64,
    },
    RevokedIsTerminal,
    AppointmentNotActive {
        status: AppointmentStatus,
    },
    VersionConflict {
        expected: u64,
        actual: u64,
    },
    CommandKeyReuse {
        command_id: String,
    },
    CorruptAppointment {
        detail: String,
    },
    CorruptCommandAudit {
        command_id: String,
    },
    CorruptScopeHead {
        scope_key: String,
        detail: String,
    },
    /// 复核拒绝：`code` 与写进审计行的原因码一致。
    Rejected {
        code: CommandRejectionCode,
        detail: String,
    },
    Store(StoreError),
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "{field} 不能为空"),
            Self::NonCanonicalTimestamp(value) => {
                write!(f, "时间 {value} 不是规范形式 YYYY-MM-DDTHH:MM:SSZ")
            }
            Self::UnknownScopeKey(value) => write!(f, "无法解析作用域键 {value}"),
            Self::UnknownCommand { command } => write!(f, "未知管理命令 {command}"),
            Self::EmptyCommandSet => write!(f, "委任命令集不能为空"),
            Self::DuplicateCommand(command) => {
                write!(f, "命令 {} 重复出现", command.as_str())
            }
            Self::GovernanceCommandNotGrantable(command) => write!(
                f,
                "治理命令 {} 不能授予委任：负责人不能任命或撤任自己",
                command.as_str()
            ),
            Self::GovernanceCommand(command) => {
                write!(f, "治理命令 {} 不能由委任凭据发出", command.as_str())
            }
            Self::ScopeAlreadyManaged {
                current_appointment_id,
                authority_epoch,
            } => write!(
                f,
                "该作用域已有在任委任 {current_appointment_id}（epoch {authority_epoch}）"
            ),
            Self::EpochNotMonotonic { previous, proposed } => {
                write!(f, "epoch 必须单调递增：当前 {previous}，提议 {proposed}")
            }
            Self::ScopeHeadConflict { scope_key } => {
                write!(f, "作用域头已变化，{scope_key} 的任命未提交")
            }
            Self::AppointmentConflict { appointment_id } => {
                write!(f, "委任 {appointment_id} 已存在且内容不同")
            }
            Self::AppointmentValidityNotPositive {
                appointed_at,
                expires_at,
            } => write!(f, "委任有效期非正：{appointed_at} → {expires_at}"),
            Self::CredentialNotPositive {
                issued_at,
                not_after,
            } => write!(f, "凭据有效期非正：{issued_at} → {not_after}"),
            Self::LongLivedCredential { seconds } => write!(
                f,
                "管理凭据有效期 {seconds}s 超过上限 {MAX_TICKET_SECONDS}s：短时凭据不能当长期密钥"
            ),
            Self::RevokedIsTerminal => write!(f, "已撤销的委任不能恢复，需要重新任命（新 epoch）"),
            Self::AppointmentNotActive { status } => {
                write!(f, "委任{}，不能执行该操作", status.describe())
            }
            Self::VersionConflict { expected, actual } => {
                write!(f, "版本冲突：期望 {expected}，实际 {actual}")
            }
            Self::CommandKeyReuse { command_id } => {
                write!(f, "命令键 {command_id} 已用于不同语义的命令")
            }
            Self::CorruptAppointment { detail } => write!(f, "委任行损坏：{detail}"),
            Self::CorruptCommandAudit { command_id } => {
                write!(f, "命令审计行损坏：{command_id}")
            }
            Self::CorruptScopeHead { scope_key, detail } => {
                write!(f, "作用域头损坏（{scope_key}）：{detail}")
            }
            Self::Rejected { code, detail } => {
                write!(f, "管理命令被拒（{}）：{detail}", code.as_str())
            }
            Self::Store(err) => write!(f, "存储错误：{err}"),
        }
    }
}

impl std::error::Error for TransferError {}

impl From<StoreError> for TransferError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const COMPANY: &str = "company-1";
    const AUDIENCE: &str = "host:local";
    const NOW_MS: i64 = 1_764_100_000_000;

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-manager-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
    }

    fn commands() -> ManagementCommandSet {
        ManagementCommandSet::new(vec![
            ManagementCommand::PlanRevise,
            ManagementCommand::AssignTask,
            ManagementCommand::ReadStatus,
            ManagementCommand::AcceptIntermediate,
            ManagementCommand::SubmitDelivery,
        ])
        .unwrap()
    }

    fn request(manager: &str) -> AppointRequest {
        AppointRequest::new(
            COMPANY,
            ManagementScope::project("project-1"),
            manager,
            "grant:manager-r2",
            "user-owner",
            "2026-11-20T12:00:00Z",
            Some("2027-01-01T00:00:00Z".to_string()),
            AUDIENCE,
            commands(),
        )
    }

    fn request_at(manager: &str, appointed_at: &str) -> AppointRequest {
        let mut item = request(manager);
        item.appointed_at = appointed_at.to_string();
        item
    }

    fn appointed(store: &SqliteStore) -> ManagementAppointment {
        ManagementAppointment::appoint(store, request("manager-a"), NOW_MS).unwrap()
    }

    /// 一张 15 分钟内有效的票证（正好等于上限）。
    fn ticket(
        appointment: &ManagementAppointment,
        command: ManagementCommand,
        at: &str,
    ) -> ManagementTicket {
        let not_after = "2026-11-20T12:20:00Z";
        ManagementTicket::issue(appointment, command, at, not_after).unwrap()
    }

    fn code_of(err: &TransferError) -> CommandRejectionCode {
        match err {
            TransferError::Rejected { code, .. } => *code,
            other => panic!("期望被拒，实际 {other:?}"),
        }
    }

    fn head(store: &SqliteStore, scope: &ManagementScope) -> ScopeHeadRow {
        store
            .get_scope_head(COMPANY, &scope.scope_key())
            .unwrap()
            .expect("头指针应存在")
    }

    #[test]
    fn appoint_assigns_host_epoch_and_blocks_a_second_active() {
        let root = temp_root("appoint");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let first = appointed(&store);

        assert_eq!(first.authority_epoch, 1);
        assert_eq!(first.status, AppointmentStatus::Active);
        assert_eq!(first.appointment_id, "appointment:project:project-1#1");
        let head_row = head(&store, &scope);
        assert_eq!(head_row.authority_epoch, 1);
        assert_eq!(head_row.current_appointment_id, first.appointment_id);
        assert_eq!(head_row.version, 1);

        // 同一 scope 不允许第二个在任委任：不是「覆盖」，是拒绝。
        let err = ManagementAppointment::appoint(&store, request("manager-b"), NOW_MS).unwrap_err();
        match err {
            TransferError::ScopeAlreadyManaged {
                current_appointment_id,
                authority_epoch,
            } => {
                assert_eq!(current_appointment_id, first.appointment_id);
                assert_eq!(authority_epoch, 1);
            }
            other => panic!("期望 ScopeAlreadyManaged，实际 {other:?}"),
        }
        assert_eq!(
            ManagementAppointment::list_for_scope(&store, COMPANY, &scope.scope_key())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn epoch_increases_on_each_generation_and_is_never_reused() {
        let root = temp_root("epochs");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");

        let mut a = appointed(&store);
        assert_eq!(a.authority_epoch, 1);
        a.revoke(&store, "2026-11-20T12:01:00Z", a.version, NOW_MS)
            .unwrap();
        // 撤任本身就把 epoch 推进一格：撤任前的票据当场失效。
        assert_eq!(head(&store, &scope).authority_epoch, 2);

        let mut b = ManagementAppointment::appoint(&store, request("manager-b"), NOW_MS).unwrap();
        assert_eq!(b.authority_epoch, 3);
        b.revoke(&store, "2026-11-20T12:02:00Z", b.version, NOW_MS)
            .unwrap();
        let c = ManagementAppointment::appoint(&store, request("manager-a"), NOW_MS).unwrap();
        // 回到原 Agent 也是新委任、新 epoch（§19.7 提交后失败不能回滚成旧 epoch）。
        // 每次撤任也推进一格（让旧票当场失效），所以是 1 → 3 → 5，不会回到 4。
        assert_eq!(c.authority_epoch, 5);

        let history =
            ManagementAppointment::list_for_scope(&store, COMPANY, &scope.scope_key()).unwrap();
        let epochs: Vec<u64> = history.iter().map(|item| item.authority_epoch).collect();
        assert_eq!(epochs, vec![1, 3, 5]);
        assert!(epochs.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn revoked_appointment_is_terminal_and_cannot_be_resumed() {
        let root = temp_root("revoked");
        let store = open_store(&root);
        let mut appointment = appointed(&store);
        appointment
            .revoke(&store, "2026-11-20T12:01:00Z", appointment.version, NOW_MS)
            .unwrap();
        assert_eq!(appointment.status, AppointmentStatus::Revoked);

        let err = appointment
            .resume(&store, "2026-11-20T12:02:00Z", appointment.version, NOW_MS)
            .unwrap_err();
        assert!(matches!(err, TransferError::RevokedIsTerminal), "{err}");
        let err = appointment
            .revoke(&store, "2026-11-20T12:02:00Z", appointment.version, NOW_MS)
            .unwrap_err();
        assert!(matches!(err, TransferError::RevokedIsTerminal), "{err}");

        // 库里状态也没被改回来。
        let reloaded = ManagementAppointment::load(&store, COMPANY, &appointment.appointment_id)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.status, AppointmentStatus::Revoked);
    }

    #[test]
    fn appoint_over_suspended_scope_is_rejected_until_revoked() {
        let root = temp_root("suspended-appoint");
        let store = open_store(&root);
        let mut appointment = appointed(&store);
        appointment
            .suspend(&store, "2026-11-20T12:01:00Z", appointment.version, NOW_MS)
            .unwrap();

        let err = ManagementAppointment::appoint(&store, request("manager-b"), NOW_MS).unwrap_err();
        assert!(
            matches!(err, TransferError::ScopeAlreadyManaged { .. }),
            "{err}"
        );

        appointment
            .revoke(&store, "2026-11-20T12:02:00Z", appointment.version, NOW_MS)
            .unwrap();
        let next = ManagementAppointment::appoint(&store, request("manager-b"), NOW_MS).unwrap();
        assert_eq!(next.authority_epoch, 3);
    }

    #[test]
    fn old_epoch_ticket_is_rejected_and_reason_is_recorded() {
        let root = temp_root("old-epoch");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let mut a = appointed(&store);
        let stale = ticket(&a, ManagementCommand::PlanRevise, "2026-11-20T12:05:00Z");

        // 换人之前这张票是有效的（对照组）。
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let first = gate
            .submit(&stale, "cmd-1", "2026-11-20T12:05:00Z", NOW_MS)
            .unwrap();
        assert!(!first.replayed);
        assert_eq!(first.authority_epoch, 1);

        a.revoke(&store, "2026-11-20T12:06:00Z", a.version, NOW_MS)
            .unwrap();
        let b = ManagementAppointment::appoint(&store, request("manager-b"), NOW_MS).unwrap();
        let before = head(&store, &scope);

        let err = gate
            .submit(&stale, "cmd-2", "2026-11-20T12:07:00Z", NOW_MS)
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::StaleAppointment);

        // 拒绝写进审计：原因码与当前 epoch 都在，可供界面说明。
        let audit = store
            .list_management_commands(COMPANY, &scope.scope_key())
            .unwrap();
        assert_eq!(audit.len(), 2);
        let rejection = audit.iter().find(|row| row.outcome == "rejected").unwrap();
        assert_eq!(rejection.reason_code.as_deref(), Some("stale_appointment"));
        assert!(
            rejection
                .reason
                .as_deref()
                .unwrap()
                .contains("当前 epoch 3")
        );

        // 当前计划/指针不变：头指针仍指向 B，epoch 不回退。
        let after = head(&store, &scope);
        assert_eq!(after.authority_epoch, before.authority_epoch);
        assert_eq!(after.version, before.version);
        assert_eq!(after.current_appointment_id, b.appointment_id);
    }

    #[test]
    fn epoch_higher_than_head_is_unknown_not_authority() {
        let root = temp_root("future-epoch");
        let store = open_store(&root);
        let appointment = appointed(&store);
        let mut forged = ticket(
            &appointment,
            ManagementCommand::ReadStatus,
            "2026-11-20T12:05:00Z",
        );
        forged.authority_epoch = 9;

        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let err = gate
            .submit(&forged, "cmd-forged", "2026-11-20T12:05:00Z", NOW_MS)
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::UnknownEpoch);
    }

    #[test]
    fn suspended_appointment_rejects_commands_until_resumed() {
        let root = temp_root("suspend");
        let store = open_store(&root);
        let mut appointment = appointed(&store);
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        gate.submit(
            &ticket(
                &appointment,
                ManagementCommand::ReadStatus,
                "2026-11-20T12:05:00Z",
            ),
            "cmd-1",
            "2026-11-20T12:05:00Z",
            NOW_MS,
        )
        .unwrap();

        appointment
            .suspend(&store, "2026-11-20T12:06:00Z", appointment.version, NOW_MS)
            .unwrap();
        let err = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::ReadStatus,
                    "2026-11-20T12:07:00Z",
                ),
                "cmd-2",
                "2026-11-20T12:07:00Z",
                NOW_MS,
            )
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::AppointmentInactive);

        appointment
            .resume(&store, "2026-11-20T12:08:00Z", appointment.version, NOW_MS)
            .unwrap();
        let ok = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::ReadStatus,
                    "2026-11-20T12:08:30Z",
                ),
                "cmd-3",
                "2026-11-20T12:08:30Z",
                NOW_MS,
            )
            .unwrap();
        assert_eq!(ok.authority_epoch, 1, "暂停与恢复不改变 epoch");
    }

    #[test]
    fn expired_appointment_is_effectively_inactive_and_replaceable() {
        let root = temp_root("expired");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let mut expired_request = request("manager-a");
        expired_request.expires_at = Some("2026-11-20T12:10:00Z".to_string());
        let appointment = ManagementAppointment::appoint(&store, expired_request, NOW_MS).unwrap();

        // 库里仍是 active，但过了 expires_at 就按到期处理。
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let err = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::ReadStatus,
                    "2026-11-20T12:11:00Z",
                ),
                "cmd-1",
                "2026-11-20T12:11:00Z",
                NOW_MS,
            )
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::AppointmentInactive);
        assert!(err_text_contains(&err, "已到期"));

        // 在 12:00 任命是不行的：那一刻旧委任还没到期（见 ScopeAlreadyManaged 语义）。
        let backdated = ManagementAppointment::appoint(&store, request("manager-b"), NOW_MS);
        assert!(
            matches!(backdated, Err(TransferError::ScopeAlreadyManaged { .. })),
            "{backdated:?}"
        );
        let next = ManagementAppointment::appoint(
            &store,
            request_at("manager-b", "2026-11-20T12:21:00Z"),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(next.authority_epoch, 2);
        let history =
            ManagementAppointment::list_for_scope(&store, COMPANY, &scope.scope_key()).unwrap();
        let statuses: Vec<AppointmentStatus> = history.iter().map(|item| item.status).collect();
        assert_eq!(
            statuses,
            vec![AppointmentStatus::Expired, AppointmentStatus::Active]
        );
    }

    fn err_text_contains(err: &TransferError, needle: &str) -> bool {
        let text = err.to_string();
        text.contains(needle)
    }

    #[test]
    fn warm_cache_does_not_authorize_after_revocation() {
        let root = temp_root("cache-revoke");
        let store = open_store(&root);
        let mut appointment = appointed(&store);
        let mut gate = ManagementGate::with_cache(&store, COMPANY, AUDIENCE);

        let first = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::ReadStatus,
                    "2026-11-20T12:05:00Z",
                ),
                "cmd-1",
                "2026-11-20T12:05:00Z",
                NOW_MS,
            )
            .unwrap();
        assert_eq!(first.cache_state, CacheState::Reloaded);
        let second = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::ReadStatus,
                    "2026-11-20T12:05:30Z",
                ),
                "cmd-2",
                "2026-11-20T12:05:30Z",
                NOW_MS,
            )
            .unwrap();
        assert_eq!(second.cache_state, CacheState::Hit);
        let warmed = gate.cache_stats().unwrap();
        assert_eq!(warmed.hits, 1);
        assert_eq!(warmed.head_reads, 2, "命中缓存也要读权威头行");

        appointment
            .revoke(&store, "2026-11-20T12:06:00Z", appointment.version, NOW_MS)
            .unwrap();
        let err = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::ReadStatus,
                    "2026-11-20T12:06:30Z",
                ),
                "cmd-3",
                "2026-11-20T12:06:30Z",
                NOW_MS,
            )
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::StaleAppointment);

        let after = gate.cache_stats().unwrap();
        assert_eq!(after.hits, 1, "撤任后不应再命中旧条目");
        assert_eq!(after.reloads, 2, "头版本变化后必须重读委任行");
        assert_eq!(after.head_reads, 3);
    }

    #[test]
    fn explicit_invalidation_forces_reload() {
        let root = temp_root("invalidate");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let appointment = appointed(&store);
        let mut gate = ManagementGate::with_cache(&store, COMPANY, AUDIENCE);
        let command = ManagementCommand::ReadStatus;

        gate.submit(
            &ticket(&appointment, command, "2026-11-20T12:05:00Z"),
            "cmd-1",
            "2026-11-20T12:05:00Z",
            NOW_MS,
        )
        .unwrap();
        let hit = gate
            .submit(
                &ticket(&appointment, command, "2026-11-20T12:05:10Z"),
                "cmd-2",
                "2026-11-20T12:05:10Z",
                NOW_MS,
            )
            .unwrap();
        assert_eq!(hit.cache_state, CacheState::Hit);

        assert!(gate.invalidate(&scope), "应为已缓存的 scope 删除条目");
        assert!(!gate.invalidate(&scope), "再删一次应当没有条目");
        let reloaded = gate
            .submit(
                &ticket(&appointment, command, "2026-11-20T12:05:20Z"),
                "cmd-3",
                "2026-11-20T12:05:20Z",
                NOW_MS,
            )
            .unwrap();
        assert_eq!(reloaded.cache_state, CacheState::Reloaded);
        let stats = gate.cache_stats().unwrap();
        assert_eq!(stats.invalidations, 2);
        assert_eq!(stats.reloads, 2);
    }

    #[test]
    fn audience_mismatch_is_rejected_on_both_sides() {
        let root = temp_root("audience");
        let store = open_store(&root);
        let appointment = appointed(&store);
        let command = ManagementCommand::ReadStatus;

        // 票证受众与委任不同。
        let mut wrong_ticket = ticket(&appointment, command, "2026-11-20T12:05:00Z");
        wrong_ticket.audience = "host:other".to_string();
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let err = gate
            .submit(&wrong_ticket, "cmd-1", "2026-11-20T12:05:00Z", NOW_MS)
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::AudienceMismatch);

        // 宿主受众与委任不同：同样拒绝，不能靠票证自称一致绕过。
        let mut other_host = ManagementGate::new(&store, COMPANY, "host:other");
        let err = other_host
            .submit(
                &ticket(&appointment, command, "2026-11-20T12:05:00Z"),
                "cmd-2",
                "2026-11-20T12:05:00Z",
                NOW_MS,
            )
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::AudienceMismatch);
    }

    #[test]
    fn command_outside_grant_is_rejected() {
        let root = temp_root("grant");
        let store = open_store(&root);
        let mut narrow = request("manager-a");
        narrow.commands = ManagementCommandSet::new(vec![ManagementCommand::ReadStatus]).unwrap();
        let appointment = ManagementAppointment::appoint(&store, narrow, NOW_MS).unwrap();

        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let err = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::AssignTask,
                    "2026-11-20T12:05:00Z",
                ),
                "cmd-1",
                "2026-11-20T12:05:00Z",
                NOW_MS,
            )
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::CommandNotGranted);
        assert!(err_text_contains(&err, "read_status"));
    }

    #[test]
    fn governance_commands_cannot_be_granted_nor_presented() {
        let err = ManagementCommandSet::new(vec![ManagementCommand::AppointManager]).unwrap_err();
        assert!(
            matches!(
                err,
                TransferError::GovernanceCommandNotGrantable(ManagementCommand::AppointManager)
            ),
            "{err}"
        );

        let root = temp_root("governance");
        let store = open_store(&root);
        let appointment = appointed(&store);
        let forged = ManagementTicket::new(
            "ticket-forged",
            &appointment.appointment_id,
            appointment.authority_epoch,
            appointment.scope.clone(),
            ManagementCommand::RevokeAppointment,
            AUDIENCE,
            "2026-11-20T12:05:00Z",
            "2026-11-20T12:10:00Z",
        );
        let err = forged.validate().unwrap_err();
        assert!(
            matches!(
                err,
                TransferError::GovernanceCommand(ManagementCommand::RevokeAppointment)
            ),
            "{err}"
        );
    }

    #[test]
    fn management_ticket_carries_no_execution_binding() {
        let root = temp_root("purpose");
        let store = open_store(&root);
        let appointment = appointed(&store);
        let ticket = ticket(
            &appointment,
            ManagementCommand::ReadStatus,
            "2026-11-20T12:05:00Z",
        );

        assert_eq!(ticket.purpose(), CredentialPurpose::Management);
        assert!(!ticket.can_act_as_execution());
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let verified = gate
            .submit(&ticket, "cmd-1", "2026-11-20T12:05:00Z", NOW_MS)
            .unwrap();
        assert_eq!(verified.purpose, CredentialPurpose::Management);
        assert!(!verified.can_act_as_execution());
    }

    #[test]
    fn long_lived_credentials_are_rejected() {
        let root = temp_root("ttl");
        let store = open_store(&root);
        let appointment = appointed(&store);

        // 正好 900 秒：允许。
        assert!(
            ManagementTicket::issue(
                &appointment,
                ManagementCommand::ReadStatus,
                "2026-11-20T12:05:00Z",
                "2026-11-20T12:20:00Z",
            )
            .is_ok()
        );
        // 901 秒：拒绝——管理凭据不能当长期密钥。
        let err = ManagementTicket::issue(
            &appointment,
            ManagementCommand::ReadStatus,
            "2026-11-20T12:05:00Z",
            "2026-11-20T12:20:01Z",
        )
        .unwrap_err();
        assert!(
            matches!(err, TransferError::LongLivedCredential { seconds: 901 }),
            "{err}"
        );

        let negative = ManagementTicket::new(
            "t",
            &appointment.appointment_id,
            1,
            appointment.scope.clone(),
            ManagementCommand::ReadStatus,
            AUDIENCE,
            "2026-11-20T12:10:00Z",
            "2026-11-20T12:10:00Z",
        );
        assert!(matches!(
            negative.validate().unwrap_err(),
            TransferError::CredentialNotPositive { .. }
        ));
    }

    #[test]
    fn credential_expired_or_not_yet_valid() {
        let root = temp_root("validity");
        let store = open_store(&root);
        let appointment = appointed(&store);
        let ticket = ManagementTicket::issue(
            &appointment,
            ManagementCommand::ReadStatus,
            "2026-11-20T12:05:00Z",
            "2026-11-20T12:10:00Z",
        )
        .unwrap();
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);

        let err = gate
            .submit(&ticket, "cmd-1", "2026-11-20T12:10:00Z", NOW_MS)
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::CredentialExpired);

        let err = gate
            .submit(&ticket, "cmd-2", "2026-11-20T12:04:00Z", NOW_MS)
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::CredentialNotYetValid);
    }

    #[test]
    fn credential_cannot_outlive_appointment() {
        let root = temp_root("outlive");
        let store = open_store(&root);
        let mut short = request("manager-a");
        short.expires_at = Some("2026-11-20T12:10:00Z".to_string());
        let appointment = ManagementAppointment::appoint(&store, short, NOW_MS).unwrap();
        let ticket = ManagementTicket::issue(
            &appointment,
            ManagementCommand::ReadStatus,
            "2026-11-20T12:05:00Z",
            "2026-11-20T12:15:00Z",
        )
        .unwrap();

        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let err = gate
            .submit(&ticket, "cmd-1", "2026-11-20T12:06:00Z", NOW_MS)
            .unwrap_err();
        assert_eq!(
            code_of(&err),
            CommandRejectionCode::CredentialOutlivesAppointment
        );
    }

    #[test]
    fn same_command_key_same_content_replays_without_second_audit() {
        let root = temp_root("idempotent");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let appointment = appointed(&store);
        let ticket = ticket(
            &appointment,
            ManagementCommand::PlanRevise,
            "2026-11-20T12:05:00Z",
        );
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);

        let first = gate
            .submit(&ticket, "cmd-same", "2026-11-20T12:05:00Z", NOW_MS)
            .unwrap();
        let again = gate
            .submit(&ticket, "cmd-same", "2026-11-20T12:06:00Z", NOW_MS)
            .unwrap();
        assert!(!first.replayed);
        assert!(again.replayed, "同键同内容应返回原结果");
        assert_eq!(again.authority_epoch, first.authority_epoch);
        assert_eq!(
            store
                .list_management_commands(COMPANY, &scope.scope_key())
                .unwrap()
                .len(),
            1,
            "重放不能写第二条审计"
        );
    }

    #[test]
    fn same_command_key_with_different_content_is_rejected() {
        let root = temp_root("key-reuse");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let appointment = appointed(&store);
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        gate.submit(
            &ticket(
                &appointment,
                ManagementCommand::PlanRevise,
                "2026-11-20T12:05:00Z",
            ),
            "cmd-key",
            "2026-11-20T12:05:00Z",
            NOW_MS,
        )
        .unwrap();

        let err = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::SubmitDelivery,
                    "2026-11-20T12:06:00Z",
                ),
                "cmd-key",
                "2026-11-20T12:06:00Z",
                NOW_MS,
            )
            .unwrap_err();
        assert!(
            matches!(err, TransferError::CommandKeyReuse { .. }),
            "{err}"
        );
        assert_eq!(
            store
                .list_management_commands(COMPANY, &scope.scope_key())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn rejected_command_leaves_scope_state_unchanged() {
        let root = temp_root("no-side-effect");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let mut appointment = appointed(&store);
        appointment
            .suspend(&store, "2026-11-20T12:01:00Z", appointment.version, NOW_MS)
            .unwrap();
        let before_head = head(&store, &scope);
        let before_row = store
            .get_appointment(COMPANY, &appointment.appointment_id)
            .unwrap()
            .unwrap();

        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let err = gate
            .submit(
                &ticket(
                    &appointment,
                    ManagementCommand::PlanRevise,
                    "2026-11-20T12:05:00Z",
                ),
                "cmd-rejected",
                "2026-11-20T12:05:00Z",
                NOW_MS,
            )
            .unwrap_err();
        assert_eq!(code_of(&err), CommandRejectionCode::AppointmentInactive);

        let after_head = head(&store, &scope);
        let after_row = store
            .get_appointment(COMPANY, &appointment.appointment_id)
            .unwrap()
            .unwrap();
        assert_eq!(after_head, before_head, "拒绝不得改动作用域头");
        assert_eq!(after_row, before_row, "拒绝不得改动委任状态");
        assert_eq!(
            store
                .list_management_commands(COMPANY, &scope.scope_key())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn scope_keys_round_trip_and_reject_garbage() {
        let project = ManagementScope::project("project-1");
        assert_eq!(project.scope_key(), "project:project-1");
        assert_eq!(
            ManagementScope::parse_key("project:project-1").unwrap(),
            project
        );

        let subtree = ManagementScope::issue_subtree("project-1", "issue-7");
        assert_eq!(subtree.scope_key(), "issue:project-1/issue-7");
        assert_eq!(
            ManagementScope::parse_key("issue:project-1/issue-7").unwrap(),
            subtree
        );
        // 专项与项目是两个作用域：专项不吞掉项目默认委任。
        assert_ne!(project.scope_key(), subtree.scope_key());

        assert!(matches!(
            ManagementScope::parse_key("weird:1").unwrap_err(),
            TransferError::UnknownScopeKey(_)
        ));
        assert!(matches!(
            ManagementScope::parse_key("issue:project-1").unwrap_err(),
            TransferError::UnknownScopeKey(_)
        ));
        assert!(matches!(
            ManagementScope::parse_key("project:").unwrap_err(),
            TransferError::EmptyField("scope.project_id")
        ));
    }

    #[test]
    fn appointment_rejects_empty_fields_and_bad_validity() {
        let root = temp_root("validate");
        let store = open_store(&root);
        let mut invalid = request(" ");
        assert!(matches!(
            ManagementAppointment::appoint(&store, invalid.clone(), NOW_MS).unwrap_err(),
            TransferError::EmptyField("manager_agent_id")
        ));

        invalid = request("manager-a");
        invalid.audience = String::new();
        assert!(matches!(
            ManagementAppointment::appoint(&store, invalid.clone(), NOW_MS).unwrap_err(),
            TransferError::EmptyField("audience")
        ));

        invalid = request("manager-a");
        invalid.appointed_at = "2026-11-20 12:00:00".to_string();
        assert!(matches!(
            ManagementAppointment::appoint(&store, invalid.clone(), NOW_MS).unwrap_err(),
            TransferError::NonCanonicalTimestamp(_)
        ));

        invalid = request("manager-a");
        invalid.expires_at = Some("2026-11-20T12:00:00Z".to_string());
        assert!(matches!(
            ManagementAppointment::appoint(&store, invalid, NOW_MS).unwrap_err(),
            TransferError::AppointmentValidityNotPositive { .. }
        ));

        assert!(matches!(
            ManagementCommandSet::new(Vec::new()).unwrap_err(),
            TransferError::EmptyCommandSet
        ));
        assert!(matches!(
            ManagementCommandSet::new(vec![
                ManagementCommand::ReadStatus,
                ManagementCommand::ReadStatus
            ])
            .unwrap_err(),
            TransferError::DuplicateCommand(ManagementCommand::ReadStatus)
        ));
    }

    #[test]
    fn store_refuses_second_active_or_reused_epoch() {
        let root = temp_root("unique");
        let store = open_store(&root);
        let scope = ManagementScope::project("project-1");
        let appointment = appointed(&store);
        let base = AppointmentRow::from_appointment(&appointment, NOW_MS).unwrap();

        // 同一 (scope, epoch) 的第二行：拒绝（epoch 不复用）。
        let mut reused = base.clone();
        reused.appointment_id = "appointment:other#1".to_string();
        assert!(!store.insert_appointment(&reused).unwrap());

        // 同一 scope 的第二个 Active（epoch 不同）：被部分唯一索引拒绝（I-58）。
        let mut second_active = base.clone();
        second_active.appointment_id = "appointment:project:project-1#2".to_string();
        second_active.authority_epoch = 2;
        assert!(!store.insert_appointment(&second_active).unwrap());

        assert_eq!(
            store
                .list_appointments(COMPANY, &scope.scope_key())
                .unwrap()
                .len(),
            1
        );
    }
}
