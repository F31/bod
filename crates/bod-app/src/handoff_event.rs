//! P2 任务 1.5b：交接事件与幂等 accept。
//!
//! 权威语义：V2.9.6 §18.13——记录 `handoff.requested / captured / validated / accepted /
//! blocked / resumed` 及原因与来源；执行 Agent 只能提交**当前工作**的交接候选，不得获得
//! 全局交接管理凭据；同一个 accept 请求重复提交返回同一结果，不创建第二份执行或第二份包。
//!
//! 本模块的边界：
//! - 事件日志只追加。状态由事件折叠得出，没有可以被单独改写的 `state` 列；
//! - 能力检查在**事件入口**（[`ActorScope::permits`]）与**聚合身份**（执行 Agent 的
//!   attempt 必须等于本次交接的来源 attempt）两处执行，不靠调用方自觉；
//! - accept 的幂等由 `(company_id, handoff_id)` 唯一约束保证：一个交接只能被接受一次，
//!   重复请求读回第一次的结果与序号，不重新分配序号、不追加第二条 `accepted` 事件；
//! - 这里不创建 Attempt、不写队列、不分配租约：接受是一个状态转换，不是一个新执行。

use crate::canonical_json::{self, CanonicalError, CanonicalValue};
use crate::domain::DomainId;
use crate::domain::artifact::Digest;
use crate::handoff_package::HandoffLevel;
use crate::store::{SqliteStore, StoreError};
use std::fmt;

/// 交接事件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffEventKind {
    Requested,
    Captured,
    Validated,
    Accepted,
    Blocked,
    Resumed,
}

impl HandoffEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Captured => "captured",
            Self::Validated => "validated",
            Self::Accepted => "accepted",
            Self::Blocked => "blocked",
            Self::Resumed => "resumed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "requested" => Some(Self::Requested),
            "captured" => Some(Self::Captured),
            "validated" => Some(Self::Validated),
            "accepted" => Some(Self::Accepted),
            "blocked" => Some(Self::Blocked),
            "resumed" => Some(Self::Resumed),
            _ => None,
        }
    }

    /// 这些事件必须指向具体材料（快照键），否则无法核对「验证的是哪一份现场」。
    pub const fn requires_evidence(self) -> bool {
        matches!(self, Self::Captured | Self::Validated | Self::Accepted)
    }
}

impl fmt::Display for HandoffEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 事件来源（同时决定能力）：执行 Agent、宿主、人、策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorScope {
    /// 执行 Agent：只能提交**自己这次**工作的交接候选。
    ExecutingAgent { attempt_id: String },
    /// 宿主控制面：捕获、验证、恢复。
    Host,
    /// 人：接受、阻塞、恢复。
    Human { subject_id: String },
    /// 策略：只能产生阻塞。
    Policy { rule: String },
}

impl ActorScope {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ExecutingAgent { .. } => "agent",
            Self::Host => "host",
            Self::Human { .. } => "human",
            Self::Policy { .. } => "policy",
        }
    }

    pub fn reference(&self) -> Option<&str> {
        match self {
            Self::ExecutingAgent { attempt_id } => Some(attempt_id),
            Self::Host => None,
            Self::Human { subject_id } => Some(subject_id),
            Self::Policy { rule } => Some(rule),
        }
    }

    /// 能力表：执行 Agent 没有 accept 权限，策略没有接受/验证权限。
    pub const fn permits(&self, kind: HandoffEventKind) -> bool {
        match self {
            Self::ExecutingAgent { .. } => {
                matches!(
                    kind,
                    HandoffEventKind::Requested | HandoffEventKind::Captured
                )
            }
            Self::Host => matches!(
                kind,
                HandoffEventKind::Captured
                    | HandoffEventKind::Validated
                    | HandoffEventKind::Resumed
            ),
            Self::Human { .. } => true,
            Self::Policy { .. } => matches!(kind, HandoffEventKind::Blocked),
        }
    }

    /// 执行 Agent 只能操作来源 attempt 相同的交接。
    fn is_in_scope_for(&self, record: &HandoffRecord) -> bool {
        match self {
            Self::ExecutingAgent { attempt_id } => attempt_id == record.source_attempt_id.as_str(),
            _ => true,
        }
    }
}

/// 交接聚合身份。事件与接受都挂在它下面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffRecord {
    pub handoff_id: String,
    pub company_id: DomainId,
    pub project_id: DomainId,
    pub issue_id: DomainId,
    pub work_package_revision: DomainId,
    pub source_attempt_id: DomainId,
}

impl HandoffRecord {
    pub fn validate(&self) -> Result<(), EventError> {
        for (field, value) in [
            ("handoff_id", self.handoff_id.as_str()),
            ("company_id", self.company_id.as_str()),
            ("project_id", self.project_id.as_str()),
            ("issue_id", self.issue_id.as_str()),
            ("work_package_revision", self.work_package_revision.as_str()),
            ("source_attempt_id", self.source_attempt_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(EventError::MissingIdentity(field));
            }
        }
        Ok(())
    }
}

/// 交接状态。`Blocked` 之前的状态由事件恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffState {
    Requested,
    Captured,
    Validated,
    Accepted,
    Blocked,
}

impl HandoffState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Captured => "captured",
            Self::Validated => "validated",
            Self::Accepted => "accepted",
            Self::Blocked => "blocked",
        }
    }
}

impl fmt::Display for HandoffState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条交接事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffEvent {
    pub handoff_id: String,
    /// 从 1 开始、连续、不可重排。
    pub seq: u64,
    pub event_id: String,
    pub kind: HandoffEventKind,
    pub reason: Option<String>,
    pub actor: ActorScope,
    /// 秒级 UTC。
    pub occurred_at: String,
    /// `captured`/`validated`/`accepted` 必填：指向被捕获/验证的快照键。
    pub evidence_ref: Option<String>,
}

/// 追加事件的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSpec {
    pub kind: HandoffEventKind,
    pub actor: ActorScope,
    pub reason: Option<String>,
    pub evidence_ref: Option<String>,
    pub occurred_at: String,
}

impl EventSpec {
    pub fn new(kind: HandoffEventKind, actor: ActorScope, occurred_at: impl Into<String>) -> Self {
        Self {
            kind,
            actor,
            reason: None,
            evidence_ref: None,
            occurred_at: occurred_at.into(),
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn with_evidence(mut self, snapshot_key: impl Into<String>) -> Self {
        self.evidence_ref = Some(snapshot_key.into());
        self
    }
}

/// accept 请求。`idempotency_key` 由调用方给，重复提交必须原样重复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptRequest {
    pub idempotency_key: String,
    pub snapshot_key: String,
    pub level: HandoffLevel,
    pub actor: ActorScope,
    pub accepted_at: String,
}

/// accept 结果。`Replayed` 与 `Accepted` 携带同一组值，区分「这次真的转了」与「读回第一次」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptOutcome {
    pub acceptance_key: String,
    pub accepted_event_id: String,
    pub accepted_seq: u64,
    pub snapshot_key: String,
    replayed: bool,
}

impl AcceptOutcome {
    pub fn replayed(&self) -> bool {
        self.replayed
    }
}

/// 交接账本：身份 + 已发生事件（按 seq 有序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffLedger {
    record: HandoffRecord,
    events: Vec<HandoffEvent>,
    state: Option<HandoffState>,
    blocked_from: Option<HandoffState>,
}

impl HandoffLedger {
    pub fn new(record: HandoffRecord) -> Result<Self, EventError> {
        record.validate()?;
        Ok(Self {
            record,
            events: Vec::new(),
            state: None,
            blocked_from: None,
        })
    }

    /// 从存储读回并重放校验：事件被手工改序/删行会在这一步暴露。
    pub fn load(store: &SqliteStore, record: HandoffRecord) -> Result<Self, EventError> {
        record.validate()?;
        let rows = store.list_handoff_events(record.company_id.as_str(), &record.handoff_id)?;
        let events = rows
            .iter()
            .map(HandoffEventRow::to_event)
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_events(record, events)
    }

    fn from_events(record: HandoffRecord, events: Vec<HandoffEvent>) -> Result<Self, EventError> {
        let mut ledger = Self {
            record,
            events: Vec::new(),
            state: None,
            blocked_from: None,
        };
        for event in events {
            ledger.accept_event(event)?;
        }
        Ok(ledger)
    }

    pub fn record(&self) -> &HandoffRecord {
        &self.record
    }

    pub fn events(&self) -> &[HandoffEvent] {
        &self.events
    }

    /// 尚未收到任何事件时为 `None`（不是「等于 requested」）。
    pub fn state(&self) -> Option<HandoffState> {
        self.state
    }

    /// 追加事件：先校验能力与状态转换，再写库，最后进入本地日志。
    pub fn append(
        &mut self,
        store: &SqliteStore,
        spec: EventSpec,
    ) -> Result<HandoffEvent, EventError> {
        self.check_spec(&spec)?;
        let seq = self.events.len() as u64 + 1;
        let event = HandoffEvent {
            handoff_id: self.record.handoff_id.clone(),
            seq,
            event_id: format!("{}#{seq}", self.record.handoff_id),
            kind: spec.kind,
            reason: spec
                .reason
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            actor: spec.actor,
            occurred_at: spec.occurred_at,
            evidence_ref: spec
                .evidence_ref
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        };
        let row = HandoffEventRow::from_event(&self.record.company_id, &event, now_ms());
        if !store.insert_handoff_event(&row)? {
            let existing = store
                .get_handoff_event(
                    self.record.company_id.as_str(),
                    &event.handoff_id,
                    &event.event_id,
                )?
                .ok_or_else(|| EventError::EventLost(event.event_id.clone()))?;
            if !existing.content_eq(&row) {
                return Err(EventError::EventConflict(event.event_id.clone()));
            }
        }
        self.accept_event(event.clone())?;
        Ok(event)
    }

    /// 幂等 accept：同一请求重复提交返回同一结果，且不追加第二条 `accepted` 事件。
    pub fn accept(
        &mut self,
        store: &SqliteStore,
        request: AcceptRequest,
    ) -> Result<AcceptOutcome, EventError> {
        if request.idempotency_key.trim().is_empty() {
            return Err(EventError::EmptyAcceptanceKey);
        }
        if request.snapshot_key.trim().is_empty() {
            return Err(EventError::EmptySnapshotKey);
        }
        if !canonical_json::is_canonical_timestamp(&request.accepted_at) {
            return Err(EventError::NonCanonicalTimestamp(request.accepted_at));
        }
        if !request.actor.permits(HandoffEventKind::Accepted) {
            return Err(EventError::CapabilityRequired {
                actor: request.actor.kind(),
                kind: HandoffEventKind::Accepted,
            });
        }

        let acceptance_key = request.idempotency_key.trim().to_string();
        let request_digest = accept_digest(&self.record, &request)?;

        // 1) 已经接受过：按 key 决定「读回」还是「拒绝」。
        if let Some(existing) = store
            .get_handoff_acceptance(self.record.company_id.as_str(), &self.record.handoff_id)?
        {
            return self.replay_or_reject(store, existing, &acceptance_key, &request_digest);
        }

        // 2) 未接受：必须是已验证状态，且证据快照必须真实存在。
        if self.state != Some(HandoffState::Validated) {
            return Err(EventError::NotValidated { state: self.state });
        }
        if store
            .get_handoff_snapshot(
                self.record.company_id.as_str(),
                request.snapshot_key.as_str(),
            )?
            .is_none()
        {
            return Err(EventError::UnknownSnapshot(request.snapshot_key));
        }

        let accepted_seq = self.events.len() as u64 + 1;
        let accepted_event_id = format!("accepted#{acceptance_key}");
        let row = HandoffAcceptanceRow {
            company_id: self.record.company_id.as_str().to_string(),
            handoff_id: self.record.handoff_id.clone(),
            acceptance_key: acceptance_key.clone(),
            request_digest: request_digest.clone(),
            accepted_event_id: accepted_event_id.clone(),
            accepted_seq,
            snapshot_key: request.snapshot_key.clone(),
            level: request.level.as_str().to_string(),
            accepted_at: request.accepted_at.clone(),
            actor_kind: request.actor.kind().to_string(),
            actor_ref: request.actor.reference().map(str::to_string),
            recorded_at_ms: now_ms(),
        };
        if !store.insert_handoff_acceptance(&row)? {
            // 竞争失败：另一条路径先接受了这次交接，读回它并保持一致。
            let existing = store
                .get_handoff_acceptance(self.record.company_id.as_str(), &self.record.handoff_id)?
                .ok_or_else(|| EventError::EventLost(acceptance_key.clone()))?;
            return self.replay_or_reject(store, existing, &acceptance_key, &request_digest);
        }

        let event = HandoffEvent {
            handoff_id: self.record.handoff_id.clone(),
            seq: accepted_seq,
            event_id: accepted_event_id.clone(),
            kind: HandoffEventKind::Accepted,
            reason: None,
            actor: request.actor,
            occurred_at: request.accepted_at,
            evidence_ref: Some(request.snapshot_key.clone()),
        };
        self.persist_accept_event(store, &event)?;
        self.accept_event(event)?;

        Ok(AcceptOutcome {
            acceptance_key,
            accepted_event_id,
            accepted_seq,
            snapshot_key: request.snapshot_key,
            replayed: false,
        })
    }

    fn replay_or_reject(
        &mut self,
        store: &SqliteStore,
        existing: HandoffAcceptanceRow,
        acceptance_key: &str,
        request_digest: &str,
    ) -> Result<AcceptOutcome, EventError> {
        if existing.acceptance_key != acceptance_key {
            return Err(EventError::AlreadyAccepted {
                existing_key: existing.acceptance_key,
            });
        }
        if existing.request_digest != request_digest {
            return Err(EventError::IdempotencyKeyReuse {
                acceptance_key: acceptance_key.to_string(),
            });
        }
        // 读回路径顺带修复「接受行已落、事件行未落」的崩溃窗口；事件 id 与序号都取自
        // 第一次的接受行，因此重复提交不会追加第二条 accepted 事件。
        if !self
            .events
            .iter()
            .any(|event| event.event_id == existing.accepted_event_id)
        {
            let event = HandoffEvent {
                handoff_id: self.record.handoff_id.clone(),
                seq: existing.accepted_seq,
                event_id: existing.accepted_event_id.clone(),
                kind: HandoffEventKind::Accepted,
                reason: None,
                actor: actor_from_parts(&existing.actor_kind, existing.actor_ref.as_deref())?,
                occurred_at: existing.accepted_at.clone(),
                evidence_ref: Some(existing.snapshot_key.clone()),
            };
            self.persist_accept_event(store, &event)?;
            self.accept_event(event)?;
        }
        Ok(AcceptOutcome {
            acceptance_key: existing.acceptance_key,
            accepted_event_id: existing.accepted_event_id,
            accepted_seq: existing.accepted_seq,
            snapshot_key: existing.snapshot_key,
            replayed: true,
        })
    }

    fn persist_accept_event(
        &self,
        store: &SqliteStore,
        event: &HandoffEvent,
    ) -> Result<(), EventError> {
        let row = HandoffEventRow::from_event(&self.record.company_id, event, now_ms());
        if store.insert_handoff_event(&row)? {
            return Ok(());
        }
        let existing = store
            .get_handoff_event(
                self.record.company_id.as_str(),
                &event.handoff_id,
                &event.event_id,
            )?
            .ok_or_else(|| EventError::EventLost(event.event_id.clone()))?;
        if existing.content_eq(&row) {
            Ok(())
        } else {
            Err(EventError::EventConflict(event.event_id.clone()))
        }
    }

    /// 校验一条**尚未入账**的事件：能力、范围、必填原因/证据、时间格式。
    fn check_spec(&self, spec: &EventSpec) -> Result<(), EventError> {
        if !spec.actor.permits(spec.kind) {
            return Err(EventError::CapabilityRequired {
                actor: spec.actor.kind(),
                kind: spec.kind,
            });
        }
        if !spec.actor.is_in_scope_for(&self.record) {
            return Err(EventError::OutOfScope {
                actor_attempt: spec.actor.reference().unwrap_or("").to_string(),
                handoff_attempt: self.record.source_attempt_id.as_str().to_string(),
            });
        }
        if !canonical_json::is_canonical_timestamp(&spec.occurred_at) {
            return Err(EventError::NonCanonicalTimestamp(spec.occurred_at.clone()));
        }
        if spec.kind == HandoffEventKind::Blocked
            && spec
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            return Err(EventError::MissingReason(spec.kind));
        }
        if spec.kind.requires_evidence()
            && spec
                .evidence_ref
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            return Err(EventError::MissingEvidence(spec.kind));
        }
        self.check_transition(spec.kind)
    }

    fn check_transition(&self, kind: HandoffEventKind) -> Result<(), EventError> {
        let allowed = matches!(
            (self.state, kind),
            (None, HandoffEventKind::Requested)
                | (Some(HandoffState::Requested), HandoffEventKind::Captured)
                | (Some(HandoffState::Captured), HandoffEventKind::Validated)
                | (Some(HandoffState::Validated), HandoffEventKind::Accepted)
                | (
                    Some(HandoffState::Requested)
                        | Some(HandoffState::Captured)
                        | Some(HandoffState::Validated),
                    HandoffEventKind::Blocked
                )
                | (Some(HandoffState::Blocked), HandoffEventKind::Resumed)
        );
        if allowed {
            Ok(())
        } else {
            Err(EventError::InvalidTransition {
                from: self.state,
                to: kind,
            })
        }
    }

    /// 把已校验的事件折叠进状态机。
    fn accept_event(&mut self, event: HandoffEvent) -> Result<(), EventError> {
        let expected_seq = self.events.len() as u64 + 1;
        if event.seq != expected_seq {
            return Err(EventError::Sequence {
                expected: expected_seq,
                found: event.seq,
            });
        }
        if event.handoff_id != self.record.handoff_id {
            return Err(EventError::HandoffMismatch);
        }
        let spec = EventSpec {
            kind: event.kind,
            actor: event.actor.clone(),
            reason: event.reason.clone(),
            evidence_ref: event.evidence_ref.clone(),
            occurred_at: event.occurred_at.clone(),
        };
        self.check_spec(&spec)?;

        let previous = self.state;
        self.state = match (self.state, event.kind) {
            (None, HandoffEventKind::Requested) => Some(HandoffState::Requested),
            (Some(HandoffState::Requested), HandoffEventKind::Captured) => {
                Some(HandoffState::Captured)
            }
            (Some(HandoffState::Captured), HandoffEventKind::Validated) => {
                Some(HandoffState::Validated)
            }
            (Some(HandoffState::Validated), HandoffEventKind::Accepted) => {
                Some(HandoffState::Accepted)
            }
            (Some(state), HandoffEventKind::Blocked) => {
                self.blocked_from = Some(state);
                Some(HandoffState::Blocked)
            }
            (Some(HandoffState::Blocked), HandoffEventKind::Resumed) => {
                let restored = self.blocked_from.take().unwrap_or(HandoffState::Requested);
                Some(restored)
            }
            _ => previous,
        };
        self.events.push(event);
        Ok(())
    }
}

/// 接受请求的内容摘要：同一 key 下内容不同必须能识别出来。
fn accept_digest(record: &HandoffRecord, request: &AcceptRequest) -> Result<String, EventError> {
    let raw = serde_json::json!({
        "handoff_id": record.handoff_id,
        "work_package_revision": record.work_package_revision.as_str(),
        "snapshot_key": request.snapshot_key,
        "level": request.level.as_str(),
        "actor": request.actor.kind(),
        "actor_ref": request.actor.reference(),
    })
    .to_string();
    let payload = CanonicalValue::parse(&raw).map_err(EventError::Canonical)?;
    Ok(canonical_json::payload_digest(&payload).as_string())
}

/// 行内存储的 (kind, ref) → 能力主体。未知 kind 视为损坏行，不静默降级。
fn actor_from_parts(kind: &str, reference: Option<&str>) -> Result<ActorScope, EventError> {
    match kind {
        "agent" => Ok(ActorScope::ExecutingAgent {
            attempt_id: reference.unwrap_or_default().to_string(),
        }),
        "host" => Ok(ActorScope::Host),
        "human" => Ok(ActorScope::Human {
            subject_id: reference.unwrap_or_default().to_string(),
        }),
        "policy" => Ok(ActorScope::Policy {
            rule: reference.unwrap_or_default().to_string(),
        }),
        other => Err(EventError::CorruptRow(format!("未知 actor kind：{other}"))),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

/// 事件错误。能力不足与范围不符分开，避免「没权限」掩盖「不是这次工作」。
#[derive(Debug)]
pub enum EventError {
    MissingIdentity(&'static str),
    Sequence {
        expected: u64,
        found: u64,
    },
    HandoffMismatch,
    InvalidTransition {
        from: Option<HandoffState>,
        to: HandoffEventKind,
    },
    CapabilityRequired {
        actor: &'static str,
        kind: HandoffEventKind,
    },
    OutOfScope {
        actor_attempt: String,
        handoff_attempt: String,
    },
    MissingReason(HandoffEventKind),
    MissingEvidence(HandoffEventKind),
    NonCanonicalTimestamp(String),
    EventConflict(String),
    EventLost(String),
    CorruptRow(String),
    NotValidated {
        state: Option<HandoffState>,
    },
    UnknownSnapshot(String),
    EmptyAcceptanceKey,
    EmptySnapshotKey,
    IdempotencyKeyReuse {
        acceptance_key: String,
    },
    AlreadyAccepted {
        existing_key: String,
    },
    Canonical(CanonicalError),
    Store(StoreError),
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingIdentity(field) => write!(f, "缺少身份字段：{field}"),
            Self::Sequence { expected, found } => {
                write!(f, "事件序号不连续：期望 {expected}，实际 {found}")
            }
            Self::HandoffMismatch => write!(f, "事件不属于该交接"),
            Self::InvalidTransition { from, to } => match from {
                Some(state) => write!(f, "状态 {state} 不允许追加 {to} 事件"),
                None => write!(f, "交接尚未开始，不能追加 {to} 事件"),
            },
            Self::CapabilityRequired { actor, kind } => {
                write!(f, "{actor} 无权重 {kind} 事件")
            }
            Self::OutOfScope {
                actor_attempt,
                handoff_attempt,
            } => write!(
                f,
                "执行 Agent 只能提交本次工作的交接：agent={actor_attempt}，handoff={handoff_attempt}"
            ),
            Self::MissingReason(kind) => write!(f, "{kind} 事件必须带原因"),
            Self::MissingEvidence(kind) => write!(f, "{kind} 事件必须带快照引用"),
            Self::NonCanonicalTimestamp(value) => write!(f, "时间不是秒级 UTC：{value}"),
            Self::EventConflict(event_id) => write!(f, "同 id 事件内容不同：{event_id}"),
            Self::EventLost(event_id) => write!(f, "事件读取失败：{event_id}"),
            Self::CorruptRow(detail) => write!(f, "事件行损坏：{detail}"),
            Self::NotValidated { state } => match state {
                Some(state) => write!(f, "当前状态 {state} 不能接受，必须先 validated"),
                None => write!(f, "交接尚未开始，不能接受"),
            },
            Self::UnknownSnapshot(key) => write!(f, "快照不存在：{key}"),
            Self::EmptyAcceptanceKey => write!(f, "accept 幂等键不能为空"),
            Self::EmptySnapshotKey => write!(f, "accept 必须指向快照"),
            Self::IdempotencyKeyReuse { acceptance_key } => {
                write!(f, "幂等键 {acceptance_key} 被用于不同内容的 accept 请求")
            }
            Self::AlreadyAccepted { existing_key } => {
                write!(f, "该交接已被接受（幂等键 {existing_key}）")
            }
            Self::Canonical(err) => write!(f, "规范编码失败：{err}"),
            Self::Store(err) => write!(f, "存储错误：{err}"),
        }
    }
}

impl std::error::Error for EventError {}

impl From<StoreError> for EventError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

// ---------------------------------------------------------------------------
// 存储行
// ---------------------------------------------------------------------------

/// `handoff_events` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffEventRow {
    pub company_id: String,
    pub handoff_id: String,
    pub seq: u64,
    pub event_id: String,
    pub kind: String,
    pub reason: Option<String>,
    pub actor_kind: String,
    pub actor_ref: Option<String>,
    pub occurred_at: String,
    pub evidence_ref: Option<String>,
    pub recorded_at_ms: i64,
}

impl HandoffEventRow {
    pub fn from_event(company_id: &DomainId, event: &HandoffEvent, recorded_at_ms: i64) -> Self {
        Self {
            company_id: company_id.as_str().to_string(),
            handoff_id: event.handoff_id.clone(),
            seq: event.seq,
            event_id: event.event_id.clone(),
            kind: event.kind.as_str().to_string(),
            reason: event.reason.clone(),
            actor_kind: event.actor.kind().to_string(),
            actor_ref: event.actor.reference().map(str::to_string),
            occurred_at: event.occurred_at.clone(),
            evidence_ref: event.evidence_ref.clone(),
            recorded_at_ms,
        }
    }

    pub fn to_event(&self) -> Result<HandoffEvent, EventError> {
        let kind = HandoffEventKind::parse(&self.kind)
            .ok_or_else(|| EventError::CorruptRow(format!("未知事件种类：{}", self.kind)))?;
        let actor = actor_from_parts(&self.actor_kind, self.actor_ref.as_deref())?;
        Ok(HandoffEvent {
            handoff_id: self.handoff_id.clone(),
            seq: self.seq,
            event_id: self.event_id.clone(),
            kind,
            reason: self.reason.clone(),
            actor,
            occurred_at: self.occurred_at.clone(),
            evidence_ref: self.evidence_ref.clone(),
        })
    }

    /// 内容等价比较：忽略登记时间。
    pub fn content_eq(&self, other: &Self) -> bool {
        self.company_id == other.company_id
            && self.handoff_id == other.handoff_id
            && self.seq == other.seq
            && self.event_id == other.event_id
            && self.kind == other.kind
            && self.reason == other.reason
            && self.actor_kind == other.actor_kind
            && self.actor_ref == other.actor_ref
            && self.occurred_at == other.occurred_at
            && self.evidence_ref == other.evidence_ref
    }

    /// 事件内容摘要：供审计与「重复 accept 不产生第二条事件」的比对。
    pub fn content_digest(&self) -> Digest {
        let mut body = String::new();
        body.push_str(&format!("seq: {}\n", self.seq));
        body.push_str(&format!("event_id: {}\n", self.event_id));
        body.push_str(&format!("kind: {}\n", self.kind));
        body.push_str(&format!("reason: {:?}\n", self.reason));
        body.push_str(&format!(
            "actor: {}:{}\n",
            self.actor_kind,
            self.actor_ref.as_deref().unwrap_or("")
        ));
        body.push_str(&format!("occurred_at: {}\n", self.occurred_at));
        body.push_str(&format!(
            "evidence_ref: {}\n",
            self.evidence_ref.as_deref().unwrap_or("")
        ));
        Digest::of_bytes(body.as_bytes())
    }
}

/// `handoff_acceptances` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffAcceptanceRow {
    pub company_id: String,
    pub handoff_id: String,
    pub acceptance_key: String,
    pub request_digest: String,
    pub accepted_event_id: String,
    pub accepted_seq: u64,
    pub snapshot_key: String,
    pub level: String,
    pub accepted_at: String,
    /// 接受者的来源与身份：读回时要能还原成同一条 `accepted` 事件，不能伪造。
    pub actor_kind: String,
    pub actor_ref: Option<String>,
    pub recorded_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::handoff_package::HANDOFF_SCHEMA;
    use crate::handoff_package::HandoffPackageHeader;
    use crate::handoff_snapshot::{
        CaptureSource, HandoffSnapshot, PAYLOAD_SEGMENT, RegistrationOutcome, SegmentIntegrity,
        SnapshotBinding,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW_MS: i64 = 1_764_000_000_000;
    const SNAPSHOT_KEY: &str = "company-1#1";

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-handoff-ev-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
    }

    fn snapshot(seq: u64) -> HandoffSnapshot {
        HandoffSnapshot {
            header: HandoffPackageHeader {
                schema: HANDOFF_SCHEMA.to_string(),
                level: HandoffLevel::L2,
                company_id: DomainId::new("company-1"),
                project_id: DomainId::new("project-1"),
                issue_id: DomainId::new("issue-1"),
                work_package_revision: DomainId::new("wp-r1"),
                source_attempt_id: DomainId::new("attempt-1"),
                snapshot_seq: seq,
            },
            binding: SnapshotBinding::new("input-r1", "wp-r1", "scene-r1", seq),
            source: CaptureSource::AgentSubmitted,
            missing: Vec::new(),
            integrity: vec![
                SegmentIntegrity::verified(PAYLOAD_SEGMENT, &Digest::of_bytes(b"payload")),
                SegmentIntegrity::verified("workspace", &Digest::of_bytes(b"workspace")),
            ],
            captured_at: "2026-11-20T10:00:00Z".to_string(),
        }
    }

    fn record() -> HandoffRecord {
        HandoffRecord {
            handoff_id: "handoff-1".to_string(),
            company_id: DomainId::new("company-1"),
            project_id: DomainId::new("project-1"),
            issue_id: DomainId::new("issue-1"),
            work_package_revision: DomainId::new("wp-r1"),
            source_attempt_id: DomainId::new("attempt-1"),
        }
    }

    fn agent() -> ActorScope {
        ActorScope::ExecutingAgent {
            attempt_id: "attempt-1".to_string(),
        }
    }

    fn human() -> ActorScope {
        ActorScope::Human {
            subject_id: "manager-1".to_string(),
        }
    }

    /// 走到 validated：requested → captured → validated，并把快照登记好。
    fn validated_ledger(store: &SqliteStore) -> HandoffLedger {
        let outcome = snapshot(1).register(store, NOW_MS).unwrap();
        assert!(matches!(outcome, RegistrationOutcome::Registered { .. }));
        let mut ledger = HandoffLedger::new(record()).unwrap();
        ledger
            .append(
                store,
                EventSpec::new(HandoffEventKind::Requested, agent(), "2026-11-20T10:00:00Z")
                    .with_reason("预算即将耗尽，准备交接"),
            )
            .unwrap();
        ledger
            .append(
                store,
                EventSpec::new(HandoffEventKind::Captured, agent(), "2026-11-20T10:01:00Z")
                    .with_evidence(SNAPSHOT_KEY),
            )
            .unwrap();
        ledger
            .append(
                store,
                EventSpec::new(
                    HandoffEventKind::Validated,
                    ActorScope::Host,
                    "2026-11-20T10:02:00Z",
                )
                .with_evidence(SNAPSHOT_KEY),
            )
            .unwrap();
        ledger
    }

    fn accept_request(key: &str) -> AcceptRequest {
        AcceptRequest {
            idempotency_key: key.to_string(),
            snapshot_key: SNAPSHOT_KEY.to_string(),
            level: HandoffLevel::L2,
            actor: human(),
            accepted_at: "2026-11-20T10:05:00Z".to_string(),
        }
    }

    #[test]
    fn invariant_i_47_duplicate_accept_returns_same_result() {
        let root = temp_root("dup");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        assert_eq!(ledger.state(), Some(HandoffState::Validated));

        let first = ledger.accept(&store, accept_request("accept-1")).unwrap();
        assert!(!first.replayed());
        assert_eq!(first.accepted_seq, 4);
        assert_eq!(first.accepted_event_id, "accepted#accept-1");

        let before = store.core_row_counts().unwrap();
        let events_before = ledger.events().len();
        let packages_before = store.list_handoff_snapshots("company-1").unwrap().len();

        let second = ledger.accept(&store, accept_request("accept-1")).unwrap();
        assert!(second.replayed(), "重复 accept 必须读回第一次的结果");
        assert_eq!(second.accepted_seq, first.accepted_seq);
        assert_eq!(second.accepted_event_id, first.accepted_event_id);
        assert_eq!(second.acceptance_key, first.acceptance_key);

        // 不追加第二条 accepted 事件、不新建第二份包、不创建第二份执行。
        assert_eq!(ledger.events().len(), events_before);
        assert_eq!(ledger.state(), Some(HandoffState::Accepted));
        assert_eq!(
            store.list_handoff_snapshots("company-1").unwrap().len(),
            packages_before
        );
        let after = store.core_row_counts().unwrap();
        assert_eq!(after.invocations, before.invocations);
        assert_eq!(after.acceptances, before.acceptances);
        assert_eq!(
            store
                .list_handoff_events("company-1", "handoff-1")
                .unwrap()
                .len(),
            events_before
        );
    }

    #[test]
    fn accept_requires_validated_state() {
        let root = temp_root("not-validated");
        let store = open_store(&root);
        let outcome = snapshot(1).register(&store, NOW_MS).unwrap();
        assert!(matches!(outcome, RegistrationOutcome::Registered { .. }));
        let mut ledger = HandoffLedger::new(record()).unwrap();
        ledger
            .append(
                &store,
                EventSpec::new(HandoffEventKind::Requested, agent(), "2026-11-20T10:00:00Z"),
            )
            .unwrap();
        ledger
            .append(
                &store,
                EventSpec::new(HandoffEventKind::Captured, agent(), "2026-11-20T10:01:00Z")
                    .with_evidence(SNAPSHOT_KEY),
            )
            .unwrap();

        let err = ledger
            .accept(&store, accept_request("accept-1"))
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::NotValidated {
                state: Some(HandoffState::Captured)
            }
        ));
        assert!(
            store
                .get_handoff_acceptance("company-1", "handoff-1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn agent_cannot_accept_or_validate() {
        let root = temp_root("agent-cap");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);

        let mut request = accept_request("accept-1");
        request.actor = agent();
        let err = ledger.accept(&store, request).unwrap_err();
        assert!(matches!(
            err,
            EventError::CapabilityRequired {
                actor: "agent",
                kind: HandoffEventKind::Accepted
            }
        ));

        // 执行 Agent 也不能替宿主做验证。
        let err = ledger
            .append(
                &store,
                EventSpec::new(HandoffEventKind::Blocked, agent(), "2026-11-20T10:03:00Z")
                    .with_reason("自认失败"),
            )
            .unwrap_err();
        assert!(matches!(err, EventError::CapabilityRequired { .. }));
    }

    #[test]
    fn agent_scope_is_limited_to_its_own_attempt() {
        let root = temp_root("agent-scope");
        let store = open_store(&root);
        let mut ledger = HandoffLedger::new(record()).unwrap();
        let err = ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Requested,
                    ActorScope::ExecutingAgent {
                        attempt_id: "attempt-other".to_string(),
                    },
                    "2026-11-20T10:00:00Z",
                ),
            )
            .unwrap_err();
        assert!(matches!(err, EventError::OutOfScope { .. }));
    }

    #[test]
    fn blocked_requires_reason_and_surfaces_state() {
        let root = temp_root("blocked");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);

        let err = ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Blocked,
                    ActorScope::Policy {
                        rule: "secret-scan".to_string(),
                    },
                    "2026-11-20T10:03:00Z",
                ),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::MissingReason(HandoffEventKind::Blocked)
        ));

        ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Blocked,
                    ActorScope::Policy {
                        rule: "secret-scan".to_string(),
                    },
                    "2026-11-20T10:03:00Z",
                )
                .with_reason("清单里出现未脱敏的凭据"),
            )
            .unwrap();
        assert_eq!(ledger.state(), Some(HandoffState::Blocked));

        let err = ledger
            .accept(&store, accept_request("accept-1"))
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::NotValidated {
                state: Some(HandoffState::Blocked)
            }
        ));
    }

    #[test]
    fn resumed_restores_the_state_before_block() {
        let root = temp_root("resume");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Blocked,
                    ActorScope::Policy {
                        rule: "budget".to_string(),
                    },
                    "2026-11-20T10:03:00Z",
                )
                .with_reason("预算未确认"),
            )
            .unwrap();
        ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Resumed,
                    ActorScope::Host,
                    "2026-11-20T10:04:00Z",
                )
                .with_reason("预算已补足"),
            )
            .unwrap();
        assert_eq!(ledger.state(), Some(HandoffState::Validated));
        assert!(ledger.accept(&store, accept_request("accept-1")).is_ok());
    }

    #[test]
    fn validated_requires_evidence_reference() {
        let root = temp_root("evidence");
        let store = open_store(&root);
        let mut ledger = HandoffLedger::new(record()).unwrap();
        ledger
            .append(
                &store,
                EventSpec::new(HandoffEventKind::Requested, agent(), "2026-11-20T10:00:00Z"),
            )
            .unwrap();
        ledger
            .append(
                &store,
                EventSpec::new(HandoffEventKind::Captured, agent(), "2026-11-20T10:01:00Z")
                    .with_evidence(SNAPSHOT_KEY),
            )
            .unwrap();
        let err = ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Validated,
                    ActorScope::Host,
                    "2026-11-20T10:02:00Z",
                ),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::MissingEvidence(HandoffEventKind::Validated)
        ));
    }

    #[test]
    fn transitions_reject_out_of_order_events() {
        let root = temp_root("order");
        let store = open_store(&root);
        let mut ledger = HandoffLedger::new(record()).unwrap();
        let err = ledger
            .append(
                &store,
                EventSpec::new(HandoffEventKind::Captured, agent(), "2026-11-20T10:01:00Z")
                    .with_evidence(SNAPSHOT_KEY),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::InvalidTransition { from: None, .. }
        ));

        ledger
            .append(
                &store,
                EventSpec::new(HandoffEventKind::Requested, agent(), "2026-11-20T10:00:00Z"),
            )
            .unwrap();
        let err = ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Validated,
                    ActorScope::Host,
                    "2026-11-20T10:02:00Z",
                )
                .with_evidence(SNAPSHOT_KEY),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::InvalidTransition {
                from: Some(HandoffState::Requested),
                to: HandoffEventKind::Validated
            }
        ));
    }

    #[test]
    fn accepted_timeline_rejects_further_events() {
        let root = temp_root("after-accept");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        ledger.accept(&store, accept_request("accept-1")).unwrap();
        let err = ledger
            .append(
                &store,
                EventSpec::new(
                    HandoffEventKind::Blocked,
                    ActorScope::Policy {
                        rule: "late".to_string(),
                    },
                    "2026-11-20T10:06:00Z",
                )
                .with_reason("迟到"),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::InvalidTransition {
                from: Some(HandoffState::Accepted),
                ..
            }
        ));
    }

    #[test]
    fn second_accept_with_another_key_is_rejected() {
        let root = temp_root("second-key");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        ledger.accept(&store, accept_request("accept-1")).unwrap();
        let err = ledger
            .accept(&store, accept_request("accept-2"))
            .unwrap_err();
        assert!(matches!(
            err,
            EventError::AlreadyAccepted { existing_key } if existing_key == "accept-1"
        ));
    }

    #[test]
    fn same_key_with_different_content_is_rejected() {
        let root = temp_root("key-reuse");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        ledger.accept(&store, accept_request("accept-1")).unwrap();

        let mut other = accept_request("accept-1");
        other.level = HandoffLevel::L1;
        let err = ledger.accept(&store, other).unwrap_err();
        assert!(matches!(err, EventError::IdempotencyKeyReuse { .. }));
    }

    #[test]
    fn accept_requires_an_existing_snapshot() {
        let root = temp_root("unknown-snap");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        let mut request = accept_request("accept-1");
        request.snapshot_key = "company-1#99".to_string();
        let err = ledger.accept(&store, request).unwrap_err();
        assert!(matches!(err, EventError::UnknownSnapshot(key) if key == "company-1#99"));
    }

    #[test]
    fn acceptance_and_events_survive_reload() {
        let root = temp_root("reload");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        let first = ledger.accept(&store, accept_request("accept-1")).unwrap();
        drop(ledger);

        let mut reloaded = HandoffLedger::load(&store, record()).unwrap();
        assert_eq!(reloaded.state(), Some(HandoffState::Accepted));
        assert_eq!(reloaded.events().len(), 4);
        let again = reloaded.accept(&store, accept_request("accept-1")).unwrap();
        assert!(again.replayed());
        assert_eq!(again.accepted_seq, first.accepted_seq);

        // 事件行的来源被原样读回：不因为重放就变成别的角色。
        let accepted = reloaded
            .events()
            .iter()
            .find(|event| event.kind == HandoffEventKind::Accepted)
            .unwrap();
        assert_eq!(accepted.actor, human());
        assert_eq!(accepted.evidence_ref.as_deref(), Some(SNAPSHOT_KEY));
    }

    #[test]
    fn sequence_gap_is_detected_on_load() {
        let root = temp_root("gap");
        let store = open_store(&root);
        let row = HandoffEventRow {
            company_id: "company-1".to_string(),
            handoff_id: "handoff-1".to_string(),
            seq: 2,
            event_id: "handoff-1#2".to_string(),
            kind: "requested".to_string(),
            reason: None,
            actor_kind: "agent".to_string(),
            actor_ref: Some("attempt-1".to_string()),
            occurred_at: "2026-11-20T10:00:00Z".to_string(),
            evidence_ref: None,
            recorded_at_ms: NOW_MS,
        };
        assert!(store.insert_handoff_event(&row).unwrap());
        let err = HandoffLedger::load(&store, record()).unwrap_err();
        assert!(matches!(
            err,
            EventError::Sequence {
                expected: 1,
                found: 2
            }
        ));
    }

    #[test]
    fn corrupt_actor_kind_is_not_silently_downgraded() {
        let row = HandoffEventRow {
            company_id: "company-1".to_string(),
            handoff_id: "handoff-1".to_string(),
            seq: 1,
            event_id: "handoff-1#1".to_string(),
            kind: "requested".to_string(),
            reason: None,
            actor_kind: "robot".to_string(),
            actor_ref: None,
            occurred_at: "2026-11-20T10:00:00Z".to_string(),
            evidence_ref: None,
            recorded_at_ms: NOW_MS,
        };
        assert!(matches!(row.to_event(), Err(EventError::CorruptRow(_))));
    }

    #[test]
    fn accept_rejects_non_canonical_timestamp() {
        let root = temp_root("bad-time");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        let mut request = accept_request("accept-1");
        request.accepted_at = "2026-11-20 10:05".to_string();
        let err = ledger.accept(&store, request).unwrap_err();
        assert!(matches!(err, EventError::NonCanonicalTimestamp(_)));
    }

    #[test]
    fn empty_idempotency_key_is_rejected() {
        let root = temp_root("empty-key");
        let store = open_store(&root);
        let mut ledger = validated_ledger(&store);
        let err = ledger.accept(&store, accept_request("  ")).unwrap_err();
        assert!(matches!(err, EventError::EmptyAcceptanceKey));
    }
}
