//! `vmdesk exec`, `vmdesk proxy` and `vmdesk forget-host-key`: reach VMs over their paths.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use vmdesk_core::{ALIASES_LABEL, HostKeyStore, Vm, VmId};
use vmdesk_db::{Db, ListFilter};
use vmdesk_transport::Transport;

/// Exit code when the remote command ended without a status (killed by a signal).
const NO_STATUS: u8 = 255;

/// Looks a VM up by UUID, name or alias.
pub async fn resolve(db: &Db, name: &str) -> anyhow::Result<(Vm, HashMap<VmId, Vm>)> {
    let all = db.list_vms(&ListFilter::default()).await?;
    let found = name
        .parse::<VmId>()
        .ok()
        .and_then(|id| all.iter().find(|v| v.id == id))
        .or_else(|| all.iter().find(|v| v.name.eq_ignore_ascii_case(name)))
        .or_else(|| {
            all.iter().find(|v| {
                v.labels
                    .get(ALIASES_LABEL)
                    .is_some_and(|a| a.split(',').any(|x| x.trim().eq_ignore_ascii_case(name)))
            })
        })
        .cloned()
        .with_context(|| format!("no VM named {name:?}; see `vmdesk list`"))?;
    Ok((found, all.into_iter().map(|v| (v.id, v)).collect()))
}

pub(crate) fn transport(db: &Db, accept_new_host_keys: bool) -> Transport {
    Transport::new(Arc::new(db.clone()) as Arc<dyn HostKeyStore>)
        .accept_new_host_keys(accept_new_host_keys)
}

/// Adds the next step to host-key errors, which are the ones a user has to act on.
pub(crate) fn explain(e: vmdesk_core::Error) -> anyhow::Error {
    let hint = match &e {
        vmdesk_core::Error::UnknownHostKey { .. } => Some(
            "check the fingerprint (`ssh-keygen -lF <host>` against ~/.ssh/known_hosts, or \
            `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the VM), then rerun with \
            --accept-new-host-keys",
        ),
        vmdesk_core::Error::HostKeyChanged { vm, .. } => {
            return anyhow::anyhow!(
                "{e}\nIf the VM was reinstalled, run `vmdesk forget-host-key {vm}` and connect \
                again with --accept-new-host-keys"
            );
        }
        _ => None,
    };
    match hint {
        Some(h) => anyhow::anyhow!("{e}\n{h}"),
        None => anyhow::Error::new(e),
    }
}

/// Runs `command` on the VM, streaming its output; exits with the remote status.
pub async fn exec(
    db: &Db,
    name: &str,
    user: Option<String>,
    command: &[String],
    accept_new_host_keys: bool,
) -> anyhow::Result<ExitCode> {
    let (mut vm, inventory) = resolve(db, name).await?;
    if let Some(user) = user {
        vm.username = user;
    }
    let t = transport(db, accept_new_host_keys);
    let session = t.session(&vm, &inventory).await.map_err(explain)?;
    // Joined with spaces like `ssh host cmd args`: the remote shell does the parsing.
    let status = session
        .exec_to(
            &command.join(" "),
            &mut tokio::io::stdout(),
            &mut tokio::io::stderr(),
        )
        .await
        .map_err(explain)?;
    t.close_all().await;
    Ok(ExitCode::from(status.map_or(NO_STATUS, |s| {
        u8::try_from(s).unwrap_or(NO_STATUS)
    })))
}

/// Pipes stdin/stdout to the VM's SSH port, for `ssh -o ProxyCommand='vmdesk proxy %n'`.
pub async fn proxy(db: &Db, name: &str, accept_new_host_keys: bool) -> anyhow::Result<()> {
    let (vm, inventory) = resolve(db, name).await?;
    let t = transport(db, accept_new_host_keys);
    let mut stream = t.stream(&vm, &inventory).await.map_err(explain)?;
    let mut stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    // The peer closing either side is the normal way a proxied session ends.
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut stdio).await;
    t.close_all().await;
    Ok(())
}

pub async fn forget_host_key(db: &Db, name: &str) -> anyhow::Result<()> {
    let (vm, _) = resolve(db, name).await?;
    let removed = db.forget_host_keys(vm.id).await?;
    println!("removed {removed} host key(s) for {}", vm.name);
    Ok(())
}
