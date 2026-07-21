//! Integration tests for the PR fetch's local reads (`git::pr_local`,
//! `git::ahead_behind_oids`) against real temp repos. Remote-tracking branches are faked
//! with `git update-ref refs/remotes/origin/<name> <sha>` — no network, no `gh`.
//! See `specs/forge-host.md` "Resolution".

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

fn point_oids(local: &PrLocalState) -> Vec<&str> {
    local.points.iter().map(|p| p.oid.as_str()).collect()
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
fn a_standard_fork_uses_the_base_repository_and_queries_the_origin() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "git@github.com:contributor/widgets.git"]);
    repo.git(&["remote", "add", "upstream", "https://github.com/acme/widgets.git"]);

    let input = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_target(&input.repository, "github.com", "acme", "widgets");
    // The association query runs where the commits live: the fork.
    let origin = input.origin_repository.expect("origin identity");
    assert_eq!((origin.owner(), origin.name()), ("contributor", "widgets"));
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
fn push_head_other_name_publishes_head_as_the_point_with_that_name() {
    // The headline workflow: `git push origin HEAD:other` with no `-u` updates the
    // remote-tracking ref; HEAD itself is the publication point, carrying the name.
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    let tip = head(&repo);
    assert_eq!(local.head_oid.as_deref(), Some(tip.as_str()));
    assert_eq!(point_oids(&local), [tip.as_str()]);
    assert_eq!(local.points[0].names, ["other"]);
    assert_eq!(local.upstream, None);
}

#[test]
fn unpushed_commits_move_the_point_to_the_published_boundary() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    let published = head(&repo);
    repo.write("c.txt", "three\n");
    repo.commit_all("unpushed");
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(point_oids(&local), [published.as_str()]);
    assert_eq!(local.points[0].names, ["other"]);
}

#[test]
fn a_zero_work_worktree_has_no_points_even_among_sibling_branches() {
    // The parallel-worktree adversary: HEAD parked at (or behind) the base tip while
    // sibling branches with open PRs descend from it. Nothing is provable, nothing shows.
    let repo = worktree();
    repo.git(&["switch", "-qC", "work", "main"]); // zero work: HEAD == base tip
    repo.git(&["update-ref", "refs/remotes/origin/sibling", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.points.is_empty(), "a base-ancestor point proves nothing");

    // The Campaigns Fable shape: HEAD strictly behind the base tip.
    repo.git(&["switch", "-q", "main"]);
    repo.write("m.txt", "advance\n");
    repo.commit_all("main moves on");
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-q", "work"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.points.is_empty(), "an ancestor of the base proves nothing");
}

#[test]
fn recorded_upstream_rides_along_unless_it_names_a_base() {
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/pub"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.upstream.as_deref(), Some("pub"));

    // The record `git switch -c work origin/main` auto-writes is tracking, not
    // publication — it never joins the tiebreak.
    repo.git(&["config", "branch.work.merge", "refs/heads/main"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.upstream, None);
}

#[test]
fn secondary_configured_bases_also_exclude_points() {
    // Gitflow: `develop` wins the pin, but a point sitting on `main` history must still
    // prove nothing — the old name-exclusion covered every configured base.
    let repo = worktree();
    repo.git(&["switch", "-qc", "develop", "main"]);
    repo.write("d.txt", "dev\n");
    repo.commit_all("develop work");
    repo.git(&["update-ref", "refs/remotes/origin/develop", "HEAD"]);
    // main advances past develop's branch point; a sibling ref sits at its tip.
    repo.git(&["switch", "-q", "main"]);
    repo.write("m.txt", "release\n");
    repo.commit_all("release merge");
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    repo.git(&["update-ref", "refs/remotes/origin/release-pr", "HEAD"]);
    // The worktree parks at main's tip with zero work of its own.
    repo.git(&["switch", "-qC", "work", "main"]);

    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("config.toml"),
        "base_branches = [\"develop\", \"main\"]\n",
    )
    .unwrap();
    let config = plugin_config_in(config_dir.path()).unwrap();
    let local = pr_local_with_config(repo.path(), None, config.base_branches()).expect("pr_local");
    let develop_tip = repo.git(&["rev-parse", "origin/develop"]).trim().to_string();
    assert_eq!(local.base_oid.as_deref(), Some(develop_tip.as_str()), "develop wins the pin");
    assert!(local.points.is_empty(), "a point on main history proves nothing");
}

#[test]
fn a_parked_merged_tip_survives_as_an_absorbed_candidate() {
    // The worktree's branch merged into main and the worktree stays parked at its tip:
    // no point survives, but the tip rides along for the exact-head epilogue.
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/fix", "HEAD"]);
    let merged_tip = head(&repo);
    repo.git(&["switch", "-q", "main"]);
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge fix", "work"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-q", "work"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.points.is_empty(), "the tip is base history now");
    assert_eq!(local.absorbed, [merged_tip], "the parked tip nominates the epilogue");

    // A surviving point clears the absorbed set — live work owns the resolution.
    repo.write("n.txt", "new\n");
    repo.commit_all("new work");
    repo.git(&["update-ref", "refs/remotes/origin/fix2", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(!local.points.is_empty());
    assert!(local.absorbed.is_empty());
}

#[test]
fn head_nominates_only_outside_every_resolved_base() {
    // A branch with its own commit past main: HEAD is provably this worktree's work.
    let repo = worktree();
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.head_nominates, "a HEAD beyond the base nominates by exact identity");

    // A zero-work worktree parked at the base tip: HEAD is base history, so a stranger's
    // PR head parked at the same commit must never attach.
    repo.git(&["switch", "-qC", "work", "main"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(!local.head_nominates, "a HEAD in base history proves nothing");
}

#[test]
fn a_squash_merged_tip_with_a_deleted_branch_still_nominates_by_head_identity() {
    // The deleted-branch epilogue: main absorbed the work as a different commit (squash)
    // and the remote branch is gone, so no origin ref reaches the parked tip. Points and
    // absorbed cannot prove the PR — only the exact-identity HEAD can.
    let repo = worktree();
    repo.git(&["switch", "-q", "main"]);
    repo.git(&["merge", "-q", "--squash", "work"]);
    repo.git(&["commit", "-qm", "squash of work"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-q", "work"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.points.is_empty(), "the orphaned tip is on no origin ref");
    assert!(local.head_nominates, "the squashed tip is not base history");
}

#[test]
fn a_point_carries_every_origin_name_at_its_tip() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/feat", "HEAD"]);
    repo.git(&["update-ref", "refs/remotes/origin/backup", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.points.len(), 1);
    assert_eq!(local.points[0].names, ["backup", "feat"], "every tip name, refname order");
}

#[test]
fn the_base_flag_resolves_verbatim_revs_before_canonical_entries() {
    let repo = worktree();
    // A raw SHA works verbatim, exactly as the flag always did.
    let main_tip = repo.git(&["rev-parse", "main"]).trim().to_string();
    let local = pr_local(repo.path(), Some(&main_tip)).expect("pr_local");
    assert_eq!(local.base_oid.as_deref(), Some(main_tip.as_str()));
    // A non-origin remote-tracking ref works verbatim too (the fork-review flag).
    repo.git(&["update-ref", "refs/remotes/upstream/main", "main"]);
    let local = pr_local(repo.path(), Some("upstream/main")).expect("pr_local");
    assert_eq!(local.base_oid.as_deref(), Some(main_tip.as_str()));
}

#[test]
fn without_a_resolvable_base_no_point_is_provable() {
    // A repo whose only branch is `trunk` and no origin/HEAD: no base resolves, so no
    // point can be proven beyond it (`specs/forge-host.md`).
    let repo = Repo::init();
    repo.git(&["branch", "-qm", "trunk"]);
    repo.write("a.txt", "one\n");
    repo.commit_all("first");
    repo.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    repo.git(&["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.base_oid, None);
    assert!(local.points.is_empty());
    assert!(!local.head_nominates, "no base resolved, so nothing can prove HEAD");

    // origin/HEAD backstops the unresolvable list (`specs/review-model.md`).
    repo.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]);
    repo.write("b.txt", "two\n");
    repo.commit_all("beyond trunk");
    repo.git(&["update-ref", "refs/remotes/origin/feat", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.base_oid.is_some(), "origin/HEAD resolves the base");
    assert_eq!(point_oids(&local), [head(&repo).as_str()]);
}

#[test]
fn base_entries_canonicalize_and_resolve_origin_first() {
    let repo = worktree();
    // `origin/main` and `main` are one entry; both pin the same base.
    let spelled = pr_local(repo.path(), Some("origin/main")).expect("pr_local");
    let bare = pr_local(repo.path(), Some("main")).expect("pr_local");
    assert_eq!(spelled.base_oid, bare.base_oid);
    assert!(spelled.base_oid.is_some());

    // A stale local base loses to the origin tracking ref (`specs/config.md`).
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.base_oid.as_deref(), Some(head(&repo).as_str()));
    assert!(local.points.is_empty(), "everything is base history under the fresh origin ref");
}

#[test]
fn detached_head_and_unborn_branch_are_clean_absences() {
    let repo = worktree();
    repo.git(&["switch", "-q", "--detach", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.detached, "detached HEAD is its own state");
    assert!(local.points.is_empty());

    // A fresh `git init`: a branch with no commits, nothing published.
    let fresh = Repo::init();
    let local = pr_local(fresh.path(), None).expect("pr_local");
    assert_eq!(local.head_oid, None);
    assert!(!local.detached);
    assert!(local.points.is_empty());
}

#[test]
fn a_missing_origin_is_absence_but_a_non_repo_is_failure() {
    let repo = Repo::init();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let input = fetch_input(repo.path(), None, &defaults()).expect("fetch input");
    assert_eq!(input.repository, RepositoryIdentity::Missing, "no origin is a clean absence");
    assert_eq!(input.origin_repository, None);
    assert!(pr_local(repo.path(), None).expect("pr_local").points.is_empty());

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
    repo.git(&["update-ref", "refs/remotes/origin/published", "HEAD"]);
    let first = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_eq!(fetch_input(repo.path(), Some("main"), &defaults()).unwrap(), first);

    // A pushed name at the same tip joins the point's names.
    repo.git(&["update-ref", "refs/remotes/origin/renamed", "HEAD"]);
    let names_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_ne!(names_changed, first);

    // A new commit moves the pinned HEAD (the point stays at the published tip).
    repo.write("new.txt", "new\n");
    repo.commit_all("new head");
    let head_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_ne!(head_changed, names_changed);

    // A different base pin changes the input.
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(config_dir.path().join("config.toml"), "base_branches = [\"work\"]\n").unwrap();
    let custom = plugin_config_in(config_dir.path()).unwrap();
    let base_changed = fetch_input(repo.path(), None, &custom).unwrap();
    assert_ne!(base_changed, head_changed);
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
