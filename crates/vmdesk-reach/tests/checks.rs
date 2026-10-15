//! Reachability checks with a scripted route table and plain TCP servers.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use vmdesk_core::{
    AgentState, ConnectionPath, CredentialRef, Endpoint, EndpointKind, Headline, HostKey,
    HostKeyStore, Lifecycle, Origin, ProviderId, Result, SshState, Vm, VmId, VmStatus,
};
use vmdesk_db::{Db, EndpointInput, NewVm};
use vmdesk_reach::{Monitor, Probe, RouteInspector, check};
use vmdesk_transport::Transport;

/// Routes everything through `dev`, or nothing when `dev` is `None`; records queries.
#[derive(Default)]
struct FakeRoutes {
    dev: Mutex<Option<String>>,
    asked: Mutex<Vec<IpAddr>>,
}

impl FakeRoutes {
    fn via(dev: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            dev: Mutex::new(dev.map(str::to_owned)),
            asked: Mutex::default(),
        })
    }
}

#[async_trait]
impl RouteInspector for FakeRoutes {
    async fn route_to(&self, addr: IpAddr) -> Option<String> {
        self.asked.lock().unwrap().push(addr);
        self.dev.lock().unwrap().clone()
    }
}

struct NoKeys;

#[async_trait]
impl HostKeyStore for NoKeys {
    async fn host_keys(&self, _: VmId) -> Result<Vec<HostKey>> {
        Ok(vec![])
    }
    async fn trust_host_key(&self, _: VmId, _: &HostKey) -> Result<()> {
        Ok(())
    }
    async fn forget_host_keys(&self, _: VmId) -> Result<u64> {
        Ok(0)
    }
}

/// A server that writes `greeting` to every connection, like sshd's banner.
async fn greeter(greeting: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let _ = s.write_all(greeting).await;
        }
    });
    port
}

async fn dead_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

fn vm(name: &str, address: &str, port: u16, paths: Vec<ConnectionPath>) -> Vm {
    Vm {
        id: VmId::new(),
        name: name.to_owned(),
        provider: ProviderId::new(ProviderId::STATIC),
        external_ref: None,
        username: "tester".to_owned(),
        credential: CredentialRef::SshAgent,
        endpoints: vec![Endpoint {
            address: address.to_owned(),
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
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn transport() -> Transport {
    Transport::new(Arc::new(NoKeys)).use_agent(false)
}

#[tokio::test]
async fn skips_stopped_and_unroutable_vms() {
    let routes = FakeRoutes::via(None);
    let mut v = vm("box", "10.1.1.1", 22, vec![]);
    let inv = HashMap::from([(v.id, v.clone())]);

    v.lifecycle = Lifecycle::Stopped;
    let s = check(&v, &inv, &transport(), routes.as_ref(), Probe::Login, None).await;
    assert_eq!(s.ssh, SshState::NotChecked);
    assert_eq!(s.headline(v.lifecycle), Headline::Stopped);
    assert!(
        routes.asked.lock().unwrap().is_empty(),
        "stopped VMs are not probed"
    );

    v.lifecycle = Lifecycle::Unknown;
    let s = check(&v, &inv, &transport(), routes.as_ref(), Probe::Login, None).await;
    assert_eq!((s.route.as_deref(), s.ssh), (None, SshState::NoRoute));
    assert_eq!(s.headline(v.lifecycle), Headline::NoRoute);
}

#[tokio::test]
async fn route_is_taken_towards_the_first_jump_host() {
    let routes = FakeRoutes::via(Some("tun0"));
    let bastion = vm("bastion", "127.0.0.1", dead_port().await, vec![]);
    let target = vm(
        "target",
        "10.99.0.5",
        22,
        vec![ConnectionPath::via(&[bastion.id], Origin::User)],
    );
    let inv = HashMap::from([(bastion.id, bastion.clone()), (target.id, target.clone())]);

    let s = check(
        &target,
        &inv,
        &transport(),
        routes.as_ref(),
        Probe::Login,
        None,
    )
    .await;
    assert_eq!(
        routes.asked.lock().unwrap().as_slice(),
        ["127.0.0.1".parse::<IpAddr>().unwrap()]
    );
    assert_eq!(s.route.as_deref(), Some("tun0"));
    // The bastion's port is closed, so the jump fails before the target is tried.
    assert_eq!(s.ssh, SshState::Refused, "{:?}", s.detail);

    let orphan = vm(
        "orphan",
        "10.99.0.6",
        22,
        vec![ConnectionPath::via(&[VmId::new()], Origin::User)],
    );
    let s = check(
        &orphan,
        &inv,
        &transport(),
        routes.as_ref(),
        Probe::Login,
        None,
    )
    .await;
    assert_eq!(s.ssh, SshState::Error);
}

#[tokio::test]
async fn port_probe_keeps_the_previous_login_result() {
    let routes = FakeRoutes::via(Some("wlan0"));
    let sshd = vm(
        "sshd",
        "127.0.0.1",
        greeter(b"SSH-2.0-OpenSSH_9.6\r\n").await,
        vec![],
    );
    let web = vm(
        "web",
        "127.0.0.1",
        greeter(b"HTTP/1.1 400 Bad Request\r\n").await,
        vec![],
    );
    let inv = HashMap::from([(sshd.id, sshd.clone()), (web.id, web.clone())]);
    let failed = VmStatus {
        vm: sshd.id,
        route: Some("wlan0".into()),
        ssh: SshState::AuthFailed,
        agent: AgentState::Unknown,
        checked_at: Utc::now(),
        detail: Some("authentication failed: tester@sshd: no key".into()),
    };

    let s = check(
        &sshd,
        &inv,
        &transport(),
        routes.as_ref(),
        Probe::Port,
        Some(&failed),
    )
    .await;
    assert_eq!(s.ssh, SshState::AuthFailed);
    assert_eq!(s.detail, failed.detail);

    let s = check(
        &sshd,
        &inv,
        &transport(),
        routes.as_ref(),
        Probe::Port,
        None,
    )
    .await;
    assert_eq!(s.ssh, SshState::Ok);

    let s = check(&web, &inv, &transport(), routes.as_ref(), Probe::Port, None).await;
    assert_eq!(s.ssh, SshState::Error, "{:?}", s.detail);
}

#[tokio::test]
async fn monitor_stores_results_and_reports_changes() {
    let db = Db::open_in_memory().await.unwrap();
    let port = dead_port().await;
    let created = db
        .create_vm(NewVm {
            name: "box".into(),
            username: "tester".into(),
            credential: CredentialRef::SshAgent,
            endpoints: vec![EndpointInput {
                address: "127.0.0.1".into(),
                port,
                kind: EndpointKind::Private,
                bind_interface: None,
            }],
            paths: None,
            tags: vec![],
            favourite: false,
        })
        .await
        .unwrap();
    let routes = FakeRoutes::via(Some("wlan0"));
    let monitor =
        Monitor::new(db.clone()).with_routes(Arc::clone(&routes) as Arc<dyn RouteInspector>);

    let first = monitor.check_now(None).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].status.ssh, SshState::Refused);
    assert!(first[0].previous.is_none());

    // VPN dropped: no route any more.
    *routes.dev.lock().unwrap() = None;
    let second = monitor.check_now(Some(&[created.id])).await.unwrap();
    let prev = second[0].previous.as_ref().expect("a change");
    assert_eq!(prev.ssh, SshState::Refused);
    assert_eq!(second[0].status.ssh, SshState::NoRoute);
    assert_eq!(
        db.get_status(created.id).await.unwrap().unwrap().ssh,
        SshState::NoRoute
    );
}
