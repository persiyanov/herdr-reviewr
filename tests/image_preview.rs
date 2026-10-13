//! Public raster preparation, coalescing worker and live native-renderer boundaries.

use herdr_reviewr::{
    image_preview::{self, Decoded, Provenance, Side},
    image_session::{KittyImage, Session},
    image_worker::{Request, Source, ViewIdentity, Worker},
    model::Scope,
    wake,
};
use image::{DynamicImage, ImageFormat, RgbaImage};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use std::{io::Cursor, sync::Arc, time::Duration};

fn encoded(format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    let image =
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 4, image::Rgb([120, 20, 40])));
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

#[test]
fn each_supported_format_reports_original_dimensions_and_fits_non_square_cells() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif, ImageFormat::WebP] {
        let Side::Ready(decoded) = image_preview::decode(&encoded(format), Provenance::Missing)
        else {
            panic!("{format:?} failed")
        };
        assert_eq!((decoded.format, decoded.width, decoded.height), (format, 8, 4));
        let prepared = image_preview::prepare(&decoded, 7, 40, 20, (10, 20)).unwrap();
        assert_eq!(prepared.cells, (40, 10));
        assert_eq!((decoded.width, decoded.height), (8, 4));
    }
}

#[test]
fn clipped_or_empty_draw_does_not_consume_upload_and_subsequent_frames_have_no_placeholders() {
    let decoded = Decoded {
        image: Arc::new(DynamicImage::ImageRgba8(RgbaImage::new(8, 4))),
        format: ImageFormat::Png,
        width: 8,
        height: 4,
        provenance: Provenance::Missing,
    };
    let prepared = image_preview::prepare(&decoded, 123, 20, 10, (10, 20)).unwrap();
    let mut clipped = Buffer::empty(Rect::new(0, 0, 2, 2));
    KittyImage::new(&prepared).render(clipped.area, &mut clipped);
    assert!(!clipped.content.iter().any(|cell| cell.symbol().contains("a=T")));
    let area = Rect::new(3, 2, 20, 10);
    let mut first = Buffer::empty(Rect::new(0, 0, 30, 15));
    KittyImage::new(&prepared).render(area, &mut first);
    let header = first.cell((22, 6)).unwrap().symbol();
    assert!(header.contains("a=T,c=20,r=5,C=1,z=0,"));
    assert!(header.starts_with(" \x1b7\x1b[3;4H"));
    assert!(header.ends_with("\x1b8"));
    for _ in 0..3 {
        let mut next = Buffer::empty(first.area);
        KittyImage::new(&prepared).render(area, &mut next);
        assert!(
            !next
                .content
                .iter()
                .any(|cell| cell.symbol().contains('\u{10eeee}') || cell.symbol().contains("a=T"))
        );
        assert!(
            !first.diff(&next).iter().any(|(x, y, _)| *x >= 3 && *x < 23 && *y >= 2 && *y < 7),
            "later frames may not erase native image cells"
        );
    }
}

#[test]
fn session_retires_only_owned_images_and_invalidates_correlated_probe() {
    let mut session = Session::new();
    if cfg!(windows) {
        assert!(session.begin_probe().is_none());
        assert_eq!(session.hop, herdr_reviewr::image_session::HopGraphics::Rejected);
        return;
    }
    let probe = session.begin_probe().unwrap();
    assert!(probe.contains("a=q,t=d,f=24,s=1,v=1;AAAA"));
    session.adopt(12);
    session.adopt(14);
    assert_eq!(session.retain(&[14]), "\x1b_Ga=d,d=I,i=12,q=2\x1b\\");
    session.cleanup_sent();
    assert_eq!(session.invalidate(), "\x1b_Ga=d,d=I,i=14,q=2\x1b\\");
    assert_eq!(session.epoch, 1);
    assert!(!session.has_images());
    assert_ne!(session.begin_probe().unwrap(), probe);
}

fn demand(repo: &std::path::Path, generation: u64, epoch: u64, graphics: bool) -> Request {
    Request {
        view_key: ViewIdentity {
            repo: repo.into(),
            scope: Scope::Uncommitted,
            base: None,
            selector_epoch: 0,
            pick: None,
            path: "a.png".into(),
            previous_path: None,
        },
        content_generation: generation,
        session_epoch: epoch,
        before: Source::Missing,
        after: if cfg!(unix) {
            Source::Worktree { path: "a.png".into() }
        } else {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["hash-object", "-w", "a.png"])
                .output()
                .unwrap();
            assert!(output.status.success());
            Source::Blob { oid: String::from_utf8(output.stdout).unwrap().trim().into() }
        },
        cells: (20, 10),
        ids: (100 + u32::try_from(generation).unwrap(), 200 + u32::try_from(generation).unwrap()),
        graphics,
        metrics: (10, 20),
        metrics_estimated: true,
    }
}

#[test]
fn worker_metadata_only_and_unchanged_refresh_do_not_encode_or_reupload() {
    let dir = tempfile::tempdir().unwrap();
    if cfg!(not(unix)) {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(dir.path().join("a.png"), encoded(ImageFormat::Png)).unwrap();
    let (tx, rx) = wake::channel(&wake::Waker::detached());
    let worker = Worker::new(tx).unwrap();
    worker.request(demand(dir.path(), 0, 0, false));
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let metadata = worker.take_result().unwrap();
    assert!(matches!(metadata.before, Side::Missing));
    assert!(matches!(metadata.after, Side::Metadata { .. }));
    assert!(metadata.prepared.0.is_none() && metadata.prepared.1.is_none());
    worker.request(demand(dir.path(), 1, 0, true));
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let first = worker.take_result().unwrap();
    let id = first.prepared.1.as_ref().unwrap().image_id;
    let mut frame = Buffer::empty(Rect::new(0, 0, 20, 10));
    KittyImage::new(first.prepared.1.as_ref().unwrap()).render(frame.area, &mut frame);
    worker.request(demand(dir.path(), 2, 0, true));
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let unchanged = worker.take_result().unwrap();
    assert_eq!(unchanged.prepared.1.as_ref().unwrap().image_id, id);
    frame.reset();
    KittyImage::new(unchanged.prepared.1.as_ref().unwrap()).render(frame.area, &mut frame);
    assert!(!frame.content.iter().any(|cell| cell.symbol().contains("a=T")));
    worker.request(demand(dir.path(), 3, 1, true));
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let reset = worker.take_result().unwrap();
    assert_ne!(reset.prepared.1.as_ref().unwrap().image_id, id);
    frame.reset();
    KittyImage::new(reset.prepared.1.as_ref().unwrap()).render(frame.area, &mut frame);
    assert!(frame.content.iter().any(|cell| cell.symbol().contains("a=T")));
}

#[test]
fn jpeg_orientation_changes_display_dimensions_but_preserves_original_metadata() {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut bytes);
    encoder
        .set_exif_metadata(vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 18, 1, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    encoder.encode_image(&DynamicImage::ImageRgb8(image::RgbImage::new(8, 4))).unwrap();
    let Side::Ready(decoded) = image_preview::decode(&bytes, Provenance::Missing) else {
        panic!("JPEG rejected")
    };
    assert_eq!((decoded.width, decoded.height), (8, 4));
    assert_eq!((decoded.image.width(), decoded.image.height()), (4, 8));
    assert!(matches!(
        image_preview::metadata(&bytes, Provenance::Missing),
        Side::Metadata { width: 8, height: 4, .. }
    ));
}

#[test]
fn size_failures_keep_known_format_and_dimensions_and_source_bytes_are_bounded() {
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image::RgbImage::new(8193, 1))
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    let side = image_preview::decode(bytes.get_ref(), Provenance::Missing);
    assert!(matches!(side, Side::Error { .. }));
    assert!(side.label().contains("PNG · 8193×1"));
    let oversized = vec![0; image_preview::MAX_SOURCE_BYTES + 1];
    assert!(matches!(image_preview::decode(&oversized, Provenance::Missing), Side::Error { .. }));
}
