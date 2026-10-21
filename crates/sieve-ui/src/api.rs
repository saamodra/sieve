//! REST and WebSocket handlers under `/api`.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sieve_core::flow::FlowId;
use tokio::sync::broadcast;

use crate::{
    model::{self, FlowSummary},
    store::{FlowFilter, FlowStore},
};

const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 5_000;

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
}

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<FlowStore>,
    pub events: broadcast::Sender<WsEvent>,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/flows", get(list_flows).delete(clear_flows))
        .route("/api/flows/{id}", get(get_flow))
        .route("/api/flows/{id}/har", get(flow_har))
        .route("/api/flows/{id}/curl", get(flow_curl))
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
