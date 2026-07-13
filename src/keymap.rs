//! The rebindable action keymap: action names, default keys, resolution of `[keybindings]`
//! overrides, and the char → action lookup the dispatcher and the hint renderers share
//! (`specs/tui.md` Interaction, `specs/config.md` Keybindings).

use std::sync::LazyLock;

/// One rebindable character-shortcut action from the keymap table in `specs/tui.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    ScopeUncommitted,
    ScopeBranch,
    ScopeLastTurn,
    TabChanges,
    TabAllFiles,
    TabPr,
    Wrap,
    Preview,
    ListWider,
    ListNarrower,
    Select,
    Comment,
    Edit,
    Delete,
    NextComment,
    PrevComment,
    Comments,
    Send,
    Copy,
    OpenPr,
    Refresh,
    Quit,
}

/// Every action with its config name and default keys — the single source the default keymap,
/// the name lookup, and the config error message are built from.
const ACTIONS: [(Action, &str, &[char]); 24] = [
    (Action::Down, "down", &['j']),
    (Action::Up, "up", &['k']),
    (Action::ScopeUncommitted, "scope-uncommitted", &['u']),
    (Action::ScopeBranch, "scope-branch", &['b']),
    (Action::ScopeLastTurn, "scope-last-turn", &['t']),
    (Action::TabChanges, "tab-changes", &['1']),
    (Action::TabAllFiles, "tab-all-files", &['2']),
    (Action::TabPr, "tab-pr", &['3']),
    (Action::Wrap, "wrap", &['w']),
    (Action::Preview, "preview", &['m']),
    (Action::ListWider, "list-wider", &[']']),
    (Action::ListNarrower, "list-narrower", &['[']),
    (Action::Select, "select", &['v']),
    (Action::Comment, "comment", &['c']),
    (Action::Edit, "edit", &['e']),
    (Action::Delete, "delete", &['d']),
    (Action::NextComment, "next-comment", &['n']),
    (Action::PrevComment, "prev-comment", &['N']),
    (Action::Comments, "comments", &['l']),
    (Action::Send, "send", &['s', 'S']),
    (Action::Copy, "copy", &['y', 'Y']),
    (Action::OpenPr, "open-pr", &['o']),
    (Action::Refresh, "refresh", &['r']),
    (Action::Quit, "quit", &['q']),
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

    /// Every action name, in keymap-table order, for the unknown-action error message.
    pub fn names() -> impl Iterator<Item = &'static str> {
        ACTIONS.iter().map(|(_, name, _)| *name)
    }
}

/// One resolved keymap: every action with its bound characters, never empty per action. Built
/// from the defaults, or from the defaults with `[keybindings]` overrides applied ([`resolve`]).
///
/// [`resolve`]: Keymap::resolve
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    bindings: Vec<(Action, Vec<char>)>,
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
    /// configured keys; every other action keeps its defaults. A character bound twice anywhere
    /// is a collision (K2, `specs/config.md`); the error detail names each action involved.
    pub fn resolve(overrides: &[(Action, Vec<char>)]) -> Result<Self, String> {
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
        let mut seen: Vec<(char, Action)> = Vec::new();
        for (action, keys) in &keymap.bindings {
            for &key in keys {
                match seen.iter().find(|(c, _)| *c == key) {
                    Some((_, first)) if first == action => {
                        return Err(format!("{key:?} is bound twice to `{}`", action.name()));
                    }
                    Some((_, first)) => {
                        return Err(format!(
                            "{key:?} is bound to both `{}` and `{}`",
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
    pub(crate) fn bindings(&self) -> &[(Action, Vec<char>)] {
        &self.bindings
    }

    /// The action `key` fires, if any.
    #[must_use]
    pub fn action_for(&self, key: char) -> Option<Action> {
        self.bindings.iter().find(|(_, keys)| keys.contains(&key)).map(|(action, _)| *action)
    }

    /// The action's hint key: the first bound character (`specs/tui.md`).
    #[must_use]
    pub fn hint(&self, action: Action) -> char {
        self.bindings
            .iter()
            .find(|(bound, _)| *bound == action)
            .map(|(_, keys)| keys[0])
            .expect("every action bound")
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, Keymap};

    #[test]
    fn defaults_bind_every_action_and_hint_is_first_key() {
        let keymap = Keymap::default();
        assert_eq!(keymap.action_for('c'), Some(Action::Comment));
        assert_eq!(keymap.action_for('S'), Some(Action::Send));
        assert_eq!(keymap.action_for('m'), Some(Action::Preview));
        assert_eq!(keymap.action_for('x'), None);
        assert_eq!(keymap.hint(Action::Send), 's');
        assert_eq!(keymap.hint(Action::TabPr), '3');
    }

    #[test]
    fn resolve_replaces_only_the_overridden_action() {
        let keymap = Keymap::resolve(&[(Action::Comment, vec!['c', 'ㅊ'])]).unwrap();
        assert_eq!(keymap.action_for('ㅊ'), Some(Action::Comment));
        assert_eq!(keymap.action_for('c'), Some(Action::Comment));
        assert_eq!(keymap.action_for('v'), Some(Action::Select));

        let keymap = Keymap::resolve(&[(Action::Send, vec!['x'])]).unwrap();
        assert_eq!(keymap.action_for('s'), None);
        assert_eq!(keymap.action_for('S'), None);
        assert_eq!(keymap.action_for('x'), Some(Action::Send));
    }

    #[test]
    fn a_freed_default_is_bindable_elsewhere() {
        let keymap =
            Keymap::resolve(&[(Action::Comment, vec!['v']), (Action::Select, vec!['c'])]).unwrap();
        assert_eq!(keymap.action_for('v'), Some(Action::Comment));
        assert_eq!(keymap.action_for('c'), Some(Action::Select));
    }

    #[test]
    fn an_empty_key_list_is_an_error_naming_the_action() {
        let error = Keymap::resolve(&[(Action::Quit, vec![])]).unwrap_err();
        assert!(error.contains("`quit`"), "{error}");
    }

    #[test]
    fn collision_names_each_action() {
        let error = Keymap::resolve(&[(Action::Comment, vec!['v'])]).unwrap_err();
        assert!(error.contains("`comment`") && error.contains("`select`"), "{error}");

        let error = Keymap::resolve(&[(Action::Comment, vec!['c', 'c'])]).unwrap_err();
        assert!(error.contains("bound twice") && error.contains("`comment`"), "{error}");
    }
}
