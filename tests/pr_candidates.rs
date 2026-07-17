//! Integration tests for the PR fetch's local reads (`git::pr_local`,
//! `git::ahead_behind_oids`) against real temp repos. Remote-tracking branches are faked
//! with `git update-ref refs/remotes/origin/<name> <sha>` — no network, no `gh`.
//! See `specs/forge-host.md` "Candidate branches".

mod common;

use common::Repo;
use herdr_reviewr::config::{PluginConfig, plugin_config_in};
use herdr_reviewr::forge::{PrInputError, fetch_input};
use herdr_reviewr::git::{
    GitFail, PrLocalState, RepositoryIdentity, ahead_behind_oids, pr_local as pr_local_with_config,
};
use std::io::Write;
use std::path::Path;

/// A repo on branch `work` (one commit past `main`), with a GitHub `origin` remote and
/// `origin/main` tracking-ref at `main`'s tip — the baseline every test builds on.
fn worktree() -> Repo {
    let repo = Repo::init();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    repo.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-qc", "work"]);
    repo.write("b.txt", "two\n");
    repo.commit_all("feature");
    repo
}

fn head(repo: &Repo) -> String {
    repo.git(&["rev-parse", "HEAD"]).trim().to_string()
}

fn defaults() -> PluginConfig {
    PluginConfig::default()
}

fn pr_local(repo: &Path, base: Option<&str>) -> Result<PrLocalState, GitFail> {
    let config = defaults();
    pr_local_with_config(repo, base, config.base_branches())
}

fn assert_target(identity: &RepositoryIdentity, host: &str, owner: &str, name: &str) {
    let RepositoryIdentity::Repository(target) = identity else {
        panic!("expected a repository target, got {identity:?}");
    };
    assert_eq!(target.host(), host);
    assert_eq!(target.owner(), owner);
    assert_eq!(target.name(), name);
}

#[test]
fn a_standard_fork_uses_the_base_repository_without_setup() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "git@github.com:contributor/widgets.git"]);
    repo.git(&["remote", "add", "upstream", "https://github.com/acme/widgets.git"]);

    let input = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_target(&input.repository, "github.com", "acme", "widgets");
}

#[test]
fn an_unusable_upstream_falls_back_to_origin() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "https://github.com/acme/widgets.git"]);
    let selected = || fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_target(&selected().repository, "github.com", "acme", "widgets");

    repo.git(&["remote", "add", "upstream", repo.path().to_str().unwrap()]);
    assert_target(&selected().repository, "github.com", "acme", "widgets");

    repo.git(&["remote", "set-url", "upstream", "https://gitlab.com/other/widgets.git"]);
    assert_target(&selected().repository, "github.com", "acme", "widgets");

    repo.git(&["remote", "set-url", "upstream", "https://github.com/acme"]);
    assert_target(&selected().repository, "github.com", "acme", "widgets");
}

#[test]
fn an_upstream_read_failure_never_falls_through_to_origin() {
    let repo = worktree();
    let mut config =
        std::fs::OpenOptions::new().append(true).open(repo.path().join(".git/config")).unwrap();
    config.write_all(b"\n[remote \"upstream\"]\n\turl = git@github.com:acme/\xff.git\n").unwrap();

    assert!(matches!(
        fetch_input(repo.path(), None, &defaults()),
        Err(PrInputError::TargetRead(message)) if message.contains("invalid UTF-8")
    ));
}

#[test]
fn a_github_com_prefixed_host_is_only_supported_when_configured_literally() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "https://github.com/acme/widgets.git"]);
    repo.git(&["remote", "add", "upstream", "git@github.com-work:enterprise/widgets.git"]);

    let input = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_target(&input.repository, "github.com", "acme", "widgets");

    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(config_dir.path().join("config.toml"), "github_host = \"github.com-work\"\n")
        .unwrap();
    let config = plugin_config_in(config_dir.path()).unwrap();
    let input = fetch_input(repo.path(), None, &config).unwrap();
    assert_target(&input.repository, "github.com-work", "enterprise", "widgets");
}

#[test]
fn push_head_other_name_yields_the_remote_branch_before_the_local_name() {
    // The headline workflow: `git push origin HEAD:other` with no `-u` updates the
    // remote-tracking ref; the pushed name must outrank the (PR-less) local name.
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_oid.as_deref(), Some(head(&repo).as_str()));
    assert_eq!(local.candidates, ["other", "work"]);
}

#[test]
fn recorded_upstream_is_the_first_candidate() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/pub"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.candidates, ["pub", "other", "work"]);
}

#[test]
fn an_upstream_naming_a_base_branch_is_excluded() {
    // `git switch -c work origin/main` auto-tracks the base; that record is not a
    // publication and must not become a candidate.
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/main"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.candidates, ["work"]);
}

#[test]
fn stacked_branches_qualify_but_the_base_and_diverged_siblings_do_not() {
    let repo = worktree();
    // Ancestor carrying non-base work: origin/stack at `work`'s first commit.
    repo.git(&["update-ref", "refs/remotes/origin/stack", "HEAD"]);
    repo.write("c.txt", "three\n");
    repo.commit_all("more");
    // Descendant: origin/cont one commit past HEAD.
    repo.git(&["switch", "-qc", "cont"]);
    repo.write("d.txt", "four\n");
    repo.commit_all("continuation");
    let cont_tip = head(&repo);
    repo.git(&["switch", "-q", "work"]);
    repo.git(&["update-ref", "refs/remotes/origin/cont", &cont_tip]);
    // Diverged sibling: branches off `main`, not comparable with HEAD.
    repo.git(&["switch", "-qc", "sibling", "main"]);
    repo.write("e.txt", "five\n");
    repo.commit_all("elsewhere");
    let sibling_tip = head(&repo);
    repo.git(&["switch", "-q", "work"]);
    repo.git(&["branch", "-qD", "cont", "sibling"]);
    repo.git(&["update-ref", "refs/remotes/origin/sibling", &sibling_tip]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    // stack (ancestor, distance 1) and cont (descendant, distance 1) qualify, ordered
    // lexicographically on the tie; origin/main (base) and the diverged sibling do not.
    assert_eq!(local.candidates, ["cont", "stack", "work"]);
}

#[test]
fn without_a_resolvable_base_only_equal_and_descendant_tips_qualify() {
    // A repo whose only branch is `trunk`: none of the default base names resolve, so
    // "an ancestor carrying non-base work" is undefined and ancestors drop out.
    let repo = Repo::init();
    repo.git(&["branch", "-qm", "trunk"]);
    repo.write("a.txt", "one\n");
    repo.commit_all("first");
    repo.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    repo.git(&["update-ref", "refs/remotes/origin/old", "HEAD"]);
    repo.write("b.txt", "two\n");
    repo.commit_all("second");
    repo.git(&["update-ref", "refs/remotes/origin/same", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    // `same` (equal tip) qualifies; `old` (ancestor) does not without a base to test against.
    assert_eq!(local.candidates, ["same", "trunk"]);
}

#[test]
fn the_base_flag_pins_the_base_and_joins_the_exclusion_set() {
    let repo = worktree();
    // A dev trunk the repo treats as base via --base; a branch merged into it must not
    // qualify, and origin/dev itself must be excluded by name.
    repo.git(&["update-ref", "refs/remotes/origin/dev", "HEAD"]);
    let local = pr_local(repo.path(), Some("origin/dev")).expect("pr_local");
    assert_eq!(local.candidates, ["work"]);
}

#[test]
fn nearest_first_and_the_cap_evicts_farthest_keeping_the_local_name() {
    let repo = worktree();
    // Nine remote names at increasing distances: d0 at HEAD, d1 one commit back, …
    // (each historical tip is an ancestor carrying non-base work).
    let mut tips = vec![head(&repo)];
    for i in 1..9 {
        repo.write(&format!("f{i}.txt"), "x\n");
        repo.commit_all(&format!("c{i}"));
        tips.push(head(&repo));
    }
    // tips[k] is k commits behind the final HEAD; name them so distance != lexical order.
    for (k, tip) in tips.iter().enumerate() {
        let dist = 8 - k; // tips[8] is HEAD (distance 0), tips[0] is distance 8
        repo.git(&["update-ref", &format!("refs/remotes/origin/d{dist}"), tip]);
    }
    let local = pr_local(repo.path(), None).expect("pr_local");
    // 9 tips + local = 10 names; the cap keeps the 7 nearest tips plus the local name.
    assert_eq!(local.candidates, ["d0", "d1", "d2", "d3", "d4", "d5", "d6", "work"]);
}

#[test]
fn detached_head_and_unborn_branch_are_clean_absences() {
    let repo = worktree();
    repo.git(&["switch", "-q", "--detach", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.candidates.is_empty(), "detached HEAD derives no candidates");

    // A fresh `git init`: a branch with no commits. The local name is still a candidate;
    // there is no HEAD to compare tips against.
    let fresh = Repo::init();
    let local = pr_local(fresh.path(), None).expect("pr_local");
    assert_eq!(local.head_oid, None);
    assert_eq!(local.candidates, ["main"]);
}

#[test]
fn a_missing_origin_is_absence_but_a_non_repo_is_failure() {
    let repo = Repo::init();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    assert_eq!(
        fetch_input(repo.path(), None, &defaults()).expect("fetch input").repository,
        RepositoryIdentity::Missing,
        "no origin remote is a clean absence"
    );
    assert_eq!(pr_local(repo.path(), None).expect("pr_local").candidates, ["main"]);

    let dir = tempfile::tempdir().unwrap();
    assert!(pr_local(dir.path(), None).is_err(), "a non-repo directory is a failure");
}

#[test]
fn fetch_input_uses_instead_of_rewrite_and_ignores_pushurl() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "corp:owner/repo.git"]);
    repo.git(&["config", "url.https://github.company.com/.insteadOf", "corp:"]);
    repo.git(&["remote", "set-url", "--push", "origin", "git@gitlab.com:owner/repo.git"]);

    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(config_dir.path().join("config.toml"), "github_host = \"github.company.com\"\n")
        .unwrap();
    let config = plugin_config_in(config_dir.path()).unwrap();
    let input = fetch_input(repo.path(), None, &config).expect("fetch input");
    assert_target(&input.repository, "github.company.com", "owner", "repo");
}

#[test]
fn fetch_input_changes_only_with_derived_query_state() {
    let repo = worktree();
    let first = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_eq!(fetch_input(repo.path(), Some("main"), &defaults()).unwrap(), first);

    repo.git(&["update-ref", "refs/remotes/origin/published", "HEAD"]);
    let candidate_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_ne!(candidate_changed, first);
    assert!(candidate_changed.candidates.contains(&"published".to_string()));

    repo.write("new.txt", "new\n");
    repo.commit_all("new head");
    let head_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_ne!(head_changed, candidate_changed);

    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("config.toml"),
        "base_branches = [\"origin/develop\", \"develop\"]\n",
    )
    .unwrap();
    let custom = plugin_config_in(config_dir.path()).unwrap();
    let base_changed = fetch_input(repo.path(), None, &custom).unwrap();
    assert_ne!(base_changed, head_changed);
    assert_ne!(base_changed.candidates, head_changed.candidates);
}

#[test]
fn ahead_behind_oids_counts_between_pins_and_tolerates_a_missing_head() {
    let repo = worktree();
    let main = repo.git(&["rev-parse", "main"]).trim().to_string();
    let work = head(&repo);
    assert_eq!(ahead_behind_oids(repo.path(), &work, &main).unwrap(), Some((1, 0)));
    assert_eq!(ahead_behind_oids(repo.path(), &main, &work).unwrap(), Some((0, 1)));
    assert_eq!(ahead_behind_oids(repo.path(), &work, &work).unwrap(), Some((0, 0)));
    // A PR head OID never fetched locally cannot be compared, but is not a git failure.
    let missing = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(ahead_behind_oids(repo.path(), &work, missing).unwrap(), None);
}
