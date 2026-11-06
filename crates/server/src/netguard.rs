use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, Uri, body::Incoming};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use tokio::{net::TcpStream, time::Instant};

use crate::ipnet::is_private_ip;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Policy {
    #[default]
    None,
    Any,
    Hosts(Vec<String>),
}

impl Policy {
    pub fn allows(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self {
            Policy::None => false,
            Policy::Any => true,
            Policy::Hosts(patterns) => patterns.iter().any(|pattern| match pattern.strip_prefix("*.") {
                Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(suffix) && host.as_bytes()[host.len() - suffix.len() - 1] == b'.',
                None => *pattern == host,
            }),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Policy::None => "none".into(),
            Policy::Any => "any public host".into(),
            Policy::Hosts(hosts) => hosts.join(", "),
        }
    }
}

impl<'de> Deserialize<'de> for Policy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Option::<Value>::deserialize(deserializer)? {
            None => Ok(Policy::None),
            Some(Value::String(s)) if s == "*" => Ok(Policy::Any),
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                Ok(Policy::Hosts(items.iter().filter_map(Value::as_str).map(str::to_ascii_lowercase).collect()))
            }
            Some(other) => Err(serde::de::Error::custom(format!("\"network\" must be a list of hosts or \"*\" (got {other})"))),
        }
    }
}

pub struct OutRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

pub struct OutResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct FetchError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
    pub details: Option<Value>,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(d) = &self.details {
            write!(f, " {d}")?;
        }
        Ok(())
    }
}

const UPSTREAM_HINT: &str = "This is a problem with a service this endpoint depends on, not with your request. Retry shortly.";

fn bad_url(message: impl Into<String>, hint: Option<&str>) -> FetchError {
    FetchError { status: 400, code: "bad_upstream_url", message: message.into(), hint: hint.map(Into::into), details: None }
}

fn upstream_unreachable(host: &str, reason: &dyn fmt::Display) -> FetchError {
    FetchError {
        status: 502,
        code: "upstream_unreachable",
        message: "Upstream service unreachable".into(),
        hint: Some(UPSTREAM_HINT.into()),
        details: Some(json!({ "upstream": host, "reason": reason.to_string() })),
    }
}

fn upstream_timed_out(host: &str, timeout: Duration) -> FetchError {
    FetchError {
        status: 504,
        code: "upstream_timeout",
        message: "Upstream service timed out".into(),
        hint: Some(UPSTREAM_HINT.into()),
        details: Some(json!({ "upstream": host, "timeoutMs": timeout.as_millis() as u64 })),
    }
}

fn upstream_too_large(host: &str, max_bytes: usize) -> FetchError {
    FetchError {
        status: 502,
        code: "upstream_too_large",
        message: "Upstream response is too large".into(),
        hint: Some(format!("The limit is {max_bytes} bytes.")),
        details: Some(json!({ "upstream": host })),
    }
}

const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Egress {
    tls: tokio_rustls::TlsConnector,
    allow_private: bool,
    max_bytes: usize,
    timeout: Duration,
}

struct Target {
    tls: bool,
    host: String,
    port: u16,
    authority: String,
}

impl Egress {
    pub fn for_extensions(allow_private: bool) -> Self {
        Self::new(allow_private, 10 << 20, Duration::from_secs(10))
    }

    pub fn for_downloads() -> Self {
        Self::new(true, 64 << 20, Duration::from_secs(120))
    }

    fn new(allow_private: bool, max_bytes: usize, timeout: Duration) -> Self {
        let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self { tls: tokio_rustls::TlsConnector::from(Arc::new(cfg)), allow_private, max_bytes, timeout }
    }

    pub async fn fetch(&self, ext: &str, policy: &Policy, request: OutRequest) -> Result<OutResponse, FetchError> {
        let deadline = Instant::now() + self.timeout;
        let OutRequest { mut method, url, headers, mut body } = request;
        let mut uri: Uri = url.parse().map_err(|e| bad_url(format!("Invalid URL {url:?}: {e}"), Some("Use an absolute http:// or https:// URL.")))?;
        let mut redirects = 0;
        loop {
            let target = Target::of(&uri)?;
            check_allowed(ext, policy, &target.host)?;
            let hop = build_request(&uri, &target, &method, &headers, body.clone(), ext)?;
            let response = tokio::time::timeout_at(deadline, self.send(&target, hop))
                .await
                .map_err(|_| upstream_timed_out(&target.host, self.timeout))??;

            if let Some(next) = redirect_location(&uri, &response)? {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(FetchError {
                        status: 502,
                        code: "too_many_redirects",
                        message: "Upstream redirected too many times".into(),
                        hint: Some("Check the upstream URL.".into()),
                        details: Some(json!({ "upstream": target.host })),
                    });
                }
                let status = response.status().as_u16();
                if status == 303 || ((status == 301 || status == 302) && method == "POST") {
                    method = "GET".into();
                    body = None;
                }
                uri = next;
                continue;
            }
            return tokio::time::timeout_at(deadline, self.read_body(&target.host, response))
                .await
                .map_err(|_| upstream_timed_out(&target.host, self.timeout))?;
        }
    }

    async fn send(&self, target: &Target, request: Request<Full<Bytes>>) -> Result<hyper::Response<Incoming>, FetchError> {
        let addresses = self.resolve_public(&target.host, target.port).await?;
        let stream = connect_any(&addresses).await.map_err(|e| upstream_unreachable(&target.host, &e))?;
        let sent = if target.tls {
            let name = rustls::pki_types::ServerName::try_from(target.host.clone()).map_err(|_| bad_url(format!("Invalid TLS host name {:?}", target.host), None))?;
            let tls_stream = self.tls.connect(name, stream).await.map_err(|e| upstream_unreachable(&target.host, &e))?;
            send_http1(tls_stream, request).await
        } else {
            send_http1(stream, request).await
        };
        sent.map_err(|e| upstream_unreachable(&target.host, &e))
    }

    async fn resolve_public(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, FetchError> {
        let addresses: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host, port)).await.map_err(|e| upstream_unreachable(host, &e))?.collect(),
        };
        if !self.allow_private {
            if let Some(private) = addresses.iter().find(|a| is_private_ip(a.ip())) {
                return Err(FetchError {
                    status: 403,
                    code: "network_private_blocked",
                    message: format!("Requests to internal addresses are blocked ({host} is {})", private.ip()),
                    hint: Some("Extensions may only reach public internet hosts. For local development the operator can set \"network\": { \"allowPrivate\": true }.".into()),
                    details: None,
                });
            }
        }
        if addresses.is_empty() {
            return Err(upstream_unreachable(host, &"the host has no addresses"));
        }
        Ok(addresses)
    }

    async fn read_body(&self, host: &str, response: hyper::Response<Incoming>) -> Result<OutResponse, FetchError> {
        let declared_length = response.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok());
        if declared_length.is_some_and(|len| len > self.max_bytes) {
            return Err(upstream_too_large(host, self.max_bytes));
        }
        let status = response.status().as_u16();
        let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect();
        let body = Limited::new(response.into_body(), self.max_bytes).collect().await.map_err(|e| {
            if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() { upstream_too_large(host, self.max_bytes) } else { upstream_unreachable(host, &*e) }
        })?;
        Ok(OutResponse { status, headers, body: body.to_bytes().to_vec() })
    }
}

impl Target {
    fn of(uri: &Uri) -> Result<Target, FetchError> {
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            other => return Err(bad_url(format!("Unsupported URL scheme {:?}", other.unwrap_or("")), Some("Only http: and https: are allowed."))),
        };
        let authority = uri.authority().ok_or_else(|| bad_url("The URL has no host", None))?;
        if authority.as_str().contains('@') {
            return Err(bad_url("Credentials in URLs are not allowed", Some("Send them in a header instead.")));
        }
        Ok(Target {
            tls,
            host: authority.host().trim_start_matches('[').trim_end_matches(']').to_owned(),
            port: authority.port_u16().unwrap_or(if tls { 443 } else { 80 }),
            authority: authority.as_str().to_owned(),
        })
    }
}

fn check_allowed(ext: &str, policy: &Policy, host: &str) -> Result<(), FetchError> {
    if policy.allows(host) {
        return Ok(());
    }
    let hint = match policy {
        Policy::None => format!("The extension declares no network access. Its author adds `.network([\"{host}\"])`, or the operator sets \"network\" for it in api.config.json."),
        _ => format!("Allowed hosts: {}. Add \"{host}\" to the extension's network list (or the operator's override in api.config.json).", policy.describe()),
    };
    Err(FetchError { status: 403, code: "network_not_allowed", message: format!("The \"{ext}\" extension is not allowed to contact {host}"), hint: Some(hint), details: None })
}

const HOP_HEADERS: &[&str] = &["host", "connection", "content-length", "transfer-encoding", "upgrade", "te", "proxy-authorization"];

fn build_request(uri: &Uri, target: &Target, method: &str, headers: &[(String, String)], body: Option<Vec<u8>>, ext: &str) -> Result<Request<Full<Bytes>>, FetchError> {
    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let mut builder = Request::builder().method(method).uri(path).header("host", &target.authority);
    let mut has_user_agent = false;
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if HOP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        has_user_agent |= lower == "user-agent";
        builder = builder.header(name.as_str(), value.as_str());
    }
    if !has_user_agent {
        builder = builder.header("user-agent", format!("samishisa/{} ({ext})", env!("CARGO_PKG_VERSION")));
    }
    builder.body(Full::new(Bytes::from(body.unwrap_or_default()))).map_err(|e| FetchError {
        status: 400,
        code: "bad_upstream_request",
        message: format!("Invalid request: {e}"),
        hint: None,
        details: None,
    })
}

fn redirect_location(current: &Uri, response: &hyper::Response<Incoming>) -> Result<Option<Uri>, FetchError> {
    if !response.status().is_redirection() {
        return Ok(None);
    }
    let Some(location) = response.headers().get("location").and_then(|l| l.to_str().ok()) else { return Ok(None) };
    join_location(current, location).map(Some).ok_or_else(|| FetchError {
        status: 502,
        code: "bad_redirect",
        message: "Upstream sent an invalid redirect".into(),
        hint: None,
        details: Some(json!({ "location": location })),
    })
}

fn join_location(base: &Uri, location: &str) -> Option<Uri> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.parse().ok();
    }
    let scheme = base.scheme_str()?;
    let authority = base.authority()?.as_str();
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{scheme}://{rest}").parse().ok();
    }
    let path = if location.starts_with('/') {
        location.to_owned()
    } else {
        let dir = base.path().rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        format!("{dir}/{location}")
    };
    format!("{scheme}://{authority}{path}").parse().ok()
}

async fn connect_any(addresses: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last_error = None;
    for address in addresses {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Ok(Err(e)) => last_error = Some(e),
            Err(_) => last_error = Some(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("connecting to {address} timed out"))),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("no addresses")))
}

async fn send_http1<S>(stream: S, request: Request<Full<Bytes>>) -> Result<hyper::Response<Incoming>, hyper::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender.send_request(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        let p = Policy::Hosts(vec!["api.example.com".into(), "*.cdn.example.com".into()]);
        assert!(p.allows("api.example.com"));
        assert!(p.allows("API.EXAMPLE.COM"));
        assert!(p.allows("a.cdn.example.com"));
        assert!(!p.allows("cdn.example.com"));
        assert!(!p.allows("evilcdn.example.com"));
        assert!(!p.allows("example.com"));
        assert!(Policy::Any.allows("anything.org"));
        assert!(!Policy::None.allows("api.example.com"));
    }

    #[test]
    fn policy_parse() {
        let parse = |s: &str| serde_json::from_str::<Policy>(s);
        assert_eq!(parse(r#""*""#).unwrap(), Policy::Any);
        assert_eq!(parse(r#"["API.example.com"]"#).unwrap(), Policy::Hosts(vec!["api.example.com".into()]));
        assert_eq!(parse("null").unwrap(), Policy::None);
        assert!(parse(r#""example.com""#).is_err());
    }

    #[test]
    fn redirects() {
        let base: Uri = "https://a.example/x/y?q=1".parse().unwrap();
        assert_eq!(join_location(&base, "/z").unwrap().to_string(), "https://a.example/z");
        assert_eq!(join_location(&base, "z").unwrap().to_string(), "https://a.example/x/z");
        assert_eq!(join_location(&base, "//b.example/p").unwrap().to_string(), "https://b.example/p");
        assert_eq!(join_location(&base, "http://c.example/").unwrap().to_string(), "http://c.example/");
    }
}
