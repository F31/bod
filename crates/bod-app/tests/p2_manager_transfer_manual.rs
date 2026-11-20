//! P2 任务 3.6：负责人切换操作手册的自检。
//!
//! 手册（`docs/spec-notes/p2-manager-transfer.md` §6）声称「§41.8 的每项验收要求都有对应命令
//! 与拒绝样本」。本测试把这个声称变成可执行断言，而不是留在文字里：
//! 1. §41.8 出口的每个 ID（M1—M12）、再验证项（N1—N6）与反例（R1—R4）都必须在上表出现；
//! 2. 每行要么给出真的 `cargo test -p bod-app <module>` 命令且该模块文件存在，要么明写「未覆盖」；
//! 3. 每行引用的稳定错误码必须真的出现在 `src/` 的源码里（防止文档写一个不存在的错误）。
//!
//! 手册与代码脱节时本测试失败——这是文档任务唯一诚实的「通过」方式。

use std::fs;
use std::path::{Path, PathBuf};

const DOC: &str = include_str!("../../../docs/spec-notes/p2-manager-transfer.md");

/// (ID, 该行必须出现的错误码)
const ITEMS: &[(&str, &[&str])] = &[
    ("M1", &["ScopeAlreadyManaged", "EpochNotMonotonic"]),
    (
        "M2",
        &["ManagementIsNotExecution", "ExecutionIsNotManagement"],
    ),
    ("M3", &["RevokedIsTerminal", "EpochNotMonotonic"]),
    ("M4", &["StaleAppointment"]),
    ("M5", &["ManagementIsNotExecution"]),
    ("M6", &["RedoAccepted", "RestartRunning"]),
    ("M7", &["FeeSubjectIsImmutable"]),
    ("M8", &["ConfirmationIsNotAuthority", "CursorMismatch"]),
    ("M9", &["CrossesSpecialBoundary", "BypassAttempt"]),
    ("M10", &["AcceptanceIsImmutable", "HistoryIsAppendOnly"]),
    ("M11", &["DisputeIsNotAuthority", "DisputeIsFinal"]),
    ("M12", &["ReportsToCycle", "SelfReport"]),
    ("N1", &["MissingSummarySnapshot"]),
    ("N2", &[]),
    ("N3", &["ResendBlocked"]),
    ("N4", &[]),
    ("N5", &["IsolationBlocked", "UnevidencedBasis"]),
    ("N6", &["UnknownIsNotResolution"]),
];

/// §6.2 反例表的 ID（四列：ID / 反例 / 触发点 / 错误码）。
const NEGATIVE_ITEMS: &[&str] = &["R1", "R2", "R3", "R4"];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// `src/` 下所有 `.rs` 的正文，用于核对错误码真的存在。
fn all_source() -> String {
    let mut text = String::new();
    let entries = fs::read_dir(src_dir()).expect("应能读取 src 目录");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("rs") {
            text.push_str(&fs::read_to_string(&path).unwrap_or_default());
            text.push('\n');
        }
    }
    text
}

fn row_for(id: &str) -> Vec<String> {
    let prefix = format!("| {id} |");
    DOC.lines()
        .find(|line| line.trim_start().starts_with(&prefix))
        .unwrap_or_else(|| panic!("手册 §6 缺少 {id} 一行"))
        .split('|')
        .map(|cell| cell.trim().replace('`', ""))
        .collect()
}

#[test]
fn manual_covers_every_section_41_8_item_with_a_command_or_an_explicit_gap() {
    for heading in ["## 6.", "### 6.1", "### 6.2", "### 6.3", "### 6.4"] {
        assert!(DOC.contains(heading), "手册缺少小节 {heading}");
    }

    let source = all_source();
    for (id, codes) in ITEMS {
        let cells = row_for(id);
        // split('|') 后首尾为空串：| ID | 要求 | 命令 | 拒绝样本 | 覆盖 |
        assert!(
            cells.len() >= 7,
            "{id} 行应有 5 列，实际 {} 列：{cells:?}",
            cells.len() - 2
        );
        let command = &cells[3];
        let rejection = &cells[4];
        let coverage = &cells[5];

        if command.contains("未覆盖") || coverage.contains("未覆盖") {
            // 诚实的缺口：必须写明归属，不能只有一句「未覆盖」。
            assert!(
                coverage.contains("4.4") || coverage.contains("5.1"),
                "{id} 未覆盖时必须写明归属切片"
            );
        } else {
            assert!(
                command.contains("cargo test -p bod-app "),
                "{id} 必须给出可执行命令"
            );
            let module = command
                .split("cargo test -p bod-app ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or_default()
                .trim_end_matches(|value: char| !value.is_ascii_alphanumeric() && value != '_');
            let module_path = src_dir().join(format!("{module}.rs"));
            assert!(
                module_path.exists(),
                "{id} 引用的模块 {module} 在 src/ 下不存在"
            );
        }

        for code in *codes {
            assert!(
                rejection.contains(code),
                "{id} 的拒绝样本应包含 {code}，实际：{rejection}"
            );
            assert!(
                source.contains(code),
                "{id} 引用的错误码 {code} 在 src/ 中不存在（文档写了不存在的错误）"
            );
        }
    }
}

#[test]
fn every_row_states_a_coverage_owner() {
    // 覆盖列不能为空：要么是「已测」，要么是明确归属的「未覆盖」。
    for (id, _) in ITEMS {
        let cells = row_for(id);
        let coverage = &cells[5];
        assert!(
            !coverage.is_empty(),
            "{id} 的覆盖列不能为空（不能只说要求、不说谁负责）"
        );
    }
}

#[test]
fn negative_examples_state_the_error_that_blocks_them() {
    let source = all_source();
    for id in NEGATIVE_ITEMS {
        let cells = row_for(id);
        assert!(cells.len() >= 6, "{id} 反例行应有 4 列，实际：{cells:?}");
        let code = &cells[4];
        assert!(!code.is_empty(), "{id} 必须写出阻断它的稳定错误码");
        assert!(
            source.contains(code.as_str()),
            "{id} 的错误码 {code} 在源码中不存在"
        );
    }
}
