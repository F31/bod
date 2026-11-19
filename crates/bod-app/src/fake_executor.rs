//! Fake Executor：只用于执行协议故障 conformance（P0 D4 任务 5.4）。
//!
//! 硬边界：**不调用模型、不执行任务、不提供任何生产智能执行能力**，其能力声明只用于
//! 让宿主的去重/缺口/终态规则可被真实断言驱动；它不写入支持矩阵，也不参与生产派发。
//!
//! 权威来源：V2.9.6 §15.7「Fake Executor 仅用于上述故障测试」。

use crate::descriptor_schema::parse_descriptor;
use crate::domain::execution::{
    AdapterDescriptor, CancelStage, CapabilityClaim, CapabilitySlot, EffectClass, EvidenceLevel,
    EvidenceSource, ExecutionState, ObservationEnvelope, ObservationType, ObserveTransport,
    Operation, PROTOCOL_ID, ProbeAspect, ProbeItem, ProbeReport, ProbeStatus, StartRequest,
    SupportLevel,
};
use crate::invocation::{StartOutcome, StartTransport};
use std::collections::BTreeMap;

pub const ADAPTER_ID: &str = "adapter.fake-executor";
pub const ADAPTER_VERSION: &str = "0.1.0";

/// 故障场景。每个场景对应 conformance 里的一个命名用例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeScenario {
    /// 正常接受并给出一次进度观察。
    HappyPath,
    /// 第一次 start 回包丢失，但执行已创建；第二次同 ID 同参返回原执行。
    ReplyLostOnce,
    /// 观察乱序：1、3、2。
    OutOfOrder,
    /// 断档：序列从 90 开始，低于保留下界。
    GapBeyondRetention,
    /// 进程退出码 0 且提交候选成果。
    ProcessExitZero,
    /// 取消只被受理、未确认停止。
    CancelUnconfirmed,
    /// 凭据失效：环境就绪、凭据不 ready，且不触发付费探测。
    AuthExpired,
    /// 超大日志：单条 200KB。
    HugeLog,
    /// 终态后到达最终费用。
    LateCost,
}

impl FakeScenario {
    pub const ALL: [FakeScenario; 9] = [
        Self::HappyPath,
        Self::ReplyLostOnce,
        Self::OutOfOrder,
        Self::GapBeyondRetention,
        Self::ProcessExitZero,
        Self::CancelUnconfirmed,
        Self::AuthExpired,
        Self::HugeLog,
        Self::LateCost,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HappyPath => "happy_path",
            Self::ReplyLostOnce => "reply_lost_once",
            Self::OutOfOrder => "out_of_order",
            Self::GapBeyondRetention => "gap_beyond_retention",
            Self::ProcessExitZero => "process_exit_zero",
            Self::CancelUnconfirmed => "cancel_unconfirmed",
            Self::AuthExpired => "auth_expired",
            Self::HugeLog => "huge_log",
            Self::LateCost => "late_cost",
        }
    }
}

#[derive(Debug, Clone)]
struct FakeExecution {
    execution_id: String,
    state: ExecutionState,
    exit_code: Option<i32>,
}

/// 故障注入用执行端。所有行为由场景脚本决定，不做任何真实推理。
pub struct FakeExecutor {
    scenario: FakeScenario,
    descriptor: AdapterDescriptor,
    executions: BTreeMap<String, FakeExecution>,
    params: BTreeMap<String, String>,
    reply_lost_armed: bool,
    start_calls: usize,
    cancel_calls: usize,
}

impl FakeExecutor {
    pub fn new(scenario: FakeScenario) -> Self {
        Self {
            scenario,
            descriptor: fake_descriptor(scenario),
            executions: BTreeMap::new(),
            params: BTreeMap::new(),
            reply_lost_armed: scenario == FakeScenario::ReplyLostOnce,
            start_calls: 0,
            cancel_calls: 0,
        }
    }

    pub const fn is_production_capable() -> bool {
        false
    }

    /// 支持矩阵的排除说明：调用方据此拒绝把 Fake Executor 写成 Supported。
    pub const fn production_exclusion_reason() -> &'static str {
        "Fake Executor 只用于故障 conformance，不调用模型，不提供生产能力"
    }

    pub const fn scenario(&self) -> FakeScenario {
        self.scenario
    }

    pub const fn start_calls(&self) -> usize {
        self.start_calls
    }

    pub const fn cancel_calls(&self) -> usize {
        self.cancel_calls
    }

    pub fn execution_state(&self, invocation_id: &str) -> Option<ExecutionState> {
        self.executions.get(invocation_id).map(|exec| exec.state)
    }

    pub fn exit_code(&self, invocation_id: &str) -> Option<i32> {
        self.executions
            .get(invocation_id)
            .and_then(|exec| exec.exit_code)
    }

    /// 生成该场景的观察序列（含乱序与断档），供宿主 ACK/缺口逻辑消费。
    pub fn observations(&self, invocation_id: &str) -> Vec<ObservationEnvelope> {
        let make = |sequence: u64, kind: ObservationType, payload: &str| ObservationEnvelope {
            invocation_id: invocation_id.to_string(),
            source_epoch: 1,
            stream_id: if kind == ObservationType::Log {
                "log".to_string()
            } else {
                "business".to_string()
            },
            source_sequence: sequence,
            observed_at_ms: 1_763_500_000_000 + sequence as i64,
            kind,
            payload: payload.to_string(),
            origin: ADAPTER_ID.to_string(),
        };
        match self.scenario {
            FakeScenario::OutOfOrder => vec![
                make(1, ObservationType::Progress, "step-1"),
                make(3, ObservationType::Progress, "step-3"),
                make(2, ObservationType::Progress, "step-2"),
            ],
            FakeScenario::GapBeyondRetention => vec![
                make(90, ObservationType::Log, "old-90"),
                make(91, ObservationType::Log, "old-91"),
            ],
            FakeScenario::HugeLog => vec![make(1, ObservationType::Log, &"L".repeat(200 * 1024))],
            FakeScenario::LateCost => vec![
                make(
                    1,
                    ObservationType::FinalResult,
                    "{\"candidate\":\"diff.patch\"}",
                ),
                make(
                    2,
                    ObservationType::Usage,
                    "{\"billed_micros\":120,\"final\":true}",
                ),
            ],
            FakeScenario::ProcessExitZero => vec![make(
                1,
                ObservationType::FinalResult,
                "{\"candidate\":\"ok\",\"exit_code\":0}",
            )],
            _ => vec![make(1, ObservationType::Progress, "accepted")],
        }
    }

    /// 取消：只有 HappyPath 类场景能确认停止；CancelUnconfirmed 只受理。
    pub fn cancel(&mut self, invocation_id: &str) -> CancelStage {
        self.cancel_calls += 1;
        if let Some(exec) = self.executions.get_mut(invocation_id) {
            exec.state = match self.scenario {
                FakeScenario::CancelUnconfirmed => ExecutionState::Cancelling,
                _ => ExecutionState::Cancelled,
            };
        }
        match self.scenario {
            FakeScenario::CancelUnconfirmed => CancelStage::Requested,
            _ => CancelStage::Confirmed,
        }
    }

    /// 分项探测：没有模型凭据这一项，也不做任何付费推理。
    pub fn probe(&self, now_ms: i64) -> ProbeReport {
        let item = |aspect, status, reason: &str| ProbeItem {
            aspect,
            status,
            reason: reason.to_string(),
            source: EvidenceSource::PlatformProbe,
            scope: "fake-executor/local".to_string(),
            observed_at_ms: now_ms,
            expires_at_ms: Some(now_ms + 300_000),
        };
        let items = match self.scenario {
            FakeScenario::AuthExpired => vec![
                item(
                    ProbeAspect::Environment,
                    ProbeStatus::Ready,
                    "内置故障执行端就绪",
                ),
                item(
                    ProbeAspect::Credentials,
                    ProbeStatus::NotReady,
                    "凭据已失效（故障注入），只检查授权状态，不回传密钥",
                ),
                item(
                    ProbeAspect::Model,
                    ProbeStatus::Unknown,
                    "Fake Executor 不调用模型，模型可用性未知",
                ),
            ],
            _ => vec![
                item(
                    ProbeAspect::Environment,
                    ProbeStatus::Ready,
                    "内置故障执行端就绪",
                ),
                item(
                    ProbeAspect::Credentials,
                    ProbeStatus::Unsupported,
                    "故障执行端不需要凭据",
                ),
                item(
                    ProbeAspect::Model,
                    ProbeStatus::Unsupported,
                    "Fake Executor 不调用模型",
                ),
            ],
        };
        ProbeReport {
            adapter_id: ADAPTER_ID.to_string(),
            adapter_version: ADAPTER_VERSION.to_string(),
            items,
            generated_at_ms: now_ms,
        }
    }
}

impl StartTransport for FakeExecutor {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn start(&mut self, request: &StartRequest) -> StartOutcome {
        self.start_calls += 1;
        let canonical = request.canonical_params();
        if let Some(existing) = self.params.get(&request.invocation_id)
            && existing != &canonical
        {
            return StartOutcome::ParamsRejected {
                reason: format!(
                    "invocation {} 已用另一组参数受理，拒绝对同一 ID 换参",
                    request.invocation_id
                ),
            };
        }

        let first_time = !self.executions.contains_key(&request.invocation_id);
        if first_time {
            if self.scenario == FakeScenario::AuthExpired {
                return StartOutcome::Unavailable {
                    reason: "凭据已失效，无法开始执行".to_string(),
                };
            }
            let execution_id = format!("fake-exec-{}", self.executions.len() + 1);
            let state = match self.scenario {
                FakeScenario::ProcessExitZero => ExecutionState::Completed,
                FakeScenario::ReplyLostOnce => ExecutionState::Accepted,
                _ => ExecutionState::Running,
            };
            let exit_code = (self.scenario == FakeScenario::ProcessExitZero).then_some(0);
            self.executions.insert(
                request.invocation_id.clone(),
                FakeExecution {
                    execution_id,
                    state,
                    exit_code,
                },
            );
            self.params.insert(request.invocation_id.clone(), canonical);
        }

        // 第一次回包丢失：执行已创建但宿主未收到确认。
        if self.reply_lost_armed {
            self.reply_lost_armed = false;
            return StartOutcome::ReplyLost;
        }

        let execution_id = self.executions[&request.invocation_id].execution_id.clone();
        if first_time {
            StartOutcome::Accepted { execution_id }
        } else {
            StartOutcome::Replayed { execution_id }
        }
    }

    fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState> {
        self.executions.get(invocation_id).map(|exec| exec.state)
    }

    fn fault_injection(&self) -> bool {
        true
    }

    fn observations(&mut self, invocation_id: &str) -> Vec<ObservationEnvelope> {
        FakeExecutor::observations(self, invocation_id)
    }

    fn cancel(&mut self, invocation_id: &str) -> CancelStage {
        FakeExecutor::cancel(self, invocation_id)
    }

    fn probe(&self, now_ms: i64) -> Option<ProbeReport> {
        Some(FakeExecutor::probe(self, now_ms))
    }
}

fn fake_descriptor(scenario: FakeScenario) -> AdapterDescriptor {
    let claim =
        |slot: CapabilitySlot, level: SupportLevel, mode: Option<&str>, reason: Option<&str>| {
            CapabilityClaim {
                slot,
                level,
                mode: mode.map(str::to_string),
                source: if level == SupportLevel::Supported {
                    EvidenceSource::TestResult
                } else {
                    EvidenceSource::AdapterSelfDeclared
                },
                evidence_level: if level == SupportLevel::Supported {
                    EvidenceLevel::RuntimeObserved
                } else {
                    EvidenceLevel::SelfDeclared
                },
                observed_at_ms: 1_763_500_000_000,
                expires_at_ms: None,
                reason: reason.map(str::to_string),
            }
        };

    let cancellation = claim(
        CapabilitySlot::Cancellation,
        SupportLevel::Supported,
        Some(if scenario == FakeScenario::CancelUnconfirmed {
            "requested_only"
        } else {
            "process_tree"
        }),
        Some("conformance 场景声明：取消受理与停止确认分离"),
    );

    AdapterDescriptor {
        protocol: PROTOCOL_ID.to_string(),
        adapter_id: ADAPTER_ID.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        capability_id: "bod.fake-executor.v1".to_string(),
        platform: "conformance".to_string(),
        transport: "in-process".to_string(),
        effect_class: EffectClass::ReadOnly,
        operations: vec![
            Operation::Describe,
            Operation::Probe,
            Operation::Start,
            Operation::Inspect,
            Operation::Observe,
            Operation::Cancel,
            Operation::Reconcile,
        ],
        observe: ObserveTransport::Poll,
        capabilities: vec![
            claim(
                CapabilitySlot::ModelControl,
                SupportLevel::Unsupported,
                Some("opaque"),
                Some("故障执行端不调用模型"),
            ),
            claim(
                CapabilitySlot::ToolControl,
                SupportLevel::Unsupported,
                Some("opaque"),
                Some("不执行工具"),
            ),
            claim(
                CapabilitySlot::WorkspaceIsolation,
                SupportLevel::Unsupported,
                Some("none"),
                Some("进程内假执行端不写工作区"),
            ),
            claim(
                CapabilitySlot::Metering,
                SupportLevel::Unsupported,
                Some("none"),
                Some("不产生真实用量；费用相关用例由宿主账本断言"),
            ),
            cancellation,
            claim(
                CapabilitySlot::Recovery,
                SupportLevel::Unsupported,
                Some("none"),
                Some("故障执行端不提供会话恢复"),
            ),
            claim(
                CapabilitySlot::ExternalIdempotency,
                SupportLevel::Supported,
                Some("persistent_in_memory"),
                Some("仅用于 conformance：模拟执行端持久去重"),
            ),
            claim(
                CapabilitySlot::ReliableObservation,
                SupportLevel::Unsupported,
                Some("poll"),
                Some("不提供 ACK/回放，仅用于构造乱序与断档样本"),
            ),
        ],
        dedup_retention_ms: Some(86_400_000),
        limitations: vec![FakeExecutor::production_exclusion_reason().to_string()],
    }
}

/// 供测试确认 Fake Executor 的描述符本身自洽（不是生产能力，但契约必须合法）。
pub fn validated_fake_descriptor(scenario: FakeScenario) -> AdapterDescriptor {
    let descriptor = fake_descriptor(scenario);
    descriptor.validate().expect("fake descriptor 必须自洽");
    descriptor
}

/// 从 fixture 解析出的描述符必须同样合法：防止 fixture 与代码漂移。
pub fn fixture_descriptor_is_valid() -> bool {
    parse_descriptor(crate::descriptor_schema::descriptor_fixture()).is_ok()
}
