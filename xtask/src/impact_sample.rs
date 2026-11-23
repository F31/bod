//! P3 任务 2.6：用**真实 git 变化**驱动 G-31 影响评估。
//!
//! 这一层只做两件事：从仓库里读出真实的模块关系与真实的接口变化，然后交给
//! [`bod_app::impact_revalidation::assess`] 计算。它不判断"改得对不对"，
//! 也不产出通过/失败结论——报告里只有区域分类、重验依据与未执行项。
//!
//! 三条自我限制：
//! 1. **不生成推测边**。命名相似、历史共现这类依据，本工具一条都不给。没有依据的
//!    "可能受影响"比"未知"更危险，因为它看起来像分析过了。
//! 2. **只认机械可判定的关系**：`use crate::x`、`use bod_app::x`、`bod_app::x::` 全路径引用。
//!    注释、字符串、宏展开、动态派发一律看不见——报告必须把这条写进未执行项。
//! 3. **不把"没连到"当"不受影响"**。不可达的已知区域进 `unknown`，这是结论不是待办。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bod_app::impact_revalidation::{
    ArtifactKind, AssessmentInput, ChangeKind, ChangeTrigger, DependencyEdge, EdgeKind,
    ImpactAssessment, RevisionId, assess,
};

/// 规则版本。口径或扫描范围一改就要改这个值，否则跨版本报告会被当成同一规则比较。
pub const RULE_VERSION: &str = "p3-impact-static/v1";

/// 一次评估的输入规模，写进报告让人能判断覆盖面。
#[derive(Debug, Clone)]
pub struct ScanStats {
    pub regions: usize,
    pub edges: usize,
    pub files: usize,
}

/// 一个真实的接口变化点。
#[derive(Debug, Clone)]
pub struct ChangedRegion {
    pub region: RevisionId,
    pub module: String,
    pub path: String,
    pub kind: ChangeKind,
    /// 变化的公开项签名（去掉前后空白）。空表示只改了实现。
    pub public_items: Vec<String>,
}

/// 一次样本的完整结果。
pub struct SampleRun {
    pub from: String,
    pub to: String,
    pub commit_subject: String,
    pub stats: ScanStats,
    pub changes: Vec<ChangedRegion>,
    pub assessments: Vec<(ChangedRegion, ImpactAssessment)>,
    /// 冻结的关系快照，报告里用来给出可核对的"谁依赖它"。
    pub edges: Vec<DependencyEdge>,
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|err| format!("无法执行 git {:?}：{err}", args))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} 失败：{}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn rev_parse(root: &Path, rev: &str) -> Result<String, String> {
    Ok(git(root, &["rev-parse", rev])?.trim().to_string())
}

/// 把仓库路径映射成区域：`crates/bod-app/src/a/b.rs` → `bod-app::a::b`。
///
/// 只认 `crates/<crate>/{src,tests}` 下的 `.rs`。其他位置（脚本、前端、桌面壳）
/// 不进图，报告里作为未覆盖范围写明。
fn region_of(path: &Path) -> Option<(String, ArtifactKind)> {
    let mut parts = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy());
    let first = parts.next()?.to_string();
    if first != "crates" {
        return None;
    }
    let krate = parts.next()?.to_string();
    let dir = parts.next()?.to_string();
    let mut module_parts: Vec<String> = parts.map(|part| part.to_string()).collect();
    if module_parts.is_empty() || !module_parts.last()?.ends_with(".rs") {
        return None;
    }
    let last = module_parts.pop()?;
    let stem = last.trim_end_matches(".rs").to_string();
    let kind = match dir.as_str() {
        "src" => ArtifactKind::Source,
        "tests" => {
            module_parts.insert(0, "tests".to_string());
            ArtifactKind::Test
        }
        _ => return None,
    };
    if stem != "lib" && stem != "main" && stem != "mod" {
        module_parts.push(stem);
    }
    let module = if module_parts.is_empty() {
        krate
    } else {
        format!("{krate}::{}", module_parts.join("::"))
    };
    Some((module, kind))
}

fn visit_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// 收集全部区域与显式依赖边。
fn build_graph(root: &Path) -> (BTreeMap<String, RevisionId>, Vec<DependencyEdge>, ScanStats) {
    let mut files = Vec::new();
    let crates_dir = root.join("crates");
    if let Ok(entries) = fs::read_dir(&crates_dir) {
        for entry in entries.flatten() {
            for sub in ["src", "tests"] {
                let dir = entry.path().join(sub);
                if dir.is_dir() {
                    visit_rs(&dir, &mut files);
                }
            }
        }
    }

    let mut regions: BTreeMap<String, RevisionId> = BTreeMap::new();
    let mut parsed: Vec<(String, PathBuf, ArtifactKind)> = Vec::new();
    for file in &files {
        let relative = file.strip_prefix(root).unwrap_or(file);
        let Some((module, kind)) = region_of(relative) else {
            continue;
        };
        let revision = RevisionId::new(kind, &module);
        regions.insert(module.clone(), revision);
        parsed.push((module, file.clone(), kind));
    }

    let mut edges = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (module, file, _) in &parsed {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for target in references(&text, module) {
            if target == *module {
                continue;
            }
            if !regions.contains_key(&target) {
                continue;
            }
            if !seen.insert((module.clone(), target.clone())) {
                continue;
            }
            // 边的两端必须用 `regions` 里的同一个 RevisionId（含 kind），
            // 否则测试文件（Test）做变化点时，Source 边永远匹配不上。
            let (Some(from), Some(to)) = (regions.get(module), regions.get(&target)) else {
                continue;
            };
            edges.push(DependencyEdge::explicit(
                from.clone(),
                to.clone(),
                EdgeKind::Requires,
                "use 声明或全路径引用",
                module,
            ));
        }
    }

    let stats = ScanStats {
        regions: regions.len(),
        edges: edges.len(),
        files: parsed.len(),
    };
    (regions, edges, stats)
}

/// 提取一个文件里引用到的同 crate / 跨 crate 模块。
///
/// 除了 `use`，还要认 `mod x;` / `pub mod x;`：`lib.rs` 的模块声明就是 crate 根对子模块的
/// 真实依赖。不认这一条，所有子模块都会落进"未知"，图看起来完整却什么也没连上。
fn references(text: &str, module: &str) -> BTreeSet<String> {
    let crate_name = module.split("::").next().unwrap_or(module);
    let mut out = BTreeSet::new();
    let local_prefix = format!("{}::", crate_name.replace('-', "_"));
    let cross = [
        ("bod_app::", "bod-app"),
        ("bod_core::", "bod-core"),
        ("bod::", "bod"),
    ];
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("use crate::") {
            if let Some(ident) = first_ident(rest) {
                out.insert(format!("{crate_name}::{ident}"));
            }
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("pub mod ")
            .or_else(|| trimmed.strip_prefix("mod "))
        {
            if let Some(ident) = first_ident(rest) {
                out.insert(format!("{module}::{ident}"));
            }
            continue;
        }
        for (needle, krate) in cross {
            let mut cursor = 0;
            while let Some(offset) = trimmed[cursor..].find(needle) {
                let value_start = cursor + offset + needle.len();
                if let Some(ident) = first_ident(&trimmed[value_start..]) {
                    out.insert(format!("{krate}::{ident}"));
                }
                cursor = value_start;
            }
        }
        // 同一 crate 内的全路径引用：`bod_app::foo::Bar::new()`。
        let mut cursor = 0;
        while let Some(offset) = trimmed[cursor..].find(&local_prefix) {
            let value_start = cursor + offset + local_prefix.len();
            if let Some(ident) = first_ident(&trimmed[value_start..]) {
                out.insert(format!("{crate_name}::{ident}"));
            }
            cursor = value_start;
        }
    }
    out
}

fn first_ident(text: &str) -> Option<&str> {
    let trimmed = text.trim_start_matches(['{', ' ']);
    let end = trimmed
        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .unwrap_or(trimmed.len());
    if end == 0 {
        None
    } else {
        Some(&trimmed[..end])
    }
}

/// 从真实 diff 里找公开项签名变化。
fn public_items_in_diff(
    root: &Path,
    from: &str,
    to: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let diff = git(
        root,
        &[
            "diff",
            "--unified=0",
            "--no-color",
            &format!("{from}..{to}"),
            "--",
            "*.rs",
        ],
    )?;
    let mut current: Option<String> = None;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current = Some(rest.to_string());
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current = None;
            continue;
        }
        let Some(path) = current.as_deref() else {
            continue;
        };
        let Some(body) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) else {
            continue;
        };
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        let trimmed = body.trim();
        if is_public_change(trimmed) {
            out.entry(path.to_string())
                .or_default()
                .push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// 公开项变化：`pub fn/struct/enum/trait/type/const/static/mod`，以及 `pub <字段>:`。
///
/// `pub(crate)` / `pub(super)` / `pub(in ...)` 不算公开——它们出了 crate 就看不见，
/// 混进来会把"内部重构"报成"接口变化"。
fn is_public_change(trimmed: &str) -> bool {
    if !trimmed.starts_with("pub ") {
        return false;
    }
    const HEAD: [&str; 8] = [
        "pub fn ",
        "pub struct ",
        "pub enum ",
        "pub trait ",
        "pub type ",
        "pub const ",
        "pub static ",
        "pub mod ",
    ];
    if HEAD.iter().any(|prefix| trimmed.starts_with(prefix)) {
        return true;
    }
    if trimmed.starts_with("pub use ") {
        return false;
    }
    // 结构体字段：`pub method_fingerprint: Option<String>,`
    trimmed.contains(':')
}

/// 跑一次样本：从 `from` 到 `to` 的真实提交。
pub fn run(root: &Path, from: &str, to: &str, now_ms: i64) -> Result<SampleRun, String> {
    let from_sha = rev_parse(root, from)?;
    let to_sha = rev_parse(root, to)?;
    let subject = git(root, &["log", "-1", "--format=%s", &to_sha])?
        .trim()
        .to_string();
    let (regions, edges, stats) = build_graph(root);

    let diffs = public_items_in_diff(root, &from_sha, &to_sha)?;
    // 没有公开项变化但文件有改动的，也算一次源码变化，否则会漏掉实现级改动。
    let touched = git(
        root,
        &[
            "diff",
            "--name-only",
            "--no-color",
            &format!("{from_sha}..{to_sha}"),
            "--",
            "*.rs",
        ],
    )?;

    let mut changes = Vec::new();
    let mut seen = BTreeSet::new();
    for path in touched.lines().filter(|line| !line.trim().is_empty()) {
        let Some((module, kind)) = region_of(Path::new(path)) else {
            continue;
        };
        if !seen.insert(module.clone()) {
            continue;
        }
        let items = diffs.get(path).cloned().unwrap_or_default();
        let change_kind = if items.is_empty() {
            ChangeKind::Source
        } else {
            ChangeKind::Interface
        };
        changes.push(ChangedRegion {
            region: RevisionId::new(kind, &module),
            module,
            path: path.to_string(),
            kind: change_kind,
            public_items: items,
        });
    }
    changes.sort_by(|left, right| left.module.cmp(&right.module));

    let known: Vec<RevisionId> = regions.values().cloned().collect();
    let mut assessments = Vec::new();
    for change in &changes {
        let trigger = ChangeTrigger {
            change_id: format!("{}:{}", &to_sha[..to_sha.len().min(8)], change.module),
            revision: change.region.clone(),
            kind: change.kind,
            description: if change.public_items.is_empty() {
                format!("{} 的实现改动（无公开项签名变化）", change.path)
            } else {
                format!("{} 的公开项变化", change.path)
            },
        };
        let input = AssessmentInput {
            old_baseline: &from_sha,
            new_baseline: &to_sha,
            rule_version: RULE_VERSION,
            known_regions: &known,
            edges: &edges,
        };
        assessments.push((change.clone(), assess(&trigger, &input, now_ms)));
    }

    Ok(SampleRun {
        from: from_sha,
        to: to_sha,
        commit_subject: subject,
        stats,
        changes,
        assessments,
        edges,
    })
}

/// 渲染 Markdown 报告。只写读数与分类，不写通过/失败。
pub fn render(run: &SampleRun) -> String {
    let mut out = String::new();
    out.push_str("# P3 G-31 真实接口变化影响评估\n\n");
    out.push_str("本报告由 `xtask impact` 从真实 git 提交生成，**不包含通过/失败结论**。\n\n");
    out.push_str(&format!(
        "- 样本：`{}` → `{}`\n",
        short(&run.from),
        short(&run.to)
    ));
    out.push_str(&format!("- 提交主题：{}\n", run.commit_subject));
    out.push_str(&format!("- 规则版本：`{RULE_VERSION}`\n"));
    out.push_str(&format!(
        "- 扫描规模：{} 个 Rust 文件、{} 个区域、{} 条显式边\n",
        run.stats.files, run.stats.regions, run.stats.edges
    ));

    let (interfaces, implementations): (Vec<_>, Vec<_>) = run
        .assessments
        .iter()
        .partition(|(change, _)| change.kind == ChangeKind::Interface);
    out.push_str(&format!(
        "- 变化点：{} 处（接口变化 {} / 实现级改动 {}）\n\n",
        run.changes.len(),
        interfaces.len(),
        implementations.len()
    ));

    if interfaces.is_empty() {
        out.push_str("本次提交没有公开项签名或字段变化，只有实现级改动，不触发接口级重验。\n\n");
    }

    for (index, (change, assessment)) in interfaces.iter().enumerate() {
        out.push_str(&format!(
            "## {}. {} —— interface\n\n",
            index + 1,
            change.module
        ));
        out.push_str(&format!("- 文件：`{}`\n", change.path));
        out.push_str(&format!("- 评估 ID：`{}`\n", assessment.assessment_id));
        out.push_str("- 公开项变化：\n");
        for item in &change.public_items {
            out.push_str(&format!("  - `{}`\n", escape(item)));
        }
        out.push_str(&format!(
            "- 区域分类：确定 {} / 可能 {} / 未知 {}\n\n",
            assessment.certain.len(),
            assessment.possible.len(),
            assessment.unknown.len()
        ));

        out.push_str("### 确定受影响（含传递依赖，只经显式边可达）\n\n");
        if assessment.certain.is_empty() {
            out.push_str("无。没有任何模块以显式 import 引用它——这不等于没人用，见未执行项。\n\n");
        } else {
            for hit in &assessment.certain {
                out.push_str(&format!("- `{}`\n", label(&hit.region)));
            }
            out.push('\n');
        }

        out.push_str("### 直接依赖边（可核对）\n\n");
        let direct: Vec<&DependencyEdge> = run
            .edges
            .iter()
            .filter(|edge| edge.to == change.region)
            .collect();
        if direct.is_empty() {
            out.push_str("无。图中没有模块显式依赖它。\n\n");
        } else {
            for edge in direct {
                out.push_str(&format!(
                    "- `{}` → `{}`（{}）\n",
                    label(&edge.from),
                    label(&edge.to),
                    edge.kind.as_str()
                ));
            }
            out.push('\n');
        }

        out.push_str("### 可能受影响\n\n");
        if assessment.possible.is_empty() {
            out.push_str("无。本工具不生成推测边：没有依据的\"可能\"会被误读成\"分析过了\"。\n\n");
        } else {
            for hit in &assessment.possible {
                out.push_str(&format!(
                    "- `{}`：{}（{}）\n",
                    label(&hit.region),
                    escape(&dedup(&hit.via).join("；")),
                    escape(&hit.note)
                ));
            }
            out.push('\n');
        }

        out.push_str("### 影响未知\n\n");
        if assessment.unknown.is_empty() {
            out.push_str("无。所有已知区域都可达。\n\n");
        } else {
            let mut buckets: BTreeMap<String, usize> = BTreeMap::new();
            for hit in &assessment.unknown {
                let krate = hit
                    .region
                    .digest
                    .split("::")
                    .next()
                    .unwrap_or("?")
                    .to_string();
                *buckets.entry(krate).or_default() += 1;
            }
            out.push_str(&format!(
                "共 {} 个已知区域不可达。这是一条结论：**不能据此认为它们不受影响**。按 crate 分布：{}\n\n",
                assessment.unknown.len(),
                buckets
                    .iter()
                    .map(|(krate, count)| format!("`{krate}` {count}"))
                    .collect::<Vec<_>>()
                    .join("、")
            ));
            for hit in assessment.unknown.iter().take(8) {
                out.push_str(&format!(
                    "- `{}`：{}\n",
                    label(&hit.region),
                    escape(&hit.note)
                ));
            }
            if assessment.unknown.len() > 8 {
                out.push_str(&format!(
                    "- 其余 {} 个同属未知，按同一规则可复算\n",
                    assessment.unknown.len() - 8
                ));
            }
            out.push('\n');
        }

        out.push_str("### 重验依据\n\n");
        out.push_str(&format!(
            "- 关系快照冻结于提交 `{}`，共 {} 条边；评估后新增的边不改变本结论（`is_stale_against` 可判过期）\n",
            short(&assessment.new_baseline),
            assessment.relation_snapshot.len()
        ));
        out.push_str(&format!(
            "- 候选区域（变化点 + 确定 + 可能）：{} 个，占已知区域 {} 个的 {:.1}%——其余不进入重验\n",
            assessment.candidates().len(),
            run.stats.regions,
            100.0 * assessment.candidates().len() as f64 / run.stats.regions.max(1) as f64
        ));
        out.push_str(&format!(
            "- 候选清单：{}\n\n",
            assessment
                .candidates()
                .iter()
                .map(|item| format!("`{}`", label(item)))
                .collect::<Vec<_>>()
                .join("、")
        ));
    }

    if !implementations.is_empty() {
        out.push_str("## 实现级改动（不展开逐区域分类）\n\n");
        out.push_str(
            "这些文件没有公开项签名或字段变化，不触发接口级重验候选；它们仍改变行为。\n\n",
        );
        out.push_str("| 模块 | 文件 | 确定 | 未知 |\n| --- | --- | --- | --- |\n");
        for (change, assessment) in implementations {
            out.push_str(&format!(
                "| `{}` | `{}` | {} | {} |\n",
                change.module,
                change.path,
                assessment.certain.len(),
                assessment.unknown.len()
            ));
        }
        out.push('\n');
    }

    out.push_str("## 未执行项（全样本）\n\n");
    out.push_str("- 只做静态 import 图：宏展开、动态派发、字符串拼出的模块名不可见\n");
    out.push_str("- 未运行下游构建与测试；本报告不构成\"重验已通过\"\n");
    out.push_str("- 跨 crate 的非 Rust 消费者（前端、脚本、Tauri 壳）不在图中\n");
    out.push_str("- 语义级行为变化（同一签名、不同含义）需人工判断，工具不覆盖\n");
    out.push_str("- 未知区域只说明\"图上看不到路径\"，不等于\"无影响\"，也不等于\"有影响\"\n");

    out
}

/// 报告里用完整模块名。`RevisionId::short()` 取 digest 前 8 字符，
/// 那是给哈希用的；这里 digest 存的是模块名，截断后会全变成 `bod-app:`。
fn label(revision: &RevisionId) -> &str {
    revision.digest.as_str()
}

fn dedup(items: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    items
        .iter()
        .filter(|item| seen.insert((*item).clone()))
        .cloned()
        .collect()
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// 报告里要放代码片段，反引号会破坏行内代码块。
fn escape(text: &str) -> String {
    text.replace('`', "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_change_detection_excludes_crate_private_items() {
        assert!(is_public_change("pub fn measure(&self) -> u8 {"));
        assert!(is_public_change("pub method_fingerprint: Option<String>,"));
        assert!(!is_public_change("pub(crate) fn internal() {}"));
        assert!(!is_public_change("pub(super) struct Local;"));
        assert!(!is_public_change("pub use crate::foo::Bar;"));
        assert!(!is_public_change("fn private() {}"));
        assert!(!is_public_change("let x = 1;"));
    }

    #[test]
    fn region_mapping_handles_lib_tests_and_nested_modules() {
        assert_eq!(
            region_of(Path::new("crates/bod-app/src/lib.rs")),
            Some(("bod-app".to_string(), ArtifactKind::Source))
        );
        assert_eq!(
            region_of(Path::new("crates/bod-app/src/a/b.rs")).map(|(module, _)| module),
            Some("bod-app::a::b".to_string())
        );
        assert_eq!(
            region_of(Path::new("crates/bod/src/main.rs")).map(|(module, _)| module),
            Some("bod".to_string())
        );
        assert_eq!(
            region_of(Path::new("crates/bod-app/tests/p4_gates.rs")).map(|(module, _)| module),
            Some("bod-app::tests::p4_gates".to_string())
        );
        assert_eq!(region_of(Path::new("apps/desktop/src/main.rs")), None);
        assert_eq!(region_of(Path::new("crates/bod-app/README.md")), None);
    }

    #[test]
    fn references_only_follow_mechanical_imports() {
        let text = "use crate::store::SqliteStore;\n// use crate::ghost::X;\nlet v = bod_app::perf_probe::run();\n";
        let refs = references(text, "bod-app");
        assert!(refs.contains("bod-app::store"));
        assert!(refs.contains("bod-app::perf_probe"));
        assert!(
            !refs.contains("bod-app::ghost"),
            "注释里的引用不算关系：{refs:?}"
        );
    }

    #[test]
    fn real_workspace_graph_is_not_empty() {
        let root = crate::workspace_root();
        let (regions, edges, stats) = build_graph(&root);
        assert!(stats.regions > 50, "区域数异常：{}", stats.regions);
        assert!(stats.edges > 50, "边数异常：{}", stats.edges);
        assert!(regions.contains_key("bod-app::impact_revalidation"));
        assert!(
            edges
                .iter()
                .any(|edge| edge.to.digest == "bod-app::impact_revalidation"),
            "没有模块依赖 impact_revalidation，图可能扫空了"
        );
    }

    #[test]
    fn real_commit_sample_is_selective_and_keeps_unknown() {
        let root = crate::workspace_root();
        // `HEAD~1` 要真实历史。CI 的 `actions/checkout` 默认浅克隆（depth 1），那时没有
        // 父提交——这条用例会在 2026-11-21 之后一直红，而原因跟 impact 分析毫无关系。
        // 报错要点名这一条，别让下一个人去翻判定逻辑。
        let run = run(&root, "HEAD~1", "HEAD", 0).unwrap_or_else(|err| {
            panic!(
                "读不到 HEAD~1（{err}）：浅克隆没有父提交。这条用例要真实历史，\
                 CI 的 actions/checkout 必须 fetch-depth: 0"
            )
        });
        assert!(run.stats.regions > 50);
        for (change, assessment) in &run.assessments {
            // 选择性：候选必须远小于已知区域，否则就是"全部重跑"。
            assert!(
                assessment.candidates().len() < run.stats.regions,
                "{} 的候选覆盖了全部区域",
                change.module
            );
            // 关系缺失必须落进 unknown，不能被静默排除：变化点之外每个区域都要有位置。
            assert_eq!(
                assessment.certain.len() + assessment.possible.len() + assessment.unknown.len() + 1,
                run.stats.regions,
                "{} 的区域分类有遗漏",
                change.module
            );
        }
    }
}
