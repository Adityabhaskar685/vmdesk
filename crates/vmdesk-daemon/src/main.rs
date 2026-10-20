//! `vmdesk`: the command-line interface.

mod agent;
mod askpass;
mod remote;
mod show;
mod status;
mod work;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use vmdesk_core::{CredentialRef, EndpointKind, Hop, Provider, Vm};
use vmdesk_db::{Db, EndpointInput, ListFilter, NewVm, SyncReport, VmPatch};
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

#[derive(clap::Args)]
struct AddArgs {
    /// Name to refer to it by.
    name: String,
    /// IP address or hostname.
    address: String,
    #[arg(short, long, default_value_t = 22)]
    port: u16,
    /// Login user (default: your local user).
    #[arg(short = 'l', long)]
    user: Option<String>,
    /// Reach it through this jump host; repeat for a chain, nearest first.
    #[arg(long, value_name = "VM")]
    via: Vec<String>,
    /// Private key file to authenticate with.
    #[arg(long, value_name = "PATH")]
    key: Option<PathBuf>,
    #[arg(long)]
    tag: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import hosts from an SSH config file (default `~/.ssh/config`).
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
    /// Run a command on a VM over its connection path; exits with the command's status.
    ///
    /// Arguments are joined with spaces and run by the remote shell, as with `ssh host cmd`.
    Exec {
        /// VM name, alias or id.
        vm: String,
        /// Log in as this user for this run only (jump hosts keep their own users).
        #[arg(short = 'l', long)]
        user: Option<String>,
        /// Trust and store host keys of VMs (including jump hosts) not seen before.
        #[arg(long)]
        accept_new_host_keys: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Change the user or key used to log in to a VM.
    Edit {
        /// VM name, alias or id.
        vm: String,
        /// Login user.
        #[arg(short = 'l', long)]
        user: Option<String>,
        /// Private key file to authenticate with (tried before ssh-agent).
        #[arg(long, value_name = "PATH", conflicts_with = "agent")]
        key: Option<PathBuf>,
        /// Authenticate with ssh-agent and the default keys in ~/.ssh instead of a key file.
        #[arg(long)]
        agent: bool,
    },
    /// Pipe stdio to a VM's SSH port over its connection path.
    ///
    /// For `ssh -o ProxyCommand='vmdesk proxy %n' <vm>`; jump-host keys are checked here,
    /// the target's own key by ssh.
    Proxy {
        /// VM name, alias or id.
        vm: String,
        /// Trust and store host keys of jump hosts not seen before.
        #[arg(long)]
        accept_new_host_keys: bool,
    },
    /// Remove a VM from the inventory, with its stored password, host keys and history.
    Remove {
        /// VM name, alias or id.
        vm: String,
    },
    /// `SSH_ASKPASS` helper for "Open terminal": answers a VM's password prompt from the
    /// keychain and passes other prompts to the user.
    #[command(hide = true)]
    Askpass {
        /// Keychain entry holding the password.
        entry: String,
        /// The prompt `ssh` shows.
        prompt: Option<String>,
    },
    /// Remove a VM's stored host keys, e.g. after it was reinstalled.
    ForgetHostKey {
        /// VM name, alias or id.
        vm: String,
    },
    /// Add a VM by hand.
    Add(AddArgs),
    /// Store a VM's login password in the OS keychain and log in with it.
    ///
    /// Reads the password from the terminal, or the first line of stdin when piped.
    SetPassword {
        /// VM name, alias or id.
        vm: String,
    },
    /// Show what is on a VM now: system, tmux, containers, ports, repos and processes.
    Show {
        /// VM name, alias or id.
        vm: String,
        /// Print the last stored snapshot instead of collecting a new one.
        #[arg(long)]
        cached: bool,
    },
    /// Show what happened on a VM, oldest first.
    Timeline {
        /// VM name, alias or id.
        vm: String,
        /// Number of most recent events.
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: u32,
    },
    /// List a VM's work sessions, newest first: stretches of activity split by idle
    /// gaps of more than 2 hours.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Sessions {
        #[command(subcommand)]
        cmd: Option<work::SessionsCmd>,
        /// VM name, alias or id.
        #[arg(required = true)]
        vm: Option<String>,
        /// Number of most recent sessions.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: u32,
    },
    /// Write a note on a VM, pinned to its latest work session.
    Note {
        /// VM name, alias or id.
        vm: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Pin it to this session instead (its id, or the start of it).
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// List a VM's notes, newest first.
    Notes {
        /// VM name, alias or id.
        vm: String,
    },
    /// Find events on every VM by command line, directory, repo, branch, container,
    /// tmux session or commit subject. Each word must match the start of a word.
    Search {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Only events this recent, e.g. 12h, 7d or 2w.
        #[arg(long, value_name = "AGE", value_parser = work::parse_age)]
        since: Option<chrono::Duration>,
        /// Most recent matches shown.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Install, upgrade, remove or sync the agent that records a VM's history while
    /// this laptop is away.
    Agent {
        #[command(subcommand)]
        cmd: agent::AgentCmd,
    },
    /// Check which VMs are reachable now: route, then SSH login.
    Status {
        /// VM names, aliases or ids (default: all).
        vms: Vec<String>,
        /// Keep running and print changes: rechecks on network changes, favourites
        /// every minute and the rest every five.
        #[arg(long)]
        watch: bool,
    },
}

fn main() -> anyhow::Result<ExitCode> {
    // stderr: stdout carries `exec` output and the `proxy` byte stream.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
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
            Cmd::Import {
                ssh_config,
                dry_run,
            } => import(&data_dir, ssh_config, dry_run).await?,
            Cmd::List { tag, query } => list(&data_dir, tag, query).await?,
            Cmd::Exec {
                vm,
                user,
                accept_new_host_keys,
                command,
            } => {
                let db = open_db(&data_dir).await?;
                return remote::exec(&db, &vm, user, &command, accept_new_host_keys).await;
            }
            Cmd::Edit {
                vm,
                user,
                key,
                agent,
            } => edit(&open_db(&data_dir).await?, &vm, user, key, agent).await?,
            Cmd::Proxy {
                vm,
                accept_new_host_keys,
            } => remote::proxy(&open_db(&data_dir).await?, &vm, accept_new_host_keys).await?,
            Cmd::ForgetHostKey { vm } => {
                remote::forget_host_key(&open_db(&data_dir).await?, &vm).await?;
            }
            Cmd::Remove { vm } => remove(&open_db(&data_dir).await?, &vm).await?,
            Cmd::Askpass { entry, prompt } => {
                return Ok(askpass::askpass(&entry, prompt.as_deref().unwrap_or("")).await);
            }
            Cmd::Add(args) => add(&open_db(&data_dir).await?, args).await?,
            Cmd::SetPassword { vm } => set_password(&open_db(&data_dir).await?, &vm).await?,
            Cmd::Show { vm, cached } => show::show(&open_db(&data_dir).await?, &vm, cached).await?,
            Cmd::Timeline { vm, limit } => {
                show::timeline(&open_db(&data_dir).await?, &vm, limit).await?;
            }
            Cmd::Sessions { cmd, vm, limit } => {
                let db = open_db(&data_dir).await?;
                match (cmd, vm) {
                    (Some(work::SessionsCmd::Rename { vm, id, title }), _) => {
                        work::rename(&db, &vm, &id, &title).await?;
                    }
                    (None, Some(vm)) => work::sessions(&db, &vm, limit).await?,
                    (None, None) => unreachable!("clap requires a VM without a subcommand"),
                }
            }
            Cmd::Note { vm, text, session } => {
                work::note(&open_db(&data_dir).await?, &vm, &text, session.as_deref()).await?;
            }
            Cmd::Notes { vm } => work::notes(&open_db(&data_dir).await?, &vm).await?,
            Cmd::Search {
                query,
                since,
                limit,
            } => work::search(&open_db(&data_dir).await?, &query, since, limit).await?,
            Cmd::Agent { cmd } => agent::run(&open_db(&data_dir).await?, cmd).await?,
            Cmd::Status { vms, watch } => {
                status::status(&open_db(&data_dir).await?, &vms, watch).await?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn open_db(data_dir: &Path) -> anyhow::Result<Db> {
    let path = data_dir.join("vmdesk.db");
    Db::open(&path)
        .await
        .with_context(|| format!("opening {}", path.display()))
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
                    p.username.clone().unwrap_or_else(local_username),
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
    let report = import_ssh_config(&db, path).await?;
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

async fn edit(
    db: &Db,
    name: &str,
    user: Option<String>,
    key: Option<PathBuf>,
    agent: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        user.is_some() || key.is_some() || agent,
        "nothing to change: pass --user, --key or --agent"
    );
    let credential = match key {
        Some(path) => Some(key_credential(&path)?),
        None => agent.then_some(CredentialRef::SshAgent),
    };
    let (vm, _) = remote::resolve(db, name).await?;
    let vm = db
        .update_vm(
            vm.id,
            VmPatch {
                username: user,
                credential,
                ..VmPatch::default()
            },
        )
        .await?;
    let auth = match &vm.credential {
        CredentialRef::KeyFile(p) => format!("key {}", p.display()),
        CredentialRef::SshAgent => "ssh-agent, then default keys".to_owned(),
        CredentialRef::KeyChain(e) => format!("keychain {e}"),
        CredentialRef::CloudProvider(p) => format!("cloud profile {p}"),
    };
    println!("{}: user {}, {auth}", vm.name, vm.username);
    Ok(())
}

async fn add(db: &Db, args: AddArgs) -> anyhow::Result<()> {
    let credential = args
        .key
        .as_deref()
        .map(key_credential)
        .transpose()?
        .unwrap_or_default();
    let mut hops = Vec::new();
    for v in &args.via {
        hops.push(Hop::Jump {
            via: remote::resolve(db, v).await?.0.id,
        });
    }
    let paths = (!hops.is_empty()).then(|| {
        hops.push(Hop::Direct);
        vec![hops]
    });
    let vm = db
        .create_vm(NewVm {
            name: args.name,
            username: args.user.unwrap_or_else(local_username),
            credential,
            endpoints: vec![EndpointInput {
                address: args.address,
                port: args.port,
                kind: EndpointKind::default(),
                bind_interface: None,
            }],
            paths,
            tags: args.tag,
            favourite: false,
        })
        .await?;
    println!("added {} ({})", vm.name, vm.id);
    Ok(())
}

async fn remove(db: &Db, name: &str) -> anyhow::Result<()> {
    let (vm, _) = remote::resolve(db, name).await?;
    db.delete_vm(vm.id).await?;
    if let CredentialRef::KeyChain(entry) = &vm.credential {
        vmdesk_transport::keychain::delete_password(entry).await?;
    }
    println!("removed {}", vm.name);
    Ok(())
}

fn key_credential(path: &Path) -> anyhow::Result<CredentialRef> {
    let path =
        std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    anyhow::ensure!(path.is_file(), "no key file at {}", path.display());
    Ok(CredentialRef::KeyFile(path))
}

async fn set_password(db: &Db, name: &str) -> anyhow::Result<()> {
    use std::io::IsTerminal;

    let (vm, _) = remote::resolve(db, name).await?;
    let password = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("Password for {}@{}: ", vm.username, vm.name))?
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    };
    anyhow::ensure!(!password.is_empty(), "empty password");
    // One keychain entry per VM, named by its id; the database keeps only that name.
    let entry = vm.id.to_string();
    vmdesk_transport::keychain::set_password(&entry, &password).await?;
    db.update_vm(
        vm.id,
        VmPatch {
            credential: Some(CredentialRef::KeyChain(entry)),
            ..VmPatch::default()
        },
    )
    .await?;
    println!(
        "stored password for {}@{} in the keychain",
        vm.username, vm.name
    );
    Ok(())
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
    let states = db.all_status().await?;
    let rows: Vec<[String; 7]> = shown
        .iter()
        .map(|vm| row(vm, &names, states.get(&vm.id)))
        .collect();
    print_table(
        &["NAME", "USER", "ADDRESS", "VIA", "STATE", "SOURCE", "TAGS"],
        &rows,
    );
    Ok(())
}

fn row(
    vm: &Vm,
    names: &HashMap<vmdesk_core::VmId, &str>,
    state: Option<&vmdesk_core::VmStatus>,
) -> [String; 7] {
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
        status::state_label(vm, state),
        source,
        vm.tags.join(","),
    ]
}

pub(crate) fn print_table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) {
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

/// Imports `path` (default `~/.ssh/config`) into the inventory.
async fn import_ssh_config(db: &Db, path: Option<PathBuf>) -> anyhow::Result<SyncReport> {
    let path = path
        .or_else(SshConfigProvider::default_path)
        .context("no home directory; pass a path")?;
    let provider = SshConfigProvider::new(path);
    let listing = provider
        .list()
        .await
        .with_context(|| format!("reading {}", provider.path().display()))?;
    Ok(db
        .sync_provider(provider.id(), listing, &local_username())
        .await?)
}

/// The local login name, which `ssh` uses when a host has no `User`.
fn local_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_owned())
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
