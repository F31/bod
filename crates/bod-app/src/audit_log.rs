//! 业务审计事件（CR-002 / V2.9.8 §8）。
//!
//! 边界：本模块只定义**事件的形状**与**能否落库**，不负责写库（`store` 负责）
//! 与输出（`telemetry` 负责）。这样同一份判定既能落表又能进日志，不会长出两套字段。
//!
//! 与 `authorization::StageAudit` 的关系：后者是**判定过程的内存产物**，随请求结束消失；
//! 本模块把它变成可持久的事实。`AuditEvent::from_stage` 是唯一的转换入口——
//! 两处各自拼字段是审计字段漂移的常见来源，所以只留一条路。
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
    pub action: StageAction,
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

        for (field, value) in [
            ("correlation_id", Some(self.correlation_id.as_str())),
            ("actor", Some(self.actor.as_str())),
            ("basis", Some(self.basis.as_str())),
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
            action: audit.action,
            scope: audit.scope.clone(),
            // 前缀是动作：同一条链里可能有多段判定（提交 → 接受），只按凭据名读会串。
            basis: format!("{}:{}", audit.action.as_str(), evidence),
            decision: outcome,
            reason,
            decided_at: decided_at.into(),
        }
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
            action: StageAction::Submit,
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
}
