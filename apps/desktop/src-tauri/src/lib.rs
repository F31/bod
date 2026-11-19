//! bod-desktop：Tauri 2 桌面壳（P0 任务 8.1）。
//!
//! 分层：
//! - `connection`：`connect_or_spawn`、握手取地址、`close_action=detach` 的决策与副作用记录，
//!   含真实实现（`StdHostProbe` / `StdHostSpawner` / `StdHostControl`）与不依赖 Tauri 的
//!   单元测试；这是 8.1 的验收对象。
//! - 窗口接线（Tauri 2 + `frontendDist`）**未落地**：加入 `tauri` 依赖后，
//!   本机 cargo 在 435 包的依赖图上持续停滞（20 分钟无输出、无 rustc 子进程），
//!   为保持工作区可构建已回退依赖。缺口与实测证据见 `docs/spec-notes/p0-desktop.md` §2。
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
