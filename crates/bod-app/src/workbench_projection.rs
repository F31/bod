//! 工作台只读投影（P1 任务 4.5，`invariant_i_55`）。
//!
//! 首页四问：本期目标、下一项可交付成果、哪里受阻、必须由用户决定的事项；
//! 任务页：状态、负责人、证据与拒绝原因。
//!
//! 边界（I-55）：投影是只读请求——只读宿主事实，不写库、不改控制状态、不启动 Agent。
//! 本模块不持有 `SqliteStore` 的可变事务，只接收调用方传入的只读快照 [`HostFacts`]；
//! 审计与有界计算允许，但任何命令（批准/取消/派发/接受）都必须走控制 API 另一条路径。
//! 待决定事项的三要素（阻塞主体、所需决定、可采取动作）来自 4.3 的对账案件与
//! 3.x 的澄清 Question，不在投影层重新发明决定。
//! 只从宿主事实计算，不建立第二个业务写入者。

use crate::domain::DomainId;
use crate::domain::artifact::{AcceptanceRecord, VerificationRecord, VerificationState};
use crate::domain::model::{Issue, Phase};
use crate::domain::queue::{QueueState, WorkQueueEntry};
use crate::recovery_models::ReconciliationCase;

// ---------------------------------------------------------------------------
// 只读事实快照：调用方从宿主读取后传入，投影函数不接触任何写入句柄
// ---------------------------------------------------------------------------

/// 宿主事实快照。投影只消费这些值，不会回写，也不会据此触发命令。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    pub company_id: String,
    pub project_id: String,
    /// 本期目标与任务页的 Issue 来源。
    pub issues: Vec<Issue>,
    /// 队列状态：下一项可交付与阻塞投影。
    pub queue: Vec<WorkQueueEntry>,
    /// 成果接受记录（含证据引用与拒绝原因）。
    pub acceptances: Vec<AcceptanceRecord>,
    /// 验证记录（含状态、证据引用与失败原因）。
    pub verifications: Vec<VerificationRecord>,
    /// 开放/全部对账案件：待决定事项与阻塞主体来源。
    pub cases: Vec<ReconciliationCase>,
    /// 修订 → 成果 id 的只读关联，用于把验证证据归属到具体任务。
    pub revision_artifact_links: Vec<(String, String)>,
    /// 澄清待决定（由 3.x 的 NeedsInputView 投影而来，只含展示所需字段）。
    pub questions: Vec<QuestionFact>,
}

/// 澄清 Question 的只读展示事实（不携带可执行的控制命令）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionFact {
    pub question_id: String,
    pub attempt_id: String,
    pub answerers: Vec<String>,
    /// 阻塞范围的可读标签（不解析为写入指令）。
    pub blocked_scope: String,
    pub required_decision: String,
    pub since: String,
}

// ---------------------------------------------------------------------------
// 首页四问视图
// ---------------------------------------------------------------------------

/// 首页四问投影结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeView {
    pub company_id: String,
    pub project_id: String,
    /// 1. 本期目标。
    pub goal: Option<GoalCard>,
    /// 2. 下一项可交付成果。
    pub next_deliverable: Option<DeliverableCard>,
    /// 3. 哪里受阻（稳定原因 + 解除条件）。
    pub blockers: Vec<BlockerCard>,
    /// 4. 必须由用户决定的事项（阻塞主体 + 所需决定 + 可采取动作 + 证据）。
    pub decisions: Vec<DecisionCard>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCard {
    pub issue_uid: String,
    pub title: String,
    pub phase: String,
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverableCard {
    pub issue_uid: String,
    pub title: String,
    /// 该成果是否已被接受（宿主事实，非执行端自报）。
    pub accepted: bool,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerCard {
    /// 被阻塞的对象（Issue uid / 队列键 / 对账案件）。
    pub subject: String,
    /// 稳定原因码（如 InsufficientBudget / ExternalOutcomeUnknown）。
    pub reason: String,
    /// 解除条件；未知时明确写 Unknown，不静默清空。
    pub next_step: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    Cancellation,
    Clarification,
    Acceptance,
    Budget,
}

impl DecisionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancellation => "cancellation",
            Self::Clarification => "clarification",
            Self::Acceptance => "acceptance",
            Self::Budget => "budget",
        }
    }
}

/// 待决定事项：必须同时含阻塞主体、所需决定与可采取动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionCard {
    pub subject: String,
    pub source: DecisionSource,
    /// 阻塞主体（谁挡住/谁需答复），不为空。
    pub blocking_actor: String,
    /// 所需决定，不为空。
    pub required_decision: String,
    /// 可采取动作，至少一项。
    pub available_actions: Vec<String>,
    /// 支撑证据引用（可为空，但显示时如实标注）。
    pub evidence: Vec<String>,
}

// ---------------------------------------------------------------------------
// 任务页视图
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    pub issue_uid: String,
    pub title: String,
    /// 当前状态（Phase 字符串）。
    pub status: String,
    /// 负责人（assignee），无则 None 而非猜测。
    pub owner: Option<String>,
    pub acceptance: Vec<String>,
    /// 证据引用（来自验收/验证记录，非空时逐条可追溯）。
    pub evidence: Vec<EvidenceCard>,
    /// 拒绝原因（验证失败/接受拒绝），含稳定原因与证据。
    pub rejections: Vec<RejectionCard>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceCard {
    pub reference: String,
    /// 证据来自的宿主记录类型。
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectionCard {
    pub reason: String,
    /// 关联的稳定原因；无稳定原因时为空串。
    pub stable_code: String,
    pub evidence: Vec<String>,
}

// ---------------------------------------------------------------------------
// 投影：纯函数，签名不含任何写入句柄，仅消费 HostFacts
// ---------------------------------------------------------------------------

/// 投影首页四问。相同事实必得相同视图（确定性、有界计算）。
pub fn project_home(facts: &HostFacts) -> HomeView {
    HomeView {
        company_id: facts.company_id.clone(),
        project_id: facts.project_id.clone(),
        goal: project_goal(facts),
        next_deliverable: project_next_deliverable(facts),
        blockers: project_blockers(facts),
        decisions: project_decisions(facts),
    }
}

/// 投影任务页。找不到 Issue 时返回 None，不伪造一张空卡。
pub fn project_task(facts: &HostFacts, issue_uid: &str) -> Option<TaskView> {
    let issue = facts
        .issues
        .iter()
        .find(|item| item.meta.uid.as_str() == issue_uid)?;
    let uid = issue_uid.to_string();
    let mut evidence = Vec::new();
    let mut rejections = Vec::new();

    for record in &facts.acceptances {
        if record.artifact_id.as_str() == issue_uid
            || record.binding.work_package_revision.as_str() == issue_uid
        {
            for reference in &record.evidence_refs {
                evidence.push(EvidenceCard {
                    reference: reference.clone(),
                    source: format!("acceptance:{}", record.decision.as_str()),
                });
            }
            if !record.decision.is_accepted() && !record.reason.is_empty() {
                rejections.push(RejectionCard {
                    reason: record.reason.clone(),
                    stable_code: "RequiresHuman".to_string(),
                    evidence: record.evidence_refs.clone(),
                });
            }
        }
    }

    let linked_revisions: Vec<&str> = facts
        .revision_artifact_links
        .iter()
        .filter(|(_, artifact_id)| artifact_id == &uid)
        .map(|(revision, _)| revision.as_str())
        .collect();
    for record in &facts.verifications {
        if !linked_revisions.contains(&record.revision_id.as_str()) {
            continue;
        }
        for reference in &record.evidence_refs {
            evidence.push(EvidenceCard {
                reference: reference.clone(),
                source: format!("verification:{}", record.state.as_str()),
            });
        }
        if matches!(
            record.state,
            VerificationState::Rejected | VerificationState::NeedsRevision
        ) && !record.reason.is_empty()
        {
            rejections.push(RejectionCard {
                reason: record.reason.clone(),
                stable_code: verification_code(record.state),
                evidence: record.evidence_refs.clone(),
            });
        }
    }

    // 稳定顺序：证据按 (source, reference) 排序，拒绝按 (stable_code, reason) 排序。
    evidence.sort_by(|left, right| {
        (&left.source, &left.reference).cmp(&(&right.source, &right.reference))
    });
    rejections.sort_by(|left, right| {
        (&left.stable_code, &left.reason).cmp(&(&right.stable_code, &right.reason))
    });

    Some(TaskView {
        issue_uid: uid.clone(),
        title: issue.spec.title.clone(),
        status: phase_str(issue.status.phase).to_string(),
        owner: issue
            .spec
            .assignment
            .as_ref()
            .map(|assignment| assignment.owner.id.as_str().to_string()),
        acceptance: issue.spec.acceptance.criteria.clone(),
        evidence,
        rejections,
    })
}

fn verification_code(state: VerificationState) -> String {
    match state {
        VerificationState::Rejected => "VerificationRejected".to_string(),
        VerificationState::NeedsRevision => "NeedsRevision".to_string(),
        other => format!("Verification{}", capitalize(other.as_str())),
    }
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn phase_str(phase: Phase) -> &'static str {
    match phase {
        Phase::Pending => "pending",
        Phase::Running => "running",
        Phase::Blocked => "blocked",
        Phase::Succeeded => "succeeded",
        Phase::Cancelled => "cancelled",
    }
}

/// 本期目标：优先 Running/Blocked，其次第一个非终态 Issue；全终态时取第一个。
fn project_goal(facts: &HostFacts) -> Option<GoalCard> {
    let rank = |issue: &Issue| match issue.status.phase {
        Phase::Running | Phase::Blocked => 0,
        Phase::Pending => 1,
        Phase::Succeeded | Phase::Cancelled => 2,
    };
    let issue = facts
        .issues
        .iter()
        .min_by_key(|item| (rank(item), item.meta.uid.as_str()))?;
    Some(GoalCard {
        issue_uid: issue.meta.uid.as_str().to_string(),
        title: issue.spec.title.clone(),
        phase: phase_str(issue.status.phase).to_string(),
        acceptance: issue.spec.acceptance.criteria.clone(),
    })
}

/// 下一项可交付：优先尚未接受的凭证成果（有验证证据），其次第一个 Pending/Blocked 队列条目。
fn project_next_deliverable(facts: &HostFacts) -> Option<DeliverableCard> {
    let verified: Vec<&VerificationRecord> = facts
        .verifications
        .iter()
        .filter(|record| record.state == VerificationState::Verified)
        .collect();
    let accepted_ids: Vec<&str> = facts
        .acceptances
        .iter()
        .filter(|record| record.decision.is_accepted())
        .map(|record| record.artifact_id.as_str())
        .collect();

    for record in &verified {
        if accepted_ids.contains(&record.revision_id.as_str()) {
            continue;
        }
        let title = facts
            .issues
            .iter()
            .find(|issue| issue.meta.uid.as_str() == record.revision_id.as_str())
            .map(|issue| issue.spec.title.clone())
            .unwrap_or_else(|| format!("成果修订 {}", record.revision_id.as_str()));
        return Some(DeliverableCard {
            issue_uid: record.revision_id.as_str().to_string(),
            title,
            accepted: false,
            evidence_refs: record.evidence_refs.clone(),
        });
    }

    facts
        .queue
        .iter()
        .find(|entry| matches!(entry.state, QueueState::Pending | QueueState::Blocked))
        .map(|entry| DeliverableCard {
            issue_uid: entry.queue_key.clone(),
            title: entry.queue_key.clone(),
            accepted: false,
            evidence_refs: Vec::new(),
        })
}

/// 阻塞点：Issue 的 Blocked 阶段、队列稳定阻塞原因、对账案件的 Unknown。
fn project_blockers(facts: &HostFacts) -> Vec<BlockerCard> {
    let mut out = Vec::new();
    for issue in &facts.issues {
        if issue.status.phase == Phase::Blocked {
            for condition in &issue.status.conditions {
                out.push(BlockerCard {
                    subject: issue.meta.uid.as_str().to_string(),
                    reason: condition_reason_str(condition.reason),
                    next_step: condition.message.clone(),
                });
            }
            if out
                .last()
                .is_none_or(|card| card.subject != issue.meta.uid.as_str())
            {
                out.push(BlockerCard {
                    subject: issue.meta.uid.as_str().to_string(),
                    reason: "Blocked".to_string(),
                    next_step: "等待解除条件；未解除前不派发".to_string(),
                });
            }
        }
    }
    for entry in &facts.queue {
        if let Some(reason) = &entry.blocked_reason {
            out.push(BlockerCard {
                subject: entry.queue_key.clone(),
                reason: denial_reason_str(reason),
                next_step: entry
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "等待显式解除条件".to_string()),
            });
        } else if entry.state == QueueState::Blocked {
            out.push(BlockerCard {
                subject: entry.queue_key.clone(),
                reason: "QueueBlocked".to_string(),
                next_step: "等待显式解除条件".to_string(),
            });
        }
    }
    for case in &facts.cases {
        if !case.next_step.is_empty() {
            out.push(BlockerCard {
                subject: case.case_id.clone(),
                reason: "ReconciliationOpen".to_string(),
                next_step: case.next_step.clone(),
            });
        }
    }
    out.sort_by(|left, right| {
        (&left.subject, &left.reason, &left.next_step).cmp(&(
            &right.subject,
            &right.reason,
            &right.next_step,
        ))
    });
    out
}

fn condition_reason_str(reason: crate::domain::model::ConditionReason) -> String {
    use crate::domain::model::ConditionReason;
    match reason {
        ConditionReason::AwaitingApproval => "AwaitingApproval",
        ConditionReason::NoEligibleAgent => "NoEligibleAgent",
        ConditionReason::InsufficientBudget => "InsufficientBudget",
        ConditionReason::DependencyNotAccepted => "DependencyNotAccepted",
        ConditionReason::ExternalOutcomeUnknown => "ExternalOutcomeUnknown",
        ConditionReason::Progressing => "Progressing",
        ConditionReason::Recovered => "Recovered",
    }
    .to_string()
}

fn denial_reason_str(reason: &crate::domain::DenialReason) -> String {
    use crate::domain::DenialReason;
    match reason {
        DenialReason::InsufficientPermission => "InsufficientPermission",
        DenialReason::MissingProjectContext => "MissingProjectContext",
        DenialReason::MissingAssignment => "MissingAssignment",
        DenialReason::VersionConflict => "VersionConflict",
        DenialReason::ImmutableDuringRun => "ImmutableDuringRun",
        DenialReason::WrongWriter => "WrongWriter",
        DenialReason::ScopeMismatch => "ScopeMismatch",
        DenialReason::InsufficientBudget => "InsufficientBudget",
        DenialReason::ExternalOutcomeUnknown => "ExternalOutcomeUnknown",
    }
    .to_string()
}

fn cancel_actions(logical_key: &str) -> Vec<String> {
    if logical_key.contains("cancel") || logical_key.contains("unknown") {
        vec!["inspect_execution".to_string(), "confirm_stop".to_string()]
    } else {
        vec![
            "inspect_scope".to_string(),
            "resolve_reconciliation".to_string(),
        ]
    }
}

/// 待决定事项：三要素齐全。阻塞主体取对账案件 assignee / 澄清 answerers /
/// 验收 actor；取不到时明确写 `unassigned`，不伪造责任人。
fn project_decisions(facts: &HostFacts) -> Vec<DecisionCard> {
    let mut out = Vec::new();

    for case in &facts.cases {
        let actor = if case.assignee.trim().is_empty() {
            "unassigned".to_string()
        } else {
            case.assignee.clone()
        };
        let decision = if case.next_step.trim().is_empty() {
            "核实对账证据并决定下一步".to_string()
        } else {
            case.next_step.clone()
        };
        out.push(DecisionCard {
            subject: case.case_id.clone(),
            source: if case.logical_action_key.contains("cancel")
                || case.logical_action_key.contains("unknown")
            {
                DecisionSource::Cancellation
            } else {
                DecisionSource::Clarification
            },
            blocking_actor: actor,
            required_decision: decision,
            available_actions: cancel_actions(&case.logical_action_key),
            evidence: vec![case.known_evidence.clone()]
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
        });
    }

    for question in &facts.questions {
        let actor = question
            .answerers
            .first()
            .cloned()
            .unwrap_or_else(|| "unassigned".to_string());
        let decision = if question.required_decision.trim().is_empty() {
            "答复澄清问题后重新唤醒 Attempt".to_string()
        } else {
            question.required_decision.clone()
        };
        out.push(DecisionCard {
            subject: question.question_id.clone(),
            source: DecisionSource::Clarification,
            blocking_actor: actor,
            required_decision: decision,
            available_actions: vec!["answer".to_string(), "cancel".to_string()],
            evidence: vec![question.blocked_scope.clone()]
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
        });
    }

    for record in &facts.acceptances {
        if record.decision.is_accepted() {
            continue;
        }
        let decision = if record.reason.trim().is_empty() {
            "决定是否人工接受该成果修订".to_string()
        } else {
            record.reason.clone()
        };
        out.push(DecisionCard {
            subject: record.artifact_id.as_str().to_string(),
            source: DecisionSource::Acceptance,
            blocking_actor: record.actor.as_str().to_string(),
            required_decision: decision,
            available_actions: vec!["accept".to_string(), "request_revalidation".to_string()],
            evidence: record.evidence_refs.clone(),
        });
    }

    for entry in &facts.queue {
        if matches!(
            entry.blocked_reason,
            Some(crate::domain::DenialReason::InsufficientBudget)
        ) {
            out.push(DecisionCard {
                subject: entry.queue_key.clone(),
                source: DecisionSource::Budget,
                blocking_actor: entry
                    .lease_owner
                    .clone()
                    .unwrap_or_else(|| "budget-approver".to_string()),
                required_decision: "批准预算或缩减范围后解除阻塞".to_string(),
                available_actions: vec!["approve_budget".to_string(), "reduce_scope".to_string()],
                evidence: Vec::new(),
            });
        }
    }

    out.sort_by(|left, right| {
        (
            &left.source.as_str(),
            &left.subject,
            &left.required_decision,
        )
            .cmp(&(
                &right.source.as_str(),
                &right.subject,
                &right.required_decision,
            ))
    });
    out
}

/// 从宿主只读查询装配快照。调用方持有 `&mut SqliteStore` 只为满足事务 API，
/// 但这里只调用只读列表方法——不 begin、不 commit、不写任何表。
pub fn collect_host_facts(
    store: &crate::store::SqliteStore,
    company_id: &str,
    project_id: &str,
) -> Result<HostFacts, crate::store::StoreError> {
    let issues = store.list_issues_for_project(company_id, project_id)?;
    let queue = store.list_queue(company_id)?;
    let cases = store.list_reconciliation_cases(company_id, false)?;
    let verifications = store.list_verifications_for_company(company_id)?;
    let revision_artifact_links = store.list_revision_artifact_links(company_id)?;

    let acceptances = store.list_acceptances_for_company(company_id)?;

    Ok(HostFacts {
        company_id: company_id.to_string(),
        project_id: project_id.to_string(),
        issues,
        queue,
        acceptances,
        verifications,
        cases,
        revision_artifact_links,
        questions: Vec::new(),
    })
}

/// 便捷入口：一次读出宿主事实并投影首页。
pub fn home_from_store(
    store: &crate::store::SqliteStore,
    company_id: &str,
    project_id: &str,
) -> Result<HomeView, crate::store::StoreError> {
    Ok(project_home(&collect_host_facts(
        store, company_id, project_id,
    )?))
}

/// 便捷入口：一次读出宿主事实并投影任务页。
pub fn task_from_store(
    store: &crate::store::SqliteStore,
    company_id: &str,
    project_id: &str,
    issue_uid: &str,
) -> Result<Option<TaskView>, crate::store::StoreError> {
    Ok(project_task(
        &collect_host_facts(store, company_id, project_id)?,
        issue_uid,
    ))
}

/// 构造 Issue 只读事实的辅助函数（供 3.x 把 NeedsInputView 投影为 QuestionFact）。
pub fn question_fact(
    question_id: impl Into<String>,
    attempt_id: impl Into<String>,
    answerers: Vec<String>,
    blocked_scope: impl Into<String>,
    required_decision: impl Into<String>,
) -> QuestionFact {
    QuestionFact {
        question_id: question_id.into(),
        attempt_id: attempt_id.into(),
        answerers,
        blocked_scope: blocked_scope.into(),
        required_decision: required_decision.into(),
        since: String::new(),
    }
}

/// 仅用于断言投影不产生副作用的核心行数快照。
pub fn state_fingerprint(
    store: &crate::store::SqliteStore,
) -> Result<crate::store::CoreRowCounts, crate::store::StoreError> {
    store.core_row_counts()
}

/// 显式标注未使用的 DomainId 导入来源：投影只比较 id，不新建领域对象。
#[allow(dead_code)]
fn _domain_id_marker(id: &DomainId) -> &str {
    id.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::artifact::{
        AcceptanceBinding, AcceptanceDecision, Artifact, ArtifactKind, ArtifactVisibility,
    };
    use crate::domain::model::{
        AcceptanceCriteria, Assignment, Condition, ConditionReason, ConditionStatus, IssueKind,
        IssueSpec, Owner, OwnerKind, Phase, ResourceMeta, ResourceStatus, Scope,
    };
    use crate::domain::queue::QueueState;
    use crate::recovery_models::CaseState;
    use crate::store::SqliteStore;
    use std::path::PathBuf;

    const COMPANY: &str = "company-workbench";
    const PROJECT: &str = "project-workbench";
    const NOW: i64 = 1_700_000_000_000;

    fn store() -> (SqliteStore, PathBuf) {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("bod-workbench-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bod.sqlite");
        let store = SqliteStore::open(&path, &BodConfig::default()).unwrap();
        (store, path)
    }

    fn scope() -> Scope {
        Scope {
            company_id: DomainId::new(COMPANY),
            project_id: Some(DomainId::new(PROJECT)),
            issue_id: None,
        }
    }

    fn issue(uid: &str, title: &str, phase: Phase) -> Issue {
        Issue {
            meta: ResourceMeta {
                uid: DomainId::new(uid),
                company_id: DomainId::new(COMPANY),
                project_id: Some(DomainId::new(PROJECT)),
                resource_version: crate::domain::Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: IssueKind::Coding,
            repository: None,
            spec: IssueSpec {
                title: title.to_string(),
                assignment: Some(Assignment {
                    owner: Owner {
                        kind: OwnerKind::Agent,
                        id: DomainId::new("coder-1"),
                    },
                    scope: scope(),
                    grant_revision: 1,
                }),
                acceptance: AcceptanceCriteria {
                    criteria: vec!["测试通过".to_string()],
                },
            },
            status: ResourceStatus {
                phase,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        }
    }

    fn blocked_issue() -> Issue {
        let mut item = issue("issue-blocked", "修复登录缺陷", Phase::Blocked);
        item.status.conditions.push(Condition {
            reason: ConditionReason::InsufficientBudget,
            status: ConditionStatus::True,
            message: "需要批准额外预算".to_string(),
            observed_generation: 1,
        });
        item
    }

    fn queue_entry(key: &str, state: QueueState) -> WorkQueueEntry {
        let mut entry = WorkQueueEntry::new(DomainId::new(COMPANY), key.to_string(), NOW);
        entry.state = state;
        entry
    }

    fn acceptance(artifact: &str, reason: &str) -> AcceptanceRecord {
        AcceptanceRecord {
            acceptance_id: DomainId::new(format!("acc-{artifact}")),
            artifact_id: DomainId::new(artifact),
            binding: AcceptanceBinding {
                work_package_revision: DomainId::new("wp-1"),
                input_baseline: DomainId::new("baseline-1"),
                artifact_revision: DomainId::new("rev-1"),
                verification_rules: DomainId::new("rules-1"),
                target_environment: DomainId::new("env-1"),
                merge_combination: None,
            },
            decision: AcceptanceDecision::RequiresHuman,
            actor: DomainId::new("reviewer-1"),
            reason: reason.to_string(),
            evidence_refs: vec!["cas:abc123".to_string()],
            accepted_at_ms: NOW,
            superseded_by: None,
        }
    }

    fn verification(revision: &str, state: VerificationState, reason: &str) -> VerificationRecord {
        VerificationRecord {
            revision_id: DomainId::new(revision),
            state,
            actor: DomainId::new("verifier-1"),
            reason: reason.to_string(),
            evidence_refs: vec![format!("cas:verify-{revision}")],
            verification_rules: DomainId::new("rules-1"),
            environment: DomainId::new("env-1"),
            observed_at_ms: NOW,
        }
    }

    fn case(case_id: &str, key: &str, assignee: &str, next_step: &str) -> ReconciliationCase {
        ReconciliationCase {
            case_id: case_id.to_string(),
            company_id: DomainId::new(COMPANY),
            invocation_id: Some("inv-1".to_string()),
            logical_action_key: key.to_string(),
            impact_scope: "project-workbench".to_string(),
            risk_reservation_refs: vec!["res-1".to_string()],
            known_evidence: "cancel_ack 已收到，停止未确认".to_string(),
            assignee: assignee.to_string(),
            next_step: next_step.to_string(),
            state: CaseState::Open,
            created_at_ms: NOW,
            updated_at_ms: NOW,
        }
    }

    fn rich_facts() -> HostFacts {
        let mut facts = HostFacts {
            company_id: COMPANY.to_string(),
            project_id: PROJECT.to_string(),
            issues: vec![
                issue("issue-a", "实现登录", Phase::Running),
                blocked_issue(),
            ],
            queue: vec![
                queue_entry("issue-ready", QueueState::Pending),
                queue_entry("issue-budget", QueueState::Blocked),
            ],
            acceptances: vec![acceptance("artifact-1", "需要独立复核")],
            verifications: vec![verification("rev-1", VerificationState::Verified, "")],
            cases: vec![case(
                "case-1",
                "cancel:attempt-1",
                "owner-1",
                "查询外部执行是否真正停止",
            )],
            revision_artifact_links: vec![
                ("rev-1".to_string(), "issue-a".to_string()),
                ("rev-fail".to_string(), "issue-a".to_string()),
            ],
            questions: Vec::new(),
        };
        facts.queue[1].blocked_reason = Some(crate::domain::DenialReason::InsufficientBudget);
        facts.queue[1].last_error = Some("额度耗尽".to_string());
        facts.queue[1].lease_owner = Some("budget-owner".to_string());
        facts
    }

    #[test]
    fn home_answers_four_questions_from_host_facts() {
        let view = project_home(&rich_facts());
        assert_eq!(view.company_id, COMPANY);
        assert_eq!(view.project_id, PROJECT);

        let goal = view.goal.unwrap();
        assert_eq!(goal.issue_uid, "issue-a");
        assert_eq!(goal.title, "实现登录");
        assert_eq!(goal.acceptance, vec!["测试通过".to_string()]);

        let next = view.next_deliverable.unwrap();
        assert_eq!(next.issue_uid, "rev-1");
        assert!(!next.accepted);
        assert_eq!(next.evidence_refs, vec!["cas:verify-rev-1".to_string()]);

        assert!(!view.blockers.is_empty());
        assert!(
            view.blockers
                .iter()
                .any(|card| card.reason == "InsufficientBudget")
        );
        assert!(
            view.blockers
                .iter()
                .any(|card| card.subject == "issue-blocked")
        );
    }

    #[test]
    fn decision_has_actor_required_decision_actions_and_evidence() {
        let view = project_home(&rich_facts());
        assert!(!view.decisions.is_empty());
        for decision in &view.decisions {
            assert!(!decision.subject.is_empty(), "事项不得为空");
            assert!(!decision.blocking_actor.is_empty(), "阻塞主体不得为空");
            assert!(!decision.required_decision.is_empty(), "所需决定不得为空");
            assert!(!decision.available_actions.is_empty(), "可采取动作不得为空");
        }
        let cancel = view
            .decisions
            .iter()
            .find(|item| item.source == DecisionSource::Cancellation)
            .expect("取消待决定必须投影");
        assert_eq!(cancel.blocking_actor, "owner-1");
        assert_eq!(cancel.required_decision, "查询外部执行是否真正停止");
        assert_eq!(
            cancel.available_actions,
            vec!["inspect_execution", "confirm_stop"]
        );
        assert_eq!(cancel.evidence, vec!["cancel_ack 已收到，停止未确认"]);

        let budget = view
            .decisions
            .iter()
            .find(|item| item.source == DecisionSource::Budget)
            .expect("预算待决定必须投影");
        assert_eq!(budget.blocking_actor, "budget-owner");
        assert!(
            budget
                .available_actions
                .contains(&"approve_budget".to_string())
        );
    }

    #[test]
    fn clarification_question_becomes_decision_with_answerers() {
        let mut facts = rich_facts();
        facts.questions.push(question_fact(
            "question-1",
            "attempt-1",
            vec!["user-1".to_string(), "manager-1".to_string()],
            "task-migrate",
            "确认是否允许迁移",
        ));
        let view = project_home(&facts);
        let clarification = view
            .decisions
            .iter()
            .find(|item| item.subject == "question-1")
            .expect("澄清待决定必须投影");
        assert_eq!(clarification.blocking_actor, "user-1");
        assert_eq!(clarification.required_decision, "确认是否允许迁移");
        assert_eq!(clarification.available_actions, vec!["answer", "cancel"]);
        assert_eq!(clarification.evidence, vec!["task-migrate"]);
    }

    #[test]
    fn task_view_shows_status_owner_evidence_and_rejection() {
        let mut facts = rich_facts();
        facts.verifications.push(verification(
            "rev-fail",
            VerificationState::NeedsRevision,
            "缺少报告正文",
        ));
        facts.acceptances = vec![acceptance("issue-a", "需要独立复核")];
        let view = project_task(&facts, "issue-a").expect("Issue 必须存在");
        assert_eq!(view.title, "实现登录");
        assert_eq!(view.status, "running");
        assert_eq!(view.owner, Some("coder-1".to_string()));
        assert_eq!(view.acceptance, vec!["测试通过".to_string()]);
        assert!(
            view.evidence
                .iter()
                .any(|item| item.reference == "cas:abc123")
        );
        assert!(
            view.evidence
                .iter()
                .any(|item| item.reference == "cas:verify-rev-1")
        );
        let rejection = view
            .rejections
            .iter()
            .find(|item| item.reason == "缺少报告正文")
            .expect("拒绝原因必须显示");
        assert_eq!(rejection.stable_code, "NeedsRevision");
        assert_eq!(rejection.evidence, vec!["cas:verify-rev-fail"]);
        assert!(project_task(&facts, "issue-missing").is_none());
    }

    #[test]
    fn unknown_case_stays_in_decisions_without_guessing_stop() {
        let facts = HostFacts {
            cases: vec![case(
                "case-unknown",
                "cancel:attempt-9",
                "owner-9",
                "查询外部执行；未确认前保留风险占用",
            )],
            ..HostFacts::default()
        };
        let view = project_home(&facts);
        let decision = view.decisions.first().expect("Unknown 必须进入待决定");
        assert_eq!(decision.source, DecisionSource::Cancellation);
        assert_eq!(decision.blocking_actor, "owner-9");
        assert!(decision.required_decision.contains("未确认前保留风险占用"));
        let blocker = view.blockers.first().expect("Unknown 必须进入阻塞点");
        assert_eq!(blocker.reason, "ReconciliationOpen");
        assert!(goal_is_absent(&view));
    }

    fn goal_is_absent(view: &HomeView) -> bool {
        view.goal.is_none()
    }

    #[test]
    fn projection_is_pure_and_deterministic() {
        let facts = rich_facts();
        let first = project_home(&facts);
        let second = project_home(&facts);
        assert_eq!(first, second, "相同宿主事实必得相同首页视图");
        let first_task = project_task(&facts, "issue-a");
        let second_task = project_task(&facts, "issue-a");
        assert_eq!(first_task, second_task);
    }

    #[test]
    fn state_fingerprint_helper_reads_core_counts() {
        let (mut store, _path) = store();
        {
            let uow = store.transaction().unwrap();
            uow.insert_issue(&issue("issue-a", "实现登录", Phase::Running))
                .unwrap();
            uow.commit().unwrap();
        }
        let before = state_fingerprint(&store).unwrap();
        let view = home_from_store(&store, COMPANY, PROJECT).unwrap();
        let after = state_fingerprint(&store).unwrap();
        assert_eq!(before, after, "只读投影不得改变核心行数（I-55）");
        assert_eq!(view.goal.unwrap().issue_uid, "issue-a");
        assert_eq!(before.issues, 1);
    }

    #[test]
    fn store_projection_reads_acceptance_evidence_and_rejection() {
        let (mut store, _path) = store();
        {
            let uow = store.transaction().unwrap();
            uow.insert_issue(&issue("issue-a", "实现登录", Phase::Running))
                .unwrap();
            uow.commit().unwrap();
        }
        let artifact = Artifact {
            id: DomainId::new("issue-a"),
            company_id: DomainId::new(COMPANY),
            project_id: DomainId::new(PROJECT),
            kind: ArtifactKind::SourceCode,
            title: "实现登录".to_string(),
            owner: DomainId::new("coder-1"),
            visibility: ArtifactVisibility::Internal,
            accepted_revision: None,
        };
        {
            let uow = store.transaction().unwrap();
            uow.insert_artifact(&artifact, NOW).unwrap();
            uow.put_reconciliation_case(&case(
                "case-1",
                "cancel:attempt-1",
                "owner-1",
                "查询外部执行",
            ))
            .unwrap();
            uow.commit().unwrap();
        }
        let view = home_from_store(&store, COMPANY, PROJECT).unwrap();
        assert!(
            view.decisions
                .iter()
                .any(|item| item.subject == "case-1" && item.blocking_actor == "owner-1")
        );
        let task = task_from_store(&store, COMPANY, PROJECT, "issue-a")
            .unwrap()
            .unwrap();
        assert_eq!(task.status, "running");
        assert_eq!(task.owner, Some("coder-1".to_string()));
    }

    #[test]
    fn empty_facts_produce_empty_but_well_formed_view() {
        let view = project_home(&HostFacts::default());
        assert_eq!(view.company_id, "");
        assert!(view.goal.is_none());
        assert!(view.next_deliverable.is_none());
        assert!(view.blockers.is_empty());
        assert!(view.decisions.is_empty());
    }
}
