//! 宿主本地控制面的最小实现：受保护握手与连接描述（P0 任务 8.1）。
//!
//! 存在的理由：`desktop.close_action` 与 `server.listen = 127.0.0.1:0` 的语义都要求
//! 「实际地址通过受保护握手交给桌面/CLI」，而此前 P0 只有配置字段、没有对端。
//! 本模块提供三件事：
//!
//! 1. `HostServer`：绑定回环地址，把 `协议/地址/pid/实例 ID/握手令牌` 原子写成描述文件
//!    （Unix 下 0600），并按 `ROUTES` 路由表回应控制面端点（含 SSE 基线订阅）。
//! 2. `probe`：客户端读描述文件 → 确认实例存活 → 用令牌换会话密钥。
//! 3. `request_shutdown`：用会话密钥请求停止，供 `close_action=stop_host` 使用。
//!
//! **本模块只做传输**（CR-003）：读请求、过 Host/Origin 关、分发、写响应。
//! 路径匹配与鉴权声明在 `crate::http_router`，加端点只改 `ROUTES` 表，
//! 不需要动 `handle`。
//!
//! 边界（如实记录，不假装是生产安全模型）：
//! - 令牌与会话密钥都只是本机同用户凭据；P0 不做 TLS，`auth_required` 的 TLS 分支未实现。
//! - 描述文件权限只能保证「同用户可读」以外的人不可读；Windows 依赖用户目录 ACL。
//! - 日志只允许出现地址、pid、实例 ID；令牌与会话密钥永不落日志。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::host_api::{
    self, ErrorResponse, HandshakeRequest, HandshakeResponse, HealthResponse, ShutdownResponse,
};
use crate::http_router::{
    Auth, Contract, ContractBody, ContractResponse, EndpointRequest, Method, Outcome, QueryParam,
    Reply, Request, Route, RouteError, token_eq,
};

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

/// 宿主主体：本机账户名（ADR-0007 第 6 条的落地）。
///
/// **只在装配时读**，请求体与请求头不参与判定。这一条就是「请求不得自报主体」的
/// 落点：只要主体来自进程环境，HTTP 客户端既改不了审计归属，也借不到别人的目标可见性。
///
/// 四个刻意的取舍：
/// - **不做名字改写**：不拼 `user-` 前缀、不折叠大小写。库里存的是主体 id，
///   改写会引入一张只有本仓库知道的映射表；映射不存在时失败方式是 404，
///   排查时看不出是「映射问题」还是「确实没权限」。
/// - **不是凭据**：它是归属来源，不是认证。认证只有会话密钥。能改这个环境变量的
///   人本来就能直接改库，所以这里不做防篡改——信任边界是 OS 账户，不是环境变量。
/// - **取不到就是没有**：空串与全空白都算没有，`serve` 因此不装配产品接口、一律 401，
///   而不是回退成 `local-user` 之类的占位主体。占位主体会让每次读取都去查一个
///   不存在的成员，把「装配缺失」写成「没有权限」。
/// - **同一台机器多个成员共用时归属失真**：这是已知限制（支持矩阵要写明），
///   不能靠加一个自报开关来「解决」——那等于把归属交给调用方。
pub fn local_principal_id() -> Option<String> {
    let raw = std::env::var("USERNAME")
        .ok()
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
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
    /// Host/Origin 白名单（CR-007）。每个请求进入 handler 前都要过这一关。
    policy: crate::host_guard::HostPolicy,
    /// 是否已收到授权停止（CR-003）。由 `/_bod/shutdown` 的 handler 置位，
    /// `run` 每轮检查。用原子而非 `handle` 返回值，是因为停服属于传输层的
    /// 循环控制，不该由路由层的 `Reply` 表达。
    stopping: AtomicBool,
    /// 产品接口（`/api/v1/*`）的只读端口 + 请求主体（P8 3.6 B3/B5）。
    ///
    /// **两者合成一个 `Option`**：有数据源却没有主体、或有主体却没有数据源，
    /// 都是「装配没完成」。拆成两个字段会让「半配置」在类型上合法，
    /// 于是要在一堆 handler 里各判一次。
    ///
    /// `None` 时产品接口一律 401。这不是占位，是刻意的安全默认：装配缺失时
    /// 不回退成「匿名用户」——匿名主体会让权限判定与审计归属同时失真，
    /// 而失真后的记录比没有记录更难查。
    delivery: Option<(
        std::sync::Arc<dyn crate::delivery_api::TrackingRead>,
        crate::delivery_api::DeliveryActor,
    )>,
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
        // 白名单必须在 address 被 move 进描述文件之前算出来：它按实际监听端口构造，
        // 不是拿配置里的 `listen` 字符串猜。
        let policy = crate::host_guard::HostPolicy::for_local_control_plane(&address)
            .map_err(HostError::Io)?;
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
            policy,
            stopping: AtomicBool::new(false),
            delivery: None,
        };
        write_descriptor(descriptor_path, &server.descriptor)?;
        Ok(server)
    }

    /// 追加允许的 Origin（本地开发服务器用）。默认只放行 Tauri 的两种来源。
    ///
    /// 必须在 `run` 之前调用；发布产物不应调用它。
    pub fn allow_origin(&mut self, origin: &str) {
        self.policy = self.policy.clone().with_origin(origin);
    }

    /// 装配产品接口（`/api/v1/*`）的只读端口与请求主体（P8 3.6 B3/B5）。
    ///
    /// **必须在 `run` 之前调用**。不调用时产品接口一律 401 `BOD_API_DENIED`，
    /// 理由见字段 `delivery` 的说明——装配缺失不等于「匿名用户可以访问」。
    ///
    /// 主体（`DeliveryActor`）由宿主决定从哪来：本机单用户配置、会话 → 成员映射、
    /// 或请求头自报，是三种不同的产品选择，接口层不预设。这里只要求传进来的
    /// 是一个确定的主体，不是一个可缺省的东西。
    pub fn with_delivery(
        mut self,
        tracking: std::sync::Arc<dyn crate::delivery_api::TrackingRead>,
        actor: crate::delivery_api::DeliveryActor,
    ) -> Self {
        self.delivery = Some((tracking, actor));
        self
    }

    /// 产品接口的只读端口。未装配时为 `None`。
    pub fn tracking_read(&self) -> Option<&dyn crate::delivery_api::TrackingRead> {
        self.delivery.as_ref().map(|(read, _)| read.as_ref())
    }

    /// 产品接口使用的请求主体。未装配时为 `None`。
    pub fn delivery_actor(&self) -> Option<&crate::delivery_api::DeliveryActor> {
        self.delivery.as_ref().map(|(_, actor)| actor)
    }

    /// 当前 Host/Origin 白名单，供测试与诊断读取。
    pub fn policy(&self) -> &crate::host_guard::HostPolicy {
        &self.policy
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
            if let Err(err) = self.handle(stream) {
                // 单连接出错不影响宿主；错误里不含凭据。
                tracing::warn!(
                    target: "bod.control_plane",
                    error = %err,
                    "控制面连接失败"
                );
            }
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
        }
        let _ = std::fs::remove_file(&self.descriptor_path);
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream) -> Result<bool, HostError> {
        let started = Instant::now();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let request = read_request(&mut stream)?;
        // 关联 ID 在读请求之后生成：畸形请求没有可归类的路径，只记连接级错误。
        let correlation_id = next_correlation_id(&self.descriptor.instance_id);
        let method = request.method.clone();
        let path = request.path_only().to_string();

        // CR-007：Host/Origin 在路由与鉴权之前先过。拒绝发生在这一层，
        // 后面的 handler 就不会被攻击者可控的请求碰到。
        let hosts = request.header_values("host");
        let origins = request.header_values("origin");
        if let Err(rejection) = self.policy.check(&hosts, &origins) {
            log_request(
                &correlation_id,
                &method,
                &path,
                403,
                RequestOutcome::RejectedHost,
                started.elapsed(),
            );
            respond_dto(
                &mut stream,
                403,
                &ErrorResponse::with_reason(rejection.code(), rejection.explain()),
            )?;
            return Ok(!self.stopping.load(Ordering::SeqCst));
        }

        // CR-003：匹配与鉴权交给 http_router，本函数只剩读请求与写响应。
        //
        // 两张路由表按前缀分流，而不是「先查控制面再查产品接口」：后者要把
        // `Request` 克隆一份才能重试，且会让「哪些路径归产品接口」变成隐式结果。
        // 前缀与路由表的一致性由 `openapi_contract` 的测试守着。
        let routes = if path.starts_with(crate::delivery_api::PREFIX) {
            crate::delivery_api::routes()
        } else {
            ROUTES
        };
        match crate::http_router::dispatch(routes, self, &self.session_key, request) {
            Outcome::Reply(Reply::Json { status, value }) => {
                log_request(
                    &correlation_id,
                    &method,
                    &path,
                    status,
                    outcome_for_json_status(status),
                    started.elapsed(),
                );
                respond(&mut stream, status, &value.to_string())?;
            }
            Outcome::Reply(Reply::Upgrade { handle }) => {
                log_request(
                    &correlation_id,
                    &method,
                    &path,
                    101,
                    RequestOutcome::Upgraded,
                    started.elapsed(),
                );
                // 订阅是长连接：放到独立线程，否则会把健康检查与停止请求堵在队列里。
                std::thread::spawn(move || handle(stream));
            }
            Outcome::Failed(err) => {
                log_request(
                    &correlation_id,
                    &method,
                    &path,
                    err.status,
                    RequestOutcome::Failed,
                    started.elapsed(),
                );
                respond_dto(&mut stream, err.status, &ErrorResponse::code(err.code))?;
            }
            Outcome::Unauthorized(code) => {
                log_request(
                    &correlation_id,
                    &method,
                    &path,
                    401,
                    RequestOutcome::DeniedAuth,
                    started.elapsed(),
                );
                respond_dto(&mut stream, 401, &ErrorResponse::code(code))?;
            }
            // 方法不对与路径不存在对外同答 404：既有客户端按 404 处理，
            // 本次不引入 405，以免改变可观测行为（理由见 spec-note）。
            Outcome::MethodNotAllowed | Outcome::NotFound => {
                log_request(
                    &correlation_id,
                    &method,
                    &path,
                    404,
                    RequestOutcome::NotFound,
                    started.elapsed(),
                );
                respond_dto(&mut stream, 404, &ErrorResponse::code("BOD_NOT_FOUND"))?;
            }
        }
        Ok(!self.stopping.load(Ordering::SeqCst))
    }
}

/// 请求结局。与 HTTP 状态码分开：同一个 403 可能来自 Host 校验或业务拒绝，
/// 观测时要分得清（§9「分别呈现阻塞原因」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    Allowed,
    Upgraded,
    DeniedAuth,
    RejectedHost,
    Failed,
    NotFound,
}

impl RequestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Upgraded => "upgraded",
            Self::DeniedAuth => "denied_auth",
            Self::RejectedHost => "rejected_host",
            Self::Failed => "failed",
            Self::NotFound => "not_found",
        }
    }
}

/// 200 段是成功，其余走 `Reply::Json` 的状态（3xx/1xx）既不是成功也不是失败，按 Failed 记。
/// 抽成纯函数是为了能直接测：日志写出去就看不见了，断言只能落在算出来的结局上。
fn outcome_for_json_status(status: u16) -> RequestOutcome {
    if (200..300).contains(&status) {
        RequestOutcome::Allowed
    } else {
        RequestOutcome::Failed
    }
}

/// 请求关联 ID：`req-<实例短号>-<进程内序号>`。
///
/// 用序号而不是随机串：同一进程内的顺序可读可排，排查「两条并发请求谁先到」时才有用；
/// 随机串在那种场景下等于没给。实例短号防止多实例日志汇总到一处时撞号。
fn next_correlation_id(instance_id: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let suffix = instance_id.strip_prefix("inst-").unwrap_or(instance_id);
    // `get` 而不是切片：实例 ID 由本模块生成、必然是 hex，
    // 但这里是日志路径，不该有 panic 的可能。
    let short = suffix.get(..8).unwrap_or(suffix);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    format!("req-{short}-{sequence}")
}

/// 记一条控制面请求日志。
///
/// **只允许** method、path、status、outcome、耗时与关联 ID。
/// 请求头、请求体、查询串都不进日志：日志是最容易泄露令牌的地方，
/// 而这里恰好有握手令牌与 Bearer 密钥经过。
fn log_request(
    correlation_id: &str,
    method: &str,
    path: &str,
    status: u16,
    outcome: RequestOutcome,
    elapsed: Duration,
) {
    let elapsed_ms = elapsed.as_millis() as u64;
    match outcome {
        // 拒绝是安全事件：留给 warn，默认级别下也能看到。
        RequestOutcome::DeniedAuth | RequestOutcome::RejectedHost => tracing::warn!(
            target: "bod.control_plane",
            correlation_id,
            method,
            path,
            status,
            outcome = outcome.as_str(),
            elapsed_ms,
            "控制面请求被拒绝"
        ),
        _ => tracing::info!(
            target: "bod.control_plane",
            correlation_id,
            method,
            path,
            status,
            outcome = outcome.as_str(),
            elapsed_ms,
            "控制面请求"
        ),
    }
}

/// 控制面路由表。**加端点只改这里**，不需要动 `handle`。
///
/// 顺序不敏感：`http_router::match_route` 先扫静态模式再扫参数模式，
/// 所以将来 `/api/v1/feedback-receipts/summary` 不会被 `{id}` 吃掉。
///
/// 每条都必须声明 `auth`、`note` 与 `contract`：声明缺失编译不过，理由为空则
/// 测试不过。`contract` 与路由同表是刻意的——「实现」与「文档」分开维护时，
/// 第十个端点之后必然漂移，而 §6/§11 合计还有 10 个端点要落地。
static ROUTES: &[Route<HostServer>] = &[
    Route {
        method: Method::Get,
        pattern: "/_bod/health",
        auth: Auth::Public,
        note: "存活探测只回协议名、实例 ID 与 pid，不含令牌与业务事实",
        contract: Contract {
            status: crate::http_router::Implementation::Served,
            summary: "存活探测",
            request: None,
            query: &[],
            responses: &[ContractResponse::new(
                200,
                "存活事实：协议名、实例 ID 与 pid",
                Some(ContractBody::json(
                    "HealthResponse",
                    crate::openapi::schema_of::<HealthResponse>,
                )),
            )],
        },
        handler: route_health,
    },
    Route {
        method: Method::Get,
        pattern: "/_bod/events",
        auth: Auth::InHandler("BOD_STREAM_DENIED"),
        note: "令牌在 Authorization 头；额外拒绝查询串令牌与非法游标，需按原因分别说明",
        contract: Contract {
            status: crate::http_router::Implementation::Served,
            summary: "订阅事件流（SSE）",
            request: None,
            query: &[
                QueryParam {
                    name: "cursor",
                    description: "起始游标；同时带 Last-Event-ID 头时以头为准",
                    required: false,
                },
                QueryParam {
                    name: "follow",
                    description: "`0` 表示只回放历史不跟随；缺省跟随",
                    required: false,
                },
                QueryParam {
                    name: "max_ms",
                    description: "单次连接最长毫秒数，缺省 30000，夹在 100..=60000",
                    required: false,
                },
                QueryParam {
                    name: "heartbeat_ms",
                    description: "心跳间隔毫秒，缺省 1000，夹在 20..=10000",
                    required: false,
                },
            ],
            responses: &[ContractResponse::new(
                200,
                "SSE 帧流；鉴权失败时先回 401 再断开，不会以流内事件表达错误",
                Some(ContractBody::EventStream),
            )],
        },
        handler: route_events,
    },
    Route {
        method: Method::Post,
        pattern: "/_bod/handshake",
        auth: Auth::InHandler("BOD_HANDSHAKE_DENIED"),
        note: "客户端此刻还没有会话密钥，令牌在请求体 handshake_token 字段",
        contract: Contract {
            status: crate::http_router::Implementation::Served,
            summary: "用一次性握手令牌换取会话密钥",
            request: Some(ContractBody::json(
                "HandshakeRequest",
                crate::openapi::schema_of::<HandshakeRequest>,
            )),
            query: &[],
            responses: &[ContractResponse::new(
                200,
                "会话密钥与控制面身份；密钥只出现在这个响应里",
                Some(ContractBody::json(
                    "HandshakeResponse",
                    crate::openapi::schema_of::<HandshakeResponse>,
                )),
            )],
        },
        handler: route_handshake,
    },
    Route {
        method: Method::Post,
        pattern: "/_bod/shutdown",
        auth: Auth::Bearer("BOD_SHUTDOWN_DENIED"),
        note: "只认 Authorization: Bearer 会话密钥，分发前统一校验",
        contract: Contract {
            status: crate::http_router::Implementation::Served,
            summary: "授权停止宿主",
            request: None,
            query: &[],
            responses: &[ContractResponse::new(
                200,
                "已置位停止意图；进程退出需另看描述文件是否消失",
                Some(ContractBody::json(
                    "ShutdownResponse",
                    crate::openapi::schema_of::<ShutdownResponse>,
                )),
            )],
        },
        handler: route_shutdown,
    },
];

/// 路由表的只读视图，给契约测试用（**只含控制面**）。
///
/// 不暴露可变入口：路由表是 `static`，`&'static []` 本身不可变，
/// 这个函数只是把「有哪些路由」变成可断言的公开事实。
pub fn contract_routes() -> &'static [Route<HostServer>] {
    ROUTES
}

/// 生成文档时用到的**全部**路由：控制面 + 产品接口（§11 冻结的 6 个操作）。
///
/// 与 `contract_routes()` 分开，是因为两者的使用场景不同：
/// 结构性断言要覆盖整份文档，而「控制面只有这 4 条」这类断言要看单张表。
pub fn documented_routes() -> Vec<&'static Route<HostServer>> {
    ROUTES
        .iter()
        .chain(crate::delivery_api::routes().iter())
        .collect()
}

/// 生成控制面 OpenAPI 3.1 文档。
///
/// 返回 `Err` 表示路由表自身不自洽（重复路由、悬空组件、手写 401），
/// 属于编程错误——契约测试会先在 CI 里失败，不会等到客户端拿到坏文档。
pub fn openapi_document() -> Result<serde_json::Value, String> {
    crate::openapi::document_many(
        ROUTES.iter().chain(crate::delivery_api::routes().iter()),
        crate::openapi::Info {
            title: "bod 控制面与产品接口",
            version: PROTOCOL,
            description: "本机控制面（/_bod/*）与产品接口（/api/v1/*）的同一份契约。\
                          文档由路由表与 DTO 生成，不手写。\
                          `x-bod-status=planned` 表示路径已冻结、实现未落地，请求会得到 501。",
        },
    )
}

fn route_health(state: &HostServer, _endpoint: EndpointRequest) -> Result<Reply, RouteError> {
    Ok(Reply::ok(host_api::encode(&HealthResponse {
        ok: true,
        protocol: PROTOCOL,
        instance_id: state.descriptor.instance_id.clone(),
        pid: state.descriptor.pid,
    })?))
}

fn route_events(state: &HostServer, endpoint: EndpointRequest) -> Result<Reply, RouteError> {
    let feed = std::sync::Arc::clone(&state.feed);
    let session_key = state.session_key.clone();
    let request = endpoint.request;
    Ok(Reply::upgrade(Box::new(move |stream| {
        if let Err(err) = stream_events(feed, &session_key, stream, &request) {
            // 客户端断开是正常结束，但其它错误要留痕；错误里不含凭据。
            eprintln!("订阅结束：{err}");
        }
    })))
}

fn route_handshake(state: &HostServer, endpoint: EndpointRequest) -> Result<Reply, RouteError> {
    // 缺字段、类型不对、JSON 畸形都落到同一个 401：不区分原因，
    // 免得把「令牌错」与「请求畸形」的差别变成探测手段。
    let token = serde_json::from_str::<HandshakeRequest>(&endpoint.request.body)
        .ok()
        .map(|request| request.handshake_token);
    match token {
        Some(token) if token_eq(&token, &state.descriptor.handshake_token) => {
            Ok(Reply::ok(host_api::encode(&HandshakeResponse {
                protocol: PROTOCOL,
                instance_id: state.descriptor.instance_id.clone(),
                pid: state.descriptor.pid,
                session_key: state.session_key.clone(),
            })?))
        }
        _ => Err(RouteError::new(401, "BOD_HANDSHAKE_DENIED")),
    }
}

fn route_shutdown(state: &HostServer, _endpoint: EndpointRequest) -> Result<Reply, RouteError> {
    state.stopping.store(true, Ordering::SeqCst);
    Ok(Reply::ok(host_api::encode(&ShutdownResponse {
        stopping: true,
    })?))
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
        respond_dto(
            &mut stream,
            401,
            &ErrorResponse::with_reason("BOD_STREAM_DENIED", auth.explain()),
        )?;
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

/// 用 DTO 写响应。
///
/// 序列化失败按 500 处理：DTO 写错是服务端缺陷，不能冒充请求错误；
/// 且这条路径上不能 panic——`run` 只捕获 `Err`，panic 会带走整个宿主。
fn respond_dto<T: serde::Serialize>(
    stream: &mut TcpStream,
    status: u16,
    body: &T,
) -> Result<(), HostError> {
    match serde_json::to_string(body) {
        Ok(text) => respond(stream, status, &text),
        Err(_) => respond(stream, 500, r#"{"error":"BOD_ENCODE_FAILED"}"#),
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> Result<(), HostError> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        501 => "Not Implemented",
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

    #[test]
    fn correlation_ids_are_unique_and_carry_instance_prefix() {
        let first = next_correlation_id("inst-0123456789abcdef");
        let second = next_correlation_id("inst-0123456789abcdef");
        assert!(first.starts_with("req-01234567-"), "{first}");
        assert_ne!(first, second, "同进程内序号必须递增，不能重复");
        let a: u64 = first.rsplit('-').next().unwrap().parse().unwrap();
        let b: u64 = second.rsplit('-').next().unwrap().parse().unwrap();
        assert_eq!(b, a + 1, "序号应严格 +1");
    }

    #[test]
    fn correlation_id_tolerates_non_ascii_instance_id() {
        // 正常路径的实例 ID 是 hex，但这里不该有 panic 的可能：它在日志路径上。
        let id = next_correlation_id("实例-未预期");
        assert!(id.starts_with("req-"), "{id}");
    }

    #[test]
    fn json_status_maps_to_two_outcomes_only() {
        assert_eq!(outcome_for_json_status(200), RequestOutcome::Allowed);
        assert_eq!(outcome_for_json_status(204), RequestOutcome::Allowed);
        assert_eq!(outcome_for_json_status(299), RequestOutcome::Allowed);
        assert_eq!(outcome_for_json_status(199), RequestOutcome::Failed);
        assert_eq!(outcome_for_json_status(300), RequestOutcome::Failed);
        assert_eq!(outcome_for_json_status(500), RequestOutcome::Failed);
    }

    #[test]
    fn request_outcome_labels_are_stable() {
        // 这些字符串进日志与指标，改了等于改观测口径。
        for (outcome, label) in [
            (RequestOutcome::Allowed, "allowed"),
            (RequestOutcome::Upgraded, "upgraded"),
            (RequestOutcome::DeniedAuth, "denied_auth"),
            (RequestOutcome::RejectedHost, "rejected_host"),
            (RequestOutcome::Failed, "failed"),
            (RequestOutcome::NotFound, "not_found"),
        ] {
            assert_eq!(outcome.as_str(), label);
        }
    }

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

    /// §11 冻结的路径必须真的可路由：先过会话关，再如实回 501。
    ///
    /// 这条测的是**分发**，不是契约：如果前缀分流写错，请求会落到控制面表上，
    /// 结果是 404——客户端会把「契约冻结了」理解成「路径拼错了」。
    #[test]
    fn frozen_product_paths_are_routed_and_answer_501() {
        let (_path, address, attached) = start_host("product-stub");
        let body = r#"{"project_id":"p1"}"#;

        let (denied, _) = post_json(
            &address,
            "/api/v1/delivery-tracking/imports",
            body,
            None,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(denied, 401, "没有会话密钥时必须拒绝，而不是泄露 501");

        let (status, response) = post_json(
            &address,
            "/api/v1/delivery-tracking/imports",
            body,
            Some(&attached.session_key),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status, 501, "{response}");
        assert!(
            response.contains("BOD_NOT_IMPLEMENTED"),
            "计划中操作必须明确说自己未实现：{response}"
        );

        // 静态段优先于参数段：`imports` 不能被 `/delivery-tracking/{id}` 吃掉。
        // 用 GET 验证方法不符（GET 只在 {id} 上注册），对外仍答 404。
        let (wrong_method, _) = roundtrip(
            &address,
            Duration::from_secs(2),
            &format!(
                "GET /api/v1/delivery-tracking/imports HTTP/1.1\r\nHost: {address}\r\n\
                 Authorization: Bearer {}\r\nConnection: close\r\n\r\n",
                attached.session_key
            ),
        )
        .unwrap();
        assert_eq!(wrong_method, 404);

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
