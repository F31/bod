//! bod-desktop：Tauri 2 桌面壳（P0 任务 8.1）。
//!
//! 分层：
//! - `connection`：`connect_or_spawn`、握手取地址、`close_action=detach` 的决策与副作用记录，
//!   不依赖 Tauri，可被单测直接驱动（这是 8.1 的验收对象）。
//! - Tauri 2 窗口接线（窗口关闭走 `close_action`、命令只返回地址不返回会话密钥）
//!   尚未落地：`cargo metadata --offline` 因依赖图需要未缓存的 `rsqlite-vfs` 失败，
//!   且 P0 没有可握手的 HTTP 宿主。缺口与证据见 `docs/spec-notes/p0-desktop.md`。
//!
//! 桌面壳只通过宿主写出的连接描述连到既有宿主，**不重复运行控制器**
//! （不建第二个业务 DB、不起第二套队列）。

pub mod connection;

/// 桌面壳阶段标记：连接控制器已完成，Tauri 窗口接线随 feature 打开。
pub const STAGE: &str = "P0-connect-or-spawn";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_is_declared() {
        assert_eq!(STAGE, "P0-connect-or-spawn");
    }
}
