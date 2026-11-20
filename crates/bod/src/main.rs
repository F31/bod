//! bod 宿主 CLI：P0 D0 阶段提供最小 headless 启动与配置 schema/explain 入口。
//! HTTP 控制面与真实宿主启动在后续切片接入；当前不启动服务、不写运行时数据。

use bod_app::adapter_conformance::{run_fake_suite, run_live_suite};
use bod_app::config::{BodConfig, FIELD_DOCS};
use bod_app::conformance_report::SectionedReport;
use bod_app::fake_executor::{FakeExecutor, FakeScenario};
use bod_app::invocation::StartTransport;
use bod_app::local_process::{default_adapter, descriptor_json};
use bod_app::{explain, load_from_str, schema_text};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
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
  bod adapter conformance          运行 Fake + 本机真实适配器的 conformance 报告
  bod conformance                  分项报告：按版本/协议/平台/能力/日期逐项列出，不出整体通过标记
  bod conformance --fake-only      只跑 Fake Executor 的分项报告
  bod conformance --json           同上，输出机器可读结果（多适配器时为 reports 数组）
  bod smoke                        跑通指派→执行→候选登记→重启找回的最小冒烟
  bod smoke --json                 同上，输出机器可读结果
  bod fault-matrix                 执行 P0 五个核心故障场景并输出实测报告
  bod fault-matrix --json          同上，输出机器可读结果
  bod perf                         实测空闲负担与 §38.2 性能预算可测项
  bod verify-index                 输出 I/G 验证索引与 G-50 结论
  bod serve                        启动本地控制面并写出连接描述（受保护握手）
  bod serve --listen <addr>        指定监听地址（默认取 server.listen，可为 127.0.0.1:0）
  bod serve --descriptor <path>    指定连接描述路径（默认 $BOD_HOME/host.endpoint.json）
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

fn json_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
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
            Ok(bod_app::perf_probe::render_markdown(&report))
        }
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
        _ => Err(format!("未知命令: {}\n\n{}", args.join(" "), USAGE)),
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

/// `bod serve`：起本地控制面，阻塞到收到带会话密钥的停止请求。
///
/// P0 的控制面只做握手与生命周期，不承载业务命令；业务 DB 仍由命令层独占，
/// 因此这里**不打开第二个业务库**（8.1 的硬约束）。
fn serve(flags: &[String]) -> Result<String, String> {
    use bod_app::host_handshake::{HostServer, default_descriptor_path};

    let mut listen: Option<String> = None;
    let mut descriptor_path: Option<std::path::PathBuf> = None;
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
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("serve 不支持参数 {other}\n\n{USAGE}")),
        }
    }

    let config = BodConfig::default();
    let listen = listen.unwrap_or_else(|| config.server.listen.clone());
    let descriptor_path = descriptor_path.unwrap_or_else(default_descriptor_path);
    let server = HostServer::bind(&listen, &descriptor_path, now_ms())
        .map_err(|err| format!("宿主启动失败：{err}"))?;
    let summary = server.descriptor().public_summary();
    if json {
        let line = serde_json::json!({
            "ready": true,
            "address": server.descriptor().address,
            "pid": server.descriptor().pid,
            "instance_id": server.descriptor().instance_id,
            "descriptor": descriptor_path.display().to_string(),
        });
        println!("{line}");
    } else {
        println!("控制面已就绪：{summary}");
        println!(
            "描述文件：{}（含握手令牌，权限 0600；不写入日志）",
            descriptor_path.display()
        );
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
    fn conformance_rejects_unknown_flag() {
        let args = vec!["conformance".to_string(), "--bogus".to_string()];
        let err = run(&args).expect_err("未知参数必须失败");
        assert!(err.contains("不支持参数 --bogus"));
    }

    #[test]
    fn unknown_command_returns_usage() {
        let args = vec!["bogus".to_string()];
        let err = run(&args).expect_err("未知命令必须失败");
        assert!(err.contains("用法"));
    }
}
