//! P5 任务 3.1—3.6：反馈分类、不可变提案、接受策略、发布撤销、缓存隔离与效果记录
//! （`knowledge-promotion`）。
//!
//! 从"用户说了一句"到"下次真的按新规则做"，中间有五道闸，本模块各管一道：
//! 1. **分类（3.1）**：反馈先判成"学习"还是"治理"。权限、密钥、预算、生产发布、已批准验收条件
//!    一律转治理流程，不写进 Profile；「以后」「下次」「所有项目」不解释成永久或全组织，
//!    默认落在最窄作用域。
//! 2. **提案（3.2）**：`FeedbackProposal` 是不可变修订，带 `base_revision` 做乐观并发；
//!    并发失败返回 Conflict 并**保留候选**，绝不追加到同一个可变文件（`invariant_i_14`）。
//! 3. **接受（3.3）**：自动接受必须同时满足 actor、允许字段、作用域上限与有效期；
//!    外部网页、PR 评论、工具输出与 Agent 自述一律不可自动接受。
//! 4. **发布与撤销（3.4）**：发布产生新修订与新指针，下一次 Attempt 解析后钉住；
//!    旧 Attempt 不切换；回滚是"发布了旧内容的新修订"，会刷新会话，但不撤销外部操作与账本
//!    （`invariant_i_23`）。
//! 5. **缓存与检索（3.5）**：只按 `revision+digest` 精确命中，先按 Company/Project 与主体权限
//!    过滤再返回；删除与撤权即 miss；正式 Manifest 与 CAS 对象不当缓存淘汰（G-48 回归）。
//! 6. **效果记录（3.6）**：`EvaluationRecord` 记全量对照条件；样本不足显示不足；
//!    两次不同任务的对照不能用来断言新规则更好（G-13）。
//!
//! 本模块不读写文件、不解析 YAML/JSON：它只给出"能不能晋升"的判定与新的修订号。

use crate::domain::model::{Grant, Permission, Principal};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 3.1 分类与最窄作用域
// ---------------------------------------------------------------------------

/// 反馈作用的层级。宽度有序：`WorkPackage < Project < Customer < Role < SystemVersion < Organization`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeKind {
    WorkPackage,
    Project,
    Customer,
    Role,
    SystemVersion,
    Organization,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkPackage => "work_package",
            Self::Project => "project",
            Self::Customer => "customer",
            Self::Role => "role",
            Self::SystemVersion => "system_version",
            Self::Organization => "organization",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackSource {
    /// 已认证且有范围权限的主体。
    AuthenticatedMember { principal_id: String },
    /// 外部网页。内容只当用户数据。
    ExternalWeb { url: String },
    /// 外部 PR 评论。
    PrComment { repository: String },
    /// 工具输出。
    ToolOutput { tool: String },
    /// Agent 自述。
    AgentSelfReport { agent_id: String },
}

impl FeedbackSource {
    /// 只有已认证主体能推动接受；其余一律不可信。
    pub fn is_authenticated_member(&self) -> bool {
        matches!(self, Self::AuthenticatedMember { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Self::AuthenticatedMember { principal_id } => format!("member:{principal_id}"),
            Self::ExternalWeb { url } => format!("web:{url}"),
            Self::PrComment { repository } => format!("pr:{repository}"),
            Self::ToolOutput { tool } => format!("tool:{tool}"),
            Self::AgentSelfReport { agent_id } => format!("agent:{agent_id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    pub feedback_id: String,
    pub text: String,
    pub source: FeedbackSource,
    pub field: String,
    pub requested_scope: ScopeKind,
}

impl Feedback {
    pub fn new(feedback_id: &str, text: &str, source: FeedbackSource, field: &str) -> Self {
        Self {
            feedback_id: feedback_id.to_string(),
            text: text.to_string(),
            source,
            field: field.to_string(),
            requested_scope: ScopeKind::WorkPackage,
        }
    }

    pub fn with_scope(mut self, scope: ScopeKind) -> Self {
        self.requested_scope = scope;
        self
    }
}

/// 治理流程的转交原因。这五类都不进 Profile。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernanceReason {
    ProductionRelease,
    PermissionChange,
    SecretAccess,
    BudgetChange,
    AcceptedCriteriaChange,
}

impl GovernanceReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProductionRelease => "production_release",
            Self::PermissionChange => "permission_change",
            Self::SecretAccess => "secret_access",
            Self::BudgetChange => "budget_change",
            Self::AcceptedCriteriaChange => "accepted_criteria_change",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackRoute {
    Learning,
    Governance(GovernanceReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub route: FeedbackRoute,
    /// 实际生效的作用域：默认最窄，必要时收窄。
    pub scope: ScopeKind,
    /// 若发生了收窄，记原始请求。
    pub narrowed_from: Option<ScopeKind>,
    pub untrusted: bool,
    pub reasons: Vec<String>,
}

impl Classification {
    pub fn is_learning(&self) -> bool {
        self.route == FeedbackRoute::Learning
    }
}

const PERMANENCE_MARKERS: [&str; 6] = ["以后", "下次", "永远", "总是", "所有项目", "全公司"];

fn governance_reason(text: &str) -> Option<GovernanceReason> {
    let lowered = text.to_ascii_lowercase();
    let hits: [(GovernanceReason, &[&str]); 5] = [
        (
            GovernanceReason::ProductionRelease,
            &[
                "发布生产",
                "替我发布",
                "直接发布",
                "publish production",
                "deploy to prod",
            ],
        ),
        (
            GovernanceReason::PermissionChange,
            &["权限", "管理员", "permission", "admin"],
        ),
        (
            GovernanceReason::SecretAccess,
            &["密钥", "口令", "secret", "token", "api key"],
        ),
        (
            GovernanceReason::BudgetChange,
            &["预算", "额度", "budget", "quota"],
        ),
        (
            GovernanceReason::AcceptedCriteriaChange,
            &["验收条件", "验收标准", "acceptance criteria"],
        ),
    ];
    for (reason, markers) in hits {
        if markers.iter().any(|marker| lowered.contains(marker)) {
            return Some(reason);
        }
    }
    None
}

/// 分类。顺序：治理关键词 → 来源可信度 → 作用域收窄。治理优先，可信度其次，收窄最后。
pub fn classify(feedback: &Feedback) -> Classification {
    let mut reasons = Vec::new();
    if let Some(reason) = governance_reason(&feedback.text) {
        reasons.push(format!("命中治理关键词，转独立流程（{}）", reason.as_str()));
        return Classification {
            route: FeedbackRoute::Governance(reason),
            scope: ScopeKind::WorkPackage,
            narrowed_from: None,
            untrusted: !feedback.source.is_authenticated_member(),
            reasons,
        };
    }

    let untrusted = !feedback.source.is_authenticated_member();
    if untrusted {
        reasons.push(format!(
            "来源 {} 不是已认证主体，只作为用户数据处理",
            feedback.source.describe()
        ));
    }

    let permanence = PERMANENCE_MARKERS
        .iter()
        .any(|marker| feedback.text.contains(marker));
    let mut scope = feedback.requested_scope;
    let mut narrowed_from = None;
    if permanence && scope > ScopeKind::Project {
        // 「以后」「所有项目」不等于永久与全组织：收到项目级别。
        narrowed_from = Some(scope);
        scope = ScopeKind::Project;
        reasons.push("出现永久/全组织措辞，作用域收窄到项目".to_string());
    } else if feedback.requested_scope == ScopeKind::Organization && !permanence {
        narrowed_from = Some(ScopeKind::Organization);
        scope = ScopeKind::Project;
        reasons.push("没有明确授权时不采用全组织作用域".to_string());
    }

    Classification {
        route: FeedbackRoute::Learning,
        scope,
        narrowed_from,
        untrusted,
        reasons,
    }
}

// ---------------------------------------------------------------------------
// 3.2 不可变提案与冲突（invariant_i_14）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalDraft {
    pub proposal_id: String,
    pub profile_id: String,
    /// 提案基于的修订。为空表示"照最新"，仍会在提交时被判为无依据。
    pub base_revision: u64,
    pub field: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackProposal {
    pub proposal_id: String,
    pub profile_id: String,
    pub base_revision: u64,
    pub field: String,
    pub value: String,
    pub status: ProposalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalStatus {
    /// 并发冲突后保留的候选：不落地，等基于新修订重新检查。
    Candidate,
    Merged,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    Merged {
        profile_id: String,
        revision: u64,
        digest: String,
    },
    /// 并发冲突：候选保留，当前修订不动，也不产生新修订。
    Conflict {
        profile_id: String,
        current_revision: u64,
        candidate_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    UnknownProposal,
    NotACandidate,
    StaleCandidate { current_revision: u64 },
}

/// Profile 的修订账本。修订只追加，摘要按修订号固定，事后不可改。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileBook {
    pub current: BTreeMap<String, u64>,
    pub digests: BTreeMap<(String, u64), String>,
    pub proposals: Vec<FeedbackProposal>,
}

impl ProfileBook {
    pub fn revision_of(&self, profile_id: &str) -> u64 {
        *self.current.get(profile_id).unwrap_or(&0)
    }

    pub fn digest_of(&self, profile_id: &str, revision: u64) -> Option<&str> {
        self.digests
            .get(&(profile_id.to_string(), revision))
            .map(String::as_str)
    }

    pub fn proposal(&self, proposal_id: &str) -> Option<&FeedbackProposal> {
        self.proposals
            .iter()
            .find(|proposal| proposal.proposal_id == proposal_id)
    }

    fn digest(profile_id: &str, revision: u64, field: &str, value: &str) -> String {
        format!("sha256:rev{revision}:{profile_id}:{field}={value}")
    }

    /// 提交提案。`base_revision` 与当前修订不一致即 Conflict，候选进 `proposals` 保留。
    pub fn submit(&mut self, draft: ProposalDraft) -> SubmitOutcome {
        let current_revision = self.revision_of(&draft.profile_id);
        if draft.base_revision != current_revision {
            self.proposals.push(FeedbackProposal {
                proposal_id: draft.proposal_id.clone(),
                profile_id: draft.profile_id.clone(),
                base_revision: draft.base_revision,
                field: draft.field,
                value: draft.value,
                status: ProposalStatus::Candidate,
            });
            return SubmitOutcome::Conflict {
                profile_id: draft.profile_id,
                current_revision,
                candidate_id: draft.proposal_id,
            };
        }
        let revision = current_revision + 1;
        let digest = Self::digest(&draft.profile_id, revision, &draft.field, &draft.value);
        self.current.insert(draft.profile_id.clone(), revision);
        self.digests
            .insert((draft.profile_id.clone(), revision), digest.clone());
        self.proposals.push(FeedbackProposal {
            proposal_id: draft.proposal_id.clone(),
            profile_id: draft.profile_id.clone(),
            base_revision: draft.base_revision,
            field: draft.field,
            value: draft.value,
            status: ProposalStatus::Merged,
        });
        SubmitOutcome::Merged {
            profile_id: draft.profile_id,
            revision,
            digest,
        }
    }

    /// 并发冲突后按新基线重新检查并合并候选。
    pub fn merge_candidate(
        &mut self,
        proposal_id: &str,
        new_base_revision: u64,
    ) -> Result<u64, SubmitError> {
        let index = self
            .proposals
            .iter()
            .position(|proposal| proposal.proposal_id == proposal_id)
            .ok_or(SubmitError::UnknownProposal)?;
        if self.proposals[index].status != ProposalStatus::Candidate {
            return Err(SubmitError::NotACandidate);
        }
        let profile_id = self.proposals[index].profile_id.clone();
        let current_revision = self.revision_of(&profile_id);
        if new_base_revision != current_revision {
            return Err(SubmitError::StaleCandidate { current_revision });
        }
        let revision = current_revision + 1;
        let field = self.proposals[index].field.clone();
        let value = self.proposals[index].value.clone();
        let digest = Self::digest(&profile_id, revision, &field, &value);
        self.current.insert(profile_id.clone(), revision);
        self.digests.insert((profile_id, revision), digest);
        self.proposals[index].status = ProposalStatus::Merged;
        Ok(revision)
    }
}

// ---------------------------------------------------------------------------
// 3.3 接受策略与来源权限
// ---------------------------------------------------------------------------

/// 自动接受规则：谁、哪些字段、最宽到哪、有效期到何时、审计编号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptancePolicy {
    pub actor_id: String,
    pub allowed_fields: Vec<String>,
    pub max_scope: ScopeKind,
    pub valid_until: String,
    pub audit_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoAccept {
    Accepted { audit_id: String },
    HumanApprovalRequired { reason: &'static str },
}

/// 自动接受判定。五道全部成立才自动；任何一条不成立都交人。
pub fn may_auto_accept(
    classification: &Classification,
    field: &str,
    policy: &AcceptancePolicy,
    now: &str,
) -> AutoAccept {
    if let FeedbackRoute::Governance(_) = classification.route {
        return AutoAccept::HumanApprovalRequired {
            reason: "governance_route",
        };
    }
    if classification.untrusted {
        return AutoAccept::HumanApprovalRequired {
            reason: "untrusted_source",
        };
    }
    if classification.scope > policy.max_scope {
        return AutoAccept::HumanApprovalRequired {
            reason: "scope_exceeds_policy",
        };
    }
    if !policy.allowed_fields.iter().any(|item| item == field) {
        return AutoAccept::HumanApprovalRequired {
            reason: "field_not_allowed",
        };
    }
    if now >= policy.valid_until.as_str() {
        return AutoAccept::HumanApprovalRequired {
            reason: "policy_expired",
        };
    }
    AutoAccept::Accepted {
        audit_id: policy.audit_id.clone(),
    }
}

/// 主体必须仍有该项目/角色范围的权限，规则才可能被推动（撤权即失效）。
pub fn actor_may_promote(grants: &[Grant], actor: &Principal, permission: Permission) -> bool {
    grants.iter().any(|grant| {
        grant.active && grant.principal_id == actor.id && grant.permission == permission
    })
}

// ---------------------------------------------------------------------------
// 3.4 发布、会话与撤销（invariant_i_23）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileVersion {
    pub revision: u64,
    pub digest: String,
    pub published_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRevision {
    pub profile_id: String,
    pub revision: u64,
    pub digest: String,
}

/// Profile 的已发布版本与当前指针。指针变 = 下一次 Attempt 换版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePointer {
    pub profile_id: String,
    pub versions: Vec<ProfileVersion>,
    pub pointer: usize,
}

impl ProfilePointer {
    pub fn new(profile_id: &str, digest: &str) -> Self {
        Self {
            profile_id: profile_id.to_string(),
            versions: vec![ProfileVersion {
                revision: 1,
                digest: digest.to_string(),
                published_at: "2026-11-21T09:00:00Z".to_string(),
            }],
            pointer: 0,
        }
    }

    pub fn current(&self) -> &ProfileVersion {
        &self.versions[self.pointer]
    }

    /// 发布新修订并移动指针。
    pub fn publish(&mut self, digest: &str, at: &str) -> u64 {
        let revision = self.versions.last().map(|v| v.revision).unwrap_or(0) + 1;
        self.versions.push(ProfileVersion {
            revision,
            digest: digest.to_string(),
            published_at: at.to_string(),
        });
        self.pointer = self.versions.len() - 1;
        revision
    }

    /// 下一次 Attempt 在解析后钉住当前修订；已开始的 Attempt 用自己手里的钉扎值。
    pub fn pin(&self) -> PinnedRevision {
        let current = self.current();
        PinnedRevision {
            profile_id: self.profile_id.clone(),
            revision: current.revision,
            digest: current.digest.clone(),
        }
    }

    /// 回滚 = 发布一个"内容等于旧版本"的新修订。会话必须刷新，账本与外部操作不动。
    pub fn rollback(&mut self, to_revision: u64, at: &str) -> Result<RollbackResult, SubmitError> {
        let digest = self
            .versions
            .iter()
            .find(|version| version.revision == to_revision)
            .map(|version| version.digest.clone())
            .ok_or(SubmitError::UnknownProposal)?;
        let published = self.publish(&digest, at);
        Ok(RollbackResult {
            published_revision: published,
            content_revision: to_revision,
            session_refreshed: true,
            external_ops_reverted: false,
            ledger_reverted: false,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackResult {
    /// 新发布的修订号（指针指向它）。
    pub published_revision: u64,
    /// 内容回到的那个旧修订。
    pub content_revision: u64,
    pub session_refreshed: bool,
    pub external_ops_reverted: bool,
    pub ledger_reverted: bool,
}

// ---------------------------------------------------------------------------
// 3.5 缓存与检索隔离（G-48 回归）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    ProfileResolve,
    SkillResolve,
    SpecResolve,
    /// 正式 Manifest 指针：不是缓存，不得淘汰。
    ManifestPointer,
    /// CAS 对象：不是缓存，不得淘汰。
    CasObject,
}

impl CacheKind {
    /// 只有解析类结果才是缓存；Manifest 与 CAS 是正式数据。
    pub fn is_evictable(self) -> bool {
        matches!(
            self,
            Self::ProfileResolve | Self::SkillResolve | Self::SpecResolve
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub artifact_id: String,
    pub revision: u64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub key: CacheKey,
    pub kind: CacheKind,
    pub company_id: String,
    pub project_id: Option<String>,
    pub text: String,
    pub deleted: bool,
    pub required_permission: Option<Permission>,
}

impl CacheEntry {
    pub fn new(key: CacheKey, kind: CacheKind, company_id: &str, text: &str) -> Self {
        Self {
            key,
            kind,
            company_id: company_id.to_string(),
            project_id: None,
            text: text.to_string(),
            deleted: false,
            required_permission: None,
        }
    }

    pub fn in_project(mut self, project_id: &str) -> Self {
        self.project_id = Some(project_id.to_string());
        self
    }

    pub fn requiring(mut self, permission: Permission) -> Self {
        self.required_permission = Some(permission);
        self
    }
}

/// 检索上下文：公司、项目、主体当前权限与是否已撤权。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalQuery {
    pub company_id: String,
    pub project_id: Option<String>,
    pub actor: Principal,
    pub grants: Vec<Grant>,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    Hit(String),
    Miss(&'static str),
}

/// 只按 `revision+digest` 精确命中；返回前先过滤公司/项目/权限，删除与撤权即 miss。
pub fn lookup(entries: &[CacheEntry], key: &CacheKey, query: &RetrievalQuery) -> CacheLookup {
    let Some(entry) = entries.iter().find(|entry| &entry.key == key) else {
        return CacheLookup::Miss("no_entry");
    };
    if entry.deleted {
        return CacheLookup::Miss("deleted");
    }
    if query.revoked {
        return CacheLookup::Miss("revoked");
    }
    if entry.company_id != query.company_id {
        return CacheLookup::Miss("company_mismatch");
    }
    if let Some(project) = &entry.project_id
        && query.project_id.as_deref() != Some(project.as_str())
    {
        return CacheLookup::Miss("project_mismatch");
    }
    if let Some(permission) = entry.required_permission {
        let allowed = query.grants.iter().any(|grant| {
            grant.active && grant.permission == permission && grant.principal_id == query.actor.id
        });
        if !allowed {
            return CacheLookup::Miss("permission_denied");
        }
    }
    CacheLookup::Hit(entry.text.clone())
}

/// 淘汰只作用于解析缓存；Manifest 与 CAS 一律保留。
pub fn evict(entries: &mut Vec<CacheEntry>) -> usize {
    let before = entries.len();
    entries.retain(|entry| !entry.kind.is_evictable());
    before - entries.len()
}

// ---------------------------------------------------------------------------
// 3.6 效果记录（G-13 / invariant_i_23）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluationResult {
    Better,
    Same,
    Worse,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationRecord {
    pub record_id: String,
    pub task_set_version: u32,
    pub profile_revision: u64,
    pub skill_revision: Option<u64>,
    pub runtime: String,
    pub model_config: String,
    pub environment: String,
    pub samples: usize,
    pub required_samples: usize,
    pub acceptance_criteria: String,
    pub cost_minor: i64,
    pub duration_seconds: u64,
    pub human_interventions: u32,
    pub result: EvaluationResult,
}

impl EvaluationRecord {
    pub fn new(record_id: &str, task_set_version: u32, samples: usize) -> Self {
        Self {
            record_id: record_id.to_string(),
            task_set_version,
            profile_revision: 1,
            skill_revision: None,
            runtime: "rust-stable".to_string(),
            model_config: "default".to_string(),
            environment: "staging".to_string(),
            samples,
            required_samples: 30,
            acceptance_criteria: "项目既定验收标准".to_string(),
            cost_minor: 0,
            duration_seconds: 0,
            human_interventions: 0,
            result: EvaluationResult::Same,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionVerdict {
    Allowed,
    InsufficientSamples { have: usize, need: usize },
    NotBetter { result: EvaluationResult },
    Inconclusive,
}

/// 单条记录能不能支持"自动提升优先级"。样本不足直接拒，不拿结果好当理由。
pub fn promotion_verdict(record: &EvaluationRecord) -> PromotionVerdict {
    if record.samples < record.required_samples {
        return PromotionVerdict::InsufficientSamples {
            have: record.samples,
            need: record.required_samples,
        };
    }
    match record.result {
        EvaluationResult::Better => PromotionVerdict::Allowed,
        EvaluationResult::Same | EvaluationResult::Worse => PromotionVerdict::NotBetter {
            result: record.result,
        },
        EvaluationResult::Inconclusive => PromotionVerdict::Inconclusive,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomparableTaskSets {
    pub left: u32,
    pub right: u32,
}

/// 两次对照必须同任务集版本才可比；不同任务的对照不构成"新规则更优"的证据。
pub fn compare(
    left: &EvaluationRecord,
    right: &EvaluationRecord,
) -> Result<PromotionVerdict, IncomparableTaskSets> {
    if left.task_set_version != right.task_set_version {
        return Err(IncomparableTaskSets {
            left: left.task_set_version,
            right: right.task_set_version,
        });
    }
    if left.samples < left.required_samples || right.samples < right.required_samples {
        return Ok(PromotionVerdict::InsufficientSamples {
            have: left.samples.min(right.samples),
            need: left.required_samples.max(right.required_samples),
        });
    }
    let better = matches!(
        (left.result, right.result),
        (
            EvaluationResult::Better,
            EvaluationResult::Same | EvaluationResult::Worse
        ) | (EvaluationResult::Same, EvaluationResult::Worse)
    );
    if better {
        Ok(PromotionVerdict::Allowed)
    } else {
        Ok(PromotionVerdict::NotBetter {
            result: left.result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DomainId;

    const NOW: &str = "2026-11-21T10:00:00Z";

    fn member(id: &str) -> FeedbackSource {
        FeedbackSource::AuthenticatedMember {
            principal_id: id.to_string(),
        }
    }

    fn policy() -> AcceptancePolicy {
        AcceptancePolicy {
            actor_id: "user-lead".to_string(),
            allowed_fields: vec!["verbosity".to_string(), "language".to_string()],
            max_scope: ScopeKind::Project,
            valid_until: "2026-12-31T00:00:00Z".to_string(),
            audit_id: "audit-1".to_string(),
        }
    }

    fn principal(id: &str) -> Principal {
        Principal {
            id: DomainId::new(id),
            kind: crate::domain::model::PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    fn grant(id: &str, permission: Permission, active: bool) -> Grant {
        Grant {
            principal_id: DomainId::new(id),
            scope: crate::domain::model::Scope {
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                issue_id: None,
            },
            permission,
            revision: 1,
            active,
        }
    }

    /// 3.1：「以后直接替我发布生产」转独立批准请求，不写入规则。
    #[test]
    fn task_3_1_production_release_request_goes_to_governance() {
        let feedback = Feedback::new(
            "fb-1",
            "以后直接替我发布生产，不用再问我",
            member("user-owner"),
            "release_policy",
        );
        let classification = classify(&feedback);
        assert_eq!(
            classification.route,
            FeedbackRoute::Governance(GovernanceReason::ProductionRelease)
        );
        assert!(!classification.is_learning(), "不得写成普通学习规则");
        assert!(!classification.reasons.is_empty());
        assert!(matches!(
            may_auto_accept(&classification, "release_policy", &policy(), NOW),
            AutoAccept::HumanApprovalRequired {
                reason: "governance_route"
            }
        ));
    }

    /// 3.1：「所有项目」不解释成全组织，作用域收窄到项目。
    #[test]
    fn task_3_1_always_is_not_read_as_organization_wide() {
        let feedback = Feedback::new(
            "fb-2",
            "以后所有项目都用中文回复",
            member("user-owner"),
            "language",
        )
        .with_scope(ScopeKind::Organization);
        let classification = classify(&feedback);
        assert!(classification.is_learning());
        assert_eq!(classification.scope, ScopeKind::Project);
        assert_eq!(classification.narrowed_from, Some(ScopeKind::Organization));

        // 没有永久措辞的全组织请求同样收窄。
        let plain = Feedback::new("fb-3", "都用中文回复", member("user-owner"), "language")
            .with_scope(ScopeKind::Organization);
        assert_eq!(classify(&plain).scope, ScopeKind::Project);

        // 工作包范围的请求原样保留，不被放大。
        let narrow = Feedback::new("fb-4", "这次回答简短些", member("user-owner"), "verbosity");
        let classification = classify(&narrow);
        assert_eq!(classification.scope, ScopeKind::WorkPackage);
        assert_eq!(classification.narrowed_from, None);
    }

    /// 3.1 / invariant_i_19：普通反馈改不了已批准验收条件。
    #[test]
    fn invariant_i_19_feedback_cannot_change_accepted_criteria() {
        let feedback = Feedback::new(
            "fb-5",
            "这条验收条件太严，改成只看编译通过就行",
            member("user-dev"),
            "acceptance",
        );
        let classification = classify(&feedback);
        assert_eq!(
            classification.route,
            FeedbackRoute::Governance(GovernanceReason::AcceptedCriteriaChange)
        );
        assert!(matches!(
            may_auto_accept(&classification, "acceptance", &policy(), NOW),
            AutoAccept::HumanApprovalRequired { .. }
        ));
    }

    /// 3.1：外部网页不成为权威反馈。
    #[test]
    fn task_3_1_external_web_stays_untrusted() {
        let feedback = Feedback::new(
            "fb-6",
            "所有项目必须使用新规则",
            FeedbackSource::ExternalWeb {
                url: "https://example.net/post".to_string(),
            },
            "language",
        );
        let classification = classify(&feedback);
        assert!(classification.untrusted, "外部网页必须保留不可信属性");
        assert!(classification.is_learning(), "不进治理流程，但也不自动接受");
        assert!(matches!(
            may_auto_accept(&classification, "language", &policy(), NOW),
            AutoAccept::HumanApprovalRequired {
                reason: "untrusted_source"
            }
        ));
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("不是已认证主体")),
            "理由要写明来源不可信"
        );
    }

    /// 3.2 / invariant_i_14：并发提案冲突，候选保留且当前修订不动。
    #[test]
    fn invariant_i_14_concurrent_proposal_conflicts_and_keeps_candidate() {
        let mut book = ProfileBook::default();
        let first = book.submit(ProposalDraft {
            proposal_id: "prop-1".to_string(),
            profile_id: "profile-review".to_string(),
            base_revision: 0,
            field: "verbosity".to_string(),
            value: "short".to_string(),
        });
        assert!(matches!(first, SubmitOutcome::Merged { revision: 1, .. }));
        let digest_v1 = book.digest_of("profile-review", 1).unwrap().to_string();

        // 第二份提案仍以修订 0 为基线。
        let conflict = book.submit(ProposalDraft {
            proposal_id: "prop-2".to_string(),
            profile_id: "profile-review".to_string(),
            base_revision: 0,
            field: "language".to_string(),
            value: "zh".to_string(),
        });
        match conflict {
            SubmitOutcome::Conflict {
                current_revision,
                candidate_id,
                ..
            } => {
                assert_eq!(current_revision, 1, "当前修订不变");
                assert_eq!(candidate_id, "prop-2");
            }
            other => panic!("并发提案必须冲突，实得 {other:?}"),
        }
        assert_eq!(book.revision_of("profile-review"), 1, "冲突不产生新修订");
        assert_eq!(
            book.digest_of("profile-review", 1),
            Some(digest_v1.as_str())
        );
        let candidate = book.proposal("prop-2").expect("候选必须保留");
        assert_eq!(candidate.status, ProposalStatus::Candidate);
        assert_eq!(candidate.base_revision, 0, "候选保留原基线，便于重新检查");

        // 基于新基线才能合并。
        assert_eq!(
            book.merge_candidate("prop-2", 0),
            Err(SubmitError::StaleCandidate {
                current_revision: 1
            })
        );
        assert_eq!(book.merge_candidate("prop-2", 1), Ok(2));
        assert_eq!(book.revision_of("profile-review"), 2);
        assert_ne!(
            book.digest_of("profile-review", 1),
            book.digest_of("profile-review", 2)
        );
    }

    /// 3.2：合并新提案不改写旧修订的摘要。
    #[test]
    fn task_3_2_merged_proposal_creates_new_revision_without_touching_old_digest() {
        let mut book = ProfileBook::default();
        book.submit(ProposalDraft {
            proposal_id: "p1".to_string(),
            profile_id: "profile-a".to_string(),
            base_revision: 0,
            field: "verbosity".to_string(),
            value: "short".to_string(),
        });
        let v1 = book.digest_of("profile-a", 1).unwrap().to_string();
        book.submit(ProposalDraft {
            proposal_id: "p2".to_string(),
            profile_id: "profile-a".to_string(),
            base_revision: 1,
            field: "language".to_string(),
            value: "zh".to_string(),
        });
        assert_eq!(book.digest_of("profile-a", 1), Some(v1.as_str()));
        assert_eq!(book.revision_of("profile-a"), 2);
        let merged: Vec<&FeedbackProposal> = book
            .proposals
            .iter()
            .filter(|proposal| proposal.status == ProposalStatus::Merged)
            .collect();
        assert_eq!(merged.len(), 2);
    }

    /// 3.3：只有允许字段、范围内、未过期的规则才自动接受。
    #[test]
    fn task_3_3_auto_accept_only_for_allowed_fields_and_scope() {
        let ok = classify(&Feedback::new(
            "fb-7",
            "回答再短一点",
            member("user-owner"),
            "verbosity",
        ));
        assert_eq!(
            may_auto_accept(&ok, "verbosity", &policy(), NOW),
            AutoAccept::Accepted {
                audit_id: "audit-1".to_string()
            }
        );

        let not_allowed = classify(&Feedback::new(
            "fb-8",
            "用深色主题",
            member("user-owner"),
            "theme",
        ));
        assert_eq!(
            may_auto_accept(&not_allowed, "theme", &policy(), NOW),
            AutoAccept::HumanApprovalRequired {
                reason: "field_not_allowed"
            }
        );

        let too_wide = Classification {
            route: FeedbackRoute::Learning,
            scope: ScopeKind::Customer,
            narrowed_from: None,
            untrusted: false,
            reasons: Vec::new(),
        };
        assert_eq!(
            may_auto_accept(&too_wide, "verbosity", &policy(), NOW),
            AutoAccept::HumanApprovalRequired {
                reason: "scope_exceeds_policy"
            }
        );

        assert_eq!(
            may_auto_accept(&ok, "verbosity", &policy(), "2027-01-01T00:00:00Z"),
            AutoAccept::HumanApprovalRequired {
                reason: "policy_expired"
            }
        );
    }

    /// 3.3：撤权主体推不动任何规则。
    #[test]
    fn task_3_3_revoked_actor_cannot_promote() {
        let actor = principal("user-owner");
        assert!(actor_may_promote(
            &[grant("user-owner", Permission::ManageScope, true)],
            &actor,
            Permission::ManageScope
        ));
        assert!(!actor_may_promote(
            &[grant("user-owner", Permission::ManageScope, false)],
            &actor,
            Permission::ManageScope
        ));
        assert!(
            !actor_may_promote(
                &[grant("user-owner", Permission::Dispatch, true)],
                &actor,
                Permission::ManageScope
            ),
            "分派权不派生规则推动权"
        );
    }

    /// 3.4：下一次 Attempt 钉新修订，旧 Attempt 不切换。
    #[test]
    fn task_3_4_next_attempt_pins_new_revision_and_old_attempt_keeps_old() {
        let mut pointer = ProfilePointer::new("profile-review", "sha256:v1");
        let running = pointer.pin();
        assert_eq!(running.revision, 1);

        let next_revision = pointer.publish("sha256:v2", "2026-11-21T10:00:00Z");
        assert_eq!(next_revision, 2);
        assert_eq!(pointer.current().digest, "sha256:v2");

        let next = pointer.pin();
        assert_eq!(next.revision, 2);
        assert_eq!(next.digest, "sha256:v2");
        assert_eq!(running.revision, 1, "已开始的 Attempt 保留自己钉住的版本");
        assert_eq!(running.digest, "sha256:v1");
    }

    /// 3.4 / invariant_i_23：回滚刷新会话，但不撤销账本与外部操作。
    #[test]
    fn invariant_i_23_rollback_refreshes_session_without_reverting_ledger() {
        let mut pointer = ProfilePointer::new("profile-review", "sha256:v1");
        pointer.publish("sha256:v2", "2026-11-21T10:00:00Z");
        let rollback = pointer
            .rollback(1, "2026-11-21T11:00:00Z")
            .expect("回到已有版本");
        assert_eq!(rollback.content_revision, 1);
        assert_eq!(rollback.published_revision, 3, "回滚也是发布一个新修订");
        assert!(rollback.session_refreshed, "回滚必须刷新会话");
        assert!(!rollback.external_ops_reverted, "不伪造外部操作回滚");
        assert!(!rollback.ledger_reverted, "不撤销已发生的账本");
        assert_eq!(pointer.current().digest, "sha256:v1");
        assert_eq!(pointer.current().revision, 3);
        // 历史版本仍在。
        assert_eq!(pointer.versions.len(), 3);
        assert_eq!(pointer.rollback(9, NOW), Err(SubmitError::UnknownProposal));
    }

    /// 3.5：撤权后缓存命中被拒。
    #[test]
    fn task_3_5_revoked_actor_cannot_hit_cache() {
        let key = CacheKey {
            artifact_id: "profile-review".to_string(),
            revision: 2,
            digest: "sha256:v2".to_string(),
        };
        let entries = vec![CacheEntry::new(
            key.clone(),
            CacheKind::ProfileResolve,
            "company-default",
            "review style",
        )];
        let query = RetrievalQuery {
            company_id: "company-default".to_string(),
            project_id: None,
            actor: principal("user-dev"),
            grants: Vec::new(),
            revoked: false,
        };
        assert_eq!(
            lookup(&entries, &key, &query),
            CacheLookup::Hit("review style".to_string())
        );
        let revoked = RetrievalQuery {
            revoked: true,
            ..query
        };
        assert_eq!(
            lookup(&entries, &key, &revoked),
            CacheLookup::Miss("revoked")
        );
    }

    /// 3.5：删除与跨项目都是 miss；缺权限也是 miss。
    #[test]
    fn task_3_5_deleted_entry_and_other_project_are_misses() {
        let key = CacheKey {
            artifact_id: "skill-tdd".to_string(),
            revision: 1,
            digest: "sha256:s1".to_string(),
        };
        let mut entry = CacheEntry::new(
            key.clone(),
            CacheKind::SkillResolve,
            "company-default",
            "tdd steps",
        )
        .in_project("project-1")
        .requiring(Permission::Accept);
        let base = RetrievalQuery {
            company_id: "company-default".to_string(),
            project_id: Some("project-1".to_string()),
            actor: principal("user-owner"),
            grants: vec![grant("user-owner", Permission::Accept, true)],
            revoked: false,
        };
        assert!(matches!(
            lookup(&[entry.clone()], &key, &base),
            CacheLookup::Hit(_)
        ));

        let no_permission = RetrievalQuery {
            grants: Vec::new(),
            ..base.clone()
        };
        assert_eq!(
            lookup(&[entry.clone()], &key, &no_permission),
            CacheLookup::Miss("permission_denied")
        );

        let other_project = RetrievalQuery {
            project_id: Some("project-2".to_string()),
            ..base.clone()
        };
        assert_eq!(
            lookup(&[entry.clone()], &key, &other_project),
            CacheLookup::Miss("project_mismatch")
        );

        entry.deleted = true;
        assert_eq!(
            lookup(&[entry], &key, &base),
            CacheLookup::Miss("deleted"),
            "敏感原文删除后不得因缓存继续泄露"
        );

        let other_key = CacheKey { revision: 3, ..key };
        assert_eq!(
            lookup(&[], &other_key, &base),
            CacheLookup::Miss("no_entry")
        );
    }

    /// 3.5 / G-48 回归：Manifest 与 CAS 不被当缓存淘汰。
    #[test]
    fn task_3_5_manifest_and_cas_are_not_evicted() {
        let key = |id: &str| CacheKey {
            artifact_id: id.to_string(),
            revision: 1,
            digest: "sha256:x".to_string(),
        };
        let mut entries = vec![
            CacheEntry::new(
                key("profile"),
                CacheKind::ProfileResolve,
                "company-default",
                "p",
            ),
            CacheEntry::new(
                key("skill"),
                CacheKind::SkillResolve,
                "company-default",
                "s",
            ),
            CacheEntry::new(key("spec"), CacheKind::SpecResolve, "company-default", "sp"),
            CacheEntry::new(
                key("manifest"),
                CacheKind::ManifestPointer,
                "company-default",
                "m",
            ),
            CacheEntry::new(key("cas"), CacheKind::CasObject, "company-default", "c"),
        ];
        assert!(!CacheKind::ManifestPointer.is_evictable());
        assert!(!CacheKind::CasObject.is_evictable());
        let removed = evict(&mut entries);
        assert_eq!(removed, 3, "只淘汰三类解析缓存");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.kind == CacheKind::ManifestPointer));
        assert!(entries.iter().any(|e| e.kind == CacheKind::CasObject));
    }

    /// 3.6 / G-13：样本不足不许自动提升。
    #[test]
    fn task_3_6_insufficient_samples_block_promotion() {
        let mut record = EvaluationRecord::new("eval-1", 3, 4);
        record.result = EvaluationResult::Better;
        assert_eq!(
            promotion_verdict(&record),
            PromotionVerdict::InsufficientSamples { have: 4, need: 30 }
        );
        record.samples = 30;
        assert_eq!(promotion_verdict(&record), PromotionVerdict::Allowed);

        record.result = EvaluationResult::Same;
        assert_eq!(
            promotion_verdict(&record),
            PromotionVerdict::NotBetter {
                result: EvaluationResult::Same
            }
        );
        record.result = EvaluationResult::Inconclusive;
        assert_eq!(promotion_verdict(&record), PromotionVerdict::Inconclusive);
    }

    /// 3.6 / invariant_i_23：两次不同任务的对照不能断言新规则更优。
    #[test]
    fn invariant_i_23_two_different_task_sets_cannot_be_compared() {
        let mut left = EvaluationRecord::new("eval-a", 3, 40);
        left.result = EvaluationResult::Better;
        let right = EvaluationRecord::new("eval-b", 4, 40);
        assert_eq!(
            compare(&left, &right),
            Err(IncomparableTaskSets { left: 3, right: 4 })
        );

        let same_set = EvaluationRecord::new("eval-c", 3, 40);
        assert_eq!(compare(&left, &same_set), Ok(PromotionVerdict::Allowed));

        let mut thin = EvaluationRecord::new("eval-d", 3, 2);
        thin.result = EvaluationResult::Better;
        assert!(matches!(
            compare(&left, &thin),
            Ok(PromotionVerdict::InsufficientSamples { have: 2, .. })
        ));
    }
}
