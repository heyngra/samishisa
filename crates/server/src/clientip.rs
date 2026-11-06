use std::net::IpAddr;

use hyper::HeaderMap;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::ipnet::{Cidr, is_private_ip, unmap};

#[derive(Debug, Clone, PartialEq)]
pub enum TrustProxy {
    Never,
    Always,
    Private,
    List(Vec<Cidr>),
}

impl TrustProxy {
    fn trusts(&self, peer: IpAddr) -> bool {
        match self {
            TrustProxy::Never => false,
            TrustProxy::Always => true,
            TrustProxy::Private => is_private_ip(peer),
            TrustProxy::List(ranges) => ranges.iter().any(|r| r.contains(peer)),
        }
    }
}

impl<'de> Deserialize<'de> for TrustProxy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let invalid = |what: String| -> D::Error { serde::de::Error::custom(what) };
        Ok(match Value::deserialize(deserializer)? {
            Value::Bool(true) => TrustProxy::Always,
            Value::Bool(false) => TrustProxy::Never,
            Value::String(s) if s == "private" => TrustProxy::Private,
            Value::Array(items) => TrustProxy::List(
                items
                    .iter()
                    .map(|item| item.as_str().and_then(Cidr::parse).ok_or_else(|| invalid(format!("trustProxy: {item} is not an IP address or CIDR"))))
                    .collect::<Result<Vec<Cidr>, D::Error>>()?,
            ),
            other => return Err(invalid(format!("trustProxy must be true, false, \"private\" or a list of CIDRs (got {other})"))),
        })
    }
}

pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trust: &TrustProxy) -> IpAddr {
    let peer = unmap(peer);
    if !trust.trusts(peer) {
        return peer;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let forwarded = header("cf-connecting-ip")
        .or_else(|| header("x-real-ip"))
        .or_else(|| header("x-forwarded-for").and_then(|v| v.split(',').next()))
        .and_then(|s| s.trim().parse::<IpAddr>().ok());
    forwarded.map(unmap).unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, v.parse().unwrap());
        }
        m
    }

    #[test]
    fn forwarded_only_from_trusted_peers() {
        let cf = headers(&[("cf-connecting-ip", "203.0.113.9")]);
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(client_ip(public, &cf, &TrustProxy::Private), public, "a public peer can't spoof");
        assert_eq!(client_ip(local, &cf, &TrustProxy::Private).to_string(), "203.0.113.9");
        assert_eq!(client_ip(local, &cf, &TrustProxy::Never), local);
        let xff = headers(&[("x-forwarded-for", "198.51.100.7, 10.0.0.1")]);
        assert_eq!(client_ip(local, &xff, &TrustProxy::Private).to_string(), "198.51.100.7");
        let list: TrustProxy = serde_json::from_str(r#"["10.0.0.0/8"]"#).unwrap();
        assert_eq!(client_ip("10.9.9.9".parse().unwrap(), &cf, &list).to_string(), "203.0.113.9");
        assert_eq!(client_ip(local, &cf, &list), local);
        assert!(serde_json::from_str::<TrustProxy>(r#""sometimes""#).is_err());
    }
}
