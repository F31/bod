//! 存储层用例：迁移、事务、条件更新、交付域的全部不变量。
//!
//! 这些用例直接打真实 SQLite（内存或临时文件），不放 mock：
//! 要验证的正是「约束在库里生效」，替身会让结论失去意义。

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
    assert_eq!(rows[1].event.action, AuditAction::Submit);
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
        action: AuditAction::Submit,
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
    let delete = store
        .execute_batch_for_test("DELETE FROM delivery_observations WHERE observation_id = 'o-1'");
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
            StoreError::Source(crate::delivery_source::SourceError::CandidateScopeMismatch { .. })
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
            StoreError::Source(crate::delivery_source::SourceError::SourceProjectMismatch { .. })
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
        count, 60,
        "应创建 9 张 D1 + 3 张 D2 + 4 张 D3 + 6 张 D4 + 10 张 D5 + 5 张 D6 + 12 张 P2 表 \
             + 1 张审计表 + 3 张交付跟踪表（016）+ 4 张来源表（017：身份/候选/修订/观察关联）\
             + 2 张去重表（018：命令回执/来源关系）\
             + 1 张关联表（019：内部工作包关联）"
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
        correlation_id: "req-import".to_string(),
        decided_at: "2026-11-22T00:00:00Z".to_string(),
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
    with_uow(&mut store, |uow| uow.import_delivery_source(&first, 1_000)).expect("首次导入应成功");

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

// -----------------------------------------------------------------------
// 关联追加与期望版本（P8 3.4）
// -----------------------------------------------------------------------

use crate::delivery_api::TargetRef;
use crate::delivery_link::{COMMAND_APPEND_LINK, LinkError, LinkRef};

fn link_target(customer: &str, unit: Option<&str>) -> TargetRef {
    TargetRef {
        customer_id: customer.to_string(),
        environment: "prod".to_string(),
        deployment_unit: unit.map(str::to_string),
    }
}

fn link_command(
    key: &str,
    link: LinkRef,
    targets: Vec<TargetRef>,
    expected_version: u64,
) -> AppendLinkCommand {
    AppendLinkCommand {
        idempotency_key: key.to_string(),
        company_id: "company-default".to_string(),
        tracking_id: "dt-1".to_string(),
        link,
        targets,
        expected_version,
        linked_by: "alice".to_string(),
        correlation_id: "req-link".to_string(),
        decided_at: "2026-11-22T00:00:00Z".to_string(),
    }
}

fn register_pull_request(store: &mut SqliteStore, source_id: &str) {
    with_uow(store, |uow| {
        uow.register_source(
            source_id,
            &SourceIdentity {
                company_id: "company-default".to_string(),
                project_id: "project-1".to_string(),
                connection_id: "gh-1".to_string(),
                object_type: "pull-request".to_string(),
                provider_object_id: "PR_kw318".to_string(),
            },
            "acme/web#318",
            "alice",
            900,
        )
        .map(|_| ())
    })
    .expect("来源身份应可登记");
}

fn insert_open_tracking(store: &mut SqliteStore) {
    with_uow(store, |uow| {
        uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))
    })
    .expect("跟踪项应可写入");
}

/// 一个来源、两个客户：共享来源不合并客户记录，A 的部署也不能成为 B 的证据。
///
/// 模型上的答案是**一条跟踪项 + 两个目标**，不是两条跟踪项（018 的主键决定：
/// 一个来源身份只承载一条跟踪关系）。所以「不合并」必须落在目标这一层。
#[test]
fn shared_source_keeps_each_customer_record_separate() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    register_pull_request(&mut store, "src-1");
    insert_open_tracking(&mut store);

    let outcome = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", Some("web")), link_target("beta", None)],
                1,
            ),
            1_000,
        )
    })
    .expect("追加应成功");
    assert!(outcome.appended);
    assert_eq!(outcome.version, 2);
    assert_eq!(outcome.created_targets.len(), 2);
    assert_eq!(count_rows(&store, "delivery_tracking"), 1);
    assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
    assert_eq!(count_rows(&store, "delivery_targets"), 2);

    let view = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("应可读取")
        .expect("应存在");
    let acme_target = view
        .targets
        .iter()
        .find(|target| target.target.customer_id == "acme")
        .expect("应有 acme 目标")
        .target
        .target_id
        .clone();

    // 只给 acme 记一次部署。
    with_uow(&mut store, |uow| {
        uow.append_delivery_observation(&delivery_observation(
            "o-1",
            &acme_target,
            ObservationKind::Deployment,
            1_100,
            "rev-1",
            "sha256:abc",
            "部署到客户 A",
        ))
    })
    .expect("部署观察应可写入");

    let view = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("应可读取")
        .expect("应存在");
    let acme = view
        .targets
        .iter()
        .find(|target| target.target.customer_id == "acme")
        .expect("应有 acme 目标");
    let beta = view
        .targets
        .iter()
        .find(|target| target.target.customer_id == "beta")
        .expect("应有 beta 目标");
    assert_eq!(acme.evidence_revision.as_deref(), Some("o-1"));
    assert!(
        beta.evidence_revision.is_none(),
        "客户 A 的部署不得出现在客户 B 的证据里"
    );
    // 制品摘要没有流到 B：它的覆盖元组仍然不完整，A 的则已经完整。
    assert!(
        !acme
            .gaps
            .contains(&crate::delivery_tracking::GAP_NO_ARTIFACT_MAPPING)
    );
    assert!(
        beta.gaps
            .contains(&crate::delivery_tracking::GAP_NO_ARTIFACT_MAPPING)
    );
    assert_eq!(acme.target.deployment_unit, "web");
    assert!(beta.target.deployment_unit.is_empty());
}

/// 目标给两遍是一条指令：只建一行，版本只推进一次。
#[test]
fn duplicate_targets_in_one_command_create_one_row() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    register_pull_request(&mut store, "src-1");
    insert_open_tracking(&mut store);

    let outcome = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None), link_target("acme", None)],
                1,
            ),
            1_000,
        )
    })
    .expect("追加应成功");
    assert_eq!(outcome.created_targets.len(), 1);
    assert_eq!(outcome.version, 2);
    assert_eq!(count_rows(&store, "delivery_targets"), 1);
}

/// 期望版本不一致：**什么都不写**。不做事后合并，也不留回执。
#[test]
fn stale_expected_version_writes_nothing() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    insert_open_tracking(&mut store);

    let err = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None)],
                5,
            ),
            1_000,
        )
    })
    .expect_err("期望版本不一致必须拒绝");
    match err {
        StoreError::Link(LinkError::VersionConflict { expected, current }) => {
            assert_eq!(expected, 5);
            assert_eq!(current, 1, "冲突里要带当前版本，客户端不必再读一次");
        }
        other => panic!("应为版本冲突：{other:?}"),
    }
    assert_eq!(count_rows(&store, "delivery_targets"), 0);
    assert_eq!(count_rows(&store, "delivery_tracking_sources"), 0);
    assert_eq!(
        count_rows(&store, "delivery_command_receipts"),
        0,
        "被拒绝的命令不得留下回执"
    );
    let view = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("应可读取")
        .expect("应存在");
    assert_eq!(view.tracking.resource_version, 1);
}

/// 换键重复同一载荷：什么都不追加，版本也不动。
///
/// 版本若照样 +1，两个客户端会互相把对方的期望版本判过期，重试变成必冲突。
#[test]
fn repeating_with_another_key_appends_nothing_and_keeps_the_version() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    register_pull_request(&mut store, "src-1");
    insert_open_tracking(&mut store);

    let command = |key: &str, expected: u64| {
        link_command(
            key,
            LinkRef::Source("src-1".to_string()),
            vec![link_target("acme", None)],
            expected,
        )
    };
    let first = with_uow(&mut store, |uow| {
        uow.append_delivery_link(&command("k-1", 1), 1_000)
    })
    .expect("首次追加应成功");
    assert!(first.appended);

    let again = with_uow(&mut store, |uow| {
        uow.append_delivery_link(&command("k-2", 2), 1_100)
    })
    .expect("换键重复应成功");
    assert!(!again.appended, "关系与目标都在，本次没有新增");
    assert_eq!(again.version, 2, "纯重复不得推进版本");
    assert!(again.created_targets.is_empty());
    assert!(!again.replayed, "换键不是重放：这是另一条命令");
    assert_eq!(count_rows(&store, "delivery_targets"), 1);
    assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
    assert_eq!(
        count_rows(&store, "delivery_command_receipts"),
        2,
        "两条命令各自留回执"
    );
}

/// 重放返回**首次**结果（含当时的版本），不按今天的状态重算。
///
/// 客户端若拿重放给的旧版本去追加，会得到 409 而不是静默覆盖——安全的一侧。
#[test]
fn replay_returns_the_first_result_not_todays_version() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    register_pull_request(&mut store, "src-1");
    insert_open_tracking(&mut store);

    let first = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None)],
                1,
            ),
            1_000,
        )
    })
    .expect("首次追加应成功");
    assert_eq!(first.version, 2);

    let second = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-2",
                LinkRef::WorkPackage("wp-17".to_string()),
                vec![link_target("beta", None)],
                2,
            ),
            1_100,
        )
    })
    .expect("第二次追加应成功");
    assert_eq!(second.version, 3);

    let replay = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None)],
                1,
            ),
            1_200,
        )
    })
    .expect("重放应成功");
    assert!(replay.replayed);
    assert!(replay.appended, "首次执行确实追加了：这是当时的回答");
    assert_eq!(
        replay.version, 2,
        "重放给的是首次执行后的版本，不是今天的 3"
    );
    assert_eq!(replay.link_id, first.link_id);
    assert_eq!(count_rows(&store, "delivery_command_receipts"), 2);

    // 拿着重放给的旧版本继续追加：409，而不是把别人的写入覆盖掉。
    let err = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-3",
                LinkRef::WorkPackage("wp-18".to_string()),
                Vec::new(),
                2,
            ),
            1_300,
        )
    })
    .expect_err("旧版本必须冲突");
    assert!(matches!(
        err,
        StoreError::Link(LinkError::VersionConflict {
            expected: 2,
            current: 3
        })
    ));
}

/// 后来补入的内部工作包：追加关联，不动外部来源，也不伪造内部运行（D3）。
#[test]
fn a_later_work_package_is_appended_without_touching_the_external_source() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    register_pull_request(&mut store, "src-1");
    insert_open_tracking(&mut store);

    let linked = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None)],
                1,
            ),
            1_000,
        )
    })
    .expect("来源关联应成功");
    assert_eq!(linked.version, 2);

    let appended = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-2",
                LinkRef::WorkPackage("wp-17".to_string()),
                Vec::new(),
                2,
            ),
            1_100,
        )
    })
    .expect("工作包关联应成功");
    assert!(appended.appended);
    assert_eq!(appended.version, 3);

    assert_eq!(
        store
            .find_tracking_for_source("company-default", "src-1")
            .expect("应可读取")
            .as_deref(),
        Some("dt-1"),
        "外部来源关系必须原样保留"
    );
    let links = store
        .list_work_package_links("company-default", "dt-1")
        .expect("应可读取");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].work_package_revision, "wp-17");
    assert_eq!(links[0].linked_by, "alice");

    // 重复追加同一修订：幂等，版本不动。
    let again = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-3",
                LinkRef::WorkPackage("wp-17".to_string()),
                Vec::new(),
                3,
            ),
            1_200,
        )
    })
    .expect("重复关联应成功");
    assert!(!again.appended);
    assert_eq!(again.version, 3);
    assert_eq!(count_rows(&store, "delivery_tracking_work_packages"), 1);

    // 补工作包不等于伪造运行：三个内部引用列仍然是空。
    let view = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("应可读取")
        .expect("应存在");
    assert!(
        !view.tracking.has_internal_anchor(),
        "工作包关联不得写成 run_uid/attempt_id"
    );
    assert_eq!(view.work_packages.len(), 1);
}

/// 已关闭的跟踪项不再接受关联。与视图同口径：关闭时 `allowed_actions` 本来就不给链接。
#[test]
fn closed_tracking_takes_no_new_links() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut row = delivery_tracking("company-default", "dt-1");
    row.state = TrackingState::Closed;
    with_uow(&mut store, |uow| uow.insert_delivery_tracking(&row)).expect("跟踪项应可写入");

    let err = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::WorkPackage("wp-17".to_string()),
                Vec::new(),
                1,
            ),
            1_000,
        )
    })
    .expect_err("关闭后追加必须拒绝");
    assert!(matches!(
        err,
        StoreError::Link(LinkError::TrackingClosed(_))
    ));
    assert_eq!(count_rows(&store, "delivery_tracking_work_packages"), 0);
    assert_eq!(count_rows(&store, "delivery_command_receipts"), 0);
}

/// 导入用过的键不能拿来追加关联：一个键只属于一条命令。
#[test]
fn a_key_used_by_import_cannot_be_reused_for_a_link() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
    })
    .expect("导入应成功");

    let err = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::WorkPackage("wp-17".to_string()),
                Vec::new(),
                1,
            ),
            1_100,
        )
    })
    .expect_err("换命令必须冲突");
    assert!(
        matches!(
            err,
            StoreError::Idempotency(IdempotencyError::KeyReusedForOtherCommand { .. })
        ),
        "{err:?}"
    );
    assert_eq!(count_rows(&store, "delivery_tracking_work_packages"), 0);
}

/// 回执要能回答「这条命令做了什么、落到哪条跟踪项」。
#[test]
fn link_receipt_records_the_command_and_its_result() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    register_pull_request(&mut store, "src-1");
    insert_open_tracking(&mut store);
    with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None)],
                1,
            ),
            1_000,
        )
    })
    .expect("追加应成功");

    let receipt = store
        .find_command_receipt("company-default", "k-1")
        .expect("应可读取")
        .expect("回执应存在");
    assert_eq!(receipt.command, COMMAND_APPEND_LINK);
    assert_eq!(receipt.result_kind, ResultKind::Tracking);
    assert_eq!(receipt.result_id, "dt-1");
    let outcome = LinkOutcome::replayed_from(&receipt.result_detail).expect("应可解析");
    assert!(outcome.link_id.starts_with("dl-"), "{}", outcome.link_id);
    assert_eq!(outcome.version, 2);
}

// ---------------------------------------------------------------------------
// 审计接线（P8 3.5）：导入与追加关联各留一条事实
// ---------------------------------------------------------------------------

/// 导入的审计与业务写入同事务：读得到的审计必然对应一次真的发生过的写入。
#[test]
fn import_writes_audit_in_the_same_transaction() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let outcome = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&import_command("k-audit", "dt-9", "src-9"), 1_000)
    })
    .expect("导入应成功");
    assert_eq!(outcome.tracking_id.as_deref(), Some("dt-9"));

    let rows = store
        .list_audit_events("company-default", "req-import")
        .expect("应可读回");
    assert_eq!(rows.len(), 1, "一次导入只留一条审计");
    let event = &rows[0].event;
    assert_eq!(event.action, AuditAction::ImportSource);
    assert_eq!(event.decision, AuditDecision::Allowed);
    assert_eq!(event.basis, "import-source:source_identity");
    assert_eq!(event.actor.as_str(), "alice");
    assert_eq!(
        event.scope.project_id.as_ref().map(DomainId::as_str),
        Some("project-1"),
        "审计范围要能定位到项目"
    );
    assert_eq!(
        event.decided_at, "2026-11-22T00:00:00Z",
        "业务时间取命令给的时刻，不从现在时刻反推"
    );
    assert_eq!(
        rows[0].recorded_at_ms, 1_000,
        "落库时间才是 now_ms——两者分开才有可恢复原值"
    );
}

/// 候选导入的审计依据是「凭什么」（只有定位），不是「产生了什么」。
#[test]
fn candidate_import_audits_the_candidate_basis() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut command = import_command("k-cand", "dt-c", "src-c");
    command.object_type = None;
    command.provider_object_id = None;
    command.locator = "https://acme.example/web/pull/318".to_string();
    with_uow(&mut store, |uow| {
        uow.import_delivery_source(&command, 1_000)
    })
    .expect("只有定位应进候选");

    let rows = store
        .list_audit_events("company-default", "req-import")
        .expect("应可读回");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.action, AuditAction::ImportSource);
    assert_eq!(rows[0].event.basis, "import-source:source_candidate");
}

/// 只有真有新增才记审计：重复提交同一关系是幂等空操作，不该在审计里多出一次改动。
#[test]
fn append_link_audits_only_when_something_is_added() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    register_pull_request(&mut store, "src-1");
    insert_open_tracking(&mut store);

    let first = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-link-1",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None)],
                1,
            ),
            1_000,
        )
    })
    .expect("首次追加应成功");
    assert!(first.appended);

    // 换一个键、同一关系、同一目标：幂等去重发生在关系层，本次什么都不新增。
    let second = with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-link-2",
                LinkRef::Source("src-1".to_string()),
                vec![link_target("acme", None)],
                first.version,
            ),
            1_000,
        )
    })
    .expect("重复追加应成功");
    assert!(!second.appended, "关系与目标都已存在，本次不新增");
    assert_eq!(second.version, first.version, "空操作不推进版本");

    // 两条命令同属一条链（同一 correlation_id）：链上只该有一条「追加」事实。
    let rows = store
        .list_audit_events("company-default", "req-link")
        .expect("应可读回");
    assert_eq!(rows.len(), 1, "空操作不得留下第二条『追加过』的审计");
    assert_eq!(rows[0].event.action, AuditAction::AppendLink);
    assert_eq!(rows[0].event.basis, "append-link:source_link");
}

/// 工作包关联的审计依据与来源关联分开：两者都是 `append-link`，但凭的不是同一样东西。
#[test]
fn work_package_link_audits_its_own_basis() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    insert_open_tracking(&mut store);
    with_uow(&mut store, |uow| {
        uow.append_delivery_link(
            &link_command(
                "k-wp",
                LinkRef::WorkPackage("wp-1".to_string()),
                vec![link_target("acme", None)],
                1,
            ),
            1_000,
        )
    })
    .expect("工作包关联应成功");

    let rows = store
        .list_audit_events("company-default", "req-link")
        .expect("应可读回");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.basis, "append-link:work_package_link");
}

/// 审计写不进去，整条导入回滚：不允许出现「业务成了、审计没有」的写入。
///
/// 构造方式是让业务字段全部合法、只有审计过不了闸门（`imported_by` 含凭据形态）。
/// 若审计被放在事务外，这个用例会看到一条落库的跟踪项——那正是要害。
#[test]
fn import_rolls_back_when_the_audit_row_is_refused() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut command = import_command("k-secret", "dt-x", "src-x");
    command.imported_by = "Authorization: Bearer abc123".to_string();

    let err = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&command, 1_000)
    })
    .expect_err("审计拒收时导入必须失败");
    assert!(
        matches!(
            err,
            StoreError::Audit(crate::audit_log::AuditRejection::SecretInText("actor"))
        ),
        "{err:?}"
    );

    assert_eq!(
        count_rows(&store, "delivery_tracking"),
        0,
        "跟踪项必须一并回滚"
    );
    assert_eq!(
        count_rows(&store, "delivery_sources"),
        0,
        "来源身份必须一并回滚"
    );
    assert_eq!(
        count_rows(&store, "delivery_command_receipts"),
        0,
        "回执不得留下——否则重放会返回一个从未发生的成功"
    );
    assert_eq!(
        count_rows(&store, "audit_events"),
        0,
        "审计本身也不得留半条"
    );
}
