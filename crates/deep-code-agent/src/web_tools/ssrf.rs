//! SSRF guard for the network tools: scheme allow-list, private/internal
//! address rejection (RFC1918 / loopback / link-local / CGNAT and their IPv6
//! forms, incl. NAT64 / 6to4 / IPv4-compat), and DNS-rebinding-safe fetching
//! that validates and pins each redirect hop to the address it checked.
//!
//! Isolated from tool orchestration and HTML handling so this — the most
//! security-sensitive code on the crate's network path — is independently
//! reviewable, with its tests alongside it.

use super::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use reqwest::blocking::Client;
use reqwest::redirect::Policy;

/// GET with manual redirect handling. Each hop's host is resolved and
/// validated exactly once, and the connection is pinned to the validated
/// address, closing the check-time/connect-time DNS rebinding gap.
pub(super) fn pinned_get(
    start: Url,
    allow_private: bool,
    lang: Lang,
) -> Result<reqwest::blocking::Response, String> {
    let mut url = start;
    for _ in 0..=MAX_REDIRECTS {
        let pin = check_url(&url, allow_private, lang)?;
        let mut builder = Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(Policy::none());
        if let Some(addr) = pin
            && let Some(host) = url.host_str()
        {
            builder = builder.resolve(host, addr);
        }
        let client = builder.build().map_err(|error| {
            tr_with(
                lang,
                TextId::WebClientInitError,
                &[("error", &error.to_string())],
            )
        })?;
        let response = client.get(url.clone()).send().map_err(|error| {
            tr_with(
                lang,
                TextId::WebRequestError,
                &[("error", &error.to_string())],
            )
        })?;
        if response.status().is_redirection()
            && let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
        {
            url = url.join(location).map_err(|error| {
                tr_with(
                    lang,
                    TextId::WebRedirectInvalid,
                    &[("error", &error.to_string())],
                )
            })?;
            continue;
        }
        return Ok(response);
    }
    Err(tr(lang, TextId::WebRedirectLimit).to_string())
}

/// Reject non-http(s) schemes and any host that resolves to a non-public
/// address (SSRF guard). For domain hosts, returns the validated address the
/// caller must pin its connection to; IP-literal hosts involve no DNS and
/// need no pin. `allow_private` is a test-only escape hatch.
fn check_url(url: &Url, allow_private: bool, lang: Lang) -> Result<Option<SocketAddr>, String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(tr_with(
                lang,
                TextId::WebSchemeNotAllowed,
                &[("scheme", other)],
            ));
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| tr(lang, TextId::WebUrlNoHost).to_string())?;
    if allow_private {
        return Ok(None);
    }
    // IP-literal host: no resolution happens, so validate the literal directly.
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        if !ip_is_public(ip) {
            return Err(tr_with(
                lang,
                TextId::WebPrivateHostBlocked,
                &[("host", host)],
            ));
        }
        return Ok(None);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| {
            tr_with(
                lang,
                TextId::WebHostResolveError,
                &[("host", host), ("error", &error.to_string())],
            )
        })?
        .collect();
    if addrs.is_empty() {
        return Err(tr_with(lang, TextId::WebHostNoAddrs, &[("host", host)]));
    }
    for addr in &addrs {
        if !ip_is_public(addr.ip()) {
            return Err(tr_with(
                lang,
                TextId::WebPrivateResolvedBlocked,
                &[("host", host), ("addr", &addr.ip().to_string())],
            ));
        }
    }
    Ok(Some(addrs[0]))
}

fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => ipv6_is_public(v6),
    }
}

fn ipv4_is_public(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let cgnat = octets[0] == 100 && (64..=127).contains(&octets[1]);
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || cgnat)
}

fn ipv6_is_public(ip: Ipv6Addr) -> bool {
    // Any IPv6 literal that embeds an IPv4 address is only as public as that
    // IPv4. Folding just the `::ffff:0:0/96` mapped form (as before) left every
    // other embedding judged public: `64:ff9b::a9fe:a9fe` (NAT64 well-known +
    // 169.254.169.254) reached the metadata endpoint on NAT64/CLAT hosts that
    // route it, and 6to4 / IPv4-compatible spellings hid loopback the same way.
    if let Some(v4) = embedded_ipv4(ip) {
        return ipv4_is_public(v4);
    }
    let s = ip.segments();
    let unique_local = (s[0] & 0xfe00) == 0xfc00; // fc00::/7
    let link_local = (s[0] & 0xffc0) == 0xfe80; // fe80::/10
    let documentation = s[0] == 0x2001 && s[1] == 0x0db8; // 2001:db8::/32
    // Teredo 2001:0000::/32 embeds server/client IPv4s; the range is effectively
    // dead, so refuse it whole rather than decode which IPv4 it would reach.
    let teredo = s[0] == 0x2001 && s[1] == 0x0000;
    let discard = s[..4] == [0x0100, 0, 0, 0]; // 100::/64 (RFC 6666)
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || unique_local
        || link_local
        || documentation
        || teredo
        || discard)
}

/// The IPv4 address an IPv6 literal embeds, across every standard embedding —
/// IPv4-mapped (`::ffff:0:0/96`), 6to4 (`2002::/16`), NAT64 well-known
/// (`64:ff9b::/96`), and deprecated IPv4-compatible (`::/96`). `None` for a
/// native IPv6 address. Callers fold the result through [`ipv4_is_public`], so a
/// wrapped public IPv4 stays reachable while a wrapped private one is refused.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = ip.segments();
    let low_v4 =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // 6to4: 2002:AABB:CCDD::/48 carries A.B.C.D in segments 1 and 2.
    if s[0] == 0x2002 {
        return Some(low_v4(s[1], s[2]));
    }
    // NAT64 well-known prefix: the trailing 32 bits are the IPv4.
    if s[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
        return Some(low_v4(s[6], s[7]));
    }
    // IPv4-compatible ::/96 (deprecated): high 96 bits zero. Exclude `::` and
    // `::1`, which are the unspecified/loopback addresses, not IPv4 embeddings.
    if s[..6] == [0, 0, 0, 0, 0, 0] {
        let v4 = low_v4(s[6], s[7]);
        if !v4.is_unspecified() && v4 != Ipv4Addr::new(0, 0, 0, 1) {
            return Some(v4);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Url;

    #[test]
    fn ssrf_guard_rejects_private_hosts() {
        for url in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            // IPv6 literals that embed an internal IPv4 by a non-mapped route:
            // NAT64 well-known prefix, 6to4, and deprecated IPv4-compatible.
            "http://[64:ff9b::a9fe:a9fe]/", // NAT64 + 169.254.169.254 (metadata)
            "http://[64:ff9b::7f00:1]/",    // NAT64 + 127.0.0.1
            "http://[2002:7f00:1::]/",      // 6to4 + 127.0.0.1
            "http://[2002:a9fe:a9fe::]/",   // 6to4 + 169.254.169.254
            "http://[::7f00:1]/",           // IPv4-compatible + 127.0.0.1
            "http://[2001:db8::1]/",        // documentation
            "http://[2001::1]/",            // Teredo
            "http://[100::1]/",             // discard-only (RFC 6666)
        ] {
            let parsed = Url::parse(url).unwrap();
            assert!(
                check_url(&parsed, false, Lang::Zh).is_err(),
                "{url} must be rejected"
            );
        }
    }

    #[test]
    fn ssrf_guard_accepts_public_literals_and_rejects_schemes() {
        let public = Url::parse("https://1.1.1.1/").unwrap();
        assert!(check_url(&public, false, Lang::Zh).is_ok());
        // A public IPv4 wrapped in NAT64 / 6to4 stays reachable — the fold must
        // not over-block legitimate public destinations, only internal ones.
        for public_v6 in ["http://[64:ff9b::808:808]/", "http://[2002:808:808::]/"] {
            let parsed = Url::parse(public_v6).unwrap();
            assert!(
                check_url(&parsed, false, Lang::Zh).is_ok(),
                "{public_v6} (wraps public 8.8.8.8) must be allowed"
            );
        }
        let file = Url::parse("file:///etc/passwd").unwrap();
        assert!(check_url(&file, false, Lang::Zh).is_err());
        let ftp = Url::parse("ftp://example.com/").unwrap();
        assert!(check_url(&ftp, false, Lang::Zh).is_err());
    }
}
