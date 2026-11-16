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
    (longest.ceil() as u32).max(1)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contained(name: &str, bounds: RectI, area: RectI) {
        assert!(!bounds.is_empty(), "{name}: preview has no usable area");
        assert!(
            bounds.left >= area.left
                && bounds.top >= area.top
                && bounds.right <= area.right
                && bounds.bottom <= area.bottom,
            "{name}: preview escaped {area:?}: {bounds:?}"
        );
    }

    fn aspect(name: &str, bounds: RectI, expected: f64, scale: f64) {
        let inset = 2.0 * INSET_DIP as f64 * scale;
        let ratio = (bounds.width() as f64 - inset) / (bounds.height() as f64 - inset);
        assert!(
            (ratio - expected).abs() <= 0.015f64.max(expected * 0.015),
            "{name}: preview changed the image aspect ratio: {ratio} vs {expected}"
        );
    }

    /// One row per placement rule the preview must follow.
    #[test]
    fn preview_bounds_stay_in_the_work_area_and_keep_the_source_aspect() {
        struct Case {
            name: &'static str,
            source: (u32, u32),
            anchor: RectI,
            area: RectI,
            scale: f32,
            /// The preview must not leave this rect: the work area plus the
            /// clearance the hovered card needs on that side.
            inside: RectI,
            /// Allowed preview size in physical pixels: min width, min height,
            /// max width, max height.
            size: (i32, i32, i32, i32),
            aspect: Option<f64>,
        }
        let cases = [
            Case {
                name: "wide preview stays below its card",
                source: (4000, 2000),
                anchor: RectI::new(880, 60, 1040, 160),
                area: RectI::new(0, 0, 1920, 1040),
                scale: 1.0,
                inside: RectI::new(0, 172, 1920, 1040),
                size: (0, 0, 900, 650),
                aspect: Some(2.0),
            },
            Case {
                name: "negative monitor origins remain in physical pixels",
                source: (3200, 1800),
                anchor: RectI::new(-1910, -140, -1760, -30),
                area: RectI::new(-1920, -200, 0, 880),
                scale: 1.5,
                // Staying right of x=0 proves it did not jump to the primary
                // monitor.
                inside: RectI::new(-1920, -12, 0, 880),
                size: (0, 0, i32::MAX, i32::MAX),
                aspect: Some(16.0 / 9.0),
            },
            Case {
                name: "tall images fit the space below the rope",
                source: (1000, 6000),
                anchor: RectI::new(500, 40, 650, 150),
                area: RectI::new(0, 0, 1280, 720),
                scale: 1.0,
                inside: RectI::new(0, 162, 1280, 708),
                size: (0, 0, i32::MAX, i32::MAX),
                aspect: Some(1.0 / 6.0),
            },
            Case {
                name: "high dpi previews use the target monitor scale",
                source: (10000, 5000),
                anchor: RectI::new(1900, 100, 2200, 300),
                area: RectI::new(0, 0, 3840, 2160),
                scale: 2.0,
                inside: RectI::new(0, 324, 3840, 2160),
                size: (901, 0, 1800, 1300),
                aspect: Some(2.0),
            },
            Case {
                name: "preview uses space above when no space remains below",
                source: (1600, 1000),
                anchor: RectI::new(500, 680, 650, 715),
                area: RectI::new(0, 0, 1280, 720),
                scale: 1.0,
                inside: RectI::new(0, 0, 1280, 668),
                size: (0, 0, i32::MAX, i32::MAX),
                aspect: None,
            },
            Case {
                name: "small source images are not enlarged into blurry previews",
                source: (40, 20),
                anchor: RectI::new(300, 30, 400, 110),
                area: RectI::new(0, 0, 1280, 720),
                scale: 1.0,
                inside: RectI::new(0, 0, 1280, 720),
                size: (0, 0, 48, 28),
                aspect: Some(2.0),
            },
            Case {
                name: "a capped preview keeps the source aspect inside its card",
                source: (4000, 3000),
                anchor: RectI::new(1800, 100, 2100, 300),
                area: RectI::new(0, 0, 3840, 2160),
                scale: 2.0,
                inside: RectI::new(0, 0, 3840, 2160),
                size: (1728, 1300, 1728, 1300),
                aspect: Some(4.0 / 3.0),
            },
        ];
        for case in cases {
            let bounds = bounds(case.source, case.anchor, case.area, case.scale);
            contained(case.name, bounds, case.inside);
            let (width, height) = (bounds.width(), bounds.height());
            assert!(
                width >= case.size.0 && height >= case.size.1,
                "{}: preview too small: {width}x{height}",
                case.name
            );
            assert!(
                width <= case.size.2 && height <= case.size.3,
                "{}: preview too large: {width}x{height}",
                case.name
            );
            if let Some(expected) = case.aspect {
                aspect(case.name, bounds, expected, case.scale as f64);
            }
        }
    }

    #[test]
    fn hover_waits_for_the_dwell_then_a_card_switch_restarts_it() {
        let now = 1_000.0;
        // A fresh hover asks for one decode only after the dwell.
        let mut state = HoverState::default();
        assert!(!state.observe(Some(7), 1, 1, now));
        assert!(!state.observe(Some(7), 1, 1, now + 0.349));
        assert!(state.observe(Some(7), 1, 1, now + 0.350));
        assert!(!state.observe(Some(7), 1, 1, now + 0.700));
        assert!(state.is_current(state.version()));

        // Switching cards cancels the previous request and restarts the dwell.
        let mut state = HoverState::default();
        state.observe(Some(1), 1, 1, now);
        state.observe(Some(1), 1, 1, now + 0.350);
        let stale = state.version();
        assert!(!state.observe(Some(2), 1, 1, now + 0.360));
        assert!(!state.is_current(stale));
        assert!(!state.observe(Some(2), 1, 1, now + 0.709));
        assert!(state.observe(Some(2), 1, 1, now + 0.710));
    }

    #[test]
    fn leaving_cancels_work_and_an_edit_replaces_the_pending_revision() {
        let now = 1_000.0;
        // Leaving cancels in-flight work and requires a fresh hover.
        let mut state = HoverState::default();
        state.observe(Some(1), 1, 1, now);
        state.observe(Some(1), 1, 1, now + 0.350);
        let stale = state.version();
        state.reset();
        assert!(!state.is_current(stale));
        assert!(!state.observe(Some(1), 1, 1, now + 0.500));
        assert!(!state.observe(Some(1), 1, 1, now + 0.849));
        assert!(state.observe(Some(1), 1, 1, now + 0.850));

        // An edit while hovered drops the pending decode of the old pixels.
        let mut state = HoverState::default();
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
    fn a_note_preview_hangs_at_a_fixed_size_and_flips_when_it_must() {
        struct Case {
            name: &'static str,
            size: (f32, f32),
            anchor: RectI,
            area: RectI,
            scale: f32,
            /// The preview must not leave this rect; `None` means the case
            /// expects no preview at all.
            inside: Option<RectI>,
            /// Exact physical size, when the case pins it.
            exact: Option<(i32, i32)>,
            /// The preview is horizontally centred on the card.
            centered: bool,
        }
        let cases = [
            Case {
                name: "note preview hangs below its card at a fixed size",
                size: (416.0, 272.0),
                anchor: RectI::new(880, 60, 1040, 218),
                area: RectI::new(0, 0, 1920, 1040),
                scale: 1.0,
                inside: Some(RectI::new(0, 230, 1920, 1040)),
                exact: Some((416, 272)),
                centered: true,
            },
            Case {
                name: "note preview flips above when the space below is gone",
                size: (416.0, 400.0),
                anchor: RectI::new(500, 500, 650, 640),
                area: RectI::new(0, 0, 1280, 720),
                scale: 1.0,
                inside: Some(RectI::new(0, 0, 1280, 488)),
                exact: None,
                centered: false,
            },
            Case {
                name: "note preview hides when neither side has room",
                size: (416.0, 400.0),
                anchor: RectI::new(500, 350, 650, 370),
                area: RectI::new(0, 0, 1280, 720),
                scale: 1.0,
                inside: None,
                exact: None,
                centered: false,
            },
            Case {
                name: "note preview keeps the work-area margin at the screen edge",
                size: (416.0, 272.0),
                anchor: RectI::new(-200, 50, -100, 200),
                area: RectI::new(0, 0, 1280, 720),
                scale: 1.0,
                inside: Some(RectI::new(12, 0, 1280, 720)),
                exact: None,
                centered: false,
            },
            Case {
                name: "note preview scales with the monitor",
                size: (416.0, 272.0),
                anchor: RectI::new(1800, 100, 2100, 300),
                area: RectI::new(0, 0, 3840, 2160),
                scale: 2.0,
                inside: Some(RectI::new(0, 324, 3840, 2160)),
                exact: Some((832, 544)),
                centered: false,
            },
        ];
        for case in cases {
            let bounds = fixed_bounds(case.size, case.anchor, case.area, case.scale);
            let Some(inside) = case.inside else {
                assert!(
                    bounds.is_empty(),
                    "{}: expected no preview, got {bounds:?}",
                    case.name
                );
                continue;
            };
            contained(case.name, bounds, inside);
            if let Some(expected) = case.exact {
                assert_eq!((bounds.width(), bounds.height()), expected, "{}", case.name);
            }
            if case.centered {
                assert_eq!(
                    bounds.center_x(),
                    case.anchor.center_x(),
                    "{}: preview is not centred on its card",
                    case.name
                );
            }
        }
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
    }
}
