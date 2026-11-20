//! P2 任务 4.4 / 5.2：真实演练取样记录。
//!
//! 4.4 要「人工耗时」，5.2 要「与手工交接的同类任务对照」。这两项都只能由人跑出来，
//! 但**「人跑出来的读数」与「人写进去的数字」必须能区分**——否则任何人敲一个
//! `--human-minutes 12` 就能让报告显示实测值，报告失去证据价值。
//!
//! 本模块把秒表交给机器：演练者只在**动作发生时**打一个档位，时间戳由系统读时钟写入
//! `drill.timeline`，人工耗时由两端时间戳**算出来**，不接受外部输入。报告的每一项都能
//! 回到「谁、什么时候、按了哪一档」。
//!
//! 三条边界：
//!
//! - **不猜缺的档位**。缺 `last_action` 或 `handoff_done` 就拒绝给读数，并点名缺哪一档，
//!   不用相邻档位顶替。
//! - **封存后不可改**。`finish` 写 `drill.summary`；之后再打点一律拒绝，避免事后把时间线
//!   拉长到想要的读数。
//! - **对照只列读数**。5.2 明确「不预设提升比例」，因此报告只给两臂原始值、样本数与
//!   未完成项，不写「提升 X%」这类结论。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DRILL_SCHEMA: &str = "bod.handoff.drill/v1";
pub const ARM_SCHEMA: &str = "bod.handoff.drill.arm/v1";
pub const META_FILE: &str = "drill.meta";
pub const TIMELINE_FILE: &str = "drill.timeline";
pub const SUMMARY_FILE: &str = "drill.summary";
pub const ARMS_FILE: &str = "drill.arms";

/// 演练档位。顺序即约束：`last_action` 之前必须有 `task_ready`，
/// `handoff_done` 之前必须有 `last_action`，复核段在交接之后且成对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrillStep {
    /// 任务已就绪（演练起点，不是秒表起点）。
    TaskReady,
    /// 原执行者**最后一次有效动作**：秒表起点。
    LastAction,
    /// 接手者**第一次有效动作**：秒表终点，两者之差即「接管就绪耗时」。
    HandoffDone,
    /// 人工复核开始（可选）。
    VerifyBegin,
    /// 人工复核结束（可选），与 `VerifyBegin` 之差即「人工补充时间」的一部分。
    VerifyEnd,
}

impl DrillStep {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TaskReady => "task_ready",
            Self::LastAction => "last_action",
            Self::HandoffDone => "handoff_done",
            Self::VerifyBegin => "verify_begin",
            Self::VerifyEnd => "verify_end",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "task_ready" => Ok(Self::TaskReady),
            "last_action" => Ok(Self::LastAction),
            "handoff_done" => Ok(Self::HandoffDone),
            "verify_begin" => Ok(Self::VerifyBegin),
            "verify_end" => Ok(Self::VerifyEnd),
            other => Err(format!(
                "未知档位 {other}；只接受 task_ready / last_action / handoff_done / verify_begin / verify_end"
            )),
        }
    }

    /// 同一档位在一次演练里最多出现几次。
    const fn max_occurrences(self) -> usize {
        match self {
            Self::TaskReady | Self::LastAction | Self::HandoffDone => 1,
            Self::VerifyBegin | Self::VerifyEnd => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillMeta {
    pub operator: String,
    pub task: String,
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    pub at_ms: i64,
    pub step: DrillStep,
    pub note: String,
}

/// 封存时写入的汇总：人工耗时**只在这里出现一次**，且由时间戳算出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillSummary {
    pub closed_at_ms: i64,
    pub handoff_ms: i64,
    pub verify_ms: Option<i64>,
    pub entries: usize,
}

impl DrillSummary {
    pub fn handoff_minutes(&self) -> f64 {
        self.handoff_ms as f64 / 60_000.0
    }

    pub fn verify_minutes(&self) -> Option<f64> {
        self.verify_ms.map(|ms| ms as f64 / 60_000.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DrillSession {
    pub root: PathBuf,
    pub meta: DrillMeta,
    pub timeline: Vec<TimelineEntry>,
    pub summary: Option<DrillSummary>,
    pub arms: Vec<ArmRecord>,
}

impl DrillSession {
    pub fn finished(&self) -> bool {
        self.summary.is_some()
    }

    /// 已封存才给读数。缺档位就点名，不拿相近档位顶替。
    pub fn handoff_minutes(&self) -> Result<f64, String> {
        match &self.summary {
            Some(summary) => Ok(summary.handoff_minutes()),
            None => Err(self.missing_hint("无法给出人工耗时")),
        }
    }

    /// 演练时的进度行：只列档位与时刻，供 `start`/`mark` 的短输出用。
    pub fn timeline_line(&self) -> String {
        if self.timeline.is_empty() {
            return "（还没有打点）".to_string();
        }
        self.timeline
            .iter()
            .map(|entry| format!("`{}`@{}", entry.step.as_str(), utc_stamp(entry.at_ms)))
            .collect::<Vec<_>>()
            .join(" → ")
    }

    pub fn source_label(&self) -> String {
        let name = self
            .root
            .file_name()
            .map(|item| item.to_string_lossy().to_string())
            .unwrap_or_else(|| self.root.to_string_lossy().to_string());
        format!("{name}/{SUMMARY_FILE}")
    }

    fn missing_hint(&self, prefix: &str) -> String {
        let has = |step: DrillStep| self.timeline.iter().any(|entry| entry.step == step);
        let mut missing: Vec<&str> = Vec::new();
        for step in [
            DrillStep::TaskReady,
            DrillStep::LastAction,
            DrillStep::HandoffDone,
        ] {
            if !has(step) {
                missing.push(step.as_str());
            }
        }
        if missing.is_empty() {
            format!("{prefix}：缺少 drill.summary，先跑 `bod handoff drill finish --root <目录>`")
        } else {
            format!(
                "{prefix}：缺少档位 {}，先补齐再封存（缺的档位不能由相邻档位顶替）",
                missing.join(", ")
            )
        }
    }
}

/// 对照实验的一侧。5.2 要「手工交接」和「系统交接」做同一类任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    Manual,
    Bod,
}

impl Arm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Bod => "bod",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Manual => "手工交接",
            Self::Bod => "bod 交接",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "manual" | "手工" => Ok(Self::Manual),
            "bod" | "system" | "系统" => Ok(Self::Bod),
            other => Err(format!("未知对照臂 {other}；只接受 manual / bod")),
        }
    }
}

/// 5.2 的四项指标。四项都要，缺一项就是「未完成项」，不能算 0。
#[derive(Debug, Clone, PartialEq)]
pub struct ArmMetrics {
    /// 接管就绪耗时（秒）。
    pub ready_seconds: f64,
    /// 员工中断数。
    pub interruptions: u32,
    /// 重复执行数。
    pub duplicate_runs: u32,
    /// 人工补充时间（分钟）。
    pub manual_minutes: f64,
}

impl ArmMetrics {
    fn validate(&self) -> Result<(), String> {
        if !self.ready_seconds.is_finite() || self.ready_seconds < 0.0 {
            return Err(format!(
                "--ready-seconds 必须是非负有限值，收到 {}",
                self.ready_seconds
            ));
        }
        if !self.manual_minutes.is_finite() || self.manual_minutes < 0.0 {
            return Err(format!(
                "--manual-minutes 必须是非负有限值，收到 {}",
                self.manual_minutes
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArmRecord {
    pub arm: Arm,
    pub metrics: ArmMetrics,
    pub recorded_at_ms: i64,
    pub note: String,
}

// ---------------------------------------------------------------- 读写

fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("建目录 {} 失败: {error}", parent.display()))?;
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, contents).map_err(|error| format!("写 {} 失败: {error}", temp.display()))?;
    fs::rename(&temp, path).map_err(|error| format!("替换 {} 失败: {error}", path.display()))
}

fn meta_path(root: &Path) -> PathBuf {
    root.join(META_FILE)
}

fn timeline_path(root: &Path) -> PathBuf {
    root.join(TIMELINE_FILE)
}

fn summary_path(root: &Path) -> PathBuf {
    root.join(SUMMARY_FILE)
}

fn arms_path(root: &Path) -> PathBuf {
    root.join(ARMS_FILE)
}

fn read_json(path: &Path, what: &str) -> Result<serde_json::Value, String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("读{what} {} 失败: {error}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("解析{what} {} 失败: {error}", path.display()))
}

fn check_schema(value: &serde_json::Value, path: &Path, expected: &str) -> Result<(), String> {
    match value.get("schema").and_then(|item| item.as_str()) {
        Some(schema) if schema == expected => Ok(()),
        Some(other) => Err(format!(
            "{} 的 schema 是 {other}，本版本只认 {expected}",
            path.display()
        )),
        None => Err(format!("{} 缺 schema 字段", path.display())),
    }
}

fn text(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .to_string()
}

fn integer(value: &serde_json::Value, key: &str) -> i64 {
    value.get(key).and_then(|item| item.as_i64()).unwrap_or(0)
}

fn float(value: &serde_json::Value, key: &str) -> f64 {
    value.get(key).and_then(|item| item.as_f64()).unwrap_or(0.0)
}

/// 打开已存在的演练目录。缺 `drill.meta` 说明这不是演练目录。
pub fn load(root: &Path) -> Result<DrillSession, String> {
    let meta_file = meta_path(root);
    if !meta_file.exists() {
        return Err(format!(
            "{} 不是演练目录（缺 {META_FILE}）；先跑 `bod handoff drill start --root <目录>`",
            root.display()
        ));
    }
    let meta_value = read_json(&meta_file, "演练元数据")?;
    check_schema(&meta_value, &meta_file, DRILL_SCHEMA)?;
    let meta = DrillMeta {
        operator: text(&meta_value, "operator"),
        task: text(&meta_value, "task"),
        started_at_ms: integer(&meta_value, "started_at_ms"),
    };

    let mut timeline: Vec<TimelineEntry> = Vec::new();
    let timeline_file = timeline_path(root);
    if timeline_file.exists() {
        let raw = fs::read_to_string(&timeline_file)
            .map_err(|error| format!("读时间线 {} 失败: {error}", timeline_file.display()))?;
        for (index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .map_err(|error| format!("时间线第 {} 行不是合法 JSON: {error}", index + 1))?;
            let step_raw = text(&value, "step");
            let step = DrillStep::parse(&step_raw)
                .map_err(|error| format!("时间线第 {} 行{error}", index + 1))?;
            timeline.push(TimelineEntry {
                at_ms: integer(&value, "at_ms"),
                step,
                note: text(&value, "note"),
            });
        }
    }

    let summary = if summary_path(root).exists() {
        let value = read_json(&summary_path(root), "演练汇总")?;
        check_schema(&value, &summary_path(root), DRILL_SCHEMA)?;
        Some(DrillSummary {
            closed_at_ms: integer(&value, "closed_at_ms"),
            handoff_ms: integer(&value, "handoff_ms"),
            verify_ms: value.get("verify_ms").and_then(|item| item.as_i64()),
            entries: integer(&value, "entries") as usize,
        })
    } else {
        None
    };

    let mut arms: Vec<ArmRecord> = Vec::new();
    let arms_file = arms_path(root);
    if arms_file.exists() {
        let raw = fs::read_to_string(&arms_file)
            .map_err(|error| format!("读对照记录 {} 失败: {error}", arms_file.display()))?;
        for (index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .map_err(|error| format!("对照记录第 {} 行不是合法 JSON: {error}", index + 1))?;
            check_schema(&value, &arms_file, ARM_SCHEMA)?;
            let arm = Arm::parse(&text(&value, "arm"))?;
            arms.push(ArmRecord {
                arm,
                metrics: ArmMetrics {
                    ready_seconds: float(&value, "ready_seconds"),
                    interruptions: integer(&value, "interruptions").max(0) as u32,
                    duplicate_runs: integer(&value, "duplicate_runs").max(0) as u32,
                    manual_minutes: float(&value, "manual_minutes"),
                },
                recorded_at_ms: integer(&value, "recorded_at_ms"),
                note: text(&value, "note"),
            });
        }
    }

    Ok(DrillSession {
        root: root.to_path_buf(),
        meta,
        timeline,
        summary,
        arms,
    })
}

// ---------------------------------------------------------------- 档位约束

/// 校验时间线满足演练协议的顺序。**已存在的时间线也重跑一遍**：磁盘上的记录不能只看追加时
/// 那一次判断，否则手工编辑文件就能绕过。
pub fn validate(timeline: &[TimelineEntry]) -> Result<(), String> {
    let find = |step: DrillStep| timeline.iter().position(|entry| entry.step == step);
    for step in [
        DrillStep::TaskReady,
        DrillStep::LastAction,
        DrillStep::HandoffDone,
        DrillStep::VerifyBegin,
        DrillStep::VerifyEnd,
    ] {
        let count = timeline.iter().filter(|entry| entry.step == step).count();
        if count > step.max_occurrences() {
            return Err(format!(
                "档位 {} 出现 {count} 次，协议要求最多 {} 次：重复打点会让秒表读数失真",
                step.as_str(),
                step.max_occurrences()
            ));
        }
    }
    if timeline.is_empty() {
        return Ok(());
    }
    if timeline[0].step != DrillStep::TaskReady {
        return Err(format!(
            "第一档必须是 task_ready，实际是 {}",
            timeline[0].step.as_str()
        ));
    }
    for window in timeline.windows(2) {
        if window[1].at_ms < window[0].at_ms {
            return Err(format!(
                "时间戳回退：{} 在 {} 之后却更早（{} ms < {} ms）",
                window[1].step.as_str(),
                window[0].step.as_str(),
                window[1].at_ms,
                window[0].at_ms
            ));
        }
    }
    let order = |a: Option<usize>, b: Option<usize>, a_step: DrillStep, b_step: DrillStep| {
        if let (Some(a), Some(b)) = (a, b)
            && a > b
        {
            return Err(format!(
                "{} 必须早于 {}，时间线里反了",
                a_step.as_str(),
                b_step.as_str()
            ));
        }
        Ok(())
    };
    let task_ready = find(DrillStep::TaskReady);
    let last_action = find(DrillStep::LastAction);
    let handoff_done = find(DrillStep::HandoffDone);
    let verify_begin = find(DrillStep::VerifyBegin);
    let verify_end = find(DrillStep::VerifyEnd);
    order(
        task_ready,
        last_action,
        DrillStep::TaskReady,
        DrillStep::LastAction,
    )?;
    order(
        last_action,
        handoff_done,
        DrillStep::LastAction,
        DrillStep::HandoffDone,
    )?;
    order(
        handoff_done,
        verify_begin,
        DrillStep::HandoffDone,
        DrillStep::VerifyBegin,
    )?;
    order(
        verify_begin,
        verify_end,
        DrillStep::VerifyBegin,
        DrillStep::VerifyEnd,
    )?;
    if verify_end.is_some() && verify_begin.is_none() {
        return Err("verify_end 没有配对的 verify_begin".to_string());
    }
    Ok(())
}

/// 封存前的完整性检查：三档齐全才能算一次完整取样。
fn require_complete(timeline: &[TimelineEntry]) -> Result<(), String> {
    validate(timeline)?;
    let has = |step: DrillStep| timeline.iter().any(|entry| entry.step == step);
    let missing: Vec<&str> = [
        DrillStep::TaskReady,
        DrillStep::LastAction,
        DrillStep::HandoffDone,
    ]
    .into_iter()
    .filter(|step| !has(*step))
    .map(|step| step.as_str())
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "缺档位 {}：接管就绪耗时需要 last_action 与 handoff_done 两端，不能用相邻档位顶替",
            missing.join(", ")
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- 操作

pub fn start(root: &Path, operator: &str, task: &str, at_ms: i64) -> Result<DrillSession, String> {
    if meta_path(root).exists() {
        return Err(format!(
            "{} 已是一次演练的记录；要重做请换一个新目录，不要覆盖旧读数",
            root.display()
        ));
    }
    if operator.trim().is_empty() {
        return Err("--operator 不能为空：没有可追溯演练者的记录不算取样".to_string());
    }
    if task.trim().is_empty() {
        return Err("--task 不能为空：对照实验要说明这是哪一类任务".to_string());
    }
    let value = serde_json::json!({
        "schema": DRILL_SCHEMA,
        "operator": operator.trim(),
        "task": task.trim(),
        "started_at_ms": at_ms,
    });
    write_atomic(&meta_path(root), &value.to_string())?;
    load(root)
}

pub fn mark(root: &Path, step: DrillStep, note: &str, at_ms: i64) -> Result<DrillSession, String> {
    let session = load(root)?;
    if session.finished() {
        return Err(format!(
            "演练已封存（{SUMMARY_FILE} 已写），不能再打点：事后补档位等于重写读数，请换新目录重做"
        ));
    }
    let mut timeline = session.timeline.clone();
    if let Some(last) = timeline.last()
        && at_ms < last.at_ms
    {
        return Err(format!(
            "打点时间早于上一档（{} ms < {} ms）：系统时钟回退时不要继续记录，先查时钟",
            at_ms, last.at_ms
        ));
    }
    timeline.push(TimelineEntry {
        at_ms,
        step,
        note: note.trim().to_string(),
    });
    validate(&timeline)?;
    let line = serde_json::json!({
        "at_ms": at_ms,
        "step": step.as_str(),
        "note": note.trim(),
    })
    .to_string();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(timeline_path(root))
        .map_err(|error| format!("打开时间线失败: {error}"))?;
    writeln!(file, "{line}").map_err(|error| format!("追加时间线失败: {error}"))?;
    load(root)
}

pub fn finish(root: &Path, at_ms: i64) -> Result<DrillSession, String> {
    let session = load(root)?;
    if session.finished() {
        return Err("演练已封存，重复 finish 不会改写读数".to_string());
    }
    require_complete(&session.timeline)?;
    let pick = |step: DrillStep| {
        session
            .timeline
            .iter()
            .find(|entry| entry.step == step)
            .map(|entry| entry.at_ms)
    };
    let start_ms = pick(DrillStep::LastAction).ok_or_else(|| "缺 last_action".to_string())?;
    let end_ms = pick(DrillStep::HandoffDone).ok_or_else(|| "缺 handoff_done".to_string())?;
    let verify_ms = match (pick(DrillStep::VerifyBegin), pick(DrillStep::VerifyEnd)) {
        (Some(begin), Some(end)) => Some(end - begin),
        _ => None,
    };
    if at_ms < end_ms {
        return Err(format!(
            "封存时间早于 handoff_done（{at_ms} ms < {end_ms} ms）：先完成接管再封存"
        ));
    }
    let summary = serde_json::json!({
        "schema": DRILL_SCHEMA,
        "closed_at_ms": at_ms,
        "handoff_ms": end_ms - start_ms,
        "verify_ms": verify_ms,
        "entries": session.timeline.len(),
    });
    write_atomic(&summary_path(root), &summary.to_string())?;
    load(root)
}

/// 记录一侧对照实验。四项指标全要，缺项由 CLI 直接拒，不默认成 0。
pub fn record_arm(
    root: &Path,
    arm: Arm,
    metrics: &ArmMetrics,
    note: &str,
    at_ms: i64,
) -> Result<DrillSession, String> {
    let session = load(root)?;
    metrics.validate()?;
    let line = serde_json::json!({
        "schema": ARM_SCHEMA,
        "arm": arm.as_str(),
        "ready_seconds": metrics.ready_seconds,
        "interruptions": metrics.interruptions,
        "duplicate_runs": metrics.duplicate_runs,
        "manual_minutes": metrics.manual_minutes,
        "recorded_at_ms": at_ms,
        "note": note.trim(),
    })
    .to_string();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(arms_path(root))
        .map_err(|error| format!("打开对照记录失败: {error}"))?;
    writeln!(file, "{line}").map_err(|error| format!("追加对照记录失败: {error}"))?;
    let _ = session;
    load(root)
}

// ---------------------------------------------------------------- 报告

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

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 5.2 的「未完成项」清单：某臂零样本、或某项指标缺失时点名。
fn incomplete_items(session: &DrillSession) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for arm in [Arm::Manual, Arm::Bod] {
        let count = session.arms.iter().filter(|item| item.arm == arm).count();
        if count == 0 {
            items.push(format!(
                "{} 臂没有样本（四项指标需两臂各有样本才能对照）",
                arm.label()
            ));
        }
    }
    if session.arms.is_empty() {
        items.push("没有任何对照记录；5.2 需要手工交接与 bod 交接各至少一次".to_string());
    }
    items
}

pub fn to_markdown(session: &DrillSession) -> String {
    let mut out = String::new();
    out.push_str("# 真实演练记录（P2 任务 4.4 / 5.2）\n\n");
    out.push_str(&format!("- schema: {DRILL_SCHEMA}\n"));
    out.push_str(&format!("- 演练者: {}\n", session.meta.operator));
    out.push_str(&format!("- 任务类别: {}\n", session.meta.task));
    out.push_str(&format!(
        "- 开始时间: {}\n",
        utc_stamp(session.meta.started_at_ms)
    ));
    out.push_str(&format!(
        "- 状态: {}\n\n",
        if session.finished() {
            "已封存（drill.summary 已写，时间线不再接受追加）"
        } else {
            "进行中（未封存，读数不成立）"
        }
    ));

    out.push_str("## 时间线（时间戳由系统时钟写入）\n\n");
    if session.timeline.is_empty() {
        out.push_str("- 还没有打点\n\n");
    } else {
        out.push_str("| # | 时刻 | 档位 | 备注 |\n| --- | --- | --- | --- |\n");
        for (index, entry) in session.timeline.iter().enumerate() {
            out.push_str(&format!(
                "| {} | {} | `{}` | {} |\n",
                index + 1,
                utc_stamp(entry.at_ms),
                entry.step.as_str(),
                if entry.note.is_empty() {
                    "—"
                } else {
                    &entry.note
                }
            ));
        }
        out.push('\n');
    }

    out.push_str("## 人工耗时\n\n");
    match &session.summary {
        Some(summary) => {
            out.push_str(&format!(
                "- 接管就绪耗时: **{:.1} 分钟**（`last_action` → `handoff_done`，{} ms，由时间戳算出）\n",
                summary.handoff_minutes(),
                summary.handoff_ms
            ));
            match summary.verify_minutes() {
                Some(minutes) => out.push_str(&format!(
                    "- 人工复核: {minutes:.1} 分钟（`verify_begin` → `verify_end`）\n"
                )),
                None => out.push_str("- 人工复核: 未成对记录复核段（不是 0，是没测到）\n"),
            }
            out.push_str(&format!(
                "- 来源: `{}`（{} 档，封存于 {}）\n\n",
                session.source_label(),
                summary.entries,
                utc_stamp(summary.closed_at_ms)
            ));
        }
        None => {
            out.push_str(&format!(
                "- 未封存：{}\n\n",
                session
                    .handoff_minutes()
                    .err()
                    .unwrap_or_else(|| "缺少 drill.summary".to_string())
            ));
        }
    }

    out.push_str("## 与手工交接的对照（5.2）\n\n");
    out.push_str(&arms_markdown(session));
    out
}

/// 5.2 对照段单独可渲染：接续样本报告要把同一张表贴在自己的输出里，
/// 两处共用一份实现，避免两次手写导致口径漂移。
pub fn arms_markdown(session: &DrillSession) -> String {
    let mut out = String::new();
    if session.arms.is_empty() {
        out.push_str("没有任何对照记录。\n\n");
    } else {
        out.push_str(
            "| 对照臂 | 样本数 | 接管就绪（s） | 员工中断数 | 重复执行数 | 人工补充（min） |\n",
        );
        out.push_str("| --- | --- | --- | --- | --- | --- |\n");
        for arm in [Arm::Manual, Arm::Bod] {
            let rows: Vec<&ArmRecord> =
                session.arms.iter().filter(|item| item.arm == arm).collect();
            if rows.is_empty() {
                out.push_str(&format!("| {} | 0 | — | — | — | — |\n", arm.label()));
                continue;
            }
            let count = rows.len() as f64;
            let ready = rows
                .iter()
                .map(|item| item.metrics.ready_seconds)
                .sum::<f64>()
                / count;
            let interruptions = rows
                .iter()
                .map(|item| item.metrics.interruptions)
                .sum::<u32>();
            let duplicates = rows
                .iter()
                .map(|item| item.metrics.duplicate_runs)
                .sum::<u32>();
            let manual = rows
                .iter()
                .map(|item| item.metrics.manual_minutes)
                .sum::<f64>()
                / count;
            out.push_str(&format!(
                "| {} | {} | {:.1} | {} | {} | {:.1} |\n",
                arm.label(),
                rows.len(),
                ready,
                interruptions,
                duplicates,
                manual
            ));
        }
        out.push('\n');
        out.push_str(
            "口径：多样本时接管就绪与人工补充取均值，中断数与重复执行数取合计；未做归一化。\n",
        );
        out.push_str(
            "**本表只列原始读数**：样本量这么小，任何「提升 X%」的说法都不成立，5.2 也不预设提升比例。\n\n",
        );
    }

    let incomplete = incomplete_items(session);
    if incomplete.is_empty() {
        out.push_str("未完成项: 无（两臂各有样本，四项指标齐全）\n");
    } else {
        out.push_str("未完成项:\n");
        for item in &incomplete {
            out.push_str(&format!("- {item}\n"));
        }
    }
    out
}

pub fn to_json(session: &DrillSession) -> String {
    let timeline: Vec<serde_json::Value> = session
        .timeline
        .iter()
        .map(|entry| {
            serde_json::json!({
                "at_ms": entry.at_ms,
                "step": entry.step.as_str(),
                "note": entry.note,
            })
        })
        .collect();
    let arms: Vec<serde_json::Value> = session
        .arms
        .iter()
        .map(|item| {
            serde_json::json!({
                "arm": item.arm.as_str(),
                "ready_seconds": item.metrics.ready_seconds,
                "interruptions": item.metrics.interruptions,
                "duplicate_runs": item.metrics.duplicate_runs,
                "manual_minutes": item.metrics.manual_minutes,
                "recorded_at_ms": item.recorded_at_ms,
                "note": item.note,
            })
        })
        .collect();
    serde_json::json!({
        "schema": DRILL_SCHEMA,
        "operator": session.meta.operator,
        "task": session.meta.task,
        "started_at_ms": session.meta.started_at_ms,
        "finished": session.finished(),
        "handoff_minutes": session.summary.as_ref().map(DrillSummary::handoff_minutes),
        "verify_minutes": session.summary.as_ref().and_then(DrillSummary::verify_minutes),
        "timeline": timeline,
        "arms": arms,
        "incomplete": incomplete_items(session),
    })
    .to_string()
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("bod-drill-{tag}-{stamp}"))
    }

    fn full_session(root: &Path) -> DrillSession {
        start(root, "张三", "修复一个已知 bug", 1_000).expect("start");
        mark(root, DrillStep::TaskReady, "任务已可执行", 1_100).expect("ready");
        mark(root, DrillStep::LastAction, "原执行者最后一次提交", 60_000).expect("last_action");
        mark(
            root,
            DrillStep::HandoffDone,
            "接手者第一次有效提交",
            264_000,
        )
        .expect("done");
        finish(root, 265_000).expect("finish")
    }

    #[test]
    fn handoff_minutes_are_derived_from_timestamps_not_from_input() {
        let root = temp_root("derive");
        let session = full_session(&root);
        // 60_000 → 264_000 = 204_000 ms = 3.4 分钟。
        let minutes = session.handoff_minutes().expect("本轮已封存");
        assert!((minutes - 3.4).abs() < 1e-9, "实际 {minutes}");
        assert!(session.finished());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_last_action_refuses_to_guess_a_number() {
        let root = temp_root("missing");
        start(&root, "李四", "任务", 1_000).expect("start");
        mark(&root, DrillStep::TaskReady, "", 1_100).expect("ready");
        mark(&root, DrillStep::HandoffDone, "", 5_000).expect("done");
        let error = finish(&root, 6_000).expect_err("缺 last_action 必须拒绝");
        assert!(error.contains("last_action"), "错误应点名缺的档位: {error}");
        let session = load(&root).expect("load");
        let error = session.handoff_minutes().expect_err("未封存不给读数");
        assert!(error.contains("last_action"), "实际 {error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sealed_drill_rejects_further_marks() {
        let root = temp_root("sealed");
        full_session(&root);
        let error = mark(&root, DrillStep::VerifyBegin, "", 300_000).expect_err("封存后必须拒绝");
        assert!(error.contains("已封存"), "实际 {error}");
        let again = finish(&root, 400_000).expect_err("重复封存必须拒绝");
        assert!(again.contains("已封存"), "实际 {again}");
        let session = load(&root).expect("load");
        assert!((session.handoff_minutes().expect("保留原读数") - 3.4).abs() < 1e-9);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn out_of_order_steps_are_rejected() {
        let root = temp_root("order");
        start(&root, "王五", "任务", 1_000).expect("start");
        mark(&root, DrillStep::TaskReady, "", 1_100).expect("ready");
        mark(&root, DrillStep::HandoffDone, "", 2_000).expect("done");
        let error = mark(&root, DrillStep::LastAction, "", 3_000).expect_err("反序必须拒绝");
        assert!(error.contains("必须早于"), "实际 {error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_step_is_rejected_because_it_stretches_the_stopwatch() {
        let root = temp_root("dup");
        start(&root, "赵六", "任务", 1_000).expect("start");
        mark(&root, DrillStep::TaskReady, "", 1_100).expect("ready");
        mark(&root, DrillStep::LastAction, "", 2_000).expect("first");
        let error = mark(&root, DrillStep::LastAction, "", 9_000).expect_err("重复打点必须拒绝");
        assert!(error.contains("最多 1 次"), "实际 {error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn timeline_rollback_is_rejected() {
        let root = temp_root("rollback");
        start(&root, "钱七", "任务", 1_000).expect("start");
        mark(&root, DrillStep::TaskReady, "", 5_000).expect("ready");
        let error = mark(&root, DrillStep::LastAction, "", 4_000).expect_err("时间回退必须拒绝");
        assert!(error.contains("早于上一档"), "实际 {error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_segment_is_optional_but_must_be_paired() {
        let root = temp_root("verify");
        start(&root, "孙八", "任务", 1_000).expect("start");
        mark(&root, DrillStep::TaskReady, "", 1_100).expect("ready");
        mark(&root, DrillStep::LastAction, "", 2_000).expect("last");
        mark(&root, DrillStep::HandoffDone, "", 62_000).expect("done");
        let error = finish(&root, 63_000).expect("无复核段也能封存").finished();
        assert!(error, "封存后应为 finished");
        let session = load(&root).expect("load");
        assert!(
            session
                .summary
                .as_ref()
                .expect("summary")
                .verify_minutes()
                .is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn report_counts_samples_and_names_incomplete_arms() {
        let root = temp_root("arms");
        full_session(&root);
        let metrics = ArmMetrics {
            ready_seconds: 412.0,
            interruptions: 3,
            duplicate_runs: 1,
            manual_minutes: 18.5,
        };
        record_arm(&root, Arm::Manual, &metrics, "手工邮件交接", 300_000).expect("arm");
        let session = load(&root).expect("load");
        let report = to_markdown(&session);
        assert!(
            report.contains("| 手工交接 | 1 | 412.0 | 3 | 1 | 18.5 |"),
            "{report}"
        );
        assert!(report.contains("未完成项"), "{report}");
        assert!(report.contains("bod 交接 臂没有样本"), "{report}");
        assert!(report.contains("不预设提升比例"), "{report}");
        assert!(
            !report.contains("提升 5"),
            "报告不得自行给出提升比例: {report}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn arm_metrics_reject_negative_or_non_finite_values() {
        let root = temp_root("bad-arm");
        full_session(&root);
        let bad = ArmMetrics {
            ready_seconds: f64::NAN,
            interruptions: 0,
            duplicate_runs: 0,
            manual_minutes: 1.0,
        };
        assert!(record_arm(&root, Arm::Bod, &bad, "", 300_000).is_err());
        let negative = ArmMetrics {
            ready_seconds: 1.0,
            interruptions: 0,
            duplicate_runs: 0,
            manual_minutes: -1.0,
        };
        assert!(record_arm(&root, Arm::Bod, &negative, "", 300_000).is_err());
        let session = load(&root).expect("load");
        assert!(session.arms.is_empty(), "非法读数不得落盘");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn start_refuses_to_overwrite_an_existing_drill() {
        let root = temp_root("overwrite");
        full_session(&root);
        let error = start(&root, "别人", "任务", 999).expect_err("不得覆盖");
        assert!(error.contains("换一个新目录"), "实际 {error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn schema_mismatch_is_a_hard_error() {
        let root = temp_root("schema");
        start(&root, "周九", "任务", 1_000).expect("start");
        let path = meta_path(&root);
        fs::write(
            &path,
            r#"{"schema":"bod.handoff.drill/v0","operator":"x","task":"y","started_at_ms":1}"#,
        )
        .expect("write");
        let error = load(&root).expect_err("schema 不符必须报错");
        assert!(error.contains("只认"), "实际 {error}");
        let _ = fs::remove_dir_all(&root);
    }
}
