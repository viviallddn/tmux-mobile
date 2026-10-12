# Team Markdown Output Viewer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make team-produced `.md` files clickable in the Team chat so clicking opens a new system-browser window that renders the Markdown (with code highlighting + mermaid) via the Team daemon's local `:8787` HTTP service.

**Architecture:** Add a host-owned `GET /view` route (+ `/view/assets/*`) merged onto the existing agora axum router on `team_bind`. The route resolves `room → workspace` from the live `TeamManager`, validates the path stays inside that workspace, server-renders the `.md` with `pulldown-cmark`, and serves a page whose embedded highlight.js + mermaid run client-side. The Svelte Team tab rewrites workspace-relative `.md` references in chat to that URL; the existing global `a[href]` handler opens them in the system browser.

**Tech Stack:** Rust (axum 0.8, pulldown-cmark 0.12), vendored highlight.js 11.11.1 + mermaid 11.12.3 (embedded via `include_str!`), Svelte 5 (`marked` already present).

## Global Constraints

- **Desktop-only.** All new Rust lives behind `#[cfg(all(desktop, not(any(target_os = "android", target_os = "ios"))))]`, matching `team`/`team_bridge`. Mobile builds never compile it; the Team tab is already hidden on mobile.
- **Do NOT modify the vendored `agora` crate** (`src-tauri/crates/agora/`). The `/view` feature lives entirely in host code. `agora::web::router` is already `pub` — merge onto it.
- **`server.rs` must not name any agora/team concrete type.** It only speaks through the `TeamBridge` trait (JSON / primitives).
- **The `:8787` port is UNAUTHENTICATED and localhost-only.** Every `/view` request must resolve the workspace server-side from `room`, accept only a *relative* `path`, canonicalize-and-prefix-check against the workspace, and allow only `.md`/`.markdown`. No write/list endpoints.
- **Asset versions pinned to `package.json`:** highlight.js `11.11.1`, mermaid `11.12.3`.
- **axum version pinned to the agora crate's:** `0.8`.
- **Tests run sequentially:** `cd src-tauri && cargo test -- --test-threads=1`.

---

## File Structure

- `src-tauri/src/team_view.rs` *(create)* — the whole viewer: `/view` + `/view/assets/*` routes, path validation, markdown→HTML, page shell. One responsibility: serve rendered workspace Markdown.
- `src-tauri/assets/view/` *(create)* — vendored `highlight.min.js`, `github-dark.min.css`, `mermaid.min.js`, plus `README.md` recording versions/sources.
- `src-tauri/src/team_bridge.rs` *(modify)* — add `workspace_for_room` accessor; swap the `agora::web::serve` spawn for a host-built `router(...).merge(team_view::router(...))` serve; implement `view_base`.
- `src-tauri/src/server.rs` *(modify)* — add `view_base` to the `TeamBridge` trait, to the `team_status` payload, and to the test `MockAgora` impl.
- `src-tauri/src/lib.rs` *(modify)* — register `team_view` module (gated).
- `src-tauri/Cargo.toml` *(modify)* — add `axum` + `pulldown-cmark` to the desktop-gated deps.
- `src-tauri/src/main.rs` *(modify)* — add `team_view` unit tests.
- `src/lib/Team.svelte` *(modify)* — read `view_base`; add `resolveMessageLinks()` DOM pass (sibling of `resolveMessageImages`).
- `docs/design-docs/features/team.md`, `CLAUDE.md` *(modify)* — document the viewer + the unauth caveat.

---

## Task 1: Backend `/view` route — validation + render (no client JS yet)

**Files:**
- Modify: `src-tauri/Cargo.toml:58-59` (desktop-gated deps)
- Create: `src-tauri/src/team_view.rs`
- Modify: `src-tauri/src/team_bridge.rs` (add `workspace_for_room`; swap serve block ~116-123)
- Modify: `src-tauri/src/lib.rs:11` (register module)
- Test: `src-tauri/src/main.rs` (unit tests calling `team_view` pure fns)

**Interfaces:**
- Consumes: `crate::team_bridge::TeamManager` (`Arc`), `agora::web::router(provider)`.
- Produces:
  - `pub fn team_view::router(mgr: std::sync::Arc<crate::team_bridge::TeamManager>) -> axum::Router`
  - `pub(crate) fn team_view::render_markdown_file(workspace: &str, rel: &str) -> Result<String, axum::http::StatusCode>`
  - `pub(crate) fn team_view::markdown_to_html(src: &str) -> String`
  - `impl TeamManager { pub(crate) fn workspace_for_room(&self, room: &str) -> Option<String> }`

- [ ] **Step 1: Add the two desktop-gated dependencies**

In `src-tauri/Cargo.toml`, under the existing `[target.'cfg(not(any(target_os = "android", target_os = "ios")))'.dependencies]` block (currently just `agora`), add:

```toml
axum = "0.8"
pulldown-cmark = "0.12"
```

- [ ] **Step 2: Register the module in `lib.rs`**

After the `team_bridge` registration (`src-tauri/src/lib.rs:10-11`), add:

```rust
#[cfg(all(desktop, not(any(target_os = "android", target_os = "ios"))))]
pub mod team_view;
```

- [ ] **Step 3: Add the `workspace_for_room` accessor to `TeamManager`**

In `src-tauri/src/team_bridge.rs`, inside `impl TeamManager { … }` (near the other helpers), add:

```rust
    /// The absolute workspace dir for a known room, if any. Lets the desktop
    /// Markdown viewer (`team_view`) scope file reads to a team's workspace —
    /// `team_view` lives in a sibling module and can't see the private field.
    pub(crate) fn workspace_for_room(&self, room: &str) -> Option<String> {
        self.teams
            .lock()
            .unwrap()
            .get(room)
            .map(|t| t.workspace.clone())
            .filter(|w| !w.is_empty())
    }
```

- [ ] **Step 4: Create `team_view.rs` with the route, validation, and renderer**

Create `src-tauri/src/team_view.rs`:

```rust
//! Desktop-only Markdown viewer served on the Team daemon's HTTP port.
//!
//! Adds `GET /view?room=&path=` (render a workspace `.md` into a styled page)
//! onto the agora axum router. `room → workspace` is resolved from the live
//! `TeamManager`, and the client only ever passes a RELATIVE path — so this
//! unauthenticated, localhost-only port can never read outside a team's
//! workspace. Client-side highlight.js + mermaid assets are wired in Task 2.

use crate::team_bridge::TeamManager;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::path::{Component, Path};
use std::sync::Arc;

/// Reject files larger than this (rendered inline in a browser tab).
pub(crate) const MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone)]
struct ViewState {
    mgr: Arc<TeamManager>,
}

/// Host-owned routes merged onto agora's router by `team_bridge`.
pub fn router(mgr: Arc<TeamManager>) -> Router {
    Router::new()
        .route("/view", get(view))
        .with_state(ViewState { mgr })
}

#[derive(Deserialize)]
struct ViewQuery {
    room: String,
    path: String,
}

async fn view(State(st): State<ViewState>, Query(q): Query<ViewQuery>) -> Response {
    let workspace = match st.mgr.workspace_for_room(&q.room) {
        Some(w) => w,
        None => return (StatusCode::NOT_FOUND, "unknown room").into_response(),
    };
    match render_markdown_file(&workspace, &q.path) {
        Ok(html) => Html(html).into_response(),
        Err(code) => (code, code.canonical_reason().unwrap_or("error")).into_response(),
    }
}

/// Validate `rel` under `workspace`, read it, and render a full HTML page.
/// Never panics and never reads outside `workspace`; every failure is a status.
pub(crate) fn render_markdown_file(workspace: &str, rel: &str) -> Result<String, StatusCode> {
    let rel_path = Path::new(rel);
    // Relative only; reject absolute paths and any `..`/root/prefix component.
    if rel_path.is_absolute()
        || rel_path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(StatusCode::FORBIDDEN);
    }
    // Extension gate.
    let ext = rel_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "md" && ext != "markdown" {
        return Err(StatusCode::NOT_FOUND);
    }
    // Canonicalize both sides and confirm containment (defends against symlink
    // escape in addition to the textual `..` check above).
    let ws_real = Path::new(workspace)
        .canonicalize()
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let file_real = ws_real
        .join(rel_path)
        .canonicalize()
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !file_real.starts_with(&ws_real) {
        return Err(StatusCode::FORBIDDEN);
    }
    // Size cap.
    let meta = std::fs::metadata(&file_real).map_err(|_| StatusCode::NOT_FOUND)?;
    if meta.len() > MAX_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let src = std::fs::read_to_string(&file_real).map_err(|_| StatusCode::NOT_FOUND)?;
    let title = file_real
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("markdown");
    Ok(render_page(title, &markdown_to_html(&src)))
}

/// GFM Markdown → HTML body. Mermaid fences become `<pre class="mermaid">`;
/// other fences become `<pre><code class="language-…">` for highlight.js.
pub(crate) fn markdown_to_html(src: &str) -> String {
    use pulldown_cmark::{html, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let mut transformed: Vec<Event> = Vec::new();
    let mut in_code = false;
    let mut lang: Option<String> = None;
    let mut buf = String::new();

    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                buf.clear();
                lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string()),
                    CodeBlockKind::Indented => None,
                };
            }
            Event::Text(t) if in_code => buf.push_str(&t),
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                let escaped = html_escape(&buf);
                let block = match lang.as_deref() {
                    Some("mermaid") => format!("<pre class=\"mermaid\">{}</pre>", escaped),
                    Some(l) => format!(
                        "<pre><code class=\"language-{}\">{}</code></pre>",
                        html_escape(l),
                        escaped
                    ),
                    None => format!("<pre><code>{}</code></pre>", escaped),
                };
                transformed.push(Event::Html(block.into()));
                lang = None;
            }
            other => transformed.push(other),
        }
    }

    let mut out = String::new();
    html::push_html(&mut out, transformed.into_iter());
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Wrap a rendered body fragment in a dark standalone page. Task 2 adds the
/// highlight.js/mermaid `<link>`/`<script>` tags + init here.
fn render_page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>{title}</title>\n<style>{css}</style>\n</head>\n\
<body><main class=\"md\">{body}</main></body></html>\n",
        title = html_escape(title),
        css = PAGE_CSS,
        body = body
    )
}

const PAGE_CSS: &str = "\
:root{color-scheme:dark}\
body{margin:0;background:#0d1117;color:#e6edf3;\
font:15px/1.65 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif}\
.md{max-width:860px;margin:0 auto;padding:32px 20px 80px}\
.md h1,.md h2,.md h3{line-height:1.25;margin:1.4em 0 .5em}\
.md h1{border-bottom:1px solid #30363d;padding-bottom:.3em}\
.md h2{border-bottom:1px solid #21262d;padding-bottom:.25em}\
.md a{color:#4493f8}\
.md code{background:#161b22;border-radius:6px;padding:.15em .35em;font-size:85%}\
.md pre{background:#161b22;border:1px solid #30363d;border-radius:8px;\
padding:14px;overflow:auto}\
.md pre code{background:none;padding:0;font-size:90%}\
.md table{border-collapse:collapse;margin:1em 0}\
.md th,.md td{border:1px solid #30363d;padding:6px 12px}\
.md th{background:#161b22}\
.md blockquote{border-left:3px solid #30363d;margin:1em 0;padding:.2em 1em;color:#9da7b3}\
.md img{max-width:100%}\
.md hr{border:0;border-top:1px solid #30363d;margin:1.5em 0}\
.mermaid{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:14px;text-align:center}";
```

- [ ] **Step 5: Swap the serve block in `team_bridge.rs` to merge the view router**

In `src-tauri/src/team_bridge.rs`, replace the spawn block (currently ~lines 116-123):

```rust
            let provider: Arc<dyn BusProvider> = me.clone();
            let bind = bind.to_string();
            tokio::spawn(async move {
                if let Err(e) = agora::web::serve(provider, &bind).await {
                    eprintln!("⚠️  team daemon not listening on {}: {}", bind, e);
                }
            });
```

with:

```rust
            let provider: Arc<dyn BusProvider> = me.clone();
            let view_mgr = me.clone();
            let bind = bind.to_string();
            tokio::spawn(async move {
                // Host-owned Markdown viewer (`/view`) merged onto agora's
                // router, so the vendored crate stays an untouched upstream
                // copy. `agora::web::router` is already `pub`.
                let app = agora::web::router(provider)
                    .merge(crate::team_view::router(view_mgr));
                match tokio::net::TcpListener::bind(&bind).await {
                    Ok(listener) => {
                        if let Err(e) = axum::serve(listener, app).await {
                            eprintln!("⚠️  team daemon stopped on {}: {}", bind, e);
                        }
                    }
                    Err(e) => eprintln!("⚠️  team daemon not listening on {}: {}", bind, e),
                }
            });
```

- [ ] **Step 6: Write the failing tests**

Add to `src-tauri/src/main.rs` inside the existing `#[cfg(test)] mod tests { … }` block (reuse its sequential style). These call the pure functions directly — no HTTP plumbing needed:

```rust
    #[test]
    fn view_rejects_path_traversal() {
        let dir = std::env::temp_dir().join("tmm_view_trav");
        std::fs::create_dir_all(&dir).unwrap();
        let ws = dir.to_str().unwrap();
        let r = tmux_mobile::team_view::render_markdown_file(ws, "../../../etc/passwd");
        assert_eq!(r.unwrap_err(), axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn view_rejects_non_markdown() {
        let dir = std::env::temp_dir().join("tmm_view_ext");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("secret.txt"), "nope").unwrap();
        let ws = dir.to_str().unwrap();
        let r = tmux_mobile::team_view::render_markdown_file(ws, "secret.txt");
        assert_eq!(r.unwrap_err(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn view_renders_markdown_with_fences() {
        let dir = std::env::temp_dir().join("tmm_view_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let md = "# Title\n\n```rust\nfn main() {}\n```\n\n```mermaid\ngraph TD; A-->B;\n```\n";
        std::fs::write(dir.join("report.md"), md).unwrap();
        let ws = dir.to_str().unwrap();
        let html = tmux_mobile::team_view::render_markdown_file(ws, "report.md").unwrap();
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<pre class=\"mermaid\">"));
        assert!(html.contains("language-rust"));
        assert!(html.contains("A--&gt;B")); // mermaid source is html-escaped
    }

    #[test]
    fn markdown_plain_fence_has_no_language_class() {
        let html = tmux_mobile::team_view::markdown_to_html("```\nplain\n```\n");
        assert!(html.contains("<pre><code>plain"));
        assert!(!html.contains("language-"));
    }
```

> If the crate's library name is not `tmux_mobile`, adjust the `tmux_mobile::` prefix to match `src-tauri/Cargo.toml`'s `[lib] name` (confirm with `grep -A2 '\[lib\]' src-tauri/Cargo.toml`; fall back to the `[package] name` with dashes→underscores).

- [ ] **Step 7: Run tests to verify they fail**

Run: `cd src-tauri && cargo test -- --test-threads=1 view_ 2>&1 | tail -20`
Expected: compile error or FAIL — `team_view` not yet resolvable / assertions unmet.

- [ ] **Step 8: Build + run tests to verify they pass**

Run: `cd src-tauri && cargo build 2>&1 | tail -5 && cargo test -- --test-threads=1 view_ markdown_plain 2>&1 | tail -20`
Expected: build OK; the four tests PASS.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/team_view.rs src-tauri/src/team_bridge.rs src-tauri/src/lib.rs src-tauri/src/main.rs
git commit -m "feat(team): serve workspace markdown via /view route"
```

---

## Task 2: Vendor highlight.js + mermaid and wire them into the page

**Files:**
- Create: `src-tauri/assets/view/highlight.min.js`, `github-dark.min.css`, `mermaid.min.js`, `README.md`
- Modify: `src-tauri/src/team_view.rs` (asset consts + `/view/assets/{name}` route + `render_page` head/tail)
- Test: `src-tauri/src/main.rs` (asset lookup)

**Interfaces:**
- Produces: `pub(crate) fn team_view::asset_lookup(name: &str) -> Option<(&'static str, &'static str)>` (body, content-type).

- [ ] **Step 1: Vendor the asset files**

Download the pinned builds into `src-tauri/assets/view/`:

```bash
mkdir -p src-tauri/assets/view
curl -fsSL https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.11.1/highlight.min.js -o src-tauri/assets/view/highlight.min.js
curl -fsSL https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.11.1/styles/github-dark.min.css -o src-tauri/assets/view/github-dark.min.css
curl -fsSL https://cdn.jsdelivr.net/npm/mermaid@11.12.3/dist/mermaid.min.js -o src-tauri/assets/view/mermaid.min.js
ls -la src-tauri/assets/view/
```

Expected: three non-empty files (mermaid ≈ 2–3 MB). If any is 0 bytes or an HTML error page, stop and fix the URL before continuing.

- [ ] **Step 2: Record provenance**

Create `src-tauri/assets/view/README.md`:

```markdown
# Vendored viewer assets

Embedded into the desktop binary via `include_str!` in `src/team_view.rs` and
served from `GET /view/assets/<name>` so the Markdown viewer works offline.

| File | Version | Source |
|------|---------|--------|
| highlight.min.js | 11.11.1 | cdnjs.cloudflare.com/ajax/libs/highlight.js/11.11.1/highlight.min.js |
| github-dark.min.css | 11.11.1 | cdnjs.cloudflare.com/ajax/libs/highlight.js/11.11.1/styles/github-dark.min.css |
| mermaid.min.js | 11.12.3 | cdn.jsdelivr.net/npm/mermaid@11.12.3/dist/mermaid.min.js |

Versions must match `package.json`. To refresh, re-run the `curl` commands in
the plan with the new version and update this table.
```

- [ ] **Step 3: Add asset consts + lookup + route to `team_view.rs`**

Near the top of `src-tauri/src/team_view.rs` (after the `use` block), add:

```rust
// Vendored client assets (see assets/view/README.md for versions/sources).
const HLJS_JS: &str = include_str!("../assets/view/highlight.min.js");
const HLJS_CSS: &str = include_str!("../assets/view/github-dark.min.css");
const MERMAID_JS: &str = include_str!("../assets/view/mermaid.min.js");

/// `(body, content-type)` for a known asset name, else `None`.
pub(crate) fn asset_lookup(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "highlight.min.js" => Some((HLJS_JS, "text/javascript; charset=utf-8")),
        "github-dark.min.css" => Some((HLJS_CSS, "text/css; charset=utf-8")),
        "mermaid.min.js" => Some((MERMAID_JS, "text/javascript; charset=utf-8")),
        _ => None,
    }
}
```

In `router(...)`, add the asset route (axum 0.8 capture syntax uses `{name}`):

```rust
pub fn router(mgr: Arc<TeamManager>) -> Router {
    Router::new()
        .route("/view", get(view))
        .route("/view/assets/{name}", get(asset))
        .with_state(ViewState { mgr })
}
```

And add the handler (uses `axum::http::header`):

```rust
async fn asset(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match asset_lookup(&name) {
        Some((body, ct)) => (
            [(axum::http::header::CONTENT_TYPE, ct)],
            body,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown asset").into_response(),
    }
}
```

- [ ] **Step 4: Add the `<link>`/`<script>` tags + init to `render_page`**

Replace the `render_page` body (the `format!` from Task 1) so the `<head>` links the CSS and the page tail loads + initializes highlight.js and mermaid:

```rust
fn render_page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>{title}</title>\n\
<link rel=\"stylesheet\" href=\"/view/assets/github-dark.min.css\">\n\
<style>{css}</style>\n</head>\n\
<body><main class=\"md\">{body}</main>\n\
<script src=\"/view/assets/highlight.min.js\"></script>\n\
<script src=\"/view/assets/mermaid.min.js\"></script>\n\
<script>document.addEventListener('DOMContentLoaded',function(){{\
if(window.hljs){{document.querySelectorAll('pre code').forEach(function(el){{window.hljs.highlightElement(el);}});}}\
if(window.mermaid){{window.mermaid.initialize({{startOnLoad:true,theme:'dark',securityLevel:'strict'}});}}\
}});</script>\n\
</body></html>\n",
        title = html_escape(title),
        css = PAGE_CSS,
        body = body
    )
}
```

- [ ] **Step 5: Write the failing test**

Add to `src-tauri/src/main.rs` tests:

```rust
    #[test]
    fn view_asset_lookup_known_and_unknown() {
        let (js, ct) = tmux_mobile::team_view::asset_lookup("mermaid.min.js").unwrap();
        assert!(!js.is_empty());
        assert!(ct.contains("javascript"));
        assert!(tmux_mobile::team_view::asset_lookup("../secret").is_none());
    }

    #[test]
    fn view_page_links_assets() {
        let html = tmux_mobile::team_view::markdown_to_html("# Hi\n");
        // sanity: body render still works (asset wiring is in render_page)
        assert!(html.contains("<h1>Hi</h1>"));
    }
```

- [ ] **Step 6: Run to verify it fails**

Run: `cd src-tauri && cargo test -- --test-threads=1 view_asset 2>&1 | tail -15`
Expected: compile error (`asset_lookup` undefined) or FAIL.

- [ ] **Step 7: Build + run to verify it passes**

Run: `cd src-tauri && cargo build 2>&1 | tail -5 && cargo test -- --test-threads=1 view_asset view_page 2>&1 | tail -15`
Expected: build OK; both PASS.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/assets/view src-tauri/src/team_view.rs src-tauri/src/main.rs
git commit -m "feat(team): embed highlight.js + mermaid in the markdown viewer"
```

---

## Task 3: Expose the view base URL to the frontend

**Files:**
- Modify: `src-tauri/src/server.rs` (`TeamBridge` trait + `team_status` payload + `MockAgora` test impl)
- Modify: `src-tauri/src/team_bridge.rs` (`view_base` impl)
- Test: `src-tauri/src/server.rs` (extend the existing `team_status` test)

**Interfaces:**
- Produces: `TeamBridge::view_base(&self) -> String` (e.g. `"http://127.0.0.1:8787"`); `team_status` JSON gains `"view_base"`.

- [ ] **Step 1: Add `view_base` to the `TeamBridge` trait**

In `src-tauri/src/server.rs`, in the `pub trait TeamBridge { … }` block (alongside `system_prompt`/`default_workspace`), add:

```rust
    /// Base URL of the local Markdown view service (same host:port as the Team
    /// daemon, e.g. `http://127.0.0.1:8787`). The Team tab builds
    /// `<view_base>/view?room=&path=` links from it.
    fn view_base(&self) -> String;
```

- [ ] **Step 2: Implement it in `team_bridge.rs`**

In `src-tauri/src/team_bridge.rs`, in `impl TeamBridge for TeamManager { … }`, add (the `cfg.url` field is already `http://127.0.0.1:<port>` — see `TeamManager::start`):

```rust
    fn view_base(&self) -> String {
        self.cfg.url.clone()
    }
```

- [ ] **Step 3: Add it to the `team_status` payload**

In `src-tauri/src/server.rs`, in the `"team_status"` arm's `serde_json::json!({ … })`, add a field (after `recent_workspaces`):

```rust
                "view_base": bus.view_base(),
```

- [ ] **Step 4: Add it to the test `MockAgora` impl + extend the test**

Find the `impl TeamBridge for MockAgora` block in `server.rs` tests and add:

```rust
    fn view_base(&self) -> String {
        "http://127.0.0.1:8787".into()
    }
```

Then extend the existing `team_status` test (the one asserting `s["available"] == true`) with:

```rust
        assert_eq!(s["view_base"], "http://127.0.0.1:8787");
```

- [ ] **Step 5: Run to verify it fails**

Run: `cd src-tauri && cargo test -- --test-threads=1 team_status 2>&1 | tail -15`
Expected: compile error (trait not implemented for all impls) until the `MockAgora` method is added, then the new assertion drives it.

- [ ] **Step 6: Build + run to verify it passes**

Run: `cd src-tauri && cargo build 2>&1 | tail -5 && cargo test -- --test-threads=1 team_status 2>&1 | tail -15`
Expected: build OK; `team_status` test PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/server.rs src-tauri/src/team_bridge.rs
git commit -m "feat(team): expose view_base in team_status"
```

---

## Task 4: Linkify `.md` references in the Team chat

**Files:**
- Modify: `src/lib/Team.svelte` (read `view_base`; add `resolveMessageLinks()` DOM pass + its `$effect` hook)

**Interfaces:**
- Consumes: `team_status.view_base` (Task 3); existing `activeTeam.workspace` / `activeRoom`; existing `.msg-body` rendered by `renderMarkdown`.
- Produces: in-place rewrite of `.md` anchors + bare `.md` paths to `<view_base>/view?room=&path=` links. No new exports; clicks ride the existing global `a[href]` handler in `App.svelte`.

The pattern mirrors the existing `resolveMessageImages()` (a DOM pass in an `$effect`, see `Team.svelte:806-840`): operate on the rendered DOM so the `renderMarkdown` memo cache is untouched.

- [ ] **Step 1: Add the `viewBase` state and read it from status**

In `src/lib/Team.svelte`, near `let teams = $state([]);` (~line 93), add:

```js
  let viewBase = $state('');     // base URL of the local /view markdown service
```

In BOTH `refreshTeams()` (the `s` from `teamStatus()`, where `recentWorkspaces`/`systemPrompt` are set, ~line 205) and `refreshRoster()` (~line 256), add after the teams assignment:

```js
    viewBase = s?.view_base || '';
```

- [ ] **Step 2: Add the `resolveMessageLinks` DOM pass**

In `src/lib/Team.svelte`, directly after the `resolveMessageImages` function (~line 834), add:

```js
  // Turn workspace-relative .md references in rendered chat into links to the
  // local /view service, so clicking opens the markdown viewer in a new system
  // browser window (via App's global a[href] handler). Two kinds:
  //   1. anchors marked up as [text](path.md)
  //   2. bare paths typed in prose (agents often just say ./report.md)
  // Absolute URLs and non-.md refs are left untouched. DOM pass (not a string
  // rewrite) so the renderMarkdown memo cache stays valid across team switches.
  const _MD_BARE = /(?<![\w/@.-])((?:\.\/)?(?:[\w.-]+\/)*[\w.-]+\.md)(#[\w-]+)?(?![\w/.-])/g;
  function mdViewHref(room, rel) {
    const clean = rel.replace(/^\.\//, '');
    return `${viewBase}/view?room=${encodeURIComponent(room)}&path=${encodeURIComponent(clean)}`;
  }
  function isLocalMd(href) {
    return !!href
      && !/^(https?:|mailto:|data:|#)/i.test(href)
      && /\.md(#[\w-]+)?$/i.test(href);
  }
  function resolveMessageLinks() {
    if (!listEl || !viewBase || !activeRoom) return;
    const room = activeRoom;
    // 1. Rewrite existing markdown-link anchors that point at a local .md.
    for (const a of listEl.querySelectorAll('.msg-body a[href]')) {
      if (a.dataset.tmmLink === '1') continue;
      const href = a.getAttribute('href') || '';
      if (isLocalMd(href)) {
        a.setAttribute('href', mdViewHref(room, href.replace(/#.*$/, '')));
      }
      a.dataset.tmmLink = '1';
    }
    // 2. Linkify bare .md paths in text nodes not already inside an anchor.
    for (const body of listEl.querySelectorAll('.msg-body')) {
      if (body.dataset.tmmBare === '1') continue;
      body.dataset.tmmBare = '1';
      const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT);
      const targets = [];
      let n;
      while ((n = walker.nextNode())) {
        if (n.parentElement && n.parentElement.closest('a,code,pre')) continue;
        if (_MD_BARE.test(n.nodeValue || '')) targets.push(n);
      }
      for (const node of targets) {
        const frag = document.createDocumentFragment();
        let last = 0;
        const text = node.nodeValue;
        _MD_BARE.lastIndex = 0;
        let m;
        while ((m = _MD_BARE.exec(text))) {
          if (m.index > last) frag.appendChild(document.createTextNode(text.slice(last, m.index)));
          const a = document.createElement('a');
          a.setAttribute('href', mdViewHref(room, m[1]));
          a.textContent = m[0];
          frag.appendChild(a);
          last = m.index + m[0].length;
        }
        if (last < text.length) frag.appendChild(document.createTextNode(text.slice(last)));
        node.parentNode.replaceChild(frag, node);
      }
    }
  }
```

- [ ] **Step 3: Call it from the render `$effect`**

In the existing `$effect` that schedules `resolveMessageImages` (~line 836-840), add the links pass alongside it:

```js
  $effect(() => {
    messages;
    activeTeam?.workspace;
    viewBase;
    setTimeout(() => { resolveMessageImages(); resolveMessageLinks(); }, 50);
  });
```

- [ ] **Step 4: Build the frontend to verify it compiles**

Run: `npm run build 2>&1 | tail -15`
Expected: Vite build succeeds (no Svelte/JS errors). *(No frontend unit-test harness exists in this repo; correctness of the rewrite is verified manually in Step 5.)*

- [ ] **Step 5: Manual verification in the Team tab**

Run `npm run tauri:dev`, start a team, and have an agent (or paste as human) post a message containing both `[report](report.md)` and a bare `./report.md`, with a real `report.md` in the workspace containing a `# heading`, a fenced `rust` block, and a `mermaid` block. Verify:
- both references render as links;
- clicking opens a **new system-browser window** at `http://127.0.0.1:8787/view?room=…&path=report.md`;
- the page shows the heading, syntax-highlighted Rust, and a rendered mermaid diagram;
- an `https://example.com/x.md` link and a `notes.txt` mention are **not** rewritten.

- [ ] **Step 6: Commit**

```bash
git add src/lib/Team.svelte
git commit -m "feat(team): linkify produced .md files to the local viewer"
```

---

## Task 5: Documentation

**Files:**
- Modify: `docs/design-docs/features/team.md` (new section)
- Modify: `CLAUDE.md` (one line in the Team paragraph)

- [ ] **Step 1: Document the viewer in `team.md`**

Append a new section to `docs/design-docs/features/team.md` (after §8 "Known edges / future" or as a new §"Markdown output viewer"):

```markdown
## Markdown output viewer

Team-produced `.md` files are clickable in the chat. When a message references a
workspace-relative `.md` (either `[text](path.md)` or a bare `./path.md` in
prose), `Team.svelte` rewrites it to `<view_base>/view?room=&path=` — a route
on the SAME agora axum port (`team_bind`, default `127.0.0.1:8787`), added in
host code (`src-tauri/src/team_view.rs`) and merged onto `agora::web::router`
so the vendored crate stays untouched. Clicking opens the system browser (via
App's global `a[href]` handler) and the route server-renders the file with
`pulldown-cmark`; vendored highlight.js + mermaid (embedded via `include_str!`,
served from `/view/assets/*`) run client-side.

Security: `/view` inherits the port's **unauthenticated, localhost-only**
posture. It resolves the workspace from `room` server-side, accepts only a
relative `path`, canonicalize-prefix-checks it against the workspace, and
allows only `.md`/`.markdown` under a 5 MB cap — so a localhost caller can never
read outside a team's workspace. Keep `team_bind` on `127.0.0.1` on untrusted
networks. The viewer is desktop-only (the phone cannot reach `:8787`); phone-
side viewing would need the WS + in-app Files viewer, a separate design.
```

- [ ] **Step 2: Note it in `CLAUDE.md`**

In `CLAUDE.md`, in the "Team is desktop-only" Key Patterns bullet, append one sentence:

```markdown
  Team-produced `.md` files referenced in chat are linkified to a desktop-only `GET /view` route on the team daemon's port (`team_view.rs`, merged onto agora's router), which server-renders markdown (highlight.js + mermaid embedded) — clicking opens the system browser; the route is workspace-scoped and `.md`-only because `:8787` is unauthenticated.
```

- [ ] **Step 3: Commit**

```bash
git add docs/design-docs/features/team.md CLAUDE.md
git commit -m "docs(team): document the markdown output viewer"
```

---

## Self-Review Notes

- **Spec coverage:** D1 (desktop/system-browser) → Task 4 Step 5 + `App.svelte` handler reuse; D2 (chat linkify) → Task 4; D3 (`:8787`) → Task 1 Step 5 merge; D4 (host code, agora untouched) → Task 1 (`team_view.rs` + `router().merge()`); D5 (pulldown-cmark server + JS client) → Task 1 render + Task 2 assets; D6 (vendored/embedded) → Task 2; D7 (anchors + bare paths) → Task 4 Step 2. Security §4.4 → Task 1 `render_markdown_file`. `view_base` plumbing → Task 3. Tests §6 → Tasks 1-3 unit tests + Task 4 manual. Docs §7 → Task 5.
- **Known heuristic:** bare-path linkify (`_MD_BARE`) is intentionally conservative (skips `code`/`pre`/existing anchors, requires non-word boundaries). False positives are possible on prose that literally contains a `.md` token; acceptable per D7.
- **Lib-name caveat:** test modules reference the crate as `tmux_mobile::team_view` — confirm/adjust to the actual `[lib] name` (Task 1 Step 6 note).
