//! `vmdesk sessions`, `note`, `notes` and `search`: what you did on a VM, and where you
//! ran something.

use chrono::{DateTime, Duration, Local, Utc};
use clap::Subcommand;
use vmdesk_db::{Db, SessionId, WorkSession};

use crate::show::local;
use crate::{print_table, remote};

/// Commands shown per session in `vmdesk sessions`.
const COMMANDS_SHOWN: usize = 3;
const COMMAND_WIDTH: usize = 40;

#[derive(Subcommand)]
pub enum SessionsCmd {
    /// Give a work session a title (an empty title clears it).
    Rename {
        /// VM name, alias or id.
        vm: String,
        /// The session's id, or the start of it as `vmdesk sessions` shows.
        id: String,
        #[arg(required = true, num_args = 1..)]
        title: Vec<String>,
    },
}

/// How far back to search, e.g. `30m`, `12h`, `7d` or `2w`.
pub fn parse_age(s: &str) -> Result<Duration, String> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().map_err(|_| format!("{s:?}: expected e.g. 7d"))?;
    match unit {
        "m" => Ok(Duration::minutes(n)),
        "h" => Ok(Duration::hours(n)),
        "d" => Ok(Duration::days(n)),
        "w" => Ok(Duration::weeks(n)),
        _ => Err(format!("{s:?}: the unit is m, h, d or w")),
    }
}

/// The start of a session id, enough to tell one VM's sessions apart.
fn short(id: SessionId) -> String {
    id.to_string()[..8].to_owned()
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

fn last_dir(path: &str) -> &str {
    path.rsplit('/').find(|p| !p.is_empty()).unwrap_or(path)
}

/// When a session ended: `ongoing`, a time on the day it started, or a date and time.
fn ended(s: &WorkSession, now: DateTime<Utc>) -> String {
    if s.ongoing(now) {
        return "ongoing".to_owned();
    }
    let (start, end) = (
        s.started_at.with_timezone(&Local),
        s.ended_at.with_timezone(&Local),
    );
    if start.date_naive() == end.date_naive() {
        end.format("%H:%M").to_string()
    } else {
        local(s.ended_at)
    }
}

pub async fn sessions(db: &Db, name: &str, limit: u32) -> anyhow::Result<()> {
    let (vm, _) = remote::resolve(db, name).await?;
    let sessions = db.sessions(vm.id, limit).await?;
    if sessions.is_empty() {
        println!("no work sessions on {} yet", vm.name);
        return Ok(());
    }
    let now = Utc::now();
    let rows: Vec<[String; 7]> = sessions
        .iter()
        .map(|s| {
            let repos: Vec<String> = s
                .summary
                .repos
                .iter()
                .map(|r| match &r.branches[..] {
                    [] => last_dir(&r.path).to_owned(),
                    branches => format!("{} @ {}", last_dir(&r.path), branches.join(", ")),
                })
                .collect();
            let commands: Vec<String> = s
                .summary
                .commands
                .iter()
                .take(COMMANDS_SHOWN)
                .map(|c| match c.runs {
                    1 => clip(&c.cmdline, COMMAND_WIDTH),
                    n => format!("{} ×{n}", clip(&c.cmdline, COMMAND_WIDTH)),
                })
                .collect();
            [
                short(s.id),
                local(s.started_at),
                ended(s, now),
                vmdesk_core::human_duration((s.ended_at - s.started_at).num_seconds()),
                s.title.clone().unwrap_or_default(),
                repos.join("; "),
                commands.join(" · "),
            ]
        })
        .collect();
    print_table(
        &[
            "ID", "STARTED", "ENDED", "DURATION", "TITLE", "REPOS", "COMMANDS",
        ],
        &rows,
    );
    Ok(())
}

pub async fn rename(db: &Db, name: &str, id: &str, title: &[String]) -> anyhow::Result<()> {
    let (vm, _) = remote::resolve(db, name).await?;
    let session = find(db, &vm, id).await?;
    let renamed = db.rename_session(session.id, &title.join(" ")).await?;
    match &renamed.title {
        Some(t) => println!("{} {}: {t}", short(renamed.id), local(renamed.started_at)),
        None => println!(
            "{} {}: title cleared",
            short(renamed.id),
            local(renamed.started_at)
        ),
    }
    Ok(())
}

async fn find(db: &Db, vm: &vmdesk_core::Vm, id: &str) -> anyhow::Result<WorkSession> {
    db.find_session(vm.id, id).await.map_err(|e| match e {
        vmdesk_db::Error::NotFound => anyhow::anyhow!(
            "no session of {} starts with {id:?}; `vmdesk sessions {}` lists them",
            vm.name,
            vm.name
        ),
        other => other.into(),
    })
}

/// Adds a note, pinned to the session `id` starts, else to the VM's latest session.
pub async fn note(db: &Db, name: &str, text: &[String], id: Option<&str>) -> anyhow::Result<()> {
    let (vm, _) = remote::resolve(db, name).await?;
    let session = match id {
        Some(id) => Some(find(db, &vm, id).await?),
        None => db.latest_session(vm.id).await?,
    };
    db.add_note(vm.id, session.as_ref().map(|s| s.id), &text.join(" "))
        .await?;
    match session {
        Some(s) => println!(
            "noted on {}, session {} (started {})",
            vm.name,
            short(s.id),
            local(s.started_at)
        ),
        None => println!("noted on {} (it has no work session yet)", vm.name),
    }
    Ok(())
}

pub async fn notes(db: &Db, name: &str) -> anyhow::Result<()> {
    let (vm, _) = remote::resolve(db, name).await?;
    let notes = db.notes(vm.id).await?;
    if notes.is_empty() {
        println!(
            "no notes on {}; add one with `vmdesk note {} <text>`",
            vm.name, vm.name
        );
        return Ok(());
    }
    let rows: Vec<[String; 3]> = notes
        .iter()
        .map(|n| {
            [
                local(n.created_at),
                n.session.map(short).unwrap_or_default(),
                n.body.clone(),
            ]
        })
        .collect();
    print_table(&["WRITTEN", "SESSION", "NOTE"], &rows);
    Ok(())
}

pub async fn search(
    db: &Db,
    query: &[String],
    since: Option<Duration>,
    limit: u32,
) -> anyhow::Result<()> {
    let query = query.join(" ");
    let hits = db
        .search(&query, since.map(|d| Utc::now() - d), limit)
        .await?;
    if hits.is_empty() {
        println!("no events match {query:?}");
        return Ok(());
    }
    let rows: Vec<[String; 4]> = hits
        .into_iter()
        .map(|h| {
            [
                h.event
                    .ts
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
                h.vm_name,
                h.event.kind,
                h.summary,
            ]
        })
        .collect();
    print_table(&["WHEN", "VM", "EVENT", "DETAIL"], &rows);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(parse_age("7d"), Ok(Duration::days(7)));
        assert_eq!(parse_age("90m"), Ok(Duration::minutes(90)));
        assert_eq!(parse_age("2w"), Ok(Duration::weeks(2)));
        assert!(parse_age("7").is_err());
        assert!(parse_age("d").is_err());
        assert!(parse_age("7y").is_err());
    }
}
