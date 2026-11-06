use std::{fmt::Display, rc::Rc, time::Duration};

use serde::{Serialize, de::DeserializeOwned};

use crate::{__bindings::samishisa::extension::host, ApiError, Result, extension::whole_seconds};

/// The config type of an extension without settings.
#[derive(Debug, Default, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoConfig {}

/// The services an extension can use: config, storage, logging and outbound HTTP.
///
/// Take it as a handler argument, in any position:
///
/// ```no_run
/// use samishisa::prelude::*;
/// # #[schema] struct Config { greeting: String }
/// fn hello(ctx: Ctx<Config>) -> String {
///     ctx.log.info("someone said hello");
///     format!("{}!", ctx.config.greeting)
/// }
/// ```
///
/// `setup` and scheduled jobs get one too, without a request id. Cloning is cheap.
pub struct Ctx<C = NoConfig> {
    /// Validated config, parsed from the operator's `api.config.json` entry.
    pub config: Rc<C>,
    /// Key/value storage, private to this extension.
    pub store: Store,
    /// Server log, tagged with the extension name and, in handlers, the request id.
    pub log: Log,
    /// Outbound HTTP, limited to the hosts in [`Extension::network`](crate::Extension::network).
    pub http: Http,
    request_id: Option<Rc<str>>,
}

impl<C> Clone for Ctx<C> {
    fn clone(&self) -> Self {
        Self { config: self.config.clone(), store: Store(()), log: self.log.clone(), http: Http(()), request_id: self.request_id.clone() }
    }
}

impl<C> Ctx<C> {
    pub(crate) fn new(config: Rc<C>, request_id: Option<Rc<str>>) -> Self {
        Self { config, store: Store(()), log: Log { request_id: request_id.clone() }, http: Http(()), request_id }
    }

    /// Id of the request being handled, also sent to the caller as `x-request-id`.
    /// `None` in `setup` and scheduled jobs.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }
}

/// Key/value storage that outlives restarts. Values are stored as JSON, so anything `Serialize` works.
///
/// All instances share it, and it survives restarts, so keep anything that must be remembered here. Instances come and go.
///
/// ```no_run
/// # use samishisa::prelude::*;
/// # fn f(ctx: Ctx) -> Result<()> {
/// ctx.store.set("last-city", &"london")?;
/// let city: Option<String> = ctx.store.get("last-city");
/// let visits = ctx.store.incr("visits", 1);
/// ctx.store.set_for("session:abc", &42, Duration::from_secs(3600))?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Store(());

impl Store {
    /// Reads a value. `None` if the key is missing, expired, or holds JSON that doesn't fit `T` (logged as a warning).
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let raw = host::store_get(key)?;
        match serde_json::from_str(&raw) {
            Ok(v) => Some(v),
            Err(e) => {
                host::log(host::LogLevel::Warn, &format!("store key {key:?} holds a value of another shape: {e}"));
                None
            }
        }
    }

    /// Writes a value, replacing any previous one. Fails only if `value` can't be serialized to JSON.
    pub fn set<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
        host::store_set(key, &serde_json::to_string(value)?, None);
        Ok(())
    }

    /// Like [`set`](Self::set), but the key expires after `ttl`.
    pub fn set_for<T: Serialize + ?Sized>(&self, key: &str, value: &T, ttl: Duration) -> Result<()> {
        host::store_set(key, &serde_json::to_string(value)?, Some(whole_seconds(ttl)));
        Ok(())
    }

    /// Removes a key. Does nothing if it's missing.
    pub fn delete(&self, key: &str) {
        host::store_delete(key)
    }

    /// Adds `by` to a counter and returns the new value. A missing key counts as 0. Concurrent increments never lose an update.
    pub fn incr(&self, key: &str, by: i64) -> i64 {
        host::store_incr(key, by)
    }

    /// All keys that start with `prefix`, sorted (`""` lists everything).
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        host::store_keys(prefix)
    }
}

/// Write to the server log.
#[derive(Debug, Clone)]
pub struct Log {
    request_id: Option<Rc<str>>,
}

impl Log {
    fn write(&self, level: host::LogLevel, msg: impl Display) {
        match &self.request_id {
            Some(id) => host::log(level, &format!("[{id}] {msg}")),
            None => host::log(level, &msg.to_string()),
        }
    }
    /// Verbose detail, shown only with `LOG_LEVEL=debug`.
    pub fn debug(&self, msg: impl Display) {
        self.write(host::LogLevel::Debug, msg)
    }
    /// Routine progress.
    pub fn info(&self, msg: impl Display) {
        self.write(host::LogLevel::Info, msg)
    }
    /// Something unexpected that was handled.
    pub fn warn(&self, msg: impl Display) {
        self.write(host::LogLevel::Warn, msg)
    }
    /// Something failed.
    pub fn error(&self, msg: impl Display) {
        self.write(host::LogLevel::Error, msg)
    }
}

/// Outbound HTTP, the only network access an extension has.
///
/// Requests reach only the hosts declared with [`Extension::network`](crate::Extension::network). Private and internal addresses are blocked, redirects are checked again, and every request has a timeout and a size cap. Failures come back as `502` or `504` [`ApiError`]s, so `?` works.
///
/// ```no_run
/// # use samishisa::prelude::*;
/// #[schema]
/// struct Forecast { temperature: f64 }
///
/// # fn f(ctx: Ctx) -> Result<Json<Forecast>> {
/// let forecast: Forecast = ctx.http
///     .get("https://api.weather.example/v1/forecast")
///     .query(&[("city", "london"), ("units", "metric")])
///     .bearer("my-api-key")
///     .send()?
///     .error_for_status()?
///     .json()?;
/// Ok(Json(forecast))
/// # }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Http(());

impl Http {
    /// Start a request with any method.
    pub fn request(&self, method: &str, url: impl Into<String>) -> HttpRequestBuilder {
        HttpRequestBuilder { method: method.to_ascii_uppercase(), url: url.into(), headers: Vec::new(), body: None }
    }
    /// Start a `GET` request.
    pub fn get(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("GET", url)
    }
    /// Start a `POST` request.
    pub fn post(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("POST", url)
    }
    /// Start a `PUT` request.
    pub fn put(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("PUT", url)
    }
    /// Start a `PATCH` request.
    pub fn patch(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("PATCH", url)
    }
    /// Start a `DELETE` request.
    pub fn delete(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("DELETE", url)
    }
}

/// Outbound request under construction. Finish it with [`send`](Self::send).
#[derive(Debug, Clone)]
#[must_use = "a request does nothing until you call .send()"]
pub struct HttpRequestBuilder {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl HttpRequestBuilder {
    /// Add a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    /// Add `Authorization: Bearer <token>`.
    pub fn bearer(self, token: impl Display) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }
    /// Append query parameters, percent-encoded: `.query(&[("q", "new york")])`.
    pub fn query<K: AsRef<str>, V: AsRef<str>>(mut self, pairs: &[(K, V)]) -> Self {
        for (k, v) in pairs {
            self.url.push(if self.url.contains('?') { '&' } else { '?' });
            self.url.push_str(&encode(k.as_ref()));
            self.url.push('=');
            self.url.push_str(&encode(v.as_ref()));
        }
        self
    }
    /// Send `value` as a JSON body (sets `content-type: application/json`).
    pub fn json<T: Serialize + ?Sized>(mut self, value: &T) -> Result<Self> {
        self.body = Some(serde_json::to_vec(value)?);
        Ok(self.header("content-type", "application/json"))
    }
    /// Send raw bytes as the body. Set a `content-type` header yourself.
    pub fn body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = Some(bytes.into());
        self
    }
    /// Sends the request. An `Err` means no answer arrived: the network policy blocked it, the host is unreachable, the request timed out, or the response was too large.
    /// An HTTP error status is still `Ok`. Use [`HttpResponse::error_for_status`] to treat it as a failure.
    pub fn send(self) -> Result<HttpResponse> {
        let req = host::HttpRequest { method: self.method, url: self.url, headers: self.headers, body: self.body };
        let res = host::fetch(&req).map_err(ApiError::from_wit)?;
        Ok(HttpResponse { status: res.status, headers: res.headers, body: res.body, url: req.url })
    }
}

/// The answer to an outbound request.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    url: String,
}

impl HttpResponse {
    /// The HTTP status, e.g. 200.
    pub fn status(&self) -> u16 {
        self.status
    }
    /// The first header with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    /// All headers, in the order received (lowercase names).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
    /// The raw body.
    pub fn bytes(&self) -> &[u8] {
        &self.body
    }
    /// Body as text. Invalid UTF-8 becomes replacement characters.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// Parses the body as JSON. A body that doesn't fit `T` becomes a `502`: the upstream sent something unexpected.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|e| {
            let mut err = ApiError::upstream("An upstream service returned an unexpected response").code("upstream_invalid_response");
            err.internal = Some(format!("{}: {e}", self.url));
            err
        })
    }
    /// Turns a non-2xx status into a `502` [`ApiError`]. A `404` from upstream stays a `502`, because your dependency failed, not the caller's request. Returns `self` otherwise.
    pub fn error_for_status(self) -> Result<Self> {
        if (200..300).contains(&self.status) {
            return Ok(self);
        }
        let mut err = ApiError::upstream(format!("An upstream service answered {}", self.status)).code("upstream_error");
        err.internal = Some(format!("{} -> {}: {}", self.url, self.status, String::from_utf8_lossy(&self.body[..self.body.len().min(300)])));
        Err(err)
    }
}

/// Percent-encode a query component (RFC 3986 unreserved characters stay as they are).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
