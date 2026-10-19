//! `install`, `start` and `uninstall`: keep one daemon running, under `systemd --user`
//! where the user manager is reachable, otherwise detached with `setsid`.
//!
//! Each step derives what to do from what is running now (the lock holder, the unit
//! file), so every command is safe to repeat and to rerun after a crash.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::home::Home;

const UNIT_NAME: &str = "vmdesk-agent.service";
const UNIT: &str = "\
[Unit]
Description=VMDesk agent: collector and local event log

[Service]
ExecStart=%h/.local/share/vmdesk/agent/vmdesk-agent daemon
Restart=on-failure

[Install]
WantedBy=default.target
";
/// When set, `systemctl --user` is not used even where it works.
pub const NOHUP_ENV: &str = "VMDESK_AGENT_NOHUP";
const POLL: Duration = Duration::from_millis(100);

/// Sets up the daemon and (re)starts it; returns the supervisor used.
pub fn install(home: &Home) -> Result<&'static str> {
    ensure_binary(home)?;
    // Whatever runs now goes first: an older binary, or a nohup daemon that would
    // otherwise keep the lock from the systemd one.
    stop(home)?;
    if systemd_usable() {
        write_unit(home)?;
        if let Err(e) = run("loginctl", &["enable-linger"]) {
            tracing::warn!(error = %e, "lingering not enabled; the daemon stops at logout");
        }
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", UNIT_NAME])?;
        systemctl(&["restart", UNIT_NAME])?;
        wait_running(home, "journalctl --user -u vmdesk-agent")?;
        Ok("systemd")
    } else {
        spawn(home)?;
        Ok("nohup")
    }
}

pub fn start(home: &Home) -> Result<&'static str> {
    if home.running_daemon()?.is_some() {
        return Ok("running");
    }
    if home.unit_file().exists() && systemd_usable() {
        systemctl(&["start", UNIT_NAME])?;
        wait_running(home, "journalctl --user -u vmdesk-agent")?;
        return Ok("systemd");
    }
    ensure_binary(home)?;
    spawn(home)?;
    Ok("nohup")
}

pub fn uninstall(home: &Home) -> Result<()> {
    let unit = home.unit_file();
    if unit.exists() {
        let systemd = systemd_usable();
        if systemd && let Err(e) = systemctl(&["disable", "--now", UNIT_NAME]) {
            tracing::warn!(error = %e, "disabling the unit");
        }
        fs::remove_file(&unit).with_context(|| format!("removing {}", unit.display()))?;
        if systemd && let Err(e) = systemctl(&["daemon-reload"]) {
            tracing::warn!(error = %e, "reloading systemd");
        }
    }
    stop(home)?;
    match fs::remove_dir_all(home.dir()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", home.dir().display()))
        }
        _ => Ok(()),
    }
}

fn ensure_binary(home: &Home) -> Result<()> {
    let bin = home.binary();
    ensure!(bin.is_file(), "{} not found", bin.display());
    Ok(())
}

fn systemd_usable() -> bool {
    std::env::var_os(NOHUP_ENV).is_none() && systemctl(&["show-environment"]).is_ok()
}

fn write_unit(home: &Home) -> Result<()> {
    let path = home.unit_file();
    if fs::read_to_string(&path).is_ok_and(|s| s == UNIT) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("service.tmp");
    fs::write(&tmp, UNIT)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Starts `vmdesk-agent daemon` in its own session, so it outlives the SSH connection.
fn spawn(home: &Home) -> Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(home.log())?;
    let status = Command::new("setsid")
        .arg("-f")
        .arg(home.binary())
        .arg("daemon")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .context("running setsid")?;
    ensure!(status.success(), "setsid exited with {status}");
    wait_running(home, &home.log().display().to_string())
}

fn wait_running(home: &Home, logs: &str) -> Result<()> {
    for _ in 0..50 {
        if home.running_daemon()?.is_some() {
            return Ok(());
        }
        sleep(POLL);
    }
    bail!("the daemon did not start; see {logs}")
}

/// Stops the daemon holding the lock, if any: SIGTERM, then SIGKILL after 10 s.
fn stop(home: &Home) -> Result<()> {
    let mut sent = None;
    for i in 0..120 {
        let Some(pid) = home.running_daemon()? else {
            return Ok(());
        };
        // The pid is checked because the lock file may still name a previous daemon
        // for an instant after a new one takes the lock.
        let signal = if i < 100 { "TERM" } else { "KILL" };
        if sent != Some(signal) && is_agent(pid) {
            run("kill", &[&format!("-{signal}"), &pid.to_string()])?;
            sent = Some(signal);
        }
        sleep(POLL);
    }
    bail!("the running daemon did not stop")
}

fn is_agent(pid: u32) -> bool {
    pid != 0
        && fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|c| String::from_utf8_lossy(&c).contains("vmdesk-agent"))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    run("systemctl", &all)
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    ensure!(
        out.status.success(),
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}
