//! Application-owned Kitty session state and the narrow library-placement wrapper.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Instant;

use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

use crate::image_preview::Prepared;

/// What the terminal path, rather than an unobservable outer client, acknowledged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HopGraphics {
    #[default]
    Unknown,
    Accepted,
    Rejected,
}

/// Cell metrics are reported by the responding hop; they do not certify outer pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CellMetrics {
    Measured {
        width: u16,
        height: u16,
    },
    #[default]
    Estimated,
}

/// The graphics lifetime independent of the visible file's stable identity.
#[derive(Debug, Default)]
pub struct Session {
    pub epoch: u64,
    pub hop: HopGraphics,
    pub metrics: CellMetrics,
    next_id: u32,
    live_ids: Vec<u32>,
    retired_ids: Vec<u32>,
    attached: bool,
    pending_probe: Option<(u32, Instant)>,
    pending_metrics: Option<Instant>,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.subsec_nanos());
        let seed = (std::process::id().wrapping_mul(0x9e37_79b9) ^ stamp) & 0x7fff_ffff;
        let mut session = Self::default();
        session.next_id = seed.max(1);
        session
    }

    pub(crate) fn attached() -> Self {
        let mut session = Self::new();
        session.attached = true;
        session
    }

    pub fn cleanup_sent(&mut self) {
        self.retired_ids.clear();
    }

    pub fn reserve_id(&mut self) -> Option<u32> {
        let id = self.next_id.max(1);
        self.next_id = id.checked_add(1)?;
        Some(id)
    }

    pub fn begin_probe(&mut self) -> Option<String> {
        if cfg!(windows) {
            self.hop = HopGraphics::Rejected;
            return None;
        }
        if self.hop != HopGraphics::Unknown || self.pending_probe.is_some() {
            return None;
        }
        let Some(id) = self.reserve_id() else {
            self.hop = HopGraphics::Rejected;
            return None;
        };
        self.pending_probe = Some((id, Instant::now()));
        self.pending_metrics = Some(Instant::now());
        Some(format!("\x1b_Gi={id},a=q,t=d,f=24,s=1,v=1;AAAA\x1b\\\x1b[c\x1b[16t"))
    }

    pub fn probe_wait(&self) -> Option<std::time::Duration> {
        self.pending_probe
            .map(|(_, at)| at)
            .into_iter()
            .chain(self.pending_metrics)
            .map(|at| std::time::Duration::from_secs(1).saturating_sub(at.elapsed()))
            .min()
    }

    pub fn expire_probe(&mut self) {
        if self.pending_metrics.is_some_and(|at| at.elapsed() >= std::time::Duration::from_secs(1))
        {
            self.pending_metrics = None;
        }
        if self
            .pending_probe
            .is_some_and(|(_, at)| at.elapsed() >= std::time::Duration::from_secs(1))
        {
            self.pending_probe = None;
            self.hop = HopGraphics::Rejected;
        }
    }

    pub fn pixel_metrics(&self) -> (u16, u16) {
        match self.metrics {
            CellMetrics::Measured { width, height } => (width, height),
            CellMetrics::Estimated => (10, 20),
        }
    }

    pub fn adopt(&mut self, id: u32) {
        if !self.live_ids.contains(&id) {
            self.live_ids.push(id);
        }
    }

    pub fn has_images(&self) -> bool {
        !self.live_ids.is_empty()
    }

    pub fn retain(&mut self, ids: &[u32]) -> String {
        let mut live = Vec::new();
        for id in self.live_ids.drain(..) {
            if ids.contains(&id) {
                live.push(id);
            } else if !self.retired_ids.contains(&id) {
                self.retired_ids.push(id);
            }
        }
        self.live_ids = live;
        let mut out = String::new();
        for id in &self.retired_ids {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        }
        out
    }

    pub fn reset_protocols(&mut self) -> String {
        self.epoch = self.epoch.wrapping_add(1);
        self.cleanup()
    }

    pub fn invalidate(&mut self) -> String {
        let cleanup = self.reset_protocols();
        self.hop = HopGraphics::Unknown;
        self.metrics = CellMetrics::Estimated;
        self.pending_probe = None;
        self.pending_metrics = None;
        cleanup
    }

    pub fn cleanup(&mut self) -> String {
        self.retain(&[])
    }

    pub fn reply(&mut self, bytes: &[u8]) {
        if self.pending_metrics.is_some()
            && let Some(body) = bytes.strip_prefix(b"\x1b[6;").and_then(|b| b.strip_suffix(b"t"))
            && let Ok(body) = std::str::from_utf8(body)
            && let Some((height, width)) = body.split_once(';')
            && let (Ok(height), Ok(width)) = (height.parse::<u16>(), width.parse::<u16>())
            && (1..=256).contains(&height)
            && (1..=256).contains(&width)
        {
            self.pending_metrics = None;
            self.metrics = CellMetrics::Measured { width, height };
            return;
        }
        let Some((query_id, _)) = self.pending_probe else { return };
        let Some(body) = bytes.strip_prefix(b"\x1b_G").and_then(|b| b.strip_suffix(b"\x1b\\"))
        else {
            return;
        };
        let Ok(body) = std::str::from_utf8(body) else { return };
        let Some((fields, message)) = body.split_once(';') else { return };
        let id = fields
            .split(',')
            .find_map(|field| field.strip_prefix("i=").and_then(|id| id.parse::<u32>().ok()));
        if id != Some(query_id) {
            return;
        }
        self.hop = if message == "OK" { HopGraphics::Accepted } else { HopGraphics::Rejected };
        self.pending_probe = None;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.attached {
            return;
        }
        let cleanup = self.cleanup();
        if cleanup.is_empty() {
            return;
        }
        let _ = std::io::stdout().write_all(cleanup.as_bytes());
        let _ = std::io::stdout().flush();
    }
}

/// Adapt the established encoder in the actual live buffer to owned native placements.
#[derive(Debug)]
pub struct KittyImage<'a> {
    prepared: &'a Prepared,
}

impl<'a> KittyImage<'a> {
    pub const fn new(prepared: &'a Prepared) -> Self {
        Self { prepared }
    }
}

impl Widget for KittyImage<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (width, height) = self.prepared.cells;
        if width == 0 || height == 0 || width > area.width || height > area.height {
            return;
        }
        let placement = Rect::new(area.x, area.y, width, height);
        if placement.intersection(buf.area) != placement {
            return;
        }
        ratatui_image::Image::new(&self.prepared.protocol).render(placement, buf);
        let first =
            buf.cell((area.x, area.y)).map_or_else(String::new, |cell| cell.symbol().to_string());
        let header = format!("a=T,c={width},r={height},C=1,z=0,");
        let upload = first.replacen("a=T,U=1,", &header, 1);
        let fresh = upload != first;
        for y in placement.y..placement.bottom() {
            for x in placement.x..placement.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(" ");
                    if !fresh {
                        cell.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
                    }
                }
            }
        }
        // Paint the region before placing, then restore the cursor the backend tracks.
        if fresh
            && let Some(end) = upload.rfind("\x1b\\")
            && let Some(cell) = buf.cell_mut((placement.right() - 1, placement.bottom() - 1))
        {
            cell.set_symbol(&format!(
                " \x1b7\x1b[{};{}H{}\x1b8",
                placement.y + 1,
                placement.x + 1,
                &upload[..end + 2]
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_preview::{Decoded, Provenance};
    use image::{DynamicImage, RgbaImage};

    #[test]
    fn only_a_matching_ack_accepts_the_hop_and_cleanup_is_targeted() {
        let mut session = Session::default();
        session.pending_probe = Some((5, Instant::now()));
        session.reply(b"\x1b_Gi=4;OK\x1b\\");
        assert_eq!(session.hop, HopGraphics::Unknown);
        session.reply(b"\x1b_Gi=5;OK\x1b\\");
        assert_eq!(session.hop, HopGraphics::Accepted);
        session.adopt(17);
        assert_eq!(session.cleanup(), "\x1b_Ga=d,d=I,i=17,q=2\x1b\\");
    }

    #[test]
    fn queued_replies_are_processed_before_probe_expiry() {
        let at = Instant::now().checked_sub(std::time::Duration::from_secs(2)).unwrap();
        let mut session = Session::default();
        session.pending_probe = Some((5, at));
        session.pending_metrics = Some(at);
        let queued = [b"\x1b_Gi=5;OK\x1b\\".as_slice(), b"\x1b[6;20;10t"];
        for reply in queued {
            session.reply(reply);
        }
        session.expire_probe();
        assert_eq!(session.hop, HopGraphics::Accepted);
        assert_eq!(session.metrics, CellMetrics::Measured { width: 10, height: 20 });
        assert_eq!(session.probe_wait(), None);
    }

    #[test]
    fn replies_arriving_after_loop_expiry_are_ignored() {
        let at = Instant::now().checked_sub(std::time::Duration::from_secs(2)).unwrap();
        let mut session = Session::default();
        session.pending_probe = Some((5, at));
        session.pending_metrics = Some(at);
        session.expire_probe();
        session.reply(b"\x1b_Gi=5;OK\x1b\\");
        session.reply(b"\x1b[6;20;10t");
        assert_eq!(session.hop, HopGraphics::Rejected);
        assert_eq!(session.metrics, CellMetrics::Estimated);
        assert_eq!(session.probe_wait(), None);
    }

    #[test]
    fn reprobe_ignores_the_old_acknowledgment_and_accepts_the_current_id() {
        if cfg!(windows) {
            return;
        }
        let mut session = Session::new();
        session.begin_probe().unwrap();
        let old_id = session.pending_probe.unwrap().0;
        session.adopt(old_id);
        assert!(!session.invalidate().is_empty());
        session.begin_probe().unwrap();
        let current_id = session.pending_probe.unwrap().0;
        assert_ne!(current_id, old_id);
        session.reply(format!("\x1b_Gi={old_id};OK\x1b\\").as_bytes());
        assert_eq!(session.hop, HopGraphics::Unknown);
        assert_eq!(session.pending_probe.unwrap().0, current_id);
        session.reply(format!("\x1b_Gi={current_id};OK\x1b\\").as_bytes());
        assert_eq!(session.hop, HopGraphics::Accepted);
        assert_eq!(session.pending_probe, None);
    }

    #[test]
    fn ids_do_not_start_at_zero_or_repeat() {
        let mut session = Session::new();
        let first = session.reserve_id().unwrap();
        assert_ne!(first, 0);
        assert_eq!(session.reserve_id(), Some(first + 1));
    }

    #[test]
    fn wrapper_amends_the_librarys_single_live_virtual_upload() {
        let decoded = Decoded {
            image: std::sync::Arc::new(DynamicImage::ImageRgba8(RgbaImage::new(2, 1))),
            format: image::ImageFormat::Png,
            width: 2,
            height: 1,
            provenance: Provenance::Missing,
        };
        let prepared =
            crate::image_preview::prepare(&decoded, 0x5200_0001, 3, 2, (10, 20)).unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        KittyImage::new(&prepared).render(Rect::new(0, 0, 3, 2), &mut buffer);
        let first = buffer.cell((2, 0)).unwrap().symbol();
        assert!(first.contains("a=T,c=3,r=1,C=1,z=0,"), "{first:?}");
        assert_eq!(first.matches("a=T").count(), 1);
        assert_eq!(buffer.cell((1, 0)).unwrap().symbol(), " ");
    }
}
