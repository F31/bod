//! 契约投影的回归测试（CR-004）。
//!
//! 这些用例检验的是**机制**，不是某份文档的内容：
//!
//! - 文档里的每个 `$ref` 都能解析，每个组件都被用到（没有死组件）；
//! - 401 的拒绝码与 `Auth` 携带的码逐字相等（两边不许各写一份）；
//! - schema 的字段集合、必填集合与类型，与「真的序列化一遍」的结果一致
//!   ——这是 serde 与 schemars 之间的漂移闸门，手写 schema 时最常在这里出错；
//! - `verification/openapi.json` 与生成结果逐字相等，且这个比较真的会失败
//!   （用一个故意改坏的副本证明它不是摆设）。
//!
//! 最后一条是重点：只看生成结果的测试无法证明自己没有空转。

use serde::Serialize;
use serde_json::{Value, json};

use bod_app::host_api::{ErrorResponse, HandshakeResponse, HealthResponse, ShutdownResponse};
use bod_app::host_handshake::{contract_routes, documented_routes, openapi_document};
use bod_app::http_router::Auth;

fn document() -> Value {
    openapi_document().expect("路由表应自洽")
}

fn render(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("文档应可序列化");
    text.push('\n');
    text
}

/// 递归收集文档里所有 `$ref` 指向的组件名。
fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(path)) = map.get("$ref") {
                let name = path
                    .rsplit('/')
                    .next()
                    .expect("$ref 至少有一段")
                    .to_string();
                out.push(name);
            }
            for nested in map.values() {
                collect_refs(nested, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        _ => {}
    }
}

#[test]
fn every_reference_resolves_to_a_component() {
    let doc = document();
    let mut refs = Vec::new();
    collect_refs(&doc, &mut refs);
    assert!(!refs.is_empty(), "文档里应有 schema 引用");

    let components = doc["components"]["schemas"]
        .as_object()
        .expect("应有 components.schemas");
    for name in refs {
        assert!(
            components.contains_key(&name),
            "$ref 指向不存在的组件 {name}"
        );
    }
}

/// 反向：组件表里没有没人用的死 schema。
#[test]
fn every_component_is_referenced() {
    let doc = document();
    let mut refs = Vec::new();
    collect_refs(&doc, &mut refs);
    let components = doc["components"]["schemas"]
        .as_object()
        .expect("应有 components.schemas");
    for name in components.keys() {
        assert!(
            refs.iter().any(|used| used == name),
            "组件 {name} 没有任何端点引用"
        );
    }
}

/// 每个声明的路径都必须来自 `ROUTES`，且每个 `ROUTES` 条目都在文档里。
#[test]
fn paths_and_methods_match_the_route_table() {
    let doc = document();
    let paths = doc["paths"].as_object().expect("应有 paths");

    let mut in_doc: Vec<(String, String)> = Vec::new();
    for (path, item) in paths {
        for method in item.as_object().expect("路径项应是对象").keys() {
            in_doc.push((method.clone(), path.clone()));
        }
    }

    let mut in_table: Vec<(String, String)> = documented_routes()
        .iter()
        .map(|route| {
            (
                route.method.as_str().to_ascii_lowercase(),
                route.pattern.to_string(),
            )
        })
        .collect();

    in_doc.sort();
    in_table.sort();
    assert_eq!(in_doc, in_table, "文档与路由表的 (方法, 路径) 集合必须相等");
}

/// 401 的码只能来自 `Auth`：这条把「两处各写一份」变成编译期就写不出来的形状。
#[test]
fn rejection_codes_come_from_the_route_itself() {
    let doc = document();
    for route in documented_routes() {
        let code = match route.auth {
            Auth::Public => continue,
            Auth::Bearer(code) | Auth::InHandler(code) => code,
        };
        let operation = &doc["paths"][route.pattern][route.method.as_str().to_ascii_lowercase()];
        let example =
            &operation["responses"]["401"]["content"]["application/json"]["example"]["error"];
        assert_eq!(example, code, "{} 的 401 码不一致", route.pattern);
    }
}

/// 所有端点都存在 404：这是分发层的真实行为，不是可选项。
#[test]
fn every_operation_documents_the_dispatch_404() {
    let doc = document();
    for route in documented_routes() {
        let operation = &doc["paths"][route.pattern][route.method.as_str().to_ascii_lowercase()];
        assert_eq!(
            operation["responses"]["404"]["content"]["application/json"]["example"]["error"],
            "BOD_NOT_FOUND",
            "{} 缺少 404",
            route.pattern
        );
    }
}

/// Bearer 档必须声明标准安全方案；InHandler 档不得声明。
#[test]
fn security_scheme_follows_the_auth_kind() {
    let doc = document();
    assert!(
        doc["components"]["securitySchemes"]["bearerAuth"].is_object(),
        "有 Bearer 路由就必须声明 bearerAuth"
    );
    for route in contract_routes() {
        let operation = &doc["paths"][route.pattern][route.method.as_str().to_ascii_lowercase()];
        let has_security = operation.get("security").is_some();
        assert_eq!(
            has_security,
            matches!(route.auth, Auth::Bearer(_)),
            "{} 的 security 声明与 Auth 档位不符",
            route.pattern
        );
    }
}

/// 产品接口与控制面按前缀分表并存，且两张表之间没有重复路由。
///
/// 分发层按 `delivery_api::PREFIX` 选表。前缀与路由表一旦不一致，请求会被交给
/// 错误的表：要么 404，要么用错鉴权级别——所以这条断言查的是机制，不是排版。
#[test]
fn surfaces_are_split_by_prefix_without_overlap() {
    use bod_app::delivery_api::{PREFIX, routes as product_routes};

    assert!(!product_routes().is_empty(), "产品接口路由表不应为空");
    for route in product_routes() {
        assert!(
            route.pattern.starts_with(PREFIX),
            "{} 不在 {PREFIX} 下，分发层不会把它交给产品接口表",
            route.pattern
        );
    }
    for route in contract_routes() {
        assert!(
            !route.pattern.starts_with(PREFIX),
            "控制面路由 {} 落在产品接口前缀下，会被错误地交出去",
            route.pattern
        );
    }

    // 两张表拼起来不得出现同一个 (方法, 路径)：否则后写入的会静默覆盖前者。
    let mut all: Vec<(String, String)> = contract_routes()
        .iter()
        .chain(product_routes().iter())
        .map(|route| {
            (
                route.method.as_str().to_ascii_lowercase(),
                route.pattern.to_string(),
            )
        })
        .collect();
    let before = all.len();
    all.sort();
    all.dedup();
    assert_eq!(before, all.len(), "两张路由表出现重复的 (方法, 路径)");
}

/// 文档必须把「路径已冻结」与「已经能用」分开：计划中操作标 `planned` 且给 501，
/// 已服务操作不得出现 501。这条挡住「把未实现悄悄当已实现」。
#[test]
fn planned_operations_are_marked_and_explain_the_501() {
    use bod_app::http_router::Implementation;

    let doc = document();
    let mut planned = 0;
    for route in documented_routes() {
        let operation = &doc["paths"][route.pattern][route.method.as_str().to_ascii_lowercase()];
        assert_eq!(
            operation["x-bod-status"],
            route.contract.status.as_str(),
            "{} 的 x-bod-status 与路由表不符",
            route.pattern
        );
        let has_501 = operation["responses"].get("501").is_some();
        assert_eq!(
            has_501,
            matches!(route.contract.status, Implementation::Planned),
            "{} 的 501 声明与落地状态不一致",
            route.pattern
        );
        if matches!(route.contract.status, Implementation::Planned) {
            planned += 1;
        }
    }
    assert_eq!(
        planned, 5,
        "§11 的 6 个操作里，只有 GET /delivery-tracking/{{id}} 已落地（P8 3.6 B5）；\
         其余 5 个仍须如实标 planned。数字变了就要回头读 tasks.md，别直接改这里"
    );
}

/// serde 与 schema 的漂移闸门。
///
/// 三件事一起查：序列化出来的键都在 schema 里；schema 必填的键都真的出现；
/// 类型一致。手写 schema 时失效的正是这三条。
///
/// 字段指向子类型时 schema 里是 `$ref`（不是缺 `type`），所以先按客户端的方式解析。
fn resolve<'a>(schema: &'a Value, doc: &'a Value) -> &'a Value {
    let mut current = schema;
    for _ in 0..8 {
        let Some(path) = current.get("$ref").and_then(Value::as_str) else {
            return current;
        };
        let name = path.rsplit('/').next().unwrap_or_default();
        match doc["components"]["schemas"].get(name) {
            Some(next) => current = next,
            None => panic!("$ref 指向不存在的组件 {name}"),
        }
    }
    panic!("$ref 嵌套过深：{schema}");
}

fn assert_shape_matches(name: &str, sample: &Value, doc: &Value) {
    let schema = resolve(&doc["components"]["schemas"][name], doc);
    let properties = schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{name} 应有 properties"));
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let sample = sample
        .as_object()
        .unwrap_or_else(|| panic!("{name} 的样本应是对象"));

    for (key, value) in sample {
        let declared = resolve(
            properties
                .get(key)
                .unwrap_or_else(|| panic!("{name} 序列化出未声明的字段 {key}")),
            doc,
        );
        let expected = match value {
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            _ => continue,
        };
        // 可空字段在 OAS 3.1 里是 `"type": ["string","null"]` 而不是 `nullable`，
        // 所以「含期望类型」就算一致，不能要求单值。
        let declared_types: Vec<&str> = match &declared["type"] {
            Value::String(one) => vec![one.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            other => panic!("{name}.{key} 的 type 既不是字符串也不是数组：{other}"),
        };
        assert!(
            declared_types.contains(&expected),
            "{name}.{key} 的 schema 类型 {declared_types:?} 与序列化结果 {expected} 不符"
        );
    }
    for key in &required {
        assert!(
            sample.contains_key(*key),
            "{name}.{key} 被标为必填，但样本里没有——要么 schema 写错，要么 skip 条件漏了"
        );
    }
    for key in properties.keys() {
        if !sample.contains_key(key) {
            assert!(
                !required.contains(&key.as_str()),
                "{name}.{key} 必填但未出现在样本中"
            );
        }
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("样本应可序列化")
}

#[test]
fn schemas_match_what_the_types_actually_serialize() {
    let doc = document();

    assert_shape_matches(
        "HealthResponse",
        &to_value(&HealthResponse {
            ok: true,
            protocol: "bod-host/v1",
            instance_id: "inst-abc".to_string(),
            pid: 4242,
        }),
        &doc,
    );
    assert_shape_matches(
        "HandshakeResponse",
        &to_value(&HandshakeResponse {
            protocol: "bod-host/v1",
            instance_id: "inst-abc".to_string(),
            pid: 4242,
            session_key: "secret".to_string(),
        }),
        &doc,
    );
    assert_shape_matches(
        "ShutdownResponse",
        &to_value(&ShutdownResponse { stopping: true }),
        &doc,
    );
    // 请求体没有 `Serialize`（生产不需要），用等价字面量比对。
    assert_shape_matches("HandshakeRequest", &json!({ "handshake_token": "t" }), &doc);
    // 两种错误形态都要过一遍：没 reason 时不得把 reason 标成必填。
    assert_shape_matches(
        "ErrorResponse",
        &to_value(&ErrorResponse::code("BOD_NOT_FOUND")),
        &doc,
    );
    assert_shape_matches(
        "ErrorResponse",
        &to_value(&ErrorResponse::with_reason("BOD_STREAM_DENIED", "解释")),
        &doc,
    );
}

/// §11 的请求/响应体也要过同一道漂移闸门。
///
/// 这些类型尚无 handler，但 schema 已经冻结——冻结的若是错形状，客户端会按错的
/// 契约生成代码。所以这里用真实值序列化一遍，再与 schema 比。
#[test]
fn product_schemas_match_what_the_types_serialize() {
    use bod_app::delivery_api::{
        CoverageVerdictCode, DeliveryImportResponse, DeliveryLinkResponse,
        DeliveryObservationResponse, DeliveryStatus, DeliveryTrackingView, DraftCitation,
        DraftResponse, DraftSendResponse, DraftStatus, EvidenceStamp, TargetCoverage, TargetRef,
    };

    let doc = document();
    let target = TargetRef {
        customer_id: "cust-a".to_string(),
        environment: "prod".to_string(),
        deployment_unit: None,
    };
    let coverage = TargetCoverage {
        target: target.clone(),
        verdict: CoverageVerdictCode::PendingVerification,
        reason: Some("缺部署观察".to_string()),
        evidence_revision: None,
        observed_at: None,
    };

    assert_shape_matches(
        "DeliveryImportResponse",
        &to_value(&DeliveryImportResponse {
            tracking_id: "trk-1".to_string(),
            created: true,
            targets: vec![coverage.clone()],
            gaps: vec!["no-artifact-mapping".to_string()],
        }),
        &doc,
    );
    assert_shape_matches(
        "DeliveryLinkResponse",
        &to_value(&DeliveryLinkResponse {
            tracking_id: "trk-1".to_string(),
            link_id: "lnk-1".to_string(),
            appended: false,
            version: 3,
        }),
        &doc,
    );
    assert_shape_matches(
        "DeliveryTrackingView",
        &to_value(&DeliveryTrackingView {
            tracking_id: "trk-1".to_string(),
            project_id: "proj-1".to_string(),
            version: 3,
            owner: None,
            targets: vec![coverage.clone()],
            evidence: vec![EvidenceStamp {
                revision: "rev-9".to_string(),
                source: "connector".to_string(),
                observed_at: "2026-11-21T00:00:00Z".to_string(),
            }],
            gaps: Vec::new(),
            allowed_actions: vec!["link-source".to_string(), "draft".to_string()],
        }),
        &doc,
    );
    assert_shape_matches(
        "DeliveryObservationResponse",
        &to_value(&DeliveryObservationResponse {
            observation_id: "obs-1".to_string(),
            applied: true,
            affected_targets: vec![coverage.clone()],
        }),
        &doc,
    );
    assert_shape_matches(
        "DraftResponse",
        &to_value(&DraftResponse {
            draft_id: "draft-1".to_string(),
            revision: 2,
            evidence_revision: "rev-9".to_string(),
            citations: vec![DraftCitation {
                evidence_revision: "rev-9".to_string(),
                reference: "receipt-7".to_string(),
                observed_at: "2026-11-21T00:00:00Z".to_string(),
            }],
            status: DraftStatus::PendingReview,
            created_at: "2026-11-21T00:01:00Z".to_string(),
        }),
        &doc,
    );
    // 递交成功只表示意图已登记：送达状态只能是 unknown。
    assert_shape_matches(
        "DraftSendResponse",
        &to_value(&DraftSendResponse {
            send_intent_id: "send-1".to_string(),
            registered: true,
            delivery_status: DeliveryStatus::Unknown,
        }),
        &doc,
    );

    // 请求体没有 `Serialize` 之外的构造需求，用等价字面量比对必填面。
    assert_shape_matches(
        "DeliveryImportRequest",
        &json!({
            "project_id": "proj-1",
            "source": {
                "connection_id": "conn-1",
                "object_type": "pull_request",
                "object_id": "4711",
                "observed_at": "2026-11-21T00:00:00Z"
            },
            "targets": [{ "customer_id": "cust-a", "environment": "prod" }],
            "acceptance": "验收条款 v3",
            "idempotency_key": "key-1"
        }),
        &doc,
    );
    assert_shape_matches(
        "DeliveryLinkRequest",
        &json!({
            "source": {
                "connection_id": "conn-1",
                "object_type": "pull_request",
                "object_id": "4711",
                "observed_at": "2026-11-21T00:00:00Z"
            },
            "targets": [{ "customer_id": "cust-a", "environment": "prod" }],
            "expected_version": 3,
            "idempotency_key": "key-2"
        }),
        &doc,
    );
    assert_shape_matches(
        "DeliveryObservationRequest",
        &json!({
            "project_id": "proj-1",
            "targets": [{ "customer_id": "cust-a", "environment": "prod" }],
            "source": {
                "connection_id": "conn-1",
                "object_type": "build",
                "object_id": "b-9",
                "observed_at": "2026-11-21T00:00:00Z"
            },
            "source_sequence": 7,
            "kind": "deployment",
            "note": "部署到 prod",
            "idempotency_key": "key-3"
        }),
        &doc,
    );
    assert_shape_matches(
        "DraftCreateRequest",
        &json!({
            "target": { "customer_id": "cust-a", "environment": "prod" },
            "purpose": "fixed-notice",
            "evidence_revision": "rev-9",
            "references": ["receipt-7"],
            "gaps": [],
            "text": ""
        }),
        &doc,
    );
    assert_shape_matches(
        "DraftSendRequest",
        &json!({
            "text_revision": 2,
            "evidence_revision": "rev-9",
            "target_channel": "chan-1",
            "idempotency_key": "key-4"
        }),
        &doc,
    );
}

/// 入库的文档必须与生成结果逐字相同。
///
/// 改了 DTO 或路由却没重新生成，这条会红——这正是「文档不会漂移」的实现方式。
/// 重生成：`BOD_BLESS_OPENAPI=1 cargo test -p bod-app --test openapi_contract`
#[test]
fn checked_in_document_matches_generated_one() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../verification/openapi.json");
    let generated = render(&document());

    if std::env::var("BOD_BLESS_OPENAPI").is_ok() {
        std::fs::write(&path, &generated).expect("应能写入 verification/openapi.json");
        return;
    }

    let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "读不到 {}：{err}。先跑 BOD_BLESS_OPENAPI=1 cargo test -p bod-app --test openapi_contract",
            path.display()
        )
    });
    assert_eq!(
        checked_in, generated,
        "verification/openapi.json 与生成结果不一致：改了契约就要重新生成并一并提交"
    );

    // 证明比较不是空转：故意改一个字段，比较必须失败。
    let mut mutated = document();
    mutated["paths"]["/_bod/health"]["get"]["summary"] = json!("被改坏的摘要");
    assert_ne!(
        render(&mutated),
        checked_in,
        "改坏的副本也应与入库文档不同，否则这个测试查不出漂移"
    );
}
