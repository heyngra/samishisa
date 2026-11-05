use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{HeaderMap, Request, Response, body::Incoming};
use serde_json::{Map, Value, json};

use crate::{
    cache::{self, Outcome, Produced, Producer, ResponseCache},
    clientip::{self, TrustProxy},
    log,
    openapi,
    ratelimit::{Limiter, Setting, Spec},
    runtime::{CallError, Runner, WitRequest, call_error_text},
};

pub type Res = Response<Full<Bytes>>;

fn forwarded(name: &str) -> bool {
    matches!(name, "accept" | "accept-language" | "authorization" | "content-type" | "user-agent") || name.starts_with("x-")
}

pub const RESERVED: &[&str] = &["docs", "openapi.json", "health", "favicon.ico", "robots.txt"];
const EXPOSE: &str = "x-request-id, x-cache, ratelimit-limit, ratelimit-remaining, ratelimit-reset, retry-after, etag";

enum Seg {
    Lit(String),
    Param(String),
}

struct Validators {
    params: Option<jsonschema::Validator>,
    query: Option<jsonschema::Validator>,
    body: Option<jsonschema::Validator>,
}

struct Route {
    index: u32,
    method: String,
    segs: Vec<Seg>,
    specificity: usize,
    usage: String,
    timeout: Duration,
    validators: Validators,
    rate: Option<(Spec, String)>,
    cache: Option<(u32, u32)>,
}

impl Route {
    fn matches<'a>(&self, parts: &[&'a str]) -> Option<Vec<(&str, &'a str)>> {
        if parts.len() != self.segs.len() {
            return None;
        }
        let mut out = Vec::new();
        for (seg, part) in self.segs.iter().zip(parts) {
            match seg {
                Seg::Lit(l) if l == part => {}
                Seg::Lit(_) => return None,
                Seg::Param(n) => out.push((n.as_str(), *part)),
            }
        }
        Some(out)
    }
}

pub struct Mounted {
    pub runner: Arc<Runner>,
    routes: Vec<Route>,
}

pub struct Failed {
    pub source: String,
    pub error: String,
}

pub struct Settings {
    pub default_timeout: Duration,
    pub max_body: usize,
    pub default_rate: Setting,
    pub trust: TrustProxy,
    pub cors: bool,
    pub compression: bool,
    pub access_log: bool,
    pub title: String,
}

pub struct App {
    exts: HashMap<String, Mounted>,
    order: Vec<String>,
    failed: Vec<Failed>,
    settings: Settings,
    limiter: Arc<Limiter>,
    cache: Arc<ResponseCache>,
    openapi: Bytes,
    next_id: AtomicU64,
    id_salt: u64,
    pub jobs: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for App {
    fn drop(&mut self) {
        for j in self.jobs.lock().unwrap().drain(..) {
            j.abort();
        }
    }
}

fn route_rate(settings: &Settings, mount: &str, index: usize, route: &crate::runtime::RouteManifest, ext_default: Setting, operator: Setting) -> anyhow::Result<Option<(Spec, String)>> {
    if settings.default_rate == Setting::Off || operator == Setting::Off {
        return Ok(None);
    }
    Ok(match Setting::from_value(route.rate_limit.as_ref(), &format!("{} {}: rateLimit", route.method, route.path))? {
        Setting::Off => None,
        Setting::On(s) => Some((s, format!("{mount}:{index}"))),
        Setting::Inherit => match if let Setting::On(s) = operator { Setting::On(s) } else { ext_default } {
            Setting::On(s) => Some((s, format!("{mount}:*"))),
            Setting::Off => None,
            Setting::Inherit => match settings.default_rate {
                Setting::On(s) => Some((s, "_".into())),
                _ => None,
            },
        },
    })
}

impl App {
    pub fn new(runners: Vec<(Arc<Runner>, Setting)>, failed: Vec<Failed>, settings: Settings, limiter: Arc<Limiter>, cache: Arc<ResponseCache>) -> anyhow::Result<Self> {
        let mut exts = HashMap::new();
        let mut order = Vec::new();
        let mut failed = failed;
        for (runner, operator_rate) in runners {
            let name = runner.mount.clone();
            if RESERVED.contains(&name.as_str()) || exts.contains_key(&name) {
                failed.push(Failed {
                    source: runner.source.clone(),
                    error: format!("the name \"{name}\" is {}; mount it under another one with {{ \"source\": ..., \"name\": \"...\" }}", if exts.contains_key(&name) { "already used" } else { "reserved" }),
                });
                continue;
            }
            let ext_default = Setting::from_value(runner.manifest.rate_limit.as_ref(), &format!("{name}: rateLimit"))?;
            let mut routes = Vec::new();
            for (i, r) in runner.manifest.routes.iter().enumerate() {
                let segs: Vec<Seg> = r.path.trim_matches('/').split('/').filter(|s| !s.is_empty()).map(|s| match s.strip_prefix(':') {
                    Some(p) => Seg::Param(p.to_owned()),
                    None => Seg::Lit(s.to_owned()),
                }).collect();
                let compile = |s: &Option<Value>| -> anyhow::Result<Option<jsonschema::Validator>> {
                    s.as_ref().map(|s| jsonschema::validator_for(s).map_err(|e| anyhow::anyhow!("{} {}: invalid schema: {e}", r.method, r.path))).transpose()
                };
                let cache = r.cache.as_ref().and_then(|c| Some((c.get("ttl")?.as_u64()? as u32, c.get("staleTtl").and_then(Value::as_u64).unwrap_or(0) as u32))).filter(|(t, _)| *t > 0);
                routes.push(Route {
                    index: i as u32,
                    method: r.method.clone(),
                    specificity: segs.iter().filter(|s| matches!(s, Seg::Lit(_))).count(),
                    segs,
                    usage: usage(&name, r),
                    timeout: r.timeout_ms.map(Duration::from_millis).unwrap_or(settings.default_timeout),
                    validators: Validators { params: compile(&r.params)?, query: compile(&r.query)?, body: compile(&r.body)? },
                    rate: route_rate(&settings, &name, i, r, ext_default, operator_rate)?,
                    cache: if r.method == "GET" { cache } else { None },
                });
            }
            order.push(name.clone());
            exts.insert(name, Mounted { runner, routes });
        }
        let openapi = Bytes::from(serde_json::to_vec_pretty(&openapi::build(order.iter().filter_map(|n| exts.get(n)).map(|m: &Mounted| m.runner.as_ref()), &settings.title))?);
        let id_salt = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        Ok(Self { exts, order, failed, settings, limiter, cache, openapi, next_id: AtomicU64::new(1), id_salt, jobs: Default::default() })
    }

    pub fn failed(&self) -> &[Failed] {
        &self.failed
    }

    pub fn mounted(&self) -> impl Iterator<Item = &Mounted> {
        self.order.iter().filter_map(|n| self.exts.get(n))
    }

    fn request_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{:016x}", (n ^ self.id_salt).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    pub async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Res {
        let started = Instant::now();
        let id = self.request_id();
        let client = clientip::client_ip(peer.ip(), req.headers(), &self.settings.trust);
        let origin = req.headers().contains_key("origin");
        let gzip_ok = self.settings.compression && accepts_gzip(req.headers());
        let (method, path) = (req.method().to_string(), req.uri().path().to_owned());
        let mut res = self.route(req, &id, client).await;
        if gzip_ok {
            res = compress(res);
        }
        let h = res.headers_mut();
        h.insert("x-request-id", id.parse().expect("hex is a valid header"));
        if origin && self.settings.cors {
            h.insert("access-control-allow-origin", "*".parse().expect("static"));
            h.insert("access-control-expose-headers", EXPOSE.parse().expect("static"));
        }
        if self.settings.access_log {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let cache = res.headers().get("x-cache").and_then(|v| v.to_str().ok()).map(|c| format!(" {c}")).unwrap_or_default();
            log::access(format!("{client} \"{method} {path}\" {} {:.1}ms{cache} {id}", res.status().as_u16(), ms));
        }
        res
    }

    async fn route(&self, req: Request<Incoming>, id: &str, client: std::net::IpAddr) -> Res {
        let path = req.uri().path().to_owned();
        let is_head = req.method() == "HEAD";
        let method = if is_head { "GET".to_owned() } else { req.method().to_string() };

        if req.method() == "OPTIONS" && self.settings.cors && req.headers().contains_key("access-control-request-method") {
            let allow_headers = req.headers().get("access-control-request-headers").and_then(|v| v.to_str().ok()).unwrap_or("content-type, authorization").to_owned();
            return Response::builder()
                .status(204)
                .header("access-control-allow-origin", "*")
                .header("access-control-allow-methods", "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS")
                .header("access-control-allow-headers", allow_headers)
                .header("access-control-max-age", "86400")
                .body(Full::new(Bytes::new()))
                .expect("static response");
        }

        let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        let Some((first, rest)) = parts.split_first() else {
            return json_res(200, &self.index());
        };
        match (*first, rest.is_empty()) {
            ("health", true) => return json_res(200, &json!({ "ok": true })),
            ("openapi.json", true) => return bytes_res(200, "application/json", self.openapi.clone()),
            ("docs", true) => return bytes_res(200, "text/html; charset=utf-8", Bytes::from_static(openapi::DOCS_HTML.as_bytes())),
            _ => {}
        }
        let Some(m) = self.exts.get(*first) else {
            let hint = closest(first, self.order.iter().map(String::as_str)).map(|c| format!("Did you mean /{c}?")).unwrap_or_else(|| "GET / lists the available extensions.".into());
            return error(404, "not_found", &format!("No extension named \"{first}\""), Some(&hint), None, id);
        };
        if rest.is_empty() && method == "GET" {
            return json_res(200, &describe(m));
        }

        let mut best: Option<(&Route, Vec<(&str, &str)>)> = None;
        let mut allowed: Vec<&str> = Vec::new();
        for r in &m.routes {
            let Some(params) = r.matches(rest) else { continue };
            if r.method != method {
                allowed.push(&r.method);
                continue;
            }
            if best.as_ref().is_none_or(|(b, _)| r.specificity > b.specificity) {
                best = Some((r, params));
            }
        }
        let Some((route, raw_params)) = best else {
            if !allowed.is_empty() {
                allowed.sort();
                allowed.dedup();
                let mut res = error(405, "method_not_allowed", &format!("{} is not allowed here", req.method()), Some(&format!("Use {}", allowed.join(" or "))), None, id);
                res.headers_mut().insert("allow", allowed.join(", ").parse().expect("method names"));
                return res;
            }
            let routes: Vec<&str> = m.routes.iter().map(|r| r.usage.as_str()).collect();
            return error(404, "not_found", &format!("No route matches {} {path}", req.method()), Some(&format!("GET /{first} describes every route")), Some(json!({ "routes": routes })), id);
        };

        let mut rate_headers = None;
        if let Some((spec, owner)) = &route.rate {
            let key = if spec.global { owner.clone() } else { format!("{owner}|{}", clientip::bucket(client)) };
            let d = self.limiter.check(&key, *spec).await;
            rate_headers = Some(d);
            if !d.ok {
                let mut res = error(
                    429,
                    "rate_limited",
                    "Too many requests",
                    Some(&format!("The limit is {} per {}s{}. Retry in {}s.", spec.limit, spec.window_sec, if spec.global { " for all callers together" } else { "" }, d.reset_sec)),
                    None,
                    id,
                );
                set_rate_headers(res.headers_mut(), &d);
                res.headers_mut().insert("retry-after", d.reset_sec.into());
                return res;
            }
        }

        let spec = &m.runner.manifest.routes[route.index as usize];
        let mut params = Map::new();
        for (k, v) in raw_params {
            params.insert(k.to_owned(), Value::String(percent_decode(v)));
        }
        let params = coerce(Value::Object(params), spec.params.as_ref());
        let mut query = Map::new();
        let raw_query = req.uri().query().unwrap_or("").to_owned();
        for (k, v) in form_urlencoded::parse(raw_query.as_bytes()).into_owned() {
            match query.get_mut(&k) {
                Some(Value::Array(a)) => a.push(Value::String(v)),
                Some(prev) => *prev = Value::Array(vec![prev.take(), Value::String(v)]),
                None => {
                    query.insert(k, Value::String(v));
                }
            }
        }
        let query = coerce(Value::Object(query), spec.query.as_ref());

        let headers: Vec<(String, String)> = req.headers().iter().filter(|(k, _)| forwarded(k.as_str())).filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect();
        let if_none_match = req.headers().get("if-none-match").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let takes_body = matches!(method.as_str(), "POST" | "PUT" | "PATCH");
        let body = if takes_body && route.validators.body.is_some() {
            match Limited::new(req.into_body(), self.settings.max_body).collect().await {
                Ok(b) => {
                    let bytes = b.to_bytes();
                    if bytes.is_empty() {
                        Value::Null
                    } else {
                        match serde_json::from_slice::<Value>(&bytes) {
                            Ok(v) => v,
                            Err(e) => return error(400, "invalid_json", &format!("The request body is not valid JSON: {e}"), Some("Send a JSON body with content-type: application/json"), None, id),
                        }
                    }
                }
                Err(_) => return error(413, "body_too_large", &format!("The request body is larger than {} bytes", self.settings.max_body), None, None, id),
            }
        } else {
            Value::Null
        };

        let mut problems = Vec::new();
        check(&route.validators.params, &params, "params", &mut problems);
        check(&route.validators.query, &query, "query", &mut problems);
        check(&route.validators.body, &body, "body", &mut problems);
        if !problems.is_empty() {
            let details = json!({ "problems": problems, "usage": route.usage, "expected": { "params": spec.params, "query": spec.query, "body": spec.body } });
            return error(400, "validation_error", &format!("Invalid request ({} problem{})", problems.len(), if problems.len() == 1 { "" } else { "s" }), Some(&format!("Usage: {}", route.usage)), Some(details), id);
        }

        let wreq = WitRequest {
            route: route.index,
            method,
            path: path.clone(),
            input_json: json!({ "params": params, "query": query, "body": body }).to_string(),
            headers,
            request_id: id.to_owned(),
        };

        let mut res = match route.cache {
            Some((ttl, stale)) => {
                let key = format!("{}\u{0}{}\u{0}{}\u{0}{}", m.runner.mount, route.index, params, query);
                let runner = m.runner.clone();
                let timeout = route.timeout;
                let rid = id.to_owned();
                let producer: Producer = Arc::new(move || {
                    let (runner, wreq, rid) = (runner.clone(), wreq.clone(), rid.clone());
                    Box::pin(async move { call(&runner, wreq, timeout, &rid).await })
                });
                match self.cache.get(&key, ttl, stale, producer).await {
                    Outcome::Hit(e) => cached(&e, "HIT", ttl, stale, if_none_match.as_deref()),
                    Outcome::Stale(e) => cached(&e, "STALE", ttl, stale, if_none_match.as_deref()),
                    Outcome::Miss(e) => cached(&e, "MISS", ttl, stale, if_none_match.as_deref()),
                    Outcome::Uncached(p) => produced_res(p),
                }
            }
            None => produced_res(call(&m.runner, wreq, route.timeout, id).await),
        };
        if let Some(d) = rate_headers {
            set_rate_headers(res.headers_mut(), &d);
        }
        if is_head {
            *res.body_mut() = Full::new(Bytes::new());
        }
        res
    }

    fn index(&self) -> Value {
        json!({
            "name": self.settings.title,
            "version": env!("CARGO_PKG_VERSION"),
            "docs": "/docs",
            "openapi": "/openapi.json",
            "extensions": self.mounted().map(|m| json!({
                "name": m.runner.mount,
                "version": m.runner.manifest.version,
                "description": m.runner.manifest.description,
                "network": m.runner.policy().describe(),
                "routes": m.routes.iter().map(|r| r.usage.clone()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "failed": self.failed.iter().map(|f| json!({ "source": f.source, "error": f.error })).collect::<Vec<_>>(),
        })
    }
}

async fn call(runner: &Runner, wreq: WitRequest, timeout: Duration, id: &str) -> Produced {
    let r = match tokio::time::timeout(timeout + Duration::from_secs(1), runner.handle(wreq, timeout)).await {
        Ok(r) => r,
        Err(_) => Err(CallError::Timeout),
    };
    match r {
        Ok(Ok(r)) => {
            let headers = r.headers.into_iter().filter(|(k, _)| !matches!(k.as_str(), "content-length" | "transfer-encoding" | "connection" | "x-request-id" | "content-encoding")).collect();
            Produced { status: r.status, headers, body: Bytes::from(r.body) }
        }
        Ok(Err(e)) => {
            let details = e.details_json.and_then(|d| serde_json::from_str(&d).ok());
            let code = e.code.unwrap_or_else(|| default_code(e.status).to_owned());
            error_produced(e.status, &code, &e.message, e.hint.as_deref(), details, id)
        }
        Err(CallError::Timeout) => error_produced(504, "timeout", &format!("The request took longer than {} ms", timeout.as_millis()), None, None, id),
        Err(CallError::Busy) => {
            let mut p = error_produced(503, "extension_busy", "The extension is at capacity", Some("Retry in a moment"), None, id);
            p.headers.push(("retry-after".into(), "1".into()));
            p
        }
        Err(e @ CallError::Crashed(_)) => {
            log::write(log::Level::Error, &runner.mount, &format!("[{id}] crashed: {}", call_error_text(&e)));
            error_produced(500, "internal_error", "The extension failed while handling this request", Some("The details are in the server log under this request id"), None, id)
        }
    }
}

fn produced_res(p: Produced) -> Res {
    let mut b = Response::builder().status(p.status);
    for (k, v) in &p.headers {
        b = b.header(k, v);
    }
    b.body(Full::new(p.body)).unwrap_or_else(|_| json_res(500, &json!({ "error": { "status": 500, "code": "internal_error", "message": "The extension returned an invalid response" } })))
}

fn cached(e: &cache::Entry, state: &str, ttl: u32, stale: u32, if_none_match: Option<&str>) -> Res {
    let max_age = if state == "STALE" { 0 } else { (e.fresh_until.saturating_sub(cache::now_ms()) / 1000).min(ttl as u64) };
    let cc = if stale > 0 { format!("public, max-age={max_age}, stale-while-revalidate={stale}") } else { format!("public, max-age={max_age}") };
    let not_modified = if_none_match.is_some_and(|v| v.split(',').any(|t| t.trim() == e.etag || t.trim() == "*"));
    let mut b = Response::builder().status(if not_modified { 304 } else { e.status }).header("etag", &e.etag).header("cache-control", cc).header("x-cache", state);
    if not_modified {
        return b.body(Full::new(Bytes::new())).expect("valid");
    }
    for (k, v) in &e.headers {
        b = b.header(k, v);
    }
    let mut res = b.body(Full::new(e.body.clone())).expect("valid");
    if let Some(gz) = &e.gz {
        res.extensions_mut().insert(Precompressed(gz.clone()));
    }
    res
}

#[derive(Clone)]
struct Precompressed(Bytes);

fn accepts_gzip(h: &HeaderMap) -> bool {
    h.get("accept-encoding").and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|e| e.trim().starts_with("gzip")))
}

fn compress(res: Res) -> Res {
    if res.headers().contains_key("content-encoding") || res.status() == 304 || res.status() == 204 {
        return res;
    }
    let compressible = res.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(cache::is_compressible);
    if !compressible {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let raw = take_bytes(body);
    let gz = match parts.extensions.remove::<Precompressed>() {
        Some(Precompressed(g)) => g,
        None if raw.len() < cache::GZIP_MIN_BYTES => return Response::from_parts(parts, Full::new(raw)),
        None => match cache::gzip(&raw) {
            Some(g) if g.len() < raw.len() => g,
            _ => return Response::from_parts(parts, Full::new(raw)),
        },
    };
    parts.headers.insert("content-encoding", "gzip".parse().expect("static"));
    parts.headers.append("vary", "accept-encoding".parse().expect("static"));
    parts.headers.remove("content-length");
    Response::from_parts(parts, Full::new(gz))
}

fn take_bytes(body: Full<Bytes>) -> Bytes {
    use std::{
        pin::pin,
        task::{Context, Poll, Waker},
    };
    let mut cx = Context::from_waker(Waker::noop());
    let mut b = pin!(body);
    match hyper::body::Body::poll_frame(b.as_mut(), &mut cx) {
        Poll::Ready(Some(Ok(frame))) => frame.into_data().unwrap_or_default(),
        _ => Bytes::new(),
    }
}

fn set_rate_headers(h: &mut HeaderMap, d: &crate::ratelimit::Decision) {
    h.insert("ratelimit-limit", d.limit.into());
    h.insert("ratelimit-remaining", d.remaining.into());
    h.insert("ratelimit-reset", d.reset_sec.into());
}

fn describe(m: &Mounted) -> Value {
    let man = &m.runner.manifest;
    json!({
        "name": m.runner.mount,
        "version": man.version,
        "description": man.description,
        "sdk": man.sdk,
        "network": m.runner.policy().describe(),
        "routes": man.routes.iter().zip(&m.routes).map(|(r, compiled)| json!({
            "method": r.method,
            "path": format!("/{}{}", m.runner.mount, if r.path == "/" { "" } else { &r.path }),
            "usage": compiled.usage,
            "summary": r.summary,
            "description": r.description,
            "params": r.params,
            "query": r.query,
            "body": r.body,
            "response": r.response,
            "cache": r.cache,
            "rateLimit": compiled.rate.as_ref().map(|(s, _)| json!({ "limit": s.limit, "windowSec": s.window_sec, "scope": if s.global { "global" } else { "client" } })),
        })).collect::<Vec<_>>(),
    })
}

fn usage(mount: &str, r: &crate::runtime::RouteManifest) -> String {
    let mut s = format!("{} /{mount}{}", r.method, if r.path == "/" { "" } else { &r.path });
    if let Some(props) = r.query.as_ref().and_then(|q| q.get("properties")).and_then(Value::as_object) {
        let qs: Vec<String> = props.iter().map(|(k, p)| format!("{k}=<{}>", type_name(p, r.query.as_ref()))).collect();
        if !qs.is_empty() {
            s.push_str(&format!("?{}", qs.join("&")));
        }
    }
    if r.body.is_some() {
        s.push_str(" + JSON body");
    }
    s
}

fn type_name(p: &Value, root: Option<&Value>) -> String {
    let p = resolve(p, root);
    if let Some(e) = p.get("enum").and_then(Value::as_array) {
        return e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("|");
    }
    primary_type(p).unwrap_or("value").to_owned()
}

fn resolve<'a>(p: &'a Value, root: Option<&'a Value>) -> &'a Value {
    match (p.get("$ref").and_then(Value::as_str), root) {
        (Some(r), Some(root)) => r.strip_prefix('#').and_then(|ptr| root.pointer(ptr)).unwrap_or(p),
        _ => p,
    }
}

fn primary_type(p: &Value) -> Option<&str> {
    match p.get("type") {
        Some(Value::String(t)) => Some(t),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    }
}

pub fn coerce(v: Value, schema: Option<&Value>) -> Value {
    let Some(schema) = schema else { return v };
    let Value::Object(map) = v else { return v };
    let props = schema.get("properties").and_then(Value::as_object);
    Value::Object(map.into_iter().map(|(k, val)| {
        let prop = props.and_then(|p| p.get(&k)).map(|p| resolve(p, Some(schema)));
        let val = match prop {
            Some(p) => coerce_one(val, p, schema),
            None => val,
        };
        (k, val)
    }).collect())
}

fn coerce_one(v: Value, p: &Value, root: &Value) -> Value {
    match (primary_type(p), v) {
        (Some("array"), Value::Array(items)) => {
            let item = p.get("items").map(|i| resolve(i, Some(root))).cloned().unwrap_or(Value::Bool(true));
            Value::Array(items.into_iter().map(|x| coerce_one(x, &item, root)).collect())
        }
        (Some("array"), single @ Value::String(_)) => coerce_one(Value::Array(vec![single]), p, root),
        (Some(t), Value::Array(mut items)) if t != "array" => coerce_one(items.pop().unwrap_or(Value::Null), p, root),
        (Some("integer"), Value::String(s)) => s.trim().parse::<i64>().map(Value::from).or_else(|_| s.trim().parse::<u64>().map(Value::from)).unwrap_or(Value::String(s)),
        (Some("number"), Value::String(s)) => s.trim().parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::String(s)),
        (Some("boolean"), Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "" => Value::Bool(true),
            "false" | "0" | "no" | "off" => Value::Bool(false),
            _ => Value::String(s),
        },
        (_, v) => v,
    }
}

fn check(validator: &Option<jsonschema::Validator>, value: &Value, part: &str, problems: &mut Vec<String>) {
    let Some(v) = validator else { return };
    if v.is_valid(value) {
        return;
    }
    for e in v.iter_errors(value) {
        let at = e.instance_path().to_string().replace('/', ".");
        problems.push(format!("{part}{at}: {e}"));
    }
}

fn percent_decode(s: &str) -> String {
    form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
}

pub fn json_res(status: u16, v: &Value) -> Res {
    bytes_res(status, "application/json", Bytes::from(serde_json::to_vec(v).unwrap_or_default()))
}

fn bytes_res(status: u16, content_type: &str, body: Bytes) -> Res {
    Response::builder().status(status).header("content-type", content_type).body(Full::new(body)).expect("valid response")
}

fn error_body(status: u16, code: &str, message: &str, hint: Option<&str>, details: Option<Value>, id: &str) -> Bytes {
    let mut e = json!({ "status": status, "code": code, "message": message, "requestId": id });
    if let Some(h) = hint {
        e["hint"] = Value::String(h.to_owned());
    }
    if let Some(d) = details {
        e["details"] = d;
    }
    Bytes::from(serde_json::to_vec(&json!({ "error": e })).unwrap_or_default())
}

fn error_produced(status: u16, code: &str, message: &str, hint: Option<&str>, details: Option<Value>, id: &str) -> Produced {
    Produced { status, headers: vec![("content-type".into(), "application/json".into())], body: error_body(status, code, message, hint, details, id) }
}

fn error(status: u16, code: &str, message: &str, hint: Option<&str>, details: Option<Value>, id: &str) -> Res {
    bytes_res(status, "application/json", error_body(status, code, message, hint, details, id))
}

fn default_code(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        422 => "unprocessable",
        429 => "rate_limited",
        500 => "internal_error",
        501 => "not_implemented",
        502 => "upstream_error",
        503 => "unavailable",
        504 => "timeout",
        _ if status < 500 => "client_error",
        _ => "server_error",
    }
}

fn closest<'a>(word: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    fn dist(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut cur = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
            }
            prev = cur;
        }
        prev[b.len()]
    }
    candidates.map(|c| (dist(word, c), c)).filter(|(d, c)| *d <= 2.max(c.len() / 3)).min_by_key(|(d, _)| *d).map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coercion() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": { "type": "integer" }, "f": { "type": "number" }, "b": { "type": "boolean" },
                "tags": { "type": "array", "items": { "type": "integer" } }, "o": { "type": ["string", "null"] }
            }
        });
        let input = json!({ "n": "42", "f": "1.5", "b": "false", "tags": ["1", "2"], "o": "x", "extra": "kept" });
        assert_eq!(coerce(input, Some(&schema)), json!({ "n": 42, "f": 1.5, "b": false, "tags": [1, 2], "o": "x", "extra": "kept" }));
        assert_eq!(coerce(json!({ "n": "abc" }), Some(&schema)), json!({ "n": "abc" }), "unconvertible stays a string for the validator");
        assert_eq!(coerce(json!({ "tags": "7" }), Some(&schema)), json!({ "tags": [7] }), "a single value for a list becomes a list");
        assert_eq!(coerce(json!({ "n": ["1", "2"] }), Some(&schema)), json!({ "n": 2 }), "last repeated key wins");
    }

    #[test]
    fn suggestions() {
        assert_eq!(closest("helo", ["hello", "weather"].into_iter()), Some("hello"));
        assert_eq!(closest("zzzzzz", ["hello"].into_iter()), None);
    }
}
