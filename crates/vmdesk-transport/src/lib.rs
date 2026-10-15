//! SSH transport: builds a [`vmdesk_core::ConnectionPath`] hop by hop and runs
//! the Russh handshake over the final byte stream.

mod auth;
mod connect;
mod hostkey;
mod session;

use std::sync::Arc;
use std::time::Duration;

pub use connect::{TcpConnector, Transport};
pub use hostkey::host_key;
pub use russh;
pub use session::SshSession;

/// Per-hop connect timeout
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// SSH keepalive interval.
pub const KEEPALIVE: Duration = Duration::from_secs(30);
/// Cached sessions close after this much idle time.
pub const IDLE_CLOSE: Duration = Duration::from_mins(10);

/// Russh client configuration shared by every session.
#[must_use]
pub fn client_config() -> Arc<russh::client::Config> {
    Arc::new(russh::client::Config {
        inactivity_timeout: Some(IDLE_CLOSE),
        keepalive_interval: Some(KEEPALIVE),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use russh::keys::{Algorithm, PrivateKey};

    use super::*;

    #[test]
    fn config_and_fingerprint() {
        let cfg = client_config();
        assert_eq!(cfg.keepalive_interval, Some(KEEPALIVE));
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let hk = host_key(key.public_key());
        assert_eq!(hk.key_type, "ssh-ed25519");
        assert!(hk.fingerprint.starts_with("SHA256:"));
    }
}
