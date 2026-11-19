//! bod-desktop：Tauri 2 桌面壳占位。
//!
//! D0 骨架阶段不引入 tauri 依赖（未被代码使用，避免无证据的重依赖）。
//! P0 任务 8.1 再接入 connect_or_spawn / 握手取地址 / close_action=detach 原型，
//! 且桌面壳只通过固定宿主 API 连接，不依赖 bod-app 重复运行控制器。

/// 桌面壳当前构建阶段标记。
pub const STAGE: &str = "P0-placeholder";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_stage_is_declared() {
        assert_eq!(STAGE, "P0-placeholder");
    }
}
