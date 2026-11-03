//! An interactive shell on a VM, over an SSH pty.
//!
//! The channel is split so a terminal can read output and write input concurrently: the
//! reader is the pty's output, the writer sends keystrokes and window-size changes.

use russh::ChannelReadHalf;
use russh::ChannelWriteHalf;
use russh::client::Msg;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use vmdesk_core::{Error, Result};

/// The name of the VM, for error messages.
pub struct Shell {
    name: String,
    reader: ChannelReadHalf,
    writer: ChannelWriteHalf<Msg>,
}

impl Shell {
    pub(crate) fn new(
        name: String,
        reader: ChannelReadHalf,
        writer: ChannelWriteHalf<Msg>,
    ) -> Self {
        Self {
            name,
            reader,
            writer,
        }
    }

    /// The read and write halves, to be used from separate tasks or `select!` branches.
    #[must_use]
    pub fn split(self) -> (ShellReader, ShellWriter) {
        (
            ShellReader {
                name: self.name.clone(),
                reader: self.reader,
            },
            ShellWriter {
                name: self.name,
                writer: self.writer,
            },
        )
    }
}

/// The pty's output.
pub struct ShellReader {
    name: String,
    reader: ChannelReadHalf,
}

impl ShellReader {
    /// Reads output. `0` means the shell closed.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut reader = self.reader.make_reader();
        reader
            .read(buf)
            .await
            .map_err(|e| Error::Connect(format!("{}: {e}", self.name)))
    }
}

/// The pty's input and window size.
pub struct ShellWriter {
    name: String,
    writer: ChannelWriteHalf<Msg>,
}

impl ShellWriter {
    /// Writes bytes as keystrokes or paste. Binary safe.
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.make_writer();
        writer
            .write_all(data)
            .await
            .map_err(|e| Error::Connect(format!("{}: {e}", self.name)))?;
        writer
            .flush()
            .await
            .map_err(|e| Error::Connect(format!("{}: {e}", self.name)))
    }

    /// Tells the remote pty its new size, in cells and pixels.
    pub async fn resize(
        &self,
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<()> {
        self.writer
            .window_change(
                u32::from(cols),
                u32::from(rows),
                u32::from(pixel_width),
                u32::from(pixel_height),
            )
            .await
            .map_err(|e| Error::Connect(format!("{}: {e}", self.name)))
    }

    /// Ends the shell. Errors are ignored: the connection may already be gone.
    pub async fn close(&self) {
        let _ = self.writer.eof().await;
        let _ = self.writer.close().await;
    }
}
