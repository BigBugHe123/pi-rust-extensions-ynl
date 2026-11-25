# background-tasks

Created 2026-10-30. Last updated 2026-11-19.

Durable background shell jobs for pi, ported from the core of `pi-background-tasks` (v2.6.10). The agent starts long-running commands (tests, builds, dev servers, watchers) without blocking a turn. A notification reports the end state. Jobs keep running across `/reload`, and in RPC mode (Paseo, ACP) across pi restarts. Fusion, delegated agents, attested runs and Anthropic attribution are not ported.

## Tools

Tool and parameter names match upstream, so prompts written for it keep working.

| Tool | Parameters | Result |
| --- | --- | --- |
| `bg_run` | `name`, `command`, `isAgent`, `description?`, `timeoutSeconds?`, `notifyOnCompletion?` (default true), `triggerOnCompletion?` (default true), `surviveReload?` (default true) | Task id and output path, at once |
| `bg_status` | `taskId?` (id or unique prefix) | One task, or all tasks of the session |
| `bg_logs` | `taskId`, `maxBytes?` (default and cap 50KB), `tail?` (default true) | Bounded output with the full output path when truncated |
| `bg_kill` | `taskId` | Stops the process group: SIGTERM, then SIGKILL after 3 seconds |

When a task completes, fails, times out or is lost, the extension sends a `background-task-notification` custom message (`deliverAs: followUp`). It starts a turn when `triggerOnCompletion` is true; then the extension also emits `agent:wake` on `pi.events`, which ends a running `sleep` of the `timers` extension. Tasks stopped by `bg_kill`, `/kill`, `/tasks` or session shutdown do not send a notification. Any other task with `notifyOnCompletion` gets one notification, also across pi restarts ([pi restarts](#pi-restarts)).

`before_agent_start` adds the compact `pi_background_shell_policy` section: use `bg_run` for long commands, continue independent work or end the turn, wait for notifications without sleeping or polling, and when tasks stop. Tool registrations omit duplicate snippets and guidelines; defaults and the log byte cap are in the schemas.

## Commands

| Command | Action |
| --- | --- |
| `/bg [--no-survive-reload] [--agent] [--name "Task name"] <command>` | Starts a task. Its notification does not start a turn. |
| `/tasks widget [collapse\|expand\|auto]` | Changes the widget below the editor; without an argument it changes between one line and the list. |
| `/tasks [id]`, `/bg-tasks [id]` | Task manager panel: `↑↓`/`jk` select, `enter` logs, `x` twice stops, `c` removes finished tasks, `esc`/`q` closes or goes back. Refreshes every second. |
| `/jobs` | Lists tasks. |
| `/logs <id> [maxBytes]` | Shows the output tail. Completes task ids. |
| `/kill <id>` | Stops a running task. Completes running task ids. |
| `/bg-clear` | Removes finished tasks and their output files. |

The footer status `background-tasks` shows counts by state (`bg: 1 running, 2 completed`). A widget below the editor lists up to 3 running tasks with elapsed time. With more running tasks, it shows one line with the count and `/tasks`. `/tasks widget` changes between one line and a list of up to 20 tasks; `/tasks widget collapse`, `expand`, or `auto` sets the view.

## Compactor active work

The extension publishes its running tasks as the `[Active Work]` block `background-tasks` of the compactor (channel `compactor:active-work`): one line with id, name, and command (or description) per task, at most 10. The text has no times, so a running task sends no updates. A finished or killed task leaves the block; no running task removes it. It publishes on each change, also without a UI, and again for each `compactor:active-work:sync` request.

## Reload and shutdown

Each task runs detached in its own process group. Its stdout and stderr go to a file, and a wrapper shell writes the exit code to a file. A new extension process reads the task files at `session_start` and adopts the tasks. It then finds exit codes, dead processes and timeouts.

- `session_shutdown` with reason `reload`: tasks with `surviveReload: true` keep running; the others stop.
- Reason `quit` in RPC mode (`ctx.mode` is `rpc`; pi-rust's ACP mode binds extensions in RPC mode too) in a session with a file: as for `reload`. A client such as Paseo stops pi and starts it again on the same session, and the new pi adopts the tasks ([pi restarts](#pi-restarts)).
- Reason `quit` in the other modes (`tui`, `print`, `json`) or without a session file (`--no-session`, which no later pi can resume), and reasons `new`, `resume`, `fork`: all running tasks stop, as upstream.
- When pi exits without `session_shutdown`, or the extension process ends on a signal, tasks keep running and the next session start with the same session id adopts them.

Upstream defaults `surviveReload` to false. This port defaults it to true because reload survival is the main feature.

## pi restarts

Paseo stops pi when a runtime is idle for 60 minutes and when its daemon restarts or reloads: stdin EOF, then SIGTERM to pi and each descendant (found by parent pid), then SIGKILL after 2 seconds (`taskkill /T /F` on Windows). It then resumes the session with `--session <file>`, which keeps the session id. Tasks survive this:

1. Launch. When the extension runs from the `pi-rust-ext` binary, it starts each task with `surviveReload: true` with `pi-rust-ext background-tasks --launch <task dir> <cwd> <command>`. The launcher starts the wrapper shell, prints the wrapper's pid and start time as one JSON line, and exits at once. The wrapper's parent is then not in pi's process tree, so a kill of that tree does not reach the task. On Unix the wrapper leads a process group of its own. On Windows it starts with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB` (a hidden console that the task's console programs share), and again without the breakaway when pi's job does not allow it. The extension starts the wrapper of a task with `surviveReload: false` itself, as before, so a kill of pi's tree ends it whichever comes first, the kill or `session_shutdown`. It does the same when the launcher does not run or does not start the task.
2. Stop. On stdin EOF pi sends `session_shutdown` with reason `quit`, and in RPC mode the tasks with `surviveReload: true` keep running when the session has a file. The SIGTERM ends the extension process at once; it stops no task.
3. Adoption. The resumed pi has the same session id, so its extension process reads the same task files at `session_start` and follows the tasks: `bg_status`, `bg_logs`, `bg_kill`, the widget, timeouts, and the notification work as before the stop.
4. One notification. pi only queues a message that it gets during a turn, and a stopped pi loses its queue. So the extension writes `noticeSentAt` to `task.json` before it sends the notification, and sets `notified` only once the session holds a `custom_message` entry with `customType: background-task-notification` and the task's id in `details.id`. A process that finds `noticeSentAt` without `notified` sends nothing when the session already holds the notification. When pi was idle for 10 seconds and the notification is still not in the session, it was lost, and the extension sends it again (at most 3 sends per process). The extension reads the session entries (a copy of the whole session) only for unconfirmed notifications, only while pi is idle, and at most every 3 seconds.

## Paths

- Task state: `<agentDir>/ext/background-tasks/sessions/<sessionId>/<taskId>/task.json`, with `notified` and `noticeSentAt` for the notification
- Output: `.../<taskId>/output.log`
- Exit code: `.../<taskId>/exit`

On unix, the extension creates these directories with mode 0700 and these files with mode 0600.

At spawn the launcher records the start time of the task's process (`ps -o lstart=`, `LC_ALL=C`) as `processStart`, while that process is still its child, so its pid cannot belong to another process yet. Before each signal the extension compares the start time of the pid with `processStart`. When the pid is gone, belongs to another process, or has no readable start time, the extension sends no signal and marks the task `lost` (or `completed`/`failed` when the exit file exists). After TERM, KILL goes to the group only when the leader still matches or a group member seen before TERM is still in the group. Task records without `processStart` become `lost` when their pid is alive.

The extension has no config file. Commands run with `/bin/bash -c` (or `/bin/sh -c` without bash) in the session cwd. On Windows they run with `cmd /C` and stop with `taskkill /T /F`.

## Tests

`cargo test -p pi-ext-background-tasks` runs unit tests (parsing, prefix resolution, bounded reads, state and old `task.json` files, panel keys, process lifecycle, recovery, timeout, TERM-ignoring descendants, shutdown rules by mode and reason, and the confirm, dedupe and resend rules of notifications on a test clock), protocol tests against an in-memory host, Unix tests of the binary and its launcher with a scripted pi (a Paseo stop, adoption by the next process, one notification, a lost and a saved notification), and e2e tests under the dev pi binary (`PI_BIN`; they skip when the binary is missing): tools and prompt, a quit of pi in RPC mode with and without a session file, and a Paseo stop of pi in RPC mode with a resume on the same session.
