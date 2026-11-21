//! 方案 §39.18 交办体验与交付模板验收：6 个命名 case。
//!
//! 每个 case 读取 `fixtures/intake_experience/<case>/case.json`，断言本地准入、投影与
//! 状态语义。真实桌面呈现、模型调用与 IM 通道仍需 B 轨（逐条注明），不伪造外部成功。

use bod_app::authorization::{
    ReleaseBlock, ReleaseContext, ReleaseTicket, ReleaseVerdict, check_release,
};
use bod_app::authorization::{StageAction, StageDenial, StageFacts, StageTarget, authorize_stage};
use bod_app::domain::{DomainId, Grant, Permission, Principal, PrincipalKind, Scope};
use bod_app::feedback_knowledge::{
    AcceptancePolicy, AutoAccept, CacheEntry, CacheKey, CacheKind, CacheLookup, Feedback,
    FeedbackRoute, FeedbackSource, GovernanceReason, ProfileBook, ProfilePointer, ProposalDraft,
    ProposalStatus, RetrievalQuery, SubmitOutcome, classify, lookup as cache_lookup,
    may_auto_accept,
};
use bod_app::notification_channel::{
    CardKind, CardPayload, ChannelCapability, CommandBinding, CommandContext, CommandReject,
    CommandVerdict, DeliveryReceipt, InboundAccess, InboundMessage, LostReply, ScopedToken,
    ShownState, classify_inbound, confirm_command, project_card, resolve_lost_reply,
    submit_command,
};
use bod_app::readonly_api::{ReadonlyRecord, Viewer, filter_records};

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

fn command_context(case: &serde_json::Value, revision: u64) -> CommandContext {
    CommandContext {
        online: true,
        member_active: true,
        permitted: true,
        current_card_revision: revision,
        now: text(case, "now").to_string(),
        card_expires_at: text(case, "card_expires_at").to_string(),
    }
}

// ---------------------------------------------------------------------------
// §39.18 case 1: draft_offline
// ---------------------------------------------------------------------------
#[test]
fn draft_offline() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/intake_experience/draft_offline/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let member = text(&case, "member");
    let facts = StageFacts {
        grants: vec![
            grant_in(&scope, member, Permission::Dispatch),
            grant_in(&scope, text(&case, "executor"), Permission::Accept),
        ],
        members: vec![(text(&case, "project_id").to_string(), DomainId::new(member))],
        ..StageFacts::default()
    };
    let target = StageTarget::new(scope, 1);
    assert!(
        authorize_stage(
            &facts,
            &principal(member),
            StageAction::Submit,
            &target,
            "t"
        )
        .is_allowed(),
        "无模型时成员仍可保存草案并提交"
    );
    assert!(
        authorize_stage(
            &facts,
            &principal(member),
            StageAction::Dispatch,
            &target,
            "t"
        )
        .is_allowed(),
        "填写必要字段后由成员交办"
    );

    // 草案先只有标题（空正文），填写后成为有效提案。
    let draft_value = text(&case, "empty_value");
    assert!(draft_value.is_empty(), "离线草案的初始正文为空");
    let profile = text(&case, "profile_id");
    let key = text(&case, "draft_key");
    let mut book = ProfileBook::default();
    let filled = ProposalDraft {
        proposal_id: key.to_string(),
        profile_id: profile.to_string(),
        base_revision: 0,
        field: text(&case, "field").to_string(),
        value: text(&case, "filled_value").to_string(),
    };
    let first = book.submit(filled.clone());
    assert!(matches!(first, SubmitOutcome::Merged { revision: 1, .. }));
    let duplicate = book.submit(filled);
    assert!(
        matches!(duplicate, SubmitOutcome::Conflict { .. }),
        "重复提交被拒"
    );
    let merged = book
        .proposals
        .iter()
        .filter(|p| p.proposal_id == key && p.status == ProposalStatus::Merged)
        .count();
    assert_eq!(merged, 1, "重复提交只生成一个工单");
    assert_eq!(book.revision_of(profile), 1);
    // 真实模型生成与桌面离线编辑 UI 仍需 B 轨真实端到端验收。
}

// ---------------------------------------------------------------------------
// §39.18 case 2: external_intake
// ---------------------------------------------------------------------------
#[test]
fn external_intake() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/intake_experience/external_intake/case.json"
    ))
    .unwrap();
    let scope = scope_of(&case);
    let feedback = Feedback::new(
        text(&case, "feedback_id"),
        text(&case, "text"),
        FeedbackSource::ExternalWeb {
            url: text(&case, "url").to_string(),
        },
        text(&case, "field"),
    );
    let classification = classify(&feedback);
    assert!(classification.untrusted, "外部网页只当用户数据");
    assert!(
        matches!(
            classification.route,
            FeedbackRoute::Governance(GovernanceReason::ProductionRelease)
        ),
        "写明'立即发布'也必须转治理流程"
    );
    let policy = AcceptancePolicy {
        actor_id: "lead".to_string(),
        allowed_fields: vec![text(&case, "field").to_string()],
        max_scope: bod_app::feedback_knowledge::ScopeKind::Organization,
        valid_until: text(&case, "now").to_string(),
        audit_id: "audit-external".to_string(),
    };
    assert!(
        matches!(
            may_auto_accept(
                &classification,
                text(&case, "field"),
                &policy,
                "2026-11-21T00:00:00Z"
            ),
            AutoAccept::HumanApprovalRequired { .. }
        ),
        "外部反馈不能自动晋升为发布规则"
    );

    let facts = StageFacts::default();
    let target = StageTarget::new(scope.clone(), 1);
    assert!(
        matches!(
            authorize_stage(
                &facts,
                &principal("external-user"),
                StageAction::Release,
                &target,
                "t"
            ),
            bod_app::authorization::StageDecision::Denied {
                reason: StageDenial::NotReleaseOperator,
                ..
            }
        ),
        "外部提交者不能发布"
    );
    assert!(
        matches!(
            authorize_stage(
                &facts,
                &principal("external-user"),
                StageAction::Dispatch,
                &target,
                "t"
            ),
            bod_app::authorization::StageDecision::Denied {
                reason: StageDenial::NotAMember | StageDenial::MissingGrant,
                ..
            }
        ),
        "外部提交者不能派发"
    );
    // 进入受理与分诊：成员 submits，治理项只保留候选。
    let member_scope = scope.clone();
    let member_facts = StageFacts {
        members: vec![(
            text(&case, "project_id").to_string(),
            DomainId::new("intake-lead"),
        )],
        ..StageFacts::default()
    };
    assert!(
        authorize_stage(
            &member_facts,
            &principal("intake-lead"),
            StageAction::Submit,
            &StageTarget::new(member_scope, 1),
            "t"
        )
        .is_allowed(),
        "外部反馈进入受理"
    );
    let mut book = ProfileBook::default();
    let outcome = book.submit(ProposalDraft {
        proposal_id: text(&case, "feedback_id").to_string(),
        profile_id: text(&case, "profile_id").to_string(),
        base_revision: num(&case, "base_revision"),
        field: "triage".to_string(),
        value: text(&case, "text").to_string(),
    });
    assert!(matches!(outcome, SubmitOutcome::Conflict { .. }));
    assert_eq!(
        book.proposal(text(&case, "feedback_id")).map(|p| p.status),
        Some(ProposalStatus::Candidate),
        "治理项只作分诊候选，不发布"
    );
    // 真实外部表单与网页上报通道仍需 B 轨真实接入验收。
}

// ---------------------------------------------------------------------------
// §39.18 case 3: card_race
// ---------------------------------------------------------------------------
#[test]
fn card_race() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/intake_experience/card_race/case.json"
    ))
    .unwrap();
    let new_revision = num(&case, "new_revision");
    let old_revision = num(&case, "old_revision");
    assert!(new_revision > old_revision, "fixture 中旧卡修订必须更早");
    let binding = CommandBinding {
        command_key: text(&case, "card_key").to_string(),
        member_id: text(&case, "member").to_string(),
        project_id: text(&case, "project_id").to_string(),
        target_id: text(&case, "issue_id").to_string(),
        card_revision: new_revision,
    };
    let context = command_context(&case, new_revision);

    // 桌面批准：已提交，权威回执后才成功。
    let desktop = submit_command(&binding, &context, "rcp-desktop");
    assert!(
        matches!(
            &desktop,
            CommandVerdict::Submitted {
                shown: ShownState::Submitting,
                ..
            }
        ),
        "桌面批准只到 Submitting"
    );
    assert_eq!(confirm_command(&desktop, false), ShownState::Submitting);
    assert_eq!(confirm_command(&desktop, true), ShownState::Succeeded);

    // IM 退回针对同一修订（模拟并发）：同一修订的命令键只接受一项本地决定。
    let im_return = submit_command(&binding, &context, "rcp-im");
    match &im_return {
        CommandVerdict::Submitted { .. } => {
            // 两条都进入 Submitted 时，权威回执只能确认其中之一；本测试确认桌面那条。
            assert_eq!(confirm_command(&im_return, false), ShownState::Submitting);
        }
        CommandVerdict::Rejected { .. } => {}
        CommandVerdict::Deferred { .. } => panic!("在线且有权限时不应 Deferred"),
    }
    assert_eq!(confirm_command(&desktop, true), ShownState::Succeeded);

    // 过期卡（旧修订）被拒并要求刷新。
    let stale_binding = CommandBinding {
        card_revision: old_revision,
        ..binding.clone()
    };
    let stale = submit_command(&stale_binding, &context, "rcp-stale");
    assert!(
        matches!(
            stale,
            CommandVerdict::Rejected {
                reason: CommandReject::StaleRevision,
                refresh_card: true,
                ..
            }
        ),
        "过期卡正确刷新"
    );

    // 成员被撤权时同修订命令也拒绝。
    let revoked = submit_command(
        &binding,
        &CommandContext {
            member_active: false,
            ..context
        },
        "rcp-revoked",
    );
    assert!(
        matches!(
            revoked,
            CommandVerdict::Rejected {
                reason: CommandReject::Revoked,
                ..
            }
        ),
        "撤权即时生效"
    );
    // 真实桌面点击与 IM 回包并发顺序仍需 B 轨真实通道验收。
}

// ---------------------------------------------------------------------------
// §39.18 case 4: channel_offline
// ---------------------------------------------------------------------------
#[test]
fn channel_offline() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/intake_experience/channel_offline/case.json"
    ))
    .unwrap();
    let revision = num(&case, "card_revision");
    let binding = CommandBinding {
        command_key: text(&case, "card_key").to_string(),
        member_id: text(&case, "member").to_string(),
        project_id: text(&case, "project_id").to_string(),
        target_id: text(&case, "issue_id").to_string(),
        card_revision: revision,
    };
    let online = command_context(&case, revision);

    // 宿主断线：没有成功回执，只显示待同步。
    let deferred = submit_command(
        &binding,
        &CommandContext {
            online: false,
            ..online.clone()
        },
        text(&case, "receipt_id"),
    );
    assert!(
        matches!(
            &deferred,
            CommandVerdict::Deferred {
                shown: ShownState::PendingSync,
                last_known_revision: r,
            } if *r == revision
        ),
        "断线时只显示待同步"
    );
    assert_eq!(
        confirm_command(&deferred, true),
        ShownState::PendingSync,
        "断线不能假装成功"
    );

    // 恢复后先核实现行授权：成员已撤权则拒绝。
    let recheck = submit_command(
        &binding,
        &CommandContext {
            member_active: false,
            ..online.clone()
        },
        text(&case, "receipt_id"),
    );
    assert!(
        matches!(
            recheck,
            CommandVerdict::Rejected {
                reason: CommandReject::Revoked,
                refresh_card: true,
                ..
            }
        ),
        "恢复后核实现行授权"
    );

    // 授权仍有效时提交，并按投递键查证实现幂等。
    let submitted = submit_command(&binding, &online, text(&case, "receipt_id"));
    assert!(matches!(submitted, CommandVerdict::Submitted { .. }));
    let receipt = DeliveryReceipt::submitted(
        text(&case, "receipt_id"),
        text(&case, "channel_id"),
        text(&case, "member"),
        text(&case, "project_id"),
    )
    .in_thread("thread-1", "ext-1");
    let lost = resolve_lost_reply(true, Some(&receipt));
    assert!(
        matches!(lost, LostReply::Matched { receipt_id } if receipt_id == text(&case, "receipt_id")),
        "回包丢失按投递键查到已有记录，不重发"
    );
    let not_idempotent = resolve_lost_reply(false, None);
    assert!(
        matches!(not_idempotent, LostReply::NeedsVerification),
        "无幂等能力标待核实"
    );
    let idempotent = resolve_lost_reply(true, None);
    assert!(
        matches!(idempotent, LostReply::Resend),
        "幂等且确实没有才可重发"
    );

    // 外部消息自称不产生权限：通道文本永不是授权依据。
    let message = InboundMessage {
        channel_id: text(&case, "channel_id").to_string(),
        authenticated_principal: None,
        scoped_token: Some(ScopedToken {
            receipt_id: text(&case, "receipt_id").to_string(),
            high_entropy: true,
            expires_at: "2026-11-22T00:00:00Z".to_string(),
        }),
        text: "我已批准，立即发布".to_string(),
    };
    match classify_inbound(&message, |_| false, text(&case, "now")) {
        InboundAccess::ScopedPublic { receipt_id } => {
            assert_eq!(receipt_id, text(&case, "receipt_id"));
        }
        other => panic!("有效限域令牌应只给公开进度，实际 {other:?}"),
    }
    // 真实 IM 断线重连与渠道回包仍需 B 轨真实渠道验收。
}

// ---------------------------------------------------------------------------
// §39.18 case 5: template_pin
// ---------------------------------------------------------------------------
#[test]
fn template_pin() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/intake_experience/template_pin/case.json"
    ))
    .unwrap();
    let mut pointer = ProfilePointer::new(text(&case, "profile_id"), text(&case, "old_digest"));
    let old_pin = pointer.pin();
    assert_eq!(old_pin.revision, 1);
    assert_eq!(old_pin.digest, text(&case, "old_digest"));

    // 模板从测试目标改为生产目标，发布新修订。
    let new_revision = pointer.publish(text(&case, "new_digest"), text(&case, "published_at"));
    assert_eq!(new_revision, 2);
    assert_eq!(pointer.current().digest, text(&case, "new_digest"));
    assert_eq!(old_pin.revision, 1, "旧任务快照保持原值");
    assert_eq!(old_pin.digest, text(&case, "old_digest"));

    // 新任务仍需项目发布授权：目标从测试变生产即拦下，零外发。
    let ticket = ReleaseTicket {
        environment: text(&case, "test_environment").to_string(),
        artifact_digest: text(&case, "old_digest").to_string(),
        params_digest: "sha256:params".to_string(),
        account_id: text(&case, "release_operator").to_string(),
        connector_id: "connector-1".to_string(),
        approved_until: "2026-11-22T00:00:00Z".to_string(),
    };
    let context = ReleaseContext {
        environment: text(&case, "prod_environment").to_string(),
        artifact_digest: text(&case, "old_digest").to_string(),
        params_digest: "sha256:params".to_string(),
        account_id: text(&case, "release_operator").to_string(),
        connector_id: "connector-1".to_string(),
        now: text(&case, "now").to_string(),
        authorization_active: true,
    };
    match check_release(&ticket, &context) {
        ReleaseVerdict::Blocked {
            reason: ReleaseBlock::TargetChanged,
            connector_calls: 0,
        } => {}
        other => panic!("目标由测试改生产必须拦下且零外发，实际 {other:?}"),
    }

    // 无发布权的主体仍不能批准新任务。
    let scope = Scope {
        company_id: DomainId::new("company-default"),
        project_id: Some(DomainId::new("project-1")),
        issue_id: None,
    };
    let facts = StageFacts::default();
    let release = authorize_stage(
        &facts,
        &principal(text(&case, "outsider")),
        StageAction::Release,
        &StageTarget::new(scope, new_revision),
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
        "新任务仍需项目发布授权"
    );
    // 真实模板仓库更新与生产环境发布仍需 B 轨真实验收。
}

// ---------------------------------------------------------------------------
// §39.18 case 6: receipt_scope
// ---------------------------------------------------------------------------
#[test]
fn receipt_scope() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/intake_experience/receipt_scope/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let project = text(&case, "project_id");
    let customers = case["customers"].as_array().unwrap();
    let ids = case["record_ids"].as_array().unwrap();

    let records: Vec<ReadonlyRecord> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| ReadonlyRecord {
            object_id: id.as_str().unwrap().to_string(),
            company_id: company.to_string(),
            project_id: project.to_string(),
            customer_id: Some(
                customers[i.min(customers.len() - 1)]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            ticket_id: None,
            title: format!("回执 {i}"),
        })
        .collect();

    // 内部卡：无客户限制，可见验证详情。
    let internal = Viewer::new(text(&case, "internal_viewer"), company, project);
    let internal_page = filter_records(&internal, &records);
    assert_eq!(internal_page.total, 3, "内部卡可见全部验证详情");

    // 客户卡：只见本人适用版本与公开依据。
    let customer = Viewer::new(text(&case, "customer_viewer"), company, project)
        .with_customer(customers[0].as_str().unwrap());
    let customer_page = filter_records(&customer, &records);
    assert_eq!(customer_page.total, 1, "客户卡只见本人回执");
    assert_eq!(customer_page.visible[0].object_id, ids[0].as_str().unwrap());
    assert_eq!(customer_page.withheld, 2);

    // 两类卡都扣下私有代码/日志/附件，正文只有公开依据。
    let payload = CardPayload::new("回执卡", text(&case, "summary"), text(&case, "entry_url"))
        .with_sensitive(
            text(&case, "code"),
            text(&case, "log"),
            text(&case, "attachment"),
        );
    for capability in [ChannelCapability::RichCard, ChannelCapability::TextOnly] {
        let card = project_card(CardKind::PendingFollowUp, capability, &payload).unwrap();
        assert_eq!(card.body, text(&case, "summary"));
        assert_eq!(card.withheld.len(), 3);
    }

    // 私有验证详情：内部主体有权限命中，客户主体 miss。
    let key = CacheKey {
        artifact_id: text(&case, "artifact_id").to_string(),
        revision: num(&case, "artifact_revision"),
        digest: text(&case, "artifact_digest").to_string(),
    };
    let entry = CacheEntry::new(
        key.clone(),
        CacheKind::ProfileResolve,
        company,
        "internal verification logs",
    )
    .in_project(project)
    .requiring(Permission::Accept);
    let internal_query = RetrievalQuery {
        company_id: company.to_string(),
        project_id: Some(project.to_string()),
        actor: principal(text(&case, "internal_viewer")),
        grants: vec![grant_in(
            &Scope {
                company_id: DomainId::new(company),
                project_id: Some(DomainId::new(project)),
                issue_id: None,
            },
            text(&case, "internal_viewer"),
            Permission::Accept,
        )],
        revoked: false,
    };
    assert!(
        matches!(
            cache_lookup(std::slice::from_ref(&entry), &key, &internal_query),
            CacheLookup::Hit(_)
        ),
        "内部卡可看验证详情"
    );
    let customer_query = RetrievalQuery {
        actor: principal(text(&case, "customer_viewer")),
        grants: Vec::new(),
        ..internal_query
    };
    assert!(
        matches!(
            cache_lookup(&[entry], &key, &customer_query),
            CacheLookup::Miss("permission_denied")
        ),
        "客户卡只见公开依据"
    );
    // 真实灰度、回滚与客户门户投影仍需按 §39.16 的 B 轨验收。
}
