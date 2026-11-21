//! OpenAPI 3.1 契约投影（CR-004）：从 `ROUTES` 生成文档，而不是另写一份。
//!
//! 这一层存在的唯一理由是**消除第二份事实源**。在此之前，「文档说的」和
//! 「路由表做的」只能靠人工比对，而人工比对在第十个端点之后必然失效。
//!
//! 三条硬规则的落地方式：
//!
//! 1. **路径只写一次**：`paths` 的键直接来自 `Route::pattern`。这里不出现任何
//!    路径字面量，也不允许调用方传入路径列表。
//! 2. **拒绝码只写一次**：401 的 `error` 例子来自 `Auth` 携带的码
//!    （`Auth::Bearer(code)` / `Auth::InHandler(code)`），契约里再写一遍会被测试拒绝。
//! 3. **schema 从类型派生**：走 `schemars` 的 `JsonSchema`，不手写字段表。
//!    手写表的典型失效方式是「字段加了、schema 忘了」，而那正是要从机制上消灭的。
//!
//! 为什么不用 `utoipa`（详见 `adr/0004-openapi-single-source.md`）：它的
//! `#[utoipa::path]` 宏要把 method/path/状态码写在 handler 上，等于把 `ROUTES`
//! 已有的元数据抄第二遍；它的收益（框架集成、Swagger UI）在本仓库没有使用者。
//! 真正难的部分是 schema 派生，那部分已交给 `schemars`。
//!
//! 文档是**投影**不是真相：改契约要改 `ROUTES` 与 DTO，然后重新生成
//! `verification/openapi.json`（`BOD_BLESS_OPENAPI=1` 见集成测试）。

use serde_json::{Map, Value, json};

use crate::http_router::{Auth, ContractBody, Method, Route};

/// 本目录生成的文档版本。OAS 3.1 的 JSON Schema 方言与 schemars 1.x 一致
/// （draft 2020-12），所以不需要在组件里重复声明 `$schema`。
pub const OPENAPI_VERSION: &str = "3.1.0";

/// 把 DTO 的类型信息投影成 JSON Schema。
///
/// `fn() -> Value` 的函数项可以直接放进 `static`，这也是 `ContractBody` 用函数
/// 指针而不是 `Value` 存 schema 的原因：路由表是 `static`，无法在编译期求值序列化。
pub fn schema_of<T: schemars::JsonSchema>() -> Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T))
        .expect("schema 必须可序列化：它本身就是 JSON");
    if let Value::Object(map) = &mut value {
        // OAS 3.1 已声明方言，组件内再写 `$schema` 会与文档级声明重复。
        map.remove("$schema");
    }
    value
}

/// 文档头信息。只放稳定事实：标题、版本与一句边界说明。
pub struct Info {
    pub title: &'static str,
    pub version: &'static str,
    pub description: &'static str,
}

/// 从路由表生成完整文档。
///
/// 返回 `Err` 的条件都是「路由表自身不自洽」——重复的 method+pattern、
/// 悬空的组件引用。这些必须能被测试单独构造出来验证检查不是空转，
/// 所以用返回值而不是 `panic`。
pub fn document<S>(routes: &[Route<S>], info: Info) -> Result<Value, String> {
    let mut paths: Map<String, Value> = Map::new();
    // 组件名 → schema。同名不同内容视为冲突：说明有人给两个类型起了同一个名字。
    let mut schemas: Map<String, Value> = Map::new();
    let mut operation_ids: Vec<String> = Vec::new();
    let mut uses_bearer = false;

    for route in routes {
        let method_key = route.method.as_str().to_ascii_lowercase();
        let operation_id = operation_id(route.method, route.pattern);

        // 先判「同一 method+pattern 重复」，再判 operationId：前者是更精确的原因。
        // 两条都会命中同一条重复路由，报错要说人话。
        let entry = paths
            .entry(route.pattern.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(entry) = entry else {
            return Err(format!("路径项不是对象：{}", route.pattern));
        };
        if entry.contains_key(&method_key) {
            return Err(format!("重复路由：{method_key} {}", route.pattern));
        }

        // operationId 冲突另算：不同路径可能归一化成同一个 id（`/a/b` 与 `/a-b`）。
        if operation_ids.iter().any(|seen| seen == &operation_id) {
            return Err(format!("operationId 重复：{operation_id}"));
        }
        operation_ids.push(operation_id.clone());

        // 请求体 schema
        if let Some(ContractBody::Json { component, schema }) = route.contract.request {
            register(&mut schemas, component, schema())?;
        }

        // 声明的响应
        let mut responses: Map<String, Value> = Map::new();
        for declared in route.contract.responses {
            if declared.status == 401 || declared.status == 404 {
                // 这两个码由 Auth 与分发层统一产生，契约里再写一遍就会与机制漂移。
                return Err(format!(
                    "{} {} 的 {} 由框架自动生成，不得在 responses 里重复声明",
                    route.method.as_str(),
                    route.pattern,
                    declared.status
                ));
            }
            if let Some(ContractBody::Json { component, schema }) = declared.body {
                register(&mut schemas, component, schema())?;
            }
            responses.insert(
                declared.status.to_string(),
                response_object(declared.description, declared.body),
            );
        }

        // 401：码来自 Auth，例子也从 Auth 取——两边不许各写一份。
        match route.auth {
            Auth::Public => {}
            Auth::Bearer(code) | Auth::InHandler(code) => {
                uses_bearer |= matches!(route.auth, Auth::Bearer(_));
                register(
                    &mut schemas,
                    "ErrorResponse",
                    schema_of::<crate::host_api::ErrorResponse>(),
                )?;
                responses.insert(
                    "401".to_string(),
                    error_response(
                        "未通过鉴权；`error` 为本端点固定的拒绝码，客户端按码分支",
                        code,
                    ),
                );
            }
        }

        // 404：分发层对所有未匹配请求都答这个，属于每个端点的真实行为。
        register(
            &mut schemas,
            "ErrorResponse",
            schema_of::<crate::host_api::ErrorResponse>(),
        )?;
        responses.insert(
            "404".to_string(),
            error_response("路径不存在或方法不匹配；两者对外同答 404", "BOD_NOT_FOUND"),
        );

        let mut operation = Map::new();
        operation.insert("operationId".to_string(), json!(operation_id));
        operation.insert("summary".to_string(), json!(route.contract.summary));
        operation.insert("description".to_string(), json!(route.note));
        operation.insert(
            "x-bod-auth".to_string(),
            json!(match route.auth {
                Auth::Public => "public",
                Auth::Bearer(_) => "bearer",
                Auth::InHandler(_) => "in-handler",
            }),
        );

        let parameters = parameters_of(route);
        if !parameters.is_empty() {
            operation.insert("parameters".to_string(), Value::Array(parameters));
        }
        if let Some(ContractBody::Json { component, .. }) = route.contract.request {
            operation.insert(
                "requestBody".to_string(),
                json!({
                    "required": true,
                    "content": { "application/json": { "schema": { "$ref": format!("#/components/schemas/{component}") } } }
                }),
            );
        }
        operation.insert(
            "responses".to_string(),
            Value::Object(responses.into_iter().collect()),
        );
        // 只有 Bearer 档能在这里声明标准安全方案；InHandler 档的令牌位置各不相同
        // （握手在请求体、事件流在头），写进 `security` 会造成「文档比事实更强」。
        if matches!(route.auth, Auth::Bearer(_)) {
            operation.insert("security".to_string(), json!([{ "bearerAuth": [] }]));
        }

        entry.insert(method_key, Value::Object(operation));
    }

    let mut components = Map::new();
    if !schemas.is_empty() {
        components.insert("schemas".to_string(), Value::Object(schemas));
    }
    if uses_bearer {
        components.insert(
            "securitySchemes".to_string(),
            json!({
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "控制面会话密钥，由 POST /_bod/handshake 取得；不发 CORS 预检"
                }
            }),
        );
    }

    let mut document = Map::new();
    document.insert("openapi".to_string(), json!(OPENAPI_VERSION));
    document.insert(
        "info".to_string(),
        json!({
            "title": info.title,
            "version": info.version,
            "description": info.description,
        }),
    );
    document.insert("paths".to_string(), Value::Object(paths));
    if !components.is_empty() {
        document.insert("components".to_string(), Value::Object(components));
    }
    Ok(Value::Object(document))
}

/// 登记一个组件；同名必须同内容，否则是命名冲突而不是重复。
fn register(
    schemas: &mut Map<String, Value>,
    component: &str,
    schema: Value,
) -> Result<(), String> {
    match schemas.get(component) {
        None => {
            schemas.insert(component.to_string(), schema);
            Ok(())
        }
        Some(existing) if existing == &schema => Ok(()),
        Some(_) => Err(format!("组件 {component} 被两个不同的 schema 占用")),
    }
}

/// 一个响应对象：有 body 时带 content，没有时只留 description。
fn response_object(description: &str, body: Option<ContractBody>) -> Value {
    match body {
        None => json!({ "description": description }),
        Some(ContractBody::Json { component, .. }) => json!({
            "description": description,
            "content": { "application/json": { "schema": { "$ref": format!("#/components/schemas/{component}") } } }
        }),
        // 事件流不建模成 JSON：写 `application/json` 会骗客户端去解析帧。
        Some(ContractBody::EventStream) => json!({
            "description": description,
            "content": { "text/event-stream": { "schema": { "type": "string" } } }
        }),
    }
}

fn error_response(description: &str, code: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {
                "schema": { "$ref": "#/components/schemas/ErrorResponse" },
                "example": { "error": code }
            }
        }
    })
}

/// 路径参数与查询参数。
///
/// 路径参数的唯一来源是模式里的 `{name}`：再声明一份「有哪些路径参数」等于
/// 允许两处不一致。查询参数无法从模式推出（它们是 handler 自己读的），
/// 所以必须显式声明，且同样随契约一起被测试覆盖。
fn parameters_of<S>(route: &Route<S>) -> Vec<Value> {
    let mut out = Vec::new();
    for name in crate::http_router::pattern_parameters(route.pattern) {
        out.push(json!({
            "name": name,
            "in": "path",
            "required": true,
            "description": "路径参数原样透传，不做百分号解码；不得直接拼路径或 SQL",
            "schema": { "type": "string" }
        }));
    }
    for query in route.contract.query {
        out.push(json!({
            "name": query.name,
            "in": "query",
            "required": query.required,
            "description": query.description,
            "schema": { "type": "string" }
        }));
    }
    out
}

/// `operationId` 必须稳定且唯一：客户端生成函数名会直接用它。
fn operation_id(method: Method, pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    out.push_str(&method.as_str().to_ascii_lowercase());
    let mut last_underscore = false;
    for ch in pattern.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_router::{Contract, ContractResponse, RouteError};
    use crate::http_router::{EndpointRequest, Reply};

    fn ping(_state: &(), _request: EndpointRequest) -> Result<Reply, RouteError> {
        Ok(Reply::ok(json!({ "ok": true })))
    }

    fn info() -> Info {
        Info {
            title: "test",
            version: "v1",
            description: "test",
        }
    }

    fn route(pattern: &'static str, method: Method, auth: Auth) -> Route<()> {
        Route {
            method,
            pattern,
            auth,
            note: "测试用路由",
            contract: Contract {
                summary: "测试",
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

    #[test]
    fn operation_id_is_stable_and_readable() {
        assert_eq!(operation_id(Method::Get, "/_bod/health"), "get_bod_health");
        assert_eq!(
            operation_id(Method::Post, "/api/v1/delivery-tracking/{id}/links"),
            "post_api_v1_delivery_tracking_id_links"
        );
        assert_eq!(operation_id(Method::Get, "/"), "get");
    }

    #[test]
    fn duplicate_method_and_pattern_is_rejected() {
        let routes = vec![
            route("/_bod/health", Method::Get, Auth::Public),
            route("/_bod/health", Method::Get, Auth::Public),
        ];
        let err = document(&routes, info()).expect_err("重复路由必须被拒绝");
        assert!(err.contains("重复路由"), "{err}");
    }

    /// 两条不同路径可能归一化成同一个 operationId：客户端按 id 生成函数名，撞了就是坏契约。
    #[test]
    fn operation_id_collision_is_rejected() {
        let routes = vec![
            route("/api/v1/a/b", Method::Get, Auth::Public),
            route("/api/v1/a-b", Method::Get, Auth::Public),
        ];
        let err = document(&routes, info()).expect_err("operationId 冲突必须被拒绝");
        assert!(err.contains("operationId 重复"), "{err}");
    }

    /// 401 与 404 由机制生成，契约里重复声明必须报错——否则两份码会漂移。
    #[test]
    fn declaring_auth_response_in_contract_is_rejected() {
        let routes = vec![Route {
            method: Method::Get,
            pattern: "/_bod/x",
            auth: Auth::Bearer("BOD_X_DENIED"),
            note: "测试用",
            contract: Contract {
                summary: "测试",
                request: None,
                query: &[],
                responses: &[ContractResponse {
                    status: 401,
                    description: "手写的 401",
                    body: None,
                }],
            },
            handler: ping,
        }];
        let err = document(&routes, info()).expect_err("手写 401 必须被拒绝");
        assert!(err.contains("自动生成"), "{err}");
    }

    #[test]
    fn auth_code_lands_in_the_401_example() {
        let routes = vec![route(
            "/_bod/shutdown",
            Method::Post,
            Auth::Bearer("BOD_SHUTDOWN_DENIED"),
        )];
        let doc = document(&routes, info()).expect("应能生成");
        let example = &doc["paths"]["/_bod/shutdown"]["post"]["responses"]["401"]["content"]["application/json"]
            ["example"]["error"];
        assert_eq!(example, "BOD_SHUTDOWN_DENIED");
    }

    #[test]
    fn in_handler_auth_does_not_claim_bearer_scheme() {
        let routes = vec![route(
            "/_bod/handshake",
            Method::Post,
            Auth::InHandler("BOD_HANDSHAKE_DENIED"),
        )];
        let doc = document(&routes, info()).expect("应能生成");
        let operation = &doc["paths"]["/_bod/handshake"]["post"];
        assert!(operation.get("security").is_none(), "不得声明 Bearer");
        assert_eq!(operation["x-bod-auth"], "in-handler");
        // 不声明 Bearer，就不该有 securitySchemes。
        assert!(
            doc.get("components")
                .and_then(|c| c.get("securitySchemes"))
                .is_none()
        );
    }

    #[test]
    fn path_parameters_come_from_the_pattern() {
        let routes = vec![route(
            "/api/v1/feedback-receipts/{id}/drafts/{draft_id}",
            Method::Post,
            Auth::Bearer("BOD_DENIED"),
        )];
        let doc = document(&routes, info()).expect("应能生成");
        let params =
            doc["paths"]["/api/v1/feedback-receipts/{id}/drafts/{draft_id}"]["post"]["parameters"]
                .as_array()
                .expect("应有参数数组");
        let names: Vec<&str> = params
            .iter()
            .map(|param| param["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["id", "draft_id"]);
        assert!(params.iter().all(|param| param["in"] == "path"));
        assert!(params.iter().all(|param| param["required"] == true));
    }

    #[test]
    fn event_stream_response_is_not_declared_as_json() {
        let routes = vec![Route {
            method: Method::Get,
            pattern: "/_bod/events",
            auth: Auth::Bearer("BOD_STREAM_DENIED"),
            note: "测试用",
            contract: Contract {
                summary: "事件流",
                request: None,
                query: &[],
                responses: &[ContractResponse {
                    status: 200,
                    description: "SSE",
                    body: Some(ContractBody::EventStream),
                }],
            },
            handler: ping,
        }];
        let doc = document(&routes, info()).expect("应能生成");
        let content = &doc["paths"]["/_bod/events"]["get"]["responses"]["200"]["content"];
        assert!(content.get("application/json").is_none());
        assert!(content.get("text/event-stream").is_some());
    }

    /// 反向验证 `register`：同名不同内容必须报错，否则冲突会被静默覆盖。
    #[test]
    fn conflicting_component_schemas_are_rejected() {
        let mut schemas = Map::new();
        register(&mut schemas, "X", json!({ "type": "string" })).expect("首次登记");
        register(&mut schemas, "X", json!({ "type": "string" })).expect("内容相同应放行");
        let err = register(&mut schemas, "X", json!({ "type": "integer" }))
            .expect_err("内容不同必须报错");
        assert!(err.contains("占用"), "{err}");
    }
}
