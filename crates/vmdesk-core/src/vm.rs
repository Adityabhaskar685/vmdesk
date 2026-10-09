use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ConnectionPath;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VmId(pub Uuid);

impl VmId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for VmId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vm {
    pub id: VmId,
    pub name: String,
    pub provider: ProviderId,
    pub external_ref: Option<String>,
    pub username: String,
    pub credentails: CredentialRef,
    pub endpoints: Vec<Endpoint>,
    pub paths: Vec<ConnectionPath>,
    pub tags: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub lifecycle: Lifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub address: String,
    pub port: u16,
    pub kind: EndpointKind,
    pub source: EndpointSource,
    pub bind_interface: Option<String>,
    pub last_success: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointKind {
    Private,
    Public,
    Management,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSource {
    User,
    Provider,
    Discovered,
}

/// Where the secret lives. The database stores this, never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CredentialRef {
    SshAgent,
    KeyFile(PathBuf),
    KeyChain(String),
    CloudProfile(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Running,
    Stopped,
    Terminated,
    #[default]
    Unknown,
}
