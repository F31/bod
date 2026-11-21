//! 路由表驱动的对外行为（CR-003）。
//!
//! 单测已经穷举了匹配与鉴权矩阵，这里验的是**它真的挂在线上**：真实子进程、
//! 真实 TCP、看线上字节的状态码与错误代码。
//!
//! 重点有两类：
//!
//! 1. **行为等价**：改造前是 `handle` 里的一条 `match (method, path)` 链，
//!    改造后是静态路由表 + 分发。四个既有端点的对外表现必须一字不差——
//!    包括「路径大小写敏感」「尾斜杠不静默规范化」「未知路径与错方法同答 404」。
//! 2. **停服机制换了载体**：改造前 `handle` 返回 `Ok(true)` 通知 `run` 退出，
//!    改造后由 handler 置原子标志、`run` 每轮检查。这条必须用真实进程退出验证，
//!    否则「请求成功但宿主不停」这种缺陷单测看不出来。

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
        "bod-router-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn start_host(tag: &str) -> HostProcess {
    let dir = temp_dir(tag);
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

fn descriptor_json(host: &HostProcess) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(&host.descriptor).expect("读描述文件"))
        .expect("描述文件是 JSON")
}

fn address_of(host: &HostProcess) -> String {
    descriptor_json(host)["address"]
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

/// 用描述文件里的握手令牌换会话密钥。这是客户端侧的常规路径，
/// 也顺带证明握手端点仍在路由表里正常工作。
fn handshake(host: &HostProcess, address: &str) -> String {
    let token = descriptor_json(host)["handshake_token"]
        .as_str()
        .expect("描述含握手令牌")
        .to_string();
    let body = serde_json::json!({ "handshake_token": token }).to_string();
    let response = raw_request(
        address,
        &format!(
            "POST /_bod/handshake HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "握手应成功：{response}"
    );
    let json: serde_json::Value =
        serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap_or(""))
            .expect("握手响应是 JSON");
    json["session_key"]
        .as_str()
        .expect("握手返回会话密钥")
        .to_string()
}

#[test]
fn health_is_public_and_returns_protocol() {
    let host = start_host("health");
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!("GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "实际：{response}");
    assert!(response.contains("bod-host/v1"), "应回协议名：{response}");
    // 公开路由也不能漏秘密：health 里不得出现会话密钥或握手令牌。
    let token = descriptor_json(&host)["handshake_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!response.contains(&token), "health 泄露握手令牌");
}

#[test]
fn query_string_still_routes_to_health() {
    let host = start_host("query");
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!(
            "GET /_bod/health?verbose=1 HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "查询串不应影响匹配：{response}"
    );
}

/// 改造前 `match` 用精确字符串比较，大小写不同即 404。改造后必须一致：
/// HTTP 路径区分大小写，静默放宽会让「路由存在性」变得不可预测。
#[test]
fn path_matching_is_case_sensitive() {
    let host = start_host("case");
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!("GET /_bod/HEALTH HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
    );
    assert!(response.starts_with("HTTP/1.1 404"), "实际：{response}");
    assert!(response.contains("BOD_NOT_FOUND"), "实际：{response}");
}

/// 尾斜杠不静默规范化：`/_bod/health/` 与 `/_bod/health` 是两条不同路径。
#[test]
fn trailing_slash_is_not_normalised() {
    let host = start_host("slash");
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!("GET /_bod/health/ HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
    );
    assert!(response.starts_with("HTTP/1.1 404"), "实际：{response}");
}

#[test]
fn unknown_path_is_404_with_stable_code() {
    let host = start_host("unknown");
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!("GET /_bod/nope HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
    );
    assert!(response.starts_with("HTTP/1.1 404"), "实际：{response}");
    assert!(response.contains("BOD_NOT_FOUND"), "实际：{response}");
}

/// 方法不匹配对外仍是 404，与改造前一致。
///
/// 内部已经能区分 `MethodMismatch`，但本次**刻意不改**为 405：
/// 改状态码属于对外行为变化，不该混在重构里做。这条断言锁住的是「没顺手改」。
#[test]
fn wrong_method_still_answers_404() {
    let host = start_host("method");
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!("DELETE /_bod/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
    );
    assert!(
        response.starts_with("HTTP/1.1 404"),
        "不应顺手改成 405：{response}"
    );
    assert!(response.contains("BOD_NOT_FOUND"), "实际：{response}");
}

/// shutdown 的鉴权由路由表声明（`Auth::Bearer`），拒绝码必须保持原样：
/// 客户端按 `BOD_SHUTDOWN_DENIED` 分支，改码等于改契约。
#[test]
fn shutdown_without_bearer_uses_declared_denial_code() {
    let host = start_host("denied");
    let address = address_of(&host);
    let response = raw_request(
        &address,
        &format!(
            "POST /_bod/shutdown HTTP/1.1\r\nHost: {address}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    assert!(response.starts_with("HTTP/1.1 401"), "实际：{response}");
    assert!(
        response.contains("BOD_SHUTDOWN_DENIED"),
        "拒绝码不得被统一成通用码：{response}"
    );
}

/// 停服机制换载体后的直接验证：请求成功后**进程要真的退出**。
#[test]
fn authorised_shutdown_actually_stops_the_process() {
    let mut host = start_host("stop");
    let address = address_of(&host);
    let key = handshake(&host, &address);

    let response = raw_request(
        &address,
        &format!(
            "POST /_bod/shutdown HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {key}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "实际：{response}");
    assert!(response.contains("stopping"), "实际：{response}");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match host.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => panic!("收到停止请求后 20 s 内未退出"),
            Err(err) => panic!("等待子进程失败：{err}"),
        }
    }
    // 描述文件随停服移除：残留的描述文件会让下一个客户端连到死地址。
    assert!(
        !host.descriptor.exists(),
        "停服后应移除描述文件：{}",
        host.descriptor.display()
    );
}
