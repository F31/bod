//! P1 切片 4.3：取消请求/确认停止分离与 Unknown 对账入口。
//!
//! 三条硬顺序：
//! 1. `cancel_ack` 只证明取消请求被收到，**不是停止确认**：不改 Attempt、不释放风险占用；
//! 2. 只有带外部证据的 `confirm_stop` 才能把停止记成已确认，且证据绑定调用/attempt；
//! 3. 回包丢失或证据不足一律显示 Unknown/待核实与处理人（I-11），不新建 Attempt 规避未知。
//!
//! 本模块复用 P0 的 events 与 reconciliation_cases，不建第二个业务库。

use crate::domain::budget::ReservationState;
use crate::domain::execution::ExecutionState;
use crate::lifecycle_recovery::{CaseError, CaseRequest, open_case, resolve_case};
use crate::store::{SqliteStore, StoreError};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const CANCEL_PREFIX: &str = "cancel:";

#[derive(Debug)]
pub enum CancelError {
    Store(StoreError),
    Case(CaseError),
    Json(serde_json::Error),
    NotFound(&'static str),
    InvalidPhase(&'static str),
    EmptyField(&'static str),
    StopNotConfirmed,
}

impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => write!(f, "取消对账存储失败：{err}"),
            Self::Case(err) => write!(f, "对账案件失败：{err}"),
            Self::Json(err) => write!(f, "取消事件载荷损坏：{err}"),
            Self::NotFound(what) => write!(f, "未找到{what}"),
            Self::InvalidPhase(reason) => write!(f, "取消阶段不允许该操作：{reason}"),
            Self::EmptyField(field) => write!(f, "{field} 不能为空"),
            Self::StopNotConfirmed => write!(f, "停止未确认：不得释放占用或关闭待核实"),
        }
    }
}

impl std::error::Error for CancelError {}
impl From<StoreError> for CancelError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<CaseError> for CancelError {
    fn from(value: CaseError) -> Self {
        Self::Case(value)
    }
}
impl From<serde_json::Error> for CancelError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// 取消状态：`AckReceived` 与 `Unknown` 都仍是"停止未确认"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelPhase {
    Requested,
    AckReceived,
    /// 外部结果不明或回包丢失；保留待核实，不新建 Attempt。
    Unknown,
    /// 有独立证据确认外部执行已停止。
    StopConfirmed,
}

impl CancelPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::AckReceived => "ack_received",
            Self::Unknown => "unknown",
            Self::StopConfirmed => "stop_confirmed",
        }
    }

    pub fn stop_confirmed(self) -> bool {
        self == Self::StopConfirmed
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelEvent {
    pub cancel_id: String,
    pub attempt_id: String,
    pub invocation_id: Option<String>,
    pub phase: CancelPhase,
    /// 取消请求的回执引用；`cancel_ack` 不携带停止证据。
    pub ack_ref: Option<String>,
    /// 停止确认必须带独立证据（invocation_id + 外部结果引用）。
    pub stop_evidence: Option<String>,
    pub risk_reservation_refs: Vec<String>,
    pub known_evidence: String,
    pub assignee: String,
    pub next_step: String,
    pub observed_at_ms: i64,
}

impl CancelEvent {
    fn aggregate_id(&self) -> String {
        format!("{CANCEL_PREFIX}{}", self.attempt_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequest<'a> {
    pub cancel_id: &'a str,
    pub attempt_id: &'a str,
    pub company_id: &'a str,
    pub invocation_id: Option<&'a str>,
    pub risk_reservation_refs: Vec<String>,
    pub known_evidence: &'a str,
    /// 处理人：待核实项必须可归属到具体人。
    pub assignee: &'a str,
    /// 下一步对账动作，界面必须可见。
    pub next_step: &'a str,
}

fn append_event(store: &mut SqliteStore, event: &CancelEvent) -> Result<(), CancelError> {
    let version = store.count_events(&event.aggregate_id())? + 1;
    let payload = serde_json::to_string(event)?;
    let event_id = format!("{}-{}", event.aggregate_id(), version);
    let uow = store.transaction()?;
    uow.insert_event(
        &event_id,
        &event.cancel_id,
        &event.aggregate_id(),
        version as u64,
        &payload,
    )?;
    uow.commit()?;
    Ok(())
}

pub fn load_cancel(
    store: &mut SqliteStore,
    company_id: &str,
    attempt_id: &str,
) -> Result<Option<CancelEvent>, CancelError> {
    let aggregate = format!("{CANCEL_PREFIX}{attempt_id}");
    let Some(payload) = store.latest_event_payload(&aggregate)? else {
        return Ok(None);
    };
    let event: CancelEvent = serde_json::from_str(&payload)?;
    if event.attempt_id != attempt_id {
        return Err(CancelError::InvalidPhase("事件与目标 Attempt 不一致"));
    }
    let _ = company_id; // 聚合 id 已包含 attempt；Company 过滤由调用方的查询入口负责。
    Ok(Some(event))
}

/// 发起取消：只记请求与对账案件，不改 Attempt、不改预留。
pub fn request_cancel(
    store: &mut SqliteStore,
    request: &CancelRequest<'_>,
    now_ms: i64,
) -> Result<CancelEvent, CancelError> {
    for (field, value) in [
        ("cancel_id", request.cancel_id),
        ("attempt_id", request.attempt_id),
        ("company_id", request.company_id),
        ("assignee", request.assignee),
        ("next_step", request.next_step),
    ] {
        if value.trim().is_empty() {
            return Err(CancelError::EmptyField(field));
        }
    }
    if load_cancel(store, request.company_id, request.attempt_id)?.is_some() {
        return Err(CancelError::InvalidPhase("同一 Attempt 已有取消请求"));
    }
    let event = CancelEvent {
        cancel_id: request.cancel_id.to_string(),
        attempt_id: request.attempt_id.to_string(),
        invocation_id: request.invocation_id.map(str::to_string),
        phase: CancelPhase::Requested,
        ack_ref: None,
        stop_evidence: None,
        risk_reservation_refs: request.risk_reservation_refs.clone(),
        known_evidence: request.known_evidence.to_string(),
        assignee: request.assignee.to_string(),
        next_step: request.next_step.to_string(),
        observed_at_ms: now_ms,
    };
    append_event(store, &event)?;
    open_case(
        store,
        &CaseRequest {
            company_id: request.company_id,
            case_id: &format!("case-{}", request.cancel_id),
            invocation_id: request.invocation_id,
            logical_action_key: &format!("{CANCEL_PREFIX}{}", request.attempt_id),
            impact_scope: "attempt_stop",
            risk_reservation_refs: request.risk_reservation_refs.clone(),
            known_evidence: request.known_evidence,
            assignee: request.assignee,
            next_step: request.next_step,
        },
        now_ms,
    )?;
    Ok(event)
}

/// 把新证据同步到对账案件，让界面投影与事件一致（案件仍是唯一展示来源）。
fn update_case_evidence(
    store: &mut SqliteStore,
    company_id: &str,
    attempt_id: &str,
    known_evidence: &str,
    now_ms: i64,
) -> Result<(), CancelError> {
    let key = format!("{CANCEL_PREFIX}{attempt_id}");
    let mut case = store
        .list_reconciliation_cases(company_id, true)?
        .into_iter()
        .find(|case| case.logical_action_key == key)
        .ok_or(CancelError::NotFound("对账案件"))?;
    case.known_evidence = known_evidence.to_string();
    case.updated_at_ms = now_ms;
    let uow = store.transaction()?;
    uow.put_reconciliation_case(&case)?;
    uow.commit()?;
    Ok(())
}

/// 收到 cancel_ack：只改事件阶段为 AckReceived，停止仍未确认。
/// 不改预留、不改 Attempt、不结案。
pub fn record_cancel_ack(
    store: &mut SqliteStore,
    company_id: &str,
    attempt_id: &str,
    ack_ref: &str,
    now_ms: i64,
) -> Result<CancelEvent, CancelError> {
    if ack_ref.trim().is_empty() {
        return Err(CancelError::EmptyField("ack_ref"));
    }
    let mut event =
        load_cancel(store, company_id, attempt_id)?.ok_or(CancelError::NotFound("取消请求"))?;
    match event.phase {
        CancelPhase::Requested => {}
        CancelPhase::AckReceived => return Ok(event), // 重复 ack 幂等，不产生新事件。
        CancelPhase::Unknown | CancelPhase::StopConfirmed => {
            return Err(CancelError::InvalidPhase(
                "ack 不能覆盖 Unknown 或已确认停止",
            ));
        }
    }
    event.phase = CancelPhase::AckReceived;
    event.ack_ref = Some(ack_ref.to_string());
    event.known_evidence = format!(
        "{}；收到 cancel_ack={ack_ref}，仅确认请求送达",
        event.known_evidence
    );
    event.observed_at_ms = now_ms;
    append_event(store, &event)?;
    update_case_evidence(store, company_id, attempt_id, &event.known_evidence, now_ms)?;
    Ok(event)
}

/// 回包丢失或外部结果不明：标记 Unknown，保留风险占用与待核实案件。
pub fn mark_unknown(
    store: &mut SqliteStore,
    company_id: &str,
    attempt_id: &str,
    evidence: &str,
    now_ms: i64,
) -> Result<CancelEvent, CancelError> {
    if evidence.trim().is_empty() {
        return Err(CancelError::EmptyField("evidence"));
    }
    let mut event =
        load_cancel(store, company_id, attempt_id)?.ok_or(CancelError::NotFound("取消请求"))?;
    if event.phase == CancelPhase::StopConfirmed {
        return Err(CancelError::InvalidPhase("已确认停止不能改回 Unknown"));
    }
    event.phase = CancelPhase::Unknown;
    event.known_evidence = format!("{}；{evidence}", event.known_evidence);
    event.observed_at_ms = now_ms;
    append_event(store, &event)?;
    update_case_evidence(store, company_id, attempt_id, &event.known_evidence, now_ms)?;
    Ok(event)
}

/// 确认停止：必须给出独立证据（调用 id + 外部结果引用），且不能是仅 ack。
/// 只确认事实，不释放预留——释放由预算 C/R/U 结算完成。
pub fn confirm_stop(
    store: &mut SqliteStore,
    company_id: &str,
    attempt_id: &str,
    invocation_id: &str,
    result_ref: &str,
    state: ExecutionState,
    now_ms: i64,
) -> Result<CancelEvent, CancelError> {
    if invocation_id.trim().is_empty() || result_ref.trim().is_empty() {
        return Err(CancelError::EmptyField("停止证据"));
    }
    if !state.is_terminal() {
        return Err(CancelError::InvalidPhase("停止证据的执行状态不是终态"));
    }
    let mut event =
        load_cancel(store, company_id, attempt_id)?.ok_or(CancelError::NotFound("取消请求"))?;
    if event.phase == CancelPhase::StopConfirmed {
        return Ok(event);
    }
    if event
        .invocation_id
        .as_deref()
        .is_some_and(|value| value != invocation_id)
    {
        return Err(CancelError::InvalidPhase("停止证据的调用与取消请求不一致"));
    }
    event.phase = CancelPhase::StopConfirmed;
    event.invocation_id = Some(invocation_id.to_string());
    event.stop_evidence = Some(format!("{invocation_id}:{result_ref}:{}", state.as_str()));
    event.known_evidence = format!(
        "{}；停止确认 {invocation_id}/{result_ref} ({})",
        event.known_evidence,
        state.as_str()
    );
    event.observed_at_ms = now_ms;
    append_event(store, &event)?;

    let case_id = format!("case-{}", event.cancel_id);
    resolve_case(store, company_id, &case_id, &event.known_evidence, now_ms)?;
    Ok(event)
}

/// 界面投影：只从宿主事件与对账案件计算，不写入业务状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCancellation {
    pub case_id: String,
    pub attempt_id: String,
    pub phase: CancelPhase,
    pub stop_confirmed: bool,
    pub assignee: String,
    pub required_decision: String,
    pub available_actions: Vec<&'static str>,
    pub risk_reservation_refs: Vec<String>,
    pub known_evidence: String,
}

pub fn pending_cancellations(
    store: &mut SqliteStore,
    company_id: &str,
) -> Result<Vec<PendingCancellation>, CancelError> {
    let mut out = Vec::new();
    for case in store
        .list_reconciliation_cases(company_id, true)?
        .into_iter()
        .filter(|case| case.logical_action_key.starts_with(CANCEL_PREFIX))
    {
        let attempt_id = case
            .logical_action_key
            .strip_prefix(CANCEL_PREFIX)
            .unwrap_or_default()
            .to_string();
        let phase = load_cancel(store, company_id, &attempt_id)?
            .map(|event| event.phase)
            .unwrap_or(CancelPhase::Unknown);
        let (required_decision, actions) = match phase {
            CancelPhase::Requested => (
                "确认取消请求是否已送达执行端",
                vec!["record_cancel_ack", "mark_unknown"],
            ),
            CancelPhase::AckReceived | CancelPhase::Unknown => (
                "查询外部执行是否真正停止；未确认前保留风险占用",
                vec!["inspect_execution", "confirm_stop"],
            ),
            CancelPhase::StopConfirmed => ("已确认停止；由预算流程结算预留", vec![]),
        };
        out.push(PendingCancellation {
            case_id: case.case_id,
            attempt_id,
            phase,
            stop_confirmed: phase.stop_confirmed(),
            assignee: case.assignee,
            required_decision: required_decision.to_string(),
            available_actions: actions,
            risk_reservation_refs: case.risk_reservation_refs,
            known_evidence: case.known_evidence,
        });
    }
    Ok(out)
}

/// 展示停止是否确认：cancel_ack 之后仍为 false（I-11）。
pub fn stop_is_confirmed(event: &CancelEvent) -> bool {
    event.phase.stop_confirmed() && event.stop_evidence.is_some()
}

/// 取消请求不释放任何风险占用；调用方可用本函数核对预留未变。
pub fn reservation_is_held(
    store: &mut SqliteStore,
    reservation_id: &str,
) -> Result<bool, CancelError> {
    let Some(reservation) = store.transaction()?.get_reservation(reservation_id)? else {
        return Ok(false);
    };
    Ok(matches!(
        reservation.state,
        ReservationState::Active | ReservationState::Risk
    ) && (reservation.reserved > 0 || reservation.risk > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::DomainId;
    use crate::domain::budget::{Measure, Reservation};
    use crate::recovery_models::CaseState;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW: i64 = 1_763_700_000_000;

    struct Fixture {
        _root: PathBuf,
        db: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let seq = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            let root =
                std::env::temp_dir().join(format!("bod-cancel-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            Self {
                _root: root.clone(),
                db: root.join("bod.sqlite"),
            }
        }

        fn store(&self) -> SqliteStore {
            SqliteStore::open(&self.db, &BodConfig::default()).unwrap()
        }

        fn risk_reservation(&self, store: &mut SqliteStore) {
            let reservation = Reservation {
                reservation_id: "res-cancel-1".to_string(),
                company_id: DomainId::new("company-default"),
                project_id: None,
                agent_id: None,
                run_id: None,
                work_package_id: None,
                measure: Measure::money_micros(0, "CNY"),
                reserved: 0,
                confirmed: 0,
                risk: 1_000,
                state: ReservationState::Risk,
                logical_key: Some("cancel-res-1".to_string()),
                created_at_ms: NOW,
                updated_at_ms: NOW,
            };
            {
                let uow = store.transaction().unwrap();
                uow.insert_reservation(&reservation).unwrap();
                uow.commit().unwrap();
            }
        }

        fn request<'a>(&'a self) -> CancelRequest<'a> {
            CancelRequest {
                cancel_id: "cancel-1",
                attempt_id: "attempt-1",
                company_id: "company-default",
                invocation_id: Some("inv-1"),
                risk_reservation_refs: vec!["res-cancel-1".to_string()],
                known_evidence: "用户请求取消 task-migrate",
                assignee: "owner-1",
                next_step: "收到回执后查询外部执行状态",
            }
        }
    }

    #[test]
    fn invariant_i_11_cancel_ack_keeps_risk_and_attempt() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.risk_reservation(&mut store);
        let attempts_before = store.count_attempts("company-default").unwrap();

        let requested = request_cancel(&mut store, &fixture.request(), NOW).unwrap();
        assert_eq!(requested.phase, CancelPhase::Requested);
        let acked = record_cancel_ack(&mut store, "company-default", "attempt-1", "ack-1", NOW + 1)
            .unwrap();
        assert_eq!(acked.phase, CancelPhase::AckReceived);
        assert_eq!(acked.ack_ref.as_deref(), Some("ack-1"));
        assert!(!stop_is_confirmed(&acked), "cancel_ack 不是停止确认");
        assert!(reservation_is_held(&mut store, "res-cancel-1").unwrap());
        assert_eq!(
            store.count_attempts("company-default").unwrap(),
            attempts_before,
            "取消不得新建 Attempt"
        );
        assert_eq!(
            store
                .transaction()
                .unwrap()
                .get_reservation("res-cancel-1")
                .unwrap()
                .unwrap()
                .state,
            ReservationState::Risk
        );

        let pending = pending_cancellations(&mut store, "company-default").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].phase, CancelPhase::AckReceived);
        assert!(!pending[0].stop_confirmed);
        assert_eq!(pending[0].assignee, "owner-1");
        assert_eq!(pending[0].case_id, "case-cancel-1");
        assert_eq!(
            pending[0].risk_reservation_refs,
            vec!["res-cancel-1".to_string()]
        );
        assert!(
            pending[0]
                .required_decision
                .contains("查询外部执行是否真正停止")
        );
        assert!(pending[0].available_actions.contains(&"confirm_stop"));
    }

    #[test]
    fn cancel_ack_is_idempotent_and_never_closes_case() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        request_cancel(&mut store, &fixture.request(), NOW).unwrap();
        let first = record_cancel_ack(&mut store, "company-default", "attempt-1", "ack-1", NOW + 1)
            .unwrap();
        let second =
            record_cancel_ack(&mut store, "company-default", "attempt-1", "ack-1", NOW + 2)
                .unwrap();
        assert_eq!(first, second, "重复 ack 不产生新事件");
        assert_eq!(store.count_events("cancel:attempt-1").unwrap(), 2);
        let cases = store
            .list_reconciliation_cases("company-default", true)
            .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].state, CaseState::Open);
    }

    #[test]
    fn lost_ack_marks_unknown_with_assignee_and_keeps_risk() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.risk_reservation(&mut store);
        request_cancel(&mut store, &fixture.request(), NOW).unwrap();
        let unknown = mark_unknown(
            &mut store,
            "company-default",
            "attempt-1",
            "回包丢失，外部结果不明",
            NOW + 3,
        )
        .unwrap();
        assert_eq!(unknown.phase, CancelPhase::Unknown);
        assert!(!stop_is_confirmed(&unknown));
        assert!(reservation_is_held(&mut store, "res-cancel-1").unwrap());
        let pending = pending_cancellations(&mut store, "company-default").unwrap();
        assert_eq!(pending[0].phase, CancelPhase::Unknown);
        assert_eq!(pending[0].assignee, "owner-1");
        assert!(!pending[0].stop_confirmed);
        assert!(pending[0].known_evidence.contains("回包丢失"));
    }

    #[test]
    fn confirm_stop_requires_terminal_evidence_and_does_not_release_reservation() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.risk_reservation(&mut store);
        request_cancel(&mut store, &fixture.request(), NOW).unwrap();
        record_cancel_ack(&mut store, "company-default", "attempt-1", "ack-1", NOW + 1).unwrap();

        let non_terminal = confirm_stop(
            &mut store,
            "company-default",
            "attempt-1",
            "inv-1",
            "external-1",
            ExecutionState::Cancelling,
            NOW + 2,
        );
        assert!(matches!(
            non_terminal,
            Err(CancelError::InvalidPhase("停止证据的执行状态不是终态"))
        ));
        let wrong_invocation = confirm_stop(
            &mut store,
            "company-default",
            "attempt-1",
            "inv-other",
            "external-1",
            ExecutionState::Completed,
            NOW + 2,
        );
        assert!(matches!(
            wrong_invocation,
            Err(CancelError::InvalidPhase(_))
        ));

        let confirmed = confirm_stop(
            &mut store,
            "company-default",
            "attempt-1",
            "inv-1",
            "external-1",
            ExecutionState::Cancelled,
            NOW + 3,
        )
        .unwrap();
        assert!(stop_is_confirmed(&confirmed));
        assert_eq!(
            confirmed.stop_evidence.as_deref(),
            Some("inv-1:external-1:Cancelled")
        );
        assert_eq!(store.count_attempts("company-default").unwrap(), 0);
        // 确认停止只确认事实；预留释放必须走预算结算，不能在此静默清零。
        assert!(reservation_is_held(&mut store, "res-cancel-1").unwrap());
        let open = store
            .list_reconciliation_cases("company-default", true)
            .unwrap();
        assert!(open.is_empty(), "已裁决案件不再显示在待核实");
        let all = store
            .list_reconciliation_cases("company-default", false)
            .unwrap();
        assert_eq!(all[0].state, CaseState::Resolved);
    }

    #[test]
    fn restart_keeps_pending_cancellation_and_handler() {
        let fixture = Fixture::new();
        {
            let mut store = fixture.store();
            fixture.risk_reservation(&mut store);
            request_cancel(&mut store, &fixture.request(), NOW).unwrap();
            record_cancel_ack(&mut store, "company-default", "attempt-1", "ack-1", NOW + 1)
                .unwrap();
        }
        // 关掉再打开同一 SQLite 文件：待核实与处理人仍可查询。
        let mut reopened = fixture.store();
        let pending = pending_cancellations(&mut reopened, "company-default").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].phase, CancelPhase::AckReceived);
        assert_eq!(pending[0].assignee, "owner-1");
        assert!(!pending[0].stop_confirmed);
        assert!(reservation_is_held(&mut reopened, "res-cancel-1").unwrap());
    }

    #[test]
    fn request_cancel_rejects_missing_handler_and_duplicate() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let mut request = fixture.request();
        request.assignee = "";
        assert!(matches!(
            request_cancel(&mut store, &request, NOW),
            Err(CancelError::EmptyField("assignee"))
        ));
        let valid = fixture.request();
        request_cancel(&mut store, &valid, NOW).unwrap();
        assert!(matches!(
            request_cancel(&mut store, &valid, NOW + 1),
            Err(CancelError::InvalidPhase(_))
        ));
    }
}
