//! 应用授权入口（P0 D1 任务 2.1—2.3 + P5 任务 2.1—2.6）。
//! 只包装 bod-core 的纯规则，不访问 DB；命令层用同一入口做当前权复核。
//!
//! P5 部分把六阶段交付拆成"每段各自判一次"：提交、范围决定、分派、开发、评审验收、发布回滚、
//! 证明与复开。要点是**权限不互相派生**——范围批准权换不来分派权与预算修改权，接受权换不来发布权；
//! 一人兼任多项时逐项记录依据。其余四段边界：
//! - 服务端过滤（2.2）：先判可见再判可用；对不可见对象统一"不可见"，计数只等于可见条数（AZ01/AZ06）。
//! - 凭据边界（2.3）：执行绑定把 invocation、项目与租约钉在一起；跨项目兑换被拒，审计不含秘密内容（AZ02）。
//! - 验收独立（2.4）：执行者不能自关闭，测试人只能交证据，旧修订重放不推进状态（AZ04）。
//! - 发布锁定（2.5）：审批后改账号/制品/参数即拦，被拦时连接器调用计数为零（AZ05）。
//! - 撤权生效（2.6）：立即阻止新准入，在途发停止请求，但不删项目资产、不改写历史归属。

use crate::config::BodConfig;
use crate::domain::DomainId;
use crate::domain::model::*;

pub use crate::domain::model::{
    Assignment, Denial, DenialReason, Grant, Owner, OwnerKind, Permission, Principal,
    PrincipalKind, Scope,
};
pub use crate::domain::rules::{assign_issue, authorize, authorize_feedback};

/// 首次启动提供的默认 Company（§identity-access 默认 Company 与归属）。
pub fn default_company() -> Company {
    Company {
        id: DomainId::new("company-default"),
        display_name: "Local Company".to_string(),
    }
}

/// 默认部署边界：只接受回环地址；远程访问须显式认证与 TLS，另由部署任务处理。
pub fn is_loopback_listen(listen: &str) -> bool {
    let host = listen
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(listen)
        .trim_matches(['[', ']']);
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

pub fn deployment_boundary(config: &BodConfig) -> Result<(), Denial> {
    let scope = Scope {
        company_id: default_company().id,
        project_id: None,
        issue_id: None,
    };
    if is_loopback_listen(&config.server.listen) {
        Ok(())
    } else {
        Err(Denial {
            reason: DenialReason::InsufficientPermission,
            actor: DomainId::new("deployment"),
            scope: Box::new(scope),
            grant_revision: None,
            reactivation: "显式启用认证与 TLS 后才允许非回环监听".to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// P5 2.1 六阶段动作矩阵
// ---------------------------------------------------------------------------

/// 六阶段交付里的具体动作。评审单列，因为它只产出证据、不推进状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageAction {
    /// 提交需求或补充需求。
    Submit,
    /// 范围决定（改范围、改验收条件）。
    ManageScope,
    /// 创建 Assignment（分派）。
    Dispatch,
    /// 开发与进度回报。
    Develop,
    /// 测试与评审：只交证据。
    Review,
    /// 验收：推进状态。
    Accept,
    /// 发布。
    Release,
    /// 回滚。
    Rollback,
    /// 证明页确认或复开。
    Prove,
    Reopen,
}

impl StageAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::ManageScope => "manage_scope",
            Self::Dispatch => "dispatch",
            Self::Develop => "develop",
            Self::Review => "review",
            Self::Accept => "accept",
            Self::Release => "release",
            Self::Rollback => "rollback",
            Self::Prove => "prove",
            Self::Reopen => "reopen",
        }
    }

    /// 从落库文本反解。审计表存的是 `as_str` 的结果，读回时必须能还原——
    /// 否则审计只能看不能算（按动作聚合、按动作过滤都做不了）。
    ///
    /// 未知文本返回 `None` 而不是默认值：库里出现表外动作，说明有人绕过应用写库，
    /// 读侧应该炸出来，不该把它当成某个已知动作。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "submit" => Some(Self::Submit),
            "manage_scope" => Some(Self::ManageScope),
            "dispatch" => Some(Self::Dispatch),
            "develop" => Some(Self::Develop),
            "review" => Some(Self::Review),
            "accept" => Some(Self::Accept),
            "release" => Some(Self::Release),
            "rollback" => Some(Self::Rollback),
            "prove" => Some(Self::Prove),
            "reopen" => Some(Self::Reopen),
            _ => None,
        }
    }
}

/// 一个动作需要什么才能做。**跨段不派生**：`ScopeApproval` 不等于 `Dispatch`，也不等于预算权。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageRequirement {
    /// 项目成员身份。
    Membership,
    /// 范围批准权（`Permission::ManageScope`）。
    ScopeApproval,
    /// 分派权（`Permission::Dispatch`）。
    Dispatch,
    /// 被指派为执行者。
    Work,
    /// 接受权（`Permission::Accept`），且不是执行者本人。
    Acceptance,
    /// 发布操作人名单。
    Release,
}

impl StageAction {
    pub fn requirement(self) -> StageRequirement {
        match self {
            Self::Submit | Self::Review | Self::Prove => StageRequirement::Membership,
            Self::ManageScope => StageRequirement::ScopeApproval,
            Self::Dispatch => StageRequirement::Dispatch,
            Self::Develop => StageRequirement::Work,
            Self::Accept | Self::Reopen => StageRequirement::Acceptance,
            Self::Release | Self::Rollback => StageRequirement::Release,
        }
    }
}

/// 授权事实快照。命令层每次动作前重新取，不用缓存里的旧 Grant。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageFacts {
    pub grants: Vec<Grant>,
    pub assignments: Vec<Assignment>,
    /// 项目成员：(project_id, principal)。成员身份只够提交与评审。
    pub members: Vec<(String, DomainId)>,
    /// 发布/回滚操作人。外部账号与连接器另由发布记录核对。
    pub release_operators: Vec<DomainId>,
    /// 客户协作者绑定：主体 → 客户 id。只影响可见范围，不产生动作权。
    pub customer_bindings: Vec<(DomainId, String)>,
    /// 最近一次撤权的修订号；离线重放早于它的要重新同步。
    pub revoked_at_revision: Option<u64>,
}

impl StageFacts {
    pub fn has_active_grant(&self, principal: &DomainId, permission: Permission) -> bool {
        self.grants.iter().any(|grant| {
            grant.principal_id == *principal && grant.permission == permission && grant.active
        })
    }

    fn is_member(&self, principal: &DomainId, project: Option<&DomainId>) -> bool {
        let Some(project) = project else {
            return false;
        };
        self.members
            .iter()
            .any(|(project_id, member)| member == principal && project_id == project.as_str())
    }

    fn customer_of(&self, principal: &DomainId) -> Option<&str> {
        self.customer_bindings
            .iter()
            .find(|(id, _)| id == principal)
            .map(|(_, customer)| customer.as_str())
    }
}

/// 动作作用的对象。`revision` 与 `current_revision` 分开，用来挡旧修订重放。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageTarget {
    pub scope: Scope,
    pub assignee: Option<DomainId>,
    pub revision: u64,
    pub current_revision: u64,
}

impl StageTarget {
    pub fn new(scope: Scope, revision: u64) -> Self {
        Self {
            scope,
            assignee: None,
            revision,
            current_revision: revision,
        }
    }

    pub fn with_assignee(mut self, assignee: DomainId) -> Self {
        self.assignee = Some(assignee);
        self
    }

    pub fn with_current_revision(mut self, current_revision: u64) -> Self {
        self.current_revision = current_revision;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageDenial {
    NotAMember,
    MissingGrant,
    NotAssignee,
    NotReleaseOperator,
    ExecutorCannotAccept,
    StaleRevision,
}

impl StageDenial {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotAMember => "not_a_member",
            Self::MissingGrant => "missing_grant",
            Self::NotAssignee => "not_assignee",
            Self::NotReleaseOperator => "not_release_operator",
            Self::ExecutorCannotAccept => "executor_cannot_accept",
            Self::StaleRevision => "stale_revision",
        }
    }
}

/// 拒绝也要留痕：谁、要做什么、作用在哪个范围、依据是什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageAudit {
    pub actor: DomainId,
    pub action: StageAction,
    pub scope: Scope,
    pub basis: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageDecision {
    Allowed {
        basis: String,
        audit: StageAudit,
    },
    Denied {
        reason: StageDenial,
        audit: StageAudit,
    },
}

impl StageDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    pub fn audit(&self) -> &StageAudit {
        match self {
            Self::Allowed { audit, .. } | Self::Denied { audit, .. } => audit,
        }
    }
}

/// 六阶段判定。顺序固定：先挡旧修订，再看该动作要求的凭据，不跨段借用。
pub fn authorize_stage(
    facts: &StageFacts,
    actor: &Principal,
    action: StageAction,
    target: &StageTarget,
    at: &str,
) -> StageDecision {
    let audit = |basis: String| StageAudit {
        actor: actor.id.clone(),
        action,
        scope: target.scope.clone(),
        basis,
        at: at.to_string(),
    };
    if target.revision != target.current_revision {
        return StageDecision::Denied {
            reason: StageDenial::StaleRevision,
            audit: audit(format!(
                "动作针对修订 {}，当前修订 {}，旧修订不得重放",
                target.revision, target.current_revision
            )),
        };
    }
    match action.requirement() {
        StageRequirement::Membership => {
            if facts.is_member(&actor.id, target.scope.project_id.as_ref()) {
                StageDecision::Allowed {
                    basis: "project_membership".to_string(),
                    audit: audit("项目成员".to_string()),
                }
            } else {
                StageDecision::Denied {
                    reason: StageDenial::NotAMember,
                    audit: audit("不是该项目成员".to_string()),
                }
            }
        }
        StageRequirement::ScopeApproval => {
            if facts.has_active_grant(&actor.id, Permission::ManageScope) {
                StageDecision::Allowed {
                    basis: "grant:manage_scope".to_string(),
                    audit: audit("范围批准权".to_string()),
                }
            } else {
                StageDecision::Denied {
                    reason: StageDenial::MissingGrant,
                    audit: audit("缺范围批准权".to_string()),
                }
            }
        }
        StageRequirement::Dispatch => {
            if facts.has_active_grant(&actor.id, Permission::Dispatch) {
                StageDecision::Allowed {
                    basis: "grant:dispatch".to_string(),
                    audit: audit("分派权".to_string()),
                }
            } else {
                StageDecision::Denied {
                    reason: StageDenial::MissingGrant,
                    audit: audit("缺分派权（范围批准权不派生分派权）".to_string()),
                }
            }
        }
        StageRequirement::Work => {
            if target.assignee.as_ref() == Some(&actor.id) {
                StageDecision::Allowed {
                    basis: "assignment:owner".to_string(),
                    audit: audit("本人是被指派执行者".to_string()),
                }
            } else {
                StageDecision::Denied {
                    reason: StageDenial::NotAssignee,
                    audit: audit("不是该对象的执行者".to_string()),
                }
            }
        }
        StageRequirement::Acceptance => {
            if target.assignee.as_ref() == Some(&actor.id) {
                return StageDecision::Denied {
                    reason: StageDenial::ExecutorCannotAccept,
                    audit: audit("执行者本人不能接受自己的成果".to_string()),
                };
            }
            if facts.has_active_grant(&actor.id, Permission::Accept) {
                StageDecision::Allowed {
                    basis: "grant:accept".to_string(),
                    audit: audit("接受权且非执行者".to_string()),
                }
            } else {
                StageDecision::Denied {
                    reason: StageDenial::MissingGrant,
                    audit: audit("缺接受权".to_string()),
                }
            }
        }
        StageRequirement::Release => {
            if facts.release_operators.contains(&actor.id) {
                StageDecision::Allowed {
                    basis: "release_operator".to_string(),
                    audit: audit("发布操作人".to_string()),
                }
            } else {
                StageDecision::Denied {
                    reason: StageDenial::NotReleaseOperator,
                    audit: audit("不在发布操作人名单".to_string()),
                }
            }
        }
    }
}

/// 预算权单列：范围批准权、分派权都不派生它（AZ03 的另一半）。
pub fn may_manage_budget(facts: &StageFacts, actor: &Principal) -> bool {
    facts.has_active_grant(&actor.id, Permission::ManageBudget)
}

// ---------------------------------------------------------------------------
// P5 2.2 服务端过滤（AZ01 / AZ06）
// ---------------------------------------------------------------------------

/// 一条待过滤记录的范围与归属。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedRecord {
    pub id: String,
    pub scope: Scope,
    /// 客户目标维度。**客户是交付目标，不是数据所有权边界**（V2.9.9 产品勘误 1）：
    /// 同一 Company 内的客户共用一个库、一个宿主写入者；需要物理隔离时用独立部署与
    /// 操作系统隔离，不在这个字段上分库、也不由它决定谁能读整张表。
    /// 它只参与阶段四的目标粒度判定（`target_visible`）。
    pub customer_id: Option<String>,
    pub assignee: Option<DomainId>,
}

impl ScopedRecord {
    pub fn new(id: &str, scope: Scope) -> Self {
        Self {
            id: id.to_string(),
            scope,
            customer_id: None,
            assignee: None,
        }
    }

    pub fn for_customer(mut self, customer_id: &str) -> Self {
        self.customer_id = Some(customer_id.to_string());
        self
    }

    pub fn assigned_to(mut self, assignee: DomainId) -> Self {
        self.assignee = Some(assignee);
        self
    }
}

/// 过滤结果。`total` 只等于可见条数：不给"还有几条别人的"这种差异信号。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilteredList {
    pub visible: Vec<String>,
    pub total: usize,
    /// 因为没有权限而完全看不到的条数——只进日志，不进响应。
    pub withheld: usize,
}

/// 返回前过滤。客户协作者只能看自己客户的记录；成员/执行者按项目与指派判定。
pub fn filter_records(
    facts: &StageFacts,
    actor: &Principal,
    records: &[ScopedRecord],
) -> FilteredList {
    let mut filtered = FilteredList::default();
    for record in records {
        if record_visible(facts, actor, record) {
            filtered.visible.push(record.id.clone());
        } else {
            filtered.withheld += 1;
        }
    }
    filtered.total = filtered.visible.len();
    filtered
}

/// §8.1 阶段二：**对象可见性**——这个主体在记录所属范围内有没有立场。
///
/// 只看「有没有立场」，不看「是哪一个目标」。客户协作者的立场来自绑定，因此其可见域
/// 只包含带客户维度的记录：内部记录属于团队，不属于任何客户目标。至于绑定的是哪个客户，
/// 属于阶段四（`target_visible`）。
///
/// 两阶段分开的理由是可分别断言：合成一步时，「内部记录不该被客户看到」与
/// 「兄弟客户的记录不该被看到」在测试里只能观察到同一个结果，也就无法证明是哪一条在起作用。
fn is_visible(facts: &StageFacts, actor: &Principal, record: &ScopedRecord) -> bool {
    if facts.customer_of(&actor.id).is_some() {
        return record.customer_id.is_some();
    }
    if record.assignee.as_ref() == Some(&actor.id) {
        return true;
    }
    facts.is_member(&actor.id, record.scope.project_id.as_ref())
}

/// §8.1 阶段四：**目标粒度**——立场成立之后，这个具体目标是否允许。
///
/// 客户协作者只见本客户的目标，父项目与兄弟客户的目标都在这一步被排除；
/// 其余角色不受目标粒度限制（他们的范围已由阶段二的项目成员与指派决定）。
///
/// 这一步不重新判断立场：没有客户维度的记录不是任何客户的目标，粒度不适用，
/// 留给阶段二挡。若在这里也判 false，两阶段的失败原因会混在一起，
/// 将来放宽其中一条时没人能说清是哪一条在起作用。
///
/// 只与 `is_visible` 合起来用（`record_visible` 是唯一组合点）。
///
/// 入参是**客户维度本身**而不是整条记录：列表过滤（`filter_records`）要用它判记录的
/// 客户字段，而 GET 这类读路径要对**每个目标**单独判一次目标粒度，两者都只需要这一个值。
/// 传 `&ScopedRecord` 会逼着调用方为每个目标现造一条假记录。
pub fn target_visible(facts: &StageFacts, actor: &Principal, customer_id: Option<&str>) -> bool {
    match facts.customer_of(&actor.id) {
        Some(customer) => match customer_id {
            Some(target) => target == customer,
            None => true,
        },
        None => true,
    }
}

fn record_visible(facts: &StageFacts, actor: &Principal, record: &ScopedRecord) -> bool {
    is_visible(facts, actor, record) && target_visible(facts, actor, record.customer_id.as_deref())
}

/// 对外拒绝码（V2.9.9 §8.1）。三个码是契约：客户端按码分支，改名等于改契约。
///
/// 四个判定阶段只产生三种拒绝——阶段二与阶段四共用 `NotFound`，因为两者对外的表现
/// 必须**完全相同**：一旦「存在但无权」与「不存在」可分，枚举就成立了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordRejection {
    /// 主体确认未通过。与对象无关，不得因对象不同而改码。
    ApiDenied,
    /// 对象不可见（含目标粒度不通过）。与「不存在」同答。
    NotFound,
    /// 可见但缺动作权限。提示改主体或申请授权。
    ScopeDenied,
}

impl RecordRejection {
    /// `const`：路由表的 401 码直接由它派生，两处字面量不许各写一遍。
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::ApiDenied => "BOD_API_DENIED",
            Self::NotFound => "BOD_NOT_FOUND",
            Self::ScopeDenied => "BOD_SCOPE_DENIED",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            Self::ApiDenied => 401,
            Self::NotFound => 404,
            Self::ScopeDenied => 403,
        }
    }

    /// 对外一句话。`ScopeDenied` 可以说明「改主体或申请授权」；
    /// `NotFound` 不能补任何细节——补了就泄漏。
    pub fn message(self) -> &'static str {
        match self {
            Self::ApiDenied => "未通过主体确认",
            Self::NotFound => "未找到该对象",
            Self::ScopeDenied => "当前主体在该范围没有所需权限；可更换主体或申请授权",
        }
    }
}

/// §8.1 阶段一：主体确认。未认证时结论与对象无关。
pub fn confirm_subject(authenticated: bool) -> Result<(), RecordRejection> {
    if authenticated {
        Ok(())
    } else {
        Err(RecordRejection::ApiDenied)
    }
}

/// §8.1 阶段二～四的对外结论：`access_record` 的结果 → 码。
///
/// 单独一个函数而不是让 handler 各自 `match`：三档码的分档规则只有一处，
/// 改档位时不会有一半路由跟着漂。
pub fn classify_access(access: &RecordAccess) -> Result<(), RecordRejection> {
    match access {
        RecordAccess::Invisible => Err(RecordRejection::NotFound),
        RecordAccess::Allowed(decision) if decision.is_allowed() => Ok(()),
        RecordAccess::Allowed(_) => Err(RecordRejection::ScopeDenied),
    }
}

/// 判定链的对外结论，顺序即 §8.1 的表：主体 → 可见 → 动作 → 目标粒度。
///
/// **主体先于对象**是这里唯一有顺序要求的地方：未认证请求的答案必须与对象无关，
/// 否则拿同一个坏凭据去打两个对象、看返回码是否不同，就能判断哪个对象存在。
pub fn rejection(authenticated: bool, access: &RecordAccess) -> Option<RecordRejection> {
    confirm_subject(authenticated)
        .err()
        .or_else(|| classify_access(access).err())
}

/// 一条**容器记录**（如跟踪项）的读结论。§8.1 阶段二在容器上的三种结果。
///
/// 为什么不是 `Result<(), RecordRejection>`：容器的可见性对客户协作者不是「过或不过」，
/// 而是「过一部分」——跟踪项本身不带客户维度，客户维度在它下面的目标上。
/// 用布尔表达会丢掉「按目标放行」这一档，调用方就只能自己去查客户绑定，
/// 判定规则于是有了第二份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRead {
    /// 容器整体可见：内部成员（项目成员）或记录负责人。
    Whole,
    /// 只按目标粒度放行。客户协作者属于这一档：他们对容器没有立场
    /// （`is_visible` 对无客户维度的记录恒为假），可见范围由目标决定。
    ByTarget,
    /// 不可见。对外一律 `NotFound`。
    Halted,
}

/// §8.1 容器记录的读判定：阶段二（立场）+ 阶段四（目标粒度），**没有阶段三**。
///
/// 读没有动作。`StageAction` 的十个变体各自回答「改这件事需要什么凭据」，
/// 往里塞一个 `Read` 会立刻逼出「读取需要 Dispatch 凭据吗」这类没有答案的问题——
/// 这正是 ADR-0007 第 1 条禁止扩这个枚举的原因。所以读不发 `StageAction`，
/// 也不查 `grants`：能看见就能读。
///
/// `Halted` 由调用方翻成 `NotFound`，与 `classify_access(&RecordAccess::Invisible)` 同档——
/// 读路径上「存在但无权」必须与「不存在」同答，否则返回码本身就成了枚举工具。
/// `ScopeDenied` 那一档只属于命令，不属于读。
pub fn read_container(
    facts: &StageFacts,
    actor: &Principal,
    record: &ScopedRecord,
) -> ContainerRead {
    if facts.customer_of(&actor.id).is_some() {
        return ContainerRead::ByTarget;
    }
    if record_visible(facts, actor, record) {
        ContainerRead::Whole
    } else {
        ContainerRead::Halted
    }
}

/// 能看到 ≠ 能做。可见但缺动作权限返回拒绝，而不是"不可见"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordAccess {
    Allowed(StageDecision),
    Invisible,
}

/// 「这个主体满足这个需求吗」——只判需求本身，不判可见性与修订：那两件由调用方先判过，
/// 在这里重判会让「看不到」和「没权限」又混成一句。
///
/// 交付域动作（`link-source` / `draft`）只需要项目成员身份，所以目前只实现 `Membership`，
/// 其余需求一律 `false`（**fail-closed**）。将来有动作需要批准权或发布权时，必须在这里
/// 加分支；届时的代价是「少显示一个动作」，而不是「对所有人放行」——
/// 静默放行会让门看起来装上了，这比少给一颗按钮危险得多。
pub fn requirement_met(
    facts: &StageFacts,
    actor: &Principal,
    requirement: StageRequirement,
    project: Option<&DomainId>,
) -> bool {
    match requirement {
        StageRequirement::Membership => {
            project.is_some_and(|project| facts.is_member(&actor.id, Some(project)))
        }
        _ => false,
    }
}

pub fn access_record(
    facts: &StageFacts,
    actor: &Principal,
    record: &ScopedRecord,
    action: StageAction,
    target: &StageTarget,
    at: &str,
) -> RecordAccess {
    if !record_visible(facts, actor, record) {
        return RecordAccess::Invisible;
    }
    RecordAccess::Allowed(authorize_stage(facts, actor, action, target, at))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionOutcome {
    Allowed,
    /// 撤权之后续传：从新基线重新同步，不补发旧事件。
    ResyncRequired {
        from: u64,
    },
    Denied,
}

/// 事件订阅续传与离线重放都受新授权约束。
pub fn check_subscription(
    facts: &StageFacts,
    actor: &Principal,
    scope: &Scope,
    since_revision: u64,
    offline: bool,
) -> SubscriptionOutcome {
    if !facts.is_member(&actor.id, scope.project_id.as_ref())
        && facts.customer_of(&actor.id).is_none()
    {
        return SubscriptionOutcome::Denied;
    }
    if let Some(revoked_at) = facts.revoked_at_revision
        && offline
        && since_revision < revoked_at
    {
        return SubscriptionOutcome::ResyncRequired { from: revoked_at };
    }
    SubscriptionOutcome::Allowed
}

// ---------------------------------------------------------------------------
// P5 2.3 执行绑定与凭据边界（AZ02）
// ---------------------------------------------------------------------------

/// 一次执行的绑定：invocation、项目与租约必须同时成立。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionBinding {
    pub invocation_id: String,
    pub project_id: DomainId,
    pub lease_id: String,
    pub lease_expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub ref_id: String,
    pub project_id: DomainId,
    /// 只用来断言"审计不包含秘密内容"，不参与任何判定输出。
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretAudit {
    pub invocation_id: String,
    pub project_id: String,
    pub secret_ref: String,
    pub outcome: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretDenial {
    CrossProject,
    LeaseMissing,
    LeaseExpired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretOutcome {
    Granted {
        audit: SecretAudit,
    },
    Denied {
        reason: SecretDenial,
        audit: SecretAudit,
    },
}

/// 兑换凭据引用。项目不同即拒，租约空或过期即拒；审计只有引用 id，没有值。
pub fn resolve_secret(binding: &ExecutionBinding, secret: &SecretRef, now: &str) -> SecretOutcome {
    let audit = |outcome: &'static str| SecretAudit {
        invocation_id: binding.invocation_id.clone(),
        project_id: binding.project_id.as_str().to_string(),
        secret_ref: secret.ref_id.clone(),
        outcome,
    };
    if binding.project_id != secret.project_id {
        return SecretOutcome::Denied {
            reason: SecretDenial::CrossProject,
            audit: audit("denied_cross_project"),
        };
    }
    if binding.lease_id.trim().is_empty() {
        return SecretOutcome::Denied {
            reason: SecretDenial::LeaseMissing,
            audit: audit("denied_lease_missing"),
        };
    }
    if binding.lease_expires_at.as_str() <= now {
        return SecretOutcome::Denied {
            reason: SecretDenial::LeaseExpired,
            audit: audit("denied_lease_expired"),
        };
    }
    SecretOutcome::Granted {
        audit: audit("granted"),
    }
}

// ---------------------------------------------------------------------------
// P5 2.4 验收独立性（AZ04）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorRole {
    Executor,
    Tester,
    Acceptor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    TestReport,
    ManualReview,
    CustomerSignoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceRequest {
    pub actor: DomainId,
    pub role: ActorRole,
    pub revision: u64,
    pub current_revision: u64,
    pub evidence: Vec<EvidenceKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceRefusal {
    /// 执行者自关闭。
    ExecutorCannotAccept,
    /// 测试人只能交证据，不能接受。
    TesterCannotAccept,
    /// 接受权缺失。
    MissingAcceptanceRight,
    /// 旧修订重放。
    StaleRevision,
    /// 无证据。
    NoEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptanceVerdict {
    Advance { basis: String },
    Refused(AcceptanceRefusal),
}

/// 验收判定。四条拒绝互不替代：角色、独立性、权限、修订各判一次。
pub fn check_acceptance(
    facts: &StageFacts,
    request: &AcceptanceRequest,
    assignee: Option<&DomainId>,
) -> AcceptanceVerdict {
    if request.revision != request.current_revision {
        return AcceptanceVerdict::Refused(AcceptanceRefusal::StaleRevision);
    }
    if request.role == ActorRole::Tester {
        return AcceptanceVerdict::Refused(AcceptanceRefusal::TesterCannotAccept);
    }
    if assignee == Some(&request.actor) {
        return AcceptanceVerdict::Refused(AcceptanceRefusal::ExecutorCannotAccept);
    }
    if !facts.has_active_grant(&request.actor, Permission::Accept) {
        return AcceptanceVerdict::Refused(AcceptanceRefusal::MissingAcceptanceRight);
    }
    if request.evidence.is_empty() {
        return AcceptanceVerdict::Refused(AcceptanceRefusal::NoEvidence);
    }
    AcceptanceVerdict::Advance {
        basis: "accept_grant+independent+current_revision".to_string(),
    }
}

/// 测试人可以提交证据，但不能推进状态。
pub fn submit_evidence(facts: &StageFacts, actor: &Principal, scope: &Scope) -> bool {
    facts.is_member(&actor.id, scope.project_id.as_ref())
}

// ---------------------------------------------------------------------------
// P5 2.5 发布账号与参数锁定（AZ05）
// ---------------------------------------------------------------------------

/// 审批时钉住的发布票：目标、制品、参数、账号、有效期。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseTicket {
    pub environment: String,
    pub artifact_digest: String,
    pub params_digest: String,
    pub account_id: String,
    pub connector_id: String,
    pub approved_until: String,
}

/// 调用前的当前事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseContext {
    pub environment: String,
    pub artifact_digest: String,
    pub params_digest: String,
    pub account_id: String,
    pub connector_id: String,
    pub now: String,
    pub authorization_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseBlock {
    TargetChanged,
    ArtifactChanged,
    ParamsChanged,
    AccountChanged,
    ConnectorChanged,
    AuthorizationRevoked,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseVerdict {
    /// 放行：`connector_calls` 是这次允许发出的调用数（恒为 1）。
    Dispatch { connector_calls: u32 },
    /// 被拦：连接器调用数必须为 0，已发动作转对账。
    Blocked {
        reason: ReleaseBlock,
        connector_calls: u32,
    },
}

/// 待发动作前的最后一道核对。任何一项变了都拦，且拦下时不允许发生连接器调用。
pub fn check_release(ticket: &ReleaseTicket, context: &ReleaseContext) -> ReleaseVerdict {
    let blocked = |reason: ReleaseBlock| ReleaseVerdict::Blocked {
        reason,
        connector_calls: 0,
    };
    if !context.authorization_active {
        return blocked(ReleaseBlock::AuthorizationRevoked);
    }
    if context.now.as_str() >= ticket.approved_until.as_str() {
        return blocked(ReleaseBlock::Expired);
    }
    if context.environment != ticket.environment {
        return blocked(ReleaseBlock::TargetChanged);
    }
    if context.artifact_digest != ticket.artifact_digest {
        return blocked(ReleaseBlock::ArtifactChanged);
    }
    if context.params_digest != ticket.params_digest {
        return blocked(ReleaseBlock::ParamsChanged);
    }
    if context.account_id != ticket.account_id {
        return blocked(ReleaseBlock::AccountChanged);
    }
    if context.connector_id != ticket.connector_id {
        return blocked(ReleaseBlock::ConnectorChanged);
    }
    ReleaseVerdict::Dispatch { connector_calls: 1 }
}

// ---------------------------------------------------------------------------
// P5 2.6 撤权与历史归属保留
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChange {
    /// 成员退出项目。
    Leave,
    /// Agent 停用。
    DisableAgent,
    /// Catalog 卸载。
    UninstallCatalog,
}

/// 撤权结果。资产与历史都不动；只有无法撤销的外部凭据进对账。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationOutcome {
    pub blocked_new: bool,
    /// 在途执行的停止请求，状态为"已请求、待确认"。
    pub in_flight: Vec<String>,
    pub in_flight_state: &'static str,
    /// 恒为空：撤权不删项目资产。
    pub assets_deleted: Vec<String>,
    /// 恒为 `false`：不改写历史分配。
    pub history_rewritten: bool,
    pub reconciliation: Vec<String>,
}

/// 撤权：停用全部 Grant、移出成员名单，保留 Assignment 与资产。
pub fn revoke_member(
    facts: &mut StageFacts,
    member: &DomainId,
    change: MembershipChange,
    in_flight: &[String],
    unrecoverable_external: &[String],
) -> RevocationOutcome {
    for grant in facts.grants.iter_mut() {
        if grant.principal_id == *member {
            grant.active = false;
        }
    }
    facts.members.retain(|(_, id)| id != member);
    facts.release_operators.retain(|id| id != member);
    facts.customer_bindings.retain(|(id, _)| id != member);

    let _ = change;
    RevocationOutcome {
        blocked_new: facts
            .grants
            .iter()
            .filter(|grant| grant.principal_id == *member)
            .all(|grant| !grant.active),
        in_flight: in_flight.to_vec(),
        in_flight_state: "stop_requested_awaiting_confirmation",
        assets_deleted: Vec::new(),
        history_rewritten: false,
        reconciliation: unrecoverable_external.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Version;

    fn scope() -> Scope {
        Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new("project-1")),
            issue_id: Some(DomainId::new("issue-1")),
        }
    }

    fn agent(id: &str) -> Principal {
        Principal {
            id: DomainId::new(id),
            kind: PrincipalKind::Agent,
            claimed_roles: vec!["admin".to_string()],
        }
    }

    fn grants(revision: u64, active: bool) -> Vec<Grant> {
        vec![
            Grant {
                principal_id: DomainId::new("coder-1"),
                scope: scope(),
                permission: Permission::Dispatch,
                revision,
                active,
            },
            Grant {
                principal_id: DomainId::new("user-1"),
                scope: scope(),
                permission: Permission::ManageScope,
                revision: 1,
                active: true,
            },
        ]
    }

    fn issue(kind: IssueKind) -> Issue {
        Issue {
            meta: ResourceMeta {
                uid: DomainId::new("issue-1"),
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind,
            repository: Some(RepositoryContext {
                provider: "local".to_string(),
                repository_id: "repo-1".to_string(),
            }),
            spec: IssueSpec {
                title: "实现登录".to_string(),
                assignment: None,
                acceptance: AcceptanceCriteria {
                    criteria: vec!["测试通过".to_string()],
                },
            },
            status: ResourceStatus {
                phase: Phase::Pending,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        }
    }

    #[test]
    fn task_2_1_assign_without_team() {
        let manager = Principal {
            id: DomainId::new("user-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let assigned = assign_issue(
            &issue(IssueKind::Coding),
            Owner {
                kind: OwnerKind::Agent,
                id: DomainId::new("coder-1"),
            },
            7,
            &manager,
            &grants(7, true),
        )
        .expect("无 Team 配置也应能直接指派");
        let assignment = assigned.spec.assignment.expect("应生成 Assignment");
        assert_eq!(assignment.owner.id.as_str(), "coder-1");
        assert_eq!(assignment.grant_revision, 7);
        assert_eq!(assigned.meta.resource_version, Version(2));
    }

    #[test]
    fn task_2_1_coding_issue_requires_context() {
        let mut invalid = issue(IssueKind::Coding);
        invalid.repository = None;
        let manager = Principal {
            id: DomainId::new("user-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let err = assign_issue(
            &invalid,
            Owner {
                kind: OwnerKind::Agent,
                id: DomainId::new("coder-1"),
            },
            7,
            &manager,
            &grants(7, true),
        )
        .expect_err("缺仓库上下文的 Coding Issue 必须被拒绝");
        assert_eq!(err.reason, DenialReason::MissingProjectContext);
        assert!(!err.reactivation.is_empty());
    }

    mod invariant_i_09 {
        use super::*;

        #[test]
        fn revoked_grant_blocks_dispatch_and_records_current_revision() {
            let assignment = Assignment {
                owner: Owner {
                    kind: OwnerKind::Agent,
                    id: DomainId::new("coder-1"),
                },
                scope: scope(),
                grant_revision: 7,
            };
            let err = authorize(
                &agent("coder-1"),
                &scope(),
                Permission::Dispatch,
                &grants(8, false),
                Some(&assignment),
            )
            .expect_err("撤权后必须拒绝");
            assert_eq!(err.reason, DenialReason::InsufficientPermission);
            assert_eq!(err.actor.as_str(), "coder-1");
            assert!(!err.reactivation.is_empty());
        }

        #[test]
        fn claimed_admin_role_does_not_grant_permission() {
            let err = authorize(
                &agent("coder-1"),
                &scope(),
                Permission::ManageBudget,
                &grants(7, true),
                None,
            )
            .expect_err("自报管理员不得扩权");
            assert_eq!(err.reason, DenialReason::InsufficientPermission);
        }

        #[test]
        fn stale_assignment_is_rejected_after_reissue() {
            let assignment = Assignment {
                owner: Owner {
                    kind: OwnerKind::Agent,
                    id: DomainId::new("coder-1"),
                },
                scope: scope(),
                grant_revision: 5,
            };
            let err = authorize(
                &agent("coder-1"),
                &scope(),
                Permission::Dispatch,
                &grants(7, true),
                Some(&assignment),
            )
            .expect_err("旧 revision 的 Binding 不得认领新工作");
            assert_eq!(err.reason, DenialReason::InsufficientPermission);
            assert_eq!(err.grant_revision, Some(7));
        }
    }

    mod invariant_i_12 {
        use super::*;

        #[test]
        fn feedback_payload_cannot_claim_another_project() {
            let other = Scope {
                project_id: Some(DomainId::new("project-2")),
                ..scope()
            };
            let err = authorize_feedback(
                &scope(),
                &other,
                &agent("coder-1"),
                Permission::Dispatch,
                &grants(7, true),
                None,
            )
            .expect_err("payload 自报项目不得扩权");
            assert_eq!(err.reason, DenialReason::ScopeMismatch);
        }

        #[test]
        fn invocation_bound_scope_is_used_not_payload_scope() {
            let granted = authorize_feedback(
                &scope(),
                &scope(),
                &agent("coder-1"),
                Permission::Dispatch,
                &grants(7, true),
                Some(&Assignment {
                    owner: Owner {
                        kind: OwnerKind::Agent,
                        id: DomainId::new("coder-1"),
                    },
                    scope: scope(),
                    grant_revision: 7,
                }),
            )
            .expect("绑定范围内应通过");
            assert_eq!(granted.actor.as_str(), "coder-1");
        }
    }

    mod task_2_3 {
        use super::*;

        #[test]
        fn default_listen_is_loopback_and_remote_is_rejected() {
            let config = BodConfig::default();
            assert!(deployment_boundary(&config).is_ok());
            assert!(is_loopback_listen("127.0.0.1:0"));
            assert!(is_loopback_listen("[::1]:8080"));
            assert!(!is_loopback_listen("0.0.0.0:8080"));

            let mut remote = config;
            remote.server.listen = "0.0.0.0:8080".to_string();
            let denial = deployment_boundary(&remote).expect_err("非回环监听必须被拒绝");
            assert!(!denial.reactivation.is_empty());
            assert_eq!(denial.scope.company_id.as_str(), "company-default");
        }
    }
}

#[cfg(test)]
mod stage_permission_tests {
    use super::*;

    fn scope(project: &str, issue: &str) -> Scope {
        Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new(project)),
            issue_id: Some(DomainId::new(issue)),
        }
    }

    fn user(id: &str) -> Principal {
        Principal {
            id: DomainId::new(id),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    fn grant(principal: &str, permission: Permission, active: bool) -> Grant {
        Grant {
            principal_id: DomainId::new(principal),
            scope: scope("project-1", "issue-1"),
            permission,
            revision: 1,
            active,
        }
    }

    fn facts() -> StageFacts {
        StageFacts {
            grants: Vec::new(),
            assignments: Vec::new(),
            members: vec![
                ("project-1".to_string(), DomainId::new("user-owner")),
                ("project-1".to_string(), DomainId::new("user-dev")),
                ("project-1".to_string(), DomainId::new("user-tester")),
            ],
            release_operators: Vec::new(),
            customer_bindings: Vec::new(),
            revoked_at_revision: None,
        }
    }

    fn target() -> StageTarget {
        StageTarget::new(scope("project-1", "issue-1"), 5).with_assignee(DomainId::new("user-dev"))
    }

    const AT: &str = "2026-11-21T10:00:00Z";

    /// 审计表存的是 `as_str`，读回必须还原；漏一个会导致审计只能看不能算。
    #[test]
    fn stage_action_text_round_trips() {
        for action in [
            StageAction::Submit,
            StageAction::ManageScope,
            StageAction::Dispatch,
            StageAction::Develop,
            StageAction::Review,
            StageAction::Accept,
            StageAction::Release,
            StageAction::Rollback,
            StageAction::Prove,
            StageAction::Reopen,
        ] {
            assert_eq!(StageAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(
            StageAction::parse("approve"),
            None,
            "表外动作不得回退成默认值"
        );
    }

    /// 2.1 / AZ03：只有范围批准权的人分派或追加预算都被拒，且拒绝留痕。
    #[test]
    fn task_2_1_scope_approval_does_not_imply_dispatch_or_budget() {
        let mut facts = facts();
        facts
            .grants
            .push(grant("user-owner", Permission::ManageScope, true));
        let actor = user("user-owner");

        let dispatch = authorize_stage(&facts, &actor, StageAction::Dispatch, &target(), AT);
        match &dispatch {
            StageDecision::Denied { reason, audit } => {
                assert_eq!(*reason, StageDenial::MissingGrant);
                assert_eq!(audit.actor.as_str(), "user-owner");
                assert_eq!(audit.action, StageAction::Dispatch);
                assert_eq!(
                    audit.scope.project_id.as_ref().map(DomainId::as_str),
                    Some("project-1")
                );
                assert!(!audit.basis.is_empty(), "拒绝必须写依据");
            }
            other => panic!("只有范围批准权不得分派，实得 {other:?}"),
        }
        assert!(!may_manage_budget(&facts, &actor), "范围批准权不派生预算权");

        // 补上分派权才放行，且依据写明用的是哪一项。
        facts
            .grants
            .push(grant("user-owner", Permission::Dispatch, true));
        let allowed = authorize_stage(&facts, &actor, StageAction::Dispatch, &target(), AT);
        assert!(allowed.is_allowed());
        match allowed {
            StageDecision::Allowed { basis, .. } => assert_eq!(basis, "grant:dispatch"),
            _ => unreachable!(),
        }
        assert!(!may_manage_budget(&facts, &actor), "分派权同样不派生预算权");
    }

    /// 2.1：接受权与发布权互不派生；一人兼任时逐项写出依据。
    #[test]
    fn task_2_1_acceptance_and_release_are_independent_rights() {
        let mut facts = facts();
        facts
            .grants
            .push(grant("user-tester", Permission::Accept, true));
        let actor = user("user-tester");

        assert!(
            authorize_stage(&facts, &actor, StageAction::Accept, &target(), AT).is_allowed(),
            "有接受权且非执行者应可接受"
        );
        let release = authorize_stage(&facts, &actor, StageAction::Release, &target(), AT);
        assert!(
            matches!(
                release,
                StageDecision::Denied {
                    reason: StageDenial::NotReleaseOperator,
                    ..
                }
            ),
            "接受权不得派生发布权"
        );

        facts.release_operators.push(DomainId::new("user-tester"));
        assert!(
            authorize_stage(&facts, &actor, StageAction::Release, &target(), AT).is_allowed(),
            "进入发布名单后才可发布"
        );
        // 两项权利分别留痕。
        let accept_basis = match authorize_stage(&facts, &actor, StageAction::Accept, &target(), AT)
        {
            StageDecision::Allowed { basis, .. } => basis,
            other => panic!("应放行，实得 {other:?}"),
        };
        let release_basis =
            match authorize_stage(&facts, &actor, StageAction::Release, &target(), AT) {
                StageDecision::Allowed { basis, .. } => basis,
                other => panic!("应放行，实得 {other:?}"),
            };
        assert_ne!(accept_basis, release_basis, "兼任也要逐项记录依据");
    }

    /// 2.1：执行者本人即使有接受权也不能自接受。
    #[test]
    fn task_2_1_executor_with_accept_grant_is_still_refused() {
        let mut facts = facts();
        facts
            .grants
            .push(grant("user-dev", Permission::Accept, true));
        let decision = authorize_stage(
            &facts,
            &user("user-dev"),
            StageAction::Accept,
            &target(),
            AT,
        );
        assert!(matches!(
            decision,
            StageDecision::Denied {
                reason: StageDenial::ExecutorCannotAccept,
                ..
            }
        ));
    }

    /// 2.2 / AZ06：客户协作者枚举不到父项目与兄弟工单，计数不暴露差异。
    #[test]
    fn task_2_2_customer_cannot_enumerate_sibling_issues() {
        let mut facts = facts();
        facts
            .customer_bindings
            .push((DomainId::new("user-customer-a"), "customer-a".to_string()));
        let records = vec![
            ScopedRecord::new("issue-a", scope("project-1", "issue-a")).for_customer("customer-a"),
            ScopedRecord::new("issue-b", scope("project-1", "issue-b")).for_customer("customer-b"),
            ScopedRecord::new("issue-parent", scope("project-1", "issue-parent")),
        ];
        let filtered = filter_records(&facts, &user("user-customer-a"), &records);
        assert_eq!(filtered.visible, vec!["issue-a".to_string()]);
        assert_eq!(filtered.total, 1, "计数只等于可见条数");
        assert_eq!(filtered.withheld, 2, "被扣数量只进日志");

        // 客户即使能看到自己的记录，也不能拿成员的提交权。
        let access = access_record(
            &facts,
            &user("user-customer-a"),
            &records[0],
            StageAction::ManageScope,
            &StageTarget::new(records[0].scope.clone(), 1),
            AT,
        );
        match access {
            RecordAccess::Allowed(decision) => assert!(
                matches!(
                    decision,
                    StageDecision::Denied {
                        reason: StageDenial::MissingGrant,
                        ..
                    }
                ),
                "可见但缺动作权限应返回拒绝"
            ),
            RecordAccess::Invisible => panic!("自己的记录应可见"),
        }
    }

    /// 2.2 / AZ01：不可见与可见但禁止是两种返回，不混成一种。
    /// 容器记录的三档读结论各有归属，且**不靠客户维度决定容器本身可不可见**。
    ///
    /// 这条用例守的是一个很容易写错的边界：跟踪项这类容器不带客户维度，
    /// 客户协作者对它的 `is_visible` 恒为假。若把它当成「不可见」，
    /// 客户协作者就永远读不到自己的目标；若把它当成「可见」，
    /// 内部记录又会对所有客户敞开。正确答案是第三档。
    #[test]
    fn container_read_is_three_way_and_customer_access_is_by_target() {
        let mut facts = facts();
        facts
            .customer_bindings
            .push((DomainId::new("user-customer-a"), "customer-a".to_string()));
        let container = ScopedRecord::new("tracking-1", scope("project-1", "tracking-1"));

        // 内部成员：整体可见。
        assert_eq!(
            read_container(&facts, &user("user-dev"), &container),
            ContainerRead::Whole,
            "项目成员对容器有立场"
        );

        // 记录负责人但**不是**项目成员：负责人是独立的立场来源。
        // 这里刻意用 `members` 之外的主体，否则验到的还是成员那条分支。
        let owned = ScopedRecord::new("tracking-1", scope("project-1", "tracking-1"))
            .assigned_to(DomainId::new("user-outside-owner"));
        assert_eq!(
            read_container(&facts, &user("user-outside-owner"), &owned),
            ContainerRead::Whole,
            "负责人看得到自己负责的容器，不需要先成为成员"
        );

        // 客户协作者：容器本身没有立场，但也不是拒绝——按目标放行。
        let customer = user("user-customer-a");
        assert_eq!(
            read_container(&facts, &customer, &container),
            ContainerRead::ByTarget,
            "客户协作者对无客户维度的容器没有立场，结论是「按目标放行」而不是拒绝"
        );

        // 同一档不因容器自带客户维度而变成整体可见：那会把兄弟客户一律放进来。
        let customer_scoped =
            ScopedRecord::new("issue-a", scope("project-1", "issue-a")).for_customer("customer-a");
        assert_eq!(
            read_container(&facts, &customer, &customer_scoped),
            ContainerRead::ByTarget,
            "带客户维度的记录上，客户协作者同样只按目标粒度放行"
        );

        // 无关用户：拒绝。这一档由调用方翻成 NotFound，与「不存在」同答。
        assert_eq!(
            read_container(&facts, &user("user-outsider"), &container),
            ContainerRead::Halted,
            "既不是成员、也不是负责人、也没有客户绑定 → 拒绝"
        );
    }

    #[test]
    fn task_2_2_invisible_and_forbidden_are_distinct() {
        let facts = facts();
        let other_project = ScopedRecord::new("issue-x", scope("project-2", "issue-x"));
        assert_eq!(
            access_record(
                &facts,
                &user("user-dev"),
                &other_project,
                StageAction::Submit,
                &StageTarget::new(other_project.scope.clone(), 1),
                AT
            ),
            RecordAccess::Invisible
        );

        let own = ScopedRecord::new("issue-1", scope("project-1", "issue-1"));
        match access_record(
            &facts,
            &user("user-dev"),
            &own,
            StageAction::Submit,
            &StageTarget::new(own.scope.clone(), 1),
            AT,
        ) {
            RecordAccess::Allowed(decision) => assert!(decision.is_allowed(), "成员可提交"),
            RecordAccess::Invisible => panic!("成员应可见本项目记录"),
        }
        match access_record(
            &facts,
            &user("user-dev"),
            &own,
            StageAction::Dispatch,
            &StageTarget::new(own.scope.clone(), 1),
            AT,
        ) {
            RecordAccess::Allowed(decision) => assert!(
                !decision.is_allowed(),
                "可见但无分派权应为 forbidden，而不是 invisible"
            ),
            RecordAccess::Invisible => panic!("成员应看得到，只是不能派"),
        }
    }

    /// §8.1：可见性（阶段二）与目标粒度（阶段四）是两步，能分别归因。
    ///
    /// 合成一步时三种记录在测试里只观察到一个「不可见」，无法证明是哪一条在起作用；
    /// 拆开之后「兄弟客户」与「内部记录」的失败原因必须不同。
    #[test]
    fn visibility_and_target_granularity_fail_for_different_reasons() {
        let mut facts = facts();
        facts
            .customer_bindings
            .push((DomainId::new("user-customer-a"), "customer-a".to_string()));
        let customer = user("user-customer-a");
        let own =
            ScopedRecord::new("issue-a", scope("project-1", "issue-a")).for_customer("customer-a");
        let sibling =
            ScopedRecord::new("issue-b", scope("project-1", "issue-b")).for_customer("customer-b");
        let internal = ScopedRecord::new("issue-internal", scope("project-1", "issue-internal"));

        assert!(
            is_visible(&facts, &customer, &own)
                && target_visible(&facts, &customer, own.customer_id.as_deref()),
            "本客户的目标两步都过"
        );
        // 兄弟客户：阶段二过（对客户维度有立场），阶段四挡下（粒度只到本客户）。
        assert!(
            is_visible(&facts, &customer, &sibling),
            "客户协作者对客户维度的记录有立场"
        );
        assert!(
            !target_visible(&facts, &customer, sibling.customer_id.as_deref()),
            "但只看得到自己那个客户——这是阶段四的职责"
        );
        // 内部记录：阶段二就挡下，阶段四不适用（不在此处重复判断）。
        assert!(
            !is_visible(&facts, &customer, &internal),
            "没有客户维度的记录不在客户的可见域内"
        );
        assert!(
            target_visible(&facts, &customer, internal.customer_id.as_deref()),
            "阶段四不重复判断立场，否则两阶段的失败原因会混在一起"
        );

        // 成员不受目标粒度限制：项目内的内部记录与客户记录都可见。
        let member = user("user-dev");
        assert!(is_visible(&facts, &member, &internal));
        assert!(target_visible(
            &facts,
            &member,
            internal.customer_id.as_deref()
        ));
        assert!(
            is_visible(&facts, &member, &sibling),
            "项目成员看得到本项目下所有客户的记录"
        );
    }

    /// §8.1：四个阶段只产生三种拒绝码，且主体先于对象。
    ///
    /// 表驱动，三条值全部由真实判定产生（不手搓 `StageDecision`）：
    /// 手搓能通过，只证明映射函数会 `match`，证明不了判定确实产出这三种。
    #[test]
    fn rejection_codes_are_three_and_the_subject_stage_comes_first() {
        let facts = facts();
        let own = ScopedRecord::new("issue-1", scope("project-1", "issue-1"));
        let other = ScopedRecord::new("issue-x", scope("project-2", "issue-x"));
        let actor = user("user-dev");

        // 阶段二挡下：不可见。
        let invisible = access_record(
            &facts,
            &actor,
            &other,
            StageAction::Submit,
            &StageTarget::new(other.scope.clone(), 1),
            AT,
        );
        // 阶段三放行：成员可提交。
        let permitted = access_record(
            &facts,
            &actor,
            &own,
            StageAction::Submit,
            &StageTarget::new(own.scope.clone(), 1),
            AT,
        );
        // 阶段三拒绝：可见但无分派权。
        let denied = access_record(
            &facts,
            &actor,
            &own,
            StageAction::Dispatch,
            &StageTarget::new(own.scope.clone(), 1),
            AT,
        );

        let cases = [
            (
                &invisible,
                Some(RecordRejection::NotFound),
                "BOD_NOT_FOUND",
                404,
            ),
            (&permitted, None, "", 0),
            (
                &denied,
                Some(RecordRejection::ScopeDenied),
                "BOD_SCOPE_DENIED",
                403,
            ),
        ];
        for (access, expected, code, status) in cases {
            let actual = rejection(true, access);
            assert_eq!(actual, expected, "分档错误：{access:?}");
            if let Some(actual) = actual {
                assert_eq!(actual.as_code(), code);
                assert_eq!(actual.http_status(), status);
            }
        }

        // 主体先于对象：未认证时答案与对象无关，否则改一个坏凭据打两个对象
        // 就能凭返回码判断哪个存在。
        for access in [&invisible, &permitted, &denied] {
            assert_eq!(
                rejection(false, access),
                Some(RecordRejection::ApiDenied),
                "未认证必须统一答 API 拒绝，不得泄漏对象状态"
            );
        }
        assert_eq!(
            RecordRejection::ApiDenied.as_code(),
            crate::delivery_api::DENIED_CODE,
            "同一档码不得有两个字面量"
        );

        // 文案也是契约的一部分：§8.1 要求「可见但缺权」提示改主体或申请授权；
        // 而 404 的正文是第二个可能泄漏存在性的地方，不许回显对象。
        assert!(
            RecordRejection::ScopeDenied.message().contains("申请授权"),
            "缺权要给出下一步，否则调用方只能猜"
        );
        assert!(
            !RecordRejection::NotFound.message().contains("issue-"),
            "不可见的文案不得回显对象 ID"
        );
    }

    /// 2.2 / AZ06：撤权之后续传与离线重放受限。
    #[test]
    fn task_2_2_revoked_member_cannot_resume_subscription() {
        let mut facts = facts();
        facts.revoked_at_revision = Some(9);
        let scope = scope("project-1", "issue-1");
        assert_eq!(
            check_subscription(&facts, &user("user-dev"), &scope, 12, true),
            SubscriptionOutcome::Allowed,
            "撤权之后的新基线可以续传"
        );
        assert_eq!(
            check_subscription(&facts, &user("user-dev"), &scope, 4, true),
            SubscriptionOutcome::ResyncRequired { from: 9 },
            "早于撤权的离线重放要重新同步"
        );

        facts.members.retain(|(_, id)| id.as_str() != "user-dev");
        assert_eq!(
            check_subscription(&facts, &user("user-dev"), &scope, 12, false),
            SubscriptionOutcome::Denied,
            "已被移出成员则不再续传"
        );
    }

    /// 2.3 / AZ02：项目 B 兑换项目 A 的凭据被拒，审计不含秘密内容。
    #[test]
    fn task_2_3_cross_project_secret_is_denied_without_secret_in_audit() {
        let binding = ExecutionBinding {
            invocation_id: "inv-b".to_string(),
            project_id: DomainId::new("project-b"),
            lease_id: "lease-1".to_string(),
            lease_expires_at: "2026-11-21T11:00:00Z".to_string(),
        };
        let secret = SecretRef {
            ref_id: "secret-a-db".to_string(),
            project_id: DomainId::new("project-a"),
            value: "postgres://user:sup3r-s3cret@db/prod".to_string(),
        };
        match resolve_secret(&binding, &secret, AT) {
            SecretOutcome::Denied { reason, audit } => {
                assert_eq!(reason, SecretDenial::CrossProject);
                assert_eq!(audit.project_id, "project-b");
                assert_eq!(audit.secret_ref, "secret-a-db");
                let rendered = format!("{audit:?}");
                assert!(
                    !rendered.contains("sup3r-s3cret"),
                    "审计不得包含秘密内容：{rendered}"
                );
            }
            other => panic!("跨项目兑换必须被拒，实得 {other:?}"),
        }
    }

    /// 2.3：同项目但租约缺失/过期同样拒绝。
    #[test]
    fn task_2_3_expired_or_missing_lease_blocks_secret() {
        let secret = SecretRef {
            ref_id: "secret-a-db".to_string(),
            project_id: DomainId::new("project-a"),
            value: "value".to_string(),
        };
        let expired = ExecutionBinding {
            invocation_id: "inv-a".to_string(),
            project_id: DomainId::new("project-a"),
            lease_id: "lease-1".to_string(),
            lease_expires_at: "2026-11-21T09:00:00Z".to_string(),
        };
        assert!(matches!(
            resolve_secret(&expired, &secret, AT),
            SecretOutcome::Denied {
                reason: SecretDenial::LeaseExpired,
                ..
            }
        ));

        let no_lease = ExecutionBinding {
            lease_id: String::new(),
            ..expired
        };
        assert!(matches!(
            resolve_secret(&no_lease, &secret, AT),
            SecretOutcome::Denied {
                reason: SecretDenial::LeaseMissing,
                ..
            }
        ));

        let ok = ExecutionBinding {
            invocation_id: "inv-a".to_string(),
            project_id: DomainId::new("project-a"),
            lease_id: "lease-2".to_string(),
            lease_expires_at: "2026-11-21T11:00:00Z".to_string(),
        };
        assert!(matches!(
            resolve_secret(&ok, &secret, AT),
            SecretOutcome::Granted { .. }
        ));
    }

    /// 2.4 / AZ04：执行者自关闭被拒，任务保持待验收。
    #[test]
    fn task_2_4_executor_cannot_self_close() {
        let mut facts = facts();
        facts
            .grants
            .push(grant("user-dev", Permission::Accept, true));
        let request = AcceptanceRequest {
            actor: DomainId::new("user-dev"),
            role: ActorRole::Executor,
            revision: 5,
            current_revision: 5,
            evidence: vec![EvidenceKind::TestReport],
        };
        assert_eq!(
            check_acceptance(&facts, &request, Some(&DomainId::new("user-dev"))),
            AcceptanceVerdict::Refused(AcceptanceRefusal::ExecutorCannotAccept)
        );
    }

    /// 2.4 / AZ04：测试人只凭测试报告不能接受，即使有接受权。
    #[test]
    fn task_2_4_tester_cannot_accept_even_with_report() {
        let mut facts = facts();
        facts
            .grants
            .push(grant("user-tester", Permission::Accept, true));
        let request = AcceptanceRequest {
            actor: DomainId::new("user-tester"),
            role: ActorRole::Tester,
            revision: 5,
            current_revision: 5,
            evidence: vec![EvidenceKind::TestReport],
        };
        assert_eq!(
            check_acceptance(&facts, &request, Some(&DomainId::new("user-dev"))),
            AcceptanceVerdict::Refused(AcceptanceRefusal::TesterCannotAccept)
        );
        // 换成验收人角色、带人工复核证据才推进。
        let acceptor = AcceptanceRequest {
            role: ActorRole::Acceptor,
            actor: DomainId::new("user-owner"),
            ..request
        };
        facts
            .grants
            .push(grant("user-owner", Permission::Accept, true));
        let with_review = AcceptanceRequest {
            evidence: vec![EvidenceKind::TestReport, EvidenceKind::ManualReview],
            ..acceptor
        };
        assert!(matches!(
            check_acceptance(&facts, &with_review, Some(&DomainId::new("user-dev"))),
            AcceptanceVerdict::Advance { .. }
        ));
        let no_evidence = AcceptanceRequest {
            evidence: Vec::new(),
            ..with_review
        };
        assert_eq!(
            check_acceptance(&facts, &no_evidence, Some(&DomainId::new("user-dev"))),
            AcceptanceVerdict::Refused(AcceptanceRefusal::NoEvidence)
        );
    }

    /// 2.4 / AZ04：旧修订重放不推进状态。
    #[test]
    fn task_2_4_stale_revision_replay_does_not_advance() {
        let mut facts = facts();
        facts
            .grants
            .push(grant("user-owner", Permission::Accept, true));
        let stale = AcceptanceRequest {
            actor: DomainId::new("user-owner"),
            role: ActorRole::Acceptor,
            revision: 4,
            current_revision: 5,
            evidence: vec![EvidenceKind::TestReport, EvidenceKind::ManualReview],
        };
        assert_eq!(
            check_acceptance(&facts, &stale, Some(&DomainId::new("user-dev"))),
            AcceptanceVerdict::Refused(AcceptanceRefusal::StaleRevision)
        );
        // 阶段动作同样先挡旧修订。
        let decision = authorize_stage(
            &facts,
            &user("user-owner"),
            StageAction::Accept,
            &StageTarget::new(scope("project-1", "issue-1"), 4).with_current_revision(5),
            AT,
        );
        assert!(matches!(
            decision,
            StageDecision::Denied {
                reason: StageDenial::StaleRevision,
                ..
            }
        ));
    }

    /// 2.5 / AZ05：审批后换成生产账号被拦，连接器调用数为零。
    #[test]
    fn task_2_5_account_switch_after_approval_blocks_with_zero_connector_calls() {
        let ticket = ReleaseTicket {
            environment: "prod".to_string(),
            artifact_digest: "sha256:aa".to_string(),
            params_digest: "params-1".to_string(),
            account_id: "deploy-test".to_string(),
            connector_id: "connector-deploy".to_string(),
            approved_until: "2026-11-21T12:00:00Z".to_string(),
        };
        let switched = ReleaseContext {
            environment: "prod".to_string(),
            artifact_digest: "sha256:aa".to_string(),
            params_digest: "params-1".to_string(),
            account_id: "deploy-prod".to_string(),
            connector_id: "connector-deploy".to_string(),
            now: AT.to_string(),
            authorization_active: true,
        };
        match check_release(&ticket, &switched) {
            ReleaseVerdict::Blocked {
                reason,
                connector_calls,
            } => {
                assert_eq!(reason, ReleaseBlock::AccountChanged);
                assert_eq!(connector_calls, 0, "被拦时连接器调用数必须为零");
            }
            other => panic!("换账号必须被拦，实得 {other:?}"),
        }
    }

    /// 2.5 / AZ05：换制品、撤授权、过期同样拦，且放行时只允许一次调用。
    #[test]
    fn task_2_5_artifact_swap_and_revoked_authorization_are_blocked() {
        let ticket = ReleaseTicket {
            environment: "prod".to_string(),
            artifact_digest: "sha256:aa".to_string(),
            params_digest: "params-1".to_string(),
            account_id: "deploy-prod".to_string(),
            connector_id: "connector-deploy".to_string(),
            approved_until: "2026-11-21T12:00:00Z".to_string(),
        };
        let base = ReleaseContext {
            environment: "prod".to_string(),
            artifact_digest: "sha256:aa".to_string(),
            params_digest: "params-1".to_string(),
            account_id: "deploy-prod".to_string(),
            connector_id: "connector-deploy".to_string(),
            now: AT.to_string(),
            authorization_active: true,
        };
        assert_eq!(
            check_release(&ticket, &base),
            ReleaseVerdict::Dispatch { connector_calls: 1 }
        );
        let swapped = ReleaseContext {
            artifact_digest: "sha256:bb".to_string(),
            ..base.clone()
        };
        assert!(matches!(
            check_release(&ticket, &swapped),
            ReleaseVerdict::Blocked {
                reason: ReleaseBlock::ArtifactChanged,
                connector_calls: 0
            }
        ));
        let revoked = ReleaseContext {
            authorization_active: false,
            ..base.clone()
        };
        assert!(matches!(
            check_release(&ticket, &revoked),
            ReleaseVerdict::Blocked {
                reason: ReleaseBlock::AuthorizationRevoked,
                connector_calls: 0
            }
        ));
        let late = ReleaseContext {
            now: "2026-11-21T12:00:00Z".to_string(),
            ..base
        };
        assert!(matches!(
            check_release(&ticket, &late),
            ReleaseVerdict::Blocked {
                reason: ReleaseBlock::Expired,
                ..
            }
        ));
    }

    /// 2.6：成员退出后新动作被拒，但项目资产与历史分配都不动。
    #[test]
    fn task_2_6_leave_keeps_assets_and_history_but_blocks_new_actions() {
        let mut facts = facts();
        facts
            .grants
            .push(grant("user-dev", Permission::Dispatch, true));
        facts.assignments.push(Assignment {
            owner: Owner {
                kind: OwnerKind::User,
                id: DomainId::new("user-dev"),
            },
            scope: scope("project-1", "issue-1"),
            grant_revision: 1,
        });
        let assignments_before = facts.assignments.clone();

        let outcome = revoke_member(
            &mut facts,
            &DomainId::new("user-dev"),
            MembershipChange::Leave,
            &["run-7".to_string()],
            &[],
        );
        assert!(outcome.blocked_new);
        assert!(outcome.assets_deleted.is_empty(), "撤权不删项目资产");
        assert!(!outcome.history_rewritten, "不改写历史分配");
        assert_eq!(facts.assignments, assignments_before, "历史分配保留原样");
        assert_eq!(
            outcome.in_flight_state,
            "stop_requested_awaiting_confirmation"
        );
        assert_eq!(outcome.in_flight, vec!["run-7".to_string()]);

        assert!(matches!(
            authorize_stage(
                &facts,
                &user("user-dev"),
                StageAction::Submit,
                &target(),
                AT
            ),
            StageDecision::Denied {
                reason: StageDenial::NotAMember,
                ..
            }
        ));
        assert!(!may_manage_budget(&facts, &user("user-dev")));
    }

    /// 2.6：无法撤销的外部凭据进对账，不假装已处理。
    #[test]
    fn task_2_6_unrecoverable_external_credentials_go_to_reconciliation() {
        let mut facts = facts();
        facts.release_operators.push(DomainId::new("user-owner"));
        let outcome = revoke_member(
            &mut facts,
            &DomainId::new("user-owner"),
            MembershipChange::DisableAgent,
            &[],
            &["oauth-grant:github-app".to_string()],
        );
        assert_eq!(
            outcome.reconciliation,
            vec!["oauth-grant:github-app".to_string()]
        );
        assert!(facts.release_operators.is_empty(), "撤销后不再在发布名单");
        assert!(outcome.in_flight.is_empty());
    }
}
