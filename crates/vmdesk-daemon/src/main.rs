//! `vmdesk`: the laptop daemon and CLI.

use std::net::SocketAddr;
use std::path::PathBuf;

use actix_web::{App, HttpServer, web};
use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use vmdesk_api::AppState;
use vmdesk_db::Db;

#[derive(Parser)]
#[command(
    name = "vmdesk",
    version,
    about = "Personal VM and dev-environment manager"
)]
struct Cli {
    /// Data directory (database, logs). Defaults to the os data dir.
    #[arg(long, env = "VMDESK_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the local API and UI server.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7878")]
        listen: SocketAddr,
    },
    /// Pipe stdio to a VM's SSH port over its `ConnectionPath` (for ssh `ProxyCommand`).
    Proxy { vm: String },
    /// Import hosts from an SSH config file.
    Import {
        #[arg(long)]
        ssh_config: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    let data_dir = cli
        .data_dir
        .or_else(|| dirs::data_dir().map(|d| d.join("vmdesk")))
        .context("no data directory, pass --data-dir")?;

    std::fs::create_dir_all(&data_dir)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match cli.cmd {
            Cmd::Serve { listen } => serve(&data_dir, listen).await,
            Cmd::Proxy { vm: _ } | Cmd::Import { .. }  => anyhow::bail!("not implemented yet"),
        }
    })
}

async fn serve(data_dir: &std::path::Path, listen: SocketAddr) -> anyhow::Result<()> {
    anyhow::ensure!(
        listen.ip().is_loopback(),
        "refusing to listen on a non-loopback address"
    );
    let db = Db::open(&data_dir.join("vmdesk.db")).await?;
    let state = web::Data::new(AppState { db });
    tracing::info!(%listen, "vmdesk running");
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .configure(vmdesk_api::configure)
    })
    .bind(listen)?
    .run()
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }
}
