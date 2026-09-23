//! Resolving the editor command for `edit` on a file.
//!
//! Two sources, in order: the `editor` config key, whose value is a template the user owns
//! outright, and `$VISUAL`/`$EDITOR`, whose binary name selects the argument dialect below.
//! This module is process-free: it builds an argv, and `src/lib.rs` spawns it.

use std::path::Path;

/// How an editor spells "open this file at this line".
///
/// Four shapes cover every editor in [`DIALECTS`]. Sources: lazygit's editor presets, Julia's
/// `InteractiveUtils` editor table, and each vendor's own CLI documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineArg {
    /// `+42 <path>` — the vi family, nano, micro, kakoune, emacs, `BBEdit`, gedit.
    Plus,
    /// `<path>:42` — helix, Zed, Sublime Text.
    Suffix,
    /// `-g <path>:42` — the VS Code family.
    Goto,
    /// `--line 42 <path>` — the `JetBrains` family, Xcode, Kate, `TextMate`.
    Flag,
}

/// One editor family: the binary names that select it, how it takes a line, and where it draws.
///
/// A window editor hands the file to an instance of its own and returns, so reviewr keeps the
/// pane. A terminal editor draws in the pane and is given it.
struct Dialect {
    names: &'static [&'static str],
    line: LineArg,
    window: bool,
}

const DIALECTS: &[Dialect] = &[
    // Terminal editors. Each holds the pane until it exits.
    Dialect {
        names: &["vi", "vim", "nvim", "lvim", "vis", "joe"],
        line: LineArg::Plus,
        window: false,
    },
    Dialect { names: &["nano", "micro", "kak"], line: LineArg::Plus, window: false },
    // Emacs is whichever its build and `DISPLAY` make it, and neither is readable from here.
    // The pane is the survivable guess: a windowed Emacs handed the pane leaves the pane blank
    // until it quits, where a terminal Emacs denied it is invisible.
    Dialect { names: &["emacs", "emacsclient"], line: LineArg::Plus, window: false },
    Dialect { names: &["hx", "helix"], line: LineArg::Suffix, window: false },
    // MacVim and gVim open a window and return, unlike every other vi-family binary.
    Dialect { names: &["mvim", "gvim"], line: LineArg::Plus, window: true },
    // Graphical editors.
    Dialect {
        names: &["code", "code-insiders", "codium", "vscodium", "cursor", "windsurf", "positron"],
        line: LineArg::Goto,
        window: true,
    },
    Dialect { names: &["subl", "sublime_text"], line: LineArg::Suffix, window: true },
    // Plain `zed` collides with the OpenZFS event daemon, so Linux packages ship the CLI
    // under a name of their own.
    Dialect { names: &["zed", "zeditor", "zedit"], line: LineArg::Suffix, window: true },
    Dialect { names: &["bbedit", "gedit"], line: LineArg::Plus, window: true },
    Dialect { names: &["mate"], line: LineArg::Flag, window: true },
    // `xed` names two editors. On macOS it is Xcode's opener, which takes `--line`. On Linux it
    // is Mint's X-Apps editor, a gedit fork that takes `+LINE` and rejects `--line` outright.
    #[cfg(target_os = "macos")]
    Dialect { names: &["xed"], line: LineArg::Flag, window: true },
    #[cfg(not(target_os = "macos"))]
    Dialect { names: &["xed"], line: LineArg::Plus, window: true },
    Dialect { names: &["kate"], line: LineArg::Flag, window: true },
    Dialect {
        names: &[
            "idea",
            "pycharm",
            "webstorm",
            "goland",
            "clion",
            "phpstorm",
            "rubymine",
            "rider",
            "datagrip",
            "rustrover",
            "dataspell",
            "fleet",
        ],
        line: LineArg::Flag,
        window: true,
    },
];

/// A resolved editor invocation: the program to run, its full argument list, and whether it
/// wants the terminal.
///
/// A terminal editor paints in the pane and must be handed it outright. A window one draws in an
/// instance of its own and never reads the terminal, so reviewr keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorCommand {
    pub program: String,
    pub args: Vec<String>,
    pub wants_terminal: bool,
}

/// Why no editor ran.
#[derive(Debug, PartialEq, Eq)]
pub enum NoEditor {
    /// Neither the `editor` key nor `$VISUAL` nor `$EDITOR` is set.
    Unset,
    /// A value is set, but its first word is not a program name.
    NamesNoProgram,
}

/// The command that opens `path` at `line`.
///
/// `configured` is the `editor` config key. Its value is the whole command, and `{file}` and
/// `{line}` substitute into it wherever they appear. A template that does not name `{file}` gets
/// the path appended, so a bare `editor = "hx"` still opens the file.
///
/// With no key, `visual` then `editor_env` supply the command, and its binary name selects a
/// dialect. An unrecognized binary opens the file without a line rather than guessing a flag it
/// may not accept. The binary name also decides who owns the pane, in every path
/// ([`wants_terminal`]).
///
/// `path` must be absolute. That is what keeps the dialects that append it bare from handing an
/// editor a file name it reads as a flag, so no dialect needs a `--` guard.
pub fn resolve(
    configured: Option<&str>,
    visual: Option<&str>,
    editor_env: Option<&str>,
    path: &Path,
    line: u32,
) -> Result<EditorCommand, NoEditor> {
    let file = path.to_string_lossy().into_owned();
    if let Some(template) = configured {
        return from_template(template, &file, line);
    }
    let Some(value) = visual
        .filter(|v| !v.trim().is_empty())
        .or_else(|| editor_env.filter(|v| !v.trim().is_empty()))
    else {
        return Err(NoEditor::Unset);
    };
    let mut words = split_command(value).into_iter();
    // The same guard the template gets: a value of two quote characters is not empty and
    // splits to one empty word, and handing that to the pane would flip the screen for a
    // spawn that cannot succeed.
    let Some(program) = words.next().filter(|p| !p.is_empty()) else {
        return Err(NoEditor::NamesNoProgram);
    };
    let mut args: Vec<String> = words.collect();
    match dialect_for(&program).map(|d| d.line) {
        None => args.push(file),
        Some(LineArg::Plus) => {
            args.push(format!("+{line}"));
            args.push(file);
        }
        Some(LineArg::Suffix) => args.push(format!("{file}:{line}")),
        Some(LineArg::Goto) => {
            args.push("-g".to_owned());
            args.push(format!("{file}:{line}"));
        }
        Some(LineArg::Flag) => {
            args.push("--line".to_owned());
            args.push(line.to_string());
            args.push(file);
        }
    }
    let wants_terminal = wants_terminal(&program, &args);
    Ok(EditorCommand { program, args, wants_terminal })
}

/// Whether the command draws in the pane it was launched from.
///
/// Two signals, asked in the same order in every path. A command already carrying a wait flag
/// names a window editor whatever its binary is called: no terminal editor has one, because
/// blocking is what a terminal editor inherently does. Long forms only, since `-w` and `-b` are
/// ordinary short flags elsewhere (helix spells `--working-dir` as `-w`).
///
/// Failing that, the binary's own name. Unknown means the pane, which is the outcome a terminal
/// editor cannot survive being denied.
fn wants_terminal(program: &str, args: &[String]) -> bool {
    if args.iter().any(|a| a == "--wait" || a == "--block") {
        return false;
    }
    dialect_for(program).is_none_or(|d| !d.window)
}

/// Split a command into words, honouring quotes.
///
/// A plain whitespace split cannot express `/Applications/Sublime Text.app/.../subl`, which is
/// how macOS spells most editor paths. Quoting is the only escape, since no shell runs the
/// command. A quote closes at its match or at the end of the string.
fn split_command(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for ch in value.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => word.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(ch);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// The dialect for a binary, matched on its file name so an absolute `$EDITOR` resolves too.
fn dialect_for(program: &str) -> Option<&'static Dialect> {
    let name = Path::new(program).file_name()?.to_string_lossy().to_lowercase();
    DIALECTS.iter().find(|d| d.names.contains(&name.as_str()))
}

/// Build the command from a user template, substituting every `{file}` and `{line}`.
///
/// `{line}` goes first, and that ordering is the whole trick: a path is only ever substituted
/// into text nothing looks at again, so a file named `{line}.cshtml` stays a file name rather
/// than becoming a second placeholder.
///
/// The config layer rejects an empty value, but a value of two quote characters is not empty and
/// splits to one empty word, which names no program.
fn from_template(template: &str, file: &str, line: u32) -> Result<EditorCommand, NoEditor> {
    let named_file = template.contains("{file}");
    let line = line.to_string();
    let substitute = |w: String| w.replace("{line}", &line).replace("{file}", file);
    let mut words = split_command(template).into_iter().map(substitute);
    let Some(program) = words.next().filter(|p| !p.is_empty()) else {
        return Err(NoEditor::NamesNoProgram);
    };
    let mut args: Vec<String> = words.collect();
    if !named_file {
        args.push(file.to_owned());
    }
    let wants_terminal = wants_terminal(&program, &args);
    Ok(EditorCommand { program, args, wants_terminal })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("/repo/src/lib.rs")
    }

    fn env(value: &str) -> Option<EditorCommand> {
        resolve(None, None, Some(value), &p(), 41).ok()
    }

    fn argv(cmd: &EditorCommand) -> String {
        format!("{} {}", cmd.program, cmd.args.join(" "))
    }

    #[test]
    fn terminal_editors_take_a_plus_line_and_never_wait() {
        for name in ["vi", "vim", "nvim", "nano", "micro", "kak", "emacs"] {
            assert_eq!(
                argv(&env(name).unwrap()),
                format!("{name} +41 /repo/src/lib.rs"),
                "{name} takes `+LINE` before the path and owns the pane already"
            );
        }
    }

    #[test]
    fn helix_takes_the_line_as_a_path_suffix() {
        assert_eq!(argv(&env("hx").unwrap()), "hx /repo/src/lib.rs:41");
        assert_eq!(argv(&env("helix").unwrap()), "helix /repo/src/lib.rs:41");
    }

    #[test]
    fn window_editors_get_their_line_and_nothing_else() {
        // reviewr adds no flag of its own: the launcher hands the file over and returns, and
        // nothing waits on it.
        for name in ["code", "code-insiders", "codium", "cursor", "windsurf", "positron"] {
            assert_eq!(argv(&env(name).unwrap()), format!("{name} -g /repo/src/lib.rs:41"));
        }
        assert_eq!(argv(&env("zed").unwrap()), "zed /repo/src/lib.rs:41");
        assert_eq!(argv(&env("subl").unwrap()), "subl /repo/src/lib.rs:41");
        assert_eq!(argv(&env("bbedit").unwrap()), "bbedit +41 /repo/src/lib.rs");
        assert_eq!(argv(&env("mate").unwrap()), "mate --line 41 /repo/src/lib.rs");
        assert_eq!(argv(&env("kate").unwrap()), "kate --line 41 /repo/src/lib.rs");
        #[cfg(target_os = "macos")]
        assert_eq!(argv(&env("xed").unwrap()), "xed --line 41 /repo/src/lib.rs");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(argv(&env("xed").unwrap()), "xed +41 /repo/src/lib.rs");
        for name in ["idea", "pycharm", "webstorm", "goland", "clion", "rustrover", "fleet"] {
            assert_eq!(argv(&env(name).unwrap()), format!("{name} --line 41 /repo/src/lib.rs"));
        }
    }

    #[test]
    fn only_a_terminal_editor_is_handed_the_pane() {
        // The one question the argv does not answer, so nothing else in this module asserts it:
        // which of the two paths `run_editor` takes.
        for name in ["vim", "nvim", "nano", "micro", "kak", "emacs", "hx", "helix"] {
            assert!(env(name).unwrap().wants_terminal, "{name} paints in the pane");
        }
        // Argv-identical to a terminal editor of the same dialect, so only this tells them
        // apart: `zed`/`subl` against `hx`, `bbedit`/`gedit`/`mvim` against `vim`.
        for name in ["code", "cursor", "zed", "zeditor", "subl", "idea", "kate", "mate"] {
            assert!(!env(name).unwrap().wants_terminal, "{name} opens a window");
        }
        for name in ["mvim", "gvim", "bbedit", "gedit"] {
            assert!(!env(name).unwrap().wants_terminal, "{name} opens a window");
        }
        // A configured command spells its own arguments, but it is still one of these
        // binaries, and the same name answers the same question.
        let cfg = |t: &str| resolve(Some(t), None, None, &p(), 41).unwrap().wants_terminal;
        assert!(!cfg("code -g {file}:{line}"), "the documented example keeps the pane");
        assert!(cfg("vim +{line} {file}"), "a terminal editor still takes it");
        // A wait flag the reviewer wrote names a window editor whatever the binary is called:
        // Zed's own bundle ships its CLI as `cli`, and that must not cost the reviewer the pane.
        assert!(!cfg("/Applications/Zed.app/Contents/MacOS/cli --wait {file}:{line}"));
        assert!(!cfg("/opt/weird/ed --block --line {line} {file}"));
        assert!(!env("/Applications/Zed.app/Contents/MacOS/cli --wait").unwrap().wants_terminal);
        // Short spellings are ordinary flags elsewhere, so they say nothing: helix's `-w` is
        // `--working-dir`.
        assert!(cfg("hx -w {file}"));
        // An unknown binary says nothing, and only one of the two guesses is survivable.
        assert!(env("myeditor").unwrap().wants_terminal);
        assert!(cfg("myed {file}"));
    }

    #[test]
    fn the_reviewers_own_flags_survive() {
        // `EDITOR="code --wait"` is the documented git setup, so it arrives already waiting.
        assert_eq!(argv(&env("code --wait").unwrap()), "code --wait -g /repo/src/lib.rs:41");
        assert_eq!(argv(&env("kate -b").unwrap()), "kate -b --line 41 /repo/src/lib.rs");
        assert_eq!(argv(&env("mvim -f").unwrap()), "mvim -f +41 /repo/src/lib.rs");
    }

    #[test]
    fn a_quoted_path_with_spaces_stays_one_word() {
        // The common macOS spelling. No shell runs the command, so quoting is the only escape.
        let subl = "/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl";
        let cmd = env(&format!("\"{subl}\"")).unwrap();
        assert_eq!(cmd.program, subl, "the whole quoted path is the program");
        assert_eq!(
            cmd.args,
            ["/repo/src/lib.rs:41"],
            "and the quoted path's own name still picks the dialect"
        );

        // Single quotes too, and a quoted argument after the program.
        let cmd = env(&format!("'{subl}' --project 'My Project.sublime-project'")).unwrap();
        assert_eq!(cmd.program, subl);
        assert_eq!(cmd.args, ["--project", "My Project.sublime-project", "/repo/src/lib.rs:41"]);

        // The config template quotes the same way.
        let cmd =
            resolve(Some("'/opt/my editor' --at {line} {file}"), None, None, &p(), 41).unwrap();
        assert_eq!(cmd.program, "/opt/my editor");
        assert_eq!(cmd.args, ["--at", "41", "/repo/src/lib.rs"]);

        // An unterminated quote closes at the end rather than dropping the word.
        assert_eq!(env("\"/opt/my editor").unwrap().program, "/opt/my editor");

        // A closed empty quote is a word, so it can be the one in program position. Dropping
        // that distinction would silently run the second word as the editor instead.
        assert_eq!(resolve(None, None, Some("'' vim"), &p(), 41), Err(NoEditor::NamesNoProgram));
    }

    #[test]
    fn an_unknown_editor_opens_the_file_without_a_line() {
        assert_eq!(
            argv(&env("myeditor").unwrap()),
            "myeditor /repo/src/lib.rs",
            "guessing a flag an unknown editor may not accept would open a stray buffer"
        );
    }

    #[test]
    fn visual_outranks_editor_and_blank_values_fall_through() {
        assert_eq!(
            argv(&resolve(None, Some("hx"), Some("vim"), &p(), 41).unwrap()),
            "hx /repo/src/lib.rs:41"
        );
        assert_eq!(
            argv(&resolve(None, Some("  "), Some("vim"), &p(), 41).unwrap()),
            "vim +41 /repo/src/lib.rs"
        );
        assert_eq!(
            resolve(None, None, None, &p(), 41),
            Err(NoEditor::Unset),
            "no editor anywhere resolves nothing"
        );
        assert_eq!(resolve(None, Some(""), Some(" "), &p(), 41), Err(NoEditor::Unset));
        // Not empty, but it splits to one empty word, so it names no program. The pane must not
        // change hands for a spawn that cannot happen.
        assert_eq!(resolve(None, None, Some("\"\""), &p(), 41), Err(NoEditor::NamesNoProgram));
    }

    #[test]
    fn the_config_template_owns_the_whole_command() {
        assert_eq!(
            argv(&resolve(Some("code -g {file}:{line}"), None, Some("vim"), &p(), 41).unwrap()),
            "code -g /repo/src/lib.rs:41",
            "the configured template outranks the environment and takes no added flags"
        );
        assert_eq!(
            argv(&resolve(Some("idea --line {line} --wait {file}"), None, None, &p(), 41).unwrap()),
            "idea --line 41 --wait /repo/src/lib.rs"
        );
        assert_eq!(
            argv(&resolve(Some("hx"), None, None, &p(), 41).unwrap()),
            "hx /repo/src/lib.rs",
            "a template naming no placeholder still gets the path"
        );
        assert_eq!(
            argv(&resolve(Some("myed {line} {file} {line}"), None, None, &p(), 41).unwrap()),
            "myed 41 /repo/src/lib.rs 41",
            "every occurrence substitutes"
        );
        // `{line}` substitutes before `{file}`, so a path is only ever placed into text nothing
        // reads again. Swap the two and this file name becomes a second placeholder.
        let odd = PathBuf::from("/repo/{line}.cshtml");
        assert_eq!(
            argv(&resolve(Some("myed {file}:{line}"), None, None, &odd, 41).unwrap()),
            "myed /repo/{line}.cshtml:41",
            "a path that spells a placeholder stays a path"
        );
    }
}
