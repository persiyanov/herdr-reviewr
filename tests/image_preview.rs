//! herpi fork: an image file previews as its sides where the diff would be.

mod common;

use common::{Repo, app_on, enter_tab};
use herdr_reviewr::app::{App, Tab};
use herdr_reviewr::ui;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui_image::picker::Picker;

fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let image =
        image::RgbaImage::from_pixel(width, height, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn put(repo: &Repo, rel: &str, bytes: &[u8]) {
    std::fs::write(repo.path().join(rel), bytes).unwrap();
}

/// Open `name` from the file list (half-block images, so no terminal is asked).
fn open(app: &mut App, name: &str) {
    app.picker = Some(Picker::halfblocks());
    let index = app
        .file_rows
        .iter()
        .position(|row| row.name == name)
        .unwrap_or_else(|| panic!("{name} is not listed"));
    app.select_file(index).unwrap();
}

fn labels(app: &App) -> Vec<&'static str> {
    app.image_view
        .as_ref()
        .map(|view| view.sides.iter().map(|side| side.label).collect())
        .unwrap_or_default()
}

fn screen(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    buffer.content().iter().map(ratatui::buffer::Cell::symbol).collect()
}

/// Whether any cell is painted in `rgb` (as half-block foreground or background).
fn drawn_in(app: &App, rgb: [u8; 3]) -> bool {
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    let color = ratatui::style::Color::Rgb(rgb[0], rgb[1], rgb[2]);
    terminal.backend().buffer().content().iter().any(|cell| cell.fg == color || cell.bg == color)
}

fn repo_with_image_changes() -> Repo {
    let r = Repo::init();
    put(&r, "logo.png", &png(40, 20, [30, 30, 200]));
    put(&r, "gone.png", &png(10, 10, [0, 0, 0]));
    r.write("notes.txt", "hello\n");
    r.commit_all("init");
    put(&r, "logo.png", &png(40, 20, [200, 30, 30]));
    put(&r, "shot.png", &png(16, 16, [30, 200, 30]));
    r.remove("gone.png");
    r.write("notes.txt", "hello\nworld\n");
    r
}

#[test]
fn a_modified_image_shows_before_and_after() {
    let r = repo_with_image_changes();
    let mut app = app_on(&r);
    open(&mut app, "logo.png");
    assert_eq!(labels(&app), vec!["before", "after"]);
    let painted = screen(&app);
    assert!(painted.contains("before") && painted.contains("after"), "the sides are labelled");
    assert!(painted.contains("40×20"), "each side says its size");
    assert!(drawn_in(&app, [30, 30, 200]), "the before image is drawn (half-blocks)");
    assert!(drawn_in(&app, [200, 30, 30]), "the after image is drawn (half-blocks)");
    assert!(!painted.contains("binary file"), "no binary notice for an image");
}

#[test]
fn an_added_image_shows_after_and_a_deleted_one_before() {
    let r = repo_with_image_changes();
    let mut app = app_on(&r);
    open(&mut app, "shot.png");
    assert_eq!(labels(&app), vec!["after"]);
    open(&mut app, "gone.png");
    assert_eq!(labels(&app), vec!["before"]);
}

#[test]
fn a_text_file_keeps_its_diff() {
    let r = repo_with_image_changes();
    let mut app = app_on(&r);
    open(&mut app, "notes.txt");
    assert!(app.image_view.is_none(), "text files never take the image path");
    assert!(screen(&app).contains("world"), "the diff is drawn as before");
}

#[test]
fn all_files_shows_the_image_itself() {
    let r = repo_with_image_changes();
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    open(&mut app, "logo.png");
    assert_eq!(labels(&app), vec!["file"]);
    assert!(screen(&app).contains("40×20"));
}

#[test]
fn without_a_picker_an_image_keeps_the_binary_notice() {
    let r = repo_with_image_changes();
    let mut app = app_on(&r);
    let index = app.file_rows.iter().position(|row| row.name == "logo.png").unwrap();
    app.select_file(index).unwrap();
    assert!(app.image_view.is_none());
    assert!(screen(&app).contains("binary file"));
}
