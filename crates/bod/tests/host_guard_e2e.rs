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
    /// 子进程 stderr 落盘位置。
    ///
    /// 原来这里是 `Stdio::null()`：宿主若在写出描述文件之后、真正开始服务之前就退出，
    /// 测试只会看到「连接被拒绝（10061）」，而**退出原因被丢掉了**。
    /// 一次 CI 失败因此无法判断是端口、库挂载还是别的启动步骤——
    /// 现场不落盘，就只能靠猜，而猜出来的结论不该写进提交信息。
    stderr_path: PathBuf,
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
    let stderr_path = dir.join("host.stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("建 stderr 落盘文件");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bod"))
        .args(["serve", "--listen", "127.0.0.1:0", "--descriptor"])
        .arg(&descriptor)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("应能启动 bod serve");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if descriptor.exists() {
            return HostProcess {
                child,
                descriptor,
                dir,
                stderr_path,
            };
        }
        // 进程已经退出就别再等到 20 s：等下去只会把一个「启动失败」报成「启动慢」。
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "宿主在写出描述文件之前就退出了（{status}）；{}",
                diagnose(&dir, &stderr_path, &descriptor)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!(
        "宿主未在 20 s 内写出描述文件；{}",
        diagnose(&dir, &stderr_path, &descriptor)
    );
}

/// 失败现场：子进程退出状态、退出前的 stderr、描述文件内容。
///
/// 只在失败路径调用。把这三样一起带出来，是因为它们回答的是三个不同的问题：
/// 「进程还在吗」（退出状态）、「它说了什么」（stderr）、「文件是不是在说谎」（描述文件）。
fn diagnose(
    dir: &std::path::Path,
    stderr_path: &std::path::Path,
    descriptor: &std::path::Path,
) -> String {
    let stderr = std::fs::read_to_string(stderr_path).unwrap_or_else(|_| "（读不到）".to_string());
    let stderr = stderr.trim();
    let tail: String = if stderr.is_empty() {
        "（空）".to_string()
    } else {
        let lines: Vec<&str> = stderr.lines().collect();
        let start = lines.len().saturating_sub(20);
        lines[start..].join("\n")
    };
    let descriptor_text = match std::fs::read_to_string(descriptor) {
        Ok(text) => text.trim().to_string(),
        Err(err) => format!("（读不到：{err}）"),
    };
    format!(
        "会话目录 {}\n描述文件：{descriptor_text}\n子进程 stderr（尾部）：\n{tail}",
        dir.display()
    )
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

/// 发一个裸请求。连接失败时带上现场：**描述文件存在不等于服务在跑**。
fn raw_request(host: &mut HostProcess, request: &str) -> String {
    let address = address_of(host);
    let mut stream = TcpStream::connect(&address).unwrap_or_else(|err| {
        let status = match host.child.try_wait() {
            Ok(Some(status)) => format!("已退出：{status}"),
            Ok(None) => "仍在运行".to_string(),
            Err(err) => format!("取状态失败：{err}"),
        };
        panic!(
            "连接 {address} 失败：{err}（子进程 {status}）\n{}",
            diagnose(&host.dir, &host.stderr_path, &host.descriptor)
        );
    });
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
    let mut host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &mut host,
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
    let mut host = start_host();
    let address = address_of(&host);
    let port = address.rsplit_once(':').expect("地址含端口").1;
    let response = raw_request(
        &mut host,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

/// DNS rebinding：域名解析到 127.0.0.1，但 Host 头仍是攻击者域名。
#[test]
fn dns_rebinding_style_host_is_rejected() {
    let mut host = start_host();
    let response = raw_request(
        &mut host,
        "GET /_bod/health HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    );
    expect_rejected(&response, "BOD_HOST_NOT_ALLOWED");
}

#[test]
fn missing_host_is_rejected() {
    let mut host = start_host();
    let response = raw_request(
        &mut host,
        "GET /_bod/health HTTP/1.1\r\nConnection: close\r\n\r\n",
    );
    expect_rejected(&response, "BOD_HOST_MISSING");
}

/// 两个 Host 头，即便第一个合法也拒——「取第一个」正是走私利用的假设。
#[test]
fn duplicated_host_headers_are_rejected() {
    let mut host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &mut host,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nHost: evil.example\r\nConnection: close\r\n\r\n"
        ),
    );
    expect_rejected(&response, "BOD_HOST_DUPLICATED");
}

#[test]
fn origin_null_is_rejected() {
    let mut host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &mut host,
        &format!(
            "GET /_bod/health HTTP/1.1\r\nHost: {address}\r\nOrigin: null\r\nConnection: close\r\n\r\n"
        ),
    );
    expect_rejected(&response, "BOD_ORIGIN_NOT_ALLOWED");
}

#[test]
fn foreign_origin_is_rejected() {
    let mut host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &mut host,
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
    let mut host = start_host();
    let address = address_of(&host);
    for origin in ["tauri://localhost", "http://tauri.localhost"] {
        let response = raw_request(
            &mut host,
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
    let mut host = start_host();
    let response = raw_request(
        &mut host,
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
    let mut host = start_host();
    let address = address_of(&host);
    let response = raw_request(
        &mut host,
        &format!(
            "POST /_bod/shutdown HTTP/1.1\r\nHost: {address}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    assert!(response.starts_with("HTTP/1.1 401"), "应当 401：{response}");
    assert!(response.contains("BOD_SHUTDOWN_DENIED"));
}

/// 诊断本身不能因为「文件都不在」而再炸一次。
///
/// 它只在失败路径被调用，而失败现场恰恰可能什么都没有：进程还没写出描述文件、
/// stderr 文件没建成、临时目录已被清掉。一个在救火时抛 panic 的诊断函数
/// 等于没有诊断。
#[test]
fn diagnosis_survives_missing_artifacts() {
    let dir = temp_dir("diag");
    let missing = dir.join("host.stderr.log");
    let text = diagnose(&dir, &missing, &missing);
    assert!(text.contains("读不到"), "缺文件要如实说：{text}");

    std::fs::write(&missing, "第一行\n最后一行\n").expect("写 stderr");
    let text = diagnose(&dir, &missing, &missing);
    assert!(text.contains("最后一行"), "要带出 stderr 原文：{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
