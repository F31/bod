//! 方案 §39.17 分工与授权闭环验证：6 个命名 case。
//!
//! 每个 case 读取 `fixtures/work_authority/<case>/case.json`，用真实公开 API 断言
//! **本地授权/状态语义**。真实适配器运行、真实连接器发布与多节点审批仍需 B 轨
//! （逐条注明），本套件不伪造外部成功。

use bod_app::authorization::{
    AcceptanceRequest, ActorRole, EvidenceKind, ReleaseContext, ReleaseTicket, StageAction,
    StageDenial, StageFacts, StageTarget, authorize_stage, check_acceptance, check_release,
};
use bod_app::domain::{DomainId, Grant, Permission, Principal, PrincipalKind, Scope};
use bod_app::feedback_knowledge::{ProfileBook, ProposalDraft, ProposalStatus, SubmitOutcome};
use bod_app::manager_transfer::{
    AppointRequest, CommandRejectionCode, ManagementAppointment, ManagementCommand,
    ManagementCommandSet, ManagementGate, ManagementScope, ManagementTicket, TransferError,
};
use bod_app::notification_channel::{DeliveryReceipt, plan_correction};
use bod_app::release_governance::{
    Deployment, DeploymentEvent, DeploymentState, apply as apply_deployment,
};
use bod_app::runtime_verification::{IndependenceGap, IndependenceInput, judge_independence};
use bod_app::{BodConfig, store::SqliteStore};

fn text<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("fixture 缺少字符串字段 {key}"))
}

fn num(v: &serde_json::Value, key: &str) -> u64 {
    v[key]
        .as_u64()
        .unwrap_or_else(|| panic!("fixture 缺少整数字段 {key}"))
}

fn scope_of(v: &serde_json::Value) -> Scope {
    Scope {
        company_id: DomainId::new(text(v, "company_id")),
        project_id: Some(DomainId::new(text(v, "project_id"))),
        issue_id: v
            .get("issue_id")
            .and_then(|item| item.as_str())
            .map(DomainId::new),
    }
}

fn principal(id: &str) -> Principal {
    Principal {
        id: DomainId::new(id),
        kind: PrincipalKind::User,
        claimed_roles: Vec::new(),
    }
}

fn grant_in(scope: &Scope, id: &str, permission: Permission) -> Grant {
    Grant {
        principal_id: DomainId::new(id),
        scope: scope.clone(),
        permission,
        revision: 1,
        active: true,
    }
}

fn command_set() -> ManagementCommandSet {
    ManagementCommandSet::new(vec![
        ManagementCommand::ReadStatus,
        ManagementCommand::PlanRevise,
        ManagementCommand::AssignTask,
    ])
    .unwrap()
}

fn appoint_request(
    case: &serde_json::Value,
    manager: &str,
    scope: ManagementScope,
) -> AppointRequest {
    AppointRequest::new(
        text(case, "company_id"),
        scope,
        manager,
        text(case, "grant_ref"),
        text(case, "appointed_by"),
        text(case, "appointed_at"),
        Some(text(case, "not_after").to_string()),
        text(case, "audience"),
        command_set(),
    )
}

// ---------------------------------------------------------------------------
// §39.17 case 1: direct_without_manager
// ---------------------------------------------------------------------------
#[test]
fn direct_without_manager() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/work_authority/direct_without_manager/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let revision = num(&case, "revision");
    let member = text(&case, "member");
    let executor = text(&case, "executor");
    let acceptor = text(&case, "acceptor");

    // 无 Team、无 Appointment：只看成员身份与 Grant，成员可批准并直派。
    let facts = StageFacts {
        grants: vec![
            grant_in(&scope, member, Permission::Dispatch),
            grant_in(&scope, acceptor, Permission::Accept),
        ],
        members: vec![(text(&case, "project_id").to_string(), DomainId::new(member))],
        ..StageFacts::default()
    };
    let target = StageTarget::new(scope.clone(), revision).with_assignee(DomainId::new(executor));
    let dispatch = authorize_stage(
        &facts,
        &principal(member),
        StageAction::Dispatch,
        &target,
        text(&case, "now"),
    );
    assert!(dispatch.is_allowed(), "有派发权的成员可直接派发");
    let acceptance = authorize_stage(
        &facts,
        &principal(acceptor),
        StageAction::Accept,
        &target,
        text(&case, "now"),
    );
    assert!(acceptance.is_allowed(), "独立接受人可按指定修订验收");

    let verdict = check_acceptance(
        &facts,
        &AcceptanceRequest {
            actor: DomainId::new(acceptor),
            role: ActorRole::Acceptor,
            revision,
            current_revision: revision,
            evidence: vec![EvidenceKind::TestReport],
        },
        Some(&DomainId::new(executor)),
    );
    assert!(
        matches!(
            verdict,
            bod_app::authorization::AcceptanceVerdict::Advance { .. }
        ),
        "验收按指定修订推进"
    );
    let release = authorize_stage(
        &facts,
        &principal(member),
        StageAction::Release,
        &target,
        text(&case, "now"),
    );
    assert!(
        matches!(
            release,
            bod_app::authorization::StageDecision::Denied {
                reason: StageDenial::NotReleaseOperator,
                ..
            }
        ),
        "直派不生成发布决定"
    );
    // 真实适配器的一次运行与成果由 B 轨真实 Agent SDK 验收；此处只断言直派准入与验收边界。
}

// ---------------------------------------------------------------------------
// §39.17 case 2: advisor_proposal_only
// ---------------------------------------------------------------------------
#[test]
fn advisor_proposal_only() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/work_authority/advisor_proposal_only/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let advisor = text(&case, "advisor");
    let facts = StageFacts {
        members: vec![(
            text(&case, "project_id").to_string(),
            DomainId::new(advisor),
        )],
        ..StageFacts::default()
    };
    let target = StageTarget::new(scope, 1);
    assert!(
        authorize_stage(
            &facts,
            &principal(advisor),
            StageAction::Submit,
            &target,
            "t"
        )
        .is_allowed(),
        "顾问可以提交提案"
    );
    let dispatch = authorize_stage(
        &facts,
        &principal(advisor),
        StageAction::Dispatch,
        &target,
        "t",
    );
    assert!(
        matches!(
            dispatch,
            bod_app::authorization::StageDecision::Denied {
                reason: StageDenial::MissingGrant,
                ..
            }
        ),
        "顾问尝试派发被拒"
    );

    // 提案落 ProfileBook；派发决策账本保持为空（无副作用）。
    let mut book = ProfileBook::default();
    let profile = text(&case, "profile_id");
    let outcome = book.submit(ProposalDraft {
        proposal_id: "proposal-advisor-1".to_string(),
        profile_id: profile.to_string(),
        base_revision: num(&case, "base_revision"),
        field: "suggestion".to_string(),
        value: text(&case, "feedback_text").to_string(),
    });
    assert!(matches!(outcome, SubmitOutcome::Merged { revision: 1, .. }));
    assert_eq!(
        book.proposal("proposal-advisor-1").map(|p| p.status),
        Some(ProposalStatus::Merged)
    );
    assert_eq!(
        book.revision_of("dispatch-ledger"),
        0,
        "工单与账本无新增派发副作用"
    );
    // 真实顾问角色 externally 的配置与界面仍需 B 轨真实产品验收。
}

// ---------------------------------------------------------------------------
// §39.17 case 3: scope_escalation
// ---------------------------------------------------------------------------
#[test]
fn scope_escalation() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/work_authority/scope_escalation/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let old_revision = num(&case, "old_revision");
    let new_revision = num(&case, "new_revision");
    let owner = text(&case, "scope_owner");
    let outsider = text(&case, "unauthorized");
    let executor = text(&case, "executor");

    let facts = StageFacts {
        grants: vec![grant_in(&scope, owner, Permission::ManageScope)],
        ..StageFacts::default()
    };
    let denied = authorize_stage(
        &facts,
        &principal(outsider),
        StageAction::ManageScope,
        &StageTarget::new(scope.clone(), new_revision),
        text(&case, "now"),
    );
    assert!(
        matches!(
            denied,
            bod_app::authorization::StageDecision::Denied {
                reason: StageDenial::MissingGrant,
                ..
            }
        ),
        "无权限者批准范围扩大被拒"
    );
    let approved = authorize_stage(
        &facts,
        &principal(owner),
        StageAction::ManageScope,
        &StageTarget::new(scope.clone(), new_revision),
        text(&case, "now"),
    );
    assert!(approved.is_allowed(), "授权人批准新修订");

    // 同一新修订只允许消费一次：旧修订重放被挡。
    let replay = authorize_stage(
        &facts,
        &principal(owner),
        StageAction::ManageScope,
        &StageTarget::new(scope.clone(), old_revision).with_current_revision(new_revision),
        text(&case, "now"),
    );
    assert!(
        matches!(
            replay,
            bod_app::authorization::StageDecision::Denied {
                reason: StageDenial::StaleRevision,
                ..
            }
        ),
        "旧修订不得重放，范围扩大只恢复一次"
    );

    // 独立执行工作不因范围门禁而停。
    let work = authorize_stage(
        &facts,
        &principal(executor),
        StageAction::Develop,
        &StageTarget::new(scope, new_revision).with_assignee(DomainId::new(executor)),
        text(&case, "now"),
    );
    assert!(work.is_allowed(), "独立工作继续");
    // 真实范围变更引发的跨项目权限重算仍需 B 轨真实组织配置验收。
}

// ---------------------------------------------------------------------------
// §39.17 case 4: promote_in_place
// ---------------------------------------------------------------------------
#[test]
fn promote_in_place() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/work_authority/promote_in_place/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let audience = text(&case, "audience");
    let issue = text(&case, "issue_id");
    let now_ms = num(&case, "now_ms") as i64;
    let scope = ManagementScope::issue_subtree(text(&case, "project_id"), issue);

    // 在途直派任务升级为专项委任：根 Issue、Reservation 与回执 ID 不满足新 scope 就拒绝。
    let scope_key = scope.scope_key();
    assert!(scope_key.contains(issue), "专项委任绑定根 Issue");
    let root_issue = text(&case, "root_issue_id");
    let reservation = text(&case, "reservation_id");
    let receipt_id = text(&case, "receipt_id");
    assert_ne!(root_issue, "");
    assert_ne!(reservation, "");

    let dir = std::env::temp_dir().join(format!(
        "bod-wa-promote-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = SqliteStore::open(&dir.join("bod.db"), &BodConfig::default()).unwrap();
    let appointment = ManagementAppointment::appoint(
        &store,
        appoint_request(&case, text(&case, "manager"), scope.clone()),
        now_ms,
    )
    .unwrap();
    let ticket = ManagementTicket::issue(
        &appointment,
        ManagementCommand::ReadStatus,
        text(&case, "submit_at"),
        text(&case, "not_after"),
    )
    .unwrap();
    let mut gate = ManagementGate::new(&store, company, audience);
    let command_id = text(&case, "command_id");
    let first = gate
        .submit(&ticket, command_id, text(&case, "submit_at"), now_ms)
        .unwrap();
    let repeated = gate
        .submit(&ticket, command_id, text(&case, "submit_at"), now_ms)
        .unwrap();
    assert!(!first.replayed);
    assert!(repeated.replayed, "同一命令键只执行一次，旧指派不重跑");
    assert_eq!(
        store
            .list_management_commands(company, &scope_key)
            .unwrap()
            .len(),
        1
    );

    // 回执 ID 在升级前后不变：更正只带原回执编号。
    let receipt = DeliveryReceipt::submitted(receipt_id, "im-1", "cust-a", "env-a");
    assert_eq!(receipt.receipt_id, receipt_id);
    let correction = plan_correction(&receipt, 2, true);
    assert_eq!(correction.original_receipt_id, receipt_id, "回执 ID 不变");
    // 真实在途任务的 Reservation 与回执迁移仍需 B 轨真实运行时验收。
}

// ---------------------------------------------------------------------------
// §39.17 case 5: revoke_separately
// ---------------------------------------------------------------------------
#[test]
fn revoke_separately() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/work_authority/revoke_separately/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let audience = text(&case, "audience");
    let now_ms = num(&case, "now_ms") as i64;
    let scope = ManagementScope::project(text(&case, "project_id"));

    let dir = std::env::temp_dir().join(format!(
        "bod-wa-revoke-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = SqliteStore::open(&dir.join("bod.db"), &BodConfig::default()).unwrap();
    let mut a = ManagementAppointment::appoint(
        &store,
        appoint_request(&case, text(&case, "manager_a"), scope.clone()),
        now_ms,
    )
    .unwrap();
    let old_ticket = ManagementTicket::issue(
        &a,
        ManagementCommand::ReadStatus,
        text(&case, "submit_at"),
        text(&case, "not_after"),
    )
    .unwrap();
    let mut gate = ManagementGate::new(&store, company, audience);
    let before = gate
        .submit(
            &old_ticket,
            "cmd-before-revoke",
            text(&case, "submit_at"),
            now_ms,
        )
        .unwrap();
    assert_eq!(before.authority_epoch, 1, "切换前旧票有效（对照组）");

    a.revoke(&store, text(&case, "revoke_at"), a.version, now_ms)
        .unwrap();
    let b = ManagementAppointment::appoint(
        &store,
        appoint_request(&case, text(&case, "manager_b"), scope.clone()),
        now_ms,
    )
    .unwrap();
    assert!(b.authority_epoch > a.authority_epoch, "epoch 单调不回退");

    let err = gate
        .submit(
            &old_ticket,
            "cmd-after-revoke",
            text(&case, "submit_at"),
            now_ms,
        )
        .unwrap_err();
    match err {
        TransferError::Rejected {
            code: CommandRejectionCode::StaleAppointment,
            ..
        } => {}
        other => panic!("期望旧 epoch 被拒，实际 {other:?}"),
    }
    let head = store
        .get_scope_head(company, &scope.scope_key())
        .unwrap()
        .unwrap();
    assert_eq!(
        head.current_appointment_id, b.appointment_id,
        "头指针仍指向新委任"
    );

    // 独立授权的工作继续： Develop 由被指派执行者完成。
    let dev_scope = Scope {
        company_id: DomainId::new(company),
        project_id: Some(DomainId::new(text(&case, "project_id"))),
        issue_id: None,
    };
    let facts = StageFacts::default();
    let work = authorize_stage(
        &facts,
        &principal(text(&case, "independent_executor")),
        StageAction::Develop,
        &StageTarget::new(dev_scope, 1)
            .with_assignee(DomainId::new(text(&case, "independent_executor"))),
        "t",
    );
    assert!(work.is_allowed(), "有独立授权的工作继续");

    // 未知外部结果先对账：Unknown 不允许自动重发。
    let mut deployment = Deployment::new(
        "dep-revoke",
        "plan-revoke",
        text(&case, "invocation_id"),
        "business-revoke",
    );
    deployment.mark_committed();
    apply_deployment(
        &mut deployment,
        DeploymentEvent::DispatchSent {
            at: text(&case, "submit_at").to_string(),
        },
    )
    .unwrap();
    apply_deployment(
        &mut deployment,
        DeploymentEvent::AckLost {
            at: text(&case, "submit_at").to_string(),
        },
    )
    .unwrap();
    assert_eq!(deployment.state, DeploymentState::Unknown);
    assert!(!deployment.can_resend(), "未知外部结果禁止自动重发");
    assert_eq!(
        deployment.unknown_query_key(),
        Some(text(&case, "invocation_id"))
    );
    // 真实连接器回调与接管者换号仍需 B 轨真实渠道验收。
}

// ---------------------------------------------------------------------------
// §39.17 case 6: separate_release
// ---------------------------------------------------------------------------
#[test]
fn separate_release() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/work_authority/separate_release/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let revision = num(&case, "revision");
    let acceptor = text(&case, "acceptor");
    let executor = text(&case, "executor");
    let tester = text(&case, "tester");
    let release_op = text(&case, "release_operator");

    let facts = StageFacts {
        grants: vec![grant_in(&scope, acceptor, Permission::Accept)],
        release_operators: vec![DomainId::new(release_op)],
        ..StageFacts::default()
    };
    let target = StageTarget::new(scope.clone(), revision).with_assignee(DomainId::new(executor));

    // 接受制品与批准发布是两个决定，由不同主体作出。
    let accept = authorize_stage(
        &facts,
        &principal(acceptor),
        StageAction::Accept,
        &target,
        "t",
    );
    assert!(accept.is_allowed(), "接受人可验收制品");
    let release = authorize_stage(
        &facts,
        &principal(release_op),
        StageAction::Release,
        &target,
        "t",
    );
    assert!(release.is_allowed(), "发布操作人可批准发布");
    let self_release = authorize_stage(
        &facts,
        &principal(acceptor),
        StageAction::Release,
        &target,
        "t",
    );
    assert!(
        matches!(
            self_release,
            bod_app::authorization::StageDecision::Denied {
                reason: StageDenial::NotReleaseOperator,
                ..
            }
        ),
        "同一主体不能复核自己的发布决定"
    );

    let acceptance = check_acceptance(
        &facts,
        &AcceptanceRequest {
            actor: DomainId::new(acceptor),
            role: ActorRole::Acceptor,
            revision,
            current_revision: revision,
            evidence: vec![EvidenceKind::CustomerSignoff],
        },
        Some(&DomainId::new(executor)),
    );
    assert!(
        matches!(
            acceptance,
            bod_app::authorization::AcceptanceVerdict::Advance { .. }
        ),
        "接受决定登记"
    );
    let tester_accept = check_acceptance(
        &facts,
        &AcceptanceRequest {
            actor: DomainId::new(tester),
            role: ActorRole::Tester,
            revision,
            current_revision: revision,
            evidence: vec![EvidenceKind::TestReport],
        },
        Some(&DomainId::new(executor)),
    );
    assert!(
        matches!(
            tester_accept,
            bod_app::authorization::AcceptanceVerdict::Refused(
                bod_app::authorization::AcceptanceRefusal::TesterCannotAccept
            )
        ),
        "测试人只交证据不接受"
    );

    // 双人政策下"只换人"的复核不构成独立证据。
    let verdict = judge_independence(&IndependenceInput::self_generated().with_author_separated());
    assert!(!verdict.is_independent());
    assert!(
        matches!(
            verdict,
            bod_app::runtime_verification::IndependenceVerdict::Insufficient {
                gap: IndependenceGap::RoleRenameOnly,
                ..
            }
        ),
        "同主体/同来源复核被拒"
    );

    // 票证钉死的账号与目标变更时拦下，connector_calls 必须为 0（真实外发由 B 轨验收）。
    let ticket = ReleaseTicket {
        environment: text(&case, "project_id").to_string(),
        artifact_digest: "sha256:artifact".to_string(),
        params_digest: "sha256:params".to_string(),
        account_id: release_op.to_string(),
        connector_id: "connector-1".to_string(),
        approved_until: "2026-11-22T00:00:00Z".to_string(),
    };
    let context = ReleaseContext {
        environment: text(&case, "project_id").to_string(),
        artifact_digest: "sha256:artifact".to_string(),
        params_digest: "sha256:params".to_string(),
        account_id: "different-account".to_string(),
        connector_id: "connector-1".to_string(),
        now: text(&case, "now").to_string(),
        authorization_active: true,
    };
    match check_release(&ticket, &context) {
        bod_app::authorization::ReleaseVerdict::Blocked {
            reason: bod_app::authorization::ReleaseBlock::AccountChanged,
            connector_calls: 0,
        } => {}
        other => panic!("期望账号变更拦下且零外发，实际 {other:?}"),
    }
    // 真实双人政策审批流与客户环境发布仍需 B 轨逐客户核对。
}
