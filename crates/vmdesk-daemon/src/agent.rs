//! `vmdesk agent install | upgrade | uninstall | status | sync`: the on-VM agent.

use std::path::PathBuf;

use chrono::Utc;
use clap::Subcommand;
use vmdesk_collect::agent::{self, AgentProbe};
use vmdesk_collect::agent_install;
use vmdesk_core::{AgentState, Vm};
use vmdesk_db::{AgentRecord, Db, ListFilter};

use crate::status::short;
use crate::{print_table, remote};

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Upload the agent to a VM, start its daemon and sync once.
    Install(InstallArgs),
    /// Replace a VM's agent with this build: the same steps as install.
    Upgrade(InstallArgs),
    /// Stop and remove the agent from a VM. Its history on this laptop is kept.
    Uninstall(Target),
    /// Sync each VM's agent now and show its state.
    Status {
        /// VM names, aliases or ids (default: every VM with an agent).
        vms: Vec<String>,
        /// Trust and store host keys of VMs (including jump hosts) not seen before.
        #[arg(long)]
        accept_new_host_keys: bool,
    },
    /// Fetch the events a VM's agent recorded since the last sync, and a snapshot.
    Sync(Target),
}

#[derive(clap::Args)]
pub struct Target {
    /// VM name, alias or id.
    vm: String,
    /// Trust and store host keys of VMs (including jump hosts) not seen before.
    #[arg(long)]
    accept_new_host_keys: bool,
}

#[derive(clap::Args)]
pub struct InstallArgs {
    #[command(flatten)]
    target: Target,
    /// Static `x86_64` build of `vmdesk-agent` to upload (default: `$VMDESK_AGENT_BIN`,
    /// else `vmdesk-agent-x86_64` next to this executable).
    #[arg(long, value_name = "PATH")]
    binary: Option<PathBuf>,
}

pub async fn run(db: &Db, cmd: AgentCmd) -> anyhow::Result<()> {
    match cmd {
        AgentCmd::Install(args) => install(db, args, "installed").await,
        AgentCmd::Upgrade(args) => install(db, args, "upgraded").await,
        AgentCmd::Uninstall(target) => uninstall(db, &target).await,
        AgentCmd::Status {
            vms,
            accept_new_host_keys,
        } => status(db, &vms, accept_new_host_keys).await,
        AgentCmd::Sync(target) => sync(db, &target).await,
    }
}

async fn install(db: &Db, args: InstallArgs, verb: &str) -> anyhow::Result<()> {
    let binary = agent_install::read_binary(args.binary)?;
    let (vm, inventory) = remote::resolve(db, &args.target.vm).await?;
    let t = remote::transport(db, args.target.accept_new_host_keys);
    let session = t.session(&vm, &inventory).await.map_err(remote::explain)?;
    let done = agent_install::install(db, session.as_ref(), vm.id, &binary).await;
    t.close_all().await;
    let done = done?;
    let version = db
        .agent(vm.id)
        .await?
        .map(|a| a.version)
        .unwrap_or_default();
    println!(
        "{verb} vmdesk-agent {version} on {} (runs under {}); {}",
        vm.name,
        done.supervisor,
        outcome(&vm, &done.probe)
    );
    Ok(())
}

async fn uninstall(db: &Db, target: &Target) -> anyhow::Result<()> {
    let (vm, inventory) = remote::resolve(db, &target.vm).await?;
    let t = remote::transport(db, target.accept_new_host_keys);
    let session = t.session(&vm, &inventory).await.map_err(remote::explain)?;
    let done = agent_install::uninstall(db, session.as_ref(), vm.id).await;
    t.close_all().await;
    done?;
    println!("removed the agent from {}; its history is kept", vm.name);
    Ok(())
}

async fn sync(db: &Db, target: &Target) -> anyhow::Result<()> {
    let (vm, inventory) = remote::resolve(db, &target.vm).await?;
    let t = remote::transport(db, target.accept_new_host_keys);
    let session = t.session(&vm, &inventory).await.map_err(remote::explain)?;
    let probe = agent::sync(db, session.as_ref(), vm.id).await;
    t.close_all().await;
    let probe = probe?;
    match probe {
        AgentProbe::Absent | AgentProbe::Error(_) => {
            anyhow::bail!("{}: {}", vm.name, outcome(&vm, &probe))
        }
        _ => println!("{}: {}", vm.name, outcome(&vm, &probe)),
    }
    Ok(())
}

async fn status(db: &Db, names: &[String], accept_new_host_keys: bool) -> anyhow::Result<()> {
    let all = db.list_vms(&ListFilter::default()).await?;
    let targets: Vec<Vm> = if names.is_empty() {
        let with_agent: Vec<_> = db.agents().await?.into_iter().map(|a| a.vm).collect();
        all.iter()
            .filter(|v| with_agent.contains(&v.id))
            .cloned()
            .collect()
    } else {
        let mut picked = Vec::new();
        for n in names {
            picked.push(remote::resolve(db, n).await?.0);
        }
        picked
    };
    if targets.is_empty() {
        println!("no VM has an agent yet; `vmdesk agent install <vm>` adds one");
        return Ok(());
    }
    let inventory = all.into_iter().map(|v| (v.id, v)).collect();
    let t = remote::transport(db, accept_new_host_keys);
    let mut rows = Vec::new();
    for vm in &targets {
        let (state, detail) = match t.session(vm, &inventory).await {
            Ok(session) => match agent::sync(db, session.as_ref(), vm.id).await {
                Ok(probe) => (probe.state(), outcome(vm, &probe)),
                Err(e) => (AgentState::Unknown, e.to_string()),
            },
            Err(e) => (AgentState::Unknown, e.to_string()),
        };
        rows.push(row(vm, state, db.agent(vm.id).await?.as_ref(), &detail));
    }
    t.close_all().await;
    print_table(
        &[
            "NAME",
            "STATE",
            "VERSION",
            "LAST SEQ",
            "LAST CONTACT",
            "DETAIL",
        ],
        &rows,
    );
    Ok(())
}

fn row(vm: &Vm, state: AgentState, agent: Option<&AgentRecord>, detail: &str) -> [String; 6] {
    let contact = agent.and_then(|a| a.last_contact_at).map_or_else(
        || "never".to_owned(),
        |t| {
            format!(
                "{} ago",
                vmdesk_core::human_duration((Utc::now() - t).num_seconds())
            )
        },
    );
    [
        vm.name.clone(),
        state.to_string(),
        agent.map(|a| a.version.clone()).unwrap_or_default(),
        agent.map(|a| a.last_seq.to_string()).unwrap_or_default(),
        contact,
        short(detail),
    ]
}

/// One line on what a sync found.
fn outcome(vm: &Vm, probe: &AgentProbe) -> String {
    match probe {
        AgentProbe::Ok(s) | AgentProbe::Outdated(s) => {
            let mut line = format!("{}, {} new event(s)", probe.state(), s.new_events);
            if s.gap {
                line.push_str(", with a gap: the VM rotated out events this laptop never read");
            }
            if matches!(probe, AgentProbe::Outdated(_)) {
                line = format!("{line}; `vmdesk agent upgrade {}` updates it", vm.name);
            }
            line
        }
        AgentProbe::NotRunning(s) => format!(
            "not running: its daemon is stopped ({} new event(s) up to then); `vmdesk show {}` starts it",
            s.new_events, vm.name
        ),
        AgentProbe::Absent => "absent: no agent binary on the VM".to_owned(),
        AgentProbe::Error(e) => format!("error: {e}"),
    }
}
