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
}

/// One card's hit boxes in window DIP, published by the render thread.
#[derive(Clone, Debug, Default)]
pub struct HitCard {
    pub id: CardId,
    /// The photo frame.
    pub frame: RectF,
    /// The button row below the frame (empty when never hovered).
    pub actions: RectF,
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
        let tolerance = 10.0;
        self.cards
            .iter()
            .find(|card| {
                card.frame.inflated(tolerance).contains(x_dip, y_dip)
                    || (!card.actions.is_empty()
                        && card.actions.inflated(tolerance).contains(x_dip, y_dip))
            })
            .map(|card| card.id)
    }
}
