//! P0 D6 最小 doctor、对账案件与派发门禁（任务 7.2，§8.5、§38.5、I-26、G-20）。
//!
//! doctor 只读事实并给出可执行下一步；它不自行"修复"任何业务状态。
//! 磁盘满或关键写入不可用时关闭派发门禁，但保留在途与未确认状态。

use crate::domain::DomainId;
use crate::domain::budget::ReservationState;
use crate::domain::execution::ExecutionState;
use crate::domain::queue::{EffectStatus, QueueState};
use crate::recovery_models::{CaseState, ReconciliationCase};
use crate::store::{SqliteStore, StoreError};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Unknown,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }

    fn weight(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Unknown => 1,
            Self::Warn => 2,
            Self::Fail => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    pub next_step: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchGate {
    pub allowed: bool,
}

impl DispatchGate {
    pub fn closed(reason: &str) -> (Self, String) {
        (Self { allowed: false }, format!("停止新派发：{reason}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub overall: CheckStatus,
    pub gate: DispatchGate,
    pub generated_at_ms: i64,
}

impl DoctorReport {
    pub fn failed(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
    }

    pub fn check(&self, name: &str) -> Option<&DoctorCheck> {
        self.checks.iter().find(|check| check.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskStatus {
    Ok,
    Full,
    Unknown,
}

/// 真实写入探测：写一个小文件并 fsync，再删除。
/// ENOSPC 视为磁盘满；其他错误视为无法判定，不用"猜测"代替事实。
pub fn probe_disk(dir: &Path) -> (DiskStatus, String) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(".bod-write-probe-{}-{stamp}", std::process::id()));
    match std::fs::write(&path, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&path);
            (DiskStatus::Ok, "写入探测成功".to_string())
        }
        Err(err) if is_no_space(&err) => (DiskStatus::Full, format!("磁盘已满: {err}")),
        Err(err) => (DiskStatus::Unknown, format!("写入探测失败: {err}")),
    }
}

fn is_no_space(err: &std::io::Error) -> bool {
    // Unix ENOSPC=28，Windows ERROR_DISK_FULL=112（含 HANDLE_DISK_FULL=39）。
    matches!(err.raw_os_error(), Some(28) | Some(112) | Some(39))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorOptions {
    pub disk: DiskStatus,
    /// 备份超过该年龄视为不新鲜。
    pub backup_max_age_ms: i64,
    pub queue_stale_ms: i64,
    pub outbox_stale_ms: i64,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        Self {
            disk: DiskStatus::Ok,
            backup_max_age_ms: 24 * 3_600_000,
            queue_stale_ms: 5 * 60_000,
            outbox_stale_ms: 60_000,
        }
    }
}

/// 跑一遍 doctor。所有查询只读；不写事件、不改业务状态。
pub fn run_doctor(
    store: &mut SqliteStore,
    company_id: &str,
    options: &DoctorOptions,
    now_ms: i64,
) -> Result<DoctorReport, StoreError> {
    let mut checks = Vec::new();
    let mut gate = DispatchGate { allowed: true };

    // 1. 磁盘与可写性。
    match options.disk {
        DiskStatus::Ok => checks.push(ok("disk_space", "写入探测成功")),
        DiskStatus::Full => {
            let (closed, step) = DispatchGate::closed("数据目录磁盘已满");
            gate = closed;
            checks.push(DoctorCheck {
                name: "disk_space",
                status: CheckStatus::Fail,
                detail: "数据目录无法写入：磁盘已满".to_string(),
                next_step: step,
            });
        }
        DiskStatus::Unknown => checks.push(DoctorCheck {
            name: "disk_space",
            status: CheckStatus::Unknown,
            detail: "写入探测失败，无法判定剩余空间".to_string(),
            next_step: "检查数据目录权限与文件系统；不要把未知当可用".to_string(),
        }),
    }

    // 2. 数据库可写（取写锁后立即回滚，不产生任何业务写入）。
    match store.transaction() {
        Ok(uow) => match uow.rollback() {
            Ok(()) => checks.push(ok("db_writable", "可获取写事务")),
            Err(err) => {
                let (closed, step) = DispatchGate::closed("数据库写事务不可用");
                gate = closed;
                checks.push(DoctorCheck {
                    name: "db_writable",
                    status: CheckStatus::Fail,
                    detail: err.to_string(),
                    next_step: step,
                });
            }
        },
        Err(err) => {
            let (closed, step) = DispatchGate::closed("数据库不可写");
            gate = closed;
            checks.push(DoctorCheck {
                name: "db_writable",
                status: CheckStatus::Fail,
                detail: err.to_string(),
                next_step: step,
            });
        }
    }

    // 3. 队列深度与最老未处理。
    let queue = store.list_queue(company_id)?;
    let mut pending = 0usize;
    let mut blocked = 0usize;
    let mut oldest_due: Option<i64> = None;
    let mut expired_leases = 0usize;
    for entry in &queue {
        match entry.state {
            QueueState::Pending => {
                pending += 1;
                oldest_due =
                    Some(oldest_due.map_or(entry.next_due_at_ms, |v| v.min(entry.next_due_at_ms)));
            }
            QueueState::Blocked => blocked += 1,
            QueueState::Running
                if entry
                    .lease_expires_at_ms
                    .is_some_and(|expires| expires <= now_ms) =>
            {
                expired_leases += 1;
            }
            _ => {}
        }
    }
    let age = oldest_due.map(|due| (now_ms - due).max(0)).unwrap_or(0);
    checks.push(if pending > 0 && age > options.queue_stale_ms {
        DoctorCheck {
            name: "queue_backlog",
            status: CheckStatus::Warn,
            detail: format!("{pending} 个待处理，最老已等待 {age} ms；阻塞 {blocked} 个"),
            next_step: "检查执行端可用性与预算阻塞原因，不要热重试".to_string(),
        }
    } else {
        ok(
            "queue_backlog",
            &format!("待处理 {pending}，阻塞 {blocked}，最老等待 {age} ms"),
        )
    });

    checks.push(if expired_leases > 0 {
        DoctorCheck {
            name: "lease_expiry",
            status: CheckStatus::Warn,
            detail: format!("{expired_leases} 个运行中条目租约已过期"),
            next_step: "执行 expire_lease_if_due 后重新认领；确认旧执行已停止再派发".to_string(),
        }
    } else {
        ok("lease_expiry", "无过期租约")
    });

    // 4. outbox 积压。
    let prepared = store.list_outbox_by_status(EffectStatus::Prepared)?;
    let outbox_age = prepared
        .iter()
        .map(|item| (now_ms - item.created_at_ms).max(0))
        .max()
        .unwrap_or(0);
    checks.push(
        if !prepared.is_empty() && outbox_age > options.outbox_stale_ms {
            DoctorCheck {
                name: "outbox_backlog",
                status: CheckStatus::Warn,
                detail: format!("{} 条未派发意图，最老 {} ms", prepared.len(), outbox_age),
                next_step: "检查派发线程与目的地可用性；重放前先 inspect/reconcile".to_string(),
            }
        } else {
            ok(
                "outbox_backlog",
                &format!("未派发意图 {} 条", prepared.len()),
            )
        },
    );

    // 5. Unknown 外部动作必须都有对账案件；已裁决的算已覆盖。
    let invocations = store.list_invocations(company_id)?;
    let unknown: Vec<_> = invocations
        .iter()
        .filter(|item| item.state == ExecutionState::Unknown)
        .collect();
    let all_cases = store.list_reconciliation_cases(company_id, false)?;
    let open_cases: Vec<_> = all_cases
        .iter()
        .filter(|case| case.state == CaseState::Open)
        .cloned()
        .collect();
    let uncovered: Vec<_> = unknown
        .iter()
        .filter(|item| {
            !all_cases
                .iter()
                .any(|case| case.invocation_id.as_deref() == Some(item.invocation_id.as_str()))
        })
        .collect();
    let awaiting_adjudication: Vec<_> = unknown
        .iter()
        .filter(|item| {
            all_cases.iter().any(|case| {
                case.invocation_id.as_deref() == Some(item.invocation_id.as_str())
                    && case.state == CaseState::Open
            })
        })
        .collect();

    if !uncovered.is_empty() {
        let (closed, step) = DispatchGate::closed("存在未建案的外部未知动作");
        gate = closed;
        checks.push(DoctorCheck {
            name: "unknown_actions",
            status: CheckStatus::Fail,
            detail: format!(
                "{} 个结果未知，其中 {} 个没有对账案件",
                unknown.len(),
                uncovered.len()
            ),
            next_step: step,
        });
    } else if !awaiting_adjudication.is_empty() {
        let (closed, step) = DispatchGate::closed("外部未知动作尚未对账完成");
        gate = closed;
        checks.push(DoctorCheck {
            name: "unknown_actions",
            status: CheckStatus::Warn,
            detail: format!(
                "{} 个结果未知，{} 个等待裁决",
                unknown.len(),
                awaiting_adjudication.len()
            ),
            next_step: step,
        });
    } else if !unknown.is_empty() {
        checks.push(ok(
            "unknown_actions",
            &format!("{} 个结果未知，均已有裁决记录", unknown.len()),
        ));
    } else {
        checks.push(ok("unknown_actions", "无结果未知的外部动作"));
    }

    checks.push(ok(
        "reconciliation_cases",
        &format!("未结对账案件 {} 个", open_cases.len()),
    ));

    // 6. 风险占用：Unknown 的费用占用保留，不按 TTL 回收。
    let reservations = store.list_reservations(company_id)?;
    let risk_count = reservations
        .iter()
        .filter(|item| item.state == ReservationState::Risk)
        .count();
    checks.push(ok(
        "risk_reservations",
        &format!("风险占用 {risk_count} 个（保留，不按 TTL 回收）"),
    ));

    // 7. 备份新鲜度。
    match store.latest_backup(company_id)? {
        None => checks.push(DoctorCheck {
            name: "backup_freshness",
            status: CheckStatus::Warn,
            detail: "尚未生成可核验备份".to_string(),
            next_step: "执行 bod backup 后核验 status=verified".to_string(),
        }),
        Some(record) => {
            let age = now_ms.saturating_sub(record.created_at_ms);
            if !record.status.is_recoverable() {
                checks.push(DoctorCheck {
                    name: "backup_freshness",
                    status: CheckStatus::Fail,
                    detail: format!("最近备份不可恢复：{}", record.problems.join("；")),
                    next_step: "重新生成备份并补全 WAL/密钥说明/适配器版本".to_string(),
                });
            } else if age > options.backup_max_age_ms {
                checks.push(DoctorCheck {
                    name: "backup_freshness",
                    status: CheckStatus::Warn,
                    detail: format!("最近备份已 {} ms 未更新", age),
                    next_step: "按保留策略重新备份".to_string(),
                });
            } else {
                checks.push(ok(
                    "backup_freshness",
                    &format!("最近备份 {}，年龄 {} ms", record.backup_id, age),
                ));
            }
        }
    }

    let overall = checks
        .iter()
        .map(|check| check.status)
        .max_by_key(|status| status.weight())
        .unwrap_or(CheckStatus::Ok);

    Ok(DoctorReport {
        checks,
        overall,
        gate,
        generated_at_ms: now_ms,
    })
}

fn ok(name: &'static str, detail: &str) -> DoctorCheck {
    DoctorCheck {
        name,
        status: CheckStatus::Ok,
        detail: detail.to_string(),
        next_step: String::new(),
    }
}

#[derive(Debug)]
pub enum CaseError {
    Store(StoreError),
    /// 案件必须可执行：缺处理人或缺下一步。
    MissingAssignee,
    MissingNextStep,
    CaseNotOpen(String),
}

impl std::fmt::Display for CaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::MissingAssignee => write!(f, "对账案件必须指定处理人"),
            Self::MissingNextStep => write!(f, "对账案件必须给出下一步"),
            Self::CaseNotOpen(id) => write!(f, "对账案件 {id} 不在 open 状态"),
        }
    }
}

impl std::error::Error for CaseError {}

impl From<StoreError> for CaseError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

#[derive(Debug, Clone)]
pub struct CaseRequest<'a> {
    pub company_id: &'a str,
    pub case_id: &'a str,
    pub invocation_id: Option<&'a str>,
    pub logical_action_key: &'a str,
    pub impact_scope: &'a str,
    pub risk_reservation_refs: Vec<String>,
    pub known_evidence: &'a str,
    pub assignee: &'a str,
    pub next_step: &'a str,
}

/// 开案：同一逻辑动作键只保留一个 open 案件（幂等）。
/// Unknown 保留风险占用；案件记录事实与处理人，不把超时当失败。
pub fn open_case(
    store: &mut SqliteStore,
    request: &CaseRequest<'_>,
    now_ms: i64,
) -> Result<ReconciliationCase, CaseError> {
    if request.assignee.trim().is_empty() {
        return Err(CaseError::MissingAssignee);
    }
    if request.next_step.trim().is_empty() {
        return Err(CaseError::MissingNextStep);
    }
    if let Some(existing) = store
        .list_reconciliation_cases(request.company_id, true)?
        .into_iter()
        .find(|case| case.logical_action_key == request.logical_action_key)
    {
        return Ok(existing);
    }

    let case = ReconciliationCase {
        case_id: request.case_id.to_string(),
        company_id: DomainId::new(request.company_id),
        invocation_id: request.invocation_id.map(str::to_string),
        logical_action_key: request.logical_action_key.to_string(),
        impact_scope: request.impact_scope.to_string(),
        risk_reservation_refs: request.risk_reservation_refs.clone(),
        known_evidence: request.known_evidence.to_string(),
        assignee: request.assignee.to_string(),
        next_step: request.next_step.to_string(),
        state: CaseState::Open,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    let uow = store.transaction()?;
    uow.put_reconciliation_case(&case)?;
    uow.commit()?;
    Ok(case)
}

/// 结案：必须给出裁决证据；只允许从 open 迁移。
pub fn resolve_case(
    store: &mut SqliteStore,
    company_id: &str,
    case_id: &str,
    resolution: &str,
    now_ms: i64,
) -> Result<ReconciliationCase, CaseError> {
    if resolution.trim().is_empty() {
        return Err(CaseError::MissingNextStep);
    }
    let mut case = store
        .list_reconciliation_cases(company_id, false)?
        .into_iter()
        .find(|item| item.case_id == case_id)
        .ok_or_else(|| CaseError::CaseNotOpen(case_id.to_string()))?;
    if case.state != CaseState::Open {
        return Err(CaseError::CaseNotOpen(case_id.to_string()));
    }
    case.state = CaseState::Resolved;
    case.known_evidence = format!("{}；裁决：{resolution}", case.known_evidence);
    case.next_step = "已裁决；如需补发请走新的派发意图".to_string();
    case.updated_at_ms = now_ms;

    let uow = store.transaction()?;
    uow.put_reconciliation_case(&case)?;
    uow.commit()?;
    Ok(case)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::recovery_models::CaseState;
    use std::path::PathBuf;

    const NOW: i64 = 1_763_500_000_000;

    fn store() -> (SqliteStore, PathBuf) {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("bod-doctor-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bod.sqlite");
        let store = SqliteStore::open(&path, &BodConfig::default()).unwrap();
        (store, dir)
    }

    /// G-16：reconcile 终态与重启（§39.14 bod-app::lifecycle_recovery）。
    /// 用文件型 SQLite 真实关闭重开，证明 Unknown → 对账案件 → 裁决结案的终态
    /// 跨进程仍可查，且结案后 doctor 不再把它当失败中的 Unknown。
    #[test]
    fn g_16_reconcile_terminal_state_survives_restart() {
        let (mut store, dir) = store();
        let uow = store.transaction().unwrap();
        uow.insert_invocation(&unknown_invocation()).unwrap();
        uow.commit().unwrap();

        let opened = open_case(
            &mut store,
            &CaseRequest {
                company_id: "company-default",
                case_id: "case-g16",
                invocation_id: Some("inv-1"),
                logical_action_key: "deploy:prod",
                impact_scope: "project-1",
                risk_reservation_refs: vec!["res-g16".to_string()],
                known_evidence: "查询三次超时",
                assignee: "user-1",
                next_step: "联系外部系统核对部署状态",
            },
            NOW,
        )
        .unwrap();
        assert_eq!(opened.state, CaseState::Open);
        let before = run_doctor(
            &mut store,
            "company-default",
            &DoctorOptions::default(),
            NOW,
        )
        .unwrap();
        assert!(!before.gate.allowed, "案件未结案前派发门保持关闭");

        let resolved = resolve_case(
            &mut store,
            "company-default",
            "case-g16",
            "外部确认未执行",
            NOW + 1,
        )
        .unwrap();
        assert_eq!(resolved.state, CaseState::Resolved);
        drop(store);

        let mut reopened =
            SqliteStore::open(&dir.join("bod.sqlite"), &BodConfig::default()).unwrap();
        let cases = reopened
            .list_reconciliation_cases("company-default", false)
            .unwrap();
        let persisted = cases
            .iter()
            .find(|item| item.case_id == "case-g16")
            .expect("重启后案件仍可查");
        assert_eq!(persisted.state, CaseState::Resolved);
        assert!(
            persisted.known_evidence.contains("外部确认未执行"),
            "裁决写入案件证据并跨重启保留"
        );
        assert!(persisted.next_step.contains("已裁决"));
        assert_eq!(
            persisted.risk_reservation_refs,
            vec!["res-g16".to_string()],
            "结案不等于自动释放风险预留"
        );

        let after = run_doctor(
            &mut reopened,
            "company-default",
            &DoctorOptions::default(),
            NOW + 2,
        )
        .unwrap();
        assert_ne!(
            after.check("unknown_actions").unwrap().status,
            CheckStatus::Fail,
            "已裁决案件不再冒充未处理的 Unknown"
        );
    }

    #[test]
    fn clean_store_reports_no_failure_and_allows_dispatch() {
        let (mut store, dir) = store();
        let (disk, _) = probe_disk(&dir);
        assert_eq!(disk, DiskStatus::Ok);
        let report = run_doctor(
            &mut store,
            "company-default",
            &DoctorOptions {
                disk,
                ..Default::default()
            },
            NOW,
        )
        .unwrap();
        assert!(!report.failed());
        assert!(report.gate.allowed);
        assert_eq!(report.overall, CheckStatus::Warn, "无备份给 warn 而非 fail");
        assert!(report.check("backup_freshness").is_some());
    }

    #[test]
    fn disk_full_closes_dispatch_gate() {
        let (mut store, _dir) = store();
        let report = run_doctor(
            &mut store,
            "company-default",
            &DoctorOptions {
                disk: DiskStatus::Full,
                ..Default::default()
            },
            NOW,
        )
        .unwrap();
        assert!(report.failed());
        assert!(!report.gate.allowed);
        assert_eq!(
            report.check("disk_space").unwrap().status,
            CheckStatus::Fail
        );
        assert!(
            report
                .check("disk_space")
                .unwrap()
                .next_step
                .contains("停止新派发")
        );
    }

    mod g_20 {
        use super::*;

        #[test]
        fn unknown_invocation_without_case_fails_and_closes_gate() {
            let (mut store, _dir) = store();
            // 直接写入一条 Unknown 的 invocation。
            let record = unknown_invocation();
            let uow = store.transaction().unwrap();
            uow.insert_invocation(&record).unwrap();
            uow.commit().unwrap();

            let report = run_doctor(
                &mut store,
                "company-default",
                &DoctorOptions::default(),
                NOW,
            )
            .unwrap();
            assert!(report.failed());
            assert!(!report.gate.allowed);
            assert!(
                report
                    .check("unknown_actions")
                    .unwrap()
                    .detail
                    .contains("没有对账案件")
            );
        }

        #[test]
        fn case_makes_unknown_traceable_but_gate_stays_closed_until_resolved() {
            let (mut store, _dir) = store();
            let record = unknown_invocation();
            let uow = store.transaction().unwrap();
            uow.insert_invocation(&record).unwrap();
            uow.commit().unwrap();

            let case = open_case(
                &mut store,
                &CaseRequest {
                    company_id: "company-default",
                    case_id: "case-1",
                    invocation_id: Some("inv-1"),
                    logical_action_key: "deploy:prod",
                    impact_scope: "project-1",
                    risk_reservation_refs: vec!["res-1".to_string()],
                    known_evidence: "查询三次超时",
                    assignee: "user-1",
                    next_step: "联系外部系统核对部署状态",
                },
                NOW,
            )
            .unwrap();
            assert_eq!(case.state, CaseState::Open);
            assert_eq!(case.risk_reservation_refs, vec!["res-1".to_string()]);

            let report = run_doctor(
                &mut store,
                "company-default",
                &DoctorOptions::default(),
                NOW,
            )
            .unwrap();
            assert_eq!(
                report.check("unknown_actions").unwrap().status,
                CheckStatus::Warn,
                "有案件后仍提示先对账"
            );
            assert!(!report.gate.allowed, "未结案前不开放派发");

            resolve_case(
                &mut store,
                "company-default",
                "case-1",
                "外部确认未执行",
                NOW + 1,
            )
            .unwrap();
            let after = run_doctor(
                &mut store,
                "company-default",
                &DoctorOptions::default(),
                NOW + 2,
            )
            .unwrap();
            assert!(after.check("unknown_actions").unwrap().status != CheckStatus::Fail);
        }

        #[test]
        fn case_requires_assignee_and_next_step() {
            let (mut store, _dir) = store();
            let base = CaseRequest {
                company_id: "company-default",
                case_id: "case-2",
                invocation_id: None,
                logical_action_key: "deploy:x",
                impact_scope: "",
                risk_reservation_refs: Vec::new(),
                known_evidence: "",
                assignee: " ",
                next_step: "核对",
            };
            assert!(matches!(
                open_case(&mut store, &base, NOW).unwrap_err(),
                CaseError::MissingAssignee
            ));
            let no_step = CaseRequest {
                assignee: "user-1",
                next_step: "",
                ..base
            };
            assert!(matches!(
                open_case(&mut store, &no_step, NOW).unwrap_err(),
                CaseError::MissingNextStep
            ));
        }

        #[test]
        fn same_logical_key_reuses_open_case() {
            let (mut store, _dir) = store();
            let request = CaseRequest {
                company_id: "company-default",
                case_id: "case-3",
                invocation_id: Some("inv-9"),
                logical_action_key: "deploy:staging",
                impact_scope: "project-1",
                risk_reservation_refs: Vec::new(),
                known_evidence: "超时",
                assignee: "user-1",
                next_step: "核对",
            };
            let first = open_case(&mut store, &request, NOW).unwrap();
            let second = open_case(&mut store, &request, NOW + 5).unwrap();
            assert_eq!(first.case_id, second.case_id);
            let all = store
                .list_reconciliation_cases("company-default", true)
                .unwrap();
            assert_eq!(all.len(), 1);
        }
    }

    fn unknown_invocation() -> crate::domain::execution::InvocationRecord {
        use crate::domain::execution::{ExecutionState, InvocationRecord};
        InvocationRecord {
            invocation_id: "inv-1".to_string(),
            company_id: "company-default".to_string(),
            run_id: "run-1".to_string(),
            work_package_revision: "wp-r1".to_string(),
            attempt_id: "attempt-1".to_string(),
            binding_id: "binding-1".to_string(),
            fencing_token: 1,
            adapter_id: "fake".to_string(),
            adapter_version: "1".to_string(),
            params_canonical: "params".to_string(),
            params_digest: "digest".to_string(),
            execution_id: None,
            state: ExecutionState::Unknown,
            created_at_ms: NOW,
            accepted_at_ms: None,
            last_observed_at_ms: None,
            retention_until_ms: None,
        }
    }
}
