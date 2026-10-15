//! `vmdesk status`: check reachability now, or keep watching for changes.

use chrono::Local;
use vmdesk_core::{Vm, VmStatus};
use vmdesk_db::Db;
use vmdesk_reach::{Checked, Monitor};

use crate::{print_table, remote};

/// Longest failure detail shown in the table.
const DETAIL_WIDTH: usize = 70;

/// `online (wlo1)`, `ssh failing`, `never checked`, ...
pub fn state_label(vm: &Vm, status: Option<&VmStatus>) -> String {
    match status {
        None => "never checked".to_owned(),
        Some(s) => match &s.route {
            Some(dev) => format!("{} ({dev})", s.headline(vm.lifecycle)),
            None => s.headline(vm.lifecycle).to_string(),
        },
    }
}

pub async fn status(db: &Db, names: &[String], watch: bool) -> anyhow::Result<()> {
    let mut only = Vec::new();
    for n in names {
        only.push(remote::resolve(db, n).await?.0.id);
    }
    let monitor = Monitor::new(db.clone());
    let checked = monitor
        .check_now((!only.is_empty()).then_some(only.as_slice()))
        .await?;
    let rows: Vec<[String; 4]> = checked
        .iter()
        .map(|c| {
            [
                c.vm.name.clone(),
                state_label(&c.vm, Some(&c.status)),
                c.status.ssh.to_string(),
                c.status.detail.as_deref().map(short).unwrap_or_default(),
            ]
        })
        .collect();
    print_table(&["NAME", "STATE", "SSH", "DETAIL"], &rows);
    if !watch {
        return Ok(());
    }

    eprintln!(
        "\nwatching for changes (network changes, favourites every 1 min, others every 5 min); Ctrl-C to stop"
    );
    let scope = (!only.is_empty()).then_some(only.as_slice());
    monitor
        .watch(scope, false, |c: &Checked| {
            let before = c
                .previous
                .as_ref()
                .map(|p| state_label(&c.vm, Some(p)))
                .unwrap_or_default();
            println!(
                "{}  {}: {before} -> {}  [ssh {}]",
                Local::now().format("%H:%M:%S"),
                c.vm.name,
                state_label(&c.vm, Some(&c.status)),
                c.status.ssh,
            );
        })
        .await?;
    Ok(())
}

fn short(detail: &str) -> String {
    let line = detail.lines().next().unwrap_or_default();
    if line.chars().count() <= DETAIL_WIDTH {
        line.to_owned()
    } else {
        let cut: String = line.chars().take(DETAIL_WIDTH - 3).collect();
        format!("{cut}...")
    }
}
