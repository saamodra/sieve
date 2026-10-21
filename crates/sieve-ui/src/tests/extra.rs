//! Tests for HAR import/export, replay, and diff.

use super::*;

async fn post_json(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn har_export_all_respects_filters() {
    let h = loopback();
    send(
        &h,
        [
            flow(1, "GET", "https://a.test/1", Some(200)),
            flow(2, "POST", "https://b.test/2", Some(500)),
        ],
    )
    .await;
    let all = get_json(&h.app, "/api/har").await;
    assert_eq!(all["log"]["entries"].as_array().unwrap().len(), 2);
    let posts = get_json(&h.app, "/api/har?method=POST").await;
    let entries = posts["log"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["request"]["url"], "https://b.test/2");
}

#[tokio::test]
async fn har_roundtrips_through_import() {
    let src = loopback();
    let mut f = flow(1, "POST", "https://a.test/p?x=1", Some(201));
    f.request.body.raw = Bytes::from_static(b"payload");
    f.request.body.content_type = Some("text/plain".into());
    f.started_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    send(&src, [f, flow(2, "GET", "https://a.test/err", None)]).await;
    let har = get_json(&src.app, "/api/har").await;

    let dst = loopback();
    let (status, out) = post_json(&dst.app, "/api/har", &har).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["imported"], 2);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let list = get_json(&dst.app, "/api/flows").await;
    assert_eq!(list["total"], 2);
    let id = list["flows"][0]["id"].as_u64().unwrap();
    assert!(id >= crate::api::SYNTHETIC_ID_BASE);
    let d = get_json(&dst.app, &format!("/api/flows/{id}")).await;
    assert_eq!(d["request"]["method"], "POST");
    assert_eq!(d["request"]["body"]["text"], "payload");
    assert_eq!(d["response"]["status"], 201);
    assert_eq!(d["response"]["body"]["text"], "{\"ok\":true}");
    assert_eq!(d["summary"]["started_at"], "2023-11-14T22:13:20Z");
    assert_eq!(d["timing"]["wait_ms"], 10.0);
    assert!(list["flows"][1]["status"].is_null());
}

#[tokio::test]
async fn har_import_rejects_garbage() {
    let h = loopback();
    let (status, out) = post_json(&h.app, "/api/har", &serde_json::json!({"nope": 1})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(out["error"].as_str().unwrap().contains("log.entries"));
    let bad = serde_json::json!({"log": {"entries": [{"request": {}}]}});
    let (status, out) = post_json(&h.app, "/api/har", &bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(out["error"].as_str().unwrap().contains("HAR entry 0"));
}

#[tokio::test]
async fn diff_of_two_responses() {
    let h = loopback();
    let mut a = flow(1, "GET", "https://a.test/", Some(200));
    let mut b = flow(2, "GET", "https://a.test/", Some(404));
    a.response.as_mut().unwrap().body.raw = Bytes::from_static(b"one\ntwo\nthree\n");
    b.response.as_mut().unwrap().body.raw = Bytes::from_static(b"one\n2\nthree\n");
    b.response
        .as_mut()
        .unwrap()
        .headers
        .push(("X-New".into(), "1".into()));
    send(&h, [a, b, flow(3, "GET", "https://a.test/", None)]).await;

    let d = get_json(&h.app, "/api/diff?a=1&b=2").await;
    assert_eq!(d["status"]["changed"], true);
    assert_eq!(d["headers"]["added"][0]["name"], "x-new");
    assert_eq!(d["body"]["identical"], false);
    let ops: Vec<_> = d["body"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            format!(
                "{}:{}",
                l["op"].as_str().unwrap(),
                l["text"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(ops, ["equal:one", "del:two", "add:2", "equal:three"]);

    let (status, _) = call(&h.app, "GET", "/api/diff?a=1&b=3").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&h.app, "GET", "/api/diff?a=1&b=99").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replay_creates_a_new_flow() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = s.read(&mut buf).await.unwrap();
        s.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew")
            .await
            .unwrap();
    });

    let h = loopback();
    send(&h, [flow(1, "GET", &format!("http://{addr}/r"), Some(200))]).await;
    let (status, out) = post_json(&h.app, "/api/flows/1/replay", &Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let new_id = out["id"].as_u64().unwrap();
    assert_ne!(new_id, 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let d = get_json(&h.app, &format!("/api/flows/{new_id}")).await;
    assert_eq!(d["response"]["status"], 202);
    assert_eq!(d["response"]["body"]["text"], "new");
    assert_eq!(d["request"]["url"], format!("http://{addr}/r"));

    let (status, _) = post_json(&h.app, "/api/flows/77/replay", &Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
