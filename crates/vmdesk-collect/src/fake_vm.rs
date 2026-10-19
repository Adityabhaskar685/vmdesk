//! A VM for tests: answers the commands the laptop runs, with a fake agent speaking the
//! wire protocol over `tokio::io::duplex` behind `stream`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, WriteHalf};
use uuid::Uuid;
use vmdesk_core::{ByteStream, Error, ExecOutput, RemoteExec, Result, Vm, VmId};
use vmdesk_db::Db;
use vmdesk_proto::{Event, EventKind, PROTO_VERSION, Snapshot, SystemInfo, WireMsg};

use crate::probe::MARKER;

/// What the agent binary does when started with `stream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Protocol,
    /// Prints something that is not the protocol and exits.
    Garbage,
}

#[derive(Debug, Clone)]
pub struct FakeAgent {
    pub agent_id: Uuid,
    pub version: String,
    pub proto: u16,
    pub daemon: bool,
    /// The VM's stored event log, oldest first.
    pub events: Vec<Event>,
    /// Largest seq ever assigned; past the log's end after rotation.
    pub latest_seq: u64,
    pub output: Output,
}

impl FakeAgent {
    pub fn recording(seqs: impl IntoIterator<Item = u64>) -> Self {
        let events: Vec<Event> = seqs.into_iter().map(event).collect();
        Self {
            agent_id: Uuid::now_v7(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            proto: PROTO_VERSION,
            daemon: true,
            latest_seq: events.last().map_or(0, |e| e.seq),
            events,
            output: Output::Protocol,
        }
    }

    /// Appends events with the next seqs, as the daemon would.
    pub fn record(&mut self, n: u64) {
        for _ in 0..n {
            self.latest_seq += 1;
            self.events.push(event(self.latest_seq));
        }
    }
}

pub fn event(seq: u64) -> Event {
    Event {
        seq,
        ts: DateTime::from_timestamp(1_700_000_000 + i64::try_from(seq).unwrap(), 0).unwrap(),
        kind: EventKind::TmuxCreated {
            name: format!("s{seq}"),
            pane_cwd: None,
        },
    }
}

pub struct FakeVm {
    pub arch: &'static str,
    /// The installed agent; `None` when there is no binary.
    pub agent: Mutex<Option<FakeAgent>>,
    /// Every command run with `exec`, in order.
    pub commands: Mutex<Vec<String>>,
    /// What the laptop sent the agent, in order.
    pub received: Arc<Mutex<Vec<WireMsg>>>,
    /// The last upload, as written to `vmdesk-agent.new`.
    pub uploaded: Arc<Mutex<Option<Vec<u8>>>>,
    /// Flips a byte of each upload on the way.
    pub damage_uploads: bool,
}

impl FakeVm {
    pub fn new(agent: Option<FakeAgent>) -> Self {
        Self {
            arch: "x86_64",
            agent: Mutex::new(agent),
            commands: Mutex::default(),
            received: Arc::default(),
            uploaded: Arc::default(),
            damage_uploads: false,
        }
    }

    pub fn agent(&self) -> std::sync::MutexGuard<'_, Option<FakeAgent>> {
        self.agent.lock().unwrap()
    }

    pub fn ran(&self, needle: &str) -> bool {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains(needle))
    }

    /// What the laptop sent the agent since the last call.
    pub fn received(&self) -> Vec<WireMsg> {
        std::mem::take(&mut *self.received.lock().unwrap())
    }

    fn sha256_of_upload(&self) -> ExecOutput {
        match self.uploaded.lock().unwrap().as_deref() {
            Some(b) => ok(format!(
                "{}  /home/u/.local/share/vmdesk/agent/vmdesk-agent.new\n",
                hex::encode(Sha256::digest(b))
            )),
            None => failed(1, "sha256sum: vmdesk-agent.new: No such file or directory"),
        }
    }
}

fn ok(stdout: impl Into<String>) -> ExecOutput {
    ExecOutput {
        status: Some(0),
        stdout: stdout.into().into_bytes(),
        stderr: vec![],
    }
}

fn failed(status: u32, stderr: &str) -> ExecOutput {
    ExecOutput {
        status: Some(status),
        stdout: vec![],
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[async_trait]
impl RemoteExec for FakeVm {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_owned());
        let has = |s: &str| command.contains(s);
        let mut agent = self.agent();
        Ok(match agent.as_mut() {
            _ if command == "uname -m" => ok(format!("{}\n", self.arch)),
            _ if has(MARKER) => ok(format!("{MARKER} system\n0\nu\nssh-host\n")),
            _ if has("sha256sum") => self.sha256_of_upload(),
            _ if has("rm -f") => ok(""),
            _ if has("mv -f") => {
                let mut placed = agent.take().unwrap_or_else(|| FakeAgent::recording([]));
                placed.daemon = false;
                *agent = Some(placed);
                ok("")
            }
            _ if has("uninstall") => {
                *agent = None;
                ok("uninstalled\n")
            }
            Some(_) if has("test -x") => ok(""),
            Some(a) if has("\" install") || has("\" start") => {
                a.daemon = true;
                ok("systemd\n")
            }
            None if has("test -x") => failed(1, ""),
            _ => failed(127, "sh: 1: vmdesk-agent: not found"),
        })
    }

    async fn exec_io(&self, command: &str) -> Result<Box<dyn ByteStream>> {
        let (laptop, vm) = tokio::io::duplex(64 * 1024);
        if command.contains("cat >") {
            let (uploaded, damage) = (Arc::clone(&self.uploaded), self.damage_uploads);
            tokio::spawn(async move {
                let mut vm = vm;
                let mut bytes = Vec::new();
                vm.read_to_end(&mut bytes).await.unwrap();
                if damage && let Some(b) = bytes.first_mut() {
                    *b ^= 0xff;
                }
                *uploaded.lock().unwrap() = Some(bytes);
            });
        } else if command.contains("stream") {
            if let Some(agent) = self.agent().clone() {
                tokio::spawn(serve(agent, vm, Arc::clone(&self.received)));
            }
        } else {
            return Err(Error::Other(format!("fake VM cannot stream {command:?}")));
        }
        Ok(Box::new(laptop))
    }
}

async fn serve(agent: FakeAgent, io: DuplexStream, received: Arc<Mutex<Vec<WireMsg>>>) {
    let (rd, mut wr) = tokio::io::split(io);
    let mut lines = BufReader::new(rd).lines();
    match agent.output {
        Output::Garbage => {
            let _ = wr.write_all(b"Welcome to Ubuntu 24.04 LTS\n").await;
            return;
        }
        Output::Protocol => {}
    }
    let hello = WireMsg::Hello {
        agent_id: agent.agent_id,
        version: agent.version.clone(),
        proto: agent.proto,
        caps: vec!["tmux".into()],
        oldest_seq: agent.events.first().map_or(agent.latest_seq + 1, |e| e.seq),
        latest_seq: agent.latest_seq,
        daemon: agent.daemon,
    };
    write(&mut wr, &hello).await;
    while let Ok(Some(line)) = lines.next_line().await {
        let msg: WireMsg = serde_json::from_str(&line).unwrap();
        received.lock().unwrap().push(msg.clone());
        match msg {
            WireMsg::Resume { since } => {
                write(&mut wr, &WireMsg::Ping).await;
                for e in agent.events.iter().filter(|e| e.seq > since) {
                    write(&mut wr, &WireMsg::Event(e.clone())).await;
                }
            }
            WireMsg::SnapshotRequest => {
                let data = Snapshot {
                    taken_at: Some(Utc::now()),
                    system: SystemInfo {
                        hostname: "agent-host".into(),
                        ..SystemInfo::default()
                    },
                    ..Snapshot::default()
                };
                let snapshot = WireMsg::Snapshot {
                    seq: agent.latest_seq,
                    data: Box::new(data),
                };
                write(&mut wr, &snapshot).await;
            }
            _ => {}
        }
    }
}

async fn write(wr: &mut WriteHalf<DuplexStream>, msg: &WireMsg) {
    let mut line = serde_json::to_vec(msg).unwrap();
    line.push(b'\n');
    let _ = wr.write_all(&line).await;
}

/// An in-memory database holding one VM.
pub async fn db_with_vm() -> (Db, Vm) {
    let db = Db::open_in_memory().await.unwrap();
    let vm = db
        .create_vm(vmdesk_db::NewVm {
            name: "box".into(),
            username: "u".into(),
            credential: vmdesk_core::CredentialRef::SshAgent,
            endpoints: vec![vmdesk_db::EndpointInput {
                address: "10.0.0.1".into(),
                port: 22,
                kind: vmdesk_core::EndpointKind::Private,
                bind_interface: None,
            }],
            paths: None,
            tags: vec![],
            favourite: false,
        })
        .await
        .unwrap();
    (db, vm)
}

/// The VM's stored agent state.
pub async fn agent_state(db: &Db, vm: VmId) -> vmdesk_core::AgentState {
    db.get_status(vm).await.unwrap().unwrap().agent
}
