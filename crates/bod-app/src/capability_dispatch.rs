//! 可选能力声明驱动的候选过滤与统一限制提示（P2 任务 4.3）。
//!
//! 规格要求：cancel/resume/reconcile/quota_windows/ACK 等可选能力「SHALL 仅在适配器声明支持时
//! 验证；不支持恢复的适配器可安装，但 MUST 禁止派发需要恢复保证的任务，并在调度准入与界面
//! 显示同一限制」。
//!
//! 本模块把「任务需要什么保证」与「适配器声明了什么」放在一起判，并让**调度与界面读同一个
//! 函数**（[`CapabilityNotice`]）——「同一限制」不是靠两处各写一句文案对齐，而是结构上只有
//! 一份文案来源。
//!
//! 边界：只按**声明等级 + 有效期**判定，不做能力真实性验证。声明 supported 但没用例证明的
//! 情况属 4.1 的 `declared_only`、真实运行时验收属 4.4；这里若把没有证据当成支持，就会
//! 把「自报」变成准入依据。4.2 的 `Enablement::Blocked` 在此生效：被禁用的适配器不进候选，
//! 不论它的能力声明写了什么。

use crate::adapter_conformance::Enablement;
use crate::domain::execution::{AdapterDescriptor, CapabilitySlot, SupportLevel};
use crate::handoff_package::{
    AdmissionError, CombinationKey, HandoffLevel, TestedCombination, admit_level,
};

/// 任务可能要求的执行保证。每个保证对应一个能力槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guarantee {
    /// 会话恢复：中断后能接着跑（自动接续的前提）。
    SessionRecovery,
    /// 可靠观察：观察有 ACK/回放，断档可被发现。
    ReliableObservation,
    /// 可取消：取消能被受理并确认停止。
    Cancellation,
    /// 计量：能给出真实用量。
    Metering,
    /// 外部幂等：重复 start 返回同一执行。
    ExternalIdempotency,
    /// 配额窗口：能按窗口给出额度事实。
    QuotaWindows,
}

impl Guarantee {
    pub const fn slot(self) -> CapabilitySlot {
        match self {
            Self::SessionRecovery => CapabilitySlot::Recovery,
            Self::ReliableObservation => CapabilitySlot::ReliableObservation,
            Self::Cancellation => CapabilitySlot::Cancellation,
            Self::Metering => CapabilitySlot::Metering,
            Self::ExternalIdempotency => CapabilitySlot::ExternalIdempotency,
            Self::QuotaWindows => CapabilitySlot::QuotaWindows,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionRecovery => "session_recovery",
            Self::ReliableObservation => "reliable_observation",
            Self::Cancellation => "cancellation",
            Self::Metering => "metering",
            Self::ExternalIdempotency => "external_idempotency",
            Self::QuotaWindows => "quota_windows",
        }
    }
}

/// 一组保证需求。空集合表示「不需要额外保证」，任何未被禁用且版本可用的适配器都可承接。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequiredGuarantees {
    guarantees: Vec<Guarantee>,
}

impl RequiredGuarantees {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn of(guarantees: &[Guarantee]) -> Self {
        let mut out = Vec::new();
        for guarantee in guarantees {
            if !out.contains(guarantee) {
                out.push(*guarantee);
            }
        }
        Self { guarantees: out }
    }

    /// 从交接等级推导。
    ///
    /// - L1（资料交接）：新 Agent 自行重建，不需要会话恢复。
    /// - L2（现场重建）：需要可靠观察来核对输入与结论，但不要求原会话能续。
    /// - L3（验证后接续）：自动接续的前提正是「会话能恢复」+「观察可靠」，缺一不可。
    pub fn for_handoff(level: HandoffLevel) -> Self {
        match level {
            HandoffLevel::L1 => Self::none(),
            HandoffLevel::L2 => Self::of(&[Guarantee::ReliableObservation]),
            HandoffLevel::L3 => {
                Self::of(&[Guarantee::SessionRecovery, Guarantee::ReliableObservation])
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.guarantees.is_empty()
    }

    pub fn contains(&self, guarantee: Guarantee) -> bool {
        self.guarantees.contains(&guarantee)
    }

    pub fn iter(&self) -> impl Iterator<Item = Guarantee> + '_ {
        self.guarantees.iter().copied()
    }
}

/// 一个候选适配器：描述符（声明）+ 4.2 的准入判定。
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub descriptor: &'a AdapterDescriptor,
    pub enablement: &'a Enablement,
}

/// 候选被过滤的稳定原因。调用方按 `code()` 分支，不解析人读消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterReason {
    /// 被 4.2 的闸门判定禁用：不参与候选，与能力声明无关。
    AdapterBlocked { failures: Vec<String> },
    /// 闸门类未取得证据：不进自动派发，只能探测。
    AdapterUnverified { cases: Vec<String> },
    /// 声明不支持。
    NotSupported { slot: String },
    /// 声明为 unknown 或根本没有该槽。
    Unknown { slot: String },
    /// 声明已过期：过期声明不参与判定。
    Stale { slot: String, expires_at_ms: i64 },
}

impl FilterReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::AdapterBlocked { .. } => "adapter_blocked",
            Self::AdapterUnverified { .. } => "adapter_unverified",
            Self::NotSupported { .. } => "capability_not_supported",
            Self::Unknown { .. } => "capability_unknown",
            Self::Stale { .. } => "capability_stale",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::AdapterBlocked { failures } => {
                format!("适配器被 conformance 禁用（{}）", failures.join("；"))
            }
            Self::AdapterUnverified { cases } => {
                format!(
                    "闸门类用例未取得证据（{}），不得用于自动派发",
                    cases.join(", ")
                )
            }
            Self::NotSupported { slot } => format!("适配器声明不支持 {slot}"),
            Self::Unknown { slot } => format!("适配器未声明 {slot}（等同不支持）"),
            Self::Stale {
                slot,
                expires_at_ms,
            } => {
                format!("{slot} 的声明已于 {expires_at_ms} 过期，不参与判定")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filtered {
    pub adapter_id: String,
    pub reasons: Vec<FilterReason>,
}

/// 选择结果。`admitted` 按候选输入顺序保留，本模块不做偏好排序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub admitted: Vec<String>,
    pub filtered: Vec<Filtered>,
    pub notice: CapabilityNotice,
}

impl Selection {
    pub fn has_candidate(&self) -> bool {
        !self.admitted.is_empty()
    }

    /// 本模块不做优先级决策：只有一个候选时返回它，多于一个时要求调用方再选（不静默取第一个）。
    pub fn sole_candidate(&self) -> Option<&str> {
        match self.admitted.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

/// 统一限制提示：调度准入、CLI、工作台都用这一个函数渲染。
///
/// 结构上只有一份文案来源，所以「界面与调度显示同一限制」不是约定，而是不可能不一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityNotice {
    pub code: &'static str,
    pub missing: Vec<String>,
    pub message: String,
    pub next_step: String,
}

impl CapabilityNotice {
    /// 全部候选都可用时的提示：明确写「不需要额外保证」或「候选可用」，不留空白。
    pub fn satisfied(required: &RequiredGuarantees, admitted: usize) -> Self {
        let message = if required.is_empty() {
            "该任务不要求额外执行保证，可用适配器都满足".to_string()
        } else {
            format!("{admitted} 个候选满足所需的执行保证")
        };
        Self {
            code: "BOD_CAPABILITY_SATISFIED",
            missing: Vec::new(),
            message,
            next_step: "可直接派发".to_string(),
        }
    }

    pub fn unsatisfied(required: &RequiredGuarantees, filtered: &[Filtered]) -> Self {
        let missing: Vec<String> = required
            .iter()
            .map(|item| item.as_str().to_string())
            .collect();
        let details = filtered
            .iter()
            .flat_map(|item| {
                item.reasons
                    .iter()
                    .map(move |reason| format!("{}: {}", item.adapter_id, reason.describe()))
            })
            .collect::<Vec<_>>()
            .join("；");
        let missing_label = if missing.is_empty() {
            "—".to_string()
        } else {
            missing.join(", ")
        };
        Self {
            code: "BOD_CAPABILITY_UNSATISFIED",
            missing,
            message: format!("没有候选适配器满足所需保证（{}）。{details}", missing_label),
            next_step: format!(
                "降低接续等级，或改用声明支持 {} 的适配器；不支持的适配器可以安装但不得承接该任务",
                missing_label
            ),
        }
    }

    /// 渲染成一行文本；调度错误消息、CLI 与桌面壳都调它。
    pub fn render(&self) -> String {
        format!(
            "[{}] {} 下一步：{}",
            self.code, self.message, self.next_step
        )
    }
}

/// 按需求过滤候选。
pub fn select(
    required: &RequiredGuarantees,
    candidates: &[Candidate<'_>],
    now_ms: i64,
) -> Selection {
    let mut admitted = Vec::new();
    let mut filtered = Vec::new();
    for candidate in candidates {
        let reasons = filter_reasons(required, candidate, now_ms);
        if reasons.is_empty() {
            admitted.push(candidate.descriptor.adapter_id.clone());
        } else {
            filtered.push(Filtered {
                adapter_id: candidate.descriptor.adapter_id.clone(),
                reasons,
            });
        }
    }
    let notice = if admitted.is_empty() {
        CapabilityNotice::unsatisfied(required, &filtered)
    } else {
        CapabilityNotice::satisfied(required, admitted.len())
    };
    Selection {
        admitted,
        filtered,
        notice,
    }
}

/// 交接承接被拒的两种原因：能力不够，或能力够但缺已测组合证据。
///
/// 顺序是刻意的：先过能力门再谈证据。适配器不支持会话恢复时，「有已测组合」也不能让它做 L3。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffRefusal {
    /// 选择结果本身带完整过滤原因，体积较大，装箱避免把 Result 撑大。
    Capability(Box<Selection>),
    Untested(AdmissionError),
}

impl HandoffRefusal {
    /// 统一提示：两种拒绝都给出同一个结构，界面不需要按分支拼文案。
    pub fn notice(&self) -> CapabilityNotice {
        match self {
            Self::Capability(selection) => selection.notice.clone(),
            Self::Untested(err) => CapabilityNotice {
                code: "BOD_HANDOFF_UNTESTED",
                missing: Vec::new(),
                message: err.to_string(),
                next_step: "补充该运行时/工具/任务类别的已测证据，或显式降级到 L2/L1".to_string(),
            },
        }
    }

    pub fn render(&self) -> String {
        self.notice().render()
    }
}

/// 交接承接的唯一入口：能力门 + 已测组合，两步都过才返回等级。
///
/// 把两件事绑在一个函数里，是为了避免调用方「只查了证据、忘了查能力」。
pub fn admit_handoff(
    level: HandoffLevel,
    key: &CombinationKey,
    tested: &[TestedCombination],
    candidate: Candidate<'_>,
    now_ms: i64,
) -> Result<HandoffLevel, HandoffRefusal> {
    let required = RequiredGuarantees::for_handoff(level);
    let selection = select(&required, &[candidate], now_ms);
    if !selection.has_candidate() {
        return Err(HandoffRefusal::Capability(Box::new(selection)));
    }
    admit_level(level, key, tested, now_ms).map_err(HandoffRefusal::Untested)
}

fn filter_reasons(
    required: &RequiredGuarantees,
    candidate: &Candidate<'_>,
    now_ms: i64,
) -> Vec<FilterReason> {
    // 4.2 的结论优先：被禁用的适配器不进候选，与它声称支持什么无关。
    if let Enablement::Blocked { failures } = candidate.enablement {
        return vec![FilterReason::AdapterBlocked {
            failures: failures
                .iter()
                .map(|(id, detail)| format!("{id}: {detail}"))
                .collect(),
        }];
    }

    let mut reasons = Vec::new();
    // 未取得闸门证据是「不能自动派发」的独立原因，但不掩盖「能力本来就不支持」：
    // 两个原因都列出来，读者才知道该补证据还是该换适配器。
    if let Enablement::Unverified { cases } = candidate.enablement
        && !required.is_empty()
    {
        reasons.push(FilterReason::AdapterUnverified {
            cases: cases.iter().map(|id| (*id).to_string()).collect(),
        });
    }
    for guarantee in required.iter() {
        let slot = guarantee.slot();
        let claim = candidate
            .descriptor
            .capabilities
            .iter()
            .find(|claim| claim.slot == slot);
        let Some(claim) = claim else {
            reasons.push(FilterReason::Unknown {
                slot: slot.as_str().to_string(),
            });
            continue;
        };
        if claim.is_stale(now_ms) {
            reasons.push(FilterReason::Stale {
                slot: slot.as_str().to_string(),
                expires_at_ms: claim.expires_at_ms.unwrap_or(0),
            });
            continue;
        }
        match claim.level {
            SupportLevel::Supported => {}
            SupportLevel::Unsupported => reasons.push(FilterReason::NotSupported {
                slot: slot.as_str().to_string(),
            }),
            SupportLevel::Unknown => reasons.push(FilterReason::Unknown {
                slot: slot.as_str().to_string(),
            }),
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_conformance::Enablement;
    use crate::domain::execution::{CapabilityClaim, EvidenceLevel, EvidenceSource};
    use crate::fake_executor::{FakeExecutor, FakeScenario};
    use crate::invocation::StartTransport;

    const NOW_MS: i64 = 1_764_100_000_000;

    fn claim(
        slot: CapabilitySlot,
        level: SupportLevel,
        expires_at_ms: Option<i64>,
    ) -> CapabilityClaim {
        CapabilityClaim {
            slot,
            level,
            mode: Some("test".to_string()),
            source: EvidenceSource::AdapterSelfDeclared,
            evidence_level: EvidenceLevel::SelfDeclared,
            observed_at_ms: NOW_MS - 1_000,
            expires_at_ms,
            reason: None,
        }
    }

    fn descriptor(adapter_id: &str, claims: Vec<CapabilityClaim>) -> AdapterDescriptor {
        let mut base = FakeExecutor::new(FakeScenario::HappyPath)
            .descriptor()
            .clone();
        base.adapter_id = adapter_id.to_string();
        base.capabilities = claims;
        base
    }

    fn l3_ready(adapter_id: &str) -> AdapterDescriptor {
        descriptor(
            adapter_id,
            vec![
                claim(CapabilitySlot::Recovery, SupportLevel::Supported, None),
                claim(
                    CapabilitySlot::ReliableObservation,
                    SupportLevel::Supported,
                    None,
                ),
            ],
        )
    }

    #[test]
    fn handoff_levels_map_to_guarantees() {
        assert!(RequiredGuarantees::for_handoff(HandoffLevel::L1).is_empty());
        let l2 = RequiredGuarantees::for_handoff(HandoffLevel::L2);
        assert_eq!(
            l2.iter().collect::<Vec<_>>(),
            vec![Guarantee::ReliableObservation]
        );
        let l3 = RequiredGuarantees::for_handoff(HandoffLevel::L3);
        assert!(l3.contains(Guarantee::SessionRecovery));
        assert!(l3.contains(Guarantee::ReliableObservation));
        // 去重：同一保证写两次只有一条。
        assert_eq!(
            RequiredGuarantees::of(&[Guarantee::Metering, Guarantee::Metering])
                .iter()
                .count(),
            1
        );
    }

    #[test]
    fn adapter_without_recovery_cannot_take_l3() {
        // Fake 的实情：recovery 声明 unsupported。
        let fake = FakeExecutor::new(FakeScenario::HappyPath);
        let enabled = Enablement::Enabled;
        let selection = select(
            &RequiredGuarantees::for_handoff(HandoffLevel::L3),
            &[Candidate {
                descriptor: fake.descriptor(),
                enablement: &enabled,
            }],
            NOW_MS,
        );
        assert!(!selection.has_candidate());
        assert_eq!(selection.filtered.len(), 1);
        let reason = &selection.filtered[0].reasons[0];
        assert_eq!(reason.code(), "capability_not_supported");
        assert!(
            reason.describe().contains("recovery"),
            "{}",
            reason.describe()
        );
        assert_eq!(selection.notice.code, "BOD_CAPABILITY_UNSATISFIED");
        assert!(
            selection
                .notice
                .missing
                .contains(&"session_recovery".to_string())
        );
        assert!(
            selection
                .notice
                .render()
                .contains("BOD_CAPABILITY_UNSATISFIED")
        );
    }

    #[test]
    fn adapter_with_recovery_can_take_l3() {
        let ready = l3_ready("adapter-ready");
        let enabled = Enablement::Enabled;
        let selection = select(
            &RequiredGuarantees::for_handoff(HandoffLevel::L3),
            &[Candidate {
                descriptor: &ready,
                enablement: &enabled,
            }],
            NOW_MS,
        );
        assert_eq!(selection.admitted, vec!["adapter-ready".to_string()]);
        assert_eq!(selection.sole_candidate(), Some("adapter-ready"));
        assert_eq!(selection.notice.code, "BOD_CAPABILITY_SATISFIED");
        // L1 不需要任何保证，同一个适配器当然也能接。
        let l1 = select(
            &RequiredGuarantees::for_handoff(HandoffLevel::L1),
            &[Candidate {
                descriptor: &ready,
                enablement: &enabled,
            }],
            NOW_MS,
        );
        assert!(l1.has_candidate());
    }

    #[test]
    fn unknown_and_undeclared_slots_are_not_supported() {
        let enabled = Enablement::Enabled;
        let undeclared = descriptor("adapter-bare", Vec::new());
        let selection = select(
            &RequiredGuarantees::of(&[Guarantee::SessionRecovery]),
            &[Candidate {
                descriptor: &undeclared,
                enablement: &enabled,
            }],
            NOW_MS,
        );
        assert_eq!(
            selection.filtered[0].reasons[0].code(),
            "capability_unknown"
        );

        let unknown = descriptor(
            "adapter-unknown",
            vec![claim(CapabilitySlot::Recovery, SupportLevel::Unknown, None)],
        );
        let selection = select(
            &RequiredGuarantees::of(&[Guarantee::SessionRecovery]),
            &[Candidate {
                descriptor: &unknown,
                enablement: &enabled,
            }],
            NOW_MS,
        );
        assert_eq!(
            selection.filtered[0].reasons[0].code(),
            "capability_unknown"
        );
    }

    #[test]
    fn stale_declaration_is_filtered() {
        let stale = descriptor(
            "adapter-stale",
            vec![claim(
                CapabilitySlot::Recovery,
                SupportLevel::Supported,
                Some(NOW_MS - 1),
            )],
        );
        let enabled = Enablement::Enabled;
        let selection = select(
            &RequiredGuarantees::of(&[Guarantee::SessionRecovery]),
            &[Candidate {
                descriptor: &stale,
                enablement: &enabled,
            }],
            NOW_MS,
        );
        assert_eq!(selection.filtered[0].reasons[0].code(), "capability_stale");
        assert!(!selection.has_candidate(), "过期声明不能当作支持");
    }

    #[test]
    fn blocked_adapter_is_filtered_even_with_full_capabilities() {
        let ready = l3_ready("adapter-blocked");
        let blocked = Enablement::Blocked {
            failures: vec![("scope-identity", "跨公司串号".to_string())],
        };
        let selection = select(
            &RequiredGuarantees::for_handoff(HandoffLevel::L3),
            &[Candidate {
                descriptor: &ready,
                enablement: &blocked,
            }],
            NOW_MS,
        );
        assert!(!selection.has_candidate());
        let reason = &selection.filtered[0].reasons[0];
        assert_eq!(reason.code(), "adapter_blocked");
        assert!(reason.describe().contains("scope-identity"));
    }

    #[test]
    fn unverified_adapter_is_filtered_only_when_guarantees_required() {
        let ready = l3_ready("adapter-unverified");
        let unverified = Enablement::Unverified {
            cases: vec!["auth-expired"],
        };
        let without = select(
            &RequiredGuarantees::none(),
            &[Candidate {
                descriptor: &ready,
                enablement: &unverified,
            }],
            NOW_MS,
        );
        assert!(without.has_candidate(), "不要求保证时允许探测");

        let with = select(
            &RequiredGuarantees::for_handoff(HandoffLevel::L3),
            &[Candidate {
                descriptor: &ready,
                enablement: &unverified,
            }],
            NOW_MS,
        );
        assert!(!with.has_candidate());
        assert_eq!(with.filtered[0].reasons[0].code(), "adapter_unverified");
    }

    #[test]
    fn notice_is_the_single_source_for_dispatch_and_ui() {
        let fake = FakeExecutor::new(FakeScenario::HappyPath);
        let enabled = Enablement::Enabled;
        let required = RequiredGuarantees::for_handoff(HandoffLevel::L3);
        let candidates = [Candidate {
            descriptor: fake.descriptor(),
            enablement: &enabled,
        }];
        let dispatch_view = select(&required, &candidates, NOW_MS);
        let ui_view = select(&required, &candidates, NOW_MS);
        // 两处读同一个结构、同一个渲染函数：不可能出现「调度说 A、界面说 B」。
        assert_eq!(dispatch_view.notice, ui_view.notice);
        assert_eq!(dispatch_view.notice.render(), ui_view.notice.render());

        let satisfied = select(
            &RequiredGuarantees::none(),
            &[Candidate {
                descriptor: fake.descriptor(),
                enablement: &enabled,
            }],
            NOW_MS,
        );
        assert!(satisfied.notice.render().contains("不要求额外执行保证"));
    }

    fn tested_combination() -> (CombinationKey, Vec<TestedCombination>) {
        let key = CombinationKey::new("runtime-a", &["cargo".to_string()], "code-change");
        let tested = vec![TestedCombination {
            key: key.clone(),
            evidence_level: EvidenceLevel::RuntimeObserved,
            source: EvidenceSource::TestResult,
            observed_at_ms: NOW_MS - 10_000,
            expires_at_ms: NOW_MS + 86_400_000,
            report_ref: "report:l3-1".to_string(),
        }];
        (key, tested)
    }

    #[test]
    fn tested_combination_does_not_override_missing_recovery() {
        let (key, tested) = tested_combination();
        let fake = FakeExecutor::new(FakeScenario::HappyPath);
        let enabled = Enablement::Enabled;
        // Fake 有完整已测组合，但声明不支持 recovery：L3 仍必须被拒。
        match admit_handoff(
            HandoffLevel::L3,
            &key,
            &tested,
            Candidate {
                descriptor: fake.descriptor(),
                enablement: &enabled,
            },
            NOW_MS,
        ) {
            Err(HandoffRefusal::Capability(selection)) => {
                assert_eq!(selection.notice.code, "BOD_CAPABILITY_UNSATISFIED");
                assert!(
                    selection
                        .filtered
                        .iter()
                        .flat_map(|item| item.reasons.iter())
                        .any(|reason| reason.code() == "capability_not_supported")
                );
            }
            other => panic!("能力不够时不得因已测组合放行，实际 {other:?}"),
        }
    }

    #[test]
    fn capability_alone_is_not_enough_without_tested_combination() {
        let ready = l3_ready("adapter-ready");
        let enabled = Enablement::Enabled;
        let key = CombinationKey::new("runtime-a", &[], "code-change");
        match admit_handoff(
            HandoffLevel::L3,
            &key,
            &[],
            Candidate {
                descriptor: &ready,
                enablement: &enabled,
            },
            NOW_MS,
        ) {
            Err(refusal @ HandoffRefusal::Untested(_)) => {
                assert_eq!(refusal.notice().code, "BOD_HANDOFF_UNTESTED");
                assert!(refusal.render().contains("L3"));
            }
            other => panic!("缺已测组合必须被拒，实际 {other:?}"),
        }
        // 能力与证据都齐时放行。
        let (key, tested) = tested_combination();
        assert_eq!(
            admit_handoff(
                HandoffLevel::L3,
                &key,
                &tested,
                Candidate {
                    descriptor: &ready,
                    enablement: &enabled,
                },
                NOW_MS,
            ),
            Ok(HandoffLevel::L3)
        );
        // L2 不需要已测组合，也不需要会话恢复。
        assert_eq!(
            admit_handoff(
                HandoffLevel::L2,
                &key,
                &[],
                Candidate {
                    descriptor: &ready,
                    enablement: &enabled,
                },
                NOW_MS,
            ),
            Ok(HandoffLevel::L2)
        );
    }

    #[test]
    fn sole_candidate_refuses_to_pick_between_two() {
        let a = l3_ready("adapter-a");
        let b = l3_ready("adapter-b");
        let enabled = Enablement::Enabled;
        let selection = select(
            &RequiredGuarantees::for_handoff(HandoffLevel::L3),
            &[
                Candidate {
                    descriptor: &a,
                    enablement: &enabled,
                },
                Candidate {
                    descriptor: &b,
                    enablement: &enabled,
                },
            ],
            NOW_MS,
        );
        assert_eq!(selection.admitted.len(), 2);
        assert_eq!(
            selection.sole_candidate(),
            None,
            "两个候选时不得静默取第一个"
        );
    }
}
