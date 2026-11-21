//! 守卫：JSON 字符串字面量只允许一份实现（CR-001）。
//!
//! 判据：**生产代码**里出现 JSON 双反斜杠转义字面量（源码形态 `"\\\\"`，即
//! 两个反斜杠的字符串），就说明有人又在手写 JSON 转义。CR-001 的根因不是某一份
//! 转义表漏了字符，而是同一份错误代码被复制到三处、各自演进；删掉之后必须拦住第四份，
//! 否则半年后又会以「这里特殊」的名义长出来。
//!
//! 放行的三处，都有明确理由：
//! - `canonical_json.rs`：做规范编码要控制字节与键序，必须自带转义，不能换 serde_json；
//! - `*/tests/` 下的文件：断言里要写期望输出，天然会含这个字面量；
//! - 单文件 `mod tests` 之后的段落：同上，测试模块按惯例在文件末尾。
//!
//! 未跟踪的情形（已知并接受）：若有人把转义写成 `.replace('\\', r"\\")`（raw string），
//! 本守卫不会命中。raw string 写 JSON 转义本身就很别扭，且同样逃不过 review；这里不为了
//! 极端写法把判据复杂化——守卫的目标是拦住「复制粘贴现成的一段」。

use std::path::{Path, PathBuf};

/// JSON 转义里「一个反斜杠」的源码形态。
///
/// 源码写 `.replace('\\', "\\\\")` 时，第二个参数的字面量文本就是 `"\\\\"`。
const ESCAPED_BACKSLASH_LITERAL: &str = r#""\\\\""#;

/// 允许保留自有转义实现的文件（按文件名）。
const ALLOWED_FILES: &[&str] = &["canonical_json.rs"];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR 是 crates/bod-app，上跳两级即仓库根。
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("仓库根目录存在")
        .to_path_buf()
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                // 构建产物与 node_modules 不扫；tests 目录由调用方整体跳过。
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "target" | "node_modules" | "dist" | ".git") {
                    continue;
                }
                if name == "tests" {
                    continue;
                }
                collect_rs(&path, out);
            }
            Ok(kind) if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") => {
                out.push(path);
            }
            _ => {}
        }
    }
}

/// 扫描一个文件，返回命中行（1-based）与行内容。
fn offenders_in(path: &Path) -> Vec<(usize, String)> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if ALLOWED_FILES.contains(&name) {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    // 测试模块之后的段落放行：断言里写期望输出是正当用法。
    let tests_start = text
        .lines()
        .position(|line| {
            let trimmed = line.trim();
            trimmed == "mod tests {" || trimmed == "pub mod tests {"
        })
        .unwrap_or(usize::MAX);

    text.lines()
        .enumerate()
        .filter(|(index, line)| {
            *index < tests_start
                && line.contains(ESCAPED_BACKSLASH_LITERAL)
                // 注释里提到这个字面量不算实现。
                && !line.trim_start().starts_with("//")
        })
        .map(|(index, line)| (index + 1, line.trim().to_string()))
        .collect()
}

#[test]
fn json_escaping_has_exactly_one_implementation() {
    let root = repo_root();
    let mut files = Vec::new();
    for top in ["crates", "xtask", "apps"] {
        collect_rs(&root.join(top), &mut files);
    }
    assert!(
        files.len() > 100,
        "扫描到的源文件只有 {} 个，路径推断可能错了（root={}）",
        files.len(),
        root.display()
    );

    let mut hits = Vec::new();
    for file in &files {
        for (line, text) in offenders_in(file) {
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            hits.push(format!("{rel}:{line}: {text}"));
        }
    }

    assert!(
        hits.is_empty(),
        "发现 {} 处手写 JSON 转义，请改用 bod_app::json_text::json_string：\n{}",
        hits.len(),
        hits.join("\n")
    );
}

/// 守卫自身必须能失败，否则它只是一段恒真的文字。
///
/// 用 `offenders_in` 跑一个临时文件：写入一行手写转义，断言被识别。
/// 临时目录用 `std::env::temp_dir()` + 进程 id，避免并发测试互相覆盖。
#[test]
fn guard_detects_a_planted_offender() {
    let dir = std::env::temp_dir().join(format!("bod-json-guard-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    let planted = dir.join("planted.rs");
    std::fs::write(
        &planted,
        "fn f(value: &str) -> String {\n    value.replace('\\\\', \"\\\\\\\\\")\n}\n",
    )
    .expect("写入临时文件");

    let hits = offenders_in(&planted);
    assert_eq!(hits.len(), 1, "应当恰好命中一行，实际：{hits:?}");
    assert_eq!(hits[0].0, 2, "命中的应当是第 2 行");

    // 测试模块之后不再报，避免断言期望输出时误伤。
    let with_tests = dir.join("with_tests.rs");
    std::fs::write(
        &with_tests,
        "fn f() {}\nmod tests {\n    const EXPECTED: &str = \"\\\\\\\\\";\n}\n",
    )
    .expect("写入临时文件");
    assert!(offenders_in(&with_tests).is_empty(), "测试模块应放行");

    let _ = std::fs::remove_dir_all(&dir);
}
