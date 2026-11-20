//! P1 切片 4.2：真实仓库直派闭环。
//!
//! 三条硬顺序：
//! 1. 共享指令/规则修订必须携带确定 base revision 并逐次 CAS，不能最后写覆盖（I-18/I-20）；
//! 2. 候选成果绑定 `git rev-parse HEAD` 的确定提交，执行端自报通过不改变 Pending；
//! 3. 接受只消费与提交/规格/规则/环境逐项匹配的本地验证报告，写入宿主库后重启可查。
//!
//! 本切片只做宿主侧绑定与门禁，不拉起 Agent、不写第二个业务库；工作台投影留 4.5。

use crate::artifact_acceptance::{
    AcceptOutcome, AcceptRequest, AcceptanceStoreError, accept, begin_verification,
    complete_verification, current_acceptance, ensure_artifact, register_external_revision,
};
use crate::domain::DomainId;
use crate::domain::artifact::{
    AcceptanceBinding, AcceptanceDecision, AcceptancePolicy, Artifact, ArtifactKind,
    ArtifactVisibility, Digest, VerificationRecord, VerificationState,
};
use crate::domain::model::Principal;
use crate::store::{SqliteStore, StoreError};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum CodingIntegrationError {
    Git { args: String, stderr: String },
    Io(std::io::Error),
    Store(StoreError),
    Acceptance(AcceptanceStoreError),
    Instruction(InstructionError),
    InvalidReport(&'static str),
    NotFound(&'static str),
}

impl fmt::Display for CodingIntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git { args, stderr } => write!(f, "git {args} 失败：{stderr}"),
            Self::Io(err) => write!(f, "直派 IO 失败：{err}"),
            Self::Store(err) => write!(f, "直派存储失败：{err}"),
            Self::Acceptance(err) => write!(f, "接受门禁失败：{err}"),
            Self::Instruction(err) => write!(f, "共享修订门禁失败：{err}"),
            Self::InvalidReport(reason) => write!(f, "验证报告不可用：{reason}"),
            Self::NotFound(what) => write!(f, "未找到{what}"),
        }
    }
}

impl std::error::Error for CodingIntegrationError {}

impl From<std::io::Error> for CodingIntegrationError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<StoreError> for CodingIntegrationError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<AcceptanceStoreError> for CodingIntegrationError {
    fn from(value: AcceptanceStoreError) -> Self {
        Self::Acceptance(value)
    }
}

impl From<InstructionError> for CodingIntegrationError {
    fn from(value: InstructionError) -> Self {
        Self::Instruction(value)
    }
}

// ---------------------------------------------------------------------------
// 共享指令与规则修订（I-18 / I-20）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstructionError {
    EmptyField(&'static str),
    IndefiniteRevision {
        field: &'static str,
        value: String,
    },
    InvalidContentDigest,
    BaseMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    ConcurrentRole {
        path: String,
        holder: String,
        requester: String,
    },
    NotFound(String),
}

impl fmt::Display for InstructionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "{field} 不能为空"),
            Self::IndefiniteRevision { field, value } => {
                write!(f, "{field} 必须是确定修订，收到 {value}")
            }
            Self::InvalidContentDigest => write!(f, "content_digest 必须是 64 位十六进制"),
            Self::BaseMismatch {
                path,
                expected,
                actual,
            } => write!(f, "{path} 基础版本冲突：期望 {expected}，实际 {actual}"),
            Self::ConcurrentRole {
                path,
                holder,
                requester,
            } => write!(f, "{path} 正由 {holder} 修订，{requester} 不能并发覆盖"),
            Self::NotFound(path) => write!(f, "未找到共享条目 {path}"),
        }
    }
}

impl std::error::Error for InstructionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionProposal {
    pub path: String,
    /// 必须指向当前 revision 或初始哨兵 `genesis`，不接受 HEAD/latest。
    pub base_revision: String,
    /// 新正文的 sha256 十六进制；由调用方对确定字节计算。
    pub content_digest: String,
    pub author_role: String,
    pub decided_by: DomainId,
    pub decided_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionRevision {
    pub path: String,
    pub revision: String,
    pub base_revision: String,
    pub content_digest: String,
    pub author_role: String,
    pub decided_by: DomainId,
    pub decided_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedRevision {
    pub path: String,
    pub base_revision: String,
    pub author_role: String,
    pub reason: InstructionError,
}

/// 共享指令/规则的串行修订账本：一次只接受一个 base，冲突候选留痕且不覆盖。
#[derive(Debug, Default)]
pub struct SharedInstructionLedger {
    records: BTreeMap<String, InstructionRevision>,
    rejected: Vec<RejectedRevision>,
}

impl SharedInstructionLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self, path: &str) -> Option<&InstructionRevision> {
        self.records.get(path)
    }

    pub fn rejected(&self) -> &[RejectedRevision] {
        &self.rejected
    }

    /// 共享指令文件：同一路径只许一个角色持有修订权（I-18），且 base 必须命中当前修订（I-20）。
    pub fn propose_instruction(
        &mut self,
        proposal: RevisionProposal,
    ) -> Result<InstructionRevision, InstructionError> {
        self.check_fields(&proposal)?;
        if let Some(holder) = self.records.get(&proposal.path)
            && holder.author_role != proposal.author_role
        {
            let rejection = RejectedRevision {
                path: proposal.path.clone(),
                base_revision: proposal.base_revision.clone(),
                author_role: proposal.author_role.clone(),
                reason: InstructionError::ConcurrentRole {
                    path: proposal.path.clone(),
                    holder: holder.author_role.clone(),
                    requester: proposal.author_role.clone(),
                },
            };
            let error = rejection.reason.clone();
            self.rejected.push(rejection);
            return Err(error);
        }
        self.propose(proposal)
    }

    /// 验证规则修订：允许不同角色排队，但每次都必须基于当前确定修订（I-20）。
    pub fn propose_rule_revision(
        &mut self,
        proposal: RevisionProposal,
    ) -> Result<InstructionRevision, InstructionError> {
        self.check_fields(&proposal)?;
        self.propose(proposal)
    }

    fn check_fields(&self, proposal: &RevisionProposal) -> Result<(), InstructionError> {
        if proposal.path.trim().is_empty() {
            return Err(InstructionError::EmptyField("path"));
        }
        if proposal.author_role.trim().is_empty() {
            return Err(InstructionError::EmptyField("author_role"));
        }
        if proposal.content_digest.len() != 64
            || !proposal
                .content_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(InstructionError::InvalidContentDigest);
        }
        if proposal.base_revision.eq_ignore_ascii_case("genesis") {
            return Ok(());
        }
        const MUTABLE: &[&str] = &["head", "latest", "main", "master", "working"];
        if MUTABLE
            .iter()
            .any(|value| proposal.base_revision.eq_ignore_ascii_case(value))
        {
            return Err(InstructionError::IndefiniteRevision {
                field: "base_revision",
                value: proposal.base_revision.clone(),
            });
        }
        Ok(())
    }

    fn propose(
        &mut self,
        proposal: RevisionProposal,
    ) -> Result<InstructionRevision, InstructionError> {
        let actual = self
            .records
            .get(&proposal.path)
            .map(|record| record.revision.as_str())
            .unwrap_or("genesis");
        if actual != proposal.base_revision {
            let rejection = RejectedRevision {
                path: proposal.path.clone(),
                base_revision: proposal.base_revision.clone(),
                author_role: proposal.author_role.clone(),
                reason: InstructionError::BaseMismatch {
                    path: proposal.path.clone(),
                    expected: actual.to_string(),
                    actual: proposal.base_revision.clone(),
                },
            };
            let error = rejection.reason.clone();
            self.rejected.push(rejection);
            return Err(error);
        }
        let revision = InstructionRevision {
            path: proposal.path.clone(),
            revision: format!("ir-{}", proposal.content_digest),
            base_revision: proposal.base_revision.clone(),
            content_digest: proposal.content_digest.clone(),
            author_role: proposal.author_role.clone(),
            decided_by: proposal.decided_by,
            decided_at_ms: proposal.decided_at_ms,
        };
        self.records.insert(proposal.path.clone(), revision.clone());
        Ok(revision)
    }
}

// ---------------------------------------------------------------------------
// 真实仓库候选与本地验证报告
// ---------------------------------------------------------------------------

fn git_output(repo: &Path, args: &[&str]) -> Result<String, CodingIntegrationError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !output.status.success() {
        return Err(CodingIntegrationError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 解析到确定的 40 位提交号；指针（HEAD/main/latest）不能作为交付修订。
pub fn resolve_head(repo: &Path) -> Result<String, CodingIntegrationError> {
    let commit = git_output(repo, &["rev-parse", "HEAD"])?;
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CodingIntegrationError::InvalidReport(
            "git HEAD 不是确定提交",
        ));
    }
    Ok(commit.to_ascii_lowercase())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryBaseline {
    pub artifact_id: DomainId,
    pub work_package_revision: DomainId,
    pub spec_revision: DomainId,
    pub rules_revision: DomainId,
    pub environment: DomainId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryCandidate {
    pub artifact_id: DomainId,
    pub revision_id: DomainId,
    pub commit: String,
    pub binding: AcceptanceBinding,
    pub produced_by: DomainId,
    pub digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVerificationReport {
    pub report_id: DomainId,
    pub commit: String,
    pub rules_revision: DomainId,
    pub environment: DomainId,
    pub actor: DomainId,
    pub checks: Vec<String>,
    pub passed: bool,
    pub observed_at_ms: i64,
    pub digest: Digest,
    pub evidence_refs: Vec<String>,
}

impl LocalVerificationReport {
    /// 报告正文由确定字段规范化后摘要；checks 排序，避免同样结果产生不同报告。
    pub fn new(
        commit: impl Into<String>,
        rules_revision: DomainId,
        environment: DomainId,
        actor: DomainId,
        mut checks: Vec<String>,
        passed: bool,
        observed_at_ms: i64,
    ) -> Result<Self, CodingIntegrationError> {
        let commit = commit.into().to_ascii_lowercase();
        if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CodingIntegrationError::InvalidReport(
                "提交号不是 40 位十六进制",
            ));
        }
        if checks.is_empty() {
            return Err(CodingIntegrationError::InvalidReport("没有实际验证项"));
        }
        checks.sort();
        checks.dedup();
        let canonical = format!(
            "bod.local-verification/v1\ncommit:{commit}\nrules:{}\nenvironment:{}\nactor:{}\nchecks:{}\npassed:{passed}\nat:{observed_at_ms}",
            rules_revision.as_str(),
            environment.as_str(),
            actor.as_str(),
            checks.join(",")
        );
        let digest = Digest::of_bytes(canonical.as_bytes());
        Ok(Self {
            report_id: DomainId::new(format!("lvr-{}", &digest.value[..16])),
            commit,
            rules_revision,
            environment,
            actor,
            checks,
            passed,
            observed_at_ms,
            digest: digest.clone(),
            evidence_refs: vec![format!("sha256:{}", digest.value)],
        })
    }

    pub fn matches(
        &self,
        candidate: &DeliveryCandidate,
        rules: &DomainId,
        environment: &DomainId,
    ) -> bool {
        self.commit == candidate.commit
            && self.rules_revision == *rules
            && self.environment == *environment
    }
}

/// 把真实仓库 HEAD 登记为候选：外部引用 + Pending 验证，绝不因实现结束而接受。
#[allow(clippy::too_many_arguments)]
pub fn register_git_candidate(
    store: &mut SqliteStore,
    company_id: &str,
    repo: &Path,
    baseline: &DeliveryBaseline,
    produced_by: &DomainId,
    now_ms: i64,
) -> Result<DeliveryCandidate, CodingIntegrationError> {
    let commit = resolve_head(repo)?;
    let artifact = Artifact {
        id: baseline.artifact_id.clone(),
        company_id: DomainId::new(company_id),
        project_id: DomainId::new("project-local"),
        kind: ArtifactKind::SourceCode,
        title: format!("真实仓库交付 {}", &commit[..12]),
        owner: produced_by.clone(),
        visibility: ArtifactVisibility::Internal,
        accepted_revision: None,
    };
    ensure_artifact(store, &artifact, now_ms)?;

    let revision_id = DomainId::new(format!("git-{commit}"));
    let digest = Digest::of_bytes(format!("git-commit:{commit}").as_bytes());
    let locator = format!("git:{}#{commit}", repo.display());
    let source_note = format!("sha256(git-commit:{commit})");
    let revision = register_external_revision(
        store,
        company_id,
        &baseline.artifact_id,
        &revision_id,
        None,
        &locator,
        digest.clone(),
        commit.len() as i64,
        "application/vnd.bod.git-commit",
        produced_by,
        &source_note,
        now_ms,
    )?;
    let record = VerificationRecord {
        revision_id: revision_id.clone(),
        state: VerificationState::Pending,
        actor: produced_by.clone(),
        reason: "实现完成只产生候选".to_string(),
        evidence_refs: Vec::new(),
        verification_rules: baseline.rules_revision.clone(),
        environment: baseline.environment.clone(),
        observed_at_ms: now_ms,
    };
    let uow = store.transaction()?;
    uow.insert_verification_record(
        &format!("vr-{}-pending", revision_id.as_str()),
        company_id,
        &record,
    )?;
    uow.commit()?;

    Ok(DeliveryCandidate {
        artifact_id: baseline.artifact_id.clone(),
        revision_id,
        commit,
        binding: AcceptanceBinding {
            work_package_revision: baseline.work_package_revision.clone(),
            input_baseline: baseline.spec_revision.clone(),
            artifact_revision: revision.revision_id,
            verification_rules: baseline.rules_revision.clone(),
            target_environment: baseline.environment.clone(),
            merge_combination: None,
        },
        produced_by: produced_by.clone(),
        digest,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationOutcome {
    /// 报告与提交/规则/环境匹配，且由独立执行者出具。
    Verified(VerificationRecord),
    /// 报告缺失、自报或与候选不匹配：只标 NeedsRevision，不接受。
    NeedsRevalidation(VerificationRecord),
}

/// 写入本地验证结果。自报通过不算独立证据；报告不匹配不推进到 Verified。
pub fn record_local_verification(
    store: &mut SqliteStore,
    company_id: &str,
    candidate: &DeliveryCandidate,
    report: Option<&LocalVerificationReport>,
    verifier: &DomainId,
    now_ms: i64,
) -> Result<VerificationOutcome, CodingIntegrationError> {
    let rules = &candidate.binding.verification_rules;
    let environment = &candidate.binding.target_environment;
    let (state, reason, evidence) = match report {
        Some(report)
            if report.matches(candidate, rules, environment)
                && report.passed
                && report.actor != candidate.produced_by =>
        {
            (
                VerificationState::Verified,
                format!("本地验证报告 {}", report.report_id.as_str()),
                report.evidence_refs.clone(),
            )
        }
        Some(report) => (
            VerificationState::NeedsRevision,
            format!("报告不匹配或非独立验证：{}", report.report_id.as_str()),
            report.evidence_refs.clone(),
        ),
        None => (
            VerificationState::NeedsRevision,
            "执行端自报通过但没有绑定提交的验证报告".to_string(),
            Vec::new(),
        ),
    };
    begin_verification(
        store,
        company_id,
        &candidate.revision_id,
        verifier,
        rules,
        environment,
        now_ms,
    )?;
    let record = complete_verification(
        store,
        company_id,
        &candidate.revision_id,
        state,
        verifier,
        &reason,
        rules,
        environment,
        evidence,
        now_ms + 1,
    )?;
    Ok(if record.state == VerificationState::Verified {
        VerificationOutcome::Verified(record)
    } else {
        VerificationOutcome::NeedsRevalidation(record)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryAcceptance {
    Accepted(crate::domain::artifact::AcceptanceRecord),
    AutoAccepted(crate::domain::artifact::AcceptanceRecord),
    RequiresHuman,
    /// 报告缺失/未验证/不匹配：保持 Pending 或 NeedsRevalidation，不写接受记录。
    Blocked(&'static str),
}

/// 接受：人工主体或预授权策略，且只消费绑定本次提交/规格/规则/环境的报告。
#[allow(clippy::too_many_arguments)]
pub fn accept_delivery(
    store: &mut SqliteStore,
    company_id: &str,
    candidate: &DeliveryCandidate,
    report: Option<&LocalVerificationReport>,
    actor: &Principal,
    policy: &AcceptancePolicy,
    acceptance_id: &DomainId,
    now_ms: i64,
) -> Result<DeliveryAcceptance, CodingIntegrationError> {
    let Some(report) = report else {
        return Ok(DeliveryAcceptance::Blocked("没有本地验证报告"));
    };
    if !report.matches(
        candidate,
        &candidate.binding.verification_rules,
        &candidate.binding.target_environment,
    ) {
        return Ok(DeliveryAcceptance::Blocked(
            "报告绑定的提交/规则/环境与候选不一致",
        ));
    }
    let verification = {
        let uow = store.transaction()?;
        uow.latest_verification(candidate.revision_id.as_str())?
            .ok_or(CodingIntegrationError::NotFound("验证记录"))?
    };
    if verification.state != VerificationState::Verified {
        return Ok(DeliveryAcceptance::Blocked("验证未通过或已失效"));
    }
    let request = AcceptRequest {
        acceptance_id: acceptance_id.clone(),
        binding: candidate.binding.clone(),
        checks_passed: report.passed && verification.state.is_verified(),
        blocking_risks: Vec::new(),
        reason: format!("绑定报告 {}", report.report_id.as_str()),
        evidence_refs: report.evidence_refs.clone(),
    };
    Ok(
        match accept(
            store,
            company_id,
            &candidate.artifact_id,
            &request,
            actor,
            policy,
            now_ms,
        )? {
            AcceptOutcome::Accepted(record) => DeliveryAcceptance::Accepted(record),
            AcceptOutcome::AutoAccepted(record) => DeliveryAcceptance::AutoAccepted(record),
            AcceptOutcome::RequiresHuman => DeliveryAcceptance::RequiresHuman,
        },
    )
}

/// 重启后可查询的交付快照：只从宿主库事实计算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverySnapshot {
    pub acceptance_id: DomainId,
    pub artifact_id: DomainId,
    pub binding: AcceptanceBinding,
    pub decision: AcceptanceDecision,
    pub evidence_refs: Vec<String>,
    pub storage_ref: String,
    pub produced_by: DomainId,
}

pub fn load_delivery(
    store: &mut SqliteStore,
    artifact_id: &DomainId,
) -> Result<Option<DeliverySnapshot>, CodingIntegrationError> {
    let Some(acceptance) = current_acceptance(store, artifact_id)? else {
        return Ok(None);
    };
    let revision = store
        .transaction()?
        .get_artifact_revision(acceptance.binding.artifact_revision.as_str())?
        .ok_or(CodingIntegrationError::NotFound("交付修订"))?;
    Ok(Some(DeliverySnapshot {
        acceptance_id: acceptance.acceptance_id,
        artifact_id: acceptance.artifact_id,
        binding: acceptance.binding,
        decision: acceptance.decision,
        evidence_refs: acceptance.evidence_refs,
        storage_ref: revision.storage.as_str().to_string(),
        produced_by: revision.produced_by,
    }))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::model::PrincipalKind;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW: i64 = 1_763_600_000_000;

    struct Fixture {
        _root: PathBuf,
        repo: PathBuf,
        db: PathBuf,
        first_commit: String,
    }

    impl Fixture {
        fn new() -> Self {
            let seq = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            let root =
                std::env::temp_dir().join(format!("bod-coding-{}-{seq}", std::process::id()));
            let repo = root.join("repo");
            fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            fs::write(repo.join("hello.txt"), "first\n").unwrap();
            git(&repo, &["add", "hello.txt"]);
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
            let first_commit = git(&repo, &["rev-parse", "HEAD"]);
            let db = root.join("bod.sqlite");
            Self {
                _root: root,
                repo,
                db,
                first_commit,
            }
        }

        fn store(&self) -> SqliteStore {
            SqliteStore::open(&self.db, &BodConfig::default()).unwrap()
        }

        fn second_commit(&self) -> String {
            fs::write(self.repo.join("hello.txt"), "second\n").unwrap();
            git(&self.repo, &["add", "hello.txt"]);
            git(
                &self.repo,
                &[
                    "-c",
                    "user.name=bod",
                    "-c",
                    "user.email=bod@local",
                    "commit",
                    "-q",
                    "-m",
                    "second",
                ],
            );
            git(&self.repo, &["rev-parse", "HEAD"])
        }

        fn baseline() -> DeliveryBaseline {
            DeliveryBaseline {
                artifact_id: DomainId::new("delivery-1"),
                work_package_revision: DomainId::new("wp-r1"),
                spec_revision: DomainId::new("spec-r7"),
                rules_revision: DomainId::new("rules-r3"),
                environment: DomainId::new("env-local"),
            }
        }
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn digest(value: &str) -> String {
        Digest::of_bytes(value.as_bytes()).value
    }

    fn proposal(path: &str, base: &str, body: &str, role: &str) -> RevisionProposal {
        RevisionProposal {
            path: path.to_string(),
            base_revision: base.to_string(),
            content_digest: digest(body),
            author_role: role.to_string(),
            decided_by: DomainId::new("decision-1"),
            decided_at_ms: NOW,
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
            claimed_roles: vec!["reviewer".to_string()],
        }
    }

    fn report(commit: &str, actor: &str, checks: &[&str], passed: bool) -> LocalVerificationReport {
        LocalVerificationReport::new(
            commit,
            DomainId::new("rules-r3"),
            DomainId::new("env-local"),
            DomainId::new(actor),
            checks.iter().map(|value| value.to_string()).collect(),
            passed,
            NOW + 5,
        )
        .unwrap()
    }

    #[test]
    fn invariant_i_18_different_roles_cannot_concurrently_overwrite_shared_instructions() {
        let mut ledger = SharedInstructionLedger::new();
        let first = ledger
            .propose_instruction(proposal("AGENTS.md", "genesis", "v1", "architect"))
            .unwrap();
        let conflict = ledger
            .propose_instruction(proposal("AGENTS.md", &first.revision, "v2", "implementer"))
            .unwrap_err();
        assert!(matches!(
            conflict,
            InstructionError::ConcurrentRole { ref holder, .. } if holder == "architect"
        ));
        assert_eq!(ledger.current("AGENTS.md").unwrap(), &first);
        assert_eq!(ledger.rejected().len(), 1);
        assert_eq!(
            ledger.rejected()[0].reason,
            InstructionError::ConcurrentRole {
                path: "AGENTS.md".to_string(),
                holder: "architect".to_string(),
                requester: "implementer".to_string(),
            }
        );
        // 持有人在正确 base 上继续修订，不受被拒候选影响。
        let second = ledger
            .propose_instruction(proposal("AGENTS.md", &first.revision, "v2", "architect"))
            .unwrap();
        assert_eq!(second.base_revision, first.revision);
        assert_ne!(second.revision, first.revision);
    }

    #[test]
    fn invariant_i_20_concurrent_rule_revisions_check_base_revision() {
        let mut ledger = SharedInstructionLedger::new();
        let first = ledger
            .propose_rule_revision(proposal("verification/rules", "genesis", "r1", "qa"))
            .unwrap();
        let stale = ledger
            .propose_rule_revision(proposal("verification/rules", "genesis", "r2", "architect"))
            .unwrap_err();
        assert!(matches!(
            stale,
            InstructionError::BaseMismatch { ref expected, .. } if expected == &first.revision
        ));
        assert_eq!(ledger.current("verification/rules").unwrap(), &first);
        let second = ledger
            .propose_rule_revision(proposal(
                "verification/rules",
                &first.revision,
                "r2",
                "architect",
            ))
            .unwrap();
        assert_eq!(second.base_revision, first.revision);
        assert!(
            ledger
                .propose_rule_revision(proposal("verification/rules", "HEAD", "r3", "qa"))
                .is_err()
        );
    }

    #[test]
    fn git_candidate_binds_exact_head_and_rejects_mutable_pointer() {
        let fixture = Fixture::new();
        assert_eq!(resolve_head(&fixture.repo).unwrap(), fixture.first_commit);
        let pointer = LocalVerificationReport::new(
            "HEAD",
            DomainId::new("rules-r3"),
            DomainId::new("env-local"),
            DomainId::new("verifier-1"),
            vec!["cargo test".to_string()],
            true,
            NOW,
        );
        assert!(matches!(
            pointer,
            Err(CodingIntegrationError::InvalidReport(_))
        ));

        let mut store = fixture.store();
        let candidate = register_git_candidate(
            &mut store,
            "company-default",
            &fixture.repo,
            &Fixture::baseline(),
            &DomainId::new("coder-1"),
            NOW,
        )
        .unwrap();
        assert_eq!(candidate.commit, fixture.first_commit);
        assert_eq!(candidate.binding.input_baseline.as_str(), "spec-r7");
        assert_eq!(
            store
                .transaction()
                .unwrap()
                .get_artifact_revision(candidate.revision_id.as_str())
                .unwrap()
                .unwrap()
                .storage
                .as_str(),
            format!("git:{}#{}", fixture.repo.display(), fixture.first_commit)
        );

        // 新提交产生新候选修订；旧接受不得批准新提交。
        let second = fixture.second_commit();
        let next = register_git_candidate(
            &mut store,
            "company-default",
            &fixture.repo,
            &Fixture::baseline(),
            &DomainId::new("coder-1"),
            NOW + 2,
        )
        .unwrap();
        assert_eq!(next.commit, second);
        assert_ne!(next.revision_id, candidate.revision_id);
    }

    #[test]
    fn self_reported_pass_without_report_keeps_acceptance_pending() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let candidate = register_git_candidate(
            &mut store,
            "company-default",
            &fixture.repo,
            &Fixture::baseline(),
            &DomainId::new("coder-1"),
            NOW,
        )
        .unwrap();
        let blocked = accept_delivery(
            &mut store,
            "company-default",
            &candidate,
            None,
            &user("user-1"),
            &AcceptancePolicy::default(),
            &DomainId::new("acc-1"),
            NOW + 10,
        )
        .unwrap();
        assert_eq!(blocked, DeliveryAcceptance::Blocked("没有本地验证报告"));
        assert!(
            load_delivery(&mut store, &candidate.artifact_id)
                .unwrap()
                .is_none()
        );

        let outcome = record_local_verification(
            &mut store,
            "company-default",
            &candidate,
            None,
            &DomainId::new("verifier-1"),
            NOW + 10,
        )
        .unwrap();
        assert!(matches!(outcome, VerificationOutcome::NeedsRevalidation(_)));
        // 即便随后补上一份格式正确的报告，已落库的 NeedsRevision 也不能直接放行；
        // 必须重新走本地验证并再次得到 Verified。
        let later_report = report(&candidate.commit, "verifier-1", &["cargo test"], true);
        let blocked = accept_delivery(
            &mut store,
            "company-default",
            &candidate,
            Some(&later_report),
            &user("user-1"),
            &AcceptancePolicy::default(),
            &DomainId::new("acc-1"),
            NOW + 11,
        )
        .unwrap();
        assert_eq!(blocked, DeliveryAcceptance::Blocked("验证未通过或已失效"));
    }

    #[test]
    fn self_authored_report_is_not_independent_evidence() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let candidate = register_git_candidate(
            &mut store,
            "company-default",
            &fixture.repo,
            &Fixture::baseline(),
            &DomainId::new("coder-1"),
            NOW,
        )
        .unwrap();
        let self_report = report(&candidate.commit, "coder-1", &["cargo test"], true);
        let outcome = record_local_verification(
            &mut store,
            "company-default",
            &candidate,
            Some(&self_report),
            &DomainId::new("coder-1"),
            NOW + 10,
        )
        .unwrap();
        assert!(matches!(outcome, VerificationOutcome::NeedsRevalidation(_)));
        let blocked = accept_delivery(
            &mut store,
            "company-default",
            &candidate,
            Some(&self_report),
            &user("user-1"),
            &AcceptancePolicy::default(),
            &DomainId::new("acc-1"),
            NOW + 11,
        )
        .unwrap();
        assert_eq!(blocked, DeliveryAcceptance::Blocked("验证未通过或已失效"));
    }

    #[test]
    fn stale_report_is_rejected_even_when_checks_passed() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let candidate = register_git_candidate(
            &mut store,
            "company-default",
            &fixture.repo,
            &Fixture::baseline(),
            &DomainId::new("coder-1"),
            NOW,
        )
        .unwrap();
        let _ = fixture.second_commit();
        let stale = report(&fixture.first_commit, "verifier-1", &["cargo test"], true);
        let blocked = accept_delivery(
            &mut store,
            "company-default",
            &candidate,
            Some(&stale),
            &user("user-1"),
            &AcceptancePolicy::default(),
            &DomainId::new("acc-1"),
            NOW + 10,
        )
        .unwrap();
        // 报告本身匹配候选，但未完成验证时仍不得接受。
        assert_eq!(blocked, DeliveryAcceptance::Blocked("验证未通过或已失效"));

        let mismatch = LocalVerificationReport::new(
            fixture.first_commit.clone(),
            DomainId::new("rules-OTHER"),
            DomainId::new("env-local"),
            DomainId::new("verifier-1"),
            vec!["cargo test".to_string()],
            true,
            NOW,
        )
        .unwrap();
        assert_eq!(
            accept_delivery(
                &mut store,
                "company-default",
                &candidate,
                Some(&mismatch),
                &user("user-1"),
                &AcceptancePolicy::default(),
                &DomainId::new("acc-1"),
                NOW + 10,
            )
            .unwrap(),
            DeliveryAcceptance::Blocked("报告绑定的提交/规则/环境与候选不一致")
        );
    }

    #[test]
    fn human_acceptance_binds_commit_spec_report_and_survives_restart() {
        let fixture = Fixture::new();
        let baseline = Fixture::baseline();
        let report = report(
            &fixture.first_commit,
            "verifier-1",
            &["cargo test", "fmt"],
            true,
        );
        let (artifact_id, revision_id) = {
            let mut store = fixture.store();
            let candidate = register_git_candidate(
                &mut store,
                "company-default",
                &fixture.repo,
                &baseline,
                &DomainId::new("coder-1"),
                NOW,
            )
            .unwrap();
            record_local_verification(
                &mut store,
                "company-default",
                &candidate,
                Some(&report),
                &DomainId::new("verifier-1"),
                NOW + 2,
            )
            .unwrap();
            let accepted = accept_delivery(
                &mut store,
                "company-default",
                &candidate,
                Some(&report),
                &user("user-1"),
                &AcceptancePolicy::default(),
                &DomainId::new("acc-1"),
                NOW + 10,
            )
            .unwrap();
            assert!(matches!(accepted, DeliveryAcceptance::Accepted(_)));
            (candidate.artifact_id, candidate.revision_id)
        };

        // 模拟桌面关闭/重启：丢弃 store，从同一 SQLite 文件重开。
        let mut reopened = fixture.store();
        let snapshot = load_delivery(&mut reopened, &artifact_id)
            .unwrap()
            .expect("重启后仍可查询交付");
        assert_eq!(snapshot.binding.artifact_revision, revision_id);
        assert_eq!(snapshot.binding.input_baseline, baseline.spec_revision);
        assert_eq!(
            snapshot.binding.work_package_revision,
            baseline.work_package_revision
        );
        assert_eq!(snapshot.binding.verification_rules, baseline.rules_revision);
        assert_eq!(snapshot.binding.target_environment, baseline.environment);
        assert_eq!(snapshot.decision, AcceptanceDecision::Accepted);
        assert_eq!(snapshot.evidence_refs, report.evidence_refs);
        assert!(snapshot.storage_ref.contains(&fixture.first_commit));
        assert_eq!(snapshot.produced_by.as_str(), "coder-1");
    }

    #[test]
    fn preauthorized_agent_acceptance_requires_policy_and_report() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let candidate = register_git_candidate(
            &mut store,
            "company-default",
            &fixture.repo,
            &Fixture::baseline(),
            &DomainId::new("coder-1"),
            NOW,
        )
        .unwrap();
        let verified_report = report(&candidate.commit, "verifier-1", &["cargo test"], true);
        record_local_verification(
            &mut store,
            "company-default",
            &candidate,
            Some(&verified_report),
            &DomainId::new("verifier-1"),
            NOW + 2,
        )
        .unwrap();

        let strict = accept_delivery(
            &mut store,
            "company-default",
            &candidate,
            Some(&verified_report),
            &agent("reviewer-agent"),
            &AcceptancePolicy::default(),
            &DomainId::new("acc-1"),
            NOW + 10,
        )
        .unwrap();
        assert_eq!(strict, DeliveryAcceptance::RequiresHuman);

        let preauthorized = AcceptancePolicy {
            auto_accept_enabled: true,
            required_checks: 1,
            high_risk: false,
            production_write: false,
        };
        let accepted = accept_delivery(
            &mut store,
            "company-default",
            &candidate,
            Some(&verified_report),
            &agent("reviewer-agent"),
            &preauthorized,
            &DomainId::new("acc-2"),
            NOW + 11,
        )
        .unwrap();
        assert!(matches!(accepted, DeliveryAcceptance::AutoAccepted(_)));
        let snapshot = load_delivery(&mut store, &candidate.artifact_id)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.decision, AcceptanceDecision::AutoAccepted);
        assert_eq!(snapshot.acceptance_id.as_str(), "acc-2");

        // 实现者不得以 Agent 身份自批，即便策略允许自动接受。
        let second = fixture.second_commit();
        let next = register_git_candidate(
            &mut store,
            "company-default",
            &fixture.repo,
            &Fixture::baseline(),
            &DomainId::new("coder-1"),
            NOW + 12,
        )
        .unwrap();
        assert_eq!(next.commit, second);
        let self_report = report(&next.commit, "verifier-1", &["cargo test"], true);
        record_local_verification(
            &mut store,
            "company-default",
            &next,
            Some(&self_report),
            &DomainId::new("verifier-1"),
            NOW + 13,
        )
        .unwrap();
        let denied = accept_delivery(
            &mut store,
            "company-default",
            &next,
            Some(&self_report),
            &agent("coder-1"),
            &preauthorized,
            &DomainId::new("acc-3"),
            NOW + 14,
        );
        assert!(matches!(
            denied,
            Err(CodingIntegrationError::Acceptance(
                AcceptanceStoreError::Domain(
                    crate::domain::artifact::AcceptanceError::SelfApprovalDenied { .. }
                )
            ))
        ));
    }

    #[test]
    fn report_digest_is_deterministic_and_order_independent() {
        let left = report(
            &"a".repeat(40),
            "verifier-1",
            &["cargo test", "fmt", "clippy"],
            true,
        );
        let right = report(
            &"a".repeat(40),
            "verifier-1",
            &["clippy", "fmt", "cargo test"],
            true,
        );
        assert_eq!(left.digest, right.digest);
        assert_eq!(left.report_id, right.report_id);
        assert_eq!(left.checks, vec!["cargo test", "clippy", "fmt"]);
        let changed = report(
            &"a".repeat(40),
            "verifier-1",
            &["cargo test", "fmt", "clippy"],
            false,
        );
        assert_ne!(left.digest, changed.digest);
    }
}
