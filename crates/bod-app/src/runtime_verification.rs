//! P4 任务 1.1—1.5：运行验收与构建记录（`runtime-verification`）。
//!
//! 这一层要解决的问题不是"跑测试"，而是让一个绿灯只在它被观测到的那个制品、环境与规则
//! 版本上成立。五件事分开做，因为它们的失败方式不同：
//! 1. **计划**（1.1）：先声明覆盖哪些面、由谁检查。缺一个面就直接拒绝建计划——否则会出现
//!    "跑了两百个用例但从没跑过权限"这种看起来很忙的空白。
//! 2. **证据绑定**（1.2）：报告必须钉住 commit/digest、规格与规则修订、环境、工具版本、
//!    执行者、时间与原始引用。对不上就不接受，分支绿灯不算证据（`invariant_i_08`）。
//! 3. **构建记录**（1.3）：一次构建、按 digest 在环境间晋升；平台变了必须重建并重验，
//!    不允许"看起来一样"就当作同一个包。
//! 4. **门禁语义**（1.4）：未运行不是 Pass。必需项 Unknown/Fail 阻止自动接受，
//!    豁免单独留痕，不能被结果里的 Pass 掩盖。
//! 5. **失败分类与独立性**（1.5）：环境炸了不算代码缺陷；自己写自己测时，只换执行人
//!    不构成独立验证，必须补基准、契约用例或人工抽样。
//!
//! 本模块不执行任何检查，也不自建浏览器引擎或测试语言：它只产出计划、判定与记录，
//! 真正的执行交给宿主已有的脚本、CI、API 客户端与浏览器驱动。

use crate::domain::artifact::sha256_hex;
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// 1.1 验证计划
// ---------------------------------------------------------------------------

/// 计划必须覆盖的五个面。缺任一项即拒绝建计划。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoverageArea {
    /// 应用启动与健康检查。
    StartupHealth,
    /// 核心 API。
    CoreApi,
    /// 权限与数据读写。
    PermissionAndData,
    /// 一条关键 UI 流程。
    KeyUiFlow,
    /// 失败路径。
    FailurePath,
}

impl CoverageArea {
    pub const ALL: [Self; 5] = [
        Self::StartupHealth,
        Self::CoreApi,
        Self::PermissionAndData,
        Self::KeyUiFlow,
        Self::FailurePath,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StartupHealth => "startup-health",
            Self::CoreApi => "core-api",
            Self::PermissionAndData => "permission-data",
            Self::KeyUiFlow => "key-ui-flow",
            Self::FailurePath => "failure-path",
        }
    }
}

/// 检查方式：只能复用宿主已有的能力，不能现造一套。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// 现成脚本。
    Script,
    /// CI 作业。
    CiJob,
    /// API 检查（外部客户端）。
    Api,
    /// 浏览器流程（外部驱动）。
    BrowserFlow,
    /// 测试 Agent（外部模型循环）。
    TestAgent,
    /// 人工检查。
    Manual,
}

impl CheckKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::CiJob => "ci-job",
            Self::Api => "api",
            Self::BrowserFlow => "browser-flow",
            Self::TestAgent => "test-agent",
            Self::Manual => "manual",
        }
    }

    /// 这几类最容易滑向"自己写一个"：必须指明用的是哪个已有工具。
    pub fn needs_external_tool(&self) -> bool {
        matches!(self, Self::Api | Self::BrowserFlow | Self::TestAgent)
    }
}

/// 一条计划项：场景/判据 → 可执行检查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub scenario: String,
    pub criterion: String,
    pub area: CoverageArea,
    pub kind: CheckKind,
    /// 承载该项检查的已有工具（`cargo test`/`playwright`/`curl`…）。
    pub tool: String,
    pub data: String,
    pub environment: String,
    pub expected: String,
}

impl PlanItem {
    pub fn new(
        scenario: &str,
        criterion: &str,
        area: CoverageArea,
        kind: CheckKind,
        tool: &str,
    ) -> Self {
        Self {
            scenario: scenario.to_string(),
            criterion: criterion.to_string(),
            area,
            kind,
            tool: tool.to_string(),
            data: String::new(),
            environment: String::new(),
            expected: String::new(),
        }
    }

    pub fn with_data(mut self, data: &str) -> Self {
        self.data = data.to_string();
        self
    }

    pub fn with_environment(mut self, environment: &str) -> Self {
        self.environment = environment.to_string();
        self
    }

    pub fn with_expected(mut self, expected: &str) -> Self {
        self.expected = expected.to_string();
        self
    }
}

/// 计划错误。缺面与"自建引擎"是两类不同的问题，分开报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    EmptyItems,
    MissingArea(CoverageArea),
    MissingTool { scenario: String },
    BuiltinHarness { scenario: String, tool: String },
    EmptyScenario,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyItems => write!(formatter, "验证计划为空"),
            Self::MissingArea(area) => {
                write!(formatter, "验证计划未覆盖 `{}`", area.as_str())
            }
            Self::MissingTool { scenario } => {
                write!(formatter, "计划项 `{scenario}` 需要指明承载检查的已有工具")
            }
            Self::BuiltinHarness { scenario, tool } => write!(
                formatter,
                "计划项 `{scenario}` 使用了自建引擎 `{tool}`：应复用已有脚本/CI/浏览器驱动"
            ),
            Self::EmptyScenario => write!(formatter, "计划项缺少场景名"),
        }
    }
}

impl std::error::Error for PlanError {}

/// 自建引擎的常见自称。命中即拒绝：这一层只允许复用宿主能力。
fn is_builtin_harness(tool: &str) -> bool {
    let tool = tool.trim().to_ascii_lowercase();
    tool == "builtin" || tool.starts_with("builtin:") || tool == "bod-browser" || tool == "selfmade"
}

/// 验证计划：场景/判据到检查的映射，加上覆盖自检。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationPlan {
    pub plan_id: String,
    pub revision: String,
    pub items: Vec<PlanItem>,
}

impl VerificationPlan {
    pub fn new(plan_id: &str, revision: &str) -> Self {
        Self {
            plan_id: plan_id.to_string(),
            revision: revision.to_string(),
            items: Vec::new(),
        }
    }

    pub fn push(&mut self, item: PlanItem) -> &mut Self {
        self.items.push(item);
        self
    }

    pub fn missing_areas(&self) -> Vec<CoverageArea> {
        CoverageArea::ALL
            .iter()
            .copied()
            .filter(|area| !self.items.iter().any(|item| item.area == *area))
            .collect()
    }

    /// 覆盖自检 + 每项检查方式自检。顺序固定：先看是否为空，再按 [`CoverageArea::ALL`]
    /// 报第一个缺面，最后逐项看检查方式，保证报告稳定可复现。
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.items.is_empty() {
            return Err(PlanError::EmptyItems);
        }
        if let Some(area) = self.missing_areas().first() {
            return Err(PlanError::MissingArea(*area));
        }
        for item in &self.items {
            if item.scenario.trim().is_empty() {
                return Err(PlanError::EmptyScenario);
            }
            if item.kind.needs_external_tool() && item.tool.trim().is_empty() {
                return Err(PlanError::MissingTool {
                    scenario: item.scenario.clone(),
                });
            }
            if is_builtin_harness(&item.tool) {
                return Err(PlanError::BuiltinHarness {
                    scenario: item.scenario.clone(),
                    tool: item.tool.clone(),
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 1.2 证据绑定
// ---------------------------------------------------------------------------

/// 被测候选：源码提交，以及（构建后的）制品 digest。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub candidate_id: String,
    pub commit: String,
    pub artifact_digest: Option<String>,
}

impl Candidate {
    pub fn new(candidate_id: &str, commit: &str) -> Self {
        Self {
            candidate_id: candidate_id.to_string(),
            commit: commit.to_string(),
            artifact_digest: None,
        }
    }

    pub fn with_digest(mut self, digest: &str) -> Self {
        self.artifact_digest = Some(digest.to_string());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    pub name: String,
    pub version: String,
}

impl ToolVersion {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
        }
    }
}

/// 执行者。CI 与 Agent 也要具名，否则"谁跑的"无法追溯。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Executor {
    Human { id: String },
    Agent { id: String },
    Ci { job: String },
}

impl Executor {
    pub fn describe(&self) -> String {
        match self {
            Self::Human { id } => format!("human:{id}"),
            Self::Agent { id } => format!("agent:{id}"),
            Self::Ci { job } => format!("ci:{job}"),
        }
    }
}

/// 检查结果。四态，缺一不可：`NotApplicable` 与 `Unknown` 是两件不同的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    Pass,
    Fail,
    Unknown,
    NotApplicable,
}

impl CheckOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
            Self::NotApplicable => "not-applicable",
        }
    }

    /// 越靠后越严重。同一检查出现多次结果时取最差，避免后面的 Pass 掩盖前面的 Fail。
    fn severity(&self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::NotApplicable => 1,
            Self::Unknown => 2,
            Self::Fail => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub check_id: String,
    pub scenario: String,
    pub outcome: CheckOutcome,
    /// 失败或未决时的信号，用于分类（1.5）。
    pub failure: Option<FailureSignal>,
    /// 原始证据引用（日志、报告文件、trace）。截图只能算辅助观察。
    pub evidence: Vec<String>,
    pub observed_at: String,
}

impl CheckResult {
    pub fn passing(check_id: &str, scenario: &str, observed_at: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            scenario: scenario.to_string(),
            outcome: CheckOutcome::Pass,
            failure: None,
            evidence: Vec::new(),
            observed_at: observed_at.to_string(),
        }
    }

    pub fn failing(
        check_id: &str,
        scenario: &str,
        signal: FailureSignal,
        observed_at: &str,
    ) -> Self {
        Self {
            check_id: check_id.to_string(),
            scenario: scenario.to_string(),
            outcome: CheckOutcome::Fail,
            failure: Some(signal),
            evidence: Vec::new(),
            observed_at: observed_at.to_string(),
        }
    }

    pub fn with_evidence(mut self, evidence: &[&str]) -> Self {
        self.evidence = evidence.iter().map(|item| item.to_string()).collect();
        self
    }
}

/// 一份验证报告。字段就是"这条结论在什么条件下成立"的全部答案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    pub report_id: String,
    pub candidate: Candidate,
    /// spec/criterion 修订。
    pub spec_revision: String,
    /// 规则版本（门禁规则、检查定义）。
    pub rule_revision: String,
    pub environment: String,
    pub tools: Vec<ToolVersion>,
    pub executor: Executor,
    pub finished_at: String,
    /// 原始报告引用。没有它就只是自述。
    pub raw_ref: String,
    pub results: Vec<CheckResult>,
}

impl VerificationReport {
    pub fn result(&self, check_id: &str) -> Option<&CheckResult> {
        self.results.iter().find(|item| item.check_id == check_id)
    }

    pub fn scope(&self) -> ValidityScope {
        ValidityScope {
            environment: self.environment.clone(),
            rule_revision: self.rule_revision.clone(),
            inputs_digest: inputs_digest(&self.candidate),
        }
    }
}

/// 报告与接受目标的匹配结论。只有 [`Self::Usable`] 能用于接受。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceMatch {
    Usable,
    /// 报告来自别的提交：分支绿灯不等于合并后制品通过（`invariant_i_08`）。
    StaleCommit {
        report_commit: String,
        target_commit: String,
    },
    DigestMismatch {
        report_digest: Option<String>,
        target_digest: String,
    },
    NeedsRevalidation {
        reason: RevalidationReason,
    },
    /// 证据本身不完整，连"是否匹配"都判断不了。
    Unknown {
        reason: String,
    },
}

impl EvidenceMatch {
    pub fn accepts(&self) -> bool {
        matches!(self, Self::Usable)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Usable => "证据与目标匹配".to_string(),
            Self::StaleCommit {
                report_commit,
                target_commit,
            } => format!("报告提交 {report_commit} 与目标 {target_commit} 不一致，需重新验证"),
            Self::DigestMismatch {
                report_digest,
                target_digest,
            } => format!(
                "报告制品 {:?} 与目标 {target_digest} 不一致",
                report_digest.as_deref().unwrap_or("(未记录)")
            ),
            Self::NeedsRevalidation { reason } => {
                format!("结论需重验：{}", reason.as_str())
            }
            Self::Unknown { reason } => format!("证据不完整：{reason}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevalidationReason {
    RuleRevisionChanged,
    EnvironmentChanged,
    InputsChanged,
}

impl RevalidationReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RuleRevisionChanged => "规则版本变化",
            Self::EnvironmentChanged => "环境变化",
            Self::InputsChanged => "被测输入变化",
        }
    }
}

fn inputs_digest(candidate: &Candidate) -> String {
    let digest = candidate.artifact_digest.as_deref().unwrap_or("(none)");
    sha256_hex(format!("commit={}\ndigest={digest}", candidate.commit).as_bytes())
}

/// 判定一份报告能否用于目标候选的接受。
///
/// 顺序是刻意的：先看"测的是不是同一个东西"（提交、digest），再看"证据本身是否完整"，
/// 最后才看规则版本。前两类对不上时，规则再新也没意义。
pub fn match_report(
    report: &VerificationReport,
    target: &Candidate,
    current_rule_revision: &str,
) -> EvidenceMatch {
    if report.candidate.commit != target.commit {
        return EvidenceMatch::StaleCommit {
            report_commit: report.candidate.commit.clone(),
            target_commit: target.commit.clone(),
        };
    }
    if let Some(target_digest) = &target.artifact_digest {
        match &report.candidate.artifact_digest {
            Some(report_digest) if report_digest == target_digest => {}
            other => {
                return EvidenceMatch::DigestMismatch {
                    report_digest: other.clone(),
                    target_digest: target_digest.clone(),
                };
            }
        }
    }
    if report.raw_ref.trim().is_empty() {
        return EvidenceMatch::Unknown {
            reason: "未记录原始报告引用".to_string(),
        };
    }
    if report.tools.is_empty() {
        return EvidenceMatch::Unknown {
            reason: "未记录工具版本".to_string(),
        };
    }
    if report.spec_revision.trim().is_empty() {
        return EvidenceMatch::Unknown {
            reason: "未记录规格修订".to_string(),
        };
    }
    if report.results.is_empty() {
        return EvidenceMatch::Unknown {
            reason: "报告没有任何检查结果".to_string(),
        };
    }
    if report.rule_revision != current_rule_revision {
        return EvidenceMatch::NeedsRevalidation {
            reason: RevalidationReason::RuleRevisionChanged,
        };
    }
    EvidenceMatch::Usable
}

/// 结论成立的条件范围。环境、规则或输入任何一项变了，旧结论就不再自动成立。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidityScope {
    pub environment: String,
    pub rule_revision: String,
    pub inputs_digest: String,
}

impl ValidityScope {
    pub fn matches(&self, other: &Self) -> Validity {
        if self.rule_revision != other.rule_revision {
            return Validity::NeedsRevalidation(RevalidationReason::RuleRevisionChanged);
        }
        if self.environment != other.environment {
            return Validity::NeedsRevalidation(RevalidationReason::EnvironmentChanged);
        }
        if self.inputs_digest != other.inputs_digest {
            return Validity::NeedsRevalidation(RevalidationReason::InputsChanged);
        }
        Validity::Valid
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    Valid,
    NeedsRevalidation(RevalidationReason),
}

impl Validity {
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }
}

// ---------------------------------------------------------------------------
// 1.3 构建记录与晋升
// ---------------------------------------------------------------------------

/// 可复现性声明。没验过就说没验过，不做默认推断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReproClaim {
    NotChecked,
    Verified,
    Failed,
}

impl ReproClaim {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotChecked => "not-checked",
            Self::Verified => "verified",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRecord {
    pub build_id: String,
    pub source_commit: String,
    pub build_definition: String,
    pub toolchain: String,
    pub dependency_lock_digest: String,
    pub platform: String,
    pub parameters_digest: String,
    pub environment: String,
    pub output_digest: String,
    pub reproducible: ReproClaim,
}

impl BuildRecord {
    /// 身份摘要：不含 `build_id`、`environment` 与可复现声明。
    ///
    /// 这三项不改变"这是哪个包"——同一候选在两个环境各构建一次，只要源码、构建定义、
    /// 工具链、锁、平台与参数一致，身份摘要就相同，可以复用同一个 digest 晋升。
    /// 平台进身份：换平台就必须是新记录。
    pub fn identity_digest(&self) -> String {
        let material = format!(
            "commit={}\ndefinition={}\ntoolchain={}\nlock={}\nplatform={}\nparameters={}\noutput={}",
            self.source_commit,
            self.build_definition,
            self.toolchain,
            self.dependency_lock_digest,
            self.platform,
            self.parameters_digest,
            self.output_digest
        );
        sha256_hex(material.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    Inserted,
    /// 已存在同一 digest 的记录：不重复登记，但也不假装是新构建。
    DuplicateOf {
        build_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionRequest {
    pub digest: String,
    pub from_environment: String,
    pub to_environment: String,
    pub platform: String,
}

impl PromotionRequest {
    pub fn new(digest: &str, from: &str, to: &str, platform: &str) -> Self {
        Self {
            digest: digest.to_string(),
            from_environment: from.to_string(),
            to_environment: to.to_string(),
            platform: platform.to_string(),
        }
    }
}

/// 晋升时对验证证据的处置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionEvidence {
    Reusable,
    NotReusable(EvidenceMatch),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// 复用同一 digest（与同一份验证结论），不重新构建。
    Reused {
        build_id: String,
        digest: String,
        to_environment: String,
        evidence: PromotionEvidence,
    },
    /// 必须重建：没这个 digest，或平台不同。重建后要重新登记并重验。
    RebuildRequired {
        reason: RebuildReason,
        recorded_platform: Option<String>,
        requested_platform: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildReason {
    UnknownDigest,
    PlatformDiffers,
}

impl RebuildReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnknownDigest => "没有该 digest 的构建记录",
            Self::PlatformDiffers => "平台不同，必须重建并重新验证",
        }
    }
}

/// 构建台账：按 digest 与身份摘要索引。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildLedger {
    records: Vec<BuildRecord>,
}

impl BuildLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn records(&self) -> &[BuildRecord] {
        &self.records
    }

    pub fn record(&mut self, record: BuildRecord) -> RecordOutcome {
        if let Some(existing) = self.find_by_digest(&record.output_digest) {
            return RecordOutcome::DuplicateOf {
                build_id: existing.build_id.clone(),
            };
        }
        self.records.push(record);
        RecordOutcome::Inserted
    }

    pub fn find_by_digest(&self, digest: &str) -> Option<&BuildRecord> {
        self.records
            .iter()
            .find(|item| item.output_digest == digest)
    }

    pub fn find_by_identity(&self, identity: &str) -> Option<&BuildRecord> {
        self.records
            .iter()
            .find(|item| item.identity_digest() == identity)
    }

    /// 按 digest 晋升。`evidence` 是该 digest 已有验证结论与目标的匹配结论。
    ///
    /// 平台不同时不晋升：即便 digest 相同也要求重建，因为"同一字节在不同平台跑过"
    /// 这件事没有被任何证据覆盖过。
    pub fn promote(
        &self,
        request: &PromotionRequest,
        evidence: &EvidenceMatch,
    ) -> PromotionOutcome {
        let Some(build) = self.find_by_digest(&request.digest) else {
            return PromotionOutcome::RebuildRequired {
                reason: RebuildReason::UnknownDigest,
                recorded_platform: None,
                requested_platform: request.platform.clone(),
            };
        };
        if build.platform != request.platform {
            return PromotionOutcome::RebuildRequired {
                reason: RebuildReason::PlatformDiffers,
                recorded_platform: Some(build.platform.clone()),
                requested_platform: request.platform.clone(),
            };
        }
        let carried = if evidence.accepts() {
            PromotionEvidence::Reusable
        } else {
            PromotionEvidence::NotReusable(evidence.clone())
        };
        PromotionOutcome::Reused {
            build_id: build.build_id.clone(),
            digest: build.output_digest.clone(),
            to_environment: request.to_environment.clone(),
            evidence: carried,
        }
    }
}

// ---------------------------------------------------------------------------
// 1.4 门禁语义
// ---------------------------------------------------------------------------

/// 检查的三类。`Manual` 不是"软一点"，而是"证据来自人，通常是验收回执"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    Required,
    Advisory,
    Manual,
}

impl GateKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Advisory => "advisory",
            Self::Manual => "manual",
        }
    }
}

/// 一条门禁规则：为什么这项是必需/建议/人工。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRule {
    pub check_id: String,
    pub kind: GateKind,
    pub reason: String,
}

impl GateRule {
    pub fn required(check_id: &str, reason: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            kind: GateKind::Required,
            reason: reason.to_string(),
        }
    }

    pub fn advisory(check_id: &str, reason: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            kind: GateKind::Advisory,
            reason: reason.to_string(),
        }
    }

    pub fn manual(check_id: &str, reason: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            kind: GateKind::Manual,
            reason: reason.to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateSpec {
    pub rules: Vec<GateRule>,
}

impl GateSpec {
    pub fn new(rules: Vec<GateRule>) -> Self {
        Self { rules }
    }

    pub fn push(&mut self, rule: GateRule) -> &mut Self {
        self.rules.push(rule);
        self
    }
}

/// 豁免：独立留痕，不能塞进结果里把 Fail 改写成 Pass。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiver {
    pub check_id: String,
    pub by: String,
    pub reason: String,
    pub at: String,
}

impl Waiver {
    pub fn new(check_id: &str, by: &str, reason: &str, at: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            by: by.to_string(),
            reason: reason.to_string(),
            at: at.to_string(),
        }
    }
}

/// 阻止自动接受的原因。失败项带上分类，报告才能把工具失败与应用失败分列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    NotRun {
        check_id: String,
        kind: GateKind,
    },
    Unknown {
        check_id: String,
        kind: GateKind,
    },
    Failed {
        check_id: String,
        kind: GateKind,
        class: FailureClass,
    },
    NotApplicableWithoutWaiver {
        check_id: String,
        kind: GateKind,
    },
    ManualPending {
        check_id: String,
    },
}

impl Blocker {
    pub fn check_id(&self) -> &str {
        match self {
            Self::NotRun { check_id, .. }
            | Self::Unknown { check_id, .. }
            | Self::Failed { check_id, .. }
            | Self::NotApplicableWithoutWaiver { check_id, .. }
            | Self::ManualPending { check_id } => check_id,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::NotRun { check_id, kind } => {
                format!("`{check_id}`（{}）没有执行记录", kind.as_str())
            }
            Self::Unknown { check_id, kind } => {
                format!("`{check_id}`（{}）结果为 Unknown", kind.as_str())
            }
            Self::Failed {
                check_id,
                kind,
                class,
            } => format!(
                "`{check_id}`（{}）失败，分类 {}",
                kind.as_str(),
                class.as_str()
            ),
            Self::NotApplicableWithoutWaiver { check_id, kind } => format!(
                "`{check_id}`（{}）标记为 NotApplicable 但没有豁免记录",
                kind.as_str()
            ),
            Self::ManualPending { check_id } => {
                format!("`{check_id}` 为人工项，缺少人工验收证据")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDecision {
    pub accepted: bool,
    pub blockers: Vec<Blocker>,
    /// 本次实际生效的豁免。单独出列，不并入结果。
    pub waivers: Vec<Waiver>,
    /// 建议项的问题：可见，但不阻止。
    pub advisories: Vec<String>,
}

impl GateDecision {
    /// 只有"带豁免接受"才需要额外复核，不能与干净通过混为一谈。
    pub fn accepted_with_waivers(&self) -> bool {
        self.accepted && !self.waivers.is_empty()
    }

    pub fn blocked_checks(&self) -> Vec<String> {
        self.blockers
            .iter()
            .map(|blocker| blocker.check_id().to_string())
            .collect()
    }
}

/// 同一检查多次结果取最差：后面的 Pass 不能掩盖前面的 Fail。
fn worst_outcome<'a>(results: &[&'a CheckResult]) -> Option<&'a CheckResult> {
    results
        .iter()
        .copied()
        .max_by_key(|item| item.outcome.severity())
}

/// 求值门禁。
///
/// 规则：
/// - 没有结果 ≠ Pass，记 `NotRun`；显式 `Unknown` 记 `Unknown`。两者都阻止接受。
/// - 必需/人工项 `Fail` 阻止接受，并按失败分类标注是不是代码缺陷。
/// - `NotApplicable` 只在有对应豁免时放过——否则"我标它不适用"就是绕过门禁的口子。
/// - 建议项的问题进 `advisories`，可见但不阻止。
pub fn evaluate_gate(spec: &GateSpec, results: &[CheckResult], waivers: &[Waiver]) -> GateDecision {
    let mut by_check: BTreeMap<&str, Vec<&CheckResult>> = BTreeMap::new();
    for result in results {
        by_check
            .entry(result.check_id.as_str())
            .or_default()
            .push(result);
    }

    let mut blockers = Vec::new();
    let mut advisories = Vec::new();
    let mut used_waivers: Vec<Waiver> = Vec::new();

    for rule in &spec.rules {
        let waiver = waivers
            .iter()
            .find(|item| item.check_id == rule.check_id)
            .cloned();
        let observed = by_check
            .get(rule.check_id.as_str())
            .and_then(|items| worst_outcome(items));

        let Some(result) = observed else {
            let blocker = match rule.kind {
                GateKind::Advisory => {
                    advisories.push(format!("`{}` 未运行", rule.check_id));
                    None
                }
                GateKind::Manual => Some(Blocker::ManualPending {
                    check_id: rule.check_id.clone(),
                }),
                GateKind::Required => Some(Blocker::NotRun {
                    check_id: rule.check_id.clone(),
                    kind: rule.kind,
                }),
            };
            if let Some(blocker) = blocker {
                if let Some(waiver) = waiver {
                    used_waivers.push(waiver);
                } else {
                    blockers.push(blocker);
                }
            }
            continue;
        };

        let candidate_blocker = match (rule.kind, result.outcome) {
            (GateKind::Advisory, CheckOutcome::Pass) => None,
            (GateKind::Advisory, outcome) => {
                advisories.push(format!("`{}` 为 {}", rule.check_id, outcome.as_str()));
                None
            }
            (_, CheckOutcome::Pass) => None,
            (_, CheckOutcome::Unknown) => Some(Blocker::Unknown {
                check_id: rule.check_id.clone(),
                kind: rule.kind,
            }),
            (_, CheckOutcome::Fail) => {
                let class = result
                    .failure
                    .map(|signal| classify_failure(signal).class)
                    .unwrap_or(FailureClass::Unknown);
                Some(Blocker::Failed {
                    check_id: rule.check_id.clone(),
                    kind: rule.kind,
                    class,
                })
            }
            (_, CheckOutcome::NotApplicable) => Some(Blocker::NotApplicableWithoutWaiver {
                check_id: rule.check_id.clone(),
                kind: rule.kind,
            }),
        };

        if let Some(blocker) = candidate_blocker {
            if let Some(waiver) = waiver {
                used_waivers.push(waiver);
            } else {
                blockers.push(blocker);
            }
        }
    }

    GateDecision {
        accepted: blockers.is_empty(),
        blockers,
        waivers: used_waivers,
        advisories,
    }
}

// ---------------------------------------------------------------------------
// 1.5 失败分类、证据独立性与测试数据
// ---------------------------------------------------------------------------

/// 失败时能拿到的信号。`Timeout` 与 `ScreenshotOnly` 都刻意不给确定结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureSignal {
    /// 种子数据、依赖服务、账号等前置条件没准备好。
    PreconditionMissing,
    /// 容器、端口、网络、浏览器不可用。
    EnvironmentUnavailable,
    /// 业务断言失败。
    AssertionFailed,
    /// 超时：分不清是慢还是死。
    Timeout,
    /// 只有界面出现，没有数据或权限断言。
    ScreenshotOnly,
    /// 失败了但没有可用证据。
    NoSignal,
}

impl FailureSignal {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreconditionMissing => "precondition-missing",
            Self::EnvironmentUnavailable => "environment-unavailable",
            Self::AssertionFailed => "assertion-failed",
            Self::Timeout => "timeout",
            Self::ScreenshotOnly => "screenshot-only",
            Self::NoSignal => "no-signal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Precondition,
    Environment,
    Application,
    Unknown,
}

impl FailureClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Precondition => "precondition",
            Self::Environment => "environment",
            Self::Application => "application",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureVerdict {
    pub class: FailureClass,
    /// 是否可以作为代码缺陷的依据。环境与前置失败一律为 false。
    pub code_defect: bool,
    pub note: String,
}

/// 把信号翻成分类。这里唯一的强判断是"业务断言失败"——其余都不够格归咎代码。
pub fn classify_failure(signal: FailureSignal) -> FailureVerdict {
    match signal {
        FailureSignal::AssertionFailed => FailureVerdict {
            class: FailureClass::Application,
            code_defect: true,
            note: "业务断言失败，进入应用缺陷排查".to_string(),
        },
        FailureSignal::PreconditionMissing => FailureVerdict {
            class: FailureClass::Precondition,
            code_defect: false,
            note: "前置条件未满足：先补齐数据与服务再重跑，不算代码缺陷".to_string(),
        },
        FailureSignal::EnvironmentUnavailable => FailureVerdict {
            class: FailureClass::Environment,
            code_defect: false,
            note: "环境不可用：记为环境失败，不作为代码质量结论".to_string(),
        },
        FailureSignal::Timeout => FailureVerdict {
            class: FailureClass::Unknown,
            code_defect: false,
            note: "超时无法区分慢与死，先看环境与时序证据".to_string(),
        },
        FailureSignal::ScreenshotOnly => FailureVerdict {
            class: FailureClass::Unknown,
            code_defect: false,
            note: "截图只是辅助观察，不能据此判断数据正确或权限安全".to_string(),
        },
        FailureSignal::NoSignal => FailureVerdict {
            class: FailureClass::Unknown,
            code_defect: false,
            note: "没有可用证据，不推断原因".to_string(),
        },
    }
}

/// 证据来源：独立来源（外部系统、基准、契约）还是自己生成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceOrigin {
    IndependentSource,
    SelfGenerated,
}

/// 独立性的四个维度。"换了个人"只是其中最难算数的那一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndependenceInput {
    pub origin: EvidenceOrigin,
    pub author_separated: bool,
    pub permission_separated: bool,
    pub method_separated: bool,
}

impl IndependenceInput {
    pub fn self_generated() -> Self {
        Self {
            origin: EvidenceOrigin::SelfGenerated,
            author_separated: false,
            permission_separated: false,
            method_separated: false,
        }
    }

    pub fn with_author_separated(mut self) -> Self {
        self.author_separated = true;
        self
    }

    pub fn with_method_separated(mut self) -> Self {
        self.method_separated = true;
        self
    }

    pub fn with_permission_separated(mut self) -> Self {
        self.permission_separated = true;
        self
    }
}

/// 补齐独立性的可行路径。不是"再找个人看一眼"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// 引入基准（已知正确输出）。
    Baseline,
    /// 契约用例（对端定义的行为）。
    ContractCases,
    /// 人工抽样。
    ManualSampling,
}

impl Remedy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Baseline => "基准输出",
            Self::ContractCases => "契约用例",
            Self::ManualSampling => "人工抽样",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndependenceBasis {
    SeparateSource,
    SeparatePermission,
    SeparateMethod,
}

impl IndependenceBasis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SeparateSource => "证据来源独立",
            Self::SeparatePermission => "权限边界独立",
            Self::SeparateMethod => "方法独立",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndependenceVerdict {
    Independent {
        basis: IndependenceBasis,
    },
    Insufficient {
        gap: IndependenceGap,
        required: Vec<Remedy>,
    },
}

impl IndependenceVerdict {
    pub fn is_independent(&self) -> bool {
        matches!(self, Self::Independent { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndependenceGap {
    /// 只换了执行人：角色名不同，假设还是同一套。
    RoleRenameOnly,
    /// 同一来源、同一权限、同一方法：共享错误假设。
    SharedAssumption,
}

fn remedies() -> Vec<Remedy> {
    vec![
        Remedy::Baseline,
        Remedy::ContractCases,
        Remedy::ManualSampling,
    ]
}

/// 判定证据独立性。来源/权限/方法任一独立即成立；只换执行人不成立。
pub fn judge_independence(input: &IndependenceInput) -> IndependenceVerdict {
    if input.origin == EvidenceOrigin::IndependentSource {
        return IndependenceVerdict::Independent {
            basis: IndependenceBasis::SeparateSource,
        };
    }
    if input.permission_separated {
        return IndependenceVerdict::Independent {
            basis: IndependenceBasis::SeparatePermission,
        };
    }
    if input.method_separated {
        return IndependenceVerdict::Independent {
            basis: IndependenceBasis::SeparateMethod,
        };
    }
    if input.author_separated {
        return IndependenceVerdict::Insufficient {
            gap: IndependenceGap::RoleRenameOnly,
            required: remedies(),
        };
    }
    IndependenceVerdict::Insufficient {
        gap: IndependenceGap::SharedAssumption,
        required: remedies(),
    }
}

/// 测试允许使用的数据来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestDataUse {
    Synthetic,
    AnonymizedFixture,
    /// 只读生产数据：需要明确授权记录。
    ProductionReadOnly,
    /// 写生产数据：测试里一律不允许。
    ProductionWritable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataAdmission {
    Admitted,
    /// 需要授权但没给：拒绝并说明缺什么。
    NeedsAuthorization {
        missing: String,
    },
    Refused {
        reason: String,
    },
}

impl DataAdmission {
    pub fn admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }
}

/// 准入测试数据用法。写生产系统在测试里无条件拒绝——不是"小心一点"能解决的。
pub fn admit_test_data(use_case: TestDataUse, authorization: Option<&str>) -> DataAdmission {
    match use_case {
        TestDataUse::Synthetic | TestDataUse::AnonymizedFixture => DataAdmission::Admitted,
        TestDataUse::ProductionReadOnly => {
            match authorization.filter(|item| !item.trim().is_empty()) {
                Some(_) => DataAdmission::Admitted,
                None => DataAdmission::NeedsAuthorization {
                    missing: "只读生产数据需要授权记录".to_string(),
                },
            }
        }
        TestDataUse::ProductionWritable => DataAdmission::Refused {
            reason: "测试不得自动写生产系统".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_plan() -> VerificationPlan {
        let mut plan = VerificationPlan::new("plan-1", "rev-1");
        plan.push(PlanItem::new(
            "应用启动",
            "健康检查返回 200",
            CoverageArea::StartupHealth,
            CheckKind::CiJob,
            "ci/smoke",
        ))
        .push(PlanItem::new(
            "核心 API",
            "配额查询返回当前用量",
            CoverageArea::CoreApi,
            CheckKind::Api,
            "curl",
        ))
        .push(PlanItem::new(
            "权限与数据读写",
            "跨租户读被拒绝",
            CoverageArea::PermissionAndData,
            CheckKind::Api,
            "curl",
        ))
        .push(PlanItem::new(
            "创建流程",
            "提交后列表出现新条目",
            CoverageArea::KeyUiFlow,
            CheckKind::BrowserFlow,
            "playwright",
        ))
        .push(PlanItem::new(
            "失败路径",
            "非法参数返回 400 且不落库",
            CoverageArea::FailurePath,
            CheckKind::Api,
            "curl",
        ));
        plan
    }

    fn report(commit: &str, digest: &str, rule: &str) -> VerificationReport {
        VerificationReport {
            report_id: "report-1".to_string(),
            candidate: Candidate::new("cand-1", commit).with_digest(digest),
            spec_revision: "spec-7".to_string(),
            rule_revision: rule.to_string(),
            environment: "staging".to_string(),
            tools: vec![ToolVersion::new("cargo", "1.83.0")],
            executor: Executor::Ci {
                job: "verify/linux".to_string(),
            },
            finished_at: "2026-11-20T10:00:00Z".to_string(),
            raw_ref: "cas://reports/report-1.json".to_string(),
            results: vec![CheckResult::passing(
                "startup",
                "应用启动",
                "2026-11-20T09:58:00Z",
            )],
        }
    }

    fn build(id: &str, platform: &str, environment: &str, digest: &str) -> BuildRecord {
        BuildRecord {
            build_id: id.to_string(),
            source_commit: "c0ffee".to_string(),
            build_definition: "xbuild/3".to_string(),
            toolchain: "rustc-1.83".to_string(),
            dependency_lock_digest: "lock-9".to_string(),
            platform: platform.to_string(),
            parameters_digest: "params-2".to_string(),
            environment: environment.to_string(),
            output_digest: digest.to_string(),
            reproducible: ReproClaim::NotChecked,
        }
    }

    /// 1.1：少一个覆盖面的计划不成立，报出缺的那一面。
    #[test]
    fn plan_covers_all_five_areas_or_is_refused() {
        let mut plan = full_plan();
        assert_eq!(plan.validate(), Ok(()));

        plan.items
            .retain(|item| item.area != CoverageArea::PermissionAndData);
        assert_eq!(
            plan.validate(),
            Err(PlanError::MissingArea(CoverageArea::PermissionAndData))
        );

        let empty = VerificationPlan::new("plan-empty", "rev-1");
        assert_eq!(empty.validate(), Err(PlanError::EmptyItems));
        assert_eq!(empty.missing_areas().len(), CoverageArea::ALL.len());
    }

    /// 1.1：API/浏览器/Agent 三类必须指名已有工具，不能空着。
    #[test]
    fn plan_item_must_name_an_external_tool() {
        let mut plan = full_plan();
        let item = plan
            .items
            .iter_mut()
            .find(|item| item.kind == CheckKind::BrowserFlow)
            .expect("计划里应有浏览器流程项");
        item.tool = "   ".to_string();
        assert_eq!(
            plan.validate(),
            Err(PlanError::MissingTool {
                scenario: "创建流程".to_string()
            })
        );
    }

    /// 1.1：自建浏览器引擎被拒——这一层只复用宿主已有能力。
    #[test]
    fn plan_refuses_a_builtin_browser_harness() {
        let mut plan = full_plan();
        let item = plan
            .items
            .iter_mut()
            .find(|item| item.kind == CheckKind::BrowserFlow)
            .expect("计划里应有浏览器流程项");
        item.tool = "builtin:mini-browser".to_string();
        assert_eq!(
            plan.validate(),
            Err(PlanError::BuiltinHarness {
                scenario: "创建流程".to_string(),
                tool: "builtin:mini-browser".to_string()
            })
        );
    }

    /// 1.2 / `invariant_i_08`：分支旧提交的报告不能用于合并后制品的接受。
    #[test]
    fn report_from_an_older_commit_cannot_accept_the_new_artifact() {
        let old = report("branch-head-1", "sha256:aaa", "rules-4");
        let merged = Candidate::new("cand-merged", "merge-commit-9").with_digest("sha256:bbb");
        let verdict = match_report(&old, &merged, "rules-4");
        assert!(!verdict.accepts());
        assert_eq!(
            verdict,
            EvidenceMatch::StaleCommit {
                report_commit: "branch-head-1".to_string(),
                target_commit: "merge-commit-9".to_string()
            }
        );

        // 同一提交、不同 digest 也不成立：重新构建出的不是同一个包。
        let same_commit = Candidate::new("cand-1", "branch-head-1").with_digest("sha256:bbb");
        assert_eq!(
            match_report(&old, &same_commit, "rules-4"),
            EvidenceMatch::DigestMismatch {
                report_digest: Some("sha256:aaa".to_string()),
                target_digest: "sha256:bbb".to_string()
            }
        );
    }

    /// 1.2：没有原始引用的报告只能是 Unknown，不能被当作可用证据。
    #[test]
    fn report_without_raw_reference_is_unknown() {
        let mut item = report("c1", "sha256:aaa", "rules-4");
        item.raw_ref = String::new();
        let target = Candidate::new("cand-1", "c1").with_digest("sha256:aaa");
        assert_eq!(
            match_report(&item, &target, "rules-4"),
            EvidenceMatch::Unknown {
                reason: "未记录原始报告引用".to_string()
            }
        );
    }

    /// 1.2（规则版本）：规则升级而制品不变时，旧结论标为需重验。
    #[test]
    fn rule_bump_marks_the_old_conclusion_for_revalidation() {
        let item = report("c1", "sha256:aaa", "rules-4");
        let target = Candidate::new("cand-1", "c1").with_digest("sha256:aaa");
        assert_eq!(
            match_report(&item, &target, "rules-4"),
            EvidenceMatch::Usable
        );
        assert_eq!(
            match_report(&item, &target, "rules-5"),
            EvidenceMatch::NeedsRevalidation {
                reason: RevalidationReason::RuleRevisionChanged
            }
        );

        let scope_now = ValidityScope {
            environment: "staging".to_string(),
            rule_revision: "rules-5".to_string(),
            inputs_digest: inputs_digest(&item.candidate),
        };
        assert_eq!(
            item.scope().matches(&scope_now),
            Validity::NeedsRevalidation(RevalidationReason::RuleRevisionChanged)
        );

        let scope_rules4 = ValidityScope {
            environment: "staging".to_string(),
            rule_revision: "rules-4".to_string(),
            inputs_digest: inputs_digest(&item.candidate),
        };
        let same_rules_other_env = ValidityScope {
            environment: "prod".to_string(),
            ..scope_rules4.clone()
        };
        assert_eq!(
            scope_rules4.matches(&same_rules_other_env),
            Validity::NeedsRevalidation(RevalidationReason::EnvironmentChanged)
        );
        assert!(scope_rules4.matches(&scope_rules4).is_valid());
    }

    /// 1.3：测试环境晋升到预发布，复用同一 digest 与同一份验证结论。
    #[test]
    fn promotion_reuses_the_same_digest_across_environments() {
        let mut ledger = BuildLedger::new();
        assert_eq!(
            ledger.record(build("b1", "linux-x86_64", "test", "sha256:pkg")),
            RecordOutcome::Inserted
        );
        let target = Candidate::new("cand-1", "c0ffee").with_digest("sha256:pkg");
        let evidence = match_report(
            &report("c0ffee", "sha256:pkg", "rules-4"),
            &target,
            "rules-4",
        );
        assert!(evidence.accepts());

        let promotion = ledger.promote(
            &PromotionRequest::new("sha256:pkg", "test", "staging", "linux-x86_64"),
            &evidence,
        );
        match promotion {
            PromotionOutcome::Reused {
                build_id,
                digest,
                to_environment,
                evidence,
            } => {
                assert_eq!(build_id, "b1", "晋升复用同一构建记录，不新建");
                assert_eq!(digest, "sha256:pkg");
                assert_eq!(to_environment, "staging");
                assert_eq!(evidence, PromotionEvidence::Reusable);
            }
            other => panic!("应复用同一 digest，实际 {other:?}"),
        }
        assert_eq!(ledger.len(), 1, "晋升不产生新构建记录");

        // 未验证的目标不变：digest 相同但证据不匹配时，复用可以，证据不能跟着走。
        let stale = match_report(&report("old", "sha256:pkg", "rules-4"), &target, "rules-4");
        match ledger.promote(
            &PromotionRequest::new("sha256:pkg", "test", "staging", "linux-x86_64"),
            &stale,
        ) {
            PromotionOutcome::Reused { evidence, .. } => {
                assert_eq!(evidence, PromotionEvidence::NotReusable(stale));
            }
            other => panic!("应复用 digest 并标证据不可复用，实际 {other:?}"),
        }
    }

    /// 1.3：换平台必须重建，生成新记录；身份摘要随平台变化。
    #[test]
    fn platform_specific_rebuild_creates_a_new_record() {
        let mut ledger = BuildLedger::new();
        let linux = build("b1", "linux-x86_64", "test", "sha256:pkg-linux");
        let identity_linux = linux.identity_digest();
        ledger.record(linux);

        let promotion = ledger.promote(
            &PromotionRequest::new("sha256:pkg-linux", "test", "staging", "windows-x86_64"),
            &EvidenceMatch::Usable,
        );
        assert_eq!(
            promotion,
            PromotionOutcome::RebuildRequired {
                reason: RebuildReason::PlatformDiffers,
                recorded_platform: Some("linux-x86_64".to_string()),
                requested_platform: "windows-x86_64".to_string()
            }
        );

        let windows = build("b2", "windows-x86_64", "test", "sha256:pkg-windows");
        assert_ne!(
            windows.identity_digest(),
            identity_linux,
            "平台进身份摘要，不能当作同一个包"
        );
        ledger.record(windows);
        assert_eq!(ledger.len(), 2, "平台重建是新记录");

        // 环境不进身份：同一平台在两个环境各构建一次，身份相同。
        let staging = build("b3", "linux-x86_64", "staging", "sha256:pkg-linux");
        assert_eq!(staging.identity_digest(), identity_linux);
    }

    /// 1.4：必需项没有执行记录时结果是 Unknown，接受被阻止。
    #[test]
    fn unrun_required_check_stays_unknown_and_blocks_acceptance() {
        let spec = GateSpec::new(vec![
            GateRule::required("startup", "启动失败则一切无意义"),
            GateRule::required("permission", "权限回归是发布前置"),
        ]);
        let results = vec![CheckResult::passing(
            "startup",
            "应用启动",
            "2026-11-20T09:58:00Z",
        )];
        let decision = evaluate_gate(&spec, &results, &[]);
        assert!(!decision.accepted);
        assert_eq!(decision.blocked_checks(), vec!["permission".to_string()]);
        assert_eq!(
            decision.blockers[0],
            Blocker::NotRun {
                check_id: "permission".to_string(),
                kind: GateKind::Required
            }
        );
        assert!(decision.blockers[0].describe().contains("没有执行记录"));
        assert!(!decision.accepted_with_waivers());
    }

    /// 1.4：豁免单独留痕——结果仍是 Fail，接受靠的是那张单独记录的豁免。
    #[test]
    fn waiver_is_recorded_separately_from_the_result() {
        let spec = GateSpec::new(vec![GateRule::required("permission", "权限回归是发布前置")]);
        let results = vec![CheckResult::failing(
            "permission",
            "跨租户读",
            FailureSignal::AssertionFailed,
            "2026-11-20T10:00:00Z",
        )];

        let blocked = evaluate_gate(&spec, &results, &[]);
        assert!(!blocked.accepted);
        assert!(blocked.waivers.is_empty());

        let waiver = Waiver::new(
            "permission",
            "lead-1",
            "已知缺陷，下个迭代修",
            "2026-11-20T11:00:00Z",
        );
        let decision = evaluate_gate(&spec, &results, std::slice::from_ref(&waiver));
        assert!(decision.accepted, "有豁免时可以接受");
        assert!(decision.accepted_with_waivers(), "必须标出这是带豁免接受");
        assert_eq!(decision.waivers, vec![waiver], "豁免单独记录");
        assert_eq!(results[0].outcome, CheckOutcome::Fail, "豁免不改写结果本身");
    }

    /// 1.4：建议项失败可见，但不阻止接受。
    #[test]
    fn advisory_failure_is_listed_but_does_not_block() {
        let spec = GateSpec::new(vec![
            GateRule::required("startup", "启动失败则一切无意义"),
            GateRule::advisory("perf", "性能不达标不阻断，但要可见"),
        ]);
        let results = vec![
            CheckResult::passing("startup", "应用启动", "2026-11-20T09:58:00Z"),
            CheckResult::failing(
                "perf",
                "首屏 2s",
                FailureSignal::AssertionFailed,
                "2026-11-20T10:02:00Z",
            ),
        ];
        let decision = evaluate_gate(&spec, &results, &[]);
        assert!(decision.accepted, "建议项失败不阻断");
        assert_eq!(decision.advisories.len(), 1);
        assert!(decision.advisories[0].contains("perf"));
        assert!(decision.blockers.is_empty());
    }

    /// 1.4：必需项标 NotApplicable 而没豁免 → 阻止，避免"我标它不适用"绕过门禁。
    #[test]
    fn not_applicable_required_check_needs_a_waiver() {
        let spec = GateSpec::new(vec![GateRule::required("migration", "迁移必须演过")]);
        let mut result = CheckResult::passing("migration", "迁移演练", "2026-11-20T10:00:00Z");
        result.outcome = CheckOutcome::NotApplicable;
        let decision = evaluate_gate(&spec, &[result.clone()], &[]);
        assert!(!decision.accepted);
        assert_eq!(
            decision.blockers[0],
            Blocker::NotApplicableWithoutWaiver {
                check_id: "migration".to_string(),
                kind: GateKind::Required
            }
        );

        let waiver = Waiver::new("migration", "dba-2", "本版本无迁移", "2026-11-20T11:00:00Z");
        assert!(evaluate_gate(&spec, &[result], &[waiver]).accepted);
    }

    /// 1.5：环境不可用记为环境失败，不归咎代码。
    #[test]
    fn environment_failure_is_not_attributed_to_the_code() {
        let verdict = classify_failure(FailureSignal::EnvironmentUnavailable);
        assert_eq!(verdict.class, FailureClass::Environment);
        assert!(!verdict.code_defect);
        assert!(verdict.note.contains("不作为代码质量结论"));

        let precondition = classify_failure(FailureSignal::PreconditionMissing);
        assert_eq!(precondition.class, FailureClass::Precondition);
        assert!(!precondition.code_defect);

        let application = classify_failure(FailureSignal::AssertionFailed);
        assert_eq!(application.class, FailureClass::Application);
        assert!(application.code_defect, "只有业务断言失败才归咎代码");
    }

    /// 1.5：只有截图不算通过，也不算代码缺陷的结论。
    #[test]
    fn screenshot_only_signal_is_not_a_pass() {
        let verdict = classify_failure(FailureSignal::ScreenshotOnly);
        assert_eq!(verdict.class, FailureClass::Unknown);
        assert!(!verdict.code_defect);
        assert!(verdict.note.contains("截图只是辅助观察"));

        let spec = GateSpec::new(vec![GateRule::required("permission", "权限必须断言")]);
        let result = CheckResult::failing(
            "permission",
            "跨租户读",
            FailureSignal::ScreenshotOnly,
            "2026-11-20T10:00:00Z",
        );
        let decision = evaluate_gate(&spec, &[result], &[]);
        assert!(!decision.accepted);
        assert_eq!(
            decision.blockers[0],
            Blocker::Failed {
                check_id: "permission".to_string(),
                kind: GateKind::Required,
                class: FailureClass::Unknown
            }
        );
    }

    /// 1.5：只换执行人（角色名不同）不构成独立验证。
    #[test]
    fn renaming_the_reviewer_is_not_independence() {
        let renamed = IndependenceInput::self_generated().with_author_separated();
        match judge_independence(&renamed) {
            IndependenceVerdict::Insufficient { gap, required } => {
                assert_eq!(gap, IndependenceGap::RoleRenameOnly);
                assert_eq!(required, remedies());
            }
            other => panic!("只换人不算独立，实际 {other:?}"),
        }

        let method = IndependenceInput::self_generated().with_method_separated();
        assert_eq!(
            judge_independence(&method),
            IndependenceVerdict::Independent {
                basis: IndependenceBasis::SeparateMethod
            }
        );
    }

    /// 1.5：自生成实现 + 自生成测试共享假设时，必须补基准/契约/人工抽样。
    #[test]
    fn self_generated_test_needs_a_baseline_or_manual_sampling() {
        let shared = IndependenceInput::self_generated();
        match judge_independence(&shared) {
            IndependenceVerdict::Insufficient { gap, required } => {
                assert_eq!(gap, IndependenceGap::SharedAssumption);
                assert!(required.contains(&Remedy::Baseline));
                assert!(required.contains(&Remedy::ContractCases));
                assert!(required.contains(&Remedy::ManualSampling));
            }
            other => panic!("共享假设必须有补救路径，实际 {other:?}"),
        }

        let external = IndependenceInput {
            origin: EvidenceOrigin::IndependentSource,
            ..IndependenceInput::self_generated()
        };
        assert!(judge_independence(&external).is_independent());
    }

    /// 1.5：测试不得自动写生产系统；只读生产数据要授权记录。
    #[test]
    fn production_data_use_is_admitted_only_with_authorization() {
        assert!(admit_test_data(TestDataUse::Synthetic, None).admitted());
        assert!(admit_test_data(TestDataUse::AnonymizedFixture, None).admitted());
        assert_eq!(
            admit_test_data(TestDataUse::ProductionReadOnly, None),
            DataAdmission::NeedsAuthorization {
                missing: "只读生产数据需要授权记录".to_string()
            }
        );
        assert!(admit_test_data(TestDataUse::ProductionReadOnly, Some("auth-7")).admitted());
        assert!(matches!(
            admit_test_data(TestDataUse::ProductionWritable, Some("auth-7")),
            DataAdmission::Refused { .. }
        ));
    }
}
