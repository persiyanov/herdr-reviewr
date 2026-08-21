//! Image classification and preview helpers for the Changes Diff view
//! (`specs/diff-view.md` Image preview).

use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::Cursor;

use image::DynamicImage;
use ratatui::layout::{Rect, Size};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

/// Byte budget for a single image side. Larger blobs degrade to `too_large` without decode.
pub const MAX_IMAGE_BYTES: usize = 8_000_000;

/// How raw file bytes should be shown in the Diff view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContentKind {
    /// Valid image decode — paint the visual preview.
    Image,
    /// No NUL and not an image — the text diff pipeline.
    Text,
    /// NUL-bearing non-image (or undecodable binary) — the binary notice.
    Binary,
    /// Over the image byte budget.
    TooLarge,
}

/// Classify `bytes` for the Changes Diff view: image decode wins over the NUL binary rule.
#[must_use]
pub fn classify(bytes: &[u8]) -> ContentKind {
    if bytes.is_empty() {
        return ContentKind::Text;
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return ContentKind::TooLarge;
    }
    if decode(bytes).is_some() {
        return ContentKind::Image;
    }
    if bytes.contains(&0) { ContentKind::Binary } else { ContentKind::Text }
}

/// Decode image bytes when the format is recognized; `None` on failure.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    reader.decode().ok()
}

/// Content hash for cache invalidation of an image pane (path-independent; path is separate).
#[must_use]
pub fn bytes_hash(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// The open image in the Diff pane: decoded pixels plus a fitted protocol for paint.
///
/// `Protocol` does not implement `Debug`; this type does, so `App` can keep deriving it.
pub struct ImagePane {
    pub path: String,
    pub content_hash: u64,
    image: DynamicImage,
    protocol: Option<Protocol>,
    /// Cell size the current `protocol` was fitted for; recreate when the pane area changes.
    fitted: Option<Size>,
}

impl fmt::Debug for ImagePane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImagePane")
            .field("path", &self.path)
            .field("content_hash", &self.content_hash)
            .field("fitted", &self.fitted)
            .field("has_protocol", &self.protocol.is_some())
            .finish_non_exhaustive()
    }
}

impl ImagePane {
    /// Build a pane from already-decoded pixels. Protocol is created on first fit.
    #[must_use]
    pub fn new(path: String, content_hash: u64, image: DynamicImage) -> Self {
        Self { path, content_hash, image, protocol: None, fitted: None }
    }

    /// Ensure `protocol` fits `area`, recreating when the cell size changes.
    /// Returns `false` when encoding fails (caller paints the error notice).
    pub fn ensure_fitted(&mut self, picker: &Picker, area: Size) -> bool {
        if area.width == 0 || area.height == 0 {
            return false;
        }
        if self.fitted == Some(area) && self.protocol.is_some() {
            return true;
        }
        if let Ok(protocol) = picker.new_protocol(self.image.clone(), area, Resize::Fit(None)) {
            self.protocol.replace(protocol);
            self.fitted = Some(area);
            true
        } else {
            self.protocol = None;
            self.fitted = None;
            false
        }
    }

    /// The protocol ready for [`Image`], if fitted.
    #[must_use]
    pub fn protocol(&self) -> Option<&Protocol> {
        self.protocol.as_ref()
    }
}

/// Center a widget of `size` inside `area`.
#[must_use]
pub fn centered(area: Rect, size: Size) -> Rect {
    let width = size.width.min(area.width);
    let height = size.height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect { x, y, width, height }
}

/// Paint an fitted image protocol into `area`, centered.
pub fn render_centered(frame: &mut ratatui::Frame, protocol: &Protocol, area: Rect) {
    let size = protocol.size();
    let target = centered(area, size);
    frame.render_widget(Image::new(protocol), target);
}

/// Prefer the worktree (new) side when present; otherwise the old side (a deletion).
#[must_use]
pub fn display_side<'a>(old: &'a [u8], new: &'a [u8]) -> &'a [u8] {
    if new.is_empty() { old } else { new }
}

#[cfg(test)]
mod tests {
    use super::{ContentKind, MAX_IMAGE_BYTES, classify, decode, display_side};

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(2, 2, image::Rgb([0xff, 0x00, 0x80]));
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode png");
        buf.into_inner()
    }

    use std::io::Cursor;

    #[test]
    fn classify_png_as_image() {
        let png = tiny_png();
        assert!(decode(&png).is_some());
        assert_eq!(classify(&png), ContentKind::Image);
    }

    #[test]
    fn classify_text_as_text() {
        assert_eq!(classify(b"fn main() {}\n"), ContentKind::Text);
    }

    #[test]
    fn classify_nul_binary_as_binary() {
        assert_eq!(classify(b"\0\0\0seed\0"), ContentKind::Binary);
    }

    #[test]
    fn classify_empty_as_text() {
        assert_eq!(classify(b""), ContentKind::Text);
    }

    #[test]
    fn classify_corrupt_png_header_with_nul_as_binary() {
        // PNG magic then NUL junk that will not decode.
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 1, b'I', b'H', b'D', b'R', 0]);
        assert_eq!(classify(&bytes), ContentKind::Binary);
    }

    #[test]
    fn classify_oversize_as_too_large() {
        let mut bytes = tiny_png();
        bytes.resize(MAX_IMAGE_BYTES + 1, 0);
        assert_eq!(classify(&bytes), ContentKind::TooLarge);
    }

    #[test]
    fn display_side_prefers_new() {
        assert_eq!(display_side(b"old", b"new"), b"new");
        assert_eq!(display_side(b"old", b""), b"old");
        assert_eq!(display_side(b"", b"new"), b"new");
    }
}
