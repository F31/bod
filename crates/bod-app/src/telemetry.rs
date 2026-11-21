//! 结构化诊断日志（CR-002 / V2.9.8 §3、§8、§9）。
//!
//! 分工（写清楚，否则会变成两套重复设施）：
//! - **业务审计** → `audit_log` + `audit_events` 表，回答「谁基于什么证据做了什么决定」，
//!   长期保留、可查询、不可改写；
//! - **诊断日志** → 本模块，回答「刚才那次请求为什么慢/为什么失败」，可滚动丢弃。
//!
//! 两者共用 `correlation_id`（一次用户动作贯穿的链）：日志里有 ID、审计表里也有同一个
//! ID，才能从一条告警追到具体判定。
//!
//! 输出目标固定为 stderr：stdout 是 CLI 的命令结果与宿主的就绪行，
//! spawner 依赖 `stdout` 第一行判断就绪——日志混进 stdout 会打断这个约定。
//!
//! 为什么不用 `env-filter`：它带 regex + aho-corasick 进生产依赖树，
//! 而本仓只需要「按级别过滤」，用 `with_max_level` 就够（理由记在 ADR-0006）。

use tracing_subscriber::filter::LevelFilter;

/// 日志格式。dev 用 `Compact`（人读），prod 用 `Json`（机器读、可聚合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Compact,
    Json,
}

impl LogFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Json => "json",
        }
    }
}

/// 解析格式串。未知值落 `Compact` 而不是报错：
/// 日志格式是运维参数，拼错不该让宿主起不来——但也不能静默变成 JSON 混进人读终端。
pub fn parse_format(text: &str) -> LogFormat {
    match text.trim().to_ascii_lowercase().as_str() {
        "json" => LogFormat::Json,
        _ => LogFormat::Compact,
    }
}

/// 解析级别串。空值与未知值落 `INFO`：默认安静但不失明。
pub fn parse_level(text: &str) -> LevelFilter {
    match text.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::OFF,
        "error" => LevelFilter::ERROR,
        "warn" | "warning" => LevelFilter::WARN,
        "debug" => LevelFilter::DEBUG,
        "trace" => LevelFilter::TRACE,
        _ => LevelFilter::INFO,
    }
}

/// 从环境变量读：`BOD_LOG`（off/error/warn/info/debug/trace）、
/// `BOD_LOG_FORMAT`（compact/json）。
pub fn level_from_env() -> LevelFilter {
    parse_level(&std::env::var("BOD_LOG").unwrap_or_default())
}

pub fn format_from_env() -> LogFormat {
    parse_format(&std::env::var("BOD_LOG_FORMAT").unwrap_or_default())
}

/// 安装全局 subscriber。进程内只应调用一次；重复调用返回 `Ok`——
/// 宿主可能被嵌进已设好 subscriber 的进程（桌面壳、测试），那不是错误。
///
/// 返回 `Err` 只表示「配置本身不合法」，此时调用方应把错误打出来再继续，
/// 不该因为装不上日志就拒绝启动。
pub fn init(format: LogFormat, level: LevelFilter) -> Result<(), String> {
    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr);
    let installed = match format {
        LogFormat::Json => builder.json().try_init(),
        LogFormat::Compact => builder.compact().try_init(),
    };
    match installed {
        Ok(()) => Ok(()),
        Err(err) if err.to_string().contains("already") => Ok(()),
        Err(err) => Err(format!("匿名日志初始化失败：{err}")),
    }
}

/// 用环境变量一步安装，供 CLI 入口调用。
pub fn init_from_env() -> Result<(), String> {
    init(format_from_env(), level_from_env())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parsing_defaults_to_compact() {
        assert_eq!(parse_format("json"), LogFormat::Json);
        assert_eq!(parse_format("  JSON "), LogFormat::Json);
        assert_eq!(parse_format(""), LogFormat::Compact);
        assert_eq!(
            parse_format("yaml"),
            LogFormat::Compact,
            "未知值不得静默变 JSON"
        );
    }

    #[test]
    fn level_parsing_covers_all_five_levels_and_falls_back_to_info() {
        assert_eq!(parse_level("off"), LevelFilter::OFF);
        assert_eq!(parse_level("ERROR"), LevelFilter::ERROR);
        assert_eq!(parse_level("warning"), LevelFilter::WARN);
        assert_eq!(parse_level("warn"), LevelFilter::WARN);
        assert_eq!(parse_level("info"), LevelFilter::INFO);
        assert_eq!(parse_level("debug"), LevelFilter::DEBUG);
        assert_eq!(parse_level("trace"), LevelFilter::TRACE);
        assert_eq!(parse_level(""), LevelFilter::INFO);
        assert_eq!(parse_level("verbose"), LevelFilter::INFO);
    }

    /// 事件真的会送达 subscriber。这条不是测 tracing 库，而是测「宏没被 feature 裁掉」：
    /// 只引 `tracing` 不引 subscriber 时，事件会静默消失且编译不报错。
    /// 用 `with_default` 而不是 `try_init`：全局只能装一次，测试之间会互相干扰。
    #[test]
    fn events_reach_a_subscriber() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tracing::subscriber::with_default;

        static COUNT: AtomicUsize = AtomicUsize::new(0);

        struct Counter;
        impl tracing::Subscriber for Counter {
            fn enabled(&self, _meta: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {
            }
            fn event(&self, _event: &tracing::Event<'_>) {
                COUNT.fetch_add(1, Ordering::SeqCst);
            }
            fn enter(&self, _span: &tracing::span::Id) {}
            fn exit(&self, _span: &tracing::span::Id) {}
        }

        with_default(Counter, || {
            tracing::info!("计数一次");
        });
        assert_eq!(
            COUNT.load(Ordering::SeqCst),
            1,
            "测试用 subscriber 必须收到事件"
        );
    }
}
