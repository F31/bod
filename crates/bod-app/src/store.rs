//! SQLite 持久化与 UnitOfWork（P0 D1 任务 2.6）。
//!
//! 本文件只保留**跨领域**的东西：迁移表、连接与事务的生命周期、错误类型、
//! 以及被多个领域共用的编码辅助。具体领域的读写分散在同名子模块里，
//! 每个子模块都通过 `impl SqliteStore` / `impl UnitOfWork` 扩展同一对类型
//! （Rust 允许 inherent impl 跨模块分布，私有字段对后代模块可见）。
//!
//! 边界：`Connection` 由结构体独占，不跨线程共享可变引用；异步层用有界写请求
//! 队列把命令送进单一写线程（§37.5）。不在事务内执行网络、Agent 或人工等待。

pub mod artifact;
pub mod audit;
pub mod budget;
pub mod delivery;
pub mod execution;
pub mod facts;
pub mod governance;
pub mod maintenance;
pub mod project;
pub mod queue;
pub mod read_port;
pub mod revoke;
pub mod rows;

#[cfg(test)]
mod tests;

use crate::audit_log::{AuditAction, AuditDecision, AuditEvent, AuditRecord};
use crate::config::BodConfig;
use crate::coverage_receipt::VerificationConclusion;
use crate::delivery_idempotency::{IdempotencyError, Receipt, Replay, ResultKind, classify};
use crate::delivery_link::{AppendLinkCommand, LinkOutcome, NormalizedTarget, WorkPackageLink};
use crate::delivery_source::{
    COMMAND_IMPORT_SOURCE, ImportOutcome, ImportSourceCommand, LinkedSource, SourceError,
    SourceIdentity, SourceResolution,
};
use crate::delivery_tracking::{
    DeliveryError, ObservationKind, ObservationRow, TargetRow, TrackingRow, TrackingState,
    TrackingView, evidence_stamps, project_target, verification_conclusion,
};
use crate::domain::artifact::{
    AcceptanceBinding, AcceptanceDecision, AcceptanceRecord, Artifact, ArtifactKind,
    ArtifactRevision, ArtifactVisibility, DependencyKind, Digest, DigestAlgorithm,
    RevisionDependency, StorageRef, VerificationRecord, VerificationState, Waiver,
    WorkPackageState,
};
use crate::domain::budget::{
    BudgetLimit, BudgetScopeKind, BudgetUsage, Dimension, LedgerEntry, LedgerEntryKind, Measure,
    Reservation, ReservationState,
};
use crate::domain::execution::{
    AdapterDescriptor, EvidenceLevel, EvidenceSource, ExecutionState, InvocationRecord, MatrixRow,
    ObservationCursor, ObservationEnvelope, ObservationType, ProbeReport, SupportLevel,
};
use crate::domain::model::{
    AcceptanceCriteria, Assignment, Grant, Issue, IssueKind, IssueSpec, Owner, OwnerKind,
    Permission, Phase, RepositoryContext, ResourceMeta, ResourceStatus, Scope,
};
use crate::domain::queue::{EffectStatus, OutboxDelivery, QueueState, WakeReceipt, WorkQueueEntry};
use crate::domain::{DenialReason, DomainId, Version};
use crate::handoff_event::{HandoffAcceptanceRow, HandoffEventRow};
use crate::handoff_snapshot::HandoffSnapshotRow;
use crate::manager_transfer::{AppointmentRow, ManagementCommandRow, ScopeHeadRow};
use crate::recovery_models::{
    BackupRecord, BackupStatus, CaseState, CleanupAbandon, CleanupRequest, CleanupState,
    CleanupTargetKind, MigrationRecord, MigrationRisk, ReconciliationCase, RevisionPin,
};
use crate::takeover_intent::TakeoverIntentRow;
use crate::takeover_review::{CheckpointRow, ConfirmationRow, InvocationFactRow};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::fs;
use std::path::Path;

// 外部只认识 `crate::store::X` 这几条路径。行结构搬进 `rows` 之后在这里重新导出，
// 调用方不必知道它们被放到了哪个文件——重构不改变对外形状。
pub use rows::{
    AppendedRevision, ConfirmedCandidate, CoreRowCounts, DecisionBodyRow, DisputeRow, GrantFactRow,
    HierarchyRow, LeaseFactRow, ManifestBodyRow, OrgMetadataRow, ProposedCandidate,
    RegisteredSource, RevisionBodyRow,
};

/// 有序迁移列表。001 为 D1 领域表，002 为 D2 队列/outbox，003 为 D3 预算，
/// 004 为 D4 执行协议，005 为 D5 成果/修订/接受，006 为 D6 恢复与清理，
/// 007 为 P2 交接快照，008 为 P2 交接事件与接受，009 为 P2 接管意图，
/// 010 为 P2 管理委任/作用域头/命令审计，011 为 P2 检查点与接管确认，
/// 012 为 P2 历史争议与接受五元组冻结，013 为 P2 专项边界层级与组织元数据，
/// 014 为 decisions 不可变 trigger（P2 3.4 加固），015 为业务审计事件（CR-002），
/// 016 为 §11 交付跟踪（P8 3.1：跟踪项/目标/追加式观察）；
/// 017 为来源身份（P8 3.2：五元组/候选/追加式修订）；
/// 018 为去重语义（P8 3.3：命令回执/来源关系）；
/// 019 为关联追加（P8 3.4：内部工作包关联；目标关联与版本推进复用 016 的表与列）；
/// 020 为授权事实（P8 3.6 B1：项目成员/客户绑定/发布操作人/撤权修订）。
/// 新增迁移只能追加，不可原地改写（§33.7）。
/// P7 基准统一页缓存：负数按 KiB 解释，64 MiB；避免依赖 SQLite 默认 2 MiB。
const BENCH_CACHE_SIZE_KIB: i64 = 65_536;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_initial.sql"),
    include_str!("../migrations/002_queue.sql"),
    include_str!("../migrations/003_budget.sql"),
    include_str!("../migrations/004_execution.sql"),
    include_str!("../migrations/005_artifacts.sql"),
    include_str!("../migrations/006_recovery.sql"),
    include_str!("../migrations/007_handoff.sql"),
    include_str!("../migrations/008_handoff_events.sql"),
    include_str!("../migrations/009_takeover.sql"),
    include_str!("../migrations/010_management.sql"),
    include_str!("../migrations/011_handover_review.sql"),
    include_str!("../migrations/012_history_disputes.sql"),
    include_str!("../migrations/013_scope_org.sql"),
    include_str!("../migrations/014_history_triggers.sql"),
    include_str!("../migrations/015_audit.sql"),
    include_str!("../migrations/016_delivery_tracking.sql"),
    include_str!("../migrations/017_delivery_sources.sql"),
    include_str!("../migrations/018_delivery_idempotency.sql"),
    include_str!("../migrations/019_delivery_links.sql"),
    include_str!("../migrations/020_stage_facts.sql"),
];

/// 与 `MIGRATIONS` 一一对应的文件名，供迁移登记与 checksum 校验使用。
pub const MIGRATION_NAMES: &[&str] = &[
    "001_initial.sql",
    "002_queue.sql",
    "003_budget.sql",
    "004_execution.sql",
    "005_artifacts.sql",
    "006_recovery.sql",
    "007_handoff.sql",
    "008_handoff_events.sql",
    "009_takeover.sql",
    "010_management.sql",
    "011_handover_review.sql",
    "012_history_disputes.sql",
    "013_scope_org.sql",
    "014_history_triggers.sql",
    "015_audit.sql",
    "016_delivery_tracking.sql",
    "017_delivery_sources.sql",
    "018_delivery_idempotency.sql",
    "019_delivery_links.sql",
    "020_stage_facts.sql",
];

/// 已编译进二进制的迁移脚本（文件名 → SQL 正文）。
pub fn migration_sources() -> impl Iterator<Item = (&'static str, &'static str)> {
    MIGRATION_NAMES
        .iter()
        .copied()
        .zip(MIGRATIONS.iter().copied())
}

/// 唯一约束冲突：并发下「先查后插」输的那一方，靠它改成返回已有行。
fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Pragma(&'static str),
    VersionConflict {
        current: Version,
    },
    /// 外部 usage_key 已入账：同一实际用量只记一次（I-35）。
    DuplicateUsageKey {
        usage_key: String,
    },
    /// 审计事件未通过落库前校验（字段为空、决策与原因不一致、含凭据形态）。
    /// 单独一档而不是塞进 `Sql`：这是调用方的编程或安全问题，不是数据库故障。
    Audit(crate::audit_log::AuditRejection),
    /// 交付跟踪的读写出错：表外取值、跨跟踪项引用目标、人工补证缺备注。
    /// 同样单独一档——这些是数据或调用方错误，重试没有意义。
    Delivery(crate::delivery_tracking::DeliveryError),
    /// 来源身份/候选/修订的校验错误：五元组不成立、候选已裁决、跨项目引用。
    /// 与 `Delivery` 分开：它的错误信息要指向「身份」而不是「覆盖结论」。
    Source(crate::delivery_source::SourceError),
    /// 幂等键冲突或回执不可读（3.3）。单独一档：它的处置方式是「换键」或「查回执」，
    /// 不是重试——重试同一个键只会得到同一个答案。
    Idempotency(IdempotencyError),
    /// 关联追加的拒绝原因（3.4）：引用没给/给了两个、期望版本不一致、跟踪项已关闭。
    /// 与 `Delivery` 分开：这些错误的处置是「改请求」或「按当前版本重提」，
    /// 而不是去补证据。
    Link(crate::delivery_link::LinkError),
    /// 授权事实取不到（3.6 B1/B2）。**单独一档**：它的处置是「报故障」，
    /// 不是「当成没有事实」——降级为全可见或全不可见都是把故障伪装成权限。
    Facts(facts::StageFactsError),
    /// 撤权命令不成立（缺公司、缺主体、缺操作者）。与 `Sql` 分开：这些是调用方
    /// 的错误，重试同一个请求只会得到同一个答案。
    Revocation(revoke::RevocationError),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "存储 IO 失败: {err}"),
            Self::Sql(err) => write!(f, "SQLite 失败: {err}"),
            Self::Pragma(name) => write!(f, "SQLite pragma 校验失败: {name}"),
            Self::VersionConflict { current } => {
                write!(f, "资源版本冲突，当前版本 {}", current.0)
            }
            Self::DuplicateUsageKey { usage_key } => {
                write!(f, "usage_key 已入账，重复上报被去重: {usage_key}")
            }
            Self::Audit(rejection) => write!(f, "审计事件被拒绝: {}", rejection.explain()),
            Self::Delivery(err) => write!(f, "交付跟踪失败: {}", err.explain()),
            Self::Source(err) => write!(f, "来源身份失败: {}", err.explain()),
            Self::Idempotency(err) => write!(f, "幂等键冲突: {}", err.explain()),
            Self::Link(err) => write!(f, "关联追加失败: {}", err.explain()),
            Self::Facts(err) => write!(f, "授权事实装配失败: {}", err.explain()),
            Self::Revocation(err) => write!(f, "撤权命令不成立: {}", err.explain()),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sql(value)
    }
}

impl From<crate::delivery_tracking::DeliveryError> for StoreError {
    fn from(value: crate::delivery_tracking::DeliveryError) -> Self {
        Self::Delivery(value)
    }
}

impl From<crate::delivery_source::SourceError> for StoreError {
    fn from(value: crate::delivery_source::SourceError) -> Self {
        Self::Source(value)
    }
}

impl From<IdempotencyError> for StoreError {
    fn from(value: IdempotencyError) -> Self {
        Self::Idempotency(value)
    }
}

impl From<crate::delivery_link::LinkError> for StoreError {
    fn from(value: crate::delivery_link::LinkError) -> Self {
        Self::Link(value)
    }
}

impl From<facts::StageFactsError> for StoreError {
    fn from(value: facts::StageFactsError) -> Self {
        Self::Facts(value)
    }
}

impl From<revoke::RevocationError> for StoreError {
    fn from(value: revoke::RevocationError) -> Self {
        Self::Revocation(value)
    }
}

/// 单写连接持有者。写事务通过 `UnitOfWork` 显式开始。
pub struct SqliteStore {
    conn: Connection,
}

/// 一个业务命令只开一个 Company UnitOfWork；BEGIN IMMEDIATE 串行化写入。
///
/// `Drop` 保证任何错误路径（决策失败、提前 `?` 返回）都会回滚，避免连接上留下悬空事务——
/// 否则后续命令将全部报 "cannot start a transaction within a transaction"。
pub struct UnitOfWork<'a> {
    conn: &'a mut Connection,
    finished: bool,
}

impl Drop for UnitOfWork<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

impl SqliteStore {
    /// 打开数据库并验证 WAL/synchronous/busy_timeout/foreign_keys，随后应用初始迁移。
    pub fn open(path: &Path, config: &BodConfig) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // 内存库只用于单测，SQLite 不支持 MEMORY 日志切到 WAL；真实文件库必须校验 WAL。
        let is_memory = path == Path::new(":memory:");
        let journal: String = conn.query_row(
            if is_memory {
                "PRAGMA journal_mode=MEMORY"
            } else {
                "PRAGMA journal_mode=WAL"
            },
            [],
            |row| row.get(0),
        )?;
        if !is_memory && !journal.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Pragma("journal_mode"));
        }
        conn.pragma_update(None, "synchronous", config.sqlite.synchronous.as_str())?;
        conn.pragma_update(None, "busy_timeout", config.sqlite.busy_timeout_ms as i64)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        for migration in MIGRATIONS {
            conn.execute_batch(migration)?;
        }
        Ok(Self { conn })
    }

    pub fn in_memory(config: &BodConfig) -> Result<Self, StoreError> {
        Self::open(Path::new(":memory:"), config)
    }

    /// P7 基准口径专用：固定页缓存与临时表位置，避免不同机器的 SQLite 默认值混入两阶段对比。
    ///
    /// 不写入正常启动路径：产品默认值仍由 §37.14 的配置决定，测量报告必须显式声明这里设的值。
    #[doc(hidden)]
    pub fn apply_bench_pragmas(&self) -> Result<(), StoreError> {
        self.conn
            .pragma_update(None, "cache_size", BENCH_CACHE_SIZE_KIB)?;
        self.conn.pragma_update(None, "temp_store", "MEMORY")?;
        Ok(())
    }

    pub fn transaction(&mut self) -> Result<UnitOfWork<'_>, StoreError> {
        UnitOfWork::begin(&mut self.conn)
    }

    /// 只读连接视图：仅供本 crate 的观测读模型（`observability`）使用。
    ///
    /// 写路径仍必须走 `UnitOfWork`——绕开事务的写入无法参与回滚与条件更新，
    /// 那正是 §5 要求的边界。这里开放的是 `&Connection`（不可变），写不进去。
    pub(crate) fn read_connection(&self) -> &Connection {
        &self.conn
    }

    /// 仅测试用：直接执行 SQL，用于证明账本 trigger 真的阻断改写/删除。
    #[cfg(test)]
    pub(crate) fn execute_batch_for_test(&self, sql: &str) -> Result<(), StoreError> {
        self.conn.execute_batch(sql).map_err(StoreError::Sql)
    }
}

impl<'a> UnitOfWork<'a> {
    pub fn begin(conn: &'a mut Connection) -> Result<Self, StoreError> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Self {
            conn,
            finished: false,
        })
    }

    pub fn commit(mut self) -> Result<(), StoreError> {
        self.conn.execute_batch("COMMIT")?;
        self.finished = true;
        Ok(())
    }

    pub fn rollback(mut self) -> Result<(), StoreError> {
        self.conn.execute_batch("ROLLBACK")?;
        self.finished = true;
        Ok(())
    }
}

/// 列表字段的稳定分隔符（与既有 acceptance 文本一致）。
const ARRAY_SEP: &str = "\u{1f}";

fn split_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(ARRAY_SEP).map(str::to_string).collect()
    }
}

fn denial_reason_str(reason: DenialReason) -> &'static str {
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
}

fn parse_denial_reason(value: String) -> Option<DenialReason> {
    match value.as_str() {
        "InsufficientPermission" => Some(DenialReason::InsufficientPermission),
        "MissingProjectContext" => Some(DenialReason::MissingProjectContext),
        "MissingAssignment" => Some(DenialReason::MissingAssignment),
        "VersionConflict" => Some(DenialReason::VersionConflict),
        "ImmutableDuringRun" => Some(DenialReason::ImmutableDuringRun),
        "WrongWriter" => Some(DenialReason::WrongWriter),
        "ScopeMismatch" => Some(DenialReason::ScopeMismatch),
        "InsufficientBudget" => Some(DenialReason::InsufficientBudget),
        "ExternalOutcomeUnknown" => Some(DenialReason::ExternalOutcomeUnknown),
        _ => None,
    }
}

fn issue_kind_str(kind: IssueKind) -> &'static str {
    match kind {
        IssueKind::Coding => "Coding",
        IssueKind::General => "General",
    }
}

fn owner_kind_str(kind: OwnerKind) -> &'static str {
    match kind {
        OwnerKind::Agent => "Agent",
        OwnerKind::User => "User",
    }
}

/// `issues.owner_kind` 的读侧。只认写入时用的那套大写拼写；
/// 表外取值返回 `None`，由调用方报错而不是猜一个默认值。
fn parse_owner_kind(text: &str) -> Option<OwnerKind> {
    match text {
        "Agent" => Some(OwnerKind::Agent),
        "User" => Some(OwnerKind::User),
        _ => None,
    }
}

fn permission_str(permission: Permission) -> &'static str {
    match permission {
        Permission::Dispatch => "Dispatch",
        Permission::ManageBudget => "ManageBudget",
        Permission::Accept => "Accept",
        Permission::ManageScope => "ManageScope",
    }
}

/// `grants.permission` 的读侧。读不成已知权限说明有人绕过应用写库——
/// 静默当成某个已知权限，等于把越权写成缺陷。
fn parse_permission(text: &str) -> Option<Permission> {
    match text {
        "Dispatch" => Some(Permission::Dispatch),
        "ManageBudget" => Some(Permission::ManageBudget),
        "Accept" => Some(Permission::Accept),
        "ManageScope" => Some(Permission::ManageScope),
        _ => None,
    }
}

fn phase_str(phase: crate::domain::Phase) -> &'static str {
    use crate::domain::Phase;
    match phase {
        Phase::Pending => "Pending",
        Phase::Running => "Running",
        Phase::Blocked => "Blocked",
        Phase::Succeeded => "Succeeded",
        Phase::Cancelled => "Cancelled",
    }
}

fn acceptance_text(acceptance: &AcceptanceCriteria) -> String {
    acceptance.criteria.join("\u{1f}")
}

fn split_acceptance(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\u{1f}').map(str::to_string).collect()
    }
}
