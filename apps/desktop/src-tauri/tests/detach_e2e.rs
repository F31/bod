//! 端到端验证桌面壳的 detach 语义（P0 任务 8.1 的进程级证据）。
//!
//! 8.1 的验收里最容易"只在文档里成立"的两条是：
//! - 桌面退出不终止外部已有宿主；
//! - 启动密钥不出现在日志。
//!
//! 这里用**真实 `bod serve` 子进程 + 真实 TCP 握手**去证明它们，而不是只测决策函数：
//! 1. 本次启动的宿主，在连接句柄被丢弃后仍然存活（`close_action=detach` 的物理含义：
//!    子进程不在 Job Object 里、不 kill-on-drop）；
//! 2. 外部已有宿主，即使配置写成 `stop_host` 也不会被桌面停止；
//! 3. 两边的连接日志都只有脱敏后的会话密钥。
//!
//! 前置条件：先构建宿主二进制（`cargo build -p bod`），否则测试直接失败，
//! 不允许"缺文件就跳过"，否则等于把通过写在未执行上。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bod_desktop::connection::{
    Attached, CloseAction, ConnectRequest, ConnectionLog, HostControl, StdHostControl,
    StdHostProbe, StdHostSpawner, close_connection, connect_or_spawn,
};

fn bod_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("BOD_BIN") {
        return PathBuf::from(path);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.join("../../../target");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or(target);
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    target
        .join(profile)
        .join(format!("bod{}", std::env::consts::EXE_SUFFIX))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bod-desktop-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for_descriptor(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// 宿主存活判据：能重新完成一次健康检查 + 握手。
fn host_is_alive(descriptor: &Path) -> bool {
    bod_app::host_handshake::probe(descriptor, Duration::from_secs(2)).is_ok()
}

struct HostGuard {
    child: Child,
    descriptor: PathBuf,
    dir: PathBuf,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.descriptor);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 外部宿主：由"别人"启动，桌面只是连接方。
fn start_external_host(tag: &str) -> HostGuard {
    let dir = temp_dir(tag);
    let descriptor = dir.join("host.endpoint.json");
    let child = Command::new(bod_bin())
        .args(["serve", "--listen", "127.0.0.1:0", "--descriptor"])
        .arg(&descriptor)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("应能启动 bod serve（先执行 cargo build -p bod）");
    assert!(
        wait_for_descriptor(&descriptor, Duration::from_secs(15)),
        "宿主未在 15s 内写出连接描述：{}",
        descriptor.display()
    );
    HostGuard {
        child,
        descriptor,
        dir,
    }
}

/// 记录调用次数：用来证明"没发请求"，而不是"发了请求但宿主没死"。
struct RecordingControl {
    calls: usize,
}

impl HostControl for RecordingControl {
    fn shutdown(&mut self, _attached: &Attached) -> Result<(), String> {
        self.calls += 1;
        Ok(())
    }
}

fn assert_log_is_redacted(log: &ConnectionLog, session_key: &str) {
    let rendered = log.rendered();
    assert!(
        !rendered.contains(session_key),
        "连接日志不得包含会话密钥：{rendered}"
    );
    assert!(
        rendered.contains("session_key=<redacted>"),
        "握手日志应显式标注脱敏位置：{rendered}"
    );
}

#[test]
fn spawned_host_survives_detach_after_connection_is_dropped() {
    let dir = temp_dir("spawned");
    let descriptor_path = dir.join("host.endpoint.json");

    let mut log = ConnectionLog::default();
    let attached = {
        let mut spawner = StdHostSpawner {
            bod_bin: bod_bin(),
            descriptor_path: descriptor_path.clone(),
            startup_timeout: Duration::from_secs(15),
        };
        let request = ConnectRequest {
            descriptor_path: &descriptor_path,
            close_action: CloseAction::Detach,
            startup_timeout: Duration::from_secs(15),
        };
        let probe = StdHostProbe {
            timeout: Duration::from_secs(2),
        };
        // spawner 在这个作用域结束时被丢弃：它的 Child 句柄不 kill-on-drop，
        // 因此这里的 detach 是物理意义上的 detach，而不是"记住要 detach"。
        connect_or_spawn(&request, &probe, &mut spawner, &mut log).expect("连接或启动应成功")
    };

    assert!(
        attached.spawned_by_desktop,
        "描述文件不存在时应由本次桌面启动宿主"
    );
    assert!(!attached.session_key.is_empty());

    // 1) 句柄丢弃后宿主仍存活。
    assert!(
        host_is_alive(&descriptor_path),
        "连接句柄丢弃不应终止宿主进程 pid={}",
        attached.descriptor.pid
    );

    // 2) close_action=detach 不发送停止请求，只记一条事实。
    let mut control = RecordingControl { calls: 0 };
    let stopped = close_connection(&attached, CloseAction::Detach, &mut control, &mut log)
        .expect("detach 不应失败");
    assert!(!stopped, "detach 不得发送停止请求");
    assert_eq!(control.calls, 0, "detach 不应触碰控制面");
    assert!(
        host_is_alive(&descriptor_path),
        "detach 之后宿主必须继续运行"
    );
    assert!(
        log.rendered().contains("detach"),
        "应记录 detach 事实：{}",
        log.rendered()
    );

    // 3) 启动密钥只出现在内存连接句柄里。
    assert_log_is_redacted(&log, &attached.session_key);
    let descriptor_raw = std::fs::read_to_string(&descriptor_path).unwrap();
    assert!(
        !descriptor_raw.contains(&attached.session_key),
        "描述文件里不得出现会话密钥"
    );

    // 收尾：显式授权停止，验证这条连接确实能管到该宿主。
    let mut control = StdHostControl {
        timeout: Duration::from_secs(2),
    };
    let stopped = close_connection(&attached, CloseAction::StopHost, &mut control, &mut log)
        .expect("本次启动的宿主允许 stop_host");
    assert!(stopped, "stop_host 应真的发出停止请求");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < deadline {
        if !host_is_alive(&descriptor_path) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(exited, "收到授权停止后宿主应退出");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn external_host_is_never_stopped_by_desktop_close() {
    let mut host = start_external_host("external");

    let mut log = ConnectionLog::default();
    let mut spawner = StdHostSpawner {
        bod_bin: bod_bin(),
        descriptor_path: host.descriptor.clone(),
        startup_timeout: Duration::from_secs(5),
    };
    let request = ConnectRequest {
        descriptor_path: &host.descriptor,
        close_action: CloseAction::StopHost,
        startup_timeout: Duration::from_secs(5),
    };
    let probe = StdHostProbe {
        timeout: Duration::from_secs(2),
    };
    let attached = connect_or_spawn(&request, &probe, &mut spawner, &mut log)
        .expect("已有可用宿主时应直接连上");

    assert!(
        !attached.spawned_by_desktop,
        "连接已有宿主时不得标记为本次启动"
    );
    assert_eq!(attached.descriptor.pid, host.child.id());

    // 即使配置写了 stop_host，外部宿主也不归桌面管。
    let mut control = RecordingControl { calls: 0 };
    let stopped = close_connection(&attached, CloseAction::StopHost, &mut control, &mut log)
        .expect("忽略 stop_host 不是错误");
    assert!(!stopped, "外部宿主不得被桌面停止");
    assert_eq!(control.calls, 0, "不得向外部宿主发送停止请求");
    assert!(
        host_is_alive(&host.descriptor),
        "桌面关闭后外部宿主必须继续运行"
    );
    assert!(
        log.rendered().contains("stop_host ignored"),
        "应记录忽略 stop_host 的事实：{}",
        log.rendered()
    );
    assert_log_is_redacted(&log, &attached.session_key);

    // 收尾：真正停止外部宿主的是它自己的授权调用，而不是桌面。
    let mut control = StdHostControl {
        timeout: Duration::from_secs(2),
    };
    control
        .shutdown(&attached)
        .expect("带会话密钥的停止请求应被接受");
    assert!(
        wait_for_exit(&mut host.child, Duration::from_secs(10)),
        "宿主收到授权停止后应退出"
    );
}
