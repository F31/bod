//! P6 任务 3.1—3.5：只读查询与订阅适配（`readonly-adapter`，I-55 / G-34）。
//!
//! 只读的含义是**不改变业务控制状态**，不是"少写一点"。五条边界：
//! 1. **无副作用（3.1 / I-55）**：打开看板、查询、计数、导出都只产生有界计算与审计，
//!    不创建 Run、不建 Binding、不发 invocation、不启动 Agent；按钮提示只是提示，放行由领域判定。
//! 2. **返回前过滤（3.2 / AZ06 类）**：列表、搜索、计数、导出、订阅、附件都在返回内容**前**
//!    按主体、Company、Project 与客户范围过滤；不可见对象一律不可见，计数与文件名不泄露其存在。
//! 3. **订阅与游标（3.3 / G-34）**：订阅用持久游标，超限释放缓冲后按最后持久游标补拉，
//!    游标超出保留期读带基准序列的快照；每次恢复都重新鉴权，撤权立即受限。
//! 4. **缓存边界（3.4）**：投影缓存键含身份范围、输入摘要与索引/工具版本，按 TTL 与授权修订
//!    失效；Manifest、账本、租约、幂等回执与 ACK 游标永不进可淘汰集合。
//! 5. **审计可追溯（3.5）**：只读请求按请求 ID 关联主体、动作、对象范围、依据、策略修订、
//!    决定与原因；审计不保存令牌或业务密文明文。
//!
//! 本模块是纯值判断：不建连接、不读外部索引、不落表、不启动任何执行体。

use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// 共享：查看者与可见性
// ---------------------------------------------------------------------------

/// 查看者能触达的范围。任何过滤都从这四个维度出发，不靠调用方"记得过滤"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    pub principal: String,
    pub company_id: String,
    pub project_ids: BTreeSet<String>,
    /// 客户协作者只应看到一个客户。
    pub customer_id: Option<String>,
    pub ticket_ids: BTreeSet<String>,
}

impl Viewer {
    pub fn new(principal: &str, company_id: &str, project_id: &str) -> Self {
        let mut project_ids = BTreeSet::new();
        project_ids.insert(project_id.to_string());
        Self {
            principal: principal.to_string(),
            company_id: company_id.to_string(),
            project_ids,
            customer_id: None,
            ticket_ids: BTreeSet::new(),
        }
    }

    pub fn with_customer(mut self, customer_id: &str) -> Self {
        self.customer_id = Some(customer_id.to_string());
        self
    }

    pub fn with_tickets(mut self, tickets: &[&str]) -> Self {
        self.ticket_ids = tickets.iter().map(|id| id.to_string()).collect();
        self
    }

    pub fn may_view_project(&self, project_id: &str) -> bool {
        self.project_ids.contains(project_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadonlyRecord {
    pub object_id: String,
    pub company_id: String,
    pub project_id: String,
    pub customer_id: Option<String>,
    pub ticket_id: Option<String>,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Visible,
    /// 不可见：对外只有这一个结论，不给"为什么不可见"的细节。
    Invisible,
}

/// 四条范围逐层收窄：Company → Project → 客户 → 工单。
pub fn visibility_of(viewer: &Viewer, record: &ReadonlyRecord) -> Visibility {
    if record.company_id != viewer.company_id {
        return Visibility::Invisible;
    }
    if !viewer.may_view_project(&record.project_id) {
        return Visibility::Invisible;
    }
    if let Some(customer) = &viewer.customer_id
        && record.customer_id.as_deref() != Some(customer.as_str())
    {
        return Visibility::Invisible;
    }
    if !viewer.ticket_ids.is_empty()
        && let Some(ticket) = &record.ticket_id
        && !viewer.ticket_ids.contains(ticket)
    {
        return Visibility::Invisible;
    }
    Visibility::Visible
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredPage {
    pub visible: Vec<ReadonlyRecord>,
    /// 等于 `visible.len()`：总数只数看得见的，未授权对象连计数都不参与。
    pub total: usize,
    /// 被挡掉的条数：只有数量，没有身份、名称或 id。
    pub withheld: usize,
}

pub fn filter_records(viewer: &Viewer, records: &[ReadonlyRecord]) -> FilteredPage {
    let mut visible = Vec::new();
    let mut withheld = 0;
    for record in records {
        match visibility_of(viewer, record) {
            Visibility::Visible => visible.push(record.clone()),
            Visibility::Invisible => withheld += 1,
        }
    }
    FilteredPage {
        total: visible.len(),
        visible,
        withheld,
    }
}

// ---------------------------------------------------------------------------
// 3.1 只读无副作用（I-55）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadonlyAction {
    Dashboard,
    Query,
    Search,
    Count,
    Export,
    Subscribe,
    Attachment,
}

impl ReadonlyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Query => "query",
            Self::Search => "search",
            Self::Count => "count",
            Self::Export => "export",
            Self::Subscribe => "subscribe",
            Self::Attachment => "attachment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyRequest {
    pub request_id: String,
    pub action: ReadonlyAction,
    pub project_id: String,
    /// 请求声明的对象范围（导出/附件用）。
    pub scope: Vec<String>,
    pub policy_revision: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadonlyAudit {
    pub request_id: String,
    pub principal_ref: String,
    pub action: &'static str,
    pub scope_refs: Vec<String>,
    pub basis: String,
    pub policy_revision: u32,
    pub decision: &'static str,
    pub reason: &'static str,
}

/// 领域判定：只看查看者的范围，不看界面按钮有没有点亮。
/// 客户与工单范围在这里不再展开——那是过滤层的职责，放行只判项目级可达。
pub fn authorize_readonly(viewer: &Viewer, request: &ReadOnlyRequest) -> Result<(), &'static str> {
    if !viewer.may_view_project(&request.project_id) {
        return Err("不在项目范围内");
    }
    Ok(())
}

/// 界面提示。**只用于显示**，不参与放行判定。
pub fn button_hint(viewer: &Viewer, request: &ReadOnlyRequest) -> &'static str {
    if viewer.may_view_project(&request.project_id) {
        "available"
    } else {
        "hidden"
    }
}

pub fn audit_for(
    viewer: &Viewer,
    request: &ReadOnlyRequest,
    decision: &'static str,
    reason: &'static str,
) -> ReadonlyAudit {
    ReadonlyAudit {
        request_id: request.request_id.clone(),
        principal_ref: format!("principal:{}", viewer.principal),
        action: request.action.as_str(),
        scope_refs: request
            .scope
            .iter()
            .map(|object| format!("object:{object}"))
            .collect(),
        basis: format!("project:{}", request.project_id),
        policy_revision: request.policy_revision,
        decision,
        reason,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyOutcome {
    pub decision: &'static str,
    pub rows: usize,
    /// 有界计算：行数有上限，且不因上限静默截断而不说。
    pub bounded: bool,
    pub truncated_from: Option<usize>,
    pub created_runs: u32,
    pub created_bindings: u32,
    pub created_invocations: u32,
    pub started_agents: u32,
    pub audit: Option<ReadonlyAudit>,
}

/// 单次只读的返回行上限。超出即标记截断，而不是"看起来全都在"。
pub const READONLY_ROW_LIMIT: usize = 200;

/// 处理一次只读请求。副作用计数结构性为 0：函数体内没有任何能创建执行路径的调用。
pub fn serve_readonly(
    viewer: &Viewer,
    request: &ReadOnlyRequest,
    records: &[ReadonlyRecord],
) -> ReadOnlyOutcome {
    if let Err(reason) = authorize_readonly(viewer, request) {
        return ReadOnlyOutcome {
            decision: "denied",
            rows: 0,
            bounded: true,
            truncated_from: None,
            created_runs: 0,
            created_bindings: 0,
            created_invocations: 0,
            started_agents: 0,
            audit: Some(audit_for(viewer, request, "denied", reason)),
        };
    }
    let page = filter_records(viewer, records);
    let (rows, truncated_from) = if page.total > READONLY_ROW_LIMIT {
        (READONLY_ROW_LIMIT, Some(page.total))
    } else {
        (page.total, None)
    };
    ReadOnlyOutcome {
        decision: "allowed",
        rows,
        bounded: true,
        truncated_from,
        created_runs: 0,
        created_bindings: 0,
        created_invocations: 0,
        started_agents: 0,
        audit: Some(audit_for(viewer, request, "allowed", "范围内只读")),
    }
}

// ---------------------------------------------------------------------------
// 3.2 导出与附件：计数与文件名不泄露
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportBundle {
    /// 文件名只由请求名与**可见**行数构成。
    pub file_name: String,
    pub rows: usize,
    pub total: usize,
    pub withheld: usize,
    /// 导出所依据的策略修订：报告与审计要能对上。
    pub policy_revision: u32,
}

pub fn build_export(
    viewer: &Viewer,
    request: &ReadOnlyRequest,
    base_name: &str,
    records: &[ReadonlyRecord],
) -> ExportBundle {
    let page = filter_records(viewer, records);
    ExportBundle {
        file_name: format!("{base_name}-{}-rows.csv", page.total),
        rows: page.total,
        total: page.total,
        withheld: page.withheld,
        policy_revision: request.policy_revision,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentOutcome {
    /// 返回内容引用（下载仍需二次鉴权）。
    Grant { object_ref: String },
    /// 不可见与不存在给同一个答案：不能靠错误码枚举对象。
    NotVisible,
}

pub fn open_attachment(viewer: &Viewer, record: &ReadonlyRecord) -> AttachmentOutcome {
    match visibility_of(viewer, record) {
        Visibility::Visible => AttachmentOutcome::Grant {
            object_ref: format!("object:{}", record.object_id),
        },
        Visibility::Invisible => AttachmentOutcome::NotVisible,
    }
}

// ---------------------------------------------------------------------------
// 3.3 订阅与游标恢复（G-34）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadonlySubscription {
    pub session_id: String,
    pub principal: String,
    pub project_id: String,
    pub company_id: String,
    /// 最后**持久**确认游标。
    pub cursor: u64,
    pub released: bool,
}

impl ReadonlySubscription {
    pub fn new(session_id: &str, viewer: &Viewer, project_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            principal: viewer.principal.clone(),
            project_id: project_id.to_string(),
            company_id: viewer.company_id.clone(),
            cursor: 0,
            released: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionResume {
    Delta {
        from: u64,
        to: u64,
    },
    Snapshot {
        baseline: u64,
        to: u64,
    },
    /// 重新鉴权失败：不续传、不补读，也不"先推一点再看看"。
    Denied {
        reason: &'static str,
    },
}

/// 续传。鉴权在补拉之前，且每次都重做。
pub fn resume_subscription(
    subscription: &ReadonlySubscription,
    viewer: &Viewer,
    retained_floor: Option<u64>,
    head: u64,
    snapshot_baseline: Option<u64>,
) -> SubscriptionResume {
    if subscription.principal != viewer.principal || subscription.company_id != viewer.company_id {
        return SubscriptionResume::Denied {
            reason: "订阅主体与当前主体不一致",
        };
    }
    if !viewer.may_view_project(&subscription.project_id) {
        return SubscriptionResume::Denied {
            reason: "已不在项目范围内",
        };
    }
    let needs_snapshot = subscription.released
        || retained_floor
            .map(|floor| subscription.cursor + 1 < floor)
            .unwrap_or(false);
    if needs_snapshot {
        return match snapshot_baseline {
            Some(baseline) => SubscriptionResume::Snapshot { baseline, to: head },
            None => SubscriptionResume::Denied {
                reason: "游标超出保留期且无快照基准",
            },
        };
    }
    SubscriptionResume::Delta {
        from: subscription.cursor + 1,
        to: head,
    }
}

// ---------------------------------------------------------------------------
// 3.4 查询缓存边界
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NeverEvict {
    /// 正式 Manifest。
    Manifest,
    /// 不可变账本。
    Ledger,
    /// 租约。
    Lease,
    /// 幂等回执。
    IdempotencyReceipt,
    /// ACK 游标。
    AckCursor,
}

impl NeverEvict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Ledger => "ledger",
            Self::Lease => "lease",
            Self::IdempotencyReceipt => "idempotency_receipt",
            Self::AckCursor => "ack_cursor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheKind {
    /// 只读投影：可淘汰。
    Projection,
    /// 统计汇总：可淘汰。
    Statistics,
    /// 以上五类：**永不可淘汰**，不进 TTL 集合。
    Never(NeverEvict),
}

impl CacheKind {
    pub fn is_evictable(&self) -> bool {
        matches!(self, Self::Projection | Self::Statistics)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CacheKey {
    /// 身份范围：主体 + Company + 项目 + 客户，任一不同即不同条目。
    pub identity: String,
    pub input_digest: String,
    /// 数据/索引版本。
    pub index_version: u32,
    pub tool_version: u32,
}

impl CacheKey {
    pub fn for_viewer(viewer: &Viewer, project_id: &str, input_digest: &str) -> Self {
        Self {
            identity: format!(
                "{}|{}|{}|{}",
                viewer.principal,
                viewer.company_id,
                project_id,
                viewer.customer_id.as_deref().unwrap_or("-")
            ),
            input_digest: input_digest.to_string(),
            index_version: 1,
            tool_version: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub kind: CacheKind,
    pub projection_seq: u64,
    pub computed_at_ms: i64,
    pub ttl_ms: i64,
    /// 计算时所依据的授权修订。
    pub authz_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    Hit {
        projection_seq: u64,
        computed_at_ms: i64,
    },
    Miss {
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionCache {
    entries: BTreeMap<CacheKey, CacheEntry>,
}

impl ProjectionCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn put(&mut self, key: CacheKey, entry: CacheEntry) -> Result<(), &'static str> {
        if !entry.kind.is_evictable() {
            return Err("Manifest/账本/租约/幂等回执/ACK 游标不得进入可淘汰缓存");
        }
        self.entries.insert(key, entry);
        Ok(())
    }

    /// 查缓存。授权修订或 TTL 不符即 miss，且 miss 不说"旧结果还热着"。
    pub fn lookup(&self, key: &CacheKey, authz_revision: u64, now_ms: i64) -> CacheLookup {
        let Some(entry) = self.entries.get(key) else {
            return CacheLookup::Miss {
                reason: "无条目"
            };
        };
        if entry.authz_revision != authz_revision {
            return CacheLookup::Miss {
                reason: "授权修订已变化",
            };
        }
        if now_ms >= entry.computed_at_ms + entry.ttl_ms {
            return CacheLookup::Miss {
                reason: "已过 TTL"
            };
        }
        CacheLookup::Hit {
            projection_seq: entry.projection_seq,
            computed_at_ms: entry.computed_at_ms,
        }
    }

    /// 授权变化时只清可淘汰条目，五类正式记录一条都不动。
    pub fn invalidate_evictable(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| !entry.kind.is_evictable());
        before - self.entries.len()
    }
}

/// 展示用时间戳：命中给计算时间，未命中给"正在重算"。
pub const fn display_stamp(lookup: &CacheLookup) -> &'static str {
    match lookup {
        CacheLookup::Hit { .. } => "computed_at",
        CacheLookup::Miss { .. } => "recomputing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        id: &str,
        project: &str,
        customer: Option<&str>,
        ticket: Option<&str>,
    ) -> ReadonlyRecord {
        ReadonlyRecord {
            object_id: id.to_string(),
            company_id: "company-1".to_string(),
            project_id: project.to_string(),
            customer_id: customer.map(|value| value.to_string()),
            ticket_id: ticket.map(|value| value.to_string()),
            title: format!("标题-{id}"),
        }
    }

    fn viewer() -> Viewer {
        Viewer::new("user-1", "company-1", "project-1")
    }

    fn request(action: ReadonlyAction) -> ReadOnlyRequest {
        ReadOnlyRequest {
            request_id: "req-1".to_string(),
            action,
            project_id: "project-1".to_string(),
            scope: vec!["project-1".to_string()],
            policy_revision: 9,
        }
    }

    /// 3.1 / I-55：只读请求只有有界计算与审计，不建 Run、不派发、不启动 Agent。
    #[test]
    fn invariant_i_55_readonly_request_creates_no_run_or_dispatch() {
        let records = vec![
            record("issue-1", "project-1", None, None),
            record("issue-2", "project-1", None, None),
        ];
        for action in [
            ReadonlyAction::Dashboard,
            ReadonlyAction::Query,
            ReadonlyAction::Search,
            ReadonlyAction::Count,
            ReadonlyAction::Export,
            ReadonlyAction::Subscribe,
            ReadonlyAction::Attachment,
        ] {
            let outcome = serve_readonly(&viewer(), &request(action), &records);
            assert_eq!(outcome.decision, "allowed", "{action:?} 在范围内应放行");
            assert_eq!(outcome.created_runs, 0, "{action:?} 不得创建 Run");
            assert_eq!(outcome.created_bindings, 0, "{action:?} 不得创建 Binding");
            assert_eq!(
                outcome.created_invocations, 0,
                "{action:?} 不得创建 invocation"
            );
            assert_eq!(outcome.started_agents, 0, "{action:?} 不得启动 Agent");
            assert!(outcome.bounded, "只读计算必须有界");
            let audit = outcome.audit.expect("每次只读都留审计");
            assert_eq!(audit.action, action.as_str());
            assert_eq!(audit.policy_revision, 9);
            assert_eq!(audit.basis, "project:project-1");
            assert!(audit.principal_ref.starts_with("principal:"));
            assert!(
                audit
                    .scope_refs
                    .iter()
                    .all(|item| item.starts_with("object:"))
            );
        }

        // 有界：超过上限的行数被标记截断，不假装全量。
        let many: Vec<ReadonlyRecord> = (0..(READONLY_ROW_LIMIT + 5))
            .map(|index| record(&format!("issue-{index}"), "project-1", None, None))
            .collect();
        let outcome = serve_readonly(&viewer(), &request(ReadonlyAction::Query), &many);
        assert_eq!(outcome.rows, READONLY_ROW_LIMIT);
        assert_eq!(outcome.truncated_from, Some(READONLY_ROW_LIMIT + 5));
    }

    /// 3.1 / G-34：按钮提示不构成授权，放行只由领域判定。
    #[test]
    fn g_34_button_hint_never_authorizes() {
        let outsider = Viewer::new("user-2", "company-1", "project-9");
        let req = request(ReadonlyAction::Dashboard);
        assert_eq!(button_hint(&outsider, &req), "hidden");
        assert_eq!(authorize_readonly(&outsider, &req), Err("不在项目范围内"));

        let denied = serve_readonly(&outsider, &req, &[]);
        assert_eq!(denied.decision, "denied");
        assert_eq!(denied.rows, 0);
        assert_eq!(denied.created_runs, 0);
        let audit = denied.audit.expect("拒绝也要留审计");
        assert_eq!(audit.decision, "denied");
        assert_eq!(audit.reason, "不在项目范围内");

        // 提示说 available 也不等于放行：范围里确实有的项目才 available + allowed。
        assert_eq!(button_hint(&viewer(), &req), "available");
        assert!(authorize_readonly(&viewer(), &req).is_ok());
    }

    /// 3.2 / AZ06 类：单工单协作者不能枚举父项目或兄弟工单。
    #[test]
    fn task_3_2_collaborator_cannot_enumerate_parent_or_siblings() {
        let collaborator = viewer()
            .with_customer("customer-a")
            .with_tickets(&["ticket-1"]);
        let records = vec![
            record("issue-1", "project-1", Some("customer-a"), Some("ticket-1")),
            record("issue-2", "project-1", Some("customer-a"), Some("ticket-2")),
            record("issue-3", "project-1", Some("customer-b"), Some("ticket-1")),
            record("issue-4", "project-2", Some("customer-a"), Some("ticket-1")),
        ];
        let page = filter_records(&collaborator, &records);
        assert_eq!(page.visible.len(), 1);
        assert_eq!(
            page.total, 1,
            "总数只数可见项，兄弟工单与父项目对象不参与计数"
        );
        assert_eq!(page.withheld, 3, "只给数量，不给身份");
        assert_eq!(page.visible[0].object_id, "issue-1");
        for hidden in ["issue-2", "issue-3", "issue-4"] {
            assert!(
                !page.visible.iter().any(|item| item.object_id == hidden),
                "{hidden} 不得可见"
            );
        }

        // 不可见与不存在给同一个答案。
        assert_eq!(
            open_attachment(&collaborator, &records[1]),
            AttachmentOutcome::NotVisible
        );
        assert_eq!(
            open_attachment(&collaborator, &records[0]),
            AttachmentOutcome::Grant {
                object_ref: "object:issue-1".to_string()
            }
        );
    }

    /// 3.2：导出只含获准对象，计数与文件名不暴露未授权对象。
    #[test]
    fn task_3_2_export_counts_and_name_expose_only_visible() {
        let collaborator = viewer().with_tickets(&["ticket-1"]);
        let records = vec![
            record("issue-1", "project-1", None, Some("ticket-1")),
            record("issue-2", "project-1", None, Some("ticket-2")),
            record("issue-3", "project-1", None, Some("ticket-3")),
        ];
        let bundle = build_export(
            &collaborator,
            &request(ReadonlyAction::Export),
            "project-export",
            &records,
        );
        assert_eq!(bundle.rows, 1);
        assert_eq!(bundle.total, 1);
        assert_eq!(bundle.file_name, "project-export-1-rows.csv");
        assert_eq!(bundle.withheld, 2, "被挡条数只有数量");
        assert_eq!(bundle.policy_revision, 9, "导出记录所依据的策略修订");
        assert!(
            !bundle.file_name.contains('3'),
            "文件名不得出现候选总数：{}",
            bundle.file_name
        );
    }

    /// 3.3 / G-34：撤权后续传被拒，不继续推范围外事件。
    #[test]
    fn task_3_3_revoked_subscription_resume_rejected() {
        let subscription = ReadonlySubscription::new("sess-1", &viewer(), "project-1");
        assert_eq!(
            resume_subscription(&subscription, &viewer(), None, 10, None),
            SubscriptionResume::Delta { from: 1, to: 10 }
        );

        // 撤权：主体不再在项目范围内。
        let revoked = Viewer::new("user-1", "company-1", "project-9");
        assert_eq!(
            resume_subscription(&subscription, &revoked, None, 10, None),
            SubscriptionResume::Denied {
                reason: "已不在项目范围内"
            }
        );

        // 主体或公司变了也一样拒。
        let other = Viewer::new("user-9", "company-1", "project-1");
        assert!(matches!(
            resume_subscription(&subscription, &other, None, 10, None),
            SubscriptionResume::Denied { .. }
        ));

        // 游标超保留期：有快照读快照，没快照直接拒，不做增量拼接。
        let stale = ReadonlySubscription {
            cursor: 3,
            ..subscription.clone()
        };
        assert_eq!(
            resume_subscription(&stale, &viewer(), Some(8), 20, Some(15)),
            SubscriptionResume::Snapshot {
                baseline: 15,
                to: 20
            }
        );
        assert_eq!(
            resume_subscription(&stale, &viewer(), Some(8), 20, None),
            SubscriptionResume::Denied {
                reason: "游标超出保留期且无快照基准"
            }
        );
    }

    /// 3.4：缓存键含身份与索引版本，五类正式记录不得进入可淘汰集合。
    #[test]
    fn task_3_4_cache_key_includes_identity_and_index_version() {
        let base = CacheKey::for_viewer(&viewer(), "project-1", "sha256:in");
        let other_viewer = CacheKey::for_viewer(
            &Viewer::new("user-2", "company-1", "project-1"),
            "project-1",
            "sha256:in",
        );
        assert_ne!(base, other_viewer, "身份不同即不同条目");

        let other_customer = CacheKey::for_viewer(
            &viewer().with_customer("customer-a"),
            "project-1",
            "sha256:in",
        );
        assert_ne!(base, other_customer, "客户范围也进身份");

        let other_index = CacheKey {
            index_version: 2,
            ..base.clone()
        };
        assert_ne!(base, other_index, "索引版本不同即不同条目");

        let mut cache = ProjectionCache::new();
        for kind in [
            NeverEvict::Manifest,
            NeverEvict::Ledger,
            NeverEvict::Lease,
            NeverEvict::IdempotencyReceipt,
            NeverEvict::AckCursor,
        ] {
            assert!(!CacheKind::Never(kind.clone()).is_evictable());
            assert_eq!(
                cache.put(
                    base.clone(),
                    CacheEntry {
                        kind: CacheKind::Never(kind.clone()),
                        projection_seq: 1,
                        computed_at_ms: 0,
                        ttl_ms: 0,
                        authz_revision: 1,
                    }
                ),
                Err("Manifest/账本/租约/幂等回执/ACK 游标不得进入可淘汰缓存"),
                "{kind:?} 不得当可淘汰缓存"
            );
        }
        assert!(cache.is_empty(), "五类正式记录一条都没进缓存");

        assert_eq!(
            cache.put(
                base.clone(),
                CacheEntry {
                    kind: CacheKind::Projection,
                    projection_seq: 4,
                    computed_at_ms: 1_000,
                    ttl_ms: 500,
                    authz_revision: 2,
                }
            ),
            Ok(())
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.lookup(&base, 2, 1_100),
            CacheLookup::Hit {
                projection_seq: 4,
                computed_at_ms: 1_000
            }
        );
        assert_eq!(
            display_stamp(&cache.lookup(&base, 2, 1_100)),
            "computed_at",
            "展示带更新时间"
        );
    }

    /// 3.4：授权变化即失效并重算，展示时间不掩盖时序。
    #[test]
    fn task_3_4_permission_change_invalidates_projection() {
        let mut cache = ProjectionCache::new();
        let key = CacheKey::for_viewer(&viewer(), "project-1", "sha256:stats");
        cache
            .put(
                key.clone(),
                CacheEntry {
                    kind: CacheKind::Statistics,
                    projection_seq: 7,
                    computed_at_ms: 1_000,
                    ttl_ms: 60_000,
                    authz_revision: 1,
                },
            )
            .expect("统计投影可缓存");

        assert_eq!(
            cache.lookup(&key, 2, 1_100),
            CacheLookup::Miss {
                reason: "授权修订已变化"
            },
            "撤权后旧统计不得复用"
        );
        assert_eq!(display_stamp(&cache.lookup(&key, 2, 1_100)), "recomputing");
        assert_eq!(
            cache.lookup(&key, 1, 70_000),
            CacheLookup::Miss {
                reason: "已过 TTL"
            }
        );
        assert_eq!(cache.invalidate_evictable(), 1);
        assert!(cache.is_empty());
    }

    /// 3.5：审计只留引用与摘要，不含令牌或业务密文。
    #[test]
    fn task_3_5_audit_holds_refs_not_tokens() {
        let mut req = request(ReadonlyAction::Export);
        req.scope = vec!["artifact-9".to_string()];
        let audit = audit_for(&viewer(), &req, "allowed", "范围内只读");
        assert_eq!(audit.request_id, "req-1");
        assert_eq!(audit.action, "export");
        assert_eq!(audit.scope_refs, vec!["object:artifact-9".to_string()]);
        assert_eq!(audit.decision, "allowed");
        assert_eq!(audit.policy_revision, 9);

        let rendered = format!("{audit:?}");
        for forbidden in ["sk-live", "bearer", "token=", "password", "ciphertext"] {
            assert!(
                !rendered.contains(forbidden),
                "审计不得出现 `{forbidden}`：{rendered}"
            );
        }
        assert!(
            audit.principal_ref.starts_with("principal:"),
            "主体只留引用"
        );
    }
}
