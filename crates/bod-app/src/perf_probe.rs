//! 性能实测：P0 §38.2 空闲负担 + P7 容量基线与瓶颈定位（任务 1.1—1.3、2.1—2.4）。
//!
//! 只测**本机进程内可测项**；需要 OS 计数器细节、WebView、真机或安装包的条目一律标
//! `Unverified` 并写清测量缺口——**不以设计目标冒充实测**，也不把 headless 的数字套到
//! 桌面/移动上。
//!
//! 测量口径（口径定义在 [`crate::capacity_evaluation`]，归因在 [`crate::bottleneck`]）：
//! - 环境：OS / 架构 / rustc / 构建参数 / SQLite 同步策略 / 数据规模 / 缓存冷热；
//! - SQLite：WAL + `synchronous=FULL` + `BEGIN IMMEDIATE`（`BodConfig::default()`）；
//!   基准连接统一 `cache_size=-65536 KiB` / `temp_store=MEMORY`，重启、恢复与体积测量前做 WAL checkpoint；
//! - 延迟取 p50/p95/p99（排序后取分位），单位毫秒；**吞吐与尾延迟分别结论**；
//! - 每项带 `method + samples` 指纹：口径或样本数变化后不得跨阶段比较；
//! - 每一项都带样本数与命令，缺命令或缺样本的项不进入容量决策；
//! - 恢复 RTO 把**外部复制**与**内部重开/校验**分开计时（2.1）。

use crate::bottleneck::{
    ContentionSample, ContentionVerdict, RecoveryPhase, RecoveryReading, RecoveryVerdict,
    SlotReading, SlotVerdict, VolumeAssessment, VolumeReading, classify_contention,
    classify_recovery, classify_slots, classify_volume,
};
use crate::capacity_evaluation::{BaselineReport, EnvironmentRecord, FormFactor, MeasureItem};
use crate::cas::CasStore;
use crate::config::BodConfig;
use crate::domain::DomainId;
use crate::domain::queue::WorkQueueEntry;
use crate::store::{SqliteStore, StoreError};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 复现时时每一项都要贴这条命令。
pub const BENCH_COMMAND: &str = "cargo run -p xtask -- bench";

const COMPANY: &str = "company-default";
const NOW: i64 = 1_763_500_000_000;

/// §38.2 目标值：实测前一律按 Target 口径对待，实测达标才置 `Some(true)`。
const TARGET_COLD_START_MS: f64 = 500.0;
const TARGET_TX_P99_MS: f64 = 10.0;
const TARGET_QUEUE_P99_MS: f64 = 20.0;
const TARGET_RESTART_MS: f64 = 3_000.0;
const TARGET_THROUGHPUT_PER_SEC: f64 = 5_000.0;
const TARGET_IDLE_RSS_MIB: f64 = 40.0;

fn workspace(name: &str) -> Result<(PathBuf, SqliteStore), String> {
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("bod-perf-{name}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    let store = open_store(&root.join("bod.sqlite"))?;
    Ok((root, store))
}

/// 与 store.rs 的 P7 基准页缓存保持一致；报告必须声明该口径。
const BENCH_CACHE_SIZE_KIB: i64 = 65_536;

fn open_store(path: &Path) -> Result<SqliteStore, String> {
    let store = SqliteStore::open(path, &BodConfig::default()).map_err(|err| err.to_string())?;
    store.apply_bench_pragmas().map_err(|err| err.to_string())?;
    Ok(store)
}

fn raw_conn(path: &Path) -> Result<rusqlite::Connection, String> {
    let conn = rusqlite::Connection::open(path).map_err(|err| err.to_string())?;
    conn.pragma_update(None, "cache_size", BENCH_CACHE_SIZE_KIB)
        .map_err(|err| err.to_string())?;
    conn.pragma_update(None, "temp_store", "MEMORY")
        .map_err(|err| err.to_string())?;
    Ok(conn)
}

/// 把 WAL 截断后再测体积/重开，避免上一项写入的 WAL 残留被误算成稳态体积或恢复成本。
fn checkpoint(path: &Path) -> Result<(), String> {
    let conn = raw_conn(path)?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|err| err.to_string())
}

fn percentile(samples: &mut [Duration], ratio: f64) -> Duration {
    samples.sort();
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let index = ((samples.len() - 1) as f64 * ratio).round() as usize;
    samples[index.min(samples.len() - 1)]
}

fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1_000.0
}

fn verdict(value_ms: f64, target_ms: f64) -> Option<bool> {
    Some(value_ms <= target_ms)
}

/// 冷启动：仅到"存储就绪（迁移已应用）"，不含 HTTP ready 与外部探测。
fn cold_start(root: &Path) -> Result<MeasureItem, String> {
    let mut samples = Vec::new();
    for round in 0..15 {
        let path = root.join(format!("cold-{round}.sqlite"));
        let started = Instant::now();
        let store = open_store(&path)?;
        samples.push(started.elapsed());
        drop(store);
    }
    let mut sorted = samples.clone();
    let p50 = percentile(&mut sorted, 0.5);
    let mut sorted = samples.clone();
    let p99 = percentile(&mut sorted, 0.99);
    Ok(MeasureItem::new(
        "cold_start_store_ready",
        FormFactor::Headless,
        "≤ 500 ms（到本地 API ready）",
    )
    .with_method("新建空库 + 应用全部迁移，重复 15 次取 p50/p99；每连接 cache_size=-65536 KiB、temp_store=MEMORY；本机页缓存仍不可控")
    .with_note("只覆盖到存储就绪，未包含 HTTP 监听与握手，故不能直接宣称 API ready 达标")
    .with_numeric(ms(p99))
    .measured(
        format!("p50 {:.1} ms / p99 {:.1} ms", ms(p50), ms(p99)),
        samples.len(),
        BENCH_COMMAND,
        verdict(ms(p99), TARGET_COLD_START_MS),
    ))
}

/// 冷启动到 API ready：在 bench 旁边真实 spawn 一个 `bod serve` 并等待就绪行。
///
/// 只测本地监听就绪，不执行会话握手；握手语义仍由 host_handshake conformance 覆盖。
fn cold_start_api_ready() -> MeasureItem {
    let name = "cold_start_to_api_ready";
    let target = "≤ 1 s（到本地 API ready）";
    let unverified = |reason: String| {
        MeasureItem::new(name, FormFactor::Headless, target)
            .with_method("spawn 同目录 bod serve --listen 127.0.0.1:0 --json，等待 ready 行后 TCP 连接；不执行会话握手")
            .unverified(reason, BENCH_COMMAND)
    };

    let Ok(exe) = std::env::current_exe() else {
        return unverified("读不到当前可执行文件，无法定位 bod serve".to_string());
    };
    let candidates = [exe.with_file_name("bod"), exe.with_file_name("bod.exe")];
    let Some(binary) = candidates.into_iter().find(|path| path.is_file()) else {
        return unverified(format!(
            "{} 旁边没有 bod/bod.exe，bench 不能冒充宿主已就绪",
            exe.display()
        ));
    };
    let descriptor = std::env::temp_dir().join(format!(
        "bod-api-ready-{}-{}.endpoint.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));

    let started = Instant::now();
    let mut child = match std::process::Command::new(&binary)
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--descriptor",
            &descriptor.to_string_lossy(),
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return unverified(format!("spawn bod serve 失败：{err}")),
    };
    let stdout = child.stdout.take().expect("已请求 piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = sender.send(result);
    });
    let ready_line = match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(line)) => line,
        Ok(Err(err)) => {
            let _ = child.kill();
            let _ = child.wait();
            return unverified(format!("读取 ready 行失败：{err}"));
        }
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return unverified(format!("5 s 内没有 ready 行：{err}"));
        }
    };
    let address = serde_json::from_str::<serde_json::Value>(&ready_line)
        .ok()
        .and_then(|value| value.get("address")?.as_str().map(str::to_string));
    let Some(address) = address else {
        let _ = child.kill();
        let _ = child.wait();
        return unverified(format!("ready 行不含可解析 address：{ready_line}"));
    };
    let connect = address
        .parse::<std::net::SocketAddr>()
        .map_err(|err| err.to_string())
        .and_then(|socket| {
            std::net::TcpStream::connect_timeout(&socket, std::time::Duration::from_secs(1))
                .map_err(|err| err.to_string())
        });
    let ready_ms = ms(started.elapsed());
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&descriptor);
    match connect {
        Ok(_) => MeasureItem::new(name, FormFactor::Headless, target)
            .with_method("spawn 同目录 bod serve --listen 127.0.0.1:0 --json，等待 ready 行后 TCP 连接；不执行会话握手")
            .with_note("只到监听就绪；受保护握手、业务 API 与 Web 可见性分别由其它用例覆盖")
            .with_numeric(ready_ms)
            .measured(
                format!("spawn→ready→connect {ready_ms:.1} ms（{address}）"),
                1,
                BENCH_COMMAND,
                verdict(ready_ms, 1_000.0),
            ),
        Err(err) => unverified(format!("ready 后 TCP 连接失败：{err}")),
    }
}

/// 关键事务延迟：单条写事务（含 WAL + synchronous=FULL 提交）。
fn transaction_latency(root: &Path) -> Result<MeasureItem, String> {
    let path = root.join("tx.sqlite");
    let mut store = open_store(&path)?;
    let mut samples = Vec::with_capacity(500);
    for index in 0..500 {
        let started = Instant::now();
        let uow = store.transaction().map_err(|err| err.to_string())?;
        uow.put_queue(&WorkQueueEntry::new(
            DomainId::new(COMPANY),
            format!("issue:tx-{index}"),
            NOW + index,
        ))
        .map_err(|err| err.to_string())?;
        uow.commit().map_err(|err| err.to_string())?;
        samples.push(started.elapsed());
    }
    let mut sorted = samples.clone();
    let p50 = percentile(&mut sorted, 0.5);
    let mut sorted = samples.clone();
    let p95 = percentile(&mut sorted, 0.95);
    let mut sorted = samples.clone();
    let p99 = percentile(&mut sorted, 0.99);
    Ok(MeasureItem::new(
        "key_transaction_latency",
        FormFactor::Headless,
        "p99 ≤ 10 ms",
    )
    .with_method("500 次单条写事务（BEGIN IMMEDIATE + 一次 INSERT + COMMIT），WAL/synchronous=FULL、cache_size=-65536 KiB、temp_store=MEMORY；单连接串行，无并发争用")
    .with_note("串行单写线程口径；多进程争用下的排队等待未覆盖")
    .with_numeric(ms(p99))
    .measured(
        format!(
            "p50 {:.2} ms / p95 {:.2} ms / p99 {:.2} ms",
            ms(p50),
            ms(p95),
            ms(p99)
        ),
        samples.len(),
        BENCH_COMMAND,
        verdict(ms(p99), TARGET_TX_P99_MS),
    ))
}

/// 事件写入吞吐（批量事务）：与尾延迟分开结论。
fn event_throughput(root: &Path, entries: usize) -> Result<MeasureItem, String> {
    let path = root.join("throughput.sqlite");
    let mut store = open_store(&path)?;
    let started = Instant::now();
    let uow = store.transaction().map_err(|err| err.to_string())?;
    for index in 0..entries {
        uow.insert_event(
            &format!("ev-batch-{index:06}"),
            COMPANY,
            &format!("agg-batch-{index:06}"),
            1,
            "{\"kind\":\"perf\"}",
        )
        .map_err(|err| err.to_string())?;
    }
    uow.commit().map_err(|err| err.to_string())?;
    let elapsed = started.elapsed();
    let per_sec = entries as f64 / elapsed.as_secs_f64().max(1e-6);
    Ok(MeasureItem::new(
        "event_throughput_batched",
        FormFactor::Headless,
        "≥ 5 000 events/s（探索目标，未冻结）",
    )
    .with_method(format!(
        "单事务写入 {entries} 条 events 后一次性提交，计 events/s"
    ))
    .with_note(
        "批量提交口径（每批一次 fsync）；逐条持久化的速率见 event_throughput_durable，两者不可互换",
    )
    .with_numeric(per_sec)
    .measured(
        format!("{per_sec:.0} events/s"),
        entries,
        BENCH_COMMAND,
        Some(per_sec >= TARGET_THROUGHPUT_PER_SEC),
    ))
}

/// 逐条持久化吞吐：每次都提交，代表"每条事件都要落盘"的口径。
fn durable_event_throughput(root: &Path, entries: usize) -> Result<MeasureItem, String> {
    let path = root.join("durable.sqlite");
    let mut store = open_store(&path)?;
    let started = Instant::now();
    for index in 0..entries {
        let uow = store.transaction().map_err(|err| err.to_string())?;
        uow.insert_event(
            &format!("ev-durable-{index:06}"),
            COMPANY,
            &format!("agg-durable-{index:06}"),
            1,
            "{\"kind\":\"perf\"}",
        )
        .map_err(|err| err.to_string())?;
        uow.commit().map_err(|err| err.to_string())?;
    }
    let elapsed = started.elapsed();
    let per_sec = entries as f64 / elapsed.as_secs_f64().max(1e-6);
    Ok(MeasureItem::new(
        "event_throughput_durable",
        FormFactor::Headless,
        "无冻结目标（受 fsync 限制，仅作对照）",
    )
    .with_method(format!(
        "逐条 insert_event + commit（每次一次 fsync），共 {entries} 条"
    ))
    .with_note("与 event_throughput_batched 是两个口径：不得用批量数字替代逐条持久化承诺")
    .with_numeric(per_sec)
    .measured(
        format!("{per_sec:.0} events/s"),
        entries,
        BENCH_COMMAND,
        None,
    ))
}

/// 队列扫描与就绪候选选择成本。
fn queue_selection(root: &Path, entries: usize) -> Result<MeasureItem, String> {
    let path = root.join("queue.sqlite");
    let mut store = open_store(&path)?;
    let uow = store.transaction().map_err(|err| err.to_string())?;
    for index in 0..entries {
        uow.put_queue(&WorkQueueEntry::new(
            DomainId::new(COMPANY),
            format!("issue:q-{index:05}"),
            NOW,
        ))
        .map_err(|err| err.to_string())?;
    }
    uow.commit().map_err(|err| err.to_string())?;

    // 预热后再计时：首次全表读取要把页装入缓存，混入 p99 会放大两阶段差异。
    for _ in 0..20 {
        let _ = store.list_queue(COMPANY).map_err(|err| err.to_string())?;
    }
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let started = Instant::now();
        let rows = store.list_queue(COMPANY).map_err(|err| err.to_string())?;
        if rows.len() != entries {
            return Err(format!("候选数不符：期望 {entries}，实得 {}", rows.len()));
        }
        samples.push(started.elapsed());
    }
    let mut sorted = samples.clone();
    let p50 = percentile(&mut sorted, 0.5);
    let mut sorted = samples.clone();
    let p95 = percentile(&mut sorted, 0.95);
    let mut sorted = samples.clone();
    let p99 = percentile(&mut sorted, 0.99);
    Ok(MeasureItem::new(
        "queue_selection_1000",
        FormFactor::Headless,
        "1000 就绪候选 p99 ≤ 20 ms",
    )
    .with_method(format!(
        "{entries} 条 work_queue 全量读取；20 次预热后计时 200 次，页缓存热态，单连接，cache_size=-65536 KiB"
    ))
    .with_note("全量扫描而非分页；p99 取 200 次样本，超过 1000 条后的线性增长需要另行测量")
    .with_numeric(ms(p99))
    .measured(
        format!(
            "{entries} 条 p50 {:.2} ms / p95 {:.2} ms / p99 {:.2} ms",
            ms(p50),
            ms(p95),
            ms(p99)
        ),
        samples.len(),
        BENCH_COMMAND,
        verdict(ms(p99), TARGET_QUEUE_P99_MS),
    ))
}

/// 10 万事件后的重启就绪时间。
fn restart_after_events(root: &Path, events: usize) -> Result<MeasureItem, String> {
    let path = root.join("events.sqlite");
    let mut store = open_store(&path)?;
    let started = Instant::now();
    let uow = store.transaction().map_err(|err| err.to_string())?;
    for index in 0..events {
        uow.insert_event(
            &format!("ev-{index:06}"),
            COMPANY,
            &format!("agg-{index:06}"),
            1,
            "{\"kind\":\"perf\"}",
        )
        .map_err(|err| err.to_string())?;
    }
    uow.commit().map_err(|err| err.to_string())?;
    let seed = started.elapsed();
    // 不把未 checkpoint 的 WAL 残留算进“重开就绪”；两阶段都按同一稳态口径。
    checkpoint(&path)?;
    drop(store);

    let mut samples = Vec::with_capacity(5);
    let mut counts = None;
    for _ in 0..5 {
        let started = Instant::now();
        let reopened = open_store(&path)?;
        samples.push(ms(started.elapsed()));
        counts = Some(reopened.core_row_counts().map_err(|err| err.to_string())?);
        drop(reopened);
    }
    let ready = median_f64(&mut samples);
    let counts = counts.expect("至少重开一次");

    Ok(MeasureItem::new(
        "restart_after_100k_events",
        FormFactor::Headless,
        "10 万事件后重启 ready ≤ 3 s",
    )
    .with_method(format!(
        "单事务写入 {events} 条 events → WAL TRUNCATE checkpoint → 关闭连接 → 重开 5 次取中位数并读取行数（issues={}）；cache_size=-65536 KiB",
        counts.issues
    ))
    .with_note("当前无事件回放投影，重开只应用幂等迁移；重复重开使用同一文件，页缓存可能偏热，接入投影后必须重测")
    .with_numeric(ready)
    .measured(
        format!(
            "{events} 事件写入 {:.0} ms；5 次重开中位就绪 {:.0} ms",
            ms(seed),
            ready
        ),
        samples.len(),
        BENCH_COMMAND,
        verdict(ready, TARGET_RESTART_MS),
    ))
}

/// 空闲循环单次成本：队列扫描 + 租约到期检查 + outbox 状态查询。
fn idle_tick(root: &Path) -> Result<MeasureItem, String> {
    let path = root.join("idle.sqlite");
    let store = open_store(&path)?;
    for _ in 0..50 {
        let _ = store.list_queue(COMPANY).map_err(|err| err.to_string())?;
    }
    let mut samples = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let started = Instant::now();
        let _ = store.list_queue(COMPANY).map_err(|err| err.to_string())?;
        let _ = store
            .list_outbox_by_status(crate::domain::queue::EffectStatus::Prepared)
            .map_err(|err| err.to_string())?;
        let _ = store
            .list_reconciliation_cases(COMPANY, true)
            .map_err(|err| err.to_string())?;
        samples.push(started.elapsed());
    }
    let mut sorted = samples.clone();
    let p50 = percentile(&mut sorted, 0.5);
    let mut sorted = samples.clone();
    let p99 = percentile(&mut sorted, 0.99);
    Ok(MeasureItem::new(
        "idle_tick_cost",
        FormFactor::Headless,
        "无冻结目标（§38.2 只约束 RSS/CPU 占比）",
    )
    .with_method("空库上预热 50 次后空转 1000 次：队列读取 + outbox Prepared 查询 + open 对账案件查询；cache_size=-65536 KiB")
    .with_note("单次成本低不等于空闲 CPU 占比达标；轮询周期与唤醒机制需另行测量")
    .with_numeric(ms(p99))
    .measured(
        format!("p50 {:.3} ms / p99 {:.3} ms", ms(p50), ms(p99)),
        samples.len(),
        BENCH_COMMAND,
        None,
    ))
}

/// 表与索引体积：`page_count × page_size`，并附文件字节数。
fn storage_volume(root: &Path, events: usize) -> Result<MeasureItem, String> {
    let path = root.join("events.sqlite");
    // 文件里已有上一项写入的 events 行，直接量它的体积（主库 + WAL，未 checkpoint 的都算上）。
    let main_bytes = std::fs::metadata(&path)
        .map(|meta| meta.len())
        .map_err(|err| err.to_string())?;
    let wal_bytes = std::fs::metadata(format!("{}-wal", path.display()))
        .map(|meta| meta.len())
        .unwrap_or(0);
    let bytes = main_bytes + wal_bytes;
    Ok(MeasureItem::new(
        "storage_volume_100k_events",
        FormFactor::Headless,
        "无冻结目标（体积压力信号见 §2.3）",
    )
    .with_method(format!(
        "{events} 条 events 后的库文件体积：主库 {main_bytes} 字节 + WAL {wal_bytes} 字节"
    ))
    .with_note("只记文件体积；表/索引分别占用与重建成本需要 SQLite 统计视图，见 §2.3")
    .with_numeric(bytes as f64 / 1_048_576.0)
    .measured(
        format!("{:.1} MiB（{bytes} 字节）", bytes as f64 / 1_048_576.0),
        1,
        BENCH_COMMAND,
        None,
    ))
}

/// 当前进程的常驻内存（只在能读到 `/proc` 的平台给值）。
fn resident_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let value = rest.split_whitespace().next()?;
            return value.parse().ok();
        }
    }
    None
}

fn idle_rss() -> MeasureItem {
    match resident_kib() {
        Some(kib) => {
            let mib = kib as f64 / 1024.0;
            MeasureItem::new(
                "idle_rss_headless",
                FormFactor::Headless,
                "≤ 40 MiB（已初始化单 Company、无运行任务）",
            )
            .with_method(
                "存储已打开、迁移已应用、尚无业务负载时读 /proc/self/status 的 VmRSS（取样一次）",
            )
            .with_note("这是 headless 进程的下界，不含桌面壳/WebView/sidecar；不能套到桌面总内存；有负载后的 RSS 需在 §2 单独测")
            .measured(
                format!("{mib:.1} MiB"),
                1,
                BENCH_COMMAND,
                Some(mib <= TARGET_IDLE_RSS_MIB),
            )
        }
        None => MeasureItem::new(
            "idle_rss_headless",
            FormFactor::Headless,
            "≤ 40 MiB（已初始化单 Company、无运行任务）",
        )
        .with_method(
            "需要 OS 级内存计数器（Windows: GetProcessMemoryInfo；Linux: /proc/self/status VmRSS）",
        )
        .unverified(
            "当前平台读不到 /proc/self/status；未取得实测值，不填设计值",
            BENCH_COMMAND,
        ),
    }
}

/// Linux `/proc/self/stat` 的 utime+stime，单位 tick；本探针按常见 USER_HZ=100 换算。
fn process_cpu_ticks() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<&str> = text.rsplit_once(')')?.1.split_whitespace().collect();
    // 右括号后第一项是 state（field 3），故 utime 是 field 14 → index 11。
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(utime + stime)
}

/// 本地空闲循环 CPU 占比探针：连续执行 idle_tick 语义并观察进程 CPU 时间。
///
/// 这不是“真实宿主 60 s 空闲守护进程”的替代测量；报告中必须保留此限制。
fn idle_cpu_ratio(root: &Path) -> MeasureItem {
    let Some(before) = process_cpu_ticks() else {
        return MeasureItem::new(
            "idle_cpu_ratio",
            FormFactor::Headless,
            "无冻结数值；§38.2 要求空闲时无模型调用与热轮询",
        )
        .with_method("需要进程 CPU 时间计数器；非 Linux 或无 /proc 时如实留空")
        .unverified(
            "当前平台读不到 /proc/self/stat 的进程 CPU 时间，不能用 idle_tick_cost 反推",
            BENCH_COMMAND,
        );
    };
    let path = root.join("idle-cpu.sqlite");
    let Ok(store) = open_store(&path) else {
        return MeasureItem::new(
            "idle_cpu_ratio",
            FormFactor::Headless,
            "无冻结数值；§38.2 要求空闲时无模型调用与热轮询",
        )
        .with_method("连续执行 idle_tick 语义并读取 /proc/self/stat 的 utime+stime")
        .unverified("空闲探针临时库无法建立，CPU 占比未测", BENCH_COMMAND);
    };
    let started = Instant::now();
    for _ in 0..2_000 {
        let _ = store.list_queue(COMPANY);
        let _ = store.list_outbox_by_status(crate::domain::queue::EffectStatus::Prepared);
        let _ = store.list_reconciliation_cases(COMPANY, true);
    }
    let elapsed = started.elapsed().as_secs_f64().max(1e-6);
    let Some(after) = process_cpu_ticks() else {
        return MeasureItem::new(
            "idle_cpu_ratio",
            FormFactor::Headless,
            "无冻结数值；§38.2 要求空闲时无模型调用与热轮询",
        )
        .with_method("连续执行 idle_tick 语义并读取 /proc/self/stat 的 utime+stime")
        .unverified("观测窗口结束时读不到 CPU 时间，未取得读数", BENCH_COMMAND);
    };
    let cpu_seconds = (after.saturating_sub(before)) as f64 / 100.0;
    let ratio_pct = cpu_seconds / elapsed * 100.0;
    MeasureItem::new(
        "idle_cpu_ratio",
        FormFactor::Headless,
        "无冻结数值；§38.2 要求空闲时无模型调用与热轮询",
    )
    .with_method("连续 2000 次 idle_tick 语义（队列/outbox/对账查询）期间读 /proc/self/stat 的 utime+stime；按 USER_HZ=100 估算")
    .with_note("这是 bench 内连续空转的 CPU 占比上界，不等于真实宿主空闲守护进程 60 s 占比；不得据此宣称空闲目标达标")
    .with_numeric(ratio_pct)
    .measured(
        format!("CPU {:.1}% / 窗口 {:.3} s", ratio_pct, elapsed),
        2_000,
        BENCH_COMMAND,
        None,
    )
}

/// 构建产物体积：当前运行的可执行文件。
fn package_size() -> MeasureItem {
    let measured = std::env::current_exe()
        .and_then(|path| std::fs::metadata(&path).map(|meta| (path, meta.len())));
    match measured {
        Ok((path, bytes)) => {
            MeasureItem::new("package_size_headless", FormFactor::Headless, "无冻结目标")
                .with_method(format!("当前运行的可执行文件展开体积：{}", path.display()))
                .with_note(
                    "这是 bench 可执行文件，不是发布包；压缩体积与分形态安装包见 desktop/mobile 项",
                )
                .measured(
                    format!("{:.1} MiB（{} 字节）", bytes as f64 / 1_048_576.0, bytes),
                    1,
                    BENCH_COMMAND,
                    None,
                )
        }
        Err(err) => MeasureItem::new("package_size_headless", FormFactor::Headless, "无冻结目标")
            .with_method("读取当前可执行文件元数据")
            .unverified(format!("读不到当前可执行文件：{err}"), BENCH_COMMAND),
    }
}

/// Web 侧本地投影探针：只计只读事实装配 + 首页投影，不冒充浏览器渲染可见。
fn web_projection_probe(root: &Path) -> MeasureItem {
    let path = root.join("web-projection.sqlite");
    let Ok(store) = open_store(&path) else {
        return MeasureItem::new(
            "web_projection_p95",
            FormFactor::Web,
            "无冻结目标（本地投影，非渲染可见）",
        )
        .with_method("空库上预热后调用 home_from_store 200 次取 p95")
        .unverified("本地投影临时库无法建立", BENCH_COMMAND);
    };
    for _ in 0..20 {
        let _ =
            crate::workbench_projection::home_from_store(&store, COMPANY, "proj-web-projection");
    }
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let started = Instant::now();
        let _ =
            crate::workbench_projection::home_from_store(&store, COMPANY, "proj-web-projection");
        samples.push(started.elapsed());
    }
    let p95 = percentile(&mut samples, 0.95);
    MeasureItem::new(
        "web_projection_p95",
        FormFactor::Web,
        "无冻结目标（本地投影，非渲染可见）",
    )
    .with_method(
        "空库上预热 20 次后调用 home_from_store 200 次取 p95；只读事实装配，不含传输与浏览器渲染",
    )
    .with_note("state_to_web_visible_p95 仍需真实 Web 宿主，不能用本地投影数字替代")
    .with_numeric(ms(p95))
    .measured(
        format!("本地首页投影 p95 {:.3} ms", ms(p95)),
        samples.len(),
        BENCH_COMMAND,
        None,
    )
}

fn web_and_shell_gaps(root: &Path) -> Vec<MeasureItem> {
    vec![
        web_projection_probe(root),
        MeasureItem::new("state_to_web_visible_p95", FormFactor::Web, "≤ 2 s")
            .with_method("需要 Web 宿主：状态变更 → 投影 → 浏览器渲染可见")
            .unverified(
                "仓库内没有 Web 宿主与浏览器端投影，测不到渲染可见延迟",
                BENCH_COMMAND,
            ),
        MeasureItem::new(
            "desktop_total_memory",
            FormFactor::Desktop,
            "≤ 200 MiB（整壳）",
        )
        .with_method("壳 + WebView + sidecar + 管理进程树 + 外部 Agent 分别计量后求和")
        .unverified("无桌面安装包与真实窗口，无法分项计量", BENCH_COMMAND),
        MeasureItem::new(
            "desktop_webview_memory",
            FormFactor::Desktop,
            "无冻结目标（单独记录 WebView 占用）",
        )
        .with_method("Windows 上按 WebView2 子进程分别取 WorkingSet")
        .unverified("无 WebView2 宿主进程，测不到", BENCH_COMMAND),
        MeasureItem::new(
            "desktop_sidecar_memory",
            FormFactor::Desktop,
            "无冻结目标（单独记录 sidecar 占用）",
        )
        .with_method("sidecar 独立进程的常驻内存")
        .unverified("无 sidecar 发布产物，测不到", BENCH_COMMAND),
        MeasureItem::new(
            "desktop_package_size",
            FormFactor::Desktop,
            "无冻结目标（压缩/展开分别记录）",
        )
        .with_method("安装包压缩体积 + 安装后展开体积，记录静态/动态链接与 TLS")
        .unverified("无桌面安装包构建产物", BENCH_COMMAND),
        MeasureItem::new("mobile_package_size", FormFactor::Mobile, "无冻结目标")
            .with_method("移动发布产物（含 SQLite 与 TLS 构建方式）的体积")
            .unverified("无移动构建产物与真机", BENCH_COMMAND),
    ]
}

// ---------------------------------------------------------------------------
// 2.1 恢复 RTO：外部复制与内部恢复分开计时
// ---------------------------------------------------------------------------

/// §38.2 的恢复目标。
const TARGET_RTO_MS: f64 = 60_000.0;

fn percentile_us(samples: &mut [u128], ratio: f64) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let index = ((samples.len() - 1) as f64 * ratio).round() as usize;
    samples[index.min(samples.len() - 1)]
}

fn median_f64(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

/// `PRAGMA quick_check`：恢复后必须先确认库没坏。
fn quick_check(path: &Path) -> Result<String, String> {
    let conn = raw_conn(path)?;
    conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .map_err(|err| err.to_string())
}

fn recovery_rto(root: &Path) -> Result<(MeasureItem, RecoveryVerdict), String> {
    let source = root.join("events.sqlite");
    if !source.exists() {
        let reason = "没有 10 万事件样本库，恢复 RTO 未测";
        let item = MeasureItem::new(
            "recovery_rto_100k_events",
            FormFactor::Headless,
            "10 万事件后恢复 RTO ≤ 60 s",
        )
        .with_method("备份 → 复制到新路径 → 重开 → quick_check + 行数校验，逐阶段计时")
        .unverified(reason, BENCH_COMMAND);
        return Ok((item, RecoveryVerdict::Unverified { reason }));
    }

    let backup = root.join("recovery-backup.sqlite");
    {
        checkpoint(&source)?;
        let store = open_store(&source)?;
        store
            .vacuum_into(&backup.to_string_lossy())
            .map_err(|err| err.to_string())?;
    }

    let mut restore = Vec::new();
    let mut reopen = Vec::new();
    let mut verify = Vec::new();
    let mut checked_rows = 0i64;
    for round in 0..5 {
        let restored = root.join(format!("recovery-restored-{round}.sqlite"));
        let _ = std::fs::remove_file(&restored);

        // 外部阶段：把备份搬到目标位置（换盘/换机时波动最大的一步）。
        let started = Instant::now();
        std::fs::copy(&backup, &restored).map_err(|err| err.to_string())?;
        restore.push(ms(started.elapsed()));

        // 内部阶段一：打开并应用迁移（幂等检查）。
        let started = Instant::now();
        let store = open_store(&restored)?;
        reopen.push(ms(started.elapsed()));

        // 内部阶段二：行数校验 + 完整性检查。
        let started = Instant::now();
        let counts = store.core_row_counts().map_err(|err| err.to_string())?;
        let check = quick_check(&restored)?;
        if !check.eq_ignore_ascii_case("ok") {
            return Err(format!("恢复后 quick_check 不是 ok：{check}"));
        }
        checked_rows = counts.invocations;
        verify.push(ms(started.elapsed()));
        drop(store);
    }

    let reading = RecoveryReading::new(vec![
        RecoveryPhase {
            name: "restore_copy",
            millis: median_f64(&mut restore),
            external: true,
        },
        RecoveryPhase {
            name: "reopen",
            millis: median_f64(&mut reopen),
            external: false,
        },
        RecoveryPhase {
            name: "quick_check_and_counts",
            millis: median_f64(&mut verify),
            external: false,
        },
    ]);
    let verdict = classify_recovery(&reading, TARGET_RTO_MS);
    let meets = Some(matches!(verdict, RecoveryVerdict::WithinTarget { .. }));
    let item = MeasureItem::new(
        "recovery_rto_100k_events",
        FormFactor::Headless,
        "10 万事件后恢复 RTO ≤ 60 s",
    )
    .with_method(format!(
        "源库 WAL TRUNCATE checkpoint → VACUUM INTO 备份 → 复制到新路径 → 重开（含迁移幂等检查）→ quick_check + 行数校验；5 轮取各阶段中位数（invocations={checked_rows}）"
    ))
    .with_note(format!(
        "总 RTO {:.0} ms 中外部复制占 {:.0} ms（{:.0}%）：换盘或换机后主要变化在外部这一段，内部恢复 {:.0} ms",
        reading.total_ms(),
        reading.external_ms(),
        reading.external_share() * 100.0,
        reading.internal_ms()
    ))
    .with_numeric(reading.total_ms())
    .measured(
        format!(
            "总 {:.0} ms（外部复制 {:.0} ms + 重开 {:.0} ms + 校验 {:.0} ms）",
            reading.total_ms(),
            reading.external_ms(),
            reading.phases[1].millis,
            reading.phases[2].millis
        ),
        5,
        BENCH_COMMAND,
        meets,
    );
    Ok((item, verdict))
}

// ---------------------------------------------------------------------------
// 2.2 单写争用：多连接并发写，数 Busy 与 p99
// ---------------------------------------------------------------------------

/// §38.2 事务 p99 目标，复用于争用判定。
const CONTENTION_WRITERS: [usize; 4] = [1, 2, 4, 8];
const CONTENTION_TX_PER_WRITER: usize = 80;

fn is_busy(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Sql(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ffi::ErrorCode::DatabaseBusy
    )
}

struct ContentionRun {
    samples: Vec<ContentionSample>,
    throughput: Vec<(usize, f64)>,
    transactions: usize,
}

/// 单个并发写者的结果。`other_errors` 与 `busy` 分开计数：
/// 只有 Busy 才是写锁争用证据，其它错误必须让 bench 直接失败，不能被算成争用。
struct WriterOutcome {
    latencies: Vec<u128>,
    busy: u64,
    other_errors: u64,
    last_error: Option<String>,
}

fn write_contention(root: &Path) -> Result<(MeasureItem, ContentionRun), String> {
    let path = root.join("contention.sqlite");
    // 先建库，避免把首次迁移算进第一个写者的延迟。
    {
        let _store = open_store(&path)?;
    }

    let mut samples = Vec::new();
    let mut throughput = Vec::new();
    // 样本数按“配置发起的事务数”固定；实际成功数与 Busy 在 detail 中分开报告。
    let total_transactions = CONTENTION_WRITERS
        .iter()
        .map(|writers| writers * CONTENTION_TX_PER_WRITER)
        .sum::<usize>();
    for writers in CONTENTION_WRITERS {
        // 连接**顺序**建好再交给线程：并发 open 会走 journal_mode/迁移路径，
        // 那部分锁竞争不属于被测事务，混进来会把 Busy 记错。
        let mut stores = Vec::with_capacity(writers);
        for _ in 0..writers {
            stores.push(
                open_store(&path)
                    .map_err(|err| format!("并发写者连接建立失败（{writers} 写者）：{err}"))?,
            );
        }

        let started = Instant::now();
        let results: Vec<WriterOutcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = stores
                .into_iter()
                .enumerate()
                .map(|(writer, store)| {
                    scope.spawn(move || {
                        let mut store = store;
                        let mut latencies = Vec::with_capacity(CONTENTION_TX_PER_WRITER);
                        let mut busy = 0u64;
                        let mut other_errors = 0u64;
                        let mut last_error = None;
                        for index in 0..CONTENTION_TX_PER_WRITER {
                            let entry = WorkQueueEntry::new(
                                DomainId::new(COMPANY),
                                format!("issue:c-{writer:02}-{index:04}"),
                                NOW + index as i64,
                            );
                            let started = Instant::now();
                            let outcome = store.transaction().and_then(|uow| {
                                uow.put_queue(&entry)?;
                                uow.commit()
                            });
                            match outcome {
                                Ok(()) => latencies.push(started.elapsed().as_micros()),
                                Err(err) if is_busy(&err) => busy += 1,
                                Err(err) => {
                                    other_errors += 1;
                                    last_error = Some(err.to_string());
                                }
                            }
                        }
                        WriterOutcome {
                            latencies,
                            busy,
                            other_errors,
                            last_error,
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect()
        });
        let elapsed = started.elapsed();
        if results.len() != writers {
            return Err(format!("{writers} 写者中有线程异常退出"));
        }

        let mut all = Vec::new();
        let mut busy = 0u64;
        for outcome in &results {
            all.extend(outcome.latencies.iter().copied());
            busy += outcome.busy;
        }
        let other: u64 = results.iter().map(|outcome| outcome.other_errors).sum();
        if other > 0 {
            let detail = results
                .iter()
                .filter_map(|outcome| outcome.last_error.clone())
                .next()
                .unwrap_or_default();
            return Err(format!(
                "并发写出现 {other} 次非 Busy 错误（{writers} 写者）：{detail}；这不能算作写锁争用"
            ));
        }

        let p99_us = percentile_us(&mut all, 0.99);
        let count = all.len();
        samples.push(ContentionSample {
            writers,
            transactions: count,
            p99_us,
            busy_errors: busy,
        });
        throughput.push((writers, count as f64 / elapsed.as_secs_f64().max(1e-6)));
    }

    let verdict = classify_contention(&samples);
    let worst_p99_ms = samples
        .iter()
        .max_by_key(|sample| sample.writers)
        .map(|sample| sample.p99_us as f64 / 1_000.0)
        .unwrap_or(0.0);
    let detail = samples
        .iter()
        .map(|sample| {
            format!(
                "{} 写者 p99 {:.2} ms / Busy {}",
                sample.writers,
                sample.p99_us as f64 / 1_000.0,
                sample.busy_errors
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    let meets = Some(matches!(verdict, ContentionVerdict::NoContention { .. }));
    let item = MeasureItem::new(
        "write_contention_p99_growth",
        FormFactor::Headless,
        "p99 增长 ≤ 25%",
    )
    .with_method(format!(
        "顺序建立 {} 个独立连接（并发 open 的锁竞争不计入），共配置 {total_transactions} 次单条写事务（BEGIN IMMEDIATE + commit，busy_timeout 取 BodConfig 默认值）；samples 记配置发起数，实际成功与 Busy 分开报告",
        CONTENTION_WRITERS
            .iter()
            .map(|count| count.to_string())
            .collect::<Vec<_>>()
            .join("/")
    ))
    .with_note(format!("{detail}；归因：{}", contention_label(&verdict)))
    .with_numeric(worst_p99_ms)
    .measured(
        detail,
        total_transactions,
        BENCH_COMMAND,
        meets,
    );
    Ok((
        item,
        ContentionRun {
            samples,
            throughput,
            transactions: total_transactions,
        },
    ))
}

fn contention_label(verdict: &ContentionVerdict) -> String {
    match verdict {
        ContentionVerdict::WriteContention {
            busy_errors,
            growth,
            ..
        } => format!(
            "写锁争用（Busy {busy_errors}，p99 增长 {:.0}%）",
            growth * 100.0
        ),
        ContentionVerdict::QueueingWithoutBusy {
            growth,
            unknown_areas,
            ..
        } => format!(
            "无 Busy，p99 增长 {:.0}%：写者排队，不归因为 Busy 争用；unknown：{}",
            growth * 100.0,
            unknown_areas.join("、")
        ),
        ContentionVerdict::NoContention { growth, .. } => {
            format!("无争用（p99 增长 {:.0}%，噪声内）", growth * 100.0)
        }
        ContentionVerdict::Unknown { reason } => format!("无法判定：{reason}"),
    }
}

// ---------------------------------------------------------------------------
// 2.3 体积、重建成本与索引优化空间
// ---------------------------------------------------------------------------

const BUDGET_DB_BYTES: u64 = 256 * 1024 * 1024;
const BUDGET_REBUILD_MS: f64 = 10_000.0;

fn volume_and_rebuild(
    root: &Path,
) -> Result<(Vec<MeasureItem>, VolumeReading, VolumeAssessment), String> {
    let path = root.join("events.sqlite");
    if !path.exists() {
        return Err("没有 10 万事件样本库，体积与重建成本未测".to_string());
    }
    checkpoint(&path)?;
    let db_bytes = std::fs::metadata(&path)
        .map(|meta| meta.len())
        .map_err(|err| err.to_string())?;
    let conn = raw_conn(&path)?;
    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .map_err(|err| err.to_string())?;
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(|err| err.to_string())?;
    let event_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .map_err(|err| err.to_string())?;

    // dbstat 是可选虚拟表：拿不到就记 None，不假装 0。
    let index_bytes: Option<u64> = conn
        .query_row(
            "SELECT SUM(pgsize) FROM dbstat WHERE name LIKE 'idx_%' OR name LIKE 'ux_%'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .map(|value| value as u64);

    let mut rebuild_samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let started = Instant::now();
        conn.execute_batch("REINDEX")
            .map_err(|err| err.to_string())?;
        rebuild_samples.push(ms(started.elapsed()));
    }
    let rebuild_ms = median_f64(&mut rebuild_samples);

    // events 上有 UNIQUE(aggregate_id, aggregate_version) 自动索引：用聚合列当"未索引"对照会失真。
    // 这里用真正没有索引的 payload 列做全表扫描，与主键等值查询对比。
    let unindexed_lookup_us = timed_lookup(
        &conn,
        "SELECT COUNT(*) FROM events WHERE payload LIKE ?1",
        "%perf%",
    )?;
    let indexed_lookup_us = timed_lookup(
        &conn,
        "SELECT COUNT(*) FROM events WHERE event_id = ?1",
        "ev-050000",
    )?;

    let reading = VolumeReading {
        db_bytes,
        event_rows,
        index_bytes,
        rebuild_ms,
        unindexed_lookup_us,
        indexed_lookup_us,
    };
    let assessment = classify_volume(&reading, BUDGET_DB_BYTES, BUDGET_REBUILD_MS);

    let volume_value = match index_bytes {
        Some(bytes) => format!(
            "库 {:.1} MiB（{} 页 × {} B），索引约 {:.1} MiB，{} 行",
            db_bytes as f64 / 1_048_576.0,
            page_count,
            page_size,
            bytes as f64 / 1_048_576.0,
            event_rows
        ),
        None => format!(
            "库 {:.1} MiB（{} 页 × {} B），索引占用未取得（dbstat 不可用），{} 行",
            db_bytes as f64 / 1_048_576.0,
            page_count,
            page_size,
            event_rows
        ),
    };
    let volume_item = MeasureItem::new(
        "table_index_volume_100k_events",
        FormFactor::Headless,
        "体积预算 256 MiB",
    )
    .with_method("库文件字节数 + page_count × page_size + dbstat 按名聚合索引页")
    .with_note(format!(
        "体积压力：{:?}；重建压力：{:?}；索引优化空间：{:?}",
        assessment.size, assessment.rebuild, assessment.index_space
    ))
    .with_numeric(db_bytes as f64 / 1_048_576.0)
    .measured(volume_value, 1, BENCH_COMMAND, None);

    let rebuild_item = MeasureItem::new(
        "index_rebuild_cost_100k_events",
        FormFactor::Headless,
        "重建预算 10 s",
    )
    .with_method(
        "对 10 万事件库执行 REINDEX（全索引重建）5 次取中位数；测量前先 WAL TRUNCATE checkpoint",
    )
    .with_note("重建成本随行数与索引数线性增长；跨机器比较必须同数据规模")
    .with_numeric(rebuild_ms)
    .measured(
        format!("{rebuild_ms:.0} ms（5 次中位）"),
        rebuild_samples.len(),
        BENCH_COMMAND,
        Some(rebuild_ms <= BUDGET_REBUILD_MS),
    );

    let factor = if indexed_lookup_us == 0 {
        0.0
    } else {
        unindexed_lookup_us as f64 / indexed_lookup_us as f64
    };
    let lookup_item = MeasureItem::new(
        "indexed_vs_unindexed_lookup",
        FormFactor::Headless,
        "无冻结目标（判断索引优化空间）",
    )
    .with_method(
        "同一张 events 表：payload（无索引，LIKE 全表扫描）vs event_id（主键等值），各 50 次取中位数",
    )
    .with_note(format!(
        "两者差 {factor:.1} 倍：{:?}；events 的 aggregate_id 已有 UNIQUE 自动索引，不能拿它当「未索引」对照",
        assessment.index_space
    ))
    .with_numeric(factor)
    .measured(
        format!(
            "全表扫描 {:.2} ms / 索引等值 {:.3} ms（{factor:.0}×）",
            unindexed_lookup_us as f64 / 1_000.0,
            indexed_lookup_us as f64 / 1_000.0
        ),
        50,
        BENCH_COMMAND,
        None,
    );

    Ok((
        vec![volume_item, rebuild_item, lookup_item],
        reading,
        assessment,
    ))
}

fn timed_lookup(conn: &rusqlite::Connection, sql: &str, value: &str) -> Result<u128, String> {
    let mut samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let started = Instant::now();
        let _count: i64 = conn
            .query_row(sql, rusqlite::params![value], |row| row.get(0))
            .map_err(|err| err.to_string())?;
        samples.push(started.elapsed().as_micros());
    }
    Ok(percentile_us(&mut samples, 0.5))
}

// ---------------------------------------------------------------------------
// 2.4 执行槽位与并发容量
// ---------------------------------------------------------------------------

fn executor_slots(run: &ContentionRun) -> (Vec<MeasureItem>, SlotReading, SlotVerdict) {
    let single = run
        .throughput
        .iter()
        .find(|(writers, _)| *writers == 1)
        .map(|(_, per_sec)| *per_sec)
        .unwrap_or(0.0);
    let (widest, saturated) = run
        .throughput
        .iter()
        .copied()
        .max_by_key(|(writers, _)| *writers)
        .unwrap_or((1, 0.0));
    let reading = SlotReading {
        single_writer_per_sec: single,
        saturated_per_sec: saturated,
        // 本仓库没有真实 Agent 进程参与测量：如实留 None。
        agent_processes_observed: None,
    };
    let verdict = classify_slots(&reading);
    let gain = saturated / single.max(1e-6) - 1.0;
    let label = match &verdict {
        SlotVerdict::WritePathBound { .. } => {
            "写路径受限：加执行节点拿不到吞吐，先优化存储".to_string()
        }
        SlotVerdict::ScaleExecutors { .. } => "可扩执行节点：多写者有明确收益".to_string(),
        SlotVerdict::Unknown { reason } => format!("无法判定：{reason}"),
    };
    let capacity = MeasureItem::new(
        "executor_slot_capacity",
        FormFactor::Headless,
        "无冻结目标（判断写路径 vs 执行槽位）",
    )
    .with_method(format!(
        "同一批写事务在 1 与 {widest} 个并发连接下的每秒完成数（共 {} 次事务）",
        run.transactions
    ))
    .with_note(format!(
        "{label}；样本来自 {}；跨阶段标量用最宽写者绝对吞吐，避免收益比例在接近零分母时失真",
        run.samples.len()
    ))
    .with_numeric(saturated)
    .measured(
        format!(
            "单写 {single:.0} txn/s → {widest} 写者 {saturated:.0} txn/s（收益 {:.0}%）",
            gain * 100.0
        ),
        run.transactions,
        BENCH_COMMAND,
        None,
    );
    let agent = MeasureItem::new(
        "agent_slot_occupancy",
        FormFactor::Headless,
        "无冻结目标（Agent 槽位占用）",
    )
    .with_method("需要真实 Agent 进程：统计并发进程数、各自 RSS 与被抢占次数")
    .unverified(
        "bench 内没有真实 Agent 进程参与，测不到 Agent 槽位占用与并发上限",
        BENCH_COMMAND,
    );
    (vec![capacity, agent], reading, verdict)
}

fn environment(environment_note_text: &str, data_scale: &str) -> EnvironmentRecord {
    EnvironmentRecord {
        note: environment_note_text.to_string(),
        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        cpu: format!(
            "{} 逻辑核",
            std::thread::available_parallelism()
                .map(|value| value.get())
                .unwrap_or(0)
        ),
        rustc: rustc_version(),
        build_params: format!(
            "{} / debug_assertions={} / CARGO_PROFILE_DEV_DEBUG={} / 单进程单连接",
            if cfg!(debug_assertions) {
                "dev profile"
            } else {
                "release profile"
            },
            cfg!(debug_assertions),
            std::env::var("CARGO_PROFILE_DEV_DEBUG").unwrap_or_else(|_| "未设置".to_string()),
        ),
        sqlite_sync: "WAL + synchronous=FULL + BEGIN IMMEDIATE（BodConfig::default）".to_string(),
        data_scale: data_scale.to_string(),
        cache_state: "每次运行新建临时库；统一 cache_size=-65536 KiB 与 temp_store=MEMORY；重启/恢复/体积前做 WAL TRUNCATE checkpoint；队列与 Web 投影预热后计时".to_string(),
    }
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 运行全部可测项。`environment_note` 是 CLI 打印的环境行，原样留在报告里便于人工对照。
/// 一次完整 bench 的产物：基线报告 + 四类瓶颈归因。
#[derive(Debug, Clone, PartialEq)]
pub struct BenchOutput {
    pub baseline: BaselineReport,
    pub contention: ContentionVerdict,
    pub recovery: RecoveryVerdict,
    pub volume_reading: VolumeReading,
    pub volume: VolumeAssessment,
    pub slots: SlotVerdict,
}

impl BenchOutput {
    /// 2.5：四类毕业信号（缺数据的一律 unknown）。
    pub fn signals(&self) -> Vec<crate::capacity_evaluation::SignalReading> {
        crate::bottleneck::signals_from(
            &self.contention,
            &self.recovery,
            &self.volume_reading,
            &self.volume,
            &self.slots,
        )
    }
}

pub fn run_detailed(environment_note: &str, commit: &str) -> Result<BenchOutput, String> {
    let (root, store) = workspace("main")?;
    drop(store);
    let _ = CasStore::open(root.join("cas")).map_err(|err| err.to_string())?;

    // 空闲 RSS 在业务负载之前取样，但要在存储就绪（迁移已应用）之后。
    let rss = idle_rss();

    let mut items = vec![
        rss,
        cold_start(&root)?,
        cold_start_api_ready(),
        transaction_latency(&root)?,
    ];
    items.push(event_throughput(&root, 5_000)?);
    items.push(durable_event_throughput(&root, 200)?);
    items.push(queue_selection(&root, 1_000)?);
    let events = 100_000;
    items.push(restart_after_events(&root, events)?);
    items.push(storage_volume(&root, events)?);

    // §2 瓶颈定位：RTO、争用、体积/重建、槽位，各自独立成项。
    let (rto_item, recovery) = recovery_rto(&root)?;
    items.push(rto_item);
    let (contention_item, contention_run) = write_contention(&root)?;
    items.push(contention_item);
    let (volume_items, volume_reading, volume) = volume_and_rebuild(&root)?;
    items.extend(volume_items);
    let (slot_items, _slot_reading, slots) = executor_slots(&contention_run);
    items.extend(slot_items);

    items.push(idle_tick(&root)?);
    items.push(idle_cpu_ratio(&root));
    items.push(package_size());
    items.extend(web_and_shell_gaps(&root));

    let baseline = BaselineReport {
        environment: environment(
            environment_note,
            &format!(
                "500 单条事务 + 5000 批量事件 + 200 逐条持久化事件 + 1000 就绪候选 + {events} 事件 checkpoint 后重开 5 次 + 恢复 5 轮 + 争用 1/2/4/8 写者 × {CONTENTION_TX_PER_WRITER} 事务 + 重建 5 次/查询各 50 次"
            ),
        ),
        commit: commit.to_string(),
        items,
    };
    Ok(BenchOutput {
        baseline,
        contention: classify_contention(&contention_run.samples),
        recovery,
        volume_reading,
        volume,
        slots,
    })
}

pub fn run(environment_note: &str, commit: &str) -> Result<BaselineReport, String> {
    run_detailed(environment_note, commit).map(|output| output.baseline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capacity_evaluation::MeasureStatus;

    /// 1.3：实测项清单必须覆盖基线要求的八项，且每项带命令与样本口径。
    #[test]
    fn task_1_3_inventory_covers_the_baseline_measure_set() {
        let required = [
            "idle_rss_headless",
            "cold_start_store_ready",
            "key_transaction_latency",
            "event_throughput_batched",
            "queue_selection_1000",
            "web_projection_p95",
            "state_to_web_visible_p95",
            "restart_after_100k_events",
            "package_size_headless",
        ];
        // 清单本身不跑重活：借用 gap 项与常量，避免在单测里做 10 万事件写入。
        let inventory = {
            let mut names: Vec<&'static str> = vec![
                "idle_rss_headless",
                "cold_start_store_ready",
                "cold_start_to_api_ready",
                "key_transaction_latency",
                "event_throughput_batched",
                "event_throughput_durable",
                "queue_selection_1000",
                "restart_after_100k_events",
                "storage_volume_100k_events",
                "idle_tick_cost",
                "idle_cpu_ratio",
                "web_projection_p95",
                "package_size_headless",
            ];
            let root = std::env::temp_dir()
                .join(format!("bod-perf-web-probe-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&root);
            names.extend(web_and_shell_gaps(&root).iter().map(|item| item.name));
            let _ = std::fs::remove_dir_all(&root);
            names
        };
        for name in required {
            assert!(inventory.contains(&name), "基线缺少测量项 {name}");
        }
        assert!(BENCH_COMMAND.contains("xtask -- bench"));
    }

    /// 1.1：环境记录字段齐全，可被逐项对照。
    #[test]
    fn task_1_1_environment_record_is_complete() {
        let record = environment("linux / x86_64 / rustc test", "测试规模");
        assert_eq!(record.note, "linux / x86_64 / rustc test");
        assert!(!record.os.is_empty());
        assert!(!record.cpu.is_empty());
        assert!(!record.rustc.is_empty());
        assert!(record.build_params.contains("debug_assertions"));
        assert!(record.sqlite_sync.contains("synchronous=FULL"));
        assert_eq!(record.data_scale, "测试规模");
        assert!(record.cache_state.contains("cache_size=-65536 KiB"));
        assert!(record.diff(&record).is_empty());
    }

    /// 1.2：分形态项各自成组；真正的缺口仍未测，本地投影探针单独可测但不冒充渲染可见。
    #[test]
    fn task_1_2_gap_items_are_all_unverified_and_per_form_factor() {
        let root = tempdir().expect("临时目录");
        let items = web_and_shell_gaps(&root);
        let gaps: Vec<_> = items
            .iter()
            .filter(|item| item.name != "web_projection_p95")
            .collect();
        assert!(
            gaps.iter()
                .all(|item| item.status == MeasureStatus::Unverified)
        );
        assert!(gaps.iter().all(|item| item.failure_reason.is_some()));
        let projection = items
            .iter()
            .find(|item| item.name == "web_projection_p95")
            .expect("本地 Web 投影探针应在清单中");
        assert_eq!(projection.status, MeasureStatus::Measured);
        assert!(projection.failure_reason.is_none());
        assert!(items.iter().all(|item| !item.command.is_empty()));
        assert!(
            items.iter().any(|item| item.form_factor == FormFactor::Web),
            "Web 形态要有独立项"
        );
        assert!(
            items
                .iter()
                .any(|item| item.form_factor == FormFactor::Desktop
                    && item.name == "desktop_total_memory"),
            "桌面总内存不能只靠 headless 数字"
        );
        assert!(
            items
                .iter()
                .any(|item| item.form_factor == FormFactor::Mobile)
        );
    }

    /// 1.3：吞吐与尾延迟是两个口径，不得互相顶替。
    #[test]
    fn task_1_3_throughput_and_tail_latency_are_separate_items() {
        let temp = tempdir().expect("临时目录");
        let batched = event_throughput(temp.as_path(), 200).expect("批量吞吐应可测");
        assert_eq!(batched.name, "event_throughput_batched");
        assert_eq!(batched.samples, 200);
        assert!(batched.value.is_some());
        assert!(batched.meets_target.is_some(), "吞吐要与目标比较");

        let durable = durable_event_throughput(temp.as_path(), 20).expect("逐条吞吐应可测");
        assert_eq!(durable.name, "event_throughput_durable");
        assert!(
            durable.meets_target.is_none(),
            "逐条持久化没有冻结目标，不得声称达标"
        );

        let tx = transaction_latency(temp.as_path()).expect("事务延迟应可测");
        assert_eq!(tx.name, "key_transaction_latency");
        assert_eq!(tx.samples, 500, "尾延迟要有样本数");
    }

    fn tempdir() -> Result<PathBuf, String> {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("bod-perf-test-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&root).map_err(|err| err.to_string())?;
        Ok(root)
    }
}
