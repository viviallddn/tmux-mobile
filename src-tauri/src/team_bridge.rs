//! Desktop-only glue between the tmux-mobile WS server and the team bus.
//!
//! Multi-team manager. Each **team** is an isolated chat **room** (= the
//! workspace slug) backed by its own `agora::Bus` on the shared SQLite db. A
//! single MCP daemon serves them all (agents pick a room via the `x-room`
//! header); the phone passes the active room with each `team_*` RPC. Every
//! room's messages funnel into one re-broadcast channel (each `Message` carries
//! its `room`), and the phone filters to the team currently in view.
//!
//! Compiled ONLY on desktop (lib.rs `#[cfg(...)]`); mobile passes `None`.

use crate::server::TeamBridge;
use crate::team::{self, TeamConfig};
use agora::bus::{Bus, BusProvider};
use agora::store::SharedConn;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::broadcast;

const SESSION_PREFIX: &str = "tmm-team-";
const RECENT_WORKSPACE_LIMIT: usize = 20;

/// One live team: its bus + launch metadata.
struct Team {
    bus: Bus,
    workspace: String,
    template: String,                  // roster template this team was started from
    session: String,                   // tmm-team-<room>
    started: bool,                     // supervisor launched for this room
    pump: tokio::task::JoinHandle<()>, // the Message→JSON re-broadcast task
}

pub struct TeamManager {
    /// One shared SQLite connection for ALL rooms (WAL single-writer).
    conn: SharedConn,
    /// room -> Team. Guarded by a std Mutex (never held across .await).
    teams: Mutex<HashMap<String, Team>>,
    /// Merged message fan-out for the WS push path (all rooms).
    json_tx: broadcast::Sender<String>,
    /// Server-level launcher config (bus URL + default model).
    cfg: TeamConfig,
    /// Where room→workspace is persisted so restarts can recover teams.
    meta_path: std::path::PathBuf,
    /// Recently started workspaces, kept separate from the recovery map so
    /// closing a team does not erase the user's one-click start history.
    recent_path: std::path::PathBuf,
    self_ref: OnceLock<Weak<TeamManager>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RecentWorkspace {
    workspace: String,
    template: String,
    room: String,
    last_started: i64,
}

impl TeamManager {
    /// Open the store, start the MCP/dashboard daemon (room-aware), and recover
    /// any teams still running in tmux from a previous run.
    pub fn start(
        db: &str,
        _room: &str,
        bind: &str,
        model: &str,
    ) -> Result<Arc<TeamManager>, Box<dyn std::error::Error>> {
        // Write the built-in default roster template if the teams/ dir is empty,
        // so the user always has something to pick + edit.
        team::ensure_templates_seeded();
        if let Some(parent) = std::path::Path::new(db).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = agora::store::open_shared(db)?;
        let (json_tx, _) = broadcast::channel::<String>(1024);
        let port = bind.rsplit(':').next().unwrap_or("8787");
        let cfg = TeamConfig {
            url: format!("http://127.0.0.1:{}", port),
            model: model.to_string(),
            team_rules: crate::config::Config::load().team_rules,
            team_kick: crate::config::Config::load().team_kick,
        };
        // room→workspace map lives next to the db so a restart knows where each
        // recovered team's agents work.
        let meta_path = std::path::Path::new(db)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("teams.json");
        let recent_path = std::path::Path::new(db)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("team_workspaces.json");

        let me = Arc::new(TeamManager {
            conn,
            teams: Mutex::new(HashMap::new()),
            json_tx,
            cfg,
            meta_path,
            recent_path,
            self_ref: OnceLock::new(),
        });
        let _ = me.self_ref.set(Arc::downgrade(&me));

        // Serve MCP + dashboard for external agents. The provider is the manager
        // itself (room-aware). Bind failure is non-fatal — the phone path still
        // works in-process.
        {
            println!(
                "🜂 team manager ready (db {}); MCP + dashboard → http://{}/",
                db, bind
            );
            let provider: Arc<dyn BusProvider> = me.clone();
            let bind = bind.to_string();
            tokio::spawn(async move {
                if let Err(e) = agora::web::serve(provider, &bind).await {
                    eprintln!("⚠️  team daemon not listening on {}: {}", bind, e);
                }
            });
        }

        me.recover_running_teams();
        Ok(me)
    }

    /// On startup, find teams still alive in tmux (sessions named tmm-team-*)
    /// and resume supervising them — the server can restart without abandoning
    /// running agents. Their workspace comes from the persisted meta map.
    fn recover_running_teams(self: &Arc<Self>) {
        let meta = self.load_meta();
        for session in crate::tmux::list_team_sessions(SESSION_PREFIX) {
            let room = match session.strip_prefix(SESSION_PREFIX) {
                Some(r) if !r.is_empty() => r.to_string(),
                _ => continue,
            };
            let (workspace, template) = meta
                .get(&room)
                .map(|(w, t)| (w.clone(), t.clone()))
                .unwrap_or_default();
            if self.ensure_room(&room, &workspace, &template).is_err() {
                continue;
            }
            self.remember_workspace(&workspace, &template, &room);
            // Mark started + relaunch the reconcile loop, which ADOPTS the
            // existing agent windows (preserving each agent's conversation
            // context + in-flight work) rather than reopening them.
            if let Some(t) = self.teams.lock().unwrap().get_mut(&room) {
                t.started = true;
            }
            if let Some(arc) = self.self_ref.get().and_then(|w| w.upgrade()) {
                let bridge: Arc<dyn TeamBridge> = arc;
                team::start(bridge, self.cfg.clone(), room.clone(), workspace, template);
                // The adopted agents are hung on a `wait` whose connection died
                // with the old daemon; their MCP clients reconnect once unstuck
                // but can't unstick themselves. Nudge each window to reconnect
                // (Esc the dead call + re-prompt). One-shot, off the hot path.
                team::nudge_session_agents(session.clone());
                println!(
                    "🜂 team: recovered running team '{}' (adopting + nudging agents)",
                    room
                );
            }
        }
    }

    /// room → (workspace, template), persisted to teams.json.
    fn load_meta(&self) -> HashMap<String, (String, String)> {
        let v: serde_json::Value = std::fs::read_to_string(&self.meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        let mut map = HashMap::new();
        if let Some(obj) = v.as_object() {
            for (room, entry) in obj {
                let ws = entry
                    .get("workspace")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let tpl = entry
                    .get("template")
                    .and_then(|x| x.as_str())
                    .unwrap_or("default")
                    .to_string();
                map.insert(room.clone(), (ws, tpl));
            }
        }
        map
    }

    /// Persist room→{workspace,template} for every known team (best-effort).
    fn save_meta(&self) {
        let map: serde_json::Map<String, serde_json::Value> = self
            .teams
            .lock()
            .unwrap()
            .iter()
            .map(|(room, t)| {
                (
                    room.clone(),
                    serde_json::json!({ "workspace": t.workspace, "template": t.template }),
                )
            })
            .collect();
        if let Ok(s) = serde_json::to_string_pretty(&serde_json::Value::Object(map)) {
            let _ = std::fs::write(&self.meta_path, s);
        }
    }

    fn load_recent_workspaces(&self) -> Vec<RecentWorkspace> {
        let v: serde_json::Value = std::fs::read_to_string(&self.recent_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        let arr = if let Some(arr) = v.get("recent_workspaces").and_then(|x| x.as_array()) {
            arr.clone()
        } else if let Some(arr) = v.as_array() {
            arr.clone()
        } else {
            Vec::new()
        };
        let mut recent = Vec::new();
        for item in arr {
            let workspace = item
                .get("workspace")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if workspace.is_empty() {
                continue;
            }
            let template = item
                .get("template")
                .and_then(|x| x.as_str())
                .unwrap_or("default")
                .trim();
            let room = item
                .get("room")
                .and_then(|x| x.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| team::workspace_slug(&workspace));
            recent.push(RecentWorkspace {
                workspace,
                template: if template.is_empty() {
                    "default".to_string()
                } else {
                    template.to_string()
                },
                room,
                last_started: item
                    .get("last_started")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0),
            });
        }
        recent.sort_by(|a, b| b.last_started.cmp(&a.last_started));
        recent.truncate(RECENT_WORKSPACE_LIMIT);
        recent
    }

    fn save_recent_workspaces(&self, recent: &[RecentWorkspace]) {
        let payload = serde_json::json!({ "recent_workspaces": recent });
        if let Ok(s) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(&self.recent_path, s);
        }
    }

    fn remember_workspace(&self, workspace: &str, template: &str, room: &str) {
        let ws = workspace.trim();
        if ws.is_empty() {
            return;
        }
        let tpl = if template.trim().is_empty() {
            "default"
        } else {
            template.trim()
        };
        let mut recent = self.load_recent_workspaces();
        recent.retain(|item| item.workspace != ws);
        recent.insert(
            0,
            RecentWorkspace {
                workspace: ws.to_string(),
                template: tpl.to_string(),
                room: room.to_string(),
                last_started: chrono_ms(),
            },
        );
        recent.truncate(RECENT_WORKSPACE_LIMIT);
        self.save_recent_workspaces(&recent);
    }

    /// Get an existing room's bus, or register it over the shared connection and
    /// start its re-broadcast pump. `workspace`/`template` recorded on first open.
    fn ensure_room(&self, room: &str, workspace: &str, template: &str) -> Result<Bus, String> {
        {
            let teams = self.teams.lock().unwrap();
            if let Some(t) = teams.get(room) {
                return Ok(t.bus.clone());
            }
        }
        // All rooms share ONE connection (no per-room write contention).
        let bus = Bus::with_shared(self.conn.clone(), room.to_string());
        // Pump this room's messages into the merged push channel. The handle is
        // stored on the Team so close_team can abort it (otherwise reopening the
        // same room would spawn a second pump → duplicate pushes).
        let pump = {
            let mut rx = bus.subscribe();
            let tx = self.json_tx.clone();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(m) => {
                            if let Ok(s) = serde_json::to_string(&m) {
                                let _ = tx.send(s);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            })
        };
        {
            let mut teams = self.teams.lock().unwrap();
            // Double-checked: another thread may have inserted while we built.
            if let Some(t) = teams.get(room) {
                pump.abort(); // discard the duplicate pump we just spawned
                return Ok(t.bus.clone());
            }
            teams.insert(
                room.to_string(),
                Team {
                    bus: bus.clone(),
                    workspace: workspace.to_string(),
                    template: if template.is_empty() {
                        "default".to_string()
                    } else {
                        template.to_string()
                    },
                    session: format!("{}{}", SESSION_PREFIX, room),
                    started: false,
                    pump,
                },
            );
        }
        self.save_meta();
        Ok(bus)
    }

    /// Bus for a known room (no creation). None if the room isn't registered.
    fn room_bus(&self, room: &str) -> Option<Bus> {
        self.teams.lock().unwrap().get(room).map(|t| t.bus.clone())
    }

    fn next_clone_room(&self, source_room: &str) -> String {
        let base = format!("{}-clone", source_room.trim_matches('-'));
        let teams = self.teams.lock().unwrap();
        for n in 1..1000 {
            let room = if n == 1 {
                base.clone()
            } else {
                format!("{}-{}", base, n)
            };
            let session = format!("{}{}", SESSION_PREFIX, room);
            if !teams.contains_key(&room) && !crate::tmux::session_exists(&session) {
                return room;
            }
        }
        format!("{}-{}", base, chrono_ms())
    }
}

fn chrono_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// The manager IS the MCP/web room provider: agents' `x-room` header selects a
// team. We only serve rooms that already exist (a team must be started first),
// so an agent can't spin up a phantom room by guessing a header.
impl BusProvider for TeamManager {
    fn bus_for(&self, room: &str) -> Option<Bus> {
        self.room_bus(room)
    }
    fn default_room(&self) -> String {
        // First registered team, else "main" (harmless; bus_for returns None
        // until a team exists).
        self.teams
            .lock()
            .unwrap()
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "main".to_string())
    }
}

impl TeamBridge for TeamManager {
    fn history(&self, room: &str, limit: i64) -> serde_json::Value {
        let msgs = self
            .room_bus(room)
            .and_then(|b| b.history(limit).ok())
            .unwrap_or_default();
        serde_json::json!({ "messages": msgs })
    }

    fn roster(&self, room: &str) -> serde_json::Value {
        let roster = self
            .room_bus(room)
            .and_then(|b| b.roster().ok())
            .unwrap_or_default();
        serde_json::json!({ "roster": roster })
    }

    fn post(
        &self,
        room: &str,
        from: &str,
        body: &str,
        requires_reply: bool,
    ) -> Result<serde_json::Value, String> {
        let bus = self
            .room_bus(room)
            .ok_or_else(|| format!("unknown team '{room}'"))?;
        bus.post(from, body, requires_reply)
            .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
            .map_err(|e| e.to_string())
    }

    fn set_agent_status(&self, room: &str, agent: &str, status: &str) -> Result<(), String> {
        let bus = self
            .room_bus(room)
            .ok_or_else(|| format!("unknown team '{room}'"))?;
        bus.set_status(agent, status).map_err(|e| e.to_string())
    }

    fn employees(&self, room: &str) -> serde_json::Value {
        let employees = self
            .room_bus(room)
            .and_then(|b| b.employees().ok())
            .unwrap_or_default();
        serde_json::json!({ "employees": employees })
    }

    fn usage(&self, room: &str) -> serde_json::Value {
        let usage = self
            .room_bus(room)
            .and_then(|b| b.usage().ok())
            .unwrap_or_default();
        serde_json::json!({ "usage": usage })
    }

    fn seed_employee(
        &self,
        room: &str,
        name: &str,
        spec: &serde_json::Value,
    ) -> Result<(), String> {
        let bus = self
            .room_bus(room)
            .ok_or_else(|| format!("unknown team '{room}'"))?;
        bus.seed_employee(name, spec).map_err(|e| e.to_string())
    }

    fn room_exists(&self, room: &str) -> bool {
        self.teams.lock().unwrap().contains_key(room)
    }

    fn employee_specs(&self, room: &str) -> Vec<(String, serde_json::Value, String)> {
        self.room_bus(room)
            .and_then(|b| b.employees().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.name, e.spec, e.state))
            .collect()
    }

    fn start_team(&self, workspace: &str, template: &str) -> serde_json::Value {
        let ws = if workspace.trim().is_empty() {
            self.default_workspace()
        } else {
            workspace.trim().to_string()
        };
        let tpl = if template.trim().is_empty() {
            "default".to_string()
        } else {
            template.trim().to_string()
        };
        let room = team::workspace_slug(&ws);

        // Self-heal the built-in templates (the teams/ dir may have been deleted)
        // and refuse up front if the chosen roster is missing/empty — otherwise
        // the team would "start" with zero agents and the UI would spin forever.
        team::ensure_templates_seeded();
        if team::read_template(&tpl).is_empty() {
            return serde_json::json!({ "started": false, "room": room, "workspace": ws,
                "error": format!("template '{}' not found or empty", tpl) });
        }

        // Open/register the room, then mark it started (one-shot per room).
        if let Err(e) = self.ensure_room(&room, &ws, &tpl) {
            return serde_json::json!({ "started": false, "room": room, "workspace": ws, "error": e });
        }
        let already = {
            let mut teams = self.teams.lock().unwrap();
            match teams.get_mut(&room) {
                Some(t) => {
                    let was = t.started;
                    t.started = true;
                    if !was {
                        t.template = tpl.clone();
                    }
                    was
                }
                None => false,
            }
        };
        if already {
            return serde_json::json!({ "started": false, "room": room, "workspace": ws });
        }
        // Authoritative fresh start: forget any leftover roster/log for this room
        // so the CHOSEN template — not stale DB rows from a previous team on the
        // same workspace — defines who comes online. Recovery uses a different
        // path and never resets, so a backend restart still adopts a live team.
        if let Some(bus) = self.room_bus(&room) {
            if let Err(e) = bus.reset_room() {
                eprintln!("⚠️  team: reset room '{}' failed: {}", room, e);
            }
        }
        self.save_meta();
        self.remember_workspace(&ws, &tpl, &room);
        match self.self_ref.get().and_then(|w| w.upgrade()) {
            Some(arc) => {
                let bridge: Arc<dyn TeamBridge> = arc;
                team::start(
                    bridge,
                    self.cfg.clone(),
                    room.clone(),
                    ws.clone(),
                    tpl.clone(),
                );
                serde_json::json!({ "started": true, "room": room, "workspace": ws, "template": tpl })
            }
            None => {
                if let Some(t) = self.teams.lock().unwrap().get_mut(&room) {
                    t.started = false;
                }
                serde_json::json!({ "started": false, "room": room, "workspace": ws, "error": "manager gone" })
            }
        }
    }

    fn clone_team(&self, room: &str) -> serde_json::Value {
        let (workspace, template) = {
            let teams = self.teams.lock().unwrap();
            match teams.get(room) {
                Some(t) => (t.workspace.clone(), t.template.clone()),
                None => {
                    return serde_json::json!({
                        "started": false,
                        "cloned_from": room,
                        "error": format!("unknown team '{}'", room),
                    });
                }
            }
        };
        let source_specs = self.employee_specs(room);
        if source_specs.is_empty() && team::read_template(&template).is_empty() {
            return serde_json::json!({
                "started": false,
                "cloned_from": room,
                "workspace": workspace,
                "template": template,
                "error": "source team has no roster to clone",
            });
        }

        let new_room = self.next_clone_room(room);
        if let Err(e) = self.ensure_room(&new_room, &workspace, &template) {
            return serde_json::json!({
                "started": false,
                "room": new_room,
                "cloned_from": room,
                "workspace": workspace,
                "template": template,
                "error": e,
            });
        }
        if let Some(bus) = self.room_bus(&new_room) {
            if let Err(e) = bus.reset_room() {
                eprintln!("⚠️  team: reset clone room '{}' failed: {}", new_room, e);
            }
        }

        let mut seeded = 0usize;
        let mut seed_errors = Vec::new();
        for (name, spec, state) in &source_specs {
            if state == "disabled" {
                continue;
            }
            match self.seed_employee(&new_room, name, spec) {
                Ok(()) => seeded += 1,
                Err(e) => seed_errors.push(format!("{}: {}", name, e)),
            }
        }
        if !source_specs.is_empty() && seeded == 0 {
            let _ = self.close_team(&new_room);
            return serde_json::json!({
                "started": false,
                "room": new_room,
                "cloned_from": room,
                "workspace": workspace,
                "template": template,
                "error": format!("failed to clone roster: {}", seed_errors.join("; ")),
            });
        }

        {
            let mut teams = self.teams.lock().unwrap();
            if let Some(t) = teams.get_mut(&new_room) {
                t.started = true;
            }
        }
        self.save_meta();
        self.remember_workspace(&workspace, &template, &new_room);

        match self.self_ref.get().and_then(|w| w.upgrade()) {
            Some(arc) => {
                let bridge: Arc<dyn TeamBridge> = arc;
                team::start(
                    bridge,
                    self.cfg.clone(),
                    new_room.clone(),
                    workspace.clone(),
                    template.clone(),
                );
                serde_json::json!({
                    "started": true,
                    "room": new_room,
                    "cloned_from": room,
                    "workspace": workspace,
                    "template": template,
                    "agents": seeded,
                    "seed_errors": seed_errors,
                })
            }
            None => {
                if let Some(t) = self.teams.lock().unwrap().get_mut(&new_room) {
                    t.started = false;
                }
                serde_json::json!({
                    "started": false,
                    "room": new_room,
                    "cloned_from": room,
                    "workspace": workspace,
                    "template": template,
                    "error": "manager gone",
                })
            }
        }
    }

    fn close_team(&self, room: &str) -> bool {
        let removed = {
            let mut teams = self.teams.lock().unwrap();
            teams.remove(room)
        };
        match removed {
            Some(t) => {
                self.remember_workspace(&t.workspace, &t.template, room);
                // Stop the re-broadcast task (no leak / no dup on reopen).
                t.pump.abort();
                // Kill the tmux session (best-effort), then forget the team's
                // persisted state so a future start on the same workspace begins
                // clean. (A backend restart resumes a LIVE team via recovery,
                // which never goes through close — so this only forgets teams the
                // operator explicitly closed.)
                let _ = crate::tmux::kill_session(&t.session);
                let _ = t.bus.reset_room();
                self.save_meta(); // drop it from the recovery map too
                true
            }
            None => false,
        }
    }

    fn teams(&self) -> serde_json::Value {
        let teams = self.teams.lock().unwrap();
        let list: Vec<serde_json::Value> = teams
            .values()
            .map(|t| {
                let agents = t
                    .bus
                    .roster()
                    .map(|r| r.iter().filter(|a| a.status != "offline").count())
                    .unwrap_or(0);
                serde_json::json!({
                    "room": t.session.strip_prefix(SESSION_PREFIX).unwrap_or(&t.session),
                    "workspace": t.workspace,
                    "template": t.template,
                    "session": t.session,
                    "started": t.started,
                    "agents": agents,
                })
            })
            .collect();
        serde_json::json!({ "teams": list })
    }

    fn templates(&self) -> serde_json::Value {
        serde_json::json!({ "templates": team::read_all_templates() })
    }

    fn save_template(&self, name: &str, agents: &serde_json::Value) -> Result<(), String> {
        team::save_template(name, agents)
    }

    fn delete_template(&self, name: &str) -> Result<(), String> {
        team::delete_template(name)
    }

    fn system_prompt(&self) -> String {
        team::read_system_prompt()
    }

    fn save_system_prompt(&self, text: &str) -> Result<(), String> {
        team::save_system_prompt(text)
    }

    fn default_workspace(&self) -> String {
        dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
    }

    fn recent_workspaces(&self) -> serde_json::Value {
        serde_json::json!({ "recent_workspaces": self.load_recent_workspaces() })
    }

    fn subscribe(&self) -> broadcast::Receiver<String> {
        self.json_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::TeamBridge;

    /// Build a manager backed by a temp db, WITHOUT the network daemon (we only
    /// exercise the room registry + bus routing here).
    fn manager() -> Arc<TeamManager> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teamtest-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        let db = dir.join("team.db").to_string_lossy().into_owned();
        let conn = agora::store::open_shared(&db).unwrap();
        let (json_tx, _) = broadcast::channel::<String>(64);
        let me = Arc::new(TeamManager {
            conn,
            teams: Mutex::new(HashMap::new()),
            json_tx,
            cfg: TeamConfig {
                url: "http://127.0.0.1:0".into(),
                model: "m".into(),
                team_rules: String::new(),
                team_kick: String::new(),
            },
            meta_path: dir.join("teams.json"),
            recent_path: dir.join("team_workspaces.json"),
            self_ref: OnceLock::new(),
        });
        let _ = me.self_ref.set(Arc::downgrade(&me));
        me
    }

    #[tokio::test]
    async fn unknown_room_is_refused_until_registered() {
        let m = manager();
        // No room yet → bus_for/post refuse.
        assert!(m.room_bus("alpha").is_none());
        assert!(m.post("alpha", "human", "hi", false).is_err());
        // Registering the room opens it.
        m.ensure_room("alpha", "/tmp/alpha", "default").unwrap();
        assert!(m.room_bus("alpha").is_some());
        assert!(m.post("alpha", "human", "hi", false).is_ok());
    }

    #[tokio::test]
    async fn rooms_are_isolated() {
        let m = manager();
        m.ensure_room("alpha", "/tmp/alpha", "default").unwrap();
        m.ensure_room("beta", "/tmp/beta", "default").unwrap();
        m.post("alpha", "human", "hello alpha", false).unwrap();
        m.post("beta", "human", "hello beta", false).unwrap();

        let a = m.history("alpha", 100);
        let b = m.history("beta", 100);
        let a_bodies: Vec<String> = a["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["body"].as_str().unwrap_or("").to_string())
            .collect();
        let b_bodies: Vec<String> = b["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["body"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(a_bodies.iter().any(|s| s == "hello alpha"));
        assert!(
            !a_bodies.iter().any(|s| s == "hello beta"),
            "beta leaked into alpha"
        );
        assert!(b_bodies.iter().any(|s| s == "hello beta"));
        assert!(
            !b_bodies.iter().any(|s| s == "hello alpha"),
            "alpha leaked into beta"
        );
    }

    #[tokio::test]
    async fn reset_room_forgets_roster_and_log() {
        // The fix for stale-state shadowing: a (re)start/close resets the room so
        // a previously-seeded roster + chat log can't carry over.
        let m = manager();
        m.ensure_room("alpha", "/tmp/alpha", "default").unwrap();
        let bus = m.room_bus("alpha").unwrap();
        bus.seed_employee("manager", &serde_json::json!({ "role": "manager" }))
            .unwrap();
        m.post("alpha", "human", "hello", false).unwrap();
        assert!(!m.employees("alpha")["employees"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(!m.history("alpha", 100)["messages"]
            .as_array()
            .unwrap()
            .is_empty());

        bus.reset_room().unwrap();
        assert!(
            m.employees("alpha")["employees"]
                .as_array()
                .unwrap()
                .is_empty(),
            "employees cleared"
        );
        assert!(
            m.history("alpha", 100)["messages"]
                .as_array()
                .unwrap()
                .is_empty(),
            "log cleared"
        );
    }

    #[tokio::test]
    async fn teams_lists_registered_rooms() {
        let m = manager();
        m.ensure_room("alpha", "/tmp/alpha", "default").unwrap();
        let teams = m.teams();
        let arr = teams["teams"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["room"], "alpha");
        assert_eq!(arr[0]["workspace"], "/tmp/alpha");
    }

    #[tokio::test]
    async fn recent_workspaces_survive_close() {
        let m = manager();
        m.ensure_room("alpha", "/tmp/alpha", "software-dev")
            .unwrap();
        m.remember_workspace("/tmp/alpha", "software-dev", "alpha");
        assert_eq!(m.close_team("alpha"), true);

        let recent = m.recent_workspaces();
        let arr = recent["recent_workspaces"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["workspace"], "/tmp/alpha");
        assert_eq!(arr[0]["template"], "software-dev");
        assert_eq!(arr[0]["room"], "alpha");
    }
}
