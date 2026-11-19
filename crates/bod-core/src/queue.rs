//! P0 D2 纯队列规则：唤醒去重、代数合并、原子认领、fencing、退避与 effect 恢复分类。
//! 纯值类型 + 确定性决策，不依赖 DB/Web/Tauri（G-1）。
//!
//! 时间（`now_ms`）、抖动（`jitter_ms`）与重试策略由宿主注入，本模块不读系统时钟、不生成随机数，
//! 使 §9.10—9.12 的唤醒/租约/崩溃窗口可在测试中确定性复现。

use crate::DomainId;
use crate::model::DenialReason;

/// 资源队列状态（§9.10）。代数只表示“已协调该轮输入”，不是业务完成语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueState {
    Idle,
    Pending,
    Running,
    Blocked,
    Dead,
    Cancelled,
}

impl QueueState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Dead => "dead",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "blocked" => Some(Self::Blocked),
            "dead" => Some(Self::Dead),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// 唤醒回执：逻辑键 = (company_id, producer_id, idempotency_key)（§9.10）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeReceipt {
    pub company_id: DomainId,
    pub producer_id: DomainId,
    pub idempotency_key: String,
    /// 请求摘要：同键不同摘要 MUST 拒绝（i_69）。
    pub request_digest: String,
    pub queue_key: String,
    pub generation: u64,
    pub accepted_at_ms: i64,
    /// 保留窗口截止；窗口内即使目标已终态也必须继续去重。
    pub retain_until_ms: i64,
    /// 定时触发绑定持久 schedule_id + occurrence_id，重启不得伪装成新重试。
    pub schedule_id: Option<String>,
    pub occurrence_id: Option<String>,
}

/// 一次唤醒请求（输入事实已持久，本请求只携带引用与摘要）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeRequest {
    pub company_id: DomainId,
    pub producer_id: DomainId,
    pub idempotency_key: String,
    pub request_digest: String,
    pub queue_key: String,
    pub schedule_id: Option<String>,
    pub occurrence_id: Option<String>,
}

/// 资源队列行：单行长期复用，唯一键为 (company_id, queue_key)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkQueueEntry {
    pub company_id: DomainId,
    pub queue_key: String,
    pub state: QueueState,
    pub wanted_generation: u64,
    pub claimed_generation: u64,
    pub completed_generation: u64,
    pub next_due_at_ms: i64,
    pub attempts: u32,
    pub lease_owner: Option<String>,
    /// 持久化、单调递增的 fencing token。
    pub lease_token: u64,
    pub lease_expires_at_ms: Option<i64>,
    pub last_error: Option<String>,
    /// 稳定阻塞原因；Some 表示等待显式解除条件，禁止热重试。
    pub blocked_reason: Option<DenialReason>,
}

impl WorkQueueEntry {
    pub fn new(company_id: DomainId, queue_key: String, now_ms: i64) -> Self {
        Self {
            company_id,
            queue_key,
            state: QueueState::Pending,
            wanted_generation: 1,
            claimed_generation: 0,
            completed_generation: 0,
            next_due_at_ms: now_ms,
            attempts: 0,
            lease_owner: None,
            lease_token: 0,
            lease_expires_at_ms: None,
            last_error: None,
            blocked_reason: None,
        }
    }

    /// generation 滞后：仍有未处理的目标代数。
    pub fn generation_lag(&self) -> u64 {
        self.wanted_generation
            .saturating_sub(self.completed_generation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// 同逻辑键但参数摘要不同（i_69）。
    KeyConflict { key: String },
    /// 资源未就绪或租约被他人持有。
    NotReady { state: QueueState },
    /// fencing：owner/token 不匹配，旧 token 不得更新宿主状态（i_02）。
    LeaseMismatch,
}

/// 唤醒决策结果。`Existing` 不创建第二个 Attempt 或派发意图（i_68）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    Existing {
        receipt: WakeReceipt,
    },
    Accepted {
        receipt: WakeReceipt,
        generation: u64,
    },
}

/// 受理唤醒的纯决策。
///
/// 顺序：先查回执 → 同键同摘要返回原回执（不递增代数）；同键异参拒绝；不存在则按
/// 当前队列的 wanted_generation 递增一代并生成回执（§33.15，禁止“先加代数再忽略冲突”）。
/// 定时触发把 schedule_id/occurrence_id 纳入摘要比对，重启不会生成伪装重试的新请求。
pub fn evaluate_wake(
    existing: Option<&WakeReceipt>,
    queue: Option<&WorkQueueEntry>,
    req: &WakeRequest,
    now_ms: i64,
    retain_window_ms: i64,
) -> Result<WakeOutcome, QueueError> {
    if let Some(receipt) = existing {
        if receipt.request_digest == req.request_digest
            && receipt.schedule_id == req.schedule_id
            && receipt.occurrence_id == req.occurrence_id
        {
            // 保留窗口内（含终态后）必须继续去重；窗口外也不因时间流逝自动重放副作用，
            // 物理回收与“窗口外新请求需新键”由 D6/调用方处理。
            let _ = (now_ms, retain_window_ms);
            return Ok(WakeOutcome::Existing {
                receipt: receipt.clone(),
            });
        }
        return Err(QueueError::KeyConflict {
            key: req.idempotency_key.clone(),
        });
    }
    let generation = queue.map(|entry| entry.wanted_generation).unwrap_or(0) + 1;
    let receipt = WakeReceipt {
        company_id: req.company_id.clone(),
        producer_id: req.producer_id.clone(),
        idempotency_key: req.idempotency_key.clone(),
        request_digest: req.request_digest.clone(),
        queue_key: req.queue_key.clone(),
        generation,
        accepted_at_ms: now_ms,
        retain_until_ms: now_ms + retain_window_ms.max(0),
        schedule_id: req.schedule_id.clone(),
        occurrence_id: req.occurrence_id.clone(),
    };
    Ok(WakeOutcome::Accepted {
        receipt,
        generation,
    })
}

/// 受理成功后如何推进队列行。新键创建行；已存在行 wanted_generation+1，
/// 运行中保持 running（不创建第二个消费者，i_70）；blocked/dead 被新事件重新排队。
pub fn apply_accepted_wake(
    existing: Option<WorkQueueEntry>,
    req: &WakeRequest,
    generation: u64,
    now_ms: i64,
) -> WorkQueueEntry {
    match existing {
        None => WorkQueueEntry::new(req.company_id.clone(), req.queue_key.clone(), now_ms),
        Some(mut entry) => {
            entry.wanted_generation = generation;
            entry.blocked_reason = None;
            entry.last_error = None;
            match entry.state {
                QueueState::Idle | QueueState::Blocked | QueueState::Dead => {
                    entry.state = QueueState::Pending;
                    entry.next_due_at_ms = now_ms;
                }
                QueueState::Running => {
                    // 保持 running：本轮完成后按 wanted_generation 发现新增代数。
                }
                QueueState::Pending | QueueState::Cancelled => {}
            }
            entry
        }
    }
}

/// 认领结果：leased 的时间与 token 由本模块给出，宿主写库时用同一 token 做 CAS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimGrant {
    pub claimed_generation: u64,
    pub lease_owner: String,
    pub lease_token: u64,
    pub lease_expires_at_ms: i64,
}

/// 原子认领的纯决策（i_01）。真实并发安全由写库条件更新（WHERE state/token/due）保证，
/// 本函数给出“若可见状态如此则应授予”的令牌。
pub fn evaluate_claim(
    entry: &WorkQueueEntry,
    owner: &str,
    now_ms: i64,
    lease_ms: i64,
) -> Result<ClaimGrant, QueueError> {
    if entry.state != QueueState::Pending {
        return Err(QueueError::NotReady { state: entry.state });
    }
    if now_ms < entry.next_due_at_ms {
        return Err(QueueError::NotReady { state: entry.state });
    }
    if entry.lease_expires_at_ms.is_some_and(|expires| {
        expires > now_ms
            && entry
                .lease_owner
                .as_deref()
                .is_some_and(|held| held != owner)
    }) {
        return Err(QueueError::NotReady { state: entry.state });
    }
    Ok(ClaimGrant {
        claimed_generation: entry.wanted_generation,
        lease_owner: owner.to_string(),
        lease_token: entry.lease_token + 1,
        lease_expires_at_ms: now_ms + lease_ms.max(0),
    })
}

/// 完成确认的纯决策（i_02/i_03）。token 必须匹配，旧 token 一律拒绝；
/// wanted > claimed 时转 pending（发现新代数），否则 idle。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionDecision {
    pub completed_generation: u64,
    pub next_state: QueueState,
    pub newer_work_pending: bool,
}

pub fn evaluate_completion(
    entry: &WorkQueueEntry,
    owner: &str,
    token: u64,
) -> Result<CompletionDecision, QueueError> {
    if entry.lease_owner.as_deref() != Some(owner) || entry.lease_token != token {
        return Err(QueueError::LeaseMismatch);
    }
    let newer = entry.wanted_generation > entry.claimed_generation;
    Ok(CompletionDecision {
        completed_generation: entry.claimed_generation,
        next_state: if newer {
            QueueState::Pending
        } else {
            QueueState::Idle
        },
        newer_work_pending: newer,
    })
}

/// 租约过期后恢复处理权：即使前轮已发起外部动作，也只回到 pending 等待 inspect/reconcile，
/// 不直接再执行（§9.11）。本函数只改队列状态，不涉及 invocation。
pub fn recover_expired_lease(entry: &WorkQueueEntry, now_ms: i64) -> Option<WorkQueueEntry> {
    let expired = entry
        .lease_expires_at_ms
        .is_some_and(|expires| now_ms >= expires);
    if !expired || entry.state != QueueState::Running {
        return None;
    }
    let mut next = entry.clone();
    next.state = QueueState::Pending;
    next.next_due_at_ms = now_ms;
    Some(next)
}

/// 退避策略：由宿主从配置构造（§37.14），core 不读配置、不读时钟。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: i64,
    pub max_delay_ms: i64,
    pub total_deadline_ms: i64,
    pub jitter_ms: i64,
}

/// 错误类别：权限/预算/输入是稳定阻塞，外部结果未知冻结盲重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Transient,
    Permission,
    Budget,
    Input,
    ExternalUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// 有界退避 + 抖动（delay_ms > 0），不形成热重试。
    Backoff { delay_ms: i64 },
    /// 稳定 Blocked：等待显式解除条件，不周期性重启。
    StableBlock { reason: DenialReason },
    /// 超过重试预算：转 dead 并保留原请求与原因。
    Dead { reason: DenialReason },
}

/// 退避决策（§9.12）。指数退避封顶，加注入抖动后 clamp 到 [1, max_delay]；
/// 权限/预算/输入直接稳定阻塞；ExternalUnknown 冻结盲重试进入稳定阻塞等待对账。
pub fn evaluate_retry(class: ErrorClass, attempts: u32, policy: RetryPolicy) -> RetryDecision {
    let stable_reason = match class {
        ErrorClass::Permission => Some(DenialReason::InsufficientPermission),
        ErrorClass::Budget => Some(DenialReason::InsufficientBudget),
        ErrorClass::Input => Some(DenialReason::MissingProjectContext),
        ErrorClass::ExternalUnknown => Some(DenialReason::ExternalOutcomeUnknown),
        ErrorClass::Transient => None,
    };
    if let Some(reason) = stable_reason {
        return RetryDecision::StableBlock { reason };
    }
    if attempts >= policy.max_attempts {
        return RetryDecision::Dead {
            reason: DenialReason::InsufficientBudget,
        };
    }
    let shift = attempts.min(20);
    let exponential = policy.base_delay_ms.max(1).saturating_mul(1i64 << shift);
    let capped = exponential.min(policy.max_delay_ms.max(1));
    let with_jitter = capped.saturating_add(policy.jitter_ms.abs().min(capped / 2));
    let bounded = with_jitter.clamp(1, policy.max_delay_ms.max(1));
    let bounded = bounded.min(policy.total_deadline_ms.max(1));
    RetryDecision::Backoff { delay_ms: bounded }
}

/// 外部动作状态与恢复分类（§10.4/§10.5、effect-gate 提交后派发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectStatus {
    Prepared,
    Dispatched,
    Confirmed,
    Failed,
    Unknown,
}

impl EffectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Dispatched => "dispatched",
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "prepared" => Some(Self::Prepared),
            "dispatched" => Some(Self::Dispatched),
            "confirmed" => Some(Self::Confirmed),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// 持久 outbox 意图。唯一键为 (event_or_command_id, destination_id)（§33.13）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxDelivery {
    pub event_or_command_id: String,
    pub destination_id: String,
    pub company_id: DomainId,
    pub queue_key: String,
    pub request_digest: String,
    pub status: EffectStatus,
    pub attempts: u32,
    pub next_due_at_ms: i64,
    pub result_ref: Option<String>,
    pub created_at_ms: i64,
}

/// 外部观测输入（由宿主 inspect 得到，core 不发起调用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalObservation {
    Confirmed { result_ref: Option<String> },
    NotFound,
    StillRunning,
    Inconclusive,
}

/// 恢复动作。所有“先 inspect/reconcile 再派发”的判断都在这里做纯决策，
/// 派发/查询由宿主在事务外执行（§33.13、effect-gate 提交后派发）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// 已完成/失败：跳过，不重放。
    Skip,
    /// Prepared 且到期：可在提交后派发。
    Dispatch,
    /// Dispatched/Unknown 且未获可核验证据：先查询再决定，禁止盲重发。
    InspectFirst,
    /// 查询确认已发生：仅更新本地 Confirmed，不重发副作用。
    MarkConfirmed { result_ref: Option<String> },
    /// 查询确认未发生且有幂等键：允许重派发一次。
    RedispatchGuarded,
}

pub fn classify_recovery(
    delivery: &OutboxDelivery,
    observed: Option<ExternalObservation>,
    now_ms: i64,
) -> RecoveryAction {
    match delivery.status {
        EffectStatus::Confirmed | EffectStatus::Failed => RecoveryAction::Skip,
        EffectStatus::Prepared => {
            if now_ms >= delivery.next_due_at_ms {
                RecoveryAction::Dispatch
            } else {
                RecoveryAction::Skip
            }
        }
        EffectStatus::Dispatched | EffectStatus::Unknown => match observed {
            Some(ExternalObservation::Confirmed { result_ref }) => {
                RecoveryAction::MarkConfirmed { result_ref }
            }
            Some(ExternalObservation::NotFound) => RecoveryAction::RedispatchGuarded,
            Some(ExternalObservation::StillRunning) | None => RecoveryAction::InspectFirst,
            Some(ExternalObservation::Inconclusive) => RecoveryAction::InspectFirst,
        },
    }
}

/// 队列可观测指标（§9.12、coordination-loop 队列可观测；供 doctor 与人工接管）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueMetrics {
    /// 队列深度：未终结的资源行数（不含 idle/cancelled）。
    pub depth: u64,
    /// 最老到期时间；无待处理项时为 None。
    pub oldest_due_at_ms: Option<i64>,
    /// 最老未处理资源标识。
    pub oldest_queue_key: Option<String>,
    /// 代数滞后总和（wanted - completed）。
    pub generation_lag_total: u64,
    /// 租约过期数。
    pub expired_leases: u64,
    pub dead_count: u64,
    pub blocked_count: u64,
    /// 累计重试/认领尝试次数。
    pub retries_total: u64,
    /// 最老待派发 outbox 的投递延迟（now - created_at）。
    pub oldest_delivery_latency_ms: Option<i64>,
}

/// 从队列行与 outbox 行聚合指标；时间注入，纯函数可测。
pub fn summarize_metrics(
    entries: &[WorkQueueEntry],
    deliveries: &[OutboxDelivery],
    now_ms: i64,
) -> QueueMetrics {
    let mut metrics = QueueMetrics::default();
    for entry in entries {
        if matches!(entry.state, QueueState::Idle | QueueState::Cancelled) {
            continue;
        }
        metrics.depth += 1;
        metrics.generation_lag_total += entry.generation_lag();
        metrics.retries_total += u64::from(entry.attempts);
        if matches!(entry.state, QueueState::Dead) {
            metrics.dead_count += 1;
        }
        if matches!(entry.state, QueueState::Blocked) {
            metrics.blocked_count += 1;
        }
        if entry
            .lease_expires_at_ms
            .is_some_and(|expires| now_ms >= expires)
        {
            metrics.expired_leases += 1;
        }
        let older = metrics
            .oldest_due_at_ms
            .is_none_or(|best| entry.next_due_at_ms < best);
        if older {
            metrics.oldest_due_at_ms = Some(entry.next_due_at_ms);
            metrics.oldest_queue_key = Some(entry.queue_key.clone());
        }
    }
    for delivery in deliveries {
        if matches!(
            delivery.status,
            EffectStatus::Prepared | EffectStatus::Dispatched
        ) {
            let latency = now_ms - delivery.created_at_ms;
            if metrics
                .oldest_delivery_latency_ms
                .is_none_or(|best| latency > best)
            {
                metrics.oldest_delivery_latency_ms = Some(latency.max(0));
            }
        }
    }
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(digest: &str) -> WakeReceipt {
        WakeReceipt {
            company_id: DomainId::new("company-default"),
            producer_id: DomainId::new("producer-1"),
            idempotency_key: "key-1".to_string(),
            request_digest: digest.to_string(),
            queue_key: "issue-1".to_string(),
            generation: 1,
            accepted_at_ms: 100,
            retain_until_ms: 100 + 168 * 3_600_000,
            schedule_id: None,
            occurrence_id: None,
        }
    }

    fn request(digest: &str) -> WakeRequest {
        WakeRequest {
            company_id: DomainId::new("company-default"),
            producer_id: DomainId::new("producer-1"),
            idempotency_key: "key-1".to_string(),
            request_digest: digest.to_string(),
            queue_key: "issue-1".to_string(),
            schedule_id: None,
            occurrence_id: None,
        }
    }

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
            total_deadline_ms: 600_000,
            jitter_ms: 37,
        }
    }

    mod wake_dedup {
        use super::*;

        #[test]
        fn i_68_same_key_same_digest_returns_original_receipt() {
            let first = evaluate_wake(None, None, &request("d1"), 100, 168 * 3_600_000)
                .expect("首次唤醒应受理");
            let generation = match &first {
                WakeOutcome::Accepted { generation, .. } => *generation,
                _ => panic!("首次应受理"),
            };
            let again = evaluate_wake(
                Some(&receipt("d1")),
                None,
                &request("d1"),
                200,
                168 * 3_600_000,
            )
            .expect("重复唤醒应返回原回执");
            assert_eq!(
                again,
                WakeOutcome::Existing {
                    receipt: receipt("d1")
                }
            );
            assert_eq!(generation, 1);
        }

        #[test]
        fn i_69_same_key_different_digest_is_rejected() {
            let err = evaluate_wake(
                Some(&receipt("d1")),
                None,
                &request("d2"),
                200,
                168 * 3_600_000,
            )
            .expect_err("同键异参必须拒绝");
            assert_eq!(
                err,
                QueueError::KeyConflict {
                    key: "key-1".to_string()
                }
            );
        }

        #[test]
        fn scheduled_occurrence_change_is_not_masked_as_retry() {
            let mut req = request("d1");
            req.schedule_id = Some("sched-1".to_string());
            req.occurrence_id = Some("occ-1".to_string());
            let mut saved = receipt("d1");
            saved.schedule_id = req.schedule_id.clone();
            saved.occurrence_id = req.occurrence_id.clone();
            req.occurrence_id = Some("occ-2".to_string());
            assert_eq!(
                evaluate_wake(Some(&saved), None, &req, 200, 0).unwrap_err(),
                QueueError::KeyConflict {
                    key: "key-1".to_string()
                }
            );
        }

        #[test]
        fn i_70_new_wake_while_running_bumps_generation_keeps_state() {
            let mut running =
                WorkQueueEntry::new(DomainId::new("company-default"), "issue-1".to_string(), 100);
            running.state = QueueState::Running;
            running.wanted_generation = 1;
            let updated = apply_accepted_wake(Some(running.clone()), &request("d2"), 2, 200);
            assert_eq!(updated.wanted_generation, 2);
            assert_eq!(updated.state, QueueState::Running, "运行中不得建第二消费者");
            assert_eq!(updated.generation_lag(), 2);
        }

        #[test]
        fn blocked_is_requeued_by_new_event() {
            let mut blocked =
                WorkQueueEntry::new(DomainId::new("company-default"), "issue-1".to_string(), 100);
            blocked.state = QueueState::Blocked;
            blocked.blocked_reason = Some(DenialReason::InsufficientBudget);
            let updated = apply_accepted_wake(Some(blocked), &request("d2"), 2, 200);
            assert_eq!(updated.state, QueueState::Pending);
            assert_eq!(updated.blocked_reason, None);
        }
    }

    mod claim_fencing {
        use super::*;

        fn pending() -> WorkQueueEntry {
            WorkQueueEntry::new(DomainId::new("company-default"), "issue-1".to_string(), 100)
        }

        #[test]
        fn i_01_only_ready_pending_can_be_claimed() {
            let grant =
                evaluate_claim(&pending(), "consumer-1", 100, 30_000).expect("到期 pending 可认领");
            assert_eq!(grant.claimed_generation, 1);
            assert_eq!(grant.lease_token, 1);

            let mut early = pending();
            early.next_due_at_ms = 500;
            assert_eq!(
                evaluate_claim(&early, "consumer-1", 100, 30_000).unwrap_err(),
                QueueError::NotReady {
                    state: QueueState::Pending
                }
            );
        }

        #[test]
        fn i_02_stale_fencing_token_is_rejected() {
            let mut entry = pending();
            entry.state = QueueState::Running;
            entry.lease_owner = Some("consumer-1".to_string());
            entry.lease_token = 4;
            assert_eq!(
                evaluate_completion(&entry, "consumer-1", 3).unwrap_err(),
                QueueError::LeaseMismatch
            );
            assert!(evaluate_completion(&entry, "consumer-1", 4).is_ok());
        }

        #[test]
        fn i_03_completion_with_newer_generation_goes_pending() {
            let mut entry = pending();
            entry.state = QueueState::Running;
            entry.lease_owner = Some("consumer-1".to_string());
            entry.lease_token = 2;
            entry.claimed_generation = 1;
            entry.wanted_generation = 2;
            let decision = evaluate_completion(&entry, "consumer-1", 2).unwrap();
            assert_eq!(decision.next_state, QueueState::Pending);
            assert!(decision.newer_work_pending);
        }

        #[test]
        fn expired_lease_recovers_without_reexecuting() {
            let mut entry = pending();
            entry.state = QueueState::Running;
            entry.lease_expires_at_ms = Some(200);
            let recovered = recover_expired_lease(&entry, 250).expect("过期租约应恢复处理权");
            assert_eq!(recovered.state, QueueState::Pending);
            assert!(recover_expired_lease(&entry, 150).is_none());
        }
    }

    mod retry_and_recovery {
        use super::*;

        #[test]
        fn permission_budget_and_unknown_are_stable_blocks() {
            for (class, reason) in [
                (ErrorClass::Permission, DenialReason::InsufficientPermission),
                (ErrorClass::Budget, DenialReason::InsufficientBudget),
                (ErrorClass::Input, DenialReason::MissingProjectContext),
                (
                    ErrorClass::ExternalUnknown,
                    DenialReason::ExternalOutcomeUnknown,
                ),
            ] {
                assert_eq!(
                    evaluate_retry(class, 0, policy()),
                    RetryDecision::StableBlock { reason }
                );
            }
        }

        #[test]
        fn transient_backoff_is_bounded_and_never_hot() {
            let first = evaluate_retry(ErrorClass::Transient, 0, policy());
            let last = evaluate_retry(ErrorClass::Transient, 4, policy());
            match (first, last) {
                (
                    RetryDecision::Backoff { delay_ms: a },
                    RetryDecision::Backoff { delay_ms: b },
                ) => {
                    assert!(a >= 1 && b >= 1, "退避必须为正，禁止热重试");
                    assert!(b <= policy().max_delay_ms);
                }
                _ => panic!("瞬时错误应有界退避"),
            }
            assert_eq!(
                evaluate_retry(ErrorClass::Transient, 5, policy()),
                RetryDecision::Dead {
                    reason: DenialReason::InsufficientBudget
                }
            );
        }

        fn delivery(status: EffectStatus) -> OutboxDelivery {
            OutboxDelivery {
                event_or_command_id: "act-1".to_string(),
                destination_id: "executor-1".to_string(),
                company_id: DomainId::new("company-default"),
                queue_key: "issue-1".to_string(),
                request_digest: "d1".to_string(),
                status,
                attempts: 0,
                next_due_at_ms: 100,
                result_ref: None,
                created_at_ms: 90,
            }
        }

        #[test]
        fn dispatched_without_evidence_inspects_first() {
            assert_eq!(
                classify_recovery(&delivery(EffectStatus::Dispatched), None, 200),
                RecoveryAction::InspectFirst
            );
            assert_eq!(
                classify_recovery(&delivery(EffectStatus::Unknown), None, 200),
                RecoveryAction::InspectFirst
            );
        }

        #[test]
        fn confirmed_externally_only_updates_local_state() {
            assert_eq!(
                classify_recovery(
                    &delivery(EffectStatus::Dispatched),
                    Some(ExternalObservation::Confirmed {
                        result_ref: Some("inv-1".to_string())
                    }),
                    200,
                ),
                RecoveryAction::MarkConfirmed {
                    result_ref: Some("inv-1".to_string())
                }
            );
        }

        #[test]
        fn prepared_dispatches_after_commit_and_terminal_is_skipped() {
            assert_eq!(
                classify_recovery(&delivery(EffectStatus::Prepared), None, 200),
                RecoveryAction::Dispatch
            );
            assert_eq!(
                classify_recovery(&delivery(EffectStatus::Confirmed), None, 200),
                RecoveryAction::Skip
            );
        }
    }

    mod observability {
        use super::*;

        #[test]
        fn metrics_report_depth_lag_dead_blocked_and_oldest() {
            let mut ready =
                WorkQueueEntry::new(DomainId::new("company-default"), "ready".to_string(), 100);
            ready.next_due_at_ms = 150;
            let mut dead =
                WorkQueueEntry::new(DomainId::new("company-default"), "dead".to_string(), 100);
            dead.state = QueueState::Dead;
            dead.wanted_generation = 3;
            dead.completed_generation = 1;
            dead.lease_expires_at_ms = Some(120);
            let mut blocked =
                WorkQueueEntry::new(DomainId::new("company-default"), "blocked".to_string(), 100);
            blocked.state = QueueState::Blocked;
            let idle = WorkQueueEntry {
                queue_key: "idle".to_string(),
                state: QueueState::Idle,
                ..WorkQueueEntry::new(DomainId::new("company-default"), "idle".to_string(), 100)
            };
            let delivery = OutboxDelivery {
                event_or_command_id: "act".to_string(),
                destination_id: "dest".to_string(),
                company_id: DomainId::new("company-default"),
                queue_key: "ready".to_string(),
                request_digest: "d".to_string(),
                status: EffectStatus::Prepared,
                attempts: 0,
                next_due_at_ms: 0,
                result_ref: None,
                created_at_ms: 50,
            };
            let metrics = summarize_metrics(&[ready, dead, blocked, idle], &[delivery], 200);
            assert_eq!(metrics.depth, 3, "idle 不计入深度");
            assert_eq!(metrics.dead_count, 1);
            assert_eq!(metrics.blocked_count, 1);
            assert_eq!(metrics.expired_leases, 1);
            assert_eq!(
                metrics.generation_lag_total, 4,
                "ready/dead/blocked 各计滞后，idle 不计"
            );
            assert_eq!(metrics.oldest_due_at_ms, Some(100));
            assert_eq!(metrics.oldest_queue_key.as_deref(), Some("dead"));
            assert_eq!(metrics.oldest_delivery_latency_ms, Some(150));
        }
    }
}
