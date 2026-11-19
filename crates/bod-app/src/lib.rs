//! bod-app：可信应用层。
//!
//! 边界（G-1）：依赖 bod-core，承载事务与 IO 端口；不得反向依赖 CLI（bod）或桌面壳（bod-desktop）。
//! P0 阶段只建立边界占位；命令入口、UnitOfWork 等在 D1 起按 spec delta 落地。

pub use bod_core as domain;

pub mod authorization;
pub mod backoff;
pub mod budget_admission;
pub mod claim_concurrency;
pub mod config;
pub mod effect_recovery;
pub mod resource_state;
pub mod store;
pub mod wakeup;

pub use config::{
    BodConfig, ConfigError, LoadedConfig, explain, load_from_str, schema_text, validate_shape,
};

/// 应用层的稳定错误码前缀，供控制 API 与 CLI 统一映射。
pub const ERROR_PREFIX: &str = "BOD";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_domain() {
        let id = domain::DomainId::new("project-1");
        assert_eq!(id.as_str(), "project-1");
    }
}
