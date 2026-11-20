//! P2 任务 1.3：交接骨架的宿主事实与 Agent 叙述分栏（纯值类型）。
//!
//! 边界：本模块只做装配与判定，不读 store、不写库、不读文件、不授予任何权限。
//! 输入分成两组且**类型上不可互换**：
//! - [`HostFactsInput`]：由宿主从版本化工作区、已登记成果、验证记录、Effect 与预算快照读取；
//! - [`NarrativeInput`]：Agent 自述（做过什么、失败原因、建议、自称已完成）。
//!
//! 结果中 [`HandoffAssembly::effects`] / `verifications` / `budget` / `workspace` 只来自宿主输入，
//! 叙述无法写入这些字段；叙述只出现在 `narrative` 分栏，并通过 [`NarrativeConflict`]
//! 显式列出与宿主账本矛盾的地方。完成度只看宿主验证记录：Agent 自称 done 但宿主没有
//! 验证证据时保持 `Unverified`，宿主事实仍显示未完成（§18.11，I-46）。
//!
//! 摘要：`payload_digest()` 复用 1.2 的 `bod-canonical-json-v1`，覆盖 `payload` 内容；
//! 冲突列表是派生视图，不写进 payload（同一宿主事实必得同一 payload 摘要）。

use crate::canonical_json::{self, CanonicalError, CanonicalValue};
use crate::domain::DomainId;
use crate::domain::artifact::{Digest, VerificationState};
use crate::domain::queue::EffectStatus;
use crate::handoff_package::HandoffPackageHeader;
use std::collections::BTreeSet;
use std::fmt;

/// 版本化工作区事实（§18.10 `workspace`）。空字符串一律视为未提供，避免把占位符当真。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceFacts {
    pub base_commit: String,
    pub committed_ref: Option<String>,
    pub uncommitted_patch_ref: Option<String>,
    pub untracked_files_manifest_ref: Option<String>,
    pub environment_ref: Option<String>,
}

impl WorkspaceFacts {
    /// 是否具备重建所需的最小信息：基线提交 + 环境定义。
    pub fn is_rebuildable(&self) -> bool {
        !self.base_commit.trim().is_empty()
            && self
                .environment_ref
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }
}

/// 已登记成果事实。`accepted` 来自成果的 `accepted_revision` 指针，不由叙述决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFact {
    pub artifact_id: DomainId,
    pub revision: DomainId,
    pub accepted: bool,
}

/// 验证记录事实。`actor == producer` 视为自报，不能作为独立证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationFact {
    pub revision_id: DomainId,
    pub state: VerificationState,
    pub evidence_refs: Vec<String>,
    pub environment: DomainId,
    pub verification_rules: DomainId,
    pub producer: DomainId,
    pub actor: DomainId,
}

impl VerificationFact {
    /// 自报：产出者自己写的验证记录，不计入完成度。
    pub fn is_self_reported(&self) -> bool {
        self.actor == self.producer
    }

    /// 可计入宿主完成度的证据：已通过、非自报、且有证据引用。
    fn counts_as_host_evidence(&self) -> bool {
        self.state.is_verified() && !self.is_self_reported() && !self.evidence_refs.is_empty()
    }
}

/// 副作用事实。状态只来自宿主 Effect 记录，叙述不能改写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectFact {
    pub effect_id: String,
    pub logical_action_key: String,
    pub status: EffectStatus,
}

/// 预算快照事实。只保存引用与三项占用，不把余额当授权。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetFact {
    pub snapshot_ref: String,
    pub limit: i64,
    pub confirmed: i64,
    pub reserved: i64,
    pub risk: i64,
}

impl BudgetFact {
    pub fn available(&self) -> i64 {
        self.limit - self.confirmed - self.reserved - self.risk
    }
}

/// 宿主输入。构造后由 [`assemble`] 原样复制到结果，不接受叙述参与。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFactsInput {
    pub workspace: WorkspaceFacts,
    pub artifacts: Vec<ArtifactFact>,
    pub verifications: Vec<VerificationFact>,
    pub effects: Vec<EffectFact>,
    pub budget: BudgetFact,
    /// 未能取得的部分（崩溃/强制停止时的 missing 列表）。非空即部分快照。
    pub missing: Vec<String>,
    /// 采集时间，必须是 1.2 的规范时间（秒级 UTC）。
    pub captured_at: String,
}

/// Agent 自述。这里是**唯一**允许 Agent 写入的入口，字段均不参与完成度判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarrativeInput {
    pub author: DomainId,
    pub summary_ref: Option<String>,
    pub claimed_done: bool,
    /// Agent 自称的证据引用。与宿主账本不符时只记冲突，不提升为证据。
    pub claimed_evidence_refs: Vec<String>,
    pub failure_reason_ref: Option<String>,
    pub suggestion_refs: Vec<String>,
    pub open_question_refs: Vec<String>,
    /// 试图声明的 Effect 终态。与宿主不同即冲突，宿主状态保持不变。
    pub claimed_effect_states: Vec<(String, EffectStatus)>,
}

impl NarrativeInput {
    pub fn new(author: impl Into<String>) -> Self {
        Self {
            author: DomainId::new(author),
            summary_ref: None,
            claimed_done: false,
            claimed_evidence_refs: Vec::new(),
            failure_reason_ref: None,
            suggestion_refs: Vec::new(),
            open_question_refs: Vec::new(),
            claimed_effect_states: Vec::new(),
        }
    }
}

/// 完成度。只由宿主验证记录推导，Agent 的 `claimed_done` 无法直接影响它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionStatus {
    /// 目标修订存在宿主验证通过且非自报的证据。
    Completed { evidence_refs: Vec<String> },
    /// 未完成，附带宿主侧的原因。
    Incomplete { reason: IncompleteReason },
}

impl CompletionStatus {
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteReason {
    /// 该工作包修订没有登记成果。
    NoRegisteredArtifact,
    /// 有成果但没有验证记录。
    NoVerificationRecorded,
    /// 只有产出者自报的验证通过记录。
    OnlySelfReportedEvidence,
    /// 验证标记通过但没有任何证据引用。
    VerificationMissingEvidence,
    /// 存在被拒或需返工的验证记录。
    VerificationRejected,
}

impl IncompleteReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoRegisteredArtifact => "no_registered_artifact",
            Self::NoVerificationRecorded => "no_verification_recorded",
            Self::OnlySelfReportedEvidence => "only_self_reported_evidence",
            Self::VerificationMissingEvidence => "verification_missing_evidence",
            Self::VerificationRejected => "verification_rejected",
        }
    }
}

impl fmt::Display for IncompleteReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 叙述相对宿主账本的状态。`Corroborated` 仍只是「叙述与宿主一致」，不等于宿主事实本身。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarrativeStatus {
    /// 没有自称完成。
    NoClaim,
    /// 自称完成但宿主没有可用证据。
    Unverified,
    /// 自称完成且宿主验证记录支持。
    Corroborated,
    /// 自称完成但宿主记录显示验证失败或需返工。
    ContradictsHost,
}

impl NarrativeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoClaim => "no_claim",
            Self::Unverified => "unverified",
            Self::Corroborated => "corroborated",
            Self::ContradictsHost => "contradicts_host",
        }
    }
}

/// 叙述与宿主账本的具体冲突。用于界面与日志解释「为什么不信这条自述」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarrativeConflict {
    /// 自称完成，但宿主没有可用验证证据。
    ClaimsDoneWithoutHostEvidence,
    /// 自称完成，但宿主记录里有被拒/需返工的验证。
    ClaimsDoneAgainstFailedVerification,
    /// 叙述给出的证据引用不在宿主账本里。
    ClaimedEvidenceOutsideLedger { reference: String },
    /// 叙述试图改写的 Effect 状态与宿主不同；宿主状态未被修改。
    EffectStateOverride {
        effect_id: String,
        host: EffectStatus,
        claimed: EffectStatus,
    },
    /// 叙述提到宿主根本不存在的 Effect。
    EffectNotInHostLedger { effect_id: String },
}

impl fmt::Display for NarrativeConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClaimsDoneWithoutHostEvidence => {
                f.write_str("Agent 自称完成，但宿主没有可用的验证证据")
            }
            Self::ClaimsDoneAgainstFailedVerification => {
                f.write_str("Agent 自称完成，但宿主验证记录为拒绝或需返工")
            }
            Self::ClaimedEvidenceOutsideLedger { reference } => {
                write!(f, "叙述引用的证据 `{reference}` 不在宿主账本中")
            }
            Self::EffectStateOverride {
                effect_id,
                host,
                claimed,
            } => write!(
                f,
                "叙述把 Effect `{effect_id}` 声明为 {}，宿主记录是 {}；以宿主为准",
                claimed.as_str(),
                host.as_str()
            ),
            Self::EffectNotInHostLedger { effect_id } => {
                write!(f, "叙述提到宿主不存在的 Effect `{effect_id}`")
            }
        }
    }
}

/// 装配结果。宿主字段保持输入原值，叙述只出现在 `narrative`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffAssembly {
    pub header: HandoffPackageHeader,
    pub workspace: WorkspaceFacts,
    pub artifacts: Vec<ArtifactFact>,
    pub verifications: Vec<VerificationFact>,
    pub effects: Vec<EffectFact>,
    pub budget: BudgetFact,
    pub captured_at: String,
    pub missing: Vec<String>,
    pub narrative: Option<NarrativeInput>,
    pub narrative_status: NarrativeStatus,
    pub completion: CompletionStatus,
    pub conflicts: Vec<NarrativeConflict>,
}

/// 装配失败。只覆盖「宿主输入本身不合法」，不是叙述问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyError {
    /// `captured_at` 不是 `YYYY-MM-DDTHH:MM:SSZ`。
    NonCanonicalCapturedAt(String),
    /// 预算快照引用为空，等于没有预算事实。
    MissingBudgetSnapshot,
    /// 规范编码失败（理论上不会发生，防御保留）。
    Canonical(CanonicalError),
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonCanonicalCapturedAt(value) => {
                write!(
                    f,
                    "captured_at `{value}` 不是秒级 UTC（YYYY-MM-DDTHH:MM:SSZ）"
                )
            }
            Self::MissingBudgetSnapshot => f.write_str("预算快照引用为空，不能作为宿主事实"),
            Self::Canonical(err) => write!(f, "规范编码失败：{err}"),
        }
    }
}

impl std::error::Error for AssemblyError {}

impl From<CanonicalError> for AssemblyError {
    fn from(err: CanonicalError) -> Self {
        Self::Canonical(err)
    }
}

/// 装配交接骨架。宿主事实原样保留；叙述只影响 `narrative_status` 与 `conflicts`。
pub fn assemble(
    header: &HandoffPackageHeader,
    host: HostFactsInput,
    narrative: Option<NarrativeInput>,
) -> Result<HandoffAssembly, AssemblyError> {
    if !canonical_json::is_canonical_timestamp(&host.captured_at) {
        return Err(AssemblyError::NonCanonicalCapturedAt(host.captured_at));
    }
    if host.budget.snapshot_ref.trim().is_empty() {
        return Err(AssemblyError::MissingBudgetSnapshot);
    }

    let completion = derive_completion(header, &host);
    let (narrative_status, conflicts) = evaluate_narrative(&completion, &host, narrative.as_ref());

    Ok(HandoffAssembly {
        header: header.clone(),
        workspace: host.workspace,
        artifacts: host.artifacts,
        verifications: host.verifications,
        effects: host.effects,
        budget: host.budget,
        captured_at: host.captured_at,
        missing: host.missing,
        narrative,
        narrative_status,
        completion,
        conflicts,
    })
}

/// 完成度只读宿主验证记录，忽略叙述。
fn derive_completion(header: &HandoffPackageHeader, host: &HostFactsInput) -> CompletionStatus {
    let target = &header.work_package_revision;
    let artifact_registered = host
        .artifacts
        .iter()
        .any(|artifact| &artifact.revision == target);
    if !artifact_registered {
        return CompletionStatus::Incomplete {
            reason: IncompleteReason::NoRegisteredArtifact,
        };
    }

    let relevant: Vec<&VerificationFact> = host
        .verifications
        .iter()
        .filter(|record| &record.revision_id == target)
        .collect();
    if relevant.is_empty() {
        return CompletionStatus::Incomplete {
            reason: IncompleteReason::NoVerificationRecorded,
        };
    }
    if relevant.iter().any(|record| {
        matches!(
            record.state,
            VerificationState::Rejected | VerificationState::NeedsRevision
        )
    }) {
        return CompletionStatus::Incomplete {
            reason: IncompleteReason::VerificationRejected,
        };
    }

    let mut evidence: BTreeSet<String> = BTreeSet::new();
    let mut saw_verified = false;
    let mut saw_verified_with_evidence = false;
    for record in &relevant {
        if !record.state.is_verified() {
            continue;
        }
        saw_verified = true;
        if !record.evidence_refs.is_empty() {
            saw_verified_with_evidence = true;
        }
        if record.counts_as_host_evidence() {
            evidence.extend(record.evidence_refs.iter().cloned());
        }
    }

    if !evidence.is_empty() {
        return CompletionStatus::Completed {
            evidence_refs: evidence.into_iter().collect(),
        };
    }
    let reason = if saw_verified_with_evidence {
        // 有通过记录且有证据，但全部是自报。
        IncompleteReason::OnlySelfReportedEvidence
    } else if saw_verified {
        IncompleteReason::VerificationMissingEvidence
    } else {
        IncompleteReason::NoVerificationRecorded
    };
    CompletionStatus::Incomplete { reason }
}

/// 评估叙述：只产出状态与冲突，不改动宿主事实。
fn evaluate_narrative(
    completion: &CompletionStatus,
    host: &HostFactsInput,
    narrative: Option<&NarrativeInput>,
) -> (NarrativeStatus, Vec<NarrativeConflict>) {
    let Some(narrative) = narrative else {
        return (NarrativeStatus::NoClaim, Vec::new());
    };

    let mut conflicts = Vec::new();

    let ledger_refs: BTreeSet<&str> = host
        .verifications
        .iter()
        .flat_map(|record| record.evidence_refs.iter().map(String::as_str))
        .collect();
    for reference in &narrative.claimed_evidence_refs {
        if !ledger_refs.contains(reference.as_str()) {
            conflicts.push(NarrativeConflict::ClaimedEvidenceOutsideLedger {
                reference: reference.clone(),
            });
        }
    }

    for (effect_id, claimed) in &narrative.claimed_effect_states {
        match host
            .effects
            .iter()
            .find(|effect| &effect.effect_id == effect_id)
        {
            Some(effect) if effect.status != *claimed => {
                conflicts.push(NarrativeConflict::EffectStateOverride {
                    effect_id: effect_id.clone(),
                    host: effect.status,
                    claimed: *claimed,
                });
            }
            Some(_) => {}
            None => conflicts.push(NarrativeConflict::EffectNotInHostLedger {
                effect_id: effect_id.clone(),
            }),
        }
    }

    if !narrative.claimed_done {
        return (NarrativeStatus::NoClaim, conflicts);
    }

    match completion {
        CompletionStatus::Completed { .. } => (NarrativeStatus::Corroborated, conflicts),
        CompletionStatus::Incomplete {
            reason: IncompleteReason::VerificationRejected,
        } => {
            conflicts.push(NarrativeConflict::ClaimsDoneAgainstFailedVerification);
            (NarrativeStatus::ContradictsHost, conflicts)
        }
        CompletionStatus::Incomplete { .. } => {
            conflicts.push(NarrativeConflict::ClaimsDoneWithoutHostEvidence);
            (NarrativeStatus::Unverified, conflicts)
        }
    }
}

impl HandoffAssembly {
    /// 是否为部分快照。部分快照仍可装配与登记，自动接续由等级准入另行判定。
    pub fn is_partial(&self) -> bool {
        !self.missing.is_empty()
    }

    /// 供界面展示的一句话结论：完成度归宿主，叙述状态单独标注。
    pub fn summary_line(&self) -> String {
        let completion = match &self.completion {
            CompletionStatus::Completed { evidence_refs } => {
                format!("宿主已完成（证据 {} 条）", evidence_refs.len())
            }
            CompletionStatus::Incomplete { reason } => format!("宿主未完成：{reason}"),
        };
        format!(
            "{completion}；叙述状态 {}；冲突 {} 条",
            self.narrative_status.as_str(),
            self.conflicts.len()
        )
    }

    /// 生成 §18.10 的 `payload` 对象（不含 `integrity`）。
    ///
    /// 冲突列表是派生视图，不写入 payload，避免同一宿主事实因叙述冲突而产生不同摘要内容。
    pub fn payload_json(&self) -> serde_json::Value {
        let workspace = serde_json::json!({
            "base_commit": self.workspace.base_commit,
            "committed_ref": self.workspace.committed_ref,
            "uncommitted_patch_ref": self.workspace.uncommitted_patch_ref,
            "untracked_files_manifest_ref": self.workspace.untracked_files_manifest_ref,
            "environment_ref": self.workspace.environment_ref,
        });

        let artifact_refs: Vec<serde_json::Value> = self
            .artifacts
            .iter()
            .map(|artifact| {
                serde_json::json!({
                    "artifact_id": artifact.artifact_id.as_str(),
                    "revision": artifact.revision.as_str(),
                    "accepted": artifact.accepted,
                })
            })
            .collect();

        let verifications: Vec<serde_json::Value> = self
            .verifications
            .iter()
            .map(|record| {
                serde_json::json!({
                    "revision_id": record.revision_id.as_str(),
                    "state": record.state.as_str(),
                    "evidence_refs": record.evidence_refs,
                    "environment": record.environment.as_str(),
                    "verification_rules": record.verification_rules.as_str(),
                    "self_reported": record.is_self_reported(),
                })
            })
            .collect();

        let effects: Vec<serde_json::Value> = self
            .effects
            .iter()
            .map(|effect| {
                serde_json::json!({
                    "effect_id": effect.effect_id,
                    "logical_action_key": effect.logical_action_key,
                    "status": effect.status.as_str(),
                })
            })
            .collect();

        let narrative = self.narrative.as_ref().map(|narrative| {
            serde_json::json!({
                "author": narrative.author.as_str(),
                "summary_ref": narrative.summary_ref,
                "claimed_done": narrative.claimed_done,
                "claimed_evidence_refs": narrative.claimed_evidence_refs,
                "failure_reason_ref": narrative.failure_reason_ref,
                "suggestion_refs": narrative.suggestion_refs,
                "open_question_refs": narrative.open_question_refs,
                "claimed_effect_states": narrative
                    .claimed_effect_states
                    .iter()
                    .map(|(effect_id, status)| {
                        serde_json::json!({
                            "effect_id": effect_id,
                            "status": status.as_str(),
                        })
                    })
                    .collect::<Vec<_>>(),
            })
        });

        serde_json::json!({
            "schema": self.header.schema,
            "level": self.header.level.as_str(),
            "company_id": self.header.company_id.as_str(),
            "project_id": self.header.project_id.as_str(),
            "issue_id": self.header.issue_id.as_str(),
            "work_package_revision": self.header.work_package_revision.as_str(),
            "source_attempt_id": self.header.source_attempt_id.as_str(),
            "snapshot_seq": self.header.snapshot_seq,
            "captured_at": self.captured_at,
            "workspace": workspace,
            "host_facts": {
                "artifact_refs": artifact_refs,
                "verification_refs": verifications,
                "effect_refs": effects,
                "budget_snapshot_ref": self.budget.snapshot_ref,
                "budget": {
                    "limit": self.budget.limit,
                    "confirmed": self.budget.confirmed,
                    "reserved": self.budget.reserved,
                    "risk": self.budget.risk,
                },
                "completion": match &self.completion {
                    CompletionStatus::Completed { evidence_refs } => serde_json::json!({
                        "state": "completed",
                        "evidence_refs": evidence_refs,
                    }),
                    CompletionStatus::Incomplete { reason } => serde_json::json!({
                        "state": "incomplete",
                        "reason": reason.as_str(),
                    }),
                },
                "narrative_status": self.narrative_status.as_str(),
            },
            "narrative": narrative,
            "missing": self.missing,
        })
    }

    /// 规范编码后的 payload（`bod-canonical-json-v1`）。
    pub fn payload_canonical(&self) -> Result<CanonicalValue, AssemblyError> {
        let text = serde_json::to_string(&self.payload_json()).map_err(|err| {
            AssemblyError::Canonical(CanonicalError::InvalidJson(err.to_string()))
        })?;
        Ok(CanonicalValue::parse(&text)?)
    }

    /// payload 摘要（`sha256:<hex>`）。同一宿主事实与叙述必得同一摘要。
    pub fn payload_digest(&self) -> Result<Digest, AssemblyError> {
        Ok(self.payload_canonical()?.digest())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff_package::HandoffLevel;

    fn header() -> HandoffPackageHeader {
        HandoffPackageHeader {
            schema: "bod.handoff/v1".to_string(),
            level: HandoffLevel::L2,
            company_id: DomainId::new("company-1"),
            project_id: DomainId::new("project-1"),
            issue_id: DomainId::new("issue-1"),
            work_package_revision: DomainId::new("wp-r1"),
            source_attempt_id: DomainId::new("attempt-1"),
            snapshot_seq: 7,
        }
    }

    fn workspace() -> WorkspaceFacts {
        WorkspaceFacts {
            base_commit: "base-sha".to_string(),
            committed_ref: Some("commit-r1".to_string()),
            uncommitted_patch_ref: None,
            untracked_files_manifest_ref: None,
            environment_ref: Some("env-r1".to_string()),
        }
    }

    fn artifact() -> ArtifactFact {
        ArtifactFact {
            artifact_id: DomainId::new("artifact-1"),
            revision: DomainId::new("wp-r1"),
            accepted: false,
        }
    }

    fn verification(state: VerificationState, actor: &str, evidence: &[&str]) -> VerificationFact {
        VerificationFact {
            revision_id: DomainId::new("wp-r1"),
            state,
            evidence_refs: evidence.iter().map(|value| value.to_string()).collect(),
            environment: DomainId::new("env-r1"),
            verification_rules: DomainId::new("rules-r1"),
            producer: DomainId::new("agent-a"),
            actor: DomainId::new(actor),
        }
    }

    fn effect(status: EffectStatus) -> EffectFact {
        EffectFact {
            effect_id: "effect-7".to_string(),
            logical_action_key: "deploy:service".to_string(),
            status,
        }
    }

    fn budget() -> BudgetFact {
        BudgetFact {
            snapshot_ref: "budget-7".to_string(),
            limit: 1000,
            confirmed: 200,
            reserved: 100,
            risk: 50,
        }
    }

    fn host(verifications: Vec<VerificationFact>, effects: Vec<EffectFact>) -> HostFactsInput {
        HostFactsInput {
            workspace: workspace(),
            artifacts: vec![artifact()],
            verifications,
            effects,
            budget: budget(),
            missing: Vec::new(),
            captured_at: "2026-11-20T10:00:00Z".to_string(),
        }
    }

    fn done_narrative() -> NarrativeInput {
        NarrativeInput {
            claimed_done: true,
            ..NarrativeInput::new("agent-a")
        }
    }

    #[test]
    fn invariant_i_46_narrative_cannot_override_effect_state() {
        let mut narrative = NarrativeInput::new("agent-a");
        narrative
            .claimed_effect_states
            .push(("effect-7".to_string(), EffectStatus::Confirmed));

        let assembly = assemble(
            &header(),
            host(
                vec![verification(
                    VerificationState::Verified,
                    "reviewer",
                    &["cas:abc"],
                )],
                vec![effect(EffectStatus::Unknown)],
            ),
            Some(narrative),
        )
        .unwrap();

        assert_eq!(
            assembly.effects[0].status,
            EffectStatus::Unknown,
            "宿主 Effect 状态不能被叙述改写"
        );
        assert!(assembly.conflicts.iter().any(|conflict| matches!(
            conflict,
            NarrativeConflict::EffectStateOverride {
                claimed: EffectStatus::Confirmed,
                host: EffectStatus::Unknown,
                ..
            }
        )));
    }

    #[test]
    fn invariant_i_46_done_without_evidence_stays_unverified() {
        let assembly =
            assemble(&header(), host(Vec::new(), vec![]), Some(done_narrative())).unwrap();

        assert_eq!(assembly.narrative_status, NarrativeStatus::Unverified);
        assert_eq!(
            assembly.completion,
            CompletionStatus::Incomplete {
                reason: IncompleteReason::NoVerificationRecorded
            }
        );
        assert!(
            !assembly.completion.is_completed(),
            "自称 done 不改变宿主完成度"
        );
        assert!(
            assembly
                .conflicts
                .contains(&NarrativeConflict::ClaimsDoneWithoutHostEvidence)
        );
        assert!(assembly.summary_line().contains("宿主未完成"));
    }

    #[test]
    fn invariant_i_46_claimed_evidence_outside_ledger_is_not_promoted() {
        let mut narrative = done_narrative();
        narrative
            .claimed_evidence_refs
            .push("sha256:invented".to_string());

        let assembly = assemble(&header(), host(Vec::new(), vec![]), Some(narrative)).unwrap();

        assert!(
            assembly
                .conflicts
                .contains(&NarrativeConflict::ClaimedEvidenceOutsideLedger {
                    reference: "sha256:invented".to_string()
                }),
            "自报证据必须显式标为不在账本内"
        );
        assert!(!assembly.completion.is_completed());
    }

    #[test]
    fn invariant_i_46_host_verification_completes_and_narrative_is_corroborated() {
        let assembly = assemble(
            &header(),
            host(
                vec![verification(
                    VerificationState::Verified,
                    "reviewer",
                    &["cas:abc", "sha256:def"],
                )],
                vec![effect(EffectStatus::Confirmed)],
            ),
            Some(done_narrative()),
        )
        .unwrap();

        assert_eq!(
            assembly.completion,
            CompletionStatus::Completed {
                evidence_refs: vec!["cas:abc".to_string(), "sha256:def".to_string()]
            }
        );
        assert_eq!(assembly.narrative_status, NarrativeStatus::Corroborated);
        assert!(assembly.conflicts.is_empty());
        assert!(assembly.summary_line().contains("宿主已完成"));
    }

    #[test]
    fn invariant_i_46_failed_verification_is_not_overwritten_by_narrative() {
        let assembly = assemble(
            &header(),
            host(
                vec![verification(
                    VerificationState::Rejected,
                    "reviewer",
                    &["cas:bad"],
                )],
                vec![],
            ),
            Some(done_narrative()),
        )
        .unwrap();

        assert_eq!(assembly.narrative_status, NarrativeStatus::ContradictsHost);
        assert_eq!(
            assembly.completion,
            CompletionStatus::Incomplete {
                reason: IncompleteReason::VerificationRejected
            }
        );
        assert!(
            assembly
                .conflicts
                .contains(&NarrativeConflict::ClaimsDoneAgainstFailedVerification)
        );
    }

    #[test]
    fn invariant_i_46_self_reported_verification_does_not_count() {
        let assembly = assemble(
            &header(),
            host(
                vec![verification(
                    VerificationState::Verified,
                    "agent-a",
                    &["cas:self"],
                )],
                vec![],
            ),
            Some(done_narrative()),
        )
        .unwrap();

        assert_eq!(
            assembly.completion,
            CompletionStatus::Incomplete {
                reason: IncompleteReason::OnlySelfReportedEvidence
            }
        );
        assert_eq!(assembly.narrative_status, NarrativeStatus::Unverified);
    }

    #[test]
    fn invariant_i_46_verified_without_evidence_is_incomplete() {
        let assembly = assemble(
            &header(),
            host(
                vec![verification(VerificationState::Verified, "reviewer", &[])],
                vec![],
            ),
            None,
        )
        .unwrap();
        assert_eq!(
            assembly.completion,
            CompletionStatus::Incomplete {
                reason: IncompleteReason::VerificationMissingEvidence
            }
        );
        assert_eq!(assembly.narrative_status, NarrativeStatus::NoClaim);
    }

    #[test]
    fn invariant_i_46_payload_digest_covers_host_and_narrative() {
        let base = host(
            vec![verification(
                VerificationState::Verified,
                "reviewer",
                &["cas:abc"],
            )],
            vec![effect(EffectStatus::Unknown)],
        );
        let first = assemble(&header(), base.clone(), Some(done_narrative())).unwrap();
        let again = assemble(&header(), base.clone(), Some(done_narrative())).unwrap();
        assert_eq!(
            first.payload_digest().unwrap(),
            again.payload_digest().unwrap(),
            "同一输入必得同一摘要"
        );

        let mut different = done_narrative();
        different.summary_ref = Some("artifact-summary-1".to_string());
        let changed = assemble(&header(), base, Some(different)).unwrap();
        assert_ne!(
            first.payload_digest().unwrap(),
            changed.payload_digest().unwrap(),
            "叙述变化必须反映在 payload 摘要中"
        );

        let canonical = first.payload_canonical().unwrap();
        assert!(canonical.member("host_facts").is_some());
        assert!(canonical.member("narrative").is_some());
    }

    #[test]
    fn partial_snapshot_still_assembles() {
        let mut facts = host(Vec::new(), Vec::new());
        facts.missing = vec!["external-session-state".to_string()];
        let assembly = assemble(&header(), facts, None).unwrap();

        assert!(assembly.is_partial());
        assert!(assembly.payload_digest().is_ok(), "部分快照仍可被登记");
        assert_eq!(assembly.narrative_status, NarrativeStatus::NoClaim);
    }

    #[test]
    fn rejects_non_canonical_captured_at_and_missing_budget_ref() {
        let mut bad_time = host(Vec::new(), Vec::new());
        bad_time.captured_at = "2026-11-20T10:00:00+08:00".to_string();
        assert!(matches!(
            assemble(&header(), bad_time, None).unwrap_err(),
            AssemblyError::NonCanonicalCapturedAt(_)
        ));

        let mut bad_budget = host(Vec::new(), Vec::new());
        bad_budget.budget.snapshot_ref = "  ".to_string();
        assert_eq!(
            assemble(&header(), bad_budget, None).unwrap_err(),
            AssemblyError::MissingBudgetSnapshot
        );
    }
}
