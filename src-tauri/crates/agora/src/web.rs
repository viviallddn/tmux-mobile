//! The web layer: a single axum app that serves both the MCP endpoint (for agents)
//! and a live dashboard (for humans), backed by the same [`Bus`].
//!
//! Routes:
//! - `POST /mcp`            — MCP over Streamable HTTP (agents connect here)
//! - `GET  /`               — dashboard HTML
//! - `GET  /events`         — Server-Sent Events: the live message stream
//! - `POST /api/post`       — human posts a message (the human is a chat participant)
//! - `GET  /api/roster`     — roster + presence
//! - `GET  /api/quiescence` — done / deadlock / active classification
//! - `GET  /api/history`    — recent messages
//! - `POST /api/usage`      — agent token-usage snapshot ingest

use crate::bus::{Bus, WaitOutcome};
use crate::mcp::AgoraMcp;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
struct AppState {
    provider: Arc<dyn crate::bus::BusProvider>,
}

impl AppState {
    /// Resolve the bus for a `?room=` query (or the provider default).
    fn bus(&self, room: &Option<String>) -> Option<Bus> {
        let room = room.clone().unwrap_or_else(|| self.provider.default_room());
        self.provider.bus_for(&room)
    }
}

/// Build the full axum router (MCP + dashboard) for the given bus provider.
/// Each request is routed to a room: agents via the `x-room` header, the human
/// API via a `?room=` query (both default to the provider's default room).
pub fn router(provider: Arc<dyn crate::bus::BusProvider>) -> Router {
    // Stateless mode: each POST is handled on its own, with NO server-side
    // session id and NO mandatory `initialize` handshake. This is essential for
    // the in-process daemon: it lives in the desktop server process, so every
    // backend restart wipes any in-memory session map. With stateful sessions, a
    // client still presenting its OLD `Mcp-Session-Id` after a restart is
    // rejected (`401 Session not found`; a no-session request gets `422 expect
    // initialize`) and rmcp does not auto-re-handshake — the agent hangs on
    // `wait` forever (roster goes offline → the UI spins "coming online"
    // indefinitely). Our tool surface is genuinely stateless: identity is
    // resolved per-request from the `x-agent`/`x-room` headers and all state
    // lives in SQLite, and the agent loop is pure request/response (`post`/
    // `wait`), needing no server→client push. So statelessness costs us nothing
    // and lets any fresh request work with no init — which is what lets a nudged
    // agent reconnect after a restart (see team_bridge::recover_running_teams
    // and team::nudge_session_agents).
    let mcp_service = StreamableHttpService::new(
        {
            let provider = provider.clone();
            move || Ok(AgoraMcp::new(provider.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: false,
            ..Default::default()
        },
    );

    Router::new()
        .route("/", get(dashboard))
        .route("/events", get(sse))
        .route("/api/post", post(api_post))
        .route("/api/roster", get(api_roster))
        .route("/api/quiescence", get(api_quiescence))
        .route("/api/history", get(api_history))
        .route("/api/employees", get(api_employees).post(api_seed_employee))
        .route("/api/heartbeat", post(api_heartbeat))
        .route("/api/usage", post(api_usage))
        .route("/api/agent/wait", get(api_agent_wait))
        .route("/api/agent/post", post(api_agent_post))
        .nest_service("/mcp", mcp_service)
        .with_state(AppState { provider })
}

/// Convenience for a single-room deployment (e.g. tests): wrap one `Bus`.
pub fn router_single(bus: Bus) -> Router {
    router(Arc::new(crate::bus::SingleRoom(bus)))
}

/// Bind `addr` and serve the full app (MCP + dashboard) until the process ends.
/// Used by the tmux-mobile desktop server to expose the in-process bus(es) to
/// external coding agents without pulling axum into the host crate.
pub async fn serve(provider: Arc<dyn crate::bus::BusProvider>, addr: &str) -> anyhow::Result<()> {
    let app = router(provider);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

const ERR_NO_ROOM: (axum::http::StatusCode, &str) =
    (axum::http::StatusCode::NOT_FOUND, "unknown room");

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// Live message stream for the dashboard. Replays recent history, then streams
/// new messages for the `?room=` room (default room if omitted).
async fn sse(
    State(st): State<AppState>,
    Query(q): Query<RoomQuery>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    // Fall back to the default room's bus if the requested room is unknown, so
    // the dashboard SSE never 500s; an empty stream is fine.
    let bus = st
        .bus(&q.room)
        .or_else(|| st.bus(&None))
        .expect("default room must exist");
    let rx = bus.subscribe();
    let history = bus.history(100).unwrap_or_default();

    let replay = tokio_stream::iter(history.into_iter().map(|m| {
        Ok(Event::default()
            .event("message")
            .data(serde_json::to_string(&m).unwrap_or_default()))
    }));

    let live = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(m) => Some(Ok(Event::default()
            .event("message")
            .data(serde_json::to_string(&m).unwrap_or_default()))),
        Err(_) => None, // lagged: dashboard re-syncs via /api/history if needed
    });

    Sse::new(replay.chain(live)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// `?room=` selector shared by the read-only GET handlers.
#[derive(Debug, Deserialize)]
struct RoomQuery {
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct PostBody {
    #[serde(default)]
    from: Option<String>,
    body: String,
    /// If omitted, inferred: a message containing an @mention requires a reply.
    #[serde(default)]
    requires_reply: Option<bool>,
    #[serde(default)]
    room: Option<String>,
}

async fn api_post(State(st): State<AppState>, Json(b): Json<PostBody>) -> axum::response::Response {
    let Some(bus) = st.bus(&b.room) else {
        return ERR_NO_ROOM.into_response();
    };
    let from = b.from.unwrap_or_else(|| "human".to_string());
    let rr = b.requires_reply.unwrap_or_else(|| b.body.contains('@'));
    match bus.post(&from, &b.body, rr) {
        Ok(m) => Json(m).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `POST /api/heartbeat` — an agent's tool hook pings this each tool/prompt so a
/// heads-down working agent keeps a fresh `last_seen` instead of decaying to
/// `unreachable`. Identity comes from the same headers agents send to `/mcp`:
/// `x-agent` (who) + `x-room` (which team). Best-effort: always 204.
async fn api_heartbeat(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let Ok((agent, room, role)) = agent_identity(&headers) else {
        return (axum::http::StatusCode::BAD_REQUEST, "missing x-agent").into_response();
    };
    let Some(bus) = st.bus(&room) else {
        return ERR_NO_ROOM.into_response();
    };
    let _ = bus.heartbeat(&agent, role.as_deref());
    axum::http::StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/usage` — best-effort token usage ingest from per-agent hooks.
/// Identity uses the same headers as `/mcp` and `/api/heartbeat`.
async fn api_usage(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let Ok((agent, room, _role)) = agent_identity(&headers) else {
        return (axum::http::StatusCode::BAD_REQUEST, "missing x-agent").into_response();
    };
    let Some(bus) = st.bus(&room) else {
        return ERR_NO_ROOM.into_response();
    };
    match bus.record_usage(&agent, &body) {
        Ok(_) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct AgentWaitQuery {
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// `full` preserves the original WaitOutcome shape. Shell fallback agents
    /// use `compact` to avoid dumping full AgentRow metadata into every CLI
    /// transcript; `none` omits roster entirely.
    #[serde(default)]
    roster: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitRosterMode {
    Full,
    Compact,
    None,
}

impl WaitRosterMode {
    fn parse(raw: Option<&str>) -> Self {
        match raw.unwrap_or("full").to_ascii_lowercase().as_str() {
            "compact" => WaitRosterMode::Compact,
            "none" | "off" | "false" | "0" => WaitRosterMode::None,
            _ => WaitRosterMode::Full,
        }
    }
}

fn agent_identity(
    headers: &axum::http::HeaderMap,
) -> Result<(String, Option<String>, Option<String>), axum::response::Response> {
    let hdr = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
    };
    let agent = hdr("x-agent")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| (axum::http::StatusCode::BAD_REQUEST, "missing x-agent").into_response())?;
    let room = hdr("x-room").filter(|s| !s.is_empty());
    let role = hdr("x-agent-role").filter(|s| !s.is_empty());
    Ok((agent, room, role))
}

/// Shell-friendly fallback for agent CLIs whose MCP tools are not surfaced to
/// the model. Identity comes from headers, matching `/mcp`.
async fn api_agent_wait(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<AgentWaitQuery>,
) -> axum::response::Response {
    let Ok((agent, room, role)) = agent_identity(&headers) else {
        return (axum::http::StatusCode::BAD_REQUEST, "missing x-agent").into_response();
    };
    let Some(bus) = st.bus(&room) else {
        return ERR_NO_ROOM.into_response();
    };
    if let Err(e) = bus.join(&agent, role.as_deref()) {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    match bus
        .wait(&agent, q.timeout_ms.map(Duration::from_millis))
        .await
    {
        Ok(outcome) => Json(wait_outcome_json(
            outcome,
            WaitRosterMode::parse(q.roster.as_deref()),
        ))
        .into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn wait_outcome_json(outcome: WaitOutcome, roster_mode: WaitRosterMode) -> serde_json::Value {
    match outcome {
        WaitOutcome::Delivered {
            messages,
            roster,
            cursor,
        } => {
            let mut out = serde_json::json!({
                "outcome": "delivered",
                "messages": messages,
                "cursor": cursor,
            });
            apply_roster_mode(&mut out, roster, roster_mode);
            out
        }
        WaitOutcome::Idle { roster, cursor } => {
            let mut out = serde_json::json!({
                "outcome": "idle",
                "cursor": cursor,
            });
            apply_roster_mode(&mut out, roster, roster_mode);
            out
        }
        WaitOutcome::Blocked { you_owe, pending } => serde_json::json!({
            "outcome": "blocked",
            "you_owe": you_owe,
            "pending": pending,
        }),
    }
}

fn apply_roster_mode(
    out: &mut serde_json::Value,
    roster: Vec<crate::store::AgentRow>,
    roster_mode: WaitRosterMode,
) {
    match roster_mode {
        WaitRosterMode::Full => {
            out["roster"] = serde_json::to_value(roster).unwrap_or_else(|_| serde_json::json!([]));
        }
        WaitRosterMode::Compact => {
            out["roster"] = serde_json::Value::Array(
                roster
                    .into_iter()
                    .filter(|a| a.status != "offline")
                    .map(|a| serde_json::json!({ "name": a.name, "status": a.status }))
                    .collect(),
            );
        }
        WaitRosterMode::None => {}
    }
}

#[derive(Debug, Deserialize)]
struct AgentPostQuery {
    #[serde(default)]
    requires_reply: Option<bool>,
}

/// Shell-friendly fallback for posting to the team bus. The raw request body is
/// the chat message, so callers don't need a JSON encoder.
async fn api_agent_post(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<AgentPostQuery>,
    body: String,
) -> axum::response::Response {
    let Ok((agent, room, role)) = agent_identity(&headers) else {
        return (axum::http::StatusCode::BAD_REQUEST, "missing x-agent").into_response();
    };
    let Some(bus) = st.bus(&room) else {
        return ERR_NO_ROOM.into_response();
    };
    if let Err(e) = bus.join(&agent, role.as_deref()) {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let requires_reply = q.requires_reply.unwrap_or_else(|| body.contains('@'));
    match bus.post(&agent, &body, requires_reply) {
        Ok(m) => Json(m).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn api_roster(
    State(st): State<AppState>,
    Query(q): Query<RoomQuery>,
) -> axum::response::Response {
    let Some(bus) = st.bus(&q.room) else {
        return ERR_NO_ROOM.into_response();
    };
    match bus.roster() {
        Ok(r) => Json(r).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn api_quiescence(
    State(st): State<AppState>,
    Query(q): Query<RoomQuery>,
) -> axum::response::Response {
    let Some(bus) = st.bus(&q.room) else {
        return ERR_NO_ROOM.into_response();
    };
    match bus.quiescence() {
        Ok(qx) => Json(qx).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn api_employees(
    State(st): State<AppState>,
    Query(q): Query<RoomQuery>,
) -> axum::response::Response {
    let Some(bus) = st.bus(&q.room) else {
        return ERR_NO_ROOM.into_response();
    };
    match bus.employees() {
        Ok(e) => Json(e).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SeedBody {
    name: String,
    #[serde(default)]
    spec: serde_json::Value,
    #[serde(default)]
    room: Option<String>,
}

/// Seed the desired roster with an employee (the dashboard / external HTTP API;
/// the in-app path seeds directly via `Bus::seed_employee`).
async fn api_seed_employee(
    State(st): State<AppState>,
    Json(b): Json<SeedBody>,
) -> axum::response::Response {
    let Some(bus) = st.bus(&b.room) else {
        return ERR_NO_ROOM.into_response();
    };
    match bus.seed_employee(&b.name, &b.spec) {
        Ok(()) => Json(serde_json::json!({"ok": true, "name": b.name})).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn api_history(
    State(st): State<AppState>,
    Query(q): Query<RoomQuery>,
) -> axum::response::Response {
    let Some(bus) = st.bus(&q.room) else {
        return ERR_NO_ROOM.into_response();
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    match bus.history(limit) {
        Ok(m) => Json(m).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn agent_shell_api_joins_waits_and_posts_without_json_encoding() {
        let conn = crate::store::open_in_memory().unwrap();
        let bus = Bus::new(conn, "main");
        let app = router_single(bus);

        let wait_req = Request::builder()
            .uri("/api/agent/wait?timeout_ms=1")
            .header("x-agent", "manager")
            .header("x-room", "main")
            .header("x-agent-role", "tech lead")
            .body(Body::empty())
            .unwrap();
        let wait_resp = app.clone().oneshot(wait_req).await.unwrap();
        assert_eq!(wait_resp.status(), StatusCode::OK);
        let wait_body = to_bytes(wait_resp.into_body(), 64 * 1024).await.unwrap();
        let wait_json: serde_json::Value = serde_json::from_slice(&wait_body).unwrap();
        assert_eq!(wait_json["outcome"], "idle");

        let roster_req = Request::builder()
            .uri("/api/roster")
            .body(Body::empty())
            .unwrap();
        let roster_resp = app.clone().oneshot(roster_req).await.unwrap();
        let roster_body = to_bytes(roster_resp.into_body(), 64 * 1024).await.unwrap();
        let roster: Vec<serde_json::Value> = serde_json::from_slice(&roster_body).unwrap();
        assert_eq!(roster[0]["name"], "manager");
        assert_eq!(roster[0]["role"], "tech lead");

        let post_req = Request::builder()
            .method("POST")
            .uri("/api/agent/post?requires_reply=false")
            .header("x-agent", "manager")
            .header("x-room", "main")
            .body(Body::from("@human standing by"))
            .unwrap();
        let post_resp = app.oneshot(post_req).await.unwrap();
        assert_eq!(post_resp.status(), StatusCode::OK);
        let post_body = to_bytes(post_resp.into_body(), 64 * 1024).await.unwrap();
        let post_json: serde_json::Value = serde_json::from_slice(&post_body).unwrap();
        assert_eq!(post_json["from"], "manager");
        assert_eq!(post_json["body"], "@human standing by");
    }

    #[tokio::test]
    async fn heartbeat_joins_requested_employee_before_wait() {
        let conn = crate::store::open_in_memory().unwrap();
        let bus = Bus::new(conn, "main");
        bus.seed_employee(
            "frontend",
            &serde_json::json!({ "role": "frontend engineer" }),
        )
        .unwrap();
        let app = router_single(bus);

        let hb_req = Request::builder()
            .method("POST")
            .uri("/api/heartbeat")
            .header("x-agent", "frontend")
            .header("x-room", "main")
            .header("x-agent-role", "frontend engineer")
            .body(Body::empty())
            .unwrap();
        let hb_resp = app.clone().oneshot(hb_req).await.unwrap();
        assert_eq!(hb_resp.status(), StatusCode::NO_CONTENT);

        let roster_req = Request::builder()
            .uri("/api/roster")
            .body(Body::empty())
            .unwrap();
        let roster_resp = app.clone().oneshot(roster_req).await.unwrap();
        let roster_body = to_bytes(roster_resp.into_body(), 64 * 1024).await.unwrap();
        let roster: Vec<serde_json::Value> = serde_json::from_slice(&roster_body).unwrap();
        assert_eq!(roster[0]["name"], "frontend");
        assert_eq!(roster[0]["role"], "frontend engineer");
        assert_eq!(roster[0]["status"], "working");

        let employees_req = Request::builder()
            .uri("/api/employees")
            .body(Body::empty())
            .unwrap();
        let employees_resp = app.oneshot(employees_req).await.unwrap();
        let employees_body = to_bytes(employees_resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let employees: Vec<serde_json::Value> = serde_json::from_slice(&employees_body).unwrap();
        assert_eq!(employees[0]["name"], "frontend");
        assert_eq!(employees[0]["state"], "active");
    }
}
