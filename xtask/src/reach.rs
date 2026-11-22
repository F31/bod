//! 模块可达性只读报表（3.5 段）。
//!
//! 解决的问题：`verify --stage P0` 只数「有没有断言」，不数「有没有调用方」。
//! 结果是一个 1,400 行的纯函数模块可以既通过全部门禁、又在全工作区零引用——
//! 集成成本不进入任何指标，沉没风险被结构性地隐藏。
//!
//! 本模块补的是**第三维**：可达 / 不可达。它只读数，不判好坏：
//!
//! - 孤岛可能是下一段要接线的资产（例如 `coverage_receipt` 对 §11 就是这样），
//!   也可能是该删的沉淀。判定权在评审，工具只负责把这件事列出来。
//! - 因此默认**不进硬门禁**。要防「新增模块悄悄变孤岛」，用 `--check BASELINE`
//!   与冻结的基线比对，只对**相对基线新增**的孤岛报警。
//!
//! 口径（写进输出，避免被当成精确的编译器语义）：
//!
//! - 「引用」= **全工作区**任意 `src` / `tests` 文件里出现 `<name>::`，或以 `use`
//!   开头且标识符里含 `<name>`。字符串字面量里的偶发命中不排除——偏保守
//!   （宁可算可达）。只扫同 crate 会把 `host_handshake` 这种「被 CLI 装配层使用」
//!   的模块误判成孤岛，所以引用者范围必须覆盖 `crates/*`、`apps/*`、`xtask`。
//! - `pub mod <name>;` 只是**注册**，不算引用。注册在报表里单独一列。
//! - 根文件（`lib.rs` / `main.rs`）里的 `pub use <name>` 单独标 `reexported`，
//!   **不计入可达**：对外导出不等于内部有调用方。根文件里的非 `pub use` 使用
//!   照常计入可达。
//! - `tests/`、`examples/` 下的引用归入 `test_files`，生产零引用 + 测试有引用
//!   记为 `test_only`，不是 `live`。

use std::fs;
use std::path::{Path, PathBuf};

/// 模块的可达状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// 至少被一个其他生产文件引用。
    Live,
    /// 生产代码零引用，只有测试引用。
    TestOnly,
    /// 连测试都不引用。
    Orphan,
}

impl Reach {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::TestOnly => "test_only",
            Self::Orphan => "orphan",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "live" => Some(Self::Live),
            "test_only" => Some(Self::TestOnly),
            "orphan" => Some(Self::Orphan),
            _ => None,
        }
    }
}

/// 一个模块的读数。
#[derive(Debug, Clone)]
pub struct ModuleRow {
    pub name: String,
    pub lines: usize,
    /// 父模块（`lib.rs` / `main.rs`）里是否有 `mod <name>;` 声明。
    pub registered: bool,
    /// 引用它的其他生产文件（不含 `lib.rs` / `main.rs` 里的 re-export）。
    pub production_files: Vec<String>,
    /// 引用它的测试文件（`tests/` / `examples/` 目录下）。
    pub test_files: Vec<String>,
    /// `lib.rs` / `main.rs` 里 `pub use` 的再导出位置。
    pub reexported_in: Vec<String>,
}

impl ModuleRow {
    pub fn reach(&self) -> Reach {
        if !self.production_files.is_empty() {
            Reach::Live
        } else if !self.test_files.is_empty() {
            Reach::TestOnly
        } else {
            Reach::Orphan
        }
    }
}

/// 一次报表。
#[derive(Debug, Clone)]
pub struct Report {
    pub crate_name: String,
    pub total_lines: usize,
    /// 已按行数降序排列。
    pub rows: Vec<ModuleRow>,
}

impl Report {
    pub fn count(&self, reach: Reach) -> usize {
        self.rows.iter().filter(|row| row.reach() == reach).count()
    }

    pub fn lines_of(&self, reach: Reach) -> usize {
        self.rows
            .iter()
            .filter(|row| row.reach() == reach)
            .map(|row| row.lines)
            .sum()
    }

    pub fn find(&self, name: &str) -> Option<&ModuleRow> {
        self.rows.iter().find(|row| row.name == name)
    }
}

/// 扫描一个 crate 的 `src`，产出读数。
///
/// 引用者范围是**整个工作区**：`crates/*`、`apps/*`、`xtask/src`。只扫同 crate
/// 会把「被 CLI 装配层使用」的模块误判成孤岛——这个错我第一版就犯过，
/// `host_handshake` 被标成孤岛，而它明明在 `crates/bod/src/main.rs` 里被 `use`。
pub fn collect(root: &Path, crate_name: &str) -> Result<Report, String> {
    let src_dir = root.join("crates").join(crate_name).join("src");
    if !src_dir.is_dir() {
        return Err(format!("找不到源码目录 {}", src_dir.display()));
    }
    let sources = read_tree(root, &src_dir)?;
    let referrers = workspace_sources(root)?;

    let mut rows: Vec<ModuleRow> = Vec::new();
    for (rel_path, text) in &sources {
        let Some(name) = module_name(rel_path) else {
            // `lib.rs` / `main.rs` 是根，不是模块本体。
            continue;
        };
        let registered = sources
            .iter()
            .any(|(other, other_text)| is_root_file(other) && declares_module(other_text, &name));

        let mut production_files = Vec::new();
        let mut test_files = Vec::new();
        let mut reexported_in = Vec::new();
        for (other, other_text) in &referrers {
            // 同一个顶层模块内部的互相引用不算「外部引用」：
            // `store/delivery.rs` 里 `use crate::store::StoreError` 不是可达性证据。
            if module_name(other).as_deref() == Some(name.as_str()) {
                continue;
            }
            if !mentions_module(other_text, &name) {
                continue;
            }
            if is_test_path(other) {
                test_files.push(other.clone());
                continue;
            }
            if is_root_file(other) {
                if reexports_module(other_text, &name) {
                    reexported_in.push(other.clone());
                }
                // 根文件里的非 `pub use` 使用是真实调用（main.rs 的装配最常见）。
                if mentions_outside_reexport(other_text, &name) {
                    production_files.push(other.clone());
                }
            } else {
                production_files.push(other.clone());
            }
        }

        // 一个顶层模块可能由多个文件组成（`store.rs` + `store/*.rs`）：
        // 行数累加，引用者并集。这样拆分不改变读数。
        match rows.iter_mut().find(|row| row.name == name) {
            Some(existing) => {
                existing.lines += text.lines().count();
                existing.registered |= registered;
                merge_into(&mut existing.production_files, production_files);
                merge_into(&mut existing.test_files, test_files);
                merge_into(&mut existing.reexported_in, reexported_in);
            }
            None => rows.push(ModuleRow {
                name,
                lines: text.lines().count(),
                registered,
                production_files,
                test_files,
                reexported_in,
            }),
        }
    }

    rows.sort_by(|left, right| {
        right
            .lines
            .cmp(&left.lines)
            .then_with(|| left.name.cmp(&right.name))
    });
    let total_lines = sources.iter().map(|(_, text)| text.lines().count()).sum();
    Ok(Report {
        crate_name: crate_name.to_string(),
        total_lines,
        rows,
    })
}

/// 全工作区里可能构成引用的源码文件。
fn workspace_sources(root: &Path) -> Result<Vec<(String, String)>, String> {
    let mut paths = Vec::new();
    for base in ["crates", "apps"] {
        let Ok(entries) = fs::read_dir(root.join(base)) else {
            continue;
        };
        for entry in entries.flatten() {
            let crate_dir = entry.path();
            if !crate_dir.is_dir() {
                continue;
            }
            for sub in ["src", "tests", "examples", "benches"] {
                let sub_dir = crate_dir.join(sub);
                if sub_dir.is_dir() {
                    collect_rs(&sub_dir, &mut paths);
                }
            }
        }
    }
    let xtask_src = root.join("xtask").join("src");
    if xtask_src.is_dir() {
        collect_rs(&xtask_src, &mut paths);
    }
    paths.sort();
    paths.dedup();

    let mut out = Vec::new();
    for path in paths {
        let text =
            fs::read_to_string(&path).map_err(|err| format!("读不了 {}：{err}", path.display()))?;
        out.push((display_rel(root, &path), text));
    }
    Ok(out)
}

fn read_tree(root: &Path, dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut paths = Vec::new();
    collect_rs(dir, &mut paths);
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let text =
            fs::read_to_string(&path).map_err(|err| format!("读不了 {}：{err}", path.display()))?;
        out.push((display_rel(root, &path), text));
    }
    Ok(out)
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_rs(&path, out),
            Ok(kind) if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") => {
                out.push(path);
            }
            _ => {}
        }
    }
}

fn display_rel(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// 文件名 → 模块名。`lib.rs` / `main.rs` 返回 `None`；`mod.rs` 取父目录名。
fn module_name(rel_path: &str) -> Option<String> {
    // 模块身份取 `src/` 之后的第一段：`store.rs` 与 `store/delivery.rs` 都属于
    // 顶层模块 `store`。子模块是本 crate 的内部结构（inherent impl 扩展），
    // 没有独立的外部调用方，按文件逐个算「模块」会把拆分误报成一批新孤岛。
    let after_src = rel_path
        .split("/src/")
        .nth(1)
        .unwrap_or_else(|| rel_path.rsplit('/').next().unwrap_or(rel_path));
    let first = after_src.split('/').next()?;
    let stem = first.strip_suffix(".rs").unwrap_or(first);
    match stem {
        "lib" | "main" => None,
        other => Some(other.to_string()),
    }
}

/// 合并引用者列表并去重（同一模块的多个文件会给出重复条目）。
fn merge_into(dst: &mut Vec<String>, src: Vec<String>) {
    for item in src {
        if !dst.contains(&item) {
            dst.push(item);
        }
    }
}

fn is_root_file(rel_path: &str) -> bool {
    let file = rel_path.rsplit('/').next().unwrap_or("");
    file == "lib.rs" || file == "main.rs"
}

/// 测试与示例目录下的文件：引用真实，但不构成「生产可达」。
fn is_test_path(rel_path: &str) -> bool {
    rel_path.contains("/tests/")
        || rel_path.contains("/examples/")
        || rel_path.contains("/benches/")
}

/// 该行是否是 `<name>` 的模块声明（注册），而非使用。
fn declares_module(text: &str, name: &str) -> bool {
    text.lines().any(|raw| line_declares_module(raw, name))
}

fn line_declares_module(raw: &str, name: &str) -> bool {
    let tokens: Vec<&str> = raw
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '{' | '(' | ')'))
        .filter(|token| !token.is_empty())
        .collect();
    let Some(index) = tokens.iter().position(|token| *token == "mod") else {
        return false;
    };
    let Some(declared) = tokens.get(index + 1) else {
        return false;
    };
    if *declared != name {
        return false;
    }
    // 前缀只允许可见性修饰：`pub`、`pub(crate)`、`pub(in crate)`。
    tokens[..index]
        .iter()
        .all(|token| matches!(*token, "pub" | "crate" | "in" | "super" | "self"))
}

/// 文件里是否引用了模块 `name`。
fn mentions_module(text: &str, name: &str) -> bool {
    text.lines().any(|raw| line_mentions(raw.trim(), name))
}

/// 是否是 `pub use ...<name>...`——对外再导出，不是内部调用方。
fn reexports_module(text: &str, name: &str) -> bool {
    text.lines().any(|raw| {
        let line = raw.trim();
        is_pub_use(line) && line_mentions(line, name)
    })
}

/// 排除 `pub use` 之后，文件里是否还有对 `name` 的使用。
fn mentions_outside_reexport(text: &str, name: &str) -> bool {
    text.lines().any(|raw| {
        let line = raw.trim();
        !is_pub_use(line) && line_mentions(line, name)
    })
}

fn is_pub_use(line: &str) -> bool {
    line.starts_with("pub use ") || line.starts_with("pub(crate) use ")
}

fn line_mentions(line: &str, name: &str) -> bool {
    if line.is_empty() || line.starts_with("//") {
        return false;
    }
    // 字符串字面量里的模块名不是引用：`assert!(LIB.contains("pub mod x;"))`
    // 这类漂移检查会让每个模块都「看起来可达」。先剥掉字符串内容再判。
    let code = strip_string_literals(line);
    if line_declares_module(&code, name) {
        return false;
    }
    if code.contains(&format!("{name}::")) {
        return true;
    }
    // `use crate::x;` / `use crate::x as y;` / `use crate::{a, x};`
    code.trim_start().starts_with("use ") && use_tokens(&code).any(|token| token == name)
}

/// 去掉双引号字符串的内容，保留引号外的代码。
///
/// 只处理普通字符串与转义；`r"..."` 原始字符串、字符字面量不处理——
/// 它们出现在「引用判定」上的概率远低于普通字符串，且误判方向是保守的。
fn strip_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            continue;
        }
        out.push(ch);
    }
    out
}

fn use_tokens(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
}

/// 渲染成人类可读的报表。
pub fn render(report: &Report, top: usize, show_all: bool) -> String {
    let mut out = String::new();
    let modules = report.rows.len();
    out.push_str(&format!(
        "[reach] crate={} 模块 {modules} / 源码 {} 行\n",
        report.crate_name, report.total_lines
    ));
    let live = report.count(Reach::Live);
    let test_only = report.count(Reach::TestOnly);
    let orphan = report.count(Reach::Orphan);
    let orphan_lines = report.lines_of(Reach::Orphan);
    let percentage = if modules == 0 {
        0.0
    } else {
        orphan as f64 * 100.0 / modules as f64
    };
    let line_percentage = if report.total_lines == 0 {
        0.0
    } else {
        orphan_lines as f64 * 100.0 / report.total_lines as f64
    };
    out.push_str(&format!(
        "[reach] 可达 {live} / 仅测试 {test_only} / 孤岛 {orphan}（{percentage:.1}% 模块，{orphan_lines} 行，{line_percentage:.1}%）\n"
    ));

    let excluded: Vec<&ModuleRow> = report
        .rows
        .iter()
        .filter(|row| row.reach() != Reach::Live)
        .collect();
    if excluded.is_empty() {
        out.push_str("[reach] 没有孤岛\n");
    } else {
        let shown: Vec<&ModuleRow> = if show_all {
            excluded.clone()
        } else {
            excluded.iter().take(top.max(1)).copied().collect()
        };
        out.push_str(&format!(
            "[reach] 非可达清单（行数降序，显示 {} / {}）：\n",
            shown.len(),
            excluded.len()
        ));
        for row in shown {
            let state = match row.reach() {
                Reach::TestOnly => "仅测试",
                Reach::Orphan => "孤岛",
                Reach::Live => "可达",
            };
            let registered = if row.registered {
                "已注册"
            } else {
                "未注册"
            };
            let reexport = if row.reexported_in.is_empty() {
                String::new()
            } else {
                format!("，reexport={}", row.reexported_in.join(","))
            };
            out.push_str(&format!(
                "[reach]   {:>5}  {:<28} {:<10} {}{}\n",
                row.lines, row.name, state, registered, reexport
            ));
        }
    }

    out.push_str(
        "[reach] 口径：引用 = 全工作区代码里出现 `<name>::` 或 `use ... <name>`；字符串字面量不计；\n",
    );
    out.push_str(
        "[reach]       `pub mod <name>;` 只是注册，不算引用；根文件的 `pub use` 不计入可达\n",
    );
    out.push_str("[reach] 本报表只读数，不判通过，不进 verify 门禁；防新增孤岛用 --check 基线\n");
    out
}

/// 基线文本（TSV，零依赖可解析）。首行是版本与 crate 名。
pub fn baseline_text(report: &Report) -> String {
    let mut out = format!(
        "# bod-reach-baseline v1 crate={} modules={} lines={}\n",
        report.crate_name,
        report.rows.len(),
        report.total_lines
    );
    out.push_str("# name\tlines\treach\n");
    for row in &report.rows {
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            row.name,
            row.lines,
            row.reach().label()
        ));
    }
    out
}

/// 基线里的一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEntry {
    pub name: String,
    pub lines: usize,
    pub reach: Reach,
}

/// 解析基线。未知行直接报错，避免「格式悄悄变了但比对照样通过」。
pub fn parse_baseline(text: &str) -> Result<Vec<BaselineEntry>, String> {
    let mut entries = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 3 {
            return Err(format!("基线第 {} 行字段数不是 3：{line}", index + 1));
        }
        let lines = fields[1]
            .parse::<usize>()
            .map_err(|_| format!("基线第 {} 行行数不是整数：{line}", index + 1))?;
        let reach = Reach::parse(fields[2])
            .ok_or_else(|| format!("基线第 {} 行状态未知：{line}", index + 1))?;
        entries.push(BaselineEntry {
            name: fields[0].to_string(),
            lines,
            reach,
        });
    }
    Ok(entries)
}

/// 与基线比对的结果。
#[derive(Debug, Clone, Default)]
pub struct Drift {
    /// 基线里没有、现在不可达的模块——这是要拦的。
    pub new_unreachable: Vec<String>,
    /// 基线可达、现在变不可达——退化。
    pub regressed: Vec<String>,
    /// 基线不可达、现在可达——改善，只报喜。
    pub fixed: Vec<String>,
    /// 已不存在的基线条目——模块被删或改名。
    pub vanished: Vec<String>,
}

impl Drift {
    pub fn is_clean(&self) -> bool {
        self.new_unreachable.is_empty() && self.regressed.is_empty()
    }
}

pub fn compare(report: &Report, baseline: &[BaselineEntry]) -> Drift {
    let mut drift = Drift::default();
    for row in &report.rows {
        let current = row.reach();
        let was = baseline.iter().find(|entry| entry.name == row.name);
        match was {
            None => {
                if current != Reach::Live {
                    drift.new_unreachable.push(row.name.clone());
                }
            }
            Some(entry) => {
                let before_reachable = entry.reach == Reach::Live;
                let now_reachable = current == Reach::Live;
                if before_reachable && !now_reachable {
                    drift.regressed.push(row.name.clone());
                } else if !before_reachable && now_reachable {
                    drift.fixed.push(row.name.clone());
                }
            }
        }
    }
    for entry in baseline {
        if report.find(&entry.name).is_none() {
            drift.vanished.push(entry.name.clone());
        }
    }
    drift
}

pub fn render_drift(drift: &Drift) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[reach] 基线比对：新增不可达 {} / 退化 {} / 改善 {} / 消失 {}\n",
        drift.new_unreachable.len(),
        drift.regressed.len(),
        drift.fixed.len(),
        drift.vanished.len()
    ));
    for name in &drift.new_unreachable {
        out.push_str(&format!("[reach]   新增不可达：{name}\n"));
    }
    for name in &drift.regressed {
        out.push_str(&format!("[reach]   退化（原可达现不可达）：{name}\n"));
    }
    for name in &drift.fixed {
        out.push_str(&format!("[reach]   已接线：{name}\n"));
    }
    for name in &drift.vanished {
        out.push_str(&format!("[reach]   基线中有、现在找不到：{name}\n"));
    }
    if drift.is_clean() {
        out.push_str("[reach] 结论：没有新增不可达模块，也没有退化\n");
    } else {
        out.push_str(
            "[reach] 结论：有新增不可达或退化。要么接线，要么在支持矩阵登记 integration: planned\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, lines: usize, production: &[&str], tests: &[&str]) -> ModuleRow {
        ModuleRow {
            name: name.to_string(),
            lines,
            registered: true,
            production_files: production.iter().map(|x| (*x).to_string()).collect(),
            test_files: tests.iter().map(|x| (*x).to_string()).collect(),
            reexported_in: Vec::new(),
        }
    }

    #[test]
    fn module_name_skips_roots_and_maps_mod_rs() {
        assert_eq!(module_name("crates/x/src/lib.rs"), None);
        assert_eq!(module_name("crates/x/src/main.rs"), None);
        assert_eq!(
            module_name("crates/x/src/store.rs").as_deref(),
            Some("store")
        );
        assert_eq!(module_name("crates/x/src/db/mod.rs").as_deref(), Some("db"));
    }

    /// 子模块按顶层模块归并：拆分文件不产生新「模块」。
    #[test]
    fn submodule_files_belong_to_their_top_level_module() {
        assert_eq!(
            module_name("crates/x/src/store/delivery.rs").as_deref(),
            Some("store")
        );
        assert_eq!(
            module_name("crates/x/src/store/tests.rs").as_deref(),
            Some("store")
        );
        // 深层子模块也一样。
        assert_eq!(
            module_name("crates/x/src/store/inner/deep.rs").as_deref(),
            Some("store")
        );
    }

    #[test]
    fn declaration_is_not_a_reference() {
        assert!(line_declares_module("pub mod store;", "store"));
        assert!(line_declares_module("pub(crate) mod store;", "store"));
        assert!(line_declares_module("  mod store;", "store"));
        assert!(!line_declares_module("pub mod store_helper;", "store"));
        assert!(!line_declares_module("let x = store::load();", "store"));
    }

    #[test]
    fn mentions_counts_qualified_paths_and_use_statements() {
        assert!(mentions_module("use crate::store::Store;", "store"));
        assert!(mentions_module("store::open(path)?;", "store"));
        assert!(mentions_module("use crate::store;", "store"));
        assert!(mentions_module("use crate::{store, other};", "store"));
        assert!(!mentions_module("pub mod store;", "store"), "注册不算引用");
        assert!(!mentions_module("// see store::open for details", "store"));
        assert!(
            !mentions_module("use crate::store_helper::x;", "store"),
            "前缀相同的模块不得误判"
        );
    }

    #[test]
    fn reach_is_three_valued() {
        assert_eq!(
            row("a", 10, &["crates/x/src/b.rs"], &[]).reach(),
            Reach::Live
        );
        assert_eq!(
            row("a", 10, &[], &["crates/x/tests/a.rs"]).reach(),
            Reach::TestOnly
        );
        assert_eq!(row("a", 10, &[], &[]).reach(), Reach::Orphan);
    }

    #[test]
    fn report_counts_and_percentages_come_from_rows() {
        let report = Report {
            crate_name: "x".to_string(),
            total_lines: 300,
            rows: vec![
                row("live", 100, &["crates/x/src/b.rs"], &[]),
                row("only_test", 100, &[], &["crates/x/tests/t.rs"]),
                row("orphan", 100, &[], &[]),
            ],
        };
        assert_eq!(report.count(Reach::Live), 1);
        assert_eq!(report.count(Reach::Orphan), 1);
        assert_eq!(report.lines_of(Reach::TestOnly), 100);
        assert!(report.find("orphan").is_some());
        assert!(report.find("missing").is_none());
    }

    #[test]
    fn baseline_round_trip() {
        let report = Report {
            crate_name: "x".to_string(),
            total_lines: 200,
            rows: vec![
                row("live", 100, &["crates/x/src/b.rs"], &[]),
                row("orphan", 100, &[], &[]),
            ],
        };
        let text = baseline_text(&report);
        let parsed = parse_baseline(&text).expect("基线应可解析");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "live");
        assert_eq!(parsed[1].reach, Reach::Orphan);
    }

    #[test]
    fn malformed_baseline_is_an_error_not_a_pass() {
        assert!(parse_baseline("name\tlines\n").is_err());
        assert!(parse_baseline("name\tnot-a-number\torphan\n").is_err());
        assert!(parse_baseline("name\t10\tunknown\n").is_err());
        assert!(parse_baseline("# comment only\n").is_ok());
    }

    #[test]
    fn drift_separates_new_regression_and_fix() {
        let report = Report {
            crate_name: "x".to_string(),
            total_lines: 400,
            rows: vec![
                row("stays_live", 100, &["crates/x/src/b.rs"], &[]),
                row("new_orphan", 100, &[], &[]),
                row("regressed", 100, &[], &[]),
                row("fixed", 100, &["crates/x/src/b.rs"], &[]),
            ],
        };
        let baseline = vec![
            BaselineEntry {
                name: "stays_live".to_string(),
                lines: 100,
                reach: Reach::Live,
            },
            BaselineEntry {
                name: "regressed".to_string(),
                lines: 100,
                reach: Reach::Live,
            },
            BaselineEntry {
                name: "fixed".to_string(),
                lines: 100,
                reach: Reach::Orphan,
            },
            BaselineEntry {
                name: "gone".to_string(),
                lines: 10,
                reach: Reach::Orphan,
            },
        ];
        let drift = compare(&report, &baseline);
        assert_eq!(drift.new_unreachable, vec!["new_orphan".to_string()]);
        assert_eq!(drift.regressed, vec!["regressed".to_string()]);
        assert_eq!(drift.fixed, vec!["fixed".to_string()]);
        assert_eq!(drift.vanished, vec!["gone".to_string()]);
        assert!(!drift.is_clean());
    }

    #[test]
    fn clean_drift_when_nothing_changed() {
        let report = Report {
            crate_name: "x".to_string(),
            total_lines: 100,
            rows: vec![row("a", 100, &["crates/x/src/b.rs"], &[])],
        };
        let baseline = vec![BaselineEntry {
            name: "a".to_string(),
            lines: 100,
            reach: Reach::Live,
        }];
        assert!(compare(&report, &baseline).is_clean());
    }

    /// 真实仓库读数：只验证「能跑、数字自洽」，不锁死具体数值。
    /// 具体基线由 `reach --write-baseline` 生成后入库。
    #[test]
    fn repository_scan_is_self_consistent() {
        let root = crate::workspace_root();
        let report = collect(&root, "bod-app").expect("bod-app 应可扫描");
        assert!(report.rows.len() > 50, "模块数应覆盖全部源文件");
        let sum: usize = report.rows.iter().map(|row| row.lines).sum();
        assert!(
            sum <= report.total_lines,
            "模块行数之和不得超过全部源码行数（lib.rs 等根文件不计入模块）"
        );
        for row in &report.rows {
            assert!(row.registered, "{} 未在 lib.rs 注册", row.name);
            assert!(row.lines > 0, "{} 行数为 0", row.name);
        }

        // 回归：引用者范围必须覆盖全工作区。
        // 第一版只扫同 crate，把 `host_handshake` 误判成孤岛——它在
        // `crates/bod/src/main.rs` 里被 `use bod_app::host_handshake::...` 使用。
        let handshake = report.find("host_handshake").expect("应有 host_handshake");
        assert_eq!(
            handshake.reach(),
            Reach::Live,
            "跨 crate 使用必须算可达，实际引用者：{:?}",
            handshake.production_files
        );
    }

    #[test]
    fn pub_use_is_reexport_not_reach() {
        let text = "pub use crate::coverage_receipt::judge_coverage;\n";
        assert!(mentions_module(text, "coverage_receipt"));
        assert!(reexports_module(text, "coverage_receipt"));
        assert!(!mentions_outside_reexport(text, "coverage_receipt"));
    }

    #[test]
    fn plain_use_in_root_file_is_reach() {
        let text = "use bod_app::host_handshake::{HostServer, default_descriptor_path};\n";
        assert!(mentions_outside_reexport(text, "host_handshake"));
        assert!(!reexports_module(text, "host_handshake"));
    }

    /// 字符串字面量里的模块名不是引用。
    ///
    /// 这条是实测出来的：`p4_coverage.rs` 用 `LIB.contains("pub mod coverage_receipt;")`
    /// 做漂移检查，结果把 1,424 行的孤岛算成「可达」——漂移检查的存在
    /// 反而掩盖了「没有任何调用方」这个事实。
    #[test]
    fn module_name_inside_string_literal_is_not_a_reference() {
        assert!(!mentions_module(
            r#"assert!(LIB.contains("pub mod store;"));"#,
            "store"
        ));
        assert!(!mentions_module(
            r#"let text = "pub use crate::store::open;";"#,
            "store"
        ));
        // 字符串外的真实调用仍要算。
        assert!(mentions_module(
            r#"assert!(store::open("x").is_ok());"#,
            "store"
        ));
    }

    #[test]
    fn strip_keeps_code_and_drops_string_bodies() {
        assert_eq!(strip_string_literals(r#"a("b::c") + d::e"#), "a() + d::e");
        assert_eq!(strip_string_literals(r#"x = "unterminated"#), "x = ");
        assert_eq!(
            strip_string_literals(r#"escaped = "a\"b" + c"#),
            "escaped =  + c"
        );
    }
}
