//! HTTP/SSE 基线订阅的端到端验证（P6 2.6 / RM06）。
//!
//! 刻意用**真实子进程 + 真实 TCP**：P6 之前的缺口正是"只有游标语义、没有传输层"。
//! 这里断言的是线上字节：状态码、`text/event-stream`、`id:`/`event:` 帧与 `resync` 提示。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bod-sse-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn start_host() -> HostGuard {
    let dir = temp_dir("host");
    let descriptor = dir.join("host.endpoint.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bod"))
        .args(["serve", "--listen", "127.0.0.1:0", "--descriptor"])
        .arg(&descriptor)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("应能启动 bod serve");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if descriptor.exists() {
            return HostGuard {
                child,
                descriptor,
                dir,
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // 超时也要先回收子进程再 panic，否则测试失败会留下僵尸进程。
    let _ = child.kill();
    let _ = child.wait();
    panic!("宿主未在 20 s 内写出描述文件");
}

/// 发一个请求并读到对端关闭（SSE 响应没有 Content-Length）。
fn request(host: &HostGuard, session_key: Option<&str>, extra: &str) -> String {
    let descriptor: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&host.descriptor).expect("读描述文件"))
            .expect("描述文件是 JSON");
    let address = descriptor["address"].as_str().expect("描述含地址");

    let mut stream = TcpStream::connect(address).expect("连接宿主");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("设置读超时");
    let auth = match session_key {
        Some(key) => format!("Authorization: Bearer {key}\r\n"),
        None => String::new(),
    };
    let head = format!(
        "GET /_bod/events{extra} HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).expect("写请求");

    let mut raw = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&raw).to_string()
}

fn session_key(host: &HostGuard) -> String {
    let attached = bod_app::host_handshake::probe(&host.descriptor, Duration::from_secs(5))
        .expect("握手应成功");
    attached.session_key
}

fn body(raw: &str) -> &str {
    raw.split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

#[test]
fn sse_baseline_transport_delivers_cursor_semantics_over_real_tcp() {
    let host = start_host();
    let key = session_key(&host);

    // 1) 没有 Bearer 一律 401，且说明原因；不得返回任何事件字节。
    let denied = request(&host, None, "?follow=0");
    assert!(
        denied.starts_with("HTTP/1.1 401"),
        "缺少凭据必须 401：{denied}"
    );
    assert!(denied.contains("BOD_STREAM_DENIED"));
    assert!(
        body(&denied).contains("Authorization: Bearer"),
        "拒绝理由要指出缺什么：{denied}"
    );
    assert!(!denied.contains("event: host.ready"));

    // 2) 首次订阅：真实 SSE 响应头 + 一条 host.ready 事件（宿主启动时发布）。
    let first = request(&host, Some(&key), "?follow=0");
    assert!(first.starts_with("HTTP/1.1 200"), "带密钥应 200：{first}");
    assert!(
        first
            .to_lowercase()
            .contains("content-type: text/event-stream"),
        "必须是 SSE 内容类型：{first}"
    );
    assert!(
        !first.to_lowercase().contains("content-length"),
        "流式响应不得声明 Content-Length：{first}"
    );
    let first_body = body(&first);
    assert!(first_body.contains("id: 1"), "首帧带事件序列：{first_body}");
    assert!(
        first_body.contains("event: host.ready"),
        "事件类型应来自宿主发布：{first_body}"
    );
    assert!(
        first_body.contains("\"protocol\":\"bod-host/v1\"") || first_body.contains("bod-host/v1"),
        "事件内容应是真实就绪事实：{first_body}"
    );

    // 3) 带 Last-Event-ID 重连：只补增量，已确认的 1 不重发。
    let resumed_delta = request_with_header(&host, &key, "Last-Event-ID: 1\r\n", "?follow=0");
    assert!(
        !body(&resumed_delta).contains("id: 1"),
        "已确认的事件不得重发：{resumed_delta}"
    );
    assert!(
        body(&resumed_delta).contains(": heartbeat"),
        "追平后应有保活注释行：{resumed_delta}"
    );
    assert!(
        !body(&resumed_delta).contains("event: resync"),
        "游标在保留窗口内不该要求重订阅：{resumed_delta}"
    );

    // 4) 查询串游标同样生效（没有 Last-Event-ID 时使用）。
    let via_query = request(&host, Some(&key), "?cursor=1&follow=0");
    assert!(
        !body(&via_query).contains("id: 1"),
        "查询串游标也要生效：{via_query}"
    );

    // 5) 无法解析的游标必须要求整体重订阅，不能静默从最新开始。
    let broken = request_with_header(&host, &key, "Last-Event-ID: not-a-number\r\n", "?follow=0");
    let broken_body = body(&broken);
    assert!(
        broken_body.contains("event: resync"),
        "非法游标必须 resync：{broken_body}"
    );
    assert!(
        broken_body.contains("retry: "),
        "resync 应给出重连间隔：{broken_body}"
    );
    assert!(
        !broken_body.contains("id: "),
        "resync 帧不得带 id，否则客户端会误以为游标前进：{broken_body}"
    );

    // 6) 查询串里塞令牌一律拒绝：那种凭据会进日志与浏览器历史。
    let in_query = request(&host, Some(&key), "?token=sk-leak&follow=0");
    assert!(
        in_query.starts_with("HTTP/1.1 401"),
        "查询串令牌必须被拒：{in_query}"
    );
    assert!(in_query.contains("BOD_STREAM_DENIED"));

    // 7) follow=1 会保活到上限再关闭，客户端可凭游标继续；这里只验证能正常结束。
    let followed = request(&host, Some(&key), "?follow=1&max_ms=150&heartbeat_ms=30");
    assert!(followed.starts_with("HTTP/1.1 200"));
    assert!(
        body(&followed).contains(": heartbeat") || body(&followed).contains("id: 1"),
        "跟随模式应先给内容或保活：{followed}"
    );
}

fn request_with_header(host: &HostGuard, session_key: &str, header: &str, extra: &str) -> String {
    let descriptor: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&host.descriptor).expect("读描述文件"))
            .expect("描述文件是 JSON");
    let address = descriptor["address"].as_str().expect("描述含地址");

    let mut stream = TcpStream::connect(address).expect("连接宿主");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("设置读超时");
    let head = format!(
        "GET /_bod/events{extra} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {session_key}\r\n{header}Connection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).expect("写请求");

    let mut raw = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&raw).to_string()
}
