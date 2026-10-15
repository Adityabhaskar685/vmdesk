//! Runs checks for the whole inventory: on demand, on a schedule, and when the laptop's
//! addresses change (VPN up or down, Wi-Fi switch).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::mpsc;
use vmdesk_core::{HostKeyStore, SshState, Vm, VmId, VmStatus};
use vmdesk_db::{Db, ListFilter};
use vmdesk_transport::Transport;

use crate::{Probe, RouteInspector, SystemRoutes, check};

/// TCP and handshake timeout per hop during checks.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// At most this many VMs are checked at once.
pub const MAX_IN_FLIGHT: usize = 32;
/// Wait this long after a network change for it to settle before rechecking.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
pub const FAVOURITE_INTERVAL: Duration = Duration::from_mins(1);
pub const INTERVAL: Duration = Duration::from_mins(5);
/// Address events within this long of starting the watcher are its initial listing.
const WATCH_SETTLE: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_secs(10);

/// One VM's check result.
pub struct Checked {
    pub vm: Vm,
    pub status: VmStatus,
    /// The previous status, when this one differs from it.
    pub previous: Option<VmStatus>,
}

pub struct Monitor {
    db: Db,
    routes: Arc<dyn RouteInspector>,
}

impl Monitor {
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self {
            db,
            routes: Arc::new(SystemRoutes),
        }
    }

    #[must_use]
    pub fn with_routes(mut self, routes: Arc<dyn RouteInspector>) -> Self {
        self.routes = routes;
        self
    }

    /// Checks `only` (or every VM), logging in to each, and stores the results.
    pub async fn check_now(&self, only: Option<&[VmId]>) -> vmdesk_db::Result<Vec<Checked>> {
        let vms = self.db.list_vms(&ListFilter::default()).await?;
        let inventory: HashMap<VmId, Vm> = vms.iter().map(|v| (v.id, v.clone())).collect();
        let targets = vms
            .into_iter()
            .filter(|v| only.is_none_or(|ids| ids.contains(&v.id)))
            .collect();
        self.round(targets, &inventory, true).await
    }

    /// Checks every VM forever: all of them at start and after each network change,
    /// favourites every minute and the rest every five. Calls `on_change` for each VM
    /// whose status changed.
    pub async fn run(&self, on_change: impl FnMut(&Checked) + Send) -> vmdesk_db::Result<()> {
        self.watch(None, true, on_change).await
    }

    /// Like [`run`](Self::run), limited to `only` when given. With `initial` false the
    /// first round is skipped, for callers that have just run [`check_now`](Self::check_now).
    pub async fn watch(
        &self,
        only: Option<&[VmId]>,
        initial: bool,
        mut on_change: impl FnMut(&Checked) + Send,
    ) -> vmdesk_db::Result<()> {
        let mut net = watch_network();
        let mut tick = tokio::time::interval(TICK);
        let mut last: HashMap<VmId, Instant> = HashMap::new();
        if !initial {
            for vm in self.db.list_vms(&ListFilter::default()).await? {
                last.insert(vm.id, Instant::now());
            }
        }
        let mut everything = initial;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                Some(()) = net.recv() => {
                    tokio::time::sleep(DEBOUNCE).await;
                    while net.try_recv().is_ok() {}
                    tracing::info!("network changed, rechecking all VMs");
                    everything = true;
                }
            }
            let vms = self.db.list_vms(&ListFilter::default()).await?;
            let inventory: HashMap<VmId, Vm> = vms.iter().map(|v| (v.id, v.clone())).collect();
            let due: Vec<Vm> = vms
                .into_iter()
                .filter(|v| only.is_none_or(|ids| ids.contains(&v.id)))
                .filter(|v| {
                    let every = if v.favourite {
                        FAVOURITE_INTERVAL
                    } else {
                        INTERVAL
                    };
                    everything || last.get(&v.id).is_none_or(|t| t.elapsed() >= every)
                })
                .collect();
            everything = false;
            if due.is_empty() {
                continue;
            }
            for c in self.round(due, &inventory, false).await? {
                last.insert(c.vm.id, Instant::now());
                if c.previous.is_some() {
                    on_change(&c);
                }
            }
        }
    }

    async fn round(
        &self,
        vms: Vec<Vm>,
        inventory: &HashMap<VmId, Vm>,
        manual: bool,
    ) -> vmdesk_db::Result<Vec<Checked>> {
        let previous = self.db.all_status().await?;
        let transport = Transport::new(Arc::new(self.db.clone()) as Arc<dyn HostKeyStore>)
            .connect_timeout(CHECK_TIMEOUT);
        let results: Vec<(Vm, VmStatus)> = futures::stream::iter(vms)
            .map(|vm| {
                let prev = previous.get(&vm.id);
                let probe = match prev.map(|p| p.ssh) {
                    Some(SshState::AuthFailed | SshState::HostKey) if !manual => Probe::Port,
                    _ => Probe::Login,
                };
                let (transport, routes) = (&transport, self.routes.as_ref());
                async move {
                    let status = check(&vm, inventory, transport, routes, probe, prev).await;
                    (vm, status)
                }
            })
            .buffer_unordered(MAX_IN_FLIGHT)
            .collect()
            .await;
        transport.close_all().await;

        let mut checked = Vec::with_capacity(results.len());
        for (vm, status) in results {
            let previous = self.db.save_status(&status).await?;
            checked.push(Checked {
                vm,
                status,
                previous,
            });
        }
        checked.sort_by_key(|c| c.vm.name.to_lowercase());
        Ok(checked)
    }
}

/// Sends `()` whenever a local address is added or removed. If the platform watcher
/// cannot start, the channel just stays quiet and scheduled checks still run.
fn watch_network() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut watcher = match if_watch::tokio::IfWatcher::new() {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "cannot watch network changes; using the schedule only");
                return;
            }
        };
        let started = Instant::now();
        while let Some(event) = watcher.next().await {
            match event {
                Ok(e) if started.elapsed() >= WATCH_SETTLE => {
                    tracing::debug!(event = ?e, "address change");
                    if tx.try_send(()).is_err() && tx.is_closed() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "network watcher"),
            }
        }
    });
    rx
}
