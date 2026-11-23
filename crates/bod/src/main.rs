//! bod 宿主 CLI：P0 D0 阶段提供最小 headless 启动与配置 schema/explain 入口。
//! HTTP 控制面与真实宿主启动在后续切片接入；当前不启动服务、不写运行时数据。

use bod_app::adapter_conformance::{CaseStatus, enablement, run_fake_suite, run_live_suite};
use bod_app::capability_dispatch::{Candidate, RequiredGuarantees, select};
use bod_app::config::{BodConfig, FIELD_DOCS};
use bod_app::conformance_report::SectionedReport;
use bod_app::domain::execution::CapabilitySlot;
use bod_app::fake_executor::{FakeExecutor, FakeScenario};
use bod_app::handoff_package::HandoffLevel;
use bod_app::handoff_sample::{
    self, DurableSampleConfig, HumanTime, LocalSampleConfig, SampleReport, run_sample,
};
use bod_app::invocation::StartTransport;
use bod_app::local_process::{default_adapter, descriptor_json};
use bod_app::release_acceptance::{ReleaseAcceptance, ReleaseEvidence};
use bod_app::{explain, load_from_str, schema_text};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

mod smoke;

const USAGE: &str = "\
用法:
  bod                              显示 headless 骨架信息
  bod config schema                列出配置字段、类型与默认值
  bod config explain               解释内置默认值、来源与语义
  bod config explain --file <p>    解释指定 TOML 文件的生效值与来源
  bod config validate --file <p>   仅校验配置文件是否符合 schema
  bod adapter describe             输出本机真实适配器的描述符 JSON
  bod adapter probe                分项探测本机执行端（不调用模型、不付费）
  bod adapter capabilities         列出本机适配器逐项能力声明与有效期
  bod adapter capabilities --need l3  按 L1/L2/L3 的保证需求过滤候选并显示限制
  bod adapter conformance          运行 Fake + 本机真实适配器的 conformance 报告
  bod adapter release-check        发布验收：五项检查 + 日志脱敏 + 能力缺失三处一致
  bod adapter release-check --logs <p>  扫描该日志文件里的凭据值（命中即该项不通过）
  bod adapter release-check --need l3   同时比对探测/准入/界面三处的缺失能力
  bod adapter release-check --json      同上，输出机器可读结果
  bod conformance                  分项报告：按版本/协议/平台/能力/日期逐项列出，不出整体通过标记
  bod conformance --fake-only      只跑 Fake Executor 的分项报告
  bod conformance --json           同上，输出机器可读结果（多适配器时为 reports 数组）
  bod handoff sample               在真实运行配置上跑 L2 接续样本，给出双运行时证据结论
  bod handoff sample --level l1    只要求 L1（不验重启后重放）
  bod handoff sample --root <dir>  指定样本工作目录（默认系统临时目录下带时间戳的子目录）
  bod handoff sample --drill <dir> 引用真实演练记录里的人工耗时（唯一算 4.4 证据的来源）
  bod handoff sample --human-minutes 2.5  口述值：写进报告但标注未附演练记录，不算 4.4 证据
  bod handoff sample --json        同上，输出机器可读结果
  bod handoff drill start --root <d> --operator <名> --task <类别>   开始一次真实演练计时
  bod handoff drill mark --root <d> --step <档位> [--note <文本>]   打一个档位（时间戳由系统写）
  bod handoff drill finish --root <d>   封存：由 last_action→handoff_done 算出接管就绪耗时
  bod handoff drill show --root <d>     查看时间线与读数（--json 输出机器可读）
  bod handoff drill arm --root <d> --arm manual|bod --ready-seconds <s> --interruptions <n> \
      --duplicate-runs <n> --manual-minutes <m>   记录 5.2 对照实验的一侧读数
  bod smoke                        跑通指派→执行→候选登记→重启找回的最小冒烟
  bod smoke --json                 同上，输出机器可读结果
  bod fault-matrix                 执行 P0 五个核心故障场景并输出实测报告
  bod fault-matrix --json          同上，输出机器可读结果
  bod perf                         实测空闲负担与 §38.2 性能预算可测项
  bod verify-index                 输出 I/G 验证索引与 G-50 结论
  bod observe --db <path>          读现有表算 §9 观测读数（算不出的项单列，不填 0）
  bod observe --db <path> --company <id>   指定公司（默认 company-default）
  bod observe --db <path> --json   同上，输出机器可读结果
  bod tracking --db <path>         列交付跟踪项与目标数（只读；结论不在列表里给）
  bod tracking --db <path> --id <跟踪项>   看单个跟踪项的逐目标结论、证据与缺口
  bod tracking --db <path> --json [--id <跟踪项>]   机器可读；带 --id 时与 GET 的响应体同形
  bod tracking import --db <path> --key <幂等键> --project <项目> --locator <定位>
      登记一个外部来源并挂到跟踪项上（写命令：会建库、跑迁移）
      --connection <id> / --object-type <类型> / --provider-object-id <稳定 ID>  连接器来源的身份
      --title <标题> / --requirement <用户要求> / --owner <负责人>   缺省：标题取定位、负责人取导入者
      --correlation <id> / --decided-at <RFC3339>   审计用；缺省取本进程与当前时刻
      --json   机器可读；与重复执行时的重放结果同形
  bod tracking link --db <path> --id <跟踪项> --key <幂等键> --expected-version <版本>
      追加一条关联（写命令）：--source <来源 ID> 或 --work-package <工作包修订>，二选一
      --expected-version 取自 `bod tracking --id <跟踪项> --json` 的 version；
      版本不一致会拒绝（带当前版本），重读后带上新版本重试即可
      --target <客户>:<环境>[:<部署单元>]   可重复；已有目标按身份命中，缺的才新建
      --correlation <id> / --decided-at <RFC3339>   审计用
      --json   机器可读（含 link_id、appended、version、created_targets）
  bod tracking observe --db <path> --id <跟踪项> --key <幂等键>
      登记一条观察（写命令）：--target <客户>:<环境>[:<部署单元>] 恰好一个
      --kind deployment|rollback|manual-evidence   必填：部署与回滚是两件不同的事实
      --observed-at <RFC3339>   必填，业务时间（来源时间）；不默认取现在——晚到补录不能记成今天
      --source-kind connector|manual|internal   缺省：人工补证=manual，部署/回滚=connector
      --source-revision <来源修订>   声称 connector 时必填，否则「连接器观察」只是自报
      --source-ref <定位> / --revision <部署修订> / --digest <摘要> / --artifact-revision <制品修订>
      --note <备注>   人工补证的授权范围与复验依据（空备注由写入原语拒绝）
      本命令不写 authorized：授权范围由 §34.12 判定，未实现前一律 0
      --correlation <id> / --decided-at <RFC3339>   审计用
      --json   机器可读
  bod sources --db <path>          列已确认的来源身份与修订条数（只读；候选不计入）
  bod sources --db <path> --project <id>   只看某个项目的来源
  bod sources --db <path> --candidates     附上来源候选（pending 在前，未确认的不能支撑结论）
  bod sources --db <path> --json   同上，输出机器可读结果
  bod serve                        启动本地控制面并写出连接描述（受保护握手）
  bod serve --listen <addr>        指定监听地址（默认取 server.listen，可为 127.0.0.1:0）
  bod serve --descriptor <path>    指定连接描述路径（默认 $BOD_HOME/host.endpoint.json）
  bod serve --db <path>            只读挂载既有业务库，启用 GET /api/v1/delivery-tracking/{id}
  bod serve --company <id>         产品接口使用的公司（默认 company-default）
  bod serve --json                 就绪后输出机器可读的一行（不含任何令牌）
  bod --help                       显示本帮助
";

/// 实测环境必须如实记录：OS/架构 + 实际 rustc 版本；取不到就写 unknown，不猜。
fn environment_note() -> String {
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "{} / {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        rustc
    )
}

/// commit 从现有构建信息读取；取不到时写明 unknown，不伪造短哈希。
fn commit_note() -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

fn report_json(report: &bod_app::fault_matrix::FaultMatrixReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{{");
    let _ = writeln!(
        out,
        "  \"environment\": {},",
        json_string(&report.environment)
    );
    let _ = writeln!(out, "  \"commit\": {},", json_string(&report.commit));
    let _ = writeln!(out, "  \"passed\": {},", report.passed_count());
    let _ = writeln!(out, "  \"total\": {},", report.scenarios.len());
    let _ = writeln!(out, "  \"all_passed\": {},", report.all_passed());
    let _ = writeln!(out, "  \"scenarios\": [");
    for (index, scenario) in report.scenarios.iter().enumerate() {
        let comma = if index + 1 == report.scenarios.len() {
            ""
        } else {
            ","
        };
        let _ = writeln!(out, "    {{");
        let _ = writeln!(out, "      \"name\": {},", json_string(scenario.name));
        let _ = writeln!(out, "      \"fault\": {},", json_string(scenario.fault));
        let _ = writeln!(
            out,
            "      \"requirement\": {},",
            json_string(scenario.requirement)
        );
        let _ = writeln!(out, "      \"passed\": {},", scenario.passed);
        let _ = writeln!(out, "      \"facts\": {},", json_array(&scenario.facts));
        let _ = writeln!(
            out,
            "      \"assertions\": {},",
            json_array(&scenario.assertions)
        );
        let _ = writeln!(
            out,
            "      \"next_step\": {}",
            json_string(&scenario.next_step)
        );
        let _ = writeln!(out, "    }}{comma}");
    }
    let _ = writeln!(out, "  ]");
    out.push('}');
    out
}

/// JSON 字符串字面量只有一份实现（CR-001），见 `bod_app::json_text`。
fn json_string(value: &str) -> String {
    bod_app::json_text::json_string(value)
}

fn json_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|item| json_string(item)).collect();
    format!("[{}]", items.join(", "))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn adapter_or_err() -> Result<bod_app::local_process::LocalProcessAdapter, String> {
    default_adapter(now_ms()).ok_or_else(|| {
        "未找到本机可用运行时（尝试 python3 / python / node）；未就绪时不假装可用".to_string()
    })
}

fn read_config_file(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|err| format!("无法读取配置文件 {path}: {err}"))
}

fn run(args: &[String]) -> Result<String, String> {
    if args.first().map(String::as_str) == Some("serve") {
        return serve(&args[1..]);
    }
    match args {
        [] => Ok(format!(
            "bod {} (headless skeleton)\n配置入口: bod config explain | bod config schema\n",
            env!("CARGO_PKG_VERSION")
        )),
        [arg] if arg == "--help" || arg == "help" => Ok(USAGE.to_string()),
        [cmd, sub] if cmd == "config" && sub == "schema" => Ok(schema_text()),
        [cmd, sub] if cmd == "config" && sub == "explain" => {
            Ok(explain(&BodConfig::default(), &BTreeSet::new()))
        }
        [cmd, sub, flag, path]
            if cmd == "config" && (sub == "explain" || sub == "validate") && flag == "--file" =>
        {
            let text = read_config_file(path)?;
            let loaded = load_from_str(&text).map_err(|err| err.to_string())?;
            if sub == "explain" {
                Ok(explain(&loaded.config, &loaded.overridden))
            } else {
                Ok(format!(
                    "OK: {} 个字段通过 schema 校验，文件覆盖 {} 个字段。\n",
                    FIELD_DOCS.len(),
                    loaded.overridden.len()
                ))
            }
        }
        [cmd] if cmd == "smoke" => {
            let report = smoke::run()?;
            let mut out = String::new();
            let _ = writeln!(out, "冒烟结果: {}", report.result);
            let _ = writeln!(out, "  工作目录: {}", report.work_dir);
            let _ = writeln!(
                out,
                "  Issue {} → invocation {} → execution {}",
                report.issue, report.invocation, report.execution_id
            );
            let _ = writeln!(
                out,
                "  候选修订 {} 验收状态 {}（accepted={}）",
                report.candidate_revision, report.verification_state, report.accepted
            );
            let _ = writeln!(
                out,
                "  重启找回: {}，只读派发次数: {}，资源版本 {} → {}",
                report.restart_recovered,
                report.read_only_dispatches,
                report.resource_version_before,
                report.resource_version_after
            );
            for note in &report.notes {
                let _ = writeln!(out, "  - {note}");
            }
            Ok(out)
        }
        [cmd, flag] if cmd == "smoke" && flag == "--json" => {
            let report = smoke::run()?;
            Ok(format!("{}\n", report.to_json()))
        }
        [cmd] if cmd == "fault-matrix" => {
            let report = bod_app::fault_matrix::run(&environment_note(), &commit_note())?;
            Ok(bod_app::fault_matrix::render_markdown(&report))
        }
        [cmd, flag] if cmd == "fault-matrix" && flag == "--json" => {
            let report = bod_app::fault_matrix::run(&environment_note(), &commit_note())?;
            Ok(format!("{}\n", report_json(&report)))
        }
        [cmd] if cmd == "verify-index" => {
            let report =
                bod_app::verification_registry::g50_report().map_err(|err| err.to_string())?;
            let registry = bod_app::verification_registry::parse(
                bod_app::verification_registry::shipped_registry(),
            )
            .map_err(|err| err.to_string())?;
            let mut out = String::new();
            let _ = writeln!(out, "# I/G 验证索引（verification/gates.yaml）\n");
            let _ = writeln!(out, "| ID | 类型 | 状态 | 模块 | 规格语义 |");
            let _ = writeln!(out, "| --- | --- | --- | --- | --- |");
            for binding in &registry.bindings {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} |",
                    binding.id,
                    binding.kind.as_str(),
                    binding.status.as_str(),
                    if binding.module.is_empty() {
                        "—"
                    } else {
                        &binding.module
                    },
                    binding.requirement
                );
            }
            let _ = writeln!(out, "\n{}", report.summary());
            if !report.gaps.is_empty() {
                let _ = writeln!(out, "缺失引用: {}", report.gaps.join(", "));
            }
            for problem in &report.problems {
                let _ = writeln!(out, "结构问题: {problem}");
            }
            for binding in registry.bindings.iter().filter(|item| {
                item.status != bod_app::verification_registry::BindingStatus::Implemented
            }) {
                let _ = writeln!(out, "未完成 {}：{}", binding.id, binding.note);
            }
            let _ = writeln!(out, "G-50: {}", if report.ok() { "PASS" } else { "FAIL" });
            Ok(out)
        }
        [cmd] if cmd == "perf" => {
            let report = bod_app::perf_probe::run(&environment_note(), &commit_note())?;
            Ok(bod_app::capacity_evaluation::render_markdown(&report, &[]))
        }
        [cmd, rest @ ..] if cmd == "observe" => observe(rest),
        [cmd, rest @ ..] if cmd == "tracking" => tracking(rest),
        [cmd, rest @ ..] if cmd == "sources" => sources(rest),
        [cmd, sub] if cmd == "adapter" && sub == "describe" => {
            let adapter = adapter_or_err()?;
            Ok(format!("{}\n", descriptor_json(adapter.descriptor())))
        }
        [cmd, sub] if cmd == "adapter" && sub == "probe" => {
            let adapter = adapter_or_err()?;
            let report = adapter.probe(now_ms());
            let version = adapter
                .version_string()
                .unwrap_or_else(|err| format!("未取得版本: {err}"));
            let mut out = String::new();
            let _ = writeln!(
                out,
                "适配器 {} v{}  运行时: {}",
                report.adapter_id, report.adapter_version, version
            );
            for item in &report.items {
                let _ = writeln!(
                    out,
                    "  {:<12} {:<11} {}（scope={}, observed_at={}）",
                    item.aspect.as_str(),
                    item.status.as_str(),
                    item.reason,
                    item.scope,
                    item.observed_at_ms
                );
            }
            let _ = writeln!(out, "整体结论: {}", report.overall().as_str());
            if report.blocking_items().is_empty() {
                let _ = writeln!(out, "阻塞项: 无（不适用项不阻塞，但也不代表已验证）");
            } else {
                let blocking: Vec<&str> = report
                    .blocking_items()
                    .iter()
                    .map(|item| item.aspect.as_str())
                    .collect();
                let _ = writeln!(out, "阻塞项: {}", blocking.join(", "));
            }
            Ok(out)
        }
        [cmd, sub] if cmd == "adapter" && sub == "capabilities" => adapter_capabilities(None),
        [cmd, sub, flag, value]
            if cmd == "adapter" && sub == "capabilities" && flag == "--need" =>
        {
            adapter_capabilities(Some(value))
        }
        [cmd, sub] if cmd == "adapter" && sub == "conformance" => {
            let now = now_ms();
            let fake = run_fake_suite(now);
            let mut adapter = adapter_or_err()?;
            let version = adapter
                .version_string()
                .unwrap_or_else(|err| format!("未取得版本: {err}"));
            let live = run_live_suite(&mut adapter, now);
            let mut out = String::new();
            let _ = writeln!(
                out,
                "# Fake Executor（仅故障 conformance，不提供生产能力）\n"
            );
            out.push_str(&fake.to_markdown());
            let _ = writeln!(out, "\n# 本机真实适配器（运行时 {version}）\n");
            out.push_str(&live.to_markdown());
            Ok(out)
        }
        [cmd, rest @ ..] if cmd == "conformance" => conformance(rest),
        [cmd, sub, rest @ ..] if cmd == "handoff" && sub == "sample" => handoff_sample(rest),
        [cmd, sub, rest @ ..] if cmd == "handoff" && sub == "drill" => handoff_drill(rest),
        [cmd, sub, rest @ ..] if cmd == "adapter" && sub == "release-check" => release_check(rest),
        _ => Err(format!("未知命令: {}\n\n{}", args.join(" "), USAGE)),
    }
}

/// `bod adapter capabilities`：逐个能力显示声明与有效期；给定 `--need` 时按保证需求过滤，
/// 并用与调度准入同一个 [`CapabilityNotice`] 渲染限制（4.3）。
fn adapter_capabilities(need: Option<&str>) -> Result<String, String> {
    // 先解析等级：参数错了不该去启适配器。
    let level = match need {
        None => None,
        Some(value) => Some(match value.to_ascii_lowercase().as_str() {
            "l1" => HandoffLevel::L1,
            "l2" => HandoffLevel::L2,
            "l3" => HandoffLevel::L3,
            other => {
                return Err(format!(
                    "--need 只接受 l1 / l2 / l3，收到 {other}\n\n{USAGE}"
                ));
            }
        }),
    };

    let mut adapter = adapter_or_err()?;
    let descriptor = adapter.descriptor().clone();
    let now = now_ms();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "适配器 {} v{}（协议 {}，平台 {}）",
        descriptor.adapter_id, descriptor.adapter_version, descriptor.protocol, descriptor.platform
    );
    let _ = writeln!(out, "\n| 能力 | 声明 | 证据 | 有效期 |");
    let _ = writeln!(out, "|---|---|---|---|");
    for slot in CapabilitySlot::KNOWN {
        let claim = descriptor
            .capabilities
            .iter()
            .find(|claim| claim.slot.as_str() == slot);
        let (level_text, evidence, validity) = match claim {
            None => ("not_declared", "—".to_string(), "—".to_string()),
            Some(claim) => {
                let validity = match claim.expires_at_ms {
                    None => "未设过期".to_string(),
                    Some(expires) if expires <= now => format!("已过期（{expires}）"),
                    Some(expires) => format!("有效至 {expires}"),
                };
                (
                    claim.level.as_str(),
                    format!(
                        "{} / {}",
                        claim.source.as_str(),
                        claim.evidence_level.as_str()
                    ),
                    validity,
                )
            }
        };
        let _ = writeln!(out, "| {slot} | {level_text} | {evidence} | {validity} |");
    }

    if let Some(level_value) = level {
        // 准入评估用标准化的含凭据负载：否则敏感的脱敏用例拿不到样本，
        // 真实适配器会一律落在 unverified 上——那是评估方式的问题，不是适配器的问题。
        adapter.use_conformance_payload(now);
        let report = run_live_suite(&mut adapter, now);
        let verdict = enablement(&report);
        let required = RequiredGuarantees::for_handoff(level_value);
        let selection = select(
            &required,
            &[Candidate {
                descriptor: &descriptor,
                enablement: &verdict,
            }],
            now,
        );
        let _ = writeln!(out, "\n## {} 需求", level_value);
        let _ = writeln!(
            out,
            "所需保证: {}",
            if required.is_empty() {
                "无".to_string()
            } else {
                required
                    .iter()
                    .map(|item| item.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        // 与调度准入共用同一渲染函数：界面不可能与调度说不一样的话。
        let _ = writeln!(out, "{}", selection.notice.render());
        for filtered in &selection.filtered {
            for reason in &filtered.reasons {
                let _ = writeln!(
                    out,
                    "  - {} [{}] {}",
                    filtered.adapter_id,
                    reason.code(),
                    reason.describe()
                );
            }
        }
    }
    Ok(out)
}

/// 从 flag 列表取单值参数（`--name value`）。
fn flag_value<'a>(flags: &'a [String], name: &str) -> Option<&'a str> {
    flags
        .iter()
        .position(|item| item == name)
        .and_then(|position| flags.get(position + 1))
        .map(std::string::String::as_str)
}

fn required_flag<'a>(flags: &'a [String], name: &str, action: &str) -> Result<&'a str, String> {
    flag_value(flags, name)
        .ok_or_else(|| format!("`handoff drill {action}` 需要 {name}\n\n{USAGE}"))
}

fn number_flag(flags: &[String], name: &str, action: &str) -> Result<f64, String> {
    let raw = required_flag(flags, name, action)?;
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("{name} 需要数字，收到 {raw}\n\n{USAGE}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("{name} 必须是非负有限值，收到 {raw}"));
    }
    Ok(value)
}

fn count_flag(flags: &[String], name: &str, action: &str) -> Result<u32, String> {
    let raw = required_flag(flags, name, action)?;
    raw.parse()
        .map_err(|_| format!("{name} 需要非负整数，收到 {raw}\n\n{USAGE}"))
}

/// `bod handoff drill`：真实演练的秒表与对照记录（4.4 人工耗时 / 5.2 对照）。
///
/// 计时不留给人手填：演练者在动作发生时打一档，时间戳由系统时钟写入，读数由两端算出。
fn handoff_drill(flags: &[String]) -> Result<String, String> {
    use bod_app::handoff_drill::{self as drill, Arm, ArmMetrics, DrillStep};

    let action = flags.first().ok_or_else(|| {
        format!("`handoff drill` 需要动作 start / mark / finish / show / arm\n\n{USAGE}")
    })?;
    let rest = &flags[1..];
    // 先抓拼错的参数：演练靠人手敲命令，静默忽略一个错字等于丢掉一档读数。
    const KNOWN: [&str; 11] = [
        "--root",
        "--operator",
        "--task",
        "--step",
        "--note",
        "--json",
        "--arm",
        "--ready-seconds",
        "--interruptions",
        "--duplicate-runs",
        "--manual-minutes",
    ];
    for item in rest.iter().filter(|item| item.starts_with("--")) {
        let name = item.split('=').next().unwrap_or(item);
        if !KNOWN.contains(&name) {
            return Err(format!("未知参数 {name}\n\n{USAGE}"));
        }
    }
    let root_text = required_flag(rest, "--root", action)?;
    let root = PathBuf::from(root_text);

    match action.as_str() {
        "start" => {
            let operator = required_flag(rest, "--operator", action)?;
            let task = required_flag(rest, "--task", action)?;
            let session = drill::start(&root, operator, task, now_ms())?;
            Ok(format!(
                "已开始计时\n- 演练者: {}\n- 任务类别: {}\n- 目录: {}\n- 时间线: {}\n\n下一步：任务就绪后打 `--step task_ready`；原执行者做最后一次有效动作时打 `--step last_action`（秒表起点）\n",
                session.meta.operator,
                session.meta.task,
                root.display(),
                session.timeline_line()
            ))
        }
        "mark" => {
            let step_raw = required_flag(rest, "--step", action)?;
            let step = DrillStep::parse(step_raw)?;
            let note = flag_value(rest, "--note").unwrap_or_default();
            let session = drill::mark(&root, step, note, now_ms())?;
            let next = match step {
                DrillStep::TaskReady => {
                    "下一步：原执行者最后一次有效动作时打 `--step last_action`（秒表起点）"
                }
                DrillStep::LastAction => {
                    "下一步：接手者第一次有效动作时打 `--step handoff_done`（秒表终点）"
                }
                DrillStep::HandoffDone => {
                    "下一步：`bod handoff drill finish --root <目录>` 封存读数"
                }
                DrillStep::VerifyBegin | DrillStep::VerifyEnd => {
                    "复核段是选填；齐了就 `finish` 封存"
                }
            };
            Ok(format!(
                "已打点 `{}`\n- 时间线: {}\n\n{next}\n",
                step.as_str(),
                session.timeline_line()
            ))
        }
        "finish" => {
            let session = drill::finish(&root, now_ms())?;
            let minutes = session.handoff_minutes()?;
            Ok(format!(
                "{}\n\n已封存：人工耗时 {minutes:.1} 分钟（由 last_action → handoff_done 的时间戳算出）\n\
                 下一步：`bod handoff sample --drill {}` 让接续样本报告引用这份读数\n",
                drill::to_markdown(&session),
                root.display()
            ))
        }
        "show" => {
            let session = drill::load(&root)?;
            if rest.iter().any(|item| item == "--json") {
                Ok(drill::to_json(&session))
            } else {
                Ok(drill::to_markdown(&session))
            }
        }
        "arm" => {
            let arm_label = required_flag(rest, "--arm", action)?;
            let arm = Arm::parse(arm_label)?;
            let metrics = ArmMetrics {
                ready_seconds: number_flag(rest, "--ready-seconds", action)?,
                interruptions: count_flag(rest, "--interruptions", action)?,
                duplicate_runs: count_flag(rest, "--duplicate-runs", action)?,
                manual_minutes: number_flag(rest, "--manual-minutes", action)?,
            };
            let note = flag_value(rest, "--note").unwrap_or_default();
            let session = drill::record_arm(&root, arm, &metrics, note, now_ms())?;
            Ok(format!(
                "已记录 {} 臂读数（接管就绪 {} s / 中断 {} / 重复 {} / 人工补充 {} min）\n\n{}",
                arm.label(),
                metrics.ready_seconds,
                metrics.interruptions,
                metrics.duplicate_runs,
                metrics.manual_minutes,
                drill::to_markdown(&session)
            ))
        }
        other => Err(format!(
            "未知动作 {other}；只接受 start / mark / finish / show / arm\n\n{USAGE}"
        )),
    }
}

/// `bod handoff sample`：P2 任务 4.4 的采样入口。
///
/// 对每个**不同运行时**各跑一遍 `durable-process` 与 `local-process` 的接续样本。结论只按
/// 「不同运行时」计数：同一运行时换适配器不算跨运行时接续，因此本命令即使在单运行时机器上
/// 全绿，也只会给出 `insufficient`——这是正确结论，不是配置问题。
fn handoff_sample(flags: &[String]) -> Result<String, String> {
    let mut level = HandoffLevel::L2;
    let mut json = false;
    let mut root: Option<PathBuf> = None;
    let mut drill_root: Option<PathBuf> = None;
    // 缺省是「脚本化，实测无人工介入」。真实读数只认演练记录（--drill）；
    // --human-minutes 是徒手给的数字，报告会标成口述值、不算 4.4 证据。
    let mut claimed_minutes: Option<f64> = None;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--level" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| format!("--level 需要取值 l1 / l2\n\n{USAGE}"))?;
                level = match value.to_ascii_lowercase().as_str() {
                    "l1" => HandoffLevel::L1,
                    "l2" => HandoffLevel::L2,
                    "l3" => HandoffLevel::L3,
                    other => {
                        return Err(format!(
                            "--level 只接受 l1 / l2 / l3，收到 {other}\n\n{USAGE}"
                        ));
                    }
                };
                index += 2;
            }
            "--root" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| format!("--root 需要目录\n\n{USAGE}"))?;
                root = Some(PathBuf::from(value));
                index += 2;
            }
            "--drill" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| format!("--drill 需要演练目录\n\n{USAGE}"))?;
                drill_root = Some(PathBuf::from(value));
                index += 2;
            }
            "--human-minutes" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| format!("--human-minutes 需要分钟数\n\n{USAGE}"))?;
                let minutes: f64 = value
                    .parse()
                    .map_err(|_| format!("--human-minutes 需要数字，收到 {value}\n\n{USAGE}"))?;
                if !minutes.is_finite() || minutes < 0.0 {
                    return Err(format!("--human-minutes 必须是非负有限值，收到 {value}"));
                }
                claimed_minutes = Some(minutes);
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("未知参数 {other}\n\n{USAGE}")),
        }
    }
    if drill_root.is_some() && claimed_minutes.is_some() {
        return Err(
            "--drill 与 --human-minutes 不能同时给：一个是演练记录算出的读数，一个是口述值，混在一起就分不清哪个进了报告"
                .to_string(),
        );
    }

    // 演练记录优先：未封存或缺档位直接报错，不退回默认值、不拿口述值顶替。
    let mut drill_session = None;
    let human_time = match &drill_root {
        Some(path) => {
            let session = bod_app::handoff_drill::load(path)?;
            let time = handoff_sample::human_time_from_drill(path)?;
            drill_session = Some(session);
            time
        }
        None => match claimed_minutes {
            Some(minutes) => HumanTime::Claimed(minutes),
            None => HumanTime::Scripted,
        },
    };

    let now = now_ms();
    let runtimes = handoff_sample::detect_runtimes(&["python3", "python", "node"]);
    if runtimes.is_empty() {
        return Err(
            "未找到可运行运行时（python3 / python / node）；没有运行时就不出样本，不假装通过"
                .to_string(),
        );
    }
    let base = root.unwrap_or_else(|| std::env::temp_dir().join(format!("bod-handoff-{now}")));
    let mut report = SampleReport::new(level, now);
    for (position, executable) in runtimes.iter().enumerate() {
        // durable：可回放观察，L2 的重放断言就落在它身上。
        let durable_root = base.join(format!("durable-{position}"));
        let durable = DurableSampleConfig::new(
            bod_app::durable_process::DurableProcessConfig::new(
                executable.as_str(),
                Vec::new(),
                &durable_root,
            ),
            now,
        );
        report.push(run_sample(
            &durable,
            level,
            &base.join(format!("ws-durable-{position}")),
            human_time.clone(),
            now,
        ));
        // local-process：观察在内存里，L2 上应如实得到 unsupported。
        let local = LocalSampleConfig::new(
            bod_app::local_process::LocalProcessConfig::new(executable.as_str(), Vec::new()),
            now,
        );
        report.push(run_sample(
            &local,
            level,
            &base.join(format!("ws-local-{position}")),
            human_time.clone(),
            now,
        ));
    }
    // 样本目录只放会话与快照；子进程都已被 cancel 确认停止，清掉不留垃圾。
    let _ = fs::remove_dir_all(&base);
    if let Some(session) = drill_session {
        report.attach_drill(session);
    }
    if json {
        Ok(report.to_json())
    } else {
        Ok(report.to_markdown())
    }
}

/// `bod conformance`：分项报告入口（P2 任务 4.1）。
///
/// 与 P0 的 `bod adapter conformance` 并存：后者是既有证据链（`verification/gates.yaml`）
/// 引用的原始输出，这里给出结构化分项的权威版本。两者都不产生整体通过标记。
fn conformance(flags: &[String]) -> Result<String, String> {
    let mut json = false;
    let mut fake_only = false;
    for flag in flags {
        match flag.as_str() {
            "--json" => json = true,
            "--fake-only" => fake_only = true,
            other => return Err(format!("conformance 不支持参数 {other}\n\n{USAGE}")),
        }
    }

    let now = now_ms();
    let mut sections = Vec::new();

    let fake_descriptor = FakeExecutor::new(FakeScenario::HappyPath)
        .descriptor()
        .clone();
    let mut fake = SectionedReport::build(&run_fake_suite(now), Some(&fake_descriptor), now);
    let exclusion = FakeExecutor::production_exclusion_reason().to_string();
    if !fake.limitations.iter().any(|note| note == &exclusion) {
        fake.limitations.insert(0, exclusion);
    }
    sections.push(fake);

    if !fake_only {
        let mut adapter = adapter_or_err()?;
        // 用含凭据的正常负载驱动：敏感字段过滤验的是「真实负载经过宿主脱敏管道」，
        // 而不是让适配器自己挑一份不含秘密的样本来免责。
        adapter.use_conformance_payload(now);
        let descriptor = adapter.descriptor().clone();
        let live = run_live_suite(&mut adapter, now);
        sections.push(SectionedReport::build(&live, Some(&descriptor), now));
    }

    if json {
        let body: Vec<String> = sections.iter().map(SectionedReport::to_json).collect();
        return Ok(format!(
            "{{\n  \"reports\": [\n{}\n  ]\n}}\n",
            body.join(",\n")
        ));
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# bod conformance（{} 个适配器，各自独立分项）\n",
        sections.len()
    );
    let _ = writeln!(
        out,
        "整体结论: 不提供（每个适配器按能力与用例逐项判定；未测项标 unknown / not_applicable）\n"
    );
    for section in &sections {
        out.push_str(&section.to_markdown_at(2));
        out.push('\n');
    }
    Ok(out)
}

/// `bod adapter release-check`：发布验收（P2 任务 4.5）。
///
/// 只做两件事：把**真实取得的**证据交给 4.5 的判定，缺的记 `not_evidenced`；以及在三处
/// 之间比对能力缺失。命令不打开业务库（P0 单库约束），所以「重启意图仍在」与「账号预算
/// 可评估」在本机恒为未核对——这是结论，不是遗漏。
fn release_check(flags: &[String]) -> Result<String, String> {
    let mut json = false;
    let mut need: Option<String> = None;
    let mut logs_path: Option<String> = None;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--json" => {
                json = true;
                index += 1;
            }
            "--need" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| format!("--need 需要 l1 / l2 / l3\n\n{USAGE}"))?;
                need = Some(value.clone());
                index += 2;
            }
            "--logs" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| format!("--logs 需要一个日志文件路径\n\n{USAGE}"))?;
                logs_path = Some(value.clone());
                index += 2;
            }
            other => {
                return Err(format!(
                    "adapter release-check 不支持参数 {other}\n\n{USAGE}"
                ));
            }
        }
    }

    // 先解析等级：参数错了不该去启适配器。
    let level = match need.as_deref() {
        None => HandoffLevel::L1,
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "l1" => HandoffLevel::L1,
            "l2" => HandoffLevel::L2,
            "l3" => HandoffLevel::L3,
            other => {
                return Err(format!(
                    "--need 只接受 l1 / l2 / l3，收到 {other}\n\n{USAGE}"
                ));
            }
        },
    };

    let now = now_ms();
    let mut adapter = adapter_or_err()?;
    // 与 conformance / capabilities 用同一份标准负载：同一个适配器在不同命令下得到
    // 不同的 enablement，本身就是 4.5「三处一致」要防的那种不一致。
    adapter.use_conformance_payload(now);
    let descriptor = adapter.descriptor().clone();
    let live = run_live_suite(&mut adapter, now);
    let report = SectionedReport::build(&live, Some(&descriptor), now);

    let mut evidence = ReleaseEvidence {
        adapter_id: descriptor.adapter_id.clone(),
        adapter_version: descriptor.adapter_version.clone(),
        platform: descriptor.platform.clone(),
        operations: descriptor
            .operations
            .iter()
            .map(|operation| operation.as_str().to_string())
            .collect(),
        // 三项都从本机真实用例取：not_applicable 就是「没取得证据」，不补默认值。
        stop_confirmed: case_signal(&live, "cancel-request-vs-confirm"),
        durable_intents: None,
        dedup_retention_ms: descriptor.dedup_retention_ms,
        reply_lost_reconciled: case_signal(&live, "start-reply-lost"),
        effect_class: Some(descriptor.effect_class.as_str().to_string()),
        authorization_refused: case_signal(&live, "auth-expired"),
        budget: None,
        report: Some(report.clone()),
        log_lines: Vec::new(),
    };
    if let Some(path) = &logs_path {
        let text =
            fs::read_to_string(path).map_err(|err| format!("读取日志 {path} 失败：{err}"))?;
        evidence.log_lines = text.lines().map(|line| line.to_string()).collect();
    }

    let required = RequiredGuarantees::for_handoff(level);
    let verdict = report.enablement.clone();
    let selection = select(
        &required,
        &[Candidate {
            descriptor: &descriptor,
            enablement: &verdict,
        }],
        now,
    );
    let acceptance = ReleaseAcceptance::evaluate(&evidence, &required, Some(&selection), now);
    if json {
        return Ok(acceptance.to_json());
    }

    let mut out = String::new();
    let _ = writeln!(out, "# bod adapter release-check\n");
    let _ = writeln!(
        out,
        "接续等级: {level}（所需保证{}）",
        if required.is_empty() {
            "无".to_string()
        } else {
            format!(
                ": {}",
                required
                    .iter()
                    .map(|item| item.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    let _ = writeln!(out, "{}\n", selection.notice.render());
    out.push_str(&acceptance.to_markdown());
    Ok(out)
}

/// 用例结果 → 证据信号：`not_applicable` 与「没有该用例」都返回 `None`，即未取得证据。
fn case_signal(report: &bod_app::adapter_conformance::ConformanceReport, id: &str) -> Option<bool> {
    report
        .outcomes
        .iter()
        .find(|outcome| outcome.id == id)
        .and_then(|outcome| match &outcome.status {
            CaseStatus::Pass { .. } => Some(true),
            CaseStatus::Fail { .. } => Some(false),
            CaseStatus::NotApplicable { .. } => None,
        })
}

/// 读命令的库来源：**只读挂载一个已存在的库**（P8 7.7.3）。
///
/// 三个读命令（`observe` / `tracking` / `sources`）的 help 一直写着「只读」，
/// 实现却走 `SqliteStore::open`——每次打开都把 20 个迁移跑一遍，目标路径不存在时
/// 还会 `create_dir_all` 建库。后果有两个，都不是「反正幂等」能带过去的：
/// 一是**改 schema 这种写操作悄悄发生在一次读取里**；二是路径写错被伪装成「库里没数据」，
/// 读者会去查数据而不是查路径。
///
/// 换成只读挂载后，缺库当场报 `MissingDatabase`，库比代码旧时读列会失败而不是被静默升级。
/// 代价是老库必须先跑一次写命令（或任意会迁移的命令）才能被读——这是有意的：
/// 读命令不该替业务库决定 schema 版本。
fn open_readonly_for(
    db: &std::path::Path,
    command: &str,
) -> Result<bod_app::store::SqliteStore, String> {
    bod_app::store::SqliteStore::open_existing_readonly(db, &BodConfig::default())
        .map_err(|err| format!("{command} 打开数据库失败：{err}"))
}

/// `bod observe`：从既有表算 §9 的观测读数（CR-002 第 3 步）。
///
/// 只读：只读挂载 + 只做 SELECT（见 `open_readonly_for`）。算不出来的项在输出里单列
/// （`not_measured`），不填 0——报表里的 0 会被读成「测到了，且是零」。
fn observe(flags: &[String]) -> Result<String, String> {
    let mut db: Option<std::path::PathBuf> = None;
    let mut company = "company-default".to_string();
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--db" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--db 需要一个路径".to_string())?;
                db = Some(std::path::PathBuf::from(value));
                index += 2;
            }
            "--company" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--company 需要一个 ID".to_string())?;
                company = value.clone();
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("observe 不支持参数 {other}\n\n{USAGE}")),
        }
    }
    let db = db.ok_or_else(|| format!("observe 需要 --db <数据库路径>\n\n{USAGE}"))?;
    let store = open_readonly_for(&db, "observe")?;
    let snapshot = bod_app::observability::snapshot(&store, &company, now_ms())
        .map_err(|err| format!("采集失败：{err}"))?;
    if json {
        return Ok(format!("{}\n", snapshot_json(&snapshot)));
    }
    Ok(snapshot.render())
}

/// 观测快照的机器可读形态。字段与 `render` 的段落一一对应，
/// 不额外加派生指标——派生会让两处口径分叉。
fn snapshot_json(snapshot: &bod_app::observability::ObservationSnapshot) -> serde_json::Value {
    let queue = snapshot
        .queue
        .iter()
        .map(|row| {
            serde_json::json!({
                "state": &row.state,
                "count": row.count,
                "oldest_due_at_ms": row.oldest_due_at_ms,
            })
        })
        .collect::<Vec<_>>();
    let blocked = snapshot
        .blocked_reasons
        .iter()
        .map(|row| serde_json::json!({ "reason": &row.reason, "count": row.count }))
        .collect::<Vec<_>>();
    let deliveries = snapshot
        .deliveries
        .iter()
        .map(|row| {
            serde_json::json!({
                "status": &row.status,
                "count": row.count,
                "oldest_created_at_ms": row.oldest_created_at_ms,
                "max_wait_ms": row.max_wait_ms,
            })
        })
        .collect::<Vec<_>>();
    let reservations = snapshot
        .reservations
        .iter()
        .map(|row| {
            serde_json::json!({
                "dimension": &row.dimension,
                "reserved": row.reserved,
                "risk": row.risk,
                "rows": row.rows,
            })
        })
        .collect::<Vec<_>>();
    let not_measured = snapshot
        .not_measured
        .iter()
        .map(|item| serde_json::json!({ "item": item.item, "reason": item.reason }))
        .collect::<Vec<_>>();
    serde_json::json!({
        "company_id": &snapshot.company_id,
        "captured_at_ms": snapshot.captured_at_ms,
        "queue": queue,
        "blocked_reasons": blocked,
        "deliveries": deliveries,
        "reservations": reservations,
        "audit": {
            "allowed": snapshot.audit_allowed,
            "denied": snapshot.audit_denied,
        },
        "not_measured": not_measured,
    })
}

/// `bod tracking`：读交付跟踪项与覆盖结论（P8 3.1）。
///
/// 只读：打开库后只做 SELECT。覆盖结论由 `coverage_receipt::judge_coverage` 在读取时算出，
/// 不从表里读——库里没有「覆盖结论」这一列，也不该有（design D1）。
///
/// 为什么先给命令行而不是先开放 `GET /api/v1/delivery-tracking/{id}`：那条契约承诺
/// 「按调用者授权目标过滤对客户不可见的目标」，而 §34.12 的主体判定尚未实现；
/// 现在开放就是把不该给的客户给出去。命令行是同一台机器上的操作者视图，
/// 不涉及跨主体过滤，也没有把未实现的判定说成已实现。
fn tracking(flags: &[String]) -> Result<String, String> {
    // `tracking import` / `tracking link` 是另**一些**命令：它们写库，而 `tracking` 只读。
    // 分派放在这里而不是 main 的 `match`，是因为它们共用 `tracking` 这个词根，
    // 拆成多个 `match` 分支会让「读/写」的差别看起来像几个无关的命令。
    match flags.first().map(String::as_str) {
        Some("import") => return tracking_import(&flags[1..]),
        Some("link") => return tracking_link(&flags[1..]),
        Some("observe") => return tracking_observe(&flags[1..]),
        _ => {}
    }
    let mut db: Option<std::path::PathBuf> = None;
    let mut company = "company-default".to_string();
    let mut id: Option<String> = None;
    let mut limit = 50usize;
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--db" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--db 需要一个路径".to_string())?;
                db = Some(std::path::PathBuf::from(value));
                index += 2;
            }
            "--company" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--company 需要一个 ID".to_string())?;
                company = value.clone();
                index += 2;
            }
            "--id" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--id 需要一个跟踪项 ID".to_string())?;
                id = Some(value.clone());
                index += 2;
            }
            "--limit" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--limit 需要一个正整数".to_string())?;
                limit = value
                    .parse::<usize>()
                    .map_err(|_| format!("--limit 需要正整数，收到 {value}"))?;
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("tracking 不支持参数 {other}\n\n{USAGE}")),
        }
    }
    let db = db.ok_or_else(|| format!("tracking 需要 --db <数据库路径>\n\n{USAGE}"))?;
    let store = open_readonly_for(&db, "tracking")?;

    if let Some(tracking_id) = id {
        let view = store
            .delivery_tracking_view(&company, &tracking_id)
            .map_err(|err| format!("读取失败：{err}"))?
            .ok_or_else(|| format!("{company} 下没有跟踪项 {tracking_id}"))?;
        // 主体与事实在这里装配（P8 7.5.3）：CLI 面从 `bod tracking import` 起真的有入口了，
        // 所以「允许动作」不能再用一个固定的空切片敷衍过去。
        let (principal, allowed) = cli_allowed_actions(&store, &company, &view)?;
        if json {
            let dto = view
                .to_dto(allowed)
                .map_err(|err| format!("投影失败：{err}"))?;
            let rendered =
                serde_json::to_string(&dto).map_err(|err| format!("序列化失败：{err}"))?;
            return Ok(format!("{rendered}\n"));
        }
        let mut out = bod_app::delivery_tracking::render_one(&view, &allowed);
        let _ = writeln!(out, "\n允许动作按本机主体 {principal} 判定（CLI 面）。");
        return Ok(out);
    }

    let rows = store
        .list_delivery_tracking(&company, limit)
        .map_err(|err| format!("读取失败：{err}"))?;
    if json {
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|(tracking, target_count)| {
                serde_json::json!({
                    "tracking_id": &tracking.tracking_id,
                    "project_id": &tracking.project_id,
                    "owner": if tracking.owner_id.trim().is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(tracking.owner_id.clone())
                    },
                    "version": tracking.resource_version,
                    "state": tracking.state.as_str(),
                    // 键名带 `_count`：契约里 `targets` 是数组，这里给数字不能同名，
                    // 否则复用模型的人会以为拿到了目标列表。
                    "target_count": target_count,
                })
            })
            .collect();
        let rendered = serde_json::json!({
            "company_id": &company,
            "limit": limit,
            "trackings": items,
        });
        return Ok(format!("{rendered}\n"));
    }
    Ok(bod_app::delivery_tracking::render_list(&company, &rows))
}

/// CLI 面的主体：本机账户名，与 `serve` 读同一个来源（ADR-0007 第 6 条）。
///
/// **不提供 `--as`**：命令行上能改写主体，等于把审计归属交给调用方。取不到就报错，
/// 不编一个 `local-user` 之类的占位主体——占位主体会让每次判定都去查一个不存在的成员，
/// 把「装不上主体」写成「没有权限」。
fn cli_principal() -> Result<bod_app::authorization::Principal, String> {
    let id = bod_app::host_handshake::local_principal_id().ok_or_else(|| {
        "无法确定本机主体：USERNAME / USER / LOGNAME 都不存在。\
         交付写入与允许动作判定都要记到具体主体上，不猜。"
            .to_string()
    })?;
    Ok(bod_app::authorization::Principal {
        id: bod_app::domain::DomainId::new(id),
        kind: bod_app::authorization::PrincipalKind::User,
        claimed_roles: Vec::new(),
    })
}

/// 装配主体事实并按 CLI 面算「允许动作」（P8 7.5.3 / 7.8）。
///
/// 事实装不上就报错，不退回空集：空事实的表现是「什么都不能做」，
/// 而那与「你没权限」在界面上长得一模一样（`StageFactsError` 的模块注释）。
fn cli_allowed_actions(
    store: &bod_app::store::SqliteStore,
    company: &str,
    view: &bod_app::delivery_tracking::TrackingView,
) -> Result<(String, Vec<&'static str>), String> {
    let principal = cli_principal()?;
    let facts = store
        .stage_facts(company, &principal.id)
        .map_err(|err| format!("授权事实装配失败（{}）：{}", err.as_code(), err.explain()))?;
    let project = bod_app::domain::DomainId::new(view.tracking.project_id.clone());
    let allowed = bod_app::delivery_api::select_allowed(
        &view.action_candidates(),
        &bod_app::delivery_api::available_on(bod_app::delivery_api::ActionSurface::Cli),
        &facts,
        &principal,
        Some(&project),
    );
    Ok((principal.id.as_str().to_string(), allowed))
}

/// 取 `--flag` 后面那个值。`tracking import` 的参数多，逐个写 `get(index + 1)` 会把
/// 「缺值」的报错文案散成十几份。
fn flag_arg(flags: &[String], index: usize, name: &str) -> Result<String, String> {
    flags
        .get(index + 1)
        .cloned()
        .ok_or_else(|| format!("{name} 需要一个值"))
}

/// 写命令失败的统一呈现：`StoreError` 的自述 + 稳定码（有的话）+ 命令给的下一步。
///
/// 不再叠一层「xxx 失败：」前缀——`StoreError` 的每个变体已经说清是哪一类失败，
/// 叠一层只会输出「追加关联失败：关联追加失败: …」这种读起来像 bug 的句子。
///
/// 码放在自述之后而不是之前：终端里先看到的是「什么错了」，往下才是可以拿去搜文档的码。
fn write_failure(err: &bod_app::store::StoreError, hint: String) -> String {
    match err.as_code() {
        Some(code) => format!("{err}（{code}）{hint}"),
        None => format!("{err}{hint}"),
    }
}

/// `bod tracking import`：登记一个外部来源并挂到跟踪项上（P8 7.5.3 第一条）。
///
/// **谁在什么时候跑它**：本机操作者（项目成员）在收到一个外部交付信号之后跑——
/// 例如客户报了一个缺陷、某个 PR 合进了客户分支——把「这件事发生在哪」变成可跟踪的来源。
/// 它是 ADR-0009 选 B 之后交付写入的第一条真实入口：HTTP 的五条 `POST` 仍是 `planned`，
/// 写入只在命令行上发生，`serve` 依旧只读挂载。
///
/// 为什么这是**写**命令，与前三个读命令相反：它调 `SqliteStore::open`，会建库、会跑迁移。
/// 读命令缺库当场失败（`open_readonly_for`），这里缺库就该建——不然一台新机器上
/// 第一条交付记录永远写不进去。
///
/// 判定边界（**已知缺口，写在这里比藏起来好**）：本命令**不判项目成员资格**。
/// `allowed_actions` 会在读取时按成员资格决定要不要推荐 `link-source`，
/// 但写入侧没有对应的闸门——因为 `project_members` 至今没有生产写入者，
/// 加上闸门会让所有人都导不进来（包括该导的人）。这条与「CLI 是同一台机器上的操作者视图」
/// 是同一立场，等 `project_members` 有入口（7.5.3 后续与 B3）再收紧。
fn tracking_import(flags: &[String]) -> Result<String, String> {
    let mut db: Option<std::path::PathBuf> = None;
    let mut company = "company-default".to_string();
    let mut key: Option<String> = None;
    let mut project: Option<String> = None;
    let mut locator: Option<String> = None;
    let mut connection_id = String::new();
    let mut object_type: Option<String> = None;
    let mut provider_object_id: Option<String> = None;
    let mut title: Option<String> = None;
    let mut requirement = String::new();
    let mut owner: Option<String> = None;
    let mut correlation: Option<String> = None;
    let mut decided_at: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--db" => {
                db = Some(std::path::PathBuf::from(flag_arg(flags, index, "--db")?));
                index += 2;
            }
            "--company" => {
                company = flag_arg(flags, index, "--company")?;
                index += 2;
            }
            "--key" => {
                key = Some(flag_arg(flags, index, "--key")?);
                index += 2;
            }
            "--project" => {
                project = Some(flag_arg(flags, index, "--project")?);
                index += 2;
            }
            "--locator" => {
                locator = Some(flag_arg(flags, index, "--locator")?);
                index += 2;
            }
            "--connection" => {
                connection_id = flag_arg(flags, index, "--connection")?;
                index += 2;
            }
            "--object-type" => {
                object_type = Some(flag_arg(flags, index, "--object-type")?);
                index += 2;
            }
            "--provider-object-id" => {
                provider_object_id = Some(flag_arg(flags, index, "--provider-object-id")?);
                index += 2;
            }
            "--title" => {
                title = Some(flag_arg(flags, index, "--title")?);
                index += 2;
            }
            "--requirement" => {
                requirement = flag_arg(flags, index, "--requirement")?;
                index += 2;
            }
            "--owner" => {
                owner = Some(flag_arg(flags, index, "--owner")?);
                index += 2;
            }
            "--correlation" => {
                correlation = Some(flag_arg(flags, index, "--correlation")?);
                index += 2;
            }
            "--decided-at" => {
                decided_at = Some(flag_arg(flags, index, "--decided-at")?);
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            // `--as` 明确拒绝而不是「不认识」：这个参数看起来太合理了，
            // 静默忽略会让调用方以为归属被改了。
            "--as" => {
                return Err(
                    "tracking import 不接受 --as：主体由本机账户确定，命令行上不得自报".to_string(),
                );
            }
            other => return Err(format!("tracking import 不支持参数 {other}\n\n{USAGE}")),
        }
    }

    let db = db.ok_or_else(|| format!("tracking import 需要 --db <数据库路径>\n\n{USAGE}"))?;
    let key = key.ok_or_else(|| {
        format!("tracking import 需要 --key <幂等键>（重试要落回同一次导入）\n\n{USAGE}")
    })?;
    let project =
        project.ok_or_else(|| format!("tracking import 需要 --project <项目>\n\n{USAGE}"))?;
    let locator =
        locator.ok_or_else(|| format!("tracking import 需要 --locator <定位>\n\n{USAGE}"))?;
    // 主体先取：取不到就根本不该碰库。`imported_by` 与审计归属都来自它。
    let principal = cli_principal()?;
    let imported_by = principal.id.as_str().to_string();

    let command = bod_app::delivery_source::ImportSourceCommand {
        idempotency_key: key,
        company_id: company.clone(),
        project_id: project,
        connection_id,
        object_type,
        provider_object_id,
        // 标题缺省取定位：空标题会让列表里出现一行空白，编一个名字又比定位更难核对。
        title: title.unwrap_or_else(|| locator.clone()),
        requirement,
        // 负责人缺省是导入者本身：导入的人先兜着，之后由 `--owner` 指定或另行指派。
        owner_id: owner.unwrap_or_else(|| imported_by.clone()),
        // 三个 ID 由来源身份派生（`with_derived_ids`）：它们进载荷指纹，
        // 每次随机生成会让「同键重试」变成「同键异载荷」。
        new_tracking_id: String::new(),
        new_source_id: String::new(),
        new_candidate_id: String::new(),
        imported_by,
        correlation_id: correlation
            .unwrap_or_else(|| format!("cli-{}-{}", std::process::id(), now_ms())),
        decided_at: decided_at.unwrap_or_else(|| bod_app::conformance_report::utc_stamp(now_ms())),
        locator,
    }
    .with_derived_ids();

    let mut store = bod_app::store::SqliteStore::open(&db, &BodConfig::default())
        .map_err(|err| format!("tracking import 打开数据库失败：{err}"))?;
    // 写入走事务而不是直连：`import_delivery_source` 内部用 SAVEPOINT 保证
    // 「业务写入 + 回执 + 审计」同生共死，但外层事务仍要由调用方开——回执与审计
    // 的可见性以提交为准，半途失败不许留下「命令已生效」的印象。
    let outcome = {
        let uow = store
            .transaction()
            .map_err(|err| format!("开启事务失败：{err}"))?;
        let outcome = uow
            .import_delivery_source(&command, now_ms())
            .map_err(|err| write_failure(&err, String::new()))?;
        uow.commit().map_err(|err| format!("提交失败：{err}"))?;
        outcome
    };

    if json {
        let mut value =
            serde_json::to_value(&outcome).map_err(|err| format!("序列化失败：{err}"))?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "company_id".to_string(),
                serde_json::Value::String(company.clone()),
            );
        }
        let rendered = serde_json::to_string(&value).map_err(|err| format!("序列化失败：{err}"))?;
        return Ok(format!("{rendered}\n"));
    }

    let mut out = String::new();
    let _ = writeln!(out, "公司 {company}  库 {}", db.display());
    match (&outcome.tracking_id, &outcome.candidate_id) {
        (Some(tracking_id), _) => {
            let _ = writeln!(
                out,
                "跟踪项 {tracking_id}（{}）",
                if outcome.created_tracking {
                    "新建"
                } else {
                    "已有，未新建"
                }
            );
            let _ = writeln!(
                out,
                "来源 {}（{}）",
                outcome.source_id.as_deref().unwrap_or("—"),
                if outcome.created_source {
                    "新建"
                } else {
                    "已有，未新建"
                }
            );
        }
        (None, Some(candidate_id)) => {
            // 只有定位时进候选：它不是身份，也就撑不起结论。这一支必须说清楚，
            // 否则调用方会以为「命令成功 = 已经能跟踪了」。
            let _ = writeln!(
                out,
                "候选 {candidate_id}（{}）：只有定位，还不是来源身份，需负责人确认",
                if outcome.created_candidate {
                    "新建"
                } else {
                    "已有"
                }
            );
        }
        (None, None) => {
            let _ = writeln!(out, "导入完成，但既没有跟踪项也没有候选——这不该发生");
        }
    }
    if outcome.replayed {
        let _ = writeln!(
            out,
            "本次是重放：同一幂等键的载荷已经执行过，没有再次写入。"
        );
    }
    Ok(out)
}

/// `bod tracking link`：给跟踪项补挂一个来源身份或内部工作包（P8 7.5.3 第二条）。
///
/// **谁在什么时候跑它**：操作者在「这条交付对应哪个工作包 / 哪个外部 PR」确定之后补挂。
/// 导入只回答「这件事发生在哪」，`link` 回答「它对应我们哪一段工作」——
/// 两者分开是因为它们常常不是同时知道的：缺陷先到，归属后定。
///
/// **`--expected-version` 必填，不从库里自动读**。这一条看着不友好，但它就是 3.4 的全部：
/// 命令行的「读一次再写」没有原子性，若让本命令自己读当前版本，读和写之间那段窗口里
/// 别人的写入会被静默吞进本次决定——闸门 100% 不会触发，等于把期望版本删了。
/// 所以操作流程是两步：先 `bod tracking --id <跟踪项> --json` 读 `version`，再带上来写。
/// 版本过期是**正常结果**，不是 bug：它的含义是「你按旧状态下的决定在写」。
/// 冲突时错误信息带当前版本，重读一次再提交即可。
///
/// 与前两条命令一样：主体取本机账户（`--as` 明确拒绝），写库走
/// `transaction → append_delivery_link → commit`（`SAVEPOINT` 在里层）。
fn tracking_link(flags: &[String]) -> Result<String, String> {
    let mut db: Option<std::path::PathBuf> = None;
    let mut company = "company-default".to_string();
    let mut tracking_id: Option<String> = None;
    let mut key: Option<String> = None;
    let mut source: Option<String> = None;
    let mut work_package: Option<String> = None;
    let mut expected_version: Option<u64> = None;
    let mut targets: Vec<bod_app::delivery_api::TargetRef> = Vec::new();
    let mut correlation: Option<String> = None;
    let mut decided_at: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--db" => {
                db = Some(std::path::PathBuf::from(flag_arg(flags, index, "--db")?));
                index += 2;
            }
            "--company" => {
                company = flag_arg(flags, index, "--company")?;
                index += 2;
            }
            "--id" => {
                tracking_id = Some(flag_arg(flags, index, "--id")?);
                index += 2;
            }
            "--key" => {
                key = Some(flag_arg(flags, index, "--key")?);
                index += 2;
            }
            "--source" => {
                source = Some(flag_arg(flags, index, "--source")?);
                index += 2;
            }
            "--work-package" => {
                work_package = Some(flag_arg(flags, index, "--work-package")?);
                index += 2;
            }
            "--expected-version" => {
                let raw = flag_arg(flags, index, "--expected-version")?;
                // 自增版本号从 1 开始：收到 0 说明调用方在拿「没读过」当默认值。
                let parsed = raw.parse::<u64>().map_err(|_| {
                    format!("--expected-version 需要正整数（当前跟踪项版本），收到 {raw}")
                })?;
                expected_version = Some(parsed);
                index += 2;
            }
            "--target" => {
                targets.push(parse_target(&flag_arg(flags, index, "--target")?)?);
                index += 2;
            }
            "--correlation" => {
                correlation = Some(flag_arg(flags, index, "--correlation")?);
                index += 2;
            }
            "--decided-at" => {
                decided_at = Some(flag_arg(flags, index, "--decided-at")?);
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            // 同 `import`：`--as` 看起来合理，静默忽略会让调用方以为归属改了。
            "--as" => {
                return Err(
                    "tracking link 不接受 --as：主体由本机账户确定，命令行上不得自报".to_string(),
                );
            }
            other => return Err(format!("tracking link 不支持参数 {other}\n\n{USAGE}")),
        }
    }

    let db = db.ok_or_else(|| format!("tracking link 需要 --db <数据库路径>\n\n{USAGE}"))?;
    let tracking_id =
        tracking_id.ok_or_else(|| format!("tracking link 需要 --id <跟踪项>\n\n{USAGE}"))?;
    let key = key.ok_or_else(|| {
        format!("tracking link 需要 --key <幂等键>（重试要落回同一次追加）\n\n{USAGE}")
    })?;
    let expected_version = expected_version.ok_or_else(|| {
        format!(
            "tracking link 需要 --expected-version <当前版本>：\
             先跑 `bod tracking --id <跟踪项> --json` 读 version。\
             本命令不会替你读——那样读与写之间的写入会被静默吞掉\n\n{USAGE}"
        )
    })?;
    // 二选一交给 `LinkRef::from_options`：它已经区分「都没给」与「都给了」，
    // 在这里重写一遍判断，就等于多一处会与它漂移的规则。
    let link = bod_app::delivery_link::LinkRef::from_options(source, work_package)
        .map_err(|err| format!("{}（{}）", err.explain(), err.code()))?;

    let principal = cli_principal()?;
    let linked_by = principal.id.as_str().to_string();
    let now = now_ms();

    let command = bod_app::delivery_link::AppendLinkCommand {
        idempotency_key: key,
        company_id: company.clone(),
        tracking_id: tracking_id.clone(),
        link,
        targets,
        expected_version,
        linked_by,
        correlation_id: correlation
            .unwrap_or_else(|| format!("cli-{}-{}", std::process::id(), now)),
        decided_at: decided_at.unwrap_or_else(|| bod_app::conformance_report::utc_stamp(now)),
    };

    let mut store = bod_app::store::SqliteStore::open(&db, &BodConfig::default())
        .map_err(|err| format!("tracking link 打开数据库失败：{err}"))?;
    let outcome = {
        let uow = store
            .transaction()
            .map_err(|err| format!("开启事务失败：{err}"))?;
        let outcome = uow.append_delivery_link(&command, now).map_err(|err| {
            // 版本冲突是这条命令**预期会**给出的答案，不是「系统坏了」：
            // 把下一步动作写进提示里，否则它会被当成 bug 报上来。
            let hint = match &err {
                bod_app::store::StoreError::Link(
                    bod_app::delivery_link::LinkError::VersionConflict { current, .. },
                ) => format!(
                    "\n当前版本是 {current}。下一步：重跑 `bod tracking --db {} --id {}` \
                     确认状态仍是你以为的那样，再带 --expected-version {current} 重试。",
                    db.display(),
                    tracking_id
                ),
                _ => String::new(),
            };
            write_failure(&err, hint)
        })?;
        uow.commit().map_err(|err| format!("提交失败：{err}"))?;
        outcome
    };

    if json {
        let mut value =
            serde_json::to_value(&outcome).map_err(|err| format!("序列化失败：{err}"))?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "company_id".to_string(),
                serde_json::Value::String(company.clone()),
            );
        }
        let rendered = serde_json::to_string(&value).map_err(|err| format!("序列化失败：{err}"))?;
        return Ok(format!("{rendered}\n"));
    }

    let mut out = String::new();
    let _ = writeln!(out, "公司 {company}  库 {}", db.display());
    let _ = writeln!(
        out,
        "跟踪项 {tracking_id}  关联 {}（{}）",
        outcome.link_id,
        if outcome.appended {
            "已追加"
        } else {
            "已存在，未重复写"
        }
    );
    if !outcome.created_targets.is_empty() {
        let _ = writeln!(
            out,
            "新建目标 {} 个：{}",
            outcome.created_targets.len(),
            outcome.created_targets.join(", ")
        );
    }
    let _ = writeln!(out, "版本：{}", outcome.version);
    if outcome.replayed {
        let _ = writeln!(
            out,
            "本次是重放：同一幂等键的命令已经执行过，没有再次写入。\
             版本是当时的版本（{0}），不是现在读到的版本。",
            outcome.version
        );
    } else if !outcome.appended {
        // 纯重复不推进版本（3.4 第 1 条）：说清楚，否则「版本没变」看起来像没生效。
        let _ = writeln!(
            out,
            "关系与目标本来就在，版本没有推进——这不是失败，重复上报不该让别人的期望版本过期。"
        );
    }
    Ok(out)
}

/// `bod tracking observe`：给某个客户目标登记一条观察（P8 7.5.3.3 第三条写入口）。
///
/// **谁在什么时候跑它**：操作者在「客户那边确实发生了什么」确定之后跑——连接器报了
/// 一次部署、现场把版本回滚了、离线安装的回执今天才拿到。它把「发生了什么」记到
/// 时间轴上，与 `import`（这件事在哪）和 `link`（对应我们哪段工作）三件事分开：
/// 三者常常不是同时知道的，合进一个命令就得逼操作者在一个还不知道的时刻填另一个。
///
/// 与 `bod observe` 的区别：那条读的是**宿主自身**的观测读数（队列、预算、交付计数），
/// 只读；这条往**某条交付**的证据时间轴上追加一行。
///
/// 三个判断写在这里，因为它们在命令行上会直接被使用者撞到：
///
/// 1. **`--observed-at` 必填**。业务时间是来源时间，不是入库时间。默认「现在」会让
///    晚到的补录把一周前的部署记成今天，于是「回滚是否晚于部署」翻结论，而且不报错。
/// 2. **没有 `--expected-version`**，也不推进版本。追加事实不是「照着某个状态做的决定」：
///    两个人同时各记一条，两条都该留下。`link` 要版本是因为它改关系集合，这里不改。
/// 3. **没有 `--authorized`**。授权范围是 §34.12 的判定结果；写入口给这个开关，
///    「授权」就变成被审者自己打的勾。未授权的人工补证不会让覆盖变成「已覆盖」，
///    这是安全的一侧——输出里明写，免得操作者以为补证生效了。
fn tracking_observe(flags: &[String]) -> Result<String, String> {
    use bod_app::delivery_tracking::ObservationKind;

    let mut db: Option<std::path::PathBuf> = None;
    let mut company = "company-default".to_string();
    let mut tracking_id: Option<String> = None;
    let mut key: Option<String> = None;
    let mut targets: Vec<bod_app::delivery_api::TargetRef> = Vec::new();
    let mut kind: Option<ObservationKind> = None;
    let mut observed_at_raw: Option<String> = None;
    let mut source_kind: Option<&'static str> = None;
    let mut source_ref = String::new();
    let mut source_revision: Option<String> = None;
    let mut artifact_revision: Option<String> = None;
    let mut digest = String::new();
    let mut revision = String::new();
    let mut note = String::new();
    let mut correlation: Option<String> = None;
    let mut decided_at: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--db" => {
                db = Some(std::path::PathBuf::from(flag_arg(flags, index, "--db")?));
                index += 2;
            }
            "--company" => {
                company = flag_arg(flags, index, "--company")?;
                index += 2;
            }
            "--id" => {
                tracking_id = Some(flag_arg(flags, index, "--id")?);
                index += 2;
            }
            "--key" => {
                key = Some(flag_arg(flags, index, "--key")?);
                index += 2;
            }
            "--target" => {
                targets.push(parse_target(&flag_arg(flags, index, "--target")?)?);
                index += 2;
            }
            "--kind" => {
                let raw = flag_arg(flags, index, "--kind")?;
                kind = Some(
                    ObservationKind::parse(&raw)
                        .map_err(|err| format!("{}（{}）", err.explain(), err.code()))?,
                );
                index += 2;
            }
            "--observed-at" => {
                observed_at_raw = Some(flag_arg(flags, index, "--observed-at")?);
                index += 2;
            }
            "--source-kind" => {
                let raw = flag_arg(flags, index, "--source-kind")?;
                source_kind = Some(
                    bod_app::delivery_tracking::parse_source_kind(&raw)
                        .map_err(|err| format!("{}（{}）", err.explain(), err.code()))?,
                );
                index += 2;
            }
            "--source-ref" => {
                source_ref = flag_arg(flags, index, "--source-ref")?;
                index += 2;
            }
            "--source-revision" => {
                source_revision = Some(flag_arg(flags, index, "--source-revision")?);
                index += 2;
            }
            "--artifact-revision" => {
                artifact_revision = Some(flag_arg(flags, index, "--artifact-revision")?);
                index += 2;
            }
            "--digest" => {
                digest = flag_arg(flags, index, "--digest")?;
                index += 2;
            }
            "--revision" => {
                revision = flag_arg(flags, index, "--revision")?;
                index += 2;
            }
            "--note" => {
                note = flag_arg(flags, index, "--note")?;
                index += 2;
            }
            "--correlation" => {
                correlation = Some(flag_arg(flags, index, "--correlation")?);
                index += 2;
            }
            "--decided-at" => {
                decided_at = Some(flag_arg(flags, index, "--decided-at")?);
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            // 同 `import` / `link`：主体由本机账户确定，命令行上不得自报。
            "--as" => {
                return Err(
                    "tracking observe 不接受 --as：主体由本机账户确定，命令行上不得自报"
                        .to_string(),
                );
            }
            other => return Err(format!("tracking observe 不支持参数 {other}\n\n{USAGE}")),
        }
    }

    let db = db.ok_or_else(|| format!("tracking observe 需要 --db <数据库路径>\n\n{USAGE}"))?;
    let tracking_id =
        tracking_id.ok_or_else(|| format!("tracking observe 需要 --id <跟踪项>\n\n{USAGE}"))?;
    let key = key.ok_or_else(|| {
        format!("tracking observe 需要 --key <幂等键>（重试要落回同一条观察）\n\n{USAGE}")
    })?;
    let kind = kind.ok_or_else(|| {
        format!(
            "tracking observe 需要 --kind deployment|rollback|manual-evidence：\
             部署与回滚是两件不同的事实，没有可以顶替的缺省\n\n{USAGE}"
        )
    })?;
    let observed_at_raw = observed_at_raw.ok_or_else(|| {
        format!(
            "tracking observe 需要 --observed-at <RFC3339>：这条事实是**什么时候**发生的。\
             不默认取现在——晚到的补录不能记成今天\n\n{USAGE}"
        )
    })?;
    if targets.len() != 1 {
        return Err(format!(
            "tracking observe 需要恰好一个 --target（收到 {} 个）：一条事实落在多个客户目标上时，\
             请分多条命令各自登记——按目标各有一份证据才能各自判定\n\n{USAGE}",
            targets.len()
        ));
    }
    let target = targets.remove(0);

    let (observed_at, observed_at_ms) =
        bod_app::delivery_observation::parse_observed_at(&observed_at_raw)
            .map_err(|err| format!("{}（{}）", err.explain(), err.code()))?;

    let principal = cli_principal()?;
    let actor_id = principal.id.as_str().to_string();
    let now = now_ms();

    let command = bod_app::delivery_observation::RecordObservationCommand {
        idempotency_key: key,
        company_id: company.clone(),
        tracking_id: tracking_id.clone(),
        target: target.clone(),
        kind,
        source_kind: source_kind
            .unwrap_or_else(|| bod_app::delivery_observation::default_source_kind(kind)),
        source_ref,
        source_revision_id: source_revision,
        artifact_revision,
        digest,
        revision,
        note,
        actor_id,
        observed_at,
        observed_at_ms,
        correlation_id: correlation
            .unwrap_or_else(|| format!("cli-{}-{}", std::process::id(), now)),
        decided_at: decided_at.unwrap_or_else(|| bod_app::conformance_report::utc_stamp(now)),
    };

    let mut store = bod_app::store::SqliteStore::open(&db, &BodConfig::default())
        .map_err(|err| format!("tracking observe 打开数据库失败：{err}"))?;
    let outcome = {
        let uow = store
            .transaction()
            .map_err(|err| format!("开启事务失败：{err}"))?;
        let outcome = uow
            .record_delivery_observation(&command, now)
            .map_err(|err| {
                // 三种拒绝各给一条可照抄的下一步。它们的处置完全不同：
                // 改请求（补时间）、先建目标、或补来源修订——不写出来就会被当成系统坏了。
                let hint = match &err {
                    bod_app::store::StoreError::Delivery(
                        bod_app::delivery_tracking::DeliveryError::TargetNotInTracking { .. },
                    ) => format!(
                        "\n下一步：先把这个目标挂到跟踪项上——`bod tracking link --db {} --id {} \
                         --key <新键> --expected-version <版本> --target {}{}`。\
                         本命令不会替你建目标：凭一条观察建客户记录，会把笔误变成承诺。",
                        db.display(),
                        tracking_id,
                        target.customer_id,
                        match target.deployment_unit.as_deref() {
                            Some(unit) => format!(":{}:{unit}", target.environment),
                            None => format!(":{}", target.environment),
                        }
                    ),
                    bod_app::store::StoreError::Delivery(
                        bod_app::delivery_tracking::DeliveryError::EmptyManualEvidenceNote,
                    ) => "\n下一步：人工补证要写清授权范围与复验依据，用 --note 给出。\
                           空备注的补证不构成证据。"
                        .to_string(),
                    bod_app::store::StoreError::Observation(
                        bod_app::delivery_observation::ObservationError::ConnectorRequiresSourceRevision,
                    ) => "\n下一步：要么给 --source-revision <来源修订>（先 `bod sources --db <库>` 找到它），\
                           要么用 --source-kind manual 声明这是人工观察。"
                        .to_string(),
                    _ => String::new(),
                };
                write_failure(&err, hint)
            })?;
        uow.commit().map_err(|err| format!("提交失败：{err}"))?;
        outcome
    };

    if json {
        let mut value =
            serde_json::to_value(&outcome).map_err(|err| format!("序列化失败：{err}"))?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "company_id".to_string(),
                serde_json::Value::String(company.clone()),
            );
            object.insert(
                "target".to_string(),
                serde_json::json!({
                    "customer_id": target.customer_id,
                    "environment": target.environment,
                    "deployment_unit": target.deployment_unit,
                }),
            );
        }
        let rendered = serde_json::to_string(&value).map_err(|err| format!("序列化失败：{err}"))?;
        return Ok(format!("{rendered}\n"));
    }

    let unit_label = target
        .deployment_unit
        .as_deref()
        .filter(|unit| !unit.is_empty())
        .unwrap_or("整个环境");
    let mut out = String::new();
    let _ = writeln!(out, "公司 {company}  库 {}", db.display());
    let _ = writeln!(
        out,
        "跟踪项 {tracking_id}  目标 {}（{} / {} / {}）",
        outcome.target_id, target.customer_id, target.environment, unit_label
    );
    let _ = writeln!(
        out,
        "观察 {}（{}，来源 {}）业务时间 {}",
        outcome.observation_id, outcome.kind, outcome.source_kind, outcome.observed_at
    );
    if outcome.recorded {
        let _ = writeln!(out, "已写入时间轴。");
    } else {
        let _ = writeln!(
            out,
            "同一事实已在库里（同一条观察 ID），本次没有重复写入——\
             重复上报不该让时间线多出一行。"
        );
    }
    if outcome.replayed {
        let _ = writeln!(
            out,
            "本次是重放：同一幂等键的命令已经执行过，返回的是当时的结果。"
        );
    }
    let _ = writeln!(
        out,
        "版本：{}（未推进——追加事实不需要乐观并发闸门，所以也没有 --expected-version）",
        outcome.version
    );
    if kind == ObservationKind::ManualEvidence {
        let _ = writeln!(
            out,
            "授权：authorized=0。授权范围由 §34.12 的主体与目标判定给出，当前未实现；\
             这条补证只会留下 install-receipt-unauthorized，不会让覆盖变成「已覆盖」。"
        );
    }
    Ok(out)
}

/// `--target <客户>:<环境>[:<部署单元>]` → 一个目标。
///
/// 分隔符选 `:` 而不是空白：目标身份三段里没有一段允许空格（`environment` 空白即拒绝），
/// 而 shell 里 `--target acme:prod` 不用引号。第三段缺省与空串都表示「整个环境」，
/// 与存储层只有一种写法这件事对齐（`NormalizedTarget` 的注释）。
fn parse_target(raw: &str) -> Result<bod_app::delivery_api::TargetRef, String> {
    let mut parts = raw.splitn(3, ':');
    let customer_id = parts.next().unwrap_or_default().to_string();
    let environment = parts.next().ok_or_else(|| {
        format!("--target 需要 <客户>:<环境>[:<部署单元>]，收到 {raw}（缺环境就不知道是哪个目标）")
    })?;
    let unit = parts.next();
    Ok(bod_app::delivery_api::TargetRef {
        customer_id,
        environment: environment.to_string(),
        deployment_unit: match unit {
            None | Some("") => None,
            Some(value) => Some(value.to_string()),
        },
    })
}

/// `bod sources`：只读列出已确认的来源身份、修订条数与待确认候选（P8 3.2）。
///
/// 为什么先给 CLI 而不是 `POST /delivery-tracking/{id}/links`：那条契约要按 §34.12
/// 的主体与目标判定授权，而判定还没实现。候选确认是「谁把定位认成身份」的责任动作，
/// 在没有主体判定前开放写入口，等于让任何本机会话都能替别人认身份。
/// 只读视图不涉及这个决定，先让人看见「哪些定位还在候选里等着」。
fn sources(flags: &[String]) -> Result<String, String> {
    let mut db: Option<std::path::PathBuf> = None;
    let mut company = "company-default".to_string();
    let mut project: Option<String> = None;
    let mut limit = 50usize;
    let mut with_candidates = false;
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--db" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--db 需要一个路径".to_string())?;
                db = Some(std::path::PathBuf::from(value));
                index += 2;
            }
            "--company" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--company 需要一个 ID".to_string())?;
                company = value.clone();
                index += 2;
            }
            "--project" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--project 需要一个项目 ID".to_string())?;
                project = Some(value.clone());
                index += 2;
            }
            "--limit" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--limit 需要一个正整数".to_string())?;
                limit = value
                    .parse::<usize>()
                    .map_err(|_| format!("--limit 需要正整数，收到 {value}"))?;
                index += 2;
            }
            "--candidates" => {
                with_candidates = true;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("sources 不支持参数 {other}\n\n{USAGE}")),
        }
    }
    let db = db.ok_or_else(|| format!("sources 需要 --db <数据库路径>\n\n{USAGE}"))?;
    let store = open_readonly_for(&db, "sources")?;

    let rows = store
        .list_sources(&company, project.as_deref(), limit)
        .map_err(|err| format!("读取来源失败：{err}"))?;
    let candidates = if with_candidates {
        Some(
            store
                .list_source_candidates(&company, project.as_deref(), false, limit)
                .map_err(|err| format!("读取候选失败：{err}"))?,
        )
    } else {
        None
    };

    if json {
        let sources: Vec<serde_json::Value> = rows
            .iter()
            .map(|(source, revision_count)| {
                serde_json::json!({
                    "source_id": &source.source_id,
                    "project_id": &source.project_id,
                    "connection_id": &source.connection_id,
                    "object_type": &source.object_type,
                    "provider_object_id": &source.provider_object_id,
                    "locator": &source.locator,
                    "confirmed_by": &source.confirmed_by,
                    "confirmed_at_ms": source.confirmed_at_ms,
                    "revision_count": revision_count,
                })
            })
            .collect();
        let mut payload = serde_json::json!({
            "company_id": &company,
            "project_id": project,
            "limit": limit,
            "sources": sources,
        });
        if let Some(candidates) = &candidates {
            payload["candidates"] = serde_json::Value::Array(
                candidates
                    .iter()
                    .map(|candidate| {
                        serde_json::json!({
                            "candidate_id": &candidate.candidate_id,
                            "project_id": &candidate.project_id,
                            "connection_id": &candidate.connection_id,
                            "object_type": &candidate.object_type,
                            "locator": &candidate.locator,
                            "proposed_by": &candidate.proposed_by,
                            "proposed_at_ms": candidate.proposed_at_ms,
                            "state": candidate.state.as_str(),
                            "resolved_source_id": candidate.resolved_source_id,
                        })
                    })
                    .collect(),
            );
        }
        let rendered =
            serde_json::to_string(&payload).map_err(|err| format!("序列化失败：{err}"))?;
        return Ok(format!("{rendered}\n"));
    }

    let mut out = bod_app::delivery_source::render_sources(&rows);
    if let Some(candidates) = &candidates {
        out.push_str(&bod_app::delivery_source::render_candidates(candidates));
    }
    Ok(out)
}

/// 控制面默认库位置：与描述文件同目录（`$BOD_HOME`）。
///
/// 文件名沿用各命令行子命令的约定（`--db` 指向同一份文件），不另起一套命名。
const DEFAULT_DATABASE_FILE: &str = "bod.db";

fn default_database_path() -> std::path::PathBuf {
    bod_app::host_handshake::default_home().join(DEFAULT_DATABASE_FILE)
}

/// 产品接口的装配结果。
///
/// `principal_id` 只用于启动说明与报告，**不参与判定**——判定读的是 `actor`。
struct DeliveryWiring {
    read: std::sync::Arc<dyn bod_app::delivery_api::TrackingRead>,
    actor: bod_app::delivery_api::DeliveryActor,
    principal_id: String,
}

/// 数据库来源：显式 `--db` 是硬要求，缺省只在默认库**已经存在**时才挂。
///
/// 两种缺失分开处理，因为它们的原因不同：
/// - 显式给了路径却不存在 → 配置错了，当场失败（否则会建空库，把所有请求变成 404）；
/// - 没给路径且默认库不存在 → 正常状态（还没导过数据），控制面照常起，产品接口 401。
fn resolve_database(
    explicit: Option<std::path::PathBuf>,
    default_path: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, String> {
    match explicit {
        Some(path) if path.is_file() => Ok(Some(path)),
        Some(path) => Err(format!(
            "--db 指向的数据库不存在：{}\n只读挂载不创建库；先用命令把数据写进该库，再让控制面挂载。",
            path.display()
        )),
        None if default_path.is_file() => Ok(Some(default_path.to_path_buf())),
        None => Ok(None),
    }
}

/// 装配产品接口：主体 + 只读端口。
///
/// 主体取本机账户名（`host_handshake::local_principal_id`），**不提供任何命令行或
/// 请求级覆盖**。给了 `--as` 之类的开关，就等于把「谁是主体」交给启动者，
/// 而启动者与 HTTP 调用方在桌面环境里常常是同一个进程树——ADR-0007 第 6 条否决的
/// 正是这条路。取不到账户名时报错，不退化成占位主体。
fn delivery_wiring(path: &std::path::Path, company: &str) -> Result<DeliveryWiring, String> {
    use bod_app::delivery_api::DeliveryActor;

    let principal_id = bod_app::host_handshake::local_principal_id().ok_or_else(|| {
        "无法确定本机主体（USERNAME / USER / LOGNAME 都为空）：产品接口不装配。\n\
         不提供自报主体的开关——那会让租户隔离与审计归属同时失效（ADR-0007 第 6 条）。"
            .to_string()
    })?;
    let store = bod_app::store::SqliteStore::open_existing_readonly(path, &BodConfig::default())
        .map_err(|err| format!("只读挂载 {} 失败：{err}", path.display()))?;
    let read = std::sync::Arc::new(bod_app::store::read_port::StoreTrackingRead::new(store));
    let actor = DeliveryActor {
        company_id: company.to_string(),
        principal: bod_app::authorization::Principal {
            id: bod_app::domain::DomainId::new(principal_id.as_str()),
            kind: bod_app::authorization::PrincipalKind::User,
            claimed_roles: Vec::new(),
        },
    };
    Ok(DeliveryWiring {
        read,
        actor,
        principal_id,
    })
}

/// `bod serve`：起本地控制面，阻塞到收到带会话密钥的停止请求。
///
/// P0 的控制面只做握手与生命周期，不承载业务命令；业务库的**写入**仍由命令层独占。
/// 产品接口要读交付视图时，这里用 `--db` 显式挂载既有库，而且是**只读连接 +
/// `query_only`**：不建库、不迁移、不写，因此不构成第二个写者（8.1 的硬约束）。
/// 未挂载时产品接口一律 401——装配缺失不等于「匿名用户可以访问」。
fn serve(flags: &[String]) -> Result<String, String> {
    use bod_app::host_handshake::{HostServer, default_descriptor_path};

    let mut listen: Option<String> = None;
    let mut descriptor_path: Option<std::path::PathBuf> = None;
    let mut db: Option<std::path::PathBuf> = None;
    let mut company: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--listen" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--listen 需要一个地址".to_string())?;
                listen = Some(value.clone());
                index += 2;
            }
            "--descriptor" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--descriptor 需要一个路径".to_string())?;
                descriptor_path = Some(std::path::PathBuf::from(value));
                index += 2;
            }
            "--db" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--db 需要一个路径".to_string())?;
                db = Some(std::path::PathBuf::from(value));
                index += 2;
            }
            "--company" => {
                let value = flags
                    .get(index + 1)
                    .ok_or_else(|| "--company 需要一个公司 ID".to_string())?;
                company = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("serve 不支持参数 {other}\n\n{USAGE}")),
        }
    }

    let config = BodConfig::default();
    // CR-002：诊断日志装到 stderr（stdout 留给就绪行与命令结果）。
    // 装不上只报告，不让宿主拒绝启动——日志参数错不该等于服务起不来。
    if let Err(err) = bod_app::telemetry::init_from_env() {
        eprintln!("{err}");
    }
    let listen = listen.unwrap_or_else(|| config.server.listen.clone());
    let descriptor_path = descriptor_path.unwrap_or_else(default_descriptor_path);
    let company = company.unwrap_or_else(|| {
        bod_app::authorization::default_company()
            .id
            .as_str()
            .to_string()
    });
    // 显式 `--db` 必须存在：路径写错时宁可启动失败，也不要建出一个空库让所有请求变 404。
    let database = resolve_database(db, &default_database_path())?;
    // 事件源就是 `GET /_bod/events` 的 SSE 基线订阅来源；宿主自己发布的事件也走同一条序列。
    let feed = std::sync::Arc::new(bod_app::event_stream::MemoryFeed::with_capacity(1024));
    let mut server = HostServer::bind_with_feed(&listen, &descriptor_path, now_ms(), feed.clone())
        .map_err(|err| format!("宿主启动失败：{err}"))?;
    // 产品接口装配（P8 7.6.2）：主体在**这里**定一次，请求无权改。
    let product_api = match &database {
        Some(path) => {
            let wiring = delivery_wiring(path, &company)?;
            let note = format!(
                "产品接口：GET /api/v1/delivery-tracking/{{id}}（主体 {} / 公司 {company} / 库 {} 只读挂载）",
                wiring.principal_id,
                path.display()
            );
            server = server.with_delivery(wiring.read, wiring.actor);
            note
        }
        None => format!(
            "产品接口未装配：默认库 {} 不存在（只读挂载不创建库）；用 --db 指向既有库。在此之前 /api/v1/* 一律 401。",
            default_database_path().display()
        ),
    };
    let ready_sequence = feed.publish(
        "host.ready",
        serde_json::json!({
            "address": server.descriptor().address,
            "instance_id": server.descriptor().instance_id,
            "protocol": bod_app::host_handshake::PROTOCOL,
        })
        .to_string(),
    );
    let summary = server.descriptor().public_summary();
    if json {
        let line = serde_json::json!({
            "ready": true,
            "address": server.descriptor().address,
            "pid": server.descriptor().pid,
            "instance_id": server.descriptor().instance_id,
            "descriptor": descriptor_path.display().to_string(),
            "events_sequence": ready_sequence,
        });
        println!("{line}");
        // 机器可读的那一行保持只有一个 JSON 对象；装配说明走 stderr。
        eprintln!("{product_api}");
    } else {
        println!("控制面已就绪：{summary}");
        println!(
            "描述文件：{}（含握手令牌，权限 0600；不写入日志）",
            descriptor_path.display()
        );
        println!("订阅端点：GET /_bod/events（Bearer 会话密钥，SSE，游标持久）");
        println!("{product_api}");
    }
    // spawner 依赖这一行判断就绪，必须立即刷出。
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    server.run().map_err(|err| format!("宿主运行失败：{err}"))?;
    Ok("控制面已停止，连接描述已移除\n".to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(output) => print!("{output}"),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_config(content: &str) -> std::path::PathBuf {
        let n = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("bod-config-test-{}-{n}.toml", std::process::id()));
        fs::write(&path, content).expect("写测试配置应成功");
        path
    }

    #[test]
    fn default_explain_covers_every_schema_field() {
        let args = vec!["config".to_string(), "explain".to_string()];
        let out = run(&args).expect("默认 explain 应成功");
        for field in FIELD_DOCS {
            assert!(out.contains(&format!("[{}]", field.path)));
            assert!(out.contains(field.semantics));
        }
        assert!(out.contains("文件覆盖：0"));
    }

    #[test]
    fn schema_command_lists_all_defaults() {
        let args = vec!["config".to_string(), "schema".to_string()];
        let out = run(&args).expect("schema 应成功");
        for field in FIELD_DOCS {
            assert!(out.contains(field.path));
            assert!(out.contains(field.default));
        }
    }

    #[test]
    fn validate_command_accepts_partial_config() {
        let path = temp_config("[server]\nlisten = \"127.0.0.1:8080\"\n");
        let p = path.to_string_lossy().to_string();
        let args = vec![
            "config".to_string(),
            "validate".to_string(),
            "--file".to_string(),
            p,
        ];
        let out = run(&args).expect("合法配置应通过校验");
        assert!(out.contains("文件覆盖 1 个字段"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn validate_command_rejects_unknown_field() {
        let path = temp_config("[server]\nauth_require = true\n");
        let p = path.to_string_lossy().to_string();
        let args = vec![
            "config".to_string(),
            "validate".to_string(),
            "--file".to_string(),
            p,
        ];
        let err = run(&args).expect_err("未知字段必须被拒绝");
        assert!(err.contains("server.auth_require"), "实际错误: {err}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn conformance_command_lists_every_section_without_overall_badge() {
        let args = vec!["conformance".to_string(), "--fake-only".to_string()];
        let out = run(&args).expect("conformance --fake-only 应成功");
        for needle in [
            "适配器版本",
            "协议",
            "平台",
            "生成日期",
            "能力分项",
            "用例分项",
            "启用判定",
            "结论口径",
            "不可归约为单一通过标记",
        ] {
            assert!(out.contains(needle), "报告缺少 {needle}:\n{out}");
        }
        assert!(!out.contains("PASS"), "CLI 不得输出整体通过标记:\n{out}");
        // 未测能力如实出现，不被隐藏。
        assert!(out.contains("not_declared") || out.contains("declared_unsupported"));
        assert!(
            out.contains("不提供生产能力"),
            "Fake 的生产能力排除说明必须出现"
        );
    }

    #[test]
    fn conformance_json_is_a_valid_document_without_passing_flag() {
        let args = vec![
            "conformance".to_string(),
            "--fake-only".to_string(),
            "--json".to_string(),
        ];
        let out = run(&args).expect("conformance --json 应成功");
        let value: serde_json::Value =
            serde_json::from_str(&out).expect("输出必须是合法 JSON 文档");
        let reports = value["reports"].as_array().expect("reports 必须是数组");
        assert_eq!(reports.len(), 1);
        assert!(reports[0]["capabilities"].as_array().unwrap().len() >= 13);
        assert!(reports[0]["cases"].as_array().unwrap().len() >= 10);
        assert_eq!(reports[0]["verdict"]["reducible"], false);
        assert!(value.get("passed").is_none(), "不得有单一通过标记");
    }

    #[test]
    fn adapter_capabilities_rejects_unknown_level_before_touching_the_adapter() {
        let args = vec![
            "adapter".to_string(),
            "capabilities".to_string(),
            "--need".to_string(),
            "l9".to_string(),
        ];
        let err = run(&args).expect_err("未知等级必须失败");
        assert!(err.contains("--need 只接受 l1 / l2 / l3"), "{err}");
    }

    #[test]
    fn conformance_rejects_unknown_flag() {
        let args = vec!["conformance".to_string(), "--bogus".to_string()];
        let err = run(&args).expect_err("未知参数必须失败");
        assert!(err.contains("不支持参数 --bogus"));
    }

    #[test]
    fn release_check_lists_every_item_and_never_claims_supported() {
        let args = vec![
            "adapter".to_string(),
            "release-check".to_string(),
            "--need".to_string(),
            "l3".to_string(),
        ];
        let out = run(&args).expect("release-check 应成功");
        for item in [
            "stop_restart",
            "replay_reconnect",
            "effect_authorization",
            "account_budget",
            "acceptance_evidence",
            "log_redaction",
        ] {
            assert!(out.contains(item), "缺少验收项 {item}:\n{out}");
        }
        // 本机适配器未声明去重保留期，这一项必须是 fail 而不是「大概可以」。
        assert!(out.contains("no_dedup_retention"), "{out}");
        assert!(out.contains("结论: blocked"), "{out}");
        assert!(
            !out.contains("结论: releasable"),
            "证据不足时不得给出可发布结论:\n{out}"
        );
        // 三处一致必须真的比对过，并列出范围。
        assert!(out.contains("能力缺失三处一致"), "{out}");
        assert!(out.contains("recovery"), "{out}");
    }

    #[test]
    fn release_check_flags_credentials_in_logs_without_echoing_them() {
        let n = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("bod-release-log-{}-{n}.log", std::process::id()));
        fs::write(
            &path,
            "boot ok\nAuthorization: Bearer sk-live-abcdefghijklmnopqrstuvwxyz012345\n",
        )
        .expect("写测试日志应成功");
        let args = vec![
            "adapter".to_string(),
            "release-check".to_string(),
            "--logs".to_string(),
            path.to_string_lossy().to_string(),
        ];
        let out = run(&args).expect("release-check --logs 应成功");
        assert!(out.contains("credentials_in_logs"), "{out}");
        assert!(
            !out.contains("sk-live-abcdefghijklmnopqrstuvwxyz012345"),
            "输出不得包含原始凭据:\n{out}"
        );
        assert!(out.contains("<redacted:"), "证据必须保留脱敏后的行:\n{out}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn release_check_json_is_a_document_with_per_item_states() {
        let args = vec![
            "adapter".to_string(),
            "release-check".to_string(),
            "--need".to_string(),
            "l2".to_string(),
            "--json".to_string(),
        ];
        let out = run(&args).expect("release-check --json 应成功");
        let value: serde_json::Value =
            serde_json::from_str(&out).expect("输出必须是合法 JSON 文档");
        assert_eq!(value["schema"], "bod.release.acceptance/v1");
        let checks = value["checks"].as_array().expect("checks 必须是数组");
        assert_eq!(checks.len(), 6);
        assert_eq!(value["verdict"]["releasable"], false);
        assert!(value.get("passed").is_none(), "不得有单一通过标记");
        assert_eq!(value["alignment"]["compared"], true);
    }

    #[test]
    fn release_check_rejects_unknown_level_before_touching_the_adapter() {
        let args = vec![
            "adapter".to_string(),
            "release-check".to_string(),
            "--need".to_string(),
            "l9".to_string(),
        ];
        let err = run(&args).expect_err("未知等级必须失败");
        assert!(err.contains("--need 只接受 l1 / l2 / l3"), "{err}");
    }

    #[test]
    fn unknown_command_returns_usage() {
        let args = vec!["bogus".to_string()];
        let err = run(&args).expect_err("未知命令必须失败");
        assert!(err.contains("用法"));
    }

    /// 显式 `--db` 指向不存在的文件：必须失败，且不得顺手创建它。
    ///
    /// 这条挡的是「路径写错 → 建出空库 → 每个请求 404」：那时运维会去查权限，
    /// 而真正的原因是控制面读了一份刚被自己造出来的空库。
    #[test]
    fn explicit_database_must_exist_and_is_never_created() {
        let dir = std::env::temp_dir().join(format!("bod-serve-db-{}-missing", std::process::id()));
        let missing = dir.join("absent.db");
        let err = resolve_database(Some(missing.clone()), &dir.join("default.db"))
            .expect_err("显式路径不存在必须失败");
        assert!(err.contains("--db 指向的数据库不存在"), "{err}");
        assert!(!missing.exists(), "不得创建数据库文件");
    }

    /// 缺省库存在时自动挂载：控制面不需要额外参数就能读。
    #[test]
    fn default_database_is_mounted_only_when_it_exists() {
        let dir = std::env::temp_dir().join(format!("bod-serve-db-{}-default", std::process::id()));
        fs::create_dir_all(&dir).expect("建临时目录");
        let existing = dir.join("bod.db");
        fs::write(&existing, b"not-a-real-db").expect("写占位文件");

        let mounted = resolve_database(None, &existing).expect("存在时应挂载");
        assert_eq!(mounted.as_deref(), Some(existing.as_path()));

        let absent = dir.join("nothing.db");
        let not_mounted = resolve_database(None, &absent).expect("不存在不是错误");
        assert!(not_mounted.is_none(), "默认库不存在时不挂载，也不建库");
        assert!(!absent.exists(), "不得创建数据库文件");
        let _ = fs::remove_dir_all(&dir);
    }
}
