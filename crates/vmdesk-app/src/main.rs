//! `vmdesk-app`: the desktop app. One process: the window, the reachability monitor and
//! the snapshot poller share the same database as the `vmdesk` CLI.

mod agent_view;
mod backend;
mod chrome;
mod field;
mod form;
mod gpu;
mod human;
mod icon;
mod kit;
mod overview;
mod search_view;
mod terminal;
mod theme;
mod timeline_view;
mod ui;
mod work_view;

use std::path::PathBuf;

use anyhow::Context as _;
use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px,
    size,
};
use tracing_subscriber::EnvFilter;

use crate::backend::Backend;
use crate::ui::AppView;

/// Same location as the CLI: `VMDESK_DATA_DIR`, else the OS data directory.
fn data_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("VMDESK_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("vmdesk")))
        .context("no data directory; set VMDESK_DATA_DIR")
}

fn main() -> anyhow::Result<()> {
    gpu::trim_drivers();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // The database and SSH code run on tokio, beside GPUI's own executor.
    // Two workers are plenty for checks and polls, which mostly wait on the network;
    // one per core (the default) only adds threads and allocator arenas.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .thread_name("vmdesk-bg")
        .build()?;
    let db_path = dir.join("vmdesk.db");
    let db = rt
        .block_on(vmdesk_db::Db::open(&db_path))
        .with_context(|| format!("opening {}", db_path.display()))?;
    // `vmdesk-app <vm>` opens with that VM (name, alias or id) selected.
    let open_vm = std::env::args().nth(1);
    let backend = Backend::new(rt.handle().clone(), db, dir);
    backend.start_background();

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("VMDesk".into()),
                    ..Default::default()
                }),
                app_id: Some("vmdesk".to_owned()),
                // GNOME does not decorate Wayland windows; the app draws its own frame.
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| {
                    AppView::start(cx);
                    AppView::new(backend, open_vm, cx)
                });
                let focus = view.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
                view
            },
        );
        if let Err(e) = opened {
            tracing::error!(
                error = %e,
                "cannot open window; if this machine has no hardware Vulkan driver, run with {}=1",
                gpu::OPT_OUT
            );
            cx.quit();
        }
        cx.activate(true);
    });
    drop(rt);
    Ok(())
}
