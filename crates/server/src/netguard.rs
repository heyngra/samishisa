use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpStream;

use crate::runtime::WitError;

pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            let s = v6.segments();
            v6.is_unspecified()
                || v6.is_loopback()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xff00) == 0xff00
                || (s[0] == 0x64 && s[1] == 0xff9b)
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
                || (s[0] == 0x2002)
        }
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    const NETS: &[([u8; 4], u8)] = &[
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([100, 64, 0, 0], 10),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 2, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([224, 0, 0, 0], 4),
        ([240, 0, 0, 0], 4),
    ];
    let n = u32::from(ip);
    NETS.iter().any(|(base, bits)| {
        let mask = if *bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        n & mask == u32::from(Ipv4Addr::from(*base)) & mask
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Policy {
    None,
    Any,
    Hosts(Vec<String>),
}

impl Policy {
    pub fn from_value(v: Option<&Value>) -> Policy {
        match v {
            Some(Value::String(s)) if s == "*" => Policy::Any,
            Some(Value::Array(a)) => Policy::Hosts(a.iter().filter_map(Value::as_str).map(str::to_ascii_lowercase).collect()),
            _ => Policy::None,
        }
    }

    pub fn allows(&self, host: &str) -> bool {
        let h = host.to_ascii_lowercase();
        match self {
            Policy::None => false,
            Policy::Any => true,
            Policy::Hosts(list) => list.iter().any(|p| match p.strip_prefix("*.") {
                Some(suffix) => h.len() > suffix.len() + 1 && h.ends_with(suffix) && h.as_bytes()[h.len() - suffix.len() - 1] == b'.',
                None => *p == h,
            }),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Policy::None => "none".into(),
            Policy::Any => "any public host".into(),
            Policy::Hosts(h) => h.join(", "),
        }
    }
}

pub struct Egress {
    tls: tokio_rustls::TlsConnector,
    pub allow_private: bool,
    pub max_bytes: usize,
    pub timeout: Duration,
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

const MAX_REDIRECTS: usize = 5;
const UPSTREAM_HINT: &str = "This is a problem with a service this endpoint depends on, not with your request. Retry shortly.";

pub fn api_error(status: u16, code: &str, message: impl Into<String>, hint: Option<&str>, details: Option<Value>) -> WitError {
    WitError {
        status,
        code: Some(code.into()),
        message: message.into(),
        hint: hint.map(Into::into),
        details_json: details.map(|d| d.to_string()),
    }
}

impl Egress {
    pub fn new(allow_private: bool) -> Self {
        let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self { tls: tokio_rustls::TlsConnector::from(Arc::new(cfg)), allow_private, max_bytes: 10 << 20, timeout: Duration::from_secs(10) }
    }

    pub async fn fetch(&self, ext: &str, policy: &Policy, req: OutRequest) -> Result<OutResponse, WitError> {
        let mut host_for_errors = String::new();
        match tokio::time::timeout(self.timeout, self.fetch_inner(ext, policy, req, &mut host_for_errors)).await {
            Ok(r) => r,
            Err(_) => Err(api_error(504, "upstream_timeout", "Upstream service timed out", Some(UPSTREAM_HINT), Some(json!({ "upstream": host_for_errors, "timeoutMs": self.timeout.as_millis() as u64 })))),
        }
    }

    async fn fetch_inner(&self, ext: &str, policy: &Policy, req: OutRequest, host_out: &mut String) -> Result<OutResponse, WitError> {
        let OutRequest { mut method, url, headers, mut body } = req;
        let mut uri: Uri = url.parse().map_err(|e| api_error(400, "bad_upstream_url", format!("Invalid URL {url:?}: {e}"), Some("Use an absolute http:// or https:// URL."), None))?;
        for hop in 0..=MAX_REDIRECTS {
            let tls = match uri.scheme_str() {
                Some("https") => true,
                Some("http") => false,
                other => return Err(api_error(400, "bad_upstream_url", format!("Unsupported URL scheme {:?}", other.unwrap_or("")), Some("Only http: and https: are allowed."), None)),
            };
            let authority = uri.authority().ok_or_else(|| api_error(400, "bad_upstream_url", "The URL has no host", None, None))?.clone();
            if authority.as_str().contains('@') {
                return Err(api_error(400, "bad_upstream_url", "Credentials in URLs are not allowed", Some("Send them in a header instead."), None));
            }
            let host = authority.host().trim_start_matches('[').trim_end_matches(']').to_owned();
            *host_out = host.clone();
            let port = authority.port_u16().unwrap_or(if tls { 443 } else { 80 });

            if !policy.allows(&host) {
                let hint = match policy {
                    Policy::None => format!("The extension declares no network access. Its author adds `.network([\"{host}\"])`, or the operator sets \"network\" for it in api.config.json."),
                    _ => format!("Allowed hosts: {}. Add \"{host}\" to the extension's network list (or the operator's override in api.config.json).", policy.describe()),
                };
                return Err(api_error(403, "network_not_allowed", format!("The \"{ext}\" extension is not allowed to contact {host}"), Some(&hint), None));
            }

            let addrs = self.resolve(&host, port).await?;
            let stream = connect_any(&addrs).await.map_err(|e| unreachable(&host, &e))?;

            let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_owned();
            let mut b = Request::builder().method(method.as_str()).uri(path).header("host", authority.as_str());
            let mut has_ua = false;
            for (k, v) in &headers {
                let lk = k.to_ascii_lowercase();
                if matches!(lk.as_str(), "host" | "connection" | "content-length" | "transfer-encoding" | "upgrade" | "te" | "proxy-authorization") {
                    continue;
                }
                has_ua |= lk == "user-agent";
                b = b.header(k.as_str(), v.as_str());
            }
            if !has_ua {
                b = b.header("user-agent", format!("samishisa/{} ({ext})", env!("CARGO_PKG_VERSION")));
            }
            let request = b.body(Full::new(Bytes::from(body.clone().unwrap_or_default()))).map_err(|e| api_error(400, "bad_upstream_request", format!("Invalid request: {e}"), None, None))?;

            let res = if tls {
                let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|_| api_error(400, "bad_upstream_url", format!("Invalid TLS host name {host:?}"), None, None))?;
                let tls_stream = self.tls.connect(name, stream).await.map_err(|e| unreachable(&host, &e))?;
                send(tls_stream, request).await
            } else {
                send(stream, request).await
            }
            .map_err(|e| unreachable(&host, &e))?;

            let status = res.status().as_u16();
            if (300..400).contains(&status) {
                if let Some(loc) = res.headers().get("location").and_then(|l| l.to_str().ok()) {
                    if hop == MAX_REDIRECTS {
                        return Err(api_error(502, "too_many_redirects", "Upstream redirected too many times", Some("Check the upstream URL."), Some(json!({ "upstream": host }))));
                    }
                    uri = join_location(&uri, loc).ok_or_else(|| api_error(502, "bad_redirect", "Upstream sent an invalid redirect", None, Some(json!({ "location": loc }))))?;
                    if status == 303 || ((status == 301 || status == 302) && method == "POST") {
                        method = "GET".into();
                        body = None;
                    }
                    continue;
                }
            }
            if let Some(len) = res.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok()) {
                if len > self.max_bytes {
                    return Err(too_large(&host, self.max_bytes));
                }
            }
            let headers: Vec<(String, String)> = res.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect();
            let collected = Limited::new(res.into_body(), self.max_bytes).collect().await.map_err(|e| {
                if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() { too_large(&host, self.max_bytes) } else { unreachable(&host, &*e) }
            })?;
            return Ok(OutResponse { status, headers, body: collected.to_bytes().to_vec() });
        }
        unreachable!("the loop returns on its last hop")
    }

    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, WitError> {
        let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| api_error(502, "upstream_unreachable", "Upstream host could not be resolved", Some(UPSTREAM_HINT), Some(json!({ "upstream": host, "reason": e.to_string() }))))?
                .collect(),
        };
        if !self.allow_private {
            if let Some(bad) = addrs.iter().find(|a| is_private_ip(a.ip())) {
                return Err(api_error(
                    403,
                    "network_private_blocked",
                    format!("Requests to internal addresses are blocked ({host} is {})", bad.ip()),
                    Some("Extensions may only reach public internet hosts. For local development the operator can set \"network\": { \"allowPrivate\": true }."),
                    None,
                ));
            }
        }
        if addrs.is_empty() {
            return Err(api_error(502, "upstream_unreachable", "Upstream host has no addresses", Some(UPSTREAM_HINT), Some(json!({ "upstream": host }))));
        }
        Ok(addrs)
    }
}

async fn connect_any(addrs: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last = None;
    for a in addrs {
        match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = Some(e),
            Err(_) => last = Some(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("connecting to {a} timed out"))),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no addresses")))
}

async fn send<S>(stream: S, req: Request<Full<Bytes>>) -> Result<hyper::Response<hyper::body::Incoming>, Box<dyn std::error::Error + Send + Sync>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender.send_request(req).await?)
}

fn unreachable(host: &str, e: &dyn std::fmt::Display) -> WitError {
    api_error(502, "upstream_unreachable", "Upstream service unreachable", Some(UPSTREAM_HINT), Some(json!({ "upstream": host, "reason": e.to_string() })))
}

fn too_large(host: &str, max: usize) -> WitError {
    api_error(502, "upstream_too_large", "Upstream response is too large", Some(&format!("The limit is {max} bytes.")), Some(json!({ "upstream": host })))
}

fn join_location(base: &Uri, loc: &str) -> Option<Uri> {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.parse().ok();
    }
    let scheme = base.scheme_str()?;
    let authority = base.authority()?.as_str();
    if let Some(rest) = loc.strip_prefix("//") {
        return format!("{scheme}://{rest}").parse().ok();
    }
    let path = if loc.starts_with('/') {
        loc.to_owned()
    } else {
        let dir = base.path().rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        format!("{dir}/{loc}")
    };
    format!("{scheme}://{authority}{path}").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_addresses() {
        for s in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.16", "169.254.169.254", "100.64.0.1", "0.0.0.0", "224.0.0.1", "::1", "::", "fd00::1", "fe80::1", "::ffff:127.0.0.1", "64:ff9b::7f00:1", "2002:7f00:1::1"] {
            assert!(is_private_ip(ip(s)), "{s} should be private");
        }
        for s in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(!is_private_ip(ip(s)), "{s} should be public");
        }
    }

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
    fn redirects() {
        let base: Uri = "https://a.example/x/y?q=1".parse().unwrap();
        assert_eq!(join_location(&base, "/z").unwrap().to_string(), "https://a.example/z");
        assert_eq!(join_location(&base, "z").unwrap().to_string(), "https://a.example/x/z");
        assert_eq!(join_location(&base, "//b.example/p").unwrap().to_string(), "https://b.example/p");
        assert_eq!(join_location(&base, "http://c.example/").unwrap().to_string(), "http://c.example/");
    }
}
