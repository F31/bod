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
    pub handler: Handler<S>,
}

/// 匹配结果。`MethodMismatch` 对外仍是 404（与既有行为一致），
/// 但对内分开表示，便于将来加 405 与日志统计。
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

/// 匹配路由。**静态优先，与注册顺序无关**——两遍扫描。
pub fn match_route<'a, S>(routes: &'a [Route<S>], request: &Request) -> Match<'a, S> {
    let method = Method::parse(&request.method);
    let path = request.path_only();
    let path_parts = segments(path);
    let path_has_param = |pattern: &'static str| pattern.contains('{');

    // 第一遍：纯静态模式，逐段全等。
    for route in routes {
        if Some(route.method) != method || path_has_param(route.pattern) {
            continue;
        }
        if let Some(params) = match_pattern(route.pattern, path) {
            return Match::Found { route, params };
        }
    }

    // 第二遍：含参数的模式。
    for route in routes {
        if Some(route.method) != method || !path_has_param(route.pattern) {
            continue;
        }
        if let Some(params) = match_pattern(route.pattern, path) {
            return Match::Found { route, params };
        }
    }

    // 没命中：区分「路径存在但方法不对」与「路径根本不存在」。
    let same_path_exists = routes
        .iter()
        .any(|route| match_pattern(route.pattern, path).is_some())
        && !path_parts.is_empty();
    if same_path_exists {
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

    fn routes() -> Vec<Route<()>> {
        vec![
            Route {
                method: Method::Get,
                pattern: "/_bod/health",
                auth: Auth::Public,
                note: "存活探测，不泄露业务信息",
                handler: ping,
            },
            Route {
                method: Method::Get,
                pattern: "/_bod/events",
                auth: Auth::Bearer("BOD_TEST_DENIED"),
                note: "事件流含业务事实",
                handler: ping,
            },
            Route {
                method: Method::Post,
                pattern: "/api/v1/feedback-receipts/{id}/responses",
                auth: Auth::Bearer("BOD_TEST_DENIED"),
                note: "补充回执需身份",
                handler: ping,
            },
            // 静态路径注册在参数路径之后，用来验证「静态优先与顺序无关」。
            Route {
                method: Method::Get,
                pattern: "/api/v1/feedback-receipts/summary",
                auth: Auth::Bearer("BOD_TEST_DENIED"),
                note: "汇总视图",
                handler: ping,
            },
            Route {
                method: Method::Post,
                pattern: "/_bod/shutdown",
                auth: Auth::Bearer("BOD_TEST_DENIED"),
                note: "停机需会话密钥",
                handler: ping,
            },
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
