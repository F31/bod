//! P2 切片 2.2：旧执行停止确认与资源隔离检查（`invariant_i_48`）。
//!
//! 一条硬边界：**停止未确认或隔离无法证实，就不得写旧现场**。
//! 因此本模块只做判定，不提供「大概没问题」的放行口子：
//!
//! 1. 停止状态只有携带独立证据时才算确认；`cancel_ack` 永远不是停止确认（沿用 P1 4.3）；
//! 2. 旧写入面未知时，任何写都按「可能重叠」处理，而不是按「大概不重叠」处理；
//! 3. 「隔离」必须由机制证据支撑（环境已销毁 / 写权限已撤销 / 所有权已移交），
//!    不接受「它是独立工作区」这类自述；
//! 4. 相关外部副作用在停止未确认时一律拒绝重发，与工作区是否独立无关。
//!
//! 本模块不写库：判定输入全部来自已持久化的事实（取消事件、租约、执行状态），
//! 结论作为 P2 接管意图 `isolation_checked` / `admitted` 阶段的准入凭据。

use crate::cancel_reconciliation as cancel;
use crate::cancel_reconciliation::{CancelEvent, CancelPhase};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const ISOLATION_SCHEMA: &str = "bod.isolation-check/v1";

/// 阻断原因码。只报事实，不产出「可以继续」的许可。
pub const COND_STOP_UNCONFIRMED: &str = "stop_unconfirmed";
pub const COND_SURFACE_OVERLAP: &str = "surface_overlap";
pub const COND_SURFACE_UNKNOWN: &str = "surface_unknown";
pub const COND_ISOLATION_UNPROVEN: &str = "isolation_unproven";
pub const COND_EFFECT_UNRECONCILED: &str = "external_effect_unreconciled";
pub const COND_EFFECT_UNKNOWN: &str = "effect_relation_unknown";

#[derive(Debug)]
pub enum IsolationError {
    UnsupportedSchema(String),
    EmptyField(&'static str),
    NonCanonicalTimestamp(String),
    /// 隔离依据没有引用：它是自述，不是证据。
    UnevidencedBasis(&'static str),
    /// 同一类依据出现两次：无法判断哪一个才是当下事实。
    DuplicateBasis(&'static str),
    MalformedTarget(String),
}

impl fmt::Display for IsolationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(schema) => write!(f, "不支持的隔离检查版本：{schema}"),
            Self::EmptyField(field) => write!(f, "{field} 不能为空"),
            Self::NonCanonicalTimestamp(value) => write!(f, "时间戳不是规范形式：{value}"),
            Self::UnevidencedBasis(kind) => write!(f, "隔离依据 {kind} 缺少引用，不能作为证据"),
            Self::DuplicateBasis(kind) => write!(f, "隔离依据 {kind} 重复出现"),
            Self::MalformedTarget(detail) => write!(f, "写目标无法解析：{detail}"),
        }
    }
}

impl std::error::Error for IsolationError {}

/// 旧执行的停止状态。只有 `Confirmed` 携带独立证据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopStatus {
    /// 调用级证据 + 外部结果引用：执行已到终态。
    Confirmed {
        invocation_id: String,
        evidence_ref: String,
    },
    /// 只证明「取消请求被收到」，执行可能仍在写。
    AckOnly { ack_ref: String },
    /// 连回执都没有。
    NoAck,
    /// 查过但结果不明：必须能归属到处理人与下一步。
    Unknown { assignee: String, next_step: String },
}

impl StopStatus {
    /// 唯一可以据此宣称「停止」的判定。除 `Confirmed` 外一律 false。
    pub fn confirmed(&self) -> bool {
        matches!(self, Self::Confirmed { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Confirmed { .. } => "confirmed",
            Self::AckOnly { .. } => "ack_only",
            Self::NoAck => "no_ack",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// 停止确认的证据引用；未确认时必须是 `None`，否则界面会拿它当结论。
    pub fn evidence_ref(&self) -> Option<&str> {
        match self {
            Self::Confirmed { evidence_ref, .. } => Some(evidence_ref),
            _ => None,
        }
    }

    /// 未确认时对账待办：`(处理人, 下一步)`。
    pub fn pending_decision(&self) -> Option<(&str, &str)> {
        match self {
            Self::Unknown {
                assignee,
                next_step,
            } => Some((assignee, next_step)),
            _ => None,
        }
    }

    /// 从 P1 4.3 的取消事件派生：`ack` 与 `unknown` 都落在「未确认」。
    pub fn from_cancel_event(event: &CancelEvent) -> Self {
        match event.phase {
            CancelPhase::StopConfirmed => Self::Confirmed {
                invocation_id: event.invocation_id.clone().unwrap_or_default(),
                // `stop_evidence` 已含调用、外部结果引用与终态，原样保留不重解析。
                evidence_ref: event.stop_evidence.clone().unwrap_or_default(),
            },
            CancelPhase::AckReceived => Self::AckOnly {
                ack_ref: event.ack_ref.clone().unwrap_or_default(),
            },
            CancelPhase::Requested => Self::NoAck,
            CancelPhase::Unknown => Self::Unknown {
                assignee: event.assignee.clone(),
                next_step: event.next_step.clone(),
            },
        }
    }

    fn validate(&self) -> Result<(), IsolationError> {
        match self {
            Self::Confirmed {
                invocation_id,
                evidence_ref,
            } => {
                if invocation_id.trim().is_empty() {
                    return Err(IsolationError::EmptyField("停止证据的调用 id"));
                }
                if evidence_ref.trim().is_empty() {
                    return Err(IsolationError::EmptyField("停止证据的外部结果引用"));
                }
            }
            Self::AckOnly { ack_ref } => {
                if ack_ref.trim().is_empty() {
                    return Err(IsolationError::EmptyField("取消回执引用"));
                }
            }
            Self::NoAck => {}
            Self::Unknown {
                assignee,
                next_step,
            } => {
                if assignee.trim().is_empty() {
                    return Err(IsolationError::EmptyField("待核实项处理人"));
                }
                if next_step.trim().is_empty() {
                    return Err(IsolationError::EmptyField("待核实项下一步"));
                }
            }
        }
        Ok(())
    }
}

/// 旧执行可能写的路径范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceKnowledge {
    /// 逐条枚举过。
    Enumerated { paths: Vec<String> },
    /// 只知道工作区根：视为覆盖根下全部路径。
    RootOnly { root: String },
    /// 完全不知道：任何写都不能被证实无冲突。
    Unknown,
}

/// 旧执行可能发出的外部副作用动作键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectSurface {
    Enumerated { keys: Vec<String> },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRelation {
    Overlaps,
    Disjoint,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectRelation {
    Related,
    Unrelated,
    Indeterminate,
}

/// 路径按组件比较：`src/a` 与 `src/ab` 不重叠，`src` 与 `src/a` 重叠。
///
/// 统一分隔符、丢弃空组件与 `.`、按词法消解 `..`；比较时统一转小写——
/// 在大小写敏感文件系统上这是**偏保守**的方向（宁可多拦，不会漏拦）。
fn normalize_path(raw: &str) -> Vec<String> {
    let unified = raw.trim().replace('\\', "/");
    let mut parts: Vec<String> = Vec::new();
    for component in unified.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_ascii_lowercase()),
        }
    }
    parts
}

fn paths_overlap(left: &[String], right: &[String]) -> bool {
    let common = left.len().min(right.len());
    left[..common] == right[..common]
}

impl SurfaceKnowledge {
    pub fn relation_to(&self, path: &str) -> SurfaceRelation {
        let target = normalize_path(path);
        if target.is_empty() {
            return SurfaceRelation::Indeterminate;
        }
        match self {
            Self::Enumerated { paths } => {
                // 空枚举等于「没有需要避让的路径」，那会放行一切；构造期已拒绝，
                // 这里再兜一次，避免手工构造的结构绕过。
                if paths.is_empty() {
                    return SurfaceRelation::Indeterminate;
                }
                if paths
                    .iter()
                    .any(|entry| paths_overlap(&normalize_path(entry), &target))
                {
                    SurfaceRelation::Overlaps
                } else {
                    SurfaceRelation::Disjoint
                }
            }
            Self::RootOnly { root } => {
                if paths_overlap(&normalize_path(root), &target) {
                    SurfaceRelation::Overlaps
                } else {
                    SurfaceRelation::Disjoint
                }
            }
            Self::Unknown => SurfaceRelation::Indeterminate,
        }
    }

    fn validate(&self) -> Result<(), IsolationError> {
        match self {
            Self::Enumerated { paths } => {
                if paths.is_empty() {
                    return Err(IsolationError::EmptyField("旧写入面路径枚举"));
                }
                if paths.iter().any(|entry| normalize_path(entry).is_empty()) {
                    return Err(IsolationError::EmptyField("旧写入面路径"));
                }
            }
            Self::RootOnly { root } => {
                if normalize_path(root).is_empty() {
                    return Err(IsolationError::EmptyField("旧写入面根路径"));
                }
            }
            Self::Unknown => {}
        }
        Ok(())
    }
}

impl EffectSurface {
    pub fn relation_to(&self, key: &str) -> EffectRelation {
        let needle = key.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return EffectRelation::Indeterminate;
        }
        match self {
            Self::Enumerated { keys } => {
                if keys.is_empty() {
                    return EffectRelation::Indeterminate;
                }
                if keys
                    .iter()
                    .any(|entry| entry.trim().to_ascii_lowercase() == needle)
                {
                    EffectRelation::Related
                } else {
                    EffectRelation::Unrelated
                }
            }
            Self::Unknown => EffectRelation::Indeterminate,
        }
    }

    fn validate(&self) -> Result<(), IsolationError> {
        if let Self::Enumerated { keys } = self {
            if keys.is_empty() {
                return Err(IsolationError::EmptyField("外部副作用枚举"));
            }
            if keys.iter().any(|entry| entry.trim().is_empty()) {
                return Err(IsolationError::EmptyField("外部副作用动作键"));
            }
        }
        Ok(())
    }
}

/// 隔离的机制证据。三种都要求引用：没有引用的「已隔离」只是自述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IsolationBasis {
    /// 旧执行所在环境已销毁（容器删除 / 进程树终止），引用为销毁记录。
    EnvironmentDestroyed { reference: String },
    /// 共享目录的写权限已撤销，引用为权限变更记录。
    WriteAccessRevoked { reference: String },
    /// 独占锁/租约已移交给新执行者，引用为移交记录。
    OwnershipTransferred { lock_ref: String },
}

impl IsolationBasis {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EnvironmentDestroyed { .. } => "environment_destroyed",
            Self::WriteAccessRevoked { .. } => "write_access_revoked",
            Self::OwnershipTransferred { .. } => "ownership_transferred",
        }
    }

    fn reference(&self) -> &str {
        match self {
            Self::EnvironmentDestroyed { reference } | Self::WriteAccessRevoked { reference } => {
                reference
            }
            Self::OwnershipTransferred { lock_ref } => lock_ref,
        }
    }

    fn validate(&self) -> Result<(), IsolationError> {
        if self.reference().trim().is_empty() {
            return Err(IsolationError::UnevidencedBasis(self.kind()));
        }
        Ok(())
    }
}

/// 写目标：现场 + 路径 + 可选的外部动作键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTarget {
    pub workspace_id: String,
    pub path: String,
    pub effect_key: Option<String>,
}

impl WriteTarget {
    pub fn new(workspace_id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            path: path.into(),
            effect_key: None,
        }
    }

    pub fn with_effect(mut self, effect_key: impl Into<String>) -> Self {
        self.effect_key = Some(effect_key.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationCondition {
    pub code: &'static str,
    pub subject: String,
    pub detail: String,
    pub required_evidence: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteDecision {
    Allowed { grounds: Vec<&'static str> },
    Blocked { conditions: Vec<IsolationCondition> },
}

impl WriteDecision {
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    pub fn conditions(&self) -> &[IsolationCondition] {
        match self {
            Self::Blocked { conditions } => conditions,
            Self::Allowed { .. } => &[],
        }
    }

    pub fn codes(&self) -> Vec<&'static str> {
        self.conditions().iter().map(|item| item.code).collect()
    }
}

/// 准入凭据：`Cleared` 才能把接管意图推进到 `admitted`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationClearance {
    Cleared {
        basis: &'static str,
        grounds: Vec<&'static str>,
    },
    NotCleared {
        conditions: Vec<IsolationCondition>,
    },
}

impl IsolationClearance {
    pub fn cleared(&self) -> bool {
        matches!(self, Self::Cleared { .. })
    }

    pub fn conditions(&self) -> &[IsolationCondition] {
        match self {
            Self::NotCleared { conditions } => conditions,
            Self::Cleared { .. } => &[],
        }
    }

    /// 写进阶段历史的一行摘要（`isolation_checked` / `admitted` 都要留痕）。
    pub fn describe(&self) -> String {
        match self {
            Self::Cleared { basis, .. } => format!("隔离已证实（依据 {basis}）"),
            Self::NotCleared { conditions } => format!(
                "隔离未通过：{}",
                conditions
                    .iter()
                    .map(|item| item.code)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
}

/// 界面投影：只读事实，不产生许可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationSummary {
    pub attempt_id: String,
    pub stop_label: &'static str,
    pub stop_confirmed: bool,
    pub stop_evidence: Option<String>,
    pub pending_decision: Option<(String, String)>,
    pub conditions: Vec<IsolationCondition>,
    pub clearable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationCheck {
    pub schema: String,
    /// 被接管的旧执行。
    pub attempt_id: String,
    pub stop: StopStatus,
    pub surface: SurfaceKnowledge,
    pub effects: EffectSurface,
    pub bases: Vec<IsolationBasis>,
    pub observed_at: String,
}

fn condition(
    code: &'static str,
    subject: impl Into<String>,
    detail: impl Into<String>,
    required_evidence: &'static str,
) -> IsolationCondition {
    IsolationCondition {
        code,
        subject: subject.into(),
        detail: detail.into(),
        required_evidence,
    }
}

impl IsolationCheck {
    pub fn new(attempt_id: impl Into<String>, stop: StopStatus, surface: SurfaceKnowledge) -> Self {
        Self {
            schema: ISOLATION_SCHEMA.to_string(),
            attempt_id: attempt_id.into(),
            stop,
            surface,
            effects: EffectSurface::Unknown,
            bases: Vec::new(),
            observed_at: String::new(),
        }
    }

    pub fn with_effects(mut self, effects: EffectSurface) -> Self {
        self.effects = effects;
        self
    }

    pub fn with_basis(mut self, basis: IsolationBasis) -> Self {
        self.bases.push(basis);
        self
    }

    pub fn observed_at(mut self, at: impl Into<String>) -> Self {
        self.observed_at = at.into();
        self
    }

    pub fn validate(&self) -> Result<(), IsolationError> {
        if self.schema != ISOLATION_SCHEMA {
            return Err(IsolationError::UnsupportedSchema(self.schema.clone()));
        }
        if self.attempt_id.trim().is_empty() {
            return Err(IsolationError::EmptyField("被接管的旧执行"));
        }
        if !crate::canonical_json::is_canonical_timestamp(&self.observed_at) {
            return Err(IsolationError::NonCanonicalTimestamp(
                self.observed_at.clone(),
            ));
        }
        self.stop.validate()?;
        self.surface.validate()?;
        self.effects.validate()?;
        for (index, basis) in self.bases.iter().enumerate() {
            basis.validate()?;
            if self.bases[..index]
                .iter()
                .any(|other| other.kind() == basis.kind())
            {
                return Err(IsolationError::DuplicateBasis(basis.kind()));
            }
        }
        Ok(())
    }

    /// 对「新执行者要写某个路径」给出判定。
    pub fn decide_write(&self, target: &WriteTarget) -> Result<WriteDecision, IsolationError> {
        if normalize_path(&target.path).is_empty() {
            return Err(IsolationError::MalformedTarget(target.path.clone()));
        }
        if target.workspace_id.trim().is_empty() {
            return Err(IsolationError::EmptyField("写目标的工作区"));
        }
        self.validate()?;

        let relation = self.surface.relation_to(&target.path);
        let subject = target.path.clone();
        let mut conditions = Vec::new();
        let confirmed = self.stop.confirmed();
        let isolated = !self.bases.is_empty();

        // 停止状态只在「要写旧现场」或「无法证明不相交」时才是阻断条件。
        // 已证实不相交的独立工作不受它约束——否则「允许独立无冲突工作」就不成立。
        if !confirmed && relation != SurfaceRelation::Disjoint {
            conditions.push(condition(
                COND_STOP_UNCONFIRMED,
                subject.clone(),
                format!(
                    "旧执行 {} 的停止状态为 {}，未能证实已停止",
                    self.attempt_id,
                    self.stop.label()
                ),
                "带外部结果引用的停止确认（invocation + result_ref + 终态）",
            ));
        }

        match relation {
            // 写同一个现场：既要停止确认，也要隔离机制证据。
            SurfaceRelation::Overlaps => {
                if !isolated {
                    conditions.push(condition(
                        COND_ISOLATION_UNPROVEN,
                        subject.clone(),
                        "目标落在旧写入面上，但没有隔离机制证据",
                        "环境销毁 / 写权限撤销 / 所有权移交记录之一",
                    ));
                }
            }
            // 写入面未知：任何隔离证据都补不上「不知道范围」这件事。
            SurfaceRelation::Indeterminate => {
                conditions.push(condition(
                    COND_SURFACE_UNKNOWN,
                    subject.clone(),
                    "旧写入面未知，无法证实该路径无冲突",
                    "旧写入面枚举或工作区根",
                ));
            }
            // 独立路径仍要求隔离机制证据：只声明「这是独立工作区」不接受。
            SurfaceRelation::Disjoint => {
                if !isolated {
                    conditions.push(condition(
                        COND_ISOLATION_UNPROVEN,
                        subject.clone(),
                        "该路径不在旧写入面上，但没有隔离机制证据",
                        "环境销毁 / 写权限撤销 / 所有权移交记录之一",
                    ));
                }
            }
        }

        if let Some(effect_key) = target.effect_key.as_deref() {
            match self.effects.relation_to(effect_key) {
                EffectRelation::Related if !confirmed => {
                    conditions.push(condition(
                        COND_EFFECT_UNRECONCILED,
                        effect_key.to_string(),
                        "该动作属于旧执行的外部副作用，停止未确认时不得重发",
                        "停止确认，或对账确认该动作未生效/已生效",
                    ));
                }
                EffectRelation::Indeterminate => {
                    conditions.push(condition(
                        COND_EFFECT_UNKNOWN,
                        effect_key.to_string(),
                        "外部副作用范围未知，无法证实该动作不相关",
                        "旧执行外部动作键枚举",
                    ));
                }
                EffectRelation::Related | EffectRelation::Unrelated => {}
            }
        }

        if !conditions.is_empty() {
            // 重叠本身是事实：被拦时要说清「你要写的正是旧现场」。
            if relation == SurfaceRelation::Overlaps {
                conditions.push(condition(
                    COND_SURFACE_OVERLAP,
                    subject,
                    "目标与旧执行可能写的路径重叠",
                    "旧写入面已收缩且不再覆盖该路径",
                ));
            }
            return Ok(WriteDecision::Blocked { conditions });
        }

        let mut grounds = Vec::new();
        match relation {
            SurfaceRelation::Disjoint => grounds.push("surface_disjoint"),
            SurfaceRelation::Overlaps => grounds.push("surface_overlap_acknowledged"),
            SurfaceRelation::Indeterminate => {}
        }
        if confirmed && relation != SurfaceRelation::Disjoint {
            grounds.push("stop_confirmed");
        }
        if isolated {
            grounds.push("isolation_evidenced");
        }
        if let Some(effect_key) = target.effect_key.as_deref()
            && matches!(
                self.effects.relation_to(effect_key),
                EffectRelation::Unrelated
            )
        {
            grounds.push("effect_unrelated");
        }
        Ok(WriteDecision::Allowed { grounds })
    }

    /// 相关外部副作用是否允许重发：停止未确认时一律不允许。
    pub fn decide_effect(&self, effect_key: &str) -> Result<WriteDecision, IsolationError> {
        if effect_key.trim().is_empty() {
            return Err(IsolationError::EmptyField("外部动作键"));
        }
        self.validate()?;
        let relation = self.effects.relation_to(effect_key);
        let mut conditions = Vec::new();
        match relation {
            EffectRelation::Related if !self.stop.confirmed() => {
                conditions.push(condition(
                    COND_EFFECT_UNRECONCILED,
                    effect_key.to_string(),
                    "该动作属于旧执行的外部副作用，停止未确认时不得重发",
                    "停止确认，或对账确认该动作未生效/已生效",
                ));
            }
            EffectRelation::Indeterminate => {
                conditions.push(condition(
                    COND_EFFECT_UNKNOWN,
                    effect_key.to_string(),
                    "外部副作用范围未知，无法证实该动作不相关",
                    "旧执行外部动作键枚举",
                ));
            }
            EffectRelation::Related | EffectRelation::Unrelated => {}
        }
        if !conditions.is_empty() {
            return Ok(WriteDecision::Blocked { conditions });
        }
        let mut grounds = Vec::new();
        if self.stop.confirmed() {
            grounds.push("stop_confirmed");
        }
        if matches!(relation, EffectRelation::Unrelated) {
            grounds.push("effect_unrelated");
        }
        Ok(WriteDecision::Allowed { grounds })
    }

    /// 准入凭据：停止确认与隔离证据**都**要有，缺一即不可推进到 `admitted`。
    pub fn clearance(&self) -> Result<IsolationClearance, IsolationError> {
        self.validate()?;
        let mut conditions = Vec::new();
        if !self.stop.confirmed() {
            conditions.push(condition(
                COND_STOP_UNCONFIRMED,
                self.attempt_id.clone(),
                format!("停止状态为 {}，未能证实旧执行已停止", self.stop.label()),
                "带外部结果引用的停止确认",
            ));
        }
        if self.bases.is_empty() {
            conditions.push(condition(
                COND_ISOLATION_UNPROVEN,
                self.attempt_id.clone(),
                "没有任何隔离机制证据",
                "环境销毁 / 写权限撤销 / 所有权移交记录之一",
            ));
        }
        if conditions.is_empty() {
            let basis = self
                .bases
                .first()
                .map(IsolationBasis::kind)
                .unwrap_or("unknown");
            Ok(IsolationClearance::Cleared {
                basis,
                grounds: vec!["stop_confirmed", "isolation_evidenced"],
            })
        } else {
            Ok(IsolationClearance::NotCleared { conditions })
        }
    }

    /// 界面投影。`stop_confirmed` 只由证据决定，`ack` 不会把它变成 true。
    pub fn summary(&self) -> Result<IsolationSummary, IsolationError> {
        let clearance = self.clearance()?;
        Ok(IsolationSummary {
            attempt_id: self.attempt_id.clone(),
            stop_label: self.stop.label(),
            stop_confirmed: self.stop.confirmed(),
            stop_evidence: self.stop.evidence_ref().map(str::to_string),
            pending_decision: self
                .stop
                .pending_decision()
                .map(|(assignee, next)| (assignee.to_string(), next.to_string())),
            conditions: clearance.conditions().to_vec(),
            clearable: clearance.cleared(),
        })
    }
}

/// 从 P1 4.3 的取消事件直接构造检查：避免调用方自己拼 `Confirmed`。
pub fn check_from_cancel(
    event: &CancelEvent,
    surface: SurfaceKnowledge,
    observed_at: &str,
) -> IsolationCheck {
    IsolationCheck::new(
        event.attempt_id.clone(),
        StopStatus::from_cancel_event(event),
        surface,
    )
    .observed_at(observed_at)
}

/// 停止确认与「取消」的判定保持同一个来源：`ack` 之后仍为 false（I-11 / I-48）。
pub fn stop_confirmed(event: &CancelEvent) -> bool {
    cancel::stop_is_confirmed(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::execution::ExecutionState;
    use crate::store::SqliteStore;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW_MS: i64 = 1_764_200_000_000;
    const AT: &str = "2026-11-20T12:00:00Z";

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-isolation-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
    }

    fn ack_only() -> StopStatus {
        StopStatus::AckOnly {
            ack_ref: "ack-1".to_string(),
        }
    }

    fn confirmed() -> StopStatus {
        StopStatus::Confirmed {
            invocation_id: "inv-1".to_string(),
            evidence_ref: "inv-1:result-1:cancelled".to_string(),
        }
    }

    fn enumerated(paths: &[&str]) -> SurfaceKnowledge {
        SurfaceKnowledge::Enumerated {
            paths: paths.iter().map(|item| item.to_string()).collect(),
        }
    }

    fn destroyed() -> IsolationBasis {
        IsolationBasis::EnvironmentDestroyed {
            reference: "destroy-2026-11-20".to_string(),
        }
    }

    fn check(stop: StopStatus, surface: SurfaceKnowledge) -> IsolationCheck {
        IsolationCheck::new("attempt-old", stop, surface).observed_at(AT)
    }

    #[test]
    fn invariant_i_48_unconfirmed_writer_is_blocked_on_the_same_surface() {
        let check = check(ack_only(), enumerated(&["/ws/app/src"])).with_basis(destroyed());
        let decision = check
            .decide_write(&WriteTarget::new("ws-b", "/ws/app/src/main.rs"))
            .unwrap();

        assert!(!decision.allowed(), "旧写入未停时不得写同一现场");
        let codes = decision.codes();
        assert!(codes.contains(&COND_STOP_UNCONFIRMED), "{codes:?}");
        assert!(codes.contains(&COND_SURFACE_OVERLAP), "{codes:?}");
        // 条件必须写清「需要什么证据」，而不是只报一个错误码。
        let overlap = decision
            .conditions()
            .iter()
            .find(|item| item.code == COND_SURFACE_OVERLAP)
            .unwrap();
        assert!(overlap.detail.contains("重叠"));
        assert!(!overlap.required_evidence.is_empty());
    }

    #[test]
    fn same_surface_is_allowed_only_after_stop_and_isolation() {
        // 同一现场：缺停止确认 → 拦；补上确认后仍缺隔离证据 → 还拦；
        // 两者齐备才放行。
        let surface = enumerated(&["/ws/app/src"]);
        let target = WriteTarget::new("ws-b", "/ws/app/src/main.rs");

        let blocked = check(ack_only(), surface.clone())
            .with_basis(destroyed())
            .decide_write(&target)
            .unwrap();
        assert!(!blocked.allowed());

        let still_blocked = check(confirmed(), surface.clone())
            .decide_write(&target)
            .unwrap();
        assert!(!still_blocked.allowed());
        let codes = still_blocked.codes();
        assert!(codes.contains(&COND_ISOLATION_UNPROVEN), "{codes:?}");
        assert!(codes.contains(&COND_SURFACE_OVERLAP), "{codes:?}");

        let allowed = check(confirmed(), surface)
            .with_basis(destroyed())
            .decide_write(&target)
            .unwrap();
        assert!(allowed.allowed());
        match allowed {
            WriteDecision::Allowed { grounds } => {
                assert!(grounds.contains(&"stop_confirmed"), "{grounds:?}");
                assert!(grounds.contains(&"isolation_evidenced"), "{grounds:?}");
            }
            other => panic!("应允许，实际 {other:?}"),
        }
    }

    #[test]
    fn invariant_i_48_independent_workspace_allowed_when_isolation_proven() {
        let check = check(ack_only(), enumerated(&["/ws/app/src"])).with_basis(destroyed());
        let decision = check
            .decide_write(&WriteTarget::new("ws-b", "/ws/other/notes.md"))
            .unwrap();

        assert!(decision.allowed(), "已证实隔离的独立路径应允许无冲突工作");
        match decision {
            WriteDecision::Allowed { grounds } => {
                assert!(grounds.contains(&"surface_disjoint"), "{grounds:?}");
                assert!(grounds.contains(&"isolation_evidenced"), "{grounds:?}");
                assert!(
                    !grounds.contains(&"stop_confirmed"),
                    "未确认停止就不能记这条"
                );
            }
            other => panic!("应允许，实际 {other:?}"),
        }
    }

    #[test]
    fn invariant_i_48_independent_workspace_blocked_without_isolation_proof() {
        let check = check(ack_only(), enumerated(&["/ws/app/src"]));
        let decision = check
            .decide_write(&WriteTarget::new("ws-b", "/ws/other/notes.md"))
            .unwrap();

        assert!(!decision.allowed(), "没有隔离证据就不算「独立」");
        assert_eq!(decision.codes(), vec![COND_ISOLATION_UNPROVEN]);
    }

    #[test]
    fn invariant_i_48_ack_is_not_a_stop_confirmation() {
        let root = temp_root("ack");
        let mut store = open_store(&root);
        let request = cancel::CancelRequest {
            cancel_id: "cancel-1",
            attempt_id: "attempt-old",
            company_id: "company-1",
            invocation_id: Some("inv-1"),
            risk_reservation_refs: vec!["reservation-1".to_string()],
            known_evidence: "已请求停止",
            assignee: "manager-1",
            next_step: "查询外部执行状态",
        };
        cancel::request_cancel(&mut store, &request, NOW_MS).unwrap();
        let acked =
            cancel::record_cancel_ack(&mut store, "company-1", "attempt-old", "ack-1", NOW_MS)
                .unwrap();

        let check = check_from_cancel(&acked, enumerated(&["/ws/app/src"]), AT);
        let summary = check.summary().unwrap();
        assert!(!summary.stop_confirmed, "回执不等于停止");
        assert_eq!(summary.stop_label, "ack_only");
        assert!(summary.stop_evidence.is_none(), "回执不能充当停止证据");
        assert!(!summary.clearable);

        // 同一现场仍然被拦。
        let decision = check
            .decide_write(&WriteTarget::new("ws-b", "/ws/app/src/main.rs"))
            .unwrap();
        assert!(!decision.allowed());
    }

    #[test]
    fn invariant_i_48_stop_confirmed_comes_from_cancel_evidence() {
        let root = temp_root("confirmed");
        let mut store = open_store(&root);
        let request = cancel::CancelRequest {
            cancel_id: "cancel-1",
            attempt_id: "attempt-old",
            company_id: "company-1",
            invocation_id: Some("inv-1"),
            risk_reservation_refs: Vec::new(),
            known_evidence: "已请求停止",
            assignee: "manager-1",
            next_step: "查询外部执行状态",
        };
        cancel::request_cancel(&mut store, &request, NOW_MS).unwrap();
        cancel::record_cancel_ack(&mut store, "company-1", "attempt-old", "ack-1", NOW_MS).unwrap();
        let stopped = cancel::confirm_stop(
            &mut store,
            "company-1",
            "attempt-old",
            "inv-1",
            "result-1",
            ExecutionState::Cancelled,
            NOW_MS,
        )
        .unwrap();
        assert!(stop_confirmed(&stopped), "取消流程自己也要给出同一结论");

        let check =
            check_from_cancel(&stopped, enumerated(&["/ws/app/src"]), AT).with_basis(destroyed());
        let summary = check.summary().unwrap();
        assert!(summary.stop_confirmed);
        assert_eq!(summary.stop_label, "confirmed");
        let evidence = summary.stop_evidence.unwrap();
        assert!(evidence.contains("result-1"), "{evidence}");
        assert!(summary.clearable);
    }

    #[test]
    fn stop_confirmed_alone_is_not_clearance() {
        // 停止确认只说明那一次调用结束了，占着现场的可能还有别的写入者。
        let check = check(confirmed(), enumerated(&["/ws/app/src"]));
        let clearance = check.clearance().unwrap();
        assert!(!clearance.cleared());
        assert_eq!(
            clearance
                .conditions()
                .iter()
                .map(|item| item.code)
                .collect::<Vec<_>>(),
            vec![COND_ISOLATION_UNPROVEN]
        );
    }

    #[test]
    fn unknown_surface_blocks_every_write() {
        let check = check(confirmed(), SurfaceKnowledge::Unknown).with_basis(destroyed());
        let decision = check
            .decide_write(&WriteTarget::new("ws-b", "/anywhere/at/all.txt"))
            .unwrap();
        assert!(!decision.allowed(), "写入面未知时不能假定无冲突");
        assert_eq!(decision.codes(), vec![COND_SURFACE_UNKNOWN]);
    }

    #[test]
    fn root_only_surface_covers_subtree_and_rejects_sibling() {
        let surface = SurfaceKnowledge::RootOnly {
            root: "/ws/app".to_string(),
        };
        assert_eq!(
            surface.relation_to("/ws/app/src/a.rs"),
            SurfaceRelation::Overlaps
        );
        assert_eq!(surface.relation_to("/ws/app"), SurfaceRelation::Overlaps);
        assert_eq!(
            surface.relation_to("/ws/app2/a.rs"),
            SurfaceRelation::Disjoint
        );
    }

    #[test]
    fn sibling_prefix_is_not_overlap() {
        let surface = enumerated(&["/ws/src"]);
        assert_eq!(
            surface.relation_to("/ws/src2/a.rs"),
            SurfaceRelation::Disjoint
        );
        let decision = check(ack_only(), surface)
            .with_basis(destroyed())
            .decide_write(&WriteTarget::new("ws-b", "/ws/src2/a.rs"))
            .unwrap();
        assert!(decision.allowed(), "按路径组件比较，不是字符串前缀");
    }

    #[test]
    fn dot_segments_and_separators_are_normalized() {
        let surface = enumerated(&["C://ws//app//src"]);
        // 反斜杠、`.`、`..` 与大小写差异都指向同一路径。
        assert_eq!(
            surface.relation_to("c:/ws/app/./src/../src/main.rs"),
            SurfaceRelation::Overlaps
        );
        assert_eq!(
            surface.relation_to("C://WS//APP//SRC//"),
            SurfaceRelation::Overlaps
        );
    }

    #[test]
    fn related_effect_is_not_resent_while_unconfirmed() {
        let check = check(ack_only(), enumerated(&["/ws/app"]))
            .with_basis(destroyed())
            .with_effects(EffectSurface::Enumerated {
                keys: vec!["payment.capture".to_string()],
            });
        let decision = check.decide_effect("payment.capture").unwrap();
        assert!(!decision.allowed(), "停止未确认时不得重发相关外部操作");
        assert_eq!(decision.codes(), vec![COND_EFFECT_UNRECONCILED]);

        // 写目标上带同一个动作键时，即使路径独立也要被拦。
        let write = check
            .decide_write(
                &WriteTarget::new("ws-b", "/ws/other/x.txt").with_effect("payment.capture"),
            )
            .unwrap();
        assert!(!write.allowed());
        assert!(write.codes().contains(&COND_EFFECT_UNRECONCILED));
    }

    #[test]
    fn unrelated_effect_is_allowed_on_enumerated_surface() {
        let check = check(ack_only(), enumerated(&["/ws/app"])).with_basis(destroyed());
        let check = check.with_effects(EffectSurface::Enumerated {
            keys: vec!["payment.capture".to_string()],
        });
        let decision = check.decide_effect("ticket.comment").unwrap();
        assert!(decision.allowed());
        assert_eq!(
            decision,
            WriteDecision::Allowed {
                grounds: vec!["effect_unrelated"]
            }
        );
    }

    #[test]
    fn effect_relation_unknown_blocks() {
        let check = check(ack_only(), enumerated(&["/ws/app"])).with_basis(destroyed());
        let decision = check.decide_effect("payment.capture").unwrap();
        assert!(!decision.allowed());
        assert_eq!(decision.codes(), vec![COND_EFFECT_UNKNOWN]);
    }

    #[test]
    fn unevidenced_basis_is_rejected() {
        let check = check(ack_only(), enumerated(&["/ws/app"])).with_basis(
            IsolationBasis::WriteAccessRevoked {
                reference: "   ".to_string(),
            },
        );
        assert!(matches!(
            check.validate(),
            Err(IsolationError::UnevidencedBasis("write_access_revoked"))
        ));
    }

    #[test]
    fn duplicate_basis_is_rejected() {
        let check = check(ack_only(), enumerated(&["/ws/app"]))
            .with_basis(destroyed())
            .with_basis(IsolationBasis::EnvironmentDestroyed {
                reference: "destroy-later".to_string(),
            });
        assert!(matches!(
            check.validate(),
            Err(IsolationError::DuplicateBasis("environment_destroyed"))
        ));
    }

    #[test]
    fn unknown_stop_requires_assignee_and_next_step() {
        let check = check(
            StopStatus::Unknown {
                assignee: " ".to_string(),
                next_step: "查询".to_string(),
            },
            enumerated(&["/ws/app"]),
        );
        assert!(matches!(
            check.validate(),
            Err(IsolationError::EmptyField("待核实项处理人"))
        ));
    }

    #[test]
    fn empty_surface_enumeration_is_rejected() {
        // 空枚举如果被当成「没有需要避让的路径」，就会放行一切。
        let check = check(ack_only(), enumerated(&[]));
        assert!(matches!(
            check.validate(),
            Err(IsolationError::EmptyField("旧写入面路径枚举"))
        ));
        assert_eq!(
            enumerated(&[]).relation_to("/ws/app/src"),
            SurfaceRelation::Indeterminate
        );
    }

    #[test]
    fn malformed_target_is_rejected() {
        let check = check(confirmed(), enumerated(&["/ws/app"])).with_basis(destroyed());
        assert!(matches!(
            check.decide_write(&WriteTarget::new("ws-b", "   ")),
            Err(IsolationError::MalformedTarget(_))
        ));
        assert!(matches!(
            check.decide_effect("  "),
            Err(IsolationError::EmptyField("外部动作键"))
        ));
    }

    #[test]
    fn non_canonical_observation_time_is_rejected() {
        let mut check = check(ack_only(), enumerated(&["/ws/app"]));
        check.observed_at = "2026-11-20 12:00".to_string();
        assert!(matches!(
            check.validate(),
            Err(IsolationError::NonCanonicalTimestamp(_))
        ));
    }

    #[test]
    fn summary_lists_conditions_without_claiming_stop() {
        let check = check(
            StopStatus::Unknown {
                assignee: "manager-1".to_string(),
                next_step: "查询外部执行状态".to_string(),
            },
            SurfaceKnowledge::Unknown,
        );
        let summary = check.summary().unwrap();
        assert!(!summary.stop_confirmed);
        assert_eq!(summary.stop_label, "unknown");
        assert_eq!(
            summary.pending_decision,
            Some(("manager-1".to_string(), "查询外部执行状态".to_string()))
        );
        assert!(!summary.clearable);
        let codes: Vec<&str> = summary.conditions.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec![COND_STOP_UNCONFIRMED, COND_ISOLATION_UNPROVEN]);
    }
}
