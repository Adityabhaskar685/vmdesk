//! The "Add VM" form.

use gpui::{App, AppContext, Context, Entity, FocusHandle, Window, div, prelude::*, px, rgb};
use vmdesk_core::{CredentialRef, Hop, Vm, VmId};

use crate::backend::{Auth, NewVmSpec};
use crate::field::TextField;
use crate::ui::{AppView, button, color, label};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Agent,
    KeyFile,
    Password,
}

pub struct AddForm {
    name: Entity<TextField>,
    address: Entity<TextField>,
    port: Entity<TextField>,
    user: Entity<TextField>,
    key: Entity<TextField>,
    password: Entity<TextField>,
    pub via: Option<VmId>,
    pub auth: AuthKind,
    pub error: Option<String>,
    pub saving: bool,
    /// The VM being edited, with the values the form started from.
    pub editing: Option<Original>,
}

/// What an edited VM looked like, so only changed connection details are sent.
pub struct Original {
    pub id: VmId,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub via: Option<VmId>,
    pub had_password: bool,
}

impl AddForm {
    pub fn new<T: 'static>(cx: &mut Context<T>) -> Self {
        let field =
            |cx: &mut Context<T>, p: &str, secret: bool| cx.new(|cx| TextField::new(p, secret, cx));
        Self {
            name: field(cx, "e.g. lab-07", false),
            address: field(cx, "IP address or hostname", false),
            port: field(cx, "22", false),
            user: field(cx, &local_user(), false),
            key: field(cx, "~/.ssh/id_ed25519", false),
            password: field(cx, "stored in the OS keychain", true),
            via: None,
            auth: AuthKind::Agent,
            error: None,
            saving: false,
            editing: None,
        }
    }

    /// The form filled in from `vm`, for editing.
    pub fn edit<T: 'static>(vm: &Vm, cx: &mut Context<T>) -> Self {
        let field = |cx: &mut Context<T>, p: &str, text: &str, secret: bool| {
            cx.new(|cx| TextField::new(p, secret, cx).with_text(text))
        };
        let ep = vm.endpoints.first();
        let (address, port) = ep.map_or((String::new(), 22), |e| (e.address.clone(), e.port));
        let via = vm.paths.first().and_then(|p| match p.hops.first() {
            Some(Hop::Jump { via }) => Some(*via),
            _ => None,
        });
        let (auth, key) = match &vm.credential {
            CredentialRef::KeyFile(k) => (AuthKind::KeyFile, k.display().to_string()),
            CredentialRef::KeyChain(_) => (AuthKind::Password, String::new()),
            CredentialRef::SshAgent | CredentialRef::CloudProvider(_) => {
                (AuthKind::Agent, String::new())
            }
        };
        let had_password = matches!(vm.credential, CredentialRef::KeyChain(_));
        Self {
            name: field(cx, "e.g. lab-07", &vm.name, false),
            address: field(cx, "IP address or hostname", &address, false),
            port: field(cx, "22", &port.to_string(), false),
            user: field(cx, &local_user(), &vm.username, false),
            key: field(cx, "~/.ssh/id_ed25519", &key, false),
            password: field(
                cx,
                if had_password {
                    "leave empty to keep the stored password"
                } else {
                    "stored in the OS keychain"
                },
                "",
                true,
            ),
            via,
            auth,
            error: None,
            saving: false,
            editing: Some(Original {
                id: vm.id,
                name: vm.name.clone(),
                address,
                port,
                via,
                had_password,
            }),
        }
    }

    pub fn first_field(&self, cx: &App) -> FocusHandle {
        self.name.read(cx).focus_handle().clone()
    }

    /// Fields in Tab order; the key and password fields only when that method is chosen.
    fn fields(&self) -> Vec<&Entity<TextField>> {
        let mut f = vec![&self.name, &self.address, &self.port, &self.user];
        match self.auth {
            AuthKind::KeyFile => f.push(&self.key),
            AuthKind::Password => f.push(&self.password),
            AuthKind::Agent => {}
        }
        f
    }

    /// Moves focus to the next (or previous) field.
    pub fn tab(&self, back: bool, window: &mut Window, cx: &mut App) {
        let handles: Vec<FocusHandle> = self
            .fields()
            .iter()
            .map(|f| f.read(cx).focus_handle().clone())
            .collect();
        let at = handles
            .iter()
            .position(|h| h.is_focused(window))
            .unwrap_or(0);
        let n = handles.len();
        let next = if back { (at + n - 1) % n } else { (at + 1) % n };
        window.focus(&handles[next], cx);
    }

    /// Reads and checks the fields.
    pub fn spec(&self, cx: &App) -> Result<NewVmSpec, String> {
        let text = |f: &Entity<TextField>| f.read(cx).text().trim().to_owned();
        let name = text(&self.name);
        let address = text(&self.address);
        if name.is_empty() || address.is_empty() {
            return Err("name and address are required".into());
        }
        let port = match text(&self.port) {
            p if p.is_empty() => 22,
            p => p.parse().map_err(|_| format!("bad port {p:?}"))?,
        };
        let user = match text(&self.user) {
            u if u.is_empty() => local_user(),
            u => u,
        };
        let auth = match self.auth {
            AuthKind::Agent => Auth::Agent,
            AuthKind::KeyFile => match text(&self.key) {
                k if k.is_empty() => return Err("choose a key file".into()),
                k => Auth::KeyFile(expand_home(&k)),
            },
            AuthKind::Password => match self.password.read(cx).text() {
                "" if self.editing.as_ref().is_some_and(|o| o.had_password) => Auth::KeepPassword,
                "" => return Err("enter the password".into()),
                p => Auth::Password(p.to_owned()),
            },
        };
        Ok(NewVmSpec {
            name,
            address,
            port,
            user,
            via: self.via,
            auth,
        })
    }
}

fn local_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "root".to_owned())
}

fn expand_home(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => path.into(),
    }
}

fn chip(id: impl Into<gpui::ElementId>, text: String, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py(px(3.))
        .rounded_md()
        .border_1()
        .text_xs()
        .cursor_pointer()
        .border_color(rgb(if on { color::ACCENT } else { color::BORDER }))
        .when(on, |d| d.bg(rgb(color::SELECTED)))
        .hover(|s| s.bg(rgb(color::ROW_HOVER)))
        .child(text)
}

fn labelled(name: &str, field: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(label(name.to_owned()))
        .child(field)
}

pub fn render(form: &AddForm, vms: &[Vm], cx: &mut Context<AppView>) -> gpui::Div {
    let auth_chip = |kind: AuthKind, id: &'static str, text: &str, cx: &mut Context<AppView>| {
        chip(id, text.to_owned(), form.auth == kind).on_click(cx.listener(
            move |this, _, window, cx| {
                if let Some(f) = this.add.as_mut() {
                    f.auth = kind;
                    if kind != AuthKind::Agent {
                        let field = if kind == AuthKind::KeyFile {
                            &f.key
                        } else {
                            &f.password
                        };
                        let h = field.read(cx).focus_handle().clone();
                        window.focus(&h, cx);
                    }
                }
                cx.notify();
            },
        ))
    };
    div()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .max_w(px(640.))
        .child(div().text_lg().child(match &form.editing {
            Some(o) => format!("Edit {}", o.name),
            None => "Add VM".to_owned(),
        }))
        .child(
            div()
                .flex()
                .gap_3()
                .child(div().flex_1().child(labelled("Name", form.name.clone())))
                .child(
                    div()
                        .flex_1()
                        .child(labelled("Address", form.address.clone())),
                )
                .child(div().w(px(80.)).child(labelled("Port", form.port.clone()))),
        )
        .child(labelled("User", form.user.clone()))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(label("Log in with"))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(auth_chip(
                            AuthKind::Agent,
                            "auth-agent",
                            "ssh-agent / default keys",
                            cx,
                        ))
                        .child(auth_chip(AuthKind::KeyFile, "auth-key", "key file", cx))
                        .child(auth_chip(
                            AuthKind::Password,
                            "auth-password",
                            "password",
                            cx,
                        )),
                ),
        )
        .when(form.auth == AuthKind::KeyFile, |d| {
            d.child(labelled("Key file", form.key.clone()))
        })
        .when(form.auth == AuthKind::Password, |d| {
            d.child(labelled("Password", form.password.clone()))
        })
        .child(via_section(form, vms, cx))
        .when_some(form.error.clone(), |d, e| {
            d.child(div().text_sm().text_color(rgb(color::RED)).child(e))
        })
        .child(actions(form, cx))
}

/// "direct" or one of the other VMs as the jump host.
fn via_section(form: &AddForm, vms: &[Vm], cx: &mut Context<AppView>) -> gpui::Div {
    let me = form.editing.as_ref().map(|o| o.id);
    let mut jump_hosts: Vec<&Vm> = vms.iter().filter(|v| Some(v.id) != me).collect();
    jump_hosts.sort_by_key(|v| v.name.to_lowercase());
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(label("Reach it through (jump host)"))
        .child(
            div()
                .id("via-list")
                .max_h(px(110.))
                .overflow_y_scroll()
                .flex()
                .flex_wrap()
                .gap_1()
                .child(
                    chip("via-none", "direct".into(), form.via.is_none()).on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(f) = this.add.as_mut() {
                                f.via = None;
                            }
                            cx.notify();
                        },
                    )),
                )
                .children(jump_hosts.into_iter().map(|v| {
                    let id = v.id;
                    chip(
                        gpui::ElementId::Name(format!("via-{id}").into()),
                        v.name.clone(),
                        form.via == Some(id),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = this.add.as_mut() {
                            f.via = Some(id);
                        }
                        cx.notify();
                    }))
                })),
        )
}

/// Save / Cancel and the keyboard hint.
fn actions(form: &AddForm, cx: &mut Context<AppView>) -> gpui::Div {
    div()
        .flex()
        .gap_2()
        .child(
            button(
                "add-save",
                match (form.saving, form.editing.is_some()) {
                    (true, _) => "Saving…",
                    (false, true) => "Save",
                    (false, false) => "Add",
                },
            )
            .on_click(cx.listener(|this, _, _, cx| this.save_new_vm(cx))),
        )
        .child(
            button("add-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| {
                this.add = None;
                cx.notify();
            })),
        )
        .child(label(
            "Tab moves between fields · Enter saves · Esc cancels",
        ))
}
