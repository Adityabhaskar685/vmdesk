use serde::{Deserialize, Serialize};

use crate::{Error, Result, VmId};

/// Ordered hops from laptop to the target VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionPath {
    pub hops: Vec<Hop>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hop {
    /// Tcp from the laptop (or the previous hop) to one of the target's endpoints.
    Direct { endpoint: usize },
    /// SSH to another inventory VM, then `direct-tcpip` onwards.
    Jump { via: VmId },
    // Future: AwsSsm, GcpIap, AzureBastion...
}

impl ConnectionPath {
    #[must_use]
    pub fn direct(endpoint: usize) -> Self {
        Self {
            hops: vec![Hop::Direct { endpoint }],
        }
    }

    /// Structural checks, cycle detection across VMs happens in the inventory.
    pub fn validate(&self, target: &VmId) -> Result<()> {
        match self.hops.last() {
            None => return Err(Error::InvalidPath("path has no hops")),
            Some(Hop::Jump { .. }) => {
                return Err(Error::InvalidPath("last hop must reach the target"));
            }
            Some(Hop::Direct { .. }) => {}
        }
        if self
            .hops
            .iter()
            .any(|h| matches!(h, Hop::Jump { via } if via == target ))
        {
            return Err(Error::InvalidPath("path jumps through its own target"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_paths() {
        let me = VmId::new();
        let bastion = VmId::new();
        assert!(ConnectionPath::direct(0).validate(&me).is_ok());
        let via = ConnectionPath {
            hops: vec![Hop::Jump { via: bastion }, Hop::Direct { endpoint: 0 }],
        };
        assert!(via.validate(&me).is_ok());
        assert!(ConnectionPath { hops: vec![] }.validate(&me).is_err());
        let selfloop = ConnectionPath {
            hops: vec![Hop::Jump { via: me.clone() }, Hop::Direct { endpoint: 0 }],
        };
        assert!(selfloop.validate(&me).is_err());
    }
}
