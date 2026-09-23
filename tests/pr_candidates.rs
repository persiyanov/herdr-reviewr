//! Integration tests for the PR fetch's local reads (`git::pr_local`,
//! `git::contains_commit`, `git::ahead_behind_oids`) against real temp repos.
//! Remote-tracking branches are faked with `git update-ref
//! refs/remotes/origin/<name> <sha>` — no network, no `gh`.

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

/// The local state the PR fetch derives, through the production path.
fn pr_local(repo: &Path, base: Option<&str>) -> Result<PrLocalState, GitFail> {
    fetch_input(repo, base, &defaults())
        .map(|input| input.local)
        .map_err(|e| GitFail(format!("{e:?}")))
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
    assert_eq!(local.head_names(), ["work", "other"]);
}

#[test]
fn unpushed_commits_keep_the_published_boundary_name() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    repo.write("c.txt", "three\n");
    repo.commit_all("unpushed");
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work", "other"]);
}

#[test]
fn a_zero_work_branch_carries_only_its_own_name() {
    // The parallel-worktree adversary: HEAD parked at (or behind) the base tip while
    // sibling branches with open PRs sit at it. Their names never join this branch's.
    let repo = worktree();
    repo.git(&["switch", "-qC", "work", "main"]); // zero work: HEAD == base tip
    repo.git(&["update-ref", "refs/remotes/origin/sibling", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work"], "a base-history tip contributes no name");

    // HEAD strictly behind the base tip: still only the branch's own name.
    repo.git(&["switch", "-q", "main"]);
    repo.write("m.txt", "advance\n");
    repo.commit_all("main moves on");
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-q", "work"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work"]);
}

#[test]
fn a_recorded_upstream_joins_the_names_unless_it_names_a_base() {
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/pub"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work", "pub"]);

    // The record `git switch -c work origin/main` auto-writes is tracking, not
    // publication — it never joins the names.
    repo.git(&["config", "branch.work.merge", "refs/heads/main"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work"]);
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
    assert_eq!(local.head_names(), ["work"], "a tip on main history contributes no name");
}

#[test]
fn a_dormant_pick_still_shields_its_name() {
    // The picked `develop` was never created, so it resolves to nothing, but the record stands:
    // an upstream naming it is still tracking a base, not publishing to it
    let repo = worktree();
    herdr_reviewr::git::write_base_pick(repo.path(), "develop").unwrap();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/develop"]);

    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work"], "the dormant pick's name never joins");
}

#[test]
fn an_upstream_on_a_base_resolved_without_its_name_is_excluded_by_tip() {
    // The `develop`-default repo under the stock `main`/`master` config: the base
    // resolves only through `origin/HEAD`, so no configured entry carries its name.
    // The auto-written tracking record must still be recognized as a base, or the
    // base branch's own PR attaches to every branch cut from it
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
    assert_eq!(local.head_names(), ["work"], "the default branch never joins the names");

    // A verbatim-rev flag (a raw SHA here) has no canonical name either; the tip
    // comparison still recognizes the upstream as that base.
    let local = pr_local(repo.path(), Some(&develop_tip)).expect("pr_local");
    assert_eq!(local.head_names(), ["work"]);
}

#[test]
fn a_merged_branch_keeps_its_local_name_and_its_recorded_upstream() {
    // The worktree's branch merged into main and the worktree stays parked at its tip:
    // the frontier ref is base history now, so recall rides on the local name and the
    // recorded upstream (recall survives on the names local records still carry).
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/fix", "HEAD"]);
    repo.git(&["switch", "-q", "main"]);
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge fix", "work"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    repo.git(&["switch", "-q", "work"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work"], "an absorbed frontier ref contributes no name");

    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/fix"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work", "fix"], "the recorded upstream survives the merge");
}

#[test]
fn resolve_pick_drives_the_ancestry_guard_against_a_real_repo() {
    // The history pick wired end to end: a finished PR admits on the branch that holds
    // its head commit and never on a fresh branch reusing the name
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
    // it.
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
    // (a synced base branch is always empty).
    let repo = worktree();
    repo.git(&["switch", "-q", "main"]);
    repo.write("f.txt", "feature\n");
    repo.commit_all("agent work on main");
    repo.git(&["update-ref", "refs/remotes/origin/side", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["main", "side"], "the pushed side branch names the work");

    // The squash lands remotely; the agent pulls it. HEAD is base history again.
    repo.git(&["switch", "-q", "work"]);
    repo.git(&["switch", "-q", "main"]);
    repo.git(&["reset", "-q", "--hard", "origin/main"]);
    repo.write("s.txt", "squash\n");
    repo.commit_all("squash of side (#1)");
    repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["main"], "a synced base branch carries only its own name");
}

#[test]
fn every_origin_name_at_the_frontier_joins_in_refname_order() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/feat", "HEAD"]);
    repo.git(&["update-ref", "refs/remotes/origin/backup", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.head_names(), ["work", "backup", "feat"]);
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
    // frontier name can be proven beyond one.
    let repo = Repo::init();
    repo.git(&["branch", "-qm", "trunk"]);
    repo.write("a.txt", "one\n");
    repo.commit_all("first");
    repo.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    repo.git(&["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.base_oid, None);
    assert_eq!(local.head_names(), ["trunk"]);

    // origin/HEAD backstops the unresolvable list.
    repo.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]);
    repo.write("b.txt", "two\n");
    repo.commit_all("beyond trunk");
    repo.git(&["update-ref", "refs/remotes/origin/feat", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert!(local.base_oid.is_some(), "origin/HEAD resolves the base");
    assert_eq!(local.head_names(), ["trunk", "feat"]);
}

#[test]
fn base_entries_canonicalize_and_resolve_origin_first() {
    let repo = worktree();
    // `origin/main` and `main` are one entry; both pin the same base.
    let spelled = pr_local(repo.path(), Some("origin/main")).expect("pr_local");
    let bare = pr_local(repo.path(), Some("main")).expect("pr_local");
    assert_eq!(spelled.base_oid, bare.base_oid);
    assert!(spelled.base_oid.is_some());

    // A stale local base loses to the origin tracking ref.
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.base_oid.as_deref(), Some(head(&repo).as_str()));
    assert_eq!(
        local.head_names(),
        ["work"],
        "everything is base history under the fresh origin ref"
    );
}

/// The derived heads as `owner/name:branch`, in order.
fn heads(repo: &Repo) -> Vec<String> {
    pr_local(repo.path(), None)
        .expect("pr_local")
        .heads
        .iter()
        .map(|h| format!("{}/{}:{}", h.repo.owner(), h.repo.name(), h.name))
        .collect()
}

#[test]
fn a_gh_checkout_of_a_fork_pr_publishes_on_the_contributors_fork() {
    // What `gh pr checkout` writes when the maintainer can push to the fork: URL values.
    let repo = worktree();
    let fork = "https://github.com/contributor/repo-fork.git";
    repo.git(&["config", "branch.work.remote", fork]);
    repo.git(&["config", "branch.work.pushremote", fork]);
    repo.git(&["config", "branch.work.merge", "refs/heads/fix-typo"]);
    assert_eq!(heads(&repo), ["contributor/repo-fork:work", "contributor/repo-fork:fix-typo"]);
}

#[test]
fn a_gh_checkout_without_push_access_pins_the_pull_request() {
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/pull/108/head"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    let pin = local.pin.expect("the recorded pull request pins");
    assert_eq!((pin.repo.owner(), pin.repo.name(), pin.number), ("owner", "repo", 108));
    // The pull ref is no branch: only the branch's own name joins, on origin.
    assert_eq!(heads(&repo), ["owner/repo:work"]);
}

#[test]
fn tracking_an_upstream_base_publishes_nothing_there() {
    // A fork clone: `git switch -c fix upstream/main` records tracking, not publication.
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "https://github.com/contributor/repo.git"]);
    repo.git(&["remote", "add", "upstream", "https://github.com/acme/repo.git"]);
    let main = repo.git(&["rev-parse", "main"]).trim().to_string();
    repo.git(&["update-ref", "refs/remotes/upstream/main", &main]);
    repo.git(&["switch", "-qc", "fix", "--track", "upstream/main"]);
    assert_eq!(repo.git(&["config", "branch.fix.remote"]).trim(), "upstream");
    assert_eq!(heads(&repo), ["contributor/repo:fix"], "no acme head: tracking is not publication");
}

#[test]
fn the_push_remote_wins_and_its_pushurl_is_honored() {
    let repo = worktree();
    repo.git(&["remote", "add", "mine", "https://github.com/me/repo.git"]);
    repo.git(&["remote", "set-url", "--push", "mine", "https://github.com/me/repo-push.git"]);
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/work-up"]);
    repo.git(&["config", "remote.pushDefault", "mine"]);
    assert_eq!(heads(&repo), ["me/repo-push:work", "owner/repo:work-up"]);
    repo.git(&["config", "branch.work.pushRemote", "origin"]);
    assert_eq!(
        heads(&repo),
        ["owner/repo:work", "owner/repo:work-up"],
        "pushRemote outranks pushDefault"
    );
}

#[test]
fn a_url_remote_follows_instead_of_rewrites() {
    let repo = worktree();
    repo.git(&["config", "url.https://github.com/contributor/.insteadOf", "fork:"]);
    repo.git(&["config", "branch.work.remote", "fork:repo-fork.git"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/fix"]);
    assert_eq!(heads(&repo), ["contributor/repo-fork:work", "contributor/repo-fork:fix"]);
}

#[test]
fn a_deleted_remote_names_no_head_never_origin() {
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "gone"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/fix"]);
    assert!(heads(&repo).is_empty(), "a leftover record of a removed remote proves nothing");
}

#[test]
fn a_fork_prs_base_named_head_is_the_forks_not_tracking() {
    // `gh pr checkout` of a PR from `contributor:main`, with push access.
    let repo = worktree();
    repo.git(&["switch", "-qc", "contributor-main"]);
    let fork = "https://github.com/contributor/repo.git";
    repo.git(&["config", "branch.contributor-main.remote", fork]);
    repo.git(&["config", "branch.contributor-main.pushremote", fork]);
    repo.git(&["config", "branch.contributor-main.merge", "refs/heads/main"]);
    assert_eq!(heads(&repo), ["contributor/repo:contributor-main", "contributor/repo:main"]);
}

#[test]
fn tracking_a_base_by_tip_on_a_named_remote_publishes_nothing_there() {
    let repo = worktree();
    repo.git(&["remote", "add", "upstream", "https://github.com/acme/repo.git"]);
    let main = repo.git(&["rev-parse", "main"]).trim().to_string();
    repo.git(&["update-ref", "refs/remotes/upstream/trunk", &main]);
    repo.git(&["switch", "-qc", "fix", "--track", "upstream/trunk"]);
    assert_eq!(heads(&repo), ["owner/repo:fix"], "trunk sits on the base: tracking");
}

#[test]
fn a_recorded_remote_without_a_merge_still_takes_the_push() {
    let repo = worktree();
    repo.git(&["remote", "add", "mine", "https://github.com/me/repo.git"]);
    repo.git(&["config", "branch.work.remote", "mine"]);
    assert_eq!(heads(&repo), ["me/repo:work"]);
}

#[test]
fn a_named_remote_follows_instead_of_rewrites() {
    let repo = worktree();
    repo.git(&["config", "url.https://github.com/contributor/.insteadOf", "fork:"]);
    repo.git(&["remote", "add", "contrib", "fork:repo-fork.git"]);
    repo.git(&["config", "branch.work.remote", "contrib"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/fix"]);
    assert_eq!(heads(&repo), ["contributor/repo-fork:work", "contributor/repo-fork:fix"]);
}

#[test]
fn an_unusable_origin_publishes_nowhere_never_on_the_target() {
    // A fork clone whose origin sits behind an ssh Host alias: no forge repository. Pairing
    // the branch with upstream would admit upstream's own same-named PR, a stranger's.
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "git@github-work:me/repo.git"]);
    repo.git(&["remote", "add", "upstream", "https://github.com/acme/repo.git"]);
    assert!(heads(&repo).is_empty());
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/work"]);
    assert!(heads(&repo).is_empty(), "a recorded unusable origin names nothing either");
}

#[test]
fn the_head_cap_keeps_the_own_name_and_the_record_first() {
    let repo = worktree();
    let tip = head(&repo);
    for i in 0..10 {
        repo.git(&["update-ref", &format!("refs/remotes/origin/copy{i}"), &tip]);
    }
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/work-up"]);
    let got = heads(&repo);
    assert_eq!(got.len(), 8);
    assert_eq!(got[..2], ["owner/repo:work", "owner/repo:work-up"]);
}

#[test]
fn a_bare_push_to_the_only_remote_is_found_at_the_frontier() {
    // An upstream-only clone; the agent ran `git push upstream HEAD` with no upstream record.
    let repo = worktree();
    repo.git(&["remote", "rename", "origin", "upstream"]);
    let tip = head(&repo);
    repo.git(&["update-ref", "refs/remotes/upstream/work", &tip]);
    assert_eq!(heads(&repo), ["owner/repo:work"]);
}

#[test]
fn a_push_url_behind_an_ssh_alias_falls_back_to_the_fetch_url() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "--push", "origin", "git@github-work:owner/repo.git"]);
    assert_eq!(heads(&repo), ["owner/repo:work"]);
}

#[test]
fn tracking_a_third_remotes_base_is_still_tracking() {
    let repo = worktree();
    repo.git(&["remote", "add", "colleague", "https://github.com/colleague/repo.git"]);
    let main = repo.git(&["rev-parse", "main"]).trim().to_string();
    repo.git(&["update-ref", "refs/remotes/colleague/main", &main]);
    repo.git(&["switch", "-qc", "fix", "--track", "colleague/main"]);
    assert_eq!(heads(&repo), ["owner/repo:fix"], "colleague's main is a base, not a head");
}

#[test]
fn a_bare_merge_name_reads_as_a_branch_and_the_first_merge_wins() {
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "main"]);
    assert_eq!(heads(&repo), ["owner/repo:work"], "bare `main` is the base: tracking");
    repo.git(&["config", "--replace-all", "branch.work.merge", "refs/heads/pub"]);
    repo.git(&["config", "--add", "branch.work.merge", "refs/heads/second"]);
    assert_eq!(heads(&repo), ["owner/repo:work", "owner/repo:pub"]);
}

#[test]
fn a_glab_checkout_without_push_access_pins_the_merge_request() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "origin", "https://gitlab.com/owner/repo.git"]);
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/merge-requests/45/head"]);
    let pin = pr_local(repo.path(), None).unwrap().pin.expect("the merge request pins");
    assert_eq!((pin.repo.forge(), pin.number), (herdr_reviewr::git::Forge::GitLab, 45));
    // A GitHub-shaped pull ref on a GitLab remote pins nothing.
    repo.git(&["config", "branch.work.merge", "refs/pull/45/head"]);
    assert!(pr_local(repo.path(), None).unwrap().pin.is_none());
}

#[test]
fn a_ref_left_by_a_removed_remote_neither_publishes_nor_hides_the_frontier() {
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/pushed", &head(&repo)]);
    repo.write("c.txt", "three\n");
    repo.commit_all("unpushed");
    // `old` was removed from config, but its tracking ref survived on the unpushed commit.
    repo.git(&["update-ref", "refs/remotes/old/stale", &head(&repo)]);
    assert_eq!(heads(&repo), ["owner/repo:work", "owner/repo:pushed"]);
}

#[test]
fn an_empty_merge_branch_is_no_head() {
    let repo = worktree();
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/"]);
    assert_eq!(heads(&repo), ["owner/repo:work"]);
}

#[test]
fn a_push_url_on_an_unsupported_host_falls_back_to_the_fetch_url() {
    let repo = worktree();
    repo.git(&["remote", "set-url", "--push", "origin", "https://bitbucket.org/owner/mirror.git"]);
    assert_eq!(heads(&repo), ["owner/repo:work"]);
}

#[test]
fn a_branch_with_no_record_publishes_on_origin() {
    let repo = worktree();
    assert_eq!(heads(&repo), ["owner/repo:work"]);
}

#[test]
fn detached_head_and_unborn_branch_are_clean_absences() {
    let repo = worktree();
    repo.git(&["switch", "-q", "--detach", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.branch, None, "detached HEAD is its own state");
    assert!(local.heads.is_empty(), "no branch, no heads");

    // A fresh clone setup before the first commit: an unborn branch still has its name.
    let fresh = Repo::init();
    fresh.git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    let local = pr_local(fresh.path(), None).expect("pr_local");
    assert_eq!(local.head_oid, None);
    assert_eq!(local.branch.as_deref(), Some("main"), "an unborn branch still has its name");
}

#[test]
fn a_missing_origin_is_absence_but_a_non_repo_is_failure() {
    let repo = Repo::init();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let input = fetch_input(repo.path(), None, &defaults()).expect("fetch input");
    assert_eq!(input.repository, RepositoryIdentity::Missing, "no origin is a clean absence");
    assert_eq!(input.origin_repository, None);
    assert_eq!(input.local, PrLocalState::default(), "no target, no branch story");

    let dir = tempfile::tempdir().unwrap();
    assert!(
        fetch_input(dir.path(), None, &defaults()).is_err(),
        "a non-repo directory is a failure"
    );
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

    // A base pick on this worktree changes the input.
    herdr_reviewr::git::write_base_pick(repo.path(), "work").unwrap();
    let base_changed = fetch_input(repo.path(), None, &defaults()).unwrap();
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
