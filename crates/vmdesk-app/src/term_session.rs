//! The live terminal behind one embedded terminal tab.
//!
//! A task on the tokio runtime owns the SSH pty. It reads output into a libghostty-vt
//! [`Terminal`] that the GPUI view locks to draw, and writes the bytes the view encodes
//! back to the pty. Parsing a flood of output can take a whole core, so it never runs on
//! the UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use vmdesk_core::VmId;
use vmdesk_db::Db;
use vmdesk_terminal::Terminal;

use crate::backend::Backend;

/// The terminal a view and its pump share. The view locks it briefly to draw and to apply
/// input; the pump locks it while it parses output.
pub type SharedTerm = Arc<Mutex<Terminal>>;

/// What the view needs to hear about. Output itself goes straight into the shared grid.
pub enum TermEvent {
    /// The grid changed. Sent at most once until the view takes it.
    Redraw,
    /// `None` once the shell is live; otherwise what's happening, such as "Connecting…".
    Status(Option<String>),
    /// The session is over.
    Closed(Ended),
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The shell exited on its own, as after `exit` or Ctrl+D: its pane closes.
    Exited,
    /// The connection failed or dropped; the string is why, for the user.
    Lost(String),
}

/// What the view sends to the pty.
pub enum Input {
    Bytes(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    },
}

/// A running shell. Dropping it stops the pump and closes the pty.
pub struct TerminalSession {
    pub events: async_channel::Receiver<TermEvent>,
    /// Cleared by the view once it has redrawn, so the next change sends `Redraw` again.
    pub dirty: Arc<AtomicBool>,
    input: async_channel::Sender<Input>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TerminalSession {
    /// Starts connecting in the background. `term` must be the terminal the view draws.
    pub fn start(backend: &Backend, vm: VmId, term: &SharedTerm) -> Self {
        let (events_tx, events) = async_channel::unbounded();
        let (input, input_rx) = async_channel::unbounded();
        let dirty = Arc::new(AtomicBool::new(false));
        let task = backend.handle().spawn(run(
            backend.db.clone(),
            vm,
            term.clone(),
            events_tx,
            input_rx,
            dirty.clone(),
        ));
        Self {
            events,
            dirty,
            input,
            task,
        }
    }

    /// Sends already-encoded input bytes to the pty.
    pub fn send(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.input.try_send(Input::Bytes(bytes));
        }
    }

    /// Tells the pty its new size, and resizes the local grid to match.
    pub fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        let _ = self.input.try_send(Input::Resize {
            cols,
            rows,
            cell_width,
            cell_height,
        });
    }
}

async fn run(
    db: Db,
    vm: VmId,
    term: SharedTerm,
    events: async_channel::Sender<TermEvent>,
    input: async_channel::Receiver<Input>,
    dirty: Arc<AtomicBool>,
) {
    let _ = events.try_send(TermEvent::Status(Some("Connecting…".into())));
    let (cols, rows) = {
        let terminal = term.lock().unwrap();
        (terminal.cols(), terminal.rows())
    };
    let (target, transport, session) = match crate::backend::connect(&db, vm).await {
        Ok(parts) => parts,
        Err(e) => {
            let _ = events.try_send(TermEvent::Status(Some("Not connected".into())));
            let _ = events.try_send(TermEvent::Closed(Ended::Lost(format!("{e:#}"))));
            return;
        }
    };
    let shell = match session.open_shell(cols, rows).await {
        Ok(shell) => shell,
        Err(e) => {
            transport.close_all().await;
            let _ = events.try_send(TermEvent::Status(Some("Not connected".into())));
            let _ = events.try_send(TermEvent::Closed(Ended::Lost(format!("{e:#}"))));
            return;
        }
    };
    let (mut reader, writer) = shell.split();
    let mut ended = None;
    let mut buf = vec![0u8; 64 * 1024];
    // The pty is live; the view can stop showing "Connecting…".
    if events.try_send(TermEvent::Status(None)).is_err() {
        writer.close().await;
        transport.close_all().await;
        return;
    }
    tracing::info!(vm = %target.name, "terminal shell opened");

    loop {
        tokio::select! {
            message = input.recv() => match message {
                Ok(Input::Bytes(bytes)) => {
                    if let Err(e) = writer.write(&bytes).await {
                        ended = Some(Ended::Lost(format!("{e:#}")));
                        break;
                    }
                }
                Ok(Input::Resize { cols, rows, cell_width, cell_height }) => {
                    term.lock().unwrap().resize(cols, rows, u32::from(cell_width), u32::from(cell_height));
                    if let Err(e) = writer.resize(cols, rows, 0, 0).await {
                        ended = Some(Ended::Lost(format!("{e:#}")));
                        break;
                    }
                }
                // The view went away, so there is nothing left to show.
                Err(_) => break,
            },
            read = reader.read(&mut buf) => match read {
                Ok(0) => {
                    ended = Some(Ended::Exited);
                    break;
                }
                Ok(n) => {
                    term.lock().unwrap().write(&buf[..n]);
                    if !changed(&dirty, &events) {
                        break;
                    }
                }
                Err(e) => {
                    ended = Some(Ended::Lost(format!("{e:#}")));
                    break;
                }
            },
        }
    }

    let ended = ended.unwrap_or_else(|| Ended::Lost("disconnected".to_owned()));
    tracing::info!(vm = %target.name, ?ended, "terminal shell ended");
    writer.close().await;
    transport.close_all().await;
    let _ = events.try_send(TermEvent::Closed(ended));
}

/// Tells the view to redraw, at most once until it takes the previous request.
fn changed(dirty: &AtomicBool, events: &async_channel::Sender<TermEvent>) -> bool {
    if dirty.swap(true, Ordering::AcqRel) {
        return !events.is_closed();
    }
    events.try_send(TermEvent::Redraw).is_ok()
}
