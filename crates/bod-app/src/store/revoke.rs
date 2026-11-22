//! 撤权的库侧写入：`authorization_revocations` 与四张事实表的**唯一**写入者。
//!
//! 为什么需要这一个文件：`authorization::revoke_member` 早就在判定层算出了「撤权后
//! 应该是什么样」，迁移 020 也建好了表，`store::facts` 也读得出 `revoked_at_revision`
//! ——三端齐备，唯独中间那一步「把事实改掉」没有代码。结果是：撤权在内存里成立，
//! 重新装配事实时又会读回全部生效授权，等于没撤。这不是「少了优化」，是功能不生效。
//!
//! 三条刻意的取舍：
//!
//! 1. **停用而不删除**。四张表都有 `active` 列，撤权就是把它们置 0 而不是 `DELETE`。
//!    删掉之后「这个人曾经在这个项目里」就无从查证，而重新授权又要从头插一遍；
//!    置 0 保留了「曾经有过、现在没有」，装配侧本来就按 `active = 1` 过滤。
//! 2. **修订号从表里现算，不由调用方给**。`revoked_at_revision` 的语义是「按哪一版
//!    授权判的」，调用方填错会让离线客户端少重同步一次（`check_subscription` 的
//!    `since < revoked_at`）。所以这里在事务内取 `MAX(revision) + 1`，调用方无从填错。
//! 3. **已经是目标状态就是 no-op**，不 bump 修订、不写审计。判据是「有没有生效授权」，
//!    不是「有没有撤权记录」——撤权后又被重新授权时记录还在，这时必须能再撤一次。
//!    这条与导入来源那条一致：**只记真发生的事**，重放不该留下第二条撤权。
//!
//! `authorization_revocations.reason` 存撤权依据（谁批的、为什么），审计行只记
//! 「某公司某时刻由某操作者撤销了某个主体的授权」。两边的关联键是
//! `(company_id, revoked_at_ms = now_ms)`：服务循环单线程串行，同一毫秒同一公司
//! 不会有两笔撤权。刻意不把主体标识拼进审计的 `evidence`——那会把外部可控的字符串
//! 送进脱敏闸门（`SECRET_MARKERS`），一个叫 `secret-agent` 的主体就能让整条审计被拒。

use rusqlite::params;

use super::{StoreError, UnitOfWork};
use crate::audit_log::{AuditAction, AuditEvent};
use crate::domain::{DomainId, Scope};

/// 一次撤权命令。
///
/// `actor_id` 与 `principal_id` 必须分开：前者是**下命令的人**（进审计的 `actor`），
/// 后者是被撤权的主体。合成一个字段时，审计里「谁撤了谁的权」会退化成「谁被撤权」，
/// 事后查不出是谁做的决定。
pub struct RevokeCommand<'a> {
    pub company_id: &'a str,
    pub principal_id: &'a str,
    pub actor_id: &'a str,
    /// 撤权依据（谁批的、为什么）。空串表示未记录，与表默认值一致。
    pub reason: &'a str,
    pub correlation_id: &'a str,
    pub decided_at: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeOutcome {
    /// 新写入的撤权修订。`None` 表示该主体本来就没有生效授权，本次没有产生任何事实
    /// ——因此也没有新修订、没有审计行。
    pub revision: Option<u64>,
    pub disabled_grants: usize,
    pub disabled_memberships: usize,
    pub disabled_bindings: usize,
    pub disabled_release_operators: usize,
}

impl RevokeOutcome {
    fn noop() -> Self {
        Self {
            revision: None,
            disabled_grants: 0,
            disabled_memberships: 0,
            disabled_bindings: 0,
            disabled_release_operators: 0,
        }
    }

    /// 本次是否真的撤销了什么。`false` 时调用方不该报告「已撤权」。
    pub fn changed_anything(&self) -> bool {
        self.revision.is_some()
    }

    pub fn disabled_total(&self) -> usize {
        self.disabled_grants
            + self.disabled_memberships
            + self.disabled_bindings
            + self.disabled_release_operators
    }
}

/// 撤权命令本身的错误：调用方给的东西不成立。与 DB 故障分开——这些改请求就能解决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationError {
    EmptyCompanyId,
    EmptyPrincipalId,
    /// 没有操作者就没有「谁撤的权」，这样的审计行不如不写。
    EmptyActor,
    /// 修订号溢出 `u64`。现实中不可能，但这里不 `unwrap`：表被外部写脏时要报错。
    RevisionOutOfRange,
}

impl RevocationError {
    pub fn explain(&self) -> &'static str {
        match self {
            Self::EmptyCompanyId => "撤权命令缺少公司标识",
            Self::EmptyPrincipalId => "撤权命令缺少被撤权主体",
            Self::EmptyActor => "撤权命令缺少操作者：审计必须能回答「谁撤的」",
            Self::RevisionOutOfRange => "撤权修订号超出可表示范围",
        }
    }
}

impl<'a> UnitOfWork<'a> {
    /// 撤销某主体在本公司的全部生效授权。
    ///
    /// 事务边界：四张表的 `active` 归零、撤权修订追加、审计事件，三件事同一个
    /// SAVEPOINT（与导入来源同构，可被更外层事务再包一层）。审计写不进去就整条回滚
    /// ——留下「授权已停用但无人知道是谁做的」比拒绝这次撤权更难收拾。
    ///
    /// 用 SAVEPOINT 而不是直接写当前事务：命令之后若要接别的步骤，回滚粒度仍然只到
    /// 撤权这一层，不会连累调用方先前写入的东西。
    pub fn revoke_authorization(
        &mut self,
        command: &RevokeCommand<'_>,
        now_ms: i64,
    ) -> Result<RevokeOutcome, StoreError> {
        if command.company_id.trim().is_empty() {
            return Err(RevocationError::EmptyCompanyId.into());
        }
        if command.principal_id.trim().is_empty() {
            return Err(RevocationError::EmptyPrincipalId.into());
        }
        if command.actor_id.trim().is_empty() {
            return Err(RevocationError::EmptyActor.into());
        }

        self.conn.execute_batch("SAVEPOINT revoke_authorization")?;
        let outcome = (|| -> Result<RevokeOutcome, StoreError> {
            let disabled_grants = self.conn.execute(
                "UPDATE grants SET active = 0
                 WHERE company_id = ?1 AND principal_id = ?2 AND active = 1",
                params![command.company_id, command.principal_id],
            )?;
            let disabled_memberships = self.conn.execute(
                "UPDATE project_members SET active = 0, updated_at_ms = ?3
                 WHERE company_id = ?1 AND principal_id = ?2 AND active = 1",
                params![command.company_id, command.principal_id, now_ms],
            )?;
            let disabled_bindings = self.conn.execute(
                "UPDATE customer_bindings SET active = 0, updated_at_ms = ?3
                 WHERE company_id = ?1 AND principal_id = ?2 AND active = 1",
                params![command.company_id, command.principal_id, now_ms],
            )?;
            let disabled_release_operators = self.conn.execute(
                "UPDATE release_operators SET active = 0, updated_at_ms = ?3
                 WHERE company_id = ?1 AND principal_id = ?2 AND active = 1",
                params![command.company_id, command.principal_id, now_ms],
            )?;

            // 一行都没停用 ⇒ 这个主体本来就没有生效授权。不写修订、不写审计。
            if disabled_grants
                + disabled_memberships
                + disabled_bindings
                + disabled_release_operators
                == 0
            {
                return Ok(RevokeOutcome::noop());
            }

            // 修订号在事务内现算：调用方填错会让离线客户端少重同步一次。
            let next_revision: i64 = self.conn.query_row(
                "SELECT COALESCE(MAX(revision), 0) + 1 FROM authorization_revocations
                 WHERE company_id = ?1",
                params![command.company_id],
                |row| row.get(0),
            )?;
            let revision =
                u64::try_from(next_revision).map_err(|_| RevocationError::RevisionOutOfRange)?;

            self.conn.execute(
                "INSERT INTO authorization_revocations (
                    company_id, revision, principal_id, revoked_at_ms, reason
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    command.company_id,
                    next_revision,
                    command.principal_id,
                    now_ms,
                    command.reason,
                ],
            )?;

            // 撤权是公司级范围的事件：它不属于某个项目，也不属于某个交付目标。
            let event = AuditEvent::for_write(
                command.company_id,
                command.correlation_id,
                DomainId::new(command.actor_id),
                AuditAction::RevokeAuthorization,
                Scope {
                    company_id: DomainId::new(command.company_id),
                    project_id: None,
                    issue_id: None,
                },
                "principal_revocation",
                command.decided_at,
            )
            .map_err(StoreError::Audit)?;
            self.append_audit_event(&event, now_ms)?;

            Ok(RevokeOutcome {
                revision: Some(revision),
                disabled_grants,
                disabled_memberships,
                disabled_bindings,
                disabled_release_operators,
            })
        })();

        match outcome {
            Ok(value) => {
                self.conn.execute_batch("RELEASE revoke_authorization")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO revoke_authorization; RELEASE revoke_authorization",
                );
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::store::SqliteStore;

    const COMPANY: &str = "company-default";
    const OTHER_COMPANY: &str = "company-other";
    const SUBJECT: &str = "user-1";
    const NOW: i64 = 1_763_600_000_000;

    fn config() -> BodConfig {
        let mut config = BodConfig::default();
        config.sqlite.busy_timeout_ms = 5_000;
        config
    }

    fn open_store() -> SqliteStore {
        SqliteStore::in_memory(&config()).expect("内存库应可打开")
    }

    fn seed(store: &SqliteStore, sql: &str) {
        store
            .execute_batch_for_test(sql)
            .expect("测试夹具应写入成功");
    }

    /// 夹具：`user-1` 是内部成员（成员 + 三种授权 + 发布操作人），
    /// `user-customer` 是客户协作者（只有客户绑定）。两者刻意不是同一个人：
    /// `is_visible` 对「有客户绑定的主体」要求记录带客户维度，让内部成员再挂一个
    /// 客户绑定会让所有内部动作在阶段二被判成不可见。另家公司与另一个主体各留一份对照。
    fn seed_credentials(store: &SqliteStore) {
        seed(
            store,
            "INSERT INTO grants (principal_id, company_id, project_id, permission, revision, active)
             VALUES
               ('user-1', 'company-default', 'project-1', 'Dispatch', 1, 1),
               ('user-1', 'company-default', 'project-1', 'ManageScope', 1, 1),
               ('user-1', 'company-default', NULL, 'Accept', 1, 1),
               ('user-2', 'company-default', 'project-1', 'Dispatch', 1, 1),
               ('user-1', 'company-other', 'project-1', 'Dispatch', 1, 1);
             INSERT INTO project_members
                 (company_id, project_id, principal_id, kind, revision, active, updated_at_ms)
             VALUES
               ('company-default', 'project-1', 'user-1', 'user', 1, 1, 10),
               ('company-default', 'project-1', 'user-2', 'user', 1, 1, 11),
               ('company-other', 'project-1', 'user-1', 'user', 1, 1, 12);
             INSERT INTO release_operators (company_id, principal_id, revision, active, updated_at_ms)
             VALUES ('company-default', 'user-1', 1, 1, 20);
             INSERT INTO customer_bindings
                 (company_id, principal_id, customer_id, revision, active, updated_at_ms)
             VALUES ('company-default', 'user-customer', 'customer-a', 1, 1, 30);",
        );
    }

    fn command() -> RevokeCommand<'static> {
        RevokeCommand {
            company_id: COMPANY,
            principal_id: SUBJECT,
            actor_id: "user-admin",
            reason: "成员退出项目",
            correlation_id: "req-revoke-1",
            decided_at: "2026-11-22T06:00:00Z",
        }
    }

    fn revoke(store: &mut SqliteStore, command: &RevokeCommand<'_>) -> RevokeOutcome {
        let mut uow = store.transaction().expect("应能开始事务");
        let outcome = uow.revoke_authorization(command, NOW).expect("撤权应成功");
        uow.commit().expect("应能提交");
        outcome
    }

    fn scalar(store: &SqliteStore, sql: &str) -> i64 {
        store
            .read_connection()
            .query_row(sql, [], |row| row.get(0))
            .expect("聚合查询应有结果")
    }

    fn facts_of(
        store: &SqliteStore,
        company: &str,
        principal: &str,
    ) -> crate::authorization::StageFacts {
        store
            .stage_facts(company, &DomainId::new(principal))
            .expect("应可装配")
    }

    /// 撤权把四类凭据一起停用，并追加一条撤权修订。装配侧读回的必须是空事实。
    #[test]
    fn revoking_disables_every_active_fact_and_appends_a_revision() {
        let mut store = open_store();
        seed_credentials(&store);

        let outcome = revoke(&mut store, &command());
        assert_eq!(outcome.revision, Some(1), "首个撤权修订应是 1");
        assert!(outcome.changed_anything());
        assert_eq!(outcome.disabled_grants, 3, "三条生效授权全部停用");
        assert_eq!(outcome.disabled_memberships, 1);
        assert_eq!(outcome.disabled_bindings, 0, "内部成员没有客户绑定");
        assert_eq!(outcome.disabled_release_operators, 1);
        assert_eq!(outcome.disabled_total(), 5);

        let facts = facts_of(&store, COMPANY, SUBJECT);
        assert!(facts.grants.is_empty(), "停用后不应再有生效授权");
        assert!(facts.members.is_empty());
        assert!(facts.release_operators.is_empty());
        assert!(facts.customer_bindings.is_empty(), "他本来就没有客户绑定");
        assert_eq!(
            facts.revoked_at_revision,
            Some(1),
            "撤权修订必须能被读到，否则离线客户端不会重同步"
        );
    }

    /// 端到端：凭据一撤，判定立刻关门——除 `Work` 外（见下一条用例）。
    ///
    /// `Work` 的凭据是「被指派」而不是「成员或 Grant」，所以不在本组里。
    #[test]
    fn revocation_closes_every_credential_path_end_to_end() {
        use crate::authorization::{ScopedRecord, StageAction, StageTarget, authorize_stage};
        use crate::domain::Scope;

        let mut store = open_store();
        seed_credentials(&store);

        let scope = Scope {
            company_id: DomainId::new(COMPANY),
            project_id: Some(DomainId::new("project-1")),
            issue_id: Some(DomainId::new("issue-1")),
        };
        let actor = crate::authorization::Principal {
            id: DomainId::new(SUBJECT),
            kind: crate::domain::PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        // 不带 assignee：本组只测「凭据类」动作，Work 单独一条用例。
        let target = StageTarget::new(scope.clone(), 1);
        let record = ScopedRecord::new("issue-1", scope);
        let at = "2026-11-22T06:00:00Z";

        // `RecordAccess` 没有便捷方法，就地说明「放行」指什么，免得两处各写一套匹配。
        let access_allowed = |access: crate::authorization::RecordAccess| matches!(access, crate::authorization::RecordAccess::Allowed(decision) if decision.is_allowed());

        let credential_actions = [
            StageAction::Submit,
            StageAction::Review,
            StageAction::Prove,
            StageAction::ManageScope,
            StageAction::Dispatch,
            StageAction::Accept,
            StageAction::Reopen,
            StageAction::Release,
            StageAction::Rollback,
        ];

        let before = facts_of(&store, COMPANY, SUBJECT);
        for action in credential_actions {
            assert!(
                authorize_stage(&before, &actor, action, &target, at).is_allowed(),
                "撤权前 {} 应当放行",
                action.as_str()
            );
        }
        assert!(
            access_allowed(crate::authorization::access_record(
                &before,
                &actor,
                &record,
                StageAction::Dispatch,
                &target,
                at
            )),
            "记录级判定同样应放行"
        );

        revoke(&mut store, &command());

        let after = facts_of(&store, COMPANY, SUBJECT);
        for action in credential_actions {
            assert!(
                !authorize_stage(&after, &actor, action, &target, at).is_allowed(),
                "撤权后 {} 必须关门",
                action.as_str()
            );
        }
        assert!(
            !access_allowed(crate::authorization::access_record(
                &after,
                &actor,
                &record,
                StageAction::Dispatch,
                &target,
                at
            )),
            "记录级判定在撤权后不得放行"
        );
    }

    /// **已知缺口，用用例钉住**：`StageRequirement::Work` 只判 `target.assignee`，
    /// 不查成员资格也不查 Grant，所以撤权之后被指派者仍能「开发与进度回报」。
    ///
    /// 这与 P5 2.6「保留 Assignment 与历史归属」是一致的取舍，但**保留归属不等于
    /// 保留执行权**：一个已离开项目的人还能往项目里写进度。要收紧就得让 `Work`
    /// 同时要求成员资格，那是判定语义变更，得由产品决定，不在撤权写入这一层顺手改。
    #[test]
    fn assignment_alone_still_permits_work_after_revocation() {
        use crate::authorization::{StageAction, StageTarget, authorize_stage};
        use crate::domain::Scope;

        let mut store = open_store();
        seed_credentials(&store);
        revoke(&mut store, &command());

        let scope = Scope {
            company_id: DomainId::new(COMPANY),
            project_id: Some(DomainId::new("project-1")),
            issue_id: Some(DomainId::new("issue-1")),
        };
        let actor = crate::authorization::Principal {
            id: DomainId::new(SUBJECT),
            kind: crate::domain::PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let assigned = StageTarget::new(scope, 1).with_assignee(DomainId::new(SUBJECT));

        let facts = facts_of(&store, COMPANY, SUBJECT);
        assert!(
            authorize_stage(
                &facts,
                &actor,
                StageAction::Develop,
                &assigned,
                "2026-11-22T06:00:00Z"
            )
            .is_allowed(),
            "撤权不清 Assignment，所以 Work 仍放行——这是现状，不是本用例的期望终点"
        );
    }

    /// 再来一次撤权是 no-op：不追加修订、不写第二条审计。
    #[test]
    fn revoking_twice_does_not_append_a_second_revision() {
        let mut store = open_store();
        seed_credentials(&store);

        let first = revoke(&mut store, &command());
        assert_eq!(first.revision, Some(1));

        let second = revoke(&mut store, &command());
        assert_eq!(second.revision, None, "已在目标状态不该产生新修订");
        assert!(!second.changed_anything());
        assert_eq!(second.disabled_total(), 0);

        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM authorization_revocations"),
            1,
            "no-op 不得追加第二行"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM audit_events WHERE action = 'revoke-authorization'"
            ),
            1,
            "没发生的事不写审计"
        );
    }

    /// 撤权后重新授权，再撤时必须是新修订——否则离线客户端会以为没过撤权。
    #[test]
    fn reauthorizing_after_revocation_requires_another_revocation() {
        let mut store = open_store();
        seed_credentials(&store);
        assert_eq!(revoke(&mut store, &command()).revision, Some(1));

        seed(
            &store,
            "INSERT INTO grants (principal_id, company_id, project_id, permission, revision, active)
             VALUES ('user-1', 'company-default', 'project-1', 'Dispatch', 2, 1)",
        );

        let again = revoke(&mut store, &command());
        assert_eq!(again.revision, Some(2), "重新授权后的撤权必须有更大的修订");
        assert_eq!(again.disabled_grants, 1);
        assert_eq!(
            facts_of(&store, COMPANY, SUBJECT).revoked_at_revision,
            Some(2),
            "读到的必须是最新撤权修订"
        );
    }

    /// 撤权只作用于 (公司, 主体)：别的公司、别的主体的凭据一动不动。
    #[test]
    fn a_revocation_leaves_other_companies_and_subjects_untouched() {
        let mut store = open_store();
        seed_credentials(&store);
        revoke(&mut store, &command());

        let other_subject = facts_of(&store, COMPANY, "user-2");
        assert_eq!(other_subject.grants.len(), 1);
        assert_eq!(other_subject.revoked_at_revision, None);

        let other_company = facts_of(&store, OTHER_COMPANY, SUBJECT);
        assert_eq!(
            other_company.grants.len(),
            1,
            "同一 uid 在另一家公司的授权不受影响"
        );
        assert_eq!(other_company.members.len(), 1);
        assert_eq!(other_company.revoked_at_revision, None);
    }

    /// 审计写不进去就整条回滚：授权仍然生效，也没有留下撤权修订。
    ///
    /// 用「相关性 ID 含 secret 形态」触发脱敏闸门——这是真实会发生的失败，
    /// 而不是为了测试编一个假错误。失败点在审计，回滚必须在数据上看得见。
    #[test]
    fn a_secret_like_correlation_id_rolls_back_the_whole_revocation() {
        let mut store = open_store();
        seed_credentials(&store);

        let bad = RevokeCommand {
            correlation_id: "req-secret-1",
            ..command()
        };
        let mut uow = store.transaction().expect("应能开始事务");
        let err = uow
            .revoke_authorization(&bad, NOW)
            .expect_err("审计闸门必须拒绝");
        assert!(matches!(err, StoreError::Audit(_)), "{err}");
        uow.commit().expect("回滚后提交空事务应成功");

        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM authorization_revocations"),
            0,
            "审计失败不得留下撤权修订"
        );
        let facts = facts_of(&store, COMPANY, SUBJECT);
        assert_eq!(facts.grants.len(), 3, "四张表的 active 必须一起回滚");
        assert_eq!(facts.members.len(), 1);
        assert_eq!(facts.release_operators.len(), 1);
        assert_eq!(facts.revoked_at_revision, None);
    }

    /// 缺公司、缺主体、缺操作者都在动库之前拒绝。
    #[test]
    fn empty_identifiers_are_rejected_before_any_write() {
        let mut store = open_store();
        seed_credentials(&store);

        for (label, bad) in [
            (
                "缺公司",
                RevokeCommand {
                    company_id: "  ",
                    ..command()
                },
            ),
            (
                "缺主体",
                RevokeCommand {
                    principal_id: "",
                    ..command()
                },
            ),
            (
                "缺操作者",
                RevokeCommand {
                    actor_id: "",
                    ..command()
                },
            ),
        ] {
            let mut uow = store.transaction().expect("应能开始事务");
            let err = uow
                .revoke_authorization(&bad, NOW)
                .expect_err(&format!("{label} 必须被拒绝"));
            assert!(matches!(err, StoreError::Revocation(_)), "{label}: {err}");
            uow.commit().expect("空事务应可提交");
        }

        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM authorization_revocations"),
            0
        );
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM grants WHERE active = 1"),
            5,
            "拒绝路径不得动任何 grant"
        );
    }

    /// 客户协作者的撤权要停用客户绑定——否则他仍能看到该客户的全部目标。
    #[test]
    fn revoking_a_customer_collaborator_disables_the_binding() {
        let mut store = open_store();
        seed_credentials(&store);

        let outcome = revoke(
            &mut store,
            &RevokeCommand {
                principal_id: "user-customer",
                ..command()
            },
        );
        assert_eq!(outcome.disabled_bindings, 1);
        assert_eq!(outcome.disabled_grants, 0, "客户协作者本来就没有授权");
        assert_eq!(outcome.revision, Some(1));

        let facts = facts_of(&store, COMPANY, "user-customer");
        assert!(
            facts.customer_bindings.is_empty(),
            "绑定停用后不再装配出客户身份"
        );
        assert_eq!(facts.revoked_at_revision, Some(1));
    }

    /// 撤权记录追加式：改写或删除都被 trigger 拒绝。
    #[test]
    fn revocations_are_append_only() {
        let mut store = open_store();
        seed_credentials(&store);
        revoke(&mut store, &command());

        for sql in [
            "UPDATE authorization_revocations SET reason = '改一下'",
            "DELETE FROM authorization_revocations",
        ] {
            let err = store
                .execute_batch_for_test(sql)
                .expect_err("追加式表必须拒绝改写");
            assert!(
                err.to_string().contains("append-only"),
                "{sql} 应被 trigger 拒绝：{err}"
            );
        }
    }
}
