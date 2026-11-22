//! §11 交付跟踪与答复草稿的应用接口（P8 2.2：路径冻结）。
//!
//! 本模块只做两件事：**声明 6 个操作的路由与契约**，以及给出它们的请求/响应类型。
//! 业务实现不在这里——`not_implemented` 是当前唯一 handler，按契约如实返回 501。
//!
//! 三条必须写下来的判断：
//!
//! 1. **单独一张路由表，不并入 `/_bod/*`**。控制面的凭据是本机会话密钥，只证明
//!    「同一个用户在本机」；§11 的操作要求按 §34.12 的**主体与目标**判定权限。
//!    混在一张表里，文档就会把两种鉴权写成同一种，客户端无从判断。
//! 2. **冻结期间回 501，不回 200 空对象**。路径冻结的目的是让客户端先按稳定路径
//!    生成代码；回一个空对象会让调用方以为交付登记成功了——那是把未实现伪装成事实。
//! 3. **`x-bod-status=planned`**。契约里每条计划中的操作都带这个标记，实现落地时
//!    必须把它改成 `served` 并删掉 501 响应，测试会挡住「悄悄当已实现」。
//!
//! 当前鉴权只到**会话级**：`Auth::Bearer(BOD_API_DENIED)` 校验的是本机会话密钥，
//! §34.12 的主体/目标判定尚未实现。这一句同时写在每条路由的 `note` 里，
//! 因为「文档比事实更强」是本仓库明确禁止的失败方式。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::host_api::ErrorResponse;
use crate::host_handshake::HostServer;
use crate::http_router::{
    Auth, Contract, ContractBody, ContractResponse, EndpointRequest, Method, Reply, Route,
    RouteError,
};

/// 产品接口前缀。分发层按它选择路由表（`host_handshake::handle`），
/// 所以注入式的前缀表与路由表必须一致——`openapi_contract` 有测试守这一条。
pub const PREFIX: &str = "/api/v1";

/// 每条计划中路由统一用在 401 上的拒绝码。
///
/// 名字只说「API 拒绝」，不说「范围不足」：字符串由 `RecordRejection` 派生（单一来源），
/// 但路由**只用第一档**——§8.1 的可见性与动作判定尚未接线，此时回 `BOD_SCOPE_DENIED`
/// 会把未实现的判定说成已实现的判定。三档码齐备而只用一档，是当前实现进度的如实表述。
pub const DENIED_CODE: &str = crate::authorization::RecordRejection::ApiDenied.as_code();

/// 计划中操作的统一 501 码。
pub const NOT_IMPLEMENTED_CODE: &str = "BOD_NOT_IMPLEMENTED";

/// 未实现的 handler：按契约返回 501，不读请求体、不碰状态。
///
/// 刻意不做「解析一半再失败」：请求体尚未被任何实现校验过，报 422 等于
/// 声称这份 schema 已经生效。
fn not_implemented(_state: &HostServer, _endpoint: EndpointRequest) -> Result<Reply, RouteError> {
    Err(RouteError::new(501, NOT_IMPLEMENTED_CODE))
}

/// 合同里共用的计划响应尾巴：200 之后必然是 501。
macro_rules! planned_responses {
    ($description:expr, $component:literal, $schema:path) => {
        &[
            ContractResponse::new(
                200,
                $description,
                Some(ContractBody::json($component, $schema)),
            ),
            ContractResponse::new(
                501,
                "契约已冻结、实现未落地：本条请求不会被受理，也不会产生任何写入；错误码固定为 \
                 BOD_NOT_IMPLEMENTED",
                Some(ContractBody::json(
                    "ErrorResponse",
                    crate::openapi::schema_of::<ErrorResponse>,
                )),
            ),
        ]
    };
}

/// §11 的 6 个操作。顺序与规格 §11 接口表一致，便于逐行对照。
static ROUTES: &[Route<HostServer>] = &[
    Route {
        method: Method::Post,
        pattern: "/api/v1/delivery-tracking/imports",
        auth: Auth::Bearer(DENIED_CODE),
        note: "只校验本机会话密钥；§34.12 的主体与目标判定未实现。导入权限与发布执行权限分开：\
               凭此不能部署、不能通知客户。同幂等键不同载荷回 409，重放返回原跟踪项",
        contract: Contract {
            summary: "导入外部来源并建立交付跟踪项",
            request: Some(ContractBody::json(
                "DeliveryImportRequest",
                crate::openapi::schema_of::<DeliveryImportRequest>,
            )),
            query: &[],
            responses: planned_responses!(
                "已建立（或幂等重放命中）的跟踪项：稳定 ID、已关联目标与缺口；`created` 区分首次与重放",
                "DeliveryImportResponse",
                crate::openapi::schema_of::<DeliveryImportResponse>
            ),
            status: crate::http_router::Implementation::Planned,
        },
        handler: not_implemented,
    },
    Route {
        method: Method::Post,
        pattern: "/api/v1/delivery-tracking/{id}/links",
        auth: Auth::Bearer(DENIED_CODE),
        note: "只校验本机会话密钥；§34.12 未实现。追加来源或内部工作包关系，按 expected_version 防覆盖；\
               重复提交返回已有关系而不是第二条",
        contract: Contract {
            summary: "给跟踪项追加来源或内部工作包关联",
            request: Some(ContractBody::json(
                "DeliveryLinkRequest",
                crate::openapi::schema_of::<DeliveryLinkRequest>,
            )),
            query: &[],
            responses: planned_responses!(
                "追加结果：关系 ID、是否新建与追加后的版本；重复提交时 `appended=false`",
                "DeliveryLinkResponse",
                crate::openapi::schema_of::<DeliveryLinkResponse>
            ),
            status: crate::http_router::Implementation::Planned,
        },
        handler: not_implemented,
    },
    Route {
        method: Method::Get,
        pattern: "/api/v1/delivery-tracking/{id}",
        auth: Auth::Bearer(DENIED_CODE),
        note: "只校验本机会话密钥；§34.12 未实现。读取按授权目标过滤：对调用者不可见的客户不出现在 targets 里，\
               不是返回空结论",
        contract: Contract {
            summary: "读取跟踪项的覆盖结论与允许动作",
            request: None,
            query: &[],
            responses: planned_responses!(
                "覆盖结论、证据版本与来源时间、缺口、负责人与允许动作",
                "DeliveryTrackingView",
                crate::openapi::schema_of::<DeliveryTrackingView>
            ),
            status: crate::http_router::Implementation::Planned,
        },
        handler: not_implemented,
    },
    Route {
        method: Method::Post,
        pattern: "/api/v1/delivery-observations",
        auth: Auth::Bearer(DENIED_CODE),
        note: "只校验本机会话密钥；§34.12 未实现。与 §34.10 的同一路径合并为一处契约：\
               按来源序列处理乱序，重复事件只确认一次；只读接入凭据可登记观察但不可部署或通知",
        contract: Contract {
            summary: "接收部署、回滚或人工补证观察",
            request: Some(ContractBody::json(
                "DeliveryObservationRequest",
                crate::openapi::schema_of::<DeliveryObservationRequest>,
            )),
            query: &[],
            responses: planned_responses!(
                "观察登记结果与受影响目标的新结论；乱序事件按来源序列落位，不回退已确认结论",
                "DeliveryObservationResponse",
                crate::openapi::schema_of::<DeliveryObservationResponse>
            ),
            status: crate::http_router::Implementation::Planned,
        },
        handler: not_implemented,
    },
    Route {
        method: Method::Post,
        pattern: "/api/v1/feedback-receipts/{id}/drafts",
        auth: Auth::Bearer(DENIED_CODE),
        note: "只校验本机会话密钥；§34.12 未实现。保存草稿**不产生任何出站动作**：\
               要实现时不得在此路径上调用渠道",
        contract: Contract {
            summary: "为客户目标装配答复草稿（不外发）",
            request: Some(ContractBody::json(
                "DraftCreateRequest",
                crate::openapi::schema_of::<DraftCreateRequest>,
            )),
            query: &[],
            responses: planned_responses!(
                "草稿修订、所依据的证据修订与逐项引用；状态为 draft 或 pending-review",
                "DraftResponse",
                crate::openapi::schema_of::<DraftResponse>
            ),
            status: crate::http_router::Implementation::Planned,
        },
        handler: not_implemented,
    },
    Route {
        method: Method::Post,
        pattern: "/api/v1/feedback-receipts/{id}/drafts/{draft_id}/send",
        auth: Auth::Bearer(DENIED_CODE),
        note: "只校验本机会话密钥；§34.12 未实现。发送必须在命令受理点与出站点各重验一次；\
               证据失效回 409 并给出可见差异。受理成功只表示意图已登记，渠道结果另查",
        contract: Contract {
            summary: "提交草稿发送意图（渠道结果另查）",
            request: Some(ContractBody::json(
                "DraftSendRequest",
                crate::openapi::schema_of::<DraftSendRequest>,
            )),
            query: &[],
            responses: planned_responses!(
                "已登记的发送意图；`delivery_status` 只能是 unknown——提交成功不等于送达",
                "DraftSendResponse",
                crate::openapi::schema_of::<DraftSendResponse>
            ),
            status: crate::http_router::Implementation::Planned,
        },
        handler: not_implemented,
    },
];

/// 产品接口的路由表只读视图。分发层用它处理 `/api/v1` 前缀的请求。
pub fn routes() -> &'static [Route<HostServer>] {
    ROUTES
}

// ---------------------------------------------------------------------------
// 允许动作 ↔ 路由
// ---------------------------------------------------------------------------

/// `GET /delivery-tracking/{id}` 的 `allowed_actions` 里可能出现的动作码。
///
/// 动作码与路由绑在一张表里，是为了让「视图说能做」与「路径真的能做」不能分家：
/// 读模型只列 `is_served_action(code) == Some(true)` 的动作。
pub const ACTION_LINK_SOURCE: &str = "link-source";
pub const ACTION_DRAFT: &str = "draft";

/// 动作码 → 承载它的路由。`send` 暂不在此列：草稿能审完之前它连候选都不该出现。
const ACTIONS: &[(&str, Method, &str)] = &[
    (
        ACTION_LINK_SOURCE,
        Method::Post,
        "/api/v1/delivery-tracking/{id}/links",
    ),
    (
        ACTION_DRAFT,
        Method::Post,
        "/api/v1/feedback-receipts/{id}/drafts",
    ),
];

/// 该动作码是否已有实现（`x-bod-status=served`）。
///
/// `None` 表示动作码没有登记路由——拼错的码不该被当成「不允许」而静默通过，
/// 调用方（与测试）应当把它当错误处理。
pub fn is_served_action(code: &str) -> Option<bool> {
    let (_, method, pattern) = ACTIONS.iter().find(|(name, _, _)| *name == code)?;
    let served = ROUTES.iter().any(|route| {
        route.method == *method
            && route.pattern == *pattern
            && matches!(
                route.contract.status,
                crate::http_router::Implementation::Served
            )
    });
    Some(served)
}

// ---------------------------------------------------------------------------
// 共用值对象
// ---------------------------------------------------------------------------

/// 客户目标：客户 × 环境 ×（可选）部署单元。
///
/// 没有 `company_id`：Company 由主体解析得出，不由请求体声明——请求体声明归属
/// 等于允许调用者指定别人的公司。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetRef {
    /// 客户标识。同名环境分属不同客户时靠它区分。
    pub customer_id: String,
    /// 目标环境标识（客户侧命名，不要求全局唯一）。
    pub environment: String,
    /// 部署单元/客户组；缺省表示整个环境。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_unit: Option<String>,
}

/// 外部来源引用，对应来源身份五元组中「连接实例 + 对象类型 + 稳定对象 ID」三项。
///
/// Company 与 Project 不在结构里：前者由主体解析，后者由请求顶层 `project_id` 给出。
/// `object_id` 缺省即**候选**——只有 URL 或纯文本时不能成为身份，须负责人确认。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    /// 连接实例引用。凭据只存在连接实例里，请求体**不得**携带令牌。
    pub connection_id: String,
    /// 来源对象类型：`pull_request` / `commit` / `build` / `label` / `manual`。
    pub object_type: String,
    /// 提供方稳定对象 ID（如 PR 的内部 ID 而非编号）。缺省表示进入候选。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
    /// 人类可读地址。仅用于候选展示与人工确认，**不参与身份判定**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// 内容摘要。可变标签每次解析都记录当次摘要，不覆盖前一次。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// 原始观察时间（RFC 3339）。这是来源修订的时间，不是入库时间。
    pub observed_at: String,
}

/// 制品引用。只有摘要、没有修订时也能登记——但覆盖结论会因此停在待补证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// 制品修订 ID（`artifact_revisions.revision_id`）。缺省表示尚未建立修订。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_revision: Option<String>,
    /// 制品内容摘要；与修订同时给出时必须一致。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// 覆盖结论。取值与 `coverage_receipt::CoverageVerdict` 一一对应——
/// 这里只做线上表示，不做第二套判定。
///
/// 三个取值的含义：`covered` 部署已观察 + 验证通过 + 开关就绪；
/// `pending-verification` 已提供但缺部署观察或验证结论；
/// `pending-install` 缺安装回执（离线安装、人群未知）。
///
/// 变体上不写注释：`schemars` 会把「带注释的变体」编译成 `oneOf` + `const`，
/// 客户端生成器便得到 union 而不是 enum。说明统一写在类型注释里，
/// schema 保持为朴素字符串枚举（ADR-0004 的撤销条件因此仍然不成立）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageVerdictCode {
    Covered,
    PendingVerification,
    PendingInstall,
}

/// 单个目标上的覆盖状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetCoverage {
    pub target: TargetRef,
    pub verdict: CoverageVerdictCode,
    /// 不能给结论时的原因。`covered` 时不出现——给成功补一句原因会让人以为有条件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 该结论所依据的证据修订；缺省表示尚无证据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_revision: Option<String>,
    /// 该证据的观察时间（RFC 3339）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

/// 一条证据的时间戳与来源。GET 用它说明「结论是按哪一版证据得出的」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceStamp {
    pub revision: String,
    /// 来源类别：`connector` / `manual` / `internal`。
    pub source: String,
    pub observed_at: String,
}

// ---------------------------------------------------------------------------
// POST /api/v1/delivery-tracking/imports
// ---------------------------------------------------------------------------

/// 外部导入请求。五项输入与规格 §11 接口表逐项对应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryImportRequest {
    pub project_id: String,
    pub source: SourceRef,
    /// 本次导入覆盖的客户目标。至少一项：没有目标的导入无法回答「对谁交付了什么」。
    pub targets: Vec<TargetRef>,
    /// 验收条件（规则修订或人可读条款）。空串表示尚未提供，覆盖结论必然停在待核实。
    pub acceptance: String,
    /// 命令幂等键。同键不同载荷回 409；与来源身份是两个概念，见 design D2。
    pub idempotency_key: String,
}

/// 导入结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryImportResponse {
    /// 稳定跟踪 ID。重复导入同一来源时返回同一个 ID。
    pub tracking_id: String,
    /// `true` 表示本次新建；`false` 表示幂等重放或来源已登记。
    pub created: bool,
    pub targets: Vec<TargetCoverage>,
    /// 缺口码（稳定字符串，不随语言变化）。
    pub gaps: Vec<String>,
}

// ---------------------------------------------------------------------------
// POST /api/v1/delivery-tracking/{id}/links
// ---------------------------------------------------------------------------

/// 追加关联请求。
///
/// `source` 与 `work_package_revision` **恰好给一个**。不用 `oneOf` 表达：
/// OAS 外层由本仓维护，多态请求体会让客户端生成器与校验器分叉（ADR-0004）。
/// 该约束由实现校验并在 422 里说明，不在 schema 里假装已经表达。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryLinkRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRef>,
    /// 后来补入的内部工作包修订；只追加关联，不建 Attempt。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_package_revision: Option<String>,
    pub targets: Vec<TargetRef>,
    /// 期望的跟踪项版本；不一致回 409 并带当前版本。
    pub expected_version: u64,
    pub idempotency_key: String,
}

/// 追加结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryLinkResponse {
    pub tracking_id: String,
    pub link_id: String,
    /// `false` 表示关系已存在（重复提交），本次没有新增。
    pub appended: bool,
    /// 追加后的跟踪项版本。
    pub version: u64,
}

// ---------------------------------------------------------------------------
// GET /api/v1/delivery-tracking/{id}
// ---------------------------------------------------------------------------

/// 跟踪项读取视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryTrackingView {
    pub tracking_id: String,
    pub project_id: String,
    /// 当前版本；调用方追加关联时应把它作为 `expected_version`。
    pub version: u64,
    /// 负责人。未指派时字段缺省——不用空串冒充「有人负责」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub targets: Vec<TargetCoverage>,
    pub evidence: Vec<EvidenceStamp>,
    pub gaps: Vec<String>,
    /// 允许动作的稳定码（如 `link-source`、`draft`、`send`）。
    /// 这是服务端判定结果的投影，客户端不得据此自行放宽。
    pub allowed_actions: Vec<String>,
}

// ---------------------------------------------------------------------------
// POST /api/v1/delivery-observations
// ---------------------------------------------------------------------------

/// 观察类别。回滚是**新的观察**，不是把部署观察改成失败。
///
/// `manual-evidence` 指授权实施人提交的补证（离线安装、现场核对）；
/// 与连接器证据冲突时两份都保留，并进待核实。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationKind {
    Deployment,
    Rollback,
    ManualEvidence,
}

/// 观察入站请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryObservationRequest {
    pub project_id: String,
    pub targets: Vec<TargetRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
    /// 来源事件。`observed_at` 即本次观察时间；`source_sequence` 用于乱序重排。
    pub source: SourceRef,
    /// 来源内的单调序号，从 1 开始。重复序号回 409，不回 200。
    pub source_sequence: u64,
    pub kind: ObservationKind,
    /// 观察正文或说明；人工补证必须写清授权范围与复验依据。
    pub note: String,
    pub idempotency_key: String,
}

/// 观察登记结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryObservationResponse {
    pub observation_id: String,
    /// `false` 表示该序号已处理过（重复事件），本次没有改变任何结论。
    pub applied: bool,
    pub affected_targets: Vec<TargetCoverage>,
}

// ---------------------------------------------------------------------------
// POST /api/v1/feedback-receipts/{id}/drafts
// ---------------------------------------------------------------------------

/// 草稿装配请求。保存**不产生任何出站副作用**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftCreateRequest {
    pub target: TargetRef,
    /// 答复用途（如 `fixed-notice`、`provided-notice`、`correction`）。
    pub purpose: String,
    /// 所依据的证据修订。证据变化后这份草稿即失效，需重审。
    pub evidence_revision: String,
    /// 逐项引用的证据或回执引用。
    pub references: Vec<String>,
    /// 已知缺口；有缺口时草稿不得声称已修复。
    pub gaps: Vec<String>,
    /// 人工编辑的正文；空串表示按规则摘要装配（无模型路径）。
    pub text: String,
}

/// 草稿状态。只有两种：可编辑与待重审。发送不是状态，是另一个操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DraftStatus {
    Draft,
    PendingReview,
}

/// 草稿里的一条引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftCitation {
    pub evidence_revision: String,
    /// 引用本身的定位（回执 ID、来源对象 ID、链接）。
    pub reference: String,
    pub observed_at: String,
}

/// 草稿装配结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftResponse {
    pub draft_id: String,
    /// 草稿修订号。每次保存 +1，发送时必须回传已审的那一版。
    pub revision: u64,
    pub evidence_revision: String,
    pub citations: Vec<DraftCitation>,
    pub status: DraftStatus,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// POST /api/v1/feedback-receipts/{id}/drafts/{draft_id}/send
// ---------------------------------------------------------------------------

/// 发送意图请求。回传已审的文本修订与证据修订，两者任一过期即 409。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftSendRequest {
    /// 已审的草稿修订号。
    pub text_revision: u64,
    /// 已审的证据修订；与当前证据不一致时不得发出。
    pub evidence_revision: String,
    /// 目标通道标识（连接实例或渠道账号引用），不是令牌。
    pub target_channel: String,
    pub idempotency_key: String,
}

/// 渠道送达状态。
///
/// 只有一个取值是刻意的：受理成功只表示**意图已登记**。渠道是否送达要另查，
/// 把「已提交」写成 `delivered` 或把「回包丢失」写成 `failed` 都是臆断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryStatus {
    Unknown,
}

/// 发送受理结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftSendResponse {
    /// 发送意图 ID（`outbox_deliveries` 侧的稳定键）。用它去对账，不要重发。
    pub send_intent_id: String,
    /// 恒为 `true`：走到这个响应就说明意图已登记。
    pub registered: bool,
    pub delivery_status: DeliveryStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路由表必须覆盖规格 §11 的 6 个操作，且路径都在产品前缀下。
    #[test]
    fn every_route_sits_under_the_product_prefix() {
        assert_eq!(routes().len(), 6, "§11 的 6 个操作要一条不少");
        for route in routes() {
            assert!(
                route.pattern.starts_with(PREFIX),
                "{} 不在 {PREFIX} 前缀下，分发层会把它交给控制面路由表",
                route.pattern
            );
            assert!(
                !route.note.trim().is_empty(),
                "{} 缺鉴权理由",
                route.pattern
            );
        }
    }

    /// 计划中操作的 handler 只能是 `not_implemented`，且必须声明 501。
    /// 这条是「不许把未实现悄悄当已实现」的机械保障。
    #[test]
    fn planned_routes_point_at_the_unimplemented_handler() {
        let unimplemented: crate::http_router::Handler<HostServer> = not_implemented;
        for route in routes() {
            assert!(
                std::ptr::fn_addr_eq(route.handler, unimplemented),
                "{} 已有实现，应从 planned 改为 served 并更新测试",
                route.pattern
            );
            let statuses: Vec<u16> = route
                .contract
                .responses
                .iter()
                .map(|response| response.status)
                .collect();
            assert!(
                statuses.contains(&501),
                "{} 未声明 501：计划中操作必须如实说明不会被受理",
                route.pattern
            );
            assert!(
                !statuses.contains(&401) && !statuses.contains(&404),
                "{} 手写了 401/404；这两个码由 Auth 与分发层生成",
                route.pattern
            );
        }
    }

    /// 唯一可能的送达状态就是 unknown：schema 自己把「不许推断成功」写死。
    #[test]
    fn delivery_status_cannot_claim_success() {
        let rendered = serde_json::to_value(DraftStatus::Draft).expect("应可序列化");
        assert_eq!(rendered, serde_json::json!("draft"));
        let unknown = serde_json::to_value(DeliveryStatus::Unknown).expect("应可序列化");
        assert_eq!(unknown, serde_json::json!("unknown"));
    }

    /// 动作码必须对得上真实路由：码与路径分家，视图就会推荐一个 404。
    #[test]
    fn action_codes_point_at_real_routes() {
        for (code, method, pattern) in ACTIONS {
            let found = routes()
                .iter()
                .filter(|route| route.method == *method && route.pattern == *pattern)
                .count();
            assert_eq!(found, 1, "动作 {code} 指向的路由 {pattern} 不存在或不唯一");
            assert_eq!(
                is_served_action(code),
                Some(false),
                "{code} 当前应为 planned"
            );
        }
        assert_eq!(
            is_served_action("send"),
            None,
            "未登记的动作码必须返回 None"
        );
        assert_eq!(
            is_served_action("link-soruce"),
            None,
            "拼错的码不得当成允许"
        );
    }

    /// 请求体拒绝未知字段：字段拼错时应当是 4xx 而不是被静默忽略。
    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_value::<DeliveryImportRequest>(serde_json::json!({
            "project_id": "p1",
            "source": {
                "connection_id": "c1",
                "object_type": "pull_request",
                "object_id": "4711",
                "observed_at": "2026-11-21T00:00:00Z"
            },
            "targets": [],
            "acceptance": "",
            "idempotency_key": "k1",
            "tenant": "other-company"
        }))
        .expect_err("未知字段必须被拒绝");
        assert!(err.to_string().contains("tenant"), "{err}");
    }
}
