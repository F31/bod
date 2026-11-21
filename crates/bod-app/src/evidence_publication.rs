//! 证据发布策略与脱敏（A6）：docs 只留本地，归档与外部发布分级。
//!
//! 三条前提（用户已定的仓库约定）：
//! 1. `docs/` 不入库，报告只在本地保留；
//! 2. 需要给别人看的证据先**脱敏**，脱敏是机械替换，不能靠"我记得删了"；
//! 3. 外部发布必须有人批——工具只检查"能不能发"，不代替谁批准。
//!
//! 因此本模块把结论分成三档（[`Audience`]），并对"外部"这一档要求显式的
//! [`HumanApproval`]：没有审批人，正文再干净也返回不可发布。
//!
//! 脱敏之后还会**再扫一遍**（[`ScrubOutcome::residual`]）：如果清洗后仍能匹配到
//! 令牌或私网地址，说明清洗有漏（例如令牌被换行拆开），此时外部发布直接阻断。

/// 发布策略 schema；格式变化必须换版本。
pub const POLICY_SCHEMA: &str = "bod.evidence.publication/v1";

/// 受众分档。分档决定"要做多少脱敏"和"要不要人批"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// 本机自查：原文即可，不做任何替换。
    LocalOnly,
    /// 团队内部归档：去掉凭据与个人目录，允许保留路径形态与内部 IP 段说明。
    TeamArchive,
    /// 外部发布：在归档要求之上，还必须有人批准。
    External,
}

impl Audience {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only",
            Self::TeamArchive => "team_archive",
            Self::External => "external",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" | "local_only" => Some(Self::LocalOnly),
            "team" | "team_archive" => Some(Self::TeamArchive),
            "external" => Some(Self::External),
            _ => None,
        }
    }

    /// 该档位至少要做哪些清洗。
    pub fn requirements(self) -> &'static [&'static str] {
        match self {
            Self::LocalOnly => &[],
            Self::TeamArchive => &[
                "去掉 Bearer 令牌与会话密钥明文",
                "替换个人主目录用户名为占位符",
            ],
            Self::External => &[
                "去掉 Bearer 令牌与会话密钥明文",
                "替换个人主目录用户名为占位符",
                "替换私网 IP 与邮箱",
                "外部发布需人工批准（工具不代批）",
            ],
        }
    }
}

/// 清洗命中的类别。只用于统计与说明，不保留原值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RedactionKind {
    /// `Authorization: Bearer <token>` 里的令牌。
    BearerToken,
    /// JSON 里的 `session_key` / `handshake_token` 取值。
    SessionKey,
    /// `sk-` 前缀的密钥。
    ApiKey,
    /// `/home/<user>`、`C:\Users\<user>` 这类个人目录。
    HomePath,
    /// 私网 IPv4。
    PrivateIp,
    /// 邮箱地址。
    Email,
}

impl RedactionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BearerToken => "bearer_token",
            Self::SessionKey => "session_key",
            Self::ApiKey => "api_key",
            Self::HomePath => "home_path",
            Self::PrivateIp => "private_ip",
            Self::Email => "email",
        }
    }
}

/// 一次清洗的统计：命中类别与次数，**不记录原值**。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubOutcome {
    pub body: String,
    pub hits: std::collections::BTreeMap<&'static str, usize>,
    /// 清洗后仍能被规则匹配到的片段说明；非空即阻断外部发布。
    pub residual: Vec<String>,
}

impl ScrubOutcome {
    pub fn total(&self) -> usize {
        self.hits.values().sum()
    }

    pub fn is_clean(&self) -> bool {
        self.residual.is_empty()
    }

    /// 给报告用的一行摘要，不含任何原值。
    pub fn summary(&self) -> String {
        if self.hits.is_empty() {
            return "未命中需脱敏内容".to_string();
        }
        let parts: Vec<String> = self
            .hits
            .iter()
            .map(|(kind, count)| format!("{kind}×{count}"))
            .collect();
        format!("已脱敏 {}", parts.join("、"))
    }
}

/// 人工批准。字段为空即视为"没有批准"，工具不会替你补。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanApproval {
    pub approver: String,
    pub scope: String,
    pub at_ms: i64,
}

impl HumanApproval {
    pub fn new(approver: impl Into<String>, scope: impl Into<String>, at_ms: i64) -> Self {
        Self {
            approver: approver.into(),
            scope: scope.into(),
            at_ms,
        }
    }

    fn is_valid(&self) -> bool {
        !self.approver.trim().is_empty() && !self.scope.trim().is_empty() && self.at_ms > 0
    }
}

/// 发布判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub audience: Audience,
    pub body: String,
    pub outcome: ScrubOutcome,
    pub approved_by: Option<String>,
}

/// 判定能否发布。返回 `Err` 时每条都说明缺什么。
pub fn prepare(
    audience: Audience,
    text: &str,
    approval: Option<&HumanApproval>,
) -> Result<Publication, Vec<String>> {
    if audience == Audience::LocalOnly {
        return Ok(Publication {
            audience,
            body: text.to_string(),
            outcome: ScrubOutcome {
                body: text.to_string(),
                ..Default::default()
            },
            approved_by: None,
        });
    }

    let outcome = scrub(text);
    let mut blockers = Vec::new();

    if !outcome.residual.is_empty() {
        blockers.extend(outcome.residual.iter().cloned());
    }
    match audience {
        Audience::TeamArchive => {}
        Audience::External => match approval {
            Some(approval) if approval.is_valid() => {}
            Some(_) => {
                blockers.push("批准信息不完整（approver/scope/时间任一为空都不算批准）".to_string())
            }
            None => blockers.push("外部发布需要人工批准：没有批准人时工具不得放行".to_string()),
        },
        Audience::LocalOnly => {}
    }

    if blockers.is_empty() {
        Ok(Publication {
            audience,
            body: outcome.body.clone(),
            outcome,
            approved_by: approval.map(|approval| approval.approver.clone()),
        })
    } else {
        Err(blockers)
    }
}

/// 机械清洗：只有明确匹配到的片段才替换，不做"看起来像才删"的猜测。
pub fn scrub(text: &str) -> ScrubOutcome {
    let mut out = ScrubOutcome::default();
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let bytes = text.as_bytes();

    while cursor < bytes.len() {
        let Some((start, end, kind)) = next_match(text, cursor) else {
            result.push_str(&text[cursor..]);
            break;
        };
        result.push_str(&text[cursor..start]);
        result.push_str(placeholder(kind));
        *out.hits.entry(kind.as_str()).or_insert(0) += 1;
        cursor = end;
    }

    // 清完再扫一遍：仍能匹配说明规则有漏（例如被换行拆开的令牌），必须阻断外部发布。
    out.residual = residual_findings(&result);
    out.body = result;
    out
}

fn placeholder(kind: RedactionKind) -> &'static str {
    match kind {
        RedactionKind::BearerToken => "<bearer-token>",
        RedactionKind::SessionKey => "<session-key>",
        RedactionKind::ApiKey => "<api-key>",
        RedactionKind::HomePath => "<home>/",
        RedactionKind::PrivateIp => "<private-ip>",
        RedactionKind::Email => "<email>",
    }
}

/// 找到 `from` 之后最早出现的一个敏感片段。
fn next_match(text: &str, from: usize) -> Option<(usize, usize, RedactionKind)> {
    let candidates = [
        find_bearer(text, from),
        find_json_key(text, from, "session_key", RedactionKind::SessionKey),
        find_json_key(text, from, "handshake_token", RedactionKind::SessionKey),
        find_api_key(text, from),
        find_home_path(text, from),
        find_private_ip(text, from),
        find_email(text, from),
    ];
    candidates
        .into_iter()
        .flatten()
        .min_by_key(|(start, ..)| *start)
}

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
}

/// `Authorization: Bearer xxx` 或裸 `Bearer xxx`。
fn find_bearer(text: &str, from: usize) -> Option<(usize, usize, RedactionKind)> {
    let needle = "Bearer ";
    let hay = &text[from..];
    let offset = hay.find(needle)?;
    let start = from + offset;
    let value_start = start + needle.len();
    let bytes = text.as_bytes();
    let mut end = value_start;
    while end < bytes.len() && is_token_char(bytes[end]) {
        end += 1;
    }
    if end == value_start {
        return None;
    }
    Some((start, end, RedactionKind::BearerToken))
}

/// JSON 里的 `"key": "value"`，只替换值部分。
fn find_json_key(
    text: &str,
    from: usize,
    key: &str,
    kind: RedactionKind,
) -> Option<(usize, usize, RedactionKind)> {
    let needle = format!("\"{key}\"");
    let hay = &text[from..];
    let offset = hay.find(&needle)?;
    let start = from + offset;
    let after = start + needle.len();
    let rest = &text[after..];
    let colon = rest.find(':')?;
    let quote = rest[colon..].find('"')?;
    let value_start = after + colon + quote + 1;
    let value = &text[value_start..];
    let len = value.find('"')?;
    if len == 0 {
        return None;
    }
    Some((start, value_start + len, kind))
}

fn find_api_key(text: &str, from: usize) -> Option<(usize, usize, RedactionKind)> {
    let hay = &text[from..];
    let offset = hay.find("sk-")?;
    let start = from + offset;
    let bytes = text.as_bytes();
    // `sk-` 前必须是边界（避免把 "task-list" 之类误判）。
    if start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
        return None;
    }
    let mut end = start + 3;
    while end < bytes.len() && is_token_char(bytes[end]) {
        end += 1;
    }
    if end - start < 8 {
        // 太短不像密钥，不误删。
        return None;
    }
    Some((start, end, RedactionKind::ApiKey))
}

/// `/home/<user>/`、`/Users/<user>/`、`C:\Users\<user>\` 里的用户名。
fn find_home_path(text: &str, from: usize) -> Option<(usize, usize, RedactionKind)> {
    for prefix in ["/home/", "/Users/"] {
        if let Some(offset) = text[from..].find(prefix) {
            let start = from + offset;
            let user_start = start + prefix.len();
            let rest = &text[user_start..];
            let len = rest.find('/').unwrap_or(0);
            if len > 0 {
                return Some((start, user_start + len, RedactionKind::HomePath));
            }
        }
    }
    for prefix in ["C:\\Users\\", "D:\\Users\\"] {
        if let Some(offset) = text[from..].find(prefix) {
            let start = from + offset;
            let user_start = start + prefix.len();
            let rest = &text[user_start..];
            let len = rest.find('\\').unwrap_or(0);
            if len > 0 {
                return Some((start, user_start + len, RedactionKind::HomePath));
            }
        }
    }
    None
}

/// 私网 IPv4：10/8、192.168/16、172.16/12。
fn find_private_ip(text: &str, from: usize) -> Option<(usize, usize, RedactionKind)> {
    let bytes = text.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        let dot = index + text[index..].find('.')?;
        // 从 dot 往前找数字起点。
        let mut start = dot;
        while start > from && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start == dot {
            index = dot + 1;
            continue;
        }
        // 从 dot 往后补齐 a.b.c.d 的剩余部分。
        let mut end = dot;
        let mut dots = 0;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
            if bytes[end] == b'.' {
                dots += 1;
            }
            end += 1;
        }
        let candidate = &text[start..end];
        let parts: Vec<&str> = candidate.split('.').collect();
        if dots >= 2 && parts.len() == 4 && parts.iter().all(|part| part.parse::<u16>().is_ok()) {
            let first: u16 = parts[0].parse().unwrap_or(0);
            let second: u16 = parts[1].parse().unwrap_or(0);
            let private = first == 10
                || (first == 192 && second == 168)
                || (first == 172 && (16..=31).contains(&second));
            if private {
                return Some((start, end, RedactionKind::PrivateIp));
            }
        }
        index = end.max(dot + 1);
    }
    None
}

fn find_email(text: &str, from: usize) -> Option<(usize, usize, RedactionKind)> {
    let bytes = text.as_bytes();
    let mut index = from;
    while let Some(offset) = text[index..].find('@') {
        let at = index + offset;
        let mut start = at;
        while start > from {
            let byte = bytes[start - 1];
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+') {
                start -= 1;
            } else {
                break;
            }
        }
        let mut end = at + 1;
        while end < bytes.len() {
            let byte = bytes[end];
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-') {
                end += 1;
            } else {
                break;
            }
        }
        if start < at && end > at + 1 && text[at + 1..end].contains('.') {
            return Some((start, end, RedactionKind::Email));
        }
        index = at + 1;
        if index >= bytes.len() {
            break;
        }
    }
    None
}

/// 清洗后残留检查。宁可误报也不能漏：外部发布被挡一次比泄漏一次便宜。
fn residual_findings(text: &str) -> Vec<String> {
    let mut findings = Vec::new();
    if find_bearer(text, 0).is_some() {
        findings.push("清洗后仍有 Bearer 令牌：可能被换行或分隔符拆开，需人工处理".to_string());
    }
    if find_json_key(text, 0, "session_key", RedactionKind::SessionKey).is_some() {
        findings.push("清洗后仍有 session_key 明文".to_string());
    }
    if find_api_key(text, 0).is_some() {
        findings.push("清洗后仍有 sk- 形式密钥".to_string());
    }
    if find_private_ip(text, 0).is_some() {
        findings.push("清洗后仍有私网 IP".to_string());
    }
    if token_continues_after_redaction(text) {
        findings.push("令牌被换行拆开：已替换前半段，后半段仍是明文，需人工确认后重写".to_string());
    }
    findings
}

/// 替换后紧跟换行、且下一行看起来是**被折行的令牌续段**——典型的长令牌折行。
///
/// 判据刻意保守，避免把 `path=/home/...` 这类下一行误判成令牌续段：
/// 续段必须是一整行、长度 ≥12、不含 `= / :` 分隔符，且至少含一个数字。
fn token_continues_after_redaction(text: &str) -> bool {
    let needle = "<bearer-token>\n";
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find(needle) {
        let line_start = cursor + offset + needle.len();
        let rest = &text[line_start..];
        let line = rest.split('\n').next().unwrap_or("").trim_end();
        let looks_like_fragment = line.len() >= 12
            && line.bytes().all(is_token_char)
            && !line.contains('=')
            && !line.contains('/')
            && !line.contains(':')
            && line.bytes().any(|byte| byte.is_ascii_digit());
        if looks_like_fragment {
            return true;
        }
        cursor = line_start;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_only_keeps_text_untouched() {
        let text = "Authorization: Bearer sk-live-abc123456\n/home/alice/notes";
        let publication = prepare(Audience::LocalOnly, text, None).expect("本机自查不需要脱敏");
        assert_eq!(publication.body, text);
        assert!(publication.outcome.hits.is_empty());
        assert!(publication.approved_by.is_none());
    }

    #[test]
    fn bearer_token_and_session_key_are_replaced_not_truncated() {
        let text = "Authorization: Bearer sk-live-9f2c\n{\"session_key\":\"sess-abc\"}";
        let outcome = scrub(text);
        assert!(!outcome.body.contains("sk-live-9f2c"), "{}", outcome.body);
        assert!(!outcome.body.contains("sess-abc"), "{}", outcome.body);
        assert!(outcome.body.contains("<bearer-token>"));
        assert!(outcome.body.contains("<session-key>"));
        assert_eq!(outcome.hits.get("bearer_token"), Some(&1));
        assert_eq!(outcome.hits.get("session_key"), Some(&1));
        assert!(outcome.summary().contains("bearer_token×1"));
    }

    #[test]
    fn home_paths_private_ips_and_emails_are_scrubbed() {
        let text = "path=/home/alice/project/report.md ip=192.168.1.20 owner=bob@example.com";
        let outcome = scrub(text);
        assert!(!outcome.body.contains("alice"), "{}", outcome.body);
        assert!(!outcome.body.contains("192.168.1.20"), "{}", outcome.body);
        assert!(
            !outcome.body.contains("bob@example.com"),
            "{}",
            outcome.body
        );
        assert!(outcome.body.contains("<home>/"));
        assert!(outcome.body.contains("<private-ip>"));
        assert!(outcome.body.contains("<email>"));

        // 公网地址不是本策略的脱敏对象，不能被误删。
        let public = scrub("dns=8.8.8.8");
        assert!(public.body.contains("8.8.8.8"));
    }

    #[test]
    fn windows_user_directory_is_scrubbed() {
        let outcome = scrub("log at C:\\Users\\jinfeng\\bod\\tmp\\bench.log");
        assert!(!outcome.body.contains("jinfeng"), "{}", outcome.body);
        assert!(outcome.body.contains("<home>"), "{}", outcome.body);
    }

    #[test]
    fn team_archive_needs_no_approval_but_external_does() {
        let text = "token: sk-live-abcdef123456";
        let archive = prepare(Audience::TeamArchive, text, None).expect("团队归档不需要批准");
        assert!(!archive.body.contains("sk-live-abcdef123456"));
        assert!(archive.approved_by.is_none());

        let blocked = prepare(Audience::External, text, None).expect_err("没有批准不得外部发布");
        assert!(blocked.iter().any(|item| item.contains("人工批准")));

        let empty = HumanApproval::new("  ", "", 0);
        assert!(
            prepare(Audience::External, text, Some(&empty)).is_err(),
            "空批准信息不算批准"
        );

        let approved = HumanApproval::new("reviewer-a", "p7-baseline", 1_763_500_000_000);
        let publication =
            prepare(Audience::External, text, Some(&approved)).expect("有批准应可发布");
        assert_eq!(publication.approved_by.as_deref(), Some("reviewer-a"));
        assert!(publication.outcome.is_clean());
    }

    #[test]
    fn split_token_survives_scrub_and_blocks_publication() {
        // 令牌被空格拆开：机械清洗匹配不到，必须由残留检查挡住，而不是放过。
        let text = "header: Bearer sk-live-\n9f2c-rest-of-token";
        let outcome = scrub(text);
        assert!(
            !outcome.residual.is_empty(),
            "清洗漏掉时必须留下残留标记：{:?}",
            outcome.body
        );
        let blocked = prepare(Audience::TeamArchive, text, None);
        assert!(blocked.is_err(), "有残留时团队归档也不得放行");
    }

    #[test]
    fn requirements_are_declared_per_audience() {
        assert!(Audience::LocalOnly.requirements().is_empty());
        assert_eq!(Audience::TeamArchive.requirements().len(), 2);
        assert!(
            Audience::External
                .requirements()
                .iter()
                .any(|item| item.contains("人工批准"))
        );
        assert_eq!(Audience::parse("external"), Some(Audience::External));
        assert_eq!(Audience::parse("nope"), None);
    }

    #[test]
    fn scrub_is_idempotent_on_clean_text() {
        let clean = "commit a9c80d5, 48 test binaries ok";
        let once = scrub(clean);
        assert_eq!(once.body, clean);
        assert_eq!(once.total(), 0);
        assert!(once.is_clean());
        let twice = scrub(&once.body);
        assert_eq!(twice.body, clean);
    }
}
