//! P2 治理：交接快照与事件、接管意图与确认、管理委任、作用域头、检查点、历史争议。
//!
//! 委任与作用域头都带 `authority_epoch`：epoch 变了，之前发出的授权即失效，
//! 不需要逐个撤销——这是「换人不必追账」的实现方式。

use super::*;

/// 委任读取列。
const APPOINTMENT_SELECT: &str = "SELECT company_id, appointment_id, scope_key, manager_agent_id,
        grant_ref, authority_epoch, status, appointed_by, appointed_at, expires_at, audience,
        commands_json, version, recorded_at_ms
     FROM management_appointments";

fn map_appointment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppointmentRow> {
    Ok(AppointmentRow {
        company_id: row.get(0)?,
        appointment_id: row.get(1)?,
        scope_key: row.get(2)?,
        manager_agent_id: row.get(3)?,
        grant_ref: row.get(4)?,
        authority_epoch: row.get::<_, i64>(5)? as u64,
        status: row.get(6)?,
        appointed_by: row.get(7)?,
        appointed_at: row.get(8)?,
        expires_at: row.get(9)?,
        audience: row.get(10)?,
        commands_json: row.get(11)?,
        version: row.get::<_, i64>(12)? as u64,
        recorded_at_ms: row.get(13)?,
    })
}

/// 作用域头读取列。
const SCOPE_HEAD_SELECT: &str = "SELECT company_id, scope_key, current_appointment_id,
        authority_epoch, version, updated_at, recorded_at_ms
     FROM management_scope_heads";

/// 管理命令审计读取列。
const MANAGEMENT_COMMAND_SELECT: &str = "SELECT company_id, command_id, scope_key,
        appointment_id, authority_epoch, command, outcome, reason_code, reason, at, recorded_at_ms
     FROM management_commands";

fn map_management_command_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ManagementCommandRow> {
    Ok(ManagementCommandRow {
        company_id: row.get(0)?,
        command_id: row.get(1)?,
        scope_key: row.get(2)?,
        appointment_id: row.get(3)?,
        authority_epoch: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
        command: row.get(5)?,
        outcome: row.get(6)?,
        reason_code: row.get(7)?,
        reason: row.get(8)?,
        at: row.get(9)?,
        recorded_at_ms: row.get(10)?,
    })
}

/// 接管意图读取列。
const TAKEOVER_INTENT_SELECT: &str = "SELECT company_id, takeover_key, intent_id, project_id,
        issue_id, work_package_revision, from_attempt_id, requested_by, reason,
        baseline_fencing_token, created_at, phase, version, summary_state, summary_requested_at,
        summary_snapshot_key, attempt_id, fencing_token, abort_reason, history_json, updated_at,
        recorded_at_ms
     FROM takeover_intents";

/// 检查点读取列。
const CHECKPOINT_SELECT: &str = "SELECT company_id, checkpoint_id, scope_key, project_id, cursor,
        item_count, items_json, gaps_json, captured_by, captured_at, recorded_at_ms
     FROM handover_checkpoints";

fn map_checkpoint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRow> {
    Ok(CheckpointRow {
        company_id: row.get(0)?,
        checkpoint_id: row.get(1)?,
        scope_key: row.get(2)?,
        project_id: row.get(3)?,
        cursor: row.get(4)?,
        item_count: row.get(5)?,
        items_json: row.get(6)?,
        gaps_json: row.get(7)?,
        captured_by: row.get(8)?,
        captured_at: row.get(9)?,
        recorded_at_ms: row.get(10)?,
    })
}

/// 接管确认读取列。
const CONFIRMATION_SELECT: &str = "SELECT company_id, confirmation_id, scope_key, checkpoint_id,
        checkpoint_cursor, reviewed_cursor, appointment_id, authority_epoch, decision,
        confirmed_by, note, open_items_json, delta_json, confirmed_at, recorded_at_ms
     FROM takeover_confirmations";

fn map_confirmation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConfirmationRow> {
    Ok(ConfirmationRow {
        company_id: row.get(0)?,
        confirmation_id: row.get(1)?,
        scope_key: row.get(2)?,
        checkpoint_id: row.get(3)?,
        checkpoint_cursor: row.get(4)?,
        reviewed_cursor: row.get(5)?,
        appointment_id: row.get(6)?,
        authority_epoch: row.get::<_, i64>(7)? as u64,
        decision: row.get(8)?,
        confirmed_by: row.get(9)?,
        note: row.get(10)?,
        open_items_json: row.get(11)?,
        delta_json: row.get(12)?,
        confirmed_at: row.get(13)?,
        recorded_at_ms: row.get(14)?,
    })
}

fn map_dispute_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DisputeRow> {
    Ok(DisputeRow {
        dispute_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        fact_kind: row.get(3)?,
        fact_id: row.get(4)?,
        fact_fingerprint: row.get(5)?,
        reason: row.get(6)?,
        evidence_refs: row.get(7)?,
        raised_by: row.get(8)?,
        state: row.get(9)?,
        resolution: row.get(10)?,
        resolved_by: row.get(11)?,
        resolved_at_ms: row.get(12)?,
        created_at_ms: row.get(13)?,
        updated_at_ms: row.get(14)?,
    })
}

fn map_takeover_intent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TakeoverIntentRow> {
    Ok(TakeoverIntentRow {
        company_id: row.get(0)?,
        takeover_key: row.get(1)?,
        intent_id: row.get(2)?,
        project_id: row.get(3)?,
        issue_id: row.get(4)?,
        work_package_revision: row.get(5)?,
        from_attempt_id: row.get(6)?,
        requested_by: row.get(7)?,
        reason: row.get(8)?,
        baseline_fencing_token: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
        created_at: row.get(10)?,
        phase: row.get(11)?,
        version: row.get::<_, i64>(12)? as u64,
        summary_state: row.get(13)?,
        summary_requested_at: row.get(14)?,
        summary_snapshot_key: row.get(15)?,
        attempt_id: row.get(16)?,
        fencing_token: row.get::<_, Option<i64>>(17)?.map(|value| value as u64),
        abort_reason: row.get(18)?,
        history_json: row.get(19)?,
        updated_at: row.get(20)?,
        recorded_at_ms: row.get(21)?,
    })
}

/// 交接事件读取列。
const HANDOFF_EVENT_SELECT: &str = "SELECT company_id, handoff_id, seq, event_id, kind, reason,
        actor_kind, actor_ref, occurred_at, evidence_ref, recorded_at_ms
     FROM handoff_events";

fn map_handoff_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffEventRow> {
    Ok(HandoffEventRow {
        company_id: row.get(0)?,
        handoff_id: row.get(1)?,
        seq: row.get::<_, i64>(2)? as u64,
        event_id: row.get(3)?,
        kind: row.get(4)?,
        reason: row.get(5)?,
        actor_kind: row.get(6)?,
        actor_ref: row.get(7)?,
        occurred_at: row.get(8)?,
        evidence_ref: row.get(9)?,
        recorded_at_ms: row.get(10)?,
    })
}

/// 交接接受读取列。
const HANDOFF_ACCEPTANCE_SELECT: &str = "SELECT company_id, handoff_id, acceptance_key,
        request_digest, accepted_event_id, accepted_seq, snapshot_key, level, accepted_at,
        actor_kind, actor_ref, recorded_at_ms
     FROM handoff_acceptances";

/// 交接快照的读取列；`list_*` 与 `get_*` 共用，避免列序漂移。
const HANDOFF_SNAPSHOT_SELECT: &str = "SELECT company_id, snapshot_key, project_id, issue_id,
        work_package_revision, source_attempt_id, level, source, completeness, missing_json,
        integrity_json, input_revision, scene_revision, snapshot_seq, payload_digest, captured_at,
        registered_at_ms
     FROM handoff_snapshots";

fn map_handoff_snapshot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffSnapshotRow> {
    Ok(HandoffSnapshotRow {
        company_id: row.get(0)?,
        snapshot_key: row.get(1)?,
        project_id: row.get(2)?,
        issue_id: row.get(3)?,
        work_package_revision: row.get(4)?,
        source_attempt_id: row.get(5)?,
        level: row.get(6)?,
        source: row.get(7)?,
        completeness: row.get(8)?,
        missing_json: row.get(9)?,
        integrity_json: row.get(10)?,
        input_revision: row.get(11)?,
        scene_revision: row.get(12)?,
        snapshot_seq: row.get::<_, i64>(13)? as u64,
        payload_digest: row.get(14)?,
        captured_at: row.get(15)?,
        registered_at_ms: row.get(16)?,
    })
}

impl SqliteStore {
    /// 插入争议行。主键冲突由调用方按幂等/ID 复用分别处理。
    pub fn insert_history_dispute(&self, row: &DisputeRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO history_disputes (
                dispute_id, company_id, project_id, fact_kind, fact_id, fact_fingerprint,
                reason, evidence_refs, raised_by, state, resolution, resolved_by,
                resolved_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                row.dispute_id,
                row.company_id,
                row.project_id,
                row.fact_kind,
                row.fact_id,
                row.fact_fingerprint,
                row.reason,
                row.evidence_refs,
                row.raised_by,
                row.state,
                row.resolution,
                row.resolved_by,
                row.resolved_at_ms,
                row.created_at_ms,
                row.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_history_dispute(&self, dispute_id: &str) -> Result<Option<DisputeRow>, StoreError> {
        self.conn
            .query_row(
                "SELECT dispute_id, company_id, project_id, fact_kind, fact_id, fact_fingerprint,
                        reason, evidence_refs, raised_by, state, resolution, resolved_by,
                        resolved_at_ms, created_at_ms, updated_at_ms
                 FROM history_disputes WHERE dispute_id = ?1",
                params![dispute_id],
                map_dispute_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn list_history_disputes(
        &self,
        company_id: &str,
        project_id: &str,
        state: Option<&str>,
    ) -> Result<Vec<DisputeRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT dispute_id, company_id, project_id, fact_kind, fact_id, fact_fingerprint,
                    reason, evidence_refs, raised_by, state, resolution, resolved_by,
                    resolved_at_ms, created_at_ms, updated_at_ms
             FROM history_disputes
             WHERE company_id = ?1 AND project_id = ?2 AND (?3 IS NULL OR state = ?3)
             ORDER BY created_at_ms, dispute_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id, state], map_dispute_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 结案：只有 `under_review` 行会被改，返回是否真的改到（并发安全）。
    pub fn resolve_history_dispute(
        &self,
        dispute_id: &str,
        state: &str,
        resolution: &str,
        resolved_by: &str,
        at_ms: i64,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE history_disputes
             SET state = ?2, resolution = ?3, resolved_by = ?4, resolved_at_ms = ?5,
                 updated_at_ms = ?5
             WHERE dispute_id = ?1 AND state = 'under_review'",
            params![dispute_id, state, resolution, resolved_by, at_ms],
        )?;
        Ok(changed == 1)
    }

    /// 公司级全部委任（只读）：专项边界的归属需要一次看全项目/各专项的活跃委任。
    pub fn list_company_appointments(
        &self,
        company_id: &str,
    ) -> Result<Vec<AppointmentRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{APPOINTMENT_SELECT} WHERE company_id = ?1 ORDER BY scope_key, authority_epoch"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_appointment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 公司级全部租约事实（只读）：reportsTo 变更前后比对「未 fencing」。
    pub fn list_company_leases(&self, company_id: &str) -> Result<Vec<LeaseFactRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT resource_key, owner, lease_token, lease_expires_at, state
             FROM leases WHERE company_id = ?1 ORDER BY resource_key",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok(LeaseFactRow {
                resource_key: row.get(0)?,
                owner: row.get(1)?,
                lease_token: row.get(2)?,
                expires_at: row.get(3)?,
                state: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 公司级全部授权事实（只读）：权限不因组织字段变化而迁移。
    pub fn list_company_grants(&self, company_id: &str) -> Result<Vec<GrantFactRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, project_id, permission, revision, active
             FROM grants WHERE company_id = ?1 ORDER BY principal_id, permission, revision",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok(GrantFactRow {
                principal_id: row.get(0)?,
                project_id: row.get(1)?,
                permission: row.get(2)?,
                revision: row.get(3)?,
                active: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内父子层级（只读）。
    pub fn list_project_hierarchy(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<HierarchyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_uid, parent_issue_uid FROM issue_hierarchy
             WHERE company_id = ?1 AND project_id = ?2 ORDER BY issue_uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(HierarchyRow {
                issue_uid: row.get(0)?,
                parent_issue_uid: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 登记一条父子关系。层级只写一次，改父节点由治理流程重建（本期不提供）。
    pub fn insert_issue_hierarchy(
        &self,
        company_id: &str,
        project_id: &str,
        issue_uid: &str,
        parent_issue_uid: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO issue_hierarchy (company_id, project_id, issue_uid, parent_issue_uid,
                created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![company_id, project_id, issue_uid, parent_issue_uid, now_ms],
        )?;
        Ok(())
    }

    /// 成员是否存在（组织元数据只对已登记主体写入，避免拼写错误变成新手）。
    pub fn member_exists(&self, company_id: &str, uid: &str) -> Result<bool, StoreError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE company_id = ?1 AND uid = ?2",
            params![company_id, uid],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_org_metadata(
        &self,
        company_id: &str,
        agent_id: &str,
    ) -> Result<Option<OrgMetadataRow>, StoreError> {
        self.conn
            .query_row(
                "SELECT agent_id, reports_to, updated_at_ms, updated_by
                 FROM agent_org_metadata WHERE company_id = ?1 AND agent_id = ?2",
                params![company_id, agent_id],
                |row| {
                    Ok(OrgMetadataRow {
                        agent_id: row.get(0)?,
                        reports_to: row.get(1)?,
                        updated_at_ms: row.get(2)?,
                        updated_by: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn upsert_org_metadata(
        &self,
        company_id: &str,
        row: &OrgMetadataRow,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO agent_org_metadata (company_id, agent_id, reports_to, updated_at_ms,
                updated_by) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(company_id, agent_id) DO UPDATE SET
                reports_to = excluded.reports_to,
                updated_at_ms = excluded.updated_at_ms,
                updated_by = excluded.updated_by",
            params![
                company_id,
                row.agent_id,
                row.reports_to,
                row.updated_at_ms,
                row.updated_by
            ],
        )?;
        Ok(())
    }

    /// 插入一条交接快照。同 `(company_id, snapshot_key)` 已存在时**不做任何修改**并返回 `false`：
    /// 部分现场一旦登记就不得被后续登记覆盖（§18.11），冲突由调用方读回后判定。
    pub fn insert_handoff_snapshot(&self, row: &HandoffSnapshotRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handoff_snapshots (
                company_id, snapshot_key, project_id, issue_id, work_package_revision,
                source_attempt_id, level, source, completeness, missing_json, integrity_json,
                input_revision, scene_revision, snapshot_seq, payload_digest, captured_at,
                registered_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(company_id, snapshot_key) DO NOTHING",
            params![
                row.company_id,
                row.snapshot_key,
                row.project_id,
                row.issue_id,
                row.work_package_revision,
                row.source_attempt_id,
                row.level,
                row.source,
                row.completeness,
                row.missing_json,
                row.integrity_json,
                row.input_revision,
                row.scene_revision,
                row.snapshot_seq as i64,
                row.payload_digest,
                row.captured_at,
                row.registered_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 按 `(company_id, snapshot_key)` 读取已登记快照。
    pub fn get_handoff_snapshot(
        &self,
        company_id: &str,
        snapshot_key: &str,
    ) -> Result<Option<HandoffSnapshotRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{HANDOFF_SNAPSHOT_SELECT} WHERE company_id = ?1 AND snapshot_key = ?2"),
                params![company_id, snapshot_key],
                map_handoff_snapshot_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 列出某 Company 的全部快照（按 snapshot_seq 升序），供人工恢复查看。
    pub fn list_handoff_snapshots(
        &self,
        company_id: &str,
    ) -> Result<Vec<HandoffSnapshotRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{HANDOFF_SNAPSHOT_SELECT} WHERE company_id = ?1 ORDER BY snapshot_seq"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_handoff_snapshot_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 只列部分快照：崩溃、预算耗尽与强制停止后留给人工恢复的现场。
    pub fn list_partial_handoff_snapshots(
        &self,
        company_id: &str,
    ) -> Result<Vec<HandoffSnapshotRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{HANDOFF_SNAPSHOT_SELECT}
             WHERE company_id = ?1 AND completeness = 'partial' ORDER BY snapshot_seq"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_handoff_snapshot_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 追加一条交接事件。同 `(company_id, handoff_id, event_id)` 已存在时不修改并返回 `false`：
    /// 事件只追加，重复提交由调用方读回比对内容。
    pub fn insert_handoff_event(&self, row: &HandoffEventRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handoff_events (
                company_id, handoff_id, seq, event_id, kind, reason, actor_kind, actor_ref,
                occurred_at, evidence_ref, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(company_id, handoff_id, event_id) DO NOTHING",
            params![
                row.company_id,
                row.handoff_id,
                row.seq as i64,
                row.event_id,
                row.kind,
                row.reason,
                row.actor_kind,
                row.actor_ref,
                row.occurred_at,
                row.evidence_ref,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 按事件 id 读取单条事件（重复提交时比对内容用）。
    pub fn get_handoff_event(
        &self,
        company_id: &str,
        handoff_id: &str,
        event_id: &str,
    ) -> Result<Option<HandoffEventRow>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "{HANDOFF_EVENT_SELECT}
                     WHERE company_id = ?1 AND handoff_id = ?2 AND event_id = ?3"
                ),
                params![company_id, handoff_id, event_id],
                map_handoff_event_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 读取某交接的完整时间线（按 seq 升序）。
    pub fn list_handoff_events(
        &self,
        company_id: &str,
        handoff_id: &str,
    ) -> Result<Vec<HandoffEventRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{HANDOFF_EVENT_SELECT} WHERE company_id = ?1 AND handoff_id = ?2 ORDER BY seq"
        ))?;
        let rows = stmt
            .query_map(params![company_id, handoff_id], map_handoff_event_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 登记一次接受。主键为 `(company_id, handoff_id)`：一个交接只能被接受一次，
    /// 并发/重复接受都无法插入第二行（返回 `false`，由调用方读回判定）。
    pub fn insert_handoff_acceptance(
        &self,
        row: &HandoffAcceptanceRow,
    ) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handoff_acceptances (
                company_id, handoff_id, acceptance_key, request_digest, accepted_event_id,
                accepted_seq, snapshot_key, level, accepted_at, actor_kind, actor_ref,
                recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(company_id, handoff_id) DO NOTHING",
            params![
                row.company_id,
                row.handoff_id,
                row.acceptance_key,
                row.request_digest,
                row.accepted_event_id,
                row.accepted_seq as i64,
                row.snapshot_key,
                row.level,
                row.accepted_at,
                row.actor_kind,
                row.actor_ref,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 读取某交接的接受记录。
    pub fn get_handoff_acceptance(
        &self,
        company_id: &str,
        handoff_id: &str,
    ) -> Result<Option<HandoffAcceptanceRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{HANDOFF_ACCEPTANCE_SELECT} WHERE company_id = ?1 AND handoff_id = ?2"),
                params![company_id, handoff_id],
                |row| {
                    Ok(HandoffAcceptanceRow {
                        company_id: row.get(0)?,
                        handoff_id: row.get(1)?,
                        acceptance_key: row.get(2)?,
                        request_digest: row.get(3)?,
                        accepted_event_id: row.get(4)?,
                        accepted_seq: row.get::<_, i64>(5)? as u64,
                        snapshot_key: row.get(6)?,
                        level: row.get(7)?,
                        accepted_at: row.get(8)?,
                        actor_kind: row.get(9)?,
                        actor_ref: row.get(10)?,
                        recorded_at_ms: row.get(11)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 插入一条接管意图。同 `(company_id, takeover_key)` 已存在时不修改并返回 `false`：
    /// 幂等开单由调用方读回比对内容，不允许覆盖第一次提出的切换。
    pub fn insert_takeover_intent(&self, row: &TakeoverIntentRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO takeover_intents (
                company_id, takeover_key, intent_id, project_id, issue_id,
                work_package_revision, from_attempt_id, requested_by, reason,
                baseline_fencing_token, created_at, phase, version, summary_state,
                summary_requested_at, summary_snapshot_key, attempt_id, fencing_token,
                abort_reason, history_json, updated_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
             ON CONFLICT(company_id, takeover_key) DO NOTHING",
            params![
                row.company_id,
                row.takeover_key,
                row.intent_id,
                row.project_id,
                row.issue_id,
                row.work_package_revision,
                row.from_attempt_id,
                row.requested_by,
                row.reason,
                row.baseline_fencing_token.map(|value| value as i64),
                row.created_at,
                row.phase,
                row.version as i64,
                row.summary_state,
                row.summary_requested_at,
                row.summary_snapshot_key,
                row.attempt_id,
                row.fencing_token.map(|value| value as i64),
                row.abort_reason,
                row.history_json,
                row.updated_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// 按 `(company_id, takeover_key)` 读取意图。
    pub fn get_takeover_intent(
        &self,
        company_id: &str,
        takeover_key: &str,
    ) -> Result<Option<TakeoverIntentRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{TAKEOVER_INTENT_SELECT} WHERE company_id = ?1 AND takeover_key = ?2"),
                params![company_id, takeover_key],
                map_takeover_intent_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 列出某 Company 的全部意图（按提出时间）。重启扫描待接管工作用。
    pub fn list_takeover_intents(
        &self,
        company_id: &str,
    ) -> Result<Vec<TakeoverIntentRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{TAKEOVER_INTENT_SELECT} WHERE company_id = ?1 ORDER BY created_at, takeover_key"
        ))?;
        let rows = stmt
            .query_map(params![company_id], map_takeover_intent_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 条件更新：只有库里的 `version` 仍等于 `expected_version` 才写入，并把它 +1。
    ///
    /// 返回是否真的改了行；`false` 表示别人的推进已经先到，调用方必须报版本冲突而不是覆盖。
    pub fn update_takeover_intent_cas(
        &self,
        row: &TakeoverIntentRow,
        expected_version: u64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE takeover_intents SET
                intent_id = ?3, project_id = ?4, issue_id = ?5, work_package_revision = ?6,
                from_attempt_id = ?7, requested_by = ?8, reason = ?9,
                baseline_fencing_token = ?10, created_at = ?11, phase = ?12, version = ?13,
                summary_state = ?14, summary_requested_at = ?15, summary_snapshot_key = ?16,
                attempt_id = ?17, fencing_token = ?18, abort_reason = ?19, history_json = ?20,
                updated_at = ?21, recorded_at_ms = ?22
             WHERE company_id = ?1 AND takeover_key = ?2 AND version = ?23",
            params![
                row.company_id,
                row.takeover_key,
                row.intent_id,
                row.project_id,
                row.issue_id,
                row.work_package_revision,
                row.from_attempt_id,
                row.requested_by,
                row.reason,
                row.baseline_fencing_token.map(|value| value as i64),
                row.created_at,
                row.phase,
                row.version as i64,
                row.summary_state,
                row.summary_requested_at,
                row.summary_snapshot_key,
                row.attempt_id,
                row.fencing_token.map(|value| value as i64),
                row.abort_reason,
                row.history_json,
                row.updated_at,
                row.recorded_at_ms,
                expected_version as i64,
            ],
        )?;
        Ok(updated == 1)
    }

    /// 插入一条委任行。`(company_id, appointment_id)` 或
    /// `(company_id, scope_key, authority_epoch)` 冲突时返回 `false`：
    /// 同一 scope 的同一 epoch 不允许出现第二个委任，也不允许覆盖既有行。
    pub fn insert_appointment(&self, row: &AppointmentRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO management_appointments (
                company_id, appointment_id, scope_key, manager_agent_id, grant_ref,
                authority_epoch, status, appointed_by, appointed_at, expires_at, audience,
                commands_json, version, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.appointment_id,
                row.scope_key,
                row.manager_agent_id,
                row.grant_ref,
                row.authority_epoch as i64,
                row.status,
                row.appointed_by,
                row.appointed_at,
                row.expires_at,
                row.audience,
                row.commands_json,
                row.version as i64,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_appointment(
        &self,
        company_id: &str,
        appointment_id: &str,
    ) -> Result<Option<AppointmentRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{APPOINTMENT_SELECT} WHERE company_id = ?1 AND appointment_id = ?2"),
                params![company_id, appointment_id],
                map_appointment_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 的历史委任，按 epoch 升序。
    pub fn list_appointments(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<AppointmentRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{APPOINTMENT_SELECT} WHERE company_id = ?1 AND scope_key = ?2
             ORDER BY authority_epoch"
        ))?;
        let rows = stmt
            .query_map(params![company_id, scope_key], map_appointment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 条件更新委任行：`version` 必须仍等于 `expected_version`。
    pub fn update_appointment_cas(
        &self,
        row: &AppointmentRow,
        expected_version: u64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE management_appointments SET
                manager_agent_id = ?3, grant_ref = ?4, authority_epoch = ?5, status = ?6,
                appointed_by = ?7, appointed_at = ?8, expires_at = ?9, audience = ?10,
                commands_json = ?11, version = ?12, recorded_at_ms = ?13
             WHERE company_id = ?1 AND appointment_id = ?2 AND version = ?14",
            params![
                row.company_id,
                row.appointment_id,
                row.manager_agent_id,
                row.grant_ref,
                row.authority_epoch as i64,
                row.status,
                row.appointed_by,
                row.appointed_at,
                row.expires_at,
                row.audience,
                row.commands_json,
                row.version as i64,
                row.recorded_at_ms,
                expected_version as i64,
            ],
        )?;
        Ok(updated == 1)
    }

    /// 插入作用域头。主键冲突返回 `false`：该 scope 已有头指针，必须走条件更新。
    pub fn insert_scope_head(&self, row: &ScopeHeadRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO management_scope_heads (
                company_id, scope_key, current_appointment_id, authority_epoch, version,
                updated_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.scope_key,
                row.current_appointment_id,
                row.authority_epoch as i64,
                row.version as i64,
                row.updated_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_scope_head(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Option<ScopeHeadRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{SCOPE_HEAD_SELECT} WHERE company_id = ?1 AND scope_key = ?2"),
                params![company_id, scope_key],
                |row| {
                    Ok(ScopeHeadRow {
                        company_id: row.get(0)?,
                        scope_key: row.get(1)?,
                        current_appointment_id: row.get(2)?,
                        authority_epoch: row.get::<_, i64>(3)? as u64,
                        version: row.get::<_, i64>(4)? as u64,
                        updated_at: row.get(5)?,
                        recorded_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 条件更新作用域头：`version` 不等就说明别人先改了指针/epoch，返回 `false`。
    pub fn update_scope_head_cas(
        &self,
        row: &ScopeHeadRow,
        expected_version: u64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE management_scope_heads SET
                current_appointment_id = ?3, authority_epoch = ?4, version = ?5,
                updated_at = ?6, recorded_at_ms = ?7
             WHERE company_id = ?1 AND scope_key = ?2 AND version = ?8",
            params![
                row.company_id,
                row.scope_key,
                row.current_appointment_id,
                row.authority_epoch as i64,
                row.version as i64,
                row.updated_at,
                row.recorded_at_ms,
                expected_version as i64,
            ],
        )?;
        Ok(updated == 1)
    }

    /// 追加一条管理命令审计。同 `(company_id, command_id)` 已存在时不改写并返回 `false`。
    pub fn insert_management_command(
        &self,
        row: &ManagementCommandRow,
    ) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO management_commands (
                company_id, command_id, scope_key, appointment_id, authority_epoch, command,
                outcome, reason_code, reason, at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.command_id,
                row.scope_key,
                row.appointment_id,
                row.authority_epoch.map(|value| value as i64),
                row.command,
                row.outcome,
                row.reason_code,
                row.reason,
                row.at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_management_command(
        &self,
        company_id: &str,
        command_id: &str,
    ) -> Result<Option<ManagementCommandRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{MANAGEMENT_COMMAND_SELECT} WHERE company_id = ?1 AND command_id = ?2"),
                params![company_id, command_id],
                map_management_command_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 的命令审计（按时间升序），供核对「拒绝留下了原因」。
    pub fn list_management_commands(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<ManagementCommandRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{MANAGEMENT_COMMAND_SELECT} WHERE company_id = ?1 AND scope_key = ?2 ORDER BY at, command_id"
        ))?;
        let rows = stmt
            .query_map(params![company_id, scope_key], map_management_command_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 登记检查点：同 `(scope, cursor)` 已存在时不改写并返回 `false`（同现场不产生第二份）。
    pub fn insert_handover_checkpoint(&self, row: &CheckpointRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO handover_checkpoints (
                company_id, checkpoint_id, scope_key, project_id, cursor, item_count,
                items_json, gaps_json, captured_by, captured_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.checkpoint_id,
                row.scope_key,
                row.project_id,
                row.cursor,
                row.item_count,
                row.items_json,
                row.gaps_json,
                row.captured_by,
                row.captured_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_handover_checkpoint(
        &self,
        company_id: &str,
        checkpoint_id: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{CHECKPOINT_SELECT} WHERE company_id = ?1 AND checkpoint_id = ?2"),
                params![company_id, checkpoint_id],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 按游标找检查点：同一现场（游标相同）只有一个检查点，重捕获应落回它。
    pub fn find_handover_checkpoint_by_cursor(
        &self,
        company_id: &str,
        scope_key: &str,
        cursor: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "{CHECKPOINT_SELECT} WHERE company_id = ?1 AND scope_key = ?2 AND cursor = ?3"
                ),
                params![company_id, scope_key, cursor],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 最新的检查点（按登记时间倒序）：只用于回显「上次核对的是哪一份现场」。
    pub fn latest_handover_checkpoint(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "{CHECKPOINT_SELECT} WHERE company_id = ?1 AND scope_key = ?2
                     ORDER BY recorded_at_ms DESC, checkpoint_id DESC LIMIT 1"
                ),
                params![company_id, scope_key],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 追加一条接管确认：同 `confirmation_id` 已存在时不改写并返回 `false`。
    pub fn insert_takeover_confirmation(&self, row: &ConfirmationRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO takeover_confirmations (
                company_id, confirmation_id, scope_key, checkpoint_id, checkpoint_cursor,
                reviewed_cursor, appointment_id, authority_epoch, decision, confirmed_by, note,
                open_items_json, delta_json, confirmed_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.confirmation_id,
                row.scope_key,
                row.checkpoint_id,
                row.checkpoint_cursor,
                row.reviewed_cursor,
                row.appointment_id,
                row.authority_epoch as i64,
                row.decision,
                row.confirmed_by,
                row.note,
                row.open_items_json,
                row.delta_json,
                row.confirmed_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_takeover_confirmation(
        &self,
        company_id: &str,
        confirmation_id: &str,
    ) -> Result<Option<ConfirmationRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{CONFIRMATION_SELECT} WHERE company_id = ?1 AND confirmation_id = ?2"),
                params![company_id, confirmation_id],
                map_confirmation_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 某 scope 的确认历史（epoch 与登记时间升序）：最后一行为当前有效的确认。
    pub fn list_takeover_confirmations(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Vec<ConfirmationRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{CONFIRMATION_SELECT} WHERE company_id = ?1 AND scope_key = ?2
             ORDER BY authority_epoch ASC, recorded_at_ms ASC, confirmation_id ASC"
        ))?;
        let rows = stmt
            .query_map(params![company_id, scope_key], map_confirmation_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

impl<'a> UnitOfWork<'a> {
    /// 事务内读作用域头：接管确认必须与「当前 epoch」在同一次事务里判定。
    pub fn get_scope_head(
        &self,
        company_id: &str,
        scope_key: &str,
    ) -> Result<Option<ScopeHeadRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{SCOPE_HEAD_SELECT} WHERE company_id = ?1 AND scope_key = ?2"),
                params![company_id, scope_key],
                |row| {
                    Ok(ScopeHeadRow {
                        company_id: row.get(0)?,
                        scope_key: row.get(1)?,
                        current_appointment_id: row.get(2)?,
                        authority_epoch: row.get::<_, i64>(3)? as u64,
                        version: row.get::<_, i64>(4)? as u64,
                        updated_at: row.get(5)?,
                        recorded_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn get_appointment(
        &self,
        company_id: &str,
        appointment_id: &str,
    ) -> Result<Option<AppointmentRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{APPOINTMENT_SELECT} WHERE company_id = ?1 AND appointment_id = ?2"),
                params![company_id, appointment_id],
                map_appointment_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn get_handover_checkpoint(
        &self,
        company_id: &str,
        checkpoint_id: &str,
    ) -> Result<Option<CheckpointRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{CHECKPOINT_SELECT} WHERE company_id = ?1 AND checkpoint_id = ?2"),
                params![company_id, checkpoint_id],
                map_checkpoint_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn insert_takeover_confirmation(&self, row: &ConfirmationRow) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO takeover_confirmations (
                company_id, confirmation_id, scope_key, checkpoint_id, checkpoint_cursor,
                reviewed_cursor, appointment_id, authority_epoch, decision, confirmed_by, note,
                open_items_json, delta_json, confirmed_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT DO NOTHING",
            params![
                row.company_id,
                row.confirmation_id,
                row.scope_key,
                row.checkpoint_id,
                row.checkpoint_cursor,
                row.reviewed_cursor,
                row.appointment_id,
                row.authority_epoch as i64,
                row.decision,
                row.confirmed_by,
                row.note,
                row.open_items_json,
                row.delta_json,
                row.confirmed_at,
                row.recorded_at_ms,
            ],
        )?;
        Ok(inserted == 1)
    }
}
