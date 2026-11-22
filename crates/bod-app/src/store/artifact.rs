//! D5 成果：制品 / 修订 / 清单 / 验证记录 / 豁免 / 验收 / 决定 / 工作包状态。
//!
//! 修订与验收都是追加式：`supersede_acceptances` 把旧验收标记取代而不是删除，
//! 因为「当时谁按什么条件接受了什么」是审计事实。

use super::*;

impl SqliteStore {
    /// 统计工作包修订行数（首次使用断言「从零自动补齐」）。
    pub fn count_work_package_revisions(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM work_package_revisions WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    /// 冻结一组具体修订（§21.4）。本切片只写入，不改写已有 Manifest 修订。
    // 参数多但都是 Manifest 的必填事实；拆结构体会掩盖"哪些字段来自接受记录"。
    #[allow(clippy::too_many_arguments)]
    pub fn put_manifest(
        &self,
        company_id: &str,
        manifest_id: &str,
        project_id: &str,
        revision: i64,
        artifact_revision_refs: &[String],
        acceptance_refs: &[String],
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO manifests (
                uid, company_id, project_id, revision, milestone,
                artifact_revision_refs, acceptance_refs, signature_ref, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, NULL, ?7)",
            params![
                manifest_id,
                company_id,
                project_id,
                revision,
                artifact_revision_refs.join(ARRAY_SEP),
                acceptance_refs.join(ARRAY_SEP),
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// 引用该修订的正式 Manifest：用分隔符包裹后精确匹配，避免子串误判。
    pub fn manifest_referencing(&self, artifact_revision: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid FROM manifests
             WHERE instr(?2 || artifact_revision_refs || ?2, ?2 || ?1 || ?2) > 0",
        )?;
        let rows = stmt.query_map(params![artifact_revision, ARRAY_SEP], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出公司下每个成果修订的最新验证记录（任务页证据视图只读来源）。
    pub fn list_verifications_for_company(
        &self,
        company_id: &str,
    ) -> Result<Vec<VerificationRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT v.revision_id, v.state, v.actor_id, v.reason, v.evidence_refs,
                    v.verification_rules, v.environment, v.observed_at_ms
             FROM verification_records v
             WHERE v.company_id = ?1
               AND v.observed_at_ms = (
                   SELECT MAX(v2.observed_at_ms) FROM verification_records v2
                   WHERE v2.revision_id = v.revision_id
               )
             ORDER BY v.revision_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let refs: String = row.get(4)?;
            Ok(VerificationRecord {
                revision_id: DomainId::new(row.get::<_, String>(0)?),
                state: VerificationState::parse(&row.get::<_, String>(1)?)
                    .unwrap_or(VerificationState::Pending),
                actor: DomainId::new(row.get::<_, String>(2)?),
                reason: row.get(3)?,
                evidence_refs: split_list(&refs),
                verification_rules: DomainId::new(row.get::<_, String>(5)?),
                environment: DomainId::new(row.get::<_, String>(6)?),
                observed_at_ms: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出公司下「修订 → 成果」的只读关联（任务页证据归属来源）。
    pub fn list_revision_artifact_links(
        &self,
        company_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revision_id, artifact_id FROM artifact_revisions
             WHERE company_id = ?1 ORDER BY revision_id",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 列出公司下的接受记录（工作台投影只读来源，不改状态、不派发）。
    pub fn list_acceptances_for_company(
        &self,
        company_id: &str,
    ) -> Result<Vec<AcceptanceRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT acceptance_id, artifact_id, work_package_revision, input_baseline,
                    artifact_revision, verification_rules, target_environment, merge_combination,
                    decision, actor_id, reason, evidence_refs, accepted_at_ms, superseded_by
             FROM acceptances WHERE company_id = ?1
             ORDER BY accepted_at_ms ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            let decision = AcceptanceDecision::parse(&row.get::<_, String>(8)?)
                .unwrap_or(AcceptanceDecision::RequiresHuman);
            let evidence: String = row.get(11)?;
            Ok(AcceptanceRecord {
                acceptance_id: DomainId::new(row.get::<_, String>(0)?),
                artifact_id: DomainId::new(row.get::<_, String>(1)?),
                binding: AcceptanceBinding {
                    work_package_revision: DomainId::new(row.get::<_, String>(2)?),
                    input_baseline: DomainId::new(row.get::<_, String>(3)?),
                    artifact_revision: DomainId::new(row.get::<_, String>(4)?),
                    verification_rules: DomainId::new(row.get::<_, String>(5)?),
                    target_environment: DomainId::new(row.get::<_, String>(6)?),
                    merge_combination: row.get::<_, Option<String>>(7)?.map(DomainId::new),
                },
                decision,
                actor: DomainId::new(row.get::<_, String>(9)?),
                reason: row.get(10)?,
                evidence_refs: split_list(&evidence),
                accepted_at_ms: row.get::<_, i64>(12)?,
                superseded_by: row.get::<_, Option<String>>(13)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目范围内的历史接受（只读）：经成果归属落到项目，供换人前后逐字段比对。
    ///
    /// 与 `list_acceptances_for_company` 的差别只是作用域；不做字段裁剪。
    pub fn list_project_acceptances(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<AcceptanceRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT acceptances.acceptance_id, acceptances.artifact_id,
                    acceptances.work_package_revision, acceptances.input_baseline,
                    acceptances.artifact_revision, acceptances.verification_rules,
                    acceptances.target_environment, acceptances.merge_combination,
                    acceptances.decision, acceptances.actor_id, acceptances.reason,
                    acceptances.evidence_refs, acceptances.accepted_at_ms, acceptances.superseded_by
             FROM acceptances
             JOIN artifacts ON artifacts.uid = acceptances.artifact_id
             WHERE acceptances.company_id = ?1 AND artifacts.project_id = ?2
             ORDER BY acceptances.accepted_at_ms ASC, acceptances.rowid ASC",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            let decision = AcceptanceDecision::parse(&row.get::<_, String>(8)?)
                .unwrap_or(AcceptanceDecision::RequiresHuman);
            let evidence: String = row.get(11)?;
            Ok(AcceptanceRecord {
                acceptance_id: DomainId::new(row.get::<_, String>(0)?),
                artifact_id: DomainId::new(row.get::<_, String>(1)?),
                binding: crate::domain::artifact::AcceptanceBinding {
                    work_package_revision: DomainId::new(row.get::<_, String>(2)?),
                    input_baseline: DomainId::new(row.get::<_, String>(3)?),
                    artifact_revision: DomainId::new(row.get::<_, String>(4)?),
                    verification_rules: DomainId::new(row.get::<_, String>(5)?),
                    target_environment: DomainId::new(row.get::<_, String>(6)?),
                    merge_combination: row.get::<_, Option<String>>(7)?.map(DomainId::new),
                },
                decision,
                actor: DomainId::new(row.get::<_, String>(9)?),
                reason: row.get(10)?,
                evidence_refs: split_list(&evidence),
                accepted_at_ms: row.get::<_, i64>(12)?,
                superseded_by: row.get::<_, Option<String>>(13)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内修订正文事实（只读）：摘要/大小/mime 进指纹，便于发现「换了一条内容」。
    pub fn list_project_revision_bodies(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<RevisionBodyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revisions.revision_id, revisions.artifact_id, revisions.produced_by,
                    revisions.digest, revisions.size_bytes, revisions.mime, revisions.created_at_ms
             FROM artifact_revisions AS revisions
             JOIN artifacts ON artifacts.uid = revisions.artifact_id
             WHERE revisions.company_id = ?1 AND artifacts.project_id = ?2
             ORDER BY revisions.revision_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(RevisionBodyRow {
                revision_id: row.get(0)?,
                artifact_id: row.get(1)?,
                produced_by: row.get(2)?,
                digest: row.get(3)?,
                size_bytes: row.get(4)?,
                mime: row.get(5)?,
                created_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内正式 Manifest 事实（只读）。冻结的是引用集合，不是正文。
    pub fn list_project_manifests(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<ManifestBodyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, revision, milestone, artifact_revision_refs, acceptance_refs,
                    signature_ref, created_at_ms
             FROM manifests WHERE company_id = ?1 AND project_id = ?2 ORDER BY uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(ManifestBodyRow {
                uid: row.get(0)?,
                revision: row.get(1)?,
                milestone: row.get(2)?,
                artifact_revision_refs: row.get(3)?,
                acceptance_refs: row.get(4)?,
                signature_ref: row.get(5)?,
                created_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目作用域内的决定（只读）：旧经理的决定不因换人消失或改口。
    pub fn list_project_decisions(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<DecisionBodyRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT decision_id, actor_id, decision, reason, evidence_refs, revision_refs,
                    created_at_ms
             FROM decisions WHERE company_id = ?1 AND scope_project = ?2
             ORDER BY created_at_ms, decision_id",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(DecisionBodyRow {
                decision_id: row.get(0)?,
                actor_id: row.get(1)?,
                decision: row.get(2)?,
                reason: row.get(3)?,
                evidence_refs: row.get(4)?,
                revision_refs: row.get(5)?,
                created_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目下的成果身份（用于列出必须保留的外部引用）。
    pub fn list_artifacts(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<Artifact>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, kind, title, owner_id, visibility, accepted_revision
             FROM artifacts WHERE company_id = ?1 AND project_id = ?2 ORDER BY uid",
        )?;
        let rows = stmt.query_map(params![company_id, project_id], |row| {
            Ok(Artifact {
                id: DomainId::new(row.get::<_, String>(0)?),
                company_id: DomainId::new(company_id.to_string()),
                project_id: DomainId::new(project_id.to_string()),
                kind: ArtifactKind::parse(&row.get::<_, String>(1)?),
                title: row.get(2)?,
                owner: DomainId::new(row.get::<_, String>(3)?),
                visibility: ArtifactVisibility::parse(&row.get::<_, String>(4)?),
                accepted_revision: row.get::<_, Option<String>>(5)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 项目内修订的产出者（只读）：负责人切换要判断某项成果由谁以执行身份产出。
    ///
    /// 返回 `(revision_id, artifact_id, produced_by)`，按 revision_id 排序；不读内容。
    pub fn list_project_revision_producers(
        &self,
        company_id: &str,
        project_id: &str,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revisions.revision_id, revisions.artifact_id, revisions.produced_by
             FROM artifact_revisions AS revisions
             JOIN artifacts ON artifacts.uid = revisions.artifact_id
             WHERE revisions.company_id = ?1 AND artifacts.project_id = ?2
             ORDER BY revisions.revision_id",
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
}

impl<'a> UnitOfWork<'a> {
    /// 首次使用路径自动补齐最小工作包修订：无需预先配置 WorkPackage。
    pub fn insert_work_package_revision(
        &self,
        revision_id: &str,
        company_id: &str,
        issue_uid: &str,
        spec_revision: &str,
        acceptance_revision: &str,
        created_at: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO work_package_revisions
             (revision_id, company_id, issue_uid, spec_revision, acceptance_revision, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                revision_id,
                company_id,
                issue_uid,
                spec_revision,
                acceptance_revision,
                created_at
            ],
        )?;
        Ok(())
    }

    /// 统计工作包修订行数；首次使用断言"从零自动补齐"，不是复用预置配置。
    pub fn count_work_package_revisions(&self, company_id: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM work_package_revisions WHERE company_id = ?1",
                params![company_id],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }

    pub fn insert_artifact(&self, artifact: &Artifact, now_ms: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO artifacts (
                uid, company_id, project_id, kind, title, owner_id, visibility,
                accepted_revision, resource_version, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                artifact.id.as_str(),
                artifact.company_id.as_str(),
                artifact.project_id.as_str(),
                artifact.kind.as_str(),
                artifact.title,
                artifact.owner.as_str(),
                artifact.visibility.as_str(),
                artifact.accepted_revision.as_ref().map(DomainId::as_str),
                1_i64,
                now_ms,
            ],
        )?;
        Ok(())
    }

    pub fn get_artifact(&self, artifact_id: &str) -> Result<Option<Artifact>, StoreError> {
        self.conn
            .query_row(
                "SELECT company_id, project_id, kind, title, owner_id, visibility, accepted_revision
                 FROM artifacts WHERE uid = ?1",
                params![artifact_id],
                |row| {
                    Ok(Artifact {
                        id: DomainId::new(artifact_id.to_string()),
                        company_id: DomainId::new(row.get::<_, String>(0)?),
                        project_id: DomainId::new(row.get::<_, String>(1)?),
                        kind: ArtifactKind::parse(&row.get::<_, String>(2)?),
                        title: row.get(3)?,
                        owner: DomainId::new(row.get::<_, String>(4)?),
                        visibility: ArtifactVisibility::parse(&row.get::<_, String>(5)?),
                        accepted_revision: row.get::<_, Option<String>>(6)?.map(DomainId::new),
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    /// 接受指针的 compare-and-swap：只有当前指针仍等于 `expected` 才写入。
    /// 返回 false 表示并发接受已发生，调用方必须要求重新读取而不是覆盖。
    pub fn cas_accepted_revision(
        &self,
        artifact_id: &str,
        expected: Option<&str>,
        next_revision: &str,
    ) -> Result<bool, StoreError> {
        let rows = match expected {
            Some(expected) => self.conn.execute(
                "UPDATE artifacts SET accepted_revision = ?1, resource_version = resource_version + 1
                 WHERE uid = ?2 AND accepted_revision = ?3",
                params![next_revision, artifact_id, expected],
            )?,
            None => self.conn.execute(
                "UPDATE artifacts SET accepted_revision = ?1, resource_version = resource_version + 1
                 WHERE uid = ?2 AND accepted_revision IS NULL",
                params![next_revision, artifact_id],
            )?,
        };
        Ok(rows == 1)
    }

    pub fn insert_artifact_revision(
        &self,
        company_id: &str,
        revision: &ArtifactRevision,
    ) -> Result<(), StoreError> {
        let (storage_kind, storage_ref) = match &revision.storage {
            StorageRef::Cas(value) => ("cas", value.clone()),
            StorageRef::External(value) => ("external", value.clone()),
        };
        self.conn.execute(
            "INSERT INTO artifact_revisions (
                revision_id, company_id, artifact_id, base_revision, storage_kind, storage_ref,
                digest_algorithm, digest, size_bytes, mime, produced_by, source_note, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                revision.revision_id.as_str(),
                company_id,
                revision.artifact_id.as_str(),
                revision.base_revision.as_ref().map(DomainId::as_str),
                storage_kind,
                storage_ref,
                revision.digest.algorithm.as_str(),
                revision.digest.value,
                revision.size_bytes,
                revision.mime,
                revision.produced_by.as_str(),
                revision.source_note,
                revision.created_at_ms,
            ],
        )?;
        for dependency in &revision.depends_on {
            self.conn.execute(
                "INSERT INTO artifact_dependencies (revision_id, depends_on_revision, kind)
                 VALUES (?1, ?2, ?3)",
                params![
                    revision.revision_id.as_str(),
                    dependency.revision_id.as_str(),
                    dependency.kind.as_str(),
                ],
            )?;
        }
        Ok(())
    }

    pub fn get_artifact_revision(
        &self,
        revision_id: &str,
    ) -> Result<Option<ArtifactRevision>, StoreError> {
        let mut revision = self
            .conn
            .query_row(
                "SELECT artifact_id, base_revision, storage_kind, storage_ref,
                        digest_algorithm, digest, size_bytes, mime, produced_by, source_note,
                        created_at_ms
                 FROM artifact_revisions WHERE revision_id = ?1",
                params![revision_id],
                |row| {
                    let storage_kind: String = row.get(2)?;
                    let storage_ref: String = row.get(3)?;
                    Ok(ArtifactRevision {
                        revision_id: DomainId::new(revision_id.to_string()),
                        artifact_id: DomainId::new(row.get::<_, String>(0)?),
                        base_revision: row.get::<_, Option<String>>(1)?.map(DomainId::new),
                        storage: if storage_kind == "external" {
                            StorageRef::External(storage_ref)
                        } else {
                            StorageRef::Cas(storage_ref)
                        },
                        digest: Digest {
                            algorithm: DigestAlgorithm::parse(&row.get::<_, String>(4)?)
                                .unwrap_or(DigestAlgorithm::Sha256),
                            value: row.get(5)?,
                        },
                        size_bytes: row.get(6)?,
                        mime: row.get(7)?,
                        produced_by: DomainId::new(row.get::<_, String>(8)?),
                        source_note: row.get(9)?,
                        depends_on: Vec::new(),
                        created_at_ms: row.get(10)?,
                    })
                },
            )
            .optional()?;

        if let Some(found) = revision.as_mut() {
            let mut stmt = self.conn.prepare(
                "SELECT depends_on_revision, kind FROM artifact_dependencies WHERE revision_id = ?1",
            )?;
            let rows = stmt.query_map(params![revision_id], |row| {
                Ok(RevisionDependency {
                    revision_id: DomainId::new(row.get::<_, String>(0)?),
                    kind: DependencyKind::parse(&row.get::<_, String>(1)?),
                })
            })?;
            found.depends_on = rows.collect::<Result<Vec<_>, _>>()?;
        }
        Ok(revision)
    }

    /// 供 GC 使用的引用集合：本地 CAS digest 与外部引用计数分开统计。
    pub fn referenced_storage(&self, company_id: &str) -> Result<(Vec<String>, usize), StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT storage_kind, storage_ref FROM artifact_revisions WHERE company_id = ?1",
        )?;
        let rows = stmt.query_map(params![company_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut cas = Vec::new();
        let mut external = 0usize;
        for row in rows {
            let (kind, value) = row?;
            if kind == "cas" {
                cas.push(value);
            } else {
                external += 1;
            }
        }
        Ok((cas, external))
    }

    pub fn insert_verification_record(
        &self,
        record_id: &str,
        company_id: &str,
        record: &VerificationRecord,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO verification_records (
                record_id, company_id, revision_id, state, actor_id, reason, evidence_refs,
                verification_rules, environment, observed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record_id,
                company_id,
                record.revision_id.as_str(),
                record.state.as_str(),
                record.actor.as_str(),
                record.reason,
                record.evidence_refs.join(ARRAY_SEP),
                record.verification_rules.as_str(),
                record.environment.as_str(),
                record.observed_at_ms,
            ],
        )?;
        Ok(())
    }

    /// 最新一条验证状态；按写入顺序取最后一条。
    pub fn latest_verification(
        &self,
        revision_id: &str,
    ) -> Result<Option<VerificationRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT state, actor_id, reason, evidence_refs, verification_rules, environment,
                        observed_at_ms
                 FROM verification_records WHERE revision_id = ?1
                 ORDER BY observed_at_ms DESC, rowid DESC LIMIT 1",
                params![revision_id],
                |row| {
                    let state = VerificationState::parse(&row.get::<_, String>(0)?)
                        .unwrap_or(VerificationState::Pending);
                    let refs: String = row.get(3)?;
                    Ok(VerificationRecord {
                        revision_id: DomainId::new(revision_id.to_string()),
                        state,
                        actor: DomainId::new(row.get::<_, String>(1)?),
                        reason: row.get(2)?,
                        evidence_refs: split_list(&refs),
                        verification_rules: DomainId::new(row.get::<_, String>(4)?),
                        environment: DomainId::new(row.get::<_, String>(5)?),
                        observed_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub fn insert_waiver(
        &self,
        waiver_id: &str,
        company_id: &str,
        waiver: &Waiver,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO waivers (
                waiver_id, company_id, revision_id, actor_id, reason,
                scope_company, scope_project, scope_issue, granted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                waiver_id,
                company_id,
                waiver.applies_to_revision.as_str(),
                waiver.actor.as_str(),
                waiver.reason,
                waiver.scope.company_id.as_str(),
                waiver.scope.project_id.as_ref().map(DomainId::as_str),
                waiver.scope.issue_id.as_ref().map(DomainId::as_str),
                waiver.granted_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn insert_acceptance(
        &self,
        company_id: &str,
        record: &AcceptanceRecord,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO acceptances (
                acceptance_id, company_id, artifact_id, work_package_revision, input_baseline,
                artifact_revision, verification_rules, target_environment, merge_combination,
                decision, actor_id, reason, evidence_refs, accepted_at_ms, superseded_by
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                record.acceptance_id.as_str(),
                company_id,
                record.artifact_id.as_str(),
                record.binding.work_package_revision.as_str(),
                record.binding.input_baseline.as_str(),
                record.binding.artifact_revision.as_str(),
                record.binding.verification_rules.as_str(),
                record.binding.target_environment.as_str(),
                record
                    .binding
                    .merge_combination
                    .as_ref()
                    .map(DomainId::as_str),
                record.decision.as_str(),
                record.actor.as_str(),
                record.reason,
                record.evidence_refs.join(ARRAY_SEP),
                record.accepted_at_ms,
                record.superseded_by.as_ref().map(DomainId::as_str),
            ],
        )?;
        Ok(())
    }

    /// 新接受生效时把同一 Artifact 的**其他**旧接受标记为被取代（保留历史，不删除）。
    pub fn supersede_acceptances(
        &self,
        artifact_id: &str,
        superseded_by: &str,
    ) -> Result<usize, StoreError> {
        let rows = self.conn.execute(
            "UPDATE acceptances SET superseded_by = ?1
             WHERE artifact_id = ?2 AND superseded_by IS NULL AND acceptance_id <> ?1",
            params![superseded_by, artifact_id],
        )?;
        Ok(rows)
    }

    pub fn list_acceptances(&self, artifact_id: &str) -> Result<Vec<AcceptanceRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT acceptance_id, work_package_revision, input_baseline, artifact_revision,
                    verification_rules, target_environment, merge_combination, decision,
                    actor_id, reason, evidence_refs, accepted_at_ms, superseded_by
             FROM acceptances WHERE artifact_id = ?1 ORDER BY accepted_at_ms ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![artifact_id], |row| {
            let decision = AcceptanceDecision::parse(&row.get::<_, String>(7)?)
                .unwrap_or(AcceptanceDecision::RequiresHuman);
            let evidence: String = row.get(10)?;
            Ok(AcceptanceRecord {
                acceptance_id: DomainId::new(row.get::<_, String>(0)?),
                artifact_id: DomainId::new(artifact_id.to_string()),
                binding: AcceptanceBinding {
                    work_package_revision: DomainId::new(row.get::<_, String>(1)?),
                    input_baseline: DomainId::new(row.get::<_, String>(2)?),
                    artifact_revision: DomainId::new(row.get::<_, String>(3)?),
                    verification_rules: DomainId::new(row.get::<_, String>(4)?),
                    target_environment: DomainId::new(row.get::<_, String>(5)?),
                    merge_combination: row.get::<_, Option<String>>(6)?.map(DomainId::new),
                },
                decision,
                actor: DomainId::new(row.get::<_, String>(8)?),
                reason: row.get(9)?,
                evidence_refs: split_list(&evidence),
                accepted_at_ms: row.get(11)?,
                superseded_by: row.get::<_, Option<String>>(12)?.map(DomainId::new),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    pub fn insert_decision(
        &self,
        company_id: &str,
        record: &crate::domain::artifact::DecisionRecord,
    ) -> Result<(), StoreError> {
        let revisions = record
            .revision_refs
            .iter()
            .map(DomainId::as_str)
            .collect::<Vec<_>>()
            .join(ARRAY_SEP);
        self.conn.execute(
            "INSERT INTO decisions (
                decision_id, company_id, actor_id, scope_company, scope_project, scope_issue,
                decision, reason, evidence_refs, revision_refs, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.decision_id.as_str(),
                company_id,
                record.actor.as_str(),
                record.scope.company_id.as_str(),
                record.scope.project_id.as_ref().map(DomainId::as_str),
                record.scope.issue_id.as_ref().map(DomainId::as_str),
                record.decision,
                record.reason,
                record.evidence_refs.join(ARRAY_SEP),
                revisions,
                record.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_work_package_state(
        &self,
        company_id: &str,
        issue_uid: &str,
        state: &WorkPackageState,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO work_package_states (
                company_id, issue_uid, revision_id, required, accepted_revision, running, blockers
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(issue_uid, revision_id) DO UPDATE SET
                required = excluded.required,
                accepted_revision = excluded.accepted_revision,
                running = excluded.running,
                blockers = excluded.blockers",
            params![
                company_id,
                issue_uid,
                state.revision.as_str(),
                i64::from(u8::from(state.required)),
                state.accepted_revision.as_ref().map(DomainId::as_str),
                i64::from(u8::from(state.running)),
                state.blockers.join(ARRAY_SEP),
            ],
        )?;
        Ok(())
    }

    pub fn list_work_package_states(
        &self,
        issue_uid: &str,
    ) -> Result<Vec<WorkPackageState>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT revision_id, required, accepted_revision, running, blockers
             FROM work_package_states WHERE issue_uid = ?1 ORDER BY revision_id",
        )?;
        let rows = stmt.query_map(params![issue_uid], |row| {
            let blockers: String = row.get(4)?;
            Ok(WorkPackageState {
                revision: DomainId::new(row.get::<_, String>(0)?),
                required: row.get::<_, i64>(1)? == 1,
                accepted_revision: row.get::<_, Option<String>>(2)?.map(DomainId::new),
                running: row.get::<_, i64>(3)? == 1,
                blockers: split_list(&blockers),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    /// 某 Issue 的决策记录条数（供控制 API 与冒烟断言）。
    pub fn count_decisions(&self, issue_uid: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE scope_issue = ?1",
                params![issue_uid],
                |row| row.get(0),
            )
            .map_err(StoreError::Sql)
    }
}
