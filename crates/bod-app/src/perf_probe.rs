//! P0 空闲负担与 §38.2 性能预算实测（任务 7.6）。
//!
//! 只测本机进程内可测项；需要 OS 计数器或外部工具（空闲 RSS、空闲 CPU 占比）的条目
//! 一律标 `Unverified` 并写清测量缺口——**不以设计目标冒充实测**。
//!
//! 测量口径（§38.2 要求记录）：
//! - 环境：OS / 架构 / rustc 版本（由 CLI 传入）；
//! - SQLite：WAL + `synchronous=FULL` + `BEGIN IMMEDIATE`（`BodConfig::default()`）；
//! - 数据规模与缓存冷热：逐项写在 `method` 与 `note` 中；
//! - 延迟取 p50/p99（排序后取分位），单位毫秒；吞吐与容量不在此报告。

use crate::cas::CasStore;
use crate::config::BodConfig;
use crate::domain::DomainId;
use crate::domain::queue::WorkQueueEntry;
use crate::store::SqliteStore;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const COMPANY: &str = "company-default";
const NOW: i64 = 1_763_500_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureStatus {
    /// 本次真实执行并取得数值。
    Measured,
    /// 机制已具备，但测量需要当前环境不具备的手段：保留 Target 并标注缺口。
    Unverified,
}

impl MeasureStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "Measured",
            Self::Unverified => "Unverified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfItem {
    pub name: &'static str,
    /// 设计目标（来自 §38.2）；未设定的写 `无冻结目标`。
    pub target: &'static str,
    pub measured: Option<String>,
    pub status: MeasureStatus,
    /// 测量方法与数据规模。
    pub method: String,
    /// 结论或缺口说明，不夸大。
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfReport {
    pub environment: String,
    pub commit: String,
    pub items: Vec<PerfItem>,
}

impl PerfReport {
    pub fn measured_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == MeasureStatus::Measured)
            .count()
    }

    pub fn unverified_count(&self) -> usize {
        self.items.len() - self.measured_count()
    }
}

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

/// 冷启动：仅到"存储就绪（迁移已应用）"，不含 HTTP ready 与外部探测。
fn cold_start(root: &Path) -> Result<PerfItem, String> {
    let mut samples = Vec::new();
    for round in 0..5 {
        let path = root.join(format!("cold-{round}.sqlite"));
        let started = Instant::now();
        let store =
            SqliteStore::open(&path, &BodConfig::default()).map_err(|err| err.to_string())?;
        let elapsed = started.elapsed();
        drop(store);
        samples.push(elapsed);
    }
    let mut sorted = samples.clone();
    let p50 = percentile(&mut sorted, 0.5);
    let p99 = percentile(&mut sorted, 0.99);
    Ok(PerfItem {
        name: "cold_start_store_ready",
        target: "≤ 500 ms（到本地 API ready）",
        measured: Some(format!("p50 {:.1} ms / p99 {:.1} ms", ms(p50), ms(p99))),
        status: MeasureStatus::Measured,
        method: "新建空库 + 应用 6 个迁移，重复 5 次取 p50/p99；文件在本地临时目录，冷缓存不可控"
            .to_string(),
        note: "只覆盖到存储就绪，未包含 HTTP 监听与握手，故不能直接宣称达标".to_string(),
    })
}

/// 关键事务延迟：单条写事务（含 WAL + synchronous=FULL 提交）。
fn transaction_latency(root: &Path) -> Result<PerfItem, String> {
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
    let p99 = percentile(&mut sorted, 0.99);
    Ok(PerfItem {
        name: "key_transaction_latency",
        target: "p99 ≤ 10 ms",
        measured: Some(format!("p50 {:.2} ms / p99 {:.2} ms", ms(p50), ms(p99))),
        status: MeasureStatus::Measured,
        method: "200 次单条写事务（BEGIN IMMEDIATE + 一次 INSERT + COMMIT），WAL/synchronous=FULL；"
            .to_string() + "单连接串行，无并发争用",
        note: "串行单写线程口径；多进程争用下的排队等待未覆盖".to_string(),
    })
}

/// 队列扫描与就绪候选选择成本。
fn queue_selection(root: &Path, entries: usize) -> Result<PerfItem, String> {
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
        assert_eq!(rows.len(), entries);
        samples.push(started.elapsed());
    }
    let mut sorted = samples.clone();
    let p50 = percentile(&mut sorted, 0.5);
    let mut sorted = samples.clone();
    let p99 = percentile(&mut sorted, 0.99);
    Ok(PerfItem {
        name: "queue_selection_1000",
        target: "1000 就绪候选 p99 ≤ 20 ms",
        measured: Some(format!(
            "{entries} 条 p50 {:.2} ms / p99 {:.2} ms",
            ms(p50),
            ms(p99)
        )),
        status: MeasureStatus::Measured,
        method: format!("{entries} 条 work_queue 全量读取（含 50 次重复），页缓存热态，单连接"),
        note: "全量扫描而非分页；超过 1000 条后的线性增长需要另行测量".to_string(),
    })
}

/// 10 万事件后的重启就绪时间。
fn restart_after_events(root: &Path, events: usize) -> Result<PerfItem, String> {
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

    Ok(PerfItem {
        name: "restart_after_100k_events",
        target: "10 万事件后重启 ready ≤ 3 s",
        measured: Some(format!(
            "{events} 事件写入 {:.0} ms；重开就绪 {:.0} ms",
            ms(seed),
            ms(ready)
        )),
        status: MeasureStatus::Measured,
        method: format!(
            "单事务写入 {events} 条 events 后关闭连接，重新打开同一文件库并读取行数（issues={}）",
            counts.issues
        ),
        note: "P0 无事件回放投影，重开只应用幂等迁移；接入投影后需重测".to_string(),
    })
}

/// 空闲循环单次成本：队列扫描 + 租约到期检查 + outbox 状态查询。
fn idle_tick(root: &Path) -> Result<PerfItem, String> {
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
    Ok(PerfItem {
        name: "idle_tick_cost",
        target: "无冻结目标（§38.2 只约束 RSS/CPU 占比）",
        measured: Some(format!("p50 {:.3} ms / p99 {:.3} ms", ms(p50), ms(p99))),
        status: MeasureStatus::Measured,
        method: "空库上空转 100 次：队列读取 + outbox Prepared 查询 + open 对账案件查询"
            .to_string(),
        note: "单次成本低不等于空闲 CPU 占比达标；轮询周期与唤醒机制需另行测量".to_string(),
    })
}

fn idle_resources() -> Vec<PerfItem> {
    vec![
        PerfItem {
            name: "idle_rss_headless",
            target: "≤ 40 MiB（已初始化单 Company、无运行任务）",
            measured: None,
            status: MeasureStatus::Unverified,
            method: "需要 OS 级内存计数器（Windows: GetProcessMemoryInfo；Linux: /proc/self/status VmRSS）"
                .to_string(),
            note: "当前 workspace 无该依赖；未取得实测值，不填设计值，保留 Target".to_string(),
        },
        PerfItem {
            name: "idle_cpu_ratio",
            target: "无冻结数值；§38.2 要求空闲时无模型调用与热轮询",
            measured: None,
            status: MeasureStatus::Unverified,
            method: "需要进程 CPU 时间计数器与固定观测窗口（≥60 s）".to_string(),
            note: "已测 idle_tick 单次成本作为下界代理，但 CPU 占比仍未验证".to_string(),
        },
    ]
}

/// 运行全部可测项。`cas_root` 参与备份类指标的环境说明，本切片未使用其度量。
pub fn run(environment: &str, commit: &str) -> Result<PerfReport, String> {
    let (root, store) = workspace("main")?;
    drop(store);
    let _ = CasStore::open(root.join("cas")).map_err(|err| err.to_string())?;

    let mut items = vec![cold_start(&root)?, transaction_latency(&root)?];
    items.push(queue_selection(&root, 1_000)?);
    items.push(restart_after_events(&root, 100_000)?);
    items.push(idle_tick(&root)?);
    items.extend(idle_resources());

    Ok(PerfReport {
        environment: environment.to_string(),
        commit: commit.to_string(),
        items,
    })
}

/// Markdown 报告：Measured 与 Unverified 分开列，避免混读。
pub fn render_markdown(report: &PerfReport) -> String {
    let mut out = String::new();
    out.push_str("# P0 空闲负担与性能预算实测\n\n");
    out.push_str(&format!("- 环境：{}\n", report.environment));
    out.push_str(&format!("- commit：{}\n", report.commit));
    out.push_str(&format!(
        "- 结果：Measured {} 项 / Unverified {} 项\n\n",
        report.measured_count(),
        report.unverified_count()
    ));
    out.push_str("| 指标 | 目标 | 实测 | 状态 |\n| --- | --- | --- | --- |\n");
    for item in &report.items {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            item.name,
            item.target,
            item.measured.as_deref().unwrap_or("—"),
            item.status.as_str()
        ));
    }
    out.push('\n');
    for item in &report.items {
        out.push_str(&format!("## {}\n\n", item.name));
        out.push_str(&format!("- 目标：{}\n", item.target));
        out.push_str(&format!(
            "- 实测：{}\n",
            item.measured.as_deref().unwrap_or("未测（Unverified）")
        ));
        out.push_str(&format!("- 状态：{}\n", item.status.as_str()));
        out.push_str(&format!("- 方法：{}\n", item.method));
        out.push_str(&format!("- 说明：{}\n\n", item.note));
    }
    out
}
