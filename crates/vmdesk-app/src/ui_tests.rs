//! Headless window tests: a real `AppView` in GPUI's test platform, driven with simulated
//! keystrokes, on an in-memory database. Nothing is drawn on screen.

use std::time::{Duration, Instant};

use gpui::{Entity, TestAppContext, VisualTestContext};
use vmdesk_core::VmId;

use super::*;

/// Keeps the tokio runtime alive for the test's duration.
struct Env {
    _rt: tokio::runtime::Runtime,
    backend: Backend,
}

fn env() -> Env {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let db = rt.block_on(vmdesk_db::Db::open_in_memory()).unwrap();
    let dir = std::env::temp_dir().join(format!("vmdesk-ui-{}", VmId::new()));
    let backend = Backend::new(rt.handle().clone(), db, dir);
    Env { _rt: rt, backend }
}

/// Runs GPUI until `done` holds, giving the tokio side time to finish in between.
fn wait(
    cx: &mut VisualTestContext,
    view: &Entity<AppView>,
    what: &str,
    done: impl Fn(&AppView) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |v, _| done(v)) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn form_error(cx: &mut VisualTestContext, view: &Entity<AppView>) -> Option<String> {
    view.read_with(cx, |v, _| v.add.as_ref().and_then(|f| f.error.clone()))
}

#[gpui::test]
fn add_validate_and_remove_a_vm(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let env = env();
    let backend = env.backend.clone();
    let (view, cx) = cx.add_window_view(|_, cx| AppView::new(backend, None, cx));
    view.update_in(cx, |v, window, cx| {
        let focus = v.focus.clone();
        window.focus(&focus, cx);
        v.reload(cx);
    });
    wait(cx, &view, "the empty inventory", |v| v.vms.is_empty());

    // An empty form is refused.
    view.update_in(cx, AppView::open_add_form);
    cx.simulate_keystrokes("enter");
    assert_eq!(
        form_error(cx, &view).as_deref(),
        Some("name and address are required")
    );

    // Name, Tab, address, Tab, port, Tab, user; Backspace edits.
    cx.simulate_keystrokes("l a b 1 x backspace tab 1 2 7 . 0 . 0 . 1 tab 1 tab r o o t");
    view.read_with(cx, |v, cx| {
        let spec = v.add.as_ref().unwrap().spec(cx).unwrap();
        assert_eq!(
            (
                spec.name.as_str(),
                spec.address.as_str(),
                spec.port,
                spec.user.as_str()
            ),
            ("lab1", "127.0.0.1", 1, "root")
        );
    });
    cx.simulate_keystrokes("enter");
    wait(cx, &view, "the VM to be added and selected", |v| {
        v.add.is_none() && v.vms.len() == 1 && v.selected == Some(v.vms[0].id)
    });
    // Port 1 on loopback refuses: the first check after adding shows that.
    wait(cx, &view, "the first status check", |v| {
        v.status
            .values()
            .any(|s| s.ssh == vmdesk_core::SshState::Refused)
    });

    // A second VM with the same name is refused by the database.
    view.update_in(cx, AppView::open_add_form);
    cx.simulate_keystrokes("l a b 1 tab 1 0 . 0 . 0 . 9 enter");
    wait(cx, &view, "the duplicate-name error", |v| {
        v.add
            .as_ref()
            .is_some_and(|f| !f.saving && f.error.is_some())
    });
    assert!(form_error(cx, &view).unwrap().contains("already exists"));
    cx.simulate_keystrokes("escape");
    assert!(
        view.read_with(cx, |v, _| v.add.is_none()),
        "Escape closes the form"
    );

    // Typing with no form open filters the list instead.
    cx.simulate_keystrokes("z z");
    assert_eq!(view.read_with(cx, |v, _| v.visible().len()), 0);
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |v, _| v.visible().len()), 1);

    let id = view.read_with(cx, |v, _| v.vms[0].id);
    view.update(cx, |v, cx| v.remove(id, cx));
    wait(cx, &view, "the VM to be removed", |v| v.vms.is_empty());
    assert_eq!(
        view.read_with(cx, |v, _| v.message.clone()),
        Some(("removed lab1".to_owned(), false))
    );
}

#[gpui::test]
fn password_field_keeps_text_and_jump_host_is_saved(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let env = env();
    let backend = env.backend.clone();
    let (view, cx) = cx.add_window_view(|_, cx| AppView::new(backend, None, cx));

    // A bastion to jump through.
    view.update_in(cx, AppView::open_add_form);
    cx.simulate_keystrokes("b a s t i o n tab 1 2 7 . 0 . 0 . 1 enter");
    wait(cx, &view, "the bastion", |v| {
        v.vms.len() == 1 && v.add.is_none()
    });
    let bastion = view.read_with(cx, |v, _| v.vms[0].id);

    view.update_in(cx, |v, window, cx| {
        v.open_add_form(window, cx);
        let form = v.add.as_mut().unwrap();
        form.via = Some(bastion);
        form.auth = crate::form::AuthKind::KeyFile;
    });
    // Tab order now ends in the key file field.
    cx.simulate_keystrokes("i n n e r tab 1 0 . 9 . 9 . 9 tab tab tab / n o / s u c h");
    view.read_with(cx, |v, cx| {
        let spec = v.add.as_ref().unwrap().spec(cx).unwrap();
        assert_eq!(spec.via, Some(bastion));
        assert!(
            matches!(spec.auth, crate::backend::Auth::KeyFile(ref p) if p.as_os_str() == "/no/such")
        );
    });
    cx.simulate_keystrokes("enter");
    wait(cx, &view, "the missing key file error", |v| {
        v.add
            .as_ref()
            .is_some_and(|f| !f.saving && f.error.is_some())
    });
    assert!(form_error(cx, &view).unwrap().contains("no key file"));
    assert_eq!(
        view.read_with(cx, |v, _| v.vms.len()),
        1,
        "nothing was added"
    );
}

#[gpui::test]
fn edit_prefills_and_only_sends_what_changed(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let env = env();
    let backend = env.backend.clone();
    let (view, cx) = cx.add_window_view(|_, cx| AppView::new(backend, None, cx));

    view.update_in(cx, AppView::open_add_form);
    cx.simulate_keystrokes("b o x tab 1 2 7 . 0 . 0 . 1 tab 1 tab r o o t enter");
    wait(cx, &view, "the VM", |v| v.vms.len() == 1 && v.add.is_none());
    let id = view.read_with(cx, |v, _| v.vms[0].id);

    // The form starts from the VM's values.
    view.update_in(cx, |v, window, cx| v.open_edit_form(id, window, cx));
    view.read_with(cx, |v, cx| {
        let spec = v.add.as_ref().unwrap().spec(cx).unwrap();
        assert_eq!(
            (
                spec.name.as_str(),
                spec.address.as_str(),
                spec.port,
                spec.user.as_str()
            ),
            ("box", "127.0.0.1", 1, "root")
        );
    });

    // Change only the user: Tab x3 to it, erase "root", type "admin".
    cx.simulate_keystrokes("tab tab tab backspace backspace backspace backspace a d m i n enter");
    wait(cx, &view, "the edit to be saved", |v| {
        v.add.is_none() && v.vms.first().is_some_and(|vm| vm.username == "admin")
    });
    view.read_with(cx, |v, _| {
        let vm = &v.vms[0];
        assert_eq!(vm.endpoints.len(), 1, "endpoint not re-sent");
        assert_eq!(
            (vm.endpoints[0].address.as_str(), vm.endpoints[0].port),
            ("127.0.0.1", 1)
        );
        assert_eq!(v.selected, Some(id));
    });

    // Changing the port replaces the endpoint.
    view.update_in(cx, |v, window, cx| v.open_edit_form(id, window, cx));
    cx.simulate_keystrokes("tab tab backspace 2 enter");
    wait(cx, &view, "the new port", |v| {
        v.add.is_none() && v.vms.first().is_some_and(|vm| vm.endpoints[0].port == 2)
    });
}
