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
