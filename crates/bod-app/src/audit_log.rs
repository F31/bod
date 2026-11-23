//! 业务审计事件（CR-002 / V2.9.8 §8）。
//!
//! 边界：本模块只定义**事件的形状**与**能否落库**，不负责写库（`store` 负责）
//! 与输出（`telemetry` 负责）。这样同一份判定既能落表又能进日志，不会长出两套字段。
//!
//! 与 `authorization::StageAudit` 的关系：后者是**判定过程的内存产物**，随请求结束消失；
//! 本模块把它变成可持久的事实。`AuditEvent::from_stage` 是阶段判定的唯一转换入口——
//! 两处各自拼字段是审计字段漂移的常见来源，所以只留一条路。
//!
//! 准入之外的持久写入（导入来源、追加关联）不走 `from_stage`：它们没有阶段判定，
//! 走 `AuditEvent::for_write`。两个入口各自挡住错的那一类动作，见 `AuditAction`。
//!
//! 为什么拒绝也要记：§8 要求「拒绝路径应无外部调用或待发意图」，要证明这一点，
//! 必须先能查到「谁在什么时候被拒绝、因为什么」。拒绝不产生业务事实，
//! 靠业务表反推不出来。

use crate::authorization::{StageAction, StageDecision};
use crate::domain::{DomainId, Scope};

/// 审计只区分两种结局。没有第三种：待定不是审计事件，是未完成的判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditDecision {
    Allowed,
    Denied,
}

impl AuditDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allowed" => Some(Self::Allowed),
            "denied" => Some(Self::Denied),
            _ => None,
        }
    }
}

/// 审计的动作词表。
///
/// 与 `authorization::StageAction` **分开**（ADR-0007 第 1 条「不扩 `StageAction`」的延伸）：
/// 阶段动作是**准入**词表，每个变体都必须能回答「需要什么凭据」（`StageRequirement`）；
/// 审计是**事实**词表，它要记的不止准入，还有准入之外的持久写入（导入来源、追加关联）。
/// 合成一个枚举只有两条路可走——给业务写入编一个假的阶段要求（`requirement()` 无法
/// 诚实回答），或者这些动作永远进不了审计。两条都在说谎，所以拆成两个闭集。
///
/// 映射只有一个方向：`From<StageAction>`。审计知道阶段动作，阶段判定不知道审计动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    // —— 阶段准入：与 `StageAction` 一一对应 ——
    Submit,
    ManageScope,
    Dispatch,
    Develop,
    Review,
    Accept,
    Release,
    Rollback,
    Prove,
    Reopen,
    // —— 业务写入：不产生阶段判定，所以没有 `StageRequirement` ——
    /// 导入外部来源：登记来源身份或提交候选，并挂到（或建立）跟踪项。
    ImportSource,
    /// 给跟踪项追加来源或内部工作包关联。
    AppendLink,
    /// 给某个客户目标登记一条观察（部署、回滚、人工补证）。
    ///
    /// 三类观察共用一个动作词：审计要回答的是「谁登记了一条证据」，
    /// 而证据类别已经写在业务行的 `kind` 上——词表里再分三种，
    /// 只是把同一件事说三遍，且新增类别时又要改一次词表。
    RecordObservation,
    /// 撤销某个主体在本公司的全部生效授权。
    ///
    /// 它不是阶段动作：撤权不改变任何交付目标的状态，也不回答「改这件事需要什么凭据」
    /// ——它就是改凭据本身。所以它进审计词表，不进 `StageAction`。
    RevokeAuthorization,
}

impl AuditAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::ManageScope => "manage_scope",
            Self::Dispatch => "dispatch",
            Self::Develop => "develop",
            Self::Review => "review",
            Self::Accept => "accept",
            Self::Release => "release",
            Self::Rollback => "rollback",
            Self::Prove => "prove",
            Self::Reopen => "reopen",
            Self::ImportSource => "import-source",
            Self::AppendLink => "append-link",
            Self::RecordObservation => "record-observation",
            Self::RevokeAuthorization => "revoke-authorization",
        }
    }

    /// 读库用。表外值返回 `None`，由调用方报错——不静默回退（见 `store::audit` 的行映射）。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "submit" => Some(Self::Submit),
            "manage_scope" => Some(Self::ManageScope),
            "dispatch" => Some(Self::Dispatch),
            "develop" => Some(Self::Develop),
            "review" => Some(Self::Review),
            "accept" => Some(Self::Accept),
            "release" => Some(Self::Release),
            "rollback" => Some(Self::Rollback),
            "prove" => Some(Self::Prove),
            "reopen" => Some(Self::Reopen),
            "import-source" => Some(Self::ImportSource),
            "append-link" => Some(Self::AppendLink),
            "record-observation" => Some(Self::RecordObservation),
            "revoke-authorization" => Some(Self::RevokeAuthorization),
            _ => None,
        }
    }

    /// 是否属于阶段准入词表。写库前的闸门用它把两类动作用不同的构造入口分开。
    ///
    /// 判断**不再手写一份名单**，而是问 `StageAction::parse` 认不认这个字符串：
    /// 阶段动作与它的字符串形一一对应（`From<StageAction>` 就是这个契约），
    /// 业务写入的字符串在 `StageAction` 里没有对应物。原先的
    /// `!matches!(self, ImportSource | AppendLink)` 是黑名单——新增一个业务写入却忘了
    /// 加进列表，`for_write` 会当场报 `ActionKindMismatch`；改走单一来源后这类遗忘
    /// 不再存在，而且两份词表无法再各自漂移。
    pub fn is_stage(self) -> bool {
        crate::authorization::StageAction::parse(self.as_str()).is_some()
    }
}

impl From<StageAction> for AuditAction {
    fn from(action: StageAction) -> Self {
        match action {
            StageAction::Submit => Self::Submit,
            StageAction::ManageScope => Self::ManageScope,
            StageAction::Dispatch => Self::Dispatch,
            StageAction::Develop => Self::Develop,
            StageAction::Review => Self::Review,
            StageAction::Accept => Self::Accept,
            StageAction::Release => Self::Release,
            StageAction::Rollback => Self::Rollback,
            StageAction::Prove => Self::Prove,
            StageAction::Reopen => Self::Reopen,
        }
    }
}

/// 一条审计事件。
///
/// 字段与 §8 的六项要求一一对应：谁（`actor`）、做了什么（`action`）、
/// 作用范围（`scope`）、依据（`basis`）、决策（`decision`）与原因（`reason`），
/// 外加贯穿一次动作的关联 ID（`correlation_id`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub company_id: String,
    /// 一次用户动作贯穿的链：入口解析、阶段校验、外部效果、回执共用同一值。
    pub correlation_id: String,
    pub actor: DomainId,
    pub action: AuditAction,
    pub scope: Scope,
    /// 判定的依据标识，形如 `submit:project_membership` / `accept:not_a_member`。
    /// 前缀是动作，便于按动作聚合；后缀是凭据或拒绝码，便于按原因聚合。
    pub basis: String,
    pub decision: AuditDecision,
    /// 拒绝原因（人可读）。放行时必须为 `None`——库层 CHECK 与 `validate` 都拦。
    pub reason: Option<String>,
    /// 业务时间（调用方给的时刻）。与落库时间分开存，恢复重放时才能保留原值。
    pub decided_at: String,
}

/// 拒绝写入的原因。用枚举而非 `String`：调用方要能区分「字段缺失」与「含秘密」，
/// 前者是编程错误，后者是安全问题，处理方式不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditRejection {
    /// 必填字段为空或全空白。
    BlankField(&'static str),
    /// `allowed` 带了原因，或 `denied` 没带原因。
    DecisionReasonMismatch,
    /// 文本里出现凭据形态。审计表可读，写进去等于泄露。
    SecretInText(&'static str),
    /// 动作与构造入口不匹配：阶段动作只能出自 `from_stage`，业务写入只能出自 `for_write`。
    /// 放过去的话，「准入被拒」与「写入成功」会在同一张表里长得一样。
    ActionKindMismatch(&'static str),
}

impl AuditRejection {
    pub fn explain(self) -> String {
        match self {
            Self::BlankField(field) => format!("审计字段 {field} 为空"),
            Self::DecisionReasonMismatch => {
                "审计的决策与原因不一致：放行不得带原因，拒绝必须带原因".to_string()
            }
            Self::SecretInText(field) => {
                format!("审计字段 {field} 含凭据形态，拒绝落库（审计表可读）")
            }
            Self::ActionKindMismatch(why) => format!("审计动作与构造入口不匹配：{why}"),
        }
    }
}

/// 凭据形态的标记。匹配是大小写不敏感的**子串**，宁可误拒也不放行：
/// 误拒的代价是改写一句说明，误放的代价是令牌进了一张长期保留的表。
///
/// 有意不含裸 `token` ——「handshake_token 缺失」这类合法理由会被误伤，
/// 而真正的令牌值前面通常跟着 `=`、`:` 或 `Bearer `。
const SECRET_MARKERS: &[&str] = &[
    "bearer ",
    "authorization:",
    "token=",
    "token:",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "private_key",
];

impl AuditEvent {
    /// 落库前的唯一闸门。`store` 在写之前必须调用它，不允许绕过。
    pub fn validate(&self) -> Result<(), AuditRejection> {
        for (field, value) in [
            ("company_id", Some(self.company_id.as_str())),
            ("correlation_id", Some(self.correlation_id.as_str())),
            ("actor", Some(self.actor.as_str())),
            ("basis", Some(self.basis.as_str())),
            ("decided_at", Some(self.decided_at.as_str())),
        ] {
            match value {
                Some(text) if !text.trim().is_empty() => {}
                _ => return Err(AuditRejection::BlankField(field)),
            }
        }

        match (self.decision, self.reason.as_deref()) {
            (AuditDecision::Allowed, Some(_)) => {
                return Err(AuditRejection::DecisionReasonMismatch);
            }
            (AuditDecision::Denied, None) => {
                return Err(AuditRejection::DecisionReasonMismatch);
            }
            (AuditDecision::Denied, Some(reason)) if reason.trim().is_empty() => {
                return Err(AuditRejection::DecisionReasonMismatch);
            }
            _ => {}
        }

        // `basis` 的前缀恒为 `<action>:`，由 `for_write` / `from_stage` 拼装，是常量；
        // 只有后半段是外部文本。检查整段会**误伤**：`revoke-authorization:` 里就含
        // `authorization:`，而那个标记本来是用来抓 HTTP `Authorization` 头的。
        // 只剥掉「本事件自己的动作名 + 冒号」，剥不掉就整段检查——防的是有人把
        // `authorization: Bearer x` 直接存进 basis 冒充动作前缀。
        let basis_body = self
            .basis
            .strip_prefix(&format!("{}:", self.action.as_str()))
            .unwrap_or(self.basis.as_str());
        for (field, value) in [
            ("correlation_id", Some(self.correlation_id.as_str())),
            ("actor", Some(self.actor.as_str())),
            ("basis", Some(basis_body)),
            ("reason", self.reason.as_deref()),
        ] {
            let Some(text) = value else { continue };
            let lowered = text.to_ascii_lowercase();
            if let Some(marker) = SECRET_MARKERS
                .iter()
                .find(|marker| lowered.contains(**marker))
            {
                let _ = marker;
                return Err(AuditRejection::SecretInText(field));
            }
        }

        Ok(())
    }

    /// 从六阶段判定生成审计事件。`correlation_id` 由调用方给：
    /// 它属于「这次动作」，不属于判定函数——判定函数不知道外部效果那一段。
    pub fn from_stage(
        company_id: impl Into<String>,
        correlation_id: impl Into<String>,
        decision: &StageDecision,
        decided_at: impl Into<String>,
    ) -> Self {
        let audit = decision.audit();
        let (outcome, evidence, reason) = match decision {
            StageDecision::Allowed { basis, .. } => (AuditDecision::Allowed, basis.as_str(), None),
            StageDecision::Denied { reason, .. } => (
                AuditDecision::Denied,
                reason.as_str(),
                Some(audit.basis.clone()),
            ),
        };
        Self {
            company_id: company_id.into(),
            correlation_id: correlation_id.into(),
            actor: audit.actor.clone(),
            action: AuditAction::from(audit.action),
            scope: audit.scope.clone(),
            // 前缀是动作：同一条链里可能有多段判定（提交 → 接受），只按凭据名读会串。
            basis: format!("{}:{}", audit.action.as_str(), evidence),
            decision: outcome,
            reason,
            decided_at: decided_at.into(),
        }
    }

    /// 准入之外的**持久写入**的审计事件（导入来源、追加关联）。
    ///
    /// 与 `from_stage` 分开而不是共用一个函数：这两类事件的决策段语义不同。
    /// 阶段判定的 `allowed` 是「这次动作被允许」；写入的 `allowed` 是「这次写入发生了」——
    /// 因为写不成的路径会连业务行一起回滚，审计也就不会留下，所以能写进表里的写入审计
    /// 必然是既成事实。共用一个构造函数会让这句话在代码里看不出来。
    ///
    /// `evidence` 是「凭什么这么做」的那一段（如 `source_identity` / `work_package`），
    /// 与阶段动作一样拼成 `动作:依据`。
    ///
    /// 拒绝路径（如 `SOURCE_ALREADY_LINKED`）不在本函数负责范围内：拒绝会让事务回滚，
    /// 回滚会带走同一事务里的审计行。这类拒绝要留痕必须另起事务写，属未完成项。
    pub fn for_write(
        company_id: impl Into<String>,
        correlation_id: impl Into<String>,
        actor: DomainId,
        action: AuditAction,
        scope: Scope,
        evidence: &str,
        decided_at: impl Into<String>,
    ) -> Result<Self, AuditRejection> {
        if action.is_stage() {
            return Err(AuditRejection::ActionKindMismatch(
                "阶段动作必须由 from_stage 生成：准入被拒与写入成功不能记成同一件事",
            ));
        }
        if evidence.trim().is_empty() {
            return Err(AuditRejection::BlankField("evidence"));
        }
        Ok(Self {
            company_id: company_id.into(),
            correlation_id: correlation_id.into(),
            actor,
            action,
            scope,
            basis: format!("{}:{evidence}", action.as_str()),
            // 写入既成事实才算数，所以恒为 allowed、恒无原因。
            decision: AuditDecision::Allowed,
            reason: None,
            decided_at: decided_at.into(),
        })
    }
}

/// 从库里读回的一行。`sequence` 是读库顺序的唯一依据——业务时间可能相同，
/// 落库时间可能被恢复脚本重写，只有自增序号永远单调。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub sequence: i64,
    pub event: AuditEvent,
    pub recorded_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::{StageAction, StageFacts, StageTarget, authorize_stage};
    use crate::domain::{Principal, PrincipalKind};

    fn scope() -> Scope {
        Scope {
            company_id: DomainId::new("c-1"),
            project_id: Some(DomainId::new("p-1")),
            issue_id: None,
        }
    }

    fn base() -> AuditEvent {
        AuditEvent {
            company_id: "c-1".to_string(),
            correlation_id: "req-7".to_string(),
            actor: DomainId::new("u-1"),
            action: AuditAction::Submit,
            scope: scope(),
            basis: "submit:project_membership".to_string(),
            decision: AuditDecision::Allowed,
            reason: None,
            decided_at: "2026-11-21T10:00:00Z".to_string(),
        }
    }

    #[test]
    fn allowed_with_reason_is_rejected() {
        let event = AuditEvent {
            reason: Some("多余的解释".to_string()),
            ..base()
        };
        assert_eq!(
            event.validate(),
            Err(AuditRejection::DecisionReasonMismatch),
            "放行带原因会让『有无原因』失去判别力"
        );
    }

    #[test]
    fn denied_without_reason_is_rejected() {
        let event = AuditEvent {
            decision: AuditDecision::Denied,
            ..base()
        };
        assert_eq!(
            event.validate(),
            Err(AuditRejection::DecisionReasonMismatch),
            "拒绝不写原因，审计就回答不了『为什么』"
        );
    }

    #[test]
    fn denied_with_blank_reason_is_rejected() {
        let event = AuditEvent {
            decision: AuditDecision::Denied,
            reason: Some("   ".to_string()),
            ..base()
        };
        assert_eq!(
            event.validate(),
            Err(AuditRejection::DecisionReasonMismatch)
        );
    }

    #[test]
    fn blank_required_field_names_the_field() {
        let event = AuditEvent {
            correlation_id: " ".to_string(),
            ..base()
        };
        assert_eq!(
            event.validate(),
            Err(AuditRejection::BlankField("correlation_id"))
        );
    }

    #[test]
    fn secret_shaped_basis_is_rejected() {
        for text in [
            "Authorization: Bearer abc123",
            "handshake_token=deadbeef",
            "client_secret 轮换后",
            "db password 在环境变量",
        ] {
            let event = AuditEvent {
                basis: text.to_string(),
                ..base()
            };
            assert!(
                matches!(event.validate(), Err(AuditRejection::SecretInText(_))),
                "{text} 应被判为含凭据"
            );
        }
    }

    #[test]
    fn ordinary_explanations_are_not_mistaken_for_secrets() {
        // 反向用例：误伤会让合法审计写不进去，等于把闸门调成常闭。
        for text in [
            "submit:project_membership",
            "accept:not_a_member",
            "项目成员，当前 Grant 有效",
            "handshake_token 缺失（未读取字段值）",
            "依据 I-65 检查点游标",
        ] {
            let event = AuditEvent {
                basis: text.to_string(),
                ..base()
            };
            assert_eq!(event.validate(), Ok(()), "{text} 不应被判为含凭据");
        }
    }

    #[test]
    fn from_stage_carries_basis_and_reason_apart() {
        let facts = StageFacts::default();
        let actor = Principal {
            id: DomainId::new("u-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let target = StageTarget::new(scope(), 1);
        let decision = authorize_stage(
            &facts,
            &actor,
            StageAction::Submit,
            &target,
            "2026-11-21T10:00:00Z",
        );
        assert!(!decision.is_allowed(), "空事实下提交应被拒（不是成员）");
        let event = AuditEvent::from_stage("c-1", "req-7", &decision, "2026-11-21T10:00:00Z");
        assert_eq!(event.decision, AuditDecision::Denied);
        assert_eq!(event.basis, "submit:not_a_member");
        assert_eq!(event.reason.as_deref(), Some("不是该项目成员"));
        assert_eq!(event.validate(), Ok(()));
    }

    #[test]
    fn decision_round_trips_through_text() {
        for decision in [AuditDecision::Allowed, AuditDecision::Denied] {
            assert_eq!(AuditDecision::parse(decision.as_str()), Some(decision));
        }
        assert_eq!(AuditDecision::parse("maybe"), None);
    }

    /// 词表是闭集：每个取值都能往返，表外值一律 `None`（读库时报错，不静默回退）。
    #[test]
    fn action_vocabulary_is_closed_under_round_trip() {
        for action in [
            AuditAction::Submit,
            AuditAction::ManageScope,
            AuditAction::Dispatch,
            AuditAction::Develop,
            AuditAction::Review,
            AuditAction::Accept,
            AuditAction::Release,
            AuditAction::Rollback,
            AuditAction::Prove,
            AuditAction::Reopen,
            AuditAction::ImportSource,
            AuditAction::AppendLink,
            AuditAction::RecordObservation,
        ] {
            assert_eq!(AuditAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(
            AuditAction::parse("import_source"),
            None,
            "下划线不是词表取值"
        );
        assert_eq!(AuditAction::parse("link"), None);
    }

    /// 阶段准入与业务写入必须分得开：混在一起，`StageRequirement` 就无法诚实回答。
    #[test]
    fn stage_actions_and_business_writes_are_disjoint() {
        for action in [
            AuditAction::Submit,
            AuditAction::ManageScope,
            AuditAction::Dispatch,
            AuditAction::Develop,
            AuditAction::Review,
            AuditAction::Accept,
            AuditAction::Release,
            AuditAction::Rollback,
            AuditAction::Prove,
            AuditAction::Reopen,
        ] {
            assert!(action.is_stage(), "{} 必须是阶段动作", action.as_str());
        }
        for action in [
            AuditAction::ImportSource,
            AuditAction::AppendLink,
            AuditAction::RecordObservation,
            AuditAction::RevokeAuthorization,
        ] {
            assert!(!action.is_stage(), "{} 不是阶段动作", action.as_str());
        }
        // 阶段判定与词表**同源**：凡是 `StageAction` 认得的字符串，`is_stage` 必须为真；
        // 反之业务写入的字符串 `StageAction::parse` 必须不认。这条断言盯的是
        // 「以后有人给审计加了个阶段动作的字符串，却忘了让 `StageAction` 认识它」——
        // 那种情况下 `for_write` 会放行一个本该只由判定生成的阶段事件。
        for action in [
            AuditAction::ImportSource,
            AuditAction::AppendLink,
            AuditAction::RecordObservation,
            AuditAction::RevokeAuthorization,
        ] {
            assert!(
                StageAction::parse(action.as_str()).is_none(),
                "{} 是业务写入，不应被 `StageAction` 认识",
                action.as_str()
            );
        }
        // `StageAction` 的每个变体都能映射进来，且映射结果仍是阶段动作——
        // 漏映射会让 `from_stage` 生成的 basis 前缀与实际动作不符。
        for action in [
            StageAction::Submit,
            StageAction::ManageScope,
            StageAction::Dispatch,
            StageAction::Develop,
            StageAction::Review,
            StageAction::Accept,
            StageAction::Release,
            StageAction::Rollback,
            StageAction::Prove,
            StageAction::Reopen,
        ] {
            let mapped = AuditAction::from(action);
            assert!(mapped.is_stage());
            assert_eq!(mapped.as_str(), action.as_str(), "词表必须同形");
        }
    }

    /// 脱敏闸门只看 `basis` 的正文：动作前缀是程序拼的常量，不该被当成凭据文本。
    ///
    /// 这条也是 `revoke-authorization` 这个动作名能成立的前提——它的前缀里含
    /// `authorization:`（标记表里那一条本来是抓 HTTP `Authorization` 头的）。
    /// 若整段检查，撤权审计永远写不进去，而报错只会说「basis 含凭据」，
    /// 病因离现场很远。
    #[test]
    fn secret_markers_scan_the_basis_body_not_the_action_prefix() {
        let mut event = base();
        event.action = AuditAction::RevokeAuthorization;
        event.basis = "revoke-authorization:principal_revocation".to_string();
        assert!(event.validate().is_ok(), "动作前缀不该被当成凭据文本");

        // 前缀剥不掉时整段检查：不能靠「随便加个像前缀的东西」绕过闸门。
        let mut forged = base();
        forged.basis = "authorization: Bearer abc".to_string();
        assert!(
            matches!(
                forged.validate(),
                Err(AuditRejection::SecretInText("basis"))
            ),
            "冒充前缀的整段文本必须照旧被拒"
        );

        // 剥掉真前缀之后，正文仍要检查。
        let mut body = base();
        body.basis = "submit:authorization: Bearer abc".to_string();
        assert!(
            matches!(body.validate(), Err(AuditRejection::SecretInText("basis"))),
            "正文里的凭据形态仍然要拦"
        );
    }

    /// `for_write` 拒绝阶段动作：那种事件只能出自判定，不能由调用方凭空构造。
    #[test]
    fn for_write_refuses_stage_actions() {
        let err = AuditEvent::for_write(
            "c-1",
            "k-1",
            DomainId::new("u-1"),
            AuditAction::Accept,
            scope(),
            "grant:accept",
            "2026-11-22T00:00:00Z",
        )
        .expect_err("阶段动作不得由 for_write 生成");
        assert!(matches!(err, AuditRejection::ActionKindMismatch(_)));
    }

    /// 写入审计恒为 allowed 且无原因，basis 以动作名开头——与阶段动作同形，便于按动作聚合。
    #[test]
    fn for_write_marks_an_established_write() {
        let event = AuditEvent::for_write(
            "c-1",
            "k-1",
            DomainId::new("u-1"),
            AuditAction::ImportSource,
            scope(),
            "source_identity",
            "2026-11-22T00:00:00Z",
        )
        .expect("业务写入应可构造");
        assert_eq!(event.action, AuditAction::ImportSource);
        assert_eq!(event.basis, "import-source:source_identity");
        assert_eq!(event.decision, AuditDecision::Allowed);
        assert_eq!(event.reason, None);
        assert_eq!(event.validate(), Ok(()));
    }

    /// 依据为空等于没写「凭什么」，与字段缺失同档处理。
    #[test]
    fn for_write_requires_evidence() {
        let err = AuditEvent::for_write(
            "c-1",
            "k-1",
            DomainId::new("u-1"),
            AuditAction::AppendLink,
            scope(),
            "   ",
            "2026-11-22T00:00:00Z",
        )
        .expect_err("空依据不得写入");
        assert_eq!(err, AuditRejection::BlankField("evidence"));
    }
}
