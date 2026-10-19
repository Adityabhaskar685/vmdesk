//! Laptop side of the on-VM agent (spec section 10). A sync runs `vmdesk-agent stream`
//! over SSH, reads its hello, stores the events recorded since the last sync, then a
//! fresh snapshot.

use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines,
};
use tokio::time::{Instant, timeout_at};
use uuid::Uuid;
use vmdesk_core::{AgentState, Error, RemoteExec, Result, VmId};
use vmdesk_db::{AgentRecord, Db};
use vmdesk_proto::{Event, PROTO_VERSION, Snapshot, WireMsg};

/// Source name of snapshots the agent took.
pub const SOURCE: &str = "agent";
/// A sync, backlog included, gives up after this long. Events stored by then are kept,
/// so a long backlog still makes progress.
pub const SYNC_TIMEOUT: Duration = Duration::from_mins(1);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Events are stored in batches of this many as they arrive.
const BATCH: usize = 500;
/// Longest piece of unexpected agent output quoted in an error.
const QUOTE: usize = 120;

/// What one sync found. [`state`](Self::state) is what the VM's status records.
#[derive(Debug)]
pub enum AgentProbe {
    /// No agent binary on the VM.
    Absent,
    /// The agent answered, but its daemon is not running, so nothing is being recorded.
    NotRunning,
    /// The agent's history and a fresh snapshot were stored.
    Ok(Synced),
    /// As `Ok`, from an agent older than this laptop.
    Outdated(Synced),
    /// The agent did not answer properly: bad output, protocol mismatch or timeout.
    Error(String),
}

#[derive(Debug)]
pub struct Synced {
    pub snapshot: Box<Snapshot>,
    /// Events this sync stored that were not stored before.
    pub new_events: u64,
    /// Whether the agent had rotated out events before the laptop read them.
    pub gap: bool,
}

impl AgentProbe {
    #[must_use]
    pub fn state(&self) -> AgentState {
        match self {
            Self::Absent => AgentState::Absent,
            Self::NotRunning => AgentState::NotRunning,
            Self::Ok(_) => AgentState::Ok,
            Self::Outdated(_) => AgentState::Outdated,
            Self::Error(_) => AgentState::Error,
        }
    }
}

/// Syncs with the VM's agent and records the resulting agent state in its status.
/// `Err` means the session or the database failed; an agent that misbehaves is an
/// [`AgentProbe::Error`].
pub async fn sync(db: &Db, session: &dyn RemoteExec, vm: VmId) -> Result<AgentProbe> {
    let known = db.agent(vm).await.map_err(db_err)?;
    let stream = session
        .exec_io(&in_dir(r#"exec "$D/vmdesk-agent" stream"#))
        .await?;
    let probe = match exchange(db, vm, known.as_ref(), stream).await? {
        Some(probe) => probe,
        None if binary_present(session).await? => {
            AgentProbe::Error("the agent exited without a hello".into())
        }
        None => AgentProbe::Absent,
    };
    db.set_agent_state(vm, probe.state())
        .await
        .map_err(db_err)?;
    Ok(probe)
}

/// Makes sure the agent's daemon runs. Returns how: `systemd`, `nohup` or `running`.
pub async fn start(session: &dyn RemoteExec) -> Result<String> {
    run(session, &in_dir(r#""$D/vmdesk-agent" start"#)).await
}

async fn binary_present(session: &dyn RemoteExec) -> Result<bool> {
    let out = session
        .exec(&in_dir(r#"test -x "$D/vmdesk-agent""#))
        .await?;
    Ok(out.status == Some(0))
}

/// The agent's side of a sync, over any byte stream. `None` when the stream ended
/// before a hello, as it does when the binary is missing.
async fn exchange<S: AsyncRead + AsyncWrite>(
    db: &Db,
    vm: VmId,
    known: Option<&AgentRecord>,
    stream: S,
) -> Result<Option<AgentProbe>> {
    let deadline = Instant::now() + SYNC_TIMEOUT;
    let (rd, mut wr) = tokio::io::split(stream);
    let mut lines = BufReader::new(rd).lines();

    let hello = match next_msg(&mut lines, deadline.min(Instant::now() + HELLO_TIMEOUT)).await {
        Ok(None) => return Ok(None),
        Ok(Some(msg)) => Hello::from_wire(msg),
        Err(e) => Err(e),
    };
    let plan = match hello.and_then(|h| plan(vm, known, h, Utc::now())) {
        Ok(p) => p,
        Err(e) => return Ok(Some(AgentProbe::Error(e))),
    };
    db.save_agent(&plan.record, plan.gap)
        .await
        .map_err(db_err)?;
    if !plan.daemon {
        let _ = wr.shutdown().await;
        return Ok(Some(AgentProbe::NotRunning));
    }

    let since = plan.record.last_seq;
    for msg in [WireMsg::Resume { since }, WireMsg::SnapshotRequest] {
        if let Err(e) = send(&mut wr, &msg).await {
            return Ok(Some(AgentProbe::Error(e)));
        }
    }
    let agent_id = plan.record.agent_id;
    let mut batch = Vec::new();
    let mut new_events = 0;
    let read = loop {
        match next_msg(&mut lines, deadline).await {
            Ok(Some(WireMsg::Event(e))) => {
                batch.push(e);
                if batch.len() >= BATCH {
                    new_events += store(db, vm, agent_id, &mut batch).await?;
                }
            }
            Ok(Some(WireMsg::Snapshot { data, .. })) => break Ok(data),
            Ok(Some(WireMsg::Ping)) => {
                if let Err(e) = send(&mut wr, &WireMsg::Pong).await {
                    break Err(e);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break Err("the agent stream ended before the snapshot".to_owned()),
            Err(e) => break Err(e),
        }
    };
    // Whatever arrived is valid history, even if the sync did not finish.
    new_events += store(db, vm, agent_id, &mut batch).await?;
    let snapshot = match read {
        Ok(s) => s,
        Err(e) => return Ok(Some(AgentProbe::Error(e))),
    };
    db.record_snapshot(vm, SOURCE, &snapshot, &[])
        .await
        .map_err(db_err)?;
    let _ = wr.shutdown().await;

    let synced = Synced {
        snapshot,
        new_events,
        gap: plan.gap.is_some(),
    };
    Ok(Some(if older_than_laptop(&plan.record.version) {
        AgentProbe::Outdated(synced)
    } else {
        AgentProbe::Ok(synced)
    }))
}

/// The fields of the agent's hello that a sync acts on.
struct Hello {
    agent_id: Uuid,
    version: String,
    proto: u16,
    oldest_seq: u64,
    latest_seq: u64,
    daemon: bool,
}

impl Hello {
    fn from_wire(msg: WireMsg) -> Result<Self, String> {
        match msg {
            WireMsg::Hello {
                agent_id,
                version,
                proto,
                oldest_seq,
                latest_seq,
                daemon,
                ..
            } => Ok(Self {
                agent_id,
                version,
                proto,
                oldest_seq,
                latest_seq,
                daemon,
            }),
            other => Err(format!(
                "expected a hello from the agent, got {}",
                quote(&format!("{other:?}"))
            )),
        }
    }
}

/// What a hello means for the stored agent.
struct Plan {
    /// The agent row to store; its `last_seq` is where the sync resumes.
    record: AgentRecord,
    /// Span whose events the agent rotated out before the laptop read them.
    gap: Option<(DateTime<Utc>, DateTime<Utc>)>,
    daemon: bool,
}

fn plan(
    vm: VmId,
    known: Option<&AgentRecord>,
    hello: Hello,
    now: DateTime<Utc>,
) -> Result<Plan, String> {
    if hello.proto != PROTO_VERSION && hello.proto.checked_add(1) != Some(PROTO_VERSION) {
        return Err(format!(
            "the agent speaks protocol {}; this laptop speaks {PROTO_VERSION} and {}",
            hello.proto,
            PROTO_VERSION - 1
        ));
    }
    // A new agent id (a re-imaged VM, a reinstall) has its own seq space from 1.
    let same = known.filter(|k| k.agent_id == hello.agent_id);
    let stored = same.map_or(0, |k| k.last_seq);
    if hello.latest_seq < stored {
        return Err(format!(
            "the agent's event log restarted (latest seq {}, {stored} already stored); \
            reinstall the agent",
            hello.latest_seq
        ));
    }
    let gap = same
        .filter(|_| hello.oldest_seq > stored.saturating_add(1))
        .map(|k| (k.last_contact_at.or(k.installed_at).unwrap_or(now), now));
    Ok(Plan {
        record: AgentRecord {
            vm,
            agent_id: hello.agent_id,
            version: hello.version,
            proto: hello.proto,
            installed_at: known.and_then(|k| k.installed_at),
            last_seq: if gap.is_some() {
                hello.oldest_seq - 1
            } else {
                stored
            },
            last_contact_at: Some(now),
        },
        gap,
        daemon: hello.daemon,
    })
}

async fn store(db: &Db, vm: VmId, agent_id: Uuid, batch: &mut Vec<Event>) -> Result<u64> {
    if batch.is_empty() {
        return Ok(0);
    }
    let added = db
        .insert_agent_events(vm, agent_id, batch)
        .await
        .map_err(db_err)?;
    batch.clear();
    Ok(added)
}

/// The next message, `None` at the end of the stream; `Err` describes what went wrong.
async fn next_msg<R: AsyncBufRead + Unpin>(
    lines: &mut Lines<R>,
    deadline: Instant,
) -> Result<Option<WireMsg>, String> {
    match timeout_at(deadline, lines.next_line()).await {
        Err(_) => Err("timed out waiting for the agent".to_owned()),
        Ok(Err(e)) => Err(format!("reading from the agent: {e}")),
        Ok(Ok(None)) => Ok(None),
        Ok(Ok(Some(line))) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| format!("unexpected output from the agent ({e}): {:?}", quote(&line))),
    }
}

async fn send<W: AsyncWrite + Unpin>(wr: &mut W, msg: &WireMsg) -> Result<(), String> {
    let mut line = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    line.push(b'\n');
    let sent = async {
        wr.write_all(&line).await?;
        wr.flush().await
    };
    sent.await.map_err(|e| format!("writing to the agent: {e}"))
}

fn quote(text: &str) -> String {
    text.chars().take(QUOTE).collect()
}

/// Whether `version` (dotted numbers) is older than this laptop's.
fn older_than_laptop(version: &str) -> bool {
    numbers(version) < numbers(env!("CARGO_PKG_VERSION"))
}

/// `0.1.0-rc1` -> `[0, 1]`: each part's leading digits, trailing zeros dropped.
fn numbers(version: &str) -> Vec<u64> {
    let mut parts: Vec<u64> = version
        .split('.')
        .map(|p| {
            let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect();
    while parts.last() == Some(&0) {
        parts.pop();
    }
    parts
}

/// `script` under `/bin/sh` whatever the login shell is, with `$D` set to the agent's
/// directory on the VM.
pub(crate) fn in_dir(script: &str) -> String {
    let script = format!(r#"D="$HOME/.local/share/vmdesk/agent"; {script}"#);
    format!("/bin/sh -c '{}'", script.replace('\'', r"'\''"))
}

/// Runs `command`, returning its trimmed stdout; a non-zero exit is an error with the
/// first line of stderr.
pub(crate) async fn run(session: &dyn RemoteExec, command: &str) -> Result<String> {
    let out = session.exec(command).await?;
    if out.status != Some(0) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Other(format!(
            "remote command failed (exit {}): {}",
            out.status
                .map_or_else(|| "none".to_owned(), |s| s.to_string()),
            stderr.lines().next().unwrap_or_default()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[allow(clippy::needless_pass_by_value)] // used as `map_err(db_err)`
pub(crate) fn db_err(e: vmdesk_db::Error) -> Error {
    Error::Other(format!("database: {e}"))
}

#[cfg(test)]
mod tests {
    use vmdesk_db::agent_source;

    use super::*;
    use crate::fake_vm::{FakeAgent, FakeVm, Output, agent_state, db_with_vm};

    async fn seqs(db: &Db, vm: VmId, source: &str) -> Vec<i64> {
        let mut seqs: Vec<i64> = db
            .timeline(vm, 1000, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.source == source)
            .map(|e| e.seq)
            .collect();
        seqs.sort_unstable();
        seqs
    }

    fn synced(probe: AgentProbe) -> Synced {
        match probe {
            AgentProbe::Ok(s) => s,
            other => panic!("expected a sync, got {other:?}"),
        }
    }

    fn id_of(fake: &FakeVm) -> Uuid {
        fake.agent().as_ref().unwrap().agent_id
    }

    #[tokio::test]
    async fn first_sync_then_resume_from_last_seq() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=3)));
        let source = agent_source(id_of(&fake));

        let first = synced(sync(&db, &fake, vm.id).await.unwrap());
        assert_eq!((first.new_events, first.gap), (3, false));
        assert_eq!(first.snapshot.system.hostname, "agent-host");
        assert_eq!(
            fake.received()[..2],
            [WireMsg::Resume { since: 0 }, WireMsg::SnapshotRequest]
        );
        let stored = db.agent(vm.id).await.unwrap().unwrap();
        assert_eq!((stored.agent_id, stored.last_seq), (id_of(&fake), 3));
        assert!(stored.last_contact_at.is_some());
        assert_eq!(seqs(&db, vm.id, &source).await, [1, 2, 3]);
        let snapshot_source: String = sqlx::query_scalar("SELECT source FROM snapshots")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(snapshot_source, "agent");
        assert_eq!(
            db.latest_snapshot(vm.id)
                .await
                .unwrap()
                .unwrap()
                .system
                .hostname,
            "agent-host"
        );
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Ok);

        fake.agent().as_mut().unwrap().record(2);
        let second = synced(sync(&db, &fake, vm.id).await.unwrap());
        assert_eq!(second.new_events, 2);
        assert_eq!(fake.received()[0], WireMsg::Resume { since: 3 });
        assert_eq!(db.agent(vm.id).await.unwrap().unwrap().last_seq, 5);
        assert_eq!(seqs(&db, vm.id, &source).await, [1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn the_same_events_twice_are_stored_once() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=4)));
        sync(&db, &fake, vm.id).await.unwrap();
        // As if the cursor had not moved: the agent sends everything again.
        let mut record = db.agent(vm.id).await.unwrap().unwrap();
        record.last_seq = 0;
        db.save_agent(&record, None).await.unwrap();
        fake.received();

        let again = synced(sync(&db, &fake, vm.id).await.unwrap());
        assert_eq!(fake.received()[0], WireMsg::Resume { since: 0 });
        assert_eq!(again.new_events, 0);
        assert_eq!(
            seqs(&db, vm.id, &agent_source(id_of(&fake))).await,
            [1, 2, 3, 4]
        );
        assert_eq!(db.agent(vm.id).await.unwrap().unwrap().last_seq, 4);
    }

    #[tokio::test]
    async fn rotated_out_events_leave_one_gap() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=3)));
        sync(&db, &fake, vm.id).await.unwrap();
        let contact = db
            .agent(vm.id)
            .await
            .unwrap()
            .unwrap()
            .last_contact_at
            .unwrap();
        // While the laptop was away the agent recorded 4..=12 and rotated out 1..=9.
        {
            let mut agent = fake.agent();
            let agent = agent.as_mut().unwrap();
            agent.record(9);
            agent.events.retain(|e| e.seq >= 10);
        }
        fake.received();

        let s = synced(sync(&db, &fake, vm.id).await.unwrap());
        assert_eq!((s.new_events, s.gap), (3, true));
        assert_eq!(fake.received()[0], WireMsg::Resume { since: 9 });
        let gaps = || async {
            db.timeline(vm.id, 100, None)
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "gap")
                .collect::<Vec<_>>()
        };
        let found = gaps().await;
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].source.as_str(), found[0].ts), ("laptop", contact));
        assert_eq!(db.agent(vm.id).await.unwrap().unwrap().last_seq, 12);

        let s = synced(sync(&db, &fake, vm.id).await.unwrap());
        assert!(!s.gap);
        assert_eq!(gaps().await.len(), 1, "a caught-up sync adds no gap");
    }

    #[tokio::test]
    async fn a_new_agent_id_starts_a_fresh_sequence() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(Some(FakeAgent::recording(1..=50)));
        let old = id_of(&fake);
        sync(&db, &fake, vm.id).await.unwrap();

        // Re-imaged VM: a new agent whose seqs start again at 1.
        *fake.agent() = Some(FakeAgent::recording(1..=2));
        let new = id_of(&fake);
        fake.received();
        let s = synced(sync(&db, &fake, vm.id).await.unwrap());
        assert_eq!((s.new_events, s.gap), (2, false));
        assert_eq!(fake.received()[0], WireMsg::Resume { since: 0 });
        let record = db.agent(vm.id).await.unwrap().unwrap();
        assert_eq!((record.agent_id, record.last_seq), (new, 2));
        assert_eq!(seqs(&db, vm.id, &agent_source(new)).await, [1, 2]);
        assert_eq!(seqs(&db, vm.id, &agent_source(old)).await.len(), 50);
    }

    #[tokio::test]
    async fn a_stopped_daemon_is_reported_without_reading_history() {
        let (db, vm) = db_with_vm().await;
        let mut agent = FakeAgent::recording(1..=3);
        agent.daemon = false;
        let fake = FakeVm::new(Some(agent));

        let probe = sync(&db, &fake, vm.id).await.unwrap();
        assert!(matches!(probe, AgentProbe::NotRunning), "{probe:?}");
        assert!(fake.received().is_empty());
        assert_eq!(agent_state(&db, vm.id).await, AgentState::NotRunning);
        let record = db.agent(vm.id).await.unwrap().unwrap();
        assert_eq!((record.agent_id, record.last_seq), (id_of(&fake), 0));
        assert!(db.timeline(vm.id, 10, None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepts_this_protocol_and_the_previous_one() {
        let (db, vm) = db_with_vm().await;
        let mut agent = FakeAgent::recording(1..=1);
        agent.proto = PROTO_VERSION + 1;
        let fake = FakeVm::new(Some(agent));

        let AgentProbe::Error(e) = sync(&db, &fake, vm.id).await.unwrap() else {
            panic!("a newer protocol is an error");
        };
        assert!(e.contains("protocol 2"), "{e}");
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Error);
        assert!(db.agent(vm.id).await.unwrap().is_none());

        fake.agent().as_mut().unwrap().proto = PROTO_VERSION - 1;
        synced(sync(&db, &fake, vm.id).await.unwrap());
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Ok);
    }

    #[tokio::test]
    async fn missing_binary_and_unexpected_output() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(None);
        let probe = sync(&db, &fake, vm.id).await.unwrap();
        assert!(matches!(probe, AgentProbe::Absent), "{probe:?}");
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Absent);

        let mut garbled = FakeAgent::recording([]);
        garbled.output = Output::Garbage;
        *fake.agent() = Some(garbled);
        let AgentProbe::Error(e) = sync(&db, &fake, vm.id).await.unwrap() else {
            panic!("expected an error");
        };
        assert!(e.contains("Welcome to Ubuntu"), "{e}");
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Error);
    }

    #[tokio::test]
    async fn a_silent_agent_times_out() {
        let (db, vm) = db_with_vm().await;
        let (laptop, _silent_agent) = tokio::io::duplex(1024);
        // Paused only now: the database's own timeouts must not run on the fake clock.
        tokio::time::pause();
        let started = Instant::now();
        let probe = exchange(&db, vm.id, None, laptop).await.unwrap();
        let Some(AgentProbe::Error(e)) = probe else {
            panic!("expected a timeout, got {probe:?}");
        };
        assert!(e.contains("timed out"), "{e}");
        let waited = started.elapsed();
        assert!(
            waited >= HELLO_TIMEOUT && waited < SYNC_TIMEOUT,
            "the hello limit applies: {waited:?}"
        );
    }

    #[tokio::test]
    async fn older_agents_are_outdated_but_still_collect() {
        let (db, vm) = db_with_vm().await;
        let mut old = FakeAgent::recording(1..=2);
        old.version = "0.0.9".into();
        let fake = FakeVm::new(Some(old));
        let probe = sync(&db, &fake, vm.id).await.unwrap();
        let AgentProbe::Outdated(s) = probe else {
            panic!("expected outdated, got {probe:?}");
        };
        assert_eq!(s.new_events, 2);
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Outdated);

        assert!(!older_than_laptop(env!("CARGO_PKG_VERSION")));
        assert!(!older_than_laptop(&format!(
            "{}-rc1",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(!older_than_laptop("999.0"));
        assert!(older_than_laptop("0.0.0"));
        assert_eq!(numbers("1.20.0"), [1, 20]);
        assert!(numbers("0.10") > numbers("0.9.7"));
    }
}
