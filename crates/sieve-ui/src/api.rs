//! REST and WebSocket handlers under `/api`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sieve_core::{
    Flow, FlowSender,
    config::DEFAULT_MAX_BODY_BYTES,
    flow::{FlowId, FlowKind},
    proxy::replay,
    rules::{Decision, HeldRequest, RequestEdit, Rule, RuleEngine, breakpoint::Outcome},
};
use time::OffsetDateTime;
use tokio::sync::broadcast;

use crate::{
    diff, har_import,
    model::{self, FlowSummary},
    store::{FlowFilter, FlowStore},
};

const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 5_000;
const REPLAY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HAR_BYTES: usize = 256 * 1024 * 1024;

/// Flows created by the UI (replays, HAR imports) get ids from here upwards,
/// far above the proxy's counter, so the two never collide.
pub const SYNTHETIC_ID_BASE: u64 = 1 << 48;

/// Message pushed over `/api/ws`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    /// A flow was added or updated.
    Flow { flow: Box<FlowSummary> },
    /// All flows were cleared.
    Clear,
    /// The subscriber missed events; refetch the list.
    Resync,
    /// A request is held at a breakpoint.
    Breakpoint { request: HeldRequest },
    /// A held request continued, was aborted, or timed out.
    BreakpointReleased { id: FlowId, outcome: Outcome },
}

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<FlowStore>,
    pub events: broadcast::Sender<WsEvent>,
    pub rules: Option<Arc<RuleEngine>>,
    /// Where replayed and imported flows are published, so they reach the
    /// store and the live feed like any captured flow.
    pub flows: FlowSender,
    pub next_id: Arc<AtomicU64>,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/flows", get(list_flows).delete(clear_flows))
        .route("/api/flows/{id}", get(get_flow))
        .route("/api/flows/{id}/har", get(flow_har))
        .route("/api/flows/{id}/curl", get(flow_curl))
        .route("/api/rules", get(get_rules).put(put_rules))
        .route("/api/breakpoints", get(list_breakpoints))
        .route("/api/breakpoints/{id}/continue", post(continue_breakpoint))
        .route("/api/breakpoints/{id}/abort", post(abort_breakpoint))
        .route("/api/flows/{id}/replay", post(flow_replay))
        .route("/api/diff", get(flow_diff))
        .route(
            "/api/har",
            get(export_har)
                .post(import_har)
                .layer(DefaultBodyLimit::max(MAX_HAR_BYTES)),
        )
        .route("/api/ws", get(ws))
        .route("/api/{*rest}", get(api_not_found))
        .with_state(state)
}

async fn api_not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    offset: Option<usize>,
    limit: Option<usize>,
    host: Option<String>,
    method: Option<String>,
    status: Option<String>,
    q: Option<String>,
    client: Option<String>,
}

async fn list_flows(State(st): State<ApiState>, Query(q): Query<ListQuery>) -> Json<Value> {
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let filter = FlowFilter {
        host: q.host,
        method: q.method,
        status: q.status,
        q: q.q,
        client: q.client,
    };
    let (total, flows) = st.store.list(&filter, offset, limit);
    Json(json!({"total": total, "offset": offset, "limit": limit, "flows": flows}))
}

fn not_found(id: FlowId) -> Response {
    let body = json!({"error": format!("flow {id} not found")});
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

async fn get_flow(State(st): State<ApiState>, Path(id): Path<FlowId>) -> Response {
    match st.store.get(id) {
        Some(f) => Json(model::detail(&f)).into_response(),
        None => not_found(id),
    }
}

async fn clear_flows(State(st): State<ApiState>) -> StatusCode {
    st.store.clear();
    // No subscribers is fine.
    let _ = st.events.send(WsEvent::Clear);
    StatusCode::NO_CONTENT
}

async fn flow_har(State(st): State<ApiState>, Path(id): Path<FlowId>) -> Response {
    let Some(f) = st.store.get(id) else {
        return not_found(id);
    };
    (
        [(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"sieve-{id}.har\""),
        )],
        Json(model::har([f.as_ref()])),
    )
        .into_response()
}

async fn flow_curl(State(st): State<ApiState>, Path(id): Path<FlowId>) -> Response {
    match st.store.get(id) {
        Some(f) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            model::curl(&f),
        )
            .into_response(),
        None => not_found(id),
    }
}

fn rules_disabled() -> Response {
    let body = json!({"error": "rules are not enabled in this session"});
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

async fn get_rules(State(st): State<ApiState>) -> Response {
    let Some(engine) = &st.rules else {
        return rules_disabled();
    };
    Json(json!({
        "rules": engine.rules(),
        "path": engine.path().map(|p| p.display().to_string()),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RulesBody {
    rules: Vec<Rule>,
}

async fn put_rules(State(st): State<ApiState>, Json(body): Json<RulesBody>) -> Response {
    let Some(engine) = &st.rules else {
        return rules_disabled();
    };
    match engine.replace(body.rules) {
        Ok(()) => Json(json!({"rules": engine.rules()})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("{e:#}")})),
        )
            .into_response(),
    }
}

async fn list_breakpoints(State(st): State<ApiState>) -> Response {
    let Some(engine) = &st.rules else {
        return rules_disabled();
    };
    let breakpoints = engine.breakpoints();
    Json(json!({
        "held": breakpoints.pending(),
        "timeout_secs": breakpoints.timeout().as_secs(),
    }))
    .into_response()
}

fn resolve(st: &ApiState, id: FlowId, decision: Decision) -> Response {
    let Some(engine) = &st.rules else {
        return rules_disabled();
    };
    if engine.breakpoints().resolve(id, decision) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        let body = json!({"error": format!("request {id} is not held (it may have timed out)")});
        (StatusCode::NOT_FOUND, Json(body)).into_response()
    }
}

async fn continue_breakpoint(
    State(st): State<ApiState>,
    Path(id): Path<FlowId>,
    edit: Option<Json<RequestEdit>>,
) -> Response {
    let edit = edit.map(|Json(e)| e).unwrap_or_default();
    resolve(&st, id, Decision::Continue(edit))
}

async fn abort_breakpoint(State(st): State<ApiState>, Path(id): Path<FlowId>) -> Response {
    resolve(&st, id, Decision::Abort)
}

/// HAR of every flow matching the list filters (no paging).
async fn export_har(State(st): State<ApiState>, Query(q): Query<ListQuery>) -> Response {
    let filter = FlowFilter {
        host: q.host,
        method: q.method,
        status: q.status,
        q: q.q,
        client: q.client,
    };
    let flows = st.store.matching(&filter);
    (
        [(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"sieve.har\"",
        )],
        Json(model::har(flows.iter().map(|f| f.as_ref()))),
    )
        .into_response()
}

fn bad_request(msg: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": msg.to_string()})),
    )
        .into_response()
}

/// Adds the entries of an uploaded HAR file as new flows.
async fn import_har(State(st): State<ApiState>, Json(doc): Json<Value>) -> Response {
    let ids = st.next_id.clone();
    let flows = match har_import::parse(&doc, || ids.fetch_add(1, Ordering::Relaxed)) {
        Ok(f) => f,
        Err(e) => return bad_request(format!("{e:#}")),
    };
    let new_ids: Vec<FlowId> = flows.iter().map(|f| f.id).collect();
    for f in flows {
        // No subscribers is fine.
        let _ = st.flows.send(Arc::new(f));
    }
    Json(json!({"imported": new_ids.len(), "ids": new_ids})).into_response()
}

/// Re-sends a flow's request and records the result as a new flow.
async fn flow_replay(State(st): State<ApiState>, Path(id): Path<FlowId>) -> Response {
    let Some(orig) = st.store.get(id) else {
        return not_found(id);
    };
    if orig.kind != FlowKind::Http {
        return bad_request("only plain HTTP flows can be replayed");
    }
    let started_at = OffsetDateTime::now_utc();
    let out = match replay::replay(&orig.request, REPLAY_TIMEOUT, DEFAULT_MAX_BODY_BYTES).await {
        Ok(o) => o,
        Err(e) => {
            let body = json!({"error": format!("{e:#}")});
            return (StatusCode::BAD_GATEWAY, Json(body)).into_response();
        }
    };
    let new_id = st.next_id.fetch_add(1, Ordering::Relaxed);
    let flow = Flow {
        id: new_id,
        started_at,
        client: None,
        kind: FlowKind::Http,
        request: orig.request.clone(),
        response: Some(out.response),
        error: None,
        timing: out.timing,
    };
    let _ = st.flows.send(Arc::new(flow));
    Json(json!({"id": new_id, "replayed_from": id})).into_response()
}

#[derive(Debug, Deserialize)]
struct DiffQuery {
    a: FlowId,
    b: FlowId,
}

/// Compares the responses of two flows.
async fn flow_diff(State(st): State<ApiState>, Query(q): Query<DiffQuery>) -> Response {
    let Some(a) = st.store.get(q.a) else {
        return not_found(q.a);
    };
    let Some(b) = st.store.get(q.b) else {
        return not_found(q.b);
    };
    match diff::responses(&a, &b) {
        Some(d) => Json(d).into_response(),
        None => bad_request("both flows need a response to be compared"),
    }
}

async fn ws(State(st): State<ApiState>, upgrade: WebSocketUpgrade) -> Response {
    // Subscribe before the upgrade completes so nothing sent after the
    // handshake is missed.
    let rx = st.events.subscribe();
    upgrade.on_upgrade(move |socket| ws_session(socket, rx))
}

async fn ws_session(mut socket: WebSocket, mut rx: broadcast::Receiver<WsEvent>) {
    loop {
        tokio::select! {
            event = rx.recv() => {
                let event = match event {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(_)) => WsEvent::Resync,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Ok(text) = serde_json::to_string(&event) else { continue };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
}
