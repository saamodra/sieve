//! A [`Flow`] is one intercepted request/response exchange.
//!
//! The proxy publishes flows on a [`FlowSender`]. The same flow id may be sent
//! more than once as it progresses (request seen, response complete, error), so
//! consumers upsert by [`Flow::id`].

use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::broadcast;

pub type FlowId = u64;
pub type FlowSender = broadcast::Sender<Arc<Flow>>;
pub type FlowReceiver = broadcast::Receiver<Arc<Flow>>;

const CHANNEL_CAPACITY: usize = 4096;

pub fn channel() -> FlowSender {
    broadcast::channel(CHANNEL_CAPACITY).0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub id: FlowId,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// Peer address of the device that made the request.
    pub client: Option<SocketAddr>,
    pub kind: FlowKind,
    pub request: Request,
    pub response: Option<Response>,
    /// Set when the exchange failed, e.g. the client rejected our certificate.
    pub error: Option<String>,
    pub timing: Timing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    Http,
    WebSocket,
    /// Tunneled without interception; only the CONNECT target is known.
    Passthrough,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub http_version: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: u16,
    pub http_version: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

/// A captured body. The bytes are not serialized; the UI decides how to encode
/// them for the API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Body {
    /// Bytes as they went over the wire, capped at `max_body_bytes`.
    #[serde(skip)]
    pub raw: Bytes,
    /// `raw` with any `Content-Encoding` (gzip, br, zstd) removed, for display.
    #[serde(skip)]
    pub decoded: Option<Bytes>,
    /// Full size on the wire, including any part that was not kept.
    pub size: u64,
    pub truncated: bool,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timing {
    /// Request start to first response byte.
    pub wait_ms: Option<f64>,
    /// Request start to last response byte.
    pub total_ms: Option<f64>,
}
