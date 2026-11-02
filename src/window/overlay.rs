//! The line strip window and its render thread.
//!
//! The window belongs to the UI thread (creation, placement, styles, show/hide,
//! click-through); the swap chain, Direct2D device and frame clock live on a
//! dedicated render thread. The only things crossing that boundary are
//! [`Command`]s one way and a [`FrameSnapshot`] the other way.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
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
use crate::scene::{Command, Scene};
use crate::types::{FrameSnapshot, RectI};
use crate::win32;
use crate::window;

const CLASS_NAME: PCWSTR = w!("PeglineOverlay");
const WINDOW_TITLE: PCWSTR = w!("Pegline");

/// HWND is a raw pointer; the window outlives the thread because `shutdown`
/// joins before destroying it.
struct SendHwnd(HWND);

// SAFETY: the thread never destroys or re-parents the window; it only uses the
// handle with DirectComposition, which is free-threaded.
unsafe impl Send for SendHwnd {}

/// The line window as seen by the UI thread.
pub struct Overlay {
    hwnd: HWND,
    tx: Sender<Command>,
    snapshot: Arc<Mutex<FrameSnapshot>>,
    join: Option<JoinHandle<()>>,
    width_px: u32,
    height_px: u32,
    scale: f32,
    click_through: bool,
    visible: bool,
}

impl Overlay {
    /// Creates the window on the calling (UI) thread and starts its renderer.
    pub fn create(work: RectI, dpi: u32) -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(overlay_wndproc))?;
        let width_px = work.width().max(1) as u32;
        let height_px = ((layout::PANEL_HEIGHT * dpi as f32 / 96.0).ceil() as i32).max(1) as u32;
        let instance = unsafe {
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .map_err(|e| format!("GetModuleHandleW: {e}"))?
        };
        // SAFETY: the class is registered; the window is never shown activated
        // and starts click-through.
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
                work.left,
                work.top,
                width_px as i32,
                height_px as i32,
                None,
                None,
                Some(windows::Win32::Foundation::HINSTANCE(instance.0)),
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW(overlay): {e}"))?;
        // The window starts hidden; the app shows it on the first reveal.
        // SAFETY: our own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        let snapshot = Arc::new(Mutex::new(FrameSnapshot::default()));
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_snapshot = Arc::clone(&snapshot);
        let send_hwnd = SendHwnd(hwnd);
        let join = thread::Builder::new()
            .name("pegline-render".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render_thread(
                        send_hwnd,
                        width_px,
                        height_px,
                        dpi,
                        rx,
                        thread_snapshot,
                        ready_tx,
                    )
                }));
                if outcome.is_err() {
                    logging::error("panic in the overlay render thread");
                }
            })
            .map_err(|e| format!("spawn render thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = join.join();
                // SAFETY: the window exists and no other thread uses it now.
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
                return Err("render thread died during setup".to_string());
            }
        }

        Ok(Self {
            hwnd,
            tx,
            snapshot,
            join: Some(join),
            width_px,
            height_px,
            scale: dpi as f32 / 96.0,
            click_through: true,
            visible: false,
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn snapshot(&self) -> Arc<Mutex<FrameSnapshot>> {
        Arc::clone(&self.snapshot)
    }

    pub fn send(&self, command: Command) {
        // A closed channel means the render thread died; the app keeps running
        // without the line rather than crashing.
        let _ = self.tx.send(command);
    }

    /// Paces the frame loop: `period` is the minimum seconds per frame, or 0 to
    /// follow the display (`fps=auto|60|120`, BEHAVIOR §12.3).
    pub fn set_frame_cap(&self, period: f64) {
        self.send(Command::SetFrameCap(period));
    }

    pub fn show(&mut self, topmost: bool) {
        if !self.visible {
            self.visible = true;
            if topmost {
                win32::set_topmost(self.hwnd);
            }
            win32::show_no_activate(self.hwnd);
            self.send(Command::SetVisible(true));
        }
        self.set_click_through(true);
    }

    pub fn hide(&mut self) {
        if self.visible {
            self.visible = false;
            win32::hide_window(self.hwnd);
            self.send(Command::SetVisible(false));
            self.send(Command::Reveal(false));
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Moves and resizes the window for a monitor's work area and DPI.
    pub fn place(&mut self, work: RectI, dpi: u32) {
        let width_px = work.width().max(1) as u32;
        let height_px = ((layout::PANEL_HEIGHT * dpi as f32 / 96.0).ceil() as i32).max(1) as u32;
        win32::place_window(
            self.hwnd,
            RectI::new(
                work.left,
                work.top,
                work.left + width_px as i32,
                work.top + height_px as i32,
            ),
            true,
        );
        self.width_px = width_px;
        self.height_px = height_px;
        self.scale = dpi as f32 / 96.0;
        self.send(Command::Layout {
            width_px,
            height_px,
            dpi,
        });
    }

    pub fn set_click_through(&mut self, enabled: bool) {
        if self.click_through == enabled {
            return;
        }
        self.click_through = enabled;
        win32::set_click_through(self.hwnd, enabled);
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn width_px(&self) -> u32 {
        self.width_px
    }

    pub fn height_px(&self) -> u32 {
        self.height_px
    }

    /// Stops the render thread and destroys the window. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.tx.send(Command::Stop);
            let _ = join.join();
        }
        // SAFETY: the render thread has been joined, so nothing else uses the
        // window; destroying it releases the DirectComposition target.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn render_thread(
    hwnd: SendHwnd,
    width_px: u32,
    height_px: u32,
    dpi: u32,
    rx: Receiver<Command>,
    snapshot: Arc<Mutex<FrameSnapshot>>,
    ready: Sender<Result<(), String>>,
) {
    let _com = match window::ComGuard::init(windows::Win32::System::Com::COINIT_MULTITHREADED) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let mut renderer = match crate::render::Renderer::new(hwnd.0, width_px, height_px, dpi) {
        Ok(renderer) => renderer,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let mut scene = Scene::new(
        width_px,
        dpi,
        renderer.actions_layout(),
        renderer.copied_height(),
        Arc::clone(&snapshot),
    );
    logging::trace(format!(
        "overlay renderer ready: {}x{} dpi={} scale={:.2}",
        renderer.width_px,
        renderer.height_px,
        renderer.dpi,
        renderer.scale()
    ));

    if ready.send(Ok(())).is_err() {
        return;
    }

    let mut visible = false;
    let mut needs_frame = false;
    let mut frame_cap = 0.0f64;
    let mut last_present = 0.0f64;
    let mut last = win32::now_seconds();
    let mut lost = 0u32;

    'outer: loop {
        // Wait for work when the line is hidden or nothing is moving: this is
        // what makes idle CPU exactly zero. While the line is visible with
        // cards on it, wake up for the next breeze instead of blocking forever.
        if !visible || !needs_frame {
            let wake = if visible {
                scene.next_wake_in(win32::now_seconds())
            } else {
                None
            };
            match wake {
                Some(seconds) => {
                    match rx.recv_timeout(std::time::Duration::from_secs_f64(seconds)) {
                        Ok(command) => {
                            if !handle(
                                command,
                                &mut scene,
                                &mut renderer,
                                &mut visible,
                                &mut frame_cap,
                            ) {
                                break 'outer;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match rx.recv() {
                    Ok(command) => {
                        if !handle(
                            command,
                            &mut scene,
                            &mut renderer,
                            &mut visible,
                            &mut frame_cap,
                        ) {
                            break 'outer;
                        }
                    }
                    Err(_) => break,
                },
            }
        }
        let mut stop = false;
        while let Ok(command) = rx.try_recv() {
            if !handle(
                command,
                &mut scene,
                &mut renderer,
                &mut visible,
                &mut frame_cap,
            ) {
                stop = true;
                break;
            }
        }
        if stop {
            break;
        }
        if !visible {
            needs_frame = false;
            continue;
        }

        // Optional frame cap: sleep out the rest of the period, then let
        // `Present(1, 0)` block on the next vertical blank.
        if frame_cap > 0.0 {
            let since = win32::now_seconds() - last_present;
            if since < frame_cap {
                std::thread::sleep(std::time::Duration::from_secs_f64(frame_cap - since));
            }
        }

        let now = win32::now_seconds();
        let dt = ((now - last) as f32).clamp(0.0, 0.05);
        last = now;
        let animating = scene.update(dt, now);

        if let Err(error) = renderer.begin() {
            logging::error(format!("overlay begin: {error}"));
            lost += 1;
        } else {
            scene.draw(&mut renderer);
            if let Err(error) = renderer.end() {
                logging::error(format!("overlay end: {error}"));
                lost += 1;
            } else {
                match renderer.present(true) {
                    crate::render::Present::Ok => last_present = win32::now_seconds(),
                    crate::render::Present::Retry => {}
                    crate::render::Present::DeviceLost => {
                        logging::error("overlay device lost");
                        lost += 1;
                    }
                }
            }
        }
        needs_frame = animating;

        if lost > 0 {
            // Rebuild the device; the scene keeps its layout and springs but
            // loses its textures, so the UI is asked to resend the cards.
            if lost > 3 {
                logging::error("overlay renderer lost too many times; stopping");
                break;
            }
            match crate::render::Renderer::new(hwnd.0, width_px, height_px, dpi) {
                Ok(rebuilt) => {
                    renderer = rebuilt;
                    scene = Scene::new(
                        renderer.width_px,
                        dpi,
                        renderer.actions_layout(),
                        renderer.copied_height(),
                        Arc::clone(&snapshot),
                    );
                    logging::error("overlay renderer rebuilt; cards need a reload");
                    lost = 0;
                    needs_frame = true;
                }
                Err(error) => {
                    logging::error(format!("overlay rebuild failed: {error}"));
                    break;
                }
            }
        }
    }
    logging::trace("overlay render thread stopped");
}

/// Applies one command. Returns false when the thread should stop.
fn handle(
    command: Command,
    scene: &mut Scene,
    renderer: &mut crate::render::Renderer,
    visible: &mut bool,
    frame_cap: &mut f64,
) -> bool {
    match command {
        Command::Stop => false,
        Command::SetVisible(on) => {
            *visible = on;
            scene.apply(Command::SetVisible(on), renderer, win32::now_seconds());
            true
        }
        Command::SetFrameCap(period) => {
            *frame_cap = period.max(0.0);
            true
        }
        other => {
            let now = win32::now_seconds();
            scene.apply(other, renderer, now);
            true
        }
    }
}
/// Window procedure for the overlay: never take focus, never erase.
///
/// SAFETY: the window is created and destroyed on the UI thread; this procedure
/// only reads message parameters.
unsafe extern "system" fn overlay_wndproc(
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
    // A click on the strip must never activate it.
    // SAFETY: default processing for messages we do not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
