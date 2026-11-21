//! P3 任务 5.2：把一次真实的共享接口变化，从发现走到再次接受。
//!
//! 与 2.6（G-31，只做影响评估）的分工：2.6 回答"谁可能受影响"，本模块回答
//! "**实际重验了什么、真的跑了几个用例、有没有跑到、漏了谁**"。
//!
//! 三件必须真做的事：
//! 1. **真执行**：重验命令真的 spawn 子进程跑 `cargo test`，退出码与耗时都是真的。
//! 2. **无效重验必须被记成无效**：按模块名过滤时，若一个用例都没跑（`0 passed`），
//!    退出码仍是 0——这种"看起来通过"的记录一律标 [`RunKind::NoTestsExecuted`]，
//!    不写成成功。这是本模块存在的主要理由。
//! 3. **漏项用独立真值核对**：评估用的是静态 import 图，图会漏。这里让 rustc 自己
//!    判断——改变一个文件后看哪些编译单元真的重编了（`cargo check` 的
//!    `compiler-artifact.fresh == false`）。这是与静态图**无关**的第二来源，
//!    两者不一致的地方就是漏项/过度。
//!
//! **本模块不给通过结论**。记录里只有读数、被跳过的东西和未执行项；是否接受由
//! [`reaccept`] 走真实接受流程（证据绑定修订、CAS 指针、自批拒绝），不由本模块判定。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use bod_app::artifact_acceptance;
use bod_app::cas::CasStore;
use bod_app::config::BodConfig;
use bod_app::domain::DomainId;
use bod_app::domain::artifact::ArtifactKind as CoreArtifactKind;
use bod_app::domain::artifact::{
    AcceptanceBinding, AcceptancePolicy, Artifact, ArtifactVisibility, VerificationState,
};
use bod_app::domain::model::{Principal, PrincipalKind};
use bod_app::store::SqliteStore;

use crate::impact_sample::SampleRun;

/// 一次重验最多跑几条命令。超出的进 `dropped`，不静默丢弃。
pub const DEFAULT_BUDGET: usize = 6;

/// 重验的作用域。刻意把它做成枚举而不是几个 bool：
/// `--lib` 与 `--bins` 都**不含** `tests/` 下的集成测试，用它们当"全量"
/// 会留下静默的覆盖洞（本次实测里 `bod` 的 e2e 就是这样被跳过的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// 不带 target 参数：跑该包全部 target（lib + bin + 集成测试）。
    All,
    Lib,
    Bins,
    /// 只跑一个具名集成测试 target。
    Test(String),
}

impl Scope {
    fn args(&self) -> Vec<String> {
        match self {
            Self::All => Vec::new(),
            Self::Lib => vec!["--lib".to_string()],
            Self::Bins => vec!["--bins".to_string()],
            Self::Test(name) => vec!["--test".to_string(), name.clone()],
        }
    }

    fn label(&self) -> String {
        match self {
            Self::All => "全量".to_string(),
            Self::Lib => "--lib".to_string(),
            Self::Bins => "--bins".to_string(),
            Self::Test(name) => format!("--test {name}"),
        }
    }
}

/// 一条重验提案。
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// cargo 包名（`-p` 参数）。
    pub package: String,
    pub scope: Scope,
    /// `Some(过滤串)` 表示在 scope 内再按测试名过滤。
    pub filter: Option<String>,
    /// 触发它的区域（`crate::module`）。
    pub modules: Vec<String>,
    pub reason: String,
}

impl Suggestion {
    pub fn command_args(&self) -> Vec<String> {
        let mut args = vec!["test".to_string(), "-p".to_string(), self.package.clone()];
        args.extend(self.scope.args());
        if let Some(filter) = &self.filter {
            args.push(filter.clone());
        }
        args
    }

    pub fn label(&self) -> String {
        let scope = self.scope.label();
        match &self.filter {
            Some(filter) => format!("{} {scope} {filter}", self.package),
            None => format!("{}（{scope}）", self.package),
        }
    }

    /// 只有"该包全部 target"才算覆盖保证；其余都是定位用。
    pub fn is_selective(&self) -> bool {
        self.scope != Scope::All
    }
}

/// 提案：在预算内可执行的 + 因预算被丢下的。
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<Suggestion>,
    pub dropped: Vec<Suggestion>,
}

impl Plan {
    pub fn selective_count(&self) -> usize {
        self.items.iter().filter(|item| item.is_selective()).count()
    }

    pub fn full_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| !item.is_selective())
            .count()
    }
}

/// 一次重验命令的实际结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// 真的有用例被执行。
    Executed,
    /// 退出码 0，但一个用例都没跑——无效重验。
    NoTestsExecuted,
    /// 用例真的跑了并失败。
    Failed,
    /// 编译/链接失败：命令没跑到用例。这是**工具环境**失败，不是应用失败，
    /// 混在一起会把"本机链接器打不开文件"记成"代码有问题"。
    BuildFailed,
    /// 进程都起不来（cargo 路径错等）。
    NotStarted,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::NoTestsExecuted => "no-tests-executed",
            Self::Failed => "failed",
            Self::BuildFailed => "build-failed",
            Self::NotStarted => "not-started",
        }
    }

    /// 是否算"有效重验"：跑到了用例且没失败。
    pub fn is_effective(self) -> bool {
        self == Self::Executed
    }
}

#[derive(Debug, Clone)]
pub struct TestRun {
    pub suggestion: Suggestion,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub passed: usize,
    pub failed: usize,
    pub filtered_out: usize,
    pub kind: RunKind,
    /// 失败时的输出尾部，便于人工定位；成功时不保留（避免报告膨胀）。
    pub tail: String,
}

/// 漏项核对的结论来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapStatus {
    /// 与 rustc 真值比过。
    Compared,
    /// 没比（并写明为什么）。
    Skipped(String),
}

/// 评估图 vs rustc 真值的差异。
#[derive(Debug, Clone)]
pub struct GapReport {
    pub touched: String,
    /// rustc 真实重编的 crate（去 `-` 归一）。
    pub truth: BTreeSet<String>,
    /// 静态图认为确定受影响的 crate。
    pub predicted: BTreeSet<String>,
    /// 真值有、图里没有 —— 漏项。
    pub missed: BTreeSet<String>,
    /// 图里有、真值没有 —— 过度（声明关系但实际没被用）。
    pub extra: BTreeSet<String>,
    pub status: GapStatus,
    /// 预热那一轮是否真的全 fresh（否则真值不可信）。
    pub warm_note: String,
}

/// 再次接受的结果。刻意保留"谁产出的 / 谁接受的"，因为同一人不得视为独立审批。
#[derive(Debug, Clone)]
pub struct ReacceptRecord {
    pub artifact_id: String,
    pub revision_id: String,
    pub digest: String,
    pub verification_state: String,
    pub outcome: String,
    pub producer: String,
    pub actor: String,
    pub evidence_refs: Vec<String>,
    pub note: String,
}

/// 一次完整 5.2 运行的记录。
pub struct FullRun {
    pub from: String,
    pub to: String,
    pub commit_subject: String,
    pub plan: Plan,
    pub runs: Vec<TestRun>,
    pub gaps: GapReport,
    pub reaccept: Option<ReacceptRecord>,
    pub reaccept_error: Option<String>,
}

// ---------------------------------------------------------------- 提案

/// 按受影响区域生成重验提案：每个受影响 crate 一条全量，变化点所在模块再加一条选择性。
///
/// 顺序刻意是"全量在前"：选择性重验会因模块名与测试名不一致而常常空跑，
/// 全量才是覆盖保证。选择性那几条的价值是**量化**空跑率，不是省时间。
pub fn plan(root: &Path, sample: &SampleRun, budget: usize) -> Plan {
    let mut by_package: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut triggers: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    for (change, assessment) in &sample.assessments {
        let package = package_of(&change.module);
        let entry = by_package.entry(package.clone()).or_default();
        entry.insert(change.module.clone());
        for hit in &assessment.certain {
            entry.insert(hit.region.digest.clone());
        }
        triggers
            .entry(package)
            .or_default()
            .push((change.module.clone(), change.path.clone()));
    }

    let mut wanted: Vec<Suggestion> = Vec::new();
    for (package, modules) in by_package {
        let listed: Vec<String> = modules.iter().cloned().collect();
        // 全量必须不带 target 参数：`--lib` 会静默跳过 tests/ 下的集成测试。
        wanted.push(Suggestion {
            package: package.clone(),
            scope: Scope::All,
            filter: None,
            modules: listed.clone(),
            reason: format!(
                "{} 个区域经显式边判定受影响；跑该包全部 target 以覆盖集成测试",
                listed.len()
            ),
        });

        let Some((module, path)) = pick_trigger(&package, triggers.get(&package)) else {
            continue;
        };
        if is_test_file(&path) {
            // tests/ 下的变化本身就是一个具名 target，用它精确重验。
            let Some(stem) = Path::new(&path).file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            wanted.push(Suggestion {
                package,
                scope: Scope::Test(stem.to_string()),
                filter: None,
                modules: vec![module.clone()],
                reason: "变化点是集成测试文件，直接跑该 test target".to_string(),
            });
            continue;
        }
        let leaf = module.rsplit("::").next().unwrap_or(&module).to_string();
        if leaf == package || leaf == crate_ident(&package) || leaf.is_empty() {
            continue;
        }
        let lib_target = has_lib_target(root, &package);
        wanted.push(Suggestion {
            package,
            scope: if lib_target { Scope::Lib } else { Scope::Bins },
            filter: Some(leaf),
            modules: vec![module],
            reason: "按变化点模块名过滤：用来量化\"名字对不上就空跑\"的无效重验".to_string(),
        });
    }

    let mut plan = Plan::default();
    for suggestion in wanted {
        if plan.items.len() < budget {
            plan.items.push(suggestion);
        } else {
            plan.dropped.push(suggestion);
        }
    }
    plan
}

/// 每个包选一条最具体的变化当选择性提案的触发点：
/// 集成测试文件（自带确定 target）优先，其次非 crate 根的模块，最后才是任意变化。
fn pick_trigger(
    package: &str,
    candidates: Option<&Vec<(String, String)>>,
) -> Option<(String, String)> {
    let list = candidates?;
    let by_test = list.iter().find(|(_, path)| is_test_file(path));
    let by_module = list.iter().find(|(module, _)| module != package);
    by_test
        .or(by_module)
        .or_else(|| list.first())
        .map(|(module, path)| (module.clone(), path.clone()))
}

fn is_test_file(path: &str) -> bool {
    path.contains("/tests/") || path.contains("\\tests\\")
}

fn has_lib_target(root: &Path, package: &str) -> bool {
    root.join("crates")
        .join(package)
        .join("src")
        .join("lib.rs")
        .is_file()
}

/// `bod-app::capacity_evaluation` → `bod-app`。
fn package_of(module: &str) -> String {
    module.split("::").next().unwrap_or(module).to_string()
}

/// `bod-app` → `bod_app`：模块路径里用的是下划线形式。
fn crate_ident(package: &str) -> String {
    package.replace('-', "_")
}

// ---------------------------------------------------------------- 真执行

/// 逐条执行提案。`cargo` 是 cargo 可执行文件路径（Windows 上通常是绝对路径）。
pub fn execute(root: &Path, cargo: &str, plan: &Plan) -> Vec<TestRun> {
    let mut runs = Vec::new();
    for suggestion in &plan.items {
        runs.push(run_one(root, cargo, suggestion.clone()));
    }
    runs
}

fn run_one(root: &Path, cargo: &str, suggestion: Suggestion) -> TestRun {
    let args = suggestion.command_args();
    let command = format!("{cargo} {}", args.join(" "));
    let started = Instant::now();
    let output = Command::new(cargo)
        .arg("test")
        // 测试开启并发即可；不确定性来自测试本身，不来自线程数。
        .args(&args[1..])
        .current_dir(root)
        .output();
    let duration_ms = started.elapsed().as_millis();

    let Ok(output) = output else {
        return TestRun {
            suggestion,
            command,
            exit_code: None,
            duration_ms,
            passed: 0,
            failed: 0,
            filtered_out: 0,
            kind: RunKind::NotStarted,
            tail: "无法启动 cargo（检查 --cargo 路径或 PATH）".to_string(),
        };
    };

    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let (passed, failed, filtered_out) = parse_test_result(&text);
    let exit_code = output.status.code();
    let kind = classify(exit_code, passed, failed, &text);
    let tail = if kind == RunKind::Executed {
        String::new()
    } else {
        tail_of(&text, 12)
    };

    TestRun {
        suggestion,
        command,
        exit_code,
        duration_ms,
        passed,
        failed,
        filtered_out,
        kind,
        tail,
    }
}

/// 退出码 0 但零用例 = 无效重验。这条判定不能放宽：`cargo test` 在过滤后
/// 无匹配时会以 0 退出，与"全部通过"不可区分。
///
/// 非零退出还要再分一层：编译/链接失败时用例根本没开始跑，与"用例失败"是两件事。
fn classify(exit_code: Option<i32>, passed: usize, failed: usize, text: &str) -> RunKind {
    match exit_code {
        None => RunKind::NotStarted,
        Some(0) if failed > 0 => RunKind::Failed,
        Some(0) if passed == 0 => RunKind::NoTestsExecuted,
        Some(0) => RunKind::Executed,
        Some(_) if looks_like_build_failure(text) => RunKind::BuildFailed,
        Some(_) => RunKind::Failed,
    }
}

/// 编译或链接失败的判据。只看 compiler 自己的措辞，不看"error"字样——
/// 测试断言失败也会打印 "error:"，那是应用失败。
fn looks_like_build_failure(text: &str) -> bool {
    text.contains("could not compile")
        || text.contains("error: linking with")
        || text.contains("LNK1104")
        || text.contains("error[E")
}

/// 汇总所有 `test result:` 行。一个 crate 可能有多个测试二进制，逐行累加。
fn parse_test_result(text: &str) -> (usize, usize, usize) {
    let mut passed = 0;
    let mut failed = 0;
    let mut filtered = 0;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("test result:") else {
            continue;
        };
        for part in rest.split(';') {
            let part = part.trim();
            let part = part
                .strip_prefix("ok.")
                .or_else(|| part.strip_prefix("FAILED."))
                .unwrap_or(part)
                .trim();
            if let Some(value) = count_before(part, "passed") {
                passed += value;
            } else if let Some(value) = count_before(part, "failed") {
                failed += value;
            } else if let Some(value) = count_before(part, "filtered out") {
                filtered += value;
            }
        }
    }
    (passed, failed, filtered)
}

fn count_before(part: &str, suffix: &str) -> Option<usize> {
    let value = part.strip_suffix(suffix)?.trim();
    value.rsplit(' ').next()?.parse().ok()
}

fn tail_of(text: &str, lines: usize) -> String {
    let collected: Vec<&str> = text.lines().collect();
    let start = collected.len().saturating_sub(lines);
    collected[start..].join("\n")
}

// ---------------------------------------------------------------- 漏项核对

/// 改变一个真实源文件，让 rustc 自己说出"谁真的受影响"。
///
/// 做法：先 `cargo check` 一次让全部 target 变 fresh，再在文件末尾追加一行注释
/// （内容是真实变化，不是只改 mtime——只改 mtime 时 cargo 会重算 hash 后仍判 fresh），
/// 然后再 check 一次，采集 `fresh == false` 的编译单元。
///
/// 工作树一定会被恢复：`Restore` 的 Drop 负责写回原字节。恢复后再次 check 一次，
/// 让下一轮从干净状态开始（失败只记录，不影响本次真值）。
pub fn compile_truth(root: &Path, cargo: &str, sample: &SampleRun) -> GapReport {
    let predicted = predicted_packages(sample);
    // 选"确定受影响最多"的那个变化做真值试验：它最能暴露图的漏项。
    let picked = sample
        .assessments
        .iter()
        .filter(|(change, _)| change.path.ends_with(".rs"))
        .max_by_key(|(_, assessment)| assessment.certain.len())
        .map(|(change, _)| change)
        .or_else(|| sample.changes.iter().find(|c| c.path.ends_with(".rs")));
    let Some(change) = picked else {
        return GapReport {
            touched: String::new(),
            truth: BTreeSet::new(),
            predicted,
            missed: BTreeSet::new(),
            extra: BTreeSet::new(),
            status: GapStatus::Skipped("本次变化里没有 .rs 文件，无法用 rustc 真值核对".into()),
            warm_note: String::new(),
        };
    };

    let path = root.join(&change.path);
    let Ok(original) = fs::read(&path) else {
        return GapReport {
            touched: change.path.clone(),
            truth: BTreeSet::new(),
            predicted,
            missed: BTreeSet::new(),
            extra: BTreeSet::new(),
            status: GapStatus::Skipped(format!("读不到 {}", change.path)),
            warm_note: String::new(),
        };
    };

    let (warm_dirty, warm_note) = warm_up(root, cargo);

    // 追加一行真实注释：内容变了，rustc 才会重编。
    let mut probe = original.clone();
    if !probe.ends_with(b"\n") {
        probe.push(b'\n');
    }
    probe.extend_from_slice(b"// p3-revalidate-probe: \n");
    let restore = Restore::new(path.clone(), original);
    if fs::write(&path, &probe).is_err() {
        return GapReport {
            touched: change.path.clone(),
            truth: BTreeSet::new(),
            predicted,
            missed: BTreeSet::new(),
            extra: BTreeSet::new(),
            status: GapStatus::Skipped("无法写入探测注释".into()),
            warm_note,
        };
    }

    let raw = check_fresh(root, cargo).unwrap_or_default();
    drop(restore);
    // 恢复后修一次状态；失败只影响下次，不影响本次真值。
    let _ = check_fresh(root, cargo);

    // 预热期就不 fresh 的包是基线噪音（上一次构建残留），不算"这次改动引起的变化"。
    let truth: BTreeSet<String> = raw
        .into_iter()
        .map(|name| normalize(&name))
        .filter(|name| !warm_dirty.contains(name))
        .collect();
    let missed: BTreeSet<String> = truth.difference(&predicted).cloned().collect();
    let extra: BTreeSet<String> = predicted.difference(&truth).cloned().collect();

    GapReport {
        touched: change.path.clone(),
        truth,
        predicted,
        missed,
        extra,
        status: GapStatus::Compared,
        warm_note,
    }
}

struct Restore {
    path: PathBuf,
    original: Vec<u8>,
}
impl Restore {
    fn new(path: PathBuf, original: Vec<u8>) -> Self {
        Self { path, original }
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.original);
    }
}

/// 跑一次 `cargo check --workspace --message-format=json`，返回**没** fresh 的包名。
fn check_fresh(root: &Path, cargo: &str) -> Result<Vec<String>, String> {
    let output = Command::new(cargo)
        .args([
            "check",
            "--workspace",
            "--message-format=json",
            "--quiet",
            "--all-targets",
        ])
        .current_dir(root)
        .output()
        .map_err(|err| format!("无法启动 cargo check：{err}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo check 失败：{}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .last()
                .unwrap_or("")
        ));
    }
    Ok(parse_dirty_packages(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// 预热：让全部 target 变 fresh。返回"预热后仍不 fresh"的包——那是构建残差，
/// 不是这次改动引起的，必须从真值里扣掉，否则漏项数字会凭空变大。
fn warm_up(root: &Path, cargo: &str) -> (BTreeSet<String>, String) {
    let mut note = String::from("预热未执行");
    for attempt in 1..=3 {
        match check_fresh(root, cargo) {
            Ok(dirty) if dirty.is_empty() => {
                note = format!("预热 {attempt} 次后全部 fresh");
                return (BTreeSet::new(), note);
            }
            Ok(dirty) => {
                let normalized: BTreeSet<String> =
                    dirty.iter().map(|name| normalize(name)).collect();
                note = format!(
                    "预热 {attempt} 次后仍有 {} 个包不 fresh（{}），已从真值中扣除",
                    normalized.len(),
                    join_set(&dirty)
                );
                if attempt == 3 {
                    return (normalized, note);
                }
            }
            Err(err) => {
                return (
                    BTreeSet::new(),
                    format!("预热失败：{err}；真值可能包含构建残差"),
                );
            }
        }
    }
    (BTreeSet::new(), note)
}

/// 采集"真的重编了"的**包**。
///
/// 只认 lib/bin 目标：集成测试与 bench 是同一个包里的额外 target，把它们当成
/// 独立编译单元会让"包名"和"测试名"混在一起，漏项数字就成了噪音。
fn parse_dirty_packages(text: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(|v| v.as_str()) != Some("compiler-artifact") {
            continue;
        }
        if value.get("fresh").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let kinds = value
            .get("target")
            .and_then(|target| target.get("kind"))
            .and_then(|kind| kind.as_array());
        let is_crate_level = kinds
            .map(|list| {
                list.iter()
                    .any(|kind| matches!(kind.as_str(), Some("lib") | Some("bin")))
            })
            .unwrap_or(false);
        if !is_crate_level {
            continue;
        }
        let Some(package) = value
            .get("package_id")
            .and_then(|id| id.as_str())
            .and_then(package_name)
        else {
            continue;
        };
        out.insert(package);
    }
    out.into_iter().collect()
}

/// 取包名。cargo 的 `package_id` 有两种尾巴：
/// `path+file:///w/crates/bod-app#bod-app@0.1.0`（重名时带名字）与
/// `path+file:///w/crates/bod-app#0.1.0`（无重名时只有版本）。后者必须回退到路径末段，
/// 否则两个包的版本号会被当成同一个包名合并。
fn package_name(package_id: &str) -> Option<String> {
    let (head, tail) = package_id.split_once('#')?;
    if let Some((name, _)) = tail.split_once('@')
        && !name.is_empty()
    {
        return Some(name.to_string());
    }
    let path = head.rsplit('/').next()?;
    (!path.is_empty()).then(|| path.to_string())
}

/// 静态图认为确定受影响的 crate 集合。
fn predicted_packages(sample: &SampleRun) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (change, assessment) in &sample.assessments {
        out.insert(normalize(&package_of(&change.module)));
        for hit in &assessment.certain {
            out.insert(normalize(&package_of(&hit.region.digest)));
        }
    }
    out
}

/// crate 名在 cargo 里是 `bod_app`，在路径里是 `bod-app`，统一成连字符形式。
fn normalize(name: &str) -> String {
    name.replace('_', "-")
}

fn join_set(items: &[String]) -> String {
    items.join("、")
}

/// 表格单元格里的竖线与换行会破坏 Markdown 结构。
fn escape(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

// ---------------------------------------------------------------- 再次接受

/// 走真实接受流程：登记产物 → 提交候选 → 开始验证 → 结束验证 → 接受。
///
/// 参数里刻意不含"是否通过"：验证结论由 [`runs`] 的实际读数决定——
/// 任何一条重验失败或无效，都只能记 `NeedsRevision`，不能记 `Verified`。
///
/// `operator` 是本机人类主体的标识。`producer` 是执行重验的 Agent 身份。
/// 两者不同才符合"实现者不得自批"，但报告里必须说明：**这仍是同一物理操作者**，
/// 不构成独立第三方审批。
pub fn reaccept(
    root: &Path,
    work_dir: &Path,
    operator: &str,
    producer: &str,
    sample: &SampleRun,
    runs: &[TestRun],
) -> Result<ReacceptRecord, String> {
    fs::create_dir_all(work_dir).map_err(|err| format!("无法创建 {work_dir:?}：{err}"))?;
    let mut store = SqliteStore::open(&work_dir.join("p3-5.2.sqlite"), &BodConfig::default())
        .map_err(|err| format!("无法打开证据库：{err}"))?;
    let cas = CasStore::open(work_dir.join("cas")).map_err(|err| format!("无法打开 CAS：{err}"))?;

    let company = "company:local";
    let artifact_id = DomainId::new("artifact:p3-5.2-revalidation");
    let rules = DomainId::new("rules:p3-impact-static/v1");
    let environment = DomainId::new("env:local-dev-box");
    let producer_id = DomainId::new(producer);
    let now = now_ms();

    let artifact = Artifact {
        id: artifact_id.clone(),
        company_id: DomainId::new(company),
        project_id: DomainId::new("project:bod"),
        kind: CoreArtifactKind::StageBundle,
        title: "P3 5.2 共享接口变化重验记录".to_string(),
        owner: DomainId::new(operator),
        visibility: ArtifactVisibility::Internal,
        accepted_revision: None,
    };
    artifact_acceptance::ensure_artifact(&mut store, &artifact, now)
        .map_err(|err| format!("登记产物失败：{err}"))?;

    let revision_id = DomainId::new(format!("rev:p3-5.2:{}:{now}", short(&sample.to)));
    let evidence = evidence_document(sample, runs);
    let candidate = artifact_acceptance::submit_candidate(
        &mut store,
        &cas,
        company,
        &artifact_id,
        &revision_id,
        None,
        evidence.as_bytes(),
        "text/markdown",
        &producer_id,
        &rules,
        &environment,
        now,
    )
    .map_err(|err| format!("提交候选失败：{err}"))?;

    let operator_id = DomainId::new(operator);
    artifact_acceptance::begin_verification(
        &mut store,
        company,
        &revision_id,
        &producer_id,
        &rules,
        &environment,
        now,
    )
    .map_err(|err| format!("开始验证失败：{err}"))?;

    let (state, reason) = verification_verdict(runs);
    let refs = evidence_refs(runs);
    artifact_acceptance::complete_verification(
        &mut store,
        company,
        &revision_id,
        state,
        &producer_id,
        &reason,
        &rules,
        &environment,
        refs.clone(),
        now,
    )
    .map_err(|err| format!("结束验证失败：{err}"))?;

    // 验证没过就不该进接受环节：调用 accept 只会拿到 `NotVerified` 错误，
    // 那会把"流程正确拒绝"显示成"工具出错"。这里显式停在验证结论上。
    if state != VerificationState::Verified {
        return Ok(ReacceptRecord {
            artifact_id: artifact_id.as_str().to_string(),
            revision_id: revision_id.as_str().to_string(),
            digest: candidate.digest.value.clone(),
            verification_state: state.as_str().to_string(),
            outcome: "refused-not-verified".to_string(),
            producer: producer.to_string(),
            actor: operator.to_string(),
            evidence_refs: refs,
            note: format!(
                "验证结论为 {}（{}），因此未进入接受环节；候选与验证记录已留存可复核。root={}",
                state.as_str(),
                reason,
                root.display()
            ),
        });
    }

    // 接受是独立一步：状态 Verified 也不代表已被接受。
    let binding = AcceptanceBinding {
        work_package_revision: DomainId::new(format!("wp:p3-5.2:{}", short(&sample.to))),
        input_baseline: DomainId::new(format!("baseline:{}", short(&sample.from))),
        artifact_revision: revision_id.clone(),
        verification_rules: rules.clone(),
        target_environment: environment.clone(),
        merge_combination: None,
    };
    let request = artifact_acceptance::AcceptRequest {
        acceptance_id: DomainId::new(format!("acc:p3-5.2:{}:{now}", short(&sample.to))),
        binding,
        checks_passed: state == VerificationState::Verified,
        blocking_risks: blocking_risks(runs),
        reason: format!("{reason}；接受人 {operator}"),
        evidence_refs: refs.clone(),
    };
    let policy = AcceptancePolicy {
        // 不开启自动接受：本次运行的意义就是看真实证据够不够，不是看策略能不能放宽。
        auto_accept_enabled: false,
        required_checks: 0,
        high_risk: false,
        production_write: false,
    };
    let actor = Principal {
        id: operator_id,
        kind: PrincipalKind::User,
        claimed_roles: vec!["operator".to_string()],
    };
    let outcome = artifact_acceptance::accept(
        &mut store,
        company,
        &artifact_id,
        &request,
        &actor,
        &policy,
        now,
    )
    .map_err(|err| format!("接受失败：{err}"))?;

    Ok(ReacceptRecord {
        artifact_id: artifact_id.as_str().to_string(),
        revision_id: revision_id.as_str().to_string(),
        digest: candidate.digest.value.clone(),
        verification_state: state.as_str().to_string(),
        outcome: outcome_str(&outcome),
        producer: producer.to_string(),
        actor: operator.to_string(),
        evidence_refs: refs,
        note: format!(
            "root={}；producer 与 actor 不同以避开自批，但仍是同一物理操作者，不构成独立第三方审批",
            root.display()
        ),
    })
}

fn outcome_str(outcome: &artifact_acceptance::AcceptOutcome) -> String {
    match outcome {
        artifact_acceptance::AcceptOutcome::Accepted(_) => "accepted",
        artifact_acceptance::AcceptOutcome::AutoAccepted(_) => "auto-accepted",
        artifact_acceptance::AcceptOutcome::RequiresHuman => "requires-human",
    }
    .to_string()
}

/// 验证结论按"全量是覆盖保证、选择性是定位信息"分开判：
///
/// - 任一命令失败 → `NeedsRevision`（真的有问题）；
/// - 没有任何**全量**重验真正执行 → `NeedsRevision`（覆盖有洞）；
/// - **选择性**空跑只记录、不阻断：它说明"按模块名定位不到测试"，而全量已经兜住了。
///   若把它也算作验证失败，一个过滤串写错就会让整个流程永远无法接受，
///   等于把工具缺陷变成系统阻塞。
fn verification_verdict(runs: &[TestRun]) -> (VerificationState, String) {
    let test_failed = runs
        .iter()
        .filter(|run| run.kind == RunKind::Failed)
        .count();
    let build_failed = runs
        .iter()
        .filter(|run| run.kind == RunKind::BuildFailed)
        .count();
    let passed: usize = runs.iter().map(|run| run.passed).sum();

    let full: Vec<&TestRun> = runs
        .iter()
        .filter(|run| !run.suggestion.is_selective())
        .collect();
    let full_effective = full.iter().filter(|run| run.kind.is_effective()).count();
    let full_broken = full.iter().filter(|run| !run.kind.is_effective()).count();
    let selective_empty = runs
        .iter()
        .filter(|run| run.suggestion.is_selective() && run.kind == RunKind::NoTestsExecuted)
        .count();

    if test_failed > 0 {
        return (
            VerificationState::NeedsRevision,
            format!("{test_failed} 条重验有用例失败、{passed} 个通过，需修改后重验"),
        );
    }
    if build_failed > 0 {
        return (
            VerificationState::NeedsRevision,
            format!(
                "{build_failed} 条重验编译或链接失败，没用例被执行——这是工具环境失败，不是用例失败，但同样没有验证任何东西"
            ),
        );
    }
    if full_effective == 0 {
        return (
            VerificationState::NeedsRevision,
            format!("没有一条全量重验真正执行（{full_broken} 条全量未产出用例），不能算通过"),
        );
    }
    if full_broken > 0 {
        return (
            VerificationState::NeedsRevision,
            format!("{full_broken} 条全量重验没有真正执行，覆盖不完整"),
        );
    }
    let note = if selective_empty > 0 {
        format!(
            "{full_effective} 条全量重验执行 {passed} 个用例并全部通过；另有 {selective_empty} 条选择性重验空跑（已记录，不影响覆盖判定）"
        )
    } else {
        format!("{full_effective} 条重验执行 {passed} 个用例并全部通过")
    };
    (VerificationState::Verified, note)
}

/// 阻断接受的只有"全量覆盖"层面的问题；选择性空跑进证据引用，不进阻断项。
fn blocking_risks(runs: &[TestRun]) -> Vec<String> {
    let mut risks = Vec::new();
    for run in runs {
        if run.suggestion.is_selective() {
            continue;
        }
        match run.kind {
            RunKind::Failed => risks.push(format!(
                "重验失败：{}（exit={:?}，{} 失败）",
                run.suggestion.label(),
                run.exit_code,
                run.failed
            )),
            RunKind::BuildFailed => risks.push(format!(
                "构建/链接失败：{}（exit={:?}，命令未跑到用例，属工具环境失败）",
                run.suggestion.label(),
                run.exit_code
            )),
            RunKind::NoTestsExecuted => risks.push(format!(
                "无效重验：{} 退出码 0 但 0 个用例被执行（过滤掉 {}）",
                run.suggestion.label(),
                run.filtered_out
            )),
            RunKind::NotStarted => risks.push(format!(
                "未能执行：{}（{}）",
                run.suggestion.label(),
                run.tail
            )),
            RunKind::Executed => {}
        }
    }
    risks
}

fn evidence_refs(runs: &[TestRun]) -> Vec<String> {
    runs.iter()
        .map(|run| {
            format!(
                "{} → {}（{} passed / {} failed / {} filtered）",
                run.suggestion.label(),
                run.kind.as_str(),
                run.passed,
                run.failed,
                run.filtered_out
            )
        })
        .collect()
}

/// 进入 CAS 的证据正文：只写读数和未执行项，不写结论。
fn evidence_document(sample: &SampleRun, runs: &[TestRun]) -> String {
    let mut out = String::new();
    out.push_str("# P3 5.2 重验证据\n\n");
    out.push_str(&format!(
        "提交：`{}..{}` {}（{} 处接口变化）\n\n",
        short(&sample.from),
        short(&sample.to),
        sample.commit_subject,
        sample.changes.len()
    ));
    out.push_str("| 命令 | 退出码 | 执行 | 通过 | 失败 | 过滤 | 耗时 |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for run in runs {
        out.push_str(&format!(
            "| `{}` | {:?} | {} | {} | {} | {} | {} ms |\n",
            run.suggestion.label(),
            run.exit_code,
            run.kind.as_str(),
            run.passed,
            run.failed,
            run.filtered_out,
            run.duration_ms
        ));
    }
    out.push_str("\n结论：本文件只含读数与命令事实，不含\"是否可接受\"的判断。\n");
    out
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- 汇总与渲染

/// 跑完整 5.2：提案 → 执行 → 漏项核对 → 再次接受。
pub fn full(
    root: &Path,
    cargo: &str,
    work_dir: &Path,
    operator: &str,
    producer: &str,
    sample: SampleRun,
    budget: usize,
) -> FullRun {
    let plan = plan(root, &sample, budget);
    let runs = execute(root, cargo, &plan);
    let gaps = compile_truth(root, cargo, &sample);
    let (reaccept, reaccept_error) =
        match reaccept(root, work_dir, operator, producer, &sample, &runs) {
            Ok(record) => (Some(record), None),
            Err(err) => (None, Some(err)),
        };
    FullRun {
        from: sample.from.clone(),
        to: sample.to.clone(),
        commit_subject: sample.commit_subject.clone(),
        plan,
        runs,
        gaps,
        reaccept,
        reaccept_error,
    }
}

/// 渲染报告。结论行固定为"未结论"，与 drill 工具包同一纪律。
pub fn render(run: &FullRun, rule_version: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# P3 5.2 共享接口变化：从发现到再次接受\n\n提交 `{}..{}` {} · 规则 `{rule_version}`\n\n",
        short(&run.from),
        short(&run.to),
        run.commit_subject
    ));

    out.push_str("## 1. 重验提案（范围与预算）\n\n");
    out.push_str(&format!(
        "预算内 {} 条（选择性 {} / 全量 {}），因预算丢弃 {} 条。\n\n",
        run.plan.items.len(),
        run.plan.selective_count(),
        run.plan.full_count(),
        run.plan.dropped.len()
    ));
    if !run.plan.items.is_empty() {
        out.push_str("| 提案 | 覆盖区域 | 理由 |\n| --- | --- | --- |\n");
        for item in &run.plan.items {
            // 覆盖区域里混着"依赖它的别的 crate"，展示本包自己的第一个模块更可读。
            let own = item
                .modules
                .iter()
                .find(|module| module.starts_with(&format!("{}::", item.package)))
                .or_else(|| item.modules.first());
            out.push_str(&format!(
                "| `{}` | {} 个，含 `{}` | {} |\n",
                item.label(),
                item.modules.len(),
                own.cloned().unwrap_or_default(),
                escape(&item.reason)
            ));
        }
        out.push('\n');
    }
    if !run.plan.dropped.is_empty() {
        out.push_str("| 被丢弃的提案 | 触发区域 |\n| --- | --- |\n");
        for item in &run.plan.dropped {
            out.push_str(&format!(
                "| `{}` | {} |\n",
                item.label(),
                item.modules.join("、")
            ));
        }
        out.push_str("\n**这些区域没有重验**。预算是我设的上限，不是\"不需要\"。\n\n");
    }

    out.push_str("## 2. 实际执行（读数为真）\n\n");
    out.push_str("| 命令 | 退出码 | 判定 | 通过 | 失败 | 过滤 | 耗时 |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for item in &run.runs {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} ms |\n",
            item.suggestion.label(),
            item.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "—".to_string()),
            item.kind.as_str(),
            item.passed,
            item.failed,
            item.filtered_out,
            item.duration_ms
        ));
    }
    out.push('\n');

    let empty: Vec<&TestRun> = run
        .runs
        .iter()
        .filter(|item| item.kind == RunKind::NoTestsExecuted)
        .collect();
    if !empty.is_empty() {
        out.push_str(&format!(
            "**无效重验 {} 条**：退出码 0 但一个用例都没跑。这种记录如果当成\"通过\"，\n就是最典型的假绿——命令成功、什么都没验证。\n\n",
            empty.len()
        ));
        for item in empty {
            out.push_str(&format!(
                "- `{}`：过滤掉 {} 个用例后剩 0 个\n",
                item.suggestion.label(),
                item.filtered_out
            ));
        }
        out.push('\n');
    }

    let failed_runs: Vec<&TestRun> = run
        .runs
        .iter()
        .filter(|item| {
            matches!(
                item.kind,
                RunKind::Failed | RunKind::BuildFailed | RunKind::NotStarted
            )
        })
        .collect();
    if !failed_runs.is_empty() {
        let build_only = failed_runs
            .iter()
            .all(|item| item.kind == RunKind::BuildFailed);
        out.push_str("### 失败与未启动的命令（原始输出尾部）\n\n");
        if build_only {
            out.push_str(
                "**全部是编译/链接失败，不是用例失败**：命令没跑到测试，所以既没证明通过，\n也没证明有问题。这类失败属于工具环境（本机链接器、文件锁、依赖缺失），\n不计入应用缺陷，但也不能当成\"已验证\"。\n\n",
            );
        }
        for item in failed_runs {
            out.push_str(&format!(
                "`{}`（exit={}）：\n\n```\n{}\n```\n\n",
                item.command,
                item.exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "—".to_string()),
                item.tail
            ));
        }
    }

    out.push_str("## 3. 漏项核对（rustc 真值 vs 静态图）\n\n");
    match &run.gaps.status {
        GapStatus::Skipped(reason) => {
            out.push_str(&format!("未比对：{reason}\n\n"));
        }
        GapStatus::Compared => {
            out.push_str(&format!(
                "改变 `{}` 后，rustc 真的重编了 {} 个编译单元；静态图预测 {} 个。\n\n",
                run.gaps.touched,
                run.gaps.truth.len(),
                run.gaps.predicted.len()
            ));
            out.push_str(&format!("预热状态：{}\n\n", run.gaps.warm_note));
            if run.gaps.missed.is_empty() {
                out.push_str("**漏项：无**——rustc 重编的每个 crate 都在静态图的确定集里。\n\n");
            } else {
                out.push_str(&format!(
                    "**漏项 {} 个**（rustc 重编了、静态图没预测到）：{}\n\n",
                    run.gaps.missed.len(),
                    join_set(&run.gaps.missed.iter().cloned().collect::<Vec<_>>())
                ));
            }
            if !run.gaps.extra.is_empty() {
                out.push_str(&format!(
                    "过度预测 {} 个（图里有边、rustc 没重编）：{}。这通常是\"只用了类型\"或\n声明了依赖但未真正引用，不构成错误，但会让人误判影响面。\n\n",
                    run.gaps.extra.len(),
                    join_set(&run.gaps.extra.iter().cloned().collect::<Vec<_>>())
                ));
            }
        }
    }

    out.push_str("## 4. 再次接受（真实接受流程）\n\n");
    match (&run.reaccept, &run.reaccept_error) {
        (Some(record), _) => {
            out.push_str(&format!(
                "| 项 | 值 |\n| --- | --- |\n| 产物 | `{}` |\n| 修订 | `{}` |\n| 内容摘要 | `{}` |\n| 验证结论 | {} |\n| 接受结果 | {} |\n| 产出者 | `{}` |\n| 接受人 | `{}` |\n\n",
                record.artifact_id,
                record.revision_id,
                record.digest,
                record.verification_state,
                record.outcome,
                record.producer,
                record.actor
            ));
            out.push_str(&format!("{}\n\n", record.note));
            out.push_str("证据引用（每条重验的实际读数）：\n\n");
            for reference in &record.evidence_refs {
                out.push_str(&format!("- {}\n", escape(reference)));
            }
            out.push('\n');
            if record.verification_state != "verified" {
                out.push_str(
                    "**验证结论不是 verified，因此本次接受被真实拒绝或需人工**。这是流程在起作用，\n不是故障——把它改写成\"通过\"才是故障。\n\n",
                );
            }
        }
        (None, Some(err)) => {
            out.push_str(&format!("再次接受未能执行：{err}\n\n"));
        }
        (None, None) => {}
    }

    out.push_str("## 5. 未执行项\n\n");
    out.push_str("- 只跑 `--lib` 单元测试：集成测试（`tests/`）与端到端用例不在本次重验范围内。\n");
    out.push_str("- 静态图会漏 `use super::`、花括号多导入、`pub use` 重导出、宏展开与动态派发；\n  第 3 节的 rustc 真值只能校正 crate 粒度的漏项，模块粒度的漏项仍未测。\n");
    out.push_str("- 只覆盖一个真实提交对；`a9c80d5`、`157298b` 各跑一次才算两个样本。\n");
    out.push_str("- 没有第三方评审：producer 与 actor 不同，但都是同一物理操作者。\n\n");
    out.push_str("## 结论\n\n");
    out.push_str("未结论：本记录只含读数与观察，\"是否可接受\"由评审给出。\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_tests_with_exit_zero_is_never_a_pass() {
        assert_eq!(classify(Some(0), 0, 0, ""), RunKind::NoTestsExecuted);
        assert_eq!(classify(Some(0), 3, 0, ""), RunKind::Executed);
        assert_eq!(classify(Some(0), 2, 1, ""), RunKind::Failed);
        assert_eq!(classify(Some(101), 0, 0, "boom"), RunKind::Failed);
        assert_eq!(classify(None, 0, 0, ""), RunKind::NotStarted);
        assert!(!RunKind::NoTestsExecuted.is_effective());
    }

    #[test]
    fn link_failure_is_not_reported_as_a_test_failure() {
        let link = "error: linking with `link.exe` failed: exit code: 1104\n  = note: LNK1104: 无法打开文件";
        assert_eq!(classify(Some(101), 0, 0, link), RunKind::BuildFailed);
        assert_eq!(
            classify(Some(101), 0, 0, "error: could not compile `bod-app`"),
            RunKind::BuildFailed
        );
        // 断言失败打的是 test result: FAILED，不能算构建失败。
        let assertion = "test result: FAILED. 0 passed; 1 failed; 0 ignored";
        assert_eq!(classify(Some(101), 0, 1, assertion), RunKind::Failed);
    }

    #[test]
    fn build_failure_blocks_but_is_named_differently() {
        let runs = vec![fake_run(RunKind::BuildFailed, 0, 0, 0, false)];
        let (state, reason) = verification_verdict(&runs);
        assert_eq!(state, VerificationState::NeedsRevision);
        assert!(
            reason.contains("工具环境失败"),
            "构建失败必须与应用失败区分：{reason}"
        );
    }

    #[test]
    fn test_result_lines_are_summed_across_binaries() {
        let text = "\
running 4 tests
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.01s
running 1 test
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
";
        assert_eq!(parse_test_result(text), (4, 1, 2));
    }

    #[test]
    fn filtered_out_only_is_detected_as_empty_run() {
        let text = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 128 filtered out\n";
        let (passed, failed, filtered) = parse_test_result(text);
        assert_eq!((passed, failed, filtered), (0, 0, 128));
        assert_eq!(
            classify(Some(0), passed, failed, text),
            RunKind::NoTestsExecuted
        );
    }

    #[test]
    fn dirty_packages_exclude_fresh_and_test_targets() {
        let text = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"bod_app","kind":["lib"]},"package_id":"path+file:///w/crates/bod-app#bod-app@0.1.0","fresh":false}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"bod_core","kind":["lib"]},"package_id":"path+file:///w/crates/bod-core#bod-core@0.1.0","fresh":true}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"p3_impact","kind":["test"]},"package_id":"path+file:///w/crates/bod-app#bod-app@0.1.0","fresh":false}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n"
        );
        // 集成测试 target 与 lib 属同一包，只记一次包名；fresh 的不记。
        assert_eq!(parse_dirty_packages(text), vec!["bod-app".to_string()]);
    }

    #[test]
    fn package_name_is_taken_from_package_id_not_target_name() {
        assert_eq!(
            package_name("path+file:///w/crates/bod-app#bod-app@0.1.0"),
            Some("bod-app".to_string())
        );
        assert_eq!(
            package_name("registry+https://x/y#serde@1.0"),
            Some("serde".to_string())
        );
        // cargo 在无重名时只写版本号：必须回退到路径末段，否则两个包会被合并成 "0.1.0"。
        assert_eq!(
            package_name("path+file:///E:/projects/bod/crates/bod-app#0.1.0"),
            Some("bod-app".to_string())
        );
        assert_eq!(
            package_name("path+file:///E:/projects/bod/crates/bod-core#0.1.0"),
            Some("bod-core".to_string())
        );
        assert_eq!(package_name("no-hash-here"), None);
    }

    #[test]
    fn full_run_gaps_block_verification_but_selective_empty_does_not() {
        // 全量空跑 = 覆盖有洞，必须挡住。
        let runs = vec![fake_run(RunKind::NoTestsExecuted, 0, 0, 128, false)];
        let (state, reason) = verification_verdict(&runs);
        assert_eq!(state, VerificationState::NeedsRevision, "{reason}");

        // 全量真的跑了 → 可 verified。
        let runs = vec![fake_run(RunKind::Executed, 12, 0, 0, false)];
        assert_eq!(verification_verdict(&runs).0, VerificationState::Verified);

        // 只有选择性，没有全量 → 不算覆盖。
        let runs = vec![fake_run(RunKind::Executed, 12, 0, 0, true)];
        assert_eq!(
            verification_verdict(&runs).0,
            VerificationState::NeedsRevision,
            "选择性重验不能独自撑起覆盖"
        );

        // 全量通过 + 选择性空跑：空跑被记录，但不否掉已跑通的全量覆盖。
        let runs = vec![
            fake_run(RunKind::Executed, 12, 0, 0, false),
            fake_run(RunKind::NoTestsExecuted, 0, 0, 40, true),
        ];
        let (state, note) = verification_verdict(&runs);
        assert_eq!(state, VerificationState::Verified, "{note}");
        assert!(note.contains("选择性重验空跑"), "{note}");
        assert!(
            blocking_risks(&runs).is_empty(),
            "选择性空跑不能成为阻断接受的理由"
        );

        // 失败一律挡住，与选择性无关。
        let runs = vec![fake_run(RunKind::Failed, 0, 1, 0, false)];
        assert_eq!(
            verification_verdict(&runs).0,
            VerificationState::NeedsRevision
        );
    }

    #[test]
    fn dropped_suggestions_are_reported_not_silently_cut() {
        let sample = sample_run_for_tests();
        let full = plan(&crate::workspace_root(), &sample, usize::MAX);
        assert!(full.dropped.is_empty());
        assert!(
            full.items.len() >= 2,
            "至少应有全量与选择性两类提案：{}",
            full.items.len()
        );

        let cut = plan(&crate::workspace_root(), &sample, 1);
        assert_eq!(cut.items.len(), 1);
        assert_eq!(
            cut.items.len() + cut.dropped.len(),
            full.items.len(),
            "被预算裁掉的提案必须留在 dropped 里，不能凭空消失"
        );
    }

    #[test]
    fn plan_prefers_full_run_before_selective_filter() {
        let sample = sample_run_for_tests();
        let full = plan(&crate::workspace_root(), &sample, usize::MAX);
        let first = full.items.first().expect("应有提案");
        assert!(
            !first.is_selective(),
            "全量必须排在选择性之前：选择性会空跑，不能当覆盖保证"
        );
    }

    fn fake_run(
        kind: RunKind,
        passed: usize,
        failed: usize,
        filtered: usize,
        selective: bool,
    ) -> TestRun {
        TestRun {
            suggestion: Suggestion {
                package: "bod-app".to_string(),
                scope: if selective { Scope::Lib } else { Scope::All },
                filter: selective.then(|| "capacity_evaluation".to_string()),
                modules: vec!["bod-app::capacity_evaluation".to_string()],
                reason: "测试用".to_string(),
            },
            command: "cargo test -p bod-app --lib capacity_evaluation".to_string(),
            exit_code: Some(0),
            duration_ms: 1,
            passed,
            failed,
            filtered_out: filtered,
            kind,
            tail: String::new(),
        }
    }

    /// 构造一个**不依赖仓库历史**的样本。
    ///
    /// 原先用 `HEAD~1..HEAD` 作样本，测试因此隐含依赖「最新一次提交恰好改了 `crates/`
    /// 下的文件」。`impact_sample::region_of` 只承认 `crates/` 下的路径，所以当 HEAD
    /// 落在只改 `xtask/` 或 `apps/` 的提交上时提案数为 0，测试就变红——那是测试对仓库
    /// 历史的隐式依赖，不是被验代码的缺陷（2026-11-21 由 `8d24199` 暴露）。
    ///
    /// `plan` 要验的是排序与预算截断，本就不该读外部状态；真实 git 链路
    /// 由 `xtask impact` / `xtask revalidate` 的实跑覆盖。
    fn sample_run_for_tests() -> SampleRun {
        use bod_app::impact_revalidation::{
            ArtifactKind as ImpactKind, AssessmentInput, ChangeKind, ChangeTrigger, RevisionId,
            assess,
        };
        use crate::impact_sample::{ChangedRegion, ScanStats};

        let pairs = [
            (
                "bod-app::capacity_evaluation",
                "crates/bod-app/src/capacity_evaluation.rs",
            ),
            ("bod-core::model", "crates/bod-core/src/model.rs"),
        ];
        let changes: Vec<ChangedRegion> = pairs
            .iter()
            .map(|(module, path)| ChangedRegion {
                region: RevisionId::new(ImpactKind::Source, module),
                module: (*module).to_string(),
                path: (*path).to_string(),
                kind: ChangeKind::Source,
                public_items: Vec::new(),
            })
            .collect();
        let count = changes.len();
        let known: Vec<RevisionId> = changes.iter().map(|change| change.region.clone()).collect();
        let assessments = changes
            .iter()
            .map(|change| {
                let trigger = ChangeTrigger {
                    change_id: format!("synthetic:{}", change.module),
                    revision: change.region.clone(),
                    kind: change.kind,
                    description: "合成样本".to_string(),
                };
                let input = AssessmentInput {
                    old_baseline: "aaa",
                    new_baseline: "bbb",
                    rule_version: "test",
                    known_regions: &known,
                    edges: &[],
                };
                (change.clone(), assess(&trigger, &input, 0))
            })
            .collect();
        SampleRun {
            from: "aaa".to_string(),
            to: "bbb".to_string(),
            commit_subject: "合成样本".to_string(),
            stats: ScanStats {
                regions: count,
                edges: 0,
                files: count,
            },
            changes,
            assessments,
            edges: Vec::new(),
        }
    }
}
