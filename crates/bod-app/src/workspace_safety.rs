//! P1 D1 工作区与 Git 基线（任务 1.1—1.5，§27、§20.10、G-29）。
//!
//! 四件事各自独立、可单测：
//! 1. **独立工作区**：每个 Attempt 一个 git worktree + 分支，暂存区与索引天然独立；
//! 2. **写范围门禁**：派发前冻结 `write_scope`/`coordinated_scope`，提交时按 diff 逐路径判定，
//!    越界与共享配置未授权一律进 NeedsRevision，不在这里"顺手放过"；
//! 3. **共享资源协调**：migration/API/公共类型等由登记表统一分号，冲突在合并门禁上暴露；
//! 4. **生命周期与环境隔离**：终态按策略回收，脏现场先登记再处置；端口、临时目录、DB、
//!    构建产物按工作区分区。
//!
//! 边界：worktree 只是**协作隔离**，不是强隔离。强隔离（容器/网络/凭据）由 runner 能力决定，
//! 本模块不宣称；共享对象缓存只读由调用方按凭据挂载，这里只校验路径不重叠。

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// 需要协调者的仓库级共享文件/目录（相对仓库根，段边界匹配）。
///
/// 只列"改了就影响所有工作包"的入口；成员 crate 自己的 `Cargo.toml` 不在此列，
/// 但 API/公共类型的逻辑资源由 [`SharedLedger`] 登记，不靠路径猜。
pub const SHARED_CONFIG_PATHS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "package.json",
    "package-lock.json",
    "openspec/",
    "verification/",
    "crates/bod-app/migrations/",
];

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum WorkspaceError {
    Io(std::io::Error),
    Git {
        args: String,
        stderr: String,
    },
    InvalidAttemptId(String),
    WorkspaceExists(PathBuf),
    NotAManagedWorkspace(PathBuf),
    /// 脏现场未经显式确认，拒绝销毁。
    DirtyNotDisposed {
        paths: Vec<String>,
    },
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "工作区 IO 失败: {err}"),
            Self::Git { args, stderr } => write!(f, "git {args} 失败: {stderr}"),
            Self::InvalidAttemptId(value) => write!(f, "非法 attempt_id: {value}"),
            Self::WorkspaceExists(path) => write!(f, "工作区已存在: {}", path.display()),
            Self::NotAManagedWorkspace(path) => {
                write!(f, "不是受管工作区: {}", path.display())
            }
            Self::DirtyNotDisposed { paths } => {
                write!(f, "工作区有未处置改动，拒绝静默销毁: {}", paths.join(", "))
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<std::io::Error> for WorkspaceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

// ---------------------------------------------------------------------------
// 写范围与路径门禁（任务 1.2）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeViolation {
    /// 绝对路径或带盘符：diff 里不应出现。
    Absolute(String),
    /// `..` 穿越。
    Traversal(String),
    /// 触碰 Git 管理目录。
    GitMetadata(String),
    /// 不在 write_scope 内。
    OutsideScope(String),
    /// 共享配置未取得 coordinated 授权。
    SharedUngranted(String),
    /// 解析后落在工作区之外（软链接/Junction 穿越）。
    SymlinkEscape { path: String, resolved: String },
}

impl ScopeViolation {
    /// 供 NeedsRevision 展示的越界路径。
    pub fn path(&self) -> &str {
        match self {
            Self::Absolute(path)
            | Self::Traversal(path)
            | Self::GitMetadata(path)
            | Self::OutsideScope(path)
            | Self::SharedUngranted(path) => path,
            Self::SymlinkEscape { path, .. } => path,
        }
    }
}

impl std::fmt::Display for ScopeViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absolute(path) => write!(f, "绝对路径不被接受: {path}"),
            Self::Traversal(path) => write!(f, "路径含 `..` 穿越: {path}"),
            Self::GitMetadata(path) => write!(f, "禁止修改 Git 管理目录: {path}"),
            Self::OutsideScope(path) => write!(f, "超出 write_scope: {path}"),
            Self::SharedUngranted(path) => write!(f, "共享配置未被授权协调: {path}"),
            Self::SymlinkEscape { path, resolved } => {
                write!(f, "路径经软链接逃出工作区: {path} -> {resolved}")
            }
        }
    }
}

/// 派发前冻结的写范围。
///
/// `allow` 是普通代码路径前缀（段边界匹配，`a/b` 不放行 `a/bc`）；
/// `coordinated` 是本工作包已获协调授权的共享路径。共享路径即使写在 `allow` 里也不算数，
/// 必须在 `coordinated` 里显式声明。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteScope {
    allow: Vec<String>,
    coordinated: Vec<String>,
}

impl WriteScope {
    pub fn new(allow: Vec<String>, coordinated: Vec<String>) -> Self {
        Self { allow, coordinated }
    }

    pub fn allow(&self) -> &[String] {
        &self.allow
    }

    pub fn coordinated(&self) -> &[String] {
        &self.coordinated
    }

    /// 判定单个 diff 路径；返回规范化后的相对路径。
    pub fn check(&self, raw: &str) -> Result<String, ScopeViolation> {
        let path = normalize_rel(raw)?;
        if path.split('/').any(|segment| segment == ".git") {
            return Err(ScopeViolation::GitMetadata(path));
        }
        if is_shared_config(&path) {
            if self
                .coordinated
                .iter()
                .any(|grant| is_within(&path, grant.trim_end_matches('/')))
            {
                Ok(path)
            } else {
                Err(ScopeViolation::SharedUngranted(path))
            }
        } else if self
            .allow
            .iter()
            .any(|prefix| is_within(&path, prefix.trim_end_matches('/')))
        {
            Ok(path)
        } else {
            Err(ScopeViolation::OutsideScope(path))
        }
    }
}

/// 提交候选时的门禁结论。越界不是警告，是 NeedsRevision。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffVerdict {
    Accepted { paths: Vec<String> },
    NeedsRevision { violations: Vec<ScopeViolation> },
}

impl DiffVerdict {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    pub fn violations(&self) -> &[ScopeViolation] {
        match self {
            Self::Accepted { .. } => &[],
            Self::NeedsRevision { violations } => violations,
        }
    }

    /// NeedsRevision 时要展示给执行端的路径清单。
    pub fn rejected_paths(&self) -> Vec<String> {
        self.violations()
            .iter()
            .map(|violation| violation.path().to_string())
            .collect()
    }
}

/// 路径解析器：把相对路径的"最深存在祖先"解析成真实路径，用于识别软链接穿越。
///
/// 单独抽出来是为了能在不创建真实软链接的平台上测逻辑（Windows 默认权限建不了符号链接），
/// 真实软链接另由 `#[cfg(unix)]` 用例覆盖。
pub trait PathInspector {
    fn canonical_ancestor(&self, root: &Path, rel: &str) -> Option<PathBuf>;
}

pub struct FsInspector;

impl PathInspector for FsInspector {
    fn canonical_ancestor(&self, root: &Path, rel: &str) -> Option<PathBuf> {
        let mut target = root.to_path_buf();
        for segment in rel.split('/').filter(|segment| !segment.is_empty()) {
            target.push(segment);
        }
        let mut probe: &Path = &target;
        loop {
            if let Ok(resolved) = fs::canonicalize(probe) {
                return Some(resolved);
            }
            probe = probe.parent()?;
        }
    }
}

/// 默认门禁：只用真实文件系统判定。
pub fn gate_diff(root: &Path, scope: &WriteScope, changed: &[String]) -> DiffVerdict {
    gate_diff_with(root, scope, changed, &FsInspector)
}

pub fn gate_diff_with(
    root: &Path,
    scope: &WriteScope,
    changed: &[String],
    inspector: &dyn PathInspector,
) -> DiffVerdict {
    let mut accepted = Vec::new();
    let mut violations = Vec::new();
    for raw in changed {
        match scope.check(raw) {
            Ok(path) => match escapes_root(root, &path, inspector) {
                Some(resolved) => violations.push(ScopeViolation::SymlinkEscape {
                    path,
                    resolved: resolved.display().to_string(),
                }),
                None => accepted.push(path),
            },
            Err(violation) => violations.push(violation),
        }
    }
    if violations.is_empty() {
        DiffVerdict::Accepted { paths: accepted }
    } else {
        DiffVerdict::NeedsRevision { violations }
    }
}

/// 解析后是否逃出工作区根。根不存在时不做判断（例如纯逻辑测试）。
fn escapes_root(root: &Path, rel: &str, inspector: &dyn PathInspector) -> Option<PathBuf> {
    let canonical_root = fs::canonicalize(root).ok()?;
    let resolved = inspector.canonical_ancestor(root, rel)?;
    if resolved.starts_with(&canonical_root) {
        None
    } else {
        Some(resolved)
    }
}

fn normalize_rel(raw: &str) -> Result<String, ScopeViolation> {
    let unified = raw.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(ScopeViolation::Absolute(raw.to_string()));
    }
    let bytes = unified.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Err(ScopeViolation::Absolute(raw.to_string()));
    }
    let mut segments = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(ScopeViolation::Traversal(raw.to_string())),
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return Err(ScopeViolation::OutsideScope(raw.to_string()));
    }
    Ok(segments.join("/"))
}

/// 段边界匹配：`a/b` 放行 `a/b` 与 `a/b/c`，不放行 `a/bc`。
fn is_within(candidate: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    candidate == prefix
        || (candidate.len() > prefix.len()
            && candidate.starts_with(prefix)
            && candidate.as_bytes()[prefix.len()] == b'/')
}

fn is_shared_config(path: &str) -> bool {
    SHARED_CONFIG_PATHS
        .iter()
        .any(|prefix| is_within(path, prefix.trim_end_matches('/')))
}

// ---------------------------------------------------------------------------
// 共享资源协调（任务 1.3）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SharedKind {
    Migration,
    ApiSchema,
    PublicType,
    EventSchema,
    RootDependency,
    OpenSpec,
}

impl SharedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Migration => "migration",
            Self::ApiSchema => "api-schema",
            Self::PublicType => "public-type",
            Self::EventSchema => "event-schema",
            Self::RootDependency => "root-dependency",
            Self::OpenSpec => "openspec",
        }
    }
}

impl std::fmt::Display for SharedKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedClaim {
    pub kind: SharedKind,
    pub value: String,
    pub attempt: String,
    pub note: String,
}

impl SharedClaim {
    pub fn new(kind: SharedKind, value: impl Into<String>, attempt: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
            attempt: attempt.into(),
            note: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedConflict {
    pub kind: SharedKind,
    pub value: String,
    /// 冲突涉及的工作包（去重、排序）。
    pub holders: Vec<String>,
    pub message: String,
}

/// 共享资源登记表：按 (kind, value) 唯一，冲突显式返回，不静默覆盖。
#[derive(Debug, Default)]
pub struct SharedLedger {
    claims: Vec<SharedClaim>,
}

impl SharedLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn claims(&self) -> &[SharedClaim] {
        &self.claims
    }

    /// 登记一条；同一工作包重复登记视为幂等，跨工作包重复即冲突。
    pub fn claim(&mut self, claim: SharedClaim) -> Result<(), SharedConflict> {
        if let Some(existing) = self
            .claims
            .iter()
            .find(|item| item.kind == claim.kind && item.value == claim.value)
        {
            if existing.attempt == claim.attempt {
                return Ok(());
            }
            return Err(SharedConflict {
                kind: claim.kind,
                value: claim.value.clone(),
                holders: vec![existing.attempt.clone(), claim.attempt.clone()],
                message: format!(
                    "{} `{}` 已由 {} 占用，{} 不得复用同一编号；请重新分配后再合并",
                    claim.kind, claim.value, existing.attempt, claim.attempt
                ),
            });
        }
        self.claims.push(claim);
        Ok(())
    }

    /// 稳定分配下一个 migration 编号（现有最大数字 +1，三位补零）。
    pub fn next_migration_id(&self) -> String {
        let max = self
            .claims
            .iter()
            .filter(|claim| claim.kind == SharedKind::Migration)
            .filter_map(|claim| claim.value.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("{:03}", max + 1)
    }

    /// 分配并登记下一个 migration 编号；同一工作包重复调用返回已分配的那个。
    ///
    /// 幂等必须是"返回同一个号"而不是"再领一个新号"——否则重试会把号段白白占掉，
    /// 后续工作包拿到跳号，合并时又对不上顺序。
    pub fn claim_migration(&mut self, attempt: &str) -> Result<String, SharedConflict> {
        if let Some(existing) = self
            .claims
            .iter()
            .find(|claim| claim.kind == SharedKind::Migration && claim.attempt == attempt)
        {
            return Ok(existing.value.clone());
        }
        let id = self.next_migration_id();
        self.claim(SharedClaim::new(SharedKind::Migration, id.clone(), attempt))?;
        Ok(id)
    }

    /// 合并门禁：把各工作包提交的声明合起来检查，与已登记条目一并比对。
    ///
    /// 返回的冲突同时包含"两个工作包提了同一编号"与"与既有登记重复"两种情况；
    /// 通过时返回空列表（不返回 Ok/Err，因为要一次列出全部冲突）。
    pub fn merge_gate(&self, incoming: &[SharedClaim]) -> Vec<SharedConflict> {
        let mut holders: BTreeMap<(SharedKind, String), BTreeSet<String>> = BTreeMap::new();
        for claim in self.claims.iter().chain(incoming.iter()) {
            holders
                .entry((claim.kind, claim.value.clone()))
                .or_default()
                .insert(claim.attempt.clone());
        }
        holders
            .into_iter()
            .filter(|(_, attempts)| attempts.len() > 1)
            .map(|((kind, value), attempts)| {
                let holders: Vec<String> = attempts.into_iter().collect();
                SharedConflict {
                    kind,
                    value: value.clone(),
                    holders: holders.clone(),
                    message: format!(
                        "{kind} `{value}` 被多个工作包同时占用（{}）；必须统一分配编号并重新检查顺序与回退兼容",
                        holders.join(", ")
                    ),
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Git 工作区（任务 1.1）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorkspace {
    pub attempt_id: String,
    pub branch: String,
    pub path: PathBuf,
}

/// 一个仓库根 + 一个工作区根目录。
#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    repo: PathBuf,
    root: PathBuf,
}

impl WorkspaceManager {
    pub fn new(repo: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            root: root.into(),
        }
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 建 worktree：分支已存在则直接检出，否则从 `base_ref` 新建。
    pub fn create(
        &self,
        attempt_id: &str,
        branch: &str,
        base_ref: &str,
    ) -> Result<GitWorkspace, WorkspaceError> {
        let slug = sanitize_attempt_id(attempt_id)?;
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(&slug);
        if path.exists() {
            return Err(WorkspaceError::WorkspaceExists(path));
        }
        let path_arg = path.to_string_lossy().to_string();
        if self.branch_exists(branch)? {
            run_git(&self.repo, &["worktree", "add", &path_arg, branch])?;
        } else {
            run_git(
                &self.repo,
                &["worktree", "add", "-b", branch, &path_arg, base_ref],
            )?;
        }
        Ok(GitWorkspace {
            attempt_id: attempt_id.to_string(),
            branch: branch.to_string(),
            path,
        })
    }

    /// 工作区相对仓库根的改动路径（未提交与已暂存都算），按字典序去重。
    ///
    /// 这是"两个 Attempt 互不可见"的可核对口径：索引按 worktree 独立，所以同一份改动
    /// 只会出现在它自己的清单里。
    pub fn changed_paths(&self, workspace: &GitWorkspace) -> Result<Vec<String>, WorkspaceError> {
        let raw = run_git_bytes(&workspace.path, &["status", "--porcelain=v1", "-z"])?;
        let mut paths = Vec::new();
        let mut fields = raw.split(|byte| *byte == 0).filter(|part| !part.is_empty());
        while let Some(entry) = fields.next() {
            if entry.len() < 4 {
                continue;
            }
            let status = &entry[..2];
            paths.push(String::from_utf8_lossy(&entry[3..]).to_string());
            // 重命名/复制条目后紧跟源路径，同样属于本次改动。
            if matches!(status[0], b'R' | b'C')
                && let Some(source) = fields.next()
            {
                paths.push(String::from_utf8_lossy(source).to_string());
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    pub fn is_clean(&self, workspace: &GitWorkspace) -> Result<bool, WorkspaceError> {
        Ok(self.changed_paths(workspace)?.is_empty())
    }

    /// 只移除 worktree 登记与目录，不删除用户仓库，也不删分支（候选证据要留）。
    pub fn remove(&self, workspace: &GitWorkspace) -> Result<(), WorkspaceError> {
        if !workspace.path.exists() {
            return Err(WorkspaceError::NotAManagedWorkspace(workspace.path.clone()));
        }
        let path_arg = workspace.path.to_string_lossy().to_string();
        run_git(&self.repo, &["worktree", "remove", "--force", &path_arg])?;
        // prune 失败不影响结论：worktree 已经不在列表里。
        let _ = run_git(&self.repo, &["worktree", "prune"]);
        Ok(())
    }

    /// 按策略回收：脏现场一律先登记再处置，绝不静默销毁。
    pub fn reclaim(
        &self,
        workspace: &GitWorkspace,
        policy: &ReclaimPolicy,
        scenes: &mut SceneRegistry,
    ) -> Result<ReclaimOutcome, WorkspaceError> {
        let dirty = self.changed_paths(workspace)?;
        if !dirty.is_empty() {
            match policy {
                ReclaimPolicy::KeepDirty => {
                    let scene = scenes.retain(workspace, dirty, RetainReason::DirtyFiles);
                    return Ok(ReclaimOutcome::Retained(scene));
                }
                ReclaimPolicy::DiscardDirty { confirmation } if confirmation.trim().is_empty() => {
                    return Err(WorkspaceError::DirtyNotDisposed { paths: dirty });
                }
                ReclaimPolicy::DiscardDirty { .. } => {}
            }
        }
        self.remove(workspace)?;
        Ok(ReclaimOutcome::Removed)
    }

    fn branch_exists(&self, branch: &str) -> Result<bool, WorkspaceError> {
        let reference = format!("refs/heads/{branch}");
        match run_git(
            &self.repo,
            &["rev-parse", "--verify", "--quiet", &reference],
        ) {
            Ok(_) => Ok(true),
            Err(WorkspaceError::Git { .. }) => Ok(false),
            Err(other) => Err(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimPolicy {
    /// 有改动就保留现场并登记待核实项。
    KeepDirty,
    /// 只有拿到显式确认（非空白字符串）才允许丢弃改动。
    DiscardDirty { confirmation: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimOutcome {
    Removed,
    Retained(RetainedScene),
}

// ---------------------------------------------------------------------------
// 生命周期与待核实现场（任务 1.4）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainReason {
    DirtyFiles,
    UnconfirmedEffect,
    ValuableCandidate,
}

impl RetainReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirtyFiles => "dirty-files",
            Self::UnconfirmedEffect => "unconfirmed-effect",
            Self::ValuableCandidate => "valuable-candidate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedScene {
    pub attempt_id: String,
    pub workspace_path: PathBuf,
    pub dirty_paths: Vec<String>,
    pub reason: RetainReason,
    pub recorded_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposal {
    Discarded,
    Accepted,
    Abandoned,
}

impl Disposal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discarded => "discarded",
            Self::Accepted => "accepted",
            Self::Abandoned => "abandoned",
        }
    }
}

/// 待核实现场登记表：保留的现场必须有明确处置才消失。
#[derive(Debug, Default)]
pub struct SceneRegistry {
    scenes: Vec<RetainedScene>,
}

impl SceneRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn retain(
        &mut self,
        workspace: &GitWorkspace,
        dirty_paths: Vec<String>,
        reason: RetainReason,
    ) -> RetainedScene {
        let scene = RetainedScene {
            attempt_id: workspace.attempt_id.clone(),
            workspace_path: workspace.path.clone(),
            dirty_paths,
            reason,
            recorded_ms: now_ms(),
        };
        self.scenes.push(scene.clone());
        scene
    }

    /// 待核实项（只读视图，供工作台/CLI 显示）。
    pub fn pending_review(&self) -> &[RetainedScene] {
        &self.scenes
    }

    pub fn is_pending(&self, attempt_id: &str) -> bool {
        self.scenes
            .iter()
            .any(|scene| scene.attempt_id == attempt_id)
    }

    /// 记录处置结果并移出待核实列表；处置本身由调用方执行。
    pub fn resolve(&mut self, attempt_id: &str, _disposal: Disposal) -> Option<RetainedScene> {
        let index = self
            .scenes
            .iter()
            .position(|scene| scene.attempt_id == attempt_id)?;
        Some(self.scenes.remove(index))
    }
}

// ---------------------------------------------------------------------------
// 环境隔离（任务 1.5）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    pub fn new(start: u16, end: u16) -> Result<Self, IsolationError> {
        if start == 0 || end < start {
            return Err(IsolationError::InvalidRange { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn start(self) -> u16 {
        self.start
    }

    pub fn end(self) -> u16 {
        self.end
    }

    fn capacity(self) -> usize {
        usize::from(self.end - self.start) + 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationError {
    InvalidRange { start: u16, end: u16 },
    Exhausted { start: u16, end: u16 },
    AlreadyLeased(u16),
    EmptyAttemptId,
}

impl std::fmt::Display for IsolationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRange { start, end } => write!(f, "端口区间非法: {start}..{end}"),
            Self::Exhausted { start, end } => write!(f, "端口区间已用尽: {start}..{end}"),
            Self::AlreadyLeased(port) => write!(f, "端口 {port} 已被占用"),
            Self::EmptyAttemptId => write!(f, "attempt_id 不能为空"),
        }
    }
}

impl std::error::Error for IsolationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortLease {
    pub attempt_id: String,
    pub port: u16,
}

/// 区间内顺序分配，释放后可复用；同一 attempt 重复申请返回同一端口，避免自相冲突。
#[derive(Debug)]
pub struct PortAllocator {
    range: PortRange,
    leases: BTreeMap<u16, String>,
}

impl PortAllocator {
    pub fn new(range: PortRange) -> Self {
        Self {
            range,
            leases: BTreeMap::new(),
        }
    }

    pub fn lease(&mut self, attempt_id: &str) -> Result<PortLease, IsolationError> {
        if attempt_id.trim().is_empty() {
            return Err(IsolationError::EmptyAttemptId);
        }
        if let Some((port, _)) = self
            .leases
            .iter()
            .find(|(_, holder)| holder.as_str() == attempt_id)
        {
            return Ok(PortLease {
                attempt_id: attempt_id.to_string(),
                port: *port,
            });
        }
        for port in self.range.start..=self.range.end {
            if let Entry::Vacant(slot) = self.leases.entry(port) {
                slot.insert(attempt_id.to_string());
                return Ok(PortLease {
                    attempt_id: attempt_id.to_string(),
                    port,
                });
            }
        }
        Err(IsolationError::Exhausted {
            start: self.range.start,
            end: self.range.end,
        })
    }

    pub fn release(&mut self, port: u16) -> bool {
        self.leases.remove(&port).is_some()
    }

    pub fn leased(&self) -> usize {
        self.leases.len()
    }

    pub fn remaining(&self) -> usize {
        self.range.capacity().saturating_sub(self.leases.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEnv {
    pub attempt_id: String,
    pub port: u16,
    pub temp_dir: PathBuf,
    pub db_path: PathBuf,
    pub build_dir: PathBuf,
    /// 只读共享缓存（同一份挂载给所有工作区）。
    pub shared_cache: PathBuf,
    pub shared_cache_read_only: bool,
}

impl WorkspaceEnv {
    /// 两套环境是否真正隔离；返回空列表才算通过。
    pub fn isolation_violations(&self, other: &Self) -> Vec<IsolationViolation> {
        let mut violations = Vec::new();
        if self.port == other.port {
            violations.push(IsolationViolation::SamePort(self.port));
        }
        if self.temp_dir == other.temp_dir {
            violations.push(IsolationViolation::SameTempDir(
                self.temp_dir.display().to_string(),
            ));
        }
        if self.db_path == other.db_path {
            violations.push(IsolationViolation::SameDb(
                self.db_path.display().to_string(),
            ));
        }
        if self.build_dir == other.build_dir {
            violations.push(IsolationViolation::SameBuildDir(
                self.build_dir.display().to_string(),
            ));
        }
        if !self.shared_cache_read_only {
            violations.push(IsolationViolation::SharedCacheWritable(
                self.shared_cache.display().to_string(),
            ));
        }
        violations
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationViolation {
    SamePort(u16),
    SameTempDir(String),
    SameDb(String),
    SameBuildDir(String),
    SharedCacheWritable(String),
}

impl std::fmt::Display for IsolationViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SamePort(port) => write!(f, "两个工作区共用端口 {port}"),
            Self::SameTempDir(path) => write!(f, "两个工作区共用临时目录 {path}"),
            Self::SameDb(path) => write!(f, "两个工作区共用数据库 {path}"),
            Self::SameBuildDir(path) => write!(f, "两个工作区共用构建输出 {path}"),
            Self::SharedCacheWritable(path) => write!(f, "共享缓存不是只读挂载: {path}"),
        }
    }
}

/// 按工作区派生一套隔离环境：端口由调用方从 [`PortAllocator`] 取，其余按 attempt 分区。
pub fn plan_env(base_dir: &Path, workspace: &GitWorkspace, port: u16) -> WorkspaceEnv {
    let slug = sanitize_attempt_id(&workspace.attempt_id).unwrap_or_else(|_| "attempt".to_string());
    WorkspaceEnv {
        attempt_id: workspace.attempt_id.clone(),
        port,
        temp_dir: base_dir.join("tmp").join(&slug),
        db_path: base_dir.join("db").join(format!("{slug}.db")),
        build_dir: base_dir.join("build").join(&slug),
        shared_cache: base_dir.join("shared-cache"),
        shared_cache_read_only: true,
    }
}

// ---------------------------------------------------------------------------
// 内部工具
// ---------------------------------------------------------------------------

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// attempt_id 会直接成为目录名：只留安全字符，避免路径穿越与非法文件名。
fn sanitize_attempt_id(attempt_id: &str) -> Result<String, WorkspaceError> {
    let trimmed = attempt_id.trim();
    if trimmed.is_empty() {
        return Err(WorkspaceError::InvalidAttemptId(attempt_id.to_string()));
    }
    let mut slug = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    if slug.chars().all(|ch| ch == '.' || ch == '-') {
        return Err(WorkspaceError::InvalidAttemptId(attempt_id.to_string()));
    }
    Ok(slug)
}

fn run_git(repo: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    let output = run_git_bytes(repo, args)?;
    Ok(String::from_utf8_lossy(&output).to_string())
}

fn run_git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(WorkspaceError::Io)?;
    if !output.status.success() {
        return Err(WorkspaceError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// 临时根目录：测试结束尽力清理，失败也不影响断言。
    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let seq = SEQ.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("bod-ws-{tag}-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        run_git(repo, args).unwrap_or_else(|err| panic!("git {args:?} 失败: {err}"))
    }

    /// 造一个有首个提交的仓库，返回 HEAD。
    fn init_repo(repo: &Path) -> String {
        fs::create_dir_all(repo).expect("建仓库目录");
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "dev@bod.local"]);
        git(repo, &["config", "user.name", "bod-test"]);
        fs::write(repo.join("README.md"), "base\n").expect("写基线文件");
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "base"]);
        git(repo, &["rev-parse", "HEAD"]).trim().to_string()
    }

    fn scope() -> WriteScope {
        WriteScope::new(
            vec![
                "crates/bod-app/src/workspace_safety.rs".to_string(),
                "crates/bod-app/src/work/".to_string(),
            ],
            vec![],
        )
    }

    // 任务 1.1 / G-29：两个并行 Attempt 的索引与未提交内容互不可见。
    #[test]
    fn g_29_parallel_attempts_use_separate_indexes() {
        let tmp = TempRoot::new("g29");
        let repo = tmp.path.join("repo");
        let head = init_repo(&repo);
        let manager = WorkspaceManager::new(&repo, tmp.path.join("worktrees"));

        let a = manager
            .create("attempt-a", "bod/attempt-a", &head)
            .expect("建工作区 A");
        let b = manager
            .create("attempt-b", "bod/attempt-b", &head)
            .expect("建工作区 B");

        assert_ne!(a.path, b.path, "两个 Attempt 不能共用目录");
        assert_ne!(a.branch, b.branch, "两个 Attempt 不能共用分支");

        fs::write(a.path.join("only-a.txt"), "a\n").expect("写 A 的改动");
        git(&a.path, &["add", "only-a.txt"]);

        assert_eq!(
            manager.changed_paths(&a).expect("读 A 的改动"),
            vec!["only-a.txt".to_string()]
        );
        assert!(
            manager.changed_paths(&b).expect("读 B 的改动").is_empty(),
            "B 不能看到 A 的未提交内容"
        );
        assert!(
            !b.path.join("only-a.txt").exists(),
            "B 的目录里不应有 A 的文件"
        );

        let staged_in_b = git(&b.path, &["diff", "--cached", "--name-only"]);
        assert!(staged_in_b.trim().is_empty(), "B 的暂存区不能包含 A 的暂存");
    }

    // 任务 1.2：越界、共享配置、Git 目录、穿越与绝对路径都进 NeedsRevision。
    #[test]
    fn diff_gate_rejects_out_of_scope_and_shared_paths() {
        let scope = scope();
        let root = Path::new("definitely-missing-root");
        let changed = vec![
            "crates/bod-app/src/workspace_safety.rs".to_string(),
            "crates/bod-app/src/workspace_safety_extra.rs".to_string(),
            "Cargo.toml".to_string(),
            ".git/config".to_string(),
            "../outside.txt".to_string(),
            "/etc/passwd".to_string(),
            "crates\\bod-app\\src\\work\\item.rs".to_string(),
        ];
        let verdict = gate_diff(root, &scope, &changed);
        assert!(!verdict.is_accepted());
        let rejected = verdict.rejected_paths();
        assert_eq!(
            rejected,
            vec![
                "crates/bod-app/src/workspace_safety_extra.rs".to_string(),
                "Cargo.toml".to_string(),
                ".git/config".to_string(),
                "../outside.txt".to_string(),
                "/etc/passwd".to_string(),
            ]
        );
        assert!(verdict.violations().iter().any(|violation| matches!(
            violation,
            ScopeViolation::SharedUngranted(path) if path == "Cargo.toml"
        )));
    }

    #[test]
    fn diff_gate_accepts_in_scope_and_granted_shared_paths() {
        let scope = WriteScope::new(
            vec!["crates/bod-app/src/work/".to_string()],
            vec!["crates/bod-app/migrations/".to_string()],
        );
        let changed = vec![
            "crates/bod-app/src/work/item.rs".to_string(),
            "crates/bod-app/migrations/007_workspace.sql".to_string(),
        ];
        let verdict = gate_diff(Path::new("definitely-missing-root"), &scope, &changed);
        match verdict {
            DiffVerdict::Accepted { paths } => {
                assert_eq!(paths.len(), 2);
                assert!(paths.contains(&"crates/bod-app/migrations/007_workspace.sql".to_string()));
            }
            other => panic!("应接受授权内的路径，实际 {other:?}"),
        }
    }

    /// 不建真实符号链接也能验证判定逻辑：解析器声称某个路径落在根之外。
    struct EscapingInspector {
        outside: PathBuf,
    }

    impl PathInspector for EscapingInspector {
        fn canonical_ancestor(&self, root: &Path, rel: &str) -> Option<PathBuf> {
            if rel.starts_with("link/") {
                Some(self.outside.clone())
            } else {
                Some(root.join(rel))
            }
        }
    }

    #[test]
    fn diff_gate_detects_symlink_escape() {
        let tmp = TempRoot::new("escape");
        let root = tmp.path.join("ws");
        fs::create_dir_all(&root).expect("建工作区根");
        let scope = WriteScope::new(vec!["link/".to_string()], vec![]);
        let inspector = EscapingInspector {
            outside: tmp.path.clone(),
        };
        let verdict = gate_diff_with(&root, &scope, &["link/passwd".to_string()], &inspector);
        match verdict {
            DiffVerdict::NeedsRevision { violations } => {
                assert!(matches!(
                    violations.as_slice(),
                    [ScopeViolation::SymlinkEscape { path, .. }] if path == "link/passwd"
                ));
            }
            other => panic!("软链接穿越必须被拒绝，实际 {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn diff_gate_detects_real_symlink_escape() {
        let tmp = TempRoot::new("real-link");
        let root = tmp.path.join("ws");
        fs::create_dir_all(&root).expect("建工作区根");
        std::os::unix::fs::symlink(tmp.path.clone(), root.join("link")).expect("建软链接");
        let scope = WriteScope::new(vec!["link/".to_string()], vec![]);
        let verdict = gate_diff(&root, &scope, &["link/passwd".to_string()]);
        assert!(!verdict.is_accepted(), "真实软链接穿越必须被拒绝");
    }

    // 任务 1.3：同一 migration 编号由两个工作包抢占时必须阻断。
    #[test]
    fn shared_ledger_blocks_duplicate_migration_ids() {
        let mut ledger = SharedLedger::new();
        assert_eq!(ledger.next_migration_id(), "001");

        let first = ledger.claim_migration("attempt-a").expect("分配 001");
        assert_eq!(first, "001");
        // 同一 attempt 重复登记是幂等的。
        ledger.claim_migration("attempt-a").expect("重复登记应幂等");

        let conflict = ledger
            .claim(SharedClaim::new(
                SharedKind::Migration,
                first.clone(),
                "attempt-b",
            ))
            .expect_err("跨工作包抢占同一编号必须冲突");
        assert_eq!(conflict.holders, vec!["attempt-a", "attempt-b"]);
        assert!(conflict.message.contains("001"));

        let second = ledger.claim_migration("attempt-b").expect("分配 002");
        assert_eq!(second, "002");

        let proposals = vec![
            SharedClaim::new(SharedKind::Migration, "007", "attempt-a"),
            SharedClaim::new(SharedKind::Migration, "007", "attempt-b"),
        ];
        let conflicts = ledger.merge_gate(&proposals);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].value, "007");
        assert_eq!(conflicts[0].holders, vec!["attempt-a", "attempt-b"]);
        assert!(ledger.merge_gate(&[]).is_empty());
    }

    // 任务 1.4：取消后有脏文件 → 保留现场、登记待核实、用户仓库不被动。
    #[test]
    fn cancel_with_dirty_files_retains_scene_and_keeps_repo() {
        let tmp = TempRoot::new("lifecycle");
        let repo = tmp.path.join("repo");
        let head = init_repo(&repo);
        let manager = WorkspaceManager::new(&repo, tmp.path.join("worktrees"));
        let workspace = manager
            .create("attempt-cancel", "bod/attempt-cancel", &head)
            .expect("建工作区");
        fs::write(workspace.path.join("dirty.txt"), "wip\n").expect("写脏文件");

        let mut scenes = SceneRegistry::new();
        let outcome = manager
            .reclaim(&workspace, &ReclaimPolicy::KeepDirty, &mut scenes)
            .expect("按保留策略回收");
        match outcome {
            ReclaimOutcome::Retained(scene) => {
                assert_eq!(scene.attempt_id, "attempt-cancel");
                assert_eq!(scene.dirty_paths, vec!["dirty.txt".to_string()]);
                assert_eq!(scene.reason, RetainReason::DirtyFiles);
            }
            other => panic!("有脏文件应保留现场，实际 {other:?}"),
        }
        assert!(workspace.path.exists(), "保留现场意味着工作区还在");
        assert!(repo.join(".git").exists(), "用户仓库不能被删除");
        assert_eq!(scenes.pending_review().len(), 1);
        assert!(scenes.is_pending("attempt-cancel"));

        // 没有显式确认时，拒绝销毁改动。
        let error = manager
            .reclaim(
                &workspace,
                &ReclaimPolicy::DiscardDirty {
                    confirmation: "  ".to_string(),
                },
                &mut scenes,
            )
            .expect_err("空确认必须被拒绝");
        assert!(matches!(error, WorkspaceError::DirtyNotDisposed { .. }));

        // 拿到确认后只移除 worktree，仓库与分支都保留。
        let outcome = manager
            .reclaim(
                &workspace,
                &ReclaimPolicy::DiscardDirty {
                    confirmation: "user:confirm".to_string(),
                },
                &mut scenes,
            )
            .expect("确认后回收");
        assert_eq!(outcome, ReclaimOutcome::Removed);
        assert!(!workspace.path.exists());
        assert!(repo.join(".git").exists(), "回收不能删除用户仓库");
        let branches = git(&repo, &["branch", "--list", "bod/attempt-cancel"]);
        assert!(
            branches.contains("bod/attempt-cancel"),
            "分支作为候选证据保留"
        );
        assert!(
            scenes
                .resolve("attempt-cancel", Disposal::Abandoned)
                .is_some()
        );
        assert!(!scenes.is_pending("attempt-cancel"));
    }

    #[test]
    fn clean_workspace_is_removed_without_scene() {
        let tmp = TempRoot::new("clean");
        let repo = tmp.path.join("repo");
        let head = init_repo(&repo);
        let manager = WorkspaceManager::new(&repo, tmp.path.join("worktrees"));
        let workspace = manager
            .create("attempt-clean", "bod/attempt-clean", &head)
            .expect("建工作区");
        let mut scenes = SceneRegistry::new();
        let outcome = manager
            .reclaim(&workspace, &ReclaimPolicy::KeepDirty, &mut scenes)
            .expect("回收干净工作区");
        assert_eq!(outcome, ReclaimOutcome::Removed);
        assert!(scenes.pending_review().is_empty());
        assert!(repo.join(".git").exists());
    }

    // 任务 1.5：并发工作区的端口、临时目录、DB、构建输出都不重叠。
    #[test]
    fn parallel_workspaces_get_isolated_ports_and_dirs() {
        let range = PortRange::new(41000, 41002).expect("合法区间");
        let mut allocator = PortAllocator::new(range);
        let lease_a = allocator.lease("attempt-a").expect("分配端口 A");
        let lease_b = allocator.lease("attempt-b").expect("分配端口 B");
        assert_ne!(lease_a.port, lease_b.port);
        // 同一 attempt 重复申请幂等。
        assert_eq!(
            allocator.lease("attempt-a").expect("重复申请").port,
            lease_a.port
        );
        assert_eq!(allocator.leased(), 2);
        assert_eq!(allocator.remaining(), 1);

        let base = Path::new("ws-root");
        let ws_a = GitWorkspace {
            attempt_id: "attempt-a".to_string(),
            branch: "bod/attempt-a".to_string(),
            path: base.join("a"),
        };
        let ws_b = GitWorkspace {
            attempt_id: "attempt-b".to_string(),
            branch: "bod/attempt-b".to_string(),
            path: base.join("b"),
        };
        let env_a = plan_env(base, &ws_a, lease_a.port);
        let env_b = plan_env(base, &ws_b, lease_b.port);
        assert!(env_a.isolation_violations(&env_b).is_empty());
        assert_ne!(env_a.temp_dir, env_b.temp_dir);
        assert_ne!(env_a.db_path, env_b.db_path);
        assert_ne!(env_a.build_dir, env_b.build_dir);
        assert_eq!(env_a.shared_cache, env_b.shared_cache);
        assert!(env_a.shared_cache_read_only);

        // 复用同一端口必须被判为冲突。
        let env_conflict = plan_env(base, &ws_b, lease_a.port);
        assert_eq!(
            env_a.isolation_violations(&env_conflict),
            vec![IsolationViolation::SamePort(lease_a.port)]
        );

        assert!(allocator.release(lease_a.port));
        assert_eq!(allocator.leased(), 1);
        let reused = allocator.lease("attempt-c").expect("复用释放的端口");
        assert_eq!(reused.port, lease_a.port);
    }

    #[test]
    fn port_range_exhaustion_is_reported() {
        let range = PortRange::new(42000, 42000).expect("合法区间");
        let mut allocator = PortAllocator::new(range);
        allocator.lease("attempt-a").expect("占满唯一端口");
        match allocator.lease("attempt-b") {
            Err(IsolationError::Exhausted { start, end }) => {
                assert_eq!((start, end), (42000, 42000));
            }
            other => panic!("区间用尽必须报错，实际 {other:?}"),
        }
        assert!(matches!(
            PortRange::new(42001, 42000),
            Err(IsolationError::InvalidRange { .. })
        ));
    }

    #[test]
    fn attempt_id_is_sanitized_for_directory_names() {
        assert_eq!(sanitize_attempt_id(" attempt/1 ").unwrap(), "attempt-1");
        assert!(matches!(
            sanitize_attempt_id("   "),
            Err(WorkspaceError::InvalidAttemptId(_))
        ));
        assert!(matches!(
            sanitize_attempt_id(".."),
            Err(WorkspaceError::InvalidAttemptId(_))
        ));
    }
}
