use std::fmt;
use std::path::PathBuf;
use std::{collections::BTreeMap, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ConnectionPath, Error};

/// Label holding extra names a VM is known by (comma-seperated), used to resolve jump hosts.
pub const ALIASES_LABEL: &str = "aliases";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
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

impl fmt::Display for VmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for VmId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|_| Error::Other(format!("invalid VM id: {s}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub const STATIC: &'static str = "static";
    pub const SSH_CONFIG: &'static str = "ssh-config";

    #[must_use]
    pub fn new(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vm {
    pub id: VmId,
    pub name: String,
    pub provider: ProviderId,
    pub external_ref: Option<String>,
    pub username: String,
    pub credential: CredentialRef,
    pub endpoints: Vec<Endpoint>,
    pub paths: Vec<ConnectionPath>,
    pub tags: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub lifecycle: Lifecycle,
    pub favourite: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub address: String,
    pub port: u16,
    pub kind: EndpointKind,
    pub source: Origin,
    pub bind_interface: Option<String>,
    pub last_success: Option<DateTime<Utc>>,
}

/// Where the secret lives, The database store this, never the secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CredentialRef {
    #[default]
    SshAgent,
    KeyFile(PathBuf),
    KeyChain(String),
    CloudProvider(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    #[default]
    Private,
    Public,
    Management,
}

/// Who owns a field: provider sync overrides `Provider` data and never touches `User` data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Hash, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    User,
    Provider,
    Discovered,
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

/// `as_str` / `FromStr` matching the serde names, for database columns
macro_rules! str_enum {
    ($t: ident { $($v:ident => $s:literal),+ $(,)? }) => {
        impl $t {
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$v => $s),+ }
            }
        }
        impl FromStr for $t {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                match s {
                    $($s => Ok(Self::$v),)+
                    other => Err(Error::Other(format!(concat!("unknown ", stringify!($t), ": {}"), other))),
                }
            }
        }
    };
}

str_enum!(EndpointKind { Private => "private", Public => "public", Management => "management" });
str_enum!(Origin { User => "user", Provider => "provider", Discovered => "discovered" });
str_enum!(Lifecycle {
    Running => "running",
    Stopped => "stopped",
    Terminated => "terminated",
    Unknown => "unknown",
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_strings() {
        for l in [
            Lifecycle::Running,
            Lifecycle::Stopped,
            Lifecycle::Terminated,
            Lifecycle::Unknown,
        ] {
            assert_eq!(l.as_str().parse::<Lifecycle>().unwrap(), l);
            assert_eq!(
                serde_json::to_string(&l).unwrap(),
                format!("\"{}\"", l.as_str())
            );
        }
        assert!("bogus".parse::<Origin>().is_err());
        let id = VmId::new();
        assert_eq!(id.to_string().parse::<VmId>().unwrap(), id);
    }
}
