//! An authenticated SSH session to one VM: exec, and TCP tunnels for the next hop.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use russh::ChannelMsg;
use russh::client::Handle;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use vmdesk_core::{ByteStream, Error, ExecOutput, RemoteExec, Result, Tunnel, Vm, VmId};

use crate::hostkey::Client;

pub struct SshSession {
    vm: VmId,
    name: String,
    handle: Handle<Client>,
    /// Keeps the jump-host session this one is tunnelled through alive.
    _upstream: Option<Arc<SshSession>>,
    last_used: Mutex<Instant>,
}

impl SshSession {
    pub(crate) fn new(vm: &Vm, handle: Handle<Client>, upstream: Option<Arc<SshSession>>) -> Self {
        Self {
            vm: vm.id,
            name: vm.name.clone(),
            handle,
            _upstream: upstream,
            last_used: Mutex::new(Instant::now()),
        }
    }

    #[must_use]
    pub fn vm(&self) -> VmId {
        self.vm
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    pub(crate) fn idle_since(&self) -> Instant {
        self.last_used
            .lock()
            .map_or_else(|_| Instant::now(), |t| *t)
    }

    fn touch(&self) {
        if let Ok(mut t) = self.last_used.lock() {
            *t = Instant::now();
        }
    }

    /// Runs `command`, streaming its output. Returns the exit status, or `None` if the
    /// command was killed by a signal or the channel closed without one.
    pub async fn exec_to(
        &self,
        command: &str,
        stdout: &mut (dyn AsyncWrite + Unpin + Send),
        stderr: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<Option<u32>> {
        self.touch();
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| self.err(&e))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| self.err(&e))?;
        // No stdin is forwarded; EOF keeps commands that read it from hanging.
        channel.eof().await.map_err(|e| self.err(&e))?;

        let mut status = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.write_all(&data).await?,
                ChannelMsg::ExtendedData { data, ext: 1 } => stderr.write_all(&data).await?,
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                _ => {}
            }
        }
        stdout.flush().await?;
        stderr.flush().await?;
        self.touch();
        Ok(status)
    }

    pub async fn disconnect(&self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }

    fn err(&self, e: &russh::Error) -> Error {
        Error::Connect(format!("{}: {e}", self.name))
    }
}

#[async_trait]
impl RemoteExec for SshSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let status = self.exec_to(command, &mut stdout, &mut stderr).await?;
        Ok(ExecOutput {
            status,
            stdout,
            stderr,
        })
    }
}

#[async_trait]
impl Tunnel for SshSession {
    async fn open_tcp(&self, host: &str, port: u16) -> Result<Box<dyn ByteStream>> {
        self.touch();
        let channel = self
            .handle
            .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
            .await
            .map_err(|e| Error::Connect(format!("{} -> {host}:{port}: {e}", self.name)))?;
        Ok(Box::new(channel.into_stream()))
    }
}
