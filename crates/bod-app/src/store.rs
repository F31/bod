//! SQLite 持久化与 UnitOfWork（P0 D1 任务 2.6）。
//!
//! 边界：`Connection` 由结构体独占，不跨线程共享可变引用；异步层后续用有界写请求队列
//! 把命令送进单一写线程（§37.5），此处先建立事务与条件更新的正确语义。
//! 不在事务内执行网络、Agent 或人工等待。

use crate::config::BodConfig;
use crate::domain::model::{AcceptanceCriteria, Grant, Issue, IssueKind, OwnerKind, Permission};
use crate::domain::{DomainId, Version};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::fs;
use std::path::Path;

const INITIAL_MIGRATION: &str = include_str!("../migrations/001_initial.sql");

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
        conn.execute_batch(INITIAL_MIGRATION)?;
        Ok(Self { conn })
    }

    pub fn in_memory(config: &BodConfig) -> Result<Self, StoreError> {
        Self::open(Path::new(":memory:"), config)
    }

    pub fn transaction(&mut self) -> Result<UnitOfWork<'_>, StoreError> {
        UnitOfWork::begin(&mut self.conn)
    }
}

/// 一个业务命令只开一个 Company UnitOfWork；BEGIN IMMEDIATE 串行化写入。
pub struct UnitOfWork<'a> {
    conn: &'a mut Connection,
}

impl<'a> UnitOfWork<'a> {
    pub fn begin(conn: &'a mut Connection) -> Result<Self, StoreError> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Self { conn })
    }

    pub fn commit(self) -> Result<(), StoreError> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(self) -> Result<(), StoreError> {
        self.conn.execute_batch("ROLLBACK")?;
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
        assert_eq!(count, 9, "应创建 9 张 D1 表");
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
