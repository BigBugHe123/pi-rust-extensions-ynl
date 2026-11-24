mod common;

use common::{Host, TestDir};
use serde_json::{Value, json};

const PLAN: &str = "# Plan\n\n- Do the thing.";

async fn started(label: &str) -> (Host, TestDir, TestDir) {
    let work = TestDir::new(&format!("{label}-work"));
    let agent = TestDir::new(&format!("{label}-agent"));
    let host = Host::start(work.path(), agent.path(), Vec::new()).await;
    (host, work, agent)
}

/// A read-only workflow: `defaultPlanTools` makes the policy explicit, so there is no draft.
async fn read_only(label: &str) -> (Host, TestDir, TestDir) {
    let work = TestDir::new(&format!("{label}-work"));
    let agent = TestDir::new(&format!("{label}-agent"));
    std::fs::create_dir_all(agent.path().join("ext")).unwrap();
    std::fs::write(
        agent.path().join("ext/plan-mode.json"),
        r#"{ "defaultPlanTools": ["read", "bash"] }"#,
    )
    .unwrap();
    let host = Host::start(work.path(), agent.path(), Vec::new()).await;
    (host, work, agent)
}

fn blocked(reply: &Value) -> Option<String> {
    (reply["result"]["block"] == true)
        .then(|| reply["result"]["reason"].as_str().unwrap().to_owned())
}

fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn registers_tools_command_and_f2() {
    let (host, _work, _agent) = started("reg").await;
    let init = host.init_result();
    let tools: Vec<&str> = init["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["plan_mode_question", "plan_mode_complete"]);
    assert_eq!(init["commands"][0]["name"], "plan");
    assert_eq!(init["commands"][0]["completions"], true);
    assert_eq!(init["shortcuts"][0]["shortcut"], "f2");
    let events = init["events"].as_array().unwrap();
    for name in [
        "tool_call",
        "context",
        "agent_settled",
        "session_before_tree",
    ] {
        assert!(events.iter().any(|e| e == name), "{name}");
    }
}

#[tokio::test]
async fn start_publishes_contracts_and_blocks_mutation() {
    let (mut host, _work, _agent) = read_only("start").await;
    host.command("start").await;
    assert_eq!(host.sent_custom("plan-mode-transition").len(), 1);
    let interaction = host.sent_custom("plan-mode-interaction");
    assert_eq!(interaction[0]["details"]["mode"], "questions");
    assert_eq!(interaction[0]["display"], false);
    assert_eq!(host.state()["enabled"], true);
    assert_eq!(host.last_status(), "plan active");
    assert!(
        host.notices()
            .iter()
            .any(|n| n.starts_with("Plan mode enabled."))
    );

    let reply = host
        .tool_call("bash", json!({ "command": "rm -rf build" }))
        .await;
    assert!(
        blocked(&reply)
            .unwrap()
            .contains("Blocked command: rm -rf build")
    );
    let reply = host
        .tool_call("bash", json!({ "command": "git status" }))
        .await;
    assert_eq!(blocked(&reply), None);
    let reply = host.tool_call("edit", json!({ "path": "a" })).await;
    assert_eq!(
        blocked(&reply).unwrap(),
        "Plan mode blocks mutating tool 'edit'."
    );
    let reply = host.tool_call("update_plan", json!({})).await;
    assert!(blocked(&reply).unwrap().contains("update_plan"));
    let reply = host.tool_call("mystery", json!({})).await;
    assert!(
        blocked(&reply)
            .unwrap()
            .contains("not registered or active")
    );
    let reply = host.tool_call("plan_mode_question", json!({})).await;
    assert_eq!(blocked(&reply), None);

    host.command("start").await;
    assert!(
        host.notices()
            .iter()
            .any(|n| n == "Plan mode is already active.")
    );
}

#[tokio::test]
async fn completion_opens_the_ready_menu_and_implements_here() {
    let (mut host, _work, _agent) = read_only("ready").await;
    host.command("start").await;
    let result = host
        .tool("plan_mode_complete", "p1", json!({ "plan": PLAN }))
        .await;
    assert_eq!(result["terminate"], true);
    assert_eq!(text(&result), format!("**Proposed Plan**\n\n{PLAN}"));
    assert_eq!(host.state()["latestPlan"], PLAN);
    assert_eq!(host.last_status(), "plan ready");

    host.selects.push_back(Some("Implement here".into()));
    host.event("agent_settled", json!({ "type": "agent_settled" }))
        .await;
    let select = host.nth_call("ui.select", 0).await;
    assert!(
        select["title"]
            .as_str()
            .unwrap()
            .starts_with("Proposed plan ready.")
    );
    let message = host.nth_call("session.sendUserMessage", 0).await;
    assert_eq!(message["content"], "Implement the plan.");
    let contracts = host.sent_custom("plan-mode-transition");
    assert!(
        contracts.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("NORMAL")
    );
    assert_eq!(host.state()["enabled"], false);
    let reply = host.tool_call("edit", json!({ "path": "a" })).await;
    assert_eq!(blocked(&reply), None, "Normal mode allows edits again");
}

#[tokio::test]
async fn save_blocks_a_new_workflow_until_cleared() {
    let (mut host, work, _agent) = read_only("save").await;
    host.command("start").await;
    host.tool("plan_mode_complete", "p1", json!({ "plan": PLAN }))
        .await;
    host.command("save").await;
    assert_eq!(host.state()["savedPlan"]["plan"], PLAN);
    assert_eq!(host.last_status(), "plan saved");
    host.command("start").await;
    let warnings: Vec<&Value> = host.called("ui.notify");
    assert!(
        warnings.last().unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("A plan is saved for later.")
    );

    host.command("export notes/PLAN.md").await;
    let exported = std::fs::read_to_string(work.path().join("notes/PLAN.md")).unwrap();
    assert_eq!(exported, format!("{PLAN}\n"));
    assert_eq!(
        host.state()["savedPlan"]["plan"],
        PLAN,
        "a saved plan stays after export"
    );

    host.command("exit").await;
    assert!(host.state()["savedPlan"].is_null());
    assert!(host.notices().iter().any(|n| n == "Saved plan cleared."));
}

#[tokio::test]
async fn exporting_a_ready_plan_ends_plan_mode_and_never_overwrites() {
    let (mut host, work, _agent) = read_only("export").await;
    std::fs::write(work.path().join("PLAN.md"), "old").unwrap();
    host.command("start").await;
    host.tool("plan_mode_complete", "p1", json!({ "plan": PLAN }))
        .await;
    host.command("export").await;
    assert_eq!(
        std::fs::read_to_string(work.path().join("PLAN.md")).unwrap(),
        "old"
    );
    assert_eq!(host.state()["enabled"], true);
    assert!(host.notices().iter().any(|n| n.contains("already exists")));
    host.command("export out.md").await;
    assert!(work.path().join("out.md").exists());
    assert_eq!(host.state()["enabled"], false);
}

#[tokio::test]
async fn discussion_mode_pauses_questionnaires() {
    let (mut host, _work, _agent) = read_only("discuss").await;
    host.command("start").await;
    host.selects
        .push_back(Some("Switch to free discussion".into()));
    host.command("").await;
    let interaction = host.sent_custom("plan-mode-interaction");
    assert_eq!(interaction.last().unwrap()["details"]["mode"], "discussion");
    assert_eq!(host.last_status(), "plan discussion");
    let question = json!({ "questions": [{ "id": "db", "header": "DB", "question": "Which?",
        "options": [{ "label": "A", "description": "a" }, { "label": "B", "description": "b" }] }] });
    let result = host.tool("plan_mode_question", "q1", question).await;
    assert_eq!(result["details"]["reason"], "questions_paused");

    // A completion in discussion stores the plan without opening a menu.
    host.tool("plan_mode_complete", "p1", json!({ "plan": PLAN }))
        .await;
    host.event("agent_settled", json!({ "type": "agent_settled" }))
        .await;
    assert!(
        host.called("ui.select").len() == 1,
        "only the /plan menu opened"
    );
}

#[tokio::test]
async fn rpc_questions_use_dialogs() {
    let work = TestDir::new("rpc-work");
    let agent = TestDir::new("rpc-agent");
    let mut host = Host::start_mode(work.path(), agent.path(), Vec::new(), "rpc").await;
    host.command("start").await;
    host.selects.push_back(Some("2. SQLite".into()));
    let question = json!({ "questions": [{ "id": "db", "header": "DB", "question": "Which database?",
        "options": [
            { "label": "Postgres", "description": "Server" },
            { "label": "SQLite", "description": "Embedded" }
        ] }] });
    let result = host.tool("plan_mode_question", "q1", question).await;
    let answers = &result["details"]["answers"];
    assert_eq!(answers[0]["answer"], "SQLite", "{result}");
    assert_eq!(answers[0]["optionIndex"], 2);
    assert_eq!(answers[0]["wasCustom"], false);
    let payload: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(payload["cancelled"], false);
}

#[tokio::test]
async fn finalize_retries_once_then_warns() {
    let (mut host, _work, _agent) = read_only("finalize").await;
    host.command("finalize").await;
    assert!(
        host.notices()
            .iter()
            .any(|n| n == "Plan mode is not active. Use /plan first.")
    );
    host.command("start").await;
    host.command("finalize").await;
    assert!(host.user_messages()[0].starts_with("Finalize the current implementation plan now."));
    let end = json!({ "type": "agent_end", "messages": [
        { "role": "assistant", "content": [{ "type": "text", "text": "I will present the plan." }], "stopReason": "stop" }
    ]});
    host.event("agent_end", end.clone()).await;
    // pi-rust reports busy while agent_settled handlers run.
    host.idle = false;
    host.event("agent_settled", json!({ "type": "agent_settled" }))
        .await;
    host.idle = true;
    assert!(host.user_messages()[1].starts_with("Your previous finalization response"));
    host.event("agent_end", end).await;
    host.event("agent_settled", json!({ "type": "agent_settled" }))
        .await;
    assert_eq!(host.user_messages().len(), 2);
    assert!(
        host.notices()
            .iter()
            .any(|n| n.starts_with("Plan finalization ended twice"))
    );
}

#[tokio::test]
async fn legacy_proposed_plan_block_completes() {
    let (mut host, _work, _agent) = read_only("legacy").await;
    host.command("start").await;
    let end = json!({ "type": "agent_end", "messages": [{ "role": "assistant", "content": [
        { "type": "text", "text": format!("Here:\n<proposed_plan>\n{PLAN}\n</proposed_plan>") }
    ]}]});
    host.event("agent_end", end).await;
    assert_eq!(host.state()["latestPlanSource"], "legacy_proposed_plan");
    host.selects.push_back(None);
    host.event("agent_settled", json!({ "type": "agent_settled" }))
        .await;
    let shown = host.sent_custom("proposed-plan");
    assert_eq!(shown[0]["display"], true);
    assert!(
        shown[0]["content"]
            .as_str()
            .unwrap()
            .contains("Do the thing.")
    );
}

#[tokio::test]
async fn context_reconciles_contracts_and_strips_exited_plans() {
    let (mut host, _work, _agent) = read_only("context").await;
    host.command("start").await;
    let messages = json!([{ "role": "compactionSummary", "summary": "s" }, { "role": "user", "content": "go" }]);
    let reply = host
        .event(
            "context",
            json!({ "type": "context", "messages": messages }),
        )
        .await;
    let out = reply["result"]["messages"].as_array().unwrap();
    assert_eq!(out[1]["customType"], "plan-mode-transition", "{out:?}");
    assert_eq!(out.last().unwrap()["customType"], "plan-mode-interaction");

    host.tool("plan_mode_complete", "p1", json!({ "plan": PLAN }))
        .await;
    host.command("exit").await;
    let messages = json!([
        { "role": "user", "content": "go" },
        { "role": "assistant", "content": [{ "type": "toolCall", "id": "p1", "name": "plan_mode_complete", "arguments": {} }] },
        { "role": "toolResult", "toolName": "plan_mode_complete", "toolCallId": "p1", "content": [] }
    ]);
    let reply = host
        .event(
            "context",
            json!({ "type": "context", "messages": messages }),
        )
        .await;
    let out = reply["result"]["messages"].as_array().unwrap();
    assert!(out.iter().all(|m| m["role"] != "toolResult"), "{out:?}");
    assert!(out.iter().all(|m| m["role"] != "assistant"), "{out:?}");
    assert!(
        out.iter()
            .any(|m| m["content"].as_str().is_some_and(|c| c.contains("NORMAL")))
    );
}

#[tokio::test]
async fn restores_an_active_workflow_and_blocks_internal_tree_targets() {
    let work = TestDir::new("restore-work");
    let agent = TestDir::new("restore-agent");
    let branch = vec![
        json!({ "type": "custom_message", "id": "m1", "customType": "plan-mode-transition", "content": "x" }),
        json!({ "type": "custom", "customType": "plan-mode-state", "data": {
            "enabled": true, "awaitingAction": false, "interactionMode": "discussion",
            "workflowToolPolicy": { "kind": "explicit", "desiredNames": ["read"], "allowedNames": ["read"], "resolved": true }
        }}),
    ];
    let mut host = Host::start(work.path(), agent.path(), branch).await;
    assert_eq!(host.last_status(), "plan discussion");
    let reply = host.tool_call("bash", json!({ "command": "ls" })).await;
    assert!(
        blocked(&reply)
            .unwrap()
            .contains("not selected by the Plan policy")
    );
    let reply = host
        .event(
            "session_before_tree",
            json!({ "type": "session_before_tree", "preparation": { "targetId": "m1" } }),
        )
        .await;
    assert_eq!(reply["result"]["cancel"], true);
}

#[tokio::test]
async fn print_mode_rejects_the_menu() {
    let work = TestDir::new("print-work");
    let agent = TestDir::new("print-agent");
    let mut host = Host::start_mode(work.path(), agent.path(), Vec::new(), "json").await;
    let reply = host.command("").await;
    let message = reply["$error"]["message"].as_str().unwrap();
    assert!(
        message.contains("unavailable in print and JSON modes"),
        "{message}"
    );
    let reply = host.command("tools").await;
    assert!(
        reply["$error"]["message"]
            .as_str()
            .unwrap()
            .contains("/plan tools requires TUI or RPC")
    );
}

#[tokio::test]
async fn thinking_level_applies_and_restores() {
    let work = TestDir::new("think-work");
    let agent = TestDir::new("think-agent");
    std::fs::create_dir_all(agent.path().join("ext")).unwrap();
    std::fs::write(
        agent.path().join("ext/plan-mode.json"),
        r#"{ "thinkingLevel": "high" }"#,
    )
    .unwrap();
    let mut host = Host::start(work.path(), agent.path(), Vec::new()).await;
    host.command("start").await;
    assert_eq!(host.thinking, "high");
    assert_eq!(host.state()["previousThinkingLevel"], "medium");
    host.command("exit").await;
    assert_eq!(host.thinking, "medium");
}

#[tokio::test]
async fn busy_transitions_are_refused() {
    let (mut host, _work, _agent) = read_only("busy").await;
    host.idle = false;
    host.command("start").await;
    assert_eq!(host.state(), Value::Null);
    assert!(
        host.notices()
            .iter()
            .any(|n| n.starts_with("Cannot start Plan mode while an agent run is active."))
    );
}

#[tokio::test]
async fn managed_draft_admits_only_its_file_and_completes_from_it() {
    let (mut host, _work, agent) = started("draft").await;
    host.command("start").await;
    let reply = host
        .event(
            "context",
            json!({ "type": "context", "messages": [{ "role": "user", "content": "go" }] }),
        )
        .await;
    let out = reply["result"]["messages"].as_array().unwrap();
    let contract = out.last().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(
        contract.starts_with("[PI PLAN MODE DRAFT CONTRACT v1]"),
        "{contract}"
    );
    let draft = host.state()["draft"].clone();
    assert_eq!(draft["status"], "active");
    let path = agent
        .path()
        .join("extensions-data/plan-mode/drafts")
        .join(draft["workflowId"].as_str().unwrap())
        .join("plan.md");
    assert!(contract.contains(&path.to_string_lossy().to_string()));

    let reply = host
        .tool_call("write", json!({ "path": "src/main.rs", "content": "x" }))
        .await;
    assert!(blocked(&reply).unwrap().contains("exact absolute path"));
    let reply = host
        .tool_call("write", json!({ "path": path, "content": PLAN }))
        .await;
    assert_eq!(blocked(&reply), None);
    std::fs::write(&path, PLAN).unwrap();
    host.event(
        "tool_execution_end",
        json!({ "type": "tool_execution_end", "toolCallId": "c1", "toolName": "write" }),
    )
    .await;

    let call = json!({ "type": "message", "message": { "role": "assistant", "content": [
        { "type": "toolCall", "id": "p1", "name": "plan_mode_complete", "arguments": {} }
    ]}});
    host.branch.push(call);
    let event = json!({ "type": "tool_call", "toolName": "plan_mode_complete", "toolCallId": "p1", "input": {} });
    let reply = host.event("tool_call", event).await;
    assert_eq!(blocked(&reply), None);
    let result = host.tool("plan_mode_complete", "p1", json!({})).await;
    assert_eq!(text(&result), format!("**Proposed Plan**\n\n{PLAN}"));
}

#[tokio::test]
async fn fresh_implementation_hands_the_plan_to_the_new_session() {
    let (mut host, work, agent) = read_only("fresh").await;
    host.command("start").await;
    host.tool("plan_mode_complete", "p1", json!({ "plan": PLAN }))
        .await;
    host.selects
        .push_back(Some("Start fresh and implement".into()));
    host.selects
        .push_back(Some("Start fresh implementation".into()));
    host.command("").await;
    let calls: Vec<String> = host
        .called("pi.call")
        .iter()
        .map(|p| {
            format!(
                "{}.{}",
                p["target"].as_str().unwrap(),
                p["method"].as_str().unwrap()
            )
        })
        .collect();
    assert!(calls.contains(&"ctx.newSession".to_owned()), "{calls:?}");
    let options = host
        .called("pi.call")
        .into_iter()
        .find(|p| p["method"] == "newSession")
        .unwrap()["args"][0]
        .clone();
    assert_eq!(options["parentSession"], "/sessions/s1.jsonl");

    let mut fresh =
        common::Host::start_full(work.path(), agent.path(), Vec::new(), "tui", "new").await;
    let kickoff = fresh.nth_call("session.sendUserMessage", 0).await;
    let text = kickoff["content"].as_str().unwrap();
    assert!(
        text.contains("Implement the plan in a fresh context."),
        "{text}"
    );
    assert!(text.ends_with(PLAN));
    let contract = fresh.sent_custom("plan-mode-transition");
    assert!(contract[0]["content"].as_str().unwrap().contains("NORMAL"));
    let handoffs = agent.path().join("extensions-data/plan-mode/handoff");
    assert_eq!(
        std::fs::read_dir(handoffs).unwrap().count(),
        0,
        "the handoff is consumed"
    );
}

#[tokio::test]
async fn an_unrestored_workflow_survives_shutdown() {
    let work = TestDir::new("unrestored-work");
    let agent = TestDir::new("unrestored-agent");
    let state = json!({ "type": "custom", "customType": "plan-mode-state", "data": {
        "enabled": true, "awaitingAction": true, "latestPlan": PLAN, "latestPlanSource": "plan_mode_complete"
    }});
    let mut host = Host::start(work.path(), agent.path(), vec![state]).await;
    host.active.retain(|name| !name.starts_with("plan_mode_"));
    host.event("session_tree", json!({ "type": "session_tree" }))
        .await;
    assert!(
        host.notices()
            .iter()
            .any(|n| n.contains("helper tools are unavailable"))
    );
    host.event("session_shutdown", json!({ "type": "session_shutdown" }))
        .await;
    assert_eq!(
        host.state()["latestPlan"],
        PLAN,
        "the stored workflow is kept"
    );
}
