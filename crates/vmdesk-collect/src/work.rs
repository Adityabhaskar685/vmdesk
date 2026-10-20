//! The rule-based current-work summary (spec section 11): what a VM's latest snapshot
//! and work session say you were doing there, as one line.
//!
//! A snapshot records when each process and tmux session started, and whether a
//! session is attached, but nothing about input or output. So "most recent activity"
//! means the newest start, and attachment, which means someone is looking at it now,
//! outranks any start time.

use std::collections::HashSet;
use std::fmt::Display;

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use serde::Serialize;
use vmdesk_db::{IDLE_GAP, Note, WorkSession};
use vmdesk_proto::{ProcessInfo, RepoInfo, Snapshot, TmuxSession};

use crate::diff::{interesting_processes, tmux_shells};
use crate::probe::program;

/// Longest command line the summary line shows.
const MAX_COMMAND: usize = 60;
/// Containers named in the line; the rest are counted.
const MAX_CONTAINERS: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurrentWork {
    pub anchor: Option<Anchor>,
    /// The repo holding the anchor's working directory.
    pub repo: Option<WorkRepo>,
    /// The tmux session the anchor command runs in, if it could be told.
    pub tmux: Option<String>,
    /// Running containers whose compose project or name is the repo's name.
    pub containers: Vec<String>,
    /// When the current work session started, else when the anchor did.
    pub since: Option<DateTime<Utc>>,
    /// The newest note on the VM's latest session, shown above the line.
    pub note: Option<String>,
}

/// What the summary is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Anchor {
    /// A command you started, e.g. `pytest tests/group2`.
    Process {
        pid: u32,
        cmdline: String,
        cwd: Option<String>,
    },
    /// A tmux session with no command that ranks higher.
    Tmux { name: String, cwd: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkRepo {
    pub path: String,
    pub branch: Option<String>,
    /// Files with uncommitted changes.
    pub dirty: u32,
}

impl WorkRepo {
    /// The repo's directory name, e.g. `regal` for `/root/regal`.
    #[must_use]
    pub fn name(&self) -> &str {
        self.path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&self.path)
    }
}

/// How strongly a candidate says "this is what I was doing", weakest first. Within
/// one rank the newest start wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    DetachedTmux,
    /// Any other process the snapshot diff would record.
    OtherProcess,
    /// A command whose parent is a login shell, as in a plain SSH session.
    LoginShellCommand,
    TmuxPaneCommand,
    AttachedTmux,
    AttachedTmuxCommand,
}

struct Candidate<'a> {
    rank: Rank,
    started: i64,
    anchor: Anchor,
    cwd: Option<&'a str>,
    tmux: Option<&'a TmuxSession>,
}

/// The current-work summary from the VM's latest snapshot, its latest work session
/// and its notes (spec section 11, steps 1 to 5).
///
/// The anchor is, in this order: the newest command in an attached tmux session's
/// pane, an attached tmux session, the newest command in any tmux pane, the newest
/// command started from a login shell, the newest other process that the snapshot
/// diff counts as interesting, then the newest detached tmux session. Login shells
/// and tmux's own processes are never the anchor.
#[must_use]
pub fn current_work(
    latest: &Snapshot,
    session: Option<&WorkSession>,
    notes: &[Note],
) -> CurrentWork {
    let taken = latest.taken_at.unwrap_or_else(Utc::now);
    let best = candidates(latest, taken.timestamp())
        .into_iter()
        .max_by_key(|c| (c.rank, c.started));
    let repo = best
        .as_ref()
        .and_then(|c| c.cwd)
        .and_then(|cwd| repo_of(cwd, &latest.repos))
        .map(|r| WorkRepo {
            path: r.path.clone(),
            branch: r.branch.clone(),
            dirty: r.dirty,
        });
    let containers = repo
        .as_ref()
        .map_or_else(Vec::new, |r| containers_of(r, latest));
    let current = session.filter(|s| s.ended_at >= taken - IDLE_GAP);
    let since = current.map(|s| s.started_at).or_else(|| {
        best.as_ref()
            .and_then(|c| DateTime::from_timestamp(c.started, 0))
    });
    let note = session.and_then(|s| {
        notes
            .iter()
            .filter(|n| n.session == Some(s.id))
            .max_by_key(|n| n.created_at)
            .map(|n| n.body.clone())
    });
    CurrentWork {
        tmux: best.as_ref().and_then(|c| c.tmux).map(|t| t.name.clone()),
        anchor: best.map(|c| c.anchor),
        repo,
        containers,
        since,
        note,
    }
}

fn candidates(s: &Snapshot, observed_at: i64) -> Vec<Candidate<'_>> {
    let panes = tmux_shells(s);
    let login_shells: HashSet<u32> = s
        .processes
        .iter()
        .filter(|p| p.cmdline.starts_with('-'))
        .map(|p| p.pid)
        .collect();
    let mut out: Vec<Candidate<'_>> = interesting_processes(s, observed_at)
        .filter(|p| !p.cmdline.starts_with('-') && program(&p.cmdline) != "tmux")
        .map(|p| {
            let tmux = panes
                .contains(&p.ppid)
                .then(|| tmux_of(p, &s.tmux))
                .flatten();
            let rank = match tmux {
                Some(t) if t.attached => Rank::AttachedTmuxCommand,
                _ if panes.contains(&p.ppid) => Rank::TmuxPaneCommand,
                _ if login_shells.contains(&p.ppid) => Rank::LoginShellCommand,
                _ => Rank::OtherProcess,
            };
            Candidate {
                rank,
                started: p.started_at,
                anchor: Anchor::Process {
                    pid: p.pid,
                    cmdline: p.cmdline.clone(),
                    cwd: p.cwd.clone(),
                },
                cwd: p.cwd.as_deref(),
                tmux,
            }
        })
        .collect();
    out.extend(s.tmux.iter().map(|t| Candidate {
        rank: if t.attached {
            Rank::AttachedTmux
        } else {
            Rank::DetachedTmux
        },
        started: t.created,
        anchor: Anchor::Tmux {
            name: t.name.clone(),
            cwd: t.pane_cwd.clone(),
        },
        cwd: t.pane_cwd.as_deref(),
        tmux: None,
    }));
    out
}

/// The tmux session a pane command runs in. The snapshot does not link panes to
/// sessions, so this is the only session, or the one whose active pane is in the
/// command's directory, preferring one running the same program.
fn tmux_of<'a>(p: &ProcessInfo, sessions: &'a [TmuxSession]) -> Option<&'a TmuxSession> {
    if let [only] = sessions {
        return Some(only);
    }
    sessions
        .iter()
        .filter(|t| t.pane_cwd.is_some() && t.pane_cwd == p.cwd)
        .max_by_key(|t| {
            let same_program = t.pane_command.as_deref().map(program) == Some(program(&p.cmdline));
            (same_program, t.attached, t.created)
        })
}

/// The deepest repo containing `cwd`.
fn repo_of<'a>(cwd: &str, repos: &'a [RepoInfo]) -> Option<&'a RepoInfo> {
    repos
        .iter()
        .filter(|r| {
            cwd.strip_prefix(r.path.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        .max_by_key(|r| r.path.len())
}

fn containers_of(repo: &WorkRepo, s: &Snapshot) -> Vec<String> {
    let name = repo.name().to_ascii_lowercase();
    let belongs = |c: &vmdesk_proto::Container| {
        let own = c.name.to_ascii_lowercase();
        c.compose_project
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(&name))
            || own == name
            || own
                .strip_prefix(&name)
                .is_some_and(|rest| rest.starts_with(['-', '_']))
    };
    let mut names: Vec<String> = s
        .containers
        .iter()
        .filter(|c| c.state == "running" && belongs(c))
        .map(|c| c.name.clone())
        .collect();
    names.sort();
    names
}

impl CurrentWork {
    /// The summary line in local time, e.g. `pytest tests/group2 in regal @
    /// group2-fix (3 modified) · tmux nightly-debug · since 21:07`.
    #[must_use]
    pub fn line(&self) -> String {
        self.line_in(&Local, Utc::now())
    }

    /// [`line`](Self::line) in time zone `tz`, with times on `now`'s day shown
    /// without a date.
    #[must_use]
    pub fn line_in<Tz: TimeZone>(&self, tz: &Tz, now: DateTime<Utc>) -> String
    where
        Tz::Offset: Display,
    {
        let (what, cwd) = match &self.anchor {
            Some(Anchor::Process { cmdline, cwd, .. }) => (shorten(cmdline), cwd.as_deref()),
            Some(Anchor::Tmux { name, cwd }) => (format!("tmux {name}"), cwd.as_deref()),
            None => ("nothing running".to_owned(), None),
        };
        let place = match (&self.repo, cwd) {
            (Some(r), _) => {
                let branch = r.branch.as_ref().map(|b| format!(" @ {b}"));
                let dirty = (r.dirty > 0).then(|| format!(" ({} modified)", r.dirty));
                format!(
                    " in {}{}{}",
                    r.name(),
                    branch.unwrap_or_default(),
                    dirty.unwrap_or_default()
                )
            }
            (None, Some(cwd)) => format!(" in {cwd}"),
            (None, None) => String::new(),
        };
        let mut parts = vec![format!("{what}{place}")];
        if let Some(t) = &self.tmux {
            parts.push(format!("tmux {t}"));
        }
        if !self.containers.is_empty() {
            let shown = &self.containers[..self.containers.len().min(MAX_CONTAINERS)];
            let more = (self.containers.len() > MAX_CONTAINERS)
                .then(|| format!(" +{}", self.containers.len() - MAX_CONTAINERS));
            parts.push(format!(
                "containers {}{}",
                shown.join(", "),
                more.unwrap_or_default()
            ));
        }
        if let Some(since) = self.since {
            let (at, today) = (since.with_timezone(tz), now.with_timezone(tz));
            let format = if at.date_naive() == today.date_naive() {
                "%H:%M"
            } else if at.year() == today.year() {
                "%-d %b %H:%M"
            } else {
                "%-d %b %Y %H:%M"
            };
            parts.push(format!("since {}", at.format(format)));
        }
        parts.join(" · ")
    }
}

fn shorten(cmdline: &str) -> String {
    if cmdline.chars().count() <= MAX_COMMAND {
        return cmdline.to_owned();
    }
    let mut short: String = cmdline.chars().take(MAX_COMMAND - 1).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;
    use vmdesk_core::VmId;
    use vmdesk_db::{NoteId, SessionId, SessionSummary};
    use vmdesk_proto::Container;

    use super::*;

    /// 2027-01-15 08:00 UTC.
    const NOW: i64 = 1_800_000_000;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn process(pid: u32, parent: u32, cmdline: &str, cwd: &str, started_at: i64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid: parent,
            user: "root".into(),
            cmdline: cmdline.into(),
            cwd: Some(cwd.into()),
            started_at,
            cpu_pct: 0.0,
            rss_kb: 0,
        }
    }

    fn tmux(name: &str, attached: bool, cwd: &str, command: &str, created: i64) -> TmuxSession {
        TmuxSession {
            name: name.into(),
            windows: 1,
            attached,
            created,
            pane_cwd: Some(cwd.into()),
            pane_command: Some(command.into()),
        }
    }

    fn repo(path: &str, branch: &str, dirty: u32) -> RepoInfo {
        RepoInfo {
            path: path.into(),
            branch: Some(branch.into()),
            dirty,
            ahead: None,
            behind: None,
            last_commit: None,
        }
    }

    fn container(name: &str, project: Option<&str>) -> Container {
        Container {
            id: name.into(),
            name: name.into(),
            image: "img".into(),
            state: "running".into(),
            status: String::new(),
            ports: String::new(),
            compose_project: project.map(str::to_owned),
        }
    }

    fn session(started: i64, ended: i64) -> WorkSession {
        WorkSession {
            id: SessionId(Uuid::now_v7()),
            vm: VmId::new(),
            started_at: at(started),
            ended_at: at(ended),
            title: None,
            summary: SessionSummary::default(),
        }
    }

    fn note(session: Option<SessionId>, body: &str, created: i64) -> Note {
        Note {
            id: NoteId(Uuid::now_v7()),
            vm: VmId::new(),
            session,
            body: body.into(),
            created_at: at(created),
            updated_at: at(created),
        }
    }

    /// A tmux server (pid 100) with one pane shell (101) and an SSH login shell (200).
    fn snapshot() -> Snapshot {
        Snapshot {
            taken_at: Some(at(NOW)),
            processes: vec![
                process(100, 1, "tmux new -s nightly-debug", "/root", NOW - 9000),
                process(101, 100, "-bash", "/root/regal", NOW - 9000),
                process(200, 50, "-bash", "/root", NOW - 4000),
            ],
            repos: vec![
                repo("/root/regal", "group2-fix", 3),
                repo("/root/other", "main", 0),
            ],
            ..Snapshot::default()
        }
    }

    fn line(w: &CurrentWork) -> String {
        w.line_in(&Utc, at(NOW))
    }

    #[test]
    fn a_command_in_the_attached_tmux_session_wins_and_renders_the_template() {
        let mut s = snapshot();
        s.processes.extend([
            process(102, 101, "pytest tests/group2", "/root/regal", NOW - 3600),
            process(201, 200, "make build", "/root/other", NOW - 60),
            process(300, 1, "/usr/bin/newer-daemon", "/", NOW - 40),
        ]);
        s.tmux = vec![
            tmux("nightly-debug", true, "/root/regal", "pytest", NOW - 9000),
            tmux("scratch", false, "/root/other", "bash", NOW - 100),
        ];
        let started = NOW - 53 * 60;
        let w = current_work(&s, Some(&session(started, NOW - 600)), &[]);
        assert_eq!(
            w.anchor,
            Some(Anchor::Process {
                pid: 102,
                cmdline: "pytest tests/group2".into(),
                cwd: Some("/root/regal".into()),
            })
        );
        assert_eq!(
            line(&w),
            "pytest tests/group2 in regal @ group2-fix (3 modified) · tmux nightly-debug \
            · since 07:07"
        );
    }

    #[test]
    fn without_tmux_a_command_from_a_login_shell_beats_newer_daemons() {
        let mut s = snapshot();
        s.processes.retain(|p| p.pid == 200);
        s.processes.extend([
            process(201, 200, "make build", "/root/other/src", NOW - 600),
            process(300, 1, "/usr/bin/newer-daemon", "/", NOW - 40),
        ]);
        let w = current_work(&s, None, &[]);
        assert_eq!(w.tmux, None);
        assert_eq!(line(&w), "make build in other @ main · since 07:50");
    }

    #[test]
    fn outside_a_repo_the_line_names_the_directory() {
        let mut s = snapshot();
        s.processes.push(process(
            201,
            200,
            "python3 -m http.server 8080",
            "/tmp",
            NOW - 120,
        ));
        let w = current_work(&s, None, &[]);
        assert_eq!(w.repo, None);
        assert_eq!(
            line(&w),
            "python3 -m http.server 8080 in /tmp · since 07:58"
        );
    }

    #[test]
    fn an_attached_session_at_a_prompt_beats_commands_in_detached_ones() {
        let mut s = snapshot();
        s.processes.push(process(
            102,
            101,
            "pytest tests/group5",
            "/root/other",
            NOW - 60,
        ));
        s.tmux = vec![
            tmux(
                "debug",
                true,
                "/root/regal",
                "bash",
                NOW - 6 * 86_400 - 9000,
            ),
            tmux("tests", false, "/root/other", "pytest", NOW - 5000),
        ];
        let w = current_work(&s, None, &[]);
        assert_eq!(
            line(&w),
            "tmux debug in regal @ group2-fix (3 modified) · since 9 Jan 05:30"
        );
    }

    #[test]
    fn the_note_on_the_latest_session_and_containers_of_the_repo_are_attached() {
        let mut s = snapshot();
        s.processes.push(process(
            102,
            101,
            "./run_nightly.sh",
            "/root/regal",
            NOW - 60,
        ));
        s.tmux = vec![tmux("nightly-debug", false, "/root", "bash", NOW - 9000)];
        s.containers = vec![
            container("regal-db-1", Some("regal")),
            container("regal_ui", None),
            container("postgres", None),
            container("regalia", None),
        ];
        let latest = session(NOW - 600, NOW - 60);
        let notes = [
            note(Some(latest.id), "older note", NOW - 500),
            note(Some(latest.id), "waiting on CI", NOW - 100),
            note(Some(SessionId(Uuid::now_v7())), "another session", NOW),
            note(None, "about the VM", NOW),
        ];
        let w = current_work(&s, Some(&latest), &notes);
        assert_eq!(w.note.as_deref(), Some("waiting on CI"));
        assert_eq!(w.containers, ["regal-db-1", "regal_ui"]);
        assert_eq!(
            line(&w),
            "./run_nightly.sh in regal @ group2-fix (3 modified) · tmux nightly-debug \
            · containers regal-db-1, regal_ui · since 07:50"
        );
    }

    #[test]
    fn a_session_that_ended_long_before_the_snapshot_is_not_the_start() {
        let mut s = snapshot();
        let old = session(NOW - 30 * 86_400, NOW - 29 * 86_400);
        let w = current_work(&s, Some(&old), &[note(Some(old.id), "left off here", 0)]);
        assert_eq!(w.note.as_deref(), Some("left off here"));
        assert_eq!(line(&w), "nothing running");

        s.processes.clear();
        s.tmux.clear();
        let current = session(NOW - 7200, NOW - 3 * 3600 / 2);
        assert_eq!(
            line(&current_work(&s, Some(&current), &[])),
            "nothing running · since 06:00"
        );
    }

    #[test]
    fn long_commands_are_shortened() {
        let mut s = snapshot();
        let long = format!("/opt/server --flag {}", "x".repeat(80));
        s.processes.push(process(201, 200, &long, "/tmp", NOW - 60));
        let shown = line(&current_work(&s, None, &[]));
        assert!(shown.starts_with(&long[..59]), "{shown}");
        assert!(shown.contains("… in /tmp"), "{shown}");
    }
}
