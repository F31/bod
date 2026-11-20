//! P2 任务 3.4：历史成果与费用的连续（`invariant_i_61`）。
//!
//! 权威语义：V2.9.6 §39.11 I-61「管理切换不默认取消正常员工、删除成果或重置预算」；
//! manager-transfer「更换负责人 SHALL NOT 修改 Artifact/Revision、Manifest、Acceptance、
//! 历史费用主体或旧决定；撤任不删除旧经理的代码、报告与决定。争议记录 MUST 标待核查，
//! 不能通过换人篡改历史」；场景「新经理要求把已接受成果全部重验 → 保留旧接受并要求
//! 沿影响与重验规则提出有理由的新提案」。
//!
//! 三段责任，边界分明：
//! 1. **不可改写**由数据库保证（005 的 `acceptances_immutable` 与 003 的账本 no_update，
//!    012 补齐接受五元组/理由/证据的冻结与取代指针单调），本模块只负责在换人前后
//!    逐字段比对，把「已经发生的改写」变成可核对的差异，而不是信任调用方自述。
//! 2. **重验请求**没有「覆盖旧接受」的出口：本模块恒定返回「保留 + 需要新提案」，
//!    提案执行属 P3 影响评估/选择性重验，这里只给出要求与保留清单。
//! 3. **争议**只写 `history_disputes` 一行状态，被争议的成果、接受、账本、决定
//!    一字不改；结案为「维持」或「推翻」都只是结论记录，推翻另需提案。
//!
//! 不做的事：不判定管理权（那是 3.1），不产生管理票证（[`dispute_as_ticket`] 恒错），
//! 不实现影响图与重验调度（P3），不建第二事实源——视图里的每条事实都从既有表读。

use crate::domain::artifact::{AcceptanceDecision, AcceptanceRecord, Artifact, sha256_hex};
use crate::domain::budget::LedgerEntry;
use crate::store::{SqliteStore, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// 视图 schema 标识：说明事实指纹的口径，不做跨版本自动解释。
pub const HISTORY_SCHEMA: &str = "bod.history-continuity/v1";

/// 单个作用域的事实上限：超过即报错，不截断（截断会把「有改动」变成「没改动」）。
pub const MAX_FACTS: usize = 16384;

/// 重验提案在 P3 的落地位置；本切片只给出要求，不假装已实现。
pub const PROPOSAL_REQUIREMENT: &str = "p3-impact-reverification";

// ---------------------------------------------------------------------------
// 事实与视图
// ---------------------------------------------------------------------------

/// 参与历史连续核对的事实种类。顺序固定，用于游标文本的稳定前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryFactKind {
    Artifact,
    Revision,
    Manifest,
    Acceptance,
    Ledger,
    Decision,
}

impl HistoryFactKind {
    pub const ALL: [Self; 6] = [
        Self::Artifact,
        Self::Revision,
        Self::Manifest,
        Self::Acceptance,
        Self::Ledger,
        Self::Decision,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::Revision => "revision",
            Self::Manifest => "manifest",
            Self::Acceptance => "acceptance",
            Self::Ledger => "ledger",
            Self::Decision => "decision",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "artifact" => Some(Self::Artifact),
            "revision" => Some(Self::Revision),
            "manifest" => Some(Self::Manifest),
            "acceptance" => Some(Self::Acceptance),
            "ledger" => Some(Self::Ledger),
            "decision" => Some(Self::Decision),
            _ => None,
        }
    }
}

/// 一条历史事实的身份与内容指纹。
///
/// `pointer` 只放「允许向前推进一次」的引用：成果的 `accepted_revision`、接受的
/// `superseded_by`。它们不进内容指纹，改写/回退单独判为 [`RewindFact`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryFact {
    pub kind: HistoryFactKind,
    pub id: String,
    pub fingerprint: String,
    pub pointer: Option<String>,
}

impl HistoryFact {
    /// 该条接受是否算「已接受成果」（accepted / auto_accepted）；用于重验范围。
    pub fn is_accepted_acceptance(&self) -> bool {
        self.kind == HistoryFactKind::Acceptance
            && (self.fingerprint.contains("|decision=accepted|")
                || self.fingerprint.contains("|decision=auto_accepted|"))
    }
}

/// 某次只读核对看到的历史切片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryView {
    pub schema: String,
    pub company_id: String,
    pub project_id: String,
    pub captured_at_ms: i64,
    pub facts: Vec<HistoryFact>,
    /// 对排序后的 `kind\tid\tfingerprint\tpointer` 文本求 sha256。
    pub cursor: String,
}

impl HistoryView {
    /// 从既有表装配视图。纯读：不写任何表，不触发派发。
    pub fn capture(
        store: &SqliteStore,
        company_id: &str,
        project_id: &str,
        now_ms: i64,
    ) -> Result<Self, HistoryError> {
        if company_id.trim().is_empty() {
            return Err(HistoryError::EmptyField("company_id"));
        }
        if project_id.trim().is_empty() {
            return Err(HistoryError::EmptyField("project_id"));
        }

        let mut facts = Vec::new();

        for artifact in store.list_artifacts(company_id, project_id)? {
            facts.push(HistoryFact {
                kind: HistoryFactKind::Artifact,
                id: artifact.id.as_str().to_string(),
                fingerprint: artifact_fingerprint(&artifact),
                pointer: artifact
                    .accepted_revision
                    .as_ref()
                    .map(|value| value.as_str().to_string()),
            });
        }

        for row in store.list_project_revision_bodies(company_id, project_id)? {
            facts.push(HistoryFact {
                kind: HistoryFactKind::Revision,
                id: row.revision_id.clone(),
                fingerprint: format!(
                    "artifact={}|producer={}|digest={}|size={}|mime={}|created={}",
                    row.artifact_id,
                    row.produced_by,
                    row.digest,
                    row.size_bytes,
                    row.mime,
                    row.created_at_ms
                ),
                pointer: None,
            });
        }

        for row in store.list_project_manifests(company_id, project_id)? {
            facts.push(HistoryFact {
                kind: HistoryFactKind::Manifest,
                id: row.uid.clone(),
                fingerprint: format!(
                    "revision={}|milestone={}|refs={}|acceptances={}|signature={}",
                    row.revision,
                    row.milestone,
                    row.artifact_revision_refs,
                    row.acceptance_refs,
                    row.signature_ref.as_deref().unwrap_or("-")
                ),
                pointer: None,
            });
        }

        for record in store.list_project_acceptances(company_id, project_id)? {
            facts.push(HistoryFact {
                kind: HistoryFactKind::Acceptance,
                id: record.acceptance_id.as_str().to_string(),
                fingerprint: acceptance_fingerprint(&record),
                pointer: record
                    .superseded_by
                    .as_ref()
                    .map(|value| value.as_str().to_string()),
            });
        }

        // 账本按项目归属过滤：无项目归属的分录不属于本作用域（公司级预留不在此列）。
        for entry in store.list_ledger_entries(company_id)? {
            if entry.project_id.as_ref().map(|id| id.as_str()) != Some(project_id) {
                continue;
            }
            facts.push(HistoryFact {
                kind: HistoryFactKind::Ledger,
                id: entry.entry_id.clone(),
                fingerprint: ledger_fingerprint(&entry),
                pointer: None,
            });
        }

        for row in store.list_project_decisions(company_id, project_id)? {
            facts.push(HistoryFact {
                kind: HistoryFactKind::Decision,
                id: row.decision_id.clone(),
                fingerprint: format!(
                    "actor={}|decision={}|reason={}|evidence={}|revisions={}|created={}",
                    row.actor_id,
                    row.decision,
                    row.reason,
                    row.evidence_refs,
                    row.revision_refs,
                    row.created_at_ms
                ),
                pointer: None,
            });
        }

        if facts.len() > MAX_FACTS {
            return Err(HistoryError::TooManyFacts(facts.len()));
        }

        facts.sort_by(|left, right| {
            (left.kind, left.id.as_str()).cmp(&(right.kind, right.id.as_str()))
        });
        let cursor = cursor_of(&facts);

        Ok(Self {
            schema: HISTORY_SCHEMA.to_string(),
            company_id: company_id.to_string(),
            project_id: project_id.to_string(),
            captured_at_ms: now_ms,
            facts,
            cursor,
        })
    }

    pub fn fingerprint_of(&self, kind: HistoryFactKind, id: &str) -> Option<&str> {
        self.facts
            .iter()
            .find(|fact| fact.kind == kind && fact.id == id)
            .map(|fact| fact.fingerprint.as_str())
    }

    pub fn pointer_of(&self, kind: HistoryFactKind, id: &str) -> Option<Option<&str>> {
        self.facts
            .iter()
            .find(|fact| fact.kind == kind && fact.id == id)
            .map(|fact| fact.pointer.as_deref())
    }

    /// 按种类计数，供接管报告直接展示「读到了什么」。
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for fact in &self.facts {
            *counts.entry(fact.kind.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// 与更早的视图比对，要求历史未被改写/删除。
    ///
    /// 只允许**追加**：新增成果、新修订、新接受、新账本分录、新决定都是正常前进；
    /// 已有行的内容变化、指针回退、整行消失一律报错。
    pub fn require_preserved(&self, before: &HistoryView) -> Result<HistoryDelta, HistoryError> {
        let delta = compare(before, self)?;
        if delta.is_clean() {
            Ok(delta)
        } else {
            Err(HistoryError::HistoryRewritten {
                altered: delta.altered.len(),
                rewinded: delta.rewinded.len(),
                vanished: delta.vanished.len(),
            })
        }
    }
}

/// 内容被改写的事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlteredFact {
    pub kind: HistoryFactKind,
    pub id: String,
    pub before: String,
    pub after: String,
}

/// 前进指针被回退或改写的事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindFact {
    pub kind: HistoryFactKind,
    pub id: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// 整行消失的事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VanishedFact {
    pub kind: HistoryFactKind,
    pub id: String,
}

/// 换人前后的历史差异。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryDelta {
    pub altered: Vec<AlteredFact>,
    pub rewinded: Vec<RewindFact>,
    pub vanished: Vec<VanishedFact>,
    pub appended: Vec<HistoryFact>,
}

impl HistoryDelta {
    /// 无改写、无回退、无消失。
    pub fn is_clean(&self) -> bool {
        self.altered.is_empty() && self.rewinded.is_empty() && self.vanished.is_empty()
    }

    /// 新增事实数（允许的前进）。
    pub fn appended_count(&self) -> usize {
        self.appended.len()
    }
}

/// 比对两个同作用域视图。
pub fn compare(before: &HistoryView, after: &HistoryView) -> Result<HistoryDelta, HistoryError> {
    if before.company_id != after.company_id || before.project_id != after.project_id {
        return Err(HistoryError::ScopeMismatch {
            expected: format!("{}/{}", before.company_id, before.project_id),
            actual: format!("{}/{}", after.company_id, after.project_id),
        });
    }

    let mut delta = HistoryDelta::default();
    for fact in &before.facts {
        match after
            .facts
            .iter()
            .find(|item| item.kind == fact.kind && item.id == fact.id)
        {
            None => delta.vanished.push(VanishedFact {
                kind: fact.kind,
                id: fact.id.clone(),
            }),
            Some(current) => {
                if current.fingerprint != fact.fingerprint {
                    delta.altered.push(AlteredFact {
                        kind: fact.kind,
                        id: fact.id.clone(),
                        before: fact.fingerprint.clone(),
                        after: current.fingerprint.clone(),
                    });
                }
                if current.pointer != fact.pointer {
                    let forward = matches!((&fact.pointer, &current.pointer), (None, Some(_)));
                    if !forward {
                        delta.rewinded.push(RewindFact {
                            kind: fact.kind,
                            id: fact.id.clone(),
                            before: fact.pointer.clone(),
                            after: current.pointer.clone(),
                        });
                    }
                }
            }
        }
    }

    for fact in &after.facts {
        if !before
            .facts
            .iter()
            .any(|item| item.kind == fact.kind && item.id == fact.id)
        {
            delta.appended.push(fact.clone());
        }
    }

    Ok(delta)
}

fn cursor_of(facts: &[HistoryFact]) -> String {
    let mut text = String::new();
    for fact in facts {
        text.push_str(fact.kind.as_str());
        text.push('\t');
        text.push_str(&fact.id);
        text.push('\t');
        text.push_str(&fact.fingerprint);
        text.push('\t');
        text.push_str(fact.pointer.as_deref().unwrap_or("-"));
        text.push('\n');
    }
    format!("sha256:{}", sha256_hex(text.as_bytes()))
}

fn artifact_fingerprint(artifact: &Artifact) -> String {
    format!(
        "kind={}|owner={}|visibility={}",
        artifact.kind.as_str(),
        artifact.owner.as_str(),
        artifact.visibility.as_str()
    )
}

fn acceptance_fingerprint(record: &AcceptanceRecord) -> String {
    format!(
        "artifact={}|wp={}|baseline={}|revision={}|rules={}|env={}|merge={}|decision={}|actor={}|reason={}|evidence={}|accepted={}",
        record.artifact_id.as_str(),
        record.binding.work_package_revision.as_str(),
        record.binding.input_baseline.as_str(),
        record.binding.artifact_revision.as_str(),
        record.binding.verification_rules.as_str(),
        record.binding.target_environment.as_str(),
        record
            .binding
            .merge_combination
            .as_ref()
            .map(|value| value.as_str())
            .unwrap_or("-"),
        match record.decision {
            AcceptanceDecision::Accepted => "accepted",
            AcceptanceDecision::AutoAccepted => "auto_accepted",
            AcceptanceDecision::RequiresHuman => "requires_human",
        },
        record.actor.as_str(),
        record.reason,
        record.evidence_refs.join(","),
        record.accepted_at_ms
    )
}

/// 账本分录指纹：**含费用主体**（项目/执行者/工作包）与金额口径。
fn ledger_fingerprint(entry: &LedgerEntry) -> String {
    format!(
        "project={}|agent={}|wp={}|kind={}|dimension={}|currency={}|amount={}|source={}|reason={}|created={}",
        entry
            .project_id
            .as_ref()
            .map(|value| value.as_str())
            .unwrap_or("-"),
        entry
            .agent_id
            .as_ref()
            .map(|value| value.as_str())
            .unwrap_or("-"),
        entry
            .work_package_id
            .as_ref()
            .map(|value| value.as_str())
            .unwrap_or("-"),
        entry.kind.as_str(),
        entry.measure.dimension.as_str(),
        entry.measure.currency.as_deref().unwrap_or("-"),
        entry.amount,
        entry.source,
        entry.reason.as_deref().unwrap_or("-"),
        entry.created_at_ms
    )
}

// ---------------------------------------------------------------------------
// 追溯改动请求：没有成功出口
// ---------------------------------------------------------------------------

/// 新经理可能提出的「改写历史」类请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetroactiveAsk {
    /// 把已接受成果全部重验（允许，但只能新增提案，旧接受保留）。
    ReverifyAccepted { artifact_id: Option<String> },
    /// 改旧接受的判定或五元组。
    RewriteAcceptance { acceptance_id: String },
    /// 把已入账费用的主体换到别人/别的项目。
    ReassignFeeSubject { entry_id: String, new_agent: String },
    /// 删除旧经理的决定、成果或修订。
    DeleteHistory { kind: HistoryFactKind, id: String },
}

/// 追溯请求的处置结果：只有两种。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetroactiveOutcome {
    /// 旧记录保留，改走新提案（重验场景）。
    PreserveThenPropose {
        preserved: Vec<String>,
        requirement: &'static str,
    },
}

impl RetroactiveAsk {
    /// 判定一次追溯请求。除「重验」外一律拒绝，且拒绝原因可区分。
    pub fn classify(&self, view: &HistoryView) -> Result<RetroactiveOutcome, HistoryError> {
        match self {
            Self::ReverifyAccepted { artifact_id } => {
                let preserved = view
                    .facts
                    .iter()
                    .filter(|fact| fact.is_accepted_acceptance())
                    .filter(|fact| match artifact_id {
                        Some(target) => {
                            view.acceptance_artifact(&fact.id).as_deref() == Some(target.as_str())
                        }
                        None => true,
                    })
                    .map(|fact| fact.id.clone())
                    .collect::<Vec<_>>();
                if preserved.is_empty() {
                    return Err(HistoryError::NoAcceptedWork);
                }
                Ok(RetroactiveOutcome::PreserveThenPropose {
                    preserved,
                    requirement: PROPOSAL_REQUIREMENT,
                })
            }
            Self::RewriteAcceptance { acceptance_id } => {
                Err(HistoryError::AcceptanceIsImmutable(acceptance_id.clone()))
            }
            Self::ReassignFeeSubject {
                entry_id,
                new_agent,
            } => Err(HistoryError::FeeSubjectIsImmutable {
                entry_id: entry_id.clone(),
                attempted_subject: new_agent.clone(),
            }),
            Self::DeleteHistory { kind, id } => Err(HistoryError::HistoryIsAppendOnly {
                kind: *kind,
                id: id.clone(),
            }),
        }
    }
}

impl HistoryView {
    /// 接受行对应的成果 ID（从指纹里取，避免再查一次库）。
    pub fn acceptance_artifact(&self, acceptance_id: &str) -> Option<String> {
        let fact = self
            .facts
            .iter()
            .find(|fact| fact.kind == HistoryFactKind::Acceptance && fact.id == acceptance_id)?;
        fact.fingerprint
            .strip_prefix("artifact=")?
            .split('|')
            .next()
            .map(|value| value.to_string())
    }

    /// 已接受的成果清单（`decision` 为 accepted / auto_accepted）。
    pub fn accepted_artifacts(&self) -> Vec<String> {
        let mut result = Vec::new();
        for fact in &self.facts {
            if !fact.is_accepted_acceptance() {
                continue;
            }
            if let Some(artifact) = self.acceptance_artifact(&fact.id)
                && !result.contains(&artifact)
            {
                result.push(artifact);
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// 争议：只标待核查
// ---------------------------------------------------------------------------

/// 争议状态。`under_review` 是唯一可写状态，结案只允许前进一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisputeState {
    UnderReview,
    Upheld,
    Overturned,
}

impl DisputeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnderReview => "under_review",
            Self::Upheld => "upheld",
            Self::Overturned => "overturned",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "under_review" => Some(Self::UnderReview),
            "upheld" => Some(Self::Upheld),
            "overturned" => Some(Self::Overturned),
            _ => None,
        }
    }

    pub const fn is_final(self) -> bool {
        !matches!(self, Self::UnderReview)
    }
}

/// 提出争议的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputeRequest {
    pub dispute_id: String,
    pub company_id: String,
    pub project_id: String,
    pub kind: HistoryFactKind,
    pub fact_id: String,
    pub reason: String,
    pub evidence_refs: Vec<String>,
    pub raised_by: String,
    pub at_ms: i64,
}

impl DisputeRequest {
    /// 必填校验在构造处一次做完；理由为空不允许「无理由质疑」。
    // 参数多但都是争议行的必填事实；拆结构体会掩盖「谁在何时对哪条事实提了什么」。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dispute_id: &str,
        company_id: &str,
        project_id: &str,
        kind: HistoryFactKind,
        fact_id: &str,
        reason: &str,
        evidence_refs: Vec<String>,
        raised_by: &str,
        at_ms: i64,
    ) -> Result<Self, HistoryError> {
        for (field, value) in [
            ("dispute_id", dispute_id),
            ("company_id", company_id),
            ("project_id", project_id),
            ("fact_id", fact_id),
            ("raised_by", raised_by),
        ] {
            if value.trim().is_empty() {
                return Err(HistoryError::EmptyField(field));
            }
        }
        if reason.trim().is_empty() {
            return Err(HistoryError::EmptyField("reason"));
        }
        if evidence_refs.iter().all(|item| item.trim().is_empty()) && evidence_refs.is_empty() {
            return Err(HistoryError::EmptyField("evidence_refs"));
        }
        Ok(Self {
            dispute_id: dispute_id.to_string(),
            company_id: company_id.to_string(),
            project_id: project_id.to_string(),
            kind,
            fact_id: fact_id.to_string(),
            reason: reason.trim().to_string(),
            evidence_refs,
            raised_by: raised_by.to_string(),
            at_ms,
        })
    }
}

/// 争议行（`history_disputes` 的读回形状）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisputeRecord {
    pub dispute_id: String,
    pub company_id: String,
    pub project_id: String,
    pub kind: HistoryFactKind,
    pub fact_id: String,
    pub fact_fingerprint: String,
    pub reason: String,
    pub evidence_refs: Vec<String>,
    pub raised_by: String,
    pub state: DisputeState,
    pub resolution: Option<String>,
    pub resolved_by: Option<String>,
    pub resolved_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// 争议结案后的后续动作。推翻结论不改历史，只要求走新提案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub requirement: &'static str,
    pub kind: HistoryFactKind,
    pub fact_id: String,
    pub note: String,
}

/// 结案结果：行 + 可能需要的后续提案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDispute {
    pub record: DisputeRecord,
    pub follow_up: Option<FollowUp>,
}

impl DisputeRecord {
    /// 登记争议：只读核对事实存在且指纹一致，然后插入一行 `under_review`。
    ///
    /// 不写成果/接受/账本/决定表；不接受「顺手改一下原记录」的附带请求。
    pub fn raise(store: &mut SqliteStore, request: &DisputeRequest) -> Result<Self, HistoryError> {
        let view = HistoryView::capture(
            store,
            &request.company_id,
            &request.project_id,
            request.at_ms,
        )?;
        let fingerprint = view
            .fingerprint_of(request.kind, &request.fact_id)
            .ok_or_else(|| HistoryError::UnknownFact {
                kind: request.kind,
                id: request.fact_id.clone(),
            })?
            .to_string();

        if let Some(existing) = store.get_history_dispute(&request.dispute_id)? {
            let existing = Self::from_row(existing)?;
            let same = existing.company_id == request.company_id
                && existing.project_id == request.project_id
                && existing.kind == request.kind
                && existing.fact_id == request.fact_id
                && existing.reason == request.reason
                && existing.raised_by == request.raised_by
                && existing.evidence_refs == request.evidence_refs;
            if same {
                return Ok(existing);
            }
            return Err(HistoryError::DisputeKeyReuse(request.dispute_id.clone()));
        }

        let row = crate::store::DisputeRow {
            dispute_id: request.dispute_id.clone(),
            company_id: request.company_id.clone(),
            project_id: request.project_id.clone(),
            fact_kind: request.kind.as_str().to_string(),
            fact_id: request.fact_id.clone(),
            fact_fingerprint: fingerprint,
            reason: request.reason.clone(),
            evidence_refs: request.evidence_refs.join(","),
            raised_by: request.raised_by.clone(),
            state: DisputeState::UnderReview.as_str().to_string(),
            resolution: None,
            resolved_by: None,
            resolved_at_ms: None,
            created_at_ms: request.at_ms,
            updated_at_ms: request.at_ms,
        };
        store.insert_history_dispute(&row)?;
        Self::load(store, &request.dispute_id)?
            .ok_or_else(|| HistoryError::CorruptDispute("登记后读不回".to_string()))
    }

    pub fn load(store: &SqliteStore, dispute_id: &str) -> Result<Option<Self>, HistoryError> {
        match store.get_history_dispute(dispute_id)? {
            None => Ok(None),
            Some(row) => Ok(Some(Self::from_row(row)?)),
        }
    }

    pub fn list(
        store: &SqliteStore,
        company_id: &str,
        project_id: &str,
        state: Option<DisputeState>,
    ) -> Result<Vec<Self>, HistoryError> {
        let rows =
            store.list_history_disputes(company_id, project_id, state.map(DisputeState::as_str))?;
        rows.into_iter().map(Self::from_row).collect()
    }

    /// 结案：只能从 `under_review` 前进一次；结论不修改被争议的历史。
    pub fn resolve(
        store: &mut SqliteStore,
        dispute_id: &str,
        verdict: DisputeState,
        resolution: &str,
        resolved_by: &str,
        at_ms: i64,
    ) -> Result<ResolvedDispute, HistoryError> {
        if !verdict.is_final() {
            return Err(HistoryError::DisputeVerdictRequired);
        }
        if resolution.trim().is_empty() {
            return Err(HistoryError::EmptyField("resolution"));
        }
        if resolved_by.trim().is_empty() {
            return Err(HistoryError::EmptyField("resolved_by"));
        }
        let current =
            Self::load(store, dispute_id)?.ok_or_else(|| HistoryError::UnknownDispute {
                dispute_id: dispute_id.to_string(),
            })?;
        if current.state.is_final() {
            return Err(HistoryError::DisputeIsFinal(current.state));
        }
        let updated = store.resolve_history_dispute(
            dispute_id,
            verdict.as_str(),
            resolution.trim(),
            resolved_by,
            at_ms,
        )?;
        if !updated {
            // 并发结案：另一个事务先到，本请求不再改判。
            return Err(HistoryError::DisputeIsFinal(current.state));
        }
        let record = Self::load(store, dispute_id)?
            .ok_or_else(|| HistoryError::CorruptDispute("结案后读不回".to_string()))?;
        let follow_up = match verdict {
            DisputeState::Overturned => Some(FollowUp {
                requirement: PROPOSAL_REQUIREMENT,
                kind: record.kind,
                fact_id: record.fact_id.clone(),
                note: "推翻结论只登记结论：原记录保留，须沿影响与重验规则提出新提案".to_string(),
            }),
            _ => None,
        };
        Ok(ResolvedDispute { record, follow_up })
    }

    fn from_row(row: crate::store::DisputeRow) -> Result<Self, HistoryError> {
        let kind = HistoryFactKind::parse(&row.fact_kind).ok_or_else(|| {
            HistoryError::CorruptDispute(format!("未知事实种类 {}", row.fact_kind))
        })?;
        let state = DisputeState::parse(&row.state)
            .ok_or_else(|| HistoryError::CorruptDispute(format!("未知状态 {}", row.state)))?;
        Ok(Self {
            dispute_id: row.dispute_id,
            company_id: row.company_id,
            project_id: row.project_id,
            kind,
            fact_id: row.fact_id,
            fact_fingerprint: row.fact_fingerprint,
            reason: row.reason,
            evidence_refs: split_list(&row.evidence_refs),
            raised_by: row.raised_by,
            state,
            resolution: row.resolution,
            resolved_by: row.resolved_by,
            resolved_at_ms: row.resolved_at_ms,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
        })
    }

    /// 争议当前看到的指纹与此刻是否一致；不一致说明对象在争议期间被改动。
    pub fn check_drift(&self, view: &HistoryView) -> Result<(), HistoryError> {
        let current = view
            .fingerprint_of(self.kind, &self.fact_id)
            .ok_or_else(|| HistoryError::UnknownFact {
                kind: self.kind,
                id: self.fact_id.clone(),
            })?;
        if current == self.fact_fingerprint {
            Ok(())
        } else {
            Err(HistoryError::FactDriftSinceCapture {
                kind: self.kind,
                id: self.fact_id.clone(),
            })
        }
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect()
}

/// 争议标记不产生管理权（与 3.2/3.3 同口径）：类型上就没有成功出口。
pub fn dispute_as_ticket(_record: &DisputeRecord) -> Result<(), HistoryError> {
    Err(HistoryError::DisputeIsNotAuthority)
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum HistoryError {
    Store(StoreError),
    EmptyField(&'static str),
    TooManyFacts(usize),
    ScopeMismatch {
        expected: String,
        actual: String,
    },
    HistoryRewritten {
        altered: usize,
        rewinded: usize,
        vanished: usize,
    },
    AcceptanceIsImmutable(String),
    FeeSubjectIsImmutable {
        entry_id: String,
        attempted_subject: String,
    },
    HistoryIsAppendOnly {
        kind: HistoryFactKind,
        id: String,
    },
    NoAcceptedWork,
    UnknownFact {
        kind: HistoryFactKind,
        id: String,
    },
    FactDriftSinceCapture {
        kind: HistoryFactKind,
        id: String,
    },
    DisputeKeyReuse(String),
    UnknownDispute {
        dispute_id: String,
    },
    DisputeIsFinal(DisputeState),
    DisputeVerdictRequired,
    DisputeIsNotAuthority,
    CorruptDispute(String),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "存储错误: {error}"),
            Self::EmptyField(field) => write!(formatter, "字段 {field} 不能为空"),
            Self::TooManyFacts(count) => write!(
                formatter,
                "作用域内历史事实 {count} 条超过上限 {MAX_FACTS}，不截断"
            ),
            Self::ScopeMismatch { expected, actual } => {
                write!(formatter, "作用域不一致：期望 {expected}，实际 {actual}")
            }
            Self::HistoryRewritten {
                altered,
                rewinded,
                vanished,
            } => write!(
                formatter,
                "历史被改写：内容变化 {altered} 条、指针回退 {rewinded} 条、消失 {vanished} 条"
            ),
            Self::AcceptanceIsImmutable(id) => {
                write!(
                    formatter,
                    "接受记录 {id} 不可改写，只能新增接受并前进取代指针"
                )
            }
            Self::FeeSubjectIsImmutable {
                entry_id,
                attempted_subject,
            } => write!(
                formatter,
                "账本分录 {entry_id} 的费用主体不可改到 {attempted_subject}：历史费用主体随分录冻结"
            ),
            Self::HistoryIsAppendOnly { kind, id } => {
                write!(formatter, "{} {id} 只可追加，不可删除", kind.as_str())
            }
            Self::NoAcceptedWork => write!(formatter, "作用域内没有已接受成果，重验提案无对象"),
            Self::UnknownFact { kind, id } => {
                write!(formatter, "作用域内不存在 {} {id}", kind.as_str())
            }
            Self::FactDriftSinceCapture { kind, id } => write!(
                formatter,
                "{} {id} 自争议提出后已变化，不能按提出时的现场下结论",
                kind.as_str()
            ),
            Self::DisputeKeyReuse(id) => {
                write!(formatter, "争议 {id} 已存在且内容不同，不能挪用同一 ID")
            }
            Self::UnknownDispute { dispute_id } => write!(formatter, "争议 {dispute_id} 不存在"),
            Self::DisputeIsFinal(state) => {
                write!(
                    formatter,
                    "争议已结案（{}），要改结论请另开一条",
                    state.as_str()
                )
            }
            Self::DisputeVerdictRequired => write!(formatter, "结案必须给出维持或推翻"),
            Self::DisputeIsNotAuthority => write!(formatter, "争议标记不是管理凭据，不产生管理权"),
            Self::CorruptDispute(detail) => write!(formatter, "争议行不可解释：{detail}"),
        }
    }
}

impl std::error::Error for HistoryError {}

impl From<StoreError> for HistoryError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::DomainId;
    use crate::domain::artifact::{
        AcceptanceBinding, AcceptanceDecision, AcceptanceRecord, ArtifactKind, ArtifactRevision,
        ArtifactVisibility, DecisionRecord, Digest, DigestAlgorithm, StorageRef,
    };
    use crate::domain::budget::{Dimension, LedgerEntry, LedgerEntryKind, Measure};
    use crate::domain::model::Scope;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const COMPANY: &str = "company-1";
    const PROJECT: &str = "project-1";
    const NOW_MS: i64 = 1_764_100_000_000;

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-history-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
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
            owner: DomainId::new("agent-old"),
            visibility: ArtifactVisibility::Internal,
            accepted_revision: None,
        };
        let uow = store.transaction().unwrap();
        uow.insert_artifact(&artifact, NOW_MS).unwrap();
        uow.commit().unwrap();
    }

    fn add_revision(store: &mut SqliteStore, revision_id: &str, artifact_id: &str) {
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
            produced_by: DomainId::new("agent-old"),
            source_note: "历史样本".to_string(),
            depends_on: Vec::new(),
            created_at_ms: NOW_MS,
        };
        let uow = store.transaction().unwrap();
        uow.insert_artifact_revision(COMPANY, &revision).unwrap();
        uow.commit().unwrap();
    }

    fn accept(
        store: &mut SqliteStore,
        acceptance_id: &str,
        artifact_id: &str,
        decision: AcceptanceDecision,
    ) {
        let record = AcceptanceRecord {
            acceptance_id: DomainId::new(acceptance_id),
            artifact_id: DomainId::new(artifact_id),
            binding: AcceptanceBinding {
                work_package_revision: DomainId::new("wp-1"),
                input_baseline: DomainId::new("input-1"),
                artifact_revision: DomainId::new(format!("rev-{artifact_id}")),
                verification_rules: DomainId::new("rules-1"),
                target_environment: DomainId::new("env-1"),
                merge_combination: None,
            },
            decision,
            actor: DomainId::new("user-owner"),
            reason: "旧经理验收通过".to_string(),
            evidence_refs: vec!["test:evidence-1".to_string()],
            accepted_at_ms: NOW_MS,
            superseded_by: None,
        };
        let uow = store.transaction().unwrap();
        uow.insert_acceptance(COMPANY, &record).unwrap();
        uow.commit().unwrap();
    }

    fn add_ledger(store: &mut SqliteStore, entry_id: &str, agent: &str) {
        let entry = LedgerEntry {
            entry_id: entry_id.to_string(),
            company_id: DomainId::new(COMPANY),
            project_id: Some(DomainId::new(PROJECT)),
            agent_id: Some(DomainId::new(agent)),
            work_package_id: Some(DomainId::new("wp-1")),
            reservation_id: None,
            usage_key: None,
            kind: LedgerEntryKind::Commit,
            measure: Measure {
                dimension: Dimension::MoneyMicros,
                value: 0,
                currency: Some("CNY".to_string()),
            },
            amount: 12_000,
            source: "provider:meter".to_string(),
            reason: Some("旧经理在任期间的实际费用".to_string()),
            created_at_ms: NOW_MS,
        };
        let uow = store.transaction().unwrap();
        uow.insert_ledger_entry(&entry).unwrap();
        uow.commit().unwrap();
    }
    fn add_decision(store: &mut SqliteStore, decision_id: &str) {
        let record = DecisionRecord {
            decision_id: DomainId::new(decision_id),
            actor: DomainId::new("agent-old"),
            scope: Scope {
                company_id: DomainId::new(COMPANY),
                project_id: Some(DomainId::new(PROJECT)),
                issue_id: None,
            },
            decision: "接受交付".to_string(),
            reason: "旧经理的决定".to_string(),
            evidence_refs: vec!["test:evidence-1".to_string()],
            revision_refs: Vec::new(),
            created_at_ms: NOW_MS,
        };
        let uow = store.transaction().unwrap();
        uow.insert_decision(COMPANY, &record).unwrap();
        uow.commit().unwrap();
    }

    fn add_manifest(store: &SqliteStore, manifest_id: &str) {
        store
            .put_manifest(
                COMPANY,
                manifest_id,
                PROJECT,
                1,
                &["rev-art-1".to_string()],
                &["acc-1".to_string()],
                NOW_MS,
            )
            .unwrap();
    }

    fn seeded(tag: &str) -> (SqliteStore, PathBuf) {
        let root = temp_root(tag);
        let mut store = open_store(&root);
        add_artifact(&mut store, "art-1");
        add_revision(&mut store, "rev-art-1", "art-1");
        accept(&mut store, "acc-1", "art-1", AcceptanceDecision::Accepted);
        add_ledger(&mut store, "led-1", "agent-old");
        add_decision(&mut store, "dec-1");
        add_manifest(&store, "mf-1");
        (store, root)
    }

    fn capture(store: &SqliteStore) -> HistoryView {
        HistoryView::capture(store, COMPANY, PROJECT, NOW_MS).unwrap()
    }

    #[test]
    fn capture_covers_six_kinds_and_is_stable() {
        let (store, _root) = seeded("capture");
        let view = capture(&store);
        let counts = view.counts();
        assert_eq!(counts.get("artifact"), Some(&1));
        assert_eq!(counts.get("revision"), Some(&1));
        assert_eq!(counts.get("manifest"), Some(&1));
        assert_eq!(counts.get("acceptance"), Some(&1));
        assert_eq!(counts.get("ledger"), Some(&1));
        assert_eq!(counts.get("decision"), Some(&1));
        assert!(view.cursor.starts_with("sha256:"));
        // 没有变化时两次捕获的游标一致（排序稳定，不依赖插入顺序）。
        assert_eq!(capture(&store).cursor, view.cursor);
        assert_eq!(view.schema, HISTORY_SCHEMA);
    }

    #[test]
    fn appending_new_history_is_not_a_rewrite() {
        let (mut store, _root) = seeded("append");
        let before = capture(&store);
        add_artifact(&mut store, "art-2");
        add_revision(&mut store, "rev-art-2", "art-2");
        accept(&mut store, "acc-2", "art-2", AcceptanceDecision::Accepted);
        add_ledger(&mut store, "led-2", "agent-old");

        let after = capture(&store);
        let delta = after.require_preserved(&before).unwrap();
        assert!(delta.is_clean());
        assert_eq!(delta.appended_count(), 4);
    }

    #[test]
    fn pure_compare_detects_rewrite_rewind_and_vanish() {
        let base = HistoryView {
            schema: HISTORY_SCHEMA.to_string(),
            company_id: COMPANY.to_string(),
            project_id: PROJECT.to_string(),
            captured_at_ms: NOW_MS,
            facts: vec![
                HistoryFact {
                    kind: HistoryFactKind::Acceptance,
                    id: "acc-1".to_string(),
                    fingerprint: "decision=accepted".to_string(),
                    pointer: None,
                },
                HistoryFact {
                    kind: HistoryFactKind::Artifact,
                    id: "art-1".to_string(),
                    fingerprint: "kind=report".to_string(),
                    pointer: Some("rev-1".to_string()),
                },
                HistoryFact {
                    kind: HistoryFactKind::Decision,
                    id: "dec-1".to_string(),
                    fingerprint: "decision=接受".to_string(),
                    pointer: None,
                },
            ],
            cursor: "sha256:00".to_string(),
        };
        let mut later = base.clone();
        later.facts[0].fingerprint = "decision=requires_human".to_string();
        // 接受被新一条取代：指针从 NULL 前进，属于允许的前进，不算回退。
        later.facts[0].pointer = Some("acc-2".to_string());
        later.facts.pop();

        let delta = compare(&base, &later).unwrap();
        assert_eq!(delta.altered.len(), 1);
        assert_eq!(delta.altered[0].id, "acc-1");
        assert_eq!(delta.vanished.len(), 1);
        assert_eq!(delta.vanished[0].kind, HistoryFactKind::Decision);
        assert!(delta.rewinded.is_empty(), "指针前进不算回退");
        assert!(later.require_preserved(&base).is_err());

        // 指针回退不是前进，必须报出来。
        let mut rewind = later;
        rewind.facts[1].pointer = None;
        let delta = compare(&base, &rewind).unwrap();
        assert_eq!(delta.rewinded.len(), 1);
        assert_eq!(delta.rewinded[0].kind, HistoryFactKind::Artifact);
    }

    #[test]
    fn compare_rejects_cross_scope() {
        let (store, _root) = seeded("scope");
        let view = capture(&store);
        let mut other = view.clone();
        other.project_id = "project-2".to_string();
        match compare(&view, &other) {
            Err(HistoryError::ScopeMismatch { expected, actual }) => {
                assert!(expected.contains("project-1"));
                assert!(actual.contains("project-2"));
            }
            other => panic!("跨作用域应拒绝，实际 {other:?}"),
        }
    }

    #[test]
    fn acceptance_binding_and_reason_cannot_be_updated() {
        let (store, _root) = seeded("binding");
        for sql in [
            "UPDATE acceptances SET target_environment = 'env-2' WHERE acceptance_id = 'acc-1'",
            "UPDATE acceptances SET work_package_revision = 'wp-2' WHERE acceptance_id = 'acc-1'",
            "UPDATE acceptances SET reason = '新经理改了理由' WHERE acceptance_id = 'acc-1'",
            "UPDATE acceptances SET evidence_refs = 'test:forged' WHERE acceptance_id = 'acc-1'",
        ] {
            let error = store.execute_batch_for_test(sql).unwrap_err().to_string();
            assert!(
                error.contains("acceptance binding is immutable"),
                "应被五元组冻结拦住，实际 {error}"
            );
        }
        // 冻结的是绑定与理由，行本身仍在。
        assert_eq!(capture(&store).counts().get("acceptance"), Some(&1));
    }

    #[test]
    fn supersede_pointer_moves_forward_only() {
        let (store, _root) = seeded("supersede");
        store
            .execute_batch_for_test(
                "UPDATE acceptances SET superseded_by = 'acc-2' WHERE acceptance_id = 'acc-1'",
            )
            .unwrap();
        let error = store
            .execute_batch_for_test(
                "UPDATE acceptances SET superseded_by = 'acc-3' WHERE acceptance_id = 'acc-1'",
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("monotonic"),
            "改写指向应被拒绝，实际 {error}"
        );
        let error = store
            .execute_batch_for_test(
                "UPDATE acceptances SET superseded_by = NULL WHERE acceptance_id = 'acc-1'",
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("monotonic"),
            "清空指向应被拒绝，实际 {error}"
        );
    }

    #[test]
    fn ledger_subject_and_amount_are_frozen() {
        let (store, _root) = seeded("ledger");
        for sql in [
            "UPDATE ledger_entries SET agent_id = 'agent-new' WHERE entry_id = 'led-1'",
            "UPDATE ledger_entries SET amount = 1 WHERE entry_id = 'led-1'",
            "DELETE FROM ledger_entries WHERE entry_id = 'led-1'",
        ] {
            let error = store.execute_batch_for_test(sql).unwrap_err().to_string();
            assert!(
                error.contains("ledger entries are immutable"),
                "账本分录应不可改写，实际 {error}"
            );
        }
    }

    #[test]
    fn deleted_decision_shows_up_as_vanished_not_cleaned() {
        let (store, _root) = seeded("vanish");
        let before = capture(&store);
        store
            .execute_batch_for_test("DELETE FROM decisions WHERE decision_id = 'dec-1'")
            .unwrap();
        let after = capture(&store);
        match after.require_preserved(&before) {
            Err(HistoryError::HistoryRewritten { vanished, .. }) => assert_eq!(vanished, 1),
            other => panic!("删除旧决定应被识别为历史改写，实际 {other:?}"),
        }
    }

    #[test]
    fn reverify_keeps_old_acceptances_and_requires_proposal() {
        let (mut store, _root) = seeded("reverify");
        add_artifact(&mut store, "art-2");
        add_revision(&mut store, "rev-art-2", "art-2");
        accept(
            &mut store,
            "acc-2",
            "art-2",
            AcceptanceDecision::AutoAccepted,
        );
        let before = capture(&store);

        let outcome = RetroactiveAsk::ReverifyAccepted { artifact_id: None }
            .classify(&before)
            .unwrap();
        match outcome {
            RetroactiveOutcome::PreserveThenPropose {
                preserved,
                requirement,
            } => {
                assert_eq!(preserved, vec!["acc-1".to_string(), "acc-2".to_string()]);
                assert_eq!(requirement, PROPOSAL_REQUIREMENT);
            }
        }
        // 旧接受一条没少、一字未改。
        let after = capture(&store);
        assert!(after.require_preserved(&before).unwrap().is_clean());
        assert_eq!(after.accepted_artifacts(), vec!["art-1", "art-2"]);
    }

    #[test]
    fn single_artifact_reverify_is_scoped() {
        let (mut store, _root) = seeded("reverify-one");
        add_artifact(&mut store, "art-2");
        accept(&mut store, "acc-2", "art-2", AcceptanceDecision::Accepted);
        let view = capture(&store);
        match (RetroactiveAsk::ReverifyAccepted {
            artifact_id: Some("art-2".to_string()),
        })
        .classify(&view)
        .unwrap()
        {
            RetroactiveOutcome::PreserveThenPropose { preserved, .. } => {
                assert_eq!(preserved, vec!["acc-2".to_string()]);
            }
        }
    }

    #[test]
    fn reverify_without_accepted_work_is_refused() {
        let root = temp_root("reverify-empty");
        let mut store = open_store(&root);
        add_artifact(&mut store, "art-1");
        add_revision(&mut store, "rev-art-1", "art-1");
        accept(
            &mut store,
            "acc-1",
            "art-1",
            AcceptanceDecision::RequiresHuman,
        );
        let view = capture(&store);
        assert!(matches!(
            RetroactiveAsk::ReverifyAccepted { artifact_id: None }.classify(&view),
            Err(HistoryError::NoAcceptedWork)
        ));
    }

    #[test]
    fn retroactive_rewrite_delete_and_fee_reassign_have_no_success_path() {
        let (store, _root) = seeded("no-path");
        let view = capture(&store);

        match (RetroactiveAsk::RewriteAcceptance {
            acceptance_id: "acc-1".to_string(),
        })
        .classify(&view)
        {
            Err(HistoryError::AcceptanceIsImmutable(id)) => assert_eq!(id, "acc-1"),
            other => panic!("应拒绝改写旧接受，实际 {other:?}"),
        }

        match (RetroactiveAsk::ReassignFeeSubject {
            entry_id: "led-1".to_string(),
            new_agent: "agent-new".to_string(),
        })
        .classify(&view)
        {
            Err(HistoryError::FeeSubjectIsImmutable {
                entry_id,
                attempted_subject,
            }) => {
                assert_eq!(entry_id, "led-1");
                assert_eq!(attempted_subject, "agent-new");
            }
            other => panic!("应拒绝改费用主体，实际 {other:?}"),
        }

        match (RetroactiveAsk::DeleteHistory {
            kind: HistoryFactKind::Decision,
            id: "dec-1".to_string(),
        })
        .classify(&view)
        {
            Err(HistoryError::HistoryIsAppendOnly { kind, id }) => {
                assert_eq!(kind, HistoryFactKind::Decision);
                assert_eq!(id, "dec-1");
            }
            other => panic!("应拒绝删除历史，实际 {other:?}"),
        }
    }

    #[test]
    fn dispute_marks_under_review_without_touching_history() {
        let (mut store, _root) = seeded("dispute");
        let before = capture(&store);
        let request = DisputeRequest::new(
            "dis-1",
            COMPANY,
            PROJECT,
            HistoryFactKind::Acceptance,
            "acc-1",
            "怀疑旧经理验收时漏了失败用例",
            vec!["test:log-42".to_string()],
            "agent-new",
            NOW_MS + 1_000,
        )
        .unwrap();
        let record = DisputeRecord::raise(&mut store, &request).unwrap();
        assert_eq!(record.state, DisputeState::UnderReview);
        assert_eq!(
            record.fact_fingerprint,
            before
                .fingerprint_of(HistoryFactKind::Acceptance, "acc-1")
                .unwrap()
        );
        assert!(record.resolved_at_ms.is_none());

        // 被争议的历史一字未改，只是多了一行待核查。
        let after = capture(&store);
        assert!(after.require_preserved(&before).unwrap().is_clean());
        assert_eq!(
            DisputeRecord::list(&store, COMPANY, PROJECT, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            DisputeRecord::list(&store, COMPANY, PROJECT, Some(DisputeState::UnderReview))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn dispute_requires_reason_evidence_and_existing_fact() {
        assert!(matches!(
            DisputeRequest::new(
                "dis-x",
                COMPANY,
                PROJECT,
                HistoryFactKind::Acceptance,
                "acc-1",
                "   ",
                vec!["test:log".to_string()],
                "agent-new",
                NOW_MS
            ),
            Err(HistoryError::EmptyField("reason"))
        ));
        assert!(matches!(
            DisputeRequest::new(
                "dis-x",
                COMPANY,
                PROJECT,
                HistoryFactKind::Acceptance,
                "acc-1",
                "有疑问",
                Vec::new(),
                "agent-new",
                NOW_MS
            ),
            Err(HistoryError::EmptyField("evidence_refs"))
        ));

        let (mut store, _root) = seeded("dispute-unknown");
        let request = DisputeRequest::new(
            "dis-2",
            COMPANY,
            PROJECT,
            HistoryFactKind::Acceptance,
            "acc-missing",
            "指向不存在的接受",
            vec!["test:log".to_string()],
            "agent-new",
            NOW_MS,
        )
        .unwrap();
        match DisputeRecord::raise(&mut store, &request) {
            Err(HistoryError::UnknownFact { kind, id }) => {
                assert_eq!(kind, HistoryFactKind::Acceptance);
                assert_eq!(id, "acc-missing");
            }
            other => panic!("争议对象必须存在，实际 {other:?}"),
        }
    }

    #[test]
    fn dispute_id_is_idempotent_but_not_reusable_for_other_content() {
        let (mut store, _root) = seeded("dispute-idem");
        let request = DisputeRequest::new(
            "dis-1",
            COMPANY,
            PROJECT,
            HistoryFactKind::Acceptance,
            "acc-1",
            "第一条理由",
            vec!["test:log".to_string()],
            "agent-new",
            NOW_MS,
        )
        .unwrap();
        let first = DisputeRecord::raise(&mut store, &request).unwrap();
        let again = DisputeRecord::raise(&mut store, &request).unwrap();
        assert_eq!(first, again);

        let mut other = request.clone();
        other.reason = "换一个理由".to_string();
        assert!(matches!(
            DisputeRecord::raise(&mut store, &other),
            Err(HistoryError::DisputeKeyReuse(_))
        ));
        assert_eq!(
            DisputeRecord::list(&store, COMPANY, PROJECT, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn upheld_keeps_history_and_records_verdict() {
        let (mut store, _root) = seeded("upheld");
        let before = capture(&store);
        let request = DisputeRequest::new(
            "dis-1",
            COMPANY,
            PROJECT,
            HistoryFactKind::Acceptance,
            "acc-1",
            "怀疑验收依据不足",
            vec!["test:log".to_string()],
            "agent-new",
            NOW_MS,
        )
        .unwrap();
        DisputeRecord::raise(&mut store, &request).unwrap();
        let resolved = DisputeRecord::resolve(
            &mut store,
            "dis-1",
            DisputeState::Upheld,
            "核对原始验证记录后维持原接受",
            "user-owner",
            NOW_MS + 5_000,
        )
        .unwrap();
        assert_eq!(resolved.record.state, DisputeState::Upheld);
        assert!(resolved.follow_up.is_none());
        assert_eq!(resolved.record.resolved_by.as_deref(), Some("user-owner"));
        assert!(
            capture(&store)
                .require_preserved(&before)
                .unwrap()
                .is_clean()
        );
    }

    #[test]
    fn overturned_requires_proposal_and_still_does_not_rewrite() {
        let (mut store, _root) = seeded("overturned");
        let before = capture(&store);
        let request = DisputeRequest::new(
            "dis-1",
            COMPANY,
            PROJECT,
            HistoryFactKind::Acceptance,
            "acc-1",
            "失败用例未覆盖",
            vec!["test:log".to_string()],
            "agent-new",
            NOW_MS,
        )
        .unwrap();
        DisputeRecord::raise(&mut store, &request).unwrap();
        let resolved = DisputeRecord::resolve(
            &mut store,
            "dis-1",
            DisputeState::Overturned,
            "确认漏验，需按影响规则重新提案",
            "user-owner",
            NOW_MS + 5_000,
        )
        .unwrap();
        assert_eq!(resolved.record.state, DisputeState::Overturned);
        let follow_up = resolved.follow_up.expect("推翻必须给出后续提案要求");
        assert_eq!(follow_up.requirement, PROPOSAL_REQUIREMENT);
        assert_eq!(follow_up.fact_id, "acc-1");
        // 旧接受仍是原来那一条，没有被「推翻」改写。
        assert!(
            capture(&store)
                .require_preserved(&before)
                .unwrap()
                .is_clean()
        );

        // 已结案不能改判或重开。
        assert!(matches!(
            DisputeRecord::resolve(
                &mut store,
                "dis-1",
                DisputeState::Upheld,
                "改判",
                "user-owner",
                NOW_MS + 9_000
            ),
            Err(HistoryError::DisputeIsFinal(DisputeState::Overturned))
        ));
        let error = store
            .execute_batch_for_test(
                "UPDATE history_disputes SET state = 'under_review' WHERE dispute_id = 'dis-1'",
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("state is final"),
            "重开应被拒绝，实际 {error}"
        );
    }

    #[test]
    fn dispute_subject_is_frozen_and_not_deletable() {
        let (mut store, _root) = seeded("dispute-freeze");
        let request = DisputeRequest::new(
            "dis-1",
            COMPANY,
            PROJECT,
            HistoryFactKind::Decision,
            "dec-1",
            "怀疑决定依据不足",
            vec!["test:log".to_string()],
            "agent-new",
            NOW_MS,
        )
        .unwrap();
        DisputeRecord::raise(&mut store, &request).unwrap();

        let error = store
            .execute_batch_for_test(
                "UPDATE history_disputes SET fact_id = 'dec-2' WHERE dispute_id = 'dis-1'",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("subject is immutable"), "实际 {error}");
        let error = store
            .execute_batch_for_test("DELETE FROM history_disputes WHERE dispute_id = 'dis-1'")
            .unwrap_err()
            .to_string();
        assert!(error.contains("append-only"), "实际 {error}");
    }

    #[test]
    fn drift_is_reported_when_disputed_fact_changes_after_raising() {
        let (mut store, _root) = seeded("drift");
        let request = DisputeRequest::new(
            "dis-1",
            COMPANY,
            PROJECT,
            HistoryFactKind::Decision,
            "dec-1",
            "理由可疑",
            vec!["test:log".to_string()],
            "agent-new",
            NOW_MS,
        )
        .unwrap();
        let record = DisputeRecord::raise(&mut store, &request).unwrap();
        record.check_drift(&capture(&store)).unwrap();

        // 决定表没有不可变 trigger：这里正好用它证明「比对能看到偏移」。
        store
            .execute_batch_for_test(
                "UPDATE decisions SET reason = '事后改口' WHERE decision_id = 'dec-1'",
            )
            .unwrap();
        match record.check_drift(&capture(&store)) {
            Err(HistoryError::FactDriftSinceCapture { kind, id }) => {
                assert_eq!(kind, HistoryFactKind::Decision);
                assert_eq!(id, "dec-1");
            }
            other => panic!("争议对象变化应报偏移，实际 {other:?}"),
        }
    }

    #[test]
    fn dispute_is_not_an_authority() {
        let (mut store, _root) = seeded("authority");
        let request = DisputeRequest::new(
            "dis-1",
            COMPANY,
            PROJECT,
            HistoryFactKind::Acceptance,
            "acc-1",
            "有疑问",
            vec!["test:log".to_string()],
            "agent-new",
            NOW_MS,
        )
        .unwrap();
        let record = DisputeRecord::raise(&mut store, &request).unwrap();
        assert!(matches!(
            dispute_as_ticket(&record),
            Err(HistoryError::DisputeIsNotAuthority)
        ));
    }

    #[test]
    fn scope_counts_only_own_project() {
        let (mut store, _root) = seeded("other-project");
        // 另一个项目的成果与费用不进本作用域视图。
        let foreign = Artifact {
            id: DomainId::new("art-foreign"),
            company_id: DomainId::new(COMPANY),
            project_id: DomainId::new("project-2"),
            kind: ArtifactKind::Report,
            title: "别的项目".to_string(),
            owner: DomainId::new("agent-old"),
            visibility: ArtifactVisibility::Internal,
            accepted_revision: None,
        };
        let uow = store.transaction().unwrap();
        uow.insert_artifact(&foreign, NOW_MS).unwrap();
        uow.commit().unwrap();
        let view = capture(&store);
        assert!(
            view.fingerprint_of(HistoryFactKind::Artifact, "art-foreign")
                .is_none()
        );
    }
}
