use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::{Error, Origin, Result, VmId};

/// Ordered hops from laptop to the target VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionPath {
    pub hops: Vec<Hop>,
    #[serde(default)]
    pub source: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hop {
    /// TCP to the target's entpoints, tried to preference order. Always the last hop.
    Direct,
    /// SSH to another inventory VM, then `direct-tcpip` onwards.
    Jump { via: VmId },
    // Future: AwsSsm, GcpIap, AzureBastion...
}

impl ConnectionPath {
    #[must_use]
    pub fn direct(source: Origin) -> Self {
        Self {
            hops: vec![Hop::Direct],
            source,
        }
    }

    /// `[Jump b1, Jump b2, ..., Direct]`.
    #[must_use]
    pub fn via(bastions: &[VmId], source: Origin) -> Self {
        let mut hops: Vec<Hop> = bastions.iter().map(|&via| Hop::Jump { via }).collect();
        hops.push(Hop::Direct);
        Self { hops, source }
    }

    /// VMs this path jumps through, in order.
    pub fn jumps_hosts(&self) -> impl Iterator<Item = VmId> + '_ {
        self.hops.iter().filter_map(|h| match h {
            Hop::Jump { via } => Some(*via),
            Hop::Direct => None,
        })
    }

    /// Structural checks, Loops across VMs are checked against the inventory.
    pub fn validate(&self, target: VmId) -> Result<()> {
        let Some((last, rest)) = self.hops.split_last() else {
            return Err(Error::InvalidPath("path has no hops"));
        };

        if *last != Hop::Direct {
            return Err(Error::InvalidPath("the last host must be direct"));
        }
        if rest.contains(&Hop::Direct) {
            return Err(Error::InvalidPath("only the last hop can be direct"));
        }
        let mut seen = HashSet::new();
        for via in self.jumps_hosts() {
            if via == target {
                return Err(Error::InvalidPath("path jumps through its own target"));
            }
            if !seen.insert(via) {
                return Err(Error::InvalidPath("path uses the same jump host twice"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_paths() {
        let me = VmId::new();
        let (b1, b2) = (VmId::new(), VmId::new());
        assert!(ConnectionPath::direct(Origin::User).validate(me).is_ok());
        assert!(
            ConnectionPath::via(&[b1, b2], Origin::User)
                .validate(me)
                .is_ok()
        );

        let empty = ConnectionPath {
            hops: vec![],
            source: Origin::User,
        };
        assert!(empty.validate(me).is_err());
        let no_direct = ConnectionPath {
            hops: vec![Hop::Jump { via: b1 }],
            source: Origin::User,
        };
        assert!(no_direct.validate(me).is_err());
        let early_direct = ConnectionPath {
            hops: vec![Hop::Direct, Hop::Direct],
            source: Origin::User,
        };
        assert!(early_direct.validate(me).is_err());
        assert!(
            ConnectionPath::via(&[me], Origin::User)
                .validate(me)
                .is_err()
        );
        assert!(
            ConnectionPath::via(&[b1, b1], Origin::User)
                .validate(me)
                .is_err()
        );
    }

    #[test]
    fn hop_json_shape() {
        let via = VmId::new();
        let json = serde_json::to_string(&Hop::Jump { via }).unwrap();
        assert_eq!(json, format!(r#"{{"kind":"jump","via":"{via}"}}"#));
        assert_eq!(
            serde_json::from_str::<Hop>(r#"{"kind":"direct"}"#).unwrap(),
            Hop::Direct
        );
    }
}
