//! 桌面生命周期、单宿主与平台组合判定（G-51；§8.5、§16.1、§37.9、I-77）。
//!
//! 本模块只放**可测的决策**，不做 IO：连接归属、退出顺序、单宿主租约、sidecar 分发与
//! 平台支持声明。真正拉起 Tauri/WebView 的部分在 `apps/desktop`（feature `tauri-shell`），
//! 这里刻意不依赖 Tauri，桌面壳与测试驱动同一份判定。
//!
//! 四条不可让步的规则：
//! 1. 桌面壳**不写业务 DB、不另跑调度**：它只能连宿主，业务事实一律由宿主落库（I-77）；
//! 2. 同一 Company 同时只有一个权威宿主：第二个实例必须被拒，而不是"都能连"；
//! 3. 退出顺序固定：先停新工作 → 等在途结束 → 断连/停宿主 → 交还租约；
//!    外部已有宿主**永不**出现在停止步骤里；
//! 4. 没有该平台的真机 conformance 报告，就不宣称 Supported ——声明不等于验证。
//!
//! 为什么"planned"：本模块给出的是判定与自证，不构成 G-51 的真实平台证据。
//! 平台组合矩阵需要在每个目标平台真机跑一遍窗口/WebView/sidecar 才能升级支持声明。

use std::collections::BTreeMap;

/// 生命周期报告 schema；格式变化必须换版本。
pub const LIFECYCLE_SCHEMA: &str = "bod.desktop.lifecycle/v1";

// ---------------------------------------------------------------------------
// 连接归属与退出顺序
// ---------------------------------------------------------------------------

/// 连接归属：本次桌面是否自己启动了宿主。
///
/// 这个标记决定退出时**能不能**停止宿主，不是"当前连的是谁"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOwnership {
    /// 连上的是已经存在的宿主（用户手动启动、上一轮桌面留下、其他实例启动）。
    Attached,
    /// 宿主由本次桌面启动：只有这种归属才允许 `stop_host`。
    SpawnedByDesktop,
}

impl HostOwnership {
    pub fn owns_host(self) -> bool {
        matches!(self, Self::SpawnedByDesktop)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::SpawnedByDesktop => "spawned_by_desktop",
        }
    }
}

/// 关闭窗口时的动作（对应 `desktop.close_action`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitAction {
    /// 默认：只断开连接，宿主继续运行。
    Detach,
    /// 仅当宿主由本次启动时生效；外部宿主一律按 Detach 处理。
    StopOwnedHost,
}

impl ExitAction {
    /// 未知取值回落到最保守的 `Detach`：配置写错不该变成"关窗口顺手停别人的宿主"。
    pub fn parse(value: &str) -> Self {
        match value {
            "stop_host" | "stop_owned_host" => Self::StopOwnedHost,
            _ => Self::Detach,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detach => "detach",
            Self::StopOwnedHost => "stop_host",
        }
    }

    /// 从生效配置取动作，避免桌面壳与配置各写一份字符串解析后漂移。
    pub fn from_config(config: crate::config::CloseAction) -> Self {
        match config {
            crate::config::CloseAction::Detach => Self::Detach,
            crate::config::CloseAction::StopHost => Self::StopOwnedHost,
        }
    }
}

/// 退出的有序步骤。顺序本身就是规格：先停止接受新工作，再等在途结束，
/// 然后才断连/停宿主，最后交还租约。倒过来会丢在途工作的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExitStep {
    /// 停止接受新工作与新的 Agent 派发。
    StopAcceptingNewWork,
    /// 等在途尝试到达可恢复点（完成或落为可接续状态）。
    AwaitInflight,
    /// 断开与宿主的连接（不停止宿主）。
    CloseConnection,
    /// 请求宿主停止——只有自己启动且显式选择时才出现。
    StopOwnedHost,
    /// 交还单宿主租约，允许下一个实例接管。
    ReleaseHostLease,
}

/// 计算退出步骤。外部宿主**永远不会**出现 [`ExitStep::StopOwnedHost`]。
pub fn exit_plan(ownership: HostOwnership, action: ExitAction) -> Vec<ExitStep> {
    let mut steps = vec![
        ExitStep::StopAcceptingNewWork,
        ExitStep::AwaitInflight,
        ExitStep::CloseConnection,
    ];
    if ownership.owns_host() && action == ExitAction::StopOwnedHost {
        steps.push(ExitStep::StopOwnedHost);
    }
    steps.push(ExitStep::ReleaseHostLease);
    steps
}

/// 退出结果：桌面壳据此记录一条事实，而不是"关掉了"这种模糊结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitOutcome {
    pub steps: Vec<ExitStep>,
    /// 是否真的发送了停止请求。
    pub stopped_host: bool,
    /// 是否因为"外部宿主 + stop_host 配置"而把动作降级为 detach。
    pub downgraded: bool,
    pub detail: String,
}

/// 按计划执行退出，返回可核对的结果。`stop` 只在允许停止时被调用一次。
pub fn run_exit<F>(
    ownership: HostOwnership,
    action: ExitAction,
    mut stop: F,
) -> Result<ExitOutcome, String>
where
    F: FnMut() -> Result<(), String>,
{
    let steps = exit_plan(ownership, action);
    let wanted_stop = steps.contains(&ExitStep::StopOwnedHost);
    let downgraded = action == ExitAction::StopOwnedHost && !ownership.owns_host();
    if wanted_stop {
        stop()?;
    }
    let detail = if wanted_stop {
        "已请求停止本次启动的宿主".to_string()
    } else if downgraded {
        "外部已有宿主：stop_host 降级为 detach，宿主继续运行".to_string()
    } else {
        "detach：断开连接，宿主继续运行".to_string()
    };
    Ok(ExitOutcome {
        steps,
        stopped_host: wanted_stop,
        downgraded,
        detail,
    })
}

// ---------------------------------------------------------------------------
// 单宿主：同一 Company 只有一个权威宿主
// ---------------------------------------------------------------------------

/// 一个宿主实例的租约。字段都是可核对事实：谁、哪个实例、哪个 pid、什么归属、何时登记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLease {
    pub company_id: String,
    pub instance_id: String,
    pub pid: u32,
    pub ownership: HostOwnership,
    pub claimed_at_ms: i64,
}

/// 申请租约被拒：同一 Company 已有其他实例持有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConflict {
    pub holder: HostLease,
}

impl std::fmt::Display for HostConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Company {} 已有权威宿主 instance={} pid={}（租约登记于 {} ms），不得并行启动第二个",
            self.holder.company_id,
            self.holder.instance_id,
            self.holder.pid,
            self.holder.claimed_at_ms
        )
    }
}

/// 证明"原宿主已不在"的证据。**没有证据不许抢占**：超时不是证据，
/// 否则会造出双主（旧宿主还活着，新宿主已经开始写同一份库）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLoss {
    /// 观察到持有租约的那个 pid 已退出。
    ProcessExited { observed_pid: u32 },
    /// 运维显式确认（记录操作者与原因）；用于无法观察 pid 的容器/远程场景。
    OperatorConfirmed { actor: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimError {
    /// 该 Company 没有租约。
    NoLease { company_id: String },
    /// 租约实例与证据不匹配：不能拿别的实例的退出证明来抢占。
    EvidenceMismatch {
        lease_instance: String,
        evidence: String,
    },
}

impl std::fmt::Display for ReclaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLease { company_id } => write!(f, "Company {company_id} 没有宿主租约可回收"),
            Self::EvidenceMismatch {
                lease_instance,
                evidence,
            } => write!(
                f,
                "回收证据 {evidence} 与租约实例 {lease_instance} 不匹配，拒绝抢占"
            ),
        }
    }
}

/// 单宿主的进程内登记簿。跨进程的权威性由宿主自己的 DB 锁保证，这里负责
/// "同一进程内不重复启动 + 退出可核对"。
#[derive(Debug, Default)]
pub struct SingleHostGuard {
    leases: BTreeMap<String, HostLease>,
}

impl SingleHostGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 申请成为该 Company 的权威宿主。同实例重复申请是幂等的；
    /// 不同实例一律拒绝，并把当前持有者返回给界面显示。
    pub fn acquire(&mut self, lease: HostLease) -> Result<HostLease, HostConflict> {
        if let Some(existing) = self.leases.get(&lease.company_id) {
            if existing.instance_id == lease.instance_id {
                return Ok(existing.clone());
            }
            return Err(HostConflict {
                holder: existing.clone(),
            });
        }
        self.leases.insert(lease.company_id.clone(), lease.clone());
        Ok(lease)
    }

    pub fn holder(&self, company_id: &str) -> Option<&HostLease> {
        self.leases.get(company_id)
    }

    /// 交还租约。只有持有者本人能交还，避免 A 实例把 B 实例的租约删掉。
    pub fn release(&mut self, company_id: &str, instance_id: &str) -> bool {
        match self.leases.get(company_id) {
            Some(lease) if lease.instance_id == instance_id => {
                self.leases.remove(company_id);
                true
            }
            _ => false,
        }
    }

    /// 用证据回收失效租约。证据必须指向同一个 pid / 同一实例，否则拒绝。
    pub fn reclaim(
        &mut self,
        company_id: &str,
        evidence: HostLoss,
    ) -> Result<HostLease, ReclaimError> {
        let Some(lease) = self.leases.get(company_id) else {
            return Err(ReclaimError::NoLease {
                company_id: company_id.to_string(),
            });
        };
        let matched = match &evidence {
            HostLoss::ProcessExited { observed_pid } => *observed_pid == lease.pid,
            HostLoss::OperatorConfirmed { actor, reason } => {
                !actor.trim().is_empty() && !reason.trim().is_empty()
            }
        };
        if !matched {
            return Err(ReclaimError::EvidenceMismatch {
                lease_instance: lease.instance_id.clone(),
                evidence: format!("{evidence:?}"),
            });
        }
        Ok(self
            .leases
            .remove(company_id)
            .expect("上面已确认存在该租约"))
    }
}

// ---------------------------------------------------------------------------
// 桌面壳职责：不写业务 DB、不另跑调度
// ---------------------------------------------------------------------------

/// 桌面壳允许做的事。枚举本身就是边界：没有"直接写库"这个选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellOperation {
    /// 打开/关闭窗口与 WebView。
    WindowLifecycle,
    /// 连接宿主、握手、断开。
    HostConnection,
    /// 向宿主转发用户命令（由宿主做授权与落库）。
    ForwardCommand,
}

/// 壳试图越界时返回的违规说明。
pub fn guard_shell_operation(operation: ShellOperation) -> Result<(), String> {
    match operation {
        ShellOperation::WindowLifecycle
        | ShellOperation::HostConnection
        | ShellOperation::ForwardCommand => Ok(()),
    }
}

/// 壳不得直接执行的动作：写业务库、另起调度、自己发 Agent 派发。
///
/// 这是白名单的反面表达，便于桌面壳在接到"顺手写一下库"的需求时立刻被拒。
pub fn guard_shell_write(target: &str) -> Result<(), String> {
    Err(format!(
        "桌面壳不得直接写 {target}：业务事实必须经宿主写入，壳只负责窗口与连接（I-77）"
    ))
}

// ---------------------------------------------------------------------------
// sidecar 分发与平台组合
// ---------------------------------------------------------------------------

/// 平台键：OS + 架构。WebView 运行时由平台决定，不单独作为维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlatformKey {
    pub os: &'static str,
    pub arch: &'static str,
}

impl PlatformKey {
    pub const fn new(os: &'static str, arch: &'static str) -> Self {
        Self { os, arch }
    }

    /// 当前编译目标；用于把"本机能否跑"与"是否已验证"分开。
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    pub fn as_str(self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
}

/// sidecar 分发条目。摘要必填：没有摘要就没法判断下载到的二进制是不是我们发的那份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarEntry {
    pub platform: PlatformKey,
    pub artifact: String,
    pub digest: String,
    pub bytes: u64,
}

/// 分发清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarManifest {
    pub version: String,
    /// 宿主与桌面壳的最低匹配版本；不匹配时宁可拒绝连接，也不冒协议漂移的风险。
    pub min_host_version: String,
    pub entries: Vec<SidecarEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformGap {
    /// 清单里没有该平台：不可宣称支持。
    NoSidecar { platform: String },
    /// 有条目但摘要为空：等于没有校验依据。
    MissingDigest { artifact: String },
}

impl std::fmt::Display for PlatformGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSidecar { platform } => {
                write!(f, "sidecar 清单没有 {platform} 的条目，该平台不得宣称支持")
            }
            Self::MissingDigest { artifact } => {
                write!(f, "sidecar 条目 {artifact} 缺少摘要，无法校验完整性")
            }
        }
    }
}

impl SidecarManifest {
    /// 取该平台的 sidecar 条目；缺失或摘要为空都返回可显示的缺口，不返回"默认值"。
    pub fn resolve(&self, platform: PlatformKey) -> Result<&SidecarEntry, PlatformGap> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.platform == platform)
            .ok_or_else(|| PlatformGap::NoSidecar {
                platform: platform.as_str(),
            })?;
        if entry.digest.trim().is_empty() {
            return Err(PlatformGap::MissingDigest {
                artifact: entry.artifact.clone(),
            });
        }
        Ok(entry)
    }

    /// 宿主版本是否满足清单要求。只比"是否声明了匹配"，不做语义版本推断。
    pub fn host_version_matches(&self, host_version: &str) -> bool {
        host_version == self.min_host_version
    }
}

/// 一条平台 conformance 记录。`report` 是真实报告的标识（文件/链接/编号），
/// 空字符串视为"没有报告"，不能算验证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceCase {
    pub platform: PlatformKey,
    pub framework: &'static str,
    pub passed: bool,
    pub report: String,
}

/// 支持等级。**没有 Supported 变体**：结论只能是"测过且通过"或更低，
/// 避免某处代码把"声明"写成"支持"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupportLevel {
    /// 有真实报告且通过。
    Tested,
    /// 只有声明/清单条目，没有报告。
    DeclaredOnly,
    /// 清单里没有该平台。
    Unsupported,
}

impl SupportLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tested => "tested",
            Self::DeclaredOnly => "declared_only",
            Self::Unsupported => "unsupported",
        }
    }

    pub fn is_tested(&self) -> bool {
        matches!(self, Self::Tested)
    }
}

/// 判定某平台的支持等级：先看清单有没有条目，再看有没有通过的真实报告。
pub fn support_level(
    platform: PlatformKey,
    manifest: &SidecarManifest,
    cases: &[ConformanceCase],
) -> SupportLevel {
    if manifest.resolve(platform).is_err() {
        return SupportLevel::Unsupported;
    }
    let tested = cases.iter().any(|case| {
        case.platform == platform
            && case.passed
            && !case.report.trim().is_empty()
            && !case.framework.trim().is_empty()
    });
    if tested {
        SupportLevel::Tested
    } else {
        SupportLevel::DeclaredOnly
    }
}

/// 宣称"该平台已支持"的准入。没有通过的报告一律拒绝，并说明缺什么。
pub fn claim_supported(
    platform: PlatformKey,
    manifest: &SidecarManifest,
    cases: &[ConformanceCase],
) -> Result<String, String> {
    match support_level(platform, manifest, cases) {
        SupportLevel::Tested => {
            let report = cases
                .iter()
                .find(|case| {
                    case.platform == platform && case.passed && !case.report.trim().is_empty()
                })
                .map(|case| case.report.clone())
                .unwrap_or_default();
            Ok(report)
        }
        SupportLevel::DeclaredOnly => Err(format!(
            "{} 只有清单条目、没有通过的 conformance 报告：只能标 declared_only",
            platform.as_str()
        )),
        SupportLevel::Unsupported => Err(manifest
            .resolve(platform)
            .expect_err("Unsupported 意味着 resolve 失败")
            .to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(company: &str, instance: &str, pid: u32) -> HostLease {
        HostLease {
            company_id: company.to_string(),
            instance_id: instance.to_string(),
            pid,
            ownership: HostOwnership::SpawnedByDesktop,
            claimed_at_ms: 1_763_500_000_000,
        }
    }

    fn manifest() -> SidecarManifest {
        SidecarManifest {
            version: "0.1.0".to_string(),
            min_host_version: "0.1.0".to_string(),
            entries: vec![
                SidecarEntry {
                    platform: PlatformKey::new("windows", "x86_64"),
                    artifact: "bod-0.1.0-x86_64-pc-windows-msvc.zip".to_string(),
                    digest: "sha256:aa".to_string(),
                    bytes: 1024,
                },
                SidecarEntry {
                    platform: PlatformKey::new("linux", "x86_64"),
                    artifact: "bod-0.1.0-x86_64-unknown-linux-gnu.tar.gz".to_string(),
                    digest: "sha256:bb".to_string(),
                    bytes: 2048,
                },
            ],
        }
    }

    #[test]
    fn exit_plan_never_stops_a_host_it_does_not_own() {
        let detach = exit_plan(HostOwnership::Attached, ExitAction::Detach);
        let stop = exit_plan(HostOwnership::Attached, ExitAction::StopOwnedHost);
        assert_eq!(detach, stop, "外部宿主：两种配置都只有 detach 路径");
        assert!(!stop.contains(&ExitStep::StopOwnedHost));

        let mut called = 0;
        let outcome = run_exit(HostOwnership::Attached, ExitAction::StopOwnedHost, || {
            called += 1;
            Ok(())
        })
        .expect("退出不应失败");
        assert_eq!(called, 0, "外部宿主不得被停止");
        assert!(outcome.downgraded, "必须如实记录 stop_host 被降级");
        assert!(!outcome.stopped_host);
        assert!(outcome.detail.contains("降级"));
    }

    #[test]
    fn exit_order_stops_new_work_waits_inflight_then_releases_lease() {
        let steps = exit_plan(HostOwnership::SpawnedByDesktop, ExitAction::StopOwnedHost);
        let index = |step: ExitStep| steps.iter().position(|value| *value == step).unwrap();
        assert!(index(ExitStep::StopAcceptingNewWork) < index(ExitStep::AwaitInflight));
        assert!(
            index(ExitStep::AwaitInflight) < index(ExitStep::CloseConnection),
            "在途工作结束前就断连会丢结果"
        );
        assert!(index(ExitStep::CloseConnection) < index(ExitStep::StopOwnedHost));
        assert_eq!(
            steps.last(),
            Some(&ExitStep::ReleaseHostLease),
            "租约必须最后交还，否则下一个实例会在旧宿主还在时接管"
        );

        let mut called = 0;
        let outcome = run_exit(
            HostOwnership::SpawnedByDesktop,
            ExitAction::StopOwnedHost,
            || {
                called += 1;
                Ok(())
            },
        )
        .expect("退出不应失败");
        assert_eq!(called, 1, "自己启动的宿主只停止一次");
        assert!(outcome.stopped_host);
        assert!(!outcome.downgraded);
    }

    #[test]
    fn spawned_host_detach_keeps_it_running_for_next_attach() {
        let steps = exit_plan(HostOwnership::SpawnedByDesktop, ExitAction::Detach);
        assert!(!steps.contains(&ExitStep::StopOwnedHost));
        let outcome = run_exit(HostOwnership::SpawnedByDesktop, ExitAction::Detach, || {
            panic!("detach 不得调用停止")
        })
        .expect("退出不应失败");
        assert!(!outcome.stopped_host);
        assert_eq!(outcome.detail, "detach：断开连接，宿主继续运行");
    }

    #[test]
    fn unknown_close_action_falls_back_to_detach() {
        assert_eq!(ExitAction::parse("stop_host"), ExitAction::StopOwnedHost);
        assert_eq!(ExitAction::parse("detach"), ExitAction::Detach);
        assert_eq!(
            ExitAction::parse("kill_everything"),
            ExitAction::Detach,
            "未知值不得升级为停止宿主"
        );
        assert_eq!(
            ExitAction::from_config(crate::config::CloseAction::StopHost),
            ExitAction::StopOwnedHost,
            "配置里的 stop_host 必须能被表达"
        );
        assert_eq!(
            ExitAction::from_config(crate::config::CloseAction::Detach),
            ExitAction::Detach
        );
    }

    #[test]
    fn second_host_for_the_same_company_is_refused() {
        let mut guard = SingleHostGuard::new();
        guard
            .acquire(lease("company-1", "inst-a", 100))
            .expect("第一个宿主可登记");

        // 同实例重复登记是幂等的（重连、重新握手不该失败）。
        let again = guard
            .acquire(lease("company-1", "inst-a", 100))
            .expect("同实例重复登记应幂等");
        assert_eq!(again.instance_id, "inst-a");

        let conflict = guard
            .acquire(lease("company-1", "inst-b", 200))
            .expect_err("同 Company 第二个实例必须被拒");
        assert_eq!(conflict.holder.instance_id, "inst-a");
        assert!(conflict.to_string().contains("company-1"));
        assert_eq!(guard.holder("company-1").map(|l| l.pid), Some(100));

        // 不同 Company 互不影响。
        guard
            .acquire(lease("company-2", "inst-c", 300))
            .expect("不同 Company 可各自持有");
    }

    #[test]
    fn release_requires_the_holder_and_frees_the_slot() {
        let mut guard = SingleHostGuard::new();
        guard
            .acquire(lease("company-1", "inst-a", 100))
            .expect("登记");
        assert!(!guard.release("company-1", "inst-b"), "非持有者不得交还");
        assert!(guard.release("company-1", "inst-a"));
        assert!(guard.holder("company-1").is_none());
        guard
            .acquire(lease("company-1", "inst-b", 200))
            .expect("交还后下一个实例可接管");
    }

    #[test]
    fn stale_lease_is_reclaimed_only_with_matching_evidence() {
        let mut guard = SingleHostGuard::new();
        guard
            .acquire(lease("company-1", "inst-a", 100))
            .expect("登记");

        let mismatch = guard
            .reclaim(
                "company-1",
                HostLoss::ProcessExited {
                    observed_pid: 999, // 别的进程退出了，不能当作本宿主已死
                },
            )
            .expect_err("证据与租约不匹配必须拒绝");
        assert!(matches!(mismatch, ReclaimError::EvidenceMismatch { .. }));
        assert!(guard.holder("company-1").is_some(), "拒绝回收后租约仍在");

        let empty_operator = guard
            .reclaim(
                "company-1",
                HostLoss::OperatorConfirmed {
                    actor: "  ".to_string(),
                    reason: String::new(),
                },
            )
            .expect_err("没有操作者与原因的确认不算证据");
        assert!(matches!(
            empty_operator,
            ReclaimError::EvidenceMismatch { .. }
        ));

        let reclaimed = guard
            .reclaim("company-1", HostLoss::ProcessExited { observed_pid: 100 })
            .expect("持有租约的 pid 已退出即可回收");
        assert_eq!(reclaimed.instance_id, "inst-a");
        assert!(guard.holder("company-1").is_none());

        assert!(matches!(
            guard.reclaim("company-1", HostLoss::ProcessExited { observed_pid: 100 }),
            Err(ReclaimError::NoLease { .. })
        ));
    }

    #[test]
    fn shell_cannot_write_business_state_directly() {
        assert!(guard_shell_operation(ShellOperation::WindowLifecycle).is_ok());
        assert!(guard_shell_operation(ShellOperation::HostConnection).is_ok());
        assert!(guard_shell_operation(ShellOperation::ForwardCommand).is_ok());
        let error = guard_shell_write("业务 SQLite").expect_err("壳不得直接写业务库");
        assert!(error.contains("I-77"));
        assert!(guard_shell_write("调度循环").is_err());
    }

    #[test]
    fn sidecar_manifest_resolves_platform_or_reports_gap() {
        let manifest = manifest();
        let windows = manifest
            .resolve(PlatformKey::new("windows", "x86_64"))
            .expect("清单含 windows");
        assert!(windows.artifact.ends_with(".zip"));
        assert!(windows.digest.starts_with("sha256:"));

        let gap = manifest
            .resolve(PlatformKey::new("macos", "aarch64"))
            .expect_err("清单没有 macos-aarch64");
        assert!(gap.to_string().contains("macos-aarch64"));

        let mut broken = manifest.clone();
        broken.entries[0].digest = "  ".to_string();
        assert!(matches!(
            broken.resolve(PlatformKey::new("windows", "x86_64")),
            Err(PlatformGap::MissingDigest { .. })
        ));

        assert!(manifest.host_version_matches("0.1.0"));
        assert!(
            !manifest.host_version_matches("0.2.0"),
            "版本不匹配不猜兼容"
        );
    }

    #[test]
    fn platform_support_requires_a_passing_report_not_just_a_manifest_entry() {
        let manifest = manifest();
        let windows = PlatformKey::new("windows", "x86_64");
        let cases = vec![ConformanceCase {
            platform: windows,
            framework: "tauri2",
            passed: true,
            report: String::new(),
        }];

        assert_eq!(
            support_level(windows, &manifest, &cases),
            SupportLevel::DeclaredOnly,
            "有清单但没报告，只能算声明"
        );
        let error = claim_supported(windows, &manifest, &cases).expect_err("不得宣称支持");
        assert!(error.contains("declared_only"));

        let failed = vec![ConformanceCase {
            platform: windows,
            framework: "tauri2",
            passed: false,
            report: "docs/reports/desktop-win.md".to_string(),
        }];
        assert_eq!(
            support_level(windows, &manifest, &failed),
            SupportLevel::DeclaredOnly,
            "报告存在但没通过，同样不能算验证"
        );

        let passed = vec![ConformanceCase {
            platform: windows,
            framework: "tauri2",
            passed: true,
            report: "docs/reports/desktop-win.md".to_string(),
        }];
        assert_eq!(
            support_level(windows, &manifest, &passed),
            SupportLevel::Tested
        );
        assert_eq!(
            claim_supported(windows, &manifest, &passed).unwrap(),
            "docs/reports/desktop-win.md"
        );

        let macos = PlatformKey::new("macos", "aarch64");
        assert_eq!(
            support_level(macos, &manifest, &passed),
            SupportLevel::Unsupported
        );
        assert!(
            claim_supported(macos, &manifest, &passed)
                .expect_err("清单没有的平台不可宣称")
                .contains("macos-aarch64")
        );
    }

    #[test]
    fn current_platform_is_reported_as_a_key() {
        let key = PlatformKey::current();
        assert!(!key.os.is_empty() && !key.arch.is_empty());
        assert!(key.as_str().contains('-'));
    }
}
