//! 审计写入与读回：不可改写、按决策计数、表外动作报错。

use super::*;

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
