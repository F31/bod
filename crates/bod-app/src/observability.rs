//! §9 运行观测的读模型（CR-002 第 3 步）。
//!
//! 只读：所有数据来自既有表，不改写入路径，也不新增后台采集。
//! 这一层存在的理由是「口径」——同一组数字如果每个调用方各写一次 SQL，
//! 「队列积压」在报表里和在告警里迟早会变成两个数。
//!
//! **算不出来的项不填 0。** `not_measured` 显式列出并写原因：
//! 报表里留白会被读成「没有缺口」，而实际是「没有这一列数据」。

use crate::audit_log::AuditDecision;
use crate::store::{SqliteStore, StoreError};
use rusqlite::params;

/// 一次观测快照。字段与 §9 的六项要求对应，缺失项见 `not_measured`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationSnapshot {
    pub company_id: String,
    pub captured_at_ms: i64,
    /// 队列按状态分组。`idle` 也在内：没有 `idle` 行说明队列表本身是空的，
    /// 与「全是 idle」是两件事，报表要分得清。
    pub queue: Vec<QueueStateCount>,
    /// 阻塞原因分布。`blocked_reason` 为 NULL 的行归入 `(未记录)` 并照常显示——
    /// 隐藏它们等于把「阻塞但没说原因」这个缺陷从报表里删掉。
    pub blocked_reasons: Vec<ReasonCount>,
    /// outbox 按状态分组，含最早创建时刻（用于算等待时长）。
    pub deliveries: Vec<DeliveryBacklog>,
    /// 预算预留：只算未结算的（active / risk）。
    pub reservations: Vec<ReservationLevel>,
    pub audit_allowed: i64,
    pub audit_denied: i64,
    /// 无法从当前表结构算出的观测项，附原因。空数组表示「六项全有」。
    pub not_measured: Vec<NotMeasured>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStateCount {
    pub state: String,
    pub count: i64,
    /// 该状态下最早的下次到期时刻；`None` 表示没有排期信息。
    pub oldest_due_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonCount {
    pub reason: String,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryBacklog {
    pub status: String,
    pub count: i64,
    pub oldest_created_at_ms: Option<i64>,
    /// 最长等待：采集时刻减最早创建时刻。负数（时钟回拨）按 0 记，
    /// 不让一个不可能的值把整张报表带偏。
    pub max_wait_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationLevel {
    pub dimension: String,
    pub reserved: i64,
    pub risk: i64,
    pub rows: i64,
}

/// 一项观测缺口：名字 + 为什么算不出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotMeasured {
    pub item: &'static str,
    pub reason: &'static str,
}

impl NotMeasured {
    /// 当前表结构下确实算不出来的三项。写进报表而不是留空。
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                item: "事件缺口",
                reason: "SSE 事件序号在进程内存（event_stream::MemoryFeed），库中没有持久序列；\
                         要算缺口得先把事件序号落库（P6 2.6 持久订阅）",
            },
            Self {
                item: "端到端延迟（回滚观察 → 回执更新 → 渠道确认）",
                reason: "三段时刻分属不同表，且渠道确认尚未落库；现在计算会把缺失段当 0",
            },
            Self {
                item: "阻塞与待核实的等待时长",
                reason: "work_queue 只有 blocked_reason 与 next_due_at_ms，没有『进入该状态的时刻』；\
                         需要新增 blocked_since_ms / dispatched_at_ms 才能算时长而非条数",
            },
        ]
    }
}

/// 采集一次快照。`captured_at_ms` 由调用方给：读模型不自己取时钟，
/// 否则同一份报表在不同机器上无法复现。
pub fn snapshot(
    store: &SqliteStore,
    company_id: &str,
    captured_at_ms: i64,
) -> Result<ObservationSnapshot, StoreError> {
    let conn = store.read_connection();

    let mut queue_stmt = conn.prepare(
        "SELECT state, COUNT(*), MIN(next_due_at_ms)
         FROM work_queue WHERE company_id = ?1 GROUP BY state ORDER BY state",
    )?;
    let queue = queue_stmt
        .query_map(params![company_id], |row| {
            Ok(QueueStateCount {
                state: row.get(0)?,
                count: row.get(1)?,
                oldest_due_at_ms: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut blocked_stmt = conn.prepare(
        "SELECT COALESCE(blocked_reason, '(未记录)') AS reason, COUNT(*)
         FROM work_queue WHERE company_id = ?1 AND state = 'blocked'
         GROUP BY reason ORDER BY COUNT(*) DESC, reason",
    )?;
    let blocked_reasons = blocked_stmt
        .query_map(params![company_id], |row| {
            Ok(ReasonCount {
                reason: row.get(0)?,
                count: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut delivery_stmt = conn.prepare(
        "SELECT status, COUNT(*), MIN(created_at_ms)
         FROM outbox_deliveries WHERE company_id = ?1 GROUP BY status ORDER BY status",
    )?;
    let deliveries = delivery_stmt
        .query_map(params![company_id], |row| {
            let oldest: Option<i64> = row.get(2)?;
            Ok(DeliveryBacklog {
                status: row.get(0)?,
                count: row.get(1)?,
                oldest_created_at_ms: oldest,
                max_wait_ms: oldest
                    .map(|at| captured_at_ms.saturating_sub(at).max(0))
                    .unwrap_or(0),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // 只算未结算的：`consumed` / `cancelled` 是历史，算进「当前占用」会让预算看着一直满。
    let mut reservation_stmt = conn.prepare(
        "SELECT dimension, COALESCE(SUM(reserved_value), 0), COALESCE(SUM(risk_value), 0), COUNT(*)
         FROM reservations WHERE company_id = ?1 AND state IN ('active', 'risk')
         GROUP BY dimension ORDER BY dimension",
    )?;
    let reservations = reservation_stmt
        .query_map(params![company_id], |row| {
            Ok(ReservationLevel {
                dimension: row.get(0)?,
                reserved: row.get(1)?,
                risk: row.get(2)?,
                rows: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ObservationSnapshot {
        company_id: company_id.to_string(),
        captured_at_ms,
        queue,
        blocked_reasons,
        deliveries,
        reservations,
        audit_allowed: store.count_audit_events(company_id, AuditDecision::Allowed)?,
        audit_denied: store.count_audit_events(company_id, AuditDecision::Denied)?,
        not_measured: NotMeasured::defaults(),
    })
}

fn seconds(ms: i64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

impl ObservationSnapshot {
    /// 人读报表。措辞刻意保留「未测量」小节：报表最容易被误读的地方就是留白。
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "观测快照  company={}  captured_at_ms={}\n",
            self.company_id, self.captured_at_ms
        ));

        out.push_str("队列：");
        if self.queue.is_empty() {
            out.push_str("（无队列行）\n");
        } else {
            let parts = self
                .queue
                .iter()
                .map(|row| match row.oldest_due_at_ms {
                    Some(due) => format!("{} {}（最早到期 {}）", row.state, row.count, due),
                    None => format!("{} {}", row.state, row.count),
                })
                .collect::<Vec<_>>();
            out.push_str(&parts.join(" / "));
            out.push('\n');
        }

        out.push_str("阻塞原因：");
        if self.blocked_reasons.is_empty() {
            out.push_str("（无阻塞行）\n");
        } else {
            let parts = self
                .blocked_reasons
                .iter()
                .map(|row| format!("{} {}", row.reason, row.count))
                .collect::<Vec<_>>();
            out.push_str(&parts.join(" / "));
            out.push('\n');
        }

        out.push_str("外部动作：");
        if self.deliveries.is_empty() {
            out.push_str("（无 outbox 行）\n");
        } else {
            let parts = self
                .deliveries
                .iter()
                .map(|row| {
                    format!(
                        "{} {}（最长等待 {}）",
                        row.status,
                        row.count,
                        seconds(row.max_wait_ms)
                    )
                })
                .collect::<Vec<_>>();
            out.push_str(&parts.join(" / "));
            out.push('\n');
        }

        out.push_str("预算预留：");
        if self.reservations.is_empty() {
            out.push_str("（无未结算预留）\n");
        } else {
            let parts = self
                .reservations
                .iter()
                .map(|row| {
                    format!(
                        "{} 已留 {} / 风险 {}（{} 行）",
                        row.dimension, row.reserved, row.risk, row.rows
                    )
                })
                .collect::<Vec<_>>();
            out.push_str(&parts.join(" / "));
            out.push('\n');
        }

        out.push_str(&format!(
            "业务审计：allowed {} / denied {}\n",
            self.audit_allowed, self.audit_denied
        ));

        if self.not_measured.is_empty() {
            out.push_str("未测量：无\n");
        } else {
            out.push_str(&format!("未测量（{} 项）：\n", self.not_measured.len()));
            for item in &self.not_measured {
                out.push_str(&format!("  - {}：{}\n", item.item, item.reason));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;

    fn store() -> SqliteStore {
        SqliteStore::in_memory(&BodConfig::default()).expect("内存库应可打开")
    }

    /// 造数直接用 `&Connection` 执行 SQL：测试要的是「表里有这些行」，
    /// 不经过业务写入路径才能把读模型的聚合逻辑单独钉住。
    fn seed(store: &SqliteStore) {
        let conn = store.read_connection();
        // work_queue 需要满足代数约束：completed <= claimed <= wanted。
        conn.execute(
            "INSERT INTO work_queue (company_id, queue_key, state, wanted_generation,
                claimed_generation, completed_generation, next_due_at_ms)
             VALUES ('c-1', 'q-pending', 'pending', 1, 0, 0, 1000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_queue (company_id, queue_key, state, wanted_generation,
                claimed_generation, completed_generation, next_due_at_ms, blocked_reason)
             VALUES ('c-1', 'q-blocked-a', 'blocked', 1, 0, 0, 2000, 'lease_expired')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_queue (company_id, queue_key, state, wanted_generation,
                claimed_generation, completed_generation, next_due_at_ms, blocked_reason)
             VALUES ('c-1', 'q-blocked-b', 'blocked', 1, 0, 0, 3000, NULL)",
            [],
        )
        .unwrap();
        // 另一个公司：不得串读。
        conn.execute(
            "INSERT INTO work_queue (company_id, queue_key, state, wanted_generation,
                claimed_generation, completed_generation, next_due_at_ms)
             VALUES ('c-2', 'q-other', 'pending', 1, 0, 0, 1000)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO outbox_deliveries (event_or_command_id, destination_id, company_id,
                queue_key, request_digest, status, attempts, next_due_at_ms, created_at_ms)
             VALUES ('e-1', 'd-1', 'c-1', 'q-pending', 'dig', 'unknown', 1, 0, 1000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO outbox_deliveries (event_or_command_id, destination_id, company_id,
                queue_key, request_digest, status, attempts, next_due_at_ms, created_at_ms)
             VALUES ('e-2', 'd-1', 'c-1', 'q-pending', 'dig', 'confirmed', 1, 0, 500)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO reservations (reservation_id, company_id, dimension, reserved_value,
                confirmed_value, risk_value, state, created_at_ms, updated_at_ms)
             VALUES ('r-1', 'c-1', 'tokens', 1200, 0, 0, 'active', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO reservations (reservation_id, company_id, dimension, reserved_value,
                confirmed_value, risk_value, state, created_at_ms, updated_at_ms)
             VALUES ('r-2', 'c-1', 'tokens', 0, 0, 300, 'risk', 0, 0)",
            [],
        )
        .unwrap();
        // 已结算（consumed）：reserved 与 risk 都为 0，confirmed 落在已用量里。
        // 它不该算进「当前占用」，否则预算看着一直是满的。
        conn.execute(
            "INSERT INTO reservations (reservation_id, company_id, dimension, reserved_value,
                confirmed_value, risk_value, state, created_at_ms, updated_at_ms)
             VALUES ('r-3', 'c-1', 'tokens', 0, 50, 0, 'consumed', 0, 0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn snapshot_groups_by_state_and_keeps_unlabelled_blockers_visible() {
        let store = store();
        seed(&store);
        let snap = snapshot(&store, "c-1", 5_000).expect("采集应成功");

        assert_eq!(
            snap.queue,
            vec![
                QueueStateCount {
                    state: "blocked".to_string(),
                    count: 2,
                    oldest_due_at_ms: Some(2000)
                },
                QueueStateCount {
                    state: "pending".to_string(),
                    count: 1,
                    oldest_due_at_ms: Some(1000)
                },
            ],
            "另一个公司的 pending 不得算进来"
        );
        assert!(
            snap.blocked_reasons
                .iter()
                .any(|row| row.reason == "(未记录)" && row.count == 1),
            "没有原因文本的阻塞行必须显示，不能隐藏"
        );
    }

    #[test]
    fn deliveries_report_wait_time_and_skip_settled() {
        let store = store();
        seed(&store);
        let snap = snapshot(&store, "c-1", 5_000).expect("采集应成功");
        let unknown = snap
            .deliveries
            .iter()
            .find(|row| row.status == "unknown")
            .expect("unknown 应出现");
        assert_eq!(unknown.count, 1);
        assert_eq!(unknown.oldest_created_at_ms, Some(1000));
        assert_eq!(unknown.max_wait_ms, 4_000);
        assert!(
            snap.deliveries.iter().any(|row| row.status == "confirmed"),
            "读模型不在 SQL 里过滤状态：过滤是报表读者的选择，不是采集的选择"
        );
    }

    #[test]
    fn reservations_ignore_settled_rows_and_keep_risk_separate() {
        let store = store();
        seed(&store);
        let snap = snapshot(&store, "c-1", 0).expect("采集应成功");
        assert_eq!(snap.reservations.len(), 1);
        let tokens = &snap.reservations[0];
        assert_eq!(tokens.dimension, "tokens");
        assert_eq!(tokens.reserved, 1200, "cancelled 行的 0 不该混进来");
        assert_eq!(tokens.risk, 300, "风险量与已留量必须分开呈现");
        assert_eq!(tokens.rows, 2);
    }

    #[test]
    fn clock_rollback_does_not_produce_negative_wait() {
        let store = store();
        seed(&store);
        let snap = snapshot(&store, "c-1", 10).expect("采集应成功");
        for row in &snap.deliveries {
            assert!(row.max_wait_ms >= 0, "等待时长不得为负：{row:?}");
        }
    }

    #[test]
    fn empty_company_reports_zeros_but_still_lists_gaps() {
        let store = store();
        let snap = snapshot(&store, "c-none", 0).expect("采集应成功");
        assert!(snap.queue.is_empty());
        assert!(snap.deliveries.is_empty());
        assert_eq!(snap.audit_allowed, 0);
        assert!(
            !snap.not_measured.is_empty(),
            "没有数据不等于六项都测到了，缺口必须照常说明"
        );
        let text = snap.render();
        assert!(text.contains("（无队列行）"), "{text}");
        assert!(text.contains("未测量"), "{text}");
    }

    #[test]
    fn render_contains_every_section() {
        let store = store();
        seed(&store);
        let text = snapshot(&store, "c-1", 5_000).expect("采集应成功").render();
        for marker in [
            "队列：",
            "阻塞原因：",
            "外部动作：",
            "预算预留：",
            "业务审计：",
        ] {
            assert!(text.contains(marker), "缺少 {marker}：\n{text}");
        }
    }

    #[test]
    fn audit_counts_come_from_audit_events() {
        let mut store = store();
        {
            let uow = store.transaction().expect("开启事务");
            uow.append_audit_event(&audit(AuditDecision::Allowed, None), 1)
                .expect("写入应成功");
            uow.append_audit_event(&audit(AuditDecision::Denied, Some("缺 Grant")), 2)
                .expect("写入应成功");
            uow.commit().expect("提交应成功");
        }
        let snap = snapshot(&store, "c-1", 0).expect("采集应成功");
        assert_eq!(snap.audit_allowed, 1);
        assert_eq!(snap.audit_denied, 1);
    }

    fn audit(decision: AuditDecision, reason: Option<&str>) -> crate::audit_log::AuditEvent {
        use crate::domain::{DomainId, Scope};
        crate::audit_log::AuditEvent {
            company_id: "c-1".to_string(),
            correlation_id: "req-1".to_string(),
            actor: DomainId::new("u-1"),
            action: crate::audit_log::AuditAction::Accept,
            scope: Scope {
                company_id: DomainId::new("c-1"),
                project_id: Some(DomainId::new("p-1")),
                issue_id: None,
            },
            basis: "accept:acceptance".to_string(),
            decision,
            reason: reason.map(str::to_string),
            decided_at: "2026-11-21T10:00:00Z".to_string(),
        }
    }
}
