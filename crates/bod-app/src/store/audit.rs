//! 业务审计事件（CR-002）的读写。
//!
//! 审计表 append-only：库层 trigger 拒绝 UPDATE/DELETE，读侧出现表外动作或
//! 表外决策一律报错，不静默回退——那说明有人绕过应用写库。

use super::*;

/// 审计表的读取列顺序。集中一处，避免三处查询各写一份列清单后互相错位。
const AUDIT_SELECT: &str = "SELECT sequence, company_id, correlation_id, actor_id, action,
            scope_company, scope_project, scope_issue, basis, decision, reason,
            decided_at, recorded_at_ms
     FROM audit_events";

/// 审计行映射。表外动作或表外决策一律报错，不静默回退：
/// 库里出现应用不认识的取值，说明有人绕过应用写库，读侧必须炸出来。
fn map_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
    let action_text: String = row.get(4)?;
    let action = AuditAction::parse(&action_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("audit_events.action 出现表外值：{action_text}"),
            )),
        )
    })?;
    let decision_text: String = row.get(9)?;
    let decision = AuditDecision::parse(&decision_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            9,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("audit_events.decision 出现表外值：{decision_text}"),
            )),
        )
    })?;
    Ok(AuditRecord {
        sequence: row.get(0)?,
        event: AuditEvent {
            company_id: row.get(1)?,
            correlation_id: row.get(2)?,
            actor: DomainId::new(row.get::<_, String>(3)?),
            action,
            scope: Scope {
                company_id: DomainId::new(row.get::<_, String>(5)?),
                project_id: row.get::<_, Option<String>>(6)?.map(DomainId::new),
                issue_id: row.get::<_, Option<String>>(7)?.map(DomainId::new),
            },
            basis: row.get(8)?,
            decision,
            reason: row.get(10)?,
            decided_at: row.get(11)?,
        },
        recorded_at_ms: row.get(12)?,
    })
}

impl SqliteStore {
    /// 按关联 ID 读一条审计链（CR-002）。顺序由自增序号决定：
    /// 同一毫秒内可能写入多条，按时间戳排会得到不稳定顺序。
    pub fn list_audit_events(
        &self,
        company_id: &str,
        correlation_id: &str,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{AUDIT_SELECT} WHERE company_id = ?1 AND correlation_id = ?2 ORDER BY sequence ASC"
        ))?;
        let rows = stmt.query_map(params![company_id, correlation_id], map_audit_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 按决策计数：§9 的拒绝率与审计分布都从这里出。
    pub fn count_audit_events(
        &self,
        company_id: &str,
        decision: AuditDecision,
    ) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE company_id = ?1 AND decision = ?2",
                params![company_id, decision.as_str()],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 最近的审计行（序号降序）。`limit` 由调用方给，不设默认值——
    /// 一个「顺手全表读」的默认值会在审计表变大后变成事故。
    pub fn latest_audit_events(
        &self,
        company_id: &str,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{AUDIT_SELECT} WHERE company_id = ?1 ORDER BY sequence DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![company_id, limit as i64], map_audit_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }
}

impl<'a> UnitOfWork<'a> {
    /// 追加一条审计事件（CR-002）。
    ///
    /// 只有 INSERT：审计不可改写，所以没有 upsert。重复写同一条事件不是幂等问题，
    /// 而是调用方逻辑问题——去重会把「同一动作被执行了两次」这件事抹掉。
    ///
    /// 必须先过 `AuditEvent::validate()`。库层的 CHECK 只兜「决策与原因不一致」，
    /// 拦不住「理由文本里带令牌」——两道闸门分工不同，别以为有库约束就够了。
    pub fn append_audit_event(
        &self,
        event: &crate::audit_log::AuditEvent,
        recorded_at_ms: i64,
    ) -> Result<i64, StoreError> {
        event.validate().map_err(StoreError::Audit)?;
        self.conn.execute(
            "INSERT INTO audit_events (
                company_id, correlation_id, actor_id, action,
                scope_company, scope_project, scope_issue,
                basis, decision, reason, decided_at, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                event.company_id,
                event.correlation_id,
                event.actor.as_str(),
                event.action.as_str(),
                event.scope.company_id.as_str(),
                event.scope.project_id.as_ref().map(DomainId::as_str),
                event.scope.issue_id.as_ref().map(DomainId::as_str),
                event.basis,
                event.decision.as_str(),
                event.reason,
                event.decided_at,
                recorded_at_ms,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
}
