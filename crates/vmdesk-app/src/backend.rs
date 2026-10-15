//! Everything off the UI thread. The database and SSH code run on tokio; GPUI has its own
//! executor, so each operation is spawned on the tokio runtime and its join handle is
//! awaited from a GPUI task.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use tokio::runtime::Handle;
use vmdesk_core::{
    CredentialRef, EndpointKind, Error as CoreError, Hop, HostKey, HostKeyStore, Vm, VmId, VmStatus,
};
use vmdesk_db::{Db, EndpointInput, ListFilter, NewVm, TimelineEntry, VmPatch};
use vmdesk_proto::Snapshot;
use vmdesk_reach::Monitor;
use vmdesk_transport::Transport;

/// Events shown in the timeline tab.
const TIMELINE_LIMIT: u32 = 300;

#[derive(Clone)]
pub struct Backend {
    rt: Handle,
    pub db: Db,
    pub data_dir: PathBuf,
}

/// What the VM list needs, loaded in one go.
pub struct Inventory {
    pub vms: Vec<Vm>,
    pub status: HashMap<VmId, VmStatus>,
}

/// How a new VM logs in.
pub enum Auth {
    Agent,
    KeyFile(PathBuf),
    /// Stored in the OS keychain; the database keeps only the entry name.
    Password(String),
    /// Editing a password VM without typing a new one.
    KeepPassword,
}

/// A VM entered in the "Add VM" form.
pub struct NewVmSpec {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub user: String,
    pub via: Option<VmId>,
    pub auth: Auth,
}

/// A host key that blocks the connection, for the user to decide on. `vm` is the
/// machine whose key it is: the VM itself or one of its jump hosts.
#[derive(Clone)]
pub enum KeyProblem {
    /// Never seen: trust it after checking the fingerprint.
    Unknown {
        vm: VmId,
        name: String,
        key: HostKey,
        /// Whether `~/.ssh/known_hosts` has this same key for the address (`None`: no entry).
        known_hosts_match: Option<bool>,
    },
    /// Different from the trusted one: only forget the old key if the VM was reinstalled.
    Changed {
        vm: VmId,
        name: String,
        expected: String,
        offered: String,
    },
}

/// The detail pane's data for one VM.
pub struct Detail {
    pub snapshot: Option<Snapshot>,
    pub timeline: Vec<TimelineEntry>,
}

impl Backend {
    pub fn new(rt: Handle, db: Db, data_dir: PathBuf) -> Self {
        Self { rt, db, data_dir }
    }

    /// Runs `fut` on tokio; the returned future can be awaited from any executor.
    fn run<T, F>(&self, fut: F) -> impl Future<Output = anyhow::Result<T>> + use<T, F>
    where
        T: Send + 'static,
        F: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let handle = self.rt.spawn(fut);
        async move { handle.await.context("background task panicked")? }
    }

    /// Starts the reachability monitor and the snapshot poller for the app's lifetime.
    pub fn start_background(&self) {
        let db = self.db.clone();
        self.rt.spawn(async move {
            if let Err(e) = Monitor::new(db).run(|_| {}).await {
                tracing::error!(error = %e, "reachability monitor stopped");
            }
        });
        let db = self.db.clone();
        self.rt.spawn(async move {
            if let Err(e) = vmdesk_collect::remote::poll_forever(&db).await {
                tracing::error!(error = %e, "snapshot poller stopped");
            }
        });
    }

    pub fn inventory(&self) -> impl Future<Output = anyhow::Result<Inventory>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            Ok(Inventory {
                vms: db.list_vms(&ListFilter::default()).await?,
                status: db.all_status().await?,
            })
        })
    }

    pub fn detail(&self, vm: VmId) -> impl Future<Output = anyhow::Result<Detail>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            Ok(Detail {
                snapshot: db.latest_snapshot(vm).await?,
                timeline: db.timeline(vm, TIMELINE_LIMIT, None).await?,
            })
        })
    }

    /// Route and SSH check for one VM, stored like the monitor's.
    pub fn check(&self, vm: VmId) -> impl Future<Output = anyhow::Result<()>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            Monitor::new(db).check_now(Some(&[vm])).await?;
            Ok(())
        })
    }

    /// Collects and stores a fresh snapshot of `vm`.
    pub fn collect(&self, vm: VmId) -> impl Future<Output = anyhow::Result<()>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            let all = db.list_vms(&ListFilter::default()).await?;
            let target = all
                .iter()
                .find(|v| v.id == vm)
                .cloned()
                .context("VM no longer exists")?;
            let inventory = all.into_iter().map(|v| (v.id, v)).collect();
            let transport = Transport::new(Arc::new(db.clone()) as Arc<dyn HostKeyStore>);
            let res = vmdesk_collect::remote::collect(&db, &transport, &target, &inventory).await;
            transport.close_all().await;
            res?;
            Ok(())
        })
    }

    /// Connects without trusting anything new, to find out which host key is in the way.
    /// `None` when the connection works (or fails for another reason).
    pub fn host_key_problem(
        &self,
        vm: VmId,
    ) -> impl Future<Output = anyhow::Result<Option<KeyProblem>>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            let all = db.list_vms(&ListFilter::default()).await?;
            let target = all
                .iter()
                .find(|v| v.id == vm)
                .cloned()
                .context("VM no longer exists")?;
            let id_of = |name: &str| all.iter().find(|v| v.name == name).map(|v| v.id);
            let inventory = all.iter().map(|v| (v.id, v.clone())).collect();
            let transport = Transport::new(Arc::new(db.clone()) as Arc<dyn HostKeyStore>);
            let res = transport.session(&target, &inventory).await;
            transport.close_all().await;
            Ok(match res {
                Err(CoreError::UnknownHostKey {
                    vm: name,
                    key_type,
                    fingerprint,
                }) => {
                    let owner = all.iter().find(|v| v.name == name);
                    let known_hosts_match = owner
                        .and_then(|v| v.endpoints.first())
                        .and_then(|ep| known_hosts_fingerprints(&ep.address, ep.port))
                        .map(|fps| fps.contains(&fingerprint));
                    owner.map(|v| KeyProblem::Unknown {
                        vm: v.id,
                        name,
                        key: HostKey {
                            key_type,
                            fingerprint,
                        },
                        known_hosts_match,
                    })
                }
                Err(CoreError::HostKeyChanged {
                    vm: name,
                    expected,
                    offered,
                }) => id_of(&name).map(|id| KeyProblem::Changed {
                    vm: id,
                    name,
                    expected,
                    offered,
                }),
                _ => None,
            })
        })
    }

    /// Stores exactly `key` as trusted for `vm` (the key the user was shown).
    pub fn trust_host_key(
        &self,
        vm: VmId,
        key: HostKey,
    ) -> impl Future<Output = anyhow::Result<()>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            db.trust_host_key(vm, &key).await?;
            Ok(())
        })
    }

    /// Removes `vm`'s trusted keys, after a reinstall changed them.
    pub fn forget_host_keys(&self, vm: VmId) -> impl Future<Output = anyhow::Result<()>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            db.forget_host_keys(vm).await?;
            Ok(())
        })
    }

    /// Adds a VM and, for password logins, stores the password in the keychain.
    pub fn add_vm(&self, spec: NewVmSpec) -> impl Future<Output = anyhow::Result<VmId>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            let credential = match &spec.auth {
                Auth::KeyFile(path) => {
                    anyhow::ensure!(path.is_file(), "no key file at {}", path.display());
                    CredentialRef::KeyFile(path.clone())
                }
                Auth::Agent | Auth::Password(_) | Auth::KeepPassword => CredentialRef::SshAgent,
            };
            let paths = spec
                .via
                .map(|via| vec![vec![Hop::Jump { via }, Hop::Direct]]);
            let vm = db
                .create_vm(NewVm {
                    name: spec.name,
                    username: spec.user,
                    credential,
                    endpoints: vec![EndpointInput {
                        address: spec.address,
                        port: spec.port,
                        kind: EndpointKind::default(),
                        bind_interface: None,
                    }],
                    paths,
                    tags: vec![],
                    favourite: false,
                })
                .await?;
            if let Auth::Password(password) = &spec.auth {
                // Same scheme as `vmdesk set-password`: one entry per VM, named by its id.
                let entry = vm.id.to_string();
                vmdesk_transport::keychain::set_password(&entry, password).await?;
                db.update_vm(
                    vm.id,
                    VmPatch {
                        credential: Some(CredentialRef::KeyChain(entry)),
                        ..VmPatch::default()
                    },
                )
                .await?;
            }
            Ok(vm.id)
        })
    }

    /// Saves an edited VM. Connection details are only sent when they changed, so editing
    /// the user of an imported VM leaves its imported endpoint and path alone.
    pub fn update_vm(
        &self,
        id: VmId,
        spec: NewVmSpec,
        endpoint_changed: bool,
        via_changed: bool,
    ) -> impl Future<Output = anyhow::Result<()>> + use<> {
        let db = self.db.clone();
        self.run(async move {
            let current = db.get_vm(id).await?;
            let old_entry = match &current.credential {
                CredentialRef::KeyChain(e) => Some(e.clone()),
                _ => None,
            };
            let credential = match spec.auth {
                Auth::KeepPassword => None,
                Auth::Agent => Some(CredentialRef::SshAgent),
                Auth::KeyFile(path) => {
                    anyhow::ensure!(path.is_file(), "no key file at {}", path.display());
                    Some(CredentialRef::KeyFile(path))
                }
                Auth::Password(password) => {
                    let entry = old_entry.clone().unwrap_or_else(|| id.to_string());
                    vmdesk_transport::keychain::set_password(&entry, &password).await?;
                    Some(CredentialRef::KeyChain(entry))
                }
            };
            let dropping_password = credential
                .as_ref()
                .is_some_and(|c| !matches!(c, CredentialRef::KeyChain(_)));
            db.update_vm(
                id,
                VmPatch {
                    name: Some(spec.name),
                    username: Some(spec.user),
                    credential,
                    endpoints: endpoint_changed.then(|| {
                        vec![EndpointInput {
                            address: spec.address,
                            port: spec.port,
                            kind: EndpointKind::default(),
                            bind_interface: None,
                        }]
                    }),
                    paths: via_changed.then(|| match spec.via {
                        Some(via) => vec![vec![Hop::Jump { via }, Hop::Direct]],
                        None => vec![vec![Hop::Direct]],
                    }),
                    ..VmPatch::default()
                },
            )
            .await?;
            if let (true, Some(entry)) = (dropping_password, old_entry) {
                vmdesk_transport::keychain::delete_password(&entry).await?;
            }
            Ok(())
        })
    }

    /// Removes a VM with its keychain password and terminal files. Fails while another
    /// VM uses it as a jump host.
    pub fn remove_vm(&self, vm: VmId) -> impl Future<Output = anyhow::Result<String>> + use<> {
        let (db, dir) = (self.db.clone(), self.data_dir.join("ssh"));
        self.run(async move {
            let target = db.get_vm(vm).await?;
            db.delete_vm(vm).await?;
            if let CredentialRef::KeyChain(entry) = &target.credential {
                vmdesk_transport::keychain::delete_password(entry).await?;
            }
            for ext in ["conf", "askpass"] {
                let _ = std::fs::remove_file(dir.join(format!("{vm}.{ext}")));
            }
            Ok(format!("removed {}", target.name))
        })
    }

    /// Opens the configured terminal with `ssh` to `vm` over its connection path.
    pub fn open_terminal(&self, vm: VmId) -> impl Future<Output = anyhow::Result<String>> + use<> {
        let (db, dir) = (self.db.clone(), self.data_dir.clone());
        self.run(async move {
            let all = db.list_vms(&ListFilter::default()).await?;
            let target = all
                .iter()
                .find(|v| v.id == vm)
                .context("VM no longer exists")?;
            crate::terminal::open(target, &all, &dir)
        })
    }
}

/// Fingerprints `~/.ssh/known_hosts` holds for `address` (`ssh-keygen -lF`), or `None`
/// when it has no entry or `ssh-keygen` is missing.
fn known_hosts_fingerprints(address: &str, port: u16) -> Option<Vec<String>> {
    let host = if port == 22 {
        address.to_owned()
    } else {
        format!("[{address}]:{port}")
    };
    let out = std::process::Command::new("ssh-keygen")
        .args(["-lF", &host])
        .output()
        .ok()?;
    let fps: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().nth(2).map(str::to_owned))
        .collect();
    (!fps.is_empty()).then_some(fps)
}
