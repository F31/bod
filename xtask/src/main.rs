//! xtask：P0 开发期门禁工具，零外部依赖。
//!
//! 当前实现：
//! - `verify --stage P0`：G-1 依赖方向守卫 + AGENTS.md 存在性校验。
//! - `migration-check`：D0 占位（迁移脚本尚未生成，明确报告占位而非假装通过）。
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

    ok
}

fn migration_check(_root: &Path) -> bool {
    // D0 阶段尚无迁移脚本；如实报告占位，不把“无迁移”当成“检查通过”。
    println!("[migration-check] PLACEHOLDER 迁移脚本在 D1 任务 2.6 生成后启用真实检查");
    true
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

fn print_help() {
    println!("usage: xtask <command> [options]");
    println!("  verify --stage P0   G-1 依赖方向与 AGENTS.md 声明校验");
    println!("  migration-check     迁移脚本检查（D1 前为占位）");
    println!("  --self-test         违例注入自测");
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let root = workspace_root();

    let passed = match args.as_slice() {
        [cmd] if cmd == "--self-test" || cmd == "self-test" => self_test(),
        [cmd] if cmd == "migration-check" => migration_check(&root),
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
}
