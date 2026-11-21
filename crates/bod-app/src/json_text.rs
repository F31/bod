//! JSON 字符串字面量的唯一实现（CR-001）。
//!
//! 背景：此前 `crates/bod/src/main.rs`、`crates/bod/src/smoke.rs`、
//! `crates/bod-app/src/conformance_report.rs` 各有一份手写转义，最长的那份也只处理
//! `\\`、`"`、`\n`（smoke 版多 `\r`、`\t`）。RFC 8259 §7 要求 U+0000—U+001F 全部转义，
//! 于是带 `\r` 的 Windows 错误信息会让 `bod fault-matrix --json`、`bod serve --json`、
//! 冒烟报告与 conformance 报告输出非法 JSON——恰好都是给机器读的那几条输出。
//!
//! 这里统一改用 `serde_json` 生成，不再逐个字符维护转义表。同一份错误代码复制三处，
//! 是这个问题长期存在的直接原因；`crates/bod-app/tests/json_literal_guard.rs`
//! 会拦住第四份的出现。
//!
//! 与 [`crate::canonical_json`] 的关系：那个模块做的是「规范编码 + 摘要」，需要字节级
//! 确定性与键序控制，因此自带 `encode_str`，不能换成 serde_json。本模块只负责「让输出
//! 成为合法且可读的 JSON」，不对键序与数字形态施加约束。两者对同一字符串的输出逐字节
//! 相同，但职责不同，不合并。

/// 把字符串包装成合法的 JSON 字符串字面量（含两侧引号）。
///
/// 对合法 UTF-8 输入不会失败：`serde_json` 对字符串的序列化是无条件成功的，
/// 因此这里不返回 `Result`，避免每个调用点写一次不可能发生的 `unwrap`。
pub fn json_string(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

/// `Option<&str>` 版本：`None` 输出裸 `null`，不输出 `"null"`。
pub fn opt_json_string(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_string(), json_string)
}

#[cfg(test)]
mod tests {
    use super::{json_string, opt_json_string};

    fn round_trips(value: &str) -> bool {
        serde_json::from_str::<String>(&json_string(value))
            .map(|decoded| decoded == value)
            .unwrap_or(false)
    }

    /// RFC 8259 §7 的全部控制字符逐一遍历。
    ///
    /// 判据用 round-trip 而非逐字符比对：只要能被标准解析器原样读回，就一定转义正确；
    /// 这样不会因为「期望写成 `\u0008` 还是 `\b`」这种实现细节把测试写死。
    #[test]
    fn every_control_character_is_escaped_and_round_trips() {
        for code in 0u32..=0x1F {
            let Some(ch) = char::from_u32(code) else {
                panic!("U+{code:04X} 应当是合法标量值");
            };
            let raw = format!("a{ch}b");
            let encoded = json_string(&raw);
            assert!(
                !encoded.contains(ch),
                "控制字符 U+{code:04X} 未被转义，输出里留有裸字符：{encoded:?}"
            );
            assert!(
                round_trips(&raw),
                "U+{code:04X} round-trip 失败：{encoded:?}"
            );
        }
    }

    /// 评审报告里的最小复现：Windows 错误信息里的 `\r` 曾经原样落进 JSON。
    #[test]
    fn carriage_return_from_windows_error_text_is_escaped() {
        let raw = "打开文件失败\r\n路径：C:\\tmp\\a.txt";
        let encoded = json_string(raw);
        assert_eq!(encoded, r#""打开文件失败\r\n路径：C:\\tmp\\a.txt""#);
        assert!(round_trips(raw));
    }

    /// 引号与反斜杠仍然正确——这是老实现唯一做对的两件事，不能让统一后反而退步。
    #[test]
    fn quote_and_backslash_are_escaped() {
        assert_eq!(json_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(json_string(r"a\b"), r#""a\\b""#);
    }

    /// 反斜杠必须先在概念上被处理，否则 `\n` 这类两字符序列会被二次转义。
    ///
    /// 这里断言的是「输入里的字面反斜杠 + n」与「输入里的真实换行」输出不同。
    #[test]
    fn literal_backslash_n_is_not_confused_with_newline() {
        let literal = json_string(r"a\nb");
        let newline = json_string("a\nb");
        assert_ne!(literal, newline);
        assert!(round_trips(r"a\nb"));
        assert!(round_trips("a\nb"));
    }

    /// 非 ASCII 原样输出：报告给人看，`\u4e2d` 会把可读性毁掉。
    /// 这与 `canonical_json` 的取值一致，两边不会因为编码风格产生分歧。
    #[test]
    fn non_ascii_is_kept_readable() {
        assert_eq!(json_string("中文与 emoji 🙂"), "\"中文与 emoji 🙂\"");
        assert!(round_trips("中文与 emoji 🙂"));
    }

    #[test]
    fn empty_string_and_option_wrapper() {
        assert_eq!(json_string(""), "\"\"");
        assert_eq!(opt_json_string(None), "null");
        assert_eq!(opt_json_string(Some("x")), "\"x\"");
    }

    /// 全 BMP 抽查：拼接后可解析、长度与内容一致。
    ///
    /// 只抽 0x00—0x7F 与若干多字节区段，避免测试本身变慢；控制字符已在上面穷举。
    #[test]
    fn mixed_ascii_and_multibyte_round_trips() {
        let mut raw = String::new();
        for code in 0u32..=0x7F {
            if let Some(ch) = char::from_u32(code) {
                raw.push(ch);
            }
        }
        raw.push_str("αβγ 中文 🙂 ẞ ｱ");
        raw.push('\u{2028}');
        raw.push('\u{FFFD}');
        assert!(round_trips(&raw));
    }
}
