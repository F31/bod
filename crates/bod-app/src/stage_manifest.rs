//! P4 任务 2.1—2.5：阶段清单、发布记录与证据包（`stage-manifest`）。
//!
//! 这一层处理的都是"对外说的话"：一份交付引用哪个修订、发布算不算发出去了、
//! 证据包给谁看什么、重打一次包还是不是同一份内容。四件事共同点是**不能靠名称和字面**：
//! 1. **冻结修订**（2.1）：Manifest 钉住具体 Artifact Revision 与 digest，改一次出一个修订；
//!    对外只引用确定修订，`latest`、`*`、制品名都不算引用（`invariant_i_07`）。
//! 2. **权威指针**（2.1）：propose 必须带 `base_revision`；指针变了就 Conflict，
//!    候选留着但不覆盖；外部内容变化只登记为待复核，不悄悄改已冻结清单。
//! 3. **发布与验收分开**（2.2）：通知发出去了不等于客户能用；签发者的声明要宿主验证过才算数。
//! 4. **三视图与字段级披露**（2.3）：内部讨论、成本明细、任何凭据都进不了客户视图；
//!    原始密钥在**任何**视图里都不出现。
//! 5. **身份与完整性分开**（2.4、2.5）：内容清单身份与运输文件身份分开；自签包过了摘要校验
//!    也只证明包内一致，外部身份仍未建立信任（`invariant_i_52`、G-33）。
//!
//! 不读写文件系统、不碰 ZIP/PDF 编解码：内容清单是路径 + digest + 长度的值类型，
//! 运输文件只给 digest。真正的打包与签名由宿主完成。

use crate::domain::artifact::sha256_hex;
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// 2.1 冻结修订与权威指针
// ---------------------------------------------------------------------------

/// 一条制品钉扎：具体修订 + 制品 digest。名称不参与身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPin {
    pub artifact_id: String,
    pub revision: String,
    pub digest: String,
}

impl ArtifactPin {
    pub fn new(artifact_id: &str, revision: &str, digest: &str) -> Self {
        Self {
            artifact_id: artifact_id.to_string(),
            revision: revision.to_string(),
            digest: digest.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    MissingRevision {
        artifact_id: String,
    },
    /// `latest`/`*`/制品名：都不是 digest。
    FloatingReference {
        artifact_id: String,
        digest: String,
    },
}

impl fmt::Display for PinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRevision { artifact_id } => {
                write!(formatter, "制品 `{artifact_id}` 未钉住修订")
            }
            Self::FloatingReference {
                artifact_id,
                digest,
            } => write!(
                formatter,
                "制品 `{artifact_id}` 的引用 `{digest}` 不是 digest：不允许浮动最新版本或用名称代替"
            ),
        }
    }
}

impl std::error::Error for PinError {}

/// 只认 `sha256:<64 hex>` 或裸 64 hex。别的写法一律当浮动引用。
fn is_digest(value: &str) -> bool {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn validate_pins(pins: &[ArtifactPin]) -> Result<(), ManifestError> {
    if pins.is_empty() {
        return Err(ManifestError::NoPins);
    }
    for pin in pins {
        if pin.revision.trim().is_empty() {
            return Err(ManifestError::BadPin(PinError::MissingRevision {
                artifact_id: pin.artifact_id.clone(),
            }));
        }
        if !is_digest(&pin.digest) {
            return Err(ManifestError::BadPin(PinError::FloatingReference {
                artifact_id: pin.artifact_id.clone(),
                digest: pin.digest.clone(),
            }));
        }
    }
    Ok(())
}

/// 修订状态。外部内容变化产生的是 `PendingReview`，不是悄悄变成新基线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionState {
    Frozen,
    PendingReview,
}

impl RevisionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Frozen => "frozen",
            Self::PendingReview => "pending-review",
        }
    }
}

/// 一个 Manifest 修订。字段不可变：改内容只能出新修订。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRevision {
    pub manifest_id: String,
    pub revision: String,
    pub base: Option<String>,
    pub artifacts: Vec<ArtifactPin>,
    pub acceptances: Vec<String>,
    pub signatures: Vec<String>,
    pub state: RevisionState,
    pub note: String,
}

/// propose 的输入。`base_revision` 是乐观并发的那把锁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDraft {
    pub manifest_id: String,
    pub base_revision: Option<String>,
    pub artifacts: Vec<ArtifactPin>,
    pub acceptances: Vec<String>,
    pub signatures: Vec<String>,
    pub note: String,
}

impl ManifestDraft {
    pub fn new(manifest_id: &str, base_revision: Option<&str>) -> Self {
        Self {
            manifest_id: manifest_id.to_string(),
            base_revision: base_revision.map(|item| item.to_string()),
            artifacts: Vec::new(),
            acceptances: Vec::new(),
            signatures: Vec::new(),
            note: String::new(),
        }
    }

    pub fn with_artifact(mut self, pin: ArtifactPin) -> Self {
        self.artifacts.push(pin);
        self
    }

    pub fn with_acceptance(mut self, acceptance_id: &str) -> Self {
        self.acceptances.push(acceptance_id.to_string());
        self
    }

    pub fn with_signature(mut self, signature_ref: &str) -> Self {
        self.signatures.push(signature_ref.to_string());
        self
    }

    pub fn with_note(mut self, note: &str) -> Self {
        self.note = note.to_string();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    NoPins,
    BadPin(PinError),
    UnknownManifest {
        manifest_id: String,
    },
    UnknownRevision {
        manifest_id: String,
        revision: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPins => write!(formatter, "Manifest 至少要钉住一个制品修订"),
            Self::BadPin(err) => write!(formatter, "{err}"),
            Self::UnknownManifest { manifest_id } => {
                write!(formatter, "未知 Manifest `{manifest_id}`")
            }
            Self::UnknownRevision {
                manifest_id,
                revision,
            } => write!(formatter, "Manifest `{manifest_id}` 没有修订 `{revision}`"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// propose 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeOutcome {
    Accepted {
        revision: String,
    },
    /// 指针已变：返回 Conflict，候选保留但不动接受指针。
    Conflict {
        current: Option<String>,
        base: Option<String>,
        candidate: String,
    },
    Rejected(ManifestError),
}

/// 对外交付引用：确定修订，不是"最新"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRef {
    pub manifest_id: String,
    pub revision: String,
}

impl DeliveryRef {
    pub fn new(manifest_id: &str, revision: &str) -> Self {
        Self {
            manifest_id: manifest_id.to_string(),
            revision: revision.to_string(),
        }
    }
}

/// 冲突后保留的候选。要么显式接受，要么重提，不自动落地。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub draft: ManifestDraft,
    pub revision: String,
}

/// Manifest 台账：修订序列 + 每个 Manifest 的接受指针。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestBook {
    revisions: Vec<ManifestRevision>,
    accepted: BTreeMap<String, String>,
    candidates: Vec<Candidate>,
}

impl ManifestBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accepted_revision(&self, manifest_id: &str) -> Option<&str> {
        self.accepted.get(manifest_id).map(|item| item.as_str())
    }

    pub fn revision(&self, manifest_id: &str, revision: &str) -> Option<&ManifestRevision> {
        self.revisions
            .iter()
            .find(|item| item.manifest_id == manifest_id && item.revision == revision)
    }

    pub fn revisions_of(&self, manifest_id: &str) -> Vec<&ManifestRevision> {
        self.revisions
            .iter()
            .filter(|item| item.manifest_id == manifest_id)
            .collect()
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// 待复核的外部变化（`PendingReview`），不参与对外引用。
    pub fn pending_review(&self, manifest_id: &str) -> Vec<&ManifestRevision> {
        self.revisions
            .iter()
            .filter(|item| {
                item.manifest_id == manifest_id && item.state == RevisionState::PendingReview
            })
            .collect()
    }

    /// 解析对外交付引用。旧引用永远解析到它当初指向的那个修订。
    pub fn resolve(&self, reference: &DeliveryRef) -> Option<&ManifestRevision> {
        self.revision(&reference.manifest_id, &reference.revision)
    }

    /// 提议一个新修订。`base_revision` 与当前接受指针不一致即 Conflict。
    pub fn propose(&mut self, draft: ManifestDraft, next_revision: &str) -> ProposeOutcome {
        if let Err(err) = validate_pins(&draft.artifacts) {
            return ProposeOutcome::Rejected(err);
        }
        let current = self.accepted.get(&draft.manifest_id).cloned();
        if draft.base_revision != current {
            self.candidates.push(Candidate {
                draft: draft.clone(),
                revision: next_revision.to_string(),
            });
            return ProposeOutcome::Conflict {
                current,
                base: draft.base_revision,
                candidate: next_revision.to_string(),
            };
        }
        self.revisions.push(ManifestRevision {
            manifest_id: draft.manifest_id.clone(),
            revision: next_revision.to_string(),
            base: draft.base_revision,
            artifacts: draft.artifacts,
            acceptances: draft.acceptances,
            signatures: draft.signatures,
            state: RevisionState::Frozen,
            note: draft.note,
        });
        self.accepted
            .insert(draft.manifest_id, next_revision.to_string());
        ProposeOutcome::Accepted {
            revision: next_revision.to_string(),
        }
    }

    /// 冲突候选的人工收口：把保留的候选提升为接受修订。
    pub fn accept_candidate(
        &mut self,
        manifest_id: &str,
        revision: &str,
    ) -> Result<(), ManifestError> {
        let Some(index) = self
            .candidates
            .iter()
            .position(|item| item.draft.manifest_id == manifest_id && item.revision == revision)
        else {
            return Err(ManifestError::UnknownRevision {
                manifest_id: manifest_id.to_string(),
                revision: revision.to_string(),
            });
        };
        let candidate = self.candidates.remove(index);
        self.revisions.push(ManifestRevision {
            manifest_id: candidate.draft.manifest_id.clone(),
            revision: candidate.revision.clone(),
            base: candidate.draft.base_revision,
            artifacts: candidate.draft.artifacts,
            acceptances: candidate.draft.acceptances,
            signatures: candidate.draft.signatures,
            state: RevisionState::Frozen,
            note: candidate.draft.note,
        });
        self.accepted
            .insert(manifest_id.to_string(), revision.to_string());
        Ok(())
    }

    /// 外部内容变化：登记一个新修订并标 `PendingReview`，**不**移动接受指针。
    pub fn note_external_change(
        &mut self,
        manifest_id: &str,
        description: &str,
        next_revision: &str,
    ) -> ProposeOutcome {
        let Some(current) = self.accepted.get(manifest_id).cloned() else {
            return ProposeOutcome::Rejected(ManifestError::UnknownManifest {
                manifest_id: manifest_id.to_string(),
            });
        };
        let Some(base) = self.revision(manifest_id, &current).cloned() else {
            return ProposeOutcome::Rejected(ManifestError::UnknownRevision {
                manifest_id: manifest_id.to_string(),
                revision: current,
            });
        };
        self.revisions.push(ManifestRevision {
            manifest_id: manifest_id.to_string(),
            revision: next_revision.to_string(),
            base: Some(base.revision),
            artifacts: base.artifacts,
            acceptances: base.acceptances,
            signatures: base.signatures,
            state: RevisionState::PendingReview,
            note: format!("外部内容变化：{description}"),
        });
        ProposeOutcome::Accepted {
            revision: next_revision.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// 2.2 发布记录
// ---------------------------------------------------------------------------

/// 谁在声明这个状态。签发者与连接器都只是声明方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declarer {
    Host,
    Signer { id: String },
    Connector { id: String },
}

impl Declarer {
    pub fn describe(&self) -> String {
        match self {
            Self::Host => "host".to_string(),
            Self::Signer { id } => format!("signer:{id}"),
            Self::Connector { id } => format!("connector:{id}"),
        }
    }

    pub fn is_host(&self) -> bool {
        matches!(self, Self::Host)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimedState {
    Prepared,
    Dispatched,
    Delivered,
    Failed,
    Unknown,
}

impl ClaimedState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Dispatched => "dispatched",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// 宿主验证后的状态。未验证的声明只能算 `Unconfirmed`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveState {
    Confirmed(ClaimedState),
    Unconfirmed(ClaimedState),
}

impl EffectiveState {
    pub fn is_confirmed_delivery(&self) -> bool {
        matches!(self, Self::Confirmed(ClaimedState::Delivered))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalObservation {
    pub claimed: ClaimedState,
    pub observed_at: String,
    pub declarer: Declarer,
    /// 宿主是否自己核对过（例如读回外部标识、回查发布记录）。
    pub host_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub publication_id: String,
    pub manifest_id: String,
    pub revision: String,
    pub target: String,
    pub external_id: Option<String>,
    pub effect_id: Option<String>,
    pub observation: Option<ExternalObservation>,
}

impl Publication {
    pub fn new(publication_id: &str, reference: &DeliveryRef, target: &str) -> Self {
        Self {
            publication_id: publication_id.to_string(),
            manifest_id: reference.manifest_id.clone(),
            revision: reference.revision.clone(),
            target: target.to_string(),
            external_id: None,
            effect_id: None,
            observation: None,
        }
    }

    pub fn observe(mut self, observation: ExternalObservation) -> Self {
        self.observation = Some(observation);
        self
    }

    /// 对外可见的发布状态。声明方不是宿主且未被核对 → `Unconfirmed`。
    pub fn effective(&self) -> EffectiveState {
        let Some(observation) = &self.observation else {
            return EffectiveState::Unconfirmed(ClaimedState::Prepared);
        };
        let trusted = observation.host_verified || observation.declarer.is_host();
        if trusted {
            EffectiveState::Confirmed(observation.claimed)
        } else {
            EffectiveState::Unconfirmed(observation.claimed)
        }
    }

    pub fn observed_at(&self) -> Option<&str> {
        self.observation
            .as_ref()
            .map(|item| item.observed_at.as_str())
    }
}

/// 投递结论。只有宿主确认过的 Delivered 才算发出去了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    NotRequested,
    Unconfirmed(ClaimedState),
    Delivered,
    Failed,
}

impl DeliveryState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotRequested => "not-requested",
            Self::Unconfirmed(_) => "unconfirmed",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }
}

pub fn delivery_state(publication: &Publication) -> DeliveryState {
    // 没有任何外部观测：请求都还没发出去，既不是已投递也不是失败。
    if publication.observation.is_none() {
        return DeliveryState::NotRequested;
    }
    match publication.effective() {
        EffectiveState::Confirmed(ClaimedState::Delivered) => DeliveryState::Delivered,
        EffectiveState::Confirmed(ClaimedState::Failed) => DeliveryState::Failed,
        EffectiveState::Confirmed(ClaimedState::Prepared) => DeliveryState::NotRequested,
        EffectiveState::Confirmed(ClaimedState::Unknown) => {
            DeliveryState::Unconfirmed(ClaimedState::Unknown)
        }
        EffectiveState::Confirmed(ClaimedState::Dispatched) => {
            DeliveryState::Unconfirmed(ClaimedState::Dispatched)
        }
        EffectiveState::Unconfirmed(claimed) => DeliveryState::Unconfirmed(claimed),
    }
}

/// 业务验收：与发布状态彻底分开的一条轴。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceState {
    NotPassed,
    Passed,
    Rejected,
}

/// 给客户看的结论。`Fixed` 只在"已投递 + 验收通过"时出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomerConclusion {
    Fixed,
    DeliveredPendingAcceptance,
    NotDelivered,
    Unknown,
}

impl CustomerConclusion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::DeliveredPendingAcceptance => "delivered-pending-acceptance",
            Self::NotDelivered => "not-delivered",
            Self::Unknown => "unknown",
        }
    }
}

pub fn customer_conclusion(
    delivery: DeliveryState,
    acceptance: AcceptanceState,
) -> CustomerConclusion {
    match (delivery, acceptance) {
        (DeliveryState::Delivered, AcceptanceState::Passed) => CustomerConclusion::Fixed,
        (DeliveryState::Delivered, _) => CustomerConclusion::DeliveredPendingAcceptance,
        (DeliveryState::Failed, _) => CustomerConclusion::NotDelivered,
        (DeliveryState::NotRequested, AcceptanceState::Passed) => CustomerConclusion::Unknown,
        (DeliveryState::NotRequested, _) => CustomerConclusion::NotDelivered,
        (DeliveryState::Unconfirmed(_), _) => CustomerConclusion::Unknown,
    }
}

// ---------------------------------------------------------------------------
// 2.3 证据包与三视图
// ---------------------------------------------------------------------------

/// 字段类别决定"谁能看见"，与字段名无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClass {
    /// 结论。
    Conclusion,
    /// 引用（清单、产物 digest、签名、授权索引）。
    Reference,
    /// 时间线事件。
    Timeline,
    /// 内部讨论。
    InternalDiscussion,
    /// 默认成本明细。
    CostDetail,
    /// 凭据。任何视图都不给。
    Credential,
    /// 来源说明与复验方法。
    Provenance,
}

impl FieldClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conclusion => "conclusion",
            Self::Reference => "reference",
            Self::Timeline => "timeline",
            Self::InternalDiscussion => "internal-discussion",
            Self::CostDetail => "cost-detail",
            Self::Credential => "credential",
            Self::Provenance => "provenance",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceField {
    pub name: String,
    pub class: FieldClass,
    pub value: String,
}

impl EvidenceField {
    pub fn new(name: &str, class: FieldClass, value: &str) -> Self {
        Self {
            name: name.to_string(),
            class,
            value: value.to_string(),
        }
    }
}

/// 三种视图。`Customer` 拿不到内部讨论、成本明细与时间线细节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Internal,
    Customer,
    Audit,
}

impl ViewKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Customer => "customer",
            Self::Audit => "audit",
        }
    }

    fn admits(&self, class: FieldClass) -> bool {
        match self {
            Self::Internal => class != FieldClass::Credential,
            Self::Audit => !matches!(
                class,
                FieldClass::Credential | FieldClass::InternalDiscussion | FieldClass::CostDetail
            ),
            Self::Customer => matches!(
                class,
                FieldClass::Conclusion | FieldClass::Reference | FieldClass::Provenance
            ),
        }
    }
}

/// 被扣下的内容如何披露。客户视图只给计数：说出"有一条内部讨论被隐藏"本身就是泄露。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithheldDisclosure {
    Names(Vec<String>),
    Count(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedView {
    pub view: ViewKind,
    pub fields: Vec<EvidenceField>,
    pub withheld: WithheldDisclosure,
}

impl ProjectedView {
    pub fn value_of(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|item| item.name == name)
            .map(|item| item.value.as_str())
    }
}

/// 形态识别：视图不能因为"类别标对了"就把凭据放出去。
fn looks_like_credential(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("begin private key")
        || value.contains("begin rsa private key")
        || value.contains("bearer ")
        || value.contains("api_key=")
        || value.contains("password=")
        || value.contains("secret=")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    pub at: String,
    pub what: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRef {
    pub check_id: String,
    pub outcome: String,
    pub artifact_id: String,
    pub raw_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureRef {
    pub signer: String,
    pub covers: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceRef {
    pub acceptance_id: String,
    pub decision: String,
}

/// 证据包。字段集就是规格里列的九项：清单、摘要、授权索引、时间线、产物引用、
/// 测试证据、来源说明、复验方法与签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidencePackage {
    pub manifest: DeliveryRef,
    pub summary: String,
    pub authorization_index: Vec<String>,
    pub timeline: Vec<TimelineEvent>,
    pub artifacts: Vec<ArtifactRef>,
    pub verification: Vec<VerificationRef>,
    pub provenance: String,
    pub revalidation: Vec<String>,
    pub signatures: Vec<SignatureRef>,
    pub acceptance: Option<AcceptanceRef>,
    /// 内部材料（讨论、成本明细）。默认不进任何对外视图。
    pub extras: Vec<EvidenceField>,
}

impl EvidencePackage {
    pub fn new(reference: DeliveryRef, summary: &str) -> Self {
        Self {
            manifest: reference,
            summary: summary.to_string(),
            authorization_index: Vec::new(),
            timeline: Vec::new(),
            artifacts: Vec::new(),
            verification: Vec::new(),
            provenance: String::new(),
            revalidation: Vec::new(),
            signatures: Vec::new(),
            acceptance: None,
            extras: Vec::new(),
        }
    }

    /// 把结构化内容摊成带类别的字段列表，视图只按类别裁剪。
    pub fn fields(&self) -> Vec<EvidenceField> {
        let mut fields = Vec::new();
        fields.push(EvidenceField::new(
            "manifest",
            FieldClass::Reference,
            &format!("{}@{}", self.manifest.manifest_id, self.manifest.revision),
        ));
        fields.push(EvidenceField::new(
            "summary",
            FieldClass::Conclusion,
            &self.summary,
        ));
        for (index, entry) in self.authorization_index.iter().enumerate() {
            fields.push(EvidenceField::new(
                &format!("authorization[{index}]"),
                FieldClass::Reference,
                entry,
            ));
        }
        for event in &self.timeline {
            fields.push(EvidenceField::new(
                &format!("timeline[{}]", event.at),
                FieldClass::Timeline,
                &event.what,
            ));
        }
        for artifact in &self.artifacts {
            fields.push(EvidenceField::new(
                &format!("artifact:{}", artifact.artifact_id),
                FieldClass::Reference,
                &artifact.digest,
            ));
        }
        for item in &self.verification {
            fields.push(EvidenceField::new(
                &format!("verification:{}", item.check_id),
                FieldClass::Reference,
                &item.raw_ref,
            ));
        }
        fields.push(EvidenceField::new(
            "provenance",
            FieldClass::Provenance,
            &self.provenance,
        ));
        for (index, step) in self.revalidation.iter().enumerate() {
            fields.push(EvidenceField::new(
                &format!("revalidation[{index}]"),
                FieldClass::Provenance,
                step,
            ));
        }
        for signature in &self.signatures {
            fields.push(EvidenceField::new(
                &format!("signature:{}", signature.signer),
                FieldClass::Reference,
                &signature.covers,
            ));
        }
        fields.extend(self.extras.iter().cloned());
        fields
    }

    /// 必填小节是否齐备。缺哪节能直接进离线校验报告。
    pub fn missing_sections(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.artifacts.is_empty() {
            missing.push("产物引用");
        }
        if self.authorization_index.is_empty() {
            missing.push("授权索引");
        }
        if self.timeline.is_empty() {
            missing.push("时间线");
        }
        if self.verification.is_empty() {
            missing.push("测试证据");
        }
        if self.provenance.trim().is_empty() {
            missing.push("来源说明");
        }
        if self.revalidation.is_empty() {
            missing.push("复验方法");
        }
        if self.signatures.is_empty() {
            missing.push("签名");
        }
        missing
    }
}

/// 按视图裁剪。凭据类字段与形似凭据的值在**任何**视图都不出现。
pub fn project(package: &EvidencePackage, view: ViewKind) -> ProjectedView {
    let mut kept = Vec::new();
    let mut withheld_names = Vec::new();
    let mut withheld_count = 0usize;

    for field in package.fields() {
        let blocked = field.class == FieldClass::Credential
            || looks_like_credential(&field.value)
            || !view.admits(field.class);
        if blocked {
            withheld_count += 1;
            withheld_names.push(field.name.clone());
        } else {
            kept.push(field);
        }
    }

    let withheld = match view {
        ViewKind::Customer => WithheldDisclosure::Count(withheld_count),
        _ => WithheldDisclosure::Names(withheld_names),
    };
    ProjectedView {
        view,
        fields: kept,
        withheld,
    }
}

// ---------------------------------------------------------------------------
// 2.4 内容清单身份与运输文件身份
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentEntry {
    pub path: String,
    pub digest: String,
    pub size: u64,
}

impl ContentEntry {
    pub fn new(path: &str, digest: &str, size: u64) -> Self {
        Self {
            path: path.to_string(),
            digest: digest.to_string(),
            size,
        }
    }
}

/// 内容清单。构造时按 path 排序，所以 digest 与打包顺序、时间戳无关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentManifest {
    entries: Vec<ContentEntry>,
}

impl ContentManifest {
    pub fn new(entries: Vec<ContentEntry>) -> Self {
        let mut entries = entries;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Self { entries }
    }

    pub fn entries(&self) -> &[ContentEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 内容清单摘要：只由 path/digest/size 决定。
    pub fn digest(&self) -> String {
        let material: String = self
            .entries
            .iter()
            .map(|entry| format!("{}\0{}\0{}\n", entry.path, entry.digest, entry.size))
            .collect();
        sha256_hex(material.as_bytes())
    }

    /// 脱敏：移走若干路径，得到新清单。被移走的路径按字典序返回，便于核对。
    pub fn without(&self, paths: &[&str]) -> (Self, Vec<String>) {
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for entry in &self.entries {
            if paths.contains(&entry.path.as_str()) {
                removed.push(entry.path.clone());
            } else {
                kept.push(entry.clone());
            }
        }
        removed.sort();
        (Self::new(kept), removed)
    }
}

/// 运输文件（.zip/.pdf）：只记文件名与整文件 digest。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportFile {
    pub file_name: String,
    pub digest: String,
}

impl TransportFile {
    pub fn new(file_name: &str, digest: &str) -> Self {
        Self {
            file_name: file_name.to_string(),
            digest: digest.to_string(),
        }
    }
}

/// 签名覆盖的 payload 必须写明确，不能"签了整个包"这种含糊说法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadScope {
    /// 覆盖内容清单摘要。
    ContentManifest(String),
    /// 覆盖运输文件整文件 digest（重新打包即失效）。
    WholeTransportFile(String),
}

impl PayloadScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ContentManifest(_) => "content-manifest",
            Self::WholeTransportFile(_) => "whole-transport-file",
        }
    }

    pub fn covered_digest(&self) -> &str {
        match self {
            Self::ContentManifest(digest) | Self::WholeTransportFile(digest) => digest,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSignature {
    pub signer: String,
    pub payload: PayloadScope,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepackVerdict {
    /// 内容一致：运输 digest 可以变，内容身份不变。
    ContentIdentical {
        content_digest: String,
        transport_digest: String,
    },
    ContentChanged {
        original: String,
        repacked: String,
    },
    /// 签名覆盖的不是内容清单，或者覆盖的摘要是旧的。
    SignatureScopeMismatch {
        covers: &'static str,
        signed: String,
        actual: String,
    },
}

impl RepackVerdict {
    pub fn is_content_identical(&self) -> bool {
        matches!(self, Self::ContentIdentical { .. })
    }
}

/// 判定重新打包。先看内容身份，再看签名覆盖的对象与摘要。
pub fn verify_repack(
    original: &ContentManifest,
    repacked: &ContentManifest,
    transport: &TransportFile,
    signature: &PackageSignature,
) -> RepackVerdict {
    let original_digest = original.digest();
    let repacked_digest = repacked.digest();
    if original_digest != repacked_digest {
        return RepackVerdict::ContentChanged {
            original: original_digest,
            repacked: repacked_digest,
        };
    }
    match &signature.payload {
        PayloadScope::ContentManifest(signed) if *signed == repacked_digest => {
            RepackVerdict::ContentIdentical {
                content_digest: repacked_digest,
                transport_digest: transport.digest.clone(),
            }
        }
        PayloadScope::ContentManifest(signed) => RepackVerdict::SignatureScopeMismatch {
            covers: signature.payload.as_str(),
            signed: signed.clone(),
            actual: repacked_digest,
        },
        PayloadScope::WholeTransportFile(signed) => RepackVerdict::SignatureScopeMismatch {
            covers: signature.payload.as_str(),
            signed: signed.clone(),
            actual: transport.digest.clone(),
        },
    }
}

/// 脱敏后的包。哈希链在删除条目处断开，这是事实而不是缺陷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskedPackage {
    pub content: ContentManifest,
    pub removed: Vec<String>,
    pub chain: ChainContinuity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainContinuity {
    Intact,
    Broken { removed_entries: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    BrokenContinuity { removed_entries: usize },
}

impl fmt::Display for ChainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BrokenContinuity { removed_entries } => write!(
                formatter,
                "脱敏删除了 {removed_entries} 个条目，原始连续哈希链已断开，不能声明可验证"
            ),
        }
    }
}

impl std::error::Error for ChainError {}

/// 脱敏：删条目后必须重新签名，且不能声称原始连续性仍可验证。
pub fn mask(package: &ContentManifest, remove: &[&str]) -> MaskedPackage {
    let (content, removed) = package.without(remove);
    let chain = if removed.is_empty() {
        ChainContinuity::Intact
    } else {
        ChainContinuity::Broken {
            removed_entries: removed.len(),
        }
    };
    MaskedPackage {
        content,
        removed,
        chain,
    }
}

impl MaskedPackage {
    /// 声称"原始哈希链仍可验证"的接口。脱敏过就只能是错。
    pub fn claim_original_chain(&self) -> Result<(), ChainError> {
        match self.chain {
            ChainContinuity::Intact => Ok(()),
            ChainContinuity::Broken { removed_entries } => {
                Err(ChainError::BrokenContinuity { removed_entries })
            }
        }
    }

    /// 脱敏包必须带自己的签名：内容变了，旧签名不再覆盖它。
    pub fn requires_new_signature(&self) -> bool {
        !self.removed.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 2.5 离线分项校验
// ---------------------------------------------------------------------------

/// 可信身份的来源。嵌入公钥不在其中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustSource {
    PreconfiguredKey,
    Directory,
    CertificateChain,
    SelfSigned,
}

impl TrustSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreconfiguredKey => "preconfigured-key",
            Self::Directory => "directory",
            Self::CertificateChain => "certificate-chain",
            Self::SelfSigned => "self-signed",
        }
    }

    pub fn establishes_external_identity(&self) -> bool {
        !matches!(self, Self::SelfSigned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignerTrust {
    pub source: TrustSource,
    /// 公钥是否随包携带。携带也只能证明包内一致。
    pub key_embedded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckSection {
    Integrity,
    SignerIdentity,
    EvidenceAssociation,
    Revalidation,
    BusinessAcceptance,
}

impl CheckSection {
    pub const ALL: [Self; 5] = [
        Self::Integrity,
        Self::SignerIdentity,
        Self::EvidenceAssociation,
        Self::Revalidation,
        Self::BusinessAcceptance,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Integrity => "integrity",
            Self::SignerIdentity => "signer-identity",
            Self::EvidenceAssociation => "evidence-association",
            Self::Revalidation => "revalidation",
            Self::BusinessAcceptance => "business-acceptance",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionReport {
    pub section: CheckSection,
    pub verdict: SectionVerdict,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionVerdict {
    Pass,
    Fail,
    Unknown,
}

impl SectionVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineReport {
    pub sections: Vec<SectionReport>,
    pub external_identity_trusted: bool,
}

impl OfflineReport {
    pub fn section(&self, section: CheckSection) -> Option<&SectionReport> {
        self.sections.iter().find(|item| item.section == section)
    }

    pub fn verdict(&self, section: CheckSection) -> Option<&SectionVerdict> {
        self.section(section).map(|item| &item.verdict)
    }

    /// 未给出确定结论的分项。整体结论只能由它拼出来，不给单一"通过"徽标。
    pub fn unresolved(&self) -> Vec<CheckSection> {
        self.sections
            .iter()
            .filter(|item| item.verdict != SectionVerdict::Pass)
            .map(|item| item.section)
            .collect()
    }

    pub fn overall(&self) -> Overall {
        let unresolved = self.unresolved();
        if unresolved.is_empty() {
            Overall::FullyVerified
        } else {
            Overall::Partial { unresolved }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overall {
    FullyVerified,
    /// 有分项没给出确定结论。不是"失败"，也不是"通过"。
    Partial {
        unresolved: Vec<CheckSection>,
    },
}

impl Overall {
    pub fn is_fully_verified(&self) -> bool {
        matches!(self, Self::FullyVerified)
    }
}

/// 离线校验：五项分别给结论，一项不过不改写别的项。
pub fn offline_check(package: &EvidencePackage, trust: SignerTrust) -> OfflineReport {
    let mut sections = Vec::new();

    let malformed: Vec<&str> = package
        .artifacts
        .iter()
        .filter(|artifact| !is_digest(&artifact.digest))
        .map(|artifact| artifact.artifact_id.as_str())
        .collect();
    let integrity = if package.artifacts.is_empty() {
        SectionReport {
            section: CheckSection::Integrity,
            verdict: SectionVerdict::Fail,
            detail: "包内没有产物引用，无法校验完整性".to_string(),
        }
    } else if malformed.is_empty() {
        SectionReport {
            section: CheckSection::Integrity,
            verdict: SectionVerdict::Pass,
            detail: format!("{} 个产物引用的 digest 形态正确", package.artifacts.len()),
        }
    } else {
        SectionReport {
            section: CheckSection::Integrity,
            verdict: SectionVerdict::Fail,
            detail: format!("产物引用不是 digest：{}", malformed.join("、")),
        }
    };
    sections.push(integrity);

    let identity_trusted = trust.source.establishes_external_identity() && !trust.key_embedded;
    sections.push(SectionReport {
        section: CheckSection::SignerIdentity,
        verdict: if identity_trusted {
            SectionVerdict::Pass
        } else {
            SectionVerdict::Unknown
        },
        detail: if identity_trusted {
            format!("签发者身份来自 {}", trust.source.as_str())
        } else if trust.key_embedded {
            "嵌入公钥只能验证包内一致性，外部身份未建立信任（G-33）".to_string()
        } else {
            "自签包未经可信公钥/目录/证书链核对，外部身份未建立信任（G-33）".to_string()
        },
    });

    let known: Vec<&str> = package
        .artifacts
        .iter()
        .map(|artifact| artifact.artifact_id.as_str())
        .collect();
    let dangling: Vec<&str> = package
        .verification
        .iter()
        .filter(|item| {
            !known.contains(&item.artifact_id.as_str()) || item.raw_ref.trim().is_empty()
        })
        .map(|item| item.check_id.as_str())
        .collect();
    sections.push(SectionReport {
        section: CheckSection::EvidenceAssociation,
        verdict: if dangling.is_empty() && !package.verification.is_empty() {
            SectionVerdict::Pass
        } else {
            SectionVerdict::Fail
        },
        detail: if dangling.is_empty() && !package.verification.is_empty() {
            format!(
                "{} 条测试证据都指向包内产物与原始引用",
                package.verification.len()
            )
        } else if package.verification.is_empty() {
            "包内没有测试证据".to_string()
        } else {
            format!("测试证据悬空或缺原始引用：{}", dangling.join("、"))
        },
    });

    sections.push(SectionReport {
        section: CheckSection::Revalidation,
        verdict: if package.revalidation.is_empty() {
            SectionVerdict::Unknown
        } else {
            SectionVerdict::Pass
        },
        detail: if package.revalidation.is_empty() {
            "未给出可执行复验方法".to_string()
        } else {
            format!("给出 {} 步复验方法", package.revalidation.len())
        },
    });

    let (verdict, detail) = match &package.acceptance {
        None => (SectionVerdict::Unknown, "未取得验收回执".to_string()),
        Some(acceptance) if acceptance.decision == "accepted" => (
            SectionVerdict::Pass,
            format!("验收回执 {} 为通过", acceptance.acceptance_id),
        ),
        Some(acceptance) => (
            SectionVerdict::Fail,
            format!(
                "验收回执 {} 为 {}",
                acceptance.acceptance_id, acceptance.decision
            ),
        ),
    };
    sections.push(SectionReport {
        section: CheckSection::BusinessAcceptance,
        verdict,
        detail,
    });

    OfflineReport {
        sections,
        external_identity_trusted: identity_trusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dg(seed: &str) -> String {
        format!("sha256:{}", sha256_hex(seed.as_bytes()))
    }

    fn pin(artifact: &str, revision: &str) -> ArtifactPin {
        ArtifactPin::new(artifact, revision, &dg(&format!("{artifact}@{revision}")))
    }

    fn frozen_book() -> (ManifestBook, DeliveryRef) {
        let mut book = ManifestBook::new();
        let draft = ManifestDraft::new("stage-1", None)
            .with_artifact(pin("report", "rev-1"))
            .with_acceptance("acc-1")
            .with_note("首个冻结基线");
        assert_eq!(
            book.propose(draft, "rev-1"),
            ProposeOutcome::Accepted {
                revision: "rev-1".to_string()
            }
        );
        let reference = DeliveryRef::new("stage-1", "rev-1");
        (book, reference)
    }

    /// 2.1 / `invariant_i_07`：改已冻结清单 → 新修订，旧引用仍指向旧修订。
    #[test]
    fn frozen_manifest_change_creates_a_new_revision_and_keeps_old_references() {
        let (mut book, reference) = frozen_book();

        let draft = ManifestDraft::new("stage-1", Some("rev-1"))
            .with_artifact(pin("report", "rev-1"))
            .with_artifact(pin("quota", "rev-2"))
            .with_acceptance("acc-2")
            .with_note("加入配额制品");
        assert_eq!(
            book.propose(draft, "rev-2"),
            ProposeOutcome::Accepted {
                revision: "rev-2".to_string()
            }
        );

        let revisions = book.revisions_of("stage-1");
        assert_eq!(revisions.len(), 2, "改动产生新修订而不是原地修改");
        let old = book.resolve(&reference).expect("旧引用必须仍可解析");
        assert_eq!(old.revision, "rev-1");
        assert_eq!(old.artifacts.len(), 1, "旧修订内容不变");
        assert_eq!(old.acceptances, vec!["acc-1".to_string()]);
        let new = book.resolve(&DeliveryRef::new("stage-1", "rev-2")).unwrap();
        assert_eq!(new.artifacts.len(), 2);
        assert_eq!(new.base.as_deref(), Some("rev-1"));
        assert_eq!(new.state, RevisionState::Frozen);
        assert_eq!(book.accepted_revision("stage-1"), Some("rev-2"));
    }

    /// 2.1：过期 base_revision → Conflict，候选保留，接受指针不动。
    #[test]
    fn stale_base_revision_returns_conflict_and_keeps_the_candidate() {
        let (mut book, _) = frozen_book();
        let second = ManifestDraft::new("stage-1", Some("rev-1"))
            .with_artifact(pin("report", "rev-1"))
            .with_artifact(pin("quota", "rev-2"));
        assert!(matches!(
            book.propose(second, "rev-2"),
            ProposeOutcome::Accepted { .. }
        ));

        // 拿着 rev-1 当 base 再来一次：指针已经是 rev-2。
        let stale = ManifestDraft::new("stage-1", Some("rev-1"))
            .with_artifact(pin("report", "rev-1"))
            .with_note("基于旧基线的提议");
        assert_eq!(
            book.propose(stale, "rev-3"),
            ProposeOutcome::Conflict {
                current: Some("rev-2".to_string()),
                base: Some("rev-1".to_string()),
                candidate: "rev-3".to_string()
            }
        );
        assert_eq!(
            book.accepted_revision("stage-1"),
            Some("rev-2"),
            "指针不被覆盖"
        );
        assert!(book.revision("stage-1", "rev-3").is_none(), "冲突不落地");
        assert_eq!(book.candidates().len(), 1, "候选保留待人工收口");

        book.accept_candidate("stage-1", "rev-3").unwrap();
        assert_eq!(book.accepted_revision("stage-1"), Some("rev-3"));
        assert!(book.candidates().is_empty());
    }

    /// 2.1：`latest`/`*`/制品名都不是 digest，不能当钉扎。
    #[test]
    fn floating_reference_is_refused_as_a_pin() {
        let mut book = ManifestBook::new();
        for floating in ["latest", "*", "report-v2"] {
            let draft = ManifestDraft::new("stage-1", None)
                .with_artifact(ArtifactPin::new("report", "rev-1", floating));
            match book.propose(draft, "rev-1") {
                ProposeOutcome::Rejected(ManifestError::BadPin(PinError::FloatingReference {
                    digest,
                    ..
                })) => assert_eq!(digest, floating),
                other => panic!("浮动引用 `{floating}` 应被拒绝，实际 {other:?}"),
            }
        }
        let empty = ManifestDraft::new("stage-1", None);
        assert_eq!(
            book.propose(empty, "rev-1"),
            ProposeOutcome::Rejected(ManifestError::NoPins)
        );
    }

    /// 2.1：外部内容变化只登记待复核修订，不动已冻结基线。
    #[test]
    fn external_change_is_registered_as_pending_review() {
        let (mut book, _) = frozen_book();
        let outcome = book.note_external_change("stage-1", "上游制品 digest 变了", "rev-2");
        assert_eq!(
            outcome,
            ProposeOutcome::Accepted {
                revision: "rev-2".to_string()
            }
        );
        assert_eq!(
            book.accepted_revision("stage-1"),
            Some("rev-1"),
            "外部变化不悄悄改基线"
        );
        let pending = book.pending_review("stage-1");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].revision, "rev-2");
        assert_eq!(pending[0].state, RevisionState::PendingReview);
        assert!(pending[0].note.contains("外部内容变化"));
    }

    /// 2.2：连接器说通知已发出，业务验收未通过 → 已投递但未通过，不生成"已修复"。
    #[test]
    fn connector_delivery_is_not_business_acceptance() {
        let reference = DeliveryRef::new("stage-1", "rev-1");
        let publication =
            Publication::new("pub-1", &reference, "customer-prod").observe(ExternalObservation {
                claimed: ClaimedState::Delivered,
                observed_at: "2026-11-21T09:00:00Z".to_string(),
                declarer: Declarer::Connector {
                    id: "conn-9".to_string(),
                },
                host_verified: true,
            });
        assert_eq!(publication.effect_id, None);
        assert_eq!(delivery_state(&publication), DeliveryState::Delivered);
        assert_eq!(
            customer_conclusion(delivery_state(&publication), AcceptanceState::NotPassed),
            CustomerConclusion::DeliveredPendingAcceptance
        );
        assert_eq!(
            customer_conclusion(delivery_state(&publication), AcceptanceState::Passed),
            CustomerConclusion::Fixed
        );
        assert_eq!(
            customer_conclusion(delivery_state(&publication), AcceptanceState::Rejected),
            CustomerConclusion::DeliveredPendingAcceptance
        );
    }

    /// 2.2：签发者声明未被宿主验证 → 仍是 Unconfirmed，不能算已投递。
    #[test]
    fn signer_claim_without_host_verification_stays_unconfirmed() {
        let reference = DeliveryRef::new("stage-1", "rev-1");
        let publication =
            Publication::new("pub-2", &reference, "customer-prod").observe(ExternalObservation {
                claimed: ClaimedState::Delivered,
                observed_at: "2026-11-21T09:00:00Z".to_string(),
                declarer: Declarer::Signer {
                    id: "signer-1".to_string(),
                },
                host_verified: false,
            });
        assert_eq!(
            publication.effective(),
            EffectiveState::Unconfirmed(ClaimedState::Delivered)
        );
        assert_eq!(
            delivery_state(&publication),
            DeliveryState::Unconfirmed(ClaimedState::Delivered)
        );
        assert_eq!(
            customer_conclusion(delivery_state(&publication), AcceptanceState::Passed),
            CustomerConclusion::Unknown,
            "验收通过但发布未确认，只能给 Unknown"
        );

        let unobserved = Publication::new("pub-3", &reference, "customer-prod");
        assert_eq!(
            delivery_state(&unobserved),
            DeliveryState::NotRequested,
            "没有任何观测就是还没发出去"
        );
        assert_eq!(
            customer_conclusion(delivery_state(&unobserved), AcceptanceState::NotPassed),
            CustomerConclusion::NotDelivered
        );
    }

    fn package() -> EvidencePackage {
        let mut package = EvidencePackage::new(
            DeliveryRef::new("stage-1", "rev-2"),
            "配额查询在跨租户场景返回拒绝，测试通过",
        );
        package.authorization_index.push("auth-7".to_string());
        package.timeline.push(TimelineEvent {
            at: "2026-11-21T09:00:00Z".to_string(),
            what: "候选合并".to_string(),
        });
        package.artifacts.push(ArtifactRef {
            artifact_id: "report".to_string(),
            digest: dg("report@rev-1"),
        });
        package.verification.push(VerificationRef {
            check_id: "permission".to_string(),
            outcome: "pass".to_string(),
            artifact_id: "report".to_string(),
            raw_ref: "cas://reports/permission.json".to_string(),
        });
        package.provenance = "由 bod-app::runtime_verification 采集".to_string();
        package
            .revalidation
            .push("按 p4-verification.md §2 对同一 digest 重跑权限检查".to_string());
        package.signatures.push(SignatureRef {
            signer: "signer-1".to_string(),
            covers: "content-manifest".to_string(),
        });
        package.acceptance = Some(AcceptanceRef {
            acceptance_id: "acc-2".to_string(),
            decision: "accepted".to_string(),
        });
        package.extras.push(EvidenceField::new(
            "discussion:thread-3",
            FieldClass::InternalDiscussion,
            "内部讨论：是否先把配额服务降级",
        ));
        package.extras.push(EvidenceField::new(
            "cost:default",
            FieldClass::CostDetail,
            "默认成本明细：0.42 元",
        ));
        package.extras.push(EvidenceField::new(
            "credential:connector",
            FieldClass::Credential,
            "token=abcdef",
        ));
        package
    }

    /// 2.3：客户视图看不到内部讨论、凭据与默认成本明细。
    #[test]
    fn customer_view_hides_internal_discussion_credentials_and_cost() {
        let package = package();
        let customer = project(&package, ViewKind::Customer);
        assert!(customer.value_of("summary").is_some(), "结论必须可见");
        assert!(
            customer.value_of("artifact:report").is_some(),
            "产物引用必须可见"
        );
        assert!(customer.value_of("discussion:thread-3").is_none());
        assert!(customer.value_of("cost:default").is_none());
        assert!(customer.value_of("credential:connector").is_none());

        let internal = project(&package, ViewKind::Internal);
        assert!(internal.value_of("discussion:thread-3").is_some());
        assert!(internal.value_of("cost:default").is_some());
        assert!(
            internal.value_of("credential:connector").is_none(),
            "内部视图也不带原始密钥"
        );

        let audit = project(&package, ViewKind::Audit);
        assert!(
            audit.value_of("cost:default").is_none(),
            "成本明细不进军审计视图"
        );
        assert!(audit.value_of("timeline[2026-11-21T09:00:00Z]").is_some());
    }

    /// 2.3：类别标错但值形似凭据时，任何视图都不放行。
    #[test]
    fn credential_like_value_is_withheld_from_every_view() {
        let mut package = package();
        package.summary = "连接器凭据 Bearer abcdef123456 已校验".to_string();
        for view in [ViewKind::Internal, ViewKind::Audit, ViewKind::Customer] {
            let projected = project(&package, view);
            assert!(
                projected.value_of("summary").is_none(),
                "{} 视图不得出现凭据值",
                view.as_str()
            );
        }

        let mut key = package.clone();
        key.provenance = "-----BEGIN PRIVATE KEY-----\nMIIE...\n".to_string();
        assert!(
            project(&key, ViewKind::Internal)
                .value_of("provenance")
                .is_none()
        );
    }

    /// 2.3：客户视图只报被扣下的数量——说出"有一条内部讨论"本身就是泄露。
    #[test]
    fn customer_view_reports_only_a_count_of_withheld_items() {
        let package = package();
        let customer = project(&package, ViewKind::Customer);
        match &customer.withheld {
            WithheldDisclosure::Count(count) => assert!(*count >= 3, "至少三条被扣下"),
            WithheldDisclosure::Names(names) => {
                panic!("客户视图不应列出被扣字段名：{names:?}")
            }
        }
        match &project(&package, ViewKind::Audit).withheld {
            WithheldDisclosure::Names(names) => {
                assert!(names.iter().any(|item| item == "cost:default"));
            }
            WithheldDisclosure::Count(_) => panic!("审计视图应列出被扣字段"),
        }
    }

    fn content() -> ContentManifest {
        ContentManifest::new(vec![
            ContentEntry::new("manifest.json", &dg("manifest"), 128),
            ContentEntry::new("evidence/permission.json", &dg("permission"), 2048),
            ContentEntry::new("summary.md", &dg("summary"), 512),
        ])
    }

    /// 2.4：重新打包后内容 digest 不变、运输 digest 更新，签名覆盖内容清单。
    #[test]
    fn repack_keeps_content_identity_and_changes_transport_identity() {
        let original = content();
        // 打包顺序变了，内容身份不应变。
        let repacked = ContentManifest::new(vec![
            ContentEntry::new("summary.md", &dg("summary"), 512),
            ContentEntry::new("evidence/permission.json", &dg("permission"), 2048),
            ContentEntry::new("manifest.json", &dg("manifest"), 128),
        ]);
        assert_eq!(original.digest(), repacked.digest(), "顺序不影响内容身份");

        let transport = TransportFile::new("evidence.zip", &dg("zip-v2"));
        let signature = PackageSignature {
            signer: "signer-1".to_string(),
            payload: PayloadScope::ContentManifest(repacked.digest()),
            value: "sig".to_string(),
        };
        let verdict = verify_repack(&original, &repacked, &transport, &signature);
        match &verdict {
            RepackVerdict::ContentIdentical {
                content_digest,
                transport_digest,
            } => {
                assert_eq!(content_digest, &original.digest());
                assert_eq!(transport_digest, &dg("zip-v2"), "运输身份跟着新文件走");
            }
            other => panic!("内容应一致，实际 {other:?}"),
        }
        assert!(verdict.is_content_identical());

        let changed = ContentManifest::new(vec![ContentEntry::new(
            "summary.md",
            &dg("summary-v2"),
            512,
        )]);
        assert!(matches!(
            verify_repack(&original, &changed, &transport, &signature),
            RepackVerdict::ContentChanged { .. }
        ));
    }

    /// 2.4：签"整个 zip"的签名在重打包后失效；签名必须说明覆盖的是哪个 payload。
    #[test]
    fn signature_must_name_the_payload_it_covers() {
        let content = content();
        let transport = TransportFile::new("evidence.zip", &dg("zip-v2"));
        let whole = PackageSignature {
            signer: "signer-1".to_string(),
            payload: PayloadScope::WholeTransportFile(dg("zip-v1")),
            value: "sig".to_string(),
        };
        match verify_repack(&content, &content, &transport, &whole) {
            RepackVerdict::SignatureScopeMismatch {
                covers,
                signed,
                actual,
            } => {
                assert_eq!(covers, "whole-transport-file");
                assert_eq!(signed, dg("zip-v1"));
                assert_eq!(actual, dg("zip-v2"), "报告要指出实际运输 digest");
            }
            other => panic!("覆盖对象不匹配应被指出，实际 {other:?}"),
        }

        let stale = PackageSignature {
            signer: "signer-1".to_string(),
            payload: PayloadScope::ContentManifest(dg("old-content")),
            value: "sig".to_string(),
        };
        assert!(matches!(
            verify_repack(&content, &content, &transport, &stale),
            RepackVerdict::SignatureScopeMismatch {
                covers: "content-manifest",
                ..
            }
        ));
    }

    /// 2.4：脱敏后不能声称原始连续哈希链仍可验证，且必须重新签名。
    #[test]
    fn masked_package_cannot_claim_the_original_hash_chain() {
        let content = content();
        let masked = mask(&content, &["evidence/permission.json"]);
        assert_eq!(masked.removed, vec!["evidence/permission.json".to_string()]);
        assert_eq!(masked.content.len(), 2);
        assert_ne!(
            masked.content.digest(),
            content.digest(),
            "脱敏后内容身份变了"
        );
        assert!(masked.requires_new_signature());
        assert_eq!(
            masked.claim_original_chain(),
            Err(ChainError::BrokenContinuity { removed_entries: 1 })
        );

        let untouched = mask(&content, &[]);
        assert_eq!(untouched.chain, ChainContinuity::Intact);
        assert_eq!(untouched.claim_original_chain(), Ok(()));
        assert!(!untouched.requires_new_signature());
    }

    /// 2.5 / `invariant_i_52`、G-33：自签包过了摘要校验，外部身份仍未建立信任。
    #[test]
    fn self_signed_package_passes_integrity_but_not_identity() {
        let package = package();
        let report = offline_check(
            &package,
            SignerTrust {
                source: TrustSource::SelfSigned,
                key_embedded: true,
            },
        );
        assert_eq!(
            report.verdict(CheckSection::Integrity),
            Some(&SectionVerdict::Pass)
        );
        assert_eq!(
            report.verdict(CheckSection::SignerIdentity),
            Some(&SectionVerdict::Unknown)
        );
        assert!(!report.external_identity_trusted);
        assert!(!report.overall().is_fully_verified());
        let identity = report.section(CheckSection::SignerIdentity).unwrap();
        assert!(identity.detail.contains("外部身份未建立信任"));
        assert!(identity.detail.contains("G-33"));

        let trusted = offline_check(
            &package,
            SignerTrust {
                source: TrustSource::PreconfiguredKey,
                key_embedded: false,
            },
        );
        assert!(trusted.external_identity_trusted);
        assert!(
            trusted.overall().is_fully_verified(),
            "{:?}",
            trusted.unresolved()
        );
    }

    /// 2.5：五项分别报告，不给单一"通过"结论。
    #[test]
    fn offline_report_lists_five_sections_separately() {
        let package = package();
        let report = offline_check(
            &package,
            SignerTrust {
                source: TrustSource::Directory,
                key_embedded: false,
            },
        );
        assert_eq!(report.sections.len(), 5);
        let order: Vec<CheckSection> = report.sections.iter().map(|item| item.section).collect();
        assert_eq!(order, CheckSection::ALL.to_vec());
        for section in CheckSection::ALL {
            assert!(report.section(section).is_some());
        }
    }

    /// 2.5：缺验收回执 → Unknown；明确拒绝 → Fail。两者都不算通过。
    #[test]
    fn missing_acceptance_receipt_is_unknown_not_pass() {
        let mut package = package();
        package.acceptance = None;
        let report = offline_check(
            &package,
            SignerTrust {
                source: TrustSource::Directory,
                key_embedded: false,
            },
        );
        assert_eq!(
            report.verdict(CheckSection::BusinessAcceptance),
            Some(&SectionVerdict::Unknown)
        );
        assert_eq!(
            report.unresolved(),
            vec![CheckSection::BusinessAcceptance],
            "只有验收项未定，其它项不受影响"
        );

        package.acceptance = Some(AcceptanceRef {
            acceptance_id: "acc-2".to_string(),
            decision: "rejected".to_string(),
        });
        let rejected = offline_check(
            &package,
            SignerTrust {
                source: TrustSource::Directory,
                key_embedded: false,
            },
        );
        assert_eq!(
            rejected.verdict(CheckSection::BusinessAcceptance),
            Some(&SectionVerdict::Fail)
        );
    }

    /// 2.5：测试证据指向包内不存在的产物 → 关联项失败，并点名。
    #[test]
    fn dangling_verification_reference_fails_association() {
        let mut package = package();
        package.verification.push(VerificationRef {
            check_id: "quota".to_string(),
            outcome: "pass".to_string(),
            artifact_id: "quota".to_string(),
            raw_ref: "cas://reports/quota.json".to_string(),
        });
        let report = offline_check(
            &package,
            SignerTrust {
                source: TrustSource::Directory,
                key_embedded: false,
            },
        );
        assert_eq!(
            report.verdict(CheckSection::EvidenceAssociation),
            Some(&SectionVerdict::Fail)
        );
        let section = report.section(CheckSection::EvidenceAssociation).unwrap();
        assert!(section.detail.contains("quota"));
        assert!(!report.overall().is_fully_verified());
        match report.overall() {
            Overall::Partial { unresolved } => {
                assert_eq!(unresolved, vec![CheckSection::EvidenceAssociation])
            }
            Overall::FullyVerified => panic!("不应给整体通过"),
        }
    }
}
