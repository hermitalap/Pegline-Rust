//! Everything the line's visibility, hover and press state machine needs.
//!
//! None of these fields describe a window or a card: they describe *where the
//! pointer is and what it is doing* — dwell timers, the hover target, the press
//! that may become a drag, the entry note's peek. The frame loop reads and
//! writes them every tick, and so does every mouse handler.

use super::{HoverNote, Press};
use crate::preview::HoverState;
use crate::types::{ActionKind, CardId};

#[derive(Default)]
pub(super) struct LineState {
    pub(super) is_present: bool,
    pub(super) is_revealed: bool,
    /// The user pinned the line down (tray toggle, or a note being edited).
    pub(super) pinned: bool,
    /// A reveal was requested for this moment; the line stays down until then.
    pub(super) peek_until: f64,
    /// When the pointer entered the top hotspot.
    pub(super) hot_zone_since: Option<f64>,
    /// When the pointer left, for the retract delay.
    pub(super) away_since: Option<f64>,
    /// A click on the strip must not immediately re-reveal it.
    pub(super) band_suppressed: bool,
    /// Keep the line down regardless of the pointer (a menu, an editor).
    pub(super) keep_open: bool,
    /// The line was put away on purpose while "Keep line on screen" is on.
    /// The automatic rule leaves it down until a reveal clears this flag.
    pub(super) resident_hidden: bool,
    /// While set, the line stays down after a launch until this deadline.
    pub(super) startup_until: Option<f64>,
    /// When to show the one-time "let Pegline handle your screenshots?" box.
    pub(super) catch_offer_at: Option<f64>,
    /// When the last card left, so an empty line retracts on time.
    pub(super) empty_since: Option<f64>,
    /// When the window should be hidden after a dismiss.
    pub(super) hide_at: Option<f64>,

    pub(super) hover: Option<CardId>,
    /// Which of the hovered card's two buttons the pointer is over.
    pub(super) hover_action: Option<(CardId, ActionKind)>,
    /// The invisible entry hotspot and its line pin.
    pub(super) note_peek_on: bool,
    pub(super) note_hover: Option<HoverNote>,
    pub(super) note_hotspot_hovered: bool,
    pub(super) note_pin: bool,
    pub(super) pressed: Option<Press>,
    /// The card whose clothespin is held. Separate from `pressed` on purpose:
    /// a pin drag must never become an OLE drag, a long press or a click.
    pub(super) pin_press: Option<CardId>,
    /// The clothespin under the pointer; the card itself stays unhovered so
    /// its action pills and the hover preview do not appear.
    pub(super) pin_hover: Option<CardId>,
    /// The grab was released but the scene has not yet published the final
    /// order (or the render thread has not ended the grab). The next tick
    /// applies the order to `line.items` and saves it once.
    pub(super) pending_order: bool,
    /// Last click, for double-click detection.
    pub(super) last_click: Option<(f64, CardId)>,
    /// The hover preview's cross-fade state.
    pub(super) preview_state: HoverState,
    /// True while the OLE drag loop owns the UI thread.
    pub(super) dragging: bool,
    pub(super) last_prune: f64,
}
