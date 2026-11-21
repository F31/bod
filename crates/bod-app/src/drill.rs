//! P2—P6 真实演练工具包：统一记录字段、模板与校验（A5）。
//!
//! 这套工具只做一件事：**把演练现场能读到的事实记下来**，并保证"没填完就不能当记录"。
//! 它刻意不提供"通过/失败"结论——判定属人工评审（出口报告），工具给结论会把
//! "记录完整"误读成"验收通过"。
//!
//! 因此 [`DrillRecord`] 里没有 `conclusion` / `passed` / `result` 字段，
//! 反序列化时带这些字段会**直接失败**（`deny_unknown_fields`）：不允许把判定塞进记录。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 记录 schema；格式变化必须换版本，旧记录按旧版本解释。
pub const DRILL_SCHEMA: &str = "bod.drill/v1";

/// 演练所属阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DrillPhase {
    P2,
    P3,
    P4,
    P5,
    P6,
}

impl DrillPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P2 => "P2",
            Self::P3 => "P3",
            Self::P4 => "P4",
            Self::P5 => "P5",
            Self::P6 => "P6",
        }
    }

    pub fn all() -> [Self; 5] {
        [Self::P2, Self::P3, Self::P4, Self::P5, Self::P6]
    }
}

/// 一个演练案件的定义：需要哪些步骤、必须记哪些读数、哪些缺口本机证不了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrillCase {
    pub id: &'static str,
    pub phase: DrillPhase,
    /// 对应的要求编号（RM/MD/SB 或门禁编号），便于评审时对齐规格。
    pub requirement: &'static str,
    /// 一句话说明要复现什么。
    pub scenario: &'static str,
    /// 是否需要真实双机/真机/真实账号；本机单进程无法自证。
    pub needs_real_env: bool,
    /// 必须执行的步骤。
    pub steps: &'static [&'static str],
    /// 必须记录的读数键（缺一项即记录不完整）。
    pub readings: &'static [&'static str],
}

/// P2—P6 的真实演练清单。
///
/// 只收"必须离开本机才能证明"的案件：能在单进程里断言的不进这张表，
/// 否则会出现"演练做完了、其实什么都没验证"。
pub const DRILL_CASES: &[DrillCase] = &[
    DrillCase {
        id: "p2-stop-foreign-writer",
        phase: DrillPhase::P2,
        requirement: "G-41 / I-48",
        scenario: "旧写入者未停时新尝试不得重复写同一资源，脏文件保留且需显式确认",
        needs_real_env: true,
        steps: &[
            "在真实仓库里让旧执行端保持未提交改动",
            "发起接续尝试并观察是否拒绝写同一资源",
            "记录脏文件清单与确认方式",
        ],
        readings: &[
            "host",
            "repo_path",
            "dirty_files",
            "rejection_reason",
            "manual_confirmation",
        ],
    },
    DrillCase {
        id: "p2-takeover-without-summary",
        phase: DrillPhase::P2,
        requirement: "G-42 / I-65",
        scenario: "无旧经理总结、部分资料缺失、快照后新增成果时仍能查缺口与增量",
        needs_real_env: true,
        steps: &[
            "让旧经理离场且不留总结",
            "由新经理打开接续检查点",
            "对照快照后新增的员工成果",
        ],
        readings: &[
            "host",
            "gaps_listed",
            "delta_added",
            "delta_changed",
            "old_summary_present",
        ],
    },
    DrillCase {
        id: "p3-interface-change",
        phase: DrillPhase::P3,
        requirement: "G-31",
        scenario: "真实接口变更只重跑相关检查，关系缺失显示 unknown",
        needs_real_env: true,
        steps: &[
            "改动一个被引用的公开接口签名",
            "触发影响分析并记录受影响区域",
            "记录未登记关系的区域是否标 unknown",
        ],
        readings: &[
            "host",
            "changed_symbol",
            "affected_regions",
            "unknown_regions",
            "rerun_checks",
        ],
    },
    DrillCase {
        id: "p4-joint-delivery",
        phase: DrillPhase::P4,
        requirement: "G-32 / G-33",
        scenario: "固定包与真实环境中工具失败与应用失败可区分，内容篡改会失败",
        needs_real_env: true,
        steps: &[
            "在固定包环境跑真实场景",
            "人为制造工具失败并记录区分依据",
            "篡改一次产物内容并观察验证结果",
        ],
        readings: &[
            "host",
            "package_version",
            "tool_failure_signal",
            "app_failure_signal",
            "tampered_rejected",
        ],
    },
    DrillCase {
        id: "p5-feedback-conflict",
        phase: DrillPhase::P5,
        requirement: "G-13",
        scenario: "真实反馈污染/冲突/回滚：普通反馈不得修改已批准验收条件",
        needs_real_env: true,
        steps: &[
            "用真实账号提交普通反馈",
            "与已批准验收条件冲突并观察归口",
            "执行规则回滚并确认不回滚历史验收",
        ],
        readings: &[
            "host",
            "actor",
            "criteria_unchanged",
            "conflict_routed_to",
            "ledger_unchanged",
        ],
    },
    DrillCase {
        id: "p6-rm01-lost-reply",
        phase: DrillPhase::P6,
        requirement: "RM01",
        scenario: "丢回包重启后返回原实例，改参被拒",
        needs_real_env: true,
        steps: &[
            "真实节点启动一次实例",
            "丢弃回包后重启宿主",
            "以同 ID 异参再次启动",
        ],
        readings: &[
            "host",
            "node",
            "original_instance",
            "replayed_instance",
            "param_conflict_rejected",
        ],
    },
    DrillCase {
        id: "p6-rm06-network-switch",
        phase: DrillPhase::P6,
        requirement: "RM06",
        scenario: "切网/游标过期时订阅按持久游标补拉，控制命令显示真实回执",
        needs_real_env: true,
        steps: &[
            "订阅 /_bod/events 并记录最后游标",
            "断开网络或让游标早于保留窗口",
            "重连后核对补齐方式与回执显示",
        ],
        readings: &[
            "host",
            "network",
            "last_cursor",
            "resume_mode",
            "receipt_display",
        ],
    },
    DrillCase {
        id: "p6-rm07-signed-batch",
        phase: DrillPhase::P6,
        requirement: "RM07",
        scenario: "篡改批次被拒，合法批次只入账一次且不自动通过验收",
        needs_real_env: true,
        steps: &[
            "真实节点上报签名批次",
            "篡改一条批次内容后重放",
            "重放同内容合法批次",
        ],
        readings: &[
            "host",
            "node",
            "tampered_rejected",
            "batch_applied_once",
            "acceptance_unchanged",
        ],
    },
    DrillCase {
        id: "p6-rm09-mailbox",
        phase: DrillPhase::P6,
        requirement: "RM09",
        scenario: "信箱满额明确反馈，两端相反决定至多一次生效",
        needs_real_env: true,
        steps: &["填满信箱并观察拒绝反馈", "两端同键相反决定各提交一次"],
        readings: &[
            "host",
            "capacity",
            "rejected_feedback",
            "effective_decision",
            "conflicting_decision",
        ],
    },
    DrillCase {
        id: "p6-double-host",
        phase: DrillPhase::P6,
        requirement: "P6 2.8",
        scenario: "双机部署下任意回包断点留下可核对事实与下一步",
        needs_real_env: true,
        steps: &[
            "两台机器各起宿主并记录设备与时延",
            "在回包途中切断一次",
            "核对两侧留下的核对事实",
        ],
        readings: &[
            "host_a",
            "host_b",
            "network",
            "rtt_ms",
            "breakpoint",
            "facts_recorded",
        ],
    },
];

pub fn case_by_id(id: &str) -> Option<&'static DrillCase> {
    DRILL_CASES.iter().find(|case| case.id == id)
}

/// 演练环境事实。**不写"环境正常"这类判断**，只写可核对的取值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrillEnvironment {
    /// 机器标识（主机名/设备编号），不是 IP。
    pub host: String,
    pub os: String,
    pub arch: String,
    /// 网络条件：例如 `lan-1g`、`5g-4bars`、`离线`。
    pub network: String,
    /// 参与端数量；单机案件写 1。
    pub peers: usize,
    /// 设备与包版本等追加事实。
    #[serde(default)]
    pub notes: BTreeMap<String, String>,
}

/// 一步的观察记录：看到了什么，证据在哪。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepRecord {
    pub step: String,
    /// 观察到的现象，必须是可核对的事实，不是"正常"。
    pub observation: String,
    /// 证据引用（日志路径、报告编号、截图名）；没有就留空，不编造。
    #[serde(default)]
    pub evidence: String,
    pub at_ms: i64,
}

/// 一次演练记录。**没有结论字段**：判定由人工评审在出口报告里给出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrillRecord {
    pub schema: String,
    pub case: String,
    pub phase: DrillPhase,
    pub commit: String,
    /// 执行者（账号或姓名），便于回溯是谁复现的。
    pub operator: String,
    pub started_at_ms: i64,
    pub duration_ms: u64,
    pub environment: DrillEnvironment,
    pub steps: Vec<StepRecord>,
    /// 读数键 → 取值。键必须覆盖案件定义的 `readings`。
    pub readings: BTreeMap<String, String>,
    /// 已知缺口：本机证不了、需要下次补的部分。
    #[serde(default)]
    pub gaps: Vec<String>,
}

/// 校验问题。每条都能直接告诉执行者"缺什么"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillProblem(pub String);

impl std::fmt::Display for DrillProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 为某案件生成空白模板：步骤与读数键预置，取值留空等待填写。
///
/// 空白模板**过不了校验**——这是刻意的，避免"生成了模板就算演练过"。
pub fn template(case: &DrillCase, commit: &str, started_at_ms: i64) -> DrillRecord {
    DrillRecord {
        schema: DRILL_SCHEMA.to_string(),
        case: case.id.to_string(),
        phase: case.phase,
        commit: commit.to_string(),
        operator: String::new(),
        started_at_ms,
        duration_ms: 0,
        environment: DrillEnvironment {
            host: String::new(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            network: String::new(),
            peers: 1,
            notes: BTreeMap::new(),
        },
        steps: case
            .steps
            .iter()
            .map(|step| StepRecord {
                step: (*step).to_string(),
                observation: String::new(),
                evidence: String::new(),
                at_ms: 0,
            })
            .collect(),
        readings: case
            .readings
            .iter()
            .map(|key| ((*key).to_string(), String::new()))
            .collect(),
        gaps: Vec::new(),
    }
}

/// 校验记录是否"填完了"。返回空列表才算记录完整——**不代表演练通过**。
pub fn validate(record: &DrillRecord) -> Vec<DrillProblem> {
    let mut problems = Vec::new();

    if record.schema != DRILL_SCHEMA {
        problems.push(DrillProblem(format!(
            "schema 不符：期望 {DRILL_SCHEMA}，实际 {}",
            record.schema
        )));
    }
    let Some(case) = case_by_id(&record.case) else {
        problems.push(DrillProblem(format!(
            "未知案件 {}：只能记录已登记的真实演练案件",
            record.case
        )));
        return problems;
    };
    if record.phase != case.phase {
        problems.push(DrillProblem(format!(
            "阶段不符：{} 属于 {}，记录写成 {}",
            case.id,
            case.phase.as_str(),
            record.phase.as_str()
        )));
    }
    if record.commit.trim().is_empty() {
        problems.push(DrillProblem(
            "commit 为空：记录必须能对应到具体代码".to_string(),
        ));
    }
    if record.operator.trim().is_empty() {
        problems.push(DrillProblem(
            "operator 为空：需要能回溯是谁复现的".to_string(),
        ));
    }
    if record.environment.host.trim().is_empty() {
        problems.push(DrillProblem("environment.host 为空".to_string()));
    }
    if record.environment.network.trim().is_empty() {
        problems.push(DrillProblem(
            "environment.network 为空（需要写明网络条件）".to_string(),
        ));
    }
    if case.needs_real_env && record.environment.peers < 2 {
        problems.push(DrillProblem(format!(
            "{} 需要真实多端环境，peers={} 不足以复现",
            case.id, record.environment.peers
        )));
    }
    if record.steps.len() != case.steps.len() {
        problems.push(DrillProblem(format!(
            "步骤数不符：{} 要求 {} 步，记录 {} 步",
            case.id,
            case.steps.len(),
            record.steps.len()
        )));
    }
    for (index, step) in record.steps.iter().enumerate() {
        if step.observation.trim().is_empty() {
            problems.push(DrillProblem(format!("第 {} 步缺少观察记录", index + 1)));
        }
    }
    for key in case.readings {
        match record.readings.get(*key) {
            Some(value) if !value.trim().is_empty() => {}
            Some(_) => problems.push(DrillProblem(format!("读数 {key} 为空"))),
            None => problems.push(DrillProblem(format!("缺少读数 {key}"))),
        }
    }
    problems
}

/// 记录是否完整（不表示通过）。
pub fn is_complete(record: &DrillRecord) -> bool {
    validate(record).is_empty()
}

/// 从 JSON 读记录。带 `conclusion`/`passed` 等未登记字段会直接失败。
pub fn from_json(text: &str) -> Result<DrillRecord, String> {
    serde_json::from_str(text)
        .map_err(|err| format!("记录不是合法的 {DRILL_SCHEMA} JSON（未登记字段会被拒）：{err}"))
}

/// 序列化记录（缩进输出，便于人工补填）。
pub fn to_json(record: &DrillRecord) -> Result<String, String> {
    serde_json::to_string_pretty(record).map_err(|err| err.to_string())
}

/// 渲染成 Markdown 片段。结论行固定为"未结论"，由人工评审填判定。
pub fn render_markdown(record: &DrillRecord) -> String {
    let mut out = String::new();
    let case = case_by_id(&record.case);
    out.push_str(&format!("# 演练记录：{}\n\n", record.case));
    if let Some(case) = case {
        out.push_str(&format!("- 要求：{}\n", case.requirement));
        out.push_str(&format!("- 场景：{}\n", case.scenario));
    }
    out.push_str(&format!("- 阶段：{}\n", record.phase.as_str()));
    out.push_str(&format!("- commit：{}\n", record.commit));
    out.push_str(&format!("- 执行者：{}\n", record.operator));
    out.push_str(&format!("- 开始（ms）：{}\n", record.started_at_ms));
    out.push_str(&format!("- 时长（ms）：{}\n", record.duration_ms));
    out.push_str(&format!(
        "- 环境：{} / {} / {} / 网络 {} / 参与端 {}\n",
        record.environment.host,
        record.environment.os,
        record.environment.arch,
        record.environment.network,
        record.environment.peers
    ));
    if !record.environment.notes.is_empty() {
        out.push_str("- 追加事实：");
        let mut first = true;
        for (key, value) in &record.environment.notes {
            if !first {
                out.push('；');
            }
            out.push_str(&format!("{key}={value}"));
            first = false;
        }
        out.push('\n');
    }

    out.push_str("\n## 步骤与观察\n\n");
    out.push_str("| # | 步骤 | 观察 | 证据 |\n| --- | --- | --- | --- |\n");
    for (index, step) in record.steps.iter().enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            index + 1,
            step.step,
            step.observation,
            if step.evidence.is_empty() {
                "—"
            } else {
                &step.evidence
            }
        ));
    }

    out.push_str("\n## 读数\n\n");
    out.push_str("| 项 | 值 |\n| --- | --- |\n");
    for (key, value) in &record.readings {
        out.push_str(&format!("| {key} | {value} |\n"));
    }

    if !record.gaps.is_empty() {
        out.push_str("\n## 已知缺口\n\n");
        for gap in &record.gaps {
            out.push_str(&format!("- {gap}\n"));
        }
    }

    let problems = validate(record);
    out.push_str("\n## 记录完整性\n\n");
    if problems.is_empty() {
        out.push_str("记录完整（所有必填读数已填）。\n");
    } else {
        for problem in &problems {
            out.push_str(&format!("- 未完成：{problem}\n"));
        }
    }
    out.push_str("\n## 结论\n\n未结论：本记录只含读数与观察，判定由出口评审给出。\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(case_id: &str) -> DrillRecord {
        let case = case_by_id(case_id).expect("案件已登记");
        let mut record = template(case, "eb4c459", 1_763_500_000_000);
        record.operator = "jinfeng".to_string();
        record.duration_ms = 12_345;
        record.environment.host = "dev-box-1".to_string();
        record.environment.network = "lan-1g".to_string();
        record.environment.peers = 2;
        for step in &mut record.steps {
            step.observation = "观察到订阅以 snapshot 基准补齐".to_string();
            step.evidence = "tmp/drill.log".to_string();
            step.at_ms = 1_763_500_001_000;
        }
        for value in record.readings.values_mut() {
            *value = "recorded".to_string();
        }
        record
    }

    #[test]
    fn registry_covers_every_phase_from_p2_to_p6() {
        for phase in DrillPhase::all() {
            assert!(
                DRILL_CASES.iter().any(|case| case.phase == phase),
                "{} 没有任何真实演练案件",
                phase.as_str()
            );
        }
        for case in DRILL_CASES {
            assert!(!case.steps.is_empty(), "{} 没有步骤", case.id);
            assert!(!case.readings.is_empty(), "{} 没有必填读数", case.id);
            assert!(
                case.needs_real_env,
                "{} 不支持单进程自证，不应进入真实演练清单",
                case.id
            );
        }
        let mut ids: Vec<&str> = DRILL_CASES.iter().map(|case| case.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), DRILL_CASES.len(), "案件 id 不得重复");
    }

    #[test]
    fn blank_template_never_passes_validation() {
        let case = case_by_id("p6-rm06-network-switch").expect("案件已登记");
        let record = template(case, "eb4c459", 0);
        let problems = validate(&record);
        assert!(
            !problems.is_empty(),
            "空白模板必须校验失败，否则等于把模板当演练结果"
        );
        assert!(problems.iter().any(|p| p.0.contains("operator")));
        assert!(problems.iter().any(|p| p.0.contains("读数")));
        assert!(!is_complete(&record));
    }

    #[test]
    fn filled_record_is_complete_but_still_not_concluded() {
        let record = filled("p6-rm06-network-switch");
        assert!(validate(&record).is_empty(), "{:?}", validate(&record));
        let markdown = render_markdown(&record);
        assert!(markdown.contains("## 读数"));
        assert!(markdown.contains("记录完整"));
        assert!(markdown.contains("未结论"));
        assert!(
            !markdown.contains("通过") && !markdown.contains("失败"),
            "工具不得替评审下结论：{markdown}"
        );
    }

    #[test]
    fn missing_reading_or_step_is_named_explicitly() {
        let mut record = filled("p4-joint-delivery");
        record.readings.remove("tampered_rejected");
        record.steps.pop();
        let problems = validate(&record);
        let text = problems
            .iter()
            .map(|p| p.0.clone())
            .collect::<Vec<_>>()
            .join("；");
        assert!(text.contains("缺少读数 tampered_rejected"), "{text}");
        assert!(text.contains("步骤数不符"), "{text}");
    }

    #[test]
    fn single_peer_cannot_stand_in_for_multi_end_device() {
        let mut record = filled("p6-double-host");
        record.environment.peers = 1;
        let problems = validate(&record);
        assert!(
            problems.iter().any(|p| p.0.contains("peers=1")),
            "双机案件不能用一个参与端冒充：{problems:?}"
        );
    }

    #[test]
    fn record_schema_refuses_smuggled_conclusion_fields() {
        let record = filled("p6-rm09-mailbox");
        let json = serde_json::to_string(&record).expect("序列化");
        let parsed: DrillRecord = serde_json::from_str(&json).expect("回读应成功");
        assert_eq!(parsed, record);

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["conclusion"] = serde_json::json!("passed");
        let rejected = serde_json::from_str::<DrillRecord>(&value.to_string());
        assert!(
            rejected.is_err(),
            "带结论字段的记录必须被拒绝，否则判定会混进读数"
        );
    }

    #[test]
    fn phase_mismatch_and_unknown_case_are_reported() {
        let mut record = filled("p2-stop-foreign-writer");
        record.phase = DrillPhase::P6;
        record.case = "p6-not-registered".to_string();
        let problems = validate(&record);
        assert!(problems.iter().any(|p| p.0.contains("未知案件")));
    }
}
