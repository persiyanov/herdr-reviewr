mod common;

use common::{Repo, app_on};
use herdr_reviewr::app::{Band, FooterAction, Mode, Tab};
use herdr_reviewr::keymap::Keymap;
use herdr_reviewr::model::Scope;
use herdr_reviewr::{handle_key, repo_actions};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

const AREA: Rect = Rect::new(0, 0, 100, 30);

fn press(app: &mut herdr_reviewr::app::App, code: KeyCode) {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), AREA, &Keymap::default()).unwrap();
}

fn dirty_repo() -> (Repo, herdr_reviewr::app::App) {
    let repo = Repo::init();
    repo.write("a.rs", "old\n");
    repo.write("b.rs", "old\n");
    repo.commit_all("init");
    repo.write("a.rs", "new\n");
    repo.write("b.rs", "new\n");
    let app = app_on(&repo);
    (repo, app)
}

fn finish_prepare(repo: &Repo, app: &mut herdr_reviewr::app::App) {
    let request = app.repo_action_request.take().unwrap();
    let (kind, success) = match request {
        repo_actions::Request::OpenCommit { files } => (
            repo_actions::Kind::PrepareCommit,
            repo_actions::Success::CommitDialog(repo_actions::guard(repo.path(), &files).unwrap()),
        ),
        repo_actions::Request::OpenDiscard { file } => (
            repo_actions::Kind::PrepareDiscard,
            repo_actions::Success::DiscardDialog(
                repo_actions::guard(repo.path(), &[file]).unwrap(),
            ),
        ),
        _ => panic!("expected dialog preparation"),
    };
    app.land_repo_action(repo_actions::Completion { kind, result: Ok(success) });
}

#[test]
fn commit_picker_starts_all_checked_and_advances_to_message() {
    let (repo, mut app) = dirty_repo();
    press(&mut app, KeyCode::Char('C'));
    assert_eq!(app.mode, Mode::RepoBusy);
    finish_prepare(&repo, &mut app);
    assert_eq!(app.mode, Mode::CommitFiles);
    let dialog = app.commit_dialog.as_ref().unwrap();
    assert_eq!(dialog.checked, vec![true, true]);

    press(&mut app, KeyCode::Char(' '));
    assert_eq!(app.commit_dialog.as_ref().unwrap().checked, vec![false, true]);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::CommitMessage);
    app.input_paste("subject\n\nbody");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::RepoBusy);
    assert!(matches!(app.repo_action_request, Some(repo_actions::Request::Commit { .. })));
}

#[test]
fn escape_from_message_goes_back_and_escape_from_files_cancels() {
    let (repo, mut app) = dirty_repo();
    press(&mut app, KeyCode::Char('C'));
    finish_prepare(&repo, &mut app);
    press(&mut app, KeyCode::Enter);
    app.input_paste("kept draft");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::CommitFiles);
    assert_eq!(app.input, "kept draft");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.commit_dialog.is_none());
}

#[test]
fn discard_requires_the_confirmation_enter() {
    let (repo, mut app) = dirty_repo();
    press(&mut app, KeyCode::Char('D'));
    assert_eq!(app.mode, Mode::RepoBusy);
    finish_prepare(&repo, &mut app);
    assert_eq!(app.mode, Mode::ConfirmDiscard);
    assert!(app.repo_action_request.is_none());
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::RepoBusy);
    assert!(matches!(app.repo_action_request, Some(repo_actions::Request::Discard { .. })));
}

#[test]
fn actions_and_footer_are_confined_to_uncommitted_changes() {
    let (_repo, mut app) = dirty_repo();
    let bands = app.footer_bands();
    let actions = bands.iter().map(|&(action, _)| action).collect::<Vec<_>>();
    assert!(actions.contains(&FooterAction::Commit));
    assert!(actions.contains(&FooterAction::Discard));
    assert!(actions.contains(&FooterAction::Push));
    assert!(
        bands
            .iter()
            .filter(|(action, _)| matches!(
                action,
                FooterAction::Commit | FooterAction::Discard | FooterAction::Push
            ))
            .all(|(_, band)| *band == Band::Do),
        "Git actions belong to the collapsed row"
    );

    app.set_scope(Scope::Branch).unwrap();
    press(&mut app, KeyCode::Char('C'));
    assert_eq!(app.mode, Mode::Normal);
    let actions = app.footer_bands().into_iter().map(|(action, _)| action).collect::<Vec<_>>();
    assert!(!actions.contains(&FooterAction::Commit));
    assert!(!actions.contains(&FooterAction::Discard));
    assert!(!actions.contains(&FooterAction::Push));

    app.set_tab(Tab::Pr).unwrap();
    press(&mut app, KeyCode::Char('P'));
    assert!(app.repo_action_request.is_none());
}

#[test]
fn push_queues_without_needing_uncommitted_files() {
    let repo = Repo::init();
    repo.write("a", "a\n");
    repo.commit_all("init");
    let mut app = app_on(&repo);
    press(&mut app, KeyCode::Char('P'));
    assert_eq!(app.repo_action_busy, Some(repo_actions::Kind::Push));
    assert!(matches!(app.repo_action_request, Some(repo_actions::Request::Push)));
    assert_eq!(app.mode, Mode::Normal, "push leaves review navigation available");
}
