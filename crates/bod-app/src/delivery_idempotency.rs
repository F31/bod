//! P8 §11 去重语义分离的第一半（3.3）：**幂等键管命令重放**。
//!
//! 另一半是「来源身份管对象重复」，落点在 `delivery_source`（五元组）与
//! `delivery_tracking_sources`（来源关系）。两半必须分开，原因写在 design D2：
//! 合成一个键会出现两种错误——「重放被当成新导入」（同一个请求建了两条跟踪项）
//! 与「不同对象被当成重放」（第二个 PR 被吞掉，谁也没发现）。
//!
//! 本模块只有**纯逻辑**：键怎么校验、载荷指纹怎么派生、看到已有回执该重放还是拒绝。
//! 落库在 `store`（`import_delivery_source` 里读回执、写回执）。
//!
//! 五条不能让步的语义，测试逐条钉住：
//!
//! 1. **键的作用域是 `(company, key)`，不含命令**。同一个键换命令也是冲突：
//!    客户端拿一个键发两条不同命令本身就是缺陷，静默接受等于替它猜意图。
//! 2. **载荷指纹必须由内容派生**，不接受调用方自编。与 `revision_fingerprint`
//!    同口径：让调用方给指纹，幂等键就成了「调用方承诺」，换一个指纹就能插进来。
//! 3. **回执只写一次**。改写回执会让重放返回一个当时并不存在的结果（库里由触发器兜住）。
//! 4. **回执与业务写入同事务**。写入失败就不该留下「这个键已执行」的记录，
//!    否则改正载荷重试会撞上自己伪造的冲突，或者更糟——重放出一个从未发生的成功。
//! 5. **重放返回首次执行的结果，不是今天的状态**。跟踪项可能已关闭、来源可能已补了修订；
//!    重放说的是「这条命令当时做了什么」，不是「现在是什么样」。

use crate::domain::artifact::Digest;
use std::collections::BTreeMap;

/// 幂等键长度上限。键是索引列，超长值先拒绝，不让它进唯一索引。
pub const MAX_IDEMPOTENCY_KEY: usize = 128;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 幂等相关的拒绝原因。码是稳定字符串：客户端与日志按码分支，改码等于改契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyError {
    /// 公司为空：键没有作用域，两条不同公司的同名键会互相顶掉。
    EmptyCompanyId,
    EmptyKey,
    KeyTooLong(usize),
    /// 键里有控制字符：它会被写进日志与错误信息，不可打印字符只会让排查变猜谜。
    KeyHasControlChar,
    /// 命令域为空：回执无法回答「这是哪条命令的重放」。
    EmptyCommand,
    /// 同一键已记录在**另一条命令**上。
    KeyReusedForOtherCommand {
        recorded: String,
        presented: String,
    },
    /// 同一键、同一命令，但载荷不同。这是本模块存在的主要目的。
    KeyReusedWithOtherPayload {
        recorded: String,
        presented: String,
    },
    /// 回执里的结果体读不回来（库被旁路写过，或格式升级未迁移）。
    /// 报错而不是「当没看见」：静默当作 Fresh 会重复执行一次已经成功的命令。
    UnreadableReceipt {
        key: String,
        detail: String,
    },
    /// 结果体写不成回执。理论上不会发生（结果只有字符串与布尔），
    /// 但库层要存的是文本，序列化失败必须报错而不是写半条回执。
    ResultNotRecordable(String),
    /// 命令载荷里带了空字段名，或压根没有字段。
    EmptyPayloadField,
}

impl IdempotencyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyCompanyId => "IDEMPOTENCY_EMPTY_COMPANY",
            Self::EmptyKey => "IDEMPOTENCY_EMPTY_KEY",
            Self::KeyTooLong(_) => "IDEMPOTENCY_KEY_TOO_LONG",
            Self::KeyHasControlChar => "IDEMPOTENCY_KEY_HAS_CONTROL_CHAR",
            Self::EmptyCommand => "IDEMPOTENCY_EMPTY_COMMAND",
            Self::KeyReusedForOtherCommand { .. } => "IDEMPOTENCY_KEY_REUSED_FOR_OTHER_COMMAND",
            Self::KeyReusedWithOtherPayload { .. } => "IDEMPOTENCY_KEY_REUSED_WITH_OTHER_PAYLOAD",
            Self::UnreadableReceipt { .. } => "IDEMPOTENCY_RECEIPT_UNREADABLE",
            Self::ResultNotRecordable(_) => "IDEMPOTENCY_RESULT_NOT_RECORDABLE",
            Self::EmptyPayloadField => "IDEMPOTENCY_EMPTY_PAYLOAD_FIELD",
        }
    }

    pub fn explain(&self) -> String {
        match self {
            Self::EmptyCompanyId => "幂等键缺少公司：键的作用域不成立".to_string(),
            Self::EmptyKey => "幂等键不能为空".to_string(),
            Self::KeyTooLong(len) => {
                format!("幂等键长度 {len} 超过上限 {MAX_IDEMPOTENCY_KEY}：键是索引列，超长值先拒绝")
            }
            Self::KeyHasControlChar => "幂等键含控制字符：键会进日志，必须是可打印文本".to_string(),
            Self::EmptyCommand => "命令域不能为空：回执要能回答这是哪条命令的重放".to_string(),
            Self::KeyReusedForOtherCommand {
                recorded,
                presented,
            } => format!(
                "同一个幂等键已经用在命令 {recorded:?} 上，本次却以 {presented:?} 提交：\
                 一个键只属于一条命令，请换键"
            ),
            Self::KeyReusedWithOtherPayload {
                recorded,
                presented,
            } => format!(
                "同一个幂等键携带了不同载荷（已记录 {}，本次 {}）：\
                 重放只接受完全相同的命令；换了载荷请换键",
                preview(recorded),
                preview(presented)
            ),
            Self::UnreadableReceipt { key, detail } => {
                format!("幂等键 {key} 的回执结果读不回来（{detail}）：不当作首次执行，避免重复建项")
            }
            Self::ResultNotRecordable(detail) => {
                format!("结果体写不成回执（{detail}）：不写半条回执")
            }
            Self::EmptyPayloadField => {
                "载荷字段名为空：指纹要能回答「比的是哪一段」，匿名字段不行".to_string()
            }
        }
    }
}

/// 指纹是长 hex 串，整段塞进错误信息没人看得完。截断只为可读，比较仍按完整值。
fn preview(value: &str) -> String {
    const KEEP: usize = 24;
    if value.chars().count() <= KEEP {
        return format!("{value:?}");
    }
    let head: String = value.chars().take(KEEP).collect();
    format!("{head:?}…")
}

impl std::fmt::Display for IdempotencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explain())
    }
}

impl std::error::Error for IdempotencyError {}

// ---------------------------------------------------------------------------
// 键与载荷指纹
// ---------------------------------------------------------------------------

/// 校验一个幂等键，返回去掉首尾空白的键。
///
/// 键是客户端给的，但**不是**客户端可以随便定义语义的地方：它的唯一作用域是
/// `(company, key)`，内容本身不参与任何判定。所以只做「能不能进索引」的校验。
pub fn validate_key(key: &str) -> Result<&str, IdempotencyError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(IdempotencyError::EmptyKey);
    }
    let len = key.chars().count();
    if len > MAX_IDEMPOTENCY_KEY {
        return Err(IdempotencyError::KeyTooLong(len));
    }
    if key.chars().any(char::is_control) {
        return Err(IdempotencyError::KeyHasControlChar);
    }
    Ok(key)
}

/// 载荷指纹：`sha256:<64 hex>`，对**规范 JSON** 求摘要。
///
/// 规范化靠 `BTreeMap`（键按字节序）加 `serde_json` 的确定转义，因此同一组字段值
/// 一定得到同一字节串；再压成 sha256 是为了让指纹定长、可进唯一索引。
/// **命令域不参与指纹**：它单独比对，这样「换命令」与「换载荷」在错误里能分开，
/// 调用方才知道自己错在哪。
///
/// 空载荷不是合法输入：空指纹会把所有「没带载荷」的命令合成一条，
/// 那是把不同的东西当成同一件（与 `revision_fingerprint` 拒绝全空的理由相同）。
pub fn payload_fingerprint(fields: &[(&str, &str)]) -> Result<String, IdempotencyError> {
    if fields.is_empty() {
        return Err(IdempotencyError::EmptyPayloadField);
    }
    let mut map: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, value) in fields {
        if name.trim().is_empty() {
            return Err(IdempotencyError::EmptyPayloadField);
        }
        map.insert(name, value);
    }
    let text = serde_json::to_string(&map).map_err(|err| {
        IdempotencyError::ResultNotRecordable(format!("载荷无法编码为 JSON：{err}"))
    })?;
    Ok(Digest::of_bytes(text.as_bytes()).as_string())
}

// ---------------------------------------------------------------------------
// 回执与判定
// ---------------------------------------------------------------------------

/// 首次执行产出的对象种类。**不写 None**：一条没有产出的命令不该留下回执，
/// 否则重放会返回一个空结果，而调用方无从判断那次到底做了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    Tracking,
    Candidate,
}

impl ResultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracking => "tracking",
            Self::Candidate => "candidate",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "tracking" => Some(Self::Tracking),
            "candidate" => Some(Self::Candidate),
            _ => None,
        }
    }
}

/// `delivery_command_receipts` 一行。只读不写——回执是历史（见模块文档第 3 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub company_id: String,
    pub idempotency_key: String,
    pub command: String,
    pub payload_fingerprint: String,
    pub result_kind: ResultKind,
    pub result_id: String,
    /// 首次执行的响应体（规范 JSON）。重放原样返回，不按今天的状态重算。
    pub result_detail: String,
    pub recorded_at_ms: i64,
}

/// 看到已有回执时的结论。只有三种，没有「部分匹配」：
/// 重放要么完全成立，要么是冲突，不存在「差不多就算重放」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay {
    /// 这个键第一次出现：可以执行。
    Fresh,
    /// 同键、同命令、同载荷：返回已有回执，不再写入。
    Replayed(Receipt),
}

/// 比对「已有回执」与「本次提交」。顺序刻意固定：先键的作用域（命令），再载荷。
///
/// 先命令后载荷，是为了让错误指向真正的问题。反过来先比载荷的话，
/// 「换命令且换载荷」会报成载荷冲突，调用方会去改载荷——改完还是冲突。
pub fn classify(
    existing: Option<Receipt>,
    command: &str,
    payload_fingerprint: &str,
) -> Result<Replay, IdempotencyError> {
    let Some(receipt) = existing else {
        return Ok(Replay::Fresh);
    };
    if receipt.command != command {
        return Err(IdempotencyError::KeyReusedForOtherCommand {
            recorded: receipt.command,
            presented: command.to_string(),
        });
    }
    if receipt.payload_fingerprint != payload_fingerprint {
        return Err(IdempotencyError::KeyReusedWithOtherPayload {
            recorded: receipt.payload_fingerprint,
            presented: payload_fingerprint.to_string(),
        });
    }
    Ok(Replay::Replayed(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt() -> Receipt {
        Receipt {
            company_id: "c1".to_string(),
            idempotency_key: "k-1".to_string(),
            command: "import-delivery-source".to_string(),
            payload_fingerprint: payload_fingerprint(&[("project_id", "p1")]).unwrap(),
            result_kind: ResultKind::Tracking,
            result_id: "dt-1".to_string(),
            result_detail: "{\"tracking_id\":\"dt-1\"}".to_string(),
            recorded_at_ms: 100,
        }
    }

    /// 第一次见到键：Fresh，可以执行。
    #[test]
    fn unknown_key_is_fresh() {
        assert_eq!(
            classify(None, "import-delivery-source", "sha256:x"),
            Ok(Replay::Fresh)
        );
    }

    /// 同键同命令同载荷：重放，返回已有回执（不是今天的重算结果）。
    #[test]
    fn same_key_same_payload_replays() {
        let existing = receipt();
        let replay = classify(
            Some(existing.clone()),
            "import-delivery-source",
            &existing.payload_fingerprint,
        )
        .expect("同键同载荷应重放");
        assert_eq!(replay, Replay::Replayed(existing));
    }

    /// 同键异载荷：冲突。这是 §11 明写的一条 MUST。
    #[test]
    fn same_key_with_other_payload_conflicts() {
        let existing = receipt();
        let other = payload_fingerprint(&[("project_id", "p2")]).unwrap();
        let err = classify(Some(existing), "import-delivery-source", &other)
            .expect_err("同键异载荷必须冲突");
        assert!(matches!(
            err,
            IdempotencyError::KeyReusedWithOtherPayload { .. }
        ));
        assert_eq!(err.code(), "IDEMPOTENCY_KEY_REUSED_WITH_OTHER_PAYLOAD");
    }

    /// 同键换命令：单独一档错误。**不能**报成载荷冲突——那样调用方会去改载荷，
    /// 改完还是冲突，因为真正的问题是键被用在了两条命令上。
    #[test]
    fn same_key_under_other_command_is_its_own_conflict() {
        let existing = receipt();
        let err = classify(
            Some(existing.clone()),
            "link-delivery-source",
            &existing.payload_fingerprint,
        )
        .expect_err("换命令必须冲突");
        assert_eq!(err.code(), "IDEMPOTENCY_KEY_REUSED_FOR_OTHER_COMMAND");
    }

    /// 键校验：空、超长、含控制字符都拒绝。首尾空白不算语义差别。
    #[test]
    fn key_validation_is_strict_about_shape_only() {
        assert_eq!(validate_key(" k-1 "), Ok("k-1"));
        assert_eq!(validate_key("   "), Err(IdempotencyError::EmptyKey));
        assert_eq!(
            validate_key(&"k".repeat(MAX_IDEMPOTENCY_KEY + 1)),
            Err(IdempotencyError::KeyTooLong(MAX_IDEMPOTENCY_KEY + 1))
        );
        assert_eq!(
            validate_key("k\u{1f}1"),
            Err(IdempotencyError::KeyHasControlChar)
        );
        // 恰好等于上限是合法的：边界不搞 off-by-one。
        assert!(validate_key(&"k".repeat(MAX_IDEMPOTENCY_KEY)).is_ok());
    }

    /// 指纹由内容派生且稳定：字段顺序不影响结果（BTreeMap 排序）。
    #[test]
    fn fingerprint_is_order_independent_and_stable() {
        let a =
            payload_fingerprint(&[("project_id", "p1"), ("object_type", "pull-request")]).unwrap();
        let b =
            payload_fingerprint(&[("object_type", "pull-request"), ("project_id", "p1")]).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"), "{a}");
        assert_eq!(a.len(), "sha256:".len() + 64);
    }

    /// 指纹区分值里的分隔符：`a` + `b` 不能和 `ab` + 空拼成同一个指纹。
    #[test]
    fn fingerprint_cannot_be_forged_by_separators() {
        let a = payload_fingerprint(&[("v", "a"), ("d", "b")]).unwrap();
        let b = payload_fingerprint(&[("v", "ab"), ("d", "")]).unwrap();
        assert_ne!(a, b);
    }

    /// 不做 trim：`"p1 "` 与 `"p1"` 是两个载荷。静默合并会把一个打错的请求
    /// 当成另一个请求的重放。
    #[test]
    fn fingerprint_does_not_trim_values() {
        let a = payload_fingerprint(&[("project_id", "p1")]).unwrap();
        let b = payload_fingerprint(&[("project_id", "p1 ")]).unwrap();
        assert_ne!(a, b);
    }

    /// 空载荷与空字段名都拒绝：空指纹会把不同命令合成一条。
    #[test]
    fn empty_payload_is_rejected() {
        assert!(payload_fingerprint(&[]).is_err());
        assert!(payload_fingerprint(&[("  ", "v")]).is_err());
    }

    /// 结果种类解析：表外值返回 None，由调用方决定报错（不静默当作 tracking）。
    #[test]
    fn result_kind_round_trips() {
        for kind in [ResultKind::Tracking, ResultKind::Candidate] {
            assert_eq!(ResultKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ResultKind::parse("observation"), None);
    }

    /// 错误信息里不塞完整指纹：截断只为可读。
    #[test]
    fn long_fingerprints_are_truncated_in_messages() {
        let long = format!("sha256:{}", "a".repeat(64));
        let message = IdempotencyError::KeyReusedWithOtherPayload {
            recorded: long.clone(),
            presented: long,
        }
        .explain();
        assert!(message.contains('…'), "{message}");
        assert!(
            !message.contains(&"a".repeat(64)),
            "不该整段贴出来：{message}"
        );
    }
}
