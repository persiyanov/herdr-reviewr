//! Integration tests for the PR fetch's local reads (`git::pr_local`,
//! `git::contains_commit`, `git::ahead_behind_oids`) against real temp repos.
//! Remote-tracking branches are faked with `git update-ref
//! refs/remotes/origin/<name> <sha>` — no network, no `gh`.
//! See `specs/forge-host.md` "Resolution".

mod common;

use common::Repo;
use herdr_reviewr::config::{PluginConfig, plugin_config_in};
use herdr_reviewr::forge::{Association, PrInputError, assoc_history, fetch_input, resolve_pick};
use herdr_reviewr::git::{
    GitFail, PrLocalState, RepositoryIdentity, ahead_behind_oids, contains_commit,
};
use std::io::Write;
use std::path::Path;

/// A repo on branch `work` (one commit past `main`), with a GitHub `origin` remote,
/// `origin/main` tracking-ref at `main`'s tip, and `origin/HEAD` naming `main` the
/// default branch — the baseline every test builds on.
fn worktree() -> Repo {
    let repo = Repo::init();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    repo.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    repo.set_origin_default("main", "main");
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
    herdr_reviewr::git::pr_local(repo, base)
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
    // The fork rides along for the dual-repository lookup.
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

    repo.git(&["remote", "set-url", "upstream", "https://bitbucket.org/other/widgets.git"]);
    assert_target(&selected().repository, "github.com", "acme", "widgets");

    // A GitLab upstream is a recognized forge repository, so it wins target selection
    // (`specs/forge-host.md`).
    repo.git(&["remote", "set-url", "upstream", "https://gitlab.com/other/widgets.git"]);
    assert_target(&selected().repository, "gitlab.com", "other", "widgets");

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
fn push_head_other_name_adds_the_pushed_name() {
    // The headline workflow: `git push origin HEAD:other` with no `-u` updates the
    // remote-tracking ref; the pushed name joins the branch's forge names.
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_oid.as_deref(), Some(head(&repo).as_str()));
    assert_eq!(local.names, ["work", "other"]);
}

#[test]
fn unpushed_commits_keep_the_published_boundary_name() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    repo.write("c.txt", "three\n");
    repo.commit_all("unpushed");
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work", "other"]);
}

#[test]
fn a_zero_work_branch_carries_only_its_own_name() {
    // The parallel-worktree adversary: HEAD parked at (or behind) the base tip while
    // sibling branches with open PRs sit at it. Their names never join this branch's.
    let repo = worktree();
    repo.git(&["switch", "-qC", "work", "main"]); // zero work: HEAD == base tip
    repo.git(&["update-ref", "refs/remotes/origin/sibling", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work"], "a base-history tip contributes no name");

    // HEAD strictly behind the base tip: still only the branch's own name.
    repo.git(&["switch", "-q", "main"]);
    repo.write("m.txt", "advance\n");
    repo.commit_all("main moves on");
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-q", "work"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work"]);
}

#[test]
fn a_recorded_upstream_joins_the_names_unless_it_names_a_base() {
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/pub"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work", "pub"]);

    // The record `git switch -c work origin/main` auto-writes is tracking, not
    // publication — it never joins the names.
    repo.git(&["config", "branch.work.merge", "refs/heads/main"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work"]);
}

#[test]
fn every_resolved_base_source_excludes_names() {
    // Gitflow: the picked `develop` wins the pin, but a tip on the default `main`'s
    // history must still contribute no name — every source that resolved excludes.
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

    herdr_reviewr::git::write_base_pick(repo.path(), "develop").unwrap();
    let local = pr_local(repo.path(), None).expect("pr_local");
    let develop_tip = repo.git(&["rev-parse", "origin/develop"]).trim().to_string();
    assert_eq!(local.base_oid.as_deref(), Some(develop_tip.as_str()), "develop wins the pin");
    assert_eq!(local.names, ["work"], "a tip on main history contributes no name");
}

#[test]
fn a_dormant_pick_still_shields_its_name() {
    // The picked `develop` was never created, so it resolves to nothing, but the record stands:
    // an upstream naming it is still tracking a base, not publishing to it
    // (`specs/forge-host.md` Resolution — "resolved or recorded").
    let repo = worktree();
    herdr_reviewr::git::write_base_pick(repo.path(), "develop").unwrap();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/develop"]);

    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work"], "the dormant pick's name never joins");
}

#[test]
fn an_upstream_on_a_base_resolved_without_its_name_is_excluded_by_tip() {
    // The `develop`-default repo under the stock `main`/`master` config: the base
    // resolves only through `origin/HEAD`, so no configured entry carries its name.
    // The auto-written tracking record must still be recognized as a base, or the
    // base branch's own PR attaches to every branch cut from it
    // (`specs/forge-host.md` Resolution — "unless that names a resolved base").
    let repo = Repo::init();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    repo.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    repo.git(&["branch", "-qm", "develop"]);
    repo.write("d.txt", "dev\n");
    repo.commit_all("develop work");
    repo.set_origin_default("develop", "develop");
    let develop_tip = repo.git(&["rev-parse", "develop"]).trim().to_string();
    repo.git(&["switch", "-qc", "work"]);
    repo.write("b.txt", "two\n");
    repo.commit_all("feature");
    // The record `git switch -c work origin/develop` auto-writes.
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/develop"]);

    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.base_oid.as_deref(), Some(develop_tip.as_str()), "origin/HEAD wins the pin");
    assert_eq!(local.names, ["work"], "the default branch never joins the names");

    // A verbatim-rev flag (a raw SHA here) has no canonical name either; the tip
    // comparison still recognizes the upstream as that base.
    let local = pr_local(repo.path(), Some(&develop_tip)).expect("pr_local");
    assert_eq!(local.names, ["work"]);
}

#[test]
fn a_merged_branch_keeps_its_local_name_and_its_recorded_upstream() {
    // The worktree's branch merged into main and the worktree stays parked at its tip:
    // the frontier ref is base history now, so recall rides on the local name and the
    // recorded upstream (`specs/forge-host.md` Resolution — recall survives on the names
    // local records still carry).
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/fix", "HEAD"]);
    repo.git(&["switch", "-q", "main"]);
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge fix", "work"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-q", "work"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work"], "an absorbed frontier ref contributes no name");

    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/fix"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work", "fix"], "the recorded upstream survives the merge");
}

#[test]
fn resolve_pick_drives_the_ancestry_guard_against_a_real_repo() {
    // The history pick wired end to end: a finished PR admits on the branch that holds
    // its head commit and never on a fresh branch reusing the name
    // (`specs/forge-host.md` Resolution).
    let repo = worktree();
    let old_tip = head(&repo);
    repo.write("c.txt", "three\n");
    repo.commit_all("continue");
    let tip = head(&repo);
    let assoc = Association {
        open: Vec::new(),
        history: vec![assoc_history(9, &old_tip, "2026-07-01T00:00:00Z")],
    };
    let pick = resolve_pick(repo.path(), &assoc, Some(tip.as_str())).unwrap();
    assert_eq!(pick, Some(9), "the continuing branch holds the PR's head");

    // Two contained candidates: the newest close time wins, whatever the row order.
    let assoc = Association {
        open: Vec::new(),
        history: vec![
            assoc_history(9, &old_tip, "2026-07-01T00:00:00Z"),
            assoc_history(12, &tip, "2026-07-03T00:00:00Z"),
        ],
    };
    let pick = resolve_pick(repo.path(), &assoc, Some(tip.as_str())).unwrap();
    assert_eq!(pick, Some(12), "the newest contained finished PR wins");

    repo.git(&["switch", "-qC", "work", "main"]);
    let fresh = head(&repo);
    let pick = resolve_pick(repo.path(), &assoc, Some(fresh.as_str())).unwrap();
    assert_eq!(pick, None, "a fresh branch reusing the name admits nothing");
}

#[test]
fn the_reused_name_guard_admits_only_contained_history() {
    // The ancestry guard: a merged PR's head commit admits only when this branch holds
    // it (`specs/forge-host.md` Resolution).
    let repo = worktree();
    let old_tip = head(&repo);
    // Continuing on the branch: the old tip stays in history.
    repo.write("c.txt", "three\n");
    repo.commit_all("continue");
    assert!(contains_commit(repo.path(), &head(&repo), &old_tip).unwrap());
    // A fresh branch from main reusing the name does not contain it.
    repo.git(&["switch", "-qC", "work", "main"]);
    assert!(!contains_commit(repo.path(), &head(&repo), &old_tip).unwrap());
    // A commit absent from the object database proves nothing.
    let missing = "0123456789012345678901234567890123456789";
    assert!(!contains_commit(repo.path(), &head(&repo), missing).unwrap());
}

#[test]
fn an_on_base_agent_carries_the_side_branch_name_until_the_pull() {
    // The on-main agent flow: commits on local main, pushed as `HEAD:side`, PR from
    // `side`. After the merged result is pulled, main carries no side name and is empty
    // (`specs/forge-host.md` — a synced base branch is always empty).
    let repo = worktree();
    repo.git(&["switch", "-q", "main"]);
    repo.write("f.txt", "feature\n");
    repo.commit_all("agent work on main");
    repo.git(&["update-ref", "refs/remotes/origin/side", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["main", "side"], "the pushed side branch names the work");

    // The squash lands remotely; the agent pulls it. HEAD is base history again.
    repo.git(&["switch", "-q", "work"]);
    repo.git(&["switch", "-q", "main"]);
    repo.git(&["reset", "-q", "--hard", "origin/main"]);
    repo.write("s.txt", "squash\n");
    repo.commit_all("squash of side (#1)");
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["main"], "a synced base branch carries only its own name");
}

#[test]
fn every_origin_name_at_the_frontier_joins_in_refname_order() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/feat", "HEAD"]);
    repo.git(&["update-ref", "refs/remotes/origin/backup", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.names, ["work", "backup", "feat"]);
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
fn without_a_resolvable_base_no_frontier_name_joins() {
    // A repo whose only branch is `trunk` and no origin/HEAD: no base resolves, so no
    // frontier name can be proven beyond one (`specs/forge-host.md`).
    let repo = Repo::init();
    repo.git(&["branch", "-qm", "trunk"]);
    repo.write("a.txt", "one\n");
    repo.commit_all("first");
    repo.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    repo.git(&["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.base_oid, None);
    assert_eq!(local.names, ["trunk"]);

    // origin/HEAD backstops the unresolvable list (`specs/review-model.md`).
    repo.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]);
    repo.write("b.txt", "two\n");
    repo.commit_all("beyond trunk");
    repo.git(&["update-ref", "refs/remotes/origin/feat", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.base_oid.is_some(), "origin/HEAD resolves the base");
    assert_eq!(local.names, ["trunk", "feat"]);
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
    assert_eq!(local.names, ["work"], "everything is base history under the fresh origin ref");
}

#[test]
fn detached_head_and_unborn_branch_are_clean_absences() {
    let repo = worktree();
    repo.git(&["switch", "-q", "--detach", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.detached, "detached HEAD is its own state");
    assert!(local.names.is_empty(), "no branch, no names");

    // A fresh `git init`: a branch with no commits, nothing published.
    let fresh = Repo::init();
    let local = pr_local(fresh.path(), None).expect("pr_local");
    assert_eq!(local.head_oid, None);
    assert!(!local.detached);
    assert_eq!(local.names, ["main"], "an unborn branch still has its name");
}

#[test]
fn a_missing_origin_is_absence_but_a_non_repo_is_failure() {
    let repo = Repo::init();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let input = fetch_input(repo.path(), None, &defaults()).expect("fetch input");
    assert_eq!(input.repository, RepositoryIdentity::Missing, "no origin is a clean absence");
    assert_eq!(input.origin_repository, None);
    assert_eq!(pr_local(repo.path(), None).expect("pr_local").names, ["main"]);

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

    // A pushed name at the frontier joins the branch's names.
    repo.git(&["update-ref", "refs/remotes/origin/renamed", "HEAD"]);
    let names_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_ne!(names_changed, first);

    // A new commit moves the pinned HEAD (the names keep the published tip's).
    repo.write("new.txt", "new\n");
    repo.commit_all("new head");
    let head_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_ne!(head_changed, names_changed);

    // A base pick written by any pane changes the input; clearing it restores the default.
    herdr_reviewr::git::write_base_pick(repo.path(), "work").unwrap();
    let base_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
    assert_ne!(base_changed, head_changed);
    herdr_reviewr::git::clear_base_pick(repo.path()).unwrap();
    assert_eq!(fetch_input(repo.path(), None, &defaults()).unwrap(), head_changed);
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
