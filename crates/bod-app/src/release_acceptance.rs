//! 发布验收（P2 任务 4.5）：五项发布检查 + 日志脱敏 + 能力缺失三处一致。
//!
//! 规格要求（`conformance-toolkit`「发布验收项」）：适配器发布 SHALL 核验停止与重启意图、
//! 重放与失联、副作用与授权、账号与预算、验收证据；能力缺失 MUST 在探测结果、调度准入和
//! 用户界面一致体现，日志中不得出现凭据值。场景明确写了失败后果：发现凭据即该项不通过，
//! 适配器不得标 Supported，**报告保留脱敏后的失败证据**。
//!
//! 本模块因此只做两件事：
//!
//! 1. **逐项判定**。每项三态 `pass` / `fail` / `not_evidenced`——「没测」和「测了不过」分开，
//!    因为把未测当通过是发布验收最常见的自欺。只有六项全 `pass` 且三处一致时才
//!    `releasable`；任何一项 `fail` 或 `not_evidenced` 都 `blocked`。
//! 2. **证据不落明文**。日志扫描复用 4.2 的 `observation_redaction`，命中的行以**脱敏后**的
//!    文本进报告：报告本身要能作为失败证据被传阅，如果它带着原始凭据，验收就变成了泄漏渠道。
//!    `to_markdown` / `to_json` 只读 `CheckItem.evidence`，而那里只存脱敏后的行。
//!
//! 边界（如实记录）：
//!
//! - 本模块**不采集**证据。`ReleaseEvidence` 的每个事实都由调用方（CLI / 测试）给出，
//!   缺就是 `None`，判 `not_evidenced`。CLI 不打开业务库（P0 单库约束），所以
//!   「重启后派发意图仍在」与「账号预算可评估」在本机命令里恒为未核对——这不是实现遗漏，
//!   而是没有第二事实源可读时的正确结论。
//! - 三处一致（探测/准入/界面）只对**本次要求的保证**比对。L1 不要求额外保证时三项都为空，
//!   一致是平凡成立的，报告会写明这一点，不当作「已覆盖 L2/L3」。

use crate::adapter_conformance::Enablement;
use crate::capability_dispatch::{FilterReason, RequiredGuarantees, Selection};
use crate::conformance_report::{CapabilityVerdict, SectionedReport, json_string, utc_stamp};
use crate::observation_redaction::redact;
use std::fmt::Write as _;

/// 报告 schema；字段变化时必须换版本。
pub const RELEASE_SCHEMA: &str = "bod.release.acceptance/v1";

/// 验收证据的有效期。超过这个年龄的报告不算证据：适配器可能已经改过版本。
pub const EVIDENCE_MAX_AGE_MS: i64 = 30 * 86_400_000;

/// 发布验收项。顺序即报告顺序，按「能否停下 → 能否重来 → 有无越权 → 有无超额 → 证据是否在」排列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseItem {
    /// 停止与重启意图：停止被受理并确认，重启后派发意图不丢。
    StopRestart,
    /// 重放与失联：回包丢失后能对账，且声明了持久去重保留期（否则不得安全重发）。
    ReplayReconnect,
    /// 副作用与授权：副作用类别已声明，越权调用被拒。
    EffectAuthorization,
    /// 账号与预算：账号归属可核对，预算闸门能给出结论。
    AccountBudget,
    /// 验收证据：4.1 的分项报告存在、未过期、且未把适配器判为禁用。
    AcceptanceEvidence,
    /// 日志脱敏：日志里没有凭据值。
    LogRedaction,
}

impl ReleaseItem {
    pub const ALL: [Self; 6] = [
        Self::StopRestart,
        Self::ReplayReconnect,
        Self::EffectAuthorization,
        Self::AccountBudget,
        Self::AcceptanceEvidence,
        Self::LogRedaction,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StopRestart => "stop_restart",
            Self::ReplayReconnect => "replay_reconnect",
            Self::EffectAuthorization => "effect_authorization",
            Self::AccountBudget => "account_budget",
            Self::AcceptanceEvidence => "acceptance_evidence",
            Self::LogRedaction => "log_redaction",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::StopRestart => "停止与重启意图",
            Self::ReplayReconnect => "重放与失联",
            Self::EffectAuthorization => "副作用与授权",
            Self::AccountBudget => "账号与预算",
            Self::AcceptanceEvidence => "验收证据",
            Self::LogRedaction => "日志脱敏",
        }
    }

    /// 对应的规格要求名（spec delta 的 Requirement），便于外部工具对齐。
    pub const fn requirement(self) -> &'static str {
        "发布验收项"
    }
}

/// 单项结论。`not_evidenced` 刻意独立于 `fail`：一个是「没测」，一个是「测了不过」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemState {
    Passed,
    Failed { code: &'static str, detail: String },
    NotEvidenced { code: &'static str, detail: String },
}

impl ItemState {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "pass",
            Self::Failed { .. } => "fail",
            Self::NotEvidenced { .. } => "not_evidenced",
        }
    }

    pub const fn is_passed(&self) -> bool {
        matches!(self, Self::Passed)
    }

    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Passed => None,
            Self::Failed { code, .. } | Self::NotEvidenced { code, .. } => Some(*code),
        }
    }

    fn failed(code: &'static str, detail: impl Into<String>) -> Self {
        Self::Failed {
            code,
            detail: detail.into(),
        }
    }

    fn unevidenced(code: &'static str, detail: impl Into<String>) -> Self {
        Self::NotEvidenced {
            code,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckItem {
    pub item: ReleaseItem,
    pub state: ItemState,
    /// 支撑结论的事实。日志项这里是**脱敏后**的行，不是原文。
    pub evidence: Vec<String>,
}

impl CheckItem {
    fn new(item: ReleaseItem, state: ItemState) -> Self {
        Self {
            item,
            state,
            evidence: Vec::new(),
        }
    }

    fn with(mut self, line: impl Into<String>) -> Self {
        self.evidence.push(line.into());
        self
    }
}

/// 预算闸门在本作用域上的结论。`blocked` 也是**结论**：说明闸门在该账号上可评估。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetGate {
    Allowed,
    Blocked { available_micros: i64 },
}

impl BudgetGate {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Blocked { .. } => "blocked",
        }
    }
}

/// 账号与预算的探测事实：账号归属 + 闸门结论必须同时存在，缺一不可评估。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetProbe {
    pub account_scope: String,
    pub gate: BudgetGate,
}

/// 发布验收的输入事实。除了描述符的静态声明，其余每一项都要求**真实样本**；
/// `None` 一律判未取得证据，绝不补默认值。
#[derive(Debug, Clone, Default)]
pub struct ReleaseEvidence {
    pub adapter_id: String,
    pub adapter_version: String,
    pub platform: String,
    /// 描述符声明的操作（`cancel` 等）。
    pub operations: Vec<String>,
    /// 停止是否被适配器受理并确认（真实往返）。
    pub stop_confirmed: Option<bool>,
    /// 重启后仍存在的派发意图条数（落库记录，不是内存计数）。
    pub durable_intents: Option<usize>,
    /// 描述符声明的持久去重保留期；`None` 表示未声明，按 §15.4 不得安全重发。
    pub dedup_retention_ms: Option<i64>,
    /// 回包丢失后是否对账成功（真实样本）。
    pub reply_lost_reconciled: Option<bool>,
    /// 描述符声明的副作用类别。
    pub effect_class: Option<String>,
    /// 越权调用是否被拒（真实样本）。
    pub authorization_refused: Option<bool>,
    /// 账号作用域与预算闸门结论。
    pub budget: Option<BudgetProbe>,
    /// 4.1 的分项报告；发布验收的证据项就是它。
    pub report: Option<SectionedReport>,
    /// 适配器日志行。
    pub log_lines: Vec<String>,
}

/// 三处之一的名称。分歧必须指名道姓，否则修不动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Probe,
    Admission,
    Interface,
}

impl Place {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Admission => "admission",
            Self::Interface => "interface",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// 探测说该能力不可用，准入却放行了这个适配器。
    ProbeSaysMissingAdmissionAdmits { slot: String },
    /// 探测没报缺失，准入却按该槽拒绝了适配器：报告与描述符不同步。
    AdmissionRejectsSlotProbeAccepts { slot: String },
    /// 准入按该槽拒绝，界面提示却没有列出这项缺失。
    InterfaceOmitsMissing { slot: String },
    /// 适配器既不在放行列表也不在过滤列表里：它压根没被评估。
    AdapterAbsentFromSelection,
}

impl Divergence {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ProbeSaysMissingAdmissionAdmits { .. } => "probe_missing_admission_admits",
            Self::AdmissionRejectsSlotProbeAccepts { .. } => "admission_rejects_slot_probe_accepts",
            Self::InterfaceOmitsMissing { .. } => "interface_omits_missing",
            Self::AdapterAbsentFromSelection => "adapter_absent_from_selection",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::ProbeSaysMissingAdmissionAdmits { slot } => {
                format!("探测把 {slot} 判为不可用，调度准入却放行了该适配器")
            }
            Self::AdmissionRejectsSlotProbeAccepts { slot } => {
                format!("探测未报 {slot} 缺失，准入却按该能力拒绝：报告与描述符不同步")
            }
            Self::InterfaceOmitsMissing { slot } => {
                format!("准入按 {slot} 拒绝，界面提示没有列出该缺失")
            }
            Self::AdapterAbsentFromSelection => {
                "适配器既未被放行也未被过滤：本次选择没有评估它".to_string()
            }
        }
    }
}

/// 探测、准入、界面三处的缺失集合与分歧。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Alignment {
    /// 本次要求的保证槽（比较范围）。
    pub required_slots: Vec<String>,
    /// 探测结果里这些槽中判为不可用的。
    pub probe_missing: Vec<String>,
    /// 调度准入按这些槽拒绝了该适配器。
    pub admission_missing: Vec<String>,
    /// 界面提示里列出的缺失。
    pub interface_missing: Vec<String>,
    pub divergences: Vec<Divergence>,
    /// 是否真的做了比对（没有报告或没有选择时无从比）。
    pub compared: bool,
}

impl Alignment {
    pub fn is_clean(&self) -> bool {
        self.divergences.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseVerdict {
    /// 六项全 pass 且三处无分歧：可以按 Supported 发布。
    Releasable,
    /// 任一项 fail / not_evidenced，或三处不一致：不得标 Supported。
    Blocked,
}

impl ReleaseVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Releasable => "releasable",
            Self::Blocked => "blocked",
        }
    }

    pub const fn is_releasable(self) -> bool {
        matches!(self, Self::Releasable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAcceptance {
    pub schema: String,
    pub adapter_id: String,
    pub adapter_version: String,
    pub platform: String,
    pub generated_at_ms: i64,
    pub checks: Vec<CheckItem>,
    pub alignment: Alignment,
    pub limitations: Vec<String>,
}

impl ReleaseAcceptance {
    /// 逐项判定。`required` / `selection` 只用于三处一致；不传就是「没有可比的准入结论」。
    pub fn evaluate(
        evidence: &ReleaseEvidence,
        required: &RequiredGuarantees,
        selection: Option<&Selection>,
        now_ms: i64,
    ) -> Self {
        let checks = vec![
            check_stop_restart(evidence),
            check_replay(evidence),
            check_effect_authorization(evidence),
            check_account_budget(evidence),
            check_acceptance_evidence(evidence, now_ms),
            check_log_redaction(evidence),
        ];
        let alignment = align(evidence.report.as_ref(), required, selection);

        let mut limitations = Vec::new();
        if required.is_empty() {
            limitations.push(
                "未指定接续等级：三处一致只覆盖「无额外保证」的场景，L2/L3 的真实过滤结果未比对"
                    .to_string(),
            );
        }
        if selection.is_none() {
            limitations
                .push("未提供调度准入结论：探测结果与界面提示无法比对，只核验了探测侧".to_string());
        }
        if !alignment.compared {
            limitations.push("没有可比对的对象，三处一致结论不成立（不视为通过）".to_string());
        }
        limitations.push(
            "验收证据由调用方提供；本模块不采集事实，缺失一律记 not_evidenced 而非补齐".to_string(),
        );

        Self {
            schema: RELEASE_SCHEMA.to_string(),
            adapter_id: evidence.adapter_id.clone(),
            adapter_version: evidence.adapter_version.clone(),
            platform: evidence.platform.clone(),
            generated_at_ms: now_ms,
            checks,
            alignment,
            limitations,
        }
    }

    /// 唯一结论：六项全 pass 且三处无分歧才 releasable。
    pub fn verdict(&self) -> ReleaseVerdict {
        let all_passed = self.checks.iter().all(|check| check.state.is_passed());
        if all_passed && self.alignment.is_clean() {
            ReleaseVerdict::Releasable
        } else {
            ReleaseVerdict::Blocked
        }
    }

    /// 「能否标 Supported」与结论同源，不给第二处判断留口子。
    pub fn can_mark_supported(&self) -> bool {
        self.verdict().is_releasable()
    }

    pub fn blocking(&self) -> Vec<&CheckItem> {
        self.checks
            .iter()
            .filter(|check| !check.state.is_passed())
            .collect()
    }

    pub fn item(&self, item: ReleaseItem) -> Option<&CheckItem> {
        self.checks.iter().find(|check| check.item == item)
    }

    /// 结论一句话：把阻塞项点名，不让读者只看 verdict 猜原因。
    pub fn verdict_statement(&self) -> String {
        let verdict = self.verdict();
        if verdict.is_releasable() {
            return "releasable（六项全部有证据且通过，三处一致无分歧）".to_string();
        }
        let mut parts: Vec<String> = self
            .blocking()
            .iter()
            .map(|check| {
                format!(
                    "{}={}",
                    check.item.as_str(),
                    check.state.code().unwrap_or("unknown")
                )
            })
            .collect();
        for divergence in &self.alignment.divergences {
            parts.push(format!("alignment={}", divergence.code()));
        }
        format!("blocked（{}；不得标 Supported）", parts.join("，"))
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "### 发布验收：{} v{}（{}）\n",
            self.adapter_id, self.adapter_version, self.platform
        );
        let _ = writeln!(
            out,
            "- 结论: {}",
            match self.verdict() {
                ReleaseVerdict::Releasable => "releasable",
                ReleaseVerdict::Blocked => "blocked",
            }
        );
        let _ = writeln!(out, "- 判定: {}", self.verdict_statement());
        let _ = writeln!(out, "- 生成时间: {}", utc_stamp(self.generated_at_ms));
        let _ = writeln!(
            out,
            "\n| 验收项 | 要求 | 结论 | 原因码 | 证据 |\n|---|---|---|---|---|"
        );
        for check in &self.checks {
            let detail = match &check.state {
                ItemState::Passed => "—".to_string(),
                ItemState::Failed { detail, .. } | ItemState::NotEvidenced { detail, .. } => {
                    detail.replace('|', "/")
                }
            };
            let evidence = if check.evidence.is_empty() {
                "—".to_string()
            } else {
                check
                    .evidence
                    .iter()
                    .map(|line| line.replace('|', "/"))
                    .collect::<Vec<_>>()
                    .join("；")
            };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                check.item.as_str(),
                check.item.requirement(),
                check.state.as_str(),
                check.state.code().unwrap_or("—"),
                format!("{detail} {evidence}").trim()
            );
        }

        let _ = writeln!(
            out,
            "\n**能力缺失三处一致**（比较范围: {}）",
            {
                if self.alignment.required_slots.is_empty() {
                    "无额外保证".to_string()
                } else {
                    self.alignment.required_slots.join(", ")
                }
            }
        );
        if !self.alignment.compared {
            let _ = writeln!(out, "- 未比对（缺少探测报告或准入结论）");
        } else if self.alignment.is_clean() {
            let _ = writeln!(
                out,
                "- 一致：探测 [{}] / 准入 [{}] / 界面 [{}]",
                self.alignment.probe_missing.join(", "),
                self.alignment.admission_missing.join(", "),
                self.alignment.interface_missing.join(", ")
            );
        } else {
            for divergence in &self.alignment.divergences {
                let _ = writeln!(
                    out,
                    "- 分歧 {}：{}",
                    divergence.code(),
                    divergence.describe()
                );
            }
        }

        if !self.limitations.is_empty() {
            let _ = writeln!(out, "\n局限：");
            for note in &self.limitations {
                let _ = writeln!(out, "- {note}");
            }
        }
        out
    }

    pub fn to_json(&self) -> String {
        let mut out = String::from("{\n");
        let _ = writeln!(out, "  \"schema\": {},", json_string(&self.schema));
        let _ = writeln!(
            out,
            "  \"adapter\": {{\"id\": {}, \"version\": {}, \"platform\": {}}},",
            json_string(&self.adapter_id),
            json_string(&self.adapter_version),
            json_string(&self.platform)
        );
        let _ = writeln!(
            out,
            "  \"generated_at\": {},",
            json_string(&utc_stamp(self.generated_at_ms))
        );
        let _ = writeln!(
            out,
            "  \"verdict\": {{\"releasable\": {}, \"statement\": {}}},",
            self.verdict().is_releasable(),
            json_string(&self.verdict_statement())
        );

        let _ = writeln!(out, "  \"checks\": [");
        for (index, check) in self.checks.iter().enumerate() {
            let comma = if index + 1 == self.checks.len() {
                ""
            } else {
                ","
            };
            let (detail, code) = match &check.state {
                ItemState::Passed => (String::new(), "null".to_string()),
                ItemState::Failed { code, detail } | ItemState::NotEvidenced { code, detail } => {
                    (detail.clone(), json_string(code))
                }
            };
            let _ = writeln!(
                out,
                "    {{\"item\": {}, \"requirement\": {}, \"state\": {}, \"code\": {}, \"detail\": {}, \"evidence\": [{}]}}{comma}",
                json_string(check.item.as_str()),
                json_string(check.item.requirement()),
                json_string(check.state.as_str()),
                code,
                json_string(&detail),
                check
                    .evidence
                    .iter()
                    .map(|line| json_string(line))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let _ = writeln!(out, "  ],");

        let _ = writeln!(
            out,
            "  \"alignment\": {{\"compared\": {}, \"required\": [{}], \"probe\": [{}], \"admission\": [{}], \"interface\": [{}], \"divergences\": [{}]}},",
            self.alignment.compared,
            join_json(&self.alignment.required_slots),
            join_json(&self.alignment.probe_missing),
            join_json(&self.alignment.admission_missing),
            join_json(&self.alignment.interface_missing),
            self.alignment
                .divergences
                .iter()
                .map(|item| format!(
                    "{{\"code\": {}, \"detail\": {}}}",
                    json_string(item.code()),
                    json_string(&item.describe())
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(out, "  \"limitations\": [{}]", join_json(&self.limitations));
        out.push_str("}\n");
        out
    }
}

fn join_json(values: &[String]) -> String {
    values
        .iter()
        .map(|value| json_string(value))
        .collect::<Vec<_>>()
        .join(", ")
}

fn check_stop_restart(evidence: &ReleaseEvidence) -> CheckItem {
    let item = ReleaseItem::StopRestart;
    if !evidence.operations.iter().any(|op| op == "cancel") {
        return CheckItem::new(
            item,
            ItemState::failed(
                "no_cancel_operation",
                "描述符没有声明 cancel：没有停止入口，谈不上重启意图",
            ),
        );
    }
    if evidence.stop_confirmed == Some(false) {
        return CheckItem::new(
            item,
            ItemState::failed("stop_not_confirmed", "停止请求未被确认：子执行可能仍在运行"),
        )
        .with("cancel 往返返回未确认");
    }
    let Some(true) = evidence.stop_confirmed else {
        return CheckItem::new(
            item,
            ItemState::unevidenced("stop_unchecked", "没有真实的停止往返样本"),
        );
    };

    match evidence.durable_intents {
        Some(count) if count > 0 => CheckItem::new(item, ItemState::Passed)
            .with(format!("停止已确认；重启后仍有 {count} 条派发意图记录")),
        Some(_) => CheckItem::new(
            item,
            ItemState::unevidenced(
                "no_durable_intent_sample",
                "停止已确认，但库里没有派发意图记录可用来核对重启语义",
            ),
        ),
        None => CheckItem::new(
            item,
            ItemState::unevidenced(
                "restart_intent_unchecked",
                "停止已确认；重启后意图是否仍在未核对（需要读业务库的派发意图）",
            ),
        ),
    }
}

fn check_replay(evidence: &ReleaseEvidence) -> CheckItem {
    let item = ReleaseItem::ReplayReconnect;
    let Some(retention) = evidence.dedup_retention_ms else {
        return CheckItem::new(
            item,
            ItemState::failed(
                "no_dedup_retention",
                "未声明持久去重保留期：宿主不得据此安全重发（§15.4）",
            ),
        );
    };
    match evidence.reply_lost_reconciled {
        Some(true) => CheckItem::new(item, ItemState::Passed)
            .with(format!("去重保留 {retention}ms；回包丢失后对账成功")),
        Some(false) => CheckItem::new(
            item,
            ItemState::failed("reply_lost_unreconciled", "回包丢失后对账失败"),
        )
        .with(format!("去重保留 {retention}ms")),
        None => CheckItem::new(
            item,
            ItemState::unevidenced(
                "replay_unchecked",
                "没有回包丢失样本：失联后的重放/对账行为未知",
            ),
        )
        .with(format!("去重保留 {retention}ms")),
    }
}

fn check_effect_authorization(evidence: &ReleaseEvidence) -> CheckItem {
    let item = ReleaseItem::EffectAuthorization;
    let Some(effect) = evidence.effect_class.as_deref() else {
        return CheckItem::new(
            item,
            ItemState::failed("no_effect_class", "描述符没有声明副作用类别"),
        );
    };
    match evidence.authorization_refused {
        Some(true) => CheckItem::new(item, ItemState::Passed)
            .with(format!("副作用类别 {effect}；越权调用被拒")),
        Some(false) => CheckItem::new(
            item,
            ItemState::failed("authorization_not_enforced", "越权调用未被拒绝"),
        )
        .with(format!("副作用类别 {effect}")),
        None => CheckItem::new(
            item,
            ItemState::unevidenced("authorization_unchecked", "没有越权调用样本"),
        )
        .with(format!("副作用类别 {effect}")),
    }
}

fn check_account_budget(evidence: &ReleaseEvidence) -> CheckItem {
    let item = ReleaseItem::AccountBudget;
    let Some(probe) = evidence.budget.as_ref() else {
        return CheckItem::new(
            item,
            ItemState::unevidenced(
                "budget_unchecked",
                "未核对账号归属与预算闸门（需要账号作用域与可用额）",
            ),
        );
    };
    if probe.account_scope.trim().is_empty() {
        return CheckItem::new(
            item,
            ItemState::failed("no_account_scope", "账号作用域为空：无法判断花的是谁的额度"),
        );
    }
    match &probe.gate {
        BudgetGate::Allowed => CheckItem::new(item, ItemState::Passed)
            .with(format!("账号 {}；预算闸门允许新派发", probe.account_scope)),
        BudgetGate::Blocked { available_micros } => {
            CheckItem::new(item, ItemState::Passed).with(format!(
                "账号 {}；闸门拒绝新派发（可用额 {available_micros} 微元，环境事实不计适配器缺陷）",
                probe.account_scope
            ))
        }
    }
}

fn check_acceptance_evidence(evidence: &ReleaseEvidence, now_ms: i64) -> CheckItem {
    let item = ReleaseItem::AcceptanceEvidence;
    let Some(report) = evidence.report.as_ref() else {
        return CheckItem::new(
            item,
            ItemState::unevidenced("no_report", "没有分项符合性报告（4.1）作为验收证据"),
        );
    };
    if let Enablement::Blocked { failures } = &report.enablement {
        return CheckItem::new(
            item,
            ItemState::failed(
                "adapter_blocked",
                format!("闸门类用例失败，适配器已被禁用（{} 项）", failures.len()),
            ),
        )
        .with(format!("报告生成于 {}", utc_stamp(report.generated_at_ms)));
    }
    let age = now_ms.saturating_sub(report.generated_at_ms);
    if age > EVIDENCE_MAX_AGE_MS {
        return CheckItem::new(
            item,
            ItemState::unevidenced(
                "stale_report",
                format!(
                    "报告生成于 {}，超过 {} 天有效期，不能作为当前版本的证据",
                    utc_stamp(report.generated_at_ms),
                    EVIDENCE_MAX_AGE_MS / 86_400_000
                ),
            ),
        );
    }
    let (pass, fail, na) = (
        report
            .cases
            .iter()
            .filter(|c| {
                matches!(
                    c.status,
                    crate::adapter_conformance::CaseStatus::Pass { .. }
                )
            })
            .count(),
        report
            .cases
            .iter()
            .filter(|c| {
                matches!(
                    c.status,
                    crate::adapter_conformance::CaseStatus::Fail { .. }
                )
            })
            .count(),
        report
            .cases
            .iter()
            .filter(|c| {
                matches!(
                    c.status,
                    crate::adapter_conformance::CaseStatus::NotApplicable { .. }
                )
            })
            .count(),
    );
    CheckItem::new(item, ItemState::Passed)
        .with(format!("报告生成于 {}", utc_stamp(report.generated_at_ms)))
        .with(format!("用例 pass={pass} fail={fail} not_applicable={na}"))
        .with(report.enablement_note())
}

fn check_log_redaction(evidence: &ReleaseEvidence) -> CheckItem {
    let item = ReleaseItem::LogRedaction;
    if evidence.log_lines.is_empty() {
        return CheckItem::new(item, ItemState::unevidenced("no_logs", "没有提供日志样本"));
    }
    let mut hits = 0usize;
    let mut kinds: Vec<&'static str> = Vec::new();
    let mut redacted_lines: Vec<String> = Vec::new();
    for (index, line) in evidence.log_lines.iter().enumerate() {
        let (clean, redaction) = redact(line);
        if redaction.is_empty() {
            continue;
        }
        hits += redaction.replaced;
        for kind in redaction.kinds_unique() {
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        // 只保留脱敏后的行：报告要能被传阅，带着原文就是把验收变成泄漏渠道。
        redacted_lines.push(format!("第 {} 行: {clean}", index + 1));
    }
    if hits == 0 {
        return CheckItem::new(item, ItemState::Passed).with(format!(
            "扫描 {} 行，未发现凭据形态",
            evidence.log_lines.len()
        ));
    }
    let mut check = CheckItem::new(
        item,
        ItemState::failed(
            "credentials_in_logs",
            format!(
                "{} 行日志里出现 {hits} 处凭据值（{}）",
                redacted_lines.len(),
                kinds.join(", ")
            ),
        ),
    );
    for line in redacted_lines {
        check = check.with(line);
    }
    check
}

/// 探测 / 准入 / 界面三处比对。只比**本次要求的保证**：L1 不要求额外保证时三者都为空，
/// 那是平凡一致，报告会写明未覆盖 L2/L3。
pub fn align(
    report: Option<&SectionedReport>,
    required: &RequiredGuarantees,
    selection: Option<&Selection>,
) -> Alignment {
    let required_slots: Vec<String> = required
        .iter()
        .map(|guarantee| guarantee.slot().as_str().to_string())
        .collect();
    let mut alignment = Alignment {
        required_slots: required_slots.clone(),
        ..Alignment::default()
    };
    if required_slots.is_empty() {
        alignment.compared = report.is_some() && selection.is_some();
        return alignment;
    }
    let (Some(report), Some(selection)) = (report, selection) else {
        // 只有一侧时不比：单侧结论不能证明一致。
        return alignment;
    };
    alignment.compared = true;

    for slot in &required_slots {
        match report.capability(slot) {
            Some(row) if !capability_missing(&row.verdict) => {}
            // 报告没有这个槽，等同于声明缺失。
            Some(_) | None => alignment.probe_missing.push(slot.clone()),
        }
    }

    let admitted = selection.admitted.iter().any(|id| id == &report.adapter_id);
    if let Some(filtered) = selection
        .filtered
        .iter()
        .find(|item| item.adapter_id == report.adapter_id)
    {
        for reason in &filtered.reasons {
            if let Some(slot) = reason_slot(reason) {
                alignment.admission_missing.push(slot.to_string());
            }
        }
    } else if !admitted {
        alignment
            .divergences
            .push(Divergence::AdapterAbsentFromSelection);
        return alignment;
    }

    for guarantee in required.iter() {
        let label = guarantee.as_str();
        let slot = guarantee.slot();
        if selection.notice.missing.iter().any(|item| item == label) {
            alignment.interface_missing.push(slot.as_str().to_string());
        }
    }

    for slot in &alignment.probe_missing {
        if admitted {
            alignment
                .divergences
                .push(Divergence::ProbeSaysMissingAdmissionAdmits { slot: slot.clone() });
        }
    }
    for slot in &alignment.admission_missing {
        if !alignment.probe_missing.contains(slot) {
            alignment
                .divergences
                .push(Divergence::AdmissionRejectsSlotProbeAccepts { slot: slot.clone() });
        }
        if !alignment.interface_missing.contains(slot) {
            alignment
                .divergences
                .push(Divergence::InterfaceOmitsMissing { slot: slot.clone() });
        }
    }
    alignment
}

fn capability_missing(verdict: &CapabilityVerdict) -> bool {
    matches!(
        verdict,
        CapabilityVerdict::CasesNotPassing { .. }
            | CapabilityVerdict::DeclaredUnsupported
            | CapabilityVerdict::Unknown
            | CapabilityVerdict::NotDeclared
            | CapabilityVerdict::Stale { .. }
    )
}

/// 只取**能力槽**级的原因：适配器被禁用/未取得证据是适配器级结论，不参与槽比对。
fn reason_slot(reason: &FilterReason) -> Option<&str> {
    match reason {
        FilterReason::NotSupported { slot }
        | FilterReason::Unknown { slot }
        | FilterReason::Stale { slot, .. } => Some(slot),
        FilterReason::AdapterBlocked { .. } | FilterReason::AdapterUnverified { .. } => None,
    }
}

/// 便捷入口：把「要求哪些保证」翻译成槽名，供调用方打印比较范围。
pub fn required_slot_labels(required: &RequiredGuarantees) -> Vec<String> {
    required
        .iter()
        .map(|item| item.slot().as_str().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_conformance::CaseOutcome;
    use crate::capability_dispatch::{CapabilityNotice, Filtered};
    use crate::conformance_report::{CapabilityRow, REPORT_SCHEMA};
    use crate::domain::execution::{PROTOCOL_ID, SupportLevel};

    const NOW: i64 = 1_763_000_000_000;

    /// 失败注入用例：(验收项, 改坏证据的方式, 期望的原因码)。
    type FailureCase = (ReleaseItem, Box<dyn Fn(&mut ReleaseEvidence)>, &'static str);

    fn row(slot: &str, verdict: CapabilityVerdict) -> CapabilityRow {
        CapabilityRow {
            slot: slot.to_string(),
            level: SupportLevel::Unsupported,
            mode: None,
            evidence: None,
            evidence_level: None,
            observed_at_ms: None,
            expires_at_ms: None,
            verdict,
            cases: Vec::new(),
            note: String::new(),
        }
    }

    fn report(
        rows: Vec<CapabilityRow>,
        enablement: Enablement,
        generated_at_ms: i64,
    ) -> SectionedReport {
        SectionedReport {
            schema: REPORT_SCHEMA.to_string(),
            adapter_id: "adapter-x".to_string(),
            adapter_version: "1.0.0".to_string(),
            protocol: PROTOCOL_ID.to_string(),
            platform: "test".to_string(),
            generated_at_ms,
            capabilities: rows,
            cases: vec![CaseOutcome {
                id: "scope-identity",
                requirement: "必需用例：范围身份",
                status: crate::adapter_conformance::CaseStatus::Pass { assertions: 2 },
            }],
            inconsistencies: Vec::new(),
            limitations: Vec::new(),
            enablement,
        }
    }

    /// 全项有证据且通过的样本。任何一项失败都必须能从这里被推翻，才说明闸门不是死的。
    fn passing_evidence() -> ReleaseEvidence {
        ReleaseEvidence {
            adapter_id: "adapter-x".to_string(),
            adapter_version: "1.0.0".to_string(),
            platform: "test".to_string(),
            operations: vec!["describe".into(), "start".into(), "cancel".into()],
            stop_confirmed: Some(true),
            durable_intents: Some(3),
            dedup_retention_ms: Some(86_400_000),
            reply_lost_reconciled: Some(true),
            effect_class: Some("process_execution".to_string()),
            authorization_refused: Some(true),
            budget: Some(BudgetProbe {
                account_scope: "company-1/account-a".to_string(),
                gate: BudgetGate::Allowed,
            }),
            report: Some(report(
                vec![
                    row(
                        "recovery",
                        CapabilityVerdict::Verified {
                            cases: vec!["resume-round-trip".to_string()],
                        },
                    ),
                    row(
                        "reliable_observation",
                        CapabilityVerdict::Verified {
                            cases: vec!["observation-ack".to_string()],
                        },
                    ),
                ],
                Enablement::Enabled,
                NOW,
            )),
            log_lines: vec!["boot ok".to_string()],
        }
    }

    fn required_l3() -> RequiredGuarantees {
        RequiredGuarantees::for_handoff(crate::handoff_package::HandoffLevel::L3)
    }

    fn selection_admitting() -> Selection {
        Selection {
            admitted: vec!["adapter-x".to_string()],
            filtered: Vec::new(),
            notice: CapabilityNotice::satisfied(&required_l3(), 1),
        }
    }

    #[test]
    fn all_items_passing_is_releasable_and_can_mark_supported() {
        let required = required_l3();
        let selection = selection_admitting();
        let acceptance =
            ReleaseAcceptance::evaluate(&passing_evidence(), &required, Some(&selection), NOW);
        assert_eq!(acceptance.verdict(), ReleaseVerdict::Releasable);
        assert!(acceptance.can_mark_supported());
        assert!(acceptance.blocking().is_empty());
        assert_eq!(acceptance.checks.len(), ReleaseItem::ALL.len());
    }

    #[test]
    fn missing_evidence_is_never_treated_as_pass() {
        let mut evidence = passing_evidence();
        evidence.stop_confirmed = Some(true);
        evidence.durable_intents = None;
        let acceptance = ReleaseAcceptance::evaluate(
            &evidence,
            &required_l3(),
            Some(&selection_admitting()),
            NOW,
        );
        let item = acceptance.item(ReleaseItem::StopRestart).unwrap();
        assert_eq!(item.state.as_str(), "not_evidenced");
        assert_eq!(item.state.code(), Some("restart_intent_unchecked"));
        assert!(!acceptance.can_mark_supported());
    }

    #[test]
    fn each_failing_item_blocks_on_its_own() {
        let cases: Vec<FailureCase> = vec![
            (
                ReleaseItem::StopRestart,
                Box::new(|e: &mut ReleaseEvidence| e.stop_confirmed = Some(false)),
                "stop_not_confirmed",
            ),
            (
                ReleaseItem::StopRestart,
                Box::new(|e: &mut ReleaseEvidence| e.operations.retain(|op| op != "cancel")),
                "no_cancel_operation",
            ),
            (
                ReleaseItem::ReplayReconnect,
                Box::new(|e: &mut ReleaseEvidence| e.dedup_retention_ms = None),
                "no_dedup_retention",
            ),
            (
                ReleaseItem::ReplayReconnect,
                Box::new(|e: &mut ReleaseEvidence| e.reply_lost_reconciled = Some(false)),
                "reply_lost_unreconciled",
            ),
            (
                ReleaseItem::EffectAuthorization,
                Box::new(|e: &mut ReleaseEvidence| e.effect_class = None),
                "no_effect_class",
            ),
            (
                ReleaseItem::EffectAuthorization,
                Box::new(|e: &mut ReleaseEvidence| e.authorization_refused = Some(false)),
                "authorization_not_enforced",
            ),
            (
                ReleaseItem::AcceptanceEvidence,
                Box::new(|e: &mut ReleaseEvidence| e.report = None),
                "no_report",
            ),
            (
                ReleaseItem::AccountBudget,
                Box::new(|e: &mut ReleaseEvidence| {
                    e.budget = Some(BudgetProbe {
                        account_scope: "  ".to_string(),
                        gate: BudgetGate::Allowed,
                    })
                }),
                "no_account_scope",
            ),
        ];
        for (item, mutate, code) in cases {
            let mut evidence = passing_evidence();
            mutate(&mut evidence);
            let acceptance = ReleaseAcceptance::evaluate(
                &evidence,
                &required_l3(),
                Some(&selection_admitting()),
                NOW,
            );
            assert_eq!(
                acceptance.verdict(),
                ReleaseVerdict::Blocked,
                "{code} 应阻断"
            );
            let check = acceptance.item(item).unwrap();
            assert_eq!(check.state.code(), Some(code), "{code} 原因码不符");
            assert!(!acceptance.can_mark_supported());
        }
    }

    #[test]
    fn credentials_in_logs_fail_without_leaking_the_value() {
        let mut evidence = passing_evidence();
        evidence.log_lines = vec![
            "starting".to_string(),
            "Authorization: Bearer sk-live-abcdefghijklmnopqrstuvwxyz012345".to_string(),
            "\"api_key\": \"AKIAIOSFODNN7EXAMPLE\"".to_string(),
        ];
        let acceptance = ReleaseAcceptance::evaluate(
            &evidence,
            &required_l3(),
            Some(&selection_admitting()),
            NOW,
        );
        let check = acceptance.item(ReleaseItem::LogRedaction).unwrap();
        assert_eq!(check.state.as_str(), "fail");
        assert_eq!(check.state.code(), Some("credentials_in_logs"));
        assert!(!acceptance.can_mark_supported());

        let markdown = acceptance.to_markdown();
        let json = acceptance.to_json();
        for secret in [
            "sk-live-abcdefghijklmnopqrstuvwxyz012345",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert!(!markdown.contains(secret), "markdown 泄漏了 {secret}");
            assert!(!json.contains(secret), "json 泄漏了 {secret}");
        }
        // 证据仍然保留：脱敏后的行让人知道「这里本来有个秘密」。
        assert!(markdown.contains(crate::observation_redaction::REDACTED_PREFIX));
        assert!(json.contains("redacted"));
    }

    #[test]
    fn clean_logs_pass_with_scanned_line_count() {
        let acceptance = ReleaseAcceptance::evaluate(
            &passing_evidence(),
            &required_l3(),
            Some(&selection_admitting()),
            NOW,
        );
        let check = acceptance.item(ReleaseItem::LogRedaction).unwrap();
        assert!(check.state.is_passed());
        assert!(check.evidence[0].contains("扫描 1 行"));
    }

    #[test]
    fn empty_logs_are_unevidenced_not_passing() {
        let mut evidence = passing_evidence();
        evidence.log_lines.clear();
        let acceptance = ReleaseAcceptance::evaluate(
            &evidence,
            &required_l3(),
            Some(&selection_admitting()),
            NOW,
        );
        let check = acceptance.item(ReleaseItem::LogRedaction).unwrap();
        assert_eq!(check.state.as_str(), "not_evidenced");
        assert_eq!(check.state.code(), Some("no_logs"));
    }

    #[test]
    fn stale_or_blocked_report_is_not_acceptance_evidence() {
        let mut stale = passing_evidence();
        stale.report = Some(report(
            Vec::new(),
            Enablement::Enabled,
            NOW - EVIDENCE_MAX_AGE_MS - 1,
        ));
        let acceptance =
            ReleaseAcceptance::evaluate(&stale, &required_l3(), Some(&selection_admitting()), NOW);
        assert_eq!(
            acceptance
                .item(ReleaseItem::AcceptanceEvidence)
                .unwrap()
                .state
                .code(),
            Some("stale_report")
        );

        let mut blocked = passing_evidence();
        blocked.report = Some(report(
            Vec::new(),
            Enablement::Blocked {
                failures: vec![("scope-identity", "跨公司返回别家执行".to_string())],
            },
            NOW,
        ));
        let acceptance = ReleaseAcceptance::evaluate(
            &blocked,
            &required_l3(),
            Some(&selection_admitting()),
            NOW,
        );
        let check = acceptance.item(ReleaseItem::AcceptanceEvidence).unwrap();
        assert_eq!(check.state.code(), Some("adapter_blocked"));
        assert_eq!(acceptance.verdict(), ReleaseVerdict::Blocked);
    }

    #[test]
    fn blocked_budget_is_still_a_conclusion_not_a_defect() {
        let mut evidence = passing_evidence();
        evidence.budget = Some(BudgetProbe {
            account_scope: "company-1/account-a".to_string(),
            gate: BudgetGate::Blocked {
                available_micros: 0,
            },
        });
        let acceptance = ReleaseAcceptance::evaluate(
            &evidence,
            &required_l3(),
            Some(&selection_admitting()),
            NOW,
        );
        let check = acceptance.item(ReleaseItem::AccountBudget).unwrap();
        assert!(check.state.is_passed());
        assert!(check.evidence[0].contains("环境事实不计适配器缺陷"));
    }

    #[test]
    fn probe_and_admission_agreeing_is_clean() {
        let required = required_l3();
        let report = report(
            vec![
                row("recovery", CapabilityVerdict::DeclaredUnsupported),
                row(
                    "reliable_observation",
                    CapabilityVerdict::DeclaredUnsupported,
                ),
            ],
            Enablement::Enabled,
            NOW,
        );
        let filtered = vec![Filtered {
            adapter_id: "adapter-x".to_string(),
            reasons: vec![
                FilterReason::NotSupported {
                    slot: "recovery".to_string(),
                },
                FilterReason::NotSupported {
                    slot: "reliable_observation".to_string(),
                },
            ],
        }];
        let selection = Selection {
            admitted: Vec::new(),
            filtered: filtered.clone(),
            notice: CapabilityNotice::unsatisfied(&required, &filtered),
        };
        let alignment = align(Some(&report), &required, Some(&selection));
        assert!(alignment.compared);
        assert!(alignment.is_clean(), "{:?}", alignment.divergences);
        assert_eq!(
            alignment.probe_missing,
            vec!["recovery".to_string(), "reliable_observation".to_string()]
        );
        assert_eq!(alignment.admission_missing, alignment.probe_missing);
        assert_eq!(alignment.interface_missing, alignment.probe_missing);
    }

    #[test]
    fn probe_saying_missing_while_admission_admits_is_a_divergence() {
        let required = required_l3();
        let report = report(
            vec![row(
                "recovery",
                CapabilityVerdict::CasesNotPassing {
                    cases: vec!["cancel-request-vs-confirm".to_string()],
                    worst: "fail".to_string(),
                },
            )],
            Enablement::Enabled,
            NOW,
        );
        // 4.3 的 select 只看声明等级，不看用例结果：这正是报告与准入会分叉的地方。
        let selection = selection_admitting();
        let alignment = align(Some(&report), &required, Some(&selection));
        assert!(
            alignment
                .divergences
                .iter()
                .any(|item| matches!(item, Divergence::ProbeSaysMissingAdmissionAdmits { slot } if slot == "recovery")),
            "{:?}",
            alignment.divergences
        );
        let mut evidence = passing_evidence();
        evidence.report = Some(report);
        let acceptance = ReleaseAcceptance::evaluate(&evidence, &required, Some(&selection), NOW);
        assert_eq!(acceptance.verdict(), ReleaseVerdict::Blocked);
        assert!(acceptance.verdict_statement().contains("alignment="));
    }

    #[test]
    fn admission_rejecting_a_slot_probe_calls_usable_is_a_divergence() {
        let required = required_l3();
        let report = report(
            vec![
                row(
                    "recovery",
                    CapabilityVerdict::Verified {
                        cases: vec!["cancel-request-vs-confirm".to_string()],
                    },
                ),
                row(
                    "reliable_observation",
                    CapabilityVerdict::DeclaredUnsupported,
                ),
            ],
            Enablement::Enabled,
            NOW,
        );
        let filtered = vec![Filtered {
            adapter_id: "adapter-x".to_string(),
            reasons: vec![FilterReason::NotSupported {
                slot: "recovery".to_string(),
            }],
        }];
        let selection = Selection {
            admitted: Vec::new(),
            filtered: filtered.clone(),
            notice: CapabilityNotice::unsatisfied(&required, &filtered),
        };
        let alignment = align(Some(&report), &required, Some(&selection));
        assert!(
            alignment
                .divergences
                .iter()
                .any(|item| matches!(item, Divergence::AdmissionRejectsSlotProbeAccepts { slot } if slot == "recovery")),
            "{:?}",
            alignment.divergences
        );
    }

    #[test]
    fn interface_omitting_a_missing_capability_is_a_divergence() {
        let required = required_l3();
        let report = report(
            vec![
                row("recovery", CapabilityVerdict::DeclaredUnsupported),
                row(
                    "reliable_observation",
                    CapabilityVerdict::DeclaredUnsupported,
                ),
            ],
            Enablement::Enabled,
            NOW,
        );
        let filtered = vec![Filtered {
            adapter_id: "adapter-x".to_string(),
            reasons: vec![FilterReason::NotSupported {
                slot: "recovery".to_string(),
            }],
        }];
        // 手写一个「说没有候选、但没列缺失」的提示：界面与准入就会分叉。
        let selection = Selection {
            admitted: Vec::new(),
            filtered,
            notice: CapabilityNotice {
                code: "BOD_CAPABILITY_UNSATISFIED",
                missing: Vec::new(),
                message: "没有候选适配器".to_string(),
                next_step: "换适配器".to_string(),
            },
        };
        let alignment = align(Some(&report), &required, Some(&selection));
        assert!(
            alignment
                .divergences
                .iter()
                .any(|item| matches!(item, Divergence::InterfaceOmitsMissing { slot } if slot == "recovery")),
            "{:?}",
            alignment.divergences
        );
    }

    #[test]
    fn adapter_absent_from_selection_is_a_divergence() {
        let required = required_l3();
        let report = report(
            vec![row("recovery", CapabilityVerdict::DeclaredUnsupported)],
            Enablement::Enabled,
            NOW,
        );
        let selection = Selection {
            admitted: vec!["another-adapter".to_string()],
            filtered: Vec::new(),
            notice: CapabilityNotice::satisfied(&required, 1),
        };
        let alignment = align(Some(&report), &required, Some(&selection));
        assert!(
            alignment
                .divergences
                .contains(&Divergence::AdapterAbsentFromSelection)
        );
    }

    #[test]
    fn l1_comparison_is_trivially_clean_and_says_so() {
        let acceptance = ReleaseAcceptance::evaluate(
            &passing_evidence(),
            &RequiredGuarantees::none(),
            None,
            NOW,
        );
        assert!(acceptance.alignment.required_slots.is_empty());
        assert!(acceptance.alignment.is_clean());
        assert!(
            acceptance
                .limitations
                .iter()
                .any(|note| note.contains("未指定接续等级"))
        );
    }

    #[test]
    fn json_and_markdown_name_every_blocking_item() {
        let mut evidence = passing_evidence();
        evidence.dedup_retention_ms = None;
        evidence.authorization_refused = None;
        let acceptance = ReleaseAcceptance::evaluate(
            &evidence,
            &required_l3(),
            Some(&selection_admitting()),
            NOW,
        );
        let markdown = acceptance.to_markdown();
        let json = acceptance.to_json();
        for code in ["no_dedup_retention", "authorization_unchecked"] {
            assert!(markdown.contains(code), "markdown 缺少 {code}");
            assert!(json.contains(code), "json 缺少 {code}");
        }
        assert!(json.contains("\"releasable\": false"));
        assert!(json.contains(RELEASE_SCHEMA));
    }
}
