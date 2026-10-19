//! Installing, upgrading and removing a VM's agent from the laptop (spec section 10).
//! Every step converges, so a command interrupted at any point is fixed by rerunning it.

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vmdesk_core::{AgentState, Error, RemoteExec, Result, VmId};
use vmdesk_db::Db;

use crate::agent::{self, AgentProbe, db_err, in_dir, run};

/// The only architecture an agent is built for so far.
pub const ARCH: &str = "x86_64";

/// What an install did.
#[derive(Debug)]
pub struct Installed {
    /// How the daemon runs on the VM: `systemd` or `nohup`.
    pub supervisor: String,
    /// The sync that followed.
    pub probe: AgentProbe,
}

/// Installs `binary` (a static build of `vmdesk-agent`) as the VM's agent, or upgrades
/// the one there, starts its daemon and syncs once.
pub async fn install(
    db: &Db,
    session: &dyn RemoteExec,
    vm: VmId,
    binary: &[u8],
) -> Result<Installed> {
    let arch = run(session, "uname -m").await?;
    if arch != ARCH {
        return Err(Error::Other(format!(
            "the VM is {arch}; the agent is only built for {ARCH}"
        )));
    }
    upload(session, binary).await?;
    let expected = hex::encode(Sha256::digest(binary));
    let sum = run(session, &in_dir(r#"sha256sum "$D/vmdesk-agent.new""#)).await?;
    if sum.split_whitespace().next() != Some(expected.as_str()) {
        let _ = session
            .exec(&in_dir(r#"rm -f "$D/vmdesk-agent.new""#))
            .await;
        return Err(Error::Other(format!(
            "the upload arrived damaged (sha256 {sum:?} on the VM, {expected} sent)"
        )));
    }
    // Running the new binary before the rename keeps a working agent in place when the
    // upload is not something the VM can run.
    run(
        session,
        &in_dir(
            r#"chmod 755 "$D/vmdesk-agent.new" && "$D/vmdesk-agent.new" version >/dev/null && mv -f "$D/vmdesk-agent.new" "$D/vmdesk-agent""#,
        ),
    )
    .await?;
    let supervisor = run(session, &in_dir(r#""$D/vmdesk-agent" install"#)).await?;

    let probe = agent::sync(db, session, vm).await?;
    let failed = match &probe {
        AgentProbe::Absent => Some("the binary is missing"),
        AgentProbe::Error(e) => Some(e.as_str()),
        _ => None,
    };
    if let Some(why) = failed {
        return Err(Error::Other(format!(
            "installed, but the agent did not answer: {why}"
        )));
    }
    if let Some(mut record) = db.agent(vm).await.map_err(db_err)? {
        record.installed_at = Some(Utc::now());
        db.save_agent(&record, None).await.map_err(db_err)?;
    }
    Ok(Installed { supervisor, probe })
}

/// Stops and removes the agent from the VM, then forgets it on the laptop. The VM's
/// history (events and snapshots) is kept.
pub async fn uninstall(db: &Db, session: &dyn RemoteExec, vm: VmId) -> Result<()> {
    run(
        session,
        &in_dir(
            r#"if [ -x "$D/vmdesk-agent" ]; then "$D/vmdesk-agent" uninstall; else rm -rf "$D"; echo uninstalled; fi"#,
        ),
    )
    .await?;
    db.delete_agent(vm).await.map_err(db_err)?;
    db.set_agent_state(vm, AgentState::Absent)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Writes `binary` to `vmdesk-agent.new` in the agent's directory, through the
/// command's stdin.
async fn upload(session: &dyn RemoteExec, binary: &[u8]) -> Result<()> {
    let mut io = session
        .exec_io(&in_dir(
            r#"umask 077; mkdir -p "$D" && cat > "$D/vmdesk-agent.new""#,
        ))
        .await?;
    io.write_all(binary).await?;
    io.shutdown().await?;
    // The command's output ends when `cat` has written the whole file and exited.
    let mut rest = Vec::new();
    io.read_to_end(&mut rest).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_vm::{FakeAgent, FakeVm, agent_state, db_with_vm};

    fn binary() -> Vec<u8> {
        (0..=255u8).cycle().take(100_000).collect()
    }

    #[tokio::test]
    async fn install_verifies_the_upload_then_syncs_and_upgrade_converges() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(None);

        let done = install(&db, &fake, vm.id, &binary()).await.unwrap();
        assert_eq!(done.supervisor, "systemd");
        assert!(matches!(done.probe, AgentProbe::Ok(_)), "{:?}", done.probe);
        assert_eq!(
            fake.uploaded.lock().unwrap().as_deref(),
            Some(&binary()[..])
        );
        let record = db.agent(vm.id).await.unwrap().unwrap();
        assert!(record.installed_at.is_some());
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Ok);

        // An upgrade runs the same steps; the agent keeps its id and the one row.
        install(&db, &fake, vm.id, &binary()).await.unwrap();
        let agents = db.agents().await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, record.agent_id);
    }

    #[tokio::test]
    async fn install_refuses_other_architectures() {
        let (db, vm) = db_with_vm().await;
        let mut fake = FakeVm::new(None);
        fake.arch = "aarch64";
        let err = install(&db, &fake, vm.id, &binary()).await.unwrap_err();
        assert!(err.to_string().contains("aarch64"), "{err}");
        assert!(fake.uploaded.lock().unwrap().is_none(), "nothing uploaded");
        assert!(db.agent(vm.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_damaged_upload_never_replaces_the_agent() {
        let (db, vm) = db_with_vm().await;
        let mut fake = FakeVm::new(Some(FakeAgent::recording(1..=1)));
        fake.damage_uploads = true;
        let err = install(&db, &fake, vm.id, &binary()).await.unwrap_err();
        assert!(err.to_string().contains("damaged"), "{err}");
        assert!(!fake.ran("mv -f"), "the binary in place is kept");
        assert!(fake.ran("rm -f"), "the bad upload is removed");
    }

    #[tokio::test]
    async fn uninstall_keeps_history_and_can_be_repeated() {
        let (db, vm) = db_with_vm().await;
        let fake = FakeVm::new(None);
        install(&db, &fake, vm.id, &binary()).await.unwrap();
        fake.agent().as_mut().unwrap().record(2);
        agent::sync(&db, &fake, vm.id).await.unwrap();

        uninstall(&db, &fake, vm.id).await.unwrap();
        assert!(fake.agent().is_none(), "removed from the VM");
        assert!(db.agent(vm.id).await.unwrap().is_none());
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Absent);
        let kept = db.timeline(vm.id, 100, None).await.unwrap();
        assert_eq!(
            kept.iter()
                .filter(|e| e.source.starts_with("agent:"))
                .count(),
            2
        );
        assert!(db.latest_snapshot(vm.id).await.unwrap().is_some());

        uninstall(&db, &fake, vm.id).await.unwrap();
        assert_eq!(agent_state(&db, vm.id).await, AgentState::Absent);
    }
}
