# Team Harness Integration

This note describes how the Team feature should host the harness pipeline
without disturbing the existing tmux control path. Team remains the chat,
presence, and tmux-pane supervisor. The harness layer owns task routing,
phase state, durable output knowledge, and verification gates.

## Non-Goals For The First Slice

- Do not restart, resize, kill, or otherwise control user tmux sessions.
- Do not run `cargo test`, because the project test suite shares tmux state.
- Do not execute live provider dispatch from the harness scripts.
- Do not write outside the selected workspace except through the existing Team
  config/template seeding path.
- Do not modify the vendored `agora` crate; it stays a room-aware message bus.

## Shape

```text
phone Team tab
  -> team_* WebSocket RPC
  -> TeamManager
      -> agora Bus room and agent tmux panes
      -> HarnessManager (desktop-only, future)
          -> docs/harness/backlog/*.json
          -> docs/harness/routing.yaml
          -> docs/harness/phase-outputs/<task-id>/
          -> scripts/harness/* dry-run helpers
```

The first committed slice is intentionally light: a built-in `harness-dev`
roster template and this design note. It gives the operator a Team that speaks
the harness phase language today, while leaving the runtime adapter and UI strip
as explicit follow-up work.

## State Authority

Team chat is the coordination surface, not the source of truth. For any harness
task, authoritative state lives in the workspace:

```text
docs/harness/routing.yaml
docs/harness/task-source.md
docs/harness/project-profile.json
docs/harness/backlog/<task-id>.json
docs/harness/phase-outputs/<task-id>/index.md
docs/harness/phase-outputs/<task-id>/knowledge-manifest.json
docs/harness/phase-outputs/<task-id>/<phase-id>.md
```

Each phase agent must read the startup artifacts named by the manifest, write
its own fixed phase output, and include the Universal Handoff block. Chat
messages may summarize or assign work, but a downstream agent must be able to
resume from files alone.

## Runtime Adapter Plan

Add a desktop-only `HarnessManager` beside `TeamManager`, then expose a narrow
JSON boundary through the existing `team_*` RPC dispatch path:

```text
team_harness_status(room)
team_harness_init(room)
team_harness_create_task(room, objective)
team_harness_watch(room, task_id?)
team_harness_dispatch_dry_run(room, task_id, phase)
team_harness_claim_phase(room, task_id, phase, agent)
team_harness_approve_gate(room, task_id, gate, approval)
```

The adapter should run commands with `cwd` set to the active team's workspace.
This matters because the upstream harness scripts currently derive their root
from their own script path. Before using those scripts directly against an
arbitrary project, choose one of these modes:

- Scaffold mode: copy the required `scripts/harness/` and `docs/harness/`
  scaffold into the workspace, then run scripts from there.
- External-runtime mode: update the harness scripts to accept
  `--workspace-root` or `HARNESS_WORKSPACE_ROOT` so code can stay in a central
  install while state is written to the selected workspace.

Scaffold mode is the safer MVP because it preserves repo-local durable state
and does not require a broader harness-runtime refactor.

## Phase To Agent Mapping

The `harness-dev` roster uses the existing Team naming conventions so current
slash commands still address live agents:

| Harness phase | Team agent |
|---|---|
| `orchestrator_brain`, `intake`, `plan`, `handoff_memory` | `manager` |
| `research`, `intent_contract` | `researcher` |
| `dev_sprint` | `builder` |
| `plan_review`, `local_review`, `external_eval` | `reviewer` |
| `local_testing` | `tester` |
| `confirm`, `pr_deploy_gate`, `deploy`, `observe` | `devops` plus human approval |

The manager is the only role that should route new human requests. Other roles
wait until the manager assigns a specific canonical phase.

## UI Plan

Add a compact Harness strip above the chat log once the runtime adapter exists:

```text
Task: <task-id>    Phase: <phase-id> <status>
[Watch] [Dry Run] [Assign] [Approve Gate]
```

An expanded view should show the ordered phase list, current gate, startup
reads, and latest verification evidence. Keep it narrow: the Team tab is still
primarily a chat plus agent terminal preview surface.

## Permission Boundary

For the initial runtime adapter, only deterministic dry-run commands are
allowed:

```text
python3 scripts/harness/watch-backlog.py --backlog-root docs/harness/backlog --once --dry-run
python3 scripts/harness/watch-backlog.py --backlog-root docs/harness/backlog --pulse --dry-run
python3 scripts/harness/dispatch-phase-runner.py <phase_output_dir> <phase-id> --dry-run
```

Live provider execution requires an explicit approval record and should remain
disabled until Team also enforces workspace claims or a pre-tool write guard.
Without that guard, harness claims are process evidence, not a hard edit-time
boundary.

## Next Implementation Slices

1. Scaffold a workspace-local harness skeleton from the Team UI.
2. Add `team_harness_status`, `team_harness_watch`, and
   `team_harness_dispatch_dry_run`.
3. Add the compact Harness strip to `Team.svelte`.
4. Add phase claim/update writes for `docs/harness/backlog/*.json`.
5. Add explicit human approval handling for confirm/deploy/provider execution.
6. Add workspace-claim and write-guard enforcement before live execution.
