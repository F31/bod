//! bod-app：可信应用层。
//!
//! 边界（G-1）：依赖 bod-core，承载事务与 IO 端口；不得反向依赖 CLI（bod）或桌面壳（bod-desktop）。
//! P0 阶段只建立边界占位；命令入口、UnitOfWork 等在 D1 起按 spec delta 落地。

pub use bod_core as domain;

pub mod adapter_conformance;
pub mod artifact_acceptance;
pub mod authorization;
pub mod backoff;
pub mod budget_admission;
pub mod budget_dispatch_gate;
pub mod cancel_reconciliation;
pub mod canonical_json;
pub mod cas;
pub mod claim_concurrency;
pub mod clarity_dialogue;
pub mod cleanup_recovery;
pub mod coding_integration;
pub mod config;
pub mod context_handoff;
pub mod continuation_card;
pub mod control_api;
pub mod delivery_verification;
pub mod descriptor_schema;
pub mod effect_reconciliation;
pub mod effect_recovery;
pub mod fake_executor;
pub mod fault_matrix;
pub mod first_use;
pub mod handoff_event;
pub mod handoff_facts;
pub mod handoff_manifest;
pub mod handoff_package;
pub mod handoff_snapshot;
pub mod host_handshake;
pub mod invocation;
pub mod isolation_check;
pub mod lifecycle_recovery;
pub mod local_process;
pub mod manager_transfer;
pub mod migration_safety;
pub mod perf_probe;
pub mod protocol_reliability;
pub mod rebuild_precheck;
pub mod recovery_models;
pub mod resource_state;
pub mod staff_continuation;
pub mod store;
pub mod takeover_intent;
pub mod verification_registry;
pub mod wakeup;
pub mod workbench_projection;
pub mod workspace_safety;

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
