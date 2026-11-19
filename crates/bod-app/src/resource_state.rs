//! 应用资源状态入口（P0 D1 任务 2.4—2.5）。
//! 只包装 bod-core 的条件更新与状态投影规则，不直接写 DB。

pub use crate::domain::model::{
    AcceptanceCriteria, Condition, ConditionReason, ConditionStatus, ControlChange, Issue,
    Observation, Phase, ResourceMeta, ResourceStatus, WriterKind,
};
pub use crate::domain::rules::{apply_observation, should_retry, update_control};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DomainId, Version};

    fn issue() -> Issue {
        Issue {
            meta: ResourceMeta {
                uid: DomainId::new("run-1"),
                company_id: DomainId::new("company-default"),
                project_id: Some(DomainId::new("project-1")),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: crate::domain::IssueKind::General,
            repository: None,
            spec: crate::domain::IssueSpec {
                title: "任务".to_string(),
                assignment: None,
                acceptance: AcceptanceCriteria {
                    criteria: vec!["有证据".to_string()],
                },
            },
            status: ResourceStatus {
                phase: Phase::Pending,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        }
    }

    fn observation(reason: ConditionReason, phase: Phase) -> Observation {
        Observation {
            phase,
            reason,
            message: "人读说明，不参与权限判定".to_string(),
            source_version: "obs-1".to_string(),
            observed_at: "2026-11-19T00:00:00Z".to_string(),
        }
    }

    mod invariant_i_27 {
        use super::*;

        #[test]
        fn concurrent_update_with_stale_version_is_rejected() {
            let current = issue();
            let err = update_control(
                &current,
                Version(99),
                &ControlChange {
                    title: Some("改名".to_string()),
                    acceptance: None,
                },
                WriterKind::DomainCommand,
            )
            .expect_err("过期版本必须拒绝");
            assert_eq!(
                err,
                crate::domain::DomainError::VersionConflict {
                    current: Version::INITIAL
                }
            );
        }

        #[test]
        fn observation_changes_status_but_not_generation() {
            let next = apply_observation(
                &issue(),
                &observation(ConditionReason::Progressing, Phase::Running),
                WriterKind::RunController,
            )
            .expect("控制器可写状态");
            assert_eq!(next.meta.generation, 1);
            assert_eq!(next.status.observed_generation, 1);
            assert_eq!(next.status.phase, Phase::Running);
            assert_eq!(next.meta.resource_version, Version(2));
            assert_eq!(
                next.status.conditions[0].observed_generation, 1,
                "observedGeneration 只表示协调处理过该代目标"
            );
        }

        #[test]
        fn running_run_rejects_acceptance_change() {
            let running = apply_observation(
                &issue(),
                &observation(ConditionReason::Progressing, Phase::Running),
                WriterKind::RunController,
            )
            .expect("先进入 Running");
            let err = update_control(
                &running,
                running.meta.resource_version,
                &ControlChange {
                    title: None,
                    acceptance: Some(AcceptanceCriteria { criteria: vec![] }),
                },
                WriterKind::DomainCommand,
            )
            .expect_err("运行中不得改验收标准");
            assert_eq!(err, crate::domain::DomainError::ImmutableDuringRun);
            assert!(!running.spec.acceptance.criteria.is_empty());
        }

        #[test]
        fn control_update_bumps_generation_only_for_spec_change() {
            let next = update_control(
                &issue(),
                Version::INITIAL,
                &ControlChange {
                    title: Some("新标题".to_string()),
                    acceptance: None,
                },
                WriterKind::DomainCommand,
            )
            .expect("合法控制字段可改");
            assert_eq!(next.meta.generation, 2);
            assert_eq!(next.spec.title, "新标题");
        }
    }

    mod state_write_ownership {
        use super::*;

        #[test]
        fn executor_cannot_write_attempt_status() {
            let err = apply_observation(
                &issue(),
                &observation(ConditionReason::Progressing, Phase::Succeeded),
                WriterKind::Executor,
            )
            .expect_err("执行端自报不能成为权威状态");
            assert_eq!(err, crate::domain::DomainError::WrongWriter);
        }

        #[test]
        fn verifier_can_record_observation() {
            let next = apply_observation(
                &issue(),
                &observation(ConditionReason::Progressing, Phase::Running),
                WriterKind::Verifier,
            )
            .expect("验证器可写验证观察");
            assert_eq!(next.status.phase, Phase::Running);
        }
    }

    mod stable_stop_states {
        use super::*;

        #[test]
        fn approval_and_unknown_do_not_hot_retry() {
            for reason in [
                ConditionReason::AwaitingApproval,
                ConditionReason::NoEligibleAgent,
                ConditionReason::InsufficientBudget,
                ConditionReason::DependencyNotAccepted,
                ConditionReason::ExternalOutcomeUnknown,
            ] {
                assert!(reason.is_stable_stop());
                let pending = issue();
                let blocked = apply_observation(
                    &pending,
                    &observation(reason, Phase::Blocked),
                    WriterKind::RunController,
                )
                .expect("稳定停止是合法状态");
                assert!(!should_retry(&blocked), "{reason:?} 不得热重试");
            }
        }

        #[test]
        fn succeeded_does_not_rerun_even_with_running_desire() {
            let succeeded = apply_observation(
                &issue(),
                &observation(ConditionReason::Recovered, Phase::Succeeded),
                WriterKind::RunController,
            )
            .expect("终态可写");
            assert!(!should_retry(&succeeded));
        }

        #[test]
        fn progressing_still_retries() {
            let running = apply_observation(
                &issue(),
                &observation(ConditionReason::Progressing, Phase::Running),
                WriterKind::RunController,
            )
            .expect("运行中观察可写");
            assert!(should_retry(&running));
        }
    }
}
