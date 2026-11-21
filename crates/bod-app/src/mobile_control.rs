//! P6 任务 4.1—4.5：移动伴随审批与客户 H5 回执的分期边界（`mobile-control`）。
//!
//! 移动端只承接"看得见、签得准、记得住"，业务决定仍由宿主任裁。五道边界：
//! 1. **分期开放（4.1）**：先桌面交付与双机执行，再伴随/推送/待办/签名审批与客户 H5 提交回执，
//!    之后才补 diff 批注、发布、覆盖视图；宣称"手机完成全流程"必须同时有完整链路与 MD01—MD04
//!    目标平台证据，模拟器不算。
//! 2. **设备签名（4.2 / MD02）**：签名绑定成果摘要与修订；旧签名不能接受新成果，生物识别取消
//!    或密钥撤销即阻断，重新核对并签署新修订后才生成有效回执。
//! 3. **离线投递（4.3 / RM05、MD03）**：命令有投递期限与容量；授权到期只换令牌入箱，到期返回
//!    待投递，宿主恢复才可能生效；提醒推送零业务副作用。
//! 4. **客户 H5（4.4 / SB04）**：门户只有受限提交、查询与回执，没有工具启动权、工作区挂载与
//!    生产凭据；打开链接只记访问。
//! 5. **重复确认与回滚（4.5 / MD04）**：重复确认只记一次决定，回滚后证明页显示当前覆盖并保留
//!    历史，成本与结算分别可核对；撤权后离线重放受新授权约束。
//!
//! 本模块是纯值判断：不起进程、不发推送、不挂载工作区、不落表。4.6 的真机证据不在本模块。

use std::collections::BTreeMap;

use crate::remote_delegation::{Channel, DeviceAction, DeviceGrant};

// ---------------------------------------------------------------------------
// 4.1 分期开放
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// 第一阶段：桌面交付与双机执行。
    DesktopAndDualMachine,
    /// 第二阶段：负责人伴随模式、推送、待办、签名审批与客户 H5 提交/回执。
    CompanionAndPortal,
    /// 第三阶段：diff 批注、发布、覆盖视图等。
    DiffAndPublish,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DesktopAndDualMachine => "desktop_and_dual_machine",
            Self::CompanionAndPortal => "companion_and_portal",
            Self::DiffAndPublish => "diff_and_publish",
        }
    }
}

/// 目标平台证据。缺一项就不能宣称对应范围。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformEvidence {
    pub ios_real_device: bool,
    pub android_real_device: bool,
    pub md01: bool,
    pub md02: bool,
    pub md03: bool,
    pub md04: bool,
}

impl PlatformEvidence {
    pub fn only_simulators() -> Self {
        Self::default()
    }

    pub fn complete() -> Self {
        Self {
            ios_real_device: true,
            android_real_device: true,
            md01: true,
            md02: true,
            md03: true,
            md04: true,
        }
    }

    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.ios_real_device {
            missing.push("ios 真机报告");
        }
        if !self.android_real_device {
            missing.push("android 真机报告");
        }
        for (flag, name) in [
            (self.md01, "MD01"),
            (self.md02, "MD02"),
            (self.md03, "MD03"),
            (self.md04, "MD04"),
        ] {
            if !flag {
                missing.push(name);
            }
        }
        missing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimScope {
    /// "手机完成全流程"。
    FullFlowOnMobile,
    /// 只宣称签名审批。
    SignatureApprovalOnly,
    /// 只宣称客户 H5 提交与回执。
    PortalReceiptOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimVerdict {
    Allowed {
        scope: ClaimScope,
    },
    /// 缺真机证据：阻止宣称并给出未完成项清单。
    Blocked {
        missing: Vec<&'static str>,
    },
}

/// 发布宣称前的门禁。三项口径不同：全流程要齐备，单项能力要对应证据。
pub fn may_claim(phase: Phase, evidence: &PlatformEvidence, scope: ClaimScope) -> ClaimVerdict {
    if phase < Phase::CompanionAndPortal {
        return ClaimVerdict::Blocked {
            missing: vec!["阶段未到伴随/门户"],
        };
    }
    let missing = match scope {
        ClaimScope::FullFlowOnMobile => evidence.missing(),
        ClaimScope::SignatureApprovalOnly => {
            let mut missing = Vec::new();
            if !evidence.md02 {
                missing.push("MD02");
            }
            missing
        }
        ClaimScope::PortalReceiptOnly => {
            let mut missing = Vec::new();
            if !evidence.md04 {
                missing.push("MD04");
            }
            missing
        }
    };
    if missing.is_empty() {
        ClaimVerdict::Allowed { scope }
    } else {
        ClaimVerdict::Blocked { missing }
    }
}

// ---------------------------------------------------------------------------
// 4.2 设备签名与修订绑定（MD02）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSignature {
    pub device_id: String,
    pub key_id: String,
    /// 签名时看到的成果摘要。
    pub artifact_digest: String,
    /// 签名时看到的修订号。
    pub revision: u32,
    pub signed_at_ms: i64,
    pub biometric_confirmed: bool,
    pub key_revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceTarget {
    pub artifact_digest: String,
    pub revision: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureVerdict {
    Valid,
    RejectedKeyRevoked,
    RejectedBiometricCancelled,
    RejectedDigestChanged { signed: String, current: String },
    RejectedRevisionChanged { signed: u32, current: u32 },
}

/// 验签顺序：密钥 → 生物识别 → 成果摘要 → 修订。前三项任一不过都不谈"这是哪一版的签名"。
pub fn verify_signature(
    signature: &DeviceSignature,
    target: &AcceptanceTarget,
) -> SignatureVerdict {
    if signature.key_revoked {
        return SignatureVerdict::RejectedKeyRevoked;
    }
    if !signature.biometric_confirmed {
        return SignatureVerdict::RejectedBiometricCancelled;
    }
    if signature.artifact_digest != target.artifact_digest {
        return SignatureVerdict::RejectedDigestChanged {
            signed: signature.artifact_digest.clone(),
            current: target.artifact_digest.clone(),
        };
    }
    if signature.revision != target.revision {
        return SignatureVerdict::RejectedRevisionChanged {
            signed: signature.revision,
            current: target.revision,
        };
    }
    SignatureVerdict::Valid
}

/// 修订变化后的提示：必须点名"看到的是哪一版、现在是哪一版"。
pub const fn recheck_prompt(
    signature: &DeviceSignature,
    target: &AcceptanceTarget,
) -> &'static str {
    if signature.revision == target.revision {
        "无需重新核对"
    } else {
        "修订已变化，需重新核对并签署"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedReceipt {
    pub receipt_id: String,
    pub device_id: String,
    pub revision: u32,
    pub artifact_digest: String,
}

/// 只有验签通过且重新签署当前修订，才生成有效回执。
pub fn issue_receipt(
    signature: &DeviceSignature,
    target: &AcceptanceTarget,
    receipt_id: &str,
) -> Result<SignedReceipt, SignatureVerdict> {
    match verify_signature(signature, target) {
        SignatureVerdict::Valid => Ok(SignedReceipt {
            receipt_id: receipt_id.to_string(),
            device_id: signature.device_id.clone(),
            revision: target.revision,
            artifact_digest: target.artifact_digest.clone(),
        }),
        other => Err(other),
    }
}

// ---------------------------------------------------------------------------
// 4.3 离线投递边界（RM05 / MD03）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineCommand {
    pub command_id: String,
    pub action: DeviceAction,
    pub channel: Channel,
    pub created_at_ms: i64,
    /// 投递窗口的截止：过了就返回待投递。
    pub deliver_by_ms: i64,
    /// 令牌授权到期：可换新令牌，但不等于可以自动执行。
    pub token_expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineBox {
    pub capacity: usize,
    pub commands: Vec<OfflineCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued { size: usize },
    RejectedFull { capacity: usize },
}

impl OfflineBox {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            commands: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn enqueue(&mut self, command: OfflineCommand) -> EnqueueOutcome {
        if self.commands.len() >= self.capacity {
            return EnqueueOutcome::RejectedFull {
                capacity: self.capacity,
            };
        }
        self.commands.push(command);
        EnqueueOutcome::Queued {
            size: self.commands.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenRefresh {
    /// 有效授权换到新令牌：仍在箱里，仍需宿主恢复才可能生效。
    Renewed { token_expires_at_ms: i64 },
    /// 授权已到期：不再换令牌。
    AuthorizationExpired,
}

pub fn refresh_token(
    command: &OfflineCommand,
    valid_grant: bool,
    new_expires_at_ms: i64,
    now_ms: i64,
) -> TokenRefresh {
    if !valid_grant || command.token_expires_at_ms <= now_ms {
        return TokenRefresh::AuthorizationExpired;
    }
    TokenRefresh::Renewed {
        token_expires_at_ms: new_expires_at_ms,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryState {
    /// 窗口内、宿主未恢复：待投递。
    PendingDelivery,
    /// 窗口已过：不生效，也不再自动补投。
    Expired,
}

/// 宿主恢复时的判定。**恢复不等于生效**：过期命令只报待投递/已过期。
pub fn delivery_state(command: &OfflineCommand, now_ms: i64) -> DeliveryState {
    if now_ms >= command.deliver_by_ms {
        DeliveryState::Expired
    } else {
        DeliveryState::PendingDelivery
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// 授权仍有效，可以执行（业务生效仍由宿主命令处理器决定）。
    Ready,
    /// 离线期间被撤权或被关通道。
    RejectedNotGranted,
    /// 投递窗口已过。
    PendingDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResult {
    pub command_id: String,
    pub verdict: ReplayVerdict,
}

/// 离线重放：先看窗口，再看**当前**授权。撤权后重放一律不生效。
pub fn replay_offline(
    device: &DeviceGrant,
    commands: &[OfflineCommand],
    now_ms: i64,
) -> Vec<ReplayResult> {
    commands
        .iter()
        .map(|command| {
            let verdict = if delivery_state(command, now_ms) == DeliveryState::Expired {
                ReplayVerdict::PendingDelivery
            } else if device
                .effective_actions(command.channel)
                .contains(&command.action)
            {
                ReplayVerdict::Ready
            } else {
                ReplayVerdict::RejectedNotGranted
            };
            ReplayResult {
                command_id: command.command_id.clone(),
                verdict,
            }
        })
        .collect()
}

/// 提醒推送的副作用账：恒为零。催办不等于批准，也不等于开始执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderEffects {
    pub business_writes: u32,
    pub started_agents: u32,
    pub created_decisions: u32,
    pub escalated_to: Option<&'static str>,
}

pub const fn reminder_effects(has_authorized_owner: bool) -> ReminderEffects {
    ReminderEffects {
        business_writes: 0,
        started_agents: 0,
        created_decisions: 0,
        escalated_to: if has_authorized_owner {
            Some("authorized_owner")
        } else {
            None
        },
    }
}

// ---------------------------------------------------------------------------
// 4.4 客户 H5 门户边界（SB04）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalAction {
    Submit,
    Query,
    Receipt,
    /// 打开链接只记访问，不算任何确认。
    OpenLink,
}

impl PortalAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::Query => "query",
            Self::Receipt => "receipt",
            Self::OpenLink => "open_link",
        }
    }
}

/// 门户进程的形态。三项必须同时为"没有"，否则不是受限门户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalContext {
    pub tool_launch_enabled: bool,
    pub workspace_mounts: usize,
    pub production_credentials: usize,
}

impl PortalContext {
    pub fn confined() -> Self {
        Self {
            tool_launch_enabled: false,
            workspace_mounts: 0,
            production_credentials: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalVerdict {
    Confined,
    Rejected { violations: Vec<&'static str> },
}

pub fn check_portal(context: &PortalContext) -> PortalVerdict {
    let mut violations = Vec::new();
    if context.tool_launch_enabled {
        violations.push("门户不得拥有工具启动权");
    }
    if context.workspace_mounts > 0 {
        violations.push("门户不得挂载工作区");
    }
    if context.production_credentials > 0 {
        violations.push("门户不得持有生产凭据");
    }
    if violations.is_empty() {
        PortalVerdict::Confined
    } else {
        PortalVerdict::Rejected { violations }
    }
}

pub fn portal_allows(action: PortalAction) -> bool {
    matches!(
        action,
        PortalAction::Submit | PortalAction::Query | PortalAction::Receipt | PortalAction::OpenLink
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkVisit {
    pub visit_recorded: bool,
    /// 打开链接永远不产生决定。
    pub decision_recorded: bool,
    pub audit_ref: String,
}

pub fn on_open_link(link_id: &str) -> LinkVisit {
    LinkVisit {
        visit_recorded: true,
        decision_recorded: false,
        audit_ref: format!("link:{link_id}"),
    }
}

// ---------------------------------------------------------------------------
// 4.5 重复确认与回滚证明页（MD04）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub confirmation_id: String,
    pub decision: String,
    pub revision: u32,
    /// 生效顺序：0 表示当前未被回滚。
    pub rollback_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmVerdict {
    Recorded {
        confirmation_id: String,
    },
    /// 重复确认：只记一次决定，不新增。
    Duplicate {
        confirmation_id: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfirmationLedger {
    entries: BTreeMap<String, Confirmation>,
    history: BTreeMap<String, Vec<String>>,
}

impl ConfirmationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn confirm(&mut self, key: &str, confirmation: Confirmation) -> ConfirmVerdict {
        if let Some(existing) = self.entries.get(key) {
            return ConfirmVerdict::Duplicate {
                confirmation_id: existing.confirmation_id.clone(),
            };
        }
        self.history
            .entry(key.to_string())
            .or_default()
            .push(confirmation.confirmation_id.clone());
        self.entries.insert(key.to_string(), confirmation);
        ConfirmVerdict::Recorded {
            confirmation_id: self.entries[key].confirmation_id.clone(),
        }
    }

    /// 回滚只推进回滚序号并留痕，不删除历史确认。
    pub fn rollback(&mut self, key: &str, current: &str) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        entry.rollback_seq += 1;
        self.history
            .entry(key.to_string())
            .or_default()
            .push(format!("rollback:{}", entry.rollback_seq));
        entry.decision = format!("rolled_back->{current}");
        true
    }

    pub fn get(&self, key: &str) -> Option<&Confirmation> {
        self.entries.get(key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofPage {
    /// 当前覆盖：回滚后应显示"已回滚"而不是旧的"已确认"。
    pub current_coverage: String,
    pub rolled_back: bool,
    /// 历史确认与回滚留痕，按发生顺序。
    pub history: Vec<String>,
    /// 成本与结算分别可核对，不合并成一个数字。
    pub cost_ref: String,
    pub settlement_ref: String,
}

pub fn proof_page(ledger: &ConfirmationLedger, key: &str) -> Option<ProofPage> {
    let entry = ledger.get(key)?;
    let history = ledger
        .history
        .get(key)
        .cloned()
        .unwrap_or_else(|| vec![entry.confirmation_id.clone()]);
    Some(ProofPage {
        current_coverage: entry.decision.clone(),
        rolled_back: entry.rollback_seq > 0,
        history,
        cost_ref: format!("cost:{key}:{}", entry.confirmation_id),
        settlement_ref: format!("settlement:{key}:{}", entry.confirmation_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceGrant {
        DeviceGrant::new("device-a", "user-1", "project-1")
            .with_device(&[
                DeviceAction::View,
                DeviceAction::Dispatch,
                DeviceAction::Stop,
            ])
            .with_user(&[
                DeviceAction::View,
                DeviceAction::Dispatch,
                DeviceAction::Stop,
            ])
            .with_channels(&[Channel::Mobile])
    }

    fn signature() -> DeviceSignature {
        DeviceSignature {
            device_id: "device-a".to_string(),
            key_id: "key-1".to_string(),
            artifact_digest: "sha256:r1".to_string(),
            revision: 1,
            signed_at_ms: 1_000,
            biometric_confirmed: true,
            key_revoked: false,
        }
    }

    fn target() -> AcceptanceTarget {
        AcceptanceTarget {
            artifact_digest: "sha256:r1".to_string(),
            revision: 1,
        }
    }

    fn command(now_ms: i64) -> OfflineCommand {
        OfflineCommand {
            command_id: "cmd-1".to_string(),
            action: DeviceAction::Dispatch,
            channel: Channel::Mobile,
            created_at_ms: now_ms,
            deliver_by_ms: now_ms + 10 * 60 * 1000,
            token_expires_at_ms: now_ms + 5 * 60 * 1000,
        }
    }

    /// 4.1：只有模拟器不得宣称手机完成全流程，必须给出未完成项清单。
    #[test]
    fn task_4_1_mobile_claim_requires_real_device_evidence() {
        let simulators = PlatformEvidence::only_simulators();
        match may_claim(
            Phase::CompanionAndPortal,
            &simulators,
            ClaimScope::FullFlowOnMobile,
        ) {
            ClaimVerdict::Blocked { missing } => {
                assert!(missing.contains(&"ios 真机报告"));
                assert!(missing.contains(&"android 真机报告"));
                assert!(missing.contains(&"MD01") && missing.contains(&"MD04"));
                assert_eq!(missing.len(), 6, "未完成项要逐条列出");
            }
            other => panic!("缺真机证据必须被阻止，实得 {other:?}"),
        }

        let complete = PlatformEvidence::complete();
        assert_eq!(
            may_claim(
                Phase::CompanionAndPortal,
                &complete,
                ClaimScope::FullFlowOnMobile
            ),
            ClaimVerdict::Allowed {
                scope: ClaimScope::FullFlowOnMobile
            }
        );

        // 单项宣称只要对应证据：有 MD02 就能宣称签名审批。
        let only_sign = PlatformEvidence {
            md02: true,
            ..PlatformEvidence::only_simulators()
        };
        assert!(matches!(
            may_claim(
                Phase::CompanionAndPortal,
                &only_sign,
                ClaimScope::SignatureApprovalOnly
            ),
            ClaimVerdict::Allowed { .. }
        ));
        assert!(matches!(
            may_claim(
                Phase::CompanionAndPortal,
                &only_sign,
                ClaimScope::PortalReceiptOnly
            ),
            ClaimVerdict::Blocked { .. }
        ));
    }

    /// 4.1：阶段顺序不可跳——第二阶段之前不得宣称伴随/门户能力。
    #[test]
    fn task_4_1_phase_order_blocks_later_capabilities_early() {
        let complete = PlatformEvidence::complete();
        assert_eq!(
            may_claim(
                Phase::DesktopAndDualMachine,
                &complete,
                ClaimScope::SignatureApprovalOnly
            ),
            ClaimVerdict::Blocked {
                missing: vec!["阶段未到伴随/门户"]
            }
        );
        assert!(matches!(
            may_claim(
                Phase::DiffAndPublish,
                &complete,
                ClaimScope::FullFlowOnMobile
            ),
            ClaimVerdict::Allowed { .. }
        ));
        assert!(Phase::DesktopAndDualMachine < Phase::CompanionAndPortal);
        assert!(Phase::CompanionAndPortal < Phase::DiffAndPublish);
    }

    /// 4.2 / MD02：审阅 r1 后提交 r2，旧签名不能接受新成果。
    #[test]
    fn md02_stale_signature_cannot_accept_new_revision() {
        let new_target = AcceptanceTarget {
            artifact_digest: "sha256:r2".to_string(),
            revision: 2,
        };
        assert_eq!(
            verify_signature(&signature(), &new_target),
            SignatureVerdict::RejectedDigestChanged {
                signed: "sha256:r1".to_string(),
                current: "sha256:r2".to_string()
            }
        );
        // 摘要相同但修订号变了：同样不能接受。
        let same_digest_new_revision = AcceptanceTarget {
            artifact_digest: "sha256:r1".to_string(),
            revision: 2,
        };
        assert_eq!(
            verify_signature(&signature(), &same_digest_new_revision),
            SignatureVerdict::RejectedRevisionChanged {
                signed: 1,
                current: 2
            }
        );
        assert_eq!(
            recheck_prompt(&signature(), &same_digest_new_revision),
            "修订已变化，需重新核对并签署"
        );

        // 重新签署新修订后才有回执。
        let resigned = DeviceSignature {
            artifact_digest: "sha256:r2".to_string(),
            revision: 2,
            signed_at_ms: 2_000,
            ..signature()
        };
        let receipt = issue_receipt(&resigned, &new_target, "receipt-2").expect("新修订应可回执");
        assert_eq!(receipt.revision, 2);
        assert_eq!(receipt.artifact_digest, "sha256:r2");
        assert!(issue_receipt(&signature(), &new_target, "receipt-x").is_err());
    }

    /// 4.2：密钥撤销或生物识别取消即阻断生效。
    #[test]
    fn task_4_2_revocation_and_biometric_cancel_block_effect() {
        let revoked = DeviceSignature {
            key_revoked: true,
            ..signature()
        };
        assert_eq!(
            verify_signature(&revoked, &target()),
            SignatureVerdict::RejectedKeyRevoked
        );

        let cancelled = DeviceSignature {
            biometric_confirmed: false,
            ..signature()
        };
        assert_eq!(
            verify_signature(&cancelled, &target()),
            SignatureVerdict::RejectedBiometricCancelled
        );

        assert_eq!(
            verify_signature(&signature(), &target()),
            SignatureVerdict::Valid
        );
        // 撤销优先于其他判定：撤销后的签名即便内容全对也不通过。
        let revoked_and_stale = DeviceSignature {
            key_revoked: true,
            artifact_digest: "sha256:r9".to_string(),
            ..signature()
        };
        assert_eq!(
            verify_signature(&revoked_and_stale, &target()),
            SignatureVerdict::RejectedKeyRevoked
        );
    }

    /// 4.3 / RM05、MD03：宿主休眠超窗口后恢复，到期命令返回待投递而非生效。
    #[test]
    fn rm05_offline_delivery_expiry_returns_pending_not_applied() {
        let now = 1_000_000;
        let mut mailbox = OfflineBox::new(1);
        assert_eq!(
            mailbox.enqueue(command(now)),
            EnqueueOutcome::Queued { size: 1 }
        );
        assert_eq!(
            mailbox.enqueue(command(now)),
            EnqueueOutcome::RejectedFull { capacity: 1 },
            "容量边界要明确反馈"
        );

        let inside = command(now);
        assert_eq!(
            delivery_state(&inside, now + 60_000),
            DeliveryState::PendingDelivery
        );
        assert_eq!(
            delivery_state(&inside, now + 10 * 60 * 1000),
            DeliveryState::Expired,
            "窗口一到即不生效"
        );

        // 有效授权可换新令牌；授权到期后只返回待投递。
        assert_eq!(
            refresh_token(&inside, true, now + 30 * 60 * 1000, now + 60_000),
            TokenRefresh::Renewed {
                token_expires_at_ms: now + 30 * 60 * 1000
            }
        );
        assert_eq!(
            refresh_token(&inside, false, now + 30 * 60 * 1000, now + 60_000),
            TokenRefresh::AuthorizationExpired
        );
        assert_eq!(
            refresh_token(&inside, true, now, now + 6 * 60 * 1000),
            TokenRefresh::AuthorizationExpired,
            "令牌本身过期时换不出新令牌"
        );

        // 恢复后重放：过期命令不生效。
        let replay = replay_offline(&device(), &[inside], now + 11 * 60 * 1000);
        assert_eq!(replay[0].verdict, ReplayVerdict::PendingDelivery);
    }

    /// 4.3：提醒推送零业务副作用，到期升级给有权主体而不是自动执行。
    #[test]
    fn task_4_3_reminder_has_no_business_effect() {
        let with_owner = reminder_effects(true);
        assert_eq!(with_owner.business_writes, 0, "催办不写业务状态");
        assert_eq!(with_owner.started_agents, 0, "催办不启动执行");
        assert_eq!(with_owner.created_decisions, 0, "催办不产生决定");
        assert_eq!(with_owner.escalated_to, Some("authorized_owner"));

        let no_owner = reminder_effects(false);
        assert_eq!(no_owner.escalated_to, None, "无人处理时不升级也不执行");
        assert_eq!(no_owner.business_writes, 0);
    }

    /// 4.4 / SB04：门户无工具启动权、无工作区挂载、无生产凭据。
    #[test]
    fn sb04_portal_has_no_tool_launch_workspace_or_credentials() {
        assert_eq!(
            check_portal(&PortalContext::confined()),
            PortalVerdict::Confined
        );

        let leaking = PortalContext {
            tool_launch_enabled: true,
            workspace_mounts: 1,
            production_credentials: 2,
        };
        match check_portal(&leaking) {
            PortalVerdict::Rejected { violations } => {
                assert_eq!(violations.len(), 3, "三项违规逐条列出");
                assert!(violations.iter().any(|item| item.contains("工具启动权")));
                assert!(violations.iter().any(|item| item.contains("工作区")));
                assert!(violations.iter().any(|item| item.contains("生产凭据")));
            }
            other => panic!("门户越界必须被拒，实得 {other:?}"),
        }

        for action in [
            PortalAction::Submit,
            PortalAction::Query,
            PortalAction::Receipt,
        ] {
            assert!(portal_allows(action), "{:?} 应受限可用", action);
        }

        // 打开链接只记访问。
        let visit = on_open_link("link-7");
        assert!(visit.visit_recorded);
        assert!(!visit.decision_recorded, "打开链接不计确认");
        assert_eq!(visit.audit_ref, "link:link-7");
    }

    /// 4.5 / MD04：重复确认只记一次决定。
    #[test]
    fn md04_repeated_confirmation_recorded_once() {
        let mut ledger = ConfirmationLedger::new();
        let confirmation = Confirmation {
            confirmation_id: "confirm-1".to_string(),
            decision: "accepted".to_string(),
            revision: 3,
            rollback_seq: 0,
        };
        assert_eq!(
            ledger.confirm("accept:artifact-1", confirmation.clone()),
            ConfirmVerdict::Recorded {
                confirmation_id: "confirm-1".to_string()
            }
        );
        assert_eq!(
            ledger.confirm("accept:artifact-1", confirmation),
            ConfirmVerdict::Duplicate {
                confirmation_id: "confirm-1".to_string()
            },
            "重复确认只记一次"
        );
        assert_eq!(ledger.len(), 1);
        assert!(!ledger.is_empty());

        let page = proof_page(&ledger, "accept:artifact-1").expect("证明页应存在");
        assert_eq!(page.current_coverage, "accepted");
        assert!(!page.rolled_back);
        assert_eq!(page.history, vec!["confirm-1".to_string()]);
        assert_ne!(page.cost_ref, page.settlement_ref, "成本与结算分别可核对");
    }

    /// 4.5：回滚后证明页显示当前覆盖并保留历史。
    #[test]
    fn task_4_5_proof_page_shows_rollback_and_keeps_history() {
        let mut ledger = ConfirmationLedger::new();
        ledger.confirm(
            "accept:artifact-1",
            Confirmation {
                confirmation_id: "confirm-1".to_string(),
                decision: "accepted".to_string(),
                revision: 3,
                rollback_seq: 0,
            },
        );
        assert!(ledger.rollback("accept:artifact-1", "rollback-1"));
        let page = proof_page(&ledger, "accept:artifact-1").expect("证明页应存在");
        assert!(page.rolled_back, "回滚后必须显示已回滚");
        assert_eq!(page.current_coverage, "rolled_back->rollback-1");
        assert!(
            page.history.contains(&"confirm-1".to_string()),
            "历史确认保留"
        );
        assert!(
            page.history
                .iter()
                .any(|item| item.starts_with("rollback:")),
            "回滚留痕：{:?}",
            page.history
        );
        assert!(!ledger.rollback("accept:unknown", "x"), "未知键不推进");
        assert!(proof_page(&ledger, "accept:unknown").is_none());
    }

    /// 4.5 / RM09：双端并发至多一次有效决定，撤权后离线重放受新授权约束。
    #[test]
    fn rm09_dual_device_single_decision_and_revocation_limits_replay() {
        // 同一键两端确认：第二次是 Duplicate 而不是第二个决定。
        let mut ledger = ConfirmationLedger::new();
        ledger.confirm(
            "accept:artifact-9",
            Confirmation {
                confirmation_id: "desktop-1".to_string(),
                decision: "accepted".to_string(),
                revision: 1,
                rollback_seq: 0,
            },
        );
        assert!(matches!(
            ledger.confirm(
                "accept:artifact-9",
                Confirmation {
                    confirmation_id: "mobile-1".to_string(),
                    decision: "accepted".to_string(),
                    revision: 1,
                    rollback_seq: 0,
                }
            ),
            ConfirmVerdict::Duplicate { .. }
        ));
        assert_eq!(ledger.len(), 1);

        // 撤权：设备能力还在，但用户权限没了 → 重放不生效。
        let now = 2_000_000;
        let revoked = DeviceGrant::new("device-a", "user-1", "project-1")
            .with_device(&[DeviceAction::View, DeviceAction::Dispatch])
            .with_user(&[DeviceAction::View])
            .with_channels(&[Channel::Mobile]);
        let replay = replay_offline(&revoked, &[command(now)], now + 1_000);
        assert_eq!(replay[0].verdict, ReplayVerdict::RejectedNotGranted);

        // 通道关闭同样拒绝。
        let channel_closed = DeviceGrant::new("device-a", "user-1", "project-1")
            .with_device(&[DeviceAction::Dispatch])
            .with_user(&[DeviceAction::Dispatch]);
        assert_eq!(
            replay_offline(&channel_closed, &[command(now)], now + 1_000)[0].verdict,
            ReplayVerdict::RejectedNotGranted
        );

        // 授权仍在：可以进入业务判定（Ready 不等于已生效）。
        assert_eq!(
            replay_offline(&device(), &[command(now)], now + 1_000)[0].verdict,
            ReplayVerdict::Ready
        );
    }
}
