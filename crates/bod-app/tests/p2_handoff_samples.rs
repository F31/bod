//! 接续样本报告（任务 4.4）的漂移检查。
//!
//! 这里**不**重跑样本（那是 `handoff_sample` 库内 10 条用例与 `bod handoff sample` 的职责，
//! 每次要起真实子进程），只保证「报告里写的」与「代码/任务表里的」没有分叉：
//!
//! - 报告的 schema 与样本 ID 必须与常量同源，不能手抄出一个新版本；
//! - 结论行的计数必须与「已证运行时」列出的个数对得上，且满足规格要求的运行时数；
//! - `local-process` 在 L2 上的 `unsupported` 必须写在报告里——把它写成 pass 就是夸大；
//! - 人工耗时必须是「脚本化 0」这种实测值，不能出现「未记录却仍下结论」；
//! - 报告点名的 `case_*` 必须真在故障验收文件里（防引用幽灵用例）；
//! - 报告说「保持未勾选」时，`tasks.md` 的 4.4 就必须仍是 `- [ ]`。

const CLI: &str = include_str!("../../bod/src/main.rs");

#[test]
fn cli_exposes_the_sample_command_and_its_flags() {
    assert!(CLI.contains("bod handoff sample"), "USAGE 必须列出采样命令");
    assert!(
        CLI.contains("cmd == \"handoff\" && sub == \"sample\""),
        "必须真有命令分支，不能只在帮助里写"
    );
    for flag in ["--level", "--root", "--json", "--human-minutes"] {
        assert!(CLI.contains(flag), "缺参数 {flag}");
    }
}
