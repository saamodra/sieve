//! Turns one proxied exchange into a [`Flow`]: records the request and
//! response as they stream through and publishes the result.

use std::{
    mem,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
    time::Instant,
};

use bytes::{Bytes, BytesMut};
use http::{HeaderMap, Version, header};
use http_body::{Body as HttpBody, Frame, SizeHint};
use time::OffsetDateTime;

use super::{BoxError, decode::decode};
use crate::flow::{Body, Flow, FlowId, FlowKind, FlowSender, Request, Response, Timing};

/// Shared by the request and response bodies of one exchange. The flow is
/// published when the response body ends, fails, or is dropped, and the
/// request body has ended too: a server can answer before the client has
/// finished sending (a 413 to an upload, an early 401), and the flow must
/// still carry the request body that was received.
pub(crate) struct Exchange {
    flows: FlowSender,
    started: Instant,
    max_body: usize,
    state: Mutex<State>,
}

struct State {
    flow: Flow,
    request_body: Captured,
    /// A request body is still being read.
    request_pending: bool,
    /// The response ended while the request body was pending.
    response_done: Option<Captured>,
    finished: bool,
}

pub(crate) struct RequestHead<'a> {
    pub method: &'a str,
    pub url: String,
    pub version: Version,
    pub headers: &'a HeaderMap,
}

impl Exchange {
    pub(crate) fn new(
        id: FlowId,
        client: SocketAddr,
        kind: FlowKind,
        head: RequestHead<'_>,
        flows: FlowSender,
        max_body: usize,
    ) -> Arc<Self> {
        let flow = Flow {
            id,
            started_at: OffsetDateTime::now_utc(),
            client: Some(client),
            kind,
            request: Request {
                method: head.method.to_string(),
                url: head.url,
                http_version: version_str(head.version),
                headers: header_pairs(head.headers),
                body: body_meta(head.headers),
            },
            response: None,
            error: None,
            timing: Timing::default(),
        };
        Arc::new(Self {
            flows,
            started: Instant::now(),
            max_body,
            state: Mutex::new(State {
                flow,
                request_body: Captured::default(),
                request_pending: false,
                response_done: None,
                finished: false,
            }),
        })
    }

    pub(crate) fn id(&self) -> FlowId {
        self.lock().flow.id
    }

    /// Publishes the flow as it stands, before the response arrives.
    pub(crate) fn emit_start(&self) {
        let flow = self.lock().flow.clone();
        let _ = self.flows.send(Arc::new(flow));
    }

    /// Replaces the recorded request head after a rule or breakpoint changed
    /// what is sent, and publishes the flow again.
    pub(crate) fn update_request(&self, method: &str, url: String, headers: &HeaderMap) {
        {
            let mut state = self.lock();
            let request = &mut state.flow.request;
            request.method = method.to_string();
            request.url = url;
            request.headers = header_pairs(headers);
            request.body = body_meta(headers);
        }
        self.emit_start();
    }

    pub(crate) fn set_response(&self, status: u16, version: Version, headers: &HeaderMap) {
        let wait = self.elapsed_ms();
        let mut state = self.lock();
        state.flow.response = Some(Response {
            status,
            http_version: version_str(version),
            headers: header_pairs(headers),
            body: body_meta(headers),
        });
        state.flow.timing.wait_ms = Some(wait);
    }

    /// Marks the request body as still being read, so the flow is held back
    /// until [`Exchange::request_body_done`].
    fn request_body_started(&self) {
        self.lock().request_pending = true;
    }

    pub(crate) fn request_body_done(&self, body: Captured, error: Option<String>) {
        let (flow, request_body, response_body) = {
            let mut state = self.lock();
            state.request_body = body;
            state.request_pending = false;
            // A body cut short after the response is complete is not an error
            // of the exchange: the server just did not want the rest.
            if let Some(error) = error
                && state.response_done.is_none()
                && !state.finished
            {
                state.flow.error.get_or_insert(error);
            }
            let Some(response_body) = state.response_done.take() else {
                return;
            };
            state.finished = true;
            (
                state.flow.clone(),
                mem::take(&mut state.request_body),
                response_body,
            )
        };
        self.publish(flow, request_body, Some(response_body));
    }

    /// Completes the flow and publishes it, once the request body has ended
    /// as well. Later calls are ignored.
    pub(crate) fn finish(&self, response_body: Option<Captured>, error: Option<String>) {
        let total = self.elapsed_ms();
        let (flow, request_body) = {
            let mut state = self.lock();
            if state.finished || state.response_done.is_some() {
                return;
            }
            state.flow.timing.total_ms = Some(total);
            if let Some(error) = error {
                state.flow.error = Some(error);
            }
            if state.request_pending {
                state.response_done = Some(response_body.unwrap_or_default());
                return;
            }
            state.finished = true;
            (state.flow.clone(), mem::take(&mut state.request_body))
        };
        self.publish(flow, request_body, response_body);
    }

    fn publish(&self, mut flow: Flow, request_body: Captured, response_body: Option<Captured>) {
        let flows = self.flows.clone();
        let limit = self.max_body;
        let publish = async move {
            fill_body(&mut flow.request.body, request_body, limit).await;
            if let (Some(res), Some(body)) = (flow.response.as_mut(), response_body) {
                fill_body(&mut res.body, body, limit).await;
            }
            let _ = flows.send(Arc::new(flow));
        };
        // Bodies can be dropped outside the runtime during shutdown; the flow is
        // lost then, which is fine because nobody is listening any more.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(publish);
        }
    }

    pub(crate) fn max_body(&self) -> usize {
        self.max_body
    }

    fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("exchange state poisoned")
    }
}

/// The part of a body that was kept, plus its full size.
#[derive(Debug, Default)]
pub(crate) struct Captured {
    buf: BytesMut,
    size: u64,
    truncated: bool,
}

impl Captured {
    /// A body that was counted but not kept, e.g. bytes through a tunnel.
    pub(crate) fn opaque(size: u64) -> Self {
        Self {
            buf: BytesMut::new(),
            size,
            truncated: size > 0,
        }
    }

    fn push(&mut self, data: &[u8], limit: usize) {
        self.size += data.len() as u64;
        let room = limit.saturating_sub(self.buf.len());
        if data.len() > room {
            self.truncated = true;
        }
        self.buf.extend_from_slice(&data[..data.len().min(room)]);
    }
}

async fn fill_body(body: &mut Body, captured: Captured, limit: usize) {
    body.raw = captured.buf.freeze();
    body.size = captured.size;
    body.truncated = captured.truncated;
    if let Some(encoding) = &body.content_encoding {
        body.decoded = decode(encoding, &body.raw, limit).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Request,
    Response,
}

/// Forwards a body frame by frame while copying up to `max_body` bytes of it.
pub(crate) struct CaptureBody<B: HttpBody> {
    inner: B,
    side: Side,
    exchange: Arc<Exchange>,
    captured: Captured,
    done: bool,
}

impl<B: HttpBody> CaptureBody<B> {
    pub(crate) fn new(inner: B, side: Side, exchange: Arc<Exchange>) -> Self {
        if side == Side::Request {
            exchange.request_body_started();
        }
        let mut body = Self {
            inner,
            side,
            exchange,
            captured: Captured::default(),
            done: false,
        };
        // Hyper never polls a body that is already at its end (GET, HEAD, 204).
        if body.inner.is_end_stream() {
            body.complete(None);
        }
        body
    }

    fn complete(&mut self, error: Option<String>) {
        self.done = true;
        let captured = mem::take(&mut self.captured);
        match self.side {
            Side::Request => self.exchange.request_body_done(captured, error),
            Side::Response => self.exchange.finish(Some(captured), error),
        }
    }
}

impl<B> HttpBody for CaptureBody<B>
where
    B: HttpBody<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    let limit = this.exchange.max_body();
                    this.captured.push(data, limit);
                }
                // Hyper stops polling once `is_end_stream` is true, so the last
                // frame may be the last call we get.
                if this.inner.is_end_stream() {
                    this.complete(None);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(e)) => {
                let e: BoxError = e.into();
                let message = match this.side {
                    Side::Request => format!("reading the request body failed: {e}"),
                    Side::Response => format!("reading the response body failed: {e}"),
                };
                this.complete(Some(message));
                Poll::Ready(Some(Err(e)))
            }
            None => {
                this.complete(None);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B: HttpBody> Drop for CaptureBody<B> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let error = (!self.inner.is_end_stream()).then(|| match self.side {
            Side::Request => "the request body was not sent completely".to_string(),
            Side::Response => {
                "the client closed the connection before the response completed".to_string()
            }
        });
        self.complete(error);
    }
}

pub(crate) fn version_str(version: Version) -> String {
    format!("{version:?}")
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn body_meta(headers: &HeaderMap) -> Body {
    let get = |name| {
        headers
            .get(name)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Body {
        content_type: get(header::CONTENT_TYPE),
        content_encoding: get(header::CONTENT_ENCODING),
        ..Body::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_keeps_at_most_the_limit_but_counts_everything() {
        let mut c = Captured::default();
        c.push(b"hello", 8);
        assert!(!c.truncated);
        c.push(b"world", 8);
        assert_eq!(&c.buf[..], b"hellowor");
        assert_eq!(c.size, 10);
        assert!(c.truncated);
        c.push(b"!", 8);
        assert_eq!(c.size, 11);
        assert_eq!(c.buf.len(), 8);
    }

    #[test]
    fn captured_exactly_at_the_limit_is_not_truncated() {
        let mut c = Captured::default();
        c.push(b"12345678", 8);
        assert!(!c.truncated);
    }
}
