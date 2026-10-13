//! Image behavior at real Git/App/worker/render boundaries.

use super::*;
use crate::{image_preview::Side as ImageSide, image_worker::Worker, ui, wake};
use ratatui::{Terminal, backend::TestBackend};
use std::{io::Cursor, process::Command};

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let result = Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    };
    run(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.png"), png(8, 4)).unwrap();
    std::fs::write(dir.path().join("b.rs"), "old\n").unwrap();
    std::fs::write(dir.path().join("text.png"), "old text\n").unwrap();
    run(&["add", "."]);
    run(&["-c", "user.name=Test", "-c", "user.email=test@invalid", "commit", "-qm", "before"]);
    std::fs::write(dir.path().join("a.png"), png(16, 8)).unwrap();
    std::fs::write(dir.path().join("b.rs"), "new text\n").unwrap();
    std::fs::write(dir.path().join("text.png"), "new pointer text\n").unwrap();
    dir
}

fn open(app: &mut App, path: &str) {
    let row = app.file_row_of_path(path).unwrap();
    app.select_file(row).unwrap();
}

fn complete(app: &mut App) {
    let request = app.image_demand((40, 12), 0, (10, 20), true).unwrap();
    app.request_image(&request);
    let (tx, rx) = wake::channel(&wake::Waker::detached());
    let worker = Worker::new(tx).unwrap();
    worker.request(request);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(app.land_image(worker.take_result().unwrap()));
}

fn painted(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|frame| ui::render(frame, app)).unwrap();
    terminal.backend().buffer().content.iter().map(ratatui::buffer::Cell::symbol).collect()
}

#[test]
fn metadata_startup_is_readable_and_other_files_tabs_never_inherit_the_pair() {
    let dir = fixture();
    let mut app = App::new(dir.path().into(), Scope::Uncommitted, None);
    app.image_metadata_only = true;
    app.reload().unwrap();
    open(&mut app, "a.png");
    complete(&mut app);
    assert!(painted(&app).contains(if cfg!(unix) { "PNG · 16×8" } else { "PNG · 8×4" }));
    let place = (app.file_cursor, app.diff_cursor, app.diff_scroll);
    app.toggle_image_view();
    assert_eq!((app.file_cursor, app.diff_cursor, app.diff_scroll), place);
    open(&mut app, "b.rs");
    assert!(!app.image_is_current_view());
    assert!(painted(&app).contains("new text"));
    assert!(!painted(&app).contains("PNG ·"));
    assert!(app.image_demand((40, 12), 0, (10, 20), true).is_none());
    open(&mut app, "text.png");
    assert!(app.image_demand((40, 12), 0, (10, 20), true).is_none());
    assert!(painted(&app).contains("new pointer text"));
    open(&mut app, "a.png");
    complete(&mut app);
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(!app.image_is_current_view());
    assert!(!painted(&app).contains("Before"));
}

#[test]
fn same_file_refresh_preserves_its_own_metadata_until_atomic_replacement() {
    let dir = fixture();
    let mut app = App::new(dir.path().into(), Scope::Uncommitted, None);
    app.reload().unwrap();
    open(&mut app, "a.png");
    complete(&mut app);
    let old = app.image_pair().unwrap().request.clone();
    std::fs::write(dir.path().join("a.png"), png(32, 16)).unwrap();
    app.reload().unwrap();
    assert!(app.image_is_current_view());
    assert!(painted(&app).contains(if cfg!(unix) { "PNG · 16×8" } else { "PNG · 8×4" }));
    if cfg!(unix) {
        assert!(!painted(&app).contains("PNG · 32×16"));
    }
    complete(&mut app);
    assert!(painted(&app).contains(if cfg!(unix) { "PNG · 32×16" } else { "PNG · 8×4" }));
    assert!(matches!(app.image_pair().unwrap().before, ImageSide::Metadata { .. }));
    let stale = ImageResultPair {
        request: old,
        before: ImageSide::Missing,
        after: ImageSide::Missing,
        prepared: (None, None),
    };
    assert!(!app.land_image(stale));
    assert!(painted(&app).contains(if cfg!(unix) { "PNG · 32×16" } else { "PNG · 8×4" }));
}

#[test]
fn shared_panel_geometry_matches_narrow_and_wide_live_layout_without_zero_area_uploads() {
    let dir = fixture();
    let mut app = App::new(dir.path().into(), Scope::Uncommitted, None);
    app.reload().unwrap();
    open(&mut app, "a.png");
    for width in [0, 1, 10, 65, 100, 140] {
        let area = ratatui::layout::Rect::new(0, 0, width, 30);
        let [left, right] = ui::image_regions(area, &app);
        let cells = ui::image_preview_cells(area, &app);
        assert_eq!(cells, (left.width.min(right.width), left.height.min(right.height)));
        assert!(area.intersection(left) == left && area.intersection(right) == right);
    }
}

#[test]
fn accepted_hop_still_prepares_headers_first_and_generation_churn_finishes_cache_work() {
    let dir = fixture();
    let mut app = App::new(dir.path().into(), Scope::Uncommitted, None);
    app.reload().unwrap();
    open(&mut app, "a.png");
    app.set_image_graphics_accepted(true);
    let headers = app.image_demand((40, 12), 0, (10, 20), true).unwrap();
    assert!(!headers.graphics);
    app.request_image(&headers);
    app.reload().unwrap();
    assert!(app.image_demand((40, 12), 0, (10, 20), true).is_none());
    let (tx, rx) = wake::channel(&wake::Waker::detached());
    let worker = Worker::new(tx).unwrap();
    worker.request(headers);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!app.land_image(worker.take_result().unwrap()));
    assert!(app.image_pair().is_none());
    complete(&mut app);
    assert!(app.image_demand((40, 12), 0, (10, 20), true).unwrap().graphics);
    open(&mut app, "b.rs");
    assert!(app.image_demand((40, 12), 0, (10, 20), true).is_none());
    open(&mut app, "a.png");
    assert!(!app.image_demand((40, 12), 0, (10, 20), true).unwrap().graphics);
}

#[test]
fn unmerged_raster_named_text_keeps_its_commentable_diff() {
    let dir = fixture();
    let run = |args: &[&str], success: bool| {
        let result = Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap();
        assert_eq!(result.status.success(), success, "{}", String::from_utf8_lossy(&result.stderr));
    };
    let commit = |message| {
        run(&["add", "."], true);
        run(
            &["-c", "user.name=Test", "-c", "user.email=test@invalid", "commit", "-qm", message],
            true,
        );
    };
    commit("after");
    run(&["checkout", "-qb", "other"], true);
    std::fs::write(dir.path().join("text.png"), "theirs\n").unwrap();
    commit("theirs");
    run(&["checkout", "-q", "main"], true);
    std::fs::write(dir.path().join("text.png"), "ours\n").unwrap();
    commit("ours");
    run(&["-c", "user.name=Test", "-c", "user.email=test@invalid", "merge", "other"], false);
    let mut app = App::new(dir.path().into(), Scope::Uncommitted, None);
    app.reload().unwrap();
    open(&mut app, "text.png");
    assert!(app.changeset.files["text.png"].conflicted);
    assert!(!app.changeset.files["text.png"].binary);
    assert!(app.image_demand((40, 12), 0, (10, 20), true).is_none());
    assert!(painted(&app).contains("ours"));
    assert!(!painted(&app).contains("Before"));
}
