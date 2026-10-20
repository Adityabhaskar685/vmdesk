//! Probe parsers, snapshot diffing and redaction. Shared by the laptop's SSH
//! collector (`remote` feature) and the agent.

use std::sync::LazyLock;

use regex::Regex;

#[cfg(feature = "remote")]
pub mod agent;
#[cfg(feature = "remote")]
pub mod agent_install;
pub mod diff;
#[cfg(all(test, feature = "remote"))]
mod fake_vm;
pub mod probe;
#[cfg(feature = "remote")]
pub mod remote;
#[cfg(feature = "remote")]
pub mod work;

pub use diff::diff;
pub use probe::{PROBE_SCRIPT, parse, probe_command};

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
    fn redacts_secrets() {
        assert_eq!(redact("run --password=hunter2 x"), "run --password=*** x");
        assert_eq!(redact("curl ?token=abc"), "curl ?token=***");
        assert_eq!(redact("key AKIAABCDEFGHIJKLMNOP"), "key ***");
        assert_eq!(redact("pytest tests/group2"), "pytest tests/group2");
    }
}
