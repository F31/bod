//! 端到端验证 `bod serve` 与桌面握手（P0 任务 8.1 的宿主对端）。
//!
//! 这里刻意用**真实子进程 + 真实 TCP**，不用 mock：
//! 8.1 之前的缺口正是"只有决策逻辑、没有可握手的宿主"。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct HostGuard {
    child: Child,
    descriptor: PathBuf,
    _dir: PathBuf,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.descriptor);
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bod-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for_descriptor(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn start_host(tag: &str) -> HostGuard {
    let dir = temp_dir(tag);
    let descriptor = dir.join("host.endpoint.json");
    let child = Command::new(env!("CARGO_BIN_EXE_bod"))
        .args(["serve", "--listen", "127.0.0.1:0", "--descriptor"])
        .arg(&descriptor)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("应能启动 bod serve");
    let guard = HostGuard {
        child,
        descriptor: descriptor.clone(),
        _dir: dir,
    };
    assert!(
        wait_for_descriptor(&descriptor, Duration::from_secs(15)),
        "宿主未在 15s 内写出连接描述：{}",
        descriptor.display()
    );
    guard
}

#[test]
fn handshake_returns_session_key_and_graceful_shutdown_removes_descriptor() {
    let mut host = start_host("handshake");
    let attached = bod_app::host_handshake::probe(&host.descriptor, Duration::from_secs(2))
        .expect("握手应成功");

    assert_eq!(
        attached.descriptor.protocol,
        bod_app::host_handshake::PROTOCOL
    );
    assert_eq!(attached.descriptor.pid, host.child.id());
    assert!(!attached.session_key.is_empty());
    assert_ne!(
        attached.session_key, attached.descriptor.handshake_token,
        "会话密钥与握手令牌必须是两个值"
    );

    // 描述文件里没有会话密钥（它只存在于宿主内存与本次连接句柄）。
    let raw = std::fs::read_to_string(&host.descriptor).unwrap();
    assert!(!raw.contains(&attached.session_key));

    bod_app::host_handshake::request_shutdown(
        &attached.descriptor.address,
        &attached.session_key,
        Duration::from_secs(2),
    )
    .expect("带会话密钥的停止请求应被接受");

    // 宿主应自行退出并移除描述文件，避免留下"看似可用"的陈旧地址。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < deadline {
        if let Some(_status) = host.child.try_wait().unwrap() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(exited, "宿主收到停止请求后应退出");
    assert!(!host.descriptor.exists(), "正常停止后应移除连接描述");
}

#[test]
fn serve_json_readiness_line_never_contains_credentials() {
    let dir = temp_dir("json");
    let descriptor = dir.join("host.endpoint.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bod"))
        .args(["serve", "--listen", "127.0.0.1:0", "--descriptor"])
        .arg(&descriptor)
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("应能启动 bod serve --json");

    let stdout = child.stdout.take().expect("应拿到 stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("应读到就绪行");

    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("就绪行必须是 JSON");
    assert_eq!(value["ready"], true);
    assert!(
        value["address"]
            .as_str()
            .is_some_and(|a| a.starts_with("127.0.0.1:"))
    );
    assert!(
        value["instance_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    for forbidden in ["session_key", "handshake_token", "token"] {
        assert!(
            !line.contains(forbidden),
            "就绪行不得包含凭据字段 {forbidden}：{line}"
        );
    }

    // 收尾：正常握手后停止。
    assert!(wait_for_descriptor(&descriptor, Duration::from_secs(15)));
    let attached =
        bod_app::host_handshake::probe(&descriptor, Duration::from_secs(2)).expect("握手应成功");
    let _ = bod_app::host_handshake::request_shutdown(
        &attached.descriptor.address,
        &attached.session_key,
        Duration::from_secs(2),
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
