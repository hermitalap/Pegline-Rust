//! Pure layout for the annotation editor: one enlarged hanging picture with
//! its toolbar floating in pills around the card.
//!
//! Everything here is DIP arithmetic with no Win32 dependency, so the window
//! size, the card, the photo and every button rectangle can be unit tested
//! without a desktop. The render thread and the hit-testing both use
//! [`fit`], so what is drawn and what is clickable cannot drift apart.

use crate::types::RectF;

/// Window padding around the outermost floating pill. It is also the room the
/// card's baked shadow (24 DIP of blur) needs before the window edge cuts it.
pub const EDGE: f32 = 26.0;
/// Height of a toolbar pill and its fully rounded ends.
pub const PILL_HEIGHT: f32 = 44.0;
pub const PILL_RADIUS: f32 = PILL_HEIGHT * 0.5;
/// Padding inside a pill, and the gap between two pills.
pub const PILL_PAD: f32 = 6.0;
pub const PILL_GAP: f32 = 10.0;

/// Tool buttons: 34x34 circles inside the tools pill.
pub const TOOL_SIZE: f32 = 34.0;
pub const TOOL_GAP: f32 = 2.0;
/// Arrow, rectangle, text, eraser, mosaic.
pub const TOOL_COUNT: usize = 5;
/// Colour swatches: 22x22.
pub const COLOUR_SIZE: f32 = 22.0;
pub const COLOUR_GAP: f32 = 6.0;
pub const COLOUR_PAD: f32 = 7.0;
pub const COLOUR_COUNT: usize = 6;
/// Line-width buttons: circles like the colour dots.
pub const WIDTH_SIZE: f32 = 34.0;
pub const WIDTH_GAP: f32 = 2.0;
pub const WIDTH_COUNT: usize = 3;

/// Action buttons inside the bottom pills.
pub const ACTION_HEIGHT: f32 = 34.0;
pub const ACTION_PAD: f32 = 5.0;
pub const ACTION_GAP: f32 = 2.0;
pub const UNDO_WIDTH: f32 = 86.0;
pub const REDO_WIDTH: f32 = 86.0;
pub const CANCEL_WIDTH: f32 = 88.0;
pub const SAVE_WIDTH: f32 = 88.0;

/// Gap between the card and the pills above and below it. It leaves room for
/// the clothespin poking out of the card's top edge (14 DIP) with a little air.
pub const ROW_GAP: f32 = 18.0;
/// The glass frame around the photo matches a hanging picture's card exactly,
/// so the flight's card and this one hand over without a jump in the chrome.
pub const CARD_INSET: f32 = crate::layout::PHOTO_INSET;
pub const CARD_RADIUS: f32 = crate::layout::CARD_RADIUS;
/// Clothespin, drawn above the card's top edge like the line's.
pub const PIN_WIDTH: f32 = crate::layout::PIN_WIDTH;
pub const PIN_HEIGHT: f32 = crate::layout::PIN_HEIGHT;
pub const PIN_ABOVE: f32 = crate::layout::CARD_OFFSET_BELOW_TOP;

/// Fraction of the work area an editor may use.
pub const WORK_FRACTION: f32 = 0.88;
/// Largest card, DIP. A 6K screenshot would otherwise ask for a 5200 px wide
/// swap chain at 200 %; 1400 DIP is still nine times the card on the line.
pub const MAX_CARD_WIDTH_DIP: f32 = 1400.0;
pub const MAX_CARD_HEIGHT_DIP: f32 = 1000.0;
/// Smallest photo side, DIP; tiny screenshots still get a readable canvas.
pub const MIN_PHOTO: f32 = 140.0;
/// Longest side of the preview rendered for the flight back to the line.
pub const PREVIEW_MAX: u32 = 720;
/// Longest side of the larger copy the flight back shows at the editor card's
/// size. The card-sized preview is sharp on the line but blurry while it is
/// still the enlarged editor card.
pub const FLIGHT_PREVIEW_MAX: u32 = 2048;

/// One toolbar hit target. The order of the variants is the order the buttons
/// are laid out in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hit {
    Tool(usize),
    Colour(usize),
    Width(usize),
    Undo,
    Redo,
    Cancel,
    Save,
}

/// Everything the editor draws and hit-tests, in DIP.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    /// The glass card (photo plus [`CARD_INSET`] on every side).
    pub card: RectF,
    /// The photo itself; the canvas transform maps source pixels here.
    pub photo: RectF,
    /// Source pixels to DIP.
    pub scale: f32,
    pub pin: RectF,
    pub tools: [RectF; TOOL_COUNT],
    pub colours: [RectF; COLOUR_COUNT],
    pub widths: [RectF; WIDTH_COUNT],
    pub undo: RectF,
    pub redo: RectF,
    pub cancel: RectF,
    pub save: RectF,
    /// The three pills above the card and the two below it, for chrome and for
    /// swallowing clicks that land between their buttons.
    pub top_pills: [RectF; 3],
    pub bottom_pills: [RectF; 2],
}

impl Layout {
    /// The button under a DIP point, if any.
    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        for (index, rect) in self.tools.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Hit::Tool(index));
            }
        }
        for (index, rect) in self.colours.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Hit::Colour(index));
            }
        }
        for (index, rect) in self.widths.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Hit::Width(index));
            }
        }
        for (rect, hit) in [
            (self.undo, Hit::Undo),
            (self.redo, Hit::Redo),
            (self.cancel, Hit::Cancel),
            (self.save, Hit::Save),
        ] {
            if rect.contains(x, y) {
                return Some(hit);
            }
        }
        None
    }

    /// Whether a point is on one of the floating pills (button or not). A
    /// click there never ends the edit, even between two buttons.
    pub fn on_pill(&self, x: f32, y: f32) -> bool {
        self.top_pills
            .iter()
            .chain(self.bottom_pills.iter())
            .any(|pill| pill.contains(x, y))
    }

    /// Whether a point may start a drawing gesture: only the photo counts, so
    /// a click on the glass frame does not mark the picture's border.
    pub fn on_photo(&self, x: f32, y: f32) -> bool {
        self.photo.contains(x, y)
    }

    /// Whether a point is anywhere on the card, including its glass frame.
    pub fn on_card(&self, x: f32, y: f32) -> bool {
        self.card.contains(x, y)
    }
}

/// Width of one pill holding `count` elements of `size`.
fn row_width(count: usize, size: f32, gap: f32, pad: f32) -> f32 {
    pad * 2.0 + size * count as f32 + gap * (count.saturating_sub(1)) as f32
}

/// Total width of the three pills above the card.
pub fn top_row_width() -> f32 {
    row_width(TOOL_COUNT, TOOL_SIZE, TOOL_GAP, PILL_PAD)
        + row_width(COLOUR_COUNT, COLOUR_SIZE, COLOUR_GAP, COLOUR_PAD)
        + row_width(WIDTH_COUNT, WIDTH_SIZE, WIDTH_GAP, PILL_PAD)
        + PILL_GAP * 2.0
}

/// Total width of the two pills below the card.
pub fn bottom_row_width() -> f32 {
    row_width(2, 0.0, ACTION_GAP, ACTION_PAD)
        + UNDO_WIDTH
        + REDO_WIDTH
        + PILL_GAP
        + row_width(2, 0.0, ACTION_GAP, ACTION_PAD)
        + CANCEL_WIDTH
        + SAVE_WIDTH
}

/// The window size for `source` on a work area of `work` DIP.
///
/// The card is as large as the work area allows (never upscaled beyond 1:1),
/// and the window is exactly the card plus the floating rows and the edge.
pub fn window_size(source: (u32, u32), work: (f32, f32)) -> (f32, f32) {
    let max_width = (work.0 * WORK_FRACTION).max(320.0);
    let max_height = (work.1 * WORK_FRACTION).max(240.0);
    let card_max_w =
        (max_width - EDGE * 2.0).clamp(MIN_PHOTO + CARD_INSET * 2.0, MAX_CARD_WIDTH_DIP);
    let card_max_h = (max_height - EDGE * 2.0 - PILL_HEIGHT * 2.0 - ROW_GAP * 2.0)
        .clamp(MIN_PHOTO + CARD_INSET * 2.0, MAX_CARD_HEIGHT_DIP);
    let (photo_w, photo_h) = fitted_photo(
        source,
        card_max_w - CARD_INSET * 2.0,
        card_max_h - CARD_INSET * 2.0,
    );
    let card_w = photo_w + CARD_INSET * 2.0;
    let card_h = photo_h + CARD_INSET * 2.0;
    let width = card_w.max(top_row_width()).max(bottom_row_width()) + EDGE * 2.0;
    let height = EDGE * 2.0 + PILL_HEIGHT * 2.0 + ROW_GAP * 2.0 + card_h;
    (width, height)
}

/// Places the card and every pill in a window of `width` x `height` DIP.
pub fn fit(width: f32, height: f32, source: (u32, u32)) -> Layout {
    let width = width.max(1.0);
    let height = height.max(1.0);
    let available_w =
        (width - EDGE * 2.0 - CARD_INSET * 2.0).clamp(1.0, MAX_CARD_WIDTH_DIP - CARD_INSET * 2.0);
    let available_h = (height - EDGE * 2.0 - PILL_HEIGHT * 2.0 - ROW_GAP * 2.0 - CARD_INSET * 2.0)
        .clamp(1.0, MAX_CARD_HEIGHT_DIP - CARD_INSET * 2.0);
    let (photo_w, photo_h) = fitted_photo(source, available_w, available_h);
    let scale = if source.0 > 0 && source.1 > 0 {
        photo_w / source.0 as f32
    } else {
        1.0
    };
    let card_w = photo_w + CARD_INSET * 2.0;
    let card_h = photo_h + CARD_INSET * 2.0;
    let card = RectF::from_xywh(
        (width - card_w) * 0.5,
        EDGE + PILL_HEIGHT + ROW_GAP,
        card_w,
        card_h,
    );
    let photo = RectF::from_xywh(
        card.left + CARD_INSET,
        card.top + CARD_INSET,
        photo_w,
        photo_h,
    );
    let pin = RectF::from_xywh(
        card.center().0 - PIN_WIDTH * 0.5,
        card.top - PIN_ABOVE,
        PIN_WIDTH,
        PIN_HEIGHT,
    );

    let top = EDGE;
    let top_left = (width - top_row_width()) * 0.5;
    let tools_pill = RectF::from_xywh(
        top_left,
        top,
        row_width(TOOL_COUNT, TOOL_SIZE, TOOL_GAP, PILL_PAD),
        PILL_HEIGHT,
    );
    let colours_pill = RectF::from_xywh(
        tools_pill.right + PILL_GAP,
        top,
        row_width(COLOUR_COUNT, COLOUR_SIZE, COLOUR_GAP, COLOUR_PAD),
        PILL_HEIGHT,
    );
    let widths_pill = RectF::from_xywh(
        colours_pill.right + PILL_GAP,
        top,
        row_width(WIDTH_COUNT, WIDTH_SIZE, WIDTH_GAP, PILL_PAD),
        PILL_HEIGHT,
    );
    let tools = row_buttons::<TOOL_COUNT>(tools_pill, TOOL_SIZE, TOOL_GAP, PILL_PAD);
    let colours = row_buttons::<COLOUR_COUNT>(colours_pill, COLOUR_SIZE, COLOUR_GAP, COLOUR_PAD);
    let widths = row_buttons::<WIDTH_COUNT>(widths_pill, WIDTH_SIZE, WIDTH_GAP, PILL_PAD);

    let bottom = card.bottom + ROW_GAP;
    let bottom_left = (width - bottom_row_width()) * 0.5;
    let undo_pill = RectF::from_xywh(
        bottom_left,
        bottom,
        row_width(2, 0.0, ACTION_GAP, ACTION_PAD) + UNDO_WIDTH + REDO_WIDTH,
        PILL_HEIGHT,
    );
    let actions_pill = RectF::from_xywh(
        undo_pill.right + PILL_GAP,
        bottom,
        row_width(2, 0.0, ACTION_GAP, ACTION_PAD) + CANCEL_WIDTH + SAVE_WIDTH,
        PILL_HEIGHT,
    );
    let actions = action_buttons(undo_pill, &[UNDO_WIDTH, REDO_WIDTH]);
    let cancel_save = action_buttons(actions_pill, &[CANCEL_WIDTH, SAVE_WIDTH]);
    let undo = actions[0];
    let redo = actions[1];
    let cancel = cancel_save[0];
    let save = cancel_save[1];

    Layout {
        width,
        height,
        card,
        photo,
        scale,
        pin,
        tools,
        colours,
        widths,
        undo,
        redo,
        cancel,
        save,
        top_pills: [tools_pill, colours_pill, widths_pill],
        bottom_pills: [undo_pill, actions_pill],
    }
}

/// The photo size for `source` inside `max_w` x `max_h`, never upscaled.
fn fitted_photo(source: (u32, u32), max_w: f32, max_h: f32) -> (f32, f32) {
    let source_w = source.0.max(1) as f32;
    let source_h = source.1.max(1) as f32;
    let scale = (max_w / source_w).min(max_h / source_h).clamp(0.0001, 1.0);
    (source_w * scale, source_h * scale)
}

/// A row of `N` square buttons centred in `pill`.
fn row_buttons<const N: usize>(pill: RectF, size: f32, gap: f32, pad: f32) -> [RectF; N] {
    let mut rects = [RectF::default(); N];
    let mut x = pill.left + pad;
    let y = pill.top + (pill.height() - size) * 0.5;
    for rect in rects.iter_mut() {
        *rect = RectF::from_xywh(x, y, size, size);
        x += size + gap;
    }
    rects
}

/// A row of `widths.len()` action buttons centred in `pill`.
fn action_buttons(pill: RectF, widths: &[f32; 2]) -> [RectF; 2] {
    let y = pill.top + (pill.height() - ACTION_HEIGHT) * 0.5;
    let first = RectF::from_xywh(pill.left + ACTION_PAD, y, widths[0], ACTION_HEIGHT);
    let second = RectF::from_xywh(first.right + ACTION_GAP, y, widths[1], ACTION_HEIGHT);
    [first, second]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every common source shape, at every common screen size.
    const SOURCES: [(u32, u32); 6] = [
        (900, 600),
        (1920, 1080),
        (1080, 1920),
        (6000, 3400),
        (320, 200),
        (200, 320),
    ];
    const WORKS: [(f32, f32); 4] = [
        (1920.0, 1040.0),
        (1470.0, 800.0),
        (3008.0, 1644.0),
        (1120.0, 700.0),
    ];

    #[test]
    fn the_window_fits_the_work_area() {
        for work in WORKS {
            for source in SOURCES {
                let (width, height) = window_size(source, work);
                assert!(width <= work.0, "{source:?} is {width} wide on {work:?}");
                assert!(height <= work.1, "{source:?} is {height} high on {work:?}");
                assert!(width >= top_row_width() + EDGE * 2.0 - 0.01);
                assert!(width >= bottom_row_width() + EDGE * 2.0 - 0.01);
            }
        }
    }

    #[test]
    fn the_card_keeps_the_source_aspect_and_sits_between_the_rows() {
        for work in WORKS {
            for source in SOURCES {
                let (width, height) = window_size(source, work);
                let layout = fit(width, height, source);
                let aspect = source.0 as f32 / source.1 as f32;
                assert!(
                    (layout.photo.width() / layout.photo.height() - aspect).abs() < 1e-3,
                    "{source:?} aspect drifted"
                );
                assert!(
                    (layout.card.width() - layout.photo.width() - CARD_INSET * 2.0).abs() < 1e-3
                );
                assert!(layout.card.top > layout.top_pills[0].bottom);
                assert!(layout.card.bottom < layout.bottom_pills[0].top);
                // The clothespin pokes into the gap above the card, never into
                // the tools pill.
                assert!(layout.pin.top >= layout.top_pills[0].bottom);
                assert!(layout.photo.left > layout.card.left);
                assert!(layout.photo.top > layout.card.top);
                assert!(layout.photo.right < layout.card.right);
                assert!(layout.photo.bottom < layout.card.bottom);
            }
        }
    }

    #[test]
    fn the_card_is_never_upscaled_and_tiny_pictures_stay_readable() {
        let (width, height) = window_size((320, 200), WORKS[0]);
        let layout = fit(width, height, (320, 200));
        assert!((layout.photo.width() - 320.0).abs() < 0.01);
        assert_eq!(layout.scale, 1.0);
        assert!(layout.photo.width() >= MIN_PHOTO);
    }

    #[test]
    fn a_six_k_picture_gets_a_capped_card() {
        // Without the cap the swap chain alone would be 5200 px wide at 200 %.
        let (width, height) = window_size((6016, 3384), WORKS[2]);
        let layout = fit(width, height, (6016, 3384));
        assert!(layout.card.width() <= MAX_CARD_WIDTH_DIP + 0.01);
        assert!(layout.card.height() <= MAX_CARD_HEIGHT_DIP + 0.01);
        assert!(layout.scale < 0.5);
    }

    #[test]
    fn the_pills_are_centred_and_do_not_overlap_each_other() {
        let (width, height) = window_size((1920, 1080), WORKS[2]);
        let layout = fit(width, height, (1920, 1080));
        let top = layout.top_pills;
        for pair in top.windows(2) {
            assert!(pair[0].right + PILL_GAP - 0.01 <= pair[1].left);
        }
        let bottom = layout.bottom_pills;
        assert!(bottom[0].right + PILL_GAP - 0.01 <= bottom[1].left);
        let left = top[0].left;
        let right = top[2].right;
        assert!(((left + right) * 0.5 - width * 0.5).abs() < 0.01);
        let left = bottom[0].left;
        let right = bottom[1].right;
        assert!(((left + right) * 0.5 - width * 0.5).abs() < 0.01);
        // The bottom row is not wider than the top row on a full-size screen.
        assert!(bottom[1].right - bottom[0].left <= top[2].right - top[0].left);
    }

    #[test]
    fn every_button_is_inside_its_pill_and_returns_its_own_hit() {
        let (width, height) = window_size((900, 600), WORKS[0]);
        let layout = fit(width, height, (900, 600));
        for (index, rect) in layout.tools.iter().enumerate() {
            assert!(layout.top_pills[0].contains(rect.center().0, rect.center().1));
            assert_eq!(
                layout.hit(rect.center().0, rect.center().1),
                Some(Hit::Tool(index))
            );
        }
        for (index, rect) in layout.colours.iter().enumerate() {
            assert!(layout.top_pills[1].contains(rect.center().0, rect.center().1));
            assert_eq!(
                layout.hit(rect.center().0, rect.center().1),
                Some(Hit::Colour(index))
            );
        }
        for (index, rect) in layout.widths.iter().enumerate() {
            assert!(layout.top_pills[2].contains(rect.center().0, rect.center().1));
            assert_eq!(
                layout.hit(rect.center().0, rect.center().1),
                Some(Hit::Width(index))
            );
        }
        for (rect, hit) in [
            (layout.undo, Hit::Undo),
            (layout.redo, Hit::Redo),
            (layout.cancel, Hit::Cancel),
            (layout.save, Hit::Save),
        ] {
            assert_eq!(layout.hit(rect.center().0, rect.center().1), Some(hit));
        }
        // The glass frame is on the card but not on the photo: a click there
        // must not start a mark.
        let frame = (layout.card.center().0, layout.card.top + 1.0);
        assert!(layout.on_card(frame.0, frame.1));
        assert!(!layout.on_photo(frame.0, frame.1));
        assert!(layout.on_pill(
            layout.top_pills[0].center().0,
            layout.top_pills[0].center().1
        ));
        assert!(!layout.on_pill(layout.card.center().0, layout.card.center().1));
    }
}
