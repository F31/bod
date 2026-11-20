//! P3 任务 4.1—4.5：默认扁平协作与执行图准入（`collaboration-planning`）。
//!
//! 这一层管三件事，且都要求"宿主判断、模型只提议"：
//! 1. **提案与权限**（4.1）：未委任顾问只能交候选；派发命令一律拒绝。负责人可在 Grant 内安排。
//! 2. **执行图准入**（4.2/4.3）：环、需求映射、共享资源、权限四项检查通过才形成 `PlanRevision`；
//!    Ready 队列**只**从已接受图算出来，不读 Agent 的自然语言步骤。
//! 3. **会话隔离**（4.4）：资源键含 Company/Project/目标身份/工作类别/执行与委任身份，
//!    同一 Agent 跨项目不共享会话与历史，并发分别计算。
//!
//! 还有一条容易被忽略的：普通答疑**不产生副作用**（4.1 的"默认扁平"）。
//! 按职责路由一个问题，不该顺手派单、改权限或改验收。

use crate::domain::artifact::sha256_hex;
use crate::workspace_safety::{SharedClaim, SharedKind};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

// ---------------------------------------------------------------------------
// 角色与权限
// ---------------------------------------------------------------------------

/// 已委任负责人 / 未委任顾问。两者能做的事不一样，这不是 UI 层的区别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Advisor,
    Owner,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Advisor => "advisor",
            Self::Owner => "owner",
        }
    }
}

/// 委任范围。`scopes` 是允许安排的工作包/需求键；空表示"该范围内全部"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub company: String,
    pub project: String,
    pub scopes: Vec<String>,
}

impl Grant {
    pub fn covers(&self, company: &str, project: &str, scope: &str) -> bool {
        self.company == company
            && self.project == project
            && (self.scopes.is_empty() || self.scopes.iter().any(|item| item == scope))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub id: String,
    pub role: Role,
    pub grant: Option<Grant>,
}

impl Actor {
    pub fn advisor(id: &str) -> Self {
        Self {
            id: id.to_string(),
            role: Role::Advisor,
            grant: None,
        }
    }

    pub fn owner(id: &str, grant: Grant) -> Self {
        Self {
            id: id.to_string(),
            role: Role::Owner,
            grant: Some(grant),
        }
    }
}

// ---------------------------------------------------------------------------
// 提案与修订
// ---------------------------------------------------------------------------

/// 提案固定下来的输入。缺一项就不知道该图基于什么定的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanInputs {
    pub spec_commit: String,
    pub task_map_digest: String,
    pub work_package_revisions: Vec<String>,
    /// 依赖边 `(下游, 上游)`。
    pub edges: Vec<(String, String)>,
    /// 成果要求。
    pub deliverables: Vec<String>,
    pub risks: Vec<String>,
}

impl PlanInputs {
    pub fn digest(&self) -> String {
        let mut revisions = self.work_package_revisions.clone();
        revisions.sort();
        let mut edges = self.edges.clone();
        edges.sort();
        // 风险也进摘要：改了风险评估就是不同的计划，不能沿用旧修订。
        let material = format!(
            "{}|{}|{}|{:?}|{:?}|{:?}",
            self.spec_commit,
            self.task_map_digest,
            revisions.join(","),
            edges,
            self.deliverables,
            self.risks
        );
        format!("sha256:{}", sha256_hex(material.as_bytes()))
    }
}

/// 一条计划项：把某个工作包安排给某个执行者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub package_id: String,
    /// 该包覆盖的需求键（用于 4.2 的映射检查）。
    pub requirements: Vec<String>,
    pub assignee: String,
    pub claims: Vec<SharedClaim>,
}

/// 模型给的计划命令。只有负责人的 `Assign` 会被执行；其余进候选或被拒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanCommand {
    Assign {
        package_id: String,
        assignee: String,
        scope: String,
    },
    Accept {
        package_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedCommand {
    pub command: PlanCommand,
    pub reason: String,
}

/// 提案提交结果。`rejected_commands` 非空**不代表**提案失败——候选部分照旧保留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub proposal_id: String,
    pub author: String,
    pub inputs: PlanInputs,
    pub items: Vec<PlanItem>,
    pub accepted_commands: Vec<PlanCommand>,
    pub rejected_commands: Vec<RejectedCommand>,
    /// 是否已具备形成 `PlanRevision` 的资格（负责人提交才可能为 true）。
    pub eligible_for_revision: bool,
}

/// 已接受的计划修订。Ready 队列只能从它算出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRevision {
    pub revision_id: String,
    pub inputs_digest: String,
    pub items: Vec<PlanItem>,
    pub accepted_by: String,
    pub accepted_at_ms: i64,
}

/// 提交提案。顾问的派发命令被拒，但候选保留。
pub fn submit(
    actor: &Actor,
    proposal_id: &str,
    inputs: PlanInputs,
    items: Vec<PlanItem>,
) -> Submission {
    Submission {
        proposal_id: proposal_id.to_string(),
        author: actor.id.clone(),
        inputs,
        items,
        accepted_commands: Vec::new(),
        rejected_commands: Vec::new(),
        // 顾问不能形成修订；只有负责人提交并经准入检查后才可能。
        eligible_for_revision: actor.role == Role::Owner,
    }
}

/// 处理模型混在提案里的命令。
///
/// 顾问 → 全部拒绝（保留候选）；负责人 → 只有 Grant 覆盖的 `Assign` 通过。
pub fn apply_commands(
    actor: &Actor,
    company: &str,
    project: &str,
    submission: &mut Submission,
    commands: Vec<PlanCommand>,
) -> Vec<RejectedCommand> {
    let mut rejected = Vec::new();
    for command in commands {
        match (&actor.role, &command) {
            (Role::Advisor, _) => rejected.push(RejectedCommand {
                command: command.clone(),
                reason: "未委任顾问只能提交候选；派发/接受命令一律拒绝".to_string(),
            }),
            (
                Role::Owner,
                PlanCommand::Assign {
                    package_id,
                    assignee,
                    scope,
                },
            ) => {
                let Some(grant) = &actor.grant else {
                    rejected.push(RejectedCommand {
                        command: command.clone(),
                        reason: "负责人没有 Grant，不能安排".to_string(),
                    });
                    continue;
                };
                if !grant.covers(company, project, scope) {
                    rejected.push(RejectedCommand {
                        command: command.clone(),
                        reason: format!("{scope} 不在 Grant 范围内"),
                    });
                    continue;
                }
                // 安排落到计划项上：执行者以命令为准。
                if let Some(item) = submission
                    .items
                    .iter_mut()
                    .find(|item| item.package_id == *package_id)
                {
                    item.assignee = assignee.clone();
                    submission.accepted_commands.push(command.clone());
                } else {
                    rejected.push(RejectedCommand {
                        command: command.clone(),
                        reason: "计划项不存在".to_string(),
                    });
                }
            }
            (Role::Owner, PlanCommand::Accept { package_id }) => {
                if submission
                    .items
                    .iter()
                    .any(|item| item.package_id == *package_id)
                {
                    submission.accepted_commands.push(command.clone());
                } else {
                    rejected.push(RejectedCommand {
                        command: command.clone(),
                        reason: "计划项不存在".to_string(),
                    });
                }
            }
        }
    }
    rejected
}

// ---------------------------------------------------------------------------
// 执行图准入
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphProblem {
    /// 依赖成环。
    Cycle(Vec<String>),
    /// 某需求没有任何计划项覆盖。
    UnmappedRequirement(String),
    /// 计划项引用了不存在的需求。
    UnknownRequirement {
        package_id: String,
        requirement: String,
    },
    /// 同一共享资源被两个计划项占用。
    SharedConflict {
        resource: String,
        packages: Vec<String>,
    },
    /// 执行者不在 Grant 内（只在有 Grant 的负责人提交时检查）。
    OutOfGrant {
        package_id: String,
        assignee: String,
    },
    /// 协作建议里的关系未确认。
    UnconfirmedRelation { from: String, to: String },
    /// 顾问提交，不能形成修订。
    NotAnOwner,
    /// 计划项没声明执行者。
    Unassigned { package_id: String },
}

impl fmt::Display for GraphProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cycle(nodes) => write!(f, "依赖成环：{}", nodes.join(" → ")),
            Self::UnmappedRequirement(id) => write!(f, "需求 `{id}` 未被任何计划项覆盖"),
            Self::UnknownRequirement {
                package_id,
                requirement,
            } => write!(f, "计划项 {package_id} 引用了不存在的需求 `{requirement}`"),
            Self::SharedConflict { resource, packages } => write!(
                f,
                "共享资源 `{resource}` 被 {} 同时占用",
                packages.join(", ")
            ),
            Self::OutOfGrant {
                package_id,
                assignee,
            } => write!(f, "{assignee} 不在 {package_id} 的 Grant 范围内"),
            Self::UnconfirmedRelation { from, to } => {
                write!(f, "{from} → {to} 的关系未确认，不能作为准入依据")
            }
            Self::NotAnOwner => write!(f, "未委任顾问不能形成 PlanRevision"),
            Self::Unassigned { package_id } => write!(f, "计划项 {package_id} 没有执行者"),
        }
    }
}

/// 准入检查的输入。
#[derive(Debug, Clone)]
pub struct Admission<'a> {
    pub known_requirements: &'a [String],
    /// 已确认的依赖边。`edges` 里未出现在此处的关系视为未确认。
    pub confirmed_edges: &'a BTreeSet<(String, String)>,
}

/// 检查执行图。返回空才能形成 `PlanRevision`。
pub fn admit(
    actor: &Actor,
    company: &str,
    project: &str,
    items: &[PlanItem],
    inputs: &PlanInputs,
    admission: &Admission<'_>,
) -> Vec<GraphProblem> {
    let mut problems = Vec::new();
    if actor.role != Role::Owner {
        problems.push(GraphProblem::NotAnOwner);
    }

    // 环检测（沿 inputs.edges）
    if let Some(cycle) = find_cycle(&inputs.edges) {
        problems.push(GraphProblem::Cycle(cycle));
    }

    // 需求映射
    let known: BTreeSet<&str> = admission
        .known_requirements
        .iter()
        .map(String::as_str)
        .collect();
    let mut covered: BTreeSet<&str> = BTreeSet::new();
    for item in items {
        if item.assignee.trim().is_empty() {
            problems.push(GraphProblem::Unassigned {
                package_id: item.package_id.clone(),
            });
        }
        if item.requirements.is_empty() {
            problems.push(GraphProblem::UnmappedRequirement(item.package_id.clone()));
        }
        for requirement in &item.requirements {
            if !known.contains(requirement.as_str()) {
                problems.push(GraphProblem::UnknownRequirement {
                    package_id: item.package_id.clone(),
                    requirement: requirement.clone(),
                });
                continue;
            }
            covered.insert(requirement.as_str());
        }
    }
    for requirement in admission.known_requirements {
        if !covered.contains(requirement.as_str()) {
            problems.push(GraphProblem::UnmappedRequirement(requirement.clone()));
        }
    }

    // 共享资源冲突
    let mut owners: BTreeMap<(SharedKind, &str), Vec<&str>> = BTreeMap::new();
    for item in items {
        for claim in &item.claims {
            owners
                .entry((claim.kind, claim.value.as_str()))
                .or_default()
                .push(item.package_id.as_str());
        }
    }
    for ((kind, value), mut packages) in owners {
        if packages.len() > 1 {
            packages.sort_unstable();
            problems.push(GraphProblem::SharedConflict {
                resource: format!("{kind}:{value}"),
                packages: packages.into_iter().map(str::to_string).collect(),
            });
        }
    }

    // 权限：负责人提交时，执行者必须在 Grant 内
    if let Some(grant) = &actor.grant {
        for item in items {
            if !item.assignee.trim().is_empty() && !grant.covers(company, project, &item.package_id)
            {
                problems.push(GraphProblem::OutOfGrant {
                    package_id: item.package_id.clone(),
                    assignee: item.assignee.clone(),
                });
            }
        }
    }

    // 未确认关系：edge 不在 confirmed 集合里 → 不能当准入依据
    for (from, to) in &inputs.edges {
        if !admission
            .confirmed_edges
            .contains(&(from.clone(), to.clone()))
        {
            problems.push(GraphProblem::UnconfirmedRelation {
                from: from.clone(),
                to: to.clone(),
            });
        }
    }

    problems
}

fn find_cycle(edges: &[(String, String)]) -> Option<Vec<String>> {
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (from, to) in edges {
        adjacency
            .entry(from.as_str())
            .or_default()
            .push(to.as_str());
    }
    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut stack: Vec<&str> = Vec::new();

    fn visit<'a>(
        node: &'a str,
        adjacency: &BTreeMap<&'a str, Vec<&'a str>>,
        state: &mut HashMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match state.get(node).copied().unwrap_or(0) {
            1 => {
                let start = stack.iter().position(|item| *item == node).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                cycle.push(node.to_string());
                return Some(cycle);
            }
            2 => return None,
            _ => {}
        }
        state.insert(node, 1);
        stack.push(node);
        if let Some(nexts) = adjacency.get(node) {
            for next in nexts {
                if let Some(cycle) = visit(next, adjacency, state, stack) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        state.insert(node, 2);
        None
    }

    let nodes: Vec<&str> = adjacency.keys().copied().collect();
    for node in nodes {
        if state.get(node).copied().unwrap_or(0) == 0
            && let Some(cycle) = visit(node, &adjacency, &mut state, &mut stack)
        {
            return Some(cycle);
        }
    }
    None
}

/// 形成计划修订。准入不通过就返回问题列表，不产生修订。
pub fn revise(
    actor: &Actor,
    company: &str,
    project: &str,
    submission: &Submission,
    admission: &Admission<'_>,
    now_ms: i64,
) -> Result<PlanRevision, Vec<GraphProblem>> {
    let problems = admit(
        actor,
        company,
        project,
        &submission.items,
        &submission.inputs,
        admission,
    );
    if !problems.is_empty() {
        return Err(problems);
    }
    let material = format!(
        "{}|{}|{}",
        submission.proposal_id,
        submission.inputs.digest(),
        actor.id
    );
    Ok(PlanRevision {
        revision_id: format!("plan-{}", &sha256_hex(material.as_bytes())[..16]),
        inputs_digest: submission.inputs.digest(),
        items: submission.items.clone(),
        accepted_by: actor.id.clone(),
        accepted_at_ms: now_ms,
    })
}

// ---------------------------------------------------------------------------
// Ready 队列
// ---------------------------------------------------------------------------

/// 从已接受图算 Ready 队列：依赖已完成的计划项才进队列。
///
/// 入参是 `(package_id, 依赖的 package_id 列表)`，由 `PlanRevision` + 输入边推导；
/// 这里不接受任何"Agent 说下一步做什么"的文本。
pub fn ready_set(
    revision: &PlanRevision,
    edges: &[(String, String)],
    completed: &BTreeSet<String>,
) -> Vec<String> {
    let mut blocked: BTreeSet<&str> = BTreeSet::new();
    for (from, to) in edges {
        if !completed.contains(to) {
            blocked.insert(from.as_str());
        }
    }
    revision
        .items
        .iter()
        .map(|item| item.package_id.clone())
        .filter(|id| !completed.contains(id) && !blocked.contains(id.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// 计划变更边界
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanChange {
    /// 就地改图（在途 Run 存在时禁止）。
    InPlace { new_edges: Vec<(String, String)> },
    /// 显式暂停后在途 Run，再改图。
    PauseThenChange { new_edges: Vec<(String, String)> },
    /// 新开 Run 承接新图，旧的继续跑完。
    NewRun { revision: String },
    /// 输入变化 → 新工作包修订，而不是改历史约定。
    NewWorkPackageRevision {
        package_id: String,
        revision: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanChangeOutcome {
    Applied { note: String },
    Rejected { reason: String },
}

/// 应用计划变更。
///
/// 规则：在途 Run 存在时，`InPlace` 一律拒绝——不是"警告后继续"。
pub fn apply_change(change: &PlanChange, in_flight_runs: usize) -> PlanChangeOutcome {
    match change {
        PlanChange::InPlace { .. } => {
            if in_flight_runs > 0 {
                PlanChangeOutcome::Rejected {
                    reason: format!(
                        "有 {in_flight_runs} 个在途 Run：就地改图会改变它们正在执行的依赖，\
                         须显式暂停、迁移或新开 Run"
                    ),
                }
            } else {
                PlanChangeOutcome::Applied {
                    note: "无在途 Run，可就地更新".to_string(),
                }
            }
        }
        PlanChange::PauseThenChange { .. } => PlanChangeOutcome::Applied {
            note: "先暂停在途 Run 再改图；恢复时按新图重建 Ready".to_string(),
        },
        PlanChange::NewRun { revision } => PlanChangeOutcome::Applied {
            note: format!("新图由 Run {revision} 承接，在途 Run 继续原计划"),
        },
        PlanChange::NewWorkPackageRevision {
            package_id,
            revision,
        } => PlanChangeOutcome::Applied {
            note: format!("{package_id} 产生新修订 {revision}；历史约定不改写"),
        },
    }
}

// ---------------------------------------------------------------------------
// 会话隔离
// ---------------------------------------------------------------------------

/// 调度资源键。缺一项就可能把两个不同项目的工作塞进同一会话。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionKey {
    pub company: String,
    pub project: String,
    /// 目标身份（Issue/工作包）。
    pub target: String,
    /// 工作类别（实现/复核/答疑…）。
    pub work_kind: String,
    /// 执行身份：同一个 Agent 在不同项目里是不同的执行上下文。
    pub execution_identity: String,
    /// 委任身份：负责人/顾问，影响可做的事。
    pub delegation_identity: String,
}

impl SessionKey {
    pub fn new(
        company: &str,
        project: &str,
        target: &str,
        work_kind: &str,
        execution_identity: &str,
        delegation_identity: &str,
    ) -> Self {
        Self {
            company: company.to_string(),
            project: project.to_string(),
            target: target.to_string(),
            work_kind: work_kind.to_string(),
            execution_identity: execution_identity.to_string(),
            delegation_identity: delegation_identity.to_string(),
        }
    }

    pub fn id(&self) -> String {
        let material = format!(
            "{}|{}|{}|{}|{}|{}",
            self.company,
            self.project,
            self.target,
            self.work_kind,
            self.execution_identity,
            self.delegation_identity
        );
        format!("sess-{}", &sha256_hex(material.as_bytes())[..16])
    }
}

/// 并发上限：按 (agent, project) 分开算，共享 Agent 另有全局上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyLimits {
    pub per_project: u32,
    pub global_per_agent: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyVerdict {
    pub allowed: bool,
    pub reason: String,
}

/// 校验一次调度是否超限。`running_in_project` 与 `running_global` 是当前占用。
pub fn check_concurrency(
    limits: &ConcurrencyLimits,
    running_in_project: u32,
    running_global: u32,
) -> ConcurrencyVerdict {
    if running_in_project >= limits.per_project {
        return ConcurrencyVerdict {
            allowed: false,
            reason: format!(
                "该项目内已占 {} / {}",
                running_in_project, limits.per_project
            ),
        };
    }
    if running_global >= limits.global_per_agent {
        return ConcurrencyVerdict {
            allowed: false,
            reason: format!(
                "该 Agent 全局已占 {} / {}",
                running_global, limits.global_per_agent
            ),
        };
    }
    ConcurrencyVerdict {
        allowed: true,
        reason: "在限额内".to_string(),
    }
}

// ---------------------------------------------------------------------------
// 答疑路由与协作建议
// ---------------------------------------------------------------------------

/// 一次答疑的副作用。全部 false 是默认，也是唯一允许"只路由"的情况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effects {
    pub dispatch: bool,
    pub permission_change: bool,
    pub acceptance_change: bool,
}

impl Effects {
    pub const fn none() -> Self {
        Self {
            dispatch: false,
            permission_change: false,
            acceptance_change: false,
        }
    }

    pub fn is_pure_routing(&self) -> bool {
        !self.dispatch && !self.permission_change && !self.acceptance_change
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    Routed {
        to: Option<String>,
        effects: Effects,
    },
    /// 问题实质在改需求/权限/验收：必须走各自修订或授权流程。
    RequiresRevision { why: String },
}

/// 按职责路由答疑。问题本身若改变需求、权限或验收，返回 `RequiresRevision`。
pub fn route_question(
    question: &str,
    changes_requirements: bool,
    roster: &[(String, String)],
) -> RouteOutcome {
    if changes_requirements {
        return RouteOutcome::RequiresRevision {
            why: "该问题实质在改需求/权限/验收：不能靠答疑解决，须走修订或授权".to_string(),
        };
    }
    let to = roster
        .iter()
        .find(|(_, duty)| !duty.is_empty() && question.contains(duty.as_str()))
        .map(|(agent, _)| agent.clone());
    RouteOutcome::Routed {
        to,
        effects: Effects::none(),
    }
}

/// 协作建议的依据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryBasis {
    pub suggestion_id: String,
    /// 谁提的：模型建议还是确定性规则。
    pub author: String,
    pub kind: BasisKind,
    /// 依据文本（模型必须给出，否则不能进图）。
    pub basis: String,
    pub triggering_revision: String,
    pub risk: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasisKind {
    Deterministic,
    ModelSuggested,
}

/// 建议里的关系状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationState {
    Confirmed,
    Unknown,
}

/// 一条并行建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelSuggestion {
    pub basis: AdvisoryBasis,
    pub left: String,
    pub right: String,
    pub relation: RelationState,
}

/// 建议是否可进执行图。无依据、或关系未知，都不能。
pub fn admit_suggestion(suggestion: &ParallelSuggestion) -> Result<(), GraphProblem> {
    if suggestion.basis.basis.trim().is_empty() {
        return Err(GraphProblem::UnconfirmedRelation {
            from: suggestion.left.clone(),
            to: suggestion.right.clone(),
        });
    }
    match suggestion.relation {
        RelationState::Confirmed => Ok(()),
        RelationState::Unknown => Err(GraphProblem::UnconfirmedRelation {
            from: suggestion.left.clone(),
            to: suggestion.right.clone(),
        }),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> Grant {
        Grant {
            company: "c1".to_string(),
            project: "p1".to_string(),
            scopes: vec!["pkg-a".to_string()],
        }
    }

    fn inputs(edges: Vec<(&str, &str)>) -> PlanInputs {
        PlanInputs {
            spec_commit: "abc123".to_string(),
            task_map_digest: "sha256:taskmap".to_string(),
            work_package_revisions: vec!["wpr-1".to_string(), "wpr-2".to_string()],
            edges: edges
                .into_iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            deliverables: vec!["可运行的增量".to_string()],
            risks: vec!["共享 schema 冲突".to_string()],
        }
    }

    fn item(id: &str, requirement: &str, assignee: &str) -> PlanItem {
        PlanItem {
            package_id: id.to_string(),
            requirements: vec![requirement.to_string()],
            assignee: assignee.to_string(),
            claims: Vec::new(),
        }
    }

    fn admission<'a>(
        requirements: &'a [String],
        confirmed: &'a BTreeSet<(String, String)>,
    ) -> Admission<'a> {
        Admission {
            known_requirements: requirements,
            confirmed_edges: confirmed,
        }
    }

    #[test]
    fn plain_question_is_routed_without_side_effects() {
        let roster = vec![
            ("agent-api".to_string(), "接口".to_string()),
            ("agent-db".to_string(), "数据库".to_string()),
        ];
        match route_question("这个接口的配额字段怎么用", false, &roster) {
            RouteOutcome::Routed { to, effects } => {
                assert_eq!(to.as_deref(), Some("agent-api"));
                assert!(
                    effects.is_pure_routing(),
                    "答疑不得产生派发/权限/验收副作用"
                );
            }
            other => panic!("应为纯路由，实际 {other:?}"),
        }
    }

    #[test]
    fn question_that_changes_scope_requires_a_revision() {
        match route_question("能不能把配额上限改成不限制", true, &[]) {
            RouteOutcome::RequiresRevision { why } => {
                assert!(why.contains("修订或授权"), "实际 {why}");
            }
            other => panic!("应要求走修订，实际 {other:?}"),
        }
    }

    #[test]
    fn advisor_dispatch_commands_are_rejected_while_candidates_survive() {
        let advisor = Actor::advisor("agent-x");
        let mut submission = submit(
            &advisor,
            "prop-1",
            inputs(vec![]),
            vec![item("pkg-a", "req-1", "agent-y")],
        );
        let rejected = apply_commands(
            &advisor,
            "c1",
            "p1",
            &mut submission,
            vec![PlanCommand::Assign {
                package_id: "pkg-a".to_string(),
                assignee: "agent-y".to_string(),
                scope: "pkg-a".to_string(),
            }],
        );
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].reason.contains("只能提交候选"));
        assert_eq!(submission.items.len(), 1, "候选部分必须保留");
        assert!(submission.accepted_commands.is_empty());
        assert!(!submission.eligible_for_revision, "顾问不能形成修订");
    }

    #[test]
    fn owner_can_assign_only_inside_the_grant() {
        let owner = Actor::owner("owner-1", grant());
        let mut submission = submit(
            &owner,
            "prop-2",
            inputs(vec![]),
            vec![item("pkg-a", "req-1", ""), item("pkg-b", "req-2", "")],
        );
        let rejected = apply_commands(
            &owner,
            "c1",
            "p1",
            &mut submission,
            vec![
                PlanCommand::Assign {
                    package_id: "pkg-a".to_string(),
                    assignee: "agent-y".to_string(),
                    scope: "pkg-a".to_string(),
                },
                PlanCommand::Assign {
                    package_id: "pkg-b".to_string(),
                    assignee: "agent-z".to_string(),
                    scope: "pkg-b".to_string(),
                },
            ],
        );
        assert_eq!(submission.accepted_commands.len(), 1);
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].reason.contains("不在 Grant 范围内"));
        assert_eq!(submission.items[0].assignee, "agent-y");
        assert_eq!(submission.items[1].assignee, "", "越权的安排不生效");
    }

    #[test]
    fn cycle_graph_is_rejected_and_produces_no_ready_items() {
        let owner = Actor::owner("owner-1", grant());
        let requirements = vec!["req-1".to_string()];
        let confirmed: BTreeSet<(String, String)> = [
            ("pkg-a".to_string(), "pkg-b".to_string()),
            ("pkg-b".to_string(), "pkg-a".to_string()),
        ]
        .into_iter()
        .collect();
        let submission = submit(
            &owner,
            "prop-3",
            inputs(vec![("pkg-a", "pkg-b"), ("pkg-b", "pkg-a")]),
            vec![item("pkg-a", "req-1", "agent-y")],
        );
        let problems = admit(
            &owner,
            "c1",
            "p1",
            &submission.items,
            &submission.inputs,
            &admission(&requirements, &confirmed),
        );
        assert!(
            problems.iter().any(|p| matches!(p, GraphProblem::Cycle(_))),
            "成环必须被拒，实际 {problems:?}"
        );
        assert!(
            revise(
                &owner,
                "c1",
                "p1",
                &submission,
                &admission(&requirements, &confirmed),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn graph_missing_requirement_mapping_is_rejected() {
        let owner = Actor::owner("owner-1", grant());
        let requirements = vec!["req-1".to_string(), "req-2".to_string()];
        let confirmed: BTreeSet<(String, String)> = BTreeSet::new();
        let submission = submit(
            &owner,
            "prop-4",
            inputs(vec![]),
            vec![item("pkg-a", "req-1", "agent-y")],
        );
        let problems = admit(
            &owner,
            "c1",
            "p1",
            &submission.items,
            &submission.inputs,
            &admission(&requirements, &confirmed),
        );
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, GraphProblem::UnmappedRequirement(id) if id == "req-2")),
            "未覆盖的需求要报，实际 {problems:?}"
        );
    }

    #[test]
    fn shared_resource_conflict_blocks_admission() {
        let owner = Actor::owner("owner-1", grant());
        let requirements = vec!["req-1".to_string()];
        let confirmed: BTreeSet<(String, String)> = BTreeSet::new();
        let mut a = item("pkg-a", "req-1", "agent-y");
        let mut b = item("pkg-b", "req-1", "agent-z");
        a.claims = vec![SharedClaim::new(SharedKind::ApiSchema, "quota.v2", "pkg-a")];
        b.claims = vec![SharedClaim::new(SharedKind::ApiSchema, "quota.v2", "pkg-b")];
        let submission = submit(&owner, "prop-5", inputs(vec![]), vec![a, b]);
        let problems = admit(
            &owner,
            "c1",
            "p1",
            &submission.items,
            &submission.inputs,
            &admission(&requirements, &confirmed),
        );
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, GraphProblem::SharedConflict { .. })),
            "共享 schema 冲突要报，实际 {problems:?}"
        );
    }

    #[test]
    fn ready_queue_comes_from_the_accepted_graph_not_natural_language() {
        // 这里只测图语义，用覆盖全部工作包的 Grant 排除权限干扰。
        let owner = Actor::owner(
            "owner-1",
            Grant {
                company: "c1".to_string(),
                project: "p1".to_string(),
                scopes: Vec::new(),
            },
        );
        let requirements = vec!["req-1".to_string()];
        let mut confirmed: BTreeSet<(String, String)> = BTreeSet::new();
        confirmed.insert(("pkg-b".to_string(), "pkg-a".to_string()));
        let submission = submit(
            &owner,
            "prop-6",
            inputs(vec![("pkg-b", "pkg-a")]),
            vec![
                item("pkg-a", "req-1", "agent-y"),
                item("pkg-b", "req-1", "agent-z"),
            ],
        );
        let revision = revise(
            &owner,
            "c1",
            "p1",
            &submission,
            &admission(&requirements, &confirmed),
            2,
        )
        .expect("应形成修订");

        // pkg-b 依赖 pkg-a：a 未完成时只有 a 在 Ready。
        let ready = ready_set(&revision, &submission.inputs.edges, &BTreeSet::new());
        assert_eq!(ready, vec!["pkg-a".to_string()]);
        let mut completed = BTreeSet::new();
        completed.insert("pkg-a".to_string());
        let ready = ready_set(&revision, &submission.inputs.edges, &completed);
        assert_eq!(ready, vec!["pkg-b".to_string()]);
    }

    #[test]
    fn in_flight_run_blocks_in_place_edit() {
        let outcome = apply_change(
            &PlanChange::InPlace {
                new_edges: vec![("pkg-a".to_string(), "pkg-b".to_string())],
            },
            2,
        );
        match outcome {
            PlanChangeOutcome::Rejected { reason } => {
                assert!(reason.contains("2 个在途 Run"), "实际 {reason}");
                assert!(reason.contains("新开 Run"));
            }
            other => panic!("在途 Run 存在时就地改图应被拒，实际 {other:?}"),
        }

        // 显式暂停/新 Run 是允许的路径。
        assert!(matches!(
            apply_change(&PlanChange::PauseThenChange { new_edges: vec![] }, 2),
            PlanChangeOutcome::Applied { .. }
        ));
        assert!(matches!(
            apply_change(
                &PlanChange::NewRun {
                    revision: "plan-v2".to_string()
                },
                2
            ),
            PlanChangeOutcome::Applied { .. }
        ));
    }

    #[test]
    fn new_input_creates_a_work_package_revision_instead_of_history_rewrite() {
        match apply_change(
            &PlanChange::NewWorkPackageRevision {
                package_id: "pkg-a".to_string(),
                revision: "wpr-3".to_string(),
            },
            1,
        ) {
            PlanChangeOutcome::Applied { note } => {
                assert!(note.contains("历史约定不改写"), "实际 {note}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn same_agent_in_two_projects_gets_distinct_sessions_and_separate_limits() {
        let a = SessionKey::new(
            "c1",
            "p1",
            "issue-1",
            "implement",
            "agent-shared",
            "owner-1",
        );
        let b = SessionKey::new(
            "c1",
            "p2",
            "issue-2",
            "implement",
            "agent-shared",
            "owner-1",
        );
        assert_ne!(a.id(), b.id(), "跨项目不得合并到同一会话");

        // 同一个键重复构造必须稳定（否则每次调度都新建会话）。
        let a2 = SessionKey::new(
            "c1",
            "p1",
            "issue-1",
            "implement",
            "agent-shared",
            "owner-1",
        );
        assert_eq!(a.id(), a2.id());

        // 工作类别不同也不能共用一个会话。
        let review = SessionKey::new("c1", "p1", "issue-1", "review", "agent-shared", "owner-1");
        assert_ne!(a.id(), review.id());

        // 并发分别计算：项目内满了，但全局没满。
        let limits = ConcurrencyLimits {
            per_project: 2,
            global_per_agent: 5,
        };
        assert!(!check_concurrency(&limits, 2, 2).allowed);
        assert!(check_concurrency(&limits, 0, 4).allowed);
        assert!(!check_concurrency(&limits, 0, 5).allowed);
    }

    #[test]
    fn unfounded_parallel_suggestion_is_not_admitted() {
        let suggestion = ParallelSuggestion {
            basis: AdvisoryBasis {
                suggestion_id: "sug-1".to_string(),
                author: "model".to_string(),
                kind: BasisKind::ModelSuggested,
                basis: String::new(),
                triggering_revision: "plan-1".to_string(),
                risk: "可能冲突".to_string(),
            },
            left: "pkg-a".to_string(),
            right: "pkg-b".to_string(),
            relation: RelationState::Unknown,
        };
        assert!(matches!(
            admit_suggestion(&suggestion),
            Err(GraphProblem::UnconfirmedRelation { .. })
        ));

        let mut confirmed = suggestion.clone();
        confirmed.relation = RelationState::Confirmed;
        confirmed.basis.basis = "两个包只改各自模块，无共享资源".to_string();
        assert!(admit_suggestion(&confirmed).is_ok());
    }

    #[test]
    fn unconfirmed_edges_cannot_be_used_for_admission() {
        let owner = Actor::owner("owner-1", grant());
        let requirements = vec!["req-1".to_string()];
        let confirmed: BTreeSet<(String, String)> = BTreeSet::new();
        let submission = submit(
            &owner,
            "prop-7",
            inputs(vec![("pkg-b", "pkg-a")]),
            vec![item("pkg-a", "req-1", "agent-y")],
        );
        let problems = admit(
            &owner,
            "c1",
            "p1",
            &submission.items,
            &submission.inputs,
            &admission(&requirements, &confirmed),
        );
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, GraphProblem::UnconfirmedRelation { .. })),
            "未确认的关系不能当准入依据，实际 {problems:?}"
        );
    }

    #[test]
    fn plan_revision_records_basis_trigger_and_risk() {
        let owner = Actor::owner("owner-1", grant());
        let requirements = vec!["req-1".to_string()];
        let confirmed: BTreeSet<(String, String)> = BTreeSet::new();
        let inputs = inputs(vec![]);
        let digest = inputs.digest();
        let submission = submit(
            &owner,
            "prop-8",
            inputs,
            vec![item("pkg-a", "req-1", "agent-y")],
        );
        let revision = revise(
            &owner,
            "c1",
            "p1",
            &submission,
            &admission(&requirements, &confirmed),
            9,
        )
        .unwrap();
        assert_eq!(revision.inputs_digest, digest);
        assert_eq!(revision.accepted_by, "owner-1");
        assert_eq!(revision.accepted_at_ms, 9);
        assert!(revision.revision_id.starts_with("plan-"));
        // 风险与成果要求进 digest：改了其中任一，修订就不同。
        let mut altered = submission.inputs.clone();
        altered.risks.push("新增风险".to_string());
        assert_ne!(altered.digest(), digest);
    }

    #[test]
    fn advisor_cannot_form_a_plan_revision_even_with_a_clean_graph() {
        let advisor = Actor::advisor("agent-x");
        let requirements = vec!["req-1".to_string()];
        let confirmed: BTreeSet<(String, String)> = BTreeSet::new();
        let submission = submit(
            &advisor,
            "prop-9",
            inputs(vec![]),
            vec![item("pkg-a", "req-1", "agent-y")],
        );
        let problems = admit(
            &advisor,
            "c1",
            "p1",
            &submission.items,
            &submission.inputs,
            &admission(&requirements, &confirmed),
        );
        assert!(problems.contains(&GraphProblem::NotAnOwner));
    }
}
