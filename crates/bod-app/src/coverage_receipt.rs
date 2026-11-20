//! P4 任务 4.1—4.6：逐客户覆盖判定与可公开回执（`coverage-receipt`）。
//!
//! 这一层决定"能不能对客户说已修复"。四条线各自独立，合起来才成结论：
//! 1. **交付与响应分开**（4.1）：制品已提供 ≠ 客户环境已观察 ≠ 验收已通过；客户回不回复是
//!    另一条轴。只有三条都成立才能说 `Fixed`，否则只能说"已提供，待核实"。
//! 2. **覆盖元组**（4.2）：按项目/环境/部署单元（客户组）/制品摘要核对部署事实、验证结论与
//!    功能开关。人群未知、开关未同步、缺制品对应关系一律"待核实"；离线安装由授权实施人
//!    提交回执并标人工来源。
//! 3. **回执限域**（4.3）：外部用户只能看自己的回执；打开链接只记访问，不写确认；
//!    列表与计数不暴露别的客户——连"还有几条"都不给。
//! 4. **回滚与更正**（4.4）：回滚是新的部署观察，重新开启交付义务并向已答复者发更正；
//!    投递按键去重，渠道无幂等能力又查不证时标待核实。
//! 5. **证明页**（4.5）与**结算**（4.6）：四类结论分列，缺验收回执就是待核实；
//!    同一里程碑同一回执只结算一次，用量仍按实际发生计。
//!
//! 不发送任何通知、不存库：本模块只产出判定、投影与去重结果。

use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// 4.1 目标交付与客户响应
// ---------------------------------------------------------------------------

/// 目标环境的部署观察。`NotObserved` 与 `RolledBack` 都不是"已部署"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentObservation {
    NotObserved,
    Observed { revision: String, at: String },
    RolledBack { at: String },
}

impl DeploymentObservation {
    pub fn is_observed(&self) -> bool {
        matches!(self, Self::Observed { .. })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotObserved => "not-observed",
            Self::Observed { .. } => "observed",
            Self::RolledBack { .. } => "rolled-back",
        }
    }
}

/// 业务验收证据。与"验证报告通过"是两件事（4.5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptanceEvidence {
    Missing,
    Passed { receipt_id: String },
    Rejected { receipt_id: String },
}

impl AcceptanceEvidence {
    pub fn passed(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }
}

/// 交付推进的三个阶段。只能顺着走，回滚会把它退回去。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStage {
    ArtifactOnly,
    DeploymentObserved,
    Accepted,
}

impl DeliveryStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ArtifactOnly => "artifact-provided",
            Self::DeploymentObserved => "deployment-observed",
            Self::Accepted => "accepted",
        }
    }
}

/// 客户响应。`NoReply` 携带"打开过"的时间，但打开不等于回应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomerResponse {
    NoReply { seen_at: Option<String> },
    Confirmed { at: String, version: String },
    Disputed { at: String, reason: String },
}

impl CustomerResponse {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoReply { .. } => "no-reply",
            Self::Confirmed { .. } => "confirmed",
            Self::Disputed { .. } => "disputed",
        }
    }
}

/// 对外答复。只有 `Fixed` 是"已修复"，其余三种各有各的说法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageReply {
    Fixed,
    ProvidedPendingVerification,
    NotDelivered,
    AwaitingCustomer,
}

impl CoverageReply {
    pub fn text(&self) -> &'static str {
        match self {
            Self::Fixed => "已修复",
            Self::ProvidedPendingVerification => "修复版本已提供，客户环境待核实",
            Self::NotDelivered => "尚未交付",
            Self::AwaitingCustomer => "已交付，等待客户确认",
        }
    }

    pub fn is_fixed_claim(&self) -> bool {
        matches!(self, Self::Fixed)
    }
}

/// 一条交付记录：目标交付 + 客户响应，两条轴分开存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRecord {
    pub record_id: String,
    pub project_id: String,
    pub target_id: String,
    pub artifact_digest: String,
    /// 制品是否已经提供给该目标（构建完成、可下载/可安装）。
    pub artifact_provided: bool,
    pub deployment: DeploymentObservation,
    pub acceptance: AcceptanceEvidence,
    pub response: CustomerResponse,
    pub history: Vec<DeploymentObservation>,
}

impl DeliveryRecord {
    pub fn new(record_id: &str, project_id: &str, target_id: &str, artifact_digest: &str) -> Self {
        Self {
            record_id: record_id.to_string(),
            project_id: project_id.to_string(),
            target_id: target_id.to_string(),
            artifact_digest: artifact_digest.to_string(),
            artifact_provided: false,
            deployment: DeploymentObservation::NotObserved,
            acceptance: AcceptanceEvidence::Missing,
            response: CustomerResponse::NoReply { seen_at: None },
            history: Vec::new(),
        }
    }

    pub fn stage(&self) -> DeliveryStage {
        if !self.artifact_provided {
            return DeliveryStage::ArtifactOnly;
        }
        if !self.deployment.is_observed() {
            return DeliveryStage::ArtifactOnly;
        }
        if self.acceptance.passed() {
            DeliveryStage::Accepted
        } else {
            DeliveryStage::DeploymentObserved
        }
    }

    /// 给客户的答复。`Fixed` 要求：制品已提供 + 部署已观察 + 验收通过。
    pub fn reply(&self) -> CoverageReply {
        if !self.artifact_provided {
            return CoverageReply::NotDelivered;
        }
        match self.stage() {
            DeliveryStage::ArtifactOnly => CoverageReply::ProvidedPendingVerification,
            DeliveryStage::DeploymentObserved => CoverageReply::AwaitingCustomer,
            DeliveryStage::Accepted => match self.response {
                CustomerResponse::Confirmed { .. } => CoverageReply::Fixed,
                _ => CoverageReply::AwaitingCustomer,
            },
        }
    }

    /// 客户读过但没回应：只记访问时间，不改响应状态。
    pub fn record_visit(&mut self, at: &str) {
        if let CustomerResponse::NoReply { .. } = self.response {
            self.response = CustomerResponse::NoReply {
                seen_at: Some(at.to_string()),
            };
        }
    }

    pub fn respond(&mut self, response: CustomerResponse) {
        self.response = response;
    }
}

// ---------------------------------------------------------------------------
// 4.2 覆盖元组
// ---------------------------------------------------------------------------

/// 覆盖判定用的元组。缺一件就没法下结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageTuple {
    pub project_id: String,
    pub environment_id: String,
    /// 部署单元或客户组。
    pub unit_id: String,
    pub artifact_digest: String,
}

impl CoverageTuple {
    pub fn new(
        project_id: &str,
        environment_id: &str,
        unit_id: &str,
        artifact_digest: &str,
    ) -> Self {
        Self {
            project_id: project_id.to_string(),
            environment_id: environment_id.to_string(),
            unit_id: unit_id.to_string(),
            artifact_digest: artifact_digest.to_string(),
        }
    }

    /// 缺少制品对应关系：digest 为空就不是"这个包"。
    pub fn is_complete(&self) -> bool {
        !self.artifact_digest.trim().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchState {
    Synced,
    NotSynced,
    Unknown,
}

impl SwitchState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::NotSynced => "not-synced",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationConclusion {
    Passed,
    Failed,
    Unknown,
}

impl VerificationConclusion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// 离线安装回执。只有授权实施人提交的才算人工来源证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationReceipt {
    pub unit_id: String,
    pub by: String,
    pub at: String,
    pub authorized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageInput {
    pub tuple: CoverageTuple,
    pub deployment: DeploymentObservation,
    pub verification: VerificationConclusion,
    pub switch: SwitchState,
    /// 人群是否已知。未知人群不能给覆盖结论。
    pub population_known: bool,
    pub installation: Option<InstallationReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageSource {
    ObservedDeployment,
    ManualInstall { by: String },
}

impl CoverageSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ObservedDeployment => "observed-deployment",
            Self::ManualInstall { .. } => "manual-install",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageVerdict {
    Covered { source: CoverageSource },
    PendingVerification { reason: String },
    PendingInstall { reason: String },
}

impl CoverageVerdict {
    pub fn is_covered(&self) -> bool {
        matches!(self, Self::Covered { .. })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Covered { .. } => "covered",
            Self::PendingVerification { .. } => "pending-verification",
            Self::PendingInstall { .. } => "pending-install",
        }
    }
}

/// 判定单个单元是否覆盖。
///
/// 顺序是刻意的：先排除"根本判断不了"（缺制品、人群未知、开关未同步、验证没通过），
/// 再看部署事实，最后才认离线安装回执。
pub fn judge_coverage(input: &CoverageInput) -> CoverageVerdict {
    if !input.tuple.is_complete() {
        return CoverageVerdict::PendingVerification {
            reason: "缺少制品对应关系".to_string(),
        };
    }
    if !input.population_known {
        return CoverageVerdict::PendingVerification {
            reason: "受影响人群未知".to_string(),
        };
    }
    if input.switch != SwitchState::Synced {
        return CoverageVerdict::PendingVerification {
            reason: format!("功能开关状态为 {}", input.switch.as_str()),
        };
    }
    if input.verification != VerificationConclusion::Passed {
        return CoverageVerdict::PendingVerification {
            reason: format!("验证结论为 {}", input.verification.as_str()),
        };
    }
    match &input.deployment {
        DeploymentObservation::Observed { .. } => CoverageVerdict::Covered {
            source: CoverageSource::ObservedDeployment,
        },
        DeploymentObservation::RolledBack { .. } => CoverageVerdict::PendingVerification {
            reason: "目标环境已回滚".to_string(),
        },
        DeploymentObservation::NotObserved => match &input.installation {
            Some(receipt) if receipt.authorized => CoverageVerdict::Covered {
                source: CoverageSource::ManualInstall {
                    by: receipt.by.clone(),
                },
            },
            Some(receipt) => CoverageVerdict::PendingInstall {
                reason: format!("{} 提交的安装回执未获授权", receipt.by),
            },
            None => CoverageVerdict::PendingInstall {
                reason: "未观察到部署".to_string(),
            },
        },
    }
}

/// 一组单元的覆盖汇总。只对 `Covered` 的单元下结论，其余逐条给原因。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageSummary {
    pub covered: Vec<String>,
    pub pending: Vec<(String, String)>,
}

impl CoverageSummary {
    pub fn add(&mut self, unit_id: &str, verdict: &CoverageVerdict) {
        match verdict {
            CoverageVerdict::Covered { .. } => self.covered.push(unit_id.to_string()),
            CoverageVerdict::PendingVerification { reason }
            | CoverageVerdict::PendingInstall { reason } => {
                self.pending.push((unit_id.to_string(), reason.clone()))
            }
        }
    }

    pub fn covered_count(&self) -> usize {
        self.covered.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

// ---------------------------------------------------------------------------
// 4.3 回执投影与隔离
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptState {
    NoReply,
    Confirmed { version: String },
    Disputed { reason: String },
}

impl ReceiptState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoReply => "no-reply",
            Self::Confirmed { .. } => "confirmed",
            Self::Disputed { .. } => "disputed",
        }
    }
}

/// 回执。`internal_note` 不外发；`customer_id` 只用于限域判断，不进投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackReceipt {
    pub receipt_id: String,
    pub project_id: String,
    pub customer_id: String,
    pub target_version: String,
    pub environment_id: String,
    pub state: ReceiptState,
    pub responded_at: Option<String>,
    pub visits: Vec<String>,
    pub internal_note: String,
}

impl FeedbackReceipt {
    pub fn new(
        receipt_id: &str,
        project_id: &str,
        customer_id: &str,
        target_version: &str,
        environment_id: &str,
    ) -> Self {
        Self {
            receipt_id: receipt_id.to_string(),
            project_id: project_id.to_string(),
            customer_id: customer_id.to_string(),
            target_version: target_version.to_string(),
            environment_id: environment_id.to_string(),
            state: ReceiptState::NoReply,
            responded_at: None,
            visits: Vec::new(),
            internal_note: String::new(),
        }
    }
}

/// 高熵限域回执令牌：只能查它绑定的那一张回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptToken {
    pub receipt_id: String,
    pub customer_id: String,
    /// 熵位数。低于 128 位不算高熵，不能当凭据用。
    pub entropy_bits: u32,
    pub expires_at: String,
}

impl ReceiptToken {
    pub fn usable_at(&self, now: &str) -> bool {
        self.entropy_bits >= 128 && now <= self.expires_at.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Viewer {
    Customer { customer_id: String },
    Internal { staff_id: String },
}

/// 对外投影：不含内部备注、不含客户 id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedReceipt {
    pub receipt_id: String,
    pub target_version: String,
    pub environment_id: String,
    pub state: ReceiptState,
    pub responded_at: Option<String>,
    pub visit_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptAccess {
    Allowed(ProjectedReceipt),
    Denied,
}

fn project(receipt: &FeedbackReceipt) -> ProjectedReceipt {
    ProjectedReceipt {
        receipt_id: receipt.receipt_id.clone(),
        target_version: receipt.target_version.clone(),
        environment_id: receipt.environment_id.clone(),
        state: receipt.state.clone(),
        responded_at: receipt.responded_at.clone(),
        visit_count: receipt.visits.len(),
    }
}

/// 读一张回执。外部用户必须靠登录身份或限域令牌，否则拒绝。
pub fn read_receipt(
    receipt: &FeedbackReceipt,
    viewer: &Viewer,
    token: Option<&ReceiptToken>,
    now: &str,
) -> ReceiptAccess {
    match viewer {
        Viewer::Internal { .. } => ReceiptAccess::Allowed(project(receipt)),
        Viewer::Customer { customer_id } => {
            if *customer_id != receipt.customer_id {
                return ReceiptAccess::Denied;
            }
            if let Some(token) = token {
                let domain_ok = token.receipt_id == receipt.receipt_id
                    && token.customer_id == receipt.customer_id
                    && token.usable_at(now);
                if !domain_ok {
                    return ReceiptAccess::Denied;
                }
            }
            ReceiptAccess::Allowed(project(receipt))
        }
    }
}

/// 列表页。`total` 只等于本页可见条数——不暴露"还有几条别人的"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptPage {
    pub items: Vec<ProjectedReceipt>,
    pub total: usize,
}

/// 列出调用者可见的回执。外部用户只能看到自己的。
pub fn list_receipts(receipts: &[FeedbackReceipt], viewer: &Viewer) -> ReceiptPage {
    let items: Vec<ProjectedReceipt> = receipts
        .iter()
        .filter(|receipt| match viewer {
            Viewer::Internal { .. } => true,
            Viewer::Customer { customer_id } => *customer_id == receipt.customer_id,
        })
        .map(project)
        .collect();
    ReceiptPage {
        total: items.len(),
        items,
    }
}

/// 打开链接：只追加访问时间，状态一动都不动。
pub fn record_open<'a>(receipt: &'a mut FeedbackReceipt, at: &str) -> &'a ReceiptState {
    receipt.visits.push(at.to_string());
    &receipt.state
}

// ---------------------------------------------------------------------------
// 4.4 回滚与更正、投递去重
// ---------------------------------------------------------------------------

/// 投递键：同一件事发两次要能认出来。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeliveryKey {
    pub project_id: String,
    pub target_id: String,
    pub artifact_digest: String,
    pub channel: String,
    pub recipient: String,
}

impl DeliveryKey {
    pub fn new(
        project_id: &str,
        target_id: &str,
        artifact_digest: &str,
        channel: &str,
        recipient: &str,
    ) -> Self {
        Self {
            project_id: project_id.to_string(),
            target_id: target_id.to_string(),
            artifact_digest: artifact_digest.to_string(),
            channel: channel.to_string(),
            recipient: recipient.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Duplicate,
    /// 渠道没有幂等能力又查不证结果：只能标待核实。
    NeedsVerification {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationLedger {
    sent: BTreeSet<DeliveryKey>,
}

impl NotificationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.sent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sent.is_empty()
    }

    /// 发送一次。`channel_idempotent = false` 且 `verified = false` 时不给"已发送"。
    pub fn send(
        &mut self,
        key: DeliveryKey,
        channel_idempotent: bool,
        verified: bool,
    ) -> SendOutcome {
        if self.sent.contains(&key) {
            return SendOutcome::Duplicate;
        }
        if !channel_idempotent && !verified {
            return SendOutcome::NeedsVerification {
                reason: format!("渠道 {} 无幂等能力且无法查证结果", key.channel),
            };
        }
        self.sent.insert(key);
        SendOutcome::Sent
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    pub record_id: String,
    pub notice: String,
    pub reopens_obligation: bool,
    pub notify_customer: bool,
}

/// 回滚：追加部署观察、重开交付义务、只更正受影响的目标。
///
/// 已答复过的客户要发更正；没安装的保持待安装，历史确认保留原版本（`CustomerResponse`
/// 不被覆盖）。
pub fn record_rollback(record: &mut DeliveryRecord, at: &str) -> Correction {
    record.history.push(record.deployment.clone());
    record.deployment = DeploymentObservation::RolledBack { at: at.to_string() };
    record.acceptance = AcceptanceEvidence::Missing;
    let notify = matches!(
        record.response,
        CustomerResponse::Confirmed { .. } | CustomerResponse::Disputed { .. }
    );
    Correction {
        record_id: record.record_id.clone(),
        notice: format!(
            "{} 于 {at} 回滚，修复版本撤回；原答复版本保持不变，等待重新交付",
            record.target_id
        ),
        reopens_obligation: true,
        notify_customer: notify,
    }
}

// ---------------------------------------------------------------------------
// 4.5 证明页
// ---------------------------------------------------------------------------

/// 四种结论各自独立，不互相替代。验证通过不能顶替人工验收。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProofSection {
    DeliveryAndVerification,
    ManualAcceptance,
    EnvironmentCoverage,
    CustomerConfirmation,
}

impl ProofSection {
    pub const ALL: [Self; 4] = [
        Self::DeliveryAndVerification,
        Self::ManualAcceptance,
        Self::EnvironmentCoverage,
        Self::CustomerConfirmation,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DeliveryAndVerification => "delivery-and-verification",
            Self::ManualAcceptance => "manual-acceptance",
            Self::EnvironmentCoverage => "environment-coverage",
            Self::CustomerConfirmation => "customer-confirmation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofVerdict {
    Verified,
    Pending { reason: String },
    NotCovered { reason: String },
}

impl ProofVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Pending { .. } => "pending",
            Self::NotCovered { .. } => "not-covered",
        }
    }

    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofSectionView {
    pub section: ProofSection,
    pub verdict: ProofVerdict,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofPage {
    pub record_id: String,
    pub sections: Vec<ProofSectionView>,
}

impl ProofPage {
    pub fn section(&self, section: ProofSection) -> Option<&ProofSectionView> {
        self.sections.iter().find(|item| item.section == section)
    }

    /// 整体结论只有在四类都 `Verified` 时才成立；否则列出未定项。
    pub fn fully_proven(&self) -> bool {
        self.sections.iter().all(|item| item.verdict.is_verified())
    }

    pub fn pending_sections(&self) -> Vec<ProofSection> {
        self.sections
            .iter()
            .filter(|item| !item.verdict.is_verified())
            .map(|item| item.section)
            .collect()
    }
}

/// 构建证明页。`acceptance_receipt` 是人工验收依据，缺它就只能是待核实。
pub fn build_proof(
    record: &DeliveryRecord,
    coverage: &CoverageVerdict,
    acceptance_receipt: Option<&str>,
) -> ProofPage {
    let mut sections = Vec::new();

    sections.push(ProofSectionView {
        section: ProofSection::DeliveryAndVerification,
        verdict: match record.stage() {
            DeliveryStage::Accepted => ProofVerdict::Verified,
            DeliveryStage::DeploymentObserved => ProofVerdict::Pending {
                reason: "已观察部署，验收未通过".to_string(),
            },
            DeliveryStage::ArtifactOnly => ProofVerdict::Pending {
                reason: "只有制品，未观察目标部署".to_string(),
            },
        },
        detail: format!("交付阶段 {}", record.stage().as_str()),
    });

    sections.push(ProofSectionView {
        section: ProofSection::ManualAcceptance,
        verdict: match acceptance_receipt {
            Some(_) => ProofVerdict::Verified,
            None => ProofVerdict::Pending {
                reason: "缺少验收回执（AcceptanceReceipt）".to_string(),
            },
        },
        detail: match acceptance_receipt {
            Some(id) => format!("回执 {id}"),
            None => "验证通过不能替代验收通过".to_string(),
        },
    });

    sections.push(ProofSectionView {
        section: ProofSection::EnvironmentCoverage,
        verdict: match coverage {
            CoverageVerdict::Covered { .. } => ProofVerdict::Verified,
            CoverageVerdict::PendingVerification { reason } => ProofVerdict::Pending {
                reason: reason.clone(),
            },
            CoverageVerdict::PendingInstall { reason } => ProofVerdict::NotCovered {
                reason: reason.clone(),
            },
        },
        detail: coverage.as_str().to_string(),
    });

    sections.push(ProofSectionView {
        section: ProofSection::CustomerConfirmation,
        verdict: match &record.response {
            CustomerResponse::Confirmed { .. } => ProofVerdict::Verified,
            CustomerResponse::NoReply { seen_at } => ProofVerdict::Pending {
                reason: match seen_at {
                    Some(at) => format!("客户于 {at} 打开但未选择"),
                    None => "客户尚未回复".to_string(),
                },
            },
            CustomerResponse::Disputed { reason, .. } => ProofVerdict::Pending {
                reason: format!("客户提出异议：{reason}"),
            },
        },
        detail: record.response.as_str().to_string(),
    });

    ProofPage {
        record_id: record.record_id.clone(),
        sections,
    }
}

// ---------------------------------------------------------------------------
// 4.6 验收回执与结算
// ---------------------------------------------------------------------------

/// 验收回执：合同按验收触发付款时的依据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceReceipt {
    pub receipt_id: String,
    pub project_id: String,
    pub milestone_id: String,
    pub customer_id: String,
    pub artifact_digest: String,
    pub state: ReceiptState,
    pub accepted_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub settlement_id: String,
    pub milestone_id: String,
    pub receipt_id: String,
    pub amount_minor: i64,
    pub usage_basis: UsageBasis,
}

/// 用量口径：始终按实际发生，被拒结算也不回滚已发生用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageBasis {
    Actual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementOutcome {
    Created(Settlement),
    /// 同一里程碑同一回执已结算：返回原记录，不重复计酬。
    Existing(Settlement),
    /// 回执被争议/撤回/未确认：进复核，且用量照记。
    UnderReview {
        reason: String,
        usage_basis: UsageBasis,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettlementBook {
    by_key: BTreeMap<(String, String), Settlement>,
}

impl SettlementBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// 登记结算。键是（里程碑, 回执），重复提交返回原记录。
    pub fn settle(
        &mut self,
        receipt: &AcceptanceReceipt,
        settlement_id: &str,
        amount_minor: i64,
    ) -> SettlementOutcome {
        if !matches!(receipt.state, ReceiptState::Confirmed { .. }) {
            return SettlementOutcome::UnderReview {
                reason: format!(
                    "回执 {} 状态为 {}",
                    receipt.receipt_id,
                    receipt.state.as_str()
                ),
                usage_basis: UsageBasis::Actual,
            };
        }
        let key = (receipt.milestone_id.clone(), receipt.receipt_id.clone());
        if let Some(existing) = self.by_key.get(&key) {
            return SettlementOutcome::Existing(existing.clone());
        }
        let settlement = Settlement {
            settlement_id: settlement_id.to_string(),
            milestone_id: receipt.milestone_id.clone(),
            receipt_id: receipt.receipt_id.clone(),
            amount_minor,
            usage_basis: UsageBasis::Actual,
        };
        self.by_key.insert(key, settlement.clone());
        SettlementOutcome::Created(settlement)
    }
}

impl ReceiptState {
    pub fn confirmed(version: &str) -> Self {
        Self::Confirmed {
            version: version.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dg(seed: &str) -> String {
        format!(
            "sha256:{}",
            crate::domain::artifact::sha256_hex(seed.as_bytes())
        )
    }

    fn record() -> DeliveryRecord {
        let mut record = DeliveryRecord::new("rec-1", "proj-1", "cust-a", &dg("pkg"));
        record.artifact_provided = true;
        record
    }

    fn covered_input(unit: &str) -> CoverageInput {
        CoverageInput {
            tuple: CoverageTuple::new("proj-1", "env-a", unit, &dg("pkg")),
            deployment: DeploymentObservation::Observed {
                revision: "build-42".to_string(),
                at: "2026-11-21T10:00:00Z".to_string(),
            },
            verification: VerificationConclusion::Passed,
            switch: SwitchState::Synced,
            population_known: true,
            installation: None,
        }
    }

    fn receipt(customer: &str, id: &str) -> FeedbackReceipt {
        FeedbackReceipt::new(id, "proj-1", customer, "v1.2.0", "env-a")
    }

    /// 4.1：制品已提供但没观察到部署 → 「已提供，客户环境待核实」，不说已修复。
    #[test]
    fn artifact_without_observed_deployment_says_pending_verification() {
        let record = record();
        assert_eq!(record.reply(), CoverageReply::ProvidedPendingVerification);
        assert_eq!(record.reply().text(), "修复版本已提供，客户环境待核实");
        assert!(!record.reply().is_fixed_claim());

        let mut missing = DeliveryRecord::new("rec-0", "proj-1", "cust-a", &dg("pkg"));
        assert_eq!(missing.reply(), CoverageReply::NotDelivered);
        missing.artifact_provided = true;
        assert_eq!(missing.reply(), CoverageReply::ProvidedPendingVerification);
    }

    /// 4.1：已修复要求部署已观察 + 验收通过 + 客户确认三件齐。
    #[test]
    fn fixed_reply_requires_deployment_observation_and_acceptance() {
        let mut record = record();
        record.deployment = DeploymentObservation::Observed {
            revision: "build-42".to_string(),
            at: "2026-11-21T10:00:00Z".to_string(),
        };
        assert_eq!(record.stage(), DeliveryStage::DeploymentObserved);
        assert_eq!(
            record.reply(),
            CoverageReply::AwaitingCustomer,
            "验收未过不能算修复"
        );

        record.acceptance = AcceptanceEvidence::Passed {
            receipt_id: "acc-1".to_string(),
        };
        assert_eq!(record.stage(), DeliveryStage::Accepted);
        assert_eq!(
            record.reply(),
            CoverageReply::AwaitingCustomer,
            "客户没确认前也只是已交付"
        );

        record.respond(CustomerResponse::Confirmed {
            at: "2026-11-21T11:00:00Z".to_string(),
            version: "v1.2.0".to_string(),
        });
        assert_eq!(record.reply(), CoverageReply::Fixed);

        // 回滚之后不再是已修复。
        record_rollback(&mut record, "2026-11-21T12:00:00Z");
        assert_ne!(record.reply(), CoverageReply::Fixed);
    }

    /// 4.2：部分灰度只对已覆盖组下结论，其余待核实/待安装。
    #[test]
    fn partial_rollout_only_concludes_for_covered_groups() {
        let mut summary = CoverageSummary::default();
        summary.add("group-a", &judge_coverage(&covered_input("group-a")));

        let mut pending = covered_input("group-b");
        pending.deployment = DeploymentObservation::NotObserved;
        summary.add("group-b", &judge_coverage(&pending));

        let mut switched_off = covered_input("group-c");
        switched_off.switch = SwitchState::NotSynced;
        summary.add("group-c", &judge_coverage(&switched_off));

        assert_eq!(summary.covered, vec!["group-a".to_string()]);
        assert_eq!(summary.covered_count(), 1);
        assert_eq!(summary.pending_count(), 2);
        let b = summary
            .pending
            .iter()
            .find(|(unit, _)| unit == "group-b")
            .expect("b 应在待处理里");
        assert!(b.1.contains("未观察到部署"));
        let c = summary
            .pending
            .iter()
            .find(|(unit, _)| unit == "group-c")
            .expect("c 应在待处理里");
        assert!(c.1.contains("not-synced"));
    }

    /// 4.2：人群未知、开关未同步、验证未通过、缺制品关系都只能待核实。
    #[test]
    fn unsynced_switch_and_unknown_population_stay_pending() {
        let mut unknown_population = covered_input("group-a");
        unknown_population.population_known = false;
        match judge_coverage(&unknown_population) {
            CoverageVerdict::PendingVerification { reason } => {
                assert!(reason.contains("人群未知"))
            }
            other => panic!("人群未知应待核实，实际 {other:?}"),
        }

        let mut unknown_switch = covered_input("group-a");
        unknown_switch.switch = SwitchState::Unknown;
        assert!(matches!(
            judge_coverage(&unknown_switch),
            CoverageVerdict::PendingVerification { .. }
        ));

        let mut failed = covered_input("group-a");
        failed.verification = VerificationConclusion::Failed;
        match judge_coverage(&failed) {
            CoverageVerdict::PendingVerification { reason } => {
                assert!(reason.contains("failed"))
            }
            other => panic!("验证失败应待核实，实际 {other:?}"),
        }

        let mut no_pin = covered_input("group-a");
        no_pin.tuple.artifact_digest = String::new();
        match judge_coverage(&no_pin) {
            CoverageVerdict::PendingVerification { reason } => {
                assert!(reason.contains("制品对应关系"))
            }
            other => panic!("缺制品关系应待核实，实际 {other:?}"),
        }
    }

    /// 4.2：离线安装只认授权实施人的回执，并标人工来源。
    #[test]
    fn offline_install_receipt_is_manual_source() {
        let mut offline = covered_input("site-7");
        offline.deployment = DeploymentObservation::NotObserved;
        offline.installation = Some(InstallationReceipt {
            unit_id: "site-7".to_string(),
            by: "engineer-1".to_string(),
            at: "2026-11-21T10:00:00Z".to_string(),
            authorized: true,
        });
        match judge_coverage(&offline) {
            CoverageVerdict::Covered { source } => {
                assert_eq!(source.as_str(), "manual-install");
                match source {
                    CoverageSource::ManualInstall { by } => assert_eq!(by, "engineer-1"),
                    other => panic!("应为人工来源，实际 {other:?}"),
                }
            }
            other => panic!("授权安装回执应判覆盖，实际 {other:?}"),
        }

        offline.installation = Some(InstallationReceipt {
            unit_id: "site-7".to_string(),
            by: "intern-9".to_string(),
            at: "2026-11-21T10:00:00Z".to_string(),
            authorized: false,
        });
        match judge_coverage(&offline) {
            CoverageVerdict::PendingInstall { reason } => {
                assert!(reason.contains("intern-9"));
                assert!(reason.contains("未获授权"));
            }
            other => panic!("未授权回执不得判覆盖，实际 {other:?}"),
        }
    }

    /// 4.3 / AZ01：打开链接只记访问，不自动写确认。
    #[test]
    fn opening_a_receipt_link_does_not_confirm() {
        let mut receipt = receipt("cust-a", "rcp-1");
        let state = record_open(&mut receipt, "2026-11-21T10:00:00Z");
        assert_eq!(state, &ReceiptState::NoReply);
        assert_eq!(receipt.state, ReceiptState::NoReply, "打开不等于确认");
        assert_eq!(receipt.visits.len(), 1);
        assert_eq!(receipt.responded_at, None);

        record_open(&mut receipt, "2026-11-21T10:05:00Z");
        assert_eq!(receipt.visits.len(), 2);
        assert_eq!(receipt.state, ReceiptState::NoReply);
    }

    /// 4.3 / AZ01：客户 A 读不到 B 的回执；令牌必须限域且未过期。
    #[test]
    fn customer_cannot_read_another_customers_receipt() {
        let b = receipt("cust-b", "rcp-b");
        let a = Viewer::Customer {
            customer_id: "cust-a".to_string(),
        };
        assert_eq!(
            read_receipt(&b, &a, None, "2026-11-21T10:00:00Z"),
            ReceiptAccess::Denied
        );

        let token = ReceiptToken {
            receipt_id: "rcp-b".to_string(),
            customer_id: "cust-a".to_string(),
            entropy_bits: 160,
            expires_at: "2026-11-22T00:00:00Z".to_string(),
        };
        assert_eq!(
            read_receipt(&b, &a, Some(&token), "2026-11-21T10:00:00Z"),
            ReceiptAccess::Denied,
            "令牌绑定的是 A，不能读 B 的回执"
        );

        let own = receipt("cust-a", "rcp-a");
        let own_token = ReceiptToken {
            receipt_id: "rcp-a".to_string(),
            customer_id: "cust-a".to_string(),
            entropy_bits: 160,
            expires_at: "2026-11-22T00:00:00Z".to_string(),
        };
        match read_receipt(&own, &a, Some(&own_token), "2026-11-21T10:00:00Z") {
            ReceiptAccess::Allowed(projected) => {
                assert_eq!(projected.receipt_id, "rcp-a");
                assert_eq!(projected.visit_count, 0, "尚未打开过");
            }
            ReceiptAccess::Denied => panic!("自己的回执应可读"),
        }

        let expired = ReceiptToken {
            expires_at: "2026-11-20T00:00:00Z".to_string(),
            ..own_token
        };
        assert_eq!(
            read_receipt(&own, &a, Some(&expired), "2026-11-21T10:00:00Z"),
            ReceiptAccess::Denied
        );

        let weak = ReceiptToken {
            entropy_bits: 32,
            ..expired
        };
        assert_eq!(
            read_receipt(&own, &a, Some(&weak), "2026-11-21T10:00:00Z"),
            ReceiptAccess::Denied,
            "低熵令牌不能当凭据"
        );
    }

    /// 4.3 / AZ01：列表与计数不泄露其他客户。
    #[test]
    fn receipt_list_and_count_do_not_leak_other_customers() {
        let mut a1 = receipt("cust-a", "rcp-a1");
        a1.internal_note = "A 的内部备注".to_string();
        let receipts = vec![a1, receipt("cust-a", "rcp-a2"), receipt("cust-b", "rcp-b")];

        let viewer = Viewer::Customer {
            customer_id: "cust-a".to_string(),
        };
        let page = list_receipts(&receipts, &viewer);
        assert_eq!(page.total, 2, "只计自己可见的条数");
        assert_eq!(page.items.len(), 2);
        assert!(
            page.items
                .iter()
                .all(|item| !item.receipt_id.contains("rcp-b"))
        );

        let rendered = format!("{page:?}");
        assert!(!rendered.contains("cust-b"), "列表投影不得出现别的客户 id");
        assert!(!rendered.contains("rcp-b"));
        assert!(!rendered.contains("内部备注"), "内部备注不外发");

        let internal = list_receipts(
            &receipts,
            &Viewer::Internal {
                staff_id: "ops-1".to_string(),
            },
        );
        assert_eq!(internal.total, 3, "内部视图看全量");
    }

    /// 4.4：A 回滚而 B 未安装 → A 收到更正，B 保持待安装，历史确认保留原版本。
    #[test]
    fn rollback_reopens_obligation_and_corrects_only_affected_customer() {
        let mut a = record();
        a.deployment = DeploymentObservation::Observed {
            revision: "build-42".to_string(),
            at: "2026-11-21T10:00:00Z".to_string(),
        };
        a.respond(CustomerResponse::Confirmed {
            at: "2026-11-21T11:00:00Z".to_string(),
            version: "v1.2.0".to_string(),
        });

        let mut b = DeliveryRecord::new("rec-b", "proj-1", "cust-b", &dg("pkg"));
        b.artifact_provided = true;
        assert_eq!(
            b.reply(),
            CoverageReply::ProvidedPendingVerification,
            "B 未安装，只能说已提供待核实"
        );

        let correction = record_rollback(&mut a, "2026-11-21T12:00:00Z");
        assert!(correction.reopens_obligation);
        assert!(correction.notify_customer, "已答复过的才发更正");
        assert!(correction.notice.contains("回滚"));
        assert_eq!(a.deployment.as_str(), "rolled-back");
        assert_eq!(a.history.len(), 1, "回滚是新的部署观察，旧的进历史");
        assert_eq!(
            a.response,
            CustomerResponse::Confirmed {
                at: "2026-11-21T11:00:00Z".to_string(),
                version: "v1.2.0".to_string()
            },
            "历史确认保留原版本"
        );
        assert_ne!(a.reply(), CoverageReply::Fixed);

        let b_correction = record_rollback(&mut b, "2026-11-21T12:00:00Z");
        assert!(
            !b_correction.notify_customer,
            "从未答复的客户不需要更正通知"
        );
    }

    /// 4.4：投递键去重，重复发送只认一次。
    #[test]
    fn delivery_key_deduplicates_notifications() {
        let mut ledger = NotificationLedger::new();
        let key = DeliveryKey::new("proj-1", "cust-a", &dg("pkg"), "email", "a@example.com");
        assert_eq!(ledger.send(key.clone(), true, false), SendOutcome::Sent);
        assert_eq!(
            ledger.send(key.clone(), true, false),
            SendOutcome::Duplicate
        );
        assert_eq!(ledger.len(), 1);

        let other_channel =
            DeliveryKey::new("proj-1", "cust-a", &dg("pkg"), "sms", "a@example.com");
        assert_eq!(ledger.send(other_channel, true, false), SendOutcome::Sent);
        assert_eq!(ledger.len(), 2);
        assert!(!ledger.is_empty());
    }

    /// 4.4：渠道无幂等能力又查不证 → 标待核实，不假装已发送。
    #[test]
    fn non_idempotent_channel_without_verification_is_marked_pending() {
        let mut ledger = NotificationLedger::new();
        let key = DeliveryKey::new("proj-1", "cust-a", &dg("pkg"), "webhook", "hook-1");
        match ledger.send(key.clone(), false, false) {
            SendOutcome::NeedsVerification { reason } => {
                assert!(reason.contains("webhook"));
                assert!(reason.contains("查证"));
            }
            other => panic!("应标待核实，实际 {other:?}"),
        }
        assert!(ledger.is_empty(), "待核实不计入已发送");

        assert_eq!(ledger.send(key, false, true), SendOutcome::Sent);
        assert_eq!(ledger.len(), 1);
    }

    /// 4.5：缺 AcceptanceReceipt 时人工验收项待核实，不拿验证通过顶替。
    #[test]
    fn proof_page_keeps_manual_acceptance_pending_without_receipt() {
        let mut record = record();
        record.deployment = DeploymentObservation::Observed {
            revision: "build-42".to_string(),
            at: "2026-11-21T10:00:00Z".to_string(),
        };
        record.acceptance = AcceptanceEvidence::Passed {
            receipt_id: "acc-1".to_string(),
        };
        let proof = build_proof(&record, &judge_coverage(&covered_input("cust-a")), None);

        let manual = proof.section(ProofSection::ManualAcceptance).unwrap();
        assert_eq!(manual.verdict.as_str(), "pending");
        match &manual.verdict {
            ProofVerdict::Pending { reason } => {
                assert!(reason.contains("缺少验收回执"));
            }
            other => panic!("应为待核实，实际 {other:?}"),
        }
        assert!(manual.detail.contains("不能替代"));
        assert!(!proof.fully_proven(), "缺人工验收就不能说完全证明");

        let with_receipt = build_proof(
            &record,
            &judge_coverage(&covered_input("cust-a")),
            Some("acc-1"),
        );
        assert!(
            with_receipt
                .section(ProofSection::ManualAcceptance)
                .unwrap()
                .verdict
                .is_verified()
        );
    }

    /// 4.5：四类结论分列，缺哪类就列哪类，不合并成"通过"。
    #[test]
    fn proof_page_shows_four_sections_separately() {
        let record = record();
        let pending = judge_coverage(&{
            let mut input = covered_input("cust-a");
            input.deployment = DeploymentObservation::NotObserved;
            input
        });
        let proof = build_proof(&record, &pending, None);
        let order: Vec<ProofSection> = proof.sections.iter().map(|item| item.section).collect();
        assert_eq!(order, ProofSection::ALL.to_vec());
        assert_eq!(proof.sections.len(), 4);
        assert_eq!(proof.pending_sections().len(), 4, "四类都缺：只有制品");
        match &proof
            .section(ProofSection::EnvironmentCoverage)
            .unwrap()
            .verdict
        {
            ProofVerdict::NotCovered { reason } => assert!(reason.contains("未观察到部署")),
            other => panic!("未安装应为 not-covered，实际 {other:?}"),
        }
    }

    /// 4.6：同一里程碑同一回执重复提交只记一次。
    #[test]
    fn repeated_settlement_of_same_receipt_is_idempotent() {
        let receipt = AcceptanceReceipt {
            receipt_id: "acc-1".to_string(),
            project_id: "proj-1".to_string(),
            milestone_id: "ms-1".to_string(),
            customer_id: "cust-a".to_string(),
            artifact_digest: dg("pkg"),
            state: ReceiptState::confirmed("v1.2.0"),
            accepted_at: "2026-11-21T11:00:00Z".to_string(),
        };
        let mut book = SettlementBook::new();
        let first = book.settle(&receipt, "stl-1", 120_000);
        match &first {
            SettlementOutcome::Created(settlement) => {
                assert_eq!(settlement.amount_minor, 120_000);
                assert_eq!(settlement.usage_basis, UsageBasis::Actual);
            }
            other => panic!("首次应落账，实际 {other:?}"),
        }

        match book.settle(&receipt, "stl-2", 120_000) {
            SettlementOutcome::Existing(settlement) => {
                assert_eq!(settlement.settlement_id, "stl-1", "返回原结算记录");
            }
            other => panic!("重复提交应返回原记录，实际 {other:?}"),
        }
        assert_eq!(book.len(), 1, "不重复计酬");
    }

    /// 4.6：争议/撤回回执进复核，用量仍按实际发生计。
    #[test]
    fn disputed_receipt_goes_to_review_and_usage_still_counts() {
        let receipt = AcceptanceReceipt {
            receipt_id: "acc-2".to_string(),
            project_id: "proj-1".to_string(),
            milestone_id: "ms-2".to_string(),
            customer_id: "cust-a".to_string(),
            artifact_digest: dg("pkg"),
            state: ReceiptState::Disputed {
                reason: "覆盖未核实".to_string(),
            },
            accepted_at: "2026-11-21T11:00:00Z".to_string(),
        };
        let mut book = SettlementBook::new();
        match book.settle(&receipt, "stl-3", 80_000) {
            SettlementOutcome::UnderReview {
                reason,
                usage_basis,
            } => {
                assert!(reason.contains("disputed"));
                assert_eq!(usage_basis, UsageBasis::Actual, "被拒结算不代表用量不算");
            }
            other => panic!("争议回执应进复核，实际 {other:?}"),
        }
        assert!(book.is_empty(), "复核前不落结算");
    }
}
