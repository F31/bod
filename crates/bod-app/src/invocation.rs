//! invocation 持久去重、回包丢失对账与执行状态推进（P0 D4 任务 5.2—5.3）。
//!
//! 顺序不可颠倒：**先**把接收记录与审计落盘并提交，**再**调用执行端。
//! 回包丢失不是"再发一次"的理由：没有持久去重+查询+保留期时只记录尝试并进入 Unknown，
//! 且始终复用同一 invocation_id，绝不新建 Attempt 规避未知状态（I-04）。

use crate::domain::execution::{
    AdapterDescriptor, CancelStage, ContractError, ExecutionState, InvocationRecord,
    ObservationEnvelope, ProbeReport, StartRequest, StartReuse, TransitionAuthority,
    start_reuse_policy,
};
use crate::store::{SqliteStore, StoreError};

/// 执行端对一次 start 的应答。实现方不得自行重试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// 首次接收：返回新 execution_id。
    Accepted {
        execution_id: String,
    },
    /// 同 invocation_id 同参数重发：返回原 execution_id，不创建第二次执行。
    Replayed {
        execution_id: String,
    },
    ParamsRejected {
        reason: String,
    },
    /// 回包丢失：请求可能已生效，也可能没到。
    ReplyLost,
    Unavailable {
        reason: String,
    },
}

/// 宿主与执行端之间的最小传输端口。真实适配器与 Fake Executor 各自实现。
pub trait StartTransport {
    fn descriptor(&self) -> &AdapterDescriptor;
    fn start(&mut self, request: &StartRequest) -> StartOutcome;
    /// 按 invocation_id 查询当前执行状态；None 表示无法确认（不是"不存在"）。
    fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState>;

    /// 是否能做故障注入（丢回包、乱序、断档等）。真实适配器默认不能，对应 conformance 用例
    /// 必须报 not_applicable，而不是"跳过即通过"。
    fn fault_injection(&self) -> bool {
        false
    }

    /// 适配器产生的观察序列；不支持的适配器返回空，由 conformance 判为 not_applicable。
    fn observations(&mut self, _invocation_id: &str) -> Vec<ObservationEnvelope> {
        Vec::new()
    }

    /// 请求取消；默认视为不支持（未受理），不得据此释放占用。
    fn cancel(&mut self, _invocation_id: &str) -> CancelStage {
        CancelStage::Requested
    }

    /// 分项探测；不支持的适配器返回 None，界面显示"未知"而非成功。
    fn probe(&self, _now_ms: i64) -> Option<ProbeReport> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartVerdict {
    Accepted {
        execution_id: String,
        replayed: bool,
    },
    Rejected {
        code: &'static str,
        reason: String,
    },
    Unknown {
        reason: String,
        reuse: StartReuse,
    },
}

#[derive(Debug)]
pub enum InvocationError {
    Store(StoreError),
    Contract(ContractError),
    /// 观察或状态推进指向一个宿主从未受理的 invocation：先对账，不凭空建记录。
    UnknownInvocation(String),
}

impl std::fmt::Display for InvocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::Contract(err) => write!(f, "{err}"),
            Self::UnknownInvocation(id) => {
                write!(f, "invocation {id} 在宿主侧没有受理记录，不能直接推进状态")
            }
        }
    }
}

impl std::error::Error for InvocationError {}

impl From<StoreError> for InvocationError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<ContractError> for InvocationError {
    fn from(value: ContractError) -> Self {
        Self::Contract(value)
    }
}

/// 推进一次 start。返回的 `Unknown` 必须保留占用与现场，不得自动新建执行。
pub fn submit_start(
    store: &mut SqliteStore,
    transport: &mut dyn StartTransport,
    company_id: &str,
    request: &StartRequest,
    now_ms: i64,
) -> Result<StartVerdict, InvocationError> {
    let descriptor = transport.descriptor().clone();
    let reuse = start_reuse_policy(&descriptor);
    let invocation_id = request.invocation_id.clone();

    let existing = {
        let uow = store.transaction()?;
        uow.get_invocation(&invocation_id)?
    };

    match existing {
        Some(record) => {
            if !record.same_params(request) {
                let uow = store.transaction()?;
                uow.record_start_audit(
                    &invocation_id,
                    "params_rejected",
                    Some("同 invocation_id 携带不同参数"),
                    now_ms,
                )?;
                uow.commit()?;
                return Ok(StartVerdict::Rejected {
                    code: "E_START_PARAMS_MISMATCH",
                    reason: format!(
                        "invocation {} 已用另一组参数受理（{}），同 ID 异参必须拒绝",
                        invocation_id, record.params_digest
                    ),
                });
            }
            if let Some(execution_id) = record.execution_id.clone() {
                let uow = store.transaction()?;
                uow.record_start_audit(
                    &invocation_id,
                    "replayed",
                    Some("同 ID 同参重发，返回原 execution_id"),
                    now_ms,
                )?;
                uow.commit()?;
                return Ok(StartVerdict::Accepted {
                    execution_id,
                    replayed: true,
                });
            }
            // 已持久但从未确认：能力不足时只能停在 Unknown。
            if reuse != StartReuse::SafeResend {
                let uow = store.transaction()?;
                uow.record_start_audit(
                    &invocation_id,
                    "unknown",
                    Some("开始回执仍未知，执行端未声明持久去重，不重发"),
                    now_ms,
                )?;
                uow.commit()?;
                return Ok(StartVerdict::Unknown {
                    reason: "开始回执丢失且执行端未声明持久去重/查询/保留期，保留现场等待裁决"
                        .to_string(),
                    reuse,
                });
            }
            // 安全重发：状态回到 Starting，仍复用同一记录。
            let next = record
                .state
                .advance(ExecutionState::Starting, TransitionAuthority::Reconciled)?;
            let uow = store.transaction()?;
            uow.set_invocation_state(&invocation_id, next, now_ms)?;
            uow.record_start_audit(
                &invocation_id,
                "persisted",
                Some("执行端声明持久去重，按原 invocation_id 安全重发"),
                now_ms,
            )?;
            uow.commit()?;
        }
        None => {
            let record = InvocationRecord::pending(request, company_id, &descriptor, now_ms);
            let uow = store.transaction()?;
            uow.insert_invocation(&record)?;
            uow.record_start_audit(
                &invocation_id,
                "persisted",
                Some("接收记录已落盘，准备发送 start"),
                now_ms,
            )?;
            uow.commit()?;
        }
    }

    // 事务外调用执行端：绝不在持锁期间等待外部系统。
    let outcome = transport.start(request);

    match outcome {
        StartOutcome::Accepted { execution_id } => {
            let uow = store.transaction()?;
            uow.confirm_invocation(
                &invocation_id,
                &execution_id,
                ExecutionState::Accepted,
                now_ms,
            )?;
            uow.record_start_audit(
                &invocation_id,
                "accepted",
                Some(&format!("execution_id={execution_id}")),
                now_ms,
            )?;
            uow.commit()?;
            Ok(StartVerdict::Accepted {
                execution_id,
                replayed: false,
            })
        }
        StartOutcome::Replayed { execution_id } => {
            let uow = store.transaction()?;
            uow.confirm_invocation(
                &invocation_id,
                &execution_id,
                ExecutionState::Accepted,
                now_ms,
            )?;
            uow.record_start_audit(
                &invocation_id,
                "replayed",
                Some(&format!("执行端返回原 execution_id={execution_id}")),
                now_ms,
            )?;
            uow.commit()?;
            Ok(StartVerdict::Accepted {
                execution_id,
                replayed: true,
            })
        }
        StartOutcome::ParamsRejected { reason } => {
            let uow = store.transaction()?;
            uow.set_invocation_state(&invocation_id, ExecutionState::Failed, now_ms)?;
            uow.record_start_audit(&invocation_id, "params_rejected", Some(&reason), now_ms)?;
            uow.commit()?;
            Ok(StartVerdict::Rejected {
                code: "E_START_PARAMS_REJECTED",
                reason,
            })
        }
        StartOutcome::ReplyLost => {
            let uow = store.transaction()?;
            uow.set_invocation_state(&invocation_id, ExecutionState::Unknown, now_ms)?;
            uow.record_start_audit(
                &invocation_id,
                "reply_lost",
                Some("未收到 start 回包：请求可能已生效，保留占用与现场"),
                now_ms,
            )?;
            uow.commit()?;
            Ok(StartVerdict::Unknown {
                reason: "开始回执丢失：已记录尝试，等待查询、人工裁决或对账，不新建执行"
                    .to_string(),
                reuse,
            })
        }
        StartOutcome::Unavailable { reason } => {
            let uow = store.transaction()?;
            uow.set_invocation_state(&invocation_id, ExecutionState::Unknown, now_ms)?;
            uow.record_start_audit(&invocation_id, "unknown", Some(&reason), now_ms)?;
            uow.commit()?;
            Ok(StartVerdict::Unknown { reason, reuse })
        }
    }
}

/// 用执行端查询结果对账。查不到不等于没执行：一律落 Unknown，等人工或下一次对账。
pub fn reconcile_invocation(
    store: &mut SqliteStore,
    transport: &mut dyn StartTransport,
    invocation_id: &str,
    now_ms: i64,
) -> Result<Option<ExecutionState>, InvocationError> {
    let record = {
        let uow = store.transaction()?;
        uow.get_invocation(invocation_id)?
    };
    let Some(record) = record else {
        return Ok(None);
    };

    let observed = transport.inspect(invocation_id);
    let uow = store.transaction()?;
    match observed {
        Some(observed) => match record
            .state
            .advance(observed, TransitionAuthority::Reconciled)
        {
            Ok(next) => {
                uow.set_invocation_state(invocation_id, next, now_ms)?;
                uow.record_start_audit(
                    invocation_id,
                    "unknown",
                    Some(&format!("对账得到状态 {}", next.as_str())),
                    now_ms,
                )?;
                uow.commit()?;
                Ok(Some(next))
            }
            Err(_) => {
                // 对账结果与权威终态矛盾：不覆盖终态，转入 Unknown 等人工裁决。
                uow.set_invocation_state(invocation_id, ExecutionState::Unknown, now_ms)?;
                uow.record_start_audit(
                    invocation_id,
                    "unknown",
                    Some(&format!(
                        "对账状态 {} 与当前 {} 矛盾，转 Unknown 等裁决",
                        observed.as_str(),
                        record.state.as_str()
                    )),
                    now_ms,
                )?;
                uow.commit()?;
                Ok(Some(ExecutionState::Unknown))
            }
        },
        None => {
            uow.set_invocation_state(invocation_id, ExecutionState::Unknown, now_ms)?;
            uow.record_start_audit(
                invocation_id,
                "unknown",
                Some("执行端无法确认该 invocation，保持 Unknown 不重发"),
                now_ms,
            )?;
            uow.commit()?;
            Ok(Some(ExecutionState::Unknown))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateUpdate {
    Applied(ExecutionState),
    /// 终态后的迟到 detail：不改写权威终态。
    Ignored {
        current: ExecutionState,
        attempted: ExecutionState,
    },
}

/// 按观察推进宿主侧执行状态。终态不被迟到 detail 推翻（只能经 reconcile 转 Unknown）。
pub fn advance_invocation_state(
    store: &mut SqliteStore,
    invocation_id: &str,
    next: ExecutionState,
    authority: TransitionAuthority,
    now_ms: i64,
) -> Result<StateUpdate, InvocationError> {
    let record = {
        let uow = store.transaction()?;
        uow.get_invocation(invocation_id)?
    };
    let Some(record) = record else {
        return Err(InvocationError::UnknownInvocation(
            invocation_id.to_string(),
        ));
    };
    match record.state.advance(next, authority) {
        Ok(applied) => {
            let uow = store.transaction()?;
            uow.set_invocation_state(invocation_id, applied, now_ms)?;
            uow.commit()?;
            Ok(StateUpdate::Applied(applied))
        }
        Err(_) => Ok(StateUpdate::Ignored {
            current: record.state,
            attempted: next,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::descriptor_schema::parse_descriptor;
    use crate::domain::execution::{
        CapabilityClaim, CapabilitySlot, EvidenceLevel, EvidenceSource,
    };

    struct StubTransport {
        descriptor: AdapterDescriptor,
        scripted: Vec<StartOutcome>,
        inspect_state: Option<ExecutionState>,
        calls: usize,
    }

    impl StubTransport {
        fn new(descriptor: AdapterDescriptor, scripted: Vec<StartOutcome>) -> Self {
            Self {
                descriptor,
                scripted,
                inspect_state: None,
                calls: 0,
            }
        }
    }

    impl StartTransport for StubTransport {
        fn descriptor(&self) -> &AdapterDescriptor {
            &self.descriptor
        }

        fn start(&mut self, _request: &StartRequest) -> StartOutcome {
            self.calls += 1;
            if self.scripted.len() >= self.calls {
                self.scripted[self.calls - 1].clone()
            } else {
                StartOutcome::ReplyLost
            }
        }

        fn inspect(&mut self, _invocation_id: &str) -> Option<ExecutionState> {
            self.inspect_state
        }
    }

    fn idempotent_descriptor() -> AdapterDescriptor {
        let mut descriptor = parse_descriptor(crate::descriptor_schema::descriptor_fixture())
            .expect("fixture 必须可用");
        // 覆盖 fixture 里 unsupported 的声明：同一能力只能有一个声明。
        descriptor
            .capabilities
            .retain(|claim| claim.slot != CapabilitySlot::ExternalIdempotency);
        descriptor.capabilities.push(CapabilityClaim {
            slot: CapabilitySlot::ExternalIdempotency,
            level: crate::domain::execution::SupportLevel::Supported,
            mode: Some("persistent".to_string()),
            source: EvidenceSource::TestResult,
            evidence_level: EvidenceLevel::RuntimeObserved,
            observed_at_ms: 1_763_500_000_000,
            expires_at_ms: None,
            reason: Some("conformance 样本证明同 ID 返回原 execution".to_string()),
        });
        descriptor.dedup_retention_ms = Some(86_400_000);
        descriptor
    }

    fn request(invocation_id: &str) -> StartRequest {
        StartRequest {
            invocation_id: invocation_id.to_string(),
            run_id: "run-1".to_string(),
            work_package_revision: "wp-1".to_string(),
            attempt_id: "attempt-1".to_string(),
            binding_id: "binding-1".to_string(),
            fencing_token: 12,
            agent_ref: "agent-1".to_string(),
            runtime_config_revision: "rt-1".to_string(),
            context_bundle_ref: "ctx-1".to_string(),
            workspace_ref: "ws-1".to_string(),
            grant_ref: "grant-1".to_string(),
            reservation_refs: vec!["res-1".to_string()],
            deadline_at_ms: Some(1_700_000_000_000),
        }
    }

    fn accepted(execution_id: &str) -> StartOutcome {
        StartOutcome::Accepted {
            execution_id: execution_id.to_string(),
        }
    }

    #[test]
    fn duplicate_start_returns_original_execution_id() {
        let cfg = BodConfig::default();
        let mut store = SqliteStore::in_memory(&cfg).unwrap();
        let descriptor = parse_descriptor(crate::descriptor_schema::descriptor_fixture()).unwrap();
        let mut transport = StubTransport::new(descriptor, vec![accepted("exec-1")]);
        let request = request("inv-1");

        let first = submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            1_000,
        )
        .unwrap();
        assert_eq!(
            first,
            StartVerdict::Accepted {
                execution_id: "exec-1".to_string(),
                replayed: false
            }
        );

        let second = submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            2_000,
        )
        .unwrap();
        assert_eq!(
            second,
            StartVerdict::Accepted {
                execution_id: "exec-1".to_string(),
                replayed: true
            }
        );
        // 只调用执行端一次：重复 start 不再产生第二次执行。
        assert_eq!(transport.calls, 1);
        assert_eq!(store.list_invocations("company-default").unwrap().len(), 1);
    }

    #[test]
    fn same_invocation_with_different_params_is_rejected() {
        let cfg = BodConfig::default();
        let mut store = SqliteStore::in_memory(&cfg).unwrap();
        let descriptor = parse_descriptor(crate::descriptor_schema::descriptor_fixture()).unwrap();
        let mut transport = StubTransport::new(descriptor, vec![accepted("exec-1")]);
        let request = request("inv-1");
        submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            1_000,
        )
        .unwrap();

        let mut changed = request.clone();
        changed.fencing_token = 13;
        let verdict = submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &changed,
            2_000,
        )
        .unwrap();
        match verdict {
            StartVerdict::Rejected { code, .. } => assert_eq!(code, "E_START_PARAMS_MISMATCH"),
            other => panic!("应拒绝同 ID 异参，实际 {other:?}"),
        }
        assert_eq!(transport.calls, 1);
        assert_eq!(store.list_invocations("company-default").unwrap().len(), 1);
    }

    #[test]
    fn reply_lost_records_attempt_and_never_creates_second_attempt() {
        let cfg = BodConfig::default();
        let mut store = SqliteStore::in_memory(&cfg).unwrap();
        // 未声明持久去重：fixture 的 external_idempotency 是 unsupported。
        let descriptor = parse_descriptor(crate::descriptor_schema::descriptor_fixture()).unwrap();
        let mut transport = StubTransport::new(descriptor, vec![StartOutcome::ReplyLost]);
        let request = request("inv-lost");

        let verdict = submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            1_000,
        )
        .unwrap();
        assert!(matches!(verdict, StartVerdict::Unknown { .. }));
        assert_eq!(transport.calls, 1);

        let stored = store.list_invocations("company-default").unwrap();
        assert_eq!(stored.len(), 1, "回包丢失不得新建 Attempt");
        assert_eq!(stored[0].state, ExecutionState::Unknown);
        assert_eq!(stored[0].execution_id, None);

        // 再次提交：能力不足，不重发。
        let again = submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            2_000,
        )
        .unwrap();
        assert!(matches!(again, StartVerdict::Unknown { .. }));
        assert_eq!(transport.calls, 1, "无幂等能力时不得重发 start");
        assert_eq!(store.list_invocations("company-default").unwrap().len(), 1);
        assert!(store.count_start_audits("inv-lost").unwrap() >= 2);
    }

    #[test]
    fn safe_resend_requires_declared_dedup_and_keeps_same_invocation() {
        let cfg = BodConfig::default();
        let mut store = SqliteStore::in_memory(&cfg).unwrap();
        let descriptor = idempotent_descriptor();
        let mut transport = StubTransport::new(
            descriptor,
            vec![
                StartOutcome::ReplyLost,
                StartOutcome::Replayed {
                    execution_id: "exec-9".to_string(),
                },
            ],
        );
        let request = request("inv-safe");

        let first = submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            1_000,
        )
        .unwrap();
        assert!(matches!(first, StartVerdict::Unknown { .. }));
        let second = submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            2_000,
        )
        .unwrap();
        assert_eq!(
            second,
            StartVerdict::Accepted {
                execution_id: "exec-9".to_string(),
                replayed: true
            }
        );
        assert_eq!(transport.calls, 2);
        let stored = store.list_invocations("company-default").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].execution_id.as_deref(), Some("exec-9"));
    }

    #[test]
    fn reconcile_marks_unknown_when_executor_cannot_confirm() {
        let cfg = BodConfig::default();
        let mut store = SqliteStore::in_memory(&cfg).unwrap();
        let descriptor = parse_descriptor(crate::descriptor_schema::descriptor_fixture()).unwrap();
        let mut transport = StubTransport::new(descriptor, vec![StartOutcome::ReplyLost]);
        let request = request("inv-reconcile");
        submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            1_000,
        )
        .unwrap();

        let state =
            reconcile_invocation(&mut store, &mut transport, "inv-reconcile", 2_000).unwrap();
        assert_eq!(state, Some(ExecutionState::Unknown));
        assert_eq!(
            store.list_invocations("company-default").unwrap()[0].state,
            ExecutionState::Unknown
        );
    }

    #[test]
    fn reconcile_applies_observed_running_state() {
        let cfg = BodConfig::default();
        let mut store = SqliteStore::in_memory(&cfg).unwrap();
        let descriptor = parse_descriptor(crate::descriptor_schema::descriptor_fixture()).unwrap();
        let mut transport = StubTransport::new(descriptor, vec![StartOutcome::ReplyLost]);
        transport.inspect_state = Some(ExecutionState::Running);
        let request = request("inv-running");
        submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            1_000,
        )
        .unwrap();

        let state = reconcile_invocation(&mut store, &mut transport, "inv-running", 2_000).unwrap();
        assert_eq!(state, Some(ExecutionState::Running));
    }

    #[test]
    fn late_detail_cannot_override_terminal_state() {
        let cfg = BodConfig::default();
        let mut store = SqliteStore::in_memory(&cfg).unwrap();
        let descriptor = parse_descriptor(crate::descriptor_schema::descriptor_fixture()).unwrap();
        let mut transport = StubTransport::new(descriptor, vec![accepted("exec-1")]);
        let request = request("inv-terminal");
        submit_start(
            &mut store,
            &mut transport,
            "company-default",
            &request,
            1_000,
        )
        .unwrap();

        advance_invocation_state(
            &mut store,
            "inv-terminal",
            ExecutionState::Completed,
            TransitionAuthority::Observed,
            2_000,
        )
        .unwrap();
        let update = advance_invocation_state(
            &mut store,
            "inv-terminal",
            ExecutionState::Running,
            TransitionAuthority::Observed,
            3_000,
        )
        .unwrap();
        assert_eq!(
            update,
            StateUpdate::Ignored {
                current: ExecutionState::Completed,
                attempted: ExecutionState::Running
            }
        );
        assert_eq!(
            store.list_invocations("company-default").unwrap()[0].state,
            ExecutionState::Completed
        );
    }
}
