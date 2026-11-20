//! P2 任务 3.5：专项委任边界与组织元数据分离（`invariant_i_62`—`invariant_i_64` 的边界部分）。
//!
//! 权威语义：manager-transfer「同一管理作用域 SHALL 只有一个当前主委任；Issue 专项委任优先于
//! 项目默认委任……项目负责人 MUST NOT 越过专项边界指挥其子任务，也不得通过创建任务绕过需要
//! 独立委任的专项范围」；「reportsTo 等组织字段 SHALL 只是可更新的元数据，变更 MUST NOT 迁移、
//! fencing 或保留下层任务与 Run 的执行状态；『primary agent』类用户偏好不改变 roles、权限或
//! 任务分配」。
//!
//! 边界：
//! - 不判定命令是否在委任命令集内（3.1 的 `ManagementCommandSet` 负责），本模块只回答
//!   「这条命令指向的对象归谁」。
//! - 不做时间有效性判定（3.1 在提交时按 `expires_at` 复核），本索引只按 `Active` 状态归属。
//! - 层级只登记父子，不改 `issues` 表与 `bod-core::Issue`；改父节点会改变边界归属，
//!   因此层级一次写定，重建走治理流程（本期不提供）。
//! - reportsTo 只写 `agent_org_metadata` 一张表；「没有迁移状态」不是声明，而是变更前后
//!   对 issues / runs / leases / grants / appointments 逐字段求摘要比对的结果。

use crate::manager_transfer::{AppointmentStatus, ManagementAppointment, ManagementScope};
use crate::store::{SqliteStore, StoreError};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// 层级与汇报链的最大深度：超过即报错，不静默截断。
pub const MAX_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// 边界归属
// ---------------------------------------------------------------------------

/// 一条 Issue 的边界归属者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeOwner {
    pub scope: ManagementScope,
    pub appointment_id: String,
    pub manager: String,
    pub authority_epoch: u64,
}

impl ScopeOwner {
    fn from_appointment(appointment: &ManagementAppointment) -> Self {
        Self {
            scope: appointment.scope.clone(),
            appointment_id: appointment.appointment_id.clone(),
            manager: appointment.manager_agent_id.clone(),
            authority_epoch: appointment.authority_epoch,
        }
    }

    pub fn is_special(&self) -> bool {
        matches!(self.scope, ManagementScope::IssueSubtree { .. })
    }
}

/// 某项目的专项边界索引：谁管项目、哪些根 Issue 有专项委任、Issue 之间的父子关系。
#[derive(Debug, Clone)]
pub struct BoundaryIndex {
    company_id: String,
    project_id: String,
    issues: BTreeSet<String>,
    parents: BTreeMap<String, String>,
    project_owner: Option<ScopeOwner>,
    special: BTreeMap<String, ScopeOwner>,
}

impl BoundaryIndex {
    /// 只读装配。发现悬空父节点/自环/重复专项根即报错，不猜。
    pub fn load(
        store: &SqliteStore,
        company_id: &str,
        project_id: &str,
    ) -> Result<Self, BoundaryError> {
        if company_id.trim().is_empty() {
            return Err(BoundaryError::EmptyField("company_id"));
        }
        if project_id.trim().is_empty() {
            return Err(BoundaryError::EmptyField("project_id"));
        }

        let issues = store
            .list_issues_for_project(company_id, project_id)?
            .into_iter()
            .map(|issue| issue.meta.uid.as_str().to_string())
            .collect::<BTreeSet<_>>();

        let mut parents = BTreeMap::new();
        for row in store.list_project_hierarchy(company_id, project_id)? {
            if !issues.contains(&row.parent_issue_uid) {
                return Err(BoundaryError::DanglingParent {
                    issue: row.issue_uid,
                    parent: row.parent_issue_uid,
                });
            }
            parents.insert(row.issue_uid, row.parent_issue_uid);
        }

        let mut project_owner: Option<ScopeOwner> = None;
        let mut special: BTreeMap<String, ScopeOwner> = BTreeMap::new();
        for row in store.list_company_appointments(company_id)? {
            let appointment = row.to_appointment()?;
            if appointment.status != AppointmentStatus::Active {
                continue;
            }
            match &appointment.scope {
                ManagementScope::Project {
                    project_id: scope_project,
                } if scope_project == project_id => {
                    // 3.1 保证同一作用域只有一个活跃主委任；这里只按 epoch 取最新一条兜底。
                    let candidate = ScopeOwner::from_appointment(&appointment);
                    if project_owner
                        .as_ref()
                        .is_none_or(|current| current.authority_epoch < candidate.authority_epoch)
                    {
                        project_owner = Some(candidate);
                    }
                }
                ManagementScope::IssueSubtree {
                    project_id: scope_project,
                    root_issue_id,
                } if scope_project == project_id => {
                    if !issues.contains(root_issue_id) {
                        return Err(BoundaryError::DanglingSpecialRoot {
                            root: root_issue_id.clone(),
                        });
                    }
                    let candidate = ScopeOwner::from_appointment(&appointment);
                    if special.insert(root_issue_id.clone(), candidate).is_some() {
                        return Err(BoundaryError::DuplicateSpecialRoot {
                            root: root_issue_id.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        let index = Self {
            company_id: company_id.to_string(),
            project_id: project_id.to_string(),
            issues,
            parents,
            project_owner,
            special,
        };
        // 自环/环在上车时就查出来，避免「解析归属」时无限上溯。
        for issue in index.issues.iter() {
            index.ancestors(issue)?;
        }
        Ok(index)
    }

    pub fn company_id(&self) -> &str {
        &self.company_id
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// 已知的专项根（按 Issue id 排序）。
    pub fn special_roots(&self) -> Vec<&str> {
        self.special.keys().map(String::as_str).collect()
    }

    /// 自底向上的祖先链（含自身，最近在前）。
    pub fn ancestors(&self, issue: &str) -> Result<Vec<String>, BoundaryError> {
        if !self.issues.contains(issue) {
            return Err(BoundaryError::UnknownIssue {
                issue: issue.to_string(),
            });
        }
        let mut chain = vec![issue.to_string()];
        let mut current = issue.to_string();
        let mut steps = 0;
        while let Some(parent) = self.parents.get(&current) {
            steps += 1;
            if steps > MAX_DEPTH || chain.iter().any(|item| item == parent) {
                return Err(BoundaryError::CycleDetected {
                    issue: issue.to_string(),
                });
            }
            chain.push(parent.clone());
            current = parent.clone();
        }
        Ok(chain)
    }

    /// 这条 Issue 归谁：最近的专项根优先，其次项目默认委任。
    pub fn owner_of(&self, issue: &str) -> Result<ScopeOwner, BoundaryError> {
        for ancestor in self.ancestors(issue)? {
            if let Some(owner) = self.special.get(&ancestor) {
                return Ok(owner.clone());
            }
        }
        self.project_owner
            .clone()
            .ok_or(BoundaryError::NoProjectManager {
                project_id: self.project_id.clone(),
            })
    }

    /// 指挥类命令的边界判定：对象归谁，就必须由谁的委任提交。
    pub fn authorize(
        &self,
        appointment: &ManagementAppointment,
        target_issue: &str,
    ) -> Result<ScopeOwner, BoundaryError> {
        if appointment.status != AppointmentStatus::Active {
            return Err(BoundaryError::AppointmentNotActive {
                status: appointment.status,
            });
        }
        let owner = self.owner_of(target_issue)?;
        let holds_owner =
            appointment.appointment_id == owner.appointment_id && appointment.scope == owner.scope;
        if holds_owner {
            return Ok(owner);
        }
        match (&appointment.scope, owner.is_special()) {
            // 项目负责人动了专项范围内的对象：这是「越界指挥」，不是「没权限」。
            (ManagementScope::Project { .. }, true) => Err(BoundaryError::CrossesSpecialBoundary {
                root: match &owner.scope {
                    ManagementScope::IssueSubtree { root_issue_id, .. } => root_issue_id.clone(),
                    ManagementScope::Project { .. } => owner.appointment_id.clone(),
                },
                owner_appointment: owner.appointment_id.clone(),
                target_issue: target_issue.to_string(),
            }),
            _ => Err(BoundaryError::OutsideScope {
                scope: appointment.scope.clone(),
                target_issue: target_issue.to_string(),
            }),
        }
    }

    /// 创建任务时的边界判定：在专项子树里建任务同样算越界（否则「建个任务绕过去」成立）。
    pub fn authorize_create(
        &self,
        appointment: &ManagementAppointment,
        parent_issue: Option<&str>,
    ) -> Result<(), BoundaryError> {
        if appointment.status != AppointmentStatus::Active {
            return Err(BoundaryError::AppointmentNotActive {
                status: appointment.status,
            });
        }
        let Some(parent) = parent_issue else {
            // 顶层任务只归项目委任；专项负责人不能在建顶层任务时扩权。
            return match appointment.scope {
                ManagementScope::Project { .. } => Ok(()),
                _ => Err(BoundaryError::OutsideScope {
                    scope: appointment.scope.clone(),
                    target_issue: "<new top-level issue>".to_string(),
                }),
            };
        };
        let owner = self.owner_of(parent)?;
        if appointment.appointment_id == owner.appointment_id && appointment.scope == owner.scope {
            return Ok(());
        }
        if owner.is_special() {
            let root = match &owner.scope {
                ManagementScope::IssueSubtree { root_issue_id, .. } => root_issue_id.clone(),
                ManagementScope::Project { .. } => owner.appointment_id.clone(),
            };
            return match appointment.scope {
                ManagementScope::Project { .. } => Err(BoundaryError::BypassAttempt {
                    parent: parent.to_string(),
                    root,
                    owner_appointment: owner.appointment_id,
                }),
                _ => Err(BoundaryError::OutsideScope {
                    scope: appointment.scope.clone(),
                    target_issue: parent.to_string(),
                }),
            };
        }
        Err(BoundaryError::OutsideScope {
            scope: appointment.scope.clone(),
            target_issue: parent.to_string(),
        })
    }

    /// 嵌套专项委任：必须由治理主体显式创建，且根 Issue 必须落在父专项子树内。
    ///
    /// 只返回可用的作用域，不落库——落库仍走 3.1 的 [`ManagementAppointment::appoint`]。
    pub fn nested_scope(
        &self,
        parent_root: &str,
        root_issue: &str,
        by_governance: bool,
    ) -> Result<ManagementScope, BoundaryError> {
        if !by_governance {
            return Err(BoundaryError::NestedRequiresGovernance);
        }
        if !self.special.contains_key(parent_root) {
            return Err(BoundaryError::UnknownSpecialRoot {
                root: parent_root.to_string(),
            });
        }
        let chain = self.ancestors(root_issue)?;
        if chain.len() < 2 || !chain[1..].iter().any(|item| item == parent_root) {
            return Err(BoundaryError::NestedOutsideParent {
                root: root_issue.to_string(),
                parent_root: parent_root.to_string(),
            });
        }
        if self.special.contains_key(root_issue) {
            return Err(BoundaryError::RootAlreadyManaged {
                root: root_issue.to_string(),
            });
        }
        Ok(ManagementScope::issue_subtree(
            self.project_id.clone(),
            root_issue,
        ))
    }
}

// ---------------------------------------------------------------------------
// 组织元数据
// ---------------------------------------------------------------------------

/// 变更前后都要相等的事实摘要：执行状态与授权不能被组织字段捎带改动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgFacts {
    pub counts: BTreeMap<&'static str, usize>,
    pub digest: String,
}

impl OrgFacts {
    /// 以项目为观察窗口；租约/授权/委任是公司级表，按全量取（避免「只看本项目」漏掉跨项目迁移）。
    pub fn capture(
        store: &SqliteStore,
        company_id: &str,
        project_id: &str,
    ) -> Result<Self, BoundaryError> {
        let mut counts = BTreeMap::new();
        let mut lines: Vec<String> = Vec::new();

        let issues = store.list_issues_for_project(company_id, project_id)?;
        counts.insert("issues", issues.len());
        for issue in &issues {
            lines.push(format!(
                "issue\t{}\t{}\t{}\t{}\t{}\t{}",
                issue.meta.uid.as_str(),
                issue
                    .spec
                    .assignment
                    .as_ref()
                    .map(|assignment| assignment.owner.id.as_str())
                    .unwrap_or("-"),
                issue
                    .spec
                    .assignment
                    .as_ref()
                    .map(|assignment| assignment.grant_revision.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                phase_label(issue.status.phase),
                issue.meta.generation,
                issue.meta.resource_version.0
            ));
        }

        let runs = store.list_project_runs(company_id, project_id)?;
        counts.insert("runs", runs.len());
        for (run_id, issue_uid, desired) in &runs {
            lines.push(format!("run\t{run_id}\t{issue_uid}\t{desired}"));
        }

        let leases = store.list_company_leases(company_id)?;
        counts.insert("leases", leases.len());
        for lease in &leases {
            lines.push(format!(
                "lease\t{}\t{}\t{}\t{}\t{}",
                lease.resource_key,
                lease.owner,
                lease.lease_token,
                lease.expires_at.as_deref().unwrap_or("-"),
                lease.state
            ));
        }

        let grants = store.list_company_grants(company_id)?;
        counts.insert("grants", grants.len());
        for grant in &grants {
            lines.push(format!(
                "grant\t{}\t{}\t{}\t{}\t{}",
                grant.principal_id,
                grant.project_id.as_deref().unwrap_or("-"),
                grant.permission,
                grant.revision,
                grant.active
            ));
        }

        let appointments = store.list_company_appointments(company_id)?;
        counts.insert("appointments", appointments.len());
        for appointment in &appointments {
            lines.push(format!(
                "appointment\t{}\t{}\t{}\t{}\t{}",
                appointment.appointment_id,
                appointment.scope_key,
                appointment.authority_epoch,
                appointment.manager_agent_id,
                appointment.status
            ));
        }

        lines.sort();
        let digest = format!(
            "sha256:{}",
            crate::domain::artifact::sha256_hex(lines.join("\n").as_bytes())
        );
        Ok(Self { counts, digest })
    }
}

/// 一次组织元数据变更的完整记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataChange {
    pub agent_id: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub updated_by: String,
    pub updated_at_ms: i64,
    /// 变更后的执行/授权事实摘要（必须与变更前相等）。
    pub preserved: OrgFacts,
}

/// 写入 reportsTo。只写 `agent_org_metadata`，并当场比对执行/授权事实未变。
pub fn set_reports_to(
    store: &mut SqliteStore,
    company_id: &str,
    project_id: &str,
    agent_id: &str,
    reports_to: Option<&str>,
    updated_by: &str,
    now_ms: i64,
) -> Result<MetadataChange, BoundaryError> {
    for (field, value) in [
        ("company_id", company_id),
        ("project_id", project_id),
        ("agent_id", agent_id),
        ("updated_by", updated_by),
    ] {
        if value.trim().is_empty() {
            return Err(BoundaryError::EmptyField(field));
        }
    }
    if !store.member_exists(company_id, agent_id)? {
        return Err(BoundaryError::UnknownMember {
            agent: agent_id.to_string(),
        });
    }
    if let Some(target) = reports_to {
        if target.trim().is_empty() {
            return Err(BoundaryError::EmptyField("reports_to"));
        }
        if target == agent_id {
            return Err(BoundaryError::SelfReport {
                agent: agent_id.to_string(),
            });
        }
        if !store.member_exists(company_id, target)? {
            return Err(BoundaryError::UnknownMember {
                agent: target.to_string(),
            });
        }
        ensure_no_report_cycle(store, company_id, agent_id, target)?;
    }

    let before = OrgFacts::capture(store, company_id, project_id)?;
    let previous = store.get_org_metadata(company_id, agent_id)?;
    let row = crate::store::OrgMetadataRow {
        agent_id: agent_id.to_string(),
        reports_to: reports_to.map(str::to_string),
        updated_at_ms: now_ms,
        updated_by: updated_by.to_string(),
    };
    store.upsert_org_metadata(company_id, &row)?;
    let after = OrgFacts::capture(store, company_id, project_id)?;
    if before != after {
        return Err(BoundaryError::MetadataTouchedExecution {
            before: before.digest,
            after: after.digest,
        });
    }

    Ok(MetadataChange {
        agent_id: agent_id.to_string(),
        from: previous.and_then(|row| row.reports_to),
        to: reports_to.map(str::to_string),
        updated_by: updated_by.to_string(),
        updated_at_ms: now_ms,
        preserved: after,
    })
}

/// 汇报链自环检查：target 一路向上如果回到 agent，就形成环。
fn ensure_no_report_cycle(
    store: &SqliteStore,
    company_id: &str,
    agent_id: &str,
    target: &str,
) -> Result<(), BoundaryError> {
    let mut current = Some(target.to_string());
    let mut steps = 0;
    while let Some(node) = current {
        steps += 1;
        if steps > MAX_DEPTH {
            return Err(BoundaryError::CycleDetected {
                issue: agent_id.to_string(),
            });
        }
        if node == agent_id {
            return Err(BoundaryError::ReportsToCycle {
                agent: agent_id.to_string(),
                target: target.to_string(),
            });
        }
        current = store
            .get_org_metadata(company_id, &node)?
            .and_then(|row| row.reports_to);
    }
    Ok(())
}

fn phase_label(phase: crate::domain::Phase) -> &'static str {
    use crate::domain::Phase;
    match phase {
        Phase::Pending => "Pending",
        Phase::Running => "Running",
        Phase::Blocked => "Blocked",
        Phase::Succeeded => "Succeeded",
        Phase::Cancelled => "Cancelled",
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------
#[derive(Debug)]
pub enum BoundaryError {
    Store(StoreError),
    EmptyField(&'static str),
    UnknownIssue {
        issue: String,
    },
    DanglingParent {
        issue: String,
        parent: String,
    },
    CycleDetected {
        issue: String,
    },
    DanglingSpecialRoot {
        root: String,
    },
    UnknownSpecialRoot {
        root: String,
    },
    DuplicateSpecialRoot {
        root: String,
    },
    NoProjectManager {
        project_id: String,
    },
    AppointmentNotActive {
        status: AppointmentStatus,
    },
    CrossesSpecialBoundary {
        root: String,
        owner_appointment: String,
        target_issue: String,
    },
    BypassAttempt {
        parent: String,
        root: String,
        owner_appointment: String,
    },
    OutsideScope {
        scope: ManagementScope,
        target_issue: String,
    },
    NestedRequiresGovernance,
    NestedOutsideParent {
        root: String,
        parent_root: String,
    },
    RootAlreadyManaged {
        root: String,
    },
    CorruptAppointment(String),
    UnknownMember {
        agent: String,
    },
    SelfReport {
        agent: String,
    },
    ReportsToCycle {
        agent: String,
        target: String,
    },
    MetadataTouchedExecution {
        before: String,
        after: String,
    },
}

impl fmt::Display for BoundaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "存储错误: {error}"),
            Self::EmptyField(field) => write!(formatter, "字段 {field} 不能为空"),
            Self::UnknownIssue { issue } => write!(formatter, "项目内不存在 Issue {issue}"),
            Self::DanglingParent { issue, parent } => write!(
                formatter,
                "层级登记悬空：{issue} 的父节点 {parent} 不在本项目"
            ),
            Self::CycleDetected { issue } => {
                write!(
                    formatter,
                    "层级/汇报链在 {issue} 处成环或超过 {MAX_DEPTH} 层"
                )
            }
            Self::DanglingSpecialRoot { root } => {
                write!(formatter, "专项委任的根 Issue {root} 不在本项目")
            }
            Self::UnknownSpecialRoot { root } => {
                write!(formatter, "{root} 没有专项委任，不能作为父专项")
            }
            Self::DuplicateSpecialRoot { root } => {
                write!(formatter, "专项根 {root} 同时挂着两个活跃委任")
            }
            Self::NoProjectManager { project_id } => {
                write!(formatter, "项目 {project_id} 没有活跃的项目委任")
            }
            Self::AppointmentNotActive { status } => {
                write!(formatter, "委任状态为 {status:?}，不能提交管理命令")
            }
            Self::CrossesSpecialBoundary {
                root,
                owner_appointment,
                target_issue,
            } => write!(
                formatter,
                "{target_issue} 属于专项 {root}（委任 {owner_appointment}），项目负责人不得越界指挥"
            ),
            Self::BypassAttempt {
                parent,
                root,
                owner_appointment,
            } => write!(
                formatter,
                "在专项 {root}（委任 {owner_appointment}）内的 {parent} 下建任务属于绕过边界"
            ),
            Self::OutsideScope {
                scope,
                target_issue,
            } => write!(formatter, "作用域 {scope} 不含 {target_issue}"),
            Self::NestedRequiresGovernance => {
                write!(formatter, "嵌套专项委任必须由治理主体显式创建")
            }
            Self::NestedOutsideParent { root, parent_root } => {
                write!(formatter, "{root} 不在父专项 {parent_root} 的子树内")
            }
            Self::RootAlreadyManaged { root } => {
                write!(formatter, "专项根 {root} 已有活跃委任")
            }
            Self::CorruptAppointment(detail) => write!(formatter, "委任行不可解释：{detail}"),
            Self::UnknownMember { agent } => write!(formatter, "成员 {agent} 未登记"),
            Self::SelfReport { agent } => write!(formatter, "{agent} 不能汇报给自己"),
            Self::ReportsToCycle { agent, target } => {
                write!(formatter, "{agent} 汇报给 {target} 会形成汇报环")
            }
            Self::MetadataTouchedExecution { before, after } => write!(
                formatter,
                "组织元数据变更触动了执行/授权事实（{before} → {after}），拒绝"
            ),
        }
    }
}

impl std::error::Error for BoundaryError {}

impl From<StoreError> for BoundaryError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<crate::manager_transfer::TransferError> for BoundaryError {
    fn from(error: crate::manager_transfer::TransferError) -> Self {
        Self::CorruptAppointment(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::domain::model::{
        AcceptanceCriteria, Issue, IssueKind, IssueSpec, Phase, ResourceMeta, ResourceStatus,
    };
    use crate::domain::{DomainId, Version};
    use crate::manager_transfer::{AppointRequest, ManagementCommand, ManagementCommandSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const COMPANY: &str = "company-1";
    const PROJECT: &str = "project-1";
    const AUDIENCE: &str = "host:local";
    const NOW_MS: i64 = 1_764_100_000_000;
    const AT: &str = "2026-11-20T12:00:00Z";

    fn temp_root(tag: &str) -> PathBuf {
        let seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bod-scope-{tag}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_store(root: &Path) -> SqliteStore {
        SqliteStore::open(&root.join("bod.db"), &BodConfig::default()).unwrap()
    }

    fn commands() -> ManagementCommandSet {
        ManagementCommandSet::new(vec![
            ManagementCommand::PlanRevise,
            ManagementCommand::AssignTask,
            ManagementCommand::ReadStatus,
        ])
        .unwrap()
    }

    fn add_issue(store: &mut SqliteStore, uid: &str) {
        let issue = Issue {
            meta: ResourceMeta {
                uid: DomainId::new(uid),
                company_id: DomainId::new(COMPANY),
                project_id: Some(DomainId::new(PROJECT)),
                resource_version: Version::INITIAL,
                generation: 1,
                deletion_requested_at: None,
            },
            kind: IssueKind::General,
            repository: None,
            spec: IssueSpec {
                title: format!("{uid} 的标题"),
                assignment: None,
                acceptance: AcceptanceCriteria {
                    criteria: vec!["测试通过".to_string()],
                },
            },
            status: ResourceStatus {
                phase: Phase::Pending,
                observed_generation: 1,
                conditions: Vec::new(),
            },
        };
        let uow = store.transaction().unwrap();
        uow.insert_issue(&issue).unwrap();
        uow.commit().unwrap();
    }

    fn link(store: &SqliteStore, child: &str, parent: &str) {
        store
            .insert_issue_hierarchy(COMPANY, PROJECT, child, parent, NOW_MS)
            .unwrap();
    }

    fn appoint(
        store: &SqliteStore,
        scope: ManagementScope,
        manager: &str,
    ) -> ManagementAppointment {
        ManagementAppointment::appoint(
            store,
            AppointRequest::new(
                COMPANY,
                scope,
                manager,
                "grant:manager",
                "user-owner",
                AT,
                Some("2027-01-01T00:00:00Z".to_string()),
                AUDIENCE,
                commands(),
            ),
            NOW_MS,
        )
        .unwrap()
    }

    fn add_member(store: &SqliteStore, uid: &str) {
        store
            .execute_batch_for_test(&format!(
                "INSERT INTO members (uid, company_id, kind, display_name)
                 VALUES ('{uid}', '{COMPANY}', 'agent', '{uid}')"
            ))
            .unwrap();
    }

    /// 三个 Issue：issue-1（普通）、issue-root（专项根）、issue-child（under root）。
    fn seeded(tag: &str) -> (SqliteStore, PathBuf) {
        let root = temp_root(tag);
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1");
        add_issue(&mut store, "issue-root");
        add_issue(&mut store, "issue-child");
        link(&store, "issue-child", "issue-root");
        (store, root)
    }

    #[test]
    fn project_owner_covers_issues_without_special_appointment() {
        let (store, _root) = seeded("project");
        let project = appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();

        let owner = index.owner_of("issue-1").unwrap();
        assert_eq!(owner.scope, ManagementScope::project(PROJECT));
        assert_eq!(owner.manager, "manager-a");
        assert!(!owner.is_special());

        // 无关 Issue 的命令由项目负责人提交是允许的。
        index.authorize(&project, "issue-1").unwrap();
    }

    #[test]
    fn special_root_and_descendants_belong_to_the_special_appointment() {
        let (store, _root) = seeded("special");
        appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        let special = appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-root"),
            "manager-special",
        );
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();
        assert_eq!(index.special_roots(), vec!["issue-root"]);

        for issue in ["issue-root", "issue-child"] {
            let owner = index.owner_of(issue).unwrap();
            assert!(owner.is_special(), "{issue} 应归专项");
            assert_eq!(owner.appointment_id, special.appointment_id);
        }
        // 专项根自己也在自己的作用域内。
        index.authorize(&special, "issue-root").unwrap();
        index.authorize(&special, "issue-child").unwrap();
    }

    #[test]
    fn project_manager_cannot_command_inside_special_subtree() {
        let (store, _root) = seeded("cross");
        let project = appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-root"),
            "manager-special",
        );
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();

        match index.authorize(&project, "issue-child") {
            Err(BoundaryError::CrossesSpecialBoundary {
                root,
                owner_appointment,
                target_issue,
            }) => {
                assert_eq!(root, "issue-root");
                assert_eq!(target_issue, "issue-child");
                assert!(owner_appointment.starts_with("appointment:issue:project-1/issue-root"));
            }
            other => panic!("越界指挥应被拒绝，实际 {other:?}"),
        }
    }

    #[test]
    fn special_holder_cannot_command_outside_its_subtree() {
        let (store, _root) = seeded("outside");
        appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        let special = appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-root"),
            "manager-special",
        );
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();
        match index.authorize(&special, "issue-1") {
            Err(BoundaryError::OutsideScope {
                scope,
                target_issue,
            }) => {
                assert_eq!(scope, ManagementScope::issue_subtree(PROJECT, "issue-root"));
                assert_eq!(target_issue, "issue-1");
            }
            other => panic!("专项负责人越出子树应被拒绝，实际 {other:?}"),
        }
    }

    #[test]
    fn creating_a_task_inside_the_subtree_is_a_bypass_attempt() {
        let (store, _root) = seeded("bypass");
        let project = appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-root"),
            "manager-special",
        );
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();

        for parent in ["issue-root", "issue-child"] {
            match index.authorize_create(&project, Some(parent)) {
                Err(BoundaryError::BypassAttempt {
                    parent: seen, root, ..
                }) => {
                    assert_eq!(seen, parent);
                    assert_eq!(root, "issue-root");
                }
                other => panic!("绕过边界建任务应被拒绝，实际 {other:?}"),
            }
        }
        // 在专项外建任务仍允许。
        index.authorize_create(&project, Some("issue-1")).unwrap();
        index.authorize_create(&project, None).unwrap();
    }

    #[test]
    fn top_level_creation_is_project_only() {
        let (store, _root) = seeded("toplevel");
        appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        let special = appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-root"),
            "manager-special",
        );
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();
        assert!(matches!(
            index.authorize_create(&special, None),
            Err(BoundaryError::OutsideScope { .. })
        ));
        assert!(matches!(
            index.authorize_create(&special, Some("issue-1")),
            Err(BoundaryError::OutsideScope { .. })
        ));
    }

    #[test]
    fn nested_special_appointment_needs_governance_and_a_parent_subtree() {
        let (store, _root) = seeded("nested");
        appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-root"),
            "manager-special",
        );
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();

        assert!(matches!(
            index.nested_scope("issue-root", "issue-child", false),
            Err(BoundaryError::NestedRequiresGovernance)
        ));
        assert!(matches!(
            index.nested_scope("issue-root", "issue-1", true),
            Err(BoundaryError::NestedOutsideParent { .. })
        ));
        assert!(matches!(
            index.nested_scope("issue-root", "issue-root", true),
            Err(BoundaryError::NestedOutsideParent { .. })
        ));
        let scope = index
            .nested_scope("issue-root", "issue-child", true)
            .unwrap();
        assert_eq!(
            scope,
            ManagementScope::issue_subtree(PROJECT, "issue-child")
        );
    }

    #[test]
    fn revoked_special_appointment_returns_the_subtree_to_the_project() {
        let (store, _root) = seeded("revoked");
        let project = appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        let mut special = appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-root"),
            "manager-special",
        );
        let version = special.version;
        special
            .revoke(&store, "2026-11-20T12:30:00Z", version, NOW_MS)
            .unwrap();

        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();
        let owner = index.owner_of("issue-child").unwrap();
        assert_eq!(owner.appointment_id, project.appointment_id);
        index.authorize(&project, "issue-child").unwrap();
    }

    #[test]
    fn dangling_parent_and_dangling_special_root_are_rejected() {
        let (store, _root) = seeded("dangling");
        store
            .insert_issue_hierarchy(COMPANY, PROJECT, "issue-1", "issue-missing", NOW_MS)
            .unwrap();
        match BoundaryIndex::load(&store, COMPANY, PROJECT) {
            Err(BoundaryError::DanglingParent { issue, parent }) => {
                assert_eq!(issue, "issue-1");
                assert_eq!(parent, "issue-missing");
            }
            other => panic!("悬空父节点应报错，实际 {other:?}"),
        }

        let root = temp_root("dangling-special");
        let mut store = open_store(&root);
        add_issue(&mut store, "issue-1");
        appoint(
            &store,
            ManagementScope::issue_subtree(PROJECT, "issue-nowhere"),
            "manager-special",
        );
        match BoundaryIndex::load(&store, COMPANY, PROJECT) {
            Err(BoundaryError::DanglingSpecialRoot { root }) => {
                assert_eq!(root, "issue-nowhere");
            }
            other => panic!("专项根不存在应报错，实际 {other:?}"),
        }
    }

    #[test]
    fn hierarchy_cycles_are_rejected_at_load() {
        let (store, _root) = seeded("cycle");
        // 直接写库制造自环（正常写入路径不会允许：CHECK + 一次写定）。
        store
            .execute_batch_for_test(
                "INSERT INTO issue_hierarchy (company_id, project_id, issue_uid, parent_issue_uid,
                    created_at_ms) VALUES ('company-1', 'project-1', 'issue-root', 'issue-child',
                    1)",
            )
            .unwrap();
        match BoundaryIndex::load(&store, COMPANY, PROJECT) {
            Err(BoundaryError::CycleDetected { .. }) => {}
            other => panic!("成环应报错，实际 {other:?}"),
        }
    }

    #[test]
    fn hierarchy_rows_are_immutable() {
        let (store, _root) = seeded("hier-immutable");
        let error = store
            .execute_batch_for_test(
                "UPDATE issue_hierarchy SET parent_issue_uid = 'issue-1' WHERE issue_uid = 'issue-child'",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("hierarchy is immutable"), "实际 {error}");
        let error = store
            .execute_batch_for_test("DELETE FROM issue_hierarchy WHERE issue_uid = 'issue-child'")
            .unwrap_err()
            .to_string();
        assert!(error.contains("hierarchy is immutable"), "实际 {error}");
    }

    #[test]
    fn unknown_issue_is_reported_not_guessed() {
        let (store, _root) = seeded("unknown");
        appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();
        assert!(matches!(
            index.owner_of("issue-missing"),
            Err(BoundaryError::UnknownIssue { .. })
        ));
    }

    /// 组织元数据变更的样本：一个 Issue、一条 Run、一条租约、一条授权、一份项目委任。
    fn org_fixture(tag: &str) -> (SqliteStore, PathBuf) {
        let (mut store, root) = seeded(tag);
        for agent in ["agent-1", "agent-2"] {
            add_member(&store, agent);
        }
        let uow = store.transaction().unwrap();
        uow.insert_run("run-1", COMPANY, "issue-1", "plan-1", "Running")
            .unwrap();
        uow.commit().unwrap();
        store
            .execute_batch_for_test(
                "INSERT INTO leases (resource_key, company_id, owner, lease_token, lease_expires_at, state)
                 VALUES ('workspace:project-1/issue-1', 'company-1', 'agent-1', 7,
                         '2026-11-20T13:00:00Z', 'held')",
            )
            .unwrap();
        store
            .execute_batch_for_test(
                "INSERT INTO grants (principal_id, company_id, project_id, permission, revision, active)
                 VALUES ('agent-1', 'company-1', 'project-1', 'issue.assign', 3, 1)",
            )
            .unwrap();
        appoint(&store, ManagementScope::project(PROJECT), "manager-a");
        (store, root)
    }

    #[test]
    fn reports_to_writes_only_metadata_and_keeps_execution_facts() {
        let (mut store, _root) = org_fixture("reports");
        let before = OrgFacts::capture(&store, COMPANY, PROJECT).unwrap();
        let change = set_reports_to(
            &mut store,
            COMPANY,
            PROJECT,
            "agent-1",
            Some("agent-2"),
            "user-owner",
            NOW_MS + 1_000,
        )
        .unwrap();
        assert_eq!(change.from, None);
        assert_eq!(change.to.as_deref(), Some("agent-2"));
        assert_eq!(change.preserved, before, "执行/授权事实必须逐字段不变");
        assert_eq!(change.preserved.counts.get("runs"), Some(&1));
        assert_eq!(change.preserved.counts.get("leases"), Some(&1));
        assert_eq!(change.preserved.counts.get("grants"), Some(&1));
        assert_eq!(change.preserved.counts.get("appointments"), Some(&1));

        // 落库只有元数据这一张表。
        let row = store.get_org_metadata(COMPANY, "agent-1").unwrap().unwrap();
        assert_eq!(row.reports_to.as_deref(), Some("agent-2"));
        assert_eq!(row.updated_by, "user-owner");
        // Run 与租约逐字段仍相等。
        let runs = store.list_project_runs(COMPANY, PROJECT).unwrap();
        assert_eq!(
            runs,
            vec![(
                "run-1".to_string(),
                "issue-1".to_string(),
                "Running".to_string()
            )]
        );
        let leases = store.list_company_leases(COMPANY).unwrap();
        assert_eq!(leases[0].owner, "agent-1");
        assert_eq!(leases[0].lease_token, 7);
        assert_eq!(leases[0].state, "held");
        // 委任未被动过：同一 id、同一 epoch、仍 Active。
        let appointments = store.list_company_appointments(COMPANY).unwrap();
        assert_eq!(appointments.len(), 1);
        assert_eq!(appointments[0].authority_epoch, 1);
        assert_eq!(appointments[0].status, "active");
    }

    #[test]
    fn reports_to_can_be_cleared_and_records_the_previous_value() {
        let (mut store, _root) = org_fixture("clear");
        set_reports_to(
            &mut store,
            COMPANY,
            PROJECT,
            "agent-1",
            Some("agent-2"),
            "user-owner",
            NOW_MS,
        )
        .unwrap();
        let change = set_reports_to(
            &mut store,
            COMPANY,
            PROJECT,
            "agent-1",
            None,
            "user-owner",
            NOW_MS + 500,
        )
        .unwrap();
        assert_eq!(change.from.as_deref(), Some("agent-2"));
        assert_eq!(change.to, None);
        assert!(
            store
                .get_org_metadata(COMPANY, "agent-1")
                .unwrap()
                .unwrap()
                .reports_to
                .is_none()
        );
    }

    #[test]
    fn reports_to_rejects_self_unknown_and_cycles() {
        let (mut store, _root) = org_fixture("invalid");
        assert!(matches!(
            set_reports_to(
                &mut store,
                COMPANY,
                PROJECT,
                "agent-1",
                Some("agent-1"),
                "user-owner",
                NOW_MS
            ),
            Err(BoundaryError::SelfReport { .. })
        ));
        assert!(matches!(
            set_reports_to(
                &mut store,
                COMPANY,
                PROJECT,
                "agent-1",
                Some("agent-missing"),
                "user-owner",
                NOW_MS
            ),
            Err(BoundaryError::UnknownMember { .. })
        ));
        assert!(matches!(
            set_reports_to(
                &mut store,
                COMPANY,
                PROJECT,
                "agent-missing",
                Some("agent-2"),
                "user-owner",
                NOW_MS
            ),
            Err(BoundaryError::UnknownMember { .. })
        ));

        // agent-2 汇报给 agent-1 之后，agent-1 不能再汇报给 agent-2。
        set_reports_to(
            &mut store,
            COMPANY,
            PROJECT,
            "agent-2",
            Some("agent-1"),
            "user-owner",
            NOW_MS,
        )
        .unwrap();
        assert!(matches!(
            set_reports_to(
                &mut store,
                COMPANY,
                PROJECT,
                "agent-1",
                Some("agent-2"),
                "user-owner",
                NOW_MS
            ),
            Err(BoundaryError::ReportsToCycle { .. })
        ));
    }

    #[test]
    fn reports_to_does_not_move_issue_ownership_or_assignment() {
        let (mut store, _root) = org_fixture("ownership");
        let issues_before = store.list_issues_for_project(COMPANY, PROJECT).unwrap();
        set_reports_to(
            &mut store,
            COMPANY,
            PROJECT,
            "agent-1",
            Some("agent-2"),
            "user-owner",
            NOW_MS,
        )
        .unwrap();
        let issues_after = store.list_issues_for_project(COMPANY, PROJECT).unwrap();
        assert_eq!(issues_before.len(), issues_after.len());
        for (before, after) in issues_before.iter().zip(issues_after.iter()) {
            assert_eq!(before.meta.uid, after.meta.uid);
            assert_eq!(before.spec.assignment, after.spec.assignment);
            assert_eq!(before.status.phase, after.status.phase);
            assert_eq!(before.meta.generation, after.meta.generation);
        }
        // 边界归属也没变：reportsTo 不是委任。
        let index = BoundaryIndex::load(&store, COMPANY, PROJECT).unwrap();
        assert_eq!(index.owner_of("issue-1").unwrap().manager, "manager-a");
    }
}
