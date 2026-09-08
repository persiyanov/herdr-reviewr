fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();

    // Recognized anywhere in argv, matching the pane-action core's identity read: a process
    // invoked with either flag never counts as the review UI, so it must never run the
    // review UI either. This dispatch and `pane_action::is_reviewr_pane`'s flag exclusion are
    // the two halves of that contract — a future non-UI flag must land in both, or the pane
    // actions will count its transient process as a live reviewr pane.
    if args.iter().any(|arg| arg == "--resolve-plugin-config") {
        if let Err(error) = herdr_reviewr::config::print_plugin_config() {
            eprintln!("reviewr: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(index) = args.iter().position(|arg| arg == herdr_reviewr::pane_action::FLAG) {
        let mode = args.get(index + 1).and_then(|arg| arg.to_str()).unwrap_or("toggle");
        herdr_reviewr::pane_action::run(mode);
    }
    herdr_reviewr::run()
}
