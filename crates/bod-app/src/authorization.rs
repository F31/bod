//! 应用授权入口（P0 D1 任务 2.1—2.3）。
//! 只包装 bod-core 的纯规则，不访问 DB；命令层用同一入口做当前权复核。

use crate::config::BodConfig;
use crate::domain::DomainId;
use crate::domain::model::*;

pub use crate::domain::model::{
    Assignment, Denial, DenialReason, Grant, Owner, OwnerKind, Permission, Principal,
    PrincipalKind, Scope,
};
pub use crate::domain::rules::{assign_issue, authorize, authorize_feedback};

/// 首次启动提供的默认 Company（§identity-access 默认 Company 与归属）。
pub fn default_company() -> Company {
    Company {
        id: DomainId::new("company-default"),
        display_name: "Local Company".to_string(),
    }
}

/// 默认部署边界：只接受回环地址；远程访问须显式认证与 TLS，另由部署任务处理。
pub fn is_loopback_listen(listen: &str) -> bool {
    let host = listen
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(listen)
        .trim_matches(['[', ']']);
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

pub fn deployment_boundary(config: &BodConfig) -> Result<(), Denial> {
    let scope = Scope {
        company_id: default_company().id,
        project_id: None,
        issue_id: None,
    };
    if is_loopback_listen(&config.server.listen) {
        Ok(())
    } else {
        Err(Denial {
            reason: DenialReason::InsufficientPermission,
            actor: DomainId::new("deployment"),
            scope: Box::new(scope),
            grant_revision: None,
            reactivation: "显式启用认证与 TLS 后才允许非回环监听".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Version;

    fn scope() -> Scope {
        Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new("project-1")),
            issue_id: Some(DomainId::new("issue-1")),
        }
    }

    fn agent(id: &str) -> Principal {
        Principal {
            id: DomainId::new(id),
            kind: PrincipalKind::Agent,
            claimed_roles: vec!["admin".to_string()],
        }
    }

    fn grants(revision: u64, active: bool) -> Vec<Grant> {
        vec![
            Grant {
                principal_id: DomainId::new("coder-1"),
                scope: scope(),
                permission: Permission::Dispatch,
                revision,
                active,
            },
            Grant {
                principal_id: DomainId::new("user-1"),
                scope: scope(),
                permission: Permission::ManageScope,
                revision: 1,
                active: true,
            },
        ]
    }

    fn issue(kind: IssueKind) -> Issue {
        Issue {
            meta: ResourceMeta {
                uid: DomainId::new("issue-1"),
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind,
            repository: Some(RepositoryContext {
                provider: "local".to_string(),
                repository_id: "repo-1".to_string(),
            }),
            spec: IssueSpec {
                title: "实现登录".to_string(),
                assignment: None,
                acceptance: AcceptanceCriteria {
                    criteria: vec!["测试通过".to_string()],
                },
            },
            status: ResourceStatus {
                phase: Phase::Pending,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        }
    }

    #[test]
    fn task_2_1_assign_without_team() {
        let manager = Principal {
            id: DomainId::new("user-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let assigned = assign_issue(
            &issue(IssueKind::Coding),
            Owner {
                kind: OwnerKind::Agent,
                id: DomainId::new("coder-1"),
            },
            7,
            &manager,
            &grants(7, true),
        )
        .expect("无 Team 配置也应能直接指派");
        let assignment = assigned.spec.assignment.expect("应生成 Assignment");
        assert_eq!(assignment.owner.id.as_str(), "coder-1");
        assert_eq!(assignment.grant_revision, 7);
        assert_eq!(assigned.meta.resource_version, Version(2));
    }

    #[test]
    fn task_2_1_coding_issue_requires_context() {
        let mut invalid = issue(IssueKind::Coding);
        invalid.repository = None;
        let manager = Principal {
            id: DomainId::new("user-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let err = assign_issue(
            &invalid,
            Owner {
                kind: OwnerKind::Agent,
                id: DomainId::new("coder-1"),
            },
            7,
            &manager,
            &grants(7, true),
        )
        .expect_err("缺仓库上下文的 Coding Issue 必须被拒绝");
        assert_eq!(err.reason, DenialReason::MissingProjectContext);
        assert!(!err.reactivation.is_empty());
    }

    mod invariant_i_09 {
        use super::*;

        #[test]
        fn revoked_grant_blocks_dispatch_and_records_current_revision() {
            let assignment = Assignment {
                owner: Owner {
                    kind: OwnerKind::Agent,
                    id: DomainId::new("coder-1"),
                },
                scope: scope(),
                grant_revision: 7,
            };
            let err = authorize(
                &agent("coder-1"),
                &scope(),
                Permission::Dispatch,
                &grants(8, false),
                Some(&assignment),
            )
            .expect_err("撤权后必须拒绝");
            assert_eq!(err.reason, DenialReason::InsufficientPermission);
            assert_eq!(err.actor.as_str(), "coder-1");
            assert!(!err.reactivation.is_empty());
        }

        #[test]
        fn claimed_admin_role_does_not_grant_permission() {
            let err = authorize(
                &agent("coder-1"),
                &scope(),
                Permission::ManageBudget,
                &grants(7, true),
                None,
            )
            .expect_err("自报管理员不得扩权");
            assert_eq!(err.reason, DenialReason::InsufficientPermission);
        }

        #[test]
        fn stale_assignment_is_rejected_after_reissue() {
            let assignment = Assignment {
                owner: Owner {
                    kind: OwnerKind::Agent,
                    id: DomainId::new("coder-1"),
                },
                scope: scope(),
                grant_revision: 5,
            };
            let err = authorize(
                &agent("coder-1"),
                &scope(),
                Permission::Dispatch,
                &grants(7, true),
                Some(&assignment),
            )
            .expect_err("旧 revision 的 Binding 不得认领新工作");
            assert_eq!(err.reason, DenialReason::InsufficientPermission);
            assert_eq!(err.grant_revision, Some(7));
        }
    }

    mod invariant_i_12 {
        use super::*;

        #[test]
        fn feedback_payload_cannot_claim_another_project() {
            let other = Scope {
                project_id: Some(DomainId::new("project-2")),
                ..scope()
            };
            let err = authorize_feedback(
                &scope(),
                &other,
                &agent("coder-1"),
                Permission::Dispatch,
                &grants(7, true),
                None,
            )
            .expect_err("payload 自报项目不得扩权");
            assert_eq!(err.reason, DenialReason::ScopeMismatch);
        }

        #[test]
        fn invocation_bound_scope_is_used_not_payload_scope() {
            let granted = authorize_feedback(
                &scope(),
                &scope(),
                &agent("coder-1"),
                Permission::Dispatch,
                &grants(7, true),
                Some(&Assignment {
                    owner: Owner {
                        kind: OwnerKind::Agent,
                        id: DomainId::new("coder-1"),
                    },
                    scope: scope(),
                    grant_revision: 7,
                }),
            )
            .expect("绑定范围内应通过");
            assert_eq!(granted.actor.as_str(), "coder-1");
        }
    }

    mod task_2_3 {
        use super::*;

        #[test]
        fn default_listen_is_loopback_and_remote_is_rejected() {
            let config = BodConfig::default();
            assert!(deployment_boundary(&config).is_ok());
            assert!(is_loopback_listen("127.0.0.1:0"));
            assert!(is_loopback_listen("[::1]:8080"));
            assert!(!is_loopback_listen("0.0.0.0:8080"));

            let mut remote = config;
            remote.server.listen = "0.0.0.0:8080".to_string();
            let denial = deployment_boundary(&remote).expect_err("非回环监听必须被拒绝");
            assert!(!denial.reactivation.is_empty());
            assert_eq!(denial.scope.company_id.as_str(), "company-default");
        }
    }
}
