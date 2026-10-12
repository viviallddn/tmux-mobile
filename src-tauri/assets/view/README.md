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
