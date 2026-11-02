//! The hover-preview window: a large copy of the hovered photo, shown after a
//! stable 0.35 s hover (BEHAVIOR §7).
//!
//! It is a second transparent DirectComposition window like the flight window.
//! The render thread owns the swap chain, the placement and the show/hide, so
//! the first thing the user sees is the finished frame: the window is resized
//! and drawn while still hidden, then shown without activation. When the
//! preview is dismissed the bitmap and the surface are released, which is what
//! keeps `probe`'s private memory flat after a preview (BEHAVIOR §6.7).

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, MA_NOACTIVATE, SW_HIDE, ShowWindow,
    WM_ERASEBKGND, WM_MOUSEACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::layout;
use crate::logging;
use crate::preview as geometry;
use crate::render::{Brush, Present, Renderer};
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;

const CLASS_NAME: PCWSTR = w!("PeglinePreview");
const WINDOW_TITLE: PCWSTR = w!("Pegline preview");

enum PreviewCommand {
    Show {
        image: ImageData,
        bounds: RectI,
        dpi: u32,
    },
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
        let join = thread::Builder::new()
            .name("pegline-preview".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    preview_thread(send_hwnd, rx, ready_tx)
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
        let _ = self.tx.send(PreviewCommand::Show { image, bounds, dpi });
    }

    /// Hides the preview and releases its surface.
    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
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

fn preview_thread(hwnd: SendHwnd, rx: Receiver<PreviewCommand>, ready: Sender<Result<(), String>>) {
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
        }
    }
    logging::trace("preview render thread stopped");
}

fn show_frame(
    renderer: &mut Renderer,
    hwnd: HWND,
    image: &ImageData,
    bounds: RectI,
    dpi: u32,
    texture: &mut Option<windows::Win32::Graphics::Direct2D::ID2D1Bitmap1>,
) -> Result<(), String> {
    let width = bounds.width().max(1) as u32;
    let height = bounds.height().max(1) as u32;
    win32::place_window(hwnd, bounds, true);
    renderer.resize(width, height, dpi)?;
    let bitmap = renderer.upload(image)?;
    *texture = Some(bitmap);

    let scale = dpi.max(96) as f32 / 96.0;
    let frame = RectF::from_xywh(0.0, 0.0, width as f32 / scale, height as f32 / scale);
    let inset = geometry::INSET_DIP;
    let photo = RectF::new(
        frame.left + inset,
        frame.top + inset,
        frame.right - inset,
        frame.bottom - inset,
    );
    renderer.begin()?;
    renderer.fill_round(frame, layout::CARD_RADIUS, Brush::Paper);
    if let Some(bitmap) = texture.as_ref() {
        renderer.draw_photo(bitmap, photo, layout::CARD_RADIUS - 4.0);
    }
    renderer.stroke_card(frame, layout::CARD_RADIUS, 1.0);
    renderer.end()?;
    if renderer.present(true) == Present::DeviceLost {
        return Err("device lost while showing a preview".to_string());
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
        WM_ERASEBKGND => return LRESULT(1),
        _ => {}
    }
    // SAFETY: default processing for messages we do not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
