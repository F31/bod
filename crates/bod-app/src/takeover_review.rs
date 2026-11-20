//! P2 任务 3.3：检查点增量读取与接管确认事务（`invariant_i_65`、G-42）。
//!
//! 权威语义：V2.9.6 §39.11 I-65「检查点带游标，接管补读增量，不依赖旧经理在线或叙述正确」；
//! G-42「无旧经理总结、部分资料缺失、快照后新增员工结果，能够查询缺口和增量」；
//! manager-transfer「初始只读确认 MUST NOT 产生管理权，随后命令仍逐次验证当前委任」。
//!
//! 边界（本模块不做的事）：
//! - 不建第二事实源。检查点只是「某次只读核对看到的事实指纹」：内容仍是 issues / runs /
//!   invocations / artifacts / artifact_revisions / acceptances / handoff_snapshots 的行，
//!   本模块不复制它们的字段，只存 `(kind, id, fingerprint)`。
//! - 确认不产生管理权。管理权只来自 3.1 的 Appointment；确认行没有任何通往
//!   `ManagementTicket` 的路径（[`confirmation_as_ticket`] 恒返回错误），撤销/换人后确认只变成
//!   `Superseded`，不会把旧 epoch 变成可用凭据。
//! - 不代替 3.4/3.5：不改历史接受与费用，不做专项子树作用域（显式拒绝并指向 3.5）。

use crate::domain::artifact::{AcceptanceDecision, Artifact};
use crate::manager_transfer::{
    AppointmentStatus, ManagementAppointment, ManagementScope, ManagementTicket,
};
use crate::store::{SqliteStore, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
/// 检查点 schema 标识（写入 `items_json` 的解释口径，不做跨版本自动迁移）。
pub const CHECKPOINT_SCHEMA: &str = "bod.handover-checkpoint/v1";

/// 单个作用域的指纹上限：超过即报错，不截断（截断会让「没有变化」变成假结论）。
pub const MAX_FACTS: usize = 8192;

// ---------------------------------------------------------------------------
// 事实指纹
// ---------------------------------------------------------------------------

/// 参与接管核对的事实种类。顺序固定，用于排序与游标文本的稳定前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Issue,
    Run,
    Invocation,
    Artifact,
    Revision,
    Acceptance,
    Snapshot,
}

impl FactKind {
    pub const ALL: [Self; 7] = [
        Self::Issue,
        Self::Run,
        Self::Invocation,
        Self::Artifact,
        Self::Revision,
        Self::Acceptance,
        Self::Snapshot,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Run => "run",
            Self::Invocation => "invocation",
            Self::Artifact => "artifact",
            Self::Revision => "revision",
            Self::Acceptance => "acceptance",
            Self::Snapshot => "snapshot",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "issue" => Some(Self::Issue),
            "run" => Some(Self::Run),
            "invocation" => Some(Self::Invocation),
            "artifact" => Some(Self::Artifact),
            "revision" => Some(Self::Revision),
            "acceptance" => Some(Self::Acceptance),
            "snapshot" => Some(Self::Snapshot),
            _ => None,
        }
    }
}

impl fmt::Display for FactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条事实的指纹：只含稳定判别值，不含标题/内容/评论。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FactState {
    pub kind: FactKind,
    pub id: String,
    pub fingerprint: String,
}

impl FactState {
    pub fn new(kind: FactKind, id: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
            fingerprint: fingerprint.into(),
        }
    }

    pub fn subject(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }
}

/// 缺口种类：全部是「资料是否够接管」的判定，不表示业务失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    /// 在跑的执行所属 Issue 没有任何交接快照：接管不能依赖旧经理总结。
    MissingSnapshot,
    /// 有快照但声明了 missing：部分资料缺失。
    PartialSnapshot,
    /// 执行处于未知/取消中：先对账或确认停止，不能靠补资料绕过。
    UnreconciledExecution,
    /// 验收待人工决定：成果是否被接受尚未闭合。
    OpenAcceptance,
}

impl GapKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingSnapshot => "missing_snapshot",
            Self::PartialSnapshot => "partial_snapshot",
            Self::UnreconciledExecution => "unreconciled_execution",
            Self::OpenAcceptance => "open_acceptance",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "missing_snapshot" => Some(Self::MissingSnapshot),
            "partial_snapshot" => Some(Self::PartialSnapshot),
            "unreconciled_execution" => Some(Self::UnreconciledExecution),
            "open_acceptance" => Some(Self::OpenAcceptance),
            _ => None,
        }
    }
}

impl fmt::Display for GapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条可查询的缺口。`detail` 只作说明，不参与游标。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Gap {
    pub kind: GapKind,
    pub subject: String,
    pub detail: String,
}

impl Gap {
    pub fn new(kind: GapKind, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            subject: subject.into(),
            detail: detail.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// 现场快照
// ---------------------------------------------------------------------------

/// 一次只读核对看到的事实与缺口。**不写库**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSnapshot {
    pub cursor: String,
    pub items: Vec<FactState>,
    pub gaps: Vec<Gap>,
}

impl FieldSnapshot {
    /// 只读读取项目现场。不依赖旧经理在线、不读其叙述、不写任何一行。
    pub fn capture(
        store: &SqliteStore,
        company_id: &str,
        project_id: &str,
    ) -> Result<Self, ReviewError> {
        if company_id.trim().is_empty() {
            return Err(ReviewError::EmptyField("company_id"));
        }
        if project_id.trim().is_empty() {
            return Err(ReviewError::EmptyField("project_id"));
        }

        let issues = store.list_issues_for_project(company_id, project_id)?;
        let runs = store.list_project_runs(company_id, project_id)?;
        let invocations = store.list_project_invocations(company_id, project_id)?;
        let artifacts = store.list_artifacts(company_id, project_id)?;
        let revisions = store.list_project_revision_producers(company_id, project_id)?;
        let acceptances = store.list_acceptances_for_company(company_id)?;
        let snapshots = store.list_handoff_snapshots(company_id)?;

        let mut items = Vec::new();
        for issue in &issues {
            let owner = issue
                .spec
                .assignment
                .as_ref()
                .map(|assignment| assignment.owner.id.as_str().to_string())
                .unwrap_or_else(|| "-".to_string());
            items.push(FactState::new(
                FactKind::Issue,
                issue.meta.uid.as_str(),
                format!(
                    "phase={}|observed={}|revision={}|owner={owner}",
                    phase_label(issue.status.phase),
                    issue.status.observed_generation,
                    issue.meta.resource_version.0
                ),
            ));
        }
        for (run_id, issue_uid, desired_state) in &runs {
            items.push(FactState::new(
                FactKind::Run,
                run_id,
                format!("issue={issue_uid}|desired={desired_state}"),
            ));
        }
        for row in &invocations {
            items.push(FactState::new(
                FactKind::Invocation,
                row.invocation_id.as_str(),
                format!(
                    "run={}|state={}|observed={}|fencing={}",
                    row.run_id,
                    row.state,
                    row.last_observed_at_ms
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    row.fencing_token
                ),
            ));
        }
        for artifact in &artifacts {
            items.push(FactState::new(
                FactKind::Artifact,
                artifact.id.as_str(),
                artifact_fingerprint(artifact),
            ));
        }
        for (revision_id, artifact_id, produced_by) in &revisions {
            items.push(FactState::new(
                FactKind::Revision,
                revision_id,
                format!("artifact={artifact_id}|produced_by={produced_by}"),
            ));
        }

        let project_artifacts: BTreeSet<&str> =
            artifacts.iter().map(|item| item.id.as_str()).collect();
        for acceptance in &acceptances {
            if !project_artifacts.contains(acceptance.artifact_id.as_str()) {
                continue;
            }
            items.push(FactState::new(
                FactKind::Acceptance,
                acceptance.acceptance_id.as_str(),
                format!(
                    "artifact={}|revision={}|decision={}|superseded={}",
                    acceptance.artifact_id.as_str(),
                    acceptance.binding.artifact_revision.as_str(),
                    acceptance.decision.as_str(),
                    acceptance
                        .superseded_by
                        .as_ref()
                        .map(|value| value.as_str())
                        .unwrap_or("-")
                ),
            ));
        }
        for snapshot in snapshots.iter().filter(|row| row.project_id == project_id) {
            items.push(FactState::new(
                FactKind::Snapshot,
                snapshot.snapshot_key.as_str(),
                format!(
                    "issue={}|level={}|completeness={}|payload={}|missing={}",
                    snapshot.issue_id,
                    snapshot.level,
                    snapshot.completeness,
                    snapshot.payload_digest.as_deref().unwrap_or("-"),
                    snapshot.missing_json
                ),
            ));
        }

        items.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.id.cmp(&right.id))
        });
        if items.len() > MAX_FACTS {
            return Err(ReviewError::TooManyFacts {
                project_id: project_id.to_string(),
                count: items.len(),
                limit: MAX_FACTS,
            });
        }
        for item in &items {
            if item.id.contains(['\t', '\n']) || item.fingerprint.contains(['\t', '\n']) {
                // 指纹文本是游标的输入，含分隔符会让两个不同现场算出同一游标。
                return Err(ReviewError::CorruptFingerprint {
                    subject: item.subject(),
                });
            }
        }

        let gaps = collect_gaps(project_id, &runs, &invocations, &acceptances, &snapshots);
        let cursor = compute_cursor(&items);
        Ok(Self {
            cursor,
            items,
            gaps,
        })
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    pub fn count_of(&self, kind: FactKind) -> usize {
        self.items.iter().filter(|item| item.kind == kind).count()
    }

    pub fn gaps_of(&self, kind: GapKind) -> usize {
        self.gaps.iter().filter(|gap| gap.kind == kind).count()
    }

    pub fn fingerprint_of(&self, kind: FactKind, id: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|item| item.kind == kind && item.id == id)
            .map(|item| item.fingerprint.as_str())
    }
}

fn artifact_fingerprint(artifact: &Artifact) -> String {
    format!(
        "kind={}|owner={}|visibility={}|accepted={}",
        artifact.kind.as_str(),
        artifact.owner.as_str(),
        artifact.visibility.as_str(),
        artifact
            .accepted_revision
            .as_ref()
            .map(|value| value.as_str())
            .unwrap_or("-")
    )
}

fn phase_label(phase: crate::domain::Phase) -> &'static str {
    use crate::domain::Phase;
    match phase {
        Phase::Pending => "Pending",
        Phase::Running => "Running",
        Phase::Blocked => "Blocked",
        Phase::Succeeded => "Succeeded",
        Phase::Cancelled => "Cancelled",
    }
}

/// 在跑/未定的执行状态（与 3.2 的分类同口径，只用于判断「是否需要资料」）。
fn is_in_flight(state: &str) -> bool {
    matches!(
        state,
        "Accepted" | "Starting" | "Running" | "NeedsInput" | "NeedsAuth" | "Suspended"
    )
}

fn collect_gaps(
    project_id: &str,
    runs: &[(String, String, String)],
    invocations: &[InvocationFactRow],
    acceptances: &[crate::domain::artifact::AcceptanceRecord],
    snapshots: &[crate::handoff_snapshot::HandoffSnapshotRow],
) -> Vec<Gap> {
    let mut gaps = Vec::new();

    let snapshot_issues: BTreeSet<&str> = snapshots
        .iter()
        .filter(|row| row.project_id == project_id)
        .map(|row| row.issue_id.as_str())
        .collect();
    for (run_id, issue_uid, _) in runs {
        let in_flight = invocations
            .iter()
            .any(|row| &row.run_id == run_id && is_in_flight(&row.state));
        if in_flight && !snapshot_issues.contains(issue_uid.as_str()) {
            gaps.push(Gap::new(
                GapKind::MissingSnapshot,
                run_id,
                format!("Issue {issue_uid} 没有任何交接快照：接管不能依赖旧经理在线或叙述正确"),
            ));
        }
    }

    for snapshot in snapshots
        .iter()
        .filter(|row| row.project_id == project_id && row.completeness == "partial")
    {
        gaps.push(Gap::new(
            GapKind::PartialSnapshot,
            snapshot.snapshot_key.as_str(),
            format!("missing={}", snapshot.missing_json),
        ));
    }

    for row in invocations {
        if row.state == "Unknown" || row.state == "Cancelling" {
            gaps.push(Gap::new(
                GapKind::UnreconciledExecution,
                row.invocation_id.as_str(),
                format!(
                    "run={} state={}：先对账或确认停止，不能靠补资料绕过",
                    row.run_id, row.state
                ),
            ));
        }
    }

    for acceptance in acceptances {
        if acceptance.decision == AcceptanceDecision::RequiresHuman {
            gaps.push(Gap::new(
                GapKind::OpenAcceptance,
                acceptance.acceptance_id.as_str(),
                format!(
                    "artifact={} revision={} 待人工决定：成果是否被接受尚未闭合",
                    acceptance.artifact_id.as_str(),
                    acceptance.binding.artifact_revision.as_str()
                ),
            ));
        }
    }

    gaps.sort();
    gaps.dedup();
    gaps
}

/// 游标 = 对排序后的 `kind\tid\tfingerprint\n` 文本求 sha256。
///
/// 只覆盖事实指纹，不覆盖缺口：缺口是「资料够不够」的判定，会在增量读取时按当前现场重新给出，
/// 不让「补了一份快照」把同一份事实清单算成不同现场。
fn compute_cursor(items: &[FactState]) -> String {
    let mut text = String::new();
    for item in items {
        text.push_str(item.kind.as_str());
        text.push('\t');
        text.push_str(&item.id);
        text.push('\t');
        text.push_str(&item.fingerprint);
        text.push('\n');
    }
    format!(
        "sha256:{}",
        crate::domain::artifact::sha256_hex(text.as_bytes())
    )
}

// ---------------------------------------------------------------------------
// 检查点
// ---------------------------------------------------------------------------

/// 已登记的检查点：一份现场游标 + 当时的指纹清单与缺口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoverCheckpoint {
    pub checkpoint_id: String,
    pub company_id: String,
    pub scope_key: String,
    pub project_id: String,
    pub cursor: String,
    pub item_count: usize,
    pub items: Vec<FactState>,
    pub gaps: Vec<Gap>,
    pub captured_by: String,
    pub captured_at: String,
}

impl HandoverCheckpoint {
    /// 捕获并登记检查点。同 scope 同游标重复捕获落回同一行（不产生第二份）。
    pub fn capture(
        store: &SqliteStore,
        company_id: &str,
        scope: &ManagementScope,
        captured_by: &str,
        captured_at: &str,
        recorded_at_ms: i64,
    ) -> Result<Self, ReviewError> {
        if captured_by.trim().is_empty() {
            return Err(ReviewError::EmptyField("captured_by"));
        }
        if !crate::canonical_json::is_canonical_timestamp(captured_at) {
            return Err(ReviewError::NonCanonicalTimestamp {
                field: "captured_at",
            });
        }
        let project_id = project_of(scope)?;
        let snapshot = FieldSnapshot::capture(store, company_id, &project_id)?;
        let scope_key = scope.scope_key();
        let checkpoint_id = checkpoint_id_for(&scope_key, &snapshot.cursor);

        let row = CheckpointRow {
            company_id: company_id.to_string(),
            checkpoint_id: checkpoint_id.clone(),
            scope_key: scope_key.clone(),
            project_id,
            cursor: snapshot.cursor.clone(),
            item_count: snapshot.items.len() as i64,
            items_json: serde_json::to_string(&snapshot.items)
                .map_err(|error| ReviewError::Serialization(error.to_string()))?,
            gaps_json: serde_json::to_string(&snapshot.gaps)
                .map_err(|error| ReviewError::Serialization(error.to_string()))?,
            captured_by: captured_by.to_string(),
            captured_at: captured_at.to_string(),
            recorded_at_ms,
        };
        store.insert_handover_checkpoint(&row)?;
        // 并发同现场：以唯一索引里的那一行为准，读回权威行而不是复用本次内存值。
        let stored = store
            .find_handover_checkpoint_by_cursor(company_id, &scope_key, &snapshot.cursor)?
            .ok_or_else(|| ReviewError::UnknownCheckpoint {
                checkpoint_id: checkpoint_id.clone(),
            })?;
        Self::from_row(&stored)
    }

    pub fn load(
        store: &SqliteStore,
        company_id: &str,
        checkpoint_id: &str,
    ) -> Result<Option<Self>, ReviewError> {
        match store.get_handover_checkpoint(company_id, checkpoint_id)? {
            Some(row) => Ok(Some(Self::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// 某 scope 最近登记的检查点：只用于回显「上次核对的是哪一份现场」。
    pub fn latest(
        store: &SqliteStore,
        company_id: &str,
        scope: &ManagementScope,
    ) -> Result<Option<Self>, ReviewError> {
        match store.latest_handover_checkpoint(company_id, &scope.scope_key())? {
            Some(row) => Ok(Some(Self::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// 相对本检查点读取增量：新增 / 变化 / 消失，并附当前缺口。
    ///
    /// 只读：读的是当前现场，不写库、不推进游标。调用方要「确认」必须另走
    /// [`TakeoverConfirmation::confirm`]，两者不能混成一步。
    pub fn delta(&self, store: &SqliteStore, company_id: &str) -> Result<FieldDelta, ReviewError> {
        let current = FieldSnapshot::capture(store, company_id, &self.project_id)?;
        let before: BTreeMap<(FactKind, String), String> = self
            .items
            .iter()
            .map(|item| ((item.kind, item.id.clone()), item.fingerprint.clone()))
            .collect();
        let after: BTreeMap<(FactKind, String), String> = current
            .items
            .iter()
            .map(|item| ((item.kind, item.id.clone()), item.fingerprint.clone()))
            .collect();

        let mut added = Vec::new();
        let mut changed = Vec::new();
        for (key, fingerprint) in &after {
            match before.get(key) {
                None => added.push(FactState::new(key.0, key.1.clone(), fingerprint.clone())),
                Some(previous) if previous != fingerprint => changed.push(FactChange {
                    kind: key.0,
                    id: key.1.clone(),
                    before: previous.clone(),
                    after: fingerprint.clone(),
                }),
                Some(_) => {}
            }
        }
        let mut vanished = Vec::new();
        for (key, fingerprint) in &before {
            if !after.contains_key(key) {
                vanished.push(FactState::new(key.0, key.1.clone(), fingerprint.clone()));
            }
        }
        added.sort();
        changed.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.id.cmp(&right.id))
        });
        vanished.sort();

        Ok(FieldDelta {
            from_cursor: self.cursor.clone(),
            to_cursor: current.cursor,
            added,
            changed,
            vanished,
            gaps: current.gaps,
        })
    }

    fn from_row(row: &CheckpointRow) -> Result<Self, ReviewError> {
        let items: Vec<FactState> = serde_json::from_str(&row.items_json).map_err(|error| {
            ReviewError::CorruptCheckpoint {
                checkpoint_id: row.checkpoint_id.clone(),
                detail: format!("items_json 无法解析：{error}"),
            }
        })?;
        let gaps: Vec<Gap> = serde_json::from_str(&row.gaps_json).map_err(|error| {
            ReviewError::CorruptCheckpoint {
                checkpoint_id: row.checkpoint_id.clone(),
                detail: format!("gaps_json 无法解析：{error}"),
            }
        })?;
        // 游标必须与清单一致：否则「增量」会以一份来历不明的现场为基准。
        let recomputed = compute_cursor(&items);
        if recomputed != row.cursor {
            return Err(ReviewError::CorruptCheckpoint {
                checkpoint_id: row.checkpoint_id.clone(),
                detail: format!("游标 {} 与清单重算的 {recomputed} 不一致", row.cursor),
            });
        }
        if items.len() as i64 != row.item_count {
            return Err(ReviewError::CorruptCheckpoint {
                checkpoint_id: row.checkpoint_id.clone(),
                detail: format!(
                    "item_count {} 与清单 {} 条不一致",
                    row.item_count,
                    items.len()
                ),
            });
        }
        Ok(Self {
            checkpoint_id: row.checkpoint_id.clone(),
            company_id: row.company_id.clone(),
            scope_key: row.scope_key.clone(),
            project_id: row.project_id.clone(),
            cursor: row.cursor.clone(),
            item_count: items.len(),
            items,
            gaps,
            captured_by: row.captured_by.clone(),
            captured_at: row.captured_at.clone(),
        })
    }
}

fn checkpoint_id_for(scope_key: &str, cursor: &str) -> String {
    let hex = cursor.strip_prefix("sha256:").unwrap_or(cursor);
    let short: String = hex.chars().take(16).collect();
    format!("checkpoint:{scope_key}#{short}")
}

fn project_of(scope: &ManagementScope) -> Result<String, ReviewError> {
    match scope {
        ManagementScope::Project { project_id } => {
            if project_id.trim().is_empty() {
                return Err(ReviewError::EmptyField("scope.project_id"));
            }
            Ok(project_id.clone())
        }
        ManagementScope::IssueSubtree { .. } => Err(ReviewError::UnsupportedScope {
            scope_key: scope.scope_key(),
            detail: "专项子树的检查点与增量读取属任务 3.5；本切片只做项目作用域，不近似代替"
                .to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// 增量
// ---------------------------------------------------------------------------

/// 同一事实的指纹变化（如 Issue 阶段推进、invocation 状态更新）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactChange {
    pub kind: FactKind,
    pub id: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeltaCounts {
    pub added: usize,
    pub changed: usize,
    pub vanished: usize,
    pub gaps: usize,
}

/// 相对某个检查点的增量。`gaps` 是当前现场的缺口，不是增量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDelta {
    pub from_cursor: String,
    pub to_cursor: String,
    pub added: Vec<FactState>,
    pub changed: Vec<FactChange>,
    pub vanished: Vec<FactState>,
    pub gaps: Vec<Gap>,
}

impl FieldDelta {
    /// 事实有没有变（缺口不算增量：缺口单独可查）。
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.vanished.is_empty()
    }

    pub fn counts(&self) -> DeltaCounts {
        DeltaCounts {
            added: self.added.len(),
            changed: self.changed.len(),
            vanished: self.vanished.len(),
            gaps: self.gaps.len(),
        }
    }

    pub fn added_of(&self, kind: FactKind) -> Vec<&FactState> {
        self.added.iter().filter(|item| item.kind == kind).collect()
    }

    pub fn gaps_of(&self, kind: GapKind) -> Vec<&Gap> {
        self.gaps.iter().filter(|gap| gap.kind == kind).collect()
    }

    /// 给界面/日志的一句话说明；不把「有缺口」说成「不能接管」。
    pub fn describe(&self) -> String {
        let counts = self.counts();
        format!(
            "相对检查点新增 {} 项、变化 {} 项、消失 {} 项；当前缺口 {} 项",
            counts.added, counts.changed, counts.vanished, counts.gaps
        )
    }
}

// ---------------------------------------------------------------------------
// 接管确认
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    /// 资料足够，按当前现场接管。
    Confirmed,
    /// 资料不足，明确拒绝在此资料下接管；缺口照实记录。
    Blocked,
}

impl ReviewDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Blocked => "blocked",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "confirmed" => Some(Self::Confirmed),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

impl fmt::Display for ReviewDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 提交接管确认的请求。字段分步绑定，避免 12 个位置参数写错顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmRequest {
    pub company_id: String,
    pub scope: ManagementScope,
    pub confirmation_id: String,
    pub confirmed_by: String,
    pub confirmed_at: String,
    pub recorded_at_ms: i64,
    pub checkpoint_id: String,
    pub expected_cursor: String,
    pub appointment_id: String,
    pub expected_epoch: u64,
    pub decision: ReviewDecision,
    pub note: String,
}

impl ConfirmRequest {
    pub fn new(
        company_id: impl Into<String>,
        scope: ManagementScope,
        confirmation_id: impl Into<String>,
        confirmed_by: impl Into<String>,
        confirmed_at: impl Into<String>,
        recorded_at_ms: i64,
    ) -> Self {
        Self {
            company_id: company_id.into(),
            scope,
            confirmation_id: confirmation_id.into(),
            confirmed_by: confirmed_by.into(),
            confirmed_at: confirmed_at.into(),
            recorded_at_ms,
            checkpoint_id: String::new(),
            expected_cursor: String::new(),
            appointment_id: String::new(),
            expected_epoch: 0,
            decision: ReviewDecision::Confirmed,
            note: String::new(),
        }
    }

    /// 绑定「确认者复核过的那份检查点」：游标不是可选项，避免确认到另一份现场。
    pub fn against(mut self, checkpoint: &HandoverCheckpoint) -> Self {
        self.checkpoint_id = checkpoint.checkpoint_id.clone();
        self.expected_cursor = checkpoint.cursor.clone();
        self
    }

    /// 绑定当前委任与 epoch。
    pub fn by(mut self, appointment: &ManagementAppointment) -> Self {
        self.appointment_id = appointment.appointment_id.clone();
        self.expected_epoch = appointment.authority_epoch;
        self
    }

    pub fn decision(mut self, decision: ReviewDecision, note: impl Into<String>) -> Self {
        self.decision = decision;
        self.note = note.into();
        self
    }

    fn validate(&self) -> Result<(), ReviewError> {
        for (field, value) in [
            ("company_id", self.company_id.as_str()),
            ("confirmation_id", self.confirmation_id.as_str()),
            ("confirmed_by", self.confirmed_by.as_str()),
            ("checkpoint_id", self.checkpoint_id.as_str()),
            ("expected_cursor", self.expected_cursor.as_str()),
            ("appointment_id", self.appointment_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ReviewError::EmptyField(field));
            }
        }
        if !crate::canonical_json::is_canonical_timestamp(&self.confirmed_at) {
            return Err(ReviewError::NonCanonicalTimestamp {
                field: "confirmed_at",
            });
        }
        if self.expected_epoch == 0 {
            return Err(ReviewError::EmptyField("expected_epoch"));
        }
        if self.decision == ReviewDecision::Blocked && self.note.trim().is_empty() {
            return Err(ReviewError::EmptyField("note"));
        }
        Ok(())
    }
}

/// 一次接管确认：绑定 scope / 检查点游标 / 委任 / epoch / 决定 / 当时的缺口与增量。
///
/// **确认不是凭据**：本类型没有任何通往 `ManagementTicket` 的转换，也没有 appointment 写入路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeoverConfirmation {
    pub confirmation_id: String,
    pub company_id: String,
    pub scope_key: String,
    pub checkpoint_id: String,
    pub checkpoint_cursor: String,
    pub reviewed_cursor: String,
    pub appointment_id: String,
    pub authority_epoch: u64,
    pub decision: ReviewDecision,
    pub confirmed_by: String,
    pub note: String,
    pub open_items: Vec<Gap>,
    pub delta: DeltaCounts,
    pub confirmed_at: String,
}

impl TakeoverConfirmation {
    /// 提交接管确认。
    ///
    /// 事务边界：在同一事务内读作用域头、委任行与检查点，再插确认行。所以「确认时 epoch 是多少」
    /// 与「确认行记录了多少」不会错位；epoch 前进后本行只变成 Superseded，不会回写委任状态。
    pub fn confirm(store: &mut SqliteStore, request: ConfirmRequest) -> Result<Self, ReviewError> {
        request.validate()?;
        let project_id = project_of(&request.scope)?;
        let scope_key = request.scope.scope_key();
        let company_id = request.company_id.clone();

        // 增量在事务外读：它是只读投影，且记录的是「确认者看到的现场」，不需要与插入互斥。
        let checkpoint = HandoverCheckpoint::load(store, &company_id, &request.checkpoint_id)?
            .ok_or_else(|| ReviewError::UnknownCheckpoint {
                checkpoint_id: request.checkpoint_id.clone(),
            })?;
        if checkpoint.scope_key != scope_key {
            return Err(ReviewError::CheckpointScopeMismatch {
                checkpoint_id: checkpoint.checkpoint_id.clone(),
                expected: scope_key.clone(),
                actual: checkpoint.scope_key.clone(),
            });
        }
        if checkpoint.cursor != request.expected_cursor {
            return Err(ReviewError::CursorMismatch {
                expected: request.expected_cursor.clone(),
                actual: checkpoint.cursor.clone(),
            });
        }
        let delta = checkpoint.delta(store, &company_id)?;

        let uow = store.transaction()?;
        let head = uow
            .get_scope_head(&company_id, &scope_key)?
            .ok_or_else(|| ReviewError::NoActiveAppointment {
                scope_key: scope_key.clone(),
            })?;
        let appointment = uow
            .get_appointment(&company_id, &head.current_appointment_id)?
            .ok_or_else(|| ReviewError::UnknownAppointment {
                appointment_id: head.current_appointment_id.clone(),
            })?
            .to_appointment()?;
        let stored_checkpoint = uow
            .get_handover_checkpoint(&company_id, &request.checkpoint_id)?
            .ok_or_else(|| ReviewError::UnknownCheckpoint {
                checkpoint_id: request.checkpoint_id.clone(),
            })?;
        if stored_checkpoint.project_id != project_id {
            return Err(ReviewError::CheckpointScopeMismatch {
                checkpoint_id: stored_checkpoint.checkpoint_id.clone(),
                expected: project_id,
                actual: stored_checkpoint.project_id.clone(),
            });
        }

        if appointment.appointment_id != request.appointment_id {
            return Err(ReviewError::StaleAppointment {
                expected_appointment_id: request.appointment_id.clone(),
                current_appointment_id: appointment.appointment_id.clone(),
                current_epoch: appointment.authority_epoch,
            });
        }
        if appointment.effective_status(&request.confirmed_at) != AppointmentStatus::Active {
            return Err(ReviewError::AppointmentInactive {
                appointment_id: appointment.appointment_id.clone(),
                status: appointment
                    .effective_status(&request.confirmed_at)
                    .as_str()
                    .to_string(),
            });
        }
        if appointment.authority_epoch != request.expected_epoch
            || head.authority_epoch != request.expected_epoch
        {
            return Err(ReviewError::StaleAppointment {
                expected_appointment_id: request.appointment_id.clone(),
                current_appointment_id: appointment.appointment_id.clone(),
                current_epoch: head.authority_epoch.max(appointment.authority_epoch),
            });
        }
        if appointment.manager_agent_id != request.confirmed_by {
            return Err(ReviewError::ConfirmerIsNotManager {
                confirmed_by: request.confirmed_by.clone(),
                manager_agent_id: appointment.manager_agent_id.clone(),
            });
        }

        let row = ConfirmationRow {
            company_id: company_id.clone(),
            confirmation_id: request.confirmation_id.clone(),
            scope_key: scope_key.clone(),
            checkpoint_id: checkpoint.checkpoint_id.clone(),
            checkpoint_cursor: checkpoint.cursor.clone(),
            reviewed_cursor: delta.to_cursor.clone(),
            appointment_id: appointment.appointment_id.clone(),
            authority_epoch: appointment.authority_epoch,
            decision: request.decision.as_str().to_string(),
            confirmed_by: request.confirmed_by.clone(),
            note: request.note.clone(),
            open_items_json: serde_json::to_string(&delta.gaps)
                .map_err(|error| ReviewError::Serialization(error.to_string()))?,
            delta_json: serde_json::to_string(&delta.counts())
                .map_err(|error| ReviewError::Serialization(error.to_string()))?,
            confirmed_at: request.confirmed_at.clone(),
            recorded_at_ms: request.recorded_at_ms,
        };
        let inserted = uow.insert_takeover_confirmation(&row)?;
        uow.commit()?;

        if !inserted {
            // 同键重试：内容一致返回原行，不一致拒绝（与 3.1 的命令键幂等同口径）。
            let existing = store
                .get_takeover_confirmation(&company_id, &request.confirmation_id)?
                .ok_or_else(|| ReviewError::ConfirmationKeyReuse {
                    confirmation_id: request.confirmation_id.clone(),
                })?;
            if existing != row {
                return Err(ReviewError::ConfirmationKeyReuse {
                    confirmation_id: request.confirmation_id.clone(),
                });
            }
            return Self::from_row(&existing);
        }
        Self::from_row(&row)
    }

    pub fn load(
        store: &SqliteStore,
        company_id: &str,
        confirmation_id: &str,
    ) -> Result<Option<Self>, ReviewError> {
        match store.get_takeover_confirmation(company_id, confirmation_id)? {
            Some(row) => Ok(Some(Self::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// 某 scope 的确认历史（epoch / 时间升序）。最后一条是「当前有效的确认」。
    pub fn history(
        store: &SqliteStore,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<Self>, ReviewError> {
        store
            .list_takeover_confirmations(company_id, scope_key)?
            .iter()
            .map(Self::from_row)
            .collect()
    }

    pub fn requires_review(&self) -> bool {
        self.decision != ReviewDecision::Confirmed
    }

    /// 按当前作用域头判断这条确认还算不算「当前有效的确认」。
    pub fn review_status(
        &self,
        store: &SqliteStore,
        company_id: &str,
    ) -> Result<ReviewState, ReviewError> {
        let head = store.get_scope_head(company_id, &self.scope_key)?;
        let Some(head) = head else {
            return Ok(ReviewState::Superseded {
                confirmation_id: self.confirmation_id.clone(),
                authority_epoch: self.authority_epoch,
                head_epoch: 0,
            });
        };
        if self.authority_epoch < head.authority_epoch {
            return Ok(ReviewState::Superseded {
                confirmation_id: self.confirmation_id.clone(),
                authority_epoch: self.authority_epoch,
                head_epoch: head.authority_epoch,
            });
        }
        if self.authority_epoch > head.authority_epoch {
            return Err(ReviewError::CorruptConfirmation {
                confirmation_id: self.confirmation_id.clone(),
                detail: format!(
                    "确认 epoch {} 领先作用域头 epoch {}",
                    self.authority_epoch, head.authority_epoch
                ),
            });
        }
        Ok(match self.decision {
            ReviewDecision::Confirmed => ReviewState::Current {
                confirmation_id: self.confirmation_id.clone(),
                authority_epoch: self.authority_epoch,
                checkpoint_id: self.checkpoint_id.clone(),
                reviewed_cursor: self.reviewed_cursor.clone(),
            },
            ReviewDecision::Blocked => ReviewState::Blocked {
                confirmation_id: self.confirmation_id.clone(),
                authority_epoch: self.authority_epoch,
                note: self.note.clone(),
            },
        })
    }

    fn from_row(row: &ConfirmationRow) -> Result<Self, ReviewError> {
        let decision = ReviewDecision::parse(&row.decision).ok_or_else(|| {
            ReviewError::CorruptConfirmation {
                confirmation_id: row.confirmation_id.clone(),
                detail: format!("未知决定 {}", row.decision),
            }
        })?;
        let open_items: Vec<Gap> = serde_json::from_str(&row.open_items_json).map_err(|error| {
            ReviewError::CorruptConfirmation {
                confirmation_id: row.confirmation_id.clone(),
                detail: format!("open_items_json 无法解析：{error}"),
            }
        })?;
        let delta: DeltaCounts = serde_json::from_str(&row.delta_json).map_err(|error| {
            ReviewError::CorruptConfirmation {
                confirmation_id: row.confirmation_id.clone(),
                detail: format!("delta_json 无法解析：{error}"),
            }
        })?;
        Ok(Self {
            confirmation_id: row.confirmation_id.clone(),
            company_id: row.company_id.clone(),
            scope_key: row.scope_key.clone(),
            checkpoint_id: row.checkpoint_id.clone(),
            checkpoint_cursor: row.checkpoint_cursor.clone(),
            reviewed_cursor: row.reviewed_cursor.clone(),
            appointment_id: row.appointment_id.clone(),
            authority_epoch: row.authority_epoch,
            decision,
            confirmed_by: row.confirmed_by.clone(),
            note: row.note.clone(),
            open_items,
            delta,
            confirmed_at: row.confirmed_at.clone(),
        })
    }
}

/// 当前作用域的接管核对状态。`Current` 也不等于「有管理权」——那只来自委任。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewState {
    NeverConfirmed,
    Current {
        confirmation_id: String,
        authority_epoch: u64,
        checkpoint_id: String,
        reviewed_cursor: String,
    },
    Blocked {
        confirmation_id: String,
        authority_epoch: u64,
        note: String,
    },
    Superseded {
        confirmation_id: String,
        authority_epoch: u64,
        head_epoch: u64,
    },
}

impl ReviewState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NeverConfirmed => "never_confirmed",
            Self::Current { .. } => "current",
            Self::Blocked { .. } => "blocked",
            Self::Superseded { .. } => "superseded",
        }
    }

    /// 当前 epoch 下是否还需要一次确认（撤销/换人后必然为真）。
    pub fn requires_review(&self) -> bool {
        !matches!(self, Self::Current { .. })
    }
}

impl fmt::Display for ReviewState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 读取某作用域当前的接管核对状态：以「最新一条确认」与当前头 epoch 比对。
pub fn review_status(
    store: &SqliteStore,
    company_id: &str,
    scope: &ManagementScope,
) -> Result<ReviewState, ReviewError> {
    let scope_key = scope.scope_key();
    let history = store.list_takeover_confirmations(company_id, &scope_key)?;
    let Some(latest) = history.last() else {
        return Ok(ReviewState::NeverConfirmed);
    };
    TakeoverConfirmation::from_row(latest)?.review_status(store, company_id)
}

/// 类型事实：接管确认不是管理票证，也没有「确认即授权」的转换。
///
/// 与 3.2 的 `management_ticket_as_execution` 同口径：双向都不存在，所以「只读确认产生管理权」
/// 不是靠运行时开关关掉的，而是没有代码路径。
pub fn confirmation_as_ticket(
    confirmation: &TakeoverConfirmation,
) -> Result<ManagementTicket, ReviewError> {
    Err(ReviewError::ConfirmationIsNotAuthority {
        confirmation_id: confirmation.confirmation_id.clone(),
        authority_epoch: confirmation.authority_epoch,
    })
}

// ---------------------------------------------------------------------------
// 持久化行
// ---------------------------------------------------------------------------

/// `invocations` 的只读投影行：检查点要能区分「哪一条执行变了」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationFactRow {
    pub invocation_id: String,
    pub run_id: String,
    pub state: String,
    pub last_observed_at_ms: Option<i64>,
    pub fencing_token: i64,
}

/// `handover_checkpoints` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRow {
    pub company_id: String,
    pub checkpoint_id: String,
    pub scope_key: String,
    pub project_id: String,
    pub cursor: String,
    pub item_count: i64,
    pub items_json: String,
    pub gaps_json: String,
    pub captured_by: String,
    pub captured_at: String,
    pub recorded_at_ms: i64,
}

/// `takeover_confirmations` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationRow {
    pub company_id: String,
    pub confirmation_id: String,
    pub scope_key: String,
    pub checkpoint_id: String,
    pub checkpoint_cursor: String,
    pub reviewed_cursor: String,
    pub appointment_id: String,
    pub authority_epoch: u64,
    pub decision: String,
    pub confirmed_by: String,
    pub note: String,
    pub open_items_json: String,
    pub delta_json: String,
    pub confirmed_at: String,
    pub recorded_at_ms: i64,
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ReviewError {
    Store(StoreError),
    Transfer(String),
    EmptyField(&'static str),
    NonCanonicalTimestamp {
        field: &'static str,
    },
    UnsupportedScope {
        scope_key: String,
        detail: String,
    },
    TooManyFacts {
        project_id: String,
        count: usize,
        limit: usize,
    },
    CorruptFingerprint {
        subject: String,
    },
    UnknownCheckpoint {
        checkpoint_id: String,
    },
    CheckpointScopeMismatch {
        checkpoint_id: String,
        expected: String,
        actual: String,
    },
    CursorMismatch {
        expected: String,
        actual: String,
    },
    CorruptCheckpoint {
        checkpoint_id: String,
        detail: String,
    },
    CorruptConfirmation {
        confirmation_id: String,
        detail: String,
    },
    NoActiveAppointment {
        scope_key: String,
    },
    UnknownAppointment {
        appointment_id: String,
    },
    StaleAppointment {
        expected_appointment_id: String,
        current_appointment_id: String,
        current_epoch: u64,
    },
    AppointmentInactive {
        appointment_id: String,
        status: String,
    },
    ConfirmerIsNotManager {
        confirmed_by: String,
        manager_agent_id: String,
    },
    ConfirmationKeyReuse {
        confirmation_id: String,
    },
    ConfirmationIsNotAuthority {
        confirmation_id: String,
        authority_epoch: u64,
    },
    Serialization(String),
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "存储错误：{error}"),
            Self::Transfer(detail) => write!(f, "委任读取失败：{detail}"),
            Self::EmptyField(field) => write!(f, "{field} 不能为空"),
            Self::NonCanonicalTimestamp { field } => {
                write!(f, "{field} 不是 YYYY-MM-DDTHH:MM:SSZ 形式的规范时间戳")
            }
            Self::UnsupportedScope { scope_key, detail } => {
                write!(f, "暂不支持作用域 {scope_key}：{detail}")
            }
            Self::TooManyFacts {
                project_id,
                count,
                limit,
            } => write!(
                f,
                "项目 {project_id} 有 {count} 项事实，超过检查点上限 {limit}：不截断，拒绝给出不完整游标"
            ),
            Self::CorruptFingerprint { subject } => {
                write!(f, "{subject} 的指纹含分隔符，无法构成稳定游标")
            }
            Self::UnknownCheckpoint { checkpoint_id } => {
                write!(f, "检查点 {checkpoint_id} 不存在")
            }
            Self::CheckpointScopeMismatch {
                checkpoint_id,
                expected,
                actual,
            } => write!(
                f,
                "检查点 {checkpoint_id} 属于 {actual}，与本次作用域 {expected} 不一致"
            ),
            Self::CursorMismatch { expected, actual } => write!(
                f,
                "确认绑定的游标 {expected} 与检查点当前游标 {actual} 不一致：先重新读取再确认"
            ),
            Self::CorruptCheckpoint {
                checkpoint_id,
                detail,
            } => write!(f, "检查点 {checkpoint_id} 不可信：{detail}"),
            Self::CorruptConfirmation {
                confirmation_id,
                detail,
            } => write!(f, "接管确认 {confirmation_id} 不可信：{detail}"),
            Self::NoActiveAppointment { scope_key } => {
                write!(
                    f,
                    "作用域 {scope_key} 当前没有委任：接管确认不能先于委任产生"
                )
            }
            Self::UnknownAppointment { appointment_id } => {
                write!(f, "委任 {appointment_id} 不存在")
            }
            Self::StaleAppointment {
                expected_appointment_id,
                current_appointment_id,
                current_epoch,
            } => write!(
                f,
                "确认绑定的委任 {expected_appointment_id} 已不是当前委任（当前 {current_appointment_id}，epoch {current_epoch}）"
            ),
            Self::AppointmentInactive {
                appointment_id,
                status,
            } => write!(f, "委任 {appointment_id} 处于 {status}，不能提交接管确认"),
            Self::ConfirmerIsNotManager {
                confirmed_by,
                manager_agent_id,
            } => write!(
                f,
                "{confirmed_by} 不是当前受委任负责人（{manager_agent_id}）：确认必须由本人提交"
            ),
            Self::ConfirmationKeyReuse { confirmation_id } => write!(
                f,
                "确认键 {confirmation_id} 已被不同内容的确认使用：同键不得改写历史"
            ),
            Self::ConfirmationIsNotAuthority {
                confirmation_id,
                authority_epoch,
            } => write!(
                f,
                "接管确认 {confirmation_id}（epoch {authority_epoch}）只是核对回执，不能作为管理票证"
            ),
            Self::Serialization(detail) => write!(f, "序列化失败：{detail}"),
        }
    }
}

impl std::error::Error for ReviewError {}

impl From<StoreError> for ReviewError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<crate::manager_transfer::TransferError> for ReviewError {
    fn from(error: crate::manager_transfer::TransferError) -> Self {
        Self::Transfer(error.to_string())
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
        AcceptanceBinding, AcceptanceRecord, ArtifactKind, ArtifactRevision, ArtifactVisibility,
        Digest, DigestAlgorithm, StorageRef,
    };
    use crate::domain::execution::{ExecutionState, InvocationRecord};
    use crate::domain::model::{
        AcceptanceCriteria, Assignment, Issue, IssueKind, IssueSpec, Owner, OwnerKind, Phase,
        ResourceMeta, ResourceStatus, Scope,
    };
    use crate::domain::{DomainId, Version};
    use crate::handoff_snapshot::HandoffSnapshotRow;
    use crate::manager_transfer::{
        AppointRequest, CommandRejectionCode, ManagementCommand, ManagementCommandSet,
        ManagementGate, TransferError,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const COMPANY: &str = "company-1";
    const PROJECT: &str = "project-1";
    const AUDIENCE: &str = "host:local";
    const NOW_MS: i64 = 1_764_100_000_000;
    const AT: &str = "2026-11-20T12:00:00Z";

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-review-{tag}-{}-{seq}", std::process::id()));
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
            execution_id: Some(format!("exec-{invocation_id}")),
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

    fn move_invocation(store: &mut SqliteStore, invocation_id: &str, state: ExecutionState) {
        let uow = store.transaction().unwrap();
        assert!(
            uow.set_invocation_state(invocation_id, state, NOW_MS + 60_000)
                .unwrap()
        );
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

    fn add_snapshot(
        store: &SqliteStore,
        snapshot_key: &str,
        issue_id: &str,
        completeness: &str,
        missing_json: &str,
    ) {
        let row = HandoffSnapshotRow {
            company_id: COMPANY.to_string(),
            snapshot_key: snapshot_key.to_string(),
            project_id: PROJECT.to_string(),
            issue_id: issue_id.to_string(),
            work_package_revision: "wp-1".to_string(),
            source_attempt_id: format!("attempt-{issue_id}"),
            level: "L2".to_string(),
            source: "host_reconstructed".to_string(),
            completeness: completeness.to_string(),
            missing_json: missing_json.to_string(),
            integrity_json: "[]".to_string(),
            input_revision: "input-1".to_string(),
            scene_revision: "scene-1".to_string(),
            snapshot_seq: 1,
            payload_digest: Some("sha256:11".to_string()),
            captured_at: AT.to_string(),
            registered_at_ms: NOW_MS,
        };
        assert!(store.insert_handoff_snapshot(&row).unwrap());
    }

    fn capture(store: &SqliteStore) -> HandoverCheckpoint {
        HandoverCheckpoint::capture(store, COMPANY, &scope(), AUDIENCE, AT, NOW_MS).unwrap()
    }

    fn confirm_request(
        checkpoint: &HandoverCheckpoint,
        appointment: &ManagementAppointment,
        confirmation_id: &str,
    ) -> ConfirmRequest {
        ConfirmRequest::new(
            COMPANY,
            scope(),
            confirmation_id,
            appointment.manager_agent_id.as_str(),
            AT,
            NOW_MS,
        )
        .against(checkpoint)
        .by(appointment)
    }

    #[test]
    fn checkpoint_is_idempotent_for_the_same_field() {
        let root = temp_root("checkpoint");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);

        let first = capture(&store);
        assert_eq!(first.item_count, 1);
        assert_eq!(first.items[0].kind, FactKind::Issue);
        assert!(first.cursor.starts_with("sha256:"));
        assert!(
            first
                .checkpoint_id
                .starts_with("checkpoint:project:project-1#")
        );

        // 同现场第二次捕获（哪怕换了捕获人和时间）落回同一行，不覆盖第一次登记。
        let second = HandoverCheckpoint::capture(
            &store,
            COMPANY,
            &scope(),
            "host:other",
            "2026-11-20T12:05:00Z",
            NOW_MS + 1,
        )
        .unwrap();
        assert_eq!(first.cursor, second.cursor);
        assert_eq!(first.checkpoint_id, second.checkpoint_id);
        assert_eq!(second.captured_by, AUDIENCE, "同游标不覆盖第一次登记");
        assert_eq!(second.captured_at, AT);

        let latest = HandoverCheckpoint::latest(&store, COMPANY, &scope())
            .unwrap()
            .unwrap();
        assert_eq!(latest, first);
    }

    #[test]
    fn delta_separates_added_changed_and_vanished() {
        let root = temp_root("delta");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        add_run(&mut store, "run-1", "issue-1");
        add_invocation(&mut store, "inv-1", "run-1", ExecutionState::Running);
        let checkpoint = capture(&store);

        add_invocation(&mut store, "inv-2", "run-1", ExecutionState::Running);
        move_invocation(&mut store, "inv-1", ExecutionState::Completed);
        add_artifact(&mut store, "art-1");
        add_revision(&mut store, "rev-1", "art-1", "manager-a");
        accept(
            &mut store,
            "acc-1",
            "art-1",
            "rev-1",
            AcceptanceDecision::Accepted,
        );

        let delta = checkpoint.delta(&store, COMPANY).unwrap();
        assert_ne!(delta.from_cursor, delta.to_cursor);
        assert!(delta.added.iter().any(|item| item.id == "inv-2"));
        assert!(delta.added.iter().any(|item| item.id == "art-1"));
        assert!(delta.added.iter().any(|item| item.id == "rev-1"));
        assert!(delta.added.iter().any(|item| item.id == "acc-1"));
        let change = delta
            .changed
            .iter()
            .find(|item| item.id == "inv-1")
            .expect("inv-1 的状态变化应被识别");
        assert!(change.before.contains("state=Running"));
        assert!(change.after.contains("state=Completed"));
        assert!(delta.vanished.is_empty());
        assert!(!delta.is_empty());
        assert_eq!(delta.counts().changed, 1);
        assert!(delta.describe().contains("新增"));

        // 消失项不静默：伪造一条只存在于旧清单里的 Run，增量必须报出来。
        let mut doctored = checkpoint.clone();
        doctored.items.push(FactState::new(
            FactKind::Run,
            "run-ghost",
            "issue=-|desired=Running",
        ));
        let delta = doctored.delta(&store, COMPANY).unwrap();
        assert_eq!(delta.vanished.len(), 1);
        assert_eq!(delta.vanished[0].id, "run-ghost");
    }

    #[test]
    fn gaps_are_queryable_without_the_old_manager_summary() {
        let root = temp_root("gaps");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        add_run(&mut store, "run-1", "issue-1");
        add_invocation(&mut store, "inv-1", "run-1", ExecutionState::Running);

        let checkpoint = capture(&store);
        assert_eq!(checkpoint.gaps.len(), 1);
        assert_eq!(checkpoint.gaps[0].kind, GapKind::MissingSnapshot);
        assert_eq!(checkpoint.gaps[0].subject, "run-1");
        assert!(checkpoint.gaps[0].detail.contains("不能依赖旧经理"));

        // 补一份部分快照：缺口种类变化，且快照本身作为事实进入增量。
        add_snapshot(&store, "snap-1", "issue-1", "partial", "[\"logs\"]");
        let delta = checkpoint.delta(&store, COMPANY).unwrap();
        assert!(
            delta.added_of(FactKind::Snapshot).len() == 1,
            "新增快照应出现在增量里"
        );
        assert_eq!(delta.gaps_of(GapKind::MissingSnapshot).len(), 0);
        assert_eq!(delta.gaps_of(GapKind::PartialSnapshot).len(), 1);
        assert!(
            delta.gaps_of(GapKind::PartialSnapshot)[0]
                .detail
                .contains("logs")
        );
    }

    #[test]
    fn unreconciled_execution_and_open_acceptance_are_gaps() {
        let root = temp_root("attention");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        add_run(&mut store, "run-1", "issue-1");
        add_invocation(&mut store, "inv-unknown", "run-1", ExecutionState::Unknown);
        add_artifact(&mut store, "art-1");
        add_revision(&mut store, "rev-1", "art-1", "agent-w1");
        accept(
            &mut store,
            "acc-human",
            "art-1",
            "rev-1",
            AcceptanceDecision::RequiresHuman,
        );

        let snapshot = FieldSnapshot::capture(&store, COMPANY, PROJECT).unwrap();
        assert_eq!(snapshot.gaps_of(GapKind::UnreconciledExecution), 1);
        assert_eq!(snapshot.gaps_of(GapKind::OpenAcceptance), 1);
        let open = snapshot
            .gaps
            .iter()
            .find(|gap| gap.kind == GapKind::OpenAcceptance)
            .unwrap();
        assert_eq!(open.subject, "acc-human");
        assert!(open.detail.contains("待人工决定"));
    }

    #[test]
    fn read_only_review_creates_no_authority() {
        let root = temp_root("readonly");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        add_run(&mut store, "run-1", "issue-1");
        add_invocation(&mut store, "inv-1", "run-1", ExecutionState::Running);

        // 未受委任的人也能只读核对（I-65：不依赖旧经理在线）。
        let checkpoint = capture(&store);
        let delta = checkpoint.delta(&store, COMPANY).unwrap();
        assert!(delta.gaps_of(GapKind::MissingSnapshot).len() == 1);

        // 但读不出任何管理权：既没有委任行，也没有作用域头。
        assert!(
            ManagementAppointment::list_for_scope(&store, COMPANY, &scope().scope_key())
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .get_scope_head(COMPANY, &scope().scope_key())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            review_status(&store, COMPANY, &scope()).unwrap(),
            ReviewState::NeverConfirmed
        );

        // 请求必须点名「确认哪一份委任」：不写就是坏请求，而不是默默挑一个。
        let unbound = ConfirmRequest::new(COMPANY, scope(), "conf-0", "manager-b", AT, NOW_MS)
            .against(&checkpoint);
        assert!(matches!(
            TakeoverConfirmation::confirm(&mut store, unbound).unwrap_err(),
            ReviewError::EmptyField("appointment_id")
        ));

        // 点名了委任但库里还没有：确认不能先于委任产生。
        let mut named = ConfirmRequest::new(COMPANY, scope(), "conf-1", "manager-b", AT, NOW_MS)
            .against(&checkpoint);
        named.appointment_id = ManagementAppointment::id_for(&scope(), 1);
        named.expected_epoch = 1;
        let error = TakeoverConfirmation::confirm(&mut store, named).unwrap_err();
        assert!(
            matches!(error, ReviewError::NoActiveAppointment { .. }),
            "实际 {error}"
        );
    }

    #[test]
    fn confirmation_is_bound_to_the_checkpoint_you_reviewed() {
        let root = temp_root("binding");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        let checkpoint = capture(&store);
        let appointment =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();

        // 游标被换掉：拒绝，而不是把确认挂到另一份现场上。
        let mut tampered = confirm_request(&checkpoint, &appointment, "conf-1");
        tampered.expected_cursor = "sha256:00".to_string();
        match TakeoverConfirmation::confirm(&mut store, tampered).unwrap_err() {
            ReviewError::CursorMismatch { expected, actual } => {
                assert_eq!(expected, "sha256:00");
                assert_eq!(actual, checkpoint.cursor);
            }
            other => panic!("期望 CursorMismatch，实际 {other}"),
        }

        // 检查点不存在：拒绝。
        let mut missing = confirm_request(&checkpoint, &appointment, "conf-2");
        missing.checkpoint_id = "checkpoint:project:project-1#deadbeefdeadbeef".to_string();
        assert!(matches!(
            TakeoverConfirmation::confirm(&mut store, missing).unwrap_err(),
            ReviewError::UnknownCheckpoint { .. }
        ));

        // 跨作用域：另一项目的检查点不能用来确认本项目。
        let mut crossed = confirm_request(&checkpoint, &appointment, "conf-3");
        crossed.scope = ManagementScope::project("project-2");
        assert!(matches!(
            TakeoverConfirmation::confirm(&mut store, crossed).unwrap_err(),
            ReviewError::CheckpointScopeMismatch { .. }
        ));

        // 正常确认：一次成功。
        let confirmation = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment, "conf-4"),
        )
        .unwrap();
        assert_eq!(confirmation.decision, ReviewDecision::Confirmed);
        assert_eq!(confirmation.authority_epoch, 1);
        assert_eq!(confirmation.checkpoint_cursor, checkpoint.cursor);
        assert_eq!(confirmation.reviewed_cursor, checkpoint.cursor);
    }

    #[test]
    fn confirmation_does_not_grant_management_authority() {
        let root = temp_root("no-authority");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        let checkpoint = capture(&store);
        let appointment =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();
        let confirmation = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment, "conf-1"),
        )
        .unwrap();

        // 类型事实：确认没有通往票证的路径。
        match confirmation_as_ticket(&confirmation).unwrap_err() {
            ReviewError::ConfirmationIsNotAuthority {
                confirmation_id,
                authority_epoch,
            } => {
                assert_eq!(confirmation_id, "conf-1");
                assert_eq!(authority_epoch, 1);
            }
            other => panic!("期望 ConfirmationIsNotAuthority，实际 {other}"),
        }

        // 确认没有写出第二份委任，也没有推高 epoch。
        assert_eq!(
            ManagementAppointment::list_for_scope(&store, COMPANY, &scope().scope_key())
                .unwrap()
                .len(),
            1
        );
        let head = store
            .get_scope_head(COMPANY, &scope().scope_key())
            .unwrap()
            .unwrap();
        assert_eq!(head.authority_epoch, 1);
        assert_eq!(head.current_appointment_id, appointment.appointment_id);
    }

    #[test]
    fn confirmation_becomes_superseded_when_the_epoch_advances() {
        let root = temp_root("superseded");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        let checkpoint = capture(&store);
        let mut appointment_a =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();
        TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment_a, "conf-a"),
        )
        .unwrap();
        assert!(matches!(
            review_status(&store, COMPANY, &scope()).unwrap(),
            ReviewState::Current { .. }
        ));

        // 撤任：头 epoch 前进到 2，旧确认只变成 Superseded。
        let version = appointment_a.version;
        appointment_a
            .revoke(&store, "2026-11-20T12:05:00Z", version, NOW_MS)
            .unwrap();
        let state = review_status(&store, COMPANY, &scope()).unwrap();
        assert_eq!(
            state,
            ReviewState::Superseded {
                confirmation_id: "conf-a".to_string(),
                authority_epoch: 1,
                head_epoch: 2,
            }
        );
        assert!(state.requires_review());

        // 旧经理不能再确认（已不是当前委任）。
        let error = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment_a, "conf-a2"),
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                ReviewError::StaleAppointment { .. } | ReviewError::AppointmentInactive { .. }
            ),
            "实际 {error}"
        );

        // 新经理在 epoch 3 确认后重新变成 Current。
        let appointment_b = ManagementAppointment::appoint(
            &store,
            appoint_request("manager-b", "2026-11-20T12:06:00Z"),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(appointment_b.authority_epoch, 3);
        let confirmation = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment_b, "conf-b"),
        )
        .unwrap();
        assert_eq!(confirmation.authority_epoch, 3);
        match review_status(&store, COMPANY, &scope()).unwrap() {
            ReviewState::Current {
                confirmation_id,
                authority_epoch,
                ..
            } => {
                assert_eq!(confirmation_id, "conf-b");
                assert_eq!(authority_epoch, 3);
            }
            other => panic!("期望 Current，实际 {other}"),
        }
    }

    #[test]
    fn blocked_confirmation_can_be_followed_by_a_confirmed_one_in_the_same_epoch() {
        let root = temp_root("blocked");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        add_run(&mut store, "run-1", "issue-1");
        add_invocation(&mut store, "inv-1", "run-1", ExecutionState::Running);
        let checkpoint = capture(&store);
        let appointment =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();

        // 缺资料：明确拒绝，不能只写一句空话。
        assert!(matches!(
            TakeoverConfirmation::confirm(
                &mut store,
                confirm_request(&checkpoint, &appointment, "conf-empty")
                    .decision(ReviewDecision::Blocked, "   ")
            )
            .unwrap_err(),
            ReviewError::EmptyField("note")
        ));

        let blocked = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment, "conf-blocked")
                .decision(ReviewDecision::Blocked, "没有旧经理总结，且快照缺失"),
        )
        .unwrap();
        assert_eq!(blocked.open_items.len(), 1);
        assert_eq!(blocked.open_items[0].kind, GapKind::MissingSnapshot);
        match review_status(&store, COMPANY, &scope()).unwrap() {
            ReviewState::Blocked {
                confirmation_id, ..
            } => {
                assert_eq!(confirmation_id, "conf-blocked");
            }
            other => panic!("期望 Blocked，实际 {other}"),
        }

        // 补资料后同一 epoch 再确认：允许，且历史保留两次记录。
        let confirmed = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment, "conf-confirmed"),
        )
        .unwrap();
        assert_eq!(confirmed.delta.gaps, 1, "增量记录的是确认时仍存在的缺口");
        let history = TakeoverConfirmation::history(&store, COMPANY, &scope().scope_key()).unwrap();
        assert_eq!(history.len(), 2);
        assert!(matches!(
            review_status(&store, COMPANY, &scope()).unwrap(),
            ReviewState::Current { .. }
        ));
    }

    #[test]
    fn commands_are_still_checked_against_the_current_epoch_after_confirmation() {
        let root = temp_root("commands");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        let checkpoint = capture(&store);
        let mut appointment_a =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();
        TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment_a, "conf-a"),
        )
        .unwrap();

        // 确认之后命令照常可用（权力来自委任，不是来自确认）。
        let ticket = ManagementTicket::issue(
            &appointment_a,
            ManagementCommand::AssignTask,
            "2026-11-20T12:01:00Z",
            "2026-11-20T12:10:00Z",
        )
        .unwrap();
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        let verified = gate
            .submit(&ticket, "cmd-1", "2026-11-20T12:01:00Z", NOW_MS)
            .unwrap();
        assert_eq!(verified.authority_epoch, 1);

        // 换人后同一条旧票被拒：确认没有让 epoch 检查短路。
        let version = appointment_a.version;
        appointment_a
            .revoke(&store, "2026-11-20T12:05:00Z", version, NOW_MS)
            .unwrap();
        ManagementAppointment::appoint(
            &store,
            appoint_request("manager-b", "2026-11-20T12:06:00Z"),
            NOW_MS,
        )
        .unwrap();
        let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
        match gate
            .submit(&ticket, "cmd-2", "2026-11-20T12:06:00Z", NOW_MS)
            .unwrap_err()
        {
            TransferError::Rejected { code, .. } => {
                assert_eq!(code, CommandRejectionCode::StaleAppointment)
            }
            other => panic!("期望 StaleAppointment，实际 {other}"),
        }
    }

    #[test]
    fn confirmation_key_is_idempotent_and_rejects_reuse() {
        let root = temp_root("idempotent");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        let checkpoint = capture(&store);
        let appointment =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();

        let first = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment, "conf-1"),
        )
        .unwrap();
        let again = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment, "conf-1"),
        )
        .unwrap();
        assert_eq!(first, again);
        assert_eq!(
            TakeoverConfirmation::history(&store, COMPANY, &scope().scope_key())
                .unwrap()
                .len(),
            1,
            "同键同内容不写第二条"
        );

        let reuse = confirm_request(&checkpoint, &appointment, "conf-1")
            .decision(ReviewDecision::Blocked, "改主意了");
        assert!(matches!(
            TakeoverConfirmation::confirm(&mut store, reuse).unwrap_err(),
            ReviewError::ConfirmationKeyReuse { .. }
        ));
    }

    #[test]
    fn confirmer_must_be_the_appointed_manager() {
        let root = temp_root("confirmer");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        let checkpoint = capture(&store);
        let appointment =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();

        let request = ConfirmRequest::new(COMPANY, scope(), "conf-1", "manager-z", AT, NOW_MS)
            .against(&checkpoint)
            .by(&appointment);
        match TakeoverConfirmation::confirm(&mut store, request).unwrap_err() {
            ReviewError::ConfirmerIsNotManager {
                confirmed_by,
                manager_agent_id,
            } => {
                assert_eq!(confirmed_by, "manager-z");
                assert_eq!(manager_agent_id, "manager-a");
            }
            other => panic!("期望 ConfirmerIsNotManager，实际 {other}"),
        }
    }

    #[test]
    fn issue_subtree_scope_is_refused_rather_than_approximated() {
        let root = temp_root("subtree");
        let store = open_store(&root);
        let error = HandoverCheckpoint::capture(
            &store,
            COMPANY,
            &ManagementScope::issue_subtree(PROJECT, "issue-root"),
            AUDIENCE,
            AT,
            NOW_MS,
        )
        .unwrap_err();
        match error {
            ReviewError::UnsupportedScope { scope_key, detail } => {
                assert_eq!(scope_key, "issue:project-1/issue-root");
                assert!(detail.contains("3.5"));
            }
            other => panic!("期望 UnsupportedScope，实际 {other}"),
        }
    }

    #[test]
    fn confirmation_survives_reopen_and_records_the_reviewed_delta() {
        let root = temp_root("reopen");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1", Some("agent-w1"), Phase::Running);
        let checkpoint = capture(&store);
        let appointment =
            ManagementAppointment::appoint(&store, appoint_request("manager-a", AT), NOW_MS)
                .unwrap();

        // 检查点之后又有一名员工交付：确认必须记录「我看到了这份增量」。
        add_run(&mut store, "run-late", "issue-1");
        add_invocation(&mut store, "inv-late", "run-late", ExecutionState::Running);
        let confirmation = TakeoverConfirmation::confirm(
            &mut store,
            confirm_request(&checkpoint, &appointment, "conf-1"),
        )
        .unwrap();
        assert_eq!(confirmation.delta.added, 2);
        assert_ne!(confirmation.reviewed_cursor, confirmation.checkpoint_cursor);
        assert_eq!(
            confirmation.open_items.len(),
            1,
            "新执行没有快照：缺口照实记录"
        );
        drop(store);

        let reopened = open_store(&root);
        let loaded = TakeoverConfirmation::load(&reopened, COMPANY, "conf-1")
            .unwrap()
            .expect("确认行应持久");
        assert_eq!(loaded, confirmation);
        assert!(matches!(
            review_status(&reopened, COMPANY, &scope()).unwrap(),
            ReviewState::Current { .. }
        ));
    }
}
