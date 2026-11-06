use std::{fmt::Display, rc::Rc, time::Duration};

use serde::{Serialize, de::DeserializeOwned};

use crate::{ApiError, Result, __bindings::samishisa::extension::host, extension::whole_seconds};

#[derive(Debug, Default, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoConfig {}

pub struct Ctx<C = NoConfig> {
    pub config: Rc<C>,
    pub store: Store,
    pub log: Log,
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

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Store(());

impl Store {
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

    pub fn set<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
        host::store_set(key, &serde_json::to_string(value)?, None);
        Ok(())
    }

    pub fn set_for<T: Serialize + ?Sized>(&self, key: &str, value: &T, ttl: Duration) -> Result<()> {
        host::store_set(key, &serde_json::to_string(value)?, Some(whole_seconds(ttl)));
        Ok(())
    }

    pub fn delete(&self, key: &str) {
        host::store_delete(key)
    }

    pub fn incr(&self, key: &str, by: i64) -> i64 {
        host::store_incr(key, by)
    }

    pub fn keys(&self, prefix: &str) -> Vec<String> {
        host::store_keys(prefix)
    }
}

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
    pub fn debug(&self, msg: impl Display) {
        self.write(host::LogLevel::Debug, msg)
    }
    pub fn info(&self, msg: impl Display) {
        self.write(host::LogLevel::Info, msg)
    }
    pub fn warn(&self, msg: impl Display) {
        self.write(host::LogLevel::Warn, msg)
    }
    pub fn error(&self, msg: impl Display) {
        self.write(host::LogLevel::Error, msg)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Http(());

impl Http {
    pub fn request(&self, method: &str, url: impl Into<String>) -> HttpRequestBuilder {
        HttpRequestBuilder { method: method.to_ascii_uppercase(), url: url.into(), headers: Vec::new(), body: None }
    }
    pub fn get(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("GET", url)
    }
    pub fn post(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("POST", url)
    }
    pub fn put(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("PUT", url)
    }
    pub fn patch(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("PATCH", url)
    }
    pub fn delete(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request("DELETE", url)
    }
}

#[derive(Debug, Clone)]
#[must_use = "a request does nothing until you call .send()"]
pub struct HttpRequestBuilder {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl HttpRequestBuilder {
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn bearer(self, token: impl Display) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }
    pub fn query<K: AsRef<str>, V: AsRef<str>>(mut self, pairs: &[(K, V)]) -> Self {
        for (k, v) in pairs {
            self.url.push(if self.url.contains('?') { '&' } else { '?' });
            self.url.push_str(&encode(k.as_ref()));
            self.url.push('=');
            self.url.push_str(&encode(v.as_ref()));
        }
        self
    }
    pub fn json<T: Serialize + ?Sized>(mut self, value: &T) -> Result<Self> {
        self.body = Some(serde_json::to_vec(value)?);
        Ok(self.header("content-type", "application/json"))
    }
    pub fn body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = Some(bytes.into());
        self
    }
    pub fn send(self) -> Result<HttpResponse> {
        let req = host::HttpRequest { method: self.method, url: self.url, headers: self.headers, body: self.body };
        let res = host::fetch(&req).map_err(ApiError::from_wit)?;
        Ok(HttpResponse { status: res.status, headers: res.headers, body: res.body, url: req.url })
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    url: String,
}

impl HttpResponse {
    pub fn status(&self) -> u16 {
        self.status
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
    pub fn bytes(&self) -> &[u8] {
        &self.body
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|e| {
            let mut err = ApiError::upstream("An upstream service returned an unexpected response").code("upstream_invalid_response");
            err.internal = Some(format!("{}: {e}", self.url));
            err
        })
    }
    pub fn error_for_status(self) -> Result<Self> {
        if (200..300).contains(&self.status) {
            return Ok(self);
        }
        let mut err = ApiError::upstream(format!("An upstream service answered {}", self.status)).code("upstream_error");
        err.internal = Some(format!("{} -> {}: {}", self.url, self.status, String::from_utf8_lossy(&self.body[..self.body.len().min(300)])));
        Err(err)
    }
}

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
