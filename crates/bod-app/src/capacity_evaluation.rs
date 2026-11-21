//! P7：容量评估的测量口径、瓶颈定位与毕业决定（`performance-baseline` / `capacity-governance`）。
//!
//! 本模块只定义**口径与治理**：什么算测到、什么算未测、什么不能拿来做决定。真正的实测在
//! [`crate::perf_probe`]（`xtask bench`），它产出的报告必须符合本模块的 `MeasureItem` 口径。
//! 本模块不改变业务语义、不迁移引擎、不冻结尚未两阶段验证的预算。
//!
//! 五条硬规则：
//! 1. **状态三档**：`Target`（只有设计目标）/ `Measured`（有样本、命令、环境）/ `Unverified`
//!    （机制有但当前环境测不到）。缺样本或缺命令的报告不进入容量决策。
//! 2. **形态分报**：headless / Web / Tauri 桌面 / 移动分别报告，headless 的空闲 RSS 不得
//!    冒充桌面总内存。
//! 3. **四类毕业信号**：RTO、单写争用、体积压力、多机/容灾需求分别判定；日均事件数不构成
//!    迁移理由。
//! 4. **先比较再决定**：索引/批处理/存储优化、执行节点扩展、PostgreSQL、外部持久执行服务
//!    四项都要有迁移代价、运维负担、故障语义与恢复证据；DAO 替换不等于语义兼容。
//! 5. **决定可审计**：`GraduationDecision` 只追加不覆盖，历史依据不因人员变化改写；
//!    本 Change 内禁止引擎变更，性能结论也不得扩大支持矩阵。

use std::collections::BTreeMap;
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// 1.4 状态与报告结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MeasureStatus {
    /// 只有设计目标，尚无任何实测。
    Target,
    /// 有样本、命令与环境，数值可用。
    Measured,
    /// 机制存在但当前环境测不到，缺口如实记录。
    Unverified,
}

impl MeasureStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Target => "Target",
            Self::Measured => "Measured",
            Self::Unverified => "Unverified",
        }
    }
}

/// 运行形态。不同形态的数字不能互相顶替。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FormFactor {
    Headless,
    Web,
    Desktop,
    Mobile,
}

impl FormFactor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
        }
    }

    pub const ALL: [Self; 4] = [Self::Headless, Self::Web, Self::Desktop, Self::Mobile];
}

/// 一条测量项：**没有样本数或命令就不算测到**。
#[derive(Debug, Clone, PartialEq)]
pub struct MeasureItem {
    pub name: &'static str,
    pub form_factor: FormFactor,
    /// 设计目标（§38.2）；未设定的写 `无冻结目标`。
    pub target: String,
    pub value: Option<String>,
    pub samples: usize,
    /// 复现所需的完整命令。
    pub command: String,
    /// 测量方法与数据规模。
    pub method: String,
    /// 结论或口径限制，不得夸大。
    pub note: String,
    /// 能否与目标比较：`None` 表示目标不是可比较数值（或未测）。
    pub meets_target: Option<bool>,
    /// 用于跨阶段比较的标量（p99 毫秒、events/s、MiB 等）；无标量则无法参与两阶段对比。
    pub numeric: Option<f64>,
    pub failure_reason: Option<String>,
    pub status: MeasureStatus,
}

impl MeasureItem {
    pub fn new(name: &'static str, form_factor: FormFactor, target: &str) -> Self {
        Self {
            name,
            form_factor,
            target: target.to_string(),
            value: None,
            samples: 0,
            command: String::new(),
            method: String::new(),
            note: String::new(),
            meets_target: None,
            numeric: None,
            failure_reason: None,
            status: MeasureStatus::Target,
        }
    }

    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = method.into();
        self
    }

    /// 标注用于跨阶段比较的标量。没有标量的项只能各自成条，不参与漂移判定。
    pub fn with_numeric(mut self, value: f64) -> Self {
        self.numeric = Some(value);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    /// 记一次真实实测。`meets_target` 由实测方按目标判定，`None` 表示目标无可比值。
    pub fn measured(
        mut self,
        value: impl Into<String>,
        samples: usize,
        command: &str,
        meets_target: Option<bool>,
    ) -> Self {
        self.value = Some(value.into());
        self.samples = samples;
        self.command = command.to_string();
        self.meets_target = meets_target;
        self.status = MeasureStatus::Measured;
        self
    }

    /// 当前环境测不到：如实写缺口，不留空、不填设计值。
    pub fn unverified(mut self, reason: impl Into<String>, command: &str) -> Self {
        self.failure_reason = Some(reason.into());
        self.command = command.to_string();
        self.status = MeasureStatus::Unverified;
        self
    }

    /// 能进入容量决策的前提：测到 + 有样本 + 有命令 + 不是缺数据造成的零。
    pub fn usable_for_decision(&self) -> bool {
        self.status == MeasureStatus::Measured
            && self.samples > 0
            && !self.command.trim().is_empty()
            && self.value.is_some()
            && self.failure_reason.is_none()
    }

    /// 达标宣称的前提：测到了，且确实与目标比较过并达标。
    pub fn may_claim_target(&self) -> bool {
        self.usable_for_decision() && self.meets_target == Some(true)
    }

    pub fn display(&self) -> &str {
        self.value.as_deref().unwrap_or("未测")
    }
}

/// 测量条件。复现的第一要件是把它写全。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentRecord {
    /// CLI 传入的环境行（`os / arch / rustc`），原样保留便于人工对照。
    pub note: String,
    pub os: String,
    pub cpu: String,
    pub rustc: String,
    pub build_params: String,
    pub sqlite_sync: String,
    pub data_scale: String,
    pub cache_state: String,
}

impl EnvironmentRecord {
    /// 与另一次运行比较，列出差异项。为空表示条件一致。
    pub fn diff(&self, other: &Self) -> Vec<&'static str> {
        let mut diffs = Vec::new();
        if self.os != other.os {
            diffs.push("os");
        }
        if self.cpu != other.cpu {
            diffs.push("cpu");
        }
        if self.rustc != other.rustc {
            diffs.push("rustc");
        }
        if self.build_params != other.build_params {
            diffs.push("build_params");
        }
        if self.sqlite_sync != other.sqlite_sync {
            diffs.push("sqlite_sync");
        }
        if self.data_scale != other.data_scale {
            diffs.push("data_scale");
        }
        if self.cache_state != other.cache_state {
            diffs.push("cache_state");
        }
        diffs
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BaselineReport {
    pub environment: EnvironmentRecord,
    pub commit: String,
    pub items: Vec<MeasureItem>,
}

impl BaselineReport {
    pub fn measured_count(&self) -> usize {
        self.count(MeasureStatus::Measured)
    }

    pub fn unverified_count(&self) -> usize {
        self.count(MeasureStatus::Unverified)
    }

    pub fn target_count(&self) -> usize {
        self.count(MeasureStatus::Target)
    }

    fn count(&self, status: MeasureStatus) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == status)
            .count()
    }

    pub fn by_form_factor(&self, form_factor: FormFactor) -> Vec<&MeasureItem> {
        self.items
            .iter()
            .filter(|item| item.form_factor == form_factor)
            .collect()
    }

    /// 报告能否作为容量决策输入：Target 项（完全没测）与非规整的 Unverified 项都不能有。
    pub fn usable_for_decision(&self) -> bool {
        !self.items.is_empty()
            && self.items.iter().all(|item| match item.status {
                MeasureStatus::Measured => item.usable_for_decision(),
                MeasureStatus::Unverified => {
                    !item.command.trim().is_empty() && item.failure_reason.is_some()
                }
                MeasureStatus::Target => false,
            })
    }

    /// 吞吐与尾延迟分开：任一已测项未达目标，就不得宣称"预算达标"。
    pub fn may_claim_all_targets(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(MeasureItem::may_claim_target)
    }

    /// 挡着"全部达标"宣称的项：未测的、或测了但没达标的。无目标的项不算在内。
    pub fn unresolved(&self) -> Vec<&'static str> {
        self.items
            .iter()
            .filter(|item| match item.status {
                MeasureStatus::Measured => item.meets_target == Some(false),
                MeasureStatus::Unverified | MeasureStatus::Target => true,
            })
            .map(|item| item.name)
            .collect()
    }

    /// 只在"该形态每一项都测到且达标"时，才可宣称该形态达标。
    pub fn may_claim_form_factor(&self, form_factor: FormFactor) -> bool {
        let items = self.by_form_factor(form_factor);
        !items.is_empty() && items.iter().all(|item| item.may_claim_target())
    }

    pub fn item(&self, name: &str) -> Option<&MeasureItem> {
        self.items.iter().find(|item| item.name == name)
    }
}

/// 用 headless 的数字冒充桌面结论一律无效。
pub fn guard_form_factor(reported: FormFactor, claimed: FormFactor) -> Result<(), &'static str> {
    if reported == claimed {
        Ok(())
    } else {
        Err("形态不同的数字不能互相顶替：需分别测壳、WebView、sidecar 与 Agent")
    }
}

// ---------------------------------------------------------------------------
// 2.5 四类毕业信号
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraduationSignal {
    /// 恢复 RTO 不达标。
    RecoveryRto,
    /// 单写争用（Busy / 写等待）。
    WriteContention,
    /// 表/索引体积与重建成本压力。
    VolumePressure,
    /// 实际多机写入或容灾需求。
    MultiHostDemand,
}

impl GraduationSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecoveryRto => "recovery_rto",
            Self::WriteContention => "write_contention",
            Self::VolumePressure => "volume_pressure",
            Self::MultiHostDemand => "multi_host_demand",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalReading {
    pub signal: GraduationSignal,
    pub observed: String,
    pub threshold: String,
    /// `true`/`false` = 有数据；`None` = 缺观测，必须记 unknown。
    pub met: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Graduation {
    /// 四类信号都没有越界（或缺观测）：不迁移，只记录观察值与复测条件。
    NoMigration {
        unknown: Vec<GraduationSignal>,
        reasons: Vec<String>,
    },
    /// 至少一类信号有数据且越界：可以进入方案对比。
    Candidate {
        signals: Vec<GraduationSignal>,
        blocked: Vec<GraduationSignal>,
    },
    /// 只有日均事件数之类的规模指标、没有任何信号观测：不构成迁移理由。
    RejectedByEventCount { daily_events: u64 },
}

/// 判定毕业。**日均事件数不参与判定**：它是观察值，不是信号。
pub fn graduation(signals: &[SignalReading], daily_events: Option<u64>) -> Graduation {
    // 只有规模指标、没有任何信号观测：直接拒绝，不进入对比。
    if signals.is_empty() {
        return match daily_events {
            Some(events) => Graduation::RejectedByEventCount {
                daily_events: events,
            },
            None => Graduation::NoMigration {
                unknown: Vec::new(),
                reasons: vec!["没有任何信号观测：不触发迁移，缺口记 unknown".to_string()],
            },
        };
    }

    let mut triggered = Vec::new();
    let mut unknown = Vec::new();
    let mut blocked = Vec::new();
    for reading in signals {
        match reading.met {
            Some(true) => triggered.push(reading.signal),
            Some(false) => blocked.push(reading.signal),
            None => unknown.push(reading.signal),
        }
    }
    if triggered.is_empty() {
        let mut reasons = vec!["四类信号均未越界或缺观测：不触发引擎迁移".to_string()];
        if let Some(events) = daily_events {
            reasons.push(format!("日均事件数 {events} 仅作观察值，不构成迁移理由"));
        }
        if !unknown.is_empty() {
            reasons.push(format!(
                "缺观测项记 unknown：{}",
                unknown
                    .iter()
                    .map(|signal| signal.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            ));
        }
        return Graduation::NoMigration { unknown, reasons };
    }
    Graduation::Candidate {
        signals: triggered,
        blocked,
    }
}

// ---------------------------------------------------------------------------
// 3.1 / 3.2 候选方案对比
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EngineCandidate {
    /// 索引 / 批处理 / 存储优化（先做）。
    Optimization,
    /// 执行节点扩展（多机执行而不是多机写）。
    ExecutorScale,
    /// PostgreSQL。
    Postgres,
    /// 外部持久执行服务。
    ExternalExecutorService,
}

impl EngineCandidate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Optimization => "optimization",
            Self::ExecutorScale => "executor_scale",
            Self::Postgres => "postgres",
            Self::ExternalExecutorService => "external_executor_service",
        }
    }

    pub const ALL: [Self; 4] = [
        Self::Optimization,
        Self::ExecutorScale,
        Self::Postgres,
        Self::ExternalExecutorService,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateAssessment {
    pub candidate: EngineCandidate,
    /// 迁移代价。
    pub migration_cost: String,
    /// 运维负担。
    pub ops_burden: String,
    /// 故障语义（单写次序是否保留、Unknown 如何处理）。
    pub failure_semantics: String,
    /// 恢复证据引用。
    pub recovery_evidence: String,
    /// 是否能在本 Change 内落地（引擎变更必须另建 Change）。
    pub requires_new_change: bool,
}

impl CandidateAssessment {
    /// 四项记录都不为空才算是"有内容的对比"。
    pub fn is_complete(&self) -> bool {
        [
            &self.migration_cost,
            &self.ops_burden,
            &self.failure_semantics,
            &self.recovery_evidence,
        ]
        .iter()
        .all(|field| !field.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    Complete {
        candidates: Vec<EngineCandidate>,
    },
    /// 缺项或空记录：不允许形成毕业决定。
    Incomplete {
        missing: Vec<EngineCandidate>,
        empty_fields: Vec<EngineCandidate>,
    },
}

pub fn compare_candidates(assessments: &[CandidateAssessment]) -> Comparison {
    let present: Vec<EngineCandidate> = assessments.iter().map(|item| item.candidate).collect();
    let missing: Vec<EngineCandidate> = EngineCandidate::ALL
        .iter()
        .copied()
        .filter(|candidate| !present.contains(candidate))
        .collect();
    let empty_fields: Vec<EngineCandidate> = assessments
        .iter()
        .filter(|item| !item.is_complete())
        .map(|item| item.candidate)
        .collect();
    if missing.is_empty() && empty_fields.is_empty() {
        Comparison::Complete {
            candidates: present,
        }
    } else {
        Comparison::Incomplete {
            missing,
            empty_fields,
        }
    }
}

/// DAO 替换不等于语义兼容：这条永远返回 `Err`，用来挡住"换了驱动就算兼容"的说法。
pub fn dao_swap_is_not_semantic_compat() -> Result<(), &'static str> {
    Err("替换 DAO/驱动不构成语义兼容证明：仍需单写次序、真实并发与备份恢复演练")
}

// ---------------------------------------------------------------------------
// 3.3 决定记录（只追加）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraduationDecision {
    pub decision_id: String,
    pub revision: u32,
    pub bottleneck: String,
    pub report_ref: String,
    pub environment: String,
    pub comparison: Vec<EngineCandidate>,
    pub chosen: String,
    pub owner: String,
    pub rollback_path: String,
    pub uncovered: Vec<String>,
    pub retest_trigger: String,
    pub decided_at: String,
}

impl GraduationDecision {
    /// 决定可审计的最低要求：缺任何一项都不算完整记录。
    pub fn audit_gaps(&self) -> Vec<&'static str> {
        let mut gaps = Vec::new();
        if self.bottleneck.trim().is_empty() {
            gaps.push("bottleneck");
        }
        if self.report_ref.trim().is_empty() {
            gaps.push("report_ref");
        }
        if self.environment.trim().is_empty() {
            gaps.push("environment");
        }
        if self.comparison.len() != EngineCandidate::ALL.len() {
            gaps.push("comparison");
        }
        if self.chosen.trim().is_empty() {
            gaps.push("chosen");
        }
        if self.owner.trim().is_empty() {
            gaps.push("owner");
        }
        if self.rollback_path.trim().is_empty() {
            gaps.push("rollback_path");
        }
        if self.retest_trigger.trim().is_empty() {
            gaps.push("retest_trigger");
        }
        gaps
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionLog {
    history: BTreeMap<String, Vec<GraduationDecision>>,
}

impl DecisionLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 首次登记。同一 `decision_id` 已存在即拒绝——历史不能被覆盖。
    pub fn record(&mut self, decision: GraduationDecision) -> Result<u32, &'static str> {
        if self.history.contains_key(&decision.decision_id) {
            return Err("同一决定已登记：修订请走 revise，历史依据不得改写");
        }
        if !decision.audit_gaps().is_empty() {
            return Err("决定记录不完整：需补齐瓶颈、样本、对比、主体、回滚路径与复测触发");
        }
        self.history
            .insert(decision.decision_id.clone(), vec![decision.clone()]);
        Ok(decision.revision)
    }

    /// 后续修订追加，旧版本原样保留。
    pub fn revise(
        &mut self,
        decision_id: &str,
        mut next: GraduationDecision,
    ) -> Result<u32, &'static str> {
        let Some(versions) = self.history.get_mut(decision_id) else {
            return Err("决定不存在：请先 record");
        };
        let last = versions.last().ok_or("决定历史为空")?;
        if !next.audit_gaps().is_empty() {
            return Err("修订不完整：需补齐瓶颈、样本、对比、主体、回滚路径与复测触发");
        }
        next.decision_id = decision_id.to_string();
        next.revision = last.revision + 1;
        let revision = next.revision;
        versions.push(next);
        Ok(revision)
    }

    pub fn versions(&self, decision_id: &str) -> usize {
        self.history
            .get(decision_id)
            .map(|versions| versions.len())
            .unwrap_or(0)
    }

    pub fn latest(&self, decision_id: &str) -> Option<&GraduationDecision> {
        self.history
            .get(decision_id)
            .and_then(|versions| versions.last())
    }

    pub fn earliest(&self, decision_id: &str) -> Option<&GraduationDecision> {
        self.history
            .get(decision_id)
            .and_then(|versions| versions.first())
    }
}

// ---------------------------------------------------------------------------
// 3.4 / 3.5 / 4.1 门禁
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineChange {
    /// 换库/换执行引擎。
    SwitchEngine { target: String },
    /// 调低 SQLite 同步级别。
    LowerSyncLevel { level: String },
    /// 放宽事务边界。
    WidenTransaction,
    /// 跳过授权或接受门禁。
    SkipGate { gate: String },
}

/// 容量阶段内任何引擎/正确性变更一律阻断，要求独立 Change 与迁移门禁。
pub fn guard_change_in_capacity_stage(change: &EngineChange) -> Result<(), String> {
    match change {
        EngineChange::SwitchEngine { target } => Err(format!(
            "阻断：容量评估内不得切换到 {target}，需另建 Change 并附迁移 ID/checksum、单写次序、并发测试与备份恢复演练"
        )),
        EngineChange::LowerSyncLevel { level } => Err(format!(
            "阻断：为达标降 SQLite 同步（{level}）属降低正确性，需显式风险授权与完整回归"
        )),
        EngineChange::WidenTransaction => {
            Err("阻断：放宽事务边界不属容量优化，需原 I/G 门禁回归".to_string())
        }
        EngineChange::SkipGate { gate } => Err(format!("阻断：不得跳过 {gate} 门禁换取性能数字")),
    }
}

/// 性能结论不得扩大支持矩阵。
pub fn guard_support_promotion(
    capability: &str,
    report: &BaselineReport,
    has_capability_evidence: bool,
) -> Result<(), String> {
    if has_capability_evidence {
        return Ok(());
    }
    Err(format!(
        "拒绝把 {capability} 标为 Supported：headless 吞吐达标（{} 项已测）不等于该能力有 conformance 与真机/双机报告",
        report.measured_count()
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetStage {
    /// 只有一轮实测：仍是 Target。
    SinglePhase,
    /// 两轮可比较阶段且差异在记录范围内。
    TwoComparablePhases { diff: Vec<&'static str> },
    /// 两轮不可比（环境或规模变了）。
    Incomparable { diff: Vec<&'static str> },
}

/// 预算冻结只能发生在两轮可比较阶段之后。
pub fn freeze_budget(stage: &BudgetStage) -> Result<&'static str, String> {
    match stage {
        BudgetStage::TwoComparablePhases { diff } if diff.is_empty() => Ok("budget-frozen"),
        BudgetStage::TwoComparablePhases { diff } => {
            Err(format!("两轮仍有差异项（{}），不得冻结", diff.join("、")))
        }
        BudgetStage::Incomparable { diff } => {
            Err(format!("两轮不可比（{}），冻结无效", diff.join("、")))
        }
        BudgetStage::SinglePhase => Err("只有一轮实测：预算保持 Target/Unverified".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 4.1 两阶段对比
// ---------------------------------------------------------------------------

/// 两阶段漂移容差：超过即视为不稳定，不得从 Target 转 Measured。
pub const PHASE_TOLERANCE_PCT: f64 = 25.0;

/// 两阶段之间单项的漂移。`within_tolerance` 为 `false` 时该项不得从 Target 转 Measured。
#[derive(Debug, Clone, PartialEq)]
pub struct ItemDrift {
    pub name: &'static str,
    pub first: f64,
    pub second: f64,
    /// `(second - first) / first`，`first` 为 0 时记 `f64::INFINITY`。
    pub delta_pct: f64,
    pub within_tolerance: bool,
    /// 两阶段都测到（都是 `Measured`）才有资格转 Measured。
    pub both_measured: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PhaseComparison {
    /// 环境或数据规模不同：两轮不可比，不允许冻结。
    Incomparable { diff: Vec<&'static str> },
    Comparable {
        drifts: Vec<ItemDrift>,
        tolerance_pct: f64,
        /// 只在某一阶段出现的项：无法判断稳定性，不得转 Measured。
        single_phase: Vec<&'static str>,
    },
}

impl PhaseComparison {
    /// 可以转成 `Measured` 的项：两阶段都测到且在容差内。
    pub fn promotable(&self) -> Vec<&'static str> {
        match self {
            Self::Incomparable { .. } => Vec::new(),
            Self::Comparable { drifts, .. } => drifts
                .iter()
                .filter(|drift| drift.both_measured && drift.within_tolerance)
                .map(|drift| drift.name)
                .collect(),
        }
    }

    /// 漂移超容差或只测到一次的项：保留 Target/Unverified 并写进复测条件。
    pub fn unresolved(&self) -> Vec<&'static str> {
        match self {
            Self::Incomparable { .. } => Vec::new(),
            Self::Comparable {
                drifts,
                single_phase,
                ..
            } => {
                let mut names: Vec<&'static str> = drifts
                    .iter()
                    .filter(|drift| !(drift.both_measured && drift.within_tolerance))
                    .map(|drift| drift.name)
                    .collect();
                names.extend(single_phase.iter().copied());
                names
            }
        }
    }

    /// 冻结预算的前提：两轮可比、且没有超出容差的项。
    pub fn stage(&self) -> BudgetStage {
        match self {
            Self::Incomparable { diff } => BudgetStage::Incomparable { diff: diff.clone() },
            Self::Comparable { drifts, .. } => {
                let drifted: Vec<&'static str> = drifts
                    .iter()
                    .filter(|drift| !drift.within_tolerance)
                    .map(|drift| drift.name)
                    .collect();
                BudgetStage::TwoComparablePhases { diff: drifted }
            }
        }
    }
}

/// 连续两个阶段跑同一测量集后对比。环境差异或数据规模差异会让整轮判为不可比。
pub fn compare_phases(
    first: &BaselineReport,
    second: &BaselineReport,
    tolerance_pct: f64,
) -> PhaseComparison {
    let diff = first.environment.diff(&second.environment);
    if !diff.is_empty() {
        return PhaseComparison::Incomparable { diff };
    }

    let mut drifts = Vec::new();
    let mut single_phase = Vec::new();
    for item in &first.items {
        let Some(other) = second.item(item.name) else {
            single_phase.push(item.name);
            continue;
        };
        let (Some(left), Some(right)) = (item.numeric, other.numeric) else {
            // 没有标量的项各自成条，不参与漂移判定。
            single_phase.push(item.name);
            continue;
        };
        let delta_pct = if left == 0.0 {
            if right == 0.0 { 0.0 } else { f64::INFINITY }
        } else {
            (right - left) / left
        };
        drifts.push(ItemDrift {
            name: item.name,
            first: left,
            second: right,
            delta_pct,
            within_tolerance: delta_pct.abs() * 100.0 <= tolerance_pct,
            both_measured: item.status == MeasureStatus::Measured
                && other.status == MeasureStatus::Measured,
        });
    }
    for item in &second.items {
        if first.item(item.name).is_none() {
            single_phase.push(item.name);
        }
    }

    PhaseComparison::Comparable {
        drifts,
        tolerance_pct,
        single_phase,
    }
}

/// 两阶段对比报告（4.1 / 4.2）。
pub fn render_phase_comparison(
    comparison: &PhaseComparison,
    first: &BaselineReport,
    second: &BaselineReport,
    tolerance_pct: f64,
) -> String {
    let mut out = String::new();
    out.push_str("# P7 两阶段预算冻结\n\n");
    let _ = writeln!(out, "- 阶段一 commit：{}", first.commit);
    let _ = writeln!(out, "- 阶段二 commit：{}", second.commit);
    let _ = writeln!(out, "- 阶段一环境：{}", first.environment.note);
    let _ = writeln!(out, "- 阶段二环境：{}", second.environment.note);
    let _ = writeln!(out, "- 数据规模：{}", first.environment.data_scale);
    let _ = writeln!(out, "- 漂移容差：±{tolerance_pct:.0}%");
    let (first_measured, second_measured) = (first.measured_count(), second.measured_count());
    let _ = writeln!(
        out,
        "- 已测项：阶段一 {first_measured} / 阶段二 {second_measured}"
    );
    out.push('\n');

    match comparison {
        PhaseComparison::Incomparable { diff } => {
            let _ = writeln!(
                out,
                "## 结论：不可比，不得冻结（差异项：{}）\n",
                diff.join("、")
            );
            out.push_str("两轮的环境或数据规模不同，数字不能相减；先对齐条件再重跑。\n");
            return out;
        }
        PhaseComparison::Comparable {
            drifts,
            single_phase,
            ..
        } => {
            out.push_str("## 单项漂移\n\n");
            out.push_str("| 指标 | 阶段一 | 阶段二 | 漂移 | 两阶段都测到 | 判定 |\n");
            out.push_str("| --- | --- | --- | --- | --- | --- |\n");
            for drift in drifts {
                let verdict = if !drift.both_measured {
                    "未两阶段都测到：保留 Target/Unverified"
                } else if drift.within_tolerance {
                    "可转 Measured"
                } else {
                    "超出容差：不转 Measured"
                };
                let _ = writeln!(
                    out,
                    "| `{}` | {:.3} | {:.3} | {:+.1}% | {} | {} |",
                    drift.name,
                    drift.first,
                    drift.second,
                    drift.delta_pct * 100.0,
                    if drift.both_measured { "是" } else { "否" },
                    verdict
                );
            }
            if !single_phase.is_empty() {
                let _ = writeln!(
                    out,
                    "\n只在单一阶段出现的项（无标量或不参与对比）：{}\n",
                    single_phase.join("、")
                );
            }

            let promotable = comparison.promotable();
            let unresolved = comparison.unresolved();
            let _ = writeln!(
                out,
                "## 结论：可转 Measured {} 项，保留 Target/Unverified {} 项\n",
                promotable.len(),
                unresolved.len()
            );
            if !promotable.is_empty() {
                let _ = writeln!(out, "- 可转 Measured：{}", promotable.join("、"));
            }
            if !unresolved.is_empty() {
                let _ = writeln!(out, "- 保留原状态：{}", unresolved.join("、"));
            }
            match freeze_budget(&comparison.stage()) {
                Ok(label) => {
                    let _ = writeln!(out, "- 预算冻结：**{label}**（两轮可比且无超容差项）");
                }
                Err(reason) => {
                    let _ = writeln!(out, "- 预算冻结：**未冻结**（{reason}）");
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 4.2 预算冻结、适用范围与超标项归因
// ---------------------------------------------------------------------------

/// 超标项的成因。冻结预算前必须先定位到「默认依赖」或「结构」，不允许直接调目标或降同步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftCause {
    /// 默认依赖：取的是某个库/配置的默认值（如 `busy_timeout` 默认 500 ms）。
    DefaultDependency {
        dependency: String,
        evidence: String,
    },
    /// 结构：写法/结构决定的成本（单写锁、逐条 fsync、全表扫）。
    Structure { name: String, evidence: String },
    /// 尚未定位：既不能据此冻结，也不能据此调目标。
    Unknown,
}

impl DriftCause {
    pub fn is_located(&self) -> bool {
        !matches!(self, Self::Unknown)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::DefaultDependency {
                dependency,
                evidence,
            } => format!("默认依赖 `{dependency}`（{evidence}）"),
            Self::Structure { name, evidence } => format!("结构「{name}」（{evidence}）"),
            Self::Unknown => "未定位".to_string(),
        }
    }
}

/// 单项超标的归因记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftAttribution {
    pub item: &'static str,
    pub causes: Vec<DriftCause>,
}

impl DriftAttribution {
    pub fn new(item: &'static str, causes: Vec<DriftCause>) -> Self {
        Self { item, causes }
    }

    /// 至少要有一条非 `Unknown` 的成因才算「定位过」。
    pub fn located(&self) -> bool {
        self.causes.iter().any(DriftCause::is_located)
    }

    pub fn describe(&self) -> String {
        if self.causes.is_empty() {
            return "未定位".to_string();
        }
        self.causes
            .iter()
            .map(DriftCause::describe)
            .collect::<Vec<_>>()
            .join("；")
    }
}

/// 冻结预算的适用范围。数字只在「同形态 + 同环境 + 同数据规模」下有效。
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetScope {
    /// 预算版本号，写进报告与复测触发条件。
    pub version: String,
    pub form_factor: FormFactor,
    /// 环境摘要（OS / CPU / rustc / 构建参数）。
    pub environment_note: String,
    /// 数据规模（事件数、事务数、写者数）。
    pub data_scale: String,
    pub tolerance_pct: f64,
}

impl BudgetScope {
    pub fn new(
        version: impl Into<String>,
        form_factor: FormFactor,
        environment_note: impl Into<String>,
        data_scale: impl Into<String>,
        tolerance_pct: f64,
    ) -> Self {
        Self {
            version: version.into(),
            form_factor,
            environment_note: environment_note.into(),
            data_scale: data_scale.into(),
            tolerance_pct,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} / {} + 环境「{}」+ 规模「{}」+ 容差 ±{:.0}%",
            self.version,
            self.form_factor.as_str(),
            self.environment_note,
            self.data_scale,
            self.tolerance_pct
        )
    }
}

/// 一条被冻结的预算项。
#[derive(Debug, Clone, PartialEq)]
pub struct FrozenItem {
    pub name: &'static str,
    pub form_factor: FormFactor,
    pub target: String,
    pub value: f64,
    pub meets_target: Option<bool>,
}

/// 冻结后的预算：只包含「两阶段都测到且在容差内」的项，其余进 `excluded` 并附复测条件。
#[derive(Debug, Clone, PartialEq)]
pub struct FrozenBudget {
    pub scope: BudgetScope,
    pub items: Vec<FrozenItem>,
    /// 未进预算的项：超容差、只测到一次或没有标量。
    pub excluded: Vec<&'static str>,
    /// 被排除项的定位结论，作为复测触发条件。
    pub attributions: Vec<DriftAttribution>,
}

impl FrozenBudget {
    pub fn item(&self, name: &str) -> Option<&FrozenItem> {
        self.items.iter().find(|item| item.name == name)
    }

    /// 预算里没有任何达标项时不许对外宣称「已冻结」。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// 冻结正式预算。
///
/// 三条硬规则：
/// 1. 两轮必须可比（环境与数据规模一致），否则冻结无效。
/// 2. 没有任何「靠降低 SQLite 同步 / 放宽事务 / 跳授权或门禁」换来的达标——这类变更一律阻断。
/// 3. 每个未进预算的项都必须**先**定位到默认依赖或结构，否则不许冻结（避免用调目标的方式"解决"超标）。
pub fn freeze_with_attribution(
    comparison: &PhaseComparison,
    report: &BaselineReport,
    scope: BudgetScope,
    attributions: &[DriftAttribution],
    relaxations: &[EngineChange],
) -> Result<FrozenBudget, String> {
    if let Some(change) = relaxations.first() {
        guard_change_in_capacity_stage(change)?;
    }

    let PhaseComparison::Comparable {
        drifts,
        tolerance_pct,
        ..
    } = comparison
    else {
        let diff = match comparison {
            PhaseComparison::Incomparable { diff } => diff.join("、"),
            PhaseComparison::Comparable { .. } => String::new(),
        };
        return Err(format!("两轮不可比（{diff}），冻结无效"));
    };

    let promotable = comparison.promotable();
    if promotable.is_empty() {
        return Err("两轮没有一项同时满足「都测到 + 在容差内」，无预算可冻结".to_string());
    }

    let excluded = comparison.unresolved();
    let mut missing: Vec<&'static str> = Vec::new();
    for name in &excluded {
        match attributions.iter().find(|entry| entry.item == *name) {
            Some(entry) if entry.located() => {}
            _ => missing.push(name),
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "阻断：超标/未定项尚未定位默认依赖与结构（{}），先定位再冻结",
            missing.join("、")
        ));
    }

    let mut items = Vec::new();
    for drift in drifts {
        if !promotable.contains(&drift.name) {
            continue;
        }
        let Some(source) = report.item(drift.name) else {
            continue;
        };
        items.push(FrozenItem {
            name: drift.name,
            form_factor: source.form_factor,
            target: source.target.clone(),
            value: drift.second,
            meets_target: source.meets_target,
        });
    }

    let mut scope = scope;
    scope.tolerance_pct = *tolerance_pct;

    Ok(FrozenBudget {
        scope,
        items,
        attributions: attributions
            .iter()
            .filter(|entry| excluded.contains(&entry.item))
            .cloned()
            .collect(),
        excluded,
    })
}

/// 冻结预算的 Markdown 记录（版本 / 适用范围 / 冻结项 / 排除项与复测条件）。
pub fn render_frozen_budget(budget: &FrozenBudget) -> String {
    let mut out = String::new();
    out.push_str("# P7 正式容量预算（冻结记录）\n\n");
    let _ = writeln!(out, "- 预算版本：{}", budget.scope.version);
    let _ = writeln!(out, "- 适用形态：{}", budget.scope.form_factor.as_str());
    let _ = writeln!(out, "- 适用环境：{}", budget.scope.environment_note);
    let _ = writeln!(out, "- 适用规模：{}", budget.scope.data_scale);
    let _ = writeln!(out, "- 漂移容差：±{:.0}%", budget.scope.tolerance_pct);
    let _ = writeln!(
        out,
        "- 冻结项：{} / 排除项：{}",
        budget.items.len(),
        budget.excluded.len()
    );
    out.push('\n');
    out.push_str("## 冻结项\n\n");
    out.push_str("| 指标 | 形态 | 目标 | 冻结值 | 达标 |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    for item in &budget.items {
        let meets = match item.meets_target {
            Some(true) => "是",
            Some(false) => "否",
            None => "无可比目标",
        };
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {:.3} | {} |",
            item.name,
            item.form_factor.as_str(),
            item.target,
            item.value,
            meets
        );
    }

    out.push_str("\n## 排除项与复测条件\n\n");
    if budget.excluded.is_empty() {
        out.push_str("无：全部项都两阶段可复现。\n");
    } else {
        out.push_str("| 指标 | 定位结论 | 复测条件 |\n");
        out.push_str("| --- | --- | --- |\n");
        for name in &budget.excluded {
            let located = budget
                .attributions
                .iter()
                .find(|entry| entry.item == *name)
                .map(DriftAttribution::describe)
                .unwrap_or_else(|| "未定位".to_string());
            let _ = writeln!(
                out,
                "| `{}` | {} | 同形态同规模连跑两轮、漂移回到 ±{:.0}% 内再纳入 |",
                name, located, budget.scope.tolerance_pct
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 1.1—1.4 报告渲染
// ---------------------------------------------------------------------------

/// Markdown 报告：Measured / Unverified / Target 分开列，并写明差异项。
pub fn render_markdown(report: &BaselineReport, diffs: &[&'static str]) -> String {
    let mut out = String::new();
    out.push_str("# P7 容量基线实测\n\n");
    let _ = writeln!(out, "- commit：{}", report.commit);
    let _ = writeln!(out, "- 环境：{}", report.environment.note);
    let _ = writeln!(
        out,
        "- OS / CPU：{} / {}",
        report.environment.os, report.environment.cpu
    );
    let _ = writeln!(out, "- rustc：{}", report.environment.rustc);
    let _ = writeln!(out, "- 构建参数：{}", report.environment.build_params);
    let _ = writeln!(out, "- SQLite：{}", report.environment.sqlite_sync);
    let _ = writeln!(out, "- 数据规模：{}", report.environment.data_scale);
    let _ = writeln!(out, "- 缓存：{}", report.environment.cache_state);
    let _ = writeln!(
        out,
        "- 结果：Measured {} / Unverified {} / Target {}",
        report.measured_count(),
        report.unverified_count(),
        report.target_count()
    );
    if diffs.is_empty() {
        out.push_str("- 与上一轮差异项：无（条件一致，可比较）\n\n");
    } else {
        let _ = writeln!(out, "- 与上一轮差异项：{}\n", diffs.join("、"));
    }

    out.push_str(
        "| 指标 | 形态 | 目标 | 实测 | 样本 | 状态 |\n| --- | --- | --- | --- | --- | --- |\n",
    );
    for item in &report.items {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} | {} |",
            item.name,
            item.form_factor.as_str(),
            item.target,
            item.display(),
            item.samples,
            item.status.as_str()
        );
    }
    out.push('\n');

    for item in &report.items {
        let _ = writeln!(out, "## {}", item.name);
        let _ = writeln!(out, "- 形态：{}", item.form_factor.as_str());
        let _ = writeln!(out, "- 状态：{}", item.status.as_str());
        let _ = writeln!(out, "- 目标：{}", item.target);
        let _ = writeln!(out, "- 实测：{}", item.display());
        let _ = writeln!(out, "- 样本数：{}", item.samples);
        let _ = writeln!(out, "- 命令：`{}`", item.command);
        if !item.method.is_empty() {
            let _ = writeln!(out, "- 方法：{}", item.method);
        }
        if !item.note.is_empty() {
            let _ = writeln!(out, "- 说明：{}", item.note);
        }
        if let Some(reason) = &item.failure_reason {
            let _ = writeln!(out, "- 未测原因：{reason}");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> EnvironmentRecord {
        EnvironmentRecord {
            note: "linux / x86_64 / rustc 1.99.1".to_string(),
            os: "linux x86_64".to_string(),
            cpu: "8 逻辑核".to_string(),
            rustc: "rustc 1.99.1".to_string(),
            build_params: "dev".to_string(),
            sqlite_sync: "WAL + FULL + IMMEDIATE".to_string(),
            data_scale: "100k 事件".to_string(),
            cache_state: "冷".to_string(),
        }
    }

    fn item_measured() -> MeasureItem {
        MeasureItem::new("transaction_p99", FormFactor::Headless, "≤ 50 ms")
            .with_method("200 次单条写事务")
            .measured(
                "12.3 ms".to_string(),
                200,
                "cargo run -p xtask -- bench",
                Some(true),
            )
    }

    fn full_decision() -> GraduationDecision {
        GraduationDecision {
            decision_id: "gd-1".to_string(),
            revision: 0,
            bottleneck: "单写 p99 与重启 ready".to_string(),
            report_ref: "docs/reports/p7-baseline.md".to_string(),
            environment: "wsl2 / 8 核".to_string(),
            comparison: EngineCandidate::ALL.to_vec(),
            chosen: "optimization".to_string(),
            owner: "backend".to_string(),
            rollback_path: "无需回滚：未改引擎".to_string(),
            uncovered: vec!["真实生产负载".to_string()],
            retest_trigger: "日均事件 > 5e6 或 RTO > 60 s".to_string(),
            decided_at: "2026-11-21".to_string(),
        }
    }

    /// 1.3：每项包含样本数、命令、环境与失败原因；吞吐与尾延迟不合并宣称。
    #[test]
    fn task_1_3_every_item_carries_samples_command_and_failure_reason() {
        let measured = item_measured();
        assert!(measured.usable_for_decision());
        assert_eq!(measured.samples, 200);
        assert!(!measured.command.is_empty());
        assert!(measured.failure_reason.is_none());

        let unverified = MeasureItem::new("state_to_web_visible_p95", FormFactor::Web, "≤ 2 s")
            .unverified("没有 Web 宿主", "cargo run -p xtask -- bench");
        assert!(!unverified.usable_for_decision(), "未测项不得进入决策");
        assert!(unverified.failure_reason.is_some(), "必须写明失败原因");
        assert_eq!(unverified.display(), "未测");
        assert!(!unverified.may_claim_target(), "未测项不得宣称达标");

        // 没有样本或没有命令的条目不算测到。
        let mut no_samples = measured.clone();
        no_samples.samples = 0;
        assert!(!no_samples.usable_for_decision());
        let mut no_command = measured.clone();
        no_command.command = String::new();
        assert!(!no_command.usable_for_decision());

        // 吞吐达标 + 尾延迟超标 → 不得宣称全部达标。
        let report =
            BaselineReport {
                environment: env(),
                commit: "abc".to_string(),
                items: vec![
                    MeasureItem::new("event_throughput", FormFactor::Headless, "≥ 5000/s")
                        .measured("9000 events/s".to_string(), 5_000, "bench", Some(true)),
                    MeasureItem::new("transaction_p99", FormFactor::Headless, "≤ 50 ms").measured(
                        "62 ms".to_string(),
                        200,
                        "bench",
                        Some(false),
                    ),
                ],
            };
        assert!(
            !report.may_claim_all_targets(),
            "尾延迟超标时不得宣称预算达标"
        );
        assert_eq!(
            report.unresolved(),
            vec!["transaction_p99"],
            "要能指出是哪一项挡着达标宣称"
        );
        assert!(
            report.usable_for_decision(),
            "但两项都测到了，可作为决策输入"
        );
    }

    /// 1.1：同条件可复现，差异项被列出。
    #[test]
    fn task_1_1_environment_diff_lists_reproducibility_gaps() {
        let base = env();
        assert!(base.diff(&base).is_empty(), "同条件应无差异项");

        // CLI 环境行不同不算条件差异：结构化字段才是比较口径。
        let same_conditions = EnvironmentRecord {
            note: "另一台机器的打印行".to_string(),
            ..env()
        };
        assert!(
            base.diff(&same_conditions).is_empty(),
            "note 不参与条件比较"
        );

        let other = EnvironmentRecord {
            sqlite_sync: "WAL + NORMAL".to_string(),
            data_scale: "10k 事件".to_string(),
            ..env()
        };
        assert_eq!(base.diff(&other), vec!["sqlite_sync", "data_scale"]);
    }

    /// 1.2：headless 数字不能冒充桌面；桌面达标要按项分别成立。
    #[test]
    fn task_1_2_headless_numbers_never_stand_in_for_desktop() {
        assert!(guard_form_factor(FormFactor::Headless, FormFactor::Headless).is_ok());
        for claimed in [FormFactor::Desktop, FormFactor::Web, FormFactor::Mobile] {
            assert!(
                guard_form_factor(FormFactor::Headless, claimed).is_err(),
                "headless 不得冒充 {claimed:?}"
            );
        }

        // headless 全绿不代表桌面达标：桌面项未测就不得宣称。
        let report = BaselineReport {
            environment: env(),
            commit: "abc".to_string(),
            items: vec![
                item_measured(),
                MeasureItem::new("desktop_total_memory", FormFactor::Desktop, "≤ 200 MiB")
                    .unverified("无桌面安装包", "bench"),
            ],
        };
        assert!(report.may_claim_form_factor(FormFactor::Headless));
        assert!(
            !report.may_claim_form_factor(FormFactor::Desktop),
            "桌面项未测，不得宣称桌面达标"
        );
    }

    /// 1.4：缺样本或缺命令的报告不进入决策；Target 项让整份报告退回。
    #[test]
    fn task_1_4_incomplete_reports_never_enter_capacity_decisions() {
        let mut report = BaselineReport {
            environment: env(),
            commit: "abc".to_string(),
            items: vec![item_measured()],
        };
        assert!(report.usable_for_decision());

        // 未测项带命令与原因，可以留在报告里。
        report.items.push(
            MeasureItem::new("idle_rss", FormFactor::Headless, "≤ 80 MiB")
                .unverified("读不到 /proc", "bench"),
        );
        assert!(
            report.usable_for_decision(),
            "写明缺口的 Unverified 项不影响可用性"
        );

        // 完全没测的 Target 项让报告退出决策。
        report.items.push(MeasureItem::new(
            "event_throughput",
            FormFactor::Headless,
            "≥ 5000/s",
        ));
        assert!(
            !report.usable_for_decision(),
            "Target 项没有样本与命令，报告整体不可用"
        );
        assert_eq!(report.target_count(), 1);
        assert_eq!(report.measured_count(), 1);
        assert_eq!(report.unverified_count(), 1);
    }

    /// 2.5：日均事件数不构成迁移理由；缺观测记 unknown。
    #[test]
    fn task_2_5_event_count_alone_never_triggers_migration() {
        // 只给规模指标、没有任何信号观测：直接拒绝。
        assert_eq!(
            graduation(&[], Some(1_000_000)),
            Graduation::RejectedByEventCount {
                daily_events: 1_000_000
            }
        );
        assert!(matches!(
            graduation(&[], None),
            Graduation::NoMigration { .. }
        ));

        let readings = vec![
            SignalReading {
                signal: GraduationSignal::RecoveryRto,
                observed: "18 s".to_string(),
                threshold: "≤ 60 s".to_string(),
                met: Some(false),
            },
            SignalReading {
                signal: GraduationSignal::WriteContention,
                observed: "无 Busy".to_string(),
                threshold: "Busy > 0 持续上升".to_string(),
                met: Some(false),
            },
            SignalReading {
                signal: GraduationSignal::VolumePressure,
                observed: "未测".to_string(),
                threshold: "重建 > 10 min".to_string(),
                met: None,
            },
            SignalReading {
                signal: GraduationSignal::MultiHostDemand,
                observed: "无".to_string(),
                threshold: "确有第二写入者".to_string(),
                met: Some(false),
            },
        ];
        match graduation(&readings, Some(1_000_000)) {
            Graduation::NoMigration { unknown, reasons } => {
                assert_eq!(unknown, vec![GraduationSignal::VolumePressure]);
                assert!(reasons.iter().any(|line| line.contains("日均事件数")));
                assert!(reasons.iter().any(|line| line.contains("unknown")));
            }
            other => panic!("百万日事件不应触发迁移，实得 {other:?}"),
        }

        // 有信号越界才进入方案对比。
        let mut triggered = readings.clone();
        triggered[0].met = Some(true);
        match graduation(&triggered, None) {
            Graduation::Candidate { signals, blocked } => {
                assert_eq!(signals, vec![GraduationSignal::RecoveryRto]);
                assert!(blocked.contains(&GraduationSignal::WriteContention));
            }
            other => panic!("RTO 越界应进入对比，实得 {other:?}"),
        }
    }

    /// 3.1：四类候选缺一不可，每项要带代价、负担、故障语义与恢复证据。
    #[test]
    fn task_3_1_all_four_candidates_must_be_compared() {
        let partial = vec![CandidateAssessment {
            candidate: EngineCandidate::Postgres,
            migration_cost: "高".to_string(),
            ops_burden: "需 DBA".to_string(),
            failure_semantics: "需重做单写次序".to_string(),
            recovery_evidence: "无".to_string(),
            requires_new_change: true,
        }];
        match compare_candidates(&partial) {
            Comparison::Incomplete { missing, .. } => {
                assert_eq!(missing.len(), 3);
                assert!(missing.contains(&EngineCandidate::Optimization));
                assert!(missing.contains(&EngineCandidate::ExecutorScale));
            }
            other => panic!("只列 PostgreSQL 不算对比，实得 {other:?}"),
        }

        let complete: Vec<CandidateAssessment> = EngineCandidate::ALL
            .iter()
            .map(|candidate| CandidateAssessment {
                candidate: *candidate,
                migration_cost: "记录".to_string(),
                ops_burden: "记录".to_string(),
                failure_semantics: "记录".to_string(),
                recovery_evidence: "记录".to_string(),
                requires_new_change: *candidate != EngineCandidate::Optimization,
            })
            .collect();
        assert!(matches!(
            compare_candidates(&complete),
            Comparison::Complete { .. }
        ));

        // 四项齐全但字段留空：同样不算对比。
        let hollow: Vec<CandidateAssessment> = complete
            .iter()
            .map(|item| CandidateAssessment {
                recovery_evidence: String::new(),
                ..item.clone()
            })
            .collect();
        match compare_candidates(&hollow) {
            Comparison::Incomplete {
                empty_fields,
                missing,
            } => {
                assert_eq!(missing.len(), 0);
                assert_eq!(empty_fields.len(), 4, "四项都缺恢复证据");
            }
            other => panic!("字段留空不得算完整对比，实得 {other:?}"),
        }
    }

    /// 3.2：DAO 替换不被当作语义兼容。
    #[test]
    fn task_3_2_dao_swap_is_not_semantic_compat() {
        assert!(dao_swap_is_not_semantic_compat().is_err());
    }

    /// 3.3：决定记录可审计、只追加、历史依据不被改写。
    #[test]
    fn task_3_3_decision_log_is_append_only_and_auditable() {
        let decision = full_decision();
        let mut log = DecisionLog::new();
        assert_eq!(log.record(decision.clone()), Ok(0));
        assert_eq!(
            log.record(decision.clone()),
            Err("同一决定已登记：修订请走 revise，历史依据不得改写"),
            "同一决定不得覆盖"
        );

        let revised = GraduationDecision {
            chosen: "optimization + executor_scale".to_string(),
            ..decision.clone()
        };
        assert_eq!(log.revise("gd-1", revised), Ok(1));
        assert_eq!(log.versions("gd-1"), 2);
        assert_eq!(
            log.earliest("gd-1").map(|item| item.chosen.as_str()),
            Some("optimization"),
            "历史依据保持原样"
        );
        assert_eq!(
            log.latest("gd-1").map(|item| item.chosen.as_str()),
            Some("optimization + executor_scale")
        );
        assert!(log.revise("gd-missing", decision.clone()).is_err());
    }

    /// 3.3：缺项的决定不得登记——审计要求先于记录便利。
    #[test]
    fn task_3_3_incomplete_decisions_cannot_be_recorded() {
        let mut incomplete = full_decision();
        incomplete.rollback_path = String::new();
        let mut log = DecisionLog::new();
        assert!(log.record(incomplete).is_err());
        assert_eq!(log.versions("gd-1"), 0);

        let mut short_comparison = full_decision();
        short_comparison.comparison = vec![EngineCandidate::Postgres];
        assert!(
            short_comparison.audit_gaps().contains(&"comparison"),
            "只列一项候选不算对比"
        );
    }

    /// 3.4：本 Change 内禁止引擎变更与降低正确性。
    #[test]
    fn task_3_4_engine_changes_are_blocked_inside_capacity_stage() {
        for change in [
            EngineChange::SwitchEngine {
                target: "postgres".to_string(),
            },
            EngineChange::LowerSyncLevel {
                level: "NORMAL".to_string(),
            },
            EngineChange::WidenTransaction,
            EngineChange::SkipGate {
                gate: "G-21".to_string(),
            },
        ] {
            let result = guard_change_in_capacity_stage(&change);
            assert!(result.is_err(), "{change:?} 必须被阻断");
            let message = result.unwrap_err();
            assert!(message.contains("阻断"), "拒绝理由要写明阻断：{message}");
        }
    }

    /// 3.5：性能结论不扩大支持矩阵。
    #[test]
    fn task_3_5_performance_never_promotes_support_matrix() {
        let report = BaselineReport {
            environment: env(),
            commit: "abc".to_string(),
            items: vec![item_measured()],
        };
        let refused = guard_support_promotion("remote_node", &report, false);
        assert!(refused.is_err());
        assert!(
            refused
                .unwrap_err()
                .contains("拒绝把 remote_node 标为 Supported"),
            "拒绝理由要点名能力"
        );
        assert!(
            guard_support_promotion("remote_node", &report, true).is_ok(),
            "有该能力的 conformance 与真机/双机报告才可提升"
        );
    }

    /// 4.1 / 4.2：两轮可比较阶段才可冻结预算。
    #[test]
    fn task_4_2_budget_freezes_only_after_two_comparable_phases() {
        assert!(freeze_budget(&BudgetStage::SinglePhase).is_err());
        assert!(
            freeze_budget(&BudgetStage::Incomparable {
                diff: vec!["data_scale"]
            })
            .is_err()
        );
        assert!(
            freeze_budget(&BudgetStage::TwoComparablePhases {
                diff: vec!["sqlite_sync"]
            })
            .is_err(),
            "还有差异项时不得冻结"
        );
        assert_eq!(
            freeze_budget(&BudgetStage::TwoComparablePhases { diff: Vec::new() }),
            Ok("budget-frozen")
        );
    }
}
