//! §11 交付跟踪：跟踪项 / 目标 / 追加式观察 / 来源身份 / 幂等回执 / 关联追加。
//!
//! 这一簇是 P8 的全部写入面。三条不变量写在代码里而不是文档里：
//! 1. 观察只追加，摘要取「最近一条带摘要的观察」，回滚是新的观察；
//! 2. 命令重放（幂等键）与对象重复（来源五元组）是两套机制，不合并；
//! 3. 重复导入返回**已有跟踪项**，不建第二个；改挂报错而不是悄悄挪走。

use super::*;

/// 交付跟踪的读取列顺序。集中一处，避免多处查询各写一份列清单后互相错位。
const TRACKING_SELECT: &str = "SELECT tracking_id, company_id, project_id, title, requirement,
            owner_id, state, issue_uid, run_uid, attempt_id, resource_version,
            created_at_ms, updated_at_ms
     FROM delivery_tracking";

const TARGET_SELECT: &str = "SELECT target_id, company_id, tracking_id, customer_id, environment,
            deployment_unit, acceptance, switch_state, population_known,
            created_at_ms, updated_at_ms
     FROM delivery_targets";

/// 观察读取列。第 17 列是关联表里的来源修订：一条观察最多挂一条修订（3.2）。
/// 用 LEFT JOIN 而不是给 016 的观察表加列——迁移每次打开都会重放，加列不幂等
/// （见 `017_delivery_sources.sql` 的说明）。
const OBSERVATION_SELECT: &str =
    "SELECT o.observation_id, o.company_id, o.tracking_id, o.target_id,
            o.kind, o.source_kind, o.source_ref, o.artifact_revision, o.digest, o.revision,
            o.note, o.actor_id, o.authorized, o.observed_at, o.observed_at_ms,
            o.recorded_at_ms, l.source_revision_id
     FROM delivery_observations o
     LEFT JOIN delivery_observation_sources l
       ON l.observation_id = o.observation_id AND l.company_id = o.company_id";

/// 交付跟踪的读侧取值错误。与审计一致：库里出现表外取值就炸，不静默回退。
fn delivery_row_error(
    index: usize,
    err: crate::delivery_tracking::DeliveryError,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            err.explain(),
        )),
    )
}

fn map_tracking_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackingRow> {
    let state_text: String = row.get(6)?;
    let state = TrackingState::parse(&state_text).map_err(|err| delivery_row_error(6, err))?;
    Ok(TrackingRow {
        tracking_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        title: row.get(3)?,
        requirement: row.get(4)?,
        owner_id: row.get(5)?,
        state,
        issue_uid: row.get(7)?,
        run_uid: row.get(8)?,
        attempt_id: row.get(9)?,
        resource_version: row.get(10)?,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
    })
}

fn map_target_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TargetRow> {
    use crate::coverage_receipt::SwitchState;
    use crate::delivery_tracking::DeliveryError;
    let switch_text: String = row.get(7)?;
    let switch_state = match switch_text.as_str() {
        "synced" => SwitchState::Synced,
        "not-synced" => SwitchState::NotSynced,
        "unknown" => SwitchState::Unknown,
        other => {
            return Err(delivery_row_error(
                7,
                DeliveryError::UnknownSwitchState(other.to_string()),
            ));
        }
    };
    Ok(TargetRow {
        target_id: row.get(0)?,
        company_id: row.get(1)?,
        tracking_id: row.get(2)?,
        customer_id: row.get(3)?,
        environment: row.get(4)?,
        deployment_unit: row.get(5)?,
        acceptance: row.get(6)?,
        switch_state,
        population_known: row.get::<_, i64>(8)? != 0,
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
    })
}

fn map_observation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ObservationRow> {
    use crate::delivery_tracking::{ObservationKind, parse_source_kind};
    let kind_text: String = row.get(4)?;
    let kind = ObservationKind::parse(&kind_text).map_err(|err| delivery_row_error(4, err))?;
    let source_text: String = row.get(5)?;
    let source_kind = parse_source_kind(&source_text).map_err(|err| delivery_row_error(5, err))?;
    Ok(ObservationRow {
        observation_id: row.get(0)?,
        company_id: row.get(1)?,
        tracking_id: row.get(2)?,
        target_id: row.get(3)?,
        kind,
        source_kind,
        source_ref: row.get(6)?,
        source_revision_id: row.get(16)?,
        artifact_revision: row.get(7)?,
        digest: row.get(8)?,
        revision: row.get(9)?,
        note: row.get(10)?,
        actor_id: row.get(11)?,
        authorized: row.get::<_, i64>(12)? != 0,
        observed_at: row.get(13)?,
        observed_at_ms: row.get(14)?,
        recorded_at_ms: row.get(15)?,
    })
}

const SOURCE_SELECT: &str = "SELECT source_id, company_id, project_id, connection_id,
            object_type, provider_object_id, locator, confirmed_by, confirmed_at_ms, created_at_ms
     FROM delivery_sources";

const SOURCE_REVISION_SELECT: &str = "SELECT source_revision_id, company_id, source_id, version,
            digest, content_fingerprint, actor_id, observed_at, observed_at_ms, recorded_at_ms
     FROM delivery_source_revisions";

const SOURCE_CANDIDATE_SELECT: &str = "SELECT candidate_id, company_id, project_id, connection_id,
            object_type, locator, proposed_by, proposed_at_ms, state, resolved_source_id,
            decided_by, decided_at_ms
     FROM delivery_source_candidates";

fn map_source_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<crate::delivery_source::SourceRow> {
    Ok(crate::delivery_source::SourceRow {
        source_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        connection_id: row.get(3)?,
        object_type: row.get(4)?,
        provider_object_id: row.get(5)?,
        locator: row.get(6)?,
        confirmed_by: row.get(7)?,
        confirmed_at_ms: row.get(8)?,
        created_at_ms: row.get(9)?,
    })
}

fn map_source_revision_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<crate::delivery_source::SourceRevisionRow> {
    Ok(crate::delivery_source::SourceRevisionRow {
        source_revision_id: row.get(0)?,
        company_id: row.get(1)?,
        source_id: row.get(2)?,
        version: row.get(3)?,
        digest: row.get(4)?,
        content_fingerprint: row.get(5)?,
        actor_id: row.get(6)?,
        observed_at: row.get(7)?,
        observed_at_ms: row.get(8)?,
        recorded_at_ms: row.get(9)?,
    })
}

fn map_source_candidate_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<crate::delivery_source::CandidateRow> {
    use crate::delivery_source::CandidateState;
    let state_text: String = row.get(8)?;
    let state = CandidateState::parse(&state_text).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                err.explain(),
            )),
        )
    })?;
    Ok(crate::delivery_source::CandidateRow {
        candidate_id: row.get(0)?,
        company_id: row.get(1)?,
        project_id: row.get(2)?,
        connection_id: row.get(3)?,
        object_type: row.get(4)?,
        locator: row.get(5)?,
        proposed_by: row.get(6)?,
        proposed_at_ms: row.get(7)?,
        state,
        resolved_source_id: row.get(9)?,
        decided_by: row.get(10)?,
        decided_at_ms: row.get(11)?,
    })
}

/// 回执读取列顺序。集中一处，读写两侧不会错位。
const RECEIPT_SELECT: &str = "SELECT company_id, idempotency_key, command, payload_fingerprint,
            result_kind, result_id, result_detail, recorded_at_ms
     FROM delivery_command_receipts";

/// 回执行映射。结果种类出现表外值一律报错：把它当成 tracking 会让重放返回一个
/// 类型错误的结果，而调用方会照着用。
fn map_receipt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Receipt> {
    let kind_text: String = row.get(4)?;
    let result_kind = ResultKind::parse(&kind_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("delivery_command_receipts.result_kind 出现表外值：{kind_text}"),
            )),
        )
    })?;
    Ok(Receipt {
        company_id: row.get(0)?,
        idempotency_key: row.get(1)?,
        command: row.get(2)?,
        payload_fingerprint: row.get(3)?,
        result_kind,
        result_id: row.get(5)?,
        result_detail: row.get(6)?,
        recorded_at_ms: row.get(7)?,
    })
}

/// 按 (公司, 幂等键) 读回执。裸 `Connection` 版本，供 `SqliteStore` 与 `UnitOfWork` 共用——
/// 两处各写一份 SQL 是列顺序漂移的老路。
fn find_receipt(
    conn: &rusqlite::Connection,
    company_id: &str,
    idempotency_key: &str,
) -> rusqlite::Result<Option<Receipt>> {
    conn.query_row(
        &format!("{RECEIPT_SELECT} WHERE company_id = ?1 AND idempotency_key = ?2"),
        params![company_id, idempotency_key.trim()],
        map_receipt_row,
    )
    .optional()
}

/// 来源 → 跟踪项。返回 `None` 表示这个身份还没挂任何交付。
fn lookup_tracking_for_source(
    conn: &rusqlite::Connection,
    company_id: &str,
    source_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT tracking_id FROM delivery_tracking_sources
         WHERE company_id = ?1 AND source_id = ?2",
        params![company_id, source_id],
        |row| row.get(0),
    )
    .optional()
}

/// 跟踪项已关联的内部工作包（3.4），按关联时间升序。裸 `Connection` 版本：
/// 读视图在 `SqliteStore` 上、写路径在 `UnitOfWork` 上，两处共用一份 SQL。
fn work_package_links(
    conn: &rusqlite::Connection,
    company_id: &str,
    tracking_id: &str,
) -> Result<Vec<WorkPackageLink>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT tracking_id, work_package_revision, linked_by, linked_at_ms
         FROM delivery_tracking_work_packages
         WHERE company_id = ?1 AND tracking_id = ?2
         ORDER BY linked_at_ms ASC, work_package_revision ASC",
    )?;
    let rows = stmt.query_map(params![company_id, tracking_id], |row| {
        Ok(WorkPackageLink {
            tracking_id: row.get(0)?,
            work_package_revision: row.get(1)?,
            linked_by: row.get(2)?,
            linked_at_ms: row.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
}

/// 按五元组查身份。查不到返回 `None`；**不做模糊匹配**——那会让两个来源悄悄合并。
fn find_source_id(
    conn: &rusqlite::Connection,
    identity: &crate::delivery_source::SourceIdentity,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT source_id FROM delivery_sources
         WHERE company_id = ?1 AND project_id = ?2 AND connection_id = ?3
           AND object_type = ?4 AND provider_object_id = ?5",
        params![
            identity.company_id,
            identity.project_id,
            identity.connection_id,
            identity.object_type,
            identity.provider_object_id,
        ],
        |row| row.get(0),
    )
    .optional()
}

impl SqliteStore {
    /// 读一个跟踪项的完整视图（P8 3.1，3.4 起含内部工作包关联）：
    /// 跟踪项 + 目标 + 观察 + 各目标的验证结论 + 后补的工作包。
    ///
    /// 查询条数固定（5 条），不随目标数量增长：观察与验证记录一次 JOIN 取回，
    /// 再在内存里按目标归并。**不写库**：读路径不产生审计、不改任何状态。
    ///
    /// 覆盖结论在这里算出（`project_target` → `judge_coverage`），不从表里读——
    /// 库里只有事实，结论永远由同一处判定产生（design D1）。
    pub fn delivery_tracking_view(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<Option<TrackingView>, StoreError> {
        let tracking = self
            .conn
            .query_row(
                &format!("{TRACKING_SELECT} WHERE company_id = ?1 AND tracking_id = ?2"),
                params![company_id, tracking_id],
                map_tracking_row,
            )
            .optional()?;
        let Some(tracking) = tracking else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(&format!(
            "{TARGET_SELECT} WHERE company_id = ?1 AND tracking_id = ?2
             ORDER BY customer_id ASC, environment ASC, deployment_unit ASC"
        ))?;
        let targets = stmt
            .query_map(params![company_id, tracking_id], map_target_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut stmt = self.conn.prepare(&format!(
            "{OBSERVATION_SELECT} WHERE o.company_id = ?1 AND o.tracking_id = ?2
             ORDER BY o.observed_at_ms DESC, o.recorded_at_ms DESC, o.observation_id DESC"
        ))?;
        let observations = stmt
            .query_map(params![company_id, tracking_id], map_observation_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let conclusions = self.delivery_verification_conclusions(company_id, tracking_id)?;
        let mut projected = Vec::with_capacity(targets.len());
        for target in &targets {
            // 克隆只为让投影不与连接共享借用；条数按目标计，不做全表扫描。
            let related: Vec<ObservationRow> = observations
                .iter()
                .filter(|observation| observation.target_id == target.target_id)
                .cloned()
                .collect();
            let conclusion = conclusions
                .get(&target.target_id)
                .copied()
                .unwrap_or(VerificationConclusion::Unknown);
            projected.push(project_target(
                &tracking.project_id,
                target,
                &related,
                conclusion,
            ));
        }

        let evidence = evidence_stamps(&observations);
        Ok(Some(TrackingView {
            tracking,
            targets: projected,
            evidence,
            work_packages: self.list_work_package_links(company_id, tracking_id)?,
        }))
    }

    /// 列出某 Company 的跟踪项与目标数量。
    ///
    /// 列表刻意**不带覆盖结论**：结论离开证据没有意义，而列表里塞不下证据；
    /// 列出来就会被当成可以引用的结论。要结论请读单个跟踪项。
    pub fn list_delivery_tracking(
        &self,
        company_id: &str,
        limit: usize,
    ) -> Result<Vec<(TrackingRow, i64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT t.tracking_id, t.company_id, t.project_id, t.title, t.requirement,
                    t.owner_id, t.state, t.issue_uid, t.run_uid, t.attempt_id,
                    t.resource_version, t.created_at_ms, t.updated_at_ms,
                    COUNT(g.target_id)
             FROM delivery_tracking t
             LEFT JOIN delivery_targets g
               ON g.company_id = t.company_id AND g.tracking_id = t.tracking_id
             WHERE t.company_id = ?1
             GROUP BY t.tracking_id
             ORDER BY t.created_at_ms DESC, t.tracking_id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![company_id, limit as i64], |row| {
            Ok((map_tracking_row(row)?, row.get::<_, i64>(13)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 读一条命令回执（P8 3.3）。查不到返回 `None`，调用方据此判定「首次执行」。
    ///
    /// 只读：回执不可改写（库层触发器也拦），因为重放必须返回**当时**的结果。
    pub fn find_command_receipt(
        &self,
        company_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<Receipt>, StoreError> {
        find_receipt(&self.conn, company_id, idempotency_key).map_err(StoreError::Sql)
    }

    /// 读一条来源关系（P8 3.3）：这个身份承载哪一条跟踪项。
    ///
    /// 查不到返回 `None`：来源可能刚登记、还没被任何交付引用。
    pub fn find_tracking_for_source(
        &self,
        company_id: &str,
        source_id: &str,
    ) -> Result<Option<String>, StoreError> {
        lookup_tracking_for_source(&self.conn, company_id, source_id).map_err(StoreError::Sql)
    }

    /// 读跟踪项已关联的内部工作包（P8 3.4），按关联时间升序。
    ///
    /// 只读：`bod tracking --id` 用它把「后补的内部工作包」显示出来。
    /// 不参与覆盖判定——工作包存在不等于交付完成，证据仍然只有观察。
    pub fn list_work_package_links(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<Vec<WorkPackageLink>, StoreError> {
        work_package_links(&self.conn, company_id, tracking_id)
    }

    /// 读一个来源身份。查不到返回 `None`——「不知道」与「没有」要分开。
    pub fn get_source(
        &self,
        company_id: &str,
        source_id: &str,
    ) -> Result<Option<crate::delivery_source::SourceRow>, StoreError> {
        self.conn
            .query_row(
                &format!("{SOURCE_SELECT} WHERE company_id = ?1 AND source_id = ?2"),
                params![company_id, source_id],
                map_source_row,
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 列出已确认的来源身份与各自的修订条数。
    ///
    /// 不返回「哪一版是最新」：那要比较版本语义（commit 序、语义化版本），
    /// 不同连接器的比较规则不同，本段不猜。时间线请读单个来源。
    pub fn list_sources(
        &self,
        company_id: &str,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(crate::delivery_source::SourceRow, i64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.source_id, s.company_id, s.project_id, s.connection_id,
                    s.object_type, s.provider_object_id, s.locator, s.confirmed_by,
                    s.confirmed_at_ms, s.created_at_ms, COUNT(r.source_revision_id)
             FROM delivery_sources s
             LEFT JOIN delivery_source_revisions r
               ON r.company_id = s.company_id AND r.source_id = s.source_id
             WHERE s.company_id = ?1 AND (?2 IS NULL OR s.project_id = ?2)
             GROUP BY s.source_id
             ORDER BY s.created_at_ms DESC, s.source_id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![company_id, project_id, limit as i64], |row| {
            Ok((map_source_row(row)?, row.get::<_, i64>(10)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 一个来源的修订时间线，**按观察时间升序**（可变标签要看前后顺序）。
    pub fn list_source_revisions(
        &self,
        company_id: &str,
        source_id: &str,
    ) -> Result<Vec<crate::delivery_source::SourceRevisionRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SOURCE_REVISION_SELECT} WHERE company_id = ?1 AND source_id = ?2
             ORDER BY observed_at_ms ASC, recorded_at_ms ASC, source_revision_id ASC"
        ))?;
        let rows = stmt.query_map(params![company_id, source_id], map_source_revision_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出来源候选。`pending_only` 时只给待确认的——它们才是需要动作的那一类。
    pub fn list_source_candidates(
        &self,
        company_id: &str,
        project_id: Option<&str>,
        pending_only: bool,
        limit: usize,
    ) -> Result<Vec<crate::delivery_source::CandidateRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SOURCE_CANDIDATE_SELECT}
             WHERE company_id = ?1 AND (?2 IS NULL OR project_id = ?2)
               AND (?3 = 0 OR state = 'pending')
             ORDER BY CASE state WHEN 'pending' THEN 0 ELSE 1 END,
                      proposed_at_ms DESC, candidate_id ASC
             LIMIT ?4"
        ))?;
        let rows = stmt.query_map(
            params![company_id, project_id, pending_only as i64, limit as i64],
            map_source_candidate_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 每个目标的验证结论：取「最近一条带制品修订的观察」对应的最新验证记录。
    ///
    /// 没带修订的观察不参与——「没给修订」不是否定证据，不该把已有的验证结论抹掉。
    /// `waived` 仍映射为 `Unknown`（见 `verification_conclusion`）。
    fn delivery_verification_conclusions(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<std::collections::HashMap<String, VerificationConclusion>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT o.target_id, v.state
             FROM delivery_observations o
             JOIN verification_records v
               ON v.company_id = o.company_id AND v.revision_id = o.artifact_revision
             WHERE o.company_id = ?1 AND o.tracking_id = ?2
             ORDER BY o.observed_at_ms DESC, o.recorded_at_ms DESC, v.observed_at_ms DESC",
        )?;
        let rows = stmt.query_map(params![company_id, tracking_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut conclusions = std::collections::HashMap::new();
        for row in rows {
            let (target_id, state) = row?;
            if conclusions.contains_key(&target_id) {
                continue;
            }
            conclusions.insert(target_id, verification_conclusion(&state)?);
        }
        Ok(conclusions)
    }

    /// 只读列出 (Issue uid, 工作包修订) 关联，供工作台把首次使用的交付证据归属到任务页。
    pub fn list_work_package_issue_links(
        &self,
        company_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_uid, revision_id FROM work_package_revisions WHERE company_id = ?1
             ORDER BY issue_uid ASC, revision_id ASC",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }
}

impl<'a> UnitOfWork<'a> {
    /// 登记一个跟踪项（P8 3.1 的写入原语，不是导入端点）。
    ///
    /// 幂等键、来源身份去重与目标追加语义分别在 3.3 / 3.4 / 3.5 落地；这里只保证
    /// 「一行能按约束写进去」，且内部引用（issue/run/attempt）缺省时**不伪造运行**（D3）。
    pub fn insert_delivery_tracking(&self, row: &TrackingRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO delivery_tracking (
                tracking_id, company_id, project_id, title, requirement, owner_id, state,
                issue_uid, run_uid, attempt_id, resource_version, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                row.tracking_id,
                row.company_id,
                row.project_id,
                row.title,
                row.requirement,
                row.owner_id,
                row.state.as_str(),
                row.issue_uid,
                row.run_uid,
                row.attempt_id,
                row.resource_version,
                row.created_at_ms,
                row.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 登记一个客户目标（同一跟踪项下「客户 × 环境 × 单元」唯一）。
    pub fn insert_delivery_target(&self, row: &TargetRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO delivery_targets (
                target_id, company_id, tracking_id, customer_id, environment, deployment_unit,
                acceptance, switch_state, population_known, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.target_id,
                row.company_id,
                row.tracking_id,
                row.customer_id,
                row.environment,
                row.deployment_unit,
                row.acceptance,
                row.switch_state.as_str(),
                row.population_known as i64,
                row.created_at_ms,
                row.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 追加一条观察。**只追加，不改写**——回滚是新的观察（3.1 的两条核心语义之一）。
    ///
    /// 两道显式校验先于库里约束，为的是给出可读原因：
    /// 人工补证没有授权范围与复验依据（`note` 为空）直接拒绝；
    /// 目标不属于该跟踪项直接拒绝（复合外键会拦，但报错信息读不出来）。
    pub fn append_delivery_observation(&self, row: &ObservationRow) -> Result<(), StoreError> {
        if row.kind == ObservationKind::ManualEvidence && row.note.trim().is_empty() {
            return Err(DeliveryError::EmptyManualEvidenceNote.into());
        }
        let belongs: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM delivery_targets
             WHERE company_id = ?1 AND tracking_id = ?2 AND target_id = ?3",
            params![row.company_id, row.tracking_id, row.target_id],
            |line| line.get(0),
        )?;
        if belongs == 0 {
            return Err(DeliveryError::TargetNotInTracking {
                target_id: row.target_id.clone(),
                tracking_id: row.tracking_id.clone(),
            }
            .into());
        }
        // 观察挂来源修订时，来源必须存在且属于同一项目：跨项目的来源不得挂到本项目交付上。
        // 复合外键只能保证「修订存在」，它读不出「项目不同」这件事。
        if let Some(revision_id) = &row.source_revision_id {
            let source_project: Option<String> = self
                .conn
                .query_row(
                    "SELECT s.project_id
                     FROM delivery_source_revisions r
                     JOIN delivery_sources s
                       ON s.company_id = r.company_id AND s.source_id = r.source_id
                     WHERE r.company_id = ?1 AND r.source_revision_id = ?2",
                    params![row.company_id, revision_id],
                    |line| line.get(0),
                )
                .optional()?;
            let Some(source_project) = source_project else {
                return Err(crate::delivery_source::SourceError::SourceRevisionNotFound(
                    revision_id.clone(),
                )
                .into());
            };
            let tracking_project: String = self.conn.query_row(
                "SELECT project_id FROM delivery_tracking
                 WHERE company_id = ?1 AND tracking_id = ?2",
                params![row.company_id, row.tracking_id],
                |line| line.get(0),
            )?;
            if source_project != tracking_project {
                return Err(crate::delivery_source::SourceError::SourceProjectMismatch {
                    source_project,
                    tracking_project,
                }
                .into());
            }
        }
        self.conn.execute(
            "INSERT INTO delivery_observations (
                observation_id, company_id, tracking_id, target_id, kind, source_kind, source_ref,
                artifact_revision, digest, revision, note, actor_id, authorized,
                observed_at, observed_at_ms, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                row.observation_id,
                row.company_id,
                row.tracking_id,
                row.target_id,
                row.kind.as_str(),
                row.source_kind,
                row.source_ref,
                row.artifact_revision,
                row.digest,
                row.revision,
                row.note,
                row.actor_id,
                row.authorized as i64,
                row.observed_at,
                row.observed_at_ms,
                row.recorded_at_ms,
            ],
        )?;
        if let Some(revision_id) = &row.source_revision_id {
            self.conn.execute(
                "INSERT INTO delivery_observation_sources (
                    observation_id, company_id, source_revision_id, linked_at_ms
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    row.observation_id,
                    row.company_id,
                    revision_id,
                    row.recorded_at_ms,
                ],
            )?;
        }
        Ok(())
    }

    /// 登记一个来源身份（五元组）。**幂等**：五元组已存在时返回已有 ID，
    /// 不覆盖首个确认人——「谁先把这个 ID 认成那个对象」是历史事实。
    ///
    /// `source_id` 只在新建时使用。ID 生成留给导入路径（3.5）：本函数与 3.1 的
    /// 写入原语一致，由调用方给主键，测试因此能确定地引用同一行。
    pub fn register_source(
        &self,
        source_id: &str,
        identity: &crate::delivery_source::SourceIdentity,
        locator: &str,
        confirmed_by: &str,
        now_ms: i64,
    ) -> Result<RegisteredSource, StoreError> {
        use crate::delivery_source::SourceError;
        identity.validate()?;
        if confirmed_by.trim().is_empty() {
            return Err(SourceError::MissingConfirmer.into());
        }
        if let Some(existing) = find_source_id(self.conn, identity)? {
            return Ok(RegisteredSource {
                source_id: existing,
                created: false,
            });
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_sources (
                source_id, company_id, project_id, connection_id, object_type,
                provider_object_id, locator, confirmed_by, confirmed_at_ms, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                source_id,
                identity.company_id,
                identity.project_id,
                identity.connection_id,
                identity.object_type,
                identity.provider_object_id,
                locator.trim(),
                confirmed_by,
                now_ms,
                now_ms,
            ],
        );
        match result {
            Ok(_) => Ok(RegisteredSource {
                source_id: source_id.to_string(),
                created: true,
            }),
            // 并发下两次登记同一身份：后到者拿已有行，而不是报一个没人看得懂的唯一约束错误。
            Err(err) if is_unique_violation(&err) => match find_source_id(self.conn, identity)? {
                Some(existing) => Ok(RegisteredSource {
                    source_id: existing,
                    created: false,
                }),
                None => Err(err.into()),
            },
            Err(err) => Err(err.into()),
        }
    }

    /// 提交一个来源候选。**幂等**：同一作用域下同一 `locator` 的待确认候选只留一条。
    ///
    /// 候选不是身份：它没有稳定 ID。重复提交同一 URL 应当得到同一条待办，
    /// 而不是四条等着同一个人点四次。
    pub fn propose_source_candidate(
        &self,
        candidate_id: &str,
        proposal: &crate::delivery_source::CandidateProposal,
        proposed_by: &str,
        now_ms: i64,
    ) -> Result<ProposedCandidate, StoreError> {
        use crate::delivery_source::SourceError;
        if proposal.locator.trim().is_empty() {
            return Err(SourceError::EmptyLocator.into());
        }
        if proposed_by.trim().is_empty() {
            return Err(SourceError::MissingProposer.into());
        }
        let lookup = |conn: &rusqlite::Connection| -> rusqlite::Result<Option<String>> {
            conn.query_row(
                "SELECT candidate_id FROM delivery_source_candidates
                 WHERE company_id = ?1 AND project_id = ?2 AND connection_id = ?3
                   AND locator = ?4 AND state = 'pending'",
                params![
                    proposal.company_id,
                    proposal.project_id,
                    proposal.connection_id,
                    proposal.locator.trim(),
                ],
                |row| row.get(0),
            )
            .optional()
        };
        if let Some(existing) = lookup(self.conn)? {
            return Ok(ProposedCandidate {
                candidate_id: existing,
                created: false,
            });
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_source_candidates (
                candidate_id, company_id, project_id, connection_id, object_type,
                locator, proposed_by, proposed_at_ms, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
            params![
                candidate_id,
                proposal.company_id,
                proposal.project_id,
                proposal.connection_id,
                proposal.object_type,
                proposal.locator.trim(),
                proposed_by,
                now_ms,
            ],
        );
        match result {
            Ok(_) => Ok(ProposedCandidate {
                candidate_id: candidate_id.to_string(),
                created: true,
            }),
            Err(err) if is_unique_violation(&err) => match lookup(self.conn)? {
                Some(id) => Ok(ProposedCandidate {
                    candidate_id: id,
                    created: false,
                }),
                None => Err(err.into()),
            },
            Err(err) => Err(err.into()),
        }
    }

    /// 确认候选：候选 → 身份，两个写入必须同生共死。
    ///
    /// 用 `SAVEPOINT` 包住，不是多余：调用方（导入路径）在自己的事务里，若这里
    /// 「身份登记成功、候选状态没改成」却返回失败，调用方很容易照样提交，
    /// 于是同一个定位会留下两条待确认候选。函数自己保证原子性。
    pub fn confirm_source_candidate(
        &self,
        company_id: &str,
        candidate_id: &str,
        source_id: &str,
        identity: &crate::delivery_source::SourceIdentity,
        decided_by: &str,
        now_ms: i64,
    ) -> Result<ConfirmedCandidate, StoreError> {
        use crate::delivery_source::{CandidateState, SourceError, decide, ensure_scope_matches};
        self.conn
            .execute_batch("SAVEPOINT confirm_source_candidate")?;
        let outcome = (|| -> Result<ConfirmedCandidate, StoreError> {
            let candidate = self
                .conn
                .query_row(
                    &format!(
                        "{SOURCE_CANDIDATE_SELECT} WHERE company_id = ?1 AND candidate_id = ?2"
                    ),
                    params![company_id, candidate_id],
                    map_source_candidate_row,
                )
                .optional()?
                .ok_or_else(|| SourceError::CandidateNotFound(candidate_id.to_string()))?;
            decide(candidate.state)?;
            ensure_scope_matches(&candidate, identity)?;
            if decided_by.trim().is_empty() {
                return Err(SourceError::MissingConfirmer.into());
            }
            let registered =
                self.register_source(source_id, identity, &candidate.locator, decided_by, now_ms)?;
            let changed = self.conn.execute(
                "UPDATE delivery_source_candidates
                 SET state = 'confirmed', resolved_source_id = ?1,
                     decided_by = ?2, decided_at_ms = ?3
                 WHERE company_id = ?4 AND candidate_id = ?5 AND state = 'pending'",
                params![
                    registered.source_id,
                    decided_by,
                    now_ms,
                    company_id,
                    candidate_id,
                ],
            )?;
            if changed != 1 {
                // 并发下另一个人先确认了：本次注册的身份随之回滚（savepoint），
                // 报告「已裁决」而不是留下一个没人引用的身份。
                return Err(SourceError::CandidateAlreadyDecided(
                    CandidateState::Confirmed.as_str().to_string(),
                )
                .into());
            }
            Ok(ConfirmedCandidate {
                candidate_id: candidate_id.to_string(),
                source_id: registered.source_id,
                source_created: registered.created,
            })
        })();
        match outcome {
            Ok(value) => {
                self.conn
                    .execute_batch("RELEASE confirm_source_candidate")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO confirm_source_candidate; RELEASE confirm_source_candidate",
                );
                Err(err)
            }
        }
    }

    /// 追加一条来源修订。**幂等**：内容指纹相同的事件返回已有修订。
    ///
    /// 指纹必须与 `(version, digest)` 重算结果一致——让调用方自己编指纹，
    /// 幂等键就成了「调用方承诺」，重复上报只要换个指纹就能插进来。
    pub fn append_source_revision(
        &self,
        revision: &crate::delivery_source::SourceRevisionRow,
    ) -> Result<AppendedRevision, StoreError> {
        use crate::delivery_source::{SourceError, revision_fingerprint};
        let derived = revision_fingerprint(&revision.version, &revision.digest)?;
        if derived != revision.content_fingerprint {
            return Err(SourceError::FingerprintMismatch {
                declared: revision.content_fingerprint.clone(),
                derived,
            }
            .into());
        }
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM delivery_sources WHERE company_id = ?1 AND source_id = ?2",
            params![revision.company_id, revision.source_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(SourceError::SourceNotFound(revision.source_id.clone()).into());
        }
        let lookup = |conn: &rusqlite::Connection| -> rusqlite::Result<Option<String>> {
            conn.query_row(
                "SELECT source_revision_id FROM delivery_source_revisions
                 WHERE company_id = ?1 AND source_id = ?2 AND content_fingerprint = ?3",
                params![
                    revision.company_id,
                    revision.source_id,
                    revision.content_fingerprint,
                ],
                |row| row.get(0),
            )
            .optional()
        };
        if let Some(existing) = lookup(self.conn)? {
            return Ok(AppendedRevision {
                source_revision_id: existing,
                created: false,
            });
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_source_revisions (
                source_revision_id, company_id, source_id, version, digest,
                content_fingerprint, actor_id, observed_at, observed_at_ms, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                revision.source_revision_id,
                revision.company_id,
                revision.source_id,
                revision.version,
                revision.digest,
                revision.content_fingerprint,
                revision.actor_id,
                revision.observed_at,
                revision.observed_at_ms,
                revision.recorded_at_ms,
            ],
        );
        match result {
            Ok(_) => Ok(AppendedRevision {
                source_revision_id: revision.source_revision_id.clone(),
                created: true,
            }),
            Err(err) if is_unique_violation(&err) => match lookup(self.conn)? {
                Some(id) => Ok(AppendedRevision {
                    source_revision_id: id,
                    created: false,
                }),
                None => Err(err.into()),
            },
            Err(err) => Err(err.into()),
        }
    }

    /// 把来源身份挂到一条跟踪项上（§11「来源关系」）。
    ///
    /// 三条语义，逐条都有测试：
    /// - 已经挂着**同一条** → `created=false`，不重复写。重复导入走的就是这条。
    /// - 已经挂着**另一条** → `SourceAlreadyLinked`。不偷偷改挂：改挂是悄悄挪走一条
    ///   已经承诺出去的交付，而且会让「重复导入返回已有关系」失去唯一对象可返回。
    /// - 跨项目 → `SourceProjectMismatch`。复合外键只保证两边存在，读不出「项目不同」。
    pub fn link_delivery_tracking_source(
        &self,
        company_id: &str,
        source_id: &str,
        tracking_id: &str,
        linked_by: &str,
        now_ms: i64,
    ) -> Result<LinkedSource, StoreError> {
        if linked_by.trim().is_empty() {
            return Err(SourceError::MissingLinker.into());
        }
        let source_project: Option<String> = self
            .conn
            .query_row(
                "SELECT project_id FROM delivery_sources WHERE company_id = ?1 AND source_id = ?2",
                params![company_id, source_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(source_project) = source_project else {
            return Err(SourceError::SourceNotFound(source_id.to_string()).into());
        };
        let tracking_project: Option<String> = self
            .conn
            .query_row(
                "SELECT project_id FROM delivery_tracking WHERE company_id = ?1 AND tracking_id = ?2",
                params![company_id, tracking_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(tracking_project) = tracking_project else {
            return Err(DeliveryError::TrackingNotFound(tracking_id.to_string()).into());
        };
        if source_project != tracking_project {
            return Err(SourceError::SourceProjectMismatch {
                source_project,
                tracking_project,
            }
            .into());
        }
        if let Some(existing) = lookup_tracking_for_source(self.conn, company_id, source_id)? {
            if existing == tracking_id {
                return Ok(LinkedSource {
                    tracking_id: existing,
                    created: false,
                });
            }
            return Err(SourceError::SourceAlreadyLinked {
                source_id: source_id.to_string(),
                tracking_id: tracking_id.to_string(),
                linked_tracking_id: existing,
            }
            .into());
        }
        let result = self.conn.execute(
            "INSERT INTO delivery_tracking_sources (
                company_id, source_id, tracking_id, linked_by, linked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![company_id, source_id, tracking_id, linked_by, now_ms],
        );
        match result {
            Ok(_) => Ok(LinkedSource {
                tracking_id: tracking_id.to_string(),
                created: true,
            }),
            // 并发下另一个写入者刚挂上：后到者拿已有关系，不报一个读不懂的唯一约束错误。
            Err(err) if is_unique_violation(&err) => {
                match lookup_tracking_for_source(self.conn, company_id, source_id)? {
                    Some(existing) if existing == tracking_id => Ok(LinkedSource {
                        tracking_id: existing,
                        created: false,
                    }),
                    Some(existing) => Err(SourceError::SourceAlreadyLinked {
                        source_id: source_id.to_string(),
                        tracking_id: tracking_id.to_string(),
                        linked_tracking_id: existing,
                    }
                    .into()),
                    None => Err(err.into()),
                }
            }
            Err(err) => Err(err.into()),
        }
    }

    /// 导入一次外部来源：**两种去重在这里分工**（§11 / design D2）。
    ///
    /// 1. 先读回执（幂等键，管命令重放）：有回执 → 同命令同载荷就原样返回首次结果，
    ///    换命令或换载荷就报冲突。整段**不进入写入路径**。
    /// 2. 没有回执 → 解析来源。成为身份时登记五元组并挂来源关系；已有关系就复用它。
    ///    只有定位时进候选，**不建跟踪项**——候选没有稳定身份，支撑不了覆盖结论。
    /// 3. 最后写回执。整段包在 `SAVEPOINT` 里：任何一步失败都不留下「这个键已执行」。
    ///
    /// 为什么回执与业务写入必须同事务：先写回执再干活，失败后重试会撞上自己伪造的
    /// 冲突；先干活再写回执而不回滚，重试会重放出一个从未发生的成功。两种都错。
    pub fn import_delivery_source(
        &self,
        command: &ImportSourceCommand,
        now_ms: i64,
    ) -> Result<ImportOutcome, StoreError> {
        if command.company_id.trim().is_empty() {
            return Err(IdempotencyError::EmptyCompanyId.into());
        }
        let key = crate::delivery_idempotency::validate_key(&command.idempotency_key)?;
        let fingerprint =
            crate::delivery_idempotency::payload_fingerprint(&command.payload_fields())?;

        // 重放判定在读回执这一步就结束：命中的命令不再碰任何业务表。
        let existing = find_receipt(self.conn, &command.company_id, key)?;
        if let Replay::Replayed(receipt) = classify(existing, COMMAND_IMPORT_SOURCE, &fingerprint)?
        {
            return ImportOutcome::replayed_from(&receipt.result_detail).map_err(|detail| {
                IdempotencyError::UnreadableReceipt {
                    key: key.to_string(),
                    detail,
                }
                .into()
            });
        }

        self.conn
            .execute_batch("SAVEPOINT import_delivery_source")?;
        let outcome = (|| -> Result<ImportOutcome, StoreError> {
            let imported = match crate::delivery_source::resolve(
                &command.company_id,
                &command.project_id,
                &command.connection_id,
                command.object_type.as_deref(),
                command.provider_object_id.as_deref(),
                &command.locator,
            )? {
                SourceResolution::Identity(identity) => {
                    self.import_identity(command, &identity, now_ms)?
                }
                SourceResolution::Candidate(proposal) => {
                    let proposed = self.propose_source_candidate(
                        &command.new_candidate_id,
                        &proposal,
                        &command.imported_by,
                        now_ms,
                    )?;
                    ImportOutcome::candidate(proposed.candidate_id, proposed.created)
                }
            };
            let detail = serde_json::to_string(&imported).map_err(|err| {
                IdempotencyError::ResultNotRecordable(format!("结果体无法编码为 JSON：{err}"))
            })?;
            let result_kind = if imported.candidate_id.is_some() {
                ResultKind::Candidate
            } else {
                ResultKind::Tracking
            };
            let result_id = imported
                .candidate_id
                .as_deref()
                .or(imported.tracking_id.as_deref())
                .unwrap_or_default();
            // 审计与业务写入同事务（SAVEPOINT 内）：先写回执会伪造冲突，先写审计而业务失败
            // 会留下一条「登记过」的假记录。两条路都由同一个 savepoint 兜住。
            //
            // correlation_id 取命令给的链路标识，不用幂等键：键是调用方自编的字符串，
            // 可能含 `secret` 之类形态，会让脱敏闸门拒掉整条导入。
            // 重放命中回执时不会走到这里（重放没有产生任何事实，不该再记一条）。
            let evidence = if imported.candidate_id.is_some() {
                "source_candidate"
            } else {
                "source_identity"
            };
            let event = crate::audit_log::AuditEvent::for_write(
                command.company_id.as_str(),
                command.correlation_id.as_str(),
                crate::domain::DomainId::new(command.imported_by.as_str()),
                AuditAction::ImportSource,
                crate::domain::Scope {
                    company_id: crate::domain::DomainId::new(command.company_id.as_str()),
                    project_id: Some(crate::domain::DomainId::new(command.project_id.as_str())),
                    issue_id: None,
                },
                evidence,
                command.decided_at.as_str(),
            )
            .map_err(StoreError::Audit)?;
            self.append_audit_event(&event, now_ms)?;
            self.conn.execute(
                "INSERT INTO delivery_command_receipts (
                    company_id, idempotency_key, command, payload_fingerprint,
                    result_kind, result_id, result_detail, recorded_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    command.company_id,
                    key,
                    COMMAND_IMPORT_SOURCE,
                    fingerprint,
                    result_kind.as_str(),
                    result_id,
                    detail,
                    now_ms,
                ],
            )?;
            Ok(imported)
        })();
        match outcome {
            Ok(value) => {
                self.conn.execute_batch("RELEASE import_delivery_source")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO import_delivery_source; RELEASE import_delivery_source",
                );
                Err(err)
            }
        }
    }

    /// 导入的身份分支：登记身份 → 找到或建跟踪项 → 挂关系。
    ///
    /// 三种情形，返回值只差 `created_*`：
    /// - 身份已承载跟踪项 → 复用它（**重复导入返回已有关系**，不建第二个跟踪项）；
    /// - 身份没有关系，但 `new_tracking_id` 已存在 → 挂到它上面（导入到指定跟踪项）；
    /// - 都不存在 → 建跟踪项再挂。
    fn import_identity(
        &self,
        command: &ImportSourceCommand,
        identity: &SourceIdentity,
        now_ms: i64,
    ) -> Result<ImportOutcome, StoreError> {
        let registered = self.register_source(
            &command.new_source_id,
            identity,
            &command.locator,
            &command.imported_by,
            now_ms,
        )?;
        let (tracking_id, created_tracking) = match lookup_tracking_for_source(
            self.conn,
            &command.company_id,
            &registered.source_id,
        )? {
            Some(existing) => (existing, false),
            None => {
                let exists: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM delivery_tracking
                         WHERE company_id = ?1 AND tracking_id = ?2",
                    params![command.company_id, command.new_tracking_id],
                    |row| row.get(0),
                )?;
                if exists > 0 {
                    (command.new_tracking_id.clone(), false)
                } else {
                    self.insert_delivery_tracking(&TrackingRow {
                        tracking_id: command.new_tracking_id.clone(),
                        company_id: command.company_id.clone(),
                        project_id: command.project_id.clone(),
                        title: command.title.clone(),
                        requirement: command.requirement.clone(),
                        owner_id: command.owner_id.clone(),
                        state: crate::delivery_tracking::TrackingState::Open,
                        // 外部导入不伪造内部运行（D3）。
                        issue_uid: None,
                        run_uid: None,
                        attempt_id: None,
                        resource_version: 1,
                        created_at_ms: now_ms,
                        updated_at_ms: now_ms,
                    })?;
                    (command.new_tracking_id.clone(), true)
                }
            }
        };
        let linked = self.link_delivery_tracking_source(
            &command.company_id,
            &registered.source_id,
            &tracking_id,
            &command.imported_by,
            now_ms,
        )?;
        Ok(ImportOutcome::tracked(
            linked.tracking_id,
            registered.source_id,
            created_tracking,
            registered.created,
        ))
    }

    /// 读跟踪项的写入闸门：当前版本与状态。
    ///
    /// 单独一条查询而不是复用 `delivery_tracking_view`：追加只需要两个标量，
    /// 读全量视图会把目标、观察、验证结论都算一遍，而那些值在读到时可能已经变了。
    /// 读跟踪项的版本、状态与所属项目。
    ///
    /// 三项一起取而不是分两次查：它们必须来自同一行，分两次读会在并发改动后拼出
    /// 「版本属于第一次、项目属于第二次」的结果。
    fn delivery_tracking_gate(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<(i64, TrackingState, String), StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT resource_version, state, project_id FROM delivery_tracking
                 WHERE company_id = ?1 AND tracking_id = ?2",
                params![company_id, tracking_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((version, state, project_id)) = row else {
            return Err(DeliveryError::TrackingNotFound(tracking_id.to_string()).into());
        };
        Ok((version, TrackingState::parse(&state)?, project_id))
    }

    /// 追加一条关联（P8 3.4）：来源或内部工作包，连同要确保存在的客户目标。
    ///
    /// 顺序固定：校验 → 读回执 → 期望版本闸门 → 挂关系 → 补目标 → 有实际新增才推进版本
    /// → 写回执，整段在 `SAVEPOINT` 里。四条语义各有测试：
    ///
    /// 1. **期望版本不一致就什么都不写**。不做「尽力而为」的合并：那次写入是照着
    ///    另一个状态做的决定，合并进去等于替调用方改主意。
    /// 2. **纯重复不推进版本**。关系和目标都已存在时 `appended=false`、版本不动；
    ///    若照样 +1，两个客户端会互相把对方的期望版本判过期，重试变成必冲突——
    ///    「重试不重复建项」就成了空话。
    /// 3. **已关闭的跟踪项不再追加**。与视图同口径：`allowed_actions` 在关闭时本来
    ///    就不给 `link-source`，写入口若还放行，视图与事实就分家了。
    /// 4. **重放返回首次结果，不回今天的版本**。当前版本可能已经更新，客户端拿它去追加
    ///    会得到 409 而不是静默覆盖——那是安全的一侧。
    pub fn append_delivery_link(
        &self,
        command: &AppendLinkCommand,
        now_ms: i64,
    ) -> Result<LinkOutcome, StoreError> {
        use crate::delivery_link::{COMMAND_APPEND_LINK, LinkError, LinkRef};

        command.validate()?;
        let targets = command.normalized_targets()?;
        let key = crate::delivery_idempotency::validate_key(&command.idempotency_key)?;
        let owned = command.payload_fields(&targets);
        let borrowed: Vec<(&str, &str)> = owned
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let fingerprint = crate::delivery_idempotency::payload_fingerprint(&borrowed)?;

        if let Replay::Replayed(receipt) = classify(
            find_receipt(self.conn, &command.company_id, key)?,
            COMMAND_APPEND_LINK,
            &fingerprint,
        )? {
            return LinkOutcome::replayed_from(&receipt.result_detail).map_err(|detail| {
                IdempotencyError::UnreadableReceipt {
                    key: key.to_string(),
                    detail,
                }
                .into()
            });
        }

        self.conn.execute_batch("SAVEPOINT append_delivery_link")?;
        let outcome = (|| -> Result<LinkOutcome, StoreError> {
            let (current, state, project_id) =
                self.delivery_tracking_gate(&command.company_id, &command.tracking_id)?;
            if state != TrackingState::Open {
                return Err(LinkError::TrackingClosed(command.tracking_id.clone()).into());
            }
            let current_version =
                u64::try_from(current).map_err(|_| DeliveryError::InvalidVersion(current))?;
            if current_version != command.expected_version {
                return Err(LinkError::VersionConflict {
                    expected: command.expected_version,
                    current: current_version,
                }
                .into());
            }

            let mut appended = false;
            match &command.link {
                LinkRef::Source(source_id) => {
                    let linked = self.link_delivery_tracking_source(
                        &command.company_id,
                        source_id,
                        &command.tracking_id,
                        &command.linked_by,
                        now_ms,
                    )?;
                    appended |= linked.created;
                }
                LinkRef::WorkPackage(revision) => {
                    appended |= self.append_work_package_link(
                        &command.company_id,
                        &command.tracking_id,
                        revision,
                        &command.linked_by,
                        now_ms,
                    )?;
                }
            }
            let mut created_targets = Vec::new();
            for target in &targets {
                if self.ensure_delivery_target(
                    &command.company_id,
                    &command.tracking_id,
                    target,
                    now_ms,
                )? {
                    created_targets.push(target.target_id.clone());
                    appended = true;
                }
            }
            let version = if appended {
                self.bump_delivery_version(
                    &command.company_id,
                    &command.tracking_id,
                    current,
                    now_ms,
                )?
            } else {
                current_version
            };

            let outcome = LinkOutcome {
                tracking_id: command.tracking_id.clone(),
                link_id: command.link_id(),
                appended,
                version,
                created_targets,
                replayed: false,
            };
            let detail = serde_json::to_string(&outcome).map_err(|err| {
                IdempotencyError::ResultNotRecordable(format!("结果体无法编码为 JSON：{err}"))
            })?;
            // 只在真有新增时记审计：`appended=false` 是幂等的空操作（关系已在、目标已在），
            // 记一条「追加了」会让审计回答「谁改了什么」时凭空多出一次改动。
            if appended {
                let evidence = match &command.link {
                    LinkRef::Source(_) => "source_link",
                    LinkRef::WorkPackage(_) => "work_package_link",
                };
                let event = crate::audit_log::AuditEvent::for_write(
                    command.company_id.as_str(),
                    command.correlation_id.as_str(),
                    crate::domain::DomainId::new(command.linked_by.as_str()),
                    AuditAction::AppendLink,
                    crate::domain::Scope {
                        company_id: crate::domain::DomainId::new(command.company_id.as_str()),
                        project_id: Some(crate::domain::DomainId::new(project_id.as_str())),
                        issue_id: None,
                    },
                    evidence,
                    command.decided_at.as_str(),
                )
                .map_err(StoreError::Audit)?;
                self.append_audit_event(&event, now_ms)?;
            }
            // 结果种类记 `tracking`：关系是跟踪项的属性，回执要回答的是「哪条跟踪项被改了」。
            // 新增第三种种类要重建表（`result_kind` 的 CHECK 不受 ALTER 幂等保护），
            // 而换命令与换载荷已经由 `command` 列分清。
            self.conn.execute(
                "INSERT INTO delivery_command_receipts (
                    company_id, idempotency_key, command, payload_fingerprint,
                    result_kind, result_id, result_detail, recorded_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    command.company_id,
                    key,
                    COMMAND_APPEND_LINK,
                    fingerprint,
                    ResultKind::Tracking.as_str(),
                    command.tracking_id,
                    detail,
                    now_ms,
                ],
            )?;
            Ok(outcome)
        })();
        match outcome {
            Ok(value) => {
                self.conn.execute_batch("RELEASE append_delivery_link")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO append_delivery_link; RELEASE append_delivery_link",
                );
                Err(err)
            }
        }
    }

    /// 推进跟踪项版本。带上 `resource_version = 当前值` 的条件：
    /// 读与写之间若有别的写入，这里会写 0 行，按版本冲突报出去而不是覆盖。
    fn bump_delivery_version(
        &self,
        company_id: &str,
        tracking_id: &str,
        current: i64,
        now_ms: i64,
    ) -> Result<u64, StoreError> {
        use crate::delivery_link::LinkError;

        let changed = self.conn.execute(
            "UPDATE delivery_tracking
             SET resource_version = resource_version + 1, updated_at_ms = ?3
             WHERE company_id = ?1 AND tracking_id = ?2 AND resource_version = ?4",
            params![company_id, tracking_id, now_ms, current],
        )?;
        if changed != 1 {
            let (latest, _, _) = self.delivery_tracking_gate(company_id, tracking_id)?;
            return Err(LinkError::VersionConflict {
                expected: u64::try_from(current)
                    .map_err(|_| DeliveryError::InvalidVersion(current))?,
                current: u64::try_from(latest)
                    .map_err(|_| DeliveryError::InvalidVersion(latest))?,
            }
            .into());
        }
        u64::try_from(current + 1).map_err(|_| DeliveryError::InvalidVersion(current + 1).into())
    }

    /// 确保一个客户目标存在。已存在返回 `false`，且**不改它的验收条件与开关**：
    /// 那是事实，不归一次关联改写。新建返回 `true`。
    ///
    /// 新建目标的开关与人群记为未知：没有证据就没有事实，不默认「已同步」。
    fn ensure_delivery_target(
        &self,
        company_id: &str,
        tracking_id: &str,
        target: &NormalizedTarget,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT target_id FROM delivery_targets
                 WHERE company_id = ?1 AND tracking_id = ?2 AND customer_id = ?3
                   AND environment = ?4 AND deployment_unit = ?5",
                params![
                    company_id,
                    tracking_id,
                    target.customer_id,
                    target.environment,
                    target.deployment_unit
                ],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Ok(false);
        }
        let inserted = self.conn.execute(
            "INSERT INTO delivery_targets (
                target_id, company_id, tracking_id, customer_id, environment, deployment_unit,
                acceptance, switch_state, population_known, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', 'unknown', 0, ?7, ?8)",
            params![
                target.target_id,
                company_id,
                tracking_id,
                target.customer_id,
                target.environment,
                target.deployment_unit,
                now_ms,
                now_ms,
            ],
        );
        match inserted {
            Ok(_) => Ok(true),
            // 并发下另一个写入者刚建成同一目标：按已有处理，不报读不懂的唯一约束错误。
            Err(err) if is_unique_violation(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// 追加内部工作包关联。同一修订重复提交返回 `false`，不新增也不报错。
    fn append_work_package_link(
        &self,
        company_id: &str,
        tracking_id: &str,
        work_package_revision: &str,
        linked_by: &str,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO delivery_tracking_work_packages (
                company_id, tracking_id, work_package_revision, linked_by, linked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                company_id,
                tracking_id,
                work_package_revision,
                linked_by,
                now_ms
            ],
        );
        match inserted {
            Ok(_) => Ok(true),
            Err(err) if is_unique_violation(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }
}
