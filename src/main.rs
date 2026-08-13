fn main() -> anyhow::Result<()> {
    // Recognized anywhere in argv, matching the actions' pane-identity read: a process
    // invoked with this flag never counts as the review UI, so it must never run the
    // review UI either (`specs/herdr-host.md` Pane identity). This dispatch and the jq
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
    // `nav` is the other non-UI invocation, recognized as the first argument only — a repo
    // path happening to be named `nav` still opens the UI when it arrives behind a flag.
    // The same first-argument read excludes it in `is_reviewr_pane` (specs/nav.md).
    if std::env::args().nth(1).as_deref() == Some("nav") {
        let args: Vec<String> = std::env::args().skip(2).collect();
        if let Err(error) = herdr_reviewr::nav_main(&args) {
            eprintln!("reviewr: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    herdr_reviewr::run()
}
