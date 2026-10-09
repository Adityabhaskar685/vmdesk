//! Minimal `~/.ssh/config` reader: `Host`, `Hostname`, `User`, `Port`, `ProxyJump`.
//! Wildcard hosts are skipped. `Include` and `Match` are not supported.

use std::collections::BTreeMap;

use vmdesk_core::{Endpoint, EndpointKind, EndpointSource, Lifecycle, ProviderVm};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEntry {
    pub alias: String,
    pub host_name: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub proxy_jump: Option<String>,
}

#[must_use]
pub fn parse(text: &str) -> Vec<HostEntry> {
    let mut out: Vec<HostEntry> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once(|c: char| c.is_whitespace() || c == '=')
            .map_or((line, ""), |(k, v)| {
                (
                    k,
                    v.trim_start_matches(|c: char| c.is_whitespace() || c == '=')
                        .trim(),
                )
            });
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                current.clear();
                for alias in value
                    .split_whitespace()
                    .filter(|a| !a.contains(['*', '?', '!']))
                {
                    current.push(out.len());
                    out.push(HostEntry {
                        alias: alias.to_owned(),
                        ..HostEntry::default()
                    });
                }
            }
            "match" => current.clear(),
            k => {
                for &i in &current {
                    let e = &mut out[i];
                    match k {
                        "hostname" => e.host_name = Some(value.to_owned()),
                        "user" => e.user = Some(value.to_owned()),
                        "port" => e.port = value.parse().ok(),
                        "proxyjump" if value != "none" => e.proxy_jump = Some(value.to_owned()),
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

impl HostEntry {
    #[must_use]
    pub fn into_provider_vm(self) -> ProviderVm {
        let mut labels = BTreeMap::new();
        if let Some(j) = &self.proxy_jump {
            // Resolved into a Jump hop once the bastion is in the inventory.
            labels.insert("ssh_config.proxy_jump".to_owned(), j.clone());
        }
        ProviderVm {
            endpoints: vec![Endpoint {
                address: self.host_name.clone().unwrap_or_else(|| self.alias.clone()),
                port: self.port.unwrap_or(22),
                kind: EndpointKind::Private,
                source: EndpointSource::Provider,
                bind_interface: None,
                last_success: None,
            }],
            external_ref: self.alias.clone(),
            name: self.alias,
            username: self.user,
            labels,
            lifecycle: Lifecycle::Unknown,
            suggested_paths: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts() {
        let cfg = "Host *\n  ServerAliveInterval 30\n\nHost regal-dev-07 r7\n  HostName 10.24.18.32\n  User aditya\n  ProxyJump bastion-01\n\nHost bastion-01\n  HostName=172.20.4.1\n  Port 2222\n";
        let hosts = parse(cfg);
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].alias, "regal-dev-07");
        assert_eq!(hosts[1].host_name.as_deref(), Some("10.24.18.32"));
        assert_eq!(hosts[0].proxy_jump.as_deref(), Some("bastion-01"));
        assert_eq!(hosts[2].port, Some(2222));
        let vm = hosts[2].clone().into_provider_vm();
        assert_eq!(vm.endpoints[0].address, "172.20.4.1");
    }
}
