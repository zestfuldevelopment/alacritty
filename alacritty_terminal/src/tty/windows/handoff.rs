//! A console session another console host started, handed to this terminal
//! (zestful addition).
//!
//! When a terminal is registered as Windows' default terminal, a console
//! program started with a console of its own (a double-clicked script, or
//! `Start-Process -Verb RunAs`) is not given a window. Its console host
//! (OpenConsole) calls the terminal's `ITerminalHandoff3::EstablishPtyHandoff`,
//! and from then on the session is a pseudoconsole this process did not
//! create: the child is already running, and there is no `HPCON` to resize or
//! close.
//!
//! What there is instead is what an `HPCON` wraps. `ConptyPackPseudoConsole`
//! in Microsoft's `winconpty.cpp` builds one from exactly these three handles,
//! and its `ResizePseudoConsole` and `ClosePseudoConsole` do nothing but what
//! [`HandedOff`] does below. So this module speaks the signal pipe itself
//! rather than depending on `conpty.dll`, which is not on every machine and
//! which a stray copy on `PATH` can replace.

use std::fs::File;
use std::io::{self, Write};
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};

use log::warn;
use miow::pipe::{AnonRead, AnonWrite};

use crate::event::{OnResize, WindowSize};
use crate::tty::windows::Pty;
use crate::tty::windows::blocking::{UnblockedReader, UnblockedWriter};
use crate::tty::windows::child::ChildExitWatcher;

const PIPE_CAPACITY: usize = crate::event_loop::READ_BUFFER_SIZE;

/// `PtySignal::ResizeWindow` in microsoft/terminal
/// `src/host/PtySignalInputThread.hpp`.
const PTY_SIGNAL_RESIZE_WINDOW: u16 = 8;

/// Everything a handoff delivers, as this process's own handles.
///
/// `conin` and `conout` are *this* side of the two pipes the terminal created
/// while answering the handoff; the console host was given the other ends.
/// The other four arrive as `[in]` parameters of `EstablishPtyHandoff`, which
/// the COM runtime closes when the call returns, so the caller must pass
/// **duplicates** here (Windows Terminal's `ConptyConnection` duplicates all
/// four for the same reason).
pub struct Handoff {
    /// Write end: bytes written here are the console's input.
    pub conin: OwnedHandle,
    /// Read end: the console's output, as VT.
    pub conout: OwnedHandle,
    /// The console host's signal pipe: resize, and close by closing it.
    pub signal: OwnedHandle,
    /// Keeps the console alive while held.
    pub reference: OwnedHandle,
    /// The console host process.
    pub server: OwnedHandle,
    /// The program the console was created for. Needs `SYNCHRONIZE` and
    /// `PROCESS_QUERY_LIMITED_INFORMATION` for the exit watcher.
    pub client: OwnedHandle,
}

/// The backend of a handed-off session: the handles an `HPCON` would wrap.
///
/// Dropping it closes all three, which is `ClosePseudoConsole` without the
/// wait: the console host sees its signal pipe break and its reference go, and
/// exits once the child has.
pub struct HandedOff {
    signal: File,
    _reference: OwnedHandle,
    _server: OwnedHandle,
}

impl HandedOff {
    fn resize(&mut self, cols: u16, lines: u16) -> io::Result<()> {
        let mut packet = [0u8; 6];
        packet[0..2].copy_from_slice(&PTY_SIGNAL_RESIZE_WINDOW.to_le_bytes());
        packet[2..4].copy_from_slice(&cols.to_le_bytes());
        packet[4..6].copy_from_slice(&lines.to_le_bytes());
        self.signal.write_all(&packet)
    }
}

impl OnResize for HandedOff {
    fn on_resize(&mut self, window_size: WindowSize) {
        // Not an assert, unlike `Conpty`'s: a console host that has already
        // gone makes this fail, and that is the child exiting, which the exit
        // watcher reports on its own.
        if let Err(err) = self.resize(window_size.num_cols, window_size.num_lines) {
            warn!("Could not resize a handed-off console: {err}");
        }
    }
}

/// Wrap a handed-off session as a [`Pty`], sized to `window_size`.
///
/// The console host starts at whatever size the program asked for, so the
/// first thing this does is tell it the pane's size.
pub fn from_handoff(handoff: Handoff, window_size: WindowSize) -> io::Result<Pty> {
    let Handoff { conin, conout, signal, reference, server, client } = handoff;

    // The watcher keeps the raw handle for the life of the pane, as it does
    // for a spawned child's `hProcess`.
    let child_watcher = ChildExitWatcher::new(client.into_raw_handle() as _)?;

    let mut backend =
        HandedOff { signal: File::from(signal), _reference: reference, _server: server };
    backend.on_resize(window_size);

    // SAFETY: both are owned handles to the ends of anonymous pipes, moved in.
    let conin = unsafe { AnonWrite::from_raw_handle(conin.into_raw_handle()) };
    let conout = unsafe { AnonRead::from_raw_handle(conout.into_raw_handle()) };

    Ok(Pty::new(
        backend,
        UnblockedReader::new(conout, PIPE_CAPACITY),
        UnblockedWriter::new(conin, PIPE_CAPACITY),
        child_watcher,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The resize packet is the three little-endian u16s `winconpty.cpp`'s
    /// `_ResizePseudoConsole` writes, in that order. A console host reads
    /// anything else as a different signal, or as garbage.
    #[test]
    fn resize_writes_the_packet_conpty_writes() {
        let (mut read, write) = std::io::pipe().unwrap();
        let write: OwnedHandle = write.into();
        let mut backend = HandedOff {
            signal: File::from(write),
            _reference: File::open("NUL").unwrap().into(),
            _server: File::open("NUL").unwrap().into(),
        };

        backend.on_resize(WindowSize {
            num_lines: 40,
            num_cols: 132,
            cell_width: 1,
            cell_height: 1,
        });

        let mut packet = [0u8; 6];
        read.read_exact(&mut packet).unwrap();
        assert_eq!(packet, [8, 0, 132, 0, 40, 0]);
    }
}
