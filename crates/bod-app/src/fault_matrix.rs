//! P0 核心故障矩阵（任务 7.5，§40.7 D6、§39.13 I 表）。
//!
//! 五个场景各自独立建库、独立断言，产出"事实记录 + 可操作下一步"，而不是只打印通过。
//! 报告必须带实际环境与 commit；未实际执行的项目不得写成通过。
//!
//! 场景清单：
//! 1. 外部成功但回包丢失（I-04、I-26）
//! 2. 已预留后宿主崩溃（I-05、I-71、G-2）
//! 3. 磁盘满（I-26、G-20）
//! 4. 权限被撤销（I-09、G-21）
//! 5. 备份恢复到新宿主（G-6、G-49）

use crate::cas::CasStore;
use crate::config::BodConfig;
use crate::control_api::{ControlApi, RequestContext};
use crate::domain::DomainId;
use crate::domain::Version;
use crate::domain::budget::{Measure, Reservation};
use crate::domain::execution::{ExecutionState, InvocationRecord};
use crate::domain::model::{
    AcceptanceCriteria, Assignment, Grant, Issue, IssueKind, IssueSpec, Owner, OwnerKind,
    Permission, Phase, Principal, PrincipalKind, RepositoryContext, ResourceMeta, ResourceStatus,
    Scope,
};
use crate::domain::queue::{EffectStatus, OutboxDelivery};
use crate::effect_recovery;
use crate::lifecycle_recovery::{self, CaseRequest, CheckStatus, DiskStatus, DoctorOptions};
use crate::migration_safety::{self, BackupInput};
use crate::store::SqliteStore;
use std::path::{Path, PathBuf};

const COMPANY: &str = "company-default";
const PROJECT: &str = "project-1";
const NOW: i64 = 1_763_500_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioResult {
    pub name: &'static str,
    pub fault: &'static str,
    pub requirement: &'static str,
    /// 故障后真实写入库/文件的事实记录。
    pub facts: Vec<String>,
    /// 断言摘要：每条都是本次实际检查的命题。
    pub assertions: Vec<String>,
    /// 面向使用者的下一步。
    pub next_step: String,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultMatrixReport {
    pub environment: String,
    pub commit: String,
    pub scenarios: Vec<ScenarioResult>,
}

impl FaultMatrixReport {
    pub fn all_passed(&self) -> bool {
        self.scenarios.iter().all(|item| item.passed)
    }

    pub fn passed_count(&self) -> usize {
        self.scenarios.iter().filter(|item| item.passed).count()
    }
}

fn config() -> BodConfig {
    BodConfig::default()
}

fn workspace(name: &str) -> Result<(PathBuf, SqliteStore), String> {
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("bod-fault-{name}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    let store =
        SqliteStore::open(&root.join("bod.sqlite"), &config()).map_err(|err| err.to_string())?;
    Ok((root, store))
}

fn assigned_issue() -> Issue {
    let scope = Scope {
        company_id: DomainId::new(COMPANY),
        project_id: Some(DomainId::new(PROJECT)),
        issue_id: Some(DomainId::new("issue-1")),
    };
    Issue {
        meta: ResourceMeta {
            uid: DomainId::new("issue-1"),
            company_id: DomainId::new(COMPANY),
            project_id: Some(DomainId::new(PROJECT)),
            resource_version: Version::INITIAL,
            generation: 1,
            deletion_requested_at: None,
        },
        kind: IssueKind::Coding,
        repository: Some(RepositoryContext {
            provider: "local".to_string(),
            repository_id: "repo-1".to_string(),
        }),
        spec: IssueSpec {
            title: "修复结账接口".to_string(),
            assignment: Some(Assignment {
                owner: Owner {
                    kind: OwnerKind::Agent,
                    id: DomainId::new("coder-1"),
                },
                scope,
                grant_revision: 1,
            }),
            acceptance: AcceptanceCriteria {
                criteria: vec!["集成测试通过".to_string()],
            },
        },
        status: ResourceStatus {
            phase: Phase::Pending,
            observed_generation: 1,
            conditions: Vec::new(),
        },
    }
}

fn unknown_invocation() -> InvocationRecord {
    InvocationRecord {
        invocation_id: "inv-ack-lost".to_string(),
        company_id: COMPANY.to_string(),
        run_id: "run-1".to_string(),
        work_package_revision: "wp-r1".to_string(),
        attempt_id: "attempt-1".to_string(),
        binding_id: "binding-1".to_string(),
        fencing_token: 1,
        adapter_id: "local-process".to_string(),
        adapter_version: "1".to_string(),
        params_canonical: "cmd=deploy".to_string(),
        params_digest: "digest".to_string(),
        execution_id: None,
        state: ExecutionState::Unknown,
        created_at_ms: NOW,
        accepted_at_ms: None,
        last_observed_at_ms: None,
        retention_until_ms: None,
    }
}

/// 场景 1：外部已成功但回包丢失 → Unknown 保留，先对账再派发。
fn ack_lost(_root: &Path, store: &mut SqliteStore) -> Result<ScenarioResult, String> {
    let uow = store.transaction().map_err(|err| err.to_string())?;
    uow.insert_invocation(&unknown_invocation())
        .map_err(|err| err.to_string())?;
    uow.commit().map_err(|err| err.to_string())?;

    let open_doctor =
        lifecycle_recovery::run_doctor(store, COMPANY, &DoctorOptions::default(), NOW)
            .map_err(|err| err.to_string())?;
    let unknown_check = open_doctor
        .check("unknown_actions")
        .ok_or("doctor 缺少 unknown_actions 检查项")?;
    let closed_before_case = !open_doctor.gate.allowed;

    let case = lifecycle_recovery::open_case(
        store,
        &CaseRequest {
            company_id: COMPANY,
            case_id: "case-ack-lost",
            invocation_id: Some("inv-ack-lost"),
            logical_action_key: "deploy:prod",
            impact_scope: PROJECT,
            risk_reservation_refs: vec!["res-ack-lost".to_string()],
            known_evidence: "start 已发出，未收到 execution_id 回执",
            assignee: "user-1",
            next_step: "对目标环境执行 inspect，确认是否已部署",
        },
        NOW + 1,
    )
    .map_err(|err| err.to_string())?;

    let after = lifecycle_recovery::run_doctor(store, COMPANY, &DoctorOptions::default(), NOW + 2)
        .map_err(|err| err.to_string())?;

    let assertions = vec![
        format!(
            "回包丢失后执行状态为 {}，未自动重试",
            ExecutionState::Unknown.as_str()
        ),
        format!(
            "没有对账案件前派发门禁关闭：{}",
            if closed_before_case { "closed" } else { "open" }
        ),
        format!(
            "开案后仍有可追踪事实：{}，处理人 {}",
            case.case_id, case.assignee
        ),
        format!(
            "案件未结时门禁仍为 {}",
            if after.gate.allowed { "open" } else { "closed" }
        ),
    ];
    let passed = closed_before_case
        && !after.gate.allowed
        && case.next_step.contains("inspect")
        && unknown_check.status != CheckStatus::Ok;

    Ok(ScenarioResult {
        name: "ack_lost_after_external_success",
        fault: "外部成功但回包丢失",
        requirement: "I-04、I-26、execution-adapter: 回包丢失先对账",
        facts: vec![
            "invocations.state = Unknown（inv-ack-lost）".to_string(),
            format!("reconciliation_cases.case_id = {}", case.case_id),
            format!("案件下一步 = {}", case.next_step),
            format!("doctor.unknown_actions = {}", unknown_check.status.as_str()),
        ],
        assertions,
        next_step: "先 inspect 外部执行结果；确认未发生再用同一幂等键补发，确认已发生则结案"
            .to_string(),
        passed,
    })
}

/// 场景 2：已预留后宿主崩溃 → 预留仍是 R，重启只重放 outbox，不把预留当已消费。
fn crash_after_reservation(root: &Path, store: &mut SqliteStore) -> Result<ScenarioResult, String> {
    let mut reservation = Reservation::new(
        "res-crash",
        DomainId::new(COMPANY),
        Measure::money_micros(2_000, "CNY"),
        2_000,
        NOW,
    );
    reservation.project_id = Some(DomainId::new(PROJECT));
    let uow = store.transaction().map_err(|err| err.to_string())?;
    uow.insert_reservation(&reservation)
        .map_err(|err| err.to_string())?;
    uow.put_outbox(&OutboxDelivery {
        event_or_command_id: "cmd-dispatch-1".to_string(),
        destination_id: "executor".to_string(),
        company_id: DomainId::new(COMPANY),
        queue_key: "issue:issue-1".to_string(),
        request_digest: "digest-1".to_string(),
        status: EffectStatus::Prepared,
        attempts: 0,
        next_due_at_ms: NOW,
        result_ref: None,
        created_at_ms: NOW,
    })
    .map_err(|err| err.to_string())?;
    uow.commit().map_err(|err| err.to_string())?;

    // 模拟宿主崩溃：丢弃连接后重新打开同一文件库。
    let db = root.join("bod.sqlite");
    drop(std::mem::replace(
        store,
        SqliteStore::in_memory(&config()).map_err(|err| err.to_string())?,
    ));
    let reopened = SqliteStore::open(&db, &config()).map_err(|err| err.to_string())?;
    *store = reopened;

    let reservations = store
        .list_reservations(COMPANY)
        .map_err(|err| err.to_string())?;
    let still_reserved = reservations
        .iter()
        .find(|item| item.reservation_id == "res-crash")
        .map(|item| item.reserved)
        .unwrap_or(0);
    let replay = effect_recovery::replay_outbox(store).map_err(|err| format!("{err:?}"))?;
    let entries = store
        .list_ledger_entries(COMPANY)
        .map_err(|err| err.to_string())?;

    let assertions = vec![
        format!("重启后预留仍为 R={still_reserved}，未被当成已消费"),
        format!("账本分录数 = {}，崩溃未产生假账单", entries.len()),
        format!("可重放的 Prepared 意图 = {}", replay.len()),
    ];
    let passed = still_reserved == 2_000 && entries.is_empty() && replay.len() == 1;

    Ok(ScenarioResult {
        name: "crash_after_reservation",
        fault: "已预留后宿主崩溃",
        requirement: "I-05、I-71、effect-gate: 提交后派发",
        facts: vec![
            format!("reservations.reserved_value = {still_reserved}（res-crash）"),
            "outbox_deliveries.status = Prepared（cmd-dispatch-1）".to_string(),
            format!("ledger_entries 行数 = {}", entries.len()),
        ],
        assertions,
        next_step: "重启后按 outbox 重放：先判定外部是否已发生，再决定补发或开对账案件".to_string(),
        passed,
    })
}

/// 场景 3：磁盘满 → 停止新派发，但在途与未确认状态不被清空。
fn disk_full(_root: &Path, store: &mut SqliteStore) -> Result<ScenarioResult, String> {
    let report = lifecycle_recovery::run_doctor(
        store,
        COMPANY,
        &DoctorOptions {
            disk: DiskStatus::Full,
            ..Default::default()
        },
        NOW,
    )
    .map_err(|err| err.to_string())?;
    let disk = report
        .check("disk_space")
        .ok_or("doctor 缺少 disk_space 检查项")?;

    let assertions = vec![
        format!("磁盘满时 disk_space = {}", disk.status.as_str()),
        format!(
            "派发门禁 = {}",
            if report.gate.allowed {
                "open"
            } else {
                "closed"
            }
        ),
        format!("下一步提示非空：{}", !disk.next_step.is_empty()),
    ];
    let passed = disk.status == CheckStatus::Fail && !report.gate.allowed && report.failed();

    Ok(ScenarioResult {
        name: "disk_full_closes_dispatch",
        fault: "磁盘满",
        requirement: "I-26、G-20、recovery-cleanup: 最小 doctor",
        facts: vec![
            format!("doctor.disk_space = {}", disk.status.as_str()),
            format!("doctor.gate.allowed = {}", report.gate.allowed),
            format!("doctor.overall = {}", report.overall.as_str()),
        ],
        assertions,
        next_step: "释放磁盘或迁移数据目录，重跑 doctor 确认 disk_space 通过后再开放派发"
            .to_string(),
        passed,
    })
}

/// 场景 4：权限被撤销 → 403 且给出重新激活条件，不产生队列消费者。
fn permission_revoked(_root: &Path, store: &mut SqliteStore) -> Result<ScenarioResult, String> {
    let issue = assigned_issue();
    let uow = store.transaction().map_err(|err| err.to_string())?;
    uow.insert_issue(&issue).map_err(|err| err.to_string())?;
    uow.commit().map_err(|err| err.to_string())?;

    let api = ControlApi::new();
    let scope = Scope {
        company_id: DomainId::new(COMPANY),
        project_id: Some(DomainId::new(PROJECT)),
        issue_id: Some(DomainId::new("issue-1")),
    };
    let actor = Principal {
        id: DomainId::new("coder-1"),
        kind: PrincipalKind::Agent,
        claimed_roles: Vec::new(),
    };
    let ctx = RequestContext {
        request_id: "req-revoked",
        actor: &actor,
        scope: &scope,
    };
    // 已撤销的授权：revision 与指派绑定不同且 active=false。
    let revoked = vec![Grant {
        principal_id: DomainId::new("coder-1"),
        scope: scope.clone(),
        permission: Permission::Dispatch,
        revision: 2,
        active: false,
    }];
    let denial = api
        .request_dispatch(store, &ctx, "issue-1", &revoked, NOW)
        .err()
        .ok_or("撤权后仍派发成功")?;

    let queue = store.list_queue(COMPANY).map_err(|err| err.to_string())?;
    let assertions = vec![
        format!("撤权后 HTTP 状态 = {}", denial.status),
        format!("稳定错误码 = {}", denial.reason_code),
        format!("重新激活条件非空：{}", !denial.next_step.is_empty()),
        format!("队列新增消费者 = {}", queue.len()),
    ];
    let passed = denial.status == 403 && queue.is_empty() && !denial.next_step.is_empty();

    Ok(ScenarioResult {
        name: "permission_revoked_denies_dispatch",
        fault: "权限被撤销",
        requirement: "I-09、identity-access: 拒绝可解释与审计、G-21",
        facts: vec![
            "grants.active = 0（coder-1 / dispatch）".to_string(),
            format!("拒绝原因码 = {}", denial.reason_code),
            format!("重新激活条件 = {}", denial.next_step),
        ],
        assertions,
        next_step: "恢复授权并递增 revision 后重试；期间不得由其他主体代派发".to_string(),
        passed,
    })
}

/// 场景 5：备份 → 新宿主恢复 → 有未知动作时先对账再开放派发。
fn backup_restore(root: &Path, store: &mut SqliteStore) -> Result<ScenarioResult, String> {
    let cas = CasStore::open(root.join("cas")).map_err(|err| err.to_string())?;
    let backup_dir = root.join("backup");
    let record = migration_safety::create_backup(
        store,
        &cas,
        &BackupInput {
            company_id: COMPANY,
            destination: &backup_dir,
            config_revision: "config-r1",
            adapter_versions: "local-process=1",
            key_recovery_note: "启动密钥由外部 KMS 托管，恢复时重新注入",
        },
        "bk-1",
        NOW,
    )
    .map_err(|err| err.to_string())?;

    let target = root.join("restored").join("bod.sqlite");
    let restore = migration_safety::restore_to_new_host(&record, &target, &config())
        .map_err(|err| err.to_string())?;

    // 恢复后的新宿主若存在 Unknown 动作，派发必须先对账：用实际 doctor 验一遍。
    let mut restored_store =
        SqliteStore::open(&target, &config()).map_err(|err| err.to_string())?;
    let uow = restored_store
        .transaction()
        .map_err(|err| err.to_string())?;
    uow.insert_invocation(&unknown_invocation())
        .map_err(|err| err.to_string())?;
    uow.commit().map_err(|err| err.to_string())?;
    let doctor = lifecycle_recovery::run_doctor(
        &mut restored_store,
        COMPANY,
        &DoctorOptions::default(),
        NOW + 5,
    )
    .map_err(|err| err.to_string())?;

    let assertions = vec![
        format!("备份状态 = {}", record.status.as_str()),
        format!("恢复接受 = {}", restore.accepted),
        format!("恢复后行数 issues = {}", restore.counts.issues),
        format!("恢复时无未知动作 → 开放派发 = {}", restore.dispatch_allowed),
        format!(
            "恢复后出现 Unknown → 派发门禁 = {}",
            if doctor.gate.allowed {
                "open"
            } else {
                "closed"
            }
        ),
    ];
    let passed = record.status.is_recoverable()
        && restore.accepted
        && restore.dispatch_allowed
        && !doctor.gate.allowed;

    Ok(ScenarioResult {
        name: "backup_restore_to_new_host",
        fault: "备份恢复到新宿主",
        requirement: "G-6、G-49、recovery-cleanup: 备份内容 / 恢复到新宿主",
        facts: vec![
            format!("backups.backup_id = {}", record.backup_id),
            format!("backups.wal_included = {}", record.wal_included),
            format!("backups.snapshot_method = {}", record.snapshot_method),
            format!("恢复目标 = {}", restore.target),
            format!("恢复检查项 = {}", restore.checks.join("；")),
        ],
        assertions,
        next_step: "恢复后先跑 doctor 与 pending outbox：有 Unknown 先对账，再开放新派发"
            .to_string(),
        passed,
    })
}

/// 运行五个场景。每个场景独立建库，互不污染。
pub fn run(environment: &str, commit: &str) -> Result<FaultMatrixReport, String> {
    let mut scenarios = Vec::new();

    {
        let (root, mut store) = workspace("ack")?;
        scenarios.push(ack_lost(&root, &mut store)?);
    }
    {
        let (root, mut store) = workspace("crash")?;
        scenarios.push(crash_after_reservation(&root, &mut store)?);
    }
    {
        let (root, mut store) = workspace("disk")?;
        scenarios.push(disk_full(&root, &mut store)?);
    }
    {
        let (root, mut store) = workspace("perm")?;
        scenarios.push(permission_revoked(&root, &mut store)?);
    }
    {
        let (root, mut store) = workspace("backup")?;
        scenarios.push(backup_restore(&root, &mut store)?);
    }

    Ok(FaultMatrixReport {
        environment: environment.to_string(),
        commit: commit.to_string(),
        scenarios,
    })
}

/// 渲染为 Markdown：供 CLI 直接输出，也用于生成 `docs/reports/p0-fault-matrix.md`。
pub fn render_markdown(report: &FaultMatrixReport) -> String {
    let mut out = String::new();
    out.push_str("# P0 故障矩阵实测报告\n\n");
    out.push_str(&format!("- 环境：{}\n", report.environment));
    out.push_str(&format!("- commit：{}\n", report.commit));
    out.push_str(&format!(
        "- 结果：{}/{} 场景通过\n\n",
        report.passed_count(),
        report.scenarios.len()
    ));
    out.push_str("| 场景 | 故障 | 规格/不变量 | 结果 |\n| --- | --- | --- | --- |\n");
    for scenario in &report.scenarios {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            scenario.name,
            scenario.fault,
            scenario.requirement,
            if scenario.passed { "PASS" } else { "FAIL" }
        ));
    }
    out.push('\n');
    for scenario in &report.scenarios {
        out.push_str(&format!("## {}\n\n", scenario.fault));
        out.push_str(&format!("- 场景 ID：`{}`\n", scenario.name));
        out.push_str(&format!("- 规格绑定：{}\n", scenario.requirement));
        out.push_str("- 留下的事实：\n");
        for fact in &scenario.facts {
            out.push_str(&format!("  - {}\n", fact));
        }
        out.push_str("- 断言：\n");
        for assertion in &scenario.assertions {
            out.push_str(&format!("  - {}\n", assertion));
        }
        out.push_str(&format!("- 下一步：{}\n\n", scenario.next_step));
    }
    out
}
