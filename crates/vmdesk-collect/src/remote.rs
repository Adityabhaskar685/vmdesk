//! SSH-only collector: one shell script per poll, sections split by markers.

use async_trait::async_trait;
use vmdesk_core::{Collector, Error, RemoteExec, Result};
use vmdesk_proto::{Event, SNAPSHOT_SCHEMA, Snapshot};

/// Marker that precedes each section of the probe script's output.
pub const MARKER: &str = "@@vmdesk@@";

/// Probe script run over a single exec channel. Missing tools just yield empty sections.
pub const PROBE_SCRIPT: &str = r#"
m='@@vmdesk@@'
echo "$m hostname"; hostname 2>/dev/null
echo "$m tmux"; tmux list-sessions -F '#{session_name}	#{session_windows}	#{session_attached}' 2>/dev/null
echo "$m end"
"#;

pub struct SshCollector;

#[async_trait]
impl Collector for SshCollector {
    async fn snapshot(&self, session: &dyn RemoteExec) -> Result<Snapshot> {
        let out = session.exec(PROBE_SCRIPT).await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut snap = Snapshot {
            schema_version: SNAPSHOT_SCHEMA,
            ..Snapshot::default()
        };
        for section in text.split(MARKER).skip(1) {
            let (name, body) = section.split_once('\n').unwrap_or((section, ""));
            match name.trim() {
                "hostname" => body.trim().clone_into(&mut snap.system.hostname),
                "tmux" => snap.tmux = crate::parse_tmux(body),
                _ => {}
            }
        }
        Ok(snap)
    }

    async fn events_since(&self, _session: &dyn RemoteExec, _cursor: u64) -> Result<Vec<Event>> {
        Err(Error::Other(
            "SSH collector derives events by diffing snapshots".into(),
        ))
    }
}
