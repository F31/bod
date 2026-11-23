//! 观察登记（P8 7.5.3.3）：事实只追加、幂等按命令、版本不动、不建目标。
//!
//! 这组用例钉的是**入口的语义**，不是 3.1 的写入原语（后者在 `tests::delivery`）。
//! 写入口与原语之间差着四件事，每一件都能悄悄把「补证」变成「改结论」或者反过来：
//! 幂等键管住重复命令，业务时间管住乱序，`authorized` 管住「谁说了算」，
//! 而「不建目标」管住笔误不要变成承诺。

use super::*;

use crate::delivery_api::TargetRef;
use crate::delivery_observation::{ObservationError, RecordObservationCommand, parse_observed_at};

fn observe_target(customer: &str, environment: &str, unit: Option<&str>) -> TargetRef {
    TargetRef {
        customer_id: customer.to_string(),
        environment: environment.to_string(),
        deployment_unit: unit.map(str::to_string),
    }
}

fn observe_command(key: &str, target: TargetRef, at: &str) -> RecordObservationCommand {
    let (observed_at, observed_at_ms) = parse_observed_at(at).expect("测试时间应可解析");
    RecordObservationCommand {
        idempotency_key: key.to_string(),
        company_id: "company-default".to_string(),
        tracking_id: "dt-1".to_string(),
        target,
        kind: ObservationKind::Deployment,
        // 主路径用人工观察：连接器观察必须先登记来源修订，那条路径单独一条用例覆盖。
        source_kind: "manual",
        source_ref: "acme/web#318".to_string(),
        source_revision_id: None,
        artifact_revision: Some("artifact-1".to_string()),
        digest: "sha256:abc".to_string(),
        revision: "deploy-1".to_string(),
        note: "现场核对".to_string(),
        actor_id: "alice".to_string(),
        observed_at,
        observed_at_ms,
        correlation_id: "req-observe".to_string(),
        decided_at: "2026-11-21T10:01:00Z".to_string(),
    }
}

fn seed_tracking_with_target(store: &mut SqliteStore) {
    with_uow(store, |uow| {
        uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))
            .expect("跟踪项应可写入");
        uow.insert_delivery_target(&delivery_target("dt-1", "t-1", "acme", "web"))
            .expect("目标应可写入");
    });
}

fn observation_count(store: &SqliteStore) -> i64 {
    store
        .read_connection()
        .query_row("SELECT COUNT(*) FROM delivery_observations", [], |row| {
            row.get(0)
        })
        .expect("应能计数")
}

fn target_count(store: &SqliteStore) -> i64 {
    store
        .read_connection()
        .query_row("SELECT COUNT(*) FROM delivery_targets", [], |row| {
            row.get(0)
        })
        .expect("应能计数")
}

fn tracking_version(store: &SqliteStore) -> i64 {
    store
        .read_connection()
        .query_row(
            "SELECT resource_version FROM delivery_tracking WHERE tracking_id = 'dt-1'",
            [],
            |row| row.get(0),
        )
        .expect("应能读版本")
}

/// 追加事实不推进版本：没有可被覆盖的读-改-写窗口，就不能假装有一个。
///
/// 若这里像 `link` 一样 +1，`link` 的期望版本会被一条与之无关的部署观察弄过期，
/// 而重试一条 `link` 命令会得到 409——那种「必须有版本」的扩张正是本条要挡的。
#[test]
fn recording_an_observation_does_not_bump_the_version() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    let outcome = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(
            &observe_command(
                "k-obs-1",
                observe_target("acme", "prod", Some("web")),
                "2026-11-21T10:00:00Z",
            ),
            2_000,
        )
    })
    .expect("观察应可登记");

    assert!(outcome.recorded, "首次登记必须真写入");
    assert_eq!(outcome.version, 1, "版本不动");
    assert_eq!(outcome.target_id, "t-1", "按身份命中 seed 里的手工目标 ID");
    assert!(!outcome.authorized, "写入口不产生授权");
    assert!(outcome.observation_id.starts_with("obs-"));
    assert_eq!(observation_count(&store), 1);
    assert_eq!(tracking_version(&store), 1, "库里版本也不动");
}

/// 两个写入者各记一条：都留下，都不需要期望版本。
///
/// 这正是「不推进版本」的另一半证明：如果版本参与，第二条会以「照着旧状态做决定」为由
/// 被拒——但两件事实之间没有任何决定关系。
#[test]
fn two_writers_record_without_conflict() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    for (key, at) in [
        ("k-obs-a", "2026-11-21T10:00:00Z"),
        ("k-obs-b", "2026-11-21T11:00:00Z"),
    ] {
        let outcome = with_uow(&mut store, |uow| {
            uow.record_delivery_observation(
                &observe_command(key, observe_target("acme", "prod", Some("web")), at),
                2_000,
            )
        })
        .unwrap_or_else(|err| panic!("{key} 应可登记：{err}"));
        assert!(outcome.recorded);
        assert_eq!(outcome.version, 1);
    }

    assert_eq!(observation_count(&store), 2, "两条事实两条记录");
    let rows = store
        .list_audit_events("company-default", "req-observe")
        .expect("应可读回");
    assert_eq!(rows.len(), 2, "两次真写入两条审计");
    assert!(
        rows.iter()
            .all(|row| row.event.action == AuditAction::RecordObservation),
        "审计动作要是 record-observation"
    );
    assert_eq!(rows[0].event.basis, "record-observation:deployment");
}

/// 同一事实重复上报（换键不换内容）：一行、一条审计、不报错。
///
/// 连接器会轮询，同一个部署被上报多次是常态。报错会让轮询变成噪声，
/// 而重复写会让时间线被同一件事刷满——两者都会让「最近一条带摘要的观察」失真。
#[test]
fn the_same_fact_reported_twice_is_one_row() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    let first = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(
            &observe_command(
                "k-obs-1",
                observe_target("acme", "prod", Some("web")),
                "2026-11-21T10:00:00Z",
            ),
            2_000,
        )
    })
    .expect("首次应成功");
    assert!(first.recorded);

    let second = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(
            &observe_command(
                "k-obs-2",
                observe_target("acme", "prod", Some("web")),
                "2026-11-21T10:00:00Z",
            ),
            3_000,
        )
    })
    .expect("重复上报不该报错");
    assert!(!second.recorded, "同一 ID 已存在，本次没有写入");
    assert!(!second.replayed, "换键不是重放");
    assert_eq!(second.observation_id, first.observation_id);

    assert_eq!(observation_count(&store), 1, "时间线只有一行");
    let rows = store
        .list_audit_events("company-default", "req-observe")
        .expect("应可读回");
    assert_eq!(rows.len(), 1, "没写入就不该有第二条审计");
}

/// 同键重放：不写库，返回首次结果。
#[test]
fn replaying_the_same_key_returns_the_first_result() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    let command = observe_command(
        "k-obs-1",
        observe_target("acme", "prod", Some("web")),
        "2026-11-21T10:00:00Z",
    );
    let first = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&command, 2_000)
    })
    .expect("首次应成功");
    let replay = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&command, 9_000)
    })
    .expect("重放不是错误");

    assert!(replay.replayed);
    assert_eq!(replay.observation_id, first.observation_id);
    assert_eq!(observation_count(&store), 1);
}

/// 同键异载荷：冲突，且不产生第二行。
#[test]
fn the_same_key_with_a_different_payload_is_refused() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    with_uow(&mut store, |uow| {
        uow.record_delivery_observation(
            &observe_command(
                "k-obs-1",
                observe_target("acme", "prod", Some("web")),
                "2026-11-21T10:00:00Z",
            ),
            2_000,
        )
    })
    .expect("首次应成功");

    let mut changed = observe_command(
        "k-obs-1",
        observe_target("acme", "prod", Some("web")),
        "2026-11-21T10:00:00Z",
    );
    changed.digest = "sha256:def".to_string();

    let err = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&changed, 3_000).map(|_| ())
    })
    .expect_err("同键异载荷必须拒绝");
    assert!(matches!(
        err,
        StoreError::Idempotency(IdempotencyError::KeyReusedWithOtherPayload { .. })
    ));
    assert_eq!(observation_count(&store), 1);
}

/// 人工补证的空备注由**写入原语**拒绝，入口不重写这条规则。
///
/// 这条路径同时证明失败没有污染幂等键：同一个键补上备注后必须能成功，
/// 否则操作者会换一个键重试，于是「同一事实」在库里有了两个身份。
#[test]
fn manual_evidence_without_a_note_is_refused_by_the_primitive() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    let mut without_note = observe_command(
        "k-obs-1",
        observe_target("acme", "prod", Some("web")),
        "2026-11-21T10:00:00Z",
    );
    without_note.kind = ObservationKind::ManualEvidence;
    without_note.source_kind = "manual";
    without_note.note = "   ".to_string();

    let err = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&without_note, 2_000)
            .map(|_| ())
    })
    .expect_err("空备注的补证必须拒绝");
    assert!(matches!(
        err,
        StoreError::Delivery(DeliveryError::EmptyManualEvidenceNote)
    ));
    assert_eq!(observation_count(&store), 0);

    // 同一个键，补上备注：必须成功（失败的尝试没有留下回执）。
    without_note.note = "现场核对：安装包校验通过，授权范围见工单 OPS-77".to_string();
    let outcome = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&without_note, 3_000)
    })
    .expect("补上备注后应可登记");
    assert!(outcome.recorded);
    assert!(!outcome.authorized, "授权不是入口能给的");
}

/// 观察不建目标：打错客户名必须失败，而不是凭空多出一条客户记录。
#[test]
fn observation_does_not_create_a_target() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    let err = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(
            &observe_command(
                "k-obs-1",
                observe_target("acmee", "prod", Some("web")),
                "2026-11-21T10:00:00Z",
            ),
            2_000,
        )
        .map(|_| ())
    })
    .expect_err("目标不存在必须拒绝");
    assert!(matches!(
        err,
        StoreError::Delivery(DeliveryError::TargetNotInTracking { .. })
    ));
    assert_eq!(target_count(&store), 1, "没有多出第二个目标");
    assert_eq!(observation_count(&store), 0);
}

/// 声称连接器却给不出来源修订：入口层拒绝，而且不碰库。
#[test]
fn connector_claims_require_a_source_revision() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    let mut command = observe_command(
        "k-obs-1",
        observe_target("acme", "prod", Some("web")),
        "2026-11-21T10:00:00Z",
    );
    command.source_kind = "connector";

    let err = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&command, 2_000).map(|_| ())
    })
    .expect_err("连接器观察缺来源修订必须拒绝");
    assert!(matches!(
        err,
        StoreError::Observation(ObservationError::ConnectorRequiresSourceRevision)
    ));
    assert_eq!(observation_count(&store), 0);
    assert_eq!(
        store
            .list_audit_events("company-default", "req-observe")
            .expect("应可读回")
            .len(),
        0,
        "被拒的命令不留审计"
    );
}

/// 连接器观察带真实来源修订：写进去，并且证据引用指向**修订**而不是观察 ID。
///
/// 这是「观察挂到来源身份上」的唯一路径：`evidence_reference` 优先用来源修订，
/// 草稿（5.1）的 `evidence_revision` 才能与来源对上同一套编号。
#[test]
fn connector_observation_links_a_source_revision() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);
    with_uow(&mut store, |uow| {
        uow.register_source(
            "src-1",
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
    .expect("来源应可登记");
    with_uow(&mut store, |uow| {
        uow.append_source_revision(&crate::delivery_source::SourceRevisionRow {
            source_revision_id: "rev-1".to_string(),
            company_id: "company-default".to_string(),
            source_id: "src-1".to_string(),
            version: "v1.4.2".to_string(),
            digest: "sha256:abc".to_string(),
            content_fingerprint: crate::delivery_source::revision_fingerprint(
                "v1.4.2",
                "sha256:abc",
            )
            .expect("指纹"),
            actor_id: "ci".to_string(),
            observed_at: "2026-11-21T10:00:00Z".to_string(),
            observed_at_ms: 1_763_715_600_000,
            recorded_at_ms: 1_763_715_600_500,
        })
    })
    .expect("修订应可写入");

    let mut command = observe_command(
        "k-obs-connector",
        observe_target("acme", "prod", Some("web")),
        "2026-11-21T10:00:00Z",
    );
    command.source_kind = "connector";
    command.source_revision_id = Some("rev-1".to_string());

    let outcome = with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&command, 2_000)
    })
    .expect("连接器观察应可登记");
    assert!(outcome.recorded);

    let view = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("读取应成功")
        .expect("跟踪项应存在");
    assert_eq!(
        view.targets[0].evidence_revision.as_deref(),
        Some("rev-1"),
        "证据引用要指向来源修订"
    );
}

/// 乱序到达：先记今天的部署，再补录一条**更早**的回滚——结论仍是「已部署」。
///
/// 判定取最新是按**业务时间**（`observed_at`），不是按写入顺序。若按写入顺序，
/// 「上周部署、今天才补录旧回滚」这种最常见的补录形式会抹掉一次真实上线。
/// 这也是 7.5.3.3 里「乱序按来源序列处理」的可执行含义：
/// 补录能补上证据，但不能改写先后。
#[test]
fn a_late_arriving_older_rollback_does_not_flip_the_conclusion() {
    use crate::delivery_tracking::GAP_ROLLBACK_OBSERVED;

    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_tracking_with_target(&mut store);

    let mut deployment = observe_command(
        "k-deploy",
        observe_target("acme", "prod", Some("web")),
        "2026-11-21T12:00:00Z",
    );
    deployment.kind = ObservationKind::Deployment;
    with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&deployment, 1_000)
    })
    .expect("部署应可登记");

    let before = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("读取应成功")
        .expect("跟踪项应存在");
    assert!(
        !before.targets[0].gaps.contains(&GAP_ROLLBACK_OBSERVED),
        "只有部署时不该出现回滚缺口：{:?}",
        before.targets[0].gaps
    );

    let mut rollback = observe_command(
        "k-rollback",
        observe_target("acme", "prod", Some("web")),
        "2026-11-20T12:00:00Z",
    );
    rollback.kind = ObservationKind::Rollback;
    with_uow(&mut store, |uow| {
        uow.record_delivery_observation(&rollback, 2_000)
    })
    .expect("更早的回滚也应可登记——它确实发生过，只是不是最新");

    let after = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("读取应成功")
        .expect("跟踪项应存在");
    assert_eq!(
        after.targets[0].gaps, before.targets[0].gaps,
        "更早的回滚晚到，不该把结论从「已部署」翻成「已回滚」"
    );
    assert_eq!(
        after.targets[0].observed_at, before.targets[0].observed_at,
        "决定结论的观察仍是那条较新的部署"
    );
    assert_eq!(
        observation_count(&store),
        2,
        "两条事实都留下，一条都没被丢掉"
    );
}
