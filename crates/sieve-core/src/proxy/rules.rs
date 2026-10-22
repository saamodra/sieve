//! Applies [`crate::rules`] to proxied requests: block, mock, map remote,
//! header and body rewrites, delays, and breakpoints.

use std::{
    collections::VecDeque,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header, request::Parts, response,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use tracing::warn;

use super::{
    BoxError, Conn, ProxyBody,
    capture::{CaptureBody, Exchange, Side},
    decode::decode_all,
    strip_hop_by_hop, text_response, upstream_failed,
};
use crate::{
    flow,
    rules::{Decision, HeldRequest, Plan, RequestEdit, RequestInfo, RuleEngine},
    script::{RequestOutcome, ScriptEngine},
};

/// The plan for `req`, or `None` when no rule applies to it.
pub(super) fn plan_for(conn: &Conn, req: &Request<Incoming>, uri: &Uri) -> Option<Plan> {
    if conn.quiet {
        return None;
    }
    let engine = conn.shared.rules.as_ref()?;
    let host = uri.host()?;
    let port = uri.port_u16().or(match uri.scheme_str() {
        Some("https") => Some(443),
        Some("http") => Some(80),
        _ => None,
    });
    let plan = engine.plan(&RequestInfo {
        method: req.method().as_str(),
        host,
        port,
        path: uri.path(),
        headers: req.headers(),
    });
    (!plan.is_empty()).then_some(plan)
}

/// Forwards a request that matched at least one rule.
pub(super) async fn forward(
    conn: &Conn,
    mut parts: Parts,
    body: Incoming,
    exchange: Arc<Exchange>,
    client_version: Version,
    plan: Plan,
) -> Response<ProxyBody> {
    if plan.block || plan.mock.is_some() {
        // Read the request so the flow records it.
        let _ = CaptureBody::new(body, Side::Request, exchange.clone())
            .collect()
            .await;
        return synthetic(&plan, exchange, client_version).await;
    }

    let mut changed =
        !plan.set_request_headers.is_empty() || !plan.remove_request_headers.is_empty();
    apply_headers(
        &mut parts.headers,
        &plan.set_request_headers,
        &plan.remove_request_headers,
    );
    if let Some(target) = &plan.map_remote {
        match remap(&parts.uri, target) {
            Some(uri) => {
                parts.uri = uri;
                changed = true;
            }
            None => {
                warn!("map_remote {target:?} is not a usable address; sending the request as is")
            }
        }
    }

    let body: ProxyBody = match (&conn.shared.rules, plan.breakpoint) {
        (Some(engine), true) => match hold_request(engine, &exchange, &mut parts, body).await {
            Ok(body) => {
                changed = true;
                body
            }
            Err(res) => return *res,
        },
        _ => CaptureBody::new(body, Side::Request, exchange.clone()).boxed_unsync(),
    };

    if changed {
        exchange.update_request(parts.method.as_str(), parts.uri.to_string(), &parts.headers);
    }

    match conn
        .shared
        .client
        .request(Request::from_parts(parts, body))
        .await
    {
        Ok(res) => {
            if !plan.delay.is_zero() {
                tokio::time::sleep(plan.delay).await;
            }
            super::respond(res, exchange, client_version, Some(&plan))
        }
        Err(e) => upstream_failed(&exchange, &e),
    }
}

/// Holds the request at a breakpoint and returns the body to send on, or the
/// response that ends the exchange when the user aborts or the body cannot be
/// read. Only bodies of at most `max_body` bytes are buffered and editable;
/// a larger one is held without its body and streams through unchanged.
async fn hold_request<B>(
    engine: &RuleEngine,
    exchange: &Arc<Exchange>,
    parts: &mut Parts,
    body: B,
) -> Result<ProxyBody, Box<Response<ProxyBody>>>
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    match buffer(body, exchange.max_body()).await {
        Ok(Buffered::Whole(mut bytes)) => {
            let held = HeldBody {
                text: std::str::from_utf8(&bytes).ok().map(str::to_string),
                size: bytes.len(),
            };
            match hold(engine, exchange, parts, held).await {
                Decision::Abort => {
                    let _ = CaptureBody::new(Full::new(bytes), Side::Request, exchange.clone())
                        .collect()
                        .await;
                    return Err(Box::new(aborted(exchange)));
                }
                Decision::Continue(edit) => apply_edit(parts, &mut bytes, edit),
            }
            if parts.headers.contains_key(header::CONTENT_LENGTH) || !bytes.is_empty() {
                parts
                    .headers
                    .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
            }
            Ok(CaptureBody::new(Full::new(bytes), Side::Request, exchange.clone()).boxed_unsync())
        }
        Ok(Buffered::Stream(body)) => {
            let held = HeldBody {
                text: None,
                size: usize::try_from(body.size_hint().lower()).unwrap_or(usize::MAX),
            };
            let body = CaptureBody::new(body, Side::Request, exchange.clone());
            match hold(engine, exchange, parts, held).await {
                Decision::Abort => {
                    drop(body);
                    Err(Box::new(aborted(exchange)))
                }
                Decision::Continue(mut edit) => {
                    // The body was not shown, so there is nothing to edit.
                    edit.body = None;
                    apply_edit(parts, &mut Bytes::new(), edit);
                    Ok(body.boxed_unsync())
                }
            }
        }
        Err(e) => {
            let e: BoxError = e.into();
            let message = format!("reading the request body failed: {e}");
            exchange.finish(None, Some(message.clone()));
            Err(Box::new(text_response(StatusCode::BAD_REQUEST, &message)))
        }
    }
}

fn aborted(exchange: &Exchange) -> Response<ProxyBody> {
    exchange.finish(None, Some("aborted at a breakpoint".to_string()));
    text_response(
        StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_GATEWAY),
        "request aborted at a breakpoint",
    )
}

/// What the user sees of the held request's body.
struct HeldBody {
    /// `None` when the body is not UTF-8 or was too large to buffer.
    text: Option<String>,
    size: usize,
}

async fn hold(engine: &RuleEngine, exchange: &Exchange, parts: &Parts, body: HeldBody) -> Decision {
    engine
        .breakpoints()
        .hold(HeldRequest {
            id: exchange.id(),
            method: parts.method.to_string(),
            url: parts.uri.to_string(),
            headers: header_pairs(&parts.headers),
            body: body.text,
            body_size: body.size,
            timeout_secs: 0,
        })
        .await
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(n, v)| {
            (
                n.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Applies what the user entered in the UI. Anything invalid is ignored so a
/// typo cannot wedge the request.
fn apply_edit(parts: &mut Parts, body: &mut Bytes, edit: RequestEdit) {
    if let Some(method) = edit.method
        && let Ok(method) = Method::from_bytes(method.trim().as_bytes())
    {
        parts.method = method;
    }
    if let Some(uri) = edit.url
        && let Ok(uri) = uri.trim().parse::<Uri>()
        && uri.scheme().is_some()
        && uri.authority().is_some()
    {
        parts.uri = uri;
    }
    if let Some(pairs) = edit.headers {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.trim().as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                headers.append(name, value);
            }
        }
        strip_hop_by_hop(&mut headers, false);
        headers.remove(header::HOST);
        parts.headers = headers;
    }
    if let Some(text) = edit.body {
        *body = Bytes::from(text);
    }
}

pub(super) fn apply_headers(headers: &mut HeaderMap, set: &[(String, String)], remove: &[String]) {
    for name in remove {
        if let Ok(name) = HeaderName::from_bytes(name.trim().as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Adds `list` after what is already in `headers`, keeping repeated names.
fn append_headers(headers: &mut HeaderMap, list: &[(String, String)]) {
    for (name, value) in list {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

/// Points `uri` at `target` (`host:port` or a URL), keeping the path and query.
pub(super) fn remap(uri: &Uri, target: &str) -> Option<Uri> {
    let target = target.trim();
    let target: Uri = if target.contains("://") {
        target.parse().ok()?
    } else {
        format!("http://{target}").parse().ok()?
    };
    let prefix = target.path().trim_end_matches('/');
    let rest = uri.path_and_query().map_or("/", |p| p.as_str());
    Uri::builder()
        .scheme(target.scheme()?.clone())
        .authority(target.authority()?.clone())
        .path_and_query(format!("{prefix}{rest}"))
        .build()
        .ok()
}

/// A response made by sieve: a rule's mock, or the `403` of a block.
pub(super) async fn synthetic(
    plan: &Plan,
    exchange: Arc<Exchange>,
    client_version: Version,
) -> Response<ProxyBody> {
    let mut headers = HeaderMap::new();
    let (status, body) = match (&plan.mock, plan.block) {
        (Some(mock), false) => {
            apply_headers(
                &mut headers,
                &mock.headers.clone().into_iter().collect::<Vec<_>>(),
                &[],
            );
            append_headers(&mut headers, &mock.header_list);
            match mock_body(mock, plan.base_dir.as_deref()).await {
                Ok(body) => (
                    StatusCode::from_u16(mock.status).unwrap_or(StatusCode::OK),
                    body,
                ),
                Err(message) => {
                    headers.clear();
                    (StatusCode::INTERNAL_SERVER_ERROR, Bytes::from(message))
                }
            }
        }
        _ => {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            (
                StatusCode::FORBIDDEN,
                Bytes::from_static(b"sieve: blocked by a rule\n"),
            )
        }
    };
    if !plan.delay.is_zero() {
        tokio::time::sleep(plan.delay).await;
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    exchange.set_response(status.as_u16(), Version::HTTP_11, &headers);
    let mut res =
        Response::new(CaptureBody::new(Full::new(body), Side::Response, exchange).boxed_unsync());
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    *res.version_mut() = client_version;
    res
}

async fn mock_body(mock: &crate::rules::Mock, base: Option<&Path>) -> Result<Bytes, String> {
    if let Some(file) = &mock.body_file {
        let path = match base {
            Some(base) if file.is_relative() => base.join(file),
            _ => file.clone(),
        };
        return tokio::fs::read(&path)
            .await
            .map(Bytes::from)
            .map_err(|e| format!("sieve: reading mock file {}: {e}\n", path.display()));
    }
    Ok(Bytes::from(mock.body.clone().unwrap_or_default()))
}

/// Applies the response edits of `plan`. Returns the replacement body, if the
/// plan replaces it.
pub(super) fn edit_response(parts: &mut response::Parts, plan: &Plan) -> Option<Bytes> {
    if let Some(status) = plan.response_status
        && let Ok(status) = StatusCode::from_u16(status)
    {
        parts.status = status;
    }
    apply_headers(
        &mut parts.headers,
        &plan.set_response_headers,
        &plan.remove_response_headers,
    );
    let body = Bytes::from(plan.response_body.clone()?);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.remove(header::TRANSFER_ENCODING);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    Some(body)
}

/// Runs `scripts` (rhai plugins, phase 5e) against a request that no
/// declarative rule already answered, then forwards it. Hooks only see bodies
/// of at most `max_body_bytes`; larger ones stream through untouched.
pub(super) async fn forward_with_scripts(
    conn: &Conn,
    mut parts: Parts,
    body: Incoming,
    exchange: Arc<Exchange>,
    client_version: Version,
    scripts: Arc<ScriptEngine>,
) -> Response<ProxyBody> {
    let limit = exchange.max_body();
    let (out_body, freq) = match buffer(body, limit).await {
        Ok(Buffered::Whole(mut bytes)) => {
            let mut freq =
                flow_request_from(&parts, script_body(&parts.headers, &bytes, limit).await);
            if freq.body.decoded.is_some() {
                let outcome = match scripts.on_request(&mut freq) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        warn!("script on_request failed, forwarding the request unmodified: {e:#}");
                        RequestOutcome::Forward
                    }
                };
                if let RequestOutcome::Respond(fres) = outcome {
                    let _ = CaptureBody::new(Full::new(bytes), Side::Request, exchange.clone())
                        .collect()
                        .await;
                    return script_response(&fres, exchange, client_version);
                }
                apply_flow_request(&freq, &mut parts, &mut bytes);
                exchange.update_request(
                    parts.method.as_str(),
                    parts.uri.to_string(),
                    &parts.headers,
                );
            } else {
                // `on_response` gets an empty body rather than undecodable bytes.
                freq.body = flow::Body::default();
            }
            let body = CaptureBody::new(Full::new(bytes), Side::Request, exchange.clone());
            (body.boxed_unsync(), freq)
        }
        Ok(Buffered::Stream(body)) => {
            let body = CaptureBody::new(body, Side::Request, exchange.clone());
            let freq = flow_request_from(&parts, flow::Body::default());
            (body.boxed_unsync(), freq)
        }
        Err(e) => {
            let message = format!("reading the request body failed: {e}");
            exchange.finish(None, Some(message.clone()));
            return text_response(StatusCode::BAD_REQUEST, &message);
        }
    };

    let res = match conn
        .shared
        .client
        .request(Request::from_parts(parts, out_body))
        .await
    {
        Ok(res) => res,
        Err(e) => return upstream_failed(&exchange, &e),
    };

    if !scripts.has_on_response() || is_event_stream(res.headers()) {
        return super::respond(res, exchange, client_version, None);
    }

    let (mut res_parts, res_body) = res.into_parts();
    let mut res_bytes = match buffer(res_body, limit).await {
        Ok(Buffered::Whole(bytes)) => bytes,
        Ok(Buffered::Stream(body)) => {
            return pass_response(res_parts, body, exchange, client_version);
        }
        Err(e) => {
            let message = format!("reading the response body failed: {e}");
            exchange.finish(None, Some(message.clone()));
            return text_response(StatusCode::BAD_GATEWAY, &message);
        }
    };
    let body = script_body(&res_parts.headers, &res_bytes, limit).await;
    let mut fres = flow_response_from(&res_parts, body);
    if fres.body.decoded.is_some() {
        if let Err(e) = scripts.on_response(&freq, &mut fres) {
            warn!("script on_response failed, leaving the response unmodified: {e:#}");
        }
        apply_flow_response(&fres, &mut res_parts, &mut res_bytes);
    }
    pass_response(res_parts, Full::new(res_bytes), exchange, client_version)
}

/// Passes an upstream response to the client after the `on_response` hooks.
fn pass_response<B>(
    mut parts: response::Parts,
    body: B,
    exchange: Arc<Exchange>,
    client_version: Version,
) -> Response<ProxyBody>
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    exchange.set_response(parts.status.as_u16(), parts.version, &parts.headers);
    strip_hop_by_hop(&mut parts.headers, false);
    parts.version = client_version;
    let body = CaptureBody::new(body, Side::Response, exchange).boxed_unsync();
    Response::from_parts(parts, body)
}

/// Server-sent events never end on their own, so they are never buffered.
fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// A body read ahead for a script hook.
enum Buffered<B> {
    /// The whole body, at most the limit.
    Whole(Bytes),
    /// A body to forward as is, because it outgrew the limit or carries
    /// trailers: what was read, then the rest.
    Stream(Replay<B>),
}

/// Reads `body` into memory if it is at most `limit` bytes.
async fn buffer<B>(mut body: B, limit: usize) -> Result<Buffered<B>, B::Error>
where
    B: HttpBody<Data = Bytes> + Unpin,
{
    let mut read = VecDeque::<Frame<Bytes>>::new();
    // A declared length over the limit needs no reading to rule out.
    if body.size_hint().lower() <= limit as u64 {
        let mut len = 0;
        loop {
            let Some(frame) = body.frame().await else {
                let mut bytes = BytesMut::with_capacity(len);
                for data in read.iter().filter_map(Frame::data_ref) {
                    bytes.extend_from_slice(data);
                }
                return Ok(Buffered::Whole(bytes.freeze()));
            };
            let frame = frame?;
            len += frame.data_ref().map_or(0, Bytes::len);
            let fits = frame.is_data() && len <= limit;
            read.push_back(frame);
            if !fits {
                break;
            }
        }
    }
    Ok(Buffered::Stream(Replay { read, rest: body }))
}

/// Plays back the frames [`buffer`] read, then the rest of the body.
struct Replay<B> {
    read: VecDeque<Frame<Bytes>>,
    rest: B,
}

impl<B> HttpBody for Replay<B>
where
    B: HttpBody<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = self.get_mut();
        match this.read.pop_front() {
            Some(frame) => Poll::Ready(Some(Ok(frame))),
            None => Pin::new(&mut this.rest).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.read.is_empty() && self.rest.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let read: u64 = self
            .read
            .iter()
            .filter_map(Frame::data_ref)
            .map(|d| d.len() as u64)
            .sum();
        let rest = self.rest.size_hint();
        let mut hint = SizeHint::new();
        hint.set_lower(rest.lower() + read);
        if let Some(upper) = rest.upper() {
            hint.set_upper(upper + read);
        }
        hint
    }
}

/// The body a script sees: `raw` as sent on the wire, plus `decoded` with any
/// `Content-Encoding` removed, which is what `body` holds in the script. A
/// `decoded` of `None` means `raw` did not decode completely within `limit`,
/// and the hook must be skipped so the original is forwarded unchanged.
async fn script_body(headers: &HeaderMap, raw: &Bytes, limit: usize) -> flow::Body {
    let encoding = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    flow::Body {
        raw: raw.clone(),
        decoded: decode_all(&encoding, raw, limit).await,
        size: raw.len() as u64,
        content_encoding: (!encoding.is_empty()).then_some(encoding),
        ..Default::default()
    }
}

/// Whether a hook wrote a new body: `set_body` in script.rs drops `decoded`
/// and puts the new text in `raw`. An untouched body keeps its encoded `raw`.
fn body_rewritten(body: &flow::Body) -> bool {
    body.decoded.is_none()
}

fn flow_request_from(parts: &Parts, body: flow::Body) -> flow::Request {
    flow::Request {
        method: parts.method.to_string(),
        url: parts.uri.to_string(),
        http_version: "HTTP/1.1".to_string(),
        headers: header_pairs(&parts.headers),
        body,
    }
}

fn flow_response_from(parts: &response::Parts, body: flow::Body) -> flow::Response {
    flow::Response {
        status: parts.status.as_u16(),
        http_version: "HTTP/1.1".to_string(),
        headers: header_pairs(&parts.headers),
        body,
    }
}

/// Copies a script's (possibly rewritten) request back into the parts and
/// body bytes that get forwarded. A rewritten body is plain text, so it loses
/// the original `Content-Encoding`.
fn apply_flow_request(freq: &flow::Request, parts: &mut Parts, bytes: &mut Bytes) {
    if let Ok(method) = Method::from_bytes(freq.method.as_bytes()) {
        parts.method = method;
    }
    if let Ok(uri) = freq.url.parse::<Uri>()
        && uri.scheme().is_some()
        && uri.authority().is_some()
    {
        parts.uri = uri;
    }
    parts.headers = headers_from_pairs(&freq.headers);
    *bytes = freq.body.raw.clone();
    if body_rewritten(&freq.body) {
        parts.headers.remove(header::CONTENT_ENCODING);
    }
    if parts.headers.contains_key(header::CONTENT_LENGTH) {
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    }
}

/// Copies a script's (possibly rewritten) response back into the parts and
/// body bytes returned to the client. An untouched body goes out as it came,
/// encoding and all; a rewritten one is plain text.
fn apply_flow_response(fres: &flow::Response, parts: &mut response::Parts, bytes: &mut Bytes) {
    if let Ok(status) = StatusCode::from_u16(fres.status) {
        parts.status = status;
    }
    parts.headers = headers_from_pairs(&fres.headers);
    parts.headers.remove(header::TRANSFER_ENCODING);
    if body_rewritten(&fres.body) {
        *bytes = fres.body.raw.clone();
        parts.headers.remove(header::CONTENT_ENCODING);
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    }
}

fn headers_from_pairs(pairs: &[(String, String)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    headers
}

/// A response made directly by a script's `respond`.
fn script_response(
    fres: &flow::Response,
    exchange: Arc<Exchange>,
    client_version: Version,
) -> Response<ProxyBody> {
    let mut headers = headers_from_pairs(&fres.headers);
    let status = StatusCode::from_u16(fres.status).unwrap_or(StatusCode::OK);
    let body = fres.body.raw.clone();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    exchange.set_response(status.as_u16(), Version::HTTP_11, &headers);
    let mut res =
        Response::new(CaptureBody::new(Full::new(body), Side::Response, exchange).boxed_unsync());
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    *res.version_mut() = client_version;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_keeps_path_and_query() {
        let uri: Uri = "https://api.prod.com/v1/users?id=3".parse().unwrap();
        assert_eq!(
            remap(&uri, "localhost:3000").unwrap().to_string(),
            "http://localhost:3000/v1/users?id=3"
        );
        assert_eq!(
            remap(&uri, "https://staging.test/base/")
                .unwrap()
                .to_string(),
            "https://staging.test/base/v1/users?id=3"
        );
        assert!(remap(&uri, "not a host").is_none());
    }

    #[tokio::test]
    async fn buffer_keeps_small_bodies_and_passes_the_rest_on_whole() {
        let body = Full::new(Bytes::from_static(b"abc"));
        let Buffered::Whole(bytes) = buffer(body, 3).await.unwrap() else {
            panic!("a body at the limit is buffered");
        };
        assert_eq!(bytes, "abc");

        let body = Full::new(Bytes::from_static(b"abcd"));
        let Buffered::Stream(body) = buffer(body, 3).await.unwrap() else {
            panic!("a body over the limit is streamed");
        };
        assert_eq!(body.collect().await.unwrap().to_bytes(), "abcd");

        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static("0"));
        let body = Full::new(Bytes::from_static(b"ab"))
            .with_trailers(std::future::ready(Some(Ok(trailers))));
        let Buffered::Stream(body) = buffer(body, 3).await.unwrap() else {
            panic!("a body with trailers is streamed");
        };
        let collected = body.collect().await.unwrap();
        assert_eq!(collected.trailers().unwrap()["grpc-status"], "0");
        assert_eq!(collected.to_bytes(), "ab");
    }

    #[test]
    fn edits_ignore_invalid_input() {
        let (mut parts, _) = Request::new(()).into_parts();
        parts.uri = "http://a.test/".parse().unwrap();
        let mut body = Bytes::from_static(b"old");
        apply_edit(
            &mut parts,
            &mut body,
            RequestEdit {
                method: Some("PO ST".into()),
                url: Some("/relative".into()),
                headers: Some(vec![
                    ("x-a".into(), "1".into()),
                    ("bad name".into(), "2".into()),
                ]),
                body: Some("new".into()),
            },
        );
        assert_eq!(parts.method, Method::GET);
        assert_eq!(parts.uri.to_string(), "http://a.test/");
        assert_eq!(parts.headers.len(), 1);
        assert_eq!(body, "new");
    }

    use std::net::SocketAddr;

    use crate::{
        flow::{self, FlowKind},
        proxy::capture::RequestHead,
        rules::Mock,
    };

    fn exchange(flows: flow::FlowSender, max_body: usize) -> Arc<Exchange> {
        let head = RequestHead {
            method: "POST",
            url: "http://a.test/upload".to_string(),
            version: Version::HTTP_11,
            headers: &HeaderMap::new(),
        };
        let client = SocketAddr::from(([127, 0, 0, 1], 1));
        Exchange::new(7, client, FlowKind::Http, head, flows, max_body)
    }

    fn post_parts() -> Parts {
        let (mut parts, _) = Request::new(()).into_parts();
        parts.method = Method::POST;
        parts.uri = "http://a.test/upload".parse().unwrap();
        parts
    }

    /// Resolves the next held request with `decision` and returns what the UI saw.
    fn resolve_next(
        engine: &Arc<RuleEngine>,
        decision: Decision,
    ) -> tokio::task::JoinHandle<HeldRequest> {
        let engine = engine.clone();
        let mut events = engine.breakpoints().subscribe();
        tokio::spawn(async move {
            loop {
                if let Ok(crate::rules::breakpoint::BreakpointEvent::Held { request }) =
                    events.recv().await
                {
                    assert!(engine.breakpoints().resolve(request.id, decision));
                    return request;
                }
            }
        })
    }

    #[tokio::test]
    async fn an_oversized_body_is_held_without_its_body_and_sent_whole() {
        let engine = RuleEngine::new(None);
        let flows = flow::channel();
        let exchange = exchange(flows, 8);
        let original = Bytes::from_static(b"0123456789abcdef");
        let mut parts = post_parts();

        let edit = RequestEdit {
            method: Some("PUT".into()),
            body: Some("ignored".into()),
            ..RequestEdit::default()
        };
        let seen = resolve_next(&engine, Decision::Continue(edit));
        let body = hold_request(&engine, &exchange, &mut parts, Full::new(original.clone()))
            .await
            .unwrap();

        let held = seen.await.unwrap();
        assert_eq!(held.body, None);
        assert_eq!(held.body_size, original.len());
        assert_eq!(parts.method, Method::PUT);
        assert_eq!(body.collect().await.unwrap().to_bytes(), original);
    }

    #[tokio::test]
    async fn a_small_body_is_still_editable() {
        let engine = RuleEngine::new(None);
        let exchange = exchange(flow::channel(), 8);
        let mut parts = post_parts();

        let edit = RequestEdit {
            body: Some("changed".into()),
            ..RequestEdit::default()
        };
        let seen = resolve_next(&engine, Decision::Continue(edit));
        let body = hold_request(&engine, &exchange, &mut parts, Full::new("abc".into()))
            .await
            .unwrap();

        let held = seen.await.unwrap();
        assert_eq!(held.body.as_deref(), Some("abc"));
        assert_eq!(held.body_size, 3);
        assert_eq!(body.collect().await.unwrap().to_bytes(), "changed");
    }

    #[tokio::test]
    async fn aborting_an_oversized_body_records_the_flow() {
        let engine = RuleEngine::new(None);
        let flows = flow::channel();
        let mut rx = flows.subscribe();
        let exchange = exchange(flows, 4);
        let mut parts = post_parts();

        let seen = resolve_next(&engine, Decision::Abort);
        let res = hold_request(
            &engine,
            &exchange,
            &mut parts,
            Full::new(Bytes::from_static(b"far too long")),
        )
        .await
        .unwrap_err();
        assert_eq!(res.status().as_u16(), 499);
        assert_eq!(seen.await.unwrap().body, None);

        drop(exchange);
        let flow = rx.recv().await.unwrap();
        assert_eq!(flow.error.as_deref(), Some("aborted at a breakpoint"));
    }

    #[tokio::test]
    async fn a_mock_sends_repeated_headers_from_header_list() {
        let flows = flow::channel();
        let exchange = exchange(flows, 1024);
        let plan = Plan {
            mock: Some(Mock {
                headers: [("x-one".to_string(), "1".to_string())].into(),
                header_list: vec![
                    ("Set-Cookie".into(), "a=1".into()),
                    ("Set-Cookie".into(), "b=2".into()),
                ],
                body: Some("ok".into()),
                ..Mock::default()
            }),
            ..Plan::default()
        };
        let res = synthetic(&plan, exchange, Version::HTTP_11).await;
        let cookies: Vec<_> = res.headers().get_all(header::SET_COOKIE).iter().collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
        assert_eq!(res.headers()["x-one"], "1");
    }
}
