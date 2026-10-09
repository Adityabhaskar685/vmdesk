//! Types shared by the laptop and `vmdesk-agent`: snapshot, events and the
//! NDJSON wire protocol spoken over SSH stdio.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire protocol version. The laptop support this one and the previous one.
pub const PROTO_VERSION: u16 = 1;
/// Snapshot schema version, bumped on breaking changes to [`Snapshot`].
pub const SNAPSHOT_SCHEMA: u16 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u16,
    pub taken_at: Option<DateTime<Utc>>,
    pub system: SystemInfo,
    pub processes: Vec<ProcessInfo>,
    pub containers: Vec<Container>,
    pub tmux: Vec<TmuxSession>,
    pub listners: Vec<Listner>,
    pub repos: Vec<RepoInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub kernel: String,
    pub uptime_secs: u64,
    pub load1: f32,
    pub mem_total_kb: u64,
    pub mem_used_kb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub user: String,
    pub cmdline: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxSession {
    pub name: String,
    pub windows: u32,
    pub attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listner {
    pub proto: String,
    pub addr: String,
    pub port: u16,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub path: String,
    pub branch: Option<String>,
    pub dirty: u32,
}

/// One etnry in a VM's timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

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
    },
    ContainerStarted {
        name: String,
        image: String,
    },
    ContainerStopped {
        name: String,
    },
    TmuxCreated {
        name: String,
    },
    TmuxClosed {
        name: String,
    },
    GitBranchChanged {
        repo: String,
        from: Option<String>,
        to: Option<String>,
    },
    VmRebooted,
    Gap {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
}

/// Messages on the agent stream (one JSON per line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WireMsg {
    Hello {
        agent_id: Uuid,
        version: String,
        proto: u16,
        caps: Vec<String>,
        oldest_seq: u64,
        latest_seq: u64,
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
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.starts_with(r#"{"t":"hello""#));
        assert_eq!(serde_json::from_str::<WireMsg>(&line).unwrap(), msg);
    }
}
