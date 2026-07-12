fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("--resolve-plugin-config") {
        if let Err(error) = herdr_reviewr::config::print_plugin_config() {
            eprintln!("reviewr: {error}");
            return std::process::ExitCode::FAILURE;
        }
        return std::process::ExitCode::SUCCESS;
    }

    if matches!(args.get(1).map(String::as_str), Some("comment" | "skill-path" | "skill-install")) {
        return herdr_reviewr::cli::run(args);
    }

    match herdr_reviewr::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reviewr: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
