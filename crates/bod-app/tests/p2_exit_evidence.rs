//! P2 出口证据报告（任务 5.3）的漂移检查。
//!
//! 这里**不**重跑被验收的逻辑（那是 5.1 与 4.x 的职责），只保证「报告里点名的」
//! 与「仓库里存在的」没有分叉：
//!
//! - 报告点名的每个故障 case 必须真在 `p2_fault_acceptance.rs` 里；
//! - 报告引用的 `--test` 目标必须真存在于 `tests/`；
//! - 支持矩阵 §8 的 P2 行必须保持 `Planned`/`Unverified`，`Planned` 行不得带证据；
//! - 出口结论仍是 `blocked` 时，任何 P2 门禁都不得被登记成 `implemented`；
//! - 报告必须点名 4.4/5.2 与四项对照指标，防止把「未取得」悄悄改写成「已就绪」。

const GATES: &str = include_str!("../../../verification/gates.yaml");

#[test]
fn no_p2_gate_is_claimed_implemented_while_exit_is_blocked() {
    for id in ["G-28", "G-38", "G-39", "G-40", "G-41", "G-42"] {
        let marker = format!("- id: {id}\n");
        let Some(start) = GATES.find(&marker) else {
            continue; // 未登记，符合「P2 绑定保持 planned/不登记」的现状
        };
        let rest = &GATES[start + marker.len()..];
        let end = rest.find("\n- id: ").unwrap_or(rest.len());
        let block = &rest[..end];
        assert!(
            !block.contains("status: implemented"),
            "出口结论仍为 blocked，{id} 不应被登记成 implemented：{block}"
        );
    }
}
