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
use crate::store::facts::StageFactsError;

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

// ---------------------------------------------------------------------------
// 读端口与请求主体（P8 3.6 B3 / B5）
// ---------------------------------------------------------------------------

/// 产品接口的请求主体：**谁**，以及**在哪个公司**。
///
/// 两者绑在一起是有意的。`Principal`（`bod-core`）只回答「是谁」，不带公司——
/// 主体与公司的关系是部署事实，不是主体定义的一部分。而装配 `StageFacts` 需要
/// `company_id`，接口层又不能从请求体里取（「Company 由主体解析得出，不由请求体声明」），
/// 所以这项对应由宿主在装配时给出。
///
/// **尚未决定的是「谁」从哪来**（会话 → 成员映射 / 宿主配置 / 请求头自报）。
/// 本结构不预设答案：宿主拿到主体后传进来，接口层只负责用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryActor {
    pub company_id: String,
    pub principal: crate::authorization::Principal,
}

/// 交付跟踪的只读端口：宿主注入，接口层只依赖这个 trait，不依赖 `SqliteStore`。
///
/// 三条约束：
/// - **只读**：这两个方法都不写库、不产生审计、不派发。
/// - **失败不降级**：读不出来返回 `StageFactsError`（503 / `BOD_FACTS_UNAVAILABLE`），
///   不得退回空事实。空事实会让每个请求都 `BOD_NOT_FOUND`，与「对象真不存在」不可区分。
/// - **一次请求一份**：调用方在单次请求内只调一次 `stage_facts`，不要放进循环。
pub trait TrackingRead: Send + Sync {
    /// 按主体装配授权事实（库侧实现见 `store::facts`）。
    fn stage_facts(
        &self,
        company_id: &str,
        principal: &crate::authorization::Principal,
    ) -> Result<crate::authorization::StageFacts, StageFactsError>;

    /// 读一条跟踪项的完整视图。`None` 表示这一条不存在（或不属于该公司）。
    fn tracking_view(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<Option<crate::delivery_tracking::TrackingView>, StageFactsError>;
}

/// 未实现的 handler：按契约返回 501，不读请求体、不碰状态。
///
/// 刻意不做「解析一半再失败」：请求体尚未被任何实现校验过，报 422 等于
/// 声称这份 schema 已经生效。
fn not_implemented(_state: &HostServer, _endpoint: EndpointRequest) -> Result<Reply, RouteError> {
    Err(RouteError::new(501, NOT_IMPLEMENTED_CODE))
}

/// 拒绝 → 路由错误。状态码与码都从 `RecordRejection` 派生，handler 里不出现第二份字面量。
fn deny(rejection: crate::authorization::RecordRejection) -> RouteError {
    RouteError::new(rejection.http_status(), rejection.as_code())
}

/// `GET /api/v1/delivery-tracking/{id}`（P8 3.6 B5）：真实实现。
///
/// 顺序即 §8.1：**主体 → 立场 → 目标粒度**。主体必须在对象之前判完：
/// 没配主体的宿主，对任意 id 都回同一档码，返回码不携带「这个 id 存不存在」。
///
/// 跟踪项与客户目标是两个粒度，这里刻意分两级判（理由见 `container_visible`）。
fn read_tracking(server: &HostServer, endpoint: EndpointRequest) -> Result<Reply, RouteError> {
    use crate::authorization::RecordRejection;

    // 阶段一：主体确认。会话密钥已由 `Auth::Bearer` 验过，但「认证通过」不等于
    // 「知道是谁」——宿主要么在装配时指定了操作者，要么这台机器上没有可归属的主体。
    // 两种缺失走同一档码：装配没完成是宿主的问题，不该用 403 写成「你没权限」。
    let Some(actor) = server.delivery_actor() else {
        return Err(deny(RecordRejection::ApiDenied));
    };
    let Some(reader) = server.tracking_read() else {
        return Err(deny(RecordRejection::ApiDenied));
    };
    let Some(tracking_id) = endpoint.param("id") else {
        return Err(deny(RecordRejection::NotFound));
    };

    let dto = evaluate_read(actor, reader, tracking_id)?;
    let value =
        serde_json::to_value(&dto).map_err(|_| RouteError::new(500, "BOD_TRACKING_UNREADABLE"))?;
    Ok(Reply::ok(value))
}

/// `GET /delivery-tracking/{id}` 的判定与投影，与传输层无关。
///
/// 单独一个函数有两个好处：用例可以直接驱动它（不必 bind 一个真端口），
/// 以及「判定顺序」这件事被固定在一处——顺序错了就是这里错了。
///
/// 顺序即 §8.1：**主体 → 立场 → 目标粒度**。主体必须在对象之前判完：
/// 没配主体的宿主，对任意 id 都回同一档码，返回码不携带「这个 id 存不存在」。
///
/// 跟踪项与客户目标是两个粒度，这里刻意分两级判（理由见 `ContainerRead`）。
fn evaluate_read(
    actor: &DeliveryActor,
    reader: &dyn TrackingRead,
    tracking_id: &str,
) -> Result<DeliveryTrackingView, RouteError> {
    use crate::authorization::{ContainerRead, RecordRejection, ScopedRecord};
    use crate::domain::{DomainId, Scope};

    // 一次请求只装配一次事实。装在循环里会让成本随目标数线性增长。
    let facts = reader
        .stage_facts(&actor.company_id, &actor.principal)
        .map_err(|err| {
            tracing::warn!(
                target: "bod.delivery_api",
                detail = %err.explain(),
                "授权事实装配失败；按 503 拒绝，不降级为空事实"
            );
            RouteError::new(err.http_status(), err.as_code())
        })?;

    let Some(mut view) = reader
        .tracking_view(&actor.company_id, tracking_id)
        .map_err(|err| RouteError::new(err.http_status(), err.as_code()))?
    else {
        return Err(deny(RecordRejection::NotFound));
    };

    // 项目标识算一次并留着：动作判定也要用它（「关联来源」「起草」都要求项目成员）。
    let project = DomainId::new(view.tracking.project_id.as_str());
    let scope = Scope {
        company_id: DomainId::new(actor.company_id.as_str()),
        project_id: Some(project.clone()),
        issue_id: None,
    };
    let mut record = ScopedRecord::new(view.tracking.tracking_id.as_str(), scope);
    // 空负责人不是主体。`assigned_to("")` 会让空串与空串相等，等于给「没有负责人」
    // 的记录配了一个人人可当的指派身份。
    if !view.tracking.owner_id.trim().is_empty() {
        record = record.assigned_to(DomainId::new(view.tracking.owner_id.as_str()));
    }

    // 阶段二：这条容器能不能看。三档结果见 `ContainerRead`：内部成员整体可见，
    // 客户协作者按目标放行，其余不可见。**判定规则只在 `authorization` 里一份**，
    // 这里不自己查客户绑定——否则「谁能看容器」会有两个说法，且没人知道哪个是准的。
    let container = crate::authorization::read_container(&facts, &actor.principal, &record);
    if container == ContainerRead::Halted {
        return Err(deny(RecordRejection::NotFound));
    }

    // 阶段四：目标粒度。客户协作者只见本客户的目标；被排除的目标**不出现**，
    // 而不是留一个「无结论」的空位——空位等于承认这些目标存在。
    if container == ContainerRead::ByTarget {
        view.targets.retain(|target| {
            crate::authorization::target_visible(
                &facts,
                &actor.principal,
                Some(target.target.customer_id.as_str()),
            )
        });
        // 证据时间线按目标归属收窄。`EvidenceStamp` 不带 target_id（契约形状在 2.2 冻结），
        // 所以这里只能保留「被保留下来的结论所依据的那几版证据」——宁可少给，
        // 不把别的客户的证据修订一起送出去。
        let kept: std::collections::BTreeSet<&str> = view
            .targets
            .iter()
            .filter_map(|target| target.evidence_revision.as_deref())
            .collect();
        view.evidence
            .retain(|stamp| kept.contains(stamp.revision.as_str()));

        // 一条都看不到时必须与「文档不存在」同答。回 200 空 targets 会泄漏
        // 「这条记录确实存在，只是里面没有你的目标」。
        if view.targets.is_empty() {
            return Err(deny(RecordRejection::NotFound));
        }
    }

    // 允许动作：状态上说得通（候选） + 这个面上有入口 + 需求已登记 + 当事人满足。
    // 四件事分开判，任何一件不成立就不列——「视图说能做」不能比「真能做」宽。
    let actions = select_allowed(
        &view.action_candidates(),
        &available_on(ActionSurface::Http),
        &facts,
        &actor.principal,
        Some(&project),
    );
    view.to_dto(actions)
        .map_err(|_| RouteError::new(500, "BOD_TRACKING_UNREADABLE"))
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
        note: "会话密钥 + §34.12 主体/目标判定（P8 3.6 B5 已接线）。判定顺序：主体 → 记录立场 → \
               目标粒度；未配主体的宿主一律 BOD_API_DENIED，对任何 id 同答。读取按授权目标过滤：\
               对调用者不可见的客户不出现在 targets 里，一条都不可见时回 BOD_NOT_FOUND（与不存在同答）。\
               事实装配失败回 503 BOD_FACTS_UNAVAILABLE，不降级为空事实",
        contract: Contract {
            summary: "读取跟踪项的覆盖结论与允许动作",
            request: None,
            query: &[],
            responses: &[ContractResponse::new(
                200,
                "覆盖结论、证据版本与来源时间、缺口、负责人与允许动作",
                Some(ContractBody::json(
                    "DeliveryTrackingView",
                    crate::openapi::schema_of::<DeliveryTrackingView>,
                )),
            )],
            status: crate::http_router::Implementation::Served,
        },
        handler: read_tracking,
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
/// 一个动作「能不能做」由三件事同时决定，缺一不列：
/// 在这个**调用面**上有入口（`available_on`）、需求已登记（`action_requirement`）、
/// 当事人满足需求（`authorization::requirement_met`）。
/// 之前只看「路由是否 served」，于是 `allowed_actions` 恒为空而契约还写着「允许动作」——
/// 字段与承诺不符。三件事分开记，是为了让「视图说能做」和「这个主体真能做」不能分家。
pub const ACTION_LINK_SOURCE: &str = "link-source";
pub const ACTION_DRAFT: &str = "draft";

/// 调用面。同一个动作码在两个面上能不能做是不同的：HTTP 看路由是否 `served`，
/// CLI 看有没有对应子命令。合成一个布尔，就等于把两者都说成「已支持」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionSurface {
    Http,
    Cli,
}

/// CLI 入口表：动作码 → 承载它的子命令。
///
/// **空表不是忘了填**：ADR-0009 选定「写入只走 CLI」，但第一条写命令
/// （`bod tracking import`）还没落地，所以现在确实一个入口都没有。
/// 加条目时必须同时把读命令的主体事实装配上（`store.stage_facts`），
/// 否则 `select_allowed` 拿不到事实、一律不列——用例
/// `cli_surface_stays_unwired_until_facts_are_assembled` 会先红。
pub const CLI_ENTRIES: &[(&str, &str)] = &[];

/// 该调用面上真有入口的动作码。
pub fn available_on(surface: ActionSurface) -> Vec<&'static str> {
    match surface {
        ActionSurface::Http => ACTIONS
            .iter()
            .filter(|(code, _, _)| is_served_action(code) == Some(true))
            .map(|(code, _, _)| *code)
            .collect(),
        ActionSurface::Cli => CLI_ENTRIES.iter().map(|(code, _)| *code).collect(),
    }
}

/// 动作码需要什么权限。**没登记的动作码永远不列**（fail-closed）：
/// 漏登记的表现是「少显示一个动作」，而不是「对所有人都放行」。
pub fn action_requirement(code: &str) -> Option<crate::authorization::StageRequirement> {
    use crate::authorization::StageRequirement;
    match code {
        // 关联来源与起草都是项目成员的日常职责：都不产生对外后果（起草不外发），
        // 因此不需要批准权或发布权。`send` 将来进入候选时必须给更强的需求，
        // 不得沿用这一条——它有对外后果。
        ACTION_LINK_SOURCE | ACTION_DRAFT => Some(StageRequirement::Membership),
        _ => None,
    }
}

/// 允许动作的选择：候选 ∩ 该面有入口 ∩ 需求已登记 ∩ 当事人满足需求。
///
/// `entries` 由调用方传入（生产路径传 `available_on(surface)`，用例传构造的切片），
/// 这样「入口」这件事只有一个实现，而规则本身可以脱离路由表被测试。
pub fn select_allowed(
    candidates: &[&'static str],
    entries: &[&'static str],
    facts: &crate::authorization::StageFacts,
    actor: &crate::authorization::Principal,
    project: Option<&crate::domain::DomainId>,
) -> Vec<&'static str> {
    candidates
        .iter()
        .copied()
        .filter(|code| entries.contains(code))
        .filter(|code| {
            action_requirement(code).is_some_and(|requirement| {
                crate::authorization::requirement_met(facts, actor, requirement, project)
            })
        })
        .collect()
}

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

    /// 声明与实现必须一致，双向都不许含糊。
    ///
    /// - `planned` → handler 只能是 `not_implemented`，且必须声明 501。
    /// - `served` → handler 不得仍是 `not_implemented`，且**不得**再声明 501
    ///   （留着 501 等于告诉客户端「这个端点可能不受理」，与 served 自相矛盾）。
    ///
    /// 两个方向都判，是因为只判一个方向时，另一侧的错法无人拦：漏改 `planned`
    /// 会让实现假装未实现，漏改 handler 会让 served 假装已实现。
    #[test]
    fn declared_status_matches_the_handler_it_points_at() {
        use crate::http_router::Implementation;
        let unimplemented: crate::http_router::Handler<HostServer> = not_implemented;

        for route in routes() {
            let still_stub = std::ptr::fn_addr_eq(route.handler, unimplemented);
            let statuses: Vec<u16> = route
                .contract
                .responses
                .iter()
                .map(|response| response.status)
                .collect();

            match route.contract.status {
                Implementation::Planned => {
                    assert!(
                        still_stub,
                        "{} 声明 planned，但 handler 不是 not_implemented：要么接线，要么别声明",
                        route.pattern
                    );
                    assert!(
                        statuses.contains(&501),
                        "{} 未声明 501：计划中操作必须如实说明不会被受理",
                        route.pattern
                    );
                }
                Implementation::Served => {
                    assert!(
                        !still_stub,
                        "{} 声明 served，但 handler 仍是 not_implemented",
                        route.pattern
                    );
                    assert!(
                        !statuses.contains(&501),
                        "{} 已 served，却仍声明 501：契约与实现自相矛盾",
                        route.pattern
                    );
                }
            }

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

    // -----------------------------------------------------------------------
    // GET /delivery-tracking/{id}：四阶段的端到端用例（P8 3.6 B5）
    // -----------------------------------------------------------------------

    fn config() -> crate::config::BodConfig {
        let mut config = crate::config::BodConfig::default();
        config.sqlite.busy_timeout_ms = 5_000;
        config
    }

    /// 一条跟踪项 + 两个客户目标 + 各一条部署观察。
    ///
    /// 三个主体刻意都不重叠：`user-dev` 只是项目成员，`user-cust-a` 只绑了 `customer-a`，
    /// `user-outsider` 什么都没有。重叠会让「哪条规则在起作用」无法归因。
    fn store_with_two_customers() -> crate::store::SqliteStore {
        let store = crate::store::SqliteStore::in_memory(&config()).expect("内存库应可打开");
        store
            .execute_batch_for_test(
                "INSERT INTO delivery_tracking (
                     tracking_id, company_id, project_id, title, resource_version,
                     created_at_ms, updated_at_ms)
                 VALUES ('tracking-1', 'company-1', 'project-1', '两客户的交付', 3, 1000, 1000);
                 INSERT INTO delivery_targets (
                     target_id, company_id, tracking_id, customer_id, environment,
                     acceptance, switch_state, population_known, created_at_ms, updated_at_ms)
                 VALUES
                   ('target-a', 'company-1', 'tracking-1', 'customer-a', 'prod',
                    '验收条款', 'synced', 1, 1000, 1000),
                   ('target-b', 'company-1', 'tracking-1', 'customer-b', 'prod',
                    '验收条款', 'synced', 1, 1000, 1000);
                 INSERT INTO delivery_observations (
                     observation_id, company_id, tracking_id, target_id, kind, source_kind,
                     source_ref, revision, actor_id, authorized, observed_at, observed_at_ms,
                     recorded_at_ms)
                 VALUES
                   ('obs-a', 'company-1', 'tracking-1', 'target-a', 'deployment', 'connector',
                    'conn-a', 'rev-a', 'user-dev', 1, '2026-11-20T00:00:00Z', 1000, 1000),
                   ('obs-b', 'company-1', 'tracking-1', 'target-b', 'deployment', 'connector',
                    'conn-b', 'rev-b', 'user-dev', 1, '2026-11-20T00:00:00Z', 1000, 1000);
                 INSERT INTO project_members (
                     company_id, project_id, principal_id, kind, revision, active, updated_at_ms)
                 VALUES ('company-1', 'project-1', 'user-dev', 'user', 1, 1, 1000);
                 INSERT INTO customer_bindings (
                     company_id, principal_id, customer_id, revision, active, updated_at_ms)
                 VALUES
                   ('company-1', 'user-cust-a', 'customer-a', 1, 1, 1000),
                   ('company-1', 'user-cust-none', 'customer-z', 1, 1, 1000);",
            )
            .expect("夹具应写入成功");
        store
    }

    fn actor(id: &str) -> DeliveryActor {
        DeliveryActor {
            company_id: "company-1".to_string(),
            principal: crate::authorization::Principal {
                id: crate::domain::DomainId::new(id),
                kind: crate::authorization::PrincipalKind::User,
                claimed_roles: Vec::new(),
            },
        }
    }

    fn read_as(id: &str, tracking_id: &str) -> Result<DeliveryTrackingView, RouteError> {
        let reader = crate::store::read_port::StoreTrackingRead::new(store_with_two_customers());
        evaluate_read(&actor(id), &reader, tracking_id)
    }

    /// 项目成员看到全部目标与全部证据：阶段二整体放行，阶段四不适用。
    #[test]
    fn a_project_member_sees_every_target() {
        let view = read_as("user-dev", "tracking-1").expect("成员应可读取");
        assert_eq!(view.targets.len(), 2, "两个客户的目标都要在");
        assert_eq!(view.evidence.len(), 2, "两条证据都要在");
        assert_eq!(view.version, 3, "版本取库里的 resource_version");
    }

    /// 客户协作者只见本客户的目标，且证据时间线一并收窄。
    ///
    /// 只断言「看不到别人的」而不去断言「一定看得到自己的」：能否有结论取决于
    /// 覆盖判定，那是另一个模块的事。这条用例守的是**不越界**。
    #[test]
    fn a_customer_collaborator_sees_only_own_target() {
        let view = read_as("user-cust-a", "tracking-1").expect("有本客户目标时应可读取");
        assert_eq!(
            view.targets.len(),
            1,
            "只有自己那个客户？被排除的目标不该出现"
        );
        assert_eq!(view.targets[0].target.customer_id, "customer-a");
        assert!(
            view.evidence.iter().all(|stamp| stamp.revision != "rev-b"),
            "别的客户的证据修订不得随视图送出去：{:?}",
            view.evidence
        );
    }

    /// 有客户绑定、但这条记录里没有他的目标 → 与「记录不存在」同答。
    ///
    /// 回 200 空 targets 会泄漏「这条记录确实存在，只是里面没有你的目标」，
    /// 那等于把「别人在这里有交付」这件事告诉了不该知道的人。
    #[test]
    fn a_customer_without_target_here_gets_the_same_answer_as_missing() {
        let absent =
            read_as("user-cust-none", "tracking-1").expect_err("本客户无目标时不得返回视图");
        let missing = read_as("user-cust-none", "tracking-404").expect_err("记录不存在");
        assert_eq!(absent.status, 404);
        assert_eq!(absent.code, "BOD_NOT_FOUND");
        assert_eq!(
            (absent.status, absent.code),
            (missing.status, missing.code),
            "「有记录但没你的目标」与「没有这条记录」必须同答"
        );
    }

    /// 无关用户：不可见，且与「不存在」同答（不允许用返回码枚举存在性）。
    #[test]
    fn an_outsider_cannot_tell_existence_apart() {
        let invisible = read_as("user-outsider", "tracking-1").expect_err("既非成员也无绑定");
        let missing = read_as("user-outsider", "tracking-404").expect_err("记录不存在");
        assert_eq!(invisible.code, "BOD_NOT_FOUND");
        assert_eq!(
            (invisible.status, invisible.code),
            (missing.status, missing.code)
        );
    }

    /// 事实装配失败回 503，**不降级成空事实**。
    ///
    /// 这条用例同时守顺序：装配失败后不得继续去读视图（假实现里 panic 会炸出来），
    /// 否则「读不出来」会先花一次查询、再被当成「没有事实」。
    struct UnreadableFacts;

    impl TrackingRead for UnreadableFacts {
        fn stage_facts(
            &self,
            _company_id: &str,
            _principal: &crate::authorization::Principal,
        ) -> Result<crate::authorization::StageFacts, StageFactsError> {
            Err(StageFactsError::Unreadable {
                group: "grants",
                detail: "锁等待超时".to_string(),
            })
        }

        fn tracking_view(
            &self,
            _company_id: &str,
            _tracking_id: &str,
        ) -> Result<Option<crate::delivery_tracking::TrackingView>, StageFactsError> {
            panic!("事实装配失败后不得继续读视图");
        }
    }

    #[test]
    fn unreadable_facts_are_503_and_never_an_empty_snapshot() {
        let err = evaluate_read(&actor("user-dev"), &UnreadableFacts, "tracking-1")
            .expect_err("装配失败必须报错");
        assert_eq!(err.status, 503, "取不到事实是服务方的问题，不是权限问题");
        assert_eq!(err.code, "BOD_FACTS_UNAVAILABLE");
    }

    /// 未装配主体的宿主一律拒绝，且**对任何 id 同答**。
    ///
    /// 这条守的是安全默认：装配缺失时不回退成「匿名用户」。匿名主体会让权限判定
    /// 与审计归属同时失真，而失真后的记录比没有记录更难查。
    #[test]
    fn a_host_without_an_actor_denies_before_looking_at_the_id() {
        let descriptor = std::env::temp_dir().join(format!(
            "bod-delivery-api-desc-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let server = HostServer::bind("127.0.0.1:0", &descriptor, 0).expect("应可绑定本机端口");

        let ask = |tracking: &str| {
            let endpoint = EndpointRequest {
                request: crate::http_router::Request::new(
                    "GET",
                    &format!("/api/v1/delivery-tracking/{tracking}"),
                ),
                params: vec![("id", tracking.to_string())],
            };
            read_tracking(&server, endpoint)
        };

        // `Reply` 没实现 `Debug`（它是传输层类型，不该为此加派生），所以用 match 而不用 `expect_err`。
        let existing = match ask("tracking-1") {
            Err(err) => err,
            Ok(_) => panic!("没有主体时不得返回视图"),
        };
        let absent = match ask("tracking-404") {
            Err(err) => err,
            Ok(_) => panic!("没有主体时不得返回视图"),
        };
        assert_eq!(existing.status, 401);
        assert_eq!(existing.code, "BOD_API_DENIED");
        assert_eq!(
            (existing.status, existing.code),
            (absent.status, absent.code),
            "主体未确认时，答案不得随对象变化"
        );

        // 装配完成后同一条路由开始走真判定：这条断言防的是「配了还是 401」——
        // 装配入口失效时它会先红，而不是等到线上才发现所有读取都被拒。
        let server = server.with_delivery(
            std::sync::Arc::new(crate::store::read_port::StoreTrackingRead::new(
                store_with_two_customers(),
            )),
            actor("user-dev"),
        );
        let endpoint = EndpointRequest {
            request: crate::http_router::Request::new(
                "GET",
                "/api/v1/delivery-tracking/tracking-1",
            ),
            params: vec![("id", "tracking-1".to_string())],
        };
        let reply = read_tracking(&server, endpoint).expect("装配后成员应可读取");
        match reply {
            Reply::Json { status, value } => {
                assert_eq!(status, 200);
                assert_eq!(value["targets"].as_array().map(Vec::len), Some(2));
            }
            Reply::Upgrade { .. } => panic!("读取产出的是普通响应，不是长连接"),
        }
    }
}

#[cfg(test)]
mod action_rules {
    //! 「允许动作」的四道关：候选 → 该面有入口 → 需求已登记 → 当事人满足需求。
    //!
    //! 这里用**合成的入口切片**来驱动 `select_allowed`，而不是去改路由表或加测试开关：
    //! 生产路径传的是 `available_on(surface)`，两边调用的是同一个函数，
    //! 所以「入口表里有什么」可以变，「四道关的顺序与判据」只有一份。

    use super::{
        ACTION_DRAFT, ACTION_LINK_SOURCE, ActionSurface, CLI_ENTRIES, available_on, select_allowed,
    };
    use crate::authorization::{Principal, PrincipalKind, StageFacts};
    use crate::domain::DomainId;

    const PROJECT: &str = "project-1";

    fn facts() -> StageFacts {
        let mut facts = StageFacts::default();
        facts
            .members
            .push((PROJECT.to_string(), DomainId::new("user-1")));
        facts
    }

    fn member() -> Principal {
        Principal {
            id: DomainId::new("user-1"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        }
    }

    fn project() -> DomainId {
        DomainId::new(PROJECT)
    }

    /// 关一：没有入口就不列。
    ///
    /// 这是 7.8 之前 `allowed_actions` 恒空的**真实原因**：两条动作路由都还是 planned。
    /// 断言它，是为了让「空」有解释——而不是让人以为判定坏了。
    #[test]
    fn no_entry_means_no_action_even_when_the_actor_may_do_it() {
        let allowed = select_allowed(
            &[ACTION_LINK_SOURCE, ACTION_DRAFT],
            &[],
            &facts(),
            &member(),
            Some(&project()),
        );
        assert!(allowed.is_empty(), "没有入口时不得宣称可做：{allowed:?}");
    }

    /// 关二：入口存在但需求没登记，也**不列**（fail-closed）。
    ///
    /// 合成的 `teleport` 码没有 `action_requirement`。这条用例挡的是「新加一个动作码、
    /// 忘了想清楚谁能做」——那种情况下静默放行比少显示一个动作危险。
    #[test]
    fn entry_without_a_declared_requirement_is_never_advertised() {
        let allowed = select_allowed(&["teleport"], &["teleport"], &facts(), &member(), None);
        assert!(
            allowed.is_empty(),
            "需求未登记的动作必须 fail-closed，实际：{allowed:?}"
        );
    }

    /// 关三与关四：有入口 + 需求已登记 + 当事人是项目成员 → 列。
    ///
    /// 反过来说，这条用例是「允许动作在接线之后真的会出现」的证明：
    /// 只断言前两条（永远为空）会把一个坏掉的判定伪装成正确。
    #[test]
    fn entry_plus_membership_is_advertised() {
        let allowed = select_allowed(
            &[ACTION_LINK_SOURCE, ACTION_DRAFT],
            &[ACTION_LINK_SOURCE, ACTION_DRAFT],
            &facts(),
            &member(),
            Some(&project()),
        );
        assert_eq!(allowed, vec![ACTION_LINK_SOURCE, ACTION_DRAFT]);
    }

    /// 关四的反面：入口与需求都在，但当事人不是成员 → 不列。
    #[test]
    fn non_member_sees_nothing_even_with_an_entry() {
        let stranger = Principal {
            id: DomainId::new("user-9"),
            kind: PrincipalKind::User,
            claimed_roles: Vec::new(),
        };
        let allowed = select_allowed(
            &[ACTION_LINK_SOURCE],
            &[ACTION_LINK_SOURCE],
            &facts(),
            &stranger,
            Some(&project()),
        );
        assert!(allowed.is_empty(), "非成员不得看到动作：{allowed:?}");
    }

    /// 项目不明时不猜：`Membership` 需要项目，缺项目一律不满足。
    #[test]
    fn membership_without_a_project_is_not_satisfied() {
        let allowed = select_allowed(
            &[ACTION_LINK_SOURCE],
            &[ACTION_LINK_SOURCE],
            &facts(),
            &member(),
            None,
        );
        assert!(allowed.is_empty(), "缺项目时不得默认满足：{allowed:?}");
    }

    /// CLI 面还没有入口；一旦有，读命令必须先装配主体事实。
    ///
    /// 这条是**待办哨兵**：给 `CLI_ENTRIES` 加条目的人会让它红。红了不要把它改成
    /// 「允许非空」——要先把 `bod tracking` 的 `stage_facts` + 主体装配补上，
    /// 否则 CLI 会静默地不列动作，看起来像「命令能跑但没权限」。
    #[test]
    fn cli_surface_stays_unwired_until_facts_are_assembled() {
        assert!(
            CLI_ENTRIES.is_empty(),
            "CLI 入口已登记：先给读命令装配 stage_facts 与主体，再放开这条断言"
        );
        assert!(available_on(ActionSurface::Cli).is_empty());
    }

    /// HTTP 面目前也没有入口：两条动作路由都还是 planned。
    ///
    /// 与上面那条不同，这一条**转 served 时必须更新**：那时 `available_on(Http)`
    /// 应当包含对应动作，本用例改成断言包含关系即证明「路由开了，视图就跟上」。
    #[test]
    fn http_surface_has_no_entry_while_both_action_routes_are_planned() {
        assert_eq!(
            available_on(ActionSurface::Http),
            Vec::<&'static str>::new(),
            "有动作路由转 served 了：请把本条改成断言对应动作出现，并补一条端到端"
        );
    }
}
