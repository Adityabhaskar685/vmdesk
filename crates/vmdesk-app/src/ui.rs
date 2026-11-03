//! The main window: VM list on the left, the selected VM on the right.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::{Local, Utc};
use gpui::{
    AnyElement, AppContext, Context, Entity, FocusHandle, FontWeight, KeyDownEvent, MouseButton,
    ScrollHandle, SharedString, Window, div, point, prelude::*, px, rgb,
};
use vmdesk_core::human::ago;
use vmdesk_core::{AgentState, Headline, Hop, SshState, Vm, VmId, VmStatus};
use vmdesk_db::{EventKey, NoteId, SessionId};

use crate::agent_view::{AgentAction, AgentView};
use crate::backend::{Backend, Detail, Inventory, KeyProblem};
use crate::field::TextField;
use crate::form::AddForm;
use crate::icon::{Icon, icon};
use crate::kit::{
    Button, backdrop, button, callout, dot, focus_ring, icon_button, label, menu, menu_action,
    menu_disabled, menu_separator, pill, popover,
};
use crate::search_view::{DEBOUNCE, Search, SearchState};
use crate::term_view::{ShellExited, TerminalView};
use crate::theme::{
    CONTROL_HEIGHT, MONO, SANS, Tone, palette, radius, space, text, tint, tone_color,
};
use crate::theme_view::ThemePicker;
use crate::timeline_view::{HIGHLIGHT, TimelineWindow};
use crate::work_view::{card_view, present_sessions, present_work, sessions_view};

/// How often the list re-reads the database (the monitor writes status there).
const RELOAD: Duration = Duration::from_secs(5);
/// The selected VM is re-collected this often while it is open (spec section 9).
const OPEN_VM_INTERVAL: Duration = Duration::from_mins(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Overview,
    Sessions,
    Timeline,
    /// A live shell on the selected VM, embedded in the pane.
    Terminal,
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

/// Which way a terminal split lays its two halves out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitDir {
    /// Side by side.
    Row,
    /// One above the other.
    Column,
}

/// One shell in a terminal tab.
#[derive(Clone)]
pub(crate) struct TerminalLeaf {
    pub(crate) id: u64,
    pub(crate) title: SharedString,
    pub(crate) view: Entity<TerminalView>,
}

/// A tab's shells: one leaf, or a binary tree of splits.
#[derive(Clone)]
pub(crate) enum TerminalNode {
    Leaf(TerminalLeaf),
    Split {
        dir: SplitDir,
        first: Box<TerminalNode>,
        second: Box<TerminalNode>,
    },
}

impl TerminalNode {
    fn leaf(&self, id: u64) -> Option<&TerminalLeaf> {
        match self {
            Self::Leaf(leaf) if leaf.id == id => Some(leaf),
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => first.leaf(id).or_else(|| second.leaf(id)),
        }
    }

    fn first_leaf(&self) -> &TerminalLeaf {
        match self {
            Self::Leaf(leaf) => leaf,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    /// Replaces the leaf `target` with a split of itself and `new`.
    fn split_leaf(&mut self, target: u64, dir: SplitDir, new: TerminalLeaf) -> bool {
        match self {
            Self::Leaf(leaf) if leaf.id == target => {
                let old = Self::Leaf(leaf.clone());
                *self = Self::Split {
                    dir,
                    first: Box::new(old),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split_leaf(target, dir, new.clone()) || second.split_leaf(target, dir, new)
            }
        }
    }

    /// Takes this tree apart without `target`. Returns the surviving subtree (`None`
    /// when this leaf was the target) and whether it was found.
    fn into_without(self, target: u64) -> (Option<Self>, bool) {
        match self {
            Self::Leaf(leaf) => {
                if leaf.id == target {
                    (None, true)
                } else {
                    (Some(Self::Leaf(leaf)), false)
                }
            }
            Self::Split { dir, first, second } => {
                let (left, removed) = (*first).into_without(target);
                if removed {
                    return match left {
                        Some(left) => (
                            Some(Self::Split {
                                dir,
                                first: Box::new(left),
                                second,
                            }),
                            true,
                        ),
                        None => (Some(*second), true),
                    };
                }
                let (right, removed) = (*second).into_without(target);
                if removed {
                    return match right {
                        Some(right) => (
                            Some(Self::Split {
                                dir,
                                first: Box::new(left.expect("the side without the target")),
                                second: Box::new(right),
                            }),
                            true,
                        ),
                        None => (Some(left.expect("the side without the target")), true),
                    };
                }
                (
                    Some(Self::Split {
                        dir,
                        first: Box::new(left.expect("a split has two sides")),
                        second: Box::new(right.expect("a split has two sides")),
                    }),
                    false,
                )
            }
        }
    }
}

/// One terminal tab: a split tree of shells.
pub(crate) struct TerminalTab {
    pub(crate) title: SharedString,
    pub(crate) root: Option<TerminalNode>,
    /// The shell that has, or last had, the keyboard in this tab.
    pub(crate) focused: u64,
    /// A shell shown alone over the whole tab; its splits come back when it is unzoomed.
    pub(crate) zoomed: Option<u64>,
}

impl TerminalTab {
    fn focused_leaf(&self) -> Option<&TerminalLeaf> {
        let root = self.root.as_ref()?;
        Some(root.leaf(self.focused).unwrap_or_else(|| root.first_leaf()))
    }
}

/// The shells open on one VM. Dropping the pane drops the views, which closes their ptys.
pub(crate) struct TerminalPane {
    pub(crate) vm: VmId,
    pub(crate) tabs: Vec<TerminalTab>,
    pub(crate) active: usize,
    /// The pane fills the window, hiding the machine list and detail header.
    pub(crate) maximized: bool,
    /// The next tab's number and shell number, so they stay unique after closing one.
    next_tab: usize,
    next_shell: usize,
    next_leaf: u64,
    /// When the pane was opened, which orders the machine tabs.
    opened: Instant,
    /// Whether the Terminal view was showing when another machine was selected, so
    /// coming back shows the shells again.
    left_on_terminal: bool,
}

impl TerminalPane {
    fn new(vm: VmId) -> Self {
        Self {
            vm,
            tabs: Vec::new(),
            active: 0,
            maximized: false,
            next_tab: 1,
            next_shell: 1,
            next_leaf: 1,
            opened: Instant::now(),
            left_on_terminal: false,
        }
    }

    pub(crate) fn active_view(&self) -> Option<&Entity<TerminalView>> {
        self.tabs
            .get(self.active)?
            .focused_leaf()
            .map(|leaf| &leaf.view)
    }
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
    /// The stretch of events the Timeline tab asks for. A load for another is dropped.
    pub(crate) timeline: TimelineWindow,
    /// The detail body's scroll, which the Timeline moves to a search hit.
    detail_scroll: ScrollHandle,
    /// The window the detail body was last scrolled for; `None` after switching VM.
    scrolled_for: Option<TimelineWindow>,
    inline: Option<Inline>,
    /// The event search, while it is open in place of the selected VM.
    search: Option<Search>,
    /// The theme picker, while it is open in place of the selected VM.
    pub(crate) themes: Option<ThemePicker>,
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
    /// Whether the machine list is shown beside the selected VM.
    show_machines: bool,
    /// The selected VM's shells while the Terminal tab is open. Dropping a view closes
    /// its pty.
    terminal: Option<TerminalPane>,
    /// The other machines' shells, kept running while another machine is selected.
    parked: HashMap<VmId, TerminalPane>,
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
            timeline: TimelineWindow::LATEST,
            detail_scroll: ScrollHandle::new(),
            scrolled_for: None,
            inline: None,
            search: None,
            themes: None,
            collected_at: None,
            busy: None,
            message: None,
            pending_select: open_vm,
            add: None,
            confirm: None,
            menu_open: false,
            show_machines: true,
            terminal: None,
            parked: HashMap::new(),
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
                    self.terminal = None;
                    if self.tab == Tab::Terminal {
                        self.tab = Tab::Overview;
                    }
                }
                let vms = &self.vms;
                self.parked.retain(|id, _| vms.iter().any(|v| v.id == *id));
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
        if online && due && self.busy.is_none() && self.backend.background {
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
        self.timeline = TimelineWindow::LATEST;
        self.scrolled_for = None;
        // Shells keep running on the machine they belong to: park them, and bring back
        // the new machine's.
        if let Some(mut pane) = self.terminal.take() {
            pane.left_on_terminal = self.tab == Tab::Terminal;
            self.parked.insert(pane.vm, pane);
        }
        self.terminal = self.parked.remove(&id);
        self.tab = match &self.terminal {
            Some(pane) if pane.left_on_terminal => Tab::Terminal,
            _ if self.tab == Tab::Terminal => Tab::Overview,
            _ => self.tab,
        };
        self.load_detail(id, cx);
        cx.notify();
    }

    /// Selects a tab, opening a shell the first time the Terminal tab is shown.
    pub(crate) fn select_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        self.inline = None;
        if tab == Tab::Terminal {
            let Some(id) = self
                .selected
                .and_then(|id| self.vms.iter().find(|v| v.id == id))
                .map(|vm| vm.id)
            else {
                return;
            };
            if self.terminal.as_ref().is_some_and(|pane| pane.vm != id) {
                self.terminal = None;
            }
            if self.terminal.is_none() {
                self.add_terminal_tab(window, cx);
                return;
            }
            if let Some(view) = self.terminal.as_ref().and_then(TerminalPane::active_view) {
                let focus = view.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
            }
        } else {
            // Typing goes back to filtering the list.
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    /// Adds a shell to the Terminal tab and focuses it, opening the pane if needed.
    fn add_terminal_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .selected
            .and_then(|id| self.vms.iter().find(|v| v.id == id))
            .map(|vm| vm.id)
        else {
            return;
        };
        if self.terminal.as_ref().is_some_and(|pane| pane.vm != id) {
            self.terminal = None;
        }
        let backend = self.backend.clone();
        let view = cx.new(|cx| TerminalView::new(backend, id, cx));
        let vm_for_exit = id;
        let focus = view.read(cx).focus_handle().clone();
        let pane = self.terminal.get_or_insert_with(|| TerminalPane::new(id));
        let tab_title: SharedString = format!("Terminal {}", pane.next_tab).into();
        pane.next_tab += 1;
        let shell_title: SharedString = format!("Shell {}", pane.next_shell).into();
        pane.next_shell += 1;
        let leaf_id = pane.next_leaf;
        pane.next_leaf += 1;
        pane.tabs.push(TerminalTab {
            title: tab_title,
            root: Some(TerminalNode::Leaf(TerminalLeaf {
                id: leaf_id,
                title: shell_title,
                view: view.clone(),
            })),
            focused: leaf_id,
            zoomed: None,
        });
        pane.active = pane.tabs.len() - 1;
        Self::close_when_exited(&view, vm_for_exit, leaf_id, window, cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes the shell's pane when its shell exits on its own (`exit`, Ctrl+D). A
    /// dropped connection leaves the pane open with Reconnect.
    fn close_when_exited(
        view: &Entity<TerminalView>,
        vm: VmId,
        leaf: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(view, window, move |this, _, _: &ShellExited, window, cx| {
            if this.terminal.as_ref().is_some_and(|pane| pane.vm == vm) {
                this.close_terminal_leaf(leaf, window, cx);
            } else {
                this.close_parked_leaf(vm, leaf, cx);
            }
        })
        .detach();
    }

    /// Closes shell `leaf` of a machine that is not selected, and its pane with its last
    /// shell.
    fn close_parked_leaf(&mut self, vm: VmId, leaf: u64, cx: &mut Context<Self>) {
        let Some(pane) = self.parked.get_mut(&vm) else {
            return;
        };
        pane.tabs.retain_mut(|tab| {
            if let Some(root) = tab.root.take() {
                tab.root = root.into_without(leaf).0;
            }
            tab.root.is_some()
        });
        pane.active = pane.active.min(pane.tabs.len().saturating_sub(1));
        if pane.tabs.is_empty() {
            self.parked.remove(&vm);
        }
        cx.notify();
    }

    /// Closes every shell on `vm`, from its machine tab.
    pub(super) fn close_machine_shells(
        &mut self,
        vm: VmId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal.as_ref().is_some_and(|pane| pane.vm == vm) {
            self.terminal = None;
            if self.tab == Tab::Terminal {
                self.tab = Tab::Overview;
            }
            window.focus(&self.focus, cx);
        }
        self.parked.remove(&vm);
        cx.notify();
    }

    /// The machines shown as tabs in the top bar: those with shells, in the order their
    /// shells were opened, then the selected machine if it has none.
    pub(super) fn machine_tabs(&self) -> Vec<(VmId, bool)> {
        let mut panes: Vec<&TerminalPane> =
            self.terminal.iter().chain(self.parked.values()).collect();
        panes.sort_by_key(|p| p.opened);
        let mut tabs: Vec<(VmId, bool)> = panes.into_iter().map(|p| (p.vm, true)).collect();
        if let Some(id) = self.selected
            && !tabs.iter().any(|(vm, _)| *vm == id)
        {
            tabs.push((id, false));
        }
        tabs
    }

    fn select_terminal_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.terminal.as_mut() else {
            return;
        };
        if index >= pane.tabs.len() {
            return;
        }
        pane.active = index;
        let view = pane.active_view().cloned();
        if let Some(view) = view {
            let focus = view.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Closes a tab and all the shells in it. The last one closes the pane and goes
    /// back to the Overview tab.
    fn close_terminal_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let empty = {
            let Some(pane) = self.terminal.as_mut() else {
                return;
            };
            if index >= pane.tabs.len() {
                return;
            }
            pane.tabs.remove(index);
            if pane.tabs.is_empty() {
                true
            } else {
                if pane.active >= pane.tabs.len() {
                    pane.active = pane.tabs.len() - 1;
                } else if index < pane.active {
                    pane.active -= 1;
                }
                false
            }
        };
        if empty {
            self.terminal = None;
            self.tab = Tab::Overview;
            window.focus(&self.focus, cx);
        } else if let Some(view) = self.terminal.as_ref().and_then(TerminalPane::active_view) {
            let focus = view.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// The shell that Ctrl+Shift+W, split and close act on.
    fn focused_terminal_leaf(&self) -> Option<u64> {
        let pane = self.terminal.as_ref()?;
        let tab = pane.tabs.get(pane.active)?;
        Some(tab.focused_leaf()?.id)
    }

    fn activate_terminal_leaf(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(pane) = self.terminal.as_mut() else {
            return;
        };
        let active = pane.active;
        let Some(tab) = pane.tabs.get_mut(active) else {
            return;
        };
        if tab.focused != id {
            tab.focused = id;
            cx.notify();
        }
    }

    /// Splits the shell `target` in two and focuses the new shell.
    fn split_terminal_at(
        &mut self,
        target: u64,
        dir: SplitDir,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.terminal.as_ref() else {
            return;
        };
        let Some(tab) = pane.tabs.get(pane.active) else {
            return;
        };
        if tab
            .root
            .as_ref()
            .is_none_or(|root| root.leaf(target).is_none())
        {
            return;
        }
        let vm = pane.vm;
        let backend = self.backend.clone();
        let view = cx.new(|cx| TerminalView::new(backend, vm, cx));
        let vm_for_exit = vm;
        let focus = view.read(cx).focus_handle().clone();
        let Some(pane) = self.terminal.as_mut() else {
            return;
        };
        let shell_title: SharedString = format!("Shell {}", pane.next_shell).into();
        pane.next_shell += 1;
        let leaf_id = pane.next_leaf;
        pane.next_leaf += 1;
        let new = TerminalLeaf {
            id: leaf_id,
            title: shell_title,
            view: view.clone(),
        };
        let active = pane.active;
        let tab = &mut pane.tabs[active];
        if tab
            .root
            .as_mut()
            .is_some_and(|root| root.split_leaf(target, dir, new))
        {
            tab.focused = leaf_id;
            tab.zoomed = None;
        }
        Self::close_when_exited(&view, vm_for_exit, leaf_id, window, cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes one shell. Closing the last shell of a tab closes the tab.
    fn close_terminal_leaf(&mut self, target: u64, window: &mut Window, cx: &mut Context<Self>) {
        let empty = {
            let Some(pane) = self.terminal.as_mut() else {
                return;
            };
            let Some(index) = pane.tabs.iter().position(|tab| {
                tab.root
                    .as_ref()
                    .is_some_and(|root| root.leaf(target).is_some())
            }) else {
                return;
            };
            let Some(root) = pane.tabs[index].root.take() else {
                return;
            };
            if pane.tabs[index].zoomed == Some(target) {
                pane.tabs[index].zoomed = None;
            }
            let (kept, _) = root.into_without(target);
            if let Some(node) = kept {
                let focused = node.first_leaf().id;
                pane.tabs[index].root = Some(node);
                if pane.tabs[index].focused == target {
                    pane.tabs[index].focused = focused;
                }
            } else {
                pane.tabs.remove(index);
                if pane.active >= pane.tabs.len() && !pane.tabs.is_empty() {
                    pane.active = pane.tabs.len() - 1;
                } else if index < pane.active {
                    pane.active -= 1;
                }
            }
            pane.tabs.is_empty()
        };
        if empty {
            self.terminal = None;
            self.tab = Tab::Overview;
            window.focus(&self.focus, cx);
        } else if let Some(view) = self.terminal.as_ref().and_then(TerminalPane::active_view) {
            let focus = view.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Shows shell `leaf` alone over its tab, or brings the tab's splits back.
    fn toggle_terminal_zoom(&mut self, leaf: u64, cx: &mut Context<Self>) {
        let Some(tab) = self
            .terminal
            .as_mut()
            .and_then(|pane| pane.tabs.get_mut(pane.active))
        else {
            return;
        };
        if tab
            .root
            .as_ref()
            .is_some_and(|root| root.leaf(leaf).is_some())
        {
            tab.zoomed = if tab.zoomed == Some(leaf) {
                None
            } else {
                Some(leaf)
            };
            tab.focused = leaf;
            cx.notify();
        }
    }

    fn toggle_terminal_max(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.terminal.as_mut() else {
            return;
        };
        pane.maximized = !pane.maximized;
        cx.notify();
    }

    fn toggle_machines(&mut self, cx: &mut Context<Self>) {
        self.show_machines = !self.show_machines;
        cx.notify();
    }

    /// The panel button that shows and hides the machine list.
    fn themes_button(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("open-themes")
            .debug_selector(|| "open-themes".to_owned())
            .size(px(28.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(radius::M)
            .cursor_pointer()
            .when(self.themes.is_some(), |d| d.bg(rgb(palette().hover)))
            .hover(|d| d.bg(rgb(palette().hover)))
            .child(icon(Icon::Palette, px(15.), palette().text_secondary))
            .on_click(cx.listener(|this, _, window, cx| this.open_themes(window, cx)))
            .into_any_element()
    }

    fn machines_button(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .size(px(28.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(radius::M)
            .cursor_pointer()
            .when(!self.show_machines, |d| d.bg(rgb(palette().hover)))
            .hover(|d| d.bg(rgb(palette().hover)))
            .child(icon(Icon::Sidebar, px(15.), palette().text_secondary))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_machines(cx)))
            .into_any_element()
    }

    fn load_detail(&mut self, id: VmId, cx: &mut Context<Self>) {
        let window = self.timeline.clone();
        let load = self.backend.detail(id, window.clone());
        cx.spawn(async move |this, cx| {
            let detail = load.await;
            this.update(cx, |v, cx| {
                if v.selected != Some(id) || v.timeline != window {
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

    /// Runs the Ctrl+Shift shortcut for `key` (see [`crate::term_keys::APP_SHORTCUTS`]), the
    /// terminal ones only while the Terminal tab is open. `false` when `key` is none of them.
    fn app_shortcut(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let leaf = self.focused_terminal_leaf();
        let terminal = self.tab == Tab::Terminal;
        match key {
            "b" => self.toggle_machines(cx),
            "p" => self.open_themes(window, cx),
            "t" if terminal => self.add_terminal_tab(window, cx),
            "m" if terminal => self.toggle_terminal_max(cx),
            "w" | "d" | "e" | "z" if terminal => {
                if let Some(leaf) = leaf {
                    match key {
                        "w" => self.close_terminal_leaf(leaf, window, cx),
                        "d" => self.split_terminal_at(leaf, SplitDir::Row, window, cx),
                        "e" => self.split_terminal_at(leaf, SplitDir::Column, window, cx),
                        _ => self.toggle_terminal_zoom(leaf, cx),
                    }
                }
            }
            _ => return false,
        }
        true
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
        if ks.modifiers.control
            && ks.modifiers.shift
            && !ks.modifiers.alt
            && self.app_shortcut(&ks.key, window, cx)
        {
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
            .is_some_and(|s| s.field.read(cx).focus_handle().is_focused(window))
            || self
                .themes
                .as_ref()
                .is_some_and(|t| t.field.read(cx).focus_handle().is_focused(window));
        match ks.key.as_str() {
            "escape" if self.menu_open => self.menu_open = false,
            "escape" if self.inline.is_some() => self.cancel_inline(window, cx),
            "escape" if self.search.is_some() => self.close_search(window, cx),
            "escape" if self.themes.is_some() => self.close_themes(window, cx),
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

    /// Opens the theme picker, or moves the focus back into it.
    fn open_themes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.add.is_some() {
            return;
        }
        self.search = None;
        let picker = self.themes.get_or_insert_with(|| ThemePicker::new(cx));
        let focus = picker.field.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        self.inline = None;
        self.menu_open = false;
        cx.notify();
    }

    fn close_themes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.themes = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Switches to theme `name` everywhere, and saves it for the next start.
    pub(crate) fn apply_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        if !crate::theme::palette::select(name) {
            return;
        }
        if let Err(e) = crate::theme::palette::remember(&self.backend.data_dir, name) {
            self.message = Some((format!("Could not save the theme: {e}"), true));
        }
        cx.refresh_windows();
    }

    /// Opens the event search, or moves the focus back into it.
    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.add.is_some() {
            return;
        }
        self.themes = None;
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

    /// Shows a search hit: its VM's Timeline, loaded around it and scrolled to it, with
    /// its row tinted for [`HIGHLIGHT`].
    pub(crate) fn open_hit(
        &mut self,
        vm: VmId,
        hit: EventKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_search(window, cx);
        self.select(vm, cx);
        self.tab = Tab::Timeline;
        let now = cx.background_executor().now();
        self.show_timeline(TimelineWindow::around(hit, now), cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    /// Loads `window` of the selected VM's events into the Timeline tab.
    pub(crate) fn show_timeline(&mut self, window: TimelineWindow, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        self.timeline = window;
        self.load_detail(id, cx);
        cx.notify();
    }

    /// Once the detail shows a VM, or its Timeline another place, scrolls the body to the
    /// search hit or to the top: in this frame, and again after it, because the body may
    /// have moved in this frame (the Timeline's banner appears) and a scroll request
    /// measures from where the body was in the last one.
    fn scroll_if_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(shown) = self.detail.as_ref().map(|d| &d.timeline.window) else {
            return;
        };
        if self
            .scrolled_for
            .as_ref()
            .is_some_and(|w| w.same_place(shown))
        {
            return;
        }
        self.scrolled_for = Some(shown.clone());
        self.scroll_to_anchor();
        cx.on_next_frame(window, |this, _, cx| {
            this.scroll_to_anchor();
            cx.notify();
        });
    }

    /// Scrolls the detail body to the Timeline's search hit, or to the top without one.
    fn scroll_to_anchor(&self) {
        let anchor = self.detail.as_ref().and_then(|d| {
            crate::timeline_view::anchor(&crate::timeline_view::items(&d.timeline, Local::now()))
        });
        match anchor {
            Some(ix) => self.detail_scroll.scroll_to_top_of_item(ix),
            None => self.detail_scroll.set_offset(point(px(0.), px(0.))),
        }
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
        let status = self.status.get(&id);
        let state = status.map_or(AgentState::Unknown, |s| s.agent);
        let reason = status.and_then(|s| s.agent_detail.as_deref());
        self.detail
            .as_ref()
            .map(|d| AgentView::new(d.agent.as_ref(), state, reason))
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
                palette().accent
            } else {
                palette().border_strong
            }))
            .when(active, |d| d.shadow(focus_ring()))
            .bg(rgb(palette().bg))
            .cursor_text()
            .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx)))
            .child(icon(Icon::Search, px(14.), palette().text_muted))
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
                            .bg(rgb(palette().accent)),
                    )
            } else {
                div()
                    .flex_1()
                    .text_size(text::BODY)
                    .text_color(rgb(palette().text_muted))
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
                        .hover(|s| s.bg(rgb(palette().hover)))
                        .child(icon(Icon::Close, px(12.), palette().text_secondary))
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
        let status = self.status.get(&id);
        let label_text =
            status.map_or_else(|| "never checked".to_owned(), |s| s.label(vm.lifecycle));
        let headline = self.headline(vm);
        let agent = status.is_some_and(|s| s.agent.collects());
        // One line of context: what you were doing there, else why it is not reachable,
        // else how it is reached.
        let context = match (self.work.get(&id), shell::Group::of(headline), status) {
            (Some(line), _, _) => line.clone(),
            (None, shell::Group::Attention | shell::Group::Offline, Some(s)) => {
                sentence_case(ssh_fact(s.ssh))
            }
            (None, _, Some(s)) => s
                .route
                .as_ref()
                .map_or_else(|| "idle".to_owned(), |r| format!("via {r}")),
            (None, _, None) => "not checked yet".to_owned(),
        };
        let tone = headline_tone(headline);
        let context_color = match shell::Group::of(headline) {
            shell::Group::Attention | shell::Group::Offline => tone_color(tone),
            _ if selected => palette().text_secondary,
            _ => palette().text_muted,
        };
        let row_label = match status.and_then(|s| s.route.clone()) {
            Some(r) => format!("{label_text} · {r}"),
            None => label_text,
        };
        div()
            .id(SharedString::from(vm.id.to_string()))
            .debug_selector(move || format!("{}: {row_label}", vm.name))
            .px(space::S)
            .py(px(8.))
            .flex()
            .gap(px(10.))
            .items_start()
            .rounded(radius::L)
            .cursor_pointer()
            .when(selected, |d| d.bg(rgb(palette().hover)))
            .when(!selected, |d| d.hover(|s| s.bg(tint(palette().hover))))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.close_search(window, cx);
                this.select(id, cx);
            }))
            .child(dot(tone_color(tone), 8.).mt(px(6.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .min_w_0()
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
                            .when(agent, |d| {
                                d.child(
                                    div()
                                        .text_size(text::SMALL)
                                        .text_color(rgb(palette().accent))
                                        .child("agent"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(text::META)
                            .text_color(rgb(context_color))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(context.clone())
                            .when_some(self.work.get(&id), |d, line| {
                                let name = vm.name.clone();
                                let line = line.clone();
                                d.debug_selector(move || format!("{name} work: {line}"))
                            }),
                    ),
            )
    }

    /// The pane shown in place of the selected VM: the Add VM form, the theme picker or
    /// the event search, when one is open.
    fn render_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(form) = &self.add {
            return Some(
                div()
                    .id("add-form")
                    .flex_1()
                    .h_full()
                    .overflow_y_scroll()
                    .child(crate::form::render(form, &self.vms, cx))
                    .into_any_element(),
            );
        }
        if let Some(picker) = &self.themes {
            return Some(
                div()
                    .id("theme-pane")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(crate::theme_view::render(picker, cx))
                    .into_any_element(),
            );
        }
        if let Some(search) = &self.search {
            return Some(
                div()
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
                    .into_any_element(),
            );
        }
        None
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(pane) = self.render_overlay(cx) {
            return pane;
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
                        .text_color(rgb(palette().text_secondary))
                        .child("Select a machine"),
                )
                .child(label("or add one with Add VM"))
                .into_any_element();
        };
        let header = self.render_header(vm, cx);
        let banner = match self.tab {
            Tab::Timeline => crate::timeline_view::banner(&self.timeline, cx),
            Tab::Overview | Tab::Sessions | Tab::Terminal => None,
        };
        let (body, pinned_day) = self.tab_body(cx);
        let terminal = self.tab == Tab::Terminal;

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .children(banner)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(if terminal {
                        // The grid fills the pane; its own canvas handles scrolling.
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .child(div().flex_1().min_w_0().min_h_0().children(body))
                            .into_any_element()
                    } else {
                        div()
                            .id("detail-body")
                            .debug_selector(|| "detail-body".to_owned())
                            .track_scroll(&self.detail_scroll)
                            .flex_1()
                            .overflow_y_scroll()
                            .px(space::XL)
                            .pt(space::L)
                            .pb(space::XL)
                            .children(body)
                            .into_any_element()
                    })
                    .children(pinned_day.map(|d| crate::timeline_view::pinned(&d))),
            )
            .into_any_element()
    }

    /// The Terminal tab: a strip of shells over the active grid.
    fn render_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pane) = &self.terminal else {
            return div().child(label("Select a machine")).into_any_element();
        };
        let mut body = div().flex_1().min_h_0().flex().p(px(6.));
        if let Some(tab) = pane.tabs.get(pane.active)
            && let Some(root) = &tab.root
        {
            let focused = tab.focused_leaf().map_or(tab.focused, |leaf| leaf.id);
            let zoomed = tab.zoomed.and_then(|id| root.leaf(id));
            body = body.child(match zoomed {
                Some(leaf) => Self::render_terminal_leaf(leaf, focused, true, cx),
                None => Self::render_terminal_node(root, focused, cx),
            });
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(Self::terminal_strip(pane, cx))
            .child(body)
            .into_any_element()
    }

    /// One shell card, or two laid out by the split that joins them.
    fn render_terminal_node(
        node: &TerminalNode,
        focused: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            TerminalNode::Leaf(leaf) => Self::render_terminal_leaf(leaf, focused, false, cx),
            TerminalNode::Split { dir, first, second } => {
                let container = div().flex_1().min_w_0().min_h_0().flex().gap(px(6.));
                let container = if *dir == SplitDir::Row {
                    container.flex_row()
                } else {
                    container.flex_col()
                };
                container
                    .child(Self::render_terminal_node(first, focused, cx))
                    .child(Self::render_terminal_node(second, focused, cx))
                    .into_any_element()
            }
        }
    }

    /// One shell's card: its header and the terminal.
    fn render_terminal_leaf(
        leaf: &TerminalLeaf,
        focused: u64,
        zoomed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = leaf.id;
        let active = id == focused;
        let header = Self::terminal_leaf_header(leaf, active, zoomed, cx);
        div()
            .id(SharedString::from(format!("terminal-leaf-{id}")))
            .group(SharedString::from(format!("terminal-leaf-{id}")))
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(radius::L)
            .overflow_hidden()
            .border_1()
            .border_color(rgb(if active {
                palette().border_strong
            } else {
                palette().border
            }))
            .bg(rgb(palette().elevated))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.activate_terminal_leaf(id, cx);
                }),
            )
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(div().flex_1().min_w_0().min_h_0().child(leaf.view.clone())),
            )
            .into_any_element()
    }

    /// The title and the split, zoom and close controls at the top of a shell card. The
    /// focused card always shows its controls; the others show them while hovered.
    fn terminal_leaf_header(
        leaf: &TerminalLeaf,
        active: bool,
        zoomed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let id = leaf.id;
        let split = |glyph: Icon, dir: SplitDir, selector: &'static str, name: &'static str| {
            Self::terminal_leaf_button(
                format!("terminal-split-{name}-{id}"),
                glyph,
                active.then_some(selector),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.split_terminal_at(id, dir, window, cx);
            }))
        };
        let controls = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when(!active, |d| {
                d.opacity(0.)
                    .group_hover(format!("terminal-leaf-{id}"), |s| s.opacity(1.))
            })
            .child(split(
                Icon::SplitRow,
                SplitDir::Row,
                "terminal-split-row",
                "row",
            ))
            .child(split(
                Icon::SplitColumn,
                SplitDir::Column,
                "terminal-split-column",
                "column",
            ))
            .child(
                Self::terminal_leaf_button(
                    format!("terminal-zoom-{id}"),
                    if zoomed { Icon::Collapse } else { Icon::Expand },
                    active.then_some("terminal-zoom"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_terminal_zoom(id, cx);
                })),
            )
            .child(
                Self::terminal_leaf_button(
                    format!("terminal-close-leaf-{id}"),
                    Icon::Close,
                    active.then_some("terminal-close-leaf"),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_terminal_leaf(id, window, cx);
                })),
            );
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(30.))
            .pl(px(10.))
            .pr(px(6.))
            .text_size(text::META)
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(if active {
                palette().text
            } else {
                palette().text_secondary
            }))
            .child(icon(
                Icon::Terminal,
                px(13.),
                if active {
                    palette().accent
                } else {
                    palette().text_muted
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(leaf.title.clone()),
            )
            .when(zoomed, |d| {
                d.child(
                    div()
                        .text_size(text::SMALL)
                        .text_color(rgb(palette().text_muted))
                        .child("zoomed"),
                )
            })
            .child(controls)
    }

    /// A small icon button in a shell's card header.
    fn terminal_leaf_button(
        id: String,
        glyph: Icon,
        debug: Option<&'static str>,
    ) -> gpui::Stateful<gpui::Div> {
        let mut button = div()
            .id(SharedString::from(id))
            .size(px(22.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|d| d.bg(rgb(palette().hover)))
            .child(icon(glyph, px(13.), palette().text_muted));
        if let Some(selector) = debug {
            button = button.debug_selector(move || selector.to_owned());
        }
        button
    }

    /// The shells' tabs, the new-shell button and the maximize toggle.
    fn terminal_strip(pane: &TerminalPane, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(36.))
            .px(space::M)
            .bg(rgb(palette().surface))
            .border_b_1()
            .border_color(rgb(palette().border))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(4.))
                    .overflow_hidden()
                    .children(pane.tabs.iter().enumerate().map(|(index, tab)| {
                        let active = index == pane.active;
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(2.))
                            .h(px(26.))
                            .pl(px(8.))
                            .pr(px(2.))
                            .rounded(radius::M)
                            .bg(if active {
                                rgb(palette().elevated)
                            } else {
                                rgb(palette().surface)
                            })
                            .text_color(if active {
                                rgb(palette().text)
                            } else {
                                rgb(palette().text_secondary)
                            })
                            .when(!active, |d| d.hover(|s| s.bg(rgb(palette().hover))))
                            .child(
                                div()
                                    .id(SharedString::from(format!("terminal-tab-{index}")))
                                    .debug_selector(|| format!("terminal-tab-{index}"))
                                    .h_full()
                                    .pr(px(6.))
                                    .flex()
                                    .items_center()
                                    .text_size(text::META)
                                    .cursor_pointer()
                                    .child(tab.title.clone())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select_terminal_tab(index, window, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("terminal-close-{index}")))
                                    .debug_selector(|| format!("terminal-close-{index}"))
                                    .size(px(18.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(palette().border_strong)))
                                    .child(icon(Icon::Close, px(11.), palette().text_muted))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.close_terminal_tab(index, window, cx);
                                    })),
                            )
                    })),
            )
            .child(
                icon_button("terminal-new", Icon::Plus, false)
                    .on_click(cx.listener(|this, _, window, cx| this.add_terminal_tab(window, cx))),
            )
            .child(
                icon_button(
                    "terminal-max",
                    if pane.maximized {
                        Icon::Collapse
                    } else {
                        Icon::Expand
                    },
                    pane.maximized,
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_terminal_max(cx))),
            )
    }

    /// The selected VM's open tab, as the detail body's children: one element, or for
    /// the Timeline one per day header and row, so the body can scroll to a row. With
    /// it, the day to pin over the top once its header has scrolled away.
    fn tab_body(&self, cx: &mut Context<Self>) -> (Vec<AnyElement>, Option<String>) {
        let one = match (&self.detail, self.tab) {
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
                        el.child(crate::overview::render(s, &d.timeline.events, cx))
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
            (Some(_), Tab::Terminal) => self.render_terminal(cx),
            (Some(d), Tab::Timeline) => {
                let items = crate::timeline_view::items(&d.timeline, Local::now());
                let pinned =
                    crate::timeline_view::pinned_day(&items, self.detail_scroll.top_item());
                let now = cx.background_executor().now();
                let rows = crate::timeline_view::render(items, &d.timeline.window, now, cx);
                return (rows, pinned);
            }
        };
        (vec![one], None)
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
                                .child(menu_action("check", "Check now", false).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.menu_open = false;
                                        this.check(id, cx);
                                    }),
                                ))
                                .child(menu_action("edit", "Edit VM…", false).on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.menu_open = false;
                                        this.open_edit_form(id, window, cx);
                                    }),
                                ))
                                .child(
                                    menu_action(
                                        "external-terminal",
                                        "Open in external terminal",
                                        false,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.menu_open = false;
                                            this.open_terminal(id, cx);
                                        },
                                    )),
                                )
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

    /// The machine's name, state and actions, over one quiet line of facts.
    fn title_row(&self, vm: &Vm, cx: &mut Context<Self>) -> gpui::Div {
        let id = vm.id;
        let status = self.status.get(&id);
        let headline = status.map(|s| s.label(vm.lifecycle));
        let login = vm
            .endpoints
            .first()
            .map(|e| format!("{}@{}:{}", vm.username, e.address, e.port))
            .unwrap_or_default();
        let mut facts = vec![self.path_text(vm)];
        if let Some(s) = status {
            facts.push(ssh_fact(s.ssh).to_owned());
            facts.extend(s.route.as_ref().map(|r| format!("via {r}")));
            facts.push(format!(
                "checked {} ago",
                ago((Utc::now() - s.checked_at).num_seconds())
            ));
        }
        let agent = self.agent_view(id);
        let busy = self.busy.map(|b| format!("{}…", sentence_case(b)));
        let meta = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .text_size(text::META)
            .text_color(rgb(palette().text_muted))
            .child(div().font_family(MONO).child(login))
            .children(facts.into_iter().map(|f| div().child(format!("· {f}"))))
            .when_some(agent, |d, agent| {
                let text = agent.text(Utc::now());
                d.child(
                    div()
                        .debug_selector(|| format!("agent: {text}"))
                        .ml(px(4.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(dot(tone_color(agent.tone()), 6.))
                        .child(text.clone()),
                )
            });
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
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::M)
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(24.))
                                    .font_weight(FontWeight::BOLD)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(vm.name.clone()),
                            )
                            .child(
                                pill(
                                    headline_tone(status.map(|s| s.headline(vm.lifecycle))),
                                    headline
                                        .as_deref()
                                        .map_or_else(|| "Never checked".to_owned(), sentence_case),
                                )
                                .debug_selector(|| {
                                    format!("headline: {}", headline.clone().unwrap_or_default())
                                }),
                            ),
                    )
                    .child(meta),
            )
            .when_some(busy, |d, b| d.child(label(b)))
            .child(
                button(Button::Secondary, "collect")
                    .child("Refresh")
                    .on_click(cx.listener(move |this, _, _, cx| this.collect(id, cx))),
            )
            .child(
                button(Button::Primary, "terminal")
                    .child(icon(Icon::Terminal, px(14.), palette().on_accent))
                    .child("Open terminal")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_tab(Tab::Terminal, window, cx);
                    })),
            )
            .child(self.more_menu(vm, cx))
    }

    fn render_header(&self, vm: &Vm, cx: &mut Context<Self>) -> gpui::Div {
        let id = vm.id;
        let status = self.status.get(&id);
        div()
            .px(space::XL)
            .pt(space::XL)
            .pb(space::L)
            .flex()
            .flex_col()
            .gap(space::L)
            .child(self.title_row(vm, cx))
            .when_some(status.and_then(|s| s.detail.clone()), |d, detail| {
                d.child(
                    div()
                        .text_size(text::META)
                        .text_color(rgb(palette().amber))
                        .child(detail),
                )
            })
            .when_some(self.agent_view(id), |d, agent| {
                d.when_some(agent.detail(), |d, why| {
                    d.child(
                        div()
                            .debug_selector(|| format!("agent-detail: {why}"))
                            .text_size(text::META)
                            .text_color(rgb(tone_color(agent.tone())))
                            .child(why),
                    )
                })
            })
            .when_some(self.message.clone(), |d, (m, err)| {
                d.child(
                    div()
                        .text_size(text::META)
                        .text_color(rgb(if err {
                            palette().red
                        } else {
                            palette().text_secondary
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
    }

    /// Overview · Sessions · Timeline (· Terminal), for the top bar.
    pub(super) fn view_switch(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .p(px(3.))
            .flex()
            .items_center()
            .gap(px(2.))
            .self_start()
            .rounded(radius::L)
            .bg(rgb(palette().bg))
            .border_1()
            .border_color(rgb(palette().border))
            .child(self.tab_button("tab-overview", "Overview", Tab::Overview, cx))
            .child(self.tab_button("tab-sessions", "Sessions", Tab::Sessions, cx))
            .child(self.tab_button("tab-timeline", "Timeline", Tab::Timeline, cx))
            // The Terminal tab exists only while a shell does; Open terminal
            // creates it.
            .when(self.terminal.is_some(), |d| {
                d.child(self.tab_button("tab-terminal", "Terminal", Tab::Terminal, cx))
            })
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
            .h(px(28.))
            .px(px(14.))
            .flex()
            .items_center()
            .rounded(radius::M)
            .cursor_pointer()
            .text_size(text::BODY)
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(if active {
                palette().text
            } else {
                palette().text_secondary
            }))
            .when(active, |d| d.bg(rgb(palette().hover)))
            .when(!active, |d| {
                d.hover(|s| s.bg(rgb(palette().hover)).text_color(rgb(palette().text)))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_tab(tab, window, cx);
            }))
            .child(content)
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.scroll_if_moved(window, cx);
        let title = match self
            .selected
            .and_then(|id| self.vms.iter().find(|v| v.id == id))
        {
            Some(vm) => format!("VMDesk — {}", vm.name),
            None => "VMDesk".to_owned(),
        };
        window.set_window_title(&title);
        let maximized =
            self.tab == Tab::Terminal && self.terminal.as_ref().is_some_and(|pane| pane.maximized);
        let workspace = if maximized {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(self.render_terminal(cx))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(if self.show_machines {
                    self.sidebar(cx)
                } else {
                    self.rail(cx)
                })
                .child(self.render_detail(cx))
                .into_any_element()
        };
        let body = div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(palette().bg))
            .text_color(rgb(palette().text))
            .font_family(SANS)
            .text_size(text::BODY)
            .child(self.top_bar(window, cx))
            .child(workspace)
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

#[path = "shell.rs"]
mod shell;

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
