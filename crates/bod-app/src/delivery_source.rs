//! P8 §11 来源身份（3.2）：五元组、候选与追加式来源修订。
//!
//! 本模块只有**规范化与校验**，没有 SQL：身份怎么判定、候选什么时候能转正、
//! 什么算「同一事件」，都是纯函数；落库在 `store` 里。
//!
//! 三条不能让步的语义：
//!
//! 1. **身份是五元组，不是 URL**（design D2）。Company + Project + 连接实例 +
//!    对象类型 + 提供方稳定对象 ID。`project_id` 在唯一索引里，所以两个项目各自的
//!    `#318` 是两个来源——这正是「跨项目同名 PR」用例要保住的东西。
//! 2. **只有定位时进候选，不猜**。URL、纯文本、乃至「把 URL 填进了 ID 字段」都进候选，
//!    由负责人确认后才成为身份。候选不是「待完善的身份」：它没有稳定 ID，也就没有身份。
//! 3. **同一事件只确认一次，但可变标签要留痕**。幂等键是 `(version, digest)` 派生的
//!    内容指纹，**不含观察时间**——含时间的话，重复上报会变成两条「新修订」。
//!    反过来，同一标签先后解析出两个摘要，指纹不同，两条修订都留着（§11 可变标签）。
//!
//! 观察与来源的连接是「可空外键」：人工补证可以没有来源修订（它是人写的事实）。
//! 连接器观察必须带来源修订这一条，属导入路径（3.5）在写入原语里强制，
//! 不在库里用 CHECK 一刀切——那样 3.1 已落库的观察会被判成非法。

/// 对象类型与稳定 ID 的长度上限：身份是索引键，超长值先拒绝，不让它进唯一索引。
const MAX_OBJECT_TYPE: usize = 64;
const MAX_PROVIDER_ID: usize = 256;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 来源相关的拒绝原因。码是稳定字符串：客户端与日志按码分支，改码等于改契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// 公司为空：五元组的第一段不成立（多租户下身份必须能定位到公司）。
    EmptyCompanyId,
    /// 有稳定 ID 却没给对象类型：五元组不成立，属调用方漏字段（不是「只有 URL」）。
    MissingObjectType,
    EmptyObjectType,
    InvalidObjectType(String),
    EmptyProviderObjectId,
    /// 传进来的 ID 是个 URL：那是定位，不是提供方稳定对象 ID。
    ProviderIdIsLocator(String),
    EmptyLocator,
    /// 版本与摘要都为空：无法区分「同一事件重复上报」与「不同事件」，不允许登记。
    RevisionWithoutContent,
    /// 传入的指纹与 `(version, digest)` 重算结果不一致：幂等键被伪造或算错了。
    FingerprintMismatch {
        declared: String,
        derived: String,
    },
    CandidateAlreadyDecided(String),
    /// 确认候选时给的身份与候选的作用域（公司/项目/连接）不一致。
    CandidateScopeMismatch {
        candidate_project: String,
        identity_project: String,
    },
    /// 登记身份或确认候选时没有记录「谁确认的」：确认人是责任归属，不能为空。
    MissingConfirmer,
    /// 提交候选没有记录提交人：无法回溯是谁认为那个定位值得确认。
    MissingProposer,
    CandidateNotFound(String),
    SourceNotFound(String),
    SourceRevisionNotFound(String),
    /// 来源与跟踪项不属于同一项目：跨项目的来源不得挂到本项目的交付上。
    SourceProjectMismatch {
        source_project: String,
        tracking_project: String,
    },
    /// 这个来源身份已经承载了另一条跟踪关系（3.3）。
    ///
    /// 不「改挂」也不「再挂一条」：前者的意思是悄悄挪走一条已承诺的交付，
    /// 后者会让「重复导入返回已有关系」失去唯一对象可返回。
    SourceAlreadyLinked {
        source_id: String,
        tracking_id: String,
        linked_tracking_id: String,
    },
    /// 挂来源到跟踪项时没有记录操作者：这也是责任归属，不能为空。
    MissingLinker,
}

impl SourceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyCompanyId => "SOURCE_EMPTY_COMPANY",
            Self::MissingObjectType => "SOURCE_MISSING_OBJECT_TYPE",
            Self::EmptyObjectType => "SOURCE_EMPTY_OBJECT_TYPE",
            Self::InvalidObjectType(_) => "SOURCE_INVALID_OBJECT_TYPE",
            Self::EmptyProviderObjectId => "SOURCE_EMPTY_PROVIDER_ID",
            Self::ProviderIdIsLocator(_) => "SOURCE_PROVIDER_ID_IS_LOCATOR",
            Self::EmptyLocator => "SOURCE_EMPTY_LOCATOR",
            Self::RevisionWithoutContent => "SOURCE_REVISION_WITHOUT_CONTENT",
            Self::FingerprintMismatch { .. } => "SOURCE_FINGERPRINT_MISMATCH",
            Self::CandidateAlreadyDecided(_) => "SOURCE_CANDIDATE_ALREADY_DECIDED",
            Self::CandidateScopeMismatch { .. } => "SOURCE_CANDIDATE_SCOPE_MISMATCH",
            Self::MissingConfirmer => "SOURCE_MISSING_CONFIRMER",
            Self::MissingProposer => "SOURCE_MISSING_PROPOSER",
            Self::CandidateNotFound(_) => "SOURCE_CANDIDATE_NOT_FOUND",
            Self::SourceNotFound(_) => "SOURCE_NOT_FOUND",
            Self::SourceRevisionNotFound(_) => "SOURCE_REVISION_NOT_FOUND",
            Self::SourceProjectMismatch { .. } => "SOURCE_PROJECT_MISMATCH",
            Self::SourceAlreadyLinked { .. } => "SOURCE_ALREADY_LINKED",
            Self::MissingLinker => "SOURCE_MISSING_LINKER",
        }
    }

    pub fn explain(&self) -> String {
        match self {
            Self::EmptyCompanyId => "来源身份缺少公司：五元组第一段不成立".to_string(),
            Self::MissingObjectType => {
                "给了提供方稳定对象 ID 却没有对象类型：五元组不成立（只有定位请走候选）".to_string()
            }
            Self::EmptyObjectType => "来源对象类型不能为空".to_string(),
            Self::InvalidObjectType(value) => {
                format!("来源对象类型 {value:?} 不是 kebab-case（只允许小写字母、数字与连字符）")
            }
            Self::EmptyProviderObjectId => "提供方稳定对象 ID 不能为空".to_string(),
            Self::ProviderIdIsLocator(value) => {
                format!("{value:?} 是定位（URL），不是提供方稳定对象 ID：请走候选确认")
            }
            Self::EmptyLocator => "候选必须有定位（URL 或纯文本），否则无从确认".to_string(),
            Self::RevisionWithoutContent => {
                "来源修订至少要带版本或摘要之一：两者都空无法区分重复上报与新事件".to_string()
            }
            Self::FingerprintMismatch { declared, derived } => format!(
                "内容指纹与版本/摘要不符（传入 {declared:?}，重算 {derived:?}）：\
                 幂等键必须由内容派生，不能由调用方自己编"
            ),
            Self::CandidateAlreadyDecided(state) => {
                format!("候选已是 {state}：确认或驳回只能发生一次")
            }
            Self::CandidateScopeMismatch {
                candidate_project,
                identity_project,
            } => format!(
                "候选属于项目 {candidate_project}，身份却指向项目 {identity_project}：\
                 确认不得把候选挪到别的项目"
            ),
            Self::MissingConfirmer => {
                "确认来源身份必须记录确认人（confirmed_by 不能为空）".to_string()
            }
            Self::MissingProposer => {
                "提交来源候选必须记录提交人（proposed_by 不能为空）".to_string()
            }
            Self::CandidateNotFound(id) => format!("候选 {id} 不存在"),
            Self::SourceNotFound(id) => format!("来源 {id} 不存在"),
            Self::SourceRevisionNotFound(id) => format!("来源修订 {id} 不存在"),
            Self::SourceProjectMismatch {
                source_project,
                tracking_project,
            } => format!(
                "来源属于项目 {source_project}，跟踪项属于项目 {tracking_project}：\
                 跨项目来源不得挂到本项目交付上"
            ),
            Self::SourceAlreadyLinked {
                source_id,
                tracking_id,
                linked_tracking_id,
            } => format!(
                "来源 {source_id} 已承载跟踪项 {linked_tracking_id}，不能再挂到 {tracking_id}：\
                 一个来源身份只承载一条跟踪关系（同一对象导入两次应返回已有关系）"
            ),
            Self::MissingLinker => {
                "把来源挂到跟踪项必须记录操作者（linked_by 不能为空）".to_string()
            }
        }
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explain())
    }
}

impl std::error::Error for SourceError {}

// ---------------------------------------------------------------------------
// 身份
// ---------------------------------------------------------------------------

/// 来源身份五元组。`connection_id` 为空串表示没有连接器实例（人工补证、内部关联），
/// 它表示「无」，不表示「未知」——未知的连接器会让身份不可比，那种情况只能进候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    pub company_id: String,
    pub project_id: String,
    pub connection_id: String,
    pub object_type: String,
    pub provider_object_id: String,
}

impl SourceIdentity {
    /// 校验五元组的每一段。任一不成立就不该造成身份。
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.company_id.trim().is_empty() {
            return Err(SourceError::EmptyCompanyId);
        }
        if self.object_type.trim().is_empty() {
            return Err(SourceError::EmptyObjectType);
        }
        if !is_kebab_case(&self.object_type) {
            return Err(SourceError::InvalidObjectType(self.object_type.clone()));
        }
        let provider = self.provider_object_id.trim();
        if provider.is_empty() {
            return Err(SourceError::EmptyProviderObjectId);
        }
        if looks_like_locator(provider) {
            return Err(SourceError::ProviderIdIsLocator(provider.to_string()));
        }
        if provider.chars().count() > MAX_PROVIDER_ID {
            return Err(SourceError::EmptyProviderObjectId);
        }
        Ok(())
    }

    /// 供日志与错误信息用的可读坐标。**不参与唯一性判定**——唯一性由数据库索引保证。
    pub fn describe(&self) -> String {
        let connection = if self.connection_id.is_empty() {
            "no-connection"
        } else {
            self.connection_id.as_str()
        };
        format!(
            "{}/{}/{}/{}/{}",
            self.company_id, self.project_id, connection, self.object_type, self.provider_object_id
        )
    }
}

/// 对象类型是 kebab-case：`pull-request` / `commit` / `merge-request`。
///
/// 不做白名单（连接器种类会持续增加），但拒绝大小写混用与空格——
/// 否则 `PullRequest` 与 `pull-request` 会变成两个来源身份。
fn is_kebab_case(value: &str) -> bool {
    if value.is_empty() || value.chars().count() > MAX_OBJECT_TYPE {
        return false;
    }
    value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}

/// 是否是「定位」而不是稳定 ID。只认显式 URL 形态：`scheme://`。
///
/// 不按「含斜杠」判断：commit sha 不会含斜杠，但某些提供方的节点 ID 含 `/`
/// （如 `owner/repo#318` 之外的 `gid://gitlab/Issue/1`——后者本身就是 URL 形态，
/// 会正确落进候选）。
fn looks_like_locator(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || value.contains("://")
}

/// 解析结果：要么是可确定身份，要么要进候选等负责人确认。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceResolution {
    Identity(SourceIdentity),
    Candidate(CandidateProposal),
}

/// 候选提案：只有定位，没有稳定身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateProposal {
    pub company_id: String,
    pub project_id: String,
    pub connection_id: String,
    /// 空串表示对象类型尚未确定。不猜。
    pub object_type: String,
    pub locator: String,
}

/// 导入路径的唯一判定入口：能不能直接成为身份，还是只能进候选。
///
/// 规则只有三条，按顺序：
/// 1. 没给 `provider_object_id` → 候选（要求 `locator` 非空）。
/// 2. 给了 ID 但没给 `object_type` → 报错。这是调用方漏字段，不是「只有 URL」；
///    静默降级成候选会让「忘了传类型」永远停在待确认里，而且不报错。
/// 3. 给了 ID 但它是 URL 形态 → 候选，定位优先用传入的 `locator`，
///    没传就用那个 URL。常见错误正是把 URL 填进 ID 字段。
pub fn resolve(
    company_id: &str,
    project_id: &str,
    connection_id: &str,
    object_type: Option<&str>,
    provider_object_id: Option<&str>,
    locator: &str,
) -> Result<SourceResolution, SourceError> {
    let object_type = object_type.map(str::trim).unwrap_or("");
    let provider = provider_object_id.map(str::trim).unwrap_or("");
    let locator = locator.trim();

    if provider.is_empty() {
        return Ok(SourceResolution::Candidate(CandidateProposal {
            company_id: company_id.to_string(),
            project_id: project_id.to_string(),
            connection_id: connection_id.to_string(),
            object_type: object_type.to_string(),
            locator: require_locator(locator)?,
        }));
    }
    if object_type.is_empty() {
        return Err(SourceError::MissingObjectType);
    }
    if looks_like_locator(provider) {
        // 把 URL 填进 ID 字段：它不是稳定身份，落到候选，定位用 URL 本身。
        let locator = if locator.is_empty() {
            provider
        } else {
            locator
        };
        return Ok(SourceResolution::Candidate(CandidateProposal {
            company_id: company_id.to_string(),
            project_id: project_id.to_string(),
            connection_id: connection_id.to_string(),
            object_type: object_type.to_string(),
            locator: require_locator(locator)?,
        }));
    }

    let identity = SourceIdentity {
        company_id: company_id.to_string(),
        project_id: project_id.to_string(),
        connection_id: connection_id.to_string(),
        object_type: object_type.to_string(),
        provider_object_id: provider.to_string(),
    };
    identity.validate()?;
    Ok(SourceResolution::Identity(identity))
}

fn require_locator(locator: &str) -> Result<String, SourceError> {
    if locator.is_empty() {
        return Err(SourceError::EmptyLocator);
    }
    Ok(locator.to_string())
}

// ---------------------------------------------------------------------------
// 候选
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateState {
    Pending,
    Confirmed,
    Rejected,
}

impl CandidateState {
    pub fn parse(text: &str) -> Result<Self, SourceError> {
        match text {
            "pending" => Ok(Self::Pending),
            "confirmed" => Ok(Self::Confirmed),
            "rejected" => Ok(Self::Rejected),
            other => Err(SourceError::CandidateAlreadyDecided(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
        }
    }
}

/// 候选的状态机：只能从 `pending` 出发一次。确认过的候选再改指向，
/// 会让「这个定位属于哪个身份」失去唯一依据，所以不改、只拒。
pub fn decide(current: CandidateState) -> Result<CandidateState, SourceError> {
    match current {
        CandidateState::Pending => Ok(current),
        other => Err(SourceError::CandidateAlreadyDecided(
            other.as_str().to_string(),
        )),
    }
}

/// 确认候选时要检查的作用域：候选的公司/项目/连接必须与身份一致。
///
/// 只查这三段，不查对象类型与稳定 ID——那两段正是确认时要补上的东西。
pub fn ensure_scope_matches(
    candidate: &CandidateRow,
    identity: &SourceIdentity,
) -> Result<(), SourceError> {
    if candidate.company_id != identity.company_id
        || candidate.project_id != identity.project_id
        || candidate.connection_id != identity.connection_id
    {
        return Err(SourceError::CandidateScopeMismatch {
            candidate_project: candidate.project_id.clone(),
            identity_project: identity.project_id.clone(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 修订
// ---------------------------------------------------------------------------

/// 内容指纹：同一事件的幂等依据。
///
/// 只由 `(version, digest)` 派生，**不含观察时间**：含时间的话每次重复上报都是新指纹，
/// 「同一重复事件 MUST 只确认一次」就不成立。两者都空时报错——空指纹会把所有
/// 「没带内容」的上报合成一条，那是把不同的东西当成同一件。
pub fn revision_fingerprint(version: &str, digest: &str) -> Result<String, SourceError> {
    let version = version.trim();
    let digest = digest.trim();
    if version.is_empty() && digest.is_empty() {
        return Err(SourceError::RevisionWithoutContent);
    }
    // 分隔符用不可打印字符，避免 version 里含分隔符时拼出同一指纹。
    Ok(format!("{version}\u{1f}{digest}"))
}

// ---------------------------------------------------------------------------
// 事实行
// ---------------------------------------------------------------------------

/// `delivery_sources` 一行：已确认的来源身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub source_id: String,
    pub company_id: String,
    pub project_id: String,
    pub connection_id: String,
    pub object_type: String,
    pub provider_object_id: String,
    pub locator: String,
    pub confirmed_by: String,
    pub confirmed_at_ms: i64,
    pub created_at_ms: i64,
}

impl SourceRow {
    /// 人可读坐标（与 `SourceIdentity::describe` 同形）。
    pub fn describe(&self) -> String {
        SourceIdentity {
            company_id: self.company_id.clone(),
            project_id: self.project_id.clone(),
            connection_id: self.connection_id.clone(),
            object_type: self.object_type.clone(),
            provider_object_id: self.provider_object_id.clone(),
        }
        .describe()
    }
}

/// `delivery_source_candidates` 一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateRow {
    pub candidate_id: String,
    pub company_id: String,
    pub project_id: String,
    pub connection_id: String,
    /// 空串表示对象类型未确定。
    pub object_type: String,
    pub locator: String,
    pub proposed_by: String,
    pub proposed_at_ms: i64,
    pub state: CandidateState,
    pub resolved_source_id: Option<String>,
    pub decided_by: String,
    pub decided_at_ms: Option<i64>,
}

/// `delivery_source_revisions` 一行。追加式，只读不写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRevisionRow {
    pub source_revision_id: String,
    pub company_id: String,
    pub source_id: String,
    pub version: String,
    pub digest: String,
    pub content_fingerprint: String,
    pub actor_id: String,
    pub observed_at: String,
    pub observed_at_ms: i64,
    pub recorded_at_ms: i64,
}

/// 一个来源的修订时间线，按观察时间升序。
///
/// 用来回答「这个标签先后解析成了什么」。**不做冲突判定**：同一版本两个摘要才是冲突，
/// 新版本换摘要是正常更新；两者要靠版本比较区分（3.5 的导入路径）。
/// 本段只保证「每次解析都留着」，不猜哪一次是对的。
pub fn digest_history(revisions: &[SourceRevisionRow]) -> Vec<(String, String)> {
    let mut ordered: Vec<&SourceRevisionRow> = revisions.iter().collect();
    ordered.sort_by_key(|revision| (revision.observed_at_ms, revision.recorded_at_ms));
    ordered
        .into_iter()
        .filter(|revision| !revision.digest.trim().is_empty())
        .map(|revision| (revision.observed_at.clone(), revision.digest.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// 导入命令（3.3：命令重放 vs 对象重复的分界处）
// ---------------------------------------------------------------------------

/// 导入来源的命令域。写进回执，**不参与**载荷指纹——两者分开比对，
/// 调用方才能分清自己是「换了命令」还是「换了载荷」。
pub const COMMAND_IMPORT_SOURCE: &str = "import-delivery-source";

/// 导入一次外部来源。这是本仓里唯一同时踩到两种去重的地方：
///
/// - `idempotency_key` 管**命令重放**：同键同载荷 → 返回首次结果，不再写入。
/// - 五元组身份管**对象重复**：换个键再导同一个 PR → 返回已有跟踪关系，不建第二项。
///
/// `new_*_id` 三个字段只在**新建**时使用（与 `register_source` 同口径），
/// 命中已有对象时被忽略。它们参与载荷指纹：改 ID 重试是另一条命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSourceCommand {
    pub idempotency_key: String,
    pub company_id: String,
    pub project_id: String,
    /// 连接实例。空串表示无连接器（人工录入）。
    pub connection_id: String,
    pub object_type: Option<String>,
    pub provider_object_id: Option<String>,
    pub locator: String,
    /// 跟踪项标题。只有真的要建跟踪项时才用到。
    pub title: String,
    pub requirement: String,
    pub owner_id: String,
    pub new_tracking_id: String,
    pub new_source_id: String,
    pub new_candidate_id: String,
    /// 谁导入的。登记主体（§11 验收表要求可追溯），也进载荷指纹。
    pub imported_by: String,
    /// 这次动作的关联 ID（由入口层给：HTTP 请求 ID、CLI 会话 ID）。
    ///
    /// **不拿幂等键顶替**：幂等键是调用方自编的字符串，取个 `k-secret` 之类的名字就会撞上
    /// 审计的脱敏闸门（`SECRET_MARKERS` 含 `secret`），整条导入被拒——一个与业务无关的
    /// 键名不该决定写入成败。关联 ID 属于入口层，与键的取值无关。
    ///
    /// 也不进载荷指纹：它是链路标识，不影响结果与责任归属。
    pub correlation_id: String,
    /// 这次导入的业务时刻（RFC 3339），直接作为审计的 `decided_at`。
    ///
    /// 不从 `now_ms` 反推：审计的业务时间定义就是「调用方给的时刻」，
    /// 库层反推出来的只是第二份落库时间。也**不进载荷指纹**——换个时钟重放同一条命令
    /// 应当命中重放，而不是被当成「同键异载荷」拒掉。
    pub decided_at: String,
}

impl ImportSourceCommand {
    /// 载荷指纹的输入：**所有影响结果或影响责任归属的字段**。
    ///
    /// 刻意不含 `idempotency_key`：含了的话换一个键就等于换一份载荷，
    /// 「同键异载荷」永远撞不上——那正好把本模块要拦的事放过去了。
    pub fn payload_fields(&self) -> Vec<(&'static str, &str)> {
        vec![
            ("project_id", self.project_id.as_str()),
            ("connection_id", self.connection_id.as_str()),
            ("object_type", self.object_type.as_deref().unwrap_or("")),
            (
                "provider_object_id",
                self.provider_object_id.as_deref().unwrap_or(""),
            ),
            ("locator", self.locator.as_str()),
            ("title", self.title.as_str()),
            ("requirement", self.requirement.as_str()),
            ("owner_id", self.owner_id.as_str()),
            ("tracking_id", self.new_tracking_id.as_str()),
            ("source_id", self.new_source_id.as_str()),
            ("candidate_id", self.new_candidate_id.as_str()),
            ("imported_by", self.imported_by.as_str()),
        ]
    }
}

/// 导入结果。`created_*` 描述**这条命令首次执行时**建了什么；
/// `replayed=true` 说明本次是重放，没有再次写入——两者不矛盾，别混。
///
/// 落进回执的 `result_detail` 就是它的 JSON，重放时原样返回：
/// 重放要回答「那条命令当时做了什么」，不是「现在是什么样」。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImportOutcome {
    /// 落到跟踪项时才有。
    pub tracking_id: Option<String>,
    /// 来源成为身份时才有。
    pub source_id: Option<String>,
    /// 只有定位、进了候选时才有。
    pub candidate_id: Option<String>,
    pub created_tracking: bool,
    pub created_source: bool,
    pub created_candidate: bool,
    /// 本次是重放（没有写入）。**唯一**能区分「刚建好」与「早就有了」的字段。
    pub replayed: bool,
}

impl ImportOutcome {
    pub fn tracked(
        tracking_id: String,
        source_id: String,
        created_tracking: bool,
        created_source: bool,
    ) -> Self {
        Self {
            tracking_id: Some(tracking_id),
            source_id: Some(source_id),
            candidate_id: None,
            created_tracking,
            created_source,
            created_candidate: false,
            replayed: false,
        }
    }

    pub fn candidate(candidate_id: String, created_candidate: bool) -> Self {
        Self {
            tracking_id: None,
            source_id: None,
            candidate_id: Some(candidate_id),
            created_tracking: false,
            created_source: false,
            created_candidate,
            replayed: false,
        }
    }

    /// 从回执结果体还原一次重放。`replayed` 由调用方此刻的事实决定，不信存档里的值。
    ///
    /// 读不回来时返回原因而不是 `None`：调用方要把它报成「回执不可读」，
    /// 而不是悄悄当作首次执行。
    pub fn replayed_from(detail: &str) -> Result<Self, String> {
        let mut outcome: Self =
            serde_json::from_str(detail).map_err(|err| format!("结果体不是合法 JSON：{err}"))?;
        outcome.replayed = true;
        Ok(outcome)
    }
}

/// 关联结果。`created=false` 表示两边早就挂着，本次没动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedSource {
    pub tracking_id: String,
    pub created: bool,
}

// ---------------------------------------------------------------------------
// 渲染（`bod sources` 的文本输出）
// ---------------------------------------------------------------------------

/// 来源列表。只给身份坐标与修订条数：结论（哪一版是「当前」）不在这里下。
pub fn render_sources(rows: &[(SourceRow, i64)]) -> String {
    if rows.is_empty() {
        return "没有已确认的来源身份。\n".to_string();
    }
    let mut out = format!("已确认来源 {} 个：\n", rows.len());
    for (source, revision_count) in rows {
        let locator = if source.locator.trim().is_empty() {
            "无定位".to_string()
        } else {
            source.locator.clone()
        };
        out.push_str(&format!(
            "- {}  {}  修订 {}  {}\n",
            source.source_id,
            source.describe(),
            revision_count,
            locator
        ));
    }
    out
}

/// 候选列表。待确认的排在前面——它才是需要动作的那一类。
pub fn render_candidates(rows: &[CandidateRow]) -> String {
    if rows.is_empty() {
        return "没有来源候选。\n".to_string();
    }
    let mut out = format!("来源候选 {} 条：\n", rows.len());
    for candidate in rows {
        let object_type = if candidate.object_type.trim().is_empty() {
            "类型未定"
        } else {
            candidate.object_type.as_str()
        };
        out.push_str(&format!(
            "- {}  {}  {}  {}  {}\n",
            candidate.candidate_id,
            candidate.state.as_str(),
            object_type,
            candidate.locator,
            if candidate.proposed_by.trim().is_empty() {
                "提交人未记录"
            } else {
                candidate.proposed_by.as_str()
            }
        ));
    }
    if rows
        .iter()
        .any(|candidate| candidate.state == CandidateState::Pending)
    {
        out.push_str("\n待确认候选还不能支撑覆盖结论：它们没有稳定身份（§11）。\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_row() -> CandidateRow {
        CandidateRow {
            candidate_id: "cand-1".to_string(),
            company_id: "c1".to_string(),
            project_id: "p1".to_string(),
            connection_id: "gh-1".to_string(),
            object_type: String::new(),
            locator: "https://github.com/acme/web/pull/318".to_string(),
            proposed_by: "alice".to_string(),
            proposed_at_ms: 100,
            state: CandidateState::Pending,
            resolved_source_id: None,
            decided_by: String::new(),
            decided_at_ms: None,
        }
    }

    /// 跨项目同名 PR 是两个来源：五元组里 project 不同，身份就不同。
    #[test]
    fn same_number_in_two_projects_is_two_identities() {
        let a = resolve(
            "c1",
            "p1",
            "gh-1",
            Some("pull-request"),
            Some("PR_kw1"),
            "acme/web#318",
        )
        .expect("应解析成功");
        let b = resolve(
            "c1",
            "p2",
            "gh-1",
            Some("pull-request"),
            Some("PR_kw1"),
            "acme/api#318",
        )
        .expect("应解析成功");
        let (SourceResolution::Identity(a), SourceResolution::Identity(b)) = (a, b) else {
            panic!("两段都给了稳定 ID 与类型，应直接成为身份");
        };
        assert_ne!(a.describe(), b.describe());
        assert_eq!(a.provider_object_id, b.provider_object_id);
        assert_ne!(a.project_id, b.project_id);
    }

    /// 只有 URL：进候选，不造成身份。
    #[test]
    fn url_only_becomes_a_candidate() {
        let resolution = resolve(
            "c1",
            "p1",
            "gh-1",
            Some("pull-request"),
            None,
            "https://github.com/acme/web/pull/318",
        )
        .expect("应解析成功");
        match resolution {
            SourceResolution::Candidate(candidate) => {
                assert_eq!(candidate.locator, "https://github.com/acme/web/pull/318");
                assert_eq!(candidate.object_type, "pull-request");
            }
            other => panic!("只有 URL 不应成为身份：{other:?}"),
        }
    }

    /// 把 URL 填进 ID 字段：不能当身份，落到候选，且用那个 URL 当定位。
    #[test]
    fn url_in_the_id_field_is_not_an_identity() {
        let resolution = resolve(
            "c1",
            "p1",
            "gh-1",
            Some("pull-request"),
            Some("https://github.com/acme/web/pull/318"),
            "",
        )
        .expect("应解析成功");
        match resolution {
            SourceResolution::Candidate(candidate) => {
                assert_eq!(candidate.locator, "https://github.com/acme/web/pull/318");
            }
            other => panic!("URL 不是稳定 ID：{other:?}"),
        }
    }

    /// 给了 ID 却没给类型：报错，不静默降级成候选。
    #[test]
    fn id_without_object_type_is_an_error_not_a_candidate() {
        let err = resolve("c1", "p1", "gh-1", None, Some("PR_kw1"), "acme/web#318")
            .expect_err("缺类型必须报错");
        assert_eq!(err, SourceError::MissingObjectType);
    }

    /// 什么都没有：不是候选也不是身份，直接拒绝。
    #[test]
    fn nothing_to_resolve_is_rejected() {
        let err = resolve("c1", "p1", "gh-1", None, None, "  ").expect_err("空定位必须报错");
        assert_eq!(err, SourceError::EmptyLocator);
    }

    /// 对象类型必须 kebab-case：`PullRequest` 与 `pull-request` 不能变成两个身份。
    #[test]
    fn object_type_must_be_kebab_case() {
        let err = resolve(
            "c1",
            "p1",
            "",
            Some("PullRequest"),
            Some("PR_kw1"),
            "acme/web#318",
        )
        .expect_err("大小写混用必须拒绝");
        assert!(matches!(err, SourceError::InvalidObjectType(_)), "{err:?}");

        // 空连接实例是合法的：人工补证没有连接器实例。
        let identity = resolve("c1", "p1", "", Some("pull-request"), Some("PR_kw1"), "")
            .expect("无连接器也应能成为身份");
        match identity {
            SourceResolution::Identity(identity) => {
                assert!(identity.connection_id.is_empty());
                assert!(identity.describe().contains("no-connection"));
            }
            other => panic!("应成为身份：{other:?}"),
        }
    }

    /// 同一事件（版本与摘要都相同）指纹相同 → 只确认一次。
    #[test]
    fn same_event_shares_one_fingerprint() {
        let first = revision_fingerprint("v1.4.2", "sha256:aaa").expect("应生成指纹");
        let again = revision_fingerprint("v1.4.2", "sha256:aaa").expect("应生成指纹");
        assert_eq!(first, again);
        // 观察时间不参与指纹：否则重复上报会变成新修订。
        assert!(!first.contains("2026"));
    }

    /// 可变标签：同一标签两个摘要 → 两个指纹，两条修订都要留。
    #[test]
    fn same_tag_with_two_digests_is_two_revisions() {
        let first = revision_fingerprint("v1.4.2", "sha256:aaa").expect("应生成指纹");
        let second = revision_fingerprint("v1.4.2", "sha256:bbb").expect("应生成指纹");
        assert_ne!(first, second);
    }

    /// 版本与摘要都空：不能登记修订（空指纹会把不同事件合并）。
    #[test]
    fn revision_without_content_is_rejected() {
        assert_eq!(
            revision_fingerprint("  ", ""),
            Err(SourceError::RevisionWithoutContent)
        );
    }

    /// 指纹拼接不会被形如 `a` + `b` 的值伪造：分隔符不可打印。
    #[test]
    fn fingerprint_separator_cannot_be_forged() {
        let a = revision_fingerprint("a", "b").unwrap();
        let b = revision_fingerprint("ab", "").unwrap();
        assert_ne!(a, b);
    }

    /// 候选只能从 pending 出发一次；确认后再确认必须拒绝。
    #[test]
    fn candidate_can_only_be_decided_once() {
        assert_eq!(decide(CandidateState::Pending), Ok(CandidateState::Pending));
        assert!(decide(CandidateState::Confirmed).is_err());
        assert!(decide(CandidateState::Rejected).is_err());
        assert!(CandidateState::parse("maybe").is_err());
    }

    /// 确认候选时不得把候选挪到别的项目。
    #[test]
    fn confirming_cannot_move_the_candidate_to_another_project() {
        let candidate = candidate_row();
        let mut identity = SourceIdentity {
            company_id: "c1".to_string(),
            project_id: "p2".to_string(),
            connection_id: "gh-1".to_string(),
            object_type: "pull-request".to_string(),
            provider_object_id: "PR_kw1".to_string(),
        };
        let err = ensure_scope_matches(&candidate, &identity).expect_err("跨项目确认必须拒绝");
        assert!(matches!(err, SourceError::CandidateScopeMismatch { .. }));

        identity.project_id = "p1".to_string();
        assert!(ensure_scope_matches(&candidate, &identity).is_ok());
    }

    /// 时间线按观察时间排序，且不吞掉同一标签的旧摘要。
    #[test]
    fn digest_history_keeps_every_resolution_in_time_order() {
        let row = |id: &str, at: i64, digest: &str| SourceRevisionRow {
            source_revision_id: id.to_string(),
            company_id: "c1".to_string(),
            source_id: "s1".to_string(),
            version: "v1.4.2".to_string(),
            digest: digest.to_string(),
            content_fingerprint: format!("v1.4.2\u{1f}{digest}"),
            actor_id: "ci".to_string(),
            observed_at: format!("2026-11-21T10:00:{:02}Z", at % 60),
            observed_at_ms: at,
            recorded_at_ms: at,
        };
        // 故意乱序传入：时间线要自己排。
        let revisions = vec![row("r2", 200, "sha256:bbb"), row("r1", 100, "sha256:aaa")];
        let history = digest_history(&revisions);
        assert_eq!(
            history,
            vec![
                ("2026-11-21T10:00:40Z".to_string(), "sha256:aaa".to_string()),
                ("2026-11-21T10:00:20Z".to_string(), "sha256:bbb".to_string()),
            ]
        );
    }

    fn import_command() -> ImportSourceCommand {
        ImportSourceCommand {
            idempotency_key: "key-1".to_string(),
            company_id: "c1".to_string(),
            project_id: "p1".to_string(),
            connection_id: "gh-1".to_string(),
            object_type: Some("pull-request".to_string()),
            provider_object_id: Some("PR_kw318".to_string()),
            locator: "acme/web#318".to_string(),
            title: "登录偶发 500".to_string(),
            requirement: String::new(),
            owner_id: "alice".to_string(),
            new_tracking_id: "dt-1".to_string(),
            new_source_id: "src-1".to_string(),
            new_candidate_id: "cand-1".to_string(),
            imported_by: "alice".to_string(),
            correlation_id: "req-1".to_string(),
            decided_at: "2026-11-22T00:00:00Z".to_string(),
        }
    }

    /// 幂等键不进载荷指纹：进去了就等于「换键即换载荷」，同键异载荷永远撞不上。
    #[test]
    fn idempotency_key_is_not_part_of_the_payload() {
        let command = import_command();
        let baseline =
            crate::delivery_idempotency::payload_fingerprint(&command.payload_fields()).unwrap();
        let mut other = command.clone();
        other.idempotency_key = "key-2".to_string();
        assert_eq!(
            crate::delivery_idempotency::payload_fingerprint(&other.payload_fields()).unwrap(),
            baseline,
            "换键不改变载荷指纹"
        );
        // 反过来：换标题必须改变指纹，否则「改了标题重试」会被当成重放。
        let mut retitled = command.clone();
        retitled.title = "登录 500（改）".to_string();
        assert_ne!(
            crate::delivery_idempotency::payload_fingerprint(&retitled.payload_fields()).unwrap(),
            baseline
        );
    }

    /// 结果体可原样往返，重放标志由调用方此刻决定，不信存档里的值。
    #[test]
    fn outcome_round_trips_and_replay_flag_is_recomputed() {
        let outcome = ImportOutcome::tracked("dt-1".to_string(), "src-1".to_string(), true, true);
        let text = serde_json::to_string(&outcome).expect("应可序列化");
        let restored = ImportOutcome::replayed_from(&text).expect("应可反序列化");
        assert_eq!(restored.tracking_id.as_deref(), Some("dt-1"));
        assert_eq!(restored.source_id.as_deref(), Some("src-1"));
        // created_* 保留首次执行的事实，replayed 反映本次。
        assert!(restored.created_tracking && restored.created_source);
        assert!(restored.replayed);
        assert!(ImportOutcome::replayed_from("not json").is_err());
    }
}
