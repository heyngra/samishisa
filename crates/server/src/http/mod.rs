mod describe;
mod input;
mod openapi;
mod reply;
mod router;

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{HeaderMap, Method, Request, body::Incoming, http::request::Parts};
use serde_json::{Value, json};

use crate::{
    cache::{Outcome, Produced, Producer, ResponseCache},
    clientip, clock, compression,
    config::Config,
    jobs::Jobs,
    log::{self, log_error},
    ratelimit::{Decision, Limiter, RateSetting, RateSettings},
    runtime::{CallError, Runner, WitError, WitRequest},
};
use input::Input;
use reply::{ErrorReply, Res};
use router::{Match, Route};

const RESERVED_NAMES: &[&str] = &["docs", "openapi.json", "health", "favicon.ico", "robots.txt"];
const EXPOSED_HEADERS: &str = "x-request-id, x-cache, ratelimit-limit, ratelimit-remaining, ratelimit-reset, retry-after, etag";
const HEADERS_EXTENSIONS_CANT_SET: &[&str] = &["content-length", "transfer-encoding", "connection", "x-request-id", "content-encoding"];

fn is_forwarded_header(name: &str) -> bool {
    matches!(name, "accept" | "accept-language" | "authorization" | "content-type" | "user-agent") || name.starts_with("x-")
}

pub struct LoadedExtension {
    pub runner: Arc<Runner>,
    pub operator_rate_limit: RateSetting,
}

pub struct Failed {
    pub source: String,
    pub error: String,
}

pub struct Mounted {
    pub runner: Arc<Runner>,
    routes: Vec<Route>,
}

impl Mounted {
    fn new(extension: LoadedExtension, already_mounted: &[Mounted], config: &Config) -> anyhow::Result<Mounted> {
        let LoadedExtension { runner, operator_rate_limit } = extension;
        let name = runner.mount.as_str();
        let problem = if RESERVED_NAMES.contains(&name) {
            Some("reserved")
        } else if already_mounted.iter().any(|m| m.runner.mount == name) {
            Some("already used")
        } else {
            None
        };
        if let Some(problem) = problem {
            anyhow::bail!("the name \"{name}\" is {problem}; mount it under another one with {{ \"source\": ..., \"name\": \"...\" }}");
        }
        let default_timeout = Duration::from_millis(config.request_timeout_ms);
        let mut routes = Vec::new();
        for (index, spec) in runner.manifest.routes.iter().enumerate() {
            let rate_settings = RateSettings { server: config.rate_limit, operator: operator_rate_limit, extension: runner.manifest.rate_limit, route: spec.rate_limit };
            routes.push(Route::compile(name, index, spec, default_timeout, rate_settings)?);
        }
        Ok(Mounted { runner, routes })
    }
}

struct RequestIds {
    next: AtomicU64,
    salt: u64,
}

impl RequestIds {
    fn next(&self) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{:016x}", (n ^ self.salt).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
}

pub struct App {
    mounted: Vec<Mounted>,
    failed: Vec<Failed>,
    config: Config,
    limiter: Arc<Limiter>,
    cache: Arc<ResponseCache>,
    openapi: Bytes,
    request_ids: RequestIds,
    _jobs: Jobs,
}

impl App {
    pub fn new(extensions: Vec<LoadedExtension>, mut failed: Vec<Failed>, config: Config, limiter: Arc<Limiter>, cache: Arc<ResponseCache>) -> App {
        let mut mounted: Vec<Mounted> = Vec::new();
        for extension in extensions {
            let source = extension.runner.source.clone();
            match Mounted::new(extension, &mounted, &config) {
                Ok(m) => mounted.push(m),
                Err(e) => failed.push(Failed { source, error: format!("{e:#}") }),
            }
        }
        let openapi = Bytes::from(serde_json::to_vec_pretty(&openapi::build(&mounted, &config.title)).unwrap_or_default());
        let jobs = Jobs::start(mounted.iter().map(|m| &m.runner));
        let request_ids = RequestIds { next: AtomicU64::new(1), salt: clock::now_ms().wrapping_mul(1_000_003) };
        App { mounted, failed, config, limiter, cache, openapi, request_ids, _jobs: jobs }
    }

    pub fn mounted(&self) -> &[Mounted] {
        &self.mounted
    }

    pub fn failed(&self) -> &[Failed] {
        &self.failed
    }

    pub async fn handle(&self, request: Request<Incoming>, peer: SocketAddr) -> Res {
        let started = Instant::now();
        let id = self.request_ids.next();
        let client = clientip::client_ip(peer.ip(), request.headers(), &self.config.trust_proxy);
        let from_browser = request.headers().contains_key("origin");
        let wants_gzip = self.config.compression && compression::accepts_gzip(request.headers());
        let request_line = format!("{} {}", request.method(), request.uri().path());

        let mut response = self.respond(request, &id, client).await;
        if wants_gzip {
            response = compression::compress(response).await;
        }
        let headers = response.headers_mut();
        headers.insert("x-request-id", id.parse().expect("hex is a valid header"));
        if from_browser && self.config.cors {
            headers.insert("access-control-allow-origin", "*".parse().expect("static"));
            headers.insert("access-control-expose-headers", EXPOSED_HEADERS.parse().expect("static"));
        }
        if self.config.access_log {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let cache = response.headers().get("x-cache").and_then(|v| v.to_str().ok()).map(|c| format!(" {c}")).unwrap_or_default();
            log::access(format!("{client} \"{request_line}\" {} {ms:.1}ms{cache} {id}", response.status().as_u16()));
        }
        response
    }

    async fn respond(&self, request: Request<Incoming>, id: &str, client: IpAddr) -> Res {
        if self.config.cors && request.method() == Method::OPTIONS && request.headers().contains_key("access-control-request-method") {
            return preflight(request.headers());
        }
        let path = request.uri().path().to_owned();
        let parts = router::split_path(&path);
        let Some((name, rest)) = parts.split_first() else {
            return reply::json(200, &describe::index(&self.config.title, &self.mounted, &self.failed));
        };
        if rest.is_empty() {
            if let Some(page) = self.server_page(name) {
                return page;
            }
        }
        let Some(extension) = self.mounted.iter().find(|m| m.runner.mount == *name) else {
            return self.unknown_extension(name, id);
        };

        let is_head = request.method() == Method::HEAD;
        let method = if is_head { "GET".to_owned() } else { request.method().to_string() };
        if rest.is_empty() && method == "GET" {
            return reply::json(200, &describe::extension(extension));
        }
        let (route, params) = match router::find(&extension.routes, &method, rest) {
            Match::Found { route, params } => (route, params),
            unmatched => return no_matching_route(unmatched, extension, request.method(), &path, id),
        };

        let rate = match self.check_rate_limit(route, client, id).await {
            Ok(decision) => decision,
            Err(too_many) => return too_many,
        };
        let (head, body) = request.into_parts();
        let input = match self.read_input(&head, body, route, &params, &method).await {
            Ok(input) => input,
            Err(invalid) => return invalid.into_response(id),
        };
        let call = WitRequest {
            route: route.index,
            method,
            path,
            input_json: json!({ "params": input.params, "query": input.query, "body": input.body }).to_string(),
            headers: forwarded_headers(&head.headers),
            request_id: id.to_owned(),
        };
        let if_none_match = head.headers.get("if-none-match").and_then(|v| v.to_str().ok());
        let mut response = self.call_extension(extension, route, call, &input, if_none_match).await;
        if let Some(decision) = rate {
            set_rate_headers(response.headers_mut(), &decision);
        }
        if is_head {
            *response.body_mut() = Full::new(Bytes::new());
        }
        response
    }

    fn unknown_extension(&self, name: &str, id: &str) -> Res {
        let hint = match router::closest(name, self.mounted.iter().map(|m| m.runner.mount.as_str())) {
            Some(close) => format!("Did you mean /{close}?"),
            None => "GET / lists the available extensions.".into(),
        };
        ErrorReply::new(404, "not_found", format!("No extension named \"{name}\"")).hint(hint).into_response(id)
    }

    fn server_page(&self, name: &str) -> Option<Res> {
        Some(match name {
            "health" => reply::json(200, &json!({ "ok": true })),
            "openapi.json" => reply::bytes(200, "application/json", self.openapi.clone()),
            "docs" => reply::bytes(200, "text/html; charset=utf-8", Bytes::from_static(openapi::DOCS_HTML.as_bytes())),
            _ => return None,
        })
    }

    async fn check_rate_limit(&self, route: &Route, client: IpAddr, id: &str) -> Result<Option<Decision>, Res> {
        let Some(route_limit) = &route.rate_limit else { return Ok(None) };
        let decision = self.limiter.check(&route_limit.key_for(client), route_limit.limit).await;
        if decision.allowed {
            return Ok(Some(decision));
        }
        let limit = route_limit.limit;
        let scope = if limit.global { " for all callers together" } else { "" };
        let mut response = ErrorReply::new(429, "rate_limited", "Too many requests")
            .hint(format!("The limit is {} per {}s{scope}. Retry in {}s.", limit.limit, limit.window_sec, decision.reset_sec))
            .into_response(id);
        set_rate_headers(response.headers_mut(), &decision);
        response.headers_mut().insert("retry-after", decision.reset_sec.into());
        Err(response)
    }

    async fn read_input(&self, head: &Parts, body: Incoming, route: &Route, params: &[(&str, &str)], method: &str) -> Result<Input, ErrorReply> {
        let takes_body = matches!(method, "POST" | "PUT" | "PATCH") && route.validators.takes_body();
        let input = Input {
            params: input::params_from_path(params, route.spec.params.as_ref()),
            query: input::query_from_url(head.uri.query(), route.spec.query.as_ref()),
            body: if takes_body { input::read_json_body(body, self.config.max_body_bytes).await? } else { Value::Null },
        };
        let problems = route.validators.problems(&input);
        if !problems.is_empty() {
            return Err(input::validation_error(problems, &route.spec, &route.usage));
        }
        Ok(input)
    }

    async fn call_extension(&self, extension: &Mounted, route: &Route, call: WitRequest, input: &Input, if_none_match: Option<&str>) -> Res {
        let id = call.request_id.clone();
        let Some(rule) = route.cache else {
            return reply::from_produced(call_runner(&extension.runner, call, route.timeout).await, &id);
        };
        let key = format!("{}\u{0}{}\u{0}{}\u{0}{}", extension.runner.mount, route.index, input.params, input.query);
        let (runner, timeout) = (extension.runner.clone(), route.timeout);
        let producer: Producer = Arc::new(move || {
            let (runner, call) = (runner.clone(), call.clone());
            Box::pin(async move { call_runner(&runner, call, timeout).await })
        });
        match self.cache.get(&key, rule, producer).await {
            Outcome::Cached(entry, state) => entry.to_response(state, rule, if_none_match),
            Outcome::Uncached(produced) => reply::from_produced(produced, &id),
        }
    }
}

async fn call_runner(runner: &Runner, call: WitRequest, timeout: Duration) -> Produced {
    let id = call.request_id.as_str();
    let outcome = match tokio::time::timeout(timeout + Duration::from_secs(1), runner.handle(&call, timeout)).await {
        Ok(outcome) => outcome,
        Err(_) => Err(CallError::Timeout),
    };
    match outcome {
        Ok(Ok(response)) => Produced {
            status: response.status,
            headers: response.headers.into_iter().filter(|(name, _)| !HEADERS_EXTENSIONS_CANT_SET.contains(&name.as_str())).collect(),
            body: Bytes::from(response.body),
        },
        Ok(Err(error)) => extension_error(error).into_produced(id),
        Err(CallError::Timeout) => ErrorReply::new(504, "timeout", format!("The request took longer than {} ms", timeout.as_millis())).into_produced(id),
        Err(CallError::Busy) => {
            let mut produced = ErrorReply::new(503, "extension_busy", "The extension is at capacity").hint("Retry in a moment").into_produced(id);
            produced.headers.push(("retry-after".into(), "1".into()));
            produced
        }
        Err(crash @ CallError::Crashed(_)) => {
            log_error!(&runner.mount, "[{id}] crashed: {crash}");
            ErrorReply::new(500, "internal_error", "The extension failed while handling this request")
                .hint("The details are in the server log under this request id")
                .into_produced(id)
        }
    }
}

fn no_matching_route(unmatched: Match<'_, '_>, extension: &Mounted, method: &Method, path: &str, id: &str) -> Res {
    match unmatched {
        Match::WrongMethod { allowed } => {
            let mut response = ErrorReply::new(405, "method_not_allowed", format!("{method} is not allowed here"))
                .hint(format!("Use {}", allowed.join(" or ")))
                .into_response(id);
            response.headers_mut().insert("allow", allowed.join(", ").parse().expect("method names"));
            response
        }
        _ => {
            let routes: Vec<&str> = extension.routes.iter().map(|r| r.usage.as_str()).collect();
            ErrorReply::new(404, "not_found", format!("No route matches {method} {path}"))
                .hint(format!("GET /{} describes every route", extension.runner.mount))
                .details(json!({ "routes": routes }))
                .into_response(id)
        }
    }
}

fn extension_error(error: WitError) -> ErrorReply {
    let code = error.code.unwrap_or_else(|| reply::default_code(error.status).to_owned());
    let mut reply = ErrorReply::new(error.status, code, error.message);
    if let Some(hint) = error.hint {
        reply = reply.hint(hint);
    }
    if let Some(details) = error.details_json.and_then(|d| serde_json::from_str(&d).ok()) {
        reply = reply.details(details);
    }
    reply
}

fn preflight(request_headers: &HeaderMap) -> Res {
    let allow_headers = request_headers.get("access-control-request-headers").and_then(|v| v.to_str().ok()).unwrap_or("content-type, authorization");
    let mut response = reply::empty(204);
    let headers = response.headers_mut();
    headers.insert("access-control-allow-origin", "*".parse().expect("static"));
    headers.insert("access-control-allow-methods", "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS".parse().expect("static"));
    headers.insert("access-control-max-age", "86400".parse().expect("static"));
    if let Ok(value) = allow_headers.parse() {
        headers.insert("access-control-allow-headers", value);
    }
    response
}

fn forwarded_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| is_forwarded_header(name.as_str()))
        .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
        .collect()
}

fn set_rate_headers(headers: &mut HeaderMap, decision: &Decision) {
    headers.insert("ratelimit-limit", decision.limit.into());
    headers.insert("ratelimit-remaining", decision.remaining.into());
    headers.insert("ratelimit-reset", decision.reset_sec.into());
}
