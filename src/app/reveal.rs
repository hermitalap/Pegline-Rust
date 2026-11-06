//! Bringing the line on screen, taking it away, and moving it between monitors.
//!
//! The line is a no-activate overlay that must never steal focus, so "show"
//! and "hide" are not simple window operations: `present` waits for a moment
//! when no full-screen app owns the screen, `reveal` springs the strip down,
//! and `dismiss` runs the retract animation before the window is hidden.
//!
//! Every function here is on `App` because it has to touch the overlay, the
//! scene channel and the state machine together; they live in their own file
//! so that `tick` reads as orchestration rather than window plumbing.

use super::HIDE_DELAY;
use crate::scene::Command;
use crate::win32::{self, Monitor};

impl super::App {
    pub(super) fn wanted(&self) -> bool {
        !self.line.items.is_empty() || self.state.keep_open || self.dev.demo
    }

    pub(super) fn present(&mut self) {
        if self.state.is_present {
            return;
        }
        // A full screen app on the line's monitor owns the screen; never put
        // the strip over it (WPF `Refresh` made the same call).
        let blocked = self
            .screen
            .is_some_and(|screen| win32::foreground_is_fullscreen_on(screen.bounds));
        if blocked {
            return;
        }
        if let Some(overlay) = self.windows.overlay.as_mut() {
            overlay.show(true);
            self.state.is_present = true;
            self.state.hide_at = None;
        }
    }

    pub(super) fn dismiss(&mut self, now: f64) {
        if !self.state.is_present {
            return;
        }
        self.hide_preview();
        self.state.preview_state.reset();
        self.set_revealed(false);
        if self.state.hide_at.is_none() {
            self.state.hide_at = Some(now + HIDE_DELAY);
        }
    }

    pub(super) fn reveal(&mut self, pin: bool, peek: f64) {
        if !self.state.is_present {
            return;
        }
        if pin {
            self.state.pinned = true;
        }
        if peek > 0.0 {
            self.state.peek_until = win32::now_seconds() + peek;
        }
        self.state.away_since = None;
        if self.state.is_revealed {
            return;
        }
        self.state.is_revealed = true;
        self.send_scene(Command::Reveal(true));
        if let Some(overlay) = self.windows.overlay.as_mut() {
            overlay.show(true);
        }
    }

    pub(super) fn set_revealed(&mut self, revealed: bool) {
        if self.state.is_revealed == revealed {
            return;
        }
        self.state.is_revealed = revealed;
        self.state.pinned = false;
        self.state.peek_until = 0.0;
        self.state.note_peek_on = false;
        if !revealed {
            self.state.hover = None;
            self.state.hover_action = None;
            if let Some(overlay) = self.windows.overlay.as_ref() {
                overlay.set_hand_cursor(false);
            }
            self.send_scene(Command::Hover(None));
            self.send_scene(Command::HoverAction(None));
            self.hide_preview();
            self.cancel_note_hover();
            self.state.preview_state.reset();
        }
        self.send_scene(Command::Reveal(revealed));
    }

    pub(super) fn toggle(&mut self) {
        if self.state.is_revealed {
            self.dismiss(win32::now_seconds());
            // An empty line opened on purpose is dismissed on purpose too; it
            // must not linger forever behind the 0.7 s empty-line rule.
            if self.line.items.is_empty() {
                self.state.keep_open = false;
            }
        } else {
            // "Open the line on purpose" (WPF `keepOpen`): an empty line stays
            // down with its hint instead of retracting after 0.7 s.
            self.state.keep_open = true;
            let screen = self.pointer_screen();
            if let Some(screen) = screen {
                if self.screen.map(|current| current.handle) != Some(screen.handle) {
                    self.place_on_screen(screen);
                }
            }
            self.present();
            self.reveal(true, 0.0);
        }
    }

    pub(super) fn place_on_screen(&mut self, screen: Monitor) {
        self.cancel_note_hover();
        self.hide_preview();
        self.state.preview_state.reset();
        if let Some(overlay) = self.windows.overlay.as_mut() {
            overlay.place(screen.work, screen.dpi);
            overlay.send(Command::SetVisible(self.state.is_present));
            overlay.set_frame_cap(self.settings.fps_cap().period());
        }
        self.screen = Some(screen);
        self.enforce_capacity();
    }
}
