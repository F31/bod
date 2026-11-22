//! D2 工作队列 / 唤醒回执 / outbox 投递的持久化。
//!
//! 条件更新（`claim_*` / `complete_*` / `expire_*`）一律用 CAS：
//! 租约与状态的判定与写入必须在同一条 SQL 里，先查后写会让两个执行者都认为自己抢到。

use super::*;

impl SqliteStore {
    /// 列出某 Company 的队列行；供重启扫描未终结资源与 doctor 指标。
    pub fn list_queue(&self, company_id: &str) -> Result<Vec<WorkQueueEntry>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT company_id, queue_key, state, wanted_generation, claimed_generation,
                    completed_generation, next_due_at_ms, attempts, lease_owner, lease_token,
                    lease_expires_at_ms, last_error, blocked_reason
             FROM work_queue WHERE company_id = ?1",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok(WorkQueueEntry {
                company_id: DomainId::new(row.get::<_, String>(0)?),
                queue_key: row.get(1)?,
                state: QueueState::parse(&row.get::<_, String>(2)?).unwrap_or(QueueState::Idle),
                wanted_generation: row.get::<_, i64>(3)? as u64,
                claimed_generation: row.get::<_, i64>(4)? as u64,
                completed_generation: row.get::<_, i64>(5)? as u64,
                next_due_at_ms: row.get(6)?,
                attempts: row.get::<_, i64>(7)? as u32,
                lease_owner: row.get(8)?,
                lease_token: row.get::<_, i64>(9)? as u64,
                lease_expires_at_ms: row.get(10)?,
                last_error: row.get(11)?,
                blocked_reason: row
                    .get::<_, Option<String>>(12)?
                    .and_then(parse_denial_reason),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 统计某 Company 的 Attempt 行数；供取消对账断言"不新建 Attempt"。
    pub fn count_attempts(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM attempts WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 列出指定状态的 outbox 意图；供提交后派发与重启恢复扫描。
    pub fn list_outbox_by_status(
        &self,
        status: EffectStatus,
    ) -> Result<Vec<OutboxDelivery>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT event_or_command_id, destination_id, company_id, queue_key, request_digest,
                    status, attempts, next_due_at_ms, result_ref, created_at_ms
             FROM outbox_deliveries WHERE status = ?1",
        )?;
        let rows = stmt.query_map(params![status.as_str()], |row| {
            Ok(OutboxDelivery {
                event_or_command_id: row.get(0)?,
                destination_id: row.get(1)?,
                company_id: DomainId::new(row.get::<_, String>(2)?),
                queue_key: row.get(3)?,
                request_digest: row.get(4)?,
                status: EffectStatus::parse(&row.get::<_, String>(5)?)
                    .unwrap_or(EffectStatus::Unknown),
                attempts: row.get::<_, i64>(6)? as u32,
                next_due_at_ms: row.get(7)?,
                result_ref: row.get(8)?,
                created_at_ms: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }
}

impl<'a> UnitOfWork<'a> {
    /// 读取队列行；不存在返回 None。
    pub fn get_queue(
        &self,
        company_id: &str,
        queue_key: &str,
    ) -> Result<Option<WorkQueueEntry>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, queue_key, state, wanted_generation, claimed_generation,
                        completed_generation, next_due_at_ms, attempts, lease_owner, lease_token,
                        lease_expires_at_ms, last_error, blocked_reason
                 FROM work_queue WHERE company_id = ?1 AND queue_key = ?2",
                params![company_id, queue_key],
                |row| {
                    Ok(WorkQueueEntry {
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        queue_key: row.get(1)?,
                        state: QueueState::parse(&row.get::<_, String>(2)?)
                            .unwrap_or(QueueState::Idle),
                        wanted_generation: row.get::<_, i64>(3)? as u64,
                        claimed_generation: row.get::<_, i64>(4)? as u64,
                        completed_generation: row.get::<_, i64>(5)? as u64,
                        next_due_at_ms: row.get(6)?,
                        attempts: row.get::<_, i64>(7)? as u32,
                        lease_owner: row.get(8)?,
                        lease_token: row.get::<_, i64>(9)? as u64,
                        lease_expires_at_ms: row.get(10)?,
                        last_error: row.get(11)?,
                        blocked_reason: row
                            .get::<_, Option<String>>(12)?
                            .and_then(parse_denial_reason),
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 插入或更新队列行（受理唤醒、认领、完成、退避统一走此写入口）。
    pub fn put_queue(&self, entry: &WorkQueueEntry) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO work_queue (
                company_id, queue_key, state, wanted_generation, claimed_generation,
                completed_generation, next_due_at_ms, attempts, lease_owner, lease_token,
                lease_expires_at_ms, last_error, blocked_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(company_id, queue_key) DO UPDATE SET
                state = excluded.state,
                wanted_generation = excluded.wanted_generation,
                claimed_generation = excluded.claimed_generation,
                completed_generation = excluded.completed_generation,
                next_due_at_ms = excluded.next_due_at_ms,
                attempts = excluded.attempts,
                lease_owner = excluded.lease_owner,
                lease_token = excluded.lease_token,
                lease_expires_at_ms = excluded.lease_expires_at_ms,
                last_error = excluded.last_error,
                blocked_reason = excluded.blocked_reason",
            params![
                entry.company_id.as_str(),
                entry.queue_key,
                entry.state.as_str(),
                entry.wanted_generation as i64,
                entry.claimed_generation as i64,
                entry.completed_generation as i64,
                entry.next_due_at_ms,
                entry.attempts as i64,
                entry.lease_owner,
                entry.lease_token as i64,
                entry.lease_expires_at_ms,
                entry.last_error,
                entry.blocked_reason.map(denial_reason_str),
            ],
        )?;
        Ok(())
    }

    /// 读取唤醒回执。
    pub fn get_wake_receipt(
        &self,
        company_id: &str,
        producer_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<WakeReceipt>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, producer_id, idempotency_key, request_digest, queue_key,
                        generation, accepted_at_ms, retain_until_ms, schedule_id, occurrence_id
                 FROM wake_receipts
                WHERE company_id = ?1 AND producer_id = ?2 AND idempotency_key = ?3",
                params![company_id, producer_id, idempotency_key],
                |row| {
                    Ok(WakeReceipt {
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        producer_id: DomainId::new(row.get::<_, String>(1)?),
                        idempotency_key: row.get(2)?,
                        request_digest: row.get(3)?,
                        queue_key: row.get(4)?,
                        generation: row.get::<_, i64>(5)? as u64,
                        accepted_at_ms: row.get(6)?,
                        retain_until_ms: row.get(7)?,
                        schedule_id: row.get(8)?,
                        occurrence_id: row.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn put_wake_receipt(&self, receipt: &WakeReceipt) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO wake_receipts (
                company_id, producer_id, idempotency_key, request_digest, queue_key,
                generation, accepted_at_ms, retain_until_ms, schedule_id, occurrence_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                receipt.company_id.as_str(),
                receipt.producer_id.as_str(),
                receipt.idempotency_key,
                receipt.request_digest,
                receipt.queue_key,
                receipt.generation as i64,
                receipt.accepted_at_ms,
                receipt.retain_until_ms,
                receipt.schedule_id,
                receipt.occurrence_id,
            ],
        )?;
        Ok(())
    }

    /// 插入持久 outbox 意图；同 (id, destination) 重复保持原记录（稳定逻辑键不重建）。
    pub fn put_outbox(&self, delivery: &OutboxDelivery) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO outbox_deliveries (
                event_or_command_id, destination_id, company_id, queue_key, request_digest,
                status, attempts, next_due_at_ms, result_ref, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                delivery.event_or_command_id,
                delivery.destination_id,
                delivery.company_id.as_str(),
                delivery.queue_key,
                delivery.request_digest,
                delivery.status.as_str(),
                delivery.attempts as i64,
                delivery.next_due_at_ms,
                delivery.result_ref,
                delivery.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_outbox(
        &self,
        event_or_command_id: &str,
        destination_id: &str,
    ) -> Result<Option<OutboxDelivery>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, queue_key, request_digest, status, attempts,
                        next_due_at_ms, result_ref, created_at_ms
                 FROM outbox_deliveries
                WHERE event_or_command_id = ?1 AND destination_id = ?2",
                params![event_or_command_id, destination_id],
                |row| {
                    Ok(OutboxDelivery {
                        event_or_command_id: event_or_command_id.to_string(),
                        destination_id: destination_id.to_string(),
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        queue_key: row.get(1)?,
                        request_digest: row.get(2)?,
                        status: EffectStatus::parse(&row.get::<_, String>(3)?)
                            .unwrap_or(EffectStatus::Unknown),
                        attempts: row.get::<_, i64>(4)? as u32,
                        next_due_at_ms: row.get(5)?,
                        result_ref: row.get(6)?,
                        created_at_ms: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 更新 outbox 状态；用于提交后派发的结果登记与恢复裁决。
    pub fn set_outbox_status(
        &self,
        event_or_command_id: &str,
        destination_id: &str,
        status: EffectStatus,
        attempts: u32,
        next_due_at_ms: i64,
        result_ref: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE outbox_deliveries SET status = ?3, attempts = ?4,
                    next_due_at_ms = ?5, result_ref = ?6
             WHERE event_or_command_id = ?1 AND destination_id = ?2",
            params![
                event_or_command_id,
                destination_id,
                status.as_str(),
                attempts as i64,
                next_due_at_ms,
                result_ref,
            ],
        )?;
        Ok(())
    }

    /// 原子认领：只有 state=pending、租约 token 匹配且已到期时更新成功。
    /// BEGIN IMMEDIATE 串行化下，先成功者递增 lease_token，竞争者的 WHERE 不再匹配。
    /// 返回 true 表示本调用持有租约；失败方必须读取当前状态后退出，不得启动执行。
    pub fn claim_queue_if_ready(
        &self,
        company_id: &str,
        queue_key: &str,
        expected_token: u64,
        owner: &str,
        now_ms: i64,
        lease_expires_at_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE work_queue SET
                state = 'running', lease_owner = ?4, lease_token = lease_token + 1,
                lease_expires_at_ms = ?5, claimed_generation = wanted_generation,
                attempts = attempts + 1, next_due_at_ms = ?6, last_error = NULL
             WHERE company_id = ?1 AND queue_key = ?2 AND lease_token = ?3
               AND state = 'pending' AND next_due_at_ms <= ?6",
            params![
                company_id,
                queue_key,
                expected_token as i64,
                owner,
                lease_expires_at_ms,
                now_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    /// 完成确认 CAS：owner 与 token 必须同时匹配，旧 token 不得更新宿主受管状态（fencing）。
    /// 成功时按决策写入 completed_generation 与下一状态；返回是否命中。
    #[allow(clippy::too_many_arguments)]
    pub fn complete_queue_if_leased(
        &self,
        company_id: &str,
        queue_key: &str,
        owner: &str,
        token: u64,
        completed_generation: u64,
        next_state: QueueState,
        next_due_at_ms: i64,
        last_error: Option<&str>,
        blocked_reason: Option<DenialReason>,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE work_queue SET
                completed_generation = ?5, state = ?6, next_due_at_ms = ?7,
                last_error = ?8, blocked_reason = ?9
             WHERE company_id = ?1 AND queue_key = ?2 AND lease_owner = ?3
               AND lease_token = ?4 AND state = 'running'",
            params![
                company_id,
                queue_key,
                owner,
                token as i64,
                completed_generation as i64,
                next_state.as_str(),
                next_due_at_ms,
                last_error,
                blocked_reason.map(denial_reason_str),
            ],
        )?;
        Ok(rows == 1)
    }

    /// 租约过期恢复处理权：到期且仍 running 时转 pending，保留 lease_token 供 fencing。
    pub fn expire_lease_if_due(
        &self,
        company_id: &str,
        queue_key: &str,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE work_queue SET state = 'pending', next_due_at_ms = ?3
             WHERE company_id = ?1 AND queue_key = ?2 AND state = 'running'
               AND lease_expires_at_ms IS NOT NULL AND lease_expires_at_ms <= ?3",
            params![company_id, queue_key, now_ms],
        )?;
        Ok(rows == 1)
    }
}
