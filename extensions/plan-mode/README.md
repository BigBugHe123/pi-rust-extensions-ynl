# pi-ext-plan-mode

Created 2026-11-24. Last updated 2026-11-24.

A Codex-like `/plan` mode for pi. The agent explores the project read-only, asks the questions that change the plan, and submits a complete plan. Nothing edits project files until you review the plan and choose how to implement it. This is a Rust port of `@narumitw/pi-plan-mode` 0.62.0. The command, the tool names and parameters, the result `details`, the session entry types, and the model-visible contract texts are the same as upstream, so prompts and sessions written for it keep working. Run it as `pi-rust-ext plan-mode`.

## Commands

| Command | Action |
| --- | --- |
| `/plan` | Opens the menu for the current state: start, the active workflow, a saved plan, or an active implementation plan. TUI and RPC only. |
| `/plan start` | Starts Plan mode without a model message. |
| `/plan <prompt>` | Starts Plan mode and sends the prompt, or sends a follow-up while Plan mode is active. Only the exact words below are subcommands, so `/plan start a migration` is a prompt. |
| `/plan tools` | Chooses the tool allowlist of this session, then starts. TUI and RPC only. |
| `/plan show` | Shows the ready, saved, or active plan in the conversation without a model turn. |
| `/plan finalize` | Asks the agent to finish the plan or ask the last material question. |
| `/plan implement` | Implements the ready or saved plan in this session. |
| `/plan save` | Saves the ready plan in this session and leaves Plan mode. |
| `/plan settings` | Opens Settings. TUI and RPC only. |
| `/plan export [path]` | Writes the ready, saved, or active plan to a new Markdown file. |
| `/plan exit`, `/plan off` | Leaves Plan mode and discards the ready plan, or clears a saved or active plan. |

Starting, leaving, saving, implementing, and exporting a ready plan need an idle agent. A busy transition changes nothing and warns, or fails the command in print and JSON mode. A saved plan blocks a new workflow until you implement or clear it.

The status key `plan-mode` shows `plan active`, `plan discussion`, `plan ready`, `plan saved`, or `plan implementing`. The widget `plan-mode-plan` above the editor shows the state, the allowed tools, and how to switch the interaction mode.

## Tools

| Tool | Parameters | Behavior |
| --- | --- | --- |
| `plan_mode_question` | `questions` (1 to 3), each `{ id, header, question, options }` with 2 to 4 options `{ label, description }` | Shows the questions and returns `{ cancelled, answers }`. Each answer is `{ id, header, question, answer, wasCustom, optionIndex?, note? }`. A cancelled call has `reason`: `cancelled`, `ui_unavailable`, `plan_mode_inactive`, `invalid_input`, `discussion_requested`, or `questions_paused`. |
| `plan_mode_complete` | `plan` (read-only workflows), or `{}` with a managed draft | Stores the plan (at most 50,000 characters), shows `**Proposed Plan**`, and ends the run (`terminate: true`). |

Both tools stay registered in Normal mode, but they and the `tool_call` policy reject calls unless Plan mode is active. Plan mode needs both tools in the active tool list: it does not start, and a restored workflow does not resume, when another tool policy hides them. With the [deferred](../deferred/README.md) extension, its `auto` mode can turn them into names; keep them declared with the rule `"tools": { "plan_mode_*": "on" }`.

## Tool policy

While Plan mode is active, the `tool_call` handler blocks:

- `edit`, `write`, and `update_plan`. With a managed draft, direct native `write` and `edit` calls on the draft path are allowed.
- Tools outside the workflow allowlist. By default the allowlist is the active safe built-ins: `read`, `bash`, `powershell`, `grep`, `find`, and `ls`. MCP tools, `codemode`, and extension tools need an explicit opt-in with `/plan tools` or `defaultPlanTools`. Allowing an orchestrator does not allow the tools it calls; every nested call is checked.
- Tools that are inactive, hidden, or model-only on a nested call. Plan mode never activates a tool.
- `bash` and `powershell` commands outside the reviewed inspection policy.

The allowlist freezes at the first model request of the workflow and restores with it after reload, resume, and `/tree`. A tool that registers later waits for the next workflow.

The Bash policy splits a command on `&&`, `||`, `;`, and `|` and checks each segment. It accepts inspection commands (`cat`, `rg`, `ls`, `jq`, and similar), read-only `git status`, `log`, `diff`, `show`, `branch`, `remote`, `ls-files`, and `grep`, `git -C` only for the working directory, and checks such as `npm test`, `cargo test`, and `tsc --noEmit`. It rejects redirects, substitutions, globs, variables, background jobs, in-place, output, and exec flags, and unknown commands. `node`, `python`, and the other runtimes pass only as `<command> --version`. The PowerShell policy accepts reviewed `Get-*`, `Select-String`, `Format-*`, and similar cmdlets plus the same `git` and `gh` queries, and rejects variables, script blocks, redirects, aliases, and multiline input. A blocked call names its first blocked segment.

`safeSubcommands` in Settings trusts segments that start with a configured `<command> <subcommand>` prefix, for example `"kubectl": ["get"]`. Other segments still need the reviewed policy. A trusted segment can still change data or run code, also through `$(...)` in its arguments. The policies reduce risk; they are not a sandbox.

## Questionnaires and free discussion

A new workflow asks questions with `plan_mode_question`. In `tui` mode the questions show in the questionnaire panel of the [ask-user-question](../ask-user-question/README.md) extension, with its keys and its `collapseKey` and `notesKey` settings. In `rpc` mode, or when pi cannot show a panel, each question is a `ui.select` dialog with a `Type something.` row.

F2 switches between questionnaires and free discussion, from the idle editor or from an open questionnaire. Switching from a questionnaire closes it without answers, stops the run, and returns `discussion_requested`. In free discussion the agent asks plain-text questions, and `plan_mode_question` returns `questions_paused`. The `/plan` menu has the same switch. F2 is not registered when `keybindings.json` binds F2 or `toggleShortcut` is `f2`. A hidden `plan-mode-interaction` message tells the model the current mode.

## Completion and the ready menu

`plan_mode_complete` stores the plan and marks it ready. A `<proposed_plan>` block on its own lines in a read-only workflow also counts. When the run settles with questionnaires on, the ready menu opens: **Implement here**, **Start fresh and implement**, **Export plan…**, **Save for later**, **Stay in Plan mode**, and **Discard plan and exit**. In free discussion no menu opens; use `/plan`.

A new user message during a ready plan starts a revision and clears the ready plan until the next completion.

`/plan finalize` sends the upstream finalization prompt. With questionnaires on, a finalization run that ends without a question or a completed plan gets one retry with stronger tool guidance, and a second failure warns.

## Implementation

- **Implement here** leaves Plan mode and sends `Implement the plan.` in this session. A saved plan, or a plan with reinjection on, goes in the prompt instead. A saved plan first gets a model and auth check.
- **Start fresh and implement** opens a screen for a one-shot model and thinking level, checks the model's auth, and creates a new session linked to this one. The new session gets only the plan, in its first user prompt, and this session keeps the plan. pi gives `ctx.newSession` only to commands, so this action works from the `/plan` menu and not from the ready menu that opens by itself.
- **Save for later** keeps one plan in the session, outside the model context.
- **Export plan…** writes a new file. It never replaces a file. Exporting a ready plan leaves Plan mode.

**Plan reinjection** (`implementationPlanRetention`) controls the context after implementation starts:

| Value | Behavior |
| --- | --- |
| `clear-on-start` (default) | Conversation history only. No active plan state. |
| `clear-after-first-run` | The `context` hook keeps the exact plan in the context until the first implementation run settles. |
| `keep` | The `context` hook keeps the exact plan until `/plan exit` or a new workflow. |

## Managed draft

A workflow on the automatic tool policy gets a private Markdown draft when native `read`, `write`, and `edit` are active at its first model request. The draft is `<agentDir>/extensions-data/plan-mode/drafts/<workflow uuid>/plan.md` in directories with mode 700. A hidden draft contract gives the model the exact path. Direct `write` and `edit` calls on that path are allowed; nested calls and other paths are blocked. `plan_mode_complete({})` reads the file, which must be UTF-8 and at most 50 KiB, 2,000 lines, and 50,000 characters, and must be called alone. Workflows with `/plan tools` or `defaultPlanTools` are read-only and pass the plan as `plan_mode_complete({ plan })`. Drafts stay on disk after the workflow.

## Settings

`/plan settings` edits `<agentDir>/ext/plan-mode.json`. When that file does not exist, the upstream `<agentDir>/pi-plan-mode.json` applies, and the first save writes `ext/plan-mode.json` from it. There is no project file, because a repository could then widen the Plan policy. A changed file applies at its next use. An invalid file warns at session start, makes Settings read only, and the defaults apply.

| Key | Default | Use |
| --- | --- | --- |
| `thinkingLevel` | `inherit` | Thinking level while Plan mode is active. Plan mode restores the earlier level on exit, unless you changed it during Plan mode. |
| `defaultPlanTools` | automatic safe built-ins | Allowlist for sessions without a `/plan tools` choice. `[]` allows only the Plan tools. |
| `implementationPlanRetention` | `clear-on-start` | Plan reinjection, see above. |
| `defaultImplementationModel` | same as plan | `{ provider, modelId }` for **Start fresh and implement**. |
| `defaultImplementationThinkingLevel` | same as plan | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. |
| `defaultPlanExportPath` | `PLAN.md` | Export destination when `/plan export` has no path. |
| `safeSubcommands` | none | JSON only. `{ "<command>": ["<subcommand>", ...] }`, see Tool policy. |
| `toggleShortcut` | off | A pi key id that toggles Plan mode. Takes effect after `/reload`. |
| `planPane` | | Accepted for upstream files; there is no pane. |

## Session state

| Entry or message | Use |
| --- | --- |
| `plan-mode-state` custom entry | The workflow state. The latest entry on the branch restores it at session start and after `/tree`. |
| `plan-mode-transition` hidden message | The Plan or Normal mode contract, sent once at each transition. |
| `plan-mode-interaction` hidden message | The questionnaire or discussion contract. |
| `plan-mode-draft-contract` custom entry | Where the `context` hook puts the draft contract. |
| `proposed-plan` message | A plan that `/plan show` or a `<proposed_plan>` completion displays. |

The `context` hook puts a mode or interaction contract back at a fixed place after compaction removes it, and drops plan artifacts from the context after you leave Plan mode without implementing. `/tree` refuses to move to a hidden contract message.

## Differences from upstream

- There is no fullscreen Plan pane and no F4 shortcut: the native protocol has no side-pane surface. Use `/plan show`.
- There is no Workflow Mutex v1. That protocol depends on listeners changing the emitted object in the same process; `events.emit` sends a copy to other processes.
- Menus use pi's `ui.select` and `ui.input` dialogs. The model list has a **Filter models…** row instead of a search field.
- The ready menu that opens after completion cannot start a fresh session (see Implementation). Upstream keeps the command context of the `/plan` that started the workflow.
- The fresh handoff goes through `<agentDir>/extensions-data/plan-mode/handoff/<pi pid>.json`, which the new session reads within 2 minutes when its parent session matches. Upstream passes it in a `withSession` callback.
- The interaction fallback contract lives only in the state entry. Upstream also sends it as a steering message, which can add a turn.
- Boundaries use FNV-1a hashes. Boundaries from upstream sessions do not match, so their contracts are anchored again.
- The settings file is `ext/plan-mode.json`, and the draft directory is under `extensions-data/plan-mode`.
- The Bash policy is stricter. Upstream allows `python3 -c <code> --version`, `fd -Hx <command>`, `git grep -O<command>`, `sed` scripts in `-e` clusters, and `sort -uo <file>`; this port blocks them.
- Questionnaire answers and notes have no 4,000-character limit.
- pi-rust reports a busy session while `agent_settled` handlers run, so the finalization retry and the ready menu do not wait for idle there.
