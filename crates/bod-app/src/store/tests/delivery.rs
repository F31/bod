//! 交付跟踪视图与观测（P8 3.1）：覆盖结论在读时算出，观测只追加。

use super::*;

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
