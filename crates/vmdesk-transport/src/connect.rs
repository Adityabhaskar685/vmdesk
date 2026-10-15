//! Walks a [`ConnectionPath`] hop by hop and caches the sessions it builds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::time::timeout;
use vmdesk_core::{
    ByteStream, ConnectionPath, Error, Hop, HopConnector, HostKeyStore, Origin, Result, Tunnel, Vm,
    VmId,
};

use crate::hostkey::Client;
use crate::session::SshSession;
use crate::{CONNECT_TIMEOUT, auth, client_config};

/// Reaches a VM's SSH port over TCP: from the laptop, or through the previous hop's
/// session with `direct-tcpip`. Endpoints are tried in order.
pub struct TcpConnector;

#[async_trait]
impl HopConnector for TcpConnector {
    async fn open(
        &self,
        _hop: &Hop,
        dest: &Vm,
        upstream: Option<&dyn Tunnel>,
    ) -> Result<Box<dyn ByteStream>> {
        let mut failures = Vec::new();
        for ep in &dest.endpoints {
            let attempt = async {
                if let Some(tunnel) = upstream {
                    tunnel.open_tcp(&ep.address, ep.port).await
                } else {
                    let tcp = TcpStream::connect((ep.address.as_str(), ep.port)).await?;
                    tcp.set_nodelay(true)?;
                    Ok(Box::new(tcp) as Box<dyn ByteStream>)
                }
            };
            match timeout(CONNECT_TIMEOUT, attempt).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) => failures.push(format!("{}:{}: {e}", ep.address, ep.port)),
                Err(_) => failures.push(format!("{}:{}: timed out", ep.address, ep.port)),
            }
        }
        if failures.is_empty() {
            failures.push("no endpoints".to_owned());
        }
        Err(Error::Connect(format!(
            "{}: {}",
            dest.name,
            failures.join("; ")
        )))
    }
}

/// Builds SSH sessions over connection paths and keeps one per VM for reuse.
pub struct Transport {
    store: Arc<dyn HostKeyStore>,
    connector: Arc<dyn HopConnector>,
    config: Arc<russh::client::Config>,
    accept_new_host_keys: bool,
    use_agent: bool,
    sessions: Mutex<HashMap<VmId, Arc<SshSession>>>,
}

impl Transport {
    #[must_use]
    pub fn new(store: Arc<dyn HostKeyStore>) -> Self {
        Self {
            store,
            connector: Arc::new(TcpConnector),
            config: client_config(),
            accept_new_host_keys: false,
            use_agent: true,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Trust and store host keys of VMs that have none yet, instead of failing.
    #[must_use]
    pub fn accept_new_host_keys(mut self, yes: bool) -> Self {
        self.accept_new_host_keys = yes;
        self
    }

    /// Whether to try ssh-agent keys (on by default).
    #[must_use]
    pub fn use_agent(mut self, yes: bool) -> Self {
        self.use_agent = yes;
        self
    }

    #[must_use]
    pub fn with_connector(mut self, connector: Arc<dyn HopConnector>) -> Self {
        self.connector = connector;
        self
    }

    /// An authenticated session to `target`, trying its paths in order. `inventory` must
    /// contain every jump host the paths name.
    pub async fn session(
        &self,
        target: &Vm,
        inventory: &HashMap<VmId, Vm>,
    ) -> Result<Arc<SshSession>> {
        if let Some(s) = self.cached(target.id) {
            return Ok(s);
        }
        let mut failures = Vec::new();
        for path in paths(target) {
            match self.session_over(target, &path, inventory).await {
                Ok(s) => return Ok(s),
                // Never fall back to another path past a host-key problem.
                Err(e @ (Error::UnknownHostKey { .. } | Error::HostKeyChanged { .. })) => {
                    return Err(e);
                }
                Err(e) => {
                    tracing::debug!(vm = %target.name, error = %e, "path failed");
                    failures.push(e);
                }
            }
        }
        Err(combine(failures))
    }

    /// A byte stream to `target`'s SSH port over its first working path, without
    /// authenticating to the target itself. Used by `vmdesk proxy`, where `ssh` does
    /// the final handshake. Jump sessions stay cached in this `Transport`, so keep it
    /// alive for as long as the stream is used.
    pub async fn stream(
        &self,
        target: &Vm,
        inventory: &HashMap<VmId, Vm>,
    ) -> Result<Box<dyn ByteStream>> {
        let mut failures = Vec::new();
        for path in paths(target) {
            let Some((last, jumps)) = path.hops.split_last() else {
                continue;
            };
            let attempt = async {
                let upstream = self.walk(target, jumps, inventory).await?;
                self.connector
                    .open(last, target, upstream.as_deref().map(|s| s as &dyn Tunnel))
                    .await
            };
            match attempt.await {
                Ok(stream) => return Ok(stream),
                Err(e @ (Error::UnknownHostKey { .. } | Error::HostKeyChanged { .. })) => {
                    return Err(e);
                }
                Err(e) => failures.push(e),
            }
        }
        Err(combine(failures))
    }

    /// Disconnects sessions unused for `idle`, unless another session tunnels through them.
    pub async fn close_idle(&self, idle: Duration) {
        let stale: Vec<Arc<SshSession>> = {
            let Ok(mut sessions) = self.sessions.lock() else {
                return;
            };
            let ids: Vec<VmId> = sessions
                .iter()
                .filter(|(_, s)| {
                    s.is_closed() || (s.idle_since().elapsed() >= idle && Arc::strong_count(s) == 1)
                })
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        for s in stale {
            s.disconnect().await;
        }
    }

    /// Disconnects every cached session.
    pub async fn close_all(&self) {
        let all: Vec<Arc<SshSession>> = match self.sessions.lock() {
            Ok(mut s) => s.drain().map(|(_, s)| s).collect(),
            Err(_) => return,
        };
        for s in all {
            s.disconnect().await;
        }
    }

    async fn session_over(
        &self,
        target: &Vm,
        path: &ConnectionPath,
        inventory: &HashMap<VmId, Vm>,
    ) -> Result<Arc<SshSession>> {
        let upstream = self.walk(target, &path.hops, inventory).await?;
        upstream.ok_or(Error::InvalidPath("path has no hops"))
    }

    /// Opens a session to the destination of each hop in turn, reusing cached ones.
    /// Returns the session for the last hop.
    async fn walk(
        &self,
        target: &Vm,
        hops: &[Hop],
        inventory: &HashMap<VmId, Vm>,
    ) -> Result<Option<Arc<SshSession>>> {
        let mut upstream: Option<Arc<SshSession>> = None;
        for hop in hops {
            let dest = match hop {
                Hop::Jump { via } => inventory.get(via).ok_or_else(|| {
                    Error::NotFound(format!("jump host {via} of {}", target.name))
                })?,
                Hop::Direct => target,
            };
            if let Some(s) = self.cached(dest.id) {
                upstream = Some(s);
                continue;
            }
            let stream = self
                .connector
                .open(hop, dest, upstream.as_deref().map(|s| s as &dyn Tunnel))
                .await?;
            let session = Arc::new(self.handshake(stream, dest, upstream.take()).await?);
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.insert(dest.id, Arc::clone(&session));
            }
            upstream = Some(session);
        }
        Ok(upstream)
    }

    async fn handshake(
        &self,
        stream: Box<dyn ByteStream>,
        dest: &Vm,
        upstream: Option<Arc<SshSession>>,
    ) -> Result<SshSession> {
        let rejected = Arc::new(Mutex::new(None));
        let client = Client {
            vm: dest.id,
            name: dest.name.clone(),
            store: Arc::clone(&self.store),
            accept_new: self.accept_new_host_keys,
            rejected: Arc::clone(&rejected),
        };
        let connected = timeout(
            CONNECT_TIMEOUT,
            russh::client::connect_stream(Arc::clone(&self.config), stream, client),
        )
        .await;
        let mut handle = match connected {
            Ok(Ok(handle)) => handle,
            Ok(Err(e)) => {
                let reason = rejected.lock().ok().and_then(|mut r| r.take());
                return Err(reason.unwrap_or_else(|| {
                    Error::Connect(format!("{}: SSH handshake: {e}", dest.name))
                }));
            }
            Err(_) => {
                return Err(Error::Connect(format!(
                    "{}: SSH handshake timed out",
                    dest.name
                )));
            }
        };
        auth::authenticate(&mut handle, dest, self.use_agent).await?;
        tracing::debug!(vm = %dest.name, "ssh session established");
        Ok(SshSession::new(dest, handle, upstream))
    }

    fn cached(&self, vm: VmId) -> Option<Arc<SshSession>> {
        let mut sessions = self.sessions.lock().ok()?;
        match sessions.get(&vm) {
            Some(s) if !s.is_closed() => Some(Arc::clone(s)),
            Some(_) => {
                sessions.remove(&vm);
                None
            }
            None => None,
        }
    }
}

/// A VM's paths in preference order; a VM without any is reached directly.
fn paths(vm: &Vm) -> Vec<ConnectionPath> {
    if vm.paths.is_empty() {
        vec![ConnectionPath::direct(Origin::User)]
    } else {
        vm.paths.clone()
    }
}

fn combine(mut failures: Vec<Error>) -> Error {
    if failures.len() == 1 {
        return failures.remove(0);
    }
    let all: Vec<String> = failures.iter().map(ToString::to_string).collect();
    Error::Connect(format!("every path failed: {}", all.join(" | ")))
}
