//! Stacked PRs: `b` again in `branch` flips the base between the default branch and the PR's base.

mod common;

use common::{Repo, pr_snapshot};
use herdr_reviewr::app::App;
use herdr_reviewr::forge::{PrSnapshot, PrView};
use herdr_reviewr::handle_key;
use herdr_reviewr::keymap::Keymap;
use herdr_reviewr::model::Scope;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

fn press(app: &mut App, code: KeyCode) {
    handle_key(app, KeyEvent::from(code), Rect::new(0, 0, 120, 40), &Keymap::default()).unwrap();
}

fn changed(app: &App) -> Vec<String> {
    app.entries.iter().map(|e| e.path.clone()).collect()
}

/// `main` ← `feature-a` ← `feature-b`, with `feature-b` in a linked worktree.
fn stack() -> (Repo, common::LinkedWorktree) {
    let r = Repo::init();
    r.write("base.rs", "base\n");
    r.commit_all("init");
    r.git(&["checkout", "-q", "-b", "feature-a"]);
    r.write("a.rs", "a\n");
    r.commit_all("a");
    let wt = r.add_worktree("feature-b");
    let at = wt.path().to_str().unwrap();
    std::fs::write(wt.path().join("b.rs"), "b\n").unwrap();
    r.git(&["-C", at, "add", "-A"]);
    r.git(&["-C", at, "commit", "-q", "-m", "b"]);
    (r, wt)
}

#[test]
fn b_again_flips_a_stacked_pr_between_its_base_and_the_default_branch() {
    let (_r, wt) = stack();
    let mut app = App::new(wt.path().to_path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.pr = PrView::Pr(Box::new(PrSnapshot { base_ref: "feature-a".into(), ..pr_snapshot() }));

    press(&mut app, KeyCode::Char('b'));
    assert_eq!(app.scope, Scope::Branch);
    assert_eq!(changed(&app), ["a.rs", "b.rs"], "the default branch first");

    press(&mut app, KeyCode::Char('b'));
    assert_eq!(app.status, "base: feature-a");
    assert_eq!(changed(&app), ["b.rs"], "only this branch's own commits");

    // The flip is the worktree's base pick, so a fresh pane opens on it.
    let mut fresh = App::new(wt.path().to_path_buf(), Scope::Branch, None);
    fresh.reload().unwrap();
    assert_eq!(changed(&fresh), ["b.rs"]);

    press(&mut app, KeyCode::Char('b'));
    assert_eq!(app.status, "base: main");
    assert_eq!(changed(&app), ["a.rs", "b.rs"]);
}

#[test]
fn b_again_without_a_stacked_pr_guesses_no_parent() {
    let (_r, wt) = stack();
    let mut app = App::new(wt.path().to_path_buf(), Scope::Branch, None);
    app.reload().unwrap();

    // `feature-a` is right there, but only a PR's base names the parent.
    press(&mut app, KeyCode::Char('b'));
    assert_eq!(app.status, "no open PR on another base: comparing against main");
    assert_eq!(changed(&app), ["a.rs", "b.rs"]);
}
