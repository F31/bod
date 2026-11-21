//! 宿主本地控制面的最小实现：受保护握手与连接描述（P0 任务 8.1）。
//!
//! 存在的理由：`desktop.close_action` 与 `server.listen = 127.0.0.1:0` 的语义都要求
//! 「实际地址通过受保护握手交给桌面/CLI」，而此前 P0 只有配置字段、没有对端。
//! 本模块提供三件事：
//!
//! 1. `HostServer`：绑定回环地址，把 `协议/地址/pid/实例 ID/握手令牌` 原子写成描述文件
//!    （Unix 下 0600），并只回应 `/_bod/health`、`/_bod/handshake`、`/_bod/shutdown`
//!    与 `/_bod/events`（SSE 基线订阅，语义见 `crate::event_stream`）。
//! 2. `probe`：客户端读描述文件 → 确认实例存活 → 用令牌换会话密钥。
//! 3. `request_shutdown`：用会话密钥请求停止，供 `close_action=stop_host` 使用。
//!
//! 边界（如实记录，不假装是生产安全模型）：
//! - 令牌与会话密钥都只是本机同用户凭据；P0 不做 TLS，`auth_required` 的 TLS 分支未实现。
//! - 描述文件权限只能保证「同用户可读」以外的人不可读；Windows 依赖用户目录 ACL。
//! - 日志只允许出现地址、pid、实例 ID；令牌与会话密钥永不落日志。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 控制面协议标识：客户端必须原样确认，避免旧版壳连上新版宿主。
pub const PROTOCOL: &str = "bod-host/v1";
/// 描述文件名（位于 `BOD_HOME` 下）。
pub const DESCRIPTOR_FILE: &str = "host.endpoint.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// 描述文件缺失、损坏或协议不符。
    Descriptor(String),
    /// 地址不可达或连接超时。
    Unreachable(String),
    /// 令牌或会话密钥被拒绝。
    Denied,
    /// 响应不符合协议。
    Protocol(String),
    Io(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Descriptor(detail) => write!(f, "连接描述不可用：{detail}"),
            Self::Unreachable(detail) => write!(f, "宿主不可达：{detail}"),
            Self::Denied => write!(f, "握手凭据被拒绝"),
            Self::Protocol(detail) => write!(f, "控制面响应不符合协议：{detail}"),
            Self::Io(detail) => write!(f, "控制面 I/O 失败：{detail}"),
        }
    }
}

impl std::error::Error for HostError {}

/// 宿主写出的连接描述。持有握手令牌，因此此结构体本身不得打印。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostDescriptor {
    pub protocol: String,
    pub address: String,
    pub pid: u32,
    pub instance_id: String,
    pub handshake_token: String,
    pub written_at_ms: i64,
}

impl HostDescriptor {
    /// 可安全写日志的一段事实：不含令牌。
    pub fn public_summary(&self) -> String {
        format!(
            "{} address={} pid={} instance={}",
            self.protocol, self.address, self.pid, self.instance_id
        )
    }
}

/// 握手成功后的连接句柄：会话密钥只在内存里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedHost {
    pub descriptor: HostDescriptor,
    pub session_key: String,
}

/// P0 的宿主目录：`BOD_HOME` 优先，否则用户主目录下的 `.bod`。
pub fn default_home() -> PathBuf {
    if let Some(home) = std::env::var_os("BOD_HOME") {
        return PathBuf::from(home);
    }
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".bod")
}

/// 默认描述文件路径。
pub fn default_descriptor_path() -> PathBuf {
    default_home().join(DESCRIPTOR_FILE)
}

/// 令牌来源：`RandomState` 由 OS 熵初始化；P0 只用它做本机凭据，不当作密码学承诺。
fn random_token(label: &str) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut material = Vec::new();
    for round in 0..2u8 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write(label.as_bytes());
        hasher.write_u32(std::process::id());
        hasher.write_u8(round);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        hasher.write_u128(nanos);
        material.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    bod_core::artifact::sha256_hex(&material)
}

/// 定长比较：避免按字节提前返回泄漏令牌前缀。
fn token_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 原子写描述文件：同目录临时文件 → 设权限 → fsync → rename。
pub fn write_descriptor(path: &Path, descriptor: &HostDescriptor) -> Result<(), HostError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| HostError::Io(err.to_string()))?;
    }
    let body = serde_json::to_vec_pretty(descriptor)
        .map_err(|err| HostError::Descriptor(err.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp).map_err(|err| HostError::Io(err.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|err| HostError::Io(err.to_string()))?;
        }
        file.write_all(&body)
            .map_err(|err| HostError::Io(err.to_string()))?;
        file.sync_all()
            .map_err(|err| HostError::Io(err.to_string()))?;
    }
    std::fs::rename(&tmp, path).map_err(|err| HostError::Io(err.to_string()))?;
    Ok(())
}

/// 读取描述文件；不删除失效文件（那是宿主的责任）。
pub fn read_descriptor(path: &Path) -> Result<HostDescriptor, HostError> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| HostError::Descriptor(format!("{}: {err}", path.display())))?;
    let descriptor: HostDescriptor = serde_json::from_str(&text)
        .map_err(|err| HostError::Descriptor(format!("{}: {err}", path.display())))?;
    if descriptor.protocol != PROTOCOL {
        return Err(HostError::Descriptor(format!(
            "协议不符：期望 {PROTOCOL}，实际 {}",
            descriptor.protocol
        )));
    }
    Ok(descriptor)
}

/// 宿主服务端：绑定 → 写描述 → 阻塞服务，直到收到带正确会话密钥的停止请求。
pub struct HostServer {
    listener: TcpListener,
    descriptor: HostDescriptor,
    descriptor_path: PathBuf,
    session_key: String,
    /// 事件源：`GET /_bod/events` 的 SSE 基线订阅从这里读（P6 2.6 / RM06）。
    feed: std::sync::Arc<dyn crate::event_stream::EventFeed>,
}

impl HostServer {
    pub fn bind(listen: &str, descriptor_path: &Path, now_ms: i64) -> Result<Self, HostError> {
        Self::bind_with_feed(
            listen,
            descriptor_path,
            now_ms,
            std::sync::Arc::new(crate::event_stream::MemoryFeed::with_capacity(1024)),
        )
    }

    /// 用调用方给的事件源绑定。宿主自身发布的事件也走同一个源，避免两条序列。
    pub fn bind_with_feed(
        listen: &str,
        descriptor_path: &Path,
        now_ms: i64,
        feed: std::sync::Arc<dyn crate::event_stream::EventFeed>,
    ) -> Result<Self, HostError> {
        let listener = TcpListener::bind(listen)
            .map_err(|err| HostError::Io(format!("无法监听 {listen}: {err}")))?;
        let address = listener
            .local_addr()
            .map_err(|err| HostError::Io(err.to_string()))?
            .to_string();
        let descriptor = HostDescriptor {
            protocol: PROTOCOL.to_string(),
            address,
            pid: std::process::id(),
            instance_id: format!("inst-{}", &random_token("instance")[..16]),
            handshake_token: random_token("handshake"),
            written_at_ms: now_ms,
        };
        let server = Self {
            listener,
            descriptor,
            descriptor_path: descriptor_path.to_path_buf(),
            session_key: random_token("session"),
            feed,
        };
        write_descriptor(descriptor_path, &server.descriptor)?;
        Ok(server)
    }

    pub fn address(&self) -> String {
        self.descriptor.address.clone()
    }

    pub fn descriptor(&self) -> &HostDescriptor {
        &self.descriptor
    }

    /// 阻塞服务。返回 `Ok(())` 表示收到授权停止；描述文件随之移除。
    pub fn run(self) -> Result<(), HostError> {
        for incoming in self.listener.incoming() {
            let Ok(stream) = incoming else {
                continue;
            };
            match self.handle(stream) {
                Ok(true) => break,
                Ok(false) => {}
                Err(err) => {
                    // 单个连接出错不影响宿主；错误里不含凭据。
                    eprintln!("控制面连接失败：{err}");
                }
            }
        }
        let _ = std::fs::remove_file(&self.descriptor_path);
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream) -> Result<bool, HostError> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let request = read_request(&mut stream)?;
        match (request.method.as_str(), request.path_only()) {
            ("GET", "/_bod/events") => {
                // 订阅是长连接：放到独立线程，否则会把健康检查与停止请求堵在队列里。
                let feed = std::sync::Arc::clone(&self.feed);
                let key = self.session_key.clone();
                std::thread::spawn(move || {
                    if let Err(err) = stream_events(feed, &key, stream, &request) {
                        // 客户端断开是正常结束，但其它错误要留痕；错误里不含凭据。
                        eprintln!("订阅结束：{err}");
                    }
                });
                Ok(false)
            }
            ("GET", "/_bod/health") => {
                let body = serde_json::json!({
                    "ok": true,
                    "protocol": PROTOCOL,
                    "instance_id": self.descriptor.instance_id,
                    "pid": self.descriptor.pid,
                });
                respond(&mut stream, 200, &body.to_string())?;
                Ok(false)
            }
            ("POST", "/_bod/handshake") => {
                let token = serde_json::from_str::<serde_json::Value>(&request.body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("handshake_token")
                            .and_then(|token| token.as_str())
                            .map(str::to_string)
                    });
                match token {
                    Some(token) if token_eq(&token, &self.descriptor.handshake_token) => {
                        let body = serde_json::json!({
                            "protocol": PROTOCOL,
                            "instance_id": self.descriptor.instance_id,
                            "pid": self.descriptor.pid,
                            "session_key": self.session_key,
                        });
                        respond(&mut stream, 200, &body.to_string())?;
                        Ok(false)
                    }
                    _ => {
                        respond(
                            &mut stream,
                            401,
                            &serde_json::json!({ "error": "BOD_HANDSHAKE_DENIED" }).to_string(),
                        )?;
                        Ok(false)
                    }
                }
            }
            ("POST", "/_bod/shutdown") => {
                let presented = request
                    .header("authorization")
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .unwrap_or_default();
                if token_eq(presented, &self.session_key) {
                    respond(
                        &mut stream,
                        200,
                        &serde_json::json!({ "stopping": true }).to_string(),
                    )?;
                    Ok(true)
                } else {
                    respond(
                        &mut stream,
                        401,
                        &serde_json::json!({ "error": "BOD_SHUTDOWN_DENIED" }).to_string(),
                    )?;
                    Ok(false)
                }
            }
            _ => {
                respond(
                    &mut stream,
                    404,
                    &serde_json::json!({ "error": "BOD_NOT_FOUND" }).to_string(),
                )?;
                Ok(false)
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 不含查询串的路径：路由匹配一律用它，避免 `?cursor=3` 落到 404。
    fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }

    fn query(&self) -> Option<&str> {
        self.path.split_once('?').map(|(_, query)| query)
    }

    fn query_param(&self, name: &str) -> Option<&str> {
        self.query()?.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then_some(value)
        })
    }

    /// 凭据是否被塞进查询串。SSE 不接受这种传法：查询串会进日志与浏览器历史。
    fn has_query_token(&self) -> bool {
        self.query().is_some_and(|query| {
            query.split('&').any(|pair| {
                let key = pair.split_once('=').map(|(key, _)| key).unwrap_or(pair);
                matches!(key, "token" | "session_key" | "access_token")
            })
        })
    }
}

/// `GET /_bod/events` 的实现：把补齐计划写成 SSE 帧，必要时保活并继续推送增量。
///
/// 三条如实边界：
/// - 鉴权只认 `Authorization: Bearer`，不接受查询串令牌；
/// - 需要整体重订阅时**立刻结束**，不发心跳伪装成正常订阅；
/// - 写失败（客户端慢/断）就结束，游标由客户端下次带 `Last-Event-ID` 回来补。
fn stream_events(
    feed: std::sync::Arc<dyn crate::event_stream::EventFeed>,
    session_key: &str,
    mut stream: TcpStream,
    request: &Request,
) -> Result<(), HostError> {
    use crate::event_stream::{
        CursorRequest, DEFAULT_BACKLOG_LIMIT, DEFAULT_RETRY_MS, StreamFrame, StreamStats,
        authorize_stream, plan_stream, render_plan,
    };

    let auth = authorize_stream(
        request.header("authorization"),
        request.has_query_token(),
        session_key,
        token_eq,
    );
    if !auth.is_authorized() {
        let body = serde_json::json!({
            "error": "BOD_STREAM_DENIED",
            "reason": auth.explain(),
        });
        respond(&mut stream, 401, &body.to_string())?;
        return Ok(());
    }

    let cursor = CursorRequest::resolve(
        request.header("last-event-id"),
        request.query_param("cursor"),
        None,
    );
    let plan = plan_stream(&*feed, &cursor, false, DEFAULT_BACKLOG_LIMIT);
    let follow = request.query_param("follow") != Some("0");
    let max_ms = request
        .query_param("max_ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000)
        .clamp(100, 60_000);
    let heartbeat_ms = request
        .query_param("heartbeat_ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000)
        .clamp(20, 10_000);

    begin_stream(&mut stream)?;
    let mut stats = StreamStats::default();
    for frame in render_plan(&plan, DEFAULT_RETRY_MS) {
        write_frame(&mut stream, &frame)?;
        stats.record_frame(&frame);
    }
    if plan.is_resync() || !follow {
        return Ok(());
    }

    // 继续跟随：有新事件就推增量，没有就心跳。中途出现空洞要立刻 resync 并停止，
    // 否则客户端会拿着一个"看起来连续"的游标继续跑。
    let mut cursor_now = plan.next_cursor().unwrap_or(0);
    let deadline = std::time::Instant::now() + Duration::from_millis(max_ms);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(heartbeat_ms));
        let head = feed.head();
        if head > cursor_now {
            let events = feed.read_from(cursor_now + 1, DEFAULT_BACKLOG_LIMIT);
            let continuous = events
                .first()
                .map(|event| event.sequence == cursor_now + 1)
                .unwrap_or(false);
            if !continuous {
                let frame = StreamFrame::Resync {
                    reason: "跟随期间增量出现空洞",
                    retry_ms: DEFAULT_RETRY_MS,
                };
                let _ = write_frame(&mut stream, &frame);
                stats.record_frame(&frame);
                return Ok(());
            }
            for event in events {
                let frame = StreamFrame::Event(event);
                if write_frame(&mut stream, &frame).is_err() {
                    return Ok(());
                }
                stats.record_frame(&frame);
            }
            cursor_now = stats.last_cursor.unwrap_or(cursor_now);
            continue;
        }
        if write_frame(&mut stream, &StreamFrame::Heartbeat).is_err() {
            return Ok(());
        }
        stats.record_frame(&StreamFrame::Heartbeat);
    }
    Ok(())
}

/// 写 SSE 响应头：没有 `Content-Length`，也不复用连接。
fn begin_stream(stream: &mut TcpStream) -> Result<(), HostError> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        crate::event_stream::SSE_CONTENT_TYPE
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|err| HostError::Io(err.to_string()))?;
    stream.flush().map_err(|err| HostError::Io(err.to_string()))
}

fn write_frame(
    stream: &mut TcpStream,
    frame: &crate::event_stream::StreamFrame,
) -> Result<(), HostError> {
    stream
        .write_all(frame.encode().as_bytes())
        .map_err(|err| HostError::Io(err.to_string()))?;
    stream.flush().map_err(|err| HostError::Io(err.to_string()))
}

fn read_request(stream: &mut TcpStream) -> Result<Request, HostError> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    // 头部上限 16 KiB：控制面请求只该有几十字节。
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| HostError::Protocol(err.to_string()))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 16 * 1024 {
            return Err(HostError::Protocol("请求头超过 16 KiB".to_string()));
        }
    }
    let head_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| HostError::Protocol("缺少请求头结束标记".to_string()))?;
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines
        .next()
        .ok_or_else(|| HostError::Protocol("缺少请求行".to_string()))?
        .split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }

    let mut request = Request {
        method,
        path,
        headers,
        body: String::new(),
    };
    let length: usize = request
        .header("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| HostError::Protocol(err.to_string()))?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    request.body = String::from_utf8_lossy(&body[..body.len().min(length)]).to_string();
    Ok(request)
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> Result<(), HostError> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|err| HostError::Io(err.to_string()))?;
    let _ = stream.flush();
    Ok(())
}

fn roundtrip(address: &str, timeout: Duration, request: &str) -> Result<(u16, String), HostError> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|err| HostError::Unreachable(format!("{address}: {err}")))?;
    let mut stream = TcpStream::connect_timeout(&socket, timeout)
        .map_err(|err| HostError::Unreachable(format!("{address}: {err}")))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    stream
        .write_all(request.as_bytes())
        .map_err(|err| HostError::Io(err.to_string()))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| HostError::Io(err.to_string()))?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HostError::Protocol("缺少状态行".to_string()))?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

fn post_json(
    address: &str,
    path: &str,
    body: &str,
    bearer: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), HostError> {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(token) = bearer {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    roundtrip(address, timeout, &request)
}

/// 客户端探测：读描述 → 确认协议一致 → 握手换会话密钥。
pub fn probe(descriptor_path: &Path, timeout: Duration) -> Result<AttachedHost, HostError> {
    let descriptor = read_descriptor(descriptor_path)?;
    let (status, body) = roundtrip(
        &descriptor.address,
        timeout,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            descriptor.address
        ),
    )?;
    if status != 200 {
        return Err(HostError::Unreachable(format!(
            "健康检查返回 {status}（描述可能已过期）"
        )));
    }
    let health: serde_json::Value =
        serde_json::from_str(&body).map_err(|err| HostError::Protocol(err.to_string()))?;
    if health.get("instance_id").and_then(|value| value.as_str())
        != Some(descriptor.instance_id.as_str())
    {
        return Err(HostError::Descriptor(
            "描述文件的实例与监听者不一致（地址已被他人复用）".to_string(),
        ));
    }

    let payload = serde_json::json!({ "handshake_token": descriptor.handshake_token });
    let (status, body) = post_json(
        &descriptor.address,
        "/_bod/handshake",
        &payload.to_string(),
        None,
        timeout,
    )?;
    match status {
        200 => {
            let reply: serde_json::Value =
                serde_json::from_str(&body).map_err(|err| HostError::Protocol(err.to_string()))?;
            let session_key = reply
                .get("session_key")
                .and_then(|value| value.as_str())
                .ok_or_else(|| HostError::Protocol("握手响应缺少 session_key".to_string()))?
                .to_string();
            Ok(AttachedHost {
                descriptor,
                session_key,
            })
        }
        401 => Err(HostError::Denied),
        other => Err(HostError::Protocol(format!("握手返回 {other}"))),
    }
}

/// 请求宿主停止；只应在 `close_action=stop_host` 且宿主由本次启动时调用。
pub fn request_shutdown(
    address: &str,
    session_key: &str,
    timeout: Duration,
) -> Result<(), HostError> {
    let (status, _) = post_json(address, "/_bod/shutdown", "{}", Some(session_key), timeout)?;
    match status {
        200 => Ok(()),
        401 => Err(HostError::Denied),
        other => Err(HostError::Protocol(format!("停止返回 {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_descriptor(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bod-host-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(DESCRIPTOR_FILE)
    }

    /// 起一个真实宿主线程；返回地址、描述路径与停止句柄。
    fn start_host(tag: &str) -> (PathBuf, String, AttachedHost) {
        let path = temp_descriptor(tag);
        let server = HostServer::bind("127.0.0.1:0", &path, 1_763_500_000_000).unwrap();
        let address = server.address();
        let instance = server.instance_id_for_test();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let _ = server.run();
        });
        ready_rx.recv().unwrap();
        let attached = probe(&path, Duration::from_secs(2)).unwrap();
        assert_eq!(attached.descriptor.instance_id, instance);
        (path, address, attached)
    }

    impl HostServer {
        fn instance_id_for_test(&self) -> String {
            self.descriptor.instance_id.clone()
        }
    }

    #[test]
    fn descriptor_is_written_without_secret_in_summary() {
        let path = temp_descriptor("descriptor");
        let server = HostServer::bind("127.0.0.1:0", &path, 1_763_500_000_000).unwrap();
        let descriptor = read_descriptor(&path).unwrap();
        assert_eq!(descriptor.protocol, PROTOCOL);
        assert_eq!(descriptor.address, server.address());
        assert_eq!(descriptor.pid, std::process::id());
        assert!(!descriptor.handshake_token.is_empty());
        // 可公开摘要里不能出现令牌。
        assert!(
            !descriptor
                .public_summary()
                .contains(&descriptor.handshake_token)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_descriptor("perm");
        let _server = HostServer::bind("127.0.0.1:0", &path, 1_763_500_000_000).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "描述文件含握手令牌，必须 0600");
    }

    #[test]
    fn probe_returns_session_key_and_rejects_wrong_token() {
        let (path, _address, attached) = start_host("probe");
        assert!(!attached.session_key.is_empty());

        let descriptor = read_descriptor(&path).unwrap();
        let (status, _) = post_json(
            &descriptor.address,
            "/_bod/handshake",
            &serde_json::json!({ "handshake_token": "wrong" }).to_string(),
            None,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status, 401);

        // 收尾：带正确会话密钥停止宿主。
        request_shutdown(
            &attached.descriptor.address,
            &attached.session_key,
            Duration::from_secs(2),
        )
        .unwrap();
    }

    #[test]
    fn shutdown_requires_session_key() {
        let (_path, address, attached) = start_host("shutdown");
        let (status, _) = post_json(
            &address,
            "/_bod/shutdown",
            "{}",
            Some("not-the-key"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status, 401);
        request_shutdown(&address, &attached.session_key, Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn health_never_returns_secret_material() {
        let (_path, address, attached) = start_host("health");
        let (status, body) = roundtrip(
            &address,
            Duration::from_secs(2),
            &format!("GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
        )
        .unwrap();
        assert_eq!(status, 200);
        assert!(!body.contains(&attached.session_key));
        assert!(!body.contains(&attached.descriptor.handshake_token));
        assert!(body.contains(&attached.descriptor.instance_id));
        request_shutdown(&address, &attached.session_key, Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn stale_descriptor_reports_unreachable_instead_of_hanging() {
        let path = temp_descriptor("stale");
        // 端口从未监听：必须快速失败，而不是让桌面卡住。
        let descriptor = HostDescriptor {
            protocol: PROTOCOL.to_string(),
            address: "127.0.0.1:9".to_string(),
            pid: 1,
            instance_id: "inst-stale".to_string(),
            handshake_token: "token".to_string(),
            written_at_ms: 0,
        };
        write_descriptor(&path, &descriptor).unwrap();
        let error = probe(&path, Duration::from_millis(300)).unwrap_err();
        assert!(matches!(error, HostError::Unreachable(_)));
    }

    #[test]
    fn descriptor_with_unknown_field_is_rejected() {
        let path = temp_descriptor("unknown");
        std::fs::write(
            &path,
            r#"{"protocol":"bod-host/v1","address":"127.0.0.1:1","pid":1,
                "instance_id":"x","handshake_token":"t","written_at_ms":0,"extra":1}"#,
        )
        .unwrap();
        let error = read_descriptor(&path).unwrap_err();
        assert!(matches!(error, HostError::Descriptor(_)));
    }
}
