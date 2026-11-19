//! P0 D5 成果登记与接受编排（任务 6.1—6.4）。
//!
//! 三条硬顺序：
//! 1. CAS 字节先落盘并原子转正，**之后**才在事务里登记引用；
//! 2. 执行端 Completed 只登记候选，验收从 Pending 开始，绝不因执行结束变 Accepted；
//! 3. 接受必须绑定确定修订，指针用 compare-and-swap 更新，并发失败整体回滚。

use crate::cas::{CasObject, CasStore};
use crate::domain::artifact::{
    AcceptanceBinding, AcceptanceDecision, AcceptanceError, AcceptanceEvidence, AcceptancePolicy,
    AcceptanceRecord, Artifact, ArtifactRevision, DecisionRecord, Digest, IssueCompletion,
    RevisionDependency, StorageRef, VerificationRecord, VerificationState, Waiver,
    WorkPackageState, decide_acceptance, evaluate_issue_completion, on_verification_failure,
};
use crate::domain::model::Principal;
use crate::domain::{DomainId, Phase};
use crate::store::{SqliteStore, StoreError, UnitOfWork};

#[derive(Debug)]
pub enum AcceptanceStoreError {
    Store(StoreError),
    Domain(AcceptanceError),
    /// 并发接受已发生：调用方必须重新读取，不得覆盖。
    PointerConflict {
        current: Option<DomainId>,
    },
    NotFound(&'static str),
}

impl std::fmt::Display for AcceptanceStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::Domain(err) => write!(f, "{err}"),
            Self::PointerConflict { current } => match current {
                Some(value) => write!(f, "接受指针已被并发更新为 {}", value.as_str()),
                None => write!(f, "接受指针已被并发更新"),
            },
            Self::NotFound(what) => write!(f, "未找到{what}"),
        }
    }
}

impl std::error::Error for AcceptanceStoreError {}

impl From<StoreError> for AcceptanceStoreError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<AcceptanceError> for AcceptanceStoreError {
    fn from(value: AcceptanceError) -> Self {
        Self::Domain(value)
    }
}

/// 候选成果：`accepted` 必须为 false，直到有人按策略显式接受。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub artifact_id: DomainId,
    pub revision_id: DomainId,
    pub digest: Digest,
    pub verification_state: VerificationState,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    Accepted(AcceptanceRecord),
    AutoAccepted(AcceptanceRecord),
    /// 需要独立人工批准：本次不写入任何接受记录。
    RequiresHuman,
}

impl AcceptOutcome {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_) | Self::AutoAccepted(_))
    }
}

/// 登记一个已存在的 Artifact（不存在则创建）。
pub fn ensure_artifact(
    store: &mut SqliteStore,
    artifact: &Artifact,
    now_ms: i64,
) -> Result<(), AcceptanceStoreError> {
    let uow = store.transaction()?;
    if uow.get_artifact(artifact.id.as_str())?.is_none() {
        uow.insert_artifact(artifact, now_ms)?;
    }
    uow.commit()?;
    Ok(())
}

/// 先 CAS 落盘，再在事务中登记修订引用（§33.5）。
/// 若事务失败，字节留在 CAS 成为孤儿，由 GC dry-run 报告——不会被当成有效成果。
#[allow(clippy::too_many_arguments)]
pub fn register_cas_revision(
    store: &mut SqliteStore,
    cas: &CasStore,
    company_id: &str,
    artifact_id: &DomainId,
    revision_id: &DomainId,
    base_revision: Option<DomainId>,
    bytes: &[u8],
    mime: &str,
    produced_by: &DomainId,
    source_note: &str,
    depends_on: Vec<RevisionDependency>,
    now_ms: i64,
) -> Result<ArtifactRevision, AcceptanceStoreError> {
    let object = cas.stage(bytes, mime).map_err(|err| {
        AcceptanceStoreError::Store(StoreError::Io(std::io::Error::other(err.to_string())))
    })?;
    register_object_as_revision(
        store,
        company_id,
        &object,
        artifact_id,
        revision_id,
        base_revision,
        produced_by,
        source_note,
        depends_on,
        now_ms,
    )
}

/// 已转正对象的登记入口：`stage` 与登记之间不得插入任何写入。
#[allow(clippy::too_many_arguments)]
fn register_object_as_revision(
    store: &mut SqliteStore,
    company_id: &str,
    object: &CasObject,
    artifact_id: &DomainId,
    revision_id: &DomainId,
    base_revision: Option<DomainId>,
    produced_by: &DomainId,
    source_note: &str,
    depends_on: Vec<RevisionDependency>,
    now_ms: i64,
) -> Result<ArtifactRevision, AcceptanceStoreError> {
    if store
        .transaction()?
        .get_artifact(artifact_id.as_str())?
        .is_none()
    {
        return Err(AcceptanceStoreError::NotFound("成果身份"));
    }
    let revision = ArtifactRevision {
        revision_id: revision_id.clone(),
        artifact_id: artifact_id.clone(),
        base_revision,
        storage: StorageRef::Cas(object.storage_ref()),
        digest: object.digest.clone(),
        size_bytes: object.size_bytes,
        mime: object.mime.clone(),
        produced_by: produced_by.clone(),
        source_note: source_note.to_string(),
        depends_on,
        created_at_ms: now_ms,
    };
    revision.validate()?;
    let uow = store.transaction()?;
    uow.insert_artifact_revision(company_id, &revision)?;
    uow.commit()?;
    Ok(revision)
}

/// 登记外部权威存储（Git commit / 制品 digest / 文档修订）的修订。
#[allow(clippy::too_many_arguments)]
pub fn register_external_revision(
    store: &mut SqliteStore,
    company_id: &str,
    artifact_id: &DomainId,
    revision_id: &DomainId,
    base_revision: Option<DomainId>,
    locator: &str,
    digest: Digest,
    size_bytes: i64,
    mime: &str,
    produced_by: &DomainId,
    source_note: &str,
    now_ms: i64,
) -> Result<ArtifactRevision, AcceptanceStoreError> {
    let revision = ArtifactRevision {
        revision_id: revision_id.clone(),
        artifact_id: artifact_id.clone(),
        base_revision,
        storage: StorageRef::External(locator.to_string()),
        digest,
        size_bytes,
        mime: mime.to_string(),
        produced_by: produced_by.clone(),
        source_note: source_note.to_string(),
        depends_on: Vec::new(),
        created_at_ms: now_ms,
    };
    revision.validate()?;
    let uow = store.transaction()?;
    uow.insert_artifact_revision(company_id, &revision)?;
    uow.commit()?;
    Ok(revision)
}

/// 执行端 Completed 之后的候选登记：写入 Pending 验证记录，绝不接受。
#[allow(clippy::too_many_arguments)]
pub fn submit_candidate(
    store: &mut SqliteStore,
    cas: &CasStore,
    company_id: &str,
    artifact_id: &DomainId,
    revision_id: &DomainId,
    base_revision: Option<DomainId>,
    bytes: &[u8],
    mime: &str,
    produced_by: &DomainId,
    verification_rules: &DomainId,
    environment: &DomainId,
    now_ms: i64,
) -> Result<Candidate, AcceptanceStoreError> {
    let revision = register_cas_revision(
        store,
        cas,
        company_id,
        artifact_id,
        revision_id,
        base_revision,
        bytes,
        mime,
        produced_by,
        "执行端提交的候选成果",
        Vec::new(),
        now_ms,
    )?;
    let record = VerificationRecord {
        revision_id: revision_id.clone(),
        state: VerificationState::Pending,
        actor: produced_by.clone(),
        reason: "执行端完成只产生候选".to_string(),
        evidence_refs: Vec::new(),
        verification_rules: verification_rules.clone(),
        environment: environment.clone(),
        observed_at_ms: now_ms,
    };
    let uow = store.transaction()?;
    uow.insert_verification_record(
        &format!("vr-{}-pending", revision_id.as_str()),
        company_id,
        &record,
    )?;
    uow.commit()?;

    Ok(Candidate {
        artifact_id: artifact_id.clone(),
        revision_id: revision_id.clone(),
        digest: revision.digest,
        verification_state: VerificationState::Pending,
        accepted: false,
    })
}

/// 开始验证：Pending → Verifying。
pub fn begin_verification(
    store: &mut SqliteStore,
    company_id: &str,
    revision_id: &DomainId,
    actor: &DomainId,
    rules: &DomainId,
    environment: &DomainId,
    now_ms: i64,
) -> Result<VerificationRecord, AcceptanceStoreError> {
    let current = store
        .transaction()?
        .latest_verification(revision_id.as_str())?
        .map(|record| record.state)
        .unwrap_or(VerificationState::Pending);
    let record = crate::domain::artifact::advance_verification(
        current,
        &crate::domain::artifact::VerificationDraft {
            next: VerificationState::Verifying,
            actor,
            reason: "开始验证",
            revision: revision_id,
            rules,
            environment,
            evidence_refs: Vec::new(),
            observed_at_ms: now_ms,
        },
    )?;
    let uow = store.transaction()?;
    uow.insert_verification_record(
        &format!(
            "vr-{}-{}-{}",
            revision_id.as_str(),
            record.state.as_str(),
            now_ms
        ),
        company_id,
        &record,
    )?;
    uow.commit()?;
    Ok(record)
}

/// 结束验证：只允许 → Verified / Rejected / NeedsRevision。
#[allow(clippy::too_many_arguments)]
pub fn complete_verification(
    store: &mut SqliteStore,
    company_id: &str,
    revision_id: &DomainId,
    next: VerificationState,
    actor: &DomainId,
    reason: &str,
    rules: &DomainId,
    environment: &DomainId,
    evidence_refs: Vec<String>,
    now_ms: i64,
) -> Result<VerificationRecord, AcceptanceStoreError> {
    let current = store
        .transaction()?
        .latest_verification(revision_id.as_str())?
        .map(|record| record.state)
        .unwrap_or(VerificationState::Pending);
    let record = crate::domain::artifact::advance_verification(
        current,
        &crate::domain::artifact::VerificationDraft {
            next,
            actor,
            reason,
            revision: revision_id,
            rules,
            environment,
            evidence_refs,
            observed_at_ms: now_ms,
        },
    )?;
    let uow = store.transaction()?;
    uow.insert_verification_record(
        &format!(
            "vr-{}-{}-{}",
            revision_id.as_str(),
            record.state.as_str(),
            now_ms
        ),
        company_id,
        &record,
    )?;
    uow.commit()?;
    Ok(record)
}

/// 豁免：记录主体/理由/版本/范围，并把验证状态置为 Waived（≠ 通过）。
#[allow(clippy::too_many_arguments)]
pub fn waive_revision(
    store: &mut SqliteStore,
    company_id: &str,
    revision_id: &DomainId,
    actor: &Principal,
    reason: &str,
    scope: crate::domain::model::Scope,
    rules: &DomainId,
    environment: &DomainId,
    now_ms: i64,
) -> Result<Waiver, AcceptanceStoreError> {
    let waiver = crate::domain::artifact::grant_waiver(actor, revision_id, reason, scope, now_ms)?;
    let current = store
        .transaction()?
        .latest_verification(revision_id.as_str())?
        .map(|record| record.state)
        .unwrap_or(VerificationState::Pending);
    let record = crate::domain::artifact::advance_verification(
        current,
        &crate::domain::artifact::VerificationDraft {
            next: VerificationState::Waived,
            actor: &actor.id,
            reason,
            revision: revision_id,
            rules,
            environment,
            evidence_refs: Vec::new(),
            observed_at_ms: now_ms,
        },
    )?;
    let uow = store.transaction()?;
    uow.insert_waiver(
        &format!("waiver-{}-{}", revision_id.as_str(), now_ms),
        company_id,
        &waiver,
    )?;
    uow.insert_verification_record(
        &format!("vr-{}-waived-{}", revision_id.as_str(), now_ms),
        company_id,
        &record,
    )?;
    uow.commit()?;
    Ok(waiver)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptRequest {
    pub acceptance_id: DomainId,
    pub binding: AcceptanceBinding,
    pub checks_passed: bool,
    pub blocking_risks: Vec<String>,
    pub reason: String,
    pub evidence_refs: Vec<String>,
}

/// 接受：证据必须匹配绑定，决策合法才写入；指针 CAS 失败整体回滚。
#[allow(clippy::too_many_arguments)]
pub fn accept(
    store: &mut SqliteStore,
    company_id: &str,
    artifact_id: &DomainId,
    request: &AcceptRequest,
    actor: &Principal,
    policy: &AcceptancePolicy,
    now_ms: i64,
) -> Result<AcceptOutcome, AcceptanceStoreError> {
    let uow = store.transaction()?;
    let artifact = uow
        .get_artifact(artifact_id.as_str())?
        .ok_or(AcceptanceStoreError::NotFound("成果身份"))?;
    let revision = uow
        .get_artifact_revision(request.binding.artifact_revision.as_str())?
        .ok_or(AcceptanceStoreError::NotFound("产物修订"))?;
    if revision.artifact_id != *artifact_id {
        return Err(AcceptanceStoreError::Domain(
            AcceptanceError::StaleAcceptance {
                expected: artifact_id.clone(),
                actual: revision.artifact_id.clone(),
            },
        ));
    }

    let verification = uow
        .latest_verification(revision.revision_id.as_str())?
        .ok_or_else(|| AcceptanceStoreError::NotFound("验证记录"))?;
    let mut evidence = AcceptanceEvidence::from_record(&verification, request.checks_passed);
    evidence.blocking_risks = request.blocking_risks.clone();

    let decision = decide_acceptance(
        actor,
        &revision.produced_by,
        &request.binding,
        &evidence,
        policy,
    )?;
    if decision == AcceptanceDecision::RequiresHuman {
        // 不写入也不推进指针：需要独立人工批准。
        return Ok(AcceptOutcome::RequiresHuman);
    }

    let record = AcceptanceRecord {
        acceptance_id: request.acceptance_id.clone(),
        artifact_id: artifact_id.clone(),
        binding: request.binding.clone(),
        decision,
        actor: actor.id.clone(),
        reason: request.reason.clone(),
        evidence_refs: request.evidence_refs.clone(),
        accepted_at_ms: now_ms,
        superseded_by: None,
    };
    uow.insert_acceptance(company_id, &record)?;
    uow.supersede_acceptances(artifact_id.as_str(), request.acceptance_id.as_str())?;
    // accepted_revision 指针：旧接受记录保留，但只有新指针生效。
    let swapped = uow.cas_accepted_revision(
        artifact_id.as_str(),
        artifact.accepted_revision.as_ref().map(DomainId::as_str),
        request.binding.artifact_revision.as_str(),
    )?;
    if !swapped {
        // 事务随 Drop 回滚：不留下"接受记录已写入但指针未前进"的半状态。
        let current = uow
            .get_artifact(artifact_id.as_str())?
            .and_then(|found| found.accepted_revision);
        return Err(AcceptanceStoreError::PointerConflict { current });
    }
    uow.commit()?;

    Ok(match decision {
        AcceptanceDecision::Accepted => AcceptOutcome::Accepted(record),
        AcceptanceDecision::AutoAccepted => AcceptOutcome::AutoAccepted(record),
        AcceptanceDecision::RequiresHuman => AcceptOutcome::RequiresHuman,
    })
}

/// 工作包接受登记：只有被接受的工作包才计入 Issue 完成条件。
pub fn mark_work_package_accepted(
    store: &mut SqliteStore,
    company_id: &str,
    issue_uid: &str,
    work_package_revision: &DomainId,
    artifact_revision: &DomainId,
) -> Result<(), AcceptanceStoreError> {
    let uow = store.transaction()?;
    let existing = uow
        .list_work_package_states(issue_uid)?
        .into_iter()
        .find(|state| &state.revision == work_package_revision);
    let state = WorkPackageState {
        revision: work_package_revision.clone(),
        required: existing
            .as_ref()
            .map(|value| value.required)
            .unwrap_or(true),
        accepted_revision: Some(artifact_revision.clone()),
        running: false,
        blockers: Vec::new(),
    };
    uow.upsert_work_package_state(company_id, issue_uid, &state)?;
    uow.commit()?;
    Ok(())
}

/// Issue 完成判定：部分工作包接受、单个 Agent 自报完成都不算完成。
pub fn evaluate_completion(
    store: &mut SqliteStore,
    issue_uid: &str,
    final_gate_passed: bool,
    agent_reported_done: bool,
) -> Result<IssueCompletion, AcceptanceStoreError> {
    let packages = store.transaction()?.list_work_package_states(issue_uid)?;
    Ok(evaluate_issue_completion(
        &packages,
        final_gate_passed,
        agent_reported_done,
    ))
}

/// 验证失败：生成新 Attempt 与 NeedsRevision，保留原规格/验收修订。
#[allow(clippy::too_many_arguments)]
pub fn apply_verification_failure(
    store: &mut SqliteStore,
    company_id: &str,
    issue_uid: &str,
    work_package_revision: &DomainId,
    previous_attempt_key: &str,
    attempt_seq: u32,
    reason: &str,
    spec_revision: &DomainId,
    acceptance_revision: &DomainId,
    now_ms: i64,
) -> Result<AttemptOutcome, AcceptanceStoreError> {
    let draft = on_verification_failure(
        previous_attempt_key,
        attempt_seq,
        reason,
        spec_revision,
        acceptance_revision,
    )?;
    let uow = store.transaction()?;
    uow.insert_event(
        &format!("attempt-{}", draft.attempt_key),
        company_id,
        work_package_revision.as_str(),
        i64::from(draft.attempt_seq) as u64,
        &format!(
            "{{\"kind\":\"attempt_created\",\"attempt\":\"{}\",\"phase\":\"{:?}\"}}",
            draft.attempt_key, draft.issue_phase
        ),
    )?;
    let existing = uow
        .list_work_package_states(issue_uid)?
        .into_iter()
        .find(|state| &state.revision == work_package_revision);
    let state = WorkPackageState {
        revision: work_package_revision.clone(),
        required: existing
            .as_ref()
            .map(|value| value.required)
            .unwrap_or(true),
        accepted_revision: existing.and_then(|value| value.accepted_revision),
        running: false,
        blockers: vec![format!("验证失败：{reason}")],
    };
    uow.upsert_work_package_state(company_id, issue_uid, &state)?;
    uow.commit()?;

    Ok(AttemptOutcome {
        attempt_key: draft.attempt_key,
        attempt_seq: draft.attempt_seq,
        issue_phase: Phase::Blocked,
        reason: draft.reason,
        spec_revision: draft.spec_revision,
        acceptance_revision: draft.acceptance_revision,
        created_at_ms: now_ms,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptOutcome {
    pub attempt_key: String,
    pub attempt_seq: u32,
    pub issue_phase: Phase,
    pub reason: String,
    pub spec_revision: DomainId,
    pub acceptance_revision: DomainId,
    pub created_at_ms: i64,
}

/// 记录决策：实际 actor、scope、决定、原因、证据与修订引用。
pub fn record_decision(
    store: &mut SqliteStore,
    company_id: &str,
    record: &DecisionRecord,
) -> Result<(), AcceptanceStoreError> {
    let uow = store.transaction()?;
    uow.insert_decision(company_id, record)?;
    uow.commit()?;
    Ok(())
}

/// 读取某产物当前生效的接受（用于"旧接受不能批准新修订"的展示）。
pub fn current_acceptance(
    store: &mut SqliteStore,
    artifact_id: &DomainId,
) -> Result<Option<AcceptanceRecord>, AcceptanceStoreError> {
    let uow = store.transaction()?;
    let artifact = uow
        .get_artifact(artifact_id.as_str())?
        .ok_or(AcceptanceStoreError::NotFound("成果身份"))?;
    let Some(accepted) = artifact.accepted_revision else {
        return Ok(None);
    };
    Ok(uow
        .list_acceptances(artifact_id.as_str())?
        .into_iter()
        .find(|record| {
            record.binding.artifact_revision == accepted && record.superseded_by.is_none()
        }))
}

/// 供 GC 调用：读取引用集合并执行 dry-run 扫描（不删除任何字节）。
pub fn gc_dry_run(
    store: &mut SqliteStore,
    cas: &CasStore,
    company_id: &str,
    now_ms: i64,
    grace_ms: i64,
) -> Result<crate::cas::GcPlan, AcceptanceStoreError> {
    let (cas_refs, external) = store.transaction()?.referenced_storage(company_id)?;
    let referenced: std::collections::HashSet<String> = cas_refs.into_iter().collect();
    cas.scan(&referenced, external, now_ms, grace_ms)
        .map_err(|err| {
            AcceptanceStoreError::Store(StoreError::Io(std::io::Error::other(err.to_string())))
        })
}

/// 事务辅助：让调用方在同一事务内完成"登记 + 事件"。
pub fn with_unit_of_work<T>(
    store: &mut SqliteStore,
    action: impl FnOnce(&UnitOfWork<'_>) -> Result<T, StoreError>,
) -> Result<T, AcceptanceStoreError> {
    let uow = store.transaction()?;
    let value = action(&uow)?;
    uow.commit()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::CasStore;
    use crate::config::BodConfig;
    use crate::domain::artifact::{ArtifactKind, ArtifactVisibility, Digest, VerificationState};
    use crate::domain::model::{PrincipalKind, Scope};
    use std::collections::HashSet;

    const NOW: i64 = 1_763_500_000_000;

    struct Fixture {
        store: SqliteStore,
        cas: CasStore,
        root: std::path::PathBuf,
        _db: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let seq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            let root =
                std::env::temp_dir().join(format!("bod-accept-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let db = root.join("bod.sqlite");
            let store = SqliteStore::open(&db, &BodConfig::default()).unwrap();
            let cas = CasStore::open(root.join("cas")).unwrap();
            Self {
                store,
                cas,
                root,
                _db: db,
            }
        }

        fn artifact(&self) -> Artifact {
            Artifact {
                id: DomainId::new("report-1"),
                company_id: DomainId::new("company-default"),
                project_id: DomainId::new("project-1"),
                kind: ArtifactKind::Report,
                title: "集成测试报告".to_string(),
                owner: DomainId::new("coder-1"),
                visibility: ArtifactVisibility::Internal,
                accepted_revision: None,
            }
        }

        fn write_report(&mut self, revision: &str, body: &[u8]) -> Candidate {
            let artifact = self.artifact();
            ensure_artifact(&mut self.store, &artifact, NOW).unwrap();
            submit_candidate(
                &mut self.store,
                &self.cas,
                "company-default",
                &DomainId::new("report-1"),
                &DomainId::new(revision),
                None,
                body,
                "text/markdown",
                &DomainId::new("coder-1"),
                &DomainId::new("rules-v1"),
                &DomainId::new("env-local"),
                NOW,
            )
            .unwrap()
        }

        fn verify(&mut self, revision: &str) {
            begin_verification(
                &mut self.store,
                "company-default",
                &DomainId::new(revision),
                &DomainId::new("reviewer-1"),
                &DomainId::new("rules-v1"),
                &DomainId::new("env-local"),
                NOW,
            )
            .unwrap();
            complete_verification(
                &mut self.store,
                "company-default",
                &DomainId::new(revision),
                VerificationState::Verified,
                &DomainId::new("reviewer-1"),
                "检查通过",
                &DomainId::new("rules-v1"),
                &DomainId::new("env-local"),
                vec!["evidence-1".to_string()],
                NOW,
            )
            .unwrap();
        }
    }

    fn user(value: &str) -> Principal {
        Principal {
            id: DomainId::new(value),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    fn agent(value: &str) -> Principal {
        Principal {
            id: DomainId::new(value),
            kind: PrincipalKind::Agent,
            claimed_roles: vec!["owner".to_string()],
        }
    }

    fn binding(revision: &str) -> AcceptanceBinding {
        AcceptanceBinding {
            work_package_revision: DomainId::new("wp-r1"),
            input_baseline: DomainId::new("baseline-1"),
            artifact_revision: DomainId::new(revision),
            verification_rules: DomainId::new("rules-v1"),
            target_environment: DomainId::new("env-local"),
            merge_combination: None,
        }
    }

    fn request(revision: &str, id: &str) -> AcceptRequest {
        AcceptRequest {
            acceptance_id: DomainId::new(id),
            binding: binding(revision),
            checks_passed: true,
            blocking_risks: Vec::new(),
            reason: "评审通过".to_string(),
            evidence_refs: vec!["evidence-1".to_string()],
        }
    }

    #[test]
    fn completed_executor_result_is_only_a_candidate() {
        let mut fixture = Fixture::new();
        let candidate = fixture.write_report("rev-1", b"report body");
        assert_eq!(candidate.verification_state, VerificationState::Pending);
        assert!(!candidate.accepted, "Completed 不等于 Accepted");
        assert!(
            current_acceptance(&mut fixture.store, &DomainId::new("report-1"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn accept_binds_revision_and_moves_pointer() {
        let mut fixture = Fixture::new();
        fixture.write_report("rev-1", b"report body");
        fixture.verify("rev-1");

        let outcome = accept(
            &mut fixture.store,
            "company-default",
            &DomainId::new("report-1"),
            &request("rev-1", "acc-1"),
            &user("user-1"),
            &AcceptancePolicy::default(),
            NOW + 10,
        )
        .unwrap();
        assert!(outcome.is_accepted());

        let current = current_acceptance(&mut fixture.store, &DomainId::new("report-1"))
            .unwrap()
            .expect("应有生效接受");
        assert_eq!(current.artifact_id.as_str(), "report-1");
        assert_eq!(current.binding.artifact_revision.as_str(), "rev-1");
        assert!(current.approves(&DomainId::new("rev-1")));
        assert!(!current.approves(&DomainId::new("rev-2")));
    }

    mod invariant_i_08 {
        use super::*;

        #[test]
        fn old_acceptance_cannot_approve_new_revision() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-1", "acc-1"),
                &user("user-1"),
                &AcceptancePolicy::default(),
                NOW,
            )
            .unwrap();

            // 新修订只有 Pending 验证；用旧接受去批准必须失败。
            fixture.write_report("rev-2", b"v2");
            let outcome = accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-2", "acc-2"),
                &user("user-1"),
                &AcceptancePolicy::default(),
                NOW + 1,
            )
            .unwrap_err();
            assert!(
                outcome.to_string().contains("验证状态"),
                "新修订未验证时不得接受：{outcome}"
            );
        }

        #[test]
        fn evidence_from_previous_revision_is_rejected() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            fixture.write_report("rev-2", b"v2");
            fixture.verify("rev-2");

            // 绑定 rev-2，但验证记录取自 rev-1：必须按旧证据拒绝。
            let revision = fixture
                .store
                .transaction()
                .unwrap()
                .get_artifact_revision("rev-1")
                .unwrap()
                .unwrap();
            assert_eq!(revision.revision_id.as_str(), "rev-1");
            let stale = AcceptanceEvidence {
                artifact_revision: DomainId::new("rev-1"),
                verification_rules: DomainId::new("rules-v1"),
                target_environment: DomainId::new("env-local"),
                verification_state: VerificationState::Verified,
                checks_passed: true,
                blocking_risks: Vec::new(),
            };
            let err = crate::domain::artifact::decide_acceptance(
                &user("user-1"),
                &DomainId::new("coder-1"),
                &binding("rev-2"),
                &stale,
                &AcceptancePolicy::default(),
            )
            .unwrap_err();
            assert_eq!(err.code(), "E_STALE_ACCEPTANCE");
        }

        #[test]
        fn later_acceptance_supersedes_previous_and_stale_cas_is_refused() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-1", "acc-1"),
                &user("user-1"),
                &AcceptancePolicy::default(),
                NOW,
            )
            .unwrap();
            accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-1", "acc-2"),
                &user("user-2"),
                &AcceptancePolicy::default(),
                NOW + 5,
            )
            .unwrap();

            let records = fixture
                .store
                .transaction()
                .unwrap()
                .list_acceptances("report-1")
                .unwrap();
            assert_eq!(records.len(), 2, "历史接受保留");
            assert_eq!(
                records[0].superseded_by.as_ref().map(DomainId::as_str),
                Some("acc-2")
            );
            assert!(records[1].superseded_by.is_none());

            // 同一写入连接内 accept 以 BEGIN IMMEDIATE 串行化；
            // CAS 是给"事务外读取后写入"的调用方的最后防线。
            let stale = fixture
                .store
                .transaction()
                .unwrap()
                .cas_accepted_revision("report-1", Some("rev-0"), "rev-9")
                .unwrap();
            assert!(!stale, "过期期望值不得推进指针");
        }
    }

    mod invariant_i_51 {
        use super::*;

        #[test]
        fn implementer_cannot_accept_own_candidate() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            let err = accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-1", "acc-1"),
                &agent("coder-1"),
                &AcceptancePolicy {
                    auto_accept_enabled: true,
                    required_checks: 3,
                    high_risk: false,
                    production_write: false,
                },
                NOW,
            )
            .unwrap_err();
            assert!(err.to_string().contains("实现者"));
        }

        #[test]
        fn high_risk_requires_human_and_writes_nothing() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            let outcome = accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-1", "acc-1"),
                &agent("reviewer-agent"),
                &AcceptancePolicy::default(),
                NOW,
            )
            .unwrap();
            assert_eq!(outcome, AcceptOutcome::RequiresHuman);
            assert!(
                fixture
                    .store
                    .transaction()
                    .unwrap()
                    .list_acceptances("report-1")
                    .unwrap()
                    .is_empty()
            );
        }

        #[test]
        fn production_write_requires_human_even_when_policy_allows_auto() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            let outcome = accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-1", "acc-1"),
                &agent("reviewer-agent"),
                &AcceptancePolicy {
                    auto_accept_enabled: true,
                    required_checks: 1,
                    high_risk: false,
                    production_write: true,
                },
                NOW,
            )
            .unwrap();
            assert_eq!(outcome, AcceptOutcome::RequiresHuman);
        }
    }

    mod g_9 {
        use super::*;

        #[test]
        fn waiver_is_recorded_but_never_accepts() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            let scope = Scope {
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                issue_id: None,
            };
            let waiver = waive_revision(
                &mut fixture.store,
                "company-default",
                &DomainId::new("rev-1"),
                &user("user-1"),
                "低风险文案修订",
                scope,
                &DomainId::new("rules-v1"),
                &DomainId::new("env-local"),
                NOW + 1,
            )
            .unwrap();
            assert_eq!(waiver.actor.as_str(), "user-1");
            assert!(!waiver.counts_as_verified());

            let err = accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &request("rev-1", "acc-1"),
                &user("user-1"),
                &AcceptancePolicy::default(),
                NOW + 2,
            )
            .unwrap_err();
            assert!(err.to_string().contains("豁免"));
        }

        #[test]
        fn checks_failure_blocks_acceptance() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            let mut req = request("rev-1", "acc-1");
            req.checks_passed = false;
            let err = accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &req,
                &user("user-1"),
                &AcceptancePolicy::default(),
                NOW,
            )
            .unwrap_err();
            assert!(err.to_string().contains("检查"));
        }

        #[test]
        fn blocking_risk_blocks_acceptance() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"v1");
            fixture.verify("rev-1");
            let mut req = request("rev-1", "acc-1");
            req.blocking_risks = vec!["迁移未验证".to_string()];
            let err = accept(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &req,
                &user("user-1"),
                &AcceptancePolicy::default(),
                NOW,
            )
            .unwrap_err();
            assert!(err.to_string().contains("阻塞风险"));
        }
    }

    mod invariant_i_07 {
        use super::*;

        #[test]
        fn registered_revision_is_retained_and_unregistered_stays_orphan() {
            let mut fixture = Fixture::new();
            fixture.write_report("rev-1", b"registered");
            // 直接写一个未登记的对象，模拟崩溃在"转正后、登记前"。
            let orphan = fixture
                .cas
                .stage(b"never registered", "text/plain")
                .unwrap();

            let plan = gc_dry_run(
                &mut fixture.store,
                &fixture.cas,
                "company-default",
                // 用一个明显晚于文件 mtime 的时间，避免依赖真实时钟。
                4_000_000_000_000,
                0,
            )
            .unwrap();
            assert!(
                plan.orphans
                    .iter()
                    .any(|item| item.digest.as_deref() == Some(orphan.digest.value.as_str()))
            );
            assert!(plan.orphans.iter().all(|item| item.digest.as_deref()
                != Some(crate::domain::artifact::sha256_hex(b"registered").as_str())));
            assert_eq!(plan.retained, 1);
            assert!(orphan.path.is_file(), "dry-run 不删除任何文件");
        }

        #[test]
        fn external_revision_is_counted_and_protected() {
            let mut fixture = Fixture::new();
            let artifact = fixture.artifact();
            ensure_artifact(&mut fixture.store, &artifact, NOW).unwrap();
            register_external_revision(
                &mut fixture.store,
                "company-default",
                &DomainId::new("report-1"),
                &DomainId::new("rev-git"),
                None,
                "git:repo-1@commit-abc:docs/report.md",
                Digest::of_bytes(b"external"),
                8,
                "text/markdown",
                &DomainId::new("coder-1"),
                "Git commit 固定引用",
                NOW,
            )
            .unwrap();

            let plan = gc_dry_run(
                &mut fixture.store,
                &fixture.cas,
                "company-default",
                NOW + 1,
                3_600_000,
            )
            .unwrap();
            assert_eq!(plan.external_refs, 1);
            assert!(plan.is_empty(), "外部引用不产生本地孤儿");
            let referenced: HashSet<String> = HashSet::new();
            assert!(
                fixture
                    .cas
                    .scan(&referenced, plan.external_refs, NOW, 3_600_000)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    mod work_packages {
        use super::*;

        #[test]
        fn partial_acceptance_does_not_complete_issue() {
            let mut fixture = Fixture::new();
            mark_work_package_accepted(
                &mut fixture.store,
                "company-default",
                "issue-1",
                &DomainId::new("wp-1"),
                &DomainId::new("rev-1"),
            )
            .unwrap();
            let uow = fixture.store.transaction().unwrap();
            uow.upsert_work_package_state(
                "company-default",
                "issue-1",
                &WorkPackageState {
                    revision: DomainId::new("wp-2"),
                    required: true,
                    accepted_revision: None,
                    running: true,
                    blockers: Vec::new(),
                },
            )
            .unwrap();
            uow.commit().unwrap();

            let completion =
                evaluate_completion(&mut fixture.store, "issue-1", true, true).unwrap();
            assert!(!completion.complete);
            assert_eq!(completion.pending, vec![DomainId::new("wp-2")]);
        }

        #[test]
        fn all_required_accepted_and_gate_passed_completes() {
            let mut fixture = Fixture::new();
            mark_work_package_accepted(
                &mut fixture.store,
                "company-default",
                "issue-1",
                &DomainId::new("wp-1"),
                &DomainId::new("rev-1"),
            )
            .unwrap();
            let completion =
                evaluate_completion(&mut fixture.store, "issue-1", true, true).unwrap();
            assert!(completion.complete);
        }
    }

    #[test]
    fn verification_failure_creates_new_attempt_and_keeps_history() {
        let mut fixture = Fixture::new();
        let outcome = apply_verification_failure(
            &mut fixture.store,
            "company-default",
            "issue-1",
            &DomainId::new("wp-1"),
            "inv-1",
            2,
            "覆盖率不足",
            &DomainId::new("spec-r3"),
            &DomainId::new("acc-r2"),
            NOW,
        )
        .unwrap();
        assert_eq!(outcome.attempt_key, "inv-1#attempt-3");
        assert_eq!(outcome.spec_revision.as_str(), "spec-r3");
        assert_eq!(outcome.acceptance_revision.as_str(), "acc-r2");

        let packages = fixture
            .store
            .transaction()
            .unwrap()
            .list_work_package_states("issue-1")
            .unwrap();
        assert_eq!(packages.len(), 1);
        assert!(packages[0].blockers[0].contains("覆盖率不足"));
    }

    #[test]
    fn decision_record_persists_actor_scope_and_refs() {
        let mut fixture = Fixture::new();
        let record = DecisionRecord {
            decision_id: DomainId::new("decision-1"),
            actor: DomainId::new("user-1"),
            scope: Scope {
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                issue_id: Some(DomainId::new("issue-1")),
            },
            decision: "按现有规格实现".to_string(),
            reason: "答疑不改变需求".to_string(),
            evidence_refs: vec!["ev-1".to_string()],
            revision_refs: vec![DomainId::new("spec-r1")],
            created_at_ms: NOW,
        };
        record_decision(&mut fixture.store, "company-default", &record).unwrap();
        let count: i64 = fixture
            .store
            .transaction()
            .unwrap()
            .count_decisions("issue-1")
            .unwrap();
        assert_eq!(count, 1);
        assert!(fixture.root.join("cas").is_dir());
    }
}
