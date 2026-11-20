//! P3 任务 3.1—3.5：多工作区协作与受控合并（`multi-agent-workspace`）。
//!
//! 三件事分开做，因为它们的失败后果不同：
//! 1. **隔离**（3.1）：每个 Attempt 一个 worktree/分支。worktree 只能叫"协作隔离"，
//!    不是安全沙箱——这层要说清楚，否则会有人拿它隔离不可信代码。
//! 2. **合并队列**（3.2—3.4）：串行、按最新目标重建、证据与实际 ref 绑定。
//!    单分支绿灯不算证据；"我测过了"也不算，`MergeRecord` 对不上就不接受。
//! 3. **契约冲突门**（3.2）：同一公共 schema 被两个包改，先到先得，后来者被拦住，
//!    必须责任者协调后重新排队。
//!
//! 这一层不执行 git 命令：它产出的 [`VerificationTicket`] 与 [`MergeRecord`] 由宿主
//! 交给真实 git 执行；测试里用真实 `git` 验证"两个 worktree 互不可见"这一条。

use crate::domain::artifact::sha256_hex;
use crate::workspace_safety::{SharedClaim, SharedConflict};
use std::collections::VecDeque;
use std::fmt;

// ---------------------------------------------------------------------------
// 隔离声明
// ---------------------------------------------------------------------------

/// 隔离提供方：谁在提供隔离，决定它能被称作什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationProvider {
    /// 同一仓的 git worktree：分支与索引独立，但共用文件系统与进程权限。
    Worktree,
    /// 独立 clone：磁盘与索引独立，仍共用宿主环境。
    Clone,
    /// 容器：文件系统、网络、凭据都可受限。
    Container,
    /// 远程工作区：最强，代价最高。
    RemoteWorkspace,
}

impl IsolationProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Clone => "clone",
            Self::Container => "container",
            Self::RemoteWorkspace => "remote",
        }
    }
}

/// 隔离等级。`Strong` 才能承接"不可信执行"一类要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// 协作隔离：防止互相踩文件，不防恶意代码。
    Collaborative,
    /// 安全沙箱：限制写路径、软链接穿越、Git 管理目录访问与凭据。
    Strong,
}

impl IsolationLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Collaborative => "collaborative",
            Self::Strong => "strong",
        }
    }
}

/// 适配器自报的隔离能力 + 实际提供方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationDeclaration {
    pub adapter: String,
    pub declared: IsolationLevel,
    pub provider: IsolationProvider,
}

/// 判定结果。被降级时 `note` 必填，且会进入报告——不能悄悄降级。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationVerdict {
    pub effective: IsolationLevel,
    pub note: Option<String>,
}

/// 按**实际提供方**判定隔离等级，适配器声明不能越级。
///
/// 这条规则是 G-29 的核心：worktree 与 clone 共用宿主文件系统与进程权限，
/// 只有容器/远程工作区才可能构成安全边界。声明与事实不符时降级并留痕。
pub fn resolve_isolation(declaration: &IsolationDeclaration) -> IsolationVerdict {
    let capability = match declaration.provider {
        IsolationProvider::Worktree | IsolationProvider::Clone => IsolationLevel::Collaborative,
        IsolationProvider::Container | IsolationProvider::RemoteWorkspace => IsolationLevel::Strong,
    };
    if capability == IsolationLevel::Strong {
        // 强隔离还需 runner 真正限制写路径/软链接/Git 目录；这里只承接"提供方具备"，
        // 具体限制由 runner 能力另测（见 `DEFAULT_WRITE_LIMITS`）。
        return IsolationVerdict {
            effective: IsolationLevel::Strong,
            note: None,
        };
    }
    if declaration.declared == IsolationLevel::Strong {
        return IsolationVerdict {
            effective: IsolationLevel::Collaborative,
            note: Some(format!(
                "适配器 {} 声明 sandboxed，但实际提供方为 {}：worktree/clone 共用宿主文件系统与进程权限，\
                 只能算协作隔离（G-29）",
                declaration.adapter,
                declaration.provider.as_str()
            )),
        };
    }
    IsolationVerdict {
        effective: IsolationLevel::Collaborative,
        note: None,
    }
}

/// runner 必须施加的写限制。worktree 提供方即使做不到也不许自称沙箱。
pub const DEFAULT_WRITE_LIMITS: &[&str] = &[
    "write_paths",
    "symlink_traversal",
    "git_admin_dir",
    "credential_scope",
];

// ---------------------------------------------------------------------------
// 合并队列
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    UnknownRequest(String),
    /// 队列为空。
    EmptyQueue,
    /// 校验期间主干前进：必须重建候选并重新验证（3.3）。
    TargetMoved {
        ticket_target: String,
        current_target: String,
    },
    /// 证据里的目标与实际更新的 ref 不一致（3.4）。
    EvidenceMismatch {
        expected: String,
        actual: String,
    },
    /// 不是队首：不许插队。
    NotAtHead {
        request_id: String,
        head: Option<String>,
    },
    /// 验证本身失败。
    VerificationFailed {
        reason: String,
    },
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRequest(id) => write!(f, "未知合并请求 {id}"),
            Self::EmptyQueue => write!(f, "合并队列为空"),
            Self::TargetMoved {
                ticket_target,
                current_target,
            } => write!(
                f,
                "校验期间主干前进了（{ticket_target} → {current_target}）：旧结果不能用于接受，须重建候选重验"
            ),
            Self::EvidenceMismatch { expected, actual } => write!(
                f,
                "合并证据绑定 {expected}，实际更新 {actual}：拒绝接受并要求重新验证"
            ),
            Self::NotAtHead { request_id, head } => {
                write!(f, "{request_id} 不是队首（队首是 {head:?}），合并必须串行")
            }
            Self::VerificationFailed { reason } => {
                write!(f, "候选验证未通过：{reason}")
            }
        }
    }
}

impl std::error::Error for MergeError {}

/// 一次候选合并的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCandidate {
    pub candidate_id: String,
    pub branch: String,
    /// 候选分支的提交。
    pub head_commit: String,
    /// 候选是基于哪个目标提交构造的。
    pub base_target: String,
    /// 该候选修改的共享资源。
    pub claims: Vec<SharedClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequest {
    pub request_id: String,
    pub candidate: MergeCandidate,
    pub target_ref: String,
}

/// 队列条目状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
    Queued,
    /// 正在 `onto` + 候选组合上验证。
    Verifying {
        onto: String,
        combination: String,
    },
    Rejected {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub request: MergeRequest,
    pub state: EntryState,
    /// 已重建次数：报告里要能看出"这个候选被重建过几次"。
    pub rebuilds: u32,
}

/// 验证命令与结果，进证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyCommand {
    pub command: String,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    Passed { commands: Vec<VerifyCommand> },
    Failed { reason: String },
}

/// 合并证据：必须对应真实组合，且期望目标与实际更新 ref 一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRecord {
    pub request_id: String,
    pub candidate_id: String,
    pub branch: String,
    pub candidate_head: String,
    pub target_ref: String,
    /// 验证时所在的目标提交。
    pub verified_on: String,
    /// 校验通过的组合标识（`target + candidate`）。
    pub combination: String,
    /// 实际被更新的 ref 值。
    pub actual_ref: String,
    pub commands: Vec<VerifyCommand>,
    pub conflict_resolution: Option<String>,
    pub rebuilds: u32,
    pub accepted_at_ms: i64,
}

impl MergeRecord {
    /// 证据是否与实际组合绑定。false 时不得作为接受依据。
    pub fn is_bound(&self) -> bool {
        self.combination == self.actual_ref
    }
}

/// 校验票据。由 `begin_verification` 发出，`finish_verification` 核销。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationTicket {
    pub request_id: String,
    pub target_head: String,
    pub combination: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued {
        position: usize,
    },
    /// 契约冲突：被拦住，需责任者协调后重新提交。
    Blocked {
        conflicts: Vec<SharedConflict>,
    },
}

/// 串行合并队列。
#[derive(Debug, Default)]
pub struct MergeQueue {
    target_ref: String,
    entries: VecDeque<QueueEntry>,
    /// 已接受候选占用的共享资源。
    accepted_claims: Vec<SharedClaim>,
    records: Vec<MergeRecord>,
    blocked: Vec<(String, Vec<SharedConflict>)>,
    /// 被拒绝的候选（含原因）。记录保留：报告要能看出"谁被拒、为什么"。
    rejected: Vec<(String, String)>,
}

impl MergeQueue {
    pub fn new(target_ref: &str) -> Self {
        Self {
            target_ref: target_ref.to_string(),
            ..Self::default()
        }
    }

    pub fn target_ref(&self) -> &str {
        &self.target_ref
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn head(&self) -> Option<&QueueEntry> {
        self.entries.front()
    }

    pub fn entries(&self) -> impl Iterator<Item = &QueueEntry> {
        self.entries.iter()
    }

    pub fn records(&self) -> &[MergeRecord] {
        &self.records
    }

    /// 被契约冲突拦下的请求（保留记录，供报告点名）。
    pub fn blocked(&self) -> &[(String, Vec<SharedConflict>)] {
        &self.blocked
    }

    /// 被拒绝的候选：`(request_id, 原因)`。
    pub fn rejected(&self) -> &[(String, String)] {
        &self.rejected
    }

    /// 入队。与已接受或已在队列中的候选共享资源重叠时**直接拦下**。
    ///
    /// 为什么不是"排到后面等"：两个包改同一份公共 schema，先后合并会得到一个
    /// 谁也没验证过的组合；正确做法是责任者先协调成一份改动，再重新提交。
    pub fn enqueue(&mut self, request: MergeRequest) -> EnqueueOutcome {
        let mut conflicts = Vec::new();
        let mut probe = ClaimIndex::default();
        for claim in &self.accepted_claims {
            probe.add(claim, "已接受");
        }
        for entry in &self.entries {
            for claim in &entry.request.candidate.claims {
                probe.add(claim, &entry.request.request_id);
            }
        }
        for claim in &request.candidate.claims {
            if let Some(holders) = probe.holders(claim) {
                conflicts.push(SharedConflict {
                    kind: claim.kind,
                    value: claim.value.clone(),
                    holders,
                    message: format!(
                        "共享资源 {} `{}` 已被占用，需责任者协调后重新提交",
                        claim.kind, claim.value
                    ),
                });
            }
        }
        if !conflicts.is_empty() {
            self.blocked
                .push((request.request_id.clone(), conflicts.clone()));
            return EnqueueOutcome::Blocked { conflicts };
        }
        let position = self.entries.len();
        self.entries.push_back(QueueEntry {
            request,
            state: EntryState::Queued,
            rebuilds: 0,
        });
        EnqueueOutcome::Queued { position }
    }

    /// 开始校验队首候选。
    ///
    /// `target_head` 是**当前**目标提交。若与候选 `base_target` 不同，说明候选需要在
    /// 新目标上重建——状态进 `Rebuilding`，`rebuilds` 加一，组合标识按新目标重算。
    pub fn begin_verification(
        &mut self,
        target_head: &str,
    ) -> Result<VerificationTicket, MergeError> {
        let Some(entry) = self.entries.front_mut() else {
            return Err(MergeError::EmptyQueue);
        };
        // 目标变了就必须在新目标上重建：组合标识随之改变，旧结果自然作废。
        if entry.request.candidate.base_target != target_head {
            entry.rebuilds += 1;
        }
        let combination = combination_id(
            target_head,
            &entry.request.candidate.branch,
            &entry.request.candidate.head_commit,
        );
        entry.state = EntryState::Verifying {
            onto: target_head.to_string(),
            combination: combination.clone(),
        };
        Ok(VerificationTicket {
            request_id: entry.request.request_id.clone(),
            target_head: target_head.to_string(),
            combination,
        })
    }

    /// 结束校验并决定是否接受。
    ///
    /// 三道门，顺序固定：
    /// 1. 票据必须属于队首（串行）；
    /// 2. 目标在验证期间不能动（动了就重建重验）；
    /// 3. 实际更新的 ref 必须等于验证组合（否则证据与事实不符）。
    pub fn finish_verification(
        &mut self,
        ticket: &VerificationTicket,
        outcome: VerifyOutcome,
        actual_ref: &str,
        current_target: &str,
        now_ms: i64,
    ) -> Result<MergeRecord, MergeError> {
        let Some(entry) = self.entries.front_mut() else {
            return Err(MergeError::EmptyQueue);
        };
        if entry.request.request_id != ticket.request_id {
            return Err(MergeError::NotAtHead {
                request_id: ticket.request_id.clone(),
                head: Some(entry.request.request_id.clone()),
            });
        }
        match &entry.state {
            EntryState::Verifying { onto, combination }
                if *combination == ticket.combination && *onto == ticket.target_head => {}
            other => {
                return Err(MergeError::NotAtHead {
                    request_id: ticket.request_id.clone(),
                    head: Some(format!("状态不是可核销的验证中：{other:?}")),
                });
            }
        }

        // 门 2：主干在校验期间前进 → 本次结果作废，回到 Queued 等重建。
        // 不把状态改成 Rejected：候选本身没问题，是基线变了。
        if current_target != ticket.target_head {
            entry.state = EntryState::Queued;
            return Err(MergeError::TargetMoved {
                ticket_target: ticket.target_head.clone(),
                current_target: current_target.to_string(),
            });
        }

        // 门 3 之前的验证结果：失败即拒绝，且不留证据（没通过的东西不该进记录）。
        if let VerifyOutcome::Failed { reason } = &outcome {
            let reason = reason.clone();
            entry.state = EntryState::Rejected {
                reason: reason.clone(),
            };
            let rejected = self.entries.pop_front().expect("上面已确认队首存在");
            self.rejected
                .push((rejected.request.request_id.clone(), reason.clone()));
            return Err(MergeError::VerificationFailed { reason });
        }

        let commands = match outcome {
            VerifyOutcome::Passed { commands } => commands,
            VerifyOutcome::Failed { .. } => unreachable!("上面已处理失败分支"),
        };

        // 门 3：实际更新的 ref 必须等于验证组合，否则证据与事实不符（3.4）。
        if actual_ref != ticket.combination {
            let why = "实际更新 ref 与验证组合不一致".to_string();
            entry.state = EntryState::Rejected {
                reason: why.clone(),
            };
            let rejected = self.entries.pop_front().expect("上面已确认队首存在");
            self.rejected
                .push((rejected.request.request_id.clone(), why));
            return Err(MergeError::EvidenceMismatch {
                expected: ticket.combination.clone(),
                actual: actual_ref.to_string(),
            });
        }

        let accepted = self.entries.pop_front().expect("上面已确认队首存在");
        let record = MergeRecord {
            request_id: accepted.request.request_id.clone(),
            candidate_id: accepted.request.candidate.candidate_id.clone(),
            branch: accepted.request.candidate.branch.clone(),
            candidate_head: accepted.request.candidate.head_commit.clone(),
            target_ref: self.target_ref.clone(),
            verified_on: ticket.target_head.clone(),
            combination: ticket.combination.clone(),
            actual_ref: actual_ref.to_string(),
            commands,
            conflict_resolution: None,
            rebuilds: accepted.rebuilds,
            accepted_at_ms: now_ms,
        };
        // 候选的共享资源转为"已接受占用"，后续候选会据此被判冲突。
        for claim in &accepted.request.candidate.claims {
            self.accepted_claims.push(claim.clone());
        }
        self.records.push(record.clone());
        Ok(record)
    }

    /// 队列里是否还有需要重建的候选（目标前进后）。
    pub fn needs_rebuild(&self, target_head: &str) -> bool {
        self.entries
            .front()
            .is_some_and(|entry| entry.request.candidate.base_target != target_head)
    }
}

/// 内部用的占用索引：同一 (kind, value) 允许多个持有者，用于报冲突。
#[derive(Default)]
struct ClaimIndex {
    holders: Vec<(String, String, Vec<String>)>,
}

impl ClaimIndex {
    fn add(&mut self, claim: &SharedClaim, holder: &str) {
        if let Some(entry) = self
            .holders
            .iter_mut()
            .find(|(kind, value, _)| kind == claim.kind.as_str() && value == &claim.value)
        {
            if !entry.2.iter().any(|item| item == holder) {
                entry.2.push(holder.to_string());
            }
            return;
        }
        self.holders.push((
            claim.kind.as_str().to_string(),
            claim.value.clone(),
            vec![holder.to_string()],
        ));
    }

    fn holders(&self, claim: &SharedClaim) -> Option<Vec<String>> {
        self.holders
            .iter()
            .find(|(kind, value, _)| kind == claim.kind.as_str() && value == &claim.value)
            .map(|(_, _, holders)| {
                let mut sorted = holders.clone();
                sorted.sort();
                sorted
            })
    }
}

/// 组合标识：目标 + 分支 + 候选提交。三者任一变化都必须重验。
fn combination_id(target_head: &str, branch: &str, candidate_head: &str) -> String {
    let material = format!("{target_head}|{branch}|{candidate_head}");
    format!("merge-{}", &sha256_hex(material.as_bytes())[..16])
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_safety::{SharedKind, WorkspaceManager};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let seq = SEQ.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("bod-merge-{tag}-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            Self { path }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|err| panic!("git {args:?} 无法执行: {err}"));
        assert!(
            output.status.success(),
            "git {args:?} 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(repo: &Path) -> String {
        fs::create_dir_all(repo).expect("建仓库目录");
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "dev@bod.local"]);
        git(repo, &["config", "user.name", "bod-test"]);
        fs::write(repo.join("README.md"), "base\n").expect("写基线文件");
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "base"]);
        git(repo, &["rev-parse", "HEAD"])
    }

    fn candidate(id: &str, base: &str, claims: Vec<SharedClaim>) -> MergeCandidate {
        MergeCandidate {
            candidate_id: id.to_string(),
            branch: format!("bod/{id}"),
            head_commit: format!("head-{id}"),
            base_target: base.to_string(),
            claims,
        }
    }

    fn request(id: &str, base: &str, claims: Vec<SharedClaim>) -> MergeRequest {
        MergeRequest {
            request_id: format!("req-{id}"),
            candidate: candidate(id, base, claims),
            target_ref: "refs/heads/main".to_string(),
        }
    }

    /// 3.1：两个候选互不读取对方未提交内容，主干 ref 不被分支提交推动。
    #[test]
    fn candidates_cannot_see_uncommitted_work_and_main_only_moves_through_the_entry() {
        let root = TempRoot::new("iso");
        let repo = root.path.join("repo");
        let base = init_repo(&repo);
        let manager = WorkspaceManager::new(&repo, root.path.join("wt"));

        let a = manager.create("attempt-a", "bod/attempt-a", &base).unwrap();
        let b = manager.create("attempt-b", "bod/attempt-b", &base).unwrap();
        assert_ne!(a.path, b.path, "每个 Attempt 必须有独立 worktree");

        // A 写一个未提交文件
        fs::write(a.path.join("only-in-a.txt"), "a\n").unwrap();
        assert_eq!(
            manager.changed_paths(&a).unwrap(),
            vec!["only-in-a.txt".to_string()]
        );
        assert!(
            manager.changed_paths(&b).unwrap().is_empty(),
            "B 不应看到 A 的未提交内容"
        );
        assert!(!b.path.join("only-in-a.txt").exists());

        // A 提交：只推动自己的分支，主干不动
        git(&a.path, &["add", "."]);
        git(&a.path, &["commit", "-q", "-m", "candidate a"]);
        let head_a = git(&a.path, &["rev-parse", "HEAD"]);
        assert_eq!(
            git(&repo, &["rev-parse", "main"]),
            base,
            "分支提交不得推动主干 ref（I-20）"
        );
        assert_eq!(git(&repo, &["rev-parse", "bod/attempt-a"]), head_a);
    }

    /// 3.2：两个包改同一公共 schema → 后到的被契约冲突门拦下。
    #[test]
    fn second_writer_of_the_same_schema_is_blocked() {
        let mut queue = MergeQueue::new("refs/heads/main");
        let base = "c0";
        let claim = |attempt: &str| SharedClaim::new(SharedKind::ApiSchema, "quota.v2", attempt);
        assert!(matches!(
            queue.enqueue(request("a", base, vec![claim("attempt-a")])),
            EnqueueOutcome::Queued { .. }
        ));
        match queue.enqueue(request("b", base, vec![claim("attempt-b")])) {
            EnqueueOutcome::Blocked { conflicts } => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].kind, SharedKind::ApiSchema);
                assert_eq!(conflicts[0].value, "quota.v2");
                assert!(conflicts[0].holders.contains(&"req-a".to_string()));
                assert!(conflicts[0].message.contains("责任者协调"));
            }
            other => panic!("应被拦下，实际 {other:?}"),
        }
        assert_eq!(queue.len(), 1, "被拦下的请求不进队列");
        assert_eq!(queue.blocked().len(), 1, "拦下要留记录，报告要点名");
    }

    /// 3.3：校验期间主干前进 → 旧结果作废，必须重建重验。
    #[test]
    fn target_moving_during_verification_invalidates_the_result() {
        let mut queue = MergeQueue::new("refs/heads/main");
        queue.enqueue(request("a", "c0", vec![]));
        let ticket = queue.begin_verification("c0").unwrap();
        assert_eq!(ticket.target_head, "c0");

        let err = queue
            .finish_verification(
                &ticket,
                VerifyOutcome::Passed {
                    commands: vec![VerifyCommand {
                        command: "cargo test".to_string(),
                        passed: true,
                    }],
                },
                &ticket.combination,
                "c1",
                10,
            )
            .unwrap_err();
        match err {
            MergeError::TargetMoved {
                ticket_target,
                current_target,
            } => {
                assert_eq!(ticket_target, "c0");
                assert_eq!(current_target, "c1");
            }
            other => panic!("应为目标前进，实际 {other:?}"),
        }
        assert!(queue.records().is_empty(), "旧组合不得留下接受记录");
        assert_eq!(queue.head().unwrap().state, EntryState::Queued);
        assert!(queue.needs_rebuild("c1"));

        // 在 c1 上重建：组合标识必须变，rebuilds 记 1。
        let rebuilt = queue.begin_verification("c1").unwrap();
        assert_ne!(rebuilt.combination, ticket.combination);
        assert_eq!(queue.head().unwrap().rebuilds, 1);
        let record = queue
            .finish_verification(
                &rebuilt,
                VerifyOutcome::Passed {
                    commands: vec![VerifyCommand {
                        command: "cargo test".to_string(),
                        passed: true,
                    }],
                },
                &rebuilt.combination,
                "c1",
                11,
            )
            .unwrap();
        assert!(record.is_bound());
        assert_eq!(record.verified_on, "c1");
        assert_eq!(record.rebuilds, 1);
    }

    /// 3.4：证据与实际更新的 ref 不一致 → 拒绝接受。
    #[test]
    fn evidence_must_match_the_ref_that_was_actually_updated() {
        let mut queue = MergeQueue::new("refs/heads/main");
        queue.enqueue(request("a", "c0", vec![]));
        let ticket = queue.begin_verification("c0").unwrap();
        let err = queue
            .finish_verification(
                &ticket,
                VerifyOutcome::Passed {
                    commands: vec![VerifyCommand {
                        command: "cargo test".to_string(),
                        passed: true,
                    }],
                },
                "merge-someone-else",
                "c0",
                12,
            )
            .unwrap_err();
        assert!(
            matches!(err, MergeError::EvidenceMismatch { .. }),
            "实际 {err:?}"
        );
        assert!(queue.records().is_empty(), "证据不符不得写入接受记录");
        assert!(queue.is_empty(), "被拒候选应移出队列");
        assert_eq!(queue.rejected().len(), 1);
        assert!(queue.rejected()[0].1.contains("不一致"));
    }

    /// 验证失败 → 拒绝，且共享资源不被占用。
    #[test]
    fn failed_verification_rejects_and_does_not_occupy_the_schema() {
        let mut queue = MergeQueue::new("refs/heads/main");
        let claim = SharedClaim::new(SharedKind::ApiSchema, "quota.v2", "attempt-a");
        queue.enqueue(request("a", "c0", vec![claim.clone()]));
        let ticket = queue.begin_verification("c0").unwrap();
        let err = queue
            .finish_verification(
                &ticket,
                VerifyOutcome::Failed {
                    reason: "contract test failed".to_string(),
                },
                "whatever",
                "c0",
                13,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::VerificationFailed { .. }));
        assert!(queue.is_empty());
        // 后来者可以正常占用，因为失败的候选没有转成"已接受占用"。
        assert!(matches!(
            queue.enqueue(request("b", "c0", vec![claim])),
            EnqueueOutcome::Queued { .. }
        ));
    }

    /// 3.1/3.3：串行——不许插队核销别人的票据。
    #[test]
    fn queue_is_strictly_serial() {
        let mut queue = MergeQueue::new("refs/heads/main");
        queue.enqueue(request("a", "c0", vec![]));
        queue.enqueue(request("b", "c0", vec![]));
        let ticket_a = queue.begin_verification("c0").unwrap();

        // 先用 a 的票据把 a 核销掉，再核销 b。
        queue
            .finish_verification(
                &ticket_a,
                VerifyOutcome::Passed { commands: vec![] },
                &ticket_a.combination,
                "c0",
                14,
            )
            .unwrap();
        assert_eq!(queue.head().unwrap().request.request_id, "req-b");

        // 拿旧票据再核销 (a) 会失败：a 已不在队列。
        let err = queue
            .finish_verification(
                &ticket_a,
                VerifyOutcome::Passed { commands: vec![] },
                &ticket_a.combination,
                "c0",
                15,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::NotAtHead { .. }), "实际 {err:?}");
    }

    /// G-29：worktree/clone 只能算协作隔离，声明成 sandboxed 要降级并留痕。
    #[test]
    fn worktree_is_collaborative_isolation_even_if_declared_sandboxed() {
        let declaration = IsolationDeclaration {
            adapter: "local-process".to_string(),
            declared: IsolationLevel::Strong,
            provider: IsolationProvider::Worktree,
        };
        let verdict = resolve_isolation(&declaration);
        assert_eq!(verdict.effective, IsolationLevel::Collaborative);
        let note = verdict.note.expect("降级必须留痕");
        assert!(note.contains("G-29"), "实际 {note}");
        assert!(note.contains("协作隔离"));

        // 容器提供方才配得上 Strong。
        let container = IsolationDeclaration {
            adapter: "runner-container".to_string(),
            declared: IsolationLevel::Strong,
            provider: IsolationProvider::Container,
        };
        let verdict = resolve_isolation(&container);
        assert_eq!(verdict.effective, IsolationLevel::Strong);
        assert!(verdict.note.is_none());

        // 老老实实声明协作隔离的，不算降级，不写 note。
        let honest = IsolationDeclaration {
            adapter: "worktree-adapter".to_string(),
            declared: IsolationLevel::Collaborative,
            provider: IsolationProvider::Worktree,
        };
        assert!(resolve_isolation(&honest).note.is_none());
        assert_eq!(DEFAULT_WRITE_LIMITS.len(), 4);
    }
}
