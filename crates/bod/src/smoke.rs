//! P0 D5 最小全流程冒烟（任务 6.6）。
//!
//! 路径：Issue 指派 → Fake Executor 启动 → 候选成果登记 → 只读查询 → 宿主重启找回。
//! 断言候选成果不因执行端 Completed 被标记为 Accepted，且重启后验收状态仍是 Pending。

use bod_app::artifact_acceptance::{ensure_artifact, submit_candidate};
use bod_app::cas::CasStore;
use bod_app::config::BodConfig;
use bod_app::control_api::{ControlApi, RequestContext};
use bod_app::domain::artifact::{Artifact, ArtifactKind, ArtifactVisibility};
use bod_app::domain::execution::StartRequest;
use bod_app::domain::model::{
    AcceptanceCriteria, Assignment, Issue, IssueKind, IssueSpec, Owner, OwnerKind, Principal,
    PrincipalKind, RepositoryContext, ResourceMeta, ResourceStatus, Scope,
};
use bod_app::domain::{DomainId, Phase, Version};
use bod_app::fake_executor::{FakeExecutor, FakeScenario};
use bod_app::invocation::{StartVerdict, submit_start};
use bod_app::json_text::json_string;
use bod_app::store::SqliteStore;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeReport {
    pub work_dir: String,
    pub issue: String,
    pub invocation: String,
    pub execution_id: String,
    pub candidate_revision: String,
    pub verification_state: String,
    pub accepted: bool,
    pub resource_version_before: u64,
    pub resource_version_after: u64,
    pub read_only_dispatches: usize,
    pub restart_recovered: bool,
    pub result: &'static str,
    pub notes: Vec<String>,
}

impl SmokeReport {
    pub fn to_json(&self) -> String {
        let notes = self
            .notes
            .iter()
            .map(|note| json_string(note))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"smoke\":\"p0-d5\",\"work_dir\":{},\"issue\":{},\"invocation\":{},\
             \"execution_id\":{},\"candidate_revision\":{},\"verification_state\":{},\
             \"accepted\":{},\"resource_version_before\":{},\"resource_version_after\":{},\
             \"read_only_dispatches\":{},\"restart_recovered\":{},\"result\":{},\"notes\":[{}]}}",
            json_string(&self.work_dir),
            json_string(&self.issue),
            json_string(&self.invocation),
            json_string(&self.execution_id),
            json_string(&self.candidate_revision),
            json_string(&self.verification_state),
            self.accepted,
            self.resource_version_before,
            self.resource_version_after,
            self.read_only_dispatches,
            self.restart_recovered,
            json_string(self.result),
            notes
        )
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn id(value: &str) -> DomainId {
    DomainId::new(value)
}

fn company() -> DomainId {
    id("company-default")
}

fn issue_scope() -> Scope {
    Scope {
        company_id: company(),
        project_id: Some(id("project-1")),
        issue_id: Some(id("issue-1")),
    }
}

fn issue() -> Issue {
    Issue {
        meta: ResourceMeta {
            uid: id("issue-1"),
            company_id: company(),
            project_id: Some(id("project-1")),
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
            title: "冒烟：实现登录".to_string(),
            assignment: Some(Assignment {
                owner: Owner {
                    kind: OwnerKind::Agent,
                    id: id("coder-1"),
                },
                scope: issue_scope(),
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

fn start_request() -> StartRequest {
    StartRequest {
        invocation_id: "inv-smoke-1".to_string(),
        run_id: "run-1".to_string(),
        work_package_revision: "wp-r1".to_string(),
        attempt_id: "attempt-1".to_string(),
        binding_id: "binding-1".to_string(),
        fencing_token: 1,
        agent_ref: "agent:coder-1".to_string(),
        runtime_config_revision: "cfg-r1".to_string(),
        context_bundle_ref: "ctx-r1".to_string(),
        workspace_ref: "ws-r1".to_string(),
        grant_ref: "grant-r1".to_string(),
        reservation_refs: vec!["res-1".to_string()],
        deadline_at_ms: None,
    }
}

/// 运行一次冒烟；失败返回可读原因，不伪造通过。
pub fn run() -> Result<SmokeReport, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let work_dir = std::env::temp_dir().join(format!("bod-smoke-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&work_dir).map_err(|err| format!("无法创建冒烟目录: {err}"))?;
    let db_path = work_dir.join("bod.sqlite");
    let config = BodConfig::default();

    let mut store =
        SqliteStore::open(&db_path, &config).map_err(|err| format!("无法打开冒烟数据库: {err}"))?;

    // 1. Issue 与指派。
    {
        let uow = store
            .transaction()
            .map_err(|err| format!("开启事务失败: {err}"))?;
        uow.insert_issue(&issue())
            .map_err(|err| format!("写入 Issue 失败: {err}"))?;
        uow.commit().map_err(|err| format!("提交失败: {err}"))?;
    }

    // 2. Fake Executor 启动（真实写入 invocations 与 start_audits）。
    let mut executor = FakeExecutor::new(FakeScenario::ProcessExitZero);
    let verdict = submit_start(
        &mut store,
        &mut executor,
        company().as_str(),
        &start_request(),
        now_ms(),
    )
    .map_err(|err| format!("start 失败: {err}"))?;
    let execution_id = match verdict {
        StartVerdict::Accepted { execution_id, .. } => execution_id,
        StartVerdict::Rejected { code, reason } => {
            return Err(format!("start 被拒绝 {code}: {reason}"));
        }
        StartVerdict::Unknown { reason, .. } => {
            return Err(format!("start 结果未知，冒烟不能假装成功: {reason}"));
        }
    };

    // 3. 候选成果登记：执行端完成只产生候选。
    let cas =
        CasStore::open(work_dir.join("cas")).map_err(|err| format!("CAS 初始化失败: {err}"))?;
    let artifact = Artifact {
        id: id("report-1"),
        company_id: company(),
        project_id: id("project-1"),
        kind: ArtifactKind::Report,
        title: "冒烟集成测试报告".to_string(),
        owner: id("coder-1"),
        visibility: ArtifactVisibility::Internal,
        accepted_revision: None,
    };
    ensure_artifact(&mut store, &artifact, now_ms())
        .map_err(|err| format!("登记成果身份失败: {err}"))?;
    let candidate = submit_candidate(
        &mut store,
        &cas,
        company().as_str(),
        &artifact.id,
        &id("rev-1"),
        None,
        b"candidate report: integration passed",
        "text/markdown",
        &id("coder-1"),
        &id("rules-v1"),
        &id("env-local"),
        now_ms(),
    )
    .map_err(|err| format!("登记候选成果失败: {err}"))?;
    if candidate.accepted {
        return Err("候选成果在执行结束时被错误标记为已接受".to_string());
    }

    // 4. 只读查询：不得改变版本，也不得触发派发。
    let api = ControlApi::new();
    let actor = Principal {
        id: id("user-1"),
        kind: PrincipalKind::User,
        claimed_roles: Vec::new(),
    };
    let scope = issue_scope();
    let before = api
        .read_issue(
            &mut store,
            &RequestContext {
                request_id: "smoke-read-1",
                actor: &actor,
                scope: &scope,
            },
            "issue-1",
        )
        .map_err(|err| format!("只读查询失败: {err:?}"))?;
    let after = api
        .read_issue(
            &mut store,
            &RequestContext {
                request_id: "smoke-read-2",
                actor: &actor,
                scope: &scope,
            },
            "issue-1",
        )
        .map_err(|err| format!("二次只读查询失败: {err:?}"))?;
    if before.resource_version != after.resource_version {
        return Err("只读查询改变了资源版本".to_string());
    }
    if api.dispatch_probe().count() != 0 {
        return Err("只读查询触发了派发".to_string());
    }

    // 5. 模拟宿主重启：重新打开同一数据库并按 execution_id 找回。
    drop(store);
    let mut reopened =
        SqliteStore::open(&db_path, &config).map_err(|err| format!("重开数据库失败: {err}"))?;
    let recovered = reopened
        .find_invocation_by_execution_id(&execution_id)
        .map_err(|err| format!("按 execution_id 查询失败: {err}"))?;
    let record = reopened
        .transaction()
        .map_err(|err| format!("开启只读事务失败: {err}"))?
        .latest_verification("rev-1")
        .map_err(|err| format!("读取验证状态失败: {err}"))?
        .ok_or_else(|| "重启后找不到候选成果的验证记录".to_string())?;
    let artifact_after = reopened
        .transaction()
        .map_err(|err| format!("开启只读事务失败: {err}"))?
        .get_artifact("report-1")
        .map_err(|err| format!("读取成果失败: {err}"))?
        .ok_or_else(|| "重启后找不到成果身份".to_string())?;

    if recovered.is_none() {
        return Err("重启后按 execution_id 找不到 invocation".to_string());
    }
    if artifact_after.accepted_revision.is_some() {
        return Err("重启后候选成果被错误标记为已接受".to_string());
    }
    if record.state != bod_app::domain::artifact::VerificationState::Pending {
        return Err(format!(
            "重启后验证状态应为 pending，实际 {}",
            record.state.as_str()
        ));
    }

    Ok(SmokeReport {
        work_dir: work_dir.display().to_string(),
        issue: "issue-1".to_string(),
        invocation: "inv-smoke-1".to_string(),
        execution_id,
        candidate_revision: "rev-1".to_string(),
        verification_state: record.state.as_str().to_string(),
        accepted: false,
        resource_version_before: before.resource_version,
        resource_version_after: after.resource_version,
        read_only_dispatches: api.dispatch_probe().count(),
        restart_recovered: true,
        result: "pass",
        notes: vec![
            "执行端 Completed 只登记候选，验收仍为 Pending".to_string(),
            "重启后按 execution_id 找回且未重复调用执行端".to_string(),
            "只读查询未改变 resource_version 也未派发".to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_runs_end_to_end_without_accepting_candidate() {
        let report = run().expect("冒烟应通过");
        assert_eq!(report.result, "pass");
        assert!(!report.accepted);
        assert_eq!(report.verification_state, "pending");
        assert!(report.restart_recovered);
        assert_eq!(report.read_only_dispatches, 0);
        assert_eq!(
            report.resource_version_before,
            report.resource_version_after
        );
        assert!(report.to_json().contains("\"result\":\"pass\""));
    }
}
