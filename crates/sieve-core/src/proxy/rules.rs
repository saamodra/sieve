//! Applies [`crate::rules`] to proxied requests: block, mock, map remote,
//! header and body rewrites, delays, and breakpoints.

use std::{path::Path, sync::Arc};

use bytes::Bytes;
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header, request::Parts, response,
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use tracing::warn;

use super::{
    Conn, ProxyBody,
    capture::{CaptureBody, Exchange, Side},
    strip_hop_by_hop, text_response, upstream_failed,
};
use crate::rules::{Decision, HeldRequest, Plan, RequestEdit, RequestInfo, RuleEngine};

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
        (Some(engine), true) => {
            let mut bytes = match body.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(e) => {
                    let message = format!("reading the request body failed: {e}");
                    exchange.finish(None, Some(message.clone()));
                    return text_response(StatusCode::BAD_REQUEST, &message);
                }
            };
            match hold(engine, &exchange, &parts, &bytes).await {
                Decision::Abort => {
                    let _ = CaptureBody::new(Full::new(bytes), Side::Request, exchange.clone())
                        .collect()
                        .await;
                    exchange.finish(None, Some("aborted at a breakpoint".to_string()));
                    return text_response(
                        StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_GATEWAY),
                        "request aborted at a breakpoint",
                    );
                }
                Decision::Continue(edit) => apply_edit(&mut parts, &mut bytes, edit),
            }
            changed = true;
            if parts.headers.contains_key(header::CONTENT_LENGTH) || !bytes.is_empty() {
                parts
                    .headers
                    .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
            }
            CaptureBody::new(Full::new(bytes), Side::Request, exchange.clone()).boxed_unsync()
        }
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

async fn hold(engine: &RuleEngine, exchange: &Exchange, parts: &Parts, body: &Bytes) -> Decision {
    engine
        .breakpoints()
        .hold(HeldRequest {
            id: exchange.id(),
            method: parts.method.to_string(),
            url: parts.uri.to_string(),
            headers: header_pairs(&parts.headers),
            body: std::str::from_utf8(body).ok().map(str::to_string),
            body_size: body.len(),
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

fn apply_headers(headers: &mut HeaderMap, set: &[(String, String)], remove: &[String]) {
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

/// Points `uri` at `target` (`host:port` or a URL), keeping the path and query.
fn remap(uri: &Uri, target: &str) -> Option<Uri> {
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
async fn synthetic(
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
}
