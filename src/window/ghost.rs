//! The drag ghost: a small see-through copy of the card that follows the
//! pointer while a file is dragged (BEHAVIOR §8.1, `Gestures.cs:212-251`).
//!
//! It is a transparent DirectComposition window like the flight and preview
//! windows, pooled across drags: created on the first drag, then hidden and
//! shrunk to 2x2 so its surface memory goes back. The render thread owns the
//! swap chain, the placement and the show/hide, and it draws only when the
//! content changes; following the pointer is one `SetWindowPos` per tick. That
//! is what lets the first frame the user sees be the finished one: the window is
//! placed, resized and drawn while still hidden, and only then shown.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, MA_NOACTIVATE, SW_HIDE, ShowWindow,
    WM_ERASEBKGND, WM_MOUSEACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::layout;
use crate::logging;
use crate::render::{Brush, Present, Renderer};
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;

const CLASS_NAME: PCWSTR = w!("PeglineGhost");
const WINDOW_TITLE: PCWSTR = w!("Pegline drag preview");

/// The ghost is the card at 90 % (BEHAVIOR §3.2).
const GHOST_SCALE: f32 = 0.9;
/// How far below the pointer the ghost hangs, DIP (BEHAVIOR §3.2).
const FOLLOW_DIP: f32 = 12.0;
/// Whole-card opacity of the ghost (BEHAVIOR §3.2).
const GHOST_OPACITY: f32 = 0.85;
/// Frame border width, DIP, as on the cards.
const BORDER_DIP: f32 = 0.75;
/// Sanity cap for the window edge: a 144x112 card at 0.9x needs 260 px at 200 %,
/// so anything past this is bad input rather than a card.
const MAX_EDGE_PX: f32 = 1024.0;

enum GhostCommand {
    /// New content: place, resize, upload, draw, present, then show.
    Show {
        image: ImageData,
        rect: RectI,
        dpi: u32,
    },
    /// Reposition only; the pixel size and the content do not change.
    Move(RectI),
    /// Hide, drop the bitmap and shrink the swap chain.
    Hide,
    Stop,
}

/// HWND is a raw pointer; the window outlives the thread because `shutdown`
/// joins before destroying it.
struct SendHwnd(HWND);

// SAFETY: the render thread only places, shows and hides the window and draws
// into its composition target; it never destroys or re-parents it.
unsafe impl Send for SendHwnd {}

/// The pooled drag ghost, owned by the UI thread.
pub struct Ghost {
    hwnd: HWND,
    tx: Sender<GhostCommand>,
    join: Option<JoinHandle<()>>,
    /// UI-side flag: a show was requested and no hide has followed yet.
    visible: bool,
    /// Pixel size of the ghost, cached by `show` so `move_to` reuses it and
    /// never needs to resize (and therefore never redraws) mid-drag.
    size_px: (u32, u32),
    /// Monitor scale `size_px` was computed at.
    dpi: u32,
}

impl Ghost {
    /// Builds the hidden window and its render thread. Call lazily on the first
    /// drag: it owns a swap chain and a render thread. Failure is not fatal, the
    /// drag simply runs with the system cursor.
    pub fn create() -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(ghost_wndproc))?;
        // SAFETY: queries this module's handle; no argument is dereferenced.
        let instance = unsafe {
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .map_err(|e| format!("GetModuleHandleW: {e}"))?
        };
        // SAFETY: the class is registered; the window starts hidden at 2x2 and
        // is placed by the render thread before it is ever shown.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT,
                CLASS_NAME,
                WINDOW_TITLE,
                WS_POPUP,
                0,
                0,
                2,
                2,
                None,
                None,
                Some(windows::Win32::Foundation::HINSTANCE(instance.0)),
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW(ghost): {e}"))?;
        // SAFETY: our own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let send_hwnd = SendHwnd(hwnd);
        let join = thread::Builder::new()
            .name("pegline-ghost".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    ghost_thread(send_hwnd, rx, ready_tx)
                }));
                if outcome.is_err() {
                    logging::error("panic in the ghost render thread");
                }
            })
            .map_err(|e| format!("spawn ghost thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = join.join();
                // SAFETY: the thread is gone and the window is ours.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
            Err(_) => {
                let _ = join.join();
                // SAFETY: the thread is gone and the window is ours.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err("ghost render thread died during setup".to_string());
            }
        }

        Ok(Self {
            hwnd,
            tx,
            join: Some(join),
            visible: false,
            size_px: (0, 0),
            dpi: 0,
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Shows `image` (the card thumbnail) at 90 % of `card_dip`, centred on the
    /// pointer horizontally and 12 DIP below it, in screen pixels.
    ///
    /// The window is placed, resized and drawn on the render thread while it is
    /// still hidden, so a stale or blank frame is never shown and the UI thread
    /// never waits for the GPU.
    pub fn show(&mut self, image: &ImageData, card_dip: (f32, f32), cursor: (i32, i32), dpi: u32) {
        let dpi = dpi.max(96);
        let Some(size) = ghost_size(card_dip, dpi) else {
            logging::error(format!("ghost: unusable card size {card_dip:?}"));
            self.hide();
            return;
        };
        if !image.is_valid() {
            logging::error("ghost: invalid thumbnail");
            self.hide();
            return;
        }
        self.size_px = size;
        self.dpi = dpi;
        self.visible = true;
        let rect = follow_rect(cursor, size, dpi);
        let _ = self.tx.send(GhostCommand::Show {
            image: image.clone(),
            rect,
            dpi,
        });
    }

    /// Moves the ghost with the pointer. Nothing is redrawn: the window keeps
    /// the pixel size [`Ghost::show`] computed, so a monitor with another scale
    /// is picked up by the next drag instead of by a resize mid-drag (which
    /// would throw the presented frame away). `dpi` is the scale of the monitor
    /// under the pointer, or 0 when the caller does not know it and the scale
    /// the ghost was drawn at should be kept; it only positions the 12 DIP gap.
    pub fn move_to(&mut self, cursor: (i32, i32), dpi: u32) {
        if !self.visible || self.size_px.0 == 0 || self.size_px.1 == 0 {
            return;
        }
        let dpi = if dpi == 0 { self.dpi } else { dpi };
        let _ = self
            .tx
            .send(GhostCommand::Move(follow_rect(cursor, self.size_px, dpi)));
    }

    /// Hides the ghost. The render thread drops the bitmap and shrinks the swap
    /// chain to 2x2, which is what keeps the memory flat across many drags.
    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        let _ = self.tx.send(GhostCommand::Hide);
    }

    pub fn shutdown(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.tx.send(GhostCommand::Stop);
            let _ = join.join();
            // SAFETY: the render thread has been joined and the window was
            // created by `create` on this thread.
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl Drop for Ghost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn ghost_thread(hwnd: SendHwnd, rx: Receiver<GhostCommand>, ready: Sender<Result<(), String>>) {
    let _com = match window::ComGuard::init(windows::Win32::System::Com::COINIT_MULTITHREADED) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut renderer = match Renderer::new(hwnd.0, 2, 2, 96) {
        Ok(renderer) => renderer,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }

    let mut texture: Option<ID2D1Bitmap1> = None;
    let mut shown = false;
    while let Ok(command) = rx.recv() {
        match command {
            GhostCommand::Stop => break,
            GhostCommand::Hide => release(&mut renderer, hwnd.0, &mut texture, &mut shown),
            GhostCommand::Move(rect) => {
                if shown {
                    win32::place_window(hwnd.0, rect, true);
                }
            }
            GhostCommand::Show { image, rect, dpi } => {
                // Never leave an old frame on screen while the new one is
                // composed: hide first, draw, then show.
                if shown {
                    win32::hide_window(hwnd.0);
                    shown = false;
                }
                match show_frame(&mut renderer, hwnd.0, &image, rect, dpi, &mut texture) {
                    Ok(()) => {
                        win32::show_no_activate(hwnd.0);
                        shown = true;
                    }
                    Err(error) => {
                        logging::error(format!("ghost: {error}"));
                        release(&mut renderer, hwnd.0, &mut texture, &mut shown);
                    }
                }
            }
        }
    }
    logging::trace("ghost render thread stopped");
}

/// Places, resizes, uploads and draws one frame. The window is hidden while
/// this runs, so nothing half-drawn can be seen.
fn show_frame(
    renderer: &mut Renderer,
    hwnd: HWND,
    image: &ImageData,
    rect: RectI,
    dpi: u32,
    texture: &mut Option<ID2D1Bitmap1>,
) -> Result<(), String> {
    let width = rect.width().max(1) as u32;
    let height = rect.height().max(1) as u32;
    win32::place_window(hwnd, rect, true);
    renderer.resize(width, height, dpi)?;
    *texture = Some(renderer.upload(image)?);

    // The window is device pixels; drawing is DIP, so the card is the whole
    // window at this monitor's scale.
    let scale = renderer.scale();
    let card = RectF::from_xywh(0.0, 0.0, width as f32 / scale, height as f32 / scale);
    let inset = layout::PHOTO_INSET;
    let photo = RectF::new(
        card.left + inset,
        card.top + inset,
        card.right - inset,
        card.bottom - inset,
    );

    renderer.begin()?;
    // The whole card fades as one group: glass, photo and border.
    let layered = renderer.push_opacity(card.inflated(2.0), GHOST_OPACITY);
    renderer.fill_round(card, layout::CARD_RADIUS, Brush::Glass);
    if let Some(bitmap) = texture.as_ref() {
        renderer.draw_photo(bitmap, photo, layout::PHOTO_RADIUS);
    }
    renderer.stroke_card(card, layout::CARD_RADIUS, BORDER_DIP);
    if layered {
        renderer.pop_opacity();
    }
    renderer.end()?;
    if renderer.present(true) == Present::DeviceLost {
        return Err("device lost while showing the drag ghost".to_string());
    }
    Ok(())
}

/// Hides the ghost, drops the thumbnail and gives the swap chain's memory back.
fn release(
    renderer: &mut Renderer,
    hwnd: HWND,
    texture: &mut Option<ID2D1Bitmap1>,
    shown: &mut bool,
) {
    if *shown {
        win32::hide_window(hwnd);
        *shown = false;
    }
    // Drop the photo first, then shrink the buffers it lived next to.
    *texture = None;
    if let Err(error) = renderer.resize(2, 2, 96) {
        logging::error(format!("ghost resize: {error}"));
    }
}

/// Pixel size of the ghost: the card at 90 %, on the monitor scale, rounded up
/// to whole pixels. `None` when the card size is unusable.
fn ghost_size(card_dip: (f32, f32), dpi: u32) -> Option<(u32, u32)> {
    let (width_dip, height_dip) = card_dip;
    if !width_dip.is_finite() || !height_dip.is_finite() {
        return None;
    }
    let scale = dpi.max(96) as f32 / 96.0;
    let width = (width_dip * GHOST_SCALE * scale).ceil();
    let height = (height_dip * GHOST_SCALE * scale).ceil();
    if width < 1.0 || height < 1.0 || width > MAX_EDGE_PX || height > MAX_EDGE_PX {
        return None;
    }
    Some((width as u32, height as u32))
}

/// The ghost's window rectangle for a pointer position: centred on the pointer
/// horizontally, 12 DIP below it, in device pixels.
fn follow_rect(cursor: (i32, i32), size: (u32, u32), dpi: u32) -> RectI {
    let scale = dpi.max(96) as f32 / 96.0;
    let width = size.0 as i32;
    let height = size.1 as i32;
    let left = cursor.0 - width / 2;
    let top = cursor.1 + (FOLLOW_DIP * scale).round() as i32;
    RectI::new(left, top, left + width, top + height)
}

/// Window procedure: never take focus, never erase.
///
/// SAFETY: this only reads message parameters; the window is destroyed on the
/// UI thread after the render thread has stopped.
unsafe extern "system" fn ghost_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => return LRESULT(1),
        _ => {}
    }
    // SAFETY: default processing for messages we do not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghost_is_the_card_at_ninety_percent_in_pixels() {
        assert_eq!(ghost_size((144.0, 112.0), 96), Some((130, 101)));
        assert_eq!(ghost_size((144.0, 112.0), 144), Some((195, 152)));
        assert_eq!(ghost_size((144.0, 112.0), 192), Some((260, 202)));
    }

    #[test]
    fn unusable_card_sizes_are_refused() {
        assert_eq!(ghost_size((0.0, 112.0), 96), None);
        assert_eq!(ghost_size((144.0, -1.0), 96), None);
        assert_eq!(ghost_size((f32::NAN, 112.0), 96), None);
        assert_eq!(ghost_size((4000.0, 3000.0), 96), None);
    }

    #[test]
    fn the_ghost_hangs_below_and_centred_on_the_pointer() {
        assert_eq!(
            follow_rect((500, 300), (130, 101), 96),
            RectI::new(435, 312, 565, 413)
        );
        assert_eq!(
            follow_rect((500, 300), (130, 101), 192),
            RectI::new(435, 324, 565, 425)
        );
    }
}
