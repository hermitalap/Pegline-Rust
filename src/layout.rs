//! Pure layout math for the line, ported number-for-number from `CardView.cs`
//! (`Layout` and `Frame`), `LineWindow.cs` and `App.cs`.
//!
//! Everything here is in DIP and has no Win32 dependency, so it is the part of
//! the app that can be unit tested without a desktop.

use crate::types::RectF;

/// Height of the strip window, DIP.
pub const PANEL_HEIGHT: f32 = 210.0;
/// Height of the rope at both ends, DIP.
pub const ROPE_TOP: f32 = 10.0;
/// Distance between cards, DIP.
pub const SPACING: f32 = 174.0;
/// Width of a card's layout slot, DIP.
pub const CARD_WIDTH: f32 = 150.0;
/// How far the clothespin reaches above the card, DIP.
pub const PIN_ABOVE: f32 = 9.5;
/// Gap between a card and its button row, DIP.
pub const ACTIONS_GAP: f32 = 6.0;
/// Height of the button row, DIP.
pub const ACTIONS_HEIGHT: f32 = 26.0;
/// Height of the note colour swatches, DIP.
pub const SWATCHES_HEIGHT: f32 = 22.0;
/// Gap between the swatches and the buttons, DIP.
pub const ROW_GAP: f32 = 4.0;
/// Diameter of one note colour dot, DIP.
pub const SWATCH_SIZE: f32 = 15.0;
/// Gap between two note colour dots, DIP.
pub const SWATCH_GAP: f32 = 6.0;
/// Horizontal padding inside the swatch pill, DIP.
pub const SWATCH_PADDING: f32 = 6.0;
/// Width of the four-dot pill: 6 + 15×4 + 6×3 + 6.
pub const SWATCH_ROW_WIDTH: f32 = SWATCH_PADDING * 2.0 + SWATCH_SIZE * 4.0 + SWATCH_GAP * 3.0;
/// Card corner radius, DIP.
pub const CARD_RADIUS: f32 = 16.0;
/// Photo inset inside the card, DIP.
pub const PHOTO_INSET: f32 = 4.0;
/// Photo corner radius (concentric with the card), DIP.
pub const PHOTO_RADIUS: f32 = CARD_RADIUS - PHOTO_INSET;
/// Distance from the hanging point down to the card, DIP.
pub const CARD_OFFSET_BELOW_TOP: f32 = 26.0 - 12.0;
/// Slide offset when the line is fully hidden, DIP.
pub const HIDDEN_SLIDE: f32 = -(PANEL_HEIGHT + 12.0);
/// Diameter of the discard cross, DIP.
pub const CROSS_SIZE: f32 = 20.0;
/// Margin of the discard cross inside the card, DIP.
pub const CROSS_MARGIN: f32 = 3.0;
/// The cross is clickable over a slightly larger square, DIP.
pub const CROSS_HIT: f32 = 26.0;
/// Clothespin size, DIP.
pub const PIN_WIDTH: f32 = 9.0;
pub const PIN_HEIGHT: f32 = 26.0;
/// Hit-test tolerance around a card, DIP.
pub const HIT_TOLERANCE: f32 = 10.0;

/// The rope sags by at most 30 DIP, and by 1.8% of the width otherwise.
pub fn sag(width: f32) -> f32 {
    (width * 0.018).min(30.0)
}

/// The rope parabola: `10 + 4 * sag * f * (1 - f)`, `f = x / width`.
pub fn rope_y(x: f32, width: f32) -> f32 {
    if width <= 0.0 {
        return ROPE_TOP;
    }
    let f = x / width;
    ROPE_TOP + 4.0 * sag(width) * f * (1.0 - f)
}

/// Horizontal centre of card `index` when `count` cards are hanging.
pub fn slot_x(index: usize, count: usize, width: f32) -> f32 {
    let total = count.saturating_sub(1) as f32 * SPACING;
    width / 2.0 - total / 2.0 + index as f32 * SPACING
}

/// How many cards fit on a screen of `width` DIP. The WPF app kept at least 3
/// and at most 12 visible no matter how narrow or wide the screen is.
pub fn capacity(width: f32) -> usize {
    let usable = (width - 200.0).max(0.0);
    ((usable / SPACING) as i32).clamp(3, 12) as usize
}

/// Photo size for a source of `w` x `h` pixels: at most 136 x 104 DIP, aspect
/// preserved, and (like the original) small images are scaled up to fit.
pub fn photo_size(w: f32, h: f32) -> (f32, f32) {
    let max_w = CARD_WIDTH - 14.0;
    let max_h = 104.0;
    if w <= 0.0 || h <= 0.0 {
        return (max_w, max_h);
    }
    let scale = (max_w / w).min(max_h / h);
    (w * scale, h * scale)
}

/// Card size: the photo plus the inset on all four sides.
pub fn card_size(w: f32, h: f32) -> (f32, f32) {
    let (pw, ph) = photo_size(w, h);
    (pw + PHOTO_INSET * 2.0, ph + PHOTO_INSET * 2.0)
}

/// How far a hovered card's controls reach below the frame.
pub fn below(note: bool) -> f32 {
    ACTIONS_GAP + if note { SWATCHES_HEIGHT + ROW_GAP } else { 0.0 } + ACTIONS_HEIGHT
}

/// Height of one card's layout slot.
pub fn slot_height(card_height: f32, note: bool) -> f32 {
    CARD_OFFSET_BELOW_TOP + card_height + below(note).max(40.0).max(below(note) + 4.0)
}

/// Top of the card frame in window DIP.
pub fn card_top(x: f32, width: f32, drop: f32) -> f32 {
    rope_y(x, width) - PIN_ABOVE + drop + CARD_OFFSET_BELOW_TOP
}

/// Top of the button row relative to the card's top.
pub fn actions_top(card_height: f32) -> f32 {
    CARD_OFFSET_BELOW_TOP + card_height + ACTIONS_GAP
}

/// Top of the button row for a note, which keeps its swatch row above it.
pub fn actions_top_for(card_height: f32, note: bool) -> f32 {
    actions_top(card_height) + if note { SWATCHES_HEIGHT + ROW_GAP } else { 0.0 }
}

/// Top of the colour swatch pill relative to the card's top.
pub fn swatches_top(card_height: f32) -> f32 {
    actions_top(card_height)
}

/// The pill holding the four colour dots, relative to the card's top.
pub fn swatch_pill(card_height: f32) -> RectF {
    RectF::from_xywh(
        (CARD_WIDTH - SWATCH_ROW_WIDTH) * 0.5,
        swatches_top(card_height),
        SWATCH_ROW_WIDTH,
        SWATCHES_HEIGHT,
    )
}

/// One colour dot inside the pill, relative to the card's top.
pub fn swatch_dot(index: usize, card_height: f32) -> RectF {
    let pill = swatch_pill(card_height);
    RectF::from_xywh(
        pill.left + SWATCH_PADDING + index as f32 * (SWATCH_SIZE + SWATCH_GAP),
        pill.top + (SWATCHES_HEIGHT - SWATCH_SIZE) * 0.5,
        SWATCH_SIZE,
        SWATCH_SIZE,
    )
}

/// Top of the "Copied" pill relative to the card's top; the pill is centred in
/// the gap between the card and the buttons.
pub fn copied_top(card_height: f32, pill_height: f32) -> f32 {
    CARD_OFFSET_BELOW_TOP + card_height + 16.0 - pill_height / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rope_hits_the_edges_and_sags_in_the_middle() {
        assert!((rope_y(0.0, 6016.0 / 2.0) - ROPE_TOP).abs() < 1e-4);
        assert!((rope_y(3008.0, 6016.0 / 2.0) - ROPE_TOP).abs() < 1e-4);
        let width = 1920.0;
        let middle = rope_y(width / 2.0, width);
        assert!((middle - (ROPE_TOP + sag(width))).abs() < 1e-4);
        assert!((middle - 40.0).abs() < 0.1);
    }

    #[test]
    fn sag_is_capped_at_thirty() {
        assert_eq!(sag(6016.0), 30.0);
        assert!((sag(1000.0) - 18.0).abs() < 1e-4);
    }

    #[test]
    fn slots_are_centred_and_one_spacing_apart() {
        let width = 1920.0;
        assert!((slot_x(0, 1, width) - 960.0).abs() < 1e-4);
        assert!((slot_x(0, 3, width) - 786.0).abs() < 1e-4);
        assert!((slot_x(2, 3, width) - 1134.0).abs() < 1e-4);
        assert!((slot_x(1, 3, width) - slot_x(0, 3, width) - SPACING).abs() < 1e-4);
    }

    #[test]
    fn capacity_matches_the_spec() {
        assert_eq!(capacity(1920.0), 9);
        assert_eq!(capacity(6016.0), 12);
        assert_eq!(capacity(400.0), 3);
        assert_eq!(capacity(100.0), 3);
    }

    #[test]
    fn photo_keeps_aspect_and_fits_the_box() {
        let (w, h) = photo_size(1920.0, 1080.0);
        assert!((w / h - 1920.0 / 1080.0).abs() < 1e-3);
        assert!(w <= 136.0 && h <= 104.0);

        let (w, h) = photo_size(1080.0, 1920.0);
        assert!((w / h - 1080.0 / 1920.0).abs() < 1e-3);
        assert!(w <= 136.0 && h <= 104.0);

        assert_eq!(photo_size(0.0, 0.0), (136.0, 104.0));
        assert_eq!(photo_size(-4.0, 20.0), (136.0, 104.0));
    }

    #[test]
    fn card_adds_the_inset_on_every_side() {
        let (w, h) = card_size(1360.0, 1040.0);
        assert!((w - 144.0).abs() < 1e-4);
        assert!((h - 112.0).abs() < 1e-4);
        assert_eq!(card_size(0.0, 0.0), (144.0, 112.0));
    }

    #[test]
    fn card_top_follows_the_rope() {
        let width = 1920.0;
        let top = card_top(width / 2.0, width, 0.0);
        assert!(
            (top - (rope_y(width / 2.0, width) - PIN_ABOVE + CARD_OFFSET_BELOW_TOP)).abs() < 1e-4
        );
        assert!((card_top(width / 2.0, width, -46.0) - (top - 46.0)).abs() < 1e-4);
    }

    #[test]
    fn actions_sit_below_the_frame() {
        assert_eq!(actions_top(112.0), 14.0 + 112.0 + 6.0);
        assert_eq!(below(false), 32.0);
        assert_eq!(below(true), 58.0);
        assert!(slot_height(112.0, false) > actions_top(112.0) + ACTIONS_HEIGHT);
    }
}
