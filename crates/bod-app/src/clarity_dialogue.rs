//! P1 D3 讨论与澄清（任务 3.1—3.3；§39.7 I-38、§39.8 I-38）。
//!
//! 目标是把「说」和「做」分开：评论、回答、外部回调都只能形成讨论事实或 Question，
//! 任何接受、派发、授权、结单都必须由**显式且已授权**的转换命令产生，且转换后仍要走
//! 对应流程（接受/修订），不因为文本里有 "LGTM" 就生效。
//!
//! 四块：
//! 1. [`Comment`]：讨论记录，带 author/时间/关联对象修订/来源；编辑保留原文版本，关联修订不变；
//! 2. [`ClarificationQuestion`]：绑定 attempt/input 修订的 Question，产生 NeedsInput，
//!    记录答复主体、阻塞范围与唤醒条件；超时只记提示，不猜测、不自动放行；
//! 3. [`PermissionSet`]/[`IssueDiscussion::convert`]：评论 → Decision/ScopeDecision/FeedbackProposal
//!    的显式转换，绑定对象、修订与授权依据；
//! 4. [`IssueDiscussion::ingest_callback`]：外部回调先认证再授权，无法验证身份的材料只进
//!    不可信候选，不进权威讨论。
//!
//! 边界：本模块不写库、不发命令，只产出 [`ControlCommand`] 描述；执行由 4.x 控制面完成。
//! 评论正文与审计摘要分开：审计条目只含元数据与正文摘要（[`Comment::audit`]）。

use std::collections::BTreeSet;

use bod_core::artifact::sha256_hex;

use crate::context_handoff::{Trust, is_definite_revision};

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarityError {
    EmptyField(&'static str),
    EmptyBody,
    /// 修订不是确定修订（空或 `latest`/`HEAD` 等可变指针）。
    IndefiniteRevision {
        field: &'static str,
        value: String,
    },
    CommentNotFound(String),
    QuestionNotFound(String),
    /// Question 已答复或已撤销，不能再次答复/超时。
    QuestionClosed(String),
    /// 答复主体不在该 Question 的指定范围内。
    AnswererNotAllowed {
        expected: Vec<String>,
        actual: String,
    },
    /// 没有该种转换的授权依据。
    NotAuthorized {
        subject: String,
        kind: ConversionKind,
    },
    /// 转换请求的修订与评论关联修订不一致。
    RevisionMismatch {
        comment: String,
        requested: String,
    },
    /// 转换请求的对象与评论关联对象不一致。
    ObjectMismatch {
        comment: String,
        requested: String,
    },
}

impl std::fmt::Display for ClarityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "字段 {field} 不能为空"),
            Self::EmptyBody => write!(f, "正文不能为空"),
            Self::IndefiniteRevision { field, value } => {
                write!(f, "{field} 不是确定修订：{value}")
            }
            Self::CommentNotFound(id) => write!(f, "评论不存在：{id}"),
            Self::QuestionNotFound(id) => write!(f, "Question 不存在：{id}"),
            Self::QuestionClosed(id) => write!(f, "Question 已关闭：{id}"),
            Self::AnswererNotAllowed { expected, actual } => {
                write!(f, "答复主体 {actual} 不在指定范围（{:?}）内", expected)
            }
            Self::NotAuthorized { subject, kind } => {
                write!(f, "{subject} 没有 {} 的转换权限", kind.as_str())
            }
            Self::RevisionMismatch { comment, requested } => {
                write!(f, "评论关联修订 {comment} 与请求 {requested} 不一致")
            }
            Self::ObjectMismatch { comment, requested } => {
                write!(f, "评论关联对象 {comment} 与请求 {requested} 不一致")
            }
        }
    }
}

impl std::error::Error for ClarityError {}

fn require_text(field: &'static str, value: &str) -> Result<(), ClarityError> {
    if value.trim().is_empty() {
        return Err(ClarityError::EmptyField(field));
    }
    Ok(())
}

fn require_definite(field: &'static str, value: &str) -> Result<(), ClarityError> {
    if !is_definite_revision(value) {
        return Err(ClarityError::IndefiniteRevision {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn short_id(prefix: &str, material: &str) -> String {
    format!("{prefix}-{}", &sha256_hex(material.as_bytes())[..8])
}

// ---------------------------------------------------------------------------
// 讨论记录（任务 3.1）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSource {
    /// 宿主工作台。
    Host,
    /// IM 回调。
    Im,
    /// 网页控制台。
    Web,
    /// 外部平台 API。
    ExternalApi,
    /// 执行端观察/输出。
    AgentOutput,
}

impl CommentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Im => "im",
            Self::Web => "web",
            Self::ExternalApi => "external_api",
            Self::AgentOutput => "agent_output",
        }
    }
}

/// 评论关联的对象与**确定修订**；编辑评论不会改变它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentTarget {
    object_id: String,
    revision: String,
}

impl CommentTarget {
    pub fn new(
        object_id: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<Self, ClarityError> {
        let object_id = object_id.into();
        let revision = revision.into();
        require_text("target.object_id", &object_id)?;
        require_definite("target.revision", &revision)?;
        Ok(Self {
            object_id: object_id.trim().to_string(),
            revision: revision.trim().to_string(),
        })
    }

    pub fn object_id(&self) -> &str {
        &self.object_id
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// 被覆盖的旧版本：保留原正文与编辑时间，不重写历史。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentRevision {
    pub body: String,
    pub edited_by: String,
    pub edited_at: String,
}

/// 审计条目：只有元数据与正文摘要，不含正文——正文与审计分开存放。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAudit {
    pub comment_id: String,
    pub issue_id: String,
    pub author: String,
    pub created_at: String,
    pub target_object: String,
    pub target_revision: String,
    pub source: CommentSource,
    pub body_digest: String,
    pub revisions: usize,
}

impl CommentAudit {
    pub fn line(&self) -> String {
        format!(
            "comment {} issue {} author {} at {} target {}@{} source {} digest {} revisions {}",
            self.comment_id,
            self.issue_id,
            self.author,
            self.created_at,
            self.target_object,
            self.target_revision,
            self.source.as_str(),
            self.body_digest,
            self.revisions
        )
    }
}

/// 一条讨论评论。正文可变，但每次编辑都把旧正文压进 [`Comment::history`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    id: String,
    issue_id: String,
    author: String,
    created_at: String,
    body: String,
    source: CommentSource,
    target: CommentTarget,
    history: Vec<CommentRevision>,
}

impl Comment {
    pub fn new(
        issue_id: impl Into<String>,
        author: impl Into<String>,
        created_at: impl Into<String>,
        body: impl Into<String>,
        source: CommentSource,
        target: CommentTarget,
    ) -> Result<Self, ClarityError> {
        let issue_id = issue_id.into();
        let author = author.into();
        let created_at = created_at.into();
        let body = body.into();
        require_text("issue_id", &issue_id)?;
        require_text("author", &author)?;
        require_text("created_at", &created_at)?;
        require_text("body", &body)?;

        let material = format!(
            "{}|{}|{}|{}|{}",
            issue_id.trim(),
            author.trim(),
            created_at.trim(),
            body.trim(),
            target.revision()
        );
        Ok(Self {
            id: short_id("comment", &material),
            issue_id: issue_id.trim().to_string(),
            author: author.trim().to_string(),
            created_at: created_at.trim().to_string(),
            body: body.trim().to_string(),
            source,
            target,
            history: Vec::new(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn issue_id(&self) -> &str {
        &self.issue_id
    }

    pub fn author(&self) -> &str {
        &self.author
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn source(&self) -> CommentSource {
        self.source
    }

    pub fn target(&self) -> &CommentTarget {
        &self.target
    }

    pub fn history(&self) -> &[CommentRevision] {
        &self.history
    }

    /// 编辑：旧正文进历史，关联对象与修订保持不变。
    pub fn edit(
        &mut self,
        editor: impl Into<String>,
        edited_at: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<(), ClarityError> {
        let editor = editor.into();
        let edited_at = edited_at.into();
        let body = body.into();
        require_text("editor", &editor)?;
        require_text("edited_at", &edited_at)?;
        require_text("body", &body)?;
        if body.trim() == self.body {
            return Ok(());
        }
        self.history.push(CommentRevision {
            body: std::mem::replace(&mut self.body, body.trim().to_string()),
            edited_by: editor.trim().to_string(),
            edited_at: edited_at.trim().to_string(),
        });
        Ok(())
    }

    pub fn audit(&self) -> CommentAudit {
        CommentAudit {
            comment_id: self.id.clone(),
            issue_id: self.issue_id.clone(),
            author: self.author.clone(),
            created_at: self.created_at.clone(),
            target_object: self.target.object_id().to_string(),
            target_revision: self.target.revision().to_string(),
            source: self.source,
            body_digest: format!("sha256:{}", sha256_hex(self.body.as_bytes())),
            revisions: self.history.len(),
        }
    }

    pub fn audit_line(&self) -> String {
        self.audit().line()
    }
}

// ---------------------------------------------------------------------------
// 评论信号分类（只用于展示，不参与授权）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSignal {
    Plain,
    /// 看起来像批准/同意。**仅用于提示**：不结单、不授权、不派发。
    ApprovalLike,
    Question,
}

/// 关键词分类。刻意放在展示侧：任何调用方都不得把它当授权依据。
pub fn classify_comment(text: &str) -> CommentSignal {
    let lower = text.to_lowercase();
    let approval_like = [
        "lgtm", "approve", "approved", "approval", "ship it", "+1", "同意", "批准", "认可",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword));
    if approval_like {
        return CommentSignal::ApprovalLike;
    }
    if text.contains('?') || text.contains('？') {
        return CommentSignal::Question;
    }
    CommentSignal::Plain
}

// ---------------------------------------------------------------------------
// Question / NeedsInput（任务 3.2）
// ---------------------------------------------------------------------------

/// 阻塞范围：只挡依赖项，还是整个 attempt。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedScope {
    pub items: BTreeSet<String>,
    pub all_attempt: bool,
}

impl BlockedScope {
    pub fn of<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            items: items.into_iter().map(Into::into).collect(),
            all_attempt: false,
        }
    }

    pub fn whole_attempt() -> Self {
        Self {
            items: BTreeSet::new(),
            all_attempt: true,
        }
    }

    /// 无关的授权工作不受阻塞（规格场景：暂停依赖项但其余工作可继续）。
    pub fn is_blocked(&self, item: &str) -> bool {
        self.all_attempt || self.items.contains(item)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeTrigger {
    /// 指定答复人给出答复。
    AnswerReceived,
    /// 关联决策被记录。
    DecisionRecorded,
    /// 人工恢复。
    ManualResume,
}

impl WakeTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnswerReceived => "answer_received",
            Self::DecisionRecorded => "decision_recorded",
            Self::ManualResume => "manual_resume",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeCondition {
    pub trigger: WakeTrigger,
    pub note: String,
}

impl WakeCondition {
    pub fn new(trigger: WakeTrigger, note: impl Into<String>) -> Self {
        Self {
            trigger,
            note: note.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarificationStatus {
    Pending,
    Answered { at: String, by: String },
    Cancelled { at: String, by: String },
}

impl ClarificationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered { .. } => "answered",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

/// 绑定 attempt 与 input 修订的 Question。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarificationQuestion {
    pub id: String,
    pub issue_id: String,
    pub attempt_id: String,
    pub input_revision: String,
    pub asked_by: String,
    pub text: String,
    pub created_at: String,
    pub answerers: BTreeSet<String>,
    pub blocked: BlockedScope,
    pub wake: WakeCondition,
    pub status: ClarificationStatus,
    pub answer: Option<String>,
    pub timeout_notices: u32,
    pub last_timeout_at: Option<String>,
}

impl ClarificationQuestion {
    pub fn new(
        issue_id: impl Into<String>,
        attempt_id: impl Into<String>,
        input_revision: impl Into<String>,
        asked_by: impl Into<String>,
        text: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Result<Self, ClarityError> {
        let issue_id = issue_id.into();
        let attempt_id = attempt_id.into();
        let input_revision = input_revision.into();
        let asked_by = asked_by.into();
        let text = text.into();
        let created_at = created_at.into();
        require_text("issue_id", &issue_id)?;
        require_text("attempt_id", &attempt_id)?;
        require_definite("input_revision", &input_revision)?;
        require_text("asked_by", &asked_by)?;
        require_text("text", &text)?;
        require_text("created_at", &created_at)?;

        let material = format!(
            "{}|{}|{}|{}|{}",
            attempt_id.trim(),
            input_revision.trim(),
            asked_by.trim(),
            text.trim(),
            created_at.trim()
        );
        Ok(Self {
            id: short_id("question", &material),
            issue_id: issue_id.trim().to_string(),
            attempt_id: attempt_id.trim().to_string(),
            input_revision: input_revision.trim().to_string(),
            asked_by: asked_by.trim().to_string(),
            text: text.trim().to_string(),
            created_at: created_at.trim().to_string(),
            answerers: BTreeSet::new(),
            blocked: BlockedScope::whole_attempt(),
            wake: WakeCondition::new(WakeTrigger::AnswerReceived, "指定答复人答复后唤醒"),
            status: ClarificationStatus::Pending,
            answer: None,
            timeout_notices: 0,
            last_timeout_at: None,
        })
    }

    pub fn with_answerers<I, S>(mut self, answerers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.answerers = answerers.into_iter().map(Into::into).collect();
        self
    }

    pub fn blocking(mut self, blocked: BlockedScope) -> Self {
        self.blocked = blocked;
        self
    }

    pub fn wake_when(mut self, wake: WakeCondition) -> Self {
        self.wake = wake;
        self
    }

    /// 答复主体必须是指定集合之一；空集合表示不限制（仅用于无需权限的内部问询）。
    pub fn accepts_answer_from(&self, subject: &str) -> bool {
        self.answerers.is_empty() || self.answerers.contains(subject.trim())
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.status, ClarificationStatus::Pending)
    }
}

/// 界面需要的 NeedsInput 视图：谁答复、挡住什么、何时唤醒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsInputView {
    pub question_id: String,
    pub attempt_id: String,
    pub answerers: Vec<String>,
    pub blocked: BlockedScope,
    pub wake: WakeCondition,
    pub since: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerOutcome {
    pub status: ClarificationStatus,
    pub commands: Vec<ControlCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeoutOutcome {
    /// 超时不改状态：仍是 Pending，等人决定。
    pub status: ClarificationStatus,
    pub notices: u32,
    /// 超时不产生任何命令（不派发、不猜测、不放行）。
    pub commands: Vec<ControlCommand>,
}

// ---------------------------------------------------------------------------
// 显式转换（任务 3.3）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionKind {
    Decision,
    ScopeDecision,
    FeedbackProposal,
}

impl ConversionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::ScopeDecision => "scope_decision",
            Self::FeedbackProposal => "feedback_proposal",
        }
    }
}

/// 授权依据：谁在什么规则修订下授予了该转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionGrant {
    pub kind: ConversionKind,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionSet {
    pub subject_id: String,
    grants: Vec<ConversionGrant>,
}

impl PermissionSet {
    pub fn new(subject_id: impl Into<String>) -> Self {
        Self {
            subject_id: subject_id.into(),
            grants: Vec::new(),
        }
    }

    pub fn grant(mut self, kind: ConversionKind, basis: impl Into<String>) -> Self {
        self.grants.push(ConversionGrant {
            kind,
            basis: basis.into(),
        });
        self
    }

    pub fn find(&self, kind: ConversionKind) -> Option<&ConversionGrant> {
        self.grants.iter().find(|grant| grant.kind == kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionRequest {
    pub kind: ConversionKind,
    pub object_id: String,
    pub base_revision: String,
    pub at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionStatus {
    /// 只是提案；接受/发布仍由对应流程决定。
    Proposed,
}

impl ConversionStatus {
    pub fn as_str(self) -> &'static str {
        "proposed"
    }
}

/// 转换结果：绑定对象、修订、来源评论与授权依据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertedDecision {
    pub id: String,
    pub kind: ConversionKind,
    pub object_id: String,
    pub base_revision: String,
    pub source_comment_id: String,
    pub decided_by: String,
    pub grant_basis: String,
    pub created_at: String,
    pub status: ConversionStatus,
}

// ---------------------------------------------------------------------------
// 外部回调（任务 3.3 第二半）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalChannel {
    Im,
    Web,
    Platform,
    Email,
}

impl ExternalChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Im => "im",
            Self::Web => "web",
            Self::Platform => "platform",
            Self::Email => "email",
        }
    }

    fn comment_source(self) -> CommentSource {
        match self {
            Self::Im => CommentSource::Im,
            Self::Web => CommentSource::Web,
            Self::Platform => CommentSource::ExternalApi,
            Self::Email => CommentSource::ExternalApi,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackRejection {
    /// 已认证但对该 Issue 没有发言权限。
    NotAuthorized,
    /// 已认证但缺少主体标识。
    MissingSubject,
    /// 回调指向的对象不是本 Issue。
    ObjectMismatch,
    /// 回调没有绑定确定修订。
    IndefiniteRevision,
}

impl CallbackRejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotAuthorized => "not_authorized",
            Self::MissingSubject => "missing_subject",
            Self::ObjectMismatch => "object_mismatch",
            Self::IndefiniteRevision => "indefinite_revision",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCallback {
    pub channel: ExternalChannel,
    pub authenticated: bool,
    pub subject_id: Option<String>,
    pub text: String,
    pub object_id: String,
    pub base_revision: String,
    /// 文本自称"已批准"。只是声明，不产生任何效果。
    pub claims_approval: bool,
    pub received_at: String,
}

/// 不可信候选材料：登记但不进权威讨论，也不能被转换成决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedMaterial {
    pub id: String,
    pub channel: ExternalChannel,
    pub subject_id: Option<String>,
    pub text: String,
    pub object_id: String,
    pub base_revision: String,
    pub claims_approval: bool,
    pub trust: Trust,
    pub reason: String,
    pub received_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome {
    /// 认证且授权通过：只成为普通评论，不自动转决策。
    Commented {
        comment_id: String,
        signal: CommentSignal,
    },
    /// 无法验证身份：登记为不可信材料。
    Unverified {
        material_id: String,
    },
    Rejected {
        reason: CallbackRejection,
    },
}

// ---------------------------------------------------------------------------
// 控制命令映射
// ---------------------------------------------------------------------------

/// 讨论侧能够请求的控制动作。**执行在 4.x 控制面**，本模块只描述意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    BeginAcceptanceFlow {
        decision_id: String,
        object_id: String,
        base_revision: String,
    },
    BeginFeedbackReview {
        proposal_id: String,
        object_id: String,
        base_revision: String,
    },
    WakeAttempt {
        question_id: String,
        attempt_id: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum DiscussionEvent<'a> {
    Comment(&'a Comment),
    Answer {
        question: &'a ClarificationQuestion,
        answerer: &'a str,
    },
    Converted(&'a ConvertedDecision),
    UnverifiedMaterial(&'a UnverifiedMaterial),
}

/// 唯一的事件 → 命令映射。评论与不可信材料恒为空；转换只"开启流程"，不是接受。
pub fn commands_for(event: DiscussionEvent<'_>) -> Vec<ControlCommand> {
    match event {
        DiscussionEvent::Comment(_) | DiscussionEvent::UnverifiedMaterial(_) => Vec::new(),
        DiscussionEvent::Answer { question, answerer } => {
            if question.answer.is_some() && question.accepts_answer_from(answerer) {
                vec![ControlCommand::WakeAttempt {
                    question_id: question.id.clone(),
                    attempt_id: question.attempt_id.clone(),
                }]
            } else {
                Vec::new()
            }
        }
        DiscussionEvent::Converted(record) => match record.kind {
            ConversionKind::Decision | ConversionKind::ScopeDecision => {
                vec![ControlCommand::BeginAcceptanceFlow {
                    decision_id: record.id.clone(),
                    object_id: record.object_id.clone(),
                    base_revision: record.base_revision.clone(),
                }]
            }
            ConversionKind::FeedbackProposal => vec![ControlCommand::BeginFeedbackReview {
                proposal_id: record.id.clone(),
                object_id: record.object_id.clone(),
                base_revision: record.base_revision.clone(),
            }],
        },
    }
}

// ---------------------------------------------------------------------------
// Issue 讨论聚合（纯内存）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentReceipt {
    pub comment_id: String,
    pub signal: CommentSignal,
    /// 评论产生的控制命令：恒为空，留作回归断言的锚点。
    pub commands: Vec<ControlCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDiscussion {
    issue_id: String,
    /// 权威验收修订。讨论侧只读，任何评论/回答/回调都不改它。
    acceptance_revision: String,
    comments: Vec<Comment>,
    questions: Vec<ClarificationQuestion>,
    converted: Vec<ConvertedDecision>,
    unverified: Vec<UnverifiedMaterial>,
}

impl IssueDiscussion {
    pub fn new(issue_id: impl Into<String>, acceptance_revision: impl Into<String>) -> Self {
        Self {
            issue_id: issue_id.into(),
            acceptance_revision: acceptance_revision.into(),
            comments: Vec::new(),
            questions: Vec::new(),
            converted: Vec::new(),
            unverified: Vec::new(),
        }
    }

    pub fn issue_id(&self) -> &str {
        &self.issue_id
    }

    pub fn acceptance_revision(&self) -> &str {
        &self.acceptance_revision
    }

    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    pub fn comment(&self, comment_id: &str) -> Option<&Comment> {
        self.comments.iter().find(|item| item.id() == comment_id)
    }

    pub fn questions(&self) -> &[ClarificationQuestion] {
        &self.questions
    }

    pub fn converted(&self) -> &[ConvertedDecision] {
        &self.converted
    }

    pub fn unverified_materials(&self) -> &[UnverifiedMaterial] {
        &self.unverified
    }

    /// 发布评论。返回的命令恒为空——这条路径没有控制面。
    pub fn post(&mut self, comment: Comment) -> CommentReceipt {
        let signal = classify_comment(comment.body());
        let receipt = CommentReceipt {
            comment_id: comment.id().to_string(),
            signal,
            commands: commands_for(DiscussionEvent::Comment(&comment)),
        };
        self.comments.push(comment);
        receipt
    }

    pub fn edit_comment(
        &mut self,
        comment_id: &str,
        editor: impl Into<String>,
        edited_at: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<(), ClarityError> {
        let comment = self
            .comments
            .iter_mut()
            .find(|item| item.id() == comment_id)
            .ok_or_else(|| ClarityError::CommentNotFound(comment_id.to_string()))?;
        comment.edit(editor, edited_at, body)
    }

    pub fn raise_question(&mut self, question: ClarificationQuestion) {
        self.questions.push(question);
    }

    /// 当前 NeedsInput（每个 attempt 取最早的待答复 Question）。
    pub fn needs_input(&self, attempt_id: &str) -> Option<NeedsInputView> {
        let question = self
            .questions
            .iter()
            .find(|item| item.attempt_id == attempt_id && item.is_pending())?;
        Some(NeedsInputView {
            question_id: question.id.clone(),
            attempt_id: question.attempt_id.clone(),
            answerers: question.answerers.iter().cloned().collect(),
            blocked: question.blocked.clone(),
            wake: question.wake.clone(),
            since: question.created_at.clone(),
        })
    }

    /// 某工作项是否因该 attempt 的待答 Question 被阻塞。
    pub fn is_blocked(&self, attempt_id: &str, work_item: &str) -> bool {
        self.questions.iter().any(|item| {
            item.attempt_id == attempt_id && item.is_pending() && item.blocked.is_blocked(work_item)
        })
    }

    pub fn answer(
        &mut self,
        question_id: &str,
        answerer: &str,
        at: &str,
        text: &str,
    ) -> Result<AnswerOutcome, ClarityError> {
        require_text("answer", text)?;
        require_text("answered_at", at)?;
        let index = self
            .questions
            .iter()
            .position(|item| item.id == question_id)
            .ok_or_else(|| ClarityError::QuestionNotFound(question_id.to_string()))?;
        if !self.questions[index].is_pending() {
            return Err(ClarityError::QuestionClosed(question_id.to_string()));
        }
        if !self.questions[index].accepts_answer_from(answerer) {
            return Err(ClarityError::AnswererNotAllowed {
                expected: self.questions[index].answerers.iter().cloned().collect(),
                actual: answerer.to_string(),
            });
        }
        {
            let question = &mut self.questions[index];
            question.answer = Some(text.trim().to_string());
            question.status = ClarificationStatus::Answered {
                at: at.trim().to_string(),
                by: answerer.trim().to_string(),
            };
        }
        let question = self.questions[index].clone();
        let commands = commands_for(DiscussionEvent::Answer {
            question: &question,
            answerer,
        });
        Ok(AnswerOutcome {
            status: question.status,
            commands,
        })
    }

    /// 超时：只记提示次数，状态保持 Pending，不产生命令。
    pub fn timeout(&mut self, question_id: &str, at: &str) -> Result<TimeoutOutcome, ClarityError> {
        require_text("timeout_at", at)?;
        let question = self
            .questions
            .iter_mut()
            .find(|item| item.id == question_id)
            .ok_or_else(|| ClarityError::QuestionNotFound(question_id.to_string()))?;
        if !question.is_pending() {
            return Err(ClarityError::QuestionClosed(question_id.to_string()));
        }
        question.timeout_notices += 1;
        question.last_timeout_at = Some(at.trim().to_string());
        Ok(TimeoutOutcome {
            status: question.status.clone(),
            notices: question.timeout_notices,
            commands: Vec::new(),
        })
    }

    /// 人工撤销：只在 Pending 时可撤销，不产生命令（撤销不是答复，也不是放行）。
    pub fn cancel(
        &mut self,
        question_id: &str,
        at: &str,
        by: &str,
    ) -> Result<ClarificationStatus, ClarityError> {
        require_text("cancelled_at", at)?;
        require_text("cancelled_by", by)?;
        let question = self
            .questions
            .iter_mut()
            .find(|item| item.id == question_id)
            .ok_or_else(|| ClarityError::QuestionNotFound(question_id.to_string()))?;
        if !question.is_pending() {
            return Err(ClarityError::QuestionClosed(question_id.to_string()));
        }
        question.status = ClarificationStatus::Cancelled {
            at: at.trim().to_string(),
            by: by.trim().to_string(),
        };
        Ok(question.status.clone())
    }

    /// 显式转换：要求调用方持有该种转换的授权，且对象/修订与评论一致。
    pub fn convert(
        &mut self,
        comment_id: &str,
        request: &ConversionRequest,
        permissions: &PermissionSet,
    ) -> Result<ConvertedDecision, ClarityError> {
        require_definite("base_revision", &request.base_revision)?;
        require_text("object_id", &request.object_id)?;
        require_text("at", &request.at)?;
        let comment = self
            .comments
            .iter()
            .find(|item| item.id() == comment_id)
            .ok_or_else(|| ClarityError::CommentNotFound(comment_id.to_string()))?;
        if comment.target().object_id() != request.object_id.trim() {
            return Err(ClarityError::ObjectMismatch {
                comment: comment.target().object_id().to_string(),
                requested: request.object_id.clone(),
            });
        }
        if comment.target().revision() != request.base_revision.trim() {
            return Err(ClarityError::RevisionMismatch {
                comment: comment.target().revision().to_string(),
                requested: request.base_revision.clone(),
            });
        }
        let grant = permissions
            .find(request.kind)
            .ok_or_else(|| ClarityError::NotAuthorized {
                subject: permissions.subject_id.clone(),
                kind: request.kind,
            })?;

        let material = format!(
            "{}|{}|{}|{}|{}|{}",
            request.kind.as_str(),
            request.object_id.trim(),
            request.base_revision.trim(),
            comment_id,
            permissions.subject_id.trim(),
            request.at.trim()
        );
        let record = ConvertedDecision {
            id: short_id("decision", &material),
            kind: request.kind,
            object_id: request.object_id.trim().to_string(),
            base_revision: request.base_revision.trim().to_string(),
            source_comment_id: comment_id.to_string(),
            decided_by: permissions.subject_id.clone(),
            grant_basis: grant.basis.clone(),
            created_at: request.at.trim().to_string(),
            status: ConversionStatus::Proposed,
        };
        self.converted.push(record.clone());
        Ok(record)
    }

    /// 外部回调入口：先认证，再授权，最后才可能成为评论。
    pub fn ingest_callback(
        &mut self,
        callback: &ExternalCallback,
        authorized: bool,
    ) -> CallbackOutcome {
        if !callback.authenticated {
            let material = UnverifiedMaterial {
                id: short_id(
                    "material",
                    &format!(
                        "{}|{}|{}|{}",
                        callback.channel.as_str(),
                        callback.object_id,
                        callback.text,
                        callback.received_at
                    ),
                ),
                channel: callback.channel,
                subject_id: callback.subject_id.clone(),
                text: callback.text.clone(),
                object_id: callback.object_id.clone(),
                base_revision: callback.base_revision.clone(),
                claims_approval: callback.claims_approval,
                trust: Trust::Untrusted,
                reason: "unauthenticated".to_string(),
                received_at: callback.received_at.clone(),
            };
            let material_id = material.id.clone();
            self.unverified.push(material);
            return CallbackOutcome::Unverified { material_id };
        }

        let Some(subject) = callback
            .subject_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return CallbackOutcome::Rejected {
                reason: CallbackRejection::MissingSubject,
            };
        };
        if callback.object_id != self.issue_id {
            return CallbackOutcome::Rejected {
                reason: CallbackRejection::ObjectMismatch,
            };
        }
        if !is_definite_revision(&callback.base_revision) {
            return CallbackOutcome::Rejected {
                reason: CallbackRejection::IndefiniteRevision,
            };
        }
        if !authorized {
            return CallbackOutcome::Rejected {
                reason: CallbackRejection::NotAuthorized,
            };
        }

        let target =
            match CommentTarget::new(callback.object_id.clone(), callback.base_revision.clone()) {
                Ok(target) => target,
                Err(_) => {
                    return CallbackOutcome::Rejected {
                        reason: CallbackRejection::IndefiniteRevision,
                    };
                }
            };
        let Ok(comment) = Comment::new(
            self.issue_id.clone(),
            subject,
            callback.received_at.clone(),
            callback.text.clone(),
            callback.channel.comment_source(),
            target,
        ) else {
            return CallbackOutcome::Rejected {
                reason: CallbackRejection::MissingSubject,
            };
        };
        let receipt = self.post(comment);
        CallbackOutcome::Commented {
            comment_id: receipt.comment_id,
            signal: receipt.signal,
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "2026-11-20T09:00:00Z";

    fn target() -> CommentTarget {
        CommentTarget::new("issue-1", "spec-r7").expect("目标必须合法")
    }

    fn comment(author: &str, body: &str) -> Comment {
        Comment::new("issue-1", author, AT, body, CommentSource::Host, target())
            .expect("评论必须合法")
    }

    fn question() -> ClarificationQuestion {
        ClarificationQuestion::new(
            "issue-1",
            "attempt-1",
            "spec-r7",
            "agent-1",
            "验收是否包含迁移回滚？",
            "2026-11-20T09:30:00Z",
        )
        .expect("Question 必须合法")
        .with_answerers(["product-owner"])
        .blocking(BlockedScope::of(["task-migrate"]))
        .wake_when(WakeCondition::new(
            WakeTrigger::AnswerReceived,
            "product-owner 答复后唤醒 attempt-1",
        ))
    }

    fn owned_permissions() -> PermissionSet {
        PermissionSet::new("alice")
            .grant(ConversionKind::Decision, "project-owner@rev3")
            .grant(ConversionKind::ScopeDecision, "project-owner@rev3")
            .grant(ConversionKind::FeedbackProposal, "project-owner@rev3")
    }

    #[test]
    fn comment_edit_keeps_original_body_and_revision() {
        let mut discussion = IssueDiscussion::new("issue-1", "spec-r7");
        let receipt = discussion.post(comment("alice", "初版：范围 A"));
        assert!(receipt.commands.is_empty(), "发布评论不得产生命令");
        assert_eq!(receipt.signal, CommentSignal::Plain);

        discussion
            .edit_comment(
                &receipt.comment_id,
                "alice",
                "2026-11-20T09:10:00Z",
                "修订：范围 A+B",
            )
            .expect("编辑应成功");
        discussion
            .edit_comment(
                &receipt.comment_id,
                "bob",
                "2026-11-20T09:20:00Z",
                "再修订：仅 A",
            )
            .expect("编辑应成功");

        let stored = discussion.comment(&receipt.comment_id).expect("评论存在");
        assert_eq!(stored.body(), "再修订：仅 A");
        assert_eq!(
            stored.target().revision(),
            "spec-r7",
            "关联修订不得随编辑改变"
        );
        assert_eq!(stored.target().object_id(), "issue-1");
        assert_eq!(stored.history().len(), 2);
        assert_eq!(stored.history()[0].body, "初版：范围 A");
        assert_eq!(stored.history()[0].edited_by, "alice");
        assert_eq!(stored.history()[1].body, "修订：范围 A+B");
        assert_eq!(stored.history()[1].edited_by, "bob");
        assert_eq!(stored.history()[1].edited_at, "2026-11-20T09:20:00Z");

        // 审计与正文分开：审计行含摘要与元数据，不含正文。
        let audit = stored.audit_line();
        assert!(audit.contains("spec-r7"));
        assert!(audit.contains("digest sha256:"));
        assert!(!audit.contains("再修订"), "审计行不得包含正文");
        assert!(!audit.contains("初版"), "审计行不得包含历史正文");
    }

    #[test]
    fn invariant_i_38_comment_never_creates_control_effect() {
        let mut discussion = IssueDiscussion::new("issue-1", "spec-r7");
        let acceptance_before = discussion.acceptance_revision().to_string();

        for text in ["LGTM", "同意", "approved, ship it", "+1", "批准"] {
            let receipt = discussion.post(comment("alice", text));
            assert_eq!(
                receipt.signal,
                CommentSignal::ApprovalLike,
                "{text} 应被识别为批准类文本（仅用于展示）"
            );
            assert!(receipt.commands.is_empty(), "{text} 不得产生控制命令");
        }

        assert!(discussion.converted().is_empty(), "评论不得自动生成决策");
        assert!(discussion.unverified_materials().is_empty());
        assert!(discussion.needs_input("attempt-1").is_none());
        assert_eq!(
            discussion.acceptance_revision(),
            acceptance_before,
            "验收修订不得被评论改变"
        );
    }

    #[test]
    fn question_sets_needs_input_and_timeout_stays_pending() {
        let mut discussion = IssueDiscussion::new("issue-1", "spec-r7");
        let question = question();
        let question_id = question.id.clone();
        discussion.raise_question(question);

        let view = discussion
            .needs_input("attempt-1")
            .expect("应进入 NeedsInput");
        assert_eq!(view.question_id, question_id);
        assert_eq!(view.answerers, vec!["product-owner".to_string()]);
        assert_eq!(view.wake.trigger, WakeTrigger::AnswerReceived);
        assert!(view.wake.note.contains("attempt-1"));
        assert!(view.blocked.is_blocked("task-migrate"));
        assert!(
            !view.blocked.is_blocked("task-unrelated"),
            "无关授权工作不应被阻塞"
        );
        assert!(discussion.is_blocked("attempt-1", "task-migrate"));
        assert!(!discussion.is_blocked("attempt-1", "task-unrelated"));

        // 超时：仍待决定，不产生命令，不自动猜测。
        let timeout = discussion
            .timeout(&question_id, "2026-11-20T10:30:00Z")
            .expect("超时应被记录");
        assert_eq!(timeout.status, ClarificationStatus::Pending);
        assert!(timeout.commands.is_empty());
        assert_eq!(timeout.notices, 1);
        assert!(discussion.needs_input("attempt-1").is_some());

        // 非指定答复人被拒，状态不变。
        let error = discussion
            .answer(&question_id, "random-dev", "2026-11-20T10:40:00Z", "放宽吧")
            .expect_err("非指定答复人必须被拒");
        assert!(matches!(error, ClarityError::AnswererNotAllowed { .. }));
        assert!(discussion.needs_input("attempt-1").is_some());

        // 指定答复人答复：只产生唤醒命令，不改验收。
        let outcome = discussion
            .answer(
                &question_id,
                "product-owner",
                "2026-11-20T11:00:00Z",
                "验收包含回滚验证",
            )
            .expect("指定答复人应可答复");
        assert!(matches!(
            outcome.commands.as_slice(),
            [ControlCommand::WakeAttempt { .. }]
        ));
        assert!(discussion.needs_input("attempt-1").is_none());
        assert_eq!(discussion.acceptance_revision(), "spec-r7");

        // 已答复的 Question 不能再次答复。
        assert!(matches!(
            discussion
                .answer(
                    &question_id,
                    "product-owner",
                    "2026-11-20T11:10:00Z",
                    "再答"
                )
                .expect_err("已回答必须拒绝"),
            ClarityError::QuestionClosed(_)
        ));
    }

    #[test]
    fn cancelled_question_leaves_needs_input_without_commands() {
        let mut discussion = IssueDiscussion::new("issue-1", "spec-r7");
        let question = question();
        let question_id = question.id.clone();
        discussion.raise_question(question);
        assert!(discussion.needs_input("attempt-1").is_some());

        let status = discussion
            .cancel(&question_id, "2026-11-20T10:20:00Z", "product-owner")
            .expect("Pending 可撤销");
        assert_eq!(
            status,
            ClarificationStatus::Cancelled {
                at: "2026-11-20T10:20:00Z".to_string(),
                by: "product-owner".to_string(),
            }
        );
        assert!(discussion.needs_input("attempt-1").is_none());
        assert!(!discussion.is_blocked("attempt-1", "task-migrate"));

        // 撤销不是答复：不能再用答复唤醒，也不能重复撤销。
        assert!(matches!(
            discussion.answer(
                &question_id,
                "product-owner",
                "2026-11-20T10:25:00Z",
                "算了"
            ),
            Err(ClarityError::QuestionClosed(_))
        ));
        assert!(matches!(
            discussion.cancel(&question_id, "2026-11-20T10:30:00Z", "product-owner"),
            Err(ClarityError::QuestionClosed(_))
        ));
        assert_eq!(discussion.acceptance_revision(), "spec-r7");
    }

    #[test]
    fn explicit_conversion_binds_object_revision_and_grant() {
        let mut discussion = IssueDiscussion::new("issue-1", "spec-r7");
        let receipt = discussion.post(comment("alice", "接口改为 v2，多一个字段"));
        let permissions = owned_permissions();

        for kind in [
            ConversionKind::Decision,
            ConversionKind::ScopeDecision,
            ConversionKind::FeedbackProposal,
        ] {
            let request = ConversionRequest {
                kind,
                object_id: "issue-1".to_string(),
                base_revision: "spec-r7".to_string(),
                at: "2026-11-20T09:40:00Z".to_string(),
            };
            let record = discussion
                .convert(&receipt.comment_id, &request, &permissions)
                .expect("有授权应可转换");
            assert_eq!(record.kind, kind);
            assert_eq!(record.object_id, "issue-1");
            assert_eq!(record.base_revision, "spec-r7");
            assert_eq!(record.source_comment_id, receipt.comment_id);
            assert_eq!(record.decided_by, "alice");
            assert_eq!(record.grant_basis, "project-owner@rev3");
            assert_eq!(record.status, ConversionStatus::Proposed);

            let commands = commands_for(DiscussionEvent::Converted(&record));
            match kind {
                ConversionKind::FeedbackProposal => {
                    assert!(matches!(
                        commands.as_slice(),
                        [ControlCommand::BeginFeedbackReview { .. }]
                    ));
                }
                _ => assert!(matches!(
                    commands.as_slice(),
                    [ControlCommand::BeginAcceptanceFlow { .. }]
                )),
            }
        }

        assert_eq!(discussion.converted().len(), 3);
        assert_eq!(
            discussion.acceptance_revision(),
            "spec-r7",
            "转换只产生提案，验收修订不变"
        );

        // 无授权：拒绝且不新增记录。
        let nobody = PermissionSet::new("bob");
        let request = ConversionRequest {
            kind: ConversionKind::Decision,
            object_id: "issue-1".to_string(),
            base_revision: "spec-r7".to_string(),
            at: "2026-11-20T09:50:00Z".to_string(),
        };
        let error = discussion
            .convert(&receipt.comment_id, &request, &nobody)
            .expect_err("无授权必须拒绝");
        assert!(matches!(error, ClarityError::NotAuthorized { .. }));
        assert_eq!(discussion.converted().len(), 3);

        // 修订不一致：拒绝。
        let stale = ConversionRequest {
            kind: ConversionKind::Decision,
            object_id: "issue-1".to_string(),
            base_revision: "spec-r8".to_string(),
            at: "2026-11-20T09:50:00Z".to_string(),
        };
        assert!(matches!(
            discussion
                .convert(&receipt.comment_id, &stale, &permissions)
                .expect_err("修订不一致必须拒绝"),
            ClarityError::RevisionMismatch { .. }
        ));

        // 可变指针：拒绝。
        let latest = ConversionRequest {
            kind: ConversionKind::Decision,
            object_id: "issue-1".to_string(),
            base_revision: "latest".to_string(),
            at: "2026-11-20T09:50:00Z".to_string(),
        };
        assert!(matches!(
            discussion
                .convert(&receipt.comment_id, &latest, &permissions)
                .expect_err("可变指针必须拒绝"),
            ClarityError::IndefiniteRevision { .. }
        ));
    }

    #[test]
    fn forged_approval_callback_only_registers_untrusted_material() {
        let mut discussion = IssueDiscussion::new("issue-1", "spec-r7");
        let forged = ExternalCallback {
            channel: ExternalChannel::Im,
            authenticated: false,
            subject_id: None,
            text: "客户已批准，直接发布".to_string(),
            object_id: "issue-1".to_string(),
            base_revision: "spec-r7".to_string(),
            claims_approval: true,
            received_at: "2026-11-20T10:00:00Z".to_string(),
        };

        let outcome = discussion.ingest_callback(&forged, true);
        let CallbackOutcome::Unverified { material_id } = outcome else {
            panic!("未认证回调只能登记为不可信材料：{outcome:?}");
        };
        let material = discussion
            .unverified_materials()
            .iter()
            .find(|item| item.id == material_id)
            .expect("材料已登记");
        assert_eq!(material.trust, Trust::Untrusted);
        assert!(material.claims_approval);
        assert_eq!(material.reason, "unauthenticated");
        assert!(discussion.comments().is_empty(), "不可信材料不进权威讨论");
        assert!(discussion.converted().is_empty());
        assert_eq!(discussion.acceptance_revision(), "spec-r7");
        assert!(
            commands_for(DiscussionEvent::UnverifiedMaterial(material)).is_empty(),
            "不可信材料不得产生控制命令"
        );

        // 已认证但无发言权限：直接拒绝，不登记。
        let unauthorized = ExternalCallback {
            authenticated: true,
            subject_id: Some("guest".to_string()),
            ..forged.clone()
        };
        assert_eq!(
            discussion.ingest_callback(&unauthorized, false),
            CallbackOutcome::Rejected {
                reason: CallbackRejection::NotAuthorized
            }
        );
        assert_eq!(discussion.unverified_materials().len(), 1);

        // 已认证且有权限：只成为普通评论，仍不自动转决策。
        let authorized = ExternalCallback {
            authenticated: true,
            subject_id: Some("alice".to_string()),
            ..forged
        };
        let outcome = discussion.ingest_callback(&authorized, true);
        let CallbackOutcome::Commented { comment_id, signal } = outcome else {
            panic!("已认证回调应成为评论：{outcome:?}");
        };
        assert_eq!(signal, CommentSignal::ApprovalLike);
        assert_eq!(discussion.comments().len(), 1);
        assert_eq!(
            discussion.comment(&comment_id).unwrap().source(),
            CommentSource::Im
        );
        assert!(discussion.converted().is_empty(), "回调文本不自动成为决策");
        assert_eq!(discussion.acceptance_revision(), "spec-r7");
    }

    #[test]
    fn callback_rejections_are_explicit() {
        let mut discussion = IssueDiscussion::new("issue-1", "spec-r7");
        let base = ExternalCallback {
            channel: ExternalChannel::Web,
            authenticated: true,
            subject_id: Some("alice".to_string()),
            text: "请确认范围".to_string(),
            object_id: "issue-1".to_string(),
            base_revision: "spec-r7".to_string(),
            claims_approval: false,
            received_at: "2026-11-20T10:05:00Z".to_string(),
        };

        let missing_subject = ExternalCallback {
            subject_id: None,
            ..base.clone()
        };
        assert_eq!(
            discussion.ingest_callback(&missing_subject, true),
            CallbackOutcome::Rejected {
                reason: CallbackRejection::MissingSubject
            }
        );

        let wrong_object = ExternalCallback {
            object_id: "issue-2".to_string(),
            ..base.clone()
        };
        assert_eq!(
            discussion.ingest_callback(&wrong_object, true),
            CallbackOutcome::Rejected {
                reason: CallbackRejection::ObjectMismatch
            }
        );

        let mobile_revision = ExternalCallback {
            base_revision: "HEAD".to_string(),
            ..base
        };
        assert_eq!(
            discussion.ingest_callback(&mobile_revision, true),
            CallbackOutcome::Rejected {
                reason: CallbackRejection::IndefiniteRevision
            }
        );
        assert!(discussion.comments().is_empty());
    }
}
