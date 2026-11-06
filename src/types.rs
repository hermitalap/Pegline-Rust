//! Small value types shared by the UI thread, the render thread and the
//! capture workers.
//!
//! Units: every type is explicit about them. `RectF` is in DIP inside a window
//! and in device pixels for anything measured on the desktop; the call sites say
//! which. `RectI` is always device pixels. `ImageData` is always pixels.

/// Identifies one pegged item. Ids are allocated by the UI thread and never
/// reused, so a stale command from a worker can be ignored safely.
pub type CardId = u64;

/// An axis-aligned rectangle. See the module comment for its units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl RectF {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn from_xywh(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self::new(x, y, x + w, y + h)
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }

    pub fn center(&self) -> (f32, f32) {
        (
            (self.left + self.right) * 0.5,
            (self.top + self.bottom) * 0.5,
        )
    }

    pub fn is_empty(&self) -> bool {
        self.width() <= 0.0 || self.height() <= 0.0
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn inflated(&self, by: f32) -> Self {
        Self::new(
            self.left - by,
            self.top - by,
            self.right + by,
            self.bottom + by,
        )
    }

    pub fn union(&self, other: &RectF) -> Self {
        Self::new(
            self.left.min(other.left),
            self.top.min(other.top),
            self.right.max(other.right),
            self.bottom.max(other.bottom),
        )
    }

    pub fn offset(&self, dx: f32, dy: f32) -> Self {
        Self::new(
            self.left + dx,
            self.top + dy,
            self.right + dx,
            self.bottom + dy,
        )
    }

    pub fn scaled(&self, factor: f32) -> Self {
        Self::new(
            self.left * factor,
            self.top * factor,
            self.right * factor,
            self.bottom * factor,
        )
    }

    pub fn round_px(&self) -> RectI {
        RectI {
            left: self.left.round() as i32,
            top: self.top.round() as i32,
            right: self.right.round() as i32,
            bottom: self.bottom.round() as i32,
        }
    }
}

/// A rectangle in device pixels, as the desktop reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RectI {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl RectI {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    pub fn center_x(&self) -> f32 {
        (self.left + self.right) as f32 * 0.5
    }

    pub fn center_y(&self) -> f32 {
        (self.top + self.bottom) as f32 * 0.5
    }

    pub fn to_f32(self) -> RectF {
        RectF::new(
            self.left as f32,
            self.top as f32,
            self.right as f32,
            self.bottom as f32,
        )
    }

    pub fn union(&self, other: &RectI) -> Self {
        Self::new(
            self.left.min(other.left),
            self.top.min(other.top),
            self.right.max(other.right),
            self.bottom.max(other.bottom),
        )
    }

    pub fn inflate(&self, by: i32) -> Self {
        Self::new(
            self.left - by,
            self.top - by,
            self.right + by,
            self.bottom + by,
        )
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// A decoded image, premultiplied BGRA, top-down, stride = `width * 4`.
///
/// WIC produces exactly this layout and Direct2D consumes it directly, so the
/// pixels travel from the decoder to the GPU without an intermediate copy in a
/// different format.
#[derive(Clone)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl ImageData {
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Self {
        Self {
            width,
            height,
            bgra,
        }
    }

    pub fn byte_len(&self) -> usize {
        self.bgra.len()
    }

    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.bgra.len() == self.width as usize * self.height as usize * 4
    }

    /// Longest side, in pixels. Used for the "don't upscale" rules.
    pub fn longest_side(&self) -> u32 {
        self.width.max(self.height)
    }
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageData")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bgra.len())
            .finish()
    }
}

/// What the renderer needs to know about a new card.
#[derive(Clone, Debug)]
pub struct CardSpec {
    pub id: CardId,
    /// Frame size in DIP (photo plus the 4 DIP inset on every side).
    pub card_size: (f32, f32),
    /// Fixed random lean of the card, -2.5..2.5 degrees.
    pub tilt: f32,
    /// True when the card flew in from a capture: it starts at its slot and only
    /// gets the small landing swing. False for cards restored at startup.
    pub arriving: bool,
    /// 480 px thumbnail (or note bitmap). May be filled in later with
    /// `Command::SetImage` when decoding finishes after the card is on the line.
    pub image: Option<ImageData>,
    /// True for a sticky note: the card shows the colour swatches on hover.
    pub note: bool,
}

/// Which of the two hover buttons a point is over.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActionKind {
    /// Copy the note's text; note cards only.
    Copy,
    /// Open the annotation editor for a picture; picture cards only.
    Annotate,
    #[default]
    Open,
}

/// One card's hit boxes in window DIP, published by the render thread.
#[derive(Clone, Debug, Default)]
pub struct HitCard {
    pub id: CardId,
    /// The photo frame.
    pub frame: RectF,
    /// The button row below the frame (empty when never hovered).
    pub actions: RectF,
    /// The left pill inside `actions`: Annotate on a picture, Copy on a note.
    /// Empty when not hovered.
    pub primary_button: RectF,
    /// What a click on `primary_button` does.
    pub primary_action: ActionKind,
    pub open_button: RectF,
    /// True for a sticky note.
    pub note: bool,
    /// The card's current visual angle, degrees, swing plus its fixed tilt.
    /// The annotations flight starts from this angle and settles upright.
    pub angle: f32,
    /// The four colour dots, in colour order; empty unless the note is hovered.
    pub swatches: [RectF; 4],
}

/// A consistent view of the line for the UI thread's hit testing.
#[derive(Clone, Debug, Default)]
pub struct FrameSnapshot {
    /// Device pixels per DIP for the monitor the line is on.
    pub scale: f32,
    /// Current slide offset in DIP; 0 fully down, -(PanelHeight+12) fully up.
    pub slide: f32,
    pub width_dip: f32,
    pub cards: Vec<HitCard>,
}

impl FrameSnapshot {
    pub fn card_at(&self, x_dip: f32, y_dip: f32) -> Option<CardId> {
        self.card_at_impl(x_dip, y_dip, 10.0)
    }

    /// The same lookup without the generous hover margin. Clicks and context
    /// menus use this: the 10 DIP slop exists so the pointer keeps hovering
    /// while it chases a tilted card, not so a click in the empty strip beside
    /// a card copies it.
    pub fn card_at_exact(&self, x_dip: f32, y_dip: f32) -> Option<CardId> {
        self.card_at_impl(x_dip, y_dip, 0.0)
    }

    fn card_at_impl(&self, x_dip: f32, y_dip: f32, tolerance: f32) -> Option<CardId> {
        self.cards
            .iter()
            .find(|card| {
                if card.frame.inflated(tolerance).contains(x_dip, y_dip) {
                    return true;
                }
                if tolerance > 0.0 {
                    // Hovering may span the whole action row, including the gap
                    // between the two pills.
                    if !card.actions.is_empty()
                        && card.actions.inflated(tolerance).contains(x_dip, y_dip)
                    {
                        return true;
                    }
                } else if card.primary_button.contains(x_dip, y_dip)
                    || card.open_button.contains(x_dip, y_dip)
                {
                    // A click in the 6 DIP gap between the pills is not a
                    // button press and should not fall through to the card
                    // either.
                    return true;
                }
                card.swatches
                    .iter()
                    .any(|swatch| swatch.inflated(tolerance).contains(x_dip, y_dip))
            })
            .map(|card| card.id)
    }

    /// The hover button under the pointer, if any. The published rectangles are
    /// inflated by the same 10 DIP tolerance the frame uses; the per-button
    /// rectangles are not, so a click in the 4 DIP gap between the pills is not
    /// swallowed by either of them.
    pub fn action_at(&self, x_dip: f32, y_dip: f32) -> Option<(CardId, ActionKind)> {
        for card in &self.cards {
            if card.primary_button.contains(x_dip, y_dip) {
                return Some((card.id, card.primary_action));
            }
            if card.open_button.contains(x_dip, y_dip) {
                return Some((card.id, ActionKind::Open));
            }
        }
        None
    }

    /// The note colour dot under the pointer, if any.
    pub fn swatch_at(&self, x_dip: f32, y_dip: f32) -> Option<(CardId, usize)> {
        for card in &self.cards {
            for (index, swatch) in card.swatches.iter().enumerate() {
                if swatch.contains(x_dip, y_dip) {
                    return Some((card.id, index));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: CardId) -> HitCard {
        HitCard {
            id,
            frame: RectF::new(100.0, 50.0, 300.0, 200.0),
            actions: RectF::new(120.0, 206.0, 280.0, 232.0),
            primary_button: RectF::new(120.0, 206.0, 180.0, 232.0),
            primary_action: ActionKind::Annotate,
            open_button: RectF::new(186.0, 206.0, 280.0, 232.0),
            note: false,
            angle: 0.0,
            swatches: [RectF::default(); 4],
        }
    }

    fn snapshot() -> FrameSnapshot {
        FrameSnapshot {
            scale: 2.0,
            slide: 0.0,
            width_dip: 3008.0,
            cards: vec![card(7)],
        }
    }

    #[test]
    fn hover_is_generous_but_clicks_are_exact() {
        let snapshot = snapshot();
        // 5 DIP to the left of the frame: hovering still counts.
        assert_eq!(snapshot.card_at(95.0, 120.0), Some(7));
        assert_eq!(snapshot.card_at_exact(95.0, 120.0), None);
        assert_eq!(snapshot.card_at_exact(101.0, 51.0), Some(7));
        assert_eq!(snapshot.card_at_exact(299.0, 199.0), Some(7));
        assert_eq!(snapshot.card_at_exact(300.0, 120.0), None);
    }

    #[test]
    fn hover_buttons_are_exact_and_split_by_the_gap() {
        let snapshot = snapshot();
        assert_eq!(
            snapshot.action_at(150.0, 220.0),
            Some((7, ActionKind::Annotate))
        );
        assert_eq!(
            snapshot.action_at(200.0, 220.0),
            Some((7, ActionKind::Open))
        );
        // The 6 DIP gap between the two pills belongs to neither, and a click
        // there does not fall through to the card either.
        assert_eq!(snapshot.action_at(183.0, 220.0), None);
        assert_eq!(snapshot.card_at_exact(183.0, 220.0), None);
        assert_eq!(snapshot.card_at(183.0, 220.0), Some(7));
    }
}
