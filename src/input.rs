//! The event loop's sole terminal-input owner: raw VT bytes on Unix and console VT on Windows.

#[cfg(not(windows))]
mod unix;
#[cfg_attr(not(windows), allow(dead_code))]
mod vt;
#[cfg(windows)]
mod windows;

use std::io;

use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::execute;

#[cfg(not(windows))]
pub(crate) use unix::{poll, read, wait};

#[cfg(not(windows))]
pub(crate) use unix::{protocol_stalled, take_graphics_reply};

#[cfg(windows)]
pub(crate) fn take_graphics_reply() -> Option<Vec<u8>> {
    None
}
#[cfg(windows)]
pub(crate) use windows::{poll, read, wait};

/// Mouse capture, bracketed paste, and the kitty protocol where the terminal has it.
pub(crate) fn claim() {
    let _ = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    #[cfg(not(windows))]
    unix::claim();
    // Last, since the mouse capture above rewrites the console mode it builds on.
    #[cfg(windows)]
    windows::claim();
}

/// Release what [`claim`] claimed, in reverse.
pub(crate) fn release() {
    #[cfg(windows)]
    windows::release();
    #[cfg(not(windows))]
    unix::release();
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
}

#[cfg(windows)]
pub(crate) fn protocol_stalled() -> bool {
    false
}
