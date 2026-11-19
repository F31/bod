//! D6 恢复与清理的持久记录（任务 7.1—7.4）。
//!
//! 纯数据 + 枚举解析，不含规则；规则在 `migration_safety` / `lifecycle_recovery` /
//! `cleanup_recovery` 中，持久化在 `store`。

use crate::domain::DomainId;
use crate::domain::artifact::StorageRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationRisk {
    Safe,
    Destructive,
}

impl MigrationRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Destructive => "destructive",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "destructive" => Self::Destructive,
            _ => Self::Safe,
        }
    }
}

/// 迁移登记项：不可变 id、checksum、依赖、兼容窗口与恢复说明（§33.7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRecord {
    pub migration_id: String,
    pub checksum: String,
    pub depends_on: Vec<String>,
    pub schema_range: String,
    pub software_range: String,
    pub risk: MigrationRisk,
    pub compatibility_window: String,
    pub backup_notes: String,
    pub data_notes: String,
    pub applied_at_ms: i64,
}

impl MigrationRecord {
    pub fn depends_on_text(&self) -> String {
        self.depends_on.join(",")
    }

    pub fn parse_depends(text: &str) -> Vec<String> {
        text.split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupStatus {
    Verified,
    Incomplete,
}

impl BackupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Incomplete => "incomplete",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "verified" => Self::Verified,
            _ => Self::Incomplete,
        }
    }

    /// 只有 verified 才允许声称"可恢复"。
    pub fn is_recoverable(self) -> bool {
        matches!(self, Self::Verified)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRecord {
    pub backup_id: String,
    pub company_id: DomainId,
    pub path: String,
    pub db_sha256: String,
    pub db_bytes: i64,
    /// 快照是否已包含已提交的 WAL 内容（VACUUM INTO 会合并）。
    pub wal_included: bool,
    pub snapshot_method: String,
    pub cas_object_count: i64,
    pub config_revision: String,
    pub adapter_versions: String,
    pub key_recovery_note: String,
    pub status: BackupStatus,
    pub problems: Vec<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseState {
    Open,
    Resolved,
    Abandoned,
}

impl CaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "resolved" => Self::Resolved,
            "abandoned" => Self::Abandoned,
            _ => Self::Open,
        }
    }
}

/// 对账案件：Unknown 外部动作的事实容器，必须带处理人与下一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationCase {
    pub case_id: String,
    pub company_id: DomainId,
    pub invocation_id: Option<String>,
    pub logical_action_key: String,
    pub impact_scope: String,
    pub risk_reservation_refs: Vec<String>,
    pub known_evidence: String,
    pub assignee: String,
    pub next_step: String,
    pub state: CaseState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupTargetKind {
    Project,
    Run,
    Artifact,
    Issue,
}

impl CleanupTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Run => "run",
            Self::Artifact => "artifact",
            Self::Issue => "issue",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "run" => Self::Run,
            "artifact" => Self::Artifact,
            "issue" => Self::Issue,
            _ => Self::Project,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupState {
    Requested,
    Blocked,
    Completed,
    Abandoned,
}

impl CleanupState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "blocked" => Self::Blocked,
            "completed" => Self::Completed,
            "abandoned" => Self::Abandoned,
            _ => Self::Requested,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRequest {
    pub request_id: String,
    pub company_id: DomainId,
    pub target_kind: CleanupTargetKind,
    pub target_id: String,
    pub state: CleanupState,
    pub blockers: Vec<String>,
    pub requested_by: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Abandon 事实记录：保留残留外部资源、风险、责任人与对账入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupAbandon {
    pub abandon_id: String,
    pub company_id: DomainId,
    pub request_id: String,
    pub residual_resources: Vec<String>,
    pub risks: Vec<String>,
    pub owner: String,
    pub reconciliation_entry: String,
    pub created_at_ms: i64,
}

/// 清理评估用的修订摘要：只回答"该修订是否必须保留"，不复制 digest 与大小。
///
/// 与 `ArtifactRevision` 的区别：这里允许被外部引用（Manifest/接受指针）标记，
/// 因此不参与不可变修订的写入路径，只读。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionPin {
    pub revision_id: String,
    pub artifact_id: String,
    pub storage: StorageRef,
    /// 该修订是否为所属成果当前的接受指针（CAS 指针，非内容地址）。
    pub accepted: bool,
    /// 引用该修订的正式 Manifest UID 列表。
    pub manifest_refs: Vec<String>,
}

impl RevisionPin {
    /// 外部权威存储：宿主从不销毁其内容，只清理自身引用。
    pub fn is_external(&self) -> bool {
        matches!(self.storage, StorageRef::External(_))
    }

    /// 被正式交付固定：接受指针或 Manifest 引用任一成立。
    pub fn is_formally_pinned(&self) -> bool {
        self.accepted || !self.manifest_refs.is_empty()
    }
}
