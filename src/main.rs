fn main() -> anyhow::Result<()> {
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--resolve-plugin-config"))
    {
        if let Err(error) = herdr_reviewr::config::print_plugin_config() {
            eprintln!("reviewr: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    herdr_reviewr::run()
}
