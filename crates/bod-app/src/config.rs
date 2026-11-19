//! 配置 schema 与 explain（P0 D0 任务 1.2）。
//!
//! 权威来源：bod-V2.9.6 §37.9「配置与默认值」与 §37.14「P0 配置样例与冻结纪律」。
//! 所有字段必须先在 [`FIELD_DOCS`] 中定义默认值、来源与语义，才允许被宿主使用。
//! §37.14 的数值是启动实验建议初值，不是实测容量承诺；冻结须经 P0 故障/负载测试。

use serde::Deserialize;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write as _;

/// 默认值来源：必须在每个 explain 输出中可见，禁止隐藏在适配器分支。
pub const BASELINE_SOURCE: &str = "内置默认：§37.14 建议初值（D0 未冻结，待故障/负载实测）";

/// schema 的 section → 允许字段。未知 section/字段一律拒绝。
pub const SECTION_FIELDS: &[(&str, &[&str])] = &[
    ("server", &["listen", "auth_required"]),
    ("desktop", &["framework", "host_mode", "close_action"]),
    (
        "sqlite",
        &[
            "busy_timeout_ms",
            "transaction_retry_max",
            "write_request_deadline_ms",
            "synchronous",
        ],
    ),
    (
        "queue",
        &[
            "scan_interval_ms",
            "claim_lease_seconds",
            "max_retry_attempts",
            "retry_total_deadline_seconds",
        ],
    ),
    ("cache", &["max_entries", "max_bytes", "probe_ttl_seconds"]),
    ("reliability", &["minimum_retry_window_hours"]),
];

/// 字段元数据：类型、默认值、语义。explain 与 schema 输出均以本表为唯一来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDoc {
    pub path: &'static str,
    pub ty: &'static str,
    pub default: &'static str,
    pub semantics: &'static str,
}

pub const FIELD_DOCS: &[FieldDoc] = &[
    FieldDoc {
        path: "server.listen",
        ty: "string",
        default: "127.0.0.1:0",
        semantics: "默认回环监听；端口 0 表示由 OS 分配，实际地址通过受保护握手交给桌面/CLI，不从日志猜测，不暴露启动密钥（§37.14）。",
    },
    FieldDoc {
        path: "server.auth_required",
        ty: "bool",
        default: "true",
        semantics: "默认要求已认证本地会话；未显式配置认证与 TLS 前不接受远程访问（§37.9、§37.14）。",
    },
    FieldDoc {
        path: "desktop.framework",
        ty: "enum",
        default: "tauri2",
        semantics: "桌面壳框架；P0 基线为 tauri2，不默认执行第三方任意 JS，也不做微前端（§37.10、§37.14）。",
    },
    FieldDoc {
        path: "desktop.host_mode",
        ty: "enum",
        default: "connect_or_spawn",
        semantics: "桌面连接或拉起宿主；首次可手工启动 WSL 宿主并显式连接，自动唤起/端口发现后置（§37.8、§37.14）。",
    },
    FieldDoc {
        path: "desktop.close_action",
        ty: "enum",
        default: "detach",
        semantics: "detach 仅退出桌面，不终止外部已有宿主；首次使用展示后台运行策略（§37.14）。",
    },
    FieldDoc {
        path: "sqlite.busy_timeout_ms",
        ty: "u64",
        default: "500",
        semantics: "写锁等待上限；与重试次数和总 deadline 共同构成有界等待，默认同步与持久性不因尾延迟被静默改写（§37.9、§37.14）。",
    },
    FieldDoc {
        path: "sqlite.transaction_retry_max",
        ty: "u32",
        default: "3",
        semantics: "写冲突时在整个总 deadline 内重读并重试事务的次数上限，不是只重放最后一条 SQL 的次数（§37.14）。",
    },
    FieldDoc {
        path: "sqlite.write_request_deadline_ms",
        ty: "u64",
        default: "2000",
        semantics: "写请求总 deadline；超时返回带稳定 request_id 的可恢复 Busy，不把超时伪装成成功（§37.14）。",
    },
    FieldDoc {
        path: "sqlite.synchronous",
        ty: "enum",
        default: "FULL",
        semantics: "每个 DB 显式验证 WAL 与 synchronous 目标，P0 基线 FULL；不能依赖库默认值（§37.9、§37.14）。",
    },
    FieldDoc {
        path: "queue.scan_interval_ms",
        ty: "u64",
        default: "1000",
        semantics: "工程队列扫描间隔；派发意图必须持久化，进程内通知只能加速，不能作为唯一存储（§2.4、§37.14）。",
    },
    FieldDoc {
        path: "queue.claim_lease_seconds",
        ty: "u64",
        default: "30",
        semantics: "这是协调消费者的租约，不是 30 秒 Agent 执行超时；续期与过期 inspect 另有测试（§37.14）。",
    },
    FieldDoc {
        path: "queue.max_retry_attempts",
        ty: "u32",
        default: "5",
        semantics: "可安全重发动作的尝试次数上限；Unknown 外部动作不按此自动重试（§37.14）。",
    },
    FieldDoc {
        path: "queue.retry_total_deadline_seconds",
        ty: "u64",
        default: "600",
        semantics: "重试的总时间窗；同时受次数、错误类别与是否可安全重发约束（§37.14）。",
    },
    FieldDoc {
        path: "cache.max_entries",
        ty: "u64",
        default: "1024",
        semantics: "有界缓存条目上限，不是启动预分配；敏感删除与撤权不能等待 TTL（§37.11、§37.14）。",
    },
    FieldDoc {
        path: "cache.max_bytes",
        ty: "u64",
        default: "8388608",
        semantics: "有界缓存字节上限，不是启动预分配；队列、去重与正式 CAS 不使用缓存作为唯一存储（§37.11、§37.14）。",
    },
    FieldDoc {
        path: "cache.probe_ttl_seconds",
        ty: "u64",
        default: "30",
        semantics: "probe 结果缓存 TTL；该缓存可丢弃，不作为队列或 outbox 的持久替代（§37.11、§37.14）。",
    },
    FieldDoc {
        path: "reliability.minimum_retry_window_hours",
        ty: "u64",
        default: "168",
        semantics: "与对端协商的重投窗口下界；实际保留取已承诺窗口、活跃工作与未决风险所需的最大值，168 小时到期不删除 Unknown 去重（§37.14）。",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopFramework {
    #[default]
    Tauri2,
}

impl DesktopFramework {
    pub fn as_str(self) -> &'static str {
        "tauri2"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMode {
    #[default]
    ConnectOrSpawn,
}

impl HostMode {
    pub fn as_str(self) -> &'static str {
        "connect_or_spawn"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseAction {
    #[default]
    Detach,
}

impl CloseAction {
    pub fn as_str(self) -> &'static str {
        "detach"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum SqliteSynchronous {
    #[default]
    #[serde(rename = "FULL")]
    Full,
    #[serde(rename = "NORMAL")]
    Normal,
    #[serde(rename = "OFF")]
    Off,
}

impl SqliteSynchronous {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "FULL",
            Self::Normal => "NORMAL",
            Self::Off => "OFF",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub listen: String,
    pub auth_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopConfig {
    pub framework: DesktopFramework,
    pub host_mode: HostMode,
    pub close_action: CloseAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteConfig {
    pub busy_timeout_ms: u64,
    pub transaction_retry_max: u32,
    pub write_request_deadline_ms: u64,
    pub synchronous: SqliteSynchronous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    pub scan_interval_ms: u64,
    pub claim_lease_seconds: u64,
    pub max_retry_attempts: u32,
    pub retry_total_deadline_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    pub max_entries: u64,
    pub max_bytes: u64,
    pub probe_ttl_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliabilityConfig {
    pub minimum_retry_window_hours: u64,
}

/// 生效配置。所有字段的默认值均来自 §37.14，见 [`FIELD_DOCS`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodConfig {
    pub server: ServerConfig,
    pub desktop: DesktopConfig,
    pub sqlite: SqliteConfig,
    pub queue: QueueConfig,
    pub cache: CacheConfig,
    pub reliability: ReliabilityConfig,
}

// 原始片段用 Option 表达“未出现”，避免 serde 字段级 default 使用类型零值
// （false/0）覆盖 §37.14 默认值。合并的唯一默认值来源是各结构体的 Default 实现。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerConfig {
    listen: Option<String>,
    auth_required: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDesktopConfig {
    framework: Option<DesktopFramework>,
    host_mode: Option<HostMode>,
    close_action: Option<CloseAction>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSqliteConfig {
    busy_timeout_ms: Option<u64>,
    transaction_retry_max: Option<u32>,
    write_request_deadline_ms: Option<u64>,
    synchronous: Option<SqliteSynchronous>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQueueConfig {
    scan_interval_ms: Option<u64>,
    claim_lease_seconds: Option<u64>,
    max_retry_attempts: Option<u32>,
    retry_total_deadline_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCacheConfig {
    max_entries: Option<u64>,
    max_bytes: Option<u64>,
    probe_ttl_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReliabilityConfig {
    minimum_retry_window_hours: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBodConfig {
    server: Option<RawServerConfig>,
    desktop: Option<RawDesktopConfig>,
    sqlite: Option<RawSqliteConfig>,
    queue: Option<RawQueueConfig>,
    cache: Option<RawCacheConfig>,
    reliability: Option<RawReliabilityConfig>,
}

impl BodConfig {
    /// 逐键合并片段到 schema 默认值；未出现的字段保持 §37.14 初值。
    fn from_raw(raw: RawBodConfig) -> Self {
        let mut out = Self::default();
        if let Some(section) = raw.server {
            if let Some(v) = section.listen {
                out.server.listen = v;
            }
            if let Some(v) = section.auth_required {
                out.server.auth_required = v;
            }
        }
        if let Some(section) = raw.desktop {
            if let Some(v) = section.framework {
                out.desktop.framework = v;
            }
            if let Some(v) = section.host_mode {
                out.desktop.host_mode = v;
            }
            if let Some(v) = section.close_action {
                out.desktop.close_action = v;
            }
        }
        if let Some(section) = raw.sqlite {
            if let Some(v) = section.busy_timeout_ms {
                out.sqlite.busy_timeout_ms = v;
            }
            if let Some(v) = section.transaction_retry_max {
                out.sqlite.transaction_retry_max = v;
            }
            if let Some(v) = section.write_request_deadline_ms {
                out.sqlite.write_request_deadline_ms = v;
            }
            if let Some(v) = section.synchronous {
                out.sqlite.synchronous = v;
            }
        }
        if let Some(section) = raw.queue {
            if let Some(v) = section.scan_interval_ms {
                out.queue.scan_interval_ms = v;
            }
            if let Some(v) = section.claim_lease_seconds {
                out.queue.claim_lease_seconds = v;
            }
            if let Some(v) = section.max_retry_attempts {
                out.queue.max_retry_attempts = v;
            }
            if let Some(v) = section.retry_total_deadline_seconds {
                out.queue.retry_total_deadline_seconds = v;
            }
        }
        if let Some(section) = raw.cache {
            if let Some(v) = section.max_entries {
                out.cache.max_entries = v;
            }
            if let Some(v) = section.max_bytes {
                out.cache.max_bytes = v;
            }
            if let Some(v) = section.probe_ttl_seconds {
                out.cache.probe_ttl_seconds = v;
            }
        }
        if let Some(v) = raw
            .reliability
            .and_then(|section| section.minimum_retry_window_hours)
        {
            out.reliability.minimum_retry_window_hours = v;
        }
        out
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:0".to_string(),
            auth_required: true,
        }
    }
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            framework: DesktopFramework::Tauri2,
            host_mode: HostMode::ConnectOrSpawn,
            close_action: CloseAction::Detach,
        }
    }
}

impl Default for SqliteConfig {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 500,
            transaction_retry_max: 3,
            write_request_deadline_ms: 2000,
            synchronous: SqliteSynchronous::Full,
        }
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            scan_interval_ms: 1000,
            claim_lease_seconds: 30,
            max_retry_attempts: 5,
            retry_total_deadline_seconds: 600,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1024,
            max_bytes: 8_388_608,
            probe_ttl_seconds: 30,
        }
    }
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            minimum_retry_window_hours: 168,
        }
    }
}

impl BodConfig {
    /// 按配置路径取生效值；路径必须已在 [`FIELD_DOCS`] 中定义。
    pub fn value_of(&self, path: &str) -> Option<String> {
        let value = match path {
            "server.listen" => self.server.listen.clone(),
            "server.auth_required" => self.server.auth_required.to_string(),
            "desktop.framework" => self.desktop.framework.as_str().to_string(),
            "desktop.host_mode" => self.desktop.host_mode.as_str().to_string(),
            "desktop.close_action" => self.desktop.close_action.as_str().to_string(),
            "sqlite.busy_timeout_ms" => self.sqlite.busy_timeout_ms.to_string(),
            "sqlite.transaction_retry_max" => self.sqlite.transaction_retry_max.to_string(),
            "sqlite.write_request_deadline_ms" => self.sqlite.write_request_deadline_ms.to_string(),
            "sqlite.synchronous" => self.sqlite.synchronous.as_str().to_string(),
            "queue.scan_interval_ms" => self.queue.scan_interval_ms.to_string(),
            "queue.claim_lease_seconds" => self.queue.claim_lease_seconds.to_string(),
            "queue.max_retry_attempts" => self.queue.max_retry_attempts.to_string(),
            "queue.retry_total_deadline_seconds" => {
                self.queue.retry_total_deadline_seconds.to_string()
            }
            "cache.max_entries" => self.cache.max_entries.to_string(),
            "cache.max_bytes" => self.cache.max_bytes.to_string(),
            "cache.probe_ttl_seconds" => self.cache.probe_ttl_seconds.to_string(),
            "reliability.minimum_retry_window_hours" => {
                self.reliability.minimum_retry_window_hours.to_string()
            }
            _ => return None,
        };
        Some(value)
    }
}

/// 配置错误：未知字段必须给出完整路径；解析与类型错误分别报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    UnknownFields(Vec<String>),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "配置解析失败: {msg}"),
            Self::UnknownFields(paths) => {
                write!(f, "配置含未知字段: {}", paths.join(", "))
            }
            Self::Invalid(msg) => write!(f, "配置不符合 schema: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// 加载结果：生效配置 + 被文件显式覆盖的字段路径集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub config: BodConfig,
    pub overridden: BTreeSet<String>,
}

/// 片段校验：未知 section/字段必须在合并前被拒绝，同时收集覆盖路径。
pub fn validate_shape(value: &toml::Value) -> Result<BTreeSet<String>, ConfigError> {
    let root = value
        .as_table()
        .ok_or_else(|| ConfigError::Invalid("配置根必须是 table".to_string()))?;
    let mut unknown = Vec::new();
    let mut overridden = BTreeSet::new();

    for (section, section_value) in root {
        let Some((_, allowed)) = SECTION_FIELDS
            .iter()
            .find(|(name, _)| *name == section.as_str())
        else {
            unknown.push(section.clone());
            continue;
        };
        let table = section_value.as_table().ok_or_else(|| {
            ConfigError::Invalid(format!("[{section}] 必须是 table，不能是标量或数组"))
        })?;
        for (field, _) in table {
            if allowed.contains(&field.as_str()) {
                overridden.insert(format!("{section}.{field}"));
            } else {
                unknown.push(format!("{section}.{field}"));
            }
        }
    }

    unknown.sort();
    if unknown.is_empty() {
        Ok(overridden)
    } else {
        Err(ConfigError::UnknownFields(unknown))
    }
}

/// 从 TOML 文本加载配置：先校验片段形状，再做完整 schema 反序列化。
pub fn load_from_str(text: &str) -> Result<LoadedConfig, ConfigError> {
    let value: toml::Value =
        toml::from_str(text).map_err(|err| ConfigError::Parse(err.to_string()))?;
    let overridden = validate_shape(&value)?;
    let raw: RawBodConfig =
        toml::from_str(text).map_err(|err| ConfigError::Invalid(err.to_string()))?;
    let config = BodConfig::from_raw(raw);
    Ok(LoadedConfig { config, overridden })
}

/// 输出所有字段的 schema（类型与默认值），供 `bod config schema` 使用。
pub fn schema_text() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "bod config schema — §37.14 P0 配置基线（未冻结）");
    for field in FIELD_DOCS {
        let _ = writeln!(
            out,
            "{:<40} {:<8} default={}",
            field.path, field.ty, field.default
        );
    }
    out
}

/// 输出生效值、来源与语义；来源区分内置默认和文件覆盖。
pub fn explain(config: &BodConfig, overridden: &BTreeSet<String>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "bod config explain — 生效配置与来源");
    let _ = writeln!(out, "基线：{BASELINE_SOURCE}");
    let _ = writeln!(
        out,
        "字段：{}；文件覆盖：{}",
        FIELD_DOCS.len(),
        overridden.len()
    );

    for field in FIELD_DOCS {
        let value = config
            .value_of(field.path)
            .unwrap_or_else(|| "<未定义>".to_string());
        let source = if overridden.contains(field.path) {
            format!("文件覆盖（原默认 {}）", field.default)
        } else {
            BASELINE_SOURCE.to_string()
        };
        let _ = writeln!(out);
        let _ = writeln!(out, "[{}]", field.path);
        let _ = writeln!(out, "  类型   : {}", field.ty);
        let _ = writeln!(out, "  生效值 : {value}");
        let _ = writeln!(out, "  默认值 : {}", field.default);
        let _ = writeln!(out, "  来源   : {source}");
        let _ = writeln!(out, "  语义   : {}", field.semantics);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_doc_37_14() {
        let c = BodConfig::default();
        assert_eq!(c.server.listen, "127.0.0.1:0");
        assert!(c.server.auth_required);
        assert_eq!(c.desktop.framework, DesktopFramework::Tauri2);
        assert_eq!(c.desktop.host_mode, HostMode::ConnectOrSpawn);
        assert_eq!(c.desktop.close_action, CloseAction::Detach);
        assert_eq!(c.sqlite.busy_timeout_ms, 500);
        assert_eq!(c.sqlite.transaction_retry_max, 3);
        assert_eq!(c.sqlite.write_request_deadline_ms, 2000);
        assert_eq!(c.sqlite.synchronous, SqliteSynchronous::Full);
        assert_eq!(c.queue.scan_interval_ms, 1000);
        assert_eq!(c.queue.claim_lease_seconds, 30);
        assert_eq!(c.queue.max_retry_attempts, 5);
        assert_eq!(c.queue.retry_total_deadline_seconds, 600);
        assert_eq!(c.cache.max_entries, 1024);
        assert_eq!(c.cache.max_bytes, 8_388_608);
        assert_eq!(c.cache.probe_ttl_seconds, 30);
        assert_eq!(c.reliability.minimum_retry_window_hours, 168);
        assert_eq!(FIELD_DOCS.len(), 17);
    }

    #[test]
    fn every_field_has_source_and_semantics_in_explain() {
        let out = explain(&BodConfig::default(), &BTreeSet::new());
        for field in FIELD_DOCS {
            assert!(
                out.contains(&format!("[{}]", field.path)),
                "缺少字段 {}",
                field.path
            );
            assert!(out.contains(field.default), "缺少默认值 {}", field.path);
            assert!(out.contains(BASELINE_SOURCE), "缺少来源 {}", field.path);
            assert!(out.contains(field.semantics), "缺少语义 {}", field.path);
            assert!(
                BodConfig::default().value_of(field.path).is_some(),
                "value_of 未覆盖 {}",
                field.path
            );
        }
    }

    #[test]
    fn unknown_field_is_rejected_with_full_path() {
        let text = "[server]\nauth_require = true\n";
        let err = load_from_str(text).expect_err("未知字段必须被拒绝");
        assert_eq!(
            err,
            ConfigError::UnknownFields(vec!["server.auth_require".to_string()])
        );
    }

    #[test]
    fn unknown_section_and_nested_unknown_field_are_reported() {
        let text = "[bogus]\nx = 1\n[queue]\nextra = 2\n";
        let err = load_from_str(text).expect_err("未知 section/字段必须被拒绝");
        assert_eq!(
            err,
            ConfigError::UnknownFields(vec!["bogus".to_string(), "queue.extra".to_string(),])
        );
    }

    #[test]
    fn partial_override_keeps_defaults_and_tracks_source() {
        let text = "[server]\nlisten = \"127.0.0.1:8080\"\n";
        let loaded = load_from_str(text).expect("合法片段应通过");
        assert_eq!(loaded.config.server.listen, "127.0.0.1:8080");
        assert!(loaded.config.server.auth_required);
        assert_eq!(loaded.config.queue.claim_lease_seconds, 30);
        assert_eq!(loaded.overridden.len(), 1);
        assert!(loaded.overridden.contains("server.listen"));

        let out = explain(&loaded.config, &loaded.overridden);
        assert!(out.contains("文件覆盖（原默认 127.0.0.1:0）"));
        assert!(out.contains("文件覆盖：1"));
    }

    #[test]
    fn partial_section_keeps_schema_defaults() {
        let text = "[sqlite]\nbusy_timeout_ms = 900\n";
        let loaded = load_from_str(text).expect("合法片段应通过");
        assert_eq!(loaded.config.sqlite.busy_timeout_ms, 900);
        assert_eq!(loaded.config.sqlite.transaction_retry_max, 3);
        assert_eq!(loaded.config.sqlite.write_request_deadline_ms, 2000);
        assert_eq!(loaded.config.sqlite.synchronous, SqliteSynchronous::Full);
        assert_eq!(loaded.config.queue.max_retry_attempts, 5);
    }

    #[test]
    fn invalid_enum_value_is_rejected() {
        let text = "[desktop]\nframework = \"electron\"\n";
        let err = load_from_str(text).expect_err("未定义枚举值必须被拒绝");
        assert!(matches!(err, ConfigError::Invalid(_)), "实际错误: {err}");
    }

    #[test]
    fn malformed_toml_is_reported_as_parse_error() {
        let err = load_from_str("[server\nlisten = 1").expect_err("语法错误必须被拒绝");
        assert!(matches!(err, ConfigError::Parse(_)), "实际错误: {err}");
    }

    #[test]
    fn schema_text_covers_all_paths() {
        let out = schema_text();
        for field in FIELD_DOCS {
            assert!(out.contains(field.path));
            assert!(out.contains(field.default));
        }
    }
}
