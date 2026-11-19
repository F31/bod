//! bod-desktop：Tauri 2 桌面壳（P0 任务 8.1）。
//!
//! 分层：
//! - `connection`：`connect_or_spawn`、握手取地址、`close_action=detach` 的决策与副作用记录，
//!   含真实实现（`StdHostProbe` / `StdHostSpawner` / `StdHostControl`）与不依赖 Tauri 的
//!   单元测试；这是 8.1 的验收对象。
//! - 窗口接线（Tauri 2 + `frontendDist`）**未落地**：本机 cargo 对含 `tauri` 的
//!   大依赖图不可用（空白 crate 也能复现：解析出锁文件后在零 IO/零 CPU 下停滞），
//!   为保持工作区可构建已回退依赖。复现数据与恢复步骤见 `docs/spec-notes/p0-desktop.md` §2。
//! - `tests/detach_e2e.rs`：用真实 `bod serve` 子进程验证「句柄丢弃不杀宿主」
//!   「外部宿主拒绝 stop_host」「日志脱敏」，即 detach 的物理语义。
//!
//! 桌面壳只通过宿主写出的连接描述连到既有宿主，**不重复运行控制器**
//! （不建第二个业务 DB、不起第二套队列）。

pub mod connection;

/// 桌面壳阶段标记：连接控制器（窗口接线待补）。
pub const STAGE: &str = "P0-connect-or-spawn";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_is_declared() {
        assert_eq!(STAGE, "P0-connect-or-spawn");
    }
}
