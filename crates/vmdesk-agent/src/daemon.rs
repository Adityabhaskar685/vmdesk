//! `vmdesk-agent daemon`: every 30 s, snapshots this machine with the SSH collector's
//! probe script run locally, and records the diff from the previous snapshot as events.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{Instant, MissedTickBehavior};
use vmdesk_collect::probe::MARKER;
use vmdesk_collect::{PROBE_SCRIPT, diff, parse};
use vmdesk_proto::Snapshot;

use crate::home::Home;
use crate::store::{Retention, Store};

pub const INTERVAL: Duration = Duration::from_secs(30);
/// Under high load ticks are skipped, but a snapshot is still taken this often.
const BACKOFF_MAX: Duration = Duration::from_mins(5);
const PROBE_TIMEOUT: Duration = Duration::from_mins(1);

/// One snapshot of this machine, from the same script and parser as the SSH collector.
pub async fn take_snapshot() -> Result<Snapshot> {
    let taken_at = Utc::now();
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(PROBE_SCRIPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("running /bin/sh")?;
    let out = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .context("probe timed out")??;
    let text = String::from_utf8_lossy(&out.stdout);
    if !text.contains(MARKER) {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "probe produced no output ({}): {}",
            out.status,
            err.lines().next().unwrap_or_default()
        );
    }
    Ok(parse(&text, taken_at))
}

pub async fn run(home: &Home) -> Result<()> {
    home.ensure_dir()?;
    let Some(_lock) = lock(home).await? else {
        tracing::info!("another daemon holds the lock; exiting");
        return Ok(());
    };
    let store = Store::open(&home.events_db(), Retention::DEFAULT).await?;
    let mut prev = store.latest().await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "unreadable latest snapshot; starting without one");
        None
    });
    tracing::info!(
        pid = std::process::id(),
        resumed = prev.is_some(),
        "daemon started"
    );

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut tick = tokio::time::interval(INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_taken: Option<Instant> = None;
    let mut last_error: Option<String> = None;
    loop {
        let work = async {
            tick.tick().await;
            if overloaded() && last_taken.is_some_and(|t| t.elapsed() < BACKOFF_MAX) {
                tracing::debug!("load is high; skipping this tick");
                return;
            }
            last_taken = Some(Instant::now());
            match collect(&store, &mut prev).await {
                Ok(n) => {
                    if last_error.take().is_some() {
                        tracing::info!("collecting again");
                    }
                    tracing::debug!(events = n, "snapshot recorded");
                }
                // Logged once per distinct failure, so a lasting one does not fill the log.
                Err(e) => {
                    let msg = format!("{e:#}");
                    if last_error.as_ref() != Some(&msg) {
                        tracing::warn!(error = %msg, "collection failed; retrying every tick");
                        last_error = Some(msg);
                    }
                }
            }
        };
        // A signal cancels a tick mid-way: an unfinished transaction rolls back, and the
        // probe's shell is killed.
        tokio::select! {
            () = work => {}
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    store.close().await;
    tracing::info!("daemon stopped");
    Ok(())
}

/// Takes the daemon lock, retrying briefly: `status` and `stream` take it for an instant
/// to see whether it is free, and a daemon starting then must not mistake them for a
/// running daemon.
async fn lock(home: &Home) -> Result<Option<std::fs::File>> {
    for _ in 0..3 {
        if let Some(file) = home.lock_daemon()? {
            return Ok(Some(file));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(None)
}

async fn collect(store: &Store, prev: &mut Option<Snapshot>) -> Result<usize> {
    let next = take_snapshot().await?;
    let events = prev.as_ref().map(|p| diff(p, &next)).unwrap_or_default();
    store.record(&next, &events).await?;
    *prev = Some(next);
    Ok(events.len())
}

/// Whether the 1-minute load average is above twice the number of CPUs.
fn overloaded() -> bool {
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    load.is_some_and(|l| l > 2.0 * f64::from(u32::try_from(cpus).unwrap_or(u32::MAX)))
}
