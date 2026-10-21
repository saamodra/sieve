use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sieve_core::{
    UiConfig, flow,
    rules::{Decision, HeldRequest, RuleEngine},
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::build_with_rules;

fn app(rules: Option<Arc<RuleEngine>>) -> (Router, CancellationToken) {
    let shutdown = CancellationToken::new();
    let app = build_with_rules(
        &UiConfig::default(),
        &flow::channel(),
        Router::new(),
        &shutdown,
        rules,
    )
    .unwrap();
    (app, shutdown)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn rules_are_unavailable_without_an_engine() {
    let (app, shutdown) = app(None);
    let (status, _) = call(&app, "GET", "/api/rules", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    shutdown.cancel();
}

#[tokio::test]
async fn rules_round_trip_through_the_api_and_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rules.json");
    let engine = RuleEngine::new(Some(path.clone()));
    let (app, shutdown) = app(Some(engine.clone()));

    let body = json!({"rules": [{"name": "block ads", "match": {"host": "ads.*"}, "action": {"block": true}}]});
    let (status, saved) = call(&app, "PUT", "/api/rules", Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["rules"][0]["id"], "rule-1");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("block ads")
    );

    let (_, listed) = call(&app, "GET", "/api/rules", None).await;
    assert_eq!(listed["rules"][0]["action"]["block"], true);
    assert_eq!(listed["rules"][0]["enabled"], true);

    let bad = json!({"rules": [{"match": {"path_regex": "("}}]});
    let (status, err) = call(&app, "PUT", "/api/rules", Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"].as_str().unwrap().contains("path_regex"));
    assert_eq!(engine.rules().len(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn breakpoints_can_be_listed_and_resolved() {
    let engine = RuleEngine::new(None);
    let (app, shutdown) = app(Some(engine.clone()));

    let held = HeldRequest {
        id: 5,
        method: "GET".into(),
        url: "http://a.test/".into(),
        headers: vec![],
        body: Some(String::new()),
        body_size: 0,
        timeout_secs: 0,
    };
    let waiting = tokio::spawn({
        let engine = engine.clone();
        async move { engine.breakpoints().hold(held).await }
    });
    while engine.breakpoints().pending().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let (_, list) = call(&app, "GET", "/api/breakpoints", None).await;
    assert_eq!(list["held"][0]["id"], 5);
    assert_eq!(list["timeout_secs"], 60);

    let (status, _) = call(
        &app,
        "POST",
        "/api/breakpoints/5/continue",
        Some(json!({"method": "POST"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    match waiting.await.unwrap() {
        Decision::Continue(edit) => assert_eq!(edit.method.as_deref(), Some("POST")),
        Decision::Abort => panic!("expected continue"),
    }

    let (status, _) = call(&app, "POST", "/api/breakpoints/5/abort", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    shutdown.cancel();
}
