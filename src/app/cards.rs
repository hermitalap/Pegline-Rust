//! The line's cards as the UI thread sees them: geometry, order and commands.
//!
//! These are the operations that turn a `line.items` entry into the pixels the
//! user points at: slot positions, hit boxes, capacity, and the Pegline file
//! that remembers the row across restarts.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use super::{Item, pseudo_random};
use crate::layout;
use crate::note_editor::display::PAPER_HEIGHT;
use crate::notes;
use crate::scene::Command;
use crate::settings;
use crate::types::{ActionKind, CardId, CardSpec, ImageData, RectI};
use crate::win32;

impl super::App {
    /// The DPI of the monitor the line is on.
    pub(super) fn screen_dpi(&self) -> Option<u32> {
        self.screen.as_ref().map(|screen| screen.dpi)
    }

    /// Decode size for a card thumbnail: exactly the pixels the card occupies
    /// (136 DIP wide at the current scale), capped by `THUMB_MAX`. Oversampling
    /// to 480 px costs ~11 MB of driver memory for six cards and buys nothing:
    /// the card never draws wider than 272 px on a 200% screen.
    pub(super) fn thumb_max(&self) -> u32 {
        let scale = self
            .screen
            .as_ref()
            .map(|screen| screen.scale())
            .unwrap_or(1.0);
        ((layout::CARD_WIDTH - 14.0) * scale)
            .ceil()
            .clamp(96.0, crate::capture::THUMB_MAX as f32) as u32
    }

    /// Where the flight should land card `index` of `count`, in screen pixels.
    ///
    /// The landing spot is one full spacing beyond the last card of the
    /// current row — half a spacing beyond the final slot — so the new card
    /// lands clear of its neighbour; the row then slides left to centre it
    /// (BEHAVIOR §5.2).
    pub(super) fn flight_target_px(
        &self,
        index: usize,
        count: usize,
        card_size: (f32, f32),
    ) -> Option<RectI> {
        let overlay = self.windows.overlay.as_ref()?;
        let window = win32::window_rect(overlay.hwnd())?;
        let scale = overlay.scale().max(0.1);
        let width_dip = overlay.width_px() as f32 / scale;
        let x = layout::flight_slot_x(index, count, width_dip);
        let top = layout::rope_y(x, width_dip) - layout::PIN_ABOVE + layout::CARD_OFFSET_BELOW_TOP;
        let left = x - card_size.0 * 0.5;
        Some(RectI::new(
            window.left + (left * scale).round() as i32,
            window.top + (top * scale).round() as i32,
            window.left + ((left + card_size.0) * scale).round() as i32,
            window.top + ((top + card_size.1) * scale).round() as i32,
        ))
    }

    /// Where card `index` of `count` will sit once the row settles, in screen
    /// pixels, ignoring any reveal offset. Used as the landing target when an
    /// edited note returns to its existing slot, including when the strip is
    /// still on its way down.
    pub(super) fn card_slot_px(
        &self,
        index: usize,
        count: usize,
        card_size: (f32, f32),
    ) -> Option<RectI> {
        let overlay = self.windows.overlay.as_ref()?;
        let window = win32::window_rect(overlay.hwnd())?;
        let scale = overlay.scale().max(0.1);
        let width_dip = overlay.width_px() as f32 / scale;
        let x = layout::slot_x(index, count, width_dip);
        let top = layout::rope_y(x, width_dip) - layout::PIN_ABOVE + layout::CARD_OFFSET_BELOW_TOP;
        let left = x - card_size.0 * 0.5;
        Some(RectI::new(
            window.left + (left * scale).round() as i32,
            window.top + (top * scale).round() as i32,
            window.left + ((left + card_size.0) * scale).round() as i32,
            window.top + ((top + card_size.1) * scale).round() as i32,
        ))
    }

    /// The card's current rectangle on screen, in pixels.
    pub(super) fn current_card_px(&self, id: CardId) -> Option<RectI> {
        let overlay = self.windows.overlay.as_ref()?;
        let window = win32::window_rect(overlay.hwnd())?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        let card = snapshot.cards.iter().find(|card| card.id == id)?;
        let scale = snapshot.scale.max(0.1);
        Some(RectI::new(
            window.left + (card.frame.left * scale).round() as i32,
            window.top + (card.frame.top * scale).round() as i32,
            window.left + (card.frame.right * scale).round() as i32,
            window.top + (card.frame.bottom * scale).round() as i32,
        ))
    }

    /// The card's current visual angle in degrees, published with the same
    /// snapshot as its frame. `None` when it is not on the line.
    pub(super) fn current_card_angle(&self, id: CardId) -> Option<f32> {
        let overlay = self.windows.overlay.as_ref()?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        snapshot
            .cards
            .iter()
            .find(|card| card.id == id)
            .map(|card| card.angle)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_item(
        &mut self,
        id: CardId,
        path: PathBuf,
        own: bool,
        size_px: (u32, u32),
        thumb: ImageData,
        area: Option<RectI>,
        arriving: bool,
        flying: bool,
        note: bool,
        color: usize,
        text: Option<String>,
    ) {
        let card_size = layout::card_size(size_px.0 as f32, size_px.1 as f32);
        let tilt = pseudo_random(id) * 5.0 - 2.5;
        let item = Item {
            id,
            path: path.clone(),
            own,
            note,
            color: color.min(notes::COLOR_COUNT - 1),
            text,
            size_px,
            card_size,
            tilt,
            thumb: thumb.clone(),
            missing: 0,
            last_write: std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH),
            revision: 1,
            captured_at: 0.0,
        };
        self.line.items.push(item);
        self.send_scene(Command::AddCard(CardSpec {
            id,
            card_size,
            tilt,
            arriving,
            image: Some(thumb),
            note,
        }));
        if note {
            self.send_scene(Command::SetNoteColor { id, color });
        }
        if flying {
            self.send_scene(Command::SetFlying { id, flying: true });
        }
        let _ = area;
        self.enforce_capacity();
        self.save_pegged();
        self.state.empty_since = None;
        if !self.state.is_present {
            self.present();
        }
    }

    pub(super) fn alloc_id(&mut self) -> CardId {
        let id = self.line.next_id;
        self.line.next_id += 1;
        id
    }

    pub(super) fn enforce_capacity(&mut self) {
        let width_dip = self
            .windows
            .overlay
            .as_ref()
            .map(|overlay| overlay.width_px() as f32 / overlay.scale().max(0.1))
            .unwrap_or(1920.0);
        let capacity = layout::capacity(width_dip);
        while self.line.items.len() > capacity {
            let id = self.line.items[0].id;
            if self.line.items[0].note {
                // Capacity eviction archives a note instead of deleting it.
                self.archive_note(id);
                break;
            }
            self.start_fall(id);
            self.remove_item(id);
        }
    }

    pub(super) fn remove_item(&mut self, id: CardId) {
        if let Some(position) = self.line.items.iter().position(|item| item.id == id) {
            self.line.items.remove(position);
            self.send_scene(Command::RemoveCard { id });
            self.save_pegged();
        }
    }

    pub(super) fn save_pegged(&mut self) {
        // The demo switches hang generated cards on a line that did not restore
        // the user's own set; writing them would replace the real pegged list
        // with demo paths. `--note-preview-demo` is the same case.
        if self.dev.demo || self.dev.note_preview_demo || self.capture.restore_pending > 0 {
            return;
        }
        let paths: Vec<PathBuf> = self
            .line
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        settings::write_pegged(&paths);
    }

    pub(super) fn hit_test(&self, x: i32, y: i32) -> Option<(CardId, crate::types::RectF)> {
        self.hit_test_impl(x, y, false)
    }

    /// Hit testing for clicks: no 10 DIP hover slop, so the empty strip around
    /// a card stays click-through.
    pub(super) fn hit_test_exact(&self, x: i32, y: i32) -> Option<(CardId, crate::types::RectF)> {
        self.hit_test_impl(x, y, true)
    }

    pub(super) fn hit_test_impl(
        &self,
        x: i32,
        y: i32,
        exact: bool,
    ) -> Option<(CardId, crate::types::RectF)> {
        let overlay = self.windows.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        let scale = snapshot.scale.max(0.1);
        let dip_x = (x - rect.left) as f32 / scale;
        let dip_y = (y - rect.top) as f32 / scale;
        let id = if exact {
            snapshot.card_at_exact(dip_x, dip_y)?
        } else {
            snapshot.card_at(dip_x, dip_y)?
        };
        let card = snapshot.cards.iter().find(|card| card.id == id)?;
        Some((id, card.frame))
    }

    /// The copy/open button under the pointer, in device pixels.
    pub(super) fn action_at(&self, x: i32, y: i32) -> Option<(CardId, ActionKind)> {
        let overlay = self.windows.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        let scale = snapshot.scale.max(0.1);
        snapshot.action_at(
            (x - rect.left) as f32 / scale,
            (y - rect.top) as f32 / scale,
        )
    }

    /// The note colour dot under the pointer, in window DIP.
    pub(super) fn swatch_at(&self, x: i32, y: i32) -> Option<(CardId, usize)> {
        let overlay = self.windows.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        let scale = snapshot.scale.max(0.1);
        snapshot.swatch_at(
            (x - rect.left) as f32 / scale,
            (y - rect.top) as f32 / scale,
        )
    }

    pub(super) fn to_window_dip(&self, x: i32, y: i32) -> (f32, f32) {
        let Some(overlay) = self.windows.overlay.as_ref() else {
            return (f32::NAN, f32::NAN);
        };
        let Some(rect) = win32::window_rect(overlay.hwnd()) else {
            return (f32::NAN, f32::NAN);
        };
        let scale = overlay.scale().max(0.1);
        (
            (x - rect.left) as f32 / scale,
            (y - rect.top) as f32 / scale,
        )
    }

    /// The note editor's paper rectangle in screen pixels; clicks outside it
    /// commit (NOTE-EDITOR.md §5.5).
    pub(super) fn note_editor_paper_px(&self) -> Option<RectI> {
        let editor = self.windows.note_editor.as_ref()?;
        let rect = win32::window_rect(editor.hwnd())?;
        let dpi = win32::window_dpi(editor.hwnd()).max(96);
        let scale = dpi as f32 / 96.0;
        let margin = crate::note_editor::render::MARGIN_DIP * scale;
        Some(RectI::new(
            rect.left + margin.round() as i32,
            rect.top + margin.round() as i32,
            rect.left
                + ((crate::note_editor::render::MARGIN_DIP
                    + crate::note_editor::display::PAPER_WIDTH)
                    * scale)
                    .round() as i32,
            rect.top
                + ((crate::note_editor::render::MARGIN_DIP + PAPER_HEIGHT) * scale).round() as i32,
        ))
    }

    /// The note editor's glass card in screen pixels: the paper inflated by
    /// the same 4 DIP frame the line's cards use. A flight carries the whole
    /// card, so the paper image inside it lands exactly on the editor paper.
    pub(super) fn note_editor_card_px(&self) -> Option<RectI> {
        let paper = self.note_editor_paper_px()?;
        let editor = self.windows.note_editor.as_ref()?;
        let dpi = win32::window_dpi(editor.hwnd()).max(96);
        let inset = (layout::PHOTO_INSET * dpi as f32 / 96.0).round() as i32;
        Some(paper.inflate(inset))
    }
}
