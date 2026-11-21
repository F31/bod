//! P5 5.1 的门禁核对：G-13、G-21、G-22、G-25 都要有命名用例，且用例里真有断言。
//!
//! 与 P4 同一判据：读**源码文本**而不是只看 `cargo test` 绿不绿——一个 `assert!(true)` 也会绿。
//! 判据是"函数体内至少两条 assert 且不含恒真断言"。

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
        gate: "G-25",
        module: "catalog_upgrade",
        case: "invariant_i_44_upgrade_preserves_local_edit_and_proposes",
    },
    Case {
        gate: "G-25",
        module: "catalog_upgrade",
        case: "task_1_2_same_name_never_overwrites_existing_object",
    },
    Case {
        gate: "G-13",
        module: "feedback_knowledge",
        case: "invariant_i_14_concurrent_proposal_conflicts_and_keeps_candidate",
    },
    Case {
        gate: "G-13",
        module: "feedback_knowledge",
        case: "invariant_i_23_two_different_task_sets_cannot_be_compared",
    },
    Case {
        gate: "G-21",
        module: "authorization",
        case: "task_2_2_revoked_member_cannot_resume_subscription",
    },
    Case {
        gate: "G-21",
        module: "authorization",
        case: "task_2_6_leave_keeps_assets_and_history_but_blocks_new_actions",
    },
    Case {
        gate: "G-22",
        module: "feedback_knowledge",
        case: "task_3_1_external_web_stays_untrusted",
    },
    Case {
        gate: "G-22",
        module: "feedback_knowledge",
        case: "task_3_3_auto_accept_only_for_allowed_fields_and_scope",
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
fn p5_gates_are_not_bound_before_the_exit_is_met() {
    // A1 起 P5 门禁以 planned 登记，覆盖规格引用但绝不提前标 implemented。
    // G-21 是 P0 的旧绑定，只允许它继续指向 control_api 的既有用例。
    let entry = |gate: &str| {
        let marker = format!("- id: {gate}");
        let start = GATES.find(&marker)?;
        let rest = &GATES[start + marker.len()..];
        Some(&rest[..rest.find("\n- id: ").unwrap_or(rest.len())])
    };
    for gate in ["G-13", "G-22", "G-25"] {
        if let Some(block) = entry(gate) {
            assert!(
                block.contains("status: planned"),
                "{gate} 在出口前应保持 planned：{block}"
            );
        }
    }
    let g21 = GATES
        .split("- id: G-21")
        .nth(1)
        .expect("G-21 是 P0 既有绑定");
    assert!(
        g21.contains("bod-app::control_api"),
        "G-21 仍应指向 P0 的 control_api"
    );
    assert!(
        !g21.contains("stage_permission_tests"),
        "P5 的新用例不得改写 G-21 的既有绑定"
    );
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
