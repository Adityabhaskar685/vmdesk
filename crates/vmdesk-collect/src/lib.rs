//! Probe parsers, snapshot diffing and redaction. Shared by the laptop's SSH
//! collector (`remote` feature) and the agent.

use std::sync::LazyLock;

use regex::Regex;
use vmdesk_proto::TmuxSession;

#[cfg(feature = "remote")]
pub mod remote;

/// `tmux list-session -F`  format the probes use.
pub const TMUX_FORMAT: &str = "#{session_name}\t#{session_windows}\t#{session_attached}";

/// Parses output produced with [`TMUX_FORMAT`].
#[must_use]
pub fn parse_tmux(output: &str) -> Vec<TmuxSession> {
    output
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            Some(TmuxSession {
                name: f.next()?.to_owned(),
                windows: f.next()?.parse().ok()?,
                attached: f.next()? != "0",
            })
        })
        .collect()
}

static SECRETS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(--?(?:password|passwd|token|secret)[= ]|(?:token|password)=|authorization:\s*\S+\s)(\S+)|AKIA[0-9A-Z]{16}")
        .expect("valid regex")
});

/// Masks common secrets in command lines before they are stored.
#[must_use]
pub fn redact(cmdline: &str) -> String {
    SECRETS
        .replace_all(cmdline, |c: &regex::Captures<'_>| match c.get(1) {
            Some(prefix) => format!("{}***", prefix.as_str()),
            None => "***".to_owned(),
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tmux_session() {
        let s = parse_tmux("vpn-0\t2\t1\nvpn-2\t2\t0");
        assert_eq!(s.len(), 2);
        assert!(s[0].attached && !s[1].attached);
    }

    #[test]
    fn redacts_secrets() {
        assert_eq!(redact("run --password=hunter2 x"), "run --password=*** x");
        assert_eq!(redact("curl ?token=abc"), "curl ?token=***");
        assert_eq!(redact("key AKIAABCDEFGHIJKLMNOP"), "key ***");
        assert_eq!(redact("pytest tests/group2"), "pytest tests/group2");
    }
}
