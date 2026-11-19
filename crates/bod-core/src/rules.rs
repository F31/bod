//! P0 D1 纯领域规则：授权交集、直接指派、条件更新与状态写入所有权。
//! 不执行 IO；所有决策只依赖传入的当前状态与当前授权（G-1）。

use crate::model::*;
use crate::{DomainId, Version};

fn denial(reason: DenialReason, actor: &DomainId, scope: &Scope, revision: Option<u64>) -> Denial {
    let reactivation = match reason {
        DenialReason::InsufficientPermission => {
            "由具备 ManageScope 的主体在当前 scope 重新授权并提升 grant revision".to_string()
        }
        DenialReason::MissingProjectContext => "补齐 project_id 与仓库上下文后重新提交".to_string(),
        DenialReason::MissingAssignment => {
            "由具备 ManageScope 的主体指定唯一 accountable owner".to_string()
        }
        DenialReason::VersionConflict => "重新读取当前 resourceVersion 后重试".to_string(),
        DenialReason::ImmutableDuringRun => {
            "新建带确定修订的 Run，或走显式可审计的修订迁移".to_string()
        }
        DenialReason::WrongWriter => "由该数据类别的主写入者提交命令".to_string(),
        DenialReason::ScopeMismatch => "按 invocation 实际绑定 scope 重新提交".to_string(),
        DenialReason::InsufficientBudget => {
            "等待预算补充或显式提高限额后重新唤醒，不自动重试".to_string()
        }
        DenialReason::ExternalOutcomeUnknown => {
            "先 inspect/reconcile 外部 invocation 与费用，确认后再决定下一步".to_string()
        }
    };
    Denial {
        reason,
        actor: actor.clone(),
        scope: Box::new(scope.clone()),
        grant_revision: revision,
        reactivation,
    }
}

/// 只检查当前有效 Grant 交集；不要求已有 Assignment。
/// 用于指派时校验候选 owner，避免「先有 Assignment 才能指派」的自举死锁。
fn authorize_grant<'a>(
    principal: &Principal,
    requested: &Scope,
    permission: Permission,
    grants: &'a [Grant],
) -> Result<&'a Grant, Denial> {
    let mut best: Option<&Grant> = None;
    for grant in grants {
        if grant.principal_id != principal.id
            || !grant.active
            || grant.permission != permission
            || !grant.scope.contains(requested)
        {
            continue;
        }
        if best.is_none_or(|current| grant.revision > current.revision) {
            best = Some(grant);
        }
    }
    best.ok_or_else(|| {
        denial(
            DenialReason::InsufficientPermission,
            &principal.id,
            requested,
            None,
        )
    })
}

/// 权限交集：角色授权 ∩ 当前有效 Grant ∩ Assignment ∩ 即时撤权。
/// `principal.claimed_roles` 与 ownerReference 均不参与判定。
pub fn authorize(
    principal: &Principal,
    requested: &Scope,
    permission: Permission,
    grants: &[Grant],
    assignment: Option<&Assignment>,
) -> Result<Authorization, Denial> {
    let grant = authorize_grant(principal, requested, permission, grants)?;

    if permission == Permission::Dispatch {
        let assignment = assignment.ok_or_else(|| {
            denial(
                DenialReason::MissingAssignment,
                &principal.id,
                requested,
                Some(grant.revision),
            )
        })?;
        let owner_matches = assignment.owner.id == principal.id
            && match principal.kind {
                PrincipalKind::Agent => assignment.owner.kind == OwnerKind::Agent,
                PrincipalKind::User => assignment.owner.kind == OwnerKind::User,
            };
        if !owner_matches
            || !assignment.scope.contains(requested)
            || assignment.grant_revision != grant.revision
        {
            return Err(denial(
                DenialReason::InsufficientPermission,
                &principal.id,
                requested,
                Some(grant.revision),
            ));
        }
    }

    Ok(Authorization {
        actor: principal.id.clone(),
        scope: requested.clone(),
        grant_revision: grant.revision,
    })
}

/// 执行反馈与主体身份分离：payload 自报的归属不得扩大 invocation 绑定范围。
pub fn authorize_feedback(
    invocation_scope: &Scope,
    payload_scope: &Scope,
    principal: &Principal,
    permission: Permission,
    grants: &[Grant],
    assignment: Option<&Assignment>,
) -> Result<Authorization, Denial> {
    if !invocation_scope.contains(payload_scope) {
        return Err(denial(
            DenialReason::ScopeMismatch,
            &principal.id,
            payload_scope,
            None,
        ));
    }
    authorize(principal, invocation_scope, permission, grants, assignment)
}

/// 直接指派：不要求 Team / TeamMembership / 组织层级存在。
pub fn assign_issue(
    issue: &Issue,
    owner: Owner,
    owner_grant_revision: u64,
    actor: &Principal,
    grants: &[Grant],
) -> Result<Issue, Denial> {
    if let Some(error) = issue.context_error() {
        let reason = match error {
            DomainError::MissingProjectContext => DenialReason::MissingProjectContext,
            _ => DenialReason::ScopeMismatch,
        };
        return Err(denial(reason, &actor.id, &issue.scope(), None));
    }
    let _ = authorize(actor, &issue.scope(), Permission::ManageScope, grants, None)?;
    let owner_principal = Principal {
        id: owner.id.clone(),
        kind: match owner.kind {
            OwnerKind::Agent => PrincipalKind::Agent,
            OwnerKind::User => PrincipalKind::User,
        },
        claimed_roles: Vec::new(),
    };
    let owner_grant = authorize_grant(
        &owner_principal,
        &issue.scope(),
        Permission::Dispatch,
        grants,
    )?;
    if owner_grant.revision != owner_grant_revision {
        return Err(denial(
            DenialReason::InsufficientPermission,
            &owner_principal.id,
            &issue.scope(),
            Some(owner_grant.revision),
        ));
    }

    let mut next = issue.clone();
    next.spec.assignment = Some(Assignment {
        owner,
        scope: issue.scope(),
        grant_revision: owner_grant.revision,
    });
    next.meta.resource_version = next.meta.resource_version.next();
    Ok(next)
}

/// 条件更新：版本不匹配时返回当前版本；只有期望配置变化才递增 generation。
pub fn update_control(
    issue: &Issue,
    expected_version: Version,
    change: &ControlChange,
    writer: WriterKind,
) -> Result<Issue, DomainError> {
    if writer != WriterKind::DomainCommand {
        return Err(DomainError::WrongWriter);
    }
    if issue.meta.resource_version != expected_version {
        return Err(DomainError::VersionConflict {
            current: issue.meta.resource_version,
        });
    }
    if issue.status.phase == Phase::Running && change.touches_immutable_spec() {
        return Err(DomainError::ImmutableDuringRun);
    }
    let mut next = issue.clone();
    if let Some(title) = &change.title {
        next.spec.title = title.clone();
    }
    if let Some(acceptance) = &change.acceptance {
        next.spec.acceptance = acceptance.clone();
    }
    next.meta.generation += 1;
    next.meta.resource_version = next.meta.resource_version.next();
    Ok(next)
}

/// 状态投影：只由控制器/验证器写入；Executor 自报只能登记为观察。
pub fn apply_observation(
    issue: &Issue,
    observation: &Observation,
    writer: WriterKind,
) -> Result<Issue, DomainError> {
    if !matches!(writer, WriterKind::RunController | WriterKind::Verifier) {
        return Err(DomainError::WrongWriter);
    }
    let mut next = issue.clone();
    next.status.phase = observation.phase;
    next.status.conditions.push(Condition {
        reason: observation.reason,
        status: if observation.reason.is_stable_stop() {
            ConditionStatus::True
        } else {
            ConditionStatus::False
        },
        message: observation.message.clone(),
        observed_generation: next.meta.generation,
    });
    next.status.observed_generation = next.meta.generation;
    next.meta.resource_version = next.meta.resource_version.next();
    Ok(next)
}

/// 稳定停止与终态：不因残留期望字段自动重试或重跑。
pub fn should_retry(issue: &Issue) -> bool {
    if matches!(issue.status.phase, Phase::Succeeded | Phase::Cancelled) {
        return false;
    }
    !issue
        .status
        .conditions
        .iter()
        .any(|condition| condition.reason.is_stable_stop())
}
