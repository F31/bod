//! P1 切片 4.1：首次使用路径与自动最小工作约定。
//!
//! 权威语义：§1.11「选项目或仓库 → 说清目标 → 查看范围和验收 → 交办」，
//! §2.1「入口简单：不强制 Team、WorkPackage、Run 和组织层级」，
//! §39.2 I-10「合并证据对应实际目标与候选组合」。
//!
//! 本模块只从用户给出的 Project + 已接入 Agent + 真实仓库派生最小工作约定，
//! 自动补齐 Issue / 工作包修订 / Run / Attempt / 约定事件，并登记真实 git 候选；
//! 接受证据必须绑定同一目标与候选组合，跨目标或跨提交证据不能合并。
//! 不拉起 Agent、不创建 Team 配置、不写第二个业务库。

use crate::artifact_acceptance::{AcceptOutcome, AcceptRequest, AcceptanceStoreError, accept};
use crate::coding_integration::{
    CodingIntegrationError, DeliveryBaseline, DeliveryCandidate, LocalVerificationReport,
    VerificationOutcome, record_local_verification, register_git_candidate_in_project,
};
use crate::domain::artifact::{AcceptanceBinding, AcceptancePolicy, AcceptanceRecord, Digest};
use crate::domain::model::{
    AcceptanceCriteria, Assignment, Issue, IssueKind, IssueSpec, Owner, OwnerKind, Phase,
    Principal, Project, RepositoryContext, ResourceMeta, ResourceStatus, Scope,
};
use crate::domain::{DomainId, Version};
use crate::store::{SqliteStore, StoreError};
use crate::workspace_safety::{SHARED_CONFIG_PATHS, WriteScope};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// 约定格式版本：格式变化必须换版本，避免同输入被静默解释成不同目标。
pub const AGREEMENT_VERSION: &str = "bod.first-use/1";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum FirstUseError {
    EmptyTitle,
    EmptyAcceptance,
    BlankAcceptance { index: usize },
    DuplicateAcceptance { value: String },
    AgentNotConnected { agent_id: String },
    RepositoryNotReady { path: String },
    EmptyWriteScope,
    Io(std::io::Error),
    Git(CodingIntegrationError),
    Store(StoreError),
    Acceptance(AcceptanceStoreError),
    EvidenceCombinationMismatch { field: &'static str },
    AlreadyDispatched { issue_uid: String },
}

impl fmt::Display for FirstUseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTitle => write!(f, "目标不能为空"),
            Self::EmptyAcceptance => write!(f, "至少给出一条验收条件"),
            Self::BlankAcceptance { index } => write!(f, "第 {index} 条验收条件为空"),
            Self::DuplicateAcceptance { value } => write!(f, "验收条件重复：{value}"),
            Self::AgentNotConnected { agent_id } => write!(f, "Agent 未接入：{agent_id}"),
            Self::RepositoryNotReady { path } => write!(f, "仓库不可用作首次派发：{path}"),
            Self::EmptyWriteScope => write!(f, "仓库没有可写目录，无法生成最小工作约定"),
            Self::Io(err) => write!(f, "首次派发 IO 失败：{err}"),
            Self::Git(err) => write!(f, "真实仓库绑定失败：{err}"),
            Self::Store(err) => write!(f, "宿主存储失败：{err}"),
            Self::Acceptance(err) => write!(f, "接受门禁失败：{err}"),
            Self::EvidenceCombinationMismatch { field } => {
                write!(f, "证据与目标/候选组合不一致：{field}")
            }
            Self::AlreadyDispatched { issue_uid } => {
                write!(f, "该目标已首次派发，issue={issue_uid}")
            }
        }
    }
}

impl From<std::io::Error> for FirstUseError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<StoreError> for FirstUseError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<AcceptanceStoreError> for FirstUseError {
    fn from(err: AcceptanceStoreError) -> Self {
        Self::Acceptance(err)
    }
}

impl From<CodingIntegrationError> for FirstUseError {
    fn from(err: CodingIntegrationError) -> Self {
        Self::Git(err)
    }
}

// ---------------------------------------------------------------------------
// 输入与最小工作约定
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstUseInput {
    pub project: Project,
    pub agent_id: DomainId,
    /// 真实本机 git 仓库；首次使用不从日志猜地址。
    pub repository: PathBuf,
    pub title: String,
    /// 用户逐字给出的验收条件，不在这里改写或推断。
    pub acceptance: Vec<String>,
}

impl FirstUseInput {
    fn canonical_repository(&self) -> Result<PathBuf, FirstUseError> {
        if !self.repository.is_dir() || !self.repository.join(".git").exists() {
            return Err(FirstUseError::RepositoryNotReady {
                path: self.repository.display().to_string(),
            });
        }
        fs::canonicalize(&self.repository).map_err(FirstUseError::Io)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstUsePlan {
    pub agreement_id: DomainId,
    pub artifact_id: DomainId,
    pub issue_id: DomainId,
    pub work_package_revision: DomainId,
    pub spec_revision: DomainId,
    pub acceptance_revision: DomainId,
    pub run_id: DomainId,
    pub attempt_id: DomainId,
    pub rules_revision: DomainId,
    pub environment: DomainId,
    pub write_scope: WriteScope,
    pub goal_digest: Digest,
    pub title: String,
    pub acceptance: Vec<String>,
}

impl FirstUsePlan {
    /// 证据合并组合：目标（含验收）+ 候选提交 + 规则 + 环境一起定址。
    /// 任一变化即换组合，旧组合的证据不能用于新交付（I-10）。
    pub fn merge_combination(&self, commit: &str) -> DomainId {
        let canonical = format!(
            "{AGREEMENT_VERSION}\ngoal:{}\ncommit:{commit}\nrules:{}\nenvironment:{}",
            self.goal_digest.as_string(),
            self.rules_revision.as_str(),
            self.environment.as_str()
        );
        DomainId::new(format!(
            "merge-{}",
            &Digest::of_bytes(canonical.as_bytes()).value[..16]
        ))
    }
}

/// 只校验与预览，不写库：对应「查看范围与验收」这一步。
pub fn plan_first_use(
    input: &FirstUseInput,
    connected_agents: &[DomainId],
) -> Result<FirstUsePlan, FirstUseError> {
    let title = input.title.trim().to_string();
    if title.is_empty() {
        return Err(FirstUseError::EmptyTitle);
    }
    if input.acceptance.is_empty() {
        return Err(FirstUseError::EmptyAcceptance);
    }
    let mut criteria = Vec::with_capacity(input.acceptance.len());
    for (index, value) in input.acceptance.iter().enumerate() {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            return Err(FirstUseError::BlankAcceptance { index });
        }
        if criteria.contains(&trimmed) {
            return Err(FirstUseError::DuplicateAcceptance { value: trimmed });
        }
        criteria.push(trimmed);
    }
    if !connected_agents.contains(&input.agent_id) {
        return Err(FirstUseError::AgentNotConnected {
            agent_id: input.agent_id.as_str().to_string(),
        });
    }
    let repo = input.canonical_repository()?;
    let write_scope = default_write_scope(&repo)?;

    let mut sorted_criteria = criteria.clone();
    sorted_criteria.sort();
    let canonical = format!(
        "{AGREEMENT_VERSION}\ncompany:{}\nproject:{}\nagent:{}\nrepository:{}\ntitle:{}\nacceptance:{}",
        input.project.company_id.as_str(),
        input.project.id.as_str(),
        input.agent_id.as_str(),
        repo.display(),
        title,
        sorted_criteria.join("\u{1f}")
    );
    let goal_digest = Digest::of_bytes(canonical.as_bytes());
    let prefix = &goal_digest.value[..16];
    Ok(FirstUsePlan {
        agreement_id: DomainId::new(format!("agree-{prefix}")),
        artifact_id: DomainId::new(format!("delivery-{prefix}")),
        issue_id: DomainId::new(format!("issue-{prefix}")),
        work_package_revision: DomainId::new(format!("wp-{prefix}")),
        spec_revision: DomainId::new(format!("spec-{prefix}")),
        acceptance_revision: DomainId::new(format!("acc-{prefix}")),
        run_id: DomainId::new(format!("run-{prefix}")),
        attempt_id: DomainId::new(format!("attempt-{prefix}")),
        rules_revision: DomainId::new(format!("rules-{prefix}")),
        environment: DomainId::new("env-local"),
        write_scope,
        goal_digest,
        title,
        acceptance: criteria,
    })
}

/// 最小写范围：仓库顶层目录，共享配置入口一律不在 allow 中（仍走协调授权门禁）。
fn default_write_scope(repo: &Path) -> Result<WriteScope, FirstUseError> {
    let mut allow = Vec::new();
    for entry in fs::read_dir(repo)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(name.as_str(), ".git" | "target" | "node_modules") {
            continue;
        }
        let relative = if entry.file_type()?.is_dir() {
            format!("{name}/")
        } else {
            name.clone()
        };
        if SHARED_CONFIG_PATHS
            .iter()
            .any(|shared| shared.trim_end_matches('/') == relative.trim_end_matches('/'))
        {
            continue;
        }
        allow.push(relative);
    }
    allow.sort();
    allow.dedup();
    if allow.is_empty() {
        return Err(FirstUseError::EmptyWriteScope);
    }
    Ok(WriteScope::new(allow, Vec::new()))
}

// ---------------------------------------------------------------------------
// 交办与证据绑定
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstUseAssignment {
    pub plan: FirstUsePlan,
    pub project_id: DomainId,
    pub agent_id: DomainId,
    pub commit: String,
    pub merge_combination: DomainId,
    pub candidate: DeliveryCandidate,
    pub dispatched_at_ms: i64,
}

impl FirstUseAssignment {
    pub fn artifact_id(&self) -> &DomainId {
        &self.plan.artifact_id
    }
}

/// 交办：只从用户目标派生最小约定，不要求 Team/WorkPackage/Run 预配置。
pub fn dispatch_first_use(
    store: &mut SqliteStore,
    input: &FirstUseInput,
    connected_agents: &[DomainId],
    now_ms: i64,
) -> Result<FirstUseAssignment, FirstUseError> {
    let plan = plan_first_use(input, connected_agents)?;
    let repo = input.canonical_repository()?;
    let commit = crate::coding_integration::resolve_head(&repo)?.to_ascii_lowercase();
    let uow = store.transaction()?;
    if uow.get_issue(plan.issue_id.as_str())?.is_some() {
        return Err(FirstUseError::AlreadyDispatched {
            issue_uid: plan.issue_id.as_str().to_string(),
        });
    }
    drop(uow);

    let issue = Issue {
        meta: ResourceMeta {
            uid: plan.issue_id.clone(),
            company_id: input.project.company_id.clone(),
            project_id: Some(input.project.id.clone()),
            resource_version: Version::INITIAL,
            generation: 1,
            deletion_requested_at: None,
        },
        kind: IssueKind::Coding,
        repository: Some(RepositoryContext {
            provider: "local-git".to_string(),
            repository_id: repo.display().to_string(),
        }),
        spec: IssueSpec {
            title: plan.title.clone(),
            assignment: Some(Assignment {
                owner: Owner {
                    kind: OwnerKind::Agent,
                    id: input.agent_id.clone(),
                },
                scope: Scope {
                    company_id: input.project.company_id.clone(),
                    project_id: Some(input.project.id.clone()),
                    issue_id: Some(plan.issue_id.clone()),
                },
                grant_revision: 1,
            }),
            acceptance: AcceptanceCriteria {
                criteria: plan.acceptance.clone(),
            },
        },
        status: ResourceStatus {
            phase: Phase::Pending,
            observed_generation: 1,
            conditions: Vec::new(),
        },
    };

    let uow = store.transaction()?;
    uow.insert_issue(&issue)?;
    uow.insert_work_package_revision(
        plan.work_package_revision.as_str(),
        input.project.company_id.as_str(),
        plan.issue_id.as_str(),
        plan.spec_revision.as_str(),
        plan.acceptance_revision.as_str(),
        &format!("{now_ms}"),
    )?;
    uow.insert_run(
        plan.run_id.as_str(),
        input.project.company_id.as_str(),
        plan.issue_id.as_str(),
        plan.work_package_revision.as_str(),
        "Pending",
    )?;
    uow.insert_event(
        &format!("evt-first-use-{}", plan.issue_id.as_str()),
        input.project.company_id.as_str(),
        plan.issue_id.as_str(),
        1,
        &format!(
            "first-use-dispatched\nproject:{}\nagent:{}\ngoal:{}\ncommit:{commit}",
            input.project.id.as_str(),
            input.agent_id.as_str(),
            plan.goal_digest.as_string()
        ),
    )?;
    uow.commit()?;

    let baseline = DeliveryBaseline {
        artifact_id: plan.artifact_id.clone(),
        work_package_revision: plan.work_package_revision.clone(),
        spec_revision: plan.spec_revision.clone(),
        rules_revision: plan.rules_revision.clone(),
        environment: plan.environment.clone(),
    };
    let candidate = register_git_candidate_in_project(
        store,
        input.project.company_id.as_str(),
        input.project.id.as_str(),
        &repo,
        &baseline,
        &input.agent_id,
        now_ms,
    )?;
    let merge_combination = plan.merge_combination(&commit);
    Ok(FirstUseAssignment {
        plan,
        project_id: input.project.id.clone(),
        agent_id: input.agent_id.clone(),
        commit,
        merge_combination,
        candidate,
        dispatched_at_ms: now_ms,
    })
}

/// 合并证据前的前置门禁：报告必须命中同一目标、提交、规则、环境，且由非实现者出具。
/// 返回的绑定带合并组合 id，旧组合不得跨交付复用。
pub fn bind_first_use_evidence(
    assignment: &FirstUseAssignment,
    report: &LocalVerificationReport,
) -> Result<AcceptanceBinding, FirstUseError> {
    let plan = &assignment.plan;
    if report.commit != assignment.commit {
        return Err(FirstUseError::EvidenceCombinationMismatch { field: "commit" });
    }
    if report.rules_revision != plan.rules_revision {
        return Err(FirstUseError::EvidenceCombinationMismatch {
            field: "rules_revision",
        });
    }
    if report.environment != plan.environment {
        return Err(FirstUseError::EvidenceCombinationMismatch {
            field: "environment",
        });
    }
    if report.actor == assignment.agent_id {
        return Err(FirstUseError::EvidenceCombinationMismatch { field: "actor" });
    }
    let binding = &assignment.candidate.binding;
    if binding.work_package_revision != plan.work_package_revision {
        return Err(FirstUseError::EvidenceCombinationMismatch {
            field: "work_package_revision",
        });
    }
    if binding.input_baseline != plan.spec_revision {
        return Err(FirstUseError::EvidenceCombinationMismatch {
            field: "input_baseline",
        });
    }
    let mut binding = binding.clone();
    binding.merge_combination = Some(assignment.merge_combination.clone());
    Ok(binding)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstUseAcceptance {
    Accepted(Box<AcceptanceRecord>),
    RequiresHuman,
    /// 报告缺失/不匹配/未验证：保持候选状态，不写接受记录。
    Blocked(&'static str),
}

/// 首次使用的收口：验证报告 → 合并组合绑定 → 宿主接受决定。
pub fn accept_first_use(
    store: &mut SqliteStore,
    assignment: &FirstUseAssignment,
    report: &LocalVerificationReport,
    verifier: &DomainId,
    actor: &Principal,
    policy: &AcceptancePolicy,
    now_ms: i64,
) -> Result<FirstUseAcceptance, FirstUseError> {
    let binding = bind_first_use_evidence(assignment, report)?;
    let outcome = record_local_verification(
        store,
        assignment.project_id.as_str(),
        &assignment.candidate,
        Some(report),
        verifier,
        now_ms,
    )?;
    if !matches!(outcome, VerificationOutcome::Verified(_)) {
        return Ok(FirstUseAcceptance::Blocked("验证未通过或非独立证据"));
    }
    let request = AcceptRequest {
        acceptance_id: DomainId::new(format!(
            "acc-{}-{}",
            &assignment.plan.goal_digest.value[..12],
            &report.report_id.as_str()[4..]
        )),
        binding,
        checks_passed: report.passed,
        blocking_risks: Vec::new(),
        reason: format!("首次使用绑定报告 {}", report.report_id.as_str()),
        evidence_refs: report.evidence_refs.clone(),
    };
    // record_local_verification 已写入 Verified 记录；此处只做接受门禁，不重复推进验证。
    Ok(
        match accept(
            store,
            assignment.project_id.as_str(),
            assignment.artifact_id(),
            &request,
            actor,
            policy,
            now_ms + 1,
        )? {
            AcceptOutcome::Accepted(record) | AcceptOutcome::AutoAccepted(record) => {
                FirstUseAcceptance::Accepted(Box::new(record))
            }
            AcceptOutcome::RequiresHuman => FirstUseAcceptance::RequiresHuman,
        },
    )
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_acceptance::current_acceptance;
    use crate::coding_integration::load_delivery;
    use crate::config::BodConfig;
    use crate::domain::artifact::AcceptanceDecision;
    use crate::workspace_safety::gate_diff;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    const COMPANY: &str = "company-first-use";
    const NOW: i64 = 1_763_700_000_000;

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
                std::env::temp_dir().join(format!("bod-first-use-{}-{seq}", std::process::id()));
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
            let first_commit = git(&repo, &["rev-parse", "HEAD"]);
            Self {
                _root: root.clone(),
                repo,
                db: root.join("bod.sqlite"),
                first_commit,
            }
        }

        fn store(&self) -> SqliteStore {
            SqliteStore::open(&self.db, &BodConfig::default()).unwrap()
        }

        fn input(&self) -> FirstUseInput {
            FirstUseInput {
                project: Project {
                    id: DomainId::new("project-real"),
                    company_id: DomainId::new(COMPANY),
                    name: "真实项目".to_string(),
                    repository: Some(RepositoryContext {
                        provider: "local-git".to_string(),
                        repository_id: "repo-real".to_string(),
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

        fn second_commit(&self) -> String {
            fs::write(self.repo.join("src/lib.rs"), "pub fn hello() -> u8 { 1 }\n").unwrap();
            git(&self.repo, &["add", "src/lib.rs"]);
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
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git 命令应可执行");
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
            kind: crate::domain::model::PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    #[test]
    fn invariant_i_10_first_use_needs_no_team_work_package_or_run_config() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        assert_eq!(store.count_work_package_revisions(COMPANY).unwrap(), 0);
        assert_eq!(store.count_runs(COMPANY).unwrap(), 0);
        assert_eq!(store.core_row_counts().unwrap().issues, 0);

        let assignment =
            dispatch_first_use(&mut store, &fixture.input(), &Fixture::agents(), NOW).unwrap();

        assert_eq!(assignment.commit, fixture.first_commit);
        assert_eq!(assignment.project_id.as_str(), "project-real");
        assert_eq!(assignment.agent_id.as_str(), "agent-ready");
        assert_eq!(store.count_work_package_revisions(COMPANY).unwrap(), 1);
        assert_eq!(store.count_runs(COMPANY).unwrap(), 1);
        assert_eq!(store.core_row_counts().unwrap().issues, 1);
        let issue = store
            .transaction()
            .unwrap()
            .get_issue(assignment.plan.issue_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(issue.spec.title, "修复登录失败");
        assert_eq!(
            issue.spec.assignment.as_ref().unwrap().owner.id.as_str(),
            "agent-ready"
        );
        assert_eq!(issue.spec.acceptance.criteria.len(), 2);
        let payload = store
            .latest_event_payload(assignment.plan.issue_id.as_str())
            .unwrap()
            .unwrap();
        assert!(payload.contains("first-use-dispatched"));
        assert!(payload.contains(&assignment.commit));
    }

    #[test]
    fn first_use_plan_is_deterministic_and_scope_excludes_shared_config() {
        let fixture = Fixture::new();
        let input = fixture.input();
        let first = plan_first_use(&input, &Fixture::agents()).unwrap();
        let replay = plan_first_use(&input, &Fixture::agents()).unwrap();
        assert_eq!(first, replay);
        assert_eq!(first.goal_digest, replay.goal_digest);

        let mut changed = input.clone();
        changed.acceptance.push("日志不包含密码".to_string());
        assert_ne!(
            plan_first_use(&changed, &Fixture::agents())
                .unwrap()
                .agreement_id,
            first.agreement_id
        );

        assert!(first.write_scope.allow().contains(&"src/".to_string()));
        assert!(first.write_scope.allow().contains(&"tests/".to_string()));
        assert!(
            !first
                .write_scope
                .allow()
                .iter()
                .any(|path| path == "Cargo.toml")
        );
        let accepted = gate_diff(
            &fixture.repo,
            &first.write_scope,
            &["src/lib.rs".to_string(), "tests/smoke.rs".to_string()],
        );
        assert!(accepted.is_accepted());
        let rejected = gate_diff(
            &fixture.repo,
            &first.write_scope,
            &[
                "src/lib.rs".to_string(),
                "Cargo.toml".to_string(),
                ".git/config".to_string(),
            ],
        );
        assert_eq!(rejected.rejected_paths(), vec!["Cargo.toml", ".git/config"]);
    }

    #[test]
    fn first_use_rejects_unconnected_agent_and_empty_acceptance() {
        let fixture = Fixture::new();
        let input = fixture.input();
        let unconnected = plan_first_use(&input, &[DomainId::new("other-agent")]);
        assert!(matches!(
            unconnected,
            Err(FirstUseError::AgentNotConnected { .. })
        ));

        let mut no_acceptance = input.clone();
        no_acceptance.acceptance.clear();
        assert!(matches!(
            plan_first_use(&no_acceptance, &Fixture::agents()),
            Err(FirstUseError::EmptyAcceptance)
        ));
        let mut duplicate = input;
        duplicate.acceptance.push(duplicate.acceptance[0].clone());
        assert!(matches!(
            plan_first_use(&duplicate, &Fixture::agents()),
            Err(FirstUseError::DuplicateAcceptance { .. })
        ));
    }

    #[test]
    fn merged_evidence_must_match_actual_goal_and_candidate() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let assignment =
            dispatch_first_use(&mut store, &fixture.input(), &Fixture::agents(), NOW).unwrap();
        let report = LocalVerificationReport::new(
            assignment.commit.clone(),
            assignment.plan.rules_revision.clone(),
            assignment.plan.environment.clone(),
            DomainId::new("independent-verifier"),
            vec!["cargo test".to_string()],
            true,
            NOW + 1,
        )
        .unwrap();
        let binding = bind_first_use_evidence(&assignment, &report).unwrap();
        assert_eq!(
            binding.merge_combination,
            Some(assignment.merge_combination.clone())
        );

        let second_commit = fixture.second_commit();
        assert_ne!(second_commit, assignment.commit);
        let stale_report = LocalVerificationReport::new(
            second_commit,
            assignment.plan.rules_revision.clone(),
            assignment.plan.environment.clone(),
            verifier(),
            vec!["cargo test".to_string()],
            true,
            NOW + 2,
        )
        .unwrap();
        assert!(matches!(
            bind_first_use_evidence(&assignment, &stale_report),
            Err(FirstUseError::EvidenceCombinationMismatch { field: "commit" })
        ));

        let other = assignment.clone();
        assert_eq!(
            binding.divergence(&other.candidate.binding),
            Some("merge_combination")
        );
        assert_eq!(
            binding.merge_combination,
            Some(assignment.merge_combination.clone())
        );
    }

    #[test]
    fn first_use_acceptance_binds_project_and_survives_restart() {
        let fixture = Fixture::new();
        let acceptance_id;
        let artifact_id;
        let combination;
        {
            let mut store = fixture.store();
            let assignment =
                dispatch_first_use(&mut store, &fixture.input(), &Fixture::agents(), NOW).unwrap();
            artifact_id = assignment.plan.artifact_id.clone();
            combination = assignment.merge_combination.clone();
            let report = LocalVerificationReport::new(
                assignment.commit.clone(),
                assignment.plan.rules_revision.clone(),
                assignment.plan.environment.clone(),
                verifier(),
                vec!["cargo test".to_string(), "cargo clippy".to_string()],
                true,
                NOW + 1,
            )
            .unwrap();
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
            acceptance_id = record.acceptance_id.clone();
            assert_eq!(record.binding.merge_combination, Some(combination.clone()));
            assert_eq!(
                record.binding.work_package_revision,
                assignment.plan.work_package_revision
            );
        }

        let mut reopened = fixture.store();
        let delivery = load_delivery(&mut reopened, &artifact_id)
            .unwrap()
            .expect("重启后仍可查询首次使用交付");
        assert_eq!(delivery.acceptance_id, acceptance_id);
        assert_eq!(delivery.binding.merge_combination, Some(combination));
        assert_eq!(delivery.decision, AcceptanceDecision::Accepted);
        assert_eq!(reopened.count_runs(COMPANY).unwrap(), 1);
    }

    #[test]
    fn self_reported_verification_never_accepts_first_use() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let assignment =
            dispatch_first_use(&mut store, &fixture.input(), &Fixture::agents(), NOW).unwrap();
        let report = LocalVerificationReport::new(
            assignment.commit.clone(),
            assignment.plan.rules_revision.clone(),
            assignment.plan.environment.clone(),
            assignment.agent_id.clone(),
            vec!["cargo test".to_string()],
            true,
            NOW + 1,
        )
        .unwrap();
        assert!(matches!(
            bind_first_use_evidence(&assignment, &report),
            Err(FirstUseError::EvidenceCombinationMismatch { field: "actor" })
        ));
        let refused = accept_first_use(
            &mut store,
            &assignment,
            &report,
            &verifier(),
            &user(),
            &AcceptancePolicy::default(),
            NOW + 2,
        );
        assert!(matches!(
            refused,
            Err(FirstUseError::EvidenceCombinationMismatch { field: "actor" })
        ));
        assert!(
            current_acceptance(&mut store, assignment.artifact_id())
                .unwrap()
                .is_none()
        );
    }
}
