use std::time::Duration;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{Ctx, NoConfig, RequestParts, Response, Result, handler::Handler};

/// A config type: a struct with `#[schema]`. Unknown keys are rejected unless you add a `#[serde(flatten)]` map.
pub trait Config: DeserializeOwned + JsonSchema + 'static {}
impl<T: DeserializeOwned + JsonSchema + 'static> Config for T {}

/// HTTP methods a route can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// Only `GET` responses can be cached.
    Get,
    /// `POST`: create something or run an action.
    Post,
    /// `PUT`: replace something.
    Put,
    /// `PATCH`: change part of something.
    Patch,
    /// `DELETE`: remove something.
    Delete,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Method {
    /// The method as text, e.g. `GET`.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// How often callers may use a route. The server enforces it before your handler runs.
///
/// ```
/// use samishisa::RateLimit;
/// let per_client = RateLimit::per_minute(60);
/// let shared = RateLimit::per_minute(100).global();
/// let custom = RateLimit::new(5, std::time::Duration::from_secs(10));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a RateLimit does nothing until you pass it to .rate_limit(...)"]
pub struct RateLimit {
    limit: u32,
    window_sec: u32,
    global: bool,
}

impl RateLimit {
    /// `limit` requests per `window` (rounded up to whole seconds, at least 1).
    pub fn new(limit: u32, window: Duration) -> Self {
        Self { limit, window_sec: whole_seconds(window), global: false }
    }
    /// `limit` requests per second.
    pub fn per_second(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(1))
    }
    /// `limit` requests per minute.
    pub fn per_minute(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(60))
    }
    /// `limit` requests per hour.
    pub fn per_hour(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(3600))
    }
    /// One budget shared by every caller, instead of one per caller. Use it to protect an upstream with its own quota.
    pub fn global(mut self) -> Self {
        self.global = true;
        self
    }

    pub(crate) fn to_json(self) -> Value {
        json!({ "limit": self.limit, "windowSec": self.window_sec, "scope": if self.global { "global" } else { "client" } })
    }
}

/// `duration` in whole seconds, rounded up, at least 1.
pub(crate) fn whole_seconds(duration: Duration) -> u32 {
    let secs = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    u32::try_from(secs.max(1)).unwrap_or(u32::MAX)
}

/// A route's own rate-limit choice.
#[derive(Debug, Clone, Copy)]
enum RouteRateLimit {
    /// The extension's default (or the server's).
    Inherit,
    Own(RateLimit),
    Off,
}

#[derive(Debug, Clone, Copy)]
struct CacheRule {
    ttl_secs: u32,
    stale_secs: u32,
}

/// What the server knows about one route, derived from a handler's argument and return types.
///
/// You only need this to write your own [`FromRequest`](crate::FromRequest) or [`IntoResponse`](crate::IntoResponse) implementation, to declare a schema for the docs and validation.
#[derive(Debug, Clone)]
pub struct RouteSpec {
    method: Method,
    path: String,
    summary: Option<String>,
    description: Option<String>,
    params: Option<Value>,
    query: Option<Value>,
    body: Option<Value>,
    response: Option<Value>,
    cache: Option<CacheRule>,
    rate_limit: RouteRateLimit,
    timeout_ms: Option<u64>,
    pub(crate) errors: Vec<String>,
}

impl RouteSpec {
    fn new(method: Method, path: &str) -> Self {
        Self {
            method,
            path: path.to_owned(),
            summary: None,
            description: None,
            params: None,
            query: None,
            body: None,
            response: None,
            cache: None,
            rate_limit: RouteRateLimit::Inherit,
            timeout_ms: None,
            errors: Vec::new(),
        }
    }
    /// The route's method.
    pub fn method(&self) -> Method {
        self.method
    }
    /// The route's path relative to the extension, e.g. `"/:city"`.
    pub fn path(&self) -> &str {
        &self.path
    }
    /// Names of the `:params` in the path, in order.
    pub fn param_names(&self) -> Vec<&str> {
        self.path.split('/').filter_map(|s| s.strip_prefix(':')).collect()
    }
    /// JSON Schema of the path params: an object with one property per `:param`.
    pub fn set_params(&mut self, schema: Value) {
        self.params = Some(schema);
    }
    /// JSON Schema of the query string (an object).
    pub fn set_query(&mut self, schema: Value) {
        self.query = Some(schema);
    }
    /// JSON Schema of the request body.
    pub fn set_body(&mut self, schema: Value) {
        self.body = Some(schema);
    }
    /// JSON Schema of a successful response. Documentation only.
    pub fn set_response(&mut self, schema: Value) {
        self.response = Some(schema);
    }
    /// Reports a mistake in the route definition. The server refuses to load the extension and shows the message.
    pub fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    pub(crate) fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("method".into(), json!(self.method.as_str()));
        o.insert("path".into(), json!(self.path));
        let mut put = |k: &str, val: Option<Value>| {
            if let Some(val) = val {
                o.insert(k.to_owned(), val);
            }
        };
        put("summary", self.summary.clone().map(Value::String));
        put("description", self.description.clone().map(Value::String));
        put("params", self.params.clone());
        put("query", self.query.clone());
        put("body", self.body.clone());
        put("response", self.response.clone());
        put("cache", self.cache.map(|c| json!({ "ttl": c.ttl_secs, "staleTtl": c.stale_secs })));
        put(
            "rateLimit",
            match self.rate_limit {
                RouteRateLimit::Inherit => None,
                RouteRateLimit::Own(limit) => Some(limit.to_json()),
                RouteRateLimit::Off => Some(Value::Bool(false)),
            },
        );
        put("timeoutMs", self.timeout_ms.map(Value::from));
        Value::Object(o)
    }
}

pub(crate) type BoxedHandler<C> = Box<dyn Fn(&RequestParts<C>) -> Result<Response>>;
pub(crate) type BoxedTask<C> = Box<dyn Fn(&Ctx<C>) -> Result<()>>;

pub(crate) struct Route<C> {
    pub(crate) spec: RouteSpec,
    pub(crate) handler: BoxedHandler<C>,
}

pub(crate) struct Job<C> {
    pub(crate) every: Duration,
    pub(crate) run: BoxedTask<C>,
}

/// The extension: its name, routes and settings, built as a chain.
///
/// ```no_run
/// use samishisa::prelude::*;
///
/// #[schema]
/// struct Config { api_key: String }
///
/// #[schema]
/// struct City { name: String, temperature: f64 }
///
/// fn current(Path(city): Path<String>, ctx: Ctx<Config>) -> Result<Json<City>> {
///     let res = ctx.http.get(format!("https://api.weather.example/v1/{city}"))
///         .bearer(&ctx.config.api_key)
///         .send()?;
///     if res.status() == 404 {
///         return Err(ApiError::not_found(format!("Unknown city {city:?}")).hint("Try /weather/london"));
///     }
///     Ok(Json(res.error_for_status()?.json()?))
/// }
///
/// fn extension() -> Extension<Config> {
///     Extension::new("weather")
///         .about("Current weather by city")
///         .network(["api.weather.example"])
///         .default_rate_limit(RateLimit::per_minute(60))
///         .get("/:city", current)
///             .summary("Current weather for a city")
///             .cache_stale(60, 300)
/// }
/// samishisa::export!(extension);
/// ```
///
/// Route settings ([`summary`](Self::summary), [`cache`](Self::cache), [`rate_limit`](Self::rate_limit), [`timeout`](Self::timeout), ...) apply to the route added just before them. Indent them under it, as above, so that's visible.
///
/// Routes can be added in any order. A static segment beats a `:param`: `/visits` wins over `/:name`.
#[must_use = "an Extension does nothing until it is returned to samishisa::export!"]
pub struct Extension<C = NoConfig> {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) about: Option<String>,
    pub(crate) network: Option<Value>,
    pub(crate) rate_limit: Option<RateLimit>,
    pub(crate) routes: Vec<Route<C>>,
    pub(crate) jobs: Vec<Job<C>>,
    pub(crate) setup: Option<BoxedTask<C>>,
    pub(crate) errors: Vec<String>,
}

impl<C: Config> Extension<C> {
    /// Starts an extension mounted at `/<name>/...`. Names are lowercase letters, digits and dashes, starting with a letter, up to 40 characters. The operator can mount it under another name.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let mut errors = Vec::new();
        let valid = name.len() <= 40 && name.starts_with(|c: char| c.is_ascii_lowercase()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            errors.push(format!("invalid extension name {name:?}: use lowercase letters, digits and dashes, starting with a letter"));
        }
        Self { name, version: None, about: None, network: None, rate_limit: None, routes: Vec::new(), jobs: Vec::new(), setup: None, errors }
    }

    /// Override the version shown in the docs (default: `version` from your `Cargo.toml`).
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// One line describing the extension (default: `description` from your `Cargo.toml`).
    pub fn about(mut self, text: impl Into<String>) -> Self {
        self.about = Some(text.into());
        self
    }

    /// Hosts that [`Ctx::http`](crate::Ctx::http) may contact, e.g. `["api.example.com", "*.cdn.example.com"]`. Without this, the extension has no network access. The operator can override the list.
    pub fn network<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.network = Some(Value::Array(hosts.into_iter().map(|h| Value::String(h.into())).collect()));
        self
    }

    /// Allow requests to any public host. Private and internal addresses stay blocked.
    pub fn network_any(mut self) -> Self {
        self.network = Some(Value::String("*".into()));
        self
    }

    /// The rate limit of every route that doesn't set its own (default: the server's global limit).
    pub fn default_rate_limit(mut self, limit: RateLimit) -> Self {
        self.rate_limit = Some(limit);
        self
    }

    /// Runs `f` once per server start, before serving: seed the store, check that an upstream key works, and so on. An error stops the extension from loading and is shown to the operator.
    pub fn setup(mut self, f: impl Fn(&Ctx<C>) -> Result<()> + 'static) -> Self {
        self.setup = Some(Box::new(f));
        self
    }

    /// Runs `f` every `interval` on one instance at a time, even with several threads. Errors are logged and the schedule continues. Share results through the store.
    pub fn every(mut self, interval: Duration, f: impl Fn(&Ctx<C>) -> Result<()> + 'static) -> Self {
        if interval < Duration::from_secs(1) {
            self.errors.push(format!("every({interval:?}): the shortest interval is 1 second"));
        }
        self.jobs.push(Job { every: interval, run: Box::new(f) });
        self
    }

    /// Adds a route. `path` is relative to the extension and starts with `/`. `:name` segments are path params. Usually you call [`get`](Self::get), [`post`](Self::post) and friends instead.
    pub fn route<H, Args>(mut self, method: Method, path: &str, handler: H) -> Self
    where
        H: Handler<C, Args>,
    {
        let mut spec = RouteSpec::new(method, path);
        if !path.starts_with('/') {
            spec.error(format!("route path {path:?} must start with '/'"));
        }
        let names: Vec<String> = spec.param_names().into_iter().map(String::from).collect();
        for (i, name) in names.iter().enumerate() {
            if name.is_empty() || names[..i].contains(name) {
                spec.error(format!("route {path:?}: path params need distinct, non-empty names"));
            }
        }
        if self.routes.iter().any(|r| r.spec.method == method && same_shape(&r.spec.path, path)) {
            spec.error(format!("{} {path} is defined twice", method.as_str()));
        }
        H::describe(&mut spec);
        self.routes.push(Route { spec, handler: Box::new(move |req| handler.call(req)) });
        self
    }

    /// Add a `GET` route: read something.
    pub fn get<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Get, path, handler)
    }
    /// Add a `POST` route: create something or run an action.
    pub fn post<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Post, path, handler)
    }
    /// Add a `PUT` route: replace something.
    pub fn put<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Put, path, handler)
    }
    /// Add a `PATCH` route: change part of something.
    pub fn patch<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Patch, path, handler)
    }
    /// Add a `DELETE` route: remove something.
    pub fn delete<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Delete, path, handler)
    }

    fn last(&mut self, setting: &str) -> Option<&mut RouteSpec> {
        if self.routes.is_empty() {
            self.errors.push(format!(".{setting}() was called before any route: route settings apply to the route added just before them"));
        }
        self.routes.last_mut().map(|r| &mut r.spec)
    }

    /// One-line summary for the docs.
    pub fn summary(mut self, text: impl Into<String>) -> Self {
        if let Some(r) = self.last("summary") {
            r.summary = Some(text.into());
        }
        self
    }

    /// Longer description for the docs, in Markdown.
    pub fn doc(mut self, text: impl Into<String>) -> Self {
        if let Some(r) = self.last("doc") {
            r.description = Some(text.into());
        }
        self
    }

    /// Caches successful `GET` responses for `ttl_secs` seconds, shared by all callers. Cached hits never reach your handler. The server sends `ETag` and `Cache-Control`, so browsers and CDNs can cache too.
    ///
    /// The cache key is the path params and query only, so the answer must not depend on the caller. Requests with an `Authorization` header skip the cache and get a `private` answer. Responses that set a cookie are never cached.
    pub fn cache(self, ttl_secs: u32) -> Self {
        self.cache_stale(ttl_secs, 0)
    }

    /// Like [`cache`](Self::cache), but after expiry it keeps serving the old response for up to `stale_secs` more seconds while one refresh runs in the background.
    pub fn cache_stale(mut self, ttl_secs: u32, stale_secs: u32) -> Self {
        if let Some(r) = self.last("cache") {
            if r.method != Method::Get {
                let msg = format!("{} {}: only GET routes can be cached", r.method.as_str(), r.path);
                r.error(msg);
            }
            r.cache = Some(CacheRule { ttl_secs, stale_secs });
        }
        self
    }

    /// This route's rate limit, instead of the extension's default.
    pub fn rate_limit(mut self, limit: RateLimit) -> Self {
        if let Some(r) = self.last("rate_limit") {
            r.rate_limit = RouteRateLimit::Own(limit);
        }
        self
    }

    /// Exempts this route from rate limiting, e.g. for a health check.
    pub fn no_rate_limit(mut self) -> Self {
        if let Some(r) = self.last("no_rate_limit") {
            r.rate_limit = RouteRateLimit::Off;
        }
        self
    }

    /// How long a call may take before the caller gets `504`. Defaults to the server's request timeout. Time spent in `ctx.http` counts too.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        if let Some(r) = self.last("timeout") {
            r.timeout_ms = Some(u64::try_from(timeout.as_millis().max(1)).unwrap_or(u64::MAX));
        }
        self
    }
}

/// `/a/:x` and `/a/:y` are the same route.
fn same_shape(a: &str, b: &str) -> bool {
    fn norm(p: &str) -> Vec<&str> {
        p.split('/').map(|s| if s.starts_with(':') { ":" } else { s }).collect()
    }
    norm(a) == norm(b)
}

/// JSON Schema (draft 2020-12) of `T`, generated from its `#[schema]` derive. Drops the `$schema` marker and titles of plain values such as `"title": "string"`.
pub(crate) fn schema_of<T: JsonSchema>() -> Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let mut schema = serde_json::to_value(generator.into_root_schema_for::<T>()).unwrap_or(Value::Bool(true));
    let title_says_something = is_object_schema(&schema) || schema.get("enum").is_some() || schema.get("oneOf").is_some();
    if let Value::Object(o) = &mut schema {
        o.remove("$schema");
        if !title_says_something {
            o.remove("title");
        }
    }
    schema
}

/// Config schema: like [`schema_of`], but unknown keys are rejected, so typos in `api.config.json` are reported.
pub(crate) fn config_schema_of<T: JsonSchema>() -> Value {
    let mut v = schema_of::<T>();
    if is_object_schema(&v)
        && let Value::Object(o) = &mut v
    {
        o.entry("additionalProperties").or_insert(Value::Bool(false));
    }
    v
}

/// Whether a schema describes an object with named properties (vs. a single value).
pub(crate) fn is_object_schema(s: &Value) -> bool {
    s.get("type").and_then(Value::as_str) == Some("object") || s.get("properties").is_some()
}
