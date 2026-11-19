//! bod 桌面壳的窗口本体（P0 任务 8.1）。
//!
//! 职责刻意很窄：**只有窗口与生命周期**。连接、握手、`close_action` 的决策全部在
//! `bod_desktop::connection` 里，这里只负责在启动时调一次、在关窗时调一次，
//! 因此窗口层没有第二套判断，也就不会出现"界面显示 A、实际做了 B"。
//!
//! 三条硬约束的落点：
//! - 不启动第二个业务 DB：本 crate 不依赖任何存储层，连接的是宿主进程；
//! - 桌面退出不终止外部宿主：关窗只调 `close_connection`，`detach` 时不发停止请求，
//!   本次启动的子进程也不在 Job Object 里（`StdHostSpawner` 不 kill-on-drop）；
//! - 启动密钥不进日志：`ConnectionLog` 只写 `session_key=<redacted>`，
//!   本文件不打印 `Attached::session_key`。

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bod_desktop::connection::{
    Attached, CloseAction, ConnectRequest, ConnectionLog, StdHostControl, StdHostProbe,
    StdHostSpawner, close_connection, connect_or_spawn,
};

/// 窗口生命周期里需要跨事件保留的状态。
struct ShellState {
    close_action: CloseAction,
    log: ConnectionLog,
    attached: Option<Attached>,
}

fn state() -> &'static Mutex<ShellState> {
    static STATE: OnceLock<Mutex<ShellState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(ShellState {
            close_action: close_action_from_env(),
            log: ConnectionLog::default(),
            attached: None,
        })
    })
}

/// `desktop.close_action` 的运行期取值；未知值回落到 `detach`（不停宿主）。
fn close_action_from_env() -> CloseAction {
    CloseAction::parse(&std::env::var("BOD_CLOSE_ACTION").unwrap_or_else(|_| "detach".to_string()))
}

/// 连接描述位置：`BOD_DESCRIPTOR` 优先，否则用宿主默认路径。
fn descriptor_path() -> PathBuf {
    match std::env::var_os("BOD_DESCRIPTOR") {
        Some(path) => PathBuf::from(path),
        None => bod_app::host_handshake::default_descriptor_path(),
    }
}

fn main() {
    connect_at_startup();
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![connection_status])
        .build(tauri::generate_context!())
        .expect("Tauri 应用构建失败");

    // 关窗事件与进程退出都收敛到同一个出口：`finish` 只认第一次调用，
    // 因此同一次退出不会既 detach 又 stop，也不会重复记录。
    app.run(|app_handle, event| match event {
        tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::CloseRequested { .. },
            ..
        } => {
            finish("close-requested");
            // 关窗=退出桌面：显式退出事件循环，避免"窗口关了进程还在"的半状态。
            // detach 的语义是"不杀宿主"，不是"不退出自己"。
            app_handle.exit(0);
        }
        tauri::RunEvent::Exit => finish("app-exit"),
        _ => {}
    });
}

/// 窗口可读的连接状态：只暴露可公开事实，会话密钥留在内存连接句柄里。
#[derive(serde::Serialize)]
struct ConnectionStatus {
    address: String,
    pid: u32,
    instance_id: String,
    spawned_by_desktop: bool,
    close_action: String,
}

#[tauri::command]
fn connection_status() -> Result<ConnectionStatus, String> {
    let guard = state().lock().map_err(|_| "状态锁不可用".to_string())?;
    let attached = guard
        .attached
        .as_ref()
        .ok_or_else(|| "尚未连接到宿主".to_string())?;
    Ok(ConnectionStatus {
        address: attached.descriptor.address.clone(),
        pid: attached.descriptor.pid,
        instance_id: attached.descriptor.instance_id.clone(),
        spawned_by_desktop: attached.spawned_by_desktop,
        close_action: guard.close_action.as_str().to_string(),
    })
}

/// 启动时只做一件事：连上已有宿主，没有可用的才启动一个。
fn connect_at_startup() {
    let descriptor = descriptor_path();
    let close_action = close_action_from_env();
    let mut log = ConnectionLog::default();
    let probe = StdHostProbe {
        timeout: Duration::from_millis(800),
    };
    let mut spawner = StdHostSpawner {
        bod_bin: StdHostSpawner::resolve_binary(),
        descriptor_path: descriptor.clone(),
        startup_timeout: Duration::from_secs(20),
    };
    let request = ConnectRequest {
        descriptor_path: &descriptor,
        close_action,
        startup_timeout: Duration::from_secs(20),
    };

    match connect_or_spawn(&request, &probe, &mut spawner, &mut log) {
        Ok(attached) => {
            println!(
                "desktop: connected address={} pid={} spawned_by_desktop={} close_action={}",
                attached.descriptor.address,
                attached.descriptor.pid,
                attached.spawned_by_desktop,
                close_action.as_str()
            );
            let mut guard = state().lock().expect("状态锁不可用");
            guard.attached = Some(attached);
            guard.log = log;
        }
        Err(err) => {
            // 连不上就如实显示失败，不假装已连接、也不在后台重试出第二个宿主。
            eprintln!("desktop: 连接宿主失败：{err}");
            let mut guard = state().lock().expect("状态锁不可用");
            guard.log = log;
        }
    }
    flush_stdout();
}

/// 关闭出口：按 `close_action` 决定是否停止宿主，并留下一条可核对的事实。
fn finish(reason: &str) {
    let mut guard = state().lock().expect("状态锁不可用");
    let ShellState {
        close_action,
        log,
        attached,
    } = &mut *guard;
    let close_action = *close_action;
    let Some(attached) = attached.take() else {
        println!("desktop: {reason}，没有活动连接");
        flush_stdout();
        return;
    };

    let mut control = StdHostControl {
        timeout: Duration::from_secs(2),
    };
    match close_connection(&attached, close_action, &mut control, log) {
        Ok(stopped) => println!(
            "desktop: {reason}，close_action={} stopped={stopped}",
            close_action.as_str()
        ),
        Err(err) => eprintln!("desktop: 关闭时停止宿主失败：{err}"),
    }
    println!("{}", log.rendered());
    flush_stdout();
}

/// 桌面壳输出可能被重定向到文件：显式 flush，避免"日志里看不到最后一跳"。
fn flush_stdout() {
    let _ = std::io::stdout().flush();
}
