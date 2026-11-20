//! P2 任务 1.1：HandoffPackage 的等级骨架与准入判定（纯值类型）。
//!
//! 边界：本模块只定义 schema 身份、L1/L2/L3 等级与「已测组合」准入的纯判定。
//! 规范编码与摘要（1.2）、宿主事实与 Agent 叙述分栏（1.3）、部分快照与版本绑定（1.4）、
//! 现场清单过滤与交接事件（1.5）由后续切片承担；接管事务（2.x）才使用本判定结果。
//! 这里不读 store、不读文件、不写库、不授予任何管理权。

use crate::domain::DomainId;
use crate::domain::execution::{EvidenceLevel, EvidenceSource};
use std::fmt;

/// §18.10 的交接包 schema 标识；1.1 只固定标识，不实现 payload 编码。
pub const HANDOFF_SCHEMA: &str = "bod.handoff/v1";

/// §18.9 的三种接续能力等级。L1 < L2 < L3，比较序即能力包含关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HandoffLevel {
    /// 资料交接：目标、成果、问题与已知执行事实；新 Agent 自行重建环境。
    L1,
    /// 现场重建：固定代码/补丁、输入和必要环境定义；不迁移内部推理与不可导出会话。
    L2,
    /// 验证后接续：重建后核验输入与结论，再执行剩余工作；只限已测组合。
    L3,
}

impl HandoffLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::L1 => "L1",
            Self::L2 => "L2",
            Self::L3 => "L3",
        }
    }

    /// 只有 L3 承诺自动接续，因此只有 L3 需要已测运行时/工具/任务组合证据。
    pub const fn requires_tested_combination(self) -> bool {
        matches!(self, Self::L3)
    }
}

impl fmt::Display for HandoffLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 交接包身份骨架（§18.10 payload 的标识字段子集）。
///
/// 只覆盖 1.1 需要的身份与等级；`payload_digest`、`integrity`、`host_facts`、
/// `narrative`、`missing`、`workspace` 分别在 1.2—1.5 扩展，这里不预留假值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffPackageHeader {
    pub schema: String,
    pub level: HandoffLevel,
    pub company_id: DomainId,
    pub project_id: DomainId,
    pub issue_id: DomainId,
    pub work_package_revision: DomainId,
    pub source_attempt_id: DomainId,
    pub snapshot_seq: u64,
}

impl HandoffPackageHeader {
    /// 该骨架是否声明了受支持的 schema 与等级。1.1 不校验正文完整性（1.2 的职责）。
    pub fn has_supported_schema(&self) -> bool {
        self.schema == HANDOFF_SCHEMA
    }
}

/// 运行时 + 工具集 + 任务类别的组合键。
///
/// `tools` 在 `new()` 与 `canonical()` 中都按字典序去重，避免「同一集合不同顺序」
/// 绕过命中；调用方直接构造字面量也不会影响判定，因为比较走 `canonical()`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinationKey {
    pub runtime: String,
    pub tools: Vec<String>,
    pub task_class: String,
}

impl CombinationKey {
    pub fn new(
        runtime: impl Into<String>,
        tools: &[String],
        task_class: impl Into<String>,
    ) -> Self {
        let mut normalized = tools.to_vec();
        normalized.sort();
        normalized.dedup();
        Self {
            runtime: runtime.into(),
            tools: normalized,
            task_class: task_class.into(),
        }
    }

    /// 稳定比较键：集合顺序不参与，运行时/任务类别任一变化都换键。
    pub fn canonical(&self) -> String {
        let mut tools = self.tools.clone();
        tools.sort();
        tools.dedup();
        format!(
            "{HANDOFF_SCHEMA}\nruntime:{}\ntools:{}\ntask:{}",
            self.runtime,
            tools.join(","),
            self.task_class
        )
    }
}

/// L3 准入缺失的证据维度。顺序固定，便于上层面板稳定展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingEvidence {
    /// 没有同运行时/工具/任务类别的已测记录。
    CombinationNotTested,
    /// 证据等级低于 RuntimeObserved（探测或自报不足以称「已测组合」）。
    EvidenceBelowRuntimeObserved,
    /// 证据来源的上限达不到 RuntimeObserved（例如适配器自报）。
    SourceCannotSupportRuntimeObserved,
    /// 证据已过期，不能据其自动接续。
    Stale,
}

impl MissingEvidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CombinationNotTested => "combination_not_tested",
            Self::EvidenceBelowRuntimeObserved => "evidence_below_runtime_observed",
            Self::SourceCannotSupportRuntimeObserved => "source_cannot_support_runtime_observed",
            Self::Stale => "stale",
        }
    }
}

impl fmt::Display for MissingEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 已测组合证据快照：由上层从 support matrix / conformance 报告只读注入。
///
/// 1.1 不查数据库、不做报告真实性校验；`report_ref` 只作审计追溯，不参与准入判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestedCombination {
    pub key: CombinationKey,
    pub evidence_level: EvidenceLevel,
    pub source: EvidenceSource,
    pub observed_at_ms: i64,
    pub expires_at_ms: i64,
    pub report_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    /// L3 被拒：回显原组合键与所有缺失维度，供「说明缺少的组合证据」与降级使用。
    UntestedCombination {
        key: CombinationKey,
        missing: Vec<MissingEvidence>,
    },
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntestedCombination { key, missing } => write!(
                f,
                "L3 接续被拒：组合 {} 缺少已测证据（{}）",
                key.canonical(),
                missing
                    .iter()
                    .map(|item| item.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for AdmissionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// 请求等级被准入；L1/L2 不需要已测组合，L3 需命中合规证据。
    Admitted { level: HandoffLevel },
    /// 原等级被拒，调用方选择的较低等级获批；`missing` 说明为何不能保持原等级。
    Downgraded {
        from: HandoffLevel,
        to: HandoffLevel,
        missing: Vec<MissingEvidence>,
    },
}

impl AdmissionDecision {
    pub fn level(&self) -> HandoffLevel {
        match self {
            Self::Admitted { level } => *level,
            Self::Downgraded { to, .. } => *to,
        }
    }
}

/// 严格准入：L1/L2 直接放行；L3 必须命中一条同时满足等级、来源上限与新鲜度的已测组合。
/// 不读任何外部状态，证据由调用方注入。
pub fn admit_level(
    requested: HandoffLevel,
    key: &CombinationKey,
    tested: &[TestedCombination],
    now_ms: i64,
) -> Result<HandoffLevel, AdmissionError> {
    if !requested.requires_tested_combination() {
        return Ok(requested);
    }
    let missing = missing_evidence(key, tested, now_ms);
    if missing.is_empty() {
        Ok(requested)
    } else {
        Err(AdmissionError::UntestedCombination {
            key: key.clone(),
            missing,
        })
    }
}

/// 允许降级的准入：仅在调用方显式选择更低等级时返回 `Downgraded`；
/// `fallback >= requested` 视为试图升格或原地伪装，按严格准入的错误返回。
pub fn admit_with_fallback(
    requested: HandoffLevel,
    fallback: HandoffLevel,
    key: &CombinationKey,
    tested: &[TestedCombination],
    now_ms: i64,
) -> Result<AdmissionDecision, AdmissionError> {
    match admit_level(requested, key, tested, now_ms) {
        Ok(level) => Ok(AdmissionDecision::Admitted { level }),
        Err(err) if fallback < requested => {
            let missing = match &err {
                AdmissionError::UntestedCombination { missing, .. } => missing.clone(),
            };
            Ok(AdmissionDecision::Downgraded {
                from: requested,
                to: fallback,
                missing,
            })
        }
        Err(err) => Err(err),
    }
}

/// 只有全部候选都不合规时才返回缺失维度；任一合规记录即视为已测组合。
fn missing_evidence(
    key: &CombinationKey,
    tested: &[TestedCombination],
    now_ms: i64,
) -> Vec<MissingEvidence> {
    let wanted = key.canonical();
    let candidates: Vec<&TestedCombination> = tested
        .iter()
        .filter(|record| record.key.canonical() == wanted)
        .collect();
    if candidates.is_empty() {
        return vec![MissingEvidence::CombinationNotTested];
    }

    let runtime_observed = EvidenceLevel::RuntimeObserved.rank();
    let mut accumulated: Vec<MissingEvidence> = Vec::new();
    for record in candidates {
        let mut reasons = Vec::new();
        if record.evidence_level.rank() < runtime_observed {
            reasons.push(MissingEvidence::EvidenceBelowRuntimeObserved);
        }
        if record.source.ceiling().rank() < runtime_observed {
            reasons.push(MissingEvidence::SourceCannotSupportRuntimeObserved);
        }
        if record.expires_at_ms <= now_ms {
            reasons.push(MissingEvidence::Stale);
        }
        if reasons.is_empty() {
            return Vec::new();
        }
        for reason in reasons {
            if !accumulated.contains(&reason) {
                accumulated.push(reason);
            }
        }
    }
    accumulated
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_764_000_000_000;

    fn key() -> CombinationKey {
        CombinationKey::new(
            "local-process@1.0/linux",
            &["git".to_string(), "cargo".to_string()],
            "delivery",
        )
    }

    fn tested() -> TestedCombination {
        TestedCombination {
            key: key(),
            evidence_level: EvidenceLevel::RuntimeObserved,
            source: EvidenceSource::TestResult,
            observed_at_ms: NOW - 10,
            expires_at_ms: NOW + 10_000,
            report_ref: "cas:sha256:aaaa".to_string(),
        }
    }

    fn header(level: HandoffLevel) -> HandoffPackageHeader {
        HandoffPackageHeader {
            schema: HANDOFF_SCHEMA.to_string(),
            level,
            company_id: DomainId::new("company-1"),
            project_id: DomainId::new("project-1"),
            issue_id: DomainId::new("issue-1"),
            work_package_revision: DomainId::new("wp-r1"),
            source_attempt_id: DomainId::new("attempt-1"),
            snapshot_seq: 7,
        }
    }

    #[test]
    fn l1_request_is_admitted_without_tested_combination() {
        assert_eq!(
            admit_level(HandoffLevel::L1, &key(), &[], NOW),
            Ok(HandoffLevel::L1)
        );
        let package = header(HandoffLevel::L1);
        assert_eq!(package.schema, HANDOFF_SCHEMA);
        assert!(package.has_supported_schema());
    }

    #[test]
    fn l2_request_is_admitted_without_tested_combination() {
        assert_eq!(
            admit_level(HandoffLevel::L2, &key(), &[], NOW),
            Ok(HandoffLevel::L2)
        );
        // L2 是现场重建，不承诺导出内部推理或自动迁移全部工具状态。
        assert!(!HandoffLevel::L2.requires_tested_combination());
    }

    #[test]
    fn l3_request_for_untested_combination_is_rejected() {
        let err = admit_level(HandoffLevel::L3, &key(), &[], NOW).unwrap_err();
        let rendered = err.to_string();
        let AdmissionError::UntestedCombination {
            key: returned,
            missing,
        } = &err;
        assert_eq!(returned, &key());
        assert_eq!(missing, &vec![MissingEvidence::CombinationNotTested]);
        assert!(
            rendered.contains("L3 接续被拒"),
            "拒绝必须说明缺少的组合证据"
        );
    }

    #[test]
    fn l3_request_for_untested_combination_can_downgrade_to_l2() {
        let decision = admit_with_fallback(HandoffLevel::L3, HandoffLevel::L2, &key(), &[], NOW)
            .expect("允许降级时不应返回硬错误");
        assert_eq!(
            decision,
            AdmissionDecision::Downgraded {
                from: HandoffLevel::L3,
                to: HandoffLevel::L2,
                missing: vec![MissingEvidence::CombinationNotTested],
            }
        );
        assert_eq!(decision.level(), HandoffLevel::L2);
    }

    #[test]
    fn l3_request_can_downgrade_to_l1_when_caller_selects_it() {
        let decision = admit_with_fallback(HandoffLevel::L3, HandoffLevel::L1, &key(), &[], NOW)
            .expect("调用方显式选择 L1 时应获批");
        assert!(matches!(
            decision,
            AdmissionDecision::Downgraded {
                from: HandoffLevel::L3,
                to: HandoffLevel::L1,
                ..
            }
        ));
    }

    #[test]
    fn tested_combination_with_runtime_observed_evidence_admits_l3() {
        assert_eq!(
            admit_level(HandoffLevel::L3, &key(), &[tested()], NOW),
            Ok(HandoffLevel::L3)
        );
        let decision =
            admit_with_fallback(HandoffLevel::L3, HandoffLevel::L2, &key(), &[tested()], NOW)
                .unwrap();
        assert_eq!(
            decision,
            AdmissionDecision::Admitted {
                level: HandoffLevel::L3
            },
            "已测组合不应被无谓降级"
        );
    }

    #[test]
    fn self_declared_evidence_cannot_admit_l3() {
        let mut record = tested();
        record.evidence_level = EvidenceLevel::SelfDeclared;
        record.source = EvidenceSource::AdapterSelfDeclared;
        let err = admit_level(HandoffLevel::L3, &key(), &[record], NOW).unwrap_err();
        let AdmissionError::UntestedCombination { missing, .. } = err;
        assert!(missing.contains(&MissingEvidence::EvidenceBelowRuntimeObserved));
        assert!(missing.contains(&MissingEvidence::SourceCannotSupportRuntimeObserved));
    }

    #[test]
    fn probe_only_evidence_cannot_admit_l3() {
        let mut record = tested();
        record.evidence_level = EvidenceLevel::ProbeVerified;
        record.source = EvidenceSource::PlatformProbe;
        let err = admit_level(HandoffLevel::L3, &key(), &[record], NOW).unwrap_err();
        let AdmissionError::UntestedCombination { missing, .. } = err;
        assert!(missing.contains(&MissingEvidence::EvidenceBelowRuntimeObserved));
        assert!(missing.contains(&MissingEvidence::SourceCannotSupportRuntimeObserved));
    }

    #[test]
    fn stale_evidence_cannot_admit_l3() {
        let mut record = tested();
        record.expires_at_ms = NOW;
        let err = admit_level(HandoffLevel::L3, &key(), &[record], NOW).unwrap_err();
        let AdmissionError::UntestedCombination { missing, .. } = err;
        assert_eq!(missing, vec![MissingEvidence::Stale]);
    }

    #[test]
    fn combination_key_is_order_independent_for_tool_set() {
        let reordered = CombinationKey::new(
            "local-process@1.0/linux",
            &["cargo".to_string(), "git".to_string(), "git".to_string()],
            "delivery",
        );
        assert_eq!(reordered, key());
        assert_eq!(reordered.canonical(), key().canonical());
        let record = TestedCombination {
            key: reordered,
            ..tested()
        };
        assert_eq!(
            admit_level(HandoffLevel::L3, &key(), &[record], NOW),
            Ok(HandoffLevel::L3)
        );
    }

    #[test]
    fn combination_key_changes_when_runtime_or_task_changes() {
        let other_runtime = CombinationKey::new(
            "remote-agent@2.0/linux",
            &["git".to_string(), "cargo".to_string()],
            "delivery",
        );
        let other_task = CombinationKey::new(
            "local-process@1.0/linux",
            &["git".to_string(), "cargo".to_string()],
            "migration",
        );
        assert_ne!(other_runtime.canonical(), key().canonical());
        assert_ne!(other_task.canonical(), key().canonical());
        for changed in [other_runtime, other_task] {
            assert!(matches!(
                admit_level(HandoffLevel::L3, &changed, &[tested()], NOW),
                Err(AdmissionError::UntestedCombination { .. })
            ));
        }
    }

    #[test]
    fn fallback_cannot_be_used_to_upgrade_a_rejected_request() {
        // L3 被拒时选同级 fallback 不构成降级，必须原样返回拒绝，不能绕过证据判定。
        assert!(matches!(
            admit_with_fallback(HandoffLevel::L3, HandoffLevel::L3, &key(), &[], NOW),
            Err(AdmissionError::UntestedCombination { .. })
        ));
        assert!(admit_level(HandoffLevel::L3, &key(), &[], NOW).is_err());
    }

    #[test]
    fn admission_never_grants_manager_authority() {
        // 决策类型只有等级、降级目标与缺失维度，没有主体、命令、凭据或 epoch 字段，
        // 类型层面即无法从交接准入推导出管理权（design §Decision 7）。
        let decision = admit_with_fallback(HandoffLevel::L3, HandoffLevel::L2, &key(), &[], NOW)
            .expect("降级应为合法决策");
        let rendered = format!("{decision:?}");
        assert!(!rendered.contains("authority"));
        assert!(!rendered.contains("credential"));
        assert!(!rendered.contains("epoch"));
        assert_eq!(decision.level(), HandoffLevel::L2);
    }
}
