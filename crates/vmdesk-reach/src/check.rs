//! One reachability check for one VM (spec section 8): lifecycle, route, then SSH.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use chrono::Utc;
use tokio::io::AsyncReadExt;
use vmdesk_core::{AgentState, Error, Hop, Lifecycle, SshState, Vm, VmId, VmStatus};
use vmdesk_transport::Transport;

use crate::RouteInspector;

/// How far the SSH part of a check goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Handshake and log in.
    Login,
    /// Only reach the SSH port and read its banner. Used in the background for VMs
    /// whose login already failed, so they are not hit with failing logins every few
    /// minutes (and banned by fail2ban). The previous SSH state is kept on success.
    Port,
}

const BANNER_TIMEOUT: Duration = Duration::from_secs(3);

/// Checks one VM. `inventory` must hold its jump hosts; `previous` is its last status.
#[allow(clippy::implicit_hasher)] // matches `Transport::session`, which takes the default map
pub async fn check(
    vm: &Vm,
    inventory: &HashMap<VmId, Vm>,
    transport: &Transport,
    routes: &dyn RouteInspector,
    probe: Probe,
    previous: Option<&VmStatus>,
) -> VmStatus {
    let mut status = VmStatus {
        vm: vm.id,
        route: None,
        ssh: SshState::NotChecked,
        agent: AgentState::Unknown,
        checked_at: Utc::now(),
        detail: None,
    };
    if matches!(vm.lifecycle, Lifecycle::Stopped | Lifecycle::Terminated) {
        status.detail = Some(format!("provider reports {}", vm.lifecycle.as_str()));
        return status;
    }

    let Some(first) = first_hop(vm, inventory) else {
        status.ssh = SshState::Error;
        status.detail = Some("jump host is not in the inventory".to_owned());
        return status;
    };
    status.route = route(first, routes).await;
    if status.route.is_none() {
        status.ssh = SshState::NoRoute;
        status.detail = Some(format!("no route to {}", first.name));
        return status;
    }

    let result = match probe {
        Probe::Login => transport.session(vm, inventory).await.map(|_| SshState::Ok),
        Probe::Port => port(vm, inventory, transport)
            .await
            .map(|()| previous.map_or(SshState::Ok, |p| p.ssh)),
    };
    match result {
        Ok(state) => {
            status.ssh = state;
            status.detail = previous
                .filter(|_| probe == Probe::Port)
                .and_then(|p| p.detail.clone());
        }
        Err(e) => {
            status.ssh = ssh_state(&e);
            status.detail = Some(e.to_string());
        }
    }
    status
}

/// The VM the laptop connects to first: the first jump host, or the VM itself.
fn first_hop<'a>(vm: &'a Vm, inventory: &'a HashMap<VmId, Vm>) -> Option<&'a Vm> {
    match vm.paths.first().and_then(|p| p.hops.first()) {
        Some(Hop::Jump { via }) => inventory.get(via),
        Some(Hop::Direct) | None => Some(vm),
    }
}

/// The interface towards the first of `vm`'s endpoints that has a route.
async fn route(vm: &Vm, routes: &dyn RouteInspector) -> Option<String> {
    for ep in &vm.endpoints {
        let ip = match ep.address.parse::<IpAddr>() {
            Ok(ip) => Some(ip),
            Err(_) => tokio::net::lookup_host((ep.address.as_str(), ep.port))
                .await
                .ok()
                .and_then(|mut addrs| addrs.next())
                .map(|a| a.ip()),
        };
        if let Some(ip) = ip
            && let Some(dev) = routes.route_to(ip).await
        {
            return Some(dev);
        }
    }
    None
}

async fn port(
    vm: &Vm,
    inventory: &HashMap<VmId, Vm>,
    transport: &Transport,
) -> vmdesk_core::Result<()> {
    let mut stream = transport.stream(vm, inventory).await?;
    let mut banner = [0u8; 4];
    match tokio::time::timeout(BANNER_TIMEOUT, stream.read_exact(&mut banner)).await {
        Ok(Ok(_)) if &banner == b"SSH-" => Ok(()),
        Ok(Ok(_)) => Err(Error::Connect(format!("{}: not an SSH server", vm.name))),
        Ok(Err(e)) => Err(Error::Connect(format!("{}: {e}", vm.name))),
        Err(_) => Err(Error::Timeout(format!("{}: SSH banner", vm.name))),
    }
}

fn ssh_state(e: &Error) -> SshState {
    match e {
        Error::Auth(_) => SshState::AuthFailed,
        Error::UnknownHostKey { .. } | Error::HostKeyChanged { .. } => SshState::HostKey,
        Error::Refused(_) => SshState::Refused,
        Error::Timeout(_) => SshState::Timeout,
        _ => SshState::Error,
    }
}
