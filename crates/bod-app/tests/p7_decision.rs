//! P7 §3（3.1—3.5）的漂移检查：决定记录必须能被解析、通过门禁，且引用的数字出自真实报告。

use std::path::PathBuf;

use bod_app::bottleneck::guard_replicate_side_effect_package;
use bod_app::capacity_evaluation::{
    CandidateAssessment, Comparison, DecisionLog, EngineCandidate, EngineChange,
    GraduationDecision, compare_candidates, dao_swap_is_not_semantic_compat,
    guard_change_in_capacity_stage, guard_support_promotion,
};

const DECISION: &str = include_str!("../../../docs/reports/p7-decision.md");
const LINUX: &str = include_str!("../../../docs/reports/p7-baseline-linux.md");
const WINDOWS: &str = include_str!("../../../docs/reports/p7-baseline-windows.md");
const MATRIX: &str = include_str!("../../../docs/spec-notes/p7-measurement.md");
const SUPPORT: &str = include_str!("../../../docs/spec-notes/p0-support-matrix.md");
const TASKS: &str = include_str!("../../../openspec/changes/p7-capacity-evaluation/tasks.md");

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

/// 取文档里的 ```json 代码块并解析。
fn decision_json() -> serde_json::Value {
    let start = DECISION.find("```json").expect("决定记录必须有 json 块") + "```json".len();
    let rest = &DECISION[start..];
    let end = rest.find("```").expect("json 块必须闭合");
    serde_json::from_str(rest[..end].trim()).expect("json 块必须能解析")
}

fn candidate(value: &serde_json::Value) -> CandidateAssessment {
    let name = value["candidate"].as_str().expect("candidate 名");
    let candidate = EngineCandidate::ALL
        .iter()
        .copied()
        .find(|item| item.as_str() == name)
        .unwrap_or_else(|| panic!("未知候选 {name}"));
    CandidateAssessment {
        candidate,
        migration_cost: value["migration_cost"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        ops_burden: value["ops_burden"].as_str().unwrap_or_default().to_string(),
        failure_semantics: value["failure_semantics"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        recovery_evidence: value["recovery_evidence"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        requires_new_change: value["requires_new_change"].as_bool().unwrap_or(false),
    }
}

fn decision(value: &serde_json::Value) -> GraduationDecision {
    let comparison = value["comparison"]
        .as_array()
        .expect("comparison 必须存在")
        .iter()
        .filter_map(|item| item.as_str())
        .filter_map(|name| {
            EngineCandidate::ALL
                .iter()
                .copied()
                .find(|candidate| candidate.as_str() == name)
        })
        .collect();
    GraduationDecision {
        decision_id: value["decision_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        revision: value["revision"].as_u64().unwrap_or(0) as u32,
        bottleneck: value["bottleneck"].as_str().unwrap_or_default().to_string(),
        report_ref: value["report_ref"].as_str().unwrap_or_default().to_string(),
        environment: value["environment"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        comparison,
        chosen: value["chosen"].as_str().unwrap_or_default().to_string(),
        owner: value["owner"].as_str().unwrap_or_default().to_string(),
        rollback_path: value["rollback_path"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        uncovered: value["uncovered"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|text| text.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        retest_trigger: value["retest_trigger"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        decided_at: value["decided_at"].as_str().unwrap_or_default().to_string(),
    }
}

/// 3.1：四类候选齐备，且每项都有迁移代价、运维负担、故障语义与恢复证据。
#[test]
fn task_3_1_all_four_candidates_have_cost_burden_semantics_and_evidence() {
    let json = decision_json();
    let assessments: Vec<CandidateAssessment> = json["candidates"]
        .as_array()
        .expect("candidates 必须存在")
        .iter()
        .map(candidate)
        .collect();
    assert_eq!(assessments.len(), 4, "四类候选都要有记录");
    match compare_candidates(&assessments) {
        Comparison::Complete { candidates } => assert_eq!(candidates.len(), 4),
        other => panic!("对比不完整：{other:?}"),
    }
    for assessment in &assessments {
        assert!(
            assessment.is_complete(),
            "{} 的代价/负担/语义/证据有空白",
            assessment.candidate.as_str()
        );
        assert!(
            assessment.recovery_evidence.contains("未取得")
                || assessment.recovery_evidence.contains("RTO")
                || assessment.recovery_evidence.contains("演练"),
            "{} 的恢复证据要写清是有演练还是未取得",
            assessment.candidate.as_str()
        );
    }
    // 没有任何一项可以在本 Change 内直接落地。
    assert!(
        assessments.iter().all(|item| item.requires_new_change),
        "引擎或写入路径变更必须另建 Change"
    );
}

/// 3.2：DAO 替换不算语义兼容；候选必须保留单写次序与并发/恢复演练要求。
#[test]
fn task_3_2_candidates_keep_single_writer_order_and_real_drills() {
    assert!(dao_swap_is_not_semantic_compat().is_err());
    assert!(
        DECISION.contains("DAO 替换不算语义兼容"),
        "决定记录必须写明 DAO 替换的边界"
    );

    let json = decision_json();
    for value in json["candidates"].as_array().expect("candidates") {
        let assessment = candidate(value);
        assert!(
            assessment.failure_semantics.contains("单写"),
            "{} 的故障语义必须交代单写次序",
            assessment.candidate.as_str()
        );
    }

    // 真实并发测试与备份恢复演练是硬要求：记录里要有"并发"和"演练/恢复"字样。
    assert!(DECISION.contains("真实并发"));
    assert!(DECISION.contains("备份恢复演练"));
    assert!(
        MATRIX.contains("真实并发") || MATRIX.contains("并发"),
        "手册要写明候选方案的并发测试要求"
    );
}

/// 3.3：决定可审计、只追加；缺项进不了日志。
#[test]
fn task_3_3_decision_is_auditable_and_append_only() {
    let json = decision_json();
    let first = decision(&json["decisions"][0]);

    assert!(
        first.audit_gaps().is_empty(),
        "决定缺项：{:?}",
        first.audit_gaps()
    );
    assert!(!first.uncovered.is_empty(), "未覆盖条件必须写出来");
    assert!(!first.retest_trigger.is_empty(), "复测触发必须写出来");
    assert!(
        first.comparison.len() == EngineCandidate::ALL.len(),
        "决定必须引用完整对比"
    );
    assert!(
        first
            .comparison
            .iter()
            .any(|item| item.as_str() == first.chosen),
        "选定方案必须来自对比列表"
    );

    let mut log = DecisionLog::new();
    assert_eq!(log.record(first.clone()), Ok(0));
    assert!(
        log.record(first.clone()).is_err(),
        "同一决定不得覆盖：历史依据不能被改写"
    );

    // 修订走追加：旧版本原样保留。
    let mut revised = first.clone();
    revised.chosen = "optimization + executor_scale".to_string();
    assert_eq!(log.revise(&first.decision_id, revised), Ok(1));
    assert_eq!(log.versions(&first.decision_id), 2);
    assert_eq!(
        log.earliest(&first.decision_id)
            .map(|item| item.chosen.as_str()),
        Some("optimization")
    );
    assert_eq!(
        log.latest(&first.decision_id)
            .map(|item| item.chosen.as_str()),
        Some("optimization + executor_scale")
    );
}

/// 3.3：决定引用的报告必须真实存在，且引用的数字能在报告里找到。
#[test]
fn task_3_3_decision_cites_real_reports_and_real_numbers() {
    let json = decision_json();
    let first = decision(&json["decisions"][0]);
    let report = root().join(&first.report_ref);
    assert!(
        report.exists(),
        "report_ref 指向的报告不存在：{}",
        first.report_ref
    );

    // 瓶颈描述里的关键数字必须能在报告里逐字找到，避免手写估数。
    for needle in ["5.31", "182.37", "Busy 5"] {
        assert!(
            LINUX.contains(needle),
            "Linux 报告缺少决定引用的数字 `{needle}`"
        );
    }
    for needle in ["1.68", "338.57"] {
        assert!(
            WINDOWS.contains(needle),
            "Windows 报告缺少决定引用的数字 `{needle}`"
        );
    }
    assert!(
        first.bottleneck.contains("p99") && first.bottleneck.contains("Busy"),
        "瓶颈描述必须带 p99 与 Busy 证据"
    );

    // 瓶颈描述里的每个数字都必须能在报告里逐字找到：防止手写估数混进决定记录。
    let haystack = format!("{LINUX}\n{WINDOWS}");
    for token in numeric_tokens(&first.bottleneck) {
        assert!(
            haystack.contains(&token),
            "决定引用的数字 `{token}` 在两份报告里都找不到"
        );
    }
}

/// 取文本里的小数与三位以上整数（其余短数字噪声太大，不参与核对）。
fn numeric_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens.retain(|token| {
        token.contains('.')
            || token
                .parse::<u64>()
                .map(|value| value >= 100)
                .unwrap_or(false)
    });
    tokens
}

/// 3.3：报告内部自洽——表格里的实测值必须与详情段一致（防止手改一处数字）。
#[test]
fn task_3_3_archived_reports_are_internally_consistent() {
    for (name, report) in [("linux", LINUX), ("windows", WINDOWS)] {
        let table = table_values(report);
        assert!(!table.is_empty(), "{name} 报告没有测量项表格");
        let detail = detail_values(report);
        for (item, value) in &table {
            let shown = detail
                .get(item)
                .unwrap_or_else(|| panic!("{name} 报告缺少 {item} 的详情段"));
            assert_eq!(shown, value, "{name} 报告的 {item} 表格值与详情值不一致");
        }
    }
}

fn table_values(report: &str) -> Vec<(String, String)> {
    report
        .lines()
        .filter(|line| line.trim_start().starts_with("| `"))
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').collect();
            if cells.len() < 7 {
                return None;
            }
            let name = cells[1].trim().trim_matches('`').to_string();
            Some((name, cells[4].trim().to_string()))
        })
        .collect()
}

fn detail_values(report: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let mut current: Option<String> = None;
    for line in report.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            current = Some(rest.trim().to_string());
            continue;
        }
        if let (Some(rest), Some(name)) = (line.strip_prefix("- 实测："), current.clone()) {
            map.insert(name, rest.trim().to_string());
        }
    }
    map
}

/// 3.4：容量评估不改变业务语义；任何引擎变更都被阻断。
#[test]
fn task_3_4_no_engine_change_inside_the_capacity_stage() {
    for change in [
        EngineChange::SwitchEngine {
            target: "postgres".to_string(),
        },
        EngineChange::LowerSyncLevel {
            level: "NORMAL".to_string(),
        },
        EngineChange::WidenTransaction,
        EngineChange::SkipGate {
            gate: "G-21".to_string(),
        },
    ] {
        assert!(
            guard_change_in_capacity_stage(&change).is_err(),
            "{change:?} 必须被阻断"
        );
    }
    assert!(guard_replicate_side_effect_package().is_err());
    assert!(
        DECISION.contains("未改变业务语义或数据"),
        "决定记录必须声明本 Change 没动语义与数据"
    );
    assert!(
        DECISION.contains("独立 Change"),
        "引擎变更要求独立 Change 与迁移门禁"
    );
}

/// 3.5：性能结论没扩大支持矩阵（G-26 回归）。
#[test]
fn task_3_5_support_matrix_is_not_promoted_by_performance() {
    assert!(
        SUPPORT.contains("Supported：**0** 条"),
        "支持矩阵必须仍是 0 Supported"
    );
    assert!(
        DECISION.contains("未扩大支持矩阵"),
        "决定记录必须声明没有扩大支持矩阵"
    );

    // 即便 headless 全绿，也不能凭性能数字提升远程能力。
    let json = decision_json();
    let report_ref = report_from_decision(&json);
    assert!(
        guard_support_promotion("remote_node", &report_ref, false).is_err(),
        "没有 conformance 与真机/双机报告时不得提升"
    );
}

/// 用真实报告构造一份 BaselineReport 供门禁检查（只取报告抬头，不重跑测量）。
fn report_from_decision(json: &serde_json::Value) -> bod_app::capacity_evaluation::BaselineReport {
    use bod_app::capacity_evaluation::{
        BaselineReport, EnvironmentRecord, FormFactor, MeasureItem,
    };
    let first = decision(&json["decisions"][0]);
    BaselineReport {
        environment: EnvironmentRecord {
            note: first.environment.clone(),
            os: "linux x86_64".to_string(),
            cpu: "24 逻辑核".to_string(),
            rustc: "rustc 1.99.1".to_string(),
            build_params: "dev profile".to_string(),
            sqlite_sync: "WAL + synchronous=FULL + BEGIN IMMEDIATE".to_string(),
            data_scale: "10 万事件".to_string(),
            cache_state: "冷".to_string(),
        },
        commit: "from-decision".to_string(),
        items: vec![
            MeasureItem::new(
                "event_throughput_batched",
                FormFactor::Headless,
                "≥ 5 000 events/s",
            )
            .measured("58359 events/s", 5_000, "xtask bench", Some(true)),
        ],
    }
}

/// tasks 3.x 必须勾选并带证据。
#[test]
fn tasks_3_x_are_checked_with_evidence() {
    let section = TASKS
        .split("## 3. 方案对比与决定")
        .nth(1)
        .expect("tasks 必须有 §3")
        .split("## 4.")
        .next()
        .unwrap_or_default();
    for id in ["3.1", "3.2", "3.3", "3.4", "3.5"] {
        assert!(
            section.contains(&format!("- [x] {id}")),
            "tasks 的 {id} 尚未勾选"
        );
    }
    assert!(
        section.contains("docs/reports/p7-decision.md"),
        "tasks 要指向决定记录"
    );
}
