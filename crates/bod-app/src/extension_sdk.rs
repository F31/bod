//! P6 任务 1.1—1.5：第三方适配器与 SDK 的接入边界（`sdk-extension`）。
//!
//! 外部运行时扩展能力，但不能改宿主安装、不能自证可用、不能悄悄把项目材料送出去。四道边界：
//! 1. **协商（1.1）**：描述符带版本、平台、能力分项与限制；宿主按能力握手，缺必需能力就拒，
//!    且**拒绝的是新扩展任务**，不牵连旧适配器原本地路径；每个受支持的配置 schema 版本都备
//!    一份 fixture，没 fixture 的版本不许装。
//! 2. **安装（1.2 / G-5、`invariant_i_15`）**：版本/来源/完整性/许可证先过关；没有 conformance
//!    报告只能以 `Experimental` 装来调试，不进调度候选、不标 Supported；报告张冠李戴一律拒。
//! 3. **证据分项（1.3 / `invariant_i_57`）**：自报 / 探测 / 契约测试 / 真实任务四级互不等价；
//!    身份、授权、消息完整性任一项失败即禁用执行，降级徽章不能豁免；探测结论就是调度与界面
//!    的唯一来源，两边不一致按缺陷处理。
//! 4. **清单复核（1.4）**：可执行摘要或工具清单变化即触发重新评估，旧批准只对原清单有效。
//! 5. **出站范围（1.5 / SB06）**：首次外发要展示项目、范围、接收方与账号并保存决定摘要；
//!    任一变化重新判断，未获准时接收端零次收到材料；审计只留引用与摘要。
//! 6. **支持声明（1.6）**：对外宣称一条能力必须同时给出确定版本、具体平台与对应报告；
//!    自报级只能说自己是自报级。
//!
//! 本模块不下载、不执行外部程序、不落 DB：`adapter_conformance` 负责跑测试，这里只判
//! "能不能装、能不能派"。

use crate::adapter_conformance::{ConformanceReport, Enablement, enablement};
use crate::stage_manifest::looks_like_credential;

// ---------------------------------------------------------------------------
// 1.1 描述符与能力协商
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// 原本地执行能力：旧适配器至少要有这个。
    LocalExecution,
    /// 委派到远端节点。
    Delegation,
    /// 流式事件。
    Streaming,
    /// 产出制品。
    Artifacts,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalExecution => "local_execution",
            Self::Delegation => "delegation",
            Self::Streaming => "streaming",
            Self::Artifacts => "artifacts",
        }
    }
}

/// 证据等级。声明"支持"不算证据，等级高的才算数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceTier {
    /// 自报：适配器自己写在描述符里。
    SelfReported,
    /// 探测：宿主握手时实测到。
    Probed,
    /// 契约测试：conformance 用例覆盖。
    ContractTested,
    /// 真实任务：在真实项目里跑过。
    RealTask,
}

impl EvidenceTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfReported => "self_reported",
            Self::Probed => "probed",
            Self::ContractTested => "contract_tested",
            Self::RealTask => "real_task",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDecl {
    pub capability: Capability,
    pub tier: EvidenceTier,
    /// 已知限制：界面与调度都要显示同一份。
    pub limits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterDescriptor {
    pub adapter_id: String,
    pub adapter_version: String,
    pub protocol_version: u32,
    pub platform: String,
    pub config_schema_version: u32,
    pub capabilities: Vec<CapabilityDecl>,
    pub limitations: Vec<String>,
}

impl AdapterDescriptor {
    pub fn new(adapter_id: &str, adapter_version: &str, protocol_version: u32) -> Self {
        Self {
            adapter_id: adapter_id.to_string(),
            adapter_version: adapter_version.to_string(),
            protocol_version,
            platform: "linux-x86_64".to_string(),
            config_schema_version: 1,
            capabilities: Vec::new(),
            limitations: Vec::new(),
        }
    }

    pub fn with_capability(mut self, capability: Capability, tier: EvidenceTier) -> Self {
        self.capabilities.push(CapabilityDecl {
            capability,
            tier,
            limits: Vec::new(),
        });
        self
    }

    pub fn with_platform(mut self, platform: &str) -> Self {
        self.platform = platform.to_string();
        self
    }

    pub fn tier_of(&self, capability: Capability) -> Option<EvidenceTier> {
        self.capabilities
            .iter()
            .find(|decl| decl.capability == capability)
            .map(|decl| decl.tier)
    }

    pub fn limits_of(&self, capability: Capability) -> Option<&[String]> {
        self.capabilities
            .iter()
            .find(|decl| decl.capability == capability)
            .map(|decl| decl.limits.as_slice())
    }

    /// 旧适配器的原本地路径：有 `LocalExecution` 就还能干活，与是否支持新扩展无关。
    pub fn keeps_local_path(&self) -> bool {
        self.tier_of(Capability::LocalExecution).is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRequirements {
    pub protocol_version: u32,
    pub required: Vec<Capability>,
    pub min_tier: EvidenceTier,
}

impl HostRequirements {
    pub fn new(protocol_version: u32, required: Vec<Capability>, min_tier: EvidenceTier) -> Self {
        Self {
            protocol_version,
            required,
            min_tier,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationReject {
    ProtocolTooOld {
        adapter: u32,
        host: u32,
    },
    MissingCapability(Capability),
    EvidenceTooLow {
        capability: Capability,
        tier: EvidenceTier,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Negotiation {
    Accepted { protocol_version: u32 },
    Rejected(NegotiationReject),
}

/// 能力协商。协议版本先判，再逐项判必需能力与证据等级。
pub fn negotiate(descriptor: &AdapterDescriptor, host: &HostRequirements) -> Negotiation {
    if descriptor.protocol_version < host.protocol_version {
        return Negotiation::Rejected(NegotiationReject::ProtocolTooOld {
            adapter: descriptor.protocol_version,
            host: host.protocol_version,
        });
    }
    for capability in &host.required {
        match descriptor.tier_of(*capability) {
            None => {
                return Negotiation::Rejected(NegotiationReject::MissingCapability(*capability));
            }
            Some(tier) if tier < host.min_tier => {
                return Negotiation::Rejected(NegotiationReject::EvidenceTooLow {
                    capability: *capability,
                    tier,
                });
            }
            Some(_) => {}
        }
    }
    Negotiation::Accepted {
        protocol_version: host.protocol_version,
    }
}

// ---------------------------------------------------------------------------
// 1.1 配置 schema 的版本化 fixtures
// ---------------------------------------------------------------------------

/// 每个受支持的 `config_schema_version` 备一份最小样本：描述符长什么样、配置长什么样。
/// 安装、升级、运行前都拿它核对，没有 fixture 的版本不许装——"能解析"不等于"我们测过"。
pub struct SchemaFixture {
    pub schema_version: u32,
    pub descriptor: &'static str,
    pub config: &'static str,
}

pub const SCHEMA_FIXTURES: &[SchemaFixture] = &[
    SchemaFixture {
        schema_version: 1,
        descriptor: r#"{"adapter_id":"adapter-local","adapter_version":"1.2.0","protocol_version":2,"platform":"linux-x86_64","config_schema_version":1,"capabilities":[{"capability":"local_execution","tier":"contract_tested","limits":[]}],"limitations":[]}"#,
        config: r#"{"model":"local","timeout_ms":60000}"#,
    },
    SchemaFixture {
        schema_version: 2,
        descriptor: r#"{"adapter_id":"adapter-local","adapter_version":"1.3.0","protocol_version":2,"platform":"linux-x86_64","config_schema_version":2,"capabilities":[{"capability":"local_execution","tier":"contract_tested","limits":[]},{"capability":"streaming","tier":"probed","limits":["按 500ms 批量推送"]}],"limitations":[]}"#,
        config: r#"{"model":"local","timeout_ms":60000,"event_stream":{"mode":"poll","batch_ms":500}}"#,
    },
    SchemaFixture {
        schema_version: 3,
        descriptor: r#"{"adapter_id":"adapter-local","adapter_version":"1.4.0","protocol_version":2,"platform":"linux-x86_64","config_schema_version":3,"capabilities":[{"capability":"local_execution","tier":"contract_tested","limits":[]},{"capability":"streaming","tier":"probed","limits":["按 500ms 批量推送"]},{"capability":"artifacts","tier":"contract_tested","limits":["只收相对路径"]}],"limitations":[]}"#,
        config: r#"{"model":"local","timeout_ms":60000,"event_stream":{"mode":"poll","batch_ms":500},"artifacts":{"root":"out","absolute_paths":false}}"#,
    },
];

pub fn fixture_for(schema_version: u32) -> Option<&'static SchemaFixture> {
    SCHEMA_FIXTURES
        .iter()
        .find(|fixture| fixture.schema_version == schema_version)
}

// ---------------------------------------------------------------------------
// 1.2 安装边界（G-5 / invariant_i_15）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPackage {
    pub descriptor: AdapterDescriptor,
    /// 安装来源（registry id 或 host）。
    pub source: String,
    pub digest: String,
    pub license: String,
    /// 可执行文件摘要：升级换包即变。
    pub executable_digest: String,
    pub tool_manifest: Vec<String>,
    pub evidence_date: String,
}

impl ExtensionPackage {
    pub fn new(descriptor: AdapterDescriptor, source: &str, digest: &str) -> Self {
        Self {
            descriptor,
            source: source.to_string(),
            digest: digest.to_string(),
            license: String::new(),
            executable_digest: format!("sha256:{:064x}", 1),
            tool_manifest: Vec::new(),
            evidence_date: "2026-11-21".to_string(),
        }
    }

    pub fn with_license(mut self, license: &str) -> Self {
        self.license = license.to_string();
        self
    }

    pub fn with_tools(mut self, tools: &[&str]) -> Self {
        self.tool_manifest = tools.iter().map(|tool| tool.to_string()).collect();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPolicy {
    pub allowed_sources: Vec<String>,
    pub allowed_licenses: Vec<String>,
    pub max_protocol_version: u32,
    pub max_schema_version: u32,
}

impl Default for ExtensionPolicy {
    fn default() -> Self {
        Self {
            allowed_sources: vec!["registry-official".to_string()],
            allowed_licenses: vec!["Apache-2.0".to_string(), "MIT".to_string()],
            max_protocol_version: 2,
            max_schema_version: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionBlock {
    UnpinnedVersion,
    UnpinnedDigest,
    UntrustedSource,
    LicenseMissing,
    LicenseNotAllowed,
    ProtocolTooNew {
        adapter: u32,
        host: u32,
    },
    SchemaTooNew {
        adapter: u32,
        host: u32,
    },
    /// 宿主没有这个 schema 版本的 fixture：没测过就不许装。
    SchemaFixtureMissing {
        version: u32,
    },
    EmbeddedSecret,
    /// 报告不是这个适配器的：不能拿别的适配器（或自测套件）的报告冒充证据。
    ReportAdapterMismatch {
        report: String,
        package: String,
    },
    /// conformance 的闸门类用例失败：禁止启用（`invariant_i_57`）。
    ConformanceBlocked {
        cases: Vec<&'static str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// 通过 conformance，可标 Supported 并进调度候选。
    Supported {
        cases_passed: usize,
    },
    /// 只通过 schema / 无报告：可装来调试，不进调度候选。
    Experimental {
        why: &'static str,
    },
    Rejected(AdmissionBlock),
}

impl Admission {
    /// 只有 Supported 才进调度候选。
    pub fn in_scheduling_pool(&self) -> bool {
        matches!(self, Self::Supported { .. })
    }

    pub fn is_supported_label(&self) -> bool {
        self.in_scheduling_pool()
    }
}

fn pinned_version(version: &str) -> bool {
    !version.trim().is_empty()
        && version.chars().any(|ch| ch.is_ascii_digit())
        && !version.contains(['^', '~', '*', '>', '<', ',', ' '])
        && !version.eq_ignore_ascii_case("latest")
}

fn pinned_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// 安装准入。顺序：版本 → digest → 来源 → 许可证 → 协议/schema → 密钥 → conformance。
pub fn admit(
    package: &ExtensionPackage,
    policy: &ExtensionPolicy,
    conformance: Option<&ConformanceReport>,
) -> Admission {
    if !pinned_version(&package.descriptor.adapter_version) {
        return Admission::Rejected(AdmissionBlock::UnpinnedVersion);
    }
    if !pinned_digest(&package.digest) {
        return Admission::Rejected(AdmissionBlock::UnpinnedDigest);
    }
    if !policy.allowed_sources.contains(&package.source) {
        return Admission::Rejected(AdmissionBlock::UntrustedSource);
    }
    if package.license.trim().is_empty() {
        return Admission::Rejected(AdmissionBlock::LicenseMissing);
    }
    if !policy.allowed_licenses.contains(&package.license) {
        return Admission::Rejected(AdmissionBlock::LicenseNotAllowed);
    }
    if package.descriptor.protocol_version > policy.max_protocol_version {
        return Admission::Rejected(AdmissionBlock::ProtocolTooNew {
            adapter: package.descriptor.protocol_version,
            host: policy.max_protocol_version,
        });
    }
    if package.descriptor.config_schema_version > policy.max_schema_version {
        return Admission::Rejected(AdmissionBlock::SchemaTooNew {
            adapter: package.descriptor.config_schema_version,
            host: policy.max_schema_version,
        });
    }
    if fixture_for(package.descriptor.config_schema_version).is_none() {
        return Admission::Rejected(AdmissionBlock::SchemaFixtureMissing {
            version: package.descriptor.config_schema_version,
        });
    }
    let carries_secret = package
        .descriptor
        .limits_of(Capability::LocalExecution)
        .unwrap_or(&[])
        .iter()
        .chain(package.descriptor.limitations.iter())
        .any(|line| looks_like_credential(line));
    if carries_secret {
        return Admission::Rejected(AdmissionBlock::EmbeddedSecret);
    }

    match conformance {
        None => Admission::Experimental {
            why: "缺 conformance 报告：可装来调试，不进调度候选",
        },
        Some(report) if report.adapter_id != package.descriptor.adapter_id => {
            Admission::Rejected(AdmissionBlock::ReportAdapterMismatch {
                report: report.adapter_id.clone(),
                package: package.descriptor.adapter_id.clone(),
            })
        }
        Some(report) => match enablement(report) {
            Enablement::Enabled => Admission::Supported {
                cases_passed: report.summary().0,
            },
            Enablement::Unverified { cases } => {
                let _ = cases;
                Admission::Experimental {
                    why: "conformance 有闸门用例未取得证据：不得标 Supported",
                }
            }
            Enablement::Blocked { failures } => {
                Admission::Rejected(AdmissionBlock::ConformanceBlocked {
                    cases: failures.iter().map(|(id, _)| *id).collect(),
                })
            }
        },
    }
}

// ---------------------------------------------------------------------------
// 1.3 分项证据与界面一致（invariant_i_57）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityState {
    Supported,
    Degraded { reason: String },
    Unsupported,
}

impl CapabilityState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Degraded { .. } => "degraded",
            Self::Unsupported => "unsupported",
        }
    }
}

/// 一次探测的结果。三项安全项与能力状态分开记：安全项失败不能被"降级"掩盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub capability: Capability,
    pub state: CapabilityState,
    pub identity_ok: bool,
    pub authorization_ok: bool,
    pub message_integrity_ok: bool,
}

/// 有效状态：安全项任一失败即 `Unsupported`，与自报的 `Degraded` 无关。
pub fn effective_state(probe: &ProbeResult) -> CapabilityState {
    if !probe.identity_ok || !probe.authorization_ok || !probe.message_integrity_ok {
        return CapabilityState::Unsupported;
    }
    probe.state.clone()
}

/// 调度与界面共用的视图：只有一份来源，不允许两边各算一次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityView {
    pub capability: Capability,
    pub state: CapabilityState,
    pub dispatch_enabled: bool,
    pub badge: &'static str,
}

pub fn view_for(probe: &ProbeResult) -> CapabilityView {
    let state = effective_state(probe);
    let dispatch_enabled = matches!(
        state,
        CapabilityState::Supported | CapabilityState::Degraded { .. }
    );
    CapabilityView {
        capability: probe.capability,
        badge: match state {
            CapabilityState::Supported => "available",
            CapabilityState::Degraded { .. } => "degraded",
            CapabilityState::Unsupported => "unavailable",
        },
        state,
        dispatch_enabled,
    }
}

pub fn dispatch_allowed(probe: &ProbeResult) -> bool {
    view_for(probe).dispatch_enabled
}

// ---------------------------------------------------------------------------
// 1.4 清单变化复核
// ---------------------------------------------------------------------------

/// 工具清单摘要：排序后拼接，顺序变化不算变化，内容变化才算。
pub fn tool_manifest_digest(tools: &[String]) -> String {
    let mut sorted = tools.to_vec();
    sorted.sort();
    sorted.dedup();
    format!("sha256:tools:{}", sorted.join(","))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionApproval {
    pub adapter_id: String,
    pub executable_digest: String,
    pub tool_manifest_digest: String,
    pub schema_version: u32,
    pub approved_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapprovalReason {
    ExecutableChanged,
    ToolManifestChanged,
    SchemaChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reapproval {
    Valid,
    Required(ReapprovalReason),
}

/// 旧批准只对原清单有效：任何一项变了都要重新评估。
pub fn approval_valid(approval: &ExtensionApproval, package: &ExtensionPackage) -> Reapproval {
    if approval.adapter_id != package.descriptor.adapter_id {
        return Reapproval::Required(ReapprovalReason::ToolManifestChanged);
    }
    if approval.executable_digest != package.executable_digest {
        return Reapproval::Required(ReapprovalReason::ExecutableChanged);
    }
    if approval.tool_manifest_digest != tool_manifest_digest(&package.tool_manifest) {
        return Reapproval::Required(ReapprovalReason::ToolManifestChanged);
    }
    if approval.schema_version != package.descriptor.config_schema_version {
        return Reapproval::Required(ReapprovalReason::SchemaChanged);
    }
    Reapproval::Valid
}

// ---------------------------------------------------------------------------
// 1.5 出站材料范围（SB06）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutboundScope {
    /// 只发摘要。
    Summary,
    /// 发日志。
    Logs,
    /// 发制品。
    Artifacts,
    /// 发完整源文件。
    FullSource,
}

impl OutboundScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Logs => "logs",
            Self::Artifacts => "artifacts",
            Self::FullSource => "full_source",
        }
    }

    pub fn is_wider_than(self, other: Self) -> bool {
        self > other
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundDecision {
    pub project_id: String,
    pub scope: OutboundScope,
    pub recipient: String,
    pub account_id: String,
    pub decided_at: String,
    /// 决定摘要：审计只用它，不复制原文。
    pub decision_digest: String,
}

impl OutboundDecision {
    pub fn new(project_id: &str, scope: OutboundScope, recipient: &str, account_id: &str) -> Self {
        let decision_digest = format!(
            "sha256:outbound:{project_id}:{}:{recipient}:{account_id}",
            scope.as_str()
        );
        Self {
            project_id: project_id.to_string(),
            scope,
            recipient: recipient.to_string(),
            account_id: account_id.to_string(),
            decided_at: "2026-11-21T09:00:00Z".to_string(),
            decision_digest,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundRequest {
    pub project_id: String,
    pub scope: OutboundScope,
    pub recipient: String,
    pub account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendGuard {
    Allowed,
    /// 有决定但范围/接收方/账号变了：重新判断，之前不算数。
    NeedsDecision {
        changed: Vec<&'static str>,
    },
    /// 根本没有决定：接收端零次收到材料。
    ZeroDelivery {
        reason: &'static str,
    },
}

/// 发送前的最后一道闸。允许 = 决定与请求逐项一致。
pub fn guard_send(decision: Option<&OutboundDecision>, request: &OutboundRequest) -> SendGuard {
    let Some(decision) = decision else {
        return SendGuard::ZeroDelivery {
            reason: "首次外发必须有明确决定",
        };
    };
    let mut changed = Vec::new();
    if decision.project_id != request.project_id {
        changed.push("project");
    }
    if decision.scope != request.scope {
        changed.push("scope");
    }
    if decision.recipient != request.recipient {
        changed.push("recipient");
    }
    if decision.account_id != request.account_id {
        changed.push("account");
    }
    if changed.is_empty() {
        SendGuard::Allowed
    } else {
        SendGuard::NeedsDecision { changed }
    }
}

/// 外发审计：只有引用与摘要，没有原文与凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialAudit {
    pub project_ref: String,
    pub scope: &'static str,
    pub recipient_ref: String,
    pub decision_ref: String,
    pub decided_at: String,
}

pub fn audit_for(decision: &OutboundDecision) -> MaterialAudit {
    MaterialAudit {
        project_ref: decision.project_id.clone(),
        scope: decision.scope.as_str(),
        recipient_ref: format!("recipient:{}", decision.recipient),
        decision_ref: decision.decision_digest.clone(),
        decided_at: decision.decided_at.clone(),
    }
}

// ---------------------------------------------------------------------------
// 1.6 支持声明必须带版本、平台与报告
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimReject {
    /// 描述符里根本没声明这项能力：不能替它宣称支持。
    NoSuchCapability,
    /// 版本不是确定的：不知道测的是哪一版。
    UnpinnedVersion,
    /// 平台是 `any` / 空 / `unknown`：没有可复现的环境。
    UnknownPlatform,
    /// 声明了契约测试或真实任务等级，却拿不出报告。
    MissingReport,
    /// 报告不是这个适配器的。
    ReportAdapterMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportClaim {
    pub capability: Capability,
    pub adapter_version: String,
    pub platform: String,
    /// 报告引用；自报/探测级为 `self_reported`，不得当契约证据读。
    pub report_ref: String,
    pub tier: EvidenceTier,
}

/// 一条可对外发布的支持声明。三项缺一不可：确定版本、具体平台、对应报告。
pub fn support_claim(
    descriptor: &AdapterDescriptor,
    capability: Capability,
    report: Option<&ConformanceReport>,
) -> Result<SupportClaim, ClaimReject> {
    let tier = descriptor
        .tier_of(capability)
        .ok_or(ClaimReject::NoSuchCapability)?;
    if !pinned_version(&descriptor.adapter_version) {
        return Err(ClaimReject::UnpinnedVersion);
    }
    let platform = descriptor.platform.trim();
    if platform.is_empty()
        || platform.eq_ignore_ascii_case("any")
        || platform.eq_ignore_ascii_case("unknown")
    {
        return Err(ClaimReject::UnknownPlatform);
    }
    let report_ref = match (tier, report) {
        // 自报与探测级本来就不需要契约报告，如实标注即可。
        (EvidenceTier::SelfReported | EvidenceTier::Probed, _) => "self_reported".to_string(),
        (_, Some(report)) if report.adapter_id == descriptor.adapter_id => {
            format!(
                "conformance:{}@{}",
                report.adapter_id, report.adapter_version
            )
        }
        (_, Some(_)) => return Err(ClaimReject::ReportAdapterMismatch),
        (_, None) => return Err(ClaimReject::MissingReport),
    };
    Ok(SupportClaim {
        capability,
        adapter_version: descriptor.adapter_version.clone(),
        platform: platform.to_string(),
        report_ref,
        tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_conformance::{
        CASES, CaseOutcome, CaseStatus, ConformanceReport, run_fake_suite,
    };

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// 一份"闸门类全部通过"的报告：用来验证 Supported 路径本身。
    fn all_pass_report(adapter_id: &str) -> ConformanceReport {
        ConformanceReport {
            adapter_id: adapter_id.to_string(),
            adapter_version: "1.2.0".to_string(),
            protocol: "2".to_string(),
            platform: "linux-x86_64".to_string(),
            generated_at_ms: 0,
            outcomes: CASES
                .iter()
                .map(|case| CaseOutcome {
                    id: case.id,
                    requirement: case.requirement,
                    status: CaseStatus::Pass { assertions: 3 },
                })
                .collect(),
        }
    }

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new("adapter-local", "1.2.0", 2)
            .with_capability(Capability::LocalExecution, EvidenceTier::ContractTested)
    }

    fn package() -> ExtensionPackage {
        ExtensionPackage::new(descriptor(), "registry-official", DIGEST)
            .with_license("Apache-2.0")
            .with_tools(&["rustc", "git"])
    }

    /// 1.1：缺必需能力只拒新扩展任务，原本地路径照旧。
    #[test]
    fn task_1_1_missing_required_capability_keeps_local_path() {
        let host = HostRequirements::new(
            2,
            vec![Capability::LocalExecution, Capability::Delegation],
            EvidenceTier::Probed,
        );
        let result = negotiate(&descriptor(), &host);
        assert_eq!(
            result,
            Negotiation::Rejected(NegotiationReject::MissingCapability(Capability::Delegation))
        );
        assert!(
            descriptor().keeps_local_path(),
            "旧适配器的原本地路径不因新扩展被拒而失效"
        );

        let upgraded = descriptor().with_capability(Capability::Delegation, EvidenceTier::Probed);
        assert!(matches!(
            negotiate(&upgraded, &host),
            Negotiation::Accepted { .. }
        ));
    }

    /// 1.1：协议版本与证据等级分别把关。
    #[test]
    fn task_1_1_protocol_and_evidence_tier_are_checked_separately() {
        let host = HostRequirements::new(3, vec![Capability::LocalExecution], EvidenceTier::Probed);
        assert_eq!(
            negotiate(&descriptor(), &host),
            Negotiation::Rejected(NegotiationReject::ProtocolTooOld {
                adapter: 2,
                host: 3
            })
        );

        let self_reported = AdapterDescriptor::new("adapter-local", "1.2.0", 3)
            .with_capability(Capability::LocalExecution, EvidenceTier::SelfReported);
        assert_eq!(
            negotiate(&self_reported, &host),
            Negotiation::Rejected(NegotiationReject::EvidenceTooLow {
                capability: Capability::LocalExecution,
                tier: EvidenceTier::SelfReported
            })
        );
        assert!(!crate::stage_manifest::looks_like_credential(
            "registry-official"
        ));
    }

    /// 1.2 / invariant_i_15：无 conformance 只可 Experimental，不进调度候选。
    #[test]
    fn invariant_i_15_unverified_extension_is_experimental_and_out_of_pool() {
        let policy = ExtensionPolicy::default();
        let admission = admit(&package(), &policy, None);
        match &admission {
            Admission::Experimental { why } => {
                assert!(why.contains("不进调度候选"), "理由要写明不进调度候选");
            }
            other => panic!("无报告应 Experimental，实得 {other:?}"),
        }
        assert!(!admission.in_scheduling_pool());
        assert!(!admission.is_supported_label(), "不得标 Supported");

        // Fake Executor 跑通自己的套件，只证明"它那类适配器的闸门用例全绿"，不等于生产能力：
        // 生产能力由 `FakeExecutor` 自己否认，两者必须同时成立，谁也不能拿自测报告当生产证据。
        let fake_report = run_fake_suite(1_000);
        assert!(fake_report.passed(), "Fake 套件本身应全绿");
        let mut fake_package = package();
        fake_package.descriptor.adapter_id = fake_report.adapter_id.clone();
        assert!(
            matches!(
                admit(&fake_package, &policy, Some(&fake_report)),
                Admission::Supported { .. }
            ),
            "套件全绿对同 id 适配器应 Supported"
        );
        assert!(
            !crate::fake_executor::FakeExecutor::is_production_capable(),
            "Fake 不得声明生产能力"
        );
        assert!(
            !crate::fake_executor::FakeExecutor::production_exclusion_reason().is_empty(),
            "Fake 必须给出排除理由"
        );
        // 报告张冠李戴一律拒：不能把 Fake 自测报告贴到真实适配器上。
        assert_eq!(
            admit(&package(), &policy, Some(&fake_report)),
            Admission::Rejected(AdmissionBlock::ReportAdapterMismatch {
                report: fake_report.adapter_id.clone(),
                package: "adapter-local".to_string(),
            }),
            "别的适配器的报告不能当本适配器证据"
        );

        // 闸门类全部通过才 Supported。
        let report = all_pass_report(&package().descriptor.adapter_id);
        let with_report = admit(&package(), &policy, Some(&report));
        assert!(
            matches!(with_report, Admission::Supported { .. }),
            "全部通过应 Supported，实得 {with_report:?}"
        );
        assert!(with_report.in_scheduling_pool());
    }

    /// 1.1：schema fixtures 按版本备齐，没 fixture 的版本不许装。
    #[test]
    fn task_1_1_schema_fixtures_are_versioned_and_required() {
        assert_eq!(
            SCHEMA_FIXTURES.len(),
            3,
            "受支持 schema 版本应有 3 份 fixture"
        );
        for fixture in SCHEMA_FIXTURES {
            let descriptor: serde_json::Value =
                serde_json::from_str(fixture.descriptor).expect("描述符 fixture 必须能解析");
            let config: serde_json::Value =
                serde_json::from_str(fixture.config).expect("配置 fixture 必须能解析");
            assert_eq!(
                descriptor["config_schema_version"].as_u64(),
                Some(u64::from(fixture.schema_version)),
                "fixture 自称的 schema 版本要与登记的一致"
            );
            assert!(
                descriptor["adapter_id"].is_string() && config.is_object(),
                "fixture 要带适配器身份与配置样本"
            );
        }

        let mut unknown = package();
        unknown.descriptor.config_schema_version = 9;
        let wide = ExtensionPolicy {
            max_schema_version: 9,
            ..ExtensionPolicy::default()
        };
        assert_eq!(
            admit(&unknown, &wide, None),
            Admission::Rejected(AdmissionBlock::SchemaFixtureMissing { version: 9 }),
            "没有 fixture 的版本不许装，即便策略上限放得下"
        );
        assert!(fixture_for(1).is_some() && fixture_for(9).is_none());
    }

    /// 1.6：对外支持声明必须有版本、平台与报告，自报级不得当契约证据。
    #[test]
    fn task_1_6_support_claim_requires_version_platform_and_report() {
        let report = all_pass_report("adapter-local");
        let claim = support_claim(&descriptor(), Capability::LocalExecution, Some(&report))
            .expect("版本/平台/报告齐备应可声明");
        assert_eq!(claim.adapter_version, "1.2.0");
        assert_eq!(claim.platform, "linux-x86_64");
        assert_eq!(claim.report_ref, "conformance:adapter-local@1.2.0");
        assert_eq!(claim.tier, EvidenceTier::ContractTested);

        // 声明了契约测试却拿不出报告：不许发声明。
        assert_eq!(
            support_claim(&descriptor(), Capability::LocalExecution, None),
            Err(ClaimReject::MissingReport)
        );

        // 平台写成 any：没有可复现环境。
        let any_platform = descriptor().with_platform("any");
        assert_eq!(
            support_claim(&any_platform, Capability::LocalExecution, Some(&report)),
            Err(ClaimReject::UnknownPlatform)
        );

        // 版本浮动：不知道测的是哪一版。
        let floating = AdapterDescriptor::new("adapter-local", "latest", 2)
            .with_capability(Capability::LocalExecution, EvidenceTier::ContractTested);
        assert_eq!(
            support_claim(&floating, Capability::LocalExecution, Some(&report)),
            Err(ClaimReject::UnpinnedVersion)
        );

        // 描述符没声明的能力，不能被替它宣称。
        assert_eq!(
            support_claim(&descriptor(), Capability::Delegation, Some(&report)),
            Err(ClaimReject::NoSuchCapability)
        );

        // 别人的报告不能当自己的证据。
        let other_report = all_pass_report("someone-else");
        assert_eq!(
            support_claim(
                &descriptor(),
                Capability::LocalExecution,
                Some(&other_report)
            ),
            Err(ClaimReject::ReportAdapterMismatch)
        );

        // 自报级如实标 self_reported，不会被读成契约证据。
        let self_reported = AdapterDescriptor::new("adapter-local", "1.2.0", 2)
            .with_capability(Capability::LocalExecution, EvidenceTier::SelfReported);
        let weak = support_claim(&self_reported, Capability::LocalExecution, None)
            .expect("自报级可声明，但只标自报");
        assert_eq!(weak.report_ref, "self_reported");
        assert_eq!(weak.tier, EvidenceTier::SelfReported);
    }

    /// 1.2：来源、许可证、版本、digest、协议与密钥各自独立拒绝。
    #[test]
    fn task_1_2_install_checks_source_license_and_pins() {
        let policy = ExtensionPolicy::default();
        let report = run_fake_suite(1_000);

        let untrusted =
            ExtensionPackage::new(descriptor(), "https://evil.example.net/ext.zip", DIGEST)
                .with_license("MIT");
        assert_eq!(
            admit(&untrusted, &policy, Some(&report)),
            Admission::Rejected(AdmissionBlock::UntrustedSource)
        );

        let no_license = ExtensionPackage::new(descriptor(), "registry-official", DIGEST);
        assert_eq!(
            admit(&no_license, &policy, Some(&report)),
            Admission::Rejected(AdmissionBlock::LicenseMissing)
        );

        let gpl = ExtensionPackage::new(descriptor(), "registry-official", DIGEST)
            .with_license("GPL-3.0");
        assert_eq!(
            admit(&gpl, &policy, Some(&report)),
            Admission::Rejected(AdmissionBlock::LicenseNotAllowed)
        );

        let floating = ExtensionPackage::new(
            AdapterDescriptor::new("adapter-local", "latest", 2),
            "registry-official",
            DIGEST,
        )
        .with_license("MIT");
        assert_eq!(
            admit(&floating, &policy, Some(&report)),
            Admission::Rejected(AdmissionBlock::UnpinnedVersion)
        );

        let bad_digest = ExtensionPackage::new(descriptor(), "registry-official", "sha256:abc")
            .with_license("MIT");
        assert_eq!(
            admit(&bad_digest, &policy, Some(&report)),
            Admission::Rejected(AdmissionBlock::UnpinnedDigest)
        );

        let mut too_new = package();
        too_new.descriptor.protocol_version = 9;
        assert_eq!(
            admit(&too_new, &policy, Some(&report)),
            Admission::Rejected(AdmissionBlock::ProtocolTooNew {
                adapter: 9,
                host: 2
            })
        );

        let mut leaking = package();
        leaking
            .descriptor
            .limitations
            .push("api_key=sk-live-abcdef123456".to_string());
        assert_eq!(
            admit(&leaking, &policy, Some(&report)),
            Admission::Rejected(AdmissionBlock::EmbeddedSecret)
        );
    }

    /// 1.3 / invariant_i_57：身份失败即禁用，降级徽章不能豁免。
    #[test]
    fn invariant_i_57_identity_failure_disables_dispatch_without_badge_exemption() {
        let probe = ProbeResult {
            capability: Capability::Delegation,
            state: CapabilityState::Degraded {
                reason: "慢但可用".to_string(),
            },
            identity_ok: false,
            authorization_ok: true,
            message_integrity_ok: true,
        };
        assert_eq!(effective_state(&probe), CapabilityState::Unsupported);
        let view = view_for(&probe);
        assert!(!view.dispatch_enabled, "身份失败不得派发");
        assert_eq!(view.badge, "unavailable");
        assert!(!dispatch_allowed(&probe));

        let integrity_bad = ProbeResult {
            message_integrity_ok: false,
            identity_ok: true,
            ..probe
        };
        assert!(!dispatch_allowed(&integrity_bad));
    }

    /// 1.3：探测结论是调度与界面的唯一来源，两者必须一致。
    #[test]
    fn task_1_3_probe_and_view_agree() {
        let healthy = ProbeResult {
            capability: Capability::LocalExecution,
            state: CapabilityState::Supported,
            identity_ok: true,
            authorization_ok: true,
            message_integrity_ok: true,
        };
        let view = view_for(&healthy);
        assert_eq!(view.badge, "available");
        assert_eq!(view.dispatch_enabled, dispatch_allowed(&healthy));

        let unsupported = ProbeResult {
            state: CapabilityState::Unsupported,
            ..healthy
        };
        assert_eq!(view_for(&unsupported).badge, "unavailable");
        assert_eq!(
            view_for(&unsupported).dispatch_enabled,
            dispatch_allowed(&unsupported)
        );
        assert!(!dispatch_allowed(&unsupported));
    }

    /// 1.4：工具清单变化使旧批准失效并触发重新评估。
    #[test]
    fn task_1_4_tool_manifest_change_invalidates_approval() {
        let package = package();
        let approval = ExtensionApproval {
            adapter_id: package.descriptor.adapter_id.clone(),
            executable_digest: package.executable_digest.clone(),
            tool_manifest_digest: tool_manifest_digest(&package.tool_manifest),
            schema_version: package.descriptor.config_schema_version,
            approved_at: "2026-11-21T09:00:00Z".to_string(),
        };
        assert_eq!(approval_valid(&approval, &package), Reapproval::Valid);

        // 工具顺序不变即不变。
        let reordered = ExtensionPackage {
            tool_manifest: vec!["git".to_string(), "rustc".to_string()],
            ..package.clone()
        };
        assert_eq!(approval_valid(&approval, &reordered), Reapproval::Valid);

        let widened = ExtensionPackage {
            tool_manifest: vec!["git".to_string(), "rustc".to_string(), "docker".to_string()],
            ..package.clone()
        };
        assert_eq!(
            approval_valid(&approval, &widened),
            Reapproval::Required(ReapprovalReason::ToolManifestChanged)
        );

        let upgraded = ExtensionPackage {
            executable_digest: format!("sha256:{:064x}", 2),
            ..package.clone()
        };
        assert_eq!(
            approval_valid(&approval, &upgraded),
            Reapproval::Required(ReapprovalReason::ExecutableChanged)
        );

        let new_schema = ExtensionPackage {
            descriptor: AdapterDescriptor {
                config_schema_version: 2,
                ..package.descriptor.clone()
            },
            ..package
        };
        assert_eq!(
            approval_valid(&approval, &new_schema),
            Reapproval::Required(ReapprovalReason::SchemaChanged)
        );
    }

    /// 1.5 / SB06：扩大发送范围必须重新判断，未获准时零投递。
    #[test]
    fn task_1_5_widening_scope_requires_new_decision() {
        let decision =
            OutboundDecision::new("project-1", OutboundScope::Summary, "plugin-b", "account-1");
        let same = OutboundRequest {
            project_id: "project-1".to_string(),
            scope: OutboundScope::Summary,
            recipient: "plugin-b".to_string(),
            account_id: "account-1".to_string(),
        };
        assert_eq!(guard_send(Some(&decision), &same), SendGuard::Allowed);

        let widened = OutboundRequest {
            scope: OutboundScope::FullSource,
            ..same.clone()
        };
        assert!(
            OutboundScope::FullSource.is_wider_than(OutboundScope::Summary),
            "完整源文件比摘要更宽"
        );
        match guard_send(Some(&decision), &widened) {
            SendGuard::NeedsDecision { changed } => {
                assert_eq!(changed, vec!["scope"], "变化项要点名");
            }
            other => panic!("扩大范围必须重新判断，实得 {other:?}"),
        }

        let other_account = OutboundRequest {
            account_id: "account-2".to_string(),
            ..same.clone()
        };
        assert!(matches!(
            guard_send(Some(&decision), &other_account),
            SendGuard::NeedsDecision { .. }
        ));

        assert_eq!(
            guard_send(None, &same),
            SendGuard::ZeroDelivery {
                reason: "首次外发必须有明确决定"
            }
        );
    }

    /// 1.5：审计只留引用与摘要，不含原文与凭据。
    #[test]
    fn task_1_5_audit_holds_refs_not_content() {
        let decision =
            OutboundDecision::new("project-1", OutboundScope::Logs, "plugin-b", "account-1");
        let audit = audit_for(&decision);
        assert_eq!(audit.scope, "logs");
        assert!(audit.decision_ref.starts_with("sha256:outbound:"));
        assert!(audit.recipient_ref.starts_with("recipient:"));
        let rendered = format!("{audit:?}");
        assert!(
            !rendered.contains("secret"),
            "审计不得出现凭据字样：{rendered}"
        );
        assert_eq!(audit.decided_at, "2026-11-21T09:00:00Z");
    }
}
