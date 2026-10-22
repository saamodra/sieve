//! Keeps clients on the network from using the proxy to reach services that
//! listen only on the Mac's loopback interface.
//!
//! The check applies to the target the client asked for. A rule's
//! `map_remote` is applied afterwards and is never checked: pointing a
//! phone at `localhost:3000` through a rule is the user's own decision.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use crate::config::ProxyConfig;

/// How long to wait for a name to resolve. A name that does not resolve in
/// time is let through; the upstream connection fails on its own then.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether a host is loopback by its spelling alone.
#[derive(Debug, PartialEq, Eq)]
enum Spelling {
    Loopback,
    /// An IP literal that is not loopback.
    Remote,
    /// A name that has to be resolved to know.
    Name,
}

/// `Err` carries the reason a request from `client` to `host:port` is refused.
/// Connections from loopback clients are never refused.
pub(super) async fn check(
    config: &ProxyConfig,
    client: SocketAddr,
    host: &str,
    port: u16,
) -> Result<(), String> {
    if config.allow_lan_to_loopback || client.ip().to_canonical().is_loopback() {
        return Ok(());
    }
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    let loopback = match spelling(host) {
        Spelling::Loopback => true,
        Spelling::Remote => false,
        Spelling::Name => resolves_only_to_loopback(host, port).await,
    };
    if loopback {
        Err(format!(
            "{host} is a loopback address on the machine running sieve, and {} is not on it. \
             Set allow_lan_to_loopback = true under [proxy] in config.toml to allow this",
            client.ip()
        ))
    } else {
        Ok(())
    }
}

fn spelling(host: &str) -> Spelling {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_loopback_or_unspecified(ip) {
            Spelling::Loopback
        } else {
            Spelling::Remote
        };
    }
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        Spelling::Loopback
    } else {
        Spelling::Name
    }
}

/// `0.0.0.0` and `::` connect to the local machine on most systems.
fn is_loopback_or_unspecified(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || ip.is_unspecified()
}

/// Whether every address `host` resolves to is loopback. A name that does
/// not resolve is not, so the request fails upstream with its own error.
async fn resolves_only_to_loopback(host: &str, port: u16) -> bool {
    let lookup = tokio::net::lookup_host((host, port));
    match tokio::time::timeout(RESOLVE_TIMEOUT, lookup).await {
        Ok(Ok(addrs)) => all_loopback(addrs.map(|a| a.ip())),
        _ => false,
    }
}

fn all_loopback(addrs: impl IntoIterator<Item = IpAddr>) -> bool {
    let mut any = false;
    for ip in addrs {
        if !is_loopback_or_unspecified(ip) {
            return false;
        }
        any = true;
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: &str = "192.168.1.20:50000";
    const LOCAL: &str = "127.0.0.1:50000";

    async fn refused(client: &str, host: &str) -> bool {
        check(&ProxyConfig::default(), client.parse().unwrap(), host, 80)
            .await
            .is_err()
    }

    #[tokio::test]
    async fn refuses_loopback_literals_from_the_lan() {
        for host in [
            "127.0.0.1",
            "127.1.2.3",
            "[::1]",
            "::1",
            "0.0.0.0",
            "[::]",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(refused(LAN, host).await, "{host}");
        }
    }

    #[tokio::test]
    async fn refuses_localhost_names_from_the_lan() {
        for host in ["localhost", "LOCALHOST", "localhost.", "app.localhost"] {
            assert!(refused(LAN, host).await, "{host}");
        }
        assert!(refused("[fe80::1]:50000", "localhost").await);
    }

    #[tokio::test]
    async fn allows_other_hosts_from_the_lan() {
        for host in ["93.184.216.34", "10.0.0.5", "[2606:2800::1]"] {
            assert!(!refused(LAN, host).await, "{host}");
        }
        assert_eq!(spelling("example.com"), Spelling::Name);
        assert_eq!(spelling("localhost.example.com"), Spelling::Name);
        assert_eq!(spelling("93.184.216.34"), Spelling::Remote);
    }

    #[tokio::test]
    async fn loopback_clients_are_never_refused() {
        for client in [LOCAL, "[::1]:50000", "[::ffff:127.0.0.1]:50000"] {
            assert!(!refused(client, "127.0.0.1").await, "{client}");
            assert!(!refused(client, "localhost").await, "{client}");
        }
    }

    #[tokio::test]
    async fn the_opt_in_flag_allows_loopback_targets() {
        let config = ProxyConfig {
            allow_lan_to_loopback: true,
            ..ProxyConfig::default()
        };
        let result = check(&config, LAN.parse().unwrap(), "127.0.0.1", 80).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn the_refusal_says_why_and_how_to_opt_in() {
        let message = check(&ProxyConfig::default(), LAN.parse().unwrap(), "[::1]", 80)
            .await
            .unwrap_err();
        assert!(message.contains("::1 is a loopback address"), "{message}");
        assert!(message.contains("192.168.1.20"), "{message}");
        assert!(message.contains("allow_lan_to_loopback"), "{message}");
    }

    #[test]
    fn a_name_is_loopback_only_if_every_address_is() {
        let lo4: IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: IpAddr = "::1".parse().unwrap();
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        assert!(all_loopback([lo4, lo6]));
        assert!(!all_loopback([lo4, public]));
        assert!(!all_loopback([]));
    }
}
