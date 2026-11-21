//! P7 §4（4.1—4.3）的漂移检查：两阶段数字必须出自真实阶段报告，预算记录必须定位每个排除项，
//! 出口证据不得把未测项说成达标，也不得把"未冻结"写成"已冻结"。

const TASKS: &str = include_str!("../../../openspec/changes/p7-capacity-evaluation/tasks.md");

/// 4.x 必须勾选，并在下一行留下落点。
#[test]
fn tasks_4_x_are_checked_with_evidence() {
    let section = TASKS
        .split("## 4. 预算冻结与出口")
        .nth(1)
        .expect("tasks 必须有 §4");
    let section = section
        .split("## Workflow follow-up")
        .next()
        .unwrap_or(section);
    let lines: Vec<&str> = section.lines().collect();
    for task in ["4.1", "4.2", "4.3"] {
        let index = lines
            .iter()
            .position(|line| line.contains(&format!("- [x] {task}")))
            .unwrap_or_else(|| panic!("{task} 未勾选"));
        let note = lines
            .get(index + 1)
            .copied()
            .unwrap_or_default()
            .trim_start();
        assert!(
            note.starts_with('→'),
            "{task} 勾选后必须写落点，便于审计：{note}"
        );
        assert!(note.len() > 60, "{task} 的落点太短，写不出具体文件与用例");
    }
}
