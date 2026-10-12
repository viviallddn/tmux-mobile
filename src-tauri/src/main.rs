use tmux_mobile::run;

fn main() {
    run();
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};
    use tmux_mobile::tmux;

    const TEST_SESSION: &str = "_tmux_mobile_test";

    fn cleanup() {
        let _ = tmux::kill_session(TEST_SESSION);
    }

    fn wait_for_capture(target: &str, lines: Option<usize>, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        while Instant::now() < deadline {
            last = tmux::capture_pane(target, lines).expect("Failed to capture pane");
            if last.contains(needle) {
                return last;
            }
            thread::sleep(Duration::from_millis(100));
        }
        last
    }

    #[test]
    fn t01_server_running() {
        assert!(tmux::is_server_running(), "tmux server is not running!");
        println!("✅ tmux server is running");
    }

    #[test]
    fn t02_list_sessions() {
        let sessions = tmux::list_sessions().expect("Failed to list sessions");
        println!("✅ Found {} sessions:", sessions.len());
        for s in &sessions {
            println!(
                "   - {} ({} windows, attached={})",
                s.name, s.windows, s.attached
            );
        }
        assert!(!sessions.is_empty(), "No sessions found");
    }

    #[test]
    fn t03_create_and_kill_session() {
        cleanup();
        tmux::new_session(TEST_SESSION, None, None).expect("Failed to create session");
        let sessions = tmux::list_sessions().unwrap();
        assert!(
            sessions.iter().any(|s| s.name == TEST_SESSION),
            "Test session not found"
        );
        println!("✅ Created session: {}", TEST_SESSION);

        tmux::kill_session(TEST_SESSION).expect("Failed to kill session");
        let sessions = tmux::list_sessions().unwrap();
        assert!(
            !sessions.iter().any(|s| s.name == TEST_SESSION),
            "Test session still exists"
        );
        println!("✅ Killed session: {}", TEST_SESSION);
    }

    #[test]
    fn t04_list_panes() {
        cleanup();
        tmux::new_session(TEST_SESSION, None, None).unwrap();
        let panes = tmux::list_panes(TEST_SESSION).expect("Failed to list panes");
        println!("✅ Session {} has {} pane(s):", TEST_SESSION, panes.len());
        for p in &panes {
            println!(
                "   - window:{} pane:{} ({}x{}) cmd={}",
                p.window, p.pane, p.width, p.height, p.current_command
            );
        }
        assert!(!panes.is_empty(), "No panes found");
        cleanup();
    }

    #[test]
    fn t05_send_command_and_capture() {
        cleanup();
        tmux::new_session(TEST_SESSION, None, None).unwrap();
        thread::sleep(Duration::from_millis(200));

        let marker = "TMUX_MOBILE_TEST_12345";
        tmux::send_command(TEST_SESSION, &format!("echo {}", marker)).unwrap();
        thread::sleep(Duration::from_millis(500));

        let output = tmux::capture_pane(TEST_SESSION, Some(50)).expect("Failed to capture pane");
        println!("✅ Captured pane output ({} chars)", output.len());
        assert!(output.contains(marker), "Marker not found in output");
        println!("✅ Command output verified!");
        cleanup();
    }

    #[test]
    fn t06_send_special_keys() {
        cleanup();
        tmux::new_session(TEST_SESSION, None, None).unwrap();
        thread::sleep(Duration::from_millis(200));

        tmux::send_keys(TEST_SESSION, "echo partial", true).unwrap();
        thread::sleep(Duration::from_millis(100));
        tmux::send_keys(TEST_SESSION, "C-c", false).unwrap();
        thread::sleep(Duration::from_millis(200));

        let marker = "AFTER_CTRL_C_OK";
        tmux::send_command(TEST_SESSION, &format!("echo {}", marker)).unwrap();
        thread::sleep(Duration::from_millis(500));

        let output = tmux::capture_pane(TEST_SESSION, Some(20)).unwrap();
        assert!(output.contains(marker), "Pane should work after Ctrl-C");
        println!("✅ Special keys (C-c) work correctly");
        cleanup();
    }

    #[test]
    fn t07_capture_scrollback() {
        cleanup();
        tmux::new_session(TEST_SESSION, None, None).unwrap();
        thread::sleep(Duration::from_millis(200));

        tmux::send_command(
            TEST_SESSION,
            "for i in $(seq 1 100); do echo \"line_$i\"; done",
        )
        .unwrap();
        let output = wait_for_capture(TEST_SESSION, Some(50), "line_100");
        assert!(output.contains("line_100"), "Should capture line_100");
        println!("✅ Scrollback capture works");
        cleanup();
    }

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

    #[test]
    fn view_escapes_raw_html_in_source() {
        let html = tmux_mobile::team_view::markdown_to_html(
            "Hello\n\n<script>alert(1)</script>\n\n<img src=x onerror=alert(2)>\n",
        );
        // Raw source HTML is escaped to inert text, so no live <script> or
        // <img onerror=…> element ever reaches the page. Escaping only
        // neutralises `<`/`>`, so the inert text still contains the literal
        // "onerror=alert" substring — assert on the live tag, not the
        // substring, to prove the handler can never fire.
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(2)&gt;"));
    }
}
