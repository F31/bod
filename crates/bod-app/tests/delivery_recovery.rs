//! 方案 §39.19 交付可靠性故障验收：7 个命名 case。
//!
//! 每个 case 读取 `fixtures/delivery_recovery/<case>/case.json`，断言本地状态语义、
//! 门禁与恢复口径。真实临时 Git 仓库、故障注入连接器与多节点恢复仍需 B 轨
//! （逐条注明），本套件不伪造外部成功。

use bod_app::authorization::{
    MembershipChange, ReleaseBlock, ReleaseVerdict, StageAction, StageFacts, StageTarget,
    authorize_stage, check_release, revoke_member,
};
use bod_app::cleanup_recovery::{
    BLOCK_SCOPE_UNKNOWN, BLOCK_STOP_UNCONFIRMED, CleanupFacts, CleanupPolicy, ConditionState,
    StopFacts, evaluate_cleanup,
};
use bod_app::context_handoff::{FragmentCache, PermissionSnapshot, RefKind, StableRef};
use bod_app::domain::sha256_hex;
use bod_app::domain::{DomainId, Principal, PrincipalKind, Scope};
use bod_app::feedback_knowledge::{
    ProfileBook, ProfilePointer, ProposalDraft, ProposalStatus, SubmitOutcome,
};
use bod_app::notification_channel::{
    DeliveryReceipt, LostReply, plan_correction, resolve_lost_reply,
};
use bod_app::release_governance::{
    Deployment, DeploymentEvent, DeploymentState, Drift, EnvironmentRef, TransitionError,
    apply as apply_deployment,
};
use bod_app::workspace_safety::{
    GitWorkspace, RetainReason, SceneRegistry, ScopeViolation, WriteScope,
};

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

fn principal(id: &str) -> Principal {
    Principal {
        id: DomainId::new(id),
        kind: PrincipalKind::User,
        claimed_roles: Vec::new(),
    }
}

fn strings(v: &serde_json::Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("fixture 缺少数组字段 {key}"))
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// §39.19 case 1: dirty_stop_restart
// ---------------------------------------------------------------------------
#[test]
fn dirty_stop_restart() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/delivery_recovery/dirty_stop_restart/case.json"
    ))
    .unwrap();
    let states = strings(&case, "stop_states");
    let stop = StopFacts::from_states(
        &states,
        num(&case, "running_issues") as i64,
        num(&case, "unattributed") as usize,
    );
    assert!(
        stop.blocks_stop(),
        "有未提交文件与未确认停止，不能视为已停止"
    );

    let facts = CleanupFacts {
        scope_known: true,
        stop,
        budget: bod_app::cleanup_recovery::BudgetFacts::default(),
        artifacts: bod_app::cleanup_recovery::ArtifactFacts::default(),
        temp_envs: ConditionState::Unknown,
        temp_env_detail: text(&case, "detail").to_string(),
        pins: Vec::new(),
    };
    let policy = CleanupPolicy {
        temp_envs: ConditionState::Unknown,
        temp_env_detail: text(&case, "detail").to_string(),
        cascade_authorized: false,
    };
    let verdict = evaluate_cleanup(&facts, &policy);
    assert!(!verdict.is_ready(), "脏现场不满足清理条件");
    let codes = verdict.blocked_codes();
    assert!(
        codes.contains(&BLOCK_STOP_UNCONFIRMED),
        "停止未确认是显式阻塞码"
    );
    assert!(
        text(&case, "blank_confirmation").trim().is_empty(),
        "空白确认不能作为丢弃脏现场的依据"
    );

    // 现场保留并登记待核实，重复确认只创建一次恢复尝试。
    let mut scenes = SceneRegistry::new();
    let workspace = GitWorkspace {
        attempt_id: text(&case, "attempt_id").to_string(),
        branch: "attempt/dirty-1".to_string(),
        path: std::env::temp_dir().join("bod-dirty-worktree"),
    };
    let dirty = strings(&case, "dirty_paths");
    let retained = scenes.retain(&workspace, dirty.clone(), RetainReason::DirtyFiles);
    assert_eq!(retained.dirty_paths, dirty, "原文件逐项登记可恢复");
    assert!(scenes.is_pending(text(&case, "attempt_id")));
    let resolved = scenes.resolve(
        text(&case, "attempt_id"),
        bod_app::workspace_safety::Disposal::Accepted,
    );
    assert!(resolved.is_some(), "首次确认创建一次恢复尝试");
    assert!(
        scenes
            .resolve(
                text(&case, "attempt_id"),
                bod_app::workspace_safety::Disposal::Accepted
            )
            .is_none(),
        "重复确认不新建第二次恢复尝试"
    );
    // 真实未提交文件/未推送提交与宿主终止的恢复仍需 B 轨真实 Git 与宿主验收。
}

// ---------------------------------------------------------------------------
// §39.19 case 2: sent_ack_lost
// ---------------------------------------------------------------------------
#[test]
fn sent_ack_lost() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/delivery_recovery/sent_ack_lost/case.json"
    ))
    .unwrap();
    let now = text(&case, "now");
    let invocation = text(&case, "invocation_id");
    let mut deployment = Deployment::new(
        text(&case, "deployment_id"),
        text(&case, "plan_id"),
        invocation,
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
            DeploymentEvent::Accepted {
                at: now.to_string()
            }
        )
        .unwrap(),
        DeploymentState::Running,
        "渠道接受请求不等于成功"
    );
    assert_eq!(
        apply_deployment(
            &mut deployment,
            DeploymentEvent::AckLost {
                at: now.to_string()
            }
        )
        .unwrap(),
        DeploymentState::Unknown
    );
    assert!(!deployment.can_resend(), "Unknown 一律不能盲重发");
    assert_eq!(
        deployment.unknown_query_key(),
        Some(invocation),
        "按同一 invocation 查询"
    );

    let resolved = apply_deployment(
        &mut deployment,
        DeploymentEvent::QueryResolved {
            at: text(&case, "later").to_string(),
            state: DeploymentState::Succeeded,
            evidence: text(&case, "query_evidence").to_string(),
        },
    )
    .unwrap();
    assert_eq!(resolved, DeploymentState::Succeeded);
    assert_eq!(deployment.query_attempts, 1, "可查渠道只补记一次");
    assert!(
        deployment
            .evidence
            .iter()
            .any(|item| item.contains(text(&case, "query_evidence")))
    );

    // 回包丢失的处置：可查幂等渠道按投递键查到原记录就不重发。
    let receipt = DeliveryReceipt::submitted(
        text(&case, "receipt_id"),
        text(&case, "channel_id"),
        "cust-a",
        "env-a",
    )
    .in_thread("thread-1", "ext-1");
    assert!(matches!(
        resolve_lost_reply(true, Some(&receipt)),
        LostReply::Matched { receipt_id } if receipt_id == text(&case, "receipt_id")
    ));
    assert!(
        matches!(
            resolve_lost_reply(false, None),
            LostReply::NeedsVerification
        ),
        "无幂等且不可查的渠道保留未知"
    );
    // 真实渠道查询 API 与回包注入仍需 B 轨真实连接器验收。
}

// ---------------------------------------------------------------------------
// §39.19 case 3: scoped_rollback
// ---------------------------------------------------------------------------
#[test]
fn scoped_rollback() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/delivery_recovery/scoped_rollback/case.json"
    ))
    .unwrap();
    let now = text(&case, "now");
    let later = text(&case, "later");

    // A 回滚并产生更正计划；B 始终没有安装观测。
    let mut pointer = ProfilePointer::new("scoped-defect", text(&case, "old_digest"));
    pointer.publish(text(&case, "new_digest"), now);
    let rollback = pointer.rollback(1, later).unwrap();
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
    assert_eq!(
        correction.key.recipient,
        text(&case, "recipient_a"),
        "只有 A 产生更正"
    );
    assert_eq!(correction.original_receipt_id, text(&case, "receipt_a"));

    let mut env_a = EnvironmentRef::new(text(&case, "env_a_id"), "k8s", "cluster/ns-a", "A")
        .with_desired(text(&case, "new_digest"));
    env_a.observe(Some(text(&case, "new_digest")), now);
    assert!(env_a.projection().is_deployed());
    let mut env_b = EnvironmentRef::new(text(&case, "env_b_id"), "k8s", "cluster/ns-b", "B")
        .with_desired(text(&case, "new_digest"));
    env_b.observe(None, now);
    assert_eq!(env_b.drift(), Drift::Unknown, "B 保持待核实");

    // 发送更正期间注入旧部署回调：终态不接受，不回退到错误状态。
    let mut deployment = Deployment::new(
        text(&case, "deployment_id"),
        text(&case, "plan_id"),
        text(&case, "invocation_id"),
        text(&case, "business_key"),
    );
    deployment.mark_committed();
    apply_deployment(
        &mut deployment,
        DeploymentEvent::DispatchSent {
            at: now.to_string(),
        },
    )
    .unwrap();
    apply_deployment(
        &mut deployment,
        DeploymentEvent::VerificationStarted {
            at: now.to_string(),
        },
    )
    .unwrap();
    apply_deployment(
        &mut deployment,
        DeploymentEvent::VerificationPassed {
            at: now.to_string(),
        },
    )
    .unwrap();
    let stale = apply_deployment(
        &mut deployment,
        DeploymentEvent::Rejected {
            at: later.to_string(),
            reason: "old deployment callback".to_string(),
        },
    );
    assert!(
        matches!(stale, Err(TransitionError::WrongState { .. })),
        "旧部署回调不改变远端最后确认修订"
    );
    assert_eq!(deployment.state, DeploymentState::Succeeded);
    assert_eq!(
        correction.key.revision, rollback.published_revision,
        "公开回执修订与最后确认一致"
    );
    // 真实两客户回滚通知与渠道回调乱序仍需 B 轨真实渠道验收。
}

// ---------------------------------------------------------------------------
// §39.19 case 4: account_revoked
// ---------------------------------------------------------------------------
#[test]
fn account_revoked() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/delivery_recovery/account_revoked/case.json"
    ))
    .unwrap();
    let member = DomainId::new(text(&case, "member"));
    let ticket = bod_app::authorization::ReleaseTicket {
        environment: text(&case, "environment").to_string(),
        artifact_digest: text(&case, "artifact_digest").to_string(),
        params_digest: text(&case, "params_digest").to_string(),
        account_id: text(&case, "account_id").to_string(),
        connector_id: text(&case, "connector_id").to_string(),
        approved_until: text(&case, "approved_until").to_string(),
    };
    let approved_context = || bod_app::authorization::ReleaseContext {
        environment: text(&case, "environment").to_string(),
        artifact_digest: text(&case, "artifact_digest").to_string(),
        params_digest: text(&case, "params_digest").to_string(),
        account_id: text(&case, "account_id").to_string(),
        connector_id: text(&case, "connector_id").to_string(),
        now: text(&case, "now").to_string(),
        authorization_active: true,
    };
    assert!(
        matches!(
            check_release(&ticket, &approved_context()),
            ReleaseVerdict::Dispatch { connector_calls: 1 }
        ),
        "撤销前原决定仍允许一次外发"
    );

    // 批准后撤销连接：授权失活即拦，connector_calls 为 0。
    let revoked = check_release(
        &ticket,
        &bod_app::authorization::ReleaseContext {
            authorization_active: false,
            ..approved_context()
        },
    );
    assert!(matches!(
        revoked,
        ReleaseVerdict::Blocked {
            reason: ReleaseBlock::AuthorizationRevoked,
            connector_calls: 0
        }
    ));
    // 接管者更换目标账号同样拦下。
    let changed = check_release(
        &ticket,
        &bod_app::authorization::ReleaseContext {
            account_id: "account-replaced".to_string(),
            ..approved_context()
        },
    );
    assert!(matches!(
        changed,
        ReleaseVerdict::Blocked {
            reason: ReleaseBlock::AccountChanged,
            connector_calls: 0
        }
    ));

    // 原决定保留，无法撤销的外部凭据进对账（不删资产、不改历史）。
    let mut facts = StageFacts {
        grants: vec![bod_app::domain::Grant {
            principal_id: member.clone(),
            scope: Scope {
                company_id: DomainId::new(text(&case, "company_id")),
                project_id: Some(DomainId::new(text(&case, "project_id"))),
                issue_id: None,
            },
            permission: bod_app::domain::Permission::Dispatch,
            revision: 1,
            active: true,
        }],
        ..StageFacts::default()
    };
    let outcome = revoke_member(
        &mut facts,
        &member,
        MembershipChange::DisableAgent,
        &[],
        &[text(&case, "external_credential").to_string()],
    );
    assert_eq!(
        outcome.reconciliation,
        vec![text(&case, "external_credential")]
    );
    assert!(outcome.assets_deleted.is_empty(), "撤权不删资产");
    assert!(!outcome.history_rewritten, "不改历史归属");
    assert_eq!(
        ticket.account_id,
        text(&case, "account_id"),
        "原决定内容不变"
    );
    // 真实连接器撤销与账号更换的拦截仍需 B 轨真实渠道验收。
}

// ---------------------------------------------------------------------------
// §39.19 case 5: concurrent_triage
// ---------------------------------------------------------------------------
#[test]
fn concurrent_triage() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/delivery_recovery/concurrent_triage/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let project = text(&case, "project_id");
    let scope = Scope {
        company_id: DomainId::new(company),
        project_id: Some(DomainId::new(project)),
        issue_id: None,
    };
    let workers = case["workers"].as_array().unwrap();
    let customers = case["customers"].as_array().unwrap();
    let receipts = case["receipt_ids"].as_array().unwrap();
    let a = workers[0].as_str().unwrap();
    let b = workers[1].as_str().unwrap();

    let facts = StageFacts {
        customer_bindings: vec![
            (DomainId::new(a), customers[0].as_str().unwrap().to_string()),
            (DomainId::new(b), customers[1].as_str().unwrap().to_string()),
        ],
        ..StageFacts::default()
    };
    let records: Vec<bod_app::authorization::ScopedRecord> = receipts
        .iter()
        .enumerate()
        .map(|(i, id)| {
            bod_app::authorization::ScopedRecord::new(id.as_str().unwrap(), scope.clone())
                .for_customer(customers[i].as_str().unwrap())
        })
        .collect();
    assert_eq!(
        bod_app::authorization::filter_records(&facts, &principal(a), &records).visible,
        vec![receipts[0].as_str().unwrap()],
        "客户回执 A 隔离"
    );
    assert_eq!(
        bod_app::authorization::filter_records(&facts, &principal(b), &records).visible,
        vec![receipts[1].as_str().unwrap()],
        "客户回执 B 隔离"
    );

    // 两人同时认领并合并重复反馈：只有一个本期研发决定落地。
    let mut book = ProfileBook::default();
    let profile = text(&case, "profile_id");
    let draft = ProposalDraft {
        proposal_id: text(&case, "feedback_id").to_string(),
        profile_id: profile.to_string(),
        base_revision: 0,
        field: "triage_owner".to_string(),
        value: a.to_string(),
    };
    let first = book.submit(draft.clone());
    let mut competing = draft;
    competing.value = b.to_string();
    let second = book.submit(competing);
    assert!(matches!(first, SubmitOutcome::Merged { revision: 1, .. }));
    assert!(matches!(second, SubmitOutcome::Conflict { .. }));
    assert_eq!(book.revision_of(profile), 1, "只有一个研发决定");
    assert_eq!(
        book.proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Merged)
            .count(),
        1
    );

    // 唯一当前责任人：只有被指派者能做 Develop。
    let work = authorize_stage(
        &StageFacts::default(),
        &principal(text(&case, "executor")),
        StageAction::Develop,
        &StageTarget::new(scope, 1).with_assignee(DomainId::new(text(&case, "executor"))),
        "t",
    );
    assert!(work.is_allowed(), "当前责任人唯一且可继续工作");
    // 真实两人并发认领与后端 CAS 串行化仍需 B 轨多节点验收。
}

// ---------------------------------------------------------------------------
// §39.19 case 6: pinned_review
// ---------------------------------------------------------------------------
#[test]
fn pinned_review() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/delivery_recovery/pinned_review/case.json"
    ))
    .unwrap();
    let company = text(&case, "company_id");
    let project = text(&case, "project_id");
    let permissions = PermissionSnapshot::new(text(&case, "subject"), company, project);
    let mut cache = FragmentCache::new();

    let old_body = text(&case, "old_body");
    let old_digest = format!("sha256:{}", sha256_hex(old_body.as_bytes()));
    let old_ref = StableRef::new(
        RefKind::Spec,
        text(&case, "ref_id"),
        text(&case, "old_revision"),
        old_digest.clone(),
    )
    .unwrap();
    cache
        .insert(old_ref.clone(), &permissions, old_body)
        .unwrap();
    assert_eq!(cache.len(), 1);
    let pinned = cache.lookup(&old_ref, &permissions).unwrap().unwrap();
    assert_eq!(pinned.body, old_body, "旧工单固定原模板正文");

    // 更新仓库模板并改写被批注段落：正文变了，摘要必变，旧键不覆盖。
    let new_body = text(&case, "new_body");
    assert_ne!(old_body, new_body);
    let new_digest = format!("sha256:{}", sha256_hex(new_body.as_bytes()));
    assert_ne!(old_digest, new_digest);
    let new_ref = StableRef::new(
        RefKind::Spec,
        text(&case, "ref_id"),
        text(&case, "new_revision"),
        new_digest,
    )
    .unwrap();
    assert_ne!(old_ref.key(), new_ref.key(), "内容变化产生新键");
    assert!(
        cache.lookup(&new_ref, &permissions).unwrap().is_none(),
        "无法确定位置的批注回到待定位，不冒充命中"
    );
    assert_eq!(
        cache.lookup(&old_ref, &permissions).unwrap().unwrap().body,
        old_body,
        "旧钉扎仍可读，不被新模板覆盖"
    );
    // 真实仓库模板更新与批注位置重算仍需 B 轨真实 Git 与评审 UI 验收。
}

// ---------------------------------------------------------------------------
// §39.19 case 7: reclaim_restore
// ---------------------------------------------------------------------------
#[test]
fn reclaim_restore() {
    let case: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/delivery_recovery/reclaim_restore/case.json"
    ))
    .unwrap();

    // 活动或归属未知的工作树：事实不明时不可回收，只登记待核实。
    let mut scenes = SceneRegistry::new();
    let workspace = GitWorkspace {
        attempt_id: text(&case, "attempt_id").to_string(),
        branch: "attempt/unknown".to_string(),
        path: std::env::temp_dir().join("bod-unknown-worktree"),
    };
    let dirty = strings(&case, "dirty_paths");
    scenes.retain(&workspace, dirty.clone(), RetainReason::UnconfirmedEffect);
    assert!(scenes.is_pending(text(&case, "attempt_id")));

    let unknown = CleanupFacts::unknown_scope(text(&case, "unknown_detail"));
    let verdict = evaluate_cleanup(
        &unknown,
        &CleanupPolicy {
            temp_envs: ConditionState::Unknown,
            temp_env_detail: text(&case, "unknown_detail").to_string(),
            cascade_authorized: false,
        },
    );
    assert!(!verdict.is_ready());
    assert!(
        verdict.blocked_codes().contains(&BLOCK_SCOPE_UNKNOWN),
        "归属未知时拒绝回收"
    );
    assert_eq!(
        verdict.condition("scope").map(|item| item.state),
        Some(ConditionState::Unknown)
    );

    // 有独有改动的任务在已校验恢复点上逐文件恢复：写范围逐项判定。
    let allow = strings(&case, "scope_allow");
    let coordinated = strings(&case, "scope_coordinated");
    let scope = WriteScope::new(allow, coordinated);
    assert!(
        scope.check("src/ exclusive.rs").is_ok(),
        "允许范围内文件可恢复"
    );
    assert!(
        matches!(
            scope.check(".git/config"),
            Err(ScopeViolation::GitMetadata(_))
        ),
        "Git 元数据永不恢复"
    );
    assert!(
        matches!(
            scope.check("other/file.rs"),
            Err(ScopeViolation::OutsideScope(_))
        ),
        "范围外文件不恢复"
    );
    assert!(
        matches!(
            scope.check("Cargo.toml"),
            Err(ScopeViolation::SharedUngranted(_))
        ),
        "共享配置未授权不恢复"
    );
    assert!(
        scope.check("openspec/spec.md").is_ok(),
        "取得 coordinated 授权的共享配置可恢复"
    );
    // 真实恢复点校验与逐文件落盘仍需 B 轨真实 Git 工作树验收；本 case 只断言本地回收/写范围语义。
}
