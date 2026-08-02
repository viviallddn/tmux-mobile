//! HTTP-level integration test for the web layer: the human `/api` surface and
//! the dashboard route, exercised directly against the axum router.

use agora::bus::Bus;
use agora::store;
use agora::web;
use axum::body::{to_bytes, Body};
use http::{Request, StatusCode};
use tower::ServiceExt;

fn app() -> axum::Router {
    let conn = store::open_in_memory().unwrap();
    let bus = Bus::new(conn, "main");
    web::router_single(bus)
}

#[tokio::test]
async fn human_api_round_trip() {
    let app = app();

    // Dashboard serves HTML.
    let html_resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(html_resp.status(), StatusCode::OK);
    let html = to_bytes(html_resp.into_body(), 64 * 1024).await.unwrap();
    let html = String::from_utf8(html.to_vec()).unwrap();
    assert!(html.contains("agora"), "dashboard should render");

    // Human posts a directed message.
    let posted_body = serde_json::to_vec(&serde_json::json!({
        "from": "human", "to": ["architect"], "body": "design the schema"
    }))
    .unwrap();
    let posted_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/post")
                .header("content-type", "application/json")
                .body(Body::from(posted_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(posted_resp.status(), StatusCode::OK);
    let posted_body = to_bytes(posted_resp.into_body(), 64 * 1024).await.unwrap();
    let posted: serde_json::Value = serde_json::from_slice(&posted_body).unwrap();
    assert_eq!(posted["from"], "human");
    assert_eq!(posted["kind"], "msg");
    assert_eq!(posted["seq"], 1);

    // The roster lists connected agents only; the human operator (posting via the
    // dashboard API) is not registered as an agent.
    let roster_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/roster")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(roster_resp.status(), StatusCode::OK);
    let roster_body = to_bytes(roster_resp.into_body(), 64 * 1024).await.unwrap();
    let roster: Vec<serde_json::Value> = serde_json::from_slice(&roster_body).unwrap();
    assert!(
        !roster.iter().any(|a| a["name"] == "human"),
        "human must not appear in the roster"
    );

    // The directed task is unanswered -> system is active (a worker owes a reply,
    // but here only the human exists, so it shows active rather than done).
    let q_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/quiescence")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(q_resp.status(), StatusCode::OK);
    let q_body = to_bytes(q_resp.into_body(), 64 * 1024).await.unwrap();
    let q: serde_json::Value = serde_json::from_slice(&q_body).unwrap();
    assert!(q["state"].is_string());

    // History returns the posted message.
    let history_resp = app
        .oneshot(
            Request::builder()
                .uri("/api/history?limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(history_resp.status(), StatusCode::OK);
    let history_body = to_bytes(history_resp.into_body(), 64 * 1024).await.unwrap();
    let history: Vec<serde_json::Value> = serde_json::from_slice(&history_body).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["body"], "design the schema");
}

#[tokio::test]
async fn agent_shell_api_joins_waits_and_posts_without_json_encoding() {
    let conn = store::open_in_memory().unwrap();
    let bus = Bus::new(conn, "main");
    let app = web::router_single(bus);

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
    assert_eq!(wait_json["roster"][0]["name"], "manager");
    assert_eq!(wait_json["roster"][0]["role"], "tech lead");

    let compact_req = Request::builder()
        .uri("/api/agent/wait?timeout_ms=1&roster=compact")
        .header("x-agent", "manager")
        .header("x-room", "main")
        .header("x-agent-role", "tech lead")
        .body(Body::empty())
        .unwrap();
    let compact_resp = app.clone().oneshot(compact_req).await.unwrap();
    assert_eq!(compact_resp.status(), StatusCode::OK);
    let compact_body = to_bytes(compact_resp.into_body(), 64 * 1024).await.unwrap();
    let compact_json: serde_json::Value = serde_json::from_slice(&compact_body).unwrap();
    assert_eq!(compact_json["outcome"], "idle");
    assert_eq!(
        compact_json["roster"],
        serde_json::json!([{ "name": "manager", "status": "idle" }])
    );

    let no_roster_req = Request::builder()
        .uri("/api/agent/wait?timeout_ms=1&roster=none")
        .header("x-agent", "manager")
        .header("x-room", "main")
        .header("x-agent-role", "tech lead")
        .body(Body::empty())
        .unwrap();
    let no_roster_resp = app.clone().oneshot(no_roster_req).await.unwrap();
    assert_eq!(no_roster_resp.status(), StatusCode::OK);
    let no_roster_body = to_bytes(no_roster_resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let no_roster_json: serde_json::Value = serde_json::from_slice(&no_roster_body).unwrap();
    assert_eq!(no_roster_json["outcome"], "idle");
    assert!(no_roster_json.get("roster").is_none());

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
