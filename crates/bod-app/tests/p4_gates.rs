//! P4 5.1 的门禁核对：G-9、G-24、G-32、G-33 都要有命名用例，且用例里真有断言。
//!
//! 这里刻意读**源码文本**而不是只跑 `cargo test`：跑通只能说明"有测试函数"，
//! 不能说明它断言了什么。一个只有 `assert!(true)` 的用例同样会绿。
//! 判据是"函数体内至少两条 assert"。

use std::path::Path;

const GATES: &str = include_str!("../../../verification/gates.yaml");

struct Case {
    gate: &'static str,
    module: &'static str,
    case: &'static str,
}

const CASES: &[Case] = &[
    Case {
        gate: "G-9",
        module: "artifact_acceptance",
        case: "evidence_from_previous_revision_is_rejected",
    },
    Case {
        gate: "G-9",
        module: "runtime_verification",
        case: "report_from_an_older_commit_cannot_accept_the_new_artifact",
    },
    Case {
        gate: "G-24",
        module: "runtime_verification",
        case: "promotion_reuses_the_same_digest_across_environments",
    },
    Case {
        gate: "G-24",
        module: "coding_integration",
        case: "stale_report_is_rejected_even_when_checks_passed",
    },
    Case {
        gate: "G-32",
        module: "runtime_verification",
        case: "unrun_required_check_stays_unknown_and_blocks_acceptance",
    },
    Case {
        gate: "G-32",
        module: "runtime_verification",
        case: "environment_failure_is_not_attributed_to_the_code",
    },
    Case {
        gate: "G-33",
        module: "stage_manifest",
        case: "self_signed_package_passes_integrity_but_not_identity",
    },
    Case {
        gate: "G-33",
        module: "stage_manifest",
        case: "signature_must_name_the_payload_it_covers",
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
        let gates: Vec<&str> = CASES
            .iter()
            .filter(|item| item.gate == case.gate)
            .map(|item| item.case)
            .collect();
        assert!(gates.len() >= 2, "{} 至少两条用例交叉覆盖", case.gate);
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
fn p4_gates_are_not_bound_as_implemented_yet() {
    // A1 起 P2—P7 的 I/G 引用全部登记到 gates.yaml，未达出口的门禁以 planned
    // 显式占位，但绝不能提前标成 implemented。G-9 是 P0 的旧绑定，只允许它
    // 继续指向 artifact_acceptance 的既有用例。
    let entry = |gate: &str| {
        let marker = format!("- id: {gate}");
        let start = GATES.find(&marker)?;
        let rest = &GATES[start + marker.len()..];
        Some(&rest[..rest.find("\n- id: ").unwrap_or(rest.len())])
    };
    for gate in ["G-24", "G-32", "G-33", "G-35"] {
        if let Some(block) = entry(gate) {
            assert!(
                block.contains("status: planned"),
                "{gate} 在出口前应保持 planned：{block}"
            );
        }
    }
    let g9 = entry("G-9").expect("G-9 是 P0 既有绑定");
    assert!(
        g9.contains("bod-app::artifact_acceptance"),
        "G-9 仍应指向 P0 的 artifact_acceptance"
    );
    assert!(
        !g9.contains("runtime_verification"),
        "P4 的新用例不得改写 G-9 的既有绑定"
    );
}

#[test]
fn gate_cases_are_distinct() {
    let mut seen = std::collections::BTreeSet::new();
    for case in CASES {
        assert!(
            seen.insert((case.module, case.case)),
            "用例 `{}` 被重复登记",
            case.case
        );
    }
}
