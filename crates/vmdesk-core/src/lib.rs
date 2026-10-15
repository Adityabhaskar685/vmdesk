//! Domain model and three layer traits: [`provider`], [`HopConnector`]
//! and [`Collector`]

mod error;
mod path;
mod traits;
mod vm;

pub use error::{Error, Result};
pub use path::{ConnectionPath, Hop};
pub use traits::{
    ByteStream, Collector, ExecOutput, HopConnector, HostKey, HostKeyStore, Provider, ProviderVm,
    RemoteExec, Tunnel,
};
pub use vm::{
    ALIASES_LABEL, CredentialRef, Endpoint, EndpointKind, Lifecycle, Origin, ProviderId, Vm, VmId,
};
