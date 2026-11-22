//! 存储层读出/写入的行结构。
//!
//! 这些类型是**列的镜像**，不含业务规则：判定在 `delivery_*` 等模块里，
//! 校验在数据库的 CHECK/trigger 里。放一处是为了让多个领域的读路径共用同一份
//! 列清单，避免各写一份后互相错位。

/// 修订正文事实（P2 3.4）：历史连续比对要覆盖摘要与大小，不能只看产出者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionBodyRow {
    pub revision_id: String,
    pub artifact_id: String,
    pub produced_by: String,
    pub digest: String,
    pub size_bytes: i64,
    pub mime: String,
    pub created_at_ms: i64,
}

/// 正式 Manifest 事实（P2 3.4）：冻结的是引用集合，不含正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestBodyRow {
    pub uid: String,
    pub revision: i64,
    pub milestone: String,
    pub artifact_revision_refs: String,
    pub acceptance_refs: String,
    pub signature_ref: Option<String>,
    pub created_at_ms: i64,
}

/// 决定事实（P2 3.4）：旧经理的决定不会因换人消失或改口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionBodyRow {
    pub decision_id: String,
    pub actor_id: String,
    pub decision: String,
    pub reason: String,
    pub evidence_refs: String,
    pub revision_refs: String,
    pub created_at_ms: i64,
}

/// 争议行（`history_disputes`）：只标待核查，不改被争议的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputeRow {
    pub dispute_id: String,
    pub company_id: String,
    pub project_id: String,
    pub fact_kind: String,
    pub fact_id: String,
    pub fact_fingerprint: String,
    pub reason: String,
    pub evidence_refs: String,
    pub raised_by: String,
    pub state: String,
    pub resolution: Option<String>,
    pub resolved_by: Option<String>,
    pub resolved_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// 专项边界层级（P2 3.5）：只登记父子，不改 `issues` 表与域对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyRow {
    pub issue_uid: String,
    pub parent_issue_uid: String,
}

/// 租约事实（P2 3.5）：组织元数据变更前后逐字段比对，证明没有 fencing/迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseFactRow {
    pub resource_key: String,
    pub owner: String,
    pub lease_token: i64,
    pub expires_at: Option<String>,
    pub state: String,
}

/// 授权事实（P2 3.5）：reportsTo 变更不得改动任何一条 grant。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFactRow {
    pub principal_id: String,
    pub project_id: Option<String>,
    pub permission: String,
    pub revision: i64,
    pub active: i64,
}

/// 组织元数据行（`agent_org_metadata`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgMetadataRow {
    pub agent_id: String,
    pub reports_to: Option<String>,
    pub updated_at_ms: i64,
    pub updated_by: String,
}

/// 登记来源身份的结果。`created=false` 表示五元组已存在，返回的是已有身份——
/// 调用方据此区分「新认识的来源」与「重复导入同一对象」（3.3 的去重语义靠它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSource {
    pub source_id: String,
    pub created: bool,
}

/// 提交来源候选的结果。`created=false` 表示已有同一作用域同一 locator 的待确认候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedCandidate {
    pub candidate_id: String,
    pub created: bool,
}

/// 确认候选的结果。`source_created` 说明这个身份是不是本次确认才建立的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedCandidate {
    pub candidate_id: String,
    pub source_id: String,
    pub source_created: bool,
}

/// 追加来源修订的结果。`created=false` 表示同一事件已确认过，返回的是已有修订。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendedRevision {
    pub source_revision_id: String,
    pub created: bool,
}

/// 恢复报告用的核心行数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoreRowCounts {
    pub issues: i64,
    pub revisions: i64,
    pub invocations: i64,
    pub acceptances: i64,
}
