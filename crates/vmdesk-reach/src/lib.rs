//! Route inspection and reachability scheduling.

use async_trait::async_trait;

/// Answer "which local interface would reach this address?".
#[async_trait]
pub trait RouteInspector: Send + Sync {
    async fn route_to(&self, addr: std::net::IpAddr) -> Option<String>;
}

/// Linux implementation backed by `ip route get`.
pub struct IpRouteInspector;

#[async_trait]
impl RouteInspector for IpRouteInspector {
    async fn route_to(&self, addr: std::net::IpAddr) -> Option<String> {
        let out = tokio::process::Command::new("ip")
            .args(["route", "get", &addr.to_string()])
            .output()
            .await
            .ok()?;
        out.status
            .success()
            .then(|| parse_ip_route_dev(&String::from_utf8_lossy(&out.stdout)))?
    }
}

/// Extracts the `dev` field from `ip route get` output.
#[must_use]
pub fn parse_ip_route_dev(output: &str) -> Option<String> {
    let mut words = output.split_whitespace();
    while let Some(w) = words.next() {
        if w == "dev" {
            return words.next().map(str::to_owned);
        }
    }
    None
}

/// Names of local interface that are up and have an address.
#[must_use]
pub fn local_interface() -> Vec<String> {
    netdev::get_interfaces()
        .into_iter()
        .filter(|i| i.is_up() && (!i.ipv4.is_empty() || !i.ipv6.is_empty()))
        .map(|i| i.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dev() {
        let out = "10.24.18.32 via 192.168.1.1 dev tun0 src 172.20.4.9 uid 1000\n    cache\n";
        assert_eq!(parse_ip_route_dev(out).as_deref(), Some("tun0"));
        assert_eq!(parse_ip_route_dev("unreachable"), None);
    }
}
