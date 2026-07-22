//! The rebindable action keymap: action names, default keys, resolution of `[keybindings]`
//! overrides, and the key → action lookup the dispatcher and the hint renderers share
//! (`specs/input.md`, `specs/config.md` Keybindings).

use std::sync::LazyLock;

/// One rebindable action from the keymap table in `specs/input.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    NextHunk,
    PrevHunk,
    NextFile,
    PrevFile,
    ScopeUncommitted,
    ScopeBranch,
    ScopeLastTurn,
    TabChanges,
    TabAllFiles,
    TabPr,
    Wrap,
    Preview,
    NavigatorPosition,
    NavigatorGrow,
    NavigatorShrink,
    Select,
    Comment,
    Edit,
    Delete,
    NextComment,
    PrevComment,
    Comments,
    Search,
    Find,
    Keys,
    Send,
    Copy,
    OpenPr,
    Refresh,
    Quit,
}

/// One bound key: a base character, alone or under a `ctrl`/`alt` modifier. A modifier-less
/// `Key` is the bare character the keymap answered before chords existed
/// (`specs/config.md` `CFG-KEY-FORM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub ctrl: bool,
    pub alt: bool,
    pub ch: char,
}

impl Key {
    /// A bare character, no modifier.
    pub const fn plain(ch: char) -> Self {
        Self { ctrl: false, alt: false, ch }
    }

    /// A `ctrl+<ch>` chord.
    pub const fn ctrl(ch: char) -> Self {
        Self { ctrl: true, alt: false, ch }
    }

    /// The spelling `[keybindings]` and `--resolve-plugin-config` round-trip, also shown as the
    /// hint: `ctrl+f`, `alt+x`, or the bare character. Spelled out, never a glyph, so the same
    /// text names a key in the config and on screen (specs/input.md, specs/config.md).
    pub fn config_str(self) -> String {
        match (self.ctrl, self.alt) {
            (true, _) => format!("ctrl+{}", self.ch),
            (false, true) => format!("alt+{}", self.ch),
            (false, false) => self.ch.to_string(),
        }
    }
}

impl std::fmt::Display for Key {
    /// The footer and header hint, spelled out like the config (`ctrl+f`), one home in `config_str`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.config_str())
    }
}

/// Every action with its config name and default keys — the single source the default keymap,
/// the name lookup, and the config error message are built from.
const ACTIONS: [(Action, &str, &[Key]); 32] = [
    (Action::Down, "down", &[Key::plain('j')]),
    (Action::Up, "up", &[Key::plain('k')]),
    (Action::NextHunk, "next-hunk", &[Key::plain(']')]),
    (Action::PrevHunk, "prev-hunk", &[Key::plain('[')]),
    (Action::NextFile, "next-file", &[Key::plain('f')]),
    (Action::PrevFile, "prev-file", &[Key::plain('F')]),
    (Action::ScopeUncommitted, "scope-uncommitted", &[Key::plain('u')]),
    (Action::ScopeBranch, "scope-branch", &[Key::plain('b')]),
    (Action::ScopeLastTurn, "scope-last-turn", &[Key::plain('t')]),
    (Action::TabChanges, "tab-changes", &[Key::plain('1')]),
    (Action::TabAllFiles, "tab-all-files", &[Key::plain('2')]),
    (Action::TabPr, "tab-pr", &[Key::plain('3')]),
    (Action::Wrap, "wrap", &[Key::plain('w')]),
    (Action::Preview, "preview", &[Key::plain('m')]),
    (Action::NavigatorPosition, "navigator-position", &[Key::plain('p')]),
    (Action::NavigatorGrow, "navigator-grow", &[Key::plain('<')]),
    (Action::NavigatorShrink, "navigator-shrink", &[Key::plain('>')]),
    (Action::Select, "select", &[Key::plain('v')]),
    (Action::Comment, "comment", &[Key::plain('c')]),
    (Action::Edit, "edit", &[Key::plain('e')]),
    (Action::Delete, "delete", &[Key::plain('d')]),
    (Action::NextComment, "next-comment", &[Key::plain('n')]),
    (Action::PrevComment, "prev-comment", &[Key::plain('N')]),
    (Action::Comments, "comments", &[Key::plain('l')]),
    (Action::Search, "search", &[Key::plain('/')]),
    (Action::Find, "find", &[Key::ctrl('f')]),
    (Action::Keys, "keys", &[Key::plain('?')]),
    (Action::Send, "send", &[Key::plain('s'), Key::plain('S')]),
    (Action::Copy, "copy", &[Key::plain('y'), Key::plain('Y')]),
    (Action::OpenPr, "open-pr", &[Key::plain('o')]),
    (Action::Refresh, "refresh", &[Key::plain('r')]),
    (Action::Quit, "quit", &[Key::plain('q')]),
];

impl Action {
    /// The action's `[keybindings]` name.
    pub fn name(self) -> &'static str {
        ACTIONS.iter().find(|(action, ..)| *action == self).expect("every action listed").1
    }

    /// The action named `name` in `[keybindings]`, if any.
    pub fn by_name(name: &str) -> Option<Self> {
        ACTIONS.iter().find(|(_, n, _)| *n == name).map(|(action, ..)| *action)
    }

    /// The canonical or legacy config name for one action.
    pub fn by_config_name(name: &str) -> Option<Self> {
        match name {
            "list-wider" => Some(Self::NavigatorGrow),
            "list-narrower" => Some(Self::NavigatorShrink),
            _ => Self::by_name(name),
        }
    }

    /// Every action name, in keymap-table order, for the unknown-action error message.
    pub fn names() -> impl Iterator<Item = &'static str> {
        ACTIONS.iter().map(|(_, name, _)| *name)
    }
}

/// One resolved keymap: every action with its bound keys, never empty per action. Built from
/// the defaults, or from the defaults with `[keybindings]` overrides applied ([`resolve`]).
///
/// [`resolve`]: Keymap::resolve
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    bindings: Vec<(Action, Vec<Key>)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            bindings: ACTIONS.iter().map(|(action, _, keys)| (*action, keys.to_vec())).collect(),
        }
    }
}

/// The default keymap, shared by the callers that need one without a validated snapshot: the
/// blocked `App`'s total `keymap()` accessor and the event loop's error-gate `quit` check.
pub fn default_keymap() -> &'static Keymap {
    static DEFAULT: LazyLock<Keymap> = LazyLock::new(Keymap::default);
    &DEFAULT
}

impl Keymap {
    /// Apply `[keybindings]` overrides to the defaults. An overridden action answers exactly its
    /// configured keys; every other action keeps its defaults. A key bound twice anywhere is a
    /// collision (`CFG-KEY-UNIQUE`, `specs/config.md`); the error detail names each action involved.
    pub fn resolve(overrides: &[(Action, Vec<Key>)]) -> Result<Self, String> {
        let mut keymap = Self::default();
        for (action, keys) in overrides {
            if keys.is_empty() {
                return Err(format!("`{}` has no keys", action.name()));
            }
            let slot = keymap
                .bindings
                .iter_mut()
                .find(|(bound, _)| bound == action)
                .expect("every action listed");
            slot.1.clone_from(keys);
        }
        let mut seen: Vec<(Key, Action)> = Vec::new();
        for (action, keys) in &keymap.bindings {
            for &key in keys {
                match seen.iter().find(|(k, _)| *k == key) {
                    Some((_, first)) if first == action => {
                        return Err(format!(
                            "{} is bound twice to `{}`",
                            key.config_str(),
                            action.name()
                        ));
                    }
                    Some((_, first)) => {
                        return Err(format!(
                            "{} is bound to both `{}` and `{}`",
                            key.config_str(),
                            first.name(),
                            action.name()
                        ));
                    }
                    None => seen.push((key, *action)),
                }
            }
        }
        Ok(keymap)
    }

    /// Every action with its bound keys, in keymap-table order.
    #[must_use]
    pub(crate) fn bindings(&self) -> &[(Action, Vec<Key>)] {
        &self.bindings
    }

    /// The action `key` fires, if any.
    #[must_use]
    pub fn action_for(&self, key: Key) -> Option<Action> {
        self.bindings.iter().find(|(_, keys)| keys.contains(&key)).map(|(action, _)| *action)
    }

    /// The action's hint key: the first bound key (`specs/input.md`).
    #[must_use]
    pub fn hint(&self, action: Action) -> Key {
        self.bindings
            .iter()
            .find(|(bound, _)| *bound == action)
            .map(|(_, keys)| keys[0])
            .expect("every action bound")
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, Key, Keymap};

    #[test]
    fn defaults_bind_every_action_and_hint_is_first_key() {
        let keymap = Keymap::default();
        assert_eq!(keymap.action_for(Key::plain('c')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::plain('S')), Some(Action::Send));
        assert_eq!(keymap.action_for(Key::plain('m')), Some(Action::Preview));
        assert_eq!(keymap.action_for(Key::plain('p')), Some(Action::NavigatorPosition));
        assert_eq!(keymap.action_for(Key::plain('x')), None);
        assert_eq!(keymap.action_for(Key::plain('?')), Some(Action::Keys));
        assert_eq!(keymap.hint(Action::Send), Key::plain('s'));
        assert_eq!(keymap.hint(Action::TabPr), Key::plain('3'));
        assert_eq!(Action::by_config_name("list-wider"), Some(Action::NavigatorGrow));
        assert_eq!(Action::by_config_name("list-narrower"), Some(Action::NavigatorShrink));
    }

    #[test]
    fn find_defaults_to_the_ctrl_f_chord() {
        let keymap = Keymap::default();
        assert_eq!(keymap.action_for(Key::ctrl('f')), Some(Action::Find));
        // The bare `f` is `next-file`, unshadowed by the chord.
        assert_eq!(keymap.action_for(Key::plain('f')), Some(Action::NextFile));
        assert_eq!(keymap.hint(Action::Find), Key::ctrl('f'));
        assert_eq!(keymap.hint(Action::Find).to_string(), "ctrl+f");
        assert_eq!(keymap.hint(Action::Find).config_str(), "ctrl+f");
    }

    #[test]
    fn resolve_replaces_only_the_overridden_action() {
        let keymap =
            Keymap::resolve(&[(Action::Comment, vec![Key::plain('c'), Key::plain('ㅊ')])]).unwrap();
        assert_eq!(keymap.action_for(Key::plain('ㅊ')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::plain('c')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::plain('v')), Some(Action::Select));

        let keymap = Keymap::resolve(&[(Action::Send, vec![Key::plain('x')])]).unwrap();
        assert_eq!(keymap.action_for(Key::plain('s')), None);
        assert_eq!(keymap.action_for(Key::plain('S')), None);
        assert_eq!(keymap.action_for(Key::plain('x')), Some(Action::Send));
    }

    #[test]
    fn find_rebinds_to_another_chord_or_a_bare_key() {
        // To another chord.
        let keymap =
            Keymap::resolve(&[(Action::Find, vec![Key { ctrl: false, alt: true, ch: 'x' }])])
                .unwrap();
        assert_eq!(keymap.action_for(Key { ctrl: false, alt: true, ch: 'x' }), Some(Action::Find));
        assert_eq!(keymap.action_for(Key::ctrl('f')), None, "the default chord is freed");

        // And to a bare key, demoting the chord action to a plain character.
        let keymap = Keymap::resolve(&[(Action::Find, vec![Key::plain('x')])]).unwrap();
        assert_eq!(keymap.action_for(Key::plain('x')), Some(Action::Find));
        assert_eq!(keymap.action_for(Key::ctrl('f')), None);
    }

    #[test]
    fn a_freed_default_is_bindable_elsewhere() {
        let keymap = Keymap::resolve(&[
            (Action::Comment, vec![Key::plain('v')]),
            (Action::Select, vec![Key::plain('c')]),
        ])
        .unwrap();
        assert_eq!(keymap.action_for(Key::plain('v')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::plain('c')), Some(Action::Select));
    }

    #[test]
    fn an_empty_key_list_is_an_error_naming_the_action() {
        let error = Keymap::resolve(&[(Action::Quit, vec![])]).unwrap_err();
        assert!(error.contains("`quit`"), "{error}");
    }

    #[test]
    fn collision_names_each_action() {
        let error = Keymap::resolve(&[(Action::Comment, vec![Key::plain('v')])]).unwrap_err();
        assert!(error.contains("`comment`") && error.contains("`select`"), "{error}");

        let error = Keymap::resolve(&[(Action::Comment, vec![Key::plain('c'), Key::plain('c')])])
            .unwrap_err();
        assert!(error.contains("bound twice") && error.contains("`comment`"), "{error}");

        // A chord collides like any other key, named in config syntax.
        let error = Keymap::resolve(&[(Action::Refresh, vec![Key::ctrl('f')])]).unwrap_err();
        assert!(error.contains("ctrl+f") && error.contains("`find`"), "{error}");
    }
}
