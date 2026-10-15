use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use vmdesk_proto::{Event, Snapshot};

use crate::{CredentialRef, Endpoint, Hop, Lifecycle, ProviderId, Result, Vm, VmId};

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

/// Opens TCP connections from an established session (SSH `direct-tcpip`).
#[async_trait]
pub trait Tunnel: Send + Sync {
    async fn open_tcp(&self, host: &str, port: u16) -> Result<Box<dyn ByteStream>>;
}

/// Opens one hop of a [`ConnectionPath`](crate::ConnectionPath): a byte stream to the
/// SSH port of `dest` (the jump host for [`Hop::Jump`], the target for [`Hop::Direct`]),
/// through `upstream` when an earlier hop exists.
#[async_trait]
pub trait HopConnector: Send + Sync {
    async fn open(
        &self,
        hop: &Hop,
        dest: &Vm,
        upstream: Option<&dyn Tunnel>,
    ) -> Result<Box<dyn ByteStream>>;
}

/// A confirmed SSH host key for a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    /// Algorithm name, e.g. `ssh-ed25519`.
    pub key_type: String,
    /// `SHA256:<base64>`, as printed by `ssh-keygen -l`.
    pub fingerprint: String,
}

/// Trusted host keys per VM (trust on first use). Implemented by the database.
#[async_trait]
pub trait HostKeyStore: Send + Sync {
    async fn host_keys(&self, vm: VmId) -> Result<Vec<HostKey>>;
    async fn trust_host_key(&self, vm: VmId, key: &HostKey) -> Result<()>;
    /// Removes every stored key for `vm`, returning how many there were.
    async fn forget_host_keys(&self, vm: VmId) -> Result<u64>;
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
