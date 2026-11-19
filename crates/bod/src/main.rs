//! bod 宿主二进制：P0 阶段只提供最小 headless 启动，供 workspace 编译与依赖守卫验证。
//! CLI 参数解析、配置 schema/explain 与真实启动在 D0 任务 1.2 起实现。

fn main() {
    println!("bod {} (headless skeleton)", env!("CARGO_PKG_VERSION"));
}
