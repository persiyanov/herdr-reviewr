//! Command-line flags and the shared plugin configuration boundary.
//!
//! See `specs/tui.md` and `specs/herdr-host.md`. Flags override defaults; the positional
//! argument (if any) is the repo path, else the current directory.

use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub repo: PathBuf,
    pub poll: Duration,
    pub base: Option<String>,
    pub theme: Option<String>,
    /// `Some(false)` when `--wrap off` is passed; `None` keeps the default (wrap on).
    pub wrap: Option<bool>,
}

impl Config {
    /// Parse `args` (the process arguments *after* argv\[0\]).
    ///
    /// Recognises `--poll <ms>` (min 200, default 2000), `--base <ref>`,
    /// `--theme <name>`, and `--wrap on|off`; the first non-flag token is the repo path.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Self {
        let mut repo: Option<PathBuf> = None;
        let mut poll_ms: u64 = 2000;
        let mut base: Option<String> = None;
        let mut theme: Option<String> = None;
        let mut wrap: Option<bool> = None;
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--poll" => {
                    if let Some(v) = it.next() {
                        poll_ms = v.parse().unwrap_or(poll_ms);
                    }
                }
                "--base" => base = it.next(),
                "--theme" => theme = it.next(),
                "--wrap" => wrap = it.next().map(|v| v != "off"),
                other if !other.starts_with('-') => repo = Some(PathBuf::from(other)),
                _ => {}
            }
        }
        let repo =
            repo.or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("."));
        Self { repo, poll: Duration::from_millis(poll_ms.max(200)), base, theme, wrap }
    }

    /// Parse from the real process arguments.
    pub fn from_env() -> Self {
        Self::parse(std::env::args().skip(1))
    }
}

/// The built-in base-branch candidates for the `branch` scope, used when `config.toml`
/// sets no `base_branches` (`specs/review-model.md`).
pub const DEFAULT_BASE_BRANCHES: [&str; 2] = ["main", "master"];

/// One `base_branches` entry's canonical bare branch name: a leading `refs/heads/`,
/// `refs/remotes/origin/`, or `origin/` prefix is stripped (`specs/config.md`).
pub(crate) fn canonical_base(entry: &str) -> String {
    entry
        .strip_prefix("refs/remotes/origin/")
        .or_else(|| entry.strip_prefix("refs/heads/"))
        .or_else(|| entry.strip_prefix("origin/"))
        .unwrap_or(entry)
        .to_string()
}

const PLUGIN_CONFIG_KEYS: [&str; 9] = [
    "theme",
    "base_branches",
    "default_scope",
    "navigator_position",
    "toggle_placement",
    "toggle_direction",
    "auto_open",
    "github_host",
    "keybindings",
];

/// Where the navigator sits around the read pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavigatorPosition {
    #[default]
    Right,
    Bottom,
    Left,
    Top,
}

impl NavigatorPosition {
    /// The config-file spelling used in normalized output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Top => "top",
        }
    }

    /// The `p` action's clockwise sequence around the read pane.
    #[must_use]
    pub fn clockwise(self) -> Self {
        match self {
            Self::Right => Self::Bottom,
            Self::Bottom => Self::Left,
            Self::Left => Self::Top,
            Self::Top => Self::Right,
        }
    }

    /// Whether this position divides body rows instead of columns.
    pub fn stacked(self) -> bool {
        matches!(self, Self::Top | Self::Bottom)
    }
}

/// Where the toggle action opens the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TogglePlacement {
    Split,
    Overlay,
    Zoomed,
    Tab,
}

impl TogglePlacement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Overlay => "overlay",
            Self::Zoomed => "zoomed",
            Self::Tab => "tab",
        }
    }
}

/// Direction for split placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToggleDirection {
    Right,
    Down,
}

impl ToggleDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }
}

/// One validated snapshot of `$HERDR_PLUGIN_CONFIG_DIR/config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginConfig {
    theme: String,
    base_branches: Vec<String>,
    default_scope: crate::model::Scope,
    navigator_position: NavigatorPosition,
    toggle_placement: TogglePlacement,
    toggle_direction: ToggleDirection,
    auto_open: bool,
    github_host: Option<String>,
    keymap: crate::keymap::Keymap,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            theme: crate::theme::DEFAULT.to_owned(),
            base_branches: DEFAULT_BASE_BRANCHES.iter().map(|s| (*s).to_owned()).collect(),
            default_scope: crate::model::Scope::Uncommitted,
            navigator_position: NavigatorPosition::Right,
            toggle_placement: TogglePlacement::Split,
            toggle_direction: ToggleDirection::Right,
            auto_open: true,
            github_host: None,
            keymap: crate::keymap::Keymap::default(),
        }
    }
}

impl PluginConfig {
    pub fn theme(&self) -> &str {
        &self.theme
    }

    pub fn base_branches(&self) -> &[String] {
        &self.base_branches
    }

    /// The scope a fresh sidebar is built with — startup and config recovery. A reread never
    /// switches a running sidebar's scope (specs/review-model.md).
    pub fn default_scope(&self) -> crate::model::Scope {
        self.default_scope
    }

    pub fn navigator_position(&self) -> NavigatorPosition {
        self.navigator_position
    }

    pub fn toggle_placement(&self) -> TogglePlacement {
        self.toggle_placement
    }

    pub fn toggle_direction(&self) -> ToggleDirection {
        self.toggle_direction
    }

    pub fn auto_open(&self) -> bool {
        self.auto_open
    }

    pub fn github_host(&self) -> Option<&str> {
        self.github_host.as_deref()
    }

    /// The resolved keymap: the defaults with this snapshot's `[keybindings]` applied.
    pub fn keymap(&self) -> &crate::keymap::Keymap {
        &self.keymap
    }

    /// Stable machine-readable output consumed by the shell entry points.
    pub fn to_json(&self) -> serde_json::Value {
        let keybindings: serde_json::Map<String, serde_json::Value> = self
            .keymap
            .bindings()
            .iter()
            .map(|(action, keys)| {
                let keys: Vec<String> = keys.iter().map(|k| k.config_str()).collect();
                (action.name().to_owned(), serde_json::json!(keys))
            })
            .collect();
        serde_json::json!({
            "theme": self.theme,
            "base_branches": self.base_branches,
            "default_scope": self.default_scope.name(),
            "navigator_position": self.navigator_position.as_str(),
            "toggle_placement": self.toggle_placement.as_str(),
            "toggle_direction": self.toggle_direction.as_str(),
            "auto_open": self.auto_open,
            "github_host": self.github_host,
            "keybindings": keybindings,
        })
    }
}

/// A whole-file configuration failure. It keeps the path in the value so every entry point can
/// show the same actionable diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginConfigError {
    path: PathBuf,
    detail: String,
}

impl PluginConfigError {
    fn new(path: &Path, detail: impl Into<String>) -> Self {
        Self { path: path.to_owned(), detail: detail.into() }
    }
}

impl fmt::Display for PluginConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config {}: {}", self.path.display(), self.detail)
    }
}

impl std::error::Error for PluginConfigError {}

/// Read one plugin config snapshot from the process environment. An unset config directory is
/// standalone mode and uses defaults; a configured directory always names `config.toml`.
pub fn plugin_config() -> Result<PluginConfig, PluginConfigError> {
    let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") else {
        return Ok(PluginConfig::default());
    };
    plugin_config_in(dir)
}

/// Read one plugin config snapshot from `<dir>/config.toml`.
pub fn plugin_config_in(dir: impl AsRef<Path>) -> Result<PluginConfig, PluginConfigError> {
    parse_plugin_config(&dir.as_ref().join("config.toml"))
}

fn parse_plugin_config(path: &Path) -> Result<PluginConfig, PluginConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(PluginConfig::default()),
        Err(error) => {
            return Err(PluginConfigError::new(path, format!("read failed: {error}")));
        }
    };
    let table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
        PluginConfigError::new(path, format!("syntax error: {}", error.message()))
    })?;
    if let Some(key) = table.keys().find(|key| !PLUGIN_CONFIG_KEYS.contains(&key.as_str())) {
        return Err(unknown_key_error(path, key, &PLUGIN_CONFIG_KEYS.join(", ")));
    }

    let mut config = PluginConfig::default();
    if let Some(value) = table.get("theme") {
        let theme = string_value(path, "theme", value, "a built-in theme name")?;
        if !crate::theme::is_known(theme) {
            return Err(PluginConfigError::new(
                path,
                format!("invalid value for `theme`: {theme:?}; expected a built-in theme name"),
            ));
        }
        theme.clone_into(&mut config.theme);
    }
    if let Some(value) = table.get("base_branches") {
        let Some(values) = value.as_array() else {
            return Err(value_error(
                path,
                "base_branches",
                "a non-empty array of non-empty strings",
            ));
        };
        if values.is_empty() {
            return Err(value_error(
                path,
                "base_branches",
                "a non-empty array of non-empty strings",
            ));
        }
        let mut branches = Vec::with_capacity(values.len());
        for value in values {
            let Some(branch) = value.as_str() else {
                return Err(value_error(
                    path,
                    "base_branches",
                    "a non-empty array of non-empty strings",
                ));
            };
            if !valid_ref_name(branch) {
                return Err(value_error(
                    path,
                    "base_branches",
                    "a non-empty array of Git ref names",
                ));
            }
            let canonical = canonical_base(branch);
            if !branches.contains(&canonical) {
                branches.push(canonical);
            }
        }
        config.base_branches = branches;
    }
    if let Some(value) = table.get("default_scope") {
        config.default_scope = match string_value(
            path,
            "default_scope",
            value,
            "one of uncommitted, branch, last-turn",
        )? {
            "uncommitted" => crate::model::Scope::Uncommitted,
            "branch" => crate::model::Scope::Branch,
            "last-turn" => crate::model::Scope::LastTurn,
            _ => {
                return Err(value_error(
                    path,
                    "default_scope",
                    "one of uncommitted, branch, last-turn",
                ));
            }
        };
    }
    if let Some(value) = table.get("navigator_position") {
        config.navigator_position = match string_value(
            path,
            "navigator_position",
            value,
            "one of right, bottom, left, top",
        )? {
            "right" => NavigatorPosition::Right,
            "bottom" => NavigatorPosition::Bottom,
            "left" => NavigatorPosition::Left,
            "top" => NavigatorPosition::Top,
            _ => {
                return Err(value_error(
                    path,
                    "navigator_position",
                    "one of right, bottom, left, top",
                ));
            }
        };
    }
    if let Some(value) = table.get("toggle_placement") {
        config.toggle_placement = match string_value(
            path,
            "toggle_placement",
            value,
            "one of split, overlay, zoomed, tab",
        )? {
            "split" => TogglePlacement::Split,
            "overlay" => TogglePlacement::Overlay,
            "zoomed" => TogglePlacement::Zoomed,
            "tab" => TogglePlacement::Tab,
            _ => {
                return Err(value_error(
                    path,
                    "toggle_placement",
                    "one of split, overlay, zoomed, tab",
                ));
            }
        };
    }
    if let Some(value) = table.get("toggle_direction") {
        config.toggle_direction =
            match string_value(path, "toggle_direction", value, "one of right, down")? {
                "right" => ToggleDirection::Right,
                "down" => ToggleDirection::Down,
                _ => return Err(value_error(path, "toggle_direction", "one of right, down")),
            };
    }
    if let Some(value) = table.get("auto_open") {
        config.auto_open =
            value.as_bool().ok_or_else(|| value_error(path, "auto_open", "a boolean"))?;
    }
    if let Some(value) = table.get("github_host") {
        let host = string_value(path, "github_host", value, "a bare hostname outside github.com")?;
        if !valid_enterprise_host(host) {
            return Err(value_error(path, "github_host", "a bare hostname other than github.com"));
        }
        config.github_host = Some(host.to_ascii_lowercase());
    }
    if let Some(value) = table.get("keybindings") {
        config.keymap = parse_keybindings(path, value)?;
    }
    Ok(config)
}

/// One `[keybindings]` key string → a [`Key`](crate::keymap::Key): a bare character, or a
/// character behind a `ctrl+`/`alt+` prefix (`specs/config.md` `CFG-KEY-FORM`). The character is
/// one visible cell — a positive display width also rejects the zero-width class `is_control`
/// misses (format chars, combining marks).
fn parse_key(text: &str) -> Option<crate::keymap::Key> {
    let (ctrl, alt, rest) = if let Some(rest) = text.strip_prefix("ctrl+") {
        (true, false, rest)
    } else if let Some(rest) = text.strip_prefix("alt+") {
        (false, true, rest)
    } else {
        (false, false, text)
    };
    let mut it = rest.chars();
    match (it.next(), it.next()) {
        (Some(ch), None)
            if !ch.is_whitespace()
                && unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 0 =>
        {
            Some(crate::keymap::Key { ctrl, alt, ch })
        }
        _ => None,
    }
}

/// Parse and resolve the `[keybindings]` table (`specs/config.md` `CFG-KEY-FORM`/`CFG-KEY-UNIQUE`):
/// action names from the keymap table in `specs/input.md`, each bound to a non-empty array of
/// keys, a bare character or a `ctrl+`/`alt+` chord.
fn parse_keybindings(
    path: &Path,
    value: &toml::Value,
) -> Result<crate::keymap::Keymap, PluginConfigError> {
    use crate::keymap::{Action, Keymap};
    let Some(entries) = value.as_table() else {
        return Err(value_error(path, "keybindings", "a table of action bindings"));
    };
    let mut overrides = Vec::with_capacity(entries.len());
    let mut names_by_action = Vec::with_capacity(entries.len());
    for (name, keys) in entries {
        let Some(action) = Action::by_config_name(name) else {
            return Err(unknown_key_error(
                path,
                &format!("keybindings.{name}"),
                &Action::names().collect::<Vec<_>>().join(", "),
            ));
        };
        if let Some((_, first_name)) =
            names_by_action.iter().find(|(bound, _): &&(Action, &str)| *bound == action)
        {
            return Err(PluginConfigError::new(
                path,
                format!(
                    "invalid value for `keybindings`: `{first_name}` and `{name}` name the same action"
                ),
            ));
        }
        names_by_action.push((action, name.as_str()));
        let entry_key = format!("keybindings.{name}");
        let expected = "a non-empty array of keys, each a character or a ctrl+/alt+ chord";
        let Some(values) = keys.as_array() else {
            return Err(value_error(path, &entry_key, expected));
        };
        if values.is_empty() {
            return Err(value_error(path, &entry_key, expected));
        }
        let mut keys = Vec::with_capacity(values.len());
        for value in values {
            let Some(text) = value.as_str() else {
                return Err(value_error(path, &entry_key, expected));
            };
            match parse_key(text) {
                Some(key) => keys.push(key),
                None => return Err(value_error(path, &entry_key, expected)),
            }
        }
        overrides.push((action, keys));
    }
    Keymap::resolve(&overrides).map_err(|detail| {
        PluginConfigError::new(path, format!("invalid value for `keybindings`: {detail}"))
    })
}

fn string_value<'a>(
    path: &Path,
    key: &str,
    value: &'a toml::Value,
    expected: &str,
) -> Result<&'a str, PluginConfigError> {
    value.as_str().ok_or_else(|| value_error(path, key, expected))
}

fn value_error(path: &Path, key: &str, expected: &str) -> PluginConfigError {
    PluginConfigError::new(path, format!("invalid value for `{key}`; expected {expected}"))
}

/// The one `CFG-WHOLE-FILE` unknown-key grammar, shared by the top-level table and `[keybindings]`.
fn unknown_key_error(path: &Path, key: &str, options: &str) -> PluginConfigError {
    PluginConfigError::new(path, format!("unknown key {key:?}; expected one of {options}"))
}

fn valid_enterprise_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    lower != "github.com" && valid_host_syntax(host)
}

/// The bare ASCII DNS-name grammar shared by configured and selected canonical hosts.
pub(crate) fn valid_host_syntax(host: &str) -> bool {
    if host.len() > 253 {
        return false;
    }
    let mut labels = host.split('.').peekable();
    if labels.peek().is_none() {
        return false;
    }
    labels.all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Git's `check-ref-format --allow-onelevel` rules, used without spawning Git from the shared
/// configuration boundary. Base entries are names, not revision expressions.
fn valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && name != "@"
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("//")
        && !name.contains("..")
        && !name.contains("@{")
        && name
            .split('/')
            .all(|part| !part.starts_with('.') && part.strip_suffix(".lock").is_none())
        && name.bytes().all(|byte| {
            byte > b' '
                && byte != 0x7f
                && !matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

/// Print the shared normalized configuration for `herdr/sidebar.sh`.
pub fn print_plugin_config() -> Result<(), PluginConfigError> {
    println!("{}", plugin_config()?.to_json());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Config, NavigatorPosition, PluginConfig, ToggleDirection, TogglePlacement};
    use crate::model::Scope;
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

    #[test]
    fn missing_file_uses_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(super::plugin_config_in(dir.path()).unwrap(), PluginConfig::default());
    }

    #[test]
    fn omitted_keys_keep_their_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "theme = \"gruvbox\"\n").unwrap();
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.theme(), "gruvbox");
        assert_eq!(config.base_branches(), PluginConfig::default().base_branches());
        assert_eq!(config.default_scope(), Scope::Uncommitted);
        assert_eq!(config.navigator_position(), NavigatorPosition::Right);
        assert_eq!(config.toggle_placement(), TogglePlacement::Split);
        assert_eq!(config.toggle_direction(), ToggleDirection::Right);
        assert!(config.auto_open());
        assert_eq!(config.github_host(), None);
    }

    #[test]
    fn reads_complete_valid_file_as_one_value() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            concat!(
                "theme = \"tokyo-night\"\n",
                "base_branches = [\"origin/dev\", \"main\"]\n",
                "default_scope = \"last-turn\"\n",
                "navigator_position = \"bottom\"\n",
                "toggle_placement = \"overlay\"\n",
                "toggle_direction = \"down\"\n",
                "auto_open = false\n",
                "github_host = \"GitHub.Example.COM\"\n",
            ),
        )
        .unwrap();
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.theme(), "tokyo-night");
        // Entries canonicalize to bare names at validation (`specs/config.md`).
        assert_eq!(config.base_branches(), ["dev", "main"]);
        assert_eq!(config.default_scope(), Scope::LastTurn);
        assert_eq!(config.navigator_position(), NavigatorPosition::Bottom);
        assert_eq!(config.toggle_placement(), TogglePlacement::Overlay);
        assert_eq!(config.toggle_direction(), ToggleDirection::Down);
        assert!(!config.auto_open());
        assert_eq!(config.github_host(), Some("github.example.com"));
    }

    #[test]
    fn base_entries_canonicalize_and_duplicates_collapse_to_the_first() {
        assert_eq!(super::canonical_base("origin/main"), "main");
        assert_eq!(super::canonical_base("refs/heads/main"), "main");
        assert_eq!(super::canonical_base("refs/remotes/origin/main"), "main");
        assert_eq!(super::canonical_base("release/1.0"), "release/1.0");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "base_branches = [\"origin/main\", \"main\", \"refs/heads/master\"]\n",
        )
        .unwrap();
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.base_branches(), ["main", "master"]);
    }

    #[test]
    fn unknown_key_and_syntax_error_fail_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"gruvbox\"\npoll = 500\n").unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains(path.to_str().unwrap()));
        assert!(error.contains("unknown key \"poll\""));

        std::fs::write(&path, "theme = [\n").unwrap();
        assert!(
            super::plugin_config_in(dir.path()).unwrap_err().to_string().contains("syntax error")
        );
    }

    #[test]
    fn every_invalid_value_fails_instead_of_falling_back() {
        let cases = [
            ("theme = \"unknown\"\n", "`theme`"),
            ("base_branches = []\n", "`base_branches`"),
            ("base_branches = [\"\"]\n", "`base_branches`"),
            ("base_branches = [\"main^{commit}\"]\n", "`base_branches`"),
            ("base_branches = [\"feature branch\"]\n", "`base_branches`"),
            ("base_branches = [\"-main\"]\n", "`base_branches`"),
            ("base_branches = [\"main\", 1]\n", "`base_branches`"),
            ("default_scope = \"weekly\"\n", "`default_scope`"),
            ("default_scope = \"last turn\"\n", "`default_scope`"),
            ("navigator_position = \"center\"\n", "`navigator_position`"),
            ("toggle_placement = \"left\"\n", "`toggle_placement`"),
            ("toggle_direction = \"left\"\n", "`toggle_direction`"),
            ("auto_open = \"yes\"\n", "`auto_open`"),
            ("github_host = \"https://github.example.com\"\n", "`github_host`"),
            ("github_host = \"github.com\"\n", "`github_host`"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (text, key) in cases {
            std::fs::write(&path, text).unwrap();
            let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
            assert!(error.contains(key), "{text}: {error}");
            assert!(error.contains("expected"), "{text}: {error}");
        }
    }

    #[test]
    fn github_host_accepts_a_literal_github_com_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "github_host = \"github.com-work\"\n")
            .unwrap();
        let config = super::plugin_config_in(dir.path()).expect("valid literal Enterprise host");
        assert_eq!(config.github_host(), Some("github.com-work"));
    }

    #[test]
    fn keybindings_alias_and_replace_per_action() {
        use crate::keymap::{Action, Key};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[keybindings]\ncomment = [\"c\", \"ㅊ\"]\nsend = [\"x\"]\n",
        )
        .unwrap();
        let config = super::plugin_config_in(dir.path()).unwrap();
        let keymap = config.keymap();
        assert_eq!(keymap.action_for(Key::plain('ㅊ')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::plain('c')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::plain('x')), Some(Action::Send));
        assert_eq!(keymap.action_for(Key::plain('s')), None, "a binding replaces its defaults");
        assert_eq!(keymap.action_for(Key::plain('S')), None);
        assert_eq!(keymap.action_for(Key::plain('v')), Some(Action::Select), "unbound keep theirs");
    }

    #[test]
    fn find_binds_to_a_chord_and_round_trips() {
        use crate::keymap::{Action, Key};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // The default `find` chord resolves and serializes in config syntax.
        std::fs::write(&path, "theme = \"catppuccin\"\n").unwrap();
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.keymap().action_for(Key::ctrl('f')), Some(Action::Find));
        let bindings = config.to_json()["keybindings"].as_object().unwrap().clone();
        assert_eq!(bindings["find"], serde_json::json!(["ctrl+f"]));

        // A rebind to another chord takes, and the old default frees.
        std::fs::write(&path, "[keybindings]\nfind = [\"alt+x\"]\n").unwrap();
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(
            config.keymap().action_for(Key { ctrl: false, alt: true, ch: 'x' }),
            Some(Action::Find)
        );
        assert_eq!(config.keymap().action_for(Key::ctrl('f')), None);
        // The `alt+` chord serializes back in config syntax, not the glyph.
        assert_eq!(config.to_json()["keybindings"]["find"], serde_json::json!(["alt+x"]));

        // A malformed chord fails `CFG-KEY-FORM`.
        std::fs::write(&path, "[keybindings]\nfind = [\"ctrl+\"]\n").unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("`keybindings.find`") && error.contains("expected"), "{error}");
    }

    #[test]
    fn legacy_navigator_actions_resolve_to_canonical_actions() {
        use crate::keymap::{Action, Key};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[keybindings]\nlist-wider = [\"g\"]\nlist-narrower = [\"h\"]\n")
            .unwrap();
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.keymap().action_for(Key::plain('g')), Some(Action::NavigatorGrow));
        assert_eq!(config.keymap().action_for(Key::plain('h')), Some(Action::NavigatorShrink));
        let json = config.to_json();
        let bindings = json["keybindings"].as_object().unwrap();
        assert!(bindings.contains_key("navigator-grow"));
        assert!(bindings.contains_key("navigator-shrink"));
        assert!(!bindings.contains_key("list-wider"));
        assert!(!bindings.contains_key("list-narrower"));

        std::fs::write(&path, "[keybindings]\nnavigator-grow = [\"g\"]\nlist-wider = [\"h\"]\n")
            .unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("same action"), "{error}");
        assert!(error.contains("navigator-grow") && error.contains("list-wider"), "{error}");
    }

    #[test]
    fn a_new_default_collision_invalidates_the_resolved_keymap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[keybindings]\npreview = [\"p\"]\n")
            .unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("`preview`") && error.contains("`navigator-position`"), "{error}");
        assert!(error.contains("p is bound"), "{error}");
    }

    #[test]
    fn key_is_one_printable_codepoint() {
        let cases = [
            "comment = [\"cc\"]\n",
            "comment = [\"\"]\n",
            "comment = [\" \"]\n",
            "comment = [\"\\u0007\"]\n",
            "comment = [\"e\\u0301\"]\n",
            "comment = [\"\\u200B\"]\n",
            "comment = [\"\\u0301\"]\n",
            "comment = []\n",
            "comment = \"c\"\n",
            "comment = [1]\n",
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for entry in cases {
            std::fs::write(&path, format!("[keybindings]\n{entry}")).unwrap();
            let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
            assert!(error.contains("`keybindings.comment`"), "{entry}: {error}");
            assert!(error.contains("expected"), "{entry}: {error}");
        }
    }

    #[test]
    fn keybinding_collision_names_each_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        std::fs::write(&path, "[keybindings]\ncomment = [\"v\"]\n").unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("`comment`") && error.contains("`select`"), "{error}");

        std::fs::write(&path, "[keybindings]\ncomment = [\"c\", \"c\"]\n").unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("bound twice") && error.contains("`comment`"), "{error}");
    }

    #[test]
    fn unknown_action_is_an_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[keybindings]\nfoo = [\"x\"]\n").unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("unknown key \"keybindings.foo\""), "{error}");
        assert!(error.contains("comment"), "the error lists the action names: {error}");
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_config_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config.toml")).unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("read failed"));
        assert!(error.contains("config.toml"));
    }

    #[test]
    fn normalized_json_contains_every_key() {
        let value = PluginConfig::default().to_json();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), super::PLUGIN_CONFIG_KEYS.len(), "one JSON key per config key");
        assert_eq!(object["default_scope"], "uncommitted");
        assert_eq!(object["navigator_position"], "right");
        assert_eq!(object["toggle_placement"], "split");
        assert_eq!(object["toggle_direction"], "right");
        assert_eq!(object["auto_open"], true);
        assert!(object["github_host"].is_null());
        let keybindings = object["keybindings"].as_object().unwrap();
        assert_eq!(
            keybindings.len(),
            crate::keymap::Action::names().count(),
            "every action is present, resolved"
        );
        assert_eq!(keybindings["quit"], serde_json::json!(["q"]));
        assert_eq!(keybindings["send"], serde_json::json!(["s", "S"]));
    }
}
