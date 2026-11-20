//! P1 切片 4.4：本地验证报告绑定与交付备份核验（G-6/G-9）。
//!
//! 权威语义：`direct-delivery` 的「本地验证与备份」——退出码 0 或 Agent 自述通过
//! **不是**接受证据；报告必须记录被测 commit、规则版本、环境、执行者与时间，
//! 且备份缺少 WAL/CAS 引用不得标可恢复。
//!
//! 本模块不重复实现 P0 的验证状态机与备份快照：它负责
//! 1) 真实执行本地命令并把正文（含每条命令的输出摘要）落 CAS；
//! 2) 从宿主验证记录反查 CAS 正文，核验报告 ↔ 正文 ↔ 候选一致；
//! 3) 在 P0 `verify_backup` 之外补交付级 CAS 引用覆盖检查，缺一项即不可恢复。

use crate::cas::{CasError, CasObject, CasStore};
use crate::coding_integration::{
    CodingIntegrationError, DeliveryCandidate, LocalVerificationReport, resolve_head,
};
use crate::domain::DomainId;
use crate::domain::artifact::Digest;
use crate::domain::sha256_hex;
use crate::migration_safety::verify_backup;
use crate::recovery_models::BackupRecord;
use crate::store::{SqliteStore, StoreError};
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 报告正文格式版本；格式变化必须换版本，避免旧正文被按新规则解释。
pub const REPORT_BODY_VERSION: &str = "bod.local-verification/2";
pub const REPORT_MIME: &str = "application/vnd.bod.local-verification+text";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum VerificationExecutionError {
    EmptyChecks,
    Git(CodingIntegrationError),
    Io(std::io::Error),
    Cas(CasError),
    Store(StoreError),
    Timeout { label: String, timeout_ms: u64 },
}

impl fmt::Display for VerificationExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyChecks => write!(f, "没有实际验证项"),
            Self::Git(err) => write!(f, "仓库提交解析失败：{err}"),
            Self::Io(err) => write!(f, "本地验证 IO 失败：{err}"),
            Self::Cas(err) => write!(f, "验证报告 CAS 写入失败：{err}"),
            Self::Store(err) => write!(f, "宿主存储失败：{err}"),
            Self::Timeout { label, timeout_ms } => {
                write!(f, "验证项 {label} 超过 {timeout_ms}ms 未完成")
            }
        }
    }
}

impl From<CodingIntegrationError> for VerificationExecutionError {
    fn from(err: CodingIntegrationError) -> Self {
        Self::Git(err)
    }
}

impl From<std::io::Error> for VerificationExecutionError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<CasError> for VerificationExecutionError {
    fn from(err: CasError) -> Self {
        Self::Cas(err)
    }
}

impl From<StoreError> for VerificationExecutionError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

#[derive(Debug)]
pub enum EvidenceError {
    NoVerification,
    NotVerified(String),
    MissingCasRef,
    MissingDigestRef,
    CasRead(CasError),
    BodyDigestMismatch { expected: String, actual: String },
    BodyDoesNotMatchReport,
    Store(StoreError),
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoVerification => write!(f, "没有可核验的验证记录"),
            Self::NotVerified(state) => write!(f, "验证状态不是 Verified：{state}"),
            Self::MissingCasRef => write!(f, "验证记录缺少报告 CAS 引用"),
            Self::MissingDigestRef => write!(f, "验证记录缺少报告摘要引用"),
            Self::CasRead(err) => write!(f, "报告正文读取失败：{err}"),
            Self::BodyDigestMismatch { expected, actual } => {
                write!(f, "报告正文摘要不匹配：期望 {expected}，实际 {actual}")
            }
            Self::BodyDoesNotMatchReport => write!(f, "报告正文与摘要字段不一致"),
            Self::Store(err) => write!(f, "宿主存储失败：{err}"),
        }
    }
}

impl From<StoreError> for EvidenceError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

// ---------------------------------------------------------------------------
// 执行与报告
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationCheckSpec {
    /// 展示用稳定标签，不参与权限判断。
    pub label: String,
    /// argv[0] 是可执行程序，其余为参数；不使用 shell 字符串拼接。
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutedCheck {
    pub label: String,
    pub argv: Vec<String>,
    pub exit_code: i32,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
    pub timed_out: bool,
}

impl ExecutedCheck {
    fn canonical(&self) -> String {
        format!(
            "{}|argv={}|exit={}|stdout={}|stderr={}|timeout={}",
            self.label,
            self.argv.join("\u{1f}"),
            self.exit_code,
            self.stdout_sha256,
            self.stderr_sha256,
            self.timed_out
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutedVerification {
    pub report: LocalVerificationReport,
    pub commit: String,
    pub checks: Vec<ExecutedCheck>,
    pub body: Vec<u8>,
    pub cas_object: CasObject,
}

impl ExecutedVerification {
    /// 供验证记录与备份核验使用：`cas:<digest>` + `sha256:<报告摘要>`。
    pub fn evidence_refs(&self) -> Vec<String> {
        vec![
            format!("cas:{}", self.cas_object.digest.value),
            format!("sha256:{}", self.report.digest.value),
        ]
    }

    pub fn cas_digest(&self) -> &Digest {
        &self.cas_object.digest
    }
}

/// 真实执行命令、落盘报告正文、生成绑定报告。
///
/// 每一项都有超时上限；超时会杀掉进程并标记 `timed_out`，不会把"没跑完"当成通过。
#[allow(clippy::too_many_arguments)]
pub fn execute_verification(
    repo: &Path,
    checks: &[VerificationCheckSpec],
    rules_revision: &DomainId,
    environment: &DomainId,
    actor: &DomainId,
    timeout_ms: u64,
    now_ms: i64,
    cas: &CasStore,
) -> Result<ExecutedVerification, VerificationExecutionError> {
    if checks.is_empty() {
        return Err(VerificationExecutionError::EmptyChecks);
    }
    let commit = resolve_head(repo)?.to_ascii_lowercase();
    let mut executed = Vec::with_capacity(checks.len());
    for spec in checks {
        let check = run_check(spec, repo, timeout_ms)?;
        executed.push(check);
    }
    let canonical_checks: Vec<String> = executed.iter().map(ExecutedCheck::canonical).collect();
    let body = format!(
        "{REPORT_BODY_VERSION}\ncommit:{commit}\nrules:{}\nenvironment:{}\nactor:{}\nchecks:{}\n{}\nat:{now_ms}\n",
        rules_revision.as_str(),
        environment.as_str(),
        actor.as_str(),
        canonical_checks.len(),
        canonical_checks.join("\n")
    )
    .into_bytes();
    let cas_object = cas.stage(&body, REPORT_MIME)?;
    let passed = executed
        .iter()
        .all(|check| check.exit_code == 0 && !check.timed_out);
    let mut report = LocalVerificationReport::new(
        commit.clone(),
        rules_revision.clone(),
        environment.clone(),
        actor.clone(),
        canonical_checks,
        passed,
        now_ms,
    )?;
    report.evidence_refs = vec![
        format!("cas:{}", cas_object.digest.value),
        format!("sha256:{}", report.digest.value),
    ];
    Ok(ExecutedVerification {
        report,
        commit,
        checks: executed,
        body,
        cas_object,
    })
}

fn run_check(
    spec: &VerificationCheckSpec,
    cwd: &Path,
    timeout_ms: u64,
) -> Result<ExecutedCheck, VerificationExecutionError> {
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or(VerificationExecutionError::EmptyChecks)?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = stdout_pipe.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });
    let stderr_thread = stderr_pipe.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            timed_out = true;
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_thread
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_thread
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(ExecutedCheck {
        label: spec.label.clone(),
        argv: spec.argv.clone(),
        exit_code: status.code().unwrap_or(1),
        stdout_sha256: sha256_hex(&stdout),
        stderr_sha256: sha256_hex(&stderr),
        timed_out,
    })
}

// ---------------------------------------------------------------------------
// 证据核验
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceVerification {
    pub report_id: DomainId,
    pub commit: String,
    pub cas_ref: String,
    pub body_bytes: usize,
    pub checks: usize,
}

/// 从宿主验证记录反查报告正文：正文必须存在、摘要一致、字段与报告逐项对应。
pub fn verify_report_evidence(
    store: &mut SqliteStore,
    cas: &CasStore,
    candidate: &DeliveryCandidate,
    report: &LocalVerificationReport,
) -> Result<EvidenceVerification, EvidenceError> {
    let record = store
        .transaction()?
        .latest_verification(candidate.revision_id.as_str())?
        .ok_or(EvidenceError::NoVerification)?;
    if record.state != crate::domain::artifact::VerificationState::Verified {
        return Err(EvidenceError::NotVerified(
            record.state.as_str().to_string(),
        ));
    }
    let cas_value = report
        .evidence_refs
        .iter()
        .find_map(|item| item.strip_prefix("cas:"))
        .ok_or(EvidenceError::MissingCasRef)?;
    let cas_prefix = format!("cas:{cas_value}");
    let expected_digest = format!("sha256:{}", report.digest.value);
    if !record.evidence_refs.iter().any(|item| item == &cas_prefix) {
        return Err(EvidenceError::MissingCasRef);
    }
    if !record
        .evidence_refs
        .iter()
        .any(|item| item == &expected_digest)
    {
        return Err(EvidenceError::MissingDigestRef);
    }
    let digest = Digest::parse(&format!("sha256:{cas_value}"))
        .expect("CAS 引用由本模块生成，必为 sha256:<hex>");
    let bytes = match cas.read(&digest) {
        Ok(bytes) => bytes,
        Err(err) => return Err(EvidenceError::CasRead(err)),
    };
    let actual = Digest::of_bytes(&bytes);
    if actual != digest {
        return Err(EvidenceError::BodyDigestMismatch {
            expected: digest.as_string(),
            actual: actual.as_string(),
        });
    }
    let text = String::from_utf8_lossy(&bytes);
    if !text.contains(&format!("commit:{}\n", report.commit))
        || !text.contains(&format!("actor:{}\n", report.actor.as_str()))
        || !report.checks.iter().all(|check| text.contains(check))
    {
        return Err(EvidenceError::BodyDoesNotMatchReport);
    }
    Ok(EvidenceVerification {
        report_id: report.report_id.clone(),
        commit: report.commit.clone(),
        cas_ref: cas_prefix,
        body_bytes: bytes.len(),
        checks: report.checks.len(),
    })
}

// ---------------------------------------------------------------------------
// 备份可恢复性（G-6 的交付级补强）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupVerification {
    pub backup_id: String,
    pub recoverable: bool,
    pub problems: Vec<String>,
}

/// P0 `verify_backup` 只检查快照自身；这里再核对交付报告引用的 CAS 对象确实在场。
/// 任一引用缺失、快照缺 WAL 或摘要被改，均不得标可恢复。
pub fn verify_backup_for_delivery(
    record: &BackupRecord,
    cas: &CasStore,
    expected_digests: &[Digest],
) -> BackupVerification {
    let mut problems = verify_backup(record);
    if record.status != crate::recovery_models::BackupStatus::Verified {
        problems.push("备份记录本身不是 verified，不得声称可恢复".to_string());
    }
    for digest in expected_digests {
        if !cas.contains(digest) {
            problems.push(format!("交付证据 CAS 引用缺失：{}", digest.as_string()));
        }
    }
    if record.cas_object_count < expected_digests.len() as i64 {
        problems.push(format!(
            "备份记录的 CAS 对象数 {} 少于交付证据引用数 {}",
            record.cas_object_count,
            expected_digests.len()
        ));
    }
    let recoverable = problems.is_empty() && record.wal_included && record.status.is_recoverable();
    BackupVerification {
        backup_id: record.backup_id.clone(),
        recoverable,
        problems,
    }
}

/// 备份核验使用的交付证据摘要列表；正文已在 [`execute_verification`] 中落 CAS。
pub fn expected_report_digests(executed: &ExecutedVerification) -> Vec<Digest> {
    vec![executed.cas_object.digest.clone()]
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_acceptance::current_acceptance;
    use crate::coding_integration::{
        DeliveryAcceptance, DeliveryBaseline, accept_delivery, record_local_verification,
        register_git_candidate,
    };
    use crate::config::BodConfig;
    use crate::domain::artifact::AcceptancePolicy;
    use crate::migration_safety::{BackupInput, create_backup};
    use crate::recovery_models::BackupStatus;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;
    use std::time::{SystemTime, UNIX_EPOCH};

    const COMPANY: &str = "company-verification";
    const NOW: i64 = 1_763_800_000_000;

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
            let root =
                std::env::temp_dir().join(format!("bod-verification-{}-{seq}", std::process::id()));
            let repo = root.join("repo");
            fs::create_dir_all(repo.join("src")).unwrap();
            fs::write(repo.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
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

        fn rules() -> DomainId {
            DomainId::new("rules-v3")
        }

        fn environment() -> DomainId {
            DomainId::new("env-local")
        }

        fn verifier() -> DomainId {
            DomainId::new("verifier-human")
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

        fn candidate(&self, store: &mut SqliteStore) -> DeliveryCandidate {
            let baseline = DeliveryBaseline {
                artifact_id: DomainId::new("delivery-verification"),
                work_package_revision: DomainId::new("wp-v1"),
                spec_revision: DomainId::new("spec-v1"),
                rules_revision: Self::rules(),
                environment: Self::environment(),
            };
            register_git_candidate(
                store,
                COMPANY,
                &self.repo,
                &baseline,
                &DomainId::new("agent-implementer"),
                NOW,
            )
            .unwrap()
        }

        fn backup(&self, store: &mut SqliteStore, cas: &CasStore) -> BackupRecord {
            create_backup(
                store,
                cas,
                &BackupInput {
                    company_id: COMPANY,
                    destination: &self.backups,
                    config_revision: "cfg-r1",
                    adapter_versions: "git=2.47",
                    key_recovery_note: "凭据管理器",
                },
                "b-delivery",
                NOW + 10,
            )
            .unwrap()
        }
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
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

    #[test]
    fn executed_report_binds_commit_checks_and_cas_body() {
        let fixture = Fixture::new();
        let cas = fixture.cas();
        let executed = execute_verification(
            &fixture.repo,
            &Fixture::checks(),
            &Fixture::rules(),
            &Fixture::environment(),
            &Fixture::verifier(),
            10_000,
            NOW + 1,
            &cas,
        )
        .unwrap();

        assert!(executed.report.passed);
        assert_eq!(executed.commit, git(&fixture.repo, &["rev-parse", "HEAD"]));
        assert_eq!(executed.checks.len(), 2);
        assert!(executed.checks.iter().all(|check| check.exit_code == 0));
        assert_eq!(executed.report.evidence_refs, executed.evidence_refs());
        assert!(cas.contains(&executed.cas_object.digest));
        let body = String::from_utf8_lossy(&executed.body);
        assert!(body.contains(&format!("commit:{}\n", executed.commit)));
        assert!(body.contains("bod.local-verification/2"));
    }

    #[test]
    fn failing_local_command_marks_report_not_passed() {
        let fixture = Fixture::new();
        let cas = fixture.cas();
        let passed = execute_verification(
            &fixture.repo,
            &Fixture::checks(),
            &Fixture::rules(),
            &Fixture::environment(),
            &Fixture::verifier(),
            10_000,
            NOW + 1,
            &cas,
        )
        .unwrap();
        assert!(passed.report.passed);

        fs::write(fixture.repo.join("src/lib.rs"), "pub fn dirty() {}\n").unwrap();
        let failed = execute_verification(
            &fixture.repo,
            &Fixture::checks(),
            &Fixture::rules(),
            &Fixture::environment(),
            &Fixture::verifier(),
            10_000,
            NOW + 2,
            &cas,
        )
        .unwrap();
        assert!(!failed.report.passed);
        assert!(
            failed
                .checks
                .iter()
                .any(|check| check.label == "worktree-clean" && check.exit_code == 1)
        );

        let mut store = fixture.store();
        let candidate = fixture.candidate(&mut store);
        let outcome = record_local_verification(
            &mut store,
            COMPANY,
            &candidate,
            Some(&failed.report),
            &Fixture::verifier(),
            NOW + 3,
        )
        .unwrap();
        assert!(!matches!(
            outcome,
            crate::coding_integration::VerificationOutcome::Verified(_)
        ));
    }

    #[test]
    fn report_from_other_commit_cannot_accept_candidate() {
        let fixture = Fixture::new();
        let cas = fixture.cas();
        let mut store = fixture.store();
        let candidate = fixture.candidate(&mut store);

        fs::write(fixture.repo.join("src/lib.rs"), "pub fn next() {}\n").unwrap();
        git(
            &fixture.repo,
            &[
                "-c",
                "user.name=bod",
                "-c",
                "user.email=bod@local",
                "commit",
                "-q",
                "-am",
                "next",
            ],
        );
        let later = execute_verification(
            &fixture.repo,
            &Fixture::checks(),
            &Fixture::rules(),
            &Fixture::environment(),
            &Fixture::verifier(),
            10_000,
            NOW + 1,
            &cas,
        )
        .unwrap();
        assert_ne!(later.commit, candidate.commit);

        let outcome = record_local_verification(
            &mut store,
            COMPANY,
            &candidate,
            Some(&later.report),
            &Fixture::verifier(),
            NOW + 2,
        )
        .unwrap();
        assert!(!matches!(
            outcome,
            crate::coding_integration::VerificationOutcome::Verified(_)
        ));
        let blocked = accept_delivery(
            &mut store,
            COMPANY,
            &candidate,
            Some(&later.report),
            &crate::domain::model::Principal {
                id: DomainId::new("user-1"),
                kind: crate::domain::model::PrincipalKind::User,
                claimed_roles: Vec::new(),
            },
            &AcceptancePolicy::default(),
            &DomainId::new("acc-other-commit"),
            NOW + 3,
        )
        .unwrap();
        assert!(
            current_acceptance(&mut store, &candidate.artifact_id)
                .unwrap()
                .is_none()
        );
        assert!(!matches!(blocked, DeliveryAcceptance::Accepted(_)));
    }

    #[test]
    fn self_report_without_executed_report_keeps_pending() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let candidate = fixture.candidate(&mut store);
        let outcome = record_local_verification(
            &mut store,
            COMPANY,
            &candidate,
            None,
            &Fixture::verifier(),
            NOW + 1,
        )
        .unwrap();
        assert!(!matches!(
            outcome,
            crate::coding_integration::VerificationOutcome::Verified(_)
        ));
        let blocked = accept_delivery(
            &mut store,
            COMPANY,
            &candidate,
            None,
            &crate::domain::model::Principal {
                id: DomainId::new("user-1"),
                kind: crate::domain::model::PrincipalKind::User,
                claimed_roles: Vec::new(),
            },
            &AcceptancePolicy::default(),
            &DomainId::new("acc-self-report"),
            NOW + 2,
        )
        .unwrap();
        assert_eq!(blocked, DeliveryAcceptance::Blocked("没有本地验证报告"));
        assert!(
            current_acceptance(&mut store, &candidate.artifact_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn evidence_verification_detects_missing_cas_body() {
        let fixture = Fixture::new();
        let cas = fixture.cas();
        let executed = execute_verification(
            &fixture.repo,
            &Fixture::checks(),
            &Fixture::rules(),
            &Fixture::environment(),
            &Fixture::verifier(),
            10_000,
            NOW + 1,
            &cas,
        )
        .unwrap();
        let mut store = fixture.store();
        let candidate = fixture.candidate(&mut store);
        record_local_verification(
            &mut store,
            COMPANY,
            &candidate,
            Some(&executed.report),
            &Fixture::verifier(),
            NOW + 2,
        )
        .unwrap();

        let verified =
            verify_report_evidence(&mut store, &cas, &candidate, &executed.report).unwrap();
        assert_eq!(verified.commit, executed.commit);
        assert_eq!(verified.checks, executed.report.checks.len());

        let object_path = cas.object_path(&executed.cas_object.digest);
        fs::remove_file(&object_path).unwrap();
        assert!(matches!(
            verify_report_evidence(&mut store, &cas, &candidate, &executed.report),
            Err(EvidenceError::CasRead(_))
        ));
    }

    #[test]
    fn delivery_backup_is_recoverable_with_report_cas() {
        let fixture = Fixture::new();
        let cas = fixture.cas();
        let executed = execute_verification(
            &fixture.repo,
            &Fixture::checks(),
            &Fixture::rules(),
            &Fixture::environment(),
            &Fixture::verifier(),
            10_000,
            NOW + 1,
            &cas,
        )
        .unwrap();
        let digests = expected_report_digests(&executed);
        let mut store = fixture.store();
        let record = fixture.backup(&mut store, &cas);
        assert_eq!(record.status, BackupStatus::Verified);
        let checked = verify_backup_for_delivery(&record, &cas, &digests);
        assert!(checked.recoverable, "问题：{:?}", checked.problems);
        assert!(checked.problems.is_empty());

        let mut forge = record.clone();
        forge.wal_included = false;
        let without_wal = verify_backup_for_delivery(&forge, &cas, &digests);
        assert!(!without_wal.recoverable);
        assert!(
            without_wal
                .problems
                .iter()
                .any(|problem| problem.contains("WAL"))
        );

        fs::remove_file(cas.object_path(&executed.cas_object.digest)).unwrap();
        let missing = verify_backup_for_delivery(&record, &cas, &digests);
        assert!(!missing.recoverable);
        assert!(
            missing
                .problems
                .iter()
                .any(|problem| problem.contains("CAS 引用缺失"))
        );
    }

    #[test]
    fn tampered_snapshot_is_not_recoverable() {
        let fixture = Fixture::new();
        let cas = fixture.cas();
        let mut store = fixture.store();
        let record = fixture.backup(&mut store, &cas);
        let checked = verify_backup_for_delivery(&record, &cas, &[]);
        assert!(checked.recoverable, "问题：{:?}", checked.problems);
        fs::write(Path::new(&record.path), b"tampered").unwrap();
        let tampered = verify_backup_for_delivery(&record, &cas, &[]);
        assert!(!tampered.recoverable);
        assert!(
            tampered
                .problems
                .iter()
                .any(|problem| problem.contains("摘要变化") || problem.contains("完整性"))
        );
    }
}
