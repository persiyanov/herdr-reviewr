//! A live resolution probe against a real worktree and real `gh`, for manual
//! verification only — never part of the suite. Run it as:
//!
//! ```sh
//! REVIEWR_LIVE_REPO=/path/to/worktree cargo test --test pr_live -- --ignored --nocapture
//! ```

use herdr_reviewr::config::PluginConfig;
use herdr_reviewr::forge::{fetch, fetch_input};

#[test]
#[ignore = "live network; run with REVIEWR_LIVE_REPO set"]
fn resolve_one_live_worktree() {
    let repo = std::env::var("REVIEWR_LIVE_REPO").expect("set REVIEWR_LIVE_REPO");
    let repo = std::path::PathBuf::from(repo);
    let input = fetch_input(&repo, None, &PluginConfig::default()).expect("fetch input");
    eprintln!("input: {input:#?}");
    let view = fetch(&repo, &input);
    eprintln!("view: {view:#?}");
}

#[test]
#[ignore = "live network; run with REVIEWR_LIVE_REPO set"]
fn resolve_one_live_bitbucket_worktree() {
    let repo = std::env::var("REVIEWR_LIVE_REPO").expect("set REVIEWR_LIVE_REPO");
    let repo = std::path::PathBuf::from(repo);
    let config_dir =
        std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/herdr/reviewr");
    let config = herdr_reviewr::config::plugin_config(Some(&config_dir)).expect("load config");
    let input = fetch_input(&repo, None, &config).expect("fetch input");
    eprintln!("input: {input:#?}");
    let view = fetch(&repo, &input);
    eprintln!("view: {view:#?}");
}
