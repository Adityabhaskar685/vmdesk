//! `vmdesk-agent`: VM-side collector daemon with a local event log, streamed to
//! the laptop as NDJSON over ssh stdio. Opens no network port.

use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use vmdesk_proto::{PROTO_VERSION, WireMsg};

#[derive(Parser)]
#[command(name = "vmdesk-agent", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Collector loop that appends events to the local log (system --user service).
    Daemon,
    /// Stream events since a sequence number to stdout, read control messages on stdion.
    Stream {
        #[arg(long, default_value_t = 0)]
        since: u64,
    },
    /// Print one snapshot as JSON.
    Snapshot,
    /// Print the hello message (version, capabilities, sequence range).
    Status,
}

fn hello(agent_id: Uuid) -> WireMsg {
    WireMsg::Hello {
        agent_id,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        proto: PROTO_VERSION,
        caps: vec!["tmux".into()],
        oldest_seq: 0,
        latest_seq: 0,
        daemon: false,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Logs go the stderr, stdout is the protocol channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let agent_id = Uuid::nil(); // TODO: load or create in the state dir
    match cli.cmd {
        Cmd::Status | Cmd::Stream { .. } => {
            let mut line = serde_json::to_vec(&hello(agent_id))?;
            line.push(b'\n');
            tokio::io::stdout().write_all(&line).await?;
            Ok(())
        }
        Cmd::Daemon | Cmd::Snapshot => anyhow::bail!("not implemented yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_and_hello() {
        Cli::command().debug_assert();
        let line = serde_json::to_string(&hello(Uuid::nil())).unwrap();
        assert!(line.contains(r#""proto":1"#));
    }
}
