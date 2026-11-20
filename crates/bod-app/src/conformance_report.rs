//! conformance 分项报告：按适配器版本、协议、平台、能力与日期逐项列出（P2 任务 4.1）。
//!
//! 规格要求「MUST NOT 用单个通过标记掩盖未支持或 Unknown 项」。因此本模块的对外接口里
//! **没有**可返回成功的整体判定：`overall_badge()` 恒返回 [`BadgeError::NotReducible`]，
//! 渲染结果也只给逐项状态与统计，不给「通过」结论。
//!
//! 另一个诚实点：能力声明来自适配器自报（`EvidenceSource::AdapterSelfDeclared`）。
//! 只有当存在**命名用例并全部 pass** 时，某能力才标 `verified`；否则一律是
//! `declared_only` / `not_declared` / `stale`，不能凭声明写成已验证。

use crate::adapter_conformance::{
    CaseOutcome, CaseStatus, ConformanceReport, Enablement, enablement,
};
use crate::domain::execution::{AdapterDescriptor, CapabilitySlot, EvidenceLevel, EvidenceSource};
use std::fmt::Write as _;

/// 报告 schema 标识；格式变化时必须换版本，便于外部工具识别。
pub const REPORT_SCHEMA: &str = "bod.conformance.sectioned/v1";

/// 能力槽 → 能真实验证**适配器**该能力的命名用例。
///
/// 这张表刻意很短，因为「用例通过」经常只证明宿主逻辑正确，不证明适配器具备该能力：
///
/// - `cancellation` ← `cancel-request-vs-confirm`：`transport.cancel()` 直接调用适配器，
///   断言它返回受理还是确认，并检查 `inspect()` 状态。是真正的适配器能力验证。
/// - `external_idempotency` **不映射**到 `duplicate-start` / `params-conflict`：这两个用例
///   断言的是宿主 `submit_start` 的持久去重（DB 只留一条 invocation、第二次直接 replayed），
///   适配器完全可以不支持外部幂等而仍然通过。把它算作适配器能力是自欺。
/// - `reliable_observation` **不映射**到乱序/断档用例：那两个用例验证的是宿主
///   `receive_observation` 的 ACK 推进与窗口外快照要求，与适配器是否提供 ACK/回放无关。
/// - `metering` **不映射**到 `late-final-cost`：那个用例验证宿主在终态后仍持久接收迟到
///   费用观察，不是适配器产生了真实用量。
///
/// 未登记的能力只能到 `declared_only`（纯自报）。要变成 `verified`，需要真实故障/真实
/// 用量样本 —— 那属 4.4 的双运行时验收，不能靠现有 Fake 用例凑。
pub const CAPABILITY_CASES: &[(&str, &[&str])] =
    &[("cancellation", &["cancel-request-vs-confirm"])];

/// 一个能力的判定。注意这里没有「通过」这种整体结论，只有「这个能力被验证到什么程度」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityVerdict {
    /// 声明 supported，且对应用例全部 pass。
    Verified { cases: Vec<String> },
    /// 声明 supported，但没有任何命名用例覆盖（纯自报）。
    DeclaredOnly,
    /// 声明 supported，有用例但存在 fail / not_applicable。
    CasesNotPassing { cases: Vec<String>, worst: String },
    /// 声明 unsupported：不是缺陷，但不得承接依赖它的任务。
    DeclaredUnsupported,
    /// 声明 unknown，或等级无法识别。
    Unknown,
    /// 适配器没提供该槽的声明。
    NotDeclared,
    /// 声明已过期：过期声明不参与验证，也不算 unsupported。
    Stale {
        observed_at_ms: i64,
        expires_at_ms: i64,
    },
}

impl CapabilityVerdict {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Verified { .. } => "verified",
            Self::DeclaredOnly => "declared_only",
            Self::CasesNotPassing { .. } => "cases_not_passing",
            Self::DeclaredUnsupported => "declared_unsupported",
            Self::Unknown => "unknown",
            Self::NotDeclared => "not_declared",
            Self::Stale { .. } => "stale",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRow {
    pub slot: String,
    pub level: crate::domain::execution::SupportLevel,
    pub mode: Option<String>,
    pub evidence: Option<EvidenceSource>,
    pub evidence_level: Option<EvidenceLevel>,
    pub observed_at_ms: Option<i64>,
    pub expires_at_ms: Option<i64>,
    pub verdict: CapabilityVerdict,
    /// 覆盖该能力声明的用例及其结果（id, 状态）。
    pub cases: Vec<(String, String)>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionedReport {
    pub schema: String,
    pub adapter_id: String,
    pub adapter_version: String,
    pub protocol: String,
    pub platform: String,
    pub generated_at_ms: i64,
    pub capabilities: Vec<CapabilityRow>,
    pub cases: Vec<CaseOutcome>,
    pub inconsistencies: Vec<String>,
    pub limitations: Vec<String>,
    /// 4.2 的准入判定：闸门类用例失败即禁用（声明 degraded 不能豁免）。
    pub enablement: Enablement,
}

/// 唯一允许的「整体」结论：报告不可归约。这不是一个可以变绿的标记，而是一条固定约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeError {
    NotReducible,
}

impl SectionedReport {
    /// 从既有 conformance 报告 + 适配器描述符构造分项报告。
    ///
    /// `descriptor` 为 None 表示没有能力清单：报告仍然完整（用例照列），
    /// 但每个能力都是 `not_declared`，并在 `limitations` 里写明原因。
    pub fn build(
        report: &ConformanceReport,
        descriptor: Option<&AdapterDescriptor>,
        now_ms: i64,
    ) -> Self {
        let mut inconsistencies = Vec::new();
        let mut limitations = Vec::new();
        let capabilities =
            build_capabilities(descriptor, &report.outcomes, now_ms, &mut inconsistencies);

        if descriptor.is_none() {
            limitations.push(
                "适配器未提供描述符，无法按能力分项；能力一律记 not_declared，不视为通过"
                    .to_string(),
            );
        } else if descriptor.is_some_and(|item| item.capabilities.is_empty()) {
            limitations
                .push("描述符没有能力声明；能力分项全部为 not_declared，不视为通过".to_string());
        }
        if let Some(item) = descriptor {
            if item.dedup_retention_ms.is_none() {
                limitations.push(
                    "未声明持久去重保留期（dedup_retention_ms），宿主不得据此安全重发".to_string(),
                );
            }
            limitations.extend(item.limitations.iter().cloned());
        }

        Self {
            schema: REPORT_SCHEMA.to_string(),
            adapter_id: report.adapter_id.clone(),
            adapter_version: report.adapter_version.clone(),
            protocol: report.protocol.clone(),
            platform: report.platform.clone(),
            generated_at_ms: report.generated_at_ms,
            capabilities,
            cases: report.outcomes.clone(),
            inconsistencies,
            limitations,
            enablement: enablement(report),
        }
    }

    /// 准入判定的展示文本。被禁用时把失败用例与原因一并写出，不给「大概可以」的余地。
    pub fn enablement_note(&self) -> String {
        match &self.enablement {
            Enablement::Enabled => "enabled（闸门类用例全部通过）".to_string(),
            Enablement::Unverified { cases } => format!(
                "unverified（闸门类未取得证据：{}；可探测/试用，但不得标 Supported）",
                cases.join(", ")
            ),
            Enablement::Blocked { failures } => {
                let listed = failures
                    .iter()
                    .map(|(id, detail)| format!("{id}: {detail}"))
                    .collect::<Vec<_>>()
                    .join("；");
                format!("blocked（{listed}；禁用执行，声明 degraded 不能豁免）")
            }
        }
    }

    pub fn capability(&self, slot: &str) -> Option<&CapabilityRow> {
        self.capabilities.iter().find(|row| row.slot == slot)
    }

    /// 逐项计数，用于展示与审计；**不是**通过结论。
    pub fn tally(&self) -> Vec<(&'static str, usize)> {
        let mut counts: Vec<(&'static str, usize)> = vec![
            ("verified", 0),
            ("declared_only", 0),
            ("cases_not_passing", 0),
            ("declared_unsupported", 0),
            ("unknown", 0),
            ("not_declared", 0),
            ("stale", 0),
        ];
        for row in &self.capabilities {
            if let Some(entry) = counts
                .iter_mut()
                .find(|item| item.0 == row.verdict.as_str())
            {
                entry.1 += 1;
            }
        }
        counts
    }

    pub fn case_tally(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut fail = 0;
        let mut na = 0;
        for outcome in &self.cases {
            match outcome.status {
                CaseStatus::Pass { .. } => pass += 1,
                CaseStatus::Fail { .. } => fail += 1,
                CaseStatus::NotApplicable { .. } => na += 1,
            }
        }
        (pass, fail, na)
    }

    /// 恒拒绝。存在的意义是让「给我一个总的通过结论」这个请求有稳定的拒绝码，
    /// 而不是被调用方自己写一个 `all pass` 拼出来。
    pub fn overall_badge(&self) -> Result<(), BadgeError> {
        Err(BadgeError::NotReducible)
    }

    /// 结论口径：统计 + 明确否定单一标记。
    pub fn verdict_statement(&self) -> String {
        let tally = self
            .tally()
            .into_iter()
            .map(|(label, count)| format!("{label}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        let (pass, fail, na) = self.case_tally();
        format!(
            "不可归约为单一通过标记（能力 {tally}；用例 pass={pass} fail={fail} not_applicable={na}）"
        )
    }

    pub fn to_markdown(&self) -> String {
        self.to_markdown_at(1)
    }

    /// 按指定标题层级渲染；多适配器汇总时用第 2 级，避免出现多个同名一级标题。
    pub fn to_markdown_at(&self, level: usize) -> String {
        let hash = "#".repeat(level.max(1));
        let mut out = String::new();
        let _ = writeln!(out, "{hash} conformance 分项报告（{}）\n", self.schema);

        let _ = writeln!(out, "{hash}# 维度\n\n| 维度 | 值 |\n|---|---|");
        let _ = writeln!(out, "| 适配器 | {} |", self.adapter_id);
        let _ = writeln!(out, "| 适配器版本 | {} |", self.adapter_version);
        let _ = writeln!(out, "| 协议 | {} |", self.protocol);
        let _ = writeln!(out, "| 平台 | {} |", self.platform);
        let _ = writeln!(out, "| 生成日期 | {} |\n", utc_stamp(self.generated_at_ms));

        let _ = writeln!(
            out,
            "{hash}# 能力分项\n\n| 能力 | 声明 | 证据 | 判定 | 覆盖用例 | 说明 |\n|---|---|---|---|---|---|"
        );
        for row in &self.capabilities {
            let evidence = match (row.evidence, row.evidence_level) {
                (Some(source), Some(level)) => format!("{} ({})", source.as_str(), level.as_str()),
                (Some(source), None) => source.as_str().to_string(),
                _ => "—".to_string(),
            };
            let cases = if row.cases.is_empty() {
                "—".to_string()
            } else {
                row.cases
                    .iter()
                    .map(|(id, status)| format!("{id}:{status}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                row.slot,
                row.level.as_str(),
                evidence,
                row.verdict.as_str(),
                cases,
                row.note.replace('|', "/")
            );
        }

        let _ = writeln!(
            out,
            "\n{hash}# 用例分项\n\n| 用例 | 要求 | 结果 | 断言/原因 |\n|---|---|---|---|"
        );
        for outcome in &self.cases {
            let detail = match &outcome.status {
                CaseStatus::Pass { assertions } => format!("{assertions} 条断言"),
                CaseStatus::Fail { detail } => detail.clone(),
                CaseStatus::NotApplicable { reason } => reason.clone(),
            };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                outcome.id,
                outcome.requirement,
                outcome.status.label(),
                detail.replace('|', "/")
            );
        }

        if !self.inconsistencies.is_empty() {
            let _ = writeln!(out, "\n{hash}# 声明与用例的不一致\n");
            for note in &self.inconsistencies {
                let _ = writeln!(out, "- {note}");
            }
        }

        if !self.limitations.is_empty() {
            let _ = writeln!(out, "\n{hash}# 限制与未覆盖\n");
            for note in &self.limitations {
                let _ = writeln!(out, "- {note}");
            }
        }

        let _ = writeln!(out, "\n{hash}# 启用判定\n\n{}", self.enablement_note());

        let _ = writeln!(out, "\n{hash}# 结论口径\n\n{}", self.verdict_statement());
        out
    }

    pub fn to_json(&self) -> String {
        let mut out = String::from("{\n");
        let _ = writeln!(out, "  \"schema\": {},", json_string(&self.schema));
        let _ = writeln!(
            out,
            "  \"adapter\": {{\"id\": {}, \"version\": {}}},",
            json_string(&self.adapter_id),
            json_string(&self.adapter_version)
        );
        let _ = writeln!(out, "  \"protocol\": {},", json_string(&self.protocol));
        let _ = writeln!(out, "  \"platform\": {},", json_string(&self.platform));
        let _ = writeln!(
            out,
            "  \"generated_at\": {},",
            json_string(&utc_stamp(self.generated_at_ms))
        );
        let _ = writeln!(out, "  \"generated_at_ms\": {},", self.generated_at_ms);

        let _ = writeln!(out, "  \"capabilities\": [");
        for (index, row) in self.capabilities.iter().enumerate() {
            let comma = if index + 1 == self.capabilities.len() {
                ""
            } else {
                ","
            };
            let cases: Vec<String> = row
                .cases
                .iter()
                .map(|(id, status)| {
                    format!(
                        "{{\"id\": {}, \"status\": {}}}",
                        json_string(id),
                        json_string(status)
                    )
                })
                .collect();
            let _ = writeln!(
                out,
                "    {{\"slot\": {}, \"level\": {}, \"mode\": {}, \"evidence\": {}, \"evidence_level\": {}, \"observed_at_ms\": {}, \"expires_at_ms\": {}, \"verdict\": {}, \"cases\": [{}], \"note\": {}}}{comma}",
                json_string(&row.slot),
                json_string(row.level.as_str()),
                opt_json_string(row.mode.as_deref()),
                row.evidence
                    .map_or("null".to_string(), |item| json_string(item.as_str())),
                row.evidence_level
                    .map_or("null".to_string(), |item| json_string(item.as_str())),
                row.observed_at_ms
                    .map_or("null".to_string(), |value| value.to_string()),
                row.expires_at_ms
                    .map_or("null".to_string(), |value| value.to_string()),
                json_string(row.verdict.as_str()),
                cases.join(", "),
                json_string(&row.note)
            );
        }
        let _ = writeln!(out, "  ],");

        let _ = writeln!(out, "  \"cases\": [");
        for (index, outcome) in self.cases.iter().enumerate() {
            let comma = if index + 1 == self.cases.len() {
                ""
            } else {
                ","
            };
            let (detail, assertions) = match &outcome.status {
                CaseStatus::Pass { assertions } => (String::new(), Some(*assertions)),
                CaseStatus::Fail { detail } => (detail.clone(), None),
                CaseStatus::NotApplicable { reason } => (reason.clone(), None),
            };
            let _ = writeln!(
                out,
                "    {{\"id\": {}, \"requirement\": {}, \"status\": {}, \"assertions\": {}, \"detail\": {}}}{comma}",
                json_string(outcome.id),
                json_string(outcome.requirement),
                json_string(outcome.status.label()),
                assertions.map_or("null".to_string(), |value| value.to_string()),
                json_string(&detail)
            );
        }
        let _ = writeln!(out, "  ],");

        let _ = writeln!(
            out,
            "  \"inconsistencies\": [{}],",
            self.inconsistencies
                .iter()
                .map(|item| json_string(item))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            out,
            "  \"limitations\": [{}],",
            self.limitations
                .iter()
                .map(|item| json_string(item))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            out,
            "  \"enablement\": {{\"verdict\": {}, \"may_probe\": {}, \"note\": {}}},",
            json_string(self.enablement.as_str()),
            self.enablement.may_probe(),
            json_string(&self.enablement_note())
        );
        let _ = writeln!(
            out,
            "  \"verdict\": {{\"reducible\": false, \"statement\": {}}}",
            json_string(&self.verdict_statement())
        );
        out.push('}');
        out
    }
}

fn build_capabilities(
    descriptor: Option<&AdapterDescriptor>,
    outcomes: &[CaseOutcome],
    now_ms: i64,
    inconsistencies: &mut Vec<String>,
) -> Vec<CapabilityRow> {
    let mut slots: Vec<String> = CapabilitySlot::KNOWN
        .iter()
        .map(|slot| (*slot).to_string())
        .collect();
    if let Some(item) = descriptor {
        for claim in &item.capabilities {
            let name = claim.slot.as_str().to_string();
            if !slots.contains(&name) {
                slots.push(name);
            }
        }
    }

    let mut rows = Vec::with_capacity(slots.len());
    for slot in slots {
        let claim = descriptor.and_then(|item| {
            item.capabilities
                .iter()
                .find(|claim| claim.slot.as_str() == slot)
        });
        rows.push(build_row(&slot, claim, outcomes, now_ms, inconsistencies));
    }
    rows
}

fn build_row(
    slot: &str,
    claim: Option<&crate::domain::execution::CapabilityClaim>,
    outcomes: &[CaseOutcome],
    now_ms: i64,
    inconsistencies: &mut Vec<String>,
) -> CapabilityRow {
    let Some(claim) = claim else {
        return CapabilityRow {
            slot: slot.to_string(),
            level: crate::domain::execution::SupportLevel::Unknown,
            mode: None,
            evidence: None,
            evidence_level: None,
            observed_at_ms: None,
            expires_at_ms: None,
            verdict: CapabilityVerdict::NotDeclared,
            cases: Vec::new(),
            note: "适配器未声明该能力；不得据此认为支持或明确不支持".to_string(),
        };
    };

    let mapped = CAPABILITY_CASES
        .iter()
        .find(|(name, _)| *name == slot)
        .map(|(_, cases)| *cases)
        .unwrap_or(&[]);
    let mut cases = Vec::with_capacity(mapped.len());
    for id in mapped {
        let status = outcomes
            .iter()
            .find(|outcome| outcome.id == *id)
            .map_or("missing", |outcome| outcome.status.label());
        cases.push(((*id).to_string(), status.to_string()));
    }

    let verdict = if claim.is_stale(now_ms) {
        CapabilityVerdict::Stale {
            observed_at_ms: claim.observed_at_ms,
            expires_at_ms: claim.expires_at_ms.unwrap_or(0),
        }
    } else {
        match claim.level {
            crate::domain::execution::SupportLevel::Supported => {
                if mapped.is_empty() {
                    CapabilityVerdict::DeclaredOnly
                } else if cases.iter().all(|(_, status)| status == "pass") {
                    CapabilityVerdict::Verified {
                        cases: cases.iter().map(|(id, _)| id.clone()).collect(),
                    }
                } else {
                    let worst = cases
                        .iter()
                        .find(|(_, status)| status != "pass")
                        .map(|(_, status)| status.clone())
                        .unwrap_or_else(|| "missing".to_string());
                    CapabilityVerdict::CasesNotPassing {
                        cases: cases.iter().map(|(id, _)| id.clone()).collect(),
                        worst,
                    }
                }
            }
            crate::domain::execution::SupportLevel::Unsupported => {
                if let Some((id, _)) = cases.iter().find(|(_, status)| status == "pass") {
                    inconsistencies.push(format!(
                        "能力 {slot} 声明 unsupported，但用例 {id} 断言通过；以声明为准（不据此放开准入），需核实声明是否过时"
                    ));
                }
                CapabilityVerdict::DeclaredUnsupported
            }
            crate::domain::execution::SupportLevel::Unknown => CapabilityVerdict::Unknown,
        }
    };

    let note = match &verdict {
        CapabilityVerdict::Verified { .. } => format!("{} 个命名用例全部通过", cases.len()),
        CapabilityVerdict::DeclaredOnly => {
            "声明 supported，但没有命名用例覆盖；属自报，不算已验证".to_string()
        }
        CapabilityVerdict::CasesNotPassing { worst, .. } => {
            format!("声明 supported，但覆盖用例存在 {worst}；不构成已验证")
        }
        CapabilityVerdict::DeclaredUnsupported => {
            "声明 unsupported：可安装，但不得承接依赖该能力的任务".to_string()
        }
        CapabilityVerdict::Unknown => "声明为 unknown；按不支持处理，界面须显示未知".to_string(),
        CapabilityVerdict::Stale { .. } => {
            "声明已过期；过期声明不参与判定，需重新探测/重新声明".to_string()
        }
        CapabilityVerdict::NotDeclared => unreachable!("未声明分支在上面提前返回"),
    };

    CapabilityRow {
        slot: slot.to_string(),
        level: claim.level,
        mode: claim.mode.clone(),
        evidence: Some(claim.source),
        evidence_level: Some(claim.evidence_level),
        observed_at_ms: Some(claim.observed_at_ms),
        expires_at_ms: claim.expires_at_ms,
        verdict,
        cases,
        note,
    }
}

fn opt_json_string(value: Option<&str>) -> String {
    value.map_or("null".to_string(), json_string)
}

pub(crate) fn json_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// 毫秒时间戳 → UTC `YYYY-MM-DDTHH:MM:SSZ`。
///
/// 不引第三方时间库：报告只需要一个可核对的时间串，多一个依赖不值得。
pub fn utc_stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days（公历，无闰秒）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_conformance::{CASES, run_fake_suite};
    use crate::domain::execution::{
        AdapterDescriptor, CapabilityClaim, CapabilitySlot, PROTOCOL_ID, SupportLevel,
    };
    use crate::fake_executor::{ADAPTER_ID, ADAPTER_VERSION, FakeExecutor, FakeScenario};
    use crate::invocation::StartTransport;

    const NOW_MS: i64 = 1_764_100_000_000;

    fn base_descriptor() -> AdapterDescriptor {
        FakeExecutor::new(FakeScenario::HappyPath)
            .descriptor()
            .clone()
    }

    fn claim(
        slot: CapabilitySlot,
        level: SupportLevel,
        expires_at_ms: Option<i64>,
    ) -> CapabilityClaim {
        CapabilityClaim {
            slot,
            level,
            mode: Some("test_mode".to_string()),
            source: EvidenceSource::AdapterSelfDeclared,
            evidence_level: EvidenceLevel::SelfDeclared,
            observed_at_ms: NOW_MS - 1_000,
            expires_at_ms,
            reason: Some("测试声明".to_string()),
        }
    }

    fn descriptor_with(claims: Vec<CapabilityClaim>) -> AdapterDescriptor {
        let mut base = base_descriptor();
        base.capabilities = claims;
        base
    }

    fn sectioned(claims: Vec<CapabilityClaim>) -> SectionedReport {
        SectionedReport::build(
            &run_fake_suite(NOW_MS),
            Some(&descriptor_with(claims)),
            NOW_MS,
        )
    }

    #[test]
    fn dimensions_include_version_protocol_platform_and_date() {
        let report =
            SectionedReport::build(&run_fake_suite(NOW_MS), Some(&base_descriptor()), NOW_MS);
        assert_eq!(report.schema, REPORT_SCHEMA);
        assert_eq!(report.adapter_id, ADAPTER_ID);
        assert_eq!(report.adapter_version, ADAPTER_VERSION);
        assert_eq!(report.protocol, PROTOCOL_ID);
        assert_eq!(report.platform, "conformance");
        assert_eq!(report.generated_at_ms, NOW_MS);

        let markdown = report.to_markdown();
        for label in ["适配器版本", "协议", "平台", "生成日期"] {
            assert!(markdown.contains(label), "报告缺少维度 {label}");
        }
        assert!(markdown.contains(&utc_stamp(NOW_MS)), "报告缺少生成日期");
        assert_eq!(report.cases.len(), CASES.len());
    }

    #[test]
    fn known_slots_are_listed_and_extra_claim_is_kept() {
        let report = sectioned(vec![
            claim(CapabilitySlot::Cancellation, SupportLevel::Supported, None),
            claim(
                CapabilitySlot::Other("vendor_extra".to_string()),
                SupportLevel::Supported,
                None,
            ),
        ]);
        assert_eq!(report.capabilities.len(), CapabilitySlot::KNOWN.len() + 1);
        let extra = report.capability("vendor_extra").expect("未知槽必须保留");
        assert_eq!(extra.verdict, CapabilityVerdict::DeclaredOnly);
        // 未声明的已知槽一律 not_declared，不默认 supported。
        assert_eq!(
            report.capability("recovery").unwrap().verdict,
            CapabilityVerdict::NotDeclared
        );
    }

    #[test]
    fn declared_supported_without_case_is_only_declared() {
        let report = sectioned(vec![claim(
            CapabilitySlot::Recovery,
            SupportLevel::Supported,
            None,
        )]);
        let recovery = report.capability("recovery").unwrap();
        assert_eq!(recovery.verdict, CapabilityVerdict::DeclaredOnly);
        assert!(recovery.cases.is_empty());
        assert!(recovery.note.contains("不算已验证"));
    }

    #[test]
    fn declared_supported_with_passing_case_is_verified() {
        let report = sectioned(vec![claim(
            CapabilitySlot::Cancellation,
            SupportLevel::Supported,
            None,
        )]);
        let cancellation = report.capability("cancellation").unwrap();
        assert_eq!(
            cancellation.verdict,
            CapabilityVerdict::Verified {
                cases: vec!["cancel-request-vs-confirm".to_string()]
            }
        );
        assert_eq!(
            cancellation.cases,
            vec![("cancel-request-vs-confirm".to_string(), "pass".to_string())]
        );
    }

    #[test]
    fn not_applicable_case_prevents_verified() {
        let mut report = run_fake_suite(NOW_MS);
        let outcome = report
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.id == "cancel-request-vs-confirm")
            .expect("用例应存在");
        outcome.status = CaseStatus::NotApplicable {
            reason: "适配器不具备故障注入能力".to_string(),
        };
        let sectioned = SectionedReport::build(
            &report,
            Some(&descriptor_with(vec![claim(
                CapabilitySlot::Cancellation,
                SupportLevel::Supported,
                None,
            )])),
            NOW_MS,
        );
        match &sectioned.capability("cancellation").unwrap().verdict {
            CapabilityVerdict::CasesNotPassing { worst, .. } => {
                assert_eq!(worst, "not_applicable");
            }
            other => panic!("不适用用例不能算已验证，实际 {other:?}"),
        }
    }

    #[test]
    fn stale_claim_is_stale_even_when_cases_pass() {
        let report = sectioned(vec![claim(
            CapabilitySlot::Cancellation,
            SupportLevel::Supported,
            Some(NOW_MS - 1),
        )]);
        let cancellation = report.capability("cancellation").unwrap();
        assert!(matches!(
            cancellation.verdict,
            CapabilityVerdict::Stale { .. }
        ));
        assert_eq!(
            report
                .tally()
                .iter()
                .find(|(label, _)| *label == "verified")
                .unwrap()
                .1,
            0
        );
    }

    #[test]
    fn unsupported_declaration_with_passing_case_is_reported() {
        let report = sectioned(vec![claim(
            CapabilitySlot::Cancellation,
            SupportLevel::Unsupported,
            None,
        )]);
        assert_eq!(
            report.capability("cancellation").unwrap().verdict,
            CapabilityVerdict::DeclaredUnsupported
        );
        assert_eq!(report.inconsistencies.len(), 1);
        assert!(report.inconsistencies[0].contains("cancel-request-vs-confirm"));
        assert!(report.inconsistencies[0].contains("以声明为准"));
    }

    #[test]
    fn overall_badge_is_refused_by_construction() {
        let report =
            SectionedReport::build(&run_fake_suite(NOW_MS), Some(&base_descriptor()), NOW_MS);
        assert_eq!(report.overall_badge(), Err(BadgeError::NotReducible));

        let markdown = report.to_markdown();
        assert!(
            !markdown.contains("PASS"),
            "报告不得出现整体通过标记:\n{markdown}"
        );
        assert!(markdown.contains("不可归约为单一通过标记"));

        let statement = report.verdict_statement();
        assert!(statement.contains("不可归约"));
        assert!(statement.contains("not_applicable="));
    }

    #[test]
    fn json_carries_every_section_and_no_passing_flag() {
        let report = sectioned(vec![claim(
            CapabilitySlot::Cancellation,
            SupportLevel::Supported,
            None,
        )]);
        let json = report.to_json();
        for key in [
            "\"schema\"",
            "\"capabilities\"",
            "\"cases\"",
            "\"inconsistencies\"",
            "\"limitations\"",
            "\"enablement\"",
            "\"reducible\": false",
        ] {
            assert!(json.contains(key), "json 缺少 {key}:\n{json}");
        }
        assert!(!json.contains("\"passed\""), "不得输出单一通过标记");
        // 至少一个能力项带 verdict 与 cases。
        assert!(json.contains("\"slot\": \"cancellation\""));
        assert!(json.contains("\"verdict\": \"verified\""));
    }

    #[test]
    fn missing_dedup_retention_is_listed_as_a_limitation() {
        let mut descriptor = base_descriptor();
        descriptor.dedup_retention_ms = None;
        let report = SectionedReport::build(&run_fake_suite(NOW_MS), Some(&descriptor), NOW_MS);
        assert!(
            report
                .limitations
                .iter()
                .any(|note| note.contains("持久去重保留期"))
        );

        // 没有描述符时能力全部 not_declared，但用例照列，不因此判「通过」。
        let bare = SectionedReport::build(&run_fake_suite(NOW_MS), None, NOW_MS);
        assert_eq!(bare.cases.len(), CASES.len());
        assert!(
            bare.capabilities
                .iter()
                .all(|row| row.verdict == CapabilityVerdict::NotDeclared)
        );
        assert!(
            bare.limitations
                .iter()
                .any(|note| note.contains("未提供描述符"))
        );
    }

    #[test]
    fn enablement_block_is_rendered_without_escape_hatch() {
        let mut report = run_fake_suite(NOW_MS);
        let ok = SectionedReport::build(&report, Some(&base_descriptor()), NOW_MS);
        assert!(ok.enablement_note().starts_with("enabled"));
        assert!(ok.to_markdown().contains("启用判定"));

        let outcome = report
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.id == "scope-identity")
            .expect("身份用例应存在");
        outcome.status = CaseStatus::Fail {
            detail: "跨公司同 ID 未被拒绝".to_string(),
        };
        let blocked = SectionedReport::build(&report, Some(&base_descriptor()), NOW_MS);
        let note = blocked.enablement_note();
        assert!(note.starts_with("blocked"), "{note}");
        assert!(
            note.contains("degraded 不能豁免"),
            "必须写明没有豁免出口: {note}"
        );
        assert!(blocked.to_markdown().contains("blocked"));
        let json = blocked.to_json();
        assert!(json.contains("\"verdict\": \"blocked\""));
        assert!(json.contains("\"may_probe\": false"));
    }

    #[test]
    fn utc_stamp_formats_known_instants() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_stamp(1_700_000_000_000), "2023-11-14T22:13:20Z");
        // 闰日：2024-02-29T00:00:00Z = 1709164800 s
        assert_eq!(utc_stamp(1_709_164_800_000), "2024-02-29T00:00:00Z");
    }
}
