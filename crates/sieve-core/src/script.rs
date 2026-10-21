//! Scripting plugins: small [rhai](https://rhai.rs) scripts that rewrite
//! requests and responses, for rules too custom for a declarative matcher.
//!
//! A script file may define either or both hooks:
//!
//! ```rhai
//! fn on_request(req) {
//!     if req.url.contains("/ads/") {
//!         req.respond = #{ status: 204, body: "" };   // short-circuit
//!     } else {
//!         req.headers.set_header("X-Debug", "1");      // rewrite
//!     }
//!     req
//! }
//!
//! fn on_response(req, res) {
//!     res.body.replace("prod", "staging");            // rhai's replace works in place
//!     res
//! }
//! ```
//!
//! A hook returns the (possibly modified) map, or `()` to leave the message
//! unchanged. The maps carry `method`, `url`, `headers` and `body` (request),
//! and `status`, `headers` and `body` (response). `headers` is an array of
//! `[name, value]` pairs; the methods `headers.header(name)`,
//! `headers.set_header(name, value)` and `headers.remove_header(name)` work
//! on it (call them method-style so the change sticks).
//! `req.respond = #{ status, headers, body }` answers the request without
//! contacting the server.
//!
//! Bodies are exposed as text (lossy UTF-8) with any `Content-Encoding`
//! (gzip, br, zstd, deflate) removed. A body the script left alone is
//! forwarded exactly as it arrived, encoding included; a body it changed is
//! sent as plain text without `Content-Encoding`.
//!
//! The proxy runs the hooks on intercepted HTTP requests that no declarative
//! rule matched (rules win), and not on connections that `intercept_processes`
//! excludes. [`ScriptEngine::on_request`] runs before the request is forwarded,
//! and [`ScriptEngine::on_response`] on the upstream response before the
//! client gets it. A script that fails is logged and skipped; changes made by
//! the scripts before it still apply.
//!
//! Hooks see whole bodies, so the proxy reads a body into memory before
//! running them, but never more than `max_body_bytes` of it. A larger body,
//! one that does not decode within that limit, or one with trailers, is
//! forwarded untouched and skips its hook: `on_request` does not run for that
//! request (`on_response` then sees an empty `req.body`), and `on_response`
//! does not run for that response. `text/event-stream` responses are never
//! buffered, so server-sent events never reach `on_response`.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use rhai::{AST, Array, Dynamic, Engine, Map, Scope};

use crate::flow::{Request, Response};

/// What a request hook decided.
#[derive(Debug, Clone)]
pub enum RequestOutcome {
    /// Forward the (possibly rewritten) request.
    Forward,
    /// Answer with this response instead of contacting the server.
    Respond(Response),
}

struct Script {
    name: String,
    ast: AST,
    on_request: bool,
    on_response: bool,
}

/// A set of loaded scripts, run in load order.
pub struct ScriptEngine {
    engine: Engine,
    scripts: Vec<Script>,
}

impl Default for ScriptEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptEngine {
    pub fn new() -> Self {
        let mut engine = Engine::new();
        // A runaway script must not hang the proxy.
        engine.set_max_operations(1_000_000);
        engine.set_max_call_levels(32);
        engine.register_fn("header", |h: &mut Array, name: &str| -> Dynamic {
            h.iter()
                .filter_map(pair)
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map_or(Dynamic::UNIT, |(_, v)| v.into())
        });
        engine.register_fn("set_header", |h: &mut Array, name: &str, value: &str| {
            remove(h, name);
            h.push(Array::from([name.into(), value.into()]).into());
        });
        engine.register_fn("remove_header", |h: &mut Array, name: &str| remove(h, name));
        Self {
            engine,
            scripts: Vec::new(),
        }
    }

    /// Compiles `source` and adds it under `name` (used in error messages).
    pub fn load_source(&mut self, name: &str, source: &str) -> Result<()> {
        let ast = self
            .engine
            .compile(source)
            .map_err(|e| anyhow!("compiling script {name}: {e}"))?;
        let has = |f: &str| ast.iter_functions().any(|m| m.name == f);
        self.scripts.push(Script {
            name: name.to_string(),
            on_request: has("on_request"),
            on_response: has("on_response"),
            ast,
        });
        Ok(())
    }

    /// Loads one `.rhai` file.
    pub fn load_file(&mut self, path: &Path) -> Result<()> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("reading script {}", path.display()))?;
        self.load_source(&path.display().to_string(), &source)
    }

    /// Loads every `*.rhai` file in `dir`, sorted by file name. A missing
    /// directory loads nothing. Returns the number of scripts loaded.
    pub fn load_dir(&mut self, dir: &Path) -> Result<usize> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "rhai"))
            .collect();
        paths.sort();
        for p in &paths {
            self.load_file(p)?;
        }
        Ok(paths.len())
    }

    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }

    /// Whether any loaded script defines `on_response`, so a caller can skip
    /// buffering a response body when nothing would use it.
    pub fn has_on_response(&self) -> bool {
        self.scripts.iter().any(|s| s.on_response)
    }

    /// Runs every `on_request` hook against `req`, rewriting it in place.
    /// Stops at the first script that asks to respond directly.
    pub fn on_request(&self, req: &mut Request) -> Result<RequestOutcome> {
        for s in self.scripts.iter().filter(|s| s.on_request) {
            let out: Dynamic = self
                .engine
                .call_fn(&mut Scope::new(), &s.ast, "on_request", (request_map(req),))
                .map_err(|e| anyhow!("script {}: on_request: {e}", s.name))?;
            let Some(map) = out.try_cast::<Map>() else {
                continue;
            };
            apply_request(&map, req);
            if let Some(r) = map.get("respond").and_then(|d| d.clone().try_cast::<Map>()) {
                return Ok(RequestOutcome::Respond(response_from(&r)));
            }
        }
        Ok(RequestOutcome::Forward)
    }

    /// Runs every `on_response` hook against `res`, rewriting it in place.
    pub fn on_response(&self, req: &Request, res: &mut Response) -> Result<()> {
        for s in self.scripts.iter().filter(|s| s.on_response) {
            let out: Dynamic = self
                .engine
                .call_fn(
                    &mut Scope::new(),
                    &s.ast,
                    "on_response",
                    (request_map(req), response_map(res)),
                )
                .map_err(|e| anyhow!("script {}: on_response: {e}", s.name))?;
            if let Some(map) = out.try_cast::<Map>() {
                apply_response(&map, res);
            }
        }
        Ok(())
    }
}

fn pair(d: &Dynamic) -> Option<(String, String)> {
    let a = d.clone().try_cast::<Array>()?;
    let k = a.first()?.clone().into_string().ok()?;
    let v = a.get(1)?.clone().into_string().ok()?;
    Some((k, v))
}

fn remove(h: &mut Array, name: &str) {
    h.retain(|p| !pair(p).is_some_and(|(k, _)| k.eq_ignore_ascii_case(name)));
}

fn headers_array(headers: &[(String, String)]) -> Array {
    headers
        .iter()
        .map(|(k, v)| Array::from([k.as_str().into(), v.as_str().into()]).into())
        .collect()
}

fn headers_from(d: &Dynamic) -> Option<Vec<(String, String)>> {
    let a = d.clone().try_cast::<Array>()?;
    Some(a.iter().filter_map(pair).collect())
}

fn body_text(body: &crate::flow::Body) -> String {
    String::from_utf8_lossy(body.decoded.as_ref().unwrap_or(&body.raw)).into_owned()
}

/// Replaces the body when `text` differs from what the script was given.
/// Clearing `decoded` is how the proxy tells a rewritten body from an
/// untouched one, which still goes out with its original encoding.
fn set_body(body: &mut crate::flow::Body, text: &str) {
    if text == body_text(body) {
        return;
    }
    body.raw = Bytes::copy_from_slice(text.as_bytes());
    body.decoded = None;
    body.size = text.len() as u64;
    body.truncated = false;
    body.content_encoding = None;
}

fn request_map(req: &Request) -> Map {
    let mut m = Map::new();
    m.insert("method".into(), req.method.as_str().into());
    m.insert("url".into(), req.url.as_str().into());
    m.insert("headers".into(), headers_array(&req.headers).into());
    m.insert("body".into(), body_text(&req.body).into());
    m
}

fn response_map(res: &Response) -> Map {
    let mut m = Map::new();
    m.insert("status".into(), i64::from(res.status).into());
    m.insert("headers".into(), headers_array(&res.headers).into());
    m.insert("body".into(), body_text(&res.body).into());
    m
}

fn apply_request(m: &Map, req: &mut Request) {
    if let Some(v) = m.get("method").and_then(|d| d.clone().into_string().ok()) {
        req.method = v;
    }
    if let Some(v) = m.get("url").and_then(|d| d.clone().into_string().ok()) {
        req.url = v;
    }
    if let Some(h) = m.get("headers").and_then(headers_from) {
        req.headers = h;
    }
    if let Some(v) = m.get("body").and_then(|d| d.clone().into_string().ok()) {
        set_body(&mut req.body, &v);
    }
}

fn apply_response(m: &Map, res: &mut Response) {
    if let Some(s) = m
        .get("status")
        .and_then(|d| d.as_int().ok())
        .and_then(|s| u16::try_from(s).ok())
    {
        res.status = s;
    }
    if let Some(h) = m.get("headers").and_then(headers_from) {
        res.headers = h;
    }
    if let Some(v) = m.get("body").and_then(|d| d.clone().into_string().ok()) {
        set_body(&mut res.body, &v);
    }
}

/// A synthetic response from a `respond` map; `status` defaults to 200.
fn response_from(m: &Map) -> Response {
    let mut res = Response {
        status: 200,
        http_version: "HTTP/1.1".into(),
        headers: Vec::new(),
        body: Default::default(),
    };
    apply_response(m, &mut res);
    let text = m
        .get("body")
        .and_then(|d| d.clone().into_string().ok())
        .unwrap_or_default();
    res.body.raw = Bytes::copy_from_slice(text.as_bytes());
    res.body.size = text.len() as u64;
    res.body.content_type = res
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone());
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::Body;

    fn request() -> Request {
        Request {
            method: "GET".into(),
            url: "https://api.test/ads/x".into(),
            http_version: "HTTP/1.1".into(),
            headers: vec![("Accept".into(), "*/*".into())],
            body: Body::default(),
        }
    }

    fn response(body: &str) -> Response {
        Response {
            status: 200,
            http_version: "HTTP/1.1".into(),
            headers: vec![],
            body: Body {
                raw: Bytes::copy_from_slice(body.as_bytes()),
                size: body.len() as u64,
                ..Body::default()
            },
        }
    }

    #[test]
    fn rewrites_request_headers_and_url() {
        let mut s = ScriptEngine::new();
        s.load_source(
            "t",
            r#"fn on_request(req) {
                req.headers.set_header("x-debug", "1");
                req.headers.remove_header("ACCEPT");
                req.url.replace("api.test", "localhost:3000");
                req
            }"#,
        )
        .unwrap();
        let mut req = request();
        assert!(matches!(
            s.on_request(&mut req).unwrap(),
            RequestOutcome::Forward
        ));
        assert_eq!(req.url, "https://localhost:3000/ads/x");
        assert_eq!(req.headers, vec![("x-debug".to_string(), "1".to_string())]);
    }

    #[test]
    fn respond_short_circuits_and_later_scripts_do_not_run() {
        let mut s = ScriptEngine::new();
        s.load_source(
            "a",
            r#"fn on_request(req) {
                if req.url.contains("/ads/") {
                    req.respond = #{ status: 204, headers: [["Content-Type", "text/plain"]], body: "no" };
                }
                req
            }"#,
        )
        .unwrap();
        s.load_source("b", r#"fn on_request(req) { throw "must not run"; }"#)
            .unwrap();
        let mut req = request();
        let RequestOutcome::Respond(r) = s.on_request(&mut req).unwrap() else {
            panic!("expected a direct response");
        };
        assert_eq!(r.status, 204);
        assert_eq!(r.body.raw, Bytes::from_static(b"no"));
        assert_eq!(r.body.content_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn rewrites_response_body_and_status() {
        let mut s = ScriptEngine::new();
        s.load_source(
            "t",
            r#"fn on_response(req, res) {
                res.status = 418;
                res.body.replace("prod", "staging");
                res
            }"#,
        )
        .unwrap();
        let mut res = response("host=prod");
        s.on_response(&request(), &mut res).unwrap();
        assert_eq!(res.status, 418);
        assert_eq!(res.body.raw, Bytes::from_static(b"host=staging"));
        assert_eq!(res.body.size, 12);
    }

    #[test]
    fn unit_return_and_untouched_body_leave_the_message_alone() {
        let mut s = ScriptEngine::new();
        s.load_source("t", "fn on_response(req, res) { }").unwrap();
        let mut res = response("\u{1}binary\u{ff}");
        res.body.raw = Bytes::from_static(&[1, 0xff]);
        s.on_response(&request(), &mut res).unwrap();
        assert_eq!(res.body.raw, Bytes::from_static(&[1, 0xff]));
    }

    #[test]
    fn errors_name_the_script_and_loops_are_bounded() {
        let mut s = ScriptEngine::new();
        assert!(s.load_source("bad", "fn on_request(").is_err());
        s.load_source("loop", "fn on_request(req) { loop {} }")
            .unwrap();
        let err = s.on_request(&mut request()).unwrap_err().to_string();
        assert!(err.contains("loop"), "{err}");
    }

    #[test]
    fn loads_rhai_files_from_a_directory_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("2.rhai"),
            r#"fn on_request(req) { req.method = req.method + "2"; req }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("1.rhai"),
            r#"fn on_request(req) { req.method = "P"; req }"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let mut s = ScriptEngine::new();
        assert_eq!(s.load_dir(dir.path()).unwrap(), 2);
        assert_eq!(s.load_dir(&dir.path().join("missing")).unwrap(), 0);
        let mut req = request();
        s.on_request(&mut req).unwrap();
        assert_eq!(req.method, "P2");
    }
}
