//! The agent's files under a home directory: `.local/share/vmdesk/agent/` (a fixed path,
//! not XDG, so the laptop can address it in shell commands) and the systemd user unit.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use uuid::Uuid;

pub struct Home {
    home: PathBuf,
}

impl Home {
    pub fn current() -> Result<Self> {
        Ok(Self::at(dirs::home_dir().context("no home directory")?))
    }

    pub fn at(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn dir(&self) -> PathBuf {
        self.home.join(".local/share/vmdesk/agent")
    }

    pub fn binary(&self) -> PathBuf {
        self.dir().join("vmdesk-agent")
    }

    pub fn events_db(&self) -> PathBuf {
        self.dir().join("events.db")
    }

    pub fn log(&self) -> PathBuf {
        self.dir().join("daemon.log")
    }

    pub fn unit_file(&self) -> PathBuf {
        self.home.join(".config/systemd/user/vmdesk-agent.service")
    }

    fn lock_file(&self) -> PathBuf {
        self.dir().join("daemon.lock")
    }

    /// Creates the agent directory. The mode is reset even when it exists, since an SFTP
    /// upload of the binary may have created it with the default one.
    pub fn ensure_dir(&self) -> Result<()> {
        let dir = self.dir();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    /// The agent's id, generated on first use.
    pub fn agent_id(&self) -> Result<Uuid> {
        let path = self.dir().join("agent-id");
        if !path.exists() {
            // Written aside and hard-linked into place: concurrent first runs agree on one
            // id, and a crash never leaves a partial file behind.
            let tmp = self.dir().join(format!("agent-id.{}", std::process::id()));
            fs::write(&tmp, format!("{}\n", Uuid::now_v7()))?;
            let linked = fs::hard_link(&tmp, &path);
            fs::remove_file(&tmp)?;
            match linked {
                Err(e) if e.kind() != ErrorKind::AlreadyExists => return Err(e.into()),
                _ => {}
            }
        }
        let text = fs::read_to_string(&path)?;
        text.trim()
            .parse()
            .with_context(|| format!("{} is not a UUID", path.display()))
    }

    /// Takes the daemon lock and writes our pid into it, or `None` when another process
    /// holds it. The lock lasts as long as the returned file is open.
    pub fn lock_daemon(&self) -> Result<Option<File>> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.lock_file())?;
        match file.try_lock() {
            Ok(()) => {
                file.set_len(0)?;
                writeln!(file, "{}", std::process::id())?;
                Ok(Some(file))
            }
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// The pid of the running daemon, when one holds the lock (0 if it has not written
    /// its pid yet). Checking takes the lock for an instant when it is free.
    pub fn running_daemon(&self) -> Result<Option<u32>> {
        let file = match File::open(self.lock_file()) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match file.try_lock() {
            Ok(()) => Ok(None),
            Err(TryLockError::WouldBlock) => {
                let pid = fs::read_to_string(self.lock_file()).unwrap_or_default();
                Ok(Some(pid.trim().parse().unwrap_or(0)))
            }
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

/// A scratch home directory for tests, removed on drop.
#[cfg(test)]
pub struct TempHome(pub PathBuf);

#[cfg(test)]
impl TempHome {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("vmdesk-agent-{}", Uuid::now_v7()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn home(&self) -> Home {
        let home = Home::at(self.0.clone());
        home.ensure_dir().unwrap();
        home
    }
}

#[cfg(test)]
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_is_stable_and_private_dir() {
        let tmp = TempHome::new();
        let home = tmp.home();
        let id = home.agent_id().unwrap();
        assert_eq!(
            home.agent_id().unwrap(),
            id,
            "generated once, then read back"
        );
        let mode = fs::metadata(home.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let tmp = TempHome::new();
        let home = tmp.home();
        assert_eq!(home.running_daemon().unwrap(), None, "no lock file yet");
        let held = home.lock_daemon().unwrap().expect("free lock");
        assert!(
            home.lock_daemon().unwrap().is_none(),
            "second daemon refused"
        );
        assert_eq!(home.running_daemon().unwrap(), Some(std::process::id()));
        drop(held);
        assert_eq!(home.running_daemon().unwrap(), None);
        assert!(
            home.lock_daemon().unwrap().is_some(),
            "free again after release"
        );
    }
}
