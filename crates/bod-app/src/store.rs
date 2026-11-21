//! SQLite 持久化与 UnitOfWork（P0 D1 任务 2.6）。
//!
//! 边界：`Connection` 由结构体独占，不跨线程共享可变引用；异步层后续用有界写请求队列
//! 把命令送进单一写线程（§37.5），此处先建立事务与条件更新的正确语义。
//! 不在事务内执行网络、Agent 或人工等待。

use crate::audit_log::{AuditDecision, AuditEvent, AuditRecord};
use crate::authorization::StageAction;
use crate::config::BodConfig;
use crate::coverage_receipt::VerificationConclusion;
use crate::delivery_idempotency::{IdempotencyError, Receipt, Replay, ResultKind, classify};
use crate::delivery_source::{
    COMMAND_IMPORT_SOURCE, ImportOutcome, ImportSourceCommand, LinkedSource, SourceError,
    SourceIdentity, SourceResolution,
};
use crate::delivery_tracking::{
    DeliveryError, ObservationKind, ObservationRow, TargetRow, TrackingRow, TrackingView,
    evidence_stamps, project_target, verification_conclusion,
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

/// 有序迁移列表。001 为 D1 领域表，002 为 D2 队列/outbox，003 为 D3 预算，
/// 004 为 D4 执行协议，005 为 D5 成果/修订/接受，006 为 D6 恢复与清理，
/// 007 为 P2 交接快照，008 为 P2 交接事件与接受，009 为 P2 接管意图，
/// 010 为 P2 管理委任/作用域头/命令审计，011 为 P2 检查点与接管确认，
/// 012 为 P2 历史争议与接受五元组冻结，013 为 P2 专项边界层级与组织元数据，
/// 014 为 decisions 不可变 trigger（P2 3.4 加固），015 为业务审计事件（CR-002），
/// 016 为 §11 交付跟踪（P8 3.1：跟踪项/目标/追加式观察）；
/// 016 为 §11 交付跟踪（P8 3.1：跟踪项/目标/追加式观察）；
/// 017 为来源身份（P8 3.2：五元组/候选/追加式修订）；
/// 018 为去重语义（P8 3.3：命令回执/来源关系）。
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
];

/// 已编译进二进制的迁移脚本（文件名 → SQL 正文）。
pub fn migration_sources() -> impl Iterator<Item = (&'static str, &'static str)> {
    MIGRATION_NAMES
        .iter()
        .copied()
        .zip(MIGRATIONS.iter().copied())
}

/// 审计表的读取列顺序。集中一处，避免三处查询各写一份列清单后互相错位。
const AUDIT_SELECT: &str = "SELECT sequence, company_id, correlation_id, actor_id, action,
            scope_company, scope_project, scope_issue, basis, decision, reason,
            decided_at, recorded_at_ms
     FROM audit_events";

/// 审计行映射。表外动作或表外决策一律报错，不静默回退：
/// 库里出现应用不认识的取值，说明有人绕过应用写库，读侧必须炸出来。
fn map_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
    let action_text: String = row.get(4)?;
    let action = StageAction::parse(&action_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("audit_events.action 出现表外值：{action_text}"),
            )),
        )
    })?;
    let decision_text: String = row.get(9)?;
    let decision = AuditDecision::parse(&decision_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            9,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("audit_events.decision 出现表外值：{decision_text}"),
            )),
        )
    })?;
    Ok(AuditRecord {
        sequence: row.get(0)?,
        event: AuditEvent {
            company_id: row.get(1)?,
            correlation_id: row.get(2)?,
            actor: DomainId::new(row.get::<_, String>(3)?),
            action,
            scope: Scope {
                company_id: DomainId::new(row.get::<_, String>(5)?),
                project_id: row.get::<_, Option<String>>(6)?.map(DomainId::new),
                issue_id: row.get::<_, Option<String>>(7)?.map(DomainId::new),
            },
            basis: row.get(8)?,
            decision,
            reason: row.get(10)?,
            decided_at: row.get(11)?,
        },
        recorded_at_ms: row.get(12)?,
    })
}

/// 交付跟踪的读取列顺序。集中一处，避免多处查询各写一份列清单后互相错位。
const TRACKING_SELECT: &str = "SELECT tracking_id, company_id, project_id, title, requirement,
            owner_id, state, issue_uid, run_uid, attempt_id, resource_version,
            created_at_ms, updated_at_ms
     FROM delivery_tracking";

const TARGET_SELECT: &str = "SELECT target_id, company_id, tracking_id, customer_id, environment,
            deployment_unit, acceptance, switch_state, population_known,
            created_at_ms, updated_at_ms
     FROM delivery_targets";

/// 观察读取列。第 17 列是关联表里的来源修订：一条观察最多挂一条修订（3.2）。
/// 用 LEFT JOIN 而不是给 016 的观察表加列——迁移每次打开都会重放，加列不幂等
/// （见 `017_delivery_sources.sql` 的说明）。
const OBSERVATION_SELECT: &str =
    "SELECT o.observation_id, o.company_id, o.tracking_id, o.target_id,
            o.kind, o.source_kind, o.source_ref, o.artifact_revision, o.digest, o.revision,
            o.note, o.actor_id, o.authorized, o.observed_at, o.observed_at_ms,
            o.recorded_at_ms, l.source_revision_id
     FROM delivery_observations o
     LEFT JOIN delivery_observation_sources l
       ON l.observation_id = o.observation_id AND l.company_id = o.company_id";

/// 交付跟踪的读侧取值错误。与审计一致：库里出现表外取值就炸，不静默回退。
fn delivery_row_error(
    index: usize,
    err: crate::delivery_tracking::DeliveryError,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            err.explain(),
        )),
    )
}

fn map_tracking_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackingRow> {
    use crate::delivery_tracking::TrackingState;
    let state_text: String = row.get(6)?;
    let state = TrackingState::parse(&state_text).map_err(|err| delivery_row_error(6, err))?;
    Ok(TrackingRow {
        tracking_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        title: row.get(3)?,
        requirement: row.get(4)?,
        owner_id: row.get(5)?,
        state,
        issue_uid: row.get(7)?,
        run_uid: row.get(8)?,
        attempt_id: row.get(9)?,
        resource_version: row.get(10)?,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
    })
}

fn map_target_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TargetRow> {
    use crate::coverage_receipt::SwitchState;
    use crate::delivery_tracking::DeliveryError;
    let switch_text: String = row.get(7)?;
    let switch_state = match switch_text.as_str() {
        "synced" => SwitchState::Synced,
        "not-synced" => SwitchState::NotSynced,
        "unknown" => SwitchState::Unknown,
        other => {
            return Err(delivery_row_error(
                7,
                DeliveryError::UnknownSwitchState(other.to_string()),
            ));
        }
    };
    Ok(TargetRow {
        target_id: row.get(0)?,
        company_id: row.get(1)?,
        tracking_id: row.get(2)?,
        customer_id: row.get(3)?,
        environment: row.get(4)?,
        deployment_unit: row.get(5)?,
        acceptance: row.get(6)?,
        switch_state,
        population_known: row.get::<_, i64>(8)? != 0,
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
    })
}

fn map_observation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ObservationRow> {
    use crate::delivery_tracking::{ObservationKind, parse_source_kind};
    let kind_text: String = row.get(4)?;
    let kind = ObservationKind::parse(&kind_text).map_err(|err| delivery_row_error(4, err))?;
    let source_text: String = row.get(5)?;
    let source_kind = parse_source_kind(&source_text).map_err(|err| delivery_row_error(5, err))?;
    Ok(ObservationRow {
        observation_id: row.get(0)?,
        company_id: row.get(1)?,
        tracking_id: row.get(2)?,
        target_id: row.get(3)?,
        kind,
        source_kind,
        source_ref: row.get(6)?,
        source_revision_id: row.get(16)?,
        artifact_revision: row.get(7)?,
        digest: row.get(8)?,
        revision: row.get(9)?,
        note: row.get(10)?,
        actor_id: row.get(11)?,
        authorized: row.get::<_, i64>(12)? != 0,
        observed_at: row.get(13)?,
        observed_at_ms: row.get(14)?,
        recorded_at_ms: row.get(15)?,
    })
}

// --- 来源身份（P8 3.2）------------------------------------------------------

const SOURCE_SELECT: &str = "SELECT source_id, company_id, project_id, connection_id,
            object_type, provider_object_id, locator, confirmed_by, confirmed_at_ms, created_at_ms
     FROM delivery_sources";

const SOURCE_REVISION_SELECT: &str = "SELECT source_revision_id, company_id, source_id, version,
            digest, content_fingerprint, actor_id, observed_at, observed_at_ms, recorded_at_ms
     FROM delivery_source_revisions";

const SOURCE_CANDIDATE_SELECT: &str = "SELECT candidate_id, company_id, project_id, connection_id,
            object_type, locator, proposed_by, proposed_at_ms, state, resolved_source_id,
            decided_by, decided_at_ms
     FROM delivery_source_candidates";

fn map_source_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<crate::delivery_source::SourceRow> {
    Ok(crate::delivery_source::SourceRow {
        source_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        connection_id: row.get(3)?,
        object_type: row.get(4)?,
        provider_object_id: row.get(5)?,
        locator: row.get(6)?,
        confirmed_by: row.get(7)?,
        confirmed_at_ms: row.get(8)?,
        created_at_ms: row.get(9)?,
    })
}

fn map_source_revision_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<crate::delivery_source::SourceRevisionRow> {
    Ok(crate::delivery_source::SourceRevisionRow {
        source_revision_id: row.get(0)?,
        company_id: row.get(1)?,
        source_id: row.get(2)?,
        version: row.get(3)?,
        digest: row.get(4)?,
        content_fingerprint: row.get(5)?,
        actor_id: row.get(6)?,
        observed_at: row.get(7)?,
        observed_at_ms: row.get(8)?,
        recorded_at_ms: row.get(9)?,
    })
}

fn map_source_candidate_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<crate::delivery_source::CandidateRow> {
    use crate::delivery_source::CandidateState;
    let state_text: String = row.get(8)?;
    let state = CandidateState::parse(&state_text).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                err.explain(),
            )),
        )
    })?;
    Ok(crate::delivery_source::CandidateRow {
        candidate_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        connection_id: row.get(3)?,
        object_type: row.get(4)?,
        locator: row.get(5)?,
        proposed_by: row.get(6)?,
        proposed_at_ms: row.get(7)?,
        state,
        resolved_source_id: row.get(9)?,
        decided_by: row.get(10)?,
        decided_at_ms: row.get(11)?,
    })
}

/// 回执读取列顺序。集中一处，读写两侧不会错位。
const RECEIPT_SELECT: &str = "SELECT company_id, idempotency_key, command, payload_fingerprint,
            result_kind, result_id, result_detail, recorded_at_ms
     FROM delivery_command_receipts";

/// 回执行映射。结果种类出现表外值一律报错：把它当成 tracking 会让重放返回一个
/// 类型错误的结果，而调用方会照着用。
fn map_receipt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Receipt> {
    let kind_text: String = row.get(4)?;
    let result_kind = ResultKind::parse(&kind_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("delivery_command_receipts.result_kind 出现表外值：{kind_text}"),
            )),
        )
    })?;
    Ok(Receipt {
        company_id: row.get(0)?,
        idempotency_key: row.get(1)?,
        command: row.get(2)?,
        payload_fingerprint: row.get(3)?,
        result_kind,
        result_id: row.get(5)?,
        result_detail: row.get(6)?,
        recorded_at_ms: row.get(7)?,
    })
}

/// 按 (公司, 幂等键) 读回执。裸 `Connection` 版本，供 `SqliteStore` 与 `UnitOfWork` 共用——
/// 两处各写一份 SQL 是列顺序漂移的老路。
fn find_receipt(
    conn: &rusqlite::Connection,
    company_id: &str,
    idempotency_key: &str,
) -> rusqlite::Result<Option<Receipt>> {
    conn.query_row(
        &format!("{RECEIPT_SELECT} WHERE company_id = ?1 AND idempotency_key = ?2"),
        params![company_id, idempotency_key.trim()],
        map_receipt_row,
    )
    .optional()
}

/// 来源 → 跟踪项。返回 `None` 表示这个身份还没挂任何交付。
fn lookup_tracking_for_source(
    conn: &rusqlite::Connection,
    company_id: &str,
    source_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT tracking_id FROM delivery_tracking_sources
         WHERE company_id = ?1 AND source_id = ?2",
        params![company_id, source_id],
        |row| row.get(0),
    )
    .optional()
}

/// 唯一约束冲突：并发下「先查后插」输的那一方，靠它改成返回已有行。
fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// 按五元组查身份。查不到返回 `None`；**不做模糊匹配**——那会让两个来源悄悄合并。
fn find_source_id(
    conn: &rusqlite::Connection,
    identity: &crate::delivery_source::SourceIdentity,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT source_id FROM delivery_sources
         WHERE company_id = ?1 AND project_id = ?2 AND connection_id = ?3
           AND object_type = ?4 AND provider_object_id = ?5",
        params![
            identity.company_id,
            identity.project_id,
            identity.connection_id,
            identity.object_type,
            identity.provider_object_id,
        ],
        |row| row.get(0),
    )
    .optional()
}

/// 修订正文事实（P2 3.4）：历史连续比对要覆盖摘要与大小，不能只看产出者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionBodyRow {
    pub revision_id: String,
    pub artifact_id: String,
    pub produced_by: String,
    pub digest: String,
    pub size_bytes: i64,
    pub mime: String,
    pub created_at_ms: i64,
}

/// 正式 Manifest 事实（P2 3.4）：冻结的是引用集合，不含正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestBodyRow {
    pub uid: String,
    pub revision: i64,
    pub milestone: String,
    pub artifact_revision_refs: String,
    pub acceptance_refs: String,
    pub signature_ref: Option<String>,
    pub created_at_ms: i64,
}

/// 决定事实（P2 3.4）：旧经理的决定不会因换人消失或改口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionBodyRow {
    pub decision_id: String,
    pub actor_id: String,
    pub decision: String,
    pub reason: String,
    pub evidence_refs: String,
    pub revision_refs: String,
    pub created_at_ms: i64,
}

/// 争议行（`history_disputes`）：只标待核查，不改被争议的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputeRow {
    pub dispute_id: String,
    pub company_id: String,
    pub project_id: String,
    pub fact_kind: String,
    pub fact_id: String,
    pub fact_fingerprint: String,
    pub reason: String,
    pub evidence_refs: String,
    pub raised_by: String,
    pub state: String,
    pub resolution: Option<String>,
    pub resolved_by: Option<String>,
    pub resolved_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// 专项边界层级（P2 3.5）：只登记父子，不改 `issues` 表与域对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyRow {
    pub issue_uid: String,
    pub parent_issue_uid: String,
}

/// 租约事实（P2 3.5）：组织元数据变更前后逐字段比对，证明没有 fencing/迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseFactRow {
    pub resource_key: String,
    pub owner: String,
    pub lease_token: i64,
    pub expires_at: Option<String>,
    pub state: String,
}

/// 授权事实（P2 3.5）：reportsTo 变更不得改动任何一条 grant。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFactRow {
    pub principal_id: String,
    pub project_id: Option<String>,
    pub permission: String,
    pub revision: i64,
    pub active: i64,
}

/// 组织元数据行（`agent_org_metadata`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgMetadataRow {
    pub agent_id: String,
    pub reports_to: Option<String>,
    pub updated_at_ms: i64,
    pub updated_by: String,
}

/// 登记来源身份的结果。`created=false` 表示五元组已存在，返回的是已有身份——
/// 调用方据此区分「新认识的来源」与「重复导入同一对象」（3.3 的去重语义靠它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSource {
    pub source_id: String,
    pub created: bool,
}

/// 提交来源候选的结果。`created=false` 表示已有同一作用域同一 locator 的待确认候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedCandidate {
    pub candidate_id: String,
    pub created: bool,
}

/// 确认候选的结果。`source_created` 说明这个身份是不是本次确认才建立的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedCandidate {
    pub candidate_id: String,
    pub source_id: String,
    pub source_created: bool,
}

/// 追加来源修订的结果。`created=false` 表示同一事件已确认过，返回的是已有修订。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendedRevision {
    pub source_revision_id: String,
    pub created: bool,
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

/// 单写连接持有者。写事务通过 `UnitOfWork` 显式开始。
pub struct SqliteStore {
    conn: Connection,
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

    /// 列出某 Company 的队列行；供重启扫描未终结资源与 doctor 指标。
    pub fn list_queue(&self, company_id: &str) -> Result<Vec<WorkQueueEntry>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT company_id, queue_key, state, wanted_generation, claimed_generation,
                    completed_generation, next_due_at_ms, attempts, lease_owner, lease_token,
                    lease_expires_at_ms, last_error, blocked_reason
             FROM work_queue WHERE company_id = ?1",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok(WorkQueueEntry {
                company_id: DomainId::new(row.get::<_, String>(0)?),
                queue_key: row.get(1)?,
                state: QueueState::parse(&row.get::<_, String>(2)?).unwrap_or(QueueState::Idle),
                wanted_generation: row.get::<_, i64>(3)? as u64,
                claimed_generation: row.get::<_, i64>(4)? as u64,
                completed_generation: row.get::<_, i64>(5)? as u64,
                next_due_at_ms: row.get(6)?,
                attempts: row.get::<_, i64>(7)? as u32,
                lease_owner: row.get(8)?,
                lease_token: row.get::<_, i64>(9)? as u64,
                lease_expires_at_ms: row.get(10)?,
                last_error: row.get(11)?,
                blocked_reason: row
                    .get::<_, Option<String>>(12)?
                    .and_then(parse_denial_reason),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 统计某 Company 的 Attempt 行数；供取消对账断言"不新建 Attempt"。
    pub fn count_attempts(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM attempts WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 按关联 ID 读一条审计链（CR-002）。顺序由自增序号决定：
    /// 同一毫秒内可能写入多条，按时间戳排会得到不稳定顺序。
    pub fn list_audit_events(
        &self,
        company_id: &str,
        correlation_id: &str,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{AUDIT_SELECT} WHERE company_id = ?1 AND correlation_id = ?2 ORDER BY sequence ASC"
        ))?;
        let rows = stmt.query_map(params![company_id, correlation_id], map_audit_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 按决策计数：§9 的拒绝率与审计分布都从这里出。
    pub fn count_audit_events(
        &self,
        company_id: &str,
        decision: AuditDecision,
    ) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE company_id = ?1 AND decision = ?2",
                params![company_id, decision.as_str()],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 最近的审计行（序号降序）。`limit` 由调用方给，不设默认值——
    /// 一个「顺手全表读」的默认值会在审计表变大后变成事故。
    pub fn latest_audit_events(
        &self,
        company_id: &str,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{AUDIT_SELECT} WHERE company_id = ?1 ORDER BY sequence DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![company_id, limit as i64], map_audit_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 读一个跟踪项的完整视图（P8 3.1）：跟踪项 + 目标 + 观察 + 各目标的验证结论。
    ///
    /// 查询条数固定（4 条），不随目标数量增长：观察与验证记录一次 JOIN 取回，
    /// 再在内存里按目标归并。**不写库**：读路径不产生审计、不改任何状态。
    ///
    /// 覆盖结论在这里算出（`project_target` → `judge_coverage`），不从表里读——
    /// 库里只有事实，结论永远由同一处判定产生（design D1）。
    pub fn delivery_tracking_view(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<Option<TrackingView>, StoreError> {
        let tracking = self
            .conn
            .query_row(
                &format!("{TRACKING_SELECT} WHERE company_id = ?1 AND tracking_id = ?2"),
                params![company_id, tracking_id],
                map_tracking_row,
            )
            .optional()?;
        let Some(tracking) = tracking else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(&format!(
            "{TARGET_SELECT} WHERE company_id = ?1 AND tracking_id = ?2
             ORDER BY customer_id ASC, environment ASC, deployment_unit ASC"
        ))?;
        let targets = stmt
            .query_map(params![company_id, tracking_id], map_target_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut stmt = self.conn.prepare(&format!(
            "{OBSERVATION_SELECT} WHERE o.company_id = ?1 AND o.tracking_id = ?2
             ORDER BY o.observed_at_ms DESC, o.recorded_at_ms DESC, o.observation_id DESC"
        ))?;
        let observations = stmt
            .query_map(params![company_id, tracking_id], map_observation_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let conclusions = self.delivery_verification_conclusions(company_id, tracking_id)?;
        let mut projected = Vec::with_capacity(targets.len());
        for target in &targets {
            // 克隆只为让投影不与连接共享借用；条数按目标计，不做全表扫描。
            let related: Vec<ObservationRow> = observations
                .iter()
                .filter(|observation| observation.target_id == target.target_id)
                .cloned()
                .collect();
            let conclusion = conclusions
                .get(&target.target_id)
                .copied()
                .unwrap_or(VerificationConclusion::Unknown);
            projected.push(project_target(
                &tracking.project_id,
                target,
                &related,
                conclusion,
            ));
        }

        let evidence = evidence_stamps(&observations);
        Ok(Some(TrackingView {
            tracking,
            targets: projected,
            evidence,
        }))
    }

    /// 列出某 Company 的跟踪项与目标数量。
    ///
    /// 列表刻意**不带覆盖结论**：结论离开证据没有意义，而列表里塞不下证据；
    /// 列出来就会被当成可以引用的结论。要结论请读单个跟踪项。
    pub fn list_delivery_tracking(
        &self,
        company_id: &str,
        limit: usize,
    ) -> Result<Vec<(TrackingRow, i64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT t.tracking_id, t.company_id, t.project_id, t.title, t.requirement,
                    t.owner_id, t.state, t.issue_uid, t.run_uid, t.attempt_id,
                    t.resource_version, t.created_at_ms, t.updated_at_ms,
                    COUNT(g.target_id)
             FROM delivery_tracking t
             LEFT JOIN delivery_targets g
               ON g.company_id = t.company_id AND g.tracking_id = t.tracking_id
             WHERE t.company_id = ?1
             GROUP BY t.tracking_id
             ORDER BY t.created_at_ms DESC, t.tracking_id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![company_id, limit as i64], |row| {
            Ok((map_tracking_row(row)?, row.get::<_, i64>(13)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    // --- 来源身份与候选（P8 3.2，只读）--------------------------------------

    /// 读一条命令回执（P8 3.3）。查不到返回 `None`，调用方据此判定「首次执行」。
    ///
    /// 只读：回执不可改写（库层触发器也拦），因为重放必须返回**当时**的结果。
    pub fn find_command_receipt(
        &self,
        company_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<Receipt>, StoreError> {
        find_receipt(&self.conn, company_id, idempotency_key).map_err(StoreError::Sql)
    }

    /// 读一条来源关系（P8 3.3）：这个身份承载哪一条跟踪项。
    ///
    /// 查不到返回 `None`：来源可能刚登记、还没被任何交付引用。
    pub fn find_tracking_for_source(
        &self,
        company_id: &str,
        source_id: &str,
    ) -> Result<Option<String>, StoreError> {
        lookup_tracking_for_source(&self.conn, company_id, source_id).map_err(StoreError::Sql)
    }

    /// 读一个来源身份。查不到返回 `None`——「不知道」与「没有」要分开。
    pub fn get_source(
        &self,
        company_id: &str,
        source_id: &str,
    ) -> Result<Option<crate::delivery_source::SourceRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{SOURCE_SELECT} WHERE company_id = ?1 AND source_id = ?2"),
                params![company_id, source_id],
                map_source_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 列出已确认的来源身份与各自的修订条数。
    ///
    /// 不返回「哪一版是最新」：那要比较版本语义（commit 序、语义化版本），
    /// 不同连接器的比较规则不同，本段不猜。时间线请读单个来源。
    pub fn list_sources(
        &self,
        company_id: &str,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(crate::delivery_source::SourceRow, i64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.source_id, s.company_id, s.project_id, s.connection_id,
                    s.object_type, s.provider_object_id, s.locator, s.confirmed_by,
                    s.confirmed_at_ms, s.created_at_ms, COUNT(r.source_revision_id)
             FROM delivery_sources s
             LEFT JOIN delivery_source_revisions r
               ON r.company_id = s.company_id AND r.source_id = s.source_id
             WHERE s.company_id = ?1 AND (?2 IS NULL OR s.project_id = ?2)
             GROUP BY s.source_id
             ORDER BY s.created_at_ms DESC, s.source_id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![company_id, project_id, limit as i64], |row| {
            Ok((map_source_row(row)?, row.get::<_, i64>(10)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 一个来源的修订时间线，**按观察时间升序**（可变标签要看前后顺序）。
    pub fn list_source_revisions(
        &self,
        company_id: &str,
        source_id: &str,
    ) -> Result<Vec<crate::delivery_source::SourceRevisionRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SOURCE_REVISION_SELECT} WHERE company_id = ?1 AND source_id = ?2
             ORDER BY observed_at_ms ASC, recorded_at_ms ASC, source_revision_id ASC"
        ))?;
        let rows = stmt.query_map(params![company_id, source_id], map_source_revision_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出来源候选。`pending_only` 时只给待确认的——它们才是需要动作的那一类。
    pub fn list_source_candidates(
        &self,
        company_id: &str,
        project_id: Option<&str>,
        pending_only: bool,
        limit: usize,
    ) -> Result<Vec<crate::delivery_source::CandidateRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SOURCE_CANDIDATE_SELECT}
             WHERE company_id = ?1 AND (?2 IS NULL OR project_id = ?2)
               AND (?3 = 0 OR state = 'pending')
             ORDER BY CASE state WHEN 'pending' THEN 0 ELSE 1 END,
                      proposed_at_ms DESC, candidate_id ASC
             LIMIT ?4"
        ))?;
        let rows = stmt.query_map(
            params![company_id, project_id, pending_only as i64, limit as i64],
            map_source_candidate_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 每个目标的验证结论：取「最近一条带制品修订的观察」对应的最新验证记录。
    ///
    /// 没带修订的观察不参与——「没给修订」不是否定证据，不该把已有的验证结论抹掉。
    /// `waived` 仍映射为 `Unknown`（见 `verification_conclusion`）。
    fn delivery_verification_conclusions(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<std::collections::HashMap<String, VerificationConclusion>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT o.target_id, v.state
             FROM delivery_observations o
             JOIN verification_records v
               ON v.company_id = o.company_id AND v.revision_id = o.artifact_revision
             WHERE o.company_id = ?1 AND o.tracking_id = ?2
             ORDER BY o.observed_at_ms DESC, o.recorded_at_ms DESC, v.observed_at_ms DESC",
        )?;
        let rows = stmt.query_map(params![company_id, tracking_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut conclusions = std::collections::HashMap::new();
        for row in rows {
            let (target_id, state) = row?;
            if conclusions.contains_key(&target_id) {
                continue;
            }
            conclusions.insert(target_id, verification_conclusion(&state)?);
        }
        Ok(conclusions)
    }

    /// 只读列出 (Issue uid, 工作包修订) 关联，供工作台把首次使用的交付证据归属到任务页。
    pub fn list_work_package_issue_links(
        &self,
        company_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_uid, revision_id FROM work_package_revisions WHERE company_id = ?1
             ORDER BY issue_uid ASC, revision_id ASC",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 统计工作包修订行数（首次使用断言「从零自动补齐」）。
    pub fn count_work_package_revisions(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM work_package_revisions WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 统计运行实例行数，语义同 [`Self::count_work_package_revisions`]。
    pub fn count_runs(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 读取某聚合最新事件载荷；取消对账只从宿主事件推导状态，不另建状态库。
    pub fn latest_event_payload(&self, aggregate_id: &str) -> Result<Option<String>, StoreError> {
        self.conn.query_row(
            "SELECT payload FROM events WHERE aggregate_id = ?1 ORDER BY aggregate_version DESC LIMIT 1",
            params![aggregate_id],
            |row| row.get(0),
        ).optional().map_err(StoreError::Sql)
    }

    /// 统计某聚合的事件数，供事件版本单调递增。
    pub fn count_events(&self, aggregate_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE aggregate_id = ?1",
                params![aggregate_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 列出某 Company 的预留行；供预算解释与测试断言。
    pub fn list_reservations(&self, company_id: &str) -> Result<Vec<Reservation>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT reservation_id, company_id, project_id, agent_id, run_id, work_package_id,
                    dimension, currency, reserved_value, confirmed_value, risk_value, state,
                    logical_key, created_at_ms, updated_at_ms
             FROM reservations WHERE company_id = ?1 ORDER BY created_at_ms, reservation_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let dimension = Dimension::parse(&row.get::<_, String>(6)?).unwrap_or(Dimension::Calls);
            let currency: String = row.get(7)?;
            Ok(Reservation {
                reservation_id: row.get(0)?,
                company_id: DomainId::new(row.get::<_, String>(1)?),
                project_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                agent_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                run_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                work_package_id: row.get::<_, Option<String>>(5)?.map(DomainId::new),
                measure: Measure {
                    dimension,
                    value: 0,
                    currency: if currency.is_empty() {
                        None
                    } else {
                        Some(currency)
                    },
                },
                reserved: row.get(8)?,
                confirmed: row.get(9)?,
                risk: row.get(10)?,
                state: ReservationState::parse(&row.get::<_, String>(11)?)
                    .unwrap_or(ReservationState::Cancelled),
                logical_key: row.get(12)?,
                created_at_ms: row.get(13)?,
                updated_at_ms: row.get(14)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出某 Company 的全部账本分录（只读；账本不可改写）。
    pub fn list_ledger_entries(&self, company_id: &str) -> Result<Vec<LedgerEntry>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT entry_id, company_id, project_id, agent_id, work_package_id, reservation_id,
                    usage_key, entry_kind, dimension, currency, amount, source, reason, created_at_ms
             FROM ledger_entries WHERE company_id = ?1 ORDER BY created_at_ms, entry_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let dimension = Dimension::parse(&row.get::<_, String>(8)?).unwrap_or(Dimension::Calls);
            let currency: String = row.get(9)?;
            Ok(LedgerEntry {
                entry_id: row.get(0)?,
                company_id: DomainId::new(row.get::<_, String>(1)?),
                project_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                agent_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                work_package_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                reservation_id: row.get(5)?,
                usage_key: row.get(6)?,
                kind: LedgerEntryKind::parse(&row.get::<_, String>(7)?)
                    .unwrap_or(LedgerEntryKind::Adjustment),
                measure: Measure {
                    dimension,
                    value: 0,
                    currency: if currency.is_empty() {
                        None
                    } else {
                        Some(currency)
                    },
                },
                amount: row.get(10)?,
                source: row.get(11)?,
                reason: row.get(12)?,
                created_at_ms: row.get(13)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出指定状态的 outbox 意图；供提交后派发与重启恢复扫描。
    pub fn list_outbox_by_status(
        &self,
        status: EffectStatus,
    ) -> Result<Vec<OutboxDelivery>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT event_or_command_id, destination_id, company_id, queue_key, request_digest,
                    status, attempts, next_due_at_ms, result_ref, created_at_ms
             FROM outbox_deliveries WHERE status = ?1",
        )?;
        let rows = stmt.query_map(params![status.as_str()], |row| {
            Ok(OutboxDelivery {
                event_or_command_id: row.get(0)?,
                destination_id: row.get(1)?,
                company_id: DomainId::new(row.get::<_, String>(2)?),
                queue_key: row.get(3)?,
                request_digest: row.get(4)?,
                status: EffectStatus::parse(&row.get::<_, String>(5)?)
                    .unwrap_or(EffectStatus::Unknown),
                attempts: row.get::<_, i64>(6)? as u32,
                next_due_at_ms: row.get(7)?,
                result_ref: row.get(8)?,
                created_at_ms: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出某 Company 的 invocation；供重启扫描未终结执行与对账。
    pub fn list_invocations(&self, company_id: &str) -> Result<Vec<InvocationRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT invocation_id, company_id, run_id, work_package_revision, attempt_id,
                    binding_id, fencing_token, adapter_id, adapter_version, params_canonical,
                    params_digest, execution_id, state, created_at_ms, accepted_at_ms,
                    last_observed_at_ms, retention_until_ms
             FROM invocations WHERE company_id = ?1 ORDER BY created_at_ms",
        )?;
        let rows = statement.query_map(params![company_id], |row| {
            let state_text: String = row.get(12)?;
            let fencing: i64 = row.get(6)?;
            Ok(InvocationRecord {
                invocation_id: row.get(0)?,
                company_id: row.get(1)?,
                run_id: row.get(2)?,
                work_package_revision: row.get(3)?,
                attempt_id: row.get(4)?,
                binding_id: row.get(5)?,
                fencing_token: fencing.max(0) as u64,
                adapter_id: row.get(7)?,
                adapter_version: row.get(8)?,
                params_canonical: row.get(9)?,
                params_digest: row.get(10)?,
                execution_id: row.get(11)?,
                state: ExecutionState::parse(&state_text).unwrap_or(ExecutionState::Unknown),
                created_at_ms: row.get(13)?,
                accepted_at_ms: row.get(14)?,
                last_observed_at_ms: row.get(15)?,
                retention_until_ms: row.get(16)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 按外部执行 ID 反查：对账时用它避免重复创建执行。
    pub fn find_invocation_by_execution_id(
        &self,
        execution_id: &str,
    ) -> Result<Option<InvocationRecord>, StoreError> {
        let invocation_id: Option<String> = self
            .conn
            .query_row(
                "SELECT invocation_id FROM invocations WHERE execution_id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?;
        match invocation_id {
            Some(id) => self.read_invocation(&id),
            None => Ok(None),
        }
    }

    fn read_invocation(&self, invocation_id: &str) -> Result<Option<InvocationRecord>, StoreError> {
        // 只读路径：借用同一查询逻辑，避免复制列映射。
        let mut statement = self.conn.prepare(
            "SELECT invocation_id, company_id, run_id, work_package_revision, attempt_id,
                    binding_id, fencing_token, adapter_id, adapter_version, params_canonical,
                    params_digest, execution_id, state, created_at_ms, accepted_at_ms,
                    last_observed_at_ms, retention_until_ms
             FROM invocations WHERE invocation_id = ?1",
        )?;
        let mut rows = statement.query(params![invocation_id])?;
        match rows.next()? {
            Some(row) => {
                let state_text: String = row.get(12)?;
                let fencing: i64 = row.get(6)?;
                Ok(Some(InvocationRecord {
                    invocation_id: row.get(0)?,
                    company_id: row.get(1)?,
                    run_id: row.get(2)?,
                    work_package_revision: row.get(3)?,
                    attempt_id: row.get(4)?,
                    binding_id: row.get(5)?,
                    fencing_token: fencing.max(0) as u64,
                    adapter_id: row.get(7)?,
                    adapter_version: row.get(8)?,
                    params_canonical: row.get(9)?,
                    params_digest: row.get(10)?,
                    execution_id: row.get(11)?,
                    state: ExecutionState::parse(&state_text).unwrap_or(ExecutionState::Unknown),
                    created_at_ms: row.get(13)?,
                    accepted_at_ms: row.get(14)?,
                    last_observed_at_ms: row.get(15)?,
                    retention_until_ms: row.get(16)?,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn count_observations(&self, invocation_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE invocation_id = ?1",
                params![invocation_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    pub fn count_start_audits(&self, invocation_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM start_audits WHERE invocation_id = ?1",
                params![invocation_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 支持矩阵读取：只返回落库的声明，不补默认值，Fake Executor 不在其中。
    pub fn list_support_matrix(
        &self,
        adapter_id: &str,
        adapter_version: &str,
    ) -> Result<Vec<MatrixRow>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT adapter_id, adapter_version, capability, level, evidence_level, source,
                    reason, observed_at_ms
             FROM adapter_capabilities
             WHERE adapter_id = ?1 AND adapter_version = ?2
             ORDER BY capability",
        )?;
        let rows = statement.query_map(params![adapter_id, adapter_version], |row| {
            let level: String = row.get(3)?;
            let evidence: String = row.get(4)?;
            let source: String = row.get(5)?;
            Ok(MatrixRow {
                adapter_id: row.get(0)?,
                adapter_version: row.get(1)?,
                capability: row.get(2)?,
                level: SupportLevel::parse(&level),
                mode: None,
                evidence_level: EvidenceLevel::parse(&evidence)
                    .unwrap_or(EvidenceLevel::SelfDeclared),
                source: EvidenceSource::parse(&source)
                    .unwrap_or(EvidenceSource::AdapterSelfDeclared),
                observed_at_ms: row.get(7)?,
                limitations: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }
    // -----------------------------------------------------------------------
    // D6 迁移登记、备份、对账与清理（任务 7.1—7.4）
    // -----------------------------------------------------------------------

    pub fn list_migration_records(&self) -> Result<Vec<MigrationRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT migration_id, checksum, depends_on, schema_range, software_range, risk,
                    compatibility_window, backup_notes, data_notes, applied_at_ms
             FROM migration_registry ORDER BY migration_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let depends: String = row.get(2)?;
            Ok(MigrationRecord {
                migration_id: row.get(0)?,
                checksum: row.get(1)?,
                depends_on: MigrationRecord::parse_depends(&depends),
                schema_range: row.get(3)?,
                software_range: row.get(4)?,
                risk: MigrationRisk::parse(&row.get::<_, String>(5)?),
                compatibility_window: row.get(6)?,
                backup_notes: row.get(7)?,
                data_notes: row.get(8)?,
                applied_at_ms: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 一致性快照：VACUUM INTO 生成独立数据库文件，已提交的 WAL 内容被合并。
    pub fn vacuum_into(&self, path: &str) -> Result<(), StoreError> {
        self.conn
            .execute("VACUUM INTO ?1", params![path])
            .map_err(StoreError::Sql)?;
        Ok(())
    }

    /// 供恢复报告使用的核心行数（不读取业务内容）。
    pub fn core_row_counts(&self) -> Result<CoreRowCounts, StoreError> {
        let issues = self.count_table("issues")?;
        let revisions = self.count_table("artifact_revisions")?;
        let invocations = self.count_table("invocations")?;
        let acceptances = self.count_table("acceptances")?;
        Ok(CoreRowCounts {
            issues,
            revisions,
            invocations,
            acceptances,
        })
    }

    fn count_table(&self, table: &str) -> Result<i64, StoreError> {
        // 表名来自本函数的固定字面量，不接受外部输入。
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(StoreError::Sql)
    }

    pub fn latest_backup(&self, company_id: &str) -> Result<Option<BackupRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT backup_id, path, db_sha256, db_bytes, wal_included, snapshot_method,
                        cas_object_count, config_revision, adapter_versions, key_recovery_note,
                        status, problems, created_at_ms
                 FROM backups WHERE company_id = ?1 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                params![company_id],
                |row| {
                    let problems: String = row.get(11)?;
                    Ok(BackupRecord {
                        backup_id: row.get(0)?,
                        company_id: DomainId::new(company_id.to_string()),
                        path: row.get(1)?,
                        db_sha256: row.get(2)?,
                        db_bytes: row.get(3)?,
                        wal_included: row.get::<_, i64>(4)? == 1,
                        snapshot_method: row.get(5)?,
                        cas_object_count: row.get(6)?,
                        config_revision: row.get(7)?,
                        adapter_versions: row.get(8)?,
                        key_recovery_note: row.get(9)?,
                        status: BackupStatus::parse(&row.get::<_, String>(10)?),
                        problems: split_list(&problems),
                        created_at_ms: row.get(12)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 冻结一组具体修订（§21.4）。本切片只写入，不改写已有 Manifest 修订。
    // 参数多但都是 Manifest 的必填事实；拆结构体会掩盖"哪些字段来自接受记录"。
    #[allow(clippy::too_many_arguments)]
    pub fn put_manifest(
        &self,
        company_id: &str,
        manifest_id: &str,
        project_id: &str,
        revision: i64,
        artifact_revision_refs: &[String],
        acceptance_refs: &[String],
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO manifests (
                uid, company_id, project_id, revision, milestone,
                artifact_revision_refs, acceptance_refs, signature_ref, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, NULL, ?7)",
            params![
                manifest_id,
                company_id,
                project_id,
                revision,
                artifact_revision_refs.join(ARRAY_SEP),
                acceptance_refs.join(ARRAY_SEP),
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// 引用该修订的正式 Manifest：用分隔符包裹后精确匹配，避免子串误判。
    pub fn manifest_referencing(&self, artifact_revision: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid FROM manifests
             WHERE instr(?2 || artifact_revision_refs || ?2, ?2 || ?1 || ?2) > 0",
        )?;
        let rows = stmt.query_map(params![artifact_revision, ARRAY_SEP], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出项目下的 Issue（工作台投影只读来源，不改状态、不派发）。
    pub fn list_issues_for_project(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<Issue>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, company_id, project_id, kind, repo_provider, repo_id, title, acceptance,
                    owner_id, owner_kind, assignment_grant_revision, resource_version,
                    generation, observed_generation, phase
             FROM issues WHERE company_id = ?1 AND project_id = ?2 ORDER BY uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            let uid: String = row.get(0)?;
            let company_id = DomainId::new(row.get::<_, String>(1)?);
            let project_id = row.get::<_, Option<String>>(2)?.map(DomainId::new);
            let kind = match row.get::<_, String>(3)?.as_str() {
                "Coding" => IssueKind::Coding,
                _ => IssueKind::General,
            };
            let provider: Option<String> = row.get(4)?;
            let repository_id: Option<String> = row.get(5)?;
            let repository = match (provider, repository_id) {
                (Some(provider), Some(repository_id)) => Some(RepositoryContext {
                    provider,
                    repository_id,
                }),
                _ => None,
            };
            let acceptance: String = row.get(7)?;
            let owner_id: Option<String> = row.get(8)?;
            let owner_kind: Option<String> = row.get(9)?;
            let grant_revision: Option<i64> = row.get(10)?;
            let assignment = match (owner_id, owner_kind) {
                (Some(owner_id), Some(owner_kind)) => Some(Assignment {
                    owner: Owner {
                        kind: if owner_kind == "User" {
                            OwnerKind::User
                        } else {
                            OwnerKind::Agent
                        },
                        id: DomainId::new(owner_id),
                    },
                    scope: Scope {
                        company_id: company_id.clone(),
                        project_id: project_id.clone(),
                        issue_id: Some(DomainId::new(uid.clone())),
                    },
                    grant_revision: grant_revision.unwrap_or(0) as u64,
                }),
                _ => None,
            };
            let phase = match row.get::<_, String>(14)?.as_str() {
                "Running" => Phase::Running,
                "Blocked" => Phase::Blocked,
                "Succeeded" => Phase::Succeeded,
                "Cancelled" => Phase::Cancelled,
                _ => Phase::Pending,
            };
            Ok(Issue {
                meta: ResourceMeta {
                    uid: DomainId::new(uid),
                    company_id,
                    project_id,
                    resource_version: Version(row.get::<_, i64>(11)? as u64),
                    generation: row.get::<_, i64>(12)? as u64,
                    deletion_requested_at: None,
                },
                kind,
                repository,
                spec: IssueSpec {
                    title: row.get(6)?,
                    assignment,
                    acceptance: AcceptanceCriteria {
                        criteria: split_acceptance(&acceptance),
                    },
                },
                status: ResourceStatus {
                    phase,
                    observed_generation: row.get::<_, i64>(13)? as u64,
                    conditions: Vec::new(),
                },
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出公司下每个成果修订的最新验证记录（任务页证据视图只读来源）。
    pub fn list_verifications_for_company(
        &self,
        company_id: &str,
    ) -> Result<Vec<VerificationRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT v.revision_id, v.state, v.actor_id, v.reason, v.evidence_refs,
                    v.verification_rules, v.environment, v.observed_at_ms
             FROM verification_records v
             WHERE v.company_id = ?1
               AND v.observed_at_ms = (
                   SELECT MAX(v2.observed_at_ms) FROM verification_records v2
                   WHERE v2.revision_id = v.revision_id
               )
             ORDER BY v.revision_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let refs: String = row.get(4)?;
            Ok(VerificationRecord {
                revision_id: DomainId::new(row.get::<_, String>(0)?),
                state: VerificationState::parse(&row.get::<_, String>(1)?)
                    .unwrap_or(VerificationState::Pending),
                actor: DomainId::new(row.get::<_, String>(2)?),
                reason: row.get(3)?,
                evidence_refs: split_list(&refs),
                verification_rules: DomainId::new(row.get::<_, String>(5)?),
                environment: DomainId::new(row.get::<_, String>(6)?),
                observed_at_ms: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出公司下「修订 → 成果」的只读关联（任务页证据归属来源）。
    pub fn list_revision_artifact_links(
        &self,
        company_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revision_id, artifact_id FROM artifact_revisions
             WHERE company_id = ?1 ORDER BY revision_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出公司下的接受记录（工作台投影只读来源，不改状态、不派发）。
    pub fn list_acceptances_for_company(
        &self,
        company_id: &str,
    ) -> Result<Vec<AcceptanceRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT acceptance_id, artifact_id, work_package_revision, input_baseline,
                    artifact_revision, verification_rules, target_environment, merge_combination,
                    decision, actor_id, reason, evidence_refs, accepted_at_ms, superseded_by
             FROM acceptances WHERE company_id = ?1
             ORDER BY accepted_at_ms ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let decision = AcceptanceDecision::parse(&row.get::<_, String>(8)?)
                .unwrap_or(AcceptanceDecision::RequiresHuman);
            let evidence: String = row.get(11)?;
            Ok(AcceptanceRecord {
                acceptance_id: DomainId::new(row.get::<_, String>(0)?),
                artifact_id: DomainId::new(row.get::<_, String>(1)?),
                binding: AcceptanceBinding {
                    work_package_revision: DomainId::new(row.get::<_, String>(2)?),
                    input_baseline: DomainId::new(row.get::<_, String>(3)?),
                    artifact_revision: DomainId::new(row.get::<_, String>(4)?),
                    verification_rules: DomainId::new(row.get::<_, String>(5)?),
                    target_environment: DomainId::new(row.get::<_, String>(6)?),
                    merge_combination: row.get::<_, Option<String>>(7)?.map(DomainId::new),
                },
                decision,
                actor: DomainId::new(row.get::<_, String>(9)?),
                reason: row.get(10)?,
                evidence_refs: split_list(&evidence),
                accepted_at_ms: row.get::<_, i64>(12)?,
                superseded_by: row.get::<_, Option<String>>(13)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目范围内的历史接受（只读）：经成果归属落到项目，供换人前后逐字段比对。
    ///
    /// 与 `list_acceptances_for_company` 的差别只是作用域；不做字段裁剪。
    pub fn list_project_acceptances(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<AcceptanceRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT acceptances.acceptance_id, acceptances.artifact_id,
                    acceptances.work_package_revision, acceptances.input_baseline,
                    acceptances.artifact_revision, acceptances.verification_rules,
                    acceptances.target_environment, acceptances.merge_combination,
                    acceptances.decision, acceptances.actor_id, acceptances.reason,
                    acceptances.evidence_refs, acceptances.accepted_at_ms, acceptances.superseded_by
             FROM acceptances
             JOIN artifacts ON artifacts.uid = acceptances.artifact_id
             WHERE acceptances.company_id = ?1 AND artifacts.project_id = ?2
             ORDER BY acceptances.accepted_at_ms ASC, acceptances.rowid ASC",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            let decision = AcceptanceDecision::parse(&row.get::<_, String>(8)?)
                .unwrap_or(AcceptanceDecision::RequiresHuman);
            let evidence: String = row.get(11)?;
            Ok(AcceptanceRecord {
                acceptance_id: DomainId::new(row.get::<_, String>(0)?),
                artifact_id: DomainId::new(row.get::<_, String>(1)?),
                binding: crate::domain::artifact::AcceptanceBinding {
                    work_package_revision: DomainId::new(row.get::<_, String>(2)?),
                    input_baseline: DomainId::new(row.get::<_, String>(3)?),
                    artifact_revision: DomainId::new(row.get::<_, String>(4)?),
                    verification_rules: DomainId::new(row.get::<_, String>(5)?),
                    target_environment: DomainId::new(row.get::<_, String>(6)?),
                    merge_combination: row.get::<_, Option<String>>(7)?.map(DomainId::new),
                },
                decision,
                actor: DomainId::new(row.get::<_, String>(9)?),
                reason: row.get(10)?,
                evidence_refs: split_list(&evidence),
                accepted_at_ms: row.get::<_, i64>(12)?,
                superseded_by: row.get::<_, Option<String>>(13)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内修订正文事实（只读）：摘要/大小/mime 进指纹，便于发现「换了一条内容」。
    pub fn list_project_revision_bodies(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<RevisionBodyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revisions.revision_id, revisions.artifact_id, revisions.produced_by,
                    revisions.digest, revisions.size_bytes, revisions.mime, revisions.created_at_ms
             FROM artifact_revisions AS revisions
             JOIN artifacts ON artifacts.uid = revisions.artifact_id
             WHERE revisions.company_id = ?1 AND artifacts.project_id = ?2
             ORDER BY revisions.revision_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(RevisionBodyRow {
                revision_id: row.get(0)?,
                artifact_id: row.get(1)?,
                produced_by: row.get(2)?,
                digest: row.get(3)?,
                size_bytes: row.get(4)?,
                mime: row.get(5)?,
                created_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内正式 Manifest 事实（只读）。冻结的是引用集合，不是正文。
    pub fn list_project_manifests(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<ManifestBodyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, revision, milestone, artifact_revision_refs, acceptance_refs,
                    signature_ref, created_at_ms
             FROM manifests WHERE company_id = ?1 AND project_id = ?2 ORDER BY uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(ManifestBodyRow {
                uid: row.get(0)?,
                revision: row.get(1)?,
                milestone: row.get(2)?,
                artifact_revision_refs: row.get(3)?,
                acceptance_refs: row.get(4)?,
                signature_ref: row.get(5)?,
                created_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目作用域内的决定（只读）：旧经理的决定不因换人消失或改口。
    pub fn list_project_decisions(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<DecisionBodyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT decision_id, actor_id, decision, reason, evidence_refs, revision_refs,
                    created_at_ms
             FROM decisions WHERE company_id = ?1 AND scope_project = ?2
             ORDER BY created_at_ms, decision_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(DecisionBodyRow {
                decision_id: row.get(0)?,
                actor_id: row.get(1)?,
                decision: row.get(2)?,
                reason: row.get(3)?,
                evidence_refs: row.get(4)?,
                revision_refs: row.get(5)?,
                created_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 插入争议行。主键冲突由调用方按幂等/ID 复用分别处理。
    pub fn insert_history_dispute(&self, row: &DisputeRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO history_disputes (
                dispute_id, company_id, project_id, fact_kind, fact_id, fact_fingerprint,
                reason, evidence_refs, raised_by, state, resolution, resolved_by,
                resolved_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                row.dispute_id,
                row.company_id,
                row.project_id,
                row.fact_kind,
                row.fact_id,
                row.fact_fingerprint,
                row.reason,
                row.evidence_refs,
                row.raised_by,
                row.state,
                row.resolution,
                row.resolved_by,
                row.resolved_at_ms,
                row.created_at_ms,
                row.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_history_dispute(&self, dispute_id: &str) -> Result<Option<DisputeRow>, StoreError> {
        self.conn
            .query_row(
                "SELECT dispute_id, company_id, project_id, fact_kind, fact_id, fact_fingerprint,
                        reason, evidence_refs, raised_by, state, resolution, resolved_by,
                        resolved_at_ms, created_at_ms, updated_at_ms
                 FROM history_disputes WHERE dispute_id = ?1",
                params![dispute_id],
                map_dispute_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn list_history_disputes(
        &self,
        company_id: &str,
        project_id: &str,
        state: Option<&str>,
    ) -> Result<Vec<DisputeRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT dispute_id, company_id, project_id, fact_kind, fact_id, fact_fingerprint,
                    reason, evidence_refs, raised_by, state, resolution, resolved_by,
                    resolved_at_ms, created_at_ms, updated_at_ms
             FROM history_disputes
             WHERE company_id = ?1 AND project_id = ?2 AND (?3 IS NULL OR state = ?3)
             ORDER BY created_at_ms, dispute_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id, state], map_dispute_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 结案：只有 `under_review` 行会被改，返回是否真的改到（并发安全）。
    pub fn resolve_history_dispute(
        &self,
        dispute_id: &str,
        state: &str,
        resolution: &str,
        resolved_by: &str,
        at_ms: i64,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE history_disputes
             SET state = ?2, resolution = ?3, resolved_by = ?4, resolved_at_ms = ?5,
                 updated_at_ms = ?5
             WHERE dispute_id = ?1 AND state = 'under_review'",
            params![dispute_id, state, resolution, resolved_by, at_ms],
        )?;
        Ok(changed == 1)
    }

    /// 公司级全部委任（只读）：专项边界的归属需要一次看全项目/各专项的活跃委任。
    pub fn list_company_appointments(
        &self,
        company_id: &str,
    ) -> Result<Vec<AppointmentRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{APPOINTMENT_SELECT} WHERE company_id = ?1 ORDER BY scope_key, authority_epoch"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_appointment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 公司级全部租约事实（只读）：reportsTo 变更前后比对「未 fencing」。
    pub fn list_company_leases(&self, company_id: &str) -> Result<Vec<LeaseFactRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT resource_key, owner, lease_token, lease_expires_at, state
             FROM leases WHERE company_id = ?1 ORDER BY resource_key",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok(LeaseFactRow {
                resource_key: row.get(0)?,
                owner: row.get(1)?,
                lease_token: row.get(2)?,
                expires_at: row.get(3)?,
                state: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 公司级全部授权事实（只读）：权限不因组织字段变化而迁移。
    pub fn list_company_grants(&self, company_id: &str) -> Result<Vec<GrantFactRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, project_id, permission, revision, active
             FROM grants WHERE company_id = ?1 ORDER BY principal_id, permission, revision",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok(GrantFactRow {
                principal_id: row.get(0)?,
                project_id: row.get(1)?,
                permission: row.get(2)?,
                revision: row.get(3)?,
                active: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内父子层级（只读）。
    pub fn list_project_hierarchy(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<HierarchyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_uid, parent_issue_uid FROM issue_hierarchy
             WHERE company_id = ?1 AND project_id = ?2 ORDER BY issue_uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(HierarchyRow {
                issue_uid: row.get(0)?,
                parent_issue_uid: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 登记一条父子关系。层级只写一次，改父节点由治理流程重建（本期不提供）。
    pub fn insert_issue_hierarchy(
        &self,
        company_id: &str,
        project_id: &str,
        issue_uid: &str,
        parent_issue_uid: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO issue_hierarchy (company_id, project_id, issue_uid, parent_issue_uid,
                created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![company_id, project_id, issue_uid, parent_issue_uid, now_ms],
        )?;
        Ok(())
    }

    /// 成员是否存在（组织元数据只对已登记主体写入，避免拼写错误变成新手）。
    pub fn member_exists(&self, company_id: &str, uid: &str) -> Result<bool, StoreError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE company_id = ?1 AND uid = ?2",
            params![company_id, uid],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_org_metadata(
        &self,
        company_id: &str,
        agent_id: &str,
    ) -> Result<Option<OrgMetadataRow>, StoreError> {
        self.conn
            .query_row(
                "SELECT agent_id, reports_to, updated_at_ms, updated_by
                 FROM agent_org_metadata WHERE company_id = ?1 AND agent_id = ?2",
                params![company_id, agent_id],
                |row| {
                    Ok(OrgMetadataRow {
                        agent_id: row.get(0)?,
                        reports_to: row.get(1)?,
                        updated_at_ms: row.get(2)?,
                        updated_by: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn upsert_org_metadata(
        &self,
        company_id: &str,
        row: &OrgMetadataRow,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO agent_org_metadata (company_id, agent_id, reports_to, updated_at_ms,
                updated_by) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(company_id, agent_id) DO UPDATE SET
                reports_to = excluded.reports_to,
                updated_at_ms = excluded.updated_at_ms,
                updated_by = excluded.updated_by",
            params![
                company_id,
                row.agent_id,
                row.reports_to,
                row.updated_at_ms,
                row.updated_by
            ],
        )?;
        Ok(())
    }

    /// 项目下仍在运行的 Issue 数（清理阻塞判定）。
    pub fn count_running_issues(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM issues
                 WHERE company_id = ?1 AND project_id = ?2
                   AND phase IN ('Running', 'Blocked')",
                params![company_id, project_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 项目下的成果身份（用于列出必须保留的外部引用）。
    pub fn list_artifacts(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<Artifact>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, kind, title, owner_id, visibility, accepted_revision
             FROM artifacts WHERE company_id = ?1 AND project_id = ?2 ORDER BY uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(Artifact {
                id: DomainId::new(row.get::<_, String>(0)?),
                company_id: DomainId::new(company_id.to_string()),
                project_id: DomainId::new(project_id.to_string()),
                kind: ArtifactKind::parse(&row.get::<_, String>(1)?),
                title: row.get(2)?,
                owner: DomainId::new(row.get::<_, String>(3)?),
                visibility: ArtifactVisibility::parse(&row.get::<_, String>(4)?),
                accepted_revision: row.get::<_, Option<String>>(5)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内修订的产出者（只读）：负责人切换要判断某项成果由谁以执行身份产出。
    ///
    /// 返回 `(revision_id, artifact_id, produced_by)`，按 revision_id 排序；不读内容。
    pub fn list_project_revision_producers(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revisions.revision_id, revisions.artifact_id, revisions.produced_by
             FROM artifact_revisions AS revisions
             JOIN artifacts ON artifacts.uid = revisions.artifact_id
             WHERE revisions.company_id = ?1 AND artifacts.project_id = ?2
             ORDER BY revisions.revision_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目运行 ID 列表：预算与执行事实按 Run 归属，需先落到项目范围。
    pub fn list_project_run_ids(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT runs.uid FROM runs JOIN issues ON issues.uid = runs.issue_uid
             WHERE runs.company_id = ?1 AND issues.project_id = ?2 ORDER BY runs.uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内 Run → Issue 归属（只读）：负责人切换的连续性投影按 Run 找执行者。
    ///
    /// 返回 `(run_id, issue_uid)` 并按 run_id 排序；执行者仍从 Issue 的 Assignment 读，
    /// 不从 adapter 或调用方自述推导。
    pub fn list_project_run_issues(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT runs.uid, runs.issue_uid FROM runs
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE runs.company_id = ?1 AND issues.project_id = ?2 ORDER BY runs.uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目范围内全部 invocation 状态（停止是否已确认的事实来源）。
    pub fn list_project_invocation_states(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT invocations.state
             FROM invocations
             JOIN runs ON runs.uid = invocations.run_id
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE invocations.company_id = ?1 AND issues.project_id = ?2
             ORDER BY invocations.invocation_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内 Run 的期望状态（只读）：检查点指纹与「在跑/已终结」判定的事实来源。
    ///
    /// 返回 `(run_id, issue_uid, desired_state)`，按 run_id 排序；不读执行内容。
    pub fn list_project_runs(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT runs.uid, runs.issue_uid, runs.desired_state
             FROM runs
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE runs.company_id = ?1 AND issues.project_id = ?2 ORDER BY runs.uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内 invocation 的标识与状态（只读）：检查点要能区分「哪一条执行变了」。
    ///
    /// 返回 `InvocationFactRow`（id / run / state / 最后观察时间 / fencing token）。
    pub fn list_project_invocations(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<InvocationFactRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT invocations.invocation_id, invocations.run_id, invocations.state,
                    invocations.last_observed_at_ms, invocations.fencing_token
             FROM invocations
             JOIN runs ON runs.uid = invocations.run_id
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE invocations.company_id = ?1 AND issues.project_id = ?2
             ORDER BY invocations.invocation_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(InvocationFactRow {
                invocation_id: row.get(0)?,
                run_id: row.get(1)?,
                state: row.get(2)?,
                last_observed_at_ms: row.get(3)?,
                fencing_token: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 单个 Run 的 invocation 状态。
    pub fn list_run_invocation_states(
        &self,
        company_id: &str,
        run_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT state FROM invocations
             WHERE company_id = ?1 AND run_id = ?2 ORDER BY invocation_id",
        )?;
        let rows = stmt.query_map(params![company_id, run_id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 目标归属反查（只读）：Issue → project_id。跨 Company 视为不存在。
    pub fn issue_project_id(
        &self,
        company_id: &str,
        issue_uid: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT project_id FROM issues WHERE company_id = ?1 AND uid = ?2",
                params![company_id, issue_uid],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(StoreError::Sql)
    }

    /// 目标归属反查（只读）：Artifact → project_id。跨 Company 视为不存在。
    pub fn artifact_project_id(
        &self,
        company_id: &str,
        artifact_id: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT project_id FROM artifacts WHERE company_id = ?1 AND uid = ?2",
                params![company_id, artifact_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// Issue 的删除请求标记（毫秒字符串）；`None` 表示未请求删除。
    pub fn issue_deletion_requested_at(
        &self,
        company_id: &str,
        issue_uid: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT deletion_requested_at FROM issues WHERE company_id = ?1 AND uid = ?2",
                params![company_id, issue_uid],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(StoreError::Sql)
    }

    /// Run 是否存在：Run 级清理不按项目聚合，但目标必须可解析。
    ///
    /// 只要 `runs` 行或该 run_id 的 invocation 存在即成立——P0 切片尚无 runs 写入者，
    /// 仅按 `runs` 判定会把真实存在的 Run 判成不存在。
    pub fn run_exists(&self, company_id: &str, run_id: &str) -> Result<bool, StoreError> {
        self.conn
            .query_row(
                "SELECT 1 FROM runs WHERE company_id = ?1 AND uid = ?2
                 UNION ALL
                 SELECT 1 FROM invocations WHERE company_id = ?1 AND run_id = ?2
                 LIMIT 1",
                params![company_id, run_id],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .map_err(StoreError::Sql)
    }

    /// 无法归属到 Run 的 invocation 数：缺少 `runs` 行时停止事实不可判定，
    /// 必须计入 Unknown，而不是当作"没有在途执行"。
    pub fn count_unattributed_invocations(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM invocations
                 WHERE company_id = ?1 AND run_id NOT IN (
                     SELECT uid FROM runs WHERE company_id = ?1
                 )",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 仍处于 requested/blocked 的清理请求：删除尚未完成的事实来源。
    pub fn list_open_cleanup_requests(
        &self,
        company_id: &str,
    ) -> Result<Vec<CleanupRequest>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT request_id, target_kind, target_id, state, blockers, requested_by,
                    created_at_ms, updated_at_ms
             FROM cleanup_requests
             WHERE company_id = ?1 AND state IN ('requested', 'blocked')
             ORDER BY created_at_ms, request_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let kind: String = row.get(1)?;
            let blockers: String = row.get(4)?;
            Ok(CleanupRequest {
                request_id: row.get(0)?,
                company_id: DomainId::new(company_id.to_string()),
                target_kind: CleanupTargetKind::parse(&kind),
                target_id: row.get(2)?,
                state: CleanupState::parse(&row.get::<_, String>(3)?),
                blockers: split_list(&blockers),
                requested_by: row.get(5)?,
                created_at_ms: row.get(6)?,
                updated_at_ms: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内修订的固定线索：接受指针 + 正式 Manifest 引用。
    ///
    /// Manifest 引用按修订逐个回查（P0 规模可接受）；此处只读，不写任何修订。
    pub fn list_project_revision_pins(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<RevisionPin>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.revision_id, r.artifact_id, r.storage_kind, r.storage_ref,
                    a.accepted_revision
             FROM artifact_revisions r
             JOIN artifacts a ON a.uid = r.artifact_id
             WHERE r.company_id = ?1 AND a.project_id = ?2
             ORDER BY r.revision_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            let revision_id: String = row.get(0)?;
            let kind: String = row.get(2)?;
            let reference: String = row.get(3)?;
            let accepted_revision: Option<String> = row.get(4)?;
            let storage = if kind == "external" {
                StorageRef::External(reference)
            } else {
                StorageRef::Cas(reference)
            };
            Ok(RevisionPin {
                accepted: accepted_revision.as_deref() == Some(revision_id.as_str()),
                revision_id,
                artifact_id: row.get(1)?,
                storage,
                manifest_refs: Vec::new(),
            })
        })?;
        let mut pins = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sql)?;
        for pin in &mut pins {
            pin.manifest_refs = self.manifest_referencing(&pin.revision_id)?;
        }
        Ok(pins)
    }

    pub fn list_reconciliation_cases(
        &self,
        company_id: &str,
        only_open: bool,
    ) -> Result<Vec<ReconciliationCase>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT case_id, invocation_id, logical_action_key, impact_scope,
                    risk_reservation_refs, known_evidence, assignee, next_step, state,
                    created_at_ms, updated_at_ms
             FROM reconciliation_cases
             WHERE company_id = ?1 AND (?2 = 0 OR state = 'open')
             ORDER BY created_at_ms",
        )?;
        let rows = stmt.query_map(params![company_id, i64::from(u8::from(only_open))], |row| {
            let refs: String = row.get(4)?;
            Ok(ReconciliationCase {
                case_id: row.get(0)?,
                company_id: DomainId::new(company_id.to_string()),
                invocation_id: row.get(1)?,
                logical_action_key: row.get(2)?,
                impact_scope: row.get(3)?,
                risk_reservation_refs: split_list(&refs),
                known_evidence: row.get(5)?,
                assignee: row.get(6)?,
                next_step: row.get(7)?,
                state: CaseState::parse(&row.get::<_, String>(8)?),
                created_at_ms: row.get(9)?,
                updated_at_ms: row.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    pub fn get_cleanup_request(
        &self,
        company_id: &str,
        kind: CleanupTargetKind,
        target_id: &str,
    ) -> Result<Option<CleanupRequest>, StoreError> {
        read_cleanup_request(&self.conn, company_id, kind, target_id)
    }

    /// 插入一条交接快照。同 `(company_id, snapshot_key)` 已存在时**不做任何修改**并返回 `false`：
    /// 部分现场一旦登记就不得被后续登记覆盖（§18.11），冲突由调用方读回后判定。
    pub fn insert_handoff_snapshot(&self, row: &HandoffSnapshotRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handoff_snapshots (
                company_id, snapshot_key, project_id, issue_id, work_package_revision,
                source_attempt_id, level, source, completeness, missing_json, integrity_json,
                input_revision, scene_revision, snapshot_seq, payload_digest, captured_at,
                registered_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(company_id, snapshot_key) DO NOTHING",
            params![
                row.company_id,
                row.snapshot_key,
                row.project_id,
                row.issue_id,
                row.work_package_revision,
                row.source_attempt_id,
                row.level,
                row.source,
                row.completeness,
                row.missing_json,
                row.integrity_json,
                row.input_revision,
                row.scene_revision,
                row.snapshot_seq as i64,
                row.payload_digest,
                row.captured_at,
                row.registered_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 按 `(company_id, snapshot_key)` 读取已登记快照。
    pub fn get_handoff_snapshot(
        &self,
        company_id: &str,
        snapshot_key: &str,
    ) -> Result<Option<HandoffSnapshotRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{HANDOFF_SNAPSHOT_SELECT} WHERE company_id = ?1 AND snapshot_key = ?2"),
                params![company_id, snapshot_key],
                map_handoff_snapshot_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 列出某 Company 的全部快照（按 snapshot_seq 升序），供人工恢复查看。
    pub fn list_handoff_snapshots(
        &self,
        company_id: &str,
    ) -> Result<Vec<HandoffSnapshotRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{HANDOFF_SNAPSHOT_SELECT} WHERE company_id = ?1 ORDER BY snapshot_seq"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_handoff_snapshot_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 只列部分快照：崩溃、预算耗尽与强制停止后留给人工恢复的现场。
    pub fn list_partial_handoff_snapshots(
        &self,
        company_id: &str,
    ) -> Result<Vec<HandoffSnapshotRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{HANDOFF_SNAPSHOT_SELECT}
             WHERE company_id = ?1 AND completeness = 'partial' ORDER BY snapshot_seq"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_handoff_snapshot_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 追加一条交接事件。同 `(company_id, handoff_id, event_id)` 已存在时不修改并返回 `false`：
    /// 事件只追加，重复提交由调用方读回比对内容。
    pub fn insert_handoff_event(&self, row: &HandoffEventRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handoff_events (
                company_id, handoff_id, seq, event_id, kind, reason, actor_kind, actor_ref,
                occurred_at, evidence_ref, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(company_id, handoff_id, event_id) DO NOTHING",
            params![
                row.company_id,
                row.handoff_id,
                row.seq as i64,
                row.event_id,
                row.kind,
                row.reason,
                row.actor_kind,
                row.actor_ref,
                row.occurred_at,
                row.evidence_ref,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 按事件 id 读取单条事件（重复提交时比对内容用）。
    pub fn get_handoff_event(
        &self,
        company_id: &str,
        handoff_id: &str,
        event_id: &str,
    ) -> Result<Option<HandoffEventRow>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "{HANDOFF_EVENT_SELECT}
                     WHERE company_id = ?1 AND handoff_id = ?2 AND event_id = ?3"
                ),
                params![company_id, handoff_id, event_id],
                map_handoff_event_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 读取某交接的完整时间线（按 seq 升序）。
    pub fn list_handoff_events(
        &self,
        company_id: &str,
        handoff_id: &str,
    ) -> Result<Vec<HandoffEventRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{HANDOFF_EVENT_SELECT} WHERE company_id = ?1 AND handoff_id = ?2 ORDER BY seq"
        ))?;
        let rows = stmt
            .query_map(params![company_id, handoff_id], map_handoff_event_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 登记一次接受。主键为 `(company_id, handoff_id)`：一个交接只能被接受一次，
    /// 并发/重复接受都无法插入第二行（返回 `false`，由调用方读回判定）。
    pub fn insert_handoff_acceptance(
        &self,
        row: &HandoffAcceptanceRow,
    ) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handoff_acceptances (
                company_id, handoff_id, acceptance_key, request_digest, accepted_event_id,
                accepted_seq, snapshot_key, level, accepted_at, actor_kind, actor_ref,
                recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(company_id, handoff_id) DO NOTHING",
            params![
                row.company_id,
                row.handoff_id,
                row.acceptance_key,
                row.request_digest,
                row.accepted_event_id,
                row.accepted_seq as i64,
                row.snapshot_key,
                row.level,
                row.accepted_at,
                row.actor_kind,
                row.actor_ref,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 读取某交接的接受记录。
    pub fn get_handoff_acceptance(
        &self,
        company_id: &str,
        handoff_id: &str,
    ) -> Result<Option<HandoffAcceptanceRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{HANDOFF_ACCEPTANCE_SELECT} WHERE company_id = ?1 AND handoff_id = ?2"),
                params![company_id, handoff_id],
                |row| {
                    Ok(HandoffAcceptanceRow {
                        company_id: row.get(0)?,
                        handoff_id: row.get(1)?,
                        acceptance_key: row.get(2)?,
                        request_digest: row.get(3)?,
                        accepted_event_id: row.get(4)?,
                        accepted_seq: row.get::<_, i64>(5)? as u64,
                        snapshot_key: row.get(6)?,
                        level: row.get(7)?,
                        accepted_at: row.get(8)?,
                        actor_kind: row.get(9)?,
                        actor_ref: row.get(10)?,
                        recorded_at_ms: row.get(11)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 插入一条接管意图。同 `(company_id, takeover_key)` 已存在时不修改并返回 `false`：
    /// 幂等开单由调用方读回比对内容，不允许覆盖第一次提出的切换。
    pub fn insert_takeover_intent(&self, row: &TakeoverIntentRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO takeover_intents (
                company_id, takeover_key, intent_id, project_id, issue_id,
                work_package_revision, from_attempt_id, requested_by, reason,
                baseline_fencing_token, created_at, phase, version, summary_state,
                summary_requested_at, summary_snapshot_key, attempt_id, fencing_token,
                abort_reason, history_json, updated_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
             ON CONFLICT(company_id, takeover_key) DO NOTHING",
            params![
                row.company_id,
                row.takeover_key,
                row.intent_id,
                row.project_id,
                row.issue_id,
                row.work_package_revision,
                row.from_attempt_id,
                row.requested_by,
                row.reason,
                row.baseline_fencing_token.map(|value| value as i64),
                row.created_at,
                row.phase,
                row.version as i64,
                row.summary_state,
                row.summary_requested_at,
                row.summary_snapshot_key,
                row.attempt_id,
                row.fencing_token.map(|value| value as i64),
                row.abort_reason,
                row.history_json,
                row.updated_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 按 `(company_id, takeover_key)` 读取意图。
    pub fn get_takeover_intent(
        &self,
        company_id: &str,
        takeover_key: &str,
    ) -> Result<Option<TakeoverIntentRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{TAKEOVER_INTENT_SELECT} WHERE company_id = ?1 AND takeover_key = ?2"),
                params![company_id, takeover_key],
                map_takeover_intent_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 列出某 Company 的全部意图（按提出时间）。重启扫描待接管工作用。
    pub fn list_takeover_intents(
        &self,
        company_id: &str,
    ) -> Result<Vec<TakeoverIntentRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{TAKEOVER_INTENT_SELECT} WHERE company_id = ?1 ORDER BY created_at, takeover_key"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_takeover_intent_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 条件更新：只有库里的 `version` 仍等于 `expected_version` 才写入，并把它 +1。
    ///
    /// 返回是否真的改了行；`false` 表示别人的推进已经先到，调用方必须报版本冲突而不是覆盖。
    pub fn update_takeover_intent_cas(
        &self,
        row: &TakeoverIntentRow,
        expected_version: u64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE takeover_intents SET
                intent_id = ?3, project_id = ?4, issue_id = ?5, work_package_revision = ?6,
                from_attempt_id = ?7, requested_by = ?8, reason = ?9,
                baseline_fencing_token = ?10, created_at = ?11, phase = ?12, version = ?13,
                summary_state = ?14, summary_requested_at = ?15, summary_snapshot_key = ?16,
                attempt_id = ?17, fencing_token = ?18, abort_reason = ?19, history_json = ?20,
                updated_at = ?21, recorded_at_ms = ?22
             WHERE company_id = ?1 AND takeover_key = ?2 AND version = ?23",
            params![
                row.company_id,
                row.takeover_key,
                row.intent_id,
                row.project_id,
                row.issue_id,
                row.work_package_revision,
                row.from_attempt_id,
                row.requested_by,
                row.reason,
                row.baseline_fencing_token.map(|value| value as i64),
                row.created_at,
                row.phase,
                row.version as i64,
                row.summary_state,
                row.summary_requested_at,
                row.summary_snapshot_key,
                row.attempt_id,
                row.fencing_token.map(|value| value as i64),
                row.abort_reason,
                row.history_json,
                row.updated_at,
                row.recorded_at_ms,
                expected_version as i64,
            ],
        )?;
        Ok(updated == 1)
    }

    // -----------------------------------------------------------------------
    // 管理委任（P2 3.1）
    // -----------------------------------------------------------------------

    /// 插入一条委任行。`(company_id, appointment_id)` 或
    /// `(company_id, scope_key, authority_epoch)` 冲突时返回 `false`：
    /// 同一 scope 的同一 epoch 不允许出现第二个委任，也不允许覆盖既有行。
    pub fn insert_appointment(&self, row: &AppointmentRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO management_appointments (
                company_id, appointment_id, scope_key, manager_agent_id, grant_ref,
                authority_epoch, status, appointed_by, appointed_at, expires_at, audience,
                commands_json, version, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.appointment_id,
                row.scope_key,
                row.manager_agent_id,
                row.grant_ref,
                row.authority_epoch as i64,
                row.status,
                row.appointed_by,
                row.appointed_at,
                row.expires_at,
                row.audience,
                row.commands_json,
                row.version as i64,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_appointment(
        &self,
        company_id: &str,
        appointment_id: &str,
    ) -> Result<Option<AppointmentRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{APPOINTMENT_SELECT} WHERE company_id = ?1 AND appointment_id = ?2"),
                params![company_id, appointment_id],
                map_appointment_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 的历史委任，按 epoch 升序。
    pub fn list_appointments(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<AppointmentRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{APPOINTMENT_SELECT} WHERE company_id = ?1 AND scope_key = ?2
             ORDER BY authority_epoch"
        ))?;
        let rows = stmt
            .query_map(params![company_id, scope_key], map_appointment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 条件更新委任行：`version` 必须仍等于 `expected_version`。
    pub fn update_appointment_cas(
        &self,
        row: &AppointmentRow,
        expected_version: u64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE management_appointments SET
                manager_agent_id = ?3, grant_ref = ?4, authority_epoch = ?5, status = ?6,
                appointed_by = ?7, appointed_at = ?8, expires_at = ?9, audience = ?10,
                commands_json = ?11, version = ?12, recorded_at_ms = ?13
             WHERE company_id = ?1 AND appointment_id = ?2 AND version = ?14",
            params![
                row.company_id,
                row.appointment_id,
                row.manager_agent_id,
                row.grant_ref,
                row.authority_epoch as i64,
                row.status,
                row.appointed_by,
                row.appointed_at,
                row.expires_at,
                row.audience,
                row.commands_json,
                row.version as i64,
                row.recorded_at_ms,
                expected_version as i64,
            ],
        )?;
        Ok(updated == 1)
    }

    /// 插入作用域头。主键冲突返回 `false`：该 scope 已有头指针，必须走条件更新。
    pub fn insert_scope_head(&self, row: &ScopeHeadRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO management_scope_heads (
                company_id, scope_key, current_appointment_id, authority_epoch, version,
                updated_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.scope_key,
                row.current_appointment_id,
                row.authority_epoch as i64,
                row.version as i64,
                row.updated_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_scope_head(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Option<ScopeHeadRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{SCOPE_HEAD_SELECT} WHERE company_id = ?1 AND scope_key = ?2"),
                params![company_id, scope_key],
                |row| {
                    Ok(ScopeHeadRow {
                        company_id: row.get(0)?,
                        scope_key: row.get(1)?,
                        current_appointment_id: row.get(2)?,
                        authority_epoch: row.get::<_, i64>(3)? as u64,
                        version: row.get::<_, i64>(4)? as u64,
                        updated_at: row.get(5)?,
                        recorded_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 条件更新作用域头：`version` 不等就说明别人先改了指针/epoch，返回 `false`。
    pub fn update_scope_head_cas(
        &self,
        row: &ScopeHeadRow,
        expected_version: u64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE management_scope_heads SET
                current_appointment_id = ?3, authority_epoch = ?4, version = ?5,
                updated_at = ?6, recorded_at_ms = ?7
             WHERE company_id = ?1 AND scope_key = ?2 AND version = ?8",
            params![
                row.company_id,
                row.scope_key,
                row.current_appointment_id,
                row.authority_epoch as i64,
                row.version as i64,
                row.updated_at,
                row.recorded_at_ms,
                expected_version as i64,
            ],
        )?;
        Ok(updated == 1)
    }

    /// 追加一条管理命令审计。同 `(company_id, command_id)` 已存在时不改写并返回 `false`。
    pub fn insert_management_command(
        &self,
        row: &ManagementCommandRow,
    ) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO management_commands (
                company_id, command_id, scope_key, appointment_id, authority_epoch, command,
                outcome, reason_code, reason, at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.command_id,
                row.scope_key,
                row.appointment_id,
                row.authority_epoch.map(|value| value as i64),
                row.command,
                row.outcome,
                row.reason_code,
                row.reason,
                row.at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_management_command(
        &self,
        company_id: &str,
        command_id: &str,
    ) -> Result<Option<ManagementCommandRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{MANAGEMENT_COMMAND_SELECT} WHERE company_id = ?1 AND command_id = ?2"),
                params![company_id, command_id],
                map_management_command_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 的命令审计（按时间升序），供核对「拒绝留下了原因」。
    pub fn list_management_commands(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<ManagementCommandRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{MANAGEMENT_COMMAND_SELECT} WHERE company_id = ?1 AND scope_key = ?2 ORDER BY at, command_id"
        ))?;
        let rows = stmt
            .query_map(params![company_id, scope_key], map_management_command_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 登记检查点：同 `(scope, cursor)` 已存在时不改写并返回 `false`（同现场不产生第二份）。
    pub fn insert_handover_checkpoint(&self, row: &CheckpointRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handover_checkpoints (
                company_id, checkpoint_id, scope_key, project_id, cursor, item_count,
                items_json, gaps_json, captured_by, captured_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.checkpoint_id,
                row.scope_key,
                row.project_id,
                row.cursor,
                row.item_count,
                row.items_json,
                row.gaps_json,
                row.captured_by,
                row.captured_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_handover_checkpoint(
        &self,
        company_id: &str,
        checkpoint_id: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{CHECKPOINT_SELECT} WHERE company_id = ?1 AND checkpoint_id = ?2"),
                params![company_id, checkpoint_id],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 按游标找检查点：同一现场（游标相同）只有一个检查点，重捕获应落回它。
    pub fn find_handover_checkpoint_by_cursor(
        &self,
        company_id: &str,
        scope_key: &str,
        cursor: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "{CHECKPOINT_SELECT} WHERE company_id = ?1 AND scope_key = ?2 AND cursor = ?3"
                ),
                params![company_id, scope_key, cursor],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 最新的检查点（按登记时间倒序）：只用于回显「上次核对的是哪一份现场」。
    pub fn latest_handover_checkpoint(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "{CHECKPOINT_SELECT} WHERE company_id = ?1 AND scope_key = ?2
                     ORDER BY recorded_at_ms DESC, checkpoint_id DESC LIMIT 1"
                ),
                params![company_id, scope_key],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 追加一条接管确认：同 `confirmation_id` 已存在时不改写并返回 `false`。
    pub fn insert_takeover_confirmation(&self, row: &ConfirmationRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO takeover_confirmations (
                company_id, confirmation_id, scope_key, checkpoint_id, checkpoint_cursor,
                reviewed_cursor, appointment_id, authority_epoch, decision, confirmed_by, note,
                open_items_json, delta_json, confirmed_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.confirmation_id,
                row.scope_key,
                row.checkpoint_id,
                row.checkpoint_cursor,
                row.reviewed_cursor,
                row.appointment_id,
                row.authority_epoch as i64,
                row.decision,
                row.confirmed_by,
                row.note,
                row.open_items_json,
                row.delta_json,
                row.confirmed_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_takeover_confirmation(
        &self,
        company_id: &str,
        confirmation_id: &str,
    ) -> Result<Option<ConfirmationRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{CONFIRMATION_SELECT} WHERE company_id = ?1 AND confirmation_id = ?2"),
                params![company_id, confirmation_id],
                map_confirmation_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 的确认历史（epoch 与登记时间升序）：最后一行为当前有效的确认。
    pub fn list_takeover_confirmations(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<ConfirmationRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{CONFIRMATION_SELECT} WHERE company_id = ?1 AND scope_key = ?2
             ORDER BY authority_epoch ASC, recorded_at_ms ASC, confirmation_id ASC"
        ))?;
        let rows = stmt
            .query_map(params![company_id, scope_key], map_confirmation_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// 委任读取列。
const APPOINTMENT_SELECT: &str = "SELECT company_id, appointment_id, scope_key, manager_agent_id,
        grant_ref, authority_epoch, status, appointed_by, appointed_at, expires_at, audience,
        commands_json, version, recorded_at_ms
     FROM management_appointments";

fn map_appointment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppointmentRow> {
    Ok(AppointmentRow {
        company_id: row.get(0)?,
        appointment_id: row.get(1)?,
        scope_key: row.get(2)?,
        manager_agent_id: row.get(3)?,
        grant_ref: row.get(4)?,
        authority_epoch: row.get::<_, i64>(5)? as u64,
        status: row.get(6)?,
        appointed_by: row.get(7)?,
        appointed_at: row.get(8)?,
        expires_at: row.get(9)?,
        audience: row.get(10)?,
        commands_json: row.get(11)?,
        version: row.get::<_, i64>(12)? as u64,
        recorded_at_ms: row.get(13)?,
    })
}

/// 作用域头读取列。
const SCOPE_HEAD_SELECT: &str = "SELECT company_id, scope_key, current_appointment_id,
        authority_epoch, version, updated_at, recorded_at_ms
     FROM management_scope_heads";

/// 管理命令审计读取列。
const MANAGEMENT_COMMAND_SELECT: &str = "SELECT company_id, command_id, scope_key,
        appointment_id, authority_epoch, command, outcome, reason_code, reason, at, recorded_at_ms
     FROM management_commands";

fn map_management_command_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ManagementCommandRow> {
    Ok(ManagementCommandRow {
        company_id: row.get(0)?,
        command_id: row.get(1)?,
        scope_key: row.get(2)?,
        appointment_id: row.get(3)?,
        authority_epoch: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
        command: row.get(5)?,
        outcome: row.get(6)?,
        reason_code: row.get(7)?,
        reason: row.get(8)?,
        at: row.get(9)?,
        recorded_at_ms: row.get(10)?,
    })
}

/// 接管意图读取列。
const TAKEOVER_INTENT_SELECT: &str = "SELECT company_id, takeover_key, intent_id, project_id,
        issue_id, work_package_revision, from_attempt_id, requested_by, reason,
        baseline_fencing_token, created_at, phase, version, summary_state, summary_requested_at,
        summary_snapshot_key, attempt_id, fencing_token, abort_reason, history_json, updated_at,
        recorded_at_ms
     FROM takeover_intents";

/// 检查点读取列。
const CHECKPOINT_SELECT: &str = "SELECT company_id, checkpoint_id, scope_key, project_id, cursor,
        item_count, items_json, gaps_json, captured_by, captured_at, recorded_at_ms
     FROM handover_checkpoints";

fn map_checkpoint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRow> {
    Ok(CheckpointRow {
        company_id: row.get(0)?,
        checkpoint_id: row.get(1)?,
        scope_key: row.get(2)?,
        project_id: row.get(3)?,
        cursor: row.get(4)?,
        item_count: row.get(5)?,
        items_json: row.get(6)?,
        gaps_json: row.get(7)?,
        captured_by: row.get(8)?,
        captured_at: row.get(9)?,
        recorded_at_ms: row.get(10)?,
    })
}

/// 接管确认读取列。
const CONFIRMATION_SELECT: &str = "SELECT company_id, confirmation_id, scope_key, checkpoint_id,
        checkpoint_cursor, reviewed_cursor, appointment_id, authority_epoch, decision,
        confirmed_by, note, open_items_json, delta_json, confirmed_at, recorded_at_ms
     FROM takeover_confirmations";

fn map_confirmation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConfirmationRow> {
    Ok(ConfirmationRow {
        company_id: row.get(0)?,
        confirmation_id: row.get(1)?,
        scope_key: row.get(2)?,
        checkpoint_id: row.get(3)?,
        checkpoint_cursor: row.get(4)?,
        reviewed_cursor: row.get(5)?,
        appointment_id: row.get(6)?,
        authority_epoch: row.get::<_, i64>(7)? as u64,
        decision: row.get(8)?,
        confirmed_by: row.get(9)?,
        note: row.get(10)?,
        open_items_json: row.get(11)?,
        delta_json: row.get(12)?,
        confirmed_at: row.get(13)?,
        recorded_at_ms: row.get(14)?,
    })
}

fn map_dispute_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DisputeRow> {
    Ok(DisputeRow {
        dispute_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        fact_kind: row.get(3)?,
        fact_id: row.get(4)?,
        fact_fingerprint: row.get(5)?,
        reason: row.get(6)?,
        evidence_refs: row.get(7)?,
        raised_by: row.get(8)?,
        state: row.get(9)?,
        resolution: row.get(10)?,
        resolved_by: row.get(11)?,
        resolved_at_ms: row.get(12)?,
        created_at_ms: row.get(13)?,
        updated_at_ms: row.get(14)?,
    })
}

fn map_takeover_intent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TakeoverIntentRow> {
    Ok(TakeoverIntentRow {
        company_id: row.get(0)?,
        takeover_key: row.get(1)?,
        intent_id: row.get(2)?,
        project_id: row.get(3)?,
        issue_id: row.get(4)?,
        work_package_revision: row.get(5)?,
        from_attempt_id: row.get(6)?,
        requested_by: row.get(7)?,
        reason: row.get(8)?,
        baseline_fencing_token: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
        created_at: row.get(10)?,
        phase: row.get(11)?,
        version: row.get::<_, i64>(12)? as u64,
        summary_state: row.get(13)?,
        summary_requested_at: row.get(14)?,
        summary_snapshot_key: row.get(15)?,
        attempt_id: row.get(16)?,
        fencing_token: row.get::<_, Option<i64>>(17)?.map(|value| value as u64),
        abort_reason: row.get(18)?,
        history_json: row.get(19)?,
        updated_at: row.get(20)?,
        recorded_at_ms: row.get(21)?,
    })
}

/// 交接事件读取列。
const HANDOFF_EVENT_SELECT: &str = "SELECT company_id, handoff_id, seq, event_id, kind, reason,
        actor_kind, actor_ref, occurred_at, evidence_ref, recorded_at_ms
     FROM handoff_events";

fn map_handoff_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffEventRow> {
    Ok(HandoffEventRow {
        company_id: row.get(0)?,
        handoff_id: row.get(1)?,
        seq: row.get::<_, i64>(2)? as u64,
        event_id: row.get(3)?,
        kind: row.get(4)?,
        reason: row.get(5)?,
        actor_kind: row.get(6)?,
        actor_ref: row.get(7)?,
        occurred_at: row.get(8)?,
        evidence_ref: row.get(9)?,
        recorded_at_ms: row.get(10)?,
    })
}

/// 交接接受读取列。
const HANDOFF_ACCEPTANCE_SELECT: &str = "SELECT company_id, handoff_id, acceptance_key,
        request_digest, accepted_event_id, accepted_seq, snapshot_key, level, accepted_at,
        actor_kind, actor_ref, recorded_at_ms
     FROM handoff_acceptances";

/// 交接快照的读取列；`list_*` 与 `get_*` 共用，避免列序漂移。
const HANDOFF_SNAPSHOT_SELECT: &str = "SELECT company_id, snapshot_key, project_id, issue_id,
        work_package_revision, source_attempt_id, level, source, completeness, missing_json,
        integrity_json, input_revision, scene_revision, snapshot_seq, payload_digest, captured_at,
        registered_at_ms
     FROM handoff_snapshots";

fn map_handoff_snapshot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffSnapshotRow> {
    Ok(HandoffSnapshotRow {
        company_id: row.get(0)?,
        snapshot_key: row.get(1)?,
        project_id: row.get(2)?,
        issue_id: row.get(3)?,
        work_package_revision: row.get(4)?,
        source_attempt_id: row.get(5)?,
        level: row.get(6)?,
        source: row.get(7)?,
        completeness: row.get(8)?,
        missing_json: row.get(9)?,
        integrity_json: row.get(10)?,
        input_revision: row.get(11)?,
        scene_revision: row.get(12)?,
        snapshot_seq: row.get::<_, i64>(13)? as u64,
        payload_digest: row.get(14)?,
        captured_at: row.get(15)?,
        registered_at_ms: row.get(16)?,
    })
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

    /// 事务内读作用域头：接管确认必须与「当前 epoch」在同一次事务里判定。
    pub fn get_scope_head(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Option<ScopeHeadRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{SCOPE_HEAD_SELECT} WHERE company_id = ?1 AND scope_key = ?2"),
                params![company_id, scope_key],
                |row| {
                    Ok(ScopeHeadRow {
                        company_id: row.get(0)?,
                        scope_key: row.get(1)?,
                        current_appointment_id: row.get(2)?,
                        authority_epoch: row.get::<_, i64>(3)? as u64,
                        version: row.get::<_, i64>(4)? as u64,
                        updated_at: row.get(5)?,
                        recorded_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 追加一条审计事件（CR-002）。
    ///
    /// 只有 INSERT：审计不可改写，所以没有 upsert。重复写同一条事件不是幂等问题，
    /// 而是调用方逻辑问题——去重会把「同一动作被执行了两次」这件事抹掉。
    ///
    /// 必须先过 `AuditEvent::validate()`。库层的 CHECK 只兜「决策与原因不一致」，
    /// 拦不住「理由文本里带令牌」——两道闸门分工不同，别以为有库约束就够了。
    pub fn append_audit_event(
        &self,
        event: &crate::audit_log::AuditEvent,
        recorded_at_ms: i64,
    ) -> Result<i64, StoreError> {
        event.validate().map_err(StoreError::Audit)?;
        self.conn.execute(
            "INSERT INTO audit_events (
                company_id, correlation_id, actor_id, action,
                scope_company, scope_project, scope_issue,
                basis, decision, reason, decided_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                event.company_id,
                event.correlation_id,
                event.actor.as_str(),
                event.action.as_str(),
                event.scope.company_id.as_str(),
                event.scope.project_id.as_ref().map(DomainId::as_str),
                event.scope.issue_id.as_ref().map(DomainId::as_str),
                event.basis,
                event.decision.as_str(),
                event.reason,
                event.decided_at,
                recorded_at_ms,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 登记一个跟踪项（P8 3.1 的写入原语，不是导入端点）。
    ///
    /// 幂等键、来源身份去重与目标追加语义分别在 3.3 / 3.4 / 3.5 落地；这里只保证
    /// 「一行能按约束写进去」，且内部引用（issue/run/attempt）缺省时**不伪造运行**（D3）。
    pub fn insert_delivery_tracking(&self, row: &TrackingRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO delivery_tracking (
                tracking_id, company_id, project_id, title, requirement, owner_id, state,
                issue_uid, run_uid, attempt_id, resource_version, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                row.tracking_id,
                row.company_id,
                row.project_id,
                row.title,
                row.requirement,
                row.owner_id,
                row.state.as_str(),
                row.issue_uid,
                row.run_uid,
                row.attempt_id,
                row.resource_version,
                row.created_at_ms,
                row.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 登记一个客户目标（同一跟踪项下「客户 × 环境 × 单元」唯一）。
    pub fn insert_delivery_target(&self, row: &TargetRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO delivery_targets (
                target_id, company_id, tracking_id, customer_id, environment, deployment_unit,
                acceptance, switch_state, population_known, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.target_id,
                row.company_id,
                row.tracking_id,
                row.customer_id,
                row.environment,
                row.deployment_unit,
                row.acceptance,
                row.switch_state.as_str(),
                row.population_known as i64,
                row.created_at_ms,
                row.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 追加一条观察。**只追加，不改写**——回滚是新的观察（3.1 的两条核心语义之一）。
    ///
    /// 两道显式校验先于库里约束，为的是给出可读原因：
    /// 人工补证没有授权范围与复验依据（`note` 为空）直接拒绝；
    /// 目标不属于该跟踪项直接拒绝（复合外键会拦，但报错信息读不出来）。
    pub fn append_delivery_observation(&self, row: &ObservationRow) -> Result<(), StoreError> {
        if row.kind == ObservationKind::ManualEvidence && row.note.trim().is_empty() {
            return Err(DeliveryError::EmptyManualEvidenceNote.into());
        }
        let belongs: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM delivery_targets
             WHERE company_id = ?1 AND tracking_id = ?2 AND target_id = ?3",
            params![row.company_id, row.tracking_id, row.target_id],
            |line| line.get(0),
        )?;
        if belongs == 0 {
            return Err(DeliveryError::TargetNotInTracking {
                target_id: row.target_id.clone(),
                tracking_id: row.tracking_id.clone(),
            }
            .into());
        }
        // 观察挂来源修订时，来源必须存在且属于同一项目：跨项目的来源不得挂到本项目交付上。
        // 复合外键只能保证「修订存在」，它读不出「项目不同」这件事。
        if let Some(revision_id) = &row.source_revision_id {
            let source_project: Option<String> = self
                .conn
                .query_row(
                    "SELECT s.project_id
                     FROM delivery_source_revisions r
                     JOIN delivery_sources s
                       ON s.company_id = r.company_id AND s.source_id = r.source_id
                     WHERE r.company_id = ?1 AND r.source_revision_id = ?2",
                    params![row.company_id, revision_id],
                    |line| line.get(0),
                )
                .optional()?;
            let Some(source_project) = source_project else {
                return Err(crate::delivery_source::SourceError::SourceRevisionNotFound(
                    revision_id.clone(),
                )
                .into());
            };
            let tracking_project: String = self.conn.query_row(
                "SELECT project_id FROM delivery_tracking
                 WHERE company_id = ?1 AND tracking_id = ?2",
                params![row.company_id, row.tracking_id],
                |line| line.get(0),
            )?;
            if source_project != tracking_project {
                return Err(crate::delivery_source::SourceError::SourceProjectMismatch {
                    source_project,
                    tracking_project,
                }
                .into());
            }
        }
        self.conn.execute(
            "INSERT INTO delivery_observations (
                observation_id, company_id, tracking_id, target_id, kind, source_kind, source_ref,
                artifact_revision, digest, revision, note, actor_id, authorized,
                observed_at, observed_at_ms, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                row.observation_id,
                row.company_id,
                row.tracking_id,
                row.target_id,
                row.kind.as_str(),
                row.source_kind,
                row.source_ref,
                row.artifact_revision,
                row.digest,
                row.revision,
                row.note,
                row.actor_id,
                row.authorized as i64,
                row.observed_at,
                row.observed_at_ms,
                row.recorded_at_ms,
            ],
        )?;
        if let Some(revision_id) = &row.source_revision_id {
            self.conn.execute(
                "INSERT INTO delivery_observation_sources (
                    observation_id, company_id, source_revision_id, linked_at_ms
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    row.observation_id,
                    row.company_id,
                    revision_id,
                    row.recorded_at_ms,
                ],
            )?;
        }
        Ok(())
    }

    // --- 来源身份（P8 3.2）--------------------------------------------------

    /// 登记一个来源身份（五元组）。**幂等**：五元组已存在时返回已有 ID，
    /// 不覆盖首个确认人——「谁先把这个 ID 认成那个对象」是历史事实。
    ///
    /// `source_id` 只在新建时使用。ID 生成留给导入路径（3.5）：本函数与 3.1 的
    /// 写入原语一致，由调用方给主键，测试因此能确定地引用同一行。
    pub fn register_source(
        &self,
        source_id: &str,
        identity: &crate::delivery_source::SourceIdentity,
        locator: &str,
        confirmed_by: &str,
        now_ms: i64,
    ) -> Result<RegisteredSource, StoreError> {
        use crate::delivery_source::SourceError;
        identity.validate()?;
        if confirmed_by.trim().is_empty() {
            return Err(SourceError::MissingConfirmer.into());
        }
        if let Some(existing) = find_source_id(self.conn, identity)? {
            return Ok(RegisteredSource {
                source_id: existing,
                created: false,
            });
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_sources (
                source_id, company_id, project_id, connection_id, object_type,
                provider_object_id, locator, confirmed_by, confirmed_at_ms, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                source_id,
                identity.company_id,
                identity.project_id,
                identity.connection_id,
                identity.object_type,
                identity.provider_object_id,
                locator.trim(),
                confirmed_by,
                now_ms,
                now_ms,
            ],
        );
        match result {
            Ok(_) => Ok(RegisteredSource {
                source_id: source_id.to_string(),
                created: true,
            }),
            // 并发下两次登记同一身份：后到者拿已有行，而不是报一个没人看得懂的唯一约束错误。
            Err(err) if is_unique_violation(&err) => match find_source_id(self.conn, identity)? {
                Some(existing) => Ok(RegisteredSource {
                    source_id: existing,
                    created: false,
                }),
                None => Err(err.into()),
            },
            Err(err) => Err(err.into()),
        }
    }

    /// 提交一个来源候选。**幂等**：同一作用域下同一 `locator` 的待确认候选只留一条。
    ///
    /// 候选不是身份：它没有稳定 ID。重复提交同一 URL 应当得到同一条待办，
    /// 而不是四条等着同一个人点四次。
    pub fn propose_source_candidate(
        &self,
        candidate_id: &str,
        proposal: &crate::delivery_source::CandidateProposal,
        proposed_by: &str,
        now_ms: i64,
    ) -> Result<ProposedCandidate, StoreError> {
        use crate::delivery_source::SourceError;
        if proposal.locator.trim().is_empty() {
            return Err(SourceError::EmptyLocator.into());
        }
        if proposed_by.trim().is_empty() {
            return Err(SourceError::MissingProposer.into());
        }
        let lookup = |conn: &rusqlite::Connection| -> rusqlite::Result<Option<String>> {
            conn.query_row(
                "SELECT candidate_id FROM delivery_source_candidates
                 WHERE company_id = ?1 AND project_id = ?2 AND connection_id = ?3
                   AND locator = ?4 AND state = 'pending'",
                params![
                    proposal.company_id,
                    proposal.project_id,
                    proposal.connection_id,
                    proposal.locator.trim(),
                ],
                |row| row.get(0),
            )
            .optional()
        };
        if let Some(existing) = lookup(self.conn)? {
            return Ok(ProposedCandidate {
                candidate_id: existing,
                created: false,
            });
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_source_candidates (
                candidate_id, company_id, project_id, connection_id, object_type,
                locator, proposed_by, proposed_at_ms, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
            params![
                candidate_id,
                proposal.company_id,
                proposal.project_id,
                proposal.connection_id,
                proposal.object_type,
                proposal.locator.trim(),
                proposed_by,
                now_ms,
            ],
        );
        match result {
            Ok(_) => Ok(ProposedCandidate {
                candidate_id: candidate_id.to_string(),
                created: true,
            }),
            Err(err) if is_unique_violation(&err) => match lookup(self.conn)? {
                Some(id) => Ok(ProposedCandidate {
                    candidate_id: id,
                    created: false,
                }),
                None => Err(err.into()),
            },
            Err(err) => Err(err.into()),
        }
    }

    /// 确认候选：候选 → 身份，两个写入必须同生共死。
    ///
    /// 用 `SAVEPOINT` 包住，不是多余：调用方（导入路径）在自己的事务里，若这里
    /// 「身份登记成功、候选状态没改成」却返回失败，调用方很容易照样提交，
    /// 于是同一个定位会留下两条待确认候选。函数自己保证原子性。
    pub fn confirm_source_candidate(
        &self,
        company_id: &str,
        candidate_id: &str,
        source_id: &str,
        identity: &crate::delivery_source::SourceIdentity,
        decided_by: &str,
        now_ms: i64,
    ) -> Result<ConfirmedCandidate, StoreError> {
        use crate::delivery_source::{CandidateState, SourceError, decide, ensure_scope_matches};
        self.conn
            .execute_batch("SAVEPOINT confirm_source_candidate")?;
        let outcome = (|| -> Result<ConfirmedCandidate, StoreError> {
            let candidate = self
                .conn
                .query_row(
                    &format!(
                        "{SOURCE_CANDIDATE_SELECT} WHERE company_id = ?1 AND candidate_id = ?2"
                    ),
                    params![company_id, candidate_id],
                    map_source_candidate_row,
                )
                .optional()?
                .ok_or_else(|| SourceError::CandidateNotFound(candidate_id.to_string()))?;
            decide(candidate.state)?;
            ensure_scope_matches(&candidate, identity)?;
            if decided_by.trim().is_empty() {
                return Err(SourceError::MissingConfirmer.into());
            }
            let registered =
                self.register_source(source_id, identity, &candidate.locator, decided_by, now_ms)?;
            let changed = self.conn.execute(
                "UPDATE delivery_source_candidates
                 SET state = 'confirmed', resolved_source_id = ?1,
                     decided_by = ?2, decided_at_ms = ?3
                 WHERE company_id = ?4 AND candidate_id = ?5 AND state = 'pending'",
                params![
                    registered.source_id,
                    decided_by,
                    now_ms,
                    company_id,
                    candidate_id,
                ],
            )?;
            if changed != 1 {
                // 并发下另一个人先确认了：本次注册的身份随之回滚（savepoint），
                // 报告「已裁决」而不是留下一个没人引用的身份。
                return Err(SourceError::CandidateAlreadyDecided(
                    CandidateState::Confirmed.as_str().to_string(),
                )
                .into());
            }
            Ok(ConfirmedCandidate {
                candidate_id: candidate_id.to_string(),
                source_id: registered.source_id,
                source_created: registered.created,
            })
        })();
        match outcome {
            Ok(value) => {
                self.conn
                    .execute_batch("RELEASE confirm_source_candidate")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO confirm_source_candidate; RELEASE confirm_source_candidate",
                );
                Err(err)
            }
        }
    }

    /// 追加一条来源修订。**幂等**：内容指纹相同的事件返回已有修订。
    ///
    /// 指纹必须与 `(version, digest)` 重算结果一致——让调用方自己编指纹，
    /// 幂等键就成了「调用方承诺」，重复上报只要换个指纹就能插进来。
    pub fn append_source_revision(
        &self,
        revision: &crate::delivery_source::SourceRevisionRow,
    ) -> Result<AppendedRevision, StoreError> {
        use crate::delivery_source::{SourceError, revision_fingerprint};
        let derived = revision_fingerprint(&revision.version, &revision.digest)?;
        if derived != revision.content_fingerprint {
            return Err(SourceError::FingerprintMismatch {
                declared: revision.content_fingerprint.clone(),
                derived,
            }
            .into());
        }
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM delivery_sources WHERE company_id = ?1 AND source_id = ?2",
            params![revision.company_id, revision.source_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(SourceError::SourceNotFound(revision.source_id.clone()).into());
        }
        let lookup = |conn: &rusqlite::Connection| -> rusqlite::Result<Option<String>> {
            conn.query_row(
                "SELECT source_revision_id FROM delivery_source_revisions
                 WHERE company_id = ?1 AND source_id = ?2 AND content_fingerprint = ?3",
                params![
                    revision.company_id,
                    revision.source_id,
                    revision.content_fingerprint,
                ],
                |row| row.get(0),
            )
            .optional()
        };
        if let Some(existing) = lookup(self.conn)? {
            return Ok(AppendedRevision {
                source_revision_id: existing,
                created: false,
            });
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_source_revisions (
                source_revision_id, company_id, source_id, version, digest,
                content_fingerprint, actor_id, observed_at, observed_at_ms, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                revision.source_revision_id,
                revision.company_id,
                revision.source_id,
                revision.version,
                revision.digest,
                revision.content_fingerprint,
                revision.actor_id,
                revision.observed_at,
                revision.observed_at_ms,
                revision.recorded_at_ms,
            ],
        );
        match result {
            Ok(_) => Ok(AppendedRevision {
                source_revision_id: revision.source_revision_id.clone(),
                created: true,
            }),
            Err(err) if is_unique_violation(&err) => match lookup(self.conn)? {
                Some(id) => Ok(AppendedRevision {
                    source_revision_id: id,
                    created: false,
                }),
                None => Err(err.into()),
            },
            Err(err) => Err(err.into()),
        }
    }

    // --- 去重语义（P8 3.3）--------------------------------------------------

    /// 把来源身份挂到一条跟踪项上（§11「来源关系」）。
    ///
    /// 三条语义，逐条都有测试：
    /// - 已经挂着**同一条** → `created=false`，不重复写。重复导入走的就是这条。
    /// - 已经挂着**另一条** → `SourceAlreadyLinked`。不偷偷改挂：改挂是悄悄挪走一条
    ///   已经承诺出去的交付，而且会让「重复导入返回已有关系」失去唯一对象可返回。
    /// - 跨项目 → `SourceProjectMismatch`。复合外键只保证两边存在，读不出「项目不同」。
    pub fn link_delivery_tracking_source(
        &self,
        company_id: &str,
        source_id: &str,
        tracking_id: &str,
        linked_by: &str,
        now_ms: i64,
    ) -> Result<LinkedSource, StoreError> {
        if linked_by.trim().is_empty() {
            return Err(SourceError::MissingLinker.into());
        }
        let source_project: Option<String> = self
            .conn
            .query_row(
                "SELECT project_id FROM delivery_sources WHERE company_id = ?1 AND source_id = ?2",
                params![company_id, source_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(source_project) = source_project else {
            return Err(SourceError::SourceNotFound(source_id.to_string()).into());
        };
        let tracking_project: Option<String> = self
            .conn
            .query_row(
                "SELECT project_id FROM delivery_tracking WHERE company_id = ?1 AND tracking_id = ?2",
                params![company_id, tracking_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(tracking_project) = tracking_project else {
            return Err(DeliveryError::TrackingNotFound(tracking_id.to_string()).into());
        };
        if source_project != tracking_project {
            return Err(SourceError::SourceProjectMismatch {
                source_project,
                tracking_project,
            }
            .into());
        }
        if let Some(existing) = lookup_tracking_for_source(self.conn, company_id, source_id)? {
            if existing == tracking_id {
                return Ok(LinkedSource {
                    tracking_id: existing,
                    created: false,
                });
            }
            return Err(SourceError::SourceAlreadyLinked {
                source_id: source_id.to_string(),
                tracking_id: tracking_id.to_string(),
                linked_tracking_id: existing,
            }
            .into());
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_tracking_sources (
                company_id, source_id, tracking_id, linked_by, linked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![company_id, source_id, tracking_id, linked_by, now_ms],
        );
        match result {
            Ok(_) => Ok(LinkedSource {
                tracking_id: tracking_id.to_string(),
                created: true,
            }),
            // 并发下另一个写入者刚挂上：后到者拿已有关系，不报一个读不懂的唯一约束错误。
            Err(err) if is_unique_violation(&err) => {
                match lookup_tracking_for_source(self.conn, company_id, source_id)? {
                    Some(existing) if existing == tracking_id => Ok(LinkedSource {
                        tracking_id: existing,
                        created: false,
                    }),
                    Some(existing) => Err(SourceError::SourceAlreadyLinked {
                        source_id: source_id.to_string(),
                        tracking_id: tracking_id.to_string(),
                        linked_tracking_id: existing,
                    }
                    .into()),
                    None => Err(err.into()),
                }
            }
            Err(err) => Err(err.into()),
        }
    }

    /// 导入一次外部来源：**两种去重在这里分工**（§11 / design D2）。
    ///
    /// 1. 先读回执（幂等键，管命令重放）：有回执 → 同命令同载荷就原样返回首次结果，
    ///    换命令或换载荷就报冲突。整段**不进入写入路径**。
    /// 2. 没有回执 → 解析来源。成为身份时登记五元组并挂来源关系；已有关系就复用它。
    ///    只有定位时进候选，**不建跟踪项**——候选没有稳定身份，支撑不了覆盖结论。
    /// 3. 最后写回执。整段包在 `SAVEPOINT` 里：任何一步失败都不留下「这个键已执行」。
    ///
    /// 为什么回执与业务写入必须同事务：先写回执再干活，失败后重试会撞上自己伪造的
    /// 冲突；先干活再写回执而不回滚，重试会重放出一个从未发生的成功。两种都错。
    pub fn import_delivery_source(
        &self,
        command: &ImportSourceCommand,
        now_ms: i64,
    ) -> Result<ImportOutcome, StoreError> {
        if command.company_id.trim().is_empty() {
            return Err(IdempotencyError::EmptyCompanyId.into());
        }
        let key = crate::delivery_idempotency::validate_key(&command.idempotency_key)?;
        let fingerprint =
            crate::delivery_idempotency::payload_fingerprint(&command.payload_fields())?;

        // 重放判定在读回执这一步就结束：命中的命令不再碰任何业务表。
        let existing = find_receipt(self.conn, &command.company_id, key)?;
        if let Replay::Replayed(receipt) = classify(existing, COMMAND_IMPORT_SOURCE, &fingerprint)?
        {
            return ImportOutcome::replayed_from(&receipt.result_detail).map_err(|detail| {
                IdempotencyError::UnreadableReceipt {
                    key: key.to_string(),
                    detail,
                }
                .into()
            });
        }

        self.conn
            .execute_batch("SAVEPOINT import_delivery_source")?;
        let outcome = (|| -> Result<ImportOutcome, StoreError> {
            let imported = match crate::delivery_source::resolve(
                &command.company_id,
                &command.project_id,
                &command.connection_id,
                command.object_type.as_deref(),
                command.provider_object_id.as_deref(),
                &command.locator,
            )? {
                SourceResolution::Identity(identity) => {
                    self.import_identity(command, &identity, now_ms)?
                }
                SourceResolution::Candidate(proposal) => {
                    let proposed = self.propose_source_candidate(
                        &command.new_candidate_id,
                        &proposal,
                        &command.imported_by,
                        now_ms,
                    )?;
                    ImportOutcome::candidate(proposed.candidate_id, proposed.created)
                }
            };
            let detail = serde_json::to_string(&imported).map_err(|err| {
                IdempotencyError::ResultNotRecordable(format!("结果体无法编码为 JSON：{err}"))
            })?;
            let result_kind = if imported.candidate_id.is_some() {
                ResultKind::Candidate
            } else {
                ResultKind::Tracking
            };
            let result_id = imported
                .candidate_id
                .as_deref()
                .or(imported.tracking_id.as_deref())
                .unwrap_or_default();
            self.conn.execute(
                "INSERT INTO delivery_command_receipts (
                    company_id, idempotency_key, command, payload_fingerprint,
                    result_kind, result_id, result_detail, recorded_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    command.company_id,
                    key,
                    COMMAND_IMPORT_SOURCE,
                    fingerprint,
                    result_kind.as_str(),
                    result_id,
                    detail,
                    now_ms,
                ],
            )?;
            Ok(imported)
        })();
        match outcome {
            Ok(value) => {
                self.conn.execute_batch("RELEASE import_delivery_source")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO import_delivery_source; RELEASE import_delivery_source",
                );
                Err(err)
            }
        }
    }

    /// 导入的身份分支：登记身份 → 找到或建跟踪项 → 挂关系。
    ///
    /// 三种情形，返回值只差 `created_*`：
    /// - 身份已承载跟踪项 → 复用它（**重复导入返回已有关系**，不建第二个跟踪项）；
    /// - 身份没有关系，但 `new_tracking_id` 已存在 → 挂到它上面（导入到指定跟踪项）；
    /// - 都不存在 → 建跟踪项再挂。
    fn import_identity(
        &self,
        command: &ImportSourceCommand,
        identity: &SourceIdentity,
        now_ms: i64,
    ) -> Result<ImportOutcome, StoreError> {
        let registered = self.register_source(
            &command.new_source_id,
            identity,
            &command.locator,
            &command.imported_by,
            now_ms,
        )?;
        let (tracking_id, created_tracking) = match lookup_tracking_for_source(
            self.conn,
            &command.company_id,
            &registered.source_id,
        )? {
            Some(existing) => (existing, false),
            None => {
                let exists: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM delivery_tracking
                         WHERE company_id = ?1 AND tracking_id = ?2",
                    params![command.company_id, command.new_tracking_id],
                    |row| row.get(0),
                )?;
                if exists > 0 {
                    (command.new_tracking_id.clone(), false)
                } else {
                    self.insert_delivery_tracking(&TrackingRow {
                        tracking_id: command.new_tracking_id.clone(),
                        company_id: command.company_id.clone(),
                        project_id: command.project_id.clone(),
                        title: command.title.clone(),
                        requirement: command.requirement.clone(),
                        owner_id: command.owner_id.clone(),
                        state: crate::delivery_tracking::TrackingState::Open,
                        // 外部导入不伪造内部运行（D3）。
                        issue_uid: None,
                        run_uid: None,
                        attempt_id: None,
                        resource_version: 1,
                        created_at_ms: now_ms,
                        updated_at_ms: now_ms,
                    })?;
                    (command.new_tracking_id.clone(), true)
                }
            }
        };
        let linked = self.link_delivery_tracking_source(
            &command.company_id,
            &registered.source_id,
            &tracking_id,
            &command.imported_by,
            now_ms,
        )?;
        Ok(ImportOutcome::tracked(
            linked.tracking_id,
            registered.source_id,
            created_tracking,
            registered.created,
        ))
    }

    pub fn get_appointment(
        &self,
        company_id: &str,
        appointment_id: &str,
    ) -> Result<Option<AppointmentRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{APPOINTMENT_SELECT} WHERE company_id = ?1 AND appointment_id = ?2"),
                params![company_id, appointment_id],
                map_appointment_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn get_handover_checkpoint(
        &self,
        company_id: &str,
        checkpoint_id: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{CHECKPOINT_SELECT} WHERE company_id = ?1 AND checkpoint_id = ?2"),
                params![company_id, checkpoint_id],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn insert_takeover_confirmation(&self, row: &ConfirmationRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO takeover_confirmations (
                company_id, confirmation_id, scope_key, checkpoint_id, checkpoint_cursor,
                reviewed_cursor, appointment_id, authority_epoch, decision, confirmed_by, note,
                open_items_json, delta_json, confirmed_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.confirmation_id,
                row.scope_key,
                row.checkpoint_id,
                row.checkpoint_cursor,
                row.reviewed_cursor,
                row.appointment_id,
                row.authority_epoch as i64,
                row.decision,
                row.confirmed_by,
                row.note,
                row.open_items_json,
                row.delta_json,
                row.confirmed_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 首次使用路径自动补齐最小工作包修订：无需预先配置 WorkPackage。
    pub fn insert_work_package_revision(
        &self,
        revision_id: &str,
        company_id: &str,
        issue_uid: &str,
        spec_revision: &str,
        acceptance_revision: &str,
        created_at: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO work_package_revisions
             (revision_id, company_id, issue_uid, spec_revision, acceptance_revision, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                revision_id,
                company_id,
                issue_uid,
                spec_revision,
                acceptance_revision,
                created_at
            ],
        )?;
        Ok(())
    }

    /// 首次使用路径自动补齐最小运行实例：无需预先配置 Run/执行图。
    pub fn insert_run(
        &self,
        uid: &str,
        company_id: &str,
        issue_uid: &str,
        plan_revision: &str,
        desired_state: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO runs (uid, company_id, issue_uid, plan_revision, desired_state)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![uid, company_id, issue_uid, plan_revision, desired_state],
        )?;
        Ok(())
    }

    /// 统计工作包修订行数；首次使用断言"从零自动补齐"，不是复用预置配置。
    pub fn count_work_package_revisions(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM work_package_revisions WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 统计运行实例行数，语义同 [`Self::count_work_package_revisions`]。
    pub fn count_runs(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    pub fn insert_issue(&self, issue: &Issue) -> Result<(), StoreError> {
        let (repo_provider, repo_id) = match &issue.repository {
            Some(repo) => (
                Some(repo.provider.as_str()),
                Some(repo.repository_id.as_str()),
            ),
            None => (None, None),
        };
        let (owner_id, owner_kind, grant_revision) = match &issue.spec.assignment {
            Some(assignment) => (
                Some(assignment.owner.id.as_str()),
                Some(owner_kind_str(assignment.owner.kind)),
                Some(assignment.grant_revision as i64),
            ),
            None => (None, None, None),
        };
        self.conn.execute(
            "INSERT INTO issues (
                uid, company_id, project_id, kind, repo_provider, repo_id, title, acceptance,
                owner_id, owner_kind, assignment_grant_revision, resource_version,
                generation, observed_generation, phase, condition_reason, deletion_requested_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                issue.meta.uid.as_str(),
                issue.meta.company_id.as_str(),
                issue.meta.project_id.as_ref().map(DomainId::as_str),
                issue_kind_str(issue.kind),
                repo_provider,
                repo_id,
                issue.spec.title,
                acceptance_text(&issue.spec.acceptance),
                owner_id,
                owner_kind,
                grant_revision,
                issue.meta.resource_version.0 as i64,
                issue.meta.generation as i64,
                issue.status.observed_generation as i64,
                phase_str(issue.status.phase),
                issue
                    .status
                    .conditions
                    .last()
                    .map(|condition| format!("{:?}", condition.reason)),
                issue.meta.deletion_requested_at,
            ],
        )?;
        Ok(())
    }

    /// 读取 Issue 的完整投影；只读路径不得因此触发任何派发。
    pub fn get_issue(&self, uid: &str) -> Result<Option<Issue>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, project_id, kind, repo_provider, repo_id, title, acceptance,
                        owner_id, owner_kind, assignment_grant_revision, resource_version,
                        generation, observed_generation, phase
                 FROM issues WHERE uid = ?1",
                params![uid],
                |row| {
                    let company_id = DomainId::new(row.get::<_, String>(0)?);
                    let project_id = row.get::<_, Option<String>>(1)?.map(DomainId::new);
                    let kind = match row.get::<_, String>(2)?.as_str() {
                        "Coding" => IssueKind::Coding,
                        _ => IssueKind::General,
                    };
                    let provider: Option<String> = row.get(3)?;
                    let repository_id: Option<String> = row.get(4)?;
                    let repository = match (provider, repository_id) {
                        (Some(provider), Some(repository_id)) => Some(RepositoryContext {
                            provider,
                            repository_id,
                        }),
                        _ => None,
                    };
                    let acceptance: String = row.get(6)?;
                    let owner_id: Option<String> = row.get(7)?;
                    let owner_kind: Option<String> = row.get(8)?;
                    let grant_revision: Option<i64> = row.get(9)?;
                    let assignment = match (owner_id, owner_kind) {
                        (Some(owner_id), Some(owner_kind)) => Some(Assignment {
                            owner: Owner {
                                kind: if owner_kind == "User" {
                                    OwnerKind::User
                                } else {
                                    OwnerKind::Agent
                                },
                                id: DomainId::new(owner_id),
                            },
                            scope: Scope {
                                company_id: company_id.clone(),
                                project_id: project_id.clone(),
                                issue_id: Some(DomainId::new(uid.to_string())),
                            },
                            grant_revision: grant_revision.unwrap_or(0) as u64,
                        }),
                        _ => None,
                    };
                    let phase = match row.get::<_, String>(13)?.as_str() {
                        "Running" => Phase::Running,
                        "Blocked" => Phase::Blocked,
                        "Succeeded" => Phase::Succeeded,
                        "Cancelled" => Phase::Cancelled,
                        _ => Phase::Pending,
                    };
                    Ok(Issue {
                        meta: ResourceMeta {
                            uid: DomainId::new(uid.to_string()),
                            company_id,
                            project_id,
                            resource_version: Version(row.get::<_, i64>(10)? as u64),
                            generation: row.get::<_, i64>(11)? as u64,
                            deletion_requested_at: None,
                        },
                        kind,
                        repository,
                        spec: IssueSpec {
                            title: row.get(5)?,
                            assignment,
                            acceptance: AcceptanceCriteria {
                                criteria: split_acceptance(&acceptance),
                            },
                        },
                        status: ResourceStatus {
                            phase,
                            observed_generation: row.get::<_, i64>(12)? as u64,
                            conditions: Vec::new(),
                        },
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 条件更新：只有 resource_version 匹配才写入；返回当前版本以便调用方重试。
    pub fn update_issue_conditional(
        &self,
        issue: &Issue,
        expected: Version,
    ) -> Result<(), StoreError> {
        let (owner_id, owner_kind, grant_revision) = match &issue.spec.assignment {
            Some(assignment) => (
                Some(assignment.owner.id.as_str()),
                Some(owner_kind_str(assignment.owner.kind)),
                Some(assignment.grant_revision as i64),
            ),
            None => (None, None, None),
        };
        let rows = self.conn.execute(
            "UPDATE issues SET
                title = ?2, acceptance = ?3, owner_id = ?4, owner_kind = ?5,
                assignment_grant_revision = ?6, resource_version = ?7, generation = ?8,
                observed_generation = ?9, phase = ?10, condition_reason = ?11
             WHERE uid = ?1 AND resource_version = ?12",
            params![
                issue.meta.uid.as_str(),
                issue.spec.title,
                acceptance_text(&issue.spec.acceptance),
                owner_id,
                owner_kind,
                grant_revision,
                issue.meta.resource_version.0 as i64,
                issue.meta.generation as i64,
                issue.status.observed_generation as i64,
                phase_str(issue.status.phase),
                issue
                    .status
                    .conditions
                    .last()
                    .map(|condition| format!("{:?}", condition.reason)),
                expected.0 as i64,
            ],
        )?;
        if rows == 1 {
            Ok(())
        } else {
            let current = self
                .conn
                .query_row(
                    "SELECT resource_version FROM issues WHERE uid = ?1",
                    params![issue.meta.uid.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0);
            Err(StoreError::VersionConflict {
                current: Version(current.max(0) as u64),
            })
        }
    }

    pub fn insert_event(
        &self,
        event_id: &str,
        company_id: &str,
        aggregate_id: &str,
        aggregate_version: u64,
        payload: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO events (event_id, company_id, aggregate_id, aggregate_version, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event_id,
                company_id,
                aggregate_id,
                aggregate_version as i64,
                payload
            ],
        )?;
        Ok(())
    }

    pub fn insert_grant(&self, grant: &Grant) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO grants (principal_id, company_id, project_id, permission, revision, active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                grant.principal_id.as_str(),
                grant.scope.company_id.as_str(),
                grant.scope.project_id.as_ref().map(DomainId::as_str),
                permission_str(grant.permission),
                grant.revision as i64,
                i64::from(grant.active),
            ],
        )?;
        Ok(())
    }

    // ---- D2：work_queue / wake_receipts / outbox_deliveries 持久化 ----

    /// 读取队列行；不存在返回 None。
    pub fn get_queue(
        &self,
        company_id: &str,
        queue_key: &str,
    ) -> Result<Option<WorkQueueEntry>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, queue_key, state, wanted_generation, claimed_generation,
                        completed_generation, next_due_at_ms, attempts, lease_owner, lease_token,
                        lease_expires_at_ms, last_error, blocked_reason
                 FROM work_queue WHERE company_id = ?1 AND queue_key = ?2",
                params![company_id, queue_key],
                |row| {
                    Ok(WorkQueueEntry {
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        queue_key: row.get(1)?,
                        state: QueueState::parse(&row.get::<_, String>(2)?)
                            .unwrap_or(QueueState::Idle),
                        wanted_generation: row.get::<_, i64>(3)? as u64,
                        claimed_generation: row.get::<_, i64>(4)? as u64,
                        completed_generation: row.get::<_, i64>(5)? as u64,
                        next_due_at_ms: row.get(6)?,
                        attempts: row.get::<_, i64>(7)? as u32,
                        lease_owner: row.get(8)?,
                        lease_token: row.get::<_, i64>(9)? as u64,
                        lease_expires_at_ms: row.get(10)?,
                        last_error: row.get(11)?,
                        blocked_reason: row
                            .get::<_, Option<String>>(12)?
                            .and_then(parse_denial_reason),
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 插入或更新队列行（受理唤醒、认领、完成、退避统一走此写入口）。
    pub fn put_queue(&self, entry: &WorkQueueEntry) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO work_queue (
                company_id, queue_key, state, wanted_generation, claimed_generation,
                completed_generation, next_due_at_ms, attempts, lease_owner, lease_token,
                lease_expires_at_ms, last_error, blocked_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(company_id, queue_key) DO UPDATE SET
                state = excluded.state,
                wanted_generation = excluded.wanted_generation,
                claimed_generation = excluded.claimed_generation,
                completed_generation = excluded.completed_generation,
                next_due_at_ms = excluded.next_due_at_ms,
                attempts = excluded.attempts,
                lease_owner = excluded.lease_owner,
                lease_token = excluded.lease_token,
                lease_expires_at_ms = excluded.lease_expires_at_ms,
                last_error = excluded.last_error,
                blocked_reason = excluded.blocked_reason",
            params![
                entry.company_id.as_str(),
                entry.queue_key,
                entry.state.as_str(),
                entry.wanted_generation as i64,
                entry.claimed_generation as i64,
                entry.completed_generation as i64,
                entry.next_due_at_ms,
                entry.attempts as i64,
                entry.lease_owner,
                entry.lease_token as i64,
                entry.lease_expires_at_ms,
                entry.last_error,
                entry.blocked_reason.map(denial_reason_str),
            ],
        )?;
        Ok(())
    }

    /// 读取唤醒回执。
    pub fn get_wake_receipt(
        &self,
        company_id: &str,
        producer_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<WakeReceipt>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, producer_id, idempotency_key, request_digest, queue_key,
                        generation, accepted_at_ms, retain_until_ms, schedule_id, occurrence_id
                 FROM wake_receipts
                WHERE company_id = ?1 AND producer_id = ?2 AND idempotency_key = ?3",
                params![company_id, producer_id, idempotency_key],
                |row| {
                    Ok(WakeReceipt {
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        producer_id: DomainId::new(row.get::<_, String>(1)?),
                        idempotency_key: row.get(2)?,
                        request_digest: row.get(3)?,
                        queue_key: row.get(4)?,
                        generation: row.get::<_, i64>(5)? as u64,
                        accepted_at_ms: row.get(6)?,
                        retain_until_ms: row.get(7)?,
                        schedule_id: row.get(8)?,
                        occurrence_id: row.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn put_wake_receipt(&self, receipt: &WakeReceipt) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO wake_receipts (
                company_id, producer_id, idempotency_key, request_digest, queue_key,
                generation, accepted_at_ms, retain_until_ms, schedule_id, occurrence_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                receipt.company_id.as_str(),
                receipt.producer_id.as_str(),
                receipt.idempotency_key,
                receipt.request_digest,
                receipt.queue_key,
                receipt.generation as i64,
                receipt.accepted_at_ms,
                receipt.retain_until_ms,
                receipt.schedule_id,
                receipt.occurrence_id,
            ],
        )?;
        Ok(())
    }

    /// 插入持久 outbox 意图；同 (id, destination) 重复保持原记录（稳定逻辑键不重建）。
    pub fn put_outbox(&self, delivery: &OutboxDelivery) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO outbox_deliveries (
                event_or_command_id, destination_id, company_id, queue_key, request_digest,
                status, attempts, next_due_at_ms, result_ref, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                delivery.event_or_command_id,
                delivery.destination_id,
                delivery.company_id.as_str(),
                delivery.queue_key,
                delivery.request_digest,
                delivery.status.as_str(),
                delivery.attempts as i64,
                delivery.next_due_at_ms,
                delivery.result_ref,
                delivery.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_outbox(
        &self,
        event_or_command_id: &str,
        destination_id: &str,
    ) -> Result<Option<OutboxDelivery>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, queue_key, request_digest, status, attempts,
                        next_due_at_ms, result_ref, created_at_ms
                 FROM outbox_deliveries
                WHERE event_or_command_id = ?1 AND destination_id = ?2",
                params![event_or_command_id, destination_id],
                |row| {
                    Ok(OutboxDelivery {
                        event_or_command_id: event_or_command_id.to_string(),
                        destination_id: destination_id.to_string(),
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        queue_key: row.get(1)?,
                        request_digest: row.get(2)?,
                        status: EffectStatus::parse(&row.get::<_, String>(3)?)
                            .unwrap_or(EffectStatus::Unknown),
                        attempts: row.get::<_, i64>(4)? as u32,
                        next_due_at_ms: row.get(5)?,
                        result_ref: row.get(6)?,
                        created_at_ms: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 更新 outbox 状态；用于提交后派发的结果登记与恢复裁决。
    pub fn set_outbox_status(
        &self,
        event_or_command_id: &str,
        destination_id: &str,
        status: EffectStatus,
        attempts: u32,
        next_due_at_ms: i64,
        result_ref: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE outbox_deliveries SET status = ?3, attempts = ?4,
                    next_due_at_ms = ?5, result_ref = ?6
             WHERE event_or_command_id = ?1 AND destination_id = ?2",
            params![
                event_or_command_id,
                destination_id,
                status.as_str(),
                attempts as i64,
                next_due_at_ms,
                result_ref,
            ],
        )?;
        Ok(())
    }

    /// 原子认领：只有 state=pending、租约 token 匹配且已到期时更新成功。
    /// BEGIN IMMEDIATE 串行化下，先成功者递增 lease_token，竞争者的 WHERE 不再匹配。
    /// 返回 true 表示本调用持有租约；失败方必须读取当前状态后退出，不得启动执行。
    pub fn claim_queue_if_ready(
        &self,
        company_id: &str,
        queue_key: &str,
        expected_token: u64,
        owner: &str,
        now_ms: i64,
        lease_expires_at_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE work_queue SET
                state = 'running', lease_owner = ?4, lease_token = lease_token + 1,
                lease_expires_at_ms = ?5, claimed_generation = wanted_generation,
                attempts = attempts + 1, next_due_at_ms = ?6, last_error = NULL
             WHERE company_id = ?1 AND queue_key = ?2 AND lease_token = ?3
               AND state = 'pending' AND next_due_at_ms <= ?6",
            params![
                company_id,
                queue_key,
                expected_token as i64,
                owner,
                lease_expires_at_ms,
                now_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    /// 完成确认 CAS：owner 与 token 必须同时匹配，旧 token 不得更新宿主受管状态（fencing）。
    /// 成功时按决策写入 completed_generation 与下一状态；返回是否命中。
    #[allow(clippy::too_many_arguments)]
    pub fn complete_queue_if_leased(
        &self,
        company_id: &str,
        queue_key: &str,
        owner: &str,
        token: u64,
        completed_generation: u64,
        next_state: QueueState,
        next_due_at_ms: i64,
        last_error: Option<&str>,
        blocked_reason: Option<DenialReason>,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE work_queue SET
                completed_generation = ?5, state = ?6, next_due_at_ms = ?7,
                last_error = ?8, blocked_reason = ?9
             WHERE company_id = ?1 AND queue_key = ?2 AND lease_owner = ?3
               AND lease_token = ?4 AND state = 'running'",
            params![
                company_id,
                queue_key,
                owner,
                token as i64,
                completed_generation as i64,
                next_state.as_str(),
                next_due_at_ms,
                last_error,
                blocked_reason.map(denial_reason_str),
            ],
        )?;
        Ok(rows == 1)
    }

    /// 租约过期恢复处理权：到期且仍 running 时转 pending，保留 lease_token 供 fencing。
    pub fn expire_lease_if_due(
        &self,
        company_id: &str,
        queue_key: &str,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE work_queue SET state = 'pending', next_due_at_ms = ?3
             WHERE company_id = ?1 AND queue_key = ?2 AND state = 'running'
               AND lease_expires_at_ms IS NOT NULL AND lease_expires_at_ms <= ?3",
            params![company_id, queue_key, now_ms],
        )?;
        Ok(rows == 1)
    }

    // ---- D3：budget_limits / reservations / ledger_entries 持久化 ----

    pub fn get_budget_limit(
        &self,
        company_id: &str,
        scope_kind: BudgetScopeKind,
        scope_id: &str,
        dimension: Dimension,
        currency: &str,
    ) -> Result<Option<BudgetLimit>, StoreError> {
        self.conn
            .query_row(
                "SELECT limit_value, version FROM budget_limits
                 WHERE company_id = ?1 AND scope_kind = ?2 AND scope_id = ?3
                   AND dimension = ?4 AND currency = ?5",
                params![
                    company_id,
                    scope_kind.as_str(),
                    scope_id,
                    dimension.as_str(),
                    currency
                ],
                |row| {
                    Ok(BudgetLimit {
                        company_id: DomainId::new(company_id),
                        scope_kind,
                        scope_id: DomainId::new(scope_id),
                        measure: Measure {
                            dimension,
                            value: 0,
                            currency: if currency.is_empty() {
                                None
                            } else {
                                Some(currency.to_string())
                            },
                        },
                        limit: row.get(0)?,
                        version: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn put_budget_limit(&self, limit: &BudgetLimit) -> Result<(), StoreError> {
        let currency = limit.measure.currency.as_deref().unwrap_or("");
        self.conn.execute(
            "INSERT INTO budget_limits (
                company_id, scope_kind, scope_id, dimension, currency, limit_value, version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(company_id, scope_kind, scope_id, dimension, currency) DO UPDATE SET
                limit_value = excluded.limit_value, version = excluded.version",
            params![
                limit.company_id.as_str(),
                limit.scope_kind.as_str(),
                limit.scope_id.as_str(),
                limit.measure.dimension.as_str(),
                currency,
                limit.limit,
                limit.version as i64,
            ],
        )?;
        Ok(())
    }

    /// 聚合某层级某维度的 C/R/U。consumed 计 C、active 计 R、risk 计 U、cancelled 全零。
    pub fn usage_for_scope(
        &self,
        company_id: &str,
        scope_kind: BudgetScopeKind,
        scope_id: &str,
        dimension: Dimension,
        currency: &str,
    ) -> Result<BudgetUsage, StoreError> {
        let mut usage = BudgetUsage::default();
        let mut read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<()> {
            usage.confirmed = row.get(0)?;
            usage.reserved = row.get(1)?;
            usage.risk = row.get(2)?;
            Ok(())
        };
        const SELECT: &str = "SELECT COALESCE(SUM(confirmed_value), 0),
                                     COALESCE(SUM(reserved_value), 0),
                                     COALESCE(SUM(risk_value), 0)
                              FROM reservations WHERE company_id = ?1";
        match scope_kind {
            BudgetScopeKind::Company => self.conn.query_row(
                &format!("{SELECT} AND dimension = ?2 AND currency = ?3"),
                params![company_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::Project => self.conn.query_row(
                &format!("{SELECT} AND project_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::Agent => self.conn.query_row(
                &format!("{SELECT} AND agent_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::Run => self.conn.query_row(
                &format!("{SELECT} AND run_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::WorkPackage => self.conn.query_row(
                &format!("{SELECT} AND work_package_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
        };
        Ok(usage)
    }

    pub fn insert_reservation(&self, reservation: &Reservation) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO reservations (
                reservation_id, company_id, project_id, agent_id, run_id, work_package_id,
                dimension, currency, reserved_value, confirmed_value, risk_value, state,
                logical_key, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                reservation.reservation_id,
                reservation.company_id.as_str(),
                reservation.project_id.as_ref().map(DomainId::as_str),
                reservation.agent_id.as_ref().map(DomainId::as_str),
                reservation.run_id.as_ref().map(DomainId::as_str),
                reservation.work_package_id.as_ref().map(DomainId::as_str),
                reservation.measure.dimension.as_str(),
                reservation.measure.currency.as_deref().unwrap_or(""),
                reservation.reserved,
                reservation.confirmed,
                reservation.risk,
                reservation.state.as_str(),
                reservation.logical_key,
                reservation.created_at_ms,
                reservation.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_reservation(&self, reservation_id: &str) -> Result<Option<Reservation>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, project_id, agent_id, run_id, work_package_id, dimension,
                        currency, reserved_value, confirmed_value, risk_value, state,
                        logical_key, created_at_ms, updated_at_ms
                 FROM reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |row| {
                    let dimension =
                        Dimension::parse(&row.get::<_, String>(5)?).unwrap_or(Dimension::Calls);
                    let currency: String = row.get(6)?;
                    Ok(Reservation {
                        reservation_id: reservation_id.to_string(),
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        project_id: row.get::<_, Option<String>>(1)?.map(DomainId::new),
                        agent_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                        run_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                        work_package_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                        measure: Measure {
                            dimension,
                            value: 0,
                            currency: if currency.is_empty() {
                                None
                            } else {
                                Some(currency)
                            },
                        },
                        reserved: row.get(7)?,
                        confirmed: row.get(8)?,
                        risk: row.get(9)?,
                        state: ReservationState::parse(&row.get::<_, String>(10)?)
                            .unwrap_or(ReservationState::Cancelled),
                        logical_key: row.get(11)?,
                        created_at_ms: row.get(12)?,
                        updated_at_ms: row.get(13)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 该项目是否已有超额调整分录：有则拒绝新的派发，直到有来源的调整解除。
    pub fn has_project_overage(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<bool, StoreError> {
        let present: i64 = self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM ledger_entries
                WHERE company_id = ?1 AND project_id = ?2 AND entry_kind = 'adjustment'
             )",
            params![company_id, project_id],
            |row| row.get(0),
        )?;
        Ok(present != 0)
    }

    /// 按稳定逻辑键查预留：同一派发意图重试应复用原预留，不重复占用。
    pub fn get_reservation_by_logical_key(
        &self,
        logical_key: &str,
    ) -> Result<Option<Reservation>, StoreError> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT reservation_id FROM reservations WHERE logical_key = ?1",
                params![logical_key],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.get_reservation(&id),
            None => Ok(None),
        }
    }

    /// 更新预留状态与 C/R/U。DB 的互斥 CHECK 会拒绝 R/U 同时非零的非法写法。
    pub fn update_reservation(&self, reservation: &Reservation) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE reservations SET
                reserved_value = ?2, confirmed_value = ?3, risk_value = ?4,
                state = ?5, updated_at_ms = ?6
             WHERE reservation_id = ?1",
            params![
                reservation.reservation_id,
                reservation.reserved,
                reservation.confirmed,
                reservation.risk,
                reservation.state.as_str(),
                reservation.updated_at_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    /// 追加不可变分录。usage_key 冲突映射为稳定错误，调用方据此去重而非重复扣费。
    pub fn insert_ledger_entry(&self, entry: &LedgerEntry) -> Result<(), StoreError> {
        let result = self.conn.execute(
            "INSERT INTO ledger_entries (
                entry_id, company_id, project_id, agent_id, work_package_id, reservation_id,
                usage_key, entry_kind, dimension, currency, amount, source, reason, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                entry.entry_id,
                entry.company_id.as_str(),
                entry.project_id.as_ref().map(DomainId::as_str),
                entry.agent_id.as_ref().map(DomainId::as_str),
                entry.work_package_id.as_ref().map(DomainId::as_str),
                entry.reservation_id,
                entry.usage_key,
                entry.kind.as_str(),
                entry.measure.dimension.as_str(),
                entry.measure.currency.as_deref().unwrap_or(""),
                entry.amount,
                entry.source,
                entry.reason,
                entry.created_at_ms,
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(err) => {
                if let Some(usage_key) = &entry.usage_key
                    && err
                        .to_string()
                        .contains("UNIQUE constraint failed: ledger_entries.usage_key")
                {
                    return Err(StoreError::DuplicateUsageKey {
                        usage_key: usage_key.clone(),
                    });
                }
                Err(StoreError::Sql(err))
            }
        }
    }

    pub fn ledger_entry_for_usage_key(
        &self,
        usage_key: &str,
    ) -> Result<Option<LedgerEntry>, StoreError> {
        self.conn
            .query_row(
                "SELECT entry_id, company_id, project_id, agent_id, work_package_id,
                        reservation_id, entry_kind, dimension, currency, amount, source,
                        reason, created_at_ms
                 FROM ledger_entries WHERE usage_key = ?1",
                params![usage_key],
                |row| {
                    let dimension =
                        Dimension::parse(&row.get::<_, String>(7)?).unwrap_or(Dimension::Calls);
                    let currency: String = row.get(8)?;
                    Ok(LedgerEntry {
                        entry_id: row.get(0)?,
                        company_id: DomainId::new(row.get::<_, String>(1)?),
                        project_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                        agent_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                        work_package_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                        reservation_id: row.get(5)?,
                        usage_key: Some(usage_key.to_string()),
                        kind: LedgerEntryKind::parse(&row.get::<_, String>(6)?)
                            .unwrap_or(LedgerEntryKind::Adjustment),
                        measure: Measure {
                            dimension,
                            value: 0,
                            currency: if currency.is_empty() {
                                None
                            } else {
                                Some(currency)
                            },
                        },
                        amount: row.get(9)?,
                        source: row.get(10)?,
                        reason: row.get(11)?,
                        created_at_ms: row.get(12)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    // --- D4 执行协议：invocation 去重、观察 inbox、能力与探测记录 ---

    /// 读 invocation 接收记录。存在即说明宿主已承诺过这次派发，不再新建。
    pub fn get_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT invocation_id, company_id, run_id, work_package_revision, attempt_id,
                        binding_id, fencing_token, adapter_id, adapter_version, params_canonical,
                        params_digest, execution_id, state, created_at_ms, accepted_at_ms,
                        last_observed_at_ms, retention_until_ms
                 FROM invocations WHERE invocation_id = ?1",
                params![invocation_id],
                |row| {
                    let state_text: String = row.get(12)?;
                    let fencing: i64 = row.get(6)?;
                    Ok(InvocationRecord {
                        invocation_id: row.get(0)?,
                        company_id: row.get(1)?,
                        run_id: row.get(2)?,
                        work_package_revision: row.get(3)?,
                        attempt_id: row.get(4)?,
                        binding_id: row.get(5)?,
                        fencing_token: fencing.max(0) as u64,
                        adapter_id: row.get(7)?,
                        adapter_version: row.get(8)?,
                        params_canonical: row.get(9)?,
                        params_digest: row.get(10)?,
                        execution_id: row.get(11)?,
                        state: ExecutionState::parse(&state_text)
                            .unwrap_or(ExecutionState::Unknown),
                        created_at_ms: row.get(13)?,
                        accepted_at_ms: row.get(14)?,
                        last_observed_at_ms: row.get(15)?,
                        retention_until_ms: row.get(16)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 先持久化接收记录，再允许对外发送 start（§15.6）。
    pub fn insert_invocation(&self, record: &InvocationRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO invocations (
                invocation_id, company_id, run_id, work_package_revision, attempt_id, binding_id,
                fencing_token, adapter_id, adapter_version, params_canonical, params_digest,
                execution_id, state, created_at_ms, accepted_at_ms, last_observed_at_ms,
                retention_until_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                record.invocation_id,
                record.company_id,
                record.run_id,
                record.work_package_revision,
                record.attempt_id,
                record.binding_id,
                record.fencing_token as i64,
                record.adapter_id,
                record.adapter_version,
                record.params_canonical,
                record.params_digest,
                record.execution_id,
                record.state.as_str(),
                record.created_at_ms,
                record.accepted_at_ms,
                record.last_observed_at_ms,
                record.retention_until_ms,
            ],
        )?;
        Ok(())
    }

    /// 确认接收：只写一次 execution_id，重复确认不覆盖原值（同 ID 返回原执行）。
    pub fn confirm_invocation(
        &self,
        invocation_id: &str,
        execution_id: &str,
        state: ExecutionState,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE invocations
             SET execution_id = ?2, state = ?3, accepted_at_ms = COALESCE(accepted_at_ms, ?4),
                 last_observed_at_ms = ?4
             WHERE invocation_id = ?1 AND execution_id IS NULL",
            params![invocation_id, execution_id, state.as_str(), now_ms],
        )?;
        Ok(rows == 1)
    }

    pub fn set_invocation_state(
        &self,
        invocation_id: &str,
        state: ExecutionState,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE invocations SET state = ?2, last_observed_at_ms = ?3 WHERE invocation_id = ?1",
            params![invocation_id, state.as_str(), now_ms],
        )?;
        Ok(rows == 1)
    }

    /// 记录一次 start 尝试。回包丢失只留审计，不新建 Attempt（I-04）。
    pub fn record_start_audit(
        &self,
        invocation_id: &str,
        outcome: &str,
        detail: Option<&str>,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO start_audits (invocation_id, outcome, detail, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![invocation_id, outcome, detail, now_ms],
        )?;
        Ok(())
    }

    /// 观察先持久接收；返回 true 表示首次接收（重复序号不再产生业务效果）。
    pub fn insert_observation(
        &self,
        envelope: &ObservationEnvelope,
        truncated: bool,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "INSERT INTO observations (
                invocation_id, source_epoch, stream_id, source_sequence, observed_at_ms, kind,
                payload, origin, truncated, persisted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT DO NOTHING",
            params![
                envelope.invocation_id,
                envelope.source_epoch as i64,
                envelope.stream_id,
                envelope.source_sequence as i64,
                envelope.observed_at_ms,
                envelope.kind.as_str(),
                envelope.payload,
                envelope.origin,
                i64::from(truncated),
                now_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    pub fn get_observation_cursor(
        &self,
        invocation_id: &str,
        source_epoch: u64,
        stream_id: &str,
    ) -> Result<Option<ObservationCursor>, StoreError> {
        self.conn
            .query_row(
                "SELECT highest_contiguous, retained_floor, buffered
                 FROM observation_cursors
                 WHERE invocation_id = ?1 AND source_epoch = ?2 AND stream_id = ?3",
                params![invocation_id, source_epoch as i64, stream_id],
                |row| {
                    let highest: Option<i64> = row.get(0)?;
                    let floor: Option<i64> = row.get(1)?;
                    let buffered: String = row.get(2)?;
                    let buffered: Vec<u64> = buffered
                        .split(',')
                        .filter(|part| !part.is_empty())
                        .filter_map(|part| part.parse::<u64>().ok())
                        .collect();
                    Ok(ObservationCursor::restore(
                        highest.map(|value| value.max(0) as u64),
                        floor.map(|value| value.max(0) as u64),
                        &buffered,
                    ))
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn put_observation_cursor(
        &self,
        invocation_id: &str,
        source_epoch: u64,
        stream_id: &str,
        cursor: &ObservationCursor,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let buffered = cursor
            .buffered_sequences()
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        self.conn.execute(
            "INSERT INTO observation_cursors (
                invocation_id, source_epoch, stream_id, highest_contiguous, retained_floor,
                buffered, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(invocation_id, source_epoch, stream_id) DO UPDATE SET
                highest_contiguous = excluded.highest_contiguous,
                retained_floor = excluded.retained_floor,
                buffered = excluded.buffered,
                updated_at_ms = excluded.updated_at_ms",
            params![
                invocation_id,
                source_epoch as i64,
                stream_id,
                cursor.ack_upto().map(|value| value as i64),
                cursor.retained_floor().map(|value| value as i64),
                buffered,
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// 未处理的观察：重启后继续业务处理，ACK 已确认但处理未完成的不能当已完成。
    pub fn list_pending_observations(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<ObservationEnvelope>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT invocation_id, source_epoch, stream_id, source_sequence, observed_at_ms,
                    kind, payload, origin
             FROM observations
             WHERE invocation_id = ?1 AND processed = 0
             ORDER BY source_epoch, stream_id, source_sequence",
        )?;
        let rows = statement.query_map(params![invocation_id], |row| {
            let kind_text: String = row.get(5)?;
            Ok(ObservationEnvelope {
                invocation_id: row.get(0)?,
                source_epoch: row.get::<_, i64>(1)?.max(0) as u64,
                stream_id: row.get(2)?,
                source_sequence: row.get::<_, i64>(3)?.max(0) as u64,
                observed_at_ms: row.get(4)?,
                kind: ObservationType::parse(&kind_text).unwrap_or(ObservationType::Log),
                payload: row.get(6)?,
                origin: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    pub fn mark_observation_processed(
        &self,
        invocation_id: &str,
        source_epoch: u64,
        stream_id: &str,
        source_sequence: u64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE observations SET processed = 1
             WHERE invocation_id = ?1 AND source_epoch = ?2 AND stream_id = ?3
               AND source_sequence = ?4 AND processed = 0",
            params![
                invocation_id,
                source_epoch as i64,
                stream_id,
                source_sequence as i64
            ],
        )?;
        Ok(rows == 1)
    }

    /// 记录适配器自报的能力声明：未知能力也落库（level=unknown），不静默丢弃。
    pub fn put_capability_claims(&self, descriptor: &AdapterDescriptor) -> Result<(), StoreError> {
        for claim in &descriptor.capabilities {
            self.conn.execute(
                "INSERT INTO adapter_capabilities (
                    adapter_id, adapter_version, capability, level, evidence_level, source,
                    reason, observed_at_ms, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(adapter_id, adapter_version, capability) DO UPDATE SET
                    level = excluded.level,
                    evidence_level = excluded.evidence_level,
                    source = excluded.source,
                    reason = excluded.reason,
                    observed_at_ms = excluded.observed_at_ms,
                    expires_at_ms = excluded.expires_at_ms",
                params![
                    descriptor.adapter_id,
                    descriptor.adapter_version,
                    claim.slot.as_str(),
                    claim.level.as_str(),
                    claim.evidence_level.as_str(),
                    claim.source.as_str(),
                    claim.reason,
                    claim.observed_at_ms,
                    claim.expires_at_ms,
                ],
            )?;
        }
        Ok(())
    }

    /// 分项探测结果落库：保留 scope 与 TTL，过期由读取方降级为 unknown。
    pub fn put_probe_report(&self, report: &ProbeReport) -> Result<(), StoreError> {
        for item in &report.items {
            self.conn.execute(
                "INSERT INTO probe_results (
                    adapter_id, adapter_version, aspect, status, reason, source, scope,
                    observed_at_ms, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(adapter_id, adapter_version, aspect) DO UPDATE SET
                    status = excluded.status,
                    reason = excluded.reason,
                    source = excluded.source,
                    scope = excluded.scope,
                    observed_at_ms = excluded.observed_at_ms,
                    expires_at_ms = excluded.expires_at_ms",
                params![
                    report.adapter_id,
                    report.adapter_version,
                    item.aspect.as_str(),
                    item.status.as_str(),
                    item.reason,
                    item.source.as_str(),
                    item.scope,
                    item.observed_at_ms,
                    item.expires_at_ms,
                ],
            )?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // D5 成果、修订与接受（任务 6.1—6.4）
    // -----------------------------------------------------------------------

    pub fn insert_artifact(&self, artifact: &Artifact, now_ms: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO artifacts (
                uid, company_id, project_id, kind, title, owner_id, visibility,
                accepted_revision, resource_version, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                artifact.id.as_str(),
                artifact.company_id.as_str(),
                artifact.project_id.as_str(),
                artifact.kind.as_str(),
                artifact.title,
                artifact.owner.as_str(),
                artifact.visibility.as_str(),
                artifact.accepted_revision.as_ref().map(DomainId::as_str),
                1_i64,
                now_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_artifact(&self, artifact_id: &str) -> Result<Option<Artifact>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, project_id, kind, title, owner_id, visibility, accepted_revision
                 FROM artifacts WHERE uid = ?1",
                params![artifact_id],
                |row| {
                    Ok(Artifact {
                        id: DomainId::new(artifact_id.to_string()),
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        project_id: DomainId::new(row.get::<_, String>(1)?),
                        kind: ArtifactKind::parse(&row.get::<_, String>(2)?),
                        title: row.get(3)?,
                        owner: DomainId::new(row.get::<_, String>(4)?),
                        visibility: ArtifactVisibility::parse(&row.get::<_, String>(5)?),
                        accepted_revision: row.get::<_, Option<String>>(6)?.map(DomainId::new),
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 接受指针的 compare-and-swap：只有当前指针仍等于 `expected` 才写入。
    /// 返回 false 表示并发接受已发生，调用方必须要求重新读取而不是覆盖。
    pub fn cas_accepted_revision(
        &self,
        artifact_id: &str,
        expected: Option<&str>,
        next_revision: &str,
    ) -> Result<bool, StoreError> {
        let rows = match expected {
            Some(expected) => self.conn.execute(
                "UPDATE artifacts SET accepted_revision = ?1, resource_version = resource_version + 1
                 WHERE uid = ?2 AND accepted_revision = ?3",
                params![next_revision, artifact_id, expected],
            )?,
            None => self.conn.execute(
                "UPDATE artifacts SET accepted_revision = ?1, resource_version = resource_version + 1
                 WHERE uid = ?2 AND accepted_revision IS NULL",
                params![next_revision, artifact_id],
            )?,
        };
        Ok(rows == 1)
    }

    pub fn insert_artifact_revision(
        &self,
        company_id: &str,
        revision: &ArtifactRevision,
    ) -> Result<(), StoreError> {
        let (storage_kind, storage_ref) = match &revision.storage {
            StorageRef::Cas(value) => ("cas", value.clone()),
            StorageRef::External(value) => ("external", value.clone()),
        };
        self.conn.execute(
            "INSERT INTO artifact_revisions (
                revision_id, company_id, artifact_id, base_revision, storage_kind, storage_ref,
                digest_algorithm, digest, size_bytes, mime, produced_by, source_note, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                revision.revision_id.as_str(),
                company_id,
                revision.artifact_id.as_str(),
                revision.base_revision.as_ref().map(DomainId::as_str),
                storage_kind,
                storage_ref,
                revision.digest.algorithm.as_str(),
                revision.digest.value,
                revision.size_bytes,
                revision.mime,
                revision.produced_by.as_str(),
                revision.source_note,
                revision.created_at_ms,
            ],
        )?;
        for dependency in &revision.depends_on {
            self.conn.execute(
                "INSERT INTO artifact_dependencies (revision_id, depends_on_revision, kind)
                 VALUES (?1, ?2, ?3)",
                params![
                    revision.revision_id.as_str(),
                    dependency.revision_id.as_str(),
                    dependency.kind.as_str(),
                ],
            )?;
        }
        Ok(())
    }

    pub fn get_artifact_revision(
        &self,
        revision_id: &str,
    ) -> Result<Option<ArtifactRevision>, StoreError> {
        let mut revision = self
            .conn
            .query_row(
                "SELECT artifact_id, base_revision, storage_kind, storage_ref,
                        digest_algorithm, digest, size_bytes, mime, produced_by, source_note,
                        created_at_ms
                 FROM artifact_revisions WHERE revision_id = ?1",
                params![revision_id],
                |row| {
                    let storage_kind: String = row.get(2)?;
                    let storage_ref: String = row.get(3)?;
                    Ok(ArtifactRevision {
                        revision_id: DomainId::new(revision_id.to_string()),
                        artifact_id: DomainId::new(row.get::<_, String>(0)?),
                        base_revision: row.get::<_, Option<String>>(1)?.map(DomainId::new),
                        storage: if storage_kind == "external" {
                            StorageRef::External(storage_ref)
                        } else {
                            StorageRef::Cas(storage_ref)
                        },
                        digest: Digest {
                            algorithm: DigestAlgorithm::parse(&row.get::<_, String>(4)?)
                                .unwrap_or(DigestAlgorithm::Sha256),
                            value: row.get(5)?,
                        },
                        size_bytes: row.get(6)?,
                        mime: row.get(7)?,
                        produced_by: DomainId::new(row.get::<_, String>(8)?),
                        source_note: row.get(9)?,
                        depends_on: Vec::new(),
                        created_at_ms: row.get(10)?,
                    })
                },
            )
            .optional()?;

        if let Some(found) = revision.as_mut() {
            let mut stmt = self.conn.prepare(
                "SELECT depends_on_revision, kind FROM artifact_dependencies WHERE revision_id = ?1",
            )?;
            let rows = stmt.query_map(params![revision_id], |row| {
                Ok(RevisionDependency {
                    revision_id: DomainId::new(row.get::<_, String>(0)?),
                    kind: DependencyKind::parse(&row.get::<_, String>(1)?),
                })
            })?;
            found.depends_on = rows.collect::<Result<Vec<_>, _>>()?;
        }
        Ok(revision)
    }

    /// 供 GC 使用的引用集合：本地 CAS digest 与外部引用计数分开统计。
    pub fn referenced_storage(&self, company_id: &str) -> Result<(Vec<String>, usize), StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT storage_kind, storage_ref FROM artifact_revisions WHERE company_id = ?1",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut cas = Vec::new();
        let mut external = 0usize;
        for row in rows {
            let (kind, value) = row?;
            if kind == "cas" {
                cas.push(value);
            } else {
                external += 1;
            }
        }
        Ok((cas, external))
    }

    pub fn insert_verification_record(
        &self,
        record_id: &str,
        company_id: &str,
        record: &VerificationRecord,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO verification_records (
                record_id, company_id, revision_id, state, actor_id, reason, evidence_refs,
                verification_rules, environment, observed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record_id,
                company_id,
                record.revision_id.as_str(),
                record.state.as_str(),
                record.actor.as_str(),
                record.reason,
                record.evidence_refs.join(ARRAY_SEP),
                record.verification_rules.as_str(),
                record.environment.as_str(),
                record.observed_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 最新一条验证状态；按写入顺序取最后一条。
    pub fn latest_verification(
        &self,
        revision_id: &str,
    ) -> Result<Option<VerificationRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT state, actor_id, reason, evidence_refs, verification_rules, environment,
                        observed_at_ms
                 FROM verification_records WHERE revision_id = ?1
                 ORDER BY observed_at_ms DESC, rowid DESC LIMIT 1",
                params![revision_id],
                |row| {
                    let state = VerificationState::parse(&row.get::<_, String>(0)?)
                        .unwrap_or(VerificationState::Pending);
                    let refs: String = row.get(3)?;
                    Ok(VerificationRecord {
                        revision_id: DomainId::new(revision_id.to_string()),
                        state,
                        actor: DomainId::new(row.get::<_, String>(1)?),
                        reason: row.get(2)?,
                        evidence_refs: split_list(&refs),
                        verification_rules: DomainId::new(row.get::<_, String>(4)?),
                        environment: DomainId::new(row.get::<_, String>(5)?),
                        observed_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn insert_waiver(
        &self,
        waiver_id: &str,
        company_id: &str,
        waiver: &Waiver,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO waivers (
                waiver_id, company_id, revision_id, actor_id, reason,
                scope_company, scope_project, scope_issue, granted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                waiver_id,
                company_id,
                waiver.applies_to_revision.as_str(),
                waiver.actor.as_str(),
                waiver.reason,
                waiver.scope.company_id.as_str(),
                waiver.scope.project_id.as_ref().map(DomainId::as_str),
                waiver.scope.issue_id.as_ref().map(DomainId::as_str),
                waiver.granted_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn insert_acceptance(
        &self,
        company_id: &str,
        record: &AcceptanceRecord,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO acceptances (
                acceptance_id, company_id, artifact_id, work_package_revision, input_baseline,
                artifact_revision, verification_rules, target_environment, merge_combination,
                decision, actor_id, reason, evidence_refs, accepted_at_ms, superseded_by
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                record.acceptance_id.as_str(),
                company_id,
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
                    .map(DomainId::as_str),
                record.decision.as_str(),
                record.actor.as_str(),
                record.reason,
                record.evidence_refs.join(ARRAY_SEP),
                record.accepted_at_ms,
                record.superseded_by.as_ref().map(DomainId::as_str),
            ],
        )?;
        Ok(())
    }

    /// 新接受生效时把同一 Artifact 的**其他**旧接受标记为被取代（保留历史，不删除）。
    pub fn supersede_acceptances(
        &self,
        artifact_id: &str,
        superseded_by: &str,
    ) -> Result<usize, StoreError> {
        let rows = self.conn.execute(
            "UPDATE acceptances SET superseded_by = ?1
             WHERE artifact_id = ?2 AND superseded_by IS NULL AND acceptance_id <> ?1",
            params![superseded_by, artifact_id],
        )?;
        Ok(rows)
    }

    pub fn list_acceptances(&self, artifact_id: &str) -> Result<Vec<AcceptanceRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT acceptance_id, work_package_revision, input_baseline, artifact_revision,
                    verification_rules, target_environment, merge_combination, decision,
                    actor_id, reason, evidence_refs, accepted_at_ms, superseded_by
             FROM acceptances WHERE artifact_id = ?1 ORDER BY accepted_at_ms ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![artifact_id], |row| {
            let decision = AcceptanceDecision::parse(&row.get::<_, String>(7)?)
                .unwrap_or(AcceptanceDecision::RequiresHuman);
            let evidence: String = row.get(10)?;
            Ok(AcceptanceRecord {
                acceptance_id: DomainId::new(row.get::<_, String>(0)?),
                artifact_id: DomainId::new(artifact_id.to_string()),
                binding: AcceptanceBinding {
                    work_package_revision: DomainId::new(row.get::<_, String>(1)?),
                    input_baseline: DomainId::new(row.get::<_, String>(2)?),
                    artifact_revision: DomainId::new(row.get::<_, String>(3)?),
                    verification_rules: DomainId::new(row.get::<_, String>(4)?),
                    target_environment: DomainId::new(row.get::<_, String>(5)?),
                    merge_combination: row.get::<_, Option<String>>(6)?.map(DomainId::new),
                },
                decision,
                actor: DomainId::new(row.get::<_, String>(8)?),
                reason: row.get(9)?,
                evidence_refs: split_list(&evidence),
                accepted_at_ms: row.get(11)?,
                superseded_by: row.get::<_, Option<String>>(12)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    pub fn insert_decision(
        &self,
        company_id: &str,
        record: &crate::domain::artifact::DecisionRecord,
    ) -> Result<(), StoreError> {
        let revisions = record
            .revision_refs
            .iter()
            .map(DomainId::as_str)
            .collect::<Vec<_>>()
            .join(ARRAY_SEP);
        self.conn.execute(
            "INSERT INTO decisions (
                decision_id, company_id, actor_id, scope_company, scope_project, scope_issue,
                decision, reason, evidence_refs, revision_refs, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.decision_id.as_str(),
                company_id,
                record.actor.as_str(),
                record.scope.company_id.as_str(),
                record.scope.project_id.as_ref().map(DomainId::as_str),
                record.scope.issue_id.as_ref().map(DomainId::as_str),
                record.decision,
                record.reason,
                record.evidence_refs.join(ARRAY_SEP),
                revisions,
                record.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_work_package_state(
        &self,
        company_id: &str,
        issue_uid: &str,
        state: &WorkPackageState,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO work_package_states (
                company_id, issue_uid, revision_id, required, accepted_revision, running, blockers
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(issue_uid, revision_id) DO UPDATE SET
                required = excluded.required,
                accepted_revision = excluded.accepted_revision,
                running = excluded.running,
                blockers = excluded.blockers",
            params![
                company_id,
                issue_uid,
                state.revision.as_str(),
                i64::from(u8::from(state.required)),
                state.accepted_revision.as_ref().map(DomainId::as_str),
                i64::from(u8::from(state.running)),
                state.blockers.join(ARRAY_SEP),
            ],
        )?;
        Ok(())
    }

    pub fn list_work_package_states(
        &self,
        issue_uid: &str,
    ) -> Result<Vec<WorkPackageState>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revision_id, required, accepted_revision, running, blockers
             FROM work_package_states WHERE issue_uid = ?1 ORDER BY revision_id",
        )?;
        let rows = stmt.query_map(params![issue_uid], |row| {
            let blockers: String = row.get(4)?;
            Ok(WorkPackageState {
                revision: DomainId::new(row.get::<_, String>(0)?),
                required: row.get::<_, i64>(1)? == 1,
                accepted_revision: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                running: row.get::<_, i64>(3)? == 1,
                blockers: split_list(&blockers),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 某 Issue 的决策记录条数（供控制 API 与冒烟断言）。
    pub fn count_decisions(&self, issue_uid: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE scope_issue = ?1",
                params![issue_uid],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    pub fn put_migration_record(&self, record: &MigrationRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO migration_registry (
                migration_id, checksum, depends_on, schema_range, software_range, risk,
                compatibility_window, backup_notes, data_notes, applied_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(migration_id) DO NOTHING",
            params![
                record.migration_id,
                record.checksum,
                record.depends_on_text(),
                record.schema_range,
                record.software_range,
                record.risk.as_str(),
                record.compatibility_window,
                record.backup_notes,
                record.data_notes,
                record.applied_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn put_backup_record(&self, record: &BackupRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO backups (
                backup_id, company_id, path, db_sha256, db_bytes, wal_included,
                snapshot_method, cas_object_count, config_revision, adapter_versions,
                key_recovery_note, status, problems, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.backup_id,
                record.company_id.as_str(),
                record.path,
                record.db_sha256,
                record.db_bytes,
                i64::from(u8::from(record.wal_included)),
                record.snapshot_method,
                record.cas_object_count,
                record.config_revision,
                record.adapter_versions,
                record.key_recovery_note,
                record.status.as_str(),
                record.problems.join(ARRAY_SEP),
                record.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn put_reconciliation_case(&self, case: &ReconciliationCase) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO reconciliation_cases (
                case_id, company_id, invocation_id, logical_action_key, impact_scope,
                risk_reservation_refs, known_evidence, assignee, next_step, state,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(case_id) DO UPDATE SET
                known_evidence = excluded.known_evidence,
                assignee = excluded.assignee,
                next_step = excluded.next_step,
                state = excluded.state,
                updated_at_ms = excluded.updated_at_ms",
            params![
                case.case_id,
                case.company_id.as_str(),
                case.invocation_id,
                case.logical_action_key,
                case.impact_scope,
                case.risk_reservation_refs.join(ARRAY_SEP),
                case.known_evidence,
                case.assignee,
                case.next_step,
                case.state.as_str(),
                case.created_at_ms,
                case.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 幂等写入：同一目标只保留一条清理记录，重复请求返回已存在记录。
    pub fn put_cleanup_request(&self, request: &CleanupRequest) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "INSERT INTO cleanup_requests (
                request_id, company_id, target_kind, target_id, state, blockers,
                requested_by, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(company_id, target_kind, target_id) DO NOTHING",
            params![
                request.request_id,
                request.company_id.as_str(),
                request.target_kind.as_str(),
                request.target_id,
                request.state.as_str(),
                request.blockers.join(ARRAY_SEP),
                request.requested_by,
                request.created_at_ms,
                request.updated_at_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    pub fn update_cleanup_state(
        &self,
        request_id: &str,
        state: CleanupState,
        blockers: &[String],
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE cleanup_requests SET state = ?1, blockers = ?2, updated_at_ms = ?3
             WHERE request_id = ?4",
            params![state.as_str(), blockers.join(ARRAY_SEP), now_ms, request_id],
        )?;
        Ok(())
    }

    pub fn put_cleanup_abandon(&self, abandon: &CleanupAbandon) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO cleanup_abandons (
                abandon_id, company_id, request_id, residual_resources, risks, owner,
                reconciliation_entry, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                abandon.abandon_id,
                abandon.company_id.as_str(),
                abandon.request_id,
                abandon.residual_resources.join(ARRAY_SEP),
                abandon.risks.join(ARRAY_SEP),
                abandon.owner,
                abandon.reconciliation_entry,
                abandon.created_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 事务内读取清理请求：派发门禁必须在同一事务里看到删除标记。
    pub fn get_cleanup_request(
        &self,
        company_id: &str,
        kind: CleanupTargetKind,
        target_id: &str,
    ) -> Result<Option<CleanupRequest>, StoreError> {
        read_cleanup_request(self.conn, company_id, kind, target_id)
    }

    /// 标记项目内 Issue 的 `deletionRequestedAt`（幂等：已有标记不覆盖）。
    ///
    /// 该标记是"停止新派发"的事实来源，与 `cleanup_requests` 行一起在同一事务写入。
    pub fn mark_project_deletion_requested(
        &self,
        company_id: &str,
        project_id: &str,
        now_ms: i64,
    ) -> Result<usize, StoreError> {
        let rows = self.conn.execute(
            "UPDATE issues SET deletion_requested_at = ?3
             WHERE company_id = ?1 AND project_id = ?2 AND deletion_requested_at IS NULL",
            params![company_id, project_id, now_ms.to_string()],
        )?;
        Ok(rows)
    }

    /// 标记单个 Issue 的删除请求。
    pub fn mark_issue_deletion_requested(
        &self,
        company_id: &str,
        issue_uid: &str,
        now_ms: i64,
    ) -> Result<usize, StoreError> {
        let rows = self.conn.execute(
            "UPDATE issues SET deletion_requested_at = ?3
             WHERE company_id = ?1 AND uid = ?2 AND deletion_requested_at IS NULL",
            params![company_id, issue_uid, now_ms.to_string()],
        )?;
        Ok(rows)
    }
}

/// 事务外/内共用的清理请求读取：`SqliteStore` 与 `UnitOfWork` 走同一 SQL。
fn read_cleanup_request(
    conn: &Connection,
    company_id: &str,
    kind: CleanupTargetKind,
    target_id: &str,
) -> Result<Option<CleanupRequest>, StoreError> {
    conn.query_row(
        "SELECT request_id, state, blockers, requested_by, created_at_ms, updated_at_ms
         FROM cleanup_requests
         WHERE company_id = ?1 AND target_kind = ?2 AND target_id = ?3",
        params![company_id, kind.as_str(), target_id],
        |row| {
            let blockers: String = row.get(2)?;
            Ok(CleanupRequest {
                request_id: row.get(0)?,
                company_id: DomainId::new(company_id.to_string()),
                target_kind: kind,
                target_id: target_id.to_string(),
                state: CleanupState::parse(&row.get::<_, String>(1)?),
                blockers: split_list(&blockers),
                requested_by: row.get(3)?,
                created_at_ms: row.get(4)?,
                updated_at_ms: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::Sql)
}

/// 恢复报告用的核心行数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoreRowCounts {
    pub issues: i64,
    pub revisions: i64,
    pub invocations: i64,
    pub acceptances: i64,
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

fn permission_str(permission: Permission) -> &'static str {
    match permission {
        Permission::Dispatch => "Dispatch",
        Permission::ManageBudget => "ManageBudget",
        Permission::Accept => "Accept",
        Permission::ManageScope => "ManageScope",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Phase, Principal, ResourceMeta, ResourceStatus, Scope, WriterKind};
    use crate::domain::rules::assign_issue;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_db() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bod-store-test-{}-{n}.db", std::process::id()))
    }

    fn config() -> BodConfig {
        let mut config = BodConfig::default();
        config.sqlite.busy_timeout_ms = 5_000;
        config
    }

    fn issue() -> Issue {
        Issue {
            meta: ResourceMeta {
                uid: DomainId::new("issue-1"),
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: IssueKind::Coding,
            repository: Some(crate::domain::RepositoryContext {
                provider: "local".to_string(),
                repository_id: "repo-1".to_string(),
            }),
            spec: crate::domain::IssueSpec {
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
    fn audit_event_round_trips_and_keeps_write_order() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let allowed = audit_event(AuditDecision::Allowed, None);
        let denied = audit_event(AuditDecision::Denied, Some("不是该项目成员"));
        // 两条用同一个业务时刻：读回顺序只能靠自增序号，不能靠时间戳。
        let (first, second) = with_uow(&mut store, |uow| {
            let first = uow
                .append_audit_event(&allowed, 1_763_500_000_000)
                .expect("放行事件应可写入");
            let second = uow
                .append_audit_event(&denied, 1_763_500_000_001)
                .expect("拒绝事件应可写入");
            (first, second)
        });
        assert!(second > first, "序号必须单调递增");

        let rows = store
            .list_audit_events("company-default", "req-1")
            .expect("应可读回");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].event.decision, AuditDecision::Allowed);
        assert_eq!(rows[1].event.decision, AuditDecision::Denied);
        assert_eq!(rows[1].event.action, StageAction::Submit);
        assert_eq!(rows[1].event.basis, "submit:not_a_member");
        assert_eq!(rows[1].event.reason.as_deref(), Some("不是该项目成员"));
        assert_eq!(
            rows[0]
                .event
                .scope
                .project_id
                .as_ref()
                .map(DomainId::as_str),
            Some("project-1")
        );
        assert_eq!(rows[0].recorded_at_ms, 1_763_500_000_000);
    }

    #[test]
    fn audit_rejects_invalid_event_before_touching_sql() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let invalid = audit_event(AuditDecision::Denied, None);
        let err = with_uow(&mut store, |uow| {
            uow.append_audit_event(&invalid, 1_763_500_000_000)
                .expect_err("拒绝不带原因应被拦下")
        });
        assert!(
            matches!(err, StoreError::Audit(_)),
            "应为审计校验错误而不是 SQL 错误，实际 {err}"
        );
        assert_eq!(
            store
                .count_audit_events("company-default", AuditDecision::Denied)
                .expect("计数应可读"),
            0,
            "被拒绝的事件不得落库"
        );
    }

    #[test]
    fn audit_rows_are_immutable() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.append_audit_event(&audit_event(AuditDecision::Allowed, None), 1)
                .expect("写入应成功");
        });
        let update = store
            .conn
            .execute("UPDATE audit_events SET basis = '改过'", [])
            .expect_err("审计不得改写");
        assert!(update.to_string().contains("immutable"), "{update}");
        let delete = store
            .conn
            .execute("DELETE FROM audit_events", [])
            .expect_err("审计不得删除");
        assert!(delete.to_string().contains("immutable"), "{delete}");
        assert_eq!(
            store
                .count_audit_events("company-default", AuditDecision::Allowed)
                .expect("计数应可读"),
            1,
            "被 trigger 拒绝后行数不变"
        );
    }

    #[test]
    fn audit_counts_by_decision_and_reads_newest_first() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            for (index, decision) in [
                AuditDecision::Allowed,
                AuditDecision::Denied,
                AuditDecision::Denied,
            ]
            .into_iter()
            .enumerate()
            {
                let reason = (decision == AuditDecision::Denied).then_some("缺 Grant");
                uow.append_audit_event(&audit_event(decision, reason), index as i64)
                    .expect("写入应成功");
            }
        });
        assert_eq!(
            store
                .count_audit_events("company-default", AuditDecision::Allowed)
                .expect("计数应可读"),
            1
        );
        assert_eq!(
            store
                .count_audit_events("company-default", AuditDecision::Denied)
                .expect("计数应可读"),
            2
        );
        let latest = store
            .latest_audit_events("company-default", 2)
            .expect("应可读回");
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].sequence, 3);
        assert_eq!(latest[1].sequence, 2);
        assert_eq!(
            store
                .count_audit_events("other-company", AuditDecision::Allowed)
                .expect("计数应可读"),
            0,
            "跨公司不得串读"
        );
    }

    /// 读侧严格性：`action` 没有库层 CHECK（动作集合会随 §6/§11 扩展，
    /// 用 CHECK 锁死会让每次加动作都要写迁移），所以靠读侧拒绝。
    /// 这条测试证明「绕过应用写库」不会静默变成某个已知动作。
    #[test]
    fn audit_reader_rejects_unknown_action() {
        let store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        store
            .conn
            .execute(
                "INSERT INTO audit_events (company_id, correlation_id, actor_id, action,
                    scope_company, scope_project, scope_issue, basis, decision, reason,
                    decided_at, recorded_at_ms)
                 VALUES ('company-default', 'req-1', 'u-1', 'approve',
                    'company-default', 'project-1', NULL, 'approve:grant', 'allowed', NULL,
                    '2026-11-21T10:00:00Z', 1)",
                [],
            )
            .expect("库层不拦表外动作，正是读侧要拦的原因");
        let err = store
            .list_audit_events("company-default", "req-1")
            .expect_err("表外动作必须报错而不是回退");
        assert!(err.to_string().contains("表外值"), "{err}");
    }

    /// 审计写入走一次真事务：审计必须在与业务事实同一个事务边界里提交，
    /// 否则会出现「业务回滚了、审计里却记着发生过」。
    fn with_uow<T>(store: &mut SqliteStore, body: impl FnOnce(&UnitOfWork<'_>) -> T) -> T {
        let uow = UnitOfWork::begin(&mut store.conn).expect("开启事务");
        let out = body(&uow);
        uow.commit().expect("提交事务");
        out
    }

    /// 审计事件的最小合法样本：范围取 project 级（issue 可空）。
    fn audit_event(decision: AuditDecision, reason: Option<&str>) -> AuditEvent {
        AuditEvent {
            company_id: "company-default".to_string(),
            correlation_id: "req-1".to_string(),
            actor: DomainId::new("user-owner"),
            action: StageAction::Submit,
            scope: Scope {
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                issue_id: None,
            },
            basis: if decision == AuditDecision::Allowed {
                "submit:project_membership"
            } else {
                "submit:not_a_member"
            }
            .to_string(),
            decision,
            reason: reason.map(str::to_string),
            decided_at: "2026-11-21T10:00:00Z".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // 交付跟踪（P8 3.1）
    // -----------------------------------------------------------------------

    fn delivery_tracking(company: &str, id: &str) -> TrackingRow {
        TrackingRow {
            tracking_id: id.to_string(),
            company_id: company.to_string(),
            project_id: "project-1".to_string(),
            title: "客户 A 的修复".to_string(),
            requirement: "登录失败".to_string(),
            owner_id: "alice".to_string(),
            state: crate::delivery_tracking::TrackingState::Open,
            issue_uid: None,
            run_uid: None,
            attempt_id: None,
            resource_version: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn delivery_target(tracking_id: &str, id: &str, customer: &str, unit: &str) -> TargetRow {
        TargetRow {
            target_id: id.to_string(),
            company_id: "company-default".to_string(),
            tracking_id: tracking_id.to_string(),
            customer_id: customer.to_string(),
            environment: "prod".to_string(),
            deployment_unit: unit.to_string(),
            acceptance: "冒烟通过".to_string(),
            switch_state: crate::coverage_receipt::SwitchState::Synced,
            population_known: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn delivery_observation(
        id: &str,
        target_id: &str,
        kind: ObservationKind,
        at_ms: i64,
        revision: &str,
        digest: &str,
        note: &str,
    ) -> ObservationRow {
        ObservationRow {
            observation_id: id.to_string(),
            company_id: "company-default".to_string(),
            tracking_id: "dt-1".to_string(),
            target_id: target_id.to_string(),
            kind,
            source_kind: if kind == ObservationKind::ManualEvidence {
                "manual"
            } else {
                "connector"
            },
            source_ref: "acme/repo#318".to_string(),
            source_revision_id: None,
            artifact_revision: Some(revision.to_string()),
            digest: digest.to_string(),
            revision: format!("deploy-{id}"),
            note: note.to_string(),
            actor_id: "alice".to_string(),
            authorized: false,
            observed_at: format!("2026-11-21T10:00:{:02}Z", at_ms % 60),
            observed_at_ms: at_ms,
            recorded_at_ms: at_ms,
        }
    }

    fn insert_verification(
        store: &SqliteStore,
        record_id: &str,
        revision: &str,
        state: &str,
        at_ms: i64,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO verification_records (record_id, company_id, revision_id, state,
                    actor_id, reason, evidence_refs, verification_rules, environment, observed_at_ms)
                 VALUES (?1, 'company-default', ?2, ?3, 'ci', '', '[]', 'smoke', 'prod', ?4)",
                params![record_id, revision, state, at_ms],
            )
            .expect("验证记录应可写入");
    }

    /// 三个目标，覆盖三种结论来源：连接器部署、未授权人工补证、豁免验证。
    fn seed_delivery(store: &mut SqliteStore) {
        with_uow(store, |uow| {
            uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))
                .expect("跟踪项应可写入");
            for (id, customer, unit) in [
                ("t-1", "acme", "web"),
                ("t-2", "beta", ""),
                ("t-3", "gamma", ""),
            ] {
                uow.insert_delivery_target(&delivery_target("dt-1", id, customer, unit))
                    .expect("目标应可写入");
            }
            uow.append_delivery_observation(&delivery_observation(
                "o-1",
                "t-1",
                ObservationKind::Deployment,
                100,
                "rev-1",
                "sha256:abc",
                "部署到客户 A 生产环境",
            ))
            .expect("部署观察应可写入");
            // 未授权的人工补证：能证明「现场装了」，但不能据此声称已覆盖。
            uow.append_delivery_observation(&delivery_observation(
                "o-2",
                "t-2",
                ObservationKind::ManualEvidence,
                110,
                "rev-2",
                "sha256:abc",
                "离线安装，现场核对完成，授权单尚未登记",
            ))
            .expect("人工补证应可写入");
            uow.append_delivery_observation(&delivery_observation(
                "o-3",
                "t-3",
                ObservationKind::Deployment,
                120,
                "rev-3",
                "sha256:def",
                "部署到客户 C 生产环境",
            ))
            .expect("部署观察应可写入");
        });
        insert_verification(store, "vr-1", "rev-1", "verified", 100);
        insert_verification(store, "vr-2", "rev-2", "verified", 110);
        insert_verification(store, "vr-3", "rev-3", "waived", 120);
    }

    /// 读路径：结论从事实算出来，且**读不写库**。
    #[test]
    fn delivery_view_derives_coverage_and_writes_nothing() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        seed_delivery(&mut store);
        let before: i64 = store
            .read_connection()
            .query_row("SELECT COUNT(*) FROM delivery_observations", [], |row| {
                row.get(0)
            })
            .expect("应能计数");

        let view = store
            .delivery_tracking_view("company-default", "dt-1")
            .expect("读取应成功")
            .expect("跟踪项应存在");

        assert_eq!(view.targets.len(), 3);
        // 目标按客户名排序，输出稳定。
        assert_eq!(view.targets[0].target.customer_id, "acme");
        assert!(
            view.targets[0].verdict.is_covered(),
            "{:?}",
            view.targets[0]
        );
        assert_eq!(view.targets[0].evidence_revision.as_deref(), Some("o-1"));

        // 未授权补证：待补安装，缺口同时说明「没观察到部署」与「回执未授权」。
        assert!(matches!(
            view.targets[1].verdict,
            crate::coverage_receipt::CoverageVerdict::PendingInstall { .. }
        ));
        assert_eq!(
            view.targets[1].gaps,
            vec![
                crate::delivery_tracking::GAP_DEPLOYMENT_NOT_OBSERVED,
                crate::delivery_tracking::GAP_INSTALL_RECEIPT_UNAUTHORIZED
            ]
        );

        // 豁免不等于验过：仍然停在待核实。
        assert!(matches!(
            view.targets[2].verdict,
            crate::coverage_receipt::CoverageVerdict::PendingVerification { .. }
        ));
        assert_eq!(
            view.targets[2].gaps,
            vec![crate::delivery_tracking::GAP_VERIFICATION_NOT_PASSED]
        );

        assert_eq!(view.evidence.len(), 3, "三条观察都是证据");

        let after: i64 = store
            .read_connection()
            .query_row("SELECT COUNT(*) FROM delivery_observations", [], |row| {
                row.get(0)
            })
            .expect("应能计数");
        assert_eq!(before, after, "读路径不得写入任何观察");
    }

    /// 观察是历史：UPDATE / DELETE 由触发器直接拒绝。
    #[test]
    fn delivery_observations_are_append_only() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        seed_delivery(&mut store);
        let update = store.execute_batch_for_test(
            "UPDATE delivery_observations SET note = '改一下' WHERE observation_id = 'o-1'",
        );
        assert!(update.is_err(), "改写观察必须被拒绝");
        let delete = store.execute_batch_for_test(
            "DELETE FROM delivery_observations WHERE observation_id = 'o-1'",
        );
        assert!(delete.is_err(), "删除观察必须被拒绝");
    }

    /// 人工补证没有授权范围与复验依据：写入前就拒绝，不等库层 CHECK。
    #[test]
    fn manual_evidence_without_note_is_rejected() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))
                .expect("跟踪项应可写入");
            uow.insert_delivery_target(&delivery_target("dt-1", "t-1", "acme", "web"))
                .expect("目标应可写入");
        });
        let err = with_uow(&mut store, |uow| {
            uow.append_delivery_observation(&delivery_observation(
                "o-1",
                "t-1",
                ObservationKind::ManualEvidence,
                100,
                "rev-1",
                "",
                "   ",
            ))
        })
        .expect_err("空备注的人工补证必须被拒绝");
        assert!(err.to_string().contains("授权范围"), "{err}");
    }

    /// 观察不能挂到别的跟踪项的目标上：跨跟踪项引用是掩盖错误的捷径。
    #[test]
    fn observation_cannot_claim_another_trackings_target() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))
                .expect("跟踪项应可写入");
            uow.insert_delivery_target(&delivery_target("dt-1", "t-1", "acme", "web"))
                .expect("目标应可写入");
            uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-2"))
                .expect("第二个跟踪项应可写入");
        });
        let mut observation = delivery_observation(
            "o-1",
            "t-1",
            ObservationKind::Deployment,
            100,
            "rev-1",
            "sha256:abc",
            "部署",
        );
        observation.tracking_id = "dt-2".to_string();
        let err = with_uow(&mut store, |uow| {
            uow.append_delivery_observation(&observation)
        })
        .expect_err("跨跟踪项引用必须被拒绝");
        assert!(err.to_string().contains("不属于"), "{err}");
    }

    /// 读按 Company 隔离：别的公司看不到这条跟踪项。
    #[test]
    fn delivery_view_is_company_scoped() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        seed_delivery(&mut store);
        assert!(
            store
                .delivery_tracking_view("company-default", "dt-1")
                .expect("读取应成功")
                .is_some()
        );
        assert!(
            store
                .delivery_tracking_view("company-other", "dt-1")
                .expect("读取应成功")
                .is_none()
        );
    }

    /// 列表只给规模与身份，不给结论；目标计数用一次 GROUP BY，不按行回查。
    #[test]
    fn list_delivery_tracking_counts_targets() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        seed_delivery(&mut store);
        let mut second = delivery_tracking("company-default", "dt-2");
        second.created_at_ms = 2;
        with_uow(&mut store, |uow| {
            uow.insert_delivery_tracking(&second)
                .expect("跟踪项应可写入");
        });

        let rows = store
            .list_delivery_tracking("company-default", 10)
            .expect("列表应可读");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.tracking_id, "dt-2", "按创建时间倒序");
        assert_eq!(rows[0].1, 0);
        assert_eq!(rows[1].0.tracking_id, "dt-1");
        assert_eq!(rows[1].1, 3);

        let limited = store
            .list_delivery_tracking("company-default", 1)
            .expect("列表应可读");
        assert_eq!(limited.len(), 1, "limit 必须生效");
    }

    // -----------------------------------------------------------------------
    // 来源身份与候选（P8 3.2）
    // -----------------------------------------------------------------------

    fn source_identity(project: &str, provider_id: &str) -> crate::delivery_source::SourceIdentity {
        crate::delivery_source::SourceIdentity {
            company_id: "company-default".to_string(),
            project_id: project.to_string(),
            connection_id: "gh-1".to_string(),
            object_type: "pull-request".to_string(),
            provider_object_id: provider_id.to_string(),
        }
    }

    fn source_proposal(locator: &str) -> crate::delivery_source::CandidateProposal {
        crate::delivery_source::CandidateProposal {
            company_id: "company-default".to_string(),
            project_id: "project-1".to_string(),
            connection_id: "gh-1".to_string(),
            // 只有 URL 时对象类型往往还没解析出来：空串表示「未定」，不猜。
            object_type: String::new(),
            locator: locator.to_string(),
        }
    }

    fn source_revision(
        id: &str,
        source_id: &str,
        version: &str,
        digest: &str,
        at_ms: i64,
    ) -> crate::delivery_source::SourceRevisionRow {
        let fingerprint =
            crate::delivery_source::revision_fingerprint(version, digest).expect("指纹应可生成");
        crate::delivery_source::SourceRevisionRow {
            source_revision_id: id.to_string(),
            company_id: "company-default".to_string(),
            source_id: source_id.to_string(),
            version: version.to_string(),
            digest: digest.to_string(),
            content_fingerprint: fingerprint,
            actor_id: "ci".to_string(),
            observed_at: format!("2026-11-21T10:00:{:02}Z", at_ms % 60),
            observed_at_ms: at_ms,
            recorded_at_ms: at_ms,
        }
    }

    /// 五元组相同 → 只登记一次；首个确认人不被后来的调用覆盖。
    #[test]
    fn same_source_identity_is_registered_once() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let identity = source_identity("project-1", "PR_kw1");

        let first = with_uow(&mut store, |uow| {
            uow.register_source("src-1", &identity, "acme/web#318", "alice", 100)
        })
        .expect("首次登记应成功");
        assert!(first.created);
        assert_eq!(first.source_id, "src-1");

        let again = with_uow(&mut store, |uow| {
            uow.register_source("src-2", &identity, "acme/web#318", "bob", 200)
        })
        .expect("重复登记应返回已有身份");
        assert!(!again.created, "重复登记不得新建");
        assert_eq!(again.source_id, "src-1");

        let row = store
            .get_source("company-default", "src-1")
            .expect("读取应成功")
            .expect("来源应存在");
        assert_eq!(row.confirmed_by, "alice", "首个确认人是历史事实，不被覆盖");
        assert_eq!(row.confirmed_at_ms, 100);
        assert_eq!(
            store
                .list_sources("company-default", None, 10)
                .expect("列表应可读")
                .len(),
            1
        );
    }

    /// 跨项目同名 PR：五元组里 project 不同就是两个来源，不会因编号相同而合并。
    #[test]
    fn same_number_in_two_projects_stay_two_sources() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.register_source(
                "src-1",
                &source_identity("project-1", "PR_kw318"),
                "acme/web#318",
                "alice",
                100,
            )
            .expect("项目 1 的来源应可登记");
            uow.register_source(
                "src-2",
                &source_identity("project-2", "PR_kw318"),
                "acme/api#318",
                "alice",
                100,
            )
            .expect("项目 2 的来源应可登记");
        });

        assert_eq!(
            store
                .list_sources("company-default", Some("project-1"), 10)
                .expect("列表应可读")
                .len(),
            1
        );
        assert_eq!(
            store
                .list_sources("company-default", None, 10)
                .expect("列表应可读")
                .len(),
            2
        );
    }

    /// 同一事件重复上报只确认一次；同一标签两个摘要留两条修订。
    #[test]
    fn source_revision_is_idempotent_per_event() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.register_source(
                "src-1",
                &source_identity("project-1", "PR_kw1"),
                "acme/web#318",
                "alice",
                100,
            )
            .expect("来源应可登记");
        });

        let first = with_uow(&mut store, |uow| {
            uow.append_source_revision(&source_revision(
                "rev-1",
                "src-1",
                "v1.4.2",
                "sha256:aaa",
                100,
            ))
        })
        .expect("首次修订应可写入");
        assert!(first.created);

        // 同一 (version, digest) 隔一分钟再报：仍是同一条修订。
        let duplicate = with_uow(&mut store, |uow| {
            uow.append_source_revision(&source_revision(
                "rev-2",
                "src-1",
                "v1.4.2",
                "sha256:aaa",
                160,
            ))
        })
        .expect("重复事件应返回已有修订");
        assert!(!duplicate.created, "同一事件不得新增修订");
        assert_eq!(duplicate.source_revision_id, "rev-1");

        // 可变标签：同一版本换了摘要 → 新的观察，必须留痕。
        let changed = with_uow(&mut store, |uow| {
            uow.append_source_revision(&source_revision(
                "rev-3",
                "src-1",
                "v1.4.2",
                "sha256:bbb",
                220,
            ))
        })
        .expect("不同摘要应可追加");
        assert!(changed.created);

        let revisions = store
            .list_source_revisions("company-default", "src-1")
            .expect("时间线应可读");
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0].source_revision_id, "rev-1", "按观察时间升序");
        assert_eq!(revisions[1].digest, "sha256:bbb");
        assert_eq!(
            crate::delivery_source::digest_history(&revisions).len(),
            2,
            "两次解析都要在时间线里"
        );
    }

    /// 指纹必须由内容派生：调用方自己编的指纹会让幂等键失效。
    #[test]
    fn revision_fingerprint_must_match_content() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.register_source(
                "src-1",
                &source_identity("project-1", "PR_kw1"),
                "acme/web#318",
                "alice",
                100,
            )
            .expect("来源应可登记");
        });
        let mut revision = source_revision("rev-1", "src-1", "v1", "sha256:aaa", 100);
        revision.content_fingerprint = "forged".to_string();
        let err = with_uow(&mut store, |uow| uow.append_source_revision(&revision))
            .expect_err("伪造指纹必须被拒绝");
        assert!(
            matches!(
                err,
                StoreError::Source(crate::delivery_source::SourceError::FingerprintMismatch { .. })
            ),
            "{err:?}"
        );
    }

    /// 修订是历史：库里拒绝改写，回滚与新解析都只能追加。
    #[test]
    fn source_revisions_are_append_only() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.register_source(
                "src-1",
                &source_identity("project-1", "PR_kw1"),
                "acme/web#318",
                "alice",
                100,
            )
            .expect("来源应可登记");
            uow.append_source_revision(&source_revision("rev-1", "src-1", "v1", "sha256:aaa", 100))
                .expect("修订应可写入");
        });
        let update = store.conn.execute(
            "UPDATE delivery_source_revisions SET digest = 'sha256:bbb' WHERE source_revision_id = 'rev-1'",
            [],
        );
        assert!(update.is_err(), "改写修订必须被触发器拒绝");
        let delete = store.conn.execute(
            "DELETE FROM delivery_source_revisions WHERE source_revision_id = 'rev-1'",
            [],
        );
        assert!(delete.is_err(), "删除修订必须被触发器拒绝");
    }

    /// 候选确认：一次动作两个写入，且只能发生一次。
    #[test]
    fn candidate_confirmation_creates_identity_and_is_final() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let proposed = with_uow(&mut store, |uow| {
            uow.propose_source_candidate(
                "cand-1",
                &source_proposal("https://github.com/acme/web/pull/318"),
                "alice",
                100,
            )
        })
        .expect("候选应可提交");
        assert!(proposed.created);

        // 同一 URL 再提一次：还是一条待办，不是第二条。
        let duplicate = with_uow(&mut store, |uow| {
            uow.propose_source_candidate(
                "cand-2",
                &source_proposal("https://github.com/acme/web/pull/318"),
                "bob",
                120,
            )
        })
        .expect("重复候选应返回已有待办");
        assert!(!duplicate.created);
        assert_eq!(duplicate.candidate_id, "cand-1");

        let confirmed = with_uow(&mut store, |uow| {
            uow.confirm_source_candidate(
                "company-default",
                "cand-1",
                "src-1",
                &source_identity("project-1", "PR_kw1"),
                "alice",
                200,
            )
        })
        .expect("确认应成功");
        assert!(confirmed.source_created);
        assert_eq!(confirmed.source_id, "src-1");

        let candidates = store
            .list_source_candidates("company-default", None, false, 10)
            .expect("候选列表应可读");
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].state,
            crate::delivery_source::CandidateState::Confirmed
        );
        assert_eq!(candidates[0].resolved_source_id.as_deref(), Some("src-1"));
        assert_eq!(candidates[0].decided_by, "alice");

        // 再确认一次：拒绝，且不得留下第二个身份。
        let err = with_uow(&mut store, |uow| {
            uow.confirm_source_candidate(
                "company-default",
                "cand-1",
                "src-9",
                &source_identity("project-1", "PR_kw2"),
                "bob",
                300,
            )
        })
        .expect_err("已裁决的候选不得再确认");
        assert!(
            matches!(
                err,
                StoreError::Source(crate::delivery_source::SourceError::CandidateAlreadyDecided(_))
            ),
            "{err:?}"
        );
        assert_eq!(
            store
                .list_sources("company-default", None, 10)
                .expect("列表应可读")
                .len(),
            1,
            "失败的确认不得留下身份"
        );
    }

    /// 确认不得把候选挪到别的项目：候选的作用域就是它被提交时的项目。
    #[test]
    fn candidate_scope_cannot_move_to_another_project() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.propose_source_candidate(
                "cand-1",
                &source_proposal("https://github.com/acme/web/pull/318"),
                "alice",
                100,
            )
            .expect("候选应可提交");
        });
        let err = with_uow(&mut store, |uow| {
            uow.confirm_source_candidate(
                "company-default",
                "cand-1",
                "src-1",
                &source_identity("project-2", "PR_kw1"),
                "alice",
                200,
            )
        })
        .expect_err("跨项目确认必须拒绝");
        assert!(
            matches!(
                err,
                StoreError::Source(
                    crate::delivery_source::SourceError::CandidateScopeMismatch { .. }
                )
            ),
            "{err:?}"
        );
        assert!(
            store
                .list_sources("company-default", None, 10)
                .expect("列表应可读")
                .is_empty()
        );
    }

    /// 观察挂来源修订：同项目才允许，且证据引用改用来源修订 ID。
    #[test]
    fn observation_links_same_project_source_revision() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        seed_delivery(&mut store);
        with_uow(&mut store, |uow| {
            uow.register_source(
                "src-1",
                &source_identity("project-1", "PR_kw1"),
                "acme/web#318",
                "alice",
                100,
            )
            .expect("来源应可登记");
            uow.append_source_revision(&source_revision("rev-1", "src-1", "v1", "sha256:abc", 100))
                .expect("修订应可写入");
        });

        let mut linked = delivery_observation(
            "o-9",
            "t-1",
            ObservationKind::Deployment,
            300,
            "rev-1",
            "sha256:abc",
            "",
        );
        linked.source_revision_id = Some("rev-1".to_string());
        with_uow(&mut store, |uow| {
            uow.append_delivery_observation(&linked)
                .expect("同项目来源的观察应可写入");
        });

        let view = store
            .delivery_tracking_view("company-default", "dt-1")
            .expect("读取应成功")
            .expect("跟踪项应存在");
        let web = view
            .targets
            .iter()
            .find(|target| target.target.target_id == "t-1")
            .expect("web 目标应存在");
        assert_eq!(
            web.evidence_revision.as_deref(),
            Some("rev-1"),
            "证据引用应改用来源修订 ID"
        );

        // 另一个项目的来源：同一公司也不行，跨项目来源不得挂到本项目交付上。
        with_uow(&mut store, |uow| {
            uow.register_source(
                "src-2",
                &source_identity("project-2", "PR_kw9"),
                "acme/api#9",
                "alice",
                100,
            )
            .expect("另一个项目的来源应可登记");
            uow.append_source_revision(&source_revision("rev-2", "src-2", "v1", "sha256:abc", 100))
                .expect("修订应可写入");
        });
        let mut cross = delivery_observation(
            "o-10",
            "t-1",
            ObservationKind::Deployment,
            400,
            "rev-1",
            "sha256:abc",
            "",
        );
        cross.source_revision_id = Some("rev-2".to_string());
        let err = with_uow(&mut store, |uow| uow.append_delivery_observation(&cross))
            .expect_err("跨项目来源必须拒绝");
        assert!(
            matches!(
                err,
                StoreError::Source(
                    crate::delivery_source::SourceError::SourceProjectMismatch { .. }
                )
            ),
            "{err:?}"
        );

        // 不存在的修订：给出可读原因，而不是外键报错。
        let mut missing = delivery_observation(
            "o-11",
            "t-1",
            ObservationKind::Deployment,
            500,
            "rev-1",
            "sha256:abc",
            "",
        );
        missing.source_revision_id = Some("rev-404".to_string());
        let err = with_uow(&mut store, |uow| uow.append_delivery_observation(&missing))
            .expect_err("不存在的修订必须拒绝");
        assert!(
            matches!(
                err,
                StoreError::Source(crate::delivery_source::SourceError::SourceRevisionNotFound(
                    _
                ))
            ),
            "{err:?}"
        );
    }

    /// 验证记录取最新一条：先 `waived` 后 `verified`，结论应变成已覆盖。
    #[test]
    fn latest_verification_record_decides() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        seed_delivery(&mut store);
        let view = store
            .delivery_tracking_view("company-default", "dt-1")
            .expect("读取应成功")
            .expect("跟踪项应存在");
        assert!(!view.targets[2].verdict.is_covered(), "豁免不算通过");

        insert_verification(&store, "vr-3b", "rev-3", "verified", 200);
        let view = store
            .delivery_tracking_view("company-default", "dt-1")
            .expect("读取应成功")
            .expect("跟踪项应存在");
        assert!(
            view.targets[2].verdict.is_covered(),
            "较新的 verified 应覆盖较早的 waived：{:?}",
            view.targets[2]
        );
    }

    #[test]
    fn migration_creates_all_d1_tables() {
        let store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .expect("应能查询表目录");
        assert_eq!(
            count, 59,
            "应创建 9 张 D1 + 3 张 D2 + 4 张 D3 + 6 张 D4 + 10 张 D5 + 5 张 D6 + 12 张 P2 表 \
             + 1 张审计表 + 3 张交付跟踪表（016）+ 4 张来源表（017：身份/候选/修订/观察关联）\
             + 2 张去重表（018：命令回执/来源关系）"
        );
    }

    #[test]
    fn pragma_profile_is_verified() {
        let path = temp_db();
        let store = SqliteStore::open(&path, &config()).expect("文件库应可打开");
        let journal: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_ascii_lowercase(), "wal");
        let synchronous: i64 = store
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2, "FULL 对应 SQLite synchronous=2");
        let busy: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy, 5_000);
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(PathBuf::from(format!("{}{}", path.display(), suffix)));
        }
    }

    #[test]
    fn event_and_grant_commit_in_one_uow() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let scope = Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new("project-1")),
            issue_id: None,
        };
        let grant = Grant {
            principal_id: DomainId::new("coder-1"),
            scope: scope.clone(),
            permission: Permission::Dispatch,
            revision: 7,
            active: true,
        };
        {
            let uow = store.transaction().expect("BEGIN IMMEDIATE 应成功");
            uow.insert_event("evt-1", "company-default", "issue-1", 1, "{}")
                .unwrap();
            uow.insert_grant(&grant).unwrap();
            uow.commit().unwrap();
        }
        let events: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        let grants: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM grants", [], |row| row.get(0))
            .unwrap();
        assert_eq!((events, grants), (1, 1));
    }

    #[test]
    fn rollback_leaves_no_partial_write() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let uow = store.transaction().unwrap();
        uow.insert_issue(&issue()).unwrap();
        uow.rollback().unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM issues", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "回滚后不应留下部分写入");
    }

    #[test]
    fn concurrent_conditional_update_only_one_succeeds() {
        let path = temp_db();
        let cfg = config();
        {
            let mut store = SqliteStore::open(&path, &cfg).expect("建库应成功");
            let uow = store.transaction().unwrap();
            uow.insert_issue(&issue()).unwrap();
            uow.commit().unwrap();
        }

        // 屏障放在拿写锁之前：BEGIN IMMEDIATE 会串行化，抢锁失败也是合法的非成功结果。
        let barrier = Arc::new(Barrier::new(2));
        let results: Vec<bool> = thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let path = path.clone();
                    let cfg = cfg.clone();
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        let mut store = match SqliteStore::open(&path, &cfg) {
                            Ok(store) => store,
                            Err(err) => panic!("连接失败: {err}"),
                        };
                        let uow = match store.transaction() {
                            Ok(uow) => uow,
                            // 写锁竞争失败：不成功，但不视为测试错误。
                            Err(err) if err.to_string().contains("database is locked") => {
                                return false;
                            }
                            Err(err) => panic!("BEGIN IMMEDIATE 失败: {err}"),
                        };
                        // 领域命令会把 resourceVersion 从 1 递增到 2 后再做条件更新。
                        let mut next = issue();
                        next.meta.resource_version = Version(2);
                        match uow.update_issue_conditional(&next, Version::INITIAL) {
                            Ok(()) => uow.commit().is_ok(),
                            Err(StoreError::VersionConflict { .. }) => false,
                            Err(err) => panic!("条件更新失败: {err}"),
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("线程不应 panic"))
                .collect()
        });

        assert_eq!(
            results.iter().filter(|ok| **ok).count(),
            1,
            "只有一个条件更新成功: {results:?}"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn stale_version_update_returns_current_version() {
        let path = temp_db();
        let cfg = config();
        let committed = {
            let mut store = SqliteStore::open(&path, &cfg).expect("建库应成功");
            let uow = store.transaction().unwrap();
            uow.insert_issue(&issue()).unwrap();
            uow.commit().unwrap();
            let mut next = issue();
            next.meta.resource_version = Version(2);
            next.spec.title = "第二次写入".to_string();
            let uow = store.transaction().unwrap();
            uow.update_issue_conditional(&next, Version::INITIAL)
                .unwrap();
            uow.commit().unwrap();
            next
        };
        let mut store = SqliteStore::open(&path, &cfg).expect("第二连接应成功");
        let uow = store.transaction().unwrap();
        let err = uow
            .update_issue_conditional(&committed, Version::INITIAL)
            .expect_err("过期版本必须被拒绝");
        assert!(matches!(
            err,
            StoreError::VersionConflict {
                current: Version(2)
            }
        ));
        uow.rollback().expect("测试结束应显式回滚");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn assigned_issue_persists_owner_and_revision() {
        let mut store = SqliteStore::in_memory(&config()).unwrap();
        let scope = Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new("project-1")),
            issue_id: None,
        };
        let grants = vec![
            Grant {
                principal_id: DomainId::new("manager-1"),
                scope: scope.clone(),
                permission: Permission::ManageScope,
                revision: 1,
                active: true,
            },
            Grant {
                principal_id: DomainId::new("coder-1"),
                scope: scope.clone(),
                permission: Permission::Dispatch,
                revision: 7,
                active: true,
            },
        ];
        let assigned = assign_issue(
            &issue(),
            crate::domain::Owner {
                kind: OwnerKind::Agent,
                id: DomainId::new("coder-1"),
            },
            7,
            &Principal {
                id: DomainId::new("manager-1"),
                kind: crate::domain::PrincipalKind::User,
                claimed_roles: Vec::new(),
            },
            &grants,
        )
        .expect("直接指派应成功");
        let _ = WriterKind::DomainCommand;
        let uow = store.transaction().unwrap();
        uow.insert_issue(&assigned).unwrap();
        uow.commit().unwrap();
        let (owner, revision): (String, i64) = store
            .conn
            .query_row(
                "SELECT owner_id, assignment_grant_revision FROM issues WHERE uid = 'issue-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(owner, "coder-1");
        assert_eq!(revision, 7);
    }

    // -----------------------------------------------------------------------
    // 去重语义（P8 3.3）：幂等键管命令重放，来源身份管对象重复
    // -----------------------------------------------------------------------

    fn count_rows(store: &SqliteStore, table: &str) -> i64 {
        store
            .read_connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("应能计数")
    }

    fn import_command(key: &str, tracking_id: &str, source_id: &str) -> ImportSourceCommand {
        ImportSourceCommand {
            idempotency_key: key.to_string(),
            company_id: "company-default".to_string(),
            project_id: "project-1".to_string(),
            connection_id: "gh-1".to_string(),
            object_type: Some("pull-request".to_string()),
            provider_object_id: Some("PR_kw318".to_string()),
            locator: "acme/web#318".to_string(),
            title: "客户 A 登录失败".to_string(),
            requirement: "登录偶发 500".to_string(),
            owner_id: "alice".to_string(),
            new_tracking_id: tracking_id.to_string(),
            new_source_id: source_id.to_string(),
            new_candidate_id: format!("{source_id}-cand"),
            imported_by: "alice".to_string(),
        }
    }

    /// §11 的并发导入用例：两人各用一个键导入同一个 PR，
    /// 只产生**一个跟踪项与一条来源关系**；后到者拿到已有关系。
    ///
    /// 两个键各自留一条回执——它们确实是两条命令，去重发生在来源身份上，不在键上。
    #[test]
    fn two_keys_importing_one_source_yield_one_tracking_item() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let first = with_uow(&mut store, |uow| {
            uow.import_delivery_source(&import_command("k-alice", "dt-1", "src-1"), 1_000)
        })
        .expect("首次导入应成功");
        assert!(first.created_tracking && first.created_source && !first.replayed);

        let second = with_uow(&mut store, |uow| {
            uow.import_delivery_source(&import_command("k-bob", "dt-2", "src-2"), 1_100)
        })
        .expect("重复导入应成功而不是报错");

        assert_eq!(
            second.tracking_id.as_deref(),
            Some("dt-1"),
            "后到者必须拿到已有跟踪项，不能建第二个"
        );
        assert_eq!(
            second.source_id.as_deref(),
            Some("src-1"),
            "五元组命中同一来源身份"
        );
        assert!(!second.created_tracking && !second.created_source);
        assert!(!second.replayed, "换键不是重放：这是另一条命令");

        assert_eq!(count_rows(&store, "delivery_tracking"), 1);
        assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
        assert_eq!(count_rows(&store, "delivery_sources"), 1);
        assert_eq!(count_rows(&store, "delivery_command_receipts"), 2);
    }

    /// 同键同载荷：重放首次结果，**不写第二遍**。
    #[test]
    fn same_key_and_payload_replays_without_writing() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let command = import_command("k-1", "dt-1", "src-1");
        let first = with_uow(&mut store, |uow| {
            uow.import_delivery_source(&command, 1_000)
        })
        .expect("首次导入应成功");
        let snapshot: Vec<i64> = [
            "delivery_tracking",
            "delivery_sources",
            "delivery_tracking_sources",
            "delivery_command_receipts",
        ]
        .iter()
        .map(|table| count_rows(&store, table))
        .collect();

        let again = with_uow(&mut store, |uow| {
            uow.import_delivery_source(&command, 1_500)
        })
        .expect("重放应成功");
        assert!(again.replayed, "同键同载荷必须是重放");
        assert_eq!(again.tracking_id, first.tracking_id);
        // created_* 描述首次执行的事实，不因重放而改写。
        assert!(again.created_tracking && again.created_source);

        let after: Vec<i64> = [
            "delivery_tracking",
            "delivery_sources",
            "delivery_tracking_sources",
            "delivery_command_receipts",
        ]
        .iter()
        .map(|table| count_rows(&store, table))
        .collect();
        assert_eq!(snapshot, after, "重放不得写入任何一行");
    }

    /// 同键异载荷：冲突，且**什么都不写**（不是「覆盖上一次」）。
    #[test]
    fn same_key_with_other_payload_conflicts_and_writes_nothing() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let first = import_command("k-1", "dt-1", "src-1");
        with_uow(&mut store, |uow| uow.import_delivery_source(&first, 1_000))
            .expect("首次导入应成功");

        let mut retitled = first.clone();
        retitled.title = "改过的标题".to_string();
        let err = with_uow(&mut store, |uow| {
            uow.import_delivery_source(&retitled, 1_100)
        })
        .expect_err("同键异载荷必须冲突");
        assert!(
            matches!(
                err,
                StoreError::Idempotency(IdempotencyError::KeyReusedWithOtherPayload { .. })
            ),
            "{err:?}"
        );
        assert_eq!(count_rows(&store, "delivery_tracking"), 1);
        assert_eq!(count_rows(&store, "delivery_command_receipts"), 1);
    }

    /// 失败的导入不污染键：修正载荷后用**同一个键**重试应当是首次执行。
    ///
    /// 这是「回执与业务写入同事务」的直接后果。若先写回执再干活，
    /// 这里的重试会撞上自己伪造的冲突，然后人就再也不敢用那个键了。
    #[test]
    fn failed_import_does_not_poison_the_key() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let mut broken = import_command("k-1", "dt-1", "src-1");
        broken.object_type = None; // 有稳定 ID 却没类型：调用方漏字段
        let err = with_uow(&mut store, |uow| uow.import_delivery_source(&broken, 1_000))
            .expect_err("缺对象类型必须报错");
        assert!(
            matches!(err, StoreError::Source(SourceError::MissingObjectType)),
            "{err:?}"
        );
        assert_eq!(
            count_rows(&store, "delivery_command_receipts"),
            0,
            "失败的导入不得留下回执"
        );
        assert_eq!(count_rows(&store, "delivery_sources"), 0);

        let fixed = import_command("k-1", "dt-1", "src-1");
        let outcome = with_uow(&mut store, |uow| uow.import_delivery_source(&fixed, 1_100))
            .expect("修正后同键重试应成功");
        assert!(!outcome.replayed, "此前没有成功执行过，这不是重放");
        assert!(outcome.created_tracking && outcome.created_source);
    }

    /// 只有定位：进候选，**不建跟踪项**。候选没有稳定身份，撑不起覆盖结论。
    #[test]
    fn locator_only_import_creates_a_candidate_not_a_tracking_item() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let mut command = import_command("k-1", "dt-1", "src-1");
        command.provider_object_id = None;
        command.locator = "https://github.com/acme/web/pull/318".to_string();

        let outcome = with_uow(&mut store, |uow| {
            uow.import_delivery_source(&command, 1_000)
        })
        .expect("只有定位应进候选而不是报错");
        assert_eq!(outcome.candidate_id.as_deref(), Some("src-1-cand"));
        assert!(outcome.created_candidate);
        assert!(!outcome.created_tracking);
        assert!(outcome.tracking_id.is_none());
        assert_eq!(count_rows(&store, "delivery_tracking"), 0);
        assert_eq!(count_rows(&store, "delivery_source_candidates"), 1);

        let receipt = store
            .find_command_receipt("company-default", "k-1")
            .expect("应能读回执")
            .expect("回执应存在");
        assert_eq!(receipt.result_kind, ResultKind::Candidate);
        assert_eq!(receipt.result_id, "src-1-cand");
    }

    /// 导入到已存在的跟踪项：不建新项，只挂关系。
    #[test]
    fn import_can_attach_to_an_existing_tracking_item() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))
        })
        .expect("跟踪项应可写入");

        let outcome = with_uow(&mut store, |uow| {
            uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
        })
        .expect("导入到已有跟踪项应成功");
        assert_eq!(outcome.tracking_id.as_deref(), Some("dt-1"));
        assert!(!outcome.created_tracking, "跟踪项本来就在，不算本次新建");
        assert!(outcome.created_source);
        assert_eq!(count_rows(&store, "delivery_tracking"), 1);
        assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
    }

    /// 一个来源身份只承载一条跟踪关系：换一条跟踪项挂要明确拒绝，重复挂同一条则幂等。
    #[test]
    fn a_source_cannot_be_linked_to_a_second_tracking_item() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))?;
            uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-2"))?;
            uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
                .map(|_| ())
        })
        .expect("首次导入应成功");

        let err = with_uow(&mut store, |uow| {
            uow.link_delivery_tracking_source("company-default", "src-1", "dt-2", "bob", 1_100)
        })
        .expect_err("改挂必须被拒绝");
        assert!(
            matches!(
                err,
                StoreError::Source(SourceError::SourceAlreadyLinked { .. })
            ),
            "{err:?}"
        );

        // 重复挂同一条：幂等，不新增行也不报错。
        let again = with_uow(&mut store, |uow| {
            uow.link_delivery_tracking_source("company-default", "src-1", "dt-1", "bob", 1_200)
        })
        .expect("重复挂同一条应幂等");
        assert!(!again.created);
        assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
    }

    /// 跨项目的来源不得挂到本项目的交付上（复合外键读不出「项目不同」）。
    #[test]
    fn cross_project_link_is_refused() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        let mut other_project = delivery_tracking("company-default", "dt-2");
        other_project.project_id = "project-2".to_string();
        with_uow(&mut store, |uow| {
            uow.insert_delivery_tracking(&other_project)?;
            uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
                .map(|_| ())
        })
        .expect("首次导入应成功");

        let err = with_uow(&mut store, |uow| {
            uow.link_delivery_tracking_source("company-default", "src-1", "dt-2", "bob", 1_100)
        })
        .expect_err("跨项目挂载必须被拒绝");
        assert!(
            matches!(
                err,
                StoreError::Source(SourceError::SourceProjectMismatch { .. })
            ),
            "{err:?}"
        );
    }

    /// 回执不可改写：重放必须返回**当时**的结果，改写等于伪造历史。
    #[test]
    fn command_receipts_are_append_only() {
        let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
        with_uow(&mut store, |uow| {
            uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
                .map(|_| ())
        })
        .expect("首次导入应成功");

        let update = store.execute_batch_for_test(
            "UPDATE delivery_command_receipts SET result_id = 'dt-9' WHERE idempotency_key = 'k-1'",
        );
        assert!(update.is_err(), "改写回执必须被拒绝");
        let delete = store.execute_batch_for_test(
            "DELETE FROM delivery_command_receipts WHERE idempotency_key = 'k-1'",
        );
        assert!(delete.is_err(), "删除回执必须被拒绝");
    }
}
