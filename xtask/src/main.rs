//! xtask：P0 开发期门禁工具，零外部依赖。
//!
//! 当前实现：
//! - `verify --stage P0`：G-1 依赖方向守卫 + AGENTS.md 存在性校验。
//! - `migration-check`：校验全部迁移（D1 领域 + D2 队列）含必需表且不含破坏性 DDL。
//! - `bench`：跑 P7 容量基线（`perf_probe::run`），输出 Markdown，可选写文件。
//! - `--self-test`：违例注入测试，证明守卫确实能阻断违例（不是永远返回通过）。
//!
//! 守卫规则来自 V2.9.6 §37.2 与 P0 design 决策 11：
//! - bod-core 不依赖 DB/Web/Tauri/Agent SDK，也不依赖 bod-app / bod / bod-desktop / xtask。
//! - bod-app 不反向依赖 bod（CLI/HTTP 装配）或 bod-desktop。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 依赖违规记录。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
    crate_name: String,
    forbidden: String,
}

/// 每个 crate 禁止直接依赖的目标。键为 crate 名，值为禁止依赖列表。
fn forbidden_rules() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        (
            "bod-core",
            &[
                "bod-app",
                "bod",
                "bod-desktop",
                "xtask",
                "rusqlite",
                "sqlx",
                "tokio",
                "axum",
                "tauri",
                "reqwest",
            ],
        ),
        ("bod-app", &["bod", "bod-desktop"]),
    ]
}

/// 从 Cargo.toml 文本中提取 [dependencies] 段落里的依赖 crate 名。
/// 保守解析：只在 `[dependencies]` 段内取形如 `name = ...` 的行；忽略注释与空行。
/// 目的是守住依赖方向，不做完整 TOML 解析。
fn parse_dependencies(manifest: &str) -> Vec<String> {
    let mut in_dependencies = false;
    let mut deps = Vec::new();

    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let name = key.trim().trim_matches('"');
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }
    deps
}

/// 检查单个 crate 的 Cargo.toml 是否包含禁止依赖。
fn check_manifest(crate_name: &str, manifest: &str, forbidden: &[&str]) -> Vec<Violation> {
    let deps = parse_dependencies(manifest);
    let mut violations = Vec::new();
    for dep in deps {
        if forbidden.contains(&dep.as_str()) {
            violations.push(Violation {
                crate_name: crate_name.to_string(),
                forbidden: dep,
            });
        }
    }
    violations
}

fn workspace_root() -> PathBuf {
    // xtask 位于 <root>/xtask，其 CARGO_MANIFEST_DIR 的父目录即 workspace 根。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live directly under workspace root")
        .to_path_buf()
}

fn collect_violations(root: &Path) -> Vec<Violation> {
    let mut all = Vec::new();
    for (crate_name, forbidden) in forbidden_rules() {
        let manifest_path = root.join("crates").join(crate_name).join("Cargo.toml");
        let manifest = match fs::read_to_string(&manifest_path) {
            Ok(m) => m,
            Err(_) => {
                // 若布局不同（如桌面壳），按 apps 或根 xtask 兜底查找。
                let alt = if crate_name == "bod-desktop" {
                    root.join("apps/desktop/src-tauri/Cargo.toml")
                } else {
                    root.join(crate_name).join("Cargo.toml")
                };
                match fs::read_to_string(&alt) {
                    Ok(m) => m,
                    Err(_) => continue,
                }
            }
        };
        all.extend(check_manifest(crate_name, &manifest, forbidden));
    }
    all
}

fn verify_stage_p0(root: &Path) -> bool {
    let mut ok = true;

    // G-1：依赖方向。
    let violations = collect_violations(root);
    if violations.is_empty() {
        println!("[G-1] PASS 依赖方向符合 §37.2");
    } else {
        ok = false;
        println!("[G-1] FAIL 发现禁止依赖：");
        for v in &violations {
            println!("  - {} -> {}", v.crate_name, v.forbidden);
        }
    }

    // 任务 1.4：AGENTS.md 必须存在且声明当前切片与禁止行为。
    let agents = root.join("AGENTS.md");
    let agents_ok = match fs::read_to_string(&agents) {
        Ok(text) => {
            let has_slice = text.contains("当前切片");
            let has_forbidden = text.contains("禁止行为");
            if has_slice && has_forbidden {
                true
            } else {
                ok = false;
                println!("[AGENTS] FAIL 缺少“当前切片”或“禁止行为”声明");
                false
            }
        }
        Err(_) => {
            ok = false;
            println!("[AGENTS] FAIL 缺少 AGENTS.md");
            false
        }
    };
    if agents_ok {
        println!("[AGENTS] PASS 当前切片与禁止行为已声明");
    }

    // G-50：验证索引全量、真实目标与断言。
    if !g50_check(root) {
        ok = false;
    }

    ok
}

/// 遍历工作区源码，收集所有 `fn`/`mod` 名，用于确认索引里的测试路径真实存在。
fn collect_identifiers(root: &Path) -> std::collections::BTreeSet<String> {
    fn walk(dir: &Path, out: &mut std::collections::BTreeSet<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for line in text.lines() {
                    let trimmed = line.trim_start();
                    for prefix in ["fn ", "mod ", "pub fn ", "pub mod ", "async fn "] {
                        if let Some(rest) = trimmed.strip_prefix(prefix) {
                            let name: String = rest
                                .chars()
                                .take_while(|c| c.is_alphanumeric() || *c == '_')
                                .collect();
                            if !name.is_empty() {
                                out.insert(name);
                            }
                        }
                    }
                    // 处理 `pub(crate) fn` / `const fn` 等前缀组合。
                    if let Some(pos) = trimmed.find("fn ") {
                        let rest = &trimmed[pos + 3..];
                        let name: String = rest
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if !name.is_empty() {
                            out.insert(name);
                        }
                    }
                }
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    for crate_dir in ["crates", "apps", "xtask"] {
        walk(&root.join(crate_dir), &mut out);
    }
    out
}

/// G-50：索引自洽 + 规格引用全覆盖 + 测试目标真实存在。
fn g50_check(root: &Path) -> bool {
    let report = match bod_app::verification_registry::g50_report() {
        Ok(report) => report,
        Err(err) => {
            println!("[G-50] FAIL 索引无法解析：{err}");
            return false;
        }
    };

    let identifiers = collect_identifiers(root);
    let registry = match bod_app::verification_registry::parse(
        bod_app::verification_registry::shipped_registry(),
    ) {
        Ok(registry) => registry,
        Err(err) => {
            println!("[G-50] FAIL 索引无法解析：{err}");
            return false;
        }
    };
    let mut missing_targets: Vec<String> = Vec::new();
    for binding in &registry.bindings {
        if binding.status != bod_app::verification_registry::BindingStatus::Implemented {
            continue;
        }
        let Some(tail) = binding.tests.rsplit("::").next() else {
            continue;
        };
        if !identifiers.contains(tail) {
            missing_targets.push(format!("{} -> {}", binding.id, binding.tests));
        }
    }
    missing_targets.sort();

    println!("[G-50] {}", report.summary());
    for binding in registry.bindings.iter().filter(|binding| {
        binding.status != bod_app::verification_registry::BindingStatus::Implemented
    }) {
        println!("  - 未完成绑定 {}：{}", binding.id, binding.note);
    }
    for gap in &report.gaps {
        println!("  - 规格引用未登记：{gap}");
    }
    for problem in &report.problems {
        println!("  - 结构问题：{problem}");
    }
    for target in &missing_targets {
        println!("  - 测试目标不存在：{target}");
    }
    if !report.unreferenced.is_empty() {
        println!(
            "  - 提示：索引中未被规格引用的条目 {}",
            report.unreferenced.join(", ")
        );
    }

    if report.ok() && missing_targets.is_empty() {
        println!("[G-50] PASS 验证索引全量、目标真实、无重复或未知引用");
        true
    } else {
        println!("[G-50] FAIL 见上方问题");
        false
    }
}

/// 迁移脚本与其中必须出现的表。001 为 D1 领域表，002 为 D2 队列/outbox。
const MIGRATION_TABLES: &[(&str, &[&str])] = &[
    (
        "001_initial.sql",
        &[
            "events",
            "issues",
            "work_package_revisions",
            "runs",
            "attempts",
            "leases",
            "members",
            "grants",
            "config_revisions",
        ],
    ),
    (
        "002_queue.sql",
        &["work_queue", "wake_receipts", "outbox_deliveries"],
    ),
    (
        "003_budget.sql",
        &[
            "budget_limits",
            "reservations",
            "ledger_entries",
            "usage_reports",
        ],
    ),
    (
        "004_execution.sql",
        &[
            "invocations",
            "start_audits",
            "observations",
            "observation_cursors",
            "adapter_capabilities",
            "probe_results",
        ],
    ),
    (
        "005_artifacts.sql",
        &[
            "artifacts",
            "artifact_revisions",
            "artifact_dependencies",
            "verification_records",
            "waivers",
            "acceptances",
            "decisions",
            "work_package_states",
            "manifests",
            "publications",
        ],
    ),
    (
        "006_recovery.sql",
        &[
            "migration_registry",
            "backups",
            "reconciliation_cases",
            "cleanup_requests",
            "cleanup_abandons",
        ],
    ),
    // P2 切片新增迁移：先前几片只更新了 store 的计数断言，没登记到这里，
    // 于是 `migration-check` 一直报「未登记」；3.2 收口时补齐（表仍只建表建索引）。
    ("007_handoff.sql", &["handoff_snapshots"]),
    (
        "008_handoff_events.sql",
        &["handoff_events", "handoff_acceptances"],
    ),
    ("009_takeover.sql", &["takeover_intents"]),
    (
        "010_management.sql",
        &[
            "management_appointments",
            "management_scope_heads",
            "management_commands",
        ],
    ),
    (
        "011_handover_review.sql",
        &["handover_checkpoints", "takeover_confirmations"],
    ),
    ("012_history_disputes.sql", &["history_disputes"]),
    (
        "013_scope_org.sql",
        &["issue_hierarchy", "agent_org_metadata"],
    ),
    // 014 只加 trigger，不改表结构，因此必需表清单为空；登记它只为让「未登记脚本」检查放行。
    ("014_history_triggers.sql", &[]),
];

fn inspect_migration_tables<'a>(sql: &str, required: &'a [&str]) -> (Vec<&'a str>, Vec<String>) {
    let missing: Vec<&'a str> = required
        .iter()
        .copied()
        .filter(|table| !sql.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")))
        .collect();
    let destructive: Vec<String> = sql
        .lines()
        .map(str::trim)
        .filter(|line| {
            let upper = line.to_ascii_uppercase();
            upper.starts_with("DROP ") || upper.starts_with("DELETE ")
        })
        .map(str::to_string)
        .collect();
    (missing, destructive)
}

/// 真实检查全部迁移脚本：必需表覆盖且不含破坏性 DDL（任务 2.6 + D2 3.1—3.4）。
fn migration_check(root: &Path) -> bool {
    let mut ok = true;
    let mut total_tables = 0usize;

    // 目录里有脚本但未登记 = 新增迁移未被检查覆盖，必须失败而不是静默放过。
    match fs::read_dir(root.join("crates/bod-app/migrations")) {
        Ok(entries) => {
            let mut unregistered: Vec<String> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .filter(|name| name.ends_with(".sql"))
                .filter(|name| !MIGRATION_TABLES.iter().any(|(file, _)| file == name))
                .collect();
            unregistered.sort();
            if !unregistered.is_empty() {
                ok = false;
                println!(
                    "[migration-check] FAIL 未登记的迁移脚本: {}",
                    unregistered.join(", ")
                );
            }
        }
        Err(err) => {
            ok = false;
            println!("[migration-check] FAIL 无法读取迁移目录: {err}");
        }
    }

    for (file, required) in MIGRATION_TABLES {
        let path = root.join("crates/bod-app/migrations").join(file);
        let sql = match fs::read_to_string(&path) {
            Ok(sql) => sql,
            Err(_) => {
                println!("[migration-check] FAIL 缺少迁移脚本 {file}");
                ok = false;
                continue;
            }
        };

        let (missing, destructive) = inspect_migration_tables(&sql, required);
        if missing.is_empty() && destructive.is_empty() {
            total_tables += required.len();
        } else {
            ok = false;
            if !missing.is_empty() {
                println!(
                    "[migration-check] FAIL {file} 缺少表: {}",
                    missing.join(", ")
                );
            }
            if !destructive.is_empty() {
                println!(
                    "[migration-check] FAIL {file} 含破坏性语句: {}",
                    destructive.join(" | ")
                );
            }
        }
    }
    if ok {
        println!(
            "[migration-check] PASS {} 个迁移脚本含 {} 张必需表且无破坏性 DDL",
            MIGRATION_TABLES.len(),
            total_tables
        );
    }
    ok
}

/// 违例注入自测：证明 check_manifest 确实能阻断违例，而非恒真。
fn self_test() -> bool {
    let fake_core = r#"
[dependencies]
bod-app = { path = "../bod-app" }
rustyline = "1"
"#;
    let rules = forbidden_rules();
    let core_forbidden = rules
        .iter()
        .find(|(name, _)| *name == "bod-core")
        .map(|(_, f)| *f)
        .unwrap_or(&[]);
    let v = check_manifest("bod-core", fake_core, core_forbidden);
    let detected = v.iter().any(|x| x.forbidden == "bod-app");

    let clean_core = r#"
[dependencies]
serde = "1"
"#;
    let v_clean = check_manifest("bod-core", clean_core, core_forbidden);

    if detected && v_clean.is_empty() {
        println!("[self-test] PASS 违例被阻断且合规样例通过");
        true
    } else {
        println!("[self-test] FAIL 守卫未能正确区分违例与合规样例");
        false
    }
}

fn environment_note() -> String {
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{} / {} / {}", env::consts::OS, env::consts::ARCH, rustc)
}

fn commit_note() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// P7 容量基线：跑一遍测量，打印 Markdown，可选落到文件。
///
/// 返回 `false` 表示**报告本身不可作为容量决策输入**（有 Target 项或某项缺命令/样本），
/// 这是 bench 的漂移信号，不是性能结论。
fn bench(out_path: Option<&str>) -> bool {
    let report = match bod_app::perf_probe::run(&environment_note(), &commit_note()) {
        Ok(report) => report,
        Err(err) => {
            println!("[bench] FAIL 测量未完成：{err}");
            return false;
        }
    };
    let markdown = bod_app::capacity_evaluation::render_markdown(&report, &[]);
    if let Some(path) = out_path {
        if let Err(err) = fs::write(path, &markdown) {
            println!("[bench] FAIL 报告写入 {path} 失败：{err}");
            return false;
        }
        println!("[bench] 报告已写入 {path}");
    } else {
        println!("{markdown}");
    }
    println!(
        "[bench] Measured {} / Unverified {} / Target {}；可作决策输入：{}；全部目标达标：{}",
        report.measured_count(),
        report.unverified_count(),
        report.target_count(),
        if report.usable_for_decision() {
            "是"
        } else {
            "否"
        },
        if report.may_claim_all_targets() {
            "是"
        } else {
            "否"
        }
    );
    let unresolved = report.unresolved();
    if !unresolved.is_empty() {
        println!("[bench] 未能宣称达标的项：{}", unresolved.join(", "));
    }
    report.usable_for_decision()
}

fn print_help() {
    println!("usage: xtask <command> [options]");
    println!("  verify --stage P0   G-1 依赖方向与 AGENTS.md 声明校验");
    println!("  migration-check     全部迁移表覆盖与破坏性 DDL 检查");
    println!("  bench [--out PATH]  P7 容量基线实测（Markdown 报告）");
    println!("  --self-test         违例注入自测");
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let root = workspace_root();

    let passed = match args.as_slice() {
        [cmd] if cmd == "--self-test" || cmd == "self-test" => self_test(),
        [cmd] if cmd == "migration-check" => migration_check(&root),
        [cmd] if cmd == "bench" => bench(None),
        [cmd, flag, path] if cmd == "bench" && flag == "--out" => bench(Some(path.as_str())),
        [cmd, opt, stage] if cmd == "verify" && opt == "--stage" && stage == "P0" => {
            verify_stage_p0(&root)
        }
        _ => {
            print_help();
            false
        }
    };

    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dependencies_section_only() {
        let manifest = r#"
[package]
name = "x"

[dependencies]
serde = "1"
tokio = { version = "1", features = ["rt"] }

[dev-dependencies]
proptest = "1"
"#;
        let deps = parse_dependencies(manifest);
        assert_eq!(deps, vec!["serde", "tokio"]);
    }

    #[test]
    fn detects_forbidden_dependency() {
        let m = "[dependencies]\nbod-app = { path = \"../bod-app\" }\n";
        let rules = forbidden_rules();
        let core_forbidden = rules
            .iter()
            .find(|(name, _)| *name == "bod-core")
            .map(|(_, f)| *f)
            .unwrap();
        let v = check_manifest("bod-core", m, core_forbidden);
        assert!(v.iter().any(|x| x.forbidden == "bod-app"));
    }

    #[test]
    fn allows_clean_dependencies() {
        let m = "[dependencies]\nserde = \"1\"\n";
        let rules = forbidden_rules();
        let core_forbidden = rules
            .iter()
            .find(|(name, _)| *name == "bod-core")
            .map(|(_, f)| *f)
            .unwrap();
        assert!(check_manifest("bod-core", m, core_forbidden).is_empty());
    }

    #[test]
    fn self_test_passes() {
        assert!(self_test());
    }

    #[test]
    fn migration_inspector_accepts_clean_script() {
        let (_, required) = MIGRATION_TABLES[0];
        let mut sql = String::new();
        for &table in required {
            sql.push_str(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (id TEXT PRIMARY KEY);\n"
            ));
        }
        let (missing, destructive) = inspect_migration_tables(&sql, required);
        assert!(missing.is_empty());
        assert!(destructive.is_empty());
    }

    #[test]
    fn migration_inspector_detects_missing_table_and_destructive_ddl() {
        let sql = "CREATE TABLE IF NOT EXISTS events (id TEXT);\nDROP TABLE issues;\n";
        let (missing, destructive) = inspect_migration_tables(sql, &["events", "issues"]);
        assert!(missing.contains(&"issues"));
        assert_eq!(destructive, vec!["DROP TABLE issues;".to_string()]);
    }

    #[test]
    fn repository_migrations_pass() {
        let root = workspace_root();
        for (file, required) in MIGRATION_TABLES {
            let path = root.join("crates/bod-app/migrations").join(file);
            let sql = fs::read_to_string(path).expect("迁移脚本应存在");
            let (missing, destructive) = inspect_migration_tables(&sql, required);
            assert!(missing.is_empty(), "{file} 缺少表: {missing:?}");
            assert!(
                destructive.is_empty(),
                "{file} 含破坏性 DDL: {destructive:?}"
            );
        }
    }
}
