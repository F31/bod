//! 薄路由层（CR-003）：把「HTTP 长什么样」与「业务怎么处理」分开。
//!
//! 本层只做四件事：**持有请求数据、匹配路由、按声明校验鉴权、调用 handler**。
//! 不做 IO——`TcpStream` 只作为 `Reply::Upgrade` 闭包的参数类型出现，
//! 真正读写字节仍在传输层（`host_handshake`）。
//!
//! 三条约束都是为「加端点时不遗漏」，不是为了好看：
//!
//! 1. **鉴权逐条必填**：`Route::auth` 无 `Default`，也没有「省略即放行」的路径；
//!    每条路由还必须写 `note` 说明为什么这样鉴权，测试拒绝空 note。
//!    既有代码里 `health` 无鉴权是合理的，但「合理」要被写下来才能被复核。
//! 2. **静态路径优先于参数路径**：两遍匹配，与注册顺序无关。否则路由顺序一调，
//!    `/feedback-receipts/{id}` 就会把 `/feedback-receipts/summary` 吃掉。
//! 3. **未匹配与鉴权失败分开表示**：对外同为 404/401，对内必须能区分，
//!    否则日志里永远看不到「有人在扫路径」。
//!
//! 参数值**不做百分号解码**，按原样返回。`%2F` 不会切段，因此不会改变匹配结果；
//! 但 handler 拿到后**不得直接拼路径或拼 SQL**——它不是可信输入。

use std::net::TcpStream;

/// HTTP 方法。
///
/// 只列出业务真正用到的四个。其余方法在 `parse` 时返回 `None`，
/// 路由表自然匹配不到，落 404——不引入「支持了但没人用」的表面完备。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PATCH" => Some(Self::Patch),
            "DELETE" => Some(Self::Delete),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// 一条 HTTP 请求的原始数据。由传输层解析后交给本层，本层不再回头读 socket。
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn new(method: &str, path: &str) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            headers: Vec::new(),
            body: String::new(),
        }
    }

    /// 测试与 handler 都要能补头，所以给构造器而不是暴露字段操作。
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn with_body(mut self, body: &str) -> Self {
        self.body = body.to_string();
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 同名字段的**全部**取值。Host 校验需要知道「有几个」，
    /// 重复 Host 是请求走私的信号，不能只看第一个。
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// 不含查询串的路径：路由匹配一律用它，避免 `?cursor=3` 落到 404。
    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }

    pub fn query(&self) -> Option<&str> {
        self.path.split_once('?').map(|(_, query)| query)
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query()?.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then_some(value)
        })
    }

    /// 凭据是否被塞进查询串。SSE 不接受这种传法：查询串会进日志与浏览器历史。
    pub fn has_query_token(&self) -> bool {
        self.query().is_some_and(|query| {
            query.split('&').any(|pair| {
                let key = pair.split_once('=').map(|(key, _)| key).unwrap_or(pair);
                matches!(key, "token" | "session_key" | "access_token")
            })
        })
    }

    /// 取出 Bearer 令牌，只认 `Authorization` 头。
    pub fn bearer_token(&self) -> Option<&str> {
        self.header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
    }
}

/// 鉴权方式。**必须逐条显式声明**——没有 `Default`，漏写就编译不过。
///
/// 分三档而不是「统一拦 / 不拦」，是因为有一类端点的拒绝原因本身要细分：
/// 事件流需要区分「没带凭据」「把凭据塞进查询串」「令牌不匹配」，
/// 这三种都答 401 但 `reason` 不同，统一拦截会把诊断信息抹平。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// 需要 `Authorization: Bearer <会话密钥>`，**分发前统一校验**。
    /// 携带该端点被拒时返回的错误码，拒绝发生在 handler 之前。
    Bearer(&'static str),
    /// 由 handler 自行校验，携带 handler 会使用的拒绝码。
    /// 令牌可能不在 `Authorization` 头里（握手用请求体字段，事件流要额外拒绝查询串令牌），
    /// 所以这一档必须在 `note` 里写清令牌位置与额外规则。
    InHandler(&'static str),
    /// 无需鉴权。只允许「探测存活且不泄露业务信息」的端点。
    Public,
}

impl Auth {
    /// 供测试与文档使用的一句话说明。
    pub fn label(self) -> &'static str {
        match self {
            Self::Bearer(_) => "bearer",
            Self::InHandler(_) => "in_handler",
            Self::Public => "public",
        }
    }

    fn requires_bearer(self) -> bool {
        matches!(self, Self::Bearer(_))
    }

    /// 被拒时对外返回的稳定错误码；`Public` 不会被拒，返回 `None`。
    fn denied_code(self) -> Option<&'static str> {
        match self {
            Self::Bearer(code) | Self::InHandler(code) => Some(code),
            Self::Public => None,
        }
    }
}

/// handler 的入参：原始请求 + 从路径里解析出的参数。
#[derive(Debug, Clone)]
pub struct EndpointRequest {
    pub request: Request,
    /// 路径参数，顺序与 pattern 中 `{name}` 出现顺序一致。
    pub params: Vec<(&'static str, String)>,
}

impl EndpointRequest {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// handler 返回的错误。用稳定码而非自由文本：调用方按码分支，不解析人类消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteError {
    pub status: u16,
    pub code: &'static str,
}

impl RouteError {
    pub fn new(status: u16, code: &'static str) -> Self {
        Self { status, code }
    }
}

/// handler 的返回。
pub enum Reply {
    /// 普通响应：状态码 + 一个 JSON 值。序列化交给传输层，避免本层拼字符串。
    Json {
        status: u16,
        value: serde_json::Value,
    },
    /// 把连接交给 handler 独占（SSE 长连接）。分发方负责放到独立线程执行，
    /// 否则一个长连接会把健康检查与停止请求堵在队列里。
    Upgrade { handle: UpgradeHandle },
}

impl Reply {
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self::Json { status, value }
    }

    pub fn ok(value: serde_json::Value) -> Self {
        Self::json(200, value)
    }

    pub fn upgrade(handle: UpgradeHandle) -> Self {
        Self::Upgrade { handle }
    }
}

/// 长连接接管句柄。用 `FnOnce` 是因为连接只能被交出一次。
pub type UpgradeHandle = Box<dyn FnOnce(TcpStream) + Send>;

/// handler 签名。`S` 是传输层的服务器状态（`HostServer`），
/// 本层不关心它是什么，也就不需要为本层再造一个 trait 来抽象状态。
pub type Handler<S> = fn(&S, EndpointRequest) -> Result<Reply, RouteError>;

/// 一条路由。
pub struct Route<S> {
    pub method: Method,
    /// 以 `/` 开头的模式，参数段写作 `{name}`。
    pub pattern: &'static str,
    pub auth: Auth,
    /// 为什么是这种鉴权。空串会被测试拒绝——理由必须写下来才能被复核。
    pub note: &'static str,
    /// 对外契约（CR-004）。与路由同表，避免「实现」与「文档」两处维护。
    pub contract: Contract,
    pub handler: Handler<S>,
}

/// 一条路由的对外契约。
///
/// 字段刻意都是 `&'static`/函数指针：路由表是 `static`，契约必须能在编译期
/// 完成构造。schema 存成 `fn() -> Value` 而不是 `Value`，因为 `serde_json`
/// 的序列化不能在 `const` 上下文里求值。
#[derive(Debug, Clone, Copy)]
pub struct Contract {
    /// 一句话说清这个端点做什么（`summary`）；更长的边界说明写 `Route::note`。
    pub summary: &'static str,
    pub request: Option<ContractBody>,
    /// 查询参数无法从路径模式推出（handler 自己读），必须显式声明。
    pub query: &'static [QueryParam],
    /// 业务响应。**不包含 401/404**：这两个由 `Auth` 与分发层统一产生，
    /// 在契约里再写一遍会形成第二份拒绝码，`openapi::document` 直接拒绝。
    pub responses: &'static [ContractResponse],
}

/// 响应或请求体。
#[derive(Debug, Clone, Copy)]
pub enum ContractBody {
    /// JSON 体。`component` 是 `components.schemas` 里的名字，必须全局唯一。
    Json {
        component: &'static str,
        schema: fn() -> serde_json::Value,
    },
    /// `text/event-stream`。不建模成 JSON——否则客户端会去解析 SSE 帧。
    EventStream,
}

impl ContractBody {
    /// `const` 而不是普通构造函数：路由表是 `static`，条目必须在编译期成型。
    pub const fn json(component: &'static str, schema: fn() -> serde_json::Value) -> Self {
        Self::Json { component, schema }
    }
}

/// 一条业务响应。
#[derive(Debug, Clone, Copy)]
pub struct ContractResponse {
    pub status: u16,
    pub description: &'static str,
    pub body: Option<ContractBody>,
}

impl ContractResponse {
    pub const fn new(status: u16, description: &'static str, body: Option<ContractBody>) -> Self {
        Self {
            status,
            description,
            body,
        }
    }
}

/// 查询参数声明。
#[derive(Debug, Clone, Copy)]
pub struct QueryParam {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// 匹配结果。`MethodMismatch` 对外仍是 404（与既有行为一致），
/// 但对内分开表示，便于将来加 405 与日志统计。
///
/// 注意 `MethodMismatch` 的触发面比「字面路径存在」更宽：只要该路径被
/// **静态模式占用**，即使另有参数模式能匹配、且方法相符，也算方法不符。
/// 理由见 `match_route` 的第一步。
pub enum Match<'a, S> {
    Found {
        route: &'a Route<S>,
        params: Vec<(&'static str, String)>,
    },
    MethodMismatch,
    NotFound,
}

/// 分发结果。
pub enum Outcome {
    Reply(Reply),
    Failed(RouteError),
    /// Bearer 缺失或不匹配。对外 401，错误码取自路由的 `Auth` 声明。
    Unauthorized(&'static str),
    /// 路径存在但方法不对。对外 404（保持既有行为）。
    MethodNotAllowed,
    NotFound,
}

/// 路径切段。`/a/b` → `["a", "b"]`；`/` → `[]`。
///
/// **不吞掉空段**：`/a//b` 切出 `["a", "", "b"]`，参数匹配要求值非空，
/// 所以它匹配不到 `/a/{x}/b`——空段是客户端错误，不该被悄悄规范化。
fn segments(path: &str) -> Vec<&str> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('/').collect()
}

/// 模式切段，同时标出哪几段是参数。
fn pattern_segments(pattern: &'static str) -> Vec<Segment> {
    segments(pattern)
        .into_iter()
        .map(
            |raw| match raw.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) => Segment::Param(name),
                None => Segment::Static(raw),
            },
        )
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    Static(&'static str),
    Param(&'static str),
}

/// 模式里出现的参数名，按出现顺序。
///
/// 公开它是为了让 OpenAPI 投影**只从模式取参数**：路径参数再声明一份，
/// 就等于允许「模式改了、参数没改」这种不一致存在。
pub fn pattern_parameters(pattern: &'static str) -> Vec<&'static str> {
    pattern_segments(pattern)
        .into_iter()
        .filter_map(|segment| match segment {
            Segment::Param(name) => Some(name),
            Segment::Static(_) => None,
        })
        .collect()
}

/// 尝试把路径匹配到某条模式上，成功则返回按出现顺序的参数值。
fn match_pattern(pattern: &'static str, path: &str) -> Option<Vec<(&'static str, String)>> {
    let pattern_parts = pattern_segments(pattern);
    let path_parts = segments(path);
    if pattern_parts.len() != path_parts.len() {
        return None;
    }
    let mut params = Vec::new();
    for (part, value) in pattern_parts.iter().zip(path_parts.iter()) {
        match part {
            Segment::Static(text) => {
                if *text != *value {
                    return None;
                }
            }
            Segment::Param(name) => {
                // 空值不接受：`/a//b` 不是「x 为空的情况」，是畸形请求。
                if value.is_empty() {
                    return None;
                }
                params.push((*name, (*value).to_string()));
            }
        }
    }
    Some(params)
}

/// 模式里是否含参数段。
fn is_parameterized(pattern: &str) -> bool {
    pattern.contains('{')
}

/// 匹配路由。**路径归属优先于方法，静态优先于参数**——三步，顺序即契约。
///
/// 1. **静态模式占用路径即定归属**：命中静态模式且方法相符就返回；方法不符
///    直接 `MethodMismatch`，不再尝试参数模式。
///    否则 `POST /delivery-tracking/imports` 与 `GET /delivery-tracking/{id}`
///    并存时，`GET /delivery-tracking/imports` 会被解析成「查 id=imports」，
///    把**方法错误伪装成数据不存在**——客户端会去查数据，而不是改方法。
/// 2. 路径没被静态占用，才进参数模式。
/// 3. 都没命中：有任一模式能匹配该路径则 `MethodMismatch`，否则 `NotFound`。
///
/// 「静态」与「参数」的区分按整条模式判断（含 `{` 即参数模式），不是逐段判断；
/// 逐段会让 `/a/{x}/c` 与 `/a/b/{y}` 这种混排产生歧义。
pub fn match_route<'a, S>(routes: &'a [Route<S>], request: &Request) -> Match<'a, S> {
    let method = Method::parse(&request.method);
    let path = request.path_only();
    let path_parts = segments(path);

    // 第一步：静态模式。先确定「谁占了这个路径」，再看方法。
    let mut static_owns_path = false;
    for route in routes {
        if is_parameterized(route.pattern) || match_pattern(route.pattern, path).is_none() {
            continue;
        }
        static_owns_path = true;
        if Some(route.method) == method {
            // 静态模式的参数集必然为空，不必回读 `match_pattern` 的结果。
            return Match::Found {
                route,
                params: Vec::new(),
            };
        }
    }
    if static_owns_path {
        return Match::MethodMismatch;
    }

    // 第二步：参数模式。走到这里说明该路径没被任何静态模式占用。
    for route in routes {
        if !is_parameterized(route.pattern) || Some(route.method) != method {
            continue;
        }
        if let Some(params) = match_pattern(route.pattern, path) {
            return Match::Found { route, params };
        }
    }

    // 第三步：区分「路径存在但方法不对」与「路径根本不存在」。
    // 根路径不参与：它永远「存在」，报成方法不符只会掩盖真实的 404。
    if !path_parts.is_empty()
        && routes
            .iter()
            .any(|route| match_pattern(route.pattern, path).is_some())
    {
        return Match::MethodMismatch;
    }
    Match::NotFound
}

/// 定长比较：避免按字节提前返回泄漏令牌前缀。
pub(crate) fn token_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 分发：匹配 → 按声明校验鉴权 → 调用 handler。
///
/// **鉴权在校验 handler 之前**，且顺序本身有意义：非法请求不进入业务逻辑。
/// `bearer` 是当前服务器的会话密钥，由传输层传入——本层不持有秘密。
pub fn dispatch<S>(routes: &[Route<S>], state: &S, bearer: &str, request: Request) -> Outcome {
    let matched = match match_route(routes, &request) {
        Match::Found { route, params } => (route, params),
        Match::MethodMismatch => return Outcome::MethodNotAllowed,
        Match::NotFound => return Outcome::NotFound,
    };
    let (route, params) = matched;

    if route.auth.requires_bearer() {
        let presented = request.bearer_token().unwrap_or_default();
        if !token_eq(presented, bearer) {
            // `requires_bearer` 为真时 `denied_code` 必为 `Some`，回退值只为避免 panic。
            return Outcome::Unauthorized(route.auth.denied_code().unwrap_or("BOD_UNAUTHORIZED"));
        }
    }

    let endpoint = EndpointRequest { request, params };
    match (route.handler)(state, endpoint) {
        Ok(reply) => Outcome::Reply(reply),
        Err(err) => Outcome::Failed(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个用不到状态的路由集合，便于只测匹配逻辑。
    fn ping(_state: &(), _request: EndpointRequest) -> Result<Reply, RouteError> {
        Ok(Reply::ok(serde_json::json!({ "ok": true })))
    }

    /// 测试路由只关心匹配与鉴权，契约字段填最小合法值。
    ///
    /// 契约仍是必填字段而不是 `Default`：新增端点时必须明确写出对外响应，
    /// 「没填」与「没有响应体」在类型上不能是同一件事。
    fn route_of(
        method: Method,
        pattern: &'static str,
        auth: Auth,
        note: &'static str,
    ) -> Route<()> {
        Route {
            method,
            pattern,
            auth,
            note,
            contract: Contract {
                summary: "测试路由",
                request: None,
                query: &[],
                responses: &[ContractResponse {
                    status: 200,
                    description: "ok",
                    body: None,
                }],
            },
            handler: ping,
        }
    }

    fn routes() -> Vec<Route<()>> {
        vec![
            route_of(
                Method::Get,
                "/_bod/health",
                Auth::Public,
                "存活探测，不泄露业务信息",
            ),
            route_of(
                Method::Get,
                "/_bod/events",
                Auth::Bearer("BOD_TEST_DENIED"),
                "事件流含业务事实",
            ),
            route_of(
                Method::Post,
                "/api/v1/feedback-receipts/{id}/responses",
                Auth::Bearer("BOD_TEST_DENIED"),
                "补充回执需身份",
            ),
            // 静态路径注册在参数路径之后，用来验证「静态优先与顺序无关」。
            route_of(
                Method::Get,
                "/api/v1/feedback-receipts/summary",
                Auth::Bearer("BOD_TEST_DENIED"),
                "汇总视图",
            ),
            route_of(
                Method::Post,
                "/_bod/shutdown",
                Auth::Bearer("BOD_TEST_DENIED"),
                "停机需会话密钥",
            ),
        ]
    }

    /// §11 的路径形态：**静态子资源只挂了 POST**，参数子资源挂 GET。
    ///
    /// 顺序刻意把参数路由放前面——若实现依赖注册顺序，这里会立刻红。
    fn tracking_routes() -> Vec<Route<()>> {
        vec![
            route_of(
                Method::Get,
                "/api/v1/delivery-tracking/{id}",
                Auth::Bearer("BOD_TEST_DENIED"),
                "跟踪项含客户交付事实",
            ),
            route_of(
                Method::Post,
                "/api/v1/delivery-tracking/imports",
                Auth::Bearer("BOD_TEST_DENIED"),
                "导入需身份，权限与发布执行分开",
            ),
        ]
    }

    #[test]
    fn exact_path_matches() {
        let routes = routes();
        let request = Request::new("GET", "/_bod/health");
        match match_route(&routes, &request) {
            Match::Found { route, params } => {
                assert_eq!(route.pattern, "/_bod/health");
                assert!(params.is_empty());
            }
            _ => panic!("应命中 health"),
        }
    }

    #[test]
    fn query_string_does_not_break_matching() {
        let routes = routes();
        let request = Request::new("GET", "/_bod/health?verbose=1");
        assert!(matches!(
            match_route(&routes, &request),
            Match::Found { .. }
        ));
    }

    #[test]
    fn path_parameters_are_captured_in_order() {
        let routes = routes();
        let request = Request::new("POST", "/api/v1/feedback-receipts/rcpt-42/responses");
        match match_route(&routes, &request) {
            Match::Found { route, params } => {
                assert_eq!(route.pattern, "/api/v1/feedback-receipts/{id}/responses");
                assert_eq!(params, vec![("id", "rcpt-42".to_string())]);
            }
            _ => panic!("应命中参数路由"),
        }
    }

    /// 这条是「两遍匹配」的存在理由：静态路由注册在后面也必须赢。
    #[test]
    fn static_pattern_wins_over_parameter_regardless_of_order() {
        let routes = routes();
        let request = Request::new("GET", "/api/v1/feedback-receipts/summary");
        match match_route(&routes, &request) {
            Match::Found { route, .. } => {
                assert_eq!(route.pattern, "/api/v1/feedback-receipts/summary")
            }
            _ => panic!("静态路由应胜出"),
        }
    }

    /// §11 的首个端点形态：静态路径归 POST，参数路径归 GET。
    ///
    /// 缺陷的原症状是 `GET /delivery-tracking/imports` 被
    /// `GET /delivery-tracking/{id}` 以 `id="imports"` 吃掉——方法错误被
    /// 伪装成「查无此跟踪项」。修好后静态路径占用即定归属，方法不符直接暴露。
    #[test]
    fn static_path_owned_by_another_method_is_not_captured_by_parameter_route() {
        let routes = tracking_routes();
        let request = Request::new("GET", "/api/v1/delivery-tracking/imports");
        assert!(
            matches!(match_route(&routes, &request), Match::MethodMismatch),
            "GET 静态路径被 POST 占用时，不得回落成查 id=imports"
        );
        assert!(matches!(
            dispatch(&routes, &(), "secret", request),
            Outcome::MethodNotAllowed
        ));
    }

    /// 同一静态路径挂了多个方法时，各自命中自己那条，互不遮蔽。
    #[test]
    fn same_static_path_serves_each_registered_method() {
        let mut routes = tracking_routes();
        routes.push(route_of(
            Method::Get,
            "/api/v1/delivery-tracking/imports",
            Auth::Bearer("BOD_TEST_DENIED"),
            "查询导入批次状态",
        ));
        for (method, path) in [
            ("GET", "/api/v1/delivery-tracking/imports"),
            ("POST", "/api/v1/delivery-tracking/imports"),
        ] {
            let request = Request::new(method, path);
            match match_route(&routes, &request) {
                Match::Found { route, params } => {
                    assert_eq!(route.method.as_str(), method);
                    assert_eq!(route.pattern, "/api/v1/delivery-tracking/imports");
                    assert!(params.is_empty(), "静态路由不应产生参数");
                }
                _ => panic!("{method} {path} 应命中静态路由"),
            }
        }
    }

    /// 修复不能把参数路由整体关掉：路径没被静态占用时照常匹配。
    #[test]
    fn parameter_route_still_matches_when_no_static_pattern_owns_path() {
        let routes = tracking_routes();
        let request = Request::new("GET", "/api/v1/delivery-tracking/track-7");
        match match_route(&routes, &request) {
            Match::Found { route, params } => {
                assert_eq!(route.pattern, "/api/v1/delivery-tracking/{id}");
                assert_eq!(params, vec![("id", "track-7".to_string())]);
            }
            _ => panic!("参数路由应命中"),
        }
    }

    #[test]
    fn empty_segment_does_not_match_parameter() {
        let routes = routes();
        let request = Request::new("POST", "/api/v1/feedback-receipts//responses");
        assert!(matches!(
            match_route(&routes, &request),
            Match::NotFound | Match::MethodMismatch
        ));
    }

    #[test]
    fn wrong_method_is_reported_separately() {
        let routes = routes();
        let request = Request::new("DELETE", "/_bod/health");
        assert!(matches!(
            match_route(&routes, &request),
            Match::MethodMismatch
        ));
    }

    #[test]
    fn unknown_path_is_not_found() {
        let routes = routes();
        let request = Request::new("GET", "/api/v1/nope");
        assert!(matches!(match_route(&routes, &request), Match::NotFound));
    }

    #[test]
    fn unsupported_method_never_matches() {
        let routes = routes();
        let request = Request::new("TRACE", "/_bod/health");
        assert!(matches!(
            match_route(&routes, &request),
            Match::MethodMismatch | Match::NotFound
        ));
    }

    #[test]
    fn dispatch_rejects_missing_bearer_on_protected_route() {
        let routes = routes();
        let request = Request::new("GET", "/_bod/events");
        assert!(matches!(
            dispatch(&routes, &(), "secret", request),
            Outcome::Unauthorized(_)
        ));
    }

    #[test]
    fn dispatch_rejects_wrong_bearer() {
        let routes = routes();
        let request =
            Request::new("GET", "/_bod/events").with_header("Authorization", "Bearer nope");
        assert!(matches!(
            dispatch(&routes, &(), "secret", request),
            Outcome::Unauthorized(_)
        ));
    }

    #[test]
    fn dispatch_accepts_correct_bearer() {
        let routes = routes();
        let request =
            Request::new("GET", "/_bod/events").with_header("Authorization", "Bearer secret");
        assert!(matches!(
            dispatch(&routes, &(), "secret", request),
            Outcome::Reply(_)
        ));
    }

    /// 公开路由不能被 Bearer 拦住——否则 health 探测会失效。
    #[test]
    fn public_route_needs_no_bearer() {
        let routes = routes();
        let request = Request::new("GET", "/_bod/health");
        assert!(matches!(
            dispatch(&routes, &(), "secret", request),
            Outcome::Reply(_)
        ));
    }

    /// 鉴权必须发生在 handler 之前：这条用手写 handler 记录是否被调用。
    #[test]
    fn unauthorized_request_never_reaches_handler() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn spy(_state: &(), _request: EndpointRequest) -> Result<Reply, RouteError> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(Reply::ok(serde_json::json!({})))
        }

        let routes = vec![Route {
            method: Method::Get,
            pattern: "/protected",
            auth: Auth::Bearer("BOD_TEST_DENIED"),
            note: "测试用",
            contract: Contract {
                summary: "测试用",
                request: None,
                query: &[],
                responses: &[],
            },
            handler: spy,
        }];
        let request = Request::new("GET", "/protected");
        let outcome = dispatch(&routes, &(), "secret", request);
        assert!(matches!(outcome, Outcome::Unauthorized(_)));
        assert_eq!(CALLS.load(Ordering::SeqCst), 0, "handler 不得被调用");
    }

    #[test]
    fn query_string_token_is_still_rejected_for_bearer_routes() {
        let routes = routes();
        let request = Request::new("GET", "/_bod/events?token=secret");
        assert!(request.has_query_token());
        assert!(matches!(
            dispatch(&routes, &(), "secret", request),
            Outcome::Unauthorized(_)
        ));
    }

    /// 路由表的自检：这些约束靠人记不住，必须机械检查。
    #[test]
    fn route_table_is_well_formed() {
        let routes = routes();
        for route in &routes {
            assert!(
                route.pattern.starts_with('/'),
                "模式必须以 / 开头：{}",
                route.pattern
            );
            assert!(
                !route.note.trim().is_empty(),
                "{} {} 缺少鉴权理由",
                route.method.as_str(),
                route.pattern
            );
            assert!(
                !route.contract.summary.trim().is_empty(),
                "{} {} 缺少契约摘要（OpenAPI 的 summary 会直接取它）",
                route.method.as_str(),
                route.pattern
            );
            // 参数名非空且不重复。
            let mut seen = Vec::new();
            for part in pattern_segments(route.pattern) {
                if let Segment::Param(name) = part {
                    assert!(!name.is_empty(), "{} 里有空参数名", route.pattern);
                    assert!(
                        !seen.contains(&name),
                        "{} 里参数 {name} 重复",
                        route.pattern
                    );
                    seen.push(name);
                }
            }
        }
        // 同一 (方法, 模式) 不得重复注册，否则先注册的静默胜出。
        for (index, left) in routes.iter().enumerate() {
            for right in routes.iter().skip(index + 1) {
                assert!(
                    !(left.method == right.method && left.pattern == right.pattern),
                    "重复路由：{} {}",
                    left.method.as_str(),
                    left.pattern
                );
            }
        }
    }

    #[test]
    fn auth_labels_are_stable() {
        assert_eq!(Auth::Bearer("x").label(), "bearer");
        assert_eq!(Auth::InHandler("x").label(), "in_handler");
        assert_eq!(Auth::Public.label(), "public");
    }

    #[test]
    fn bearer_token_only_reads_authorization_header() {
        let request = Request::new("GET", "/x").with_header("Authorization", "Bearer abc");
        assert_eq!(request.bearer_token(), Some("abc"));

        let wrong_scheme = Request::new("GET", "/x").with_header("Authorization", "Basic abc");
        assert_eq!(wrong_scheme.bearer_token(), None);
    }

    #[test]
    fn method_parse_covers_only_supported_verbs() {
        assert_eq!(Method::parse("GET"), Some(Method::Get));
        assert_eq!(Method::parse("POST"), Some(Method::Post));
        assert_eq!(Method::parse("PATCH"), Some(Method::Patch));
        assert_eq!(Method::parse("DELETE"), Some(Method::Delete));
        assert_eq!(Method::parse("PUT"), None);
        assert_eq!(Method::parse("get"), None, "方法区分大小写");
    }

    #[test]
    fn token_eq_is_length_then_content() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "abcd"));
    }
}
