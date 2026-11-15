//! SSRF guard for browser tool URLs — port of Android `UrlSafety.checkAnyHttpUrl`
//! (scheme + host + DNS-resolution check) plus `is_global_ip` for reuse by
//! site_scrape seed fetching.
//!
//! Every resolved address must be global-scope; any private/loopback/
//! link-local/unspecified/multicast answer rejects the whole URL.
//! Known tradeoff (same as Android): the browser does its own DNS after this
//! check, so fast-rebinding TOCTOU remains.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

/// Parse + validate scheme/host/userinfo, then DNS-resolve and require every
/// answer to be a global IP. Sync (blocking DNS) — call `check_async` from
/// async contexts.
pub fn check(url: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|_| "unparseable URL".to_string())?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("scheme '{scheme}' not allowed"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("userinfo not allowed".to_string());
    }
    let host = parsed.host().ok_or("no host")?.to_owned();
    // socket_addrs resolves domains via getaddrinfo and returns literal IPs
    // directly; http/https always have a known default port so the fallback
    // closure never fires.
    let addrs = parsed
        .socket_addrs(|| None)
        .map_err(|_| format!("host {host} does not resolve"))?;
    for addr in &addrs {
        if !is_global_ip(addr.ip()) {
            return Err(format!(
                "host {host} resolves to a private/loopback address {}",
                addr.ip()
            ));
        }
    }
    Ok(())
}

/// Async wrapper: blocking DNS resolution runs on `spawn_blocking`.
pub async fn check_async(url: &str) -> Result<(), String> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || check(&url))
        .await
        .map_err(|e| format!("policy check task failed: {e}"))?
}

/// True iff `ip` is a global-scope unicast address (routable on the public
/// internet). IPv4-mapped/compatible IPv6 counts as its IPv4.
pub fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(
        ip.is_private() // 10/8, 172.16/12, 192.168/16
        || ip.is_loopback() // 127/8
        || ip.is_link_local() // 169.254/16
        || ip.is_unspecified() // 0.0.0.0
        || ip.is_broadcast() // 255.255.255.255
        || ip.is_multicast() // 224/4
        || in_v4(&o, [192, 0, 2, 0], 24) // TEST-NET-1
        || in_v4(&o, [198, 51, 100, 0], 24) // TEST-NET-2
        || in_v4(&o, [203, 0, 113, 0], 24) // TEST-NET-3
        || in_v4(&o, [192, 0, 0, 0], 24) // IETF protocol assignments
        || in_v4(&o, [192, 31, 196, 0], 24) // AS112-v4
        || in_v4(&o, [192, 52, 193, 0], 24) // AMT
        || in_v4(&o, [192, 88, 99, 0], 24) // 6to4 relay anycast
        || in_v4(&o, [192, 175, 48, 0], 24) // AS112
        || in_v4(&o, [198, 18, 0, 0], 15) // benchmarking
        || in_v4(&o, [240, 0, 0, 0], 4) // reserved (incl. 255/8 tail)
        || in_v4(&o, [100, 64, 0, 0], 10) // CGNAT
    )
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d, nonzero
    // tail) defer to the embedded IPv4 — Android's privateV4Bytes rule.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_global_v4(v4);
    }
    let o = ip.octets();
    let v4_prefix = o[..10].iter().all(|&b| b == 0);
    if v4_prefix && o[10] == 0xff && o[11] == 0xff {
        // non-canonical mapped forms to_ipv4_mapped misses
        return is_global_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // IPv4-compatible (::a.b.c.d) defers to the embedded IPv4 — but ::1 is
    // loopback, not "IPv4 0.0.0.1"; check the v6 specials first.
    if ip.is_loopback() || ip.is_unspecified() {
        return false;
    }
    if v4_prefix && o[10] == 0 && o[11] == 0 && o[12..] != [0u8; 4][..] {
        return is_global_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    !(
        ip.is_unspecified() // ::
        || ip.is_loopback() // ::1
        || ip.is_multicast() // ff00::/8
        || ip.is_unique_local() // fc00::/7
        || ip.is_unicast_link_local() // fe80::/10
        || in_v6(&o, &[0x20, 0x01, 0x0d, 0xb8], 32) // documentation 2001:db8::/32
        || in_v6(&o, &[0x01, 0x00, 0x00, 0x00], 64) // discard-only 100::/64
        || in_v6(&o, &[0x20, 0x01, 0x00, 0x00], 23) // IETF protocol assignments 2001::/23
        || in_v6(&o, &[0x20, 0x01, 0x00, 0x20], 28) // ORCHIDv2 2001:20::/28
        || in_v6(&o, &[0x20, 0x01, 0x00, 0x02], 48) // benchmarking 2001:2::/48
        || in_v6(&o, &[0x3f, 0xff], 20) // documentation 3fff::/20
        || in_v6(&o, &[0x5f, 0x00], 16) // segment routing SIDs 5f00::/16
        || in_v6(&o, &[0x64, 0xff, 0x9b, 0x00, 0x01], 48) // local-use NAT64 64:ff9b:1::/48
    )
}

/// True iff `o`'s first `bits` bits equal `base`'s (base shorter than `o`).
fn in_prefix(o: &[u8], base: &[u8], bits: usize) -> bool {
    let full = bits / 8;
    let rem = bits % 8;
    if o.len() < full || base.len() < full || o[..full] != base[..full] {
        return false;
    }
    if rem == 0 {
        return true;
    }
    if o.len() <= full || base.len() <= full {
        return false;
    }
    let mask = 0xffu8 << (8 - rem);
    o[full] & mask == base[full] & mask
}

/// IPv4 octets vs a dotted-quad base.
fn in_v4(o: &[u8; 4], base: [u8; 4], prefix_bits: u8) -> bool {
    in_prefix(o, &base, prefix_bits as usize)
}

/// IPv6 octets vs a byte-prefix base.
fn in_v6(o: &[u8; 16], base: &[u8], prefix_bits: u8) -> bool {
    in_prefix(o, base, prefix_bits as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_literal_private_loopback_linklocal() {
        for u in [
            "http://127.0.0.1/",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.1.1/",
            "http://100.64.0.1/",         // CGNAT
            "http://255.255.255.255/",
            "http://224.0.0.1/",          // multicast
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/", // v4-mapped loopback
            "http://[::ffff:10.0.0.1]/",  // v4-mapped private
            "http://[fe80::1]/",
            "http://[fd00::1]/",          // unique-local
            "http://[ff02::1]/",          // multicast
            "http://[::]/",               // unspecified
        ] {
            assert!(check(u).is_err(), "should reject {u}");
        }
    }

    #[test]
    fn accepts_public_literal_ip() {
        // Global unicast literals — no DNS needed.
        assert!(check("http://8.8.8.8/").is_ok());
        assert!(check("https://1.1.1.1/path?q=1").is_ok());
        assert!(check("http://[2606:4700:4700::1111]/").is_ok());
    }

    #[test]
    fn rejects_bad_scheme_userinfo_nohost() {
        for u in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "example.com",               // no scheme
            "http://user@example.com/",  // userinfo
            "http://user:pass@example.com/",
            "http:///path",              // no host
            "not a url",
        ] {
            assert!(check(u).is_err(), "should reject {u}");
        }
    }

    #[test]
    fn rejects_unresolvable_and_localhost() {
        assert!(check("http://nonexistent.invalid/").is_err());
        // localhost resolves to loopback via /etc/hosts — must be rejected.
        assert!(check("http://localhost/").is_err());
    }

    #[test]
    fn is_global_ip_unit() {
        assert!(is_global_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_global_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_global_ip("::1".parse().unwrap()));
        assert!(is_global_ip("2606:4700:4700::1111".parse().unwrap()));
        // v4-mapped v6 defers to v4
        assert!(!is_global_ip("::ffff:192.168.0.1".parse().unwrap()));
        assert!(is_global_ip("::ffff:8.8.8.8".parse().unwrap()));
    }

    #[tokio::test]
    async fn check_async_mirrors_reject() {
        assert!(check_async("http://127.0.0.1/").await.is_err());
        assert!(check_async("http://8.8.8.8/").await.is_ok());
    }
}
