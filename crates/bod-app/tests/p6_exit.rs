//! P6 5.1—5.4 的出口核对：用例清单、可观测字段、远程关闭影响面、支持矩阵与出口报告。
//!
//! 全部是对**源码与文档文本**的检查，加上两个组合场景的值级用例。它不证明任何一次真实
//! 双机或真机交互发生过——2.8 与 4.6 仍未完成。

use std::collections::BTreeSet;
use std::path::Path;

use bod_app::extension_sdk::{
    AdapterDescriptor, Capability, EvidenceTier, HostRequirements, Negotiation, NegotiationReject,
    admit, negotiate, support_claim,
};
use bod_app::mobile_control::{
    Confirmation, ConfirmationLedger, OfflineCommand, ReplayVerdict, replay_offline,
};
use bod_app::readonly_api::{ReadonlyRecord, Viewer, filter_records};
use bod_app::remote_delegation::{Channel, DeviceAction, DeviceGrant, signature_label};

const TASKS: &str = include_str!("../../../openspec/changes/p6-open-extension/tasks.md");
const SELF: &str = include_str!("../tests/p6_exit.rs");

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn source(module: &str) -> String {
    if module == "self" {
        return SELF.to_string();
    }
    let path = root().join("src").join(format!("{module}.rs"));
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("读不到 {}: {err}", path.display()))
}

fn body<'a>(text: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}(");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("没有用例 `{name}`"));
    let rest = &text[start..];
    let end = rest
        .find("\n    #[test]")
        .or_else(|| rest.find("\n#[test]"))
        .unwrap_or(rest.len());
    &rest[..end]
}

struct Inventory {
    case: &'static str,
    module: &'static str,
    named: &'static str,
    /// 命令 ID / 尝试 / 前后状态 / 回执 四类字段在本模块里的名字。
    fields: &'static [&'static str],
}

const INVENTORY: &[Inventory] = &[
    Inventory {
        case: "RM01",
        module: "remote_delegation",
        named: "rm01_same_request_returns_same_instance_and_changed_params_rejected",
        fields: &["invocation_id", "attempts", "execution_id", "params_digest"],
    },
    Inventory {
        case: "RM02",
        module: "remote_partition",
        named: "rm02_applied_effect_recorded_once_or_needs_verification",
        fields: &["business_key", "external_id", "blind_writes", "recorded"],
    },
    Inventory {
        case: "RM03",
        module: "remote_partition",
        named: "rm03_lease_expired_new_attempt_keeps_old_report_as_history",
        fields: &[
            "attempt",
            "authoritative_attempt",
            "acceptance_created",
            "actual_usage",
        ],
    },
    Inventory {
        case: "RM04",
        module: "mobile_control",
        named: "rm04_revoked_device_cannot_accept_signed_decision",
        fields: &[
            "receipt_id",
            "target_state_changed",
            "revision",
            "artifact_digest",
        ],
    },
    Inventory {
        case: "RM05",
        module: "mobile_control",
        named: "rm05_offline_delivery_expiry_returns_pending_not_applied",
        fields: &[
            "command_id",
            "deliver_by_ms",
            "token_expires_at_ms",
            "PendingDelivery",
        ],
    },
    Inventory {
        case: "RM06",
        module: "remote_delegation",
        named: "rm06_slow_subscriber_releases_buffer_and_resumes_from_snapshot",
        fields: &[
            "last_persisted_cursor",
            "buffered",
            "baseline",
            "display_state",
        ],
    },
    Inventory {
        case: "RM07",
        module: "remote_delegation",
        named: "rm07_tampered_batch_rejected_and_legal_batch_applied_once",
        fields: &["event_digest", "key_version", "to_sequence", "cursor"],
    },
    Inventory {
        case: "RM08",
        module: "self",
        named: "rm08_subset_webhook_rejected_for_managed_release_but_admitted_for_low_risk",
        fields: &["MissingCapability", "in_scheduling_pool", "support_claim"],
    },
    Inventory {
        case: "RM09",
        module: "mobile_control",
        named: "rm09_dual_device_single_decision_and_revocation_limits_replay",
        fields: &["confirmation_id", "Duplicate", "ReplayVerdict", "verdict"],
    },
    Inventory {
        case: "MD01",
        module: "self",
        named: "md01_h5_submit_to_customer_confirmation_survives_reconnect_once",
        fields: &[
            "filter_records",
            "confirm",
            "replay_offline",
            "ReplayVerdict",
        ],
    },
    Inventory {
        case: "MD02",
        module: "mobile_control",
        named: "md02_stale_signature_cannot_accept_new_revision",
        fields: &["revision", "artifact_digest", "recheck_prompt", "receipt"],
    },
    Inventory {
        case: "MD03",
        module: "mobile_control",
        named: "task_4_3_reminder_has_no_business_effect",
        fields: &[
            "business_writes",
            "started_agents",
            "escalated_to",
            "created_decisions",
        ],
    },
    Inventory {
        case: "MD04",
        module: "mobile_control",
        named: "md04_repeated_confirmation_recorded_once",
        fields: &[
            "confirmation_id",
            "rollback_seq",
            "cost_ref",
            "settlement_ref",
        ],
    },
    Inventory {
        case: "SB05",
        module: "remote_delegation",
        named: "sb05_offline_revocation_blocks_queued_dispatch_but_keeps_view",
        fields: &[
            "command_id",
            "control_session_terminated",
            "queued_rejected",
            "external_ops_to_reconcile",
        ],
    },
    Inventory {
        case: "SB06",
        module: "extension_sdk",
        named: "task_1_5_widening_scope_requires_new_decision",
        fields: &["project_id", "scope", "recipient", "account_id"],
    },
];

#[test]
fn rm_md_sb_cases_have_named_implementations() {
    let mut seen = BTreeSet::new();
    for item in INVENTORY {
        assert!(seen.insert(item.case), "{} 被重复登记", item.case);
        let text = source(item.module);
        let case_body = body(&text, item.named);
        assert!(
            case_body.matches("assert").count() >= 2,
            "{} 的用例 `{}` 断言不足两条",
            item.case,
            item.named
        );
        assert!(
            !case_body.contains("assert!(true)"),
            "{} 的用例 `{}` 用了恒真断言",
            item.case,
            item.named
        );
    }
    assert_eq!(seen.len(), 15, "RM01—RM09、MD01—MD04、SB05—SB06 共 15 条");
}

#[test]
fn observability_fields_exist_for_each_case() {
    for item in INVENTORY {
        let text = source(item.module);
        for field in item.fields {
            assert!(
                text.contains(field),
                "{} 的链路缺可观测字段 `{field}`（命令 ID / 尝试 / 前后状态 / 回执）",
                item.case
            );
        }
    }
}

/// 5.2：本地路径不得依赖远程模块，历史无签名如实标注。
#[test]
fn remote_disabled_keeps_local_paths_independent() {
    for module in [
        "catalog_upgrade",
        "first_use",
        "artifact_acceptance",
        "control_api",
    ] {
        let text = source(module);
        for remote in [
            "remote_delegation",
            "remote_partition",
            "mobile_control",
            "readonly_api",
        ] {
            assert!(
                !text.contains(&format!("crate::{remote}")),
                "{module} 不得依赖远程模块 {remote}"
            );
        }
    }
    assert_eq!(
        signature_label(None),
        "无节点签名",
        "历史执行无签名必须如实标注"
    );
}

/// 5.1：任务勾选状态必须与出口结论一致（2.8 与 4.6 未勾选）。
#[test]
fn only_environment_blocked_tasks_stay_open() {
    let mut unchecked = BTreeSet::new();
    for line in TASKS.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- [ ] ") else {
            continue;
        };
        if let Some(id) = rest.split_whitespace().next() {
            unchecked.insert(id.to_string());
        }
    }
    assert_eq!(
        unchecked,
        ["2.8".to_string(), "4.6".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "P6 只剩 2.8 与 4.6 未完成，其余自动可做的都必须已勾选"
    );
}

/// 5.1 / RM08：只有基本回调的 webhook 不能承接受管发布，但可承接低风险任务。
#[test]
fn rm08_subset_webhook_rejected_for_managed_release_but_admitted_for_low_risk() {
    // webhook 只声明基本回调：没有委派与制品能力。
    let webhook = AdapterDescriptor::new("webhook-basic", "1.0.0", 2)
        .with_capability(Capability::LocalExecution, EvidenceTier::Probed);

    let managed = HostRequirements::new(
        2,
        vec![Capability::LocalExecution, Capability::Delegation],
        EvidenceTier::Probed,
    );
    assert_eq!(
        negotiate(&webhook, &managed),
        Negotiation::Rejected(NegotiationReject::MissingCapability(Capability::Delegation)),
        "受管发布缺必需能力必须拒绝，并点名缺哪项"
    );

    let low_risk = HostRequirements::new(2, vec![Capability::LocalExecution], EvidenceTier::Probed);
    assert!(matches!(
        negotiate(&webhook, &low_risk),
        Negotiation::Accepted { .. }
    ));

    // 没有声明就不能对外宣称支持该能力。
    assert!(support_claim(&webhook, Capability::Delegation, None).is_err());
    // 安装侧：缺 conformance 报告只可 Experimental。
    let package = bod_app::extension_sdk::ExtensionPackage::new(
        webhook.clone(),
        "registry-official",
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .with_license("MIT");
    let admission = admit(
        &package,
        &bod_app::extension_sdk::ExtensionPolicy::default(),
        None,
    );
    assert!(!admission.in_scheduling_pool(), "无报告不得进调度候选");
}

/// 5.1 / MD01：H5 提交到客户确认，断线重连后命令只生效一次，客户只见自己的目标。
#[test]
fn md01_h5_submit_to_customer_confirmation_survives_reconnect_once() {
    // 客户 A 只看得到自己的目标。
    let customer_a = Viewer::new("customer-user-a", "company-1", "project-1")
        .with_customer("customer-a")
        .with_tickets(&["ticket-1"]);
    let records = vec![
        ReadonlyRecord {
            object_id: "target-a".to_string(),
            company_id: "company-1".to_string(),
            project_id: "project-1".to_string(),
            customer_id: Some("customer-a".to_string()),
            ticket_id: Some("ticket-1".to_string()),
            title: "客户 A 的目标".to_string(),
        },
        ReadonlyRecord {
            object_id: "target-b".to_string(),
            company_id: "company-1".to_string(),
            project_id: "project-1".to_string(),
            customer_id: Some("customer-b".to_string()),
            ticket_id: Some("ticket-2".to_string()),
            title: "客户 B 的目标".to_string(),
        },
    ];
    let page = filter_records(&customer_a, &records);
    assert_eq!(page.total, 1, "客户只能访问自己的目标");
    assert_eq!(page.visible[0].object_id, "target-a");

    // 断网重连：同一条确认命令重放两次，只记一次决定。
    let mut ledger = ConfirmationLedger::new();
    let confirmation = Confirmation {
        confirmation_id: "confirm-md01".to_string(),
        decision: "accepted".to_string(),
        revision: 1,
        rollback_seq: 0,
    };
    ledger.confirm("accept:target-a", confirmation.clone());
    ledger.confirm("accept:target-a", confirmation);
    assert_eq!(ledger.len(), 1, "原命令只生效一次");

    // 离线命令在窗口内可按当前授权重放，且不重复生效。
    let device = DeviceGrant::new("device-a", "user-1", "project-1")
        .with_device(&[DeviceAction::View])
        .with_user(&[DeviceAction::View])
        .with_channels(&[Channel::Mobile]);
    let command = OfflineCommand {
        command_id: "cmd-md01".to_string(),
        action: DeviceAction::View,
        channel: Channel::Mobile,
        created_at_ms: 1_000,
        deliver_by_ms: 61_000,
        token_expires_at_ms: 31_000,
    };
    let replays = replay_offline(&device, std::slice::from_ref(&command), 2_000);
    assert_eq!(replays[0].verdict, ReplayVerdict::Ready);
    let expired = replay_offline(&device, &[command], 120_000);
    assert_eq!(expired[0].verdict, ReplayVerdict::PendingDelivery);
}
