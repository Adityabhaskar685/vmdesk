//! End-to-end transport tests against in-process russh servers: a target that answers
//! `exec` (and echoes stdin for `cat`), and a bastion that also forwards `direct-tcpip`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vmdesk_core::{
    ConnectionPath, CredentialRef, Endpoint, EndpointKind, Error, HostKey, HostKeyStore, Lifecycle,
    Origin, ProviderId, RemoteExec, Result, Vm, VmId,
};
use vmdesk_transport::Transport;

// ---- test SSH server ---------------------------------------------------------------

#[derive(Clone)]
struct TestServer {
    name: &'static str,
    allowed: PublicKey,
    forwards: Arc<AtomicUsize>,
    /// Channels running `cat`: stdin is echoed until the client's EOF.
    cats: HashSet<ChannelId>,
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if key.key_data() == self.allowed.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(&mut self, _: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.auth_publickey_offered("", key).await
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        if data == b"cat" {
            session.extended_data(channel, 1, b"stderr is not part of the stream".to_vec())?;
            self.cats.insert(channel);
            return Ok(());
        }
        let (out, status) = match data {
            b"uname -a" => (format!("Linux {} 6.8.0 x86_64 GNU/Linux\n", self.name), 0),
            other => (
                format!("{}: command not found\n", String::from_utf8_lossy(other)),
                127,
            ),
        };
        if status == 0 {
            session.data(channel, out.into_bytes())?;
        } else {
            session.extended_data(channel, 1, out.into_bytes())?;
        }
        session.exit_status_request(channel, status)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.cats.contains(&channel) {
            session.data(channel, data.to_vec())?;
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.cats.remove(&channel) {
            session.eof(channel)?;
            session.exit_status_request(channel, 0)?;
            session.close(channel)?;
        }
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        let Ok(mut tcp) = TcpStream::connect((host, u16::try_from(port).unwrap())).await else {
            reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        reply.accept().await;
        self.forwards.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
        });
        Ok(())
    }
}

/// Starts a server on a random local port with a fresh host key.
async fn start(name: &'static str, allowed: PublicKey) -> (u16, PublicKey, Arc<AtomicUsize>) {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let public = host_key.public_key().clone();
    let config = Arc::new(server::Config {
        keys: vec![host_key],
        auth_rejection_time: std::time::Duration::from_millis(1),
        ..Default::default()
    });
    let forwards = Arc::new(AtomicUsize::new(0));
    let handler = TestServer {
        name,
        allowed,
        forwards: Arc::clone(&forwards),
        cats: HashSet::new(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (config, handler) = (Arc::clone(&config), handler.clone());
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, socket, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    (port, public, forwards)
}

// ---- fixtures ----------------------------------------------------------------------

#[derive(Default)]
struct MemStore(Mutex<HashMap<VmId, Vec<HostKey>>>);

#[async_trait]
impl HostKeyStore for MemStore {
    async fn host_keys(&self, vm: VmId) -> Result<Vec<HostKey>> {
        Ok(self.0.lock().unwrap().get(&vm).cloned().unwrap_or_default())
    }
    async fn trust_host_key(&self, vm: VmId, key: &HostKey) -> Result<()> {
        self.0.lock().unwrap().insert(vm, vec![key.clone()]);
        Ok(())
    }
    async fn forget_host_keys(&self, vm: VmId) -> Result<u64> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .remove(&vm)
            .map_or(0, |k| k.len() as u64))
    }
}

/// A client key written to a temp file, as `CredentialRef::KeyFile` expects.
fn client_key() -> (PublicKey, PathBuf) {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let path = std::env::temp_dir().join(format!("vmdesk-key-{}", uuid::Uuid::now_v7()));
    std::fs::write(&path, key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    (key.public_key().clone(), path)
}

fn vm(name: &str, port: u16, key: &Path, paths: Vec<ConnectionPath>) -> Vm {
    Vm {
        id: VmId::new(),
        name: name.to_owned(),
        provider: ProviderId::new(ProviderId::STATIC),
        external_ref: None,
        username: "tester".to_owned(),
        credential: CredentialRef::KeyFile(key.to_owned()),
        endpoints: vec![Endpoint {
            address: "127.0.0.1".to_owned(),
            port,
            kind: EndpointKind::Private,
            source: Origin::User,
            bind_interface: None,
            last_success: None,
        }],
        paths,
        tags: vec![],
        labels: BTreeMap::new(),
        lifecycle: Lifecycle::Unknown,
        favourite: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn inventory(vms: &[&Vm]) -> HashMap<VmId, Vm> {
    vms.iter().map(|v| (v.id, (*v).clone())).collect()
}

fn transport(store: &Arc<MemStore>, accept_new: bool) -> Transport {
    Transport::new(Arc::clone(store) as Arc<dyn HostKeyStore>)
        .accept_new_host_keys(accept_new)
        .use_agent(false)
}

// ---- tests -------------------------------------------------------------------------

#[tokio::test]
async fn exec_direct_and_through_bastion() {
    let (client_pub, key) = client_key();
    let (bastion_port, _, forwards) = start("bastion", client_pub.clone()).await;
    let (target_port, _, _) = start("target", client_pub).await;

    let bastion = vm("bastion", bastion_port, &key, vec![]);
    let direct = vm("direct", target_port, &key, vec![]);
    let behind = vm(
        "behind",
        target_port,
        &key,
        vec![ConnectionPath::via(&[bastion.id], Origin::User)],
    );
    let inv = inventory(&[&bastion, &direct, &behind]);
    let store = Arc::new(MemStore::default());
    let t = transport(&store, true);

    let out = t
        .session(&direct, &inv)
        .await
        .unwrap()
        .exec("uname -a")
        .await
        .unwrap();
    assert_eq!(out.status, Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "Linux target 6.8.0 x86_64 GNU/Linux\n"
    );
    assert_eq!(forwards.load(Ordering::SeqCst), 0);

    let session = t.session(&behind, &inv).await.unwrap();
    let out = session.exec("uname -a").await.unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("Linux target")
    );
    assert_eq!(
        forwards.load(Ordering::SeqCst),
        1,
        "went through the bastion"
    );

    // Cached: the same session object, no new tunnel.
    let again = t.session(&behind, &inv).await.unwrap();
    assert!(Arc::ptr_eq(&session, &again));
    assert_eq!(forwards.load(Ordering::SeqCst), 1);

    // stderr and exit status come through.
    let out = session.exec("nope").await.unwrap();
    assert_eq!(out.status, Some(127));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("command not found")
    );

    // Every hop's key was recorded on first use.
    for v in [&bastion, &direct, &behind] {
        assert_eq!(store.host_keys(v.id).await.unwrap().len(), 1, "{}", v.name);
    }
    t.close_all().await;
    std::fs::remove_file(key).ok();
}

#[tokio::test]
async fn unknown_and_changed_host_keys_block() {
    let (client_pub, key) = client_key();
    let (port, server_key, _) = start("target", client_pub.clone()).await;
    let mut target = vm("target", port, &key, vec![]);
    let store = Arc::new(MemStore::default());

    let err = transport(&store, false)
        .session(&target, &inventory(&[&target]))
        .await
        .err()
        .unwrap();
    let Error::UnknownHostKey { fingerprint, .. } = err else {
        panic!("expected UnknownHostKey, got {err}");
    };
    assert_eq!(
        fingerprint,
        vmdesk_transport::host_key(&server_key).fingerprint
    );
    assert!(store.host_keys(target.id).await.unwrap().is_empty());

    transport(&store, true)
        .session(&target, &inventory(&[&target]))
        .await
        .unwrap();

    // Same VM, new server key: blocked even with accept-new.
    let (new_port, _, _) = start("target", client_pub).await;
    target.endpoints[0].port = new_port;
    let err = transport(&store, true)
        .session(&target, &inventory(&[&target]))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::HostKeyChanged { .. }), "{err}");

    store.forget_host_keys(target.id).await.unwrap();
    transport(&store, true)
        .session(&target, &inventory(&[&target]))
        .await
        .unwrap();
    std::fs::remove_file(key).ok();
}

#[tokio::test]
async fn auth_and_path_failures() {
    let (client_pub, key) = client_key();
    let (_, other_key) = client_key();
    let (port, _, _) = start("target", client_pub).await;
    let store = Arc::new(MemStore::default());

    let wrong = vm("target", port, &other_key, vec![]);
    let err = transport(&store, true)
        .session(&wrong, &inventory(&[&wrong]))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::Auth(_)), "{err}");

    let ghost = VmId::new();
    let orphan = vm(
        "orphan",
        port,
        &key,
        vec![ConnectionPath::via(&[ghost], Origin::User)],
    );
    let err = transport(&store, true)
        .session(&orphan, &inventory(&[&orphan]))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::NotFound(_)), "{err}");

    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = closed.local_addr().unwrap().port();
    drop(closed);
    let dead = vm("dead", dead_port, &key, vec![]);
    let err = transport(&store, true)
        .session(&dead, &inventory(&[&dead]))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::Refused(_)), "{err}");

    for p in [key, other_key] {
        std::fs::remove_file(p).ok();
    }
}

#[tokio::test]
async fn stream_reaches_target_ssh_port_through_bastion() {
    let (client_pub, key) = client_key();
    let (bastion_port, _, forwards) = start("bastion", client_pub.clone()).await;
    let (target_port, _, _) = start("target", client_pub).await;
    let bastion = vm("bastion", bastion_port, &key, vec![]);
    let target = vm(
        "target",
        target_port,
        &key,
        vec![ConnectionPath::via(&[bastion.id], Origin::User)],
    );
    let store = Arc::new(MemStore::default());
    let t = transport(&store, true);

    let mut stream = t
        .stream(&target, &inventory(&[&bastion, &target]))
        .await
        .unwrap();
    let mut banner = [0u8; 8];
    stream.read_exact(&mut banner).await.unwrap();
    assert_eq!(&banner, b"SSH-2.0-");
    assert_eq!(forwards.load(Ordering::SeqCst), 1);
    // The target's own key is left to the real ssh client.
    assert!(store.host_keys(target.id).await.unwrap().is_empty());
    std::fs::remove_file(key).ok();
}

#[tokio::test]
async fn exec_io_echoes_stdin_until_eof() {
    let (client_pub, key) = client_key();
    let (port, _, _) = start("target", client_pub).await;
    let target = vm("target", port, &key, vec![]);
    let store = Arc::new(MemStore::default());
    let t = transport(&store, true);
    let session = t.session(&target, &inventory(&[&target])).await.unwrap();

    // Many channel windows of every byte value: binary-safe and flow-controlled.
    let payload: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let (mut rd, mut wr) = tokio::io::split(session.exec_io("cat").await.unwrap());
    let sent = payload.clone();
    let writer = tokio::spawn(async move {
        wr.write_all(&sent).await.unwrap();
        wr.shutdown().await.unwrap();
    });
    let mut echoed = Vec::new();
    // The server ends its output only after it has seen our EOF.
    rd.read_to_end(&mut echoed).await.unwrap();
    writer.await.unwrap();
    assert_eq!(echoed.len(), payload.len());
    assert!(echoed == payload, "stdout is exactly stdin, without stderr");

    assert_eq!(session.exec("uname -a").await.unwrap().status, Some(0));
    t.close_all().await;
    std::fs::remove_file(key).ok();
}
