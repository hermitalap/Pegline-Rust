//! Hover-preview geometry and dwell state, ported from `HoverPreview.cs`
//! (`PreviewLayout` and `PreviewHoverState`).
//!
//! Everything here is plain arithmetic in device pixels with no Win32
//! dependency, so the rules that used to be tested by
//! `archive/wpf-v1/tests/HoverPreviewTests.cs` are unit tests here. The window
//! itself lives in `window/preview.rs`; the app feeds it with [`bounds`].

use crate::types::{CardId, RectI};

/// Glass-card inset around the preview content, DIP, on each side. It is the
/// same [`crate::layout::PHOTO_INSET`] a picture card wears on the line, so a
/// preview reads as the card enlarged.
pub const INSET_DIP: f32 = 4.0;
/// Gap between the preview and the monitor work-area edge, DIP.
pub const MARGIN_DIP: f32 = 12.0;
/// Gap between the preview and the hovered card, DIP.
pub const GAP_DIP: f32 = 12.0;
/// Largest visible preview card, DIP.
pub const MAX_WIDTH_DIP: f32 = 900.0;
pub const MAX_HEIGHT_DIP: f32 = 650.0;
/// How long the pointer must rest on one photo before decoding it.
pub const HOVER_DELAY: f64 = 0.35;

/// Where the preview card goes, in device pixels, for a source of `source`
/// pixels.
///
/// The returned rectangle is the *visible* card (the glass frame around the
/// picture); [`window_bounds`] adds the transparent room its baked shadow
/// needs. The anchor rectangle and the work area are device pixels; `scale`
/// converts the DIP margins and caps to this monitor's pixels. Returns an
/// empty rectangle when the source is invalid or neither side of the anchor
/// has room, which means "do not show a preview" (BEHAVIOR §7).
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

/// Where a hover preview of a fixed DIP size goes, in device pixels: below the
/// anchor when it fits, above it otherwise, centred on the anchor and clamped
/// to the work area. `size` is the visible card in DIP; the caller adds the
/// shadow room with [`window_bounds`]. Empty when neither side has room, which
/// means "do not show".
///
/// Notes use this: unlike a photo, a note preview is the same size on every
/// monitor (scaled by the monitor), so it never shrinks to the source.
pub fn fixed_bounds(size: (f32, f32), anchor: RectI, work: RectI, scale: f32) -> RectI {
    if size.0 <= 0.0
        || size.1 <= 0.0
        || !size.0.is_finite()
        || !size.1.is_finite()
        || anchor.is_empty()
        || work.is_empty()
        || !scale.is_finite()
        || scale <= 0.0
    {
        return RectI::default();
    }

    let scale = scale as f64;
    let margin = MARGIN_DIP as f64 * scale;
    let gap = GAP_DIP as f64 * scale;
    let left = (work.left as f64 + margin).ceil();
    let right = (work.right as f64 - margin).floor();
    let top = (work.top as f64 + margin).ceil();
    let bottom = (work.bottom as f64 - margin).floor();
    let width = (size.0 as f64 * scale).round();
    let height = (size.1 as f64 * scale).round();
    if width > right - left || height > bottom - top {
        return RectI::default();
    }
    let below = (anchor.bottom as f64 + gap).ceil().max(top);
    let above = (anchor.top as f64 - gap).floor().min(bottom);
    let y = if below + height <= bottom {
        below
    } else if above - height >= top {
        above - height
    } else {
        return RectI::default();
    };
    let centered = anchor.center_x() as f64 - width * 0.5;
    let x = left.max((right - width).min(centered)).round();
    RectI::new(x as i32, y as i32, (x + width) as i32, (y + height) as i32)
}

/// The transparent window around a visible preview card, in device pixels.
///
/// The card keeps the placement [`bounds`]/[`fixed_bounds`] gave it; this adds
/// `padding_dip` on every side so the baked card shadow fits. The window may
/// reach past the work area (the card itself does not), which is fine: the
/// margin is transparent.
pub fn window_bounds(card: RectI, padding_dip: f32, scale: f32) -> RectI {
    if card.is_empty()
        || !padding_dip.is_finite()
        || padding_dip < 0.0
        || !scale.is_finite()
        || scale <= 0.0
    {
        return RectI::default();
    }
    let pad = (padding_dip as f64 * scale as f64).round() as i32;
    RectI::new(
        card.left - pad,
        card.top - pad,
        card.right + pad,
        card.bottom + pad,
    )
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
        let inset = 2.0 * INSET_DIP as f64 * scale;
        let ratio = (bounds.width() as f64 - inset) / (bounds.height() as f64 - inset);
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
            bounds.width() <= 48 && bounds.height() <= 28,
            "small source was enlarged"
        );
        aspect(bounds, 2.0, 1.0);
    }

    #[test]
    fn a_capped_preview_keeps_the_source_aspect_inside_its_card() {
        // A 200 % monitor tall enough to show the full 900x650 DIP cap.
        let bounds = bounds(
            (4000, 3000),
            RectI::new(1800, 100, 2100, 300),
            RectI::new(0, 0, 3840, 2160),
            2.0,
        );
        // The visible card is capped at 900x650 DIP, minus its 4 DIP inset.
        assert_eq!((bounds.width(), bounds.height()), (1728, 1300));
        aspect(bounds, 4.0 / 3.0, 2.0);
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
        assert_eq!(
            max + (2.0 * INSET_DIP * 2.0) as u32,
            bounds.width().max(bounds.height()) as u32
        );
    }

    #[test]
    fn a_note_preview_hangs_below_its_card_at_a_fixed_size() {
        let size = (416.0, 272.0);
        let anchor = RectI::new(880, 60, 1040, 218);
        let area = RectI::new(0, 0, 1920, 1040);
        let bounds = fixed_bounds(size, anchor, area, 1.0);
        contained(bounds, area);
        assert_eq!((bounds.width(), bounds.height()), (416, 272));
        assert!(bounds.top >= 230, "preview overlaps the hovered card");
        assert_eq!(bounds.center_x(), anchor.center_x());
    }

    #[test]
    fn a_note_preview_flips_above_or_hides() {
        let size = (416.0, 400.0);
        let area = RectI::new(0, 0, 1280, 720);
        // No room below the card: hang it above instead.
        let anchor = RectI::new(500, 500, 650, 640);
        let bounds = fixed_bounds(size, anchor, area, 1.0);
        contained(bounds, area);
        assert!(bounds.bottom <= 488);
        // Neither side has room: no preview at all.
        let anchor = RectI::new(500, 350, 650, 370);
        assert!(fixed_bounds(size, anchor, area, 1.0).is_empty());
        // A card near the left edge still keeps the work-area margin.
        let bounds = fixed_bounds((416.0, 272.0), RectI::new(-200, 50, -100, 200), area, 1.0);
        assert!(bounds.left >= 12);
    }

    #[test]
    fn a_note_preview_scales_with_the_monitor() {
        let area = RectI::new(0, 0, 3840, 2160);
        let anchor = RectI::new(1800, 100, 2100, 300);
        let bounds = fixed_bounds((416.0, 272.0), anchor, area, 2.0);
        contained(bounds, area);
        assert_eq!((bounds.width(), bounds.height()), (832, 544));
        assert!(bounds.top >= 324);
    }

    #[test]
    fn invalid_fixed_previews_are_suppressed() {
        let area = RectI::new(0, 0, 1280, 720);
        let anchor = RectI::new(100, 50, 200, 150);
        assert!(fixed_bounds((0.0, 10.0), anchor, area, 1.0).is_empty());
        assert!(fixed_bounds((10.0, f32::NAN), anchor, area, 1.0).is_empty());
        assert!(fixed_bounds((10.0, 10.0), RectI::default(), area, 1.0).is_empty());
        assert!(fixed_bounds((10.0, 10.0), anchor, RectI::default(), 1.0).is_empty());
        assert!(fixed_bounds((10.0, 10.0), anchor, area, f32::NAN).is_empty());
        assert!(fixed_bounds((10.0, 10.0), anchor, area, 0.0).is_empty());
    }

    #[test]
    fn the_shadow_room_is_added_around_the_visible_card() {
        let card = RectI::new(100, 200, 300, 400);
        assert_eq!(
            window_bounds(card, 30.0, 1.0),
            RectI::new(70, 170, 330, 430)
        );
        assert_eq!(
            window_bounds(card, 30.0, 2.0),
            RectI::new(40, 140, 360, 460)
        );
        assert!(window_bounds(RectI::default(), 30.0, 1.0).is_empty());
        assert!(window_bounds(card, f32::NAN, 1.0).is_empty());
        assert!(window_bounds(card, 30.0, 0.0).is_empty());
    }
}
