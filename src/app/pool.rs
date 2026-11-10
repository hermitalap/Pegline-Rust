//! The pooled windows, and the Win32 resources that belong to them.
//!
//! Every helper window is created on demand and pooled afterwards: the flight
//! window, the hover preview, the drag ghost and the two editors are expensive
//! (each owns a swap chain and a render thread), so they are hidden rather than
//! destroyed, and only released after an idle period.
//!
//! Grouping them here is not cosmetic. Code that does
//! `if let Some(flight) = self.windows.flight.as_mut() { ... }` keeps a mutable
//! borrow of `self.windows` only, so `self.items`, `self.book` and the rest stay
//! usable in the same block. With all of it flattened into `App`, that same
//! block had to finish before any other field could be touched — which is what
//! forced the clone-and-reindex workarounds elsewhere.

use std::sync::mpsc::Sender;

use windows::Win32::UI::WindowsAndMessaging::HHOOK;

use super::Event;
use crate::editor::Editor;
use crate::logging;
use crate::note_editor::window::NoteEditor;
use crate::tray::Tray;
use crate::window;
use crate::window::flight::Flight;
use crate::window::ghost::Ghost;
use crate::window::overlay::Overlay;
use crate::window::preview::Preview;

/// Every window and window-adjacent handle the UI thread owns.
#[derive(Default)]
pub(super) struct WindowPool {
    pub(super) tray: Option<Tray>,
    /// The line strip. Created once at startup and kept for the process.
    pub(super) overlay: Option<Overlay>,
    /// The flight window: one animation at a time, pooled between captures.
    pub(super) flight: Option<Flight>,
    /// The hover preview; created on first use and pooled afterwards.
    pub(super) preview: Option<Preview>,
    /// The drag preview; created on the first drag and hidden between drags.
    pub(super) ghost: Option<Ghost>,
    /// The annotation editor, while one is open.
    pub(super) editor: Option<Editor>,
    /// The WYSIWYG note editor, while one is open.
    pub(super) note_editor: Option<NoteEditor>,
    /// The low-level mouse hook.
    pub(super) hook: Option<HHOOK>,

    /// When the pooled flight window last went idle; after a while it is
    /// destroyed to give its device memory back.
    pub(super) flight_idle_at: Option<f64>,
    /// The same for the preview and the drag ghost, which are cheaper to
    /// rebuild.
    pub(super) preview_idle_at: Option<f64>,
    pub(super) ghost_idle_at: Option<f64>,

    /// A tray or card menu is open: a modal loop owns the UI thread.
    pub(super) menu_open: bool,
}

impl WindowPool {
    /// Creates the flight window on first use. Keeping a second D3D device
    /// alive costs ~34 MB of private memory for an animation that may never
    /// run, so the window is built lazily and then pooled.
    ///
    /// Takes the event sender rather than `&mut App` on purpose: the returned
    /// borrow is on this struct alone, so the caller can keep touching the line,
    /// the flight book and everything else while the flight is in the air.
    pub(super) fn ensure_flight(&mut self, events: &Sender<Event>) -> Option<&mut Flight> {
        if self.flight.is_none() {
            let flight_tx = events.clone();
            match Flight::create(Box::new(move |event| {
                let _ = flight_tx.send(Event::Flight(event));
                window::wake();
            })) {
                Ok(flight) => {
                    self.flight = Some(flight);
                    logging::trace("flight window created");
                }
                Err(error) => logging::error(format!("flight window: {error}")),
            }
        }
        self.flight.as_mut()
    }
}
