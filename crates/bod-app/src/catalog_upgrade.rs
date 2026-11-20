//! P5 任务 1.1—1.5：协作配置包的安装、升级与卸载边界（`team-catalog`）。
//!
//! 安装一套协作配置，风险不在"装不上"，而在装上去以后三件事被悄悄改变：
//! 已有对象被同名覆盖、本地改动被新包吞掉、装完顺手多拿了权限。本模块按这三点分四段：
//! 1. **安装事务**（1.1）：版本必须确定、digest 必须钉住、来源/许可证/`schema` 必须过关；
//!    预览先列清"新建 / 复用 / 冲突"。联网下载期间不持写锁，候选资源在正式启用前不执行。
//! 2. **安装记录与幂等**（1.2）：`CatalogInstallRecord` 记来源、digest、`schema`、目标 Company、
//!    对象映射、参数摘要与已应用修订；重复请求按逻辑键去重，绝不按名称覆盖已有对象。
//! 3. **三方升级**（1.3 / `invariant_i_44`）：原包基线 / 本地修订 / 新包版本三方比较；
//!    两边都改过的对象保留本地并生成提案；新增权限、外部程序与付费连接必须重新准入；
//!    安装包不携带真实密钥，也不会自动取得管理员权。
//! 4. **卸载边界**（1.4）：默认只解除追踪或停用包独占且无人使用的配置；Issue、交付、
//!    用量与证据保留，被引用的资源不级联删除。
//!
//! 另有一条贯穿全模块的边界（1.5）：能力目录回答"谁适合这项工作"，Team Catalog 回答
//! "怎样装一套配置"，两者数据身份不混；推荐、组织图与 `reports_to` 都不产生权限。
//!
//! 本模块不联网、不读写包文件、不落 DB：只做判断（版本是否确定、来源是否可信、内容是否被占、
//! 升级会不会覆盖本地、卸载会不会带走历史），下载、解包与写事务由宿主按这里的结论执行。
//!
//! 时间是固定格式的 UTC 串（`2026-11-21T09:00:00Z`），本模块只做字符串比较。

use crate::domain::model::{Assignment, Grant, Permission, Principal, Scope};
use crate::domain::rules::authorize;
use crate::stage_manifest::looks_like_credential;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 1.1 安装事务
// ---------------------------------------------------------------------------

/// 包来源。三种来源的信任假设不同，不能合成一个字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// 已登记目录（按 registry id 白名单）。
    Registry { registry_id: String },
    /// 通道方提供的 URL（按 host 白名单，且只取摘要不执行内容）。
    Url { host: String },
    /// 本地文件：需要显式开启，默认拒绝。
    LocalFile { path: String },
}

impl PackageSource {
    fn describe(&self) -> String {
        match self {
            Self::Registry { registry_id } => format!("registry:{registry_id}"),
            Self::Url { host } => format!("url:{host}"),
            Self::LocalFile { path } => format!("file:{path}"),
        }
    }
}

/// 包里要被安装的一个对象（Agent / Profile / Skill / 模板 / 规则集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageObject {
    pub object_id: String,
    pub name: String,
    pub kind: String,
    pub digest: String,
    /// 对象携带的字段：安装前要扫一遍有没有真实密钥。
    pub fields: BTreeMap<String, String>,
}

impl PackageObject {
    pub fn new(object_id: &str, name: &str, kind: &str, digest: &str) -> Self {
        Self {
            object_id: object_id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            digest: digest.to_string(),
            fields: BTreeMap::new(),
        }
    }

    pub fn with_field(mut self, key: &str, value: &str) -> Self {
        self.fields.insert(key.to_string(), value.to_string());
        self
    }
}

/// 包声明它需要的东西。这三类都不能由安装动作自动批准（1.3）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageRequires {
    pub permissions: Vec<String>,
    pub external_programs: Vec<String>,
    pub paid_connectors: Vec<String>,
}

/// 一个确定版本的协作配置包。版本与 digest 都是钉住的，没有 `latest`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRef {
    pub package_id: String,
    pub version: String,
    pub source: PackageSource,
    pub digest: String,
    pub license: String,
    pub schema_version: u32,
    pub objects: Vec<PackageObject>,
    pub requires: PackageRequires,
    /// 包是否请求管理员权。为真即拒绝：装一套配置不改变授权结构。
    pub requests_admin: bool,
}

impl PackageRef {
    pub fn new(package_id: &str, version: &str, source: PackageSource, digest: &str) -> Self {
        Self {
            package_id: package_id.to_string(),
            version: version.to_string(),
            source,
            digest: digest.to_string(),
            license: String::new(),
            schema_version: 1,
            objects: Vec::new(),
            requires: PackageRequires::default(),
            requests_admin: false,
        }
    }

    pub fn with_license(mut self, license: &str) -> Self {
        self.license = license.to_string();
        self
    }

    pub fn with_objects(mut self, objects: Vec<PackageObject>) -> Self {
        self.objects = objects;
        self
    }

    pub fn with_requires(mut self, requires: PackageRequires) -> Self {
        self.requires = requires;
        self
    }

    pub fn requesting_admin(mut self) -> Self {
        self.requests_admin = true;
        self
    }
}

/// 来源策略：谁允许被装、什么许可证允许、宿主支持到哪个 schema。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePolicy {
    pub allowed_registries: Vec<String>,
    pub allowed_hosts: Vec<String>,
    pub allowed_licenses: Vec<String>,
    pub supported_schema: u32,
    /// 本地文件默认不允许：走文件系统就能绕过目录审核。
    pub allow_local_files: bool,
}

impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            allowed_registries: vec!["registry-official".to_string()],
            allowed_hosts: vec!["packages.example.com".to_string()],
            allowed_licenses: vec!["Apache-2.0".to_string(), "MIT".to_string()],
            supported_schema: 3,
            allow_local_files: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRejection {
    /// `latest`、`^1.2`、`~1.2`、`*`、范围式版本都不算确定版本。
    FloatingVersion {
        version: String,
    },
    /// digest 必须是 `sha256:<64 hex>`。
    UnpinnedDigest {
        digest: String,
    },
    /// 声明 digest 与实际取到的内容不一致。
    DigestMismatch {
        expected: String,
        actual: String,
    },
    UntrustedSource {
        source: String,
    },
    LicenseMissing,
    LicenseNotAllowed {
        license: String,
    },
    SchemaTooNew {
        schema: u32,
        supported: u32,
    },
    RequestsAdmin,
    /// 包内字段出现形似凭据的值：安装包不得携带真实密钥。
    EmbeddedSecret {
        object_id: String,
        field: String,
    },
    /// 包内对象 id 已被别的内容占用：不能按 id 覆盖。
    ObjectIdConflict {
        id: String,
    },
}

impl InstallRejection {
    /// 给人看的拒绝理由。要求能直接贴进安装报告。
    pub fn message(&self) -> String {
        match self {
            Self::FloatingVersion { version } => {
                format!("版本 `{version}` 不是确定版本，安装必须指名具体版本")
            }
            Self::UnpinnedDigest { digest } => {
                format!("digest `{digest}` 未钉住，必须是 sha256:<64 hex>")
            }
            Self::DigestMismatch { expected, actual } => {
                format!("内容 digest 不匹配：声明 {expected}，实得 {actual}")
            }
            Self::UntrustedSource { source } => format!("来源 `{source}` 不在允许列表内"),
            Self::LicenseMissing => "包未声明许可证，不能安装".to_string(),
            Self::LicenseNotAllowed { license } => {
                format!("许可证 `{license}` 不在允许列表内")
            }
            Self::SchemaTooNew { schema, supported } => {
                format!("包 schema 版本 {schema} 高于宿主支持的 {supported}")
            }
            Self::RequestsAdmin => "包请求管理员权，安装不改变授权结构".to_string(),
            Self::EmbeddedSecret { object_id, field } => {
                format!("对象 `{object_id}` 的字段 `{field}` 形似真实凭据，安装包只能放凭据引用")
            }
            Self::ObjectIdConflict { id } => {
                format!("对象 id `{id}` 已被其他内容占用，安装不按 id 覆盖")
            }
        }
    }
}

fn is_pinned_version(version: &str) -> bool {
    if version.trim().is_empty() {
        return false;
    }
    let lowered = version.to_ascii_lowercase();
    if lowered == "latest" || lowered == "*" {
        return false;
    }
    let floating_markers = ['^', '~', '*', '>', '<', '=', ',', '|', ' '];
    if version.contains(floating_markers) {
        return false;
    }
    // 至少要有一个数字，"main"/"stable" 这类分支名不算版本。
    version.chars().any(|c| c.is_ascii_digit())
}

fn is_pinned_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// 安装前的校验。顺序固定：版本 → digest → 来源 → 许可证 → schema → 权限 → 密钥。
/// 前面的项不过就不必再谈后面的项。
pub fn verify_package(
    package: &PackageRef,
    actual_digest: &str,
    policy: &SourcePolicy,
) -> Result<(), InstallRejection> {
    if !is_pinned_version(&package.version) {
        return Err(InstallRejection::FloatingVersion {
            version: package.version.clone(),
        });
    }
    if !is_pinned_digest(&package.digest) {
        return Err(InstallRejection::UnpinnedDigest {
            digest: package.digest.clone(),
        });
    }
    if package.digest != actual_digest {
        return Err(InstallRejection::DigestMismatch {
            expected: package.digest.clone(),
            actual: actual_digest.to_string(),
        });
    }
    let source_trusted = match &package.source {
        PackageSource::Registry { registry_id } => policy.allowed_registries.contains(registry_id),
        PackageSource::Url { host } => policy.allowed_hosts.contains(host),
        PackageSource::LocalFile { .. } => policy.allow_local_files,
    };
    if !source_trusted {
        return Err(InstallRejection::UntrustedSource {
            source: package.source.describe(),
        });
    }
    if package.license.trim().is_empty() {
        return Err(InstallRejection::LicenseMissing);
    }
    if !policy.allowed_licenses.contains(&package.license) {
        return Err(InstallRejection::LicenseNotAllowed {
            license: package.license.clone(),
        });
    }
    if package.schema_version > policy.supported_schema {
        return Err(InstallRejection::SchemaTooNew {
            schema: package.schema_version,
            supported: policy.supported_schema,
        });
    }
    if package.requests_admin {
        return Err(InstallRejection::RequestsAdmin);
    }
    for object in &package.objects {
        for (field, value) in &object.fields {
            if looks_like_credential(value) {
                return Err(InstallRejection::EmbeddedSecret {
                    object_id: object.object_id.clone(),
                    field: field.clone(),
                });
            }
        }
    }
    Ok(())
}

/// 已安装对象。`installed_id` 与包内 `object_id` 可能不同：名称冲突时改 id 而不是覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledObject {
    pub installed_id: String,
    pub source_object_id: String,
    pub name: String,
    pub kind: String,
    pub digest: String,
    pub enabled: bool,
}

impl InstalledObject {
    pub fn new(installed_id: &str, name: &str, kind: &str, digest: &str) -> Self {
        Self {
            installed_id: installed_id.to_string(),
            source_object_id: installed_id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            digest: digest.to_string(),
            enabled: false,
        }
    }

    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }
}

/// 安装阶段。写锁只在 `Committing` 之后出现，下载阶段不持锁。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Downloading,
    Previewed,
    AwaitingApproval,
    Committing,
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallConflict {
    /// 同名但不同对象：可解决（改 id），但必须让用户看见。
    NameTaken { name: String, existing_id: String },
    /// 同 id 已被别的内容占用。
    IdTaken { id: String, kind: String },
    /// 同 id 的包版本低于已装版本。
    VersionDowngrade {
        id: String,
        installed: String,
        incoming: String,
    },
}

impl InstallConflict {
    /// 硬冲突必须解决后才能提交；`NameTaken` 由安装器改 id 解决，不阻断。
    pub fn is_blocking(&self) -> bool {
        !matches!(self, Self::NameTaken { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedObject {
    pub source_object_id: String,
    pub installed_id: String,
    pub digest: String,
    /// 预览阶段一律 `false`：候选资源正式启用前不执行。
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallPreview {
    pub create: Vec<PlannedObject>,
    pub reuse: Vec<String>,
    pub conflicts: Vec<InstallConflict>,
}

impl InstallPreview {
    pub fn blocking_conflicts(&self) -> Vec<&InstallConflict> {
        self.conflicts
            .iter()
            .filter(|conflict| conflict.is_blocking())
            .collect()
    }

    /// 预览与提交都不允许出现"已启用"的候选。
    pub fn has_executable_candidate(&self) -> bool {
        self.create.iter().any(|planned| planned.enabled)
    }
}

/// 预览：算出新建、复用与冲突。不写任何东西。
pub fn preview_install(package: &PackageRef, existing: &[InstalledObject]) -> InstallPreview {
    let mut preview = InstallPreview::default();
    for object in &package.objects {
        let same_id = existing
            .iter()
            .find(|installed| installed.installed_id == object.object_id);
        if let Some(installed) = same_id {
            if installed.digest == object.digest && installed.kind == object.kind {
                preview.reuse.push(installed.installed_id.clone());
            } else {
                preview.conflicts.push(InstallConflict::IdTaken {
                    id: installed.installed_id.clone(),
                    kind: installed.kind.clone(),
                });
            }
            continue;
        }
        let name_taken = existing
            .iter()
            .find(|installed| installed.name == object.name);
        if let Some(installed) = name_taken {
            // 同名不同 id：不覆盖，改名装一份新的。
            preview.conflicts.push(InstallConflict::NameTaken {
                name: object.name.clone(),
                existing_id: installed.installed_id.clone(),
            });
            preview.create.push(PlannedObject {
                source_object_id: object.object_id.clone(),
                installed_id: format!("{}-{}", object.object_id, package.version),
                digest: object.digest.clone(),
                enabled: false,
            });
            continue;
        }
        preview.create.push(PlannedObject {
            source_object_id: object.object_id.clone(),
            installed_id: object.object_id.clone(),
            digest: object.digest.clone(),
            enabled: false,
        });
    }
    preview
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallApproval {
    pub actor: String,
    pub at: String,
    /// 授权范围（哪个人/哪个项目装的），只用于留痕，不推导权限。
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnError {
    SkippedPreview,
    BlockingConflicts(usize),
    NotApproved,
    AlreadyApplied,
}

/// 安装事务：下载 → 预览 → 待授权 → 提交 → 完成。每一步都不允许跳跃。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallTxn {
    pub logical_key: String,
    pub phase: InstallPhase,
    pub preview: Option<InstallPreview>,
    pub approval: Option<InstallApproval>,
}

impl InstallTxn {
    pub fn begin(logical_key: &str) -> Self {
        Self {
            logical_key: logical_key.to_string(),
            phase: InstallPhase::Downloading,
            preview: None,
            approval: None,
        }
    }

    /// 写锁只在提交阶段存在。下载联网时不占写事务。
    pub fn holds_write_lock(&self) -> bool {
        matches!(self.phase, InstallPhase::Committing | InstallPhase::Applied)
    }

    /// 候选资源在 `Applied` 之前一律不可执行。
    pub fn executable_now(&self) -> bool {
        self.phase == InstallPhase::Applied
    }

    pub fn attach_preview(mut self, preview: InstallPreview) -> Self {
        self.preview = Some(preview);
        self.phase = InstallPhase::Previewed;
        self
    }

    pub fn approve(mut self, approval: InstallApproval) -> Result<Self, TxnError> {
        let Some(preview) = &self.preview else {
            return Err(TxnError::SkippedPreview);
        };
        let blocking = preview.blocking_conflicts().len();
        if blocking > 0 {
            return Err(TxnError::BlockingConflicts(blocking));
        }
        self.approval = Some(approval);
        self.phase = InstallPhase::AwaitingApproval;
        Ok(self)
    }

    pub fn commit(mut self) -> Result<Self, TxnError> {
        if self.phase == InstallPhase::Applied {
            return Err(TxnError::AlreadyApplied);
        }
        if self.approval.is_none() || self.phase != InstallPhase::AwaitingApproval {
            return Err(TxnError::NotApproved);
        }
        self.phase = InstallPhase::Committing;
        Ok(self)
    }

    /// 提交完成后才启用对象。
    pub fn finish(mut self) -> Result<Self, TxnError> {
        if self.phase == InstallPhase::Applied {
            return Err(TxnError::AlreadyApplied);
        }
        if self.phase != InstallPhase::Committing {
            return Err(TxnError::NotApproved);
        }
        self.phase = InstallPhase::Applied;
        Ok(self)
    }
}

// ---------------------------------------------------------------------------
// 1.2 安装记录与幂等
// ---------------------------------------------------------------------------

/// 安装记录。一个逻辑键只对应一份记录，重复请求返回原结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogInstallRecord {
    pub record_id: String,
    pub company_id: String,
    pub package_id: String,
    pub package_version: String,
    pub digest: String,
    pub schema_version: u32,
    pub source: String,
    pub params_digest: String,
    pub applied_revision: u64,
    pub object_id_map: BTreeMap<String, String>,
    pub enabled_objects: Vec<String>,
}

impl CatalogInstallRecord {
    /// 逻辑键 = Company + 包 + 版本 + digest + 参数摘要。
    /// 不含包名与展示名：改名不算新安装，换内容才算。
    pub fn logical_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.company_id, self.package_id, self.package_version, self.digest, self.params_digest
        )
    }

    pub fn installed_id(&self, source_object_id: &str) -> Option<&str> {
        self.object_id_map.get(source_object_id).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CatalogStore {
    pub installs: Vec<CatalogInstallRecord>,
    pub objects: Vec<InstalledObject>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// 新装：返回这次创建与复用的对象。
    Installed {
        record_id: String,
        created: Vec<String>,
        reused: Vec<String>,
        conflicts: Vec<InstallConflict>,
        revision: u64,
    },
    /// 重复请求：原样返回，不创建第二份配置。
    Replayed {
        record_id: String,
        revision: u64,
    },
    Rejected {
        rejection: InstallRejection,
    },
}

/// 安装：逻辑键去重 → 预览 → 落对象 → 写记录。全程不按名称覆盖已有对象。
pub fn install(
    store: &mut CatalogStore,
    package: &PackageRef,
    company_id: &str,
    params_digest: &str,
    record_id: &str,
    policy: &SourcePolicy,
) -> InstallOutcome {
    let mut probe = CatalogInstallRecord {
        record_id: record_id.to_string(),
        company_id: company_id.to_string(),
        package_id: package.package_id.clone(),
        package_version: package.version.clone(),
        digest: package.digest.clone(),
        schema_version: package.schema_version,
        source: package.source.describe(),
        params_digest: params_digest.to_string(),
        applied_revision: 0,
        object_id_map: BTreeMap::new(),
        enabled_objects: Vec::new(),
    };
    let key = probe.logical_key();
    if let Some(existing) = store.installs.iter().find(|r| r.logical_key() == key) {
        return InstallOutcome::Replayed {
            record_id: existing.record_id.clone(),
            revision: existing.applied_revision,
        };
    }
    if let Err(rejection) = verify_package(package, &package.digest, policy) {
        return InstallOutcome::Rejected { rejection };
    }

    let preview = preview_install(package, &store.objects);
    if let Some(conflict) = preview.blocking_conflicts().first() {
        let id = match conflict {
            InstallConflict::IdTaken { id, .. } => id.clone(),
            InstallConflict::VersionDowngrade { id, .. } => id.clone(),
            InstallConflict::NameTaken { existing_id, .. } => existing_id.clone(),
        };
        return InstallOutcome::Rejected {
            rejection: InstallRejection::ObjectIdConflict { id },
        };
    }

    store.revision += 1;
    let revision = store.revision;
    for planned in &preview.create {
        let object = package
            .objects
            .iter()
            .find(|object| object.object_id == planned.source_object_id);
        let (name, kind) = match object {
            Some(object) => (object.name.clone(), object.kind.clone()),
            None => (planned.installed_id.clone(), "unknown".to_string()),
        };
        let mut installed =
            InstalledObject::new(&planned.installed_id, &name, &kind, &planned.digest);
        installed.source_object_id = planned.source_object_id.clone();
        installed.enabled = true;
        store.objects.push(installed);
        probe.object_id_map.insert(
            planned.source_object_id.clone(),
            planned.installed_id.clone(),
        );
        probe.enabled_objects.push(planned.installed_id.clone());
    }
    for reused in &preview.reuse {
        if let Some(object) = store
            .objects
            .iter()
            .find(|object| object.installed_id == *reused)
        {
            probe
                .object_id_map
                .insert(object.source_object_id.clone(), reused.clone());
        }
    }
    probe.applied_revision = revision;
    store.installs.push(probe);

    InstallOutcome::Installed {
        record_id: record_id.to_string(),
        created: preview
            .create
            .iter()
            .map(|p| p.installed_id.clone())
            .collect(),
        reused: preview.reuse,
        conflicts: preview.conflicts,
        revision,
    }
}

// ---------------------------------------------------------------------------
// 1.3 三方升级
// ---------------------------------------------------------------------------

/// 三方比较的输入快照。只比 digest，不比字段：字段变化必然改 digest。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSnapshot {
    pub object_id: String,
    pub name: String,
    pub digest: String,
}

impl ObjectSnapshot {
    pub fn new(object_id: &str, name: &str, digest: &str) -> Self {
        Self {
            object_id: object_id.to_string(),
            name: name.to_string(),
            digest: digest.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeStep {
    Unchanged {
        id: String,
    },
    /// 只有新包改了：跟随新包。
    TakeRemote {
        id: String,
        from: String,
        to: String,
    },
    /// 只有本地改了：保留本地。
    KeepLocal {
        id: String,
        why: KeepLocalReason,
    },
    /// 两边都改且不一致：保留本地并生成提案。
    Conflict {
        id: String,
        proposal_id: String,
    },
    Added {
        id: String,
    },
    /// 新包删了、本地没改：跟随删除。
    Removed {
        id: String,
    },
    /// 新包删了但本地改过：不删，转提案。
    SkippedRemoval {
        id: String,
        proposal_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepLocalReason {
    LocalOnlyChange,
    RemoteRemovedButLocallyEdited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeProposal {
    pub proposal_id: String,
    pub object_id: String,
    pub base_digest: Option<String>,
    /// 本地把对象删掉也是"本地修订"的一种，故为 `Option`。
    pub local_digest: Option<String>,
    pub remote_digest: Option<String>,
    pub status: ProposalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalStatus {
    /// 提案只是候选：不会自己改本地文件。
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReAdmissionKind {
    Permission { name: String },
    ExternalProgram { name: String },
    PaidConnector { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadmissionItem {
    pub kind: ReAdmissionKind,
    pub source_package: String,
}

impl ReadmissionItem {
    pub fn label(&self) -> String {
        match &self.kind {
            ReAdmissionKind::Permission { name } => format!("新增权限 `{name}`"),
            ReAdmissionKind::ExternalProgram { name } => format!("新增外部程序 `{name}`"),
            ReAdmissionKind::PaidConnector { name } => format!("新增付费连接 `{name}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpgradePlan {
    pub steps: Vec<UpgradeStep>,
    pub preserved_local: Vec<String>,
    pub proposals: Vec<UpgradeProposal>,
    pub readmission: Vec<ReadmissionItem>,
}

impl UpgradePlan {
    pub fn proposal_for(&self, object_id: &str) -> Option<&UpgradeProposal> {
        self.proposals
            .iter()
            .find(|proposal| proposal.object_id == object_id)
    }

    /// 提案数量即"还没处理完的本地分歧"，不是错误。
    pub fn pending_proposals(&self) -> usize {
        self.proposals.len()
    }
}

fn find<'a>(items: &'a [ObjectSnapshot], id: &str) -> Option<&'a ObjectSnapshot> {
    items.iter().find(|item| item.object_id == id)
}

fn added_requires(
    base: &PackageRequires,
    remote: &PackageRequires,
    package: &str,
) -> Vec<ReadmissionItem> {
    let mut items = Vec::new();
    for permission in &remote.permissions {
        if !base.permissions.contains(permission) {
            items.push(ReadmissionItem {
                kind: ReAdmissionKind::Permission {
                    name: permission.clone(),
                },
                source_package: package.to_string(),
            });
        }
    }
    for program in &remote.external_programs {
        if !base.external_programs.contains(program) {
            items.push(ReadmissionItem {
                kind: ReAdmissionKind::ExternalProgram {
                    name: program.clone(),
                },
                source_package: package.to_string(),
            });
        }
    }
    for connector in &remote.paid_connectors {
        if !base.paid_connectors.contains(connector) {
            items.push(ReadmissionItem {
                kind: ReAdmissionKind::PaidConnector {
                    name: connector.clone(),
                },
                source_package: package.to_string(),
            });
        }
    }
    items
}

/// 三方比较。绝不静默覆盖本地：任何"两边都改"的情形都落成提案。
pub fn three_way_upgrade(
    base: &[ObjectSnapshot],
    local: &[ObjectSnapshot],
    remote: &[ObjectSnapshot],
    base_requires: &PackageRequires,
    remote_requires: &PackageRequires,
    package_id: &str,
) -> UpgradePlan {
    let mut plan = UpgradePlan::default();
    let mut ids: Vec<String> = Vec::new();
    for snapshot in base.iter().chain(local).chain(remote) {
        if !ids.contains(&snapshot.object_id) {
            ids.push(snapshot.object_id.clone());
        }
    }
    ids.sort();

    for id in ids {
        let b = find(base, &id);
        let l = find(local, &id);
        let r = find(remote, &id);
        match (b, l, r) {
            // 本地仍有这个对象（基线可能有也可能没有，本地可能改过也可能没改）。
            (base_obj, Some(local_obj), Some(remote_obj)) => {
                let base_digest = base_obj.map(|snapshot| snapshot.digest.clone());
                let local_changed = base_digest.as_deref() != Some(local_obj.digest.as_str());
                let remote_changed = base_digest.as_deref() != Some(remote_obj.digest.as_str());
                if local_obj.digest == remote_obj.digest {
                    plan.steps.push(UpgradeStep::Unchanged { id });
                } else if !local_changed {
                    plan.steps.push(UpgradeStep::TakeRemote {
                        id,
                        from: local_obj.digest.clone(),
                        to: remote_obj.digest.clone(),
                    });
                } else if !remote_changed {
                    plan.preserved_local.push(id.clone());
                    plan.steps.push(UpgradeStep::KeepLocal {
                        id,
                        why: KeepLocalReason::LocalOnlyChange,
                    });
                } else {
                    let proposal_id = format!("proposal-{id}");
                    plan.preserved_local.push(id.clone());
                    plan.proposals.push(UpgradeProposal {
                        proposal_id: proposal_id.clone(),
                        object_id: id.clone(),
                        base_digest,
                        local_digest: Some(local_obj.digest.clone()),
                        remote_digest: Some(remote_obj.digest.clone()),
                        status: ProposalStatus::Pending,
                    });
                    plan.steps.push(UpgradeStep::Conflict { id, proposal_id });
                }
            }
            // 本地删了、新包还在。
            (Some(base_obj), None, Some(remote_obj)) => {
                if base_obj.digest == remote_obj.digest {
                    // 删除是本地唯一的改动：尊重删除。
                    plan.steps.push(UpgradeStep::Removed { id });
                } else {
                    // 本地删了、上游又改过：不偷偷复活，也不擅自删除，转提案。
                    let proposal_id = format!("proposal-{id}");
                    plan.preserved_local.push(id.clone());
                    plan.proposals.push(UpgradeProposal {
                        proposal_id: proposal_id.clone(),
                        object_id: id.clone(),
                        base_digest: Some(base_obj.digest.clone()),
                        local_digest: None,
                        remote_digest: Some(remote_obj.digest.clone()),
                        status: ProposalStatus::Pending,
                    });
                    plan.steps.push(UpgradeStep::Conflict { id, proposal_id });
                }
            }
            // 新包新加的对象。
            (None, None, Some(_)) => plan.steps.push(UpgradeStep::Added { id }),
            // 新包删了、本地还在。
            (Some(base_obj), Some(local_obj), None) => {
                if base_obj.digest == local_obj.digest {
                    plan.steps.push(UpgradeStep::Removed { id });
                } else {
                    let proposal_id = format!("proposal-{id}");
                    plan.preserved_local.push(id.clone());
                    plan.proposals.push(UpgradeProposal {
                        proposal_id: proposal_id.clone(),
                        object_id: id.clone(),
                        base_digest: Some(base_obj.digest.clone()),
                        local_digest: Some(local_obj.digest.clone()),
                        remote_digest: None,
                        status: ProposalStatus::Pending,
                    });
                    plan.steps
                        .push(UpgradeStep::SkippedRemoval { id, proposal_id });
                }
            }
            // 本地独有、新包没有：原样保留，不算冲突。
            (None, Some(_), None) => {
                plan.preserved_local.push(id.clone());
                plan.steps.push(UpgradeStep::KeepLocal {
                    id,
                    why: KeepLocalReason::LocalOnlyChange,
                });
            }
            // 两边都删了：已收敛。
            (Some(_), None, None) => plan.steps.push(UpgradeStep::Unchanged { id }),
            (None, None, None) => {}
        }
    }

    plan.readmission = added_requires(base_requires, remote_requires, package_id);
    plan
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    Applied {
        applied: Vec<String>,
        preserved_local: Vec<String>,
        pending_proposals: usize,
    },
    /// 新包要求新权限/外部程序/付费连接：准入没走完就不落地。
    BlockedReAdmission { items: Vec<ReadmissionItem> },
}

/// 应用升级。准入项必须逐条被批准（按内容相等匹配），否则整次升级不落地。
pub fn apply_upgrade(plan: &UpgradePlan, approved: &[ReadmissionItem]) -> UpgradeOutcome {
    let missing: Vec<ReadmissionItem> = plan
        .readmission
        .iter()
        .filter(|item| !approved.contains(item))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return UpgradeOutcome::BlockedReAdmission { items: missing };
    }
    let applied = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            UpgradeStep::TakeRemote { id, .. }
            | UpgradeStep::Added { id }
            | UpgradeStep::Removed { id } => Some(id.clone()),
            _ => None,
        })
        .collect();
    UpgradeOutcome::Applied {
        applied,
        preserved_local: plan.preserved_local.clone(),
        pending_proposals: plan.pending_proposals(),
    }
}

// ---------------------------------------------------------------------------
// 1.4 卸载边界
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReferenceKind {
    Issue,
    Delivery,
    Usage,
    Evidence,
    OtherPackage,
    OtherProject,
}

impl ReferenceKind {
    /// 这些引用必须保留：卸载不撤历史。
    pub fn must_retain(self) -> bool {
        matches!(
            self,
            Self::Issue | Self::Delivery | Self::Usage | Self::Evidence
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub kind: ReferenceKind,
    pub object_id: String,
    pub referrer: String,
}

impl Reference {
    pub fn new(kind: ReferenceKind, object_id: &str, referrer: &str) -> Self {
        Self {
            kind,
            object_id: object_id.to_string(),
            referrer: referrer.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallMode {
    /// 只解除安装追踪，对象留在原地。
    DetachTracking,
    /// 停用包独占且没人使用的配置。
    DisableExclusiveUnused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallImpact {
    pub mode: UninstallMode,
    pub referenced: Vec<Reference>,
    pub disabled: Vec<String>,
    pub retained: Vec<Reference>,
    /// 恒为 `false`：卸载永不级联删除。
    pub cascade_delete: bool,
    /// 因为有引用而不能停用/删除的对象。
    pub blocked_objects: Vec<String>,
}

impl UninstallImpact {
    pub fn summary(&self) -> String {
        format!(
            "被引用 {} 项，停用 {} 项，保留历史 {} 项，级联删除 {}",
            self.referenced.len(),
            self.disabled.len(),
            self.retained.len(),
            if self.cascade_delete { "是" } else { "否" }
        )
    }
}

/// 卸载影响分析。引用存在即保留，不外推"没人用就删"。
pub fn plan_uninstall(record: &CatalogInstallRecord, references: &[Reference]) -> UninstallImpact {
    plan_uninstall_with_mode(record, references, UninstallMode::DisableExclusiveUnused)
}

pub fn plan_uninstall_with_mode(
    record: &CatalogInstallRecord,
    references: &[Reference],
    mode: UninstallMode,
) -> UninstallImpact {
    let owned: Vec<String> = record.object_id_map.values().cloned().collect();
    let mut referenced = Vec::new();
    let mut retained = Vec::new();
    let mut blocked = Vec::new();
    for reference in references {
        if !owned.contains(&reference.object_id) {
            continue;
        }
        referenced.push(reference.clone());
        if reference.kind.must_retain() {
            retained.push(reference.clone());
        }
        if !blocked.contains(&reference.object_id) {
            blocked.push(reference.object_id.clone());
        }
    }
    let disabled = match mode {
        UninstallMode::DetachTracking => Vec::new(),
        UninstallMode::DisableExclusiveUnused => owned
            .iter()
            .filter(|id| !blocked.contains(id) && record.enabled_objects.contains(id))
            .cloned()
            .collect(),
    };
    UninstallImpact {
        mode,
        referenced,
        disabled,
        retained,
        cascade_delete: false,
        blocked_objects: blocked,
    }
}

// ---------------------------------------------------------------------------
// 1.5 目录与权限分离
// ---------------------------------------------------------------------------

/// 能力目录条目：回答"当前谁适合这项工作"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityEntry {
    pub entry_id: String,
    pub agent_id: String,
    pub capability: String,
    /// 0 表示未推荐；数字越小越靠前。
    pub recommended_rank: u32,
}

impl CapabilityEntry {
    pub fn new(entry_id: &str, agent_id: &str, capability: &str, rank: u32) -> Self {
        Self {
            entry_id: entry_id.to_string(),
            agent_id: agent_id.to_string(),
            capability: capability.to_string(),
            recommended_rank: rank,
        }
    }
}

/// 数据身份：目录条目与安装包不共用命名空间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataIdentity {
    Capability { entry_id: String },
    CatalogPackage { package_id: String },
}

impl DataIdentity {
    pub fn namespace(&self) -> &'static str {
        match self {
            Self::Capability { .. } => "capability",
            Self::CatalogPackage { .. } => "catalog",
        }
    }

    pub fn key(&self) -> String {
        match self {
            Self::Capability { entry_id } => format!("capability:{entry_id}"),
            Self::CatalogPackage { package_id } => format!("catalog:{package_id}"),
        }
    }
}

pub fn identity_of_entry(entry: &CapabilityEntry) -> DataIdentity {
    DataIdentity::Capability {
        entry_id: entry.entry_id.clone(),
    }
}

pub fn identity_of_package(package: &PackageRef) -> DataIdentity {
    DataIdentity::CatalogPackage {
        package_id: package.package_id.clone(),
    }
}

/// 两类身份是否被混用：同一个字符串既当目录条目 id 又当包 id。
pub fn mixes_identity(left: &DataIdentity, right: &DataIdentity) -> bool {
    left.namespace() != right.namespace()
        && match (left, right) {
            (
                DataIdentity::Capability { entry_id },
                DataIdentity::CatalogPackage { package_id },
            ) => entry_id == package_id,
            (
                DataIdentity::CatalogPackage { package_id },
                DataIdentity::Capability { entry_id },
            ) => package_id == entry_id,
            _ => false,
        }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityCollision {
    pub shared_id: String,
}

/// 目录条目与安装包不得共用 id：同一 id 既当"谁适合"又当"装什么"就分不清授权对象。
pub fn check_identity_separation(
    entries: &[CapabilityEntry],
    packages: &[PackageRef],
) -> Result<(), IdentityCollision> {
    for entry in entries {
        for package in packages {
            if entry.entry_id == package.package_id {
                return Err(IdentityCollision {
                    shared_id: entry.entry_id.clone(),
                });
            }
        }
    }
    Ok(())
}

/// 授权事实。推荐与组织图刻意不在这里：它们不产生权限。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccessFacts {
    pub grants: Vec<Grant>,
    pub assignments: Vec<Assignment>,
    pub recommendations: Vec<CapabilityEntry>,
    pub reports_to: BTreeMap<String, String>,
}

impl AccessFacts {
    /// 判定只看 grants 与 assignments。`recommendations` / `reports_to` 不参与，
    /// 因此"推荐为首选"或"直属上级是管理员"都换不来权限。
    pub fn permits(&self, principal: &Principal, scope: &Scope, permission: Permission) -> bool {
        let assignment = self
            .assignments
            .iter()
            .find(|assignment| assignment.owner.id == principal.id);
        authorize(principal, scope, permission, &self.grants, assignment).is_ok()
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DomainId;
    use crate::domain::model::{Owner, OwnerKind, PrincipalKind};

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_C: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn package(version: &str, digest: &str) -> PackageRef {
        PackageRef::new(
            "pkg-team-basic",
            version,
            PackageSource::Registry {
                registry_id: "registry-official".to_string(),
            },
            digest,
        )
        .with_license("Apache-2.0")
        .with_objects(vec![
            PackageObject::new("agent-reviewer", "评审 Agent", "agent", DIGEST_A),
            PackageObject::new("skill-tdd", "TDD 技能", "skill", DIGEST_B),
        ])
    }

    fn scope() -> Scope {
        Scope {
            company_id: DomainId::new("company-default"),
            project_id: Some(DomainId::new("project-1")),
            issue_id: Some(DomainId::new("issue-1")),
        }
    }

    fn principal(id: &str) -> Principal {
        Principal {
            id: DomainId::new(id),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    fn grant(principal_id: &str, permission: Permission, revision: u64) -> Grant {
        Grant {
            principal_id: DomainId::new(principal_id),
            scope: scope(),
            permission,
            revision,
            active: true,
        }
    }

    /// 1.1：版本必须确定。`latest`、范围式、分支名都拒。
    #[test]
    fn task_1_1_floating_versions_are_rejected() {
        let policy = SourcePolicy::default();
        for version in [
            "latest",
            "^1.2.0",
            "~1.2",
            "*",
            "1.2 - 2.0",
            ">=1.2",
            "main",
        ] {
            let pkg = package(version, DIGEST_A);
            let err = verify_package(&pkg, DIGEST_A, &policy)
                .expect_err(&format!("版本 `{version}` 必须被拒绝"));
            assert!(
                matches!(err, InstallRejection::FloatingVersion { .. }),
                "版本 `{version}` 的拒绝理由应为 FloatingVersion，实得 {err:?}"
            );
            assert!(!err.message().is_empty());
        }
        assert!(verify_package(&package("1.2.0", DIGEST_A), DIGEST_A, &policy).is_ok());
    }

    /// 1.1：来源、许可证、schema、管理员权、内嵌密钥各自独立拒绝。
    #[test]
    fn task_1_1_source_license_schema_and_secrets_are_checked() {
        let policy = SourcePolicy::default();

        let untrusted = PackageRef::new(
            "pkg-x",
            "1.0.0",
            PackageSource::Url {
                host: "evil.example.net".to_string(),
            },
            DIGEST_A,
        )
        .with_license("MIT");
        assert!(matches!(
            verify_package(&untrusted, DIGEST_A, &policy),
            Err(InstallRejection::UntrustedSource { .. })
        ));

        let local = PackageRef::new(
            "pkg-x",
            "1.0.0",
            PackageSource::LocalFile {
                path: "/tmp/pkg.zip".to_string(),
            },
            DIGEST_A,
        )
        .with_license("MIT");
        assert!(
            matches!(
                verify_package(&local, DIGEST_A, &policy),
                Err(InstallRejection::UntrustedSource { .. })
            ),
            "本地文件默认不允许"
        );

        let no_license = PackageRef::new(
            "pkg-x",
            "1.0.0",
            PackageSource::Registry {
                registry_id: "registry-official".to_string(),
            },
            DIGEST_A,
        );
        assert!(matches!(
            verify_package(&no_license, DIGEST_A, &policy),
            Err(InstallRejection::LicenseMissing)
        ));

        let gpl = package("1.0.0", DIGEST_A).with_license("GPL-3.0");
        assert!(matches!(
            verify_package(&gpl, DIGEST_A, &policy),
            Err(InstallRejection::LicenseNotAllowed { .. })
        ));

        let mut too_new = package("1.0.0", DIGEST_A);
        too_new.schema_version = 9;
        assert!(matches!(
            verify_package(&too_new, DIGEST_A, &policy),
            Err(InstallRejection::SchemaTooNew {
                schema: 9,
                supported: 3
            })
        ));

        assert!(matches!(
            verify_package(
                &package("1.0.0", DIGEST_A).requesting_admin(),
                DIGEST_A,
                &policy
            ),
            Err(InstallRejection::RequestsAdmin)
        ));

        let mut leaking = package("1.0.0", DIGEST_A);
        leaking.objects[0].fields.insert(
            "api_key".to_string(),
            "sk-live-9f8e7d6c5b4a3210".to_string(),
        );
        let err = verify_package(&leaking, DIGEST_A, &policy).expect_err("内嵌密钥必须被拒");
        match err {
            InstallRejection::EmbeddedSecret { object_id, field } => {
                assert_eq!(object_id, "agent-reviewer");
                assert_eq!(field, "api_key");
            }
            other => panic!("应报 EmbeddedSecret，实得 {other:?}"),
        }
    }

    /// 1.1：digest 必须钉住且与实得内容一致。
    #[test]
    fn task_1_1_digest_must_be_pinned_and_match() {
        let policy = SourcePolicy::default();
        assert!(matches!(
            verify_package(&package("1.0.0", "sha256:abc"), "sha256:abc", &policy),
            Err(InstallRejection::UnpinnedDigest { .. })
        ));
        let err = verify_package(&package("1.0.0", DIGEST_A), DIGEST_B, &policy)
            .expect_err("内容与声明不符必须拒绝");
        match err {
            InstallRejection::DigestMismatch { expected, actual } => {
                assert_eq!(expected, DIGEST_A);
                assert_eq!(actual, DIGEST_B);
            }
            other => panic!("应报 DigestMismatch，实得 {other:?}"),
        }
    }

    /// 1.1：预览先给出新建/复用/冲突，下载阶段不持写锁，候选启用前不执行。
    #[test]
    fn task_1_1_preview_then_approve_without_write_lock_during_download() {
        let pkg = package("1.0.0", DIGEST_A);
        let preview = preview_install(&pkg, &[]);
        assert_eq!(preview.create.len(), 2);
        assert!(preview.reuse.is_empty());
        assert!(preview.conflicts.is_empty());
        assert!(
            !preview.has_executable_candidate(),
            "预览里不能有已启用候选"
        );

        let mut txn = InstallTxn::begin("company-default|pkg-team-basic|1.0.0");
        assert!(!txn.holds_write_lock(), "下载阶段不持写锁");
        assert!(!txn.executable_now());

        // 没预览就批准：拒。
        assert_eq!(
            InstallTxn::begin("k").approve(InstallApproval {
                actor: "user-1".to_string(),
                at: "2026-11-21T09:00:00Z".to_string(),
                scope: "project-1".to_string(),
            }),
            Err(TxnError::SkippedPreview)
        );

        txn = txn.attach_preview(preview);
        assert!(!txn.holds_write_lock());
        txn = txn
            .approve(InstallApproval {
                actor: "user-1".to_string(),
                at: "2026-11-21T09:00:00Z".to_string(),
                scope: "project-1".to_string(),
            })
            .expect("无硬冲突可批准");
        txn = txn.commit().expect("批准后可提交");
        assert!(txn.holds_write_lock(), "提交阶段才持写锁");
        assert!(!txn.executable_now(), "提交完成前不执行候选");
        txn = txn.finish().expect("提交后可完成");
        assert!(txn.executable_now());
        assert_eq!(txn.finish(), Err(TxnError::AlreadyApplied));
    }

    /// 1.1：同 id 被别的内容占用是硬冲突，必须挡在批准之前。
    #[test]
    fn task_1_1_id_taken_blocks_approval() {
        let pkg = package("1.0.0", DIGEST_A);
        let existing = vec![InstalledObject::new(
            "agent-reviewer",
            "别的 Agent",
            "agent",
            DIGEST_B,
        )];
        let preview = preview_install(&pkg, &existing);
        assert_eq!(preview.blocking_conflicts().len(), 1);
        let txn = InstallTxn::begin("k").attach_preview(preview);
        assert_eq!(
            txn.approve(InstallApproval {
                actor: "user-1".to_string(),
                at: "2026-11-21T09:00:00Z".to_string(),
                scope: "project-1".to_string(),
            }),
            Err(TxnError::BlockingConflicts(1))
        );
    }

    /// 1.2：重复请求返回原结果，不创建第二份配置。
    #[test]
    fn task_1_2_repeated_install_replays_original_record() {
        let policy = SourcePolicy::default();
        let pkg = package("1.0.0", DIGEST_A);
        let mut store = CatalogStore::default();
        let first = install(
            &mut store,
            &pkg,
            "company-default",
            "params-1",
            "rec-1",
            &policy,
        );
        let created = match &first {
            InstallOutcome::Installed {
                created, revision, ..
            } => {
                assert_eq!(created.len(), 2);
                assert_eq!(*revision, 1);
                created.clone()
            }
            other => panic!("首次安装应成功，实得 {other:?}"),
        };
        assert_eq!(store.objects.len(), 2);

        let second = install(
            &mut store,
            &pkg,
            "company-default",
            "params-1",
            "rec-2",
            &policy,
        );
        match second {
            InstallOutcome::Replayed {
                record_id,
                revision,
            } => {
                assert_eq!(record_id, "rec-1", "重复请求应返回原记录，而不是新记录");
                assert_eq!(revision, 1);
            }
            other => panic!("重复请求应 Replayed，实得 {other:?}"),
        }
        assert_eq!(store.objects.len(), 2, "不得创建第二份配置");
        assert_eq!(store.installs.len(), 1);
        assert_eq!(store.revision, 1, "幂等请求不推进修订");
        assert_eq!(store.installs[0].object_id_map.len(), 2);
        assert_eq!(created.len(), 2);
    }

    /// 另一个包，对象 id 不同但展示名与本包相同：用来验证"同名不覆盖"。
    fn other_package() -> PackageRef {
        PackageRef::new(
            "pkg-team-extra",
            "1.0.0",
            PackageSource::Registry {
                registry_id: "registry-official".to_string(),
            },
            DIGEST_B,
        )
        .with_license("MIT")
        .with_objects(vec![PackageObject::new(
            "agent-reviewer-alt",
            "评审 Agent",
            "agent",
            DIGEST_B,
        )])
    }

    /// 1.2：同名不覆盖。另一个包带来同名对象时改 id 落一份新的。
    #[test]
    fn task_1_2_same_name_never_overwrites_existing_object() {
        let policy = SourcePolicy::default();
        let mut store = CatalogStore::default();
        install(
            &mut store,
            &package("1.0.0", DIGEST_A),
            "c",
            "p1",
            "rec-1",
            &policy,
        );

        let outcome = install(&mut store, &other_package(), "c", "p1", "rec-2", &policy);
        match outcome {
            InstallOutcome::Installed {
                created, conflicts, ..
            } => {
                assert!(
                    matches!(conflicts.first(), Some(InstallConflict::NameTaken { .. })),
                    "同名必须报 NameTaken，实得 {conflicts:?}"
                );
                assert!(
                    created.iter().all(|id| id.ends_with("-1.0.0")),
                    "同名对象应改 id：{created:?}"
                );
                assert_eq!(created, vec!["agent-reviewer-alt-1.0.0".to_string()]);
            }
            other => panic!("换包应新装，实得 {other:?}"),
        }
        assert_eq!(store.objects.len(), 3);
        let originals: Vec<&InstalledObject> = store
            .objects
            .iter()
            .filter(|object| object.installed_id == "agent-reviewer")
            .collect();
        assert_eq!(originals.len(), 1, "原对象必须只有一份，且未改名");
        assert_eq!(originals[0].name, "评审 Agent");
        assert_eq!(originals[0].digest, DIGEST_A, "原对象内容不得被覆盖");
    }

    /// 1.3 / invariant_i_44：本地改过的 Skill 与新包冲突时保留本地并出提案，不静默覆盖。
    #[test]
    fn invariant_i_44_upgrade_preserves_local_edit_and_proposes() {
        let base = vec![
            ObjectSnapshot::new("agent-reviewer", "评审 Agent", DIGEST_A),
            ObjectSnapshot::new("skill-tdd", "TDD 技能", DIGEST_A),
        ];
        let local = vec![
            ObjectSnapshot::new("agent-reviewer", "评审 Agent", DIGEST_A),
            ObjectSnapshot::new("skill-tdd", "TDD 技能（本地调过）", DIGEST_B),
        ];
        let remote = vec![
            ObjectSnapshot::new("agent-reviewer", "评审 Agent", DIGEST_C),
            ObjectSnapshot::new("skill-tdd", "TDD 技能", DIGEST_C),
        ];
        let plan = three_way_upgrade(
            &base,
            &local,
            &remote,
            &PackageRequires::default(),
            &PackageRequires::default(),
            "pkg-team-basic",
        );

        assert!(plan.preserved_local.contains(&"skill-tdd".to_string()));
        let proposal = plan.proposal_for("skill-tdd").expect("冲突必须出提案");
        assert_eq!(proposal.local_digest.as_deref(), Some(DIGEST_B));
        assert_eq!(proposal.remote_digest.as_deref(), Some(DIGEST_C));
        assert_eq!(proposal.status, ProposalStatus::Pending);

        // 只有新包改了的对象跟随新包。
        assert!(plan.steps.iter().any(|step| matches!(
            step,
            UpgradeStep::TakeRemote { id, .. } if id == "agent-reviewer"
        )));

        match apply_upgrade(&plan, &[]) {
            UpgradeOutcome::Applied {
                applied,
                preserved_local,
                pending_proposals,
            } => {
                assert_eq!(applied, vec!["agent-reviewer".to_string()]);
                assert!(preserved_local.contains(&"skill-tdd".to_string()));
                assert_eq!(pending_proposals, 1);
            }
            other => panic!("无新增准入项应直接应用，实得 {other:?}"),
        }
    }

    /// 1.3 / invariant_i_44：新增权限必须重新准入；包不得自动取得管理员权。
    #[test]
    fn invariant_i_44_new_permissions_require_readmission_and_never_grant_admin() {
        let base = vec![ObjectSnapshot::new(
            "agent-reviewer",
            "评审 Agent",
            DIGEST_A,
        )];
        let local = base.clone();
        let remote = vec![ObjectSnapshot::new(
            "agent-reviewer",
            "评审 Agent",
            DIGEST_B,
        )];
        let base_requires = PackageRequires {
            permissions: vec!["read-project".to_string()],
            external_programs: Vec::new(),
            paid_connectors: Vec::new(),
        };
        let new_requires = PackageRequires {
            permissions: vec!["read-project".to_string(), "admin".to_string()],
            external_programs: vec!["ffmpeg".to_string()],
            paid_connectors: vec!["connector-billing".to_string()],
        };
        let plan = three_way_upgrade(
            &base,
            &local,
            &remote,
            &base_requires,
            &new_requires,
            "pkg-team-basic",
        );
        assert_eq!(plan.readmission.len(), 3, "三类新增项都要重准入");

        match apply_upgrade(&plan, &[]) {
            UpgradeOutcome::BlockedReAdmission { items } => {
                assert_eq!(items.len(), 3);
                assert!(items.iter().any(|item| item.label().contains("admin")));
                assert!(items.iter().any(|item| item.label().contains("ffmpeg")));
            }
            other => panic!("未准入时升级必须被挡，实得 {other:?}"),
        }

        // 逐条批准后落地，且批准动作本身不产生 Grant。
        match apply_upgrade(&plan, &plan.readmission) {
            UpgradeOutcome::Applied { applied, .. } => {
                assert_eq!(applied, vec!["agent-reviewer".to_string()]);
            }
            other => panic!("准入齐全后应应用，实得 {other:?}"),
        }
        let facts = AccessFacts {
            grants: Vec::new(),
            assignments: Vec::new(),
            recommendations: Vec::new(),
            reports_to: BTreeMap::new(),
        };
        assert!(!facts.permits(
            &principal("agent-reviewer"),
            &scope(),
            Permission::ManageBudget
        ));
    }

    /// 1.3：新包删掉本地改过的对象时，不删、转提案。
    #[test]
    fn task_1_3_remote_removal_of_locally_edited_object_is_skipped() {
        let base = vec![
            ObjectSnapshot::new("skill-tdd", "TDD 技能", DIGEST_A),
            ObjectSnapshot::new("tmpl-bug", "缺陷模板", DIGEST_A),
        ];
        let local = vec![
            ObjectSnapshot::new("skill-tdd", "TDD 技能（本地调过）", DIGEST_B),
            ObjectSnapshot::new("tmpl-bug", "缺陷模板", DIGEST_A),
        ];
        let remote = vec![ObjectSnapshot::new("tmpl-bug", "缺陷模板", DIGEST_A)];
        let plan = three_way_upgrade(
            &base,
            &local,
            &remote,
            &PackageRequires::default(),
            &PackageRequires::default(),
            "pkg",
        );
        assert!(plan.steps.iter().any(|step| matches!(
            step,
            UpgradeStep::SkippedRemoval { id, .. } if id == "skill-tdd"
        )));
        assert!(plan.proposal_for("skill-tdd").is_some());
        assert!(plan.steps.iter().any(|step| matches!(
            step,
            UpgradeStep::Unchanged { id } if id == "tmpl-bug"
        )));
    }

    /// 1.4：被已接受 Issue 引用的 Agent 不级联删除，历史证据保留。
    #[test]
    fn task_1_4_uninstall_keeps_referenced_objects_and_history() {
        let policy = SourcePolicy::default();
        let mut store = CatalogStore::default();
        install(
            &mut store,
            &package("1.0.0", DIGEST_A),
            "c",
            "p1",
            "rec-1",
            &policy,
        );
        let record = store.installs[0].clone();

        let references = vec![
            Reference::new(ReferenceKind::Issue, "agent-reviewer", "issue-42"),
            Reference::new(ReferenceKind::Delivery, "skill-tdd", "delivery-7"),
            Reference::new(ReferenceKind::Evidence, "agent-reviewer", "evidence-3"),
        ];
        let impact = plan_uninstall(&record, &references);
        assert!(!impact.cascade_delete, "卸载永不级联删除");
        assert_eq!(impact.referenced.len(), 3);
        assert_eq!(impact.retained.len(), 3, "Issue/交付/证据都要保留");
        assert!(
            impact
                .blocked_objects
                .contains(&"agent-reviewer".to_string())
        );
        assert!(
            impact.disabled.is_empty(),
            "两个对象都被引用，不得停用：{:?}",
            impact.disabled
        );
        assert!(impact.summary().contains("级联删除 否"));

        // 只解除追踪时，什么都不停用。
        let detached =
            plan_uninstall_with_mode(&record, &references, UninstallMode::DetachTracking);
        assert!(detached.disabled.is_empty());
        assert_eq!(detached.mode, UninstallMode::DetachTracking);

        // 没人用的独占配置才停用。
        let unused = plan_uninstall_with_mode(&record, &[], UninstallMode::DisableExclusiveUnused);
        assert_eq!(unused.disabled.len(), 2);
        assert!(unused.referenced.is_empty());
    }

    /// 1.5：推荐与 reports_to 不产生权限，授权仍只看 Grant 与 Assignment。
    #[test]
    fn task_1_5_recommendation_and_reports_to_never_grant_permission() {
        let mut facts = AccessFacts::default();
        facts
            .recommendations
            .push(CapabilityEntry::new("entry-1", "user-1", "code-review", 1));
        facts
            .reports_to
            .insert("user-1".to_string(), "user-admin".to_string());
        assert!(
            !facts.permits(&principal("user-1"), &scope(), Permission::ManageBudget),
            "被推荐为首选不等于可改预算"
        );
        assert!(!facts.permits(&principal("user-1"), &scope(), Permission::ManageScope));

        facts
            .grants
            .push(grant("user-1", Permission::ManageScope, 1));
        assert!(facts.permits(&principal("user-1"), &scope(), Permission::ManageScope));
        assert!(
            !facts.permits(&principal("user-1"), &scope(), Permission::ManageBudget),
            "一项权限不派生另一项"
        );

        // 增删推荐与组织关系不改变判定结果。
        facts.recommendations.clear();
        facts.reports_to.clear();
        assert!(facts.permits(&principal("user-1"), &scope(), Permission::ManageScope));
    }

    /// 1.5：派发还需要本人 Assignment，推荐不能替代指派。
    #[test]
    fn task_1_5_dispatch_requires_assignment_not_recommendation() {
        let facts = AccessFacts {
            grants: vec![grant("user-1", Permission::Dispatch, 3)],
            assignments: Vec::new(),
            recommendations: vec![CapabilityEntry::new("e", "user-1", "code", 1)],
            reports_to: BTreeMap::new(),
        };
        assert!(
            !facts.permits(&principal("user-1"), &scope(), Permission::Dispatch),
            "没有 Assignment 不得派发"
        );
        let assigned = AccessFacts {
            assignments: vec![Assignment {
                owner: Owner {
                    kind: OwnerKind::User,
                    id: DomainId::new("user-1"),
                },
                scope: scope(),
                grant_revision: 3,
            }],
            ..facts
        };
        assert!(assigned.permits(&principal("user-1"), &scope(), Permission::Dispatch));
    }

    /// 1.5：目录条目与安装包不共用数据身份。
    #[test]
    fn task_1_5_capability_and_catalog_identities_never_mix() {
        let pkg = package("1.0.0", DIGEST_A);
        let separate = vec![CapabilityEntry::new(
            "entry-code-review",
            "agent-1",
            "review",
            1,
        )];
        assert!(
            check_identity_separation(&separate, std::slice::from_ref(&pkg)).is_ok(),
            "不同 id 应通过"
        );

        let colliding = vec![CapabilityEntry::new(
            "pkg-team-basic",
            "agent-1",
            "review",
            1,
        )];
        let err = check_identity_separation(&colliding, std::slice::from_ref(&pkg))
            .expect_err("同一 id 兼作目录条目与包必须被拒绝");
        assert_eq!(err.shared_id, "pkg-team-basic");
        assert!(mixes_identity(
            &identity_of_entry(&colliding[0]),
            &identity_of_package(&pkg)
        ));
        assert!(!mixes_identity(
            &identity_of_entry(&separate[0]),
            &identity_of_package(&pkg)
        ));
        assert_eq!(identity_of_entry(&separate[0]).namespace(), "capability");
        assert_eq!(identity_of_package(&pkg).namespace(), "catalog");
    }
}
