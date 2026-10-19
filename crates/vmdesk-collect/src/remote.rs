//! Laptop side: runs the probe over SSH (or syncs the VM's agent), stores the snapshot,
//! and turns the change since the previous one into timeline events. Also the
//! background poller.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use vmdesk_core::{Collector, Error, HostKeyStore, RemoteExec, Result, SshState, Vm, VmId};
use vmdesk_db::{Db, ListFilter};
use vmdesk_proto::{Event, Snapshot};
use vmdesk_transport::Transport;

use crate::agent::{self, AgentProbe};
use crate::{diff, parse, probe_command};

/// Event source name for snapshots taken over SSH.
pub const SOURCE: &str = "ssh";
/// Background poll interval (spec section 9; 2 min while a VM is open in the app, M5).
pub const POLL_INTERVAL: Duration = Duration::from_mins(15);
/// At most this many VMs are collected at once.
const MAX_IN_FLIGHT: usize = 8;
const TICK: Duration = Duration::from_mins(1);

pub struct SshCollector;

#[async_trait]
impl Collector for SshCollector {
    async fn snapshot(&self, session: &dyn RemoteExec) -> Result<Snapshot> {
        let taken_at = Utc::now();
        let out = session.exec(&probe_command()).await?;
        let text = String::from_utf8_lossy(&out.stdout);
        if !text.contains(crate::probe::MARKER) {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Other(format!(
                "probe produced no output (exit {:?}): {}",
                out.status,
                err.lines().next().unwrap_or_default()
            )));
        }
        Ok(parse(&text, taken_at))
    }

    async fn events_since(&self, _session: &dyn RemoteExec, _cursor: u64) -> Result<Vec<Event>> {
        Err(Error::Other(
            "SSH collector derives events by diffing snapshots".into(),
        ))
    }
}

/// Collects a snapshot of `vm` now, stores it with the events since the previous one,
/// and returns it.
#[allow(clippy::implicit_hasher)] // matches `Transport::session`
pub async fn collect(
    db: &Db,
    transport: &Transport,
    vm: &Vm,
    inventory: &HashMap<VmId, Vm>,
) -> Result<Snapshot> {
    let session = transport.session(vm, inventory).await?;
    collect_from(db, session.as_ref(), vm).await
}

/// [`collect`] over an established session. A VM with an agent is synced from it
/// while its daemon records; otherwise (and for VMs without one) the SSH probe runs.
pub async fn collect_from(db: &Db, session: &dyn RemoteExec, vm: &Vm) -> Result<Snapshot> {
    // A failed sync says nothing about whether the daemon still records, so the SSH
    // snapshot then derives no events: the agent's log will cover that span.
    let mut derive_events = true;
    if db.agent(vm.id).await.map_err(|e| store_err(&e))?.is_some() {
        match agent::sync(db, session, vm.id).await? {
            AgentProbe::Ok(synced) | AgentProbe::Outdated(synced) => return Ok(*synced.snapshot),
            // The agent's log owns every span after its last snapshot: once started, its
            // daemon diffs from there, so nothing here may derive events for that span.
            AgentProbe::NotRunning(synced) => {
                match agent::start(session).await {
                    Ok(how) => tracing::info!(vm = %vm.name, how, "started the agent's daemon"),
                    Err(e) => {
                        tracing::warn!(vm = %vm.name, error = %e, "starting the agent's daemon");
                    }
                }
                return Ok(*synced.snapshot);
            }
            AgentProbe::Absent => {
                tracing::warn!(vm = %vm.name, "the agent binary is gone; collecting over SSH");
            }
            AgentProbe::Error(e) => {
                tracing::warn!(vm = %vm.name, error = %e, "agent sync failed; collecting over SSH");
                derive_events = false;
            }
        }
    }
    let snapshot = SshCollector.snapshot(session).await?;
    let previous = db.latest_snapshot(vm.id).await.map_err(|e| store_err(&e))?;
    let events = previous
        .filter(|_| derive_events)
        .map(|p| diff(&p, &snapshot))
        .unwrap_or_default();
    db.record_snapshot(vm.id, SOURCE, &snapshot, &events)
        .await
        .map_err(|e| store_err(&e))?;
    tracing::debug!(vm = %vm.name, events = events.len(), "snapshot stored");
    Ok(snapshot)
}

fn store_err(e: &vmdesk_db::Error) -> Error {
    Error::Other(format!("storing snapshot: {e}"))
}

/// Collects from every VM whose last reachability check succeeded, every
/// [`POLL_INTERVAL`]. VMs that are not reachable are left to the reachability monitor,
/// so the poller never adds failing logins of its own.
pub async fn poll_forever(db: &Db) -> vmdesk_db::Result<()> {
    let mut tick = tokio::time::interval(TICK);
    let mut last: HashMap<VmId, std::time::Instant> = HashMap::new();
    loop {
        tick.tick().await;
        let (vms, status) = match (
            db.list_vms(&ListFilter::default()).await,
            db.all_status().await,
        ) {
            (Ok(v), Ok(s)) => (v, s),
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(error = %e, "reading the inventory; retrying next tick");
                continue;
            }
        };
        let inventory: HashMap<VmId, Vm> = vms.iter().map(|v| (v.id, v.clone())).collect();
        let due: Vec<Vm> = vms
            .iter()
            .filter(|v| status.get(&v.id).is_some_and(|s| s.ssh == SshState::Ok))
            .filter(|v| last.get(&v.id).is_none_or(|t| t.elapsed() >= POLL_INTERVAL))
            .cloned()
            .collect();
        if due.is_empty() {
            continue;
        }
        let transport = Transport::new(Arc::new(db.clone()) as Arc<dyn HostKeyStore>);
        let results: Vec<(VmId, String, Result<Snapshot>)> = futures::stream::iter(due)
            .map(|vm| {
                let (transport, inventory) = (&transport, &inventory);
                async move {
                    (
                        vm.id,
                        vm.name.clone(),
                        collect(db, transport, &vm, inventory).await,
                    )
                }
            })
            .buffer_unordered(MAX_IN_FLIGHT)
            .collect()
            .await;
        transport.close_all().await;
        for (id, name, res) in results {
            last.insert(id, std::time::Instant::now());
            if let Err(e) = res {
                tracing::warn!(vm = %name, error = %e, "collection failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use vmdesk_core::AgentState;

    use super::*;
    use crate::fake_vm::{FakeAgent, FakeVm, agent_state, db_with_vm};
    use crate::probe::MARKER;

    #[tokio::test]
    async fn vms_without_an_agent_row_only_run_the_probe() {
        let (db, vm) = db_with_vm().await;
        // A binary is there, but the laptop has not installed or adopted it.
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=2)));
        let snap = collect_from(&db, &fake, &vm).await.unwrap();
        assert_eq!(snap.system.hostname, "ssh-host");
        let commands = fake.commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(commands[0].contains(MARKER));
        assert!(fake.received().is_empty());
    }

    #[tokio::test]
    async fn a_recording_agent_replaces_the_probe() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=2)));
        agent::sync(&db, &fake, vm.id).await.unwrap();
        fake.agent().as_mut().unwrap().record(1);

        let snap = collect_from(&db, &fake, &vm).await.unwrap();
        assert_eq!(snap.system.hostname, "agent-host");
        assert!(!fake.ran(MARKER));
        assert_eq!(db.agent(vm.id).await.unwrap().unwrap().last_seq, 3);
    }

    #[tokio::test]
    async fn a_stopped_daemon_is_started_and_nothing_else_records_its_span() {
        let (db, vm) = db_with_vm().await;
        let mut stopped = FakeAgent::recording(1..=2);
        stopped.daemon = false;
        let fake = FakeVm::new(Some(stopped));
        agent::sync(&db, &fake, vm.id).await.unwrap();

        let snap = collect_from(&db, &fake, &vm).await.unwrap();
        assert_eq!(snap.system.hostname, "agent-host");
        assert!(fake.ran("\" start"));
        assert!(
            !fake.ran(MARKER),
            "no SSH probe, so no events derived beside the agent's"
        );
        assert_eq!(agent_state(&db, vm.id).await, AgentState::NotRunning);

        let snap = collect_from(&db, &fake, &vm).await.unwrap();
        assert_eq!(
            snap.system.hostname, "agent-host",
            "next poll uses the agent"
        );
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Ok);
    }

    /// Collects over SSH after a sync that found the agent `broken` (or gone), with a
    /// stored snapshot that the SSH probe's differs from, and returns the SSH events.
    async fn ssh_events_after_fallback(broken: bool) -> usize {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=2)));
        agent::sync(&db, &fake, vm.id).await.unwrap();
        let mut earlier = Snapshot::default();
        earlier.tmux.push(vmdesk_proto::TmuxSession {
            name: "work".into(),
            windows: 1,
            attached: false,
            created: 0,
            pane_cwd: None,
            pane_command: None,
        });
        db.record_snapshot(vm.id, agent::SOURCE, &earlier, &[])
            .await
            .unwrap();
        if broken {
            fake.agent().as_mut().unwrap().output = crate::fake_vm::Output::Garbage;
        } else {
            *fake.agent() = None;
        }
        collect_from(&db, &fake, &vm).await.unwrap();
        db.timeline(vm.id, 100, None)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.source == SOURCE)
            .count()
    }

    #[tokio::test]
    async fn a_failed_sync_leaves_the_span_to_the_agent_log() {
        assert!(
            ssh_events_after_fallback(false).await > 0,
            "a gone agent: SSH diffs"
        );
        assert_eq!(
            ssh_events_after_fallback(true).await,
            0,
            "a failed sync: no SSH events"
        );
    }

    #[tokio::test]
    async fn a_vanished_binary_falls_back_to_the_probe() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=2)));
        agent::sync(&db, &fake, vm.id).await.unwrap();
        *fake.agent() = None;

        let snap = collect_from(&db, &fake, &vm).await.unwrap();
        assert_eq!(snap.system.hostname, "ssh-host");
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Absent);
    }
}
