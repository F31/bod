//! 本机真实执行配置适配器：用真实子进程完成 describe/probe/start/inspect/observe/cancel
//! （P0 D4 任务 5.5）。
//!
//! 这是"真实适配器"而非 Fake Executor：它真的启动本机可执行程序、真的读 stdout/stderr、
//! 真的用 wait 状态判定进程是否停止、真的跑 `--version` 做环境探测。不做任何模型调用，
//! 也不在后台触发交互登录或付费推理。
//!
//! 能力声明刻意保守：没有持久去重、没有可靠观察回放、没有文件系统沙箱、没有用量口径。
//! 这些都写进 capabilities 与 limitations，界面据此显示限制，而不是假装支持。

use crate::descriptor_schema::parse_descriptor;
use crate::domain::execution::{
    AdapterDescriptor, CancelStage, CapabilityClaim, CapabilitySlot, EffectClass, EvidenceLevel,
    EvidenceSource, ExecutionState, ObservationEnvelope, ObservationType, ObserveTransport,
    Operation, PROTOCOL_ID, ProbeAspect, ProbeItem, ProbeReport, ProbeStatus, StartRequest,
    SupportLevel, map_process_exit,
};
use crate::invocation::{StartOutcome, StartTransport};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const ADAPTER_ID: &str = "adapter.local-process";
pub const ADAPTER_VERSION: &str = "1.0.0";

/// 本机执行配置：一个真实可执行程序 + 参数 + 工作目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProcessConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub working_dir: Option<PathBuf>,
}

impl LocalProcessConfig {
    pub fn new(executable: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            executable: executable.into(),
            args,
            working_dir: None,
        }
    }

    pub fn with_working_dir(mut self, dir: PathBuf) -> Self {
        self.working_dir = Some(dir);
        self
    }

    /// 探测本机已就绪的解释器。返回第一个能真实执行 `--version` 的候选。
    /// 只探测候选列表，不遍历全部二进制，避免宿主启动时扫盘（§15.2）。
    pub fn detect(candidates: &[&str]) -> Option<Self> {
        for candidate in candidates {
            let ok = Command::new(candidate)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .is_ok_and(|output| output.status.success());
            if ok {
                return Some(Self::new(*candidate, Vec::new()));
            }
        }
        None
    }

    /// 本机默认探测顺序：python3 → python → node。
    pub fn detect_default() -> Option<Self> {
        Self::detect(&["python3", "python", "node"])
    }

    /// 一段真实可执行的演示脚本：打印一行后存活一小段时间，便于观察与取消。
    pub fn keep_alive_script(language_hint: &str) -> Vec<String> {
        if language_hint.contains("node") {
            vec![
                "-e".to_string(),
                "console.log('bod-local-process ready'); setTimeout(()=>{}, 1500);".to_string(),
            ]
        } else {
            vec![
                "-c".to_string(),
                "import time; print('bod-local-process ready'); time.sleep(1.5)".to_string(),
            ]
        }
    }

    pub fn exit_zero_script(language_hint: &str) -> Vec<String> {
        if language_hint.contains("node") {
            vec![
                "-e".to_string(),
                "console.log('done'); process.exit(0);".to_string(),
            ]
        } else {
            vec!["-c".to_string(), "print('done')".to_string()]
        }
    }
}

struct ManagedChild {
    child: Child,
    sink: Arc<Mutex<Vec<String>>>,
    sequence: u64,
    started_at_ms: i64,
}

impl std::fmt::Debug for ManagedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedChild")
            .field("pid", &self.child.id())
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// 真实子进程适配器。持有子进程句柄，`Drop` 时清理，避免留下孤儿进程。
pub struct LocalProcessAdapter {
    config: LocalProcessConfig,
    descriptor: AdapterDescriptor,
    children: BTreeMap<String, ManagedChild>,
    exits: BTreeMap<String, i32>,
    cancelled: BTreeMap<String, bool>,
}

impl LocalProcessAdapter {
    pub fn new(config: LocalProcessConfig, now_ms: i64) -> Self {
        let descriptor = build_descriptor(&config, now_ms);
        Self {
            config,
            descriptor,
            children: BTreeMap::new(),
            exits: BTreeMap::new(),
            cancelled: BTreeMap::new(),
        }
    }

    /// 自动探测本机运行时并构造适配器；没有可用运行时返回 None，不假装就绪。
    pub fn detect(now_ms: i64) -> Option<Self> {
        LocalProcessConfig::detect_default().map(|config| Self::new(config, now_ms))
    }

    pub fn config(&self) -> &LocalProcessConfig {
        &self.config
    }

    /// describe 结果。与 `StartTransport::descriptor` 同源，CLI 无需导入 trait。
    pub fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    /// 替换入口参数并重建描述符：describe 必须反映当前配置，不能留旧参数。
    pub fn set_args(&mut self, args: Vec<String>, now_ms: i64) {
        self.config.args = args;
        self.descriptor = build_descriptor(&self.config, now_ms);
    }

    pub fn exit_code(&self, invocation_id: &str) -> Option<i32> {
        self.exits.get(invocation_id).copied()
    }

    /// 真实探测：环境项真的执行 `--version`；凭据/模型/配额如实标 unsupported。
    pub fn probe(&self, now_ms: i64) -> ProbeReport {
        let item = |aspect, status, reason: String, scope: &str| ProbeItem {
            aspect,
            status,
            reason,
            source: EvidenceSource::PlatformProbe,
            scope: scope.to_string(),
            observed_at_ms: now_ms,
            expires_at_ms: Some(now_ms + 300_000),
        };

        let (env_status, env_reason) = match self.version_string() {
            Ok(version) => (
                ProbeStatus::Ready,
                format!(
                    "{} 可执行且 --version 成功：{version}",
                    self.config.executable
                ),
            ),
            Err(err) => (
                ProbeStatus::NotReady,
                format!(
                    "{} 不可执行或 --version 失败：{err}",
                    self.config.executable
                ),
            ),
        };

        let mut items = vec![
            item(
                ProbeAspect::Environment,
                env_status,
                env_reason,
                "local-process/executable",
            ),
            item(
                ProbeAspect::Credentials,
                ProbeStatus::Unsupported,
                "本机进程不读取模型凭据；宿主不代为登录，也不回传密钥".to_string(),
                "local-process/env",
            ),
            item(
                ProbeAspect::Model,
                ProbeStatus::Unsupported,
                "不调用模型推理，模型可用性由用户在程序内自行配置".to_string(),
                "local-process/model",
            ),
            item(
                ProbeAspect::Quota,
                ProbeStatus::Unsupported,
                "本机进程不提供配额窗口或计费口径".to_string(),
                "local-process/quota",
            ),
        ];

        match &self.config.working_dir {
            Some(dir) => {
                let ok = dir.is_dir();
                items.push(item(
                    ProbeAspect::Workspace,
                    if ok {
                        ProbeStatus::Ready
                    } else {
                        ProbeStatus::NotReady
                    },
                    format!("工作目录 {} 存在={ok}", dir.display()),
                    "local-process/workspace",
                ));
            }
            None => items.push(item(
                ProbeAspect::Workspace,
                ProbeStatus::Ready,
                "使用宿主进程当前目录".to_string(),
                "local-process/workspace",
            )),
        }

        ProbeReport {
            adapter_id: ADAPTER_ID.to_string(),
            adapter_version: ADAPTER_VERSION.to_string(),
            items,
            generated_at_ms: now_ms,
        }
    }

    /// 执行真实 `--version`，返回首行文本（截断到 120 字符）。
    pub fn version_string(&self) -> Result<String, String> {
        let output = Command::new(&self.config.executable)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| err.to_string())?;
        let mut text = String::new();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let line = text.lines().next().unwrap_or("").trim().to_string();
        if !output.status.success() && line.is_empty() {
            return Err(format!("退出码 {:?} 且无输出", output.status.code()));
        }
        Ok(line.chars().take(120).collect())
    }

    fn spawn(&mut self, request: &StartRequest) -> Result<String, String> {
        let mut command = Command::new(&self.config.executable);
        command
            .args(&self.config.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &self.config.working_dir {
            command.current_dir(dir);
        }
        let mut child = command.spawn().map_err(|err| err.to_string())?;
        let sink: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, Arc::clone(&sink));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, Arc::clone(&sink));
        }
        let execution_id = format!("local-process:{}", child.id());
        self.children.insert(
            request.invocation_id.clone(),
            ManagedChild {
                child,
                sink,
                sequence: 0,
                started_at_ms: now_ms(),
            },
        );
        Ok(execution_id)
    }

    /// 刷新进程状态：已退出则记录退出码并映射为 Completed/Failed。
    fn refresh(&mut self, invocation_id: &str) -> Option<ExecutionState> {
        let entry = self.children.get_mut(invocation_id)?;
        match entry.child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                self.exits.insert(invocation_id.to_string(), code);
                let cancelled = self.cancelled.get(invocation_id).copied().unwrap_or(false);
                Some(if cancelled {
                    ExecutionState::Cancelled
                } else {
                    map_process_exit(code).state
                })
            }
            Ok(None) => Some(ExecutionState::Running),
            Err(_) => Some(ExecutionState::Unknown),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn spawn_reader<R: Read + Send + 'static>(stream: R, sink: Arc<Mutex<Vec<String>>>) {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    if let Ok(mut guard) = sink.lock() {
                        guard.push(text);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

impl StartTransport for LocalProcessAdapter {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn start(&mut self, request: &StartRequest) -> StartOutcome {
        // 适配器自身没有持久去重：同 ID 已存在时视为重放，交给宿主的记录判定。
        if self.children.contains_key(&request.invocation_id) {
            let execution_id = self.children[&request.invocation_id].child.id().to_string();
            return StartOutcome::Replayed {
                execution_id: format!("local-process:{execution_id}"),
            };
        }
        match self.spawn(request) {
            Ok(execution_id) => StartOutcome::Accepted { execution_id },
            Err(err) => StartOutcome::Unavailable {
                reason: format!("启动本机进程失败：{err}"),
            },
        }
    }

    fn inspect(&mut self, invocation_id: &str) -> Option<ExecutionState> {
        self.refresh(invocation_id)
    }

    fn observations(&mut self, invocation_id: &str) -> Vec<ObservationEnvelope> {
        let Some(entry) = self.children.get_mut(invocation_id) else {
            return Vec::new();
        };
        let lines = entry
            .sink
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default();
        let mut envelopes = Vec::with_capacity(lines.len());
        for line in lines {
            entry.sequence += 1;
            envelopes.push(ObservationEnvelope {
                invocation_id: invocation_id.to_string(),
                source_epoch: 1,
                stream_id: "log".to_string(),
                source_sequence: entry.sequence,
                observed_at_ms: entry.started_at_ms,
                kind: ObservationType::Log,
                payload: line,
                origin: ADAPTER_ID.to_string(),
            });
        }
        envelopes
    }

    fn cancel(&mut self, invocation_id: &str) -> CancelStage {
        let Some(entry) = self.children.get_mut(invocation_id) else {
            // 没有句柄就无法确认停止：只报受理，不假装成功。
            return CancelStage::Requested;
        };
        if entry.child.try_wait().ok().flatten().is_some() {
            self.cancelled.insert(invocation_id.to_string(), true);
            return CancelStage::Confirmed;
        }
        let killed = entry.child.kill().is_ok();
        let waited = entry.child.wait().is_ok();
        self.cancelled.insert(invocation_id.to_string(), true);
        if killed && waited {
            CancelStage::Confirmed
        } else {
            CancelStage::Requested
        }
    }

    fn probe(&self, now_ms: i64) -> Option<ProbeReport> {
        Some(LocalProcessAdapter::probe(self, now_ms))
    }

    fn fault_injection(&self) -> bool {
        false
    }
}

impl Drop for LocalProcessAdapter {
    fn drop(&mut self) {
        for entry in self.children.values_mut() {
            let _ = entry.child.kill();
            let _ = entry.child.wait();
        }
    }
}

/// 等待子进程退出并返回其最终状态（仅测试与 CLI 收尾使用）。
pub fn wait_until_terminal(
    adapter: &mut LocalProcessAdapter,
    invocation_id: &str,
    timeout: Duration,
) -> Option<ExecutionState> {
    let step = Duration::from_millis(20);
    let mut waited = Duration::ZERO;
    loop {
        if let Some(state) = adapter.inspect(invocation_id)
            && state.is_terminal()
        {
            return Some(state);
        }
        if waited >= timeout {
            return None;
        }
        thread::sleep(step);
        waited += step;
    }
}

fn build_descriptor(config: &LocalProcessConfig, now_ms: i64) -> AdapterDescriptor {
    let claim = |slot: CapabilitySlot,
                 level: SupportLevel,
                 mode: &str,
                 source: EvidenceSource,
                 evidence: EvidenceLevel,
                 reason: &str| CapabilityClaim {
        slot,
        level,
        mode: Some(mode.to_string()),
        source,
        evidence_level: evidence,
        observed_at_ms: now_ms,
        expires_at_ms: None,
        reason: Some(reason.to_string()),
    };
    let unsupported = |slot: CapabilitySlot, mode: &str, reason: &str| {
        claim(
            slot,
            SupportLevel::Unsupported,
            mode,
            EvidenceSource::AdapterSelfDeclared,
            EvidenceLevel::SelfDeclared,
            reason,
        )
    };

    AdapterDescriptor {
        protocol: PROTOCOL_ID.to_string(),
        adapter_id: ADAPTER_ID.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        capability_id: "bod.local-process.v1".to_string(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        transport: "stdio".to_string(),
        effect_class: EffectClass::ProcessExecution,
        operations: vec![
            Operation::Describe,
            Operation::Probe,
            Operation::Start,
            Operation::Inspect,
            Operation::Observe,
            Operation::Cancel,
            Operation::Reconcile,
        ],
        observe: ObserveTransport::Poll,
        capabilities: vec![
            unsupported(
                CapabilitySlot::ModelControl,
                "opaque",
                "模型与提示词由被启动的程序自行决定，宿主不控制路由",
            ),
            unsupported(
                CapabilitySlot::ToolControl,
                "opaque",
                "工具调用发生在子进程内部，宿主只观察 stdout/stderr",
            ),
            unsupported(
                CapabilitySlot::WorkspaceIsolation,
                "none",
                "只设置独立工作目录，不做文件系统隔离或沙箱",
            ),
            unsupported(
                CapabilitySlot::Metering,
                "none",
                "本机进程不提供用量与计费口径，宿主只记录墙钟时间",
            ),
            claim(
                CapabilitySlot::Cancellation,
                SupportLevel::Supported,
                "child_process",
                EvidenceSource::TestResult,
                EvidenceLevel::RuntimeObserved,
                "终止直接子进程并以 wait 状态确认停止；不保证清理其派生子进程",
            ),
            unsupported(
                CapabilitySlot::Recovery,
                "none",
                "子进程无会话恢复语义，重启只能重新派发",
            ),
            unsupported(
                CapabilitySlot::ExternalIdempotency,
                "none",
                "任意脚本没有持久去重，重发可能重复副作用；去重由宿主记录承担",
            ),
            unsupported(
                CapabilitySlot::ReliableObservation,
                "poll",
                "轮询 stdout/stderr 无法回放，宿主重启期间的事件可能丢失",
            ),
            unsupported(
                CapabilitySlot::QuotaWindows,
                "none",
                "没有服务方配额窗口可查询",
            ),
        ],
        dedup_retention_ms: None,
        limitations: vec![
            format!(
                "单次只运行配置好的入口命令（{} {}），不支持按任务替换参数",
                config.executable,
                config.args.join(" ")
            ),
            "进程退出码只代表进程结果，验收由 D5 接受流程判定".to_string(),
        ],
    }
}

/// CLI 与测试的默认入口：探测本机运行时，并配置一段真实保活脚本。
pub fn default_adapter(now_ms: i64) -> Option<LocalProcessAdapter> {
    let config = LocalProcessConfig::detect_default()?;
    let hint = config.executable.clone();
    let mut adapter = LocalProcessAdapter::new(config, now_ms);
    adapter.set_args(LocalProcessConfig::keep_alive_script(&hint), now_ms);
    Some(adapter)
}

/// 把本机适配器的描述符序列化为可校验 JSON（供 CLI 与矩阵文档使用）。
pub fn descriptor_json(descriptor: &AdapterDescriptor) -> String {
    serde_json::json!({
        "protocol": descriptor.protocol,
        "adapter_id": descriptor.adapter_id,
        "adapter_version": descriptor.adapter_version,
        "capability_id": descriptor.capability_id,
        "platform": descriptor.platform,
        "transport": descriptor.transport,
        "effect_class": descriptor.effect_class.as_str(),
        "operations": descriptor.operations.iter().map(|op| op.as_str()).collect::<Vec<_>>(),
        "observe": descriptor.observe.as_str(),
        "dedup_retention_ms": descriptor.dedup_retention_ms,
        "capabilities": descriptor.capabilities.iter().map(|claim| serde_json::json!({
            "name": claim.slot.as_str(),
            "level": claim.level.as_str(),
            "mode": claim.mode,
            "source": claim.source.as_str(),
            "evidence_level": claim.evidence_level.as_str(),
            "observed_at_ms": claim.observed_at_ms,
            "expires_at_ms": claim.expires_at_ms,
            "reason": claim.reason,
        })).collect::<Vec<_>>(),
        "limitations": descriptor.limitations,
    })
    .to_string()
}

/// 自检入口：本机适配器描述符必须能被同一套 schema 解析回来（证明 describe 与解析一致）。
pub fn descriptor_round_trip(
    config: &LocalProcessConfig,
    now_ms: i64,
) -> Result<AdapterDescriptor, String> {
    let descriptor = build_descriptor(config, now_ms);
    descriptor.validate().map_err(|err| err.to_string())?;
    parse_descriptor(&descriptor_json(&descriptor)).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn detected() -> LocalProcessAdapter {
        LocalProcessAdapter::detect(1_763_500_000_000)
            .expect("本机应至少有一个可用的 python3/python/node")
    }

    fn request(invocation_id: &str) -> StartRequest {
        StartRequest {
            invocation_id: invocation_id.to_string(),
            run_id: "run-real".to_string(),
            work_package_revision: "wp-real".to_string(),
            attempt_id: "attempt-real".to_string(),
            binding_id: "binding-real".to_string(),
            fencing_token: 3,
            agent_ref: "agent-real".to_string(),
            runtime_config_revision: "rt-real".to_string(),
            context_bundle_ref: "ctx-real".to_string(),
            workspace_ref: "ws-real".to_string(),
            grant_ref: "grant-real".to_string(),
            reservation_refs: vec![],
            deadline_at_ms: None,
        }
    }

    #[test]
    fn probe_really_runs_the_executable() {
        let adapter = detected();
        let report = adapter.probe(1_763_500_000_000);
        let env = report
            .item(ProbeAspect::Environment)
            .expect("环境项必须存在");
        assert_eq!(
            env.status,
            ProbeStatus::Ready,
            "本机运行时应当就绪: {report:?}"
        );
        assert!(
            env.reason.contains("--version"),
            "环境结论必须给出真实探测依据: {}",
            env.reason
        );
        // 凭据与模型如实标 unsupported，不冒充可用。
        assert_eq!(
            report
                .item(ProbeAspect::Credentials)
                .map(|item| item.status),
            Some(ProbeStatus::Unsupported)
        );
        assert_eq!(
            report.item(ProbeAspect::Model).map(|item| item.status),
            Some(ProbeStatus::Unsupported)
        );
        assert!(report.blocking_items().is_empty());
    }

    #[test]
    fn describe_is_valid_and_honest_about_missing_capabilities() {
        let adapter = detected();
        let descriptor = adapter.descriptor();
        descriptor.validate().expect("描述符必须自洽");
        assert_eq!(descriptor.adapter_id, ADAPTER_ID);
        assert!(descriptor.supports(Operation::Start));
        assert!(descriptor.supports(Operation::Inspect));
        assert_eq!(
            descriptor.level_of(CapabilitySlot::ExternalIdempotency),
            SupportLevel::Unsupported
        );
        assert_eq!(
            descriptor.level_of(CapabilitySlot::ReliableObservation),
            SupportLevel::Unsupported
        );
        assert_eq!(
            descriptor.level_of(CapabilitySlot::Cancellation),
            SupportLevel::Supported
        );
        // 任何 supported 声明都必须有 probe 级及以上证据。
        for claim in &descriptor.capabilities {
            if claim.level == SupportLevel::Supported {
                assert!(claim.evidence_level.rank() >= EvidenceLevel::ProbeVerified.rank());
                assert!(claim.reason.is_some());
            }
        }
    }

    #[test]
    fn start_observe_inspect_round_trip_on_real_process() {
        let mut adapter = detected();
        // 用真实解释器跑一段真实脚本。
        let hint = adapter.config().executable.clone();
        let args = LocalProcessConfig::keep_alive_script(&hint);
        adapter.set_args(args, 1_763_500_000_000);
        let request = request("inv-real-observe");

        let outcome = adapter.start(&request);
        let execution_id = match outcome {
            StartOutcome::Accepted { execution_id } => execution_id,
            other => panic!("真实进程应被接受，实际 {other:?}"),
        };
        assert!(execution_id.starts_with("local-process:"));

        // 观察到真实 stdout 行。
        let mut lines: Vec<String> = Vec::new();
        for _ in 0..100 {
            for envelope in adapter.observations("inv-real-observe") {
                lines.push(envelope.payload);
            }
            if !lines.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            lines.iter().any(|line| line.contains("ready")),
            "应观察到真实 stdout 输出: {lines:?}"
        );

        let final_state =
            wait_until_terminal(&mut adapter, "inv-real-observe", Duration::from_secs(10))
                .expect("进程应在超时前退出");
        assert_eq!(final_state, ExecutionState::Completed);
        assert_eq!(adapter.exit_code("inv-real-observe"), Some(0));
        // 退出码 0 只映射为 Completed，验收信号仍是待验证。
        assert_eq!(
            map_process_exit(0).acceptance,
            crate::domain::execution::AcceptanceSignal::PendingVerification
        );
    }

    #[test]
    fn cancel_confirms_stop_of_real_process() {
        let mut adapter = detected();
        let hint = adapter.config().executable.clone();
        let args = LocalProcessConfig::keep_alive_script(&hint);
        adapter.set_args(args, 1_763_500_000_000);
        let request = request("inv-real-cancel");
        assert!(matches!(
            adapter.start(&request),
            StartOutcome::Accepted { .. }
        ));
        assert_eq!(
            adapter.inspect("inv-real-cancel"),
            Some(ExecutionState::Running)
        );

        let stage = adapter.cancel("inv-real-cancel");
        assert_eq!(stage, CancelStage::Confirmed);
        assert!(stage.stop_confirmed());
        assert_eq!(
            adapter.inspect("inv-real-cancel"),
            Some(ExecutionState::Cancelled)
        );
    }

    #[test]
    fn missing_invocation_cancel_is_only_requested() {
        let mut adapter = detected();
        assert_eq!(adapter.cancel("inv-unknown"), CancelStage::Requested);
        assert!(!adapter.cancel("inv-unknown").stop_confirmed());
    }

    #[test]
    fn descriptor_round_trips_through_schema() {
        let config = LocalProcessConfig::detect_default().expect("本机应有运行时");
        let parsed = descriptor_round_trip(&config, 1_763_500_000_000).expect("必须可解析");
        assert_eq!(parsed.adapter_id, ADAPTER_ID);
        assert_eq!(parsed.operations.len(), 7);
    }
}
