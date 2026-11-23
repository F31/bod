//! P8 §11 外部交付跟踪的读模型（3.1）：跟踪项、客户目标与覆盖投影。
//!
//! 三层分工，不要混：
//!
//! - **事实**在库里：跟踪项、目标（客户/环境/单元、验收条件、开关、人群）、追加式观察。
//! - **判定**只有一处：`coverage_receipt::judge_coverage`。本模块把事实规范化成
//!   `CoverageInput` 再交给它，**不写第二套判定**（design D1）。因此库里不存覆盖结论——
//!   存了就一定会与事实漂移。
//! - **投影**在这里：把判定结果、决定该结果的那条观察、缺口码与允许动作拼成视图。
//!
//! 三条容易做错的语义，都在测试里钉住：
//!
//! 1. **回滚是新的观察**，不是把部署观察改判为失败。判定取「部署观察与回滚观察中较晚的一条」，
//!    时间相同时回滚胜出（它是后写下的事实）。
//! 2. **缺口是「所有不满足项」，原因码是「判定停在哪一项」**。两者不互为派生：
//!    原因只有一句（先卡住的那条），缺口要列全，否则修完一处又冒一处。
//! 3. **允许动作只列已实现（`served`）的端点**。视图里写着能做、路径却回 501，
//!    比不写更糟——客户端会照着做。

use crate::coverage_receipt::{
    CoverageInput, CoverageSource, CoverageTuple, CoverageVerdict, DeploymentObservation,
    InstallationReceipt, SwitchState, VerificationConclusion, judge_coverage,
};
use crate::delivery_api::{self, DeliveryTrackingView, EvidenceStamp, TargetCoverage, TargetRef};
use crate::delivery_link::WorkPackageLink;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// 缺口码
// ---------------------------------------------------------------------------

/// 缺口码是**稳定字符串**，不随语言变化：客户端按码分支，改码等于改契约。
pub const GAP_NO_ARTIFACT_MAPPING: &str = "no-artifact-mapping";
pub const GAP_POPULATION_UNKNOWN: &str = "population-unknown";
pub const GAP_SWITCH_NOT_SYNCED: &str = "switch-not-synced";
pub const GAP_VERIFICATION_NOT_PASSED: &str = "verification-not-passed";
pub const GAP_DEPLOYMENT_NOT_OBSERVED: &str = "deployment-not-observed";
pub const GAP_ROLLBACK_OBSERVED: &str = "rollback-observed";
pub const GAP_INSTALL_RECEIPT_UNAUTHORIZED: &str = "install-receipt-unauthorized";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 读模型错误。读侧遇到表外取值一律报错，不静默回退：
/// 库里出现应用不认识的取值，说明有人绕过应用写库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryError {
    UnknownTrackingState(String),
    UnknownObservationKind(String),
    UnknownSourceKind(String),
    UnknownSwitchState(String),
    UnknownVerificationState(String),
    /// 观察的目标不属于该跟踪项（复合外键之外的显式校验，给出可读原因）。
    TargetNotInTracking {
        target_id: String,
        tracking_id: String,
    },
    /// 人工补证没有写授权范围与复验依据：不构成证据。
    EmptyManualEvidenceNote,
    /// 跟踪项不存在（关联来源、追加观察前的显式校验）。
    TrackingNotFound(String),
    /// 资源版本无法表示为无符号版本号。
    InvalidVersion(i64),
}

impl DeliveryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownTrackingState(_) => "DELIVERY_UNKNOWN_STATE",
            Self::UnknownObservationKind(_) => "DELIVERY_UNKNOWN_KIND",
            Self::UnknownSourceKind(_) => "DELIVERY_UNKNOWN_SOURCE_KIND",
            Self::UnknownSwitchState(_) => "DELIVERY_UNKNOWN_SWITCH_STATE",
            Self::UnknownVerificationState(_) => "DELIVERY_UNKNOWN_VERIFICATION_STATE",
            Self::TargetNotInTracking { .. } => "DELIVERY_TARGET_NOT_IN_TRACKING",
            Self::EmptyManualEvidenceNote => "DELIVERY_MANUAL_EVIDENCE_WITHOUT_NOTE",
            Self::TrackingNotFound(_) => "DELIVERY_TRACKING_NOT_FOUND",
            Self::InvalidVersion(_) => "DELIVERY_INVALID_VERSION",
        }
    }

    pub fn explain(&self) -> String {
        match self {
            Self::UnknownTrackingState(value) => {
                format!("delivery_tracking.state 出现表外值：{value}")
            }
            Self::UnknownObservationKind(value) => {
                format!("delivery_observations.kind 出现表外值：{value}")
            }
            Self::UnknownSourceKind(value) => {
                format!("delivery_observations.source_kind 出现表外值：{value}")
            }
            Self::UnknownSwitchState(value) => {
                format!("delivery_targets.switch_state 出现表外值：{value}")
            }
            Self::UnknownVerificationState(value) => {
                format!("verification_records.state 出现表外值：{value}")
            }
            Self::TargetNotInTracking {
                target_id,
                tracking_id,
            } => format!("目标 {target_id} 不属于跟踪项 {tracking_id}"),
            Self::EmptyManualEvidenceNote => {
                "人工补证必须写清洗授权范围与复验依据（note 不能为空）".to_string()
            }
            Self::TrackingNotFound(id) => format!("跟踪项 {id} 不存在"),
            Self::InvalidVersion(value) => format!("资源版本 {value} 不是正数"),
        }
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explain())
    }
}

impl std::error::Error for DeliveryError {}

// ---------------------------------------------------------------------------
// 事实行
// ---------------------------------------------------------------------------

/// 跟踪项状态。只区分「进行中 / 已关闭」：交付进度由目标推出来，不由状态字段声称。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackingState {
    Open,
    Closed,
}

impl TrackingState {
    pub fn parse(text: &str) -> Result<Self, DeliveryError> {
        match text {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            other => Err(DeliveryError::UnknownTrackingState(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// `delivery_tracking` 一行。
///
/// `issue_uid` / `run_uid` / `attempt_id` 都是 `Option`：外部完成的研发不要求
/// 内部运行存在（D3）。测试里有一条用例专门证明「三个都为空也能成立」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackingRow {
    pub tracking_id: String,
    pub company_id: String,
    pub project_id: String,
    pub title: String,
    pub requirement: String,
    /// 空串表示未指派。不假装「有人负责」。
    pub owner_id: String,
    pub state: TrackingState,
    pub issue_uid: Option<String>,
    pub run_uid: Option<String>,
    pub attempt_id: Option<String>,
    pub resource_version: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl TrackingRow {
    /// 是否挂在内部分工上。用来说明「这条跟踪项是外部导入，不是内部运行的投影」。
    pub fn has_internal_anchor(&self) -> bool {
        self.issue_uid.is_some() || self.run_uid.is_some() || self.attempt_id.is_some()
    }
}

/// `delivery_targets` 一行：客户 × 环境 ×（可选）部署单元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRow {
    pub target_id: String,
    pub company_id: String,
    pub tracking_id: String,
    pub customer_id: String,
    pub environment: String,
    /// 空串表示整个环境。
    pub deployment_unit: String,
    pub acceptance: String,
    pub switch_state: SwitchState,
    pub population_known: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl TargetRow {
    /// 覆盖元组与安装回执里的单元标识：有部署单元用它，没有就用环境。
    ///
    /// 不用空串兜底：`unit_id` 会被写进回执与对账，空串会让「整个环境」与
    /// 「字段没填」看起来一样。
    pub fn unit_key(&self) -> String {
        if self.deployment_unit.is_empty() {
            self.environment.clone()
        } else {
            self.deployment_unit.clone()
        }
    }

    pub fn to_ref(&self) -> TargetRef {
        TargetRef {
            customer_id: self.customer_id.clone(),
            environment: self.environment.clone(),
            deployment_unit: if self.deployment_unit.is_empty() {
                None
            } else {
                Some(self.deployment_unit.clone())
            },
        }
    }
}

/// 观察类别。回滚是**新的观察**，不是把部署观察改判为失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationKind {
    Deployment,
    Rollback,
    /// 授权实施人提交的补证（离线安装、现场核对）。
    ManualEvidence,
}

impl ObservationKind {
    pub fn parse(text: &str) -> Result<Self, DeliveryError> {
        match text {
            "deployment" => Ok(Self::Deployment),
            "rollback" => Ok(Self::Rollback),
            "manual-evidence" => Ok(Self::ManualEvidence),
            other => Err(DeliveryError::UnknownObservationKind(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deployment => "deployment",
            Self::Rollback => "rollback",
            Self::ManualEvidence => "manual-evidence",
        }
    }
}

/// 证据来源类别，直接对应 `EvidenceStamp.source` 的三个取值。
pub fn parse_source_kind(text: &str) -> Result<&'static str, DeliveryError> {
    match text {
        "connector" => Ok("connector"),
        "manual" => Ok("manual"),
        "internal" => Ok("internal"),
        other => Err(DeliveryError::UnknownSourceKind(other.to_string())),
    }
}

/// `delivery_observations` 一行。追加式，只读不写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRow {
    pub observation_id: String,
    pub company_id: String,
    pub tracking_id: String,
    pub target_id: String,
    pub kind: ObservationKind,
    /// `connector` / `manual` / `internal`，已校验过的稳定取值。
    pub source_kind: &'static str,
    /// 人类可读定位（如 `owner/repo#318`）。**不参与判定**：它没有稳定 ID，
    /// 只是给人看的坐标；身份在 `delivery_source`（五元组）。
    pub source_ref: String,
    /// 关联的来源修订（3.2）。`None` 表示这次观察没有来源身份——
    /// 人工补证可以没有来源；连接器观察必须带，由导入路径在写入时强制（3.5）。
    pub source_revision_id: Option<String>,
    pub artifact_revision: Option<String>,
    pub digest: String,
    /// 部署修订标识（`DeploymentObservation::Observed.revision`）。
    pub revision: String,
    pub note: String,
    pub actor_id: String,
    pub authorized: bool,
    pub observed_at: String,
    pub observed_at_ms: i64,
    pub recorded_at_ms: i64,
}

/// `verification_records.state` → 验证结论。
///
/// `waived` 映射为 `Unknown` 而不是 `Passed`：豁免是「不要求验」，不是「验过了」。
/// 把它算成通过，等于用决策替代证据。
pub fn verification_conclusion(state: &str) -> Result<VerificationConclusion, DeliveryError> {
    match state {
        "verified" => Ok(VerificationConclusion::Passed),
        "rejected" | "needs_revision" => Ok(VerificationConclusion::Failed),
        "pending" | "verifying" | "waived" => Ok(VerificationConclusion::Unknown),
        other => Err(DeliveryError::UnknownVerificationState(other.to_string())),
    }
}

// ---------------------------------------------------------------------------
// 覆盖投影
// ---------------------------------------------------------------------------

/// 每组目标的投影结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCoverageView {
    pub target: TargetRow,
    pub verdict: CoverageVerdict,
    /// 判定停在哪一项（`covered` 时为 `None`）。
    pub reason: Option<String>,
    /// 所有不满足项。与 `reason` 不互为派生。
    pub gaps: Vec<&'static str>,
    /// 决定该结论的那条观察。
    pub evidence_revision: Option<String>,
    pub observed_at: Option<String>,
}

/// 跟踪项的读模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackingView {
    pub tracking: TrackingRow,
    /// 按客户/环境/单元排序，输出稳定。
    pub targets: Vec<TargetCoverageView>,
    /// 证据时间线，最新在前。
    pub evidence: Vec<EvidenceStamp>,
    /// 后补的内部工作包关联（3.4），按关联时间升序。
    /// **不进契约响应**：`GET /delivery-tracking/{id}` 的形状在 2.2 已冻结；
    /// 它现在只在 `bod tracking --id` 里显示，用于核对「后补的关联落到了哪条跟踪项」。
    pub work_packages: Vec<WorkPackageLink>,
}

impl TrackingView {
    /// 跟踪项层面的缺口：各目标缺口的并集，去重后按字典序。
    ///
    /// 排序而不是保持出现顺序：客户端会把它当集合用，顺序不定会让两次读数难以比对。
    pub fn gaps(&self) -> Vec<&'static str> {
        let mut set: BTreeSet<&'static str> = BTreeSet::new();
        for target in &self.targets {
            set.extend(target.gaps.iter().copied());
        }
        set.into_iter().collect()
    }

    /// 允许动作的**候选**：只按状态算「这件事此刻说不说得通」。
    ///
    /// 候选不等于能做。上有入口、需求、当事人三关（`delivery_api::select_allowed`），
    /// 那三关与视图无关，所以不写在读模型里——读模型只回答「状态上合不合理」。
    ///
    /// 候选里有 `send` 的位置是刻意的：它要等草稿能审完之后才可能出现，
    /// 在那之前连候选都不该有——所以这里不写 `send`，而不是让它被过滤掉。
    pub fn action_candidates(&self) -> Vec<&'static str> {
        let mut candidates = Vec::new();
        if self.tracking.state == TrackingState::Open {
            candidates.push(delivery_api::ACTION_LINK_SOURCE);
        }
        if self
            .targets
            .iter()
            .any(|target| target.verdict.is_covered())
        {
            candidates.push(delivery_api::ACTION_DRAFT);
        }
        candidates
    }

    /// 投影成冻结契约里的响应体（`GET /api/v1/delivery-tracking/{id}`）。
    ///
    /// `allowed_actions` 由调用方算好传进来：它是主体相对且调用面相对的结果，
    /// 而读模型既不知道主体、也不知道这次是从 HTTP 还是 CLI 读的。
    /// 让读模型自己算，等于把「谁」和「哪个面」硬编码成某一种。
    /// CLI 的 `--json` 直接输出它：同一形状只维护一份，避免「命令行看到的」
    /// 与「接口返回的」两套字段各自演进。
    pub fn to_dto(
        &self,
        allowed_actions: Vec<&'static str>,
    ) -> Result<DeliveryTrackingView, DeliveryError> {
        let version = u64::try_from(self.tracking.resource_version)
            .map_err(|_| DeliveryError::InvalidVersion(self.tracking.resource_version))?;
        Ok(DeliveryTrackingView {
            tracking_id: self.tracking.tracking_id.clone(),
            project_id: self.tracking.project_id.clone(),
            version,
            owner: if self.tracking.owner_id.trim().is_empty() {
                None
            } else {
                Some(self.tracking.owner_id.clone())
            },
            targets: self
                .targets
                .iter()
                .map(|target| TargetCoverage {
                    target: target.target.to_ref(),
                    verdict: verdict_code(&target.verdict),
                    reason: target.reason.clone(),
                    evidence_revision: target.evidence_revision.clone(),
                    observed_at: target.observed_at.clone(),
                })
                .collect(),
            evidence: self.evidence.clone(),
            gaps: self.gaps().into_iter().map(str::to_string).collect(),
            allowed_actions: allowed_actions.into_iter().map(str::to_string).collect(),
        })
    }
}

/// `CoverageVerdict` → 线上枚举。取值一一对应，不做二次判断。
pub fn verdict_code(verdict: &CoverageVerdict) -> delivery_api::CoverageVerdictCode {
    match verdict {
        CoverageVerdict::Covered { .. } => delivery_api::CoverageVerdictCode::Covered,
        CoverageVerdict::PendingVerification { .. } => {
            delivery_api::CoverageVerdictCode::PendingVerification
        }
        CoverageVerdict::PendingInstall { .. } => delivery_api::CoverageVerdictCode::PendingInstall,
    }
}

/// 证据引用：优先用来源修订 ID，没有才退回观察 ID。
///
/// 两套编号并存是刻意的：来源修订是「哪一版来源内容」，观察是「哪一次核对」。
/// 有修订时用修订，草稿（5.1）的 `evidence_revision` 才能与来源对上同一套编号；
/// 人工补证没有来源修订，退回观察 ID 至少保证引用非空且唯一。
pub fn evidence_reference(observation: &ObservationRow) -> &str {
    observation
        .source_revision_id
        .as_deref()
        .unwrap_or(&observation.observation_id)
}

/// 证据时间线：观察即证据。
pub fn evidence_stamps(observations: &[ObservationRow]) -> Vec<EvidenceStamp> {
    observations
        .iter()
        .map(|observation| EvidenceStamp {
            revision: evidence_reference(observation).to_string(),
            source: observation.source_kind.to_string(),
            observed_at: observation.observed_at.clone(),
        })
        .collect()
}

/// 把一组事实规范化成判定输入。**只做规范化，不做判定。**
pub fn coverage_input(
    project_id: &str,
    target: &TargetRow,
    observations: &[ObservationRow],
    verification: VerificationConclusion,
) -> CoverageInput {
    let deployment_observation = latest(observations, ObservationKind::Deployment);
    let rollback_observation = latest(observations, ObservationKind::Rollback);
    let deployment = match (deployment_observation, rollback_observation) {
        (Some(deployed), Some(rolled_back)) if rollback_wins(rolled_back, deployed) => {
            DeploymentObservation::RolledBack {
                at: rolled_back.observed_at.clone(),
            }
        }
        (Some(deployed), _) => DeploymentObservation::Observed {
            revision: deployed.revision.clone(),
            at: deployed.observed_at.clone(),
        },
        (None, Some(rolled_back)) => DeploymentObservation::RolledBack {
            at: rolled_back.observed_at.clone(),
        },
        (None, None) => DeploymentObservation::NotObserved,
    };
    // 制品摘要取自**最近一条带摘要的观察**：部署观察与人工补证都可能携带它
    // （离线安装回执引用的是同一个包）。只认部署观察会让离线安装永远无法被覆盖，
    // 那不是「更严格」，是把一种合法交付判成永久待核实。
    //
    // 不在这里判「摘要冲突」：同一版本出现两个摘要才算冲突，而「新版本部署」也会换摘要。
    // 两者要靠来源身份与版本区分（3.2 / 3.5），现在按时间取最新，并在报告里登记这个缺口。
    let digest = observations
        .iter()
        .filter(|observation| !observation.digest.trim().is_empty())
        .max_by_key(|observation| (observation.observed_at_ms, observation.recorded_at_ms))
        .map(|observation| observation.digest.clone())
        .unwrap_or_default();
    let installation = latest(observations, ObservationKind::ManualEvidence).map(|observation| {
        InstallationReceipt {
            unit_id: target.unit_key(),
            by: observation.actor_id.clone(),
            at: observation.observed_at.clone(),
            authorized: observation.authorized,
        }
    });
    CoverageInput {
        tuple: CoverageTuple::new(project_id, &target.environment, &target.unit_key(), &digest),
        deployment,
        verification,
        switch: target.switch_state,
        population_known: target.population_known,
        installation,
    }
}

/// 投影一个目标：规范化 → `judge_coverage` → 缺口 → 决定性观察。
/// 同一版本出现两个内容摘要：连接器与人工回执对同一个版本给出了不同内容。
///
/// 规格要求「保留两者并进入待核实，不自行选择其中一份」。这里只做识别：
/// 两条观察仍原样留在时间线里，结论停在待核实并说明冲突在哪一组版本上。
///
/// 判据必须带版本。只比摘要的话，每次正常发版都会被判成冲突（新版本换摘要），
/// 那会让「冲突」这个词失去意义——真正的冲突是**同一个版本**被描述成两份内容。
fn digest_conflict(observations: &[ObservationRow]) -> Option<String> {
    let mut by_revision: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for observation in observations {
        if observation.revision.trim().is_empty() || observation.digest.trim().is_empty() {
            continue;
        }
        by_revision
            .entry(observation.revision.as_str())
            .or_default()
            .insert(observation.digest.as_str());
    }
    by_revision
        .into_iter()
        .find(|(_, digests)| digests.len() > 1)
        .map(|(revision, digests)| {
            format!(
                "版本 {revision} 出现 {} 个不同摘要（{}）：连接器与人工证据不一致，需人工确认",
                digests.len(),
                digests.into_iter().collect::<Vec<_>>().join(" / ")
            )
        })
}

pub fn project_target(
    project_id: &str,
    target: &TargetRow,
    observations: &[ObservationRow],
    verification: VerificationConclusion,
) -> TargetCoverageView {
    let input = coverage_input(project_id, target, observations, verification);
    // 冲突优先于判定：两份证据互相矛盾时，任何「选一份」的结论都是替人做决定。
    let verdict = match digest_conflict(observations) {
        Some(reason) => CoverageVerdict::PendingVerification { reason },
        None => judge_coverage(&input),
    };
    let gaps = gaps_for(&input, &verdict);
    let decisive = decisive_observation(&verdict, observations);
    TargetCoverageView {
        target: target.clone(),
        reason: verdict_reason(&verdict),
        verdict,
        gaps,
        evidence_revision: decisive.map(|observation| evidence_reference(observation).to_string()),
        observed_at: decisive.map(|observation| observation.observed_at.clone()),
    }
}

fn verdict_reason(verdict: &CoverageVerdict) -> Option<String> {
    match verdict {
        CoverageVerdict::Covered { .. } => None,
        CoverageVerdict::PendingVerification { reason }
        | CoverageVerdict::PendingInstall { reason } => Some(reason.clone()),
    }
}

/// 缺口 = **所有**不满足项，不是「第一处卡点」。
///
/// 判定本身仍然只有 `judge_coverage` 一处：这里只按它的输入复查每一项是否成立，
/// 不重新排序、不自己决定结论。列出全部而不是只列第一条，是因为负责人需要
/// 一次看全要补什么；只给第一条会让人修完一处又冒一处。
pub fn gaps_for(input: &CoverageInput, verdict: &CoverageVerdict) -> Vec<&'static str> {
    match verdict {
        CoverageVerdict::Covered { .. } => Vec::new(),
        CoverageVerdict::PendingVerification { .. } => {
            let mut gaps = Vec::new();
            if !input.tuple.is_complete() {
                gaps.push(GAP_NO_ARTIFACT_MAPPING);
            }
            if !input.population_known {
                gaps.push(GAP_POPULATION_UNKNOWN);
            }
            if input.switch != SwitchState::Synced {
                gaps.push(GAP_SWITCH_NOT_SYNCED);
            }
            if input.verification != VerificationConclusion::Passed {
                gaps.push(GAP_VERIFICATION_NOT_PASSED);
            }
            if matches!(input.deployment, DeploymentObservation::RolledBack { .. }) {
                gaps.push(GAP_ROLLBACK_OBSERVED);
            }
            gaps
        }
        // `judge_coverage` 只在「部署未观察」时给 `PendingInstall`；仍然按输入复查，
        // 避免判定规则调整后缺口悄悄说谎。
        CoverageVerdict::PendingInstall { .. } => {
            let mut gaps = Vec::new();
            if matches!(input.deployment, DeploymentObservation::NotObserved) {
                gaps.push(GAP_DEPLOYMENT_NOT_OBSERVED);
            }
            if matches!(&input.installation, Some(receipt) if !receipt.authorized) {
                gaps.push(GAP_INSTALL_RECEIPT_UNAUTHORIZED);
            }
            gaps
        }
    }
}

/// 决定结论的那条观察。
///
/// `covered` 时按来源取对应的观察；其余情况取最近一条观察——「按哪一版证据得出的结论」
/// 对未覆盖的目标同样要能回答，否则负责人不知道该去看哪次部署。
fn decisive_observation<'a>(
    verdict: &CoverageVerdict,
    observations: &'a [ObservationRow],
) -> Option<&'a ObservationRow> {
    match verdict {
        CoverageVerdict::Covered {
            source: CoverageSource::ObservedDeployment,
        } => latest(observations, ObservationKind::Deployment),
        CoverageVerdict::Covered {
            source: CoverageSource::ManualInstall { .. },
        } => latest(observations, ObservationKind::ManualEvidence),
        _ => latest_any(observations),
    }
}

fn latest(observations: &[ObservationRow], kind: ObservationKind) -> Option<&ObservationRow> {
    observations
        .iter()
        .filter(|observation| observation.kind == kind)
        .max_by_key(|observation| (observation.observed_at_ms, observation.recorded_at_ms))
}

fn latest_any(observations: &[ObservationRow]) -> Option<&ObservationRow> {
    observations
        .iter()
        .max_by_key(|observation| (observation.observed_at_ms, observation.recorded_at_ms))
}

/// 回滚是否比部署更晚。时间相同时回滚胜出：它是后写下的事实，
/// 「同毫秒内先部署后回滚」比反向更可能。
fn rollback_wins(rolled_back: &ObservationRow, deployed: &ObservationRow) -> bool {
    (rolled_back.observed_at_ms, rolled_back.recorded_at_ms)
        >= (deployed.observed_at_ms, deployed.recorded_at_ms)
}

// ---------------------------------------------------------------------------
// 渲染（`bod tracking` 的文本输出）
// ---------------------------------------------------------------------------

/// 列表渲染。只给身份与规模，不给结论——结论离开证据就没有意义。
pub fn render_list(company_id: &str, rows: &[(TrackingRow, i64)]) -> String {
    if rows.is_empty() {
        return format!("{company_id} 没有交付跟踪项。\n");
    }
    let mut out = format!("{company_id} 交付跟踪：{} 个跟踪项\n", rows.len());
    for (tracking, target_count) in rows {
        let owner = if tracking.owner_id.trim().is_empty() {
            "未指派"
        } else {
            tracking.owner_id.as_str()
        };
        out.push_str(&format!(
            "- {}  {}  负责人 {}  v{}  目标 {}  {}\n",
            tracking.tracking_id,
            tracking.project_id,
            owner,
            tracking.resource_version,
            target_count,
            tracking.state.as_str()
        ));
    }
    out.push_str("\n详情：bod tracking --db <path> --id <跟踪项>\n");
    out
}

/// 详情渲染。逐目标给出结论、原因、决定该结论的证据与缺口。
pub fn render_one(view: &TrackingView, allowed: &[&'static str]) -> String {
    let tracking = &view.tracking;
    let mut out = format!(
        "跟踪项 {}（{}）\n  标题：{}\n  负责人：{}\n  版本：{}   状态：{}\n",
        tracking.tracking_id,
        tracking.project_id,
        tracking.title,
        if tracking.owner_id.trim().is_empty() {
            "未指派"
        } else {
            tracking.owner_id.as_str()
        },
        tracking.resource_version,
        tracking.state.as_str(),
    );
    if tracking.has_internal_anchor() {
        out.push_str("  内部关联：有\n");
    } else {
        out.push_str("  内部关联：无（外部交付不要求内部运行存在）\n");
    }
    if view.work_packages.is_empty() {
        out.push_str("  内部工作包：无（外部交付不要求工作包；后来补入的会追加在这里）\n");
    } else {
        out.push_str(&format!("  内部工作包：{} 个\n", view.work_packages.len()));
        for link in &view.work_packages {
            out.push_str(&format!(
                "  - {}（{}，{}）\n",
                link.work_package_revision,
                link.linked_by,
                // 时间用 UTC 字符串：毫秒数在终端里读不出「什么时候补的」。
                crate::conformance_report::utc_stamp(link.linked_at_ms)
            ));
        }
    }
    if view.targets.is_empty() {
        out.push_str("  目标：0 个（没有目标就没有交付对象，覆盖结论无从谈起）\n");
    } else {
        out.push_str(&format!("  目标 {} 个：\n", view.targets.len()));
        for target in &view.targets {
            let unit = if target.target.deployment_unit.is_empty() {
                "整个环境"
            } else {
                target.target.deployment_unit.as_str()
            };
            out.push_str(&format!(
                "  - 客户 {} / 环境 {} / {}：{}\n",
                target.target.customer_id,
                target.target.environment,
                unit,
                target.verdict.as_str()
            ));
            if let Some(reason) = &target.reason {
                out.push_str(&format!("      原因：{reason}\n"));
            }
            if let Some(revision) = &target.evidence_revision {
                let observed_at = target.observed_at.as_deref().unwrap_or("时间未记录");
                out.push_str(&format!("      证据：{revision}（{observed_at}）\n"));
            }
            if !target.gaps.is_empty() {
                out.push_str(&format!("      缺口：{}\n", target.gaps.join(", ")));
            }
        }
    }
    let gaps = view.gaps();
    if gaps.is_empty() {
        out.push_str("  缺口合计：无\n");
    } else {
        out.push_str(&format!("  缺口合计：{}\n", gaps.join(", ")));
    }
    if allowed.is_empty() {
        out.push_str("  允许动作：无（这个调用面上没有可用入口，视图不承诺做不到的事）\n");
    } else {
        out.push_str(&format!("  允许动作：{}\n", allowed.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "p1";

    fn target() -> TargetRow {
        TargetRow {
            target_id: "t1".to_string(),
            company_id: "c1".to_string(),
            tracking_id: "dt1".to_string(),
            customer_id: "acme".to_string(),
            environment: "prod".to_string(),
            deployment_unit: "web".to_string(),
            acceptance: "冒烟通过".to_string(),
            switch_state: SwitchState::Synced,
            population_known: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn observation(id: &str, kind: ObservationKind, at_ms: i64, digest: &str) -> ObservationRow {
        ObservationRow {
            observation_id: id.to_string(),
            company_id: "c1".to_string(),
            tracking_id: "dt1".to_string(),
            target_id: "t1".to_string(),
            kind,
            source_kind: if kind == ObservationKind::ManualEvidence {
                "manual"
            } else {
                "connector"
            },
            source_ref: "acme/web#318".to_string(),
            source_revision_id: None,
            artifact_revision: Some("rev-1".to_string()),
            digest: digest.to_string(),
            revision: format!("deploy-{id}"),
            note: "冒烟通过".to_string(),
            actor_id: "alice".to_string(),
            authorized: false,
            observed_at: format!("2026-11-21T00:00:{:02}Z", at_ms % 60),
            observed_at_ms: at_ms,
            recorded_at_ms: at_ms,
        }
    }

    /// 同一份观察，但版本号由调用方指定：冲突判据要用它分组。
    fn observation_at(
        id: &str,
        kind: ObservationKind,
        at_ms: i64,
        revision: &str,
        digest: &str,
    ) -> ObservationRow {
        let mut row = observation(id, kind, at_ms, digest);
        row.revision = revision.to_string();
        row
    }

    fn tracking() -> TrackingRow {
        TrackingRow {
            tracking_id: "dt1".to_string(),
            company_id: "c1".to_string(),
            project_id: PROJECT.to_string(),
            title: "客户 A 的修复".to_string(),
            requirement: "登录失败".to_string(),
            owner_id: "alice".to_string(),
            state: TrackingState::Open,
            issue_uid: None,
            run_uid: None,
            attempt_id: None,
            resource_version: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    /// 三个内部引用都为空也必须能成立——这是 D3 的核心断言，
    /// 不是「顺便测一下 Option」。
    #[test]
    fn tracking_item_stands_without_internal_run() {
        let row = tracking();
        assert!(!row.has_internal_anchor());

        let deployed = observation("o1", ObservationKind::Deployment, 100, "sha256:abc");
        let view = project_target(
            PROJECT,
            &target(),
            &[deployed],
            VerificationConclusion::Passed,
        );
        assert!(view.verdict.is_covered(), "{:?}", view.verdict);
        assert!(view.gaps.is_empty(), "{:?}", view.gaps);
    }

    /// 只有 PR 没有制品摘要：覆盖结论必须停在「缺少制品对应关系」，
    /// 且缺口码是稳定字符串。
    #[test]
    fn deployment_without_digest_is_not_covered() {
        let deployed = observation("o1", ObservationKind::Deployment, 100, "");
        let view = project_target(
            PROJECT,
            &target(),
            &[deployed],
            VerificationConclusion::Passed,
        );
        assert_eq!(view.gaps, vec![GAP_NO_ARTIFACT_MAPPING]);
        assert!(matches!(
            view.verdict,
            CoverageVerdict::PendingVerification { .. }
        ));
    }

    /// 回滚是新的观察：它把结论推回待核实，而不是把部署观察改判为失败。
    #[test]
    fn rollback_after_deployment_is_a_new_observation() {
        let deployed = observation("o1", ObservationKind::Deployment, 100, "sha256:abc");
        let rolled_back = observation("o2", ObservationKind::Rollback, 200, "");

        let view = project_target(
            PROJECT,
            &target(),
            &[deployed.clone(), rolled_back.clone()],
            VerificationConclusion::Passed,
        );
        assert!(matches!(
            view.verdict,
            CoverageVerdict::PendingVerification { .. }
        ));
        assert_eq!(view.gaps, vec![GAP_ROLLBACK_OBSERVED]);
        assert_eq!(view.evidence_revision.as_deref(), Some("o2"));

        // 回滚之后又部署了一次：较晚的部署说了算，且与切片顺序无关。
        let redeployed = observation("o3", ObservationKind::Deployment, 300, "sha256:abc");
        let view = project_target(
            PROJECT,
            &target(),
            &[redeployed, rolled_back, deployed],
            VerificationConclusion::Passed,
        );
        assert!(view.verdict.is_covered(), "{:?}", view.verdict);
        assert!(view.gaps.is_empty(), "{:?}", view.gaps);
        assert_eq!(view.evidence_revision.as_deref(), Some("o3"));
    }

    /// 时间完全相同时回滚胜出：后写下的事实优先。
    #[test]
    fn rollback_wins_ties() {
        let deployed = observation("o1", ObservationKind::Deployment, 100, "sha256:abc");
        let rolled_back = observation("o2", ObservationKind::Rollback, 100, "");
        let view = project_target(
            PROJECT,
            &target(),
            &[deployed, rolled_back],
            VerificationConclusion::Passed,
        );
        assert_eq!(view.evidence_revision.as_deref(), Some("o2"));
        assert_eq!(view.gaps, vec![GAP_ROLLBACK_OBSERVED]);
    }

    /// 未授权的人工补证不构成证据：仍然是待补安装，且缺口说清「未授权」。
    #[test]
    fn unauthorized_manual_evidence_stays_pending_install() {
        let mut manual = observation("o1", ObservationKind::ManualEvidence, 100, "sha256:abc");
        manual.authorized = false;
        let view = project_target(
            PROJECT,
            &target(),
            &[manual],
            VerificationConclusion::Passed,
        );
        assert!(matches!(
            view.verdict,
            CoverageVerdict::PendingInstall { .. }
        ));
        assert_eq!(
            view.gaps,
            vec![
                GAP_DEPLOYMENT_NOT_OBSERVED,
                GAP_INSTALL_RECEIPT_UNAUTHORIZED
            ]
        );
    }

    /// 授权实施人提交的补证可以支撑「已覆盖」，且证据来源标为人工。
    #[test]
    fn authorized_manual_evidence_counts_as_covered() {
        let mut manual = observation("o1", ObservationKind::ManualEvidence, 100, "sha256:abc");
        manual.authorized = true;
        let view = project_target(
            PROJECT,
            &target(),
            &[manual],
            VerificationConclusion::Passed,
        );
        match &view.verdict {
            CoverageVerdict::Covered { source } => {
                assert_eq!(source.as_str(), "manual-install", "来源必须标人工");
                match source {
                    CoverageSource::ManualInstall { by } => assert_eq!(by, "alice"),
                    other => panic!("来源应为人工安装：{other:?}"),
                }
            }
            other => panic!("应判定为已覆盖：{other:?}"),
        }
        assert!(view.gaps.is_empty(), "{:?}", view.gaps);
        assert_eq!(view.evidence_revision.as_deref(), Some("o1"));
    }

    /// 缺口列全、原因只给一条：两者不互为派生。
    #[test]
    fn gaps_list_everything_while_reason_stops_at_the_first_blocker() {
        let mut unknown_population = target();
        unknown_population.population_known = false;
        let deployed = observation("o1", ObservationKind::Deployment, 100, "sha256:abc");
        let view = project_target(
            PROJECT,
            &unknown_population,
            &[deployed],
            VerificationConclusion::Failed,
        );
        assert_eq!(
            view.gaps,
            vec![GAP_POPULATION_UNKNOWN, GAP_VERIFICATION_NOT_PASSED]
        );
        let reason = view.reason.expect("未覆盖必须有原因");
        assert!(reason.contains("人群"), "原因应停在人群未知：{reason}");
    }

    /// 两个目标各自独立判定：一个已覆盖不能让另一个跟着变成已覆盖。
    #[test]
    fn targets_are_judged_independently() {
        let deployed = observation("o1", ObservationKind::Deployment, 100, "sha256:abc");
        let mut other = target();
        other.target_id = "t2".to_string();
        other.customer_id = "beta".to_string();
        other.population_known = false;

        let covered = project_target(
            PROJECT,
            &target(),
            std::slice::from_ref(&deployed),
            VerificationConclusion::Passed,
        );
        let pending = project_target(PROJECT, &other, &[], VerificationConclusion::Unknown);
        assert!(covered.verdict.is_covered());
        assert!(!pending.verdict.is_covered());

        let view = TrackingView {
            tracking: tracking(),
            targets: vec![covered, pending],
            evidence: Vec::new(),
            work_packages: Vec::new(),
        };
        // 没有观察的目标同时缺三样：制品对应、人群、验证结论。
        assert_eq!(
            view.gaps(),
            vec![
                GAP_NO_ARTIFACT_MAPPING,
                GAP_POPULATION_UNKNOWN,
                GAP_VERIFICATION_NOT_PASSED
            ]
        );
    }

    /// 允许动作的候选码必须都能在路由表里找到对应路由——
    /// 打错一个字就会静默变成「不允许」，那是不会报错的失败。
    #[test]
    fn action_codes_are_registered_routes() {
        let view = TrackingView {
            tracking: tracking(),
            targets: vec![project_target(
                PROJECT,
                &target(),
                &[observation(
                    "o1",
                    ObservationKind::Deployment,
                    100,
                    "sha256:abc",
                )],
                VerificationConclusion::Passed,
            )],
            evidence: Vec::new(),
            work_packages: Vec::new(),
        };
        let candidates = view.action_candidates();
        assert!(!candidates.is_empty(), "已覆盖的目标至少有候选动作");
        for code in candidates {
            assert!(
                delivery_api::is_served_action(code).is_some(),
                "动作码 {code} 没有登记路由"
            );
        }
        // 「允许动作」不在这里判：它同时取决于调用面有没有入口、需求是否登记、
        // 当事人是否满足需求——三件都不在读模型的知识范围里。规则与用例在
        // `delivery_api` 的 `action_rules` 模块。读模型只回答「状态上说不说得通」。
    }

    /// 视图投影成契约响应体后，字段形状与 GET 的响应一致。
    #[test]
    fn view_projects_onto_the_frozen_contract() {
        let view = TrackingView {
            tracking: tracking(),
            targets: vec![project_target(
                PROJECT,
                &target(),
                &[observation(
                    "o1",
                    ObservationKind::Deployment,
                    100,
                    "sha256:abc",
                )],
                VerificationConclusion::Passed,
            )],
            evidence: vec![EvidenceStamp {
                revision: "o1".to_string(),
                source: "connector".to_string(),
                observed_at: "2026-11-21T00:01:40Z".to_string(),
            }],
            work_packages: Vec::new(),
        };
        let dto = view.to_dto(Vec::new()).expect("版本 1 应可投影");
        let rendered = serde_json::to_value(&dto).expect("应可序列化");
        for key in [
            "tracking_id",
            "project_id",
            "version",
            "targets",
            "evidence",
            "gaps",
            "allowed_actions",
        ] {
            assert!(rendered.get(key).is_some(), "缺少字段 {key}");
        }
        assert!(rendered.get("owner").is_some());
        assert_eq!(
            rendered["targets"][0]["verdict"],
            serde_json::json!("covered")
        );
        assert_eq!(rendered["version"], serde_json::json!(1));
    }

    /// 没有负责人时字段缺省，不用空串冒充「有人负责」。
    #[test]
    fn unassigned_owner_is_absent_not_empty() {
        let mut row = tracking();
        row.owner_id = String::new();
        let view = TrackingView {
            tracking: row,
            targets: Vec::new(),
            evidence: Vec::new(),
            work_packages: Vec::new(),
        };
        let dto = view.to_dto(Vec::new()).expect("应可投影");
        assert!(dto.owner.is_none());
    }

    /// 表外取值必须报错，不静默回退。
    #[test]
    fn out_of_table_values_are_rejected() {
        assert!(TrackingState::parse("archived").is_err());
        assert!(ObservationKind::parse("deploy").is_err());
        assert!(parse_source_kind("webhook").is_err());
        assert!(verification_conclusion("probably-fine").is_err());
    }

    /// 豁免不等于验过：`waived` 只能是 `Unknown`。
    #[test]
    fn waived_verification_is_not_passed() {
        assert_eq!(
            verification_conclusion("waived").unwrap(),
            VerificationConclusion::Unknown
        );
        assert_eq!(
            verification_conclusion("verified").unwrap(),
            VerificationConclusion::Passed
        );
    }

    // -------------------------------------------------------------------
    // 证据冲突（§11：保留两者并进入待核实，不自行选一份）
    // -------------------------------------------------------------------

    /// 同一版本两个摘要：保留两份证据，结论停在待核实，并说明冲突在哪。
    #[test]
    fn conflicting_digests_on_one_revision_stop_at_pending_verification() {
        let observations = vec![
            observation_at(
                "o1",
                ObservationKind::Deployment,
                100,
                "rev-1",
                "sha256:aaa",
            ),
            observation_at(
                "o2",
                ObservationKind::ManualEvidence,
                200,
                "rev-1",
                "sha256:bbb",
            ),
        ];
        let view = project_target(
            PROJECT,
            &target(),
            &observations,
            VerificationConclusion::Passed,
        );
        match &view.verdict {
            CoverageVerdict::PendingVerification { reason } => {
                assert!(reason.contains("rev-1"), "{reason}");
                assert!(reason.contains("sha256:aaa"), "{reason}");
                assert!(reason.contains("sha256:bbb"), "{reason}");
            }
            other => panic!("应为待核实：{other:?}"),
        }
        assert!(!view.verdict.is_covered());
        // 「保留两者」不是口号：时间线里两条都还在，一条都没被折叠。
        assert_eq!(evidence_stamps(&observations).len(), 2);
    }

    /// 新版本换摘要不是冲突：正常发版也会换内容，只比摘要会把发版判成事故。
    #[test]
    fn a_newer_revision_with_another_digest_is_not_a_conflict() {
        let observations = vec![
            observation_at(
                "o1",
                ObservationKind::Deployment,
                100,
                "rev-1",
                "sha256:aaa",
            ),
            observation_at(
                "o2",
                ObservationKind::Deployment,
                200,
                "rev-2",
                "sha256:bbb",
            ),
        ];
        let view = project_target(
            PROJECT,
            &target(),
            &observations,
            VerificationConclusion::Passed,
        );
        assert!(view.verdict.is_covered(), "{:?}", view.verdict);
    }

    /// 连接器与人工对同一版本给出同一摘要：一致，不制造冲突。
    #[test]
    fn matching_digests_from_two_sources_do_not_conflict() {
        let observations = vec![
            observation_at(
                "o1",
                ObservationKind::Deployment,
                100,
                "rev-1",
                "sha256:aaa",
            ),
            observation_at(
                "o2",
                ObservationKind::ManualEvidence,
                200,
                "rev-1",
                "sha256:aaa",
            ),
        ];
        let view = project_target(
            PROJECT,
            &target(),
            &observations,
            VerificationConclusion::Passed,
        );
        assert!(view.verdict.is_covered(), "{:?}", view.verdict);
    }
}
