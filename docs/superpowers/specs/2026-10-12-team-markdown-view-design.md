# Team Markdown Output → Local-Service Link → Browser Viewer

**Date:** 2026-10-12
**Status:** Approved design, pending implementation plan
**Scope:** Desktop-only (Team feature is already desktop-gated)

## 1. Goal

When a team agent produces a Markdown file in its workspace and mentions it in
chat, that `.md` reference should become a clickable link served by the Team's
existing local HTTP service. Clicking it opens a **new system-browser window**
rendering the Markdown with code highlighting and mermaid diagrams.

## 2. Decisions (locked)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Viewing surface is the **desktop Team tab**; links open in the **system browser** (not an in-app iframe). | A real browser on the desktop host can reach `:8787`; the existing global `a[href]` handler already routes external `http` links to the system browser. |
| D2 | Trigger is **chat `.md` linkification** — no agent-protocol change. | User choice. The frontend rewrites `.md` references at render time. |
| D3 | The local service is the **existing agora axum server on `:8787`** (`team_bind`), extended with a `GET /view` route. | It is already running and serves the dashboard; near-zero new infrastructure. |
| D4 | The `/view` route lives in **host code** (`src-tauri/src/team_view.rs`), merged onto agora's router — the vendored `agora` crate is **not** modified. | Repo creed: agora stays a faithful upstream copy. The Markdown viewer is our feature. |
| D5 | Markdown is **rendered server-side with `pulldown-cmark`** (GFM); **highlight.js + mermaid run client-side** on the served page. | Server emits the HTML skeleton; the two JS libraries do highlighting and diagram rendering in the browser. |
| D6 | highlight.js (11.11.1) + mermaid (11.12.3) are **vendored and embedded** via `include_str!`, served from `/view/assets/*`. | Offline-safe and self-contained, matching team.md's "packaged `.app` needs no external files" principle. Cost: ~3 MB in the desktop-only binary. |
| D7 | Linkify both **markdown-link anchors** ending in `.md` and **bare relative `.md` paths** in prose. | Agents more often say "wrote it to `./report.md`" than write a formal link; bare-path linkify better fits "whenever the team produces". |

## 3. Architecture / Data Flow

```
agent writes report.md in workspace, mentions it in chat
        │
        ▼
Team.svelte renderMarkdown()  — rewrites workspace-relative .md references to
        │                        http://127.0.0.1:8787/view?room=<room>&path=report.md
        ▼
click → App global a[href] handler → system browser (new window)
        │
        ▼
agora :8787 axum  ── merged GET /view (host code: team_view.rs) ──
        │   resolve room→workspace (TeamManager), validate path, read .md,
        │   pulldown-cmark → HTML skeleton, wrap in page shell
        ▼
browser renders: highlight.js highlights <code>, mermaid renders <pre class="mermaid">
```

One line: reuse the Team's already-running `:8787` service; add a `/view` route
that server-renders workspace `.md` into a styled standalone page; the frontend
rewrites chat `.md` references to that URL; clicking opens the system browser.

## 4. Backend — `/view` route

### 4.1 Module & wiring

- New desktop-only module `src-tauri/src/team_view.rs`
  (`#[cfg(not(any(target_os = "android", target_os = "ios")))]`), registered in
  `lib.rs` under the same gate as the other team modules.
- Exports `pub fn router(mgr: Arc<TeamManager>) -> axum::Router`.
- `team_bridge.rs` currently spawns `agora::web::serve(provider, &bind)`. Change
  it to build the router in host code and serve it:

  ```rust
  let app = agora::web::router(provider).merge(team_view::router(me.clone()));
  let listener = tokio::net::TcpListener::bind(&bind).await?;
  axum::serve(listener, app).await?;
  ```

  `agora::web::router` is already `pub`. This keeps all Markdown-view logic in
  host code; the agora crate stays untouched.

### 4.2 Routes

- `GET /view?room=<room>&path=<relative>` → rendered HTML page.
- `GET /view/assets/<name>` → embedded JS/CSS assets (highlight.js, its
  github-dark theme, mermaid), served from `include_str!` bytes with correct
  `Content-Type`.

### 4.3 room → workspace

Resolve via `TeamManager.teams` (`Mutex<HashMap<room, Team { workspace, … }>>`).
Unknown or unstarted room → `404`. The client passes only a **relative** path;
the absolute base always comes from the server-resolved workspace.

### 4.4 Security (the `:8787` port has NO auth)

All checks are mandatory — this port is reachable by anything on localhost:

1. `room` must map to a known workspace, else `404`.
2. `path` must be **relative** (reject if it is absolute).
3. Compute `canonicalize(workspace.join(path))`; it must still start with
   `canonicalize(workspace)`, else `403` (blocks `../` traversal and symlink
   escape).
4. Extension must be `.md` or `.markdown`, else `404`.
5. File size cap (5 MB); larger → `413`.
6. Read-only `GET`; no write/list endpoints are added.

Docs restate that `/view` inherits the port's unauthenticated, localhost-only
posture; keep `team_bind` on `127.0.0.1` on untrusted networks.

### 4.5 Rendering

- Parse with `pulldown-cmark` (new desktop-only dependency) with GFM options
  (tables, strikethrough, task lists).
- Walk the event stream; for fenced code blocks special-case the info string:
  - `mermaid` → emit `<pre class="mermaid">{text-escaped source}</pre>`
    (mermaid.js discovers this class);
  - any other language → `<pre><code class="language-{lang}">{escaped}</code></pre>`
    (highlight.js highlights it on load);
  - no language → plain `<pre><code>`.
- Everything else uses pulldown-cmark's standard HTML writer.
- Wrap the body fragment in a page shell: embedded dark CSS matching the
  dashboard, `<link>`/`<script>` to `/view/assets/*`, and an init tail:
  `hljs.highlightAll()` and
  `mermaid.initialize({ startOnLoad: true, theme: 'dark' })`.
- The page `<title>` is the file's basename.

### 4.6 Assets

Vendor the minified builds into the repo (e.g. `src-tauri/assets/view/`) and
`include_str!` them into `team_view.rs`:

- `highlight.min.js` (highlight.js 11.11.1, common languages build)
- `github-dark.min.css` (highlight.js theme)
- `mermaid.min.js` (mermaid 11.12.3)

Pin these versions (match `package.json`) and record the download source in a
short `README` beside the vendored files so they can be refreshed deliberately.

## 5. Frontend — chat `.md` linkification

### 5.1 Knowing the base URL

Backend exposes the view service base to the client. Add a `viewBase` field
(e.g. `http://127.0.0.1:8787`, derived from `team_bind`) to the `teams()` /
team-status payload so `Team.svelte` can build
`${viewBase}/view?room=${room}&path=${rel}`.

### 5.2 Rewrite logic in `renderMarkdown()`

After `marked.parse(...)` and `highlightHumanMentions(...)`, for the active
room's workspace:

1. **Markdown-link anchors:** any `<a href="X">` where `X` is **not** absolute
   (`http(s):`, `mailto:`, `#`, `data:`) and ends in `.md` (optionally with a
   `#fragment`) → rewrite `href` to the `/view` URL with the normalized relative
   path (strip a leading `./`).
2. **Bare paths:** a conservative regex over text (not already inside an anchor)
   matching a workspace-relative token ending in `.md`
   (e.g. `./report.md`, `docs/x.md`) → wrap in an anchor to the same `/view`
   URL. Keep the pattern tight to limit false positives; this is explicitly
   heuristic.

Absolute URLs, non-`.md` references, and already-rewritten links are left
untouched. Clicking relies on the existing global `a[href]` handler to open the
system browser — no extra `window.open` code.

### 5.3 Memoization

`renderMarkdown` is memoized by body text; the cache key must also incorporate
`viewBase`/workspace so a team switch does not serve a stale rewrite. Clear or
key the `_mdCache` on active-room change.

## 6. Testing

Rust sequential tests (in `src-tauri/src/main.rs`, matching the existing
`--test-threads=1` convention), driving `team_view::router` against a temp
workspace:

- path traversal (`path=../../etc/passwd`) → `403`;
- non-markdown (`path=secret.txt`) → `404`;
- unknown room → `404`;
- oversize file → `413`;
- valid `.md` → `200`, body contains the rendered `<h1>` and a
  `<pre class="mermaid">` for a mermaid fence and `language-rust` for a rust
  fence;
- `/view/assets/mermaid.min.js` → `200` with a JS content-type.

Frontend linkify is covered by a small unit check of the rewrite function
(markdown-link `.md` → rewritten; bare `./x.md` → rewritten; `https://…/y.md`
and `z.txt` → untouched) if a frontend test harness is present; otherwise
verified manually in the Team tab.

## 7. Documentation

- `docs/design-docs/features/team.md`: new section "Markdown output viewer"
  describing the `/view` route, the linkify behavior, and the unauth-port
  caveat.
- `CLAUDE.md`: one line in the Team paragraph noting `/view` + `.md` linkify.
- Config/docs note that `/view` shares `team_bind`'s unauthenticated,
  localhost-only exposure.

## 8. Out of scope (future)

- katex math rendering on the `/view` page (app's Files preview has it).
- Phone-side viewing (the phone cannot reach `:8787`; would need the WS +
  in-app Files viewer path — a separate design).
- Watching the filesystem for produced files, or an agent "artifacts" list.
- Live reload of the `/view` page when the file changes.
