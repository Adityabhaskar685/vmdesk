//! Domain model and three layer traits: [`provider`], [`HopConnector`]
//! and [`Collector`]

mod error;
mod path;
mod traits;
mod vm;

pub use error::{Error, Result};
pub use path::{ConnectionPath, Hop};
pub use traits::{
    ByteStream, Collector, ExecOutput, HopConnector, Provider, ProviderVm, RemoteExec,
};
pub use vm::{
    CredentialRef, Endpoint, EndpointKind, EndpointSource, Lifecycle, ProviderId, Vm, VmId,
};
