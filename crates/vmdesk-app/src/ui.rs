//! The main window: VM list on the left, the selected VM on the right.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::{Local, Utc};
use gpui::{
    AnyElement, AppContext, Context, Entity, FocusHandle, FontWeight, KeyDownEvent, SharedString,
    Window, div, prelude::*, px, rgb, transparent_black,
};
use vmdesk_core::{AgentState, Headline, Hop, SshState, Vm, VmId, VmStatus};
use vmdesk_db::{NoteId, SessionId};

use crate::agent_view::{AgentAction, AgentView};
use crate::backend::{Backend, Detail, Inventory, KeyProblem};
use crate::field::TextField;
use crate::form::AddForm;
use crate::human::ago;
use crate::icon::{Icon, icon};
use crate::kit::{
    Button, backdrop, button, callout, dot, focus_ring, icon_button, label, menu, menu_action,
    menu_disabled, menu_separator, pill, popover,
};
use crate::search_view::{DEBOUNCE, Search, SearchState};
use crate::theme::{CONTROL_HEIGHT, MONO, SANS, Tone, color, radius, space, text, tint};
use crate::work_view::{card_view, present_sessions, present_work, sessions_view, sidebar_line};

/// How often the list re-reads the database (the monitor writes status there).
const RELOAD: Duration = Duration::from_secs(5);
/// The selected VM is re-collected this often while it is open (spec section 9).
const OPEN_VM_INTERVAL: Duration = Duration::from_mins(2);
const SIDEBAR_WIDTH: f32 = 280.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Sessions,
    Timeline,
}

/// The one inline text field open in the detail pane. Enter saves it, Esc closes it.
enum Inline {
    /// A note for the latest work session, from the current-work card.
    Note(Entity<TextField>),
    /// A new title for a session, in the Sessions tab.
    Rename {
        session: SessionId,
        field: Entity<TextField>,
    },
}

impl Inline {
    fn field(&self) -> &Entity<TextField> {
        let (Self::Note(field) | Self::Rename { field, .. }) = self;
        field
    }
}

/// A change to a VM's notes or sessions, with the message to show once it is done.
type WorkChange = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>>>>;

/// A removal waiting for the user to confirm it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    RemoveVm(VmId),
    RemoveAgent(VmId),
}

pub struct AppView {
    backend: Backend,
    focus: FocusHandle,
    vms: Vec<Vm>,
    status: HashMap<VmId, VmStatus>,
    /// Each VM's current-work line, for the list.
    work: HashMap<VmId, String>,
    filter: String,
    selected: Option<VmId>,
    detail: Option<Detail>,
    tab: Tab,
    inline: Option<Inline>,
    /// The event search, while it is open in place of the selected VM.
    search: Option<Search>,
    /// When the selected VM was last collected by this window.
    collected_at: Option<Instant>,
    busy: Option<&'static str>,
    message: Option<(String, bool)>,
    /// VM to select once the inventory has loaded (from the command line).
    pending_select: Option<String>,
    /// The "Add VM" form, while it is open.
    pub(crate) add: Option<AddForm>,
    confirm: Option<Confirm>,
    /// The header's "More" menu is open.
    menu_open: bool,
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
            work: HashMap::new(),
            filter: String::new(),
            selected: None,
            detail: None,
            tab: Tab::Overview,
            inline: None,
            search: None,
            collected_at: None,
            busy: None,
            message: None,
            pending_select: open_vm,
            add: None,
            confirm: None,
            menu_open: false,
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
                    && inv.work == self.work
                    && self.pending_select.is_none() =>
            {
                return;
            }
            Ok(inv) => {
                self.vms = inv.vms;
                self.status = inv.status;
                self.work = inv.work;
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
                    self.inline = None;
                    self.confirm = None;
                    self.menu_open = false;
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
        self.inline = None;
        self.key_prompt = None;
        self.confirm = None;
        self.menu_open = false;
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
                        if let Some(Inline::Rename { session, .. }) = &v.inline
                            && !d.sessions.iter().any(|s| s.id == *session)
                        {
                            v.inline = None;
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
        if ks.modifiers.control && ks.key == "k" {
            self.open_search(window, cx);
            return;
        }
        // A field that lost focus stays open with its text; the keys go back to the list.
        let in_inline = self
            .inline
            .as_ref()
            .is_some_and(|i| i.field().read(cx).focus_handle().is_focused(window));
        let in_search = self
            .search
            .as_ref()
            .is_some_and(|s| s.field.read(cx).focus_handle().is_focused(window));
        match ks.key.as_str() {
            "escape" if self.menu_open => self.menu_open = false,
            "escape" if self.inline.is_some() => self.cancel_inline(window, cx),
            "escape" if self.search.is_some() => self.close_search(window, cx),
            "enter" if in_inline => self.save_inline(window, cx),
            _ if in_inline || in_search => return,
            _ if ks.modifiers.control || ks.modifiers.alt || ks.modifiers.platform => return,
            "up" | "down" => {
                if self.search.is_some() {
                    self.close_search(window, cx);
                }
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
        self.search = None;
        self.inline = None;
        cx.notify();
    }

    fn open_inline(&mut self, inline: Inline, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&inline.field().read(cx).focus_handle().clone(), cx);
        self.inline = Some(inline);
        cx.notify();
    }

    /// Opens the note field on the current-work card.
    pub(crate) fn start_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = cx.new(|cx| TextField::new("What are you in the middle of?", false, cx));
        self.open_inline(Inline::Note(field), window, cx);
    }

    /// Turns a session's title into a field holding it.
    pub(crate) fn start_rename(
        &mut self,
        session: SessionId,
        title: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = cx.new(|cx| {
            TextField::new("Name this session", false, cx).with_text(title.unwrap_or_default())
        });
        self.open_inline(Inline::Rename { session, field }, window, cx);
    }

    fn cancel_inline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.inline = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Saves the inline field: the note goes to the latest session, as `vmdesk note`
    /// does; an empty title clears the session's title.
    fn save_inline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vm) = self.selected else { return };
        let job: WorkChange = match &self.inline {
            None => return,
            Some(Inline::Note(field)) => {
                let body = field.read(cx).text().trim().to_owned();
                if body.is_empty() {
                    return;
                }
                let name = self
                    .vms
                    .iter()
                    .find(|v| v.id == vm)
                    .map(|v| v.name.clone())
                    .unwrap_or_default();
                let add = self.backend.add_note(vm, body);
                Box::pin(async move {
                    let note = add.await.context("adding the note")?;
                    Ok(note.session.is_none().then(|| {
                        format!(
                            "Noted. {name} has no work session yet; the note is under Sessions."
                        )
                    }))
                })
            }
            Some(Inline::Rename { session, field }) => {
                let rename = self
                    .backend
                    .rename_session(*session, field.read(cx).text().to_owned());
                Box::pin(async move {
                    rename.await.context("renaming the session")?;
                    Ok(None)
                })
            }
        };
        let field = self.inline.take();
        window.focus(&self.focus, cx);
        Self::after(vm, job, field, window, cx);
    }

    pub(crate) fn delete_note(&mut self, id: NoteId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vm) = self.selected else { return };
        let delete = self.backend.delete_note(id);
        Self::after(
            vm,
            Box::pin(async move {
                delete.await.context("deleting the note")?;
                Ok(None)
            }),
            None,
            window,
            cx,
        );
    }

    /// Runs a change to `vm`'s work record, shows its message or error, and reloads the
    /// VM's detail. If it fails, the field it came from opens again with its text.
    fn after(
        vm: VmId,
        job: WorkChange,
        field: Option<Inline>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let res = job.await;
            this.update_in(cx, |v, window, cx| {
                match res {
                    Ok(Some(m)) => v.message = Some((m, false)),
                    Ok(None) => {}
                    Err(e) => {
                        v.message = Some((format!("{e:#}"), true));
                        if let Some(field) = field.filter(|_| v.selected == Some(vm)) {
                            v.open_inline(field, window, cx);
                        }
                    }
                }
                if v.selected == Some(vm) {
                    v.load_detail(vm, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Opens the event search, or moves the focus back into it.
    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.add.is_some() {
            return;
        }
        let search = self.search.get_or_insert_with(|| Search::new(cx));
        let focus = search.field.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        self.inline = None;
        self.menu_open = false;
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The query changed: search for it once typing pauses for [`DEBOUNCE`].
    pub(crate) fn search_edited(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let query = search.field.read(cx).text().trim().to_owned();
        if !query.chars().any(char::is_alphanumeric) {
            search.pending = None;
            search.state = SearchState::Idle;
            cx.notify();
            return;
        }
        search.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let Ok(found) = this.update(cx, |v, cx| {
                if let Some(s) = v.search.as_mut() {
                    s.state = SearchState::Searching {
                        query: query.clone(),
                    };
                }
                cx.notify();
                v.backend.search(query.clone())
            }) else {
                return;
            };
            let found = found.await;
            this.update(cx, |v, cx| {
                if let Some(s) = v.search.as_mut() {
                    s.state = SearchState::answered(query, found, Local::now());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows a search hit: its VM, on the Timeline tab.
    pub(crate) fn open_hit(&mut self, vm: VmId, window: &mut Window, cx: &mut Context<Self>) {
        self.close_search(window, cx);
        self.select(vm, cx);
        self.tab = Tab::Timeline;
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
        self.confirm = None;
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

    /// Installs, upgrades or removes `id`'s agent, then reloads its status and detail.
    fn run_agent_action(&mut self, id: VmId, action: AgentAction, cx: &mut Context<Self>) {
        self.confirm = None;
        self.busy = Some(action.busy());
        let job: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>>>> =
            match action {
                AgentAction::Install | AgentAction::Upgrade => {
                    Box::pin(self.backend.install_agent(id))
                }
                AgentAction::Remove => Box::pin(self.backend.uninstall_agent(id)),
            };
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let inv = backend.inventory().await;
            this.update(cx, |v, cx| {
                v.busy = None;
                v.message = Some(match res {
                    Ok(m) => (m, false),
                    Err(e) => (format!("{} failed: {e:#}", action.label()), true),
                });
                v.apply_inventory(inv, cx);
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

    fn agent_view(&self, id: VmId) -> Option<AgentView> {
        let state = self
            .status
            .get(&id)
            .map_or(AgentState::Unknown, |s| s.agent);
        self.detail
            .as_ref()
            .map(|d| AgentView::new(d.agent.as_ref(), state))
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

fn headline_tone(h: Option<Headline>) -> Tone {
    match h {
        Some(Headline::Online) => Tone::Good,
        Some(Headline::SshFailing) => Tone::Warn,
        Some(Headline::NoRoute) => Tone::Bad,
        Some(Headline::Stopped | Headline::Unknown) | None => Tone::Neutral,
    }
}

/// `online + agent` as a pill says it: `Online + agent`, `SSH failing`.
fn sentence_case(label: &str) -> String {
    if let Some(rest) = label.strip_prefix("ssh") {
        return format!("SSH{rest}");
    }
    let mut chars = label.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().chain(chars).collect::<String>()
    })
}

fn ssh_fact(ssh: SshState) -> &'static str {
    match ssh {
        SshState::Ok => "SSH ok",
        SshState::AuthFailed => "SSH login refused",
        SshState::HostKey => "Host key needs review",
        SshState::Refused => "SSH connection refused",
        SshState::Timeout => "SSH timed out",
        SshState::NoRoute => "No route",
        SshState::Error => "SSH error",
        SshState::NotChecked => "SSH not checked",
    }
}

/// Confirmation before removing a VM.
fn remove_panel(vm: &Vm, cx: &mut Context<AppView>) -> gpui::Div {
    let id = vm.id;
    callout(Tone::Bad)
        .child(div().flex_1().min_w_0().child(format!(
            "Remove {} from VMDesk? Its history, host keys and stored password are deleted; the VM itself is not touched.",
            vm.name
        )))
        .child(
            button(Button::Secondary, "remove-no")
                .child("Cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.confirm = None;
                    cx.notify();
                })),
        )
        .child(
            button(Button::Danger, "remove-yes")
                .child("Remove")
                .on_click(cx.listener(move |this, _, _, cx| this.remove(id, cx))),
        )
}

/// Confirmation before removing a VM's agent.
fn agent_remove_panel(vm: &Vm, cx: &mut Context<AppView>) -> gpui::Div {
    let id = vm.id;
    callout(Tone::Warn)
        .debug_selector(|| "agent-remove-panel".to_owned())
        .child(div().flex_1().min_w_0().child(format!(
            "Remove the agent from {}? It is stopped and deleted on the VM, and the VM is collected over SSH again. Its history on this laptop is kept.",
            vm.name
        )))
        .child(
            button(Button::Secondary, "agent-remove-no")
                .child("Cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.confirm = None;
                    cx.notify();
                })),
        )
        .child(
            button(Button::Danger, "agent-remove-yes")
                .child("Remove agent")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_agent_action(id, AgentAction::Remove, cx);
                })),
        )
}

/// The host-key decision: trust a new key (with how to check it), or forget a changed one.
fn key_panel(p: &KeyProblem, cx: &mut Context<AppView>) -> gpui::Div {
    let (tone, lines, action, kind): (Tone, Vec<String>, &'static str, Button) = match p {
        KeyProblem::Unknown { name, key, known_hosts_match, .. } => (
            Tone::Warn,
            vec![
                format!("First connection to {name}. It presents this host key:"),
                format!("{} {}", key.key_type, key.fingerprint),
                match known_hosts_match {
                    Some(true) => "Matches the key in your ~/.ssh/known_hosts.".to_owned(),
                    Some(false) => "Differs from the key in your ~/.ssh/known_hosts. Do not trust it unless you know why.".to_owned(),
                    None => "Check it on the VM: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub".to_owned(),
                },
            ],
            "Trust this key",
            Button::Primary,
        ),
        KeyProblem::Changed { name, expected, offered, .. } => (
            Tone::Bad,
            vec![
                format!("The host key of {name} has CHANGED."),
                format!("trusted: {expected}"),
                format!("offered: {offered}"),
                "Expected only if the VM was reinstalled; otherwise someone may be intercepting the connection.".to_owned(),
            ],
            "Forget old key",
            Button::Danger,
        ),
    };
    callout(tone)
        .flex_col()
        .items_start()
        .gap(space::XS)
        .children(lines.into_iter().enumerate().map(|(i, l)| {
            let line = div().child(l);
            if i == 1 {
                line.font_family(MONO).text_size(text::META)
            } else {
                line
            }
        }))
        .child(
            div()
                .flex()
                .gap(space::S)
                .mt(space::S)
                .child(
                    button(kind, "key-yes")
                        .child(action)
                        .on_click(cx.listener(|this, _, _, cx| this.resolve_host_key(true, cx))),
                )
                .child(
                    button(Button::Secondary, "key-no")
                        .child("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.resolve_host_key(false, cx))),
                ),
        )
}

impl AppView {
    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let vms = self.visible();
        let online = self
            .vms
            .iter()
            .filter(|v| self.headline(v) == Some(Headline::Online))
            .count();
        let none_match = vms.is_empty() && !self.vms.is_empty();
        div()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(color::SURFACE))
            .border_r_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .px(space::L)
                    .pt(space::L)
                    .pb(space::M)
                    .flex()
                    .flex_col()
                    .gap(space::M)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .text_size(text::SECTION)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Machines"),
                                    )
                                    .child(label(format!(
                                        "{online} online · {} VMs",
                                        self.vms.len()
                                    ))),
                            )
                            .child(
                                button(Button::Secondary, "add-vm")
                                    .px(space::S)
                                    .child(icon(Icon::Plus, px(14.), color::TEXT))
                                    .child("Add VM")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_add_form(window, cx);
                                    })),
                            ),
                    )
                    .child(self.filter_input(cx)),
            )
            .child(
                div()
                    .id("vm-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .px(space::S)
                    .pb(space::S)
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .when(none_match, |d| {
                        d.child(
                            div()
                                .pt(space::L)
                                .flex()
                                .justify_center()
                                .child(label(format!("No machines match “{}”", self.filter))),
                        )
                    })
                    .children(vms.into_iter().map(|vm| self.list_row(vm, cx))),
            )
            .child(self.search_entry(cx))
    }

    /// The way into the event search, at the foot of the list.
    fn search_entry(&self, cx: &mut Context<Self>) -> gpui::Div {
        let open = self.search.is_some();
        div()
            .p(space::S)
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .id("open-search")
                    .debug_selector(|| "open-search".to_owned())
                    .h(CONTROL_HEIGHT)
                    .px(space::S)
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .rounded(radius::M)
                    .cursor_pointer()
                    .text_size(text::BODY)
                    .text_color(rgb(if open {
                        color::TEXT
                    } else {
                        color::TEXT_SECONDARY
                    }))
                    .when(open, |d| d.bg(tint(color::ACCENT)))
                    .when(!open, |d| d.hover(|s| s.bg(rgb(color::HOVER))))
                    .on_click(cx.listener(|this, _, window, cx| this.open_search(window, cx)))
                    .child(icon(Icon::Search, px(14.), color::TEXT_MUTED))
                    .child(div().flex_1().child("Search events"))
                    .child(
                        div()
                            .px(px(6.))
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .rounded(radius::S)
                            .border_1()
                            .border_color(rgb(color::BORDER_STRONG))
                            .text_size(text::SMALL)
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Ctrl K"),
                    ),
            )
    }

    /// The filter as an input. Typing anywhere in the window edits it.
    fn filter_input(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let active = !self.filter.is_empty();
        div()
            .id("filter")
            .debug_selector(|| "filter".to_owned())
            .h(CONTROL_HEIGHT)
            .px(space::S)
            .flex()
            .items_center()
            .gap(space::S)
            .rounded(radius::M)
            .border_1()
            .border_color(rgb(if active {
                color::ACCENT
            } else {
                color::BORDER_STRONG
            }))
            .when(active, |d| d.shadow(focus_ring()))
            .bg(rgb(color::BG))
            .cursor_text()
            .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx)))
            .child(icon(Icon::Search, px(14.), color::TEXT_MUTED))
            .child(if active {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .text_size(text::BODY)
                    .child(self.filter.clone())
                    .child(
                        div()
                            .ml(px(1.))
                            .w(px(1.5))
                            .h(px(15.))
                            .bg(rgb(color::ACCENT)),
                    )
            } else {
                div()
                    .flex_1()
                    .text_size(text::BODY)
                    .text_color(rgb(color::TEXT_MUTED))
                    .child("Filter machines")
            })
            .when(active, |d| {
                d.child(
                    div()
                        .id("filter-clear")
                        .debug_selector(|| "filter-clear".to_owned())
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(color::HOVER)))
                        .child(icon(Icon::Close, px(12.), color::TEXT_SECONDARY))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.filter.clear();
                            cx.notify();
                        })),
                )
            })
    }

    fn list_row(&self, vm: &Vm, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let id = vm.id;
        let selected = self.selected == Some(id);
        let sub = match self.status.get(&vm.id) {
            Some(s) => match &s.route {
                Some(r) => format!("{} · {r}", s.label(vm.lifecycle)),
                None => s.label(vm.lifecycle),
            },
            None => "never checked".to_owned(),
        };
        div()
            .id(SharedString::from(vm.id.to_string()))
            .px(space::S)
            .py(px(7.))
            .flex()
            .gap(px(10.))
            .items_start()
            .rounded(radius::M)
            .cursor_pointer()
            .when(selected, |d| d.bg(tint(color::ACCENT)))
            .when(!selected, |d| d.hover(|s| s.bg(rgb(color::HOVER))))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.close_search(window, cx);
                this.select(id, cx);
            }))
            .child(dot(headline_tone(self.headline(vm)).color(), 8.).mt(px(6.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .text_size(text::BODY)
                            .font_weight(if selected {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(vm.name.clone()),
                    )
                    .child(
                        label(sub.clone())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .when(selected, |d| d.text_color(rgb(color::TEXT_SECONDARY)))
                            .debug_selector(|| format!("{}: {sub}", vm.name)),
                    )
                    .when_some(self.work.get(&id), |d, line| {
                        d.child(sidebar_line(&vm.name, line))
                    }),
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
        if let Some(search) = &self.search {
            return div()
                .id("search-pane")
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if let Some(s) = &this.search {
                            window.focus(&s.field.read(cx).focus_handle().clone(), cx);
                            window.prevent_default();
                        }
                    }),
                )
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_y_scroll()
                .child(crate::search_view::render(search, cx))
                .into_any_element();
        }
        let Some(vm) = self
            .selected
            .and_then(|id| self.vms.iter().find(|v| v.id == id))
        else {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(space::XS)
                .child(
                    div()
                        .text_size(text::SECTION)
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child("Select a machine"),
                )
                .child(label("or add one with Add VM"))
                .into_any_element();
        };
        let header = self.render_header(vm, cx);

        let body = self.tab_body(cx);

        div()
            .flex_1()
            .min_w_0()
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
                    .px(space::XL)
                    .pt(space::L)
                    .pb(space::XL)
                    .child(body),
            )
            .into_any_element()
    }

    /// The selected VM's open tab.
    fn tab_body(&self, cx: &mut Context<Self>) -> AnyElement {
        match (&self.detail, self.tab) {
            (None, _) => div().child(label("Loading…")).into_any_element(),
            (Some(d), Tab::Overview) => {
                let work = present_work(d.work.as_ref(), &d.sessions, &d.notes, Local::now());
                let adding = match &self.inline {
                    Some(Inline::Note(field)) => Some(field),
                    _ => None,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(space::L)
                    .child(card_view(&work, adding, cx))
                    .when_some(d.snapshot.as_ref(), |el, s| {
                        el.child(crate::overview::render(s))
                    })
                    .into_any_element()
            }
            (Some(d), Tab::Sessions) => {
                let renaming = match &self.inline {
                    Some(Inline::Rename { session, field }) => Some((*session, field)),
                    _ => None,
                };
                sessions_view(
                    &present_sessions(&d.sessions, &d.notes, Local::now()),
                    renaming,
                    cx,
                )
                .into_any_element()
            }
            (Some(d), Tab::Timeline) => crate::timeline_view::render(
                &crate::timeline_view::present(&d.timeline, chrono::Local::now()),
            )
            .into_any_element(),
        }
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

    fn status_strip(&self, vm: &Vm) -> gpui::Div {
        let status = self.status.get(&vm.id);
        let headline = status.map(|s| s.label(vm.lifecycle));
        let facts = status.map(|s| {
            let mut facts = vec![ssh_fact(s.ssh).to_owned()];
            facts.extend(s.route.as_ref().map(|r| format!("via {r}")));
            facts.push(format!(
                "checked {} ago",
                ago((Utc::now() - s.checked_at).num_seconds())
            ));
            facts.join(" · ")
        });
        let agent = self.agent_view(vm.id);
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(space::M)
            .child(
                pill(
                    headline_tone(status.map(|s| s.headline(vm.lifecycle))),
                    headline
                        .as_deref()
                        .map_or_else(|| "Never checked".to_owned(), sentence_case),
                )
                .debug_selector(|| format!("headline: {}", headline.clone().unwrap_or_default())),
            )
            .when_some(facts, |d, f| {
                d.child(
                    div()
                        .text_size(text::META)
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child(f),
                )
            })
            .when_some(agent, |d, agent| {
                let text = agent.text(Utc::now());
                d.child(div().w(px(1.)).h(px(14.)).bg(rgb(color::BORDER_STRONG)))
                    .child(
                        div()
                            .debug_selector(|| format!("agent: {text}"))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(text::META)
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(dot(agent.tone().color(), 6.))
                            .child(text.clone()),
                    )
            })
    }

    fn more_menu(&self, vm: &Vm, cx: &mut Context<Self>) -> gpui::Div {
        let id = vm.id;
        let agent_action = self.agent_view(id).map(|a| a.action());
        let idle = self.busy.is_none() && self.confirm != Some(Confirm::RemoveAgent(id));
        div()
            .relative()
            .child(
                icon_button("more", Icon::More, self.menu_open).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.menu_open = !this.menu_open;
                        cx.notify();
                    },
                )),
            )
            .when(self.menu_open, |d| {
                let agent_item = agent_action.map(|action| {
                    if !idle {
                        return menu_disabled(action.menu_label()).into_any_element();
                    }
                    menu_action(action.id(), action.menu_label(), action.asks_first())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.menu_open = false;
                            if action.asks_first() {
                                this.confirm = Some(Confirm::RemoveAgent(id));
                                cx.notify();
                            } else {
                                this.run_agent_action(id, action, cx);
                            }
                        }))
                        .into_any_element()
                });
                d.child(popover(
                    div()
                        .absolute()
                        .top(CONTROL_HEIGHT + px(6.))
                        .right_0()
                        .child(
                            menu()
                                .debug_selector(|| "menu".to_owned())
                                .occlude()
                                .child(menu_action("edit", "Edit VM…", false).on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.menu_open = false;
                                        this.open_edit_form(id, window, cx);
                                    }),
                                ))
                                .child(menu_action("remove", "Remove VM…", true).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.menu_open = false;
                                        this.confirm = Some(Confirm::RemoveVm(id));
                                        cx.notify();
                                    }),
                                ))
                                .when_some(agent_item, |m, item| {
                                    m.child(menu_separator()).child(item)
                                }),
                        ),
                ))
            })
    }

    fn title_row(&self, vm: &Vm, cx: &mut Context<Self>) -> gpui::Div {
        let id = vm.id;
        let ep = vm
            .endpoints
            .first()
            .map(|e| format!("{}@{}:{}", vm.username, e.address, e.port))
            .unwrap_or_default();
        let busy = self.busy.map(|b| format!("{}…", sentence_case(b)));
        div()
            .flex()
            .items_center()
            .gap(space::M)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(text::TITLE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(vm.name.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(text::META)
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(div().font_family(MONO).child(ep))
                            .child("·")
                            .child(self.path_text(vm)),
                    ),
            )
            .when_some(busy, |d, b| d.child(label(b)))
            .child(
                button(Button::Secondary, "check")
                    .child("Check")
                    .on_click(cx.listener(move |this, _, _, cx| this.check(id, cx))),
            )
            .child(
                button(Button::Secondary, "collect")
                    .child("Refresh")
                    .on_click(cx.listener(move |this, _, _, cx| this.collect(id, cx))),
            )
            .child(
                button(Button::Primary, "terminal")
                    .child(icon(Icon::Terminal, px(14.), color::ON_ACCENT))
                    .child("Open terminal")
                    .on_click(cx.listener(move |this, _, _, cx| this.open_terminal(id, cx))),
            )
            .child(self.more_menu(vm, cx))
    }

    fn render_header(&self, vm: &Vm, cx: &mut Context<Self>) -> gpui::Div {
        let id = vm.id;
        let status = self.status.get(&id);
        div()
            .px(space::XL)
            .pt(space::L)
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .flex()
            .flex_col()
            .gap(space::M)
            .child(self.title_row(vm, cx))
            .child(self.status_strip(vm))
            .when_some(status.and_then(|s| s.detail.clone()), |d, detail| {
                d.child(
                    div()
                        .text_size(text::META)
                        .text_color(rgb(color::AMBER))
                        .child(detail),
                )
            })
            .when_some(self.message.clone(), |d, (m, err)| {
                d.child(
                    div()
                        .text_size(text::META)
                        .text_color(rgb(if err {
                            color::RED
                        } else {
                            color::TEXT_SECONDARY
                        }))
                        .child(m),
                )
            })
            .when(self.confirm == Some(Confirm::RemoveVm(id)), |d| {
                d.child(remove_panel(vm, cx))
            })
            .when(self.confirm == Some(Confirm::RemoveAgent(id)), |d| {
                d.child(agent_remove_panel(vm, cx))
            })
            .when(
                status.is_some_and(|s| s.ssh == SshState::HostKey) && self.key_prompt.is_none(),
                |d| {
                    d.child(
                        callout(Tone::Warn)
                            .child(div().flex_1().min_w_0().child(
                                "The VM's host key needs your decision before VMDesk connects.",
                            ))
                            .child(
                                button(Button::Secondary, "review-key")
                                    .child("Review host key…")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.review_host_key(id, cx);
                                    })),
                            ),
                    )
                },
            )
            .when_some(self.key_prompt.clone(), |d, p| d.child(key_panel(&p, cx)))
            .child(
                div()
                    .flex()
                    .gap(space::XL)
                    .child(self.tab_button("tab-overview", "Overview", Tab::Overview, cx))
                    .child(self.tab_button("tab-sessions", "Sessions", Tab::Sessions, cx))
                    .child(self.tab_button("tab-timeline", "Timeline", Tab::Timeline, cx)),
            )
    }

    fn tab_button(
        &self,
        id: &'static str,
        content: &'static str,
        tab: Tab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.tab == tab;
        div()
            .id(id)
            .debug_selector(|| id.to_owned())
            .h(px(36.))
            .flex()
            .items_center()
            .border_b_2()
            .border_color(if active {
                rgb(color::ACCENT).into()
            } else {
                transparent_black()
            })
            .cursor_pointer()
            .text_size(text::BODY)
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(if active {
                color::TEXT
            } else {
                color::TEXT_SECONDARY
            }))
            .hover(|s| s.text_color(rgb(color::TEXT)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tab = tab;
                this.inline = None;
                cx.notify();
            }))
            .child(content)
    }
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
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(color::BG))
            .text_color(rgb(color::TEXT))
            .font_family(SANS)
            .text_size(text::BODY)
            .children(crate::chrome::title_bar(window, title))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_list(cx))
                    .child(self.render_detail(cx)),
            )
            .when(self.menu_open, |d| {
                d.child(backdrop(
                    "menu-backdrop",
                    cx.listener(|this, _, _, cx| {
                        this.menu_open = false;
                        cx.notify();
                    }),
                ))
            });
        crate::chrome::frame(window, body.into_any_element())
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
