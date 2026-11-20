//! P2 故障验收（任务 5.1）：六个命名场景，每个都驱动真实代码并写真实断言。
//!
//! 规格要求（`handoff-package` / `manager-transfer` 的 P2 出口条件）：执行 P2 故障验收——
//! 停旧写入、脏文件、丢回包、崩溃中交接、旧经理并发切换、B 不可用，并让 G-28—G-30、
//! G-39—G-41 有**命名 case 与真实断言**，而不是"设计上支持"。
//!
//! 六个 case 与门禁的对应（前缀 `case_` 后面就是场景名，便于在报告里点名）：
//!
//! | # | 命名 case | 场景 | 门禁 |
//! |---|---|---|---|
//! | 1 | `case_stop_old_writer` | 旧执行仍在写、停不下来 | G-29 / G-41 |
//! | 2 | `case_dirty_files` | 现场有未提交改动 | G-28 |
//! | 3 | `case_lost_reply` | 提交后回包丢失 | G-39 |
//! | 4 | `case_crash_mid_handoff` | 交接中途崩溃（旧已撤、新未立） | G-39 / G-38 |
//! | 5 | `case_concurrent_manager_switch` | 两个经理争同一作用域 | G-40 / G-41 |
//! | 6 | `case_target_runtime_unavailable` | 目标运行时不可用 | G-28 |
//!
//! 本文件只做验收，不改任何既有实现，也不写 `verification/gates.yaml`（P2 的 I/G 绑定按纪律
//! 保持 `planned`，由 5.3 统一补齐）。因此这里的通过**不等于**门禁已 implemented：
//! 它证明的是"这些故障场景在本机有可复跑的命名断言"。

use bod_app::adapter_conformance::Enablement;
use bod_app::capability_dispatch::{Candidate, HandoffRefusal, admit_handoff};
use bod_app::config::BodConfig;
use bod_app::domain::execution::{EvidenceLevel, EvidenceSource};
use bod_app::fake_executor::{FakeExecutor, FakeScenario};
use bod_app::handoff_package::{CombinationKey, HandoffLevel, TestedCombination};
use bod_app::invocation::{StartTransport, StartVerdict, reconcile_invocation, submit_start};
use bod_app::lifecycle_recovery::{CaseError, CaseRequest, open_case, resolve_case};
use bod_app::manager_transfer::{
    AppointRequest, ManagementAppointment, ManagementCommand, ManagementCommandSet, ManagementGate,
    ManagementScope, ManagementTicket, TransferError,
};
use bod_app::store::SqliteStore;
use bod_app::workspace_safety::{
    ReclaimOutcome, ReclaimPolicy, SceneRegistry, WorkspaceError, WorkspaceManager, WriteScope,
    gate_diff,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMPANY: &str = "company-1";
const AUDIENCE: &str = "host:local";
const NOW_MS: i64 = 1_765_000_000_000;
const ISS: &str = "2026-11-20T12:00:00Z";
const NOT_AFTER: &str = "2026-11-20T12:15:00Z";

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn store() -> SqliteStore {
    SqliteStore::in_memory(&BodConfig::default()).expect("内存库应可打开")
}

/// 临时根目录：测试结束尽力清理，清理失败不影响断言。
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("bod-p2-fault-{tag}-{}-{nanos}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|err| panic!("git {args:?} 无法执行（验收需要 git 在 PATH 上）：{err}"));
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 一个有首个提交的仓库，返回 HEAD。
fn init_repo(repo: &Path) -> String {
    fs::create_dir_all(repo).expect("建仓库目录");
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "dev@bod.local"]);
    git(repo, &["config", "user.name", "bod-fault-test"]);
    fs::write(repo.join("README.md"), "base\n").expect("写基线文件");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "base"]);
    git(repo, &["rev-parse", "HEAD"])
}

fn commands() -> ManagementCommandSet {
    ManagementCommandSet::new(vec![
        ManagementCommand::PlanRevise,
        ManagementCommand::AssignTask,
        ManagementCommand::ReadStatus,
        ManagementCommand::SubmitDelivery,
    ])
    .expect("命令集应合法")
}

fn appoint_request(manager: &str) -> AppointRequest {
    AppointRequest::new(
        COMPANY,
        ManagementScope::project("project-1"),
        manager,
        "grant:manager-r2",
        "user-owner",
        ISS,
        Some("2027-01-01T00:00:00Z".to_string()),
        AUDIENCE,
        commands(),
    )
}

fn ticket(appointment: &ManagementAppointment, command: ManagementCommand) -> ManagementTicket {
    ManagementTicket::issue(appointment, command, ISS, NOT_AFTER).expect("签发票证应成功")
}

// ---------------------------------------------------------------------------
// 1. 停旧写入（G-29 / G-41）
// ---------------------------------------------------------------------------

/// 旧执行停不下来时，新执行不能把它的改动当成自己的成果提交，也不能销毁它的现场。
///
/// 断言三件事：
/// 1. 越界路径（旧执行写的文件）进 `NeedsRevision`，且只拒这一条；
/// 2. 本工作包范围内的改动被接受（门禁不是一刀切拒绝）；
/// 3. 旧现场回收时被登记保留，目录与改动都还在（"停不下来"就不能假装停了）。
#[test]
fn case_stop_old_writer_rejects_foreign_diff_and_keeps_the_scene() {
    let tmp = TempRoot::new("stop-old-writer");
    let repo = tmp.path.join("repo");
    let head = init_repo(&repo);
    let manager = WorkspaceManager::new(&repo, tmp.path.join("worktrees"));

    let old = manager
        .create("attempt-old", "bod/attempt-old", &head)
        .expect("建旧工作区");
    let new = manager
        .create("attempt-new", "bod/attempt-new", &head)
        .expect("建新工作区");

    // 旧执行仍在写：它的改动留在自己的 worktree 里。
    fs::create_dir_all(old.path.join("crates/bod-app/src")).expect("建目录");
    fs::write(old.path.join("crates/bod-app/src/legacy.rs"), "// old\n").expect("旧执行写文件");
    git(&old.path, &["add", "crates/bod-app/src/legacy.rs"]);

    fs::create_dir_all(new.path.join("crates/bod-app/src/new_work")).expect("建目录");
    fs::write(
        new.path.join("crates/bod-app/src/new_work/feature.rs"),
        "// new\n",
    )
    .expect("新执行写文件");

    // 新工作包的写范围只覆盖自己的目录。
    let scope = WriteScope::new(vec!["crates/bod-app/src/new_work/".to_string()], Vec::new());
    let verdict = gate_diff(
        &repo,
        &scope,
        &[
            "crates/bod-app/src/legacy.rs".to_string(),
            "crates/bod-app/src/new_work/feature.rs".to_string(),
        ],
    );
    assert!(!verdict.is_accepted(), "越界改动必须进 NeedsRevision");
    assert_eq!(
        verdict.rejected_paths(),
        vec!["crates/bod-app/src/legacy.rs".to_string()],
        "只应拒绝旧执行写的那一条，范围内的改动不能连带被拒"
    );

    // 旧执行停不下来 → 只能保留现场，改动必须还在。
    let mut scenes = SceneRegistry::new();
    let outcome = manager
        .reclaim(&old, &ReclaimPolicy::KeepDirty, &mut scenes)
        .expect("保留现场应成功");
    let ReclaimOutcome::Retained(scene) = outcome else {
        panic!("脏现场必须被登记保留，实际 {outcome:?}");
    };
    assert_eq!(scene.attempt_id, "attempt-old");
    assert_eq!(
        scene.dirty_paths,
        vec!["crates/bod-app/src/legacy.rs".to_string()],
        "登记表必须记下到底哪些改动还在"
    );
    assert!(old.path.exists(), "旧执行可能仍在写，目录不能被销毁");
    assert_eq!(
        manager.changed_paths(&old).expect("读旧改动"),
        vec!["crates/bod-app/src/legacy.rs".to_string()],
        "保留后改动应仍可核对"
    );
    assert!(
        scenes.is_pending("attempt-old"),
        "保留的现场必须有待核处置记录"
    );
}

// ---------------------------------------------------------------------------
// 2. 脏文件（G-28）
// ---------------------------------------------------------------------------

/// 有未提交改动的工作区，回收必须"先登记再处置"：没确认不销毁，确认后才销毁。
#[test]
fn case_dirty_files_are_retained_then_require_explicit_confirmation() {
    let tmp = TempRoot::new("dirty");
    let repo = tmp.path.join("repo");
    let head = init_repo(&repo);
    let manager = WorkspaceManager::new(&repo, tmp.path.join("worktrees"));
    let workspace = manager
        .create("attempt-dirty", "bod/attempt-dirty", &head)
        .expect("建工作区");

    assert!(manager.is_clean(&workspace).expect("读干净状态"));
    fs::write(workspace.path.join("scratch.txt"), "未提交\n").expect("写脏文件");
    assert!(!manager.is_clean(&workspace).expect("读脏状态"));

    // 没有确认 → 拒绝销毁，并把路径列出来。
    let mut scenes = SceneRegistry::new();
    let err = manager
        .reclaim(
            &workspace,
            &ReclaimPolicy::DiscardDirty {
                confirmation: "   ".to_string(),
            },
            &mut scenes,
        )
        .expect_err("空白确认不得销毁现场");
    match err {
        WorkspaceError::DirtyNotDisposed { paths } => {
            assert_eq!(paths, vec!["scratch.txt".to_string()])
        }
        other => panic!("期望 DirtyNotDisposed，实际 {other:?}"),
    }
    assert!(workspace.path.exists(), "拒绝销毁时目录必须还在");

    // 显式确认 → 允许销毁。
    let outcome = manager
        .reclaim(
            &workspace,
            &ReclaimPolicy::DiscardDirty {
                confirmation: "已核对：这是临时草稿".to_string(),
            },
            &mut scenes,
        )
        .expect("显式确认后应可回收");
    assert_eq!(outcome, ReclaimOutcome::Removed);
    assert!(!workspace.path.exists(), "确认后目录应被移除");
}

// ---------------------------------------------------------------------------
// 3. 丢回包（G-39）
// ---------------------------------------------------------------------------

/// 提交后回包丢失：进 Unknown、不新建 Attempt；对账出真实状态后仍只有一条 invocation；
/// 对账案件按逻辑动作键幂等，且结案必须有裁决。
#[test]
fn case_lost_reply_reconciles_without_a_second_invocation() {
    let mut store = store();
    let mut transport = FakeExecutor::new(FakeScenario::ReplyLostOnce);
    let request = bod_app::adapter_conformance::conformance_request("inv-lost-51");

    let first = submit_start(&mut store, &mut transport, COMPANY, &request, NOW_MS)
        .expect("提交应返回结论而不是错误");
    match &first {
        StartVerdict::Unknown { .. } => {}
        other => panic!("丢回包必须进 Unknown，实际 {other:?}"),
    }
    assert_eq!(
        store
            .list_invocations(COMPANY)
            .expect("列 invocation")
            .len(),
        1,
        "回包丢失不得新建第二条 invocation"
    );

    // 对账：执行端能查回该执行时，状态不能停在 Unknown。
    let reconciled = reconcile_invocation(&mut store, &mut transport, "inv-lost-51", NOW_MS + 20)
        .expect("对账应成功")
        .expect("执行端应有该执行");
    assert_ne!(
        reconciled,
        bod_app::domain::execution::ExecutionState::Unknown,
        "对账后必须得到真实状态"
    );
    assert_eq!(
        store
            .list_invocations(COMPANY)
            .expect("列 invocation")
            .len(),
        1,
        "对账不得复制出第二条 invocation"
    );

    // 对账案件：同一逻辑动作键只保留一个 open 案件。
    let req = CaseRequest {
        company_id: COMPANY,
        case_id: "case-lost-51",
        invocation_id: Some("inv-lost-51"),
        logical_action_key: "dispatch:project-1:wp-7",
        impact_scope: "project-1",
        risk_reservation_refs: vec!["res-51".to_string()],
        known_evidence: "start 回包丢失，执行端可查",
        assignee: "staff-ops",
        next_step: "核对执行端输出后裁决",
    };
    let opened = open_case(&mut store, &req, NOW_MS + 30).expect("开案应成功");
    let again = open_case(&mut store, &req, NOW_MS + 40).expect("重复开案应返回原案");
    assert_eq!(
        opened.case_id, again.case_id,
        "同一逻辑动作键不应开出第二个案件"
    );
    assert_eq!(
        store
            .list_reconciliation_cases(COMPANY, true)
            .expect("列 open 案件")
            .len(),
        1
    );

    // 结案必须有裁决，且结案后不能再结一次。
    let resolved = resolve_case(
        &mut store,
        COMPANY,
        &opened.case_id,
        "执行端已产出候选，按原意图接受",
        NOW_MS + 50,
    )
    .expect("结案应成功");
    assert!(resolved.known_evidence.contains("裁决："));
    let err = resolve_case(
        &mut store,
        COMPANY,
        &opened.case_id,
        "再次裁决",
        NOW_MS + 60,
    )
    .expect_err("已结案不得再结");
    assert!(matches!(err, CaseError::CaseNotOpen(_)), "实际 {err:?}");
}

// ---------------------------------------------------------------------------
// 4. 崩溃中交接（G-39 / G-38）
// ---------------------------------------------------------------------------

/// 交接事务在中途崩溃：旧经理已撤、新经理未立。恢复后必须
/// ① 旧票证当场失效；② 新委任 epoch 严格递增、绝不复用；③ 新票证可用。
#[test]
fn case_crash_mid_handoff_never_reuses_epoch() {
    let store = store();
    let mut old = ManagementAppointment::appoint(&store, appoint_request("manager-a"), NOW_MS)
        .expect("首任应成功");
    let old_epoch = old.authority_epoch;
    assert_eq!(old_epoch, 1, "首个委任的 epoch 应为 1");
    let old_ticket = ticket(&old, ManagementCommand::ReadStatus);

    // 交接第一步：撤销旧经理。随后崩溃——还没有任命新经理。
    old.revoke(&store, "2026-11-20T12:05:00Z", old.version, NOW_MS + 1)
        .expect("撤任应成功");

    // 恢复后旧票证不能再用：撤任会递增 epoch，旧凭据连 epoch 都对不上头指针。
    let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
    let refused = gate
        .submit(
            &old_ticket,
            "cmd-old-51",
            "2026-11-20T12:06:00Z",
            NOW_MS + 2,
        )
        .expect_err("已撤任的票证必须被拒");
    match &refused {
        TransferError::Rejected { code, .. } => {
            // 实际命中 `stale_appointment` 而不是 `appointment_inactive`：撤任把 epoch 推进了，
            // 旧票证在 epoch 比对这一步就被拦下。这是更强的结论——即使状态检查被绕过，
            // 旧凭据也已经过期。
            assert_eq!(code.as_str(), "stale_appointment", "实际原因码 {code:?}");
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }

    // 新经理上任：epoch 必须严格递增，不能回落到 1。
    let new = ManagementAppointment::appoint(&store, appoint_request("manager-b"), NOW_MS + 3)
        .expect("新任应成功");
    assert!(
        new.authority_epoch > old_epoch,
        "新 epoch {} 必须大于旧 epoch {}",
        new.authority_epoch,
        old_epoch
    );
    assert_ne!(
        new.appointment_id, old.appointment_id,
        "换人不能复用同一条委任行"
    );

    // 新票证可用：交接完成后管理命令能正常落实。
    let new_ticket = ticket(&new, ManagementCommand::ReadStatus);
    let verified = gate
        .submit(
            &new_ticket,
            "cmd-new-51",
            "2026-11-20T12:07:00Z",
            NOW_MS + 4,
        )
        .expect("新经理的票证应可用");
    assert_eq!(verified.authority_epoch, new.authority_epoch);
    assert!(!verified.replayed, "首次提交不能被当成重放");
}

// ---------------------------------------------------------------------------
// 5. 旧经理并发切换（G-40 / G-41）
// ---------------------------------------------------------------------------

/// 两个经理争同一作用域：只允许一个主委任，第二次任命被拒，旧经理不被误杀。
#[test]
fn case_concurrent_manager_switch_keeps_exactly_one_owner() {
    let store = store();
    let first = ManagementAppointment::appoint(&store, appoint_request("manager-a"), NOW_MS)
        .expect("首任应成功");

    // 第二次任命（等价于并发切换中的后到者）必须被拒，并指出当前主委任。
    let err = ManagementAppointment::appoint(&store, appoint_request("manager-b"), NOW_MS + 1)
        .expect_err("同一作用域不得出现第二个主委任");
    match &err {
        TransferError::ScopeAlreadyManaged {
            current_appointment_id,
            authority_epoch,
        } => {
            assert_eq!(current_appointment_id, &first.appointment_id);
            assert_eq!(*authority_epoch, first.authority_epoch);
        }
        other => panic!("期望 ScopeAlreadyManaged，实际 {other:?}"),
    }

    // 旧经理没有被误杀：他的票证仍然可用。
    let mut gate = ManagementGate::new(&store, COMPANY, AUDIENCE);
    let verified = gate
        .submit(
            &ticket(&first, ManagementCommand::SubmitDelivery),
            "cmd-keep-51",
            "2026-11-20T12:03:00Z",
            NOW_MS + 2,
        )
        .expect("被拒的第二次任命不应影响当前主委任");
    assert_eq!(verified.authority_epoch, first.authority_epoch);

    // 同一管理命令重复提交是重放，不是第二次执行。
    let replay = gate
        .submit(
            &ticket(&first, ManagementCommand::SubmitDelivery),
            "cmd-keep-51",
            "2026-11-20T12:03:00Z",
            NOW_MS + 3,
        )
        .expect("同键同语义应返回原结论");
    assert!(replay.replayed, "同一 command_id 必须被识别为重放");
}

// ---------------------------------------------------------------------------
// 6. B 不可用（G-28）
// ---------------------------------------------------------------------------

/// 目标运行时不可用时，即使有已测组合证据也不能承接：能力门先于证据门。
#[test]
fn case_target_runtime_unavailable_yields_no_binding() {
    let descriptor = FakeExecutor::new(FakeScenario::HappyPath)
        .descriptor()
        .clone();
    // 目标运行时被 conformance 判为禁用（身份类失败），等价于"B 不可用"。
    let blocked = Enablement::Blocked {
        failures: vec![("scope-identity", "跨公司返回别家执行".to_string())],
    };
    let key = CombinationKey::new("runtime-b", &["git".to_string()], "coding");
    let tested = vec![TestedCombination {
        key: key.clone(),
        evidence_level: EvidenceLevel::RuntimeObserved,
        source: EvidenceSource::TestResult,
        observed_at_ms: NOW_MS,
        expires_at_ms: NOW_MS + 86_400_000,
        report_ref: "report:runtime-b-tested".to_string(),
    }];

    // 连 L1（不要求额外保证）都承接不了：被禁用的适配器不进候选。
    let refusal = admit_handoff(
        HandoffLevel::L1,
        &key,
        &tested,
        Candidate {
            descriptor: &descriptor,
            enablement: &blocked,
        },
        NOW_MS,
    )
    .expect_err("不可用的目标运行时不得承接任何等级");
    let HandoffRefusal::Capability(selection) = refusal else {
        panic!("应先被能力门拒绝，而不是缺证据：{refusal:?}");
    };
    assert!(!selection.has_candidate(), "被禁用的运行时不能出现在候选里");
    assert_eq!(selection.notice.code, "BOD_CAPABILITY_UNSATISFIED");
    assert!(
        selection.notice.message.contains("禁用"),
        "提示必须说明是禁用：「{}」",
        selection.notice.message
    );

    // 反证一：同一个适配器可用（未被禁用）时，L1 立即放行——说明上面的拒绝来自禁用，不是缺证据。
    let enabled = Enablement::Enabled;
    let admitted = admit_handoff(
        HandoffLevel::L1,
        &key,
        &tested,
        Candidate {
            descriptor: &descriptor,
            enablement: &enabled,
        },
        NOW_MS,
    )
    .expect("可用适配器承接 L1 不应需要额外证据");
    assert_eq!(admitted, HandoffLevel::L1);

    // 反证二：同一个可用适配器 + 同一份已测组合，L3 仍被能力门拒绝——
    // Fake 描述符声明不支持 recovery，证据再足也不能弥补能力缺失。
    let refusal = admit_handoff(
        HandoffLevel::L3,
        &key,
        &tested,
        Candidate {
            descriptor: &descriptor,
            enablement: &enabled,
        },
        NOW_MS,
    )
    .expect_err("声明不支持 recovery 的适配器不得承接 L3");
    let HandoffRefusal::Capability(selection) = refusal else {
        panic!("不能被判成缺证据：有已测组合时缺的只能是能力：{refusal:?}");
    };
    assert_eq!(selection.notice.code, "BOD_CAPABILITY_UNSATISFIED");
    assert!(
        selection
            .notice
            .missing
            .iter()
            .any(|item| item == "session_recovery"),
        "提示必须点名缺的是会话恢复：{:?}",
        selection.notice.missing
    );
}
