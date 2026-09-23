fn main() -> anyhow::Result<()> {
    // Recognized anywhere in argv, matching the actions' pane-identity read: a process
    // invoked with this flag never counts as the review UI, so it must never run the
    // review UI either. This dispatch and the jq
    // exclusion in `herdr/pane.sh` (`is_reviewr_pane`) are the two halves of that
    // contract — a future non-UI flag must land in both, or the actions will count its
    // transient process as a live reviewr pane.
    if std::env::args_os().skip(1).any(|arg| arg == "--resolve-plugin-config") {
        if let Err(error) = herdr_reviewr::config::print_plugin_config() {
            eprintln!("reviewr: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    herdr_reviewr::run()
}
