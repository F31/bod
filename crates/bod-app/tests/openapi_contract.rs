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
use bod_app::host_handshake::{contract_routes, openapi_document};
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

    let mut in_table: Vec<(String, String)> = contract_routes()
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
    for route in contract_routes() {
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
    for route in contract_routes() {
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

/// serde 与 schema 的漂移闸门。
///
/// 三件事一起查：序列化出来的键都在 schema 里；schema 必填的键都真的出现；
/// 类型一致。手写 schema 时失效的正是这三条。
fn assert_shape_matches(name: &str, sample: &Value, doc: &Value) {
    let schema = &doc["components"]["schemas"][name];
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
        let declared = properties
            .get(key)
            .unwrap_or_else(|| panic!("{name} 序列化出未声明的字段 {key}"));
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
