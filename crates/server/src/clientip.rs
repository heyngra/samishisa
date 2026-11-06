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

    fn is_proxy_hop(&self, hop: IpAddr) -> bool {
        match self {
            TrustProxy::Always => is_private_ip(hop),
            other => other.trusts(hop),
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
                items.iter().map(|item| item.as_str().and_then(Cidr::parse).ok_or_else(|| invalid(format!("trustProxy: {item} is not an IP address or CIDR")))).collect::<Result<
                    Vec<Cidr>,
                    D::Error,
                >>(
                )?,
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
    let header_ip = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<IpAddr>().ok()).map(unmap);
    if peer.is_loopback()
        && let Some(ip) = header_ip("cf-connecting-ip")
    {
        return ip;
    }
    let hops: Vec<&str> = headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).collect();
    if !hops.is_empty() {
        let mut nearest_proxy = None;
        for hop in hops.iter().rev() {
            let Ok(ip) = hop.trim().parse::<IpAddr>().map(unmap) else { break };
            if !trust.is_proxy_hop(ip) {
                return ip;
            }
            nearest_proxy = Some(ip);
        }
        return nearest_proxy.unwrap_or(peer);
    }
    header_ip("x-real-ip").unwrap_or(peer)
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
        assert_eq!(client_ip("10.9.9.9".parse().unwrap(), &xff, &list).to_string(), "198.51.100.7");
        assert_eq!(client_ip(local, &cf, &list), local);
    }

    #[test]
    fn what_the_client_wrote_itself_is_ignored() {
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        let spoofed = headers(&[("x-forwarded-for", "1.2.3.4, 9.9.9.9")]);
        assert_eq!(client_ip(local, &spoofed, &TrustProxy::Private).to_string(), "9.9.9.9");
        assert_eq!(client_ip(local, &spoofed, &TrustProxy::Always).to_string(), "9.9.9.9");
        let both = headers(&[("x-forwarded-for", "9.9.9.9"), ("x-real-ip", "1.2.3.4")]);
        assert_eq!(client_ip(local, &both, &TrustProxy::Private).to_string(), "9.9.9.9");
        assert_eq!(client_ip(local, &headers(&[("x-real-ip", "9.9.9.10")]), &TrustProxy::Private).to_string(), "9.9.9.10");
        let cf = headers(&[("cf-connecting-ip", "203.0.113.9")]);
        assert_eq!(client_ip(lan, &cf, &TrustProxy::Private), lan);
        let garbled = headers(&[("x-forwarded-for", "9.9.9.9, nonsense, 10.0.0.1")]);
        assert_eq!(client_ip(local, &garbled, &TrustProxy::Private).to_string(), "10.0.0.1");
        assert!(serde_json::from_str::<TrustProxy>(r#""sometimes""#).is_err());
    }
}
