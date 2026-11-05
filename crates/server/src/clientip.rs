use std::net::IpAddr;

use hyper::HeaderMap;
use serde_json::Value;

use crate::netguard::is_private_ip;

#[derive(Debug, Clone, PartialEq)]
pub enum TrustProxy {
    Never,
    Always,
    Private,
    List(Vec<Cidr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cidr {
    base: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Cidr> {
        let (addr, bits) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
            None => (s, None),
        };
        let base: IpAddr = addr.parse().ok()?;
        let max = if base.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        (bits <= max).then_some(Cidr { base, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, normalize(ip)) {
            (IpAddr::V4(b), IpAddr::V4(i)) => {
                let mask = if self.bits == 0 { 0 } else { u32::MAX << (32 - self.bits) };
                u32::from(b) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(b), IpAddr::V6(i)) => {
                let mask = if self.bits == 0 { 0 } else { u128::MAX << (128 - self.bits) };
                u128::from(b) & mask == u128::from(i) & mask
            }
            _ => false,
        }
    }
}

impl TrustProxy {
    pub fn from_value(v: &Value) -> anyhow::Result<TrustProxy> {
        Ok(match v {
            Value::Bool(true) => TrustProxy::Always,
            Value::Bool(false) => TrustProxy::Never,
            Value::String(s) if s == "private" => TrustProxy::Private,
            Value::Array(items) => TrustProxy::List(
                items
                    .iter()
                    .map(|i| i.as_str().and_then(Cidr::parse).ok_or_else(|| anyhow::anyhow!("trustProxy: {i} is not an IP address or CIDR")))
                    .collect::<anyhow::Result<_>>()?,
            ),
            other => anyhow::bail!("trustProxy must be true, false, \"private\" or a list of CIDRs (got {other})"),
        })
    }

    fn trusts(&self, peer: IpAddr) -> bool {
        match self {
            TrustProxy::Never => false,
            TrustProxy::Always => true,
            TrustProxy::Private => is_private_ip(peer),
            TrustProxy::List(l) => l.iter().any(|c| c.contains(peer)),
        }
    }
}

fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trust: &TrustProxy) -> IpAddr {
    let peer = normalize(peer);
    if trust.trusts(peer) {
        let fwd = headers
            .get("cf-connecting-ip")
            .or_else(|| headers.get("x-real-ip"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(|s| s.trim().to_owned()));
        if let Some(ip) = fwd.and_then(|s| s.parse::<IpAddr>().ok()) {
            return normalize(ip);
        }
    }
    peer
}

pub fn bucket(ip: IpAddr) -> String {
    match normalize(ip) {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, v.parse().unwrap());
        }
        m
    }

    #[test]
    fn forwarded_only_from_trusted_peers() {
        let hdrs = h(&[("cf-connecting-ip", "203.0.113.9")]);
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(client_ip(public, &hdrs, &TrustProxy::Private), public, "a public peer can't spoof");
        assert_eq!(client_ip(local, &hdrs, &TrustProxy::Private).to_string(), "203.0.113.9");
        assert_eq!(client_ip(local, &hdrs, &TrustProxy::Never), local);
        let xff = h(&[("x-forwarded-for", "198.51.100.7, 10.0.0.1")]);
        assert_eq!(client_ip(local, &xff, &TrustProxy::Private).to_string(), "198.51.100.7");
        let list = TrustProxy::from_value(&serde_json::json!(["10.0.0.0/8"])).unwrap();
        assert_eq!(client_ip("10.9.9.9".parse().unwrap(), &hdrs, &list).to_string(), "203.0.113.9");
        assert_eq!(client_ip(local, &hdrs, &list), local);
    }

    #[test]
    fn buckets() {
        assert_eq!(bucket("2001:db8:1:2:3:4:5:6".parse().unwrap()), "2001:db8:1:2::/64");
        assert_eq!(bucket("::ffff:1.2.3.4".parse().unwrap()), "1.2.3.4");
    }
}
