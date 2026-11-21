//! P5 5.3 的漂移检查：出口报告、支持矩阵与任务勾选三处不能各说各话。

use std::collections::BTreeSet;

const REPORT: &str = include_str!("../../../docs/reports/p5-exit-evidence.md");
const MATRIX: &str = include_str!("../../../docs/spec-notes/p0-support-matrix.md");
const TASKS: &str = include_str!("../../../openspec/changes/p5-team-reuse/tasks.md");

fn task_state(id: &str) -> Option<bool> {
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let (checked, rest) = if let Some(rest) = trimmed.strip_prefix("- [x] ") {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix("- [ ] ") {
            (false, rest)
        } else {
            continue;
        };
        if rest.split_whitespace().next() == Some(id) {
            return Some(checked);
        }
    }
    None
}

#[test]
fn report_states_blocked_and_names_the_open_task() {
    assert!(
        REPORT.contains("出口结论: blocked"),
        "P5 退出前结论必须是 blocked"
    );
    assert!(REPORT.contains("阻塞项: 5.2"), "阻塞项必须点名 5.2");
    assert!(
        REPORT.contains("**未勾选**"),
        "未完成的任务必须明确标为未勾选"
    );
    assert!(
        !REPORT.contains("基本完成"),
        "出口报告不得用「基本完成」代替结论"
    );
}

#[test]
fn p5_matrix_rows_are_never_supported() {
    let section = MATRIX.split("## 11. P5").nth(1).expect("矩阵必须有 P5 段");
    assert!(
        !section.contains("| Supported |"),
        "P5 段不得出现 Supported 行：真实多主体验收未跑"
    );
    assert!(
        section.contains("Unverified") && section.contains("Planned"),
        "P5 段应同时有 Unverified 与 Planned"
    );
    assert!(
        section.matches("Unverified").count() >= 4,
        "P5 段应有四条 Unverified（Catalog/权限/晋升/通道）"
    );
}

#[test]
fn open_task_stays_unchecked_while_the_real_acceptance_is_absent() {
    assert_eq!(
        task_state("5.2"),
        Some(false),
        "5.2 需真实多主体授权与隔离验收，不得勾选"
    );
    for id in ["5.1", "5.3"] {
        assert_eq!(task_state(id), Some(true), "任务 {id} 应已勾选");
    }
    for id in ["1.1", "1.6", "2.1", "2.7", "3.1", "3.7", "4.1", "4.5"] {
        assert_eq!(task_state(id), Some(true), "任务 {id} 应已勾选");
    }
}

#[test]
fn every_cited_target_exists_on_disk() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for target in [
        "p5_catalog",
        "p5_permissions",
        "p5_knowledge",
        "p5_channel",
        "p5_gates",
    ] {
        let path = root.join("tests").join(format!("{target}.rs"));
        assert!(path.is_file(), "报告引用的测试目标 {target} 不存在");
    }
    for module in [
        "catalog_upgrade",
        "authorization",
        "feedback_knowledge",
        "notification_channel",
        "maintenance",
    ] {
        let path = root.join("src").join(format!("{module}.rs"));
        assert!(path.is_file(), "报告引用的模块 {module} 不存在");
    }
    for note in ["p5-catalog", "p5-permissions", "p5-knowledge", "p5-channel"] {
        let path = root
            .join("..")
            .join("..")
            .join("docs")
            .join("spec-notes")
            .join(format!("{note}.md"));
        assert!(path.is_file(), "报告引用的 spec note {note} 不存在");
    }
}

#[test]
fn report_keeps_the_rules_that_prevent_fake_green() {
    assert!(
        REPORT.contains("全部保持 `planned`"),
        "报告必须写明 I/G 绑定仍是 planned"
    );
    assert!(
        REPORT.contains("0 行 `Supported`"),
        "报告必须写明支持矩阵没有 Supported 行"
    );
    assert!(
        REPORT.contains("样本不足时不得宣布通过"),
        "报告必须写明样本不足不宣布通过"
    );
    assert!(
        REPORT.contains("G-21 的既有绑定不得被 P5 的新用例改写"),
        "报告必须写明不改写既有门禁绑定"
    );
}

#[test]
fn checked_task_ids_are_unique() {
    let mut seen = BTreeSet::new();
    for line in TASKS.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("- [x] ")
            .or_else(|| trimmed.strip_prefix("- [ ] "))
        else {
            continue;
        };
        let Some(id) = rest.split_whitespace().next() else {
            continue;
        };
        if !id.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            continue;
        }
        assert!(seen.insert(id.to_string()), "任务编号 {id} 重复");
    }
}
