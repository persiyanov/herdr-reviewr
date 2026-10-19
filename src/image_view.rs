//! Image previews: a changed image's two sides, drawn with the terminal's graphics.
//! `png`, `jpg`, `gif`, `webp` and `bmp` show where the diff would be; see `README.md`.

use std::cell::RefCell;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui_image::StatefulImage;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::model::ChangeKind;

/// Files larger than this are named, not decoded.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Images are shrunk to at most this many pixels on their long side before drawing.
const MAX_SIDE: u32 = 2048;

/// Whether `path` names an image reviewr can preview.
pub fn is_image_path(path: &str) -> bool {
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
            .iter()
            .any(|known| e.eq_ignore_ascii_case(known))
    })
}

/// Which sides a change has: `(before, after)`. Added and untracked files are new only; a
/// deleted file is old only; everything else has both.
pub fn sides_for(kind: ChangeKind) -> (bool, bool) {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => (false, true),
        ChangeKind::Deleted => (true, false),
        ChangeKind::Modified | ChangeKind::Renamed | ChangeKind::Copied => (true, true),
    }
}

/// One side of the preview: its label, a line about the file, and the image ready to draw
/// (or why there is none).
pub struct ImageSide {
    pub label: &'static str,
    pub info: String,
    image: Option<RefCell<StatefulProtocol>>,
}

impl std::fmt::Debug for ImageSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSide")
            .field("label", &self.label)
            .field("info", &self.info)
            .field("image", &self.image.is_some())
            .finish()
    }
}

/// The preview of one image file.
#[derive(Debug)]
pub struct ImageView {
    pub path: String,
    pub sides: Vec<ImageSide>,
}

impl ImageSide {
    /// Decode `bytes` (or say why not) for `label`.
    pub fn new(label: &'static str, bytes: Result<Vec<u8>, String>, picker: &Picker) -> Self {
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(why) => return Self { label, info: why, image: None },
        };
        let size = human_size(bytes.len());
        if bytes.len() > MAX_IMAGE_BYTES {
            return Self { label, info: format!("{size} · too large to preview"), image: None };
        }
        match image::load_from_memory(&bytes) {
            Ok(decoded) => {
                let (width, height) = (decoded.width(), decoded.height());
                let shown = if width.max(height) > MAX_SIDE {
                    decoded.thumbnail(MAX_SIDE, MAX_SIDE)
                } else {
                    decoded
                };
                Self {
                    label,
                    info: format!("{width}×{height} · {size}"),
                    image: Some(RefCell::new(picker.new_resize_protocol(shown))),
                }
            }
            Err(_) => Self { label, info: format!("{size} · not a readable image"), image: None },
        }
    }

    /// Whether there is an image to draw.
    pub fn has_image(&self) -> bool {
        self.image.is_some()
    }
}

/// `1.2 MB`, `84 KB`, `512 B`.
#[allow(clippy::cast_precision_loss)] // a display figure; 52 bits of mantissa is plenty
pub fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Draw the preview into `area`: the sides side by side, each with its label on top and its
/// info line under the image.
pub fn render(frame: &mut Frame, view: &ImageView, area: Rect, dim: Style, strong: Style) {
    if view.sides.is_empty() || area.width < 4 || area.height < 3 {
        return;
    }
    let count = view.sides.len() as u32;
    let columns = Layout::horizontal(vec![Constraint::Ratio(1, count); view.sides.len()])
        .spacing(2)
        .split(area);
    for (side, column) in view.sides.iter().zip(columns.iter()) {
        let rows =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
                .split(*column);
        frame.render_widget(Paragraph::new(Line::from(Span::styled(side.label, strong))), rows[0]);
        if let Some(image) = &side.image {
            frame.render_stateful_widget(
                StatefulImage::default(),
                rows[1],
                &mut *image.borrow_mut(),
            );
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(side.info.clone(), dim))),
            rows[2],
        );
    }
}

/// The picker for this terminal: its graphics when it answers, half-blocks otherwise.
/// `REVIEWR_IMAGES=off` turns previews off; `halfblocks` forces half-blocks.
pub fn picker() -> Option<Picker> {
    match std::env::var("REVIEWR_IMAGES").ok().as_deref() {
        Some("off" | "0") => None,
        Some("halfblocks") => Some(Picker::halfblocks()),
        _ => Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn image_names_are_recognised_by_extension() {
        assert!(is_image_path("docs/logo.PNG"));
        assert!(is_image_path("a/b.webp"));
        assert!(!is_image_path("src/main.rs"));
        assert!(!is_image_path("diagram.svg"), "no SVG");
    }

    #[test]
    fn each_change_kind_shows_the_sides_it_has() {
        assert_eq!(sides_for(ChangeKind::Modified), (true, true));
        assert_eq!(sides_for(ChangeKind::Added), (false, true));
        assert_eq!(sides_for(ChangeKind::Untracked), (false, true));
        assert_eq!(sides_for(ChangeKind::Deleted), (true, false));
    }

    #[test]
    fn a_side_decodes_reports_and_falls_back() {
        let picker = Picker::halfblocks();
        let side = ImageSide::new("after", Ok(png(30, 20)), &picker);
        assert!(side.has_image());
        assert!(side.info.starts_with("30×20 · "), "{}", side.info);
        let broken = ImageSide::new("after", Ok(b"not an image".to_vec()), &picker);
        assert!(!broken.has_image());
        assert!(broken.info.contains("not a readable image"), "{}", broken.info);
        let huge = ImageSide::new("after", Ok(vec![0; MAX_IMAGE_BYTES + 1]), &picker);
        assert!(huge.info.contains("too large"), "{}", huge.info);
        let missing = ImageSide::new("before", Err("unreadable".into()), &picker);
        assert_eq!(missing.info, "unreadable");
    }

    #[test]
    fn sizes_read_short() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(84 * 1024), "84 KB");
        assert_eq!(human_size(3 * 1024 * 1024 / 2), "1.5 MB");
    }
}
