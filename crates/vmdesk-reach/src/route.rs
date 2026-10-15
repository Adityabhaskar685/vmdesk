//! Asks the OS which interface it would use to reach an address. Routes are
//! only observed, never changed.

use std::net::IpAddr;

use async_trait::async_trait;

/// Answers "which local interface would reach this address?".
#[async_trait]
pub trait RouteInspector: Send + Sync {
    /// The interface name, or `None` when there is no route.
    async fn route_to(&self, addr: IpAddr) -> Option<String>;
}

/// `ip route get` on Linux, `route -n get` on macOS.
pub struct SystemRoutes;

#[async_trait]
impl RouteInspector for SystemRoutes {
    async fn route_to(&self, addr: IpAddr) -> Option<String> {
        let addr = addr.to_string();
        let (program, args, parse): (&str, Vec<&str>, fn(&str) -> Option<String>) =
            if cfg!(target_os = "macos") {
                ("route", vec!["-n", "get", &addr], parse_route_get)
            } else {
                ("ip", vec!["route", "get", &addr], parse_ip_route_dev)
            };
        let out = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .ok()?;
        out.status
            .success()
            .then(|| parse(&String::from_utf8_lossy(&out.stdout)))?
    }
}

/// Extracts the `dev` field from Linux `ip route get` output.
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

/// Extracts the `interface:` line from macOS `route -n get` output.
#[must_use]
pub fn parse_route_get(output: &str) -> Option<String> {
    output.lines().find_map(|l| {
        l.trim()
            .strip_prefix("interface:")
            .map(|i| i.trim().to_owned())
            .filter(|i| !i.is_empty())
    })
}

/// Names of local interfaces that are up and have an address.
#[must_use]
pub fn local_interfaces() -> Vec<String> {
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
    fn parses_linux_and_macos_output() {
        let out = "10.24.18.32 via 192.168.1.1 dev tun0 src 172.20.4.9 uid 1000\n    cache\n";
        assert_eq!(parse_ip_route_dev(out).as_deref(), Some("tun0"));
        assert_eq!(parse_ip_route_dev("unreachable"), None);

        let mac = "   route to: 10.24.18.32\ndestination: default\n    gateway: 192.168.1.1\n  interface: en0\n      flags: <UP,GATEWAY,DONE>\n";
        assert_eq!(parse_route_get(mac).as_deref(), Some("en0"));
        assert_eq!(
            parse_route_get("route: writing to routing socket: not in table"),
            None
        );
    }

    #[tokio::test]
    async fn loopback_has_a_route() {
        let dev = SystemRoutes.route_to("127.0.0.1".parse().unwrap()).await;
        assert!(dev.is_some(), "no route to 127.0.0.1");
    }
}
