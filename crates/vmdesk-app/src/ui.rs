//! The main window: VM list on the left, the selected VM on the right.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, KeyDownEvent, SharedString, Window, div,
    prelude::*, px, rgb,
};
use vmdesk_core::{Headline, Hop, SshState, Vm, VmId, VmStatus, human_duration};
use vmdesk_db::TimelineEntry;
use vmdesk_proto::Snapshot;

use crate::backend::{Backend, Detail, Inventory, KeyProblem};
use crate::form::AddForm;

/// How often the list re-reads the database (the monitor writes status there).
const RELOAD: Duration = Duration::from_secs(5);
/// The selected VM is re-collected this often while it is open (spec section 9).
const OPEN_VM_INTERVAL: Duration = Duration::from_mins(2);
const MONO: &str = "DejaVu Sans Mono";

#[allow(clippy::unreadable_literal)] // hex colours read best as written
pub(crate) mod color {
    pub const BG: u32 = 0x1b1d21;
    pub const PANEL: u32 = 0x22252a;
    pub const ROW_HOVER: u32 = 0x2b2f35;
    pub const SELECTED: u32 = 0x2f3b4d;
    pub const BORDER: u32 = 0x33373e;
    pub const TEXT: u32 = 0xdcdfe4;
    pub const DIM: u32 = 0x8a919c;
    pub const ACCENT: u32 = 0x6ea8fe;
    pub const GREEN: u32 = 0x5fb878;
    pub const AMBER: u32 = 0xe0a84e;
    pub const RED: u32 = 0xe06c75;
    pub const GREY: u32 = 0x6b7280;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Timeline,
}

pub struct AppView {
    backend: Backend,
    focus: FocusHandle,
    vms: Vec<Vm>,
    status: HashMap<VmId, VmStatus>,
    filter: String,
    selected: Option<VmId>,
    detail: Option<Detail>,
    tab: Tab,
    /// When the selected VM was last collected by this window.
    collected_at: Option<Instant>,
    busy: Option<&'static str>,
    message: Option<(String, bool)>,
    /// VM to select once the inventory has loaded (from the command line).
    pending_select: Option<String>,
    /// The "Add VM" form, while it is open.
    pub(crate) add: Option<AddForm>,
    /// VM waiting for the user to confirm its removal.
    confirm_remove: Option<VmId>,
    /// A host key the user is deciding on, for the selected VM.
    key_prompt: Option<KeyProblem>,
}

impl AppView {
    pub fn new(backend: Backend, open_vm: Option<String>, cx: &mut Context<Self>) -> Self {
        Self {
            backend,
            focus: cx.focus_handle(),
            vms: Vec::new(),
            status: HashMap::new(),
            filter: String::new(),
            selected: None,
            detail: None,
            tab: Tab::Overview,
            collected_at: None,
            busy: None,
            message: None,
            pending_select: open_vm,
            add: None,
            confirm_remove: None,
            key_prompt: None,
        }
    }

    /// Re-reads the inventory every [`RELOAD`] for as long as the window is open.
    pub fn start(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(load) = this.update(cx, |v, _| v.backend.inventory()) else {
                    break;
                };
                let inventory = load.await;
                let alive = this
                    .update(cx, |v, cx| {
                        v.apply_inventory(inventory, cx);
                        v.refresh_open_vm(cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(RELOAD).await;
            }
        })
        .detach();
    }

    /// Reads the inventory once.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let load = self.backend.inventory();
        cx.spawn(async move |this, cx| {
            let inventory = load.await;
            this.update(cx, |v, cx| v.apply_inventory(inventory, cx))
                .ok();
        })
        .detach();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn apply_inventory(&mut self, inv: anyhow::Result<Inventory>, cx: &mut Context<Self>) {
        match inv {
            // The 5-second reload usually finds nothing new: skip the redraw.
            Ok(inv)
                if inv.vms == self.vms
                    && inv.status == self.status
                    && self.pending_select.is_none() =>
            {
                return;
            }
            Ok(inv) => {
                self.vms = inv.vms;
                self.status = inv.status;
                if let Some(want) = self.pending_select.take() {
                    match find_vm(&self.vms, &want) {
                        Some(id) => self.select(id, cx),
                        None => self.message = Some((format!("no VM named {want:?}"), true)),
                    }
                }
                if self
                    .selected
                    .is_some_and(|id| !self.vms.iter().any(|v| v.id == id))
                {
                    self.selected = None;
                    self.detail = None;
                }
            }
            Err(e) => self.message = Some((format!("loading inventory: {e:#}"), true)),
        }
        cx.notify();
    }

    /// Re-collects the open VM every [`OPEN_VM_INTERVAL`] while it is reachable.
    fn refresh_open_vm(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        let online = self.status.get(&id).is_some_and(|s| s.ssh == SshState::Ok);
        let due = self
            .collected_at
            .is_none_or(|t| t.elapsed() >= OPEN_VM_INTERVAL);
        if online && due && self.busy.is_none() {
            self.collect(id, cx);
        }
    }

    fn select(&mut self, id: VmId, cx: &mut Context<Self>) {
        if self.selected == Some(id) {
            return;
        }
        self.selected = Some(id);
        self.detail = None;
        self.key_prompt = None;
        self.confirm_remove = None;
        self.collected_at = None;
        self.message = None;
        self.load_detail(id, cx);
        cx.notify();
    }

    fn load_detail(&mut self, id: VmId, cx: &mut Context<Self>) {
        let load = self.backend.detail(id);
        cx.spawn(async move |this, cx| {
            let detail = load.await;
            this.update(cx, |v, cx| {
                if v.selected != Some(id) {
                    return; // the user moved on
                }
                match detail {
                    Ok(d) => {
                        // Show the stored snapshot now; collect a fresh one if it is stale.
                        let stale = d
                            .snapshot
                            .as_ref()
                            .and_then(|s| s.taken_at)
                            .is_none_or(|t| {
                                Utc::now() - t
                                    > chrono::Duration::from_std(OPEN_VM_INTERVAL)
                                        .unwrap_or_default()
                            });
                        if !stale && v.collected_at.is_none() {
                            v.collected_at = Some(Instant::now());
                        }
                        v.detail = Some(d);
                    }
                    Err(e) => v.message = Some((format!("{e:#}"), true)),
                }
                v.refresh_open_vm(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn collect(&mut self, id: VmId, cx: &mut Context<Self>) {
        self.busy = Some("collecting");
        self.collected_at = Some(Instant::now());
        let job = self.backend.collect(id);
        cx.spawn(async move |this, cx| {
            let res = job.await;
            this.update(cx, |v, cx| {
                v.busy = None;
                if let Err(e) = res {
                    v.message = Some((format!("collect failed: {e:#}"), true));
                }
                if v.selected == Some(id) {
                    v.load_detail(id, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn check(&mut self, id: VmId, cx: &mut Context<Self>) {
        self.busy = Some("checking");
        let job = self.backend.check(id);
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let res = job.await;
            // Read the inventory only once the check has stored its result.
            let inv = backend.inventory().await;
            this.update(cx, |v, cx| {
                v.busy = None;
                if let Err(e) = res {
                    v.message = Some((format!("check failed: {e:#}"), true));
                }
                v.apply_inventory(inv, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn open_terminal(&mut self, id: VmId, cx: &mut Context<Self>) {
        let job = self.backend.open_terminal(id);
        cx.spawn(async move |this, cx| {
            let res = job.await;
            this.update(cx, |v, cx| {
                v.message = Some(match res {
                    Ok(m) => (m, false),
                    Err(e) => (format!("open terminal: {e:#}"), true),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Typing anywhere filters the list; Backspace edits, Escape clears.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if let Some(form) = &self.add {
            match ks.key.as_str() {
                "tab" => form.tab(ks.modifiers.shift, window, cx),
                "enter" => self.save_new_vm(cx),
                "escape" => {
                    self.add = None;
                    window.focus(&self.focus, cx);
                }
                _ => {}
            }
            cx.notify();
            return;
        }
        if ks.modifiers.control || ks.modifiers.alt || ks.modifiers.platform {
            return;
        }
        match ks.key.as_str() {
            "up" | "down" => {
                self.step_selection(ks.key == "up", cx);
                return;
            }
            "backspace" => {
                self.filter.pop();
            }
            "escape" => self.filter.clear(),
            _ => match &ks.key_char {
                Some(c) if !c.chars().any(char::is_control) => self.filter.push_str(c),
                _ => return,
            },
        }
        cx.notify();
    }

    /// Finds out which host key blocks `id` and shows it for a decision.
    fn review_host_key(&mut self, id: VmId, cx: &mut Context<Self>) {
        self.busy = Some("checking host key");
        let job = self.backend.host_key_problem(id);
        cx.spawn(async move |this, cx| {
            let res = job.await;
            this.update(cx, |v, cx| {
                v.busy = None;
                match res {
                    Ok(Some(p)) => v.key_prompt = Some(p),
                    Ok(None) => {
                        v.message = Some(("no host key problem any more".to_owned(), false));
                        v.check(id, cx);
                    }
                    Err(e) => v.message = Some((format!("{e:#}"), true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Acts on the user's decision, then rechecks the selected VM (whose path may still be
    /// blocked by another hop's key).
    fn resolve_host_key(&mut self, trust: bool, cx: &mut Context<Self>) {
        let (Some(problem), Some(selected)) = (self.key_prompt.take(), self.selected) else {
            return;
        };
        let job: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>>>> =
            match (&problem, trust) {
                (KeyProblem::Unknown { vm, key, .. }, true) => {
                    Box::pin(self.backend.trust_host_key(*vm, key.clone()))
                }
                (KeyProblem::Changed { vm, .. }, true) => {
                    Box::pin(self.backend.forget_host_keys(*vm))
                }
                (_, false) => {
                    cx.notify();
                    return;
                }
            };
        let changed = matches!(problem, KeyProblem::Changed { .. });
        cx.spawn(async move |this, cx| {
            let res = job.await;
            this.update(cx, |v, cx| {
                match res {
                    // A forgotten key is now unknown: show the new one to trust.
                    Ok(()) if changed => v.review_host_key(selected, cx),
                    Ok(()) => v.check(selected, cx),
                    Err(e) => v.message = Some((format!("{e:#}"), true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_edit_form(&mut self, id: VmId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vm) = self.vms.iter().find(|v| v.id == id) else {
            return;
        };
        let form = AddForm::edit(vm, cx);
        window.focus(&form.first_field(cx), cx);
        self.add = Some(form);
        cx.notify();
    }

    fn open_add_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = AddForm::new(cx);
        window.focus(&form.first_field(cx), cx);
        self.add = Some(form);
        cx.notify();
    }

    pub(crate) fn save_new_vm(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.add.as_mut() else {
            return;
        };
        if form.saving {
            return;
        }
        let spec = match form.spec(cx) {
            Ok(spec) => spec,
            Err(e) => {
                form.error = Some(e);
                cx.notify();
                return;
            }
        };
        form.saving = true;
        form.error = None;
        let job: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<VmId>>>> =
            match &form.editing {
                Some(o) => {
                    let id = o.id;
                    let endpoint_changed = spec.address != o.address || spec.port != o.port;
                    let via_changed = spec.via != o.via;
                    let save = self
                        .backend
                        .update_vm(id, spec, endpoint_changed, via_changed);
                    Box::pin(async move { save.await.map(|()| id) })
                }
                None => Box::pin(self.backend.add_vm(spec)),
            };
        cx.spawn(async move |this, cx| {
            let res = job.await;
            this.update(cx, |v, cx| match res {
                Ok(id) => {
                    v.add = None;
                    v.pending_select = Some(id.to_string());
                    v.message = None;
                    v.reload(cx);
                    // Status appears once the first check is done.
                    v.check(id, cx);
                }
                Err(e) => {
                    if let Some(f) = v.add.as_mut() {
                        f.saving = false;
                        f.error = Some(format!("{e:#}"));
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn remove(&mut self, id: VmId, cx: &mut Context<Self>) {
        self.confirm_remove = None;
        let job = self.backend.remove_vm(id);
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let inv = backend.inventory().await;
            this.update(cx, |v, cx| {
                v.message = Some(match res {
                    Ok(m) => (m, false),
                    Err(e) => (format!("remove failed: {e:#}"), true),
                });
                v.apply_inventory(inv, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Moves the selection one row up or down the filtered list.
    fn step_selection(&mut self, up: bool, cx: &mut Context<Self>) {
        let ids: Vec<VmId> = self.visible().iter().map(|v| v.id).collect();
        let at = self.selected.and_then(|s| ids.iter().position(|i| *i == s));
        let next = match at {
            None => 0,
            Some(i) if up => i.saturating_sub(1),
            Some(i) => (i + 1).min(ids.len().saturating_sub(1)),
        };
        if let Some(id) = ids.get(next) {
            self.select(*id, cx);
        }
    }

    fn visible(&self) -> Vec<&Vm> {
        let q = self.filter.to_lowercase();
        let mut vms: Vec<&Vm> = self
            .vms
            .iter()
            .filter(|v| {
                q.is_empty()
                    || v.name.to_lowercase().contains(&q)
                    || v.tags.iter().any(|t| t.to_lowercase().contains(&q))
                    || v.endpoints.iter().any(|e| e.address.contains(&q))
            })
            .collect();
        vms.sort_by_key(|v| (!v.favourite, v.name.to_lowercase()));
        vms
    }

    fn headline(&self, vm: &Vm) -> Option<Headline> {
        self.status.get(&vm.id).map(|s| s.headline(vm.lifecycle))
    }
}

/// A VM by name, alias or id.
fn find_vm(vms: &[Vm], want: &str) -> Option<VmId> {
    vms.iter()
        .find(|v| {
            v.name.eq_ignore_ascii_case(want)
                || v.id.to_string() == want
                || v.labels
                    .get(vmdesk_core::ALIASES_LABEL)
                    .is_some_and(|a| a.split(',').any(|x| x.trim().eq_ignore_ascii_case(want)))
        })
        .map(|v| v.id)
}

fn headline_color(h: Option<Headline>) -> u32 {
    match h {
        Some(Headline::Online) => color::GREEN,
        Some(Headline::SshFailing) => color::AMBER,
        Some(Headline::NoRoute) => color::RED,
        Some(Headline::Stopped | Headline::Unknown) | None => color::GREY,
    }
}

/// Confirmation before removing a VM.
fn remove_panel(vm: &Vm, cx: &mut Context<AppView>) -> gpui::Div {
    let id = vm.id;
    div()
        .mt_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(color::RED))
        .flex()
        .items_center()
        .gap_3()
        .child(div().flex_1().text_sm().child(format!(
            "Remove {} from VMDesk? Its history, host keys and stored password are deleted; the VM itself is not touched.",
            vm.name
        )))
        .child(button("remove-yes", "Remove").on_click(cx.listener(move |this, _, _, cx| this.remove(id, cx))))
        .child(button("remove-no", "Cancel").on_click(cx.listener(|this, _, _, cx| {
            this.confirm_remove = None;
            cx.notify();
        })))
}

/// The host-key decision: trust a new key (with how to check it), or forget a changed one.
fn key_panel(p: &KeyProblem, cx: &mut Context<AppView>) -> gpui::Div {
    let (border, lines, action): (u32, Vec<String>, &'static str) = match p {
        KeyProblem::Unknown { name, key, known_hosts_match, .. } => (
            color::AMBER,
            vec![
                format!("First connection to {name}. It presents this host key:"),
                format!("{} {}", key.key_type, key.fingerprint),
                match known_hosts_match {
                    Some(true) => "✓ Matches the key in your ~/.ssh/known_hosts.".to_owned(),
                    Some(false) => "✗ Differs from the key in your ~/.ssh/known_hosts. Do not trust it unless you know why.".to_owned(),
                    None => "Check it on the VM: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub".to_owned(),
                },
            ],
            "Trust this key",
        ),
        KeyProblem::Changed { name, expected, offered, .. } => (
            color::RED,
            vec![
                format!("The host key of {name} has CHANGED."),
                format!("trusted: {expected}"),
                format!("offered: {offered}"),
                "Expected only if the VM was reinstalled; otherwise someone may be intercepting the connection.".to_owned(),
            ],
            "Forget old key",
        ),
    };
    div()
        .mt_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(border))
        .flex()
        .flex_col()
        .gap_1()
        .children(lines.into_iter().enumerate().map(|(i, l)| {
            let line = div().text_sm().child(l);
            if i == 1 { line.font_family(MONO) } else { line }
        }))
        .child(
            div()
                .flex()
                .gap_2()
                .mt_1()
                .child(
                    button("key-yes", action)
                        .on_click(cx.listener(|this, _, _, cx| this.resolve_host_key(true, cx))),
                )
                .child(
                    button("key-no", "Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.resolve_host_key(false, cx))),
                ),
        )
}

pub(crate) fn label(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_color(rgb(color::DIM))
        .text_xs()
        .child(text.into())
}

pub(crate) fn button(id: &'static str, text: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(color::BORDER))
        .bg(rgb(color::PANEL))
        .hover(|s| s.bg(rgb(color::ROW_HOVER)))
        .cursor_pointer()
        .text_sm()
        .child(text)
}

fn section(title: &str, count: usize) -> gpui::Div {
    div()
        .mt_4()
        .mb_1()
        .flex()
        .gap_2()
        .items_baseline()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(label(count.to_string()))
}

/// A row of fixed-width monospace cells; the last cell takes the rest.
fn row(cells: Vec<(f32, String)>, dim: bool) -> gpui::Div {
    let n = cells.len();
    div()
        .flex()
        .gap_3()
        .py(px(2.))
        .font_family(MONO)
        .text_xs()
        .when(dim, |d| d.text_color(rgb(color::DIM)))
        .children(cells.into_iter().enumerate().map(|(i, (w, text))| {
            let cell = div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(text);
            if i + 1 == n {
                cell.flex_1()
            } else {
                cell.w(px(w)).flex_shrink_0()
            }
        }))
}

fn table(headers: &[(f32, &str)], rows: Vec<Vec<String>>) -> gpui::Div {
    let head = row(
        headers.iter().map(|(w, h)| (*w, (*h).to_owned())).collect(),
        true,
    );
    div().child(head).children(
        rows.into_iter()
            .map(|r| row(headers.iter().map(|(w, _)| *w).zip(r).collect(), false)),
    )
}

fn state_text(vm: &Vm, status: Option<&VmStatus>) -> String {
    match status {
        Some(s) => format!(
            "{} · ssh {} · route {} · checked {} ago",
            s.headline(vm.lifecycle),
            s.ssh,
            s.route.as_deref().unwrap_or("none"),
            human_duration((Utc::now() - s.checked_at).num_seconds())
        ),
        None => "never checked".to_owned(),
    }
}

fn ago(unix: i64) -> String {
    human_duration(Utc::now().timestamp() - unix)
}

fn gb(kb: u64) -> String {
    const GIB: u64 = 1_048_576;
    format!("{}.{}G", kb / GIB, kb % GIB * 10 / GIB)
}

impl AppView {
    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let vms = self.visible();
        let online = self
            .vms
            .iter()
            .filter(|v| self.headline(v) == Some(Headline::Online))
            .count();
        div()
            .w(px(300.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(color::PANEL))
            .border_r_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Machines"),
                            )
                            .child(button("add-vm", "+ Add VM").on_click(
                                cx.listener(|this, _, window, cx| this.open_add_form(window, cx)),
                            )),
                    )
                    .child(label(format!("{online} online · {} VMs", self.vms.len())))
                    .child(
                        div()
                            .mt_1()
                            .text_sm()
                            .text_color(rgb(if self.filter.is_empty() {
                                color::DIM
                            } else {
                                color::TEXT
                            }))
                            .child(if self.filter.is_empty() {
                                "type to filter".to_owned()
                            } else {
                                format!("filter: {}▏", self.filter)
                            }),
                    ),
            )
            .child(
                div()
                    .id("vm-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(vms.into_iter().map(|vm| {
                        let id = vm.id;
                        let h = self.headline(vm);
                        let status = self.status.get(&vm.id);
                        let sub = match (h, status.and_then(|s| s.route.as_deref())) {
                            (Some(h), Some(r)) => format!("{h} · {r}"),
                            (Some(h), None) => h.to_string(),
                            (None, _) => "never checked".to_owned(),
                        };
                        div()
                            .id(SharedString::from(vm.id.to_string()))
                            .px_3()
                            .py_1()
                            .flex()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .when(self.selected == Some(id), |d| d.bg(rgb(color::SELECTED)))
                            .when(self.selected != Some(id), |d| {
                                d.hover(|s| s.bg(rgb(color::ROW_HOVER)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
                            .child(div().size(px(8.)).rounded_full().bg(rgb(headline_color(h))))
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .text_sm()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(vm.name.clone()),
                                    )
                                    .child(label(sub)),
                            )
                    })),
            )
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(form) = &self.add {
            return div()
                .id("add-form")
                .flex_1()
                .h_full()
                .overflow_y_scroll()
                .child(crate::form::render(form, &self.vms, cx))
                .into_any_element();
        }
        let Some(vm) = self
            .selected
            .and_then(|id| self.vms.iter().find(|v| v.id == id))
        else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(color::DIM))
                .child("Select a VM")
                .into_any_element();
        };
        let header = self.render_header(vm, cx);

        let body = match (&self.detail, self.tab) {
            (None, _) => div().child(label("loading…")).into_any_element(),
            (Some(d), Tab::Overview) => match &d.snapshot {
                Some(s) => overview(s).into_any_element(),
                None => div()
                    .child(label(
                        "No snapshot yet. Refresh collects one once the VM is reachable.",
                    ))
                    .into_any_element(),
            },
            (Some(d), Tab::Timeline) => timeline(&d.timeline).into_any_element(),
        };

        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(
                div()
                    .id("detail-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_4()
                    .pb_4()
                    .child(body),
            )
            .into_any_element()
    }

    /// `direct`, or `via bastion → inner` for the VM's preferred path.
    fn path_text(&self, vm: &Vm) -> String {
        let via: Vec<&str> = vm
            .paths
            .first()
            .map(|p| {
                p.hops
                    .iter()
                    .filter_map(|h| match h {
                        Hop::Jump { via } => self
                            .vms
                            .iter()
                            .find(|v| v.id == *via)
                            .map(|v| v.name.as_str()),
                        Hop::Direct => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if via.is_empty() {
            "direct".to_owned()
        } else {
            format!("via {}", via.join(" → "))
        }
    }

    fn render_header(&self, vm: &Vm, cx: &mut Context<Self>) -> gpui::Div {
        let id = vm.id;
        let status = self.status.get(&id);
        let ep = vm
            .endpoints
            .first()
            .map(|e| format!("{}:{}", e.address, e.port))
            .unwrap_or_default();
        let path = self.path_text(vm);
        let state_line = state_text(vm, status);

        div()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(vm.name.clone()),
                    )
                    .child(label(format!("{}@{ep} · {path}", vm.username)))
                    .child(div().flex_1())
                    .when_some(self.busy, |d, b| d.child(label(format!("{b}…"))))
                    .child(
                        button("check", "Check")
                            .on_click(cx.listener(move |this, _, _, cx| this.check(id, cx))),
                    )
                    .child(
                        button("collect", "Refresh")
                            .on_click(cx.listener(move |this, _, _, cx| this.collect(id, cx))),
                    )
                    .child(
                        button("terminal", "Open terminal").on_click(
                            cx.listener(move |this, _, _, cx| this.open_terminal(id, cx)),
                        ),
                    )
                    .child(button("edit", "Edit").on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.open_edit_form(id, window, cx);
                        },
                    )))
                    .child(button("remove", "Remove").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.confirm_remove = Some(id);
                            cx.notify();
                        },
                    ))),
            )
            .when(self.confirm_remove == Some(id), |d| {
                d.child(remove_panel(vm, cx))
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(8.)).rounded_full().bg(rgb(headline_color(
                        status.map(|s| s.headline(vm.lifecycle)),
                    ))))
                    .child(label(state_line)),
            )
            .when(
                status.is_some_and(|s| s.ssh == SshState::HostKey) && self.key_prompt.is_none(),
                |d| {
                    d.child(div().flex().child(
                        button("review-key", "Review host key…").on_click(
                            cx.listener(move |this, _, _, cx| this.review_host_key(id, cx)),
                        ),
                    ))
                },
            )
            .when_some(self.key_prompt.clone(), |d, p| d.child(key_panel(&p, cx)))
            .when_some(status.and_then(|s| s.detail.clone()), |d, detail| {
                d.child(div().text_xs().text_color(rgb(color::AMBER)).child(detail))
            })
            .when_some(self.message.clone(), |d, (m, err)| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(if err { color::RED } else { color::ACCENT }))
                        .child(m),
                )
            })
            .child(
                div()
                    .mt_2()
                    .flex()
                    .gap_4()
                    .child(self.tab_button("tab-overview", "Overview", Tab::Overview, cx))
                    .child(self.tab_button("tab-timeline", "Timeline", Tab::Timeline, cx)),
            )
    }

    fn tab_button(
        &self,
        id: &'static str,
        text: &'static str,
        tab: Tab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.tab == tab;
        div()
            .id(id)
            .pb_1()
            .cursor_pointer()
            .text_sm()
            .when(active, |d| d.border_b_2().border_color(rgb(color::ACCENT)))
            .when(!active, |d| d.text_color(rgb(color::DIM)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tab = tab;
                cx.notify();
            }))
            .child(text)
    }
}

fn overview(s: &Snapshot) -> gpui::Div {
    let sys = &s.system;
    let taken = s
        .taken_at
        .map(|t| {
            format!(
                "snapshot {} ago",
                human_duration((Utc::now() - t).num_seconds())
            )
        })
        .unwrap_or_default();
    let disks: Vec<String> = sys
        .disks
        .iter()
        .map(|d| format!("{} {}/{}", d.mount, gb(d.used_kb), gb(d.total_kb)))
        .collect();

    div()
        .pt_3()
        .child(label(format!(
            "{} · {} · kernel {} · {taken}",
            sys.hostname, sys.os, sys.kernel
        )))
        .child(label(format!(
            "up {} · load {:.2} {:.2} {:.2} on {} cpus · mem {} / {} · disks {}",
            human_duration(i64::try_from(sys.uptime_secs).unwrap_or(0)),
            sys.load[0],
            sys.load[1],
            sys.load[2],
            sys.cpus,
            gb(sys.mem_used_kb),
            gb(sys.mem_total_kb),
            disks.join(", ")
        )))
        .child(tmux_section(s))
        .child(containers_section(s))
        .child(ports_section(s))
        .child(repos_section(s))
        .child(processes_section(s))
        .when(!s.unavailable.is_empty(), |d| {
            d.child(div().mt_4().child(label(format!(
                "not collected: {}",
                s.unavailable.join("; ")
            ))))
        })
}

fn tmux_section(s: &Snapshot) -> gpui::Div {
    div().child(section("tmux", s.tmux.len())).child(table(
        &[
            (160., "SESSION"),
            (80., "STATE"),
            (90., "CREATED"),
            (0., "ACTIVE PANE"),
        ],
        s.tmux
            .iter()
            .map(|t| {
                vec![
                    t.name.clone(),
                    if t.attached { "attached" } else { "detached" }.to_owned(),
                    format!("{} ago", ago(t.created)),
                    format!(
                        "{} {}",
                        t.pane_command.as_deref().unwrap_or(""),
                        t.pane_cwd.as_deref().unwrap_or("")
                    ),
                ]
            })
            .collect(),
    ))
}

fn containers_section(s: &Snapshot) -> gpui::Div {
    div()
        .child(section("containers", s.containers.len()))
        .child(table(
            &[
                (200., "NAME"),
                (220., "IMAGE"),
                (170., "STATUS"),
                (0., "COMPOSE"),
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
        ))
}

fn ports_section(s: &Snapshot) -> gpui::Div {
    div()
        .child(section("listening ports", s.listeners.len()))
        .child(table(
            &[(200., "ADDRESS"), (80., "PID"), (0., "PROCESS")],
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
        ))
}

fn repos_section(s: &Snapshot) -> gpui::Div {
    div().child(section("repos", s.repos.len())).child(table(
        &[
            (260., "PATH"),
            (200., "BRANCH"),
            (100., "CHANGES"),
            (0., "LAST COMMIT"),
        ],
        s.repos
            .iter()
            .map(|r| {
                vec![
                    r.path.clone(),
                    match (r.ahead, r.behind) {
                        (Some(a), Some(b)) if a + b > 0 => {
                            format!("{} +{a} -{b}", r.branch.as_deref().unwrap_or("(detached)"))
                        }
                        _ => r.branch.clone().unwrap_or_else(|| "(detached)".into()),
                    },
                    if r.dirty == 0 {
                        "clean".into()
                    } else {
                        format!("{} modified", r.dirty)
                    },
                    r.last_commit
                        .as_ref()
                        .map(|c| format!("{} ago: {}", ago(c.time), c.subject))
                        .unwrap_or_default(),
                ]
            })
            .collect(),
    ))
}

fn processes_section(s: &Snapshot) -> gpui::Div {
    let mut procs: Vec<_> = s.processes.iter().collect();
    procs.sort_by_key(|p| std::cmp::Reverse(p.started_at));
    div().child(section("processes", procs.len())).child(table(
        &[
            (70., "PID"),
            (80., "RUNNING"),
            (200., "CWD"),
            (0., "COMMAND"),
        ],
        procs
            .iter()
            .map(|p| {
                vec![
                    p.pid.to_string(),
                    ago(p.started_at),
                    p.cwd.clone().unwrap_or_default(),
                    p.cmdline.clone(),
                ]
            })
            .collect(),
    ))
}

fn timeline(entries: &[TimelineEntry]) -> gpui::Div {
    if entries.is_empty() {
        return div()
            .pt_3()
            .child(label("No events yet. They appear as snapshots change."));
    }
    div().pt_3().children(entries.iter().map(|e| {
        let local =
            e.ts.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string();
        row(
            vec![(90., local), (150., e.kind.clone()), (0., e.summary())],
            false,
        )
    }))
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self
            .selected
            .and_then(|id| self.vms.iter().find(|v| v.id == id))
        {
            Some(vm) => format!("VMDesk — {}", vm.name),
            None => "VMDesk".to_owned(),
        };
        let body = div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(color::BG))
            .text_color(rgb(color::TEXT))
            .children(crate::chrome::title_bar(window, title))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_list(cx))
                    .child(self.render_detail(cx)),
            );
        crate::chrome::frame(window, body.into_any_element())
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
