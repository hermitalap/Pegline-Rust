//! Hover-preview geometry and dwell state, ported from `HoverPreview.cs`
//! (`PreviewLayout` and `PreviewHoverState`).
//!
//! Everything here is plain arithmetic in device pixels with no Win32
//! dependency, so the rules that used to be tested by
//! `archive/wpf-v1/tests/HoverPreviewTests.cs` are unit tests here. The window
//! itself lives in `window/preview.rs`; the app feeds it with [`bounds`].

use crate::types::{CardId, RectI};

/// White frame around the photo, DIP, on each side.
pub const INSET_DIP: f32 = 6.0;
/// Gap between the preview and the monitor work-area edge, DIP.
pub const MARGIN_DIP: f32 = 12.0;
/// Gap between the preview and the hovered card, DIP.
pub const GAP_DIP: f32 = 12.0;
/// Largest preview, DIP.
pub const MAX_WIDTH_DIP: f32 = 900.0;
pub const MAX_HEIGHT_DIP: f32 = 650.0;
/// How long the pointer must rest on one photo before decoding it.
pub const HOVER_DELAY: f64 = 0.35;

/// Where the preview goes, in device pixels, for a source of `source` pixels.
///
/// The card rectangle and the work area are device pixels; `scale` converts the
/// DIP margins and caps to this monitor's pixels. Returns an empty rectangle
/// when the source is invalid or neither side of the card has room, which means
/// "do not show a preview" (BEHAVIOR §7).
pub fn bounds(source: (u32, u32), anchor: RectI, work: RectI, scale: f32) -> RectI {
    let (source_w, source_h) = (source.0 as f64, source.1 as f64);
    if source_w <= 0.0
        || source_h <= 0.0
        || anchor.is_empty()
        || work.is_empty()
        || !scale.is_finite()
        || scale <= 0.0
    {
        return RectI::default();
    }

    let scale = scale as f64;
    let margin = MARGIN_DIP as f64 * scale;
    let inset = INSET_DIP as f64 * scale;
    let gap = GAP_DIP as f64 * scale;
    let left = (work.left as f64 + margin).ceil();
    let right = (work.right as f64 - margin).floor();
    let top = (work.top as f64 + margin).ceil();
    let bottom = (work.bottom as f64 - margin).floor();
    let below = (anchor.bottom as f64 + gap).ceil().max(top);
    let above = (anchor.top as f64 - gap).floor().min(bottom);
    let place_below = bottom - below > 2.0 * inset + 1.0;
    let available = if place_below {
        bottom - below
    } else {
        above - top
    };
    let max_width = (MAX_WIDTH_DIP as f64 * scale).min(right - left);
    let max_height = (MAX_HEIGHT_DIP as f64 * scale).min(available);
    if max_width <= 2.0 * inset + 1.0 || max_height <= 2.0 * inset + 1.0 {
        return RectI::default();
    }

    // "Do not enlarge the source" lives in this `min(1, ...)`: a small image
    // gets a small frame, never a blurry upscale.
    let fit = 1.0f64
        .min(((max_width - 2.0 * inset) / source_w).min((max_height - 2.0 * inset) / source_h));
    let width = max_width.floor().min((source_w * fit + 2.0 * inset).ceil());
    let height = max_height
        .floor()
        .min((source_h * fit + 2.0 * inset).ceil());
    let centered = anchor.center_x() as f64 - width * 0.5;
    let x = left.max((right - width).min(centered)).round();
    let y = if place_below { below } else { above - height };
    RectI::new(x as i32, y as i32, (x + width) as i32, (y + height) as i32)
}

/// The decode size for `bounds`: the longest photo side, never the frame.
pub fn decode_max(bounds: RectI, scale: f32) -> u32 {
    let inset = (INSET_DIP as f64 * scale as f64).round();
    let longest = bounds.width().max(bounds.height()) as f64 - 2.0 * inset;
    (longest.ceil() as i64).clamp(1, u32::MAX as i64) as u32
}

/// The dwell state for one photo: which card, which revision of it, and when
/// the pointer arrived. A generation number invalidates results that were
/// decoded for a card or an image that is no longer under the pointer.
#[derive(Debug, Default)]
pub struct HoverState {
    id: Option<CardId>,
    revision: u64,
    stamp: u64,
    since: f64,
    requested: bool,
    version: u32,
}

impl HoverState {
    /// Feeds one observation (`Some(id)` while the pointer is over a card).
    /// Returns true exactly once per stable hover, 0.35 s after it started or
    /// after the image was replaced under the pointer.
    pub fn observe(&mut self, next: Option<CardId>, revision: u64, stamp: u64, now: f64) -> bool {
        let Some(next) = next else {
            self.reset();
            return false;
        };
        if self.id != Some(next) {
            self.id = Some(next);
            self.since = now;
            self.revision = revision;
            self.stamp = stamp;
            self.requested = false;
            self.version = self.version.wrapping_add(1);
        } else if self.revision != revision || self.stamp != stamp {
            // The file was edited (or a note recoloured) while hovered: restart
            // the dwell so the preview never shows the old pixels.
            self.revision = revision;
            self.stamp = stamp;
            self.requested = false;
            self.version = self.version.wrapping_add(1);
        }
        if self.requested || now - self.since < HOVER_DELAY {
            return false;
        }
        self.requested = true;
        true
    }

    /// Whether a decode started for generation `version` may still be shown.
    pub fn is_current(&self, version: u32) -> bool {
        self.id.is_some() && self.requested && self.version == version
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// The pointer left, the line retracted, a drag started, and so on.
    pub fn reset(&mut self) {
        if self.id.is_some() {
            self.version = self.version.wrapping_add(1);
        }
        self.id = None;
        self.revision = 0;
        self.stamp = 0;
        self.requested = false;
    }

    pub fn is_active(&self) -> bool {
        self.id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contained(bounds: RectI, area: RectI) {
        assert!(!bounds.is_empty(), "preview has no usable area");
        assert!(
            bounds.left >= area.left
                && bounds.top >= area.top
                && bounds.right <= area.right
                && bounds.bottom <= area.bottom,
            "preview escaped the monitor work area: {bounds:?} in {area:?}"
        );
    }

    fn aspect(bounds: RectI, expected: f64, scale: f64) {
        let ratio =
            (bounds.width() as f64 - 12.0 * scale) / (bounds.height() as f64 - 12.0 * scale);
        assert!(
            (ratio - expected).abs() <= 0.015f64.max(expected * 0.015),
            "preview changed the image aspect ratio: {ratio} vs {expected}"
        );
    }

    #[test]
    fn wide_preview_stays_below_its_card() {
        let anchor = RectI::new(880, 60, 1040, 160);
        let area = RectI::new(0, 0, 1920, 1040);
        let bounds = bounds((4000, 2000), anchor, area, 1.0);
        contained(bounds, area);
        assert!(bounds.top >= 172, "preview overlaps the hovered card");
        assert!(
            bounds.width() <= 900 && bounds.height() <= 650,
            "preview exceeds its size limit"
        );
        aspect(bounds, 2.0, 1.0);
    }

    #[test]
    fn negative_monitor_origins_remain_in_physical_pixels() {
        let area = RectI::new(-1920, -200, 0, 880);
        let anchor = RectI::new(-1910, -140, -1760, -30);
        let bounds = bounds((3200, 1800), anchor, area, 1.5);
        contained(bounds, area);
        assert!(bounds.left < 0, "preview jumped to the primary monitor");
        assert!(
            bounds.top >= -12,
            "preview overlaps the card on the negative-origin monitor"
        );
        aspect(bounds, 16.0 / 9.0, 1.5);
    }

    #[test]
    fn tall_images_fit_the_space_below_the_rope() {
        let area = RectI::new(0, 0, 1280, 720);
        let bounds = bounds((1000, 6000), RectI::new(500, 40, 650, 150), area, 1.0);
        contained(bounds, area);
        assert!(
            bounds.top >= 162 && bounds.bottom <= 708,
            "tall preview extends outside available height"
        );
        aspect(bounds, 1.0 / 6.0, 1.0);
    }

    #[test]
    fn high_dpi_previews_use_the_target_monitor_scale() {
        let area = RectI::new(0, 0, 3840, 2160);
        let bounds = bounds((10000, 5000), RectI::new(1900, 100, 2200, 300), area, 2.0);
        contained(bounds, area);
        assert!(
            bounds.width() > 900 && bounds.width() <= 1800,
            "high DPI cap was treated as unscaled pixels"
        );
        assert!(
            bounds.height() <= 1300 && bounds.top >= 324,
            "high DPI placement exceeds available bounds"
        );
        aspect(bounds, 2.0, 2.0);
    }

    #[test]
    fn preview_uses_space_above_when_no_space_remains_below() {
        let area = RectI::new(0, 0, 1280, 720);
        let bounds = bounds((1600, 1000), RectI::new(500, 680, 650, 715), area, 1.0);
        contained(bounds, area);
        assert!(
            bounds.bottom <= 668,
            "preview overlaps card despite available room above"
        );
    }

    #[test]
    fn small_source_images_are_not_enlarged_into_blurry_previews() {
        let bounds = bounds(
            (40, 20),
            RectI::new(300, 30, 400, 110),
            RectI::new(0, 0, 1280, 720),
            1.0,
        );
        assert!(
            bounds.width() <= 52 && bounds.height() <= 32,
            "small source was enlarged"
        );
        aspect(bounds, 2.0, 1.0);
    }

    #[test]
    fn invalid_dimensions_suppress_the_preview() {
        let source = (800, 600);
        let anchor = RectI::new(100, 50, 200, 150);
        let area = RectI::new(0, 0, 1280, 720);
        assert!(bounds((0, 10), anchor, area, 1.0).is_empty());
        assert!(bounds(source, RectI::default(), area, 1.0).is_empty());
        assert!(bounds(source, anchor, RectI::default(), 1.0).is_empty());
        assert!(bounds(source, anchor, area, f32::NAN).is_empty());
        assert!(bounds(source, anchor, area, 0.0).is_empty());
    }

    #[test]
    fn hover_waits_before_requesting_one_image() {
        let mut state = HoverState::default();
        let now = 1_000.0;
        assert!(!state.observe(Some(7), 1, 1, now));
        assert!(!state.observe(Some(7), 1, 1, now + 0.349));
        assert!(state.observe(Some(7), 1, 1, now + 0.350));
        assert!(!state.observe(Some(7), 1, 1, now + 0.700));
        assert!(state.is_current(state.version()));
    }

    #[test]
    fn switching_cards_cancels_the_previous_request_and_restarts_dwell() {
        let mut state = HoverState::default();
        let now = 1_000.0;
        state.observe(Some(1), 1, 1, now);
        state.observe(Some(1), 1, 1, now + 0.350);
        let stale = state.version();
        assert!(!state.observe(Some(2), 1, 1, now + 0.360));
        assert!(!state.is_current(stale));
        assert!(!state.observe(Some(2), 1, 1, now + 0.709));
        assert!(state.observe(Some(2), 1, 1, now + 0.710));
    }

    #[test]
    fn leaving_cancels_in_flight_work_and_requires_a_fresh_hover() {
        let mut state = HoverState::default();
        let now = 1_000.0;
        state.observe(Some(1), 1, 1, now);
        state.observe(Some(1), 1, 1, now + 0.350);
        let stale = state.version();
        state.reset();
        assert!(!state.is_current(stale));
        assert!(!state.observe(Some(1), 1, 1, now + 0.500));
        assert!(!state.observe(Some(1), 1, 1, now + 0.849));
        assert!(state.observe(Some(1), 1, 1, now + 0.850));
    }

    #[test]
    fn edited_images_replace_the_pending_revision() {
        let mut state = HoverState::default();
        let now = 1_000.0;
        state.observe(Some(1), 1, 1, now);
        state.observe(Some(1), 1, 1, now + 0.350);
        let stale = state.version();
        assert!(state.observe(Some(1), 2, 1, now + 0.400));
        assert!(!state.is_current(stale));
        let stale = state.version();
        assert!(state.observe(Some(1), 2, 2, now + 0.450));
        assert!(!state.is_current(stale));
    }

    #[test]
    fn decode_size_is_the_photo_not_the_frame() {
        let bounds = bounds(
            (4000, 2000),
            RectI::new(880, 60, 1040, 160),
            RectI::new(0, 0, 1920, 1040),
            2.0,
        );
        let max = decode_max(bounds, 2.0);
        assert_eq!(max + 24, bounds.width().max(bounds.height()) as u32);
    }
}
