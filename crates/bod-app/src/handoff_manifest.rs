//! P2 任务 1.5a：现场清单边界（submodule / LFS / 锁文件 / 冲突 / 密钥过滤）。
//!
//! 权威语义：V2.9.6 §18.12——清单说明 Git submodule/LFS、工具链与锁文件、必要服务、
//! 未解决冲突与未导出的外部会话状态；**不得**导出明文密钥、凭据环境变量或未经允许的
//! 客户资料；大文件按授权引用；「摘要存在」不等于「内容仍可恢复」。
//!
//! 本模块的边界：
//! - 只产出**事实**，不判定能否自动接续（1.1 的等级准入 + 1.4 的完整度各管一段）；
//!   `blocking_conditions()` 只回答「重建设现场还缺什么、需要谁授权」，不是接续许可。
//! - 凭据只以 [`SecretRef`]（名称 + 摘要）进入清单：值在 [`ManifestBuilder`] 内被消费后
//!   立即丢弃，清单结构里**没有**存放值的字段，因此「包内无凭据值」不是靠序列化时过滤。
//! - [`ContentState`] 明确区分「已包含字节」「只有指针」「按授权引用」「已脱敏」「未导出」
//!   「清单级声明」：只有 `included` 的内容可从包里恢复，指针上的摘要不能当内容完整性证明。
//! - 不读网络、不执行 Git、不碰工作区文件系统；调用方把捕获结果交进来，这里只做归类与过滤。

use crate::canonical_json::{self, CanonicalError, CanonicalValue};
use crate::context_handoff::SecretRef;
use crate::domain::DomainId;
use crate::domain::artifact::Digest;
use std::fmt;

/// 清单 schema：与交接包 schema 分开版本化，现场清单可以单独演进。
pub const MANIFEST_SCHEMA: &str = "bod.workspace-manifest/v1";

/// 默认内联上限：超过即按授权引用（不把大文件塞进交接包）。
pub const DEFAULT_MAX_INLINE_BYTES: usize = 256 * 1024;

/// 清单条目种类。缺哪一类是重建现场的具体障碍，不能合并成「其他」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    /// 普通受版本控制的文件。
    TrackedFile,
    /// Git submodule：只记录提交号，内容不在包内。
    Submodule,
    /// Git LFS 指针：只记录 oid 与大小，对象需另行拉取。
    LfsPointer,
    /// 依赖锁文件：决定重建出的依赖图。
    LockFile,
    /// 工具链固定（版本文件 / 容器镜像摘要）。
    ToolchainPin,
    /// 任务必需的服务（数据库、消息、浏览器等）。
    RequiredService,
    /// 未解决的合并冲突：重建前必须人工决定。
    UnresolvedConflict,
    /// 运行时外部会话状态：明确不导出（§18.9）。
    ExternalSessionState,
}

impl EntryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TrackedFile => "tracked_file",
            Self::Submodule => "submodule",
            Self::LfsPointer => "lfs_pointer",
            Self::LockFile => "lock_file",
            Self::ToolchainPin => "toolchain_pin",
            Self::RequiredService => "required_service",
            Self::UnresolvedConflict => "unresolved_conflict",
            Self::ExternalSessionState => "external_session_state",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tracked_file" => Some(Self::TrackedFile),
            "submodule" => Some(Self::Submodule),
            "lfs_pointer" => Some(Self::LfsPointer),
            "lock_file" => Some(Self::LockFile),
            "toolchain_pin" => Some(Self::ToolchainPin),
            "required_service" => Some(Self::RequiredService),
            "unresolved_conflict" => Some(Self::UnresolvedConflict),
            "external_session_state" => Some(Self::ExternalSessionState),
            _ => None,
        }
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 脱敏原因：必须写清是凭据、客户资料还是策略，用户才知道能不能申请授权。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionReason {
    /// 内容里含凭据值（环境变量、token、私钥）。
    Credential,
    /// 未经允许的客户资料。
    CustomerData,
    /// 显式策略禁止导出。
    Policy,
}

impl RedactionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::CustomerData => "customer_data",
            Self::Policy => "policy",
        }
    }
}

/// 未导出原因：外部会话状态不是「忘了捕获」，而是结构上不可导出时要分开说。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotExportedReason {
    /// 运行时内部状态（推理上下文等）不可导出。
    RuntimeInternalState,
    /// 外部会话状态不在宿主可读范围。
    SessionNotExportable,
    /// 尝试捕获但失败。
    CaptureFailed,
}

impl NotExportedReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeInternalState => "runtime_internal_state",
            Self::SessionNotExportable => "session_not_exportable",
            Self::CaptureFailed => "capture_failed",
        }
    }
}

/// 一个条目的内容状态。摘要与可恢复性是两件事，故拆成显式取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentState {
    /// 字节已包含在交接材料中，摘要按原始字节计算。
    Included { digest: Digest },
    /// 只有指针（submodule 提交号 / LFS oid）：内容不在包内，摘要不能当内容证明。
    Pointer { object_id: String },
    /// 按授权引用（大文件）：需要持授权的主体另行取回。
    Referenced {
        authorization_ref: String,
        size_bytes: u64,
    },
    /// 已过滤：内容不进入交接包，只留原因。
    Redacted { reason: RedactionReason },
    /// 未导出：内容不在宿主可捕获范围。
    NotExported { reason: NotExportedReason },
    /// 清单级声明（必需服务、未解决冲突）：没有对应的包内内容实体。
    Declared { detail: String },
}

impl ContentState {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Included { .. } => "included",
            Self::Pointer { .. } => "pointer",
            Self::Referenced { .. } => "referenced",
            Self::Redacted { .. } => "redacted",
            Self::NotExported { .. } => "not_exported",
            Self::Declared { .. } => "declared",
        }
    }

    /// 内容字节是否在包里。只有 `Included` 为真。
    pub const fn carries_content(&self) -> bool {
        matches!(self, Self::Included { .. })
    }

    /// 内容摘要：只从 `Included` 报告。指针/引用/脱敏都没有可比对的内容摘要。
    pub fn content_digest(&self) -> Option<&Digest> {
        match self {
            Self::Included { digest } => Some(digest),
            _ => None,
        }
    }

    /// 能否从这份包直接恢复该条目。「摘要存在」不等于「可恢复」。
    pub const fn recoverable_from_package(&self) -> bool {
        self.carries_content()
    }

    /// 人读解释：每条不可恢复的原因都要能对上具体动作。
    pub fn explain(&self) -> String {
        match self {
            Self::Included { .. } => "内容已包含在包内".to_string(),
            Self::Pointer { .. } => "只有指针，需按提交号/oid 另行取回".to_string(),
            Self::Referenced { .. } => "按授权引用，需持授权的主体取回".to_string(),
            Self::Redacted { reason } => {
                format!("已脱敏（{}），不进入交接包", reason.as_str())
            }
            Self::NotExported { reason } => format!("未导出（{}）", reason.as_str()),
            Self::Declared { detail } => format!("清单级声明：{detail}"),
        }
    }
}

/// 清单条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub kind: EntryKind,
    pub path: String,
    pub state: ContentState,
    /// 说明性备注（冲突详情、服务版本等）。写入前已按已知凭据值脱敏。
    pub note: Option<String>,
}

impl WorkspaceEntry {
    pub fn new(kind: EntryKind, path: impl Into<String>, state: ContentState) -> Self {
        Self {
            kind,
            path: path.into(),
            state,
            note: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// 重建设现场仍缺什么。这是**事实清单**，不是「能否自动接续」的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestGap {
    pub path: String,
    pub code: &'static str,
    pub detail: String,
}

impl ManifestGap {
    pub fn describe(&self) -> String {
        format!("{}（{}：{}）", self.path, self.code, self.detail)
    }
}

/// 现场清单。只落事实与引用，不含任何凭据值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceManifest {
    pub schema: String,
    pub company_id: DomainId,
    pub project_id: DomainId,
    pub work_package_revision: DomainId,
    pub entries: Vec<WorkspaceEntry>,
    pub secret_refs: Vec<SecretRef>,
}

impl WorkspaceManifest {
    /// 未解决冲突：重建前必须人工决定。
    pub fn conflicts(&self) -> Vec<&WorkspaceEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::UnresolvedConflict)
            .collect()
    }

    /// 包里可直接恢复的条目。
    pub fn recoverable(&self) -> Vec<&WorkspaceEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.state.recoverable_from_package())
            .collect()
    }

    /// 不可从包里直接恢复的条目及其原因（指针、引用、脱敏、未导出、声明）。
    pub fn non_recoverable(&self) -> Vec<(&WorkspaceEntry, String)> {
        self.entries
            .iter()
            .filter(|entry| !entry.state.recoverable_from_package())
            .map(|entry| (entry, entry.state.explain()))
            .collect()
    }

    /// 重建障碍：冲突、未取回的 submodule/LFS、需授权的引用、脱敏材料、未导出会话。
    pub fn blocking_conditions(&self) -> Vec<ManifestGap> {
        let mut gaps = Vec::new();
        for entry in &self.entries {
            let gap = match (&entry.kind, &entry.state) {
                (EntryKind::UnresolvedConflict, ContentState::Declared { detail }) => {
                    Some(ManifestGap {
                        path: entry.path.clone(),
                        code: "unresolved_conflict",
                        detail: detail.clone(),
                    })
                }
                (EntryKind::UnresolvedConflict, _) => Some(ManifestGap {
                    path: entry.path.clone(),
                    code: "unresolved_conflict",
                    detail: entry
                        .note
                        .clone()
                        .unwrap_or_else(|| "需人工决定合并结果".to_string()),
                }),
                (EntryKind::Submodule, ContentState::Pointer { .. }) => Some(ManifestGap {
                    path: entry.path.clone(),
                    code: "submodule_not_fetched",
                    detail: "需按提交号拉取子模块".to_string(),
                }),
                (EntryKind::LfsPointer, ContentState::Pointer { .. }) => Some(ManifestGap {
                    path: entry.path.clone(),
                    code: "lfs_object_not_fetched",
                    detail: "需按 oid 拉取 LFS 对象".to_string(),
                }),
                (_, ContentState::Referenced { .. }) => Some(ManifestGap {
                    path: entry.path.clone(),
                    code: "large_file_needs_authorization",
                    detail: "需持授权主体取回大文件".to_string(),
                }),
                (_, ContentState::Redacted { reason }) => Some(ManifestGap {
                    path: entry.path.clone(),
                    code: "redacted_material",
                    detail: format!("已脱敏（{}），需另行授权或人工补齐", reason.as_str()),
                }),
                (EntryKind::ExternalSessionState, _) => Some(ManifestGap {
                    path: entry.path.clone(),
                    code: "external_session_state",
                    detail: "外部会话状态不导出，只能人工重建".to_string(),
                }),
                _ => None,
            };
            if let Some(gap) = gap {
                gaps.push(gap);
            }
        }
        gaps
    }

    /// 一句话结论：条目数、可恢复数、密钥引用数、冲突数、障碍数。
    pub fn summary_line(&self) -> String {
        let included = self
            .entries
            .iter()
            .filter(|entry| entry.state.carries_content())
            .count();
        format!(
            "清单 {} 项（包含 {}、不可直接恢复 {}），密钥引用 {}，未解决冲突 {}，重建障碍 {}",
            self.entries.len(),
            included,
            self.entries.len() - included,
            self.secret_refs.len(),
            self.conflicts().len(),
            self.blocking_conditions().len()
        )
    }

    /// 只做结构自检：schema、身份字段、路径唯一、状态与附带字段自洽。
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(ManifestError::UnsupportedSchema(self.schema.clone()));
        }
        for (field, value) in [
            ("company_id", self.company_id.as_str()),
            ("project_id", self.project_id.as_str()),
            ("work_package_revision", self.work_package_revision.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ManifestError::MissingIdentity(field));
            }
        }

        let mut seen: Vec<(EntryKind, &str)> = Vec::new();
        for entry in &self.entries {
            if entry.path.trim().is_empty() {
                return Err(ManifestError::EmptyPath);
            }
            if seen.contains(&(entry.kind, entry.path.as_str())) {
                return Err(ManifestError::DuplicateEntry {
                    kind: entry.kind.as_str(),
                    path: entry.path.clone(),
                });
            }
            seen.push((entry.kind, entry.path.as_str()));

            match &entry.state {
                ContentState::Included { digest } => {
                    if !digest.is_usable() {
                        return Err(ManifestError::UnusableDigest(entry.path.clone()));
                    }
                }
                ContentState::Pointer { object_id } => {
                    if object_id.trim().is_empty() {
                        return Err(ManifestError::EmptyObjectId(entry.path.clone()));
                    }
                }
                ContentState::Referenced {
                    authorization_ref, ..
                } => {
                    if authorization_ref.trim().is_empty() {
                        return Err(ManifestError::MissingAuthorizationRef(entry.path.clone()));
                    }
                }
                ContentState::Declared { detail } => {
                    if detail.trim().is_empty() {
                        return Err(ManifestError::MissingConflictDetail(entry.path.clone()));
                    }
                }
                ContentState::Redacted { .. } | ContentState::NotExported { .. } => {}
            }
        }

        let mut names: Vec<&str> = Vec::new();
        for secret in &self.secret_refs {
            if secret.name().trim().is_empty() {
                return Err(ManifestError::MissingIdentity("secret_refs.name"));
            }
            if names.contains(&secret.name()) {
                return Err(ManifestError::DuplicateSecretName(
                    secret.name().to_string(),
                ));
            }
            names.push(secret.name());
            if Digest::parse(secret.digest()).is_none() {
                return Err(ManifestError::UnusableDigest(format!(
                    "secret:{}",
                    secret.name()
                )));
            }
        }
        Ok(())
    }

    /// 规范 payload 文本（键序由 1.2 的编码规则决定，条目与密钥引用已排序）。
    pub fn payload_json(&self) -> String {
        use serde_json::{Map, Value, json};
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| {
                let mut object = Map::new();
                object.insert("kind".to_string(), json!(entry.kind.as_str()));
                object.insert("path".to_string(), json!(entry.path));
                object.insert("state".to_string(), json!(entry.state.as_str()));
                match &entry.state {
                    ContentState::Included { digest } => {
                        object.insert("digest".to_string(), json!(digest.as_string()));
                    }
                    ContentState::Pointer { object_id } => {
                        object.insert("object_id".to_string(), json!(object_id));
                    }
                    ContentState::Referenced {
                        authorization_ref,
                        size_bytes,
                    } => {
                        object.insert("authorization_ref".to_string(), json!(authorization_ref));
                        object.insert("size_bytes".to_string(), json!(size_bytes));
                    }
                    ContentState::Redacted { reason } => {
                        object.insert("redaction".to_string(), json!(reason.as_str()));
                    }
                    ContentState::NotExported { reason } => {
                        object.insert("not_exported_reason".to_string(), json!(reason.as_str()));
                    }
                    ContentState::Declared { detail } => {
                        object.insert("declared".to_string(), json!(detail));
                    }
                }
                if let Some(note) = &entry.note {
                    object.insert("note".to_string(), json!(note));
                }
                Value::Object(object)
            })
            .collect();
        let secrets: Vec<Value> = self
            .secret_refs
            .iter()
            .map(|secret| json!({"name": secret.name(), "digest": secret.digest()}))
            .collect();
        json!({
            "schema": self.schema,
            "company_id": self.company_id.as_str(),
            "project_id": self.project_id.as_str(),
            "work_package_revision": self.work_package_revision.as_str(),
            "entries": entries,
            "secret_refs": secrets,
        })
        .to_string()
    }

    /// 规范 payload 摘要：供交接包 `integrity` 使用（1.2 的编码规则）。
    pub fn digest(&self) -> Result<Digest, ManifestError> {
        let payload = CanonicalValue::parse(&self.payload_json())?;
        Ok(canonical_json::payload_digest(&payload))
    }

    /// 独立审计：给定候选凭据值，返回**出现泄漏的字段路径**。
    ///
    /// 结构上清单没有存放值的字段，这个审计仍要有：备注/路径等自由文本可能被调用方写入
    /// 敏感内容；验收与日志脱敏（4.5）复用同一入口。
    pub fn audit_for_secret_values(&self, candidates: &[&[u8]]) -> Vec<String> {
        let mut leaks = Vec::new();
        let rendered = self.payload_json();
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.is_empty() {
                // 空 needle 会匹配一切，不能当成「无泄漏」。
                leaks.push(format!("candidate[{index}]:empty"));
                continue;
            }
            let needle = String::from_utf8_lossy(candidate);
            if rendered.contains(needle.as_ref()) {
                leaks.push(format!("payload[{index}]"));
            }
        }
        leaks
    }
}

/// 清单构建错误。
#[derive(Debug)]
pub enum ManifestError {
    UnsupportedSchema(String),
    MissingIdentity(&'static str),
    EmptyPath,
    DuplicateEntry { kind: &'static str, path: String },
    UnusableDigest(String),
    EmptyObjectId(String),
    MissingAuthorizationRef(String),
    MissingConflictDetail(String),
    EmptyCredentialValue(String),
    DuplicateSecretName(String),
    Canonical(CanonicalError),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(schema) => write!(f, "不支持的清单 schema：{schema}"),
            Self::MissingIdentity(field) => write!(f, "缺少身份字段：{field}"),
            Self::EmptyPath => write!(f, "清单条目路径为空"),
            Self::DuplicateEntry { kind, path } => {
                write!(f, "清单条目重复：{kind} {path}")
            }
            Self::UnusableDigest(target) => write!(f, "摘要不可用：{target}"),
            Self::EmptyObjectId(path) => write!(f, "指针条目缺少 object_id：{path}"),
            Self::MissingAuthorizationRef(path) => {
                write!(f, "引用条目缺少授权引用：{path}")
            }
            Self::MissingConflictDetail(path) => {
                write!(f, "声明条目缺少说明：{path}")
            }
            Self::EmptyCredentialValue(name) => {
                write!(f, "凭据 {name} 的值为空，无法生成可核对的引用")
            }
            Self::DuplicateSecretName(name) => write!(f, "同名凭据出现多次：{name}"),
            Self::Canonical(err) => write!(f, "规范编码失败：{err}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<CanonicalError> for ManifestError {
    fn from(err: CanonicalError) -> Self {
        Self::Canonical(err)
    }
}

/// 原始捕获 → 清单的构建器。
///
/// 凭据值只在这里出现：`add_credential_env` 立刻转成 [`SecretRef`]，值随后被丢弃；
/// 文件内容在 `build` 时按已知凭据值扫描，命中即整条脱敏（不保留片段）。
#[derive(Debug, Default)]
pub struct ManifestBuilder {
    entries: Vec<WorkspaceEntry>,
    /// 待扫描的候选文件：种类 + 路径 + 字节，`build` 时才决定包含/脱敏/引用。
    pending_files: Vec<(EntryKind, String, Vec<u8>)>,
    /// 已知凭据（名称 + 值）。值用完即弃。
    credentials: Vec<(String, Vec<u8>)>,
    /// 命中即脱敏的客户资料路径前缀。
    customer_prefixes: Vec<String>,
    /// 内联上限。
    max_inline_bytes: usize,
}

impl ManifestBuilder {
    pub fn new() -> Self {
        Self {
            max_inline_bytes: DEFAULT_MAX_INLINE_BYTES,
            ..Self::default()
        }
    }

    pub fn with_inline_limit(mut self, bytes: usize) -> Self {
        self.max_inline_bytes = bytes;
        self
    }

    /// 未经允许的客户资料：路径前缀命中即脱敏。
    pub fn block_customer_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.customer_prefixes.push(prefix.into());
        self
    }

    /// 加入一个受版本控制的文件。是否内联由大小与过滤规则在 `build` 时决定。
    pub fn add_file(&mut self, path: impl Into<String>, bytes: &[u8]) -> &mut Self {
        self.pending_files
            .push((EntryKind::TrackedFile, path.into(), bytes.to_vec()));
        self
    }

    /// 加入锁文件（同样做凭据扫描：锁文件里也可能带私有源 token）。
    pub fn add_lock_file(&mut self, path: impl Into<String>, bytes: &[u8]) -> &mut Self {
        self.pending_files
            .push((EntryKind::LockFile, path.into(), bytes.to_vec()));
        self
    }

    /// Git submodule：只记录提交号。
    pub fn add_submodule(
        &mut self,
        path: impl Into<String>,
        commit: impl Into<String>,
    ) -> &mut Self {
        self.entries.push(WorkspaceEntry::new(
            EntryKind::Submodule,
            path,
            ContentState::Pointer {
                object_id: commit.into(),
            },
        ));
        self
    }

    /// Git LFS 指针：只记录 oid 与大小。
    pub fn add_lfs_pointer(
        &mut self,
        path: impl Into<String>,
        oid: impl Into<String>,
        size_bytes: u64,
    ) -> &mut Self {
        self.entries.push(
            WorkspaceEntry::new(
                EntryKind::LfsPointer,
                path,
                ContentState::Pointer {
                    object_id: oid.into(),
                },
            )
            .with_note(format!("size_bytes={size_bytes}")),
        );
        self
    }

    /// 工具链固定：固定值的摘要进入清单，安装包不内联。
    pub fn add_toolchain_pin(
        &mut self,
        name: impl Into<String>,
        pin: impl Into<String>,
    ) -> &mut Self {
        let pin = pin.into();
        self.entries.push(
            WorkspaceEntry::new(
                EntryKind::ToolchainPin,
                name,
                ContentState::Included {
                    digest: Digest::of_bytes(pin.as_bytes()),
                },
            )
            .with_note(pin),
        );
        self
    }

    /// 必需服务。
    pub fn add_required_service(
        &mut self,
        name: impl Into<String>,
        note: impl Into<String>,
    ) -> &mut Self {
        self.entries.push(WorkspaceEntry::new(
            EntryKind::RequiredService,
            name,
            ContentState::Declared {
                detail: note.into(),
            },
        ));
        self
    }

    /// 未解决冲突。
    pub fn add_conflict(
        &mut self,
        path: impl Into<String>,
        detail: impl Into<String>,
    ) -> &mut Self {
        self.entries.push(WorkspaceEntry::new(
            EntryKind::UnresolvedConflict,
            path,
            ContentState::Declared {
                detail: detail.into(),
            },
        ));
        self
    }

    /// 外部会话状态：结构上不导出。
    pub fn add_external_session(&mut self, session_id: impl Into<String>) -> &mut Self {
        self.entries.push(WorkspaceEntry::new(
            EntryKind::ExternalSessionState,
            session_id,
            ContentState::NotExported {
                reason: NotExportedReason::RuntimeInternalState,
            },
        ));
        self
    }

    /// 凭据环境变量：**值立刻丢弃**，只留名称与摘要。
    pub fn add_credential_env(&mut self, name: impl Into<String>, value: &[u8]) -> &mut Self {
        self.credentials.push((name.into(), value.to_vec()));
        self
    }

    /// 生成清单：先做凭据扫描，再按大小/客户资料规则决定内容状态。
    pub fn build(
        self,
        company_id: &str,
        project_id: &str,
        work_package_revision: &str,
    ) -> Result<WorkspaceManifest, ManifestError> {
        let ManifestBuilder {
            entries: declared,
            pending_files,
            credentials,
            customer_prefixes,
            max_inline_bytes,
        } = self;

        let mut secret_refs = Vec::new();
        for (name, value) in &credentials {
            if value.is_empty() {
                return Err(ManifestError::EmptyCredentialValue(name.clone()));
            }
            secret_refs.push(SecretRef::from_value(name.clone(), value));
        }
        secret_refs.sort_by(|left, right| left.name().cmp(right.name()));
        for pair in secret_refs.windows(2) {
            if pair[0].name() == pair[1].name() {
                return Err(ManifestError::DuplicateSecretName(
                    pair[0].name().to_string(),
                ));
            }
        }

        let mut entries = declared;
        for (kind, path, bytes) in pending_files {
            let state = classify(
                &path,
                &bytes,
                &customer_prefixes,
                &credentials,
                max_inline_bytes,
            );
            entries.push(WorkspaceEntry::new(kind, path, state));
        }

        // 备注、冲突说明等自由文本同样过一遍脱敏，避免调用方把值写进说明。
        for entry in &mut entries {
            if let Some(note) = &entry.note {
                entry.note = Some(sanitize(note, &credentials));
            }
            if let ContentState::Declared { detail } = &entry.state {
                let cleaned = sanitize(detail, &credentials);
                entry.state = ContentState::Declared { detail: cleaned };
            }
        }

        entries.sort_by(|left, right| {
            (left.kind.as_str(), left.path.as_str())
                .cmp(&(right.kind.as_str(), right.path.as_str()))
        });

        let manifest = WorkspaceManifest {
            schema: MANIFEST_SCHEMA.to_string(),
            company_id: DomainId::new(company_id),
            project_id: DomainId::new(project_id),
            work_package_revision: DomainId::new(work_package_revision),
            entries,
            secret_refs,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

/// 按「客户资料 → 凭据命中 → 大小」的固定顺序归类。顺序固定才能解释为什么被脱敏。
fn classify(
    path: &str,
    bytes: &[u8],
    customer_prefixes: &[String],
    credentials: &[(String, Vec<u8>)],
    max_inline_bytes: usize,
) -> ContentState {
    if customer_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix.as_str()))
    {
        return ContentState::Redacted {
            reason: RedactionReason::CustomerData,
        };
    }
    if credentials
        .iter()
        .any(|(_, value)| !value.is_empty() && contains_bytes(bytes, value))
    {
        return ContentState::Redacted {
            reason: RedactionReason::Credential,
        };
    }
    if bytes.len() > max_inline_bytes {
        return ContentState::Referenced {
            authorization_ref: format!("authorized-large-file:{path}"),
            size_bytes: bytes.len() as u64,
        };
    }
    ContentState::Included {
        digest: Digest::of_bytes(bytes),
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 把文本中的凭据值替换成 `<redacted:NAME>`；名称保留，值不保留。
fn sanitize(text: &str, credentials: &[(String, Vec<u8>)]) -> String {
    let mut out = text.to_string();
    for (name, value) in credentials {
        if value.is_empty() {
            continue;
        }
        let needle = String::from_utf8_lossy(value).to_string();
        if needle.is_empty() || !out.contains(&needle) {
            continue;
        }
        out = out.replace(&needle, &format!("<redacted:{name}>"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &[u8] = b"sk-live-abc123";

    fn base_builder() -> ManifestBuilder {
        let mut builder = ManifestBuilder::new();
        builder.add_file("src/main.rs", b"fn main() {}\n");
        builder
    }

    #[test]
    fn invariant_i_47_package_carries_no_credential_value() {
        let mut builder = base_builder();
        builder.add_credential_env("GITHUB_TOKEN", TOKEN);
        builder.add_file(".env", b"GITHUB_TOKEN=sk-live-abc123\nDEBUG=1\n");
        builder.add_conflict("src/lib.rs", "合并时看到 token=sk-live-abc123，需人工处理");
        let manifest = builder.build("company-1", "project-1", "wp-r1").unwrap();

        // 1) 值不进入清单文本。
        let rendered = manifest.payload_json();
        assert!(!rendered.contains("sk-live-abc123"), "清单里不得出现凭据值");
        assert!(manifest.audit_for_secret_values(&[TOKEN]).is_empty());

        // 2) 凭据只留名称与摘要，摘要可由值复算。
        assert_eq!(manifest.secret_refs.len(), 1);
        let secret = &manifest.secret_refs[0];
        assert_eq!(secret.name(), "GITHUB_TOKEN");
        assert_eq!(
            secret.digest(),
            SecretRef::from_value("GITHUB_TOKEN", TOKEN).digest()
        );

        // 3) 含凭据的文件整条脱敏，不保留片段。
        let env_entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == ".env")
            .unwrap();
        assert_eq!(
            env_entry.state,
            ContentState::Redacted {
                reason: RedactionReason::Credential
            }
        );
        assert!(env_entry.state.content_digest().is_none());
        assert!(!env_entry.state.recoverable_from_package());

        // 4) 自由文本（冲突说明）也脱敏，名称保留、值不保留。
        let conflict = manifest.conflicts()[0];
        match &conflict.state {
            ContentState::Declared { detail } => {
                assert!(detail.contains("<redacted:GITHUB_TOKEN>"), "{detail}");
                assert!(!detail.contains("sk-live-abc123"));
            }
            other => panic!("冲突应为声明条目，实际 {other:?}"),
        }
    }

    #[test]
    fn manifest_lists_submodule_lfs_lockfile_toolchain_and_services() {
        let mut builder = base_builder();
        builder.add_submodule("vendor/opc", "3f9c2a1b4d5e6f708192a3b4c5d6e7f8091a2b3c");
        builder.add_lfs_pointer("assets/demo.mp4", "sha256:aaaa", 7_340_032);
        builder.add_lock_file("Cargo.lock", b"[[package]]\nname = \"bod\"\n");
        builder.add_toolchain_pin("rust-toolchain.toml", "1.83.0");
        builder.add_required_service("postgres", "需要 16.x，本地端口 5433");
        let manifest = builder.build("company-1", "project-1", "wp-r1").unwrap();

        for kind in [
            EntryKind::TrackedFile,
            EntryKind::Submodule,
            EntryKind::LfsPointer,
            EntryKind::LockFile,
            EntryKind::ToolchainPin,
            EntryKind::RequiredService,
        ] {
            assert!(
                manifest.entries.iter().any(|entry| entry.kind == kind),
                "缺少条目种类 {kind}"
            );
        }
        let codes: Vec<&str> = manifest
            .blocking_conditions()
            .iter()
            .map(|gap| gap.code)
            .collect();
        assert!(codes.contains(&"submodule_not_fetched"));
        assert!(codes.contains(&"lfs_object_not_fetched"));
        // 工具链固定与必需服务是声明/固定值，不是重建障碍。
        assert!(!codes.contains(&"large_file_needs_authorization"));
    }

    #[test]
    fn digest_existing_does_not_mean_content_recoverable() {
        let mut builder = base_builder();
        builder.add_lfs_pointer("assets/demo.mp4", "sha256:bbbb", 1024);
        let manifest = builder.build("company-1", "project-1", "wp-r1").unwrap();

        let lfs = manifest
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::LfsPointer)
            .unwrap();
        assert!(lfs.state.content_digest().is_none());
        assert!(!lfs.state.recoverable_from_package());
        assert!(matches!(lfs.state, ContentState::Pointer { .. }));

        let source = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "src/main.rs")
            .unwrap();
        assert!(source.state.content_digest().is_some());
        assert!(source.state.recoverable_from_package());

        let non_recoverable: Vec<&str> = manifest
            .non_recoverable()
            .iter()
            .map(|(entry, _)| entry.path.as_str())
            .collect();
        assert_eq!(non_recoverable, vec!["assets/demo.mp4"]);
    }

    #[test]
    fn large_file_is_referenced_by_authorization_not_inlined() {
        let mut builder = ManifestBuilder::new().with_inline_limit(8);
        builder.add_file("big.bin", &[7u8; 32]);
        let manifest = builder.build("company-1", "project-1", "wp-r1").unwrap();

        let entry = &manifest.entries[0];
        match &entry.state {
            ContentState::Referenced {
                authorization_ref,
                size_bytes,
            } => {
                assert_eq!(*size_bytes, 32);
                assert!(authorization_ref.starts_with("authorized-large-file:"));
            }
            other => panic!("大文件应按授权引用，实际 {other:?}"),
        }
        assert!(!entry.state.carries_content());
        assert!(
            manifest
                .blocking_conditions()
                .iter()
                .any(|gap| gap.code == "large_file_needs_authorization")
        );
    }

    #[test]
    fn customer_data_prefix_is_redacted_with_reason() {
        let mut builder = ManifestBuilder::new().block_customer_prefix("customers/");
        builder.add_file("customers/acme/contract.md", "客户合同内容".as_bytes());
        builder.add_file("src/main.rs", b"fn main() {}\n");
        let manifest = builder.build("company-1", "project-1", "wp-r1").unwrap();

        let contract = manifest
            .entries
            .iter()
            .find(|entry| entry.path.starts_with("customers/"))
            .unwrap();
        assert_eq!(
            contract.state,
            ContentState::Redacted {
                reason: RedactionReason::CustomerData
            }
        );
        let gap = manifest
            .blocking_conditions()
            .into_iter()
            .find(|gap| gap.path == contract.path)
            .unwrap();
        assert_eq!(gap.code, "redacted_material");
        assert!(gap.detail.contains("customer_data"));
    }

    #[test]
    fn external_session_state_is_reported_as_not_exported() {
        let mut builder = base_builder();
        builder.add_external_session("session-7f21");
        let manifest = builder.build("company-1", "project-1", "wp-r1").unwrap();

        let session = manifest
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::ExternalSessionState)
            .unwrap();
        assert_eq!(
            session.state,
            ContentState::NotExported {
                reason: NotExportedReason::RuntimeInternalState
            }
        );
        assert!(
            manifest
                .blocking_conditions()
                .iter()
                .any(|gap| gap.code == "external_session_state")
        );
    }

    #[test]
    fn lock_file_with_registry_token_is_redacted() {
        let mut builder = ManifestBuilder::new();
        builder.add_credential_env("REGISTRY_TOKEN", b"tok-registry-9");
        builder.add_lock_file(
            "package-lock.json",
            b"{\"resolved\":\"https://user:tok-registry-9@npm.internal/\"}",
        );
        let manifest = builder.build("company-1", "project-1", "wp-r1").unwrap();
        let lock = manifest
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::LockFile)
            .unwrap();
        assert_eq!(
            lock.state,
            ContentState::Redacted {
                reason: RedactionReason::Credential
            }
        );
        assert!(
            manifest
                .audit_for_secret_values(&[b"tok-registry-9"])
                .is_empty()
        );
    }

    #[test]
    fn empty_credential_value_is_rejected() {
        let mut builder = base_builder();
        builder.add_credential_env("EMPTY", b"");
        let err = builder
            .build("company-1", "project-1", "wp-r1")
            .unwrap_err();
        assert!(matches!(err, ManifestError::EmptyCredentialValue(name) if name == "EMPTY"));
    }

    #[test]
    fn duplicate_credential_name_is_rejected() {
        let mut builder = base_builder();
        builder.add_credential_env("TOKEN", b"first-value");
        builder.add_credential_env("TOKEN", b"second-value");
        let err = builder
            .build("company-1", "project-1", "wp-r1")
            .unwrap_err();
        assert!(matches!(err, ManifestError::DuplicateSecretName(name) if name == "TOKEN"));
    }

    #[test]
    fn duplicate_entry_path_is_rejected() {
        let mut builder = ManifestBuilder::new();
        builder.add_file("src/main.rs", b"a");
        builder.add_file("src/main.rs", b"b");
        let err = builder
            .build("company-1", "project-1", "wp-r1")
            .unwrap_err();
        assert!(matches!(err, ManifestError::DuplicateEntry { path, .. } if path == "src/main.rs"));
    }

    #[test]
    fn declared_conflict_requires_detail() {
        let mut builder = ManifestBuilder::new();
        builder.add_conflict("src/lib.rs", "   ");
        let err = builder
            .build("company-1", "project-1", "wp-r1")
            .unwrap_err();
        assert!(matches!(err, ManifestError::MissingConflictDetail(path) if path == "src/lib.rs"));
    }

    #[test]
    fn manifest_digest_is_stable_across_insertion_order() {
        let mut first = ManifestBuilder::new();
        first.add_file("b.rs", b"b");
        first.add_file("a.rs", b"a");
        first.add_credential_env("TOKEN", b"value-1");
        let first = first.build("company-1", "project-1", "wp-r1").unwrap();

        let mut second = ManifestBuilder::new();
        second.add_credential_env("TOKEN", b"value-1");
        second.add_file("a.rs", b"a");
        second.add_file("b.rs", b"b");
        let second = second.build("company-1", "project-1", "wp-r1").unwrap();

        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
        assert_eq!(first.entries, second.entries);
    }

    #[test]
    fn audit_flags_empty_candidate_instead_of_passing() {
        let manifest = base_builder()
            .build("company-1", "project-1", "wp-r1")
            .unwrap();
        let leaks = manifest.audit_for_secret_values(&[b""]);
        assert_eq!(leaks, vec!["candidate[0]:empty".to_string()]);
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let mut manifest = base_builder()
            .build("company-1", "project-1", "wp-r1")
            .unwrap();
        manifest.schema = "bod.workspace-manifest/v0".to_string();
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::UnsupportedSchema(_))
        ));
    }
}
