//! 可回放的第二个真实执行端（P2 4.4 的前置能力）。
//!
//! `adapter.local-process` 把 stdout/stderr 收进内存 `Vec`，宿主一重启序列与内容都没了，
//! 所以它如实声明 `recovery` 与 `reliable_observation` 为 `unsupported`——于是 L2/L3 样本
//! 在它身上**根本产不出来**，不是缺人工分钟，是缺能声明这两个保证的执行端。
//!
//! 本模块提供第二种真实运行配置：子进程的 stdout/stderr 直接落进会话目录的追加文件
//! `events.log`，**序号由行号决定**（第 n 行就是 seq n）。因此：
//!
//! - 重放是平凡的：同一份文件读出同样的序号，不需要游标也不怕游标损坏；
//! - 重启不丢事件：新适配器从第 0 字节重读，宿主 inbox 按 `(epoch, stream, seq)` 去重，
//!   重复是 `duplicate` 而不是新事实；
//! - 没有"游标先写后交"的丢事件窗口：本模块**不持久化读游标**，只在本进程内存里记偏移，
//!   代价是重启后重放一遍（有界、幂等），收益是任何时刻崩溃都不会漏事件。
//!
//! 会话恢复（`recovery`）靠会话目录 + 进程号存活判断：宿主重启后 `inspect` 仍能回答
//! Running（存活）或 Unknown（已死但没有退出记录——如实报未知，不假装 Completed）。
//! 与 `local-process` 的关键取舍：本适配器 `Drop` **不杀**子进程，否则"重启后接管"
//! 无从谈起；代价是宿主崩溃会留下孤儿进程，必须由 `cancel` 或上层清理收尾。

use crate::domain::execution::{
    AdapterDescriptor, CancelStage, CapabilityClaim, CapabilitySlot, EffectClass, EvidenceLevel,
    EvidenceSource, ExecutionState, ObservationEnvelope, ObservationType, ObserveTransport,
    Operation, PROTOCOL_ID, ProbeAspect, ProbeItem, ProbeReport, ProbeStatus, StartRequest,
    SupportLevel, map_process_exit,
};
use crate::invocation::{StartOutcome, StartTransport};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub const ADAPTER_ID: &str = "adapter.durable-process";
pub const ADAPTER_VERSION: &str = "1.0.0";

/// 会话目录里的固定文件名。
const SESSION_FILE: &str = "session.meta";
const EVENTS_FILE: &str = "events.log";
const EXIT_FILE: &str = "exit.meta";
const CANCELLED_FILE: &str = "cancelled.meta";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableProcessConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub working_dir: Option<PathBuf>,
    /// 会话根目录：会话目录、事件文件与退出记录都在这里，必须跨宿主重启保留。
    pub root: PathBuf,
}

impl DurableProcessConfig {
    pub fn new(executable: impl Into<String>, args: Vec<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args,
            working_dir: None,
            root: root.into(),
        }
    }

    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// 探测本机运行时（python3 → python → node），参数由调用方给定。
    pub fn detect(root: impl Into<PathBuf>) -> Option<Self> {
        let root = root.into();
        for candidate in ["python3", "python", "node"] {
            let ok = Command::new(candidate)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if ok {
                return Some(Self::new(candidate, Vec::new(), root));
            }
        }
        None
    }
}

/// 会话的持久事实。用 `key=value` 而不是 JSON：这里的字段只有三个，
/// 手写解析比引入反序列化错误面更小；缺字段即视为不可信，不补默认值。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionMeta {
    /// `-1` 表示"已决定启动但还没拿到进程号"：此时不能重放，也不能再启动一次。
    pid: i64,
    started_at_ms: i64,
    execution_id: String,
}

impl SessionMeta {
    fn render(&self) -> String {
        format!(
            "# bod durable session v1\npid={}\nstarted_at_ms={}\nexecution_id={}\n",
            self.pid, self.started_at_ms, self.execution_id
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut pid = None;
        let mut started_at_ms = None;
        let mut execution_id = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=')?;
            match key.trim() {
                "pid" => pid = value.trim().parse::<i64>().ok(),
                "started_at_ms" => started_at_ms = value.trim().parse::<i64>().ok(),
                "execution_id" => execution_id = Some(value.trim().to_string()),
                _ => {}
            }
        }
        Some(Self {
            pid: pid?,
            started_at_ms: started_at_ms?,
            execution_id: execution_id?,
        })
    }
}

fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    // 先写临时文件再改名：半截的 session.meta 比没有更危险（会被读成"没启动过"）。
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    // Windows 上 rename 不能覆盖已有文件：先删除再改名，失败即报错。
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)
}

/// 会话目录名不能直接用 invocation_id（可能含路径分隔符），做一层可逆转义。
fn session_dir_name(invocation_id: &str) -> String {
    let mut out = String::with_capacity(invocation_id.len());
    for byte in invocation_id.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    if out.is_empty() {
        out.push_str("%00");
    }
    out
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(output) = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    // 注意 tasklist 的首列是映像名（python.exe），PID 在第二列；映像名带空格时会错位，
    // 故再补一条「首列是 .exe 且某字段等于 pid」的判据。避免把内存/会话号误认成 pid。
    let wanted = pid.to_string();
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let second_is_pid = fields.get(1).is_some_and(|field| *field == wanted.as_str());
        let exe_line = fields
            .first()
            .is_some_and(|field| field.to_ascii_lowercase().ends_with(".exe"));
        second_is_pid || (exe_line && fields.contains(&wanted.as_str()))
    })
}

/// `kill -0` 只证明「pid 还在进程表里」，**僵尸也满足它**——已退出但父进程没回收的进程
/// 照样能收信号，signal 探测会一直说它活着。因此 Linux 侧的存活判断要看状态位。
///
/// `/proc/<pid>/stat` 的 `comm`（第二个字段）可以含空格与括号，所以取**最后一个** `)` 之后
/// 的内容。返回 `None` 表示格式不认识，调用方应回落到 signal 探测，不要猜成活或死。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn proc_stat_reports_dead(stat: &str) -> Option<bool> {
    let rest = stat.rsplit_once(')')?.1;
    let state = rest.trim_start().chars().next()?;
    // Z = 僵尸；X/x = 已终止（新内核用它表示已死）；S/R/D/T 等一律算存活。
    Some(matches!(state, 'Z' | 'X' | 'x'))
}

/// signal 探测。只用于**没有** `/proc` 的 Unix，以及 `/proc` 读不出来的场合
/// （容器未挂载等）。这些场合下僵尸会被误判为存活——这是已知妥协，写在注释里而不是隐瞒。
#[cfg(unix)]
fn kill_zero(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match proc_stat_reports_dead(&stat) {
            Some(dead) => !dead,
            None => kill_zero(pid),
        },
        // /proc 里没有这一条：进程已不存在。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        // 读不出来时不猜，回落到 signal 探测。
        Err(_) => kill_zero(pid),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn pid_alive(pid: u32) -> bool {
    kill_zero(pid)
}

#[cfg(not(any(windows, unix)))]
fn pid_alive(_pid: u32) -> bool {
    // 无法判断存活时必须返回 false，让上层落到 Unknown 而不是假装 Running。
    false
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> bool {
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> bool {
    Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(any(windows, unix)))]
fn kill_pid(_pid: u32) -> bool {
    false
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 可回放的本地进程适配器。同一 `root` 上的新实例即为「宿主重启后的适配器」：
/// 不依赖任何进程内状态就能重新观察同一会话。
pub struct DurableProcessAdapter {
    config: DurableProcessConfig,
    descriptor: AdapterDescriptor,
    /// 本进程内仍有句柄的子进程：有句柄才能拿到真实退出码。
    children: BTreeMap<String, Child>,
    /// 本进程已读到的字节偏移与行数；**不持久化**，重启后从 0 重放。
    offsets: BTreeMap<String, u64>,
    lines: BTreeMap<String, u64>,
    cancelled: BTreeMap<String, bool>,
}

impl DurableProcessAdapter {
    pub fn new(config: DurableProcessConfig, now_ms: i64) -> Self {
        let descriptor = build_descriptor(&config, now_ms);
        Self {
            config,
            descriptor,
            children: BTreeMap::new(),
            offsets: BTreeMap::new(),
            lines: BTreeMap::new(),
            cancelled: BTreeMap::new(),
        }
    }

    /// 自动探测运行时并构造：没有可用运行时返回 None，不假装就绪。
    pub fn detect(root: impl Into<PathBuf>, now_ms: i64) -> Option<Self> {
        DurableProcessConfig::detect(root).map(|config| Self::new(config, now_ms))
    }

    pub fn config(&self) -> &DurableProcessConfig {
        &self.config
    }

    pub fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    pub fn set_args(&mut self, args: Vec<String>, now_ms: i64) {
        self.config.args = args;
        self.descriptor = build_descriptor(&self.config, now_ms);
    }

    /// 换成 conformance 用的含凭据负载。与 `local_process` 共用同一份脚本定义，
    /// 避免两个真实配置在「什么算敏感样本」上各说一套。
    pub fn use_conformance_payload(&mut self, now_ms: i64) {
        let hint = self.config.executable.clone();
        self.set_args(
            crate::local_process::LocalProcessConfig::conformance_script(&hint),
            now_ms,
        );
    }

    fn session_dir(&self, invocation_id: &str) -> PathBuf {
        self.config.root.join(session_dir_name(invocation_id))
    }

    fn read_session(&self, invocation_id: &str) -> Option<SessionMeta> {
        let path = self.session_dir(invocation_id).join(SESSION_FILE);
        let text = fs::read_to_string(path).ok()?;
        SessionMeta::parse(&text)
    }

    fn read_exit(&self, invocation_id: &str) -> Option<i32> {
        let path = self.session_dir(invocation_id).join(EXIT_FILE);
        let text = fs::read_to_string(path).ok()?;
        parse_key_value(&text, "code")?.parse::<i32>().ok()
    }

    fn write_exit(&self, invocation_id: &str, code: i32) -> std::io::Result<()> {
        let body = format!("# bod durable exit v1\ncode={code}\nat_ms={}\n", now_ms());
        write_atomic(&self.session_dir(invocation_id).join(EXIT_FILE), &body)
    }

    fn is_cancelled(&self, invocation_id: &str) -> bool {
        self.cancelled.get(invocation_id).copied().unwrap_or(false)
            || self
                .session_dir(invocation_id)
                .join(CANCELLED_FILE)
                .exists()
    }

    /// 真实探测：环境项真的跑 `--version`；会话根目录必须可写，否则恢复语义不成立。
    pub fn probe(&self, now_ms: i64) -> ProbeReport {
        let item = |aspect, status, reason: String, scope: &str| ProbeItem {
            aspect,
            status,
            reason,
            source: EvidenceSource::PlatformProbe,
            scope: scope.to_string(),
            observed_at_ms: now_ms,
            expires_at_ms: Some(now_ms + 300_000),
        };
        let version = Command::new(&self.config.executable)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        let (env_status, env_reason) = match version {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                let line = text.lines().next().unwrap_or("").trim().to_string();
                (
                    ProbeStatus::Ready,
                    format!("{} 可用：{line}", self.config.executable),
                )
            }
            Ok(output) => (
                ProbeStatus::NotReady,
                format!("--version 退出码 {:?}", output.status.code()),
            ),
            Err(err) => (
                ProbeStatus::NotReady,
                format!("{} 不可执行：{err}", self.config.executable),
            ),
        };
        let writable = fs::create_dir_all(&self.config.root)
            .and_then(|_| {
                let probe = self.config.root.join(".probe");
                let written = fs::write(&probe, b"ok");
                let _ = fs::remove_file(&probe);
                written
            })
            .is_ok();
        ProbeReport {
            adapter_id: ADAPTER_ID.to_string(),
            adapter_version: ADAPTER_VERSION.to_string(),
            items: vec![
                item(
                    ProbeAspect::Environment,
                    env_status,
                    env_reason,
                    "durable/executable",
                ),
                item(
                    ProbeAspect::Workspace,
                    if writable {
                        ProbeStatus::Ready
                    } else {
                        ProbeStatus::NotReady
                    },
                    format!(
                        "会话根目录 {} 可写={writable}；不可写就无法在重启后恢复观察",
                        self.config.root.display()
                    ),
                    "durable/sessions",
                ),
                item(
                    ProbeAspect::Credentials,
                    ProbeStatus::Unsupported,
                    "本机进程不读取模型凭据".to_string(),
                    "durable/env",
                ),
                item(
                    ProbeAspect::Model,
                    ProbeStatus::Unsupported,
                    "不调用模型推理".to_string(),
                    "durable/model",
                ),
                item(
                    ProbeAspect::Quota,
                    ProbeStatus::Unsupported,
                    "本机进程不提供配额窗口".to_string(),
                    "durable/quota",
                ),
            ],
            generated_at_ms: now_ms,
        }
    }

    fn spawn(&mut self, request: &StartRequest) -> Result<(String, bool), String> {
        let dir = self.session_dir(&request.invocation_id);
        fs::create_dir_all(&dir).map_err(|err| format!("建会话目录失败：{err}"))?;
        let session_path = dir.join(SESSION_FILE);
        if session_path.exists() {
            // 已有会话：同 ID 不重复启动。拿不到进程号说明上次启动中断，如实报无法确认。
            let meta = self.read_session(&request.invocation_id);
            return match meta {
                Some(meta) if meta.pid > 0 => Ok((meta.execution_id, true)),
                Some(_) => Err("上次启动在写入进程号前中断，不能确认是否已启动".to_string()),
                None => Err("会话目录存在但记录损坏，不能确认是否已启动".to_string()),
            };
        }

        // 先落「已决定启动」标记，再 spawn：两者之间崩溃时宁可选"不确定"，也不重复启动。
        let execution_id = format!("durable:{}", request.invocation_id);
        write_atomic(
            &session_path,
            &SessionMeta {
                pid: -1,
                started_at_ms: now_ms(),
                execution_id: execution_id.clone(),
            }
            .render(),
        )
        .map_err(|err| format!("写会话记录失败：{err}"))?;

        let events_path = dir.join(EVENTS_FILE);
        let events = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .map_err(|err| format!("打开事件文件失败：{err}"))?;
        let stdout = events
            .try_clone()
            .map_err(|err| format!("克隆事件句柄失败：{err}"))?;
        let stderr = events
            .try_clone()
            .map_err(|err| format!("克隆事件句柄失败：{err}"))?;

        let mut command = Command::new(&self.config.executable);
        if !self.config.args.is_empty() {
            command.args(&self.config.args);
        }
        if let Some(dir) = &self.config.working_dir {
            command.current_dir(dir);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|err| format!("启动失败：{err}"))?;

        let pid = child.id();
        write_atomic(
            &session_path,
            &SessionMeta {
                pid: i64::from(pid),
                started_at_ms: now_ms(),
                execution_id: execution_id.clone(),
            }
            .render(),
        )
        .map_err(|err| format!("写进程号失败：{err}"))?;
        self.children.insert(request.invocation_id.clone(), child);
        self.offsets.insert(request.invocation_id.clone(), 0);
        self.lines.insert(request.invocation_id.clone(), 0);
        Ok((execution_id, false))
    }

    /// 读退出记录或句柄状态；句柄可用时顺便落 `exit.meta`（重启后仍可判定终态）。
    fn refresh(&mut self, invocation_id: &str) -> Option<ExecutionState> {
        if let Some(code) = self.read_exit(invocation_id) {
            return Some(if self.is_cancelled(invocation_id) {
                ExecutionState::Cancelled
            } else {
                map_process_exit(code).state
            });
        }
        if let Some(child) = self.children.get_mut(invocation_id) {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    let _ = self.write_exit(invocation_id, code);
                    return Some(if self.is_cancelled(invocation_id) {
                        ExecutionState::Cancelled
                    } else {
                        map_process_exit(code).state
                    });
                }
                Ok(None) => return Some(ExecutionState::Running),
                Err(_) => return Some(ExecutionState::Unknown),
            }
        }
        let meta = self.read_session(invocation_id)?;
        if meta.pid > 0 && pid_alive(meta.pid as u32) {
            return Some(ExecutionState::Running);
        }
        // 宿主不在时子进程死了：没有退出码，不说 Completed/Failed，如实报 Unknown。
        Some(ExecutionState::Unknown)
    }

    /// 从事件文件读出完整行；序号 = 行号（1 基）。
    fn read_new_lines(&mut self, invocation_id: &str) -> Vec<ObservationEnvelope> {
        let Some(meta) = self.read_session(invocation_id) else {
            return Vec::new();
        };
        let path = self.session_dir(invocation_id).join(EVENTS_FILE);
        let Ok(mut file) = File::open(&path) else {
            return Vec::new();
        };
        let offset = self.offsets.get(invocation_id).copied().unwrap_or(0);
        if offset > 0 && file.seek(SeekFrom::Start(offset)).is_err() {
            return Vec::new();
        }
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_err() {
            return Vec::new();
        }
        // 只消费到最后一个换行：半截行不算一条事件，留到下次。
        let Some(last_newline) = buffer.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        let complete = &buffer[..=last_newline];
        let mut lines = self.lines.get(invocation_id).copied().unwrap_or(0);
        let mut envelopes = Vec::new();
        for raw in complete.split(|byte| *byte == b'\n') {
            if raw.is_empty() {
                continue;
            }
            lines += 1;
            envelopes.push(ObservationEnvelope {
                invocation_id: invocation_id.to_string(),
                source_epoch: 1,
                stream_id: "log".to_string(),
                source_sequence: lines,
                observed_at_ms: meta.started_at_ms,
                kind: ObservationType::Log,
                payload: String::from_utf8_lossy(raw)
                    .trim_end_matches('\r')
                    .to_string(),
                origin: ADAPTER_ID.to_string(),
            });
        }
        self.offsets
            .insert(invocation_id.to_string(), offset + last_newline as u64 + 1);
        self.lines.insert(invocation_id.to_string(), lines);
        envelopes
    }
}

fn parse_key_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim())
    })
}

impl StartTransport for DurableProcessAdapter {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn start(&mut self, request: &StartRequest) -> StartOutcome {
        match self.spawn(request) {
            Ok((execution_id, true)) => StartOutcome::Replayed { execution_id },
            Ok((execution_id, false)) => StartOutcome::Accepted { execution_id },
            Err(reason) => StartOutcome::Unavailable { reason },
        }
    }

    fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState> {
        self.refresh(invocation_id)
    }

    fn observations(&mut self, invocation_id: &str) -> Vec<ObservationEnvelope> {
        self.read_new_lines(invocation_id)
    }

    fn cancel(&mut self, invocation_id: &str) -> CancelStage {
        let _ = write_atomic(
            &self.session_dir(invocation_id).join(CANCELLED_FILE),
            "# bod durable cancel v1\n",
        );
        self.cancelled.insert(invocation_id.to_string(), true);
        if let Some(mut child) = self.children.remove(invocation_id) {
            let _ = child.kill();
            return match child.wait() {
                Ok(status) => {
                    // 被杀的子进程通常没有退出码：写 -1 占位，让重启后的 inspect 落 Cancelled。
                    let _ = self.write_exit(invocation_id, status.code().unwrap_or(-1));
                    CancelStage::Confirmed
                }
                Err(_) => CancelStage::Requested,
            };
        }
        let Some(meta) = self.read_session(invocation_id) else {
            return CancelStage::Requested;
        };
        if meta.pid <= 0 {
            return CancelStage::Requested;
        }
        let pid = meta.pid as u32;
        if !pid_alive(pid) {
            return CancelStage::Confirmed;
        }
        let killed = kill_pid(pid);
        // 杀完必须复核存活：只有确认不再存活才算停止，不能凭 kill 返回成功就宣布。
        let stopped = killed && !pid_alive(pid);
        if stopped {
            let _ = self.write_exit(invocation_id, -1);
            CancelStage::Confirmed
        } else {
            CancelStage::Requested
        }
    }

    fn probe(&self, now_ms: i64) -> Option<ProbeReport> {
        Some(DurableProcessAdapter::probe(self, now_ms))
    }

    fn fault_injection(&self) -> bool {
        // 真实执行端不做故障注入：需要注入的用例必须记 not_applicable，不能冒充通过。
        false
    }
}

fn build_descriptor(config: &DurableProcessConfig, now_ms: i64) -> AdapterDescriptor {
    let claim = |slot: CapabilitySlot,
                 level: SupportLevel,
                 mode: &str,
                 source: EvidenceSource,
                 evidence: EvidenceLevel,
                 reason: &str| CapabilityClaim {
        slot,
        level,
        mode: Some(mode.to_string()),
        source,
        evidence_level: evidence,
        observed_at_ms: now_ms,
        expires_at_ms: None,
        reason: Some(reason.to_string()),
    };
    let unsupported = |slot: CapabilitySlot, mode: &str, reason: &str| {
        claim(
            slot,
            SupportLevel::Unsupported,
            mode,
            EvidenceSource::AdapterSelfDeclared,
            EvidenceLevel::SelfDeclared,
            reason,
        )
    };

    AdapterDescriptor {
        protocol: PROTOCOL_ID.to_string(),
        adapter_id: ADAPTER_ID.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        capability_id: "bod.durable-process.v1".to_string(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        transport: "file+stdio".to_string(),
        effect_class: EffectClass::ProcessExecution,
        operations: vec![
            Operation::Describe,
            Operation::Probe,
            Operation::Start,
            Operation::Inspect,
            Operation::Observe,
            Operation::Cancel,
            Operation::Reconcile,
        ],
        // 观察是"跟随追加文件"的流式读，不是轮询子进程管道。
        observe: ObserveTransport::Stream,
        capabilities: vec![
            unsupported(
                CapabilitySlot::ModelControl,
                "opaque",
                "模型与提示词由被启动的程序自行决定",
            ),
            unsupported(
                CapabilitySlot::ToolControl,
                "opaque",
                "工具调用发生在子进程内部，宿主只观察事件文件",
            ),
            unsupported(
                CapabilitySlot::WorkspaceIsolation,
                "none",
                "只设置独立工作目录，不做文件系统隔离",
            ),
            unsupported(
                CapabilitySlot::Metering,
                "none",
                "本机进程不提供用量与计费口径",
            ),
            claim(
                CapabilitySlot::Cancellation,
                SupportLevel::Supported,
                "process_kill",
                EvidenceSource::TestResult,
                EvidenceLevel::RuntimeObserved,
                "有句柄直接 kill，无句柄按进程号杀并复核存活；不保证清理派生子进程",
            ),
            claim(
                CapabilitySlot::Recovery,
                SupportLevel::Supported,
                "session_dir",
                EvidenceSource::TestResult,
                EvidenceLevel::RuntimeObserved,
                "会话目录记录进程号与事件文件，宿主重启后可重新观察同一会话；宿主不在时子进程已死则报 Unknown 而非终态",
            ),
            unsupported(
                CapabilitySlot::ExternalIdempotency,
                "session_marker",
                "只保证同一 invocation 不重复启动，不保证业务副作用幂等；去重仍由宿主记录承担",
            ),
            claim(
                CapabilitySlot::ReliableObservation,
                SupportLevel::Supported,
                "append_only_events",
                EvidenceSource::TestResult,
                EvidenceLevel::RuntimeObserved,
                "stdout/stderr 追加进会话事件文件，序号=行号，重启从 0 重放由宿主 inbox 去重，不丢事件",
            ),
            unsupported(
                CapabilitySlot::QuotaWindows,
                "none",
                "没有服务方配额窗口可查询",
            ),
        ],
        // 不做业务去重保留期承诺：重复 start 只保证不重复 spawn，不保证副作用幂等。
        dedup_retention_ms: None,
        limitations: vec![
            "stdout 与 stderr 合并进同一事件文件，无法区分来源流".to_string(),
            "宿主崩溃不杀子进程（否则重启无法接管），孤儿需由 cancel 或上层清理收尾".to_string(),
            "重启后重放整份事件文件：重复无害（宿主按序号去重），但长日志会重复读取".to_string(),
            format!(
                "单次只运行配置好的入口命令（{} {}）",
                config.executable,
                config.args.join(" ")
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BodConfig;
    use crate::protocol_reliability::receive_observation;
    use crate::store::SqliteStore;
    use std::time::Duration;

    fn temp_root(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("bod-durable-{tag}-{}-{stamp}", std::process::id()))
    }

    fn request(invocation_id: &str) -> StartRequest {
        StartRequest {
            invocation_id: invocation_id.to_string(),
            run_id: "run-durable".to_string(),
            work_package_revision: "wp-durable-r1".to_string(),
            attempt_id: "attempt-durable".to_string(),
            binding_id: "binding-durable".to_string(),
            fencing_token: 3,
            agent_ref: "agent-durable".to_string(),
            runtime_config_revision: "rt-durable".to_string(),
            context_bundle_ref: "ctx-durable".to_string(),
            workspace_ref: "ws-durable".to_string(),
            grant_ref: "grant-durable".to_string(),
            reservation_refs: vec![],
            deadline_at_ms: None,
        }
    }

    fn detect(root: &Path) -> Option<DurableProcessAdapter> {
        let config = DurableProcessConfig::detect(root)?;
        Some(DurableProcessAdapter::new(config, now_ms()))
    }

    fn script(adapter: &DurableProcessAdapter, python: &str, node: &str) -> Vec<String> {
        if adapter.config().executable.contains("node") {
            vec!["-e".to_string(), node.to_string()]
        } else {
            vec!["-c".to_string(), python.to_string()]
        }
    }

    fn collect_until(
        adapter: &mut DurableProcessAdapter,
        invocation_id: &str,
        want: usize,
        timeout: Duration,
    ) -> Vec<ObservationEnvelope> {
        let step = Duration::from_millis(20);
        let mut waited = Duration::ZERO;
        let mut all = Vec::new();
        loop {
            all.extend(adapter.observations(invocation_id));
            if all.len() >= want || waited >= timeout {
                return all;
            }
            std::thread::sleep(step);
            waited += step;
        }
    }

    #[test]
    fn descriptor_declares_reliable_observation_and_recovery_with_test_evidence() {
        let root = temp_root("descriptor");
        let config = DurableProcessConfig::new("python", vec![], &root);
        let adapter = DurableProcessAdapter::new(config, 1);
        let descriptor = adapter.descriptor();
        for slot in [
            CapabilitySlot::ReliableObservation,
            CapabilitySlot::Recovery,
        ] {
            assert_eq!(descriptor.level_of(slot.clone()), SupportLevel::Supported);
            let claim = descriptor
                .capabilities
                .iter()
                .find(|claim| claim.slot == slot)
                .expect("能力声明必须存在");
            assert_eq!(claim.source, EvidenceSource::TestResult);
            assert_eq!(claim.evidence_level, EvidenceLevel::RuntimeObserved);
        }
        // 观察是流式跟随文件，不是轮询子进程管道。
        assert_eq!(descriptor.observe, ObserveTransport::Stream);
        // 不做业务幂等承诺：去重仍由宿主记录承担。
        assert_eq!(
            descriptor.level_of(CapabilitySlot::ExternalIdempotency),
            SupportLevel::Unsupported
        );
        assert_eq!(descriptor.dedup_retention_ms, None);
    }

    #[test]
    fn start_persists_session_and_publishes_lines_contiguously() {
        let root = temp_root("start");
        let Some(mut adapter) = detect(&root) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        adapter.set_args(
            script(
                &adapter,
                "import time\nfor i in range(3): print('line-%d' % i, flush=True)\ntime.sleep(0.4)",
                "for (let i=0;i<3;i++) console.log('line-'+i); setTimeout(()=>{},400);",
            ),
            now_ms(),
        );
        let start = adapter.start(&request("inv-start"));
        assert!(
            matches!(start, StartOutcome::Accepted { .. }),
            "首次启动应受理：{start:?}"
        );
        let events = collect_until(&mut adapter, "inv-start", 3, Duration::from_secs(5));
        assert_eq!(events.len(), 3, "应读到三行：{events:?}");
        let sequences: Vec<u64> = events.iter().map(|e| e.source_sequence).collect();
        assert_eq!(sequences, vec![1, 2, 3], "序号必须是连续行号");
        assert_eq!(events[0].payload, "line-0");
        assert_eq!(events[2].payload, "line-2");
        // 会话记录里必须有真实进程号，否则重启后无法判断存活。
        let meta = adapter.read_session("inv-start").expect("会话记录必须落地");
        assert!(meta.pid > 0, "进程号必须为正：{meta:?}");
        assert_eq!(meta.execution_id, "durable:inv-start");
        // 再读一次不应重复给出已消费的行。
        assert!(adapter.observations("inv-start").is_empty());
        let _ = adapter.cancel("inv-start");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_adapter_replays_same_sequences_after_host_restart() {
        let root = temp_root("restart");
        let Some(mut first) = detect(&root) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        first.set_args(
            script(
                &first,
                // 第二行之后留足存活时间：本测试要跨「宿主退出 → 新宿主接管」两个时刻，
                // 进程若在 inspect 之前退出，会被正确判成 Unknown，测试就测不到接管。
                "import time\nprint('a', flush=True)\ntime.sleep(0.6)\nprint('b', flush=True)\ntime.sleep(5)",
                "console.log('a'); setTimeout(()=>{console.log('b')},600); setTimeout(()=>{},5600);",
            ),
            now_ms(),
        );
        assert!(matches!(
            first.start(&request("inv-restart")),
            StartOutcome::Accepted { .. }
        ));
        let seen = collect_until(&mut first, "inv-restart", 1, Duration::from_secs(5));
        assert_eq!(seen.len(), 1, "先只读到第一行：{seen:?}");
        assert_eq!(seen[0].source_sequence, 1);
        let pid = first.read_session("inv-restart").expect("会话记录").pid;
        // 模拟宿主退出：Drop 不杀子进程，句柄随之丢失。
        drop(first);
        assert!(pid_alive(pid as u32), "宿主退出后子进程应仍在运行");

        let mut second = detect(&root).expect("第二个适配器可用");
        assert_eq!(
            second.inspect("inv-restart"),
            Some(ExecutionState::Running),
            "新适配器必须能对同一会话回答 Running"
        );
        let replayed = collect_until(&mut second, "inv-restart", 2, Duration::from_secs(5));
        let sequences: Vec<u64> = replayed.iter().map(|e| e.source_sequence).collect();
        assert_eq!(
            sequences,
            vec![1, 2],
            "重启后必须从第 1 行重放并续到第 2 行，不允许跳号"
        );
        assert_eq!(replayed[0].payload, "a");
        assert_eq!(replayed[1].payload, "b");
        let _ = second.cancel("inv-restart");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replayed_events_are_duplicates_in_the_host_inbox() {
        let root = temp_root("dedup");
        let dir = root.join(session_dir_name("inv-dedup"));
        std::fs::create_dir_all(&dir).expect("建目录");
        write_atomic(
            &dir.join(SESSION_FILE),
            &SessionMeta {
                pid: -1,
                started_at_ms: 1_000,
                execution_id: "durable:inv-dedup".to_string(),
            }
            .render(),
        )
        .expect("写会话");
        std::fs::write(dir.join(EVENTS_FILE), "一\n二\n").expect("写事件");

        let config = DurableProcessConfig::new("python", vec![], &root);
        let mut store = SqliteStore::in_memory(&BodConfig::default()).expect("内存库");
        let mut first = DurableProcessAdapter::new(config.clone(), 1);
        let once = first.observations("inv-dedup");
        assert_eq!(once.len(), 2);
        for envelope in &once {
            let receipt = receive_observation(&mut store, envelope, None, 10).expect("接收");
            assert!(receipt.persisted, "首次接收必须落库");
        }

        // 宿主重启：新适配器从 0 重放同一批序号，inbox 必须判成重复而不是新事实。
        let mut second = DurableProcessAdapter::new(config, 2);
        let again = second.observations("inv-dedup");
        assert_eq!(again.len(), 2);
        for envelope in &again {
            let receipt = receive_observation(&mut store, envelope, None, 20).expect("接收");
            assert_eq!(receipt.disposition, "duplicate", "重放必须是重复");
            assert!(!receipt.persisted, "重复不得再落库");
        }
        assert_eq!(store.count_observations("inv-dedup").expect("计数"), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn partial_line_waits_for_newline() {
        let root = temp_root("partial");
        let dir = root.join(session_dir_name("inv-partial"));
        std::fs::create_dir_all(&dir).expect("建目录");
        write_atomic(
            &dir.join(SESSION_FILE),
            &SessionMeta {
                pid: -1,
                started_at_ms: 5,
                execution_id: "durable:inv-partial".to_string(),
            }
            .render(),
        )
        .expect("写会话");
        std::fs::write(dir.join(EVENTS_FILE), "a\nhalf").expect("写事件");

        let config = DurableProcessConfig::new("python", vec![], &root);
        let mut adapter = DurableProcessAdapter::new(config, 1);
        let first = adapter.observations("inv-partial");
        assert_eq!(first.len(), 1, "半截行不能被当成一条事件：{first:?}");
        assert_eq!(first[0].payload, "a");

        let mut handle = OpenOptions::new()
            .append(true)
            .open(dir.join(EVENTS_FILE))
            .expect("追加");
        handle.write_all(b" tail\n").expect("追加写");
        let second = adapter.observations("inv-partial");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].source_sequence, 2);
        assert_eq!(second[0].payload, "half tail");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_session_is_unknown_not_running() {
        let root = temp_root("missing");
        let config = DurableProcessConfig::new("python", vec![], &root);
        let mut adapter = DurableProcessAdapter::new(config, 1);
        assert_eq!(adapter.inspect("never-started"), None);
        assert!(adapter.observations("never-started").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_confirms_stop_and_terminal_state_is_cancelled() {
        let root = temp_root("cancel");
        let Some(mut adapter) = detect(&root) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        adapter.set_args(
            script(
                &adapter,
                "import time\ntime.sleep(10)",
                "setTimeout(()=>{},10000);",
            ),
            now_ms(),
        );
        assert!(matches!(
            adapter.start(&request("inv-cancel")),
            StartOutcome::Accepted { .. }
        ));
        assert_eq!(adapter.inspect("inv-cancel"), Some(ExecutionState::Running));
        assert_eq!(adapter.cancel("inv-cancel"), CancelStage::Confirmed);
        assert_eq!(
            adapter.inspect("inv-cancel"),
            Some(ExecutionState::Cancelled),
            "确认停止后必须落 Cancelled，不能停在 Running"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_start_does_not_spawn_a_second_process() {
        let root = temp_root("dup");
        let Some(mut adapter) = detect(&root) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        adapter.set_args(
            script(
                &adapter,
                "import time\ntime.sleep(5)",
                "setTimeout(()=>{},5000);",
            ),
            now_ms(),
        );
        adapter.start(&request("inv-dup"));
        let pid = adapter.read_session("inv-dup").expect("会话记录").pid;
        let again = adapter.start(&request("inv-dup"));
        match again {
            StartOutcome::Replayed { execution_id } => {
                assert_eq!(execution_id, "durable:inv-dup")
            }
            other => panic!("重复启动必须是重放：{other:?}"),
        }
        assert_eq!(
            adapter.read_session("inv-dup").expect("会话记录").pid,
            pid,
            "重放不得换进程号"
        );
        assert_eq!(adapter.children.len(), 1, "不得产生第二个子进程句柄");
        let _ = adapter.cancel("inv-dup");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn after_restart_a_dead_child_is_unknown_not_terminal() {
        let root = temp_root("died");
        let Some(mut first) = detect(&root) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        first.set_args(
            script(
                &first,
                "import time\ntime.sleep(10)",
                "setTimeout(()=>{},10000);",
            ),
            now_ms(),
        );
        first.start(&request("inv-died"));
        let pid = first.read_session("inv-died").expect("会话记录").pid as u32;
        assert!(kill_pid(pid), "测试需要真的杀掉子进程");
        // 等进程真的消失，避免用"刚杀完还残留"的瞬间做断言。
        let mut waited = Duration::ZERO;
        while pid_alive(pid) && waited < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
            waited += Duration::from_millis(50);
        }
        assert!(!pid_alive(pid), "子进程应已消失");
        // 先断言、后回收，顺序不能反：先 wait 掉僵尸，signal 探测也会返回 false，
        // 这条断言就退化成同义反复，测不到「僵尸被判死」这件事。
        if let Some(mut child) = first.children.remove("inv-died") {
            let _ = child.wait();
        }
        drop(first);

        let mut second = detect(&root).expect("第二个适配器");
        // 宿主不在时子进程死了：没有退出码，只能报未知；不能猜 Completed/Failed。
        assert_eq!(second.inspect("inv-died"), Some(ExecutionState::Unknown));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 僵尸判定是纯字符串解析，可以在任何平台测——而这正是 Linux 那条红灯的根因，
    /// 不能只靠「在 Linux 上跑一遍」来兜。
    #[test]
    fn zombie_state_reads_as_dead_while_signal_probe_would_say_alive() {
        // 真实 /proc/<pid>/stat 的形状：pid (comm) state ppid pgrp ...
        assert_eq!(
            proc_stat_reports_dead("1234 (python3) S 1 1234 1234 0 -1"),
            Some(false),
            "S（可中断睡眠）是活着"
        );
        assert_eq!(
            proc_stat_reports_dead("1234 (python3) Z 1 1234 1234 0 -1"),
            Some(true),
            "Z（僵尸）必须判死：kill -0 对它仍然成功"
        );
        assert_eq!(proc_stat_reports_dead("1234 (python3) X 1"), Some(true));
        assert_eq!(
            proc_stat_reports_dead("1234 (my (weird) proc) Z 1"),
            Some(true),
            "comm 含空格与括号时按最后一个 ')' 切"
        );
        // 不认识就返回 None，让调用方回落；把垃圾当成「活着」会让进程永远收不了尾。
        assert_eq!(proc_stat_reports_dead("garbage"), None);
        assert_eq!(proc_stat_reports_dead("1234 (python3)"), None);
        assert_eq!(proc_stat_reports_dead(""), None);
    }

    #[test]
    fn stderr_shares_the_same_event_stream() {
        let root = temp_root("stderr");
        let Some(mut adapter) = detect(&root) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        adapter.set_args(
            script(
                &adapter,
                "import sys\nprint('out', flush=True)\nprint('err', file=sys.stderr, flush=True)",
                "console.log('out'); console.error('err');",
            ),
            now_ms(),
        );
        adapter.start(&request("inv-err"));
        let events = collect_until(&mut adapter, "inv-err", 2, Duration::from_secs(5));
        let payloads: Vec<&str> = events.iter().map(|e| e.payload.as_str()).collect();
        assert!(
            payloads.contains(&"out"),
            "stdout 应进事件文件：{payloads:?}"
        );
        assert!(
            payloads.contains(&"err"),
            "stderr 也应进同一事件文件：{payloads:?}"
        );
        let _ = adapter.cancel("inv-err");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 对真实适配器跑完整 conformance：闸门类三条都必须有证据，其余故障类如实 not_applicable。
    #[test]
    fn live_suite_passes_core_cases_and_reports_gate_cases_as_not_applicable() {
        let root = temp_root("suite");
        let Some(mut adapter) = detect(&root) else {
            eprintln!("跳过：本机没有 python/node");
            return;
        };
        adapter.use_conformance_payload(now_ms());
        let report = crate::adapter_conformance::run_live_suite(&mut adapter, now_ms());
        let (pass, fail, na) = report.summary();
        assert_eq!(
            fail,
            0,
            "真实适配器不得有用例失败：\n{}",
            report.to_markdown()
        );
        assert!(pass >= 7, "核心用例应真过：\n{}", report.to_markdown());
        assert_eq!(
            na,
            7,
            "只有真正需要故障注入的七条应记 not_applicable：\n{}",
            report.to_markdown()
        );

        // 闸门类三条全部有证据：授权走「无凭据面 + 授权约束仍生效」，完整性走真实负载。
        // 这正是 durable-process 相对 local-process 的差别之一：它可以被准入。
        assert_eq!(
            crate::adapter_conformance::enablement(&report),
            crate::adapter_conformance::Enablement::Enabled,
            "闸门类齐全时应可准入：\n{}",
            report.to_markdown()
        );
        // 取消请求与确认分离这条必须真过：否则"确认停止"没有证据。
        let cancel = report
            .outcomes
            .iter()
            .find(|outcome| outcome.id == "cancel-request-vs-confirm")
            .expect("用例必须存在");
        assert!(
            matches!(
                cancel.status,
                crate::adapter_conformance::CaseStatus::Pass { .. }
            ),
            "取消用例应通过：{cancel:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
