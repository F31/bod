//! P6 5.1 的门禁核对：G-19、G-23、G-34、G-46、G-47 都要有命名用例，且用例里真有断言。
//!
//! 判据与 P4/P5 一致：读**源码文本**而不是只看 `cargo test` 绿不绿——一个 `assert!(true)` 也会绿。

use std::collections::BTreeSet;
use std::path::Path;

const GATES: &str = include_str!("../../../verification/gates.yaml");

struct Case {
    gate: &'static str,
    module: &'static str,
    case: &'static str,
}

const CASES: &[Case] = &[
    Case {
        gate: "G-19",
        module: "remote_partition",
        case: "rm03_lease_expired_new_attempt_keeps_old_report_as_history",
    },
    Case {
        gate: "G-19",
        module: "remote_partition",
        case: "task_2_3_resume_checks_epoch_lease_attempt_cursor_and_effects",
    },
    Case {
        gate: "G-23",
        module: "extension_sdk",
        case: "task_1_1_missing_required_capability_keeps_local_path",
    },
    Case {
        gate: "G-23",
        module: "extension_sdk",
        case: "task_1_1_protocol_and_evidence_tier_are_checked_separately",
    },
    Case {
        gate: "G-34",
        module: "readonly_api",
        case: "invariant_i_55_readonly_request_creates_no_run_or_dispatch",
    },
    Case {
        gate: "G-34",
        module: "readonly_api",
        case: "g_34_button_hint_never_authorizes",
    },
    Case {
        gate: "G-46",
        module: "extension_sdk",
        case: "invariant_i_57_identity_failure_disables_dispatch_without_badge_exemption",
    },
    Case {
        gate: "G-46",
        module: "extension_sdk",
        case: "task_1_3_probe_and_view_agree",
    },
    Case {
        gate: "G-47",
        module: "remote_delegation",
        case: "rm07_tampered_batch_rejected_and_legal_batch_applied_once",
    },
    Case {
        gate: "G-47",
        module: "remote_delegation",
        case: "task_2_2_gap_requests_backfill_and_signature_never_accepts",
    },
];

fn source(module: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("src").join(format!("{module}.rs"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("读不到模块 {}: {err}", path.display()))
}

/// 取函数体：从 `fn name(` 起到下一个 `#[test]` 或文件尾。
fn body<'a>(text: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}(");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("模块里没有用例 `{name}`"));
    let rest = &text[start..];
    let end = rest.find("\n    #[test]").unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn every_gate_has_named_cases_in_the_expected_module() {
    for case in CASES {
        let text = source(case.module);
        let source_body = body(&text, case.case);
        assert!(
            !source_body.trim().is_empty(),
            "{} 的用例 `{}` 是空的",
            case.gate,
            case.case
        );
        let per_gate = CASES.iter().filter(|item| item.gate == case.gate).count();
        assert!(
            per_gate >= 2,
            "{} 至少两条用例交叉覆盖，实际 {per_gate}",
            case.gate
        );
    }
}

#[test]
fn each_case_carries_real_assertions() {
    for case in CASES {
        let text = source(case.module);
        let source_body = body(&text, case.case);
        let asserts = source_body.matches("assert").count();
        assert!(
            asserts >= 2,
            "{} 的用例 `{}` 只有 {asserts} 处断言，不算真实断言",
            case.gate,
            case.case
        );
        assert!(
            !source_body.contains("assert!(true)"),
            "{} 的用例 `{}` 用了恒真断言",
            case.gate,
            case.case
        );
    }
}

#[test]
fn p6_gates_are_not_bound_before_the_exit_is_met() {
    // P6 出口未达（2.8 与 4.6 缺真实环境）：A1 起五个门禁以 planned 登记，
    // 只恢复规格覆盖，不提前宣称 implemented。
    let entry = |gate: &str| {
        let marker = format!("- id: {gate}");
        let Some(start) = GATES.find(&marker) else {
            return None;
        };
        let rest = &GATES[start + marker.len()..];
        Some(&rest[..rest.find("\n- id: ").unwrap_or(rest.len())])
    };
    for gate in ["G-19", "G-23", "G-34", "G-46", "G-47"] {
        if let Some(block) = entry(gate) {
            assert!(
                block.contains("status: planned"),
                "{gate} 在出口前应保持 planned：{block}"
            );
        }
    }
}

#[test]
fn gate_cases_are_distinct() {
    let mut seen = BTreeSet::new();
    for case in CASES {
        assert!(
            seen.insert((case.module, case.case)),
            "用例 `{}` 被重复登记",
            case.case
        );
    }
}
