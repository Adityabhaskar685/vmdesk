//! The laptop's agent commands run by a real `/bin/sh` against a temporary `$HOME`,
//! with a shell script standing in for the agent binary. Proves the quoting, the
//! upload through stdin, the checksum and the atomic replace, not just the protocol.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;
use vmdesk_collect::agent::AgentProbe;
use vmdesk_collect::agent_install::{install, uninstall};
use vmdesk_core::{ByteStream, CredentialRef, EndpointKind, ExecOutput, RemoteExec, Result};
use vmdesk_db::{Db, EndpointInput, NewVm};
use vmdesk_proto::{Event, EventKind, PROTO_VERSION, Snapshot, SystemInfo, WireMsg};

/// Runs commands with the local shell, as a VM's login shell would.
struct LocalShell {
    home: PathBuf,
}

impl LocalShell {
    fn command(&self, command: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command).env("HOME", &self.home);
        c
    }
}

#[async_trait]
impl RemoteExec for LocalShell {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        let out = self.command(command).output().await?;
        Ok(ExecOutput {
            status: out.status.code().and_then(|c| u32::try_from(c).ok()),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    async fn exec_io(&self, command: &str) -> Result<Box<dyn ByteStream>> {
        let mut child = self
            .command(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let (mut stdin, mut stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let (laptop, vm) = tokio::io::duplex(64 * 1024);
        let (mut from_laptop, mut to_laptop) = tokio::io::split(vm);
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut from_laptop, &mut stdin).await;
            // Dropping stdin is the EOF the command waits for.
        });
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stdout, &mut to_laptop).await;
            let _ = to_laptop.shutdown().await;
            let _ = child.wait().await;
        });
        Ok(Box::new(laptop))
    }
}

fn line(msg: &WireMsg) -> String {
    serde_json::to_string(msg).unwrap()
}

/// A shell script that answers like `vmdesk-agent`, with one recorded event.
fn agent_script(agent_id: Uuid) -> String {
    let hello = line(&WireMsg::Hello {
        agent_id,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        proto: PROTO_VERSION,
        caps: vec![],
        oldest_seq: 1,
        latest_seq: 1,
        daemon: true,
    });
    let event = line(&WireMsg::Event(Event {
        seq: 1,
        ts: Utc::now(),
        kind: EventKind::TmuxCreated {
            name: "work".into(),
            pane_cwd: Some("/home/u/src".into()),
        },
    }));
    let snapshot = line(&WireMsg::Snapshot {
        seq: 1,
        data: Box::new(Snapshot {
            system: SystemInfo {
                hostname: "scripted".into(),
                ..SystemInfo::default()
            },
            ..Snapshot::default()
        }),
    });
    format!(
        r#"#!/bin/sh
dir=$(dirname "$0")
case "$1" in
  version) echo {version} ;;
  install) echo "$1" >> "$dir/calls"; echo nohup ;;
  uninstall) rm -rf "$dir"; echo uninstalled ;;
  stream)
    printf '%s\n' '{hello}'
    read -r resume; read -r request
    printf '%s\n' "$resume" > "$dir/resume"
    printf '%s\n' '{event}'
    printf '%s\n' '{snapshot}'
    cat > /dev/null ;;
  *) echo "unknown command $1" >&2; exit 2 ;;
esac
"#,
        version = env!("CARGO_PKG_VERSION"),
    )
}

async fn db_with_vm() -> (Db, vmdesk_core::VmId) {
    let db = Db::open_in_memory().await.unwrap();
    let vm = db
        .create_vm(NewVm {
            name: "box".into(),
            username: "u".into(),
            credential: CredentialRef::SshAgent,
            endpoints: vec![EndpointInput {
                address: "10.0.0.1".into(),
                port: 22,
                kind: EndpointKind::Private,
                bind_interface: None,
            }],
            paths: None,
            tags: vec![],
            favourite: false,
        })
        .await
        .unwrap();
    (db, vm.id)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn install_upgrade_and_uninstall_with_a_real_shell() {
    let shell = LocalShell {
        home: std::env::temp_dir().join(format!("vmdesk-agent-home-{}", Uuid::now_v7())),
    };
    std::fs::create_dir_all(&shell.home).unwrap();
    let arch = shell.exec("uname -m").await.unwrap();
    if String::from_utf8_lossy(&arch.stdout).trim() != "x86_64" {
        eprintln!("skipped: the install path only accepts x86_64 hosts");
        return;
    }
    let dir = shell.home.join(".local/share/vmdesk/agent");
    let (db, vm) = db_with_vm().await;
    let agent_id = Uuid::now_v7();
    let binary = agent_script(agent_id);

    let done = install(&db, &shell, vm, binary.as_bytes()).await.unwrap();
    assert_eq!(done.supervisor, "nohup");
    let AgentProbe::Ok(synced) = done.probe else {
        panic!("expected a sync, got {:?}", done.probe);
    };
    assert_eq!(synced.snapshot.system.hostname, "scripted");
    assert_eq!(synced.new_events, 1);
    assert_eq!(
        std::fs::read(dir.join("vmdesk-agent")).unwrap(),
        binary.as_bytes()
    );
    assert_eq!(mode(&dir), 0o700, "agent directory is private");
    assert_eq!(mode(&dir.join("vmdesk-agent")), 0o755);
    assert!(!dir.join("vmdesk-agent.new").exists(), "renamed into place");
    assert_eq!(
        std::fs::read_to_string(dir.join("resume")).unwrap().trim(),
        line(&WireMsg::Resume { since: 0 })
    );
    let record = db.agent(vm).await.unwrap().unwrap();
    assert_eq!((record.agent_id, record.last_seq), (agent_id, 1));

    // Upgrading over a running install converges: same row, resumed after seq 1.
    install(&db, &shell, vm, binary.as_bytes()).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("calls")).unwrap(),
        "install\ninstall\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("resume")).unwrap().trim(),
        line(&WireMsg::Resume { since: 1 })
    );
    assert_eq!(db.agents().await.unwrap().len(), 1);

    uninstall(&db, &shell, vm).await.unwrap();
    assert!(!dir.exists(), "agent directory removed");
    // A leftover partial upload is cleaned up by a repeated uninstall too.
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("vmdesk-agent.new"), b"partial").unwrap();
    uninstall(&db, &shell, vm).await.unwrap();
    assert!(!dir.exists());
    assert!(db.agent(vm).await.unwrap().is_none());

    std::fs::remove_dir_all(&shell.home).ok();
}

#[tokio::test]
async fn a_binary_the_vm_cannot_run_is_not_put_in_place() {
    let shell = LocalShell {
        home: std::env::temp_dir().join(format!("vmdesk-agent-home-{}", Uuid::now_v7())),
    };
    std::fs::create_dir_all(&shell.home).unwrap();
    let arch = shell.exec("uname -m").await.unwrap();
    if String::from_utf8_lossy(&arch.stdout).trim() != "x86_64" {
        return;
    }
    let dir = shell.home.join(".local/share/vmdesk/agent");
    let (db, vm) = db_with_vm().await;
    let good = agent_script(Uuid::now_v7());
    install(&db, &shell, vm, good.as_bytes()).await.unwrap();

    let err = install(&db, &shell, vm, b"\x7fELF not really")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("remote command failed"), "{err}");
    assert_eq!(
        std::fs::read(dir.join("vmdesk-agent")).unwrap(),
        good.as_bytes(),
        "the working agent is kept"
    );
    std::fs::remove_dir_all(&shell.home).ok();
}
