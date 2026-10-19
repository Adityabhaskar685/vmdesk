//! Types shared by the laptop and `vmdesk-agent`: snapshot, events and the
//! NDJSON wire protocol spoken over SSH stdio.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire protocol version. The laptop support this one and the previous one.
pub const PROTO_VERSION: u16 = 1;
/// Snapshot schema version, bumped on breaking changes to [`Snapshot`].
pub const SNAPSHOT_SCHEMA: u16 = 1;

/// Everything collected from a VM at one moment. Sections whose command is missing or
/// fails are listed in `unavailable` instead of failing the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub schema_version: u16,
    pub taken_at: Option<DateTime<Utc>>,
    pub system: SystemInfo,
    pub processes: Vec<ProcessInfo>,
    pub containers: Vec<Container>,
    pub tmux: Vec<TmuxSession>,
    pub listeners: Vec<Listener>,
    pub repos: Vec<RepoInfo>,
    /// Section name and why it could not be collected (e.g. `docker: not installed`).
    pub unavailable: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemInfo {
    pub hostname: String,
    /// e.g. `Ubuntu 24.04 LTS`.
    pub os: String,
    pub kernel: String,
    pub uptime_secs: u64,
    /// Unix seconds; changes when the VM reboots.
    pub boot_time: i64,
    pub load: [f32; 3],
    pub cpus: u32,
    pub mem_total_kb: u64,
    pub mem_used_kb: u64,
    pub disks: Vec<Disk>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub mount: String,
    pub total_kb: u64,
    pub used_kb: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub user: String,
    /// Redacted command line.
    pub cmdline: String,
    pub cwd: Option<String>,
    /// Unix seconds.
    pub started_at: i64,
    pub cpu_pct: f32,
    pub rss_kb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    /// `running`, `exited`, `restarting`, ...
    pub state: String,
    /// Human-readable, e.g. `Up 45 hours`.
    pub status: String,
    pub ports: String,
    pub compose_project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxSession {
    pub name: String,
    pub windows: u32,
    pub attached: bool,
    /// Unix seconds.
    pub created: i64,
    /// Working directory and command of the active pane.
    pub pane_cwd: Option<String>,
    pub pane_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    pub proto: String,
    pub addr: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub path: String,
    pub branch: Option<String>,
    /// Files with uncommitted changes.
    pub dirty: u32,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub last_commit: Option<Commit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    /// Unix seconds.
    pub time: i64,
    pub subject: String,
}

/// One entry in a VM's timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Event types (spec section 11). The tag is stored as the event's `kind` column and the
/// content as its `data_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    ProcessStarted {
        pid: u32,
        cmdline: String,
        cwd: Option<String>,
    },
    ProcessExited {
        pid: u32,
        cmdline: String,
        exit_code: Option<i32>,
        /// Seconds it ran for, as far as observed.
        duration_secs: Option<u64>,
    },
    ContainerStarted {
        name: String,
        image: String,
        compose_project: Option<String>,
    },
    ContainerStopped {
        name: String,
        image: String,
    },
    TmuxCreated {
        name: String,
        pane_cwd: Option<String>,
    },
    TmuxClosed {
        name: String,
    },
    TmuxAttached {
        name: String,
    },
    GitBranchChanged {
        repo: String,
        from: Option<String>,
        to: Option<String>,
    },
    GitCommitted {
        repo: String,
        branch: Option<String>,
        sha: String,
        subject: String,
    },
    GitDirtyChanged {
        repo: String,
        from: u32,
        to: u32,
    },
    ListenerOpened {
        proto: String,
        addr: String,
        port: u16,
        process: Option<String>,
    },
    ListenerClosed {
        proto: String,
        addr: String,
        port: u16,
    },
    VmRebooted {
        previous_boot: i64,
    },
    /// The VM was not observed between `from` and `to`.
    Gap {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
}

impl EventKind {
    /// The serde tag, e.g. `process_started`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::ProcessStarted { .. } => "process_started",
            Self::ProcessExited { .. } => "process_exited",
            Self::ContainerStarted { .. } => "container_started",
            Self::ContainerStopped { .. } => "container_stopped",
            Self::TmuxCreated { .. } => "tmux_created",
            Self::TmuxClosed { .. } => "tmux_closed",
            Self::TmuxAttached { .. } => "tmux_attached",
            Self::GitBranchChanged { .. } => "git_branch_changed",
            Self::GitCommitted { .. } => "git_committed",
            Self::GitDirtyChanged { .. } => "git_dirty_changed",
            Self::ListenerOpened { .. } => "listener_opened",
            Self::ListenerClosed { .. } => "listener_closed",
            Self::VmRebooted { .. } => "vm_rebooted",
            Self::Gap { .. } => "gap",
        }
    }
}

/// Messages on the agent stream (one JSON per line).
///
/// `vmdesk-agent stream` writes `Hello` first, then answers control messages in the
/// order they arrive: `Resume` sends every stored event after `since`, then keeps
/// sending new ones as the daemon records them; `SnapshotRequest` sends a fresh
/// `Snapshot` after any pending events, so a laptop that sends both and reads up to the
/// `Snapshot` has caught up. The stream ends when stdin closes.
///
/// Event `seq`s start at 1, only grow, and are never reused, even after rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WireMsg {
    Hello {
        agent_id: Uuid,
        version: String,
        proto: u16,
        caps: Vec<String>,
        /// Smallest stored seq; `latest_seq + 1` when the log is empty.
        oldest_seq: u64,
        /// Largest seq ever assigned; 0 before the first event.
        latest_seq: u64,
        /// Whether the collector daemon is running. When it is not, history is not
        /// being recorded and the laptop falls back to the SSH collector.
        #[serde(default)]
        daemon: bool,
    },
    Resume {
        since: u64,
    },
    SnapshotRequest,
    Event(Event),
    Snapshot {
        seq: u64,
        data: Box<Snapshot>,
    },
    Ping,
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips_as_ndjson() {
        let msg = WireMsg::Hello {
            agent_id: Uuid::nil(),
            version: "0.1.0".to_string(),
            proto: PROTO_VERSION,
            caps: vec!["docker".into()],
            oldest_seq: 1,
            latest_seq: 9,
            daemon: true,
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.starts_with(r#"{"t":"hello""#));
        assert_eq!(serde_json::from_str::<WireMsg>(&line).unwrap(), msg);
    }

    #[test]
    fn event_name_matches_serde_tag() {
        let kinds = [
            EventKind::TmuxClosed { name: "a".into() },
            EventKind::VmRebooted { previous_boot: 1 },
            EventKind::GitDirtyChanged {
                repo: "r".into(),
                from: 0,
                to: 2,
            },
        ];
        for k in kinds {
            let json = serde_json::to_value(&k).unwrap();
            assert_eq!(json["kind"], k.name());
        }
    }
}
