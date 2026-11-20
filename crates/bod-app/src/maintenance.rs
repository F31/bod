//! P4 任务 3.6：维护计划与有限触发提案（`release-governance` / G-35）。
//!
//! G-35 的原话是「条件变化生成去重提案，预算到限停止，未知环境不显示健康」。三件事对应
//! 三个硬约束，缺一个这条门禁就不成立：
//! 1. **去重**：同一 Manifest 上同一触发条件反复出现，只生成一个提案。否则依赖到期会每天
//!    开一张单，最后没人看。
//! 2. **有界**：提案只能做计划里声明的只读检查，预算到限就停。**不**自动升级、**不**自动发布、
//!    **不**开启无限开发——这是维护与"自己改生产"的分界线。
//! 3. **不装健康**：环境观测不到就是 `Unknown`，不允许显示成 healthy。
//!
//! 本模块只产出提案与判定，不执行检查、不写库、不改环境。

use crate::release_governance::{DeploymentProjection, EnvironmentRef};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// 环境健康
// ---------------------------------------------------------------------------

/// 环境健康。`Unknown` 是独立一档，不是"健康"的默认值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy { revision: String },
    Degraded { detail: String },
    Unknown { reason: String },
}

impl Health {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy { .. } => "healthy",
            Self::Degraded { .. } => "degraded",
            Self::Unknown { .. } => "unknown",
        }
    }

    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy { .. })
    }
}

/// 从部署投影推健康。期望与实际一致才算健康；观测不到一律 Unknown。
pub fn health_of(environment: &EnvironmentRef) -> Health {
    match environment.projection() {
        DeploymentProjection::Deployed { revision, .. } => Health::Healthy { revision },
        DeploymentProjection::Drifted {
            desired, observed, ..
        } => Health::Degraded {
            detail: format!("期望 {desired}，实际 {observed}"),
        },
        DeploymentProjection::Unknown { reason } => Health::Unknown { reason },
    }
}

// ---------------------------------------------------------------------------
// 维护计划
// ---------------------------------------------------------------------------

/// 触发条件。去重键只由"种类 + 主体"决定，与发现时间无关。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    DependencyExpiring { name: String },
    CertificateExpiring { name: String },
    DriftDetected { environment_id: String },
    EnvironmentUnknown { environment_id: String },
}

impl Trigger {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DependencyExpiring { .. } => "dependency-expiring",
            Self::CertificateExpiring { .. } => "certificate-expiring",
            Self::DriftDetected { .. } => "drift-detected",
            Self::EnvironmentUnknown { .. } => "environment-unknown",
        }
    }

    pub fn subject(&self) -> &str {
        match self {
            Self::DependencyExpiring { name } | Self::CertificateExpiring { name } => name,
            Self::DriftDetected { environment_id }
            | Self::EnvironmentUnknown { environment_id } => environment_id,
        }
    }
}

/// 维护计划。字段按规格：主体、前提、最后验证、触发条件、允许检查、预算、停止条件、人工接管。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenancePlan {
    pub plan_id: String,
    pub manifest_id: String,
    pub revision: String,
    pub owner: String,
    pub preconditions: Vec<String>,
    pub last_verified: Option<String>,
    pub triggers: Vec<Trigger>,
    /// 允许做的检查（白名单）。提案里出现名单外的动作即拒绝。
    pub allowed_checks: Vec<String>,
    pub budget_minor: i64,
    pub stop_conditions: Vec<String>,
    pub manual_takeover: String,
}

impl MaintenancePlan {
    pub fn new(plan_id: &str, manifest_id: &str, revision: &str, owner: &str) -> Self {
        Self {
            plan_id: plan_id.to_string(),
            manifest_id: manifest_id.to_string(),
            revision: revision.to_string(),
            owner: owner.to_string(),
            preconditions: Vec::new(),
            last_verified: None,
            triggers: Vec::new(),
            allowed_checks: Vec::new(),
            budget_minor: 0,
            stop_conditions: Vec::new(),
            manual_takeover: String::new(),
        }
    }

    pub fn with_checks(mut self, checks: &[&str]) -> Self {
        self.allowed_checks = checks.iter().map(|item| item.to_string()).collect();
        self
    }

    pub fn with_budget(mut self, budget_minor: i64) -> Self {
        self.budget_minor = budget_minor;
        self
    }

    pub fn with_stop_condition(mut self, condition: &str) -> Self {
        self.stop_conditions.push(condition.to_string());
        self
    }

    pub fn with_trigger(mut self, trigger: Trigger) -> Self {
        self.triggers.push(trigger);
        self
    }

    /// 去重键：`manifest@revision::trigger-kind::subject`。
    pub fn dedup_key(&self, trigger: &Trigger) -> String {
        format!(
            "{}@{}::{}::{}",
            self.manifest_id,
            self.revision,
            trigger.kind(),
            trigger.subject()
        )
    }
}

/// 提案。刻意只有"检查"这一种动作，且恒为只读。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceProposal {
    pub proposal_id: String,
    pub plan_id: String,
    pub dedup_key: String,
    pub trigger: Trigger,
    pub checks: Vec<String>,
    pub budget_minor: i64,
    pub read_only: bool,
    pub requires_review: bool,
    pub note: String,
}

impl MaintenanceProposal {
    /// 提案能不能升级/发布：永远不能。
    pub fn may_upgrade(&self) -> bool {
        false
    }

    pub fn may_publish(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalOutcome {
    Proposed(MaintenanceProposal),
    /// 已有同一去重键的提案：不重复开单。
    Duplicate {
        dedup_key: String,
    },
    /// 预算到限：停止，不再提新提案。
    BudgetExhausted {
        spent_minor: i64,
        budget_minor: i64,
    },
    /// 计划没声明这个触发条件。
    UnexpectedTrigger {
        kind: String,
    },
    /// 请求的检查超出白名单。
    OutOfScope {
        requested: String,
    },
}

impl ProposalOutcome {
    pub fn proposal(&self) -> Option<&MaintenanceProposal> {
        match self {
            Self::Proposed(proposal) => Some(proposal),
            _ => None,
        }
    }
}

/// 提案台账：只按去重键记，不做别的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceBook {
    seen: BTreeSet<String>,
    proposals: Vec<MaintenanceProposal>,
}

impl MaintenanceBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.proposals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.proposals.is_empty()
    }

    pub fn proposals(&self) -> &[MaintenanceProposal] {
        &self.proposals
    }
}

/// 生成提案。`spent_minor` 是该计划已花的维护预算。
pub fn propose(
    plan: &MaintenancePlan,
    book: &mut MaintenanceBook,
    trigger: Trigger,
    proposal_id: &str,
    spent_minor: i64,
) -> ProposalOutcome {
    if !plan.triggers.contains(&trigger) {
        return ProposalOutcome::UnexpectedTrigger {
            kind: trigger.kind().to_string(),
        };
    }
    let dedup_key = plan.dedup_key(&trigger);
    if book.seen.contains(&dedup_key) {
        return ProposalOutcome::Duplicate { dedup_key };
    }
    if spent_minor >= plan.budget_minor {
        return ProposalOutcome::BudgetExhausted {
            spent_minor,
            budget_minor: plan.budget_minor,
        };
    }

    // 默认只读检查：计划声明的白名单，配合触发条件给出最小集合。
    let checks = match &trigger {
        Trigger::DependencyExpiring { name } => vec![format!("check-dependency:{name}")],
        Trigger::CertificateExpiring { name } => vec![format!("check-certificate:{name}")],
        Trigger::DriftDetected { environment_id } => {
            vec![format!("check-drift:{environment_id}")]
        }
        Trigger::EnvironmentUnknown { environment_id } => {
            vec![format!("check-connectivity:{environment_id}")]
        }
    };
    if let Some(unknown) = checks
        .iter()
        .find(|check| !plan.allowed_checks.iter().any(|allowed| allowed == *check))
    {
        return ProposalOutcome::OutOfScope {
            requested: unknown.clone(),
        };
    }

    let remaining = plan.budget_minor - spent_minor;
    let proposal = MaintenanceProposal {
        proposal_id: proposal_id.to_string(),
        plan_id: plan.plan_id.clone(),
        dedup_key: dedup_key.clone(),
        trigger,
        checks: checks.clone(),
        budget_minor: remaining,
        read_only: true,
        requires_review: true,
        note: format!(
            "只读检查 {}；不动环境，不升级依赖，不发布；人工接管：{}",
            checks.join("、"),
            plan.manual_takeover
        ),
    };
    book.seen.insert(dedup_key);
    book.proposals.push(proposal.clone());
    ProposalOutcome::Proposed(proposal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> MaintenancePlan {
        MaintenancePlan::new("maint-1", "stage-1", "rev-2", "release-ops")
            .with_checks(&[
                "check-dependency:serde",
                "check-certificate:edge-tls",
                "check-drift:env-staging",
                "check-connectivity:env-staging",
            ])
            .with_budget(500)
            .with_stop_condition("发现需要改代码即停，转人工")
            .with_trigger(Trigger::DependencyExpiring {
                name: "serde".to_string(),
            })
            .with_trigger(Trigger::CertificateExpiring {
                name: "edge-tls".to_string(),
            })
            .with_trigger(Trigger::DriftDetected {
                environment_id: "env-staging".to_string(),
            })
            .with_trigger(Trigger::EnvironmentUnknown {
                environment_id: "env-staging".to_string(),
            })
    }

    /// 3.6 / G-35：提案只做白名单里的只读检查，不升级不发布。
    #[test]
    fn proposals_are_limited_to_allowed_read_only_checks() {
        let plan = plan();
        let mut book = MaintenanceBook::new();
        let outcome = propose(
            &plan,
            &mut book,
            Trigger::DependencyExpiring {
                name: "serde".to_string(),
            },
            "prop-1",
            0,
        );
        let proposal = outcome.proposal().expect("应生成提案");
        assert!(proposal.read_only, "维护提案默认只读");
        assert!(proposal.requires_review, "要人工审阅");
        assert_eq!(proposal.checks, vec!["check-dependency:serde".to_string()]);
        assert_eq!(proposal.budget_minor, 500, "剩余预算随提案带出");
        assert!(proposal.note.contains("不发布"));
        assert!(!proposal.may_upgrade());
        assert!(!proposal.may_publish());
    }

    /// 3.6 / G-35：同一触发反复出现只生成一个提案。
    #[test]
    fn repeated_trigger_generates_one_proposal() {
        let plan = plan();
        let mut book = MaintenanceBook::new();
        let trigger = Trigger::DriftDetected {
            environment_id: "env-staging".to_string(),
        };
        assert!(
            propose(&plan, &mut book, trigger.clone(), "prop-1", 0)
                .proposal()
                .is_some()
        );
        let second = propose(&plan, &mut book, trigger.clone(), "prop-2", 0);
        match second {
            ProposalOutcome::Duplicate { dedup_key } => {
                assert!(dedup_key.contains("drift-detected"));
                assert!(dedup_key.contains("stage-1@rev-2"));
            }
            other => panic!("重复触发应去重，实际 {other:?}"),
        }
        assert_eq!(book.len(), 1, "去重后台账只有一条");

        // 换了主体就是新条件：计划没声明的不会被开单（去重键含主体，不会串台）。
        let other_env = Trigger::DriftDetected {
            environment_id: "env-prod".to_string(),
        };
        assert_eq!(
            propose(&plan, &mut book, other_env, "prop-3", 0),
            ProposalOutcome::UnexpectedTrigger {
                kind: "drift-detected".to_string()
            }
        );
    }

    /// 3.6 / G-35：预算到限即停，不再开新提案。
    #[test]
    fn budget_exhaustion_stops_maintenance_proposals() {
        let plan = plan();
        let mut book = MaintenanceBook::new();
        match propose(
            &plan,
            &mut book,
            Trigger::CertificateExpiring {
                name: "edge-tls".to_string(),
            },
            "prop-1",
            500,
        ) {
            ProposalOutcome::BudgetExhausted {
                spent_minor,
                budget_minor,
            } => {
                assert_eq!(spent_minor, 500);
                assert_eq!(budget_minor, 500);
            }
            other => panic!("预算到限应停止，实际 {other:?}"),
        }
        assert!(book.is_empty(), "到限不生成提案");
    }

    /// 3.6：计划没声明的触发条件不会被开单；白名单外的检查直接拒绝。
    #[test]
    fn unexpected_trigger_and_out_of_scope_checks_are_refused() {
        let plan = plan();
        let mut book = MaintenanceBook::new();
        assert_eq!(
            propose(
                &plan,
                &mut book,
                Trigger::DependencyExpiring {
                    name: "tokio".to_string()
                },
                "prop-1",
                0
            ),
            ProposalOutcome::UnexpectedTrigger {
                kind: "dependency-expiring".to_string()
            }
        );

        let narrow = MaintenancePlan::new("maint-2", "stage-1", "rev-2", "release-ops")
            .with_checks(&["check-drift:env-staging"])
            .with_budget(500)
            .with_trigger(Trigger::CertificateExpiring {
                name: "edge-tls".to_string(),
            });
        assert_eq!(
            propose(
                &narrow,
                &mut book,
                Trigger::CertificateExpiring {
                    name: "edge-tls".to_string()
                },
                "prop-2",
                0
            ),
            ProposalOutcome::OutOfScope {
                requested: "check-certificate:edge-tls".to_string()
            }
        );
    }

    /// 3.6 / G-35：未知环境不显示健康。
    #[test]
    fn unknown_environment_health_stays_unknown() {
        let mut env = EnvironmentRef::new("env-staging", "k8s", "cluster-a/ns-b", "预发布")
            .with_desired("build-42");
        env.observe(None, "2026-11-21T10:00:00Z");
        let health = health_of(&env);
        assert_eq!(health.as_str(), "unknown");
        assert!(!health.is_healthy(), "观测不到不能算健康");
        match health {
            Health::Unknown { reason } => assert!(reason.contains("没有实际版本观测")),
            other => panic!("应为 Unknown，实际 {other:?}"),
        }

        env.observe(Some("build-42"), "2026-11-21T10:05:00Z");
        assert!(health_of(&env).is_healthy());

        env.observe(Some("build-39"), "2026-11-21T10:10:00Z");
        match health_of(&env) {
            Health::Degraded { detail } => assert!(detail.contains("build-39")),
            other => panic!("漂移应显示 degraded，实际 {other:?}"),
        }
    }
}
