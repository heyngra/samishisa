use std::{net::IpAddr, sync::LazyLock};

#[derive(Debug, Clone, PartialEq)]
pub struct Cidr {
    base: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Option<Cidr> {
        let (address, bits) = match text.split_once('/') {
            Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
            None => (text, None),
        };
        let base: IpAddr = address.parse().ok()?;
        let max_bits = if base.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max_bits);
        (bits <= max_bits).then_some(Cidr { base, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, unmap(ip)) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                let mask = if self.bits == 0 { 0 } else { u32::MAX << (32 - self.bits) };
                u32::from(base) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                let mask = if self.bits == 0 { 0 } else { u128::MAX << (128 - self.bits) };
                u128::from(base) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

const PRIVATE_RANGE_TEXT: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
    "::/128",
    "::1/128",
    "fc00::/7",
    "fe80::/10",
    "ff00::/8",
    "64:ff9b::/32",
    "2001:db8::/32",
    "100::/64",
    "2002::/16",
];

static PRIVATE_RANGES: LazyLock<Vec<Cidr>> = LazyLock::new(|| PRIVATE_RANGE_TEXT.iter().filter_map(|range| Cidr::parse(range)).collect());

pub fn is_private_ip(ip: IpAddr) -> bool {
    PRIVATE_RANGES.iter().any(|range| range.contains(ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_range_parses() {
        assert_eq!(PRIVATE_RANGES.len(), PRIVATE_RANGE_TEXT.len());
    }

    #[test]
    fn private_addresses() {
        let private = [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.16",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
        ];
        for s in private {
            assert!(is_private_ip(s.parse().unwrap()), "{s} should be private");
        }
        for s in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(!is_private_ip(s.parse().unwrap()), "{s} should be public");
        }
    }
}
