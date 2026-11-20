//! P2 任务 4.4：接续样本驱动。
//!
//! 规格（`conformance-toolkit`「双运行时接续证据」）要求**至少两个已测运行配置在相同任务样本上
//! 完成 L1/L2 接续**，报告记录环境、版本、样本、失败路径与人工耗时；缺任一项保持
//! Planned/Unverified。说明文字满足不了这条——缺的是一次真实可复跑的采样。
//!
//! 样本 `sample.resume-from-snapshot/v1` 做四件事：
//!
//! 1. 配置 A 起真实子进程，产出 `checkpoint 1`、`checkpoint 2`；
//! 2. 把「工作包修订 + 最后一条观察序号」写成交接快照文件；
//! 3. 确认停止 A（`CancelStage::Confirmed`，未确认即判失败）；
//! 4. **新实例**（新进程、新内存）读快照续跑，第一条输出必须是 `resume from <序号>`。
//!
//! 第 4 步既要回声序号，又要产出 `checkpoint 3`。这是整个样本唯一不可省的断言：新执行端若没读
//! 快照，它只能从零重来，而「重来」与「接续」在副作用上是两件事——把重来记成接续，正是这一
//! 规格要防的。
//!
//! L2 另加一条：交接后**又开一个实例**（模拟宿主重启）仍能重放 A 的完整观察序列（序号连续、
//! 内容一致），且重复接收在宿主 inbox 里判 `duplicate`、不重复落库。这正是 `local-process`
//! 声明不支持的那一项——它如实得到 `unsupported`，而不是被记成缺陷。
//!
//! 三条边界，本模块不去越过：
//!
//! - **人工耗时不由本模块编造**。`HumanTime::Scripted` 表示「实测没有人工介入」，与
//!   `HumanTime::Unrecorded`（没测）是两件事，报告分开呈现。
//! - **L3 不做**。L3 还要求「核验输入与结论后再接续」，本样本没有该步骤，如实报
//!   `unsupported(verified_continuation)`，不用 L1/L2 的通过冒充。
//! - **通过 ≠ 已登记门禁**。本模块只产出证据；`verification/gates.yaml` 与支持矩阵由 5.3 裁定。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::BodConfig;
use crate::domain::execution::{
    AdapterDescriptor, CancelStage, CapabilitySlot, ObservationEnvelope, StartRequest, SupportLevel,
};
use crate::handoff_package::HandoffLevel;
use crate::invocation::{StartOutcome, StartTransport};
use crate::protocol_reliability::receive_observation;
use crate::store::SqliteStore;

pub const SAMPLE_SCHEMA: &str = "bod.handoff.sample/v1";
/// 固定任务样本 ID：两配置必须跑同一份，换了内容就该换版本。
pub const TASK_SAMPLE_ID: &str = "sample.resume-from-snapshot/v1";
/// 规格要求的最少**不同运行时**个数。同一运行时跑两个适配器不算跨运行时接续。
pub const REQUIRED_RUNTIMES: usize = 2;
pub const SNAPSHOT_FILE: &str = "handoff-sample.json";
pub const WORK_PACKAGE_REVISION: &str = "wp.sample.resume/v1";

// 观察窗口给足以躲开并行跑全量时的瞬时负载：poll 一旦集齐就返回，正常路径不额外等待。
// 窗口太短会把「还没写出来」误判成「没有输出」，那是测试环境的错，不是被测事实。
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_STEP: Duration = Duration::from_millis(20);
const CHECKPOINT_ONE: &str = "sample: checkpoint 1";
const CHECKPOINT_TWO: &str = "sample: checkpoint 2";
const CHECKPOINT_THREE: &str = "sample: checkpoint 3";

/// 样本的三个步骤。`Replay` 模拟宿主重启：**新实例**，不共享任何内存态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Produce,
    Resume,
    Replay,
}

/// 一个运行配置：适配器身份 + 真实运行时版本 + 平台。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProfile {
    pub adapter_id: String,
    pub adapter_version: String,
    /// 运行时版本行（如 `Python 3.13.14`、`v22.22.2`）。跨运行时的判定就靠它。
    pub runtime: String,
    pub platform: String,
}

impl RuntimeProfile {
    pub fn label(&self) -> String {
        format!(
            "{} v{} / {} / {}",
            self.adapter_id, self.adapter_version, self.runtime, self.platform
        )
    }
}

/// 采样对象。每次 `open` 都必须返回**新实例**：样本要验的正是「内存态不共享时能否接续」。
pub trait SampleConfig {
    fn profile(&self) -> RuntimeProfile;
    fn open(
        &self,
        phase: Phase,
        snapshot: Option<&Path>,
    ) -> Result<Box<dyn StartTransport>, String>;
}

/// 人工介入的实测值。
///
/// **实测与口述必须分开**：`Drilled` 来自 `drill.summary`（时间戳由系统时钟写入、由两端档位
/// 算出），`Claimed` 是命令行徒手给的数字。两者都能写进报告，但只有前者能算 4.4 的
/// 「人工耗时」证据——否则敲一个数字就能让报告显示实测值。
#[derive(Debug, Clone, PartialEq)]
pub enum HumanTime {
    /// 脚本驱动：实测**没有**人工介入。是测到的 0，不是没测。
    Scripted,
    /// 真实演练记录算出分钟数（秒表：原执行者最后一次有效动作 → 接手者第一次有效动作）。
    Drilled { minutes: f64, source: String },
    /// 命令行徒手给的值：口述值，未附演练记录，不计入 4.4。
    Claimed(f64),
    /// 没有记录。报告必须显式点出，不能默认成 0。
    Unrecorded,
}

impl HumanTime {
    pub fn label(&self) -> String {
        match self {
            Self::Scripted => "0（脚本化，无人工介入）".to_string(),
            Self::Drilled { minutes, source } => {
                format!("{minutes:.1} 分钟（演练记录 {source}）")
            }
            Self::Claimed(minutes) => format!("{minutes:.1} 分钟（口述值，未附演练记录）"),
            Self::Unrecorded => "未记录".to_string(),
        }
    }

    pub const fn is_recorded(&self) -> bool {
        !matches!(self, Self::Unrecorded)
    }

    /// 是否**可追溯**：只有脚本化零介入与演练记录算。口述值有数字但没有出处。
    pub const fn is_evidenced(&self) -> bool {
        matches!(self, Self::Scripted | Self::Drilled { .. })
    }

    /// 报告里给「为什么不算」的一句话。
    pub fn caveat(&self) -> Option<&'static str> {
        match self {
            Self::Claimed(_) => Some("口述值：没有演练记录，不能算 4.4 的人工耗时证据"),
            Self::Unrecorded => Some("未测：不能按 0 处理"),
            _ => None,
        }
    }
}

/// 从真实演练记录取人工耗时。**未封存或缺档位就报错**，不退回默认值。
pub fn human_time_from_drill(root: &Path) -> Result<HumanTime, String> {
    let session = crate::handoff_drill::load(root)?;
    let minutes = session.handoff_minutes()?;
    Ok(HumanTime::Drilled {
        minutes,
        source: session.source_label(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleStep {
    Produce,
    Snapshot,
    Stop,
    Resume,
    Replay,
}

impl SampleStep {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Produce => "produce",
            Self::Snapshot => "snapshot",
            Self::Stop => "stop",
            Self::Resume => "resume",
            Self::Replay => "replay",
        }
    }
}

/// 三种结论分开：**未测**（unsupported）与**测了没过**（failed）不是一回事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleOutcome {
    Passed {
        assertions: usize,
    },
    Failed {
        step: SampleStep,
        code: &'static str,
        detail: String,
    },
    /// 该配置声明不具备该等级的保证。不是缺陷，但也**不能**算通过。
    Unsupported {
        slot: &'static str,
        detail: String,
    },
}

impl SampleOutcome {
    pub const fn passed(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }

    /// 报告与 JSON 共用的短标签。
    pub fn label(&self) -> String {
        match self {
            Self::Passed { assertions } => format!("pass（{assertions} 项断言）"),
            Self::Failed { step, code, .. } => format!("fail@{}/{}", step.as_str(), code),
            Self::Unsupported { slot, .. } => format!("unsupported({slot})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SampleRecord {
    pub profile: RuntimeProfile,
    pub sample_id: String,
    pub level: HandoffLevel,
    pub started_at_ms: i64,
    pub elapsed_ms: i64,
    pub outcome: SampleOutcome,
    pub human_time: HumanTime,
}

/// 交接快照：新执行端要读的东西。字段名与两侧脚本的读音一致（`last_sequence`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFile {
    pub sample_id: String,
    pub work_package_revision: String,
    pub last_sequence: u64,
    pub produced_by: RuntimeProfile,
    pub produced_at_ms: i64,
}

impl SnapshotFile {
    pub fn render(&self) -> String {
        serde_json::json!({
            "schema": SAMPLE_SCHEMA,
            "sample_id": &self.sample_id,
            "work_package_revision": &self.work_package_revision,
            "last_sequence": self.last_sequence,
            "produced_by": {
                "adapter_id": &self.produced_by.adapter_id,
                "adapter_version": &self.produced_by.adapter_version,
                "runtime": &self.produced_by.runtime,
                "platform": &self.produced_by.platform,
            },
            "produced_at_ms": self.produced_at_ms,
        })
        .to_string()
    }
}

// ---------------------------------------------------------------- 运行配置

/// `adapter.durable-process`：可回放观察，声明 `recovery` 与 `reliable_observation`。
pub struct DurableSampleConfig {
    pub config: crate::durable_process::DurableProcessConfig,
    profile: RuntimeProfile,
}

impl DurableSampleConfig {
    pub fn new(config: crate::durable_process::DurableProcessConfig, now_ms: i64) -> Self {
        let adapter = crate::durable_process::DurableProcessAdapter::new(config.clone(), now_ms);
        let descriptor = adapter.descriptor();
        let profile = RuntimeProfile {
            adapter_id: descriptor.adapter_id.clone(),
            adapter_version: descriptor.adapter_version.clone(),
            runtime: runtime_version(&config.executable),
            platform: descriptor.platform.clone(),
        };
        Self { config, profile }
    }
}

impl SampleConfig for DurableSampleConfig {
    fn profile(&self) -> RuntimeProfile {
        self.profile.clone()
    }

    fn open(
        &self,
        phase: Phase,
        snapshot: Option<&Path>,
    ) -> Result<Box<dyn StartTransport>, String> {
        let args = phase_args(&self.config.executable, phase, snapshot)?;
        let mut adapter =
            crate::durable_process::DurableProcessAdapter::new(self.config.clone(), now_ms());
        adapter.set_args(args, now_ms());
        Ok(Box::new(adapter))
    }
}

/// `adapter.local-process`：观察在内存里，重启即丢。它会在 L2 上如实得到 `unsupported`。
pub struct LocalSampleConfig {
    pub config: crate::local_process::LocalProcessConfig,
    profile: RuntimeProfile,
}

impl LocalSampleConfig {
    pub fn new(config: crate::local_process::LocalProcessConfig, now_ms: i64) -> Self {
        let adapter = crate::local_process::LocalProcessAdapter::new(config.clone(), now_ms);
        let descriptor = adapter.descriptor();
        let profile = RuntimeProfile {
            adapter_id: descriptor.adapter_id.clone(),
            adapter_version: descriptor.adapter_version.clone(),
            runtime: runtime_version(&config.executable),
            platform: descriptor.platform.clone(),
        };
        Self { config, profile }
    }
}

impl SampleConfig for LocalSampleConfig {
    fn profile(&self) -> RuntimeProfile {
        self.profile.clone()
    }

    fn open(
        &self,
        phase: Phase,
        snapshot: Option<&Path>,
    ) -> Result<Box<dyn StartTransport>, String> {
        let args = phase_args(&self.config.executable, phase, snapshot)?;
        let mut adapter =
            crate::local_process::LocalProcessAdapter::new(self.config.clone(), now_ms());
        adapter.set_args(args, now_ms());
        Ok(Box::new(adapter))
    }
}

/// 探一个可执行文件的版本；不可执行返回 `None`（不猜、不回落成"可用"）。
pub fn runtime_version(executable: &str) -> String {
    let output = Command::new(executable).arg("--version").output();
    let Ok(output) = output else {
        return executable.to_string();
    };
    if !output.status.success() {
        return executable.to_string();
    }
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(executable);
    line.to_string()
}

/// 本机可用运行时，按版本行去重（`python3` 与 `python` 常是同一个解释器，不该算两个配置）。
pub fn detect_runtimes(candidates: &[&str]) -> Vec<String> {
    let mut found: Vec<(String, String)> = Vec::new();
    for candidate in candidates {
        let version = runtime_version(candidate);
        if !is_runnable(candidate) {
            continue;
        }
        if found.iter().any(|(_, seen)| seen == &version) {
            continue;
        }
        found.push(((*candidate).to_string(), version));
    }
    found.into_iter().map(|(exe, _)| exe).collect()
}

fn is_runnable(executable: &str) -> bool {
    Command::new(executable)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- 脚本

fn is_node(executable: &str) -> bool {
    executable.to_ascii_lowercase().contains("node")
}

/// 三个阶段的参数。`Resume` 必须拿到快照路径，否则「接续」无从谈起。
fn phase_args(
    executable: &str,
    phase: Phase,
    snapshot: Option<&Path>,
) -> Result<Vec<String>, String> {
    let script = match (is_node(executable), phase) {
        (false, Phase::Produce) | (false, Phase::Replay) => {
            "import time\nprint('sample: checkpoint 1', flush=True)\nprint('sample: checkpoint 2', flush=True)\ntime.sleep(20)"
        }
        (false, Phase::Resume) => {
            "import json,sys,time\nsnap=json.load(open(sys.argv[1]))\nprint('sample: resume from %d' % snap['last_sequence'], flush=True)\nprint('sample: checkpoint 3', flush=True)\ntime.sleep(5)"
        }
        (true, Phase::Produce) | (true, Phase::Replay) => {
            "console.log('sample: checkpoint 1');console.log('sample: checkpoint 2');setTimeout(()=>{},20000);"
        }
        (true, Phase::Resume) => {
            "const fs=require('fs');const s=JSON.parse(fs.readFileSync(process.argv[1],'utf8'));console.log('sample: resume from '+s.last_sequence);console.log('sample: checkpoint 3');setTimeout(()=>{},5000);"
        }
    };
    let mut args = if is_node(executable) {
        vec!["-e".to_string(), script.to_string()]
    } else {
        vec!["-c".to_string(), script.to_string()]
    };
    if phase == Phase::Resume {
        let path = snapshot.ok_or_else(|| "续跑阶段必须提供交接快照路径".to_string())?;
        args.push(path.to_string_lossy().to_string());
    }
    Ok(args)
}

// ---------------------------------------------------------------- 驱动

fn start_request(invocation_id: &str) -> StartRequest {
    StartRequest {
        invocation_id: invocation_id.to_string(),
        run_id: "run-handoff-sample".to_string(),
        work_package_revision: WORK_PACKAGE_REVISION.to_string(),
        attempt_id: format!("attempt-{invocation_id}"),
        binding_id: "binding-handoff-sample".to_string(),
        fencing_token: 1,
        agent_ref: "agent-handoff-sample".to_string(),
        runtime_config_revision: "rt-handoff-sample".to_string(),
        context_bundle_ref: "ctx-handoff-sample".to_string(),
        workspace_ref: "ws-handoff-sample".to_string(),
        grant_ref: "grant-handoff-sample".to_string(),
        reservation_refs: Vec::new(),
        deadline_at_ms: None,
    }
}

struct StepFailure {
    step: SampleStep,
    code: &'static str,
    detail: String,
}

impl StepFailure {
    fn new(step: SampleStep, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            step,
            code,
            detail: detail.into(),
        }
    }
}

enum StepOutcome {
    Passed(usize),
    Failed(StepFailure),
    Unsupported { slot: &'static str, detail: String },
}

/// 跑一次样本并落一条记录。失败不抛错——失败本身就是证据，必须留在报告里。
pub fn run_sample(
    config: &dyn SampleConfig,
    level: HandoffLevel,
    workspace: &Path,
    human_time: HumanTime,
    at_ms: i64,
) -> SampleRecord {
    let started = Instant::now();
    let profile = config.profile();
    let outcome = match sample_steps(config, level, workspace, at_ms) {
        StepOutcome::Passed(assertions) => SampleOutcome::Passed { assertions },
        StepOutcome::Failed(failure) => SampleOutcome::Failed {
            step: failure.step,
            code: failure.code,
            detail: failure.detail,
        },
        StepOutcome::Unsupported { slot, detail } => SampleOutcome::Unsupported { slot, detail },
    };
    SampleRecord {
        profile,
        sample_id: TASK_SAMPLE_ID.to_string(),
        level,
        started_at_ms: at_ms,
        elapsed_ms: started.elapsed().as_millis() as i64,
        outcome,
        human_time,
    }
}

fn sample_steps(
    config: &dyn SampleConfig,
    level: HandoffLevel,
    workspace: &Path,
    at_ms: i64,
) -> StepOutcome {
    if level == HandoffLevel::L3 {
        return StepOutcome::Unsupported {
            slot: "verified_continuation",
            detail: "本样本只覆盖 L1/L2：L3 还要求核验输入与结论后再接续，样本里没有该步骤"
                .to_string(),
        };
    }
    let stamp = at_ms.max(0);
    let produce_id = format!("inv-sample-{stamp}-a");
    let resume_id = format!("inv-sample-{stamp}-b");

    // 1. 产出
    let mut producer = match config.open(Phase::Produce, None) {
        Ok(transport) => transport,
        Err(err) => {
            return StepOutcome::Failed(StepFailure::new(
                SampleStep::Produce,
                "adapter_unavailable",
                err,
            ));
        }
    };
    let descriptor: AdapterDescriptor = producer.descriptor().clone();
    match producer.start(&start_request(&produce_id)) {
        StartOutcome::Accepted { .. } => {}
        other => {
            return StepOutcome::Failed(StepFailure::new(
                SampleStep::Produce,
                "produce_not_started",
                format!("启动未受理：{other:?}"),
            ));
        }
    }
    let produced = poll(producer.as_mut(), &produce_id, 2, SAMPLE_TIMEOUT);
    if produced.len() < 2 {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Produce,
            "produce_no_output",
            format!("只读到 {} 条观察，期望两个检查点", produced.len()),
        ));
    }
    let payloads: Vec<&str> = produced.iter().map(|item| item.payload.as_str()).collect();
    if !payloads[0].contains(CHECKPOINT_ONE) || !payloads[1].contains(CHECKPOINT_TWO) {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Produce,
            "produce_unexpected_output",
            format!("检查点内容不符：{payloads:?}"),
        ));
    }
    let last_sequence = produced
        .last()
        .map(|item| item.source_sequence)
        .unwrap_or(0);

    // 2. 交接快照
    if let Err(err) = fs::create_dir_all(workspace) {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Snapshot,
            "snapshot_write_failed",
            format!("无法创建样本目录：{err}"),
        ));
    }
    let snapshot_path: PathBuf = workspace.join(SNAPSHOT_FILE);
    let snapshot = SnapshotFile {
        sample_id: TASK_SAMPLE_ID.to_string(),
        work_package_revision: WORK_PACKAGE_REVISION.to_string(),
        last_sequence,
        produced_by: config.profile(),
        produced_at_ms: at_ms,
    };
    if let Err(err) = fs::write(&snapshot_path, snapshot.render()) {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Snapshot,
            "snapshot_write_failed",
            format!("写交接快照失败：{err}"),
        ));
    }

    // 3. 停旧执行：未确认即失败，不把"请求了"当成"停了"
    let stage: CancelStage = producer.cancel(&produce_id);
    if !stage.stop_confirmed() {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Stop,
            "stop_not_confirmed",
            "停止只被受理、未确认；此时不能宣称现场已冻结".to_string(),
        ));
    }
    // 丢掉产出端的句柄：接续与重放都必须不依赖它的内存态。
    drop(producer);

    // 4. 新实例读快照续跑
    let mut resumer = match config.open(Phase::Resume, Some(&snapshot_path)) {
        Ok(transport) => transport,
        Err(err) => {
            return StepOutcome::Failed(StepFailure::new(
                SampleStep::Resume,
                "adapter_unavailable",
                err,
            ));
        }
    };
    match resumer.start(&start_request(&resume_id)) {
        StartOutcome::Accepted { .. } => {}
        other => {
            return StepOutcome::Failed(StepFailure::new(
                SampleStep::Resume,
                "resume_not_started",
                format!("续跑启动未受理：{other:?}"),
            ));
        }
    }
    let resumed = poll(resumer.as_mut(), &resume_id, 2, SAMPLE_TIMEOUT);
    if resumed.is_empty() {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Resume,
            "resume_no_output",
            "续跑没有任何输出".to_string(),
        ));
    }
    let first = resumed[0].payload.clone();
    let echo = format!("resume from {last_sequence}");
    if !first.contains(&echo) {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Resume,
            "resume_not_continuing",
            format!("第一条输出是 {first:?}，没有回声 {echo:?}：新执行端没有读交接快照"),
        ));
    }
    if !resumed
        .iter()
        .any(|item| item.payload.contains(CHECKPOINT_THREE))
    {
        return StepOutcome::Failed(StepFailure::new(
            SampleStep::Resume,
            "resume_incomplete",
            format!("没有产出 {CHECKPOINT_THREE}：接续没走完"),
        ));
    }
    let _ = resumer.cancel(&resume_id);

    // 5. L1 到此成立；L2 追加「重放不丢、重放幂等」
    let mut assertions = 9;
    if level == HandoffLevel::L2 {
        if descriptor.level_of(CapabilitySlot::ReliableObservation) != SupportLevel::Supported {
            return StepOutcome::Unsupported {
                slot: "reliable_observation",
                detail: "描述符声明 unsupported：重启后无法重放，L2 的「失联不丢」无证据"
                    .to_string(),
            };
        }
        match replay_check(config, &produce_id, &produced, at_ms) {
            Ok(extra) => assertions += extra,
            Err(failure) => return StepOutcome::Failed(failure),
        }
    }
    StepOutcome::Passed(assertions)
}

/// 交接之后再开一个实例：旧观察必须还读得到、序号连续、重复接收判重。
fn replay_check(
    config: &dyn SampleConfig,
    invocation_id: &str,
    expected: &[ObservationEnvelope],
    at_ms: i64,
) -> Result<usize, StepFailure> {
    let mut fresh = config
        .open(Phase::Replay, None)
        .map_err(|err| StepFailure::new(SampleStep::Replay, "adapter_unavailable", err))?;
    let replayed = fresh.observations(invocation_id);
    if replayed.is_empty() {
        return Err(StepFailure::new(
            SampleStep::Replay,
            "replay_unavailable",
            "新实例读不到旧观察：观察只活在旧实例的内存里".to_string(),
        ));
    }
    let sequences: Vec<u64> = replayed.iter().map(|item| item.source_sequence).collect();
    let wanted: Vec<u64> = (1..=expected.len() as u64).collect();
    if sequences != wanted {
        return Err(StepFailure::new(
            SampleStep::Replay,
            "replay_gap",
            format!("重放序号 {sequences:?}，期望 {wanted:?}"),
        ));
    }
    let got: Vec<&str> = replayed.iter().map(|item| item.payload.as_str()).collect();
    let want: Vec<&str> = expected.iter().map(|item| item.payload.as_str()).collect();
    if got != want {
        return Err(StepFailure::new(
            SampleStep::Replay,
            "replay_payload_mismatch",
            format!("重放内容 {got:?}，期望 {want:?}"),
        ));
    }

    let mut store = SqliteStore::in_memory(&BodConfig::default()).map_err(|err| {
        StepFailure::new(
            SampleStep::Replay,
            "store_unavailable",
            format!("内存库不可用：{err}"),
        )
    })?;
    for envelope in expected {
        let receipt = receive_observation(&mut store, envelope, None, at_ms).map_err(|err| {
            StepFailure::new(
                SampleStep::Replay,
                "receive_failed",
                format!("首次接收失败：{err}"),
            )
        })?;
        if !receipt.persisted {
            return Err(StepFailure::new(
                SampleStep::Replay,
                "receive_not_persisted",
                format!("首次接收未落库（{}）", receipt.disposition),
            ));
        }
    }
    for envelope in &replayed {
        let receipt = receive_observation(&mut store, envelope, None, at_ms).map_err(|err| {
            StepFailure::new(
                SampleStep::Replay,
                "receive_failed",
                format!("重放接收失败：{err}"),
            )
        })?;
        if receipt.disposition != "duplicate" || receipt.persisted {
            return Err(StepFailure::new(
                SampleStep::Replay,
                "replay_not_idempotent",
                format!(
                    "重放被判成 {}（persisted={}），应判 duplicate 且不落库",
                    receipt.disposition, receipt.persisted
                ),
            ));
        }
    }
    let stored = store.count_observations(invocation_id).map_err(|err| {
        StepFailure::new(
            SampleStep::Replay,
            "store_unavailable",
            format!("计数失败：{err}"),
        )
    })?;
    if stored != expected.len() as i64 {
        return Err(StepFailure::new(
            SampleStep::Replay,
            "replay_not_idempotent",
            format!("重放后库里 {} 条，期望 {}", stored, expected.len()),
        ));
    }
    Ok(4)
}

fn poll(
    transport: &mut dyn StartTransport,
    invocation_id: &str,
    want: usize,
    timeout: Duration,
) -> Vec<ObservationEnvelope> {
    let started = Instant::now();
    let mut all = Vec::new();
    loop {
        all.extend(transport.observations(invocation_id));
        if all.len() >= want || started.elapsed() >= timeout {
            return all;
        }
        std::thread::sleep(POLL_STEP);
    }
}

// ---------------------------------------------------------------- 报告与结论

/// 接续声明。只有**至少两个不同运行时**在同一样本上通过，才算拿到证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Evidenced {
        runtimes: Vec<String>,
    },
    Insufficient {
        passed: usize,
        required: usize,
        blockers: Vec<String>,
    },
}

impl Claim {
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Evidenced { .. } => "evidenced",
            Self::Insufficient { .. } => "insufficient",
        }
    }

    pub fn passed(&self) -> usize {
        match self {
            Self::Evidenced { runtimes } => runtimes.len(),
            Self::Insufficient { passed, .. } => *passed,
        }
    }

    /// 机器可读的一行，报告与 JSON 共用。
    pub fn line(&self) -> String {
        format!(
            "接续证据: {} ({}/{})",
            self.state(),
            self.passed(),
            REQUIRED_RUNTIMES
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SampleReport {
    pub sample_id: String,
    pub level: HandoffLevel,
    pub generated_at_ms: i64,
    pub records: Vec<SampleRecord>,
    /// 真实演练记录：给了就附在报告里，让「人工耗时」与「5.2 对照」有出处可查。
    pub drill: Option<crate::handoff_drill::DrillSession>,
}

impl SampleReport {
    pub fn new(level: HandoffLevel, generated_at_ms: i64) -> Self {
        Self {
            sample_id: TASK_SAMPLE_ID.to_string(),
            level,
            generated_at_ms,
            records: Vec::new(),
            drill: None,
        }
    }

    pub fn attach_drill(&mut self, drill: crate::handoff_drill::DrillSession) {
        self.drill = Some(drill);
    }

    pub fn push(&mut self, record: SampleRecord) {
        self.records.push(record);
    }

    /// 按**运行时**去重计数：同一运行时换适配器不算跨运行时。
    pub fn claim(&self) -> Claim {
        let mut runtimes: Vec<String> = Vec::new();
        for record in self.records.iter().filter(|item| item.outcome.passed()) {
            if !runtimes.iter().any(|seen| seen == &record.profile.runtime) {
                runtimes.push(record.profile.runtime.clone());
            }
        }
        if runtimes.len() >= REQUIRED_RUNTIMES {
            return Claim::Evidenced { runtimes };
        }
        let mut blockers: Vec<String> = self
            .records
            .iter()
            .filter(|item| !item.outcome.passed())
            .map(|item| format!("{}: {}", item.profile.label(), item.outcome.label()))
            .collect();
        blockers.push(format!(
            "通过的不同运行时只有 {} 个（需 {}）：同一运行时换适配器不算跨运行时接续",
            runtimes.len(),
            REQUIRED_RUNTIMES
        ));
        Claim::Insufficient {
            passed: runtimes.len(),
            required: REQUIRED_RUNTIMES,
            blockers,
        }
    }

    pub fn to_markdown(&self) -> String {
        let claim = self.claim();
        let mut out = String::new();
        out.push_str("# 接续样本报告（P2 任务 4.4）\n\n");
        out.push_str(&format!("- schema: {SAMPLE_SCHEMA}\n"));
        out.push_str(&format!("- 样本: {}\n", self.sample_id));
        out.push_str(&format!("- 等级: {}\n", self.level.as_str()));
        out.push_str(&format!(
            "- 生成时间: {}\n",
            utc_stamp(self.generated_at_ms)
        ));
        out.push_str(&format!(
            "- 要求: 至少 {REQUIRED_RUNTIMES} 个**不同运行时**的运行配置在同一样本上通过\n\n"
        ));
        out.push_str(&format!("{}\n", claim.line()));
        if let Claim::Evidenced { runtimes } = &claim {
            out.push_str(&format!("已证运行时: {}\n", runtimes.join(", ")));
        }
        out.push('\n');

        out.push_str("| # | 运行配置 | 结果 | 人工耗时 | 用时 |\n");
        out.push_str("| --- | --- | --- | --- | --- |\n");
        for (index, record) in self.records.iter().enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} ms |\n",
                index + 1,
                record.profile.label(),
                record.outcome.label(),
                record.human_time.label(),
                record.elapsed_ms
            ));
        }

        let failures: Vec<&SampleRecord> = self
            .records
            .iter()
            .filter(|item| !item.outcome.passed())
            .collect();
        if failures.is_empty() {
            out.push_str("\n失败路径: 无（所有配置均通过）\n");
        } else {
            out.push_str("\n## 失败路径与不适用（如实保留）\n\n");
            for record in failures {
                match &record.outcome {
                    SampleOutcome::Failed { step, code, detail } => out.push_str(&format!(
                        "- {} — failed@{}/{}：{}\n",
                        record.profile.label(),
                        step.as_str(),
                        code,
                        detail
                    )),
                    SampleOutcome::Unsupported { slot, detail } => out.push_str(&format!(
                        "- {} — unsupported({slot})：{}\n",
                        record.profile.label(),
                        detail
                    )),
                    SampleOutcome::Passed { .. } => {}
                }
            }
        }

        let unrecorded: Vec<&SampleRecord> = self
            .records
            .iter()
            .filter(|item| !item.human_time.is_recorded())
            .collect();
        let claimed: Vec<&SampleRecord> = self
            .records
            .iter()
            .filter(|item| matches!(item.human_time, HumanTime::Claimed(_)))
            .collect();
        let evidenced = self
            .records
            .iter()
            .filter(|item| item.human_time.is_evidenced())
            .count();
        out.push_str(&format!(
            "\n人工耗时证据: {evidenced}/{}（脚本化零介入与演练记录算，口述值不算）\n",
            self.records.len()
        ));
        if unrecorded.is_empty() {
            out.push_str("\n人工耗时: 每条都有记录（脚本化样本记 0，不记“没测”）。\n");
        } else {
            out.push_str("\n人工耗时: 下列配置未记录，不能按 0 处理。\n");
            for record in unrecorded {
                out.push_str(&format!("- {}\n", record.profile.label()));
            }
        }
        if !claimed.is_empty() {
            out.push_str("\n口述值（不计入 4.4，需 `bod handoff drill` 记录的演练读数替代）:\n");
            for record in claimed {
                out.push_str(&format!(
                    "- {}：{}\n",
                    record.profile.label(),
                    record.human_time.label()
                ));
            }
        }

        if let Some(drill) = &self.drill {
            out.push_str("\n## 附：真实演练记录（4.4 人工耗时 / 5.2 对照）\n\n");
            out.push_str(&format!(
                "- 演练者: {} / 任务类别: {}\n",
                drill.meta.operator, drill.meta.task
            ));
            match &drill.summary {
                Some(summary) => out.push_str(&format!(
                    "- 接管就绪耗时: **{:.1} 分钟**（`last_action` → `handoff_done`，{} ms，来源 `{}`）\n\n",
                    summary.handoff_minutes(),
                    summary.handoff_ms,
                    drill.source_label()
                )),
                None => out.push_str(
                    "- 演练未封存：不能提供人工耗时（先 `bod handoff drill finish`）\n\n",
                ),
            }
            out.push_str(&crate::handoff_drill::arms_markdown(drill));
        }
        out
    }

    pub fn to_json(&self) -> String {
        let claim = self.claim();
        let records: Vec<serde_json::Value> = self
            .records
            .iter()
            .map(|record| {
                serde_json::json!({
                    "adapter_id": record.profile.adapter_id,
                    "adapter_version": record.profile.adapter_version,
                    "runtime": record.profile.runtime,
                    "platform": record.profile.platform,
                    "level": record.level.as_str(),
                    "sample_id": record.sample_id,
                    "outcome": record.outcome.label(),
                    "passed": record.outcome.passed(),
                    "started_at_ms": record.started_at_ms,
                    "elapsed_ms": record.elapsed_ms,
                    "human_time": record.human_time.label(),
                    "human_time_recorded": record.human_time.is_recorded(),
                    "human_time_evidenced": record.human_time.is_evidenced(),
                })
            })
            .collect();
        let value = serde_json::json!({
            "schema": SAMPLE_SCHEMA,
            "sample_id": self.sample_id,
            "level": self.level.as_str(),
            "generated_at_ms": self.generated_at_ms,
            "required_runtimes": REQUIRED_RUNTIMES,
            "claim": {
                "state": claim.state(),
                "passed": claim.passed(),
                "line": claim.line(),
            },
            "human_time_evidenced": self
                .records
                .iter()
                .filter(|item| item.human_time.is_evidenced())
                .count(),
            "records": records,
            "drill": self.drill.as_ref().map(|drill| serde_json::json!({
                "operator": drill.meta.operator,
                "task": drill.meta.task,
                "finished": drill.finished(),
                "handoff_minutes": drill.summary.as_ref().map(|item| item.handoff_minutes()),
                "arms": drill.arms.iter().map(|item| serde_json::json!({
                    "arm": item.arm.as_str(),
                    "ready_seconds": item.metrics.ready_seconds,
                    "interruptions": item.metrics.interruptions,
                    "duplicate_runs": item.metrics.duplicate_runs,
                    "manual_minutes": item.metrics.manual_minutes,
                })).collect::<Vec<_>>(),
            })),
        });
        value.to_string()
    }
}

fn utc_stamp(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant 的 days → civil 算法；不引时区库。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_process::DurableProcessConfig;

    fn temp_root(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("bod-sample-{tag}-{}-{stamp}", std::process::id()))
    }

    fn runtimes() -> Vec<String> {
        detect_runtimes(&["python3", "python", "node"])
    }

    fn durable(root: &Path, exe: &str, at: i64) -> DurableSampleConfig {
        DurableSampleConfig::new(DurableProcessConfig::new(exe, Vec::new(), root), at)
    }

    /// 两件事都要成立才算「双运行时接续」：两个运行时各跑一遍，且各自 L2 通过。
    #[test]
    fn two_real_runtimes_reach_an_evidenced_l2_claim() {
        let available = runtimes();
        if available.len() < REQUIRED_RUNTIMES {
            eprintln!("跳过：本机只有 {} 个可运行运行时", available.len());
            return;
        }
        let base = temp_root("two");
        let mut report = SampleReport::new(HandoffLevel::L2, 1_763_600_000_000);
        for (index, exe) in available.iter().take(REQUIRED_RUNTIMES).enumerate() {
            let root = base.join(format!("rt-{index}"));
            let config = durable(&root, exe, 1_763_600_000_000);
            report.push(run_sample(
                &config,
                HandoffLevel::L2,
                &base.join(format!("ws-{index}")),
                HumanTime::Scripted,
                1_763_600_000_000 + index as i64 * 1000,
            ));
        }
        match report.claim() {
            Claim::Evidenced { runtimes } => assert_eq!(runtimes.len(), REQUIRED_RUNTIMES),
            Claim::Insufficient {
                passed, blockers, ..
            } => panic!(
                "两个真实运行时都应通过 L2，实际 {passed} 个：{blockers:?}\n{}",
                report.to_markdown()
            ),
        }
        for record in &report.records {
            assert!(
                record.outcome.passed(),
                "{} 应通过：{:?}",
                record.profile.label(),
                record.outcome
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// 样本确实验了「接续」而不是「重来」：产出的序号被续跑端回声。
    #[test]
    fn continuation_echoes_the_last_produced_sequence() {
        let available = runtimes();
        let Some(exe) = available.first() else {
            eprintln!("跳过：本机没有可运行运行时");
            return;
        };
        let base = temp_root("echo");
        let config = durable(&base.join("rt"), exe, 1_763_600_100_000);
        let record = run_sample(
            &config,
            HandoffLevel::L2,
            &base.join("ws"),
            HumanTime::Scripted,
            1_763_600_100_000,
        );
        assert!(record.outcome.passed(), "样本应通过：{:?}", record.outcome);
        // 快照里的序号必须等于真实读到的条数，否则续跑端回声的就不是真序号。
        let snapshot =
            fs::read_to_string(base.join("ws").join(SNAPSHOT_FILE)).expect("快照必须落地");
        assert!(
            snapshot.contains("\"last_sequence\":2"),
            "实际快照：{snapshot}"
        );
        assert!(snapshot.contains(TASK_SAMPLE_ID));
        let _ = fs::remove_dir_all(&base);
    }

    /// 不读快照的「续跑」必须被判成没接续，而不是通过。
    #[test]
    fn resume_ignoring_the_snapshot_is_not_continuation() {
        if !is_runnable("python") && !is_runnable("python3") {
            eprintln!("跳过：本机没有 python");
            return;
        }
        struct IgnoresSnapshot {
            config: DurableProcessConfig,
            profile: RuntimeProfile,
        }
        impl SampleConfig for IgnoresSnapshot {
            fn profile(&self) -> RuntimeProfile {
                self.profile.clone()
            }
            fn open(
                &self,
                phase: Phase,
                _snapshot: Option<&Path>,
            ) -> Result<Box<dyn StartTransport>, String> {
                let script = match phase {
                    // 只产出两个检查点，不读快照，直接宣称从 0 开始。
                    Phase::Produce | Phase::Replay => {
                        "import time\nprint('sample: checkpoint 1', flush=True)\nprint('sample: checkpoint 2', flush=True)\ntime.sleep(10)"
                    }
                    Phase::Resume => {
                        "import time\nprint('sample: resume from 0', flush=True)\nprint('sample: checkpoint 3', flush=True)\ntime.sleep(3)"
                    }
                };
                let mut adapter = crate::durable_process::DurableProcessAdapter::new(
                    self.config.clone(),
                    1_763_600_200_000,
                );
                adapter.set_args(
                    vec!["-c".to_string(), script.to_string()],
                    1_763_600_200_000,
                );
                Ok(Box::new(adapter))
            }
        }
        let base = temp_root("ignore");
        let config = IgnoresSnapshot {
            config: DurableProcessConfig::new("python", Vec::new(), base.join("rt")),
            profile: RuntimeProfile {
                adapter_id: "adapter.test".to_string(),
                adapter_version: "1.0.0".to_string(),
                runtime: "python".to_string(),
                platform: "test".to_string(),
            },
        };
        let record = run_sample(
            &config,
            HandoffLevel::L1,
            &base.join("ws"),
            HumanTime::Scripted,
            1_763_600_200_000,
        );
        match &record.outcome {
            SampleOutcome::Failed { step, code, .. } => {
                assert_eq!(*step, SampleStep::Resume);
                assert_eq!(*code, "resume_not_continuing");
            }
            other => panic!("应判成没接续，实际 {other:?}"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// L2 的重放断言不是摆设：新实例读不到旧观察时必须失败。
    #[test]
    fn missing_replay_is_a_failure_not_a_pass() {
        let available = runtimes();
        let Some(exe) = available.first() else {
            eprintln!("跳过：本机没有可运行运行时");
            return;
        };
        struct FreshRootOnReplay {
            inner: DurableSampleConfig,
            outer_root: PathBuf,
        }
        impl SampleConfig for FreshRootOnReplay {
            fn profile(&self) -> RuntimeProfile {
                self.inner.profile()
            }
            fn open(
                &self,
                phase: Phase,
                snapshot: Option<&Path>,
            ) -> Result<Box<dyn StartTransport>, String> {
                if phase == Phase::Replay {
                    // 换一个空根目录：等价于"宿主重启后什么都没有留下"。
                    let mut adapter = crate::durable_process::DurableProcessAdapter::new(
                        DurableProcessConfig::new(
                            self.inner.config.executable.clone(),
                            Vec::new(),
                            &self.outer_root,
                        ),
                        now_ms(),
                    );
                    adapter.set_args(Vec::new(), now_ms());
                    return Ok(Box::new(adapter));
                }
                self.inner.open(phase, snapshot)
            }
        }
        let base = temp_root("noreplay");
        let config = FreshRootOnReplay {
            inner: durable(&base.join("rt"), exe, 1_763_600_300_000),
            outer_root: base.join("empty-root"),
        };
        let record = run_sample(
            &config,
            HandoffLevel::L2,
            &base.join("ws"),
            HumanTime::Scripted,
            1_763_600_300_000,
        );
        match &record.outcome {
            SampleOutcome::Failed { step, code, .. } => {
                assert_eq!(*step, SampleStep::Replay);
                assert_eq!(*code, "replay_unavailable");
            }
            other => panic!("应判重放失败，实际 {other:?}"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// `local-process` 在 L2 上是 `unsupported`（它自己声明的），不是 `fail`（缺陷）。
    #[test]
    fn local_process_is_unsupported_for_l2_and_not_counted_as_a_defect() {
        let available = runtimes();
        let Some(exe) = available.first() else {
            eprintln!("跳过：本机没有可运行运行时");
            return;
        };
        let base = temp_root("local-l2");
        let config = LocalSampleConfig::new(
            crate::local_process::LocalProcessConfig::new(exe, Vec::new()),
            1_763_600_400_000,
        );
        let record = run_sample(
            &config,
            HandoffLevel::L2,
            &base.join("ws"),
            HumanTime::Scripted,
            1_763_600_400_000,
        );
        match &record.outcome {
            SampleOutcome::Unsupported { slot, .. } => assert_eq!(*slot, "reliable_observation"),
            other => panic!("应记 unsupported，实际 {other:?}"),
        }
        // 同一配置在 L1 上能通过：这说明 L2 的 unsupported 来自能力边界，不是它根本跑不起来。
        let record_l1 = run_sample(
            &config,
            HandoffLevel::L1,
            &base.join("ws-l1"),
            HumanTime::Scripted,
            1_763_600_400_500,
        );
        assert!(
            record_l1.outcome.passed(),
            "L1 应通过：{:?}",
            record_l1.outcome
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// 一条通过的记录不足以宣称跨运行时接续。
    #[test]
    fn a_single_runtime_is_insufficient_and_says_why() {
        let mut report = SampleReport::new(HandoffLevel::L2, 0);
        report.push(SampleRecord {
            profile: RuntimeProfile {
                adapter_id: "adapter.x".to_string(),
                adapter_version: "1.0.0".to_string(),
                runtime: "Python 3.13.14".to_string(),
                platform: "windows-x86_64".to_string(),
            },
            sample_id: TASK_SAMPLE_ID.to_string(),
            level: HandoffLevel::L2,
            started_at_ms: 0,
            elapsed_ms: 10,
            outcome: SampleOutcome::Passed { assertions: 13 },
            human_time: HumanTime::Scripted,
        });
        match report.claim() {
            Claim::Insufficient {
                passed,
                required,
                blockers,
            } => {
                assert_eq!((passed, required), (1, 2));
                assert!(
                    blockers
                        .iter()
                        .any(|item| item.contains("不同运行时只有 1 个"))
                );
            }
            other => panic!("应判不足，实际 {other:?}"),
        }
        let markdown = report.to_markdown();
        assert!(
            markdown.contains("接续证据: insufficient (1/2)"),
            "{markdown}"
        );
        assert!(report.to_json().contains("\"state\":\"insufficient\""));
    }

    /// 同一运行时换适配器不算第二个配置：去重按运行时，不按适配器。
    #[test]
    fn same_runtime_with_two_adapters_counts_once() {
        let mut report = SampleReport::new(HandoffLevel::L2, 0);
        for adapter in ["adapter.durable-process", "adapter.local-process"] {
            report.push(SampleRecord {
                profile: RuntimeProfile {
                    adapter_id: adapter.to_string(),
                    adapter_version: "1.0.0".to_string(),
                    runtime: "Python 3.13.14".to_string(),
                    platform: "windows-x86_64".to_string(),
                },
                sample_id: TASK_SAMPLE_ID.to_string(),
                level: HandoffLevel::L2,
                started_at_ms: 0,
                elapsed_ms: 10,
                outcome: SampleOutcome::Passed { assertions: 13 },
                human_time: HumanTime::Scripted,
            });
        }
        assert_eq!(report.claim().passed(), 1, "同一运行时只能算一个");
        assert!(report.claim().line().contains("(1/2)"));
    }

    /// 失败与不适用都要留在报告里，且人工耗时未记录不能被当成 0。
    #[test]
    fn failures_are_kept_and_unrecorded_human_time_is_flagged() {
        let mut report = SampleReport::new(HandoffLevel::L2, 1_763_600_500_000);
        report.push(SampleRecord {
            profile: RuntimeProfile {
                adapter_id: "adapter.broken".to_string(),
                adapter_version: "0.9.0".to_string(),
                runtime: "Python 3.13.14".to_string(),
                platform: "windows-x86_64".to_string(),
            },
            sample_id: TASK_SAMPLE_ID.to_string(),
            level: HandoffLevel::L2,
            started_at_ms: 0,
            elapsed_ms: 40,
            outcome: SampleOutcome::Failed {
                step: SampleStep::Stop,
                code: "stop_not_confirmed",
                detail: "停止只被受理".to_string(),
            },
            human_time: HumanTime::Unrecorded,
        });
        let markdown = report.to_markdown();
        assert!(
            markdown.contains("fail@stop/stop_not_confirmed"),
            "{markdown}"
        );
        assert!(markdown.contains("## 失败路径与不适用"), "{markdown}");
        assert!(markdown.contains("未记录"), "{markdown}");
        assert!(markdown.contains("不能按 0 处理"), "{markdown}");
        assert!(report.to_json().contains("\"human_time_recorded\":false"));
        assert_eq!(report.claim().state(), "insufficient");
    }

    /// L3 不用 L1/L2 的通过冒充：样本没有核验步骤，就报 unsupported。
    #[test]
    fn l3_is_unsupported_because_the_sample_lacks_verification() {
        let available = runtimes();
        let Some(exe) = available.first() else {
            eprintln!("跳过：本机没有可运行运行时");
            return;
        };
        let base = temp_root("l3");
        let config = durable(&base.join("rt"), exe, 1_763_600_600_000);
        let record = run_sample(
            &config,
            HandoffLevel::L3,
            &base.join("ws"),
            HumanTime::Scripted,
            1_763_600_600_000,
        );
        match &record.outcome {
            SampleOutcome::Unsupported { slot, .. } => {
                assert_eq!(*slot, "verified_continuation")
            }
            other => panic!("L3 应记 unsupported，实际 {other:?}"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn human_time_labels_keep_zero_and_unmeasured_apart() {
        assert!(HumanTime::Scripted.label().starts_with('0'));
        assert!(!HumanTime::Scripted.label().contains("未记录"));
        assert_eq!(HumanTime::Unrecorded.label(), "未记录");
        assert!(HumanTime::Scripted.is_recorded());
        assert!(!HumanTime::Unrecorded.is_recorded());
    }

    /// 口述值与演练记录必须能一眼分开：都带数字，但只有后者有出处。
    #[test]
    fn claimed_minutes_never_pass_as_evidence() {
        let drilled = HumanTime::Drilled {
            minutes: 3.4,
            source: "drill-1/drill.summary".to_string(),
        };
        let claimed = HumanTime::Claimed(3.4);
        assert!(drilled.label().contains("3.4"));
        assert!(drilled.label().contains("演练记录"));
        assert!(claimed.label().contains("口述值"));
        assert!(drilled.is_evidenced());
        assert!(!claimed.is_evidenced());
        assert!(claimed.is_recorded(), "口述值也是记录过的，只是不可追溯");
        assert!(
            claimed
                .caveat()
                .is_some_and(|text| text.contains("不能算 4.4"))
        );
        assert!(drilled.caveat().is_none());
        assert!(HumanTime::Unrecorded.caveat().is_some());
    }
}
