//! P1 切片 4.7：操作手册的每一步都用真实 git 仓库 + 文件型 SQLite + 真实 CAS 复现。
//!
//! 本测试不是新能力，而是「手册 ↔ 工作台」的可执行对账：
//! 首次使用 → 真实验证 → 接受 → 证据/备份反查 → 重启查询 → 取消/Unknown → 确认停止，
//! 每一步都从宿主事实投影首页/任务页，不写第二个业务库、不伪造通过结论。
//! 真实 Agent SDK、UI 按钮、桌面进程重启等不在此复现，见 docs/spec-notes/p1-user-flow.md §5。

use bod_app::budget_admission::{Measure, Reservation, ReservationState};
use bod_app::cancel_reconciliation::{
    CancelPhase, CancelRequest, confirm_stop, mark_unknown, pending_cancellations,
    record_cancel_ack, request_cancel, reservation_is_held, stop_is_confirmed,
};
use bod_app::cas::CasStore;
use bod_app::coding_integration::LocalVerificationReport;
use bod_app::config::BodConfig;
use bod_app::delivery_verification::{
    VerificationCheckSpec, execute_verification, expected_report_digests,
    verify_backup_for_delivery, verify_report_evidence,
};
use bod_app::domain::DomainId;
use bod_app::domain::artifact::AcceptancePolicy;
use bod_app::domain::execution::ExecutionState;
use bod_app::domain::model::{Principal, PrincipalKind, Project, RepositoryContext};
use bod_app::first_use::{
    FirstUseAcceptance, FirstUseInput, accept_first_use, bind_first_use_evidence,
    dispatch_first_use, plan_first_use,
};
use bod_app::migration_safety::{BackupInput, create_backup};
use bod_app::recovery_models::BackupStatus;
use bod_app::store::SqliteStore;
use bod_app::workbench_projection::{
    DecisionSource, collect_host_facts, home_from_store, state_fingerprint, task_from_store,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const COMPANY: &str = "company-user-flow";
const PROJECT: &str = "project-user-flow";
const NOW: i64 = 1_763_900_000_000;

struct Fixture {
    _root: PathBuf,
    repo: PathBuf,
    db: PathBuf,
    cas_root: PathBuf,
    backups: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("bod-user-flow-{}-{seq}", std::process::id()));
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("tests")).unwrap();
        fs::write(repo.join("Cargo.toml"), "[package]\nname = 'x'\n").unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=bod",
                "-c",
                "user.email=bod@local",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        Self {
            _root: root.clone(),
            repo,
            db: root.join("bod.sqlite"),
            cas_root: root.join("cas"),
            backups: root.join("backups"),
        }
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open(&self.db, &BodConfig::default()).unwrap()
    }

    fn cas(&self) -> CasStore {
        CasStore::open(&self.cas_root).unwrap()
    }

    fn input(&self) -> FirstUseInput {
        FirstUseInput {
            project: Project {
                id: DomainId::new(PROJECT),
                company_id: DomainId::new(COMPANY),
                name: "真实项目".to_string(),
                repository: Some(RepositoryContext {
                    provider: "local-git".to_string(),
                    repository_id: "repo-user-flow".to_string(),
                }),
            },
            agent_id: DomainId::new("agent-ready"),
            repository: self.repo.clone(),
            title: "修复登录失败".to_string(),
            acceptance: vec![
                "登录成功返回 200".to_string(),
                "错误密码返回 401".to_string(),
            ],
        }
    }

    fn agents() -> Vec<DomainId> {
        vec![DomainId::new("agent-ready")]
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git 应可执行");
    assert!(
        output.status.success(),
        "git {args:?} 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn verifier() -> DomainId {
    DomainId::new("verifier-human")
}

fn user() -> Principal {
    Principal {
        id: DomainId::new("user-1"),
        kind: PrincipalKind::User,
        claimed_roles: Vec::new(),
    }
}

fn checks() -> Vec<VerificationCheckSpec> {
    vec![
        VerificationCheckSpec {
            label: "commit-is-resolvable".to_string(),
            argv: vec![
                "git".to_string(),
                "rev-parse".to_string(),
                "--verify".to_string(),
                "HEAD".to_string(),
            ],
        },
        VerificationCheckSpec {
            label: "worktree-clean".to_string(),
            argv: vec!["git".to_string(), "diff".to_string(), "--quiet".to_string()],
        },
    ]
}

#[test]
fn user_flow_steps_are_reproducible_in_workbench() {
    let fixture = Fixture::new();
    let cas = fixture.cas();
    let mut store = fixture.store();
    let input = fixture.input();
    let agents = Fixture::agents();

    // 手册步骤 1：选项目与仓库 → 说清目标 → 查看范围与验收（只预览不写库）。
    let plan = plan_first_use(&input, &agents).unwrap();
    assert_eq!(plan, plan_first_use(&input, &agents).unwrap());
    assert!(plan.write_scope.allow().iter().any(|p| p == "src/"));
    assert!(!plan.write_scope.allow().iter().any(|p| p == "Cargo.toml"));
    assert_eq!(
        store
            .list_issues_for_project(COMPANY, PROJECT)
            .unwrap()
            .len(),
        0
    );

    // 手册步骤 2：交办；从零补齐 Issue/工作包/Run，并登记真实 git 候选。
    let assignment = dispatch_first_use(&mut store, &input, &agents, NOW).unwrap();
    assert_eq!(
        assignment.commit,
        git(&fixture.repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        assignment.candidate.revision_id.as_str(),
        format!("git-{}", assignment.commit)
    );
    assert_eq!(store.count_work_package_revisions(COMPANY).unwrap(), 1);
    assert_eq!(store.count_runs(COMPANY).unwrap(), 1);
    let home = home_from_store(&store, COMPANY, PROJECT).unwrap();
    assert_eq!(
        home.goal.as_ref().unwrap().issue_uid,
        plan.issue_id.as_str()
    );

    // 手册步骤 3：本地验证真跑命令，正文落 CAS；报告与候选提交/规则/环境同组合。
    let executed = execute_verification(
        &fixture.repo,
        &checks(),
        &plan.rules_revision,
        &plan.environment,
        &verifier(),
        10_000,
        NOW + 1,
        &cas,
    )
    .unwrap();
    assert!(executed.report.passed);
    assert_eq!(executed.checks.len(), 2);
    assert!(cas.object_path(&executed.cas_object.digest).exists());
    // 5.2：报告正文必须注明实际 commit 与环境（不是只写进内存结构体）。
    assert_eq!(executed.report.environment, plan.environment);
    let report_body = String::from_utf8(cas.read(&executed.cas_object.digest).unwrap()).unwrap();
    assert!(report_body.contains(&format!("commit:{}\n", assignment.commit)));
    assert!(report_body.contains(&format!("environment:{}\n", plan.environment.as_str())));
    let report = executed.report.clone();
    let binding = bind_first_use_evidence(&assignment, &report).unwrap();
    assert_eq!(
        binding.merge_combination,
        Some(assignment.merge_combination.clone())
    );
    let stale = LocalVerificationReport::new(
        "0".repeat(40),
        plan.rules_revision.clone(),
        plan.environment.clone(),
        verifier(),
        vec!["git rev-parse HEAD".to_string()],
        true,
        NOW + 2,
    )
    .unwrap();
    assert!(bind_first_use_evidence(&assignment, &stale).is_err());

    // 手册步骤 4：人工接受；绑定合并组合，工作台按真实 Company 看到验证与接受。
    let accepted = accept_first_use(
        &mut store,
        &assignment,
        &report,
        &verifier(),
        &user(),
        &AcceptancePolicy::default(),
        NOW + 3,
    )
    .unwrap();
    let FirstUseAcceptance::Accepted(record) = accepted else {
        panic!("人工接受应写入接受记录");
    };
    assert_eq!(
        record.binding.merge_combination,
        Some(assignment.merge_combination.clone())
    );
    let facts = collect_host_facts(&store, COMPANY, PROJECT).unwrap();
    assert!(facts.verifications.iter().any(|record| record.revision_id
        == assignment.candidate.revision_id
        && record.environment == plan.environment));
    assert_eq!(facts.acceptances.len(), 1);
    assert_eq!(
        facts.acceptances[0].binding.work_package_revision, assignment.plan.work_package_revision,
        "接受记录必须绑定交办时的工作包修订"
    );
    let before = state_fingerprint(&store).unwrap();
    let task = task_from_store(&store, COMPANY, PROJECT, plan.issue_id.as_str())
        .unwrap()
        .expect("任务页应能打开首次使用 Issue");
    assert!(
        task.evidence
            .iter()
            .any(|card| card.reference.starts_with("cas:"))
    );
    assert_eq!(state_fingerprint(&store).unwrap(), before, "投影只读");

    // 手册步骤 5：证据反查与备份核验；删 WAL/删 CAS 的负例由 4.4 分片测试承担。
    let evidence =
        verify_report_evidence(&mut store, &cas, &assignment.candidate, &report).unwrap();
    assert_eq!(evidence.commit, assignment.commit);
    assert_eq!(evidence.checks, report.checks.len());
    let backup = create_backup(
        &mut store,
        &cas,
        &BackupInput {
            company_id: COMPANY,
            destination: &fixture.backups,
            config_revision: "cfg-user-flow",
            adapter_versions: "git=2.47",
            key_recovery_note: "凭据管理器",
        },
        "b-user-flow",
        NOW + 10,
    )
    .unwrap();
    assert_eq!(backup.status, BackupStatus::Verified);
    let checked = verify_backup_for_delivery(&backup, &cas, &expected_report_digests(&executed));
    assert!(checked.recoverable, "问题：{:?}", checked.problems);
    // 5.2 的第四段证据：备份快照重开后仍查到同一修订的接受与验证，
    // 且接受证据引用正是刚核验过的报告 CAS，而不是另一份交付的证据。
    let restored = SqliteStore::open(Path::new(&backup.path), &BodConfig::default()).unwrap();
    let restored_facts = collect_host_facts(&restored, COMPANY, PROJECT).unwrap();
    assert_eq!(restored_facts.acceptances.len(), 1);
    assert_eq!(
        restored_facts.acceptances[0].binding.work_package_revision,
        assignment.plan.work_package_revision
    );
    assert!(
        restored_facts.acceptances[0]
            .evidence_refs
            .contains(&format!("cas:{}", executed.cas_object.digest.value))
    );
    assert!(restored_facts.verifications.iter().any(|record| {
        record.revision_id == assignment.candidate.revision_id
            && record.environment == plan.environment
    }));

    // 手册步骤 6：关闭并重开（真实进程语义不在本切片）→ 同 SQLite 文件仍可查询。
    drop(store);
    let reopened = fixture.store();
    let facts = collect_host_facts(&reopened, COMPANY, PROJECT).unwrap();
    assert_eq!(facts.acceptances.len(), 1);
    assert!(facts.acceptances[0].approves(&assignment.candidate.revision_id));

    // 手册步骤 7：取消回包丢失 → 只记 ack、不停止确认；Unknown 显示处理人与风险占用。
    let mut store = reopened;
    let attempt_id = plan.attempt_id.as_str();
    let reservation = Reservation {
        reservation_id: "res-user-flow".to_string(),
        company_id: DomainId::new(COMPANY),
        project_id: None,
        agent_id: None,
        run_id: None,
        work_package_id: None,
        measure: Measure::money_micros(0, "CNY"),
        reserved: 0,
        confirmed: 0,
        risk: 1_000,
        state: ReservationState::Risk,
        logical_key: Some("user-flow-risk".to_string()),
        created_at_ms: NOW,
        updated_at_ms: NOW,
    };
    {
        let uow = store.transaction().unwrap();
        uow.insert_reservation(&reservation).unwrap();
        uow.commit().unwrap();
    }
    let attempts_before = store.count_attempts(COMPANY).unwrap();
    let request = CancelRequest {
        cancel_id: "cancel-user-flow",
        attempt_id,
        company_id: COMPANY,
        invocation_id: Some("inv-user-flow"),
        risk_reservation_refs: vec!["res-user-flow".to_string()],
        known_evidence: "用户请求取消",
        assignee: "owner-1",
        next_step: "收到回执后查询外部执行状态",
    };
    request_cancel(&mut store, &request, NOW + 20).unwrap();
    let acked =
        record_cancel_ack(&mut store, COMPANY, attempt_id, "ack-user-flow", NOW + 21).unwrap();
    assert_eq!(acked.phase, CancelPhase::AckReceived);
    assert!(!stop_is_confirmed(&acked), "ack 不是停止确认");
    assert!(reservation_is_held(&mut store, "res-user-flow").unwrap());
    assert_eq!(store.count_attempts(COMPANY).unwrap(), attempts_before);

    let unknown = mark_unknown(
        &mut store,
        COMPANY,
        attempt_id,
        "回包丢失，外部结果不明",
        NOW + 22,
    )
    .unwrap();
    assert_eq!(unknown.phase, CancelPhase::Unknown);
    assert!(unknown.known_evidence.contains("回包丢失"));
    let pending = pending_cancellations(&mut store, COMPANY).unwrap();
    assert!(
        pending
            .iter()
            .any(|item| item.required_decision.contains("未确认前保留风险占用"))
    );
    let home = home_from_store(&store, COMPANY, PROJECT).unwrap();
    let card = home
        .decisions
        .iter()
        .find(|card| card.source == DecisionSource::Cancellation)
        .expect("Unknown 必须进入待决定");
    assert_eq!(card.blocking_actor, "owner-1");
    assert!(
        card.available_actions
            .iter()
            .any(|a| a == "inspect_execution")
    );
    assert!(!card.required_decision.contains("已停止"), "不得猜已停止");
    assert!(
        home.blockers
            .iter()
            .any(|card| card.reason == "ReconciliationOpen")
    );

    // 手册步骤 8：独立证据确认停止；结案后不再冒充待决定，预留仍保留给预算结算。
    let stopped = confirm_stop(
        &mut store,
        COMPANY,
        attempt_id,
        "inv-user-flow",
        "external-user-flow",
        ExecutionState::Cancelled,
        NOW + 23,
    )
    .unwrap();
    assert_eq!(stopped.phase, CancelPhase::StopConfirmed);
    assert_eq!(
        stopped.stop_evidence.as_deref(),
        Some("inv-user-flow:external-user-flow:Cancelled")
    );
    assert!(reservation_is_held(&mut store, "res-user-flow").unwrap());
    assert!(
        pending_cancellations(&mut store, COMPANY)
            .unwrap()
            .iter()
            .all(|item| item.attempt_id != attempt_id)
    );
    let home = home_from_store(&store, COMPANY, PROJECT).unwrap();
    assert!(
        !home
            .decisions
            .iter()
            .any(|card| card.source == DecisionSource::Cancellation)
    );
}
