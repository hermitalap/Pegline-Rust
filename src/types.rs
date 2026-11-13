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
///
/// The boxes are unrotated: they are expressed in the same frame the card is
/// laid out in, before the scene swings it. [`HitCard::angle`] and
/// [`HitCard::pin`] say how it was swung, and the hit tests undo that rotation
/// so a click lands where the card is drawn.
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
    /// The rotation origin of that angle, in window DIP: the clothespin at the
    /// top of the frame. The card is drawn swung about this point, so the hit
    /// tests turn the pointer back around it.
    pub pin: (f32, f32),
    /// The clothespin's grab box in the card's own frame: 20 DIP wide and 34
    /// DIP tall, reaching 4 DIP above the pin and 30 below it. It is tested
    /// after [`HitCard::in_card_frame`], so it swings with the card.
    pub pin_box: RectF,
    /// False while the card is in flight, held by the flight window, or being
    /// dragged; a pin that cannot be grabbed must not swallow a click.
    pub pin_ready: bool,
    /// True while this card is the one the pin drag is moving. The scene
    /// publishes it so the UI can keep the pointer captured and the order
    /// pending.
    pub dragged: bool,
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
    /// Horizontal centre of the note entry's corner, in window DIP, from the
    /// scene's row spring. `None` when the entry does not fit. The UI uses it
    /// so the invisible hotspot glides with the drawn corner while the row
    /// reflows.
    pub note_entry_x: Option<f32>,
    /// Every card id in draw order, including the ones in flight (the scene
    /// publishes all of them here, while `cards` skips the flight cards). The
    /// UI reorders `line.items` from this after a pin drag.
    pub order: Vec<CardId>,
    /// True while a pin drag owns the pointer. The UI keeps the window
    /// click-through off and must not start another gesture.
    pub grabbing: bool,
}

impl HitCard {
    /// The pointer in the card's own frame, where its boxes live.
    ///
    /// The scene draws the card swung by [`HitCard::angle`] about
    /// [`HitCard::pin`], so a hit test has to rotate the pointer back by the
    /// same amount before testing the unrotated rectangles. At rest the angle
    /// is 0 and this is the identity, so an upright card is tested exactly as
    /// before.
    fn in_card_frame(&self, x_dip: f32, y_dip: f32) -> (f32, f32) {
        if self.angle == 0.0 {
            return (x_dip, y_dip);
        }
        let (sin, cos) = self.angle.to_radians().sin_cos();
        let dx = x_dip - self.pin.0;
        let dy = y_dip - self.pin.1;
        (
            self.pin.0 + dx * cos + dy * sin,
            self.pin.1 - dx * sin + dy * cos,
        )
    }
}

impl FrameSnapshot {
    pub fn card_at(&self, x_dip: f32, y_dip: f32) -> Option<CardId> {
        self.card_at_impl(x_dip, y_dip, crate::layout::HIT_TOLERANCE)
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
                // The tolerance is in DIP and stays so: it is applied to the
                // box in the card's own frame, not to the rotated result.
                let (x, y) = card.in_card_frame(x_dip, y_dip);
                if card.pin_ready && card.pin_box.contains(x, y) {
                    return true;
                }
                if card.frame.inflated(tolerance).contains(x, y) {
                    return true;
                }
                if tolerance > 0.0 {
                    // Hovering may span the whole action row, including the gap
                    // between the two pills.
                    if !card.actions.is_empty() && card.actions.inflated(tolerance).contains(x, y) {
                        return true;
                    }
                } else if card.primary_button.contains(x, y) || card.open_button.contains(x, y) {
                    // A click in the 6 DIP gap between the pills is not a
                    // button press and should not fall through to the card
                    // either.
                    return true;
                }
                card.swatches
                    .iter()
                    .any(|swatch| swatch.inflated(tolerance).contains(x, y))
            })
            .map(|card| card.id)
    }

    /// The hover button under the pointer, if any. The published rectangles are
    /// inflated by the same 10 DIP tolerance the frame uses; the per-button
    /// rectangles are not, so a click in the 4 DIP gap between the pills is not
    /// swallowed by either of them.
    pub fn action_at(&self, x_dip: f32, y_dip: f32) -> Option<(CardId, ActionKind)> {
        for card in &self.cards {
            // The pills are drawn inside the card's swing like the rest of it.
            let (x, y) = card.in_card_frame(x_dip, y_dip);
            if card.primary_button.contains(x, y) {
                return Some((card.id, card.primary_action));
            }
            if card.open_button.contains(x, y) {
                return Some((card.id, ActionKind::Open));
            }
        }
        None
    }

    /// The clothespin grab box under the pointer, if any. Pin presses are
    /// checked before the card body, the action pills and the band click: the
    /// pin lies outside the card frame, above the rope, where the band click
    /// would otherwise retract the line.
    pub fn pin_at(&self, x_dip: f32, y_dip: f32) -> Option<CardId> {
        self.cards
            .iter()
            .find(|card| {
                if !card.pin_ready || card.dragged {
                    return false;
                }
                let (x, y) = card.in_card_frame(x_dip, y_dip);
                card.pin_box.contains(x, y)
            })
            .map(|card| card.id)
    }

    /// The note colour dot under the pointer, if any.
    pub fn swatch_at(&self, x_dip: f32, y_dip: f32) -> Option<(CardId, usize)> {
        for card in &self.cards {
            let (x, y) = card.in_card_frame(x_dip, y_dip);
            for (index, swatch) in card.swatches.iter().enumerate() {
                if swatch.contains(x, y) {
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
            // The clothespin sits PIN_ABOVE the frame's top, on the centre.
            pin: (200.0, 36.0),
            pin_box: RectF::new(190.0, 32.0, 210.0, 66.0),
            pin_ready: true,
            dragged: false,
            swatches: [RectF::default(); 4],
        }
    }

    fn snapshot() -> FrameSnapshot {
        FrameSnapshot {
            scale: 2.0,
            slide: 0.0,
            width_dip: 3008.0,
            cards: vec![card(7)],
            note_entry_x: Some(3008.0 * 0.5 + 87.0),
            order: vec![7],
            grabbing: false,
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

    /// A point of the card's own frame, placed where the scene draws it: swung
    /// about the pin by `angle`, the same matrix `Scene::draw_card` sets.
    fn drawn(card: &HitCard, x: f32, y: f32) -> (f32, f32) {
        let rotation = windows_numerics::Matrix3x2::rotation_around(
            card.angle,
            windows_numerics::Vector2 {
                X: card.pin.0,
                Y: card.pin.1,
            },
        );
        (
            x * rotation.M11 + y * rotation.M21 + rotation.M31,
            x * rotation.M12 + y * rotation.M22 + rotation.M32,
        )
    }

    #[test]
    fn a_swinging_card_is_hit_where_it_is_drawn() {
        let tilted = FrameSnapshot {
            cards: vec![HitCard {
                angle: 18.0,
                ..card(7)
            }],
            ..snapshot()
        };
        let card = &tilted.cards[0];

        // The bottom corners of a card swung 18° about its pin sit about 50 DIP
        // away from the upright box. They are on the card the pointer is over,
        // so a click there has to find it.
        for (x, y) in [(103.0, 197.0), (297.0, 197.0)] {
            let (x, y) = drawn(card, x, y);
            assert_eq!(tilted.card_at_exact(x, y), Some(7), "corner at {x}, {y}");
        }

        // The upright box is not the card any more: its old bottom-right corner
        // is well outside the swung frame, even with the hover slop.
        assert_eq!(tilted.card_at(290.0, 190.0), None);
        assert_eq!(tilted.card_at_exact(290.0, 190.0), None);

        // The slop still applies, still 10 DIP, and is measured along the card:
        // 4 DIP left of the swung edge hovers but does not click.
        let (x, y) = drawn(card, 96.0, 150.0);
        assert_eq!(tilted.card_at(x, y), Some(7));
        assert_eq!(tilted.card_at_exact(x, y), None);

        // The hover pills swing with the card, so an "Annotate" that is drawn
        // to the left of the upright row is still a press on that pill.
        let (x, y) = drawn(card, 150.0, 219.0);
        assert_eq!(tilted.action_at(x, y), Some((7, ActionKind::Annotate)));
    }

    #[test]
    fn the_pin_box_turns_with_the_card_and_ignores_unready_pins() {
        let mut snapshot = snapshot();
        assert_eq!(snapshot.pin_at(200.0, 36.0), Some(7));
        assert_eq!(snapshot.pin_at(200.0, 70.0), None);

        // The pin is the rotation origin, so its centre stays hit while the
        // card swings; the box itself turns with the card.
        snapshot.cards[0].angle = 30.0;
        assert_eq!(snapshot.pin_at(200.0, 36.0), Some(7));

        snapshot.cards[0].pin_ready = false;
        assert_eq!(snapshot.pin_at(200.0, 36.0), None);
        snapshot.cards[0].pin_ready = true;
        snapshot.cards[0].dragged = true;
        assert_eq!(snapshot.pin_at(200.0, 36.0), None);
    }
}
