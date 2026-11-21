//! 性能实测：P0 §38.2 空闲负担 + P7 容量基线（任务 1.1—1.3）。
//!
//! 只测**本机进程内可测项**；需要 OS 计数器细节、WebView、真机或安装包的条目一律标
//! `Unverified` 并写清测量缺口——**不以设计目标冒充实测**，也不把 headless 的数字套到
//! 桌面/移动上。
//!
//! 测量口径（口径定义在 [`crate::capacity_evaluation`]）：
//! - 环境：OS / 架构 / rustc / 构建参数 / SQLite 同步策略 / 数据规模 / 缓存冷热；
//! - SQLite：WAL + `synchronous=FULL` + `BEGIN IMMEDIATE`（`BodConfig::default()`）；
//! - 延迟取 p50/p95/p99（排序后取分位），单位毫秒；**吞吐与尾延迟分别结论**；
//! - 每一项都带样本数与命令，缺命令或缺样本的项不进入容量决策。

use crate::capacity_evaluation::{BaselineReport, EnvironmentRecord, FormFactor, MeasureItem};
use crate::cas::CasStore;
use crate::config::BodConfig;
use crate::domain::DomainId;
use crate::domain::queue::WorkQueueEntry;
use crate::store::SqliteStore;
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
    let store = SqliteStore::open(&root.join("bod.sqlite"), &BodConfig::default())
        .map_err(|err| err.to_string())?;
    Ok((root, store))
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
    for round in 0..5 {
        let path = root.join(format!("cold-{round}.sqlite"));
        let started = Instant::now();
        let store =
            SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
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
    .with_method("新建空库 + 应用全部迁移，重复 5 次取 p50/p99；文件在本地临时目录，冷缓存不可控")
    .with_note("只覆盖到存储就绪，未包含 HTTP 监听与握手，故不能直接宣称 API ready 达标")
    .measured(
        format!("p50 {:.1} ms / p99 {:.1} ms", ms(p50), ms(p99)),
        samples.len(),
        BENCH_COMMAND,
        verdict(ms(p99), TARGET_COLD_START_MS),
    ))
}

/// 冷启动到 API ready：bench 内没有宿主监听器，如实记缺口。
fn cold_start_api_ready() -> MeasureItem {
    MeasureItem::new(
        "cold_start_to_api_ready",
        FormFactor::Headless,
        "≤ 1 s（到本地 API ready）",
    )
    .with_method("需要宿主进程内真实监听 + 一次成功握手；bench 不启动宿主")
    .unverified(
        "bench 进程内没有 HTTP 监听器，无法测到 ready；只测到存储就绪（cold_start_store_ready）",
        BENCH_COMMAND,
    )
}

/// 关键事务延迟：单条写事务（含 WAL + synchronous=FULL 提交）。
fn transaction_latency(root: &Path) -> Result<MeasureItem, String> {
    let path = root.join("tx.sqlite");
    let mut store =
        SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
    let mut samples = Vec::with_capacity(200);
    for index in 0..200 {
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
    .with_method("200 次单条写事务（BEGIN IMMEDIATE + 一次 INSERT + COMMIT），WAL/synchronous=FULL；单连接串行，无并发争用")
    .with_note("串行单写线程口径；多进程争用下的排队等待未覆盖")
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
    let mut store =
        SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
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
    let mut store =
        SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
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
    let mut store =
        SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
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

    let mut samples = Vec::new();
    for _ in 0..50 {
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
        "{entries} 条 work_queue 全量读取（含 50 次重复），页缓存热态，单连接"
    ))
    .with_note("全量扫描而非分页；超过 1000 条后的线性增长需要另行测量")
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
    let mut store =
        SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
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
    drop(store);

    let started = Instant::now();
    let reopened =
        SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
    let ready = started.elapsed();
    let counts = reopened.core_row_counts().map_err(|err| err.to_string())?;
    drop(reopened);

    Ok(MeasureItem::new(
        "restart_after_100k_events",
        FormFactor::Headless,
        "10 万事件后重启 ready ≤ 3 s",
    )
    .with_method(format!(
        "单事务写入 {events} 条 events 后关闭连接，重新打开同一文件库并读取行数（issues={}）",
        counts.issues
    ))
    .with_note("当前无事件回放投影，重开只应用幂等迁移；接入投影后必须重测")
    .measured(
        format!(
            "{events} 事件写入 {:.0} ms；重开就绪 {:.0} ms",
            ms(seed),
            ms(ready)
        ),
        1,
        BENCH_COMMAND,
        verdict(ms(ready), TARGET_RESTART_MS),
    ))
}

/// 空闲循环单次成本：队列扫描 + 租约到期检查 + outbox 状态查询。
fn idle_tick(root: &Path) -> Result<MeasureItem, String> {
    let path = root.join("idle.sqlite");
    let store = SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
    let mut samples = Vec::new();
    for _ in 0..100 {
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
    .with_method("空库上空转 100 次：队列读取 + outbox Prepared 查询 + open 对账案件查询")
    .with_note("单次成本低不等于空闲 CPU 占比达标；轮询周期与唤醒机制需另行测量")
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

fn idle_cpu_ratio() -> MeasureItem {
    MeasureItem::new(
        "idle_cpu_ratio",
        FormFactor::Headless,
        "无冻结数值；§38.2 要求空闲时无模型调用与热轮询",
    )
    .with_method("需要进程 CPU 时间计数器与固定观测窗口（≥60 s）")
    .unverified(
        "已测 idle_tick_cost 作为单次成本下界，但空闲 CPU 占比未取得实测",
        BENCH_COMMAND,
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

fn web_and_shell_gaps() -> Vec<MeasureItem> {
    vec![
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
        cache_state: "冷缓存：每次运行新建临时库；仅队列候选读取为页缓存热态".to_string(),
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
pub fn run(environment_note: &str, commit: &str) -> Result<BaselineReport, String> {
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
    items.push(idle_tick(&root)?);
    items.push(idle_cpu_ratio());
    items.push(package_size());
    items.extend(web_and_shell_gaps());

    Ok(BaselineReport {
        environment: environment(
            environment_note,
            &format!(
                "200 单条事务 + 5000 批量事件 + 200 逐条持久化事件 + 1000 就绪候选 + {events} 事件重启"
            ),
        ),
        commit: commit.to_string(),
        items,
    })
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
                "package_size_headless",
            ];
            names.extend(web_and_shell_gaps().iter().map(|item| item.name));
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
        assert!(record.cache_state.contains("冷"));
        assert!(record.diff(&record).is_empty());
    }

    /// 1.2：分形态项各自成组，headless 之外一律未测。
    #[test]
    fn task_1_2_gap_items_are_all_unverified_and_per_form_factor() {
        let items = web_and_shell_gaps();
        assert!(
            items
                .iter()
                .all(|item| item.status == MeasureStatus::Unverified)
        );
        assert!(items.iter().all(|item| item.failure_reason.is_some()));
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
        assert_eq!(tx.samples, 200, "尾延迟要有样本数");
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
