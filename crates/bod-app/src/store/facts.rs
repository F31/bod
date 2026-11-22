//! §8.1 事实装配端口（P8 3.6 B1/B2）：把库里的授权事实读成 `StageFacts`。
//!
//! 边界（ADR-0007 第 2 条）：
//! - **只读**：不写库、不派发、不触发任何外部效果。写路径仍走 `UnitOfWork`。
//! - **不判定**：这里只回答「库里的现况是什么」，不回答「能不能做」。
//!   判定在 `authorization`，它不依赖 `SqliteStore`（模块头第一句）。
//! - **按主体投影**：只装传入主体的那几行。判定只读 `facts` 里与该主体相关的部分
//!   （`is_member` / `customer_of` / `has_active_grant` 都以 `actor.id` 过滤），
//!   所以这是等价而不是近似；装全公司会把一次请求的成本绑到组织规模上。
//! - **失败不降级**：读不出来就报 `StageFactsError`。返回空 `StageFacts` 会让
//!   `record_visible` 全假——每个请求都 `BOD_NOT_FOUND`，且与「对象不存在」不可区分。
//!
//! 一组请求只装配一次：目标数量增长时不要在循环里重查（ADR-0007 的 N+1 代价）。
//! 端口本身不做缓存，缓存属于调用方（一个请求一份）。

use super::{parse_owner_kind, parse_permission};
use crate::authorization::{Assignment, Grant, StageFacts};
use crate::domain::DomainId;
use crate::domain::model::{Owner, Scope};
use rusqlite::Connection;

/// 装配失败的两种成因。分开是因为处置不同：
/// `Unreadable` 是环境故障（重试或报修），`UnknownValue` 是数据被绕过应用改写（要人看）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageFactsError {
    /// 某组事实读不出来：缺表、锁等待超时、库损坏。
    Unreadable {
        /// 出事的是哪一组事实（`grants` / `members` / ...）。排查时先看这一项。
        group: &'static str,
        detail: String,
    },
    /// 库里出现表外取值。典型成因是有人直接写库，或写入侧改了拼写而读侧没跟着改。
    UnknownValue { field: &'static str, value: String },
}

impl StageFactsError {
    /// 对外稳定码。与三档拒绝码分开：这不是「没权限」，是「判不了」。
    pub const CODE: &'static str = "BOD_FACTS_UNAVAILABLE";

    fn unreadable(group: &'static str, err: impl std::fmt::Display) -> Self {
        Self::Unreadable {
            group,
            detail: err.to_string(),
        }
    }

    pub fn as_code(&self) -> &'static str {
        Self::CODE
    }

    pub fn explain(&self) -> String {
        match self {
            Self::Unreadable { group, detail } => {
                format!("授权事实 {group} 读取失败：{detail}（不降级为「没有事实」）")
            }
            Self::UnknownValue { field, value } => {
                format!("授权事实 {field} 出现表外取值 {value:?}；读侧不猜默认值")
            }
        }
    }

    /// 对外状态码。取不到事实是服务方的问题，不是调用方的权限问题：
    /// 回 403 会把「我们坏了」写成「你没权限」。
    pub fn http_status(&self) -> u16 {
        503
    }
}

/// 装配端口。持有一个**不可变**连接引用——类型上就写不进去。
pub struct StageFactsPort<'a> {
    conn: &'a Connection,
}

impl<'a> StageFactsPort<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
    /// 装配一个主体的完整事实快照。顺序固定，失败时先报靠前的组。
    pub fn assemble(
        &self,
        company_id: &str,
        principal: &DomainId,
    ) -> Result<StageFacts, StageFactsError> {
        Ok(StageFacts {
            grants: self.grants(company_id, principal)?,
            assignments: self.assignments(company_id, principal)?,
            members: self.members(company_id, principal)?,
            release_operators: self.release_operators(company_id)?,
            customer_bindings: self.customer_bindings(company_id, principal)?,
            revoked_at_revision: self.revoked_at_revision(company_id, principal)?,
        })
    }

    /// 生效的授权。停用的不装：装配给的是**当前事实**，历史在 `audit_events`。
    /// 撤权对离线重放的影响由 `revoked_at_revision` 单独表达，不靠这一组。
    fn grants(
        &self,
        company_id: &str,
        principal: &DomainId,
    ) -> Result<Vec<Grant>, StageFactsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT project_id, permission, revision FROM grants
                 WHERE company_id = ?1 AND principal_id = ?2 AND active = 1
                 ORDER BY revision, project_id",
            )
            .map_err(|err| StageFactsError::unreadable("grants", err))?;
        let rows = stmt
            .query_map(rusqlite::params![company_id, principal.as_str()], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|err| StageFactsError::unreadable("grants", err))?;
        let mut grants = Vec::new();
        for row in rows {
            let (project_id, permission, revision) =
                row.map_err(|err| StageFactsError::unreadable("grants", err))?;
            let permission =
                parse_permission(&permission).ok_or(StageFactsError::UnknownValue {
                    field: "grants.permission",
                    value: permission,
                })?;
            grants.push(Grant {
                principal_id: principal.clone(),
                scope: Scope {
                    company_id: DomainId::new(company_id),
                    project_id: project_id.map(DomainId::new),
                    issue_id: None,
                },
                permission,
                revision: to_revision(revision).ok_or_else(|| StageFactsError::UnknownValue {
                    field: "grants.revision",
                    value: revision.to_string(),
                })?,
                // 查询已按 `active = 1` 过滤，构造出来必然是生效的。
                active: true,
            });
        }
        Ok(grants)
    }

    /// 指派**从既有 `issues` 的 owner 列派生**，不新建表（3.6 B6）。
    ///
    /// 理由：`issues.owner_id / owner_kind / assignment_grant_revision` 已经是写路径
    /// 唯一维护的指派事实（`insert_issue` 三列同写）。再建一张 `assignments` 表就是
    /// 第二份会漂移的真相——而且没有任何写入者，等于凭空多一个「看起来有」的来源。
    ///
    /// `assignment_grant_revision` 为空时按 0 处理：schema 允许为空，写路径不会产生，
    /// 判定也不读这个字段（`StageFacts.assignments` 目前不参与 `authorize_stage`）。
    /// 不因此整组失败，是因为一行脏数据不该让该主体所有读取都 503。
    fn assignments(
        &self,
        company_id: &str,
        principal: &DomainId,
    ) -> Result<Vec<Assignment>, StageFactsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT uid, project_id, owner_kind, assignment_grant_revision FROM issues
                 WHERE company_id = ?1 AND owner_id = ?2
                 ORDER BY uid",
            )
            .map_err(|err| StageFactsError::unreadable("assignments", err))?;
        let rows = stmt
            .query_map(rusqlite::params![company_id, principal.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })
            .map_err(|err| StageFactsError::unreadable("assignments", err))?;
        let mut assignments = Vec::new();
        for row in rows {
            let (issue_id, project_id, kind, revision) =
                row.map_err(|err| StageFactsError::unreadable("assignments", err))?;
            let kind = parse_owner_kind(&kind).ok_or(StageFactsError::UnknownValue {
                field: "issues.owner_kind",
                value: kind,
            })?;
            assignments.push(Assignment {
                owner: Owner {
                    kind,
                    id: principal.clone(),
                },
                scope: Scope {
                    company_id: DomainId::new(company_id),
                    project_id: project_id.map(DomainId::new),
                    issue_id: Some(DomainId::new(issue_id)),
                },
                grant_revision: revision.and_then(to_revision).unwrap_or(0),
            });
        }
        Ok(assignments)
    }

    /// 项目成员：`(project_id, principal)`。Company 级身份在 001 的 `members`，不在这里。
    fn members(
        &self,
        company_id: &str,
        principal: &DomainId,
    ) -> Result<Vec<(String, DomainId)>, StageFactsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT project_id FROM project_members
                 WHERE company_id = ?1 AND principal_id = ?2 AND active = 1
                 ORDER BY project_id",
            )
            .map_err(|err| StageFactsError::unreadable("members", err))?;
        let rows = stmt
            .query_map(rusqlite::params![company_id, principal.as_str()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|err| StageFactsError::unreadable("members", err))?;
        let mut members = Vec::new();
        for row in rows {
            let project_id = row.map_err(|err| StageFactsError::unreadable("members", err))?;
            members.push((project_id, principal.clone()));
        }
        Ok(members)
    }

    /// 发布操作人名单。整份读出：判定问的是「我在不在名单里」，
    /// 把「我是否在」直接查出来的写法会让 `StageFacts` 从事实退化成答案。
    fn release_operators(&self, company_id: &str) -> Result<Vec<DomainId>, StageFactsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT principal_id FROM release_operators
                 WHERE company_id = ?1 AND active = 1
                 ORDER BY principal_id",
            )
            .map_err(|err| StageFactsError::unreadable("release_operators", err))?;
        let rows = stmt
            .query_map(rusqlite::params![company_id], |row| row.get::<_, String>(0))
            .map_err(|err| StageFactsError::unreadable("release_operators", err))?;
        let mut operators = Vec::new();
        for row in rows {
            operators.push(DomainId::new(row.map_err(|err| {
                StageFactsError::unreadable("release_operators", err)
            })?));
        }
        Ok(operators)
    }

    /// 客户绑定。主键保证一个主体只有一条，所以这里最多一行。
    fn customer_bindings(
        &self,
        company_id: &str,
        principal: &DomainId,
    ) -> Result<Vec<(DomainId, String)>, StageFactsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT customer_id FROM customer_bindings
                 WHERE company_id = ?1 AND principal_id = ?2 AND active = 1",
            )
            .map_err(|err| StageFactsError::unreadable("customer_bindings", err))?;
        let rows = stmt
            .query_map(rusqlite::params![company_id, principal.as_str()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|err| StageFactsError::unreadable("customer_bindings", err))?;
        let mut bindings = Vec::new();
        for row in rows {
            let customer_id =
                row.map_err(|err| StageFactsError::unreadable("customer_bindings", err))?;
            bindings.push((principal.clone(), customer_id));
        }
        Ok(bindings)
    }

    /// 最近一次撤权的修订号：公司级（`principal_id IS NULL`）或本主体专属。
    ///
    /// 只认 `authorization_revocations`。**不从 `grants.active = 0` 的行反推**：
    /// `grants.revision` 是授予时的修订，停用不 bump，推出来会偏旧——
    /// `check_subscription` 用 `since < revoked_at` 决定要不要重同步，偏旧等于漏一次重同步。
    fn revoked_at_revision(
        &self,
        company_id: &str,
        principal: &DomainId,
    ) -> Result<Option<u64>, StageFactsError> {
        let row: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(revision) FROM authorization_revocations
                 WHERE company_id = ?1 AND (principal_id IS NULL OR principal_id = ?2)",
                rusqlite::params![company_id, principal.as_str()],
                |row| row.get(0),
            )
            .map_err(|err| StageFactsError::unreadable("revoked_at_revision", err))?;
        Ok(row.and_then(to_revision))
    }
}

/// `i64` → `u64` 的收敛：负修订是表外取值，返回 `None` 让调用方报错。
fn to_revision(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

impl super::SqliteStore {
    /// 一次请求一份事实：目标循环里不要重复调用（ADR-0007 的 N+1 代价）。
    pub fn stage_facts(
        &self,
        company_id: &str,
        principal: &DomainId,
    ) -> Result<StageFacts, StageFactsError> {
        StageFactsPort::new(&self.conn).assemble(company_id, principal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::Version;
    use crate::domain::model::{
        AcceptanceCriteria, Issue, IssueKind, OwnerKind, Permission, Phase, ResourceMeta,
        ResourceStatus,
    };
    use crate::store::SqliteStore;

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

    fn principal(id: &str) -> DomainId {
        DomainId::new(id)
    }

    /// 空库装配出的是一份**空事实**，不是一份「全都不可见」的判定结果。
    /// 六组一起断言：新增一组事实时这条会跟着提醒补断言。
    #[test]
    fn a_fresh_store_yields_empty_facts() {
        let store = open_store();
        let facts = store
            .stage_facts("company-default", &principal("user-1"))
            .expect("应可装配");
        assert_eq!(facts, StageFacts::default());
    }

    /// 授权按 (公司, 主体) 取：跨公司、跨主体、已停用的都不进事实。
    #[test]
    fn grants_are_scoped_to_company_and_subject_and_skip_inactive_rows() {
        let store = open_store();
        seed(
            &store,
            "INSERT INTO grants (principal_id, company_id, project_id, permission, revision, active)
             VALUES
               ('user-1', 'company-default', 'project-1', 'Dispatch', 3, 1),
               ('user-1', 'company-default', NULL, 'ManageScope', 5, 1),
               ('user-1', 'company-default', 'project-1', 'Accept', 7, 0),
               ('user-2', 'company-default', 'project-1', 'Accept', 9, 1),
               ('user-1', 'company-other', 'project-1', 'Accept', 9, 1)",
        );

        let facts = store
            .stage_facts("company-default", &principal("user-1"))
            .expect("应可装配");
        assert_eq!(facts.grants.len(), 2, "两条生效的，停用与跨公司各一条不算");
        assert!(facts.has_active_grant(&principal("user-1"), Permission::Dispatch));
        assert!(facts.has_active_grant(&principal("user-1"), Permission::ManageScope));
        assert!(
            !facts.has_active_grant(&principal("user-1"), Permission::Accept),
            "停用（active=0）的授权不得出现在事实里"
        );
        assert!(
            facts
                .grants
                .iter()
                .any(|grant| grant.scope.project_id.is_none()),
            "NULL project_id 是公司级授权，不能被读成某个项目"
        );
    }

    /// 成员、客户绑定、发布操作人三组各自有来源；停用的一律不装。
    #[test]
    fn members_bindings_and_release_operators_come_from_their_own_tables() {
        let store = open_store();
        seed(
            &store,
            "INSERT INTO project_members
                 (company_id, project_id, principal_id, kind, revision, active, updated_at_ms)
             VALUES
               ('company-default', 'project-1', 'user-1', 'user', 2, 1, 10),
               ('company-default', 'project-2', 'user-1', 'user', 3, 0, 11),
               ('company-default', 'project-1', 'user-2', 'agent', 2, 1, 12);
             INSERT INTO customer_bindings
                 (company_id, principal_id, customer_id, revision, active, updated_at_ms)
             VALUES
               ('company-default', 'user-customer', 'customer-a', 4, 1, 20),
               ('company-default', 'user-gone', 'customer-b', 5, 0, 21);
             INSERT INTO release_operators
                 (company_id, principal_id, revision, active, updated_at_ms)
             VALUES
               ('company-default', 'user-release', 6, 1, 30),
               ('company-default', 'user-past', 7, 0, 31);",
        );

        let facts = store
            .stage_facts("company-default", &principal("user-1"))
            .expect("应可装配");
        assert_eq!(
            facts.members,
            vec![("project-1".to_string(), principal("user-1"))],
            "停用的项目成员不进事实；别人的成员资格也不进"
        );

        let customer = store
            .stage_facts("company-default", &principal("user-customer"))
            .expect("应可装配");
        assert_eq!(
            customer.customer_bindings,
            vec![(principal("user-customer"), "customer-a".to_string())]
        );
        assert!(
            facts.release_operators.contains(&principal("user-release")),
            "发布操作人名单是公司级的，按公司读全量"
        );
        assert!(!facts.release_operators.contains(&principal("user-past")));

        let gone = store
            .stage_facts("company-default", &principal("user-gone"))
            .expect("应可装配");
        assert_eq!(
            gone.customer_bindings,
            Vec::new(),
            "撤掉的绑定不得继续给客户视角"
        );
    }

    /// 指派从既有 `issues` 派生（B6）。不另建表：那会是第二份会漂移的真相。
    #[test]
    fn assignments_are_derived_from_existing_issue_rows() {
        let mut store = open_store();
        let assignment = Assignment {
            owner: Owner {
                kind: OwnerKind::User,
                id: principal("user-1"),
            },
            scope: Scope {
                company_id: principal("company-default"),
                project_id: Some(principal("project-1")),
                issue_id: Some(principal("issue-1")),
            },
            grant_revision: 4,
        };
        let mut issue = Issue {
            meta: ResourceMeta {
                uid: principal("issue-1"),
                company_id: principal("company-default"),
                project_id: Some(principal("project-1")),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: IssueKind::Coding,
            repository: None,
            spec: crate::domain::IssueSpec {
                title: "实现登录".to_string(),
                assignment: Some(assignment.clone()),
                acceptance: AcceptanceCriteria::default(),
            },
            status: ResourceStatus {
                phase: Phase::Pending,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        };
        {
            // 写入走 UnitOfWork：`insert_issue` 只存在于事务上，测试不绕开那条路径。
            let uow = store.transaction().expect("应可开始事务");
            uow.insert_issue(&issue).expect("应可写入");
            issue.meta.uid = principal("issue-2");
            issue.spec.title = "另一件事".to_string();
            uow.insert_issue(&issue).expect("应可写入");
            uow.commit().expect("应可提交");
        }

        let facts = store
            .stage_facts("company-default", &principal("user-1"))
            .expect("应可装配");
        assert_eq!(facts.assignments.len(), 2, "两条指派都属于这个主体");
        assert_eq!(
            facts.assignments[0]
                .scope
                .issue_id
                .as_ref()
                .map(DomainId::as_str),
            Some("issue-1")
        );
        assert_eq!(facts.assignments[0].grant_revision, 4);
        assert_eq!(
            facts.assignments[0].owner.kind,
            OwnerKind::User,
            "owner_kind 在库里是 \"User\"（大写），读侧必须按同一套拼写还原"
        );

        let other = store
            .stage_facts("company-default", &principal("user-2"))
            .expect("应可装配");
        assert!(
            other.assignments.is_empty(),
            "没有指派关系的身体不该看到别人的指派"
        );
    }

    /// 撤权修订取「公司级或本主体」里最大的一条；别人的专属撤权不算。
    #[test]
    fn revoked_revision_takes_the_latest_relevant_revocation() {
        let store = open_store();
        seed(
            &store,
            "INSERT INTO authorization_revocations
                 (company_id, revision, principal_id, revoked_at_ms, reason)
             VALUES
               ('company-default', 4, NULL, 100, '公司级撤权'),
               ('company-default', 7, 'user-1', 200, '该主体的会话撤掉'),
               ('company-default', 9, 'user-2', 300, '与 user-1 无关');",
        );

        assert_eq!(
            store
                .stage_facts("company-default", &principal("user-1"))
                .expect("应可装配")
                .revoked_at_revision,
            Some(7),
            "取 4 与 7 的较大者；9 是别人的"
        );
        assert_eq!(
            store
                .stage_facts("company-default", &principal("user-2"))
                .expect("应可装配")
                .revoked_at_revision,
            Some(9)
        );
        assert_eq!(
            store
                .stage_facts("company-other", &principal("user-1"))
                .expect("应可装配")
                .revoked_at_revision,
            None,
            "别的公司一条撤权记录都没有"
        );
    }

    /// 表外取值必须报错，不能猜成某个已知权限——猜错就是把越权写成缺陷。
    #[test]
    fn unknown_permission_text_is_an_error_not_a_default() {
        let store = open_store();
        seed(
            &store,
            "INSERT INTO grants (principal_id, company_id, project_id, permission, revision, active)
             VALUES ('user-1', 'company-default', NULL, 'Root', 1, 1)",
        );
        let err = store
            .stage_facts("company-default", &principal("user-1"))
            .expect_err("表外权限必须报错");
        assert_eq!(
            err,
            StageFactsError::UnknownValue {
                field: "grants.permission",
                value: "Root".to_string()
            }
        );
        assert_eq!(err.http_status(), 503, "判不了不是没权限");
        assert_eq!(err.as_code(), StageFactsError::CODE);
    }

    /// 缺表时返回错误，而不是一份空事实：空事实会让每个请求都 `BOD_NOT_FOUND`，
    /// 与「对象不存在」不可区分——这比直接报故障更糟。
    #[test]
    fn a_missing_table_is_an_error_and_never_an_empty_snapshot() {
        let conn = Connection::open_in_memory().expect("裸连接应可打开");
        let err = StageFactsPort::new(&conn)
            .assemble("company-default", &principal("user-1"))
            .expect_err("没有迁移的表必须报错");
        assert!(
            matches!(
                err,
                StageFactsError::Unreadable {
                    group: "grants",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// 一个主体同一时刻只绑定一个客户：多绑会让 `customer_of` 的答案取决于行序。
    #[test]
    fn a_principal_cannot_be_bound_to_two_customers_at_once() {
        let store = open_store();
        seed(
            &store,
            "INSERT INTO customer_bindings
                 (company_id, principal_id, customer_id, revision, active, updated_at_ms)
             VALUES ('company-default', 'user-1', 'customer-a', 1, 1, 10)",
        );
        let err = store
            .execute_batch_for_test(
                "INSERT INTO customer_bindings
                     (company_id, principal_id, customer_id, revision, active, updated_at_ms)
                 VALUES ('company-default', 'user-1', 'customer-b', 2, 1, 20)",
            )
            .expect_err("第二个绑定必须被主键拒绝");
        assert!(
            matches!(err, super::super::StoreError::Sql(_)),
            "应是唯一约束冲突，而不是静默覆盖"
        );
    }

    /// 撤权记过就不能改：改写撤权修订等于追溯性地改掉「按哪一版授权判的」。
    #[test]
    fn revocations_are_append_only() {
        let store = open_store();
        seed(
            &store,
            "INSERT INTO authorization_revocations
                 (company_id, revision, principal_id, revoked_at_ms, reason)
             VALUES ('company-default', 4, NULL, 100, '公司级撤权')",
        );
        assert!(
            store
                .execute_batch_for_test(
                    "UPDATE authorization_revocations SET revision = 1 WHERE revision = 4"
                )
                .is_err(),
            "UPDATE 必须被 trigger 拒绝"
        );
        assert!(
            store
                .execute_batch_for_test("DELETE FROM authorization_revocations")
                .is_err(),
            "DELETE 必须被 trigger 拒绝"
        );
    }

    /// 装配与判定之间的接缝：同一份事实驱动 `access_record`，不再回库查第二次。
    ///
    /// 这条用例替代的是「等 GET 路由落地才能验证」的等待：它证明三档判定在真实库行上
    /// 走得通，剩下的只差把 `Principal` 与连接交给 handler。
    #[test]
    fn assembled_facts_drive_the_record_decision() {
        use crate::authorization::{
            Principal, PrincipalKind, RecordAccess, RecordRejection, ScopedRecord, StageAction,
            StageTarget, access_record, classify_access,
        };

        let store = open_store();
        seed(
            &store,
            "INSERT INTO project_members
                 (company_id, project_id, principal_id, kind, revision, active, updated_at_ms)
             VALUES ('company-default', 'project-1', 'user-1', 'user', 1, 1, 10);
             INSERT INTO customer_bindings
                 (company_id, principal_id, customer_id, revision, active, updated_at_ms)
             VALUES ('company-default', 'user-cust', 'customer-a', 1, 1, 10);",
        );
        let scope = |project: &str| Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new(project)),
            issue_id: None,
        };
        let at = "2026-11-22T00:00:00Z";

        let member = Principal {
            id: principal("user-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let facts = store
            .stage_facts("company-default", &member.id)
            .expect("应可装配");
        let target = StageTarget::new(scope("project-1"), 1);
        let own = ScopedRecord::new("dt-own", scope("project-1"));
        let sibling = ScopedRecord::new("dt-sibling", scope("project-2"));

        assert!(matches!(
            access_record(&facts, &member, &own, StageAction::Review, &target, at),
            RecordAccess::Allowed(_)
        ));
        let sibling_access =
            access_record(&facts, &member, &sibling, StageAction::Review, &target, at);
        assert_eq!(
            classify_access(&sibling_access).err(),
            Some(RecordRejection::NotFound),
            "非成员的项目一律「不存在」，不产生差异信号"
        );

        // 客户协作者：本客户目标可见，兄弟客户与无客户维度的内部记录都不可见。
        let customer = Principal {
            id: principal("user-cust"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let customer_facts = store
            .stage_facts("company-default", &customer.id)
            .expect("应可装配");
        let mine = ScopedRecord::new("dt-a", scope("project-1")).for_customer("customer-a");
        let theirs = ScopedRecord::new("dt-b", scope("project-1")).for_customer("customer-b");
        let internal = ScopedRecord::new("dt-inner", scope("project-1"));
        for (record, label) in [
            (&mine, "本客户目标"),
            (&theirs, "兄弟客户的目标"),
            (&internal, "无客户维度的内部记录"),
        ] {
            let access = access_record(
                &customer_facts,
                &customer,
                record,
                StageAction::Review,
                &target,
                at,
            );
            let visible = !matches!(access, RecordAccess::Invisible);
            assert_eq!(
                visible,
                label == "本客户目标",
                "{label} 的可见性判错了：{access:?}"
            );
        }
    }
}
