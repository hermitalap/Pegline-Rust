//! The hover-preview window: a large copy of the hovered photo, shown after a
//! stable 0.35 s hover (BEHAVIOR §7).
//!
//! It is a second transparent DirectComposition window like the flight window.
//! The render thread owns the swap chain, the placement and the show/hide, so
//! the first thing the user sees is the finished frame: the window is resized
//! and drawn while still hidden, then shown without activation. When the
//! preview is dismissed the bitmap and the surface are released, which is what
//! keeps `probe`'s private memory flat after a preview (BEHAVIOR §6.7).
//!
//! The blank entry note also leans towards the pointer (NOTE-EDITOR.md §4.5).
//! A top-level window cannot rotate, so the UI thread keeps moving the window
//! while the render thread repaints the paper at the latest lean; the requests
//! are coalesced, and a paper that is standing still is not repainted at all.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HTCLIENT, IDC_HAND, LoadCursorW, MA_NOACTIVATE,
    SW_HIDE, SetCursor, ShowWindow, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_SETCURSOR, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::layout;
use crate::logging;
use crate::notes;
use crate::preview as geometry;
use crate::render::{Brush, Present, Renderer, TextId};
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;

const CLASS_NAME: PCWSTR = w!("PeglinePreview");
const WINDOW_TITLE: PCWSTR = w!("Pegline preview");
/// Transparent margin around the hover note, DIP (the shadow lives here).
pub const NOTE_PADDING_DIP: f32 = 24.0;
/// Transparent margin around a hover preview card, DIP: the baked card shadow
/// reaches 24 DIP out and sits 5 DIP lower, so both picture and note previews
/// use the same room and the same frame.
pub const PREVIEW_PADDING_DIP: f32 = 30.0;
/// The blank entry's "add note" icon: the supplied SVG's 24×24 box, DIP.
const NOTE_ICON_DIP: f32 = 24.0;
/// Gap between that icon and the label below it, DIP.
const NOTE_ICON_GAP_DIP: f32 = 4.0;
/// Smallest lean change, in degrees, that is worth a repaint. Well below what
/// the eye can see on a 5° tilt, and it lets a settled paper sleep.
const NOTE_TILT_STEP_DEG: f32 = 0.05;

/// The visible card of a note's hover preview, DIP: the large sheet inside the
/// same glass card as a note on the line. [`PREVIEW_PADDING_DIP`] is added by
/// the window painter.
pub fn note_preview_card_dip() -> (f32, f32) {
    (
        notes::PREVIEW_WIDTH_DIP + layout::PHOTO_INSET * 2.0,
        notes::PREVIEW_HEIGHT_DIP + layout::PHOTO_INSET * 2.0,
    )
}

/// True while the window shows a note rather than a photo: the note is a real
/// hover target, so it stops being click-through and uses the hand cursor.
static NOTE_MODE: AtomicBool = AtomicBool::new(false);

enum PreviewCommand {
    Show {
        image: ImageData,
        bounds: RectI,
        dpi: u32,
    },
    /// A blank note for the invisible entry hotspot (NOTE-EDITOR.md §4.4).
    ShowNote {
        color: usize,
        bounds: RectI,
        dpi: u32,
        /// Initial lean, degrees; the entry starts tilted with only one corner
        /// above the screen and straightens as it falls.
        tilt: f32,
    },
    /// The rendered sheet of a note whose card is hovered (BEHAVIOR §7).
    ShowNoteContent {
        image: ImageData,
        bounds: RectI,
        dpi: u32,
    },
    /// Repaint the note at the latest lean; the window is already placed.
    Turn,
    Hide,
    Stop,
}

/// HWND is a raw pointer; the window outlives the thread because `shutdown`
/// joins before destroying it.
struct SendHwnd(HWND);

// SAFETY: the render thread only places the window and draws into its
// composition target; it never destroys or re-parents it.
unsafe impl Send for SendHwnd {}

/// The pooled preview window, owned by the UI thread.
pub struct Preview {
    hwnd: HWND,
    tx: Sender<PreviewCommand>,
    join: Option<JoinHandle<()>>,
    visible: bool,
    /// Latest lean the UI thread asked for, degrees. The render thread always
    /// reads this slot, so a request that lands while a repaint is queued is
    /// merged into it instead of being dropped.
    tilt: Arc<Mutex<f32>>,
    /// True while the render thread still has to pick the lean up.
    tilt_pending: Arc<AtomicBool>,
    /// The lean last handed to the render thread, so a still paper does not
    /// wake it every 15 ms.
    sent_tilt: Cell<f32>,
}

impl Preview {
    pub fn create() -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(preview_wndproc))?;
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
        .map_err(|e| format!("CreateWindowExW(preview): {e}"))?;
        // SAFETY: our own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let send_hwnd = SendHwnd(hwnd);
        let tilt = Arc::new(Mutex::new(0.0f32));
        let tilt_pending = Arc::new(AtomicBool::new(false));
        let thread_tilt = Arc::clone(&tilt);
        let thread_pending = Arc::clone(&tilt_pending);
        let join = thread::Builder::new()
            .name("pegline-preview".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    preview_thread(send_hwnd, rx, ready_tx, thread_tilt, thread_pending)
                }));
                if outcome.is_err() {
                    logging::error("panic in the preview render thread");
                }
            })
            .map_err(|e| format!("spawn preview thread: {e}"))?;

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
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err("preview render thread died during setup".to_string());
            }
        }

        Ok(Self {
            hwnd,
            tx,
            join: Some(join),
            visible: false,
            tilt,
            tilt_pending,
            sent_tilt: Cell::new(0.0),
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Shows `image` in `bounds` (screen pixels). The window is placed and
    /// drawn on the render thread; the UI thread never blocks on it.
    pub fn show(&mut self, image: ImageData, bounds: RectI, dpi: u32) {
        if bounds.is_empty() {
            self.hide();
            return;
        }
        self.visible = true;
        NOTE_MODE.store(false, Ordering::Release);
        win32::set_click_through(self.hwnd, true);
        let _ = self.tx.send(PreviewCommand::Show { image, bounds, dpi });
    }

    /// Shows a blank note in `bounds` (the card plus [`NOTE_PADDING_DIP`] on
    /// every side), drawn like the real note card: shadow, paper, glass border.
    /// `tilt` is the starting lean in degrees.
    pub fn show_note(&mut self, color: usize, bounds: RectI, dpi: u32, tilt: f32) {
        if bounds.is_empty() {
            self.hide();
            return;
        }
        self.visible = true;
        NOTE_MODE.store(true, Ordering::Release);
        // The note is a hover target: the hand cursor and the click belong to
        // this window, not to whatever is underneath it.
        win32::set_click_through(self.hwnd, false);
        self.sent_tilt.set(tilt);
        if let Ok(mut slot) = self.tilt.lock() {
            *slot = tilt;
        }
        let _ = self.tx.send(PreviewCommand::ShowNote {
            color,
            bounds,
            dpi,
            tilt,
        });
    }

    /// Moves the visible window without redrawing. Used while the hover note
    /// follows the pointer (the content is static, so this is one SetWindowPos
    /// per tick, not a frame).
    pub fn move_to(&self, bounds: RectI) {
        if !self.visible || bounds.is_empty() {
            return;
        }
        win32::place_window(self.hwnd, bounds, true);
    }

    /// Shows a rendered note sheet in `bounds` (the sheet plus its glass card
    /// and shadow padding): the hover preview of a note on the line.
    ///
    /// Like a photo preview it is click-through and never takes focus, so the
    /// card's own buttons stay usable while it hangs below them.
    pub fn show_note_content(&mut self, image: ImageData, bounds: RectI, dpi: u32) {
        if bounds.is_empty() {
            self.hide();
            return;
        }
        self.visible = true;
        NOTE_MODE.store(false, Ordering::Release);
        win32::set_click_through(self.hwnd, true);
        let _ = self
            .tx
            .send(PreviewCommand::ShowNoteContent { image, bounds, dpi });
    }

    /// Leans the visible note by `degrees` (positive = clockwise).
    ///
    /// The window cannot rotate, so the paper is repainted in place. Requests
    /// are coalesced: at most one repaint is queued, the render thread always
    /// picks up the newest angle, and a paper that is standing still is not
    /// repainted at all.
    pub fn tilt_note(&self, degrees: f32) {
        if !self.visible || !degrees.is_finite() {
            return;
        }
        if (degrees - self.sent_tilt.get()).abs() < NOTE_TILT_STEP_DEG {
            return;
        }
        self.sent_tilt.set(degrees);
        if let Ok(mut slot) = self.tilt.lock() {
            *slot = degrees;
        }
        if !self.tilt_pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(PreviewCommand::Turn);
        }
    }

    /// Hides the preview and releases its surface.
    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        NOTE_MODE.store(false, Ordering::Release);
        self.sent_tilt.set(0.0);
        let _ = self.tx.send(PreviewCommand::Hide);
    }

    pub fn shutdown(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.tx.send(PreviewCommand::Stop);
            let _ = join.join();
        }
        // SAFETY: the render thread has been joined.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn preview_thread(
    hwnd: SendHwnd,
    rx: Receiver<PreviewCommand>,
    ready: Sender<Result<(), String>>,
    tilt: Arc<Mutex<f32>>,
    tilt_pending: Arc<AtomicBool>,
) {
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

    let mut texture: Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap1> = None;
    let mut shown = false;
    // The lean currently painted into the swap chain, degrees.
    let mut drawn_tilt = 0.0f32;
    while let Ok(command) = rx.recv() {
        match command {
            PreviewCommand::Stop => break,
            PreviewCommand::Hide => {
                if shown {
                    win32::hide_window(hwnd.0);
                    shown = false;
                }
                // Drop the photo first, then give the swap chain's memory back.
                texture = None;
                if let Err(error) = renderer.resize(2, 2, 96) {
                    logging::error(format!("preview resize: {error}"));
                }
            }
            PreviewCommand::Show { image, bounds, dpi } => {
                let outcome = show_frame(&mut renderer, hwnd.0, &image, bounds, dpi, &mut texture);
                match outcome {
                    Ok(()) => {
                        if !shown {
                            win32::show_no_activate(hwnd.0);
                            shown = true;
                        }
                    }
                    Err(error) => {
                        logging::error(format!("preview: {error}"));
                        if shown {
                            win32::hide_window(hwnd.0);
                            shown = false;
                        }
                    }
                }
            }
            PreviewCommand::ShowNote {
                color,
                bounds,
                dpi,
                tilt,
            } => {
                let outcome = show_note_frame(
                    &mut renderer,
                    hwnd.0,
                    color,
                    bounds,
                    dpi,
                    tilt,
                    &mut texture,
                );
                match outcome {
                    Ok(()) => {
                        drawn_tilt = tilt;
                        if !shown {
                            win32::show_no_activate(hwnd.0);
                            shown = true;
                        }
                    }
                    Err(error) => {
                        logging::error(format!("note preview: {error}"));
                        if shown {
                            win32::hide_window(hwnd.0);
                            shown = false;
                        }
                    }
                }
            }
            PreviewCommand::ShowNoteContent { image, bounds, dpi } => {
                let outcome = show_note_content_frame(
                    &mut renderer,
                    hwnd.0,
                    &image,
                    bounds,
                    dpi,
                    &mut texture,
                );
                match outcome {
                    Ok(()) => {
                        if !shown {
                            win32::show_no_activate(hwnd.0);
                            shown = true;
                        }
                    }
                    Err(error) => {
                        logging::error(format!("note preview: {error}"));
                        if shown {
                            win32::hide_window(hwnd.0);
                            shown = false;
                        }
                    }
                }
            }
            PreviewCommand::Turn => {
                // Clear the flag before reading the slot: a lean that lands
                // after this point either shows up here or queues a new Turn,
                // so no request can be lost.
                tilt_pending.store(false, Ordering::Release);
                let degrees = tilt.lock().map(|slot| *slot).unwrap_or(0.0);
                if (degrees - drawn_tilt).abs() < NOTE_TILT_STEP_DEG {
                    continue;
                }
                let Some(bitmap) = texture.as_ref() else {
                    continue;
                };
                drawn_tilt = degrees;
                if let Err(error) = turn_note_frame(&mut renderer, bitmap, degrees) {
                    logging::error(format!("note lean: {error}"));
                }
            }
        }
    }
    logging::trace("preview render thread stopped");
}

/// Shows a picture's hover preview. `bounds` is the visible card; the window
/// around it is enlarged by [`PREVIEW_PADDING_DIP`] so the baked shadow fits
/// exactly as on a note preview.
fn show_frame(
    renderer: &mut Renderer,
    hwnd: HWND,
    image: &ImageData,
    bounds: RectI,
    dpi: u32,
    texture: &mut Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap1>,
) -> Result<(), String> {
    let scale = dpi.max(96) as f32 / 96.0;
    let window = geometry::window_bounds(bounds, PREVIEW_PADDING_DIP, scale);
    if window.is_empty() {
        return Err("empty preview window".to_string());
    }
    let width = window.width().max(1) as u32;
    let height = window.height().max(1) as u32;
    logging::trace(format!(
        "preview card {}x{} at {bounds:?}, window {window:?}",
        image.width, image.height
    ));
    win32::place_window(hwnd, window, true);
    renderer.resize(width, height, dpi)?;
    let bitmap = renderer.upload(image)?;
    let card = RectF::from_xywh(
        PREVIEW_PADDING_DIP,
        PREVIEW_PADDING_DIP,
        bounds.width() as f32 / scale,
        bounds.height() as f32 / scale,
    );
    renderer.begin()?;
    paint_preview_card(renderer, &bitmap, card);
    renderer.end()?;
    *texture = Some(bitmap);
    if renderer.present(true) == Present::DeviceLost {
        return Err("device lost while showing a preview".to_string());
    }
    Ok(())
}

/// The blank note's glass card inside its window, DIP: the paper plus
/// [`layout::PHOTO_INSET`] on every side, exactly like a picture card. The
/// padding around it holds the shadow, and the 5° lean never leaves it.
fn note_card() -> RectF {
    let (width, height) = layout::note_card_size();
    RectF::from_xywh(NOTE_PADDING_DIP, NOTE_PADDING_DIP, width, height)
}

/// Paints the shared preview frame into the current frame: the baked card
/// shadow, the glass card, the content inset like a photo card and the
/// hairline border. Both a picture and a note preview use it, so their frames
/// are identical (BEHAVIOR §7).
fn paint_preview_card(
    renderer: &mut Renderer,
    bitmap: &windows::Win32::Graphics::Direct2D::ID2D1Bitmap1,
    card: RectF,
) {
    let inset = layout::PHOTO_INSET;
    let content = RectF::new(
        card.left + inset,
        card.top + inset,
        card.right - inset,
        card.bottom - inset,
    );
    renderer.draw_shadow(card, 0.18, false);
    renderer.fill_round(card, layout::CARD_RADIUS, Brush::Glass);
    renderer.draw_photo(bitmap, content, layout::PHOTO_RADIUS);
    renderer.stroke_card(card, layout::CARD_RADIUS, 0.75);
}

/// Paints the card (shadow, glass frame, the note paper inset like a photo and
/// the hairline border), leaning by `degrees` (positive = clockwise) about the
/// card's centre. `label` draws the blank entry's add-note icon and caption;
/// the content preview carries its text in the paper bitmap instead.
///
/// The lean stays inside [`NOTE_PADDING_DIP`], so the window keeps its size and
/// the render thread can repaint without resizing or re-placing it.
fn draw_note(
    renderer: &mut Renderer,
    bitmap: &windows::Win32::Graphics::Direct2D::ID2D1Bitmap1,
    card: RectF,
    degrees: f32,
    label: bool,
) -> Result<(), String> {
    let (center_x, center_y) = card.center();
    renderer.begin()?;
    renderer.set_transform(Matrix3x2::rotation_around(
        degrees,
        Vector2 {
            X: center_x,
            Y: center_y,
        },
    ));
    paint_preview_card(renderer, bitmap, card);
    if label {
        // The blank note says what it is in the note's own ink: an add-note
        // icon above, and a label at the same size and weight as a short
        // note's body text, so it reads as something already written on the
        // paper.
        let (text_w, text_h) = renderer.text_size(TextId::NewNote);
        let content_h = NOTE_ICON_DIP + NOTE_ICON_GAP_DIP + text_h;
        let top = center_y - content_h * 0.5;
        renderer.draw_note_add_icon(
            (center_x, top + NOTE_ICON_DIP * 0.5),
            NOTE_ICON_DIP,
            Brush::NoteInk,
        );
        renderer.draw_text(
            TextId::NewNote,
            (
                center_x - text_w * 0.5,
                top + NOTE_ICON_DIP + NOTE_ICON_GAP_DIP,
            ),
            Brush::NoteInk,
        );
    }
    renderer.reset_transform();
    renderer.end()
}

/// Draws the blank hover note upright, once, when the note window appears.
fn show_note_frame(
    renderer: &mut Renderer,
    hwnd: HWND,
    color: usize,
    bounds: RectI,
    dpi: u32,
    tilt: f32,
    texture: &mut Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap1>,
) -> Result<(), String> {
    let width = bounds.width().max(1) as u32;
    let height = bounds.height().max(1) as u32;
    logging::trace(format!(
        "note preview frame {width}x{height} at {bounds:?} tilt={tilt:.1}"
    ));
    win32::place_window(hwnd, bounds, true);
    renderer.resize(width, height, dpi)?;
    let image = notes::render("", color).ok_or_else(|| "note render failed".to_string())?;
    let bitmap = renderer.upload(&image)?;
    draw_note(renderer, &bitmap, note_card(), tilt, true)?;
    *texture = Some(bitmap);
    if renderer.present(true) == Present::DeviceLost {
        return Err("device lost while showing a note preview".to_string());
    }
    logging::trace("note preview frame presented");
    Ok(())
}

/// Draws the hover preview of a note that is hanging on the line: the same
/// glass card and paper as the card, with the larger sheet rendered off the UI
/// thread. `bounds` is the visible card; the window adds the shadow room.
fn show_note_content_frame(
    renderer: &mut Renderer,
    hwnd: HWND,
    image: &ImageData,
    bounds: RectI,
    dpi: u32,
    texture: &mut Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap1>,
) -> Result<(), String> {
    let scale = dpi.max(96) as f32 / 96.0;
    let window = geometry::window_bounds(bounds, PREVIEW_PADDING_DIP, scale);
    if window.is_empty() {
        return Err("empty preview window".to_string());
    }
    let width = window.width().max(1) as u32;
    let height = window.height().max(1) as u32;
    logging::trace(format!(
        "note content preview {}x{} at {bounds:?}, window {window:?}",
        image.width, image.height
    ));
    win32::place_window(hwnd, window, true);
    renderer.resize(width, height, dpi)?;
    let card = RectF::from_xywh(
        PREVIEW_PADDING_DIP,
        PREVIEW_PADDING_DIP,
        bounds.width() as f32 / scale,
        bounds.height() as f32 / scale,
    );
    let bitmap = renderer.upload(image)?;
    renderer.begin()?;
    paint_preview_card(renderer, &bitmap, card);
    renderer.end()?;
    *texture = Some(bitmap);
    if renderer.present(true) == Present::DeviceLost {
        return Err("device lost while showing a note preview".to_string());
    }
    Ok(())
}

/// Repaints the note at a new lean; the window is already placed and sized.
fn turn_note_frame(
    renderer: &mut Renderer,
    bitmap: &windows::Win32::Graphics::Direct2D::ID2D1Bitmap1,
    degrees: f32,
) -> Result<(), String> {
    draw_note(renderer, bitmap, note_card(), degrees, true)?;
    if renderer.present(true) == Present::DeviceLost {
        return Err("device lost while leaning the note".to_string());
    }
    Ok(())
}

/// Window procedure: never take focus, never erase.
///
/// SAFETY: this only reads message parameters; the window is destroyed on the
/// UI thread after the render thread has stopped.
unsafe extern "system" fn preview_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => {
            if (lparam.0 as u32 & 0xFFFF) == HTCLIENT && NOTE_MODE.load(Ordering::Acquire) {
                // SAFETY: the hand cursor is a system resource owned by the
                // process; SetCursor affects only this thread's cursor.
                unsafe {
                    if let Ok(cursor) = LoadCursorW(None, IDC_HAND) {
                        let _ = SetCursor(Some(cursor));
                    }
                }
                return LRESULT(1);
            }
        }
        WM_ERASEBKGND => return LRESULT(1),
        _ => {}
    }
    // SAFETY: default processing for messages we do not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
