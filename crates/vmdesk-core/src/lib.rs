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
