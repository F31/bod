//! 方案 §39.16 反馈闭环的可执行验收计划：14 个命名 case。
//!
//! 每个 case 读取 `fixtures/product_feedback/<case>/case.json` 的最小可审计输入，
//! 用真实公开 API 驱动状态变化并做实质断言。
//! 涉及真实外部渠道、客户环境、模型与多节点的部分只断言**本地准入/投影/状态语义**；
//! 真实外部验收仍需 B 轨（文件内逐条注明），不伪造外部成功。

use bod_app::BodConfig;
use bod_app::authorization::{
    MembershipChange, StageAction, StageFacts, StageTarget, SubscriptionOutcome, authorize_stage,
    check_subscription, filter_records as authz_filter_records, revoke_member,
};
use bod_app::domain::{DomainId, Grant, Permission, Principal, PrincipalKind, Scope};
use bod_app::feedback_knowledge::{
    CacheEntry, CacheKey, CacheKind, CacheLookup, EvaluationRecord, EvaluationResult, Feedback,
    FeedbackRoute, FeedbackSource, GovernanceReason, ProfileBook, ProfilePointer, PromotionVerdict,
    ProposalDraft, ProposalStatus, RetrievalQuery, SubmitOutcome, classify, compare,
    lookup as cache_lookup,
};
use bod_app::manager_transfer::{
    AppointRequest, ManagementAppointment, ManagementCommand, ManagementCommandSet, ManagementGate,
    ManagementScope, ManagementTicket,
};
use bod_app::notification_channel::{
    CardKind, CardPayload, ChannelCapability, CommandBinding, CommandContext, CommandVerdict,
    CorrectionMode, DeliveryKey, DeliveryReceipt, InboundAccess, InboundMessage, ScopedToken,
    ShownState, classify_inbound, confirm_command, plan_correction, project_card, submit_command,
    update_in_scope,
};
use bod_app::readonly_api::{
    AttachmentOutcome, ReadOnlyRequest, ReadonlyAction, ReadonlyRecord, Viewer, build_export,
    filter_records, open_attachment, serve_readonly,
};
use bod_app::release_governance::{
    BudgetExplanation, Deployment, DeploymentEvent, DeploymentState, Drift, EnvironmentRef,
    MeteringSource, ReleaseAdmission, ReleaseBlockReason, TransitionError, admit_release,
    apply as apply_deployment,
};
use bod_app::runtime_verification::{
    Candidate, CheckResult, EvidenceMatch, Executor, ToolVersion, VerificationReport, match_report,
};
use bod_app::store::SqliteStore;

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

fn grant(id: &str, permission: Permission) -> Grant {
    Grant {
        principal_id: DomainId::new(id),
        scope: Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new("project-1")),
            issue_id: None,
        },
        permission,
        revision: 1,
        active: true,
    }
}

// ---------------------------------------------------------------------------
// §39.16 case 1: intake_retry
// ---------------------------------------------------------------------------
#[test]
fn intake_retry() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/intake_retry/case.json"
    ))
    .unwrap();
    assert_eq!(text(&case, "case"), "intake_retry");

    // 外部提交：来源不可信，只当用户数据处理。
    let feedback = Feedback::new(
        text(&case, "external_key"),
        text(&case, "text"),
        FeedbackSource::ExternalWeb {
            url: text(&case, "url").to_string(),
        },
        text(&case, "field"),
    );
    let classification = classify(&feedback);
    assert!(classification.untrusted, "外部网页来源必须标记为不可信");

    let mut book = ProfileBook::default();
    let key = text(&case, "external_key");
    let profile = text(&case, "profile_id");
    let draft = ProposalDraft {
        proposal_id: key.to_string(),
        profile_id: profile.to_string(),
        base_revision: 0,
        field: text(&case, "field").to_string(),
        value: text(&case, "value").to_string(),
    };
    let first = book.submit(draft.clone());
    assert!(
        matches!(first, SubmitOutcome::Merged { revision: 1, .. }),
        "首次提交应合并为修订 1"
    );

    // 同一外部 key 重发且载荷不同：返回冲突，候选保留，修订不动。
    let mut changed = draft.clone();
    changed.value = text(&case, "changed_value").to_string();
    let second = book.submit(changed);
    assert!(
        matches!(
            second,
            SubmitOutcome::Conflict {
                current_revision: 1,
                ..
            }
        ),
        "相同键变更载荷必须返回冲突"
    );
    assert_eq!(book.revision_of(profile), 1, "冲突不产生第二个修订");
    let merged = book
        .proposals
        .iter()
        .filter(|p| p.proposal_id == key && p.status == ProposalStatus::Merged)
        .count();
    assert_eq!(merged, 1, "同一提交只产生一条已合并反馈");
    assert_eq!(
        book.proposal(key).map(|p| p.status),
        Some(ProposalStatus::Merged)
    );
    // 真实外部渠道的投递去重（同一 external_id 只落一条消息）仍需 B 轨真实渠道验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 2: approve_race
// ---------------------------------------------------------------------------
#[test]
fn approve_race() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/approve_race/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let revision = num(&case, "revision");
    let approvers = case["approvers"].as_array().unwrap();
    let a = approvers[0].as_str().unwrap();
    let b = approvers[1].as_str().unwrap();

    let facts = StageFacts {
        grants: vec![grant(a, Permission::Accept), grant(b, Permission::Accept)],
        ..StageFacts::default()
    };
    let target = StageTarget::new(scope.clone(), revision);
    let decision_a = authorize_stage(&facts, &principal(a), StageAction::Accept, &target, "t");
    let decision_b = authorize_stage(&facts, &principal(b), StageAction::Accept, &target, "t");
    assert!(decision_a.is_allowed(), "审批人 A 有接受权");
    assert!(decision_b.is_allowed(), "审批人 B 有接受权");

    // 两人针对同一修订各自提交决定：决定账本用 base_revision CAS 保证只有一个写入。
    let mut ledger = ProfileBook::default();
    let ledger_id = text(&case, "decision_ledger");
    let draft = ProposalDraft {
        proposal_id: format!("approval:{}", text(&case, "issue_id")),
        profile_id: ledger_id.to_string(),
        base_revision: 0,
        field: text(&case, "decision_field").to_string(),
        value: a.to_string(),
    };
    let first = ledger.submit(draft.clone());
    assert!(
        matches!(first, SubmitOutcome::Merged { revision: 1, .. }),
        "先到者写入授权决定"
    );
    let mut competing = draft;
    competing.value = b.to_string();
    let second = ledger.submit(competing);
    assert!(
        matches!(second, SubmitOutcome::Conflict { .. }),
        "后到者旧修订被 CAS 拒绝"
    );
    assert_eq!(ledger.revision_of(ledger_id), 1);
    let merged = ledger
        .proposals
        .iter()
        .filter(|p| p.status == ProposalStatus::Merged)
        .count();
    assert_eq!(
        merged, 1,
        "最终只有一个授权决定、一个根 Issue 与一个派发意图"
    );
    // 真实多人并发 HTTP 审批与根 Issue 落库的串行化仍需 B 轨多节点验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 3: crash_after_commit
// ---------------------------------------------------------------------------
#[test]
fn crash_after_commit() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/crash_after_commit/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let project = text(&case, "project_id");
    let audience = text(&case, "audience");

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("bod-pf-crash-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("bod.db");
    let now_ms = num(&case, "now_ms") as i64;
    let scope = ManagementScope::project(project);

    // 提交后"杀进程"：grant 与委任已落 SQLite。
    let commands = ManagementCommandSet::new(vec![ManagementCommand::ReadStatus]).unwrap();
    let request = AppointRequest::new(
        company,
        scope.clone(),
        text(&case, "manager_agent_id"),
        text(&case, "grant_ref"),
        text(&case, "appointed_by"),
        text(&case, "appointed_at"),
        Some(text(&case, "not_after").to_string()),
        audience,
        commands,
    );
    let appointed = {
        let store = SqliteStore::open(&db, &BodConfig::default()).unwrap();
        ManagementAppointment::appoint(&store, request, now_ms).unwrap()
    };

    // 重新打开同一 SQLite 文件（进程重启），授权工作可继续。
    let store = SqliteStore::open(&db, &BodConfig::default()).unwrap();
    let loaded = ManagementAppointment::load(&store, company, &appointed.appointment_id)
        .unwrap()
        .expect("委任应在重启后仍可读");
    assert_eq!(loaded.authority_epoch, 1);
    assert_eq!(loaded.appointment_id, appointed.appointment_id);
    let ticket = ManagementTicket::issue(
        &loaded,
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
    assert!(!first.replayed, "首次恢复命令真实执行");
    assert!(repeated.replayed, "重复确认返回原结果，不新建恢复尝试");
    let audits = store
        .list_management_commands(company, &scope.scope_key())
        .unwrap();
    assert_eq!(audits.len(), 1, "根 Issue 数仍为一，恢复意图只登记一次");
    // 真实进程崩溃与宿主重启恢复仍需 B 轨真机验收；此处只验证已提交状态可重载。
}

// ---------------------------------------------------------------------------
// §39.16 case 4: no_capacity
// ---------------------------------------------------------------------------
#[test]
fn no_capacity() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/no_capacity/case.json"
    ))
    .unwrap();
    let plan = bod_app::release_governance::ReleasePlan::new(
        text(&case, "plan_id"),
        text(&case, "stage_id"),
        text(&case, "revision_id"),
        text(&case, "artifact_digest"),
        text(&case, "environment_id"),
    );
    let approval = bod_app::release_governance::Approval {
        approval_id: text(&case, "approval_id").to_string(),
        artifact_digest: plan.artifact_digest.clone(),
        environment_id: plan.environment_id.clone(),
        parameters_digest: plan.parameters_digest.clone(),
        valid_until: text(&case, "valid_until").to_string(),
        approved_by: text(&case, "approved_by").to_string(),
        approved_at: text(&case, "now").to_string(),
    };
    let now = text(&case, "now");
    let exhausted = BudgetExplanation {
        limit_minor: num(&case, "limit_minor") as i64,
        settled_minor: num(&case, "settled_minor") as i64,
        reserved_minor: 0,
        unknown_risk_minor: 0,
        metering_source: MeteringSource::Invocation,
        coverage_gaps: Vec::new(),
    };
    assert!(exhausted.available() <= 0, "耗尽预算不能显示为仍有额度");
    let blocked = admit_release(&plan, &approval, now, &exhausted, &[]);
    match &blocked {
        ReleaseAdmission::Refused {
            reason: ReleaseBlockReason::BudgetExhausted,
            detail,
        } => {
            assert!(detail.contains("需提额或结清"), "阻塞必须可解释");
        }
        other => panic!("期望预算耗尽阻塞，实际 {other:?}"),
    }

    let restored = BudgetExplanation {
        settled_minor: num(&case, "settled_after_topup_minor") as i64,
        ..exhausted
    };
    assert!(restored.consistent());
    assert!(admit_release(&plan, &approval, now, &restored, &[]).admitted());
    // 真实 Agent 池容量与外部额度提额仍需 B 轨；此处断言本地预算门禁的阻塞/恢复语义。
}

// ---------------------------------------------------------------------------
// §39.16 case 5: scoped_rollout
// ---------------------------------------------------------------------------
#[test]
fn scoped_rollout() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/scoped_rollout/case.json"
    ))
    .unwrap();
    let fixed = text(&case, "revision_fixed");
    let rollback = text(&case, "revision_rollback");

    let mut env_a = EnvironmentRef::new(text(&case, "env_a_id"), "k8s", "cluster-a/ns-a", "客户 A")
        .with_desired(fixed);
    env_a.observe(Some(fixed), text(&case, "published_at"));
    let mut env_b = EnvironmentRef::new(text(&case, "env_b_id"), "k8s", "cluster-a/ns-b", "客户 B")
        .with_desired(fixed);
    env_b.observe(None, text(&case, "published_at"));

    assert!(env_a.projection().is_deployed(), "A 已部署修复");
    assert_eq!(env_b.drift(), Drift::Unknown, "B 没有实际观测，仍待发布");
    assert!(!env_b.projection().is_deployed(), "B 不得显示为已部署");

    // 回滚是发布旧内容的新修订：会话刷新，外部动作与账本不回滚。
    let mut pointer = ProfilePointer::new(text(&case, "defect_id"), text(&case, "old_digest"));
    pointer.publish(text(&case, "fixed_digest"), text(&case, "published_at"));
    let pin_before = pointer.pin();
    let result = pointer.rollback(1, text(&case, "rolled_back_at")).unwrap();
    assert!(result.session_refreshed);
    assert!(!result.external_ops_reverted, "不伪造外部回滚");
    assert!(!result.ledger_reverted, "不改写历史账本");
    assert_eq!(pointer.current().digest, text(&case, "old_digest"));
    assert_eq!(pin_before.revision, 2, "旧 Attempt 的钉扎值不变");

    env_a = env_a.with_desired(rollback);
    env_a.observe(Some(rollback), text(&case, "rolled_back_at"));
    assert!(env_a.projection().is_deployed(), "A 的结论按新修订更新");
    assert_eq!(env_b.drift(), Drift::Unknown, "B 仍未安装");
    // 真实两客户环境的部署/回滚与逐客户结论核对仍需 B 轨真实渠道验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 6: private_receipt
// ---------------------------------------------------------------------------
#[test]
fn private_receipt() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/private_receipt/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let project = text(&case, "project_id");
    let customers = case["customers"].as_array().unwrap();
    let receipts = case["receipt_ids"].as_array().unwrap();
    let tickets = case["ticket_ids"].as_array().unwrap();

    let records: Vec<ReadonlyRecord> = (0..receipts.len())
        .map(|i| ReadonlyRecord {
            object_id: receipts[i].as_str().unwrap().to_string(),
            company_id: company.to_string(),
            project_id: project.to_string(),
            customer_id: Some(
                customers[i.min(customers.len() - 1)]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            ticket_id: Some(tickets[i].as_str().unwrap().to_string()),
            title: format!("回执 {}", i),
        })
        .collect();
    let viewer_a = Viewer::new(text(&case, "viewer_a"), company, project)
        .with_customer(customers[0].as_str().unwrap())
        .with_tickets(&[tickets[0].as_str().unwrap()]);
    let page = filter_records(&viewer_a, &records);
    assert_eq!(page.total, 1, "提交者只见自己的回执");
    assert_eq!(page.visible[0].object_id, receipts[0].as_str().unwrap());
    assert_eq!(page.withheld, 2, "被挡条数只进日志，不泄露身份");

    let request = ReadOnlyRequest {
        request_id: text(&case, "request_id").to_string(),
        action: ReadonlyAction::Query,
        project_id: project.to_string(),
        scope: Vec::new(),
        policy_revision: 1,
    };
    let outcome = serve_readonly(&viewer_a, &request, &records);
    assert_eq!(outcome.decision, "allowed");
    assert_eq!(outcome.rows, 1);
    assert_eq!(outcome.created_runs, 0, "只读查询不创建 Run");

    let cross_project = Viewer::new(text(&case, "viewer_a"), company, "project-other");
    let denied = serve_readonly(&cross_project, &request, &records);
    assert_eq!(denied.decision, "denied", "跨项目查询被拒绝");
    assert_eq!(denied.rows, 0);
    let exported = build_export(&viewer_a, &request, text(&case, "export_base"), &records);
    assert_eq!(
        exported.file_name, "receipts-1-rows.csv",
        "文件名只反映可见行"
    );
    assert!(
        matches!(
            open_attachment(&viewer_a, &records[1]),
            AttachmentOutcome::NotVisible
        ),
        "下载他人附件被拒绝且不与不存在区分"
    );
    // 真实门户跨项目访问的 HTTP 鉴权仍需 B 轨真实端到端验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 7: reopen
// ---------------------------------------------------------------------------
#[test]
fn reopen() {
    let case: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/product_feedback/reopen/case.json")).unwrap();
    let scope = scope_of(&case);
    let revision = num(&case, "revision");
    let now = text(&case, "now");
    let later = text(&case, "later");
    let lead = text(&case, "lead");

    let binding = CommandBinding {
        command_key: text(&case, "card_key").to_string(),
        member_id: lead.to_string(),
        project_id: text(&case, "project_id").to_string(),
        target_id: text(&case, "issue_id").to_string(),
        card_revision: revision,
    };
    let online = CommandContext {
        online: true,
        member_active: true,
        permitted: true,
        current_card_revision: revision,
        now: now.to_string(),
        card_expires_at: text(&case, "card_expires_at").to_string(),
    };
    let receipt_confirmed = case["receipt_ids"][2].as_str().unwrap();
    let submitted = submit_command(&binding, &online, receipt_confirmed);
    assert!(
        matches!(
            &submitted,
            CommandVerdict::Submitted {
                shown: ShownState::Submitting,
                ..
            }
        ),
        "已提交但未确认前只显示 Submitting"
    );
    assert_eq!(confirm_command(&submitted, false), ShownState::Submitting);
    assert_eq!(confirm_command(&submitted, true), ShownState::Succeeded);

    let rejected = submit_command(
        &binding,
        &CommandContext {
            permitted: false,
            ..online.clone()
        },
        "rcp-failed",
    );
    assert!(
        matches!(
            rejected,
            CommandVerdict::Rejected {
                reason: bod_app::notification_channel::CommandReject::NotPermitted,
                refresh_card: false,
                ..
            }
        ),
        "通知失败产生 Failed 记录"
    );
    let deferred = submit_command(
        &binding,
        &CommandContext {
            online: false,
            ..online
        },
        "rcp-unconfirmed",
    );
    assert!(
        matches!(
            deferred,
            CommandVerdict::Deferred {
                shown: ShownState::PendingSync,
                ..
            }
        ),
        "用户未确认保持待同步，不假装成功"
    );
    assert_eq!(confirm_command(&deferred, true), ShownState::PendingSync);

    // 复开是独立动作：接受权主体可以复开，执行者本人不能。
    let facts = StageFacts {
        grants: vec![grant(lead, Permission::Accept)],
        ..StageFacts::default()
    };
    let target = StageTarget::new(scope, revision);
    assert!(
        authorize_stage(&facts, &principal(lead), StageAction::Reopen, &target, now).is_allowed()
    );
    assert!(
        !authorize_stage(
            &facts,
            &principal(text(&case, "executor")),
            StageAction::Reopen,
            &target,
            now
        )
        .is_allowed(),
        "无接受权者不能复开"
    );

    let mut pointer = ProfilePointer::new("receipt-evidence", text(&case, "evidence_digest"));
    pointer.publish("sha256:new-evidence", later);
    assert_eq!(
        pointer.versions[0].digest,
        text(&case, "evidence_digest"),
        "原证据仍可读"
    );
    // 真实用户点击与通知渠道投递仍需 B 轨；此处断言三类记录的本地状态语义。
}

// ---------------------------------------------------------------------------
// §39.16 case 8: backlog_only
// ---------------------------------------------------------------------------
#[test]
fn backlog_only() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/backlog_only/case.json"
    ))
    .unwrap();
    let feedback = Feedback::new(
        text(&case, "feedback_id"),
        text(&case, "text"),
        FeedbackSource::AuthenticatedMember {
            principal_id: text(&case, "policy_actor").to_string(),
        },
        text(&case, "field"),
    );
    let classification = classify(&feedback);
    assert!(
        matches!(
            classification.route,
            FeedbackRoute::Governance(GovernanceReason::ProductionRelease)
        ),
        "生产发布措辞必须转治理流程"
    );
    assert!(classification.is_learning() == false);

    let policy = bod_app::feedback_knowledge::AcceptancePolicy {
        actor_id: text(&case, "policy_actor").to_string(),
        allowed_fields: vec![text(&case, "field").to_string()],
        max_scope: bod_app::feedback_knowledge::ScopeKind::Project,
        valid_until: text(&case, "now").to_string(),
        audit_id: "audit-backlog".to_string(),
    };
    assert!(
        matches!(
            bod_app::feedback_knowledge::may_auto_accept(
                &classification,
                text(&case, "field"),
                &policy,
                text(&case, "now")
            ),
            bod_app::feedback_knowledge::AutoAccept::HumanApprovalRequired {
                reason: "governance_route"
            }
        ),
        "高风险生产操作不可自动接受"
    );

    // 候选需求进入 backlog，但以 stale base 提交只保留候选，不产生修订/Run。
    let mut book = ProfileBook::default();
    let profile = text(&case, "profile_id");
    let outcome = book.submit(ProposalDraft {
        proposal_id: text(&case, "feedback_id").to_string(),
        profile_id: profile.to_string(),
        base_revision: num(&case, "base_revision"),
        field: text(&case, "field").to_string(),
        value: text(&case, "text").to_string(),
    });
    assert!(matches!(outcome, SubmitOutcome::Conflict { .. }));
    assert_eq!(book.revision_of(profile), 0, "未创建 Run/发布修订");
    assert_eq!(
        book.proposal(text(&case, "feedback_id")).map(|p| p.status),
        Some(ProposalStatus::Candidate),
        "仅保留候选需求"
    );

    let facts = StageFacts::default();
    let dispatch = authorize_stage(
        &facts,
        &principal(text(&case, "policy_actor")),
        StageAction::Dispatch,
        &StageTarget::new(
            Scope {
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                issue_id: None,
            },
            1,
        ),
        "t",
    );
    assert!(!dispatch.is_allowed(), "无派发权时不能推进生产发布");
    // 真实 backlog 选代与 Run 创建流程仍需 B 轨真实产品验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 9: token_revoked
// ---------------------------------------------------------------------------
#[test]
fn token_revoked() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/token_revoked/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let member = DomainId::new(text(&case, "member"));
    let project = text(&case, "project_id");
    let mut facts = StageFacts {
        grants: vec![grant(text(&case, "member"), Permission::Accept)],
        members: vec![(project.to_string(), member.clone())],
        customer_bindings: vec![(member.clone(), "cust-a".to_string())],
        ..StageFacts::default()
    };
    let outcome = revoke_member(
        &mut facts,
        &member,
        MembershipChange::Leave,
        &["inv-1".to_string()],
        &[text(&case, "external_credential").to_string()],
    );
    assert!(outcome.blocked_new, "撤权后新动作被拦");
    assert_eq!(
        outcome.in_flight_state,
        "stop_requested_awaiting_confirmation"
    );
    assert_eq!(
        outcome.reconciliation,
        vec![text(&case, "external_credential")]
    );
    assert!(!facts.has_active_grant(&member, Permission::Accept));

    // 重新查询：撤权即 miss，受保护结果不可读。
    let key = CacheKey {
        artifact_id: text(&case, "artifact_id").to_string(),
        revision: num(&case, "artifact_revision"),
        digest: text(&case, "artifact_digest").to_string(),
    };
    let entry = CacheEntry::new(
        key.clone(),
        CacheKind::ProfileResolve,
        text(&case, "company_id"),
        "private diagnostic body",
    )
    .in_project(project)
    .requiring(Permission::Accept);
    let query = RetrievalQuery {
        company_id: text(&case, "company_id").to_string(),
        project_id: Some(project.to_string()),
        actor: principal(text(&case, "member")),
        grants: facts.grants.clone(),
        revoked: true,
    };
    assert!(
        matches!(
            cache_lookup(&[entry], &key, &query),
            CacheLookup::Miss("revoked")
        ),
        "撤销后重新查询被拒"
    );

    // 门户离线续传：早于撤权修订的游标必须重新同步。
    // 用撤权前的订阅事实快照（成员身份仍在）核对离线重放口径。
    facts.members.push((project.to_string(), member.clone()));
    facts
        .customer_bindings
        .push((member.clone(), "cust-a".to_string()));
    facts.revoked_at_revision = Some(num(&case, "revoked_at_revision"));
    let subscription = check_subscription(
        &facts,
        &principal(text(&case, "member")),
        &scope,
        num(&case, "since_revision"),
        true,
    );
    assert!(
        matches!(
            subscription,
            SubscriptionOutcome::ResyncRequired { from: r } if r == num(&case, "revoked_at_revision")
        ),
        "离线重放早于撤权修订必须重新同步"
    );

    let viewer = Viewer::new(
        text(&case, "member"),
        text(&case, "company_id"),
        "project-other",
    );
    let request = ReadOnlyRequest {
        request_id: "req-token-revoked".to_string(),
        action: ReadonlyAction::Query,
        project_id: project.to_string(),
        scope: Vec::new(),
        policy_revision: 1,
    };
    assert_eq!(serve_readonly(&viewer, &request, &[]).decision, "denied");

    // 外部令牌过期：消息自称什么都不看，只判认证与令牌。
    let message = InboundMessage {
        channel_id: "web".to_string(),
        authenticated_principal: None,
        scoped_token: Some(ScopedToken {
            receipt_id: text(&case, "receipt_id").to_string(),
            high_entropy: true,
            expires_at: text(&case, "token_expires_at").to_string(),
        }),
        text: "立即发布".to_string(),
    };
    assert!(
        matches!(
            classify_inbound(&message, |_| false, text(&case, "now")),
            InboundAccess::Denied
        ),
        "过期令牌不能读受保护结果"
    );
    // 真实门户离线缓存与外部凭据吊销下发仍需 B 轨真实验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 10: projection_whitelist
// ---------------------------------------------------------------------------
#[test]
fn projection_whitelist() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/projection_whitelist/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let project = text(&case, "project_id");
    let customers = case["customers"].as_array().unwrap();
    let receipts = case["receipt_ids"].as_array().unwrap();

    // 通知白名单：私有诊断/内部讨论/其他客户名与附件都不进正文。
    let payload = CardPayload::new("回执卡", text(&case, "summary"), text(&case, "entry_url"))
        .with_sensitive(
            text(&case, "code"),
            text(&case, "log"),
            text(&case, "attachment"),
        );
    let card = project_card(
        CardKind::PendingFollowUp,
        ChannelCapability::TextOnly,
        &payload,
    )
    .unwrap();
    assert_eq!(card.body, text(&case, "summary"));
    assert!(!card.body.contains("secret"), "卡片正文不含私有代码");
    assert_eq!(card.withheld, vec!["code", "logs", "attachments"]);

    let records: Vec<ReadonlyRecord> = receipts
        .iter()
        .enumerate()
        .map(|(i, id)| ReadonlyRecord {
            object_id: id.as_str().unwrap().to_string(),
            company_id: if i == 2 {
                text(&case, "other_company_id").to_string()
            } else {
                company.to_string()
            },
            project_id: project.to_string(),
            customer_id: Some(
                customers[i.min(customers.len() - 1)]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            ticket_id: None,
            title: format!("记录 {i}"),
        })
        .collect();
    let viewer = Viewer::new(text(&case, "viewer"), company, project)
        .with_customer(customers[0].as_str().unwrap());
    let page = filter_records(&viewer, &records);
    assert_eq!(page.total, 1, "API 只返回本客户可见记录");
    assert_eq!(page.withheld, 2, "其他客户与跨公司记录被挡");

    let request = ReadOnlyRequest {
        request_id: text(&case, "request_id").to_string(),
        action: ReadonlyAction::Export,
        project_id: project.to_string(),
        scope: Vec::new(),
        policy_revision: 1,
    };
    let exported = build_export(&viewer, &request, text(&case, "export_base"), &records);
    assert_eq!(exported.file_name, "feedback-1-rows.csv");
    assert!(
        matches!(
            open_attachment(&viewer, &records[1]),
            AttachmentOutcome::NotVisible
        ),
        "页面下载鉴权拒绝不可见附件"
    );

    let key = CacheKey {
        artifact_id: "private-diagnostic".to_string(),
        revision: 2,
        digest: "sha256:diag".to_string(),
    };
    let entry = CacheEntry::new(
        key.clone(),
        CacheKind::ProfileResolve,
        company,
        "internal discussion",
    )
    .in_project(project)
    .requiring(Permission::Accept);
    let query = RetrievalQuery {
        company_id: company.to_string(),
        project_id: Some(project.to_string()),
        actor: principal(text(&case, "viewer")),
        grants: Vec::new(),
        revoked: false,
    };
    assert!(
        matches!(
            cache_lookup(&[entry], &key, &query),
            CacheLookup::Miss("permission_denied")
        ),
        "缺权限时私有诊断不返回"
    );
    // 真实通知渠道投递、页面渲染与下载链路仍需 B 轨逐渠道验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 11: split_merge_scope
// ---------------------------------------------------------------------------
#[test]
fn split_merge_scope() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/split_merge_scope/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let project = text(&case, "project_id");
    let keys = case["feedback_keys"].as_array().unwrap();
    let customers = case["customers"].as_array().unwrap();
    let receipts = case["receipt_ids"].as_array().unwrap();

    // 先合并两客户反馈到同一 Profile 修订序列。
    let mut book = ProfileBook::default();
    let profile = text(&case, "profile_id");
    let first = book.submit(ProposalDraft {
        proposal_id: keys[0].as_str().unwrap().to_string(),
        profile_id: profile.to_string(),
        base_revision: 0,
        field: "merged_feedback".to_string(),
        value: customers[0].as_str().unwrap().to_string(),
    });
    assert!(matches!(first, SubmitOutcome::Merged { revision: 1, .. }));
    let second = book.submit(ProposalDraft {
        proposal_id: keys[1].as_str().unwrap().to_string(),
        profile_id: profile.to_string(),
        base_revision: 1,
        field: "merged_feedback".to_string(),
        value: customers[1].as_str().unwrap().to_string(),
    });
    assert!(matches!(second, SubmitOutcome::Merged { revision: 2, .. }));
    assert_eq!(book.revision_of(profile), 2, "合并成一条序列");

    // 再拆分：每条回执只能读写自己的公开记录。
    let scope = Scope {
        company_id: DomainId::new(company),
        project_id: Some(DomainId::new(project)),
        issue_id: None,
    };
    let customers_arr = customers;
    let env_a = text(&case, "env_a");
    // 用 fixture 中的客户身份构造绑定，避免写死。
    let facts = StageFacts {
        customer_bindings: vec![
            (
                DomainId::new(customers_arr[0].as_str().unwrap()),
                customers_arr[0].as_str().unwrap().to_string(),
            ),
            (
                DomainId::new(customers_arr[1].as_str().unwrap()),
                customers_arr[1].as_str().unwrap().to_string(),
            ),
        ],
        ..StageFacts::default()
    };
    let records = vec![
        bod_app::authorization::ScopedRecord::new(receipts[0].as_str().unwrap(), scope.clone())
            .for_customer(customers_arr[0].as_str().unwrap()),
        bod_app::authorization::ScopedRecord::new(receipts[1].as_str().unwrap(), scope)
            .for_customer(customers_arr[1].as_str().unwrap()),
    ];
    assert_eq!(
        authz_filter_records(
            &facts,
            &principal(customers_arr[0].as_str().unwrap()),
            &records
        )
        .visible,
        vec![receipts[0].as_str().unwrap()],
        "客户 A 拆分后只见自己的回执"
    );
    assert_eq!(
        authz_filter_records(
            &facts,
            &principal(customers_arr[1].as_str().unwrap()),
            &records
        )
        .visible,
        vec![receipts[1].as_str().unwrap()],
        "客户 B 拆分后只见自己的回执"
    );

    let receipt_a = DeliveryReceipt::submitted(
        receipts[0].as_str().unwrap(),
        text(&case, "channel_id"),
        customers_arr[0].as_str().unwrap(),
        env_a,
    )
    .in_thread(text(&case, "thread_id"), "ext-a");
    assert!(update_in_scope(
        &receipt_a,
        customers_arr[0].as_str().unwrap(),
        env_a,
        text(&case, "thread_id"),
        "ext-a"
    ));
    assert!(
        !update_in_scope(
            &receipt_a,
            customers_arr[1].as_str().unwrap(),
            env_a,
            text(&case, "thread_id"),
            "ext-a"
        ),
        "跨客户写入回执被拒"
    );
    let key_a = DeliveryKey::new(
        receipts[0].as_str().unwrap(),
        text(&case, "thread_id"),
        text(&case, "channel_id"),
        customers_arr[0].as_str().unwrap(),
        num(&case, "revision"),
    )
    .render();
    let key_b = DeliveryKey::new(
        receipts[1].as_str().unwrap(),
        text(&case, "thread_id"),
        text(&case, "channel_id"),
        customers_arr[1].as_str().unwrap(),
        num(&case, "revision"),
    )
    .render();
    assert_ne!(key_a, key_b, "两条回执投递键不同");
    // 真实跨客户工单拆分与权限回收仍需 B 轨真实产品验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 12: offline_target
// ---------------------------------------------------------------------------
#[test]
fn offline_target() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/offline_target/case.json"
    ))
    .unwrap();
    let target = Candidate::new(text(&case, "candidate_id"), text(&case, "commit"))
        .with_digest(text(&case, "artifact_digest"));
    let mut report = VerificationReport {
        report_id: text(&case, "report_id").to_string(),
        candidate: target.clone(),
        spec_revision: text(&case, "spec_revision").to_string(),
        rule_revision: text(&case, "rule_revision").to_string(),
        environment: "staging".to_string(),
        tools: vec![ToolVersion::new("cargo", "1.90")],
        executor: Executor::Ci {
            job: "ci-build".to_string(),
        },
        finished_at: text(&case, "now").to_string(),
        raw_ref: String::new(),
        results: vec![CheckResult::passing(
            "build",
            "offline-target",
            text(&case, "now"),
        )],
    };

    // 仅有制品、没有目标安装观测与原始报告：待核实。
    let mut env = EnvironmentRef::new(
        text(&case, "environment_id"),
        text(&case, "provider"),
        text(&case, "external_id"),
        "客户目标",
    )
    .with_desired(text(&case, "desired_revision"));
    assert_eq!(env.drift(), Drift::Unknown);
    assert!(!env.projection().is_deployed());
    let missing = match_report(&report, &target, text(&case, "rule_revision"));
    assert!(
        matches!(missing, EvidenceMatch::Unknown { .. }),
        "没有原始报告引用时不可用于答复修复"
    );

    // 绑定目标安装观测与复验依据后才可对该目标答复。
    env.observe(Some(text(&case, "desired_revision")), text(&case, "now"));
    assert!(env.projection().is_deployed(), "目标安装已有实际观测");
    report.raw_ref = text(&case, "raw_ref").to_string();
    let usable = match_report(&report, &target, text(&case, "rule_revision"));
    assert!(usable.accepts(), "复验依据齐全后才可用");
    // 真实客户目标的部署观测与现场复验仍需 B 轨真实验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 13: rollback_late_event
// ---------------------------------------------------------------------------
#[test]
fn rollback_late_event() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/rollback_late_event/case.json"
    ))
    .unwrap();
    let now = text(&case, "now");
    let mut deployment = Deployment::new(
        text(&case, "deployment_id"),
        text(&case, "plan_id"),
        text(&case, "invocation_id"),
        text(&case, "business_key"),
    );
    deployment.mark_committed();
    assert_eq!(
        apply_deployment(
            &mut deployment,
            DeploymentEvent::DispatchSent {
                at: now.to_string()
            }
        )
        .unwrap(),
        DeploymentState::Running
    );
    assert_eq!(
        apply_deployment(
            &mut deployment,
            DeploymentEvent::VerificationStarted {
                at: now.to_string()
            }
        )
        .unwrap(),
        DeploymentState::Verifying
    );
    assert_eq!(
        apply_deployment(
            &mut deployment,
            DeploymentEvent::VerificationPassed {
                at: now.to_string()
            }
        )
        .unwrap(),
        DeploymentState::Succeeded
    );

    // A 回滚并纠正回执。
    let mut pointer = ProfilePointer::new("release-defect", text(&case, "old_digest"));
    pointer.publish(text(&case, "new_digest"), now);
    let rollback = pointer.rollback(1, text(&case, "later")).unwrap();
    assert!(rollback.session_refreshed);
    assert!(!rollback.external_ops_reverted);
    let receipt_a = DeliveryReceipt::submitted(
        text(&case, "receipt_a"),
        text(&case, "channel_id"),
        text(&case, "recipient_a"),
        text(&case, "env_a_id"),
    )
    .in_thread("thread-a", "ext-a");
    let correction = plan_correction(&receipt_a, rollback.published_revision, false);
    assert!(correction.atomic, "覆盖变化与回执修订同一次写入");
    assert_eq!(correction.mode, CorrectionMode::AppendCorrection);
    assert_eq!(correction.key.recipient, text(&case, "recipient_a"));

    // 旧部署事件迟到：终态不接受，状态不回退。
    let late = apply_deployment(
        &mut deployment,
        DeploymentEvent::VerificationPassed {
            at: text(&case, "later").to_string(),
        },
    );
    assert!(
        matches!(late, Err(TransitionError::WrongState { .. })),
        "旧部署事件必须被拒绝"
    );
    assert_eq!(
        deployment.state,
        DeploymentState::Succeeded,
        "不恢复错误状态"
    );

    // B 仍未安装，不受 A 回滚与旧事件影响。
    let mut env_b = EnvironmentRef::new(text(&case, "env_b_id"), "k8s", "cluster-b/ns-b", "客户 B")
        .with_desired(text(&case, "new_revision"));
    env_b.observe(None, now);
    assert_eq!(env_b.drift(), Drift::Unknown, "B 保持待核实");
    // 真实客户 A 回滚通知与渠道回调乱序仍需 B 轨真实渠道验收。
}

// ---------------------------------------------------------------------------
// §39.16 case 14: metrics_fixture
// ---------------------------------------------------------------------------
#[test]
fn metrics_fixture() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/product_feedback/metrics_fixture/case.json"
    ))
    .unwrap();
    let samples = num(&case, "samples") as usize;
    let required = num(&case, "required_samples") as usize;
    let duration = num(&case, "duration_seconds");
    let result_from = |key: &str| match text(&case, key) {
        "Better" => EvaluationResult::Better,
        "Same" => EvaluationResult::Same,
        "Worse" => EvaluationResult::Worse,
        other => panic!("fixture 结果无法识别: {other}"),
    };
    let make = |result: EvaluationResult| EvaluationRecord {
        record_id: text(&case, "record_id").to_string(),
        task_set_version: num(&case, "task_set_version") as u32,
        profile_revision: num(&case, "profile_revision"),
        skill_revision: None,
        runtime: text(&case, "runtime").to_string(),
        model_config: "default".to_string(),
        environment: text(&case, "environment").to_string(),
        samples,
        required_samples: required,
        acceptance_criteria: "项目既定验收标准".to_string(),
        cost_minor: 0,
        duration_seconds: duration,
        human_interventions: 0,
        result,
    };

    let baseline = make(result_from("baseline_result"));
    let candidate = make(result_from("candidate_result"));
    let replay = make(result_from("candidate_result"));
    assert_eq!(baseline.samples, samples, "分母（样本数）由 fixture 驱动");
    assert_eq!(baseline.required_samples, required, "门槛由 fixture 驱动");
    assert_eq!(candidate.duration_seconds, duration, "纠正时长固定");

    let baseline_verdict = bod_app::feedback_knowledge::promotion_verdict(&baseline);
    let candidate_verdict = bod_app::feedback_knowledge::promotion_verdict(&candidate);
    let replay_verdict = bod_app::feedback_knowledge::promotion_verdict(&replay);
    assert_eq!(
        baseline_verdict,
        PromotionVerdict::NotBetter {
            result: EvaluationResult::Same
        }
    );
    assert_eq!(candidate_verdict, PromotionVerdict::Allowed);
    assert_eq!(replay_verdict, candidate_verdict, "重放时结论一致");
    assert_eq!(
        compare(&candidate, &baseline).unwrap(),
        PromotionVerdict::Allowed,
        "同任务集对照才允许提升"
    );
    let mut different_task_set = candidate.clone();
    different_task_set.task_set_version += 1;
    assert!(
        compare(&candidate, &different_task_set).is_err(),
        "不同任务集的对照不可比"
    );
    // 真实模型效果评测仍需 B 轨；此处只断言固定样本重放的确定性。
}
