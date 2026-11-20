//! P3 任务 2.1—2.5：依赖边、影响评估与有界重验（`impact-analysis`）。
//!
//! 这一层的立场是**宁缺毋滥**：找不到关系时，结论只能是"未知"，不能是"不受影响"。
//! 现实里"没人写依赖"和"没有依赖"长得一模一样，把两者混为一谈，就是漏项的开始。
//!
//! 四件事：
//! 1. [`DependencyEdge`] 分**显式**与**推测**两类。推测边带依据与置信，参与"可能受影响"，
//!    但永远不升级为确定事实——没有 [`EdgeSource::Explicit`] 就不会进入"确定"集合。
//! 2. [`assess`] 产出冻结的 [`ImpactAssessment`]：旧/新基线、关系快照、规则版本、触发事件、
//!    分析时间，一个不少。区域分三类：确定、可能、未知（含"没有任何关系"的）。
//! 3. [`propose`] 只对候选区域生成**有限**重验提案：有幂等键、有目标上限、有预算，
//!    超出的显式排队而不是悄悄丢掉。重验提案**不修改**旧接受，也不删除成果。
//! 4. [`maintenance_proposals`] 处理观测缺失：断网/版本漂移 → `unknown`/`stale` 提案，
//!    默认只读检查，"没有告警"不等于"健康"。

use crate::domain::artifact::sha256_hex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

// ---------------------------------------------------------------------------
// 依赖边
// ---------------------------------------------------------------------------

/// 修订所属的类型。影响分析要能回答"这是接口还是监控"，否则无法分辨候选。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactKind {
    Spec,
    Api,
    Source,
    Test,
    Build,
    Environment,
    Acceptance,
    /// 观测、监控、报表一类：默认不进重验候选，除非有显式依赖边。
    Monitoring,
}

impl ArtifactKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::Api => "api",
            Self::Source => "source",
            Self::Test => "test",
            Self::Build => "build",
            Self::Environment => "environment",
            Self::Acceptance => "acceptance",
            Self::Monitoring => "monitoring",
        }
    }
}

/// 一个确定的修订引用。`digest` 是内容摘要，`kind` 说明它是什么。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RevisionId {
    pub kind: ArtifactKind,
    pub digest: String,
}

impl RevisionId {
    pub fn new(kind: ArtifactKind, digest: &str) -> Self {
        Self {
            kind,
            digest: digest.to_string(),
        }
    }

    /// 便于测试与报告阅读的短标签：`api:1a2b3c4d`。
    pub fn short(&self) -> String {
        let head = &self.digest[..self.digest.len().min(8)];
        format!("{}:{}", self.kind.as_str(), head)
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short())
    }
}

/// 关系类型。规格列了五种；新增类型要同步本文档与测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    Requires,
    Implements,
    Verifies,
    BuiltFrom,
    DeployedTo,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Requires => "requires",
            Self::Implements => "implements",
            Self::Verifies => "verifies",
            Self::BuiltFrom => "built_from",
            Self::DeployedTo => "deployed_to",
        }
    }
}

/// 边的来源。`Inferred` 必须带依据与置信，且不参与"确定受影响"计算。
///
/// 置信用**万分比整数**（`confidence_bp`）而不是浮点：`f64` 的 NaN 会破坏 `Eq`，
/// 让"这条边是否同一条边"变得不可判定；整数也没有 0.1+0.2 那种比较陷阱。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeSource {
    /// 显式声明：源码 import、构建清单、部署清单、规格引用等可机械判定的关系。
    Explicit { by: String },
    /// Agent 推测：命名相似、历史共现等。`confidence_bp` 取值 0—10000。
    Inferred { basis: String, confidence_bp: u16 },
}

impl EdgeSource {
    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit { .. })
    }

    /// 置信度（0.0—1.0）。只用于展示与排序，不参与事实判定。
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Self::Explicit { .. } => None,
            Self::Inferred { confidence_bp, .. } => Some(f64::from(*confidence_bp) / 10_000.0),
        }
    }
}

/// 一条依赖边：`from` 依赖 `to`（`to` 是上游）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyEdge {
    pub from: RevisionId,
    pub to: RevisionId,
    pub kind: EdgeKind,
    pub source: EdgeSource,
    /// 适用范围：本边在哪个组件/环境范围内成立，避免"全局生效"的隐式放大。
    pub scope: String,
}

impl DependencyEdge {
    pub fn explicit(
        from: RevisionId,
        to: RevisionId,
        kind: EdgeKind,
        by: &str,
        scope: &str,
    ) -> Self {
        Self {
            from,
            to,
            kind,
            source: EdgeSource::Explicit { by: by.to_string() },
            scope: scope.to_string(),
        }
    }

    pub fn inferred(
        from: RevisionId,
        to: RevisionId,
        kind: EdgeKind,
        basis: &str,
        confidence: f64,
        scope: &str,
    ) -> Self {
        let clamped = if confidence.is_finite() {
            confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            from,
            to,
            kind,
            source: EdgeSource::Inferred {
                basis: basis.to_string(),
                confidence_bp: (clamped * 10_000.0).round() as u16,
            },
            scope: scope.to_string(),
        }
    }

    pub fn describe(&self) -> String {
        match &self.source {
            EdgeSource::Explicit { by } => {
                format!("{}({}) 由 {} 显式声明", self.kind.as_str(), self.scope, by)
            }
            EdgeSource::Inferred {
                basis,
                confidence_bp,
            } => format!(
                "{}({}) 为推测边，依据：{}，置信 {:.2}",
                self.kind.as_str(),
                self.scope,
                basis,
                f64::from(*confidence_bp) / 10_000.0
            ),
        }
    }
}

/// 推测边的升级申请。只有拿到显式确认，才允许把推测边当事实用。
///
/// 这个类型存在本身就是口径：升级不是"再算一遍置信度"，而是**换一个来源**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgePromotion {
    pub edge_from: RevisionId,
    pub edge_to: RevisionId,
    pub approved_by: String,
    pub evidence: String,
}

/// 用一次显式确认把推测边替换成显式边。没有 `EdgePromotion` 就没有升级路径。
pub fn promote_inferred(
    edge: &DependencyEdge,
    promotion: &EdgePromotion,
) -> Result<DependencyEdge, ImpactError> {
    if edge.from != promotion.edge_from || edge.to != promotion.edge_to {
        return Err(ImpactError::PromotionMismatch {
            expected: format!("{} → {}", edge.from, edge.to),
            actual: format!("{} → {}", promotion.edge_from, promotion.edge_to),
        });
    }
    if promotion.approved_by.trim().is_empty() || promotion.evidence.trim().is_empty() {
        return Err(ImpactError::PromotionWithoutEvidence);
    }
    Ok(DependencyEdge {
        from: edge.from.clone(),
        to: edge.to.clone(),
        kind: edge.kind,
        source: EdgeSource::Explicit {
            by: promotion.approved_by.clone(),
        },
        scope: edge.scope.clone(),
    })
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImpactError {
    /// 升级申请指向的不是这条边。
    PromotionMismatch { expected: String, actual: String },
    /// 升级没有确认人/证据。
    PromotionWithoutEvidence,
    /// 评估已经冻结，不能再按其派发（要重新评估）。
    AssessmentSuperseded { assessment_id: String, by: String },
    /// 幂等键不存在。
    ProposalNotFound { proposal_id: String },
}

impl fmt::Display for ImpactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PromotionMismatch { expected, actual } => {
                write!(f, "升级申请与边不匹配：期望 {expected}，收到 {actual}")
            }
            Self::PromotionWithoutEvidence => {
                write!(
                    f,
                    "推测边升级必须带确认人与证据；无证据的升级等于把猜测变成事实"
                )
            }
            Self::AssessmentSuperseded { assessment_id, by } => {
                write!(f, "评估 {assessment_id} 已被 {by} 取代，不能再据此派发")
            }
            Self::ProposalNotFound { proposal_id } => {
                write!(f, "提案 {proposal_id} 不存在")
            }
        }
    }
}

impl std::error::Error for ImpactError {}

// ---------------------------------------------------------------------------
// 触发与评估
// ---------------------------------------------------------------------------

/// 变化的类别。部分类别必须强制人工判断，不能因为"预算内"就自动通过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// 普通接口/源码变化。
    Interface,
    Source,
    /// 安全相关。
    Safety,
    /// 公共迁移（数据库 schema 等）。
    PublicMigration,
    /// 工具链升级。
    Toolchain,
    /// 部署环境变化。
    Environment,
    /// 跨组件契约。
    CrossComponentContract,
}

impl ChangeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Source => "source",
            Self::Safety => "safety",
            Self::PublicMigration => "public_migration",
            Self::Toolchain => "toolchain",
            Self::Environment => "environment",
            Self::CrossComponentContract => "cross_component_contract",
        }
    }

    /// 是否强制走人工判断。
    pub fn forces_human_review(&self) -> bool {
        matches!(
            self,
            Self::Safety
                | Self::PublicMigration
                | Self::Toolchain
                | Self::Environment
                | Self::CrossComponentContract
        )
    }
}

/// 触发一次评估的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeTrigger {
    pub change_id: String,
    pub revision: RevisionId,
    pub kind: ChangeKind,
    pub description: String,
}

/// 一个区域在评估里的位置。`hit` 说明怎么被找到的，`note` 给人看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionHit {
    pub region: RevisionId,
    /// 关系的可读描述（`implements(web)` 等）。未知区域为空。
    pub via: Vec<String>,
    pub note: String,
}

/// 影响评估。字段对应规格「评估 MUST 固定旧/新基线、关系快照、规则版本、触发事件与分析时间」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactAssessment {
    pub assessment_id: String,
    pub trigger: ChangeTrigger,
    pub old_baseline: String,
    pub new_baseline: String,
    /// 冻结的关系快照。后续新增的边不改变这份评估。
    pub relation_snapshot: Vec<DependencyEdge>,
    pub rule_version: String,
    pub analyzed_at_ms: i64,
    /// 确定受影响（只经显式边可达）。
    pub certain: Vec<RegionHit>,
    /// 可能受影响（经推测边可达，或经显式边但关系类型无法确定下游行为）。
    pub possible: Vec<RegionHit>,
    /// 影响未知：没有可解析的关系，或根本没被任何边连到。
    pub unknown: Vec<RegionHit>,
}

impl ImpactAssessment {
    pub fn status(&self, region: &RevisionId) -> RegionStatus {
        if self.trigger.revision == *region {
            return RegionStatus::Changed;
        }
        if self.certain.iter().any(|hit| &hit.region == region) {
            return RegionStatus::Certain;
        }
        if self.possible.iter().any(|hit| &hit.region == region) {
            return RegionStatus::Possible;
        }
        RegionStatus::Unknown
    }

    /// 重验候选：变化点本身 + 确定 + 可能。未知区域不进候选（它需要先补关系）。
    pub fn candidates(&self) -> Vec<RevisionId> {
        let mut out = vec![self.trigger.revision.clone()];
        out.extend(self.certain.iter().map(|hit| hit.region.clone()));
        out.extend(self.possible.iter().map(|hit| hit.region.clone()));
        let mut seen = BTreeSet::new();
        out.retain(|item| seen.insert(item.clone()));
        out
    }

    /// 这条评估是否已过期：关系快照里的边与当前边集合是否一致。
    pub fn is_stale_against(&self, current: &[DependencyEdge]) -> bool {
        relation_set(&self.relation_snapshot) != relation_set(current)
    }
}

/// 区域状态。`Unknown` 是**结论**，不是"待补"——报告里必须这样写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionStatus {
    Changed,
    Certain,
    Possible,
    Unknown,
}

impl RegionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Certain => "certain",
            Self::Possible => "possible",
            Self::Unknown => "unknown",
        }
    }
}

fn relation_set(edges: &[DependencyEdge]) -> BTreeSet<(String, String, &'static str, bool)> {
    edges
        .iter()
        .map(|edge| {
            (
                edge.from.short(),
                edge.to.short(),
                edge.kind.as_str(),
                edge.source.is_explicit(),
            )
        })
        .collect()
}

/// 评估输入：关系、已知区域清单、基线、规则版本。
#[derive(Debug, Clone)]
pub struct AssessmentInput<'a> {
    pub old_baseline: &'a str,
    pub new_baseline: &'a str,
    pub rule_version: &'a str,
    /// 项目里已知的全部区域。用来判"没被连到"而不是"没有这个区域"。
    pub known_regions: &'a [RevisionId],
    pub edges: &'a [DependencyEdge],
}

/// 生成影响评估（确定性部分）。
///
/// 只做图上的可达性，不做语义判断：
/// - 显式边 → `certain`；
/// - 推测边 → `possible`，附依据与置信；
/// - 已知但不可达 → `unknown`，理由是"没有可解析的关系"。
pub fn assess(
    trigger: &ChangeTrigger,
    input: &AssessmentInput<'_>,
    now_ms: i64,
) -> ImpactAssessment {
    let mut certain: BTreeMap<RevisionId, Vec<String>> = BTreeMap::new();
    let mut possible: BTreeMap<RevisionId, (Vec<String>, u16)> = BTreeMap::new();
    let mut visited: BTreeSet<RevisionId> = BTreeSet::new();
    let mut queue: VecDeque<RevisionId> = VecDeque::new();

    // 依赖边方向：from 依赖 to；变化在 to 上时，受影响的是 from。
    // 因此从触发点出发，沿"谁依赖我"（反向）扩散。
    let mut dependents: BTreeMap<&RevisionId, Vec<&DependencyEdge>> = BTreeMap::new();
    for edge in input.edges {
        dependents.entry(&edge.to).or_default().push(edge);
    }

    queue.push_back(trigger.revision.clone());
    visited.insert(trigger.revision.clone());
    while let Some(current) = queue.pop_front() {
        let Some(outgoing) = dependents.get(&current) else {
            continue;
        };
        for edge in outgoing {
            if edge.from == trigger.revision {
                continue;
            }
            match &edge.source {
                EdgeSource::Explicit { .. } => {
                    certain
                        .entry(edge.from.clone())
                        .or_default()
                        .push(edge.describe());
                    if visited.insert(edge.from.clone()) {
                        queue.push_back(edge.from.clone());
                    }
                }
                EdgeSource::Inferred {
                    basis,
                    confidence_bp,
                } => {
                    let entry = possible
                        .entry(edge.from.clone())
                        .or_insert_with(|| (Vec::new(), 0));
                    entry
                        .0
                        .push(format!("{}（依据：{}）", edge.kind.as_str(), basis));
                    entry.1 = entry.1.max(*confidence_bp);
                    // 推测边不继续扩散：否则一条猜测会引发整条链上的"可能受影响"，
                    // 看起来很像真的有影响。
                }
            }
        }
    }

    // 确定集合里如果同时被推测边命中，以确定为结论，不再重复列进 possible。
    for region in certain.keys() {
        possible.remove(region);
    }

    let certain_hits: Vec<RegionHit> = certain
        .into_iter()
        .map(|(region, via)| RegionHit {
            region,
            via,
            note: "经显式依赖边可达".to_string(),
        })
        .collect();
    let possible_hits: Vec<RegionHit> = possible
        .into_iter()
        .map(|(region, (via, confidence_bp))| RegionHit {
            region,
            via,
            note: format!(
                "仅经推测边可达，最高置信 {:.2}；不据此判定失效",
                f64::from(confidence_bp) / 10_000.0
            ),
        })
        .collect();

    let reached: BTreeSet<RevisionId> = certain_hits
        .iter()
        .chain(possible_hits.iter())
        .map(|hit| hit.region.clone())
        .collect();
    let unknown: Vec<RegionHit> = input
        .known_regions
        .iter()
        .filter(|region| **region != trigger.revision)
        .filter(|region| !reached.contains(*region))
        .map(|region| RegionHit {
            region: region.clone(),
            via: Vec::new(),
            note: if input
                .edges
                .iter()
                .any(|edge| &edge.from == region || &edge.to == region)
            {
                "有关系记录，但没有指向变化点的路径；影响未知".to_string()
            } else {
                "缺少关系：没有任何依赖边连到它，不能据此认为不受影响".to_string()
            },
        })
        .collect();

    let id_material = format!(
        "{}|{}|{}|{}|{}",
        trigger.change_id,
        trigger.revision.short(),
        input.old_baseline,
        input.new_baseline,
        input.rule_version
    );

    ImpactAssessment {
        assessment_id: format!("impact-{}", &sha256_hex(id_material.as_bytes())[..16]),
        trigger: trigger.clone(),
        old_baseline: input.old_baseline.to_string(),
        new_baseline: input.new_baseline.to_string(),
        relation_snapshot: input.edges.to_vec(),
        rule_version: input.rule_version.to_string(),
        analyzed_at_ms: now_ms,
        certain: certain_hits,
        possible: possible_hits,
        unknown,
    }
}

// ---------------------------------------------------------------------------
// 重验提案
// ---------------------------------------------------------------------------

/// 单个提案的目标上限。超过就拆成多个提案，而不是无限膨胀一个。
pub const MAX_TARGETS_PER_PROPOSAL: usize = 8;
/// 单次触发允许的提案上限。超出的进 `deferred` 显式排队。
pub const MAX_PROPOSALS_PER_TRIGGER: usize = 4;

/// 提案状态。只有 `Approved` 才能建新工作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalState {
    Proposed,
    Approved,
    Rejected,
}

impl ProposalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

/// 一条重验提案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevalidationProposal {
    /// 幂等键：变化 + 评估规则 + 目标集合。短时间内重复的事件会落到同一个键上。
    pub proposal_id: String,
    pub assessment_id: String,
    pub change_id: String,
    pub targets: Vec<RevisionId>,
    /// 该变化类别是否强制人工判断。
    pub requires_human: bool,
    pub state: ProposalState,
    pub reason: String,
}

/// 提案批次。`deferred` 是超出预算、被显式排队的目标，不是被丢弃。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalBatch {
    pub proposals: Vec<RevalidationProposal>,
    pub deferred: Vec<RevisionId>,
    pub force_human: bool,
}

/// 幂等键。
pub fn proposal_key(assessment: &ImpactAssessment, targets: &[RevisionId]) -> String {
    let mut list: Vec<String> = targets.iter().map(RevisionId::short).collect();
    list.sort();
    let material = format!(
        "{}|{}|{}",
        assessment.trigger.change_id,
        assessment.rule_version,
        list.join(",")
    );
    format!("reval-{}", &sha256_hex(material.as_bytes())[..16])
}

/// 从评估生成有限的重验提案。
///
/// 只对 [`ImpactAssessment::candidates`] 里的区域提案；未知区域不提案（先补关系）。
/// 提案本身**不修改**任何既有接受记录，也不删除成果——它只创建"待授权的新工作"。
pub fn propose(assessment: &ImpactAssessment) -> ProposalBatch {
    let force_human = assessment.trigger.kind.forces_human_review();
    let candidates = assessment.candidates();
    let mut proposals = Vec::new();
    let mut deferred = Vec::new();

    for (index, chunk) in candidates.chunks(MAX_TARGETS_PER_PROPOSAL).enumerate() {
        if index >= MAX_PROPOSALS_PER_TRIGGER {
            deferred.extend(chunk.iter().cloned());
            continue;
        }
        proposals.push(RevalidationProposal {
            proposal_id: proposal_key(assessment, chunk),
            assessment_id: assessment.assessment_id.clone(),
            change_id: assessment.trigger.change_id.clone(),
            targets: chunk.to_vec(),
            requires_human: force_human,
            state: ProposalState::Proposed,
            reason: if force_human {
                format!(
                    "{} 类变化必须人工判断：不能因成本理由跳过门禁",
                    assessment.trigger.kind.as_str()
                )
            } else {
                format!(
                    "{} 类变化：按确定关系计算候选范围",
                    assessment.trigger.kind.as_str()
                )
            },
        });
    }

    ProposalBatch {
        proposals,
        deferred,
        force_human,
    }
}

/// 去重账本：同一幂等键只保留一条，重复事件合并进来。
#[derive(Debug, Default)]
pub struct RevalidationLedger {
    by_key: BTreeMap<String, RevalidationProposal>,
    merged_events: BTreeMap<String, usize>,
}

/// 提交结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    New {
        proposal_id: String,
    },
    Merged {
        proposal_id: String,
        events: usize,
    },
    /// 已有提案被拒绝：同键再提交不复活它，要人显式重开。
    SuppressedByRejection {
        proposal_id: String,
    },
}

impl RevalidationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn submit(&mut self, proposal: RevalidationProposal) -> SubmitOutcome {
        match self.by_key.get_mut(&proposal.proposal_id) {
            None => {
                let id = proposal.proposal_id.clone();
                self.by_key.insert(id.clone(), proposal);
                self.merged_events.insert(id.clone(), 1);
                SubmitOutcome::New { proposal_id: id }
            }
            Some(existing) => {
                let id = existing.proposal_id.clone();
                if existing.state == ProposalState::Rejected {
                    return SubmitOutcome::SuppressedByRejection { proposal_id: id };
                }
                // 合并：目标并集，仍只保留一条提案。
                for target in &proposal.targets {
                    if !existing.targets.contains(target) {
                        existing.targets.push(target.clone());
                    }
                }
                if proposal.requires_human {
                    existing.requires_human = true;
                }
                let events = self.merged_events.entry(id.clone()).or_insert(0);
                *events += 1;
                SubmitOutcome::Merged {
                    proposal_id: id,
                    events: *events,
                }
            }
        }
    }

    pub fn get(&self, proposal_id: &str) -> Option<&RevalidationProposal> {
        self.by_key.get(proposal_id)
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// 审批。人工判断类必须带确认人；否则拒绝审批。
    pub fn approve(&mut self, proposal_id: &str, approved_by: &str) -> Result<(), ImpactError> {
        let Some(proposal) = self.by_key.get_mut(proposal_id) else {
            return Err(ImpactError::ProposalNotFound {
                proposal_id: proposal_id.to_string(),
            });
        };
        if proposal.requires_human && approved_by.trim().is_empty() {
            return Err(ImpactError::PromotionWithoutEvidence);
        }
        if proposal.state == ProposalState::Rejected {
            return Err(ImpactError::AssessmentSuperseded {
                assessment_id: proposal.assessment_id.clone(),
                by: "已拒绝的提案".to_string(),
            });
        }
        proposal.state = ProposalState::Approved;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 维护提案（观测缺失）
// ---------------------------------------------------------------------------

/// 一次观测结果。`reachable = false` 时其余字段都不可信。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub region: RevisionId,
    pub reachable: bool,
    pub observed_version: Option<String>,
    pub observed_at_ms: Option<i64>,
}

/// 维护提案的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceState {
    /// 观测到版本与期望一致：无需动作。这是唯一可以写"健康"的状态。
    Healthy,
    /// 版本漂移：生成只读检查提案。
    Stale { expected: String, observed: String },
    /// 观测不可达：标 unknown，**不是**健康。
    Unknown { reason: String },
}

/// 维护提案：默认只读，不自动升级、不发布。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceProposal {
    pub proposal_id: String,
    pub region: RevisionId,
    pub state: MaintenanceState,
    pub read_only: bool,
    pub requires_owner: bool,
    pub created_at_ms: i64,
}

/// 根据观测与期望版本生成维护提案。
///
/// `expected` 是"部署清单声明的版本"：它来自确定输入，不是从观测里反推的，
/// 否则观测断了就永远看不出漂移。
pub fn maintenance_proposals(
    observations: &[Observation],
    expected: &BTreeMap<RevisionId, String>,
    now_ms: i64,
) -> Vec<MaintenanceProposal> {
    let mut out = Vec::new();
    for observation in observations {
        let state = if !observation.reachable {
            MaintenanceState::Unknown {
                reason: "观测不可达：不能把没有告警当作健康".to_string(),
            }
        } else {
            match (
                expected.get(&observation.region),
                observation.observed_version.as_ref(),
            ) {
                (Some(want), Some(got)) if want == got => MaintenanceState::Healthy,
                (Some(want), Some(got)) => MaintenanceState::Stale {
                    expected: want.clone(),
                    observed: got.clone(),
                },
                (Some(want), None) => MaintenanceState::Stale {
                    expected: want.clone(),
                    observed: "未报告".to_string(),
                },
                (None, _) => MaintenanceState::Unknown {
                    reason: "部署清单没有该区域的期望版本：先补声明".to_string(),
                },
            }
        };
        if matches!(state, MaintenanceState::Healthy) {
            continue;
        }
        let material = format!(
            "{}|{}|{:?}",
            observation.region.short(),
            observation.region.kind.as_str(),
            state
        );
        out.push(MaintenanceProposal {
            proposal_id: format!("maint-{}", &sha256_hex(material.as_bytes())[..16]),
            region: observation.region.clone(),
            state,
            // 维护提案默认只读：检查、核对、报告，不改环境、不发版。
            read_only: true,
            requires_owner: true,
            created_at_ms: now_ms,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> RevisionId {
        RevisionId::new(ArtifactKind::Api, "aaaa1111")
    }
    fn web() -> RevisionId {
        RevisionId::new(ArtifactKind::Source, "bbbb2222")
    }
    fn sdk() -> RevisionId {
        RevisionId::new(ArtifactKind::Source, "cccc3333")
    }
    fn perm_scenario() -> RevisionId {
        RevisionId::new(ArtifactKind::Spec, "dddd4444")
    }
    fn monitor() -> RevisionId {
        RevisionId::new(ArtifactKind::Monitoring, "eeee5555")
    }

    fn change(kind: ChangeKind) -> ChangeTrigger {
        ChangeTrigger {
            change_id: "chg-1".to_string(),
            revision: api(),
            kind,
            description: "配额接口字段变更".to_string(),
        }
    }

    fn full_edges() -> Vec<DependencyEdge> {
        vec![
            DependencyEdge::explicit(
                web(),
                api(),
                EdgeKind::Implements,
                "web/src/quota.ts",
                "web",
            ),
            DependencyEdge::explicit(sdk(), api(), EdgeKind::BuiltFrom, "sdk/gen.sh", "sdk"),
            DependencyEdge::explicit(
                perm_scenario(),
                api(),
                EdgeKind::Verifies,
                "specs/quota/spec.md",
                "spec",
            ),
        ]
    }

    fn input<'a>(edges: &'a [DependencyEdge], regions: &'a [RevisionId]) -> AssessmentInput<'a> {
        AssessmentInput {
            old_baseline: "base-old",
            new_baseline: "base-new",
            rule_version: "impact-rules/v1",
            known_regions: regions,
            edges,
        }
    }

    #[test]
    fn inferred_edge_stays_inferred_and_does_not_mark_downstream_failed() {
        let observed = RevisionId::new(ArtifactKind::Source, "ffff6666");
        let edges = vec![DependencyEdge::inferred(
            observed.clone(),
            api(),
            EdgeKind::Requires,
            "命名相似 quotaService",
            0.42,
            "obs",
        )];
        let regions = vec![api(), observed.clone()];
        let assessment = assess(&change(ChangeKind::Interface), &input(&edges, &regions), 10);

        assert!(assessment.certain.is_empty(), "推测边不得进入确定集合");
        assert_eq!(assessment.possible.len(), 1);
        assert_eq!(assessment.status(&observed), RegionStatus::Possible);
        let hit = &assessment.possible[0];
        assert!(hit.note.contains("不据此判定失效"), "实际 {}", hit.note);
        assert!(hit.via[0].contains("命名相似"));

        // 没有显式确认就不能升级。
        let err = promote_inferred(
            &edges[0],
            &EdgePromotion {
                edge_from: observed.clone(),
                edge_to: api(),
                approved_by: "  ".to_string(),
                evidence: String::new(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, ImpactError::PromotionWithoutEvidence));

        let upgraded = promote_inferred(
            &edges[0],
            &EdgePromotion {
                edge_from: observed.clone(),
                edge_to: api(),
                approved_by: "owner-a".to_string(),
                evidence: "确认两者共用同一配额实现".to_string(),
            },
        )
        .unwrap();
        assert!(upgraded.source.is_explicit());
        let after = assess(
            &change(ChangeKind::Interface),
            &input(std::slice::from_ref(&upgraded), &regions),
            11,
        );
        assert_eq!(after.status(&observed), RegionStatus::Certain);
    }

    #[test]
    fn region_without_relations_is_unknown_not_unaffected() {
        let edges = full_edges();
        let regions = vec![api(), web(), sdk(), perm_scenario(), monitor()];
        let assessment = assess(&change(ChangeKind::Interface), &input(&edges, &regions), 12);

        assert_eq!(assessment.status(&web()), RegionStatus::Certain);
        assert_eq!(assessment.status(&sdk()), RegionStatus::Certain);
        assert_eq!(assessment.status(&perm_scenario()), RegionStatus::Certain);
        // 监控没被任何边连到 → unknown，不能进"不受影响"
        assert_eq!(assessment.status(&monitor()), RegionStatus::Unknown);
        let hit = assessment
            .unknown
            .iter()
            .find(|item| item.region == monitor())
            .expect("监控应出现在未知列表");
        assert!(hit.note.contains("缺少关系"), "实际 {}", hit.note);
        assert!(
            !assessment.candidates().contains(&monitor()),
            "未知区域不进候选"
        );
    }

    #[test]
    fn region_with_edges_but_no_path_is_also_unknown() {
        let other = RevisionId::new(ArtifactKind::Source, "9999aaaa");
        let unrelated = RevisionId::new(ArtifactKind::Test, "8888bbbb");
        let edges = vec![DependencyEdge::explicit(
            other.clone(),
            unrelated.clone(),
            EdgeKind::Requires,
            "x",
            "g",
        )];
        let regions = vec![api(), other.clone(), unrelated.clone()];
        let assessment = assess(&change(ChangeKind::Source), &input(&edges, &regions), 13);
        let hit = assessment
            .unknown
            .iter()
            .find(|item| item.region == unrelated)
            .expect("应有未知项");
        assert!(
            hit.note.contains("没有指向变化点的路径"),
            "有关系但不可达应单独说明，实际 {}",
            hit.note
        );
    }

    #[test]
    fn assessment_freezes_baselines_relations_rule_and_time() {
        let edges = full_edges();
        let regions = vec![api(), web(), sdk()];
        let assessment = assess(&change(ChangeKind::Interface), &input(&edges, &regions), 42);

        assert_eq!(assessment.old_baseline, "base-old");
        assert_eq!(assessment.new_baseline, "base-new");
        assert_eq!(assessment.rule_version, "impact-rules/v1");
        assert_eq!(assessment.analyzed_at_ms, 42);
        assert_eq!(assessment.relation_snapshot.len(), 3);
        assert!(assessment.assessment_id.starts_with("impact-"));
        assert!(!assessment.is_stale_against(&edges));

        // 新增一条边后，旧评估被判过期（但快照本身不变）。
        let mut newer = edges.clone();
        newer.push(DependencyEdge::explicit(
            RevisionId::new(ArtifactKind::Test, "7777cccc"),
            api(),
            EdgeKind::Verifies,
            "tests/quota.rs",
            "test",
        ));
        assert!(assessment.is_stale_against(&newer));
        assert_eq!(assessment.relation_snapshot.len(), 3);
    }

    #[test]
    fn only_related_regions_enter_the_candidate_scope() {
        let edges = full_edges();
        let regions = vec![api(), web(), sdk(), perm_scenario(), monitor()];
        let assessment = assess(&change(ChangeKind::Interface), &input(&edges, &regions), 20);
        let batch = propose(&assessment);

        assert_eq!(batch.proposals.len(), 1);
        let targets = &batch.proposals[0].targets;
        // 变化点本身 + 三个受影响区域 = 4
        assert_eq!(targets.len(), 4, "实际 {targets:?}");
        assert!(targets.contains(&api()));
        assert!(targets.contains(&web()));
        assert!(targets.contains(&sdk()));
        assert!(targets.contains(&perm_scenario()));
        assert!(
            !targets.contains(&monitor()),
            "无关监控报表不得进入重验候选"
        );
        assert_eq!(batch.proposals[0].state, ProposalState::Proposed);
        assert!(!batch.proposals[0].requires_human);
    }

    #[test]
    fn proposal_does_not_touch_old_acceptance_or_delete_anything() {
        let edges = full_edges();
        let regions = vec![api(), web()];
        let assessment = assess(&change(ChangeKind::Interface), &input(&edges, &regions), 21);
        let batch = propose(&assessment);
        // 提案只带目标与理由，没有任何"删除""改写接受"的入口。
        let proposal = &batch.proposals[0];
        assert!(proposal.state == ProposalState::Proposed);
        assert!(proposal.reason.contains("候选范围"));
        assert!(
            !proposal.reason.contains("删除") && !proposal.reason.contains("覆盖"),
            "重验提案不得声称会删除成果或改写旧接受"
        );
    }

    #[test]
    fn duplicate_change_events_merge_into_one_proposal() {
        let edges = full_edges();
        let regions = vec![api(), web(), sdk()];
        let trigger = change(ChangeKind::Interface);
        let mut ledger = RevalidationLedger::new();

        let first = propose(&assess(&trigger, &input(&edges, &regions), 30));
        let id = match ledger.submit(first.proposals[0].clone()) {
            SubmitOutcome::New { proposal_id } => proposal_id,
            other => panic!("首次应为新建，实际 {other:?}"),
        };

        // 同一接口修订的第二次变化事件：目标集合相同 → 合并。
        let second = propose(&assess(&trigger, &input(&edges, &regions), 31));
        match ledger.submit(second.proposals[0].clone()) {
            SubmitOutcome::Merged {
                proposal_id,
                events,
            } => {
                assert_eq!(proposal_id, id);
                assert_eq!(events, 2);
            }
            other => panic!("重复事件应合并，实际 {other:?}"),
        }
        assert_eq!(ledger.len(), 1, "短时重复变化只生成一个提案");
    }

    #[test]
    fn rejected_proposal_is_not_revived_by_a_repeat_event() {
        let edges = full_edges();
        let regions = vec![api(), web()];
        let batch = propose(&assess(
            &change(ChangeKind::Interface),
            &input(&edges, &regions),
            32,
        ));
        let mut ledger = RevalidationLedger::new();
        let id = match ledger.submit(batch.proposals[0].clone()) {
            SubmitOutcome::New { proposal_id } => proposal_id,
            other => panic!("{other:?}"),
        };
        ledger.by_key.get_mut(&id).unwrap().state = ProposalState::Rejected;
        match ledger.submit(batch.proposals[0].clone()) {
            SubmitOutcome::SuppressedByRejection { .. } => {}
            other => panic!("被拒绝的提案不能被同键复活，实际 {other:?}"),
        }
    }

    #[test]
    fn budget_and_scope_bound_the_number_of_proposals() {
        // 造 20 个区域，全部显式依赖：应拆成 4 个提案（每包 8 个）并把其余的排队。
        let mut edges = Vec::new();
        let mut regions = vec![api()];
        for index in 0..20 {
            let region = RevisionId::new(ArtifactKind::Source, &format!("{index:08}"));
            edges.push(DependencyEdge::explicit(
                region.clone(),
                api(),
                EdgeKind::Requires,
                "manifest",
                "g",
            ));
            regions.push(region);
        }
        let assessment = assess(&change(ChangeKind::Interface), &input(&edges, &regions), 33);
        let batch = propose(&assessment);
        // 21 个候选按每包 8 个切 → 8/8/5 共 3 个提案，仍在预算内。
        assert_eq!(batch.proposals.len(), 3);
        assert_eq!(batch.proposals[0].targets.len(), MAX_TARGETS_PER_PROPOSAL);
        assert!(batch.deferred.is_empty(), "21 个目标在预算内，不该排队");

        // 把上限当成"总数上限"时：40 个候选会超出。
        let mut edges2 = Vec::new();
        let mut regions2 = vec![api()];
        for index in 0..40 {
            let region = RevisionId::new(ArtifactKind::Source, &format!("11{index:06}"));
            edges2.push(DependencyEdge::explicit(
                region.clone(),
                api(),
                EdgeKind::Requires,
                "manifest",
                "g",
            ));
            regions2.push(region);
        }
        let assessment2 = assess(
            &change(ChangeKind::Interface),
            &input(&edges2, &regions2),
            34,
        );
        let batch2 = propose(&assessment2);
        assert_eq!(batch2.proposals.len(), MAX_PROPOSALS_PER_TRIGGER);
        assert_eq!(
            batch2.deferred.len(),
            41 - MAX_PROPOSALS_PER_TRIGGER * MAX_TARGETS_PER_PROPOSAL,
            "超出预算的目标必须显式排队，不能悄悄丢掉"
        );
    }

    #[test]
    fn safety_and_migration_changes_force_human_review() {
        for kind in [
            ChangeKind::Safety,
            ChangeKind::PublicMigration,
            ChangeKind::Toolchain,
            ChangeKind::Environment,
            ChangeKind::CrossComponentContract,
        ] {
            assert!(
                kind.forces_human_review(),
                "{} 应强制人工判断",
                kind.as_str()
            );
            let edges = full_edges();
            let regions = vec![api(), web()];
            let batch = propose(&assess(&change(kind), &input(&edges, &regions), 40));
            assert!(batch.force_human);
            assert!(batch.proposals[0].requires_human);
            assert!(
                batch.proposals[0].reason.contains("不能因成本理由"),
                "理由要写明门禁不可因成本跳过"
            );
        }

        // 强制人工的提案，无确认人不得审批。
        let edges = full_edges();
        let regions = vec![api(), web()];
        let batch = propose(&assess(
            &change(ChangeKind::PublicMigration),
            &input(&edges, &regions),
            41,
        ));
        let mut ledger = RevalidationLedger::new();
        let id = match ledger.submit(batch.proposals[0].clone()) {
            SubmitOutcome::New { proposal_id } => proposal_id,
            other => panic!("{other:?}"),
        };
        assert!(ledger.approve(&id, "  ").is_err(), "无确认人不得审批");
        ledger.approve(&id, "owner-b").unwrap();
        assert_eq!(ledger.get(&id).unwrap().state, ProposalState::Approved);
    }

    #[test]
    fn maintenance_proposal_is_read_only_by_default() {
        let env = RevisionId::new(ArtifactKind::Environment, "12121212");
        let mut expected = BTreeMap::new();
        expected.insert(env.clone(), "1.4.0".to_string());
        let observations = vec![Observation {
            region: env.clone(),
            reachable: true,
            observed_version: Some("1.5.0".to_string()),
            observed_at_ms: Some(99),
        }];
        let proposals = maintenance_proposals(&observations, &expected, 100);
        assert_eq!(proposals.len(), 1);
        assert!(proposals[0].read_only, "维护提案默认只读检查");
        assert!(proposals[0].requires_owner);
        match &proposals[0].state {
            MaintenanceState::Stale { expected, observed } => {
                assert_eq!(expected, "1.4.0");
                assert_eq!(observed, "1.5.0");
            }
            other => panic!("应为版本漂移，实际 {other:?}"),
        }
    }

    #[test]
    fn unreachable_observation_is_unknown_not_healthy() {
        let env = RevisionId::new(ArtifactKind::Environment, "34343434");
        let mut expected = BTreeMap::new();
        expected.insert(env.clone(), "1.4.0".to_string());
        let observations = vec![Observation {
            region: env.clone(),
            reachable: false,
            observed_version: None,
            observed_at_ms: None,
        }];
        let proposals = maintenance_proposals(&observations, &expected, 101);
        assert_eq!(proposals.len(), 1, "不可达必须产出提案，不能静默当健康");
        match &proposals[0].state {
            MaintenanceState::Unknown { reason } => {
                assert!(reason.contains("没有告警当作健康"), "实际 {reason}");
            }
            other => panic!("不可达应为 unknown，实际 {other:?}"),
        }
    }

    #[test]
    fn healthy_observation_produces_no_proposal() {
        let env = RevisionId::new(ArtifactKind::Environment, "56565656");
        let mut expected = BTreeMap::new();
        expected.insert(env.clone(), "1.4.0".to_string());
        let observations = vec![Observation {
            region: env,
            reachable: true,
            observed_version: Some("1.4.0".to_string()),
            observed_at_ms: Some(102),
        }];
        assert!(maintenance_proposals(&observations, &expected, 103).is_empty());
    }

    #[test]
    fn missing_expected_version_is_unknown_not_healthy() {
        let env = RevisionId::new(ArtifactKind::Environment, "78787878");
        let expected = BTreeMap::new();
        let observations = vec![Observation {
            region: env,
            reachable: true,
            observed_version: Some("9.9.9".to_string()),
            observed_at_ms: Some(104),
        }];
        let proposals = maintenance_proposals(&observations, &expected, 105);
        assert!(matches!(
            proposals[0].state,
            MaintenanceState::Unknown { .. }
        ));
    }
}
