//! Switching the reviewed worktree: the pane restarts on the pick as a pane opened there would.

mod common;

use common::{FakeTarget, Repo, app_on};
use herdr_reviewr::app::{App, FooterAction, Mode, Tab};
use herdr_reviewr::config::Config;
use herdr_reviewr::git::canonical;
use herdr_reviewr::keymap::default_keymap;
use herdr_reviewr::model::{Comment, Rev, Scope, Side};
use herdr_reviewr::{handle_key, handle_mouse, switched_app};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

const AREA: Rect = Rect::new(0, 0, 120, 40);

fn paths(app: &App) -> Vec<String> {
    app.entries.iter().map(|e| e.path.clone()).collect()
}

fn press(app: &mut App, code: KeyCode) {
    handle_key(app, KeyEvent::from(code), AREA, default_keymap()).unwrap();
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn comment() -> Comment {
    Comment {
        file: "main.txt".into(),
        side: Side::New,
        start: 1,
        end: 1,
        lines: "+edited".into(),
        text: "unsent".into(),
        diff_anchored: true,
        rev: Rev::Worktree,
    }
}

/// A repo on `main` with two linked worktrees, `zeta` and `company-pages`.
fn clone_with_worktrees() -> (Repo, common::LinkedWorktree, common::LinkedWorktree) {
    let repo = Repo::init();
    repo.write("main.txt", "base\n");
    repo.commit_all("base");
    let zeta = repo.add_worktree("zeta");
    let pages = repo.add_worktree("company-pages");
    (repo, zeta, pages)
}

/// Where the pane will restart, as the OS spells it (macOS temp dirs sit under `/private`).
fn switch_target(app: &App) -> Option<std::path::PathBuf> {
    app.switch_to.as_deref().map(canonical)
}

/// The pane restarted on `path`, loaded as the run loop loads it.
fn switch_to(old: &mut App, path: &std::path::Path) -> App {
    let mut cfg = Config::parse([old.repo.to_string_lossy().into_owned()]);
    old.switch_to = Some(path.to_path_buf());
    let mut switched = switched_app(&mut cfg, old).expect("a switch was asked");
    assert_eq!(cfg.repo, path, "the config follows the pick, so a recovery rebuilds there");
    switched.reload().unwrap();
    switched
}

fn row_names(app: &App) -> Vec<String> {
    let wp = app.worktree_picker.as_ref().expect("the picker is open");
    wp.visible().iter().map(|r| r.name.clone()).collect()
}

#[test]
fn the_picker_lists_the_current_then_home_then_by_branch_and_filters() {
    let (repo, _zeta, pages) = clone_with_worktrees();
    // A branch that sorts before home, so only the home rule puts home second.
    let _alpha = repo.add_worktree("alpha");
    let mut app = app_on(&repo);
    press(&mut app, KeyCode::Char('W'));
    assert_eq!(app.mode, Mode::WorktreePick);
    let current = herdr_reviewr::git::checked_out_branch(repo.path()).unwrap().unwrap();
    assert_eq!(row_names(&app), [current.as_str(), "alpha", "company-pages", "zeta"]);
    let wp = app.worktree_picker.as_ref().unwrap();
    assert!(wp.rows[0].current && wp.cursor == 0, "the cursor starts on the current worktree");

    // Enter on the current worktree changes nothing.
    press(&mut app, KeyCode::Enter);
    assert_eq!((app.mode.clone(), app.switch_to.clone()), (Mode::Normal, None));

    press(&mut app, KeyCode::Char('W'));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    type_text(&mut app, "comp");
    assert_eq!(row_names(&app), ["company-pages"]);
    let wp = app.worktree_picker.as_ref().unwrap();
    assert_eq!(wp.cursor, 0, "a filter edit puts the highlight on the best match");
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        switch_target(&app),
        Some(canonical(pages.path())),
        "no comments: the pick switches"
    );
    assert_eq!(app.mode, Mode::Normal);

    // Switched, the pick leads and home, where the pane opened, follows it.
    let mut switched = switch_to(&mut app, pages.path());
    press(&mut switched, KeyCode::Char('W'));
    assert_eq!(row_names(&switched), ["company-pages", current.as_str(), "alpha", "zeta"]);
    let wp = switched.worktree_picker.as_ref().unwrap();
    assert!(wp.rows[1].home && !wp.rows[1].current, "home is marked, under the current");
    assert_eq!(wp.cursor, 0, "the cursor still starts on the current worktree");
}

#[test]
fn unsent_comments_get_the_switch_question() {
    let (repo, _zeta, pages) = clone_with_worktrees();
    let mut app = app_on(&repo);
    app.store.add(comment());
    press(&mut app, KeyCode::Char('W'));
    type_text(&mut app, "comp");
    press(&mut app, KeyCode::Enter);
    assert!(app.switch_question().is_some(), "enter asks first");
    assert_eq!(app.switch_to, None);
    let bands: Vec<FooterAction> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert_eq!(
        bands,
        [FooterAction::SwitchDiscard, FooterAction::Cancel, FooterAction::Send, FooterAction::Copy]
    );

    // Any other key returns to the picker, the filter kept.
    press(&mut app, KeyCode::Char('x'));
    assert!(app.switch_question().is_none());
    assert_eq!(app.mode, Mode::WorktreePick);
    assert_eq!(row_names(&app), ["company-pages"], "the stray key never reached the filter");

    // Quit's discard key is quit's alone: here it answers like any other key.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('Q'));
    assert!(!app.should_quit && app.switch_question().is_none());
    assert_eq!(app.mode, Mode::WorktreePick);

    // A config error blocks the pane: it closes the picker and its question with it.
    let mut blocked = app_on(&repo);
    blocked.store.add(comment());
    press(&mut blocked, KeyCode::Char('W'));
    type_text(&mut blocked, "comp");
    press(&mut blocked, KeyCode::Enter);
    blocked.set_config_error("invalid config".into());
    assert!(blocked.switch_question().is_none() && blocked.worktree_picker.is_none());

    // Esc, a paste, or a click answers it the same way.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    assert!(app.switch_question().is_none());
    assert_eq!(app.mode, Mode::WorktreePick, "esc leaves the question, not the picker");
    press(&mut app, KeyCode::Enter);
    app.input_paste("zeta");
    assert!(app.switch_question().is_none());
    assert_eq!(row_names(&app), ["company-pages"], "the paste never reached the filter");
    press(&mut app, KeyCode::Enter);
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    handle_mouse(&mut app, click, AREA, &[], default_keymap(), &FakeTarget::ok()).unwrap();
    assert!(app.switch_question().is_none());
    assert_eq!((app.mode.clone(), app.switch_to.clone()), (Mode::WorktreePick, None));

    // The bound switch key answers by switching and dropping the comments.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('W'));
    assert_eq!(switch_target(&app), Some(canonical(pages.path())));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn send_from_the_switch_question_stays() {
    let (repo, _zeta, _pages) = clone_with_worktrees();
    let mut app = app_on(&repo);
    app.store.add(comment());
    press(&mut app, KeyCode::Char('W'));
    type_text(&mut app, "comp");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('s'));
    assert_eq!(app.switch_to, None, "a send never switches");
    assert!(app.worktree_picker.is_none());
    assert_eq!(app.store.len(), 1, "no agent took them: the comments stay");
}

#[test]
fn a_worktree_removed_after_listing_is_gone_not_switched_to() {
    let (repo, _zeta, pages) = clone_with_worktrees();
    let mut app = app_on(&repo);
    press(&mut app, KeyCode::Char('W'));
    type_text(&mut app, "comp");
    std::fs::remove_dir_all(pages.path()).unwrap();
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.switch_to, None);
    assert!(app.status.starts_with("worktree gone"), "{}", app.status);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn w_opens_on_a_lone_worktree_and_is_silent_outside_a_repo() {
    let repo = Repo::init();
    repo.write("main.txt", "base\n");
    repo.commit_all("base");
    let mut app = app_on(&repo);
    let advertised =
        |app: &App| app.footer_bands().iter().any(|(a, _)| *a == FooterAction::WorktreePick);
    assert!(advertised(&app), "like the commit picker, `W` shows on every tab");
    app.set_tab(Tab::Pr).unwrap();
    assert!(advertised(&app), "`W` works on `PR`, so its footer says so");
    app.set_tab(Tab::Changes).unwrap();
    let rows = |app: &App| -> Vec<_> {
        let wp = app.worktree_picker.as_ref().expect("the picker is open");
        wp.rows.iter().map(|r| (canonical(&r.path), r.current)).collect()
    };
    press(&mut app, KeyCode::Char('W'));
    assert_eq!(rows(&app), [(canonical(repo.path()), true)], "the one worktree, current");
    press(&mut app, KeyCode::Enter);
    assert_eq!((app.mode.clone(), app.switch_to.clone()), (Mode::Normal, None));

    // A deleted, unpruned worktree is nothing to switch to.
    let wt = repo.add_worktree("other");
    std::fs::remove_dir_all(wt.path()).unwrap();
    press(&mut app, KeyCode::Char('W'));
    assert_eq!(rows(&app), [(canonical(repo.path()), true)], "the prunable one is left out");
    press(&mut app, KeyCode::Esc);

    // Outside a repo there is nothing to list, and nothing to say about it.
    let plain = tempfile::tempdir().unwrap();
    let mut app = App::new(plain.path().to_path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    press(&mut app, KeyCode::Char('W'));
    assert_eq!((app.mode.clone(), app.status.as_str()), (Mode::Normal, ""));

    // The failed read was not kept: once the folder is a repo, `W` lists it.
    let made = std::process::Command::new("git")
        .args(["-C", plain.path().to_str().unwrap(), "init", "-q"])
        .status()
        .unwrap();
    assert!(made.success());
    press(&mut app, KeyCode::Char('W'));
    assert_eq!(rows(&app), [(canonical(plain.path()), true)]);
}

#[test]
fn a_worktree_re_added_from_another_clone_lists_that_clones_worktrees() {
    let (a, b) = (Repo::init(), Repo::init());
    for r in [&a, &b] {
        r.write("main.txt", "base\n");
        r.commit_all("base");
    }
    let wt = a.add_worktree("x");
    let at = wt.path().to_str().unwrap().to_string();
    let mut app = App::new(wt.path().to_path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    let paths = |app: &App| -> Vec<_> {
        let wp = app.worktree_picker.as_ref().expect("the picker is open");
        wp.rows.iter().map(|r| canonical(&r.path)).collect()
    };
    press(&mut app, KeyCode::Char('W'));
    assert_eq!(paths(&app), [canonical(wt.path()), canonical(a.path())]);
    press(&mut app, KeyCode::Esc);

    a.git(&["worktree", "remove", "--force", &at]);
    b.git(&["worktree", "add", "-q", "-b", "x", &at]);
    press(&mut app, KeyCode::Char('W'));
    let now = [canonical(wt.path()), canonical(b.path())];
    assert_eq!(paths(&app), now, "the clone its `.git` names now");
}

#[test]
fn a_main_worktree_git_lists_by_its_git_dir_reads_as_itself() {
    // `--separate-git-dir`, a submodule and a symlinked `.git` list their main worktree as the git dir.
    let layouts: &[bool] = if cfg!(unix) { &[false, true] } else { &[false] };
    for &symlinked in layouts {
        let repo = Repo::init();
        let (work, gd) = (repo.path().join("work"), repo.path().join("work.git"));
        let (work_s, gd_s) = (work.to_str().unwrap(), gd.to_str().unwrap());
        repo.git(&["init", "-q", "-b", "main", "--separate-git-dir", gd_s, work_s]);
        if symlinked {
            std::fs::remove_file(work.join(".git")).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&gd, work.join(".git")).unwrap();
        }
        repo.git(&["-C", work_s, "commit", "-q", "--allow-empty", "-m", "base"]);
        let mut app = App::new(work.clone(), Scope::Uncommitted, None);
        app.reload().unwrap();
        press(&mut app, KeyCode::Char('W'));
        let wp = app.worktree_picker.as_ref().expect("the picker is open");
        let rows: Vec<_> = wp.rows.iter().map(|r| (canonical(&r.path), r.current)).collect();
        assert_eq!(rows, [(canonical(&work), true)], "never the git dir, symlinked {symlinked}");
        press(&mut app, KeyCode::Esc);

        let linked = repo.path().join("linked");
        let linked_s = linked.to_str().unwrap();
        repo.git(&["-C", work_s, "worktree", "add", "-q", "-b", "linked", linked_s]);
        press(&mut app, KeyCode::Char('W'));
        let wp = app.worktree_picker.as_ref().expect("the linked worktree is listed");
        let rows: Vec<_> = wp.rows.iter().map(|r| (canonical(&r.path), r.current)).collect();
        let listed = [(canonical(&work), true), (canonical(&linked), false)];
        assert_eq!(rows, listed, "symlinked {symlinked}");

        // From the linked one, git can't place that git dir, but the pane opened there.
        press(&mut app, KeyCode::Esc);
        let mut away = switch_to(&mut app, &linked);
        press(&mut away, KeyCode::Char('W'));
        let wp = away.worktree_picker.as_ref().expect("home is listed");
        let rows: Vec<_> =
            wp.rows.iter().map(|r| (canonical(&r.path), r.current, r.home)).collect();
        let listed = [(canonical(&linked), true, false), (canonical(&work), false, true)];
        assert_eq!(rows, listed, "symlinked {symlinked}");
    }
}

#[test]
fn a_switch_reviews_the_pick_and_keeps_only_the_view() {
    let repo = Repo::init();
    repo.write("main.txt", "base\n");
    repo.commit_all("base");
    let wt = repo.add_worktree("company-pages");
    std::fs::write(wt.path().join("page.txt"), "new\n").unwrap();
    repo.write("main.txt", "edited\n");

    let mut old = app_on(&repo);
    old.wrap = false;
    old.set_scope(Scope::Branch).unwrap();
    old.store.add(Comment { text: "stays behind".into(), ..comment() });
    // Where herdr last saw this pane, which a move changes after launch.
    old.herdr.ids.pane = Some("w9:p3".into());
    old.herdr.ids.workspace = Some("w9".into());

    let switched = switch_to(&mut old, wt.path());

    let mut fresh = herdr_reviewr::app::App::new(switched.repo.clone(), Scope::Branch, None);
    fresh.reload().unwrap();
    assert_eq!(paths(&switched), paths(&fresh), "the pick, as a pane opened there sees it");
    assert_eq!(paths(&switched), ["page.txt"]);
    assert_eq!(switched.store.len(), 0, "comments stay with the worktree they were made on");
    assert_eq!(switched.scope, Scope::Branch, "the scope carries");
    assert!(!switched.wrap, "wrap carries");
    assert_eq!(
        switched.herdr.ids.pane.as_deref(),
        Some("w9:p3"),
        "the live pane id, not the env's"
    );
    assert_eq!(switched.herdr.ids.workspace.as_deref(), Some("w9"));
    assert_eq!(switched.status, "", "the footer names the pick, so the switch says nothing");
    let named = ("wt".to_string(), Some("company-pages"));
    assert_eq!(switched.worktree_name(), named, "the pick's folder and branch");
    assert_eq!(canonical(&switched.home), canonical(repo.path()), "where the pane opened");
}

#[test]
fn a_review_names_the_worktree_on_screen_before_and_after_a_switch() {
    let repo = Repo::init();
    repo.write("main.txt", "base\n");
    repo.commit_all("base");
    let wt = repo.add_worktree("company-pages");
    std::fs::write(wt.path().join("main.txt"), "edited\n").unwrap();
    repo.write("main.txt", "edited\n");
    let review =
        |at: &std::path::Path| format!("Worktree: {}\n\nmain.txt:1\n+edited\nunsent", at.display());

    let mut old = app_on(&repo);
    old.store.add(comment());
    let copied = FakeTarget::ok();
    assert!(old.export(&copied));
    assert_eq!(copied.last(), review(repo.path()));

    let mut switched = switch_to(&mut old, wt.path());
    assert_eq!(canonical(&switched.repo), canonical(wt.path()), "the pick, not where it opened");
    switched.store.add(comment());
    assert!(switched.export(&copied));
    assert_eq!(copied.last(), review(&switched.repo));
}

#[test]
fn every_view_of_a_switched_pane_matches_a_pane_opened_there() {
    let repo = Repo::init();
    repo.write("main.txt", "base\n");
    repo.commit_all("base");
    let wt = repo.add_worktree("company-pages");
    std::fs::write(wt.path().join("page.txt"), "committed\n").unwrap();
    let in_wt = wt.path().to_str().unwrap();
    repo.git(&["-C", in_wt, "add", "-A"]);
    repo.git(&["-C", in_wt, "commit", "-qm", "page"]);
    std::fs::write(wt.path().join("draft.txt"), "uncommitted\n").unwrap();

    for scope in [Scope::Uncommitted, Scope::Branch] {
        let mut old = app_on(&repo);
        old.set_scope(scope).unwrap();
        let mut switched = switch_to(&mut old, wt.path());
        let mut fresh = App::new(switched.repo.clone(), scope, None);
        fresh.reload().unwrap();
        assert_eq!(paths(&switched), paths(&fresh), "{scope:?}");
        common::enter_tab(&mut switched, Tab::AllFiles);
        common::enter_tab(&mut fresh, Tab::AllFiles);
        assert_eq!(paths(&switched), paths(&fresh), "All files from {scope:?}");
        assert!(paths(&switched).contains(&"page.txt".to_string()));
    }
}

#[test]
fn a_commit_pick_scope_stays_behind() {
    let repo = Repo::init();
    repo.write("a.txt", "a\n");
    repo.commit_all("a");
    let wt = repo.add_worktree("other");
    let mut old = app_on(&repo);
    old.scope = Scope::Commits;
    let switched = switch_to(&mut old, wt.path());
    assert_eq!(switched.scope, Scope::Uncommitted, "the default scope, not the old pick's");
}
