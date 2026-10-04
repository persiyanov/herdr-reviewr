//! Best-effort system appearance following.
//!
//! We deliberately do not issue OSC 11 from the event loop. Crossterm 0.29's Unix input parser
//! does not expose OSC replies as events (it can interpret an `ESC ]` reply as Alt+`]` and the
//! remaining reply as ordinary keystrokes), so probing stdin there could eat or corrupt user
//! input. Herdr 0.9.3's public API snapshot/schema also exposes no pane/system appearance field.
//! Instead we sample the host's desktop appearance once when the pane opens, where it is
//! queryable, with terminal environment hints as a fallback. Unsupported or remote/headless
//! environments stay on the dark default.

use std::process::{Command, Stdio};
use std::thread;

use std::time::{Duration, Instant};

/// The detected display preference, independent of a particular theme palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    /// The paired built-in default for this appearance.
    pub const fn default_theme(self) -> &'static str {
        match self {
            Self::Dark => "catppuccin",
            Self::Light => "catppuccin-latte",
        }
    }
}

/// The winning theme request: CLI, explicit plugin config, detected appearance, dark fallback.
///
/// A `String` return keeps the order explicit while preserving user-provided names for the
/// theme resolver to validate/fallback as it already does.
pub fn select_theme(
    cli: Option<&str>,
    configured: Option<&str>,
    appearance: Option<Appearance>,
) -> String {
    cli.or(configured).map_or_else(
        || appearance.unwrap_or(Appearance::Dark).default_theme().to_owned(),
        str::to_owned,
    )
}

/// Read a GNOME color-scheme value (`gsettings get … color-scheme`).
pub fn parse_gnome_color_scheme(value: &str) -> Option<Appearance> {
    match value.trim().trim_matches('\'').trim_matches('"') {
        "prefer-dark" => Some(Appearance::Dark),
        "prefer-light" => Some(Appearance::Light),
        _ => None,
    }
}

/// Read the appearance convention used by GTK theme names and `$GTK_THEME`.
pub fn parse_gtk_theme(value: &str) -> Option<Appearance> {
    let theme = value.trim().trim_matches('\'').trim_matches('"').to_ascii_lowercase();
    if theme.ends_with(":dark") || theme.ends_with("-dark") {
        Some(Appearance::Dark)
    } else if theme.ends_with(":light") || theme.ends_with("-light") {
        Some(Appearance::Light)
    } else {
        None
    }
}

/// Classify a conventional `foreground;background` `$COLORFGBG` value when its background is
/// one of the unambiguous ANSI black/white endpoints. Other palette indices are terminal-specific.
pub fn parse_colorfgbg(value: &str) -> Option<Appearance> {
    let background = value.trim().rsplit_once(';')?.1.parse::<u8>().ok()?;
    match background {
        0 | 8 => Some(Appearance::Dark),
        7 | 15 => Some(Appearance::Light),
        _ => None,
    }
}

/// Parse the successful output of `defaults read -g AppleInterfaceStyle`.
///
/// macOS omits the key in Light mode; a completed query without `Dark` is treated as Light.
/// A command that cannot start or times out remains unknown.
pub fn parse_macos_interface_style(value: &str) -> Appearance {
    if value.trim().eq_ignore_ascii_case("dark") { Appearance::Dark } else { Appearance::Light }
}

const COMMAND_TIMEOUT: Duration = Duration::from_millis(400);

/// Detect the current host appearance once, during startup. Command-based platform probes are
/// individually bounded and run before normal input dispatch begins.
pub(crate) fn detect() -> Option<Appearance> {
    #[cfg(target_os = "macos")]
    if let Some(result) = command_output("defaults", &["read", "-g", "AppleInterfaceStyle"]) {
        if result.success {
            return Some(parse_macos_interface_style(&result.stdout));
        }
        // The global key is absent in the normal Light appearance. A command timeout/spawn error
        // is `None` above and falls through to other hints instead.
        return Some(Appearance::Light);
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(result) =
            command_output("gsettings", &["get", "org.gnome.desktop.interface", "color-scheme"])
            && result.success
            && let Some(appearance) = parse_gnome_color_scheme(&result.stdout)
        {
            return Some(appearance);
        }
        if let Some(result) =
            command_output("gsettings", &["get", "org.gnome.desktop.interface", "gtk-theme"])
            && result.success
            && let Some(appearance) = parse_gtk_theme(&result.stdout)
        {
            return Some(appearance);
        }
        if let Some(theme) = std::env::var("GTK_THEME").ok().and_then(|s| parse_gtk_theme(&s)) {
            return Some(theme);
        }
    }

    std::env::var("COLORFGBG").ok().and_then(|value| parse_colorfgbg(&value))
}

struct CommandOutput {
    success: bool,
    stdout: String,
}

/// Run a desktop-preference command off the frame loop with a strict deadline.
fn command_output(program: &str, args: &[&str]) -> Option<CommandOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().ok()?;
                return Some(CommandOutput {
                    success: status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                });
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Appearance, parse_colorfgbg, parse_gnome_color_scheme, parse_gtk_theme,
        parse_macos_interface_style, select_theme,
    };

    #[test]
    fn parses_gnome_preferences_without_guessing_default() {
        assert_eq!(parse_gnome_color_scheme("'prefer-dark'\n"), Some(Appearance::Dark));
        assert_eq!(parse_gnome_color_scheme("'prefer-light'"), Some(Appearance::Light));
        assert_eq!(parse_gnome_color_scheme("'default'"), None);
        assert_eq!(parse_gnome_color_scheme("unknown"), None);
    }

    #[test]
    fn parses_gtk_and_terminal_environment_hints() {
        assert_eq!(parse_gtk_theme("'Adwaita-dark'"), Some(Appearance::Dark));
        assert_eq!(parse_gtk_theme("Adwaita:light"), Some(Appearance::Light));
        assert_eq!(parse_gtk_theme("Adwaita"), None);
        assert_eq!(parse_colorfgbg("15;0"), Some(Appearance::Dark));
        assert_eq!(parse_colorfgbg("0;15"), Some(Appearance::Light));
        assert_eq!(parse_colorfgbg("15;4"), None, "palette indices are terminal-specific");
        assert_eq!(parse_colorfgbg("bad"), None);
        assert_eq!(parse_colorfgbg("0"), None, "COLORFGBG needs a foreground/background pair");
    }

    #[test]
    fn macos_missing_dark_value_means_light_after_a_successful_query() {
        assert_eq!(parse_macos_interface_style("Dark\n"), Appearance::Dark);
        assert_eq!(parse_macos_interface_style(""), Appearance::Light);
        assert_eq!(parse_macos_interface_style("Light"), Appearance::Light);
    }

    #[test]
    fn fallback_and_override_order_are_explicit() {
        assert_eq!(select_theme(None, None, None), "catppuccin");
        assert_eq!(select_theme(None, None, Some(Appearance::Light)), "catppuccin-latte");
        assert_eq!(select_theme(None, None, Some(Appearance::Dark)), "catppuccin");
        assert_eq!(select_theme(None, Some("nord"), Some(Appearance::Light)), "nord");
        assert_eq!(
            select_theme(Some("tokyo-night-day"), Some("nord"), Some(Appearance::Dark)),
            "tokyo-night-day"
        );
    }
}
