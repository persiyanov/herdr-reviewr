//! The rebindable keymap: actions, default keys, `[keybindings]` overrides, and the lookup.

use std::sync::LazyLock;

use crate::app::Tab;

/// A tab's name as an error says it.
fn tab_name(tab: Tab) -> &'static str {
    match tab {
        Tab::Changes => "Changes",
        Tab::AllFiles => "All files",
        Tab::Pr => "PR",
        Tab::Releases => "Releases",
    }
}

/// One rebindable action from the keymap table in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    NextHunk,
    PrevHunk,
    NextFile,
    PrevFile,
    Collapse,
    Expand,
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    ScopeUncommitted,
    ScopeBranch,
    ScopeLastTurn,
    ScopeCommits,
    BasePick,
    CommitPick,
    TabChanges,
    TabAllFiles,
    TabPr,
    TabReleases,
    Wrap,
    Rendered,
    NavigatorPosition,
    NavigatorHide,
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
    GotoLine,
    Keys,
    Send,
    Copy,
    OpenPr,
    CreateRelease,
    Refresh,
    Quit,
    /// Quit and drop unsent comments; its own key, so a held `q` can't answer its own question.
    QuitDiscard,
}

/// A key's base: a printable character or a named key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
}

impl KeyCode {
    /// Every named key, the one list `by_name` and `names` derive from.
    const NAMED: [KeyCode; 6] =
        [Self::Left, Self::Right, Self::Up, Self::Down, Self::PageUp, Self::PageDown];

    /// The config spelling: the bare character, or the named key's lowercase name
    fn name(self) -> String {
        match self {
            Self::Char(ch) => ch.to_string(),
            Self::Left => "left".into(),
            Self::Right => "right".into(),
            Self::Up => "up".into(),
            Self::Down => "down".into(),
            Self::PageUp => "pageup".into(),
            Self::PageDown => "pagedown".into(),
        }
    }

    /// The screen label a hint paints: the character, or the named key's glyph
    fn label(self) -> String {
        match self {
            Self::Char(ch) => ch.to_string(),
            Self::Left => "←".into(),
            Self::Right => "→".into(),
            Self::Up => "↑".into(),
            Self::Down => "↓".into(),
            Self::PageUp => "PageUp".into(),
            Self::PageDown => "PageDown".into(),
        }
    }

    /// The named key called `name` in `[keybindings]`, if any. Exact lowercase names only.
    pub fn by_name(name: &str) -> Option<Self> {
        Self::NAMED.into_iter().find(|code| code.name() == name)
    }

    /// Every named key's config name, for the keybindings value-error message.
    pub fn names() -> impl Iterator<Item = String> {
        Self::NAMED.into_iter().map(Self::name)
    }
}

/// One bound key: a [`KeyCode`], alone or under `ctrl` or `alt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub ctrl: bool,
    pub alt: bool,
    pub code: KeyCode,
}

impl Key {
    /// A bare character, no modifier.
    pub const fn plain(ch: char) -> Self {
        Self { ctrl: false, alt: false, code: KeyCode::Char(ch) }
    }

    /// A `ctrl+<ch>` chord.
    pub const fn ctrl(ch: char) -> Self {
        Self { ctrl: true, alt: false, code: KeyCode::Char(ch) }
    }

    /// A bare named key, no modifier.
    pub const fn named(code: KeyCode) -> Self {
        Self { ctrl: false, alt: false, code }
    }

    /// The `ctrl+`/`alt+` prefixing both spellings share.
    fn prefixed(self, base: String) -> String {
        match (self.ctrl, self.alt) {
            (true, _) => format!("ctrl+{base}"),
            (false, true) => format!("alt+{base}"),
            (false, false) => base,
        }
    }

    /// The config spelling (`ctrl+f`, `pageup`); no `Display`, so callers pick this or `label`.
    pub fn config_str(self) -> String {
        self.prefixed(self.code.name())
    }

    /// The painted hint: the config spelling, a named key as its label (`→`, `PageUp`).
    pub fn label(self) -> String {
        self.prefixed(self.code.label())
    }
}

/// Every action with its config name and default keys, the one table the keymap derives from.
const ACTIONS: [(Action, &str, &[Key]); 46] = [
    (Action::Down, "down", &[Key::plain('j'), Key::named(KeyCode::Down)]),
    (Action::Up, "up", &[Key::plain('k'), Key::named(KeyCode::Up)]),
    (Action::NextHunk, "next-hunk", &[Key::plain(']')]),
    (Action::PrevHunk, "prev-hunk", &[Key::plain('[')]),
    (Action::NextFile, "next-file", &[Key::plain('f')]),
    (Action::PrevFile, "prev-file", &[Key::plain('F')]),
    (Action::Collapse, "collapse", &[Key::named(KeyCode::Left)]),
    (Action::Expand, "expand", &[Key::named(KeyCode::Right)]),
    (Action::PageUp, "page-up", &[Key::named(KeyCode::PageUp)]),
    (Action::PageDown, "page-down", &[Key::named(KeyCode::PageDown)]),
    (Action::HalfUp, "half-up", &[Key::ctrl('u')]),
    (Action::HalfDown, "half-down", &[Key::ctrl('d')]),
    (Action::ScopeUncommitted, "scope-uncommitted", &[Key::plain('u')]),
    (Action::ScopeBranch, "scope-branch", &[Key::plain('b')]),
    (Action::ScopeLastTurn, "scope-last-turn", &[Key::plain('t')]),
    (Action::ScopeCommits, "scope-commits", &[Key::plain('g')]),
    (Action::BasePick, "base-pick", &[Key::plain('B')]),
    (Action::CommitPick, "commit-pick", &[Key::plain('G')]),
    (Action::TabChanges, "tab-changes", &[Key::plain('1')]),
    (Action::TabAllFiles, "tab-all-files", &[Key::plain('2')]),
    (Action::TabPr, "tab-pr", &[Key::plain('3')]),
    (Action::TabReleases, "tab-releases", &[Key::plain('4')]),
    (Action::Wrap, "wrap", &[Key::plain('w')]),
    (Action::Rendered, "rendered", &[Key::plain('m')]),
    (Action::NavigatorPosition, "navigator-position", &[Key::plain('p')]),
    (Action::NavigatorHide, "navigator-hide", &[Key::plain('z')]),
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
    (Action::GotoLine, "goto-line", &[Key::plain(':')]),
    (Action::Keys, "keys", &[Key::plain('?')]),
    (Action::Send, "send", &[Key::plain('s'), Key::plain('S')]),
    (Action::Copy, "copy", &[Key::plain('y'), Key::plain('Y')]),
    (Action::OpenPr, "open-pr", &[Key::plain('o')]),
    (Action::CreateRelease, "create-release", &[Key::plain('c')]),
    (Action::Refresh, "refresh", &[Key::plain('r')]),
    (Action::Quit, "quit", &[Key::plain('q')]),
    (Action::QuitDiscard, "quit-discard", &[Key::plain('Q')]),
];

/// Every tab.
const ALL_TABS: &[Tab] = &[Tab::Changes, Tab::AllFiles, Tab::Pr, Tab::Releases];
/// The tabs with a file list and a diff.
const FILE_TABS: &[Tab] = &[Tab::Changes, Tab::AllFiles];

impl Action {
    /// The tabs this action acts on, as `dispatch_key` routes it; its keys bind only there.
    #[must_use]
    pub fn tabs(self) -> &'static [Tab] {
        use Action as A;
        match self {
            // The `PR` and `Releases` key branches handle these too; send, copy and quit-discard
            // answer the quit question, which every tab asks.
            A::Down
            | A::Up
            | A::PageUp
            | A::PageDown
            | A::Expand
            | A::Collapse
            | A::NavigatorPosition
            | A::NavigatorGrow
            | A::NavigatorShrink
            | A::TabChanges
            | A::TabAllFiles
            | A::TabPr
            | A::TabReleases
            | A::Search
            | A::Keys
            | A::Send
            | A::Copy
            | A::Refresh
            | A::Quit
            | A::QuitDiscard => ALL_TABS,
            A::NextHunk
            | A::PrevHunk
            | A::NextFile
            | A::PrevFile
            | A::HalfUp
            | A::HalfDown
            | A::ScopeUncommitted
            | A::ScopeBranch
            | A::ScopeLastTurn
            | A::ScopeCommits
            | A::BasePick
            | A::CommitPick
            | A::Wrap
            | A::Rendered
            | A::NavigatorHide
            | A::Select
            | A::Comment
            | A::Edit
            | A::Delete
            | A::NextComment
            | A::PrevComment
            | A::Comments
            | A::Find
            | A::GotoLine => FILE_TABS,
            A::OpenPr => &[Tab::Pr],
            A::CreateRelease => &[Tab::Releases],
        }
    }

    /// The first tab both actions act on: where their keys would collide.
    fn shared_tab(self, other: Self) -> Option<Tab> {
        self.tabs().iter().copied().find(|tab| other.tabs().contains(tab))
    }

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
            "preview" => Some(Self::Rendered),
            _ => Self::by_name(name),
        }
    }

    /// Every action name, in keymap-table order, for the unknown-action error message.
    pub fn names() -> impl Iterator<Item = &'static str> {
        ACTIONS.iter().map(|(_, name, _)| *name)
    }
}

/// Every action with at least one key: the defaults, overridden by `[keybindings]`.
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

/// The default keymap, for callers with no valid config.
pub fn default_keymap() -> &'static Keymap {
    static DEFAULT: LazyLock<Keymap> = LazyLock::new(Keymap::default);
    &DEFAULT
}

impl Keymap {
    /// Apply overrides to the defaults; a key bound twice is an error naming both actions.
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
                // Two actions share a key only when no tab has both.
                let collides = |(k, other): &&(Key, Action)| {
                    *k == key && (other == action || other.shared_tab(*action).is_some())
                };
                match seen.iter().find(collides) {
                    Some((_, first)) if first == action => {
                        return Err(format!(
                            "`{}` is bound twice to `{}`",
                            key.config_str(),
                            action.name()
                        ));
                    }
                    Some((_, first)) => {
                        let tab = first.shared_tab(*action).expect("a collision shares a tab");
                        return Err(format!(
                            "`{}` is bound to both `{}` and `{}` on the {} tab",
                            key.config_str(),
                            first.name(),
                            action.name(),
                            tab_name(tab)
                        ));
                    }
                    None => seen.push((key, *action)),
                }
            }
        }
        Ok(keymap)
    }

    /// Every action with its bound keys, in keymap-table order.
    #[cfg(test)]
    pub(crate) fn bindings(&self) -> &[(Action, Vec<Key>)] {
        &self.bindings
    }

    /// The action `key` fires on `tab`, if any.
    #[must_use]
    pub fn action_on(&self, tab: Tab, key: Key) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(action, keys)| keys.contains(&key) && action.tabs().contains(&tab))
            .map(|(action, _)| *action)
    }

    /// The action `key` fires on some tab, for tests of keys no two actions share.
    #[cfg(test)]
    pub(crate) fn action_for(&self, key: Key) -> Option<Action> {
        self.bindings.iter().find(|(_, keys)| keys.contains(&key)).map(|(action, _)| *action)
    }

    /// The action's hint key: the first bound key.
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
    use super::{ACTIONS, Action, Key, KeyCode, Keymap};
    use crate::app::Tab;

    #[test]
    fn defaults_bind_every_action_and_hint_is_first_key() {
        let keymap = Keymap::default();
        assert_eq!(keymap.action_for(Key::plain('c')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::plain('S')), Some(Action::Send));
        assert_eq!(keymap.action_for(Key::plain('m')), Some(Action::Rendered));
        assert_eq!(keymap.action_for(Key::plain('p')), Some(Action::NavigatorPosition));
        assert_eq!(keymap.action_for(Key::plain('z')), Some(Action::NavigatorHide));
        assert_eq!(keymap.action_for(Key::plain('x')), None);
        assert_eq!(keymap.action_for(Key::plain('g')), Some(Action::ScopeCommits));
        assert_eq!(keymap.action_for(Key::plain('G')), Some(Action::CommitPick));
        assert_eq!(keymap.action_for(Key::plain('?')), Some(Action::Keys));
        assert_eq!(keymap.hint(Action::Send), Key::plain('s'));
        assert_eq!(keymap.hint(Action::TabPr), Key::plain('3'));
        assert_eq!(keymap.action_for(Key::plain('4')), Some(Action::TabReleases));
        assert_eq!(keymap.action_on(Tab::Releases, Key::plain('c')), Some(Action::CreateRelease));
        assert_eq!(keymap.action_on(Tab::Changes, Key::plain('c')), Some(Action::Comment));
        assert_eq!(keymap.action_for(Key::named(KeyCode::Right)), Some(Action::Expand));
        assert_eq!(keymap.action_for(Key::named(KeyCode::Left)), Some(Action::Collapse));
        assert_eq!(keymap.action_for(Key::named(KeyCode::Down)), Some(Action::Down));
        assert_eq!(keymap.action_for(Key::named(KeyCode::Up)), Some(Action::Up));
        assert_eq!(keymap.action_for(Key::named(KeyCode::PageUp)), Some(Action::PageUp));
        assert_eq!(keymap.action_for(Key::named(KeyCode::PageDown)), Some(Action::PageDown));
        assert_eq!(keymap.action_for(Key::ctrl('u')), Some(Action::HalfUp));
        assert_eq!(keymap.action_for(Key::ctrl('d')), Some(Action::HalfDown));
        // The hint stays the first bound key: `j` for `down`, the named key for `expand`.
        assert_eq!(keymap.hint(Action::Down), Key::plain('j'));
        assert_eq!(keymap.hint(Action::Expand), Key::named(KeyCode::Right));
    }

    #[test]
    fn a_key_fires_the_action_bound_to_it_on_that_tab() {
        let keymap = Keymap::default();
        let c = Key::plain('c');
        assert_eq!(keymap.action_on(Tab::Changes, c), Some(Action::Comment));
        assert_eq!(keymap.action_on(Tab::AllFiles, c), Some(Action::Comment));
        assert_eq!(keymap.action_on(Tab::Releases, c), Some(Action::CreateRelease));
        assert_eq!(keymap.action_on(Tab::Pr, c), None, "the PR tab comments nowhere");
        assert_eq!(keymap.action_on(Tab::Pr, Key::plain('o')), Some(Action::OpenPr));
        assert_eq!(keymap.action_on(Tab::Changes, Key::plain('o')), None);
        for tab in [Tab::Changes, Tab::AllFiles, Tab::Pr, Tab::Releases] {
            assert_eq!(keymap.action_on(tab, Key::plain('q')), Some(Action::Quit), "{tab:?}");
            assert_eq!(keymap.action_on(tab, Key::plain('4')), Some(Action::TabReleases));
        }
    }

    #[test]
    fn every_default_key_fires_its_action_on_every_tab_it_acts_on() {
        let keymap = Keymap::default();
        for (action, _, keys) in ACTIONS {
            for &tab in action.tabs() {
                for &key in keys {
                    assert_eq!(keymap.action_on(tab, key), Some(action), "{key:?} on {tab:?}");
                }
            }
        }
    }

    #[test]
    fn a_shared_key_collides_only_on_a_tab_with_both_actions() {
        // Open-PR and create-release never share a tab, so one key serves both.
        let keymap = Keymap::resolve(&[(Action::CreateRelease, vec![Key::plain('o')])]).unwrap();
        assert_eq!(keymap.action_on(Tab::Releases, Key::plain('o')), Some(Action::CreateRelease));
        assert_eq!(keymap.action_on(Tab::Pr, Key::plain('o')), Some(Action::OpenPr));
        // A global action shares every tab, so it collides with either.
        let error = Keymap::resolve(&[(Action::Refresh, vec![Key::plain('c')])]).unwrap_err();
        assert!(error.contains("`comment`") && error.contains("`refresh`"), "{error}");
        assert!(error.contains("on the Changes tab"), "the error names the tab: {error}");
        let error = Keymap::resolve(&[(Action::OpenPr, vec![Key::plain('r')])]).unwrap_err();
        assert!(error.contains("on the PR tab"), "{error}");
        let error = Keymap::resolve(&[(Action::Comment, vec![Key::plain('o')])]);
        assert!(error.is_ok(), "comment and open-PR share no tab: {error:?}");
    }

    #[test]
    fn a_named_key_spells_its_name_in_config_and_its_label_on_screen() {
        let keymap = Keymap::default();
        assert_eq!(keymap.hint(Action::Expand).config_str(), "right");
        assert_eq!(keymap.hint(Action::Expand).label(), "→");
        assert_eq!(keymap.hint(Action::Collapse).label(), "←");
        assert_eq!(keymap.hint(Action::PageUp).config_str(), "pageup");
        assert_eq!(keymap.hint(Action::PageUp).label(), "PageUp");
        assert_eq!(keymap.hint(Action::PageDown).label(), "PageDown");
        assert_eq!(Key { ctrl: true, alt: false, code: KeyCode::Right }.config_str(), "ctrl+right");
        // A character key labels as itself, chords included.
        assert_eq!(Key::ctrl('u').label(), "ctrl+u");
        assert_eq!(Action::by_config_name("list-wider"), Some(Action::NavigatorGrow));
        assert_eq!(Action::by_config_name("list-narrower"), Some(Action::NavigatorShrink));
        assert_eq!(Action::by_config_name("preview"), Some(Action::Rendered));
    }

    #[test]
    fn find_defaults_to_the_ctrl_f_chord() {
        let keymap = Keymap::default();
        assert_eq!(keymap.action_for(Key::ctrl('f')), Some(Action::Find));
        // The bare `f` is `next-file`, unshadowed by the chord.
        assert_eq!(keymap.action_for(Key::plain('f')), Some(Action::NextFile));
        assert_eq!(keymap.hint(Action::Find), Key::ctrl('f'));
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
        let alt_x = Key { ctrl: false, alt: true, code: KeyCode::Char('x') };
        let keymap = Keymap::resolve(&[(Action::Find, vec![alt_x])]).unwrap();
        assert_eq!(keymap.action_for(alt_x), Some(Action::Find));
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
