//! Inventory providers. Cloud providers live behind Cargo features.

pub mod ssh_config;

use std::path::PathBuf;

use async_trait::async_trait;
use vmdesk_core::{Error, Lifecycle, Provider, ProviderId, ProviderVm, Result};

/// Manully entered VMs. Its record live only in the database. so `list` is empty.
pub struct StaticProvider {
    id: ProviderId,
}

impl Default for StaticProvider {
    fn default() -> Self {
        Self {
            id: ProviderId::new(ProviderId::STATIC),
        }
    }
}

#[async_trait]
impl Provider for StaticProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    async fn list(&self) -> Result<Vec<ProviderVm>> {
        Ok(Vec::new())
    }

    async fn lifecycle(&self, _external_ref: &str) -> Result<Lifecycle> {
        Ok(Lifecycle::Unknown)
    }
    fn supports_sync(&self) -> bool {
        false
    }
}

/// Import `Host` blocks from an SSH config file.
pub struct SshConfigProvider {
    id: ProviderId,
    path: PathBuf,
}

impl SshConfigProvider {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            id: ProviderId::new(ProviderId::SSH_CONFIG),
            path,
        }
    }

    /// `~/.ssh/config`, if a home directory is known.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".ssh").join("config"))
    }

    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[async_trait]
impl Provider for SshConfigProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    async fn list(&self) -> Result<Vec<ProviderVm>> {
        let text = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(Error::Io)?;
        let home = dirs::home_dir();
        Ok(ssh_config::parse(&text, home.as_deref())
            .into_iter()
            .map(ssh_config::HostEntry::into_provider_vm)
            .collect())
    }

    async fn lifecycle(&self, _external_ref: &str) -> Result<Lifecycle> {
        Ok(Lifecycle::Unknown)
    }

    fn supports_sync(&self) -> bool {
        true
    }
}
