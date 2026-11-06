//! Bookkeeping for the flight that is currently in the air.
//!
//! The flight window is a single pooled surface, so at most one card can be
//! flying at a time. This records which card that is and when the app is
//! allowed to stop waiting for the flight to report back: a capture, a menu or
//! a closed editor can take the window over, and without these deadlines the
//! hidden editor or the flying card would be leaked.
//!
//! Kept separate from [`super::pool::WindowPool`] on purpose: the flight
//! callbacks mutate this while holding `&mut` on the window itself, and two
//! disjoint fields are what make that legal without cloning.

use crate::types::CardId;

/// The card in the air, and the deadlines that guard the hand-offs around it.
#[derive(Default)]
pub(super) struct FlightBook {
    /// The card the flight window is carrying, if any. A newer capture or a
    /// drop interrupts the old animation (BEHAVIOR §3.8), so this is cleared as
    /// soon as the flight window moves on.
    pub(super) flying_card: Option<CardId>,
    /// True when `flying_card` is an edited note or picture returning to its
    /// existing slot rather than a new card arriving at the right end.
    pub(super) flying_rehang: bool,
    /// The opening flight did not report its landing yet; show the editor after
    /// this time anyway (the flight may have been interrupted by a capture).
    pub(super) annotate_show_at: Option<f64>,
    /// Safety deadline for the closing flight: release the hidden editor even
    /// if the flight window was taken over by a capture.
    pub(super) annotate_close_at: Option<f64>,
    /// Fallback deadline for showing a hidden note editor if its flight is
    /// interrupted by a screenshot.
    pub(super) note_show_at: Option<f64>,
}
