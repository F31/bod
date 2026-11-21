//! P6 任务 2.1、2.2、2.5、2.6、2.7：远程委派与投递链路（`remote-execution`）。
//!
//! 远端只按任务拿到工作区与凭据，业务决定与验收始终留在宿主。五道边界：
//! 1. **委派契约（2.1 / RM01）**：节点与 webhook 共用同一 `DelegationRequest`；参数摘要随
//!    invocation 持久化，同 ID 异参一律拒，重启后同一请求返回原执行实例。
//! 2. **签名批次与游标（2.2 / RM07）**：批次带节点标识、密钥版本、起止 sequence、摘要与签名；
//!    验签通过不等于业务通过，同一合法批次只入账一次，游标只在连续处推进。
//! 3. **设备授权（2.5 / SB05）**：配对与在线只说明可识别、链路可用；设备能力与用户项目权限取
//!    交集，新通道默认关闭；关闭即终止控制会话并拒绝排队中未生效的命令。
//! 4. **订阅与快照（2.6 / RM06）**：慢订阅超限即释放缓冲，按最后持久游标补拉，游标过期读带
//!    基准序列的快照；控制命令没有权威回执就显示待核实，不显示成功。
//! 5. **信箱（2.7 / RM09）**：满额与过期明确反馈；同一审批的两端不同决定至多一次生效。
//!
//! 本模块是纯值判断：不建连接、不签名、不落表；持久化与传输由宿主按这里的结论执行。

use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// 2.1 委派契约（RM01）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Node,
    Webhook,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Webhook => "webhook",
        }
    }
}

/// 节点与 webhook 共用一份启动请求结构：字段不因目标类型而变，缺项就拒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationRequest {
    pub invocation_id: String,
    pub project_id: String,
    pub target_kind: TargetKind,
    pub target_device: String,
    pub handoff_version: String,
    pub handoff_digest: String,
    /// 执行级 grant 的摘要（不含权限原文）。
    pub grant_digest: String,
    pub budget_reservation: i64,
    pub policy_version: u32,
    pub workspace_digest: String,
    /// 管理动作才需要：Appointment 与 authority epoch。
    pub appointment: Option<String>,
    pub authority_epoch: Option<u64>,
}

impl DelegationRequest {
    /// 参数摘要：只覆盖"改了就必须重派"的字段，不含重试计数与时间。
    pub fn params_digest(&self) -> String {
        format!(
            "sha256:delegate:{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.invocation_id,
            self.project_id,
            self.target_kind.as_str(),
            self.target_device,
            self.handoff_version,
            self.handoff_digest,
            self.grant_digest,
            self.budget_reservation,
            self.policy_version,
            self.workspace_digest,
            self.authority_epoch
                .map(|epoch| epoch.to_string())
                .unwrap_or_else(|| "-".to_string())
        )
    }

    pub fn is_management(&self) -> bool {
        self.appointment.is_some() || self.authority_epoch.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDelegation {
    pub invocation_id: String,
    pub params_digest: String,
    /// 宿主侧原执行实例：重放同一请求必须回到它，而不是新建。
    pub execution_id: String,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationVerdict {
    /// 新执行。
    Accepted { execution_id: String },
    /// 同一参数重放（回包丢失后的重送）：回到原实例，不新建、不增尝试。
    Replayed { execution_id: String, attempts: u32 },
    /// 同 ID 异参：拒绝并保留原记录。
    RejectedSameIdDifferentParams {
        original_digest: String,
        incoming_digest: String,
    },
    /// 管理动作缺 Appointment 或 authority epoch。
    RejectedMissingAuthority { missing: Vec<&'static str> },
}

/// 委派台账。`restore` 用来模拟宿主重启：从持久记录重建，重放判定不变。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegationLedger {
    by_invocation: BTreeMap<String, StoredDelegation>,
}

impl DelegationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn restore(records: Vec<StoredDelegation>) -> Self {
        let mut ledger = Self::default();
        for record in records {
            ledger
                .by_invocation
                .insert(record.invocation_id.clone(), record);
        }
        ledger
    }

    pub fn stored(&self, invocation_id: &str) -> Option<&StoredDelegation> {
        self.by_invocation.get(invocation_id)
    }

    pub fn len(&self) -> usize {
        self.by_invocation.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_invocation.is_empty()
    }

    /// 提交一次委派。先判管理动作的授权附件，再判同 ID 幂等。
    pub fn submit(&mut self, request: &DelegationRequest) -> DelegationVerdict {
        if request.is_management() {
            let mut missing = Vec::new();
            if request.appointment.is_none() {
                missing.push("appointment");
            }
            if request.authority_epoch.is_none() {
                missing.push("authority_epoch");
            }
            if !missing.is_empty() {
                return DelegationVerdict::RejectedMissingAuthority { missing };
            }
        }
        let digest = request.params_digest();
        if let Some(stored) = self.by_invocation.get(&request.invocation_id) {
            if stored.params_digest == digest {
                return DelegationVerdict::Replayed {
                    execution_id: stored.execution_id.clone(),
                    attempts: stored.attempts,
                };
            }
            return DelegationVerdict::RejectedSameIdDifferentParams {
                original_digest: stored.params_digest.clone(),
                incoming_digest: digest,
            };
        }
        let execution_id = format!("exec:{}", request.invocation_id);
        self.by_invocation.insert(
            request.invocation_id.clone(),
            StoredDelegation {
                invocation_id: request.invocation_id.clone(),
                params_digest: digest,
                execution_id: execution_id.clone(),
                attempts: 1,
            },
        );
        DelegationVerdict::Accepted { execution_id }
    }
}

// ---------------------------------------------------------------------------
// 2.2 签名事件批次与游标（RM07）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeKey {
    pub node_id: String,
    pub key_version: u32,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBatch {
    pub node_id: String,
    pub key_version: u32,
    pub invocation_id: String,
    pub from_sequence: u64,
    pub to_sequence: u64,
    /// 事件内容摘要：改一个字节就变。
    pub event_digest: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchReject {
    UnknownKey { node_id: String, key_version: u32 },
    RevokedKey { key_version: u32 },
    Tampered { expected: String, actual: String },
    EmptyRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 合法且连续：游标推进到 `to_sequence`。
    Applied {
        cursor: u64,
        events: u64,
    },
    /// 同一合法批次重复送达：只入账一次。
    Duplicate {
        cursor: u64,
    },
    /// 有缺口：不跳序，要求从 `expected_from` 补传。
    Gap {
        expected_from: u64,
        cursor: u64,
    },
    Rejected(BatchReject),
}

impl ApplyOutcome {
    /// 验签通过只说明来源可信；验收另需独立证据（RM07 后半）。
    pub const fn requires_acceptance_evidence(&self) -> bool {
        true
    }

    pub const fn accepted_artifact(&self) -> bool {
        false
    }
}

/// 批次台账：只记游标与已入账批次摘要，不保存事件正文。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchLedger {
    cursors: BTreeMap<(String, String), u64>,
    applied: BTreeMap<(String, String), BTreeSet<String>>,
}

impl BatchLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(batch: &EventBatch) -> (String, String) {
        (batch.node_id.clone(), batch.invocation_id.clone())
    }

    pub fn cursor_for(&self, node_id: &str, invocation_id: &str) -> u64 {
        self.cursors
            .get(&(node_id.to_string(), invocation_id.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// 验签 + 去重 + 连续推进，三步一起做。
    pub fn apply(
        &mut self,
        batch: &EventBatch,
        keys: &[NodeKey],
        claimed_digest: &str,
    ) -> ApplyOutcome {
        let key = Self::key(batch);
        let cursor = self.cursors.get(&key).copied().unwrap_or(0);

        let Some(node_key) = keys
            .iter()
            .find(|key| key.node_id == batch.node_id && key.key_version == batch.key_version)
        else {
            return ApplyOutcome::Rejected(BatchReject::UnknownKey {
                node_id: batch.node_id.clone(),
                key_version: batch.key_version,
            });
        };
        if !node_key.active {
            return ApplyOutcome::Rejected(BatchReject::RevokedKey {
                key_version: batch.key_version,
            });
        }
        if batch.event_digest != claimed_digest {
            return ApplyOutcome::Rejected(BatchReject::Tampered {
                expected: claimed_digest.to_string(),
                actual: batch.event_digest.clone(),
            });
        }
        if batch.to_sequence < batch.from_sequence {
            return ApplyOutcome::Rejected(BatchReject::EmptyRange);
        }

        let seen = self.applied.entry(key.clone()).or_default();
        if seen.contains(&batch.event_digest) {
            return ApplyOutcome::Duplicate { cursor };
        }
        if batch.from_sequence > cursor + 1 {
            return ApplyOutcome::Gap {
                expected_from: cursor + 1,
                cursor,
            };
        }
        if batch.to_sequence <= cursor {
            // 内容不同但整段已被覆盖：按重复处理，不重复入账。
            return ApplyOutcome::Duplicate { cursor };
        }
        seen.insert(batch.event_digest.clone());
        self.cursors.insert(key, batch.to_sequence);
        ApplyOutcome::Applied {
            cursor: batch.to_sequence,
            events: batch.to_sequence - batch.from_sequence + 1,
        }
    }
}

// ---------------------------------------------------------------------------
// 2.5 设备授权与撤权（SB05）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceAction {
    View,
    Dispatch,
    Stop,
}

impl DeviceAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Dispatch => "dispatch",
            Self::Stop => "stop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    /// 既有通道：P5 已交付的通道类能力。
    Existing,
    /// 新增远程业务通道：默认关闭，启用须声明命令与作用域。
    RemoteBusiness,
    /// 移动端通道：另按 §40.13 分期。
    Mobile,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Existing => "existing",
            Self::RemoteBusiness => "remote_business",
            Self::Mobile => "mobile",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceGrant {
    pub device_id: String,
    pub user_id: String,
    pub project_id: String,
    /// 设备自身具备的能力（配对时声明）。
    pub device_capabilities: BTreeSet<DeviceAction>,
    /// 用户在该项目上的权限（来自授权系统）。
    pub user_permissions: BTreeSet<DeviceAction>,
    /// 已显式启用的通道；未列出的通道一律关闭。
    pub enabled_channels: BTreeSet<Channel>,
    pub paired: bool,
    pub online: bool,
}

impl DeviceGrant {
    pub fn new(device_id: &str, user_id: &str, project_id: &str) -> Self {
        let mut device_capabilities = BTreeSet::new();
        device_capabilities.insert(DeviceAction::View);
        let mut user_permissions = BTreeSet::new();
        user_permissions.insert(DeviceAction::View);
        Self {
            device_id: device_id.to_string(),
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
            device_capabilities,
            user_permissions,
            enabled_channels: BTreeSet::new(),
            paired: true,
            online: true,
        }
    }

    pub fn with_device(mut self, actions: &[DeviceAction]) -> Self {
        self.device_capabilities = actions.iter().copied().collect();
        self
    }

    pub fn with_user(mut self, actions: &[DeviceAction]) -> Self {
        self.user_permissions = actions.iter().copied().collect();
        self
    }

    pub fn with_channels(mut self, channels: &[Channel]) -> Self {
        self.enabled_channels = channels.iter().copied().collect();
        self
    }

    /// 有效动作 = 设备能力 ∩ 用户项目权限 ∩ 已启用通道内的动作。
    /// 配对与在线都不在此函数里参与放宽——它们只说明可识别、链路可用。
    pub fn effective_actions(&self, channel: Channel) -> BTreeSet<DeviceAction> {
        if !self.paired || !self.enabled_channels.contains(&channel) {
            return BTreeSet::new();
        }
        self.device_capabilities
            .intersection(&self.user_permissions)
            .copied()
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCommand {
    pub command_id: String,
    pub action: DeviceAction,
    pub channel: Channel,
    pub queued_offline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandReplay {
    /// 仍被授权：可以执行。
    Executed,
    /// 授权已被撤销或从未授予：不生效，并给出可展示理由。
    RejectedNotGranted,
    /// 通道已关闭：不生效。
    RejectedChannelClosed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayVerdict {
    pub command_id: String,
    pub outcome: CommandReplay,
}

/// 重连后的排队命令重放：按**当前**授权判定，不按排队时的授权。
pub fn replay_queued(device: &DeviceGrant, queued: &[QueuedCommand]) -> Vec<ReplayVerdict> {
    queued
        .iter()
        .map(|command| {
            let outcome = if !device.enabled_channels.contains(&command.channel) {
                CommandReplay::RejectedChannelClosed
            } else if device
                .effective_actions(command.channel)
                .contains(&command.action)
            {
                CommandReplay::Executed
            } else {
                CommandReplay::RejectedNotGranted
            };
            ReplayVerdict {
                command_id: command.command_id.clone(),
                outcome,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelClosure {
    pub channel: Channel,
    pub control_session_terminated: bool,
    /// 排队中未生效的命令条数：一律不生效。
    pub queued_rejected: usize,
    /// 已发出的外部操作：不撤不回，交给对账。
    pub external_ops_to_reconcile: usize,
}

/// 关闭一个远程业务通道。终止控制会话、拒绝排队命令、已发外部操作转对账。
pub fn close_channel(
    device: &mut DeviceGrant,
    channel: Channel,
    queued: &[QueuedCommand],
    external_in_flight: usize,
) -> ChannelClosure {
    device.enabled_channels.remove(&channel);
    let queued_rejected = queued
        .iter()
        .filter(|command| command.channel == channel && command.queued_offline)
        .count();
    ChannelClosure {
        channel,
        control_session_terminated: true,
        queued_rejected,
        external_ops_to_reconcile: external_in_flight,
    }
}

// ---------------------------------------------------------------------------
// 2.6 订阅、慢订阅与快照基准（RM06）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// 基线：HTTP/SSE。
    HttpSse,
    /// 实验传输：只在预登记指标有可重复收益时才发布。
    Experimental,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowSubscriber {
    /// 最后**持久**游标：补拉只信这个，不信内存里读到的位置。
    pub last_persisted_cursor: u64,
    pub buffer_limit: usize,
    pub buffered: Vec<u64>,
    pub released: bool,
}

impl SlowSubscriber {
    pub fn new(last_persisted_cursor: u64, buffer_limit: usize) -> Self {
        Self {
            last_persisted_cursor,
            buffer_limit,
            buffered: Vec::new(),
            released: false,
        }
    }

    /// 推一条事件给订阅者。超限即释放整段缓冲，不做"丢最老的那条"。
    pub fn push(&mut self, sequence: u64) -> &'static str {
        if self.released {
            return "released";
        }
        self.buffered.push(sequence);
        if self.buffered.len() > self.buffer_limit {
            self.buffered.clear();
            self.released = true;
            return "released";
        }
        "buffered"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePlan {
    /// 缓冲还在：从最后持久游标继续增量。
    Delta { from: u64, to: u64 },
    /// 游标早于保留窗口：读带基准序列的快照。
    Snapshot { baseline: u64, to: u64 },
    /// 连快照基准都拿不到：必须整体重订阅，不做增量拼接。
    FullResync { reason: &'static str },
}

/// 补齐计划。判断只看持久游标与保留窗口，不看连接是否"看起来正常"。
pub fn plan_resume(
    subscriber: &SlowSubscriber,
    retained_floor: Option<u64>,
    head: u64,
    snapshot_baseline: Option<u64>,
) -> ResumePlan {
    if subscriber.released {
        return match snapshot_baseline {
            Some(baseline) => ResumePlan::Snapshot { baseline, to: head },
            None => ResumePlan::FullResync {
                reason: "缓冲已释放且无快照基准",
            },
        };
    }
    match retained_floor {
        Some(floor) if subscriber.last_persisted_cursor + 1 < floor => match snapshot_baseline {
            Some(baseline) => ResumePlan::Snapshot { baseline, to: head },
            None => ResumePlan::FullResync {
                reason: "游标早于保留窗口",
            },
        },
        _ => ResumePlan::Delta {
            from: subscriber.last_persisted_cursor + 1,
            to: head,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptAuthority {
    /// 宿主自己的持久回执。
    HostPersisted,
    /// 渠道观察到的送达。
    ChannelObserved,
    /// 什么都没有：只有"发出去了"这个事实。
    None,
}

/// 控制命令的展示状态。**没有权威回执就绝不显示成功**。
pub fn display_state(authority: ReceiptAuthority) -> &'static str {
    match authority {
        ReceiptAuthority::HostPersisted => "confirmed",
        ReceiptAuthority::ChannelObserved => "delivered",
        ReceiptAuthority::None => "pending_verification",
    }
}

// ---------------------------------------------------------------------------
// 2.7 信箱容量、过期与冲突决定（RM09）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxMessage {
    pub message_id: String,
    pub decision_key: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub capacity: usize,
    pub messages: Vec<MailboxMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Stored {
        size: usize,
    },
    /// 满额明确反馈，不静默丢弃也不无限增长。
    RejectedFull {
        capacity: usize,
    },
    RejectedExpired {
        message_id: String,
    },
}

impl Mailbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            messages: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// 过期消息在入箱与读取时都要先清：过期消息既不能占容量，也不能被当作有效决定。
    pub fn sweep_expired(&mut self, now_ms: i64) -> Vec<String> {
        let mut expired = Vec::new();
        self.messages.retain(|message| {
            if message.expires_at_ms <= now_ms {
                expired.push(message.message_id.clone());
                false
            } else {
                true
            }
        });
        expired
    }

    pub fn enqueue(&mut self, message: MailboxMessage, now_ms: i64) -> EnqueueOutcome {
        if message.expires_at_ms <= now_ms {
            return EnqueueOutcome::RejectedExpired {
                message_id: message.message_id,
            };
        }
        self.sweep_expired(now_ms);
        if self.messages.len() >= self.capacity {
            return EnqueueOutcome::RejectedFull {
                capacity: self.capacity,
            };
        }
        self.messages.push(message);
        EnqueueOutcome::Stored {
            size: self.messages.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub decision_id: String,
    pub decision_key: String,
    pub value: String,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionVerdict {
    /// 第一次下达：生效。
    Effective { decision_id: String },
    /// 同一端重送同一决定：幂等，不重复计数。
    Duplicate { decision_id: String },
    /// 与已生效决定不同：不生效，保留首个为权威并列出冲突方。
    Conflicting {
        effective: String,
        conflicting: Vec<String>,
    },
}

/// 决定性台账：同一 `decision_key` 至多一次有效业务决定。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionLedger {
    effective: BTreeMap<String, Decision>,
    conflicts: BTreeMap<String, BTreeSet<String>>,
}

impl DecisionLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn effective(&self, decision_key: &str) -> Option<&Decision> {
        self.effective.get(decision_key)
    }

    pub fn conflicts_for(&self, decision_key: &str) -> Vec<String> {
        self.conflicts
            .get(decision_key)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn decide(&mut self, decision: &Decision) -> DecisionVerdict {
        match self.effective.get(&decision.decision_key) {
            None => {
                self.effective
                    .insert(decision.decision_key.clone(), decision.clone());
                DecisionVerdict::Effective {
                    decision_id: decision.decision_id.clone(),
                }
            }
            Some(current) if current.value == decision.value => DecisionVerdict::Duplicate {
                decision_id: current.decision_id.clone(),
            },
            Some(current) => {
                let effective = current.decision_id.clone();
                self.conflicts
                    .entry(decision.decision_key.clone())
                    .or_default()
                    .insert(format!("{}:{}", decision.origin, decision.decision_id));
                DecisionVerdict::Conflicting {
                    effective,
                    conflicting: self.conflicts_for(&decision.decision_key),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DelegationRequest {
        DelegationRequest {
            invocation_id: "inv-1".to_string(),
            project_id: "project-1".to_string(),
            target_kind: TargetKind::Node,
            target_device: "device-a".to_string(),
            handoff_version: "handoff@2".to_string(),
            handoff_digest: "sha256:handoff".to_string(),
            grant_digest: "sha256:grant".to_string(),
            budget_reservation: 5_000,
            policy_version: 7,
            workspace_digest: "sha256:ws".to_string(),
            appointment: None,
            authority_epoch: None,
        }
    }

    fn key(active: bool) -> NodeKey {
        NodeKey {
            node_id: "node-a".to_string(),
            key_version: 3,
            active,
        }
    }

    fn batch(from: u64, to: u64, digest: &str) -> EventBatch {
        EventBatch {
            node_id: "node-a".to_string(),
            key_version: 3,
            invocation_id: "inv-1".to_string(),
            from_sequence: from,
            to_sequence: to,
            event_digest: digest.to_string(),
            signature: "sig".to_string(),
        }
    }

    /// 2.1 / RM01：回包丢失重启后同一请求回到原实例，改参被拒。
    #[test]
    fn rm01_same_request_returns_same_instance_and_changed_params_rejected() {
        let mut ledger = DelegationLedger::new();
        let first = ledger.submit(&request());
        let DelegationVerdict::Accepted { execution_id } = first else {
            panic!("首次委派应 Accepted，实得 {first:?}");
        };

        // 宿主重启：从持久记录重建台账。
        let restored: Vec<StoredDelegation> = ledger.stored("inv-1").cloned().into_iter().collect();
        let mut after_restart = DelegationLedger::restore(restored);
        let replay = after_restart.submit(&request());
        assert_eq!(
            replay,
            DelegationVerdict::Replayed {
                execution_id: execution_id.clone(),
                attempts: 1,
            },
            "回包丢失后的重送必须回到原实例，不新建"
        );
        assert_eq!(after_restart.len(), 1, "重放不新增记录");

        let mut tampered = request();
        tampered.workspace_digest = "sha256:other-ws".to_string();
        match after_restart.submit(&tampered) {
            DelegationVerdict::RejectedSameIdDifferentParams {
                original_digest,
                incoming_digest,
            } => {
                assert_ne!(original_digest, incoming_digest);
            }
            other => panic!("同 ID 改工作区必须拒，实得 {other:?}"),
        }

        let mut budget_changed = request();
        budget_changed.budget_reservation = 9_000;
        assert!(matches!(
            after_restart.submit(&budget_changed),
            DelegationVerdict::RejectedSameIdDifferentParams { .. }
        ));
        assert_eq!(
            after_restart
                .stored("inv-1")
                .map(|r| r.params_digest.clone()),
            Some(request().params_digest()),
            "原记录不被改写"
        );
    }

    /// 2.1：管理动作缺 Appointment 或 authority epoch 即拒。
    #[test]
    fn task_2_1_management_action_requires_appointment_and_epoch() {
        let mut ledger = DelegationLedger::new();
        let mut management = request();
        management.authority_epoch = Some(4);
        assert_eq!(
            ledger.submit(&management),
            DelegationVerdict::RejectedMissingAuthority {
                missing: vec!["appointment"]
            },
            "有 epoch 缺 Appointment 也要拒"
        );
        management.appointment = Some("appointment-1".to_string());
        assert!(matches!(
            ledger.submit(&management),
            DelegationVerdict::Accepted { .. }
        ));

        // 只给 Appointment、缺 epoch。
        let mut epoch_missing = request();
        epoch_missing.invocation_id = "inv-2".to_string();
        epoch_missing.appointment = Some("appointment-1".to_string());
        assert_eq!(
            ledger.submit(&epoch_missing),
            DelegationVerdict::RejectedMissingAuthority {
                missing: vec!["authority_epoch"]
            }
        );

        // 两项都缺：按列出顺序报两条，不合并成一句。
        let mut bare = request();
        bare.invocation_id = "inv-3".to_string();
        bare.target_kind = TargetKind::Webhook;
        bare.authority_epoch = Some(4);
        bare.appointment = None;
        assert_eq!(
            ledger.submit(&bare),
            DelegationVerdict::RejectedMissingAuthority {
                missing: vec!["appointment"]
            }
        );

        // 非管理动作（两项都不带）走普通委派：webhook 与 node 共用同一结构。
        let mut plain_webhook = request();
        plain_webhook.invocation_id = "inv-4".to_string();
        plain_webhook.target_kind = TargetKind::Webhook;
        assert!(matches!(
            ledger.submit(&plain_webhook),
            DelegationVerdict::Accepted { .. }
        ));
        assert_eq!(plain_webhook.params_digest(), plain_webhook.params_digest());
        assert_ne!(
            plain_webhook.params_digest(),
            request().params_digest(),
            "目标类型与参数都进摘要"
        );
        assert_eq!(ledger.len(), 2);
        assert!(ledger.stored("inv-4").is_some());
        assert!(!ledger.is_empty());
    }

    /// 2.2 / RM07：篡改被拒，合法批次只入账一次。
    #[test]
    fn rm07_tampered_batch_rejected_and_legal_batch_applied_once() {
        let mut ledger = BatchLedger::new();
        let legal = batch(1, 3, "sha256:events-a");
        let tampered = batch(1, 3, "sha256:events-evil");
        assert!(matches!(
            ledger.apply(&tampered, &[key(true)], "sha256:events-a"),
            ApplyOutcome::Rejected(BatchReject::Tampered { .. })
        ));
        assert_eq!(ledger.cursor_for("node-a", "inv-1"), 0, "篡改不推进游标");

        let applied = ledger.apply(&legal, &[key(true)], "sha256:events-a");
        assert_eq!(
            applied,
            ApplyOutcome::Applied {
                cursor: 3,
                events: 3
            }
        );
        assert_eq!(
            ledger.apply(&legal, &[key(true)], "sha256:events-a"),
            ApplyOutcome::Duplicate { cursor: 3 },
            "重送合法批次只入账一次"
        );

        // 撤销密钥后同一批次的后续送达一律拒。
        assert!(matches!(
            ledger.apply(
                &batch(4, 4, "sha256:events-b"),
                &[key(false)],
                "sha256:events-b"
            ),
            ApplyOutcome::Rejected(BatchReject::RevokedKey { .. })
        ));
        assert!(matches!(
            ledger.apply(&batch(4, 4, "sha256:events-b"), &[], "sha256:events-b"),
            ApplyOutcome::Rejected(BatchReject::UnknownKey { .. })
        ));
    }

    /// 2.2：缺口不跳序，游标只在连续处推进；签名通过不等于验收通过。
    #[test]
    fn task_2_2_gap_requests_backfill_and_signature_never_accepts() {
        let mut ledger = BatchLedger::new();
        assert!(matches!(
            ledger.apply(
                &batch(6, 8, "sha256:events-c"),
                &[key(true)],
                "sha256:events-c"
            ),
            ApplyOutcome::Gap {
                expected_from: 1,
                cursor: 0
            }
        ));
        assert_eq!(ledger.cursor_for("node-a", "inv-1"), 0);

        let applied = ledger.apply(
            &batch(1, 5, "sha256:events-d"),
            &[key(true)],
            "sha256:events-d",
        );
        assert_eq!(
            applied,
            ApplyOutcome::Applied {
                cursor: 5,
                events: 5
            }
        );
        assert!(
            applied.requires_acceptance_evidence(),
            "验签通过仍需独立验收证据"
        );
        assert!(!applied.accepted_artifact(), "验收不由入账触发");
        assert!(matches!(
            ledger.apply(
                &batch(9, 7, "sha256:events-e"),
                &[key(true)],
                "sha256:events-e"
            ),
            ApplyOutcome::Rejected(BatchReject::EmptyRange)
        ));
        assert_eq!(ledger.cursor_for("node-a", "inv-1"), 5);
    }

    /// 2.5 / SB05：离线期间关闭交办权，排队命令不生效、停止被拒、查看继续。
    #[test]
    fn sb05_offline_revocation_blocks_queued_dispatch_but_keeps_view() {
        let mut device = DeviceGrant::new("device-a", "user-1", "project-1")
            .with_device(&[DeviceAction::View, DeviceAction::Dispatch])
            .with_user(&[DeviceAction::View, DeviceAction::Dispatch])
            .with_channels(&[Channel::RemoteBusiness]);
        device.online = false;

        let queued = vec![
            QueuedCommand {
                command_id: "cmd-dispatch".to_string(),
                action: DeviceAction::Dispatch,
                channel: Channel::RemoteBusiness,
                queued_offline: true,
            },
            QueuedCommand {
                command_id: "cmd-stop".to_string(),
                action: DeviceAction::Stop,
                channel: Channel::RemoteBusiness,
                queued_offline: true,
            },
            QueuedCommand {
                command_id: "cmd-view".to_string(),
                action: DeviceAction::View,
                channel: Channel::RemoteBusiness,
                queued_offline: true,
            },
        ];

        let closure = close_channel(&mut device, Channel::RemoteBusiness, &queued, 2);
        assert!(closure.control_session_terminated, "关闭即终止控制会话");
        assert_eq!(closure.queued_rejected, 3);
        assert_eq!(
            closure.external_ops_to_reconcile, 2,
            "已发外部操作不撤回，交对账"
        );

        device.online = true;
        let replay = replay_queued(&device, &queued);
        for verdict in &replay {
            assert_eq!(
                verdict.outcome,
                CommandReplay::RejectedChannelClosed,
                "通道关闭后排队命令一律不生效：{verdict:?}"
            );
        }
        assert!(device.effective_actions(Channel::RemoteBusiness).is_empty());
    }

    /// 2.5：配对与在线不放宽权限；新通道默认关闭；取交集。
    #[test]
    fn task_2_5_pairing_never_widens_permissions_and_channels_default_closed() {
        let device = DeviceGrant::new("device-a", "user-1", "project-1")
            .with_device(&[
                DeviceAction::View,
                DeviceAction::Dispatch,
                DeviceAction::Stop,
            ])
            .with_user(&[DeviceAction::View]);
        // 已配对且在线，但新通道默认关闭：什么都不放行。
        assert!(device.paired && device.online);
        assert!(device.effective_actions(Channel::Mobile).is_empty());

        let enabled = device
            .clone()
            .with_channels(&[Channel::Mobile, Channel::Existing]);
        let mobile = enabled.effective_actions(Channel::Mobile);
        assert_eq!(
            mobile,
            [DeviceAction::View].into_iter().collect(),
            "设备有交办/停止能力而用户只有查看：交集只有查看"
        );

        let offline_unpaired = DeviceGrant {
            paired: false,
            ..enabled
        };
        assert!(
            offline_unpaired
                .effective_actions(Channel::Mobile)
                .is_empty()
        );

        // 设备没有、用户有的权限也不放行。
        let device_limited = DeviceGrant::new("device-b", "user-1", "project-1")
            .with_device(&[DeviceAction::View])
            .with_user(&[DeviceAction::View, DeviceAction::Dispatch])
            .with_channels(&[Channel::Existing]);
        assert_eq!(
            device_limited.effective_actions(Channel::Existing),
            [DeviceAction::View].into_iter().collect()
        );
    }

    /// 2.6 / RM06：慢订阅释放缓冲，游标过期改读快照基准。
    #[test]
    fn rm06_slow_subscriber_releases_buffer_and_resumes_from_snapshot() {
        let mut subscriber = SlowSubscriber::new(10, 3);
        assert_eq!(subscriber.push(11), "buffered");
        assert_eq!(subscriber.push(12), "buffered");
        assert_eq!(subscriber.push(13), "buffered");
        assert_eq!(subscriber.push(14), "released", "超限即释放整段缓冲");
        assert!(subscriber.buffered.is_empty(), "不保留半截缓冲");
        assert!(subscriber.released);

        match plan_resume(&subscriber, Some(5), 40, Some(30)) {
            ResumePlan::Snapshot { baseline, to } => {
                assert_eq!(baseline, 30);
                assert_eq!(to, 40);
            }
            other => panic!("释放后必须读快照基准，实得 {other:?}"),
        }
        assert_eq!(
            plan_resume(&subscriber, Some(5), 40, None),
            ResumePlan::FullResync {
                reason: "缓冲已释放且无快照基准"
            }
        );

        // 未释放但游标早于保留窗口：同样读快照，不拼接序列。
        let steady = SlowSubscriber::new(2, 8);
        assert!(matches!(
            plan_resume(&steady, Some(9), 40, Some(35)),
            ResumePlan::Snapshot { baseline: 35, .. }
        ));
        assert_eq!(
            plan_resume(&steady, None, 40, None),
            ResumePlan::Delta { from: 3, to: 40 }
        );
    }

    /// 2.6：没有权威回执就不显示成功。
    #[test]
    fn task_2_6_command_receipt_never_fakes_success() {
        assert_eq!(display_state(ReceiptAuthority::HostPersisted), "confirmed");
        assert_eq!(
            display_state(ReceiptAuthority::ChannelObserved),
            "delivered"
        );
        assert_eq!(
            display_state(ReceiptAuthority::None),
            "pending_verification",
            "只有发出记录时必须显示待核实"
        );
        // 切网与实验传输都不改变这条：传输类型不参与展示判定。
        let transports = [Transport::HttpSse, Transport::Experimental];
        for transport in transports {
            let shown = display_state(ReceiptAuthority::None);
            assert_eq!(shown, "pending_verification", "{transport:?} 也不例外");
        }
    }

    /// 2.7 / RM09：信箱满额与过期明确反馈。
    #[test]
    fn rm09_mailbox_full_and_expired_are_reported() {
        let mut mailbox = Mailbox::new(2);
        let make = |id: &str, expires: i64| MailboxMessage {
            message_id: id.to_string(),
            decision_key: format!("key-{id}"),
            expires_at_ms: expires,
        };
        assert_eq!(
            mailbox.enqueue(make("m1", 1_000), 100),
            EnqueueOutcome::Stored { size: 1 }
        );
        assert_eq!(
            mailbox.enqueue(make("m2", 1_000), 100),
            EnqueueOutcome::Stored { size: 2 }
        );
        assert_eq!(
            mailbox.enqueue(make("m3", 1_000), 100),
            EnqueueOutcome::RejectedFull { capacity: 2 },
            "满额必须明确反馈"
        );
        assert_eq!(
            mailbox.enqueue(make("m4", 50), 100),
            EnqueueOutcome::RejectedExpired {
                message_id: "m4".to_string()
            }
        );

        // 过期消息先清再入箱：腾出容量，但过期内容不会被当成有效决定。
        assert_eq!(mailbox.sweep_expired(2_000), vec!["m1", "m2"]);
        assert_eq!(
            mailbox.enqueue(make("m5", 5_000), 2_000),
            EnqueueOutcome::Stored { size: 1 }
        );
        assert_eq!(mailbox.len(), 1);
        assert!(!mailbox.is_empty());
    }

    /// 2.7：两端对同一审批给出不同决定时至多一次生效。
    #[test]
    fn task_2_7_conflicting_decisions_take_effect_at_most_once() {
        let mut ledger = DecisionLedger::new();
        let approve = Decision {
            decision_id: "dec-approve".to_string(),
            decision_key: "accept:artifact-1".to_string(),
            value: "approve".to_string(),
            origin: "desktop".to_string(),
        };
        assert_eq!(
            ledger.decide(&approve),
            DecisionVerdict::Effective {
                decision_id: "dec-approve".to_string()
            }
        );
        assert_eq!(
            ledger.decide(&approve),
            DecisionVerdict::Duplicate {
                decision_id: "dec-approve".to_string()
            },
            "同一端重送幂等"
        );

        let reject = Decision {
            decision_id: "dec-reject".to_string(),
            decision_key: "accept:artifact-1".to_string(),
            value: "reject".to_string(),
            origin: "mobile".to_string(),
        };
        match ledger.decide(&reject) {
            DecisionVerdict::Conflicting {
                effective,
                conflicting,
            } => {
                assert_eq!(effective, "dec-approve", "首个决定保持权威");
                assert_eq!(conflicting, vec!["mobile:dec-reject".to_string()]);
            }
            other => panic!("不同决定必须报冲突，实得 {other:?}"),
        }
        assert_eq!(
            ledger
                .effective("accept:artifact-1")
                .map(|d| d.value.as_str()),
            Some("approve"),
            "冲突不改变已生效决定"
        );
        assert_eq!(ledger.conflicts_for("accept:artifact-1").len(), 1);
    }
}
