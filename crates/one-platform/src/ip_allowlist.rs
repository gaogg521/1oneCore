//! IP allowlist matching (P1-4). Pure logic — no request blocking is wired by
//! default (that middleware is a reserved drop-in, since auto-enabling it could
//! lock out the operator). The config store + this matcher let a deployment
//! enable enforcement without reimplementing CIDR math.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Does `ip` fall within any of the allowlist `entries`? Each entry is a CIDR
/// (`10.0.0.0/8`, `2001:db8::/32`) or a bare address (treated as `/32` / `/128`
/// exact match). Unparseable entries/IPs are skipped (never panic). An empty
/// list matches nothing.
pub fn ip_allowed(entries: &[String], ip: &str) -> bool {
    let ip = ip.trim();
    entries.iter().any(|entry| entry_matches(entry.trim(), ip))
}

fn entry_matches(entry: &str, ip: &str) -> bool {
    match entry.split_once('/') {
        Some((net, prefix)) => match prefix.trim().parse::<u8>() {
            Ok(prefix) => cidr_contains(net.trim(), prefix, ip),
            Err(_) => false,
        },
        // Bare address = exact match.
        None => exact_match(entry, ip),
    }
}

fn exact_match(a: &str, b: &str) -> bool {
    if let (Ok(x), Ok(y)) = (a.parse::<Ipv4Addr>(), b.parse::<Ipv4Addr>()) {
        return x == y;
    }
    matches!((a.parse::<Ipv6Addr>(), b.parse::<Ipv6Addr>()), (Ok(x), Ok(y)) if x == y)
}

fn cidr_contains(net: &str, prefix: u8, ip: &str) -> bool {
    if let (Ok(net), Ok(ip)) = (net.parse::<Ipv4Addr>(), ip.parse::<Ipv4Addr>()) {
        if prefix > 32 {
            return false;
        }
        let mask: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
        return (u32::from(net) & mask) == (u32::from(ip) & mask);
    }
    if let (Ok(net), Ok(ip)) = (net.parse::<Ipv6Addr>(), ip.parse::<Ipv6Addr>()) {
        if prefix > 128 {
            return false;
        }
        let mask: u128 = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
        return (u128::from(net) & mask) == (u128::from(ip) & mask);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_and_exact_matching() {
        let list = vec![
            "10.0.0.0/8".to_owned(),
            "192.168.1.5".to_owned(),
            "2001:db8::/32".to_owned(),
        ];
        assert!(ip_allowed(&list, "10.3.4.5"));
        assert!(!ip_allowed(&list, "11.0.0.1"));
        assert!(ip_allowed(&list, "192.168.1.5"));
        assert!(!ip_allowed(&list, "192.168.1.6"));
        assert!(ip_allowed(&list, "2001:db8:1234::1"));
        assert!(!ip_allowed(&list, "2001:dead::1"));
        // Empty list matches nothing; junk entries are skipped.
        assert!(!ip_allowed(&[], "10.0.0.1"));
        assert!(!ip_allowed(&["not-an-ip".to_owned()], "10.0.0.1"));
    }
}
