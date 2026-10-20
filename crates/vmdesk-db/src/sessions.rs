//! Work sessions (spec section 11): a VM's activity, split wherever it was idle for more
//! than [`IDLE_GAP`]. Sessions are derived from the events, then stored so that one
//! keeps its id, title and notes while new events extend it or merge it with another.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::ops::Range;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};
use uuid::{NoContext, Timestamp, Uuid};
use vmdesk_core::VmId;
use vmdesk_proto::EventKind;

use crate::inventory::{from_ms, parse_id};
use crate::timeline::{latest_snapshot_in, parse_event};
use crate::{Db, Error, Result};

/// Activity further apart than this starts a new session.
pub const IDLE_GAP: Duration = Duration::hours(2);
/// Commands kept in a session's summary.
const TOP_COMMANDS: usize = 5;
const MAX_TITLE: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A stretch of work on one VM.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkSession {
    /// A v7 UUID stamped with the time the session first started, so ids of one VM's
    /// sessions differ within their first 8 characters.
    pub id: SessionId,
    pub vm: VmId,
    pub started_at: DateTime<Utc>,
    /// The last activity.
    pub ended_at: DateTime<Utc>,
    pub title: Option<String>,
    pub summary: SessionSummary,
}

impl WorkSession {
    /// Whether more activity now would still extend this session.
    #[must_use]
    pub fn ongoing(&self, now: DateTime<Utc>) -> bool {
        now - self.ended_at <= IDLE_GAP
    }
}

/// What a session's events say was done in it (`summary_json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSummary {
    /// Events that counted as activity.
    pub events: u32,
    /// Repos with a git event, or a command or tmux session started in them.
    pub repos: Vec<RepoTouched>,
    /// Commands started in the session, most runs first, then the longest.
    pub commands: Vec<CommandRuns>,
    /// Containers started or stopped.
    pub containers: Vec<String>,
    /// tmux sessions created, attached or closed.
    pub tmux: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTouched {
    pub path: String,
    /// Branches seen in git events; empty when the session had none for this repo.
    pub branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRuns {
    pub cmdline: String,
    pub runs: u32,
    /// The longest run that also ended in the session, as far as observed.
    pub longest_secs: Option<u64>,
}

/// One stored event as derivation reads it. `kind` is `None` for kinds this build does
/// not know; they still count as activity.
#[derive(Debug, Clone)]
pub(crate) struct Logged {
    pub ts: i64,
    pub source: String,
    pub kind: Option<EventKind>,
}

/// A derived session: its first and last activity (Unix ms) and summary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Derived {
    pub start: i64,
    pub end: i64,
    pub summary: SessionSummary,
}

/// Splits sorted activity times (Unix ms) into sessions: ranges of indices whose
/// neighbours are at most [`IDLE_GAP`] apart.
pub(crate) fn split(times: &[i64]) -> Vec<Range<usize>> {
    let gap = IDLE_GAP.num_milliseconds();
    let mut sessions = Vec::new();
    let mut start = 0;
    for i in 1..times.len() {
        if times[i] - times[i - 1] > gap {
            sessions.push(start..i);
            start = i;
        }
    }
    if !times.is_empty() {
        sessions.push(start..times.len());
    }
    sessions
}

/// The events that count as activity, still sorted. Gaps do not, and neither does what
/// a collector noticed on the poll that ended its own gap: a process found gone then
/// ended at some unknown time inside the gap, not at the poll.
fn activity(events: &[Logged]) -> Vec<&Logged> {
    let gap_ends: HashSet<(&str, i64)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Some(EventKind::Gap { to, .. }) => Some((e.source.as_str(), to.timestamp_millis())),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter(|e| !matches!(e.kind, Some(EventKind::Gap { .. })))
        .filter(|e| !gap_ends.contains(&(e.source.as_str(), e.ts)))
        .collect()
}

/// Sessions from a VM's events (sorted by time, `status_changed` left out), oldest
/// first. `repos` are repo paths from the VM's latest snapshot; with those of each
/// session's git events they place its commands by working directory.
pub(crate) fn derive(events: &[Logged], repos: &[String]) -> Vec<Derived> {
    let active = activity(events);
    let times: Vec<i64> = active.iter().map(|e| e.ts).collect();
    split(&times)
        .into_iter()
        .map(|r| Derived {
            start: times[r.start],
            end: times[r.end - 1],
            summary: summarize(&active[r], repos),
        })
        .collect()
}

/// The longest repo path that contains `cwd`.
fn repo_of<'a>(cwd: &str, repos: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    repos
        .filter(|r| {
            cwd.strip_prefix(r)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        .max_by_key(|r| r.len())
}

fn summarize(events: &[&Logged], snapshot_repos: &[String]) -> SessionSummary {
    let mut touched: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in events {
        let (repo, branches): (&str, &[&Option<String>]) = match &e.kind {
            Some(EventKind::GitBranchChanged { repo, from, to }) => (repo, &[from, to]),
            Some(EventKind::GitCommitted { repo, branch, .. }) => (repo, &[branch]),
            Some(EventKind::GitDirtyChanged { repo, .. }) => (repo, &[]),
            _ => continue,
        };
        let seen = touched.entry(repo.to_owned()).or_default();
        seen.extend(branches.iter().filter_map(|b| (*b).clone()));
    }
    let known: BTreeSet<String> = snapshot_repos
        .iter()
        .chain(touched.keys())
        .cloned()
        .collect();
    let mut place = |cwd: &Option<String>| {
        if let Some(repo) = cwd
            .as_deref()
            .and_then(|c| repo_of(c, known.iter().map(String::as_str)))
        {
            touched.entry(repo.to_owned()).or_default();
        }
    };

    let mut started: Vec<(u32, &str)> = Vec::new();
    let mut ended: HashMap<(u32, &str), u64> = HashMap::new();
    let mut containers = BTreeSet::new();
    let mut tmux = BTreeSet::new();
    for e in events {
        let Some(kind) = &e.kind else { continue };
        match kind {
            EventKind::ProcessStarted { pid, cmdline, cwd } => {
                place(cwd);
                // A login shell (`-bash`) is where commands run, not a command.
                if !cmdline.starts_with('-') {
                    started.push((*pid, cmdline));
                }
            }
            EventKind::ProcessExited {
                pid,
                cmdline,
                duration_secs: Some(secs),
                ..
            } => {
                ended.insert((*pid, cmdline), *secs);
            }
            EventKind::ContainerStarted { name, .. } | EventKind::ContainerStopped { name, .. } => {
                containers.insert(name.clone());
            }
            EventKind::TmuxCreated { name, pane_cwd } => {
                place(pane_cwd);
                tmux.insert(name.clone());
            }
            EventKind::TmuxClosed { name } | EventKind::TmuxAttached { name } => {
                tmux.insert(name.clone());
            }
            _ => {}
        }
    }

    let mut commands: Vec<CommandRuns> = Vec::new();
    for (pid, cmdline) in started {
        let secs = ended.get(&(pid, cmdline)).copied();
        match commands.iter_mut().find(|c| c.cmdline == cmdline) {
            Some(c) => {
                c.runs += 1;
                c.longest_secs = c.longest_secs.max(secs);
            }
            None => commands.push(CommandRuns {
                cmdline: cmdline.to_owned(),
                runs: 1,
                longest_secs: secs,
            }),
        }
    }
    // Stable: commands with equal runs and length stay in the order first started.
    commands.sort_by_key(|c| std::cmp::Reverse((c.runs, c.longest_secs)));
    commands.truncate(TOP_COMMANDS);

    SessionSummary {
        events: u32::try_from(events.len()).unwrap_or(u32::MAX),
        repos: touched
            .into_iter()
            .map(|(path, branches)| RepoTouched {
                path,
                branches: branches.into_iter().collect(),
            })
            .collect(),
        commands,
        containers: containers.into_iter().collect(),
        tmux: tmux.into_iter().collect(),
    }
}

/// A stored session's span, for [`reconcile`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stored {
    pub id: SessionId,
    pub start: i64,
    pub end: i64,
}

/// How stored sessions map onto freshly derived ones.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Per derived session, in order: the stored session it continues, or `None` for a
    /// new one.
    pub keep: Vec<Option<SessionId>>,
    /// Stored sessions folded into another, `(merged, into)`: their notes move over.
    pub merged: Vec<(SessionId, SessionId)>,
    /// Stored sessions none of whose span has activity left.
    pub gone: Vec<SessionId>,
}

/// Matches `stored` sessions (sorted by start) to `derived` spans `(start, end)`, also
/// sorted. A derived span continues the oldest stored session it overlaps; other
/// stored sessions it overlaps are merged into that one. A stored session overlapping
/// several derived spans stays with the first, and the later ones get new ids.
pub(crate) fn reconcile(stored: &[Stored], derived: &[(i64, i64)]) -> Plan {
    let mut claimed = vec![false; stored.len()];
    let mut plan = Plan::default();
    for &(start, end) in derived {
        let hits: Vec<usize> = (0..stored.len())
            .filter(|&i| !claimed[i] && stored[i].start <= end && start <= stored[i].end)
            .collect();
        for &i in &hits {
            claimed[i] = true;
        }
        match hits.split_first() {
            Some((&keeper, rest)) => {
                let into = stored[keeper].id;
                plan.keep.push(Some(into));
                plan.merged
                    .extend(rest.iter().map(|&i| (stored[i].id, into)));
            }
            None => plan.keep.push(None),
        }
    }
    plan.gone = (0..stored.len())
        .filter(|&i| !claimed[i])
        .map(|i| stored[i].id)
        .collect();
    plan
}

fn new_session_id(start_ms: i64) -> SessionId {
    let ms = u64::try_from(start_ms).unwrap_or(0);
    let nanos = u32::try_from(ms % 1000 * 1_000_000).unwrap_or(0);
    SessionId(Uuid::new_v7(Timestamp::from_unix(
        NoContext,
        ms / 1000,
        nanos,
    )))
}

/// The window that re-derives every session.
pub(crate) const EVERYTHING: (i64, i64) = (i64::MIN, i64::MAX);

/// The window (Unix ms) that events with these times can change: from the first to
/// the last, or to the end of a gap among them, whose closing poll's events stop
/// counting as activity.
pub(crate) fn window<'a>(events: impl IntoIterator<Item = (i64, &'a EventKind)>) -> (i64, i64) {
    events
        .into_iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), (ts, kind)| {
            let end = match kind {
                EventKind::Gap { to, .. } => to.timestamp_millis().max(ts),
                _ => ts,
            };
            (lo.min(ts), hi.max(end))
        })
}

/// Re-derives the sessions of `vm` that events in `window` (Unix ms) can change, and
/// stores the difference. Stored sessions are always more than [`IDLE_GAP`] apart, so
/// only those within that distance of the window can change, and only the events
/// from the first of them to the last need reading. Running it again changes nothing.
pub(crate) async fn refresh_in(
    conn: &mut SqliteConnection,
    vm: VmId,
    (from, to): (i64, i64),
) -> Result<()> {
    let gap = IDLE_GAP.num_milliseconds();
    let stored = stored_near(conn, vm, from.saturating_sub(gap), to.saturating_add(gap)).await?;
    let lo = stored.iter().map(|s| s.start).fold(from, i64::min);
    let hi = stored.iter().map(|s| s.end).fold(to, i64::max);

    // Gaps are read whole: one that started before `lo` can still end inside it.
    let rows = sqlx::query(
        "SELECT ts, source, seq, kind, data_json FROM events \
        WHERE vm_id = ?1 AND ts BETWEEN ?2 AND ?3 AND kind NOT IN ('status_changed', 'gap') \
        UNION ALL SELECT ts, source, seq, kind, data_json FROM events \
        WHERE vm_id = ?1 AND kind = 'gap' AND ts <= ?3 \
        ORDER BY ts, source, seq",
    )
    .bind(vm.to_string())
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut *conn)
    .await?;
    let mut events = Vec::with_capacity(rows.len());
    for r in &rows {
        events.push(Logged {
            ts: r.try_get("ts")?,
            source: r.try_get("source")?,
            kind: parse_event(r.try_get("kind")?, r.try_get("data_json")?),
        });
    }
    let repos: Vec<String> = latest_snapshot_in(conn, vm)
        .await?
        .map(|s| s.repos.into_iter().map(|r| r.path).collect())
        .unwrap_or_default();
    store(conn, vm, &stored, &derive(&events, &repos)).await
}

/// `vm`'s stored sessions that overlap `[from, to]`, oldest first.
async fn stored_near(
    conn: &mut SqliteConnection,
    vm: VmId,
    from: i64,
    to: i64,
) -> Result<Vec<Stored>> {
    sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT id, started_at, COALESCE(ended_at, started_at) AS ended FROM work_sessions \
        WHERE vm_id = ?1 AND started_at <= ?3 AND ended >= ?2 ORDER BY started_at, id",
    )
    .bind(vm.to_string())
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(id, start, end)| {
        Ok(Stored {
            id: parse_session_id(&id)?,
            start,
            end,
        })
    })
    .collect()
}

/// Replaces `stored` with `derived`, writing only what changed.
async fn store(
    conn: &mut SqliteConnection,
    vm: VmId,
    stored: &[Stored],
    derived: &[Derived],
) -> Result<()> {
    let spans: Vec<(i64, i64)> = derived.iter().map(|d| (d.start, d.end)).collect();
    let plan = reconcile(stored, &spans);

    for (merged, into) in &plan.merged {
        sqlx::query("UPDATE notes SET session_id = ?1 WHERE session_id = ?2")
            .bind(into.to_string())
            .bind(merged.to_string())
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "UPDATE work_sessions SET title = \
            COALESCE(title, (SELECT title FROM work_sessions WHERE id = ?2)) WHERE id = ?1",
        )
        .bind(into.to_string())
        .bind(merged.to_string())
        .execute(&mut *conn)
        .await?;
    }
    for id in plan.merged.iter().map(|(m, _)| m).chain(&plan.gone) {
        sqlx::query("DELETE FROM work_sessions WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *conn)
            .await?;
    }
    for (d, keep) in derived.iter().zip(&plan.keep) {
        let summary = serde_json::to_string(&d.summary)?;
        match keep {
            Some(id) => {
                sqlx::query(
                    "UPDATE work_sessions SET started_at = ?2, ended_at = ?3, summary_json = ?4 \
                    WHERE id = ?1 AND (started_at != ?2 OR ended_at IS NOT ?3 \
                    OR summary_json IS NOT ?4)",
                )
                .bind(id.to_string())
                .bind(d.start)
                .bind(d.end)
                .bind(summary)
                .execute(&mut *conn)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO work_sessions (id, vm_id, started_at, ended_at, summary_json) \
                    VALUES (?, ?, ?, ?, ?)",
                )
                .bind(new_session_id(d.start).to_string())
                .bind(vm.to_string())
                .bind(d.start)
                .bind(d.end)
                .bind(summary)
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    Ok(())
}

/// Derives sessions for VMs that have activity but no sessions yet: those recorded
/// before sessions existed.
pub(crate) async fn backfill(db: &Db) -> Result<()> {
    let vms: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM vms v \
        WHERE NOT EXISTS (SELECT 1 FROM work_sessions s WHERE s.vm_id = v.id) \
        AND EXISTS (SELECT 1 FROM events e WHERE e.vm_id = v.id \
            AND e.kind NOT IN ('status_changed', 'gap'))",
    )
    .fetch_all(db.pool())
    .await?;
    for id in vms {
        db.refresh_sessions(parse_id(&id)?).await?;
    }
    Ok(())
}

fn parse_session_id(s: &str) -> Result<SessionId> {
    s.parse()
        .map(SessionId)
        .map_err(|_| Error::Corrupt(format!("bad session id {s:?}")))
}

const COLUMNS: &str = "id, vm_id, started_at, ended_at, title, summary_json";

fn session_from_row(row: &SqliteRow) -> Result<WorkSession> {
    let started: i64 = row.try_get("started_at")?;
    let ended: Option<i64> = row.try_get("ended_at")?;
    let summary: Option<String> = row.try_get("summary_json")?;
    Ok(WorkSession {
        id: parse_session_id(&row.try_get::<String, _>("id")?)?,
        vm: parse_id(&row.try_get::<String, _>("vm_id")?)?,
        started_at: from_ms(started)?,
        ended_at: from_ms(ended.unwrap_or(started))?,
        title: row.try_get("title")?,
        summary: summary
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default(),
    })
}

impl Db {
    /// Re-derives all of `vm`'s sessions from its events. Event writes keep the
    /// sessions they touch current themselves; running this again changes nothing.
    pub async fn refresh_sessions(&self, vm: VmId) -> Result<()> {
        let mut tx = self.write().await?;
        refresh_in(&mut tx, vm, EVERYTHING).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Newest first, at most `limit`.
    pub async fn sessions(&self, vm: VmId, limit: u32) -> Result<Vec<WorkSession>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM work_sessions WHERE vm_id = ? \
            ORDER BY started_at DESC LIMIT ?"
        ))
        .bind(vm.to_string())
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(session_from_row)
        .collect()
    }

    /// The VM's most recent session, ongoing or not.
    pub async fn latest_session(&self, vm: VmId) -> Result<Option<WorkSession>> {
        Ok(self.sessions(vm, 1).await?.pop())
    }

    pub async fn session(&self, id: SessionId) -> Result<WorkSession> {
        sqlx::query(&format!("SELECT {COLUMNS} FROM work_sessions WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?
            .map(|r| session_from_row(&r))
            .transpose()?
            .ok_or(Error::NotFound)
    }

    /// The session of `vm` whose id starts with `prefix` (case-insensitive).
    pub async fn find_session(&self, vm: VmId, prefix: &str) -> Result<WorkSession> {
        let prefix = prefix.trim().to_ascii_lowercase();
        if prefix.is_empty() {
            return Err(Error::Invalid("empty session id".into()));
        }
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM work_sessions \
            WHERE vm_id = ?1 AND substr(id, 1, length(?2)) = ?2 LIMIT 2"
        ))
        .bind(vm.to_string())
        .bind(&prefix)
        .fetch_all(self.pool())
        .await?;
        match &rows[..] {
            [] => Err(Error::NotFound),
            [one] => session_from_row(one),
            _ => Err(Error::Conflict(format!(
                "more than one session starts with {prefix}"
            ))),
        }
    }

    /// Sets a session's title; an empty one clears it. Re-deriving keeps the title.
    pub async fn rename_session(&self, id: SessionId, title: &str) -> Result<WorkSession> {
        let title = title.trim();
        if title.chars().count() > MAX_TITLE {
            return Err(Error::Invalid(format!(
                "title longer than {MAX_TITLE} characters"
            )));
        }
        let done = sqlx::query("UPDATE work_sessions SET title = ? WHERE id = ?")
            .bind((!title.is_empty()).then_some(title))
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        self.session(id).await
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::db_with_vm;

    const H: i64 = 3_600_000;
    const T0: i64 = 1_800_000_000_000;

    fn logged(ts: i64, kind: EventKind) -> Logged {
        Logged {
            ts,
            source: "ssh".into(),
            kind: Some(kind),
        }
    }

    fn started(pid: u32, cmdline: &str, cwd: &str) -> EventKind {
        EventKind::ProcessStarted {
            pid,
            cmdline: cmdline.into(),
            cwd: Some(cwd.into()),
        }
    }

    fn exited(pid: u32, cmdline: &str, secs: u64) -> EventKind {
        EventKind::ProcessExited {
            pid,
            cmdline: cmdline.into(),
            exit_code: None,
            duration_secs: Some(secs),
        }
    }

    fn tmux(name: &str) -> EventKind {
        EventKind::TmuxAttached { name: name.into() }
    }

    fn id(n: u128) -> SessionId {
        SessionId(Uuid::from_u128(n))
    }

    #[test]
    fn idle_gaps_over_two_hours_split_sessions() {
        let gap = IDLE_GAP.num_milliseconds();
        let bounds = |times: &[i64]| -> Vec<(usize, usize)> {
            split(times).into_iter().map(|r| (r.start, r.end)).collect()
        };
        assert!(bounds(&[]).is_empty());
        assert_eq!(bounds(&[T0]), [(0, 1)]);
        assert_eq!(
            bounds(&[T0, T0 + gap, T0 + 2 * gap]),
            [(0, 3)],
            "exactly 2 h apart: one session"
        );
        assert_eq!(bounds(&[T0, T0 + gap + 1, T0 + gap + 2]), [(0, 1), (1, 3)]);
        assert_eq!(bounds(&[T0, T0, T0 + 3 * H]), [(0, 2), (2, 3)]);
    }

    #[test]
    fn gaps_and_what_their_closing_poll_noticed_are_not_activity() {
        let gap_end = T0 + 6 * H + 123;
        let events = [
            logged(T0, started(1, "pytest tests/a", "/r")),
            logged(
                T0 + H,
                EventKind::Gap {
                    from: Utc.timestamp_millis_opt(T0 + H).unwrap(),
                    to: Utc.timestamp_millis_opt(gap_end).unwrap(),
                },
            ),
            logged(gap_end, exited(1, "pytest tests/a", 3600)),
            Logged {
                source: "agent:x".into(),
                ..logged(gap_end, tmux("other-collector"))
            },
        ];
        let got = derive(&events, &[]);
        let spans: Vec<(i64, i64)> = got.iter().map(|d| (d.start, d.end)).collect();
        assert_eq!(
            spans,
            [(T0, T0), (gap_end, gap_end)],
            "the exit is dropped, the agent's event at the same instant is not"
        );
        assert_eq!(got[1].summary.tmux, ["other-collector"]);
    }

    #[test]
    fn summary_lists_repos_commands_containers_and_tmux() {
        let repos = ["/root/regal".to_owned(), "/root/regal-old".to_owned()];
        let events = [
            logged(T0, started(10, "-bash", "/root")),
            logged(
                T0 + 1,
                started(11, "pytest tests/group2", "/root/regal/tests"),
            ),
            logged(T0 + 2, exited(11, "pytest tests/group2", 90)),
            logged(T0 + 3, started(12, "pytest tests/group2", "/root/regal")),
            logged(T0 + 4, exited(12, "pytest tests/group2", 300)),
            logged(T0 + 5, started(13, "make build", "/root/regal-old")),
            logged(T0 + 6, exited(99, "sleep-started-earlier", 9999)),
            logged(T0 + 7, started(14, "python3 -m http.server", "/tmp")),
            logged(
                T0 + 8,
                EventKind::GitBranchChanged {
                    repo: "/root/regal".into(),
                    from: Some("main".into()),
                    to: Some("group2-fix".into()),
                },
            ),
            logged(
                T0 + 9,
                EventKind::ContainerStarted {
                    name: "regal-db".into(),
                    image: "postgres".into(),
                    compose_project: None,
                },
            ),
            logged(T0 + 10, tmux("nightly")),
        ];
        let got = derive(&events, &repos);
        assert_eq!(got.len(), 1);
        let s = &got[0].summary;
        assert_eq!(s.events, 11);
        assert_eq!(
            s.repos,
            [
                RepoTouched {
                    path: "/root/regal".into(),
                    branches: vec!["group2-fix".into(), "main".into()],
                },
                RepoTouched {
                    path: "/root/regal-old".into(),
                    branches: vec![],
                },
            ],
            "/root/regal-old is its own repo, not under /root/regal"
        );
        let commands: Vec<(&str, u32, Option<u64>)> = s
            .commands
            .iter()
            .map(|c| (c.cmdline.as_str(), c.runs, c.longest_secs))
            .collect();
        assert_eq!(
            commands,
            [
                ("pytest tests/group2", 2, Some(300)),
                ("make build", 1, None),
                ("python3 -m http.server", 1, None),
            ],
            "no login shell, nothing that only ended here"
        );
        assert_eq!(s.containers, ["regal-db"]);
        assert_eq!(s.tmux, ["nightly"]);
    }

    #[test]
    fn reconcile_keeps_ids_merges_into_the_older_and_splits_off_new_ones() {
        let stored = |n, start, end| Stored {
            id: id(n),
            start,
            end,
        };
        assert_eq!(
            reconcile(&[stored(1, 10, 20)], &[(5, 25)]),
            Plan {
                keep: vec![Some(id(1))],
                ..Plan::default()
            },
            "an extended session keeps its id"
        );
        assert_eq!(
            reconcile(&[stored(1, 10, 20), stored(2, 30, 40)], &[(10, 40)]),
            Plan {
                keep: vec![Some(id(1))],
                merged: vec![(id(2), id(1))],
                gone: vec![],
            }
        );
        assert_eq!(
            reconcile(&[stored(1, 10, 40)], &[(10, 15), (35, 40)]),
            Plan {
                keep: vec![Some(id(1)), None],
                ..Plan::default()
            },
            "a split keeps the id on the first part"
        );
        assert_eq!(
            reconcile(
                &[stored(1, 10, 20), stored(2, 50, 60)],
                &[(50, 70), (90, 95)]
            ),
            Plan {
                keep: vec![Some(id(2)), None],
                merged: vec![],
                gone: vec![id(1)],
            }
        );
    }

    #[test]
    fn session_ids_carry_their_start_time() {
        let a = new_session_id(T0);
        let b = new_session_id(T0 + IDLE_GAP.num_milliseconds() + 1);
        assert_ne!(a.to_string()[..8], b.to_string()[..8]);
        assert_eq!(
            a.0.get_timestamp().unwrap().to_unix(),
            (u64::try_from(T0 / 1000).unwrap(), 0)
        );
    }

    async fn record(db: &Db, vm: VmId, events: &[(i64, EventKind)]) {
        let events: Vec<(DateTime<Utc>, EventKind)> = events
            .iter()
            .map(|(ms, k)| (Utc.timestamp_millis_opt(*ms).unwrap(), k.clone()))
            .collect();
        let snap = vmdesk_proto::Snapshot {
            taken_at: events.last().map(|(t, _)| *t),
            ..vmdesk_proto::Snapshot::default()
        };
        db.record_snapshot(vm, "ssh", &snap, &events).await.unwrap();
    }

    async fn rows(db: &Db) -> Vec<(String, i64, Option<i64>, Option<String>, Option<String>)> {
        sqlx::query_as(
            "SELECT id, started_at, ended_at, title, summary_json FROM work_sessions \
            ORDER BY started_at",
        )
        .fetch_all(db.pool())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn event_writes_keep_sessions_current_and_refresh_is_idempotent() {
        let (db, vm) = db_with_vm().await;
        record(&db, vm, &[(T0, tmux("a")), (T0 + H, tmux("b"))]).await;
        record(&db, vm, &[(T0 + 5 * H, tmux("c"))]).await;
        let sessions = db.sessions(vm, 10).await.unwrap();
        let spans: Vec<(i64, i64)> = sessions
            .iter()
            .map(|s| {
                (
                    s.started_at.timestamp_millis(),
                    s.ended_at.timestamp_millis(),
                )
            })
            .collect();
        assert_eq!(
            spans,
            [(T0 + 5 * H, T0 + 5 * H), (T0, T0 + H)],
            "newest first"
        );
        assert_eq!(sessions[1].summary.tmux, ["a", "b"]);

        let before = rows(&db).await;
        db.refresh_sessions(vm).await.unwrap();
        db.refresh_sessions(vm).await.unwrap();
        assert_eq!(rows(&db).await, before);
    }

    #[tokio::test]
    async fn writes_in_any_order_leave_what_a_full_refresh_derives() {
        let (db, vm) = db_with_vm().await;
        let mut rng = 11u64;
        let mut next = |n: u64| {
            rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (rng >> 33) % n
        };
        let agent = Uuid::now_v7();
        let mut seq = 0;
        for batch in 0..60 {
            // Events anywhere in five days, as late agent history and old start times are.
            let at = |r: u64| T0 + i64::try_from(r).unwrap() * 60_000;
            let mut events: Vec<(i64, EventKind)> = (0..=next(4))
                .map(|_| {
                    let pid = u32::try_from(next(5)).unwrap();
                    let kind = match next(3) {
                        0 => started(pid, "pytest tests/a", "/r"),
                        1 => exited(pid, "pytest tests/a", next(900)),
                        _ => tmux("t"),
                    };
                    (at(next(7200)), kind)
                })
                .collect();
            if next(5) == 0 {
                let (from, to) = (at(next(7200)), at(next(7200)) + 10 * H);
                events.push((
                    from,
                    EventKind::Gap {
                        from: Utc.timestamp_millis_opt(from).unwrap(),
                        to: Utc.timestamp_millis_opt(to).unwrap(),
                    },
                ));
                events.push((to, exited(1, "found gone after the gap", 5)));
            }
            if batch % 3 == 0 {
                let batch: Vec<vmdesk_proto::Event> = events
                    .into_iter()
                    .filter(|(_, k)| !matches!(k, EventKind::Gap { .. }))
                    .map(|(ms, kind)| {
                        seq += 1;
                        vmdesk_proto::Event {
                            seq,
                            ts: Utc.timestamp_millis_opt(ms).unwrap(),
                            kind,
                        }
                    })
                    .collect();
                db.insert_agent_events(vm, agent, &batch).await.unwrap();
            } else {
                record(&db, vm, &events).await;
            }
        }
        let incremental = rows(&db).await;
        assert!(incremental.len() > 3, "{} sessions", incremental.len());
        db.refresh_sessions(vm).await.unwrap();
        assert_eq!(rows(&db).await, incremental);
    }

    #[tokio::test]
    async fn a_gap_that_began_long_before_still_hides_its_closing_poll() {
        let (db, vm) = db_with_vm().await;
        let gap_end = T0 + 10 * H;
        record(&db, vm, &[(T0, tmux("a"))]).await;
        record(
            &db,
            vm,
            &[
                (
                    T0 + H,
                    EventKind::Gap {
                        from: Utc.timestamp_millis_opt(T0 + H).unwrap(),
                        to: Utc.timestamp_millis_opt(gap_end).unwrap(),
                    },
                ),
                (gap_end - H / 2, started(7, "make", "/r")),
                (gap_end, exited(3, "gone", 60)),
            ],
        )
        .await;
        record(&db, vm, &[(gap_end + H / 2, tmux("b"))]).await;
        let latest = db.latest_session(vm).await.unwrap().unwrap();
        assert_eq!(latest.summary.events, 2, "make and tmux b, not the exit");
    }

    #[tokio::test]
    async fn a_merge_keeps_the_older_session_with_its_title_and_both_notes() {
        let (db, vm) = db_with_vm().await;
        record(&db, vm, &[(T0, tmux("a"))]).await;
        record(&db, vm, &[(T0 + 3 * H, tmux("b"))]).await;
        let [newer, older] =
            <[WorkSession; 2]>::try_from(db.sessions(vm, 10).await.unwrap()).unwrap();
        db.rename_session(newer.id, "afternoon").await.unwrap();
        db.add_note(vm, Some(older.id), "first").await.unwrap();
        db.add_note(vm, Some(newer.id), "second").await.unwrap();

        // Agent history that arrives later fills the idle stretch between the two.
        record(&db, vm, &[(T0 + 3 * H / 2, tmux("bridge"))]).await;
        let merged = db.sessions(vm, 10).await.unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, older.id);
        assert_eq!(merged[0].title.as_deref(), Some("afternoon"));
        assert_eq!(merged[0].summary.tmux, ["a", "b", "bridge"]);
        let notes = db.notes(vm).await.unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes.iter().all(|n| n.session == Some(older.id)));
    }

    #[tokio::test]
    async fn sessions_are_found_by_id_prefix_and_renamed() {
        let (db, vm) = db_with_vm().await;
        record(&db, vm, &[(T0, tmux("a"))]).await;
        record(&db, vm, &[(T0 + 30 * H, tmux("b"))]).await;
        let [newer, older] =
            <[WorkSession; 2]>::try_from(db.sessions(vm, 10).await.unwrap()).unwrap();
        let prefix = &older.id.to_string()[..8];
        assert_eq!(db.find_session(vm, prefix).await.unwrap().id, older.id);
        assert!(matches!(
            db.find_session(vm, &prefix[..1]).await,
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            db.find_session(vm, "ffffffff").await,
            Err(Error::NotFound)
        ));

        let renamed = db
            .rename_session(newer.id, "  nightly debug ")
            .await
            .unwrap();
        assert_eq!(renamed.title.as_deref(), Some("nightly debug"));
        db.refresh_sessions(vm).await.unwrap();
        assert_eq!(
            db.session(newer.id).await.unwrap().title.as_deref(),
            Some("nightly debug")
        );
        assert_eq!(db.rename_session(newer.id, "").await.unwrap().title, None);
        assert!(matches!(
            db.rename_session(SessionId(Uuid::now_v7()), "x").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn existing_history_gets_sessions_when_the_database_opens() {
        let dir = std::env::temp_dir().join(format!("vmdesk-sessions-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vmdesk.db");
        let vm = {
            let db = Db::open(&path).await.unwrap();
            let vm = crate::tests::add_vm(&db, "box").await;
            record(&db, vm, &[(T0, tmux("a"))]).await;
            // As left by a build from before sessions existed.
            sqlx::query("DELETE FROM work_sessions")
                .execute(db.pool())
                .await
                .unwrap();
            db.pool().close().await;
            vm
        };
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.sessions(vm, 10).await.unwrap().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }
}
