//! bod 宿主 CLI：P0 D0 阶段提供最小 headless 启动与配置 schema/explain 入口。
//! HTTP 控制面与真实宿主启动在后续切片接入；当前不启动服务、不写运行时数据。

use bod_app::adapter_conformance::{run_fake_suite, run_live_suite};
use bod_app::config::{BodConfig, FIELD_DOCS};
use bod_app::local_process::{default_adapter, descriptor_json};
use bod_app::{explain, load_from_str, schema_text};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

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
  bod --help                       显示本帮助
";

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
        _ => Err(format!("未知命令: {}\n\n{}", args.join(" "), USAGE)),
    }
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
    fn unknown_command_returns_usage() {
        let args = vec!["bogus".to_string()];
        let err = run(&args).expect_err("未知命令必须失败");
        assert!(err.contains("用法"));
    }
}
