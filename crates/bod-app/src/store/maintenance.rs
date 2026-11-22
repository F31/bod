//! D6 恢复与清理：迁移/备份记录、对账、清理请求、行数盘点。
//!
//! 清理状态机只前进不回退（`update_cleanup_state` 校验当前态），
//! 放弃清理要留下 `CleanupAbandon`：中止也是一条要能被审计的事实。

use super::*;

/// 事务外/内共用的清理请求读取：`SqliteStore` 与 `UnitOfWork` 走同一 SQL。
fn read_cleanup_request(
    conn: &Connection,
    company_id: &str,
    kind: CleanupTargetKind,
    target_id: &str,
) -> Result<Option<CleanupRequest>, StoreError> {
    conn.query_row(
        "SELECT request_id, state, blockers, requested_by, created_at_ms, updated_at_ms
         FROM cleanup_requests
         WHERE company_id = ?1 AND target_kind = ?2 AND target_id = ?3",
        params![company_id, kind.as_str(), target_id],
        |row| {
            let blockers: String = row.get(2)?;
            Ok(CleanupRequest {
                request_id: row.get(0)?,
                company_id: DomainId::new(company_id.to_string()),
                target_kind: kind,
                target_id: target_id.to_string(),
                state: CleanupState::parse(&row.get::<_, String>(1)?),
                blockers: split_list(&blockers),
                requested_by: row.get(3)?,
                created_at_ms: row.get(4)?,
                updated_at_ms: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::Sql)
}

impl SqliteStore {
    pub fn list_migration_records(&self) -> Result<Vec<MigrationRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT migration_id, checksum, depends_on, schema_range, software_range, risk,
                    compatibility_window, backup_notes, data_notes, applied_at_ms
             FROM migration_registry ORDER BY migration_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let depends: String = row.get(2)?;
            Ok(MigrationRecord {
                migration_id: row.get(0)?,
                checksum: row.get(1)?,
                depends_on: MigrationRecord::parse_depends(&depends),
                schema_range: row.get(3)?,
                software_range: row.get(4)?,
                risk: MigrationRisk::parse(&row.get::<_, String>(5)?),
                compatibility_window: row.get(6)?,
                backup_notes: row.get(7)?,
                data_notes: row.get(8)?,
                applied_at_ms: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 一致性快照：VACUUM INTO 生成独立数据库文件，已提交的 WAL 内容被合并。
    pub fn vacuum_into(&self, path: &str) -> Result<(), StoreError> {
        self.conn
            .execute("VACUUM INTO ?1", params![path])
            .map_err(StoreError::Sql)?;
        Ok(())
    }

    /// 供恢复报告使用的核心行数（不读取业务内容）。
    pub fn core_row_counts(&self) -> Result<CoreRowCounts, StoreError> {
        let issues = self.count_table("issues")?;
        let revisions = self.count_table("artifact_revisions")?;
        let invocations = self.count_table("invocations")?;
        let acceptances = self.count_table("acceptances")?;
        Ok(CoreRowCounts {
            issues,
            revisions,
            invocations,
            acceptances,
        })
    }

    fn count_table(&self, table: &str) -> Result<i64, StoreError> {
        // 表名来自本函数的固定字面量，不接受外部输入。
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(StoreError::Sql)
    }

    pub fn latest_backup(&self, company_id: &str) -> Result<Option<BackupRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT backup_id, path, db_sha256, db_bytes, wal_included, snapshot_method,
                        cas_object_count, config_revision, adapter_versions, key_recovery_note,
                        status, problems, created_at_ms
                 FROM backups WHERE company_id = ?1 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                params![company_id],
                |row| {
                    let problems: String = row.get(11)?;
                    Ok(BackupRecord {
                        backup_id: row.get(0)?,
                        company_id: DomainId::new(company_id.to_string()),
                        path: row.get(1)?,
                        db_sha256: row.get(2)?,
                        db_bytes: row.get(3)?,
                        wal_included: row.get::<_, i64>(4)? == 1,
                        snapshot_method: row.get(5)?,
                        cas_object_count: row.get(6)?,
                        config_revision: row.get(7)?,
                        adapter_versions: row.get(8)?,
                        key_recovery_note: row.get(9)?,
                        status: BackupStatus::parse(&row.get::<_, String>(10)?),
                        problems: split_list(&problems),
                        created_at_ms: row.get(12)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 仍处于 requested/blocked 的清理请求：删除尚未完成的事实来源。
    pub fn list_open_cleanup_requests(
        &self,
        company_id: &str,
    ) -> Result<Vec<CleanupRequest>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT request_id, target_kind, target_id, state, blockers, requested_by,
                    created_at_ms, updated_at_ms
             FROM cleanup_requests
             WHERE company_id = ?1 AND state IN ('requested', 'blocked')
             ORDER BY created_at_ms, request_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let kind: String = row.get(1)?;
            let blockers: String = row.get(4)?;
            Ok(CleanupRequest {
                request_id: row.get(0)?,
                company_id: DomainId::new(company_id.to_string()),
                target_kind: CleanupTargetKind::parse(&kind),
                target_id: row.get(2)?,
                state: CleanupState::parse(&row.get::<_, String>(3)?),
                blockers: split_list(&blockers),
                requested_by: row.get(5)?,
                created_at_ms: row.get(6)?,
                updated_at_ms: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内修订的固定线索：接受指针 + 正式 Manifest 引用。
    ///
    /// Manifest 引用按修订逐个回查（P0 规模可接受）；此处只读，不写任何修订。
    pub fn list_project_revision_pins(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<RevisionPin>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.revision_id, r.artifact_id, r.storage_kind, r.storage_ref,
                    a.accepted_revision
             FROM artifact_revisions r
             JOIN artifacts a ON a.uid = r.artifact_id
             WHERE r.company_id = ?1 AND a.project_id = ?2
             ORDER BY r.revision_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            let revision_id: String = row.get(0)?;
            let kind: String = row.get(2)?;
            let reference: String = row.get(3)?;
            let accepted_revision: Option<String> = row.get(4)?;
            let storage = if kind == "external" {
                StorageRef::External(reference)
            } else {
                StorageRef::Cas(reference)
            };
            Ok(RevisionPin {
                accepted: accepted_revision.as_deref() == Some(revision_id.as_str()),
                revision_id,
                artifact_id: row.get(1)?,
                storage,
                manifest_refs: Vec::new(),
            })
        })?;
        let mut pins = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sql)?;
        for pin in &mut pins {
            pin.manifest_refs = self.manifest_referencing(&pin.revision_id)?;
        }
        Ok(pins)
    }

    pub fn list_reconciliation_cases(
        &self,
        company_id: &str,
        only_open: bool,
    ) -> Result<Vec<ReconciliationCase>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT case_id, invocation_id, logical_action_key, impact_scope,
                    risk_reservation_refs, known_evidence, assignee, next_step, state,
                    created_at_ms, updated_at_ms
             FROM reconciliation_cases
             WHERE company_id = ?1 AND (?2 = 0 OR state = 'open')
             ORDER BY created_at_ms",
        )?;
        let rows = stmt.query_map(params![company_id, i64::from(u8::from(only_open))], |row| {
            let refs: String = row.get(4)?;
            Ok(ReconciliationCase {
                case_id: row.get(0)?,
                company_id: DomainId::new(company_id.to_string()),
                invocation_id: row.get(1)?,
                logical_action_key: row.get(2)?,
                impact_scope: row.get(3)?,
                risk_reservation_refs: split_list(&refs),
                known_evidence: row.get(5)?,
                assignee: row.get(6)?,
                next_step: row.get(7)?,
                state: CaseState::parse(&row.get::<_, String>(8)?),
                created_at_ms: row.get(9)?,
                updated_at_ms: row.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    pub fn get_cleanup_request(
        &self,
        company_id: &str,
        kind: CleanupTargetKind,
        target_id: &str,
    ) -> Result<Option<CleanupRequest>, StoreError> {
        read_cleanup_request(&self.conn, company_id, kind, target_id)
    }
}

impl<'a> UnitOfWork<'a> {
    pub fn put_migration_record(&self, record: &MigrationRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO migration_registry (
                migration_id, checksum, depends_on, schema_range, software_range, risk,
                compatibility_window, backup_notes, data_notes, applied_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(migration_id) DO NOTHING",
            params![
                record.migration_id,
                record.checksum,
                record.depends_on_text(),
                record.schema_range,
                record.software_range,
                record.risk.as_str(),
                record.compatibility_window,
                record.backup_notes,
                record.data_notes,
                record.applied_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn put_backup_record(&self, record: &BackupRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO backups (
                backup_id, company_id, path, db_sha256, db_bytes, wal_included,
                snapshot_method, cas_object_count, config_revision, adapter_versions,
                key_recovery_note, status, problems, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.backup_id,
                record.company_id.as_str(),
                record.path,
                record.db_sha256,
                record.db_bytes,
                i64::from(u8::from(record.wal_included)),
                record.snapshot_method,
                record.cas_object_count,
                record.config_revision,
                record.adapter_versions,
                record.key_recovery_note,
                record.status.as_str(),
                record.problems.join(ARRAY_SEP),
                record.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn put_reconciliation_case(&self, case: &ReconciliationCase) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO reconciliation_cases (
                case_id, company_id, invocation_id, logical_action_key, impact_scope,
                risk_reservation_refs, known_evidence, assignee, next_step, state,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(case_id) DO UPDATE SET
                known_evidence = excluded.known_evidence,
                assignee = excluded.assignee,
                next_step = excluded.next_step,
                state = excluded.state,
                updated_at_ms = excluded.updated_at_ms",
            params![
                case.case_id,
                case.company_id.as_str(),
                case.invocation_id,
                case.logical_action_key,
                case.impact_scope,
                case.risk_reservation_refs.join(ARRAY_SEP),
                case.known_evidence,
                case.assignee,
                case.next_step,
                case.state.as_str(),
                case.created_at_ms,
                case.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 幂等写入：同一目标只保留一条清理记录，重复请求返回已存在记录。
    pub fn put_cleanup_request(&self, request: &CleanupRequest) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "INSERT INTO cleanup_requests (
                request_id, company_id, target_kind, target_id, state, blockers,
                requested_by, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(company_id, target_kind, target_id) DO NOTHING",
            params![
                request.request_id,
                request.company_id.as_str(),
                request.target_kind.as_str(),
                request.target_id,
                request.state.as_str(),
                request.blockers.join(ARRAY_SEP),
                request.requested_by,
                request.created_at_ms,
                request.updated_at_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    pub fn update_cleanup_state(
        &self,
        request_id: &str,
        state: CleanupState,
        blockers: &[String],
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE cleanup_requests SET state = ?1, blockers = ?2, updated_at_ms = ?3
             WHERE request_id = ?4",
            params![state.as_str(), blockers.join(ARRAY_SEP), now_ms, request_id],
        )?;
        Ok(())
    }

    pub fn put_cleanup_abandon(&self, abandon: &CleanupAbandon) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO cleanup_abandons (
                abandon_id, company_id, request_id, residual_resources, risks, owner,
                reconciliation_entry, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                abandon.abandon_id,
                abandon.company_id.as_str(),
                abandon.request_id,
                abandon.residual_resources.join(ARRAY_SEP),
                abandon.risks.join(ARRAY_SEP),
                abandon.owner,
                abandon.reconciliation_entry,
                abandon.created_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 事务内读取清理请求：派发门禁必须在同一事务里看到删除标记。
    pub fn get_cleanup_request(
        &self,
        company_id: &str,
        kind: CleanupTargetKind,
        target_id: &str,
    ) -> Result<Option<CleanupRequest>, StoreError> {
        read_cleanup_request(self.conn, company_id, kind, target_id)
    }

    /// 标记项目内 Issue 的 `deletionRequestedAt`（幂等：已有标记不覆盖）。
    ///
    /// 该标记是"停止新派发"的事实来源，与 `cleanup_requests` 行一起在同一事务写入。
    pub fn mark_project_deletion_requested(
        &self,
        company_id: &str,
        project_id: &str,
        now_ms: i64,
    ) -> Result<usize, StoreError> {
        let rows = self.conn.execute(
            "UPDATE issues SET deletion_requested_at = ?3
             WHERE company_id = ?1 AND project_id = ?2 AND deletion_requested_at IS NULL",
            params![company_id, project_id, now_ms.to_string()],
        )?;
        Ok(rows)
    }

    /// 标记单个 Issue 的删除请求。
    pub fn mark_issue_deletion_requested(
        &self,
        company_id: &str,
        issue_uid: &str,
        now_ms: i64,
    ) -> Result<usize, StoreError> {
        let rows = self.conn.execute(
            "UPDATE issues SET deletion_requested_at = ?3
             WHERE company_id = ?1 AND uid = ?2 AND deletion_requested_at IS NULL",
            params![company_id, issue_uid, now_ms.to_string()],
        )?;
        Ok(rows)
    }
}
