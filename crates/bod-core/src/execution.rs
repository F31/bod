//! bod-execution/v1 宿主—执行端最小契约（P0 D4 任务 5.1—5.3）。
//!
//! 纯值类型与纯规则：零外部依赖、时间注入，不做 IO，不调用模型。
//! 两条硬边界在类型层面固定下来：
//! 1. 执行端 `Completed` 只表示"候选结果已提交"，**永不**表示工作包 `Accepted`；
//! 2. 进程退出码 0 只是进程成功信号，`map_process_exit` 的接受信号恒为 `PendingVerification`。
//!
//! 权威来源：V2.9.6 §15.2—§15.7、§14.2、§33 执行组。

use std::collections::BTreeSet;
use std::fmt;

/// 协议标识。正式 v1 承诺在实现与 conformance 通过后发布（§15.1）。
pub const PROTOCOL_ID: &str = "bod-execution/v1";

/// 观察缓冲区上限：超过即要求快照/对账，不无限缓存。
pub const MAX_BUFFERED_OBSERVATIONS: usize = 4096;

// ---------------------------------------------------------------------------
// 操作与传输
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Operation {
    Describe,
    Probe,
    Start,
    Inspect,
    Observe,
    Cancel,
    Resume,
    Reconcile,
    QuotaWindows,
    Acknowledge,
    Replay,
    InputResponse,
    PermissionResponse,
    ArtifactTransfer,
    SourceReplay,
}

impl Operation {
    /// 必需的基础契约操作（observe 另由 `ObserveTransport` 约束）。
    pub const REQUIRED: [Operation; 4] = [
        Operation::Describe,
        Operation::Probe,
        Operation::Start,
        Operation::Inspect,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Describe => "describe",
            Self::Probe => "probe",
            Self::Start => "start",
            Self::Inspect => "inspect",
            Self::Observe => "observe",
            Self::Cancel => "cancel",
            Self::Resume => "resume",
            Self::Reconcile => "reconcile",
            Self::QuotaWindows => "quota_windows",
            Self::Acknowledge => "acknowledge",
            Self::Replay => "replay",
            Self::InputResponse => "input_response",
            Self::PermissionResponse => "permission_response",
            Self::ArtifactTransfer => "artifact_transfer",
            Self::SourceReplay => "source_replay",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "describe" => Self::Describe,
            "probe" => Self::Probe,
            "start" => Self::Start,
            "inspect" => Self::Inspect,
            "observe" => Self::Observe,
            "cancel" => Self::Cancel,
            "resume" => Self::Resume,
            "reconcile" => Self::Reconcile,
            "quota_windows" => Self::QuotaWindows,
            "acknowledge" => Self::Acknowledge,
            "replay" => Self::Replay,
            "input_response" => Self::InputResponse,
            "permission_response" => Self::PermissionResponse,
            "artifact_transfer" => Self::ArtifactTransfer,
            "source_replay" => Self::SourceReplay,
            _ => return None,
        })
    }

    pub const fn is_required(self) -> bool {
        matches!(
            self,
            Self::Describe | Self::Probe | Self::Start | Self::Inspect
        )
    }
}

/// observe 的传输形态：流、轮询或两者都有；至少一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveTransport {
    Stream,
    Poll,
    Both,
    None,
}

impl ObserveTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Poll => "poll",
            Self::Both => "stream+poll",
            Self::None => "none",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "stream" => Self::Stream,
            "poll" => Self::Poll,
            "stream+poll" | "both" => Self::Both,
            "none" => Self::None,
            _ => return None,
        })
    }
}

/// 副作用类别，用于界面提示与授权需求，不替代运行时校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClass {
    ReadOnly,
    WorkspaceWrite,
    ProcessExecution,
    ExternalSideEffect,
}

impl EffectClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::ProcessExecution => "process_execution",
            Self::ExternalSideEffect => "external_side_effect",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "read_only" => Self::ReadOnly,
            "workspace_write" => Self::WorkspaceWrite,
            "process_execution" => Self::ProcessExecution,
            "external_side_effect" => Self::ExternalSideEffect,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// 能力分项声明与证据等级
// ---------------------------------------------------------------------------

/// 能力槽位。未知能力保留原名（`Other`），不静默丢弃，也绝不允许标 Supported。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapabilitySlot {
    ModelControl,
    ToolControl,
    WorkspaceIsolation,
    Metering,
    Cancellation,
    Recovery,
    ExternalIdempotency,
    ReliableObservation,
    QuotaWindows,
    InputResponse,
    PermissionResponse,
    ArtifactTransfer,
    SourceReplay,
    Other(String),
}

impl CapabilitySlot {
    pub const KNOWN: [&'static str; 13] = [
        "model_control",
        "tool_control",
        "workspace_isolation",
        "metering",
        "cancellation",
        "recovery",
        "external_idempotency",
        "reliable_observation",
        "quota_windows",
        "input_response",
        "permission_response",
        "artifact_transfer",
        "source_replay",
    ];

    pub fn as_str(&self) -> &str {
        match self {
            Self::ModelControl => "model_control",
            Self::ToolControl => "tool_control",
            Self::WorkspaceIsolation => "workspace_isolation",
            Self::Metering => "metering",
            Self::Cancellation => "cancellation",
            Self::Recovery => "recovery",
            Self::ExternalIdempotency => "external_idempotency",
            Self::ReliableObservation => "reliable_observation",
            Self::QuotaWindows => "quota_windows",
            Self::InputResponse => "input_response",
            Self::PermissionResponse => "permission_response",
            Self::ArtifactTransfer => "artifact_transfer",
            Self::SourceReplay => "source_replay",
            Self::Other(name) => name,
        }
    }

    /// 未知名称不报错：按契约要求保留并降级为 unknown（§14.2「未知能力保留原值并显示不支持」）。
    pub fn parse(value: &str) -> Self {
        match value {
            "model_control" => Self::ModelControl,
            "tool_control" => Self::ToolControl,
            "workspace_isolation" => Self::WorkspaceIsolation,
            "metering" => Self::Metering,
            "cancellation" => Self::Cancellation,
            "recovery" => Self::Recovery,
            "external_idempotency" => Self::ExternalIdempotency,
            "reliable_observation" => Self::ReliableObservation,
            "quota_windows" => Self::QuotaWindows,
            "input_response" => Self::InputResponse,
            "permission_response" => Self::PermissionResponse,
            "artifact_transfer" => Self::ArtifactTransfer,
            "source_replay" => Self::SourceReplay,
            other => Self::Other(other.to_string()),
        }
    }

    pub const fn is_known(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SupportLevel {
    Supported,
    Unsupported,
    Unknown,
}

impl SupportLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }

    /// 无法识别的等级一律降级为 Unknown，不当作 Supported。
    pub fn parse(value: &str) -> Self {
        match value {
            "supported" => Self::Supported,
            "unsupported" => Self::Unsupported,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    AdapterSelfDeclared,
    PlatformProbe,
    TestResult,
    ManualOverride,
}

impl EvidenceSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AdapterSelfDeclared => "self_declared",
            Self::PlatformProbe => "platform_probe",
            Self::TestResult => "test_result",
            Self::ManualOverride => "manual_override",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "self_declared" => Self::AdapterSelfDeclared,
            "platform_probe" => Self::PlatformProbe,
            "test_result" => Self::TestResult,
            "manual_override" => Self::ManualOverride,
            _ => return None,
        })
    }

    /// 该来源最多能支撑的证据等级：不能靠自报拿到 probe 级结论。
    pub const fn ceiling(self) -> EvidenceLevel {
        match self {
            Self::AdapterSelfDeclared => EvidenceLevel::SelfDeclared,
            Self::PlatformProbe => EvidenceLevel::ProbeVerified,
            Self::TestResult => EvidenceLevel::RuntimeObserved,
            Self::ManualOverride => EvidenceLevel::IndependentlyVerified,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceLevel {
    SelfDeclared,
    ConfigurationChecked,
    ProbeVerified,
    RuntimeObserved,
    IndependentlyVerified,
}

impl EvidenceLevel {
    pub const fn rank(self) -> u8 {
        match self {
            Self::SelfDeclared => 0,
            Self::ConfigurationChecked => 1,
            Self::ProbeVerified => 2,
            Self::RuntimeObserved => 3,
            Self::IndependentlyVerified => 4,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfDeclared => "self_declared",
            Self::ConfigurationChecked => "configuration_checked",
            Self::ProbeVerified => "probe_verified",
            Self::RuntimeObserved => "runtime_observed",
            Self::IndependentlyVerified => "independently_verified",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "self_declared" => Self::SelfDeclared,
            "configuration_checked" => Self::ConfigurationChecked,
            "probe_verified" => Self::ProbeVerified,
            "runtime_observed" => Self::RuntimeObserved,
            "independently_verified" => Self::IndependentlyVerified,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityClaim {
    pub slot: CapabilitySlot,
    pub level: SupportLevel,
    /// 定性形态（如 model_control 的 mediated/observed/opaque）。P0 只要求可读、非空，
    /// 具体取值随适配器与已测版本变化，不做平台无关的统一枚举。
    pub mode: Option<String>,
    pub source: EvidenceSource,
    pub evidence_level: EvidenceLevel,
    pub observed_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub reason: Option<String>,
}

impl CapabilityClaim {
    pub fn is_stale(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|expires| expires <= now_ms)
    }

    /// 校验单条声明：Supported 必须有 probe 及以上证据，且等级不超过来源上限。
    pub fn verify(&self) -> Result<(), ContractError> {
        let name = self.slot.as_str().to_string();
        if self
            .mode
            .as_deref()
            .is_some_and(|mode| mode.trim().is_empty())
        {
            return Err(ContractError::EmptyCapabilityMode(name));
        }
        if !self.slot.is_known() && self.level == SupportLevel::Supported {
            return Err(ContractError::UnknownCapabilityClaimedSupported(name));
        }
        if self.evidence_level.rank() > self.source.ceiling().rank() {
            return Err(ContractError::EvidenceExceedsSource {
                capability: name,
                source: self.source,
                claimed: self.evidence_level,
            });
        }
        if self.level == SupportLevel::Supported
            && self.evidence_level.rank() < EvidenceLevel::ProbeVerified.rank()
        {
            return Err(ContractError::SupportWithoutProbeEvidence(name));
        }
        if self.level != SupportLevel::Supported && self.reason.is_none() {
            return Err(ContractError::UnsupportedWithoutReason(name));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 分项探测
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProbeAspect {
    Environment,
    Credentials,
    Model,
    Quota,
    Workspace,
    Network,
}

impl ProbeAspect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Credentials => "credentials",
            Self::Model => "model",
            Self::Quota => "quota",
            Self::Workspace => "workspace",
            Self::Network => "network",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "environment" => Self::Environment,
            "credentials" => Self::Credentials,
            "model" => Self::Model,
            "quota" => Self::Quota,
            "workspace" => Self::Workspace,
            "network" => Self::Network,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProbeStatus {
    Ready,
    NotReady,
    Unsupported,
    Unknown,
}

impl ProbeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotReady => "not_ready",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "ready" => Self::Ready,
            "not_ready" => Self::NotReady,
            "unsupported" => Self::Unsupported,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeItem {
    pub aspect: ProbeAspect,
    pub status: ProbeStatus,
    pub reason: String,
    pub source: EvidenceSource,
    /// 探测范围：账户、目录、二进制路径等，避免跨 scope 复用结论。
    pub scope: String,
    pub observed_at_ms: i64,
    pub expires_at_ms: Option<i64>,
}

impl ProbeItem {
    pub fn is_stale(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|expires| expires <= now_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub adapter_id: String,
    pub adapter_version: String,
    pub items: Vec<ProbeItem>,
    pub generated_at_ms: i64,
}

impl ProbeReport {
    /// 整体结论绝不笼统报成功：任一项未就绪即 not_ready，任一项未知即 unknown；
    /// 不适用（unsupported）不阻塞整体，但也不会被读成"该项已验证"。
    pub fn overall(&self) -> ProbeStatus {
        if self.items.is_empty() {
            return ProbeStatus::Unknown;
        }
        if self
            .items
            .iter()
            .any(|item| item.status == ProbeStatus::NotReady)
        {
            return ProbeStatus::NotReady;
        }
        if self
            .items
            .iter()
            .any(|item| item.status == ProbeStatus::Unknown)
        {
            return ProbeStatus::Unknown;
        }
        if self
            .items
            .iter()
            .all(|item| item.status == ProbeStatus::Unsupported)
        {
            return ProbeStatus::Unsupported;
        }
        ProbeStatus::Ready
    }

    /// 阻塞项：必须显式暴露给用户，不能藏在整体结论后面。
    pub fn blocking_items(&self) -> Vec<&ProbeItem> {
        self.items
            .iter()
            .filter(|item| matches!(item.status, ProbeStatus::NotReady | ProbeStatus::Unknown))
            .collect()
    }

    pub fn item(&self, aspect: ProbeAspect) -> Option<&ProbeItem> {
        self.items.iter().find(|item| item.aspect == aspect)
    }
}

// ---------------------------------------------------------------------------
// 描述符
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterDescriptor {
    pub protocol: String,
    pub adapter_id: String,
    pub adapter_version: String,
    pub capability_id: String,
    pub platform: String,
    pub transport: String,
    pub effect_class: EffectClass,
    pub operations: Vec<Operation>,
    pub observe: ObserveTransport,
    pub capabilities: Vec<CapabilityClaim>,
    /// 持久去重保留期；None 表示未声明，宿主不得据此安全重发（§15.4）。
    pub dedup_retention_ms: Option<i64>,
    pub limitations: Vec<String>,
}

impl AdapterDescriptor {
    pub fn supports(&self, operation: Operation) -> bool {
        self.operations.contains(&operation)
    }

    pub fn capability(&self, slot: &CapabilitySlot) -> Option<&CapabilityClaim> {
        self.capabilities.iter().find(|claim| &claim.slot == slot)
    }

    pub fn level_of(&self, slot: CapabilitySlot) -> SupportLevel {
        self.capability(&slot)
            .map_or(SupportLevel::Unknown, |claim| claim.level)
    }

    /// 描述符自洽性校验：缺必需操作、能力重复、证据越级都必须拒绝。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.protocol != PROTOCOL_ID {
            return Err(ContractError::ProtocolMismatch {
                expected: PROTOCOL_ID,
                actual: self.protocol.clone(),
            });
        }
        if self.adapter_id.trim().is_empty() {
            return Err(ContractError::EmptyAdapterId);
        }
        for operation in Operation::REQUIRED {
            if !self.supports(operation) {
                return Err(ContractError::MissingRequiredOperation(operation));
            }
        }
        let declares_observe = self.supports(Operation::Observe);
        if declares_observe && self.observe == ObserveTransport::None {
            return Err(ContractError::MissingObserveTransport);
        }
        if !declares_observe && self.observe != ObserveTransport::None {
            return Err(ContractError::ObserveTransportWithoutOperation);
        }
        if self.dedup_retention_ms.is_some_and(|ms| ms <= 0) {
            return Err(ContractError::InvalidDedupRetention);
        }

        let mut seen: Vec<&str> = Vec::with_capacity(self.capabilities.len());
        for claim in &self.capabilities {
            let name = claim.slot.as_str();
            if seen.contains(&name) {
                return Err(ContractError::DuplicateCapability(name.to_string()));
            }
            seen.push(name);
            claim.verify()?;
        }
        Ok(())
    }

    /// 支持矩阵条目：只输出已声明能力，未知能力不补默认值。
    pub fn support_matrix(&self) -> Vec<MatrixRow> {
        self.capabilities
            .iter()
            .map(|claim| MatrixRow {
                adapter_id: self.adapter_id.clone(),
                adapter_version: self.adapter_version.clone(),
                capability: claim.slot.as_str().to_string(),
                level: claim.level,
                mode: claim.mode.clone(),
                evidence_level: claim.evidence_level,
                source: claim.source,
                observed_at_ms: claim.observed_at_ms,
                limitations: claim.reason.clone().unwrap_or_default(),
            })
            .collect()
    }
}

/// 支持矩阵一行：能力、等级、证据等级、来源与观测时间必须同时在。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixRow {
    pub adapter_id: String,
    pub adapter_version: String,
    pub capability: String,
    pub level: SupportLevel,
    /// 定性形态；与 level 一起展示，避免把"支持该能力"读成"完全等价"。
    pub mode: Option<String>,
    pub evidence_level: EvidenceLevel,
    pub source: EvidenceSource,
    pub observed_at_ms: i64,
    pub limitations: String,
}

// ---------------------------------------------------------------------------
// 契约错误（稳定原因码）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    ProtocolMismatch {
        expected: &'static str,
        actual: String,
    },
    EmptyAdapterId,
    MissingRequiredOperation(Operation),
    MissingObserveTransport,
    ObserveTransportWithoutOperation,
    InvalidDedupRetention,
    DuplicateCapability(String),
    UnknownCapabilityClaimedSupported(String),
    EmptyCapabilityMode(String),
    SupportWithoutProbeEvidence(String),
    EvidenceExceedsSource {
        capability: String,
        source: EvidenceSource,
        claimed: EvidenceLevel,
    },
    UnsupportedWithoutReason(String),
    InvalidStateTransition {
        from: ExecutionState,
        to: ExecutionState,
    },
    InvalidSequence,
}

impl ContractError {
    /// 稳定原因码：控制 API 与报告据此判定，不解析人读文案。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ProtocolMismatch { .. } => "E_PROTOCOL_MISMATCH",
            Self::EmptyAdapterId => "E_ADAPTER_ID_EMPTY",
            Self::MissingRequiredOperation(_) => "E_OP_REQUIRED_MISSING",
            Self::MissingObserveTransport => "E_OBSERVE_TRANSPORT_MISSING",
            Self::ObserveTransportWithoutOperation => "E_OBSERVE_OP_MISSING",
            Self::InvalidDedupRetention => "E_DEDUP_RETENTION_INVALID",
            Self::DuplicateCapability(_) => "E_CAPABILITY_DUPLICATE",
            Self::UnknownCapabilityClaimedSupported(_) => "E_UNKNOWN_CAPABILITY_SUPPORTED",
            Self::EmptyCapabilityMode(_) => "E_CAPABILITY_MODE_EMPTY",
            Self::SupportWithoutProbeEvidence(_) => "E_SUPPORT_WITHOUT_EVIDENCE",
            Self::EvidenceExceedsSource { .. } => "E_EVIDENCE_EXCEEDS_SOURCE",
            Self::UnsupportedWithoutReason(_) => "E_UNSUPPORTED_WITHOUT_REASON",
            Self::InvalidStateTransition { .. } => "E_STATE_TRANSITION_INVALID",
            Self::InvalidSequence => "E_SEQUENCE_INVALID",
        }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolMismatch { expected, actual } => {
                write!(f, "协议不匹配：期望 {expected}，实际 {actual}")
            }
            Self::EmptyAdapterId => write!(f, "adapter_id 不能为空"),
            Self::MissingRequiredOperation(op) => {
                write!(f, "缺少必需操作：{}", op.as_str())
            }
            Self::MissingObserveTransport => write!(f, "声明了 observe 但未给出传输方式"),
            Self::ObserveTransportWithoutOperation => {
                write!(f, "给出观察传输方式但未声明 observe 操作")
            }
            Self::InvalidDedupRetention => write!(f, "持久去重保留期必须为正"),
            Self::DuplicateCapability(name) => write!(f, "能力重复声明：{name}"),
            Self::UnknownCapabilityClaimedSupported(name) => {
                write!(f, "未知能力不得声明为 supported：{name}")
            }
            Self::EmptyCapabilityMode(name) => write!(f, "能力形态为空串：{name}"),
            Self::SupportWithoutProbeEvidence(name) => {
                write!(f, "能力缺少 probe 级证据却声明 supported：{name}")
            }
            Self::EvidenceExceedsSource {
                capability,
                source,
                claimed,
            } => write!(
                f,
                "证据等级 {} 超过来源 {} 能支撑的上限：{capability}",
                claimed.as_str(),
                source.as_str()
            ),
            Self::UnsupportedWithoutReason(name) => {
                write!(f, "非 supported 能力必须给出原因：{name}")
            }
            Self::InvalidStateTransition { from, to } => {
                write!(f, "非法执行状态迁移：{} → {}", from.as_str(), to.as_str())
            }
            Self::InvalidSequence => write!(f, "观察序号非法（0 或非连续起点）"),
        }
    }
}

impl std::error::Error for ContractError {}

// ---------------------------------------------------------------------------
// Start 请求与参数摘要
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    pub invocation_id: String,
    pub run_id: String,
    pub work_package_revision: String,
    pub attempt_id: String,
    pub binding_id: String,
    pub fencing_token: u64,
    pub agent_ref: String,
    pub runtime_config_revision: String,
    pub context_bundle_ref: String,
    pub workspace_ref: String,
    pub grant_ref: String,
    pub reservation_refs: Vec<String>,
    pub deadline_at_ms: Option<i64>,
}

impl StartRequest {
    /// 规范化参数文本：同一次派发的重发必须逐字一致，用于"同 ID 异参"判定。
    /// invocation_id 是键本身，不参与摘要。
    pub fn canonical_params(&self) -> String {
        let mut refs = self.reservation_refs.clone();
        refs.sort();
        refs.dedup();
        let deadline = self
            .deadline_at_ms
            .map_or_else(|| "-".to_string(), |ms| ms.to_string());
        format!(
            "protocol={PROTOCOL_ID}\nrun_id={}\nwork_package_revision={}\nattempt_id={}\n\
             binding_id={}\nfencing_token={}\nagent_ref={}\nruntime_config_revision={}\n\
             context_bundle_ref={}\nworkspace_ref={}\ngrant_ref={}\nreservation_refs={}\n\
             deadline_at_ms={}",
            self.run_id,
            self.work_package_revision,
            self.attempt_id,
            self.binding_id,
            self.fencing_token,
            self.agent_ref,
            self.runtime_config_revision,
            self.context_bundle_ref,
            self.workspace_ref,
            self.grant_ref,
            refs.join(","),
            deadline
        )
    }

    /// 短指纹，仅用于日志/报告展示；去重比较用 `canonical_params` 精确比对。
    pub fn params_digest(&self) -> String {
        let hash = fnv1a64(self.canonical_params().as_bytes());
        format!("fnv1a64:{hash:016x}")
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// 执行状态与迁移
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecutionState {
    Accepted,
    Starting,
    Running,
    NeedsInput,
    NeedsAuth,
    Suspended,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl ExecutionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "Accepted",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::NeedsInput => "NeedsInput",
            Self::NeedsAuth => "NeedsAuth",
            Self::Suspended => "Suspended",
            Self::Cancelling => "Cancelling",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Unknown => "Unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Accepted" => Self::Accepted,
            "Starting" => Self::Starting,
            "Running" => Self::Running,
            "NeedsInput" => Self::NeedsInput,
            "NeedsAuth" => Self::NeedsAuth,
            "Suspended" => Self::Suspended,
            "Cancelling" => Self::Cancelling,
            "Completed" => Self::Completed,
            "Failed" => Self::Failed,
            "Cancelled" => Self::Cancelled,
            "Unknown" => Self::Unknown,
            _ => return None,
        })
    }

    /// 终态：迟到 detail 日志不得自动推翻，只能经对账改写为 Unknown。
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub const fn allowed_next(self) -> &'static [ExecutionState] {
        use ExecutionState::*;
        match self {
            Accepted => &[
                Starting, Running, Cancelling, Completed, Failed, Cancelled, Unknown,
            ],
            Starting => &[Running, Completed, Failed, Cancelled, Unknown],
            Running => &[
                NeedsInput, NeedsAuth, Suspended, Cancelling, Completed, Failed, Cancelled, Unknown,
            ],
            NeedsInput | NeedsAuth => &[Running, Cancelling, Failed, Cancelled, Unknown],
            Suspended => &[Running, Cancelling, Failed, Cancelled, Unknown],
            Cancelling => &[Cancelled, Failed, Unknown],
            Completed | Failed | Cancelled => &[],
            Unknown => &[
                Accepted, Starting, Running, NeedsInput, NeedsAuth, Suspended, Cancelling,
                Completed, Failed, Cancelled,
            ],
        }
    }

    /// 状态推进。终态之间的改写必须显式走对账（`Reconciled`），否则拒绝。
    pub fn advance(
        self,
        next: ExecutionState,
        authority: TransitionAuthority,
    ) -> Result<ExecutionState, ContractError> {
        if self == next {
            return Ok(next);
        }
        if self.is_terminal() {
            return if authority == TransitionAuthority::Reconciled
                && next == ExecutionState::Unknown
            {
                Ok(next)
            } else {
                Err(ContractError::InvalidStateTransition {
                    from: self,
                    to: next,
                })
            };
        }
        if self.allowed_next().contains(&next) {
            return Ok(next);
        }
        Err(ContractError::InvalidStateTransition {
            from: self,
            to: next,
        })
    }
}

/// 谁在推进状态：观察只能推进未终结状态；对账可以纠正终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionAuthority {
    Observed,
    Reconciled,
}

/// 验收信号。`map_process_exit` 只会产生 `PendingVerification`：
/// 退出码 0 是进程成功信号，不是验收通过（§15.4、§15.9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceSignal {
    PendingVerification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    pub state: ExecutionState,
    pub acceptance: AcceptanceSignal,
}

/// 进程退出只决定执行状态；验收恒为待验证。
pub const fn map_process_exit(code: i32) -> ProcessExit {
    ProcessExit {
        state: if code == 0 {
            ExecutionState::Completed
        } else {
            ExecutionState::Failed
        },
        acceptance: AcceptanceSignal::PendingVerification,
    }
}

/// 回合结束原因映射。未知原因进入 Unknown，不推断成功（§15.9）。
pub fn map_stop_reason(reason: &str) -> ExecutionState {
    match reason {
        "end_turn" | "completed" => ExecutionState::Completed,
        "cancelled" | "canceled" | "aborted" => ExecutionState::Cancelled,
        "error" | "failed" => ExecutionState::Failed,
        "awaiting_input" | "input_required" => ExecutionState::NeedsInput,
        "awaiting_auth" | "auth_required" => ExecutionState::NeedsAuth,
        _ => ExecutionState::Unknown,
    }
}

/// 取消语义：请求被受理（cancel_ack）不等于已停止（stop_confirmed）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelStage {
    Requested,
    Confirmed,
}

impl CancelStage {
    /// 只有 `Confirmed` 才算停止确认；未确认必须保留风险与现场。
    pub const fn stop_confirmed(self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// 回包丢失后的重发策略：只有持久去重 + 查询 + 保留期三者齐备才可安全重发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartReuse {
    /// 可安全重发（同 invocation_id 返回原 execution_id）。
    SafeResend,
    /// 只能先查询对账；查不到也不得新建执行，落 Unknown 等人工裁决。
    ReconcileThenUnknown,
    /// 仅记录尝试并进入 Unknown。
    RecordAttemptThenUnknown,
}

impl StartReuse {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SafeResend => "safe_resend",
            Self::ReconcileThenUnknown => "reconcile_then_unknown",
            Self::RecordAttemptThenUnknown => "record_attempt_then_unknown",
        }
    }
}

pub fn start_reuse_policy(descriptor: &AdapterDescriptor) -> StartReuse {
    let idempotent =
        descriptor.level_of(CapabilitySlot::ExternalIdempotency) == SupportLevel::Supported;
    let can_inspect = descriptor.supports(Operation::Inspect);
    let retains = descriptor.dedup_retention_ms.is_some_and(|ms| ms > 0);
    if idempotent && can_inspect && retains {
        StartReuse::SafeResend
    } else if can_inspect {
        StartReuse::ReconcileThenUnknown
    } else {
        StartReuse::RecordAttemptThenUnknown
    }
}

// ---------------------------------------------------------------------------
// invocation 接收记录
// ---------------------------------------------------------------------------

/// 宿主侧 invocation 记录：先落盘再对外确认，重启后据此去重与对账（§15.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationRecord {
    pub invocation_id: String,
    pub company_id: String,
    pub run_id: String,
    pub work_package_revision: String,
    pub attempt_id: String,
    pub binding_id: String,
    pub fencing_token: u64,
    pub adapter_id: String,
    pub adapter_version: String,
    pub params_canonical: String,
    pub params_digest: String,
    pub execution_id: Option<String>,
    pub state: ExecutionState,
    pub created_at_ms: i64,
    pub accepted_at_ms: Option<i64>,
    pub last_observed_at_ms: Option<i64>,
    pub retention_until_ms: Option<i64>,
}

impl InvocationRecord {
    /// 从 Start 请求生成"已持久、尚未确认"的记录：state 用 Starting 表示宿主已发起。
    pub fn pending(
        request: &StartRequest,
        company_id: &str,
        descriptor: &AdapterDescriptor,
        now_ms: i64,
    ) -> Self {
        Self {
            invocation_id: request.invocation_id.clone(),
            company_id: company_id.to_string(),
            run_id: request.run_id.clone(),
            work_package_revision: request.work_package_revision.clone(),
            attempt_id: request.attempt_id.clone(),
            binding_id: request.binding_id.clone(),
            fencing_token: request.fencing_token,
            adapter_id: descriptor.adapter_id.clone(),
            adapter_version: descriptor.adapter_version.clone(),
            params_canonical: request.canonical_params(),
            params_digest: request.params_digest(),
            execution_id: None,
            state: ExecutionState::Starting,
            created_at_ms: now_ms,
            accepted_at_ms: None,
            last_observed_at_ms: None,
            retention_until_ms: descriptor
                .dedup_retention_ms
                .map(|retention| now_ms.saturating_add(retention)),
        }
    }

    /// 同 ID 是否同参：比较规范化文本，不做模糊匹配。
    pub fn same_params(&self, request: &StartRequest) -> bool {
        self.params_canonical == request.canonical_params()
    }

    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// 去重保留是否仍然有效：过期后不得据此安全重发。
    pub fn retention_covers(&self, now_ms: i64) -> bool {
        self.retention_until_ms.is_none_or(|until| until > now_ms)
    }
}

// ---------------------------------------------------------------------------
// 观察信封、游标与缺口
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationType {
    Progress,
    Question,
    PermissionRequest,
    Artifact,
    Usage,
    Log,
    StateChange,
    FinalResult,
    Error,
}

impl ObservationType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Question => "question",
            Self::PermissionRequest => "permission_request",
            Self::Artifact => "artifact",
            Self::Usage => "usage",
            Self::Log => "log",
            Self::StateChange => "state_change",
            Self::FinalResult => "final_result",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "progress" => Self::Progress,
            "question" => Self::Question,
            "permission_request" => Self::PermissionRequest,
            "artifact" => Self::Artifact,
            "usage" => Self::Usage,
            "log" => Self::Log,
            "state_change" => Self::StateChange,
            "final_result" => Self::FinalResult,
            "error" => Self::Error,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationEnvelope {
    pub invocation_id: String,
    pub source_epoch: u64,
    pub stream_id: String,
    pub source_sequence: u64,
    pub observed_at_ms: i64,
    pub kind: ObservationType,
    pub payload: String,
    pub origin: String,
}

impl ObservationEnvelope {
    /// ACK/重放绑定 invocation + source_epoch + stream_id（§15.5）。
    pub fn stream_key(&self) -> (&str, u64, &str) {
        (
            &self.invocation_id,
            self.source_epoch,
            self.stream_id.as_str(),
        )
    }
}

/// 接收游标：只确认最高连续 sequence，缺口不越过。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservationCursor {
    highest_contiguous: Option<u64>,
    retained_floor: Option<u64>,
    buffered: BTreeSet<u64>,
}

impl ObservationCursor {
    pub fn new(highest_contiguous: Option<u64>, retained_floor: Option<u64>) -> Self {
        Self {
            highest_contiguous,
            retained_floor,
            buffered: BTreeSet::new(),
        }
    }

    pub fn ack_upto(&self) -> Option<u64> {
        self.highest_contiguous
    }

    /// 从持久状态重建游标（重启继续原流时保持序列与缺口）。
    pub fn restore(
        highest_contiguous: Option<u64>,
        retained_floor: Option<u64>,
        buffered: &[u64],
    ) -> Self {
        Self {
            highest_contiguous,
            retained_floor,
            buffered: buffered
                .iter()
                .copied()
                .filter(|sequence| {
                    *sequence > 0 && highest_contiguous.is_none_or(|highest| *sequence > highest)
                })
                .collect(),
        }
    }

    pub fn buffered_sequences(&self) -> Vec<u64> {
        self.buffered.iter().copied().collect()
    }

    pub fn buffered_len(&self) -> usize {
        self.buffered.len()
    }

    pub fn retained_floor(&self) -> Option<u64> {
        self.retained_floor
    }

    /// 接收一个序号并推进游标。重复不重复处理；乱序缓存但不确认；
    /// 低于保留下界要求快照，不静默跳到最新。
    pub fn receive(&mut self, sequence: u64) -> ReceiveOutcome {
        if sequence == 0 {
            return ReceiveOutcome::Invalid;
        }
        if self.retained_floor.is_some_and(|floor| sequence < floor) {
            return ReceiveOutcome::BeyondRetention {
                ack_upto: self.highest_contiguous,
                retained_floor: self.retained_floor.unwrap_or(1),
            };
        }
        if self
            .highest_contiguous
            .is_some_and(|highest| sequence <= highest)
        {
            return ReceiveOutcome::Duplicate {
                ack_upto: self.highest_contiguous,
            };
        }
        let expected = self.highest_contiguous.map_or(1, |highest| highest + 1);
        if sequence == expected {
            self.highest_contiguous = Some(sequence);
            // 连续推进：把缓存中紧邻的后续序号一起消费。
            while let Some(next) = self.highest_contiguous.map(|highest| highest + 1) {
                if !self.buffered.remove(&next) {
                    break;
                }
                self.highest_contiguous = Some(next);
            }
            return ReceiveOutcome::Contiguous {
                ack_upto: self.highest_contiguous,
                advanced_to: sequence,
            };
        }
        if self.buffered.len() >= MAX_BUFFERED_OBSERVATIONS {
            return ReceiveOutcome::BufferLimitExceeded {
                ack_upto: self.highest_contiguous,
            };
        }
        self.buffered.insert(sequence);
        ReceiveOutcome::Buffered {
            ack_upto: self.highest_contiguous,
            missing_from: expected,
            missing_count: sequence - expected,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveOutcome {
    /// 新序号连续推进。
    Contiguous {
        ack_upto: Option<u64>,
        advanced_to: u64,
    },
    /// 重复序号：已持久接收过，不重复处理。
    Duplicate { ack_upto: Option<u64> },
    /// 乱序：缓存但不确认缺口之前的序号。
    Buffered {
        ack_upto: Option<u64>,
        missing_from: u64,
        missing_count: u64,
    },
    /// 超出保留窗口：要求快照/对账。
    BeyondRetention {
        ack_upto: Option<u64>,
        retained_floor: u64,
    },
    /// 缓存超限：必须快照，不再无限缓存。
    BufferLimitExceeded { ack_upto: Option<u64> },
    /// 序号 0 非法。
    Invalid,
}

impl ReceiveOutcome {
    pub const fn ack_upto(&self) -> Option<u64> {
        match self {
            Self::Contiguous { ack_upto, .. }
            | Self::Duplicate { ack_upto }
            | Self::Buffered { ack_upto, .. }
            | Self::BeyondRetention { ack_upto, .. }
            | Self::BufferLimitExceeded { ack_upto } => *ack_upto,
            Self::Invalid => None,
        }
    }

    /// ACK 只表示已持久接收，不表示业务处理完成或成果已接受（§15.5）。
    pub const fn requires_snapshot(&self) -> bool {
        matches!(
            self,
            Self::BeyondRetention { .. } | Self::BufferLimitExceeded { .. }
        )
    }

    pub const fn disposition(&self) -> &'static str {
        match self {
            Self::Contiguous { .. } => "contiguous",
            Self::Duplicate { .. } => "duplicate",
            Self::Buffered { .. } => "gap_buffered",
            Self::BeyondRetention { .. } => "beyond_retention",
            Self::BufferLimitExceeded { .. } => "buffer_limit",
            Self::Invalid => "invalid_sequence",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(slot: CapabilitySlot, level: SupportLevel) -> CapabilityClaim {
        CapabilityClaim {
            slot,
            level,
            mode: None,
            source: EvidenceSource::PlatformProbe,
            evidence_level: EvidenceLevel::ProbeVerified,
            observed_at_ms: 1_000,
            expires_at_ms: Some(2_000),
            reason: Some("probe 结论".to_string()),
        }
    }

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor {
            protocol: PROTOCOL_ID.to_string(),
            adapter_id: "adapter.test".to_string(),
            adapter_version: "0.1.0".to_string(),
            capability_id: "cap.test.v1".to_string(),
            platform: "linux".to_string(),
            transport: "stdio".to_string(),
            effect_class: EffectClass::ProcessExecution,
            operations: vec![
                Operation::Describe,
                Operation::Probe,
                Operation::Start,
                Operation::Inspect,
                Operation::Observe,
            ],
            observe: ObserveTransport::Poll,
            capabilities: vec![claim(CapabilitySlot::Cancellation, SupportLevel::Supported)],
            dedup_retention_ms: None,
            limitations: vec!["无可靠观察回放".to_string()],
        }
    }

    #[test]
    fn required_operations_are_enforced() {
        let mut desc = descriptor();
        desc.operations.retain(|op| *op != Operation::Start);
        assert_eq!(
            desc.validate(),
            Err(ContractError::MissingRequiredOperation(Operation::Start))
        );
    }

    #[test]
    fn observe_declaration_must_match_transport() {
        let mut desc = descriptor();
        desc.observe = ObserveTransport::None;
        assert_eq!(desc.validate(), Err(ContractError::MissingObserveTransport));

        let mut desc = descriptor();
        desc.operations.retain(|op| *op != Operation::Observe);
        assert_eq!(
            desc.validate(),
            Err(ContractError::ObserveTransportWithoutOperation)
        );
    }

    #[test]
    fn unknown_capability_cannot_be_supported() {
        let mut desc = descriptor();
        desc.capabilities.push(CapabilityClaim {
            level: SupportLevel::Supported,
            ..claim(
                CapabilitySlot::Other("quantum_tools".to_string()),
                SupportLevel::Supported,
            )
        });
        assert!(matches!(
            desc.validate(),
            Err(ContractError::UnknownCapabilityClaimedSupported(_))
        ));
    }

    #[test]
    fn self_declared_evidence_cannot_reach_probe_level() {
        let mut desc = descriptor();
        desc.capabilities = vec![CapabilityClaim {
            source: EvidenceSource::AdapterSelfDeclared,
            evidence_level: EvidenceLevel::ProbeVerified,
            ..claim(CapabilitySlot::Metering, SupportLevel::Supported)
        }];
        assert!(matches!(
            desc.validate(),
            Err(ContractError::EvidenceExceedsSource { .. })
        ));
    }

    #[test]
    fn supported_requires_probe_evidence_and_unsupported_requires_reason() {
        let mut desc = descriptor();
        desc.capabilities = vec![CapabilityClaim {
            evidence_level: EvidenceLevel::SelfDeclared,
            ..claim(CapabilitySlot::Metering, SupportLevel::Supported)
        }];
        assert!(matches!(
            desc.validate(),
            Err(ContractError::SupportWithoutProbeEvidence(_))
        ));

        let mut desc = descriptor();
        desc.capabilities = vec![CapabilityClaim {
            reason: None,
            ..claim(CapabilitySlot::Metering, SupportLevel::Unsupported)
        }];
        assert!(matches!(
            desc.validate(),
            Err(ContractError::UnsupportedWithoutReason(_))
        ));
    }

    #[test]
    fn probe_report_never_reports_blank_success() {
        let item = |aspect, status| ProbeItem {
            aspect,
            status,
            reason: "test".to_string(),
            source: EvidenceSource::PlatformProbe,
            scope: "local".to_string(),
            observed_at_ms: 1,
            expires_at_ms: None,
        };
        let report = ProbeReport {
            adapter_id: "a".to_string(),
            adapter_version: "1".to_string(),
            items: vec![
                item(ProbeAspect::Environment, ProbeStatus::Ready),
                item(ProbeAspect::Credentials, ProbeStatus::NotReady),
            ],
            generated_at_ms: 10,
        };
        assert_eq!(report.overall(), ProbeStatus::NotReady);

        let mixed = ProbeReport {
            items: vec![
                item(ProbeAspect::Environment, ProbeStatus::Ready),
                item(ProbeAspect::Quota, ProbeStatus::Unsupported),
            ],
            ..report.clone()
        };
        // 不适用项不阻塞：整体仍可就绪，但该项本身仍是 unsupported。
        assert_eq!(mixed.overall(), ProbeStatus::Ready);
        assert_eq!(mixed.blocking_items().len(), 0);

        let empty = ProbeReport {
            items: vec![],
            ..report
        };
        assert_eq!(empty.overall(), ProbeStatus::Unknown);
    }

    #[test]
    fn process_exit_never_yields_acceptance() {
        for code in [0, 1, 2, 127, 130] {
            assert_eq!(
                map_process_exit(code).acceptance,
                AcceptanceSignal::PendingVerification
            );
        }
        assert_eq!(map_process_exit(0).state, ExecutionState::Completed);
        assert_eq!(map_process_exit(1).state, ExecutionState::Failed);
        assert_eq!(map_stop_reason("end_turn"), ExecutionState::Completed);
        assert_eq!(
            map_stop_reason("weird_future_reason"),
            ExecutionState::Unknown
        );
    }

    #[test]
    fn terminal_state_is_not_overwritten_by_late_detail() {
        let completed = ExecutionState::Completed;
        assert!(
            completed
                .advance(ExecutionState::Running, TransitionAuthority::Observed)
                .is_err()
        );
        assert_eq!(
            completed
                .advance(ExecutionState::Unknown, TransitionAuthority::Reconciled)
                .unwrap(),
            ExecutionState::Unknown
        );
    }

    #[test]
    fn cancelling_is_not_cancelled_until_confirmed() {
        let cancelling = ExecutionState::Running
            .advance(ExecutionState::Cancelling, TransitionAuthority::Observed)
            .unwrap();
        assert_eq!(cancelling, ExecutionState::Cancelling);
        assert!(!CancelStage::Requested.stop_confirmed());
        assert!(CancelStage::Confirmed.stop_confirmed());
        assert_eq!(
            cancelling
                .advance(ExecutionState::Cancelled, TransitionAuthority::Observed)
                .unwrap(),
            ExecutionState::Cancelled
        );
    }

    #[test]
    fn ack_stops_at_gap() {
        let mut cursor = ObservationCursor::default();
        assert_eq!(
            cursor.receive(1),
            ReceiveOutcome::Contiguous {
                ack_upto: Some(1),
                advanced_to: 1
            }
        );
        assert_eq!(
            cursor.receive(3),
            ReceiveOutcome::Buffered {
                ack_upto: Some(1),
                missing_from: 2,
                missing_count: 1
            }
        );
        // 缺 2 时收到 3 不能确认到 3。
        assert_eq!(cursor.ack_upto(), Some(1));
        // 2 到达后连续推进到 3。
        assert_eq!(
            cursor.receive(2),
            ReceiveOutcome::Contiguous {
                ack_upto: Some(3),
                advanced_to: 2
            }
        );
        assert_eq!(
            cursor.receive(2),
            ReceiveOutcome::Duplicate { ack_upto: Some(3) }
        );
    }

    #[test]
    fn beyond_retention_requires_snapshot() {
        let mut cursor = ObservationCursor::new(Some(10), Some(5));
        let outcome = cursor.receive(3);
        assert_eq!(
            outcome,
            ReceiveOutcome::BeyondRetention {
                ack_upto: Some(10),
                retained_floor: 5
            }
        );
        assert!(outcome.requires_snapshot());
        assert_eq!(cursor.receive(0), ReceiveOutcome::Invalid);
    }

    #[test]
    fn start_reuse_requires_dedup_query_and_retention() {
        let mut desc = descriptor();
        assert_eq!(start_reuse_policy(&desc), StartReuse::ReconcileThenUnknown);
        desc.dedup_retention_ms = Some(3_600_000);
        assert_eq!(start_reuse_policy(&desc), StartReuse::ReconcileThenUnknown);
        desc.capabilities.push(claim(
            CapabilitySlot::ExternalIdempotency,
            SupportLevel::Supported,
        ));
        assert_eq!(start_reuse_policy(&desc), StartReuse::SafeResend);

        let mut desc = descriptor();
        desc.operations.retain(|op| *op != Operation::Inspect);
        desc.capabilities.push(claim(
            CapabilitySlot::ExternalIdempotency,
            SupportLevel::Supported,
        ));
        assert_eq!(
            start_reuse_policy(&desc),
            StartReuse::RecordAttemptThenUnknown
        );
    }

    #[test]
    fn canonical_params_change_with_any_reference() {
        let request = StartRequest {
            invocation_id: "inv-1".to_string(),
            run_id: "run-1".to_string(),
            work_package_revision: "wp-1".to_string(),
            attempt_id: "attempt-1".to_string(),
            binding_id: "binding-1".to_string(),
            fencing_token: 12,
            agent_ref: "agent-1".to_string(),
            runtime_config_revision: "rt-2".to_string(),
            context_bundle_ref: "ctx-1".to_string(),
            workspace_ref: "ws-1".to_string(),
            grant_ref: "grant-1".to_string(),
            reservation_refs: vec!["res-2".to_string(), "res-1".to_string()],
            deadline_at_ms: Some(1_700_000_000_000),
        };
        let mut reordered = request.clone();
        reordered.reservation_refs = vec!["res-1".to_string(), "res-2".to_string()];
        assert_eq!(request.canonical_params(), reordered.canonical_params());
        assert_eq!(request.params_digest(), reordered.params_digest());

        let mut changed = request.clone();
        changed.fencing_token = 13;
        assert_ne!(request.canonical_params(), changed.canonical_params());
        assert_ne!(request.params_digest(), changed.params_digest());
    }
}
