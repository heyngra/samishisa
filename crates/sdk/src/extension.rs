use std::time::Duration;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{Ctx, NoConfig, RequestParts, Response, Result, handler::Handler};

pub trait Config: DeserializeOwned + JsonSchema + 'static {}
impl<T: DeserializeOwned + JsonSchema + 'static> Config for T {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    limit: u32,
    window_sec: u32,
    global: bool,
}

impl RateLimit {
    pub fn new(limit: u32, window: Duration) -> Self {
        let secs = window.as_secs() + u64::from(window.subsec_nanos() > 0);
        Self { limit, window_sec: secs.clamp(1, u32::MAX as u64) as u32, global: false }
    }
    pub fn per_second(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(1))
    }
    pub fn per_minute(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(60))
    }
    pub fn per_hour(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(3600))
    }
    pub fn global(mut self) -> Self {
        self.global = true;
        self
    }

    pub(crate) fn to_json(self) -> Value {
        json!({ "limit": self.limit, "windowSec": self.window_sec, "scope": if self.global { "global" } else { "client" } })
    }
}

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
    cache: Option<(u32, u32)>,
    rate_limit: Option<Option<RateLimit>>,
    timeout_ms: Option<u64>,
    pub(crate) errors: Vec<String>,
}

impl RouteSpec {
    fn new(method: Method, path: &str) -> Self {
        Self {
            method, path: path.to_owned(), summary: None, description: None, params: None, query: None, body: None,
            response: None, cache: None, rate_limit: None, timeout_ms: None, errors: Vec::new(),
        }
    }
    pub fn method(&self) -> Method {
        self.method
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn param_names(&self) -> Vec<&str> {
        self.path.split('/').filter_map(|s| s.strip_prefix(':')).collect()
    }
    pub fn set_params(&mut self, schema: Value) {
        self.params = Some(schema);
    }
    pub fn set_query(&mut self, schema: Value) {
        self.query = Some(schema);
    }
    pub fn set_body(&mut self, schema: Value) {
        self.body = Some(schema);
    }
    pub fn set_response(&mut self, schema: Value) {
        self.response = Some(schema);
    }
    pub fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    pub(crate) fn to_json(&self) -> Value {
        let mut v = json!({ "method": self.method.as_str(), "path": self.path });
        let o = v.as_object_mut().expect("object");
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
        put("cache", self.cache.map(|(ttl, stale)| json!({ "ttl": ttl, "staleTtl": stale })));
        put("rateLimit", self.rate_limit.map(|r| r.map_or(Value::Bool(false), RateLimit::to_json)));
        put("timeoutMs", self.timeout_ms.map(Value::from));
        v
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
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let mut errors = Vec::new();
        let valid = name.len() <= 40
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            errors.push(format!("invalid extension name {name:?}: use lowercase letters, digits and dashes, starting with a letter"));
        }
        Self { name, version: None, about: None, network: None, rate_limit: None, routes: Vec::new(), jobs: Vec::new(), setup: None, errors }
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    pub fn about(mut self, text: impl Into<String>) -> Self {
        self.about = Some(text.into());
        self
    }

    pub fn network<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.network = Some(Value::Array(hosts.into_iter().map(|h| Value::String(h.into())).collect()));
        self
    }

    pub fn network_any(mut self) -> Self {
        self.network = Some(Value::String("*".into()));
        self
    }

    pub fn default_rate_limit(mut self, limit: RateLimit) -> Self {
        self.rate_limit = Some(limit);
        self
    }

    pub fn setup(mut self, f: impl Fn(&Ctx<C>) -> Result<()> + 'static) -> Self {
        self.setup = Some(Box::new(f));
        self
    }

    pub fn every(mut self, interval: Duration, f: impl Fn(&Ctx<C>) -> Result<()> + 'static) -> Self {
        if interval < Duration::from_secs(1) {
            self.errors.push(format!("every({interval:?}): the shortest interval is 1 second"));
        }
        self.jobs.push(Job { every: interval, run: Box::new(f) });
        self
    }

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

    pub fn get<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Get, path, handler)
    }
    pub fn post<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Post, path, handler)
    }
    pub fn put<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Put, path, handler)
    }
    pub fn patch<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Patch, path, handler)
    }
    pub fn delete<H: Handler<C, Args>, Args>(self, path: &str, handler: H) -> Self {
        self.route(Method::Delete, path, handler)
    }

    fn last(&mut self, setting: &str) -> Option<&mut RouteSpec> {
        if self.routes.is_empty() {
            self.errors.push(format!(".{setting}() was called before any route: route settings apply to the route added just before them"));
        }
        self.routes.last_mut().map(|r| &mut r.spec)
    }

    pub fn summary(mut self, text: impl Into<String>) -> Self {
        if let Some(r) = self.last("summary") {
            r.summary = Some(text.into());
        }
        self
    }

    pub fn doc(mut self, text: impl Into<String>) -> Self {
        if let Some(r) = self.last("doc") {
            r.description = Some(text.into());
        }
        self
    }

    pub fn cache(self, ttl_secs: u32) -> Self {
        self.cache_stale(ttl_secs, 0)
    }

    pub fn cache_stale(mut self, ttl_secs: u32, stale_secs: u32) -> Self {
        if let Some(r) = self.last("cache") {
            if r.method != Method::Get {
                let msg = format!("{} {}: only GET routes can be cached", r.method.as_str(), r.path);
                r.error(msg);
            }
            r.cache = Some((ttl_secs, stale_secs));
        }
        self
    }

    pub fn rate_limit(mut self, limit: RateLimit) -> Self {
        if let Some(r) = self.last("rate_limit") {
            r.rate_limit = Some(Some(limit));
        }
        self
    }

    pub fn no_rate_limit(mut self) -> Self {
        if let Some(r) = self.last("no_rate_limit") {
            r.rate_limit = Some(None);
        }
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        if let Some(r) = self.last("timeout") {
            r.timeout_ms = Some(timeout.as_millis().max(1) as u64);
        }
        self
    }
}

fn same_shape(a: &str, b: &str) -> bool {
    fn norm(p: &str) -> Vec<&str> {
        p.split('/').map(|s| if s.starts_with(':') { ":" } else { s }).collect()
    }
    norm(a) == norm(b)
}

pub(crate) fn schema_of<T: JsonSchema>() -> Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let mut v = serde_json::to_value(generator.into_root_schema_for::<T>()).unwrap_or(Value::Bool(true));
    if let Value::Object(o) = &mut v {
        o.remove("$schema");
        if !is_object_schema(&Value::Object(o.clone())) && o.get("enum").is_none() && o.get("oneOf").is_none() {
            o.remove("title");
        }
    }
    v
}

pub(crate) fn config_schema_of<T: JsonSchema>() -> Value {
    let mut v = schema_of::<T>();
    if is_object_schema(&v) {
        if let Value::Object(o) = &mut v {
            o.entry("additionalProperties").or_insert(Value::Bool(false));
        }
    }
    v
}

pub(crate) fn is_object_schema(s: &Value) -> bool {
    s.get("type").and_then(Value::as_str) == Some("object") || s.get("properties").is_some()
}
