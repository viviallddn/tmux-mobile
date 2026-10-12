//! In-process team supervisor (desktop only).
//!
//! This is the Rust port of the standalone `team/` Python launcher, moved
//! inside the app so the user never starts a separate process: the desktop
//! server itself seeds the team into the team bus and reconciles the desired
//! roster into real agent windows in tmux. The agent CLIs (kiro/claude/codex)
//! still run as their own processes in tmux panes — that is intrinsic, and it
//! is exactly what lets the Team tab preview an agent's live execution state.
//!
//! Flow (mirrors team's supervisor, but native):
//!   1. `seed_default_team` registers the built-in team (manager/worker/
//!      reviewer) as employees on the bus, if no team is present yet.
//!   2. a reconcile loop polls the bus's employee roster: a `requested`/`active`
//!      employee not yet launched gets its backend config written + a named
//!      tmux window opened; a `disabled` one has its window killed. The same
//!      path serves the initial team and any runtime `hire`/`fire`.
//!
//! The bus is reached through [`TeamBridge`] (JSON-only) so this module stays
//! decoupled from team's concrete types, like the rest of server.rs.

use crate::server::TeamBridge;
use crate::tmux;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Shared team brief + keepalive hook, embedded so a packaged .app has no
// external file dependency. Written to the team work dir at startup.
const KEEPALIVE_SH: &str = include_str!("../../team/hooks/keepalive.sh");
const HEARTBEAT_SH: &str = include_str!("../../team/hooks/heartbeat.sh");
const USAGE_SH: &str = include_str!("../../team/hooks/usage.sh");
const TEAM_WAIT_SH: &str = r#"#!/bin/sh
set -eu
base="${TEAM_API_URL:-${TEAM_HB_URL%/api/heartbeat}}"
timeout="${1:-50000}"
roster="${TEAM_WAIT_ROSTER:-compact}"
[ -x "${TEAM_USAGE_SH:-}" ] && "$TEAM_USAGE_SH" >/dev/null 2>&1 || true
curl -sS -G "$base/api/agent/wait" \
  -H "x-agent: ${TEAM_AGENT:?TEAM_AGENT is required}" \
  -H "x-room: ${TEAM_ROOM:-main}" \
  -H "x-agent-role: ${TEAM_AGENT_ROLE:-}" \
  --data-urlencode "timeout_ms=$timeout" \
  --data-urlencode "roster=$roster"
printf '\n'
"#;
const TEAM_POST_SH: &str = r#"#!/bin/sh
set -eu
requires_reply=false
if [ "${1:-}" = "--requires-reply" ]; then
  requires_reply=true
  shift
fi
body="$*"
if [ -z "$body" ]; then
  echo "usage: team-post [--requires-reply] 'message body'" >&2
  exit 2
fi
base="${TEAM_API_URL:-${TEAM_HB_URL%/api/heartbeat}}"
[ -x "${TEAM_USAGE_SH:-}" ] && "$TEAM_USAGE_SH" >/dev/null 2>&1 || true
printf '%s' "$body" | curl -sS -X POST "$base/api/agent/post?requires_reply=$requires_reply" \
  -H "x-agent: ${TEAM_AGENT:?TEAM_AGENT is required}" \
  -H "x-room: ${TEAM_ROOM:-main}" \
  -H "x-agent-role: ${TEAM_AGENT_ROLE:-}" \
  --data-binary @-
printf '\n'
"#;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);
/// A launched pane must join the bus within this window. If it does not, the
/// command probably died, stalled at a prompt, or started with stale config; we
/// replace the window and try again.
const STARTUP_JOIN_GRACE_MS: i64 = 90_000;
/// After repeated launch errors/stale starts, keep retrying but cool down so a
/// missing CLI or broken auth does not churn tmux windows every reconcile tick.
const LAUNCH_FAILURE_COOLDOWN_MS: i64 = 60_000;
/// Self-heal threshold: no `wait` and no heartbeat for this long ⇒ the agent is
/// wedged; the supervisor nudges its window (Esc + reconnect re-prompt). Well
/// above the bus's 90s `unreachable` mark so we only ever auto-restart an agent
/// that has been silent for a genuinely long time, not one merely between tools.
const RECOVERY_STALE_MS: i64 = 1_800_000; // 30 minutes
/// Codex currently has no reliable Stop hook equivalent to Claude's keepalive.
/// After it finishes a turn it can sit at the TUI prompt, no longer calling
/// `wait`. Treat that prompt-idle state as parkable before the bus's 90s stale
/// overlay paints it red. If it has unread work, wake it much sooner than the
/// park threshold; otherwise a prompt-idle Codex can sit on an @assignment for
/// over a minute before the supervisor notices.
const CODEX_PROMPT_PARK_MS: i64 = 75_000;
const CODEX_PROMPT_WAKE_MS: i64 = 10_000;
const CODEX_PROMPT_NUDGE_COOLDOWN_MS: i64 = 15_000;
const PROMPT_IDLE_HISTORY_LIMIT: i64 = 500;
/// Idle-sleep threshold: when EVERY non-offline agent has been parked in `wait`
/// (status=`idle`) for this long, the supervisor sends Esc to each pane to
/// cancel the in-flight `wait` MCP call. The agent's CLI falls back to its
/// shell prompt — no more 50-second wait/think cycles burning tokens. Any new
/// message in the room (typically the human resuming) wakes the team back up
/// via the standard reconnect nudge. Set to 0 to disable.
const IDLE_SLEEP_MS: i64 = 5 * 60 * 1000; // 5 min

// ─── Team templates (named rosters under <config>/tmux-mobile/teams/) ──────
// A template is a JSON file `teams/<name>.json` = { "agents": [ {name, backend,
// role, goal, model, manage}, … ] }. The user edits these from the
// app (Templates panel); `start_team` seeds the chosen template into the room.
// The built-in default is written to teams/default.json on first run so there
// is always something to edit.

/// Default model placeholder substituted in when a kiro agent leaves model empty.
pub const BUILTIN_TEMPLATE: &str = include_str!("../../team/templates/default/team.yaml");

/// A ready-made software-development roster (tech-lead / product / architect /
/// coder / reviewer / tester), seeded alongside the default so it appears in
/// the app's template picker out of the box. The whole collaboration workflow
/// lives in each agent's `goal` (role isolation) — team-brief.md stays a
/// role-agnostic, workflow-free communication contract.
pub const SOFTWARE_DEV_TEMPLATE: &str = include_str!("../../team/templates/software-dev/team.yaml");

/// A harness-pipeline software roster. It maps canonical harness phases onto
/// the existing Team chat + tmux-pane model, but keeps live provider dispatch
/// and risky actions behind explicit human approval.
pub const HARNESS_DEV_TEMPLATE: &str = include_str!("../../team/templates/harness-dev/team.yaml");

/// A financial-research roster modeled on Dexter (virattt/dexter): a research
/// director plus fundamentals / market+sentiment / valuation(DCF) / memo /
/// reviewer analysts. Dexter's single-agent skills (DCF, investment memo, X
/// sentiment) become specialist roles; its data discipline (figures carry
/// sources, the deliverable is a file, chat is a scannable header) and its
/// educational-only / not-investment-advice posture are baked into the goals.
pub const FINANCIAL_RESEARCH_TEMPLATE: &str =
    include_str!("../../team/templates/financial-research/team.yaml");

/// A deep-research roster: a director who decomposes the question, two parallel
/// researchers, a synthesist, and a skeptic — every claim sourced, output to
/// report.md.
pub const DEEP_RESEARCH_TEMPLATE: &str =
    include_str!("../../team/templates/deep-research/team.yaml");

/// A product-strategy roster inspired by the think-tank workflow: lead,
/// independent research lanes, roadmap synthesis, adversarial challenge, and
/// evidence/risk audit for product evolution and iteration direction.
pub const PRODUCT_STRATEGY_TEMPLATE: &str =
    include_str!("../../team/templates/product-strategy/team.yaml");

/// A content-studio roster (editor-in-chief / researcher / writer / copy editor)
/// for shipping a publish-ready article or docs in a shared house style.
pub const CONTENT_STUDIO_TEMPLATE: &str =
    include_str!("../../team/templates/content-studio/team.yaml");

/// A data-analysis roster (lead / data engineer / analyst / reporter) that
/// answers a question from data with reproducible work and honest caveats.
pub const DATA_ANALYSIS_TEMPLATE: &str =
    include_str!("../../team/templates/data-analysis/team.yaml");

/// Built-in templates seeded into teams/ on first run: (file stem, contents).
const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    ("default", BUILTIN_TEMPLATE),
    ("software-dev", SOFTWARE_DEV_TEMPLATE),
    ("harness-dev", HARNESS_DEV_TEMPLATE),
    ("financial-research", FINANCIAL_RESEARCH_TEMPLATE),
    ("deep-research", DEEP_RESEARCH_TEMPLATE),
    ("product-strategy", PRODUCT_STRATEGY_TEMPLATE),
    ("content-studio", CONTENT_STUDIO_TEMPLATE),
    ("data-analysis", DATA_ANALYSIS_TEMPLATE),
];

/// The teams/ template directory.
fn templates_dir() -> PathBuf {
    crate::config::config_dir().join("teams")
}

/// Sanitize a template name to a safe single path segment (no escaping the dir).
fn sanitize_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if safe.trim_matches('-').is_empty() {
        "default".to_string()
    } else {
        safe
    }
}

/// A template now lives in its OWN folder `teams/<name>/`, holding `team.yaml`
/// (the roster + per-agent env/mcp/skills) and optionally a `skills/` dir of
/// local skills bundled with the team. The folder is the unit so a team can
/// carry its own assets.
fn team_dir(name: &str) -> PathBuf {
    templates_dir().join(sanitize_name(name))
}

fn template_yaml_path(name: &str) -> PathBuf {
    team_dir(name).join("team.yaml")
}

/// One-time migration of the old flat `teams/<name>.json` files into the new
/// `teams/<name>/team.yaml` folder layout. The legacy file is renamed to
/// `<name>.json.bak` (kept, not deleted) so the move is reversible.
fn migrate_legacy_json() {
    let dir = templates_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let yaml_path = team_dir(stem).join("team.yaml");
        if yaml_path.exists() {
            continue; // already migrated
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(parent) = yaml_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(yaml) = serde_yml::to_string(&val) {
            if std::fs::write(&yaml_path, yaml).is_ok() {
                let _ = std::fs::rename(&p, p.with_extension("json.bak"));
                println!("🜂 team: migrated legacy template '{}' → team.yaml", stem);
            }
        }
    }
}

/// Ensure the teams/ dir exists and holds the built-in templates. Migrates any
/// legacy `*.json` first, then seed-once per folder: an existing template is
/// never overwritten, so a user's edits (and custom templates) survive restarts.
pub fn ensure_templates_seeded() {
    let dir = templates_dir();
    let _ = std::fs::create_dir_all(&dir);
    migrate_legacy_json();
    for (name, body) in BUILTIN_TEMPLATES {
        let path = template_yaml_path(name);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, body);
        }
    }
}

/// List available template names (folders in teams/ that hold a team.yaml).
pub fn list_templates() -> Vec<String> {
    ensure_templates_seeded();
    let mut names: Vec<String> = std::fs::read_dir(templates_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    if p.is_dir() && p.join("team.yaml").is_file() {
                        p.file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Read a template's full definition object (`{ env?, agents }`) from YAML, or
/// `null` if missing/bad.
pub fn read_team_def(name: &str) -> Value {
    std::fs::read_to_string(template_yaml_path(name))
        .ok()
        .and_then(|s| serde_yml::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null)
}

/// Read a template's agent list (the `agents` array), or empty if missing/bad.
pub fn read_template(name: &str) -> Vec<Value> {
    read_team_def(name)
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Read every template as `{ name, env, mcp, skills, prompt, agents }` for the
/// editor panel (team-wide fields + the roster).
pub fn read_all_templates() -> Vec<Value> {
    list_templates()
        .into_iter()
        .map(|name| {
            let def = read_team_def(&name);
            serde_json::json!({
                "name": name,
                "env": def.get("env").cloned().unwrap_or(serde_json::json!({})),
                "mcp": def.get("mcp").cloned().unwrap_or(serde_json::json!([])),
                "skills": def.get("skills").cloned().unwrap_or(serde_json::json!([])),
                "prompt": def.get("prompt").cloned().unwrap_or(serde_json::json!("")),
                "agents": def.get("agents").cloned().unwrap_or(serde_json::json!([])),
            })
        })
        .collect()
}

/// Write a template from the full definition object `{ env?, mcp?, skills?,
/// prompt?, agents }`. Empty team-wide fields are pruned so the YAML stays tidy.
pub fn save_template(name: &str, def: &Value) -> Result<(), String> {
    ensure_templates_seeded();
    // Accept either a full def object or a bare agents array (legacy callers).
    let def = if def.is_array() {
        serde_json::json!({ "agents": def })
    } else {
        def.clone()
    };
    let mut out = serde_json::Map::new();
    if let Some(env) = def.get("env").and_then(|v| v.as_object()) {
        if !env.is_empty() {
            out.insert("env".into(), Value::Object(env.clone()));
        }
    }
    if let Some(mcp) = def.get("mcp").and_then(|v| v.as_array()) {
        if !mcp.is_empty() {
            out.insert("mcp".into(), Value::Array(mcp.clone()));
        }
    }
    if let Some(sk) = def.get("skills").and_then(|v| v.as_array()) {
        if !sk.is_empty() {
            out.insert("skills".into(), Value::Array(sk.clone()));
        }
    }
    if let Some(p) = def.get("prompt").and_then(|v| v.as_str()) {
        if !p.trim().is_empty() {
            out.insert("prompt".into(), Value::String(p.to_string()));
        }
    }
    out.insert(
        "agents".into(),
        def.get("agents").cloned().unwrap_or(serde_json::json!([])),
    );
    let yaml = serde_yml::to_string(&Value::Object(out)).map_err(|e| e.to_string())?;
    let path = template_yaml_path(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, yaml).map_err(|e| e.to_string())
}

/// Delete a template folder (the built-in default is protected).
pub fn delete_template(name: &str) -> Result<(), String> {
    if name == "default" {
        return Err("the default template cannot be deleted".into());
    }
    std::fs::remove_dir_all(team_dir(name)).map_err(|e| e.to_string())
}

/// Per-team config home under `<workspace>/.tmm/`. Lives inside the project but
/// is self-gitignored (`.tmm/.gitignore` = `*`). Normal workspace-slug teams keep
/// the historical `.tmm/` layout; cloned rooms use `.tmm/rooms/<room>/` so two
/// teams can run against the same workspace without sharing backend homes.
struct Paths {
    /// Agents' working directory (the user's project) — agents run `-c` here.
    workspace: PathBuf,
    /// Our private per-team config root: `<workspace>/.tmm/`
    kiro_home: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    bin: PathBuf,
    launch: PathBuf,
    keepalive: PathBuf,
    heartbeat: PathBuf,
    usage: PathBuf,
}

impl Paths {
    fn new(workspace: &str) -> Self {
        Self::under(workspace, PathBuf::from(workspace).join(".tmm"))
    }

    fn for_room(workspace: &str, room: &str) -> Self {
        if room == workspace_slug(workspace) {
            Self::new(workspace)
        } else {
            Self::under(
                workspace,
                PathBuf::from(workspace)
                    .join(".tmm")
                    .join("rooms")
                    .join(sanitize_name(room)),
            )
        }
    }

    fn under(workspace: &str, home: PathBuf) -> Self {
        Paths {
            workspace: PathBuf::from(workspace),
            kiro_home: home.join("kiro-home"),
            claude: home.join("claude"),
            codex: home.join("codex"),
            bin: home.join("bin"),
            launch: home.join("launch"),
            keepalive: home.join("keepalive.sh"),
            heartbeat: home.join("heartbeat.sh"),
            usage: home.join("usage.sh"),
        }
    }
}

/// Server-level config the supervisor needs (bus URL + default model). The
/// per-run session + workspace are passed to `start`.
#[derive(Clone)]
pub struct TeamConfig {
    /// Bus URL the agents connect to over HTTP MCP (the in-process daemon).
    pub url: String,
    /// Default model for kiro-backed agents.
    pub model: String,
    /// Shared collaboration rules prepended to every agent's brief.
    pub team_rules: String,
    /// The kick message that connects an agent to the bus loop.
    pub team_kick: String,
}

/// Start the team for `workspace`: seed the default roster and spawn the
/// reconcile loop, launching agents into a per-workspace tmux session. The
/// agents' working directory is `workspace` (the user's project); our config +
/// brief live in a private per-team home, never written into the project.
/// Best-effort — any failure is logged, never fatal.
pub fn start(
    bridge: Arc<dyn TeamBridge>,
    cfg: TeamConfig,
    room: String,
    workspace: String,
    template: String,
) {
    tokio::spawn(async move {
        let session = format!("tmm-team-{}", room);
        let paths = Paths::for_room(&workspace, &room);
        let tpl = if template.trim().is_empty() {
            "default".to_string()
        } else {
            template
        };
        if let Err(e) = prepare_home(&paths) {
            eprintln!("⚠️  team: failed to prepare config home: {}", e);
            return;
        }
        seed_template(&*bridge, &room, &tpl, &cfg);
        println!(
            "🜂 team: room={} workspace={} template={} session={}",
            room, workspace, tpl, session
        );
        reconcile_loop(bridge, cfg, room, session, paths).await;
    });
}

/// Sanitize a workspace path into a tmux-safe slug. The slug = sanitized
/// basename + 6-char hash of the full canonical path. This guarantees uniqueness
/// even when two workspaces share a basename (e.g. `/a/demo` vs `/b/demo`).
/// tmux session names can't contain ':' or '.'.
pub fn workspace_slug(workspace: &str) -> String {
    use sha2::{Digest, Sha256};
    let canonical =
        std::fs::canonicalize(workspace).unwrap_or_else(|_| std::path::PathBuf::from(workspace));
    let base = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("root");
    let mut name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    name.make_ascii_lowercase();
    let name = name.trim_matches('-');
    let name = if name.is_empty() {
        "root"
    } else {
        &name[..name.len().min(24)]
    };
    // 6 hex chars of SHA-256 of full path → 16M buckets, collision-free in practice.
    let hash = format!(
        "{:x}",
        Sha256::digest(canonical.to_string_lossy().as_bytes())
    );
    format!("{}-{}", name, &hash[..6])
}

// ─── Global system prompt (shared across every team + agent) ──────────────
// A single editable text at <config>/tmux-mobile/system_prompt.md, prepended to
// the brief that EVERY agent reads at startup. Use it for project-wide
// conventions, tone, language preference, etc. — instructions that should apply
// regardless of team or role. Empty by default (no-op).

fn system_prompt_path() -> PathBuf {
    crate::config::config_dir().join("system_prompt.md")
}

/// Read the global system prompt (empty string if unset).
pub fn read_system_prompt() -> String {
    std::fs::read_to_string(system_prompt_path()).unwrap_or_default()
}

/// Write the global system prompt (creates the file; empty clears it).
pub fn save_system_prompt(text: &str) -> Result<(), String> {
    let _ = std::fs::create_dir_all(crate::config::config_dir());
    std::fs::write(system_prompt_path(), text).map_err(|e| e.to_string())
}

/// Write hooks into our private per-team home (`<workspace>/.tmm/`).
/// The agent prompt is now fully inline (no external brief file).
fn prepare_home(p: &Paths) -> std::io::Result<()> {
    std::fs::create_dir_all(&p.kiro_home)?;
    // Self-gitignore: `.tmm/.gitignore` = `*`
    let tmm_dir = p.workspace.join(".tmm");
    let gi = tmm_dir.join(".gitignore");
    if !gi.exists() {
        std::fs::write(&gi, "*\n")?;
    }
    std::fs::write(&p.keepalive, KEEPALIVE_SH)?;
    std::fs::write(&p.heartbeat, HEARTBEAT_SH)?;
    std::fs::write(&p.usage, USAGE_SH)?;
    std::fs::create_dir_all(&p.bin)?;
    std::fs::create_dir_all(&p.launch)?;
    let team_wait = p.bin.join("team-wait");
    let team_post = p.bin.join("team-post");
    std::fs::write(&team_wait, TEAM_WAIT_SH)?;
    std::fs::write(&team_post, TEAM_POST_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p.keepalive, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(&p.heartbeat, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(&p.usage, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(&team_wait, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(&team_post, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Seed the chosen `template`'s roster as employees, unless the room already has
/// a team (so a restart doesn't duplicate-seed; seed_employee also rejects taken
/// names). Each agent's `spec` is the template entry; an empty `model` on a kiro
/// agent falls back to the server default.
fn seed_template(bridge: &dyn TeamBridge, room: &str, template: &str, cfg: &TeamConfig) {
    let existing = bridge.employee_specs(room);
    if !existing.is_empty() {
        return; // already seeded (this run or a recovered one)
    }
    let def = read_team_def(template);
    let agents = def
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    if agents.is_empty() {
        eprintln!(
            "⚠️  team: template '{}' empty/missing; nothing to seed",
            template
        );
        return;
    }
    // Team-wide config applies to every agent. env merges (agent overrides);
    // mcp/skills are prepended so per-agent entries come last (agent wins on a
    // same-named MCP server when the launcher builds the server map).
    let team_env = def
        .get("env")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let team_mcp = def
        .get("mcp")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let team_skills = def
        .get("skills")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let team_prompt = def.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    let mut names = Vec::new();
    for a in &agents {
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let backend = a.get("backend").and_then(|v| v.as_str()).unwrap_or("kiro");
        // Empty model on a kiro agent → server default; other backends keep null.
        let model = a
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let model = match (backend, model) {
            (_, Some(m)) => Value::String(m.to_string()),
            ("kiro", None) => Value::String(cfg.model.clone()),
            _ => Value::Null,
        };
        let env = merge_env(&team_env, a.get("env"));
        let agent_mcp = a
            .get("mcp")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let agent_skills = a
            .get("skills")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let spec = serde_json::json!({
            "role": a.get("role").and_then(|v| v.as_str()).unwrap_or(name),
            "goal": a.get("goal").and_then(|v| v.as_str()).unwrap_or(""),
            "backend": backend,
            "manage": a.get("manage").and_then(|v| v.as_bool()).unwrap_or(false),
            "model": model,
            "env": env,
            "mcp": merge_list(&team_mcp, &agent_mcp),
            "skills": merge_list(&team_skills, &agent_skills),
            // The team folder is where local (relative) skills resolve from.
            "team_dir": team_dir(template).to_string_lossy(),
            "team_prompt": team_prompt,
        });
        if let Err(e) = bridge.seed_employee(room, name, &spec) {
            eprintln!("⚠️  team: seed '{}' failed: {}", name, e);
        } else {
            names.push(name.to_string());
        }
    }
    println!(
        "🜂 team: seeded '{}' ({}); launching…",
        template,
        names.join(" · ")
    );
}

/// Concatenate a team-wide list with a per-agent list (team first), de-duping
/// string entries (skills) and keeping objects as-is (mcp — the launcher builds
/// a name-keyed map, so a later per-agent server overrides a same-named team one).
fn merge_list(team: &[Value], agent: &[Value]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for v in team.iter().chain(agent.iter()) {
        if let Some(s) = v.as_str() {
            if out.iter().any(|x| x.as_str() == Some(s)) {
                continue;
            }
        }
        out.push(v.clone());
    }
    Value::Array(out)
}

/// Merge a team-wide env object with a per-agent env object (agent wins) into a
/// flat JSON object. Either side may be absent/non-object.
fn merge_env(team_env: &Value, agent_env: Option<&Value>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(o) = team_env.as_object() {
        for (k, v) in o {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(o) = agent_env.and_then(|v| v.as_object()) {
        for (k, v) in o {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Reconcile the desired roster into real agent windows, forever (until the
/// process exits). `launched` maps a name → its pane id (or None if adopted).
///
/// Idempotency (the dup-window fix): before launching, we check tmux for an
/// EXISTING window named after the agent in this session. If one is there
/// (server restarted, agent already running), we adopt it instead of opening a
/// second. The previous in-memory-only tracking re-launched every agent on
/// restart, piling up duplicate manager/worker/reviewer windows. Reconnecting
/// adopted agents after a *server* restart is handled separately, once, by
/// `nudge_session_agents` (called from recovery) — not here, because the loop's
/// presence check can't tell a healthy agent from one hung on a dead socket.
async fn reconcile_loop(
    bridge: Arc<dyn TeamBridge>,
    cfg: TeamConfig,
    room: String,
    session: String,
    paths: Paths,
) {
    const MAX_LAUNCH_FAILURES: u32 = 3; // cool down after this many failures
    let mut launched: HashMap<String, Option<String>> = HashMap::new();
    let mut launch_started: HashMap<String, i64> = HashMap::new();
    let mut fail_count: HashMap<String, u32> = HashMap::new();
    let mut last_failure: HashMap<String, i64> = HashMap::new();
    let mut last_nudge: HashMap<String, i64> = HashMap::new(); // self-heal cooldown
    let mut last_prompt_nudge: HashMap<String, i64> = HashMap::new();
    let mut launched_any = false;
    let mut sleep_state = SleepState::default();
    loop {
        // Stop the loop once the team is closed. close_team removes the room
        // from the registry AND kills the session — exit on either signal (the
        // room check also covers a team closed before any agent launched).
        if !bridge.room_exists(&room) || (launched_any && !tmux::session_exists(&session)) {
            println!("🜂 team: room '{}' closed; supervisor exiting", room);
            return;
        }
        let employees = bridge.employee_specs(&room);
        let mut roster = roster_liveness(&*bridge, &room);
        let now = now_ms();
        for (name, spec, state) in &employees {
            if state == "disabled" {
                // Kill any window we launched OR an orphan window with this name.
                if let Some(Some(pane)) = launched.get(name) {
                    let _ = tmux::kill_window(pane);
                } else if let Some(pane) = tmux::find_window_by_name(&session, name) {
                    let _ = tmux::kill_window(&pane);
                }
                launched.remove(name);
                launch_started.remove(name);
                continue;
            }
            let online = roster
                .get(name)
                .map(|p| p.status != "offline")
                .unwrap_or(false);
            if online {
                launched.remove(name);
                launch_started.remove(name);
                fail_count.remove(name);
                last_failure.remove(name);
                continue;
            }
            if let Some(existing) = launched.get(name).cloned() {
                let launched_at = launch_started.get(name).copied().unwrap_or(now);
                let startup_stale = launched_agent_needs_replacement(state, launched_at, now);
                if !startup_stale {
                    continue;
                }
                let pane = existing.or_else(|| tmux::find_window_by_name(&session, name));
                if let Some(pane) = pane {
                    println!(
                        "🜂 team: '{}' did not join after {}s — replacing {}",
                        name,
                        (now - launched_at) / 1000,
                        pane
                    );
                    write_pane_capture(&paths, name, &pane, "startup stale before replacement");
                    if let Err(e) = tmux::kill_window(&pane) {
                        eprintln!("⚠️  team: could not kill stale '{}': {}", name, e);
                        continue;
                    }
                } else {
                    println!(
                        "🜂 team: '{}' did not join after {}s and has no pane — retrying",
                        name,
                        (now - launched_at) / 1000
                    );
                }
                launched.remove(name);
                launch_started.remove(name);
                let n = fail_count.entry(name.clone()).or_insert(0);
                *n += 1;
                last_failure.insert(name.clone(), now);
            }
            // Already-online agents are handled above. A leftover window for
            // an offline requested agent is stale startup debris; replace it
            // so fixes to the launch command take effect.
            if let Some(pane) = tmux::find_window_by_name(&session, name) {
                if state == "requested" {
                    println!(
                        "🜂 team: replacing stale requested window for '{}' ({})",
                        name, pane
                    );
                    write_pane_capture(&paths, name, &pane, "requested stale before replacement");
                    if let Err(e) = tmux::kill_window(&pane) {
                        eprintln!(
                            "⚠️  team: could not replace stale window for '{}': {}",
                            name, e
                        );
                        continue;
                    }
                } else {
                    println!("🜂 team: adopted existing window for '{}' ({})", name, pane);
                    launched.insert(name.clone(), Some(pane));
                    launch_started.insert(name.clone(), now);
                    launched_any = true;
                    continue;
                }
            }
            // Give up after repeated failures (e.g. backend CLI not installed)
            // only for a cooldown window. The Team feature is meant to recover
            // from auth/config fixes without requiring another server restart.
            if fail_count.get(name).copied().unwrap_or(0) >= MAX_LAUNCH_FAILURES
                && now - last_failure.get(name).copied().unwrap_or(0) < LAUNCH_FAILURE_COOLDOWN_MS
            {
                continue;
            }
            match launch_agent(name, spec, &cfg, &room, &session, &paths) {
                Ok(pane) => {
                    println!("🜂 team: launched '{}' in window {}", name, pane);
                    launched.insert(name.clone(), Some(pane));
                    launch_started.insert(name.clone(), now);
                    launched_any = true;
                }
                Err(e) => {
                    let n = fail_count.entry(name.clone()).or_insert(0);
                    *n += 1;
                    last_failure.insert(name.clone(), now);
                    eprintln!(
                        "⚠️  team: launch '{}' failed ({}/{}): {}",
                        name, n, MAX_LAUNCH_FAILURES, e
                    );
                }
            }
        }
        // ── Codex prompt-idle park / wake ────────────────────────────────
        // Claude/Kiro have Stop hooks that self-nudge back into `wait` when a
        // turn ends. Codex does not, so a healthy Codex agent can finish its
        // turn and sit at the prompt; from the bus's point of view that looks
        // like a dead idle wait loop. Park such agents as `sleeping` unless
        // they have actionable unread messages, in which case wake them now.
        let recent_messages = recent_room_messages(&*bridge, &room, PROMPT_IDLE_HISTORY_LIMIT);
        for (name, spec, state) in &employees {
            if state == "disabled" || agent_backend(spec) != "codex" {
                continue;
            }
            let Some(presence) = roster.get(name).cloned() else {
                continue;
            };
            match prompt_idle_action(
                name,
                &presence,
                &roster,
                &recent_messages,
                now,
                CODEX_PROMPT_PARK_MS,
                CODEX_PROMPT_WAKE_MS,
            ) {
                PromptIdleAction::Park => {
                    let _ = bridge.set_agent_status(&room, name, "sleeping");
                    if let Some(p) = roster.get_mut(name) {
                        p.status = "sleeping".to_string();
                        p.last_seen = now;
                    }
                }
                PromptIdleAction::Wake => {
                    if now - last_prompt_nudge.get(name).copied().unwrap_or(0)
                        < CODEX_PROMPT_NUDGE_COOLDOWN_MS
                    {
                        continue;
                    }
                    if let Some(pane) = tmux::find_window_by_name(&session, name) {
                        println!(
                            "🜂 team: codex agent '{}' has unread work while prompt-idle — nudging {}",
                            name, pane
                        );
                        let _ = bridge.set_agent_status(&room, name, "idle");
                        if let Some(p) = roster.get_mut(name) {
                            p.status = "idle".to_string();
                            p.last_seen = now;
                        }
                        tokio::spawn(async move {
                            nudge_pane(&pane).await;
                        });
                        last_prompt_nudge.insert(name.clone(), now);
                    }
                }
                PromptIdleAction::None => {}
            }
        }
        // ── Sleep / wake ─────────────────────────────────────────────────
        // Once every non-offline agent has been parked in `wait` (status =
        // "idle") for IDLE_SLEEP_MS, send Esc to each pane to cancel the
        // in-flight wait. The CLI returns to its shell prompt and stops
        // burning a fresh LLM turn every 50s. Any new bus message — typically
        // the human resuming — wakes the team back up via `nudge_pane`. Skip
        // the existing self-heal while slept: this silence is intentional, not
        // a wedged agent.
        let online_idle = sleep_state.is_online_idle(&roster);
        let latest_seq = latest_room_seq(&*bridge, &room);
        match sleep_state.step(now, online_idle, latest_seq, IDLE_SLEEP_MS) {
            SleepAction::Sleep => {
                println!(
                    "🜂 team: room '{}' all-idle ≥ {}s — sending Esc to {} agents to sleep",
                    room,
                    IDLE_SLEEP_MS / 1000,
                    employees.iter().filter(|(_, _, s)| s != "disabled").count(),
                );
                for (name, _, state) in &employees {
                    if state == "disabled" {
                        continue;
                    }
                    if let Some(pane) = tmux::find_window_by_name(&session, name) {
                        let _ = tmux::send_keys(&pane, "Escape", false);
                    }
                }
            }
            SleepAction::Wake => {
                println!(
                    "🜂 team: room '{}' new message during sleep — waking agents",
                    room
                );
                for (name, _, state) in &employees {
                    if state == "disabled" {
                        continue;
                    }
                    // Clear the sleep label immediately for a responsive UI; the
                    // agent's own fresh `wait` will refine it to idle/thinking.
                    let _ = bridge.set_agent_status(&room, name, "idle");
                    if let Some(pane) = tmux::find_window_by_name(&session, name) {
                        tokio::spawn(async move {
                            nudge_pane(&pane).await;
                        });
                    }
                }
            }
            SleepAction::None => {}
        }

        // Re-assert "sleeping" every tick while slept. Our Esc takes ~1-2s to
        // actually cancel the agent's in-flight `wait`; in that window the wait
        // loop parks at least once more and writes "idle" (refreshing last_seen),
        // clobbering the status we just set. Setting it only once on the Sleep
        // tick therefore loses the race — the stale "idle" then ages into
        // "stalled" (red) after 90s, which is exactly the bug we saw. Re-stamping
        // it on every 3s tick wins: by the next tick the wait is truly stopped,
        // nothing else writes the row, and apply_presence never ages "sleeping"
        // itself, so the label sticks. Idempotent + cheap (one UPDATE per agent).
        if sleep_state.slept {
            for (name, _, state) in &employees {
                if state == "disabled" {
                    continue;
                }
                let _ = bridge.set_agent_status(&room, name, "sleeping");
            }
        }

        // ── Self-heal backstop ────────────────────────────────────────────
        // An agent we have heard NOTHING from for RECOVERY_STALE_MS — no `wait`
        // (a parked agent touches ~every second), no per-tool heartbeat — is
        // wedged: a dead MCP socket, a crashed loop, or a stop we never caught.
        // Nudge its window once (Esc to cancel the stuck call + a re-prompt to
        // resume `wait`), then cool down for the same window before trying
        // again, so we never spam — and so a genuinely long single tool (which
        // emits no heartbeat until it returns) is interrupted at most rarely.
        // Skipped while slept: the silence is intentional.
        if !sleep_state.slept {
            for (name, presence) in &roster {
                if matches!(presence.status.as_str(), "offline" | "sleeping")
                    || now - presence.last_seen < RECOVERY_STALE_MS
                {
                    continue;
                }
                if now - last_nudge.get(name).copied().unwrap_or(0) < RECOVERY_STALE_MS {
                    continue; // already nudged recently; give it time
                }
                if let Some(pane) = tmux::find_window_by_name(&session, name) {
                    println!(
                        "🜂 team: agent '{}' unreachable for {}s — self-heal nudging {}",
                        name,
                        (now - presence.last_seen) / 1000,
                        pane
                    );
                    tokio::spawn(async move {
                        nudge_pane(&pane).await;
                    });
                    last_nudge.insert(name.clone(), now);
                }
            }
        }

        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RosterPresence {
    status: String,
    last_seen: i64,
    cursor: i64,
}

fn roster_liveness(bridge: &dyn TeamBridge, room: &str) -> HashMap<String, RosterPresence> {
    let mut out = HashMap::new();
    if let Some(arr) = bridge.roster(room).get("roster").and_then(|v| v.as_array()) {
        for a in arr {
            let name = a.get("name").and_then(|v| v.as_str());
            let status = a.get("status").and_then(|v| v.as_str());
            let last_seen = a.get("last_seen").and_then(|v| v.as_i64()).unwrap_or(0);
            let cursor = a.get("cursor").and_then(|v| v.as_i64()).unwrap_or(0);
            if let (Some(n), Some(s)) = (name, status) {
                out.insert(
                    n.to_string(),
                    RosterPresence {
                        status: s.to_string(),
                        last_seen,
                        cursor,
                    },
                );
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptIdleAction {
    None,
    Park,
    Wake,
}

fn agent_backend(spec: &Value) -> &str {
    spec.get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("kiro")
}

fn recent_room_messages(bridge: &dyn TeamBridge, room: &str, limit: i64) -> Vec<Value> {
    bridge
        .history(room, limit)
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn prompt_idle_action(
    agent: &str,
    presence: &RosterPresence,
    roster: &HashMap<String, RosterPresence>,
    messages: &[Value],
    now: i64,
    park_ms: i64,
    wake_ms: i64,
) -> PromptIdleAction {
    if presence.status == "offline" {
        return PromptIdleAction::None;
    }
    let pending = has_actionable_unread(agent, presence.cursor, roster, messages);
    if presence.status == "sleeping" {
        return if pending {
            PromptIdleAction::Wake
        } else {
            PromptIdleAction::None
        };
    }

    let age = now.saturating_sub(presence.last_seen);
    if presence.status == "idle" {
        if pending && age >= wake_ms {
            return PromptIdleAction::Wake;
        }
        if !pending && age >= park_ms {
            return PromptIdleAction::Park;
        }
        return PromptIdleAction::None;
    }

    if presence.status == "stalled" && age < RECOVERY_STALE_MS {
        return if pending {
            PromptIdleAction::Wake
        } else {
            PromptIdleAction::Park
        };
    }

    PromptIdleAction::None
}

fn has_actionable_unread(
    agent: &str,
    cursor: i64,
    roster: &HashMap<String, RosterPresence>,
    messages: &[Value],
) -> bool {
    messages
        .iter()
        .filter(|m| m.get("seq").and_then(|v| v.as_i64()).unwrap_or(0) > cursor)
        .any(|m| message_is_actionable_for(agent, roster, m))
}

fn message_is_actionable_for(
    agent: &str,
    roster: &HashMap<String, RosterPresence>,
    message: &Value,
) -> bool {
    let from = message.get("from").and_then(|v| v.as_str()).unwrap_or("");
    if from.eq_ignore_ascii_case(agent) {
        return false;
    }
    if message_addresses_agent(message, agent) {
        return true;
    }
    let kind = message
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("msg");
    kind == "msg" && !roster.keys().any(|name| name.eq_ignore_ascii_case(from))
}

fn message_addresses_agent(message: &Value, agent: &str) -> bool {
    message
        .get("to")
        .and_then(|v| v.as_array())
        .map(|items| {
            items.iter().any(|t| {
                t.as_str()
                    .map(|s| {
                        s.eq_ignore_ascii_case(agent) || s.eq_ignore_ascii_case("all") || s == "*"
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn launched_agent_needs_replacement(state: &str, launched_at: i64, now: i64) -> bool {
    matches!(state, "requested" | "active") && now - launched_at >= STARTUP_JOIN_GRACE_MS
}

/// Latest message sequence number in `room`, or 0 if the bus is empty / the
/// room hasn't logged anything yet. Used as the anchor we compare against
/// while slept: any seq strictly greater than the anchor means a new message
/// has arrived (the human resuming, almost always) and the team should wake.
fn latest_room_seq(bridge: &dyn TeamBridge, room: &str) -> i64 {
    bridge
        .history(room, 1)
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
        .and_then(|m| m.get("seq"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// What `SleepState::step` decided this tick. The reconcile loop translates
/// each action into pane keystrokes (Esc to sleep, the standard reconnect
/// nudge to wake); separating decision from effect keeps the state machine
/// trivial to unit-test without tmux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepAction {
    /// Nothing to do this tick.
    None,
    /// All agents have been idle past the threshold — send Esc to every pane.
    Sleep,
    /// We're slept and a new bus message just arrived — re-prompt every pane
    /// back into `wait` (the existing `nudge_pane` primitive).
    Wake,
}

/// State for the idle-sleep / new-message-wake state machine that lives inside
/// `reconcile_loop`. Pulled into its own type so the decision logic can be
/// covered by plain unit tests (no tmux, no bus). Once `slept` is set, only a
/// new bus message clears it: status changes after sleep are EXPECTED (Esc'd
/// agents have no live `wait`, so they age to "stalled" within ~90s) and must
/// not be treated as a wake signal, otherwise we would oscillate.
#[derive(Debug, Default, Clone, Copy)]
struct SleepState {
    /// First tick on which all online agents looked idle, or `None` if any of
    /// them has been doing real work since.
    idle_since: Option<i64>,
    /// True between the Esc-everyone tick and the wake tick.
    slept: bool,
    /// Latest bus seq we saw at the moment we went to sleep. Wake when the
    /// real latest is strictly greater than this.
    sleep_anchor_seq: i64,
}

impl SleepState {
    /// Advance one tick. `online_idle` is "every non-offline agent has status
    /// `idle`" (caller computes from the roster); `latest_seq` is the latest
    /// bus sequence; `threshold_ms` is `IDLE_SLEEP_MS` (test injection).
    fn step(
        &mut self,
        now: i64,
        online_idle: bool,
        latest_seq: i64,
        threshold_ms: i64,
    ) -> SleepAction {
        if self.slept {
            // Once slept, ONLY a new message wakes us. (Status drift to
            // "stalled" while the wait MCP call is cancelled is expected.)
            if latest_seq > self.sleep_anchor_seq {
                self.slept = false;
                self.idle_since = None;
                self.sleep_anchor_seq = latest_seq;
                return SleepAction::Wake;
            }
            return SleepAction::None;
        }
        if !online_idle {
            self.idle_since = None;
            return SleepAction::None;
        }
        // Every online agent is idle. Start the clock if we haven't already,
        // then trip Sleep once the threshold is met.
        let started = *self.idle_since.get_or_insert(now);
        if threshold_ms > 0 && now - started >= threshold_ms {
            self.slept = true;
            self.sleep_anchor_seq = latest_seq;
            return SleepAction::Sleep;
        }
        SleepAction::None
    }

    /// Convenience: derive `online_idle` from the supervisor's roster snapshot
    /// (`roster_liveness` output). Empty room or no online agent ⇒ false (so
    /// we never sleep a team that hasn't even come online yet).
    fn is_online_idle(&self, roster: &HashMap<String, RosterPresence>) -> bool {
        let mut saw_online = false;
        for presence in roster.values() {
            if presence.status == "offline" {
                continue;
            }
            saw_online = true;
            if presence.status != "idle" && presence.status != "sleeping" {
                return false;
            }
        }
        saw_online
    }
}

/// Wall-clock millis since the epoch — same basis as the bus's `last_seen`, so
/// staleness math in the self-heal backstop lines up across the crate boundary.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Write the backend config for `name` and open a named tmux window running it.
/// Returns the new pane id. Blocking tmux/fs work runs on the caller (the
/// reconcile loop is its own task and the cadence is 3 s, so this is fine).
fn launch_agent(
    name: &str,
    spec: &Value,
    cfg: &TeamConfig,
    room: &str,
    session: &str,
    paths: &Paths,
) -> Result<String, String> {
    let backend = spec
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("kiro");
    let role = spec.get("role").and_then(|v| v.as_str()).unwrap_or(name);
    let goal = spec.get("goal").and_then(|v| v.as_str()).unwrap_or("");
    let model = spec.get("model").and_then(|v| v.as_str());
    let team_prompt = spec
        .get("team_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Per-agent extras (env / extra MCP servers / skills) from the team.yaml.
    let env: Vec<(String, String)> = spec
        .get("env")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let mcp: Vec<McpDef> = spec
        .get("mcp")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let skill_refs: Vec<String> = spec
        .get("skills")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let team_dir_ref = spec.get("team_dir").and_then(|v| v.as_str()).unwrap_or("");
    let skills = resolve_skills(&skill_refs, team_dir_ref);
    let extras = Extras { env, mcp, skills };

    let (env, cmd, post_keys) = match backend {
        "kiro" => prepare_kiro(
            name,
            role,
            goal,
            team_prompt,
            cfg,
            room,
            paths,
            model,
            &extras,
        )?,
        "claude" => prepare_claude(
            name,
            role,
            goal,
            team_prompt,
            cfg,
            room,
            paths,
            model,
            &extras,
        )?,
        "codex" => prepare_codex(
            name,
            role,
            goal,
            team_prompt,
            cfg,
            room,
            paths,
            model,
            &extras,
        )?,
        other => return Err(format!("unknown backend: {}", other)),
    };
    let launch_script = write_launch_script(paths, name, &env, &cmd)?;
    write_launch_manifest(
        paths,
        name,
        backend,
        role,
        room,
        &cmd,
        &launch_script,
        post_keys.len(),
    );

    let ws = paths.workspace.to_string_lossy().to_string();
    tmux::ensure_session(session, &ws)?;
    let pane = tmux::new_named_window(session, name, &ws)?;

    // Give the new shell a beat to initialize before sending the launch line.
    std::thread::sleep(Duration::from_millis(800));
    tmux::send_command(&pane, &shell_quote(&launch_script.to_string_lossy()))?;

    // Post-launch scripted steps (e.g. Claude's folder-trust accept + kick).
    for step in post_keys {
        std::thread::sleep(Duration::from_secs(4));
        match step {
            PostKey::Enter => {
                let _ = tmux::send_keys(&pane, "Enter", false);
            }
            PostKey::Text(t) => {
                if let Err(e) = tmux::paste_text(&pane, &t) {
                    eprintln!(
                        "⚠️  team: bracketed paste failed for '{}' ({}); falling back to send-keys",
                        name, e
                    );
                    let _ = tmux::send_keys(&pane, &t, true);
                }
            }
        }
    }
    write_pane_capture(paths, name, &pane, "post launch");
    Ok(pane)
}

fn write_launch_manifest(
    paths: &Paths,
    name: &str,
    backend: &str,
    role: &str,
    room: &str,
    cmd: &str,
    launch_script: &Path,
    post_steps: usize,
) {
    let _ = std::fs::create_dir_all(&paths.launch);
    let prompt_delivery = if post_steps == 0 && cmd.contains("TEAM_INITIAL_PROMPT_FILE") {
        "initial prompt argument via temp file"
    } else {
        "tmux paste-buffer with send-keys fallback"
    };
    let body = serde_json::json!({
        "agent": name,
        "backend": backend,
        "role": role,
        "room": room,
        "workspace": paths.workspace,
        "command": cmd,
        "launch_script": launch_script,
        "post_launch_steps": post_steps,
        "prompt_delivery": prompt_delivery,
        "team_wait": paths.bin.join("team-wait"),
        "team_post": paths.bin.join("team-post"),
        "capture": paths.launch.join(format!("{}.capture.txt", name)),
        "written_at_ms": now_ms(),
    });
    let path = paths.launch.join(format!("{}.json", name));
    if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()) {
        eprintln!(
            "⚠️  team: failed to write launch manifest for '{}': {}",
            name, e
        );
    }
}

fn write_launch_script(
    paths: &Paths,
    name: &str,
    env: &[(String, String)],
    cmd: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&paths.launch).map_err(|e| e.to_string())?;
    let path = paths.launch.join(format!("{}.sh", name));
    let mut body = String::new();
    body.push_str("#!/bin/sh\nset -eu\n");
    body.push_str(&format!(
        "cd {}\n",
        shell_quote(&paths.workspace.to_string_lossy())
    ));
    for (k, v) in env {
        if !valid_env_key(k) {
            return Err(format!("invalid environment key for '{}': {}", name, k));
        }
        body.push_str(&format!("export {}={}\n", k, shell_quote(v)));
    }
    body.push_str(&format!("exec {}\n", cmd));
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    Ok(path)
}

fn valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn write_pane_capture(paths: &Paths, name: &str, pane: &str, reason: &str) {
    let _ = std::fs::create_dir_all(&paths.launch);
    let path = paths.launch.join(format!("{}.capture.txt", name));
    let body = match tmux::capture_pane(pane, Some(160)) {
        Ok(content) => format!(
            "agent: {name}\npane: {pane}\nreason: {reason}\nwritten_at_ms: {}\n\n{}\n",
            now_ms(),
            content
        ),
        Err(e) => format!(
            "agent: {name}\npane: {pane}\nreason: {reason}\nwritten_at_ms: {}\n\ncapture failed: {e}\n",
            now_ms()
        ),
    };
    if let Err(e) = std::fs::write(&path, body) {
        eprintln!(
            "⚠️  team: failed to write pane capture for '{}': {}",
            name, e
        );
    }
}

/// After a *server* restart, nudge every agent window in `session` back online.
///
/// A recovered agent's MCP client lost its connection to the old (now dead)
/// daemon and is hung inside a `wait` tool call. Verified with kiro-cli 2.7.0:
/// the client neither times out nor retries on its own — but it reconnects fine
/// once the dead call is cancelled and a new turn starts. So for each agent
/// window we press Escape to cancel the in-flight call (returning the TUI to its
/// prompt), then send a short re-prompt that makes it call `wait` again, which
/// re-establishes the connection. Harmless if an agent happened to be healthy:
/// it just restarts its wait loop. This is done ONCE from recovery rather than
/// in the reconcile loop, whose presence check can't distinguish a healthy agent
/// from one hung on a dead socket (a just-restarted agent still looks "online"
/// for ~30 s until its presence TTL lapses).
///
/// Runs in a spawned task: it sleeps between keystrokes (TUI needs a beat to
/// settle) and we must not block the recovery path.
pub fn nudge_session_agents(session: String) {
    tokio::spawn(async move {
        // Give a freshly-restarted daemon a moment to be listening before we
        // ask agents to reconnect.
        tokio::time::sleep(Duration::from_secs(2)).await;
        for (name, pane) in tmux::list_named_windows(&session) {
            if name == "zsh" {
                continue; // the session's initial shell, not an agent
            }
            println!(
                "🜂 team: nudging adopted agent '{}' ({}) to reconnect",
                name, pane
            );
            nudge_pane(&pane).await;
        }
    });
}

/// The re-prompt that gets a wedged/stopped agent calling `wait` again.
const RECONNECT_NUDGE: &str = "Reconnect to the team chat: call the team MCP `wait` now, and keep calling it to stay in the conversation. If MCP `wait` is unavailable, run `./.tmm/bin/team-wait 50000`. Never run bare Bash `wait`; it is unrelated to the team chat.";

/// Press Esc (cancel any stuck in-flight call → back to the prompt), then send
/// the reconnect re-prompt and submit it. Shared by restart-recovery and the
/// supervisor's liveness self-heal. Sleeps between keystrokes (the TUI needs a
/// beat to settle), so callers run it inside a spawned task.
async fn nudge_pane(pane: &str) {
    let _ = tmux::send_keys(pane, "Escape", false);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = tmux::send_keys(pane, RECONNECT_NUDGE, true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = tmux::send_keys(pane, "Enter", false);
}

enum PostKey {
    Enter,
    Text(String),
}

/// What a backend `prepare_*` returns: (env vars, launch command, post-launch
/// scripted keys). Aliased to keep the per-backend signatures readable.
type Prepared = (Vec<(String, String)>, String, Vec<PostKey>);

const TEAM_SHELL_FALLBACK_PROMPT: &str = "\n\nIf the team MCP tools are unavailable or unclear, use the injected shell fallback commands instead: `team-wait 50000` to wait for team messages and `team-post 'message'` to post. Never run bare Bash `wait`; it waits for background jobs and is unrelated to the team chat.";

/// Build the complete agent system prompt with XML-structured layers.
/// - `<team-system-prompt>`: global rules (from config) + team-specific prompt
/// - `<role-system-prompt>`: this agent's role + goal
fn build_agent_prompt(role: &str, goal: &str, team_prompt: &str, cfg: &TeamConfig) -> String {
    let mut team_section = String::new();
    if !cfg.team_rules.trim().is_empty() {
        team_section.push_str(cfg.team_rules.trim());
    }
    if !team_prompt.trim().is_empty() {
        if !team_section.is_empty() {
            team_section.push_str("\n\n---\n\n");
        }
        team_section.push_str(team_prompt.trim());
    }
    format!(
        "<team-system-prompt>\n{}{}\n</team-system-prompt>\n\n<role-system-prompt>\nYou are the {}.\nGoal: {}\n</role-system-prompt>",
        team_section,
        TEAM_SHELL_FALLBACK_PROMPT,
        role,
        goal.trim()
    )
}

// ---- Kiro ----
#[allow(clippy::too_many_arguments)] // agent config genuinely needs all of these
/// An extra MCP server attached to an agent (from the team.yaml `mcp:` list).
/// Either a remote HTTP server (`url` [+ `headers`]) or a local stdio server
/// (`command` [+ `args`/`env`]).
#[derive(serde::Deserialize, Default, Clone)]
struct McpDef {
    name: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

/// A skill resolved to a concrete local directory (containing SKILL.md), ready
/// to wire into a backend.
struct ResolvedSkill {
    name: String,
    dir: PathBuf,
    description: String,
}

/// Per-agent extras threaded from the spec into each backend's launcher.
struct Extras {
    env: Vec<(String, String)>,
    mcp: Vec<McpDef>,
    skills: Vec<ResolvedSkill>,
}

/// kiro mcpServers entry: remote = `{url,headers}`, local = `{command,args,env}`.
fn kiro_mcp_value(m: &McpDef) -> Value {
    if let Some(url) = &m.url {
        let mut o = serde_json::json!({ "url": url });
        if !m.headers.is_empty() {
            o["headers"] = serde_json::to_value(&m.headers).unwrap_or(Value::Null);
        }
        o
    } else if let Some(cmd) = &m.command {
        let mut o = serde_json::json!({ "command": cmd, "args": m.args });
        if !m.env.is_empty() {
            o["env"] = serde_json::to_value(&m.env).unwrap_or(Value::Null);
        }
        o
    } else {
        serde_json::json!({})
    }
}

/// claude mcpServers entry: remote gets explicit `type:"http"`.
fn claude_mcp_value(m: &McpDef) -> Value {
    if let Some(url) = &m.url {
        let mut o = serde_json::json!({ "type": "http", "url": url });
        if !m.headers.is_empty() {
            o["headers"] = serde_json::to_value(&m.headers).unwrap_or(Value::Null);
        }
        o
    } else {
        kiro_mcp_value(m) // local stdio form is identical
    }
}

/// codex `[mcp_servers.<name>]` TOML block for an extra server.
fn codex_mcp_toml(m: &McpDef) -> String {
    let mut s = format!("\n[mcp_servers.{}]\n", m.name);
    if let Some(url) = &m.url {
        s += &format!(
            "url = \"{}\"\nenabled = true\nexperimental_use_rmcp_client = true\n",
            url
        );
        if !m.headers.is_empty() {
            s += &format!("\n[mcp_servers.{}.http_headers]\n", m.name);
            for (k, v) in &m.headers {
                s += &format!("\"{}\" = \"{}\"\n", k, v);
            }
        }
    } else if let Some(cmd) = &m.command {
        s += &format!("command = \"{}\"\n", cmd);
        if !m.args.is_empty() {
            let args: Vec<String> = m.args.iter().map(|a| format!("\"{}\"", a)).collect();
            s += &format!("args = [{}]\n", args.join(", "));
        }
        if !m.env.is_empty() {
            s += &format!("\n[mcp_servers.{}.env]\n", m.name);
            for (k, v) in &m.env {
                s += &format!("\"{}\" = \"{}\"\n", k, v);
            }
        }
    }
    s
}

/// A one-line skills index appended to the kick for backends without a native
/// skill mechanism (claude/codex). kiro instead gets `skill://` resources.
fn skills_index_text(skills: &[ResolvedSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(" Skills available — read the named SKILL.md before a matching task:");
    for sk in skills {
        s += &format!(
            " [{}] {} (at {}/SKILL.md);",
            sk.name,
            sk.description,
            sk.dir.display()
        );
    }
    s
}

fn skills_cache_dir() -> PathBuf {
    crate::config::config_dir().join("skills-cache")
}

/// Resolve each skill reference to a local directory. A reference is either a
/// local path (relative to the team folder, or absolute) or a GitHub URL, which
/// is sparse-cloned into a shared cache (reused across teams/agents).
fn resolve_skills(refs: &[String], team_dir: &str) -> Vec<ResolvedSkill> {
    let mut out = Vec::new();
    for r in refs {
        let r = r.trim();
        if r.is_empty() {
            continue;
        }
        let dir = if r.starts_with("http://") || r.starts_with("https://") {
            match fetch_git_skill(r) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("⚠️  team: skill '{}' fetch failed: {}", r, e);
                    continue;
                }
            }
        } else {
            let p = PathBuf::from(r);
            let p = if p.is_absolute() {
                p
            } else {
                PathBuf::from(team_dir).join(r)
            };
            if p.is_file() {
                p.parent().map(|x| x.to_path_buf()).unwrap_or(p)
            } else {
                p
            }
        };
        if !dir.exists() {
            eprintln!("⚠️  team: skill path not found: {}", dir.display());
            continue;
        }
        let (name, description) = read_skill_meta(&dir);
        out.push(ResolvedSkill {
            name,
            dir,
            description,
        });
    }
    out
}

/// Parse SKILL.md YAML frontmatter for name/description (best-effort).
fn read_skill_meta(dir: &std::path::Path) -> (String, String) {
    let fallback = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("skill")
        .to_string();
    let md = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
    let mut name = fallback;
    let mut desc = String::new();
    if let Some(rest) = md.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = rest[..end].trim_start_matches('\n');
            if let Ok(v) = serde_yml::from_str::<Value>(fm) {
                if let Some(n) = v.get("name").and_then(|x| x.as_str()) {
                    name = n.to_string();
                }
                if let Some(d) = v.get("description").and_then(|x| x.as_str()) {
                    desc = d.to_string();
                }
            }
        }
    }
    (name, desc)
}

/// Sparse-clone a GitHub `tree/<ref>/<subpath>` URL (or a bare repo URL) into the
/// shared skills cache and return the skill directory. Cache key = owner/repo/ref;
/// repeated refs to the same repo reuse the clone (sparse-checkout adds subpaths).
fn fetch_git_skill(url: &str) -> Result<PathBuf, String> {
    let (owner, repo, gitref, subpath) = parse_github(url)?;
    let repo_cache = skills_cache_dir().join(&owner).join(&repo).join(&gitref);
    let resolved = if subpath.is_empty() {
        repo_cache.clone()
    } else {
        repo_cache.join(&subpath)
    };
    // Cache hit: the subpath already materialised.
    if resolved.join("SKILL.md").is_file() || (subpath.is_empty() && resolved.exists()) {
        return Ok(resolved);
    }
    let repo_url = format!("https://github.com/{}/{}", owner, repo);
    if !repo_cache.join(".git").exists() {
        if let Some(p) = repo_cache.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_dir_all(&repo_cache);
        let out = std::process::Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--filter=blob:none",
                "--sparse",
                "--branch",
                &gitref,
                &repo_url,
            ])
            .arg(&repo_cache)
            .output()
            .map_err(|e| format!("spawn git: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "git clone: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    if !subpath.is_empty() {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_cache)
            .args(["sparse-checkout", "set", &subpath])
            .output()
            .map_err(|e| format!("spawn git: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "git sparse-checkout: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(resolved)
}

/// Parse a GitHub URL into (owner, repo, ref, subpath). Supports the `tree/<ref>/
/// <subpath>` form and a bare `owner/repo` (defaults ref=main, no subpath).
fn parse_github(url: &str) -> Result<(String, String, String, String), String> {
    let u = url.trim().trim_end_matches('/');
    let rest = u
        .strip_prefix("https://github.com/")
        .or_else(|| u.strip_prefix("http://github.com/"))
        .ok_or_else(|| format!("only github.com skill URLs are supported: {}", url))?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 2 {
        return Err(format!("expected github.com/owner/repo…: {}", url));
    }
    let owner = parts[0].to_string();
    let repo = parts[1].trim_end_matches(".git").to_string();
    if parts.len() >= 4 && (parts[2] == "tree" || parts[2] == "blob") {
        Ok((owner, repo, parts[3].to_string(), parts[4..].join("/")))
    } else {
        Ok((owner, repo, "main".to_string(), String::new()))
    }
}

/// Env the agent process exports so hooks can ping liveness and usage to the
/// daemon. Injected on every backend's launch line.
fn hook_env(
    name: &str,
    room: &str,
    role: &str,
    backend: &str,
    model: Option<&str>,
    cfg: &TeamConfig,
    paths: &Paths,
) -> Vec<(String, String)> {
    vec![
        (
            "TEAM_HB_URL".to_string(),
            format!("{}/api/heartbeat", cfg.url),
        ),
        (
            "TEAM_USAGE_URL".to_string(),
            format!("{}/api/usage", cfg.url),
        ),
        ("TEAM_API_URL".to_string(), cfg.url.clone()),
        ("TEAM_AGENT".to_string(), name.to_string()),
        ("TEAM_ROOM".to_string(), room.to_string()),
        ("TEAM_AGENT_ROLE".to_string(), role.to_string()),
        ("TEAM_BACKEND".to_string(), backend.to_string()),
        ("TEAM_MODEL".to_string(), model.unwrap_or("").to_string()),
        (
            "TEAM_WORKSPACE".to_string(),
            paths.workspace.to_string_lossy().to_string(),
        ),
        (
            "TEAM_USAGE_SH".to_string(),
            paths.usage.to_string_lossy().to_string(),
        ),
    ]
}

fn path_with_team_bin(paths: &Paths) -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    if current.is_empty() {
        paths.bin.to_string_lossy().to_string()
    } else {
        format!("{}:{}", paths.bin.to_string_lossy(), current)
    }
}

fn prepare_kiro(
    name: &str,
    role: &str,
    goal: &str,
    team_prompt: &str,
    cfg: &TeamConfig,
    room: &str,
    paths: &Paths,
    model: Option<&str>,
    extras: &Extras,
) -> Result<Prepared, String> {
    let home = &paths.kiro_home;
    std::fs::create_dir_all(home.join("agents")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(home.join("settings")).map_err(|e| e.to_string())?;
    std::fs::write(
        home.join("settings").join("cli.json"),
        serde_json::to_string_pretty(
            &serde_json::json!({ "chat.disableTrustAllConfirmation": true }),
        )
        .unwrap(),
    )
    .map_err(|e| e.to_string())?;

    let tools = vec!["*".to_string(), "@team".to_string()];

    // Skills are loaded as native skill:// resources.
    let resources: Vec<String> = extras
        .skills
        .iter()
        .map(|sk| format!("skill://{}/SKILL.md", sk.dir.to_string_lossy()))
        .collect();
    // The team MCP server plus any extra per-agent servers from the team.yaml.
    let mut mcp_servers = serde_json::json!({
        "team": { "url": format!("{}/mcp", cfg.url), "headers": { "x-agent": name, "x-room": room, "x-agent-role": role } }
    });
    {
        let obj = mcp_servers.as_object_mut().unwrap();
        for m in &extras.mcp {
            if !m.name.is_empty() {
                obj.insert(m.name.clone(), kiro_mcp_value(m));
            }
        }
    }
    let conf = serde_json::json!({
        "name": name,
        "description": format!("{} on the team bus", role),
        "prompt": build_agent_prompt(role, goal, team_prompt, cfg),
        "tools": tools,
        "allowedTools": ["*"],
        "resources": resources,
        "mcpServers": mcp_servers,
        "hooks": {
            "postToolUse": [
                { "matcher": "*", "command": paths.heartbeat.to_string_lossy() },
                { "matcher": "*", "command": paths.usage.to_string_lossy() }
            ],
            "userPromptSubmit": [
                { "command": paths.heartbeat.to_string_lossy() },
                { "command": paths.usage.to_string_lossy() }
            ],
            "stop": [
                { "command": paths.keepalive.to_string_lossy() },
                { "command": paths.usage.to_string_lossy() }
            ]
        },
    });
    std::fs::write(
        home.join("agents").join(format!("{}.json", name)),
        serde_json::to_string_pretty(&conf).unwrap(),
    )
    .map_err(|e| e.to_string())?;

    let m = model.filter(|m| !m.trim().is_empty()).unwrap_or(&cfg.model);
    let mut env = vec![("KIRO_HOME".to_string(), home.to_string_lossy().to_string())];
    env.extend(hook_env(name, room, role, "kiro", Some(m), cfg, paths));
    env.push(("PATH".to_string(), path_with_team_bin(paths)));
    env.extend(extras.env.iter().cloned());
    let cmd = format!(
        "kiro-cli chat --agent {} --model {} --trust-all-tools {}",
        name,
        m,
        shell_quote(&cfg.team_kick)
    );
    Ok((env, cmd, vec![]))
}

// ---- Claude Code ----
#[allow(clippy::too_many_arguments)]
fn prepare_claude(
    name: &str,
    role: &str,
    goal: &str,
    team_prompt: &str,
    cfg: &TeamConfig,
    room: &str,
    paths: &Paths,
    model: Option<&str>,
    extras: &Extras,
) -> Result<Prepared, String> {
    let d = &paths.claude;
    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    let mcpfile = d.join(format!("{}.mcp.json", name));
    let mut mcp_servers = serde_json::json!({
        "team": { "type": "http", "url": format!("{}/mcp", cfg.url), "headers": { "x-agent": name, "x-room": room, "x-agent-role": role } }
    });
    {
        let obj = mcp_servers.as_object_mut().unwrap();
        for m in &extras.mcp {
            if !m.name.is_empty() {
                obj.insert(m.name.clone(), claude_mcp_value(m));
            }
        }
    }
    std::fs::write(
        &mcpfile,
        serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": mcp_servers })).unwrap(),
    )
    .map_err(|e| e.to_string())?;
    let settingsfile = d.join(format!("{}.settings.json", name));
    std::fs::write(
        &settingsfile,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "PostToolUse": [ { "matcher": "*", "hooks": [
                    { "type": "command", "command": paths.heartbeat.to_string_lossy() },
                    { "type": "command", "command": paths.usage.to_string_lossy() }
                ] } ],
                "UserPromptSubmit": [ { "hooks": [
                    { "type": "command", "command": paths.heartbeat.to_string_lossy() },
                    { "type": "command", "command": paths.usage.to_string_lossy() }
                ] } ],
                "Stop": [ { "hooks": [
                    { "type": "command", "command": paths.keepalive.to_string_lossy() },
                    { "type": "command", "command": paths.usage.to_string_lossy() }
                ] } ]
            }
        }))
        .unwrap(),
    )
    .map_err(|e| e.to_string())?;

    let mut cmd = format!(
        "claude --mcp-config {} --strict-mcp-config --settings {}",
        shell_quote(&mcpfile.to_string_lossy()),
        shell_quote(&settingsfile.to_string_lossy()),
    );
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        cmd.push_str(&format!(" --model {}", shell_quote(m)));
    }
    cmd.push_str(" --permission-mode bypassPermissions --dangerously-skip-permissions");
    let first_msg = format!(
        "{}\n\n{}{}",
        build_agent_prompt(role, goal, team_prompt, cfg),
        cfg.team_kick,
        skills_index_text(&extras.skills)
    );
    // Start interactive; then accept the folder-trust dialog, type the kick, submit.
    let post = vec![PostKey::Enter, PostKey::Text(first_msg), PostKey::Enter];
    let mut env = hook_env(
        name,
        room,
        role,
        "claude",
        model.filter(|m| !m.trim().is_empty()),
        cfg,
        paths,
    );
    env.push(("PATH".to_string(), path_with_team_bin(paths)));
    env.extend(extras.env.iter().cloned());
    Ok((env, cmd, post))
}

// ---- Codex ----
#[allow(clippy::too_many_arguments)]
fn prepare_codex(
    name: &str,
    role: &str,
    goal: &str,
    team_prompt: &str,
    cfg: &TeamConfig,
    room: &str,
    paths: &Paths,
    model: Option<&str>,
    extras: &Extras,
) -> Result<Prepared, String> {
    let home = paths.codex.join(name);
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let mut config = String::new();
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        config.push_str(&format!("model = \"{}\"\n\n", toml_dq(m)));
    }
    config.push_str(&format!(
        "[mcp_servers.team]\nurl = \"{}/mcp\"\nenabled = true\nexperimental_use_rmcp_client = true\n\n[mcp_servers.team.http_headers]\n\"x-agent\" = \"{}\"\n\"x-room\" = \"{}\"\n\"x-agent-role\" = \"{}\"\n",
        cfg.url, name, room, role
    ));
    for m in &extras.mcp {
        if !m.name.is_empty() {
            config.push_str(&codex_mcp_toml(m));
        }
    }
    std::fs::write(home.join("config.toml"), config).map_err(|e| e.to_string())?;
    let hooks_file = home.join("hooks.json");
    std::fs::write(
        &hooks_file,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": paths.heartbeat.to_string_lossy(), "timeout": 10 },
                    { "type": "command", "command": paths.usage.to_string_lossy(), "timeout": 10 }
                ] } ],
                "PostToolUse": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": paths.heartbeat.to_string_lossy(), "timeout": 10 },
                    { "type": "command", "command": paths.usage.to_string_lossy(), "timeout": 10 }
                ] } ],
                "Stop": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": paths.keepalive.to_string_lossy(), "timeout": 10 },
                    { "type": "command", "command": paths.usage.to_string_lossy(), "timeout": 10 }
                ] } ]
            }
        }))
        .unwrap(),
    )
    .map_err(|e| e.to_string())?;
    let mut env = Vec::new();
    env.extend(hook_env(
        name,
        room,
        role,
        "codex",
        model.filter(|m| !m.trim().is_empty()),
        cfg,
        paths,
    ));
    env.push(("PATH".to_string(), path_with_team_bin(paths)));
    env.extend(extras.env.iter().cloned());
    let first_msg = format!(
        "{}\n\n{}\n\nCodex team-loop instructions: first try the team MCP tool `wait` with timeout_ms=50000. If you cannot call MCP `wait`/`post`, use the injected shell fallback commands `team-wait 50000` and `team-post 'message'` instead. Never run bare Bash `wait`; it is unrelated to the team chat. End every turn by calling MCP `wait` or `team-wait 50000`.{}",
        build_agent_prompt(role, goal, team_prompt, cfg),
        cfg.team_kick,
        skills_index_text(&extras.skills),
    );
    let prompt_file = home.join("initial_prompt.txt");
    std::fs::write(&prompt_file, &first_msg).map_err(|e| e.to_string())?;
    let mut args = vec![
        "codex".to_string(),
        "--enable".to_string(),
        "non_prefixed_mcp_tool_names".to_string(),
        "--no-alt-screen".to_string(),
        "--cd".to_string(),
        shell_quote(&paths.workspace.to_string_lossy()),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        "--dangerously-bypass-hook-trust".to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        args.push("--model".to_string());
        args.push(shell_quote(m));
    }
    push_codex_config_arg(
        &mut args,
        "mcp_servers.team.url",
        toml_string(&format!("{}/mcp", cfg.url)),
    );
    push_codex_config_arg(&mut args, "mcp_servers.team.enabled", "true".to_string());
    push_codex_config_arg(
        &mut args,
        "mcp_servers.team.experimental_use_rmcp_client",
        "true".to_string(),
    );
    push_codex_config_arg(
        &mut args,
        "mcp_servers.team.http_headers.x-agent",
        toml_string(name),
    );
    push_codex_config_arg(
        &mut args,
        "mcp_servers.team.http_headers.x-room",
        toml_string(room),
    );
    push_codex_config_arg(
        &mut args,
        "mcp_servers.team.http_headers.x-agent-role",
        toml_string(role),
    );
    push_codex_config_arg(
        &mut args,
        "core.hooksPath",
        toml_string(&hooks_file.to_string_lossy()),
    );
    for m in &extras.mcp {
        push_codex_mcp_config_args(&mut args, m);
    }
    env.push((
        "TEAM_INITIAL_PROMPT_FILE".to_string(),
        prompt_file.to_string_lossy().to_string(),
    ));
    let inner = format!(
        "prompt=$(cat \"$TEAM_INITIAL_PROMPT_FILE\") || exit $?; rm -f -- \"$TEAM_INITIAL_PROMPT_FILE\"; exec {} \"$prompt\"",
        args.join(" ")
    );
    let cmd = format!("sh -lc {}", shell_quote(&inner));
    Ok((env, cmd, Vec::new()))
}

fn push_codex_config_arg(args: &mut Vec<String>, key: &str, value: String) {
    args.push("-c".to_string());
    args.push(shell_quote(&format!("{}={}", key, value)));
}

fn push_codex_mcp_config_args(args: &mut Vec<String>, m: &McpDef) {
    if let Some(url) = &m.url {
        push_codex_config_arg(
            args,
            &format!("mcp_servers.{}.url", m.name),
            toml_string(url),
        );
        push_codex_config_arg(
            args,
            &format!("mcp_servers.{}.enabled", m.name),
            "true".to_string(),
        );
        push_codex_config_arg(
            args,
            &format!("mcp_servers.{}.experimental_use_rmcp_client", m.name),
            "true".to_string(),
        );
        for (k, v) in &m.headers {
            push_codex_config_arg(
                args,
                &format!("mcp_servers.{}.http_headers.{}", m.name, k),
                toml_string(v),
            );
        }
    } else if let Some(cmd) = &m.command {
        push_codex_config_arg(
            args,
            &format!("mcp_servers.{}.command", m.name),
            toml_string(cmd),
        );
        if !m.args.is_empty() {
            push_codex_config_arg(
                args,
                &format!("mcp_servers.{}.args", m.name),
                toml_string_array(&m.args),
            );
        }
        for (k, v) in &m.env {
            push_codex_config_arg(
                args,
                &format!("mcp_servers.{}.env.{}", m.name, k),
                toml_string(v),
            );
        }
    }
}

/// Single-quote a string for the shell (the agent launch line is sent to a
/// tmux pane's shell). Wraps in '…' and escapes embedded single quotes.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'=' | b':'))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn toml_dq(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", toml_dq(s))
}

fn toml_string_array(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── SleepState ──────────────────────────────────────────────────────
    // The state machine that mediates "all idle long enough → Esc the team"
    // and "new message arrived → wake them". Pure: no tmux, no bus.

    fn presence(status: &str, last_seen: i64, cursor: i64) -> RosterPresence {
        RosterPresence {
            status: status.to_string(),
            last_seen,
            cursor,
        }
    }

    fn idle_roster() -> HashMap<String, RosterPresence> {
        let mut r = HashMap::new();
        r.insert("alice".into(), presence("idle", 0, 0));
        r.insert("bob".into(), presence("idle", 0, 0));
        r
    }

    #[test]
    fn sleep_state_does_nothing_on_an_empty_room() {
        // No agents at all (a freshly-started team that hasn't booted yet)
        // must not be considered idle — there's no one to sleep.
        let mut s = SleepState::default();
        let empty: HashMap<String, RosterPresence> = HashMap::new();
        assert!(!s.is_online_idle(&empty));
        assert_eq!(s.step(0, false, 0, 60_000), SleepAction::None);
        assert!(!s.slept);
    }

    #[test]
    fn sleep_state_skips_if_any_agent_is_busy() {
        // The whole point: ONE working agent cancels sleep for everyone.
        let mut s = SleepState::default();
        let mut r = idle_roster();
        r.insert("worker".into(), presence("working", 0, 0));
        assert!(!s.is_online_idle(&r));
        assert_eq!(s.step(0, false, 0, 60_000), SleepAction::None);
        assert!(s.idle_since.is_none());
    }

    #[test]
    fn sleep_state_offline_agents_dont_block_sleep() {
        // Offline agents are deliberately gone — they don't count toward the
        // "is anyone busy?" check. The remaining online agents alone decide.
        let s = SleepState::default();
        let mut r = idle_roster();
        r.insert("ghost".into(), presence("offline", 0, 0));
        assert!(s.is_online_idle(&r));
    }

    #[test]
    fn sleep_state_treats_sleeping_agents_as_not_busy() {
        let s = SleepState::default();
        let mut r = idle_roster();
        r.insert("napper".into(), presence("sleeping", 0, 0));
        assert!(s.is_online_idle(&r));
    }

    #[test]
    fn sleep_state_arms_then_fires_at_threshold() {
        // First idle tick arms the timer; later ticks below threshold = no
        // action; once we cross threshold = Sleep + state flips.
        let mut s = SleepState::default();
        assert_eq!(s.step(1_000, true, 0, 60_000), SleepAction::None);
        assert_eq!(s.idle_since, Some(1_000));
        assert!(!s.slept);

        assert_eq!(s.step(30_000, true, 0, 60_000), SleepAction::None);
        assert!(!s.slept);

        assert_eq!(s.step(61_000, true, 5, 60_000), SleepAction::Sleep);
        assert!(s.slept);
        assert_eq!(s.sleep_anchor_seq, 5);
    }

    #[test]
    fn sleep_state_threshold_zero_disables_sleep() {
        // 0 means "feature off". We still arm idle_since (cheap), but never
        // trip Sleep no matter how long the team has been idle.
        let mut s = SleepState::default();
        assert_eq!(s.step(0, true, 0, 0), SleepAction::None);
        assert_eq!(s.step(86_400_000, true, 0, 0), SleepAction::None); // 1 day
        assert!(!s.slept);
    }

    #[test]
    fn sleep_state_resets_arming_when_an_agent_starts_working() {
        // If the team started looking idle, then someone actually picks work
        // back up before the threshold, we drop the timer entirely.
        let mut s = SleepState::default();
        s.step(1_000, true, 0, 60_000); // arm
        assert_eq!(s.idle_since, Some(1_000));

        s.step(2_000, false, 0, 60_000); // someone working
        assert!(s.idle_since.is_none());

        // And re-arming starts fresh, not from the original idle_since.
        s.step(3_000, true, 0, 60_000);
        assert_eq!(s.idle_since, Some(3_000));
    }

    #[test]
    fn sleep_state_wakes_only_on_a_new_message_seq() {
        // While slept, the rule is intentionally narrow: a strictly newer bus
        // seq than the anchor wakes. Stale roster (status drifting from idle
        // to stalled because the wait MCP call is dead) MUST NOT wake — that
        // would oscillate forever.
        let mut s = SleepState::default();
        // Force slept state at anchor seq=10.
        s.step(0, true, 10, 60_000);
        s.step(60_001, true, 10, 60_000); // → Sleep
        assert!(s.slept);
        assert_eq!(s.sleep_anchor_seq, 10);

        // Same seq, status drifts to stalled (online_idle now false). No wake.
        assert_eq!(s.step(60_002, false, 10, 60_000), SleepAction::None);
        assert!(s.slept);

        // New message: seq jumps to 11. Wake.
        assert_eq!(s.step(60_003, false, 11, 60_000), SleepAction::Wake);
        assert!(!s.slept);
        assert_eq!(s.sleep_anchor_seq, 11);
        assert!(s.idle_since.is_none());
    }

    #[test]
    fn sleep_state_can_re_sleep_after_a_wake_round_trip() {
        // Sleep → wake → idle again → sleep again. Exercises the full cycle.
        let mut s = SleepState::default();
        s.step(0, true, 0, 60_000);
        assert_eq!(s.step(60_000, true, 0, 60_000), SleepAction::Sleep);

        // Human posts (seq 1) → wake.
        assert_eq!(s.step(60_100, false, 1, 60_000), SleepAction::Wake);
        assert!(!s.slept);

        // Team handles it, returns to idle, threshold passes again → sleep.
        s.step(60_200, true, 1, 60_000); // re-arm
        assert_eq!(s.step(120_300, true, 2, 60_000), SleepAction::Sleep);
        assert_eq!(s.sleep_anchor_seq, 2);
    }

    // ── Codex prompt-idle handling ──────────────────────────────────────

    #[test]
    fn prompt_idle_parks_stale_codex_prompt_without_actionable_messages() {
        let mut roster = HashMap::new();
        roster.insert("manager".into(), presence("idle", 0, 10));
        let messages = vec![serde_json::json!({
            "seq": 10,
            "from": "human",
            "to": [],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "manager",
                roster.get("manager").unwrap(),
                &roster,
                &messages,
                80_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::Park
        );
    }

    #[test]
    fn prompt_idle_wakes_idle_codex_with_pending_after_short_grace() {
        let mut roster = HashMap::new();
        roster.insert("architect".into(), presence("idle", 0, 97));
        roster.insert("manager".into(), presence("idle", 0, 102));
        let messages = vec![serde_json::json!({
            "seq": 100,
            "from": "manager",
            "to": ["architect"],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "architect",
                roster.get("architect").unwrap(),
                &roster,
                &messages,
                12_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::Wake
        );
    }

    #[test]
    fn prompt_idle_does_not_wake_idle_codex_before_short_grace() {
        let mut roster = HashMap::new();
        roster.insert("architect".into(), presence("idle", 0, 97));
        roster.insert("manager".into(), presence("idle", 0, 102));
        let messages = vec![serde_json::json!({
            "seq": 100,
            "from": "manager",
            "to": ["architect"],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "architect",
                roster.get("architect").unwrap(),
                &roster,
                &messages,
                9_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::None
        );
    }

    #[test]
    fn prompt_idle_wakes_stale_codex_prompt_for_direct_assignment() {
        let mut roster = HashMap::new();
        roster.insert("architect".into(), presence("stalled", 0, 97));
        roster.insert("manager".into(), presence("idle", 0, 102));
        let messages = vec![serde_json::json!({
            "seq": 100,
            "from": "manager",
            "to": ["architect"],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "architect",
                roster.get("architect").unwrap(),
                &roster,
                &messages,
                100_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::Wake
        );
    }

    #[test]
    fn prompt_idle_wakes_sleeping_codex_for_human_message() {
        let mut roster = HashMap::new();
        roster.insert("manager".into(), presence("sleeping", 80_000, 10));
        let messages = vec![serde_json::json!({
            "seq": 11,
            "from": "human",
            "to": [],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "manager",
                roster.get("manager").unwrap(),
                &roster,
                &messages,
                80_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::Wake
        );
    }

    #[test]
    fn prompt_idle_does_not_wake_on_own_messages() {
        let mut roster = HashMap::new();
        roster.insert("manager".into(), presence("stalled", 0, 98));
        let messages = vec![serde_json::json!({
            "seq": 102,
            "from": "manager",
            "to": ["human"],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "manager",
                roster.get("manager").unwrap(),
                &roster,
                &messages,
                100_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::Park
        );
    }

    #[test]
    fn prompt_idle_leaves_working_agents_to_presence_ladder() {
        let mut roster = HashMap::new();
        roster.insert("reviewer".into(), presence("hardworking", 0, 10));
        let messages = vec![serde_json::json!({
            "seq": 11,
            "from": "manager",
            "to": ["reviewer"],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "reviewer",
                roster.get("reviewer").unwrap(),
                &roster,
                &messages,
                100_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::None
        );
    }

    #[test]
    fn prompt_idle_does_not_interrupt_online_codex() {
        let mut roster = HashMap::new();
        roster.insert("architect".into(), presence("online", 0, 10));
        roster.insert("manager".into(), presence("idle", 0, 11));
        let messages = vec![serde_json::json!({
            "seq": 11,
            "from": "manager",
            "to": ["architect"],
            "kind": "msg"
        })];

        assert_eq!(
            prompt_idle_action(
                "architect",
                roster.get("architect").unwrap(),
                &roster,
                &messages,
                100_000,
                75_000,
                10_000,
            ),
            PromptIdleAction::None
        );
    }

    // ── existing tests follow ──

    #[test]
    fn parse_github_tree_url() {
        let (o, r, gr, sub) = parse_github(
            "https://github.com/anthropics/claude-code/tree/main/plugins/frontend-design/skills/frontend-design",
        )
        .unwrap();
        assert_eq!(o, "anthropics");
        assert_eq!(r, "claude-code");
        assert_eq!(gr, "main");
        assert_eq!(sub, "plugins/frontend-design/skills/frontend-design");
    }

    #[test]
    fn parse_github_bare_repo_defaults_main() {
        let (o, r, gr, sub) = parse_github("https://github.com/owner/repo").unwrap();
        assert_eq!(
            (o.as_str(), r.as_str(), gr.as_str(), sub.as_str()),
            ("owner", "repo", "main", "")
        );
        assert!(
            parse_github("https://gitlab.com/x/y").is_err(),
            "only github.com supported"
        );
    }

    #[test]
    fn merge_env_agent_overrides_team() {
        let team = serde_json::json!({ "A": "1", "B": "team" });
        let agent = serde_json::json!({ "B": "agent", "C": "3" });
        let m = merge_env(&team, Some(&agent));
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "agent", "agent env wins");
        assert_eq!(m["C"], "3");
    }

    #[test]
    fn mcp_value_remote_and_local_per_backend() {
        let remote = McpDef {
            name: "gh".into(),
            url: Some("https://x/mcp".into()),
            headers: [("Authorization".to_string(), "Bearer t".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        // kiro remote omits an explicit type; claude tags it http.
        assert!(kiro_mcp_value(&remote).get("type").is_none());
        assert_eq!(claude_mcp_value(&remote)["type"], "http");
        assert_eq!(kiro_mcp_value(&remote)["url"], "https://x/mcp");

        let local = McpDef {
            name: "pg".into(),
            command: Some("mcp-pg".into()),
            args: vec!["--stdio".into()],
            ..Default::default()
        };
        let toml = codex_mcp_toml(&local);
        assert!(toml.contains("[mcp_servers.pg]"));
        assert!(toml.contains("command = \"mcp-pg\""));
        assert!(toml.contains("args = [\"--stdio\"]"));
    }

    // Records seed_employee calls so we can assert the default roster.
    struct RecordingBridge {
        seeded: Mutex<Vec<(String, Value)>>,
        existing: Vec<(String, Value, String)>,
    }
    impl TeamBridge for RecordingBridge {
        fn history(&self, _room: &str, _l: i64) -> Value {
            serde_json::json!({})
        }
        fn roster(&self, _room: &str) -> Value {
            serde_json::json!({ "roster": [] })
        }
        fn post(&self, _room: &str, _f: &str, _b: &str, _r: bool) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn set_agent_status(&self, _room: &str, _agent: &str, _status: &str) -> Result<(), String> {
            Ok(())
        }
        fn employees(&self, _room: &str) -> Value {
            serde_json::json!({})
        }
        fn usage(&self, _room: &str) -> Value {
            serde_json::json!({ "usage": [] })
        }
        fn seed_employee(&self, _room: &str, name: &str, spec: &Value) -> Result<(), String> {
            self.seeded
                .lock()
                .unwrap()
                .push((name.to_string(), spec.clone()));
            Ok(())
        }
        fn employee_specs(&self, _room: &str) -> Vec<(String, Value, String)> {
            self.existing.clone()
        }
        fn room_exists(&self, _room: &str) -> bool {
            true
        }
        fn start_team(&self, _workspace: &str, _template: &str) -> Value {
            serde_json::json!({ "started": false })
        }
        fn clone_team(&self, _room: &str) -> Value {
            serde_json::json!({ "started": false })
        }
        fn close_team(&self, _room: &str) -> bool {
            false
        }
        fn teams(&self) -> Value {
            serde_json::json!({ "teams": [] })
        }
        fn templates(&self) -> Value {
            serde_json::json!({ "templates": [] })
        }
        fn save_template(&self, _name: &str, _agents: &Value) -> Result<(), String> {
            Ok(())
        }
        fn delete_template(&self, _name: &str) -> Result<(), String> {
            Ok(())
        }
        fn system_prompt(&self) -> String {
            String::new()
        }
        fn save_system_prompt(&self, _text: &str) -> Result<(), String> {
            Ok(())
        }
        fn default_workspace(&self) -> String {
            "/tmp/ws".into()
        }
        fn recent_workspaces(&self) -> Value {
            serde_json::json!({ "recent_workspaces": [] })
        }
        fn view_base(&self) -> String {
            "http://127.0.0.1:8787".into()
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
            tokio::sync::broadcast::channel(1).1
        }
    }

    fn cfg() -> TeamConfig {
        TeamConfig {
            url: "http://127.0.0.1:8787".into(),
            model: "claude-sonnet-4.6".into(),
            team_rules: String::new(),
            team_kick: "kick".into(),
        }
    }

    #[test]
    fn builtin_default_template_is_minimal_manager_worker() {
        // The default template is the minimal demo: a manager + one worker that
        // shows the delegate→report loop and can grow via the manager's hire().
        let v: Value = serde_yml::from_str(BUILTIN_TEMPLATE).unwrap();
        let agents = v["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2, "minimal demo = manager + worker");
        let names: Vec<&str> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
        assert!(names.contains(&"manager") && names.contains(&"worker"));
        let managers = agents.iter().filter(|a| a["manage"] == true).count();
        assert_eq!(managers, 1, "exactly one manager");
        assert!(
            agents.iter().all(|a| a["model"] == ""),
            "models use the server default"
        );
    }

    #[test]
    fn software_dev_template_roster_and_tools() {
        // The software-dev roster is a built-in (teams/software-dev/team.yaml).
        let v: Value = serde_yml::from_str(SOFTWARE_DEV_TEMPLATE).unwrap();
        let agents = v["agents"].as_array().unwrap();
        assert_eq!(
            agents.len(),
            8,
            "manager+product+architect+frontend+backend+reviewer+tester+devops"
        );
        let names: Vec<&str> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
        for expected in [
            "manager",
            "product",
            "architect",
            "frontend",
            "backend",
            "reviewer",
            "tester",
            "devops",
        ] {
            assert!(
                names.contains(&expected),
                "missing role '{expected}': {names:?}"
            );
        }
        let managers = agents.iter().filter(|a| a["manage"] == true).count();
        assert_eq!(managers, 0, "hire/fire off: no manage=true agent");
        // Workflow lives in each role's goal (team-brief.md is contract-only), so
        // every agent must carry a substantive goal.
        assert!(
            agents
                .iter()
                .all(|a| a["goal"].as_str().map(|g| g.len() > 80).unwrap_or(false)),
            "each role's goal must carry its slice of the workflow"
        );
        assert!(
            agents.iter().all(|a| a["model"] == ""),
            "models use the server default"
        );
        let backend = |n: &str| {
            agents.iter().find(|a| a["name"] == n).unwrap()["backend"]
                .as_str()
                .unwrap()
                .to_string()
        };
        for analyst in ["manager", "product", "architect", "reviewer"] {
            assert_eq!(
                backend(analyst),
                "codex",
                "{analyst} should use codex for analysis/review"
            );
        }
        for builder in ["frontend", "backend", "tester", "devops"] {
            assert_eq!(
                backend(builder),
                "claude",
                "{builder} should use claude for build/test/delivery"
            );
        }

        // Per-agent tools wired via the new schema.
        let agent = |n: &str| agents.iter().find(|a| a["name"] == n).unwrap();
        let mcp_names = |n: &str| -> Vec<String> {
            agent(n)["mcp"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["name"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        for dev in [
            "architect",
            "frontend",
            "backend",
            "reviewer",
            "tester",
            "devops",
        ] {
            assert!(
                mcp_names(dev).contains(&"context7".to_string()),
                "{dev} should have context7"
            );
        }
        // architect/backend/reviewer/devops reach the AWS knowledge base.
        for n in ["architect", "backend", "reviewer", "devops"] {
            assert!(
                mcp_names(n).contains(&"aws-knowledge".to_string()),
                "{n} has AWS knowledge"
            );
        }
        assert!(
            mcp_names("tester").contains(&"chrome-devtools".to_string()),
            "tester has chrome-devtools for e2e"
        );
        let fe_skills = agent("frontend")["skills"].as_array().unwrap();
        assert!(
            fe_skills.iter().any(|s| s
                .as_str()
                .map(|x| x.contains("frontend-design"))
                .unwrap_or(false)),
            "frontend has the frontend-design skill"
        );
    }

    #[test]
    fn both_builtin_templates_are_seeded() {
        // Isolate config dir so we don't touch the real ~/.config.
        let dir = std::env::temp_dir().join(format!("teamtest-tpl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        ensure_templates_seeded();
        let mut names = list_templates();
        names.sort();
        for expected in [
            "default",
            "software-dev",
            "harness-dev",
            "financial-research",
            "deep-research",
            "product-strategy",
            "content-studio",
            "data-analysis",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected} seeded: {names:?}"
            );
        }

        // Each built-in parses and every agent carries a substantive goal.
        for (name, body) in BUILTIN_TEMPLATES {
            let v: Value =
                serde_yml::from_str(body).unwrap_or_else(|e| panic!("{name} bad yaml: {e}"));
            let agents = v["agents"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} has no agents"));
            assert!(!agents.is_empty(), "{name} empty roster");
            assert!(
                agents
                    .iter()
                    .all(|a| a["goal"].as_str().map(|g| g.len() > 80).unwrap_or(false)),
                "{name}: every role needs a substantive goal"
            );
        }

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn financial_research_template_has_lead_and_default_models() {
        let v: Value = serde_yml::from_str(FINANCIAL_RESEARCH_TEMPLATE).unwrap();
        let agents = v["agents"].as_array().unwrap();
        assert!(agents.len() >= 5, "a multi-analyst research team");
        let names: Vec<&str> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
        for expected in ["lead", "fundamentals", "valuation", "memo", "reviewer"] {
            assert!(
                names.contains(&expected),
                "missing role '{expected}': {names:?}"
            );
        }
        let managers = agents.iter().filter(|a| a["manage"] == true).count();
        assert_eq!(managers, 0, "hire/fire off: no manage=true agent");
        assert!(
            agents.iter().all(|a| a["model"] == ""),
            "models use the server default"
        );
        assert!(
            agents
                .iter()
                .all(|a| a["goal"].as_str().map(|g| g.len() > 80).unwrap_or(false)),
            "each role's goal must carry its methodology"
        );
    }

    #[test]
    fn product_strategy_template_matches_think_tank_shape() {
        let v: Value = serde_yml::from_str(PRODUCT_STRATEGY_TEMPLATE).unwrap();
        assert!(v["prompt"]
            .as_str()
            .unwrap_or("")
            .contains("product strategy think tank"));
        let agents = v["agents"].as_array().unwrap();
        assert_eq!(
            agents.len(),
            8,
            "lead + 4 research/synthesis lanes + challenge + audit"
        );
        let names: Vec<&str> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
        for expected in [
            "lead",
            "user_researcher",
            "market_researcher",
            "methodology",
            "capability",
            "synthesist",
            "challenger",
            "auditor",
        ] {
            assert!(
                names.contains(&expected),
                "missing role '{expected}': {names:?}"
            );
        }
        let managers = agents.iter().filter(|a| a["manage"] == true).count();
        assert_eq!(managers, 0, "hire/fire off: no manage=true agent");
        assert!(
            agents.iter().all(|a| a["model"] == ""),
            "models use the server default"
        );
        assert!(
            agents
                .iter()
                .all(|a| a["goal"].as_str().map(|g| g.len() > 80).unwrap_or(false)),
            "each role's goal must carry its product-strategy method"
        );
    }

    #[test]
    fn seed_template_skips_when_team_already_present() {
        let b = RecordingBridge {
            seeded: Mutex::new(vec![]),
            existing: vec![("manager".into(), Value::Null, "active".into())],
        };
        seed_template(&b, "myroom", "default", &cfg());
        assert!(
            b.seeded.lock().unwrap().is_empty(),
            "must not re-seed an existing team"
        );
    }

    #[test]
    fn seed_template_kiro_empty_model_falls_back_to_default() {
        // Built-in templates now pin "auto", so the empty-model fallback is for
        // USER-authored templates that leave model blank. Verify with a custom one.
        let dir = std::env::temp_dir().join(format!("teamtest-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let agents = serde_json::json!([
            { "name": "w", "backend": "kiro", "role": "w", "goal": "do things well", "model": "", "manage": false }
        ]);
        save_template("blankmodel", &agents).unwrap();
        let b = RecordingBridge {
            seeded: Mutex::new(vec![]),
            existing: vec![],
        };
        seed_template(&b, "myroom", "blankmodel", &cfg());
        let seeded = b.seeded.lock().unwrap();
        assert!(!seeded.is_empty(), "custom template should seed agents");
        // Empty model on a kiro agent inherits the server default.
        assert!(seeded
            .iter()
            .all(|(_, s)| s["model"] == "claude-sonnet-4.6"));

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn team_wide_config_folds_into_each_agent() {
        let dir = std::env::temp_dir().join(format!("teamtest-tw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let def = serde_json::json!({
            "env": { "TEAM": "1" },
            "mcp": [{ "name": "context7", "url": "https://mcp.context7.com/mcp" }],
            "skills": ["./skills/shared"],
            "prompt": "Team charter for all.",
            "agents": [{
                "name": "w", "backend": "kiro", "role": "w", "goal": "do things well",
                "env": { "A": "2" }, "mcp": [{ "name": "pg", "command": "x" }]
            }],
        });
        save_template("tw", &def).unwrap();
        // prompt round-trips into the saved YAML.
        assert_eq!(read_team_def("tw")["prompt"], "Team charter for all.");

        let b = RecordingBridge {
            seeded: Mutex::new(vec![]),
            existing: vec![],
        };
        seed_template(&b, "room", "tw", &cfg());
        let seeded = b.seeded.lock().unwrap();
        let (_, s) = seeded.first().expect("seeded one agent");
        assert_eq!(s["env"]["TEAM"], "1", "team env reaches the agent");
        assert_eq!(s["env"]["A"], "2", "agent env preserved");
        let mcp_names: Vec<&str> = s["mcp"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["name"].as_str())
            .collect();
        assert!(
            mcp_names.contains(&"context7"),
            "team mcp folded in: {mcp_names:?}"
        );
        assert!(mcp_names.contains(&"pg"), "agent mcp kept: {mcp_names:?}");
        assert!(
            s["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x.as_str() == Some("./skills/shared")),
            "team skill folded in"
        );

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_slug_is_tmux_safe_with_hash() {
        let s = workspace_slug("/Users/clawd/work/My Project");
        assert!(s.starts_with("my-project-"), "got {s}");
        assert_eq!(
            s.len(),
            "my-project-".len() + 6,
            "basename + 6-char hash: {s}"
        );

        // Different paths, same basename → different slugs.
        let a = workspace_slug("/a/demo");
        let b = workspace_slug("/b/demo");
        assert_ne!(a, b, "same basename must get different hashes: {a} vs {b}");

        // No ':' or '.' (illegal in tmux session names).
        let s = workspace_slug("/a/b.c:d");
        assert!(!s.contains(':') && !s.contains('.'), "got {s}");

        // Empty / root paths.
        let r = workspace_slug("/");
        assert!(r.starts_with("root-"), "got {r}");
    }

    #[test]
    fn cloned_room_paths_are_isolated_under_tmm_rooms() {
        let dir = std::env::temp_dir().join(format!("teamtest-paths-{}", std::process::id()));
        let ws = dir.join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let ws_s = ws.to_str().unwrap();
        let primary_room = workspace_slug(ws_s);
        let primary = Paths::for_room(ws_s, &primary_room);
        assert_eq!(primary.bin, ws.join(".tmm").join("bin"));

        let clone = Paths::for_room(ws_s, "proj-clone");
        assert_eq!(
            clone.bin,
            ws.join(".tmm").join("rooms").join("proj-clone").join("bin")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_quote_plain_passthrough() {
        assert_eq!(shell_quote("kiro-cli"), "kiro-cli");
        assert_eq!(shell_quote("a/b_c.d"), "a/b_c.d");
    }

    #[test]
    fn shell_quote_wraps_spaces_and_unicode() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("你是「经理」"), "'你是「经理」'");
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn prepare_claude_omits_model_when_blank() {
        let dir = std::env::temp_dir().join(format!("teamtest-claude-{}", std::process::id()));
        let paths = Paths::new(dir.to_str().unwrap());
        let extras = Extras {
            env: vec![],
            mcp: vec![],
            skills: vec![],
        };
        let (_, cmd, _) = prepare_claude(
            "coder",
            "coder",
            "ship code",
            "",
            &cfg(),
            "room",
            &paths,
            None,
            &extras,
        )
        .unwrap();
        assert!(!cmd.contains("--model"), "cmd: {cmd}");
        assert!(
            cmd.contains("--permission-mode bypassPermissions"),
            "cmd: {cmd}"
        );
        assert!(cmd.contains("--dangerously-skip-permissions"), "cmd: {cmd}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_claude_uses_explicit_model() {
        let dir =
            std::env::temp_dir().join(format!("teamtest-claude-model-{}", std::process::id()));
        let paths = Paths::new(dir.to_str().unwrap());
        let extras = Extras {
            env: vec![],
            mcp: vec![],
            skills: vec![],
        };
        let (_, cmd, _) = prepare_claude(
            "coder",
            "coder",
            "ship code",
            "",
            &cfg(),
            "room",
            &paths,
            Some("opus"),
            &extras,
        )
        .unwrap();
        assert!(cmd.contains("--model opus"), "cmd: {cmd}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_codex_uses_user_home_auth_and_cli_overrides() {
        let dir = std::env::temp_dir().join(format!("teamtest-codex-{}", std::process::id()));
        let paths = Paths::new(dir.to_str().unwrap());
        let extras = Extras {
            env: vec![],
            mcp: vec![McpDef {
                name: "context7".into(),
                url: Some("https://mcp.context7.com/mcp".into()),
                ..Default::default()
            }],
            skills: vec![],
        };
        let (env, cmd, post_keys) = prepare_codex(
            "analyst",
            "analyst",
            "think",
            "",
            &cfg(),
            "room",
            &paths,
            Some("gpt-5"),
            &extras,
        )
        .unwrap();
        assert!(
            env.iter().all(|(k, _)| k != "CODEX_HOME"),
            "must inherit ~/.codex auth: {env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "PATH" && v.contains(".tmm/bin")),
            "team helper bin must be on PATH: {env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "TEAM_AGENT_ROLE" && v == "analyst"),
            "role env missing: {env:?}"
        );
        assert!(cmd.contains("--model gpt-5"), "cmd: {cmd}");
        assert!(
            cmd.contains("--enable non_prefixed_mcp_tool_names"),
            "cmd: {cmd}"
        );
        assert!(cmd.contains("--no-alt-screen"), "cmd: {cmd}");
        assert!(
            !cmd.contains("--ask-for-approval"),
            "bypass mode conflicts with approval flags: {cmd}"
        );
        assert!(
            !cmd.contains("--sandbox"),
            "bypass mode conflicts with sandbox flags: {cmd}"
        );
        assert!(cmd.contains("--cd"), "cmd: {cmd}");
        assert!(
            cmd.contains("--dangerously-bypass-approvals-and-sandbox"),
            "cmd: {cmd}"
        );
        assert!(cmd.contains("TEAM_INITIAL_PROMPT_FILE"), "cmd: {cmd}");
        assert!(cmd.contains("\"$prompt\""), "cmd: {cmd}");
        assert!(post_keys.is_empty(), "codex prompt is passed at startup");
        assert!(
            cmd.contains("mcp_servers.team.http_headers.x-agent"),
            "cmd: {cmd}"
        );
        assert!(
            cmd.contains("mcp_servers.team.http_headers.x-agent-role"),
            "cmd: {cmd}"
        );
        assert!(cmd.contains("mcp_servers.context7.url"), "cmd: {cmd}");
        assert!(
            !cmd.contains("think"),
            "prompt should stay out of the persisted launch command: {cmd}"
        );
        let prompt_file = env
            .iter()
            .find(|(k, _)| k == "TEAM_INITIAL_PROMPT_FILE")
            .map(|(_, v)| v)
            .expect("prompt file env missing");
        let prompt = std::fs::read_to_string(prompt_file).unwrap();
        assert!(prompt.contains("Goal: think"), "prompt: {prompt}");
        assert!(
            prompt.contains("team-wait 50000"),
            "fallback missing: {prompt}"
        );
        let config =
            std::fs::read_to_string(paths.codex.join("analyst").join("config.toml")).unwrap();
        assert!(config.contains("model = \"gpt-5\""), "config: {config}");
        assert!(
            config.contains("\"x-agent\" = \"analyst\""),
            "config: {config}"
        );
        assert!(
            config.contains("\"x-agent-role\" = \"analyst\""),
            "config: {config}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconnect_nudge_uses_team_wait_fallback() {
        assert!(RECONNECT_NUDGE.contains("team MCP `wait`"));
        assert!(RECONNECT_NUDGE.contains("./.tmm/bin/team-wait 50000"));
        assert!(RECONNECT_NUDGE.contains("Never run bare Bash `wait`"));
    }

    #[test]
    fn launched_agent_replacement_waits_for_join_grace() {
        assert!(!launched_agent_needs_replacement(
            "requested",
            1_000,
            1_000 + STARTUP_JOIN_GRACE_MS - 1
        ));
        assert!(launched_agent_needs_replacement(
            "requested",
            1_000,
            1_000 + STARTUP_JOIN_GRACE_MS
        ));
        assert!(launched_agent_needs_replacement(
            "active",
            1_000,
            1_000 + STARTUP_JOIN_GRACE_MS
        ));
        assert!(!launched_agent_needs_replacement(
            "disabled",
            1_000,
            1_000 + STARTUP_JOIN_GRACE_MS
        ));
    }

    #[test]
    fn build_agent_prompt_structure() {
        let cfg = TeamConfig {
            url: String::new(),
            model: String::new(),
            team_rules: "Rule one.\nRule two.".into(),
            team_kick: "kick".into(),
        };
        let p = build_agent_prompt("architect", "Design the system.", "Blog style.", &cfg);
        assert!(p.contains("<team-system-prompt>"));
        assert!(p.contains("</team-system-prompt>"));
        assert!(p.contains("<role-system-prompt>"));
        assert!(p.contains("Rule one."));
        assert!(p.contains("Blog style."));
        assert!(p.contains("team-wait 50000"));
        assert!(p.contains("team-post 'message'"));
        assert!(p.contains("Never run bare Bash `wait`"));
        assert!(p.contains("architect"));
        assert!(p.contains("Design the system."));
    }

    #[test]
    fn prepare_home_creates_gitignore() {
        let dir = std::env::temp_dir().join(format!("teamtest-home-{}", std::process::id()));
        let ws = dir.join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let paths = Paths::new(ws.to_str().unwrap());
        prepare_home(&paths).unwrap();
        let gi = ws.join(".tmm").join(".gitignore");
        assert!(gi.exists());
        assert_eq!(std::fs::read_to_string(&gi).unwrap(), "*\n");
        assert!(paths.keepalive.exists());
        assert!(paths.heartbeat.exists());
        assert!(paths.launch.exists());
        assert!(paths.bin.join("team-wait").exists());
        assert!(paths.bin.join("team-post").exists());
        let team_wait = std::fs::read_to_string(paths.bin.join("team-wait")).unwrap();
        assert!(team_wait.contains("/api/agent/wait"));
        assert!(team_wait.contains("TEAM_WAIT_ROSTER:-compact"));
        assert!(team_wait.contains("roster=$roster"));
        assert!(std::fs::read_to_string(paths.bin.join("team-post"))
            .unwrap()
            .contains("/api/agent/post"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_launch_manifest_records_non_prompt_diagnostics() {
        let dir = std::env::temp_dir().join(format!("teamtest-launch-{}", std::process::id()));
        let paths = Paths::new(dir.to_str().unwrap());
        prepare_home(&paths).unwrap();
        write_launch_manifest(
            &paths,
            "manager",
            "codex",
            "tech lead",
            "room",
            "codex --no-alt-screen",
            &paths.launch.join("manager.sh"),
            2,
        );
        let text = std::fs::read_to_string(paths.launch.join("manager.json")).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["agent"], "manager");
        assert_eq!(v["backend"], "codex");
        assert_eq!(v["role"], "tech lead");
        assert_eq!(v["post_launch_steps"], 2);
        assert!(v["command"].as_str().unwrap().contains("codex"));
        assert!(v["launch_script"].as_str().unwrap().ends_with("manager.sh"));
        assert!(
            v.get("prompt").is_none(),
            "do not persist long prompts or secrets: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
