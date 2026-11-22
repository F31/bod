//! 产品接口的真实装配（P8 7.6.2 / ADR-0007 第 6 条）。
//!
//! 这里验的不是 handler 的判定——那部分已由 `delivery_api` 的单测穷举——
//! 而是**装配真的发生在服务启动路径上**：
//!
//! 1. `serve --db <既有库>` 之后，不带任何自报头的请求能读到视图（200），
//!    在此之前它恒为 401；
//! 2. 主体来自宿主，所以**请求头改不动归属**：加一个 `X-Bod-Actor` 仍然看不到别人的跟踪项；
//! 3. 未装配（没有可挂载的库）时仍是一律 401，不回退成匿名主体；
//! 4. 显式指向不存在的库时启动失败，且不创建空库。
//!
//! 用真实子进程 + 真实 TCP：装配缺陷（例如 `with_delivery` 忘了被调用）
//! 在进程内测试里看不出来。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 本机主体：测试通过子进程环境给出，与真实部署读的是同一个来源。
const HOST_PRINCIPAL: &str = "e2e-host-user";

struct Host {
    child: Child,
    descriptor: PathBuf,
    dir: PathBuf,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.descriptor);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bod-delivery-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 用公开的写入原语造一份真实的库：没有直接写 SQL，走的是与命令层同一条路径。
fn seed_database(db: &Path) {
    use bod_app::config::BodConfig;
    use bod_app::coverage_receipt::SwitchState;
    use bod_app::delivery_tracking::{TargetRow, TrackingRow, TrackingState};
    use bod_app::store::SqliteStore;

    let mut store = SqliteStore::open(db, &BodConfig::default()).expect("建库应成功");
    let uow = store.transaction().expect("开事务");

    let tracking = |id: &str, owner: &str, title: &str| TrackingRow {
        tracking_id: id.to_string(),
        company_id: "company-default".to_string(),
        project_id: "project-1".to_string(),
        title: title.to_string(),
        requirement: "登录偶发 500".to_string(),
        owner_id: owner.to_string(),
        state: TrackingState::Open,
        issue_uid: None,
        run_uid: None,
        attempt_id: None,
        resource_version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let target = |tracking_id: &str, id: &str, customer: &str| TargetRow {
        target_id: id.to_string(),
        company_id: "company-default".to_string(),
        tracking_id: tracking_id.to_string(),
        customer_id: customer.to_string(),
        environment: "prod".to_string(),
        deployment_unit: String::new(),
        acceptance: "冒烟通过".to_string(),
        switch_state: SwitchState::Synced,
        population_known: true,
        created_at_ms: 1,
        updated_at_ms: 1,
    };

    // 本机主体是负责人：立场成立，整条视图可见。
    uow.insert_delivery_tracking(&tracking("dt-host", HOST_PRINCIPAL, "本机可见的交付"))
        .expect("写跟踪项");
    uow.insert_delivery_target(&target("dt-host", "t-acme", "acme"))
        .expect("写目标");
    uow.insert_delivery_target(&target("dt-host", "t-beta", "beta"))
        .expect("写目标");

    // 另一条属于别人：主体没换、id 换了，结果必须换。
    uow.insert_delivery_tracking(&tracking("dt-other", "someone-else", "别人的交付"))
        .expect("写跟踪项");
    uow.insert_delivery_target(&target("dt-other", "t-other", "acme"))
        .expect("写目标");

    uow.commit().expect("提交");
}

fn start_host_with(tag: &str, configure: impl FnOnce(&mut Command)) -> Host {
    let dir = temp_dir(tag);
    let descriptor = dir.join("host.endpoint.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_bod"));
    command
        .args(["serve", "--listen", "127.0.0.1:0", "--descriptor"])
        .arg(&descriptor);
    configure(&mut command);
    // 主体来自进程环境（真实部署取的是 OS 账户）。请求头不参与——这正是要验证的点。
    let mut child = command
        .env("USERNAME", HOST_PRINCIPAL)
        .env("USER", HOST_PRINCIPAL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("应能启动 bod serve");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if descriptor.exists() {
            return Host {
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

fn start_host(tag: &str, db: Option<&Path>) -> Host {
    start_host_with(tag, |command| {
        if let Some(db) = db {
            command.arg("--db").arg(db);
        }
    })
}

fn descriptor_json(host: &Host) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(&host.descriptor).expect("读描述文件"))
        .expect("描述文件是 JSON")
}

fn address_of(host: &Host) -> String {
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

fn session_key(host: &Host, address: &str) -> String {
    let token = descriptor_json(host)["handshake_token"]
        .as_str()
        .expect("描述含握手令牌")
        .to_string();
    let body = serde_json::json!({ "handshake_token": token }).to_string();
    let response = raw_request(
        address,
        &format!(
            "POST /_bod/handshake HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// 一次 GET，可附加额外的请求头（用来证明自报头无效）。
fn get_tracking(address: &str, key: &str, tracking_id: &str, extra_headers: &str) -> (u16, String) {
    let response = raw_request(
        address,
        &format!(
            "GET /api/v1/delivery-tracking/{tracking_id} HTTP/1.1\r\nHost: {address}\r\n\
             Authorization: Bearer {key}\r\n{extra_headers}Connection: close\r\n\r\n"
        ),
    );
    let status: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[test]
fn mounted_host_serves_the_view_and_ignores_self_reported_subject() {
    let dir = temp_dir("mounted");
    let db = dir.join("bod.db");
    seed_database(&db);

    let host = start_host("mounted-host", Some(&db));
    let address = address_of(&host);
    let key = session_key(&host, &address);

    // 1) 不带任何自报头：200，且两个目标都在（本机主体是负责人，容器整体可见）。
    let (status, body) = get_tracking(&address, &key, "dt-host", "");
    assert_eq!(status, 200, "装配完成后应为 200，实际响应：{body}");
    assert!(body.contains("\"tracking_id\":\"dt-host\""), "{body}");
    assert!(body.contains("\"customer_id\":\"acme\""), "{body}");
    assert!(body.contains("\"customer_id\":\"beta\""), "{body}");

    // 2) 换一个 id：主体没变，可见性必须变——装配没有把「能看」变成「都能看」。
    let (other_status, other_body) = get_tracking(&address, &key, "dt-other", "");
    assert_eq!(other_status, 404, "别人的跟踪项必须 404：{other_body}");
    assert!(other_body.contains("BOD_NOT_FOUND"), "{other_body}");

    // 3) 自报主体无效：头部说自己是 someone-else，仍然看不到 dt-other。
    //    这一条是 ADR-0007 第 6 条的对外证明——如果哪天有人加了 `X-Bod-Actor` 支持，
    //    这里会立刻变成 200。
    let (spoof_status, spoof_body) =
        get_tracking(&address, &key, "dt-other", "X-Bod-Actor: someone-else\r\n");
    assert_eq!(spoof_status, 404, "请求头不得改变主体归属：{spoof_body}");
}

#[test]
fn unmounted_product_api_still_denies_every_request() {
    // 没有 --db，且把默认库指到一个空目录：没有任何可挂载的库。
    let empty_home = temp_dir("unmounted-home");
    let host = start_host_with("unmounted", |command| {
        command.env("BOD_HOME", &empty_home);
    });
    let address = address_of(&host);
    let key = session_key(&host, &address);

    let (status, body) = get_tracking(&address, &key, "dt-host", "");
    assert_eq!(status, 401, "未装配时必须 401，实际：{body}");
    assert!(body.contains("BOD_API_DENIED"), "{body}");
    assert!(
        !empty_home.join("bod.db").exists(),
        "控制面不得替默认路径建库"
    );
    let _ = std::fs::remove_dir_all(&empty_home);
}

#[test]
fn explicit_database_that_does_not_exist_fails_startup_without_creating_it() {
    let dir = temp_dir("missing");
    let db = dir.join("absent.db");
    let descriptor = dir.join("host.endpoint.json");
    let output = Command::new(env!("CARGO_BIN_EXE_bod"))
        .args(["serve", "--listen", "127.0.0.1:0", "--descriptor"])
        .arg(&descriptor)
        .arg("--db")
        .arg(&db)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("应能运行 bod serve");
    assert!(!output.status.success(), "指向不存在的库必须启动失败");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--db 指向的数据库不存在"), "{stderr}");
    assert!(!db.exists(), "启动失败不得留下空库");
    assert!(!descriptor.exists(), "启动失败不得留下描述文件");
    let _ = std::fs::remove_dir_all(&dir);
}
