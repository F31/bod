//! `bod tracking` 的端到端用例（P8 3.1）。
//!
//! 这个命令是交付跟踪读路径在**生产入口**上的唯一落点：库里放事实，结论在读取时由
//! `coverage_receipt::judge_coverage` 算出。用例跑真实二进制，不用库 API 代替——
//! 参数解析、退出码与输出形状都是契约的一部分。
//!
//! 为什么不是 HTTP：`GET /api/v1/delivery-tracking/{id}` 的契约承诺「按调用者授权目标过滤」，
//! §34.12 的主体判定还没实现；现在开放等于把不该给的客户给出去。

use bod_app::config::BodConfig;
use bod_app::coverage_receipt::SwitchState;
use bod_app::delivery_link::{AppendLinkCommand, LinkRef};
use bod_app::delivery_tracking::{
    ObservationKind, ObservationRow, TargetRow, TrackingRow, TrackingState,
};
use bod_app::domain::DomainId;
use bod_app::domain::artifact::{VerificationRecord, VerificationState};
use bod_app::store::SqliteStore;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn db_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "bod-tracking-cli-{}-{n}.sqlite",
        std::process::id()
    ))
}

/// 一个已覆盖的目标：部署被观察、验证通过、开关同步、人群已知、制品摘要齐全。
fn seed(path: &Path) {
    let mut store = SqliteStore::open(path, &BodConfig::default()).expect("应可建库");
    {
        let uow = store.transaction().expect("开启事务");
        uow.insert_delivery_tracking(&TrackingRow {
            tracking_id: "dt-1".to_string(),
            company_id: "company-default".to_string(),
            project_id: "project-1".to_string(),
            title: "客户 A 登录失败".to_string(),
            requirement: "登录偶发 500".to_string(),
            owner_id: "alice".to_string(),
            state: TrackingState::Open,
            // 外部交付：三个内部引用全为空，不许为了满足约束造假运行（D3）。
            issue_uid: None,
            run_uid: None,
            attempt_id: None,
            resource_version: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
        .expect("跟踪项应可写入");
        uow.insert_delivery_target(&TargetRow {
            target_id: "t-1".to_string(),
            company_id: "company-default".to_string(),
            tracking_id: "dt-1".to_string(),
            customer_id: "acme".to_string(),
            environment: "prod".to_string(),
            deployment_unit: "web".to_string(),
            acceptance: "冒烟通过".to_string(),
            switch_state: SwitchState::Synced,
            population_known: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
        .expect("目标应可写入");
        uow.append_delivery_observation(&ObservationRow {
            observation_id: "o-1".to_string(),
            company_id: "company-default".to_string(),
            tracking_id: "dt-1".to_string(),
            target_id: "t-1".to_string(),
            kind: ObservationKind::Deployment,
            source_kind: "connector",
            source_ref: "acme/web#318".to_string(),
            // 3.1 的观察还没有来源身份（3.2 起连接器观察必须带来源修订，属 3.5 的导入路径）。
            source_revision_id: None,
            artifact_revision: Some("rev-1".to_string()),
            digest: "sha256:abc".to_string(),
            revision: "deploy-2026-11-21".to_string(),
            note: "部署到客户生产环境".to_string(),
            actor_id: "ci".to_string(),
            authorized: false,
            observed_at: "2026-11-21T10:00:00Z".to_string(),
            observed_at_ms: 1_763_715_600_000,
            recorded_at_ms: 1_763_715_600_500,
        })
        .expect("观察应可写入");
        uow.insert_verification_record(
            "vr-1",
            "company-default",
            &VerificationRecord {
                revision_id: DomainId::new("rev-1"),
                state: VerificationState::Verified,
                actor: DomainId::new("ci"),
                reason: String::new(),
                evidence_refs: Vec::new(),
                verification_rules: DomainId::new("rules-1"),
                environment: DomainId::new("prod"),
                observed_at_ms: 1_763_715_600_100,
            },
        )
        .expect("验证记录应可写入");
        uow.commit().expect("提交事务");
    }
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bod"))
        .args(args)
        .output()
        .expect("应能启动 bod 二进制");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// 详情：结论、决定该结论的证据、缺口与允许动作都在文本里。
#[test]
fn tracking_detail_renders_coverage_with_evidence() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["tracking", "--db", &db, "--id", "dt-1"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("跟踪项 dt-1（project-1）"), "{stdout}");
    assert!(
        stdout.contains("内部关联：无"),
        "外部交付不要求内部运行：{stdout}"
    );
    assert!(
        stdout.contains("客户 acme / 环境 prod / web：covered"),
        "{stdout}"
    );
    assert!(
        stdout.contains("证据：o-1"),
        "要能回答按哪一版证据：{stdout}"
    );
    assert!(stdout.contains("缺口合计：无"), "{stdout}");
    assert!(
        stdout.contains("允许动作：无"),
        "端点未实现时不得承诺动作：{stdout}"
    );
}

/// `--json` 输出的就是冻结契约的响应体形状，客户端不必再维护第二套模型。
#[test]
fn tracking_json_matches_contract_shape() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["tracking", "--db", &db, "--id", "dt-1", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(value["tracking_id"], serde_json::json!("dt-1"));
    assert_eq!(value["project_id"], serde_json::json!("project-1"));
    assert_eq!(value["version"], serde_json::json!(1));
    assert_eq!(value["targets"][0]["verdict"], serde_json::json!("covered"));
    assert_eq!(
        value["targets"][0]["target"]["deployment_unit"],
        serde_json::json!("web")
    );
    assert_eq!(value["gaps"], serde_json::json!([]));
    assert_eq!(value["allowed_actions"], serde_json::json!([]));
    assert!(
        value["evidence"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
}

/// 列表只给身份与规模：结论离开证据没有意义，列表里塞不下证据。
#[test]
fn tracking_list_reports_scale_not_conclusions() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["tracking", "--db", &db]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("dt-1"), "{stdout}");
    assert!(stdout.contains("目标 1"), "{stdout}");
    for conclusion in ["covered", "pending-verification", "pending-install"] {
        assert!(
            !stdout.contains(conclusion),
            "列表不得出现结论 {conclusion}：{stdout}"
        );
    }

    // 机器可读的列表是**摘要**，不是契约响应体：键名用 target_count，
    // 不与契约里 `targets`（数组）同名，免得复用模型的人以为拿到了目标列表。
    let (code, stdout, stderr) = run(&["tracking", "--db", &db, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(value["company_id"], serde_json::json!("company-default"));
    assert_eq!(
        value["trackings"][0]["tracking_id"],
        serde_json::json!("dt-1")
    );
    assert_eq!(value["trackings"][0]["target_count"], serde_json::json!(1));
    assert!(value["trackings"][0].get("targets").is_none());
}

/// 找不到跟踪项时退出码非 0，且说清是哪个公司下没有，而不是返回空对象。
#[test]
fn unknown_tracking_id_fails_loudly() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["tracking", "--db", &db, "--id", "dt-missing"]);
    assert_ne!(code, 0, "找不到必须非 0 退出");
    assert!(stdout.is_empty(), "失败不该往 stdout 写半份结果：{stdout}");
    assert!(stderr.contains("dt-missing"), "{stderr}");
}

/// 缺少 `--db` 时给出用法提示，不猜一个默认库路径。
#[test]
fn missing_db_flag_is_refused() {
    let (code, _stdout, stderr) = run(&["tracking"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("--db"), "{stderr}");
}

/// 读命令不建库、不迁移：路径不存在就报错（P8 7.7.3）。
///
/// 三条断言各挡一件事：
/// - **退出码非 0**：缺库不等于「库里没有记录」。用 0 退出，调用方会把空输出当成
///   「确认了，什么都没有」，而这正是「路径写错」最常见的表现形式。
/// - **报错里带路径**：否则用户不知道该改哪个参数。
/// - **文件没被创建**：`SqliteStore::open` 会 `create_dir_all` 并建库跑迁移，
///   于是一次读取会悄悄变成一次 schema 写入，还在错误的位置留下一份空库。
#[test]
fn read_commands_refuse_missing_db_instead_of_creating_it() {
    for command in ["observe", "tracking", "sources"] {
        let path = db_path();
        assert!(!path.exists(), "{command}: 前置应为不存在的路径");
        let db = path.to_string_lossy().to_string();
        let (code, stdout, stderr) = run(&[command, "--db", &db]);
        assert_ne!(code, 0, "{command} 缺库必须失败，实际 stdout：{stdout}");
        assert!(stdout.is_empty(), "{command} 失败不该往 stdout 写半份结果：{stdout}");
        assert!(stderr.contains("不存在"), "{command}: {stderr}");
        assert!(!path.exists(), "{command} 不得创建库");
    }
}

// ---------------------------------------------------------------------------
// 后补的内部工作包（P8 3.4）
// ---------------------------------------------------------------------------

/// 追加的内部工作包在详情里可见，且版本随之推进。
///
/// 3.4 没有写命令入口（§34.12 的主体判定未实现），所以这里用库层入口造事实，
/// 再用唯一的读出口核对：写进去的东西必须看得见，否则等于没有记录。
#[test]
fn tracking_detail_shows_later_work_packages() {
    let path = db_path();
    seed(&path);
    {
        let mut store = SqliteStore::open(&path, &BodConfig::default()).expect("应可建库");
        let uow = store.transaction().expect("开启事务");
        uow.append_delivery_link(
            &AppendLinkCommand {
                idempotency_key: "k-wp".to_string(),
                company_id: "company-default".to_string(),
                tracking_id: "dt-1".to_string(),
                link: LinkRef::WorkPackage("wp-17".to_string()),
                targets: Vec::new(),
                expected_version: 1,
                linked_by: "alice".to_string(),
                correlation_id: "req-cli-1".to_string(),
                decided_at: "2026-11-22T00:00:00Z".to_string(),
            },
            1_763_715_601_000,
        )
        .expect("工作包关联应可追加");
        uow.commit().expect("提交事务");
    }
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["tracking", "--db", &db, "--id", "dt-1"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("内部工作包：1 个"), "{stdout}");
    assert!(stdout.contains("wp-17"), "{stdout}");
    assert!(stdout.contains("alice"), "{stdout}");
    assert!(stdout.contains("版本：2"), "追加后版本要推进：{stdout}");
    // 补工作包不等于伪造运行：内部引用仍然是空。
    assert!(stdout.contains("内部关联：无"), "{stdout}");

    // 工作包不进 2.2 冻结的响应体：`--json` 形状不受影响。
    let (code, stdout, stderr) = run(&["tracking", "--db", &db, "--id", "dt-1", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(value["version"], serde_json::json!(2));
    assert!(value.get("work_packages").is_none(), "{stdout}");
}

// ---------------------------------------------------------------------------
// `bod sources`（P8 3.2）
// ---------------------------------------------------------------------------

/// 一个已确认来源 + 一条修订 + 一条待确认候选。
fn seed_sources(path: &Path) {
    use bod_app::delivery_source::{
        CandidateProposal, SourceIdentity, SourceRevisionRow, revision_fingerprint,
    };
    let mut store = SqliteStore::open(path, &BodConfig::default()).expect("应可建库");
    let uow = store.transaction().expect("开启事务");
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
        100,
    )
    .expect("来源应可登记");
    uow.append_source_revision(&SourceRevisionRow {
        source_revision_id: "rev-1".to_string(),
        company_id: "company-default".to_string(),
        source_id: "src-1".to_string(),
        version: "v1.4.2".to_string(),
        digest: "sha256:abc".to_string(),
        content_fingerprint: revision_fingerprint("v1.4.2", "sha256:abc").expect("指纹"),
        actor_id: "ci".to_string(),
        observed_at: "2026-11-21T10:00:00Z".to_string(),
        observed_at_ms: 1_763_715_600_000,
        recorded_at_ms: 1_763_715_600_500,
    })
    .expect("修订应可写入");
    uow.propose_source_candidate(
        "cand-1",
        &CandidateProposal {
            company_id: "company-default".to_string(),
            project_id: "project-1".to_string(),
            connection_id: "gh-1".to_string(),
            object_type: String::new(),
            locator: "https://gitlab.example.com/acme/web/-/merge_requests/7".to_string(),
        },
        "bob",
        200,
    )
    .expect("候选应可提交");
    uow.commit().expect("提交事务");
}

/// 来源列表给身份坐标与修订条数；没加 `--candidates` 时不该出现候选。
#[test]
fn sources_list_shows_identity_and_revision_count() {
    let path = db_path();
    seed_sources(&path);
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["sources", "--db", &db]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("src-1"), "{stdout}");
    assert!(
        stdout.contains("company-default/project-1/gh-1/pull-request/PR_kw318"),
        "五元组坐标要能看懂：{stdout}"
    );
    assert!(stdout.contains("修订 1"), "{stdout}");
    assert!(
        !stdout.contains("cand-1"),
        "未加 --candidates 时不列候选：{stdout}"
    );
}

/// 候选只有 pending，且 `--json` 如实标出来——不能把候选显示成身份。
#[test]
fn sources_json_marks_pending_candidates() {
    let path = db_path();
    seed_sources(&path);
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["sources", "--db", &db, "--candidates", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(
        value["sources"][0]["provider_object_id"],
        serde_json::json!("PR_kw318")
    );
    assert_eq!(value["sources"][0]["revision_count"], serde_json::json!(1));
    assert_eq!(
        value["candidates"][0]["candidate_id"],
        serde_json::json!("cand-1")
    );
    assert_eq!(
        value["candidates"][0]["state"],
        serde_json::json!("pending")
    );
    assert!(
        value["candidates"][0]["resolved_source_id"].is_null(),
        "待确认候选没有身份：{stdout}"
    );
    // 候选不得混进 sources：它没有稳定 ID，也就不是身份。
    assert_eq!(
        value["sources"].as_array().map(|items| items.len()),
        Some(1),
        "{stdout}"
    );
}

/// 按项目过滤：另一个项目的来源不出现。
#[test]
fn sources_can_be_scoped_to_one_project() {
    let path = db_path();
    seed_sources(&path);
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&["sources", "--db", &db, "--project", "project-2", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(value["sources"], serde_json::json!([]), "{stdout}");
}
