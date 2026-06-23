//! herdr-review — a herdr-native review sidebar.
//!
//! Browse an agent's changes (uncommitted / branch / last-turn), leave comments,
//! and send them back to the agent ("Add all to chat") — entirely in a herdr pane.
//!
//! This crate is split into a thin binary ([`crate`] consumed by `src/main.rs`)
//! and this library so the logic stays unit-testable. The TUI is built with
//! ratatui using a component architecture; modules are introduced milestone by
//! milestone (see `docs/` and the planning docs).

use anyhow::Result;

/// Entry point for the herdr-review sidebar.
///
/// Currently a scaffold; the interactive TUI lands in later milestones.
pub fn run() -> Result<()> {
    println!("herdr-review {} — scaffold (not yet interactive)", env!("CARGO_PKG_VERSION"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn run_scaffold_is_ok() {
        assert!(run().is_ok());
    }
}
