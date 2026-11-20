//! 执行协议 conformance：命名用例 + 真实断言 + 如实的 not_applicable（P0 D4 任务 5.4）。
//!
//! 每个用例都驱动真实宿主代码（invocation 去重、观察 ACK、状态映射），断言写在用例里，
//! 不是"运行过就算通过"。适配器不具备故障注入能力时对应用例记 `not_applicable` 并写明
//! 原因，绝不记成通过。

use crate::config::BodConfig;
use crate::domain::execution::{
    AcceptanceSignal, CancelStage, EvidenceSource, ExecutionState, ObservationType, PROTOCOL_ID,
    ProbeStatus, StartRequest, TransitionAuthority, map_process_exit,
};
use crate::fake_executor::{
    ADAPTER_ID as FAKE_ADAPTER_ID, ADAPTER_VERSION as FAKE_VERSION, FakeExecutor, FakeScenario,
};
use crate::invocation::{
    InvocationError, StartTransport, StartVerdict, advance_invocation_state, reconcile_invocation,
    submit_start,
};
use crate::protocol_reliability::{
    MAX_OBSERVATION_PAYLOAD, pending_observations, receive_observation,
};
use crate::store::SqliteStore;
use std::fmt::Write as _;

/// 用例类别。身份、授权与消息完整性三类是**准入闸门**：
/// 任一失败即禁用该适配器执行（4.2 / `invariant_i_57`、G-17），不允许用 `degraded` 声明豁免。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseClass {
    Identity,
    Authorization,
    Integrity,
    Behaviour,
}

impl CaseClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Authorization => "authorization",
            Self::Integrity => "integrity",
            Self::Behaviour => "behaviour",
        }
    }

    /// 是否属于「失败即禁用」的闸门类别。
    pub const fn is_gating(self) -> bool {
        matches!(self, Self::Identity | Self::Authorization | Self::Integrity)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConformanceCase {
    pub id: &'static str,
    /// 对应的规格要求（spec delta 的 Requirement 名）。
    pub requirement: &'static str,
    pub scenario: FakeScenario,
    pub class: CaseClass,
    /// 是否必须由具备故障注入能力的适配器驱动。
    pub needs_fault_injection: bool,
}

const fn case(
    id: &'static str,
    requirement: &'static str,
    scenario: FakeScenario,
    class: CaseClass,
    needs_fault_injection: bool,
) -> ConformanceCase {
    ConformanceCase {
        id,
        requirement,
        scenario,
        class,
        needs_fault_injection,
    }
}

pub const CASES: [ConformanceCase; 14] = [
    case(
        "scope-identity",
        "必需用例：范围身份",
        FakeScenario::HappyPath,
        CaseClass::Identity,
        false,
    ),
    case(
        "duplicate-start",
        "Start 请求与持久去重",
        FakeScenario::HappyPath,
        CaseClass::Integrity,
        false,
    ),
    case(
        "params-conflict",
        "Start 请求与持久去重",
        FakeScenario::HappyPath,
        CaseClass::Integrity,
        false,
    ),
    case(
        "protocol-version",
        "必需用例：版本不匹配",
        FakeScenario::HappyPath,
        CaseClass::Integrity,
        false,
    ),
    case(
        "start-reply-lost",
        "回包丢失先对账",
        FakeScenario::ReplyLostOnce,
        CaseClass::Behaviour,
        true,
    ),
    case(
        "observation-out-of-order",
        "观察信封与缺口",
        FakeScenario::OutOfOrder,
        CaseClass::Behaviour,
        true,
    ),
    case(
        "observation-gap",
        "观察信封与缺口",
        FakeScenario::GapBeyondRetention,
        CaseClass::Behaviour,
        true,
    ),
    case(
        "process-exit-zero",
        "接收状态与终态边界",
        FakeScenario::ProcessExitZero,
        CaseClass::Behaviour,
        true,
    ),
    case(
        "cancel-request-vs-confirm",
        "接收状态与终态边界",
        FakeScenario::CancelUnconfirmed,
        CaseClass::Behaviour,
        false,
    ),
    // 授权闸门：驱动方式是「读探测的凭据面」而不是「注入一次凭据失效」。
    // 无凭据面的配置由用例自己判 not_applicable，并改用「授权约束仍生效」一组断言。
    case(
        "auth-expired",
        "必需操作与分项探测",
        FakeScenario::AuthExpired,
        CaseClass::Authorization,
        false,
    ),
    case(
        "huge-log",
        "观察信封与缺口",
        FakeScenario::HugeLog,
        CaseClass::Behaviour,
        true,
    ),
    case(
        "late-final-cost",
        "接收状态与终态边界",
        FakeScenario::LateCost,
        CaseClass::Behaviour,
        true,
    ),
    // 完整性闸门：凭据出现在**正常负载**里就能验，不需要故障注入；没有凭据的样本由用例判 NA。
    case(
        "sensitive-redaction",
        "必需用例：敏感字段过滤",
        FakeScenario::SensitivePayload,
        CaseClass::Integrity,
        false,
    ),
    case(
        "late-observation",
        "必需用例：迟到观察",
        FakeScenario::LateProgress,
        CaseClass::Behaviour,
        true,
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseStatus {
    Pass { assertions: usize },
    Fail { detail: String },
    NotApplicable { reason: String },
}

impl CaseStatus {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Pass { .. } => "pass",
            Self::Fail { .. } => "fail",
            Self::NotApplicable { .. } => "not_applicable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseOutcome {
    pub id: &'static str,
    pub requirement: &'static str,
    pub status: CaseStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    pub adapter_id: String,
    pub adapter_version: String,
    pub protocol: String,
    pub platform: String,
    pub generated_at_ms: i64,
    pub outcomes: Vec<CaseOutcome>,
}

impl ConformanceReport {
    /// (pass, fail, not_applicable)
    pub fn summary(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut fail = 0;
        let mut na = 0;
        for outcome in &self.outcomes {
            match outcome.status {
                CaseStatus::Pass { .. } => pass += 1,
                CaseStatus::Fail { .. } => fail += 1,
                CaseStatus::NotApplicable { .. } => na += 1,
            }
        }
        (pass, fail, na)
    }

    /// 没有 fail 才算通过；not_applicable 不算通过，但也不掩盖不支持项。
    pub fn passed(&self) -> bool {
        self.summary().1 == 0 && self.summary().0 > 0
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let (pass, fail, na) = self.summary();
        let _ = writeln!(out, "| 用例 | 要求 | 结果 | 断言/原因 |\n|---|---|---|---|");
        for outcome in &self.outcomes {
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
        let _ = writeln!(
            out,
            "\n适配器 {} v{}，协议 {}，平台 {}，pass={pass} fail={fail} not_applicable={na}",
            self.adapter_id, self.adapter_version, self.protocol, self.platform
        );
        out
    }
}

/// 稳定的 conformance 请求样本，避免每个用例各写一份。
pub fn conformance_request(invocation_id: &str) -> StartRequest {
    StartRequest {
        invocation_id: invocation_id.to_string(),
        run_id: "run-conformance".to_string(),
        work_package_revision: "wp-conformance-r1".to_string(),
        attempt_id: "attempt-conformance".to_string(),
        binding_id: "binding-conformance".to_string(),
        fencing_token: 7,
        agent_ref: "agent-conformance".to_string(),
        runtime_config_revision: "rt-conformance".to_string(),
        context_bundle_ref: "ctx-conformance".to_string(),
        workspace_ref: "ws-conformance".to_string(),
        grant_ref: "grant-conformance".to_string(),
        reservation_refs: vec!["res-conformance".to_string()],
        deadline_at_ms: None,
    }
}

fn store() -> Result<SqliteStore, String> {
    SqliteStore::in_memory(&BodConfig::default()).map_err(|err| err.to_string())
}

fn assert_true(condition: bool, message: &str) -> Result<usize, String> {
    if condition {
        Ok(1)
    } else {
        Err(message.to_string())
    }
}

/// 运行全套 conformance（Fake Executor 驱动，全部用例 applicable）。
pub fn run_fake_suite(now_ms: i64) -> ConformanceReport {
    let mut outcomes = Vec::with_capacity(CASES.len());
    for case in CASES {
        let mut transport = FakeExecutor::new(case.scenario);
        let status = run_case(case.id, &mut transport, now_ms);
        outcomes.push(CaseOutcome {
            id: case.id,
            requirement: case.requirement,
            status,
        });
    }
    ConformanceReport {
        adapter_id: FAKE_ADAPTER_ID.to_string(),
        adapter_version: FAKE_VERSION.to_string(),
        protocol: PROTOCOL_ID.to_string(),
        platform: "conformance".to_string(),
        generated_at_ms: now_ms,
        outcomes,
    }
}

/// 对真实适配器运行 applicable 的部分；故障注入类用例如实记 not_applicable。
pub fn run_live_suite(transport: &mut dyn StartTransport, now_ms: i64) -> ConformanceReport {
    let descriptor = transport.descriptor().clone();
    let injectable = transport.fault_injection();
    let mut outcomes = Vec::with_capacity(CASES.len());
    for case in CASES {
        let status = if case.needs_fault_injection && !injectable {
            CaseStatus::NotApplicable {
                reason: format!(
                    "适配器不具备故障注入能力，无法驱动 {} 场景",
                    case.scenario.as_str()
                ),
            }
        } else {
            run_case(case.id, transport, now_ms)
        };
        outcomes.push(CaseOutcome {
            id: case.id,
            requirement: case.requirement,
            status,
        });
    }
    ConformanceReport {
        adapter_id: descriptor.adapter_id,
        adapter_version: descriptor.adapter_version,
        protocol: descriptor.protocol,
        platform: descriptor.platform,
        generated_at_ms: now_ms,
        outcomes,
    }
}

fn run_case(id: &str, transport: &mut dyn StartTransport, now_ms: i64) -> CaseStatus {
    match dispatch_case(id, transport, now_ms) {
        Ok(assertions) => CaseStatus::Pass { assertions },
        Err(CaseFailure::Fail(detail)) => CaseStatus::Fail { detail },
        Err(CaseFailure::NotApplicable(reason)) => CaseStatus::NotApplicable { reason },
    }
}

/// 用例执行失败的两类原因。**分开是必须的**：把「本配置没有这个可测面」记成失败，
/// 会让适配器作者去改代码而不是补样本；记成通过则是自欺。
#[derive(Debug)]
enum CaseFailure {
    Fail(String),
    NotApplicable(String),
}

impl CaseFailure {
    fn fail(detail: impl ToString) -> Self {
        Self::Fail(detail.to_string())
    }

    fn not_applicable(reason: impl Into<String>) -> Self {
        Self::NotApplicable(reason.into())
    }
}

/// 断言失败（`assert_true` 返回字符串）默认就是 `Fail`：只有显式声明的能力缺失才走
/// `NotApplicable`，不允许用 `?` 悄悄把失败降级成不可用。
impl From<String> for CaseFailure {
    fn from(detail: String) -> Self {
        Self::Fail(detail)
    }
}

fn dispatch_case(
    id: &str,
    transport: &mut dyn StartTransport,
    now_ms: i64,
) -> Result<usize, CaseFailure> {
    match id {
        "duplicate-start" => case_duplicate_start(transport, now_ms).map_err(CaseFailure::Fail),
        "params-conflict" => case_params_conflict(transport, now_ms).map_err(CaseFailure::Fail),
        "start-reply-lost" => case_reply_lost(transport, now_ms).map_err(CaseFailure::Fail),
        "observation-out-of-order" => {
            case_out_of_order(transport, now_ms).map_err(CaseFailure::Fail)
        }
        "observation-gap" => case_gap(transport, now_ms).map_err(CaseFailure::Fail),
        "process-exit-zero" => case_process_exit(transport, now_ms).map_err(CaseFailure::Fail),
        "cancel-request-vs-confirm" => case_cancel(transport, now_ms).map_err(CaseFailure::Fail),
        "auth-expired" => case_auth_expired(transport, now_ms),
        "huge-log" => case_huge_log(transport, now_ms).map_err(CaseFailure::Fail),
        "late-final-cost" => case_late_cost(transport, now_ms).map_err(CaseFailure::Fail),
        "scope-identity" => case_scope_identity(transport, now_ms).map_err(CaseFailure::Fail),
        "protocol-version" => case_protocol_version(transport, now_ms).map_err(CaseFailure::Fail),
        "sensitive-redaction" => case_sensitive_redaction(transport, now_ms),
        "late-observation" => case_late_observation(transport, now_ms).map_err(CaseFailure::Fail),
        other => Err(CaseFailure::fail(format!(
            "未实现的 conformance 用例: {other}"
        ))),
    }
}

fn case_duplicate_start(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-dup");
    let first = submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(|err| err.to_string())?;
    let execution_id = match first {
        StartVerdict::Accepted {
            execution_id,
            replayed,
        } => {
            if replayed {
                return Err("首次 start 不应是重放".to_string());
            }
            execution_id
        }
        other => return Err(format!("首次 start 未受理: {other:?}")),
    };
    let second = submit_start(
        &mut store,
        transport,
        "company-default",
        &request,
        now_ms + 10,
    )
    .map_err(|err| err.to_string())?;

    let mut assertions = 0;
    assertions += assert_true(
        matches!(&second, StartVerdict::Accepted { execution_id: id, replayed: true } if id == &execution_id),
        "重复 start 必须返回原 execution_id 且标记 replayed",
    )?;
    let invocations = store
        .list_invocations("company-default")
        .map_err(|err| err.to_string())?;
    assertions += assert_true(
        invocations.len() == 1,
        "重复 start 不得创建第二条 invocation",
    )?;
    Ok(assertions)
}

fn case_params_conflict(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-conflict");
    submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(|err| err.to_string())?;

    let mut changed = request.clone();
    changed.fencing_token += 1;
    let verdict = submit_start(
        &mut store,
        transport,
        "company-default",
        &changed,
        now_ms + 10,
    )
    .map_err(|err| err.to_string())?;
    let mut assertions = 0;
    assertions += assert_true(
        matches!(verdict, StartVerdict::Rejected { code, .. } if code == "E_START_PARAMS_MISMATCH"),
        "同 ID 异参必须稳定拒绝",
    )?;
    assertions += assert_true(
        store
            .list_invocations("company-default")
            .map_err(|err| err.to_string())?
            .len()
            == 1,
        "异参拒绝后不得新增 invocation",
    )?;
    Ok(assertions)
}

fn case_reply_lost(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-lost");
    let first = submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(|err| err.to_string())?;
    let mut assertions = 0;
    assertions += assert_true(
        matches!(first, StartVerdict::Unknown { .. }),
        "丢回包必须进入 Unknown",
    )?;

    let record = store
        .list_invocations("company-default")
        .map_err(|err| err.to_string())?;
    assertions += assert_true(record.len() == 1, "丢回包不得新建 Attempt")?;

    // 对账：执行端若已有该执行，必须能查回来。
    let reconciled = reconcile_invocation(&mut store, transport, "inv-lost", now_ms + 20)
        .map_err(|err| err.to_string())?;
    assertions += assert_true(
        reconciled.is_some_and(|state| state != ExecutionState::Unknown),
        "执行端可查询时必须对账出真实状态，而不是停在 Unknown",
    )?;
    assertions += assert_true(
        store
            .list_invocations("company-default")
            .map_err(|err| err.to_string())?
            .len()
            == 1,
        "对账后仍然只有一条 invocation",
    )?;
    Ok(assertions)
}

fn collect_observations(
    store: &mut SqliteStore,
    transport: &mut dyn StartTransport,
    invocation_id: &str,
    retained_floor: Option<u64>,
    now_ms: i64,
) -> Result<Vec<(String, Option<u64>, bool)>, String> {
    let envelopes = transport.observations(invocation_id);
    if envelopes.is_empty() {
        return Err("适配器未提供观察，无法断言缺口行为".to_string());
    }
    let mut receipts = Vec::with_capacity(envelopes.len());
    for envelope in envelopes {
        let receipt = receive_observation(store, &envelope, retained_floor, now_ms)
            .map_err(|err| err.to_string())?;
        receipts.push((
            receipt.disposition.to_string(),
            receipt.ack_upto,
            receipt.persisted,
        ));
    }
    Ok(receipts)
}

fn case_out_of_order(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let receipts = collect_observations(&mut store, transport, "inv-order", None, now_ms)?;
    let mut assertions = 0;
    assertions += assert_true(receipts[0].1 == Some(1), "收到 1 后 ACK 应为 1")?;
    assertions += assert_true(
        receipts[1].0 == "gap_buffered" && receipts[1].1 == Some(1),
        "收到 3 但缺 2 时 ACK 必须停在 1",
    )?;
    assertions += assert_true(receipts[2].1 == Some(3), "补齐 2 后 ACK 连续推进到 3")?;
    Ok(assertions)
}

fn case_gap(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let envelopes = transport.observations("inv-gap");
    if envelopes.is_empty() {
        return Err("适配器未提供观察，无法断言断档行为".to_string());
    }
    let mut assertions = 0;
    for envelope in envelopes {
        let receipt = receive_observation(&mut store, &envelope, Some(100), now_ms)
            .map_err(|err| err.to_string())?;
        assertions += assert_true(
            receipt.disposition == "beyond_retention" && receipt.requires_snapshot,
            "超出保留窗口必须返回快照要求，不得静默跳到最新",
        )?;
        assertions += assert_true(!receipt.persisted, "窗口外观察不得当作连续序号落库")?;
    }
    Ok(assertions)
}

fn case_process_exit(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-exit");
    submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(|err| err.to_string())?;
    let exit = transport.observations("inv-exit").len();
    // 执行端提交了候选成果，且进程退出码为 0。
    let mapped = map_process_exit(0);
    let mut assertions = 0;
    assertions += assert_true(exit > 0, "应产生至少一条候选成果观察")?;
    assertions += assert_true(
        mapped.state == ExecutionState::Completed,
        "退出码 0 只映射为 Completed",
    )?;
    assertions += assert_true(
        mapped.acceptance == AcceptanceSignal::PendingVerification,
        "退出码 0 不得产生验收通过信号",
    )?;
    // 业务接受与执行完成分离：Completed 不是 Accepted。
    assertions += assert_true(
        ExecutionState::Completed != ExecutionState::Accepted,
        "Completed 与 Accepted 必须是不同状态",
    )?;
    let update = advance_invocation_state(
        &mut store,
        "inv-exit",
        mapped.state,
        TransitionAuthority::Observed,
        now_ms + 1,
    )
    .map_err(|err| err.to_string())?;
    assertions += assert_true(
        matches!(
            update,
            crate::invocation::StateUpdate::Applied(ExecutionState::Completed)
        ),
        "宿主把执行记为 Completed",
    )?;
    Ok(assertions)
}

fn case_cancel(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-cancel");
    submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(|err| err.to_string())?;
    let stage = transport.cancel("inv-cancel");
    let mut assertions = 0;
    assertions += assert_true(
        stage == CancelStage::Requested || stage == CancelStage::Confirmed,
        "取消结果只能是受理或确认",
    )?;
    if stage == CancelStage::Requested {
        // 只受理：不得释放占用，也不得把状态写成 Cancelled。
        let states = transport.inspect("inv-cancel");
        assertions += assert_true(
            states.is_some_and(|state| state != ExecutionState::Cancelled),
            "未确认停止时不得把状态写成 Cancelled",
        )?;
    } else {
        assertions += assert_true(stage.stop_confirmed(), "确认停止后 stop_confirmed 必须为真")?;
    }
    assertions += assert_true(!CancelStage::Requested.stop_confirmed(), "受理不等于停止")?;
    let _ = &mut store;
    Ok(assertions)
}

/// 授权闸门：按适配器**声明的凭据面**分支，两条分支都要有实质断言。
///
/// 这条用例的原始形态要求「凭据失效时探测不得报就绪」，隐含假设适配器有凭据。
/// 真实进程型适配器没有凭据概念（`probe` 如实报 `unsupported`），此时把用例记成失败
/// 是在指责适配器有缺陷；记成通过则等于宣称验证过授权。正确做法是换一组断言：
/// 凭据面不存在时，验的是「不得冒充验证过凭据」以及「授权约束仍然生效」。
fn case_auth_expired(
    transport: &mut dyn StartTransport,
    now_ms: i64,
) -> Result<usize, CaseFailure> {
    let Some(report) = transport.probe(now_ms) else {
        return Err(CaseFailure::not_applicable(
            "适配器未提供分项探测：无法断言凭据诊断，也无法确认授权面相貌",
        ));
    };
    let credentials = report.item(crate::domain::execution::ProbeAspect::Credentials);
    let Some(credentials) = credentials else {
        return Err(CaseFailure::not_applicable(
            "分项探测未覆盖凭据项：视为本配置没有凭据面",
        ));
    };

    match credentials.status {
        ProbeStatus::NotReady => {
            // 有凭据面且已失效：原来的四条断言一字不动。
            let mut assertions = 0;
            assertions += assert_true(
                report.overall() != ProbeStatus::Ready,
                "凭据失效时整体探测不得报就绪",
            )?;
            assertions += assert_true(
                report
                    .item(crate::domain::execution::ProbeAspect::Model)
                    .is_some_and(|item| item.status != ProbeStatus::Ready),
                "不得用付费模型探测冒充就绪",
            )?;
            assertions += assert_true(
                report
                    .items
                    .iter()
                    .all(|item| item.source != EvidenceSource::ManualOverride),
                "探测结论必须来自探测本身，不是人工覆盖",
            )?;
            assertions += assert_true(
                !credentials.reason.trim().is_empty(),
                "凭据项必须给出失效理由",
            )?;
            Ok(assertions)
        }
        ProbeStatus::Unsupported => {
            // 无凭据面：不得把「没有这一面」读成「这一面已验证」，且授权约束必须在别处成立。
            let mut assertions = 0;
            assertions += assert_true(
                !credentials.reason.trim().is_empty(),
                "声明凭据不适用必须给理由，不能空口 unsupported",
            )?;
            assertions += assert_true(
                report
                    .item(crate::domain::execution::ProbeAspect::Model)
                    .is_some_and(|item| item.status != ProbeStatus::Ready),
                "不得用付费模型探测冒充就绪",
            )?;
            assertions += assert_true(
                report
                    .items
                    .iter()
                    .all(|item| item.source != EvidenceSource::ManualOverride),
                "探测结论必须来自探测本身，不是人工覆盖",
            )?;
            assertions += assert_true(
                report
                    .items
                    .iter()
                    .any(|item| item.status == ProbeStatus::Unsupported),
                "既然凭据项不适用，探测结果里就该看得到不适用项，而不是一片 ready",
            )?;
            // 授权约束不由凭据面提供，但必须仍然生效：跨公司同 ID 提交要被拒。
            let mut store = store().map_err(CaseFailure::Fail)?;
            let request = conformance_request("inv-auth-scope");
            let accepted = submit_start(&mut store, transport, "company-a", &request, now_ms)
                .map_err(CaseFailure::fail)?;
            assertions += assert_true(
                matches!(accepted, StartVerdict::Accepted { .. }),
                "无凭据面时首次提交仍应被受理（授权不等于拒绝一切）",
            )?;
            let crossed = submit_start(&mut store, transport, "company-b", &request, now_ms + 10)
                .map_err(CaseFailure::fail)?;
            match crossed {
                StartVerdict::Rejected { code, .. } => {
                    assertions += assert_true(
                        code == "E_SCOPE_MISMATCH",
                        &format!("无凭据面不能豁免授权：跨公司应报 E_SCOPE_MISMATCH，实际 {code}"),
                    )?
                }
                other => {
                    return Err(CaseFailure::fail(format!(
                        "无凭据面时跨公司同 ID 未被拒绝: {other:?}"
                    )));
                }
            }
            Ok(assertions)
        }
        ProbeStatus::Ready => Err(CaseFailure::not_applicable(
            "本配置凭据可用：无法驱动失效场景，该路径需由具备凭据注入能力的配置覆盖",
        )),
        ProbeStatus::Unknown => Err(CaseFailure::fail(
            "凭据项报 unknown：未知不等于不适用，不能据此判定授权面相貌",
        )),
    }
}

fn case_huge_log(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let envelopes = transport.observations("inv-huge");
    let envelope = envelopes
        .into_iter()
        .find(|item| item.kind == ObservationType::Log)
        .ok_or_else(|| "适配器未提供超大日志观察".to_string())?;
    let original_len = envelope.payload.len();
    let receipt =
        receive_observation(&mut store, &envelope, None, now_ms).map_err(|err| err.to_string())?;
    let stored = pending_observations(&mut store, "inv-huge").map_err(|err| err.to_string())?;
    let mut assertions = 0;
    assertions += assert_true(
        receipt.truncated_from == Some(original_len),
        "超大日志必须如实记录被截断的原始长度",
    )?;
    assertions += assert_true(
        stored
            .first()
            .is_some_and(|item| item.payload.len() <= MAX_OBSERVATION_PAYLOAD),
        "落库内容不得超过单条上限",
    )?;
    assertions += assert_true(receipt.ack_upto == Some(1), "截断不影响持久接收的 ACK")?;
    Ok(assertions)
}

fn case_late_cost(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-cost");
    submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(|err| err.to_string())?;
    let envelopes = transport.observations("inv-cost");
    if envelopes.len() < 2 {
        return Err("适配器未提供最终费用观察".to_string());
    }
    // 先落到终态，再接收迟到的费用观察。
    advance_invocation_state(
        &mut store,
        "inv-cost",
        ExecutionState::Completed,
        TransitionAuthority::Observed,
        now_ms + 1,
    )
    .map_err(|err| err.to_string())?;
    let mut assertions = 0;
    for envelope in envelopes {
        let receipt = receive_observation(&mut store, &envelope, None, now_ms + 2)
            .map_err(|err| err.to_string())?;
        assertions += assert_true(receipt.persisted, "终态后的观察仍要持久接收，不得丢弃")?;
    }
    let invocations = store
        .list_invocations("company-default")
        .map_err(|err| err.to_string())?;
    assertions += assert_true(
        invocations[0].state == ExecutionState::Completed,
        "迟到费用观察不得推翻权威终态",
    )?;
    Ok(assertions)
}

// ---------------------------------------------------------------------------
// 4.2 新增的四个必需用例
// ---------------------------------------------------------------------------

/// 范围身份：`invocation_id` 全局唯一，但归属只属于第一次受理它的公司。
/// 另一家公司拿同一个 ID 重发不是重放，必须在同参判定之前被拒。
fn case_scope_identity(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-scope");
    let first = submit_start(&mut store, transport, "company-a", &request, now_ms)
        .map_err(|err| err.to_string())?;
    let mut assertions = 0;
    assertions += assert_true(
        matches!(first, StartVerdict::Accepted { .. }),
        "首次提交必须被受理",
    )?;

    let second = submit_start(&mut store, transport, "company-b", &request, now_ms + 10)
        .map_err(|err| err.to_string())?;
    match second {
        StartVerdict::Rejected { code, reason } => {
            assertions += assert_true(
                code == "E_SCOPE_MISMATCH",
                &format!("跨公司重发应报 E_SCOPE_MISMATCH，实际 {code}"),
            )?;
            assertions += assert_true(reason.contains("company-a"), "拒绝理由必须点明原归属公司")?;
        }
        other => return Err(format!("跨公司同 ID 提交未被拒绝: {other:?}")),
    }
    assertions += assert_true(
        store
            .list_invocations("company-a")
            .map_err(|err| err.to_string())?
            .len()
            == 1,
        "原公司仍只有一条 invocation",
    )?;
    assertions += assert_true(
        store
            .list_invocations("company-b")
            .map_err(|err| err.to_string())?
            .is_empty(),
        "另一家公司不得因同 ID 获得 invocation 记录",
    )?;
    Ok(assertions)
}

/// 版本不匹配：描述符里的协议版本必须与宿主支持的版本一致，且适配器版本可读。
/// 不一致属 Integrity 类失败 —— 适配器被禁用，而不是「试着跑跑看」。
fn case_protocol_version(
    transport: &mut dyn StartTransport,
    _now_ms: i64,
) -> Result<usize, String> {
    let descriptor = transport.descriptor();
    let mut assertions = 0;
    assertions += assert_true(
        descriptor.protocol == PROTOCOL_ID,
        &format!(
            "适配器协议 {} 与宿主 {} 不一致；版本不匹配必须禁用该适配器",
            descriptor.protocol, PROTOCOL_ID
        ),
    )?;
    assertions += assert_true(
        !descriptor.adapter_version.trim().is_empty(),
        "适配器版本为空时无法核对报告与支持矩阵",
    )?;
    Ok(assertions)
}

/// 敏感字段过滤：观察里的凭据形态内容必须在落库前被替换，且留下脱敏标记。
/// 敏感字段过滤：**用真实负载驱动**，不靠故障注入。
///
/// 三个结论分支，刻意分开：
/// 1. 本配置没有产生任何观察 → 无法验证，记 `not_applicable`（能力/样本缺失，不是缺陷）；
/// 2. 观察里没有凭据形态 → 同样记 `not_applicable`，但理由必须点明「样本不含凭据」，
///    否则适配器只要永远不打印秘密就能免检；
/// 3. 观察里出现凭据 → 必须**全部**被脱敏，落库内容不得保留原文，否则记 `fail`。
///
/// 第 3 支的断言从「至少 3 处」改成「脱敏处数等于原始命中处数」，因为样本形态由配置决定，
/// 写死 3 只会让不同配置得到不同含义的「通过」。
fn case_sensitive_redaction(
    transport: &mut dyn StartTransport,
    now_ms: i64,
) -> Result<usize, CaseFailure> {
    let mut store = store().map_err(CaseFailure::Fail)?;
    let request = conformance_request(REDACTION_SAMPLE_INVOCATION);
    submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(CaseFailure::fail)?;

    // 真实进程的输出是异步的：给它一段时间，但不无限等。
    let envelopes = await_observations(transport, REDACTION_SAMPLE_INVOCATION, OBSERVE_WAIT);
    if envelopes.is_empty() {
        return Err(CaseFailure::not_applicable(format!(
            "本配置在 {} 秒内未产生观察样本，敏感字段过滤无从验证",
            OBSERVE_WAIT.as_secs()
        )));
    }

    let raw = envelopes
        .iter()
        .map(|envelope| envelope.payload.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let (_, detected) = crate::observation_redaction::redact(&raw);
    if detected.replaced == 0 {
        // 结束前收掉为取样启动的进程，别把清理留给调用方。
        let _ = transport.cancel(REDACTION_SAMPLE_INVOCATION);
        return Err(CaseFailure::not_applicable(format!(
            "本配置的负载不含凭据形态（扫描 {} 行，命中 0 处）：脱敏管道本身由 \
             observation_redaction 单测覆盖，此处无法取得适配器侧样本",
            envelopes.len()
        )));
    }

    let mut assertions = 0;
    let mut total_redactions = 0usize;
    for envelope in envelopes {
        let receipt =
            receive_observation(&mut store, &envelope, None, now_ms).map_err(CaseFailure::fail)?;
        total_redactions += receipt.redactions;
    }
    assertions += assert_true(
        total_redactions == detected.replaced,
        &format!(
            "落库前脱敏处数应等于原始命中处数：原始 {} 处，实际 {} 处",
            detected.replaced, total_redactions
        ),
    )?;
    let stored =
        pending_observations(&mut store, REDACTION_SAMPLE_INVOCATION).map_err(CaseFailure::fail)?;
    let joined = stored
        .iter()
        .map(|item| item.payload.clone())
        .collect::<Vec<_>>()
        .join("\n");
    // 已知测试样本里的秘密必须彻底不在落库内容里（按样本里实际出现的那些检查）。
    for secret in ["sk-live-abcdef0123456789", "abcd1234efgh5678", "hunter2"] {
        if raw.contains(secret) {
            assertions += assert_true(
                !joined.contains(secret),
                &format!("落库内容仍含凭据片段 {secret}"),
            )?;
        }
    }
    assertions += assert_true(
        joined.contains(crate::observation_redaction::REDACTED_PREFIX),
        "应留下脱敏标记，让读者知道这里本来有秘密",
    )?;
    let _ = transport.cancel(REDACTION_SAMPLE_INVOCATION);
    Ok(assertions)
}

/// 取样用的 invocation id 与等待窗口；两处引用同一常量，避免用例与清理各写一份。
const REDACTION_SAMPLE_INVOCATION: &str = "inv-redact";
const OBSERVE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// 轮询取观察，直到非空或超时。真实进程异步输出，取一次就断言会把「还没写」误判成「没有」。
fn await_observations(
    transport: &mut dyn StartTransport,
    invocation_id: &str,
    timeout: std::time::Duration,
) -> Vec<crate::domain::execution::ObservationEnvelope> {
    let step = std::time::Duration::from_millis(20);
    let mut waited = std::time::Duration::ZERO;
    loop {
        let envelopes = transport.observations(invocation_id);
        if !envelopes.is_empty() || waited >= timeout {
            return envelopes;
        }
        std::thread::sleep(step);
        waited += step;
    }
}

/// 迟到观察：终态之后到达的**普通**观察仍要持久接收，且不得推翻权威终态。
/// 与 `late-final-cost` 的区别是这里不含费用，验的是「终态之后的一切观察」。
fn case_late_observation(transport: &mut dyn StartTransport, now_ms: i64) -> Result<usize, String> {
    let mut store = store()?;
    let request = conformance_request("inv-late");
    submit_start(&mut store, transport, "company-default", &request, now_ms)
        .map_err(|err| err.to_string())?;
    let envelopes = transport.observations("inv-late");
    if envelopes.len() < 2 {
        return Err("适配器未提供终态后的观察样本".to_string());
    }
    advance_invocation_state(
        &mut store,
        "inv-late",
        ExecutionState::Completed,
        TransitionAuthority::Observed,
        now_ms + 1,
    )
    .map_err(|err| err.to_string())?;

    let mut assertions = 0;
    for envelope in envelopes {
        let receipt = receive_observation(&mut store, &envelope, None, now_ms + 2)
            .map_err(|err| err.to_string())?;
        assertions += assert_true(receipt.persisted, "终态后到达的观察仍要持久接收")?;
    }
    let invocations = store
        .list_invocations("company-default")
        .map_err(|err| err.to_string())?;
    assertions += assert_true(
        invocations[0].state == ExecutionState::Completed,
        "迟到观察不得推翻权威终态",
    )?;
    Ok(assertions)
}

// ---------------------------------------------------------------------------
// 4.2 准入判定：闸门类用例失败即禁用
// ---------------------------------------------------------------------------

/// 适配器启用判定。
///
/// 只看**用例结果**，不看适配器自报的 `limitations`：声明 `degraded` 不能豁免，
/// 因为 `enablement` 的入参里根本没有描述符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enablement {
    /// 闸门类用例全部通过。
    Enabled,
    /// 没有失败，但闸门类有 not_applicable —— 未取得证据，不得标 Supported。
    Unverified { cases: Vec<&'static str> },
    /// 闸门类有用例失败：禁用执行。
    Blocked {
        failures: Vec<(&'static str, String)>,
    },
}

impl Enablement {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Unverified { .. } => "unverified",
            Self::Blocked { .. } => "blocked",
        }
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    /// 是否允许在「探测/试用」范围内使用（未验证可以试用，被禁用不行）。
    pub fn may_probe(&self) -> bool {
        !self.is_blocked()
    }
}

/// 对一份 conformance 报告做准入判定。
pub fn enablement(report: &ConformanceReport) -> Enablement {
    let mut failures: Vec<(&'static str, String)> = Vec::new();
    let mut unverified: Vec<&'static str> = Vec::new();
    for outcome in &report.outcomes {
        let Some(case) = CASES.iter().find(|case| case.id == outcome.id) else {
            continue;
        };
        if !case.class.is_gating() {
            continue;
        }
        match &outcome.status {
            CaseStatus::Pass { .. } => {}
            CaseStatus::Fail { detail } => failures.push((outcome.id, detail.clone())),
            CaseStatus::NotApplicable { .. } => unverified.push(outcome.id),
        }
    }
    if !failures.is_empty() {
        return Enablement::Blocked { failures };
    }
    if !unverified.is_empty() {
        return Enablement::Unverified { cases: unverified };
    }
    Enablement::Enabled
}

/// 便捷入口：对 Fake Executor 跑全套，并断言 Fake 不产生生产能力声明。
pub fn verify_fake_suite(now_ms: i64) -> Result<ConformanceReport, String> {
    let report = run_fake_suite(now_ms);
    if !report.passed() {
        return Err(format!("conformance 未全部通过:\n{}", report.to_markdown()));
    }
    if FakeExecutor::is_production_capable() {
        return Err("Fake Executor 不得声明生产能力".to_string());
    }
    let fake_store = store()?;
    if !fake_store
        .list_support_matrix(FAKE_ADAPTER_ID, FAKE_VERSION)
        .map_err(|err| err.to_string())?
        .is_empty()
    {
        return Err("Fake Executor 不得写入支持矩阵".to_string());
    }
    Ok(report)
}

/// 供 D5/D6 复用的错误映射：把 InvocationError 归为可展示类别。
pub const fn classify_invocation_error(err: &InvocationError) -> &'static str {
    match err {
        InvocationError::Store(_) => "storage",
        InvocationError::Contract(_) => "contract",
        InvocationError::UnknownInvocation(_) => "unknown_invocation",
    }
}

/// 报告附带的证据说明：哪些用例来自真实断言、哪些被如实标为不适用。
pub fn evidence_notes() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "conformance",
            "每个用例都有真实断言，not_applicable 不计入通过数",
        ),
        ("fake-executor", FakeExecutor::production_exclusion_reason()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::execution::ProbeReport;

    #[test]
    fn fake_suite_passes_every_named_case() {
        let report = verify_fake_suite(1_763_500_000_000).expect("conformance 应全部通过");
        let (pass, fail, na) = report.summary();
        assert_eq!(fail, 0, "不应有失败用例: {}", report.to_markdown());
        assert_eq!(na, 0, "Fake Executor 支持故障注入，不应有 not_applicable");
        assert_eq!(pass, CASES.len());
        // 每个用例至少一条断言，避免空跑。
        for outcome in &report.outcomes {
            match outcome.status {
                CaseStatus::Pass { assertions } => assert!(assertions > 0),
                _ => panic!("用例 {} 未通过", outcome.id),
            }
        }
    }

    #[test]
    fn report_lists_every_case_with_requirement() {
        let report = run_fake_suite(1_763_500_000_000);
        let markdown = report.to_markdown();
        for case in CASES {
            assert!(markdown.contains(case.id), "报告缺少用例 {}", case.id);
            assert!(
                markdown.contains(case.requirement),
                "报告缺少要求 {}",
                case.requirement
            );
        }
        assert!(markdown.contains("not_applicable=0"));
    }

    #[test]
    fn live_suite_marks_fault_injection_cases_not_applicable() {
        // 用一个不具备故障注入能力的适配器：除故障类外其余用例仍要真跑。
        struct Plain(FakeExecutor);
        impl StartTransport for Plain {
            fn descriptor(&self) -> &crate::domain::execution::AdapterDescriptor {
                self.0.descriptor()
            }
            fn start(&mut self, request: &StartRequest) -> crate::invocation::StartOutcome {
                self.0.start(request)
            }
            fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState> {
                self.0.inspect(invocation_id)
            }
        }
        let mut transport = Plain(FakeExecutor::new(FakeScenario::HappyPath));
        let report = run_live_suite(&mut transport, 1_763_500_000_000);
        let (pass, fail, na) = report.summary();
        assert_eq!(fail, 0, "可适用用例不得失败: {}", report.to_markdown());
        assert!(pass >= 3, "至少应有 3 个可适用用例真跑");
        assert_eq!(na, 9, "9 个故障注入用例应如实标 not_applicable");
        for outcome in &report.outcomes {
            if case_needs_fault(outcome.id) {
                assert!(matches!(outcome.status, CaseStatus::NotApplicable { .. }));
            }
        }
    }

    /// 只覆盖 `probe` 的适配器：用于逐条验证授权闸门的分支，不引入真实进程。
    struct ProbeTransport {
        inner: FakeExecutor,
        probe: Option<ProbeReport>,
    }

    impl StartTransport for ProbeTransport {
        fn descriptor(&self) -> &crate::domain::execution::AdapterDescriptor {
            self.inner.descriptor()
        }
        fn start(&mut self, request: &StartRequest) -> crate::invocation::StartOutcome {
            self.inner.start(request)
        }
        fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState> {
            self.inner.inspect(invocation_id)
        }
        fn probe(&self, _now_ms: i64) -> Option<ProbeReport> {
            self.probe.clone()
        }
    }

    const GATE_NOW: i64 = 1_763_500_000_000;

    fn probe_report(credentials: ProbeStatus, reason: &str) -> ProbeReport {
        use crate::domain::execution::{ProbeAspect, ProbeItem};
        let item = |aspect, status, reason: &str| ProbeItem {
            aspect,
            status,
            reason: reason.to_string(),
            source: EvidenceSource::PlatformProbe,
            scope: "conformance/test".to_string(),
            observed_at_ms: GATE_NOW,
            expires_at_ms: Some(GATE_NOW + 1_000),
        };
        ProbeReport {
            adapter_id: "adapter.test".to_string(),
            adapter_version: "0.0.0".to_string(),
            items: vec![
                item(ProbeAspect::Environment, ProbeStatus::Ready, "可执行"),
                item(ProbeAspect::Credentials, credentials, reason),
                item(ProbeAspect::Model, ProbeStatus::Unsupported, "不调用模型"),
            ],
            generated_at_ms: GATE_NOW,
        }
    }

    fn probe_transport(credentials: ProbeStatus, reason: &str) -> ProbeTransport {
        ProbeTransport {
            inner: FakeExecutor::new(FakeScenario::HappyPath),
            probe: Some(probe_report(credentials, reason)),
        }
    }

    /// 授权闸门按**声明的凭据面**分支：四种声明各有确定结论，不能一律当失败。
    #[test]
    fn credential_gate_branches_on_declared_credential_face() {
        // 有凭据面且已失效：走原断言。
        let mut expired = probe_transport(ProbeStatus::NotReady, "api key 已过期");
        assert!(
            case_auth_expired(&mut expired, GATE_NOW).is_ok(),
            "凭据失效场景应通过"
        );

        // 声明不适用但不说理由：不适用本身也要有依据。
        let mut silent = probe_transport(ProbeStatus::Unsupported, "   ");
        match case_auth_expired(&mut silent, GATE_NOW) {
            Err(CaseFailure::Fail(detail)) => {
                assert!(detail.contains("必须给理由"), "实际 {detail}")
            }
            other => panic!("空口 unsupported 应判失败，实际 {other:?}"),
        }

        // 声明不适用且说清理由：通过，但授权约束仍要生效（跨公司必须被拒）。
        let mut no_creds = probe_transport(ProbeStatus::Unsupported, "本机进程不读取凭据");
        assert!(
            case_auth_expired(&mut no_creds, GATE_NOW).is_ok(),
            "无凭据面时授权约束仍应可验证"
        );

        // 凭据可用：本配置驱动不了失效场景，这是样本缺口而不是缺陷。
        let mut ready = probe_transport(ProbeStatus::Ready, "凭据有效");
        match case_auth_expired(&mut ready, GATE_NOW) {
            Err(CaseFailure::NotApplicable(reason)) => {
                assert!(reason.contains("凭据可用"), "实际 {reason}")
            }
            other => panic!("凭据可用应判不适用，实际 {other:?}"),
        }

        // unknown 不等于不适用：未知不能拿来放行。
        let mut unknown = probe_transport(ProbeStatus::Unknown, "探测超时");
        assert!(matches!(
            case_auth_expired(&mut unknown, GATE_NOW),
            Err(CaseFailure::Fail(_))
        ));

        // 完全没有探测能力：能力缺失，不是缺陷。
        let mut no_probe = ProbeTransport {
            inner: FakeExecutor::new(FakeScenario::HappyPath),
            probe: None,
        };
        assert!(matches!(
            case_auth_expired(&mut no_probe, GATE_NOW),
            Err(CaseFailure::NotApplicable(_))
        ));
    }

    /// 有观察但不含凭据的适配器：过滤无从验证，必须记不适用且点明「样本不含凭据」。
    struct ChattyWithoutSecrets {
        inner: FakeExecutor,
        served: bool,
    }

    impl StartTransport for ChattyWithoutSecrets {
        fn descriptor(&self) -> &crate::domain::execution::AdapterDescriptor {
            self.inner.descriptor()
        }
        fn start(&mut self, request: &StartRequest) -> crate::invocation::StartOutcome {
            self.inner.start(request)
        }
        fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState> {
            self.inner.inspect(invocation_id)
        }
        fn observations(
            &mut self,
            invocation_id: &str,
        ) -> Vec<crate::domain::execution::ObservationEnvelope> {
            if self.served {
                return Vec::new();
            }
            self.served = true;
            vec![crate::domain::execution::ObservationEnvelope {
                invocation_id: invocation_id.to_string(),
                source_epoch: 1,
                stream_id: "log".to_string(),
                source_sequence: 1,
                observed_at_ms: GATE_NOW,
                kind: ObservationType::Log,
                payload: "build finished in 3s".to_string(),
                origin: "adapter.test".to_string(),
            }]
        }
    }

    #[test]
    fn redaction_gate_without_a_credential_sample_is_not_applicable() {
        let mut transport = ChattyWithoutSecrets {
            inner: FakeExecutor::new(FakeScenario::HappyPath),
            served: false,
        };
        match case_sensitive_redaction(&mut transport, GATE_NOW) {
            Err(CaseFailure::NotApplicable(reason)) => {
                assert!(reason.contains("不含凭据形态"), "实际 {reason}");
                assert!(reason.contains("命中 0 处"), "必须写明扫了多少、命中多少");
            }
            other => panic!("无凭据样本应判不适用，实际 {other:?}"),
        }
    }

    #[test]
    fn redaction_gate_requires_every_hit_to_be_redacted() {
        // Fake 的 SensitivePayload 提供三类凭据：命中几处就必须脱敏几处。
        let mut transport = FakeExecutor::new(FakeScenario::SensitivePayload);
        let assertions = case_sensitive_redaction(&mut transport, GATE_NOW).expect("应通过");
        assert!(assertions >= 3, "至少三类命中各一条断言，实际 {assertions}");
    }

    /// 端到端：真实进程 + 含凭据的**正常**负载，闸门类三条全过，准入为 enabled。
    ///
    /// 这条测试是「闸门冲突已解」的证据：此前 `auth-expired` 与 `sensitive-redaction`
    /// 因为要求故障注入，对任何真实适配器恒为 not_applicable，导致 select() 永远过滤真实候选。
    #[test]
    fn real_adapter_with_credential_payload_enables_every_gating_case() {
        let Some(mut adapter) = crate::local_process::default_adapter(GATE_NOW) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        adapter.use_conformance_payload(GATE_NOW);
        let report = run_live_suite(&mut adapter, GATE_NOW);
        let (pass, fail, na) = report.summary();
        assert_eq!(
            fail,
            0,
            "真实适配器不得有用例失败：\n{}",
            report.to_markdown()
        );
        assert_eq!(
            na,
            7,
            "只有真正需要故障注入的七条应记不适用：\n{}",
            report.to_markdown()
        );
        assert!(pass >= 7, "实际 pass={pass}：\n{}", report.to_markdown());
        assert_eq!(
            enablement(&report),
            Enablement::Enabled,
            "闸门类全部有证据时应可准入：\n{}",
            report.to_markdown()
        );
    }

    fn case_needs_fault(id: &str) -> bool {
        CASES
            .iter()
            .find(|case| case.id == id)
            .is_some_and(|case| case.needs_fault_injection)
    }

    #[test]
    fn gating_failure_blocks_enablement_even_when_adapter_claims_degraded() {
        let mut report = run_fake_suite(1_763_500_000_000);
        assert_eq!(enablement(&report), Enablement::Enabled);

        let outcome = report
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.id == "scope-identity")
            .expect("身份用例应存在");
        outcome.status = CaseStatus::Fail {
            detail: "跨公司同 ID 未被拒绝".to_string(),
        };

        // `enablement` 的入参只有报告：适配器把 limitations 写成 degraded 也无处生效。
        let verdict = enablement(&report);
        match &verdict {
            Enablement::Blocked { failures } => {
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].0, "scope-identity");
                assert!(failures[0].1.contains("跨公司"));
            }
            other => panic!("闸门类失败必须禁用，实际 {other:?}"),
        }
        assert!(!verdict.may_probe());
    }

    #[test]
    fn behaviour_failure_does_not_block_by_itself() {
        let mut report = run_fake_suite(1_763_500_000_000);
        let outcome = report
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.id == "huge-log")
            .expect("用例应存在");
        outcome.status = CaseStatus::Fail {
            detail: "截断长度未记录".to_string(),
        };
        assert_eq!(enablement(&report), Enablement::Enabled);
    }

    #[test]
    fn gating_not_applicable_is_unverified_not_enabled() {
        struct Plain(FakeExecutor);
        impl StartTransport for Plain {
            fn descriptor(&self) -> &crate::domain::execution::AdapterDescriptor {
                self.0.descriptor()
            }
            fn start(&mut self, request: &StartRequest) -> crate::invocation::StartOutcome {
                self.0.start(request)
            }
            fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState> {
                self.0.inspect(invocation_id)
            }
        }
        let mut transport = Plain(FakeExecutor::new(FakeScenario::HappyPath));
        let report = run_live_suite(&mut transport, 1_763_500_000_000);
        match enablement(&report) {
            Enablement::Unverified { cases } => {
                // 没有故障注入就驱动不了凭据失效场景：授权类拿不到证据。
                assert!(cases.contains(&"auth-expired"), "实际 {cases:?}");
            }
            other => panic!("未取得授权证据应为 unverified，实际 {other:?}"),
        }
    }

    #[test]
    fn gating_failure_takes_precedence_over_not_applicable() {
        let mut report = run_fake_suite(1_763_500_000_000);
        let auth = report
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.id == "auth-expired")
            .expect("用例应存在");
        auth.status = CaseStatus::NotApplicable {
            reason: "无注入".to_string(),
        };
        let scope = report
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.id == "scope-identity")
            .expect("用例应存在");
        scope.status = CaseStatus::Fail {
            detail: "串号".to_string(),
        };
        assert!(enablement(&report).is_blocked(), "失败优先于未测");
    }

    #[test]
    fn fake_executor_is_excluded_from_support_matrix() {
        let store = store().unwrap();
        assert!(
            store
                .list_support_matrix(FAKE_ADAPTER_ID, FAKE_VERSION)
                .unwrap()
                .is_empty()
        );
        assert!(!FakeExecutor::is_production_capable());
        assert!(FakeExecutor::production_exclusion_reason().contains("不提供生产能力"));
    }
}
