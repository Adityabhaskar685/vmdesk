//! Domain model and three layer traits: [`provider`], [`HopConnector`]
//! and [`Collector`]

mod error;
mod path;
mod status;
mod traits;
mod vm;

pub use error::{Error, Result};
pub use path::{ConnectionPath, Hop};
pub use status::{AgentState, Headline, SshState, VmStatus};
pub use traits::{
    ByteStream, Collector, ExecOutput, HopConnector, HostKey, HostKeyStore, Provider, ProviderVm,
    RemoteExec, Tunnel,
};
pub use vm::{
    ALIASES_LABEL, CredentialRef, Endpoint, EndpointKind, Lifecycle, Origin, ProviderId, Vm, VmId,
};

/// A duration in seconds as `45s`, `12m`, `3h05m` or `2d4h`.
#[must_use]
pub fn human_duration(secs: i64) -> String {
    let d = secs.max(0);
    match d {
        0..60 => format!("{d}s"),
        60..3600 => format!("{}m", d / 60),
        3600..86_400 => format!("{}h{:02}m", d / 3600, d % 3600 / 60),
        _ => format!("{}d{}h", d / 86_400, d % 86_400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_durations() {
        assert_eq!(super::human_duration(-5), "0s");
        assert_eq!(super::human_duration(125), "2m");
        assert_eq!(super::human_duration(3 * 3600 + 5 * 60), "3h05m");
        assert_eq!(super::human_duration(2 * 86_400 + 4 * 3600), "2d4h");
    }
}
