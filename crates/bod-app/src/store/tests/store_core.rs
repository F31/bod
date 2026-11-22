//! 迁移、pragma、事务边界、条件更新与版本冲突。

use super::*;

use crate::domain::model::{Phase, Principal, ResourceMeta, ResourceStatus, WriterKind};
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
    assert_eq!(
        count, 64,
        "应创建 9 张 D1 + 3 张 D2 + 4 张 D3 + 6 张 D4 + 10 张 D5 + 5 张 D6 + 12 张 P2 表 \
             + 1 张审计表 + 3 张交付跟踪表（016）+ 4 张来源表（017：身份/候选/修订/观察关联）\
             + 2 张去重表（018：命令回执/来源关系）\
             + 1 张关联表（019：内部工作包关联）\
             + 4 张授权事实表（020：项目成员/客户绑定/发布操作人/撤权修订）"
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
