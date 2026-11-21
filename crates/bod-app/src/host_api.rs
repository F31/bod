//! 控制面 HTTP 的请求/响应体（CR-004 的共同前置：先有类型，再谈 schema 与 OpenAPI）。
//!
//! 抽出之前，每个 handler 各自用 `serde_json::json!({...})` 拼响应。问题是：
//! 字段名、顺序、必填/可选只存在于那一行的字面量里——没有类型可以挂
//! `ToSchema`，也没有地方能断言「文档与实现一致」。§6 与 §11 合计还有 10 个
//! 操作要落地，先把这层立起来比事后补便宜得多。
//!
//! **本次不改对外字节**：字段顺序按原 `json!` 的插入顺序声明——`serde` 按声明序
//! 输出，`serde_json::Value` 比较则会忽略顺序，所以测试用**序列化字符串**断言，
//! 不用 `Value` 相等，否则顺序变了也发现不了。
//!
//! 尚未覆盖的部分（下一步再做，不在本模块假装完成）：
//! - §6/§11 的业务响应：端点还没写，无类型可抽。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::http_router::RouteError;

/// `GET /_bod/health` 的响应。
///
/// 只回存活事实：不含会话密钥、不含业务数据。`ok` 恒为 `true`——探测失败时
/// 根本没有响应，所以这里不需要「false 的 ok」。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HealthResponse {
    pub ok: bool,
    pub protocol: &'static str,
    pub instance_id: String,
    pub pid: u32,
}

/// `POST /_bod/handshake` 的请求体。
///
/// 令牌在**请求体**而不是 `Authorization` 头：调用这一刻客户端还没有会话密钥，
/// 头部放不进去。缺字段、类型不对、JSON 畸形一律落到同一个 401——不区分原因，
/// 免得把「令牌错」与「请求畸形」的差别变成探测手段。
///
/// 不设 `deny_unknown_fields`：旧客户端可能带多余字段，加严会是破坏性变更。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HandshakeRequest {
    pub handshake_token: String,
}

/// `POST /_bod/handshake` 的响应。
///
/// 这是唯一会把 `session_key` 写进响应体的地方。字段顺序与旧字面量一致，
/// 但**顺序不是契约**——客户端按名取值。测试锁顺序只是为了证明重构没改字节。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HandshakeResponse {
    pub protocol: &'static str,
    pub instance_id: String,
    pub pid: u32,
    pub session_key: String,
}

/// `POST /_bod/shutdown` 的响应。
///
/// `stopping: true` 只表示「授权停止已置位」，不表示进程已经退出——
/// 退出发生在 `run` 的下一轮循环。客户端要确认退出，应看描述文件是否消失。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShutdownResponse {
    pub stopping: bool,
}

/// 统一错误体。
///
/// 两种形态共用一个类型：
/// - 带解释：`{"error":"BOD_HOST_REJECTED","reason":"..."}`，用于需要区分
///   「为什么拒」的场景（Host/Origin 校验、事件流授权）；
/// - 只有码：`{"error":"BOD_NOT_FOUND"}`，用于码本身已经说清的场景。
///
/// `reason` 用 `skip_serializing_if` 而不是输出 `null`：既有客户端按「字段存在与否」
/// 判断是否有解释，补一个 `null` 会改变它们的解析结果。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ErrorResponse {
    pub error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ErrorResponse {
    /// 只有错误码。
    pub fn code(error: &'static str) -> Self {
        Self {
            error,
            reason: None,
        }
    }

    /// 错误码 + 人可读解释。解释里**不得**含凭据、令牌或客户数据。
    pub fn with_reason(error: &'static str, reason: impl Into<String>) -> Self {
        Self {
            error,
            reason: Some(reason.into()),
        }
    }
}

/// 把 DTO 编码成 `Reply` 能接受的值。
///
/// 序列化失败只可能是 DTO 自己写了会失败的 `Serialize` 实现（例如 map 的 key
/// 不是字符串），属于编程错误而不是请求错误：返回 500 而不是 400，避免把
/// 服务端缺陷说成客户端问题。
pub(crate) fn encode<T: Serialize>(body: &T) -> Result<serde_json::Value, RouteError> {
    serde_json::to_value(body).map_err(|_| RouteError::new(500, "BOD_ENCODE_FAILED"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render<T: Serialize>(body: &T) -> String {
        serde_json::to_string(body).expect("DTO 应可序列化")
    }

    /// 与重构前的 `json!` 逐字节比对：字段顺序变了也算回归。
    #[test]
    fn health_body_is_byte_identical_to_previous_literal() {
        let body = HealthResponse {
            ok: true,
            protocol: "bod-host/v1",
            instance_id: "inst-abc".to_string(),
            pid: 4242,
        };
        assert_eq!(
            render(&body),
            r#"{"ok":true,"protocol":"bod-host/v1","instance_id":"inst-abc","pid":4242}"#
        );
    }

    #[test]
    fn handshake_body_is_byte_identical_to_previous_literal() {
        let body = HandshakeResponse {
            protocol: "bod-host/v1",
            instance_id: "inst-abc".to_string(),
            pid: 4242,
            session_key: "session-secret".to_string(),
        };
        assert_eq!(
            render(&body),
            r#"{"protocol":"bod-host/v1","instance_id":"inst-abc","pid":4242,"session_key":"session-secret"}"#
        );
    }

    #[test]
    fn shutdown_body_is_byte_identical_to_previous_literal() {
        assert_eq!(
            render(&ShutdownResponse { stopping: true }),
            r#"{"stopping":true}"#
        );
    }

    /// 只有码时不输出 `reason` 字段，而不是输出 `null`。
    #[test]
    fn error_without_reason_omits_the_field() {
        assert_eq!(
            render(&ErrorResponse::code("BOD_NOT_FOUND")),
            r#"{"error":"BOD_NOT_FOUND"}"#
        );
        assert!(!render(&ErrorResponse::code("BOD_NOT_FOUND")).contains("reason"));
    }

    #[test]
    fn error_with_reason_keeps_field_order() {
        let body = ErrorResponse::with_reason("BOD_HOST_REJECTED", "Host 不在白名单");
        assert_eq!(
            render(&body),
            r#"{"error":"BOD_HOST_REJECTED","reason":"Host 不在白名单"}"#
        );
    }

    #[test]
    fn encode_returns_json_value_for_reply() {
        let value = encode(&ShutdownResponse { stopping: true }).expect("应编码成功");
        assert_eq!(value, serde_json::json!({ "stopping": true }));
    }

    /// 解释文本要经过 JSON 转义，不能靠 `json!` 之外的路径漏掉控制字符。
    #[test]
    fn reason_is_escaped_by_serde() {
        let body = ErrorResponse::with_reason("BOD_STREAM_DENIED", "第一行\n第二行\t制表");
        let rendered = render(&body);
        assert!(!rendered.contains('\n'), "换行必须被转义：{rendered}");
        assert!(rendered.contains(r"\n"));
        assert!(rendered.contains(r"\t"));
    }
}
