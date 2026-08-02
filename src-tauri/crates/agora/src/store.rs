//! SQLite-backed durable store: the message log, the agent roster, and the
//! obligation graph ("who owes whom a response").
//!
//! Functions take `&Connection` so they are easy to unit-test against an in-memory db.

use crate::envelope::{now_ms, Kind, Message};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::str::FromStr;

/// A row in the agent roster.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct AgentRow {
    pub name: String,
    pub role: Option<String>,
    pub status: String,
    pub cursor: i64,
    pub joined_at: i64,
    pub last_seen: i64,
}

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening database at {path}"))?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// A connection shared across rooms, behind a Mutex. Hides the rusqlite type so
/// host crates can hold the handle without depending on rusqlite directly.
pub type SharedConn = std::sync::Arc<std::sync::Mutex<Connection>>;

/// Open the db once and return a shareable handle. All rooms run over this one
/// connection (WAL single-writer → no inter-connection write contention).
pub fn open_shared(path: &str) -> Result<SharedConn> {
    Ok(std::sync::Arc::new(std::sync::Mutex::new(open(path)?)))
}

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS messages (
            seq    INTEGER PRIMARY KEY AUTOINCREMENT,
            id     TEXT NOT NULL UNIQUE,
            ts     INTEGER NOT NULL,
            room   TEXT NOT NULL,
            sender TEXT NOT NULL,
            to_json TEXT NOT NULL,
            kind   TEXT NOT NULL,
            body   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_room_seq ON messages(room, seq);

        CREATE TABLE IF NOT EXISTS agents (
            room       TEXT NOT NULL,
            name       TEXT NOT NULL,
            role       TEXT,
            status     TEXT NOT NULL,
            cursor     INTEGER NOT NULL DEFAULT 0,
            joined_at  INTEGER NOT NULL,
            last_seen  INTEGER NOT NULL,
            PRIMARY KEY (room, name)
        );

        -- obligation graph: `debtor` owes `creditor` a reply to message `msg_id`.
        CREATE TABLE IF NOT EXISTS obligations (
            room     TEXT NOT NULL,
            debtor   TEXT NOT NULL,
            creditor TEXT NOT NULL,
            msg_id   TEXT NOT NULL DEFAULT '',
            ts       INTEGER NOT NULL,
            PRIMARY KEY (room, debtor, creditor)
        );

        -- employees: the DESIRED roster. Every agent (the initial team and any the
        -- manager hires at runtime) is an employee; a supervisor reconciles this into
        -- real processes. `spec` is an opaque JSON blob (role/backend/manage/…) that the
        -- bus does not interpret — only the launcher does.
        -- state: 'requested' (awaiting launch) | 'active' (joined) | 'disabled' (fired).
        CREATE TABLE IF NOT EXISTS employees (
            room   TEXT NOT NULL,
            name   TEXT NOT NULL,
            spec   TEXT NOT NULL,
            state  TEXT NOT NULL,
            ts     INTEGER NOT NULL,
            PRIMARY KEY (room, name)
        );

        -- Latest token-usage snapshot per agent. Backends report their native
        -- counters (Claude transcript usage, Codex token_count events); the
        -- Team UI aggregates these rows into the usage bar.
        CREATE TABLE IF NOT EXISTS usage_snapshots (
            room                    TEXT NOT NULL,
            agent                   TEXT NOT NULL,
            backend                 TEXT NOT NULL DEFAULT '',
            model                   TEXT,
            source                  TEXT NOT NULL DEFAULT '',
            session_id              TEXT,
            input_tokens            INTEGER NOT NULL DEFAULT 0,
            cached_input_tokens     INTEGER NOT NULL DEFAULT 0,
            output_tokens           INTEGER NOT NULL DEFAULT 0,
            reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens            INTEGER NOT NULL DEFAULT 0,
            last_input_tokens       INTEGER NOT NULL DEFAULT 0,
            last_cached_input_tokens INTEGER NOT NULL DEFAULT 0,
            last_output_tokens      INTEGER NOT NULL DEFAULT 0,
            last_reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
            last_total_tokens       INTEGER NOT NULL DEFAULT 0,
            context_tokens          INTEGER NOT NULL DEFAULT 0,
            context_window          INTEGER NOT NULL DEFAULT 0,
            updated_at              INTEGER NOT NULL,
            PRIMARY KEY (room, agent)
        );
        "#,
    )?;
    Ok(())
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let to_json: String = row.get("to_json")?;
    let to: Vec<String> = serde_json::from_str(&to_json).unwrap_or_default();
    let kind_str: String = row.get("kind")?;
    Ok(Message {
        seq: row.get("seq")?,
        id: row.get("id")?,
        ts: row.get("ts")?,
        room: row.get("room")?,
        from: row.get("sender")?,
        to,
        kind: Kind::from_str(&kind_str).unwrap_or(Kind::Msg),
        body: row.get("body")?,
    })
}

/// Append a message to the log, assigning id/ts/seq. Returns the stored message.
pub fn append(
    conn: &Connection,
    room: &str,
    from: &str,
    to: &[String],
    kind: Kind,
    body: &str,
) -> Result<Message> {
    let id = uuid::Uuid::new_v4().to_string();
    let ts = now_ms();
    let to_json = serde_json::to_string(to)?;
    conn.execute(
        "INSERT INTO messages (id, ts, room, sender, to_json, kind, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, ts, room, from, to_json, kind.as_str(), body],
    )?;
    let seq = conn.last_insert_rowid();
    Ok(Message {
        seq,
        id,
        ts,
        room: room.to_string(),
        from: from.to_string(),
        to: to.to_vec(),
        kind,
        body: body.to_string(),
    })
}

pub fn messages_after(
    conn: &Connection,
    room: &str,
    after: i64,
    limit: i64,
) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT seq,id,ts,room,sender,to_json,kind,body
         FROM messages WHERE room=?1 AND seq>?2 ORDER BY seq ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![room, after, limit], row_to_message)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn history(conn: &Connection, room: &str, limit: i64) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM (
            SELECT seq,id,ts,room,sender,to_json,kind,body
            FROM messages WHERE room=?1 ORDER BY seq DESC LIMIT ?2
         ) ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(params![room, limit], row_to_message)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn max_seq(conn: &Connection, room: &str) -> Result<i64> {
    let seq: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM messages WHERE room=?1",
        params![room],
        |r| r.get(0),
    )?;
    Ok(seq.unwrap_or(0))
}

/// Recent messages sent by `sender`, newest first (used to re-surface an unanswered @).
pub fn messages_from(
    conn: &Connection,
    room: &str,
    sender: &str,
    limit: i64,
) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT seq,id,ts,room,sender,to_json,kind,body
         FROM messages WHERE room=?1 AND sender=?2 ORDER BY seq DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![room, sender, limit], row_to_message)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Register or refresh an agent. New agents start with their cursor at the current
/// head. Returns the (possibly pre-existing) cursor and whether it was a fresh join.
pub fn upsert_agent(
    conn: &Connection,
    room: &str,
    name: &str,
    role: Option<&str>,
) -> Result<(i64, bool)> {
    let now = now_ms();
    let existing: Option<i64> = conn
        .query_row(
            "SELECT cursor FROM agents WHERE room=?1 AND name=?2",
            params![room, name],
            |r| r.get(0),
        )
        .ok();
    match existing {
        Some(cursor) => {
            conn.execute(
                "UPDATE agents SET status='online', last_seen=?3, role=COALESCE(?4, role)
                 WHERE room=?1 AND name=?2",
                params![room, name, now, role],
            )?;
            Ok((cursor, false))
        }
        None => {
            let head = max_seq(conn, room)?;
            conn.execute(
                "INSERT INTO agents (room,name,role,status,cursor,joined_at,last_seen)
                 VALUES (?1,?2,?3,'online',?4,?5,?5)",
                params![room, name, role, head, now],
            )?;
            Ok((head, true))
        }
    }
}

pub fn set_status(conn: &Connection, room: &str, name: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET status=?3, last_seen=?4 WHERE room=?1 AND name=?2",
        params![room, name, status, now_ms()],
    )?;
    Ok(())
}

pub fn set_cursor(conn: &Connection, room: &str, name: &str, cursor: i64) -> Result<()> {
    conn.execute(
        "UPDATE agents SET cursor=?3, last_seen=?4 WHERE room=?1 AND name=?2",
        params![room, name, cursor, now_ms()],
    )?;
    Ok(())
}

pub fn touch(conn: &Connection, room: &str, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET last_seen=?3 WHERE room=?1 AND name=?2",
        params![room, name, now_ms()],
    )?;
    Ok(())
}

pub fn get_agent(conn: &Connection, room: &str, name: &str) -> Result<Option<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT name,role,status,cursor,joined_at,last_seen FROM agents WHERE room=?1 AND name=?2",
    )?;
    let mut rows = stmt.query_map(params![room, name], agent_from_row)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

pub fn roster(conn: &Connection, room: &str) -> Result<Vec<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT name,role,status,cursor,joined_at,last_seen FROM agents WHERE room=?1 ORDER BY joined_at ASC",
    )?;
    let rows = stmt.query_map(params![room], agent_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn agent_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRow> {
    Ok(AgentRow {
        name: row.get(0)?,
        role: row.get(1)?,
        status: row.get(2)?,
        cursor: row.get(3)?,
        joined_at: row.get(4)?,
        last_seen: row.get(5)?,
    })
}

// ---- obligation graph: `debtor` owes `creditor` a reply (to message `msg_id`) ----

pub fn add_obligation(
    conn: &Connection,
    room: &str,
    debtor: &str,
    creditor: &str,
    msg_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO obligations (room, debtor, creditor, msg_id, ts) VALUES (?1,?2,?3,?4,?5)",
        params![room, debtor, creditor, msg_id, now_ms()],
    )?;
    Ok(())
}

pub fn clear_obligation(conn: &Connection, room: &str, debtor: &str, creditor: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM obligations WHERE room=?1 AND debtor=?2 AND creditor=?3",
        params![room, debtor, creditor],
    )?;
    Ok(())
}

/// Remove every obligation involving `name` (used when an agent is fired/leaves).
pub fn clear_agent_obligations(conn: &Connection, room: &str, name: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM obligations WHERE room=?1 AND (debtor=?2 OR creditor=?2)",
        params![room, name],
    )?;
    Ok(())
}

pub fn has_obligation(conn: &Connection, room: &str, debtor: &str, creditor: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM obligations WHERE room=?1 AND debtor=?2 AND creditor=?3",
        params![room, debtor, creditor],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// The creditors that `debtor` currently owes a reply to.
pub fn owes(conn: &Connection, room: &str, debtor: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT creditor FROM obligations WHERE room=?1 AND debtor=?2 ORDER BY ts ASC")?;
    let rows = stmt.query_map(params![room, debtor], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// What `debtor` owes, as (creditor, originating message id) pairs.
pub fn owed_with_msg(conn: &Connection, room: &str, debtor: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT creditor, msg_id FROM obligations WHERE room=?1 AND debtor=?2 ORDER BY ts ASC",
    )?;
    let rows = stmt.query_map(params![room, debtor], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn message_by_id(conn: &Connection, room: &str, id: &str) -> Result<Option<Message>> {
    let mut stmt = conn.prepare(
        "SELECT seq,id,ts,room,sender,to_json,kind,body FROM messages WHERE room=?1 AND id=?2",
    )?;
    let mut rows = stmt.query_map(params![room, id], row_to_message)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Every outstanding obligation in the room as (debtor, creditor) pairs.
pub fn all_obligations(conn: &Connection, room: &str) -> Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT debtor, creditor FROM obligations WHERE room=?1 ORDER BY ts ASC")?;
    let rows = stmt.query_map(params![room], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---- employees: runtime-hired agents ----

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Employee {
    pub name: String,
    /// Opaque launcher spec (role / backend / manage / …); the bus never reads it.
    pub spec: serde_json::Value,
    pub state: String,
    pub ts: i64,
}

fn employee_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Employee> {
    let spec_str: String = row.get(1)?;
    Ok(Employee {
        name: row.get(0)?,
        spec: serde_json::from_str(&spec_str).unwrap_or(serde_json::Value::Null),
        state: row.get(2)?,
        ts: row.get(3)?,
    })
}

pub fn get_employee(conn: &Connection, room: &str, name: &str) -> Result<Option<Employee>> {
    let mut stmt =
        conn.prepare("SELECT name,spec,state,ts FROM employees WHERE room=?1 AND name=?2")?;
    let mut rows = stmt.query_map(params![room, name], employee_from_row)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

pub fn list_employees(conn: &Connection, room: &str) -> Result<Vec<Employee>> {
    let mut stmt =
        conn.prepare("SELECT name,spec,state,ts FROM employees WHERE room=?1 ORDER BY ts ASC")?;
    let rows = stmt.query_map(params![room], employee_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn create_employee(
    conn: &Connection,
    room: &str,
    name: &str,
    spec: &serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO employees (room,name,spec,state,ts) VALUES (?1,?2,?3,'requested',?4)",
        params![room, name, spec.to_string(), now_ms()],
    )?;
    Ok(())
}

pub fn set_employee_state(conn: &Connection, room: &str, name: &str, state: &str) -> Result<()> {
    conn.execute(
        "UPDATE employees SET state=?3 WHERE room=?1 AND name=?2",
        params![room, name, state],
    )?;
    Ok(())
}

// ---- usage snapshots: latest token counters per agent ----

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageSnapshot {
    pub agent: String,
    pub backend: String,
    pub model: Option<String>,
    pub source: String,
    pub session_id: Option<String>,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
    pub last_input_tokens: i64,
    pub last_cached_input_tokens: i64,
    pub last_output_tokens: i64,
    pub last_reasoning_output_tokens: i64,
    pub last_total_tokens: i64,
    pub context_tokens: i64,
    pub context_window: i64,
    pub updated_at: i64,
}

fn usage_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageSnapshot> {
    Ok(UsageSnapshot {
        agent: row.get(0)?,
        backend: row.get(1)?,
        model: row.get(2)?,
        source: row.get(3)?,
        session_id: row.get(4)?,
        input_tokens: row.get(5)?,
        cached_input_tokens: row.get(6)?,
        output_tokens: row.get(7)?,
        reasoning_output_tokens: row.get(8)?,
        total_tokens: row.get(9)?,
        last_input_tokens: row.get(10)?,
        last_cached_input_tokens: row.get(11)?,
        last_output_tokens: row.get(12)?,
        last_reasoning_output_tokens: row.get(13)?,
        last_total_tokens: row.get(14)?,
        context_tokens: row.get(15)?,
        context_window: row.get(16)?,
        updated_at: row.get(17)?,
    })
}

fn token_i64(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0).max(0)
}

fn token_string(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

pub fn upsert_usage(
    conn: &Connection,
    room: &str,
    agent: &str,
    payload: &serde_json::Value,
) -> Result<UsageSnapshot> {
    let total = payload.get("total").unwrap_or(payload);
    let last = payload.get("last").unwrap_or(&serde_json::Value::Null);

    let input_tokens = token_i64(total, "input_tokens");
    let cached_input_tokens = token_i64(total, "cached_input_tokens");
    let output_tokens = token_i64(total, "output_tokens");
    let reasoning_output_tokens = token_i64(total, "reasoning_output_tokens");
    let mut total_tokens = token_i64(total, "total_tokens");
    if total_tokens == 0 {
        total_tokens = input_tokens + cached_input_tokens + output_tokens + reasoning_output_tokens;
    }

    let last_input_tokens = token_i64(last, "input_tokens");
    let last_cached_input_tokens = token_i64(last, "cached_input_tokens");
    let last_output_tokens = token_i64(last, "output_tokens");
    let last_reasoning_output_tokens = token_i64(last, "reasoning_output_tokens");
    let mut last_total_tokens = token_i64(last, "total_tokens");
    if last_total_tokens == 0 {
        last_total_tokens = last_input_tokens
            + last_cached_input_tokens
            + last_output_tokens
            + last_reasoning_output_tokens;
    }
    let context_tokens = token_i64(payload, "context_tokens").max(last_total_tokens);
    let context_window = token_i64(payload, "context_window");
    let backend = token_string(payload, "backend").unwrap_or_default();
    let model = token_string(payload, "model");
    let source = token_string(payload, "source").unwrap_or_default();
    let session_id = token_string(payload, "session_id");
    let updated_at = now_ms();

    conn.execute(
        "INSERT INTO usage_snapshots (
            room,agent,backend,model,source,session_id,
            input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,
            last_input_tokens,last_cached_input_tokens,last_output_tokens,last_reasoning_output_tokens,last_total_tokens,
            context_tokens,context_window,updated_at
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
         ON CONFLICT(room, agent) DO UPDATE SET
            backend=excluded.backend,
            model=excluded.model,
            source=excluded.source,
            session_id=excluded.session_id,
            input_tokens=excluded.input_tokens,
            cached_input_tokens=excluded.cached_input_tokens,
            output_tokens=excluded.output_tokens,
            reasoning_output_tokens=excluded.reasoning_output_tokens,
            total_tokens=excluded.total_tokens,
            last_input_tokens=excluded.last_input_tokens,
            last_cached_input_tokens=excluded.last_cached_input_tokens,
            last_output_tokens=excluded.last_output_tokens,
            last_reasoning_output_tokens=excluded.last_reasoning_output_tokens,
            last_total_tokens=excluded.last_total_tokens,
            context_tokens=excluded.context_tokens,
            context_window=excluded.context_window,
            updated_at=excluded.updated_at",
        params![
            room,
            agent,
            backend,
            model,
            source,
            session_id,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
            last_input_tokens,
            last_cached_input_tokens,
            last_output_tokens,
            last_reasoning_output_tokens,
            last_total_tokens,
            context_tokens,
            context_window,
            updated_at,
        ],
    )?;

    Ok(UsageSnapshot {
        agent: agent.to_string(),
        backend,
        model,
        source,
        session_id,
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        last_input_tokens,
        last_cached_input_tokens,
        last_output_tokens,
        last_reasoning_output_tokens,
        last_total_tokens,
        context_tokens,
        context_window,
        updated_at,
    })
}

pub fn usage(conn: &Connection, room: &str) -> Result<Vec<UsageSnapshot>> {
    let mut stmt = conn.prepare(
        "SELECT agent,backend,model,source,session_id,
                input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,
                last_input_tokens,last_cached_input_tokens,last_output_tokens,last_reasoning_output_tokens,last_total_tokens,
                context_tokens,context_window,updated_at
         FROM usage_snapshots WHERE room=?1 ORDER BY total_tokens DESC, agent ASC",
    )?;
    let rows = stmt.query_map(params![room], usage_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Forget everything persisted about a room: its message log, roster, obligation
/// graph, and desired-employee roster. Used on an explicit (re)start or close so
/// leftover rows from a previous team on the same workspace can never shadow a
/// fresh start. Recovery never calls this — it adopts a live team as-is.
pub fn clear_room(conn: &Connection, room: &str) -> Result<()> {
    conn.execute("DELETE FROM messages WHERE room=?1", params![room])?;
    conn.execute("DELETE FROM agents WHERE room=?1", params![room])?;
    conn.execute("DELETE FROM obligations WHERE room=?1", params![room])?;
    conn.execute("DELETE FROM employees WHERE room=?1", params![room])?;
    conn.execute("DELETE FROM usage_snapshots WHERE room=?1", params![room])?;
    Ok(())
}
