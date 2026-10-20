//! Headless window tests: a real `AppView` in GPUI's test platform, driven with simulated
//! keystrokes, on an in-memory database. Nothing is drawn on screen.

use std::time::{Duration, Instant};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use vmdesk_core::{AgentState, CredentialRef, EndpointKind, VmId, VmStatus};
use vmdesk_db::{AgentRecord, EndpointInput, NewVm};

use super::*;

/// Keeps the tokio runtime alive for the test's duration.
struct Env {
    rt: tokio::runtime::Runtime,
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
    Env { rt, backend }
}

/// VM `lab` on 127.0.0.1:1, which the last check found online and whose agent state is
/// `agent`. With `version`, it also has an agent row, last synced 130 s ago. Collecting
/// it fails fast (port 1 refuses), so opening it changes nothing stored.
fn seed_vm(env: &Env, agent: AgentState, version: Option<&str>) -> VmId {
    let db = env.backend.db.clone();
    env.rt.block_on(async {
        let vm = db
            .create_vm(NewVm {
                name: "lab".into(),
                username: "root".into(),
                credential: CredentialRef::SshAgent,
                endpoints: vec![EndpointInput {
                    address: "127.0.0.1".into(),
                    port: 1,
                    kind: EndpointKind::default(),
                    bind_interface: None,
                }],
                paths: None,
                tags: vec![],
                favourite: false,
            })
            .await
            .unwrap();
        db.save_status(&VmStatus {
            vm: vm.id,
            route: None,
            ssh: vmdesk_core::SshState::Ok,
            agent,
            checked_at: Utc::now(),
            detail: None,
        })
        .await
        .unwrap();
        if let Some(version) = version {
            db.save_agent(
                &AgentRecord {
                    vm: vm.id,
                    agent_id: uuid::Uuid::now_v7(),
                    version: version.into(),
                    proto: 1,
                    installed_at: None,
                    last_seq: 0,
                    last_contact_at: Some(Utc::now() - chrono::Duration::seconds(130)),
                },
                None,
            )
            .await
            .unwrap();
        }
        vm.id
    })
}

/// A window with `lab` open, once the collect that opening starts has given up.
fn open_lab<'a>(
    cx: &'a mut TestAppContext,
    env: &Env,
) -> (Entity<AppView>, &'a mut VisualTestContext) {
    cx.executor().allow_parking();
    let backend = env.backend.clone();
    let (view, cx) = cx.add_window_view(|_, cx| AppView::new(backend, Some("lab".into()), cx));
    view.update(cx, AppView::reload);
    wait(cx, &view, "lab's detail", |v| {
        v.selected.is_some() && v.detail.is_some() && v.busy.is_none()
    });
    (view, cx)
}

fn shown(cx: &mut VisualTestContext, selector: &'static str) -> bool {
    cx.run_until_parked();
    cx.debug_bounds(selector).is_some()
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is not on screen"));
    cx.simulate_click(bounds.center(), Modifiers::none());
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

#[gpui::test]
fn a_vm_without_an_agent_offers_to_install_one(cx: &mut TestAppContext) {
    let env = env();
    seed_vm(&env, AgentState::Unknown, None);
    let (view, cx) = open_lab(cx, &env);

    assert!(shown(cx, "agent: No agent"));
    assert!(shown(cx, "lab: online"), "no agent, so no `+ agent`");
    assert!(shown(cx, "headline: online"));

    click(cx, "more");
    assert!(shown(cx, "agent-install"));
    assert!(!shown(cx, "agent-remove"));

    click(cx, "agent-install");
    assert!(!shown(cx, "menu"), "choosing an action closes the menu");
    wait(cx, &view, "the install to fail", |v| {
        v.busy.is_none()
            && v.message
                .as_ref()
                .is_some_and(|(m, _)| m.starts_with("Install"))
    });
    let (message, err) = view.read_with(cx, |v, _| v.message.clone().unwrap());
    assert!(
        err && message.starts_with("Install agent failed: "),
        "{message}"
    );
    click(cx, "more");
    assert!(shown(cx, "agent-install"));
}

#[gpui::test]
fn a_collecting_agent_shows_online_plus_agent_and_asks_before_removal(cx: &mut TestAppContext) {
    let env = env();
    seed_vm(&env, AgentState::Ok, Some("0.1.0"));
    let (_view, cx) = open_lab(cx, &env);

    assert!(shown(cx, "agent: Agent 0.1.0 · recording · synced 2m ago"));
    assert!(shown(cx, "lab: online + agent"));
    assert!(shown(cx, "headline: online + agent"));

    click(cx, "more");
    assert!(!shown(cx, "agent-install") && !shown(cx, "agent-upgrade"));
    click(cx, "agent-remove");
    assert!(shown(cx, "agent-remove-panel"), "asks first");
    assert!(shown(cx, "agent-remove-yes"));
    click(cx, "more");
    assert!(
        shown(cx, "menu") && !shown(cx, "agent-remove"),
        "one way to remove at a time"
    );
    cx.simulate_keystrokes("escape");

    click(cx, "agent-remove-no");
    assert!(!shown(cx, "agent-remove-panel"));
    click(cx, "more");
    assert!(shown(cx, "agent-remove"));
    assert!(
        shown(cx, "agent: Agent 0.1.0 · recording · synced 2m ago"),
        "nothing removed"
    );
}

#[gpui::test]
fn an_outdated_agent_offers_an_upgrade(cx: &mut TestAppContext) {
    let env = env();
    seed_vm(&env, AgentState::Outdated, Some("0.0.9"));
    let (_view, cx) = open_lab(cx, &env);

    assert!(shown(cx, "agent: Agent 0.0.9 · outdated · synced 2m ago"));
    assert!(
        shown(cx, "lab: online + agent"),
        "an outdated agent still collects"
    );
    click(cx, "more");
    assert!(shown(cx, "agent-upgrade"));
    assert!(!shown(cx, "agent-remove"));
}

#[gpui::test]
fn the_more_menu_opens_and_closes(cx: &mut TestAppContext) {
    let env = env();
    seed_vm(&env, AgentState::Unknown, None);
    let (view, cx) = open_lab(cx, &env);
    view.update_in(cx, |v, window, cx| {
        let focus = v.focus.clone();
        window.focus(&focus, cx);
    });

    assert!(!shown(cx, "menu"));
    click(cx, "more");
    assert!(shown(cx, "menu") && shown(cx, "edit") && shown(cx, "remove"));

    cx.simulate_keystrokes("x");
    cx.simulate_keystrokes("escape");
    assert!(!shown(cx, "menu"), "Escape closes the menu");
    assert_eq!(
        view.read_with(cx, |v, _| v.filter.clone()),
        "x",
        "and leaves the filter alone"
    );
    cx.simulate_keystrokes("escape");

    click(cx, "more");
    let outside = cx.debug_bounds("tab-timeline").unwrap().center();
    cx.simulate_click(outside, Modifiers::none());
    assert!(!shown(cx, "menu"), "a click outside closes it");
    assert!(
        view.read_with(cx, |v, _| v.tab == Tab::Overview),
        "and does not reach what is under it"
    );

    click(cx, "more");
    click(cx, "more");
    assert!(!shown(cx, "menu"), "the button toggles it");

    click(cx, "more");
    click(cx, "remove");
    assert!(!shown(cx, "menu") && shown(cx, "remove-yes"), "asks first");
    click(cx, "remove-no");
    assert!(!shown(cx, "remove-yes"));

    click(cx, "more");
    click(cx, "edit");
    assert!(
        view.read_with(cx, |v, _| v
            .add
            .as_ref()
            .is_some_and(|f| f.editing.is_some())),
        "Edit opens the form filled in"
    );
}

#[gpui::test]
fn the_timeline_reads_as_sentences_under_day_headers(cx: &mut TestAppContext) {
    let env = env();
    let vm = seed_vm(&env, AgentState::Unknown, None);
    let db = env.backend.db.clone();
    let from = Utc::now() - chrono::Duration::days(1);
    let to = from + chrono::Duration::minutes(5 * 60 + 32);
    env.rt.block_on(async {
        db.set_agent_state(vm, AgentState::Ok).await.unwrap();
        let snapshot = vmdesk_proto::Snapshot {
            taken_at: Some(Utc::now()),
            ..vmdesk_proto::Snapshot::default()
        };
        let events = [(from, vmdesk_proto::EventKind::Gap { from, to })];
        db.record_snapshot(vm, "ssh", &snapshot, &events)
            .await
            .unwrap();
    });
    let (view, cx) = open_lab(cx, &env);

    click(cx, "tab-timeline");
    assert!(shown(cx, "day: Today"));
    assert!(shown(cx, "event: Agent connected and recording"));
    assert!(shown(cx, "day: Yesterday"));
    assert!(shown(cx, "event: Not observed for 5h 32m"));

    let gap = view.read_with(cx, |v, _| {
        let days = crate::timeline_view::present(
            &v.detail.as_ref().unwrap().timeline,
            chrono::Local::now(),
        );
        days.into_iter()
            .flat_map(|d| d.rows)
            .find(|r| crate::timeline_view::text_of(&r.title).starts_with("Not observed"))
            .and_then(|r| r.detail)
            .map(|d| crate::timeline_view::text_of(&d))
            .unwrap()
    });
    let local =
        |t: chrono::DateTime<Utc>| t.with_timezone(&chrono::Local).format("%H:%M").to_string();
    assert!(
        gap.starts_with(&local(from))
            && gap.ends_with(&local(to))
            && !gap.contains(&from.format("%Y-%m-%d").to_string()),
        "local times, no ISO timestamp: {gap}"
    );
}

/// A VM with nothing else stored.
fn add_vm(env: &Env, name: &str) -> VmId {
    let db = env.backend.db.clone();
    env.rt.block_on(async {
        db.create_vm(NewVm {
            name: name.into(),
            username: "root".into(),
            credential: CredentialRef::SshAgent,
            endpoints: vec![EndpointInput {
                address: "127.0.0.1".into(),
                port: 1,
                kind: EndpointKind::default(),
                bind_interface: None,
            }],
            paths: None,
            tags: vec![],
            favourite: false,
        })
        .await
        .unwrap()
        .id
    })
}

/// `pytest tests/group2` running in the attached tmux session `nightly`, in the repo
/// `/root/regal` on `group2-fix` with 3 modified files; started 50 minutes ago, which
/// starts the VM's work session.
fn seed_work(env: &Env, vm: VmId) {
    use vmdesk_proto::{EventKind, ProcessInfo, RepoInfo, Snapshot, TmuxSession};
    let now = Utc::now();
    let process = |pid, ppid, cmdline: &str, cwd: &str, ago: i64| ProcessInfo {
        pid,
        ppid,
        user: "root".into(),
        cmdline: cmdline.into(),
        cwd: Some(cwd.into()),
        started_at: now.timestamp() - ago,
        cpu_pct: 0.,
        rss_kb: 0,
    };
    let snapshot = Snapshot {
        taken_at: Some(now),
        processes: vec![
            process(100, 1, "tmux new -s nightly", "/root", 9000),
            process(101, 100, "-bash", "/root/regal", 9000),
            process(102, 101, "pytest tests/group2", "/root/regal", 3000),
        ],
        tmux: vec![TmuxSession {
            name: "nightly".into(),
            windows: 1,
            attached: true,
            created: now.timestamp() - 9000,
            pane_cwd: Some("/root/regal".into()),
            pane_command: Some("pytest".into()),
        }],
        repos: vec![RepoInfo {
            path: "/root/regal".into(),
            branch: Some("group2-fix".into()),
            dirty: 3,
            ahead: None,
            behind: None,
            last_commit: None,
        }],
        ..Snapshot::default()
    };
    let events = [
        (
            now - chrono::Duration::minutes(50),
            EventKind::ProcessStarted {
                pid: 102,
                cmdline: "pytest tests/group2".into(),
                cwd: Some("/root/regal".into()),
            },
        ),
        (
            now - chrono::Duration::minutes(10),
            EventKind::TmuxAttached {
                name: "nightly".into(),
            },
        ),
    ];
    let db = env.backend.db.clone();
    env.rt.block_on(async {
        db.record_snapshot(vm, "ssh", &snapshot, &events)
            .await
            .unwrap();
    });
}

fn sessions_of(env: &Env, vm: VmId) -> Vec<vmdesk_db::WorkSession> {
    let db = env.backend.db.clone();
    env.rt
        .block_on(async { db.sessions(vm, 10).await.unwrap() })
}

fn notes_of(env: &Env, vm: VmId) -> Vec<vmdesk_db::Note> {
    let db = env.backend.db.clone();
    env.rt.block_on(async { db.notes(vm).await.unwrap() })
}

/// A selector built at run time.
fn dynamic(selector: String) -> &'static str {
    Box::leak(selector.into_boxed_str())
}

fn search_state(cx: &mut VisualTestContext, view: &Entity<AppView>) -> String {
    view.read_with(cx, |v, _| {
        format!("{:?}", v.search.as_ref().map(|s| &s.state))
    })
}

#[gpui::test]
fn the_card_and_the_list_say_what_you_were_doing(cx: &mut TestAppContext) {
    let env = env();
    let vm = seed_vm(&env, AgentState::Unknown, None);
    seed_work(&env, vm);
    let session = sessions_of(&env, vm)[0].id;
    env.rt.block_on(async {
        env.backend
            .db
            .add_note(vm, Some(session), "waiting on CI")
            .await
            .unwrap();
    });
    let (view, cx) = open_lab(cx, &env);

    assert!(shown(cx, "work-card"), "the card is on the Overview");
    assert!(shown(cx, "work: pytest tests/group2"));
    assert!(shown(cx, "note: waiting on CI"), "the note is on the card");
    assert!(shown(cx, "work-session"), "and the session it belongs to");

    let line = view.read_with(cx, |v, _| v.work.get(&vm).cloned()).unwrap();
    assert!(
        line.starts_with(
            "pytest tests/group2 in regal @ group2-fix (3 modified) · tmux nightly · since "
        ),
        "{line}"
    );
    assert!(
        shown(cx, dynamic(format!("lab work: {line}"))),
        "the list shows it too"
    );
}

#[gpui::test]
fn a_vm_with_nothing_running_says_so_and_the_list_skips_it(cx: &mut TestAppContext) {
    let env = env();
    let vm = seed_vm(&env, AgentState::Unknown, None);
    let db = env.backend.db.clone();
    env.rt.block_on(async {
        let snapshot = vmdesk_proto::Snapshot {
            taken_at: Some(Utc::now()),
            ..vmdesk_proto::Snapshot::default()
        };
        db.record_snapshot(vm, "ssh", &snapshot, &[]).await.unwrap();
    });
    let (view, cx) = open_lab(cx, &env);
    assert!(shown(cx, "work: nothing"));
    assert!(view.read_with(cx, |v, _| v.work.is_empty()));
    assert!(!shown(cx, "work-session"), "no session yet");
}

#[gpui::test]
fn sessions_are_listed_and_a_rename_persists(cx: &mut TestAppContext) {
    let env = env();
    let vm = seed_vm(&env, AgentState::Unknown, None);
    seed_work(&env, vm);
    let (view, cx) = open_lab(cx, &env);

    click(cx, "tab-sessions");
    let started = sessions_of(&env, vm)[0]
        .started_at
        .with_timezone(&chrono::Local);
    let header = if started.date_naive() == chrono::Local::now().date_naive() {
        "day: Today"
    } else {
        "day: Yesterday"
    };
    assert!(shown(cx, header), "under the day it started");
    assert!(shown(cx, "session: Untitled session"));

    click(cx, "session: Untitled session");
    cx.simulate_keystrokes("x escape");
    assert!(
        shown(cx, "session: Untitled session"),
        "Esc leaves the title alone"
    );
    assert_eq!(sessions_of(&env, vm)[0].title, None);

    click(cx, "session: Untitled session");
    cx.simulate_keystrokes("n i g h t l y space d e b u g enter");
    wait(cx, &view, "the new title", |v| {
        v.detail
            .as_ref()
            .is_some_and(|d| d.sessions[0].title.is_some())
    });
    assert_eq!(
        sessions_of(&env, vm)[0].title.as_deref(),
        Some("nightly debug"),
        "stored"
    );
    assert!(shown(cx, "session: nightly debug"));
    assert!(view.read_with(cx, |v, _| v.inline.is_none()));
}

#[gpui::test]
fn a_note_added_on_the_card_lands_on_the_latest_session(cx: &mut TestAppContext) {
    let env = env();
    let vm = seed_vm(&env, AgentState::Unknown, None);
    seed_work(&env, vm);
    let (view, cx) = open_lab(cx, &env);

    click(cx, "add-note");
    assert!(!shown(cx, "add-note"), "the button gives way to the field");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(notes_of(&env, vm).is_empty(), "an empty note is not saved");
    assert_eq!(
        view.read_with(cx, |v, _| v.message.clone()),
        None,
        "nor sent"
    );
    cx.simulate_keystrokes("f l a k y space n 4 space t e s t enter");
    wait(cx, &view, "the note", |v| {
        v.detail.as_ref().is_some_and(|d| d.notes.len() == 1)
    });
    let notes = notes_of(&env, vm);
    assert_eq!(notes[0].body, "flaky n4 test");
    assert_eq!(notes[0].session, Some(sessions_of(&env, vm)[0].id));
    assert!(shown(cx, "note: flaky n4 test"), "on the card");
    assert!(shown(cx, "add-note"));

    click(cx, "tab-sessions");
    assert!(shown(cx, "note: flaky n4 test"), "and in its session");
    click(cx, "delete note: flaky n4 test");
    wait(cx, &view, "the note to go", |v| {
        v.detail.as_ref().is_some_and(|d| d.notes.is_empty())
    });
    assert!(notes_of(&env, vm).is_empty());
    assert!(!shown(cx, "note: flaky n4 test"));
}

#[gpui::test]
fn search_finds_an_event_on_another_vm_and_opens_its_timeline(cx: &mut TestAppContext) {
    let env = env();
    seed_vm(&env, AgentState::Unknown, None);
    let dev93 = add_vm(&env, "dev93");
    let db = env.backend.db.clone();
    let at = Utc::now() - chrono::Duration::days(6);
    env.rt.block_on(async {
        let snapshot = vmdesk_proto::Snapshot {
            taken_at: Some(at),
            ..vmdesk_proto::Snapshot::default()
        };
        let started = vmdesk_proto::EventKind::ProcessStarted {
            pid: 7,
            cmdline: "pytest upf-tests/test_n4.py".into(),
            cwd: Some("/root/regal".into()),
        };
        db.record_snapshot(dev93, "ssh", &snapshot, &[(at, started)])
            .await
            .unwrap();
    });
    let (view, cx) = open_lab(cx, &env);
    view.update_in(cx, |v, window, cx| {
        let focus = v.focus.clone();
        window.focus(&focus, cx);
    });

    cx.simulate_keystrokes("ctrl-k");
    assert!(view.read_with(cx, |v, _| v.search.is_some()));
    cx.simulate_keystrokes("- -");
    cx.executor().advance_clock(DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(
        search_state(cx, &view),
        "Some(Idle)",
        "nothing to search for is quiet"
    );

    cx.simulate_keystrokes("backspace backspace u p f");
    cx.executor().advance_clock(DEBOUNCE / 2);
    cx.simulate_keystrokes("- t e s t");
    cx.executor().advance_clock(DEBOUNCE / 2);
    cx.run_until_parked();
    assert_eq!(
        search_state(cx, &view),
        "Some(Idle)",
        "no query until typing pauses, and each edit restarts the wait"
    );
    cx.executor().advance_clock(DEBOUNCE / 2);
    wait(cx, &view, "the hits", |v| {
        matches!(
            v.search.as_ref().map(|s| &s.state),
            Some(SearchState::Results { .. })
        )
    });
    assert!(shown(cx, "hit: dev93 Started pytest upf-tests/test_n4.py"));

    click(cx, "hit: dev93 Started pytest upf-tests/test_n4.py");
    wait(cx, &view, "dev93's timeline", |v| {
        v.search.is_none()
            && v.selected == Some(dev93)
            && v.tab == Tab::Timeline
            && v.detail.is_some()
    });
    assert!(shown(cx, "event: Started pytest upf-tests/test_n4.py"));

    cx.simulate_keystrokes("ctrl-k z z z q");
    cx.executor().advance_clock(DEBOUNCE);
    wait(cx, &view, "no match", |v| {
        matches!(
            v.search.as_ref().map(|s| &s.state),
            Some(SearchState::Empty { .. })
        )
    });
    cx.simulate_keystrokes("escape");
    assert!(
        view.read_with(cx, |v, _| v.search.is_none()),
        "Esc closes it"
    );
}

#[gpui::test]
fn a_field_that_loses_focus_gives_the_keys_back(cx: &mut TestAppContext) {
    let env = env();
    let vm = seed_vm(&env, AgentState::Unknown, None);
    seed_work(&env, vm);
    let (view, cx) = open_lab(cx, &env);
    let field_text = |cx: &mut VisualTestContext| {
        view.read_with(cx, |v, cx| {
            v.inline
                .as_ref()
                .map(|i| i.field().read(cx).text().to_owned())
        })
    };

    click(cx, "add-note");
    cx.simulate_keystrokes("h a l f");
    click(cx, "work: pytest tests/group2");
    cx.simulate_keystrokes("x enter");
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |v, _| v.filter.clone()),
        "x",
        "typing filters the list again"
    );
    assert_eq!(
        field_text(cx).as_deref(),
        Some("half"),
        "the field keeps its text"
    );
    assert!(
        notes_of(&env, vm).is_empty(),
        "Enter outside it saves nothing"
    );

    cx.simulate_keystrokes("escape");
    assert_eq!(field_text(cx), None, "Esc closes it");
    assert_eq!(view.read_with(cx, |v, _| v.filter.clone()), "x");
}

#[gpui::test]
fn a_rename_the_database_refuses_keeps_its_text(cx: &mut TestAppContext) {
    let env = env();
    let vm = seed_vm(&env, AgentState::Unknown, None);
    seed_work(&env, vm);
    let (view, cx) = open_lab(cx, &env);

    click(cx, "tab-sessions");
    click(cx, "session: Untitled session");
    let too_long = vec!["x"; 201].join(" ");
    cx.simulate_keystrokes(&too_long);
    cx.simulate_keystrokes("enter");
    wait(cx, &view, "the refusal", |v| {
        v.message.as_ref().is_some_and(|(_, err)| *err)
    });
    let kept = view.read_with(cx, |v, cx| {
        v.inline
            .as_ref()
            .map(|i| i.field().read(cx).text().chars().count())
    });
    assert_eq!(kept, Some(201), "the field is back with what was typed");
    assert_eq!(sessions_of(&env, vm)[0].title, None);
}
