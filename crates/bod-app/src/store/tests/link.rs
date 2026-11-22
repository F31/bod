//! 关联追加的幂等语义（P8 3.4）：目标去重、期望版本、重放返回首次结果。

use super::*;

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
