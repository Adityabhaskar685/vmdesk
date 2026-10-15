//! `vmdesk`: the laptop daemon and CLI.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use actix_web::{App, HttpServer, web};
use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use vmdesk_api::AppState;
use vmdesk_core::{Provider, Vm};
use vmdesk_db::{Db, ListFilter, SyncReport};
use vmdesk_providers::SshConfigProvider;

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
    /// Import hosts from an SSH config file (defaut `~/.ssh/config`).
    Import {
        #[arg(long, value_name = "PATH")]
        ssh_config: Option<PathBuf>,
        /// Show what would be imported without changing the inventory.
        #[arg(long)]
        dry_run: bool,
    },
    /// List the inventory.
    #[command(alias = "ls")]
    List {
        /// Only VMs with this tag.
        #[arg(long)]
        tag: Option<String>,
        /// Match name, address or tag.
        query: Option<String>,
    },
    /// Pipe stdio to a VM's SSH port over its `ConnectionPath` (for ssh `ProxyCommand`).
    Proxy { vm: String },
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

    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match cli.cmd {
            Cmd::Serve { listen } => serve(&data_dir, listen).await,
            Cmd::Import {
                ssh_config,
                dry_run,
            } => import(&data_dir, ssh_config, dry_run).await,
            Cmd::List { tag, query } => list(&data_dir, tag, query).await,
            Cmd::Proxy { vm: _ } => anyhow::bail!("not implemented yet"),
        }
    })
}

async fn open_db(data_dir: &Path) -> anyhow::Result<Db> {
    let path = data_dir.join("vmdesk.db");
    Db::open(&path)
        .await
        .with_context(|| format!("opening {}", path.display()))
}

async fn serve(data_dir: &Path, listen: SocketAddr) -> anyhow::Result<()> {
    anyhow::ensure!(
        listen.ip().is_loopback(),
        "refusing to listen on a non-loopback address"
    );
    let db = open_db(&data_dir).await?;
    let token_path = data_dir.join("api-token");
    let token: Arc<str> = load_or_create_token(&token_path)?.into();
    let state = web::Data::new(AppState { db, token });
    tracing::info!(%listen, token_file = %token_path.display(), "vmdesk serving");
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

/// Reads the API token, creating a random one on the first run.
fn load_or_create_token(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_owned()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(token.as_bytes())?;
    Ok(token)
}

async fn import(data_dir: &Path, path: Option<PathBuf>, dry_run: bool) -> anyhow::Result<()> {
    if dry_run {
        let path = path
            .or_else(SshConfigProvider::default_path)
            .context("no home directory, pass --ssh-config")?;
        let provider = SshConfigProvider::new(path);
        let listing = provider.list().await.context("reading SSH config")?;
        let rows: Vec<[String; 4]> = listing
            .iter()
            .map(|p| {
                let ep = p
                    .endpoints
                    .first()
                    .map(|e| format!("{}:{}", e.address, e.port));
                [
                    p.name.clone(),
                    p.username
                        .clone()
                        .unwrap_or_else(vmdesk_api::local_username),
                    ep.unwrap_or_default(),
                    p.jump_via.join(" -> "),
                ]
            })
            .collect();
        print_table(&["NAME", "USER", "ADDRESS", "VIA"], &rows);
        println!("\n{} host(s), dry run nothing changed.", rows.len());
        return Ok(());
    }
    let db = open_db(data_dir).await?;
    let report = vmdesk_api::import_ssh_config(&db, path).await?;
    print_report(&report);
    Ok(())
}

fn print_report(r: &SyncReport) {
    let line = |label: &str, items: &[String]| {
        if !items.is_empty() {
            println!("{label:<11} {}", items.join(", "));
        }
    };
    line("added", &r.added);
    line("refreshed", &r.refreshed);
    line("terminated", &r.terminated);
    for s in &r.skipped {
        println!("skipped   {s}");
    }
    for w in &r.warnings {
        println!("warnings  {w}");
    }
    if r.added.is_empty() && r.refreshed.is_empty() && r.skipped.is_empty() {
        println!("no hosts found");
    }
}

async fn list(data_dir: &Path, tag: Option<String>, q: Option<String>) -> anyhow::Result<()> {
    let db = open_db(data_dir).await?;
    let all = db.list_vms(&ListFilter::default()).await?;
    let names: HashMap<_, _> = all.iter().map(|v| (v.id, v.name.as_str())).collect();
    let shown = db
        .list_vms(&ListFilter {
            tag,
            q,
            provider: None,
        })
        .await?;
    let rows: Vec<[String; 6]> = shown.iter().map(|vm| row(vm, &names)).collect();
    print_table(&["NAME", "USER", "ADDRESS", "VIA", "SOURCE", "TAGS"], &rows);
    Ok(())
}

fn row(vm: &Vm, names: &HashMap<vmdesk_core::VmId, &str>) -> [String; 6] {
    let address = vm
        .endpoints
        .first()
        .map(|e| format!("{}:{}", e.address, e.port));
    let via = match vm.paths.first() {
        None => "(no_path)".to_owned(),
        Some(p) => {
            let hops: Vec<&str> = p
                .jumps_hosts()
                .map(|id| names.get(&id).copied().unwrap_or("?"))
                .collect();
            if hops.is_empty() {
                "direct".to_owned()
            } else {
                hops.join(" -> ")
            }
        }
    };
    let mut source = vm.provider.to_string();
    if vm.lifecycle == vmdesk_core::Lifecycle::Terminated {
        source.push_str(" (gone)");
    }
    [
        vm.name.clone(),
        vm.username.clone(),
        address.unwrap_or_default(),
        via,
        source,
        vm.tags.join(","),
    ]
}

fn print_table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) {
    let mut widths = headers.map(|h| h.chars().count());
    for r in rows {
        for (w, cell) in widths.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let fmt = |cells: Vec<&str>| {
        let parts: Vec<String> = cells
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", parts.join("  ").trim_end());
    };
    fmt(headers.to_vec());
    for r in rows {
        fmt(r.iter().map(String::as_str).collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn token_created_once_with_private_permissions() {
        let dir = std::env::temp_dir().join(format!("vmdesk-token-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api-token");
        let first = load_or_create_token(&path).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(load_or_create_token(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
