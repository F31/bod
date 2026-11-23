//! 存储层用例：迁移、事务、条件更新、交付域的全部不变量。
//!
//! 这些用例直接打真实 SQLite（内存或临时文件），不放 mock：
//! 要验证的正是「约束在库里生效」，替身会让结论失去意义。
//!
//! **按域分文件**：每个子模块只放自己那一段，跨域复用的夹具留在本文件末尾的
//! 「公共夹具」区。加用例时先找域、再加到对应文件——不要往本文件里塞测试。

mod audit;
mod delivery;
mod import;
mod link;
mod observation;
mod source;
mod store_core;

use super::*;

// -----------------------------------------------------------------------
// 公共夹具（两个以上子模块会用到；只在一个子模块里用的留在那个子模块）
// -----------------------------------------------------------------------

fn config() -> BodConfig {
    let mut config = BodConfig::default();
    config.sqlite.busy_timeout_ms = 5_000;
    config
}

/// 审计写入走一次真事务：审计必须在与业务事实同一个事务边界里提交，
/// 否则会出现「业务回滚了、审计里却记着发生过」。
fn with_uow<T>(store: &mut SqliteStore, body: impl FnOnce(&UnitOfWork<'_>) -> T) -> T {
    let uow = UnitOfWork::begin(&mut store.conn).expect("开启事务");
    let out = body(&uow);
    uow.commit().expect("提交事务");
    out
}

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
