//! The Overview tab, below the current-work card: the VM's last snapshot as a dashboard.

use chrono::Utc;
use gpui::{Context, Div, FontWeight, div, prelude::*, px, rgb};
use vmdesk_core::human::{ago, duration};
use vmdesk_db::TimelineEntry;
use vmdesk_proto::Snapshot;

use crate::kit::{Col, card, col, count_badge, dot, label, mono, table};
use crate::theme::{MONO, palette, radius, space, text};
use crate::timeline_view::{describe, marker, sentence, time_cell};
use crate::ui::{AppView, Tab};

/// The Overview below the current-work card: the system as tiles, what runs and what
/// happened side by side, then the full tables.
pub fn render(s: &Snapshot, recent: &[TimelineEntry], cx: &mut Context<AppView>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(space::L)
        .child(system(s))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(space::L)
                .child(div().flex_1().min_w(px(320.)).child(running(s)))
                .child(div().flex_1().min_w(px(320.)).child(activity(recent, cx))),
        )
        .child(ports_card(s))
        .child(repos_card(s))
        .child(processes_card(s))
        .when(!s.unavailable.is_empty(), |d| {
            d.child(label(format!(
                "Not collected: {}",
                s.unavailable.join("; ")
            )))
        })
}

/// The host line over one tile per figure: uptime, load, memory and each disk.
fn system(s: &Snapshot) -> Div {
    let sys = &s.system;
    let taken = s
        .taken_at
        .map(|t| format!("snapshot {} ago", ago((Utc::now() - t).num_seconds())));
    let mut tiles = vec![
        stat(
            "Uptime",
            duration(i64::try_from(sys.uptime_secs).unwrap_or(0)),
            None,
        ),
        stat(
            &format!("Load · {} CPUs", sys.cpus),
            format!("{:.2}", sys.load[0]),
            None,
        ),
        stat(
            "Memory",
            format!("{} / {}", gb(sys.mem_used_kb), gb(sys.mem_total_kb)),
            Some((sys.mem_used_kb, sys.mem_total_kb)),
        ),
    ];
    tiles.extend(sys.disks.iter().map(|d| {
        stat(
            &format!("Disk {}", d.mount),
            format!("{} / {}", gb(d.used_kb), gb(d.total_kb)),
            Some((d.used_kb, d.total_kb)),
        )
    }));
    div()
        .flex()
        .flex_col()
        .gap(space::S)
        .child(label(
            [
                Some(sys.hostname.clone()),
                Some(sys.os.clone()),
                Some(format!("kernel {}", sys.kernel)),
                taken,
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
        ))
        .child(div().flex().flex_wrap().gap(space::M).children(tiles))
}

/// Tmux sessions and containers, one line each.
fn running(s: &Snapshot) -> Div {
    let line = |live: bool, name: String, right: String, code: bool| {
        div()
            .h(px(30.))
            .flex()
            .items_center()
            .gap(space::S)
            .child(dot(
                if live {
                    palette().green
                } else {
                    palette().text_muted
                },
                7.,
            ))
            .child(div().flex_1().min_w_0().truncate().child(name))
            .child(
                div()
                    .text_size(text::META)
                    .text_color(rgb(palette().text_muted))
                    .when(code, |d| d.font_family(MONO))
                    .child(right),
            )
    };
    let tmux = s.tmux.iter().map(|t| {
        let right = if t.attached {
            "tmux · attached".to_owned()
        } else {
            format!("tmux · {}", since(t.created))
        };
        line(t.attached, t.name.clone(), right, false)
    });
    let containers = s
        .containers
        .iter()
        .map(|c| line(c.state == "running", c.name.clone(), c.image.clone(), true));
    let rows: Vec<Div> = tmux.chain(containers).collect();
    panel(
        "Running",
        format!("{} tmux · {} containers", s.tmux.len(), s.containers.len()),
    )
    .child(if rows.is_empty() {
        label("No tmux sessions or containers.")
    } else {
        div().flex().flex_col().children(rows)
    })
}

/// The newest few events, told as the Timeline tells them.
fn activity(recent: &[TimelineEntry], cx: &mut Context<AppView>) -> Div {
    let rows: Vec<Div> = recent
        .iter()
        .take(6)
        .map(|e| {
            let r = describe(e);
            div()
                .min_h(px(30.))
                .flex()
                .items_center()
                .gap(space::S)
                .child(time_cell(&r))
                .child(marker(&r))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(sentence(&r)),
                )
        })
        .collect();
    panel("Latest activity", String::new())
        .child(
            div()
                .absolute()
                .top(space::L)
                .right(space::L)
                .id("overview-timeline")
                .cursor_pointer()
                .text_size(text::META)
                .text_color(rgb(palette().accent))
                .hover(|s| s.text_color(rgb(palette().accent_hover)))
                .child("Timeline")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.select_tab(Tab::Timeline, window, cx);
                })),
        )
        .child(if rows.is_empty() {
            label("Nothing recorded yet.")
        } else {
            div().flex().flex_col().children(rows)
        })
}

/// A titled box for one of the side-by-side lists.
fn panel(title: &'static str, meta: String) -> Div {
    card().relative().p(space::L).gap(space::S).child(
        div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(
                div()
                    .text_size(text::SECTION)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(label(meta)),
    )
}

fn since(unix: i64) -> String {
    format!("{} ago", ago(Utc::now().timestamp() - unix))
}

fn gb(kb: u64) -> String {
    const GIB: u64 = 1_048_576;
    format!("{}.{}G", kb / GIB, kb % GIB * 10 / GIB)
}

/// One figure as a tile, with a usage bar when `used` is a share of a whole.
fn stat(caption: &str, value: String, used: Option<(u64, u64)>) -> Div {
    div()
        .flex_1()
        .min_w(px(168.))
        .p(space::L)
        .flex()
        .flex_col()
        .gap(space::XS)
        .rounded(radius::L)
        .border_1()
        .border_color(rgb(palette().border))
        .bg(rgb(palette().surface))
        .child(label(caption.to_owned()))
        .child(
            div()
                .text_size(px(18.))
                .font_weight(FontWeight::SEMIBOLD)
                .whitespace_nowrap()
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
                s if s >= 0.9 => palette().red,
                s if s >= 0.75 => palette().amber,
                _ => palette().accent,
            };
            d.child(
                div()
                    .mt(px(6.))
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(palette().hover))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(share))
                            .rounded_full()
                            .bg(rgb(fill)),
                    ),
            )
        })
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
