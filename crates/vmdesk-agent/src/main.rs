//! `vmdesk-agent`: VM-side collector daemon with a local event log, streamed to
//! the laptop as NDJSON over ssh stdio. Opens no network port.

mod daemon;
mod home;
mod service;
mod store;
mod stream;

use std::io::IsTerminal;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::BufReader;
use tracing_subscriber::EnvFilter;
use vmdesk_proto::{PROTO_VERSION, WireMsg};

use crate::home::Home;
use crate::store::{Retention, Store};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Optional tools reported in `hello`.
const CAPS: [&str; 3] = ["docker", "tmux", "git"];

#[derive(Parser)]
#[command(name = "vmdesk-agent", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the version.
    Version,
    /// Print the hello message: id, version, capabilities, sequence range, daemon state.
    Status,
    /// Print one snapshot of this machine as JSON.
    Snapshot,
    /// Collector loop that appends events to the local log.
    Daemon,
    /// NDJSON protocol on stdio: events and snapshots out, control messages in.
    Stream {
        /// Start as if `{"t":"resume","since":N}` had been received.
        #[arg(long)]
        since: Option<u64>,
    },
    /// Set up the daemon (systemd --user, or setsid where unavailable) and restart it.
    Install,
    /// Start the daemon unless it is running.
    Start,
    /// Stop the daemon and remove the service and the agent directory.
    Uninstall,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Logs go to stderr; stdout is the protocol channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = rt.block_on(run(cli.cmd));
    // A pending stdin read must not hold up the exit.
    rt.shutdown_background();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vmdesk-agent: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cmd: Cmd) -> Result<()> {
    let home = || Home::current();
    match cmd {
        Cmd::Version => println!("{VERSION}"),
        Cmd::Snapshot => println!(
            "{}",
            serde_json::to_string(&daemon::take_snapshot().await?)?
        ),
        Cmd::Daemon => daemon::run(&home()?).await?,
        Cmd::Status => {
            let home = home()?;
            let store = open(&home).await?;
            println!("{}", serde_json::to_string(&hello(&home, &store).await?)?);
        }
        Cmd::Stream { since } => {
            let home = home()?;
            let store = open(&home).await?;
            let hello = hello(&home, &store).await?;
            let stdio = (BufReader::new(tokio::io::stdin()), tokio::io::stdout());
            stream::run(
                &store,
                &hello,
                since,
                stdio,
                stream::POLL,
                daemon::take_snapshot,
            )
            .await?;
        }
        Cmd::Install => {
            let home = home()?;
            open(&home).await?.close().await;
            println!("{}", service::install(&home)?);
        }
        Cmd::Start => println!("{}", service::start(&home()?)?),
        Cmd::Uninstall => {
            service::uninstall(&home()?)?;
            println!("uninstalled");
        }
    }
    Ok(())
}

/// Ensures the agent directory, id and event log exist, and opens the log.
async fn open(home: &Home) -> Result<Store> {
    home.ensure_dir()?;
    home.agent_id()?;
    Store::open(&home.events_db(), Retention::DEFAULT).await
}

async fn hello(home: &Home, store: &Store) -> Result<WireMsg> {
    let range = store.range().await?;
    Ok(WireMsg::Hello {
        agent_id: home.agent_id()?,
        version: VERSION.to_owned(),
        proto: PROTO_VERSION,
        caps: installed(&CAPS),
        oldest_seq: range.oldest,
        latest_seq: range.latest,
        daemon: home.running_daemon()?.is_some(),
    })
}

/// The tools among `tools` found as executables on `PATH`.
fn installed(tools: &[&str]) -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    tools
        .iter()
        .filter(|tool| std::env::split_paths(&path).any(|dir| executable(&dir.join(tool))))
        .map(|tool| (*tool).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses() {
        Cli::command().debug_assert();
    }

    #[test]
    fn finds_tools_on_path() {
        assert_eq!(installed(&["sh", "no-such-tool-vmdesk"]), ["sh"]);
    }
}
