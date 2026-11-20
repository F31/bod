//! 接续可见性卡片（P2 任务 2.5，`specs/takeover-transaction`「接续可见性」）。
//!
//! 规格要求：界面分项显示**可接续等级、已保留成果、仍缺内容、旧执行是否停止、预计重建步骤**；
//! 换人 MUST NOT 自动改变验收、扩大权限或合并旧未验收成果。
//!
//! 本模块是**只读投影**，不持 `SqliteStore` 句柄、不写库、不产生命令：
//! - 输入全部来自已持久化事实（1.4 快照、1.5 清单、2.1 意图、2.2 隔离判定、2.4 预检报告）
//!   与宿主传入的接受记录；
//! - 「已保留成果」只有一个构造入口 [`PreservedArtifact::from_record`]，只认宿主
//!   `AcceptanceRecord` 且必须仍批准当前工作包修订——候选成果与它**不是同一个类型**，
//!   因此「把旧候选标为已接受」在代码里没有可表达的路径；
//! - 权限原样回显（[`AuthorityView`]），等级不派生任何 scope；
//! - 停止状态照抄 2.2 的四态，`ack_only` 不会被渲染成「已停止」。
//!
//! 边界：不判断能否接续（那是 2.4 的结论）、不接受成果（P1 验收路径）、不渲染接续卡片的
//! 视觉样式（宿主 UI 负责），只给出可核对的分项数据与文本行。

use crate::artifact_acceptance::Candidate;
use crate::domain::artifact::{AcceptanceDecision, AcceptanceRecord};
use crate::handoff_package::{AdmissionDecision, HandoffLevel};
use crate::handoff_snapshot::{HandoffSnapshot, IntegrityStatus, MissingReason};
use crate::isolation_check::{IsolationCheck, StopStatus, WriteTarget};
use crate::rebuild_precheck::{PrecheckReport, REBUILD_STEPS, RebuildStep};
use crate::takeover_intent::{AttemptCreation, TakeoverPhase};
use std::fmt;

/// 卡片 schema：界面契约单独版本化，与交接包/意图 schema 不共用。
pub const CARD_SCHEMA: &str = "bod.continuation-card/v1";

/// 卡片构造失败。只覆盖「输入自相矛盾」，不覆盖「能不能接续」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardError {
    EmptyField(&'static str),
    /// 预检报告属于另一次尝试：串卡会把别人的结论贴到这张卡上。
    AttemptMismatch {
        expected: String,
        actual: String,
    },
}

impl fmt::Display for CardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "接续卡片缺少字段：{field}"),
            Self::AttemptMismatch { expected, actual } => {
                write!(f, "预检报告属于另一次尝试：期望 {expected}，实际 {actual}")
            }
        }
    }
}

impl std::error::Error for CardError {}

/// 观察者身份与现有权限。卡片只回显，任何等级都不在此追加 scope。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    pub subject_id: String,
    /// 当前授权 epoch；与 3.x 的委任复核共用同一事实源。
    pub epoch: u64,
    /// 现有 scope，原样回显。
    pub scopes: Vec<String>,
}

impl Viewer {
    pub fn new(subject_id: impl Into<String>, epoch: u64, scopes: Vec<String>) -> Self {
        Self {
            subject_id: subject_id.into(),
            epoch,
            scopes,
        }
    }
}

/// 卡片输入。借用而非持有：调用方从宿主读到的只读事实。
pub struct CardSpec<'a> {
    /// 被接管的旧执行（卡片标题里的 attempt）。
    pub from_attempt_id: String,
    /// 已登记的新执行；未到 `attempt_created` 时为 `None`。
    pub new_attempt: Option<&'a AttemptCreation>,
    /// 旧执行的基线 fencing token，用于核对新 token 是否更高。
    pub baseline_fencing_token: Option<u64>,
    pub phase: TakeoverPhase,
    /// 1.1 的准入结论：显示的是**获批**等级，不是请求等级。
    pub level: &'a AdmissionDecision,
    pub snapshot: Option<&'a HandoffSnapshot>,
    /// 2.2 的隔离判定；未检查时为 `None`（卡片写「尚未检查」，不猜）。
    pub isolation: Option<&'a IsolationCheck>,
    /// 若给出写目标，卡片附带 2.2 的写判定与隔离条件。
    pub write_target: Option<&'a WriteTarget>,
    /// 宿主接受记录——「已保留成果」的唯一来源。
    pub acceptance_records: &'a [AcceptanceRecord],
    /// 候选成果：进「候选」分区，永远不计入保留成果。
    pub candidates: &'a [Candidate],
    /// 2.4 的预检报告；未跑预检时为 `None`（8 步全未完成）。
    pub precheck: Option<&'a PrecheckReport>,
    /// 当前工作包修订：接受记录必须绑定它才算保留。
    pub work_package_revision: &'a str,
    pub viewer: &'a Viewer,
}

/// 等级视图：获批等级 + 是否降级 + 缺哪些维度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelView {
    pub admitted: HandoffLevel,
    pub requested: HandoffLevel,
    pub downgraded: bool,
    pub missing_evidence: Vec<&'static str>,
}

impl LevelView {
    pub fn from_decision(decision: &AdmissionDecision) -> Self {
        match decision {
            AdmissionDecision::Admitted { level } => Self {
                admitted: *level,
                requested: *level,
                downgraded: false,
                missing_evidence: Vec::new(),
            },
            AdmissionDecision::Downgraded { from, to, missing } => Self {
                admitted: *to,
                requested: *from,
                downgraded: true,
                missing_evidence: missing.iter().map(|item| item.as_str()).collect(),
            },
        }
    }

    pub fn line(&self) -> String {
        if self.downgraded {
            format!(
                "可接续等级 {}（请求 {} 已降级；缺 {}）",
                self.admitted.as_str(),
                self.requested.as_str(),
                join_or_none(&self.missing_evidence)
            )
        } else {
            format!("可接续等级 {}", self.admitted.as_str())
        }
    }
}

/// 已保留成果：字段私有，只能由宿主接受记录构造。
///
/// 这是「不把旧候选标为已接受」的类型级保证——候选走 [`CandidateArtifact`]，两者无法互换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedArtifact {
    artifact_id: String,
    revision_id: String,
    acceptance_id: String,
    decision: &'static str,
    actor: String,
    accepted_at_ms: i64,
    evidence_refs: Vec<String>,
}

impl PreservedArtifact {
    /// 唯一构造入口：必须是仍批准当前修订的宿主接受记录。
    ///
    /// 返回 `None` 的情形（未批准、被取代、要求人工批准）由调用方放进
    /// [`StaleAcceptance`]，而不是悄悄丢弃或折算成保留。
    pub fn from_record(record: &AcceptanceRecord, work_package_revision: &str) -> Option<Self> {
        if record.decision == AcceptanceDecision::RequiresHuman {
            return None;
        }
        if record.superseded_by.is_some() {
            return None;
        }
        // 注意：不能用 `record.approves(rev)`——它比的是 **artifact revision**，
        // 拿工作包修订去比会永远为假（首版实测 preserved 恒为空）。
        // 这里要回答的是「这条接受是否针对当前工作包修订」，所以比绑定里的工作包字段。
        if record.binding.work_package_revision.as_str() != work_package_revision {
            return None;
        }
        Some(Self {
            artifact_id: record.artifact_id.as_str().to_string(),
            revision_id: record.binding.artifact_revision.as_str().to_string(),
            acceptance_id: record.acceptance_id.as_str().to_string(),
            decision: record.decision.as_str(),
            actor: record.actor.as_str().to_string(),
            accepted_at_ms: record.accepted_at_ms,
            evidence_refs: record.evidence_refs.clone(),
        })
    }

    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    pub fn revision_id(&self) -> &str {
        &self.revision_id
    }

    pub fn acceptance_id(&self) -> &str {
        &self.acceptance_id
    }

    pub fn decision(&self) -> &str {
        self.decision
    }

    /// 验收人：换人后仍然是原验收人，卡片不重写这一栏。
    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub fn accepted_at_ms(&self) -> i64 {
        self.accepted_at_ms
    }

    pub fn evidence_refs(&self) -> &[String] {
        &self.evidence_refs
    }

    pub fn line(&self) -> String {
        format!(
            "{}@{}（{}，验收人 {}，证据 {} 条）",
            self.artifact_id,
            self.revision_id,
            self.decision,
            self.actor,
            self.evidence_refs.len()
        )
    }
}

/// 存在但**不批准当前修订**的接受记录。
///
/// 必须单独列出：既不能算已保留（会合并旧成果），也不能消失（会让人以为从没验收过）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleAcceptance {
    pub acceptance_id: String,
    pub artifact_id: String,
    pub reason: &'static str,
}

/// 候选成果：永远不是「已保留」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateArtifact {
    artifact_id: String,
    revision_id: String,
    verification_state: &'static str,
    /// 候选自带的 `accepted` 标记，只作原始声明保留，不参与「已接受」判定。
    accepted_claim: bool,
}

impl CandidateArtifact {
    pub fn from_candidate(candidate: &Candidate) -> Self {
        Self {
            artifact_id: candidate.artifact_id.as_str().to_string(),
            revision_id: candidate.revision_id.as_str().to_string(),
            verification_state: candidate.verification_state.as_str(),
            accepted_claim: candidate.accepted,
        }
    }

    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    pub fn revision_id(&self) -> &str {
        &self.revision_id
    }

    pub fn verification_state(&self) -> &str {
        self.verification_state
    }

    pub fn accepted_claim(&self) -> bool {
        self.accepted_claim
    }

    pub fn line(&self) -> String {
        if self.accepted_claim {
            format!(
                "{}@{}（{}；自称已接受，宿主无接受记录——仍未接受）",
                self.artifact_id, self.revision_id, self.verification_state
            )
        } else {
            format!(
                "{}@{}（{}；候选，未接受）",
                self.artifact_id, self.revision_id, self.verification_state
            )
        }
    }
}

/// 仍缺内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingItem {
    pub part: String,
    pub reason: &'static str,
    pub detail: String,
}

impl MissingItem {
    pub fn line(&self) -> String {
        if self.detail.is_empty() {
            format!("{}（{}）", self.part, self.reason)
        } else {
            format!("{}（{}：{}）", self.part, self.reason, self.detail)
        }
    }
}

/// 停止与写权限视图。`checked = false` 表示 2.2 尚未检查，不是「未知」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopView {
    pub checked: bool,
    /// confirmed / ack_only / no_ack / unknown / not_checked。
    pub status: &'static str,
    pub confirmed: bool,
    pub evidence_ref: Option<String>,
    pub ack_ref: Option<String>,
    /// `unknown` 时的处理人与下一步：不许留空，否则「不明」变成没人负责。
    pub assignee: Option<String>,
    pub next_step: Option<String>,
    /// 有写目标时的 2.2 判定；未给写目标为 `None`（不是「允许」）。
    pub writable: Option<bool>,
    pub write_error: Option<String>,
    /// 隔离条件（未清除时）：`code: detail`。
    pub blocking_conditions: Vec<String>,
}

impl StopView {
    fn not_checked() -> Self {
        Self {
            checked: false,
            status: "not_checked",
            confirmed: false,
            evidence_ref: None,
            ack_ref: None,
            assignee: None,
            next_step: None,
            writable: None,
            write_error: None,
            blocking_conditions: Vec::new(),
        }
    }

    fn from_check(check: &IsolationCheck, target: Option<&WriteTarget>) -> Self {
        let (status, evidence_ref, ack_ref, assignee, next_step) = match &check.stop {
            StopStatus::Confirmed { evidence_ref, .. } => {
                ("confirmed", Some(evidence_ref.clone()), None, None, None)
            }
            StopStatus::AckOnly { ack_ref } => {
                ("ack_only", None, Some(ack_ref.clone()), None, None)
            }
            StopStatus::NoAck => ("no_ack", None, None, None, None),
            StopStatus::Unknown {
                assignee,
                next_step,
            } => (
                "unknown",
                None,
                None,
                Some(assignee.clone()),
                Some(next_step.clone()),
            ),
        };

        let mut conditions = Vec::new();
        if let Ok(crate::isolation_check::IsolationClearance::NotCleared { conditions: list }) =
            check.clearance()
        {
            conditions = list
                .iter()
                .map(|item| format!("{}: {}", item.code, item.detail))
                .collect();
        }

        let (writable, write_error) = match target {
            None => (None, None),
            Some(target) => match check.decide_write(target) {
                Ok(decision) => (Some(decision.allowed()), None),
                Err(err) => (None, Some(err.to_string())),
            },
        };

        Self {
            checked: true,
            status,
            confirmed: check.stop.confirmed(),
            evidence_ref,
            ack_ref,
            assignee,
            next_step,
            writable,
            write_error,
            blocking_conditions: conditions,
        }
    }

    /// 停止状态说明。只有 `Confirmed` 才会出现「已确认停止」，其余三态如实说没证实。
    pub fn line(&self) -> String {
        if !self.checked {
            return "停止状态：尚未检查旧执行是否停止".to_string();
        }
        let base = match self.status {
            "confirmed" => format!(
                "停止状态：已确认停止（证据 {}）",
                self.evidence_ref.as_deref().unwrap_or("<缺失>")
            ),
            "ack_only" => format!(
                "停止状态：只收到取消回执（{}），未证实停止——不得据此写旧现场",
                self.ack_ref.as_deref().unwrap_or("<缺失>")
            ),
            "no_ack" => "停止状态：未收到取消回执，旧执行可能仍在写".to_string(),
            "unknown" => format!(
                "停止状态：不明（处理人 {}，下一步 {}）",
                self.assignee.as_deref().unwrap_or("<缺失>"),
                self.next_step.as_deref().unwrap_or("<缺失>")
            ),
            other => format!("停止状态：{other}"),
        };
        let write = match self.writable {
            Some(true) => "；写判定：允许".to_string(),
            Some(false) => {
                let reasons = if self.blocking_conditions.is_empty() {
                    String::new()
                } else {
                    format!("（{}）", self.blocking_conditions.join("；"))
                };
                format!("；写判定：拒绝{reasons}")
            }
            None => match &self.write_error {
                Some(err) => format!("；写判定：无法判定（{err}）"),
                None => String::new(),
            },
        };
        format!("{base}{write}")
    }
}

/// 重建步骤：代码与标签来自 2.4 的固定表，完成标记来自预检报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepView {
    pub code: &'static str,
    pub label: &'static str,
    pub done: bool,
}

impl StepView {
    pub fn line(&self) -> String {
        format!(
            "{} {} {}",
            if self.done { "[x]" } else { "[ ]" },
            self.label,
            self.code
        )
    }
}

/// 阻塞项（来自 2.4 预检），只给可核对的三元组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerView {
    pub code: &'static str,
    pub subject: String,
    pub detail: String,
}

/// 观察者权限：原样回显，卡片不授予、不扩大。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityView {
    pub subject_id: String,
    pub epoch: u64,
    pub scopes: Vec<String>,
}

impl AuthorityView {
    fn echo(viewer: &Viewer) -> Self {
        Self {
            subject_id: viewer.subject_id.clone(),
            epoch: viewer.epoch,
            scopes: viewer.scopes.clone(),
        }
    }

    pub fn line(&self) -> String {
        format!(
            "观察者 {}（epoch {}，scope {}；本卡片不授予额外权限）",
            self.subject_id,
            self.epoch,
            join_or_none(&self.scopes)
        )
    }
}

/// 已登记的新执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttempt {
    pub attempt_id: String,
    pub fencing_token: u64,
}

/// 接续可见性卡片。只读：所有字段由构造时的输入派生，构建后不再变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationCard {
    pub schema: String,
    pub from_attempt_id: String,
    pub new_attempt: Option<NewAttempt>,
    pub phase: TakeoverPhase,
    pub level: LevelView,
    pub preserved: Vec<PreservedArtifact>,
    pub candidates: Vec<CandidateArtifact>,
    pub stale_acceptances: Vec<StaleAcceptance>,
    pub missing: Vec<MissingItem>,
    /// 拿到了内容但没核验的片段：不算缺失，也不算完整。
    pub unverified_segments: Vec<String>,
    pub stop: StopView,
    pub steps: Vec<StepView>,
    pub blockers: Vec<BlockerView>,
    pub open_questions: usize,
    pub authority: AuthorityView,
    /// 输入之间互相矛盾之处（候选自称已接受、token 未高于基线等），照实列出。
    pub discrepancies: Vec<String>,
}

impl ContinuationCard {
    pub fn build(spec: CardSpec<'_>) -> Result<Self, CardError> {
        if spec.from_attempt_id.trim().is_empty() {
            return Err(CardError::EmptyField("from_attempt_id"));
        }
        if spec.work_package_revision.trim().is_empty() {
            return Err(CardError::EmptyField("work_package_revision"));
        }
        if spec.viewer.subject_id.trim().is_empty() {
            return Err(CardError::EmptyField("viewer.subject_id"));
        }
        if let Some(report) = spec.precheck {
            let expected = match spec.new_attempt {
                Some(attempt) => attempt.attempt_id.as_str(),
                None => spec.from_attempt_id.as_str(),
            };
            if report.attempt_id != expected {
                return Err(CardError::AttemptMismatch {
                    expected: expected.to_string(),
                    actual: report.attempt_id.clone(),
                });
            }
        }

        let mut preserved = Vec::new();
        let mut stale = Vec::new();
        for record in spec.acceptance_records {
            match PreservedArtifact::from_record(record, spec.work_package_revision) {
                Some(item) => preserved.push(item),
                None => stale.push(stale_reason(record, spec.work_package_revision)),
            }
        }

        let mut discrepancies = Vec::new();
        let candidates: Vec<CandidateArtifact> = spec
            .candidates
            .iter()
            .map(CandidateArtifact::from_candidate)
            .collect();
        for candidate in &candidates {
            if candidate.accepted_claim()
                && !preserved
                    .iter()
                    .any(|item| item.artifact_id() == candidate.artifact_id())
            {
                discrepancies.push(format!(
                    "{}@{} 自称已接受，但宿主没有对应接受记录；卡片保持未接受",
                    candidate.artifact_id(),
                    candidate.revision_id()
                ));
            }
        }

        if let (Some(baseline), Some(attempt)) = (spec.baseline_fencing_token, spec.new_attempt)
            && attempt.fencing_token <= baseline
        {
            discrepancies.push(format!(
                "新 fencing token {} 未高于基线 {}；按规格应更高",
                attempt.fencing_token, baseline
            ));
        }

        let (missing, unverified_segments) = missing_view(spec.snapshot);

        let steps = match spec.precheck {
            Some(report) => report.steps.iter().map(step_view).collect(),
            None => REBUILD_STEPS
                .iter()
                .map(|(code, label)| StepView {
                    code,
                    label,
                    done: false,
                })
                .collect(),
        };

        let (blockers, open_questions) = match spec.precheck {
            Some(report) => (
                report
                    .blockers
                    .iter()
                    .map(|blocker| BlockerView {
                        code: blocker.code,
                        subject: blocker.subject.clone(),
                        detail: blocker.detail.clone(),
                    })
                    .collect(),
                report.questions.len(),
            ),
            None => (Vec::new(), 0),
        };

        Ok(Self {
            schema: CARD_SCHEMA.to_string(),
            from_attempt_id: spec.from_attempt_id,
            new_attempt: spec.new_attempt.map(|attempt| NewAttempt {
                attempt_id: attempt.attempt_id.clone(),
                fencing_token: attempt.fencing_token,
            }),
            phase: spec.phase,
            level: LevelView::from_decision(spec.level),
            preserved,
            candidates,
            stale_acceptances: stale,
            missing,
            unverified_segments,
            stop: match spec.isolation {
                Some(check) => StopView::from_check(check, spec.write_target),
                None => StopView::not_checked(),
            },
            steps,
            blockers,
            open_questions,
            authority: AuthorityView::echo(spec.viewer),
            discrepancies,
        })
    }

    pub fn preserved_count(&self) -> usize {
        self.preserved.len()
    }

    /// 候选数量单独给出，永不加进「保留成果」。
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    pub fn missing_count(&self) -> usize {
        self.missing.len()
    }

    pub fn steps_done(&self) -> usize {
        self.steps.iter().filter(|step| step.done).count()
    }

    /// 分项渲染：一行一个分区，供界面直接排布或测试断言。
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "接续卡片 {}（阶段 {}，新执行 {}）",
            self.from_attempt_id,
            self.phase.as_str(),
            match &self.new_attempt {
                Some(attempt) =>
                    format!("{} / fencing {}", attempt.attempt_id, attempt.fencing_token),
                None => "尚未登记".to_string(),
            }
        )];
        lines.push(self.level.line());

        if self.preserved.is_empty() {
            lines.push("已保留成果：0 项".to_string());
        } else {
            lines.push(format!("已保留成果 {} 项", self.preserved.len()));
            for item in &self.preserved {
                lines.push(format!("  - {}", item.line()));
            }
        }

        if !self.candidates.is_empty() {
            lines.push(format!(
                "候选成果 {} 项（未接受，不计入保留成果）",
                self.candidates.len()
            ));
            for item in &self.candidates {
                lines.push(format!("  - {}", item.line()));
            }
        }

        if !self.stale_acceptances.is_empty() {
            lines.push(format!(
                "不批准当前修订的历史接受 {} 项",
                self.stale_acceptances.len()
            ));
            for item in &self.stale_acceptances {
                lines.push(format!(
                    "  - {}@{}（{}）",
                    item.artifact_id, item.acceptance_id, item.reason
                ));
            }
        }

        if self.missing.is_empty() {
            lines.push("仍缺内容：0 项".to_string());
        } else {
            lines.push(format!("仍缺内容 {} 项", self.missing.len()));
            for item in &self.missing {
                lines.push(format!("  - {}", item.line()));
            }
        }
        for segment in &self.unverified_segments {
            lines.push(format!("  - {}（内容已取到但未核验，不算完整）", segment));
        }

        lines.push(self.stop.line());
        lines.push(format!(
            "重建步骤 {}/{}",
            self.steps_done(),
            self.steps.len()
        ));
        for step in &self.steps {
            lines.push(format!("  - {}", step.line()));
        }

        if !self.blockers.is_empty() {
            lines.push(format!("阻塞 {} 项", self.blockers.len()));
            for blocker in &self.blockers {
                lines.push(format!(
                    "  - {} {}：{}",
                    blocker.code, blocker.subject, blocker.detail
                ));
            }
        }
        if self.open_questions > 0 {
            lines.push(format!(
                "待答问题 {} 项（须答复后才能继续）",
                self.open_questions
            ));
        }
        lines.push(self.authority.line());
        for note in &self.discrepancies {
            lines.push(format!("不一致：{note}"));
        }
        lines
    }

    /// 一行摘要：分列「已接受 / 候选（未计入）/ 缺失 / 停止」。
    pub fn summary_line(&self) -> String {
        let stop = match self.stop.status {
            _ if !self.stop.checked => "停止未检查",
            "confirmed" => "停止已确认",
            "ack_only" => "停止未证实",
            "no_ack" => "无停止回执",
            _ => "停止不明",
        };
        format!(
            "接续 {} → {}，已接受 {} 项、候选 {} 项（未计入）、缺 {} 项、{}、步骤 {}/{}",
            self.from_attempt_id,
            self.level.admitted.as_str(),
            self.preserved.len(),
            self.candidates.len(),
            self.missing.len(),
            stop,
            self.steps_done(),
            self.steps.len()
        )
    }
}

fn stale_reason(record: &AcceptanceRecord, revision: &str) -> StaleAcceptance {
    let reason = if record.decision == AcceptanceDecision::RequiresHuman {
        "requires_human"
    } else if record.superseded_by.is_some() {
        "superseded"
    } else if record.binding.work_package_revision.as_str() != revision {
        "work_package_mismatch"
    } else {
        "not_approved"
    };
    StaleAcceptance {
        acceptance_id: record.acceptance_id.as_str().to_string(),
        artifact_id: record.artifact_id.as_str().to_string(),
        reason,
    }
}

fn missing_view(snapshot: Option<&HandoffSnapshot>) -> (Vec<MissingItem>, Vec<String>) {
    let Some(snapshot) = snapshot else {
        // 没捕获现场：这是「缺一次捕获」，不是「什么都不缺」。
        return (
            vec![MissingItem {
                part: "handoff_snapshot".to_string(),
                reason: MissingReason::NotCaptured.as_str(),
                detail: "尚未捕获现场".to_string(),
            }],
            Vec::new(),
        );
    };
    let missing = snapshot
        .missing
        .iter()
        .map(|part| MissingItem {
            part: part.part.clone(),
            reason: part.reason.as_str(),
            detail: String::new(),
        })
        .collect();
    let unverified = snapshot
        .integrity
        .iter()
        .filter(|segment| segment.status == IntegrityStatus::Unverified)
        .map(|segment| segment.segment.clone())
        .collect();
    (missing, unverified)
}

fn step_view(step: &RebuildStep) -> StepView {
    StepView {
        code: step.code,
        label: step.label,
        done: step.done,
    }
}

fn join_or_none(items: &[impl AsRef<str>]) -> String {
    if items.is_empty() {
        "<无>".to_string()
    } else {
        items
            .iter()
            .map(|item| item.as_ref().to_string())
            .collect::<Vec<_>>()
            .join("、")
    }
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DomainId;
    use crate::domain::artifact::{
        AcceptanceBinding, AcceptanceDecision, AcceptanceRecord, Digest, VerificationState,
    };
    use crate::handoff_package::{
        AdmissionDecision, HANDOFF_SCHEMA, HandoffLevel, HandoffPackageHeader, MissingEvidence,
    };
    use crate::handoff_snapshot::{
        CaptureSource, MissingPart, MissingReason, SegmentIntegrity, SnapshotBinding,
    };
    use crate::isolation_check::{IsolationBasis, StopStatus, SurfaceKnowledge, WriteTarget};
    use crate::rebuild_precheck::{
        Authorization, GoalAssessment, ObservedFile, ObservedWorkspace, PrecheckReport,
        RebuildPrecheck,
    };
    use crate::takeover_intent::{AttemptCreation, TakeoverPhase};

    const WP: &str = "wp-r3";

    struct Fixture {
        level: AdmissionDecision,
        snapshot: HandoffSnapshot,
        isolation: IsolationCheck,
        write_target: WriteTarget,
        records: Vec<AcceptanceRecord>,
        candidates: Vec<Candidate>,
        report: Option<PrecheckReport>,
        viewer: Viewer,
        attempt: AttemptCreation,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                level: AdmissionDecision::Admitted {
                    level: HandoffLevel::L2,
                },
                snapshot: snapshot(Vec::new(), Vec::new()),
                isolation: confirmed_isolation(),
                write_target: WriteTarget::new("ws-1", "/ws/src/main.rs"),
                records: Vec::new(),
                candidates: Vec::new(),
                report: None,
                viewer: Viewer::new("manager-2", 7, vec!["workspace:read".to_string()]),
                attempt: AttemptCreation {
                    attempt_id: "attempt-new".to_string(),
                    fencing_token: 4,
                },
            }
        }

        fn build(&self) -> Result<ContinuationCard, CardError> {
            self.build_at(TakeoverPhase::AttemptCreated)
        }

        fn build_at(&self, phase: TakeoverPhase) -> Result<ContinuationCard, CardError> {
            ContinuationCard::build(CardSpec {
                from_attempt_id: "attempt-old".to_string(),
                new_attempt: Some(&self.attempt),
                baseline_fencing_token: Some(3),
                phase,
                level: &self.level,
                snapshot: Some(&self.snapshot),
                isolation: Some(&self.isolation),
                write_target: Some(&self.write_target),
                acceptance_records: &self.records,
                candidates: &self.candidates,
                precheck: self.report.as_ref(),
                work_package_revision: WP,
                viewer: &self.viewer,
            })
        }
    }

    fn fixture_with_records() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.records = vec![
            acceptance("acc-1", "art-1", "rev-1", WP),
            acceptance("acc-2", "art-2", "rev-2", WP),
        ];
        fixture
    }

    fn snapshot(missing: Vec<MissingPart>, integrity: Vec<SegmentIntegrity>) -> HandoffSnapshot {
        HandoffSnapshot {
            header: HandoffPackageHeader {
                schema: HANDOFF_SCHEMA.to_string(),
                level: HandoffLevel::L2,
                company_id: DomainId::new("company-1"),
                project_id: DomainId::new("project-1"),
                issue_id: DomainId::new("issue-1"),
                work_package_revision: DomainId::new(WP),
                source_attempt_id: DomainId::new("attempt-old"),
                snapshot_seq: 3,
            },
            binding: SnapshotBinding::new("input-r1", WP, "scene-r1", 3),
            source: CaptureSource::HostReconstructed,
            missing,
            integrity,
            captured_at: "2026-11-20T10:00:00Z".to_string(),
        }
    }

    fn acceptance(id: &str, artifact: &str, revision: &str, wp: &str) -> AcceptanceRecord {
        AcceptanceRecord {
            acceptance_id: DomainId::new(id),
            artifact_id: DomainId::new(artifact),
            binding: AcceptanceBinding {
                work_package_revision: DomainId::new(wp),
                input_baseline: DomainId::new("input-r1"),
                artifact_revision: DomainId::new(revision),
                verification_rules: DomainId::new("rules-1"),
                target_environment: DomainId::new("env-1"),
                merge_combination: None,
            },
            decision: AcceptanceDecision::Accepted,
            actor: DomainId::new("manager-1"),
            reason: "按验收规则通过".to_string(),
            evidence_refs: vec!["evidence:test-1".to_string()],
            accepted_at_ms: 1_700_000_000_000,
            superseded_by: None,
        }
    }

    fn candidate(artifact: &str, revision: &str, accepted: bool) -> Candidate {
        Candidate {
            artifact_id: DomainId::new(artifact),
            revision_id: DomainId::new(revision),
            digest: Digest::of_bytes(revision.as_bytes()),
            verification_state: VerificationState::Pending,
            accepted,
        }
    }

    fn confirmed_isolation() -> IsolationCheck {
        IsolationCheck::new(
            "attempt-old",
            StopStatus::Confirmed {
                invocation_id: "inv-1".to_string(),
                evidence_ref: "result:inv-1".to_string(),
            },
            SurfaceKnowledge::Enumerated {
                paths: vec!["/ws/src/main.rs".to_string()],
            },
        )
        .with_basis(IsolationBasis::EnvironmentDestroyed {
            reference: "destroy:env-1".to_string(),
        })
        .observed_at("2026-11-20T10:30:00Z")
    }

    fn ack_only_isolation() -> IsolationCheck {
        IsolationCheck::new(
            "attempt-old",
            StopStatus::AckOnly {
                ack_ref: "ack:inv-1".to_string(),
            },
            SurfaceKnowledge::Enumerated {
                paths: vec!["/ws/src/main.rs".to_string()],
            },
        )
        .with_basis(IsolationBasis::EnvironmentDestroyed {
            reference: "destroy:env-1".to_string(),
        })
        .observed_at("2026-11-20T10:30:00Z")
    }

    /// 用 2.4 的真实判定产出报告：`ready=false` 时改一个字节让摘要不符。
    fn precheck(attempt_id: &str, ready: bool) -> PrecheckReport {
        use crate::handoff_manifest::ManifestBuilder;

        let mut builder = ManifestBuilder::new();
        builder.add_file("src/main.rs", b"fn main() {}");
        let manifest = builder
            .build("company-1", "project-1", WP)
            .expect("manifest");
        let observed = ObservedWorkspace::new(vec![ObservedFile::present(
            "src/main.rs",
            if ready {
                Digest::of_bytes(b"fn main() {}")
            } else {
                Digest::of_bytes(b"fn main() { tampered }")
            },
        )])
        .expect("observed");
        RebuildPrecheck::new(
            attempt_id,
            WP,
            "2026-11-20T11:08:30Z",
            "manager-1",
            Authorization::new(7, "2026-11-20T11:00:00Z")
                .unwrap()
                .grant("workspace:write"),
            7,
            "workspace:write",
        )
        .unwrap()
        .with_goal(GoalAssessment::no_material_difference(1))
        .evaluate(&manifest, &observed)
        .expect("report")
    }

    fn downgraded() -> AdmissionDecision {
        AdmissionDecision::Downgraded {
            from: HandoffLevel::L3,
            to: HandoffLevel::L2,
            missing: vec![
                MissingEvidence::CombinationNotTested,
                MissingEvidence::Stale,
            ],
        }
    }

    #[test]
    fn card_lists_level_preserved_missing_stop_and_steps() {
        let mut fixture = fixture_with_records();
        fixture.snapshot = snapshot(
            vec![
                MissingPart::new("runtime_state", MissingReason::NotExportable),
                MissingPart::new("session_notes", MissingReason::Redacted),
            ],
            vec![SegmentIntegrity::verified(
                "workspace",
                &Digest::of_bytes(b"ws"),
            )],
        );
        fixture.report = Some(precheck("attempt-new", true));
        let card = fixture.build().unwrap();

        assert_eq!(card.schema, CARD_SCHEMA);
        assert_eq!(card.preserved_count(), 2);
        assert_eq!(card.candidate_count(), 0);
        assert_eq!(card.missing_count(), 2);
        assert!(card.steps_done() > 0);
        assert_eq!(card.stop.status, "confirmed");
        assert!(card.stop.confirmed);
        assert_eq!(card.authority.scopes, vec!["workspace:read".to_string()]);

        let lines = card.render_lines();
        let text = lines.join("\n");
        for section in [
            "可接续等级 L2",
            "已保留成果 2 项",
            "仍缺内容 2 项",
            "停止状态：已确认停止",
            "重建步骤",
            "观察者 manager-2",
        ] {
            assert!(text.contains(section), "缺少分区：{section}\n{text}");
        }
        assert!(text.contains("runtime_state（not_exportable）"), "{text}");
        assert!(text.contains("session_notes（redacted）"), "{text}");
    }

    #[test]
    fn downgraded_level_shows_admitted_level_and_missing_evidence() {
        let mut fixture = Fixture::new();
        fixture.level = downgraded();
        let card = fixture.build().unwrap();

        assert_eq!(card.level.admitted, HandoffLevel::L2);
        assert_eq!(card.level.requested, HandoffLevel::L3);
        assert!(card.level.downgraded);
        let line = card.level.line();
        assert!(line.contains("可接续等级 L2"), "{line}");
        assert!(line.contains("请求 L3 已降级"), "{line}");
        assert!(line.contains("combination_not_tested"), "{line}");
        assert!(line.contains("stale"), "{line}");
        // 摘要行也只说获批等级，不出现「L3 自动接续」。
        let summary = card.summary_line();
        assert!(summary.contains("→ L2"), "{summary}");
        assert!(!summary.contains("L3"), "{summary}");
    }

    #[test]
    fn candidate_is_never_preserved_even_when_claiming_accepted() {
        let mut fixture = Fixture::new();
        fixture.candidates = vec![candidate("art-9", "rev-9", true)];
        let card = fixture.build().unwrap();

        assert_eq!(card.preserved_count(), 0, "候选不得进保留成果");
        assert_eq!(card.candidate_count(), 1);
        assert!(card.candidates[0].accepted_claim());
        assert!(!card.candidates[0].line().contains("已接受）"));
        assert!(
            card.candidates[0].line().contains("仍未接受"),
            "{}",
            card.candidates[0].line()
        );
        assert_eq!(card.discrepancies.len(), 1);
        assert!(
            card.discrepancies[0].contains("自称已接受"),
            "{:?}",
            card.discrepancies
        );
        // 摘要行把候选单独成列，不加进已接受。
        let summary = card.summary_line();
        assert!(summary.contains("已接受 0 项"), "{summary}");
        assert!(summary.contains("候选 1 项（未计入）"), "{summary}");
    }

    #[test]
    fn acceptance_for_another_revision_is_stale_not_preserved() {
        let mut fixture = Fixture::new();
        fixture.records = vec![acceptance("acc-old", "art-1", "rev-1", "wp-r2")];
        let card = fixture.build().unwrap();

        assert_eq!(card.preserved_count(), 0, "旧修订的接受不得算保留");
        assert_eq!(card.stale_acceptances.len(), 1);
        assert_eq!(card.stale_acceptances[0].reason, "work_package_mismatch");
        assert!(
            card.render_lines()
                .join("\n")
                .contains("不批准当前修订的历史接受 1 项")
        );
    }

    #[test]
    fn superseded_acceptance_is_stale() {
        let mut record = acceptance("acc-1", "art-1", "rev-1", WP);
        record.superseded_by = Some(DomainId::new("acc-2"));
        let mut fixture = Fixture::new();
        fixture.records = vec![record];
        let card = fixture.build().unwrap();

        assert_eq!(card.preserved_count(), 0);
        assert_eq!(card.stale_acceptances[0].reason, "superseded");
    }

    #[test]
    fn requires_human_acceptance_is_not_preserved() {
        let mut record = acceptance("acc-1", "art-1", "rev-1", WP);
        record.decision = AcceptanceDecision::RequiresHuman;
        let mut fixture = Fixture::new();
        fixture.records = vec![record];
        let card = fixture.build().unwrap();

        assert_eq!(card.preserved_count(), 0);
        assert_eq!(card.stale_acceptances[0].reason, "requires_human");
    }

    #[test]
    fn manager_change_does_not_alter_acceptance_records() {
        let fixture = fixture_with_records();
        let before = fixture.build().unwrap();

        // 换人：仅观察者变，宿主接受记录不变。
        let mut after_fixture = fixture_with_records();
        after_fixture.viewer = Viewer::new("manager-3", 9, vec!["workspace:read".to_string()]);
        let after = after_fixture.build().unwrap();

        assert_eq!(before.preserved, after.preserved, "换人不得改变保留成果");
        assert_eq!(after.preserved[0].actor(), "manager-1", "验收人保持原样");
        assert_eq!(after.preserved[0].acceptance_id(), "acc-1");
        assert_eq!(after.preserved[0].accepted_at_ms(), 1_700_000_000_000);
        assert_eq!(after.authority.subject_id, "manager-3");
        assert_eq!(after.authority.epoch, 9);
    }

    #[test]
    fn level_grants_no_extra_authority() {
        let mut low = Fixture::new();
        low.level = AdmissionDecision::Admitted {
            level: HandoffLevel::L1,
        };
        let mut high = Fixture::new();
        high.level = AdmissionDecision::Admitted {
            level: HandoffLevel::L3,
        };

        let low_card = low.build().unwrap();
        let high_card = high.build().unwrap();
        assert_eq!(
            low_card.authority, high_card.authority,
            "等级不派生任何 scope"
        );
        assert_eq!(
            high_card.authority.scopes,
            vec!["workspace:read".to_string()]
        );
        assert!(!high_card.authority.line().contains("workspace:write"));
        assert!(high_card.authority.line().contains("不授予额外权限"));
    }

    #[test]
    fn ack_only_stop_is_never_reported_as_stopped() {
        let mut fixture = Fixture::new();
        fixture.isolation = ack_only_isolation();
        let card = fixture.build().unwrap();

        assert!(card.stop.checked);
        assert!(!card.stop.confirmed);
        assert_eq!(card.stop.status, "ack_only");
        let line = card.stop.line();
        let head = line.split('；').next().unwrap_or_default();
        assert!(!head.contains("已确认停止"), "{head}");
        assert!(!head.contains("已停止"), "{head}");
        assert!(head.contains("未证实停止"), "{head}");
        assert!(line.contains("不得据此写旧现场"), "{line}");
        assert_eq!(card.stop.writable, Some(false));
        assert!(
            card.stop
                .blocking_conditions
                .iter()
                .any(|item| item.starts_with("stop_unconfirmed")),
            "{:?}",
            card.stop.blocking_conditions
        );
        assert!(card.summary_line().contains("停止未证实"));
    }

    #[test]
    fn unchecked_isolation_says_not_checked() {
        let fixture = Fixture::new();
        let card = ContinuationCard::build(CardSpec {
            from_attempt_id: "attempt-old".to_string(),
            new_attempt: None,
            baseline_fencing_token: None,
            phase: TakeoverPhase::Requested,
            level: &fixture.level,
            snapshot: Some(&fixture.snapshot),
            isolation: None,
            write_target: None,
            acceptance_records: &[],
            candidates: &[],
            precheck: None,
            work_package_revision: WP,
            viewer: &fixture.viewer,
        })
        .unwrap();

        assert!(!card.stop.checked);
        assert_eq!(card.stop.status, "not_checked");
        assert!(!card.stop.confirmed);
        assert_eq!(card.stop.writable, None, "未检查不等于允许");
        let line = card.stop.line();
        assert!(line.contains("尚未检查"), "{line}");
        assert!(!line.contains("不明"), "{line}");
    }

    #[test]
    fn confirmed_stop_and_isolation_allows_write_to_same_surface() {
        let fixture = Fixture::new();
        let card = fixture.build().unwrap();
        assert_eq!(card.stop.writable, Some(true));
        assert!(card.stop.line().contains("写判定：允许"));
        assert!(card.stop.blocking_conditions.is_empty());
    }

    #[test]
    fn missing_parts_keep_reasons_and_unverified_segments_are_separate() {
        let mut fixture = Fixture::new();
        fixture.snapshot = snapshot(
            vec![
                MissingPart::new("runtime_state", MissingReason::NotExportable),
                MissingPart::new("logs", MissingReason::CaptureFailed),
            ],
            vec![
                SegmentIntegrity::verified("workspace", &Digest::of_bytes(b"ws")),
                SegmentIntegrity::unverified("tool_output", "没核验"),
            ],
        );
        let card = fixture.build().unwrap();

        assert_eq!(card.missing_count(), 2);
        assert_eq!(card.missing[0].reason, "not_exportable");
        assert_eq!(card.missing[1].reason, "capture_failed");
        assert_eq!(card.unverified_segments, vec!["tool_output".to_string()]);
        let text = card.render_lines().join("\n");
        assert!(text.contains("内容已取到但未核验，不算完整"), "{text}");
    }

    #[test]
    fn absent_snapshot_reports_capture_missing() {
        let fixture = Fixture::new();
        let card = ContinuationCard::build(CardSpec {
            from_attempt_id: "attempt-old".to_string(),
            new_attempt: None,
            baseline_fencing_token: None,
            phase: TakeoverPhase::AwaitingSummary,
            level: &fixture.level,
            snapshot: None,
            isolation: None,
            write_target: None,
            acceptance_records: &[],
            candidates: &[],
            precheck: None,
            work_package_revision: WP,
            viewer: &fixture.viewer,
        })
        .unwrap();

        assert_eq!(card.missing_count(), 1);
        assert_eq!(card.missing[0].part, "handoff_snapshot");
        assert_eq!(card.missing[0].reason, "not_captured");
    }

    #[test]
    fn steps_come_from_precheck_and_stay_incomplete_without_it() {
        let fixture = Fixture::new();
        let card = fixture.build().unwrap();
        assert_eq!(card.steps.len(), REBUILD_STEPS.len());
        assert_eq!(card.steps_done(), 0, "未跑预检时不能有步骤算完成");
        assert!(card.steps.iter().all(|step| !step.done));

        let mut with_report = Fixture::new();
        with_report.report = Some(precheck("attempt-new", true));
        let card = with_report.build().unwrap();
        assert!(card.blockers.is_empty());
        assert_eq!(card.open_questions, 0);
        // 报告就绪不等于 8 步全完成：只读/测试预检没有运行记录，最后一步也还没继续执行。
        assert_eq!(card.steps_done(), 6);
        assert!(!card.steps[4].done, "只读预检未运行不得算完成");
        assert!(!card.steps[5].done, "测试预检未运行不得算完成");
        assert!(card.steps[7].done, "无阻塞且无待答问题才允许继续");
    }

    #[test]
    fn blocked_precheck_surfaces_blockers_and_keeps_steps_honest() {
        let mut fixture = Fixture::new();
        fixture.report = Some(precheck("attempt-new", false));
        let card = fixture.build().unwrap();

        assert_eq!(card.blockers.len(), 1);
        assert_eq!(card.blockers[0].code, "digest_mismatch");
        assert!(
            card.steps_done() < REBUILD_STEPS.len(),
            "有阻塞时不可能全部步骤完成"
        );
        let text = card.render_lines().join("\n");
        assert!(text.contains("阻塞 1 项"), "{text}");
        assert!(text.contains("digest_mismatch src/main.rs"), "{text}");
    }

    #[test]
    fn precheck_from_another_attempt_is_rejected() {
        let mut fixture = Fixture::new();
        fixture.report = Some(precheck("attempt-other", true));
        match fixture.build() {
            Err(CardError::AttemptMismatch { expected, actual }) => {
                assert_eq!(expected, "attempt-new");
                assert_eq!(actual, "attempt-other");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn fencing_token_not_above_baseline_is_flagged() {
        let mut fixture = Fixture::new();
        fixture.attempt = AttemptCreation {
            attempt_id: "attempt-new".to_string(),
            fencing_token: 2,
        };
        let card = fixture.build().unwrap();
        assert_eq!(card.discrepancies.len(), 1);
        assert!(card.discrepancies[0].contains("未高于基线"));
    }

    #[test]
    fn empty_attempt_or_viewer_is_rejected() {
        let fixture = Fixture::new();
        let base = || CardSpec {
            from_attempt_id: String::new(),
            new_attempt: None,
            baseline_fencing_token: None,
            phase: TakeoverPhase::Requested,
            level: &fixture.level,
            snapshot: None,
            isolation: None,
            write_target: None,
            acceptance_records: &[],
            candidates: &[],
            precheck: None,
            work_package_revision: WP,
            viewer: &fixture.viewer,
        };
        assert_eq!(
            ContinuationCard::build(base()).unwrap_err(),
            CardError::EmptyField("from_attempt_id")
        );

        let mut spec = base();
        spec.from_attempt_id = "attempt-old".to_string();
        spec.work_package_revision = "  ";
        assert_eq!(
            ContinuationCard::build(spec).unwrap_err(),
            CardError::EmptyField("work_package_revision")
        );

        let empty_viewer = Viewer::new("  ", 1, Vec::new());
        let mut spec = base();
        spec.from_attempt_id = "attempt-old".to_string();
        spec.viewer = &empty_viewer;
        assert_eq!(
            ContinuationCard::build(spec).unwrap_err(),
            CardError::EmptyField("viewer.subject_id")
        );
    }
}
