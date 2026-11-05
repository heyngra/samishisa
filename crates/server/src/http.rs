use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, Response, body::Incoming};
use serde_json::{Map, Value, json};

use crate::runtime::{CallError, Runner, WitRequest, call_error_text};

pub type Res = Response<Full<Bytes>>;

fn forwarded(name: &str) -> bool {
    matches!(name, "accept" | "accept-language" | "authorization" | "content-type" | "user-agent") || name.starts_with("x-")
}

const RESERVED: &[&str] = &["docs", "openapi.json", "health", "favicon.ico", "robots.txt"];

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

pub struct App {
    exts: HashMap<String, Mounted>,
    order: Vec<String>,
    failed: Vec<Failed>,
    max_body: usize,
    next_id: AtomicU64,
    id_salt: u64,
}

impl App {
    pub fn new(runners: Vec<Arc<Runner>>, failed: Vec<Failed>, default_timeout: Duration, max_body: usize) -> anyhow::Result<Self> {
        let mut exts = HashMap::new();
        let mut order = Vec::new();
        let mut failed = failed;
        for runner in runners {
            let name = runner.mount.clone();
            if RESERVED.contains(&name.as_str()) || exts.contains_key(&name) {
                failed.push(Failed {
                    source: runner.source.clone(),
                    error: format!("the name \"{name}\" is {}; mount it under another one with {{ \"source\": ..., \"name\": \"...\" }}", if exts.contains_key(&name) { "already used" } else { "reserved" }),
                });
                continue;
            }
            let mut routes = Vec::new();
            for (i, r) in runner.manifest.routes.iter().enumerate() {
                let segs: Vec<Seg> = r.path.trim_matches('/').split('/').filter(|s| !s.is_empty()).map(|s| match s.strip_prefix(':') {
                    Some(p) => Seg::Param(p.to_owned()),
                    None => Seg::Lit(s.to_owned()),
                }).collect();
                let compile = |s: &Option<Value>| -> anyhow::Result<Option<jsonschema::Validator>> {
                    s.as_ref().map(|s| jsonschema::validator_for(s).map_err(|e| anyhow::anyhow!("{} {}: invalid schema: {e}", r.method, r.path))).transpose()
                };
                routes.push(Route {
                    index: i as u32,
                    method: r.method.clone(),
                    specificity: segs.iter().filter(|s| matches!(s, Seg::Lit(_))).count(),
                    segs,
                    usage: usage(&name, r),
                    timeout: r.timeout_ms.map(Duration::from_millis).unwrap_or(default_timeout),
                    validators: Validators { params: compile(&r.params)?, query: compile(&r.query)?, body: compile(&r.body)? },
                });
            }
            order.push(name.clone());
            exts.insert(name, Mounted { runner, routes });
        }
        let id_salt = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        Ok(Self { exts, order, failed, max_body, next_id: AtomicU64::new(1), id_salt })
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

    pub async fn handle(&self, req: Request<Incoming>) -> Res {
        let id = self.request_id();
        let mut res = self.route(req, &id).await;
        res.headers_mut().insert("x-request-id", id.parse().expect("hex is a valid header"));
        res
    }

    async fn route(&self, req: Request<Incoming>, id: &str) -> Res {
        let path = req.uri().path().to_owned();
        let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        let Some((first, rest)) = parts.split_first() else {
            return json_res(200, &self.index());
        };
        if *first == "health" && rest.is_empty() {
            return json_res(200, &json!({ "ok": true }));
        }
        let Some(m) = self.exts.get(*first) else {
            let hint = closest(first, self.order.iter().map(String::as_str)).map(|c| format!("Did you mean /{c}?")).unwrap_or_else(|| "GET / lists the available extensions.".into());
            return error(404, "not_found", &format!("No extension named \"{first}\""), Some(&hint), None, id);
        };
        if rest.is_empty() && req.method() == "GET" {
            return json_res(200, &describe(m));
        }

        let mut best: Option<(&Route, Vec<(&str, &str)>)> = None;
        let mut allowed: Vec<&str> = Vec::new();
        for r in &m.routes {
            let Some(params) = r.matches(rest) else { continue };
            if r.method != req.method().as_str() {
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

        let spec = &m.runner.manifest.routes[route.index as usize];
        let mut params = Map::new();
        for (k, v) in raw_params {
            params.insert(k.to_owned(), Value::String(percent_decode(v)));
        }
        let params = coerce(Value::Object(params), spec.params.as_ref());
        let mut query = Map::new();
        for (k, v) in form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes()).into_owned() {
            match query.get_mut(&k) {
                Some(Value::Array(a)) => a.push(Value::String(v)),
                Some(prev) => *prev = Value::Array(vec![prev.take(), Value::String(v)]),
                None => {
                    query.insert(k, Value::String(v));
                }
            }
        }
        let query = coerce(Value::Object(query), spec.query.as_ref());

        let mut problems = Vec::new();
        let headers: Vec<(String, String)> = req.headers().iter().filter(|(k, _)| forwarded(k.as_str())).filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect();
        let method = req.method().to_string();
        let takes_body = matches!(method.as_str(), "POST" | "PUT" | "PATCH");
        let body = if takes_body && route.validators.body.is_some() {
            match Limited::new(req.into_body(), self.max_body).collect().await {
                Ok(b) => {
                    let bytes = b.to_bytes();
                    if bytes.is_empty() {
                        Value::Null
                    } else {
                        match serde_json::from_slice::<Value>(&bytes) {
                            Ok(v) => v,
                            Err(e) => {
                                return error(400, "invalid_json", &format!("The request body is not valid JSON: {e}"), Some("Send a JSON body with content-type: application/json"), None, id);
                            }
                        }
                    }
                }
                Err(_) => return error(413, "body_too_large", &format!("The request body is larger than {} bytes", self.max_body), None, None, id),
            }
        } else {
            Value::Null
        };

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
        match m.runner.handle(wreq, route.timeout).await {
            Ok(Ok(r)) => {
                let mut b = Response::builder().status(r.status);
                for (k, v) in &r.headers {
                    if !matches!(k.as_str(), "content-length" | "transfer-encoding" | "connection" | "x-request-id") {
                        b = b.header(k, v);
                    }
                }
                b.body(Full::new(Bytes::from(r.body))).unwrap_or_else(|_| error(500, "internal_error", "The extension returned an invalid response", None, None, id))
            }
            Ok(Err(e)) => {
                let details = e.details_json.and_then(|d| serde_json::from_str(&d).ok());
                let code = e.code.unwrap_or_else(|| default_code(e.status).to_owned());
                error(e.status, &code, &e.message, e.hint.as_deref(), details, id)
            }
            Err(CallError::Timeout) => error(504, "timeout", &format!("The request took longer than {} ms", route.timeout.as_millis()), None, None, id),
            Err(CallError::Busy) => {
                let mut res = error(503, "extension_busy", "The extension is at capacity", Some("Retry in a moment"), None, id);
                res.headers_mut().insert("retry-after", "1".parse().expect("digit"));
                res
            }
            Err(e @ CallError::Crashed(_)) => {
                crate::log::log_error!(&m.runner.mount, "[{id}] crashed: {}", call_error_text(&e));
                error(500, "internal_error", "The extension failed while handling this request", Some("The details are in the server log under this request id"), None, id)
            }
        }
    }

    fn index(&self) -> Value {
        json!({
            "name": "samishisa",
            "version": env!("CARGO_PKG_VERSION"),
            "extensions": self.mounted().map(|m| json!({
                "name": m.runner.mount,
                "version": m.runner.manifest.version,
                "description": m.runner.manifest.description,
                "routes": m.routes.iter().map(|r| r.usage.clone()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "failed": self.failed.iter().map(|f| json!({ "source": f.source, "error": f.error })).collect::<Vec<_>>(),
        })
    }
}

fn describe(m: &Mounted) -> Value {
    let man = &m.runner.manifest;
    json!({
        "name": m.runner.mount,
        "version": man.version,
        "description": man.description,
        "sdk": man.sdk,
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

fn coerce(v: Value, schema: Option<&Value>) -> Value {
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
    for e in v.iter_errors(value) {
        let at = e.instance_path().to_string().replace('/', ".");
        problems.push(format!("{part}{at}: {e}"));
    }
}

fn percent_decode(s: &str) -> String {
    form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
}

pub fn json_res(status: u16, v: &Value) -> Res {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(serde_json::to_vec(v).unwrap_or_default())))
        .expect("valid response")
}

fn error(status: u16, code: &str, message: &str, hint: Option<&str>, details: Option<Value>, id: &str) -> Res {
    let mut e = json!({ "status": status, "code": code, "message": message, "requestId": id });
    if let Some(h) = hint {
        e["hint"] = Value::String(h.to_owned());
    }
    if let Some(d) = details {
        e["details"] = d;
    }
    json_res(status, &json!({ "error": e }))
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
