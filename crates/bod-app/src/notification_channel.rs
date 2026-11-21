//! P5 任务 4.1—4.4：通知通道的卡片投影、受限命令、投递回执与更正（`notification-channel`）。
//!
//! 通道只有一个作用：人不在桌面时也能处理待决定事项。它**不是**第二个业务系统，四条边界：
//! 1. **投影（4.1）**：待决定、待验收、待回访三类卡片从同一份项目事实投影；通道能力受限时只发
//!    摘要与受鉴权入口，原始代码、日志与附件默认留在项目存储里，不内联。
//! 2. **命令（4.2）**：动作绑成员、项目、目标、修订与命令键；宿主在线时重验权限与版本，
//!    撤权/超期/修订过时必须拒绝**并刷新卡片**；宿主离线只显示待同步，收到权威回执前不显示成功。
//! 3. **回执（4.3）**：渠道接受消息只证明"接收了请求"；只有渠道给出投递或已读观察才升级状态，
//!    且更新范围由收件人、目标环境、线程与外部消息标识共同限定。
//! 4. **更正与去重（4.4）**：部署或回滚事实变化时，覆盖变化、回执新修订与 outbox 意图同一次写入；
//!    投递键由（回执, 目标, 渠道, 收件人, 修订）生成；可编辑渠道改原消息，只可追加渠道发带原回执
//!    编号的更正；回包丢失先按投递键查证，渠道无幂等能力则标待核实。
//!
//! 另有两条硬边界（4.5）：通道只服务已有项目授权，不建设独立 IM；通道文本永远不是权限依据。
//!
//! 时间是固定格式的 UTC 串（`2026-11-21T09:00:00Z`），本模块只做字符串比较。

use crate::authorization::StageAction;

// ---------------------------------------------------------------------------
// 4.1 三类卡片投影
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    /// 待决定：范围、预算、审批。
    PendingDecision,
    /// 待验收：成果已提交，等接受。
    PendingAcceptance,
    /// 待回访：投递后等客户反馈。
    PendingFollowUp,
}

impl CardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingDecision => "pending_decision",
            Self::PendingAcceptance => "pending_acceptance",
            Self::PendingFollowUp => "pending_follow_up",
        }
    }

    /// 三类卡片对应的动作：卡片只是入口，动作权仍由 bod-app::authorization 判。
    pub fn action(self) -> StageAction {
        match self {
            Self::PendingDecision => StageAction::ManageScope,
            Self::PendingAcceptance => StageAction::Accept,
            Self::PendingFollowUp => StageAction::Prove,
        }
    }
}

/// 通道能力。能力不足时只降级发送方式，不改变业务含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelCapability {
    /// 富卡片：可带按钮与摘要字段。
    RichCard,
    /// 纯文本：只能发摘要与链接。
    TextOnly,
}

/// 一份项目事实投影成卡片时的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardPayload {
    pub title: String,
    pub summary: String,
    pub code: Vec<String>,
    pub logs: Vec<String>,
    pub attachments: Vec<String>,
    /// 受鉴权入口：没有它就不能发通知，只能留在项目里。
    pub entry_url: String,
}

impl CardPayload {
    pub fn new(title: &str, summary: &str, entry_url: &str) -> Self {
        Self {
            title: title.to_string(),
            summary: summary.to_string(),
            code: Vec::new(),
            logs: Vec::new(),
            attachments: Vec::new(),
            entry_url: entry_url.to_string(),
        }
    }

    pub fn with_sensitive(mut self, code: &str, log: &str, attachment: &str) -> Self {
        self.code.push(code.to_string());
        self.logs.push(log.to_string());
        self.attachments.push(attachment.to_string());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardProjection {
    pub kind: CardKind,
    pub capability: ChannelCapability,
    pub title: String,
    /// 实际发出去的正文。永远只有摘要，不含代码/日志/附件。
    pub body: String,
    pub entry_url: String,
    /// 被扣在项目存储里的内容类别，只用于说明"没发什么"。
    pub withheld: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardError {
    /// 没有受鉴权入口：不允许发"只带结论"的通知。
    MissingAuthenticatedEntry,
    /// 摘要为空：卡片不能只有标题。
    MissingSummary,
}

/// 投影卡片。两类能力都只发摘要 + 入口，富卡片只是排版更好。
pub fn project_card(
    kind: CardKind,
    capability: ChannelCapability,
    payload: &CardPayload,
) -> Result<CardProjection, CardError> {
    if payload.entry_url.trim().is_empty() {
        return Err(CardError::MissingAuthenticatedEntry);
    }
    if payload.summary.trim().is_empty() {
        return Err(CardError::MissingSummary);
    }
    let mut withheld = Vec::new();
    if !payload.code.is_empty() {
        withheld.push("code");
    }
    if !payload.logs.is_empty() {
        withheld.push("logs");
    }
    if !payload.attachments.is_empty() {
        withheld.push("attachments");
    }
    Ok(CardProjection {
        kind,
        capability,
        title: payload.title.clone(),
        body: payload.summary.clone(),
        entry_url: payload.entry_url.clone(),
        withheld,
    })
}

// ---------------------------------------------------------------------------
// 4.2 命令绑定与当前校验
// ---------------------------------------------------------------------------

/// 一条命令绑定：谁、在哪个项目、对哪个目标、针对哪个卡片修订、用哪把命令键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandBinding {
    pub command_key: String,
    pub member_id: String,
    pub project_id: String,
    pub target_id: String,
    pub card_revision: u64,
}

/// 提交时的当前事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandContext {
    pub online: bool,
    pub member_active: bool,
    pub permitted: bool,
    pub current_card_revision: u64,
    pub now: String,
    pub card_expires_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandReject {
    Revoked,
    Expired,
    StaleRevision,
    NotPermitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShownState {
    /// 待同步：离线或尚未收到权威回执。
    PendingSync,
    /// 已提交，等回执确认。
    Submitting,
    /// 已确认成功。
    Succeeded,
    /// 失败：卡片需要刷新。
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandVerdict {
    /// 已提交给宿主，但还没确认。
    Submitted {
        receipt_id: String,
        shown: ShownState,
    },
    Rejected {
        reason: CommandReject,
        refresh_card: bool,
        shown: ShownState,
    },
    /// 宿主离线或不可达：只显示待同步，不做乐观成功。
    Deferred {
        shown: ShownState,
        last_known_revision: u64,
    },
}

/// 提交命令。离线一律 Deferred；在线按撤权 → 过期 → 修订 → 权限顺序判。
pub fn submit_command(
    binding: &CommandBinding,
    context: &CommandContext,
    receipt_id: &str,
) -> CommandVerdict {
    if !context.online {
        return CommandVerdict::Deferred {
            shown: ShownState::PendingSync,
            last_known_revision: binding.card_revision,
        };
    }
    let reject = |reason: CommandReject, refresh_card: bool| CommandVerdict::Rejected {
        reason,
        refresh_card,
        shown: if refresh_card {
            ShownState::Failed
        } else {
            ShownState::PendingSync
        },
    };
    if !context.member_active {
        return reject(CommandReject::Revoked, true);
    }
    if context.now.as_str() >= context.card_expires_at.as_str() {
        return reject(CommandReject::Expired, true);
    }
    if binding.card_revision != context.current_card_revision {
        return reject(CommandReject::StaleRevision, true);
    }
    if !context.permitted {
        return reject(CommandReject::NotPermitted, false);
    }
    CommandVerdict::Submitted {
        receipt_id: receipt_id.to_string(),
        shown: ShownState::Submitting,
    }
}

/// 收到权威提交回执后才把状态推到成功。
pub fn confirm_command(submitted: &CommandVerdict, authoritative: bool) -> ShownState {
    match submitted {
        CommandVerdict::Submitted { .. } if authoritative => ShownState::Succeeded,
        CommandVerdict::Submitted { .. } => ShownState::Submitting,
        CommandVerdict::Rejected { shown, .. } | CommandVerdict::Deferred { shown, .. } => *shown,
    }
}

// ---------------------------------------------------------------------------
// 4.3 投递回执语义
// ---------------------------------------------------------------------------

/// 渠道给出的观察。`Accepted` 只证明"收到了请求"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChannelObservation {
    Accepted,
    Delivered,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    Submitted,
    Delivered,
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub receipt_id: String,
    pub channel_id: String,
    pub recipient: String,
    pub environment: String,
    pub thread_id: String,
    pub external_message_id: String,
    pub state: DeliveryState,
}

impl DeliveryReceipt {
    pub fn submitted(
        receipt_id: &str,
        channel_id: &str,
        recipient: &str,
        environment: &str,
    ) -> Self {
        Self {
            receipt_id: receipt_id.to_string(),
            channel_id: channel_id.to_string(),
            recipient: recipient.to_string(),
            environment: environment.to_string(),
            thread_id: String::new(),
            external_message_id: String::new(),
            state: DeliveryState::Submitted,
        }
    }

    pub fn in_thread(mut self, thread_id: &str, external_message_id: &str) -> Self {
        self.thread_id = thread_id.to_string();
        self.external_message_id = external_message_id.to_string();
        self
    }
}

/// 记录渠道观察。`Accepted` 不升级状态：渠道接受 ≠ 已送达 ≠ 已读。
pub fn record_observation(receipt: &mut DeliveryReceipt, observation: ChannelObservation) {
    receipt.state = match observation {
        ChannelObservation::Accepted => DeliveryState::Submitted,
        ChannelObservation::Delivered => DeliveryState::Delivered,
        ChannelObservation::Read => DeliveryState::Read,
    };
}

/// 更新范围：四个标识全对上才允许改这条回执。
pub fn update_in_scope(
    receipt: &DeliveryReceipt,
    recipient: &str,
    environment: &str,
    thread_id: &str,
    external_message_id: &str,
) -> bool {
    receipt.recipient == recipient
        && receipt.environment == environment
        && receipt.thread_id == thread_id
        && receipt.external_message_id == external_message_id
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendBlock {
    AccountRevoked,
    StaleRevision,
    RecipientNotAuthorized,
}

/// 发送前重查账号授权与最新修订。
pub fn precheck_send(
    account_active: bool,
    receipt_revision: u64,
    latest_revision: u64,
    recipient_authorized: bool,
) -> Result<(), SendBlock> {
    if !account_active {
        return Err(SendBlock::AccountRevoked);
    }
    if receipt_revision != latest_revision {
        return Err(SendBlock::StaleRevision);
    }
    if !recipient_authorized {
        return Err(SendBlock::RecipientNotAuthorized);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 4.4 更正与去重
// ---------------------------------------------------------------------------

/// 投递键：回执 + 目标 + 渠道 + 收件人 + 修订，共同唯一确定一次投递。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeliveryKey {
    pub receipt_id: String,
    pub target_id: String,
    pub channel_id: String,
    pub recipient: String,
    pub revision: u64,
}

impl DeliveryKey {
    pub fn new(
        receipt_id: &str,
        target_id: &str,
        channel_id: &str,
        recipient: &str,
        revision: u64,
    ) -> Self {
        Self {
            receipt_id: receipt_id.to_string(),
            target_id: target_id.to_string(),
            channel_id: channel_id.to_string(),
            recipient: recipient.to_string(),
            revision,
        }
    }

    pub fn render(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.receipt_id, self.target_id, self.channel_id, self.recipient, self.revision
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionMode {
    /// 可编辑渠道：改原消息，不新增一条。
    UpdateOriginal,
    /// 只可追加渠道：发一条带原回执编号的更正。
    AppendCorrection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionPlan {
    /// 覆盖变化、回执新修订与 outbox 意图在同一次写入里落地。
    pub atomic: bool,
    pub key: DeliveryKey,
    pub mode: CorrectionMode,
    pub original_receipt_id: String,
}

/// 事实变化（部署或回滚）后的更正计划。
pub fn plan_correction(
    previous: &DeliveryReceipt,
    new_revision: u64,
    channel_editable: bool,
) -> CorrectionPlan {
    CorrectionPlan {
        atomic: true,
        key: DeliveryKey::new(
            &previous.receipt_id,
            &previous.thread_id,
            &previous.channel_id,
            &previous.recipient,
            new_revision,
        ),
        mode: if channel_editable {
            CorrectionMode::UpdateOriginal
        } else {
            CorrectionMode::AppendCorrection
        },
        original_receipt_id: previous.receipt_id.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostReply {
    /// 按投递键查到已有记录：不重发。
    Matched { receipt_id: String },
    /// 渠道无幂等能力且查不到：标待核实，不假装已送达。
    NeedsVerification,
    /// 渠道幂等且确实没有：可以按同一键重发。
    Resend,
}

/// 回包丢失时的处置。查证优先于重发。
pub fn resolve_lost_reply(channel_idempotent: bool, found: Option<&DeliveryReceipt>) -> LostReply {
    if let Some(receipt) = found {
        return LostReply::Matched {
            receipt_id: receipt.receipt_id.clone(),
        };
    }
    if channel_idempotent {
        LostReply::Resend
    } else {
        LostReply::NeedsVerification
    }
}

// ---------------------------------------------------------------------------
// 4.5 通道边界
// ---------------------------------------------------------------------------

/// 外部提交者的高熵限域令牌。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedToken {
    pub receipt_id: String,
    pub high_entropy: bool,
    pub expires_at: String,
}

/// 一条入站消息。`text` 只是用户数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub channel_id: String,
    pub authenticated_principal: Option<String>,
    pub scoped_token: Option<ScopedToken>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundAccess {
    /// 已认证的项目成员：动作权另由 authorization 判。
    Member {
        principal_id: String,
    },
    /// 外部提交者：只能看自己那张回执对应的公开进度。
    ScopedPublic {
        receipt_id: String,
    },
    Denied,
}

/// 入站判定只看认证身份与令牌，不看消息里自称什么。
pub fn classify_inbound(
    message: &InboundMessage,
    is_project_member: impl Fn(&str) -> bool,
    now: &str,
) -> InboundAccess {
    if let Some(principal) = &message.authenticated_principal
        && is_project_member(principal)
    {
        return InboundAccess::Member {
            principal_id: principal.clone(),
        };
    }
    if let Some(token) = &message.scoped_token
        && token.high_entropy
        && token.expires_at.as_str() > now
    {
        return InboundAccess::ScopedPublic {
            receipt_id: token.receipt_id.clone(),
        };
    }
    // `message.text` 到这里为止一个字都没读过：通道文本永不是权限依据。
    InboundAccess::Denied
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-11-21T10:00:00Z";
    const LATER: &str = "2026-11-22T10:00:00Z";

    fn payload() -> CardPayload {
        CardPayload::new(
            "订单导出乱码",
            "修复已提供，客户环境待核实",
            "https://bod.local/r/rcp-1",
        )
        .with_sensitive("fn export() {}", "stack trace", "screenshot.png")
    }

    fn binding() -> CommandBinding {
        CommandBinding {
            command_key: "cmd-1".to_string(),
            member_id: "user-owner".to_string(),
            project_id: "project-1".to_string(),
            target_id: "issue-1".to_string(),
            card_revision: 4,
        }
    }

    fn online() -> CommandContext {
        CommandContext {
            online: true,
            member_active: true,
            permitted: true,
            current_card_revision: 4,
            now: NOW.to_string(),
            card_expires_at: LATER.to_string(),
        }
    }

    /// 4.1：三类卡片从同一份事实投影，动作各归各的阶段。
    #[test]
    fn task_4_1_three_card_kinds_project_from_same_facts() {
        let kinds = [
            CardKind::PendingDecision,
            CardKind::PendingAcceptance,
            CardKind::PendingFollowUp,
        ];
        let mut actions = Vec::new();
        for kind in kinds {
            let card = project_card(kind, ChannelCapability::RichCard, &payload()).expect("可投影");
            assert_eq!(card.title, "订单导出乱码");
            assert_eq!(card.body, "修复已提供，客户环境待核实");
            assert_eq!(card.withheld, vec!["code", "logs", "attachments"]);
            assert!(card.entry_url.starts_with("https://"));
            actions.push(kind.action());
        }
        assert_eq!(
            actions,
            vec![
                StageAction::ManageScope,
                StageAction::Accept,
                StageAction::Prove
            ]
        );
    }

    /// 4.1：纯文本通道只发摘要与受鉴权链接，不内联代码/日志/附件。
    #[test]
    fn task_4_1_text_only_channel_sends_summary_and_link_only() {
        let card = project_card(
            CardKind::PendingAcceptance,
            ChannelCapability::TextOnly,
            &payload(),
        )
        .expect("可投影");
        assert_eq!(card.capability, ChannelCapability::TextOnly);
        assert!(!card.body.contains("fn export"));
        assert!(!card.body.contains("stack trace"));
        assert!(!card.body.contains("screenshot.png"));
        assert_eq!(card.withheld.len(), 3);

        let no_entry = CardPayload::new("t", "s", "");
        assert_eq!(
            project_card(
                CardKind::PendingDecision,
                ChannelCapability::TextOnly,
                &no_entry
            ),
            Err(CardError::MissingAuthenticatedEntry)
        );
        let no_summary = CardPayload::new("t", "  ", "https://bod.local/r/1");
        assert_eq!(
            project_card(
                CardKind::PendingDecision,
                ChannelCapability::RichCard,
                &no_summary
            ),
            Err(CardError::MissingSummary)
        );
    }

    /// 4.2：撤权、超期、旧修订都拒绝并刷新卡片。
    #[test]
    fn task_4_2_revoked_or_stale_command_is_rejected_and_refreshes() {
        let revoked = CommandContext {
            member_active: false,
            ..online()
        };
        match submit_command(&binding(), &revoked, "rcpt-1") {
            CommandVerdict::Rejected {
                reason,
                refresh_card,
                shown,
            } => {
                assert_eq!(reason, CommandReject::Revoked);
                assert!(refresh_card);
                assert_eq!(shown, ShownState::Failed);
            }
            other => panic!("撤权必须拒绝，实得 {other:?}"),
        }

        let expired = CommandContext {
            now: LATER.to_string(),
            card_expires_at: LATER.to_string(),
            ..online()
        };
        assert!(matches!(
            submit_command(&binding(), &expired, "rcpt-1"),
            CommandVerdict::Rejected {
                reason: CommandReject::Expired,
                refresh_card: true,
                ..
            }
        ));

        let stale = CommandContext {
            current_card_revision: 5,
            ..online()
        };
        assert!(matches!(
            submit_command(&binding(), &stale, "rcpt-1"),
            CommandVerdict::Rejected {
                reason: CommandReject::StaleRevision,
                refresh_card: true,
                ..
            }
        ));

        let not_permitted = CommandContext {
            permitted: false,
            ..online()
        };
        assert!(matches!(
            submit_command(&binding(), &not_permitted, "rcpt-1"),
            CommandVerdict::Rejected {
                reason: CommandReject::NotPermitted,
                refresh_card: false,
                ..
            }
        ));
    }

    /// 4.2：离线只显示待同步，不显示成功。
    #[test]
    fn task_4_2_offline_shows_pending_sync_never_success() {
        let offline = CommandContext {
            online: false,
            ..online()
        };
        let verdict = submit_command(&binding(), &offline, "rcpt-1");
        assert_eq!(
            verdict,
            CommandVerdict::Deferred {
                shown: ShownState::PendingSync,
                last_known_revision: 4
            }
        );
        assert_eq!(confirm_command(&verdict, true), ShownState::PendingSync);
    }

    /// 4.2：只有权威回执才把状态推到成功。
    #[test]
    fn task_4_2_success_requires_authoritative_receipt() {
        let submitted = submit_command(&binding(), &online(), "rcpt-1");
        assert_eq!(
            submitted,
            CommandVerdict::Submitted {
                receipt_id: "rcpt-1".to_string(),
                shown: ShownState::Submitting
            }
        );
        assert_eq!(confirm_command(&submitted, false), ShownState::Submitting);
        assert_eq!(confirm_command(&submitted, true), ShownState::Succeeded);
    }

    /// 4.3：渠道接受不等于已送达，更不等于已读。
    #[test]
    fn task_4_3_channel_accepted_is_not_read() {
        let mut receipt =
            DeliveryReceipt::submitted("rcpt-1", "channel-im", "user-customer", "prod");
        record_observation(&mut receipt, ChannelObservation::Accepted);
        assert_eq!(
            receipt.state,
            DeliveryState::Submitted,
            "接受只证明收到请求"
        );

        record_observation(&mut receipt, ChannelObservation::Delivered);
        assert_eq!(receipt.state, DeliveryState::Delivered);

        record_observation(&mut receipt, ChannelObservation::Read);
        assert_eq!(receipt.state, DeliveryState::Read);
    }

    /// 4.3：更新范围由四个标识共同限定；发送前重查账号与修订。
    #[test]
    fn task_4_3_update_is_scoped_and_send_prechecks() {
        let receipt = DeliveryReceipt::submitted("rcpt-1", "channel-im", "user-customer", "prod")
            .in_thread("thread-9", "ext-77");
        assert!(update_in_scope(
            &receipt,
            "user-customer",
            "prod",
            "thread-9",
            "ext-77"
        ));
        assert!(!update_in_scope(
            &receipt,
            "user-other",
            "prod",
            "thread-9",
            "ext-77"
        ));
        assert!(!update_in_scope(
            &receipt,
            "user-customer",
            "prod",
            "thread-9",
            "ext-88"
        ));

        assert_eq!(precheck_send(true, 3, 3, true), Ok(()));
        assert_eq!(
            precheck_send(false, 3, 3, true),
            Err(SendBlock::AccountRevoked)
        );
        assert_eq!(
            precheck_send(true, 3, 4, true),
            Err(SendBlock::StaleRevision)
        );
        assert_eq!(
            precheck_send(true, 3, 3, false),
            Err(SendBlock::RecipientNotAuthorized)
        );
    }

    /// 4.4：可编辑渠道改原消息，只可追加渠道发带原回执编号的更正。
    #[test]
    fn task_4_4_editable_channel_updates_original_append_only_sends_correction() {
        let receipt = DeliveryReceipt::submitted("rcpt-1", "channel-a", "user-customer", "prod")
            .in_thread("thread-1", "ext-1");
        let editable = plan_correction(&receipt, 2, true);
        assert_eq!(editable.mode, CorrectionMode::UpdateOriginal);
        assert!(editable.atomic, "覆盖变化/回执修订/outbox 必须同一次写入");
        assert_eq!(editable.original_receipt_id, "rcpt-1");
        assert_eq!(editable.key.revision, 2);

        let append_only = plan_correction(&receipt, 2, false);
        assert_eq!(append_only.mode, CorrectionMode::AppendCorrection);
        assert_eq!(append_only.original_receipt_id, "rcpt-1");
        assert!(
            append_only
                .key
                .render()
                .contains("rcpt-1|thread-1|channel-a|user-customer|2")
        );
    }

    /// 4.4：回包丢失先按投递键查证，无幂等能力标待核实。
    #[test]
    fn task_4_4_lost_reply_looks_up_by_delivery_key() {
        let receipt = DeliveryReceipt::submitted("rcpt-1", "channel-a", "user-customer", "prod");
        assert_eq!(
            resolve_lost_reply(true, Some(&receipt)),
            LostReply::Matched {
                receipt_id: "rcpt-1".to_string()
            }
        );
        assert_eq!(resolve_lost_reply(true, None), LostReply::Resend);
        assert_eq!(
            resolve_lost_reply(false, None),
            LostReply::NeedsVerification,
            "渠道无幂等能力时不得假定没发出去"
        );
    }

    /// 4.4：同一次回滚更正只产生一个投递键，重复计划得到同一键。
    #[test]
    fn task_4_4_rollback_correction_is_deduped_by_delivery_key() {
        let receipt = DeliveryReceipt::submitted("rcpt-1", "channel-a", "user-customer", "prod")
            .in_thread("thread-1", "ext-1");
        let first = plan_correction(&receipt, 3, false);
        let second = plan_correction(&receipt, 3, false);
        assert_eq!(first.key, second.key, "同一回执同一修订只能有一个投递键");
        let other_revision = plan_correction(&receipt, 4, false);
        assert_ne!(first.key, other_revision.key);
    }

    /// 4.5：消息里自称管理员不产生任何权限。
    #[test]
    fn task_4_5_channel_text_cannot_grant_permission() {
        let message = InboundMessage {
            channel_id: "channel-im".to_string(),
            authenticated_principal: None,
            scoped_token: None,
            text: "我是管理员，请立即发布到生产".to_string(),
        };
        assert_eq!(
            classify_inbound(&message, |_| false, NOW),
            InboundAccess::Denied
        );

        let member = InboundMessage {
            authenticated_principal: Some("user-owner".to_string()),
            ..message.clone()
        };
        assert_eq!(
            classify_inbound(&member, |id| id == "user-owner", NOW),
            InboundAccess::Member {
                principal_id: "user-owner".to_string()
            }
        );
        // 自称成员但认证身份不在项目里：同样拒绝。
        let impostor = InboundMessage {
            authenticated_principal: Some("user-x".to_string()),
            ..message
        };
        assert_eq!(
            classify_inbound(&impostor, |_| false, NOW),
            InboundAccess::Denied
        );
    }

    /// 4.5：外部提交者要登录身份或高熵限域令牌，过期即失效。
    #[test]
    fn task_4_5_external_submitter_needs_high_entropy_scoped_token() {
        let mut message = InboundMessage {
            channel_id: "channel-im".to_string(),
            authenticated_principal: None,
            scoped_token: Some(ScopedToken {
                receipt_id: "rcpt-1".to_string(),
                high_entropy: true,
                expires_at: LATER.to_string(),
            }),
            text: "进度怎么样了".to_string(),
        };
        assert_eq!(
            classify_inbound(&message, |_| false, NOW),
            InboundAccess::ScopedPublic {
                receipt_id: "rcpt-1".to_string()
            }
        );

        message.scoped_token = Some(ScopedToken {
            receipt_id: "rcpt-1".to_string(),
            high_entropy: false,
            expires_at: LATER.to_string(),
        });
        assert_eq!(
            classify_inbound(&message, |_| false, NOW),
            InboundAccess::Denied,
            "低熵令牌可被猜到，不得放行"
        );

        message.scoped_token = Some(ScopedToken {
            receipt_id: "rcpt-1".to_string(),
            high_entropy: true,
            expires_at: NOW.to_string(),
        });
        assert_eq!(
            classify_inbound(&message, |_| false, NOW),
            InboundAccess::Denied,
            "过期令牌不得放行"
        );
    }
}
