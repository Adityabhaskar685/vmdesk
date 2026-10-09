//! SSH transport: builds a [`vmdesk_core::ConnectionPath`] hop by hop and runs
//! the Russh handshake over the final byte stream.

use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

pub use russh;

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

/// `SHA256:<hex>` fingerprint of a raw public key blob, stored in `known_hosts`.
#[must_use]
pub fn fingerprint(public_key_blob: &[u8]) -> String {
    format!("SHA256:{}", hex::encode(Sha256::digest(public_key_blob)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_fingerprint() {
        let cfg = client_config();
        assert_eq!(cfg.keepalive_interval, Some(KEEPALIVE));
        let fp = fingerprint(b"key");
        assert!(fp.starts_with("SHA256:"));
        assert_eq!(fp.len(), 7 + 64);
    }
}
