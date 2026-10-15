//! Russh client handler: verifies the server's host key against the [`HostKeyStore`].

use std::sync::{Arc, Mutex};

use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};
use vmdesk_core::{Error, HostKey, HostKeyStore, Result, VmId};

/// The algorithm and fingerprint stored for a server key.
#[must_use]
pub fn host_key(key: &PublicKey) -> HostKey {
    HostKey {
        key_type: key.algorithm().as_str().to_owned(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
    }
}

pub(crate) struct Client {
    pub(crate) vm: VmId,
    pub(crate) name: String,
    pub(crate) store: Arc<dyn HostKeyStore>,
    pub(crate) accept_new: bool,
    /// Why the key was rejected; russh itself only reports `UnknownKey`.
    pub(crate) rejected: Arc<Mutex<Option<Error>>>,
}

impl Client {
    async fn verify(&self, offered: &HostKey) -> Result<()> {
        let trusted = self.store.host_keys(self.vm).await?;
        if trusted.contains(offered) {
            return Ok(());
        }
        // Any other trusted key, even of another type, means the server changed.
        if let Some(expected) = trusted.first() {
            return Err(Error::HostKeyChanged {
                vm: self.name.clone(),
                expected: format!("{} {}", expected.key_type, expected.fingerprint),
                offered: format!("{} {}", offered.key_type, offered.fingerprint),
            });
        }
        if !self.accept_new {
            return Err(Error::UnknownHostKey {
                vm: self.name.clone(),
                key_type: offered.key_type.clone(),
                fingerprint: offered.fingerprint.clone(),
            });
        }
        self.store.trust_host_key(self.vm, offered).await?;
        tracing::info!(
            vm = %self.name,
            key = %offered.fingerprint,
            "trusted new host key"
        );
        Ok(())
    }
}

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        match self.verify(&host_key(&key)).await {
            Ok(()) => Ok(true),
            Err(e) => {
                if let Ok(mut slot) = self.rejected.lock() {
                    *slot = Some(e);
                }
                Ok(false)
            }
        }
    }
}
