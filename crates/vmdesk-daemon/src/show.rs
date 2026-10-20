//! `vmdesk show` (what you are doing on a VM and what is on it now) and `vmdesk
//! timeline` (what happened there).

use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use vmdesk_core::HostKeyStore;
use vmdesk_db::Db;
use vmdesk_proto::Snapshot;
use vmdesk_transport::Transport;

use crate::{print_table, remote};

/// Processes listed by `show`, most recently started first.
const MAX_PROCESSES: usize = 25;

pub async fn show(db: &Db, name: &str, cached: bool) -> anyhow::Result<()> {
    let (vm, inventory) = remote::resolve(db, name).await?;
    let snap = if cached {
        db.latest_snapshot(vm.id).await?.ok_or_else(|| {
            anyhow::anyhow!("no snapshot of {} yet; run without --cached", vm.name)
        })?
    } else {
        let t = Transport::new(Arc::new(db.clone()) as Arc<dyn HostKeyStore>);
        let snap = vmdesk_collect::remote::collect(db, &t, &vm, &inventory)
            .await
            .map_err(anyhow::Error::new)?;
        t.close_all().await;
        snap
    };
    let session = db.latest_session(vm.id).await?;
    let notes = db.notes(vm.id).await?;
    let work = vmdesk_collect::work::current_work(&snap, session.as_ref(), &notes);
    if let Some(note) = &work.note {
        println!("note: {note}");
    }
    println!("{}\n", work.line());
    print_snapshot(&vm.name, &snap);
    Ok(())
}

/// Time since a Unix timestamp, e.g. `3h05m`.
fn ago(secs: i64) -> String {
    vmdesk_core::human_duration(Utc::now().timestamp() - secs)
}

fn gb(kb: u64) -> String {
    const GIB: u64 = 1_048_576;
    format!("{}.{}G", kb / GIB, kb % GIB * 10 / GIB)
}

fn heading(title: &str, count: usize) {
    println!("\n{title} ({count})");
}

fn print_snapshot(name: &str, s: &Snapshot) {
    print_system(name, s);
    print_tmux(s);
    print_containers(s);
    print_listeners(s);
    print_repos(s);
    print_processes(s);
    if !s.unavailable.is_empty() {
        println!("\nnot collected: {}", s.unavailable.join("; "));
    }
}

fn print_system(name: &str, s: &Snapshot) {
    let sys = &s.system;
    let taken = s
        .taken_at
        .map(|t| {
            t.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default();
    println!(
        "{name} ({}) · {} · kernel {} · taken {taken}",
        sys.hostname, sys.os, sys.kernel
    );
    println!(
        "up {} · load {:.2} {:.2} {:.2} on {} cpus · mem {} / {}",
        ago(Utc::now().timestamp() - i64::try_from(sys.uptime_secs).unwrap_or(0)),
        sys.load[0],
        sys.load[1],
        sys.load[2],
        sys.cpus,
        gb(sys.mem_used_kb),
        gb(sys.mem_total_kb),
    );
    let disks: Vec<String> = sys
        .disks
        .iter()
        .map(|d| format!("{} {}/{}", d.mount, gb(d.used_kb), gb(d.total_kb)))
        .collect();
    if !disks.is_empty() {
        println!("disks {}", disks.join(" · "));
    }
}

fn print_tmux(s: &Snapshot) {
    heading("tmux", s.tmux.len());
    let rows: Vec<[String; 4]> = s
        .tmux
        .iter()
        .map(|t| {
            [
                t.name.clone(),
                if t.attached { "attached" } else { "detached" }.to_owned(),
                format!(
                    "{} {}",
                    t.pane_command.as_deref().unwrap_or(""),
                    t.pane_cwd.as_deref().unwrap_or("")
                ),
                format!("{} ago", ago(t.created)),
            ]
        })
        .collect();
    print_table(&["SESSION", "STATE", "ACTIVE PANE", "CREATED"], &rows);
}

fn print_containers(s: &Snapshot) {
    heading("containers", s.containers.len());
    let rows: Vec<[String; 4]> = s
        .containers
        .iter()
        .map(|c| {
            [
                c.name.clone(),
                c.image.clone(),
                c.status.clone(),
                c.compose_project.clone().unwrap_or_default(),
            ]
        })
        .collect();
    print_table(&["NAME", "IMAGE", "STATUS", "COMPOSE"], &rows);
}

fn print_listeners(s: &Snapshot) {
    heading("listening ports", s.listeners.len());
    let rows: Vec<[String; 3]> = s
        .listeners
        .iter()
        .map(|l| {
            [
                format!("{}:{}", l.addr, l.port),
                l.process.clone().unwrap_or_default(),
                l.pid.map(|p| p.to_string()).unwrap_or_default(),
            ]
        })
        .collect();
    print_table(&["ADDRESS", "PROCESS", "PID"], &rows);
}

fn print_repos(s: &Snapshot) {
    heading("repos", s.repos.len());
    let rows: Vec<[String; 4]> = s
        .repos
        .iter()
        .map(|r| {
            let sync = match (r.ahead, r.behind) {
                (Some(a), Some(b)) if a + b > 0 => format!("+{a} -{b}"),
                _ => String::new(),
            };
            [
                r.path.clone(),
                format!("{} {sync}", r.branch.as_deref().unwrap_or("(detached)")),
                if r.dirty == 0 {
                    "clean".to_owned()
                } else {
                    format!("{} modified", r.dirty)
                },
                r.last_commit
                    .as_ref()
                    .map(|c| format!("{} ago: {}", ago(c.time), c.subject))
                    .unwrap_or_default(),
            ]
        })
        .collect();
    print_table(&["PATH", "BRANCH", "CHANGES", "LAST COMMIT"], &rows);
}

fn print_processes(s: &Snapshot) {
    let mut procs: Vec<_> = s.processes.iter().collect();
    procs.sort_by_key(|p| std::cmp::Reverse(p.started_at));
    heading("processes", procs.len());
    let rows: Vec<[String; 4]> = procs
        .iter()
        .take(MAX_PROCESSES)
        .map(|p| {
            [
                p.pid.to_string(),
                ago(p.started_at),
                p.cwd.clone().unwrap_or_default(),
                p.cmdline.chars().take(90).collect(),
            ]
        })
        .collect();
    print_table(&["PID", "RUNNING", "CWD", "COMMAND"], &rows);
    if procs.len() > MAX_PROCESSES {
        println!("... {} more", procs.len() - MAX_PROCESSES);
    }
}

pub async fn timeline(db: &Db, name: &str, limit: u32) -> anyhow::Result<()> {
    let (vm, _) = remote::resolve(db, name).await?;
    let mut entries = db.timeline(vm.id, limit, None).await?;
    if entries.is_empty() {
        println!(
            "no events for {} yet; `vmdesk show {}` takes a snapshot",
            vm.name, vm.name
        );
        return Ok(());
    }
    entries.reverse();
    for e in &entries {
        println!("{}  {:<20} {}", local(e.ts), e.kind, e.summary());
    }
    Ok(())
}

pub(crate) fn local(ts: DateTime<Utc>) -> String {
    ts.with_timezone(&Local).format("%m-%d %H:%M").to_string()
}
