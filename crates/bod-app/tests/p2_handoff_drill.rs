//! 真实演练取样（4.4 人工耗时 / 5.2 对照）的漂移检查。
//!
//! 这里**不**重跑演练（那是人手按秒表的事），只保证「文档写的口径」与「代码强制的口径」
//! 以及「任务表的状态」没有分叉：
//!
//! - CLI 必须暴露 `handoff drill` 与 `--drill`，且不允许 `--drill` 与 `--human-minutes` 混用；
//! - `HumanTime` 四态与「口述值不算证据」的规则必须写在 spec note 里；
//! - 秒表两端必须是 `last_action` / `handoff_done`，且读数写成「算出」而不是「填入」；
//! - 5.2 的四项指标名必须与实现一致；
//! - 读数未取得时，`tasks.md` 的 4.4 / 5.2 必须是 `- [ ]`，出口结论必须是 `blocked`——
//!   谁把冒烟当证据去勾任务，这里先挂。

const CLI: &str = include_str!("../../bod/src/main.rs");

#[test]
fn cli_exposes_the_drill_family_and_its_evidence_flag() {
    for needle in [
        "handoff\" && sub == \"drill\"",
        "--drill",
        "handoff drill start",
        "handoff drill mark",
        "handoff drill finish",
        "handoff drill arm",
    ] {
        assert!(
            CLI.contains(needle),
            "CLI 使用说明或分发里缺 {needle}：演练入口必须可发现"
        );
    }
    assert!(
        CLI.contains("不能同时给"),
        "CLI 必须拒绝 --drill 与 --human-minutes 混用，否则分不清哪个读数进了报告"
    );
}
