//! Writes a database of fictional VMs for screenshots and demos:
//! `cargo run -p vmdesk-db --example demo_data -- <data dir>`.
//!
//! Every name, address (RFC 5737 documentation ranges), process and repo is invented.
//! Run the app on it with `VMDESK_DATA_DIR=<data dir> VMDESK_NO_BACKGROUND=1`, so it
//! never tries to reach these hosts.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;
use vmdesk_core::{AgentState, CredentialRef, EndpointKind, Hop, SshState, VmId, VmStatus};
use vmdesk_db::{AgentRecord, Db, EndpointInput, NewVm};
use vmdesk_proto::{
    Commit, Container, Disk, Event, EventKind, Listener, PROTO_VERSION, ProcessInfo, RepoInfo,
    SNAPSHOT_SCHEMA, Snapshot, SystemInfo, TmuxSession,
};

const USER: &str = "dev";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("usage: demo_data <data dir>")?;
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("vmdesk.db");
    if path.exists() {
        return Err(format!("{} exists; pass an empty directory", path.display()).into());
    }
    let db = Db::open(&path).await?;
    let now = Utc::now();

    let bastion = vm(&db, "bastion", "198.51.100.5", None, &["infra"]).await?;
    let build = vm(&db, "build-01", "192.0.2.10", None, &["ci"]).await?;
    let staging = vm(
        &db,
        "staging-api",
        "192.0.2.21",
        Some(bastion),
        &["staging"],
    )
    .await?;
    let gpu = vm(&db, "gpu-lab", "192.0.2.40", None, &["ml"]).await?;
    let old = vm(&db, "old-ci", "192.0.2.55", None, &["ci"]).await?;

    status(&db, bastion, Some("wlan0"), SshState::Ok, None, now).await?;
    status(&db, build, Some("wlan0"), SshState::Ok, None, now).await?;
    status(&db, staging, Some("wlan0"), SshState::Ok, None, now).await?;
    let timeout = "timed out: gpu-lab: 192.0.2.40:22: timed out";
    status(
        &db,
        gpu,
        Some("tun0"),
        SshState::Timeout,
        Some(timeout),
        now,
    )
    .await?;
    status(
        &db,
        old,
        None,
        SshState::NoRoute,
        Some("no route to old-ci"),
        now,
    )
    .await?;

    build_01(&db, build, now).await?;
    staging_api(&db, staging, now).await?;
    bastion_host(&db, bastion, now).await?;

    println!("demo database written to {}", path.display());
    Ok(())
}

async fn vm(
    db: &Db,
    name: &str,
    address: &str,
    via: Option<VmId>,
    tags: &[&str],
) -> Result<VmId, vmdesk_db::Error> {
    let paths = via.map(|via| vec![vec![Hop::Jump { via }, Hop::Direct]]);
    Ok(db
        .create_vm(NewVm {
            name: name.into(),
            username: USER.into(),
            credential: CredentialRef::SshAgent,
            endpoints: vec![EndpointInput {
                address: address.into(),
                port: 22,
                kind: EndpointKind::Private,
                bind_interface: None,
            }],
            paths,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            favourite: false,
        })
        .await?
        .id)
}

async fn status(
    db: &Db,
    vm: VmId,
    route: Option<&str>,
    ssh: SshState,
    detail: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), vmdesk_db::Error> {
    db.save_status(&VmStatus {
        vm,
        route: route.map(str::to_owned),
        ssh,
        agent: AgentState::Unknown,
        checked_at: now - Duration::seconds(12),
        detail: detail.map(str::to_owned),
    })
    .await?;
    Ok(())
}

fn secs(t: DateTime<Utc>) -> i64 {
    t.timestamp()
}

fn process(pid: u32, parent: u32, cmdline: &str, cwd: &str, started: DateTime<Utc>) -> ProcessInfo {
    ProcessInfo {
        pid,
        ppid: parent,
        user: USER.into(),
        cmdline: cmdline.into(),
        cwd: Some(cwd.into()),
        started_at: secs(started),
        cpu_pct: 1.5,
        rss_kb: 48_000,
    }
}

fn system(host: &str, now: DateTime<Utc>, days_up: i64, cpus: u32) -> SystemInfo {
    SystemInfo {
        hostname: host.into(),
        os: "Ubuntu 24.04.1 LTS".into(),
        kernel: "6.8.0-45-generic".into(),
        uptime_secs: u64::try_from(days_up * 86_400).unwrap_or_default(),
        boot_time: secs(now - Duration::days(days_up)),
        load: [0.42, 0.61, 0.58],
        cpus,
        mem_total_kb: 16_000_000,
        mem_used_kb: 6_200_000,
        disks: vec![
            Disk {
                mount: "/".into(),
                total_kb: 200_000_000,
                used_kb: 88_000_000,
            },
            Disk {
                mount: "/data".into(),
                total_kb: 500_000_000,
                used_kb: 310_000_000,
            },
        ],
    }
}

fn container(name: &str, image: &str, status: &str, ports: &str, project: &str) -> Container {
    Container {
        id: format!("{:x}", Uuid::now_v7().as_u128() & 0xffff_ffff_ffff),
        name: name.into(),
        image: image.into(),
        state: "running".into(),
        status: status.into(),
        ports: ports.into(),
        compose_project: Some(project.into()),
    }
}

fn listener(port: u16, process: &str, pid: u32) -> Listener {
    Listener {
        proto: "tcp".into(),
        addr: "0.0.0.0".into(),
        port,
        pid: Some(pid),
        process: Some(process.into()),
    }
}

#[allow(clippy::too_many_lines)] // one VM's fictional data, listed in full
/// The machine with an agent and an active piece of work: a tmux session running the
/// test suite of a service on a fix branch.
async fn build_01(db: &Db, vm: VmId, now: DateTime<Utc>) -> Result<(), vmdesk_db::Error> {
    let repo = "/home/dev/src/inventory-service";
    let web = "/home/dev/src/web-console";
    let started = now - Duration::minutes(95);
    let tests = now - Duration::minutes(7);
    let snapshot = Snapshot {
        schema_version: SNAPSHOT_SCHEMA,
        taken_at: Some(now - Duration::seconds(20)),
        system: system("build-01", now, 23, 8),
        processes: vec![
            process(900, 1, "tmux new -s release-fix", "/home/dev", started),
            process(901, 900, "-bash", repo, started),
            process(950, 901, "cargo test --workspace", repo, tests),
            process(902, 900, "-bash", web, started),
            process(960, 902, "npm run dev", web, now - Duration::minutes(80)),
        ],
        containers: vec![
            container(
                "inventory-db",
                "postgres:16",
                "Up 2 hours (healthy)",
                "5432/tcp",
                "inventory-service",
            ),
            container(
                "inventory-cache",
                "redis:7",
                "Up 2 hours",
                "6379/tcp",
                "inventory-service",
            ),
        ],
        tmux: vec![
            TmuxSession {
                name: "release-fix".into(),
                windows: 3,
                attached: true,
                created: secs(started),
                pane_cwd: Some(repo.into()),
                pane_command: Some("cargo".into()),
            },
            TmuxSession {
                name: "scratch".into(),
                windows: 1,
                attached: false,
                created: secs(now - Duration::days(4)),
                pane_cwd: Some("/home/dev".into()),
                pane_command: Some("bash".into()),
            },
        ],
        listeners: vec![
            listener(5432, "docker-proxy", 1201),
            listener(6379, "docker-proxy", 1202),
            listener(3000, "node", 960),
        ],
        repos: vec![
            RepoInfo {
                path: repo.into(),
                branch: Some("fix/retry-backoff".into()),
                dirty: 3,
                ahead: Some(2),
                behind: Some(0),
                last_commit: Some(Commit {
                    sha: "4f1c2e9a7d0b".into(),
                    time: secs(now - Duration::minutes(25)),
                    subject: "Back off exponentially on supplier timeouts".into(),
                }),
            },
            RepoInfo {
                path: web.into(),
                branch: Some("main".into()),
                dirty: 0,
                ahead: Some(0),
                behind: Some(1),
                last_commit: Some(Commit {
                    sha: "9b7e01c3aa52".into(),
                    time: secs(now - Duration::days(1)),
                    subject: "Show stock levels per warehouse".into(),
                }),
            },
        ],
        unavailable: vec![],
    };

    let agent_id = Uuid::now_v7();
    db.save_agent(
        &AgentRecord {
            vm,
            agent_id,
            version: env!("CARGO_PKG_VERSION").into(),
            proto: PROTO_VERSION,
            installed_at: Some(now - Duration::days(3)),
            last_seq: 0,
            last_contact_at: Some(now - Duration::seconds(18)),
        },
        None,
    )
    .await?;
    db.set_agent_state(vm, AgentState::Ok).await?;

    let yesterday = now - Duration::days(1);
    let kinds = vec![
        (
            yesterday - Duration::hours(2),
            EventKind::TmuxCreated {
                name: "scratch".into(),
                pane_cwd: Some("/home/dev".into()),
            },
        ),
        (
            yesterday - Duration::minutes(90),
            started_kind(951, "make bench", repo),
        ),
        (
            yesterday - Duration::minutes(20),
            EventKind::ProcessExited {
                pid: 951,
                cmdline: "make bench".into(),
                exit_code: Some(0),
                duration_secs: Some(4_200),
            },
        ),
        (
            started,
            EventKind::TmuxCreated {
                name: "release-fix".into(),
                pane_cwd: Some(repo.into()),
            },
        ),
        (
            started + Duration::minutes(2),
            EventKind::GitBranchChanged {
                repo: repo.into(),
                from: Some("main".into()),
                to: Some("fix/retry-backoff".into()),
            },
        ),
        (
            started + Duration::minutes(4),
            EventKind::ContainerStarted {
                name: "inventory-db".into(),
                image: "postgres:16".into(),
                compose_project: Some("inventory-service".into()),
            },
        ),
        (
            started + Duration::minutes(5),
            EventKind::ListenerOpened {
                proto: "tcp".into(),
                addr: "0.0.0.0".into(),
                port: 5432,
                process: Some("docker-proxy".into()),
            },
        ),
        (
            now - Duration::minutes(80),
            started_kind(960, "npm run dev", web),
        ),
        (
            now - Duration::minutes(52),
            started_kind(940, "cargo test --workspace", repo),
        ),
        (
            now - Duration::minutes(44),
            EventKind::ProcessExited {
                pid: 940,
                cmdline: "cargo test --workspace".into(),
                exit_code: Some(101),
                duration_secs: Some(480),
            },
        ),
        (
            now - Duration::minutes(25),
            EventKind::GitCommitted {
                repo: repo.into(),
                branch: Some("fix/retry-backoff".into()),
                sha: "4f1c2e9a7d0b".into(),
                subject: "Back off exponentially on supplier timeouts".into(),
            },
        ),
        (
            now - Duration::minutes(18),
            EventKind::GitDirtyChanged {
                repo: repo.into(),
                from: 0,
                to: 3,
            },
        ),
        (tests, started_kind(950, "cargo test --workspace", repo)),
    ];
    let events: Vec<Event> = kinds
        .into_iter()
        .enumerate()
        .map(|(i, (ts, kind))| Event {
            seq: u64::try_from(i).unwrap_or_default() + 1,
            ts,
            kind,
        })
        .collect();
    db.insert_agent_events(vm, agent_id, &events).await?;
    db.record_snapshot(vm, "agent", &snapshot, &[]).await?;

    if let Some(session) = db.latest_session(vm).await? {
        db.rename_session(session.id, "Retry backoff for supplier timeouts")
            .await?;
        db.add_note(
            vm,
            Some(session.id),
            "Integration test against the supplier stub still flaky; rerun before the PR",
        )
        .await?;
    }
    Ok(())
}

fn started_kind(pid: u32, cmdline: &str, cwd: &str) -> EventKind {
    EventKind::ProcessStarted {
        pid,
        cmdline: cmdline.into(),
        cwd: Some(cwd.into()),
    }
}

/// A staging host reached through the bastion, collected over SSH only.
async fn staging_api(db: &Db, vm: VmId, now: DateTime<Utc>) -> Result<(), vmdesk_db::Error> {
    let app = "/srv/orders-api";
    let up = now - Duration::days(6);
    let snapshot = Snapshot {
        schema_version: SNAPSHOT_SCHEMA,
        taken_at: Some(now - Duration::minutes(3)),
        system: system("staging-api", now, 41, 4),
        processes: vec![process(
            2100,
            1,
            "./orders-api --config staging.toml",
            app,
            now - Duration::hours(5),
        )],
        containers: vec![container(
            "orders-db",
            "postgres:16",
            "Up 6 days (healthy)",
            "5432/tcp",
            "orders",
        )],
        tmux: vec![TmuxSession {
            name: "deploy".into(),
            windows: 2,
            attached: false,
            created: secs(up),
            pane_cwd: Some(app.into()),
            pane_command: Some("bash".into()),
        }],
        listeners: vec![listener(8080, "orders-api", 2100)],
        repos: vec![RepoInfo {
            path: app.into(),
            branch: Some("release/2.4".into()),
            dirty: 0,
            ahead: Some(0),
            behind: Some(0),
            last_commit: Some(Commit {
                sha: "c03d55e1f9a8".into(),
                time: secs(now - Duration::hours(6)),
                subject: "Release 2.4.1".into(),
            }),
        }],
        unavailable: vec!["docker: permission denied".into()],
    };
    let mut earlier = snapshot.clone();
    earlier.taken_at = Some(now - Duration::hours(6));
    earlier.processes.clear();
    earlier.listeners.clear();
    db.record_snapshot(vm, "ssh", &earlier, &[]).await?;
    let events = vec![
        (
            now - Duration::hours(5),
            started_kind(2100, "./orders-api --config staging.toml", app),
        ),
        (
            now - Duration::hours(5),
            EventKind::ListenerOpened {
                proto: "tcp".into(),
                addr: "0.0.0.0".into(),
                port: 8080,
                process: Some("orders-api".into()),
            },
        ),
    ];
    db.record_snapshot(vm, "ssh", &snapshot, &events).await?;
    Ok(())
}

async fn bastion_host(db: &Db, vm: VmId, now: DateTime<Utc>) -> Result<(), vmdesk_db::Error> {
    let snapshot = Snapshot {
        schema_version: SNAPSHOT_SCHEMA,
        taken_at: Some(now - Duration::minutes(9)),
        system: system("bastion", now, 120, 2),
        unavailable: vec!["docker: not installed".into()],
        ..Snapshot::default()
    };
    db.record_snapshot(vm, "ssh", &snapshot, &[]).await?;
    Ok(())
}
