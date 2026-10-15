//! `~/.ssh/config` reader, Options resolve the way `ssh` resolves them: blocks
//! ae read top to bottom and the first value obtained for each option wins,
//! including values from wildcards blocks. `Match` blocks and `Include` are skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use vmdesk_core::{
    ALIASES_LABEL, CredentialRef, Endpoint, EndpointKind, Lifecycle, Origin, ProviderVm,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEntry {
    /// Frist concrete name on the `Host` line.
    pub alias: String,
    /// Other concrete names on the same line (same machine).
    pub aliases: Vec<String>,
    pub host_name: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
    /// `ProxyJump` hosts in order, reduced to the host port.
    pub proxy_jump: Vec<String>,
}

struct Block {
    /// `None` for `Match` blocks, which are never applied.
    patterns: Option<Vec<String>>,
    settings: Vec<(String, String)>,
}

impl Block {
    fn applies_to(&self, host: &str) -> bool {
        let Some(patterns) = &self.patterns else {
            return false;
        };
        let mut matched = false;
        for p in patterns {
            if let Some(negated) = p.strip_prefix('!') {
                if glob(negated, host) {
                    return false;
                }
            } else if glob(p, host) {
                matched = true;
            }
        }
        matched
    }
}

/// Parse an SSH config. `home` expands `~` in `IdentityFile`.
#[must_use]
pub fn parse(text: &str, home: Option<&Path>) -> Vec<HostEntry> {
    // Options before the first `Host` line apply to every host.
    let mut blocks = vec![Block {
        patterns: Some(vec!["*".to_owned()]),
        settings: Vec::new(),
    }];
    let mut hosts: Vec<(String, Vec<String>)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = split_option(line);
        match key.as_str() {
            "host" => {
                let patterns: Vec<String> = value.split_whitespace().map(str::to_owned).collect();
                let concrete: Vec<String> = patterns
                    .iter()
                    .filter(|p| !p.contains(['*', '?', '!']))
                    .cloned()
                    .collect();
                if let Some((first, rest)) = concrete.split_first() {
                    let known = hosts
                        .iter()
                        .any(|(a, more)| a == first || more.contains(first));
                    if !known {
                        hosts.push((first.clone(), rest.to_vec()));
                    }
                }
                blocks.push(Block {
                    patterns: Some(patterns),
                    settings: Vec::new(),
                });
            }
            "match" => blocks.push(Block {
                patterns: None,
                settings: Vec::new(),
            }),
            _ => {
                if let Some(block) = blocks.last_mut() {
                    block.settings.push((key, value.to_owned()));
                }
            }
        }
    }
    hosts
        .into_iter()
        .map(|(alias, aliases)| resolve(&blocks, alias, aliases, home))
        .collect()
}

fn resolve(
    blocks: &[Block],
    alias: String,
    aliases: Vec<String>,
    home: Option<&Path>,
) -> HostEntry {
    let name = alias.clone();
    let mut e = HostEntry {
        alias,
        aliases,
        ..HostEntry::default()
    };
    let mut jump_seen = false;
    for block in blocks.iter().filter(|b| b.applies_to(&name)) {
        for (key, value) in &block.settings {
            match key.as_str() {
                "hostname" if e.host_name.is_none() => {
                    e.host_name = Some(value.replace("%h", &name));
                }
                "user" if e.user.is_none() => e.user = Some(value.clone()),
                "port" if e.port.is_none() => e.port = value.parse().ok(),
                "identityfile" if e.identity_file.is_none() => {
                    e.identity_file = Some(expand_home(value, home));
                }
                "proxyjump" if !jump_seen => {
                    jump_seen = true;
                    if !value.eq_ignore_ascii_case("none") {
                        e.proxy_jump = value.split(',').filter_map(jump_host).collect();
                    }
                }
                _ => {}
            }
        }
    }
    e
}

/// `key value`, `key=value` or `Key = value`, key lowercased, quotes stripped.
fn split_option(line: &str) -> (String, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (key, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let value = rest.strip_prefix('=').unwrap_or(rest).trim();
    (key.to_ascii_lowercase(), value.trim_matches('"'))
}

/// `[ssh://][user@]host[:port]` -> `host`.
fn jump_host(spec: &str) -> Option<String> {
    let s = spec.trim();
    let s = s.strip_prefix("ssh://").unwrap_or(s);
    let s = s.rsplit_once('@').map_or(s, |(_, host)| host);
    let host = match s.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => s.split(':').next()?,
    };
    (!host.is_empty()).then(|| host.to_owned())
}

fn expand_home(value: &str, home: Option<&Path>) -> PathBuf {
    match (value.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(value),
    }
}

/// Case-insensitive glob with `*` and `?`, as used by `Host` patterns.
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

impl HostEntry {
    #[must_use]
    pub fn into_provider_vm(self) -> ProviderVm {
        let mut labels = BTreeMap::new();
        if !self.aliases.is_empty() {
            labels.insert(ALIASES_LABEL.to_owned(), self.aliases.join(","));
        }
        ProviderVm {
            endpoints: vec![Endpoint {
                address: self.host_name.unwrap_or_else(|| self.alias.clone()),
                port: self.port.unwrap_or(22),
                kind: EndpointKind::Private,
                source: Origin::Provider,
                bind_interface: None,
                last_success: None,
            }],
            external_ref: self.alias.clone(),
            name: self.alias,
            username: self.user,
            credential: self.identity_file.map(CredentialRef::KeyFile),
            labels,
            lifecycle: Lifecycle::Unknown,
            jump_via: self.proxy_jump,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# lab VMs
Host regal-dev-07 r7
    HostName 10.24.18.32
    ProxyJump aditya@bastion-01:2222,jump2

Host bastion-01
    HostName=172.20.4.1
    Port 2222
    IdentityFile "~/.ssh/lab_ed25519"

Host jump2 !nothing
    HostName %h.lab.example

Match host secret
    User nobody

Host *
    User aditya
    IdentityFile ~/.ssh/id_ed25519
"#;

    fn hosts() -> Vec<HostEntry> {
        parse(CONFIG, Some(Path::new("/home/aditya")))
    }

    #[test]
    fn one_entry_per_machine_with_aliases() {
        let h = hosts();
        let names: Vec<&str> = h.iter().map(|e| e.alias.as_str()).collect();
        assert_eq!(names, ["regal-dev-07", "bastion-01", "jump2"]);
        assert_eq!(h[0].aliases, ["r7"]);
    }

    #[test]
    fn wildcard_values_fill_gaps_first_value_wins() {
        let h = hosts();
        assert_eq!(h[0].user.as_deref(), Some("aditya"));
        assert_eq!(
            h[1].identity_file,
            Some(PathBuf::from("/home/aditya/.ssh/lab_ed25519"))
        );
        assert_eq!(
            h[0].identity_file,
            Some(PathBuf::from("/home/aditya/.ssh/id_ed25519"))
        );
        assert_eq!(h[1].port, Some(2222));
        assert_eq!(h[2].host_name.as_deref(), Some("jump2.lab.example"));
    }

    #[test]
    fn wildcard_first_overrides_later_specific_block() {
        let h = parse("Host *\n  User root\nHost a\n  User aditya\n", None);
        assert_eq!(h[0].user.as_deref(), Some("root"));
    }

    #[test]
    fn proxy_jump_reduced_to_hosts() {
        assert_eq!(hosts()[0].proxy_jump, ["bastion-01", "jump2"]);
        assert_eq!(
            jump_host("ssh://u@[fe80::1]:22").as_deref(),
            Some("fe80::1")
        );
        let none = parse("Host a\n  ProxyJump none\nHost *\n  ProxyJump b\n", None);
        assert!(none[0].proxy_jump.is_empty());
    }

    #[test]
    fn negated_pattern_excludes_host() {
        let h = parse("Host a b\nHost * !b\n  User x\n", None);
        assert_eq!(h[0].user.as_deref(), Some("x"));
        let b = parse("Host b\nHost * !b\n  User x\n", None);
        assert_eq!(b[0].user, None);
    }

    #[test]
    fn glob_matches() {
        assert!(glob("*.lab", "x.LAB"));
        assert!(glob("r?-*", "r7-dev"));
        assert!(!glob("r?", "r77"));
    }

    #[test]
    fn provider_vm_mapping() {
        let vm = hosts().remove(0).into_provider_vm();
        assert_eq!(vm.external_ref, "regal-dev-07");
        assert_eq!(vm.endpoints[0].address, "10.24.18.32");
        assert_eq!(vm.endpoints[0].port, 22);
        assert_eq!(vm.labels[ALIASES_LABEL], "r7");
        assert_eq!(vm.jump_via, ["bastion-01", "jump2"]);
        assert!(matches!(vm.credential, Some(CredentialRef::KeyFile(_))));
    }
}
