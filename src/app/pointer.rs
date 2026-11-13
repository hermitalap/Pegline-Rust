//! The pointer state machine: dwell, hover, press, double click and drag.
//!
//! The UI thread never receives ordinary mouse messages for the line — the
//! overlay is click-through except over a card, and a low-level hook feeds a
//! queue instead (see `mouse_queue` in the parent). This module is what turns
//! that queue into gestures: it decides when the strip reveals, which card is
//! hovered, when a press becomes a drag, and what the cursor looks like.
//!
//! It is on `App` rather than on a `Pointer` struct because every decision
//! needs the cards, the reveal timing and the windows at once.

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

use super::{
    CardAction, DOUBLE_CLICK, DRAG_DIP, LONG_PRESS, MouseKind, Press, RETRACT_DELAY, REVEAL_DELAY,
    mouse_queue,
};
use crate::layout;
use crate::logging;
use crate::scene::Command;
use crate::types::{ActionKind, CardId};
use crate::win32;
use crate::window::overlay::Overlay;

impl super::App {
    pub(super) fn drain_mouse(&mut self, _now: f64) {
        // A popup menu pumps messages on this thread while it is open, so the
        // low-level hook would otherwise replay the click that chose a menu
        // item as a click on whatever card control sits underneath it.
        if self.windows.menu_open {
            return;
        }
        loop {
            let event = {
                let Ok(mut queue) = mouse_queue().try_lock() else {
                    return;
                };
                queue.pop_front()
            };
            let Some(event) = event else {
                return;
            };
            match event.kind {
                MouseKind::LeftDown => self.on_left_down(event.x, event.y, event.time),
                MouseKind::LeftUp => self.on_left_up(event.x, event.y, event.time),
                MouseKind::RightUp => self.on_right_up(event.x, event.y),
                MouseKind::RightDown => self.on_right_down(event.x, event.y),
            }
        }
    }

    pub(super) fn on_left_down(&mut self, x: i32, y: i32, now: f64) {
        if self.sessions.note_tumbling {
            // The discarded paper is already on its way off the screen; the
            // editor behind it is hidden and must not be committed again.
            return;
        }
        if self.windows.editor.is_some() {
            // The annotation window owns the pointer; the line never reacts
            // while a picture is being marked up. A click outside the window
            // finishes the edit through its focus loss.
            return;
        }
        if self.windows.note_editor.is_some() {
            // The note editor owns this click. Only a click outside its paper
            // commits; the line never reacts while a note is being edited
            // (NOTE-EDITOR.md §5.5, §5.7).
            if let Some(editor) = self.windows.note_editor.as_mut() {
                // The editor's own hit test decides: the paper and its resize
                // bands belong to it, everything else commits.
                if editor.is_commit_point(x, y) {
                    editor.commit();
                }
            }
            return;
        }
        if self.state.note_hover.is_some() {
            // Clicking the floating blank note opens it in the editor; a click
            // in the hotspot itself does nothing (NOTE-EDITOR.md §4.2).
            if self.note_hover_hits(x, y) {
                self.open_new_note_editor();
                return;
            }
            if self.state.note_hotspot_hovered {
                return;
            }
            let preview = self
                .state
                .note_hover
                .as_ref()
                .map(|hover| (hover.center, hover.dpi));
            if let Some((center, dpi)) = preview {
                // A click in the shadow padding is not a note click, but it must
                // not fall through to a card underneath either.
                if self.note_preview_bounds(center, dpi).contains(x, y) {
                    self.cancel_note_hover();
                    return;
                }
            }
            self.cancel_note_hover();
        }
        self.hide_preview();
        // The clothespin is the card's reorder handle and must win over the
        // band click, the swatches, the action pills and the OLE drag path.
        if let Some(id) = self.pin_at(x, y) {
            self.begin_pin_press(id, x, y, now);
            return;
        }
        self.on_band_click(x, y);
        if let Some((id, color)) = self.swatch_at(x, y) {
            self.set_note_color(id, color);
            return;
        }
        // The two hover buttons are real targets; without this a click on
        // "Open" would fall through to the card's single-click action.
        if let Some((id, action)) = self.action_at(x, y) {
            logging::trace(format!("action {action:?} on card {id}"));
            match action {
                ActionKind::Copy => self.copy_item(id),
                ActionKind::Annotate => self.annotate_item(id),
                ActionKind::Open => self.open_item(id),
            }
            return;
        }
        let Some((id, frame)) = self.hit_test_exact(x, y) else {
            return;
        };
        // The discard cross is the top-left 26 DIP square of the card.
        let (dip_x, dip_y) = self.to_window_dip(x, y);
        if dip_x >= frame.left
            && dip_x < frame.left + layout::CROSS_HIT
            && dip_y >= frame.top
            && dip_y < frame.top + layout::CROSS_HIT
        {
            self.discard(id);
            return;
        }
        logging::trace(format!("press card {id} at {x},{y}"));
        self.state.pressed = Some(Press {
            id,
            start: (x, y),
            since: now,
            moved: false,
            handled: false,
        });
        self.send_scene(Command::Press(Some(id)));
        if let Some(overlay) = self.windows.overlay.as_mut() {
            overlay.set_click_through(false);
        }
    }

    pub(super) fn on_left_up(&mut self, x: i32, y: i32, now: f64) {
        // A pin press is a reorder, never a click: releasing inside the slop
        // still settles the card back and does nothing else.
        if let Some(id) = self.state.pin_press {
            self.finish_pin_press();
            logging::trace(format!("pin release {id} at {x},{y}"));
            return;
        }
        let Some(press) = self.state.pressed.take() else {
            return;
        };
        self.send_scene(Command::Press(None));
        if press.moved || press.handled {
            return;
        }
        let (dx, dy) = (x - press.start.0, y - press.start.1);
        let moved = (dx.abs() + dy.abs()) as f32;
        let scale = self
            .windows
            .overlay
            .as_ref()
            .map(Overlay::scale)
            .unwrap_or(1.0);
        if moved > DRAG_DIP * scale {
            return;
        }
        let double = self
            .state
            .last_click
            .map(|(when, id)| id == press.id && now - when < DOUBLE_CLICK)
            .unwrap_or(false);
        if double {
            self.state.last_click = None;
            self.open_item(press.id);
        } else {
            self.state.last_click = Some((now, press.id));
            // A picture still copies on a single click; a note opens its
            // editor. Its Copy pill stays available for the clipboard.
            let note = self
                .line
                .items
                .iter()
                .find(|item| item.id == press.id)
                .is_some_and(|item| item.note);
            if note {
                self.open_note_editor(press.id);
            } else {
                self.copy_item(press.id);
            }
        }
    }

    /// A click on any screen top band suppresses the reveal until the pointer
    /// leaves the band, and puts a revealed line away unless the click landed
    /// on a photo (the same rule as the WPF low-level hook).
    pub(super) fn on_band_click(&mut self, x: i32, y: i32) {
        let Some(screen) = win32::monitor_at(x, y) else {
            return;
        };
        let band_top = screen.bounds.top + (screen.work.top - screen.bounds.top).max(2);
        let in_band = x >= screen.bounds.left
            && x < screen.bounds.right
            && y >= screen.bounds.top
            && y < band_top;
        if !in_band {
            return;
        }
        self.state.band_suppressed = true;
        self.state.hot_zone_since = None;
        logging::trace(format!("band click at {x},{y}"));
        if self.state.is_revealed && self.hit_test(x, y).is_none() {
            self.state.pinned = false;
            if self.settings.line_always() {
                // A top-band click must be able to get the line out of the way;
                // the resident rule resumes on the next reveal.
                self.state.resident_hidden = true;
            }
            self.set_revealed(false);
        }
    }

    pub(super) fn on_right_down(&mut self, x: i32, y: i32) {
        if self.state.pin_press.is_some() {
            return;
        }
        if self.windows.note_editor.is_some() || self.windows.editor.is_some() {
            return;
        }
        self.hide_preview();
        self.on_band_click(x, y);
    }
    pub(super) fn on_right_up(&mut self, x: i32, y: i32) {
        if self.state.pin_press.is_some() {
            return;
        }
        let Some((id, _)) = self.hit_test_exact(x, y) else {
            return;
        };
        let Some(next) = self.show_card_menu(id, x, y) else {
            return;
        };
        match next {
            CardAction::Copy => self.copy_item(id),
            CardAction::Open => self.open_item(id),
            CardAction::Annotate => self.annotate_item(id),
            CardAction::Edit => self.edit_item(id),
            CardAction::Reveal => {
                if let Some(item) = self.line.items.iter().find(|item| item.id == id) {
                    if let Err(error) = win32::reveal_in_explorer(&item.path) {
                        logging::error(format!("reveal: {error}"));
                    }
                }
            }
            CardAction::SaveDesktop => self.save_to_desktop(id),
            CardAction::Discard | CardAction::TakeDown => self.discard(id),
            CardAction::Trash => self.trash_item(id),
        }
    }

    pub(super) fn update_pointer(&mut self, now: f64) {
        let pointer = win32::cursor_pos();
        // `--note-preview-demo`: the parked demo note's card is the pointer, so
        // the hover preview appears while the real mouse stays put.
        let demo_pointer = self
            .dev
            .note_preview_demo_card
            .and_then(|id| self.current_card_px(id))
            .map(|card| {
                (
                    card.center_x().round() as i32,
                    card.center_y().round() as i32,
                )
            });
        let Some((x, y)) = demo_pointer.or(pointer) else {
            return;
        };
        let Some(screen) = win32::monitor_at(x, y) else {
            return;
        };

        // A monitor switch and a live DPI change both invalidate the overlay's
        // size and scale. Only the handle was compared, so changing the display
        // scaling at run time left `screen.dpi` and the overlay's scale at their
        // old values while `window_dpi()` already returned the new one — two
        // scales on one line, and a strip that DXGI stretched to the new window.
        //
        // The `is_revealed` guard is load-bearing: `place_on_screen` also runs
        // `enforce_capacity`, which for pictures starts the fall animation and
        // plays the pop sound. Re-placing a *hidden* line would therefore drop
        // cards out of a strip nobody can see. While the line is down `self.screen`
        // stays stale on purpose, so the reveal path below still sees the change
        // and places the window exactly once, when it comes down.
        let screen_changed = self
            .screen
            .map(|current| current.handle != screen.handle || current.dpi != screen.dpi)
            .unwrap_or(true);
        if screen_changed && self.state.is_revealed {
            // The card under the cursor is about to be re-placed on another
            // monitor or at another scale; a drag cannot survive that because
            // the pin's window coordinates changed underneath it.
            if self.state.pin_press.is_some() {
                self.finish_pin_press();
            }
            self.place_on_screen(screen);
        }

        let band_top = screen.bounds.top + (screen.work.top - screen.bounds.top).max(2);
        let in_band = x >= screen.bounds.left
            && x < screen.bounds.right
            && y >= screen.bounds.top
            && y < band_top;
        if !in_band {
            self.state.band_suppressed = false;
        }

        let blocked = win32::foreground_is_fullscreen_on(screen.bounds);
        let button_down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;

        // The render thread follows the pointer and ends the grab itself when
        // the button is up; this is the UI-side mirror of that fallback. A
        // hidden line can no longer be grabbed, so release it too.
        if self.state.pin_press.is_some() && (!button_down || !self.state.is_revealed) {
            self.finish_pin_press();
        }
        let pin_held = self.state.pin_press.is_some();

        // The line may already be down when a full screen app comes forward
        // (video, game, slideshow). Take it away instead of waiting for the
        // pointer to leave; `present` keeps it from coming back while blocked.
        if blocked && self.state.is_present && self.state.hide_at.is_none() {
            self.dismiss(now);
        }

        if !self.state.is_revealed {
            let can_reveal =
                self.wanted() && !blocked && in_band && !self.state.band_suppressed && !button_down;
            if can_reveal {
                match self.state.hot_zone_since {
                    Some(since) if now - since >= REVEAL_DELAY => {
                        if screen_changed || self.screen.is_none() {
                            self.place_on_screen(screen);
                        }
                        logging::trace(format!("reveal: dwell elapsed on {x},{y}"));
                        self.present();
                        self.reveal(false, 0.0);
                    }
                    Some(_) => {}
                    None => self.state.hot_zone_since = Some(now),
                }
            } else {
                self.state.hot_zone_since = None;
            }
        } else {
            // The zone reaches from the screen's top edge to the window bottom,
            // so moving up never hides the line.
            let inside = self.windows.overlay.as_ref().is_some_and(|overlay| {
                let Some(rect) = win32::window_rect(overlay.hwnd()) else {
                    return false;
                };
                x >= rect.left && x < rect.right && y >= screen.bounds.top && y < rect.bottom
            });
            if inside && self.state.pinned && self.windows.note_editor.is_none() {
                self.state.pinned = false;
            }
            let busy = self.state.pinned
                || self.state.note_pin
                || self.settings.line_always()
                || pin_held
                || self.state.pressed.is_some()
                || self.windows.menu_open
                || now < self.state.peek_until
                || self.state.startup_until.is_some_and(|until| now < until);
            if inside || busy {
                self.state.away_since = None;
            } else {
                let since = *self.state.away_since.get_or_insert(now);
                if now - since >= RETRACT_DELAY {
                    self.state.away_since = None;
                    self.set_revealed(false);
                }
            }
        }

        // Hover and click-through. A pin under the pointer owns the hover:
        // the card does not lift, its buttons do not appear and the 0.35 s
        // preview does not start while the pin is lit.
        let pin_hover = if self.state.is_revealed
            && !pin_held
            && self.windows.note_editor.is_none()
            && self.windows.editor.is_none()
        {
            self.pin_at(x, y)
        } else {
            None
        };
        if self.state.pin_hover != pin_hover {
            self.state.pin_hover = pin_hover;
            self.send_scene(Command::PinHover(pin_hover));
        }
        let hover = if !pin_held
            && pin_hover.is_none()
            && self.state.is_revealed
            && self.windows.note_editor.is_none()
            && self.windows.editor.is_none()
        {
            self.hit_test(x, y).map(|(id, _)| id)
        } else {
            None
        };
        if self.state.hover != hover {
            self.state.hover = hover;
            self.send_scene(Command::Hover(hover));
        }
        // The hover buttons light up individually. The scene only publishes
        // their boxes while the card is hovered, so this is None the rest of
        // the time.
        let hover_action = if !pin_held
            && self.state.is_revealed
            && self.windows.note_editor.is_none()
            && self.windows.editor.is_none()
        {
            self.action_at(x, y)
        } else {
            None
        };
        if self.state.hover_action != hover_action {
            self.state.hover_action = hover_action;
            self.send_scene(Command::HoverAction(hover_action));
        }
        if let Some(overlay) = self.windows.overlay.as_ref() {
            // Note colour dots are clickable too and the WPF dots used a hand
            // cursor; the cards themselves stay on the normal arrow. The note
            // entry hotspot (and its floating note) also use the hand.
            let on_swatch =
                hover_action.is_none() && self.state.is_revealed && self.swatch_at(x, y).is_some();
            overlay.set_hand_cursor(
                hover_action.is_some()
                    || on_swatch
                    || pin_hover.is_some()
                    || self.state.note_hotspot_hovered
                    || self.state.note_hover.is_some(),
            );
            overlay.set_drag_cursor(pin_held);
        }
        if let Some(press) = self.state.pressed.as_mut() {
            let scale = self
                .windows
                .overlay
                .as_ref()
                .map(Overlay::scale)
                .unwrap_or(1.0);
            let distance = ((x - press.start.0).abs() + (y - press.start.1).abs()) as f32;
            if distance > DRAG_DIP * scale && !press.moved {
                press.moved = true;
                logging::trace(format!("press moved {distance:.0}px, drag armed"));
            }
            if !press.moved
                && !press.handled
                && now - press.since >= LONG_PRESS
                && unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0
            {
                let id = press.id;
                press.handled = true;
                self.edit_item(id);
            }
        }
        // Start a drag once the pointer has clearly moved.
        if let Some(press) = self.state.pressed.as_ref() {
            if press.moved && !press.handled && unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0
            {
                let id = press.id;
                self.start_drag(id);
            }
        }
    }

    pub(super) fn update_click_through(&mut self) {
        let Some(overlay) = self.windows.overlay.as_mut() else {
            return;
        };
        let pass = self.state.hover.is_none()
            && self.state.pressed.is_none()
            && self.state.pin_press.is_none()
            && self.state.pin_hover.is_none()
            && !self.state.note_hotspot_hovered
            && self.state.note_hover.is_none();
        overlay.set_click_through(pass);
    }

    /// The clothespin under the pointer, in window DIP. Only an arrived,
    /// published pin is draggable.
    pub(super) fn pin_at(&self, x: i32, y: i32) -> Option<CardId> {
        if !self.state.is_revealed {
            return None;
        }
        let (dip_x, dip_y) = self.to_window_dip(x, y);
        if !dip_x.is_finite() || !dip_y.is_finite() {
            return None;
        }
        let overlay = self.windows.overlay.as_ref()?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        snapshot.pin_at(dip_x, dip_y)
    }

    fn begin_pin_press(&mut self, id: CardId, x: i32, y: i32, now: f64) {
        let (dip_x, dip_y) = self.to_window_dip(x, y);
        if !dip_x.is_finite() || !dip_y.is_finite() {
            return;
        }
        // A pin click must not pair with an earlier card click into a double
        // click, and must never arm the long press or the OLE drag.
        self.state.last_click = None;
        self.state.pin_press = Some(id);
        self.state.pin_hover = Some(id);
        if self.state.hover.take().is_some() {
            self.send_scene(Command::Hover(None));
        }
        if self.state.hover_action.take().is_some() {
            self.send_scene(Command::HoverAction(None));
        }
        self.state.pressed = None;
        self.send_scene(Command::Press(None));
        // The press may have landed on a card whose 0.35 s preview was about
        // to appear; kill it before the grab starts so nothing pops up below
        // the card while it is being dragged.
        self.hide_preview();
        self.state.preview_state.reset();
        self.send_scene(Command::Grab {
            id,
            pointer: (dip_x, dip_y),
        });
        if let Some(overlay) = self.windows.overlay.as_mut() {
            overlay.set_click_through(false);
            overlay.set_hand_cursor(false);
            overlay.set_drag_cursor(true);
        }
        logging::trace(format!("pin press {id} at {x},{y} ({now:.3})"));
    }

    fn finish_pin_press(&mut self) {
        if self.state.pin_press.take().is_none() {
            return;
        }
        self.state.pin_hover = None;
        self.state.pending_order = true;
        self.send_scene(Command::PinHover(None));
        self.send_scene(Command::EndGrab);
        if let Some(overlay) = self.windows.overlay.as_ref() {
            overlay.set_drag_cursor(false);
        }
    }
}
