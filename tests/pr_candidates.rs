//! Integration tests for the PR fetch's local reads (`git::pr_local`,
//! `git::ahead_behind_oids`) against real temp repos. Remote-tracking branches are faked
//! with `git update-ref refs/remotes/origin/<name> <sha>` — no network, no `gh`.
//! See `specs/forge-host.md` "Candidate branches".

mod common;

use common::Repo;
use herdr_reviewr::git::{ahead_behind_oids, pr_local};

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

#[test]
fn push_head_other_name_yields_the_remote_branch_before_the_local_name() {
    // The headline workflow: `git push origin HEAD:other` with no `-u` updates the
    // remote-tracking ref; the pushed name must outrank the (PR-less) local name.
    let repo = worktree();
    repo.git(&["update-ref", "refs/remotes/origin/other", "HEAD"]);
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.slug, Some(("owner".to_string(), "repo".to_string())));
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
    let local = pr_local(repo.path(), None).expect("pr_local");
    assert_eq!(local.slug, None, "no origin remote is a clean absence");
    assert_eq!(local.candidates, ["main"]);

    let dir = tempfile::tempdir().unwrap();
    assert!(pr_local(dir.path(), None).is_err(), "a non-repo directory is a failure");
}

#[test]
fn ahead_behind_oids_counts_between_pins_and_tolerates_a_missing_head() {
    let repo = worktree();
    let main = repo.git(&["rev-parse", "main"]).trim().to_string();
    let work = head(&repo);
    assert_eq!(ahead_behind_oids(repo.path(), &work, &main).unwrap(), Some((1, 0)));
    assert_eq!(ahead_behind_oids(repo.path(), &main, &work).unwrap(), Some((0, 1)));
    assert_eq!(ahead_behind_oids(repo.path(), &work, &work).unwrap(), Some((0, 0)));
    // A PR head OID never fetched locally reads as in-sync-unknown, not an error.
    let missing = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(ahead_behind_oids(repo.path(), &work, missing).unwrap(), None);
}
