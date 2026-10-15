use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use vmdesk_proto::{Event, Snapshot};

use crate::{CredentialRef, Endpoint, Hop, Lifecycle, ProviderId, Result};

/// A VM record as reported by an inventory source.
#[derive(Debug, Clone)]
pub struct ProviderVm {
    /// Stable ID within the provider (SSH config alias, cloud instance ID).
    pub external_ref: String,
    pub name: String,
    pub username: Option<String>,
    pub credential: Option<CredentialRef>,
    pub endpoints: Vec<Endpoint>,
    pub labels: BTreeMap<String, String>,
    pub lifecycle: Lifecycle,
    /// Jump hosts to traverse in order, named by externel ref, alias or VM name.
    pub jump_via: Vec<String>,
}

/// Where VMs comes from. Read-only: never creates or delete machines.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &ProviderId;
    async fn list(&self) -> Result<Vec<ProviderVm>>;
    async fn lifecycle(&self, external_ref: &str) -> Result<Lifecycle>;
    fn supports_sync(&self) -> bool;
}

/// Any bidirectional byte stream (TCP socket, SSH channel, cloud tunnel).
pub trait ByteStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ByteStream for T {}

/// Open one hop of [`ConnectionPath`].
#[async_trait]
pub trait HopConnector: Send + Sync {
    async fn open(
        &self,
        hop: &Hop,
        upstream: Option<&dyn RemoteExec>,
    ) -> Result<Box<dyn ByteStream>>;
}

#[derive(Debug, Clone, Default)]
pub struct ExecOutput {
    pub status: Option<u32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// What collectors need from an established session. Implemented by the transport.
#[async_trait]
pub trait RemoteExec: Send + Sync {
    async fn exec(&self, command: &str) -> Result<ExecOutput>;
}

/// Produces state for one VM. SSH-only and agent collectors share this contract.
#[async_trait]
pub trait Collector: Send + Sync {
    async fn snapshot(&self, session: &dyn RemoteExec) -> Result<Snapshot>;
    async fn events_since(&self, session: &dyn RemoteExec, cursor: u64) -> Result<Vec<Event>>;
}
