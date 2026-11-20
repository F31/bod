//! P2 任务 2.1：持久交接意图、阶段状态机与 `expected_version` 条件提交。
//!
//! 权威语义：V2.9.6 §19.2——接管用**持久化交接意图 + 阶段状态**协调，不得在数据库事务内等待
//! Agent 总结或传输大文件；最终绑定以 `expected_version` 条件提交；重复 accept 返回同一结果，
//! 不创建多份执行。§19.1 规定八阶段顺序，且任何阶段状态变化必须可查询。
//!
//! 本模块的边界：
//! - 只协调「意图到了哪一步」，不判断旧执行是否真的停了（2.2）、不对账（2.3）、不预检内容（2.4）。
//!   阶段名字如实反映「已请求/已记录」，不把「请求过」写成「已完成」。
//! - 等待总结是一个**持久状态**而不是一次数据库事务：`request_summary` 只把请求这件事写进
//!   意图，宿主在事务之外去等 Agent；重启后读到 `awaiting_summary` 就知道不必再请求一次。
//! - 所有写入走同一个条件提交入口（`commit` / `request_summary`），版本不匹配一律
//!   `VersionConflict`，不覆盖别人的推进结果。
//! - 不创建 Attempt：`attempt_created` 阶段只登记「新执行是谁、fencing token 多少」，
//!   Attempt 的落库属于执行协议（P0 4.x），这里不越界写第二业务表。

use crate::canonical_json::{self, CanonicalError};
use crate::domain::DomainId;
use crate::store::{SqliteStore, StoreError};
use std::fmt;

/// 意图 schema：与交接包 schema 分开版本化。
pub const TAKEOVER_SCHEMA: &str = "bod.takeover/v1";

/// 接管阶段。顺序即 §19.1 的八阶段，外加「等待总结」与「已中止」。
///
/// 「等待总结」单独成阶段，是因为它跨越事务边界（宿主在等 Agent），
/// 崩溃恢复必须能一眼看出「正在等」，而不是误以为「还没开始」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TakeoverPhase {
    /// 已提出切换，尚未请求现场总结。
    Requested,
    /// 已请求 Agent 现场总结，等待其返回（不在事务内等待）。
    AwaitingSummary,
    /// 已收到总结候选；字段含义是「收到了」，不是「验证通过了」。
    SummaryReceived,
    /// 已请求旧执行停止、已停止新派发（停止是否确认由 2.2 判定）。
    Draining,
    /// 已捕获现场。
    Captured,
    /// 已检查旧租约与资源隔离。
    IsolationChecked,
    /// 新执行已通过准入与预算检查。
    Admitted,
    /// 已登记新 Attempt 与 fencing token。
    AttemptCreated,
    /// 新工作区已重建。
    Restored,
    /// 核验后继续执行（终态）。
    Running,
    /// 已中止（终态）。
    Aborted,
}

impl TakeoverPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::AwaitingSummary => "awaiting_summary",
            Self::SummaryReceived => "summary_received",
            Self::Draining => "draining",
            Self::Captured => "captured",
            Self::IsolationChecked => "isolation_checked",
            Self::Admitted => "admitted",
            Self::AttemptCreated => "attempt_created",
            Self::Restored => "restored",
            Self::Running => "running",
            Self::Aborted => "aborted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "requested" => Some(Self::Requested),
            "awaiting_summary" => Some(Self::AwaitingSummary),
            "summary_received" => Some(Self::SummaryReceived),
            "draining" => Some(Self::Draining),
            "captured" => Some(Self::Captured),
            "isolation_checked" => Some(Self::IsolationChecked),
            "admitted" => Some(Self::Admitted),
            "attempt_created" => Some(Self::AttemptCreated),
            "restored" => Some(Self::Restored),
            "running" => Some(Self::Running),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }

    /// 终态不再推进。
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Running | Self::Aborted)
    }

    /// 正常推进的下一阶段；分叉（中止）不在此列。
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::Requested => Some(Self::AwaitingSummary),
            Self::AwaitingSummary => Some(Self::SummaryReceived),
            Self::SummaryReceived => Some(Self::Draining),
            Self::Draining => Some(Self::Captured),
            Self::Captured => Some(Self::IsolationChecked),
            Self::IsolationChecked => Some(Self::Admitted),
            Self::Admitted => Some(Self::AttemptCreated),
            Self::AttemptCreated => Some(Self::Restored),
            Self::Restored => Some(Self::Running),
            Self::Running | Self::Aborted => None,
        }
    }
}

impl fmt::Display for TakeoverPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 总结请求的持久状态。`Requested` 是「已经发出去过」，不是「已经拿到」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryState {
    NotRequested,
    Requested,
    Received,
    /// 明确放弃等待（超时或用户取消），不再自动重试。
    Abandoned,
}

impl SummaryState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::Requested => "requested",
            Self::Received => "received",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "not_requested" => Some(Self::NotRequested),
            "requested" => Some(Self::Requested),
            "received" => Some(Self::Received),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }
}

/// 阶段记录：每次状态变化的时间与结果，全程可查询。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseRecord {
    pub phase: TakeoverPhase,
    /// 秒级 UTC。
    pub at: String,
    pub note: Option<String>,
}

impl PhaseRecord {
    pub fn describe(&self) -> String {
        match &self.note {
            Some(note) => format!("{} @ {}（{note}）", self.phase, self.at),
            None => format!("{} @ {}", self.phase, self.at),
        }
    }
}

/// 新执行的登记信息。只登记，不创建 Attempt 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptCreation {
    pub attempt_id: String,
    pub fencing_token: u64,
}

/// 提出切换的输入。`baseline_fencing_token` 是旧执行的 token，用于要求新 token 更高。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeoverRequest {
    pub takeover_key: String,
    pub project_id: DomainId,
    pub issue_id: DomainId,
    pub work_package_revision: DomainId,
    pub from_attempt_id: DomainId,
    pub requested_by: String,
    pub reason: Option<String>,
    pub baseline_fencing_token: Option<u64>,
    pub created_at: String,
}

impl TakeoverRequest {
    fn validate(&self) -> Result<(), TakeoverError> {
        for (field, value) in [
            ("takeover_key", self.takeover_key.as_str()),
            ("project_id", self.project_id.as_str()),
            ("issue_id", self.issue_id.as_str()),
            ("work_package_revision", self.work_package_revision.as_str()),
            ("from_attempt_id", self.from_attempt_id.as_str()),
            ("requested_by", self.requested_by.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(TakeoverError::MissingIdentity(field));
            }
        }
        if !canonical_json::is_canonical_timestamp(&self.created_at) {
            return Err(TakeoverError::NonCanonicalTimestamp(
                self.created_at.clone(),
            ));
        }
        Ok(())
    }
}

/// 一次条件提交的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub to: TakeoverPhase,
    pub at: String,
    pub note: Option<String>,
    /// 进入 `AttemptCreated` 时必须提供。
    pub attempt: Option<AttemptCreation>,
    /// 进入 `SummaryReceived` 时必须提供。
    pub summary_snapshot_key: Option<String>,
}

impl Transition {
    pub fn new(to: TakeoverPhase, at: impl Into<String>) -> Self {
        Self {
            to,
            at: at.into(),
            note: None,
            attempt: None,
            summary_snapshot_key: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn with_attempt(mut self, attempt: AttemptCreation) -> Self {
        self.attempt = Some(attempt);
        self
    }

    pub fn with_summary(mut self, snapshot_key: impl Into<String>) -> Self {
        self.summary_snapshot_key = Some(snapshot_key.into());
        self
    }
}

/// 条件提交的结果。`Unchanged` 表示「已经是这个状态」，没有再写库、也没有递增版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    Committed { version: u64, phase: TakeoverPhase },
    Unchanged { version: u64, phase: TakeoverPhase },
}

/// 请求总结的结果。重复请求返回第一次的时间，宿主据此不再发第二次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    Requested {
        requested_at: String,
        version: u64,
    },
    AlreadyRequested {
        requested_at: String,
        version: u64,
    },
    AlreadyReceived {
        requested_at: String,
        snapshot_key: String,
        version: u64,
    },
    /// 明确放弃过等待：不再自动重试。
    Abandoned {
        requested_at: String,
        version: u64,
    },
}

/// 打开意图的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    Opened(TakeoverIntent),
    /// 同键同内容：没有新建第二行，也没有改动第一次的意图。
    AlreadyOpen(TakeoverIntent),
}

/// 持久交接意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeoverIntent {
    pub intent_id: String,
    pub company_id: DomainId,
    pub request: TakeoverRequest,
    pub phase: TakeoverPhase,
    /// 从 1 开始；每次成功写入 +1。条件提交的比较基准。
    pub version: u64,
    pub summary: SummaryState,
    pub summary_requested_at: Option<String>,
    pub summary_snapshot_key: Option<String>,
    pub attempt: Option<AttemptCreation>,
    pub abort_reason: Option<String>,
    pub history: Vec<PhaseRecord>,
    pub updated_at: String,
}

impl TakeoverIntent {
    /// 打开（或复用）意图。同键同内容幂等；同键不同内容拒绝。
    pub fn open(
        store: &SqliteStore,
        company_id: &DomainId,
        request: TakeoverRequest,
        recorded_at_ms: i64,
    ) -> Result<OpenOutcome, TakeoverError> {
        request.validate()?;
        let intent = Self {
            intent_id: format!("takeover:{}#{}", company_id.as_str(), request.takeover_key),
            company_id: company_id.clone(),
            phase: TakeoverPhase::Requested,
            version: 1,
            summary: SummaryState::NotRequested,
            summary_requested_at: None,
            summary_snapshot_key: None,
            attempt: None,
            abort_reason: None,
            history: vec![PhaseRecord {
                phase: TakeoverPhase::Requested,
                at: request.created_at.clone(),
                note: request.reason.clone(),
            }],
            updated_at: request.created_at.clone(),
            request,
        };
        let row = TakeoverIntentRow::from_intent(&intent, recorded_at_ms)?;
        if store.insert_takeover_intent(&row)? {
            return Ok(OpenOutcome::Opened(intent));
        }
        let existing = store
            .get_takeover_intent(company_id.as_str(), &intent.request.takeover_key)?
            .ok_or_else(|| TakeoverError::IntentLost(intent.intent_id.clone()))?;
        let existing = existing.to_intent()?;
        if existing.request == intent.request {
            Ok(OpenOutcome::AlreadyOpen(existing))
        } else {
            Err(TakeoverError::IntentConflict {
                takeover_key: intent.request.takeover_key,
            })
        }
    }

    /// 从存储读回：重启后的恢复入口。
    pub fn load(
        store: &SqliteStore,
        company_id: &str,
        takeover_key: &str,
    ) -> Result<Option<Self>, TakeoverError> {
        match store.get_takeover_intent(company_id, takeover_key)? {
            Some(row) => Ok(Some(row.to_intent()?)),
            None => Ok(None),
        }
    }

    /// 列出某 Company 的全部意图（推进顺序按创建时间）。
    pub fn list(store: &SqliteStore, company_id: &str) -> Result<Vec<Self>, TakeoverError> {
        store
            .list_takeover_intents(company_id)?
            .iter()
            .map(TakeoverIntentRow::to_intent)
            .collect()
    }

    /// 是否处于等待总结：重启后据此判断「不必再请求一次」。
    pub fn is_awaiting_summary(&self) -> bool {
        self.phase == TakeoverPhase::AwaitingSummary && self.summary == SummaryState::Requested
    }

    /// 请求 Agent 现场总结。**不在事务内等待**：只把「请求过」写进意图。
    pub fn request_summary(
        &mut self,
        store: &SqliteStore,
        expected_version: u64,
        at: &str,
        recorded_at_ms: i64,
    ) -> Result<SummaryOutcome, TakeoverError> {
        if !canonical_json::is_canonical_timestamp(at) {
            return Err(TakeoverError::NonCanonicalTimestamp(at.to_string()));
        }
        match self.summary {
            SummaryState::Requested => {
                return Ok(SummaryOutcome::AlreadyRequested {
                    requested_at: self.summary_requested_at.clone().unwrap_or_default(),
                    version: self.version,
                });
            }
            SummaryState::Received => {
                return Ok(SummaryOutcome::AlreadyReceived {
                    requested_at: self.summary_requested_at.clone().unwrap_or_default(),
                    snapshot_key: self.summary_snapshot_key.clone().unwrap_or_default(),
                    version: self.version,
                });
            }
            SummaryState::Abandoned => {
                return Ok(SummaryOutcome::Abandoned {
                    requested_at: self.summary_requested_at.clone().unwrap_or_default(),
                    version: self.version,
                });
            }
            SummaryState::NotRequested => {}
        }
        if self.phase != TakeoverPhase::Requested {
            return Err(TakeoverError::InvalidTransition {
                from: self.phase,
                to: TakeoverPhase::AwaitingSummary,
            });
        }

        let mut next = self.clone();
        next.summary = SummaryState::Requested;
        next.summary_requested_at = Some(at.to_string());
        next.phase = TakeoverPhase::AwaitingSummary;
        next.updated_at = at.to_string();
        next.history.push(PhaseRecord {
            phase: TakeoverPhase::AwaitingSummary,
            at: at.to_string(),
            note: Some("已请求现场总结（不在事务内等待）".to_string()),
        });
        *self = self.write(store, expected_version, recorded_at_ms, next)?;
        Ok(SummaryOutcome::Requested {
            requested_at: at.to_string(),
            version: self.version,
        })
    }

    /// 条件提交一次阶段推进。
    pub fn commit(
        &mut self,
        store: &SqliteStore,
        expected_version: u64,
        transition: Transition,
        recorded_at_ms: i64,
    ) -> Result<CommitOutcome, TakeoverError> {
        if !canonical_json::is_canonical_timestamp(&transition.at) {
            return Err(TakeoverError::NonCanonicalTimestamp(transition.at.clone()));
        }
        // 已经是目标阶段：幂等返回，不写库、不递增版本。
        if transition.to == self.phase {
            return Ok(CommitOutcome::Unchanged {
                version: self.version,
                phase: self.phase,
            });
        }
        self.check_transition(&transition)?;

        let mut next = self.clone();
        next.phase = transition.to;
        next.updated_at = transition.at.clone();
        next.history.push(PhaseRecord {
            phase: transition.to,
            at: transition.at.clone(),
            note: transition.note.clone(),
        });
        match transition.to {
            TakeoverPhase::SummaryReceived => {
                next.summary = SummaryState::Received;
                next.summary_snapshot_key = transition.summary_snapshot_key.clone();
            }
            TakeoverPhase::AttemptCreated => {
                next.attempt = transition.attempt.clone();
            }
            TakeoverPhase::Aborted => {
                next.abort_reason = transition.note.clone();
            }
            _ => {}
        }

        *self = self.write(store, expected_version, recorded_at_ms, next)?;
        Ok(CommitOutcome::Committed {
            version: self.version,
            phase: self.phase,
        })
    }

    /// 阶段记录的人读视图：每阶段一行。
    pub fn history_lines(&self) -> Vec<String> {
        self.history.iter().map(PhaseRecord::describe).collect()
    }

    /// 直接写一条 `aborted`，供恢复流程放弃时使用。
    pub fn abort(
        &mut self,
        store: &SqliteStore,
        expected_version: u64,
        reason: &str,
        at: &str,
        recorded_at_ms: i64,
    ) -> Result<CommitOutcome, TakeoverError> {
        self.commit(
            store,
            expected_version,
            Transition::new(TakeoverPhase::Aborted, at).with_note(reason.to_string()),
            recorded_at_ms,
        )
    }

    fn check_transition(&self, transition: &Transition) -> Result<(), TakeoverError> {
        if self.phase.is_terminal() {
            return Err(TakeoverError::TerminalPhase(self.phase));
        }
        if transition.to == TakeoverPhase::Aborted {
            let reason = transition
                .note
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if reason.is_none() {
                return Err(TakeoverError::MissingAbortReason);
            }
            return Ok(());
        }
        if self.phase.next() != Some(transition.to) {
            return Err(TakeoverError::InvalidTransition {
                from: self.phase,
                to: transition.to,
            });
        }
        match transition.to {
            TakeoverPhase::SummaryReceived => {
                if self.summary != SummaryState::Requested {
                    return Err(TakeoverError::SummaryNotRequested);
                }
                if transition
                    .summary_snapshot_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_none()
                {
                    return Err(TakeoverError::MissingSummarySnapshot);
                }
            }
            TakeoverPhase::AttemptCreated => {
                let attempt = transition
                    .attempt
                    .as_ref()
                    .ok_or(TakeoverError::MissingAttempt)?;
                if attempt.attempt_id.trim().is_empty() {
                    return Err(TakeoverError::MissingAttempt);
                }
                if let Some(baseline) = self.request.baseline_fencing_token
                    && attempt.fencing_token <= baseline
                {
                    return Err(TakeoverError::FencingNotIncreased {
                        baseline,
                        proposed: attempt.fencing_token,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// 条件写入：`UPDATE ... WHERE version = expected`，写入时把版本推进到 `expected + 1`。
    /// 0 行受影响即版本冲突，不覆盖别人的推进结果。
    fn write(
        &self,
        store: &SqliteStore,
        expected_version: u64,
        recorded_at_ms: i64,
        mut next: Self,
    ) -> Result<Self, TakeoverError> {
        if expected_version != self.version {
            return Err(TakeoverError::VersionConflict {
                expected: expected_version,
                actual: self.version,
            });
        }
        next.version = expected_version + 1;
        let row = TakeoverIntentRow::from_intent(&next, recorded_at_ms)?;
        if store.update_takeover_intent_cas(&row, expected_version)? {
            return Ok(next);
        }
        let actual = store
            .get_takeover_intent(self.company_id.as_str(), &self.request.takeover_key)?
            .map(|row| row.version)
            .unwrap_or(0);
        Err(TakeoverError::VersionConflict {
            expected: expected_version,
            actual,
        })
    }
}

/// 交接意图错误。
#[derive(Debug)]
pub enum TakeoverError {
    MissingIdentity(&'static str),
    NonCanonicalTimestamp(String),
    UnsupportedSchema(String),
    UnknownPhase(String),
    UnknownSummaryState(String),
    InvalidTransition {
        from: TakeoverPhase,
        to: TakeoverPhase,
    },
    TerminalPhase(TakeoverPhase),
    MissingAbortReason,
    MissingAttempt,
    MissingSummarySnapshot,
    SummaryNotRequested,
    FencingNotIncreased {
        baseline: u64,
        proposed: u64,
    },
    VersionConflict {
        expected: u64,
        actual: u64,
    },
    IntentConflict {
        takeover_key: String,
    },
    IntentLost(String),
    CorruptRow(String),
    Canonical(CanonicalError),
    Store(StoreError),
}

impl fmt::Display for TakeoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingIdentity(field) => write!(f, "缺少身份字段：{field}"),
            Self::NonCanonicalTimestamp(value) => {
                write!(f, "时间不是秒级 UTC：{value}")
            }
            Self::UnsupportedSchema(schema) => write!(f, "不支持的意图 schema：{schema}"),
            Self::UnknownPhase(value) => write!(f, "未知阶段：{value}"),
            Self::UnknownSummaryState(value) => write!(f, "未知总结状态：{value}"),
            Self::InvalidTransition { from, to } => {
                write!(f, "阶段 {from} 不能直接推进到 {to}")
            }
            Self::TerminalPhase(phase) => write!(f, "阶段 {phase} 已是终态，不能再推进"),
            Self::MissingAbortReason => write!(f, "中止必须给出原因"),
            Self::MissingAttempt => write!(f, "进入 attempt_created 必须登记新 Attempt"),
            Self::MissingSummarySnapshot => {
                write!(f, "进入 summary_received 必须带总结快照引用")
            }
            Self::SummaryNotRequested => write!(f, "尚未请求总结，不能标记为已收到"),
            Self::FencingNotIncreased { baseline, proposed } => write!(
                f,
                "新 fencing token 必须高于旧执行：baseline={baseline}，proposed={proposed}"
            ),
            Self::VersionConflict { expected, actual } => {
                write!(f, "版本冲突：期望 {expected}，实际 {actual}")
            }
            Self::IntentConflict { takeover_key } => {
                write!(f, "同键意图内容不同：{takeover_key}")
            }
            Self::IntentLost(intent_id) => write!(f, "意图读取失败：{intent_id}"),
            Self::CorruptRow(detail) => write!(f, "意图行损坏：{detail}"),
            Self::Canonical(err) => write!(f, "规范编码失败：{err}"),
            Self::Store(err) => write!(f, "存储错误：{err}"),
        }
    }
}

impl std::error::Error for TakeoverError {}

impl From<StoreError> for TakeoverError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<CanonicalError> for TakeoverError {
    fn from(err: CanonicalError) -> Self {
        Self::Canonical(err)
    }
}

// ---------------------------------------------------------------------------
// 存储行
// ---------------------------------------------------------------------------

/// `takeover_intents` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeoverIntentRow {
    pub company_id: String,
    pub takeover_key: String,
    pub intent_id: String,
    pub project_id: String,
    pub issue_id: String,
    pub work_package_revision: String,
    pub from_attempt_id: String,
    pub requested_by: String,
    pub reason: Option<String>,
    pub baseline_fencing_token: Option<u64>,
    pub created_at: String,
    pub phase: String,
    pub version: u64,
    pub summary_state: String,
    pub summary_requested_at: Option<String>,
    pub summary_snapshot_key: Option<String>,
    pub attempt_id: Option<String>,
    pub fencing_token: Option<u64>,
    pub abort_reason: Option<String>,
    pub history_json: String,
    pub updated_at: String,
    pub recorded_at_ms: i64,
}

impl TakeoverIntentRow {
    pub fn from_intent(
        intent: &TakeoverIntent,
        recorded_at_ms: i64,
    ) -> Result<Self, TakeoverError> {
        let history: Vec<serde_json::Value> = intent
            .history
            .iter()
            .map(|record| {
                serde_json::json!({
                    "phase": record.phase.as_str(),
                    "at": record.at,
                    "note": record.note,
                })
            })
            .collect();
        Ok(Self {
            company_id: intent.company_id.as_str().to_string(),
            takeover_key: intent.request.takeover_key.clone(),
            intent_id: intent.intent_id.clone(),
            project_id: intent.request.project_id.as_str().to_string(),
            issue_id: intent.request.issue_id.as_str().to_string(),
            work_package_revision: intent.request.work_package_revision.as_str().to_string(),
            from_attempt_id: intent.request.from_attempt_id.as_str().to_string(),
            requested_by: intent.request.requested_by.clone(),
            reason: intent.request.reason.clone(),
            baseline_fencing_token: intent.request.baseline_fencing_token,
            created_at: intent.request.created_at.clone(),
            phase: intent.phase.as_str().to_string(),
            version: intent.version,
            summary_state: intent.summary.as_str().to_string(),
            summary_requested_at: intent.summary_requested_at.clone(),
            summary_snapshot_key: intent.summary_snapshot_key.clone(),
            attempt_id: intent.attempt.as_ref().map(|item| item.attempt_id.clone()),
            fencing_token: intent.attempt.as_ref().map(|item| item.fencing_token),
            abort_reason: intent.abort_reason.clone(),
            history_json: serde_json::Value::Array(history).to_string(),
            updated_at: intent.updated_at.clone(),
            recorded_at_ms,
        })
    }

    pub fn to_intent(&self) -> Result<TakeoverIntent, TakeoverError> {
        let phase = TakeoverPhase::parse(&self.phase)
            .ok_or_else(|| TakeoverError::UnknownPhase(self.phase.clone()))?;
        let summary = SummaryState::parse(&self.summary_state)
            .ok_or_else(|| TakeoverError::UnknownSummaryState(self.summary_state.clone()))?;
        let history = parse_history(&self.history_json)?;
        // 阶段列与最后一条历史必须一致：手工改库伪装阶段会在这里暴露。
        match history.last() {
            Some(last) if last.phase == phase => {}
            _ => {
                return Err(TakeoverError::CorruptRow(format!(
                    "阶段列 {} 与历史末条不一致",
                    self.phase
                )));
            }
        }
        Ok(TakeoverIntent {
            intent_id: self.intent_id.clone(),
            company_id: DomainId::new(self.company_id.clone()),
            request: TakeoverRequest {
                takeover_key: self.takeover_key.clone(),
                project_id: DomainId::new(self.project_id.clone()),
                issue_id: DomainId::new(self.issue_id.clone()),
                work_package_revision: DomainId::new(self.work_package_revision.clone()),
                from_attempt_id: DomainId::new(self.from_attempt_id.clone()),
                requested_by: self.requested_by.clone(),
                reason: self.reason.clone(),
                baseline_fencing_token: self.baseline_fencing_token,
                created_at: self.created_at.clone(),
            },
            phase,
            version: self.version,
            summary,
            summary_requested_at: self.summary_requested_at.clone(),
            summary_snapshot_key: self.summary_snapshot_key.clone(),
            attempt: match (&self.attempt_id, self.fencing_token) {
                (Some(attempt_id), Some(fencing_token)) => Some(AttemptCreation {
                    attempt_id: attempt_id.clone(),
                    fencing_token,
                }),
                (None, None) => None,
                _ => {
                    return Err(TakeoverError::CorruptRow(
                        "attempt_id 与 fencing_token 只有一半".to_string(),
                    ));
                }
            },
            abort_reason: self.abort_reason.clone(),
            history,
            updated_at: self.updated_at.clone(),
        })
    }

    /// 内容等价比较：忽略登记时间。
    pub fn content_eq(&self, other: &Self) -> bool {
        self.company_id == other.company_id
            && self.takeover_key == other.takeover_key
            && self.intent_id == other.intent_id
            && self.project_id == other.project_id
            && self.issue_id == other.issue_id
            && self.work_package_revision == other.work_package_revision
            && self.from_attempt_id == other.from_attempt_id
            && self.requested_by == other.requested_by
            && self.reason == other.reason
            && self.baseline_fencing_token == other.baseline_fencing_token
            && self.created_at == other.created_at
            && self.phase == other.phase
            && self.version == other.version
            && self.summary_state == other.summary_state
            && self.summary_requested_at == other.summary_requested_at
            && self.summary_snapshot_key == other.summary_snapshot_key
            && self.attempt_id == other.attempt_id
            && self.fencing_token == other.fencing_token
            && self.abort_reason == other.abort_reason
            && self.history_json == other.history_json
            && self.updated_at == other.updated_at
    }
}

fn parse_history(raw: &str) -> Result<Vec<PhaseRecord>, TakeoverError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|err| TakeoverError::CorruptRow(format!("历史不是 JSON：{err}")))?;
    let items = value
        .as_array()
        .ok_or_else(|| TakeoverError::CorruptRow("历史不是数组".to_string()))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let phase = item
            .get("phase")
            .and_then(|value| value.as_str())
            .and_then(TakeoverPhase::parse)
            .ok_or_else(|| TakeoverError::CorruptRow("历史缺少阶段".to_string()))?;
        let at = item
            .get("at")
            .and_then(|value| value.as_str())
            .ok_or_else(|| TakeoverError::CorruptRow("历史缺少时间".to_string()))?;
        let note = item
            .get("note")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        out.push(PhaseRecord {
            phase,
            at: at.to_string(),
            note,
        });
    }
    if out.is_empty() {
        return Err(TakeoverError::CorruptRow("历史为空".to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW_MS: i64 = 1_764_100_000_000;

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-takeover-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
    }

    fn request() -> TakeoverRequest {
        TakeoverRequest {
            takeover_key: "takeover-1".to_string(),
            project_id: DomainId::new("project-1"),
            issue_id: DomainId::new("issue-1"),
            work_package_revision: DomainId::new("wp-r1"),
            from_attempt_id: DomainId::new("attempt-old"),
            requested_by: "manager-1".to_string(),
            reason: Some("预算耗尽，换已测运行时".to_string()),
            baseline_fencing_token: Some(3),
            created_at: "2026-11-20T11:00:00Z".to_string(),
        }
    }

    fn opened(store: &SqliteStore) -> TakeoverIntent {
        match TakeoverIntent::open(store, &DomainId::new("company-1"), request(), NOW_MS).unwrap() {
            OpenOutcome::Opened(intent) => intent,
            other => panic!("首次开单应新建，实际 {other:?}"),
        }
    }

    fn advance(
        intent: &mut TakeoverIntent,
        store: &SqliteStore,
        to: TakeoverPhase,
        at: &str,
    ) -> CommitOutcome {
        let expected = intent.version;
        let mut transition = Transition::new(to, at);
        // 收到总结必须带快照引用；测试里统一指向 `company-1#1`。
        if to == TakeoverPhase::SummaryReceived {
            transition = transition.with_summary("company-1#1");
        }
        intent.commit(store, expected, transition, NOW_MS).unwrap()
    }

    #[test]
    fn intent_open_is_idempotent_per_key() {
        let root = temp_root("open");
        let store = open_store(&root);
        let company = DomainId::new("company-1");
        let first = TakeoverIntent::open(&store, &company, request(), NOW_MS).unwrap();
        let second = TakeoverIntent::open(&store, &company, request(), NOW_MS + 1).unwrap();

        let (first, second) = match (first, second) {
            (OpenOutcome::Opened(a), OpenOutcome::AlreadyOpen(b)) => (a, b),
            other => panic!("第二次应复用，实际 {other:?}"),
        };
        assert_eq!(first.version, 1);
        assert_eq!(second.version, 1);
        assert_eq!(second.request, first.request);
        assert_eq!(second.history.len(), 1);
        assert_eq!(TakeoverIntent::list(&store, "company-1").unwrap().len(), 1);
    }

    #[test]
    fn same_key_with_different_content_is_rejected() {
        let root = temp_root("open-conflict");
        let store = open_store(&root);
        let company = DomainId::new("company-1");
        TakeoverIntent::open(&store, &company, request(), NOW_MS).unwrap();

        let mut other = request();
        other.from_attempt_id = DomainId::new("attempt-other");
        let err = TakeoverIntent::open(&store, &company, other, NOW_MS).unwrap_err();
        assert!(
            matches!(err, TakeoverError::IntentConflict { takeover_key } if takeover_key == "takeover-1")
        );
    }

    #[test]
    fn crash_while_awaiting_summary_does_not_request_summary_again() {
        let root = temp_root("crash-summary");
        let store = open_store(&root);
        let mut intent = opened(&store);

        let outcome = intent
            .request_summary(&store, intent.version, "2026-11-20T11:01:00Z", NOW_MS)
            .unwrap();
        assert!(matches!(outcome, SummaryOutcome::Requested { .. }));
        assert_eq!(intent.version, 2);
        assert_eq!(intent.phase, TakeoverPhase::AwaitingSummary);
        let before = store.core_row_counts().unwrap();

        // 宿主在此崩溃：丢掉内存态，只从存储恢复。
        drop(intent);
        let mut restored = TakeoverIntent::load(&store, "company-1", "takeover-1")
            .unwrap()
            .unwrap();
        assert!(restored.is_awaiting_summary());
        assert_eq!(restored.phase, TakeoverPhase::AwaitingSummary);
        assert_eq!(restored.version, 2);

        // 恢复流程会重新申请一次总结：必须读回第一次的请求，而不是再发一次。
        let again = restored
            .request_summary(&store, restored.version, "2026-11-20T11:02:00Z", NOW_MS)
            .unwrap();
        match again {
            SummaryOutcome::AlreadyRequested {
                requested_at,
                version,
            } => {
                assert_eq!(requested_at, "2026-11-20T11:01:00Z");
                assert_eq!(version, 2);
            }
            other => panic!("应读回第一次请求，实际 {other:?}"),
        }
        assert_eq!(restored.version, 2, "重复请求不得推进版本");
        assert_eq!(restored.history.len(), 2, "不得追加第二条 awaiting_summary");

        // 不创建 Attempt：意图里没有新执行，账本里也没有 invocation。
        assert!(restored.attempt.is_none());
        let after = store.core_row_counts().unwrap();
        assert_eq!(after.invocations, before.invocations);
        assert_eq!(after.invocations, 0);
        assert_eq!(TakeoverIntent::list(&store, "company-1").unwrap().len(), 1);
    }

    #[test]
    fn expected_version_conflict_blocks_the_stale_writer() {
        let root = temp_root("cas");
        let store = open_store(&root);
        let mut intent = opened(&store);
        intent
            .request_summary(&store, intent.version, "2026-11-20T11:01:00Z", NOW_MS)
            .unwrap();
        advance(
            &mut intent,
            &store,
            TakeoverPhase::SummaryReceived,
            "2026-11-20T11:02:00Z",
        );
        assert_eq!(intent.version, 3);

        // 两个读端同时拿到 version=3；先写者胜。
        let mut first = intent.clone();
        let mut second = intent.clone();
        let outcome = first
            .commit(
                &store,
                3,
                Transition::new(TakeoverPhase::Draining, "2026-11-20T11:03:00Z"),
                NOW_MS,
            )
            .unwrap();
        assert_eq!(
            outcome,
            CommitOutcome::Committed {
                version: 4,
                phase: TakeoverPhase::Draining
            }
        );

        let err = second
            .commit(
                &store,
                3,
                Transition::new(TakeoverPhase::Draining, "2026-11-20T11:04:00Z")
                    .with_note("迟到的推进"),
                NOW_MS,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            TakeoverError::VersionConflict {
                expected: 3,
                actual: 4
            }
        ));

        // 库里的结果仍是先写者的：阶段、版本、历史都没有被迟到者改动。
        let stored = TakeoverIntent::load(&store, "company-1", "takeover-1")
            .unwrap()
            .unwrap();
        assert_eq!(stored.version, 4);
        assert_eq!(stored.phase, TakeoverPhase::Draining);
        assert_eq!(stored.history.len(), 4);
        assert_eq!(stored.history[3].at, "2026-11-20T11:03:00Z");
    }

    #[test]
    fn out_of_order_phase_is_rejected() {
        let root = temp_root("order");
        let store = open_store(&root);
        let mut intent = opened(&store);
        let err = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::Draining, "2026-11-20T11:01:00Z"),
                NOW_MS,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            TakeoverError::InvalidTransition {
                from: TakeoverPhase::Requested,
                to: TakeoverPhase::Draining
            }
        ));
        assert_eq!(intent.version, 1);
    }

    #[test]
    fn commit_to_the_same_phase_is_idempotent() {
        let root = temp_root("same");
        let store = open_store(&root);
        let mut intent = opened(&store);
        intent
            .request_summary(&store, intent.version, "2026-11-20T11:01:00Z", NOW_MS)
            .unwrap();
        advance(
            &mut intent,
            &store,
            TakeoverPhase::SummaryReceived,
            "2026-11-20T11:02:00Z",
        );
        advance(
            &mut intent,
            &store,
            TakeoverPhase::Draining,
            "2026-11-20T11:03:00Z",
        );
        let version = intent.version;
        let history = intent.history.len();

        let again = intent
            .commit(
                &store,
                version,
                Transition::new(TakeoverPhase::Draining, "2026-11-20T11:05:00Z"),
                NOW_MS,
            )
            .unwrap();
        assert_eq!(
            again,
            CommitOutcome::Unchanged {
                version,
                phase: TakeoverPhase::Draining
            }
        );
        assert_eq!(intent.version, version);
        assert_eq!(intent.history.len(), history);
    }

    #[test]
    fn summary_received_requires_a_snapshot_reference() {
        let root = temp_root("summary");
        let store = open_store(&root);
        let mut intent = opened(&store);
        intent
            .request_summary(&store, intent.version, "2026-11-20T11:01:00Z", NOW_MS)
            .unwrap();
        let err = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::SummaryReceived, "2026-11-20T11:02:00Z"),
                NOW_MS,
            )
            .unwrap_err();
        assert!(matches!(err, TakeoverError::MissingSummarySnapshot));

        let outcome = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::SummaryReceived, "2026-11-20T11:02:00Z")
                    .with_summary("company-1#1"),
                NOW_MS,
            )
            .unwrap();
        assert!(matches!(outcome, CommitOutcome::Committed { .. }));
        assert_eq!(intent.summary, SummaryState::Received);
        assert_eq!(intent.summary_snapshot_key.as_deref(), Some("company-1#1"));
    }

    #[test]
    fn attempt_created_requires_higher_fencing_token() {
        let root = temp_root("fencing");
        let store = open_store(&root);
        let mut intent = walk_to_admitted(&store);

        let err = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::AttemptCreated, "2026-11-20T11:08:00Z")
                    .with_attempt(AttemptCreation {
                        attempt_id: "attempt-new".to_string(),
                        fencing_token: 3,
                    }),
                NOW_MS,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            TakeoverError::FencingNotIncreased {
                baseline: 3,
                proposed: 3
            }
        ));

        let outcome = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::AttemptCreated, "2026-11-20T11:08:00Z")
                    .with_attempt(AttemptCreation {
                        attempt_id: "attempt-new".to_string(),
                        fencing_token: 4,
                    }),
                NOW_MS,
            )
            .unwrap();
        assert!(matches!(outcome, CommitOutcome::Committed { .. }));
        assert_eq!(
            intent.attempt,
            Some(AttemptCreation {
                attempt_id: "attempt-new".to_string(),
                fencing_token: 4
            })
        );

        // 新执行没有被这里创建成 Attempt 行：接管意图只登记，不越界写执行协议表。
        let counts = store.core_row_counts().unwrap();
        assert_eq!(counts.invocations, 0);
        assert_eq!(counts.acceptances, 0);
    }

    #[test]
    fn attempt_created_requires_the_attempt_payload() {
        let root = temp_root("no-attempt");
        let store = open_store(&root);
        let mut intent = walk_to_admitted(&store);
        let err = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::AttemptCreated, "2026-11-20T11:08:00Z"),
                NOW_MS,
            )
            .unwrap_err();
        assert!(matches!(err, TakeoverError::MissingAttempt));
    }

    #[test]
    fn abort_requires_reason_and_is_terminal() {
        let root = temp_root("abort");
        let store = open_store(&root);
        let mut intent = opened(&store);
        let err = intent.abort(
            &store,
            intent.version,
            "   ",
            "2026-11-20T11:01:00Z",
            NOW_MS,
        );
        assert!(matches!(err, Err(TakeoverError::MissingAbortReason)));

        intent
            .abort(
                &store,
                intent.version,
                "用户取消切换",
                "2026-11-20T11:01:00Z",
                NOW_MS,
            )
            .unwrap();
        assert_eq!(intent.phase, TakeoverPhase::Aborted);
        assert_eq!(intent.abort_reason.as_deref(), Some("用户取消切换"));

        let err = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::AwaitingSummary, "2026-11-20T11:02:00Z"),
                NOW_MS,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            TakeoverError::TerminalPhase(TakeoverPhase::Aborted)
        ));
    }

    #[test]
    fn history_records_every_phase_in_order() {
        let root = temp_root("history");
        let store = open_store(&root);
        let intent = walk_to_running(&store);

        let phases: Vec<TakeoverPhase> = intent.history.iter().map(|item| item.phase).collect();
        assert_eq!(
            phases,
            vec![
                TakeoverPhase::Requested,
                TakeoverPhase::AwaitingSummary,
                TakeoverPhase::SummaryReceived,
                TakeoverPhase::Draining,
                TakeoverPhase::Captured,
                TakeoverPhase::IsolationChecked,
                TakeoverPhase::Admitted,
                TakeoverPhase::AttemptCreated,
                TakeoverPhase::Restored,
                TakeoverPhase::Running,
            ]
        );
        assert_eq!(intent.history.len(), 10);
        assert_eq!(intent.version, 10, "每次推进 +1（含请求总结）");
        assert_eq!(intent.history_lines().len(), 10);
        assert!(intent.history[1].describe().contains("不在事务内等待"));

        // 重启后阶段、版本、历史一致。
        let restored = TakeoverIntent::load(&store, "company-1", "takeover-1")
            .unwrap()
            .unwrap();
        assert_eq!(restored.phase, TakeoverPhase::Running);
        assert_eq!(restored.version, 10);
        assert_eq!(restored.history, intent.history);
        assert_eq!(restored.attempt, intent.attempt);
    }

    #[test]
    fn phase_and_history_mismatch_is_corrupt() {
        let root = temp_root("corrupt");
        let store = open_store(&root);
        let intent = opened(&store);
        // 模拟手工改库：只改 phase 列，不改历史，读回时必须报损坏而不是接受。
        let mut row = TakeoverIntentRow::from_intent(&intent, NOW_MS).unwrap();
        row.phase = "draining".to_string();
        row.version = 2;
        assert!(store.update_takeover_intent_cas(&row, 1).unwrap());
        let err = TakeoverIntent::load(&store, "company-1", "takeover-1").unwrap_err();
        assert!(matches!(err, TakeoverError::CorruptRow(_)));
    }

    #[test]
    fn request_summary_rejects_non_canonical_timestamp() {
        let root = temp_root("bad-time");
        let store = open_store(&root);
        let mut intent = opened(&store);
        let err = intent
            .request_summary(&store, intent.version, "2026-11-20 11:01", NOW_MS)
            .unwrap_err();
        assert!(matches!(err, TakeoverError::NonCanonicalTimestamp(_)));
    }

    #[test]
    fn missing_baseline_accepts_any_fencing_token() {
        let root = temp_root("no-baseline");
        let store = open_store(&root);
        let mut request = request();
        request.baseline_fencing_token = None;
        let OpenOutcome::Opened(mut intent) =
            TakeoverIntent::open(&store, &DomainId::new("company-1"), request, NOW_MS).unwrap()
        else {
            panic!("首次开单应新建");
        };
        walk_from_summary_to_admitted(&mut intent, &store);
        let outcome = intent
            .commit(
                &store,
                intent.version,
                Transition::new(TakeoverPhase::AttemptCreated, "2026-11-20T11:08:00Z")
                    .with_attempt(AttemptCreation {
                        attempt_id: "attempt-new".to_string(),
                        fencing_token: 1,
                    }),
                NOW_MS,
            )
            .unwrap();
        assert!(matches!(outcome, CommitOutcome::Committed { .. }));
    }

    /// 走到 admitted（含请求总结）。
    fn walk_to_admitted(store: &SqliteStore) -> TakeoverIntent {
        let mut intent = opened(store);
        walk_from_summary_to_admitted(&mut intent, store);
        intent
    }

    fn walk_from_summary_to_admitted(intent: &mut TakeoverIntent, store: &SqliteStore) {
        intent
            .request_summary(store, intent.version, "2026-11-20T11:01:00Z", NOW_MS)
            .unwrap();
        advance(
            intent,
            store,
            TakeoverPhase::SummaryReceived,
            "2026-11-20T11:02:00Z",
        );
        advance(
            intent,
            store,
            TakeoverPhase::Draining,
            "2026-11-20T11:03:00Z",
        );
        advance(
            intent,
            store,
            TakeoverPhase::Captured,
            "2026-11-20T11:04:00Z",
        );
        advance(
            intent,
            store,
            TakeoverPhase::IsolationChecked,
            "2026-11-20T11:05:00Z",
        );
        advance(
            intent,
            store,
            TakeoverPhase::Admitted,
            "2026-11-20T11:06:00Z",
        );
    }

    fn walk_to_running(store: &SqliteStore) -> TakeoverIntent {
        let mut intent = walk_to_admitted(store);
        intent
            .commit(
                store,
                intent.version,
                Transition::new(TakeoverPhase::AttemptCreated, "2026-11-20T11:08:00Z")
                    .with_attempt(AttemptCreation {
                        attempt_id: "attempt-new".to_string(),
                        fencing_token: 4,
                    }),
                NOW_MS,
            )
            .unwrap();
        advance(
            &mut intent,
            store,
            TakeoverPhase::Restored,
            "2026-11-20T11:09:00Z",
        );
        advance(
            &mut intent,
            store,
            TakeoverPhase::Running,
            "2026-11-20T11:10:00Z",
        );
        intent
    }
}
