//! Experiment: prove the stateless daemon answers a tool call with NO prior
//! `initialize` and NO session id — the exact shape a recovered agent sends
//! after a daemon restart (it still holds a now-dead session). With the old
//! stateful config this returned 401/422; stateless must return the result.
use agora::bus::Bus;
use agora::store;
use agora::web;
use axum::body::{to_bytes, Body};
use http::Request;
use tower::ServiceExt;

#[tokio::test]
async fn tool_call_without_init_or_session_succeeds() {
    let conn = store::open_in_memory().unwrap();
    let bus = Bus::new(conn, "main");
    let app = web::router_single(bus);

    // NO initialize handshake, NO Mcp-Session-Id header — straight tools/call.
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"list_agents","arguments":{}}
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("x-agent", "probe")
                .header("x-room", "main")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    eprintln!("STATUS={status}\nBODY={text}");
    assert!(
        status.is_success(),
        "stateless tool call should succeed, got {status}: {text}"
    );
    assert!(
        text.contains("Present:"),
        "should get a roster render back: {text}"
    );
}
