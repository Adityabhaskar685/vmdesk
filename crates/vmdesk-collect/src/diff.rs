//! Turns two consecutive snapshots into timeline events (spec section 11). The SSH
//! collector only sees what exists at each poll, so anything that starts and ends
//! between two polls is missed; the agent (M6) closes that gap.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use vmdesk_proto::{EventKind, ProcessInfo, Snapshot};

/// Polls further apart than this leave a `gap` in the timeline.
pub const GAP_AFTER: Duration = Duration::minutes(45);
/// A process becomes an event once it has run this long (or matches the watchlist).
const LONG_LIVED_SECS: i64 = 30;
/// A boot time that moved by more than this means the VM rebooted (clock jitter aside).
const REBOOT_SLACK_SECS: i64 = 120;
/// Tolerance when matching a process's start time across snapshots.
const START_SLACK_SECS: i64 = 2;
/// Commands worth an event even when short-lived.
const WATCHLIST: [&str; 11] = [
    "pytest",
    "make",
    "cargo",
    "go",
    "kubectl",
    "helm",
    "npm",
    "mvn",
    "gradle",
    "ansible-playbook",
    "docker",
];

/// Events between `prev` and `next`, each with its timestamp.
#[must_use]
pub fn diff(prev: &Snapshot, next: &Snapshot) -> Vec<(DateTime<Utc>, EventKind)> {
    let now = next.taken_at.unwrap_or_else(Utc::now);
    let then = prev.taken_at.unwrap_or(now);
    let mut events = Vec::new();

    if now - then > GAP_AFTER {
        events.push((
            then,
            EventKind::Gap {
                from: then,
                to: now,
            },
        ));
    }
    let rebooted = prev.system.boot_time != 0
        && next.system.boot_time - prev.system.boot_time > REBOOT_SLACK_SECS;
    if rebooted {
        events.push((
            at(next.system.boot_time).unwrap_or(now),
            EventKind::VmRebooted {
                previous_boot: prev.system.boot_time,
            },
        ));
    }

    processes(prev, next, now, then, rebooted, &mut events);
    containers(prev, next, now, &mut events);
    tmux(prev, next, now, &mut events);
    repos(prev, next, now, &mut events);
    listeners(prev, next, now, &mut events);
    events.sort_by_key(|(ts, _)| *ts);
    events
}

fn at(secs: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(secs, 0)
}

/// Long-lived, on the watchlist, or running in a tmux pane.
fn interesting(p: &ProcessInfo, observed_at: i64, tmux_shells: &HashSet<u32>) -> bool {
    let prog = p.cmdline.split_whitespace().next().unwrap_or("");
    let prog = prog.rsplit('/').next().unwrap_or(prog);
    observed_at - p.started_at >= LONG_LIVED_SECS
        || WATCHLIST.contains(&prog)
        || tmux_shells.contains(&p.ppid)
}

/// Shell processes whose parent is the tmux server: commands under them run in a pane.
fn tmux_shells(s: &Snapshot) -> HashSet<u32> {
    let servers: HashSet<u32> = s
        .processes
        .iter()
        .filter(|p| p.cmdline.starts_with("tmux"))
        .map(|p| p.pid)
        .collect();
    s.processes
        .iter()
        .filter(|p| servers.contains(&p.ppid))
        .map(|p| p.pid)
        .collect()
}

fn processes(
    prev: &Snapshot,
    next: &Snapshot,
    now: DateTime<Utc>,
    then: DateTime<Utc>,
    rebooted: bool,
    events: &mut Vec<(DateTime<Utc>, EventKind)>,
) {
    let (prev_tmux, next_tmux) = (tmux_shells(prev), tmux_shells(next));
    let before: HashMap<u32, &ProcessInfo> = prev
        .processes
        .iter()
        .filter(|p| interesting(p, then.timestamp(), &prev_tmux))
        .map(|p| (p.pid, p))
        .collect();
    let after: HashMap<u32, &ProcessInfo> = next
        .processes
        .iter()
        .filter(|p| interesting(p, now.timestamp(), &next_tmux))
        .map(|p| (p.pid, p))
        .collect();
    // A pid is only the same process if it also started at (about) the same time;
    // start times are derived from whole seconds of uptime and can be off by one.
    let same =
        |a: &ProcessInfo, b: &ProcessInfo| (a.started_at - b.started_at).abs() <= START_SLACK_SECS;

    for (pid, p) in &after {
        if !before.get(pid).is_some_and(|b| same(b, p)) {
            events.push((
                at(p.started_at).unwrap_or(now),
                EventKind::ProcessStarted {
                    pid: p.pid,
                    cmdline: p.cmdline.clone(),
                    cwd: p.cwd.clone(),
                },
            ));
        }
    }
    if rebooted {
        return; // every process is gone; the reboot event says so
    }
    for (pid, p) in &before {
        if !after.get(pid).is_some_and(|a| same(a, p)) {
            let seen_until = then.timestamp();
            events.push((
                now,
                EventKind::ProcessExited {
                    pid: p.pid,
                    cmdline: p.cmdline.clone(),
                    exit_code: None,
                    duration_secs: u64::try_from(seen_until - p.started_at).ok(),
                },
            ));
        }
    }
}

fn containers(
    prev: &Snapshot,
    next: &Snapshot,
    now: DateTime<Utc>,
    events: &mut Vec<(DateTime<Utc>, EventKind)>,
) {
    let running = |s: &Snapshot| -> HashMap<String, (String, Option<String>)> {
        s.containers
            .iter()
            .filter(|c| c.state == "running")
            .map(|c| (c.name.clone(), (c.image.clone(), c.compose_project.clone())))
            .collect()
    };
    let (before, after) = (running(prev), running(next));
    for (name, (image, compose_project)) in &after {
        if !before.contains_key(name) {
            events.push((
                now,
                EventKind::ContainerStarted {
                    name: name.clone(),
                    image: image.clone(),
                    compose_project: compose_project.clone(),
                },
            ));
        }
    }
    for (name, (image, _)) in &before {
        if !after.contains_key(name) {
            events.push((
                now,
                EventKind::ContainerStopped {
                    name: name.clone(),
                    image: image.clone(),
                },
            ));
        }
    }
}

fn tmux(
    prev: &Snapshot,
    next: &Snapshot,
    now: DateTime<Utc>,
    events: &mut Vec<(DateTime<Utc>, EventKind)>,
) {
    let before: HashMap<&str, bool> = prev
        .tmux
        .iter()
        .map(|t| (t.name.as_str(), t.attached))
        .collect();
    let after: HashSet<&str> = next.tmux.iter().map(|t| t.name.as_str()).collect();
    for t in &next.tmux {
        match before.get(t.name.as_str()) {
            None => events.push((
                at(t.created).unwrap_or(now),
                EventKind::TmuxCreated {
                    name: t.name.clone(),
                    pane_cwd: t.pane_cwd.clone(),
                },
            )),
            Some(false) if t.attached => events.push((
                now,
                EventKind::TmuxAttached {
                    name: t.name.clone(),
                },
            )),
            Some(_) => {}
        }
    }
    for name in before.keys().filter(|n| !after.contains(*n)) {
        events.push((
            now,
            EventKind::TmuxClosed {
                name: (*name).to_owned(),
            },
        ));
    }
}

fn repos(
    prev: &Snapshot,
    next: &Snapshot,
    now: DateTime<Utc>,
    events: &mut Vec<(DateTime<Utc>, EventKind)>,
) {
    let before: HashMap<&str, _> = prev.repos.iter().map(|r| (r.path.as_str(), r)).collect();
    for r in &next.repos {
        // A repo seen for the first time has no history to compare with.
        let Some(old) = before.get(r.path.as_str()) else {
            continue;
        };
        if old.branch != r.branch {
            events.push((
                now,
                EventKind::GitBranchChanged {
                    repo: r.path.clone(),
                    from: old.branch.clone(),
                    to: r.branch.clone(),
                },
            ));
        } else if let Some(c) = &r.last_commit
            && old.last_commit.as_ref().map(|o| &o.sha) != Some(&c.sha)
        {
            events.push((
                at(c.time).unwrap_or(now),
                EventKind::GitCommitted {
                    repo: r.path.clone(),
                    branch: r.branch.clone(),
                    sha: c.sha.clone(),
                    subject: c.subject.clone(),
                },
            ));
        }
        if old.dirty != r.dirty {
            events.push((
                now,
                EventKind::GitDirtyChanged {
                    repo: r.path.clone(),
                    from: old.dirty,
                    to: r.dirty,
                },
            ));
        }
    }
}

fn listeners(
    prev: &Snapshot,
    next: &Snapshot,
    now: DateTime<Utc>,
    events: &mut Vec<(DateTime<Utc>, EventKind)>,
) {
    let key = |l: &vmdesk_proto::Listener| (l.proto.clone(), l.addr.clone(), l.port);
    let before: HashSet<_> = prev.listeners.iter().map(key).collect();
    let after: HashSet<_> = next.listeners.iter().map(key).collect();
    for l in &next.listeners {
        if !before.contains(&key(l)) {
            events.push((
                now,
                EventKind::ListenerOpened {
                    proto: l.proto.clone(),
                    addr: l.addr.clone(),
                    port: l.port,
                    process: l.process.clone(),
                },
            ));
        }
    }
    for l in &prev.listeners {
        if !after.contains(&key(l)) {
            events.push((
                now,
                EventKind::ListenerClosed {
                    proto: l.proto.clone(),
                    addr: l.addr.clone(),
                    port: l.port,
                },
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use vmdesk_proto::{Commit, Container, Listener, RepoInfo, SystemInfo, TmuxSession};

    use super::*;

    const T0: i64 = 1_800_000_000;

    fn proc(pid: u32, cmd: &str, started_at: i64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid: 100,
            user: "root".into(),
            cmdline: cmd.into(),
            cwd: Some("/root/regal".into()),
            started_at,
            cpu_pct: 0.0,
            rss_kb: 0,
        }
    }

    fn snap(at_secs: i64, boot: i64) -> Snapshot {
        Snapshot {
            taken_at: DateTime::from_timestamp(at_secs, 0),
            system: SystemInfo {
                boot_time: boot,
                ..SystemInfo::default()
            },
            ..Snapshot::default()
        }
    }

    fn container(name: &str, state: &str) -> Container {
        Container {
            id: name.into(),
            name: name.into(),
            image: format!("{name}:1"),
            state: state.into(),
            status: String::new(),
            ports: String::new(),
            compose_project: Some("uegnb".into()),
        }
    }

    fn repo(branch: &str, sha: &str, dirty: u32) -> RepoInfo {
        RepoInfo {
            path: "/root/regal".into(),
            branch: Some(branch.into()),
            dirty,
            ahead: None,
            behind: None,
            last_commit: Some(Commit {
                sha: sha.into(),
                time: T0 + 100,
                subject: "msg".into(),
            }),
        }
    }

    fn names(events: &[(DateTime<Utc>, EventKind)]) -> Vec<&'static str> {
        events.iter().map(|(_, e)| e.name()).collect()
    }

    #[test]
    fn detects_changes_between_polls() {
        let mut a = snap(T0, T0 - 10_000);
        let mut b = snap(T0 + 600, T0 - 10_000);
        a.processes = vec![proc(10, "vim notes", T0 - 900), proc(11, "ls", T0 - 1)];
        b.processes = vec![
            proc(10, "vim notes", T0 - 900),
            proc(12, "pytest tests/group2", T0 + 590),
            proc(13, "sleep 5", T0 + 595),
        ];
        a.containers = vec![container("uegnb", "running"), container("ui", "running")];
        b.containers = vec![
            container("uegnb", "running"),
            container("ui", "exited"),
            container("db", "running"),
        ];
        a.tmux = vec![TmuxSession {
            name: "old".into(),
            windows: 1,
            attached: false,
            created: T0 - 50,
            pane_cwd: None,
            pane_command: None,
        }];
        b.tmux = vec![TmuxSession {
            name: "nightly".into(),
            windows: 2,
            attached: true,
            created: T0 + 300,
            pane_cwd: Some("/root/regal".into()),
            pane_command: None,
        }];
        a.repos = vec![repo("main", "aaa", 0)];
        b.repos = vec![repo("main", "bbb", 2)];
        let l = |port| Listener {
            proto: "tcp".into(),
            addr: "0.0.0.0".into(),
            port,
            pid: None,
            process: None,
        };
        a.listeners = vec![l(22), l(8080)];
        b.listeners = vec![l(22), l(9090)];

        let mut got = names(&diff(&a, &b));
        got.sort_unstable();
        assert_eq!(
            got,
            [
                "container_started",
                "container_stopped",
                "git_committed",
                "git_dirty_changed",
                "listener_closed",
                "listener_opened",
                "process_started",
                "tmux_closed",
                "tmux_created",
            ],
            "ls was too short to count; sleep is neither long-lived nor on the watchlist"
        );
        assert!(diff(&a, &a).is_empty());

        // The same process, its start time off by a second: no event. A reused pid: both.
        let mut c = b.clone();
        c.taken_at = DateTime::from_timestamp(T0 + 1200, 0);
        c.processes = vec![proc(10, "vim notes", T0 - 899), proc(12, "make", T0 + 1100)];
        let mut got = names(&diff(&b, &c));
        got.sort_unstable();
        assert_eq!(
            got,
            ["process_exited", "process_started"],
            "pid 12 was reused by make"
        );
    }

    #[test]
    fn reboot_and_gap() {
        let mut a = snap(T0, T0 - 10_000);
        a.processes = vec![proc(10, "vim notes", T0 - 900)];
        let b = snap(T0 + 7200, T0 + 3600);
        let got = names(&diff(&a, &b));
        assert_eq!(
            got,
            ["gap", "vm_rebooted"],
            "no process_exited noise after a reboot"
        );

        let mut c = b.clone();
        c.taken_at = DateTime::from_timestamp(T0 + 7300, 0);
        c.repos = vec![repo("main", "aaa", 0)];
        let mut d = c.clone();
        d.taken_at = DateTime::from_timestamp(T0 + 7400, 0);
        d.repos = vec![repo("feature", "ccc", 0)];
        assert_eq!(names(&diff(&c, &d)), ["git_branch_changed"]);
    }
}
