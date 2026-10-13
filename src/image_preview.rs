//! Bounded raster-image preparation shared by the image worker and its metadata fallback.

use image::ImageDecoder;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};
use ratatui::layout::Size;
use ratatui_image::{
    FontSize, Resize,
    protocol::{Protocol, kitty::Kitty},
};
use std::{io::Cursor, sync::Arc};

/// The largest encoded side admitted from git or the worktree.
pub const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
/// A decoder may not admit dimensions larger than this on either axis.
pub const MAX_DIMENSION: u32 = 8_192;
/// The decoded image is limited independently of its encoded size.
pub const MAX_PIXELS: u64 = 8 * 1024 * 1024;
/// Best-effort decoder allocation budget, separate from source and display-pixel bounds.
pub const MAX_DECODED_BYTES: u64 = MAX_PIXELS * 8;
/// A prepared side cannot put more than this many RGBA bytes on the terminal wire.
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024;
/// Kitty's placeholder diacritic table supports at most 297 cells per axis.
pub const MAX_CELLS: u16 = 297;

/// A source's stable provenance, shown even when the graphics hop is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    Blob { oid: String },
    Worktree { digest: [u8; 32] },
    Unavailable { detail: String },
    Missing,
}

/// A successful, bounded decode. Animated formats deliberately retain their first frame.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub image: Arc<DynamicImage>,
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    pub provenance: Provenance,
}

/// A never-rendered Kitty protocol made by the image worker for one session/geometry epoch.
#[derive(Clone)]
pub struct Prepared {
    pub protocol: Protocol,
    pub cells: (u16, u16),
    pub image_id: u32,
    fitted: Arc<DynamicImage>,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("cells", &self.cells)
            .field("image_id", &self.image_id)
            .finish_non_exhaustive()
    }
}

/// A per-side result which always has a human-readable metadata representation.
#[derive(Clone, Debug)]
pub enum Side {
    Missing,
    Unsupported { detail: String, provenance: Provenance },
    Error { detail: String, provenance: Provenance },
    Metadata { format: ImageFormat, width: u32, height: u32, provenance: Provenance },
    Ready(Decoded),
}

impl Side {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Missing => "missing".into(),
            Self::Unsupported { detail, .. } | Self::Error { detail, .. } => detail.clone(),
            Self::Metadata { format, width, height, .. } => {
                format!("{} · {width}×{height}", format_name(*format))
            }
            Self::Ready(decoded) => {
                format!("{} · {}×{}", format_name(decoded.format), decoded.width, decoded.height)
            }
        }
    }

    #[must_use]
    pub fn provenance(&self) -> Provenance {
        match self {
            Self::Missing => Provenance::Missing,
            Self::Unsupported { provenance, .. } | Self::Error { provenance, .. } => {
                provenance.clone()
            }
            Self::Metadata { provenance, .. } => provenance.clone(),
            Self::Ready(decoded) => decoded.provenance.clone(),
        }
    }
}

/// Decode exact source bytes. This never smudges LFS pointers or asks git for a transformed view.
#[must_use]
pub fn decode(bytes: &[u8], provenance: Provenance) -> Side {
    decode_mode(bytes, provenance, true)
}

/// Read bounded header metadata without allocating or decoding the original pixel buffer.
#[must_use]
pub fn metadata(bytes: &[u8], provenance: Provenance) -> Side {
    decode_mode(bytes, provenance, false)
}

fn decode_mode(bytes: &[u8], provenance: Provenance, pixels: bool) -> Side {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Side::Error {
            detail: format!(
                "image exceeds the {} MiB preview limit",
                MAX_SOURCE_BYTES / 1024 / 1024
            ),
            provenance,
        };
    }
    if is_lfs_pointer(bytes) {
        return Side::Unsupported {
            detail: "Git LFS pointer; preview uses no network fetch".into(),
            provenance,
        };
    }
    let format = match image::guess_format(bytes) {
        Ok(
            format @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP),
        ) => format,
        Ok(ImageFormat::Avif | ImageFormat::Bmp | ImageFormat::Ico | ImageFormat::Tiff) => {
            return Side::Unsupported {
                detail: "unsupported image format (PNG, JPEG, GIF, and WebP are supported)".into(),
                provenance,
            };
        }
        Ok(_) => {
            return Side::Unsupported { detail: "unsupported image format".into(), provenance };
        }
        Err(_) => {
            return Side::Unsupported { detail: "not a supported raster image".into(), provenance };
        }
    };
    // The reader applies limits while constructing a decoder (PNG needs them before that), so an
    // advertised dimension cannot force the old decode-then-check allocation.
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut header_limits = limits.clone();
    header_limits.max_image_width = None;
    header_limits.max_image_height = None;
    reader.limits(header_limits);
    let (width, height) = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(error) => {
            return Side::Error {
                detail: format!("{} header is not admissible: {error}", format_name(format)),
                provenance,
            };
        }
    };
    if width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width).checked_mul(u64::from(height)).is_none_or(|pixels| pixels > MAX_PIXELS)
    {
        return Side::Error {
            detail: format!(
                "{} · {width}×{height}: exceeds preview size limit; open in an external editor",
                format_name(format)
            ),
            provenance,
        };
    }
    if !pixels {
        return Side::Metadata { format, width, height, provenance };
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(error) => {
            return Side::Error { detail: format!("could not read image: {error}"), provenance };
        }
    };
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let image = match DynamicImage::from_decoder(decoder) {
        Ok(image) => image,
        Err(error) => {
            return Side::Error {
                detail: format!(
                    "{} · {width}×{height}: could not decode image: {error}",
                    format_name(format)
                ),
                provenance,
            };
        }
    };
    let mut image = DynamicImage::ImageRgba8(image.into_rgba8());
    image.apply_orientation(orientation);
    Side::Ready(Decoded { image: Arc::new(image), format, width, height, provenance })
}

/// Fit and encode off the frame loop; pad to cell geometry and composite alpha on neutral gray.
pub fn prepare(
    decoded: &Decoded,
    image_id: u32,
    available_width: u16,
    available_height: u16,
    metrics: (u16, u16),
) -> Result<Prepared, String> {
    if available_width == 0
        || available_height == 0
        || decoded.image.width() == 0
        || decoded.image.height() == 0
    {
        return Err("pane is too small for graphics; enlarge it".into());
    }
    let metrics = (metrics.0.max(1), metrics.1.max(1));
    // The library's proportional fit admits at most u16::MAX pixels per axis.
    let available = Size::new(
        available_width.min(MAX_CELLS).min(u16::MAX / metrics.0),
        available_height.min(MAX_CELLS).min(u16::MAX / metrics.1),
    );
    let size = Resize::Scale(None).size_for(
        &decoded.image,
        FontSize::new(metrics.0, metrics.1),
        available,
    );
    let cells = (size.width, size.height);
    // Keep the cell aspect while bounding the encoded pixels independently of pane size.
    let mut width = u32::from(cells.0) * u32::from(metrics.0.max(1));
    let mut height = u32::from(cells.1) * u32::from(metrics.1.max(1));
    let scale = (f64::from(decoded.image.width()) / f64::from(width))
        .max(f64::from(decoded.image.height()) / f64::from(height))
        .min(1.0);
    width = (f64::from(width) * scale).ceil() as u32;
    height = (f64::from(height) * scale).ceil() as u32;
    while u64::from(width) * u64::from(height) * 4 > MAX_UPLOAD_BYTES {
        width = width.div_ceil(2).max(1);
        height = height.div_ceil(2).max(1);
    }
    let flattened =
        decoded.image.as_rgba8().filter(|rgba| rgba.pixels().any(|pixel| pixel.0[3] != 255)).map(
            |rgba| {
                let mut canvas = image::RgbaImage::from_pixel(
                    rgba.width(),
                    rgba.height(),
                    image::Rgba([128, 128, 128, 255]),
                );
                image::imageops::overlay(&mut canvas, rgba, 0, 0);
                DynamicImage::ImageRgba8(canvas)
            },
        );
    let fitted = Resize::Scale(Some(FilterType::Triangle)).resize(
        flattened.as_ref().unwrap_or(&decoded.image),
        FontSize::new(1, 1),
        Size::new(width as u16, height as u16),
        Some(image::Rgba([128, 128, 128, 255])),
    );
    encode(Arc::new(fitted), cells, image_id)
}

/// Reconstruct a fresh protocol from the bounded fitted cache, never a cloned sent flag.
pub fn reconstruct(prepared: &Prepared, image_id: u32) -> Result<Prepared, String> {
    encode(Arc::clone(&prepared.fitted), prepared.cells, image_id)
}

fn encode(fitted: Arc<DynamicImage>, cells: (u16, u16), image_id: u32) -> Result<Prepared, String> {
    let kitty = Kitty::new((*fitted).clone(), Size::new(cells.0, cells.1), image_id, false, false)
        .map_err(|error| format!("could not encode Kitty image: {error}"))?;
    Ok(Prepared { protocol: Protocol::Kitty(kitty), cells, image_id, fitted })
}

fn is_lfs_pointer(bytes: &[u8]) -> bool {
    bytes
        .strip_prefix(b"version https://git-lfs.github.com/spec/v1")
        .is_some_and(|tail| tail.starts_with(b"\n") || tail.starts_with(b"\r\n"))
}

fn format_name(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF (first frame)",
        ImageFormat::WebP => "WebP (first frame)",
        _ => "image",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lfs_pointers_are_never_fetched_or_decoded() {
        let side = decode(
            b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\n",
            Provenance::Missing,
        );
        assert!(matches!(side, Side::Unsupported { detail, .. } if detail.contains("LFS")));
    }

    #[test]
    fn library_fit_preserves_aspect_and_bounds_encoded_pixels() {
        for (width, height, available, metrics, expected) in [
            (400, 200, (1000, 1000), (10, 20), (297, 75)),
            (200, 400, (30, 10), (10, 20), (10, 10)),
            (8192, 1, (30, 10), (10, 20), (30, 1)),
            (8192, 1, (1000, 1000), (256, 256), (255, 1)),
        ] {
            let decoded = Decoded {
                image: Arc::new(DynamicImage::ImageRgba8(image::RgbaImage::new(width, height))),
                format: ImageFormat::Png,
                width,
                height,
                provenance: Provenance::Missing,
            };
            let prepared = prepare(&decoded, 1, available.0, available.1, metrics).unwrap();
            assert_eq!(prepared.cells, expected);
            assert!(
                u64::from(prepared.fitted.width()) * u64::from(prepared.fitted.height()) * 4
                    <= MAX_UPLOAD_BYTES
            );
            assert!(prepared.fitted.to_rgba8().pixels().all(|pixel| pixel.0[3] == 255));
        }
    }
}
