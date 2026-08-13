//! End-to-end tests of CLI navigation (specs/nav.md): a command file written the way
//! `herdr-reviewr nav` writes one, taken and applied the way the event loop does.

mod common;

use common::{Repo, app_on};
use herdr_reviewr::app::Mode;
use herdr_reviewr::apply_nav;
use herdr_reviewr::nav::{self, NavCommand};

/// A repo with edits in two nested files, so navigation has somewhere to go.
fn nested_repo() -> Repo {
    let repo = Repo::init();
    repo.write("src/deep/inner.rs", "fn inner() {}\n");
    repo.write("src/outer.rs", "fn outer() {}\n");
    repo.commit_all("base");
    repo.write("src/deep/inner.rs", "fn inner() { changed(); }\n");
    repo.write("src/outer.rs", "fn outer() { changed(); }\n");
    repo
}

#[test]
fn a_command_opens_the_named_file_and_reports_it() {
    let repo = nested_repo();
    let mut app = app_on(&repo);
    assert_ne!(app.diff_path.as_deref(), Some("src/outer.rs"));

    nav::write(
        repo.path(),
        &NavCommand { file: Some("src/outer.rs".into()), ..NavCommand::default() },
    )
    .unwrap();
    let cmd = nav::take(repo.path()).expect("the command file was just written");
    apply_nav(&mut app, &cmd);

    assert_eq!(app.diff_path.as_deref(), Some("src/outer.rs"));
    assert_eq!(app.status, "nav: src/outer.rs");
    assert!(nav::take(repo.path()).is_none(), "a command applies exactly once");
}

#[test]
fn a_file_outside_the_scope_lands_in_the_status_line_and_moves_nothing() {
    let repo = nested_repo();
    let mut app = app_on(&repo);
    let open_before = app.diff_path.clone();

    apply_nav(
        &mut app,
        &NavCommand { file: Some("src/untouched.rs".into()), ..NavCommand::default() },
    );

    assert_eq!(app.diff_path, open_before);
    assert!(app.status.contains("is not in the uncommitted scope"), "status: {}", app.status);
}

#[test]
fn a_command_is_dropped_while_a_comment_is_being_composed() {
    let repo = nested_repo();
    let mut app = app_on(&repo);
    app.mode = Mode::Composing { editing: None };

    apply_nav(&mut app, &NavCommand { file: Some("src/outer.rs".into()), ..NavCommand::default() });

    assert_ne!(app.diff_path.as_deref(), Some("src/outer.rs"));
    assert!(app.status.starts_with("nav: ignored"), "status: {}", app.status);
}
