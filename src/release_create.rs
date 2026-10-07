//! A new GitHub release on `origin`: the in-pane form, its rules, and its `gh` requests.

use crate::forge::GhError;
use crate::git::RepoTarget;
use crate::releases::{Release, ReleasesSnapshot};

/// One field of the release form, in the order `tab` walks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Tag,
    Title,
    Prerelease,
    Latest,
    Discussion,
    Notes,
}

/// Every field, in form order.
pub const FIELDS: [Field; 6] =
    [Field::Tag, Field::Title, Field::Prerelease, Field::Latest, Field::Discussion, Field::Notes];

impl Field {
    /// The label the form and the review print.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tag => "Tag",
            Self::Title => "Title",
            Self::Prerelease => "Pre-release",
            Self::Latest => "Latest",
            Self::Discussion => "Discussion",
            Self::Notes => "Notes",
        }
    }

    /// Whether the field takes typed text, and `enter` breaks a line in it.
    fn multiline(self) -> bool {
        self == Self::Notes
    }
}

/// An editable text with a caret, a char index; multi-line fields move by line too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextField {
    pub text: String,
    pub caret: usize,
}

impl TextField {
    fn new(text: &str) -> Self {
        Self { text: text.to_string(), caret: text.chars().count() }
    }

    fn byte(&self, caret: usize) -> usize {
        self.text.char_indices().nth(caret).map_or(self.text.len(), |(i, _)| i)
    }

    pub fn set(&mut self, text: &str) {
        *self = Self::new(text);
    }

    pub fn insert(&mut self, ch: char) {
        let at = self.byte(self.caret);
        self.text.insert(at, ch);
        self.caret += 1;
    }

    pub fn backspace(&mut self) {
        if self.caret > 0 {
            self.caret -= 1;
            let at = self.byte(self.caret);
            self.text.remove(at);
        }
    }

    pub fn delete(&mut self) {
        if self.caret < self.text.chars().count() {
            let at = self.byte(self.caret);
            self.text.remove(at);
        }
    }

    pub fn left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.caret = (self.caret + 1).min(self.text.chars().count());
    }

    /// The caret's line and column.
    #[must_use]
    pub fn line_col(&self) -> (usize, usize) {
        let before: Vec<char> = self.text.chars().take(self.caret).collect();
        let line = before.iter().filter(|c| **c == '\n').count();
        let col = before.iter().rev().take_while(|c| **c != '\n').count();
        (line, col)
    }

    /// Put the caret at `line`, as near `col` as that line allows.
    fn goto(&mut self, line: usize, col: usize) {
        let mut caret = 0;
        for (i, text) in self.text.split('\n').enumerate() {
            let len = text.chars().count();
            if i == line {
                self.caret = caret + col.min(len);
                return;
            }
            caret += len + 1;
        }
    }

    pub fn home(&mut self) {
        let (line, _) = self.line_col();
        self.goto(line, 0);
    }

    pub fn end(&mut self) {
        let (line, _) = self.line_col();
        self.goto(line, usize::MAX);
    }

    /// Move a line up or down; `false` at the edge, so the key can move the focus instead.
    pub fn vertical(&mut self, down: bool) -> bool {
        let (line, col) = self.line_col();
        let lines = self.text.split('\n').count();
        match (down, line) {
            (true, l) if l + 1 < lines => self.goto(l + 1, col),
            (false, l) if l > 0 => self.goto(l - 1, col),
            _ => return false,
        }
        true
    }
}

/// Which GitHub write the review ends in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Publish,
    Draft,
}

/// Where the form stands; only `Review` takes the send key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Edit,
    /// Every field and option that will be sent is on screen, checked, and waiting for the key.
    Review(Finish),
    Sending(Finish),
}

/// The repository's discussion categories, read once as the form opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Categories {
    Loading,
    Ready(Vec<String>),
    /// Discussions are off for this repository.
    Off,
    Failed(String),
}

/// A release being written, before anything leaves the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReleaseDraft {
    /// Tags this draft's worker results, so a cancelled draft's result lands nowhere.
    pub id: u64,
    pub repository: RepoTarget,
    pub branch: String,
    /// The selected unreleased commit: where the release and its new tag point.
    pub target: String,
    /// Its subject, so the form names the commit it releases.
    pub target_subject: String,
    /// `origin`'s highest version tag, where generated notes start.
    pub previous: Option<String>,
    pub tag: TextField,
    pub title: TextField,
    /// Whether the title still follows the tag, until it is typed in.
    title_follows: bool,
    pub prerelease: bool,
    /// Whether GitHub marks this the latest release; a pre-release never is.
    pub latest: bool,
    pub categories: Categories,
    /// The discussion category picked, by index into the categories; none by default.
    pub discussion: Option<usize>,
    pub notes: TextField,
    pub field: Field,
    /// The latest release, whose title the new one copies.
    pattern: Option<Release>,
    /// `origin`'s git tags.
    existing: Vec<String>,
    /// Every release's tag, drafts included: taken though no git tag exists yet.
    held: Vec<String>,
    pub generating: bool,
    /// The generate request in flight; only its result lands, and an edit drops it.
    pending: Option<u64>,
    requests: u64,
    pub stage: Stage,
    /// What the open review will send, checked once as it opened.
    pub reviewed: Option<Reviewed>,
    /// Why the last check, generation, or send failed, as said.
    pub error: Option<String>,
}

/// A review's checked send, fixed as the review opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewed {
    pub release: Publish,
}

/// A request a draft owes a worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Categories { id: u64, repository: RepoTarget },
    Generate { id: u64, seq: u64, host: String, args: Vec<String> },
    Publish { id: u64, release: Publish },
}

/// Everything one send carries, fixed at the review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publish {
    pub repository: RepoTarget,
    pub tag: String,
    pub target: String,
    pub title: String,
    pub notes: String,
    pub prerelease: bool,
    /// Whether GitHub marks it latest; `None` for a draft, which only a publish can mark.
    pub latest: Option<bool>,
    /// Saved as a draft, which GitHub tags only when it is published.
    pub draft: bool,
    pub discussion: Option<String>,
}

impl ReleaseDraft {
    /// A form releasing unreleased commit `target`, or `None` when it is not one.
    #[must_use]
    pub fn new(snapshot: &ReleasesSnapshot, id: u64, target: &str) -> Option<Self> {
        let subject = snapshot.root.iter().find(|commit| commit.oid == target)?.subject.clone();
        let previous = snapshot.versions.first().map(|version| version.tag.clone());
        let existing: Vec<String> = snapshot.tags.values().flatten().cloned().collect();
        let held: Vec<String> = snapshot.release_tags().map(String::from).collect();
        let tag = next_free(previous.as_deref(), &existing, &held);
        let pattern = snapshot.releases.iter().find(|release| !release.draft).cloned();
        Some(Self {
            id,
            repository: snapshot.repository.clone(),
            branch: snapshot.branch.clone(),
            target: target.to_string(),
            target_subject: subject,
            title: TextField::new(&title_for(pattern.as_ref(), &tag)),
            tag: TextField::new(&tag),
            title_follows: true,
            previous,
            prerelease: false,
            latest: true,
            categories: Categories::Loading,
            discussion: None,
            notes: TextField::default(),
            field: Field::Tag,
            pattern,
            existing,
            held,
            generating: false,
            pending: None,
            requests: 0,
            stage: Stage::Edit,
            reviewed: None,
            error: None,
        })
    }

    /// The read of the discussion categories this form owes as it opens.
    #[must_use]
    pub fn categories_request(&self) -> Request {
        Request::Categories { id: self.id, repository: self.repository.clone() }
    }

    /// The text of the focused field, when it takes text.
    pub fn text_mut(&mut self) -> Option<&mut TextField> {
        Some(match self.field {
            Field::Tag => &mut self.tag,
            Field::Title => &mut self.title,
            Field::Notes => &mut self.notes,
            _ => return None,
        })
    }

    /// Run `edit` on the focused text, then follow what it changed.
    pub fn edit(&mut self, edit: impl FnOnce(&mut TextField)) {
        let field = self.field;
        let Some(text) = self.text_mut() else { return };
        let before = text.text.clone();
        edit(text);
        if text.text == before {
            return;
        }
        self.error = None;
        match field {
            Field::Tag => self.tag_changed(),
            Field::Title => self.title_follows = false,
            // Notes typed while generating win: the generated ones are dropped.
            Field::Notes => self.drop_generation(),
            _ => {}
        }
    }

    /// A key on a field without text: a toggle flips, the discussion steps through categories.
    pub fn toggle(&mut self, forward: bool) {
        self.error = None;
        match self.field {
            Field::Prerelease => {
                self.prerelease = !self.prerelease;
                // GitHub never marks a pre-release latest.
                self.latest = !self.prerelease;
            }
            Field::Latest if !self.prerelease => self.latest = !self.latest,
            Field::Discussion => {
                let Categories::Ready(categories) = &self.categories else { return };
                let n = categories.len() + 1;
                let at = self.discussion.map_or(0, |i| i + 1);
                let next = if forward { (at + 1) % n } else { (at + n - 1) % n };
                self.discussion = next.checked_sub(1);
            }
            _ => {}
        }
    }

    /// Replace the notes, as the external editor does; a generation in flight is dropped.
    pub fn set_notes(&mut self, notes: &str) {
        self.notes.set(notes.trim_end());
        self.drop_generation();
    }

    /// Whether `enter` breaks a line in the focused field.
    #[must_use]
    pub fn takes_newline(&self) -> bool {
        self.field.multiline()
    }

    /// Focus the next or previous field.
    pub fn step_field(&mut self, forward: bool) {
        let at = FIELDS.iter().position(|f| *f == self.field).unwrap_or(0);
        let n = FIELDS.len();
        self.field = FIELDS[if forward { (at + 1) % n } else { (at + n - 1) % n }];
    }

    /// Focus `field`, as a click on its row does.
    pub fn focus(&mut self, field: Field) {
        self.field = field;
    }

    fn tag_changed(&mut self) {
        self.drop_generation();
        if self.title_follows {
            self.title.set(&title_for(self.pattern.as_ref(), self.tag.text.trim()));
        }
    }

    /// Notes asked for under other inputs never land.
    fn drop_generation(&mut self) {
        self.generating = false;
        self.pending = None;
    }

    /// The checked tag, or why the tag cannot be one.
    pub fn checked_tag(&self) -> Result<String, String> {
        check_version(&self.tag.text, &self.existing, &self.held)
    }

    /// Ask GitHub for its generated notes, for a tag, target, and starting tag that check out.
    pub fn generate(&mut self) -> Option<Request> {
        if self.generating {
            self.error = Some("The notes are already generating.".to_string());
            return None;
        }
        let tag = match self.checked_tag() {
            Ok(tag) => tag,
            Err(error) => {
                self.error = Some(error);
                return None;
            }
        };
        self.error = None;
        self.generating = true;
        self.requests += 1;
        self.pending = Some(self.requests);
        Some(Request::Generate {
            id: self.id,
            seq: self.requests,
            host: self.repository.host().to_string(),
            args: generate_args(&self.repository, &tag, &self.target, self.previous.as_deref()),
        })
    }

    /// Land the generated notes this draft still waits for; any other result is stale.
    pub fn generated(&mut self, id: u64, seq: u64, notes: Result<String, String>) {
        if id != self.id || self.pending != Some(seq) {
            return;
        }
        self.pending = None;
        self.generating = false;
        match notes {
            Ok(notes) => self.notes.set(&notes),
            Err(error) => self.error = Some(error),
        }
    }

    /// Land the discussion categories this draft asked for.
    pub fn categories_read(&mut self, id: u64, categories: Categories) {
        if id == self.id {
            self.categories = categories;
        }
    }

    /// Everything the send would carry, checked, or the first reason it cannot go.
    pub fn checked(&self, finish: Finish) -> Result<Publish, String> {
        if self.generating {
            return Err("The notes are still generating.".to_string());
        }
        let tag = self.checked_tag()?;
        let title = self.title.text.trim();
        Ok(Publish {
            repository: self.repository.clone(),
            target: self.target.clone(),
            title: if title.is_empty() { tag.clone() } else { title.to_string() },
            tag,
            notes: self.notes.text.clone(),
            prerelease: self.prerelease,
            latest: (finish == Finish::Publish).then_some(self.latest),
            draft: finish == Finish::Draft,
            discussion: match (&self.categories, self.discussion) {
                (Categories::Ready(names), Some(i)) => names.get(i).cloned(),
                _ => None,
            },
        })
    }

    /// Show every field and option that will be sent, once they all check out.
    pub fn review(&mut self, finish: Finish) {
        match self.checked(finish) {
            Ok(release) => {
                self.error = None;
                self.stage = Stage::Review(finish);
                self.reviewed = Some(Reviewed { release });
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Back from the review to editing.
    pub fn back(&mut self) {
        if matches!(self.stage, Stage::Review(_)) {
            self.stage = Stage::Edit;
            self.reviewed = None;
        }
    }

    /// The send, and only from the review: the explicit key is the one way out.
    pub fn confirm(&mut self) -> Option<Request> {
        let Stage::Review(finish) = self.stage else { return None };
        let release = match self.checked(finish) {
            Ok(release) => release,
            Err(error) => {
                self.failed(error);
                return None;
            }
        };
        self.stage = Stage::Sending(finish);
        Some(Request::Publish { id: self.id, release })
    }

    /// A failed send goes back to editing with GitHub's own words.
    pub fn failed(&mut self, error: String) {
        self.stage = Stage::Edit;
        self.reviewed = None;
        self.error = Some(error);
    }

    /// Paste into the focused field, one line for a single-line one; the review takes none.
    pub fn paste(&mut self, text: &str) {
        if self.stage != Stage::Edit {
            return;
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text = if self.takes_newline() { text } else { text.replace('\n', " ") };
        self.edit(|field| text.chars().for_each(|ch| field.insert(ch)));
    }
}

use crate::releases::version as version_of;

/// The next patch version after `highest`, spelled in its style; `v0.1.0` with no version yet.
#[must_use]
pub fn next_patch(highest: Option<&str>) -> String {
    let Some((tag, version)) = highest.and_then(|tag| Some((tag, version_of(tag)?))) else {
        return "v0.1.0".to_string();
    };
    let prefix = if tag.trim().starts_with('v') { "v" } else { "" };
    format!("{prefix}{}.{}.{}", version.major, version.minor, version.patch + 1)
}

/// The new release's title in the latest release's pattern, its version swapped in; else the tag.
#[must_use]
pub fn title_for(latest: Option<&Release>, tag: &str) -> String {
    let bare = |tag: &str| tag.strip_prefix('v').unwrap_or(tag).to_string();
    match latest {
        Some(release) if release.name.contains(&release.tag) => {
            release.name.replace(&release.tag, tag)
        }
        Some(release) if release.name.contains(&bare(&release.tag)) => {
            release.name.replace(&bare(&release.tag), &bare(tag))
        }
        _ => tag.to_string(),
    }
}

/// The next patch after `highest` that no tag or release on `origin` holds.
#[must_use]
pub fn next_free(highest: Option<&str>, tags: &[String], held: &[String]) -> String {
    let mut tag = next_patch(highest);
    // A draft holds its tag without a git tag, so the patch steps past it.
    for _ in 0..100 {
        if check_version(&tag, tags, held).is_ok() {
            break;
        }
        tag = next_patch(Some(&tag));
    }
    tag
}

/// `input` as a tag: a semver version, its `v` optional, no tag or release on `origin` holds.
pub fn check_version(input: &str, existing: &[String], held: &[String]) -> Result<String, String> {
    let tag = input.trim();
    if tag.is_empty() {
        return Err("Type a version, like v1.2.3.".to_string());
    }
    let Some(version) = version_of(tag) else {
        return Err(format!("{tag} is not a semver version, like v1.2.3."));
    };
    if let Some(taken) = existing.iter().find(|other| version_of(other).as_ref() == Some(&version))
    {
        return Err(format!("{taken} already exists on origin."));
    }
    if let Some(taken) = held.iter().find(|other| version_of(other).as_ref() == Some(&version)) {
        return Err(format!("{taken} is held by a release on origin, a draft included."));
    }
    Ok(tag.to_string())
}

/// `HOST/OWNER/NAME`, the `--repo` spelling that names the host too.
fn repo_flag(repository: &RepoTarget) -> String {
    format!("{}/{}/{}", repository.host(), repository.owner(), repository.name())
}

/// GitHub's own release-notes generation, the call behind its web button. It stores nothing.
#[must_use]
pub fn generate_args(
    repository: &RepoTarget,
    tag: &str,
    target: &str,
    previous: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "api".to_string(),
        "--hostname".to_string(),
        repository.host().to_string(),
        "--method".to_string(),
        "POST".to_string(),
        format!("repos/{}/{}/releases/generate-notes", repository.owner(), repository.name()),
        "-f".to_string(),
        format!("tag_name={tag}"),
        "-f".to_string(),
        format!("target_commitish={target}"),
    ];
    if let Some(previous) = previous {
        args.extend(["-f".to_string(), format!("previous_tag_name={previous}")]);
    }
    args
}

/// The discussion categories, and whether discussions are on at all.
fn categories_args(repository: &RepoTarget) -> Vec<String> {
    let query = "query($o:String!,$n:String!){repository(owner:$o,name:$n){\
                 hasDiscussionsEnabled discussionCategories(first:50){nodes{name}}}}";
    vec![
        "api".to_string(),
        "graphql".to_string(),
        "--hostname".to_string(),
        repository.host().to_string(),
        "-f".to_string(),
        format!("query={query}"),
        "-f".to_string(),
        format!("o={}", repository.owner()),
        "-f".to_string(),
        format!("n={}", repository.name()),
    ]
}

/// Every release's tag on `origin`, drafts included, one per line, all pages read.
fn release_tags_args(repository: &RepoTarget) -> Vec<String> {
    vec![
        "api".to_string(),
        "--hostname".to_string(),
        repository.host().to_string(),
        "--paginate".to_string(),
        format!("repos/{}/{}/releases?per_page=100", repository.owner(), repository.name()),
        "--jq".to_string(),
        ".[].tag_name".to_string(),
    ]
}

/// The read that proves `tag` is free on `origin`, past the newest tags the list read.
fn tag_ref_args(repository: &RepoTarget, tag: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        "--hostname".to_string(),
        repository.host().to_string(),
        format!("repos/{}/{}/git/ref/tags/{tag}", repository.owner(), repository.name()),
    ]
}

/// The one GitHub write: a release or draft at the target; `=` keeps each value off a flag.
#[must_use]
pub fn create_args(release: &Publish) -> Vec<String> {
    let mut args = vec![
        "release".to_string(),
        "create".to_string(),
        release.tag.clone(),
        format!("--repo={}", repo_flag(&release.repository)),
        format!("--target={}", release.target),
        format!("--title={}", release.title),
        format!("--notes={}", release.notes),
    ];
    // A draft sends no latest flag: GitHub marks a release latest only as it publishes.
    if let Some(latest) = release.latest {
        args.push(format!("--latest={latest}"));
    }
    if release.prerelease {
        args.push("--prerelease".to_string());
    }
    if release.draft {
        args.push("--draft".to_string());
    }
    if let Some(category) = &release.discussion {
        args.push(format!("--discussion-category={category}"));
    }
    args
}

/// Read the discussion categories through `run`.
pub(crate) fn categories(
    run: &dyn Fn(&[String]) -> Result<String, GhError>,
    repository: &RepoTarget,
) -> Categories {
    let out = match run(&categories_args(repository)) {
        Ok(out) => out,
        Err(error) => return Categories::Failed(said(error)),
    };
    let Ok(response) = serde_json::from_str::<serde_json::Value>(&out) else {
        return Categories::Failed("GitHub returned no categories".to_string());
    };
    let node = &response["data"]["repository"];
    if node["hasDiscussionsEnabled"].as_bool() != Some(true) {
        return Categories::Off;
    }
    let names = node["discussionCategories"]["nodes"].as_array().into_iter().flatten();
    Categories::Ready(names.filter_map(|n| n["name"].as_str().map(String::from)).collect())
}

/// Run generate-notes through `run`, a `gh` runner, and return the notes.
pub(crate) fn generate(
    run: &dyn Fn(&[String]) -> Result<String, GhError>,
    args: &[String],
) -> Result<String, String> {
    let failed = |why: String| format!("GitHub could not generate notes: {why}");
    let out = run(args).map_err(|error| failed(said(error)))?;
    let response: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| failed(e.to_string()))?;
    response["body"].as_str().map(String::from).ok_or_else(|| failed("no notes returned".into()))
}

/// Send through `run`: refuse a tag `origin` already has, then create; the release's URL.
pub(crate) fn publish(
    run: &dyn Fn(&[String]) -> Result<String, GhError>,
    release: &Publish,
) -> Result<String, String> {
    match run(&tag_ref_args(&release.repository, &release.tag)) {
        Ok(_) => return Err(format!("{} already exists on origin.", release.tag)),
        Err(GhError::Other(message) | GhError::NotFound(message))
            if crate::forge::reports_status(&message.to_lowercase(), 404) => {}
        Err(error) => return Err(said(error)),
    }
    // Every release, past the newest the list read: a draft holds its tag with no git tag.
    let held = run(&release_tags_args(&release.repository)).map_err(said)?;
    let held: Vec<String> = held.lines().map(str::trim).map(String::from).collect();
    check_version(&release.tag, &[], &held)?;
    run(&create_args(release)).map(|url| url.trim().to_string()).map_err(said)
}

/// What `gh` said, as the screen shows it.
fn said(error: GhError) -> String {
    match error {
        GhError::NoGh => "GitHub CLI not found. Install `gh`.".to_string(),
        GhError::NotAuthed(host) => format!("Not signed in to {host}."),
        GhError::LocalGit(message) | GhError::NotFound(message) | GhError::Other(message) => {
            message
        }
    }
}

/// The real runner: `gh` in `repo` against `host`, off the frame loop by its caller.
pub(crate) fn gh_runner(
    repo: std::path::PathBuf,
    host: String,
) -> impl Fn(&[String]) -> Result<String, GhError> {
    move |args: &[String]| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::forge::gh(&repo, &host, &args, &std::sync::atomic::AtomicBool::new(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn target() -> RepoTarget {
        RepoTarget::new("github.com", "me", "tool").unwrap()
    }

    fn release(tag: &str, name: &str) -> Release {
        Release { tag: tag.into(), name: name.into(), ..Release::default() }
    }

    fn sample() -> Publish {
        Publish {
            repository: target(),
            tag: "v1.2.4".into(),
            target: "abc123".into(),
            title: "tool 1.2.4".into(),
            notes: "## What's Changed\n* a fix".into(),
            prerelease: false,
            latest: Some(true),
            draft: false,
            discussion: None,
        }
    }

    fn snapshot(root: bool) -> ReleasesSnapshot {
        let mut tags = std::collections::HashMap::new();
        tags.insert("c1".to_string(), vec!["v0.46.0".to_string()]);
        ReleasesSnapshot {
            repository: target(),
            branch: "main".into(),
            root: if root {
                vec![crate::releases::ReleaseCommit { oid: "c2".into(), ..Default::default() }]
            } else {
                vec![]
            },
            versions: vec![crate::releases::VersionTag {
                tag: "v0.46.0".into(),
                oid: "c1".into(),
                commit: crate::releases::ReleaseCommit::default(),
            }],
            tags,
            releases: vec![release("v0.46.0", "tool 0.46.0")],
            upstream: None,
        }
    }

    fn draft() -> ReleaseDraft {
        ReleaseDraft::new(&snapshot(true), 7, "c2").unwrap()
    }

    fn typed(draft: &mut ReleaseDraft, text: &str) {
        for ch in text.chars() {
            draft.edit(|t| t.insert(ch));
        }
    }

    #[test]
    fn one_leading_v_is_a_tag_style_and_two_are_not_a_version() {
        assert_eq!(version_of("v1.2.3"), crate::releases::version("v1.2.3"));
        assert!(version_of("vv1.2.3").is_none());
        assert!(check_version("vv1.2.3", &[], &[]).is_err());
    }

    #[test]
    fn the_next_patch_keeps_the_tag_style() {
        assert_eq!(next_patch(Some("v0.46.0")), "v0.46.1");
        assert_eq!(next_patch(Some("1.9.9")), "1.9.10");
        assert_eq!(next_patch(Some("v2.0.0-rc.1")), "v2.0.1");
        assert_eq!(next_patch(None), "v0.1.0");
        assert_eq!(next_patch(Some("nightly")), "v0.1.0");
    }

    #[test]
    fn the_title_follows_the_latest_release_pattern_else_the_tag() {
        assert_eq!(
            title_for(Some(&release("v0.46.0", "herdr-reviewr 0.46.0")), "v0.46.1"),
            "herdr-reviewr 0.46.1"
        );
        assert_eq!(title_for(Some(&release("v2", "Release v2")), "v3"), "Release v3");
        assert_eq!(title_for(Some(&release("v2.0.0", "Big one")), "v2.0.1"), "v2.0.1");
        assert_eq!(title_for(None, "v0.1.0"), "v0.1.0");
    }

    #[test]
    fn a_version_must_be_semver_and_new_on_origin() {
        let existing = vec!["v0.46.0".to_string(), "nightly".to_string()];
        assert_eq!(check_version(" v0.46.1 ", &existing, &[]), Ok("v0.46.1".into()));
        assert_eq!(check_version("0.47.0", &existing, &[]), Ok("0.47.0".into()));
        assert!(check_version("", &existing, &[]).unwrap_err().contains("Type a version"));
        assert!(
            check_version("v0.46", &existing, &[]).unwrap_err().contains("not a semver version")
        );
        assert!(check_version("nightly", &existing, &[]).unwrap_err().contains("not a semver"));
        assert_eq!(
            check_version("v0.46.0", &existing, &[]),
            Err("v0.46.0 already exists on origin.".into())
        );
        assert_eq!(
            check_version("0.46.0", &existing, &[]),
            Err("v0.46.0 already exists on origin.".into()),
            "the same version in the other spelling is taken too"
        );
    }

    #[test]
    fn a_text_field_edits_at_its_caret_and_walks_lines() {
        let mut field = TextField::new("ab\ncde");
        assert_eq!(field.line_col(), (1, 3));
        assert!(field.vertical(false));
        assert_eq!(field.line_col(), (0, 2), "the column clamps to the shorter line");
        field.insert('X');
        assert_eq!(field.text, "abX\ncde");
        field.home();
        field.delete();
        assert_eq!(field.text, "bX\ncde");
        field.end();
        field.backspace();
        assert_eq!(field.text, "b\ncde");
        assert!(field.vertical(true));
        assert!(!field.vertical(true), "the last line hands the key back");
        field.left();
        field.right();
        field.right();
        assert_eq!(field.line_col(), (1, 2));
    }

    #[test]
    fn every_field_takes_typing_at_once_and_toggles_flip() {
        let mut draft = draft();
        assert_eq!(
            (draft.tag.text.as_str(), draft.title.text.as_str()),
            ("v0.46.1", "tool 0.46.1")
        );
        assert_eq!((draft.target.as_str(), draft.previous.as_deref()), ("c2", Some("v0.46.0")));
        draft.edit(TextField::backspace);
        typed(&mut draft, "2");
        assert_eq!(draft.title.text, "tool 0.46.2", "the title follows the tag");
        draft.step_field(true);
        typed(&mut draft, "!");
        draft.focus(Field::Tag);
        draft.edit(TextField::backspace);
        typed(&mut draft, "3");
        assert_eq!(draft.title.text, "tool 0.46.2!", "a typed title stays");
        assert_eq!(FIELDS.iter().filter(|f| f.multiline()).count(), 1, "the notes alone wrap");
        draft.focus(Field::Notes);
        typed(&mut draft, "line one");
        draft.edit(|t| t.insert('\n'));
        typed(&mut draft, "line two");
        assert_eq!(draft.notes.text, "line one\nline two");
        assert!(draft.takes_newline());
        draft.focus(Field::Prerelease);
        assert!(draft.text_mut().is_none());
        draft.toggle(true);
        assert!(draft.prerelease && !draft.latest, "a pre-release is never latest");
        draft.focus(Field::Latest);
        draft.toggle(true);
        assert!(!draft.latest, "and latest stays off while it is one");
        draft.focus(Field::Prerelease);
        draft.toggle(true);
        draft.focus(Field::Latest);
        draft.toggle(true);
        assert!(!draft.latest && !draft.prerelease);
        draft.step_field(false);
        assert_eq!(draft.field, Field::Prerelease);
        draft.focus(Field::Notes);
        draft.step_field(true);
        assert_eq!(draft.field, Field::Tag, "tab wraps");
    }

    #[test]
    fn the_discussion_steps_through_its_categories_and_back_to_none() {
        let mut draft = draft();
        draft.focus(Field::Discussion);
        draft.toggle(true);
        assert_eq!(draft.discussion, None, "no categories yet, nothing to pick");
        draft.categories_read(6, Categories::Ready(vec!["Stale".into()]));
        assert_eq!(draft.categories, Categories::Loading, "another form's read never lands");
        draft.categories_read(7, Categories::Ready(vec!["Announcements".into(), "Q&A".into()]));
        draft.toggle(true);
        assert_eq!(draft.discussion, Some(0));
        draft.toggle(true);
        draft.toggle(true);
        assert_eq!(draft.discussion, None, "past the last, none");
        draft.toggle(false);
        assert_eq!(draft.discussion, Some(1));
        assert_eq!(draft.checked(Finish::Publish).unwrap().discussion.as_deref(), Some("Q&A"));
    }

    #[test]
    fn every_option_maps_to_its_gh_flag() {
        assert_eq!(
            create_args(&sample()),
            [
                "release",
                "create",
                "v1.2.4",
                "--repo=github.com/me/tool",
                "--target=abc123",
                "--title=tool 1.2.4",
                "--notes=## What's Changed\n* a fix",
                "--latest=true",
            ]
        );
        let every = Publish {
            prerelease: true,
            latest: None,
            draft: true,
            discussion: Some("Announcements".into()),
            ..sample()
        };
        assert_eq!(
            create_args(&every)[7..],
            ["--prerelease", "--draft", "--discussion-category=Announcements"],
            "a draft sends no latest flag"
        );
        let dashed = Publish { title: "--draft".into(), notes: "-x".into(), ..sample() };
        let args = create_args(&dashed);
        assert!(
            args.contains(&"--title=--draft".to_string()) && !args.contains(&"--draft".to_string()),
            "a value never takes a flag's place: {args:?}"
        );
        let pre = Publish { prerelease: true, latest: Some(false), ..sample() };
        assert_eq!(create_args(&pre)[7..], ["--latest=false", "--prerelease"]);
    }

    #[test]
    fn the_target_is_the_selected_unreleased_commit_and_notes_start_at_the_highest_version() {
        let mut snapshot = snapshot(true);
        snapshot.root.push(crate::releases::ReleaseCommit {
            oid: "c3".into(),
            subject: "older work".into(),
            ..Default::default()
        });
        let mut draft = ReleaseDraft::new(&snapshot, 1, "c3").unwrap();
        assert_eq!((draft.target.as_str(), draft.target_subject.as_str()), ("c3", "older work"));
        assert_eq!(draft.checked(Finish::Publish).unwrap().target, "c3");
        let Some(Request::Generate { args, .. }) = draft.generate() else { panic!("generate") };
        assert!(args.contains(&"target_commitish=c3".to_string()), "{args:?}");
        assert!(args.contains(&"previous_tag_name=v0.46.0".to_string()), "{args:?}");
        assert_eq!(ReleaseDraft::new(&snapshot, 1, "c1"), None, "a released commit is no target");
        assert_eq!(ReleaseDraft::new(&snapshot, 1, "nope"), None);
    }

    #[test]
    fn generate_asks_github_for_that_tag_target_and_previous_version() {
        assert_eq!(
            generate_args(&target(), "v1.2.4", "abc123", Some("v1.2.3")),
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/me/tool/releases/generate-notes",
                "-f",
                "tag_name=v1.2.4",
                "-f",
                "target_commitish=abc123",
                "-f",
                "previous_tag_name=v1.2.3",
            ]
        );
        let first = generate_args(&target(), "v0.1.0", "abc123", None);
        let body = r###"{"name": "v1.2.4", "body": "## What's Changed"}"###;
        assert_eq!(generate(&|_| Ok(body.into()), &first), Ok("## What's Changed".into()));
        assert_eq!(
            generate(&|_| Err(GhError::Other("HTTP 422".into())), &first),
            Err("GitHub could not generate notes: HTTP 422".into())
        );
    }

    #[test]
    fn categories_read_on_off_and_failed() {
        let on = r#"{"data":{"repository":{"hasDiscussionsEnabled":true,
            "discussionCategories":{"nodes":[{"name":"Announcements"},{"name":"Ideas"}]}}}}"#;
        let off = r#"{"data":{"repository":{"hasDiscussionsEnabled":false,
            "discussionCategories":{"nodes":[]}}}}"#;
        assert_eq!(
            categories(&|_| Ok(on.into()), &target()),
            Categories::Ready(vec!["Announcements".into(), "Ideas".into()])
        );
        assert_eq!(categories(&|_| Ok(off.into()), &target()), Categories::Off);
        assert_eq!(
            categories(&|_| Err(GhError::NotAuthed("github.com".into())), &target()),
            Categories::Failed("Not signed in to github.com.".into())
        );
        let args = categories_args(&target());
        assert!(args.starts_with(&["api".into(), "graphql".into()]), "a read: {args:?}");
    }

    #[test]
    fn publish_sends_the_read_then_the_release_and_nothing_else() {
        let calls = RefCell::new(Vec::new());
        let run = |args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            if args.contains(&"--paginate".to_string()) {
                Ok("v1.2.3\nv1.2.2\n".into())
            } else if args[0] == "api" {
                Err(GhError::Other("gh: Not Found (HTTP 404)".into()))
            } else {
                Ok("https://github.com/me/tool/releases/tag/v1.2.4\n".into())
            }
        };
        let draft = Publish { draft: true, prerelease: true, latest: None, ..sample() };
        assert_eq!(
            publish(&run, &draft),
            Ok("https://github.com/me/tool/releases/tag/v1.2.4".into())
        );
        let calls = calls.into_inner();
        assert_eq!(
            calls[0],
            ["api", "--hostname", "github.com", "repos/me/tool/git/ref/tags/v1.2.4"]
        );
        assert_eq!(calls[1], release_tags_args(&target()), "every release's tag, all pages");
        assert_eq!(calls[2], create_args(&draft));
        assert_eq!(calls.len(), 3, "two reads, then the release; GitHub cuts its tag");
    }

    #[test]
    fn publish_refuses_a_taken_tag_and_reports_a_failure_without_creating() {
        let calls = RefCell::new(0);
        let taken = |args: &[String]| {
            *calls.borrow_mut() += 1;
            assert_eq!(args[0], "api", "only the read runs");
            Ok("{\"ref\": \"refs/tags/v1.2.4\"}".into())
        };
        assert_eq!(publish(&taken, &sample()), Err("v1.2.4 already exists on origin.".into()));
        assert_eq!(*calls.borrow(), 1);
        let unauthed = |args: &[String]| {
            assert_eq!(args[0], "api", "an unreadable tag never creates");
            Err(GhError::NotAuthed("github.com".into()))
        };
        assert_eq!(publish(&unauthed, &sample()), Err("Not signed in to github.com.".into()));
        let refused = |args: &[String]| {
            if args.contains(&"--paginate".to_string()) {
                Ok(String::new())
            } else if args[0] == "api" {
                Err(GhError::Other("HTTP 404".into()))
            } else {
                Err(GhError::Other("HTTP 403: Resource not accessible by integration".into()))
            }
        };
        assert_eq!(
            publish(&refused, &sample()),
            Err("HTTP 403: Resource not accessible by integration".into()),
            "gh's own words"
        );
        // A draft holds its tag with no git tag; the send refuses it all the same.
        let calls = RefCell::new(Vec::new());
        let drafted = |args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            if args.contains(&"--paginate".to_string()) {
                Ok("v9.0.0\n1.2.4\n".into())
            } else {
                Err(GhError::Other("gh: Not Found (HTTP 404)".into()))
            }
        };
        assert_eq!(
            publish(&drafted, &sample()),
            Err("1.2.4 is held by a release on origin, a draft included.".into())
        );
        assert!(calls.into_inner().iter().all(|args| args[0] == "api"), "nothing is created");
    }

    #[test]
    fn a_tag_a_draft_holds_is_taken_and_the_prefill_steps_past_it() {
        let held = vec!["v0.46.1".to_string()];
        assert_eq!(
            check_version("v0.46.1", &[], &held),
            Err("v0.46.1 is held by a release on origin, a draft included.".into())
        );
        assert!(check_version("0.46.1", &[], &held).is_err(), "in either spelling");
        assert_eq!(next_free(Some("v0.46.0"), &[], &held), "v0.46.2");
        assert_eq!(next_free(Some("v0.46.0"), &[], &[]), "v0.46.1");
        let mut snapshot = snapshot(true);
        snapshot.releases.insert(0, Release { draft: true, ..release("v0.46.1", "") });
        let draft = ReleaseDraft::new(&snapshot, 1, "c2").unwrap();
        assert_eq!(draft.tag.text, "v0.46.2", "the prefill never proposes a draft's tag");
        let mut taken = draft;
        taken.tag.set("v0.46.1");
        assert!(taken.checked(Finish::Draft).unwrap_err().contains("held by a release"));
    }

    #[test]
    fn a_draft_is_never_latest_and_the_review_keeps_what_it_checked() {
        let mut draft = draft();
        assert_eq!(draft.checked(Finish::Draft).unwrap().latest, None);
        assert_eq!(draft.checked(Finish::Publish).unwrap().latest, Some(true));
        draft.review(Finish::Draft);
        let reviewed = draft.reviewed.clone().expect("the review keeps what it checked");
        assert_eq!(reviewed.release, draft.checked(Finish::Draft).unwrap());
        draft.back();
        assert_eq!(draft.reviewed, None, "leaving the review drops it");
    }

    #[test]
    fn a_paste_lands_in_the_focused_field_and_never_at_the_review() {
        let mut draft = draft();
        draft.focus(Field::Notes);
        draft.paste("## Changes\r\n- one\n- two");
        assert_eq!(draft.notes.text, "## Changes\n- one\n- two");
        draft.focus(Field::Title);
        draft.paste(" (a\nb)");
        assert_eq!(draft.title.text, "tool 0.46.1 (a b)", "a single-line field takes one line");
        draft.focus(Field::Prerelease);
        draft.paste("x");
        assert!(!draft.prerelease, "a toggle takes no paste");
        draft.focus(Field::Title);
        draft.review(Finish::Publish);
        draft.paste("late");
        assert_eq!(draft.title.text, "tool 0.46.1 (a b)", "the review takes no paste");
        assert_eq!(draft.stage, Stage::Review(Finish::Publish));
    }

    #[test]
    fn nothing_sends_until_the_review_and_stale_notes_never_land() {
        assert_eq!(
            ReleaseDraft::new(&snapshot(false), 1, "c2"),
            None,
            "nothing waits to be released"
        );
        let mut draft = draft();
        assert_eq!(draft.confirm(), None, "editing never sends");
        // A taken tag stops at the review, with the reason.
        draft.tag.set("v0.46.0");
        draft.review(Finish::Publish);
        assert_eq!(draft.stage, Stage::Edit);
        assert_eq!(draft.error.as_deref(), Some("v0.46.0 already exists on origin."));
        assert_eq!(draft.generate(), None, "nor does it generate");
        draft.focus(Field::Tag);
        draft.edit(|t| t.set("v0.46.3"));
        let Some(Request::Generate { id: 7, seq, host, args }) = draft.generate() else {
            panic!("generate")
        };
        assert!(args.contains(&"tag_name=v0.46.3".to_string()) && draft.generating);
        assert_eq!(host, "github.com");
        draft.review(Finish::Publish);
        assert_eq!(draft.stage, Stage::Edit, "no review while the notes generate");
        assert_eq!(draft.generate(), None, "one generation at a time");
        draft.generated(6, seq, Ok("stale".into()));
        assert_eq!(draft.notes.text, "", "another draft's notes never land");
        draft.generated(7, seq + 1, Ok("stale".into()));
        assert_eq!(draft.notes.text, "", "another request's notes never land");
        draft.generated(7, seq, Ok("## Notes".into()));
        assert_eq!((draft.notes.text.as_str(), draft.generating), ("## Notes", false));
        // Notes typed while generating win over the generated ones.
        let Some(Request::Generate { seq: old, .. }) = draft.generate() else { panic!() };
        draft.focus(Field::Notes);
        draft.edit(|t| t.insert('!'));
        assert!(!draft.generating, "an edit drops the request in flight");
        draft.generated(7, old, Ok("generated over the edit".into()));
        assert_eq!(draft.notes.text, "## Notes!", "the edit stays");
        draft.review(Finish::Draft);
        assert_eq!(draft.stage, Stage::Review(Finish::Draft));
        draft.back();
        assert_eq!(draft.stage, Stage::Edit);
        assert_eq!(draft.confirm(), None);
        draft.review(Finish::Publish);
        let Some(Request::Publish { id: 7, release }) = draft.confirm() else { panic!("send") };
        assert_eq!(
            release,
            Publish {
                repository: target(),
                tag: "v0.46.3".into(),
                target: "c2".into(),
                title: "tool 0.46.3".into(),
                notes: "## Notes!".into(),
                prerelease: false,
                latest: Some(true),
                draft: false,
                discussion: None,
            }
        );
        assert_eq!(draft.confirm(), None, "one confirm, one send");
        draft.failed("HTTP 422".into());
        assert_eq!((draft.stage.clone(), draft.error.as_deref()), (Stage::Edit, Some("HTTP 422")));
    }
}
