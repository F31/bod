//! 观察内容的敏感字段过滤（P2 任务 4.2「敏感字段过滤」用例的落地实现）。
//!
//! 观察来自执行端，属于不可信输入：日志里出现 `Authorization: Bearer ...`、
//! `"api_key": "..."`、`password=...` 或 PEM 私钥块都很常见。这些内容一旦原样落库，
//! 就会通过查询、导出、日志三个出口泄漏。
//!
//! 本模块只做**模式识别**，不依赖外部凭据清单：真实场景里宿主并不知道适配器用了哪些密钥值，
//! 拿不到值就只能按形态识别。识别保守但可核对：
//!
//! | 类别 | 形态 |
//! |---|---|
//! | `assignment` | `"token": "..."` / `api_key=...` / `密码: ...` 等敏感键的赋值 |
//! | `bearer` | `Bearer <token>` |
//! | `pem_private_key` | `-----BEGIN ... PRIVATE KEY-----` 到 `-----END ...-----` 整段 |
//! | `prefixed_token` | `sk-` / `ghp_` / `xoxb-` / `AKIA` / `AIza` / `eyJ` 等公认前缀的长令牌 |
//!
//! 脱敏后的文本保留键名与结构，只把值换成 `<redacted:类别>`，方便人工判断「这里本来有个秘密」，
//! 而不是整条丢弃 —— 丢掉会让人误以为日志本来就没有这段。

/// 替换文本的前缀；调用方据此判断是否已脱敏。
pub const REDACTED_PREFIX: &str = "<redacted:";

/// 敏感键名（小写比较）。包含常见变体与中文写法。
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "client_secret",
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "api_key",
    "apikey",
    "access_key",
    "secret_key",
    "private_key",
    "authorization",
    "credential",
    "credentials",
    "session_key",
    "密码",
    "口令",
    "密钥",
    "令牌",
];

/// 公认的令牌前缀：命中后把整段连续 token 字符替换。
const TOKEN_PREFIXES: &[(&str, &str)] = &[
    ("sk-", "openai_key"),
    ("sk_live_", "stripe_key"),
    ("ghp_", "github_token"),
    ("gho_", "github_token"),
    ("ghs_", "github_token"),
    ("xoxb-", "slack_token"),
    ("xoxp-", "slack_token"),
    ("AKIA", "aws_access_key"),
    ("AIza", "google_api_key"),
    ("eyJ", "jwt"),
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redaction {
    /// 被替换的次数（同一条里出现多次就计多次）。
    pub replaced: usize,
    /// 命中的类别，按出现顺序，允许重复。
    pub kinds: Vec<&'static str>,
}

impl Redaction {
    pub const fn is_empty(&self) -> bool {
        self.replaced == 0
    }

    /// 去重后的类别列表，用于展示。
    pub fn kinds_unique(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for kind in &self.kinds {
            if !out.contains(kind) {
                out.push(kind);
            }
        }
        out
    }
}

/// 过滤一段文本；返回新文本与命中记录。未命中时原样返回（不重新分配内容）。
pub fn redact(payload: &str) -> (String, Redaction) {
    let mut redaction = Redaction::default();
    if payload.is_empty() {
        return (String::new(), redaction);
    }

    let mut text = redact_pem(payload, &mut redaction);
    text = redact_bearer(&text, &mut redaction);
    text = redact_assignments(&text, &mut redaction);
    text = redact_chinese_keys(&text, &mut redaction);
    text = redact_prefixed_tokens(&text, &mut redaction);
    (text, redaction)
}

/// 是否已不含可识别的敏感内容（用于自检与验收，不用于准入判定）。
pub fn is_clean(payload: &str) -> bool {
    redact(payload).1.is_empty()
}

fn redact_pem(text: &str, redaction: &mut Redaction) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(begin) = rest.find("-----BEGIN ") else {
            out.push_str(rest);
            break;
        };
        // 头部结束的五个横线：从标签之后开始找，避免命中 "-----BEGIN" 自身。
        let Some(header_close_rel) = rest[begin + 11..].find("-----") else {
            out.push_str(&rest[..begin + 11]);
            rest = &rest[begin + 11..];
            continue;
        };
        let header_close = begin + 11 + header_close_rel;
        let header = &rest[begin + 11..header_close];
        if !header.contains("PRIVATE KEY") {
            // 证书、公钥等不是秘密：原样保留到该块结束。
            out.push_str(&rest[..header_close + 5]);
            rest = &rest[header_close + 5..];
            continue;
        }
        let Some(end_rel) = rest[header_close..].find("-----END ") else {
            // 块没有收尾（被截断）：从头整体替换，不保留半截密钥。
            out.push_str(&rest[..begin]);
            out.push_str("<redacted:pem_private_key>");
            redaction.replaced += 1;
            redaction.kinds.push("pem_private_key");
            break;
        };
        let end_start = header_close + end_rel;
        let end_close = rest[end_start + 5..]
            .find("-----")
            .map(|offset| end_start + 5 + offset + 5)
            .unwrap_or(rest.len());
        out.push_str(&rest[..begin]);
        out.push_str("<redacted:pem_private_key>");
        redaction.replaced += 1;
        redaction.kinds.push("pem_private_key");
        rest = &rest[end_close..];
    }
    out
}

fn redact_bearer(text: &str, redaction: &mut Redaction) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(index) = find_ascii_ignore_case(rest, b"bearer ") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..index]);
        let token_start = index + "bearer ".len();
        let token_end = rest[token_start..]
            .find(|ch: char| !is_token_char(ch))
            .map(|offset| token_start + offset)
            .unwrap_or(rest.len());
        out.push_str(&rest[index..token_start]);
        if token_end > token_start {
            out.push_str("<redacted:bearer>");
            redaction.replaced += 1;
            redaction.kinds.push("bearer");
        }
        rest = &rest[token_end..];
    }
    out
}

fn redact_assignments(text: &str, redaction: &mut Redaction) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some((key, key_end, quoted)) = read_key(bytes, index)
            && is_sensitive_key(&key)
            && let Some((sep_index, value_end, had_value)) = read_assignment(bytes, key_end, quoted)
        {
            out.push_str(&text[index..=sep_index]);
            if had_value {
                out.push_str(&format!("<redacted:{}>", assignment_kind(&key)));
                redaction.replaced += 1;
                redaction.kinds.push("assignment");
            } else {
                // 空值不是秘密，原样保留（含引号），但也要跳过它。
                out.push_str(&text[sep_index + 1..value_end]);
            }
            index = value_end;
            continue;
        }
        let width = char_width(bytes[index]);
        out.push_str(&text[index..index + width]);
        index += width;
    }
    out
}

/// 读一个键：`"key"` 或裸标识符/中文键。返回 (小写键名, 结束位置, 是否带引号)。
fn read_key(bytes: &[u8], start: usize) -> Option<(String, usize, bool)> {
    if bytes[start] == b'"' {
        let mut index = start + 1;
        while index < bytes.len() {
            if bytes[index] == b'"' {
                let key = String::from_utf8_lossy(&bytes[start + 1..index]).to_string();
                return Some((key, index + 1, true));
            }
            index += 1;
        }
        return None;
    }
    // 裸键：必须在边界上（行首/空白/{/,/&/?/;/|）。
    if start > 0 {
        let prev = bytes[start - 1];
        let boundary = prev.is_ascii_whitespace()
            || matches!(prev, b'{' | b',' | b'&' | b'?' | b';' | b'|' | b'(' | b'[');
        if !boundary {
            return None;
        }
    }
    let mut index = start;
    while index < bytes.len() {
        let byte = bytes[index];
        let is_key_byte =
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.') || byte >= 0x80;
        if !is_key_byte {
            break;
        }
        index += 1;
    }
    if index == start {
        return None;
    }
    let key = String::from_utf8_lossy(&bytes[start..index]).to_string();
    Some((key, index, false))
}

/// 在键后读分隔符与值。返回 (分隔符位置, 值结束位置, 是否真的有值)。
fn read_assignment(bytes: &[u8], key_end: usize, quoted: bool) -> Option<(usize, usize, bool)> {
    let mut index = key_end;
    while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    // 带引号的键之后只认 `:`；裸键认 `=` 或 `:`。
    let sep = *bytes.get(index)?;
    let ok = if quoted {
        sep == b':'
    } else {
        sep == b'=' || sep == b':'
    };
    if !ok {
        return None;
    }
    let sep_index = index;
    index += 1;
    while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    if index >= bytes.len() {
        return Some((sep_index, index, false));
    }
    if bytes[index] == b'"' {
        let value_start = index + 1;
        let mut cursor = value_start;
        while cursor < bytes.len() {
            if bytes[cursor] == b'\\' {
                cursor += 2;
                continue;
            }
            if bytes[cursor] == b'"' {
                let empty = cursor == value_start;
                return Some((sep_index, cursor + 1, !empty));
            }
            cursor += 1;
        }
        return Some((sep_index, bytes.len(), true));
    }
    let value_start = index;
    let mut cursor = index;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b','
            || byte == b'}'
            || byte == b']'
            || byte == b'&'
            || byte == b'\n'
            || byte == b'\r'
            || byte == b' '
            || byte == b'\t'
        {
            break;
        }
        cursor += 1;
    }
    let had_value = cursor > value_start;
    Some((sep_index, cursor, had_value))
}

/// 中文键的赋值（`密码: xxx`、`令牌：yyy`）。
///
/// 不能复用 ASCII 扫描器：中文键的字节边界与全角冒号都不满足裸标识符规则，
/// 分开处理比把 ASCII 扫描器复杂化更可靠。
fn redact_chinese_keys(text: &str, redaction: &mut Redaction) -> String {
    const KEYS: &[(&str, &str)] = &[
        ("密码", "password"),
        ("口令", "password"),
        ("密钥", "key"),
        ("令牌", "token"),
        ("凭据", "credential"),
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let mut best: Option<(usize, &str, &str)> = None;
        for (key, kind) in KEYS {
            if let Some(position) = rest.find(key)
                && best.is_none_or(|(seen, _, _)| position < seen)
            {
                best = Some((position, key, kind));
            }
        }
        let Some((position, key, kind)) = best else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..position]);
        let after_key = position + key.len();
        let tail = &rest[after_key..];
        let trimmed = tail.trim_start();
        let leading = tail.len() - trimmed.len();
        let sep = trimmed.chars().next();
        let Some(sep @ (':' | '：')) = sep else {
            // 不是赋值形态（例如「密码字段」），原样输出键之后继续。
            out.push_str(key);
            rest = &rest[after_key..];
            continue;
        };
        let after_sep = &trimmed[sep.len_utf8()..];
        let value_part = after_sep.trim_start();
        let gap = after_sep.len() - value_part.len();
        let value_end = value_part
            .find(['，', '；', ',', ';', '&', '。', '\n', '\r'])
            .unwrap_or(value_part.len());
        if value_part[..value_end].trim().is_empty() {
            out.push_str(key);
            rest = &rest[after_key..];
            continue;
        }
        out.push_str(key);
        out.push_str(&tail[..leading]);
        out.push(sep);
        out.push_str(&after_sep[..gap]);
        out.push_str(&format!("<redacted:{kind}>"));
        redaction.replaced += 1;
        redaction.kinds.push("assignment");
        rest = &value_part[value_end..];
    }
    out
}

fn redact_prefixed_tokens(text: &str, redaction: &mut Redaction) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let bytes = text.as_bytes();
    while index < bytes.len() {
        let mut hit: Option<(&'static str, usize)> = None;
        for (prefix, kind) in TOKEN_PREFIXES {
            if text[index..].starts_with(prefix) {
                hit = Some((kind, prefix.len()));
                break;
            }
        }
        if let Some((kind, prefix_len)) = hit {
            let mut end = index + prefix_len;
            while end < bytes.len() && is_token_char(bytes[end] as char) {
                end += 1;
            }
            // 太短的只可能是普通单词（例如 "sk-" 后面什么都没有），不动。
            if end - (index + prefix_len) >= 12 {
                out.push_str(&format!("<redacted:{kind}>"));
                redaction.replaced += 1;
                redaction.kinds.push("prefixed_token");
                index = end;
                continue;
            }
        }
        let width = char_width(bytes[index]);
        out.push_str(&text[index..index + width]);
        index += width;
    }
    out
}

fn assignment_kind(key: &str) -> &'static str {
    let lowered = key.to_ascii_lowercase();
    if lowered.contains("password") || lowered.contains("passwd") || key.contains("密码") {
        "password"
    } else if lowered.contains("token") || key.contains("令牌") {
        "token"
    } else if lowered.contains("key") || lowered.contains("secret") || key.contains("密钥") {
        "key"
    } else if lowered.contains("credential") || key.contains("口令") {
        "credential"
    } else {
        "secret"
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lowered = key.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return false;
    }
    SENSITIVE_KEYS.iter().any(|candidate| {
        lowered == *candidate
            || lowered.ends_with(&format!("_{candidate}"))
            || lowered.starts_with(&format!("{candidate}_"))
    })
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~' | '+' | '/' | '=')
}

fn char_width(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte >= 0xF0 {
        4
    } else if byte >= 0xE0 {
        3
    } else if byte >= 0xC0 {
        2
    } else {
        1
    }
}

/// 大小写不敏感的 ASCII 子串查找；不整串 lowercase，避免长日志上的重复分配。
fn find_ascii_ignore_case(haystack: &str, needle: &[u8]) -> Option<usize> {
    let bytes = haystack.as_bytes();
    if bytes.len() < needle.len() {
        return None;
    }
    bytes
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_json_secret_pairs_and_keeps_other_keys() {
        let (text, redaction) = redact(r#"{"token":"abc123","decision":"accepted"}"#);
        assert!(!text.contains("abc123"), "token 值必须被替换: {text}");
        assert!(text.contains("<redacted:token>"), "{text}");
        assert!(
            text.contains(r#""decision":"accepted""#),
            "非敏感键必须原样保留: {text}"
        );
        assert_eq!(redaction.replaced, 1);
        assert_eq!(redaction.kinds_unique(), vec!["assignment"]);
    }

    #[test]
    fn redacts_query_and_env_style_values() {
        let (text, redaction) = redact("password=hunter2&user=bob&api_key=abcd1234");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("abcd1234"), "{text}");
        assert!(text.contains("user=bob"), "非敏感参数必须保留: {text}");
        assert_eq!(redaction.replaced, 2);
    }

    #[test]
    fn redacts_bearer_and_prefixed_tokens() {
        let (text, _) = redact("Authorization: Bearer sk-live-abcdef0123456789");
        assert!(!text.contains("sk-live-abcdef0123456789"), "{text}");
        assert!(text.contains("<redacted:bearer>"), "{text}");

        let (text, _) = redact("leaked ghp_0123456789abcdefghij in log");
        assert!(!text.contains("ghp_0123456789abcdefghij"), "{text}");
        assert!(text.contains("<redacted:github_token>"), "{text}");
    }

    #[test]
    fn short_prefixed_word_is_not_a_token() {
        // "sk-" 后面不足 12 个字符，按普通文本处理，避免误伤。
        let (text, redaction) = redact("the sk-runner is ready");
        assert_eq!(text, "the sk-runner is ready");
        assert!(redaction.is_empty());
    }

    #[test]
    fn redacts_private_key_block_but_keeps_certificate() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
        let (text, redaction) = redact(pem);
        assert_eq!(text, "<redacted:pem_private_key>");
        assert_eq!(redaction.kinds, vec!["pem_private_key"]);

        let cert = "-----BEGIN CERTIFICATE-----\nMIIBszCCARygAwIBAg\n-----END CERTIFICATE-----";
        let (text, redaction) = redact(cert);
        assert_eq!(text, cert, "证书不是秘密，不得改动");
        assert!(redaction.is_empty());
    }

    #[test]
    fn chinese_keys_are_recognized() {
        let (text, redaction) = redact("登录失败：密码: hunter2，用户: bob");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("用户: bob"), "非敏感中文键必须保留: {text}");
        assert_eq!(redaction.replaced, 1);
    }

    #[test]
    fn plain_conformance_payloads_are_untouched() {
        for payload in [
            "step-1",
            r#"{"candidate":"diff.patch"}"#,
            r#"{"billed_micros":120,"final":true}"#,
            "cleanup-after-final",
        ] {
            let (text, redaction) = redact(payload);
            assert_eq!(text, payload, "普通内容不应被改写: {payload}");
            assert!(redaction.is_empty(), "普通内容不应记脱敏: {payload}");
        }
        // 200KB 纯文本：只验证不被改写、不 panic。
        let huge = "L".repeat(200 * 1024);
        let (text, redaction) = redact(&huge);
        assert_eq!(text.len(), huge.len());
        assert!(redaction.is_empty());
    }

    #[test]
    fn empty_value_is_not_counted_as_redaction() {
        let (text, redaction) = redact(r#"{"token":""}"#);
        assert_eq!(text, r#"{"token":""}"#);
        assert_eq!(redaction.replaced, 0);
    }

    #[test]
    fn is_clean_matches_redaction_result() {
        assert!(is_clean("step-1"));
        assert!(!is_clean("password=hunter2"));
    }
}
