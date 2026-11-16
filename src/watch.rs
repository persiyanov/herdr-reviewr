//! The watcher: what changed in the worktree, the git files reviewr reads, and the plugin config,
//! as batches. Ignored paths and git's own noise drop; a dead watcher restarts itself.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::schedule::Backoff;
use crate::turn::{TurnFeed, TurnNews, Wrote};

/// How long a batch gathers after its first event. `FSEvents` itself coalesces for the same span.
pub(crate) const GATHER: Duration = Duration::from_millis(50);
/// Past this many cached verdicts a cache starts over, so it never grows with a session.
const CACHE_CAP: usize = 100_000;
/// `FSEvents` can refuse a stream while others start in the same instant: a start retries this often.
const START_ATTEMPTS: u32 = 5;
/// The name prefix of the cookie files watchman's git integration writes into the worktree.
const WATCHMAN_COOKIE: &str = ".watchman-cookie-";

/// A change in the git files reviewr reads, by what it moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitChange {
    /// The index: a stage, or a stat-only rewrite by someone's `git status`.
    Index,
    /// `HEAD`: a commit or a checkout.
    Head,
    /// A ref reviewr reads: this worktree's branch, its base, remotes, tags, `packed-refs`.
    Refs,
    /// Repository or worktree config: remotes, upstreams, excludes.
    Config,
    /// An ignore rule: what counts as untracked or ignored.
    IgnoreRules,
    /// An attribute rule: what counts as binary or diffable.
    Attributes,
    /// A private ref under `refs/worktree/reviewr/` another process wrote; reviewr's own writes drop.
    ReviewrRefs,
}

/// What changed since the last batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    /// Worktree paths, `/`-separated; a path inside a nested repository is that repository's entry.
    pub worktree: BTreeSet<String>,
    /// The same changes by the paths themselves, inside nested repositories too: what search indexes.
    pub files: BTreeSet<String>,
    pub git: BTreeSet<GitChange>,
    /// Events were lost: only a full rebuild is safe.
    pub rescan: bool,
    /// The plugin config changed: the config is read again.
    pub config: bool,
}

impl GitChange {
    /// Whether it moves what the PR tab's local probe reads: HEAD, refs, or git config.
    pub fn moves_pr_input(self) -> bool {
        matches!(self, Self::Head | Self::Refs | Self::Config)
    }
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.worktree.is_empty() && self.git.is_empty() && !self.rescan && !self.config
    }
}

/// What the watcher reports.
#[derive(Debug)]
pub enum WatchEvent {
    Batch(Batch),
    /// The watcher started. After a restart a batch asking for everything follows.
    Ready,
    /// No events come until it restarts: it failed, died, or watches a network or FUSE mount.
    Unavailable(String),
}

enum Msg {
    /// An event, stamped when notify delivered it, not when this thread got to it.
    Raw(Instant, notify::Result<Event>),
    Shown(BTreeSet<String>),
    Refs(BTreeSet<String>),
    Visible(bool),
    /// The handle dropped; the channel never closes on its own, since notify's handler holds a sender.
    Stop,
}

/// The handle. Dropping it stops the thread and releases its stream (macOS allows a process only
/// a few live `FSEvents` streams).
#[derive(Debug)]
pub struct Watch {
    control: mpsc::Sender<Msg>,
    /// What the thread was last told, so a repeat sends nothing.
    shown: BTreeSet<String>,
    refs: BTreeSet<String>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.control.send(Msg::Stop);
    }
}

impl Watch {
    /// Watch `repo`, and the plugin config directory `config` when there is one; report to `tx`.
    /// Real changes also go to turn tracking through `feed`, never through the frame loop.
    pub fn start(
        repo: &Path,
        config: Option<&Path>,
        tx: crate::wake::Sender<WatchEvent>,
        feed: Option<TurnFeed>,
    ) -> Self {
        let (control, rx) = mpsc::channel();
        let (repo, raw, config) =
            (repo.to_path_buf(), control.clone(), config.map(Path::to_path_buf));
        let spawned = std::thread::Builder::new().name("watch".into()).spawn(move || {
            Thread { repo, config, tx, rx, raw, feed, kept: Kept::default() }.run();
        });
        if let Err(e) = spawned {
            logln!("watch thread failed to start: {e}");
            let _ = control.send(Msg::Stop);
        }
        Self { control, shown: BTreeSet::new(), refs: BTreeSet::new() }
    }

    /// The ignored entries on screen (expanded ignored directories, an open ignored file): their
    /// changes report like any other.
    pub fn show(&mut self, shown: BTreeSet<String>) {
        if shown != self.shown {
            self.shown.clone_from(&shown);
            let _ = self.control.send(Msg::Shown(shown));
        }
    }

    /// Full ref names whose moves matter beyond this branch, remotes and tags: a local base branch.
    pub fn set_refs(&mut self, refs: BTreeSet<String>) {
        if refs != self.refs {
            self.refs.clone_from(&refs);
            let _ = self.control.send(Msg::Refs(refs));
        }
    }

    /// Whether the pane is on screen: a watcher that died restarts only while it is.
    pub fn set_visible(&self, visible: bool) {
        let _ = self.control.send(Msg::Visible(visible));
    }
}

/// What the loop told the watcher, kept across restarts.
#[derive(Debug)]
struct Kept {
    shown: BTreeSet<String>,
    refs: BTreeSet<String>,
    visible: bool,
}

impl Default for Kept {
    fn default() -> Self {
        Self { shown: BTreeSet::new(), refs: BTreeSet::new(), visible: true }
    }
}

/// How one watching stretch ended.
enum Ended {
    Stop,
    /// It failed; `retry` is false when it never can work here (a network filesystem).
    Failed {
        reason: String,
        retry: bool,
    },
}

/// The watcher thread: watching stretches, and the waits between them.
struct Thread {
    repo: PathBuf,
    config: Option<PathBuf>,
    tx: crate::wake::Sender<WatchEvent>,
    rx: mpsc::Receiver<Msg>,
    raw: mpsc::Sender<Msg>,
    feed: Option<TurnFeed>,
    kept: Kept,
}

impl Thread {
    fn run(&mut self) {
        let mut backoff = Backoff::default();
        let mut restarted = false;
        loop {
            // A stretch that panics fails like any other and restarts; the panic hook logs it.
            let watched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.watch(restarted, &mut backoff)
            }));
            let (reason, retry) = match watched {
                Ok(Ended::Stop) => return,
                Ok(Ended::Failed { reason, retry }) => (reason, retry),
                Err(_) => ("the watcher crashed".to_string(), true),
            };
            if let Some(feed) = &self.feed {
                feed.send(TurnNews::WatcherDown(true));
            }
            if self.tx.send(WatchEvent::Unavailable(reason)).is_err() {
                return;
            }
            // One that died retries after the backoff, counted while the pane is on screen.
            let until = retry.then(|| Instant::now() + backoff.next_wait());
            if !self.wait_to_restart(until) {
                return;
            }
            restarted = true;
        }
    }

    /// Take control messages until `until` has passed with the pane on screen; `None` waits for the
    /// stop alone. `false` on a stop.
    fn wait_to_restart(&mut self, until: Option<Instant>) -> bool {
        loop {
            let msg = match until {
                Some(at) if self.kept.visible => {
                    match self.rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Timeout) => return true,
                        Err(RecvTimeoutError::Disconnected) => return false,
                    }
                }
                _ => match self.rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return false,
                },
            };
            match msg {
                Msg::Stop => return false,
                Msg::Shown(shown) => self.kept.shown = shown,
                Msg::Refs(refs) => self.kept.refs = refs,
                Msg::Visible(visible) => self.kept.visible = visible,
                Msg::Raw(..) => {}
            }
        }
    }

    /// One watching stretch, until it fails or stops.
    fn watch(&mut self, restarted: bool, backoff: &mut Backoff) -> Ended {
        let root = match self.repo.canonicalize() {
            Ok(root) => root,
            Err(e) => {
                let reason = format!("{}: {e}", self.repo.display());
                return Ended::Failed { reason, retry: true };
            }
        };
        if let Some(kind) = remote_filesystem(&root) {
            let reason = format!("{kind} filesystem delivers no events");
            return Ended::Failed { reason, retry: false };
        }
        let git = crate::git::git_dirs(&root);
        // A repository whose git dirs would not resolve is retried, never watched as a plain folder.
        if git.is_none() && root.join(".git").exists() {
            let reason = format!("{}: git did not name its git dir", root.display());
            return Ended::Failed { reason, retry: true };
        }
        let excludes = git.as_ref().and_then(|_| crate::git::global_excludes(&root));
        let layout = Layout {
            git,
            config: self.config.as_deref().map(ConfigFiles::locate),
            excludes: excludes.map(|e| canonical_beside(&e)),
            root,
        };
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
        let (mut watcher, placed) = match start_watcher(&layout, &self.raw) {
            Ok(started) => started,
            Err(reason) => return Ended::Failed { reason, retry: true },
        };
        if self.tx.send(WatchEvent::Ready).is_err() {
            return Ended::Stop;
        }
        if let Some(feed) = &self.feed {
            feed.send(TurnNews::WatcherDown(false));
        }
        let mut state = State {
            layout,
            ignored: HashMap::new(),
            nested: HashMap::new(),
            present: HashSet::new(),
            #[cfg(target_os = "linux")]
            watches: placed,
        };
        #[cfg(target_os = "linux")]
        {
            let mut failed = None;
            let none = BTreeSet::new();
            let root = &state.layout.root;
            state.watches.show(&mut watcher, root, &none, &self.kept.shown, &mut failed);
            if let Some(reason) = failed {
                return Ended::Failed { reason, retry: true };
            }
        }
        // Placed in full, shown paths included: only now does the next failure start over.
        *backoff = Backoff::default();
        // Whatever changed while no watcher ran was never seen.
        if restarted {
            let everything = Batch {
                rescan: true,
                config: true,
                git: [GitChange::Head, GitChange::Refs].into(),
                ..Batch::default()
            };
            if self.tx.send(WatchEvent::Batch(everything)).is_err() {
                return Ended::Stop;
            }
        }
        let mut pending: Vec<(Instant, Event)> = Vec::new();
        let mut deadline: Option<Instant> = None;
        loop {
            let msg = match deadline {
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(at) => self.rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            };
            match msg {
                // A read is no change, and on Linux the watcher's own reads would feed it forever.
                Ok(Msg::Raw(_, Ok(event))) if matches!(event.kind, EventKind::Access(_)) => {}
                Ok(Msg::Raw(at, Ok(event))) => {
                    pending.push((at, event));
                    deadline.get_or_insert_with(|| Instant::now() + GATHER);
                }
                Ok(Msg::Raw(_, Err(e))) => {
                    return Ended::Failed { reason: e.to_string(), retry: true };
                }
                Ok(Msg::Shown(shown)) => {
                    #[cfg(target_os = "linux")]
                    {
                        let mut failed = None;
                        let root = &state.layout.root;
                        state.watches.show(
                            &mut watcher,
                            root,
                            &self.kept.shown,
                            &shown,
                            &mut failed,
                        );
                        if let Some(reason) = failed {
                            self.kept.shown = shown;
                            return Ended::Failed { reason, retry: true };
                        }
                    }
                    self.kept.shown = shown;
                }
                Ok(Msg::Refs(refs)) => self.kept.refs = refs,
                Ok(Msg::Visible(visible)) => self.kept.visible = visible,
                Err(RecvTimeoutError::Timeout) => {
                    deadline = None;
                    let events = std::mem::take(&mut pending);
                    let mut failed = None;
                    let (batch, wrote) =
                        state.batch(&events, &mut watcher, &self.kept, &mut failed);
                    if let (Some(feed), Some(wrote)) = (&self.feed, wrote) {
                        feed.send(TurnNews::Wrote(wrote));
                    }
                    if !batch.is_empty() && self.tx.send(WatchEvent::Batch(batch)).is_err() {
                        return Ended::Stop;
                    }
                    // What the batch saw still went out; a watch it could not place fails the watcher.
                    if let Some(reason) = failed {
                        return Ended::Failed { reason, retry: true };
                    }
                }
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return Ended::Stop,
            }
        }
    }
}

/// `path` with its directory resolved, even when `path` itself does not exist yet.
fn canonical_beside(path: &Path) -> PathBuf {
    match (path.parent().and_then(|p| p.canonicalize().ok()), path.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

/// The plugin config, as the watcher finds it on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigFiles {
    /// The config directory, resolved through its nearest existing ancestor.
    dir: PathBuf,
    /// The nearest directory that exists; a change there may be the config directory appearing.
    watched: PathBuf,
    /// Where `config.toml` really lives, when it is a symlink out of the directory.
    target: Option<PathBuf>,
}

impl ConfigFiles {
    fn locate(dir: &Path) -> Self {
        let mut watched = dir.to_path_buf();
        let mut missing = Vec::new();
        while !watched.is_dir() {
            match (watched.file_name(), watched.parent()) {
                (Some(name), Some(parent)) => {
                    missing.push(name.to_os_string());
                    watched = parent.to_path_buf();
                }
                _ => break,
            }
        }
        let watched = crate::git::canonical(&watched);
        let dir = missing.iter().rev().fold(watched.clone(), |d, name| d.join(name));
        let file = dir.join("config.toml");
        let target =
            std::fs::canonicalize(&file).ok().filter(|t| t.parent() != Some(dir.as_path()));
        Self { dir, watched, target }
    }
}

/// Where things live.
#[derive(Clone, Debug)]
struct Layout {
    root: PathBuf,
    /// The worktree's git directory and the common directory, when `root` is a repository.
    git: Option<(PathBuf, PathBuf)>,
    config: Option<ConfigFiles>,
    /// The global ignore file git reads.
    excludes: Option<PathBuf>,
}

/// One raw path, classified.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Class {
    Worktree(String),
    Config,
    Git(GitChange),
    /// A ref by full name (`refs/heads/main`), kept or dropped once the batch knows `HEAD`.
    Ref(String),
    Drop,
}

/// Classify one event path by where it is, never by its content.
fn classify(layout: &Layout, path: &Path) -> Class {
    if let Some(config) = &layout.config
        && (path.starts_with(&config.dir)
            || config.dir.starts_with(path)
            || config.target.as_deref() == Some(path))
    {
        return Class::Config;
    }
    if layout.excludes.as_deref() == Some(path) {
        return Class::Git(GitChange::IgnoreRules);
    }
    if let Some((dir, common)) = &layout.git {
        if let Ok(rel) = path.strip_prefix(dir) {
            let class = classify_git_dir(rel);
            if class != Class::Drop || dir != common {
                return class;
            }
            return classify_common_dir(rel);
        }
        if let Ok(rel) = path.strip_prefix(common) {
            return classify_common_dir(rel);
        }
    }
    let Ok(rel) = path.strip_prefix(&layout.root) else { return Class::Drop };
    let rel = slash(rel);
    // The root itself, anything inside a nested repository's `.git`, and watchman's cookies.
    let name = rel.rsplit('/').next().unwrap_or_default();
    if rel.is_empty() || rel.split('/').any(|c| c == ".git") || name.starts_with(WATCHMAN_COOKIE) {
        return Class::Drop;
    }
    Class::Worktree(rel)
}

/// Inside this worktree's own git directory; reviewr's index copies under `reviewr/` drop.
fn classify_git_dir(rel: &Path) -> Class {
    let rel = slash(rel);
    if is_lock(&rel) {
        return Class::Drop;
    }
    match rel.as_str() {
        "HEAD" => Class::Git(GitChange::Head),
        "index" => Class::Git(GitChange::Index),
        "config.worktree" => Class::Git(GitChange::Config),
        _ if rel.starts_with("refs/") => Class::Ref(rel),
        // A reftable repository keeps its refs, and a linked worktree its own HEAD, there.
        _ if rel.starts_with("reftable/") => Class::Git(GitChange::Refs),
        _ => Class::Drop,
    }
}

/// Inside the common directory: shared refs, config and rules. Other worktrees' entries, objects,
/// logs, hooks and locks drop.
fn classify_common_dir(rel: &Path) -> Class {
    let rel = slash(rel);
    if is_lock(&rel) {
        return Class::Drop;
    }
    match rel.as_str() {
        "packed-refs" => Class::Git(GitChange::Refs),
        "config" => Class::Git(GitChange::Config),
        "info/exclude" => Class::Git(GitChange::IgnoreRules),
        "info/attributes" => Class::Git(GitChange::Attributes),
        _ if rel.starts_with("refs/") => Class::Ref(rel),
        _ if rel.starts_with("reftable/") => Class::Git(GitChange::Refs),
        _ => Class::Drop,
    }
}

/// Git's lock files (`index.lock`): git renames one over the file it replaces, which reports itself.
fn is_lock(rel: &str) -> bool {
    Path::new(rel).extension().is_some_and(|e| e == "lock")
}

fn slash(rel: &Path) -> String {
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// Whether a moved ref is one reviewr reads: this branch, a remote, a tag, or one the app named.
/// The `refs/remotes` and `refs/tags` directories count too: a first fetch creates them.
fn ref_matters(name: &str, head_branch: Option<&str>, extra: &BTreeSet<String>) -> bool {
    let under = |dir| crate::git::under_or_eq(name, dir);
    Some(name) == head_branch || under("refs/remotes") || under("refs/tags") || extra.contains(name)
}

/// The branch `HEAD` names, read straight from the file: `ref: refs/heads/main`.
fn head_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: ").map(str::to_string)
}

/// A rename, creation or removal by the event's word. `FSEvents` accumulates a path's flags, so
/// whether an entry appeared or went is decided by looking, not by this.
fn structural(kind: EventKind) -> bool {
    matches!(kind, EventKind::Create(_) | EventKind::Remove(_))
        || matches!(kind, EventKind::Modify(notify::event::ModifyKind::Name(_)))
}

/// A path's kind as git sees it: a link is a file, whatever it points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Folder,
    File,
}

/// The folders above `raw` inside the nested repository at `entry`, spelled `entry/a/`.
fn nested_folders<'a>(entry: &'a str, raw: &str) -> impl Iterator<Item = String> + 'a {
    let mut chain = ancestors_and_self(&raw[entry.len() + 1..]);
    chain.pop();
    chain.into_iter().map(move |rel| format!("{entry}/{rel}/"))
}

/// A path as git is asked about it: `build/` for a folder, `build` for a file.
fn spelled(path: &str, kind: Kind) -> String {
    match kind {
        Kind::Folder => format!("{path}/"),
        Kind::File => path.to_string(),
    }
}

/// What is at `path` now, without following a link; `None` when nothing is.
fn kind_on_disk(path: &Path) -> Option<Kind> {
    let meta = path.symlink_metadata().ok()?;
    Some(if meta.is_dir() { Kind::Folder } else { Kind::File })
}

/// One watching stretch's state between batches.
struct State {
    layout: Layout,
    /// Whether git ignores a path that exists, spelled as what it is (`build/` a folder, `build` a
    /// file); a gone path has no verdict and reports as a change. Every read asks git again.
    ignored: HashMap<String, bool>,
    /// Directory to whether it is the root of a nested repository or submodule.
    nested: HashMap<String, bool>,
    /// Ignored entries now there, so one shows only when it appears: macOS can flag a plain edit
    /// as a creation.
    present: HashSet<String>,
    #[cfg(target_os = "linux")]
    watches: linux::Watches,
}

/// What placing the watches leaves to keep: on Linux, the folders the walk watched.
#[cfg(target_os = "linux")]
type Placed = linux::Watches;
#[cfg(not(target_os = "linux"))]
type Placed = ();

/// Create the watcher and place its watches, retrying a refused start.
fn start_watcher(
    layout: &Layout,
    raw: &mpsc::Sender<Msg>,
) -> Result<(notify::RecommendedWatcher, Placed), String> {
    let mut last = String::new();
    for attempt in 0..START_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
        }
        let raw = raw.clone();
        let config = notify::Config::default().with_fsevent_latency(GATHER);
        let started = notify::RecommendedWatcher::new(
            move |event| drop(raw.send(Msg::Raw(Instant::now(), event))),
            config,
        )
        .and_then(|mut watcher| {
            place_watches(&mut watcher, layout).map(|placed| (watcher, placed))
        });
        match started {
            Ok(watcher) => return Ok(watcher),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// Widen `span` to cover a real change that arrived `at`.
fn widen(span: &mut Option<Wrote>, at: Instant) {
    match span {
        Some(w) => (w.first, w.last) = (w.first.min(at), w.last.max(at)),
        None => *span = Some(Wrote { first: at, last: at, paths: Some(BTreeSet::new()) }),
    }
}

impl State {
    /// Turn one window's raw events into a batch, and the real changes it holds.
    fn batch(
        &mut self,
        events: &[(Instant, Event)],
        watcher: &mut notify::RecommendedWatcher,
        kept: &Kept,
        failed: &mut Option<String>,
    ) -> (Batch, Option<Wrote>) {
        let mut batch = Batch::default();
        let mut wrote: Option<Wrote> = None;
        let mut paths: Vec<(String, bool, Instant)> = Vec::new();
        let mut moved_refs: BTreeSet<String> = BTreeSet::new();
        for (at, event) in events {
            if event.need_rescan() {
                batch.rescan = true;
                widen(&mut wrote, *at);
            }
            let structural = structural(event.kind);
            for path in &event.paths {
                // A name git and this batch cannot spell alike is re-read whole.
                if path.to_str().is_none() {
                    batch.rescan = true;
                    widen(&mut wrote, *at);
                    continue;
                }
                match classify(&self.layout, path) {
                    Class::Worktree(rel) => paths.push((rel, structural, *at)),
                    Class::Git(change) => {
                        batch.git.insert(change);
                    }
                    Class::Config => batch.config = true,
                    Class::Ref(name) => {
                        moved_refs.insert(name);
                    }
                    Class::Drop => {}
                }
            }
        }
        self.refs_into(&moved_refs, &kept.refs, &mut batch);
        // A new config watch restarts the stream on macOS, dropping what it gathered: read it all.
        if batch.config && self.follow_config(watcher, failed) {
            batch.rescan = true;
        }
        // Each path as the entry it belongs to, beside the path itself.
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut paths: Vec<(String, String, bool, Instant)> = paths
            .into_iter()
            .map(|(raw, structural, at)| (self.repo_entry(&raw), raw, structural, at))
            .collect();
        // A new `core.excludesFile` is a rule change, and its file is watched from now on.
        if batch.git.contains(&GitChange::Config) && self.follow_excludes(watcher, failed) {
            batch.git.insert(GitChange::IgnoreRules);
            // A new watch restarts the stream on macOS, dropping what it gathered: read it all.
            batch.rescan = true;
        }
        // What git's verdicts rest on moved: its rules, or the index, where a force-added file is
        // kept. The cache starts over before anything is asked.
        let from_git = batch.git.contains(&GitChange::IgnoreRules);
        if from_git || batch.git.contains(&GitChange::Index) {
            self.ignored.clear();
        }
        #[cfg(target_os = "linux")]
        if batch.git.contains(&GitChange::Index) {
            linux::watch_tracked(watcher, &self.layout.root, failed);
        }
        self.query_ignored(paths.iter().map(|(p, ..)| p.as_str()));
        // A `.gitignore` outside every ignored directory can flip any verdict: ask again under it.
        if !from_git && paths.iter().any(|(p, ..)| named(p, ".gitignore") && !self.under_ignored(p))
        {
            batch.git.insert(GitChange::IgnoreRules);
            self.ignored.clear();
            self.query_ignored(paths.iter().map(|(p, ..)| p.as_str()));
        }
        let ignored = |p: &str| self.under_ignored(p) || self.is_ignored(p);
        if paths.iter().any(|(p, ..)| named(p, ".gitattributes") && !ignored(p)) {
            batch.git.insert(GitChange::Attributes);
        }
        #[cfg(target_os = "linux")]
        if batch.git.contains(&GitChange::IgnoreRules) {
            self.watches.rewatch(watcher, &self.layout, &kept.shown, failed);
        }
        if paths.iter().any(|(p, ..)| named(p, ".gitmodules")) {
            self.nested.clear();
        }
        // A kept nested repository's own rule file moves only its verdicts, and on Linux its watches.
        let ruled: BTreeSet<String> = paths
            .iter()
            .filter(|(entry, raw, ..)| {
                entry != raw && named(raw, ".gitignore") && self.keeps_nested(entry)
            })
            .map(|(entry, ..)| entry.clone())
            .collect();
        for entry in &ruled {
            let inside = format!("{entry}/");
            self.ignored.retain(|path, _| !path.starts_with(&inside));
            #[cfg(target_os = "linux")]
            self.watches.follow_new_dir(watcher, &self.layout, entry, failed);
        }
        // A path inside a nested repository the worktree keeps is that repository's to ignore.
        self.query_nested(&paths);
        // On Linux a new directory gets its own watch; what it already holds reports as changed.
        #[cfg(target_os = "linux")]
        {
            let mut found = Vec::new();
            for (entry, raw, structural, at) in &paths {
                let ignored = self.is_ignored(entry)
                    || self.under_ignored(entry)
                    || (entry != raw && self.nested_ignored(entry, raw));
                if *structural && !ignored {
                    let found_here =
                        self.watches.follow_new_dir(watcher, &self.layout, raw, failed);
                    for inside in found_here {
                        found.push((self.repo_entry(&inside), inside, false, *at));
                    }
                }
            }
            if !found.is_empty() {
                self.query_ignored(found.iter().map(|(p, ..)| p.as_str()));
                self.query_nested(&found);
                paths.extend(found);
            }
        }
        // A path seen twice in one window is structural if either event was.
        let mut seen: BTreeMap<(String, String), (bool, Vec<Instant>)> = BTreeMap::new();
        for (entry, raw, structural, at) in paths {
            let seen = seen.entry((entry, raw)).or_default();
            seen.0 |= structural;
            seen.1.push(at);
        }
        for ((entry, raw), (structural, times)) in seen {
            let now = kind_on_disk(&self.layout.root.join(&raw));
            // A folder's own modify event (Windows sends one for any change inside) is no change.
            if !structural && now == Some(Kind::Folder) {
                continue;
            }
            // A path that came back since the batch was asked about gets its own answer now.
            self.query_ignored(std::iter::once(entry.as_str()));
            let Some(change) = self.keeps(&entry, &kept.shown, structural) else { continue };
            // Only a change git does not ignore is a write a turn made.
            if change {
                for at in times {
                    widen(&mut wrote, at);
                }
                if let Some(Wrote { paths: Some(paths), .. }) = wrote.as_mut() {
                    paths.insert(entry.clone());
                }
            }
            // Search hears what git keeps and every folder that appeared, whose walk applies the
            // ignore rules; dropping what went, or what a folder replaced, is free.
            let folder = structural && now == Some(Kind::Folder) && !self.under_ignored(&entry);
            if (change || folder) && (entry == raw || !self.nested_ignored(&entry, &raw)) {
                batch.files.insert(raw);
            }
            batch.worktree.insert(entry);
        }
        if let Some(wrote) = wrote.as_mut() {
            // Lost events, or a stream restarted to follow the config: the writes may be anywhere.
            wrote.paths = wrote.paths.take().filter(|_| !batch.rescan);
        }
        (batch, wrote)
    }

    /// Keep the refs reviewr reads; a private ref only when another process wrote it.
    fn refs_into(&self, moved: &BTreeSet<String>, extra: &BTreeSet<String>, batch: &mut Batch) {
        let Some((dir, _)) = &self.layout.git else { return };
        if moved.is_empty() {
            return;
        }
        let head = head_branch(dir);
        for name in moved {
            if name.starts_with("refs/worktree/reviewr/") {
                let value = std::fs::read_to_string(dir.join(name)).ok();
                if !crate::git::is_own_ref_write(dir, name, value.as_deref().map(str::trim)) {
                    batch.git.insert(GitChange::ReviewrRefs);
                }
            } else if ref_matters(name, head.as_deref(), extra) {
                batch.git.insert(GitChange::Refs);
            }
        }
    }

    /// The global excludes file may have moved with git's config: watch where it lives now.
    /// Whether it moved.
    fn follow_excludes(
        &mut self,
        watcher: &mut notify::RecommendedWatcher,
        failed: &mut Option<String>,
    ) -> bool {
        let found = crate::git::global_excludes(&self.layout.root).map(|e| canonical_beside(&e));
        if found == self.layout.excludes {
            return false;
        }
        self.layout.excludes = found;
        if let Err(e) = watch_outside(watcher, &self.layout) {
            failed.get_or_insert_with(|| e.to_string());
        }
        true
    }

    /// The config directory may have appeared or `config.toml` been relinked: watch where it lives
    /// now. Whether the watched paths changed; a watch that fails is noted in `failed`.
    fn follow_config(
        &mut self,
        watcher: &mut notify::RecommendedWatcher,
        failed: &mut Option<String>,
    ) -> bool {
        let Some(config) = &self.layout.config else { return false };
        let found = ConfigFiles::locate(&config.dir);
        if found == *config {
            return false;
        }
        self.layout.config = Some(found);
        if let Err(e) = watch_outside(watcher, &self.layout) {
            failed.get_or_insert_with(|| e.to_string());
        }
        true
    }

    /// The entry a path belongs to: itself, or the nearest enclosing nested repository, whose
    /// inside `git diff -- <path>` cannot see. Nothing is looked up under a known-ignored directory.
    fn repo_entry(&mut self, path: &str) -> String {
        if self.nested.len() > CACHE_CAP {
            self.nested.clear();
        }
        let mut ancestors = ancestors_and_self(path);
        ancestors.pop();
        for prefix in ancestors {
            if self.ignored.get(&format!("{prefix}/")) == Some(&true) {
                break;
            }
            let root = &self.layout.root;
            let nested = *self
                .nested
                .entry(prefix.clone())
                .or_insert_with(|| root.join(&prefix).join(".git").exists());
            if nested {
                return prefix;
            }
        }
        path.to_string()
    }

    /// Fill the ignore cache for `paths` that exist and their folders in one `check-ignore`. A
    /// failed run caches nothing, so the paths count as not ignored for now.
    fn query_ignored<'a>(&mut self, paths: impl Iterator<Item = &'a str>) {
        if self.layout.git.is_none() {
            return;
        }
        if self.ignored.len() > CACHE_CAP {
            self.ignored.clear();
        }
        let mut unknown: BTreeSet<String> = BTreeSet::new();
        for path in paths {
            let mut chain = ancestors_and_self(path);
            let leaf = chain.pop().expect("a path is its own last ancestor");
            let folders = chain.into_iter().map(|a| format!("{a}/"));
            for spelling in folders.chain(self.spelling(&leaf)) {
                match self.ignored.get(&spelling) {
                    Some(true) => break,
                    Some(false) => {}
                    None => {
                        unknown.insert(spelling);
                    }
                }
            }
        }
        if unknown.is_empty() {
            return;
        }
        let root = &self.layout.root;
        let answers: Vec<(String, bool)> =
            match crate::git::check_ignore(root, unknown.iter().map(String::as_str)) {
                Some(ignored) => unknown
                    .into_iter()
                    .map(|p| {
                        let verdict = ignored.contains(&p);
                        (p, verdict)
                    })
                    .collect(),
                None => one_at_a_time(root, &unknown),
            };
        self.ignored.extend(answers);
    }

    /// How git is asked about a path: as what it is now; a gone path is not asked.
    fn spelling(&self, path: &str) -> Option<String> {
        kind_on_disk(&self.layout.root.join(path)).map(|kind| spelled(path, kind))
    }

    /// Whether git ignores the path itself, as what it is now; a gone path, never.
    fn is_ignored(&self, path: &str) -> bool {
        self.spelling(path).is_some_and(|s| self.ignored.get(&s) == Some(&true))
    }

    /// Fill the ignore cache for paths inside kept nested repositories, each asked of its own
    /// repository, cached by the full path. A failed run caches nothing.
    fn query_nested(&mut self, paths: &[(String, String, bool, Instant)]) {
        let inside: Vec<(String, String)> = paths
            .iter()
            .filter(|(entry, raw, ..)| entry != raw && self.keeps_nested(entry))
            .map(|(entry, raw, ..)| (entry.clone(), raw.clone()))
            .collect();
        let mut unknown: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for (entry, raw) in &inside {
            for spelling in self.nested_spellings(entry, raw) {
                match self.ignored.get(&spelling) {
                    Some(true) => break,
                    Some(false) => {}
                    None => {
                        unknown.entry(entry).or_default().insert(spelling);
                    }
                }
            }
        }
        for (entry, spellings) in unknown {
            let repo = self.layout.root.join(entry);
            let rel = |spelling: &str| spelling[entry.len() + 1..].to_string();
            let Some(ignored) =
                crate::git::check_ignore(&repo, spellings.iter().map(|s| &s[entry.len() + 1..]))
            else {
                continue;
            };
            for spelling in spellings {
                let verdict = ignored.contains(&rel(&spelling));
                self.ignored.insert(spelling, verdict);
            }
        }
    }

    /// Whether the worktree keeps the nested repository at `entry`: git ignores neither it nor a parent.
    fn keeps_nested(&self, entry: &str) -> bool {
        !self.under_ignored(entry) && !self.is_ignored(entry)
    }

    /// Whether the nested repository at `entry` ignores `raw` or a directory above it.
    fn nested_ignored(&self, entry: &str, raw: &str) -> bool {
        self.nested_spellings(entry, raw).iter().any(|s| self.ignored.get(s) == Some(&true))
    }

    /// How `raw` and its folders inside the nested repository at `entry` are asked of it,
    /// outermost first, keyed by their full path.
    fn nested_spellings(&self, entry: &str, raw: &str) -> Vec<String> {
        nested_folders(entry, raw).chain(self.spelling(raw)).collect()
    }

    /// Whether a folder above `path`, not the path itself, is known ignored.
    fn under_ignored(&self, path: &str) -> bool {
        let chain = ancestors_and_self(path);
        chain[..chain.len() - 1].iter().any(|a| self.ignored.get(&format!("{a}/")) == Some(&true))
    }

    /// Whether a path reaches the batch, as a change (`true`) or for the screen alone: a path git
    /// keeps, or one that went, is a change; an ignored one shows when on screen or appearing.
    fn keeps(&mut self, path: &str, shown: &BTreeSet<String>, structural: bool) -> Option<bool> {
        let on_screen = crate::git::covered(shown, path);
        if self.under_ignored(path) {
            return on_screen.then_some(false);
        }
        if !self.is_ignored(path) {
            self.present.remove(path);
            return Some(true);
        }
        if self.present.len() > CACHE_CAP {
            self.present.clear();
        }
        let appeared = self.present.insert(path.to_string()) && structural;
        (on_screen || appeared).then_some(false)
    }
}

/// A batch's ignore check one path at a time, when one path git refuses failed the whole batch
/// (a path beyond a symbolic link): the verdicts git gave, none for a path it refused.
fn one_at_a_time(root: &Path, unknown: &BTreeSet<String>) -> Vec<(String, bool)> {
    if unknown.len() > 64 {
        return Vec::new();
    }
    let answer = |path: &String| {
        let ignored = crate::git::check_ignore(root, std::iter::once(path.as_str()))?;
        Some((path.clone(), ignored.contains(path)))
    };
    unknown.iter().filter_map(answer).collect()
}

/// `a`, `a/b`, `a/b/c` for `a/b/c`.
fn ancestors_and_self(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut prefix = String::new();
    for part in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        out.push(prefix.clone());
    }
    out
}

/// Whether `path`'s own name is `name`, at any depth.
fn named(path: &str, name: &str) -> bool {
    path.rsplit('/').next() == Some(name)
}

/// Place the watches: one recursive watch per root on macOS and Windows; on Linux one per
/// directory, skipping ignored ones.
fn place_watches(
    watcher: &mut notify::RecommendedWatcher,
    layout: &Layout,
) -> notify::Result<Placed> {
    #[cfg(target_os = "linux")]
    {
        linux::place(watcher, layout)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // One batch: `FSEvents` restarts its stream on every change of paths, losing writes meanwhile.
        let ops = |outside: bool| {
            let mut ops = vec![notify::PathOp::watch_recursive(&layout.root)];
            if let Some((dir, common)) = &layout.git {
                for extra in [dir, common].into_iter().filter(|d| !d.starts_with(&layout.root)) {
                    ops.push(notify::PathOp::watch_recursive(extra));
                }
            }
            if outside {
                ops.extend(
                    outside_paths(layout).into_iter().map(notify::PathOp::watch_non_recursive),
                );
            }
            ops
        };
        match watcher.update_paths(ops(true)) {
            // The stream itself would not start.
            Err(e) if e.origin.is_none() => Err(e.source),
            // A file outside is best effort: the worktree is watched without them.
            Err(e) => {
                logln!("watch outside the worktree failed: {}", e.source);
                watcher.update_paths(ops(false)).map_err(|e| e.source)
            }
            Ok(()) => Ok(()),
        }
    }
}

/// The files outside the worktree that change what reviewr shows: the plugin config (or where it
/// will appear), a `config.toml` linked from elsewhere, and the global ignore file.
fn outside_paths(layout: &Layout) -> Vec<PathBuf> {
    let files =
        layout.config.iter().filter_map(|c| c.target.clone()).chain(layout.excludes.clone());
    let config = layout.config.iter().map(|c| c.watched.clone());
    config.chain(files.filter_map(|f| file_watch(&f))).collect()
}

/// Watch [`outside_paths`] as the config moves: each best effort on Linux, one batch elsewhere,
/// where a failure may leave the stream stopped and so fails the watcher.
fn watch_outside(watcher: &mut notify::RecommendedWatcher, layout: &Layout) -> notify::Result<()> {
    let paths = outside_paths(layout);
    if cfg!(target_os = "linux") {
        for path in paths {
            if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                logln!("watch {} failed: {e}", path.display());
            }
        }
        return Ok(());
    }
    let ops = paths.into_iter().map(notify::PathOp::watch_non_recursive).collect();
    watcher.update_paths(ops).map_err(|e| e.source)
}

/// What to watch for one file outside the worktree: on macOS the file itself, or its directory
/// while missing unless that is `$HOME` or above; elsewhere its directory, since a save replaces the file.
fn file_watch(file: &Path) -> Option<PathBuf> {
    let dir = file.parent().filter(|d| d.is_dir()).map(Path::to_path_buf);
    if !cfg!(target_os = "macos") {
        return dir;
    }
    if file.exists() {
        return Some(file.to_path_buf());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let home = crate::git::canonical(&home);
    dir.filter(|d| !home.starts_with(d))
}

#[cfg(target_os = "linux")]
mod linux {
    //! One inotify watch per non-ignored directory, so `node_modules` never eats the watch limit.
    //! Git files are watched through their directories, since git renames a lock over a file.

    use super::{Layout, RecursiveMode, Watcher};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn walker(dir: &Path) -> ignore::Walk {
        ignore::WalkBuilder::new(dir)
            .hidden(false)
            .ignore(false)
            .parents(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .filter_entry(|e| e.file_name() != ".git")
            .build()
    }

    /// The worktree folders the walk placed a watch on, so new ignore rules can take some back.
    #[derive(Debug, Default)]
    pub(crate) struct Watches {
        walked: BTreeSet<PathBuf>,
    }

    /// Watch `dir`; a watch refused for lack of inotify watches fails the watcher (`failed`), so
    /// the stand-in takes over rather than leave the folder unwatched.
    fn watch(
        watcher: &mut notify::RecommendedWatcher,
        dir: &Path,
        failed: &mut Option<String>,
    ) -> bool {
        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => true,
            Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
                failed.get_or_insert_with(|| e.to_string());
                false
            }
            // A folder gone before its watch landed holds nothing to report.
            Err(_) => false,
        }
    }

    /// The folders the walk would watch under `dir`.
    fn walk_dirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
        walker(dir)
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
            .map(ignore::DirEntry::into_path)
    }

    pub(super) fn place(
        watcher: &mut notify::RecommendedWatcher,
        layout: &Layout,
    ) -> notify::Result<Watches> {
        let mut failed = None;
        let mut walked = BTreeSet::new();
        for dir in walk_dirs(&layout.root) {
            if watch(watcher, &dir, &mut failed) {
                walked.insert(dir);
            }
            if let Some(reason) = failed {
                return Err(notify::Error::generic(&reason));
            }
        }
        if let Some((dir, common)) = &layout.git {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
            let reftables = [dir.join("reftable"), common.join("reftable")];
            for sub in [dir.join("refs"), common.join("refs")].into_iter().chain(reftables) {
                if sub.is_dir() {
                    watcher.watch(&sub, RecursiveMode::Recursive)?;
                }
            }
            watcher.watch(common, RecursiveMode::NonRecursive)?;
            if common.join("info").is_dir() {
                watcher.watch(&common.join("info"), RecursiveMode::NonRecursive)?;
            }
        }
        super::watch_outside(watcher, layout)?;
        watch_tracked(watcher, &layout.root, &mut failed);
        match failed {
            Some(reason) => Err(notify::Error::generic(&reason)),
            None => Ok(Watches { walked }),
        }
    }

    /// Watch the folder of each tracked file under an ignored folder, which the walk skipped.
    pub(super) fn watch_tracked(
        watcher: &mut notify::RecommendedWatcher,
        root: &Path,
        failed: &mut Option<String>,
    ) {
        for file in crate::git::tracked_ignored(root) {
            if let Some(dir) = root.join(file).parent() {
                watch(watcher, dir, failed);
            }
        }
    }

    impl Watches {
        /// Ignore rules changed: watch what the walk now reaches, and drop what it no longer does
        /// unless it is on screen (`shown`).
        pub(super) fn rewatch(
            &mut self,
            watcher: &mut notify::RecommendedWatcher,
            layout: &Layout,
            shown: &BTreeSet<String>,
            failed: &mut Option<String>,
        ) {
            let now: BTreeSet<PathBuf> = walk_dirs(&layout.root).collect();
            let on_screen = |dir: &Path| shown.iter().any(|s| layout.root.join(s) == dir);
            for gone in self.walked.difference(&now).filter(|d| !on_screen(d)) {
                let _ = watcher.unwatch(gone);
            }
            let kept: BTreeSet<PathBuf> = self.walked.intersection(&now).cloned().collect();
            let added: Vec<PathBuf> = now.difference(&self.walked).cloned().collect();
            self.walked = kept;
            for dir in added {
                if watch(watcher, &dir, failed) {
                    self.walked.insert(dir);
                }
            }
            // A tracked file's folder the walk now skips keeps its watch.
            watch_tracked(watcher, &layout.root, failed);
        }

        /// A directory that appeared: watch it and everything non-ignored inside, and report what
        /// was already written there before its watch existed.
        pub(super) fn follow_new_dir(
            &mut self,
            watcher: &mut notify::RecommendedWatcher,
            layout: &Layout,
            rel: &str,
            failed: &mut Option<String>,
        ) -> Vec<String> {
            let dir = layout.root.join(rel);
            if !dir.is_dir() {
                return Vec::new();
            }
            let mut found = Vec::new();
            for entry in walker(&dir).flatten() {
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    if watch(watcher, entry.path(), failed) {
                        self.walked.insert(entry.into_path());
                    }
                } else if let Ok(rel) = entry.path().strip_prefix(&layout.root) {
                    found.push(super::slash(rel));
                }
            }
            found
        }

        /// Watch newly shown ignored directories, and drop the watches of ones no longer shown.
        pub(super) fn show(
            &self,
            watcher: &mut notify::RecommendedWatcher,
            root: &Path,
            old: &BTreeSet<String>,
            new: &BTreeSet<String>,
            failed: &mut Option<String>,
        ) {
            // An unshown folder's watch goes unless the walk placed it; an unshown file's stays.
            let gone = old.difference(new).map(|g| root.join(g)).filter(|g| !g.is_file());
            for gone in gone.filter(|g| !self.walked.contains(g)) {
                let _ = watcher.unwatch(&gone);
            }
            // A shown file is watched through its directory, which the walk skipped.
            for added in new.difference(old) {
                let path = root.join(added);
                let dir = if path.is_dir() { Some(path.as_path()) } else { path.parent() };
                if let Some(dir) = dir {
                    watch(watcher, dir, failed);
                }
            }
            // A tracked file's folder keeps its watch when it leaves the screen.
            watch_tracked(watcher, root, failed);
        }
    }
}

/// The kind of a network or FUSE filesystem at `path`, which delivers no change events; `None`
/// for a local one, and always on Windows, which has no such check.
#[cfg(unix)]
fn remote_filesystem(path: &Path) -> Option<String> {
    let stat = rustix::fs::statfs(path).ok()?;
    #[cfg(target_os = "linux")]
    {
        // NFS, SMB, CIFS, SMB2, FUSE, 9P, Ceph, AFS.
        const REMOTE: &[i64] = &[
            0x6969,
            0x517b,
            0xff53_4d42_u32 as i64,
            0xfe53_4d42_u32 as i64,
            0x6573_5546,
            0x0102_1997,
            0x00c3_6400,
            0x5346_414f,
        ];
        #[allow(clippy::useless_conversion)]
        let kind = i64::from(stat.f_type);
        REMOTE.contains(&kind).then(|| format!("{kind:#x}"))
    }
    #[cfg(target_os = "macos")]
    {
        let name: String = stat
            .f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| char::from(u8::try_from(c).unwrap_or(b'?')))
            .collect();
        let remote = ["nfs", "smbfs", "afpfs", "webdav", "macfuse", "osxfuse", "fusefs", "ftp"];
        remote.iter().any(|r| name.starts_with(r)).then_some(name)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stat;
        None
    }
}

#[cfg(windows)]
fn remote_filesystem(_: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::{Class, GitChange, Layout, classify, ref_matters};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn main_checkout() -> Layout {
        Layout {
            root: "/w".into(),
            git: Some(("/w/.git".into(), "/w/.git".into())),
            config: None,
            excludes: None,
        }
    }

    fn linked() -> Layout {
        Layout {
            root: "/wt".into(),
            git: Some(("/main/.git/worktrees/feature".into(), PathBuf::from("/main/.git"))),
            config: None,
            excludes: None,
        }
    }

    fn c(layout: &Layout, path: &str) -> Class {
        classify(layout, Path::new(path))
    }

    #[test]
    fn a_batch_git_refuses_whole_is_asked_one_path_at_a_time() {
        let (dir, _) = crate::test_support::test_repo();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("d"), &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(dir.path().join("d"), &link).unwrap();
        let unknown: BTreeSet<String> = ["a.log", "link/x"].map(String::from).into();
        let batch = crate::git::check_ignore(dir.path(), unknown.iter().map(String::as_str));
        assert_eq!(batch, None, "a path beyond a symbolic link fails the whole run");
        let one = super::one_at_a_time(dir.path(), &unknown);
        assert_eq!(one, [("a.log".to_string(), true)], "the refused path gets no verdict to cache");
    }

    #[test]
    fn worktree_files_report_by_their_relative_path() {
        let l = main_checkout();
        assert_eq!(c(&l, "/w/src/a.rs"), Class::Worktree("src/a.rs".into()));
        assert_eq!(c(&l, "/w"), Class::Drop, "the root itself is no entry");
        assert_eq!(c(&l, "/w/vendor/lib/.git/index"), Class::Drop, "a nested repo's internals");
        assert_eq!(c(&l, "/w/.watchman-cookie-host-1"), Class::Drop, "watchman's cookie");
        assert_eq!(c(&l, "/elsewhere/a.rs"), Class::Drop);
    }

    #[test]
    fn a_main_checkouts_git_files_classify_by_what_they_move() {
        let l = main_checkout();
        let rows = [
            ("/w/.git/HEAD", Class::Git(GitChange::Head)),
            ("/w/.git/index", Class::Git(GitChange::Index)),
            ("/w/.git/packed-refs", Class::Git(GitChange::Refs)),
            ("/w/.git/config", Class::Git(GitChange::Config)),
            ("/w/.git/info/exclude", Class::Git(GitChange::IgnoreRules)),
            ("/w/.git/info/attributes", Class::Git(GitChange::Attributes)),
            ("/w/.git/refs/heads/main", Class::Ref("refs/heads/main".into())),
            ("/w/.git/reftable/tables.list", Class::Git(GitChange::Refs)),
        ];
        for (path, class) in rows {
            assert_eq!(c(&l, path), class, "{path}");
        }
        for noise in [
            "/w/.git/index.lock",
            "/w/.git/HEAD.lock",
            "/w/.git/refs/heads/main.lock",
            "/w/.git/objects/ab/cdef",
            "/w/.git/logs/HEAD",
            "/w/.git/FETCH_HEAD",
            "/w/.git/ORIG_HEAD",
            "/w/.git/COMMIT_EDITMSG",
            "/w/.git/gc.pid",
            "/w/.git/hooks/pre-commit",
            "/w/.git/fsmonitor--daemon/cookies/1",
            "/w/.git/worktrees/other/index",
            "/w/.git/worktrees/other/HEAD",
            "/w/.git/modules/sub/index",
            "/w/.git/reviewr/index-12-ab/index",
            "/w/.git/reviewr/index-12-ab/index.lock",
        ] {
            assert_eq!(c(&l, noise), Class::Drop, "{noise} changes nothing a refresh shows");
        }
    }

    #[test]
    fn a_linked_worktree_reads_its_own_dir_and_the_shared_rules() {
        let l = linked();
        assert_eq!(c(&l, "/main/.git/worktrees/feature/HEAD"), Class::Git(GitChange::Head));
        assert_eq!(c(&l, "/main/.git/worktrees/feature/index"), Class::Git(GitChange::Index));
        assert_eq!(
            c(&l, "/main/.git/worktrees/feature/config.worktree"),
            Class::Git(GitChange::Config)
        );
        assert_eq!(
            c(&l, "/main/.git/worktrees/feature/refs/worktree/reviewr/base-pick"),
            Class::Ref("refs/worktree/reviewr/base-pick".into())
        );
        assert_eq!(
            c(&l, "/main/.git/refs/remotes/origin/main"),
            Class::Ref("refs/remotes/origin/main".into())
        );
        assert_eq!(c(&l, "/main/.git/packed-refs"), Class::Git(GitChange::Refs));
        assert_eq!(c(&l, "/main/.git/info/exclude"), Class::Git(GitChange::IgnoreRules));
        for noise in [
            "/main/.git/HEAD",
            "/main/.git/index",
            "/main/.git/worktrees/other/index",
            "/main/.git/worktrees/other/config.worktree",
            "/main/.git/objects/ab/cdef",
            "/main/.git/worktrees/feature/reviewr/index-7-cd/index",
        ] {
            assert_eq!(c(&l, noise), Class::Drop, "{noise} is another worktree's or noise");
        }
        assert_eq!(c(&l, "/wt/.git"), Class::Drop, "the linked worktree's `.git` file");
    }

    #[test]
    fn only_the_refs_reviewr_reads_matter() {
        let extra: BTreeSet<String> = ["refs/heads/develop".to_string()].into();
        let head = Some("refs/heads/feature");
        assert!(ref_matters("refs/heads/feature", head, &extra), "this worktree's branch");
        assert!(ref_matters("refs/remotes/origin/main", head, &extra), "a fetch moves the base");
        assert!(ref_matters("refs/tags/v1", head, &extra), "a tag base");
        assert!(ref_matters("refs/remotes", head, &extra), "a first fetch's new directory");
        assert!(!ref_matters("refs/remotesque", head, &extra), "by whole components");
        assert!(ref_matters("refs/heads/develop", head, &extra), "a local base the app named");
        assert!(!ref_matters("refs/heads/other-agent", head, &extra), "another agent's branch");
    }

    #[cfg(unix)]
    #[test]
    fn a_local_filesystem_is_no_remote_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(super::remote_filesystem(dir.path()), None);
    }
}
