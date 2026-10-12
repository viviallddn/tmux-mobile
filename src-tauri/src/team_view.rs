//! Desktop-only Markdown viewer served on the Team daemon's HTTP port.
//!
//! Adds `GET /view?room=&path=` (render a workspace `.md` into a styled page)
//! onto the agora axum router. `room → workspace` is resolved from the live
//! `TeamManager`, and the client only ever passes a RELATIVE path — so this
//! unauthenticated, localhost-only port can never read outside a team's
//! workspace. Raw HTML in the source markdown is escaped to inert text, and
//! client-side highlight.js + mermaid assets are embedded and served from
//! `GET /view/assets/<name>`.

use crate::team_bridge::TeamManager;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::path::{Component, Path};
use std::sync::Arc;

// Vendored client assets (see assets/view/README.md for versions/sources).
const HLJS_JS: &str = include_str!("../assets/view/highlight.min.js");
const HLJS_CSS: &str = include_str!("../assets/view/github-dark.min.css");
const MERMAID_JS: &str = include_str!("../assets/view/mermaid.min.js");

/// `(body, content-type)` for a known asset name, else `None`.
pub fn asset_lookup(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "highlight.min.js" => Some((HLJS_JS, "text/javascript; charset=utf-8")),
        "github-dark.min.css" => Some((HLJS_CSS, "text/css; charset=utf-8")),
        "mermaid.min.js" => Some((MERMAID_JS, "text/javascript; charset=utf-8")),
        _ => None,
    }
}

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
        .route("/view/assets/{name}", get(asset))
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

async fn asset(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match asset_lookup(&name) {
        Some((body, ct)) => ([(axum::http::header::CONTENT_TYPE, ct)], body).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown asset").into_response(),
    }
}

/// Validate `rel` under `workspace`, read it, and render a full HTML page.
/// Never panics and never reads outside `workspace`; every failure is a status.
pub fn render_markdown_file(workspace: &str, rel: &str) -> Result<String, StatusCode> {
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
pub fn markdown_to_html(src: &str) -> String {
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
            // Agent-produced markdown is untrusted and shares the localhost
            // origin with the unauthenticated team dashboard/MCP API. Render
            // raw HTML from the SOURCE as inert escaped text. (Our highlight/
            // mermaid scripts are injected by render_page, not here.)
            Event::Html(h) | Event::InlineHtml(h) if !in_code => {
                transformed.push(Event::Text(h));
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
