//! P2 任务 1.2：`bod-canonical-json-v1` 规范编码与摘要分离（纯值类型）。
//!
//! 边界：本模块只做「把一段 JSON 文本规范化 + 求摘要」，不读 store、不读文件、不写库、
//! 不决定接续等级。宿主事实与叙述（1.3）、部分快照（1.4）、现场清单与交接事件（1.5）
//! 由后续切片承担；接管事务（2.x）使用本模块产出的摘要做完整性绑定。
//!
//! 规范（与 `verification/vectors/bod-canonical-json-v1.json` 一一对应）：
//! 1. UTF-8 原样输出，只对 `"`、`\` 与 U+0000—U+001F 做最小编码；非 ASCII 字符不转义。
//! 2. 对象键按 UTF-8 字节序（等价 Unicode 码点序）升序；数组保持原顺序。
//! 3. 数字只接受 `0` 或 `-?[1-9][0-9]*` 形态的十进制整数字面量（i64/u64 范围）；小数、指数与
//!    `-0` 一律拒绝，避免 `1`/`1.0`/`1e0`/`-0` 在不同语言里被规范化成不同文本。
//! 4. 同一对象内重复键一律拒绝，不采用「后者覆盖」语义。
//! 5. 摘要形如 `sha256:<64 hex>`，对规范编码字节求值。
//!
//! 摘要分离（§18.10）：`integrity.payload_digest` 只覆盖 `payload` 子对象，覆盖范围不含
//! `integrity` 自身，因此不存在递归；原始文件的完整性必须另按字节计摘要（[`raw_bytes_digest`]），
//! 文本规范化不能当作原文件未被改动的证明。

use crate::domain::artifact::Digest;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;
use std::fmt;
use std::fmt::Write as _;

/// 规范编码标识，写入 `integrity.canonicalization`。
pub const CANONICALIZATION: &str = "bod-canonical-json-v1";

/// 规范编码拒绝的原因。全部为稳定分类，便于上层给用户解释「为什么不是有效交接包」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalError {
    /// 不是合法 JSON，或使用了本版本不接受的写法。
    InvalidJson(String),
    /// 同一对象内出现重复键（`path` 指向所在对象）。
    DuplicateKey { path: String, key: String },
    /// 小数字面量；v1 只接受整数。
    FractionalNumber { path: String },
    /// 非有限数（合法 JSON 字面量不会产生，但防御实现层注入）。
    NonFiniteNumber { path: String },
    /// 数字超出 i64/u64 范围，或使用了 `-0`/指数等无法跨语言稳定复现的整数值写法。
    NonCanonicalNumber { path: String },
    /// 顶层必须是对象（交接包 payload 是对象）。
    NotAnObject,
    /// 缺少 `payload` 子对象，无法计算 payload 摘要。
    MissingPayload,
    /// `integrity.canonicalization` 不是本实现支持的取值。
    UnsupportedCanonicalization(String),
    /// `integrity.payload_digest` 与按 payload 重算的结果不一致。
    DigestMismatch { expected: String, actual: String },
    /// `integrity.payload_digest` 不是 `sha256:<64 hex>`。
    MalformedDigest(String),
    /// 包声明了 `integrity` 却没有 `payload_digest`。
    MissingDigest,
    /// 时间字段不是 `YYYY-MM-DDTHH:MM:SSZ` 形式（v1 只接受秒级 UTC）。
    NonCanonicalTimestamp { path: String },
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(reason) => write!(f, "不是合法 JSON：{reason}"),
            Self::DuplicateKey { path, key } => {
                write!(f, "{path} 出现重复键 `{key}`，规范编码禁止覆盖语义")
            }
            Self::FractionalNumber { path } => {
                write!(f, "{path} 是小数，v1 规范编码只接受整数")
            }
            Self::NonFiniteNumber { path } => write!(f, "{path} 不是有限数"),
            Self::NonCanonicalNumber { path } => write!(
                f,
                "{path} 不是规范十进制整数字面量（`-0`、指数形式与超出 i64/u64 的值一律拒绝）"
            ),
            Self::NotAnObject => f.write_str("顶层必须是 JSON 对象"),
            Self::MissingPayload => f.write_str("交接包缺少 payload 对象"),
            Self::UnsupportedCanonicalization(value) => {
                write!(f, "不支持的规范编码 `{value}`")
            }
            Self::DigestMismatch { expected, actual } => {
                write!(f, "payload 摘要不匹配：声明 {expected}，重算 {actual}")
            }
            Self::MalformedDigest(value) => write!(f, "摘要格式非法：`{value}`"),
            Self::MissingDigest => f.write_str("integrity 缺少 payload_digest"),
            Self::NonCanonicalTimestamp { path } => write!(
                f,
                "{path} 不是规范时间（只接受秒级 UTC，形如 2026-11-17T11:00:00Z）"
            ),
        }
    }
}

impl std::error::Error for CanonicalError {}

/// 解析后的 JSON 值。保留对象键的原始顺序与重复项，供后续校验给出准确路径。
#[derive(Debug, Clone, PartialEq)]
enum StrictValue {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    Array(Vec<StrictValue>),
    Object(Vec<(String, StrictValue)>),
}

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("任意 JSON 值")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue::Int(i128::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue::Int(i128::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        // 保留为 Float，让校验阶段给出带路径的稳定分类，而不是在解析层丢失原因。
        Ok(StrictValue::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue::Str(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue::Str(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<StrictValue>()? {
            items.push(item);
        }
        Ok(StrictValue::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
            entries.push((key, value));
        }
        Ok(StrictValue::Object(entries))
    }
}

/// 已通过规范校验的 JSON 对象（payload 或整包）。
///
/// 构造即保证：顶层是对象、无重复键、数字均为 i64/u64 范围内的整数。
/// 因此 [`Self::to_canonical_string`] 对同一语义值稳定产出同一字节串。
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalValue {
    value: StrictValue,
}

impl CanonicalValue {
    /// 解析并校验。顶层必须是对象，否则 `NotAnObject`。
    pub fn parse(input: &str) -> Result<Self, CanonicalError> {
        let value: StrictValue = serde_json::from_str(input)
            .map_err(|err| CanonicalError::InvalidJson(err.to_string()))?;
        validate(&value, &mut String::from("$"))?;
        if !matches!(value, StrictValue::Object(_)) {
            return Err(CanonicalError::NotAnObject);
        }
        Ok(Self { value })
    }

    /// 规范编码字节（UTF-8）。键已排序、无空白、非 ASCII 原样。
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        encode(&self.value, &mut out);
        out.into_bytes()
    }

    /// 规范编码文本，跨语言对比用。
    pub fn canonical_string(&self) -> String {
        // encode 只产出合法 UTF-8，这里不会失败。
        String::from_utf8(self.canonical_bytes()).expect("规范编码只产生 UTF-8")
    }

    /// 对规范编码字节求摘要。
    pub fn digest(&self) -> Digest {
        Digest::of_bytes(&self.canonical_bytes())
    }

    /// 取成员；仅顶层字段，用于 `payload` / `integrity` 定位。返回副本以免把内部表示暴露出去。
    pub fn member(&self, key: &str) -> Option<CanonicalValue> {
        match &self.value {
            StrictValue::Object(entries) => {
                entries
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| CanonicalValue {
                        value: value.clone(),
                    })
            }
            _ => None,
        }
    }

    /// 取字符串成员（仅顶层）。
    pub fn member_str(&self, key: &str) -> Option<&str> {
        match &self.value {
            StrictValue::Object(entries) => {
                entries
                    .iter()
                    .find(|(name, _)| name == key)
                    .and_then(|(_, value)| match value {
                        StrictValue::Str(text) => Some(text.as_str()),
                        _ => None,
                    })
            }
            _ => None,
        }
    }

    /// 顶层键名，按编码后的顺序（已排序）返回，便于测试与展示。
    pub fn keys(&self) -> Vec<&str> {
        match &self.value {
            StrictValue::Object(entries) => {
                let mut names: Vec<&str> = entries.iter().map(|(key, _)| key.as_str()).collect();
                names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
                names
            }
            _ => Vec::new(),
        }
    }
}

/// `integrity` 段：只声明规范编码与 payload 摘要，不覆盖自身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    pub canonicalization: &'static str,
    pub payload_digest: Digest,
}

impl Integrity {
    pub fn as_json(&self) -> String {
        format!(
            "{{\"canonicalization\":\"{}\",\"payload_digest\":\"{}\"}}",
            self.canonicalization,
            self.payload_digest.as_string()
        )
    }
}

/// 为一段 payload 生成 `integrity`。摘要只覆盖 payload，因此可重复调用且结果稳定。
pub fn seal_payload(payload: &CanonicalValue) -> Integrity {
    Integrity {
        canonicalization: CANONICALIZATION,
        payload_digest: payload.digest(),
    }
}

/// 重算整包摘要并核对声明值。
///
/// 返回 payload 的摘要（调用方应再与 `integrity.payload_digest` 一起展示）。
/// 覆盖范围不含 `integrity`，所以篡改摘要字段只会得到 `DigestMismatch`，
/// 不会改变「正确摘要」本身——这正是「不递归计入自身」的可观测后果。
pub fn verify_envelope(envelope: &CanonicalValue) -> Result<Digest, CanonicalError> {
    let payload = envelope
        .member("payload")
        .ok_or(CanonicalError::MissingPayload)?;
    let computed = payload.digest();

    let Some(integrity) = envelope.member("integrity") else {
        // 未声明摘要的包可以计算摘要，但不能声称已校验；由调用方决定是否接受。
        return Ok(computed);
    };

    if let Some(value) = integrity.member_str("canonicalization")
        && value != CANONICALIZATION
    {
        return Err(CanonicalError::UnsupportedCanonicalization(
            value.to_string(),
        ));
    }

    let declared = integrity
        .member_str("payload_digest")
        .ok_or(CanonicalError::MissingDigest)?;
    let declared = Digest::parse(declared)
        .ok_or_else(|| CanonicalError::MalformedDigest(declared.to_string()))?;
    if declared != computed {
        return Err(CanonicalError::DigestMismatch {
            expected: declared.as_string(),
            actual: computed.as_string(),
        });
    }
    Ok(computed)
}

/// 时间字段的规范形式：秒级 UTC，`YYYY-MM-DDTHH:MM:SSZ`。
///
/// 不接受的写法（都由 [`validate_timestamp_field`] 拒绝）：时区偏移（`+08:00`）、
/// 小数秒、省略 `Z` 的本地时间、非补零的数字。这样两个语言实现不会对同一时刻产出不同文本。
pub const TIMESTAMP_EXAMPLE: &str = "2026-11-17T11:00:00Z";

/// 判断文本是否满足 v1 的规范时间形式。
///
/// 只做形状与范围检查（月 1—12、日 1—31、时 ≤ 23、分/秒 ≤ 59），不做日历精确校验：
/// `2026-02-31T00:00:00Z` 会被接受，因为真实日历校验属于宿主事实生成侧的职责，
/// 本模块不引入时区/闰秒库。
pub fn is_canonical_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 20 {
        return false;
    }
    let digits_ok =
        |range: std::ops::Range<usize>| bytes[range].iter().all(|byte| byte.is_ascii_digit());
    if !(digits_ok(0..4)
        && bytes[4] == b'-'
        && digits_ok(5..7)
        && bytes[7] == b'-'
        && digits_ok(8..10)
        && bytes[10] == b'T'
        && digits_ok(11..13)
        && bytes[13] == b':'
        && digits_ok(14..16)
        && bytes[16] == b':'
        && digits_ok(17..19)
        && bytes[19] == b'Z')
    {
        return false;
    }
    let number = |range: std::ops::Range<usize>| -> u32 {
        text[range].parse().expect("已确认是 ASCII 数字")
    };
    let (month, day) = (number(5..7), number(8..10));
    let (hour, minute, second) = (number(11..13), number(14..16), number(17..19));
    (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
}

/// 校验 payload 的某个顶层字段是规范时间。缺字段视为通过（是否必填由 schema 决定）。
pub fn validate_timestamp_field(payload: &CanonicalValue, key: &str) -> Result<(), CanonicalError> {
    let Some(text) = payload.member_str(key) else {
        return Ok(());
    };
    if is_canonical_timestamp(text) {
        Ok(())
    } else {
        Err(CanonicalError::NonCanonicalTimestamp {
            path: format!("$.{key}"),
        })
    }
}

/// 原始文件按字节计摘要。与规范摘要并列报告，二者不可互相替代。
pub fn raw_bytes_digest(bytes: &[u8]) -> Digest {
    Digest::of_bytes(bytes)
}

/// 一份交接材料的两种摘要：规范 payload 摘要 + 原始字节摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialIntegrity {
    pub canonical: Digest,
    pub raw_bytes: Digest,
    /// 规范摘要是否覆盖了全部原始字节（仅当原始文本恰好已是规范形式时为真）。
    pub byte_identical_to_canonical: bool,
}

/// 对一份原始 JSON 文本同时给出规范摘要与字节摘要。
pub fn material_integrity(raw: &[u8]) -> Result<MaterialIntegrity, CanonicalError> {
    let text = std::str::from_utf8(raw)
        .map_err(|err| CanonicalError::InvalidJson(format!("不是 UTF-8：{err}")))?;
    let value = CanonicalValue::parse(text)?;
    let canonical_bytes = value.canonical_bytes();
    Ok(MaterialIntegrity {
        canonical: Digest::of_bytes(&canonical_bytes),
        raw_bytes: raw_bytes_digest(raw),
        byte_identical_to_canonical: canonical_bytes == raw,
    })
}

// ---------------------------------------------------------------------------
// 校验与编码
// ---------------------------------------------------------------------------

fn validate(value: &StrictValue, path: &mut String) -> Result<(), CanonicalError> {
    match value {
        StrictValue::Null | StrictValue::Bool(_) | StrictValue::Str(_) => Ok(()),
        StrictValue::Int(_) => Ok(()),
        StrictValue::Float(number) => {
            if !number.is_finite() {
                Err(CanonicalError::NonFiniteNumber { path: path.clone() })
            } else if number.fract() != 0.0 {
                Err(CanonicalError::FractionalNumber { path: path.clone() })
            } else {
                // 形如 `-0` / `1.0` / `1e3` 的整数值：字面量本身不可跨语言稳定复现，仍拒绝。
                // serde_json 把 `-0` 解析为浮点，无法与 `0` 区分，因此 v1 只接受
                // `0` 或 `-?[1-9][0-9]*` 形态的整数字面量。
                Err(CanonicalError::NonCanonicalNumber { path: path.clone() })
            }
        }
        StrictValue::Array(items) => {
            let base_len = path.len();
            for (index, item) in items.iter().enumerate() {
                path.push_str(&format!("[{index}]"));
                validate(item, path)?;
                path.truncate(base_len);
            }
            Ok(())
        }
        StrictValue::Object(entries) => {
            let mut seen: HashSet<&str> = HashSet::new();
            for (key, _) in entries {
                if !seen.insert(key.as_str()) {
                    return Err(CanonicalError::DuplicateKey {
                        path: path.clone(),
                        key: key.clone(),
                    });
                }
            }
            let base_len = path.len();
            for (key, item) in entries {
                path.push('.');
                path.push_str(key);
                validate(item, path)?;
                path.truncate(base_len);
            }
            Ok(())
        }
    }
}

fn encode(value: &StrictValue, out: &mut String) {
    match value {
        StrictValue::Null => out.push_str("null"),
        StrictValue::Bool(true) => out.push_str("true"),
        StrictValue::Bool(false) => out.push_str("false"),
        StrictValue::Int(number) => {
            let _ = write!(out, "{number}");
        }
        // 校验阶段已拒绝 Float，这里不产出任何内容；保留分支避免未来改动静默丢值。
        StrictValue::Float(_) => out.push_str("null"),
        StrictValue::Str(text) => encode_str(text, out),
        StrictValue::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                encode(item, out);
            }
            out.push(']');
        }
        StrictValue::Object(entries) => {
            let mut sorted: Vec<&(String, StrictValue)> = entries.iter().collect();
            sorted.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            out.push('{');
            for (index, (key, item)) in sorted.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                encode_str(key, out);
                out.push(':');
                encode(item, out);
            }
            out.push('}');
        }
    }
}

fn encode_str(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// 便捷入口：把一段 JSON 文本规范化为字符串。
pub fn canonicalize(input: &str) -> Result<String, CanonicalError> {
    CanonicalValue::parse(input).map(|value| value.canonical_string())
}

/// 便捷入口：计算 payload 的摘要（`sha256:<hex>`）。
pub fn payload_digest(payload: &CanonicalValue) -> Digest {
    payload.digest()
}

/// 若干字符串按**长度前缀**拼接后求 sha256（64 位 hex，不带算法前缀）。
///
/// 用途只有一个：把「一组字段」压成一个可复现的 ID 原料。长度前缀不是讲究，
/// 是必需的——直接拼接时 `("a","bc")` 与 `("ab","c")` 会得到同一段字节，
/// 于是两件不同的事派生出同一个 ID。
///
/// 放在这里而不是各模块各写一份：`delivery_link`（目标与关系 ID）与
/// `delivery_source`（导入时新建的 ID）用的是同一条规则，两份实现迟早会漂移，
/// 而漂移的表现是「重试落不到同一行」——最难查的那种。
pub fn length_prefixed_digest(parts: &[&str]) -> String {
    Digest::of_bytes(length_prefixed_parts(parts).as_bytes()).value
}

/// 同一套长度前缀拼接，**不**求摘要。
///
/// 用途是把「一组字段」变成一个可比较的字符串（如载荷指纹里的目标身份）。
/// 与 [`length_prefixed_digest`] 共用拼接规则：两处各写一遍的话，一边改了分隔方式，
/// 另一边不会跟着变，于是「同一组字段」在 ID 与指纹里指向两个东西。
pub fn length_prefixed_parts(parts: &[&str]) -> String {
    let mut material = String::new();
    for part in parts {
        material.push_str(&part.chars().count().to_string());
        material.push(':');
        material.push_str(part);
    }
    material
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> &'static str {
        r#"{"b":2,"a":{"z":1,"y":[3,2]},"s":"中文"}"#
    }

    #[test]
    fn invariant_i_46_key_order_and_utf8_are_stable() {
        let first = canonicalize(payload()).unwrap();
        let reordered = canonicalize(r#"{"s":"中文","a":{"y":[3,2],"z":1},"b":2}"#).unwrap();
        assert_eq!(first, r#"{"a":{"y":[3,2],"z":1},"b":2,"s":"中文"}"#);
        assert_eq!(
            first, reordered,
            "键顺序不同的同语义输入必须规范成同一字节串"
        );
        assert!(
            !first.contains("\\u4e2d"),
            "非 ASCII 字符必须原样输出，不能转义"
        );
    }

    #[test]
    fn invariant_i_46_payload_digest_does_not_recurse_into_itself() {
        let payload = CanonicalValue::parse(payload()).unwrap();
        let integrity = seal_payload(&payload);
        let envelope = CanonicalValue::parse(&format!(
            r#"{{"payload":{},"integrity":{}}}"#,
            payload.canonical_string(),
            integrity.as_json()
        ))
        .unwrap();

        let computed = verify_envelope(&envelope).unwrap();
        assert_eq!(computed, integrity.payload_digest);
        assert_eq!(
            computed,
            payload.digest(),
            "整包校验必须等于单独对 payload 求值"
        );

        // 篡改声明值不会改变「正确摘要」，只会被判为不匹配。
        let tampered = CanonicalValue::parse(&format!(
            r#"{{"payload":{},"integrity":{{"canonicalization":"{CANONICALIZATION}","payload_digest":"sha256:{}"}}}}"#,
            payload.canonical_string(),
            "0".repeat(64)
        ))
        .unwrap();
        let error = verify_envelope(&tampered).unwrap_err();
        assert!(matches!(error, CanonicalError::DigestMismatch { .. }));
        assert_eq!(
            payload.digest(),
            seal_payload(&payload).payload_digest,
            "同一 payload 重复求值结果稳定"
        );
    }

    #[test]
    fn invariant_i_46_raw_bytes_digest_is_reported_separately() {
        let pretty = b"{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let compact = br#"{"a":1,"b":2}"#;
        let pretty_integrity = material_integrity(pretty).unwrap();
        let compact_integrity = material_integrity(compact).unwrap();

        assert_eq!(
            pretty_integrity.canonical, compact_integrity.canonical,
            "空白不影响规范摘要"
        );
        assert_ne!(
            pretty_integrity.raw_bytes, compact_integrity.raw_bytes,
            "原始字节摘要必须如实反映空白差异，不能拿规范摘要冒充文件完整性"
        );
        assert!(!pretty_integrity.byte_identical_to_canonical);
        assert!(compact_integrity.byte_identical_to_canonical);
    }

    #[test]
    fn invariant_i_46_rejects_duplicate_keys_and_non_integer_numbers() {
        let duplicate = CanonicalValue::parse(r#"{"a":1,"a":2}"#).unwrap_err();
        assert_eq!(
            duplicate,
            CanonicalError::DuplicateKey {
                path: "$".to_string(),
                key: "a".to_string()
            }
        );
        assert!(matches!(
            CanonicalValue::parse(r#"{"n":1.5}"#).unwrap_err(),
            CanonicalError::FractionalNumber { .. }
        ));
        assert!(matches!(
            CanonicalValue::parse(r#"{"n":1e3}"#).unwrap_err(),
            CanonicalError::NonCanonicalNumber { .. }
        ));
        assert!(matches!(
            CanonicalValue::parse(r#"{"n":-0}"#).unwrap_err(),
            CanonicalError::NonCanonicalNumber { .. }
        ));
        assert_eq!(
            CanonicalValue::parse("[1,2]").unwrap_err(),
            CanonicalError::NotAnObject
        );
        assert!(matches!(
            CanonicalValue::parse(r#"{"a":"\ud800"}"#).unwrap_err(),
            CanonicalError::InvalidJson(_)
        ));
    }

    #[test]
    fn invariant_i_46_escapes_only_required_characters() {
        let value = CanonicalValue::parse(r#"{"s":"a\u0001b\n\t\"\\/"}"#).unwrap();
        assert_eq!(
            value.canonical_string(),
            r#"{"s":"a\u0001b\n\t\"\\/"}"#,
            "只转义引号、反斜杠与控制字符，斜杠保持原样"
        );
    }

    #[test]
    fn invariant_i_46_timestamp_format_is_fixed() {
        assert!(is_canonical_timestamp("2026-11-17T11:00:00Z"));
        for rejected in [
            "2026-11-17T11:00:00+08:00",
            "2026-11-17T11:00:00.500Z",
            "2026-11-17T11:00:00",
            "2026-11-17 11:00:00Z",
            "2026-13-17T11:00:00Z",
            "2026-11-17T24:00:00Z",
            "2026-1-17T11:00:00Z",
        ] {
            assert!(
                !is_canonical_timestamp(rejected),
                "{rejected} 不应被当作规范时间"
            );
        }

        let payload =
            CanonicalValue::parse(r#"{"captured_at":"2026-11-17T11:00:00Z","n":1}"#).unwrap();
        assert!(validate_timestamp_field(&payload, "captured_at").is_ok());
        assert!(
            validate_timestamp_field(&payload, "absent").is_ok(),
            "缺字段不在此处判必填"
        );

        let bad = CanonicalValue::parse(r#"{"captured_at":"2026-11-17T11:00:00+08:00"}"#).unwrap();
        assert_eq!(
            validate_timestamp_field(&bad, "captured_at").unwrap_err(),
            CanonicalError::NonCanonicalTimestamp {
                path: "$.captured_at".to_string()
            }
        );
    }

    #[test]
    fn invariant_i_46_cross_language_vectors_match() {
        let raw = include_str!("../../../verification/vectors/bod-canonical-json-v1.json");
        let vectors: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            vectors["canonicalization"], CANONICALIZATION,
            "向量文件必须声明同一规范版本"
        );
        let cases = vectors["vectors"].as_array().unwrap();
        assert!(cases.len() >= 8, "向量数量不足，无法覆盖键序/转义/错误样本");

        let mut ok_cases = 0usize;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = case["input"].as_str().unwrap();
            if let Some(expected_error) = case["expected_error"].as_str() {
                let error = match CanonicalValue::parse(input) {
                    Ok(_) => panic!("{name} 应被拒绝，但解析成功"),
                    Err(err) => err,
                };
                assert_eq!(
                    error_kind(&error),
                    expected_error,
                    "{name} 的拒绝分类与向量不一致"
                );
                continue;
            }

            let value = CanonicalValue::parse(input)
                .unwrap_or_else(|err| panic!("{name} 应可解析，实际：{err}"));
            let canonical = value.canonical_string();
            assert_eq!(
                canonical,
                case["canonical"].as_str().unwrap(),
                "{name} 的规范文本与向量不一致"
            );
            assert_eq!(
                value.digest().value,
                case["sha256"].as_str().unwrap(),
                "{name} 的规范摘要与向量不一致"
            );
            if let Some(raw_sha256) = case["raw_sha256"].as_str() {
                assert_eq!(
                    raw_bytes_digest(input.as_bytes()).value,
                    raw_sha256,
                    "{name} 的原始字节摘要与向量不一致"
                );
            }
            ok_cases += 1;
        }
        assert!(ok_cases >= 4, "至少要有 4 条可计算摘要的向量");
    }

    /// 把错误映射为向量文件里的稳定分类名。
    fn error_kind(error: &CanonicalError) -> &'static str {
        match error {
            CanonicalError::InvalidJson(_) => "invalid_json",
            CanonicalError::DuplicateKey { .. } => "duplicate_key",
            CanonicalError::FractionalNumber { .. } => "fractional_number",
            CanonicalError::NonFiniteNumber { .. } => "non_finite_number",
            CanonicalError::NonCanonicalNumber { .. } => "non_canonical_number",
            CanonicalError::NotAnObject => "not_an_object",
            CanonicalError::MissingPayload => "missing_payload",
            CanonicalError::UnsupportedCanonicalization(_) => "unsupported_canonicalization",
            CanonicalError::DigestMismatch { .. } => "digest_mismatch",
            CanonicalError::MalformedDigest(_) => "malformed_digest",
            CanonicalError::MissingDigest => "missing_digest",
            CanonicalError::NonCanonicalTimestamp { .. } => "non_canonical_timestamp",
        }
    }
}
