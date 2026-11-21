//! 本地控制面的 Host / Origin 校验（CR-007，V2.9.6 §8「宿主校验允许的 Host、
//! 适用的 Origin 和动作权限」）。
//!
//! 为什么需要：控制面绑在 127.0.0.1，但**浏览器不区分「本机」与「外部页面」**。
//! DNS rebinding 能让 `evil.com` 解析到 127.0.0.1，页面上的脚本就能对本机端口发请求——
//! 此时 Host 头是 `evil.com`。只要服务不校验 Host，这种请求与真实客户端无从区分。
//! 现有 Bearer 兜底挡得住拿不到密钥的攻击者，但 `POST /_bod/handshake` 用的是描述文件里的
//! 一次性令牌、不走 Bearer，因此 Host 校验是那条路径上唯一的网络层防线。
//!
//! 三层判定，顺序即优先级：
//! 1. **Host 必须恰有一个**：缺失是畸形请求，重复是请求走私的典型手法，都直接拒；
//! 2. **Host 必须在白名单**：回环名 + 本进程实际监听的端口；
//! 3. **Origin 若存在必须在白名单**：不存在表示非浏览器客户端（CLI、Tauri 的 Rust 侧），
//!    放行；存在则精确匹配，`null` 一律拒（沙箱 iframe 与 `file://` 的通用来源）。
//!
//! 不做的事（明确边界，避免看起来比实际更强）：
//! - **不发 CORS 头**。加 `Access-Control-Allow-Origin` 会让浏览器真的能读响应，
//!   等于主动扩大攻击面；当前工作台不经 WebView fetch 直连控制面，没有这个需求。
//! - **不校验 Referer**。Origin 已覆盖跨源场景，Referer 可被策略剥掉，加了只是噪声。
//! - **不替代 Bearer**。Host/Origin 是网络层过滤，鉴权仍在 handler 里各自做。

/// Host 被拒的原因。每一条都能对应到一次真实的攻击面，不是内部实现细节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRejection {
    /// 请求没有 Host 头。HTTP/1.1 要求必填，缺失即畸形。
    MissingHost,
    /// 多个 Host 头。请求走私与缓存投毒的经典构造，不作「取第一个」的妥协。
    DuplicateHost,
    /// Host 不在白名单（典型：DNS rebinding 带来的外部域名）。
    HostNotAllowed { presented: String },
    /// Origin 存在但不在白名单（典型：第三方页面发起的跨源请求）。
    OriginNotAllowed { presented: String },
}

impl HostRejection {
    /// 稳定的机器可读代码，供客户端与测试断言。
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingHost => "BOD_HOST_MISSING",
            Self::DuplicateHost => "BOD_HOST_DUPLICATED",
            Self::HostNotAllowed { .. } => "BOD_HOST_NOT_ALLOWED",
            Self::OriginNotAllowed { .. } => "BOD_ORIGIN_NOT_ALLOWED",
        }
    }

    /// 给人看的原因。回显值经过净化：只留可打印 ASCII 且截断，避免控制字符进日志。
    pub fn explain(&self) -> String {
        match self {
            Self::MissingHost => "请求缺少 Host 头".to_string(),
            Self::DuplicateHost => "请求含多个 Host 头".to_string(),
            Self::HostNotAllowed { presented } => {
                format!("Host 不在允许列表：{}", sanitize(presented))
            }
            Self::OriginNotAllowed { presented } => {
                format!("Origin 不在允许列表：{}", sanitize(presented))
            }
        }
    }
}

/// 控制面的 Host/Origin 策略。
///
/// 只存「允许什么」，不存「监听在哪」——监听地址在建策略时已折算成白名单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPolicy {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

/// Tauri 应用的页面来源。Windows 用 `http://tauri.localhost`，macOS/Linux 用
/// `tauri://localhost`；两者都列，避免平台差异变成「某个平台安静地不校验」。
const TAURI_ORIGINS: &[&str] = &["tauri://localhost", "http://tauri.localhost"];

impl HostPolicy {
    /// 按本进程实际监听的地址构造。
    ///
    /// `bound` 是 `TcpListener::local_addr()` 的字符串形式，形如 `127.0.0.1:54321`
    /// 或 `[::1]:54321`。端口必须能取到——取不到说明调用方给错了地址，这里不猜。
    pub fn for_local_control_plane(bound: &str) -> Result<Self, String> {
        let (host, port) = split_host_port(bound)
            .ok_or_else(|| format!("监听地址缺少端口，无法构造 Host 策略：{bound}"))?;

        let mut allowed_hosts = vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        // 通配绑定（0.0.0.0 / [::]）**不**加入白名单：那种写法意味着监听所有网卡，
        // 若把局域网 IP 也放进来，Host 校验就形同虚设。显式写死某个 IP 才尊重它。
        if !is_wildcard(host) {
            let literal = format!("{}:{port}", host.to_ascii_lowercase());
            if !allowed_hosts.contains(&literal) {
                allowed_hosts.push(literal);
            }
        }

        Ok(Self {
            allowed_hosts,
            allowed_origins: TAURI_ORIGINS.iter().map(|s| s.to_string()).collect(),
        })
    }

    /// 追加一个允许的 Origin。用于本地开发服务器（如 `http://localhost:5173`）。
    ///
    /// 开发来源不在默认白名单里：它只应在开发者显式调用时生效，不能让「本地起个
    /// dev server」变成发布产物的默认攻击面。返回 `self` 便于链式构造。
    pub fn with_origin(mut self, origin: &str) -> Self {
        let normalized = origin.trim().to_ascii_lowercase();
        if !normalized.is_empty() && !self.allowed_origins.contains(&normalized) {
            self.allowed_origins.push(normalized);
        }
        self
    }

    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    pub fn allowed_origins(&self) -> &[String] {
        &self.allowed_origins
    }

    /// 判定一次请求。
    ///
    /// `hosts` / `origins` 是请求里**同名字段的全部取值**（正常各 0 或 1 个）。
    /// 传全部而不是「第一个」，是因为重复 Host 本身就是要拒的信号。
    pub fn check(&self, hosts: &[&str], origins: &[&str]) -> Result<(), HostRejection> {
        match hosts.len() {
            0 => return Err(HostRejection::MissingHost),
            1 => {}
            _ => return Err(HostRejection::DuplicateHost),
        }
        let presented = hosts[0].trim().to_ascii_lowercase();
        if !self.allowed_hosts.contains(&presented) {
            return Err(HostRejection::HostNotAllowed {
                presented: hosts[0].to_string(),
            });
        }

        for origin in origins {
            let normalized = origin.trim().to_ascii_lowercase();
            // `null` 是 opaque origin：沙箱 iframe、`data:`、`file://` 都会得到它。
            // 它不带任何来源信息，等于「我不知道自己是谁」，不能当白名单成员。
            if !self.allowed_origins.contains(&normalized) {
                return Err(HostRejection::OriginNotAllowed {
                    presented: origin.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// `127.0.0.1:1234` / `[::1]:1234` → (`127.0.0.1`, `1234`) / (`[::1]`, `1234`)。
fn split_host_port(bound: &str) -> Option<(&str, &str)> {
    let (host, port) = bound.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((host, port))
}

fn is_wildcard(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "[::]" | "::" | "*")
}

/// 回显净化：只留可打印 ASCII，最长 96 字符。
///
/// 不追求「保留原样以便排查」——这个值来自未通过校验的请求，属于攻击者可控输入，
/// 进日志与 UI 前必须先削掉控制字符。
fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
        .take(96)
        .collect();
    if cleaned.is_empty() {
        "<空或不可打印>".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::{HostPolicy, HostRejection, split_host_port};

    fn policy() -> HostPolicy {
        HostPolicy::for_local_control_plane("127.0.0.1:54123").expect("监听地址合法")
    }

    #[test]
    fn loopback_names_on_the_real_port_pass() {
        let policy = policy();
        for host in ["127.0.0.1:54123", "localhost:54123", "[::1]:54123"] {
            assert!(policy.check(&[host], &[]).is_ok(), "回环名 {host} 应当通过");
        }
    }

    /// Host 比较不区分大小写：`LocalHost:54123` 与 `localhost:54123` 等价。
    #[test]
    fn host_comparison_is_case_insensitive() {
        assert!(policy().check(&["LocalHost:54123"], &[]).is_ok());
        assert!(policy().check(&["127.0.0.1:54123"], &[]).is_ok());
    }

    /// DNS rebinding 的核心用例：外部域名解析到 127.0.0.1，Host 仍是外部域名。
    #[test]
    fn foreign_domain_is_rejected() {
        let rejection = policy()
            .check(&["evil.example:54123"], &[])
            .expect_err("外部域名必须被拒");
        assert_eq!(rejection.code(), "BOD_HOST_NOT_ALLOWED");
        assert!(rejection.explain().contains("evil.example"));
    }

    /// 端口不符同样拒：同一台机上另一个服务的端口不是本控制面。
    #[test]
    fn wrong_port_is_rejected() {
        assert!(policy().check(&["127.0.0.1:9999"], &[]).is_err());
        assert!(policy().check(&["localhost"], &[]).is_err());
    }

    #[test]
    fn missing_and_duplicate_host_are_distinguished() {
        assert_eq!(
            policy().check(&[], &[]).unwrap_err(),
            HostRejection::MissingHost
        );
        assert_eq!(
            policy()
                .check(&["127.0.0.1:54123", "evil.example"], &[])
                .unwrap_err(),
            HostRejection::DuplicateHost
        );
    }

    /// 重复 Host 即便两个都合法也拒：这是走私手法，不该因为「有一个合法的」就放行。
    #[test]
    fn duplicate_even_if_both_allowed_is_rejected() {
        let rejection = policy()
            .check(&["127.0.0.1:54123", "127.0.0.1:54123"], &[])
            .unwrap_err();
        assert_eq!(rejection, HostRejection::DuplicateHost);
    }

    #[test]
    fn absent_origin_is_allowed_for_non_browser_clients() {
        assert!(policy().check(&["127.0.0.1:54123"], &[]).is_ok());
    }

    #[test]
    fn tauri_origins_are_allowed_on_both_platforms() {
        let policy = policy();
        for origin in ["tauri://localhost", "http://tauri.localhost"] {
            assert!(
                policy.check(&["127.0.0.1:54123"], &[origin]).is_ok(),
                "{origin} 应当通过"
            );
        }
        // 大小写差异不该导致平台间行为不一致。
        assert!(
            policy
                .check(&["127.0.0.1:54123"], &["HTTP://TAURI.LOCALHOST"])
                .is_ok()
        );
    }

    /// `null` 是不带来源信息的 opaque origin，必须拒。
    #[test]
    fn null_and_foreign_origins_are_rejected() {
        let policy = policy();
        for origin in ["null", "https://evil.example", "http://localhost:5173"] {
            let rejection = policy
                .check(&["127.0.0.1:54123"], &[origin])
                .expect_err("应当被拒");
            assert_eq!(rejection.code(), "BOD_ORIGIN_NOT_ALLOWED", "{origin}");
        }
    }

    #[test]
    fn dev_origin_is_opt_in_only() {
        let with_dev = policy().with_origin("http://localhost:5173");
        assert!(
            with_dev
                .check(&["127.0.0.1:54123"], &["http://localhost:5173"])
                .is_ok()
        );
        // 默认策略不含它——不影响未显式开启的实例。
        assert!(
            policy()
                .check(&["127.0.0.1:54123"], &["http://localhost:5173"])
                .is_err()
        );
    }

    /// 通配绑定不能顺带把局域网地址放进白名单，否则校验等于没有。
    #[test]
    fn wildcard_bind_does_not_whitelist_lan_addresses() {
        let policy = HostPolicy::for_local_control_plane("0.0.0.0:54123").expect("合法");
        assert_eq!(
            policy.allowed_hosts(),
            &[
                "127.0.0.1:54123".to_string(),
                "localhost:54123".to_string(),
                "[::1]:54123".to_string(),
            ]
        );
        assert!(policy.check(&["192.168.1.5:54123"], &[]).is_err());
    }

    /// 显式绑定某个具体地址时尊重它——这是操作者的决定，不是默认值。
    #[test]
    fn explicit_bind_address_is_whitelisted() {
        let policy = HostPolicy::for_local_control_plane("192.168.1.5:54123").expect("合法");
        assert!(policy.check(&["192.168.1.5:54123"], &[]).is_ok());
    }

    #[test]
    fn bind_address_without_port_is_an_error() {
        assert!(HostPolicy::for_local_control_plane("127.0.0.1").is_err());
        assert!(HostPolicy::for_local_control_plane("").is_err());
    }

    #[test]
    fn ipv6_bind_splits_correctly() {
        assert_eq!(split_host_port("[::1]:8080"), Some(("[::1]", "8080")));
        assert_eq!(
            split_host_port("127.0.0.1:8080"),
            Some(("127.0.0.1", "8080"))
        );
        assert_eq!(split_host_port("[::]:8080"), Some(("[::]", "8080")));
    }

    /// 回显必须削掉控制字符，否则攻击者可控内容会进日志与 UI。
    #[test]
    fn rejection_explain_strips_control_characters() {
        let rejection = policy()
            .check(&["evil\r\nX-Injected: 1:54123"], &[])
            .unwrap_err();
        let text = rejection.explain();
        assert!(!text.contains('\r'), "不得保留回车：{text:?}");
        assert!(!text.contains('\n'), "不得保留换行：{text:?}");
    }

    /// 空或全不可打印的回显不能变成空白，否则排查时看不出发生了什么。
    #[test]
    fn rejection_explain_is_never_blank() {
        let rejection = policy().check(&["\u{7}\u{1}"], &[]).unwrap_err();
        assert!(rejection.explain().contains("不可打印"));
    }
}
