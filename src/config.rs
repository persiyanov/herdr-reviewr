//! Command-line configuration: the worktree, poll interval, and base ref.
//!
//! See `specs/tui.md` and `specs/herdr-host.md`. Flags override defaults; the
//! positional argument (if any) is the repo path, else the current directory.

use std::path::PathBuf;
use std::time::Duration;

/// Resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub repo: PathBuf,
    pub poll: Duration,
    pub base: Option<String>,
}

impl Config {
    /// Parse `args` (the process arguments *after* argv\[0\]).
    ///
    /// Recognises `--poll <ms>` (min 200, default 2000) and `--base <ref>`; the
    /// first non-flag token is the repo path.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Self {
        let mut repo: Option<PathBuf> = None;
        let mut poll_ms: u64 = 2000;
        let mut base: Option<String> = None;
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--poll" => {
                    if let Some(v) = it.next() {
                        poll_ms = v.parse().unwrap_or(poll_ms);
                    }
                }
                "--base" => base = it.next(),
                other if !other.starts_with('-') => repo = Some(PathBuf::from(other)),
                _ => {}
            }
        }
        let repo =
            repo.or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("."));
        Self { repo, poll: Duration::from_millis(poll_ms.max(200)), base }
    }

    /// Parse from the real process arguments.
    pub fn from_env() -> Self {
        Self::parse(std::env::args().skip(1))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::time::Duration;

    fn parse(args: &[&str]) -> Config {
        Config::parse(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn defaults_when_no_args() {
        let c = parse(&[]);
        assert_eq!(c.poll, Duration::from_secs(2));
        assert_eq!(c.base, None);
    }

    #[test]
    fn flags_and_positional_repo() {
        let c = parse(&["--poll", "500", "--base", "origin/dev", "/tmp/work"]);
        assert_eq!(c.poll, Duration::from_millis(500));
        assert_eq!(c.base.as_deref(), Some("origin/dev"));
        assert_eq!(c.repo.to_str(), Some("/tmp/work"));
    }

    #[test]
    fn poll_has_a_floor() {
        assert_eq!(parse(&["--poll", "10"]).poll, Duration::from_millis(200));
        assert_eq!(parse(&["--poll", "garbage"]).poll, Duration::from_secs(2));
    }
}
