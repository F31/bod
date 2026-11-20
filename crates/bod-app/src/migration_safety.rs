//! P0 D6 迁移登记与一致性备份（任务 7.1、7.4，§33.7、G-6/G-49）。
//!
//! 两条硬要求：
//! 1. 已发布脚本不可原地改写：校验和与登记表比对，漂移即报错；
//! 2. 备份必须包含已提交的 WAL 内容与独立密钥恢复说明，缺任一项不标"可恢复"。

use crate::cas::CasStore;
use crate::domain::DomainId;
use crate::domain::artifact::sha256_hex;
use crate::domain::execution::ExecutionState;
use crate::recovery_models::{BackupRecord, BackupStatus, MigrationRecord, MigrationRisk};
use crate::store::{CoreRowCounts, SqliteStore, StoreError, migration_sources};
use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum RecoveryError {
    Store(StoreError),
    Io(std::io::Error),
    /// 备份不可恢复：缺内容或校验失败。
    BackupNotRecoverable(Vec<String>),
    Unsupported(&'static str),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "恢复 IO 失败: {err}"),
            Self::BackupNotRecoverable(problems) => {
                write!(f, "备份不可恢复: {}", problems.join("；"))
            }
            Self::Unsupported(what) => write!(f, "不支持的恢复操作: {what}"),
        }
    }
}

impl std::error::Error for RecoveryError {}

impl From<StoreError> for RecoveryError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<std::io::Error> for RecoveryError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShippedMigration {
    pub migration_id: &'static str,
    pub checksum: String,
}

/// 已编译进二进制的迁移及其校验和。
pub fn shipped_migrations() -> Vec<ShippedMigration> {
    migration_sources()
        .map(|(name, sql)| ShippedMigration {
            migration_id: name,
            checksum: sha256_hex(sql.as_bytes()),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationIssue {
    DuplicateId(String),
    MissingChecksum(String),
    /// 登记表里的 checksum 与已编译脚本不一致：说明脚本被原地改写。
    ChecksumDrift {
        migration_id: String,
        recorded: String,
        actual: String,
    },
    DestructiveWithoutWindow(String),
    UnknownDependency {
        migration_id: String,
        dependency: String,
    },
    MissingMetadata(String),
}

impl MigrationIssue {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateId(_) => "MIGRATION_DUPLICATE_ID",
            Self::MissingChecksum(_) => "MIGRATION_MISSING_CHECKSUM",
            Self::ChecksumDrift { .. } => "MIGRATION_CHECKSUM_DRIFT",
            Self::DestructiveWithoutWindow(_) => "MIGRATION_DESTRUCTIVE_WITHOUT_WINDOW",
            Self::UnknownDependency { .. } => "MIGRATION_UNKNOWN_DEPENDENCY",
            Self::MissingMetadata(_) => "MIGRATION_MISSING_METADATA",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::DuplicateId(id) => format!("迁移 ID 重复：{id}"),
            Self::MissingChecksum(id) => format!("迁移缺少 checksum：{id}"),
            Self::ChecksumDrift {
                migration_id,
                recorded,
                actual,
            } => format!(
                "迁移脚本被原地改写：{migration_id}（登记 {recorded}，实际 {actual}），必须新增迁移修复"
            ),
            Self::DestructiveWithoutWindow(id) => {
                format!("破坏性迁移缺少授权维护窗口与恢复说明：{id}")
            }
            Self::UnknownDependency {
                migration_id,
                dependency,
            } => format!("{migration_id} 依赖未登记的迁移 {dependency}"),
            Self::MissingMetadata(id) => format!("迁移缺少兼容窗口或恢复说明：{id}"),
        }
    }
}

/// 校验已编译脚本自身：ID 唯一、checksum 可用、无破坏性 DDL。
pub fn verify_shipped_migrations() -> Vec<MigrationIssue> {
    let shipped = shipped_migrations();
    let mut issues = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in &shipped {
        if !seen.insert(item.migration_id) {
            issues.push(MigrationIssue::DuplicateId(item.migration_id.to_string()));
        }
        if item.checksum.len() != 64 {
            issues.push(MigrationIssue::MissingChecksum(
                item.migration_id.to_string(),
            ));
        }
    }
    for (name, sql) in migration_sources() {
        if contains_destructive_ddl(sql) {
            issues.push(MigrationIssue::DestructiveWithoutWindow(name.to_string()));
        }
    }
    issues
}

/// 只识别语句级破坏性 DDL，不把 trigger 内的 `BEFORE DELETE ON` 误判。
pub fn contains_destructive_ddl(sql: &str) -> bool {
    sql.lines().map(str::trim).any(|line| {
        let upper = line.to_ascii_uppercase();
        upper.starts_with("DROP TABLE")
            || upper.starts_with("DROP INDEX")
            || upper.starts_with("DROP COLUMN")
            || upper.starts_with("DELETE FROM")
            || upper.starts_with("ALTER TABLE") && upper.contains("DROP")
    })
}

/// 把所有已发布迁移登记进 registry（幂等）。
pub fn register_shipped_migrations(
    store: &mut SqliteStore,
    now_ms: i64,
) -> Result<usize, RecoveryError> {
    let uow = store.transaction()?;
    let mut inserted = 0usize;
    for (index, item) in shipped_migrations().iter().enumerate() {
        let depends_on = if index == 0 {
            Vec::new()
        } else {
            vec![shipped_migrations()[index - 1].migration_id.to_string()]
        };
        let record = MigrationRecord {
            migration_id: item.migration_id.to_string(),
            checksum: item.checksum.clone(),
            depends_on,
            schema_range: "bod.schema/v1".to_string(),
            software_range: format!(">={}", env!("CARGO_PKG_VERSION")),
            risk: MigrationRisk::Safe,
            compatibility_window: "P0：同类补丁内向前兼容；改结构必须新增迁移".to_string(),
            backup_notes: "升级前执行 bod backup，并核验 verified 状态".to_string(),
            data_notes: "仅新增表/索引与不可变 trigger，无数据转换".to_string(),
            applied_at_ms: now_ms,
        };
        uow.put_migration_record(&record)?;
        inserted += 1;
    }
    uow.commit()?;
    Ok(inserted)
}

/// 校验登记表：重复、缺 checksum、依赖未知、checksum 漂移、破坏性缺窗口。
pub fn verify_registry(records: &[MigrationRecord]) -> Vec<MigrationIssue> {
    let shipped = shipped_migrations();
    let known: std::collections::BTreeSet<&str> = records
        .iter()
        .map(|item| item.migration_id.as_str())
        .collect();
    let mut issues = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for record in records {
        if !seen.insert(record.migration_id.as_str()) {
            issues.push(MigrationIssue::DuplicateId(record.migration_id.clone()));
        }
        if record.checksum.trim().is_empty() {
            issues.push(MigrationIssue::MissingChecksum(record.migration_id.clone()));
        }
        if record.risk == MigrationRisk::Destructive
            && record.compatibility_window.trim().is_empty()
        {
            issues.push(MigrationIssue::DestructiveWithoutWindow(
                record.migration_id.clone(),
            ));
        }
        if record.compatibility_window.trim().is_empty() || record.backup_notes.trim().is_empty() {
            issues.push(MigrationIssue::MissingMetadata(record.migration_id.clone()));
        }
        for dependency in &record.depends_on {
            if !known.contains(dependency.as_str()) {
                issues.push(MigrationIssue::UnknownDependency {
                    migration_id: record.migration_id.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        if let Some(actual) = shipped
            .iter()
            .find(|item| item.migration_id == record.migration_id)
            && actual.checksum != record.checksum
        {
            issues.push(MigrationIssue::ChecksumDrift {
                migration_id: record.migration_id.clone(),
                recorded: record.checksum.clone(),
                actual: actual.checksum.clone(),
            });
        }
    }
    issues
}

#[derive(Debug, Clone)]
pub struct BackupInput<'a> {
    pub company_id: &'a str,
    pub destination: &'a Path,
    pub config_revision: &'a str,
    pub adapter_versions: &'a str,
    /// 独立密钥恢复说明；为空则备份不标可恢复。
    pub key_recovery_note: &'a str,
}

/// 生成一致性快照并核验可恢复性。
pub fn create_backup(
    store: &mut SqliteStore,
    cas: &CasStore,
    input: &BackupInput<'_>,
    backup_id: &str,
    now_ms: i64,
) -> Result<BackupRecord, RecoveryError> {
    fs::create_dir_all(input.destination)?;
    let snapshot = input
        .destination
        .join(format!("bod-backup-{backup_id}.sqlite"));
    if snapshot.exists() {
        fs::remove_file(&snapshot)?;
    }
    // VACUUM INTO 生成一致快照，已提交的 WAL 内容被合并进主文件。
    store.vacuum_into(&snapshot.to_string_lossy())?;

    let bytes = fs::read(&snapshot)?;
    let digest = sha256_hex(&bytes);
    let (cas_refs, external_refs) = store.transaction()?.referenced_storage(input.company_id)?;
    let mut problems = Vec::new();

    if input.key_recovery_note.trim().is_empty() {
        problems.push("缺少独立密钥恢复说明".to_string());
    }
    if input.config_revision.trim().is_empty() {
        problems.push("缺少配置修订".to_string());
    }
    if input.adapter_versions.trim().is_empty() {
        problems.push("缺少适配器版本".to_string());
    }
    if let Some(reason) = integrity_problem(&snapshot) {
        problems.push(reason);
    }
    // 有本地 CAS 引用但目录为空：备份缺失成果字节。
    if !cas_refs.is_empty() && cas.object_count() == 0 {
        problems.push(format!(
            "登记了 {} 个 CAS 引用但备份目录没有对象",
            cas_refs.len()
        ));
    }

    let record = BackupRecord {
        backup_id: backup_id.to_string(),
        company_id: DomainId::new(input.company_id),
        path: snapshot.to_string_lossy().to_string(),
        db_sha256: digest,
        db_bytes: bytes.len() as i64,
        wal_included: true,
        snapshot_method: format!("vacuum_into（合并 WAL；外部引用 {external_refs} 个）"),
        cas_object_count: cas.object_count() as i64,
        config_revision: input.config_revision.to_string(),
        adapter_versions: input.adapter_versions.to_string(),
        key_recovery_note: input.key_recovery_note.to_string(),
        status: if problems.is_empty() {
            BackupStatus::Verified
        } else {
            BackupStatus::Incomplete
        },
        problems,
        created_at_ms: now_ms,
    };

    let uow = store.transaction()?;
    uow.put_backup_record(&record)?;
    uow.commit()?;
    Ok(record)
}

/// 重新核验既有备份记录：文件缺失、摘要变化、完整性失败都算不可恢复。
pub fn verify_backup(record: &BackupRecord) -> Vec<String> {
    let mut problems = Vec::new();
    let path = Path::new(&record.path);
    if !path.is_file() {
        problems.push("备份文件缺失".to_string());
        return problems;
    }
    match fs::read(path) {
        Ok(bytes) => {
            let actual = sha256_hex(&bytes);
            if actual != record.db_sha256 {
                problems.push(format!(
                    "备份摘要变化：记录 {}，实际 {}",
                    record.db_sha256, actual
                ));
            }
        }
        Err(err) => problems.push(format!("无法读取备份: {err}")),
    }
    if !record.wal_included {
        problems.push("备份未包含 WAL 内容，不得标记为可恢复".to_string());
    }
    if record.key_recovery_note.trim().is_empty() {
        problems.push("缺少独立密钥恢复说明".to_string());
    }
    if let Some(reason) = integrity_problem(path) {
        problems.push(reason);
    }
    problems
}

/// 恢复报告：先核验，再统计，最后决定是否开放派发。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub target: String,
    pub accepted: bool,
    pub checks: Vec<String>,
    pub counts: CoreRowCounts,
    pub unknown_invocations: usize,
    pub open_cases: usize,
    /// 有未知外部动作时必须先对账，不得直接开放新派发。
    pub dispatch_allowed: bool,
    pub problems: Vec<String>,
}

/// 恢复到新宿主：复制快照、打开校验、统计未知动作；不自动重发任何外部动作。
pub fn restore_to_new_host(
    record: &BackupRecord,
    target: &Path,
    config: &crate::config::BodConfig,
) -> Result<RestoreReport, RecoveryError> {
    let problems = verify_backup(record);
    if record.status != BackupStatus::Verified {
        return Err(RecoveryError::BackupNotRecoverable(
            if problems.is_empty() {
                record.problems.clone()
            } else {
                problems
            },
        ));
    }
    if !problems.is_empty() {
        return Err(RecoveryError::BackupNotRecoverable(problems));
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&record.path, target)?;

    let store = SqliteStore::open(target, config)?;
    let counts = store.core_row_counts()?;
    let invocations = store.list_invocations(record.company_id.as_str())?;
    let unknown = invocations
        .iter()
        .filter(|item| item.state == ExecutionState::Unknown)
        .count();
    let open_cases = store
        .list_reconciliation_cases(record.company_id.as_str(), true)?
        .len();

    let mut checks = vec![
        format!("快照摘要 {}", record.db_sha256),
        format!("快照方法 {}", record.snapshot_method),
        format!("数据库完整性 {}", integrity_summary(target)),
        format!(
            "恢复行数：issues={} revisions={} invocations={} acceptances={}",
            counts.issues, counts.revisions, counts.invocations, counts.acceptances
        ),
        format!("未知外部动作 {unknown} 个，未结对账案件 {open_cases} 个"),
    ];
    if unknown > 0 || open_cases > 0 {
        checks.push("恢复后先对账，未结清前不开放新派发".to_string());
    }

    Ok(RestoreReport {
        target: target.to_string_lossy().to_string(),
        accepted: true,
        checks,
        counts,
        unknown_invocations: unknown,
        open_cases,
        dispatch_allowed: unknown == 0 && open_cases == 0,
        problems: Vec::new(),
    })
}

fn integrity_problem(path: &Path) -> Option<String> {
    match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => {
            match conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
                Ok(result) if result.eq_ignore_ascii_case("ok") => None,
                Ok(result) => Some(format!("完整性检查失败: {result}")),
                Err(err) => Some(format!("无法执行完整性检查: {err}")),
            }
        }
        Err(err) => Some(format!("无法打开快照: {err}")),
    }
}

fn integrity_summary(path: &Path) -> String {
    integrity_problem(path).unwrap_or_else(|| "ok".to_string())
}

/// 默认备份目录：Company 数据目录下的 backups/。
pub fn default_backup_dir(base: &Path) -> PathBuf {
    base.join("backups")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::recovery_models::MigrationRecord;

    fn temp_root(tag: &str) -> PathBuf {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("bod-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(id: &str, checksum: &str) -> MigrationRecord {
        MigrationRecord {
            migration_id: id.to_string(),
            checksum: checksum.to_string(),
            depends_on: Vec::new(),
            schema_range: "bod.schema/v1".to_string(),
            software_range: ">=0.1.0".to_string(),
            risk: MigrationRisk::Safe,
            compatibility_window: "P0".to_string(),
            backup_notes: "先备份".to_string(),
            data_notes: "无转换".to_string(),
            applied_at_ms: 1,
        }
    }

    #[test]
    fn shipped_migrations_have_stable_checksums_and_no_destructive_ddl() {
        let shipped = shipped_migrations();
        assert_eq!(shipped.len(), 12);
        for item in &shipped {
            assert_eq!(item.checksum.len(), 64, "{} 应有 sha256", item.migration_id);
        }
        assert!(verify_shipped_migrations().is_empty());
        // 两次调用的 checksum 必须一致（编译期内容未变）。
        assert_eq!(shipped_migrations(), shipped);
    }

    #[test]
    fn registry_round_trip_has_no_issues() {
        let dir = temp_root("migr");
        let mut store = SqliteStore::open(&dir.join("bod.sqlite"), &BodConfig::default()).unwrap();
        let inserted = register_shipped_migrations(&mut store, 1).unwrap();
        assert_eq!(inserted, 12);
        let records = store.list_migration_records().unwrap();
        assert_eq!(records.len(), 12);
        assert!(verify_registry(&records).is_empty());

        // 幂等：重复登记不新增。
        register_shipped_migrations(&mut store, 2).unwrap();
        let again = store.list_migration_records().unwrap();
        assert_eq!(again.len(), 12);
    }

    #[test]
    fn checksum_drift_is_detected() {
        let mut items = vec![record("001_initial.sql", &"a".repeat(64))];
        items.extend(
            shipped_migrations()
                .into_iter()
                .filter(|item| item.migration_id != "001_initial.sql")
                .map(|item| record(item.migration_id, &item.checksum)),
        );
        let issues = verify_registry(&items);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code() == "MIGRATION_CHECKSUM_DRIFT")
        );
    }

    #[test]
    fn unknown_dependency_and_missing_metadata_are_detected() {
        let mut item = record("999_extra.sql", &"b".repeat(64));
        item.depends_on = vec!["404_missing.sql".to_string()];
        item.compatibility_window = String::new();
        item.backup_notes = String::new();
        let issues = verify_registry(&[item]);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code() == "MIGRATION_UNKNOWN_DEPENDENCY")
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.code() == "MIGRATION_MISSING_METADATA")
        );
    }

    #[test]
    fn destructive_migration_requires_window() {
        let mut item = record("900_rebuild.sql", &"c".repeat(64));
        item.risk = MigrationRisk::Destructive;
        item.compatibility_window = String::new();
        let issues = verify_registry(&[item]);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code() == "MIGRATION_DESTRUCTIVE_WITHOUT_WINDOW")
        );
    }

    #[test]
    fn destructive_ddl_detection_ignores_trigger_lines() {
        assert!(contains_destructive_ddl("DROP TABLE issues;"));
        assert!(contains_destructive_ddl("  delete from ledger_entries;"));
        assert!(!contains_destructive_ddl(
            "BEFORE DELETE ON artifact_revisions\nBEGIN\n  SELECT RAISE(ABORT, 'x');\nEND;"
        ));
    }

    fn backup_fixture() -> (SqliteStore, CasStore, PathBuf, PathBuf) {
        let dir = temp_root("backup");
        let mut store = SqliteStore::open(&dir.join("bod.sqlite"), &BodConfig::default()).unwrap();
        // 写入一条 Issue，确保快照非空。
        let uow = store.transaction().unwrap();
        uow.insert_issue(&sample_issue()).unwrap();
        uow.commit().unwrap();
        let cas = CasStore::open(dir.join("cas")).unwrap();
        (store, cas, dir.clone(), dir.join("backups"))
    }

    fn sample_issue() -> crate::domain::model::Issue {
        use crate::domain::model::{
            AcceptanceCriteria, IssueKind, IssueSpec, ResourceMeta, ResourceStatus,
        };
        use crate::domain::{DomainId, Phase, Version};
        crate::domain::model::Issue {
            meta: ResourceMeta {
                uid: DomainId::new("issue-1"),
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: IssueKind::General,
            repository: None,
            spec: IssueSpec {
                title: "备份样本".to_string(),
                assignment: None,
                acceptance: AcceptanceCriteria::default(),
            },
            status: ResourceStatus {
                phase: Phase::Pending,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        }
    }

    #[test]
    fn verified_backup_includes_wal_and_key_note() {
        let (mut store, cas, _dir, backups) = backup_fixture();
        let record = create_backup(
            &mut store,
            &cas,
            &BackupInput {
                company_id: "company-default",
                destination: &backups,
                config_revision: "cfg-r1",
                adapter_versions: "python=3.13.14",
                key_recovery_note: "启动密钥存于 OS 凭据管理器；恢复时重新录入",
            },
            "b1",
            10,
        )
        .unwrap();
        assert_eq!(record.status, BackupStatus::Verified);
        assert!(record.wal_included, "VACUUM INTO 合并 WAL 内容");
        assert!(record.db_bytes > 0);
        assert!(verify_backup(&record).is_empty());
        assert!(Path::new(&record.path).is_file());
    }

    #[test]
    fn missing_key_note_marks_incomplete() {
        let (mut store, cas, _dir, backups) = backup_fixture();
        let record = create_backup(
            &mut store,
            &cas,
            &BackupInput {
                company_id: "company-default",
                destination: &backups,
                config_revision: "cfg-r1",
                adapter_versions: "python=3.13.14",
                key_recovery_note: "   ",
            },
            "b2",
            11,
        )
        .unwrap();
        assert_eq!(record.status, BackupStatus::Incomplete);
        assert!(record.problems.iter().any(|item| item.contains("密钥")));
        assert!(!record.status.is_recoverable());
    }

    #[test]
    fn backup_without_wal_is_not_recoverable() {
        let (mut store, cas, _dir, backups) = backup_fixture();
        let mut record = create_backup(
            &mut store,
            &cas,
            &BackupInput {
                company_id: "company-default",
                destination: &backups,
                config_revision: "cfg-r1",
                adapter_versions: "python=3.13.14",
                key_recovery_note: "凭据管理器",
            },
            "b3",
            12,
        )
        .unwrap();
        record.wal_included = false;
        let problems = verify_backup(&record);
        assert!(problems.iter().any(|item| item.contains("WAL")));
    }

    #[test]
    fn corrupted_snapshot_is_rejected() {
        let (mut store, cas, _dir, backups) = backup_fixture();
        let record = create_backup(
            &mut store,
            &cas,
            &BackupInput {
                company_id: "company-default",
                destination: &backups,
                config_revision: "cfg-r1",
                adapter_versions: "python=3.13.14",
                key_recovery_note: "凭据管理器",
            },
            "b4",
            13,
        )
        .unwrap();
        fs::write(&record.path, b"not a database").unwrap();
        let problems = verify_backup(&record);
        assert!(problems.iter().any(|item| item.contains("摘要变化")));
        assert!(problems.iter().any(|item| item.contains("完整性")));
    }

    #[test]
    fn restore_reports_counts_and_opens_dispatch_when_clean() {
        let (mut store, cas, dir, backups) = backup_fixture();
        let record = create_backup(
            &mut store,
            &cas,
            &BackupInput {
                company_id: "company-default",
                destination: &backups,
                config_revision: "cfg-r1",
                adapter_versions: "python=3.13.14",
                key_recovery_note: "凭据管理器",
            },
            "b5",
            14,
        )
        .unwrap();
        let target = dir.join("restored.sqlite");
        let report = restore_to_new_host(&record, &target, &BodConfig::default()).unwrap();
        assert!(report.accepted);
        assert_eq!(report.counts.issues, 1);
        assert_eq!(report.unknown_invocations, 0);
        assert!(report.dispatch_allowed, "无未知动作时可直接开放派发");
        assert!(report.checks.iter().any(|item| item.contains("完整性")));
    }

    #[test]
    fn incomplete_backup_cannot_be_restored() {
        let (mut store, cas, dir, backups) = backup_fixture();
        let record = create_backup(
            &mut store,
            &cas,
            &BackupInput {
                company_id: "company-default",
                destination: &backups,
                config_revision: "cfg-r1",
                adapter_versions: "python=3.13.14",
                key_recovery_note: "",
            },
            "b6",
            15,
        )
        .unwrap();
        let err =
            restore_to_new_host(&record, &dir.join("x.sqlite"), &BodConfig::default()).unwrap_err();
        assert!(matches!(err, RecoveryError::BackupNotRecoverable(_)));
    }
}
