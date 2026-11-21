//! Host/Origin 校验的端到端验证（CR-007）。
//!
//! 单测已经穷举了判定矩阵，这里验的是**它真的挂在线上**：真实子进程、真实 TCP、
//! 看线上字节的状态码与错误代码。之所以要有这一层，是因为判定函数写对了但没接到
//! 请求入口，是这个项目里出现过的一类缺陷（逻辑有测试、装配没测试）。
//!
//! 另外验一个顺序问题：**Host 校验必须发生在鉴权之前**。无 Bearer 且 Host 非法时，
//! 答案是 403 而不是 401——否则攻击者能靠 401/403 的差异判断 Host 是否被接受。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct HostProcess {
    child: Child,
    descriptor: PathBuf,
    dir: PathBuf,
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.descriptor);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bod-hostguard-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn start_host() -> HostProcess {
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
            return HostProcess {
                child,
                descriptor,
                dir,
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("宿主未在 20 s 内写出描述文件");
}

fn address_of(host: &HostProcess) -> String {
    let descriptor: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&host.descriptor).expect("读描述文件"))
            .expect("描述文件是 JSON");
    descriptor["address"]
        .as_str()
        .expect("描述含地址")
        .to_string()
}

fn raw_request(address: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("连接宿主");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("设置读超时");
    stream.write_all(request.as_bytes()).expect("写请求");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

fn expect_rejected(response: &str, code: &str) {
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "应当 403，实际：{response}"
    );
    assert!(response.contains(code), "缺少错误代码 {code}：{response}");
}

#[test]
fn legitimate_loopback_host_is_served() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!("GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "回环 Host 应当放行：{response}"
    );
    assert!(response.contains("\"ok\":true"));
}

#[test]
fn localhost_alias_on_the_same_port_is_served() {
    let host = start_host();
    let address = address_of(&host);
    let port = address.rsplit_once(':').expect("地址含端口").1;
    let response = raw_request(
        &address,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

/// DNS rebinding：域名解析到 127.0.0.1，但 Host 头仍是攻击者域名。
#[test]
fn dns_rebinding_style_host_is_rejected() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        "GET /_bod/health HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    );
    expect_rejected(&response, "BOD_HOST_NOT_ALLOWED");
}

#[test]
fn missing_host_is_rejected() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        "GET /_bod/health HTTP/1.1\r\nConnection: close\r\n\r\n",
    );
    expect_rejected(&response, "BOD_HOST_MISSING");
}

/// 两个 Host 头，即便第一个合法也拒——「取第一个」正是走私利用的假设。
#[test]
fn duplicated_host_headers_are_rejected() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nHost: evil.example\r\nConnection: close\r\n\r\n"
        ),
    );
    expect_rejected(&response, "BOD_HOST_DUPLICATED");
}

#[test]
fn origin_null_is_rejected() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nOrigin: null\r\nConnection: close\r\n\r\n"
        ),
    );
    expect_rejected(&response, "BOD_ORIGIN_NOT_ALLOWED");
}

#[test]
fn foreign_origin_is_rejected() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nOrigin: https://evil.example\r\nConnection: close\r\n\r\n"
        ),
    );
    expect_rejected(&response, "BOD_ORIGIN_NOT_ALLOWED");
}

/// Tauri 两种来源在真实链路上放行——Windows 与 macOS/Linux 的差异不能变成
/// 「某个平台上悄悄被拒」。
#[test]
fn tauri_origin_is_served_on_the_wire() {
    let host = start_host();
    let address = address_of(&host);
    for origin in ["tauri://localhost", "http://tauri.localhost"] {
        let response = raw_request(
            &address,
            &format!(
                "GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "{origin} 应当放行：{response}"
        );
    }
}

/// 顺序断言：Host 非法时，即使没有 Bearer 也先答 403，不答 401。
#[test]
fn host_check_runs_before_authentication() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        "POST /_bod/shutdown HTTP/1.1\r\nHost: evil.example\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    expect_rejected(&response, "BOD_HOST_NOT_ALLOWED");
    assert!(
        !response.contains("BOD_SHUTDOWN_DENIED"),
        "不该走到鉴权分支：{response}"
    );
}

/// 反向保证：Host 合法但缺 Bearer 时仍是 401，说明校验没有把正常路径挡掉。
#[test]
fn valid_host_without_bearer_still_401() {
    let host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!(
            "POST /_bod/shutdown HTTP/1.1\r\nHost: {address}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    assert!(response.starts_with("HTTP/1.1 401"), "应当 401：{response}");
    assert!(response.contains("BOD_SHUTDOWN_DENIED"));
}
