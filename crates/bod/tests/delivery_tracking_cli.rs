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
        assert!(
            stdout.is_empty(),
            "{command} 失败不该往 stdout 写半份结果：{stdout}"
        );
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

// ---------------------------------------------------------------------------
// 写入入口（P8 7.5.3 第一条）：`bod tracking import`
// ---------------------------------------------------------------------------
//
// ADR-0009 选 B（写入只走 CLI）之后，这是交付域**第一条真实的写入口**：
// HTTP 的五条 POST 仍是 planned，`serve` 依旧只读挂载。所以这些用例跑真实二进制、
// 跑真实库——参数解析、退出码、幂等语义与「写进去读得到」都是契约的一部分。

/// 用例里的本机账户。真实部署读的是同一个环境变量，改不了，也不需要改。
const CLI_USER: &str = "cli-actor";

/// 以指定本机账户跑命令。
///
/// 主体由宿主按本地身份装配（ADR-0007 第 6 条），命令行上没有覆盖开关——
/// 所以用例要控制主体只能改子进程环境。这本身也是一次验证：`--as` 不存在。
fn run_as(user: &str, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bod"))
        .args(args)
        .env("USERNAME", user)
        .env("USER", user)
        .env("LOGNAME", user)
        .output()
        .expect("应能启动 bod 二进制");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// 造一条「本机主体是本项目成员」的事实。
///
/// `project_members` 至今**没有生产写入者**（B3 之后才有入口），所以只能直接写 SQL。
/// 用库层 API 假装有写入者会把那个缺口掩盖掉——而「允许动作判定读不到事实」
/// 与「这个人没有权限」在界面上长得一样，正是最容易糊过去的地方。
fn seed_project_member(path: &Path, principal: &str, project: &str) {
    let conn = rusqlite::Connection::open(path).expect("应能打开库");
    conn.execute(
        "INSERT INTO project_members
             (company_id, project_id, principal_id, kind, revision, active, updated_at_ms)
         VALUES ('company-default', ?1, ?2, 'user', 1, 1, 1)",
        rusqlite::params![project, principal],
    )
    .expect("成员资格应可写入");
}

fn import_args<'a>(db: &'a str, key: &'a str, locator: &'a str) -> Vec<&'a str> {
    vec![
        "tracking",
        "import",
        "--db",
        db,
        "--key",
        key,
        "--project",
        "project-1",
        "--connection",
        "gh-1",
        "--object-type",
        "pull-request",
        "--provider-object-id",
        "PR_kw318",
        "--locator",
        locator,
        "--title",
        "客户 A 登录失败",
        "--json",
    ]
}

/// 写进去 → 读得到；同一键重试是重放；换键导同一对象不建第二项。
#[test]
fn import_creates_a_tracked_source_and_replays_on_retry() {
    let path = db_path();
    let db = path.to_string_lossy().to_string();

    let (code, stdout, stderr) = run_as(CLI_USER, &import_args(&db, "k-1", "acme/web#318"));
    assert_eq!(code, 0, "stderr: {stderr}");
    let first: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(
        first["created_tracking"],
        serde_json::json!(true),
        "{stdout}"
    );
    assert_eq!(first["created_source"], serde_json::json!(true), "{stdout}");
    assert_eq!(first["replayed"], serde_json::json!(false), "{stdout}");
    let tracking_id = first["tracking_id"]
        .as_str()
        .expect("导入应给出跟踪项")
        .to_string();
    assert!(
        tracking_id.starts_with("dt-"),
        "跟踪项 ID 由身份派生：{stdout}"
    );

    // 重试：同键同载荷 → 重放。`created_*` 保留首次执行的事实，`replayed` 说本次。
    let (code, stdout, stderr) = run_as(CLI_USER, &import_args(&db, "k-1", "acme/web#318"));
    assert_eq!(code, 0, "stderr: {stderr}");
    let replay: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(replay["replayed"], serde_json::json!(true), "{stdout}");
    assert_eq!(replay["tracking_id"].as_str(), Some(tracking_id.as_str()));
    assert_eq!(
        replay["created_tracking"],
        serde_json::json!(true),
        "重放回答的是「那条命令当时做了什么」：{stdout}"
    );

    // 换键导同一个对象（同一五元组）：命中已有来源与跟踪关系，不建第二项。
    let (code, stdout, stderr) = run_as(CLI_USER, &import_args(&db, "k-2", "acme/web#318"));
    assert_eq!(code, 0, "stderr: {stderr}");
    let second: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(
        second["created_tracking"],
        serde_json::json!(false),
        "{stdout}"
    );
    assert_eq!(
        second["created_source"],
        serde_json::json!(false),
        "{stdout}"
    );
    assert_eq!(
        second["tracking_id"].as_str(),
        Some(tracking_id.as_str()),
        "同一来源身份只承载一条跟踪关系：{stdout}"
    );

    // 写进去的东西必须读得到：写入口与读入口指向同一份事实。
    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &["tracking", "--db", &db, "--id", &tracking_id, "--json"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let view: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(
        view["tracking_id"],
        serde_json::json!(tracking_id),
        "{stdout}"
    );
    assert_eq!(
        view["project_id"],
        serde_json::json!("project-1"),
        "{stdout}"
    );
}

/// 同键异载荷：拒绝，且不留下半条写入。
#[test]
fn import_with_a_different_payload_under_the_same_key_is_refused() {
    let path = db_path();
    let db = path.to_string_lossy().to_string();
    let (code, _, stderr) = run_as(CLI_USER, &import_args(&db, "k-1", "acme/web#318"));
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, stdout, stderr) = run_as(CLI_USER, &import_args(&db, "k-1", "acme/web#319"));
    assert_ne!(code, 0, "同键换载荷必须失败，实际 stdout：{stdout}");
    assert!(stdout.is_empty(), "失败不该往 stdout 写半份结果：{stdout}");
    assert!(
        stderr.contains("同一个幂等键"),
        "错误要说清是键的问题而不是数据的问题：{stderr}"
    );

    // 第一次导入仍然在：拒绝的是第二条命令，不是回滚第一条。
    let (code, stdout, stderr) = run_as(CLI_USER, &["sources", "--db", &db, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(
        value["sources"].as_array().map(Vec::len),
        Some(1),
        "被拒绝的导入不得影响已登记的来源：{stdout}"
    );
}

/// `--as` 被明确拒绝，而不是「不认识」——它看起来太合理了，静默忽略更危险。
#[test]
fn import_refuses_a_self_reported_subject() {
    let path = db_path();
    let db = path.to_string_lossy().to_string();
    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &[
            "tracking",
            "import",
            "--db",
            &db,
            "--key",
            "k-1",
            "--project",
            "project-1",
            "--locator",
            "acme/web#318",
            "--as",
            "mallory",
        ],
    );
    assert_ne!(code, 0, "自报主体必须失败，实际 stdout：{stdout}");
    assert!(stderr.contains("不得自报"), "{stderr}");
    assert!(!path.exists(), "取主体就该在碰库之前失败，不得建库");
}

/// 允许动作：成员看得到 `link-source`，非成员看不到；判定依据是真实装配的事实。
///
/// 这条用例同时钉住两件事：`CLI_ENTRIES` 非空之后视图不再恒空（判定不是坏的），
/// 以及「主体从哪来」——换 `USERNAME` 就换人，命令行上没有别的办法。
#[test]
fn tracking_detail_advertises_link_source_only_to_a_project_member() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();

    let detail = |user: &str| {
        let (code, stdout, stderr) =
            run_as(user, &["tracking", "--db", &db, "--id", "dt-1", "--json"]);
        assert_eq!(code, 0, "stderr: {stderr}");
        serde_json::from_str::<serde_json::Value>(&stdout).expect("应输出合法 JSON")
    };

    let outsider = detail("nobody");
    assert_eq!(
        outsider["allowed_actions"],
        serde_json::json!([]),
        "非成员不得看到动作：{outsider}"
    );

    seed_project_member(&path, CLI_USER, "project-1");
    let member = detail(CLI_USER);
    let actions = member["allowed_actions"]
        .as_array()
        .expect("allowed_actions 是数组");
    assert!(
        actions
            .iter()
            .any(|item| item.as_str() == Some("link-source")),
        "成员应看到 link-source（CLI 面已有 `bod tracking link`）：{member}"
    );
}

/// 入口表里写的子命令必须真的存在：表是数据，数据要有对应的行为。
#[test]
fn the_command_named_in_the_entry_table_actually_exists() {
    let (code, stdout, stderr) = run(&["--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        !bod_app::delivery_api::CLI_ENTRIES.is_empty(),
        "ADR-0009 选 B 之后 CLI 是唯一写入面，入口表不得为空"
    );
    for (action, command) in bod_app::delivery_api::CLI_ENTRIES {
        assert!(
            stdout.contains(command),
            "入口表写着 {action} 由 `{command}` 承载，但 --help 里没有这条命令"
        );
    }
}

// ---------------------------------------------------------------------------
// 关联追加的写入口（P8 7.5.3.2）
// ---------------------------------------------------------------------------
//
// `bod tracking link` 是第二条真实写入口。3.4 的写原语早就在，缺的只是操作者手上有
// 一个能跑的命令——以及 `expected_version` 到底由谁给出这件事，在命令行上没有 HTTP
// 请求体可以携带。这一组用例钉住的是**入口的语义**，不是写原语（后者在 store 用例里）。

fn link_args<'a>(
    db: &'a str,
    key: &'a str,
    flag: &'a str,
    value: &'a str,
    expected_version: &'a str,
) -> Vec<&'a str> {
    vec![
        "tracking",
        "link",
        "--db",
        db,
        "--id",
        "dt-1",
        "--key",
        key,
        "--expected-version",
        expected_version,
        flag,
        value,
        "--json",
    ]
}

/// 补挂内部工作包：写进去、版本推进、读得到。
///
/// 这是「导入之后再定归属」的常用顺序：缺陷先到，工作包后挂。两条命令分开的代价是
/// 要带版本；收益是导入不必等归属确定。
#[test]
fn link_appends_a_work_package_and_bumps_the_version() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();

    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &link_args(&db, "k-1", "--work-package", "wp-17", "1"),
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let outcome: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(
        outcome["tracking_id"],
        serde_json::json!("dt-1"),
        "{stdout}"
    );
    assert_eq!(outcome["appended"], serde_json::json!(true), "{stdout}");
    assert_eq!(outcome["replayed"], serde_json::json!(false), "{stdout}");
    assert_eq!(outcome["version"], serde_json::json!(2), "{stdout}");
    assert!(
        outcome["link_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("dl-")),
        "关系 ID 由身份派生：{stdout}"
    );

    // 写入口与读入口指向同一份事实：追加的东西必须看得见。
    let (code, stdout, stderr) = run_as(CLI_USER, &["tracking", "--db", &db, "--id", "dt-1"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("内部工作包：1 个"), "{stdout}");
    assert!(stdout.contains("wp-17"), "{stdout}");
    assert!(stdout.contains("版本：2"), "{stdout}");
}

/// 期望版本过期：拒绝、什么都不写、并把当前版本直接告诉操作者。
///
/// 这条用例是这个命令存在的理由。若 `--expected-version` 改成「自己读当前版本」，
/// 它会永远绿——因为读和写之间那段窗口被命令自己填平了，闸门不再有机会触发。
#[test]
fn link_refuses_a_stale_expected_version_and_names_the_current_one() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();

    let (code, _, stderr) = run_as(
        CLI_USER,
        &link_args(&db, "k-1", "--work-package", "wp-17", "1"),
    );
    assert_eq!(code, 0, "第一次追加应成功，stderr: {stderr}");

    // 拿旧版本再追加另一条：版本已经变成 2，这条命令是照着 v1 的状态做的决定。
    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &link_args(&db, "k-2", "--work-package", "wp-99", "1"),
    );
    assert_ne!(code, 0, "过期版本必须拒绝，实际 stdout：{stdout}");
    assert!(stdout.is_empty(), "失败不该往 stdout 写半份结果：{stdout}");
    assert!(
        stderr.contains("当前版本是 2"),
        "错误要直接给出当前版本，别让人再去猜：{stderr}"
    );
    assert!(
        stderr.contains("--expected-version 2"),
        "还要给出可照抄的下一步命令：{stderr}"
    );

    // 拒绝的那条没有留下半条写入：工作包仍是 1 个，版本仍是 2。
    let (code, stdout, stderr) = run_as(CLI_USER, &["tracking", "--db", &db, "--id", "dt-1"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("内部工作包：1 个"), "{stdout}");
    assert!(!stdout.contains("wp-99"), "{stdout}");
    assert!(stdout.contains("版本：2"), "{stdout}");
}

/// 同键重放：返回**首次**结果里的版本，不回今天读到的版本。
///
/// 两件事都重要：重放不该再写一次；重放也不该把「现在能做什么」混进「那条命令当时
/// 做了什么」——客户端拿今天的版本去追加，会得到冲突而不是静默覆盖，那是安全的一侧。
#[test]
fn link_replays_the_first_result_not_todays_version() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();

    let (code, _, stderr) = run_as(
        CLI_USER,
        &link_args(&db, "k-1", "--work-package", "wp-17", "1"),
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    // 再追加一条，把当前版本推到 3。
    let (code, _, stderr) = run_as(
        CLI_USER,
        &link_args(&db, "k-2", "--work-package", "wp-18", "2"),
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &link_args(&db, "k-1", "--work-package", "wp-17", "1"),
    );
    assert_eq!(code, 0, "重放不是错误，stderr: {stderr}");
    let replay: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    assert_eq!(replay["replayed"], serde_json::json!(true), "{stdout}");
    assert_eq!(
        replay["appended"],
        serde_json::json!(true),
        "重放回答的是「那条命令当时做了什么」：{stdout}"
    );
    assert_eq!(
        replay["version"],
        serde_json::json!(2),
        "重放不许把今天的版本塞回历史结果：{stdout}"
    );
}

/// 引用必须恰好给一个：都不给与都给，修法不同，报错也不能合成一档。
#[test]
fn link_requires_exactly_one_reference() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();

    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &[
            "tracking",
            "link",
            "--db",
            &db,
            "--id",
            "dt-1",
            "--key",
            "k-1",
            "--expected-version",
            "1",
        ],
    );
    assert_ne!(code, 0, "两个引用都不给必须失败，实际 stdout：{stdout}");
    assert!(
        stderr.contains("必须给一个"),
        "缺字段的提示要指向补什么：{stderr}"
    );

    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &[
            "tracking",
            "link",
            "--db",
            &db,
            "--id",
            "dt-1",
            "--key",
            "k-2",
            "--expected-version",
            "1",
            "--source",
            "ds-abc",
            "--work-package",
            "wp-17",
        ],
    );
    assert_ne!(code, 0, "两个引用都给必须失败，实际 stdout：{stdout}");
    assert!(
        stderr.contains("只能给一个"),
        "多余字段的提示要指向删什么：{stderr}"
    );
}

/// 不给期望版本就失败，并说清为什么不去替调用方读。
#[test]
fn link_refuses_to_guess_the_expected_version() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();

    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &[
            "tracking",
            "link",
            "--db",
            &db,
            "--id",
            "dt-1",
            "--key",
            "k-1",
            "--work-package",
            "wp-17",
        ],
    );
    assert_ne!(code, 0, "缺期望版本必须失败，实际 stdout：{stdout}");
    assert!(
        stderr.contains("不会替你读"),
        "要说明为什么不自动读，否则这看起来是个可以补的默认值：{stderr}"
    );
}

/// 按身份补目标：已有的不重建（seed 里的 `t-1` 就是 acme/prod/web），缺的才新建。
///
/// 新建的目标没有验收条件与开关状态（`acceptance=''`、`switch_state=unknown`），
/// 因此补挂一个未验收目标会让这条跟踪项的结论从「全覆盖」退回「有缺口」——
/// 这不是副作用，是事实：多一个客户目标就多一份没被证明的东西。
#[test]
fn link_creates_missing_targets_by_identity() {
    let path = db_path();
    seed(&path);
    let db = path.to_string_lossy().to_string();

    let (code, stdout, stderr) = run_as(
        CLI_USER,
        &[
            "tracking",
            "link",
            "--db",
            &db,
            "--id",
            "dt-1",
            "--key",
            "k-1",
            "--expected-version",
            "1",
            "--work-package",
            "wp-17",
            "--target",
            "acme:prod:web",
            "--target",
            "acme:staging",
            "--json",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let outcome: serde_json::Value = serde_json::from_str(&stdout).expect("应输出合法 JSON");
    let created = outcome["created_targets"]
        .as_array()
        .expect("created_targets 是数组");
    assert_eq!(
        created.len(),
        1,
        "只有 acme:staging 是新的，acme:prod:web 按身份命中 seed 里的 t-1：{stdout}"
    );

    let (code, stdout, stderr) = run_as(CLI_USER, &["tracking", "--db", &db, "--id", "dt-1"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("环境 staging"), "新目标要看得见：{stdout}");
}
