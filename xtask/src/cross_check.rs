//! 跨平台验证触发判定：一次改动是否必须跑第二个 OS 实例。
//!
//! 背景（`docs/spec-notes/p2-durable-adapter.md` §9）：P2 曾出现「Windows 绿、Linux 红」——
//! `pid_alive` 用 `kill -0` 探测，僵尸进程被判为存活，Windows 走 `OpenProcess` 永远测不出来。
//! 同时有 3 个安全断言写在 `#[cfg(unix)]` 下，在 Windows 上**连编译都不发生**。
//! 所以「Windows 全绿」不能证明这些路径成立。
//!
//! 但全量跑第二遍的成本并不低，而平台分叉只集中在少数文件。本模块把这个决策机械化：
//!
//! - **不硬编码文件清单**：每次扫描源码里的 `cfg(unix)` / `cfg(windows)` / `cfg(target_os...)`
//!   等标记，新增分叉文件自动纳入，清单不会腐烂。
//! - **未知一律从严**：拿不到 diff、扫描失败、解析不了删除文件时给 `Unknown`，
//!   按「必须跨平台」处理。信息不全时不得放行，这是本模块唯一允许的偏向。
//! - **删除的文件读旧内容**：文件被删掉后不再出现在扫描结果里，但删除本身可能移除一条
//!   平台分叉。用 `git show <from>:<path>` 读旧版本再扫，避免漏判。
//!
//! 退出码语义（供脚本组合）：
//! - `0` 可以只跑单平台
//! - `1` 必须跨平台
//! - `2` 无法判定，按必须跨平台处理

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 条件编译标记。命中任一即视为平台分叉。
///
/// 不含裸 `cfg(`：`cfg(test)` / `cfg(feature = ...)` 与平台无关。
/// `cfg_attr` 只收平台谓词形式，避免 `cfg_attr(feature = "serde", derive(...))` 这类误报。
pub const PLATFORM_MARKERS: &[&str] = &[
    "cfg(unix)",
    "cfg(windows)",
    "cfg(not(unix))",
    "cfg(not(windows))",
    "cfg(target_os",
    "cfg(target_family",
    "cfg(target_env",
    "cfg(target_arch",
    "cfg_attr(unix",
    "cfg_attr(windows",
];

/// 构建配置。改这些可能改变链接目标、feature 组合或工具链，两个平台都要过一遍。
pub const BUILD_SENSITIVE: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rust-toolchain",
    "build.rs",
    ".cargo/config.toml",
    ".cargo/config",
];

/// 扫描根（相对 workspace 根）。
const SCAN_ROOTS: &[&str] = &["crates", "xtask", "apps"];

/// 一处分叉命中：文件内的行号与原始行文本，用于 `--explain`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformHit {
    pub line: usize,
    pub text: String,
}

/// 命中原因。两个来源分别记录，报告里能分辨是「代码分叉」还是「构建配置」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reason {
    /// 命中的平台分叉文件 → 命中处。
    pub platform_hits: BTreeMap<String, Vec<PlatformHit>>,
    /// 命中的构建配置文件。
    pub build_files: Vec<String>,
}

impl Reason {
    pub fn is_empty(&self) -> bool {
        self.platform_hits.is_empty() && self.build_files.is_empty()
    }
}

/// 判定结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossVerdict {
    /// 改动未触及平台分叉，可以只跑单平台。
    NotRequired {
        /// 有改动但都不相关。
        changed: usize,
    },
    /// 必须跨平台。
    Required { changed: usize, reason: Reason },
    /// 无法判定。**按必须跨平台处理**，并说明缺什么信息。
    Unknown { detail: String },
}

impl CrossVerdict {
    /// 退出码：0 放行单平台，1 必须跨平台，2 无法判定。
    pub fn exit_code(&self) -> u8 {
        match self {
            CrossVerdict::NotRequired { .. } => 0,
            CrossVerdict::Required { .. } => 1,
            CrossVerdict::Unknown { .. } => 2,
        }
    }
}

/// 判断一行是否真的含条件编译属性。
///
/// 只认以 `#[` 开头（允许缩进）的行。这一条同时挡掉三类假阳性：
/// - 文档注释引用属性，如 `workspace_safety.rs` 里「另由 `#[cfg(unix)]` 用例覆盖」；
/// - 字符串字面量，如 `"#[cfg(unix)]"`——本模块自己的测试里就有；
/// - 行内注释 `// #[cfg(unix)] 暂时禁用`。
///
/// 不这么做的话，本模块会把自己标记成跨平台文件（自指）。
fn line_has_marker(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("#[") {
        return false;
    }
    PLATFORM_MARKERS
        .iter()
        .any(|marker| trimmed.contains(marker))
}

/// 扫一段源码，收集所有分叉命中。
pub fn scan_source(source: &str) -> Vec<PlatformHit> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| line_has_marker(line))
        .map(|(index, line)| PlatformHit {
            line: index + 1,
            text: line.trim().to_string(),
        })
        .collect()
}

/// 把路径统一成正斜杠相对路径，与 `git diff --name-only` 的输出对齐。
///
/// Windows 上 `Path` 拼出来是 `crates\bod-app\src\cas.rs`，不统一就永远匹配不上。
fn normalize(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// 递归收集 `.rs` 文件。跳过 `target` 与点目录（`.git`、`.workbuddy` 等）。
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "target" || name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_rs_files(&path, out),
            Ok(kind) if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") => {
                out.push(path);
            }
            _ => {}
        }
    }
}

/// 扫描结果。分开记录「哪些文件是分叉文件」与「每个文件的命中处」。
#[derive(Debug, Clone, Default)]
pub struct PlatformScan {
    pub hits: BTreeMap<String, Vec<PlatformHit>>,
    /// 扫描到的 `.rs` 总数，用来说明覆盖面（避免「0 个分叉」其实是没扫到东西）。
    pub files_scanned: usize,
    /// 扫描失败的根，非空表示覆盖面不全。
    pub failed_roots: Vec<String>,
}

impl PlatformScan {
    pub fn files_with_fork(&self) -> usize {
        self.hits.len()
    }

    pub fn total_hits(&self) -> usize {
        self.hits.values().map(Vec::len).sum()
    }
}

/// 扫描整个工作区。读失败的文件记入 `failed_roots`（以文件路径形式），不静默跳过。
pub fn scan_workspace(root: &Path) -> PlatformScan {
    let mut scan = PlatformScan::default();
    let mut files = Vec::new();
    for dir in SCAN_ROOTS {
        let base = root.join(dir);
        if base.is_dir() {
            collect_rs_files(&base, &mut files);
        }
    }

    for path in files {
        scan.files_scanned += 1;
        match fs::read_to_string(&path) {
            Ok(source) => {
                let hits = scan_source(&source);
                if !hits.is_empty() {
                    scan.hits.insert(normalize(&path, root), hits);
                }
            }
            Err(err) => {
                // 读不了就不能断言它没有分叉。
                scan.failed_roots
                    .push(format!("{}：{err}", normalize(&path, root)));
            }
        }
    }
    scan
}

/// 判定用的输入。抽出来是为了能脱离 git 测试。
pub struct Assessment<'a> {
    pub changed: &'a [String],
    pub scan: &'a PlatformScan,
    /// 已删除文件在旧版本里的分叉命中（由调用方用 `git show` 取回后扫描）。
    pub deleted_hits: &'a BTreeMap<String, Vec<PlatformHit>>,
}

/// 纯判定：不碰文件系统、不碰 git。
pub fn assess(input: Assessment<'_>) -> CrossVerdict {
    // 扫描有读失败的文件时，命中集可能不全，不能据此说「不相关」。
    if !input.scan.failed_roots.is_empty() {
        return CrossVerdict::Unknown {
            detail: format!(
                "{} 个文件读取失败，分叉清单不完整：{}",
                input.scan.failed_roots.len(),
                input.scan.failed_roots.join("；")
            ),
        };
    }

    if input.changed.is_empty() {
        return CrossVerdict::NotRequired { changed: 0 };
    }

    let mut reason = Reason::default();

    for path in input.changed {
        // 现役分叉文件。
        if let Some(hits) = input.scan.hits.get(path) {
            reason.platform_hits.insert(path.clone(), hits.clone());
        }
        // 被删除的分叉文件：现役扫描里已经没有它了，靠旧内容补。
        if let Some(hits) = input.deleted_hits.get(path)
            && !hits.is_empty()
        {
            reason.platform_hits.insert(path.clone(), hits.clone());
        }
        // 构建配置：只按文件名匹配，任意层级。
        let file_name = path.rsplit('/').next().unwrap_or(path);
        let is_build = BUILD_SENSITIVE.contains(&file_name)
            || path.ends_with(".cargo/config.toml")
            || path.ends_with(".cargo/config");
        if is_build {
            reason.build_files.push(path.clone());
        }
    }

    if reason.is_empty() {
        CrossVerdict::NotRequired {
            changed: input.changed.len(),
        }
    } else {
        CrossVerdict::Required {
            changed: input.changed.len(),
            reason,
        }
    }
}

/// diff 模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSpec {
    /// 提交区间。
    Range { from: String, to: String },
    /// 暂存区相对 HEAD。
    Staged,
    /// 工作区相对暂存区。
    Worktree,
}

impl DiffSpec {
    fn git_args(&self) -> Vec<String> {
        let names = ["diff", "--name-status", "--no-renames"];
        match self {
            DiffSpec::Range { from, to } => {
                let mut args: Vec<String> = names.iter().map(|s| s.to_string()).collect();
                args.push(format!("{from}..{to}"));
                args
            }
            DiffSpec::Staged => {
                let mut args: Vec<String> = names.iter().map(|s| s.to_string()).collect();
                args.push("--cached".to_string());
                args
            }
            DiffSpec::Worktree => names.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            DiffSpec::Range { from, to } => format!("git diff {from}..{to}"),
            DiffSpec::Staged => "git diff --cached".to_string(),
            DiffSpec::Worktree => "git diff（工作区）".to_string(),
        }
    }
}

/// 一次 diff 解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffFiles {
    /// 变更路径（含新增、修改、删除）。
    pub all: Vec<String>,
    /// 被删除的路径，需要回读旧内容。
    pub deleted: Vec<String>,
}

/// 解析 `git diff --name-status` 输出。状态码首字母为 `D` 的即删除。
pub fn parse_name_status(output: &str) -> DiffFiles {
    let mut files = DiffFiles::default();
    for line in output.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let Some((status, path)) = line.split_once('\t') else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        files.all.push(path.to_string());
        if status.starts_with('D') {
            files.deleted.push(path.to_string());
        }
    }
    files
}

/// 把 git 的多行 stderr 压成一行，避免未知判定时把结论排版冲散。
fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 200 {
        format!("{}…", joined.chars().take(200).collect::<String>())
    } else {
        joined
    }
}

fn git(root: &Path, args: &[String]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|err| format!("无法执行 git：{err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} 失败：{}",
            args.join(" "),
            one_line(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 读取已删除文件在 `rev` 处的旧内容并扫描分叉。
///
/// 读不到就是 `Err`——调用方据此给 `Unknown`，不能当「没有分叉」。
fn scan_deleted(
    root: &Path,
    rev: &str,
    deleted: &[String],
) -> Result<BTreeMap<String, Vec<PlatformHit>>, String> {
    let mut out = BTreeMap::new();
    for path in deleted {
        if !path.ends_with(".rs") {
            continue;
        }
        let spec = format!("{rev}:{path}");
        let source = git(root, &["show".to_string(), spec])?;
        let hits = scan_source(&source);
        if !hits.is_empty() {
            out.insert(path.clone(), hits);
        }
    }
    Ok(out)
}

/// 一次判定的完整产物：结论 + 扫描快照（渲染 `--explain` 要用）。
pub struct RunResult {
    pub verdict: CrossVerdict,
    pub scan: PlatformScan,
}

/// 未跟踪的新文件。`git diff` 看不见它们，但新文件完全可能引入一条 `#[cfg(unix)]`。
fn untracked(root: &Path) -> Result<Vec<String>, String> {
    let output = git(
        root,
        &[
            "ls-files".to_string(),
            "--others".to_string(),
            "--exclude-standard".to_string(),
        ],
    )?;
    Ok(output
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

/// 完整流程：取 diff → 扫工作区 → （必要时）读旧内容 → 判定。
pub fn run(root: &Path, spec: &DiffSpec) -> RunResult {
    let scan = scan_workspace(root);
    let output = match git(root, &spec.git_args()) {
        Ok(text) => text,
        Err(err) => {
            return RunResult {
                verdict: CrossVerdict::Unknown {
                    detail: format!("{}：{err}", spec.describe()),
                },
                scan,
            };
        }
    };
    let mut files = parse_name_status(&output);

    // 工作区模式下把未跟踪的新文件算进来。`git diff` 不显示它们，漏掉就是漏判。
    if matches!(spec, DiffSpec::Worktree) {
        match untracked(root) {
            Ok(list) => {
                for path in list {
                    if !files.all.contains(&path) {
                        files.all.push(path);
                    }
                }
            }
            Err(err) => {
                return RunResult {
                    verdict: CrossVerdict::Unknown {
                        detail: format!("读取未跟踪文件失败：{err}"),
                    },
                    scan,
                };
            }
        }
    }

    // 删除的文件回读旧版本；读不到就不猜。
    let deleted_hits = if files.deleted.is_empty() {
        BTreeMap::new()
    } else {
        let rev = match spec {
            DiffSpec::Range { from, .. } => from.clone(),
            // 暂存区/工作区的删除，旧版本在 HEAD。
            DiffSpec::Staged | DiffSpec::Worktree => "HEAD".to_string(),
        };
        match scan_deleted(root, &rev, &files.deleted) {
            Ok(hits) => hits,
            Err(err) => {
                return RunResult {
                    verdict: CrossVerdict::Unknown {
                        detail: format!("读取已删除文件的旧版本失败：{err}"),
                    },
                    scan,
                };
            }
        }
    };

    let verdict = assess(Assessment {
        changed: &files.all,
        scan: &scan,
        deleted_hits: &deleted_hits,
    });
    RunResult { verdict, scan }
}

/// 渲染人类可读结论。`explain` 为真时逐条列出命中行。
pub fn render(
    verdict: &CrossVerdict,
    scan: &PlatformScan,
    spec: &DiffSpec,
    explain: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("[cross] 依据：{}\n", spec.describe()));
    out.push_str(&format!(
        "[cross] 分叉扫描：{} 个文件 / {} 处条件编译，覆盖 {} 个 .rs\n",
        scan.files_with_fork(),
        scan.total_hits(),
        scan.files_scanned
    ));

    match verdict {
        CrossVerdict::NotRequired { changed } => {
            out.push_str(&format!(
                "[cross] 改动 {changed} 个文件，未触及平台分叉与构建配置\n"
            ));
            out.push_str("[cross] 判定：可以只跑单平台\n");
            out.push_str("[cross] 结论：本次改动只需 Windows 全量；push 后 CI 仍会跑 Linux 全量\n");
        }
        CrossVerdict::Required { changed, reason } => {
            out.push_str(&format!(
                "[cross] 改动 {changed} 个文件，其中触及跨平台语义：\n"
            ));
            for (path, hits) in &reason.platform_hits {
                let lines: Vec<String> = hits.iter().map(|h| h.line.to_string()).collect();
                out.push_str(&format!(
                    "  - {path}：{} 处（行 {}）\n",
                    hits.len(),
                    lines.join(", ")
                ));
                if explain {
                    for hit in hits {
                        out.push_str(&format!("      {}: {}\n", hit.line, hit.text));
                    }
                }
            }
            for path in &reason.build_files {
                out.push_str(&format!("  - {path}：构建配置\n"));
            }
            out.push_str("[cross] 判定：必须跨平台\n");
            out.push_str("[cross] 结论：不得只跑 Windows；需在 Linux（WSL 或 CI）跑一次全量测试\n");
        }
        CrossVerdict::Unknown { detail } => {
            out.push_str(&format!("[cross] 判定：无法判定（{detail}）\n"));
            out.push_str("[cross] 结论：按必须跨平台处理；信息不全时不得放行单平台\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(lines: &[usize]) -> Vec<PlatformHit> {
        lines
            .iter()
            .map(|&line| PlatformHit {
                line,
                text: format!("#[cfg(unix)] // {line}"),
            })
            .collect()
    }

    fn scan_with(files: &[(&str, Vec<usize>)]) -> PlatformScan {
        let mut scan = PlatformScan {
            files_scanned: 100,
            ..Default::default()
        };
        for (path, lines) in files {
            scan.hits.insert((*path).to_string(), hits(lines));
        }
        scan
    }

    #[test]
    fn comment_mentioning_cfg_is_not_a_fork() {
        // workspace_safety.rs 里真有这样一行文档注释，纯文本搜索会误报。
        let source = "/// 真实软链接另由 `#[cfg(unix)]` 用例覆盖。\nfn real() {}\n";
        assert!(scan_source(source).is_empty());
    }

    #[test]
    fn leading_comment_line_is_skipped() {
        let source = "    // #[cfg(unix)] 暂时禁用\n    #[cfg(windows)]\n    fn x() {}\n";
        let found = scan_source(source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 2);
    }

    /// 自指保护：本模块的测试里满是 `"#[cfg(unix)]"` 这样的字符串字面量，
    /// 若把它们算作分叉，扫描器会把自己标成跨平台文件。
    #[test]
    fn string_literals_quoting_the_attribute_do_not_match() {
        let source = "        \"#[cfg(unix)]\",\n        \"#[cfg(windows)]\",\n";
        assert!(scan_source(source).is_empty());
    }

    #[test]
    fn cfg_attr_matches_platform_predicates_only() {
        let platform = "#[cfg_attr(unix, allow(dead_code))]\nfn a() {}\n";
        assert_eq!(scan_source(platform).len(), 1);

        let feature = "#[cfg_attr(feature = \"serde\", derive(Serialize))]\nstruct S;\n";
        assert!(
            scan_source(feature).is_empty(),
            "feature 的 cfg_attr 不是平台分叉"
        );
    }

    #[test]
    fn detects_common_markers() {
        for marker in [
            "#[cfg(unix)]",
            "#[cfg(windows)]",
            "#[cfg(not(unix))]",
            "#[cfg(target_os = \"linux\")]",
            "#[cfg(target_family = \"unix\")]",
        ] {
            let source = format!("{marker}\nfn f() {{}}\n");
            assert_eq!(scan_source(&source).len(), 1, "应识别 {marker}");
        }
    }

    #[test]
    fn cfg_test_and_feature_are_not_platform_forks() {
        let source = "#[cfg(test)]\nmod tests {}\n#[cfg(feature = \"pg\")]\nfn g() {}\n";
        assert!(scan_source(source).is_empty());
    }

    #[test]
    fn no_change_is_not_required() {
        let scan = scan_with(&[("crates/a.rs", vec![1])]);
        let verdict = assess(Assessment {
            changed: &[],
            scan: &scan,
            deleted_hits: &BTreeMap::new(),
        });
        assert_eq!(verdict, CrossVerdict::NotRequired { changed: 0 });
        assert_eq!(verdict.exit_code(), 0);
    }

    #[test]
    fn unrelated_change_is_not_required() {
        let scan = scan_with(&[("crates/bod-app/src/cas.rs", vec![411])]);
        let changed = vec!["crates/bod-app/src/store.rs".to_string()];
        let verdict = assess(Assessment {
            changed: &changed,
            scan: &scan,
            deleted_hits: &BTreeMap::new(),
        });
        assert_eq!(verdict, CrossVerdict::NotRequired { changed: 1 });
    }

    #[test]
    fn fork_file_change_is_required() {
        let scan = scan_with(&[("crates/bod-app/src/durable_process.rs", vec![159, 199])]);
        let changed = vec![
            "crates/bod-app/src/durable_process.rs".to_string(),
            "crates/bod-app/src/store.rs".to_string(),
        ];
        let verdict = assess(Assessment {
            changed: &changed,
            scan: &scan,
            deleted_hits: &BTreeMap::new(),
        });
        match verdict {
            CrossVerdict::Required { changed, reason } => {
                assert_eq!(changed, 2);
                assert_eq!(reason.platform_hits.len(), 1);
                assert_eq!(
                    reason.platform_hits["crates/bod-app/src/durable_process.rs"].len(),
                    2
                );
            }
            other => panic!("应判必须跨平台，实际 {other:?}"),
        }
    }

    #[test]
    fn build_files_are_required_at_any_depth() {
        let scan = scan_with(&[]);
        for path in [
            "Cargo.toml",
            "crates/bod-app/Cargo.toml",
            "xtask/Cargo.toml",
            "build.rs",
            "crates/bod-app/build.rs",
            "rust-toolchain.toml",
            "Cargo.lock",
        ] {
            let changed = vec![path.to_string()];
            let verdict = assess(Assessment {
                changed: &changed,
                scan: &scan,
                deleted_hits: &BTreeMap::new(),
            });
            assert!(
                matches!(verdict, CrossVerdict::Required { .. }),
                "{path} 应要求跨平台，实际 {verdict:?}"
            );
        }
    }

    #[test]
    fn deleting_a_fork_file_is_required_via_old_content() {
        let scan = scan_with(&[]);
        let mut deleted_hits = BTreeMap::new();
        deleted_hits.insert("crates/bod-app/src/old_unix.rs".to_string(), hits(&[7]));
        let changed = vec!["crates/bod-app/src/old_unix.rs".to_string()];
        let verdict = assess(Assessment {
            changed: &changed,
            scan: &scan,
            deleted_hits: &deleted_hits,
        });
        assert!(matches!(verdict, CrossVerdict::Required { .. }));
    }

    #[test]
    fn unreadable_file_forces_unknown_not_release() {
        let mut scan = scan_with(&[]);
        scan.failed_roots.push("crates/x.rs：权限不足".to_string());
        let changed = vec!["crates/bod-app/src/store.rs".to_string()];
        let verdict = assess(Assessment {
            changed: &changed,
            scan: &scan,
            deleted_hits: &BTreeMap::new(),
        });
        assert!(matches!(verdict, CrossVerdict::Unknown { .. }));
        assert_eq!(verdict.exit_code(), 2);
    }

    #[test]
    fn parses_name_status_with_deletions() {
        let output =
            "M\tcrates/bod-app/src/store.rs\nD\tcrates/bod-app/src/gone.rs\nA\tdocs/x.md\n";
        let files = parse_name_status(output);
        assert_eq!(files.all.len(), 3);
        assert_eq!(files.deleted, vec!["crates/bod-app/src/gone.rs"]);
    }

    #[test]
    fn parses_empty_diff() {
        assert_eq!(parse_name_status(""), DiffFiles::default());
        assert_eq!(parse_name_status("\n\n"), DiffFiles::default());
    }

    #[test]
    fn normalize_uses_forward_slashes() {
        let root = Path::new("/repo");
        let path = PathBuf::from("crates")
            .join("bod-app")
            .join("src")
            .join("cas.rs");
        assert_eq!(normalize(&path, root), "crates/bod-app/src/cas.rs");
    }

    /// 真实仓库的自检：扫描结果必须覆盖到已知的 4 个分叉文件。
    ///
    /// 这条用例同时防两种退化——扫描器坏掉返回 0 个文件，或误把注释算进去。
    #[test]
    fn repository_scan_finds_known_fork_files() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask 在 workspace 根下")
            .to_path_buf();
        let scan = scan_workspace(&root);
        assert!(
            scan.failed_roots.is_empty(),
            "扫描不应有读失败：{:?}",
            scan.failed_roots
        );
        for expected in [
            "crates/bod-app/src/cas.rs",
            "crates/bod-app/src/durable_process.rs",
            "crates/bod-app/src/host_handshake.rs",
            "crates/bod-app/src/workspace_safety.rs",
        ] {
            assert!(
                scan.hits.contains_key(expected),
                "应扫到 {expected}，实际 {:?}",
                scan.hits.keys().collect::<Vec<_>>()
            );
        }
        // workspace_safety.rs 那行文档注释不得计入。
        let safety = &scan.hits["crates/bod-app/src/workspace_safety.rs"];
        assert!(
            safety.iter().all(|h| !h.text.starts_with("///")),
            "文档注释被误判：{safety:?}"
        );
    }
}
