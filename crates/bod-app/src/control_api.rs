//! P0 D5 最小控制 API（任务 6.5，§16.1、G-21、I-55）。
//!
//! 只做三件事：**准入（403）→ 条件更新（409）→ 有副作用的命令才触碰派发**。
//! 只读查询路径不改变资源版本、不写事件、不调用派发探针。

use crate::artifact_acceptance::{self, AcceptOutcome, AcceptRequest, AcceptanceStoreError};
use crate::domain::artifact::AcceptancePolicy;
use crate::domain::model::{
    ControlChange, Denial, DenialReason, Grant, Issue, Permission, Principal, Scope, WriterKind,
};
use crate::domain::rules::{authorize, update_control};
use crate::domain::{DomainError, Version};
use crate::store::{SqliteStore, StoreError};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 稳定错误码：调用方按 `stable_code` 分支，不解析人类消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Forbidden,
    NotFound,
    VersionConflict,
    /// 状态不允许该操作（运行中改验收、非法转换等）。
    Conflict,
    InvalidArgument,
    Unavailable,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::VersionConflict | Self::Conflict => 409,
            Self::InvalidArgument => 400,
            Self::Unavailable => 503,
        }
    }

    pub fn stable_code(self) -> &'static str {
        match self {
            Self::Forbidden => "BOD_FORBIDDEN",
            Self::NotFound => "BOD_NOT_FOUND",
            Self::VersionConflict => "BOD_VERSION_CONFLICT",
            Self::Conflict => "BOD_CONFLICT",
            Self::InvalidArgument => "BOD_INVALID_ARGUMENT",
            Self::Unavailable => "BOD_UNAVAILABLE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: ErrorCode,
    /// 细分原因码（例如 BOD_IMMUTABLE_DURING_RUN）；与 code 一起稳定。
    pub reason_code: String,
    pub status: u16,
    pub message: String,
    pub next_step: String,
    pub request_id: String,
}

impl ApiError {
    fn new(
        code: ErrorCode,
        reason_code: impl Into<String>,
        request_id: &str,
        message: impl Into<String>,
        next_step: impl Into<String>,
    ) -> Self {
        Self {
            code,
            reason_code: reason_code.into(),
            status: code.http_status(),
            message: message.into(),
            next_step: next_step.into(),
            request_id: request_id.to_string(),
        }
    }

    pub fn forbidden(request_id: &str, denial: &Denial) -> Self {
        Self::new(
            ErrorCode::Forbidden,
            format!("BOD_DENIED_{:?}", denial.reason).to_uppercase(),
            request_id,
            "当前主体在该范围没有所需权限",
            denial.reactivation.clone(),
        )
    }

    pub fn not_found(request_id: &str, what: &str) -> Self {
        Self::new(
            ErrorCode::NotFound,
            "BOD_NOT_FOUND",
            request_id,
            format!("未找到{what}"),
            "确认资源 ID 与所属 Company",
        )
    }

    pub fn version_conflict(request_id: &str, current: Version) -> Self {
        Self::new(
            ErrorCode::VersionConflict,
            "BOD_VERSION_CONFLICT",
            request_id,
            format!("资源版本冲突，当前版本 {}", current.0),
            "重新读取最新版本后重试；不要覆盖他人已提交的变更",
        )
    }

    pub fn conflict(request_id: &str, reason_code: &str, message: &str, next_step: &str) -> Self {
        Self::new(
            ErrorCode::Conflict,
            reason_code,
            request_id,
            message,
            next_step,
        )
    }

    pub fn invalid(request_id: &str, message: &str) -> Self {
        Self::new(
            ErrorCode::InvalidArgument,
            "BOD_INVALID_ARGUMENT",
            request_id,
            message,
            "修正请求参数后重试",
        )
    }

    pub fn unavailable(request_id: &str, message: impl Into<String>) -> Self {
        Self::new(
            ErrorCode::Unavailable,
            "BOD_UNAVAILABLE",
            request_id,
            message,
            "稍后重试；若持续失败请查看 doctor 输出",
        )
    }

    pub fn to_json(&self) -> String {
        serde_json::json!({
            "error": {
                "code": self.code.stable_code(),
                "reason": self.reason_code,
                "status": self.status,
                "message": self.message,
                "next_step": self.next_step,
                "request_id": self.request_id,
            }
        })
        .to_string()
    }
}

impl From<StoreError> for ApiError {
    fn from(value: StoreError) -> Self {
        Self::unavailable("", value.to_string())
    }
}

impl From<AcceptanceStoreError> for ApiError {
    fn from(value: AcceptanceStoreError) -> Self {
        match value {
            AcceptanceStoreError::Store(err) => Self::unavailable("", err.to_string()),
            AcceptanceStoreError::Domain(err) => Self::conflict(
                "",
                err.code(),
                &err.to_string(),
                "按稳定原因码修正后重新提交",
            ),
            AcceptanceStoreError::PointerConflict { .. } => Self::new(
                ErrorCode::VersionConflict,
                "BOD_ACCEPTANCE_POINTER_CONFLICT",
                "",
                "接受指针已被并发更新",
                "重新读取当前接受指针后重试",
            ),
            AcceptanceStoreError::NotFound(what) => Self::not_found("", what),
        }
    }
}

/// 请求上下文：每个命令都必须携带稳定 request_id 供去重与审计。
#[derive(Debug, Clone)]
pub struct RequestContext<'a> {
    pub request_id: &'a str,
    pub actor: &'a Principal,
    pub scope: &'a Scope,
}

/// 派发探针：只读路径必须保持 0；任何把工作交给执行端的命令必须 +1。
#[derive(Debug, Default)]
pub struct DispatchProbe {
    count: AtomicUsize,
}

impl DispatchProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    fn record(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueView {
    pub uid: String,
    pub company_id: String,
    pub project_id: Option<String>,
    pub title: String,
    pub phase: String,
    pub resource_version: u64,
    pub acceptance: Vec<String>,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchReceipt {
    pub queue_key: String,
    /// 相同 generation 重复请求不新建消费者。
    pub already_queued: bool,
}

pub struct ControlApi {
    probe: DispatchProbe,
}

impl Default for ControlApi {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlApi {
    pub fn new() -> Self {
        Self {
            probe: DispatchProbe::new(),
        }
    }

    /// 供测试与观测读取：只读请求不应让该值增长。
    pub fn dispatch_probe(&self) -> &DispatchProbe {
        &self.probe
    }

    /// 只读查询：不改变状态、不写事件、不触碰派发探针。
    pub fn read_issue(
        &self,
        store: &mut SqliteStore,
        ctx: &RequestContext<'_>,
        issue_uid: &str,
    ) -> Result<IssueView, ApiError> {
        let issue = store
            .transaction()
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?
            .get_issue(issue_uid)
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?
            .ok_or_else(|| ApiError::not_found(ctx.request_id, " Issue"))?;
        self.check_scope(ctx, &issue)?;
        Ok(view_of(&issue))
    }

    /// 只读查询当前生效接受：同样零副作用。
    pub fn read_acceptance(
        &self,
        store: &mut SqliteStore,
        ctx: &RequestContext<'_>,
        artifact_id: &str,
    ) -> Result<Option<crate::domain::artifact::AcceptanceRecord>, ApiError> {
        artifact_acceptance::current_acceptance(store, &id(artifact_id))
            .map_err(|err| with_request(err.into(), ctx.request_id))
    }

    /// 有授权的控制命令：准入 403 → 条件更新 409。
    pub fn update_title(
        &self,
        store: &mut SqliteStore,
        ctx: &RequestContext<'_>,
        issue_uid: &str,
        title: &str,
        expected: Version,
        grants: &[Grant],
    ) -> Result<IssueView, ApiError> {
        let uow = store
            .transaction()
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;
        let issue = uow
            .get_issue(issue_uid)
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?
            .ok_or_else(|| ApiError::not_found(ctx.request_id, " Issue"))?;
        self.check_scope(ctx, &issue)?;
        authorize(
            ctx.actor,
            &issue.scope(),
            Permission::ManageScope,
            grants,
            issue.spec.assignment.as_ref(),
        )
        .map_err(|denial| ApiError::forbidden(ctx.request_id, &denial))?;

        if title.trim().is_empty() {
            return Err(ApiError::invalid(ctx.request_id, "标题不能为空"));
        }
        let next = update_control(
            &issue,
            expected,
            &ControlChange {
                title: Some(title.to_string()),
                acceptance: None,
            },
            WriterKind::DomainCommand,
        )
        .map_err(|err| map_domain_error(ctx.request_id, err, &issue))?;

        uow.update_issue_conditional(&next, expected)
            .map_err(|err| match err {
                StoreError::VersionConflict { current } => {
                    ApiError::version_conflict(ctx.request_id, current)
                }
                other => ApiError::unavailable(ctx.request_id, other.to_string()),
            })?;
        uow.commit()
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;
        Ok(view_of(&next))
    }

    /// 接受命令：Accept 权限 + 绑定确定修订；拒绝时返回稳定原因码。
    #[allow(clippy::too_many_arguments)]
    pub fn accept_revision(
        &self,
        store: &mut SqliteStore,
        ctx: &RequestContext<'_>,
        artifact_id: &str,
        request: &AcceptRequest,
        policy: &AcceptancePolicy,
        grants: &[Grant],
        now_ms: i64,
    ) -> Result<AcceptOutcome, ApiError> {
        authorize(ctx.actor, ctx.scope, Permission::Accept, grants, None)
            .map_err(|denial| ApiError::forbidden(ctx.request_id, &denial))?;

        artifact_acceptance::accept(
            store,
            ctx.scope.company_id.as_str(),
            &id(artifact_id),
            request,
            ctx.actor,
            policy,
            now_ms,
        )
        .map_err(|err| with_request(err.into(), ctx.request_id))
    }

    /// 派发命令：唯一会触碰探针的路径；同 generation 幂等。
    pub fn request_dispatch(
        &self,
        store: &mut SqliteStore,
        ctx: &RequestContext<'_>,
        issue_uid: &str,
        grants: &[Grant],
        now_ms: i64,
    ) -> Result<DispatchReceipt, ApiError> {
        let uow = store
            .transaction()
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;
        let issue = uow
            .get_issue(issue_uid)
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?
            .ok_or_else(|| ApiError::not_found(ctx.request_id, " Issue"))?;
        self.check_scope(ctx, &issue)?;
        authorize(
            ctx.actor,
            &issue.scope(),
            Permission::Dispatch,
            grants,
            issue.spec.assignment.as_ref(),
        )
        .map_err(|denial| ApiError::forbidden(ctx.request_id, &denial))?;

        let queue_key = format!("issue:{}", issue_uid);
        let existing = uow
            .get_queue(ctx.scope.company_id.as_str(), &queue_key)
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;

        if let Some(entry) = existing {
            if entry.wanted_generation >= issue.meta.generation {
                // 已有覆盖该代的消费者：不新建第二个，也不重复派发。
                return Ok(DispatchReceipt {
                    queue_key,
                    already_queued: true,
                });
            }
            let mut next = entry;
            next.wanted_generation = issue.meta.generation;
            next.state = crate::domain::queue::QueueState::Pending;
            next.next_due_at_ms = now_ms;
            uow.put_queue(&next)
                .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;
            uow.commit()
                .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;
            self.probe.record();
            return Ok(DispatchReceipt {
                queue_key,
                already_queued: false,
            });
        }

        let entry = crate::domain::queue::WorkQueueEntry {
            company_id: ctx.scope.company_id.clone(),
            queue_key: queue_key.clone(),
            state: crate::domain::queue::QueueState::Pending,
            wanted_generation: issue.meta.generation,
            claimed_generation: 0,
            completed_generation: 0,
            next_due_at_ms: now_ms,
            attempts: 0,
            lease_owner: None,
            lease_token: 0,
            lease_expires_at_ms: None,
            last_error: None,
            blocked_reason: None,
        };
        uow.put_queue(&entry)
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;
        uow.commit()
            .map_err(|err| ApiError::unavailable(ctx.request_id, err.to_string()))?;
        self.probe.record();
        Ok(DispatchReceipt {
            queue_key,
            already_queued: false,
        })
    }

    fn check_scope(&self, ctx: &RequestContext<'_>, issue: &Issue) -> Result<(), ApiError> {
        if ctx.scope.company_id != issue.meta.company_id
            || matches!(
                (&ctx.scope.project_id, &issue.meta.project_id),
                (Some(requested), Some(actual)) if requested != actual
            )
        {
            // 越界与不存在返回同一错误，避免泄露其他 Company 的存在性。
            return Err(ApiError::forbidden(
                ctx.request_id,
                &Denial {
                    reason: DenialReason::ScopeMismatch,
                    actor: ctx.actor.id.clone(),
                    scope: Box::new(ctx.scope.clone()),
                    grant_revision: None,
                    reactivation: "按 invocation 实际绑定 scope 重新提交".to_string(),
                },
            ));
        }
        Ok(())
    }
}

fn view_of(issue: &Issue) -> IssueView {
    IssueView {
        uid: issue.meta.uid.as_str().to_string(),
        company_id: issue.meta.company_id.as_str().to_string(),
        project_id: issue
            .meta
            .project_id
            .as_ref()
            .map(|value| value.as_str().to_string()),
        title: issue.spec.title.clone(),
        phase: format!("{:?}", issue.status.phase),
        resource_version: issue.meta.resource_version.0,
        acceptance: issue.spec.acceptance.criteria.clone(),
        owner: issue
            .spec
            .assignment
            .as_ref()
            .map(|assignment| assignment.owner.id.as_str().to_string()),
    }
}

fn id(value: &str) -> crate::domain::DomainId {
    crate::domain::DomainId::new(value)
}

fn with_request(mut error: ApiError, request_id: &str) -> ApiError {
    if error.request_id.is_empty() {
        error.request_id = request_id.to_string();
    }
    error
}

fn map_domain_error(request_id: &str, error: DomainError, issue: &Issue) -> ApiError {
    match error {
        DomainError::VersionConflict { current } => ApiError::version_conflict(request_id, current),
        DomainError::ImmutableDuringRun => ApiError::conflict(
            request_id,
            "BOD_IMMUTABLE_DURING_RUN",
            "运行中不得修改验收标准",
            "停止运行后修改，或改为提交规格修订",
        ),
        DomainError::InsufficientPermission => ApiError::new(
            ErrorCode::Forbidden,
            "BOD_DENIED_INSUFFICIENTPERMISSION",
            request_id,
            "权限不足",
            "申请所需权限后重试",
        ),
        DomainError::MissingAssignment => ApiError::conflict(
            request_id,
            "BOD_MISSING_ASSIGNMENT",
            "资源尚无可认领的 Assignment",
            "先完成指派",
        ),
        DomainError::MissingProjectContext => {
            ApiError::invalid(request_id, "Coding Issue 缺少项目或仓库上下文")
        }
        DomainError::WrongWriter | DomainError::ScopeMismatch => ApiError::conflict(
            request_id,
            "BOD_WRONG_WRITER",
            "该字段只能由指定写入者修改",
            "用对应资源类别的命令入口提交",
        ),
    }
    .tap_issue(issue)
}

/// 让错误附带资源标识，便于审计定位；不改变状态码。
trait TapIssue {
    fn tap_issue(self, issue: &Issue) -> Self;
}

impl TapIssue for ApiError {
    fn tap_issue(mut self, issue: &Issue) -> Self {
        if !issue.meta.uid.as_str().is_empty() && !self.message.contains(issue.meta.uid.as_str()) {
            self.message = format!("{}（资源 {}）", self.message, issue.meta.uid.as_str());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Phase;
    use crate::domain::artifact::{AcceptanceBinding, Artifact, ArtifactKind, ArtifactVisibility};
    use crate::domain::model::{
        AcceptanceCriteria, Assignment, IssueKind, IssueSpec, Owner, OwnerKind, PrincipalKind,
        RepositoryContext, ResourceMeta, ResourceStatus,
    };
    use crate::store::SqliteStore;

    fn store() -> (SqliteStore, std::path::PathBuf) {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("bod-api-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bod.sqlite");
        let store = SqliteStore::open(&path, &crate::config::BodConfig::default()).unwrap();
        (store, path)
    }

    fn scope() -> Scope {
        Scope {
            company_id: id("company-default"),
            project_id: Some(id("project-1")),
            issue_id: Some(id("issue-1")),
        }
    }

    fn user() -> Principal {
        Principal {
            id: id("user-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    fn grants(permission: Permission) -> Vec<Grant> {
        vec![Grant {
            principal_id: id("user-1"),
            scope: scope(),
            permission,
            revision: 1,
            active: true,
        }]
    }

    fn dispatch_grants(principal: &str, revision: u64) -> Vec<Grant> {
        vec![Grant {
            principal_id: id(principal),
            scope: scope(),
            permission: Permission::Dispatch,
            revision,
            active: true,
        }]
    }

    fn issue() -> Issue {
        Issue {
            meta: ResourceMeta {
                uid: id("issue-1"),
                company_id: id("company-default"),
                project_id: Some(id("project-1")),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: IssueKind::Coding,
            repository: Some(RepositoryContext {
                provider: "local".to_string(),
                repository_id: "repo-1".to_string(),
            }),
            spec: IssueSpec {
                title: "实现登录".to_string(),
                assignment: Some(Assignment {
                    owner: Owner {
                        kind: OwnerKind::Agent,
                        id: id("coder-1"),
                    },
                    scope: scope(),
                    grant_revision: 1,
                }),
                acceptance: AcceptanceCriteria {
                    criteria: vec!["测试通过".to_string()],
                },
            },
            status: ResourceStatus {
                phase: Phase::Pending,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        }
    }

    fn seed(store: &mut SqliteStore) {
        let uow = store.transaction().unwrap();
        uow.insert_issue(&issue()).unwrap();
        uow.commit().unwrap();
    }

    fn ctx<'a>(request_id: &'a str, actor: &'a Principal, scope: &'a Scope) -> RequestContext<'a> {
        RequestContext {
            request_id,
            actor,
            scope,
        }
    }

    #[test]
    fn read_only_returns_view_without_mutating_state_or_dispatching() {
        let (mut store, _path) = store();
        seed(&mut store);
        let api = ControlApi::new();
        let actor = user();
        let scope = scope();

        let view = api
            .read_issue(&mut store, &ctx("req-1", &actor, &scope), "issue-1")
            .unwrap();
        assert_eq!(view.title, "实现登录");
        assert_eq!(view.resource_version, 1);
        assert_eq!(api.dispatch_probe().count(), 0, "只读请求不得派发");

        // 再读一次：版本仍不变，说明没有隐式写入。
        let again = api
            .read_issue(&mut store, &ctx("req-2", &actor, &scope), "issue-1")
            .unwrap();
        assert_eq!(again.resource_version, 1);
        assert_eq!(api.dispatch_probe().count(), 0);
    }

    mod g_21 {
        use super::*;

        #[test]
        fn unauthorized_command_is_403_with_stable_code() {
            let (mut store, _path) = store();
            seed(&mut store);
            let api = ControlApi::new();
            let actor = user();
            let scope = scope();
            let empty: Vec<Grant> = Vec::new();

            let error = api
                .update_title(
                    &mut store,
                    &ctx("req-3", &actor, &scope),
                    "issue-1",
                    "新标题",
                    Version::INITIAL,
                    &empty,
                )
                .unwrap_err();
            assert_eq!(error.status, 403);
            assert_eq!(error.code.stable_code(), "BOD_FORBIDDEN");
            assert!(!error.next_step.is_empty());
            assert!(error.to_json().contains("BOD_FORBIDDEN"));
        }

        #[test]
        fn stale_version_is_409_and_keeps_original() {
            let (mut store, _path) = store();
            seed(&mut store);
            let api = ControlApi::new();
            let actor = user();
            let scope = scope();

            let error = api
                .update_title(
                    &mut store,
                    &ctx("req-4", &actor, &scope),
                    "issue-1",
                    "新标题",
                    Version(99),
                    &grants(Permission::ManageScope),
                )
                .unwrap_err();
            assert_eq!(error.status, 409);
            assert_eq!(error.code.stable_code(), "BOD_VERSION_CONFLICT");

            let view = api
                .read_issue(&mut store, &ctx("req-5", &actor, &scope), "issue-1")
                .unwrap();
            assert_eq!(view.title, "实现登录", "冲突不得覆盖原值");
        }

        #[test]
        fn authorized_update_succeeds_and_bumps_version() {
            let (mut store, _path) = store();
            seed(&mut store);
            let api = ControlApi::new();
            let actor = user();
            let scope = scope();

            let view = api
                .update_title(
                    &mut store,
                    &ctx("req-6", &actor, &scope),
                    "issue-1",
                    "实现登录（改）",
                    Version::INITIAL,
                    &grants(Permission::ManageScope),
                )
                .unwrap();
            assert_eq!(view.title, "实现登录（改）");
            assert_eq!(view.resource_version, 2);
            assert_eq!(api.dispatch_probe().count(), 0, "修改标题不派发");
        }

        #[test]
        fn out_of_scope_read_is_forbidden_not_leaked() {
            let (mut store, _path) = store();
            seed(&mut store);
            let api = ControlApi::new();
            let actor = user();
            let other = Scope {
                company_id: id("company-other"),
                project_id: None,
                issue_id: None,
            };
            let error = api
                .read_issue(&mut store, &ctx("req-7", &actor, &other), "issue-1")
                .unwrap_err();
            assert_eq!(error.status, 403);
        }

        #[test]
        fn dispatch_requires_permission_and_is_idempotent_per_generation() {
            let (mut store, _path) = store();
            seed(&mut store);
            let api = ControlApi::new();
            let scope = scope();
            // 派发必须由当前 Assignment 的 owner 本人发起（权限 ∩ 指派 ∩ 范围）。
            let coder = Principal {
                id: id("coder-1"),
                kind: PrincipalKind::Agent,
                claimed_roles: Vec::new(),
            };
            let coder_grants = dispatch_grants("coder-1", 1);
            let not_owner = user();

            let denied = api
                .request_dispatch(
                    &mut store,
                    &ctx("req-8", &not_owner, &scope),
                    "issue-1",
                    &dispatch_grants("user-1", 1),
                    NOW,
                )
                .unwrap_err();
            assert_eq!(denied.status, 403);
            assert_eq!(api.dispatch_probe().count(), 0, "被拒绝的派发不得计数");

            let stale_grant = api
                .request_dispatch(
                    &mut store,
                    &ctx("req-8b", &coder, &scope),
                    "issue-1",
                    &dispatch_grants("coder-1", 9),
                    NOW,
                )
                .unwrap_err();
            assert_eq!(stale_grant.status, 403, "旧 grant revision 不得认领新工作");
            assert_eq!(api.dispatch_probe().count(), 0);

            let first = api
                .request_dispatch(
                    &mut store,
                    &ctx("req-9", &coder, &scope),
                    "issue-1",
                    &coder_grants,
                    NOW,
                )
                .unwrap();
            assert!(!first.already_queued);
            assert_eq!(api.dispatch_probe().count(), 1);

            let second = api
                .request_dispatch(
                    &mut store,
                    &ctx("req-10", &coder, &scope),
                    "issue-1",
                    &coder_grants,
                    NOW,
                )
                .unwrap();
            assert!(second.already_queued, "同一代不新建第二个消费者");
            assert_eq!(api.dispatch_probe().count(), 1);
        }

        #[test]
        fn acceptance_command_requires_accept_permission() {
            let (mut store, _path) = store();
            seed(&mut store);
            let api = ControlApi::new();
            let actor = user();
            let scope = scope();
            let request = AcceptRequest {
                acceptance_id: id("acc-1"),
                binding: AcceptanceBinding {
                    work_package_revision: id("wp-1"),
                    input_baseline: id("baseline-1"),
                    artifact_revision: id("rev-1"),
                    verification_rules: id("rules-v1"),
                    target_environment: id("env-local"),
                    merge_combination: None,
                },
                checks_passed: true,
                blocking_risks: Vec::new(),
                reason: String::new(),
                evidence_refs: Vec::new(),
            };
            let error = api
                .accept_revision(
                    &mut store,
                    &ctx("req-11", &actor, &scope),
                    "report-1",
                    &request,
                    &AcceptancePolicy::default(),
                    &grants(Permission::Dispatch),
                    NOW,
                )
                .unwrap_err();
            assert_eq!(error.status, 403);
            assert_eq!(error.code.stable_code(), "BOD_FORBIDDEN");
        }
    }

    #[test]
    fn artifact_exists_but_not_accepted_reads_as_none() {
        let (mut store, _path) = store();
        let uow = store.transaction().unwrap();
        uow.insert_artifact(
            &Artifact {
                id: id("report-1"),
                company_id: id("company-default"),
                project_id: id("project-1"),
                kind: ArtifactKind::Report,
                title: "报告".to_string(),
                owner: id("coder-1"),
                visibility: ArtifactVisibility::Internal,
                accepted_revision: None,
            },
            NOW,
        )
        .unwrap();
        uow.commit().unwrap();

        let api = ControlApi::new();
        let actor = user();
        let scope = scope();
        let current = api
            .read_acceptance(&mut store, &ctx("req-12", &actor, &scope), "report-1")
            .unwrap();
        assert!(current.is_none());
        assert_eq!(api.dispatch_probe().count(), 0);
    }

    const NOW: i64 = 1_763_500_000_000;
}
