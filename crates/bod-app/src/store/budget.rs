//! D3 预算上限 / 预留 / 账本。
//!
//! 预留与账本都以「逻辑使用键」去重：同一个键重复记账不累加，
//! 否则重试会把一次消耗记成两次。

use super::*;

impl SqliteStore {
    /// 列出某 Company 的预留行；供预算解释与测试断言。
    pub fn list_reservations(&self, company_id: &str) -> Result<Vec<Reservation>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT reservation_id, company_id, project_id, agent_id, run_id, work_package_id,
                    dimension, currency, reserved_value, confirmed_value, risk_value, state,
                    logical_key, created_at_ms, updated_at_ms
             FROM reservations WHERE company_id = ?1 ORDER BY created_at_ms, reservation_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let dimension = Dimension::parse(&row.get::<_, String>(6)?).unwrap_or(Dimension::Calls);
            let currency: String = row.get(7)?;
            Ok(Reservation {
                reservation_id: row.get(0)?,
                company_id: DomainId::new(row.get::<_, String>(1)?),
                project_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                agent_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                run_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                work_package_id: row.get::<_, Option<String>>(5)?.map(DomainId::new),
                measure: Measure {
                    dimension,
                    value: 0,
                    currency: if currency.is_empty() {
                        None
                    } else {
                        Some(currency)
                    },
                },
                reserved: row.get(8)?,
                confirmed: row.get(9)?,
                risk: row.get(10)?,
                state: ReservationState::parse(&row.get::<_, String>(11)?)
                    .unwrap_or(ReservationState::Cancelled),
                logical_key: row.get(12)?,
                created_at_ms: row.get(13)?,
                updated_at_ms: row.get(14)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出某 Company 的全部账本分录（只读；账本不可改写）。
    pub fn list_ledger_entries(&self, company_id: &str) -> Result<Vec<LedgerEntry>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT entry_id, company_id, project_id, agent_id, work_package_id, reservation_id,
                    usage_key, entry_kind, dimension, currency, amount, source, reason, created_at_ms
             FROM ledger_entries WHERE company_id = ?1 ORDER BY created_at_ms, entry_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let dimension = Dimension::parse(&row.get::<_, String>(8)?).unwrap_or(Dimension::Calls);
            let currency: String = row.get(9)?;
            Ok(LedgerEntry {
                entry_id: row.get(0)?,
                company_id: DomainId::new(row.get::<_, String>(1)?),
                project_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                agent_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                work_package_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                reservation_id: row.get(5)?,
                usage_key: row.get(6)?,
                kind: LedgerEntryKind::parse(&row.get::<_, String>(7)?)
                    .unwrap_or(LedgerEntryKind::Adjustment),
                measure: Measure {
                    dimension,
                    value: 0,
                    currency: if currency.is_empty() {
                        None
                    } else {
                        Some(currency)
                    },
                },
                amount: row.get(10)?,
                source: row.get(11)?,
                reason: row.get(12)?,
                created_at_ms: row.get(13)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }
}

impl<'a> UnitOfWork<'a> {
    pub fn get_budget_limit(
        &self,
        company_id: &str,
        scope_kind: BudgetScopeKind,
        scope_id: &str,
        dimension: Dimension,
        currency: &str,
    ) -> Result<Option<BudgetLimit>, StoreError> {
        self.conn
            .query_row(
                "SELECT limit_value, version FROM budget_limits
                 WHERE company_id = ?1 AND scope_kind = ?2 AND scope_id = ?3
                   AND dimension = ?4 AND currency = ?5",
                params![
                    company_id,
                    scope_kind.as_str(),
                    scope_id,
                    dimension.as_str(),
                    currency
                ],
                |row| {
                    Ok(BudgetLimit {
                        company_id: DomainId::new(company_id),
                        scope_kind,
                        scope_id: DomainId::new(scope_id),
                        measure: Measure {
                            dimension,
                            value: 0,
                            currency: if currency.is_empty() {
                                None
                            } else {
                                Some(currency.to_string())
                            },
                        },
                        limit: row.get(0)?,
                        version: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn put_budget_limit(&self, limit: &BudgetLimit) -> Result<(), StoreError> {
        let currency = limit.measure.currency.as_deref().unwrap_or("");
        self.conn.execute(
            "INSERT INTO budget_limits (
                company_id, scope_kind, scope_id, dimension, currency, limit_value, version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(company_id, scope_kind, scope_id, dimension, currency) DO UPDATE SET
                limit_value = excluded.limit_value, version = excluded.version",
            params![
                limit.company_id.as_str(),
                limit.scope_kind.as_str(),
                limit.scope_id.as_str(),
                limit.measure.dimension.as_str(),
                currency,
                limit.limit,
                limit.version as i64,
            ],
        )?;
        Ok(())
    }

    /// 聚合某层级某维度的 C/R/U。consumed 计 C、active 计 R、risk 计 U、cancelled 全零。
    pub fn usage_for_scope(
        &self,
        company_id: &str,
        scope_kind: BudgetScopeKind,
        scope_id: &str,
        dimension: Dimension,
        currency: &str,
    ) -> Result<BudgetUsage, StoreError> {
        let mut usage = BudgetUsage::default();
        let mut read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<()> {
            usage.confirmed = row.get(0)?;
            usage.reserved = row.get(1)?;
            usage.risk = row.get(2)?;
            Ok(())
        };
        const SELECT: &str = "SELECT COALESCE(SUM(confirmed_value), 0),
                                     COALESCE(SUM(reserved_value), 0),
                                     COALESCE(SUM(risk_value), 0)
                              FROM reservations WHERE company_id = ?1";
        match scope_kind {
            BudgetScopeKind::Company => self.conn.query_row(
                &format!("{SELECT} AND dimension = ?2 AND currency = ?3"),
                params![company_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::Project => self.conn.query_row(
                &format!("{SELECT} AND project_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::Agent => self.conn.query_row(
                &format!("{SELECT} AND agent_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::Run => self.conn.query_row(
                &format!("{SELECT} AND run_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
            BudgetScopeKind::WorkPackage => self.conn.query_row(
                &format!("{SELECT} AND work_package_id = ?2 AND dimension = ?3 AND currency = ?4"),
                params![company_id, scope_id, dimension.as_str(), currency],
                |row| read(row),
            )?,
        };
        Ok(usage)
    }

    pub fn insert_reservation(&self, reservation: &Reservation) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO reservations (
                reservation_id, company_id, project_id, agent_id, run_id, work_package_id,
                dimension, currency, reserved_value, confirmed_value, risk_value, state,
                logical_key, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                reservation.reservation_id,
                reservation.company_id.as_str(),
                reservation.project_id.as_ref().map(DomainId::as_str),
                reservation.agent_id.as_ref().map(DomainId::as_str),
                reservation.run_id.as_ref().map(DomainId::as_str),
                reservation.work_package_id.as_ref().map(DomainId::as_str),
                reservation.measure.dimension.as_str(),
                reservation.measure.currency.as_deref().unwrap_or(""),
                reservation.reserved,
                reservation.confirmed,
                reservation.risk,
                reservation.state.as_str(),
                reservation.logical_key,
                reservation.created_at_ms,
                reservation.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_reservation(&self, reservation_id: &str) -> Result<Option<Reservation>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, project_id, agent_id, run_id, work_package_id, dimension,
                        currency, reserved_value, confirmed_value, risk_value, state,
                        logical_key, created_at_ms, updated_at_ms
                 FROM reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |row| {
                    let dimension =
                        Dimension::parse(&row.get::<_, String>(5)?).unwrap_or(Dimension::Calls);
                    let currency: String = row.get(6)?;
                    Ok(Reservation {
                        reservation_id: reservation_id.to_string(),
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        project_id: row.get::<_, Option<String>>(1)?.map(DomainId::new),
                        agent_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                        run_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                        work_package_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                        measure: Measure {
                            dimension,
                            value: 0,
                            currency: if currency.is_empty() {
                                None
                            } else {
                                Some(currency)
                            },
                        },
                        reserved: row.get(7)?,
                        confirmed: row.get(8)?,
                        risk: row.get(9)?,
                        state: ReservationState::parse(&row.get::<_, String>(10)?)
                            .unwrap_or(ReservationState::Cancelled),
                        logical_key: row.get(11)?,
                        created_at_ms: row.get(12)?,
                        updated_at_ms: row.get(13)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 该项目是否已有超额调整分录：有则拒绝新的派发，直到有来源的调整解除。
    pub fn has_project_overage(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<bool, StoreError> {
        let present: i64 = self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM ledger_entries
                WHERE company_id = ?1 AND project_id = ?2 AND entry_kind = 'adjustment'
             )",
            params![company_id, project_id],
            |row| row.get(0),
        )?;
        Ok(present != 0)
    }

    /// 按稳定逻辑键查预留：同一派发意图重试应复用原预留，不重复占用。
    pub fn get_reservation_by_logical_key(
        &self,
        logical_key: &str,
    ) -> Result<Option<Reservation>, StoreError> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT reservation_id FROM reservations WHERE logical_key = ?1",
                params![logical_key],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.get_reservation(&id),
            None => Ok(None),
        }
    }

    /// 更新预留状态与 C/R/U。DB 的互斥 CHECK 会拒绝 R/U 同时非零的非法写法。
    pub fn update_reservation(&self, reservation: &Reservation) -> Result<bool, StoreError> {
        let rows = self.conn.execute(
            "UPDATE reservations SET
                reserved_value = ?2, confirmed_value = ?3, risk_value = ?4,
                state = ?5, updated_at_ms = ?6
             WHERE reservation_id = ?1",
            params![
                reservation.reservation_id,
                reservation.reserved,
                reservation.confirmed,
                reservation.risk,
                reservation.state.as_str(),
                reservation.updated_at_ms,
            ],
        )?;
        Ok(rows == 1)
    }

    /// 追加不可变分录。usage_key 冲突映射为稳定错误，调用方据此去重而非重复扣费。
    pub fn insert_ledger_entry(&self, entry: &LedgerEntry) -> Result<(), StoreError> {
        let result = self.conn.execute(
            "INSERT INTO ledger_entries (
                entry_id, company_id, project_id, agent_id, work_package_id, reservation_id,
                usage_key, entry_kind, dimension, currency, amount, source, reason, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                entry.entry_id,
                entry.company_id.as_str(),
                entry.project_id.as_ref().map(DomainId::as_str),
                entry.agent_id.as_ref().map(DomainId::as_str),
                entry.work_package_id.as_ref().map(DomainId::as_str),
                entry.reservation_id,
                entry.usage_key,
                entry.kind.as_str(),
                entry.measure.dimension.as_str(),
                entry.measure.currency.as_deref().unwrap_or(""),
                entry.amount,
                entry.source,
                entry.reason,
                entry.created_at_ms,
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(err) => {
                if let Some(usage_key) = &entry.usage_key
                    && err
                        .to_string()
                        .contains("UNIQUE constraint failed: ledger_entries.usage_key")
                {
                    return Err(StoreError::DuplicateUsageKey {
                        usage_key: usage_key.clone(),
                    });
                }
                Err(StoreError::Sql(err))
            }
        }
    }

    pub fn ledger_entry_for_usage_key(
        &self,
        usage_key: &str,
    ) -> Result<Option<LedgerEntry>, StoreError> {
        self.conn
            .query_row(
                "SELECT entry_id, company_id, project_id, agent_id, work_package_id,
                        reservation_id, entry_kind, dimension, currency, amount, source,
                        reason, created_at_ms
                 FROM ledger_entries WHERE usage_key = ?1",
                params![usage_key],
                |row| {
                    let dimension =
                        Dimension::parse(&row.get::<_, String>(7)?).unwrap_or(Dimension::Calls);
                    let currency: String = row.get(8)?;
                    Ok(LedgerEntry {
                        entry_id: row.get(0)?,
                        company_id: DomainId::new(row.get::<_, String>(1)?),
                        project_id: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                        agent_id: row.get::<_, Option<String>>(3)?.map(DomainId::new),
                        work_package_id: row.get::<_, Option<String>>(4)?.map(DomainId::new),
                        reservation_id: row.get(5)?,
                        usage_key: Some(usage_key.to_string()),
                        kind: LedgerEntryKind::parse(&row.get::<_, String>(6)?)
                            .unwrap_or(LedgerEntryKind::Adjustment),
                        measure: Measure {
                            dimension,
                            value: 0,
                            currency: if currency.is_empty() {
                                None
                            } else {
                                Some(currency)
                            },
                        },
                        amount: row.get(9)?,
                        source: row.get(10)?,
                        reason: row.get(11)?,
                        created_at_ms: row.get(12)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }
}
