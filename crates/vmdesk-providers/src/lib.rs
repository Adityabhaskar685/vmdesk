//! Inventory providers. Cloud providers live behind Cargo features.

pub mod ssh_config;

use async_trait::async_trait;
use vmdesk_core::{Error, Lifecycle, Provider, ProviderId, ProviderVm, Result};

/// Manully entered VMs. Its record live only in the database. so `list` is empty.
pub struct StaticProvider {
    id: ProviderId,
}

impl Default for StaticProvider {
    fn default() -> Self {
        Self {
            id: ProviderId("static".into()),
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

    async fn lifecycle(&self, external_ref: &str) -> Result<Lifecycle> {
        let _ = external_ref;
        Ok(Lifecycle::Unknown)
    }
    fn supports_sync(&self) -> bool {
        false
    }
}

/// Import `Host` blocks from an SSH config file.
pub struct SshConfigProvider {
    id: ProviderId,
    path: std::path::PathBuf,
}

impl SshConfigProvider {
    #[must_use]
    pub fn new(path: std::path::PathBuf) -> Self {
        Self {
            id: ProviderId("ssh-config".into()),
            path,
        }
    }

    /// `~/.ssh/config`, if a home directory is known.
    pub fn default_path() -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|h| h.join(".ssh").join("config"))
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
        Ok(ssh_config::parse(&text)
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
