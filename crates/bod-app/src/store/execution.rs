//! D4 执行协议：调用记录、观察 inbox、能力声明与探测报告、支持矩阵。
//!
//! 调用按 `(company_id, execution_id)` 去重；观察 inbox 用游标推进，
//! 处理过的观察不删（删了就无法回答「这条观察当时是什么」）。

use super::*;

impl SqliteStore {
    /// 列出某 Company 的 invocation；供重启扫描未终结执行与对账。
    pub fn list_invocations(&self, company_id: &str) -> Result<Vec<InvocationRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT invocation_id, company_id, run_id, work_package_revision, attempt_id,
                    binding_id, fencing_token, adapter_id, adapter_version, params_canonical,
                    params_digest, execution_id, state, created_at_ms, accepted_at_ms,
                    last_observed_at_ms, retention_until_ms
             FROM invocations WHERE company_id = ?1 ORDER BY created_at_ms",
        )?;
        let rows = statement.query_map(params![company_id], |row| {
            let state_text: String = row.get(12)?;
            let fencing: i64 = row.get(6)?;
            Ok(InvocationRecord {
                invocation_id: row.get(0)?,
                company_id: row.get(1)?,
                run_id: row.get(2)?,
                work_package_revision: row.get(3)?,
                attempt_id: row.get(4)?,
                binding_id: row.get(5)?,
                fencing_token: fencing.max(0) as u64,
                adapter_id: row.get(7)?,
                adapter_version: row.get(8)?,
                params_canonical: row.get(9)?,
                params_digest: row.get(10)?,
                execution_id: row.get(11)?,
                state: ExecutionState::parse(&state_text).unwrap_or(ExecutionState::Unknown),
                created_at_ms: row.get(13)?,
                accepted_at_ms: row.get(14)?,
                last_observed_at_ms: row.get(15)?,
                retention_until_ms: row.get(16)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 按外部执行 ID 反查：对账时用它避免重复创建执行。
    pub fn find_invocation_by_execution_id(
        &self,
        execution_id: &str,
    ) -> Result<Option<InvocationRecord>, StoreError> {
        let invocation_id: Option<String> = self
            .conn
            .query_row(
                "SELECT invocation_id FROM invocations WHERE execution_id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?;
        match invocation_id {
            Some(id) => self.read_invocation(&id),
            None => Ok(None),
        }
    }

    fn read_invocation(&self, invocation_id: &str) -> Result<Option<InvocationRecord>, StoreError> {
        // 只读路径：借用同一查询逻辑，避免复制列映射。
        let mut statement = self.conn.prepare(
            "SELECT invocation_id, company_id, run_id, work_package_revision, attempt_id,
                    binding_id, fencing_token, adapter_id, adapter_version, params_canonical,
                    params_digest, execution_id, state, created_at_ms, accepted_at_ms,
                    last_observed_at_ms, retention_until_ms
             FROM invocations WHERE invocation_id = ?1",
        )?;
        let mut rows = statement.query(params![invocation_id])?;
        match rows.next()? {
            Some(row) => {
                let state_text: String = row.get(12)?;
                let fencing: i64 = row.get(6)?;
                Ok(Some(InvocationRecord {
                    invocation_id: row.get(0)?,
                    company_id: row.get(1)?,
                    run_id: row.get(2)?,
                    work_package_revision: row.get(3)?,
                    attempt_id: row.get(4)?,
                    binding_id: row.get(5)?,
                    fencing_token: fencing.max(0) as u64,
                    adapter_id: row.get(7)?,
                    adapter_version: row.get(8)?,
                    params_canonical: row.get(9)?,
                    params_digest: row.get(10)?,
                    execution_id: row.get(11)?,
                    state: ExecutionState::parse(&state_text).unwrap_or(ExecutionState::Unknown),
                    created_at_ms: row.get(13)?,
                    accepted_at_ms: row.get(14)?,
                    last_observed_at_ms: row.get(15)?,
                    retention_until_ms: row.get(16)?,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn count_observations(&self, invocation_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE invocation_id = ?1",
                params![invocation_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    pub fn count_start_audits(&self, invocation_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM start_audits WHERE invocation_id = ?1",
                params![invocation_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 支持矩阵读取：只返回落库的声明，不补默认值，Fake Executor 不在其中。
    pub fn list_support_matrix(
        &self,
        adapter_id: &str,
        adapter_version: &str,
    ) -> Result<Vec<MatrixRow>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT adapter_id, adapter_version, capability, level, evidence_level, source,
                    reason, observed_at_ms
             FROM adapter_capabilities
             WHERE adapter_id = ?1 AND adapter_version = ?2
             ORDER BY capability",
        )?;
        let rows = statement.query_map(params![adapter_id, adapter_version], |row| {
            let level: String = row.get(3)?;
            let evidence: String = row.get(4)?;
            let source: String = row.get(5)?;
            Ok(MatrixRow {
                adapter_id: row.get(0)?,
                adapter_version: row.get(1)?,
                capability: row.get(2)?,
                level: SupportLevel::parse(&level),
                mode: None,
                evidence_level: EvidenceLevel::parse(&evidence)
                    .unwrap_or(EvidenceLevel::SelfDeclared),
                source: EvidenceSource::parse(&source)
                    .unwrap_or(EvidenceSource::AdapterSelfDeclared),
                observed_at_ms: row.get(7)?,
                limitations: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }
}

impl<'a> UnitOfWork<'a> {
    /// 读 invocation 接收记录。存在即说明宿主已承诺过这次派发，不再新建。
    pub fn get_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT invocation_id, company_id, run_id, work_package_revision, attempt_id,
                        binding_id, fencing_token, adapter_id, adapter_version, params_canonical,
                        params_digest, execution_id, state, created_at_ms, accepted_at_ms,
                        last_observed_at_ms, retention_until_ms
                 FROM invocations WHERE invocation_id = ?1",
                params![invocation_id],
                |row| {
                    let state_text: String = row.get(12)?;
                    let fencing: i64 = row.get(6)?;
                    Ok(InvocationRecord {
                        invocation_id: row.get(0)?,
                        company_id: row.get(1)?,
                        run_id: row.get(2)?,
                        work_package_revision: row.get(3)?,
                        attempt_id: row.get(4)?,
                        binding_id: row.get(5)?,
                        fencing_token: fencing.max(0) as u64,
                        adapter_id: row.get(7)?,
                        adapter_version: row.get(8)?,
                        params_canonical: row.get(9)?,
                        params_digest: row.get(10)?,
                        execution_id: row.get(11)?,
                        state: ExecutionState::parse(&state_text)
                            .unwrap_or(ExecutionState::Unknown),
                        created_at_ms: row.get(13)?,
                        accepted_at_ms: row.get(14)?,
                        last_observed_at_ms: row.get(15)?,
                        retention_until_ms: row.get(16)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 先持久化接收记录，再允许对外发送 start（§15.6）。
    pub fn insert_invocation(&self, record: &InvocationRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO invocations (
                invocation_id, company_id, run_id, work_package_revision, attempt_id, binding_id,
                fencing_token, adapter_id, adapter_version, params_canonical, params_digest,
                execution_id, state, created_at_ms, accepted_at_ms, last_observed_at_ms,
                retention_until_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                record.invocation_id,
                record.company_id,
                record.run_id,
                record.work_package_revision,
                record.attempt_id,
                record.binding_id,
                record.fencing_token as i64,
                record.adapter_id,
                record.adapter_version,
                record.params_canonical,
                record.params_digest,
                record.execution_id,
                record.state.as_str(),
                record.created_at_ms,
                record.accepted_at_ms,
                record.last_observed_at_ms,
                record.retention_until_ms,
            ],
        )?;
        Ok(())
    }

    /// 确认接收：只写一次 execution_id，重复确认不覆盖原值（同 ID 返回原执行）。
    pub fn confirm_invocation(
        &self,
        invocation_id: &str,
        execution_id: &str,
        state: ExecutionState,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE invocations
             SET execution_id = ?2, state = ?3, accepted_at_ms = COALESCE(accepted_at_ms, ?4),
                 last_observed_at_ms = ?4
             WHERE invocation_id = ?1 AND execution_id IS NULL",
            params![invocation_id, execution_id, state.as_str(), now_ms],
        )?;
        Ok(rows == 1)
    }

    pub fn set_invocation_state(
        &self,
        invocation_id: &str,
        state: ExecutionState,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE invocations SET state = ?2, last_observed_at_ms = ?3 WHERE invocation_id = ?1",
            params![invocation_id, state.as_str(), now_ms],
        )?;
        Ok(rows == 1)
    }

    /// 记录一次 start 尝试。回包丢失只留审计，不新建 Attempt（I-04）。
    pub fn record_start_audit(
        &self,
        invocation_id: &str,
        outcome: &str,
        detail: Option<&str>,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO start_audits (invocation_id, outcome, detail, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![invocation_id, outcome, detail, now_ms],
        )?;
        Ok(())
    }

    /// 观察先持久接收；返回 true 表示首次接收（重复序号不再产生业务效果）。
    pub fn insert_observation(
        &self,
        envelope: &ObservationEnvelope,
        truncated: bool,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "INSERT INTO observations (
                invocation_id, source_epoch, stream_id, source_sequence, observed_at_ms, kind,
                payload, origin, truncated, persisted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT DO NOTHING",
            params![
                envelope.invocation_id,
                envelope.source_epoch as i64,
                envelope.stream_id,
                envelope.source_sequence as i64,
                envelope.observed_at_ms,
                envelope.kind.as_str(),
                envelope.payload,
                envelope.origin,
                i64::from(truncated),
                now_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    pub fn get_observation_cursor(
        &self,
        invocation_id: &str,
        source_epoch: u64,
        stream_id: &str,
    ) -> Result<Option<ObservationCursor>, StoreError> {
        self.conn
            .query_row(
                "SELECT highest_contiguous, retained_floor, buffered
                 FROM observation_cursors
                 WHERE invocation_id = ?1 AND source_epoch = ?2 AND stream_id = ?3",
                params![invocation_id, source_epoch as i64, stream_id],
                |row| {
                    let highest: Option<i64> = row.get(0)?;
                    let floor: Option<i64> = row.get(1)?;
                    let buffered: String = row.get(2)?;
                    let buffered: Vec<u64> = buffered
                        .split(',')
                        .filter(|part| !part.is_empty())
                        .filter_map(|part| part.parse::<u64>().ok())
                        .collect();
                    Ok(ObservationCursor::restore(
                        highest.map(|value| value.max(0) as u64),
                        floor.map(|value| value.max(0) as u64),
                        &buffered,
                    ))
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn put_observation_cursor(
        &self,
        invocation_id: &str,
        source_epoch: u64,
        stream_id: &str,
        cursor: &ObservationCursor,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let buffered = cursor
            .buffered_sequences()
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        self.conn.execute(
            "INSERT INTO observation_cursors (
                invocation_id, source_epoch, stream_id, highest_contiguous, retained_floor,
                buffered, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(invocation_id, source_epoch, stream_id) DO UPDATE SET
                highest_contiguous = excluded.highest_contiguous,
                retained_floor = excluded.retained_floor,
                buffered = excluded.buffered,
                updated_at_ms = excluded.updated_at_ms",
            params![
                invocation_id,
                source_epoch as i64,
                stream_id,
                cursor.ack_upto().map(|value| value as i64),
                cursor.retained_floor().map(|value| value as i64),
                buffered,
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// 未处理的观察：重启后继续业务处理，ACK 已确认但处理未完成的不能当已完成。
    pub fn list_pending_observations(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<ObservationEnvelope>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT invocation_id, source_epoch, stream_id, source_sequence, observed_at_ms,
                    kind, payload, origin
             FROM observations
             WHERE invocation_id = ?1 AND processed = 0
             ORDER BY source_epoch, stream_id, source_sequence",
        )?;
        let rows = statement.query_map(params![invocation_id], |row| {
            let kind_text: String = row.get(5)?;
            Ok(ObservationEnvelope {
                invocation_id: row.get(0)?,
                source_epoch: row.get::<_, i64>(1)?.max(0) as u64,
                stream_id: row.get(2)?,
                source_sequence: row.get::<_, i64>(3)?.max(0) as u64,
                observed_at_ms: row.get(4)?,
                kind: ObservationType::parse(&kind_text).unwrap_or(ObservationType::Log),
                payload: row.get(6)?,
                origin: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    pub fn mark_observation_processed(
        &self,
        invocation_id: &str,
        source_epoch: u64,
        stream_id: &str,
        source_sequence: u64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE observations SET processed = 1
             WHERE invocation_id = ?1 AND source_epoch = ?2 AND stream_id = ?3
               AND source_sequence = ?4 AND processed = 0",
            params![
                invocation_id,
                source_epoch as i64,
                stream_id,
                source_sequence as i64
            ],
        )?;
        Ok(rows == 1)
    }

    /// 记录适配器自报的能力声明：未知能力也落库（level=unknown），不静默丢弃。
    pub fn put_capability_claims(&self, descriptor: &AdapterDescriptor) -> Result<(), StoreError> {
        for claim in &descriptor.capabilities {
            self.conn.execute(
                "INSERT INTO adapter_capabilities (
                    adapter_id, adapter_version, capability, level, evidence_level, source,
                    reason, observed_at_ms, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(adapter_id, adapter_version, capability) DO UPDATE SET
                    level = excluded.level,
                    evidence_level = excluded.evidence_level,
                    source = excluded.source,
                    reason = excluded.reason,
                    observed_at_ms = excluded.observed_at_ms,
                    expires_at_ms = excluded.expires_at_ms",
                params![
                    descriptor.adapter_id,
                    descriptor.adapter_version,
                    claim.slot.as_str(),
                    claim.level.as_str(),
                    claim.evidence_level.as_str(),
                    claim.source.as_str(),
                    claim.reason,
                    claim.observed_at_ms,
                    claim.expires_at_ms,
                ],
            )?;
        }
        Ok(())
    }

    /// 分项探测结果落库：保留 scope 与 TTL，过期由读取方降级为 unknown。
    pub fn put_probe_report(&self, report: &ProbeReport) -> Result<(), StoreError> {
        for item in &report.items {
            self.conn.execute(
                "INSERT INTO probe_results (
                    adapter_id, adapter_version, aspect, status, reason, source, scope,
                    observed_at_ms, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(adapter_id, adapter_version, aspect) DO UPDATE SET
                    status = excluded.status,
                    reason = excluded.reason,
                    source = excluded.source,
                    scope = excluded.scope,
                    observed_at_ms = excluded.observed_at_ms,
                    expires_at_ms = excluded.expires_at_ms",
                params![
                    report.adapter_id,
                    report.adapter_version,
                    item.aspect.as_str(),
                    item.status.as_str(),
                    item.reason,
                    item.source.as_str(),
                    item.scope,
                    item.observed_at_ms,
                    item.expires_at_ms,
                ],
            )?;
        }
        Ok(())
    }
}
