//! bod-app：可信应用层。
//!
//! 边界（G-1）：依赖 bod-core，承载事务与 IO 端口；不得反向依赖 CLI（bod）或桌面壳（bod-desktop）。
//! P0 阶段只建立边界占位；命令入口、UnitOfWork 等在 D1 起按 spec delta 落地。

pub use bod_core as domain;

pub mod adapter_conformance;
pub mod artifact_acceptance;
pub mod audit_log;
pub mod authorization;
pub mod backoff;
pub mod bottleneck;
pub mod budget_admission;
pub mod budget_dispatch_gate;
pub mod cancel_reconciliation;
pub mod canonical_json;
pub mod capability_dispatch;
pub mod capacity_evaluation;
pub mod cas;
pub mod catalog_upgrade;
pub mod claim_concurrency;
pub mod clarity_dialogue;
pub mod cleanup_recovery;
pub mod coding_integration;
pub mod collaboration_plan;
pub mod config;
pub mod conformance_report;
pub mod context_handoff;
pub mod continuation_card;
pub mod control_api;
pub mod coverage_receipt;
pub mod delivery_api;
pub mod delivery_idempotency;
pub mod delivery_link;
pub mod delivery_source;
pub mod delivery_tracking;
pub mod delivery_verification;
pub mod descriptor_schema;
pub mod desktop_lifecycle;
pub mod drill;
pub mod durable_process;
pub mod effect_reconciliation;
pub mod effect_recovery;
pub mod event_stream;
pub mod evidence_publication;
pub mod extension_sdk;
pub mod fake_executor;
pub mod fault_matrix;
pub mod feedback_knowledge;
pub mod first_use;
pub mod handoff_drill;
pub mod handoff_event;
pub mod handoff_facts;
pub mod handoff_manifest;
pub mod handoff_package;
pub mod handoff_sample;
pub mod handoff_snapshot;
pub mod history_continuity;
pub mod host_api;
pub mod host_guard;
pub mod host_handshake;
pub mod http_router;
pub mod impact_revalidation;
pub mod invocation;
pub mod isolation_check;
pub mod json_text;
pub mod lifecycle_recovery;
pub mod local_process;
pub mod maintenance;
pub mod manager_transfer;
pub mod merge_queue;
pub mod migration_safety;
pub mod mobile_control;
pub mod notification_channel;
pub mod observability;
pub mod observation_redaction;
pub mod openapi;
pub mod openspec_integration;
pub mod perf_probe;
pub mod protocol_reliability;
pub mod readonly_api;
pub mod rebuild_precheck;
pub mod recovery_models;
pub mod release_acceptance;
pub mod release_governance;
pub mod remote_delegation;
pub mod remote_partition;
pub mod resource_state;
pub mod runtime_verification;
pub mod scope_boundary;
pub mod staff_continuation;
pub mod stage_manifest;
pub mod store;
pub mod takeover_intent;
pub mod takeover_review;
pub mod telemetry;
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
