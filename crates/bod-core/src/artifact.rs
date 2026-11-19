//! P0 D5 成果、修订与接受契约（§21、§33.2、§33.5、I-07/I-08/I-51/G-9）。
//!
//! 纯值类型 + 确定性规则：零外部依赖、时间由调用方注入。
//! 本模块只回答"某个确定修订能否被某个主体按某组证据接受"，
//! 不负责文件落盘、事务或 HTTP 映射（见 `bod-app::cas` / `artifact_acceptance` / `control_api`）。

use crate::DomainId;
use crate::model::{Phase, Principal, PrincipalKind, Scope};

// ---------------------------------------------------------------------------
// 摘要
// ---------------------------------------------------------------------------

/// SHA-256（FIPS 180-4）。自实现以避免为单个哈希引入新依赖（G-1 依赖准入）。
pub fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (idx, word) in chunk.chunks(4).enumerate() {
            w[idx] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for idx in 16..64 {
            let s0 =
                w[idx - 15].rotate_right(7) ^ w[idx - 15].rotate_right(18) ^ (w[idx - 15] >> 3);
            let s1 = w[idx - 2].rotate_right(17) ^ w[idx - 2].rotate_right(19) ^ (w[idx - 2] >> 10);
            w[idx] = w[idx - 16]
                .wrapping_add(s0)
                .wrapping_add(w[idx - 7])
                .wrapping_add(s1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];

        for idx in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[idx])
                .wrapping_add(w[idx]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = String::with_capacity(64);
    for word in state {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgorithm {
    Sha256,
}

impl DigestAlgorithm {
    pub fn as_str(self) -> &'static str {
        "sha256"
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }
}

/// 内容摘要：必须同时记录算法与被计算的字节表示（§21.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub algorithm: DigestAlgorithm,
    pub value: String,
}

impl Digest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self {
            algorithm: DigestAlgorithm::Sha256,
            value: sha256_hex(bytes),
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let (algorithm, hex) = value.split_once(':')?;
        let algorithm = DigestAlgorithm::parse(algorithm)?;
        if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self {
            algorithm,
            value: hex.to_ascii_lowercase(),
        })
    }

    pub fn as_string(&self) -> String {
        format!("{}:{}", self.algorithm.as_str(), self.value)
    }

    /// 空摘要或算法/长度不符不得作为固定引用。
    pub fn is_usable(&self) -> bool {
        self.value.len() == 64 && self.value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }
}

// ---------------------------------------------------------------------------
// 成果与修订
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactKind {
    SourceCode,
    DesignDoc,
    Report,
    Binary,
    StageBundle,
    Profile,
    Knowledge,
    /// 未知种类保留原名：不静默归入已知种类。
    Other(String),
}

impl ArtifactKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::SourceCode => "source_code",
            Self::DesignDoc => "design_doc",
            Self::Report => "report",
            Self::Binary => "binary",
            Self::StageBundle => "stage_bundle",
            Self::Profile => "profile",
            Self::Knowledge => "knowledge",
            Self::Other(name) => name,
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "source_code" => Self::SourceCode,
            "design_doc" => Self::DesignDoc,
            "report" => Self::Report,
            "binary" => Self::Binary,
            "stage_bundle" => Self::StageBundle,
            "profile" => Self::Profile,
            "knowledge" => Self::Knowledge,
            other => Self::Other(other.to_string()),
        }
    }
}

/// 权威性/可见性决定证据包的字段级披露（§21.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactVisibility {
    Internal,
    Customer,
    Audit,
}

impl ArtifactVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Customer => "customer",
            Self::Audit => "audit",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "customer" => Self::Customer,
            "audit" => Self::Audit,
            _ => Self::Internal,
        }
    }
}

/// 物理位置。外部权威存储（Git/制品仓库/文档系统）的对象本地 GC 一律不可删除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageRef {
    /// 本地 CAS：值为 digest 的十六进制正文。
    Cas(String),
    /// 外部权威存储：repo/commit/path、制品 digest 或 document_id+revision。
    External(String),
}

impl StorageRef {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Cas(value) | Self::External(value) => value,
        }
    }

    pub fn is_local_cas(&self) -> bool {
        matches!(self, Self::Cas(_))
    }

    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub id: DomainId,
    pub company_id: DomainId,
    pub project_id: DomainId,
    pub kind: ArtifactKind,
    pub title: String,
    pub owner: DomainId,
    pub visibility: ArtifactVisibility,
    /// 通过 compare-and-swap 更新的接受指针；与内容寻址 CAS 不是同一概念（§21.1）。
    pub accepted_revision: Option<DomainId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    Implements,
    BuiltFrom,
    Tests,
    Documents,
    DerivedFrom,
    Supersedes,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implements => "implements",
            Self::BuiltFrom => "built_from",
            Self::Tests => "tests",
            Self::Documents => "documents",
            Self::DerivedFrom => "derived_from",
            Self::Supersedes => "supersedes",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "built_from" => Self::BuiltFrom,
            "tests" => Self::Tests,
            "documents" => Self::Documents,
            "derived_from" => Self::DerivedFrom,
            "supersedes" => Self::Supersedes,
            _ => Self::Implements,
        }
    }
}

/// 指向上游**确定修订**的依赖；不接受浮动 latest。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionDependency {
    pub revision_id: DomainId,
    pub kind: DependencyKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRevision {
    pub revision_id: DomainId,
    pub artifact_id: DomainId,
    pub base_revision: Option<DomainId>,
    pub storage: StorageRef,
    pub digest: Digest,
    pub size_bytes: i64,
    pub mime: String,
    pub produced_by: DomainId,
    /// 来源说明：外部摘要必须写明算法与字节表示（§21.2）。
    pub source_note: String,
    pub depends_on: Vec<RevisionDependency>,
    pub created_at_ms: i64,
}

impl ArtifactRevision {
    /// 修订自身的最小完整性：不可变、可定位、可复算。
    pub fn validate(&self) -> Result<(), AcceptanceError> {
        if self.revision_id.as_str().is_empty() || self.artifact_id.as_str().is_empty() {
            return Err(AcceptanceError::EmptyRevision);
        }
        if !self.digest.is_usable() {
            return Err(AcceptanceError::UnusableDigest);
        }
        if self.size_bytes < 0 {
            return Err(AcceptanceError::InvalidSize);
        }
        if self.mime.trim().is_empty() {
            return Err(AcceptanceError::MissingMime);
        }
        if matches!(&self.storage, StorageRef::Cas(value) if value != &self.digest.value) {
            return Err(AcceptanceError::StorageDigestMismatch);
        }
        if self
            .depends_on
            .iter()
            .any(|dep| dep.revision_id.as_str().is_empty())
        {
            return Err(AcceptanceError::EmptyDependency);
        }
        Ok(())
    }

    /// 新修订必须基于某个既有修订；同一 base 重复提交由调用方按逻辑键判重。
    pub fn is_newer_than(&self, other: &DomainId) -> bool {
        self.base_revision.as_ref() == Some(other)
    }
}

// ---------------------------------------------------------------------------
// 验证状态机
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerificationState {
    Pending,
    Verifying,
    Verified,
    Rejected,
    NeedsRevision,
    Waived,
}

impl VerificationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verifying => "verifying",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::NeedsRevision => "needs_revision",
            Self::Waived => "waived",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "verifying" => Some(Self::Verifying),
            "verified" => Some(Self::Verified),
            "rejected" => Some(Self::Rejected),
            "needs_revision" => Some(Self::NeedsRevision),
            "waived" => Some(Self::Waived),
            _ => None,
        }
    }

    /// 只有 Verified 代表"验证通过"。Waived 是"已豁免"，不是测试通过。
    pub fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Verified | Self::Rejected | Self::NeedsRevision | Self::Waived
        )
    }

    /// 面向界面的稳定标签：禁止把豁免渲染成通过。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待验证",
            Self::Verifying => "验证中",
            Self::Verified => "验证通过",
            Self::Rejected => "验证失败",
            Self::NeedsRevision => "需要修订",
            Self::Waived => "已豁免（未验证）",
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Pending => matches!(next, Self::Verifying | Self::Rejected | Self::Waived),
            Self::Verifying => matches!(
                next,
                Self::Verified | Self::Rejected | Self::NeedsRevision | Self::Waived
            ),
            // 终态不因迟到信息被推翻；新的验证需求必须走新修订。
            Self::Verified | Self::Rejected | Self::NeedsRevision | Self::Waived => false,
        }
    }
}

/// 豁免记录：主体、理由、适用版本与范围缺一不可（spec: 验收被豁免）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiver {
    pub actor: DomainId,
    pub reason: String,
    pub applies_to_revision: DomainId,
    pub scope: Scope,
    pub granted_at_ms: i64,
}

impl Waiver {
    pub fn validate(&self, revision: &DomainId) -> Result<(), AcceptanceError> {
        if &self.applies_to_revision != revision {
            return Err(AcceptanceError::WaiverRevisionMismatch);
        }
        if self.reason.trim().is_empty() {
            return Err(AcceptanceError::MissingReason);
        }
        Ok(())
    }

    /// 豁免永远不等于验证通过：界面与证据包都不得显示"测试已通过"。
    pub fn counts_as_verified(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRecord {
    pub revision_id: DomainId,
    pub state: VerificationState,
    pub actor: DomainId,
    pub reason: String,
    pub evidence_refs: Vec<String>,
    /// 记录被测的验证规则修订，接受时按此与 AcceptanceBinding 比对（§21.9）。
    pub verification_rules: DomainId,
    pub environment: DomainId,
    pub observed_at_ms: i64,
}

impl VerificationRecord {
    /// 只有与候选交付匹配的证据可用于接受。
    pub fn matches(&self, revision: &DomainId, rules: &DomainId, environment: &DomainId) -> bool {
        &self.revision_id == revision
            && &self.verification_rules == rules
            && &self.environment == environment
    }

    /// 自报通过不改变接受主体：实现者自己写的验证记录不能被当作独立证据。
    pub fn is_self_reported(&self, producer: &DomainId) -> bool {
        &self.actor == producer
    }
}

/// 状态转换的输入：字段较多，用结构体避免位置参数错配。
#[derive(Debug, Clone)]
pub struct VerificationDraft<'a> {
    pub next: VerificationState,
    pub actor: &'a DomainId,
    pub reason: &'a str,
    pub revision: &'a DomainId,
    pub rules: &'a DomainId,
    pub environment: &'a DomainId,
    pub evidence_refs: Vec<String>,
    pub observed_at_ms: i64,
}

/// 状态转换的唯一入口：非法转换与缺理由一律拒绝，不静默降级。
pub fn advance_verification(
    current: VerificationState,
    draft: &VerificationDraft<'_>,
) -> Result<VerificationRecord, AcceptanceError> {
    if !current.can_transition_to(draft.next) {
        return Err(AcceptanceError::IllegalTransition {
            from: current,
            to: draft.next,
        });
    }
    if matches!(
        draft.next,
        VerificationState::Rejected | VerificationState::NeedsRevision | VerificationState::Waived
    ) && draft.reason.trim().is_empty()
    {
        return Err(AcceptanceError::MissingReason);
    }
    Ok(VerificationRecord {
        revision_id: draft.revision.clone(),
        state: draft.next,
        actor: draft.actor.clone(),
        reason: draft.reason.to_string(),
        evidence_refs: draft.evidence_refs.clone(),
        verification_rules: draft.rules.clone(),
        environment: draft.environment.clone(),
        observed_at_ms: draft.observed_at_ms,
    })
}

/// 豁免只能由人类主体授予，且必须带理由与范围。
pub fn grant_waiver(
    actor: &Principal,
    revision: &DomainId,
    reason: &str,
    scope: Scope,
    now_ms: i64,
) -> Result<Waiver, AcceptanceError> {
    if actor.kind != PrincipalKind::User {
        return Err(AcceptanceError::WaiverRequiresHuman);
    }
    if reason.trim().is_empty() {
        return Err(AcceptanceError::MissingReason);
    }
    Ok(Waiver {
        actor: actor.id.clone(),
        reason: reason.to_string(),
        applies_to_revision: revision.clone(),
        scope,
        granted_at_ms: now_ms,
    })
}

// ---------------------------------------------------------------------------
// 接受：绑定确定修订
// ---------------------------------------------------------------------------

/// 接受的五个绑定项；任一影响性变化都产生新的验证需求（spec: 接受绑定确定修订）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceBinding {
    pub work_package_revision: DomainId,
    pub input_baseline: DomainId,
    pub artifact_revision: DomainId,
    pub verification_rules: DomainId,
    pub target_environment: DomainId,
    /// 合并组合（联合交付时固定各项目修订）。
    pub merge_combination: Option<DomainId>,
}

impl AcceptanceBinding {
    /// 绑定是否与给定修订完全一致：旧接受不得批准新产物。
    pub fn binds(&self, revision: &DomainId) -> bool {
        &self.artifact_revision == revision
    }

    /// 影响性变化检测：返回第一处不匹配的字段名，便于"拒绝并要求重验"。
    pub fn divergence(&self, other: &AcceptanceBinding) -> Option<&'static str> {
        if self.work_package_revision != other.work_package_revision {
            return Some("work_package_revision");
        }
        if self.input_baseline != other.input_baseline {
            return Some("input_baseline");
        }
        if self.artifact_revision != other.artifact_revision {
            return Some("artifact_revision");
        }
        if self.verification_rules != other.verification_rules {
            return Some("verification_rules");
        }
        if self.target_environment != other.target_environment {
            return Some("target_environment");
        }
        if self.merge_combination != other.merge_combination {
            return Some("merge_combination");
        }
        None
    }
}

/// 可用证据：必须与绑定逐项对应，否则不算证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceEvidence {
    pub artifact_revision: DomainId,
    pub verification_rules: DomainId,
    pub target_environment: DomainId,
    pub verification_state: VerificationState,
    pub checks_passed: bool,
    pub blocking_risks: Vec<String>,
}

impl AcceptanceEvidence {
    pub fn from_record(record: &VerificationRecord, checks_passed: bool) -> Self {
        Self {
            artifact_revision: record.revision_id.clone(),
            verification_rules: record.verification_rules.clone(),
            target_environment: record.environment.clone(),
            verification_state: record.state,
            checks_passed,
            blocking_risks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptancePolicy {
    pub auto_accept_enabled: bool,
    /// 自动接受必须通过的检查项数量（项目预先授权，非运行时自报）。
    pub required_checks: u32,
    pub high_risk: bool,
    pub production_write: bool,
}

impl Default for AcceptancePolicy {
    /// 默认无自动接受权，且视为高风险：必须显式授权才能放宽。
    fn default() -> Self {
        Self {
            auto_accept_enabled: false,
            required_checks: 0,
            high_risk: true,
            production_write: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceDecision {
    /// 由人类主体显式接受。
    Accepted,
    /// 按预先授权策略由系统自动接受。
    AutoAccepted,
    /// 需要独立人工批准，当前不得接受。
    RequiresHuman,
}

impl AcceptanceDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::AutoAccepted => "auto_accepted",
            Self::RequiresHuman => "requires_human",
        }
    }

    pub fn is_accepted(self) -> bool {
        matches!(self, Self::Accepted | Self::AutoAccepted)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "accepted" => Some(Self::Accepted),
            "auto_accepted" => Some(Self::AutoAccepted),
            "requires_human" => Some(Self::RequiresHuman),
            _ => None,
        }
    }
}

/// 接受决策：先证明证据与绑定一致，再判断谁有权接受。
pub fn decide_acceptance(
    actor: &Principal,
    producer: &DomainId,
    binding: &AcceptanceBinding,
    evidence: &AcceptanceEvidence,
    policy: &AcceptancePolicy,
) -> Result<AcceptanceDecision, AcceptanceError> {
    // 1. 证据必须绑定同一确定修订与同一规则/环境（I-08、G-9）。
    if evidence.artifact_revision != binding.artifact_revision {
        return Err(AcceptanceError::StaleAcceptance {
            expected: binding.artifact_revision.clone(),
            actual: evidence.artifact_revision.clone(),
        });
    }
    if evidence.verification_rules != binding.verification_rules {
        return Err(AcceptanceError::StaleAcceptance {
            expected: binding.verification_rules.clone(),
            actual: evidence.verification_rules.clone(),
        });
    }
    if evidence.target_environment != binding.target_environment {
        return Err(AcceptanceError::StaleAcceptance {
            expected: binding.target_environment.clone(),
            actual: evidence.target_environment.clone(),
        });
    }

    // 2. 实现者不得自批（I-51）：即便策略允许自动接受，也不允许实现者以 Agent 身份接受。
    if actor.kind == PrincipalKind::Agent && &actor.id == producer {
        return Err(AcceptanceError::SelfApprovalDenied {
            actor: actor.id.clone(),
        });
    }

    // 3. 豁免不是验证：不得用于接受。
    if evidence.verification_state == VerificationState::Waived {
        return Err(AcceptanceError::WaiverIsNotVerification);
    }
    if !evidence.verification_state.is_verified() {
        return Err(AcceptanceError::NotVerified {
            state: evidence.verification_state,
        });
    }
    if !evidence.checks_passed {
        return Err(AcceptanceError::ChecksNotPassed);
    }
    if let Some(risk) = evidence.blocking_risks.first() {
        return Err(AcceptanceError::BlockingRisk(risk.clone()));
    }

    // 4. 人类主体显式接受：独立于自动策略。
    if actor.kind == PrincipalKind::User {
        return Ok(AcceptanceDecision::Accepted);
    }

    // 5. 生产写入、不可逆操作与高风险项要求独立人工批准。
    if policy.production_write || policy.high_risk {
        return Ok(AcceptanceDecision::RequiresHuman);
    }

    // 6. 自动接受：必须有项目预先授权且证据与版本匹配（已在上方校验）。
    if policy.auto_accept_enabled && policy.required_checks > 0 {
        return Ok(AcceptanceDecision::AutoAccepted);
    }

    Ok(AcceptanceDecision::RequiresHuman)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceRecord {
    pub acceptance_id: DomainId,
    pub artifact_id: DomainId,
    pub binding: AcceptanceBinding,
    pub decision: AcceptanceDecision,
    pub actor: DomainId,
    pub reason: String,
    pub evidence_refs: Vec<String>,
    pub accepted_at_ms: i64,
    /// 被后续接受取代时指向新的接受记录；旧记录保留但不再批准新版本。
    pub superseded_by: Option<DomainId>,
}

impl AcceptanceRecord {
    /// 该接受能否用于批准目标修订：必须未被取代且绑定同一修订。
    pub fn approves(&self, revision: &DomainId) -> bool {
        self.superseded_by.is_none() && self.binding.binds(revision)
    }
}

// ---------------------------------------------------------------------------
// 多工作包完成条件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPackageState {
    pub revision: DomainId,
    pub required: bool,
    /// 已接受修订；None 表示尚未被接受。
    pub accepted_revision: Option<DomainId>,
    /// 该工作包是否仍有活跃执行（Running/待处理）。
    pub running: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCompletion {
    pub complete: bool,
    pub pending: Vec<DomainId>,
    pub blockers: Vec<String>,
}

/// Issue 只在**必需**工作包全部接受且终结门禁通过后完成。
/// `agent_reported_done` 只是执行端自报，永远不足以完成 Issue（spec: 部分工作包已接受）。
pub fn evaluate_issue_completion(
    packages: &[WorkPackageState],
    final_gate_passed: bool,
    agent_reported_done: bool,
) -> IssueCompletion {
    let mut pending = Vec::new();
    let mut blockers = Vec::new();

    for package in packages {
        if package.required && package.accepted_revision.is_none() {
            pending.push(package.revision.clone());
            if package.running {
                blockers.push(format!("{}: 执行中", package.revision.as_str()));
            } else {
                blockers.push(format!("{}: 尚未接受", package.revision.as_str()));
            }
        }
        for blocker in &package.blockers {
            blockers.push(blocker.clone());
        }
    }

    if !final_gate_passed {
        blockers.push("终结门禁未通过".to_string());
    }

    if agent_reported_done && !pending.is_empty() {
        blockers.push("执行端自报完成不足以完成 Issue".to_string());
    }

    let complete = pending.is_empty() && final_gate_passed && !packages.is_empty();
    IssueCompletion {
        complete,
        pending,
        blockers,
    }
}

// ---------------------------------------------------------------------------
// 验证失败回路
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptDraft {
    pub attempt_key: String,
    pub attempt_seq: u32,
    pub issue_phase: Phase,
    pub reason: String,
    /// 被保留的历史约定：规格修订与验收修订不被覆盖。
    pub spec_revision: DomainId,
    pub acceptance_revision: DomainId,
}

/// 验证失败不覆盖历史约定：保留原规格/验收修订，生成新 Attempt 与 NeedsRevision。
pub fn on_verification_failure(
    previous_attempt_key: &str,
    attempt_seq: u32,
    reason: &str,
    spec_revision: &DomainId,
    acceptance_revision: &DomainId,
) -> Result<AttemptDraft, AcceptanceError> {
    if reason.trim().is_empty() {
        return Err(AcceptanceError::MissingReason);
    }
    let next_seq = attempt_seq.saturating_add(1);
    Ok(AttemptDraft {
        attempt_key: format!("{previous_attempt_key}#attempt-{next_seq}"),
        attempt_seq: next_seq,
        issue_phase: Phase::Blocked,
        reason: reason.to_string(),
        spec_revision: spec_revision.clone(),
        acceptance_revision: acceptance_revision.clone(),
    })
}

// ---------------------------------------------------------------------------
// 决策记录与答疑边界
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    pub decision_id: DomainId,
    pub actor: DomainId,
    pub scope: Scope,
    pub decision: String,
    pub reason: String,
    pub evidence_refs: Vec<String>,
    pub revision_refs: Vec<DomainId>,
    pub created_at_ms: i64,
}

/// 一次答疑可能触及的影响面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnswerImpact {
    pub changes_interface_behavior: bool,
    pub changes_permission: bool,
    pub changes_acceptance: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerEffect {
    /// 只是解释既有事实，可直接答复。
    Informational,
    /// 改变接口/行为：必须走规格修订。
    RequiresSpecRevision,
    /// 改变权限：必须走授权流程。
    RequiresAuthorization,
    /// 改变验收条件：必须走规格/验收修订。
    RequiresAcceptanceChange,
}

impl AnswerEffect {
    /// 任何非 Informational 的答疑都不得被视为已批准规格。
    pub fn mutates_requirements(self) -> bool {
        !matches!(self, Self::Informational)
    }

    pub fn required_next_step(self) -> &'static str {
        match self {
            Self::Informational => "可直接回答；不得据此改变需求、权限或验收",
            Self::RequiresSpecRevision => "提交规格修订并由负责人接受后才影响验收",
            Self::RequiresAuthorization => "提交授权变更；答疑本身不授予权限",
            Self::RequiresAcceptanceChange => "提交验收条件修订，旧接受不随之扩展",
        }
    }
}

pub fn classify_answer(impact: AnswerImpact) -> AnswerEffect {
    if impact.changes_permission {
        return AnswerEffect::RequiresAuthorization;
    }
    if impact.changes_acceptance {
        return AnswerEffect::RequiresAcceptanceChange;
    }
    if impact.changes_interface_behavior {
        return AnswerEffect::RequiresSpecRevision;
    }
    AnswerEffect::Informational
}

// ---------------------------------------------------------------------------
// 稳定错误
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptanceError {
    EmptyRevision,
    UnusableDigest,
    InvalidSize,
    MissingMime,
    StorageDigestMismatch,
    EmptyDependency,
    IllegalTransition {
        from: VerificationState,
        to: VerificationState,
    },
    MissingReason,
    WaiverRequiresHuman,
    WaiverRevisionMismatch,
    WaiverIsNotVerification,
    StaleAcceptance {
        expected: DomainId,
        actual: DomainId,
    },
    SelfApprovalDenied {
        actor: DomainId,
    },
    NotVerified {
        state: VerificationState,
    },
    ChecksNotPassed,
    BlockingRisk(String),
    UnknownState(String),
}

impl AcceptanceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyRevision => "E_EMPTY_REVISION",
            Self::UnusableDigest => "E_UNUSABLE_DIGEST",
            Self::InvalidSize => "E_INVALID_SIZE",
            Self::MissingMime => "E_MISSING_MIME",
            Self::StorageDigestMismatch => "E_STORAGE_DIGEST_MISMATCH",
            Self::EmptyDependency => "E_EMPTY_DEPENDENCY",
            Self::IllegalTransition { .. } => "E_ILLEGAL_TRANSITION",
            Self::MissingReason => "E_MISSING_REASON",
            Self::WaiverRequiresHuman => "E_WAIVER_REQUIRES_HUMAN",
            Self::WaiverRevisionMismatch => "E_WAIVER_REVISION_MISMATCH",
            Self::WaiverIsNotVerification => "E_WAIVER_IS_NOT_VERIFICATION",
            Self::StaleAcceptance { .. } => "E_STALE_ACCEPTANCE",
            Self::SelfApprovalDenied { .. } => "E_SELF_APPROVAL_DENIED",
            Self::NotVerified { .. } => "E_NOT_VERIFIED",
            Self::ChecksNotPassed => "E_CHECKS_NOT_PASSED",
            Self::BlockingRisk(_) => "E_BLOCKING_RISK",
            Self::UnknownState(_) => "E_UNKNOWN_STATE",
        }
    }
}

impl std::fmt::Display for AcceptanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRevision => write!(f, "修订缺少 ID"),
            Self::UnusableDigest => write!(f, "摘要不可用：不得伪造 hash"),
            Self::InvalidSize => write!(f, "字节数不得为负"),
            Self::MissingMime => write!(f, "缺少 MIME 类型"),
            Self::StorageDigestMismatch => write!(f, "CAS 引用与摘要不一致"),
            Self::EmptyDependency => write!(f, "依赖必须指向确定修订"),
            Self::IllegalTransition { from, to } => {
                write!(f, "非法验证状态转换：{} → {}", from.as_str(), to.as_str())
            }
            Self::MissingReason => write!(f, "必须记录理由"),
            Self::WaiverRequiresHuman => write!(f, "豁免必须由人类主体授予"),
            Self::WaiverRevisionMismatch => write!(f, "豁免版本与目标修订不一致"),
            Self::WaiverIsNotVerification => write!(f, "已豁免不等于验证通过，不能用于接受"),
            Self::StaleAcceptance { expected, actual } => write!(
                f,
                "旧接受不能批准新修订：期望 {}，实际 {}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::SelfApprovalDenied { actor } => {
                write!(f, "实现者不得自行批准自己的成果：{}", actor.as_str())
            }
            Self::NotVerified { state } => write!(f, "验证状态为 {}", state.as_str()),
            Self::ChecksNotPassed => write!(f, "必需检查未通过"),
            Self::BlockingRisk(risk) => write!(f, "存在阻塞风险：{risk}"),
            Self::UnknownState(value) => write!(f, "未知验证状态：{value}"),
        }
    }
}

impl std::error::Error for AcceptanceError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> DomainId {
        DomainId::new(value)
    }

    fn user(value: &str) -> Principal {
        Principal {
            id: id(value),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    fn agent(value: &str) -> Principal {
        Principal {
            id: id(value),
            kind: PrincipalKind::Agent,
            claimed_roles: vec!["reviewer".to_string()],
        }
    }

    fn binding(revision: &str) -> AcceptanceBinding {
        AcceptanceBinding {
            work_package_revision: id("wp-r1"),
            input_baseline: id("baseline-1"),
            artifact_revision: id(revision),
            verification_rules: id("rules-v1"),
            target_environment: id("env-local"),
            merge_combination: None,
        }
    }

    fn verified_evidence(revision: &str) -> AcceptanceEvidence {
        AcceptanceEvidence {
            artifact_revision: id(revision),
            verification_rules: id("rules-v1"),
            target_environment: id("env-local"),
            verification_state: VerificationState::Verified,
            checks_passed: true,
            blocking_risks: Vec::new(),
        }
    }

    mod digest {
        use super::*;

        #[test]
        fn sha256_matches_published_vectors() {
            assert_eq!(
                sha256_hex(b""),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            );
            assert_eq!(
                sha256_hex(b"abc"),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
            assert_eq!(
                sha256_hex(b"The quick brown fox jumps over the lazy dog"),
                "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
            );
        }

        #[test]
        fn digest_round_trip_and_rejects_malformed() {
            let digest = Digest::of_bytes(b"abc");
            assert!(digest.is_usable());
            assert_eq!(Digest::parse(&digest.as_string()), Some(digest));
            assert_eq!(Digest::parse("sha256:short"), None);
            assert_eq!(Digest::parse("md5:abc"), None);
            assert_eq!(Digest::parse("no-colon"), None);
        }
    }

    mod invariant_i_07 {
        use super::*;

        #[test]
        fn external_reference_is_never_local_cas() {
            let base = ArtifactRevision {
                revision_id: id("rev-1"),
                artifact_id: id("payment-design"),
                base_revision: None,
                storage: StorageRef::External("git:repo-1@commit-abc:docs/design.md".to_string()),
                digest: Digest::of_bytes(b"design"),
                size_bytes: 6,
                mime: "text/markdown".to_string(),
                produced_by: id("coder-1"),
                source_note: "Git commit 固定引用".to_string(),
                depends_on: Vec::new(),
                created_at_ms: 1_763_500_000_000,
            };
            base.validate().expect("外部引用符合修订完整性");
            assert!(base.storage.is_external());
            assert!(!base.storage.is_local_cas(), "外部对象不进入本地 GC 范围");
        }

        #[test]
        fn cas_reference_must_match_digest() {
            let digest = Digest::of_bytes(b"hello");
            let mut revision = ArtifactRevision {
                revision_id: id("rev-1"),
                artifact_id: id("report"),
                base_revision: None,
                storage: StorageRef::Cas(digest.value.clone()),
                digest: digest.clone(),
                size_bytes: 5,
                mime: "text/plain".to_string(),
                produced_by: id("coder-1"),
                source_note: "本地 CAS".to_string(),
                depends_on: Vec::new(),
                created_at_ms: 1,
            };
            revision.validate().expect("引用与摘要一致");
            revision.storage = StorageRef::Cas("deadbeef".to_string());
            assert_eq!(
                revision.validate(),
                Err(AcceptanceError::StorageDigestMismatch)
            );
        }

        #[test]
        fn unusable_digest_is_rejected_not_faked() {
            let mut revision = ArtifactRevision {
                revision_id: id("rev-1"),
                artifact_id: id("report"),
                base_revision: None,
                storage: StorageRef::Cas(String::new()),
                digest: Digest {
                    algorithm: DigestAlgorithm::Sha256,
                    value: String::new(),
                },
                size_bytes: 0,
                mime: "text/plain".to_string(),
                produced_by: id("coder-1"),
                source_note: String::new(),
                depends_on: Vec::new(),
                created_at_ms: 1,
            };
            assert_eq!(revision.validate(), Err(AcceptanceError::UnusableDigest));
            revision.digest = Digest::of_bytes(b"");
            revision.size_bytes = -1;
            assert_eq!(revision.validate(), Err(AcceptanceError::InvalidSize));
        }
    }

    mod invariant_i_08 {
        use super::*;

        #[test]
        fn old_acceptance_cannot_approve_new_revision() {
            let old = AcceptanceRecord {
                acceptance_id: id("acc-1"),
                artifact_id: id("payment-design"),
                binding: binding("rev-1"),
                decision: AcceptanceDecision::Accepted,
                actor: id("user-1"),
                reason: "评审通过".to_string(),
                evidence_refs: vec!["report-1".to_string()],
                accepted_at_ms: 1,
                superseded_by: None,
            };
            assert!(old.approves(&id("rev-1")));
            assert!(!old.approves(&id("rev-2")), "旧接受不得批准新产物");
        }

        #[test]
        fn superseded_acceptance_stops_approving() {
            let mut record = AcceptanceRecord {
                acceptance_id: id("acc-1"),
                artifact_id: id("payment-design"),
                binding: binding("rev-1"),
                decision: AcceptanceDecision::Accepted,
                actor: id("user-1"),
                reason: String::new(),
                evidence_refs: Vec::new(),
                accepted_at_ms: 1,
                superseded_by: None,
            };
            record.superseded_by = Some(id("acc-2"));
            assert!(!record.approves(&id("rev-1")));
        }

        #[test]
        fn evidence_from_other_rules_or_environment_is_stale() {
            let mut evidence = verified_evidence("rev-2");
            evidence.verification_rules = id("rules-v2");
            let err = decide_acceptance(
                &user("user-1"),
                &id("coder-1"),
                &binding("rev-2"),
                &evidence,
                &AcceptancePolicy::default(),
            )
            .expect_err("规则版本不匹配必须拒绝");
            assert_eq!(err.code(), "E_STALE_ACCEPTANCE");

            let mut wrong_env = verified_evidence("rev-2");
            wrong_env.target_environment = id("env-prod");
            assert_eq!(
                decide_acceptance(
                    &user("user-1"),
                    &id("coder-1"),
                    &binding("rev-2"),
                    &wrong_env,
                    &AcceptancePolicy::default(),
                )
                .unwrap_err()
                .code(),
                "E_STALE_ACCEPTANCE"
            );
        }

        #[test]
        fn binding_divergence_names_the_changed_field() {
            let mut other = binding("rev-1");
            assert_eq!(binding("rev-1").divergence(&other), None);
            other.input_baseline = id("baseline-2");
            assert_eq!(binding("rev-1").divergence(&other), Some("input_baseline"));
        }
    }

    mod invariant_i_51 {
        use super::*;

        #[test]
        fn implementer_cannot_approve_own_work() {
            let err = decide_acceptance(
                &agent("coder-1"),
                &id("coder-1"),
                &binding("rev-1"),
                &verified_evidence("rev-1"),
                &AcceptancePolicy {
                    auto_accept_enabled: true,
                    required_checks: 3,
                    high_risk: false,
                    production_write: false,
                },
            )
            .expect_err("实现者自批必须拒绝");
            assert_eq!(err.code(), "E_SELF_APPROVAL_DENIED");
        }

        #[test]
        fn self_reported_verification_does_not_change_actor_rights() {
            let record = advance_verification(
                VerificationState::Verifying,
                &VerificationDraft {
                    next: VerificationState::Verified,
                    actor: &id("coder-1"),
                    reason: "我自己跑过了",
                    revision: &id("rev-1"),
                    rules: &id("rules-v1"),
                    environment: &id("env-local"),
                    evidence_refs: Vec::new(),
                    observed_at_ms: 1,
                },
            )
            .expect("转换本身合法");
            assert!(record.is_self_reported(&id("coder-1")));
            // 自报不产生接受权：同一 Agent 仍被拒绝。
            assert_eq!(
                decide_acceptance(
                    &agent("coder-1"),
                    &id("coder-1"),
                    &binding("rev-1"),
                    &AcceptanceEvidence::from_record(&record, true),
                    &AcceptancePolicy::default(),
                )
                .unwrap_err()
                .code(),
                "E_SELF_APPROVAL_DENIED"
            );
        }

        #[test]
        fn other_agent_needs_human_when_high_risk() {
            let decision = decide_acceptance(
                &agent("reviewer-agent"),
                &id("coder-1"),
                &binding("rev-1"),
                &verified_evidence("rev-1"),
                &AcceptancePolicy::default(),
            )
            .expect("非实现者可评估");
            assert_eq!(decision, AcceptanceDecision::RequiresHuman);
        }

        #[test]
        fn authorized_auto_accept_only_for_low_risk_with_checks() {
            let policy = AcceptancePolicy {
                auto_accept_enabled: true,
                required_checks: 2,
                high_risk: false,
                production_write: false,
            };
            assert_eq!(
                decide_acceptance(
                    &agent("reviewer-agent"),
                    &id("coder-1"),
                    &binding("rev-1"),
                    &verified_evidence("rev-1"),
                    &policy,
                )
                .expect("策略允许时自动接受"),
                AcceptanceDecision::AutoAccepted
            );

            let disabled = AcceptancePolicy {
                auto_accept_enabled: false,
                ..policy.clone()
            };
            assert_eq!(
                decide_acceptance(
                    &agent("reviewer-agent"),
                    &id("coder-1"),
                    &binding("rev-1"),
                    &verified_evidence("rev-1"),
                    &disabled,
                )
                .unwrap(),
                AcceptanceDecision::RequiresHuman
            );

            let production = AcceptancePolicy {
                production_write: true,
                ..policy
            };
            assert_eq!(
                decide_acceptance(
                    &agent("reviewer-agent"),
                    &id("coder-1"),
                    &binding("rev-1"),
                    &verified_evidence("rev-1"),
                    &production,
                )
                .unwrap(),
                AcceptanceDecision::RequiresHuman
            );
        }

        #[test]
        fn blocking_risk_prevents_acceptance() {
            let mut evidence = verified_evidence("rev-1");
            evidence.blocking_risks = vec!["迁移未回滚验证".to_string()];
            assert_eq!(
                decide_acceptance(
                    &user("user-1"),
                    &id("coder-1"),
                    &binding("rev-1"),
                    &evidence,
                    &AcceptancePolicy::default(),
                )
                .unwrap_err()
                .code(),
                "E_BLOCKING_RISK"
            );
        }
    }

    mod verification {
        use super::*;

        #[test]
        fn waived_is_not_verified_and_cannot_be_accepted() {
            let mut evidence = verified_evidence("rev-1");
            evidence.verification_state = VerificationState::Waived;
            assert_eq!(
                decide_acceptance(
                    &user("user-1"),
                    &id("coder-1"),
                    &binding("rev-1"),
                    &evidence,
                    &AcceptancePolicy::default(),
                )
                .unwrap_err(),
                AcceptanceError::WaiverIsNotVerification
            );
            assert!(!VerificationState::Waived.is_verified());
            assert!(
                !Waiver {
                    actor: id("user-1"),
                    reason: "低风险".to_string(),
                    applies_to_revision: id("rev-1"),
                    scope: Scope {
                        company_id: id("company-default"),
                        project_id: None,
                        issue_id: None,
                    },
                    granted_at_ms: 1,
                }
                .counts_as_verified()
            );
            assert_eq!(VerificationState::Waived.label(), "已豁免（未验证）");
        }

        #[test]
        fn waiver_requires_human_and_reason_and_matching_revision() {
            let scope = Scope {
                company_id: id("company-default"),
                project_id: None,
                issue_id: None,
            };
            assert_eq!(
                grant_waiver(&agent("coder-1"), &id("rev-1"), "赶时间", scope.clone(), 1)
                    .unwrap_err(),
                AcceptanceError::WaiverRequiresHuman
            );
            assert_eq!(
                grant_waiver(&user("user-1"), &id("rev-1"), "  ", scope.clone(), 1).unwrap_err(),
                AcceptanceError::MissingReason
            );
            let waiver =
                grant_waiver(&user("user-1"), &id("rev-1"), "低风险文案", scope, 5).unwrap();
            assert_eq!(
                waiver.validate(&id("rev-2")),
                Err(AcceptanceError::WaiverRevisionMismatch)
            );
            waiver.validate(&id("rev-1")).unwrap();
        }

        #[test]
        fn terminal_states_are_not_overturned_by_late_detail() {
            assert!(!VerificationState::Verified.can_transition_to(VerificationState::Verifying));
            let err = advance_verification(
                VerificationState::Verified,
                &VerificationDraft {
                    next: VerificationState::Verifying,
                    actor: &id("user-1"),
                    reason: "迟到的日志",
                    revision: &id("rev-1"),
                    rules: &id("rules-v1"),
                    environment: &id("env-local"),
                    evidence_refs: Vec::new(),
                    observed_at_ms: 1,
                },
            )
            .expect_err("终态不得被迟到 detail 推翻");
            assert_eq!(err.code(), "E_ILLEGAL_TRANSITION");
        }

        #[test]
        fn rejection_requires_reason() {
            assert_eq!(
                advance_verification(
                    VerificationState::Verifying,
                    &VerificationDraft {
                        next: VerificationState::Rejected,
                        actor: &id("user-1"),
                        reason: "",
                        revision: &id("rev-1"),
                        rules: &id("rules-v1"),
                        environment: &id("env-local"),
                        evidence_refs: Vec::new(),
                        observed_at_ms: 1,
                    },
                )
                .unwrap_err(),
                AcceptanceError::MissingReason
            );
        }

        #[test]
        fn state_round_trip_covers_all_variants() {
            for state in [
                VerificationState::Pending,
                VerificationState::Verifying,
                VerificationState::Verified,
                VerificationState::Rejected,
                VerificationState::NeedsRevision,
                VerificationState::Waived,
            ] {
                assert_eq!(VerificationState::parse(state.as_str()), Some(state));
            }
            assert_eq!(VerificationState::parse("unknown"), None);
        }
    }

    mod work_packages {
        use super::*;

        fn package(
            revision: &str,
            required: bool,
            accepted: Option<&str>,
            running: bool,
        ) -> WorkPackageState {
            WorkPackageState {
                revision: id(revision),
                required,
                accepted_revision: accepted.map(id),
                running,
                blockers: Vec::new(),
            }
        }

        #[test]
        fn partially_accepted_issue_stays_open_with_blockers() {
            let completion = evaluate_issue_completion(
                &[
                    package("wp-1", true, Some("rev-1"), false),
                    package("wp-2", true, None, true),
                ],
                true,
                true,
            );
            assert!(!completion.complete);
            assert_eq!(completion.pending, vec![id("wp-2")]);
            assert!(completion.blockers.iter().any(|b| b.contains("执行中")));
        }

        #[test]
        fn agent_report_alone_never_completes_issue() {
            let completion =
                evaluate_issue_completion(&[package("wp-1", true, None, false)], true, true);
            assert!(!completion.complete);
            assert!(
                completion
                    .blockers
                    .iter()
                    .any(|b| b.contains("自报完成不足以完成"))
            );
        }

        #[test]
        fn optional_package_does_not_block_completion() {
            let completion = evaluate_issue_completion(
                &[
                    package("wp-1", true, Some("rev-1"), false),
                    package("wp-2", false, None, false),
                ],
                true,
                false,
            );
            assert!(completion.complete);
        }

        #[test]
        fn final_gate_is_required() {
            let completion = evaluate_issue_completion(
                &[package("wp-1", true, Some("rev-1"), false)],
                false,
                false,
            );
            assert!(!completion.complete);
            assert!(completion.blockers.iter().any(|b| b.contains("终结门禁")));
        }

        #[test]
        fn empty_issue_is_not_complete() {
            assert!(!evaluate_issue_completion(&[], true, false).complete);
        }
    }

    mod revision_failure {
        use super::*;

        #[test]
        fn failure_creates_new_attempt_and_preserves_history() {
            let draft =
                on_verification_failure("inv-1", 2, "覆盖率不足", &id("spec-r3"), &id("acc-r2"))
                    .expect("有理由即可生成新尝试");
            assert_eq!(draft.attempt_seq, 3);
            assert_eq!(draft.attempt_key, "inv-1#attempt-3");
            assert_eq!(draft.issue_phase, Phase::Blocked);
            assert_eq!(draft.spec_revision, id("spec-r3"), "历史规格修订不被覆盖");
            assert_eq!(draft.acceptance_revision, id("acc-r2"));
        }

        #[test]
        fn failure_without_reason_is_rejected() {
            assert_eq!(
                on_verification_failure("inv-1", 1, "", &id("spec-r1"), &id("acc-r1")).unwrap_err(),
                AcceptanceError::MissingReason
            );
        }
    }

    mod decisions {
        use super::*;

        #[test]
        fn plain_answer_cannot_change_requirements() {
            let effect = classify_answer(AnswerImpact::default());
            assert_eq!(effect, AnswerEffect::Informational);
            assert!(!effect.mutates_requirements());

            let interface = classify_answer(AnswerImpact {
                changes_interface_behavior: true,
                ..Default::default()
            });
            assert_eq!(interface, AnswerEffect::RequiresSpecRevision);
            assert!(interface.mutates_requirements());

            let permission = classify_answer(AnswerImpact {
                changes_permission: true,
                changes_interface_behavior: true,
                changes_acceptance: true,
            });
            assert_eq!(permission, AnswerEffect::RequiresAuthorization);

            let acceptance = classify_answer(AnswerImpact {
                changes_acceptance: true,
                ..Default::default()
            });
            assert_eq!(acceptance, AnswerEffect::RequiresAcceptanceChange);
            assert!(!permission.required_next_step().is_empty());
        }
    }
}
