//! `/plan` under the real pi binary in JSON mode.

use pi_ext_e2e::{Run, Turn, cargo_bin, json, pi_bin_or_skip};

const PLAN: &str = "# Add a flag\n\n- Parse `--flag` in main.rs.\n- Test it.";

#[test]
fn read_only_workflow_blocks_edits_and_completes_a_plan() {
    if pi_bin_or_skip().is_none() {
        return;
    }
    let output = Run::new(cargo_bin("pi-rust-ext", "pi-rust-ext"))
        .args(["plan-mode"])
        .file("work/main.rs", "fn main() {}\n")
        .file(
            "agent/ext/plan-mode.json",
            r#"{ "defaultPlanTools": ["read", "bash"] }"#,
        )
        .turns([
            Turn::tool_calls([
                ("bash", json!({ "command": "ls && rm -rf build" })),
                ("bash", json!({ "command": "cat main.rs" })),
                ("write", json!({ "path": "main.rs", "content": "x" })),
            ]),
            Turn::tool_call("plan_mode_complete", json!({ "plan": PLAN })),
            Turn::text("unused"),
        ])
        .prompt("/plan Add a --flag option")
        .run()
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);

    let bash = output.tool_results("bash");
    assert_eq!(bash.len(), 2, "{bash:?}");
    let blocked = bash[0].to_string();
    assert!(
        blocked.contains("Blocked command: rm -rf build"),
        "{blocked}"
    );
    assert!(bash[1].to_string().contains("fn main"), "{}", bash[1]);
    let write = output.tool_result_text("write").unwrap_or_default();
    assert!(
        write.contains("Plan mode blocks mutating tool 'write'"),
        "{write}"
    );
    let completed = output
        .tool_result_text("plan_mode_complete")
        .unwrap_or_default();
    assert_eq!(completed, format!("**Proposed Plan**\n\n{PLAN}"));

    let first = output.requests[0].to_string();
    assert!(
        first.contains("[PI PLAN MODE CONTRACT v1: PLAN]"),
        "{first}"
    );
    assert!(first.contains("[PI PLAN MODE INTERACTION CONTRACT v1: QUESTIONS]"));
    assert!(
        !first.contains("DRAFT CONTRACT"),
        "an explicit policy has no draft"
    );
    assert_eq!(output.requests.len(), 2, "plan_mode_complete ends the run");
}

#[test]
fn automatic_workflow_gets_a_managed_draft() {
    if pi_bin_or_skip().is_none() {
        return;
    }
    let output = Run::new(cargo_bin("pi-rust-ext", "pi-rust-ext"))
        .args(["plan-mode"])
        .turns([
            Turn::tool_call("write", json!({ "path": "main.rs", "content": "x" })),
            Turn::tool_call("plan_mode_complete", json!({})),
            Turn::text("unused"),
        ])
        .prompt("/plan Add a --flag option")
        .run()
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    let write = output.tool_result_text("write").unwrap_or_default();
    assert!(
        write.contains("exact absolute path to this workflow's Markdown draft"),
        "{write}"
    );
    let completed = output
        .tool_result_text("plan_mode_complete")
        .unwrap_or_default();
    assert!(
        completed.contains("Write the decision-ready Markdown draft"),
        "{completed}"
    );
    let first = output.requests[0].to_string();
    assert!(
        first.contains("[PI PLAN MODE DRAFT CONTRACT v1]"),
        "{first}"
    );
    assert!(
        first.contains("extensions-data/plan-mode/drafts/"),
        "{first}"
    );
}

#[test]
fn helpers_are_rejected_outside_plan_mode() {
    if pi_bin_or_skip().is_none() {
        return;
    }
    let output = Run::new(cargo_bin("pi-rust-ext", "pi-rust-ext"))
        .args(["plan-mode"])
        .turns([
            Turn::tool_call("plan_mode_complete", json!({ "plan": "# P" })),
            Turn::text("done"),
        ])
        .prompt("Write a roadmap")
        .run()
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    let text = output
        .tool_result_text("plan_mode_complete")
        .unwrap_or_default();
    assert!(
        text.contains("only available while Plan mode is active"),
        "{text}"
    );
    let first = output.requests[0].to_string();
    assert!(!first.contains("PI PLAN MODE CONTRACT"), "{first}");
}
