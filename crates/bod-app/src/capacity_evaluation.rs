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
#[derive(Debug, Clone, PartialEq, Eq)]
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
            failure_reason: None,
            status: MeasureStatus::Target,
        }
    }

    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = method.into();
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
