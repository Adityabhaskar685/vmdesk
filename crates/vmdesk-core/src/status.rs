//! Reachability signals per VM (spec section 5): route, SSH and agent, plus the
//! provider's lifecycle, reduced to one headline for display.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Error, Lifecycle, VmId};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshState {
    Ok,
    AuthFailed,
    /// Host key unknown or changed; the connection was not made.
    HostKey,
    Refused,
    Timeout,
    NoRoute,
    Error,
    /// Skipped, e.g. the provider reports the VM stopped.
    #[default]
    NotChecked,
}

/// Agent handshake result over the VM's SSH session (spec section 10).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Not probed yet, or the VM was not reachable.
    #[default]
    Unknown,
    /// No agent binary on the VM.
    Absent,
    /// The agent answered and its daemon is running.
    Ok,
    /// Older than this laptop's version; the daemon still runs.
    Outdated,
    /// Installed, but its daemon is not running: the SSH collector is used instead.
    NotRunning,
    /// The agent did not answer properly (bad output, protocol mismatch).
    Error,
}

impl AgentState {
    /// Whether the agent, not the SSH collector, provides this VM's state.
    #[must_use]
    pub fn collects(self) -> bool {
        matches!(self, Self::Ok | Self::Outdated)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Headline {
    Stopped,
    NoRoute,
    SshFailing,
    Online,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmStatus {
    pub vm: VmId,
    /// Laptop interface that routes to the first hop (the VM, or its first jump host).
    /// `None` when there is no route or the check was skipped.
    pub route: Option<String>,
    pub ssh: SshState,
    pub agent: AgentState,
    pub checked_at: DateTime<Utc>,
    /// Why the SSH check failed, when it did.
    pub detail: Option<String>,
}

impl VmStatus {
    #[must_use]
    pub fn headline(&self, lifecycle: Lifecycle) -> Headline {
        match (lifecycle, self.ssh) {
            (Lifecycle::Stopped | Lifecycle::Terminated, _) => Headline::Stopped,
            (_, SshState::Ok) => Headline::Online,
            (_, SshState::NoRoute) => Headline::NoRoute,
            (_, SshState::NotChecked) => Headline::Unknown,
            _ => Headline::SshFailing,
        }
    }

    /// Whether `self` differs from `other` in a way worth an event.
    #[must_use]
    pub fn changed_from(&self, other: &Self) -> bool {
        self.route != other.route || self.ssh != other.ssh || self.agent != other.agent
    }
}

macro_rules! text_enum {
    ($t: ident { $($v:ident => $s:literal),+ $(,)? }) => {
        impl $t {
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$v => $s),+ }
            }
        }
        impl std::str::FromStr for $t {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                match s {
                    $($s => Ok(Self::$v),)+
                    other => Err(Error::Other(format!(concat!("unknown ", stringify!($t), ": {}"), other))),
                }
            }
        }
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(SshState {
    Ok => "ok",
    AuthFailed => "auth_failed",
    HostKey => "host_key",
    Refused => "refused",
    Timeout => "timeout",
    NoRoute => "no_route",
    Error => "error",
    NotChecked => "not_checked",
});
text_enum!(AgentState {
    Unknown => "unknown",
    Absent => "absent",
    Ok => "ok",
    Outdated => "outdated",
    NotRunning => "not_running",
    Error => "error",
});
text_enum!(Headline {
    Stopped => "stopped",
    NoRoute => "no route",
    SshFailing => "ssh failing",
    Online => "online",
    Unknown => "unknown",
});

#[cfg(test)]
mod tests {
    use super::*;

    fn status(ssh: SshState) -> VmStatus {
        VmStatus {
            vm: VmId::new(),
            route: Some("wlan0".into()),
            ssh,
            agent: AgentState::Unknown,
            checked_at: Utc::now(),
            detail: None,
        }
    }

    #[test]
    fn headline_and_change() {
        assert_eq!(
            status(SshState::Ok).headline(Lifecycle::Unknown),
            Headline::Online
        );
        assert_eq!(
            status(SshState::Ok).headline(Lifecycle::Stopped),
            Headline::Stopped
        );
        assert_eq!(
            status(SshState::NoRoute).headline(Lifecycle::Running),
            Headline::NoRoute
        );
        assert_eq!(
            status(SshState::Timeout).headline(Lifecycle::Unknown),
            Headline::SshFailing
        );

        let a = status(SshState::Ok);
        let mut b = a.clone();
        b.checked_at = Utc::now();
        b.detail = Some("x".into());
        assert!(!b.changed_from(&a));
        b.route = Some("tun0".into());
        assert!(b.changed_from(&a));

        for s in [
            SshState::AuthFailed,
            SshState::HostKey,
            SshState::NotChecked,
        ] {
            assert_eq!(s.as_str().parse::<SshState>().unwrap(), s);
            assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{s}\""));
        }
    }
}
