//! 项目视图：问题 / 运行 / 调用 / 事件，以及跨表归属查询。
//!
//! 这里的读模型服务于上层聚合，不做业务判定；
//! 写路径的条件更新（`update_issue_conditional`）用版本号 CAS，冲突时返回当前版本。

use super::*;

impl SqliteStore {
    /// 统计运行实例行数，语义同 [`Self::count_work_package_revisions`]。
    pub fn count_runs(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 读取某聚合最新事件载荷；取消对账只从宿主事件推导状态，不另建状态库。
    pub fn latest_event_payload(&self, aggregate_id: &str) -> Result<Option<String>, StoreError> {
        self.conn.query_row(
            "SELECT payload FROM events WHERE aggregate_id = ?1 ORDER BY aggregate_version DESC LIMIT 1",
            params![aggregate_id],
            |row| row.get(0),
        ).optional().map_err(StoreError::Sql)
    }

    /// 统计某聚合的事件数，供事件版本单调递增。
    pub fn count_events(&self, aggregate_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE aggregate_id = ?1",
                params![aggregate_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 列出项目下的 Issue（工作台投影只读来源，不改状态、不派发）。
    pub fn list_issues_for_project(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<Issue>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, company_id, project_id, kind, repo_provider, repo_id, title, acceptance,
                    owner_id, owner_kind, assignment_grant_revision, resource_version,
                    generation, observed_generation, phase
             FROM issues WHERE company_id = ?1 AND project_id = ?2 ORDER BY uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            let uid: String = row.get(0)?;
            let company_id = DomainId::new(row.get::<_, String>(1)?);
            let project_id = row.get::<_, Option<String>>(2)?.map(DomainId::new);
            let kind = match row.get::<_, String>(3)?.as_str() {
                "Coding" => IssueKind::Coding,
                _ => IssueKind::General,
            };
            let provider: Option<String> = row.get(4)?;
            let repository_id: Option<String> = row.get(5)?;
            let repository = match (provider, repository_id) {
                (Some(provider), Some(repository_id)) => Some(RepositoryContext {
                    provider,
                    repository_id,
                }),
                _ => None,
            };
            let acceptance: String = row.get(7)?;
            let owner_id: Option<String> = row.get(8)?;
            let owner_kind: Option<String> = row.get(9)?;
            let grant_revision: Option<i64> = row.get(10)?;
            let assignment = match (owner_id, owner_kind) {
                (Some(owner_id), Some(owner_kind)) => Some(Assignment {
                    owner: Owner {
                        kind: if owner_kind == "User" {
                            OwnerKind::User
                        } else {
                            OwnerKind::Agent
                        },
                        id: DomainId::new(owner_id),
                    },
                    scope: Scope {
                        company_id: company_id.clone(),
                        project_id: project_id.clone(),
                        issue_id: Some(DomainId::new(uid.clone())),
                    },
                    grant_revision: grant_revision.unwrap_or(0) as u64,
                }),
                _ => None,
            };
            let phase = match row.get::<_, String>(14)?.as_str() {
                "Running" => Phase::Running,
                "Blocked" => Phase::Blocked,
                "Succeeded" => Phase::Succeeded,
                "Cancelled" => Phase::Cancelled,
                _ => Phase::Pending,
            };
            Ok(Issue {
                meta: ResourceMeta {
                    uid: DomainId::new(uid),
                    company_id,
                    project_id,
                    resource_version: Version(row.get::<_, i64>(11)? as u64),
                    generation: row.get::<_, i64>(12)? as u64,
                    deletion_requested_at: None,
                },
                kind,
                repository,
                spec: IssueSpec {
                    title: row.get(6)?,
                    assignment,
                    acceptance: AcceptanceCriteria {
                        criteria: split_acceptance(&acceptance),
                    },
                },
                status: ResourceStatus {
                    phase,
                    observed_generation: row.get::<_, i64>(13)? as u64,
                    conditions: Vec::new(),
                },
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目下仍在运行的 Issue 数（清理阻塞判定）。
    pub fn count_running_issues(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM issues
                 WHERE company_id = ?1 AND project_id = ?2
                   AND phase IN ('Running', 'Blocked')",
                params![company_id, project_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 项目运行 ID 列表：预算与执行事实按 Run 归属，需先落到项目范围。
    pub fn list_project_run_ids(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT runs.uid FROM runs JOIN issues ON issues.uid = runs.issue_uid
             WHERE runs.company_id = ?1 AND issues.project_id = ?2 ORDER BY runs.uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内 Run → Issue 归属（只读）：负责人切换的连续性投影按 Run 找执行者。
    ///
    /// 返回 `(run_id, issue_uid)` 并按 run_id 排序；执行者仍从 Issue 的 Assignment 读，
    /// 不从 adapter 或调用方自述推导。
    pub fn list_project_run_issues(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT runs.uid, runs.issue_uid FROM runs
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE runs.company_id = ?1 AND issues.project_id = ?2 ORDER BY runs.uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目范围内全部 invocation 状态（停止是否已确认的事实来源）。
    pub fn list_project_invocation_states(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT invocations.state
             FROM invocations
             JOIN runs ON runs.uid = invocations.run_id
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE invocations.company_id = ?1 AND issues.project_id = ?2
             ORDER BY invocations.invocation_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内 Run 的期望状态（只读）：检查点指纹与「在跑/已终结」判定的事实来源。
    ///
    /// 返回 `(run_id, issue_uid, desired_state)`，按 run_id 排序；不读执行内容。
    pub fn list_project_runs(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT runs.uid, runs.issue_uid, runs.desired_state
             FROM runs
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE runs.company_id = ?1 AND issues.project_id = ?2 ORDER BY runs.uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内 invocation 的标识与状态（只读）：检查点要能区分「哪一条执行变了」。
    ///
    /// 返回 `InvocationFactRow`（id / run / state / 最后观察时间 / fencing token）。
    pub fn list_project_invocations(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<InvocationFactRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT invocations.invocation_id, invocations.run_id, invocations.state,
                    invocations.last_observed_at_ms, invocations.fencing_token
             FROM invocations
             JOIN runs ON runs.uid = invocations.run_id
             JOIN issues ON issues.uid = runs.issue_uid
             WHERE invocations.company_id = ?1 AND issues.project_id = ?2
             ORDER BY invocations.invocation_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(InvocationFactRow {
                invocation_id: row.get(0)?,
                run_id: row.get(1)?,
                state: row.get(2)?,
                last_observed_at_ms: row.get(3)?,
                fencing_token: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 单个 Run 的 invocation 状态。
    pub fn list_run_invocation_states(
        &self,
        company_id: &str,
        run_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT state FROM invocations
             WHERE company_id = ?1 AND run_id = ?2 ORDER BY invocation_id",
        )?;
        let rows = stmt.query_map(params![company_id, run_id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 目标归属反查（只读）：Issue → project_id。跨 Company 视为不存在。
    pub fn issue_project_id(
        &self,
        company_id: &str,
        issue_uid: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT project_id FROM issues WHERE company_id = ?1 AND uid = ?2",
                params![company_id, issue_uid],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(StoreError::Sql)
    }

    /// 目标归属反查（只读）：Artifact → project_id。跨 Company 视为不存在。
    pub fn artifact_project_id(
        &self,
        company_id: &str,
        artifact_id: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT project_id FROM artifacts WHERE company_id = ?1 AND uid = ?2",
                params![company_id, artifact_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// Issue 的删除请求标记（毫秒字符串）；`None` 表示未请求删除。
    pub fn issue_deletion_requested_at(
        &self,
        company_id: &str,
        issue_uid: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT deletion_requested_at FROM issues WHERE company_id = ?1 AND uid = ?2",
                params![company_id, issue_uid],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(StoreError::Sql)
    }

    /// Run 是否存在：Run 级清理不按项目聚合，但目标必须可解析。
    ///
    /// 只要 `runs` 行或该 run_id 的 invocation 存在即成立——P0 切片尚无 runs 写入者，
    /// 仅按 `runs` 判定会把真实存在的 Run 判成不存在。
    pub fn run_exists(&self, company_id: &str, run_id: &str) -> Result<bool, StoreError> {
        self.conn
            .query_row(
                "SELECT 1 FROM runs WHERE company_id = ?1 AND uid = ?2
                 UNION ALL
                 SELECT 1 FROM invocations WHERE company_id = ?1 AND run_id = ?2
                 LIMIT 1",
                params![company_id, run_id],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .map_err(StoreError::Sql)
    }

    /// 无法归属到 Run 的 invocation 数：缺少 `runs` 行时停止事实不可判定，
    /// 必须计入 Unknown，而不是当作"没有在途执行"。
    pub fn count_unattributed_invocations(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM invocations
                 WHERE company_id = ?1 AND run_id NOT IN (
                     SELECT uid FROM runs WHERE company_id = ?1
                 )",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }
}

impl<'a> UnitOfWork<'a> {
    /// 首次使用路径自动补齐最小运行实例：无需预先配置 Run/执行图。
    pub fn insert_run(
        &self,
        uid: &str,
        company_id: &str,
        issue_uid: &str,
        plan_revision: &str,
        desired_state: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO runs (uid, company_id, issue_uid, plan_revision, desired_state)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![uid, company_id, issue_uid, plan_revision, desired_state],
        )?;
        Ok(())
    }

    /// 统计运行实例行数，语义同 [`Self::count_work_package_revisions`]。
    pub fn count_runs(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    pub fn insert_issue(&self, issue: &Issue) -> Result<(), StoreError> {
        let (repo_provider, repo_id) = match &issue.repository {
            Some(repo) => (
                Some(repo.provider.as_str()),
                Some(repo.repository_id.as_str()),
            ),
            None => (None, None),
        };
        let (owner_id, owner_kind, grant_revision) = match &issue.spec.assignment {
            Some(assignment) => (
                Some(assignment.owner.id.as_str()),
                Some(owner_kind_str(assignment.owner.kind)),
                Some(assignment.grant_revision as i64),
            ),
            None => (None, None, None),
        };
        self.conn.execute(
            "INSERT INTO issues (
                uid, company_id, project_id, kind, repo_provider, repo_id, title, acceptance,
                owner_id, owner_kind, assignment_grant_revision, resource_version,
                generation, observed_generation, phase, condition_reason, deletion_requested_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                issue.meta.uid.as_str(),
                issue.meta.company_id.as_str(),
                issue.meta.project_id.as_ref().map(DomainId::as_str),
                issue_kind_str(issue.kind),
                repo_provider,
                repo_id,
                issue.spec.title,
                acceptance_text(&issue.spec.acceptance),
                owner_id,
                owner_kind,
                grant_revision,
                issue.meta.resource_version.0 as i64,
                issue.meta.generation as i64,
                issue.status.observed_generation as i64,
                phase_str(issue.status.phase),
                issue
                    .status
                    .conditions
                    .last()
                    .map(|condition| format!("{:?}", condition.reason)),
                issue.meta.deletion_requested_at,
            ],
        )?;
        Ok(())
    }

    /// 读取 Issue 的完整投影；只读路径不得因此触发任何派发。
    pub fn get_issue(&self, uid: &str) -> Result<Option<Issue>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, project_id, kind, repo_provider, repo_id, title, acceptance,
                        owner_id, owner_kind, assignment_grant_revision, resource_version,
                        generation, observed_generation, phase
                 FROM issues WHERE uid = ?1",
                params![uid],
                |row| {
                    let company_id = DomainId::new(row.get::<_, String>(0)?);
                    let project_id = row.get::<_, Option<String>>(1)?.map(DomainId::new);
                    let kind = match row.get::<_, String>(2)?.as_str() {
                        "Coding" => IssueKind::Coding,
                        _ => IssueKind::General,
                    };
                    let provider: Option<String> = row.get(3)?;
                    let repository_id: Option<String> = row.get(4)?;
                    let repository = match (provider, repository_id) {
                        (Some(provider), Some(repository_id)) => Some(RepositoryContext {
                            provider,
                            repository_id,
                        }),
                        _ => None,
                    };
                    let acceptance: String = row.get(6)?;
                    let owner_id: Option<String> = row.get(7)?;
                    let owner_kind: Option<String> = row.get(8)?;
                    let grant_revision: Option<i64> = row.get(9)?;
                    let assignment = match (owner_id, owner_kind) {
                        (Some(owner_id), Some(owner_kind)) => Some(Assignment {
                            owner: Owner {
                                kind: if owner_kind == "User" {
                                    OwnerKind::User
                                } else {
                                    OwnerKind::Agent
                                },
                                id: DomainId::new(owner_id),
                            },
                            scope: Scope {
                                company_id: company_id.clone(),
                                project_id: project_id.clone(),
                                issue_id: Some(DomainId::new(uid.to_string())),
                            },
                            grant_revision: grant_revision.unwrap_or(0) as u64,
                        }),
                        _ => None,
                    };
                    let phase = match row.get::<_, String>(13)?.as_str() {
                        "Running" => Phase::Running,
                        "Blocked" => Phase::Blocked,
                        "Succeeded" => Phase::Succeeded,
                        "Cancelled" => Phase::Cancelled,
                        _ => Phase::Pending,
                    };
                    Ok(Issue {
                        meta: ResourceMeta {
                            uid: DomainId::new(uid.to_string()),
                            company_id,
                            project_id,
                            resource_version: Version(row.get::<_, i64>(10)? as u64),
                            generation: row.get::<_, i64>(11)? as u64,
                            deletion_requested_at: None,
                        },
                        kind,
                        repository,
                        spec: IssueSpec {
                            title: row.get(5)?,
                            assignment,
                            acceptance: AcceptanceCriteria {
                                criteria: split_acceptance(&acceptance),
                            },
                        },
                        status: ResourceStatus {
                            phase,
                            observed_generation: row.get::<_, i64>(12)? as u64,
                            conditions: Vec::new(),
                        },
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 条件更新：只有 resource_version 匹配才写入；返回当前版本以便调用方重试。
    pub fn update_issue_conditional(
        &self,
        issue: &Issue,
        expected: Version,
    ) -> Result<(), StoreError> {
        let (owner_id, owner_kind, grant_revision) = match &issue.spec.assignment {
            Some(assignment) => (
                Some(assignment.owner.id.as_str()),
                Some(owner_kind_str(assignment.owner.kind)),
                Some(assignment.grant_revision as i64),
            ),
            None => (None, None, None),
        };
        let rows = self.conn.execute(
            "UPDATE issues SET
                title = ?2, acceptance = ?3, owner_id = ?4, owner_kind = ?5,
                assignment_grant_revision = ?6, resource_version = ?7, generation = ?8,
                observed_generation = ?9, phase = ?10, condition_reason = ?11
             WHERE uid = ?1 AND resource_version = ?12",
            params![
                issue.meta.uid.as_str(),
                issue.spec.title,
                acceptance_text(&issue.spec.acceptance),
                owner_id,
                owner_kind,
                grant_revision,
                issue.meta.resource_version.0 as i64,
                issue.meta.generation as i64,
                issue.status.observed_generation as i64,
                phase_str(issue.status.phase),
                issue
                    .status
                    .conditions
                    .last()
                    .map(|condition| format!("{:?}", condition.reason)),
                expected.0 as i64,
            ],
        )?;
        if rows == 1 {
            Ok(())
        } else {
            let current = self
                .conn
                .query_row(
                    "SELECT resource_version FROM issues WHERE uid = ?1",
                    params![issue.meta.uid.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0);
            Err(StoreError::VersionConflict {
                current: Version(current.max(0) as u64),
            })
        }
    }

    pub fn insert_event(
        &self,
        event_id: &str,
        company_id: &str,
        aggregate_id: &str,
        aggregate_version: u64,
        payload: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO events (event_id, company_id, aggregate_id, aggregate_version, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event_id,
                company_id,
                aggregate_id,
                aggregate_version as i64,
                payload
            ],
        )?;
        Ok(())
    }

    pub fn insert_grant(&self, grant: &Grant) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO grants (principal_id, company_id, project_id, permission, revision, active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                grant.principal_id.as_str(),
                grant.scope.company_id.as_str(),
                grant.scope.project_id.as_ref().map(DomainId::as_str),
                permission_str(grant.permission),
                grant.revision as i64,
                i64::from(grant.active),
            ],
        )?;
        Ok(())
    }
}
