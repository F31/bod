//! P2 任务 1.4：部分快照（missing / 来源 / 完整性状态）与版本绑定。
//!
//! 权威语义：V2.9.6 §18.11——崩溃、预算耗尽或强制停止时生成**尽力快照**，带 missing、
//! 来源与完整性状态；部分包 MUST 可持久登记供人工恢复，不能因为字段缺失就禁止保存唯一现场。
//! 版本绑定只使用实际输入 / 工作包 / 现场 revision 与 `snapshot_seq`，无关元数据不构成工作失效。
//!
//! 本模块的边界：
//! - 只判定「这份现场还够不够用」，不判定「能不能自动接续」——后者是 1.1 的等级准入，
//!   两者分开是为了让「缺内容」与「没测过」不被混成一个理由；
//! - 绑定比较只看类型里存在的四个维度。标题、评论、标签、更新时间不在绑定类型中，
//!   因此它们**无法**参与失效判定（不是比较时过滤，而是没有可比对的入口）；
//! - 摘要只从「已核验」的 payload 片段报告；`Unverified` 表示没核验过，不等于完整。
//! - 只写快照登记表，不写第二业务库，不改成果、账本或 Issue 状态。

use crate::canonical_json;
use crate::domain::DomainId;
use crate::domain::artifact::Digest;
use crate::handoff_package::{HANDOFF_SCHEMA, HandoffLevel, HandoffPackageHeader};
use crate::store::{SqliteStore, StoreError};
use serde::{Deserialize, Serialize};
use std::fmt;

/// payload 完整性片段名。摘要只从这一段取，避免与工作区/清单片段的摘要混用。
pub const PAYLOAD_SEGMENT: &str = "payload";

/// 快照是在什么情形下捕获的。崩溃/耗尽/强停都属于「尽力快照」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureSource {
    /// 正常交接：执行 Agent 提交的候选。
    AgentSubmitted,
    /// 宿主从版本化工作区与账本重建。
    HostReconstructed,
    /// 执行端崩溃后的尽力捕获。
    CrashRecovered,
    /// 预算耗尽时的捕获。
    BudgetExhausted,
    /// 强制停止时的捕获。
    ForceStopped,
}

impl CaptureSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentSubmitted => "agent_submitted",
            Self::HostReconstructed => "host_reconstructed",
            Self::CrashRecovered => "crash_recovered",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ForceStopped => "force_stopped",
        }
    }

    /// 尽力快照：允许缺内容，但不允许冒充完整或已测组合。
    pub const fn is_best_effort(self) -> bool {
        matches!(
            self,
            Self::CrashRecovered | Self::BudgetExhausted | Self::ForceStopped
        )
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "agent_submitted" => Some(Self::AgentSubmitted),
            "host_reconstructed" => Some(Self::HostReconstructed),
            "crash_recovered" => Some(Self::CrashRecovered),
            "budget_exhausted" => Some(Self::BudgetExhausted),
            "force_stopped" => Some(Self::ForceStopped),
            _ => None,
        }
    }
}

impl fmt::Display for CaptureSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 缺失原因。missing 列表必须带原因，否则界面只能让用户猜「为什么没有」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissingReason {
    /// 崩溃/强停来不及捕获。
    NotCaptured,
    /// 尝试捕获但失败（无权限、IO、目标不可达）。
    CaptureFailed,
    /// 运行时内部状态不可导出（§18.9 明确不承诺导出内部推理与会话）。
    NotExportable,
    /// 含凭据或未授权客户资料而被过滤（1.5 细化过滤规则）。
    Redacted,
}

impl MissingReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotCaptured => "not_captured",
            Self::CaptureFailed => "capture_failed",
            Self::NotExportable => "not_exportable",
            Self::Redacted => "redacted",
        }
    }
}

impl fmt::Display for MissingReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 未取得的部分现场。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingPart {
    pub part: String,
    pub reason: MissingReason,
}

impl MissingPart {
    pub fn new(part: impl Into<String>, reason: MissingReason) -> Self {
        Self {
            part: part.into(),
            reason,
        }
    }
}

impl fmt::Display for MissingPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.part, self.reason.as_str())
    }
}

/// 单个片段的完整性状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntegrityStatus {
    /// 已按字节/规范编码核验过摘要。
    Verified,
    /// 拿到了内容但没有核验；不能据此声称完整。
    Unverified,
    /// 这一段根本没拿到。
    Missing,
    /// 摘要与记录不符：内容不可信，即使文件存在。
    DigestMismatch,
}

impl IntegrityStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverified => "unverified",
            Self::Missing => "missing",
            Self::DigestMismatch => "digest_mismatch",
        }
    }

    /// 只有已核验才算「完整」。`Unverified` 是没核验，不是通过。
    pub const fn claims_intact(self) -> bool {
        matches!(self, Self::Verified)
    }

    /// 明确的异常：缺内容或摘要对不上。
    pub const fn is_problem(self) -> bool {
        matches!(self, Self::Missing | Self::DigestMismatch)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "verified" => Some(Self::Verified),
            "unverified" => Some(Self::Unverified),
            "missing" => Some(Self::Missing),
            "digest_mismatch" => Some(Self::DigestMismatch),
            _ => None,
        }
    }
}

impl fmt::Display for IntegrityStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个片段的完整性记录。`digest` 只在已核验或摘要不符时出现（后者供比对差异）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentIntegrity {
    pub segment: String,
    pub status: IntegrityStatus,
    pub digest: Option<String>,
    pub note: String,
}

impl SegmentIntegrity {
    /// 已核验片段：必须带摘要，否则「已核验」无从证明。
    pub fn verified(segment: impl Into<String>, digest: &Digest) -> Self {
        Self {
            segment: segment.into(),
            status: IntegrityStatus::Verified,
            digest: Some(digest.as_string()),
            note: String::new(),
        }
    }

    pub fn missing(segment: impl Into<String>, note: impl Into<String>) -> Self {
        Self {
            segment: segment.into(),
            status: IntegrityStatus::Missing,
            digest: None,
            note: note.into(),
        }
    }

    pub fn unverified(segment: impl Into<String>, note: impl Into<String>) -> Self {
        Self {
            segment: segment.into(),
            status: IntegrityStatus::Unverified,
            digest: None,
            note: note.into(),
        }
    }

    pub fn digest_mismatch(
        segment: impl Into<String>,
        expected: &Digest,
        note: impl Into<String>,
    ) -> Self {
        Self {
            segment: segment.into(),
            status: IntegrityStatus::DigestMismatch,
            digest: Some(expected.as_string()),
            note: note.into(),
        }
    }
}

/// 版本绑定（§18.11）：实际输入 / 工作包 / 现场 revision 与 `snapshot_seq`。
///
/// 这里**没有**标题、评论、标签、更新时间等元数据字段——无关元数据变化无法参与失效判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBinding {
    /// 输入（需求/目标/材料）的实际 revision。
    pub input_revision: DomainId,
    /// 工作包的实际 revision；必须与 header 一致。
    pub work_package_revision: DomainId,
    /// 现场 revision（基线提交 + 补丁 + 未跟踪清单的合成版本）。
    pub scene_revision: DomainId,
    /// 快照序号；只前进表示有更新的快照，不使已接受的工作失效。
    pub snapshot_seq: u64,
}

impl SnapshotBinding {
    pub fn new(
        input_revision: impl Into<String>,
        work_package_revision: impl Into<String>,
        scene_revision: impl Into<String>,
        snapshot_seq: u64,
    ) -> Self {
        Self {
            input_revision: DomainId::new(input_revision),
            work_package_revision: DomainId::new(work_package_revision),
            scene_revision: DomainId::new(scene_revision),
            snapshot_seq,
        }
    }
}

/// 观察到的当前元数据。刻意与 [`SnapshotBinding`] 分开：元数据只用于提示改了什么，
/// 不参与失效判定，也**不持久化**——一旦落库就会被误当成失效依据。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetadataStamp {
    pub title: String,
    pub labels: Vec<String>,
    pub comment_count: u64,
    pub updated_at_ms: i64,
}

/// 再次观察到的现场：绑定 + 元数据戳。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedWork {
    pub binding: SnapshotBinding,
    pub metadata: MetadataStamp,
}

/// 元数据变化明细。只用于界面提示「改了什么」，不构成失效理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataChange {
    Title,
    Labels,
    CommentCount,
    UpdatedAt,
}

impl MetadataChange {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Labels => "labels",
            Self::CommentCount => "comment_count",
            Self::UpdatedAt => "updated_at",
        }
    }
}

/// 相关差异：只有实际 revision 变化才会让已接受的工作失效。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingDifference {
    InputChanged { from: String, to: String },
    WorkPackageChanged { from: String, to: String },
    SceneChanged { from: String, to: String },
}

impl BindingDifference {
    pub fn dimension(&self) -> &'static str {
        match self {
            Self::InputChanged { .. } => "input_revision",
            Self::WorkPackageChanged { .. } => "work_package_revision",
            Self::SceneChanged { .. } => "scene_revision",
        }
    }
}

impl fmt::Display for BindingDifference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputChanged { from, to } => {
                write!(f, "input_revision {from} → {to}")
            }
            Self::WorkPackageChanged { from, to } => {
                write!(f, "work_package_revision {from} → {to}")
            }
            Self::SceneChanged { from, to } => {
                write!(f, "scene_revision {from} → {to}")
            }
        }
    }
}

/// 绑定判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingVerdict {
    /// 绑定未变，工作仍然有效。
    Current,
    /// 只有 `snapshot_seq` 前进：有更新的快照，旧工作不失效。
    Superseded { from: u64, to: u64 },
    /// 实际 revision 变了：需要计算相关差异，可能使工作失效。
    Drifted { differences: Vec<BindingDifference> },
}

impl BindingVerdict {
    /// 是否使已接受/正在进行的工作失效。`Superseded` 不算失效。
    pub fn invalidates_work(&self) -> bool {
        matches!(self, Self::Drifted { .. })
    }

    pub fn differences(&self) -> &[BindingDifference] {
        match self {
            Self::Drifted { differences } => differences,
            _ => &[],
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Superseded { .. } => "superseded",
            Self::Drifted { .. } => "drifted",
        }
    }
}

/// 绑定判定报告：结论 + 元数据变化提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingReport {
    pub verdict: BindingVerdict,
    /// 元数据变化明细；只作提示，即使非空也不改变 `verdict`。
    pub metadata_changes: Vec<MetadataChange>,
}

impl BindingReport {
    /// 界面一句话结论：结论 + 元数据变化数量（明确说明后者不影响接续）。
    pub fn summary_line(&self) -> String {
        let base = match &self.verdict {
            BindingVerdict::Current => "绑定未变".to_string(),
            BindingVerdict::Superseded { from, to } => format!("已有更新快照（{from} → {to}）"),
            BindingVerdict::Drifted { differences } => format!(
                "绑定已变化：{}",
                differences
                    .iter()
                    .map(BindingDifference::to_string)
                    .collect::<Vec<_>>()
                    .join("；")
            ),
        };
        if self.metadata_changes.is_empty() {
            base
        } else {
            format!(
                "{base}；元数据变化 {} 项（不影响接续）",
                self.metadata_changes.len()
            )
        }
    }
}

/// 绑定比较失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    /// 观察到的序号比记录更旧：说明读到的是过期现场，不能据此判定失效。
    RegressedSnapshotSeq { recorded: u64, observed: u64 },
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegressedSnapshotSeq { recorded, observed } => write!(
                f,
                "观察到的 snapshot_seq {observed} 早于已登记快照 {recorded}，请重新读取现场"
            ),
        }
    }
}

impl std::error::Error for BindingError {}

/// 比较已登记快照与当前观察到的现场。
///
/// 结论只由 `input` / `work_package` / `scene` 三个实际 revision 与 `snapshot_seq` 决定；
/// 元数据变化只写入 [`BindingReport::metadata_changes`]，无论多少条都不改变 `verdict`。
/// 元数据基线由调用方提供（登记时的读视图），只用于解释「改了什么」。
pub fn evaluate_binding(
    recorded: &SnapshotBinding,
    recorded_metadata: &MetadataStamp,
    observed: &ObservedWork,
) -> Result<BindingReport, BindingError> {
    if observed.binding.snapshot_seq < recorded.snapshot_seq {
        return Err(BindingError::RegressedSnapshotSeq {
            recorded: recorded.snapshot_seq,
            observed: observed.binding.snapshot_seq,
        });
    }

    let mut differences = Vec::new();
    if observed.binding.input_revision != recorded.input_revision {
        differences.push(BindingDifference::InputChanged {
            from: recorded.input_revision.as_str().to_string(),
            to: observed.binding.input_revision.as_str().to_string(),
        });
    }
    if observed.binding.work_package_revision != recorded.work_package_revision {
        differences.push(BindingDifference::WorkPackageChanged {
            from: recorded.work_package_revision.as_str().to_string(),
            to: observed.binding.work_package_revision.as_str().to_string(),
        });
    }
    if observed.binding.scene_revision != recorded.scene_revision {
        differences.push(BindingDifference::SceneChanged {
            from: recorded.scene_revision.as_str().to_string(),
            to: observed.binding.scene_revision.as_str().to_string(),
        });
    }

    let verdict = if differences.is_empty() {
        if observed.binding.snapshot_seq > recorded.snapshot_seq {
            BindingVerdict::Superseded {
                from: recorded.snapshot_seq,
                to: observed.binding.snapshot_seq,
            }
        } else {
            BindingVerdict::Current
        }
    } else {
        BindingVerdict::Drifted { differences }
    };

    Ok(BindingReport {
        verdict,
        metadata_changes: metadata_changes(recorded_metadata, &observed.metadata),
    })
}

/// 元数据差异只做提示。注意返回值不进入 [`BindingVerdict`] 的构造路径。
fn metadata_changes(recorded: &MetadataStamp, observed: &MetadataStamp) -> Vec<MetadataChange> {
    let mut changes = Vec::new();
    if recorded.title != observed.title {
        changes.push(MetadataChange::Title);
    }
    let mut recorded_labels = recorded.labels.clone();
    let mut observed_labels = observed.labels.clone();
    recorded_labels.sort();
    observed_labels.sort();
    if recorded_labels != observed_labels {
        changes.push(MetadataChange::Labels);
    }
    if recorded.comment_count != observed.comment_count {
        changes.push(MetadataChange::CommentCount);
    }
    if recorded.updated_at_ms != observed.updated_at_ms {
        changes.push(MetadataChange::UpdatedAt);
    }
    changes
}

/// 快照完整度。只有「没有缺失项且所有片段都已核验」才算完整。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    Partial,
}

impl Completeness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

impl fmt::Display for Completeness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次捕获的交接现场。缺内容与完整性状态分开记录，避免「缺一个字段」就丢掉整份现场。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSnapshot {
    pub header: HandoffPackageHeader,
    pub binding: SnapshotBinding,
    pub source: CaptureSource,
    pub missing: Vec<MissingPart>,
    pub integrity: Vec<SegmentIntegrity>,
    /// 采集时间，必须是 1.2 的规范时间（秒级 UTC）。
    pub captured_at: String,
}

impl HandoffSnapshot {
    /// 派生完整度：缺项或任一片段未核验即为部分。
    pub fn completeness(&self) -> Completeness {
        let has_missing = self.missing.iter().any(|part| !part.part.trim().is_empty());
        let all_verified = self
            .integrity
            .iter()
            .all(|segment| segment.status.claims_intact());
        if has_missing || !all_verified {
            Completeness::Partial
        } else {
            Completeness::Complete
        }
    }

    pub fn is_partial(&self) -> bool {
        self.completeness() == Completeness::Partial
    }

    /// 去重后的缺失项（保持登记顺序），供界面「仍缺什么」。
    pub fn missing_parts(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for part in &self.missing {
            if part.part.trim().is_empty() {
                continue;
            }
            let rendered = part.to_string();
            if !out.contains(&rendered) {
                out.push(rendered);
            }
        }
        out
    }

    /// 未达已核验的片段（含 `Unverified`）：都是「不能说完整」的理由。
    pub fn integrity_problems(&self) -> Vec<String> {
        self.integrity
            .iter()
            .filter(|segment| !segment.status.claims_intact())
            .map(|segment| format!("{}:{}", segment.segment, segment.status.as_str()))
            .collect()
    }

    /// payload 片段；摘要只从这一段取。
    pub fn payload_segment(&self) -> Option<&SegmentIntegrity> {
        self.integrity
            .iter()
            .find(|segment| segment.segment == PAYLOAD_SEGMENT)
    }

    /// 已核验的 payload 摘要。`Unverified`/`Missing`/摘要不符一律返回 `None`。
    pub fn payload_digest(&self) -> Option<&str> {
        self.payload_segment()
            .filter(|segment| segment.status.claims_intact())
            .and_then(|segment| segment.digest.as_deref())
    }

    /// 登记键：同一 Company 内按 `snapshot_seq` 唯一。同序号重复登记不得覆盖既有现场。
    pub fn registration_key(&self) -> String {
        format!(
            "{}#{}",
            self.header.company_id.as_str(),
            self.binding.snapshot_seq
        )
    }

    /// 界面一句话结论：完整度 + 来源 + 缺什么。
    pub fn completeness_line(&self) -> String {
        let mut line = format!(
            "{}（来源 {}）",
            self.completeness().as_str(),
            self.source.as_str()
        );
        let missing = self.missing_parts();
        if !missing.is_empty() {
            line.push_str(&format!(
                "；缺 {} 项：{}",
                missing.len(),
                missing.join(", ")
            ));
        }
        let problems = self.integrity_problems();
        if !problems.is_empty() {
            line.push_str(&format!("；完整性异常 {}", problems.join(", ")));
        }
        line
    }

    /// 登记前的自检：只校验宿主输入本身，不判定能否自动接续。
    pub fn validate(&self) -> Result<(), SnapshotError> {
        if self.header.schema != HANDOFF_SCHEMA {
            return Err(SnapshotError::UnsupportedSchema(self.header.schema.clone()));
        }
        for (field, value) in [
            ("company_id", self.header.company_id.as_str()),
            ("project_id", self.header.project_id.as_str()),
            ("issue_id", self.header.issue_id.as_str()),
            (
                "work_package_revision",
                self.header.work_package_revision.as_str(),
            ),
            ("source_attempt_id", self.header.source_attempt_id.as_str()),
            ("input_revision", self.binding.input_revision.as_str()),
            ("scene_revision", self.binding.scene_revision.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(SnapshotError::MissingIdentity(field));
            }
        }
        if self.binding.snapshot_seq == 0 {
            return Err(SnapshotError::MissingSnapshotSeq);
        }
        if self.header.work_package_revision != self.binding.work_package_revision {
            return Err(SnapshotError::BindingMismatch {
                header: self.header.work_package_revision.as_str().to_string(),
                binding: self.binding.work_package_revision.as_str().to_string(),
            });
        }
        if !canonical_json::is_canonical_timestamp(&self.captured_at) {
            return Err(SnapshotError::NonCanonicalCapturedAt(
                self.captured_at.clone(),
            ));
        }

        let mut seen: Vec<&str> = Vec::new();
        for segment in &self.integrity {
            if segment.segment.trim().is_empty() {
                return Err(SnapshotError::MissingIdentity("integrity.segment"));
            }
            if seen.contains(&segment.segment.as_str()) {
                return Err(SnapshotError::DuplicateSegment(segment.segment.clone()));
            }
            seen.push(segment.segment.as_str());
            if let Some(digest) = &segment.digest
                && Digest::parse(digest).is_none()
            {
                return Err(SnapshotError::MalformedDigest(digest.clone()));
            }
            // 「已核验」必须带摘要；带摘要却标未核验则自相矛盾，一并拒绝。
            match (segment.status, segment.digest.is_some()) {
                (IntegrityStatus::Verified, false) => {
                    return Err(SnapshotError::PayloadDigestMissing(segment.segment.clone()));
                }
                (IntegrityStatus::Unverified, true) | (IntegrityStatus::Missing, true) => {
                    return Err(SnapshotError::MalformedDigest(format!(
                        "{} 标为 {} 却带摘要",
                        segment.segment,
                        segment.status.as_str()
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 持久登记。同键同内容幂等返回；同键不同内容拒绝覆盖并报冲突。
    pub fn register(
        &self,
        store: &SqliteStore,
        registered_at_ms: i64,
    ) -> Result<RegistrationOutcome, SnapshotError> {
        self.validate()?;
        let row = HandoffSnapshotRow::from_snapshot(self, registered_at_ms)?;
        if store.insert_handoff_snapshot(&row)? {
            return Ok(RegistrationOutcome::Registered {
                snapshot_key: row.snapshot_key,
            });
        }
        let existing = store
            .get_handoff_snapshot(row.company_id.as_str(), &row.snapshot_key)?
            .ok_or_else(|| SnapshotError::RegistrationRaceLost {
                snapshot_key: row.snapshot_key.clone(),
            })?;
        if existing.content_eq(&row) {
            Ok(RegistrationOutcome::AlreadyRegistered {
                snapshot_key: row.snapshot_key,
            })
        } else {
            Err(SnapshotError::SnapshotKeyConflict {
                snapshot_key: row.snapshot_key,
                existing: existing.payload_digest,
                incoming: row.payload_digest,
            })
        }
    }

    /// 按键读回并还原为领域值。
    pub fn load(
        store: &SqliteStore,
        company_id: &str,
        snapshot_key: &str,
    ) -> Result<Option<Self>, SnapshotError> {
        match store.get_handoff_snapshot(company_id, snapshot_key)? {
            Some(row) => Ok(Some(row.to_snapshot()?)),
            None => Ok(None),
        }
    }

    /// 列出某 Company 的部分快照：人工恢复入口只看这些。
    pub fn list_partial(store: &SqliteStore, company_id: &str) -> Result<Vec<Self>, SnapshotError> {
        store
            .list_partial_handoff_snapshots(company_id)?
            .iter()
            .map(HandoffSnapshotRow::to_snapshot)
            .collect()
    }
}

/// 登记结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationOutcome {
    Registered {
        snapshot_key: String,
    },
    /// 同键同内容重复登记：没有新建第二行，也没有改动第一次登记的现场。
    AlreadyRegistered {
        snapshot_key: String,
    },
}

impl RegistrationOutcome {
    pub fn snapshot_key(&self) -> &str {
        match self {
            Self::Registered { snapshot_key } | Self::AlreadyRegistered { snapshot_key } => {
                snapshot_key
            }
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, Self::Registered { .. })
    }
}

/// 快照登记失败。`StoreError` 既不可比较也不可克隆，因此本类型不派生 `PartialEq`/`Clone`。
#[derive(Debug)]
pub enum SnapshotError {
    /// schema 不是 `bod.handoff/v1`。
    UnsupportedSchema(String),
    /// 身份字段为空（字段名）。
    MissingIdentity(&'static str),
    /// `snapshot_seq` 为 0：没有序号就没有版本绑定。
    MissingSnapshotSeq,
    /// `captured_at` 不是秒级 UTC。
    NonCanonicalCapturedAt(String),
    /// header 与绑定里的工作包 revision 不一致。
    BindingMismatch {
        header: String,
        binding: String,
    },
    /// 同一片段被登记两次（状态互相矛盾）。
    DuplicateSegment(String),
    /// 片段标为已核验却没有摘要（payload 片段专用文案）。
    PayloadDigestMissing(String),
    /// 摘要不是 `sha256:<64 hex>`，或状态与摘要自相矛盾。
    MalformedDigest(String),
    /// 同键已存在不同内容的现场：拒绝覆盖。
    SnapshotKeyConflict {
        snapshot_key: String,
        existing: Option<String>,
        incoming: Option<String>,
    },
    /// 插入未生效却读不到已有行（并发删除）。
    RegistrationRaceLost {
        snapshot_key: String,
    },
    /// 读回的行无法还原（手工改库或迁移不一致）。
    CorruptRow(String),
    /// JSON 序列化失败（防御保留）。
    Encode(String),
    Store(StoreError),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(schema) => {
                write!(f, "不支持的交接包 schema：{schema}")
            }
            Self::MissingIdentity(field) => write!(f, "快照身份字段为空：{field}"),
            Self::MissingSnapshotSeq => f.write_str("snapshot_seq 为 0，无法建立版本绑定"),
            Self::NonCanonicalCapturedAt(value) => {
                write!(
                    f,
                    "captured_at `{value}` 不是秒级 UTC（YYYY-MM-DDTHH:MM:SSZ）"
                )
            }
            Self::BindingMismatch { header, binding } => {
                write!(f, "header 工作包修订 {header} 与绑定修订 {binding} 不一致")
            }
            Self::DuplicateSegment(segment) => write!(f, "片段 `{segment}` 被重复登记"),
            Self::PayloadDigestMissing(segment) => {
                write!(f, "片段 `{segment}` 标为已核验但没有摘要")
            }
            Self::MalformedDigest(detail) => write!(f, "摘要不合法：{detail}"),
            Self::SnapshotKeyConflict {
                snapshot_key,
                existing,
                incoming,
            } => write!(
                f,
                "快照 {snapshot_key} 已登记不同内容（原摘要 {:?}，新摘要 {:?}），拒绝覆盖",
                existing, incoming
            ),
            Self::RegistrationRaceLost { snapshot_key } => {
                write!(f, "快照 {snapshot_key} 插入未生效且已不存在，请重试")
            }
            Self::CorruptRow(detail) => write!(f, "快照行无法还原：{detail}"),
            Self::Encode(detail) => write!(f, "快照编码失败：{detail}"),
            Self::Store(err) => write!(f, "存储失败：{err}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<StoreError> for SnapshotError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

/// `handoff_snapshots` 行的扁平表示：存储层只认字符串与整数，领域类型在本模块还原。
///
/// `completeness` 是 [`HandoffSnapshot::completeness`] 的冗余副本，只为 SQL 筛选；
/// 读回时会重算并比对，防止手工改库把部分快照伪装成完整。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSnapshotRow {
    pub company_id: String,
    pub snapshot_key: String,
    pub project_id: String,
    pub issue_id: String,
    pub work_package_revision: String,
    pub source_attempt_id: String,
    pub level: String,
    pub source: String,
    pub completeness: String,
    pub missing_json: String,
    pub integrity_json: String,
    pub input_revision: String,
    pub scene_revision: String,
    pub snapshot_seq: u64,
    pub payload_digest: Option<String>,
    pub captured_at: String,
    pub registered_at_ms: i64,
}

impl HandoffSnapshotRow {
    pub fn from_snapshot(
        snapshot: &HandoffSnapshot,
        registered_at_ms: i64,
    ) -> Result<Self, SnapshotError> {
        Ok(Self {
            company_id: snapshot.header.company_id.as_str().to_string(),
            snapshot_key: snapshot.registration_key(),
            project_id: snapshot.header.project_id.as_str().to_string(),
            issue_id: snapshot.header.issue_id.as_str().to_string(),
            work_package_revision: snapshot.header.work_package_revision.as_str().to_string(),
            source_attempt_id: snapshot.header.source_attempt_id.as_str().to_string(),
            level: snapshot.header.level.as_str().to_string(),
            source: snapshot.source.as_str().to_string(),
            completeness: snapshot.completeness().as_str().to_string(),
            missing_json: serde_json::to_string(&snapshot.missing)
                .map_err(|err| SnapshotError::Encode(err.to_string()))?,
            integrity_json: serde_json::to_string(&snapshot.integrity)
                .map_err(|err| SnapshotError::Encode(err.to_string()))?,
            input_revision: snapshot.binding.input_revision.as_str().to_string(),
            scene_revision: snapshot.binding.scene_revision.as_str().to_string(),
            snapshot_seq: snapshot.binding.snapshot_seq,
            payload_digest: snapshot.payload_digest().map(str::to_string),
            captured_at: snapshot.captured_at.clone(),
            registered_at_ms,
        })
    }

    pub fn to_snapshot(&self) -> Result<HandoffSnapshot, SnapshotError> {
        let level = HandoffLevel::parse(&self.level)
            .ok_or_else(|| SnapshotError::CorruptRow(format!("未知等级 {}", self.level)))?;
        let source = CaptureSource::parse(&self.source)
            .ok_or_else(|| SnapshotError::CorruptRow(format!("未知捕获来源 {}", self.source)))?;
        let missing: Vec<MissingPart> = serde_json::from_str(&self.missing_json)
            .map_err(|err| SnapshotError::CorruptRow(format!("missing_json: {err}")))?;
        let integrity: Vec<SegmentIntegrity> = serde_json::from_str(&self.integrity_json)
            .map_err(|err| SnapshotError::CorruptRow(format!("integrity_json: {err}")))?;

        let snapshot = HandoffSnapshot {
            header: HandoffPackageHeader {
                schema: HANDOFF_SCHEMA.to_string(),
                level,
                company_id: DomainId::new(self.company_id.clone()),
                project_id: DomainId::new(self.project_id.clone()),
                issue_id: DomainId::new(self.issue_id.clone()),
                work_package_revision: DomainId::new(self.work_package_revision.clone()),
                source_attempt_id: DomainId::new(self.source_attempt_id.clone()),
                snapshot_seq: self.snapshot_seq,
            },
            binding: SnapshotBinding::new(
                self.input_revision.clone(),
                self.work_package_revision.clone(),
                self.scene_revision.clone(),
                self.snapshot_seq,
            ),
            source,
            missing,
            integrity,
            captured_at: self.captured_at.clone(),
        };

        // 冗余列必须与推导结果一致：否则是被手工改过的行，不能当真。
        if self.completeness != snapshot.completeness().as_str() {
            return Err(SnapshotError::CorruptRow(format!(
                "completeness 记录为 {}，推导为 {}",
                self.completeness,
                snapshot.completeness().as_str()
            )));
        }
        Ok(snapshot)
    }

    /// 内容等价（忽略登记时间）：用于区分幂等重复登记与真正的冲突。
    pub fn content_eq(&self, other: &Self) -> bool {
        self.company_id == other.company_id
            && self.snapshot_key == other.snapshot_key
            && self.project_id == other.project_id
            && self.issue_id == other.issue_id
            && self.work_package_revision == other.work_package_revision
            && self.source_attempt_id == other.source_attempt_id
            && self.level == other.level
            && self.source == other.source
            && self.completeness == other.completeness
            && self.missing_json == other.missing_json
            && self.integrity_json == other.integrity_json
            && self.input_revision == other.input_revision
            && self.scene_revision == other.scene_revision
            && self.snapshot_seq == other.snapshot_seq
            && self.payload_digest == other.payload_digest
            && self.captured_at == other.captured_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW_MS: i64 = 1_764_000_000_000;

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-handoff-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
    }

    fn header(seq: u64) -> HandoffPackageHeader {
        HandoffPackageHeader {
            schema: HANDOFF_SCHEMA.to_string(),
            level: HandoffLevel::L2,
            company_id: DomainId::new("company-1"),
            project_id: DomainId::new("project-1"),
            issue_id: DomainId::new("issue-1"),
            work_package_revision: DomainId::new("wp-r1"),
            source_attempt_id: DomainId::new("attempt-1"),
            snapshot_seq: seq,
        }
    }

    fn binding(seq: u64) -> SnapshotBinding {
        SnapshotBinding::new("input-r1", "wp-r1", "scene-r1", seq)
    }

    fn crash_snapshot(seq: u64) -> HandoffSnapshot {
        HandoffSnapshot {
            header: header(seq),
            binding: binding(seq),
            source: CaptureSource::CrashRecovered,
            missing: vec![
                MissingPart::new("external-session-state", MissingReason::NotExportable),
                MissingPart::new("untracked-manifest", MissingReason::CaptureFailed),
            ],
            integrity: vec![
                SegmentIntegrity::unverified("payload", "崩溃时未重新核验"),
                SegmentIntegrity::missing("untracked-files", "崩溃后目录不可读"),
            ],
            captured_at: "2026-11-20T10:00:00Z".to_string(),
        }
    }

    fn complete_snapshot(seq: u64) -> HandoffSnapshot {
        HandoffSnapshot {
            header: header(seq),
            binding: binding(seq),
            source: CaptureSource::AgentSubmitted,
            missing: Vec::new(),
            integrity: vec![
                SegmentIntegrity::verified(PAYLOAD_SEGMENT, &Digest::of_bytes(b"payload")),
                SegmentIntegrity::verified("workspace", &Digest::of_bytes(b"workspace")),
            ],
            captured_at: "2026-11-20T10:00:00Z".to_string(),
        }
    }

    #[test]
    fn partial_snapshot_reports_missing_reasons_and_integrity() {
        let snapshot = crash_snapshot(7);
        assert_eq!(snapshot.completeness(), Completeness::Partial);
        assert!(snapshot.is_partial());
        assert!(snapshot.source.is_best_effort());
        assert_eq!(
            snapshot.missing_parts(),
            vec![
                "external-session-state:not_exportable".to_string(),
                "untracked-manifest:capture_failed".to_string(),
            ]
        );
        assert_eq!(
            snapshot.integrity_problems(),
            vec![
                "payload:unverified".to_string(),
                "untracked-files:missing".to_string(),
            ]
        );
        let line = snapshot.completeness_line();
        assert!(line.contains("partial"), "{line}");
        assert!(line.contains("crash_recovered"), "{line}");
        assert!(line.contains("缺 2 项"), "{line}");
    }

    #[test]
    fn complete_snapshot_requires_verified_segments() {
        let snapshot = complete_snapshot(7);
        assert_eq!(snapshot.completeness(), Completeness::Complete);
        assert_eq!(
            snapshot.payload_digest(),
            Some(Digest::of_bytes(b"payload").as_string().as_str()),
            "已核验 payload 片段应报告摘要"
        );

        // 任何一段未核验就不能声称完整。
        let mut degraded = complete_snapshot(7);
        degraded.integrity[1] = SegmentIntegrity::unverified("workspace", "未复算");
        assert_eq!(degraded.completeness(), Completeness::Partial);
        assert_eq!(
            degraded.integrity_problems(),
            vec!["workspace:unverified".to_string()]
        );
    }

    #[test]
    fn unverified_payload_does_not_report_a_digest() {
        let snapshot = crash_snapshot(7);
        assert_eq!(snapshot.payload_digest(), None, "未核验不得报告摘要");

        let mut mismatch = complete_snapshot(7);
        mismatch.integrity[0] = SegmentIntegrity::digest_mismatch(
            PAYLOAD_SEGMENT,
            &Digest::of_bytes(b"expected"),
            "现场摘要与记录不符",
        );
        assert_eq!(mismatch.payload_digest(), None);
        assert!(mismatch.is_partial());
    }

    #[test]
    fn crash_sample_registers_and_survives_reopen() {
        let root = temp_root("register");
        let original = crash_snapshot(7);
        {
            let store = open_store(&root);
            let outcome = original.register(&store, NOW_MS).unwrap();
            assert!(outcome.is_new());
            assert_eq!(outcome.snapshot_key(), "company-1#7");
        }

        // 真实关闭重开：部分现场必须还在，且 missing 与完整性状态完整保留。
        let reopened = open_store(&root);
        let loaded = HandoffSnapshot::load(&reopened, "company-1", "company-1#7")
            .unwrap()
            .expect("崩溃样本关开后仍应可查");
        assert_eq!(loaded, original);
        assert!(loaded.is_partial());
        assert_eq!(loaded.source, CaptureSource::CrashRecovered);

        let all = reopened.list_handoff_snapshots("company-1").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].registered_at_ms, NOW_MS);
        assert_eq!(all[0].payload_digest, None, "未核验的 payload 不写摘要");

        let partial = HandoffSnapshot::list_partial(&reopened, "company-1").unwrap();
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0], original);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repeated_registration_is_idempotent_and_keeps_first_record() {
        let root = temp_root("idempotent");
        let store = open_store(&root);
        let snapshot = crash_snapshot(7);

        let first = snapshot.register(&store, NOW_MS).unwrap();
        assert!(first.is_new());
        let second = snapshot.register(&store, NOW_MS + 5_000).unwrap();
        assert_eq!(
            second,
            RegistrationOutcome::AlreadyRegistered {
                snapshot_key: "company-1#7".to_string()
            }
        );

        let rows = store.list_handoff_snapshots("company-1").unwrap();
        assert_eq!(rows.len(), 1, "重复登记不得新建第二份现场");
        assert_eq!(
            rows[0].registered_at_ms, NOW_MS,
            "重复登记不得改写第一次的登记时间"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn conflicting_content_under_same_key_is_rejected_not_overwritten() {
        let root = temp_root("conflict");
        let store = open_store(&root);
        let original = crash_snapshot(7);
        original.register(&store, NOW_MS).unwrap();

        let mut conflicting = crash_snapshot(7);
        conflicting.missing = vec![MissingPart::new("session-log", MissingReason::NotCaptured)];
        let err = conflicting.register(&store, NOW_MS + 1).unwrap_err();
        match err {
            SnapshotError::SnapshotKeyConflict { snapshot_key, .. } => {
                assert_eq!(snapshot_key, "company-1#7")
            }
            other => panic!("同键不同内容必须报冲突，实际 {other:?}"),
        }

        let loaded = HandoffSnapshot::load(&store, "company-1", "company-1#7")
            .unwrap()
            .unwrap();
        assert_eq!(loaded, original, "冲突不得覆盖已登记的现场");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_rejects_incomplete_identity_and_bad_inputs() {
        let mut missing_project = crash_snapshot(7);
        missing_project.header.project_id = DomainId::new("");
        assert!(matches!(
            missing_project.validate().unwrap_err(),
            SnapshotError::MissingIdentity("project_id")
        ));

        let mut no_seq = crash_snapshot(7);
        no_seq.binding.snapshot_seq = 0;
        no_seq.header.snapshot_seq = 0;
        assert!(matches!(
            no_seq.validate().unwrap_err(),
            SnapshotError::MissingSnapshotSeq
        ));

        let mut bad_time = crash_snapshot(7);
        bad_time.captured_at = "2026-11-20T10:00:00+08:00".to_string();
        assert!(matches!(
            bad_time.validate().unwrap_err(),
            SnapshotError::NonCanonicalCapturedAt(_)
        ));

        let mut bad_schema = crash_snapshot(7);
        bad_schema.header.schema = "bod.handoff/v2".to_string();
        assert!(matches!(
            bad_schema.validate().unwrap_err(),
            SnapshotError::UnsupportedSchema(_)
        ));

        let mut mismatch = crash_snapshot(7);
        mismatch.binding.work_package_revision = DomainId::new("wp-r2");
        assert!(matches!(
            mismatch.validate().unwrap_err(),
            SnapshotError::BindingMismatch { .. }
        ));
    }

    #[test]
    fn validate_rejects_unproven_verified_segments() {
        let mut no_digest = complete_snapshot(7);
        no_digest.integrity[0].digest = None;
        assert!(matches!(
            no_digest.validate().unwrap_err(),
            SnapshotError::PayloadDigestMissing(_)
        ));

        let mut unverified_with_digest = complete_snapshot(7);
        unverified_with_digest.integrity[0].status = IntegrityStatus::Unverified;
        assert!(matches!(
            unverified_with_digest.validate().unwrap_err(),
            SnapshotError::MalformedDigest(_)
        ));

        let mut duplicate = complete_snapshot(7);
        duplicate.integrity.push(SegmentIntegrity::verified(
            "workspace",
            &Digest::of_bytes(b"x"),
        ));
        assert!(matches!(
            duplicate.validate().unwrap_err(),
            SnapshotError::DuplicateSegment(segment) if segment == "workspace"
        ));

        let mut bad_digest = complete_snapshot(7);
        bad_digest.integrity[0].digest = Some("sha256:not-hex".to_string());
        assert!(matches!(
            bad_digest.validate().unwrap_err(),
            SnapshotError::MalformedDigest(_)
        ));
    }

    #[test]
    fn irrelevant_metadata_change_does_not_invalidate_work() {
        let recorded = binding(7);
        let baseline = MetadataStamp {
            title: "原始标题".to_string(),
            labels: vec!["handoff".to_string(), "p1".to_string()],
            comment_count: 3,
            updated_at_ms: NOW_MS - 60_000,
        };
        let observed = ObservedWork {
            binding: binding(7),
            metadata: MetadataStamp {
                title: "改过的标题".to_string(),
                labels: vec![
                    "p1".to_string(),
                    "handoff".to_string(),
                    "review".to_string(),
                ],
                comment_count: 12,
                updated_at_ms: NOW_MS,
            },
        };
        let report = evaluate_binding(&recorded, &baseline, &observed).unwrap();

        assert_eq!(report.verdict, BindingVerdict::Current);
        assert!(!report.verdict.invalidates_work());
        assert_eq!(
            report.metadata_changes,
            vec![
                MetadataChange::Title,
                MetadataChange::Labels,
                MetadataChange::CommentCount,
                MetadataChange::UpdatedAt,
            ],
            "元数据变化要如实列出，但不构成失效"
        );
        assert!(report.summary_line().contains("不影响接续"));

        // 顺序变化不算标签变化；只有集合变化才算。
        let reordered = MetadataStamp {
            labels: vec!["p1".to_string(), "handoff".to_string()],
            ..baseline.clone()
        };
        let same = evaluate_binding(
            &recorded,
            &baseline,
            &ObservedWork {
                binding: binding(7),
                metadata: reordered,
            },
        )
        .unwrap();
        assert!(same.metadata_changes.is_empty());
        assert_eq!(same.verdict, BindingVerdict::Current);
    }

    #[test]
    fn work_package_or_input_change_is_a_relevant_difference() {
        let recorded = binding(7);
        let baseline = MetadataStamp::default();

        let mut observed = ObservedWork {
            binding: binding(7),
            metadata: MetadataStamp::default(),
        };
        observed.binding.work_package_revision = DomainId::new("wp-r2");
        let report = evaluate_binding(&recorded, &baseline, &observed).unwrap();
        assert_eq!(
            report.verdict,
            BindingVerdict::Drifted {
                differences: vec![BindingDifference::WorkPackageChanged {
                    from: "wp-r1".to_string(),
                    to: "wp-r2".to_string(),
                }]
            }
        );
        assert!(report.verdict.invalidates_work());

        let mut input_changed = ObservedWork {
            binding: binding(7),
            metadata: MetadataStamp::default(),
        };
        input_changed.binding.input_revision = DomainId::new("input-r2");
        let report = evaluate_binding(&recorded, &baseline, &input_changed).unwrap();
        assert_eq!(report.verdict.differences().len(), 1);
        assert_eq!(
            report.verdict.differences()[0].dimension(),
            "input_revision"
        );
        assert!(report.summary_line().contains("绑定已变化"));

        let mut scene_changed = ObservedWork {
            binding: binding(7),
            metadata: MetadataStamp::default(),
        };
        scene_changed.binding.scene_revision = DomainId::new("scene-r9");
        assert_eq!(
            evaluate_binding(&recorded, &baseline, &scene_changed)
                .unwrap()
                .verdict
                .differences()[0]
                .dimension(),
            "scene_revision"
        );
    }

    #[test]
    fn newer_snapshot_seq_supersedes_without_invalidating_work() {
        let recorded = binding(7);
        let observed = ObservedWork {
            binding: binding(9),
            metadata: MetadataStamp::default(),
        };
        let report = evaluate_binding(&recorded, &MetadataStamp::default(), &observed).unwrap();
        assert_eq!(
            report.verdict,
            BindingVerdict::Superseded { from: 7, to: 9 }
        );
        assert!(!report.verdict.invalidates_work(), "更新快照不等于工作失效");
        assert!(report.summary_line().contains("已有更新快照"));
    }

    #[test]
    fn regressed_snapshot_seq_is_rejected_instead_of_guessed() {
        let recorded = binding(9);
        let observed = ObservedWork {
            binding: binding(7),
            metadata: MetadataStamp::default(),
        };
        assert_eq!(
            evaluate_binding(&recorded, &MetadataStamp::default(), &observed).unwrap_err(),
            BindingError::RegressedSnapshotSeq {
                recorded: 9,
                observed: 7
            }
        );
    }

    #[test]
    fn snapshot_level_round_trips_through_storage() {
        let row = HandoffSnapshotRow::from_snapshot(&complete_snapshot(11), NOW_MS).unwrap();
        assert_eq!(row.level, "L2");
        assert_eq!(row.completeness, "complete");
        assert_eq!(
            row.payload_digest,
            Some(Digest::of_bytes(b"payload").as_string())
        );
        let restored = row.to_snapshot().unwrap();
        assert_eq!(restored, complete_snapshot(11));
        assert_eq!(restored.header.level, HandoffLevel::L2);
    }

    #[test]
    fn row_with_tampered_completeness_is_rejected() {
        let mut row = HandoffSnapshotRow::from_snapshot(&crash_snapshot(7), NOW_MS).unwrap();
        row.completeness = "complete".to_string();
        assert!(matches!(
            row.to_snapshot().unwrap_err(),
            SnapshotError::CorruptRow(_)
        ));
    }
}
