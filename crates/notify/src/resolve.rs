//! DNS for notification requests that never lands on this machine.
//!
//! Notification and webhook URLs can be set from Sentry Manager over the
//! link, and the Pi's API trusts loopback callers without a login. The URL
//! checks catch literal loopback and link-local addresses, but a host name
//! whose DNS answer is 127.0.0.1 would still reach the Pi itself. This
//! resolver drops loopback, unspecified and link-local answers (IPv4,
//! IPv6 and IPv4-mapped), so such a name fails to connect.
//!
//! Private LAN addresses (10/8, 172.16/12, 192.168/16, fc00::/7) stay
//! allowed: Home Assistant and similar often run on the LAN.

use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// An address a notification request must never connect to: this machine
/// (loopback, unspecified, 0.0.0.0/8) or link-local (169.254/16 with the
/// cloud metadata address, fe80::/10).
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified() || v4.octets()[0] == 0 || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Keeps the answers that aren't blocked; none left is an error.
pub fn filter(name: &str, addrs: impl IntoIterator<Item = SocketAddr>) -> std::io::Result<Vec<SocketAddr>> {
    let mut any = false;
    let kept: Vec<SocketAddr> = addrs
        .into_iter()
        .inspect(|_| any = true)
        .filter(|a| !is_blocked(a.ip()))
        .collect();
    if kept.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            if any {
                format!("{name} resolves only to this device or a link-local address; not connecting")
            } else {
                format!("{name} did not resolve")
            },
        ));
    }
    Ok(kept)
}

/// The system resolver, filtered with [`filter`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLocalResolver;

impl Resolve for NoLocalResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let kept = filter(&host, found)?;
            Ok(Box::new(kept.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_link_local_are_blocked_lan_is_not() {
        for ip in [
            "127.0.0.1", "127.8.9.10", "0.0.0.0", "0.1.2.3", "169.254.169.254", "::1", "::", "fe80::1", "febf::1",
            "::ffff:127.0.0.1", "::ffff:169.254.1.1",
        ] {
            assert!(is_blocked(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["192.168.1.5", "10.0.0.2", "172.16.4.4", "172.31.255.1", "100.64.0.1", "8.8.8.8", "fd00::5", "2001:db8::1"] {
            assert!(!is_blocked(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn mixed_answers_keep_only_the_allowed_ones() {
        let a: Vec<SocketAddr> = vec!["127.0.0.1:0".parse().unwrap(), "192.168.1.9:0".parse().unwrap()];
        assert_eq!(filter("ha.lan", a).unwrap(), vec!["192.168.1.9:0".parse::<SocketAddr>().unwrap()]);
        let only_local: Vec<SocketAddr> = vec!["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()];
        let e = filter("evil.example", only_local).unwrap_err();
        assert!(e.to_string().contains("this device"), "{e}");
    }

    #[tokio::test]
    async fn a_name_resolving_to_loopback_is_refused() {
        let r = NoLocalResolver.resolve("localhost".parse().unwrap()).await;
        assert!(r.is_err());
    }
}
