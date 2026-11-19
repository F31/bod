//! bod-execution/v1 描述符、Start 请求与观察信封的 schema 与严格解析（P0 D4 任务 5.1）。
//!
//! 边界：本模块只做 JSON 契约（schema 文本、fixture、解析、降级规则），不做网络与进程 IO。
//! 未知能力保留原名并降级为 unknown；不认识的等级字符串同样降级，不当作 supported。

use crate::domain::execution::{
    AdapterDescriptor, CapabilityClaim, CapabilitySlot, ContractError, EffectClass, EvidenceLevel,
    EvidenceSource, ExecutionState, ObservationEnvelope, ObservationType, ObserveTransport,
    Operation, PROTOCOL_ID, StartRequest, SupportLevel,
};
use serde::Deserialize;
use std::fmt;

const DESCRIBE_SCHEMA: &str = include_str!("../assets/execution/describe.schema.json");
const START_SCHEMA: &str = include_str!("../assets/execution/start.schema.json");
const OBSERVATION_SCHEMA: &str = include_str!("../assets/execution/observation.schema.json");
const DESCRIPTOR_FIXTURE: &str = include_str!("../assets/execution/descriptor.fixture.json");
const UNKNOWN_CAPABILITY_FIXTURE: &str =
    include_str!("../assets/execution/descriptor.unknown-capability.json");

/// describe 响应的 JSON Schema（G-5 的 schema 侧证据）。
pub const fn describe_schema() -> &'static str {
    DESCRIBE_SCHEMA
}

pub const fn start_schema() -> &'static str {
    START_SCHEMA
}

pub const fn observation_schema() -> &'static str {
    OBSERVATION_SCHEMA
}

/// 一个自洽的最小描述符样本：覆盖全部必需操作与能力分项。
pub const fn descriptor_fixture() -> &'static str {
    DESCRIPTOR_FIXTURE
}

/// 未知能力样本：`quantum_tool_router` 与未知等级都必须降级为 unknown。
pub const fn unknown_capability_fixture() -> &'static str {
    UNKNOWN_CAPABILITY_FIXTURE
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    JsonSyntax(String),
    JsonShape(String),
    UnknownOperation(String),
    UnknownObserveTransport(String),
    UnknownEffectClass(String),
    UnknownEvidenceSource(String),
    UnknownEvidenceLevel(String),
    Contract(ContractError),
}

impl SchemaError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::JsonSyntax(_) => "E_DESC_JSON_SYNTAX",
            Self::JsonShape(_) => "E_DESC_JSON_SHAPE",
            Self::UnknownOperation(_) => "E_DESC_UNKNOWN_OPERATION",
            Self::UnknownObserveTransport(_) => "E_DESC_UNKNOWN_OBSERVE",
            Self::UnknownEffectClass(_) => "E_DESC_UNKNOWN_EFFECT_CLASS",
            Self::UnknownEvidenceSource(_) => "E_DESC_UNKNOWN_EVIDENCE_SOURCE",
            Self::UnknownEvidenceLevel(_) => "E_DESC_UNKNOWN_EVIDENCE_LEVEL",
            Self::Contract(err) => err.code(),
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JsonSyntax(message) => write!(f, "描述符 JSON 语法错误: {message}"),
            Self::JsonShape(message) => write!(f, "描述符字段不合法: {message}"),
            Self::UnknownOperation(name) => write!(f, "未实现的操作声明: {name}"),
            Self::UnknownObserveTransport(name) => write!(f, "未知观察传输方式: {name}"),
            Self::UnknownEffectClass(name) => write!(f, "未知副作用类别: {name}"),
            Self::UnknownEvidenceSource(name) => write!(f, "未知证据来源: {name}"),
            Self::UnknownEvidenceLevel(name) => write!(f, "未知证据等级: {name}"),
            Self::Contract(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SchemaError {}

impl From<ContractError> for SchemaError {
    fn from(value: ContractError) -> Self {
        Self::Contract(value)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDescriptor {
    protocol: String,
    adapter_id: String,
    adapter_version: String,
    capability_id: String,
    platform: String,
    transport: String,
    effect_class: String,
    operations: Vec<String>,
    observe: String,
    capabilities: Vec<RawCapability>,
    #[serde(default)]
    dedup_retention_ms: Option<i64>,
    #[serde(default)]
    limitations: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapability {
    name: String,
    level: String,
    #[serde(default)]
    mode: Option<String>,
    source: String,
    evidence_level: String,
    observed_at_ms: i64,
    #[serde(default)]
    expires_at_ms: Option<i64>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStart {
    protocol: String,
    invocation_id: String,
    run_id: String,
    work_package_revision: String,
    attempt_id: String,
    binding_id: String,
    fencing_token: u64,
    agent_ref: String,
    runtime_config_revision: String,
    context_bundle_ref: String,
    workspace_ref: String,
    grant_ref: String,
    reservation_refs: Vec<String>,
    #[serde(default)]
    deadline_at_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObservation {
    invocation_id: String,
    source_epoch: u64,
    stream_id: String,
    source_sequence: u64,
    observed_at_ms: i64,
    #[serde(rename = "type")]
    kind: String,
    payload: String,
    origin: String,
}

fn syntax_or_shape(err: &serde_json::Error) -> SchemaError {
    match err.classify() {
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
            SchemaError::JsonSyntax(err.to_string())
        }
        _ => SchemaError::JsonShape(err.to_string()),
    }
}

/// 解析 describe 响应并做契约校验：未知能力降级为 unknown，其余不自洽一律拒绝。
pub fn parse_descriptor(json: &str) -> Result<AdapterDescriptor, SchemaError> {
    let raw: RawDescriptor = serde_json::from_str(json).map_err(|err| syntax_or_shape(&err))?;
    let mut operations = Vec::with_capacity(raw.operations.len());
    for name in &raw.operations {
        let operation =
            Operation::parse(name).ok_or_else(|| SchemaError::UnknownOperation(name.clone()))?;
        if !operations.contains(&operation) {
            operations.push(operation);
        }
    }
    let observe = ObserveTransport::parse(&raw.observe)
        .ok_or_else(|| SchemaError::UnknownObserveTransport(raw.observe.clone()))?;
    let effect_class = EffectClass::parse(&raw.effect_class)
        .ok_or_else(|| SchemaError::UnknownEffectClass(raw.effect_class.clone()))?;

    let mut capabilities = Vec::with_capacity(raw.capabilities.len());
    for capability in raw.capabilities {
        capabilities.push(convert_capability(capability)?);
    }

    let descriptor = AdapterDescriptor {
        protocol: raw.protocol,
        adapter_id: raw.adapter_id,
        adapter_version: raw.adapter_version,
        capability_id: raw.capability_id,
        platform: raw.platform,
        transport: raw.transport,
        effect_class,
        operations,
        observe,
        capabilities,
        dedup_retention_ms: raw.dedup_retention_ms,
        limitations: raw.limitations,
    };
    descriptor.validate()?;
    Ok(descriptor)
}

fn convert_capability(raw: RawCapability) -> Result<CapabilityClaim, SchemaError> {
    let slot = CapabilitySlot::parse(&raw.name);
    let mut level = SupportLevel::parse(&raw.level);
    let mut reason = raw.reason;
    if !slot.is_known() && level == SupportLevel::Supported {
        level = SupportLevel::Unknown;
        reason = Some(format!(
            "未识别的能力 '{}' 自报 supported，按 unknown 保留，不写入支持矩阵",
            raw.name
        ));
    }
    if level != SupportLevel::Supported && reason.is_none() {
        reason = Some("适配器未给出原因，按未知处理".to_string());
    }
    let source = EvidenceSource::parse(&raw.source)
        .ok_or_else(|| SchemaError::UnknownEvidenceSource(raw.source.clone()))?;
    let evidence_level = EvidenceLevel::parse(&raw.evidence_level)
        .ok_or_else(|| SchemaError::UnknownEvidenceLevel(raw.evidence_level.clone()))?;

    Ok(CapabilityClaim {
        slot,
        level,
        mode: raw.mode,
        source,
        evidence_level,
        observed_at_ms: raw.observed_at_ms,
        expires_at_ms: raw.expires_at_ms,
        reason,
    })
}

pub fn parse_start_request(json: &str) -> Result<StartRequest, SchemaError> {
    let raw: RawStart = serde_json::from_str(json).map_err(|err| syntax_or_shape(&err))?;
    if raw.protocol != PROTOCOL_ID {
        return Err(ContractError::ProtocolMismatch {
            expected: PROTOCOL_ID,
            actual: raw.protocol,
        }
        .into());
    }
    Ok(StartRequest {
        invocation_id: raw.invocation_id,
        run_id: raw.run_id,
        work_package_revision: raw.work_package_revision,
        attempt_id: raw.attempt_id,
        binding_id: raw.binding_id,
        fencing_token: raw.fencing_token,
        agent_ref: raw.agent_ref,
        runtime_config_revision: raw.runtime_config_revision,
        context_bundle_ref: raw.context_bundle_ref,
        workspace_ref: raw.workspace_ref,
        grant_ref: raw.grant_ref,
        reservation_refs: raw.reservation_refs,
        deadline_at_ms: raw.deadline_at_ms,
    })
}

pub fn parse_observation(json: &str) -> Result<ObservationEnvelope, SchemaError> {
    let raw: RawObservation = serde_json::from_str(json).map_err(|err| syntax_or_shape(&err))?;
    let kind = ObservationType::parse(&raw.kind)
        .ok_or_else(|| SchemaError::JsonShape(format!("未知观察类型: {}", raw.kind)))?;
    Ok(ObservationEnvelope {
        invocation_id: raw.invocation_id,
        source_epoch: raw.source_epoch,
        stream_id: raw.stream_id,
        source_sequence: raw.source_sequence,
        observed_at_ms: raw.observed_at_ms,
        kind,
        payload: raw.payload,
        origin: raw.origin,
    })
}

/// 从 schema 自身读出操作枚举：证明确实是 schema 在约束必需操作，而不是代码里另写一份。
pub fn schema_operations() -> Vec<Operation> {
    let value: serde_json::Value =
        serde_json::from_str(DESCRIBE_SCHEMA).expect("内置 describe schema 必须是合法 JSON");
    let names = value
        .pointer("/$defs/operation/enum")
        .and_then(|node| node.as_array())
        .expect("describe schema 必须声明 $defs.operation.enum");
    names
        .iter()
        .filter_map(|name| name.as_str().and_then(Operation::parse))
        .collect()
}

/// 序列化 Start 请求，供适配器与 conformance 复用同一字段集合。
pub fn start_request_json(request: &StartRequest) -> String {
    serde_json::json!({
        "protocol": PROTOCOL_ID,
        "invocation_id": request.invocation_id,
        "run_id": request.run_id,
        "work_package_revision": request.work_package_revision,
        "attempt_id": request.attempt_id,
        "binding_id": request.binding_id,
        "fencing_token": request.fencing_token,
        "agent_ref": request.agent_ref,
        "runtime_config_revision": request.runtime_config_revision,
        "context_bundle_ref": request.context_bundle_ref,
        "workspace_ref": request.workspace_ref,
        "grant_ref": request.grant_ref,
        "reservation_refs": request.reservation_refs,
        "deadline_at_ms": request.deadline_at_ms,
    })
    .to_string()
}

/// 状态名的 JSON 侧稳定写法（报告与 fixture 共用）。
pub const fn state_name(state: ExecutionState) -> &'static str {
    state.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_covers_every_required_operation() {
        let declared = schema_operations();
        for operation in Operation::REQUIRED {
            assert!(
                declared.contains(&operation),
                "schema 必须声明必需操作 {}",
                operation.as_str()
            );
        }
        let descriptor = parse_descriptor(descriptor_fixture()).expect("fixture 必须自洽");
        for operation in Operation::REQUIRED {
            assert!(descriptor.supports(operation));
        }
        assert_ne!(descriptor.observe, ObserveTransport::None);
        assert_eq!(descriptor.protocol, PROTOCOL_ID);
        assert_eq!(descriptor.effect_class, EffectClass::ProcessExecution);
    }

    #[test]
    fn fixture_declares_each_capability_once_with_evidence() {
        let descriptor = parse_descriptor(descriptor_fixture()).unwrap();
        for claim in &descriptor.capabilities {
            assert!(claim.observed_at_ms > 0, "能力声明必须带观测时间");
            assert!(
                claim.evidence_level.rank() <= claim.source.ceiling().rank(),
                "证据等级不得超过来源上限: {}",
                claim.slot.as_str()
            );
            if claim.level == SupportLevel::Supported {
                assert!(claim.evidence_level.rank() >= EvidenceLevel::ProbeVerified.rank());
            }
        }
        let matrix = descriptor.support_matrix();
        assert_eq!(matrix.len(), descriptor.capabilities.len());
        assert!(
            matrix
                .iter()
                .all(|row| !row.capability.is_empty() && row.observed_at_ms > 0)
        );
    }

    #[test]
    fn unknown_capability_is_downgraded_not_dropped() {
        let descriptor = parse_descriptor(unknown_capability_fixture()).unwrap();
        let unknown = descriptor
            .capability(&CapabilitySlot::Other("quantum_tool_router".to_string()))
            .expect("未知能力必须保留原名");
        assert_eq!(unknown.level, SupportLevel::Unknown);
        assert!(unknown.reason.as_deref().unwrap_or("").contains("unknown"));

        // 不认识的等级字符串也降级为 unknown，不当作 supported。
        let model = descriptor
            .capability(&CapabilitySlot::ModelControl)
            .expect("model_control 应保留");
        assert_eq!(model.level, SupportLevel::Unknown);
    }

    #[test]
    fn descriptor_missing_start_is_rejected() {
        let json = descriptor_fixture().replace("\"start\",", "");
        let err = parse_descriptor(&json).unwrap_err();
        assert_eq!(err.code(), "E_OP_REQUIRED_MISSING");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let json = descriptor_fixture().replace(
            "\"limitations\"",
            "\"trust_tier\": \"gold\",\n  \"limitations\"",
        );
        let err = parse_descriptor(&json).unwrap_err();
        assert_eq!(err.code(), "E_DESC_JSON_SHAPE");
    }

    #[test]
    fn start_request_round_trip_keeps_params_stable() {
        let json = serde_json::json!({
            "protocol": PROTOCOL_ID,
            "invocation_id": "inv-1",
            "run_id": "run-1",
            "work_package_revision": "wp-1",
            "attempt_id": "attempt-1",
            "binding_id": "binding-1",
            "fencing_token": 12,
            "agent_ref": "agent-1",
            "runtime_config_revision": "rt-1",
            "context_bundle_ref": "ctx-1",
            "workspace_ref": "ws-1",
            "grant_ref": "grant-1",
            "reservation_refs": ["res-1"],
            "deadline_at_ms": 1_700_000_000_000i64
        })
        .to_string();
        let request = parse_start_request(&json).unwrap();
        let again = parse_start_request(&start_request_json(&request)).unwrap();
        assert_eq!(request.canonical_params(), again.canonical_params());
        assert_eq!(request.params_digest(), again.params_digest());
    }

    #[test]
    fn observation_envelope_rejects_unknown_type() {
        let json = serde_json::json!({
            "invocation_id": "inv-1",
            "source_epoch": 1,
            "stream_id": "business",
            "source_sequence": 1,
            "observed_at_ms": 10,
            "type": "telepathy",
            "payload": "{}",
            "origin": "executor"
        })
        .to_string();
        assert_eq!(
            parse_observation(&json).unwrap_err().code(),
            "E_DESC_JSON_SHAPE"
        );
    }

    #[test]
    fn all_bundled_schemas_are_valid_json() {
        for text in [describe_schema(), start_schema(), observation_schema()] {
            let value: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(
                value["$schema"].as_str(),
                Some("https://json-schema.org/draft/2020-12/schema")
            );
        }
    }
}
