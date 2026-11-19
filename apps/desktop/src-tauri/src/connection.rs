//! 桌面连接控制器：`connect_or_spawn`、握手取地址、`close_action=detach`（P0 任务 8.1）。
//!
//! 规格来源：V2.9.6 §8.5 生命周期、§16.1 本地控制面、§37.9 桌面配置（`desktop.close_action`）。
//!
//! 三条硬约束：
//! 1. **不启动第二个业务 DB**：先探测已有宿主；能连就连，只有确认没有可用宿主才 spawn。
//! 2. **桌面退出不终止外部已有宿主**：`close_action = detach` 只断开连接，
//!    只有在"本次自己 spawn"且显式选择 `stop_host` 时才发送停止。
//! 3. **启动密钥不进日志**：握手返回的 session key 只保留在内存连接句柄里，
//!    写入日志的一律是 `<redacted>`；地址、pid、实例 ID 可记录。
//!
//! 本模块不依赖 Tauri：窗口与 WebView 接线在 `crate::shell`（feature `tauri-shell`），
//! 决策与副作用记录在这里，可被单测直接驱动。

use std::path::Path;
use std::time::Duration;

/// 桌面关闭窗口时的行为（对应 `desktop.close_action`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// 默认：只断开连接，宿主继续运行。
    Detach,
    /// 仅当宿主由本次启动时允许；外部已有宿主永不适用。
    StopHost,
}

impl CloseAction {
    /// 配置解析：未知值回落到最保守的 detach，而不是停宿主。
    pub fn parse(value: &str) -> Self {
        match value {
            "stop_host" => Self::StopHost,
            _ => Self::Detach,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detach => "detach",
            Self::StopHost => "stop_host",
        }
    }
}

/// 宿主写出的连接描述（由宿主进程在监听成功后原子写出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDescriptor {
    /// 本地控制面地址，例如 `127.0.0.1:53001`。
    pub address: String,
    pub pid: u32,
    /// 宿主实例标识：用于判断"是不是同一个宿主"，不是安全凭据。
    pub instance_id: String,
}

/// 连接结果：桌面壳持有它，直到窗口关闭。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub descriptor: HostDescriptor,
    /// 握手得到的会话密钥；**不得写入日志或界面**。
    pub session_key: String,
    /// true 表示宿主由本次桌面启动；只有这种情况才允许 stop_host。
    pub spawned_by_desktop: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// 描述文件存在但握手失败，且不允许再 spawn 第二个宿主。
    HandshakeFailed(String),
    /// spawn 后超时仍没有拿到描述。
    SpawnTimeout {
        waited_ms: u64,
    },
    SpawnFailed(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HandshakeFailed(detail) => write!(f, "连接已有宿主失败：{detail}"),
            Self::SpawnTimeout { waited_ms } => {
                write!(f, "启动宿主后 {waited_ms} ms 内未取得握手地址")
            }
            Self::SpawnFailed(detail) => write!(f, "启动宿主失败：{detail}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// 探测与握手：读描述文件并确认该 pid 仍活着，返回会话密钥。
pub trait HostProbe {
    fn probe(&self, descriptor_path: &Path) -> Option<(HostDescriptor, String)>;
}

/// 启动宿主并等待其写出描述（内部按 `startup_timeout` 有界等待）。
pub trait HostSpawner {
    fn spawn_and_wait(&mut self) -> Result<(HostDescriptor, String), ConnectError>;
}

/// 停止宿主：实现方负责发请求，且只应针对自己启动的实例。
///
/// 入参是完整连接句柄而不是裸 pid：停止需要 `address`（控制面端点）与 `session_key`
/// （授权），只凭 pid 既发不出请求，也容易误杀无关进程。
pub trait HostControl {
    fn shutdown(&mut self, attached: &Attached) -> Result<(), String>;
}

/// 连接日志：只记录可公开事实，密钥一律脱敏。
#[derive(Debug, Default, Clone)]
pub struct ConnectionLog {
    entries: Vec<String>,
}

impl ConnectionLog {
    pub fn record(&mut self, line: impl Into<String>) {
        self.entries.push(line.into());
    }

    /// 记录一次握手：地址与 pid 可见，密钥脱敏。
    pub fn record_handshake(&mut self, descriptor: &HostDescriptor, spawned: bool) {
        self.entries.push(format!(
            "handshake ok: address={} pid={} instance={} spawned_by_desktop={} session_key=<redacted>",
            descriptor.address, descriptor.pid, descriptor.instance_id, spawned
        ));
    }

    pub fn rendered(&self) -> String {
        self.entries.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 真实探测：读描述文件 → 健康检查 → 握手换会话密钥。
///
/// 复用 `bod_app::host_handshake`，避免桌面壳与宿主各写一份协议解析后漂移。
#[derive(Debug, Clone, Copy)]
pub struct StdHostProbe {
    pub timeout: Duration,
}

impl HostProbe for StdHostProbe {
    fn probe(&self, descriptor_path: &Path) -> Option<(HostDescriptor, String)> {
        let attached = bod_app::host_handshake::probe(descriptor_path, self.timeout).ok()?;
        Some((
            HostDescriptor {
                address: attached.descriptor.address,
                pid: attached.descriptor.pid,
                instance_id: attached.descriptor.instance_id,
            },
            attached.session_key,
        ))
    }
}

/// 真实启动：运行 `bod serve`，等它写出描述并完成握手。
///
/// 子进程**不放入 Job Object、不设置 kill-on-drop**：桌面退出只会 detach，
/// 宿主按自己的生命周期继续运行（这正是 `close_action=detach` 的物理含义）。
#[derive(Debug, Clone)]
pub struct StdHostSpawner {
    pub bod_bin: std::path::PathBuf,
    pub descriptor_path: std::path::PathBuf,
    pub startup_timeout: Duration,
}

impl StdHostSpawner {
    /// 解析 `bod` 可执行文件：`BOD_BIN` 优先，其次与桌面壳同目录，最后靠 PATH。
    pub fn resolve_binary() -> std::path::PathBuf {
        if let Some(path) = std::env::var_os("BOD_BIN") {
            return std::path::PathBuf::from(path);
        }
        if let Ok(current) = std::env::current_exe()
            && let Some(dir) = current.parent()
        {
            for name in ["bod.exe", "bod"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
        std::path::PathBuf::from("bod")
    }
}

impl HostSpawner for StdHostSpawner {
    fn spawn_and_wait(&mut self) -> Result<(HostDescriptor, String), ConnectError> {
        // 明确由本次启动掌管的路径：旧描述一定是过期的，删掉避免读到别人的地址。
        let _ = std::fs::remove_file(&self.descriptor_path);
        let mut child = std::process::Command::new(&self.bod_bin)
            .arg("serve")
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--descriptor")
            .arg(&self.descriptor_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|err| {
                ConnectError::SpawnFailed(format!("{}: {err}", self.bod_bin.display()))
            })?;

        let started = std::time::Instant::now();
        let probe = StdHostProbe {
            timeout: Duration::from_millis(500),
        };
        loop {
            if let Some((descriptor, session_key)) = probe.probe(&self.descriptor_path) {
                return Ok((descriptor, session_key));
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|err| ConnectError::SpawnFailed(err.to_string()))?
            {
                return Err(ConnectError::SpawnFailed(format!(
                    "宿主在写出描述前退出（{status}）"
                )));
            }
            if started.elapsed() >= self.startup_timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ConnectError::SpawnTimeout {
                    waited_ms: self.startup_timeout.as_millis() as u64,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// 真实停止：带会话密钥请求 `/_bod/shutdown`，宿主自行移除描述文件。
#[derive(Debug, Clone, Copy)]
pub struct StdHostControl {
    pub timeout: Duration,
}

impl HostControl for StdHostControl {
    fn shutdown(&mut self, attached: &Attached) -> Result<(), String> {
        bod_app::host_handshake::request_shutdown(
            &attached.descriptor.address,
            &attached.session_key,
            self.timeout,
        )
        .map_err(|err| err.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct ConnectRequest<'a> {
    pub descriptor_path: &'a Path,
    pub close_action: CloseAction,
    pub startup_timeout: Duration,
}

/// 连接优先、必要时启动：只有探测失败才 spawn，且返回结果标明归属。
pub fn connect_or_spawn(
    request: &ConnectRequest<'_>,
    probe: &dyn HostProbe,
    spawner: &mut dyn HostSpawner,
    log: &mut ConnectionLog,
) -> Result<Attached, ConnectError> {
    if request.descriptor_path.exists() {
        match probe.probe(request.descriptor_path) {
            Some((descriptor, session_key)) => {
                log.record_handshake(&descriptor, false);
                return Ok(Attached {
                    descriptor,
                    session_key,
                    spawned_by_desktop: false,
                });
            }
            None => {
                // 描述存在但宿主不可用：记录事实，不删除他人文件，继续走 spawn。
                log.record(format!(
                    "stale descriptor ignored: {}（未删除，交由宿主自己回收）",
                    request.descriptor_path.display()
                ));
            }
        }
    } else {
        log.record(format!(
            "no descriptor at {}，准备启动宿主",
            request.descriptor_path.display()
        ));
    }

    let (descriptor, session_key) = spawner.spawn_and_wait().map_err(|err| {
        log.record(format!("spawn failed: {err}"));
        err
    })?;
    log.record_handshake(&descriptor, true);
    if request.startup_timeout.is_zero() {
        return Err(ConnectError::SpawnTimeout { waited_ms: 0 });
    }
    Ok(Attached {
        descriptor,
        session_key,
        spawned_by_desktop: true,
    })
}

/// 关闭窗口：按 `close_action` 决定是否停止宿主，并保证不误杀外部宿主。
///
/// 返回值表示"是否真的发送了停止请求"，便于桌面壳在退出前记一条事实。
pub fn close_connection(
    attached: &Attached,
    close_action: CloseAction,
    control: &mut dyn HostControl,
    log: &mut ConnectionLog,
) -> Result<bool, String> {
    if close_action == CloseAction::Detach {
        log.record(format!(
            "detach: 断开 address={}，宿主继续运行（close_action=detach）",
            attached.descriptor.address
        ));
        return Ok(false);
    }
    if !attached.spawned_by_desktop {
        // 外部已有宿主：即使配置写了 stop_host 也不得停止别人的宿主。
        log.record(format!(
            "stop_host ignored: pid={} 不是本次桌面启动，保持运行",
            attached.descriptor.pid
        ));
        return Ok(false);
    }
    control.shutdown(attached)?;
    log.record(format!(
        "stop_host: 已请求停止本次启动的宿主 pid={}",
        attached.descriptor.pid
    ));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    const KEY: &str = "sk-live-9f2c-should-never-be-logged";

    struct FakeProbe {
        result: Option<(HostDescriptor, String)>,
        calls: RefCell<usize>,
    }

    impl HostProbe for FakeProbe {
        fn probe(&self, _path: &Path) -> Option<(HostDescriptor, String)> {
            *self.calls.borrow_mut() += 1;
            self.result.clone()
        }
    }

    struct FakeSpawner {
        result: Result<(HostDescriptor, String), ConnectError>,
        calls: RefCell<usize>,
    }

    impl HostSpawner for FakeSpawner {
        fn spawn_and_wait(&mut self) -> Result<(HostDescriptor, String), ConnectError> {
            *self.calls.borrow_mut() += 1;
            self.result.clone()
        }
    }

    struct FakeControl {
        calls: RefCell<Vec<u32>>,
    }

    impl HostControl for FakeControl {
        fn shutdown(&mut self, attached: &Attached) -> Result<(), String> {
            self.calls.borrow_mut().push(attached.descriptor.pid);
            Ok(())
        }
    }

    fn descriptor(pid: u32) -> HostDescriptor {
        HostDescriptor {
            address: "127.0.0.1:53001".to_string(),
            pid,
            instance_id: "inst-1".to_string(),
        }
    }

    fn request(path: &Path) -> ConnectRequest<'_> {
        ConnectRequest {
            descriptor_path: path,
            close_action: CloseAction::Detach,
            startup_timeout: Duration::from_millis(5_000),
        }
    }

    fn temp_descriptor(name: &str, exists: bool) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bod-desktop-{name}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        if exists {
            std::fs::write(&path, "{}").unwrap();
        }
        path
    }

    #[test]
    fn attach_does_not_spawn_and_never_stops_host() {
        let path = temp_descriptor("attach", true);
        let probe = FakeProbe {
            result: Some((descriptor(4100), KEY.to_string())),
            calls: RefCell::new(0),
        };
        let mut spawner = FakeSpawner {
            result: Err(ConnectError::SpawnFailed("不应被调用".to_string())),
            calls: RefCell::new(0),
        };
        let mut control = FakeControl {
            calls: RefCell::new(Vec::new()),
        };
        let mut log = ConnectionLog::default();

        let attached = connect_or_spawn(&request(&path), &probe, &mut spawner, &mut log).unwrap();
        assert!(!attached.spawned_by_desktop);
        assert_eq!(*spawner.calls.borrow(), 0, "已有宿主时不得启动第二个");

        let stopped =
            close_connection(&attached, CloseAction::Detach, &mut control, &mut log).unwrap();
        assert!(!stopped);
        assert!(control.calls.borrow().is_empty(), "detach 不得停止宿主");
        assert!(log.rendered().contains("宿主继续运行"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stale_descriptor_is_reported_and_triggers_spawn() {
        let path = temp_descriptor("stale", true);
        let probe = FakeProbe {
            result: None,
            calls: RefCell::new(0),
        };
        let mut spawner = FakeSpawner {
            result: Ok((descriptor(4200), KEY.to_string())),
            calls: RefCell::new(0),
        };
        let mut log = ConnectionLog::default();

        let attached = connect_or_spawn(&request(&path), &probe, &mut spawner, &mut log).unwrap();
        assert!(attached.spawned_by_desktop);
        assert_eq!(*spawner.calls.borrow(), 1);
        assert!(log.rendered().contains("stale descriptor ignored"));
        assert!(path.exists(), "不删除他人写下的描述文件，交由宿主回收");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn session_key_never_appears_in_log() {
        let path = temp_descriptor("key", false);
        let probe = FakeProbe {
            result: None,
            calls: RefCell::new(0),
        };
        let mut spawner = FakeSpawner {
            result: Ok((descriptor(4300), KEY.to_string())),
            calls: RefCell::new(0),
        };
        let mut log = ConnectionLog::default();
        let attached = connect_or_spawn(&request(&path), &probe, &mut spawner, &mut log).unwrap();
        assert_eq!(attached.session_key, KEY);
        let rendered = log.rendered();
        assert!(
            !rendered.contains(KEY),
            "启动密钥不得出现在日志：{rendered}"
        );
        assert!(rendered.contains("session_key=<redacted>"));
        assert!(rendered.contains("127.0.0.1:53001"), "地址可记录");
    }

    #[test]
    fn stop_host_never_kills_external_host() {
        let path = temp_descriptor("external", true);
        let probe = FakeProbe {
            result: Some((descriptor(4400), KEY.to_string())),
            calls: RefCell::new(0),
        };
        let mut spawner = FakeSpawner {
            result: Err(ConnectError::SpawnFailed("不应被调用".to_string())),
            calls: RefCell::new(0),
        };
        let mut control = FakeControl {
            calls: RefCell::new(Vec::new()),
        };
        let mut log = ConnectionLog::default();
        let attached = connect_or_spawn(&request(&path), &probe, &mut spawner, &mut log).unwrap();

        let stopped =
            close_connection(&attached, CloseAction::StopHost, &mut control, &mut log).unwrap();
        assert!(!stopped);
        assert!(
            control.calls.borrow().is_empty(),
            "外部已有宿主即使配置 stop_host 也不得停止"
        );
        assert!(log.rendered().contains("不是本次桌面启动"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stop_host_stops_only_self_spawned_host() {
        let path = temp_descriptor("self", false);
        let probe = FakeProbe {
            result: None,
            calls: RefCell::new(0),
        };
        let mut spawner = FakeSpawner {
            result: Ok((descriptor(4500), KEY.to_string())),
            calls: RefCell::new(0),
        };
        let mut control = FakeControl {
            calls: RefCell::new(Vec::new()),
        };
        let mut log = ConnectionLog::default();
        let attached = connect_or_spawn(&request(&path), &probe, &mut spawner, &mut log).unwrap();

        let stopped =
            close_connection(&attached, CloseAction::StopHost, &mut control, &mut log).unwrap();
        assert!(stopped);
        assert_eq!(*control.calls.borrow(), vec![4500]);
    }

    #[test]
    fn unknown_close_action_falls_back_to_detach() {
        assert_eq!(CloseAction::parse("stop_host"), CloseAction::StopHost);
        assert_eq!(CloseAction::parse("detach"), CloseAction::Detach);
        assert_eq!(
            CloseAction::parse("kill_everything"),
            CloseAction::Detach,
            "未知值不得升级为停止宿主"
        );
    }
}
