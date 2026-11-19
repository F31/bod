//! SQLite 持久化与 UnitOfWork（P0 D1 任务 2.6）。
//!
//! 边界：`Connection` 由结构体独占，不跨线程共享可变引用；异步层后续用有界写请求队列
//! 把命令送进单一写线程（§37.5），此处先建立事务与条件更新的正确语义。
//! 不在事务内执行网络、Agent 或人工等待。

use crate::config::BodConfig;
use crate::domain::model::{AcceptanceCriteria, Grant, Issue, IssueKind, OwnerKind, Permission};
use crate::domain::queue::{EffectStatus, OutboxDelivery, QueueState, WakeReceipt, WorkQueueEntry};
use crate::domain::{DenialReason, DomainId, Version};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::fs;
use std::path::Path;

/// 有序迁移列表。001 为 D1 领域表，002 为 D2 队列/outbox；新增迁移只能追加，不可原地改写。
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_initial.sql"),
    include_str!("../migrations/002_queue.sql"),
];

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Pragma(&'static str),
    VersionConflict { current: Version },
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

    pub fn transaction(&mut self) -> Result<UnitOfWork<'_>, StoreError> {
        UnitOfWork::begin(&mut self.conn)
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
        assert_eq!(count, 12, "应创建 9 张 D1 表 + 3 张 D2 队列表");
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
}
