//! The Overview tab: the VM's last snapshot as a stack of cards.

use chrono::Utc;
use gpui::{AnyElement, Div, FontWeight, div, prelude::*, px, rgb};
use vmdesk_proto::Snapshot;

use crate::human::{ago, duration};
use crate::kit::{Col, card, col, count_badge, label, mono, table};
use crate::theme::{color, radius, space, text};

/// Cards top to bottom.
pub fn render(s: &Snapshot) -> Div {
    let cards: Vec<AnyElement> = vec![
        system_card(s).into_any_element(),
        tmux_card(s).into_any_element(),
        containers_card(s).into_any_element(),
        ports_card(s).into_any_element(),
        repos_card(s).into_any_element(),
        processes_card(s).into_any_element(),
    ];
    div()
        .flex()
        .flex_col()
        .gap(space::L)
        .children(cards)
        .when(!s.unavailable.is_empty(), |d| {
            d.child(label(format!(
                "Not collected: {}",
                s.unavailable.join("; ")
            )))
        })
}

fn since(unix: i64) -> String {
    format!("{} ago", ago(Utc::now().timestamp() - unix))
}

fn gb(kb: u64) -> String {
    const GIB: u64 = 1_048_576;
    format!("{}.{}G", kb / GIB, kb % GIB * 10 / GIB)
}

/// One figure with its caption, and a usage bar when `used` is a share of a whole.
fn stat(caption: &str, value: String, used: Option<(u64, u64)>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .min_w(px(132.))
        .child(label(caption.to_owned()))
        .child(
            div()
                .text_size(text::BODY)
                .font_weight(FontWeight::MEDIUM)
                .child(value),
        )
        .when_some(used, |d, (used, total)| {
            #[allow(clippy::cast_precision_loss)] // a bar's width needs no precision
            let share = if total == 0 {
                0.
            } else {
                (used as f32 / total as f32).clamp(0., 1.)
            };
            let fill = match share {
                s if s >= 0.9 => color::RED,
                s if s >= 0.75 => color::AMBER,
                _ => color::ACCENT,
            };
            d.child(
                div()
                    .mt(px(2.))
                    .w(px(132.))
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(color::HOVER))
                    .child(
                        div()
                            .h_full()
                            .w(px(132. * share))
                            .rounded_full()
                            .bg(rgb(fill)),
                    ),
            )
        })
}

fn system_card(s: &Snapshot) -> Div {
    let sys = &s.system;
    let taken = s
        .taken_at
        .map(|t| format!("Snapshot {} ago", ago((Utc::now() - t).num_seconds())));
    let mut stats = vec![
        stat(
            "Uptime",
            duration(i64::try_from(sys.uptime_secs).unwrap_or(0)),
            None,
        ),
        stat(
            "Load",
            format!(
                "{:.2}  {:.2}  {:.2}  on {} CPUs",
                sys.load[0], sys.load[1], sys.load[2], sys.cpus
            ),
            None,
        ),
        stat(
            "Memory",
            format!("{} / {}", gb(sys.mem_used_kb), gb(sys.mem_total_kb)),
            Some((sys.mem_used_kb, sys.mem_total_kb)),
        ),
    ];
    stats.extend(sys.disks.iter().map(|d| {
        stat(
            &format!("Disk {}", d.mount),
            format!("{} / {}", gb(d.used_kb), gb(d.total_kb)),
            Some((d.used_kb, d.total_kb)),
        )
    }));
    card()
        .p(space::L)
        .gap(space::L)
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(space::S)
                .child(
                    div()
                        .text_size(text::SECTION)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(sys.hostname.clone()),
                )
                .child(label(format!("{} · kernel {}", sys.os, sys.kernel)))
                .child(div().flex_1())
                .when_some(taken, |d, t| d.child(label(t))),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_x(space::XL)
                .gap_y(space::M)
                .children(stats),
        )
}

/// A card with a heading, its count, and a table or the empty text.
fn section(title: &str, empty: &str, cols: &[Col], rows: Vec<Vec<String>>) -> Div {
    let n = rows.len();
    card()
        .child(
            div()
                .px(space::L)
                .h(px(44.))
                .flex()
                .items_center()
                .gap(space::S)
                .child(
                    div()
                        .text_size(text::SECTION)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.to_owned()),
                )
                .child(count_badge(n)),
        )
        .child(if n == 0 {
            div()
                .px(space::L)
                .pb(space::L)
                .child(label(empty.to_owned()))
        } else {
            div()
                .px(space::L)
                .pb(space::XS)
                .rounded_b(radius::L)
                .child(table(cols, rows))
        })
}

fn tmux_card(s: &Snapshot) -> Div {
    section(
        "tmux sessions",
        "No tmux sessions.",
        &[
            col("Session", 180.),
            col("State", 90.),
            col("Created", 110.),
            mono("Active pane", 0.),
        ],
        s.tmux
            .iter()
            .map(|t| {
                vec![
                    t.name.clone(),
                    if t.attached { "attached" } else { "detached" }.to_owned(),
                    since(t.created),
                    [t.pane_command.as_deref(), t.pane_cwd.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("  "),
                ]
            })
            .collect(),
    )
}

fn containers_card(s: &Snapshot) -> Div {
    section(
        "Containers",
        "No containers.",
        &[
            col("Name", 200.),
            mono("Image", 240.),
            col("Status", 180.),
            col("Compose", 0.),
        ],
        s.containers
            .iter()
            .map(|c| {
                vec![
                    c.name.clone(),
                    c.image.clone(),
                    c.status.clone(),
                    c.compose_project.clone().unwrap_or_default(),
                ]
            })
            .collect(),
    )
}

fn ports_card(s: &Snapshot) -> Div {
    section(
        "Listening ports",
        "Nothing is listening.",
        &[mono("Address", 220.), col("PID", 90.), col("Process", 0.)],
        s.listeners
            .iter()
            .map(|l| {
                vec![
                    format!("{}:{}", l.addr, l.port),
                    l.pid.map(|p| p.to_string()).unwrap_or_default(),
                    l.process.clone().unwrap_or_default(),
                ]
            })
            .collect(),
    )
}

fn repos_card(s: &Snapshot) -> Div {
    section(
        "Git repositories",
        "No git repositories found.",
        &[
            mono("Path", 260.),
            col("Branch", 180.),
            col("Changes", 110.),
            col("Last commit", 0.),
        ],
        s.repos
            .iter()
            .map(|r| {
                let branch = r.branch.as_deref().unwrap_or("(detached)");
                vec![
                    r.path.clone(),
                    match (r.ahead, r.behind) {
                        (Some(a), Some(b)) if a + b > 0 => {
                            format!("{branch} · {a} ahead, {b} behind")
                        }
                        _ => branch.to_owned(),
                    },
                    if r.dirty == 0 {
                        "clean".into()
                    } else {
                        format!("{} modified", r.dirty)
                    },
                    r.last_commit
                        .as_ref()
                        .map(|c| format!("{} · {}", c.subject, since(c.time)))
                        .unwrap_or_default(),
                ]
            })
            .collect(),
    )
}

fn processes_card(s: &Snapshot) -> Div {
    let mut procs: Vec<_> = s.processes.iter().collect();
    procs.sort_by_key(|p| std::cmp::Reverse(p.started_at));
    section(
        "Processes",
        "No processes.",
        &[
            col("PID", 80.),
            col("Running", 90.),
            mono("Directory", 220.),
            mono("Command", 0.),
        ],
        procs
            .iter()
            .map(|p| {
                vec![
                    p.pid.to_string(),
                    duration(Utc::now().timestamp() - p.started_at),
                    p.cwd.clone().unwrap_or_default(),
                    p.cmdline.clone(),
                ]
            })
            .collect(),
    )
}
