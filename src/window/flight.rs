//! The flight and drop window: the full-screen card that flies from where the
//! screenshot was taken to its place on the line, and the card that falls when
//! it is taken down.
//!
//! The window is pooled: it is created once, hidden between animations and
//! shrunk to 2x2 so it gives its surface back. Its render thread owns the swap
//! chain and the timeline; the UI thread only places, shows and hides it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::render::{Brush, Present, Renderer};
use crate::spring::ease;
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;

const CLASS_NAME: PCWSTR = w!("PeglineFlight");
const WINDOW_TITLE: PCWSTR = w!("Pegline");

/// Flight duration, seconds.
const FLIGHT_SECONDS: f32 = 0.65;
/// Fade-out after landing, seconds.
const LAND_FADE_SECONDS: f32 = 0.16;
/// Drop duration, seconds.
const DROP_SECONDS: f32 = 0.55;
/// How far a dropped card falls, DIP.
const DROP_DISTANCE: f32 = 520.0;
/// Arc height of the flight, DIP.
const ARC_DIP: f32 = 30.0;
/// Chrome fades in between these two fractions of the flight.
const CHROME_FADE_START: f32 = 0.10;
const CHROME_FADE_END: f32 = 0.55;
/// Padding around the travel bounds: 60 DIP plus 5% of the longer side.
const PAD_DIP: f32 = 60.0;
const PAD_FRACTION: f32 = 0.05;
/// Smallest card that still gets a shadow, DIP.
const SHADOW_MAX_CARD: f32 = 300.0;

/// Messages from the render thread back to the UI thread.
#[derive(Clone, Copy, Debug)]
pub enum FlightEvent {
    /// The card reached the line; the real card can fade in.
    Landed {
        token: u64,
        /// Frames presented during the flight, for `probe`.
        frames: u32,
        /// Wall time of the flight in seconds, for `probe`.
        elapsed: f32,
        /// Largest single-frame draw time in milliseconds, for `probe`.
        draw_max_ms: f32,
    },
    /// The animation (including its fade) is over; the window can be hidden.
    Finished(u64),
}

enum FlightCommand {
    Fly {
        token: u64,
        generation: u64,
        image: ImageData,
        from: RectI,
        to: RectI,
        origin: RectI,
        dpi: u32,
    },
    Drop {
        token: u64,
        generation: u64,
        image: ImageData,
        rect: RectI,
        tilt: f32,
        origin: RectI,
        dpi: u32,
    },
    Stop,
}

/// HWND is a raw pointer; the window outlives the thread because `shutdown`
/// joins before destroying it.
struct SendHwnd(HWND);

// SAFETY: the render thread only draws into the window's composition target.
unsafe impl Send for SendHwnd {}

/// The pooled flight window.
pub struct Flight {
    hwnd: HWND,
    tx: Sender<FlightCommand>,
    join: Option<JoinHandle<()>>,
    visible: bool,
    /// Bumped for every animation; the render thread abandons the one it is
    /// running when the counter moves, so a new capture never waits behind an
    /// old flight (BEHAVIOR §3.8).
    interrupt: Arc<AtomicU64>,
    generation: u64,
}

impl Flight {
    /// Creates the hidden window and its render thread.
    pub fn create(on_event: Box<dyn Fn(FlightEvent) + Send>) -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(flight_wndproc))?;
        let instance = unsafe {
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .map_err(|e| format!("GetModuleHandleW: {e}"))?
        };
        // SAFETY: the class is registered; the window starts hidden and 2x2.
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
        .map_err(|e| format!("CreateWindowExW(flight): {e}"))?;
        // SAFETY: our own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let send_hwnd = SendHwnd(hwnd);
        let interrupt = Arc::new(AtomicU64::new(0));
        let thread_interrupt = Arc::clone(&interrupt);
        let join = thread::Builder::new()
            .name("pegline-flight".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    flight_thread(send_hwnd, rx, ready_tx, on_event, thread_interrupt)
                }));
                if outcome.is_err() {
                    logging::error("panic in the flight render thread");
                }
            })
            .map_err(|e| format!("spawn flight thread: {e}"))?;

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
                return Err("flight render thread died during setup".to_string());
            }
        }

        Ok(Self {
            hwnd,
            tx,
            join: Some(join),
            visible: false,
            interrupt,
            generation: 0,
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Places and shows the window for a flight and starts the animation.
    ///
    /// `from` is the capture rectangle in screen pixels, `to` the card's final
    /// rectangle. Returns false when the animation could not be started.
    pub fn fly(&mut self, token: u64, image: ImageData, from: RectI, to: RectI, dpi: u32) -> bool {
        let scale = dpi.max(96) as f32 / 96.0;
        let union = from.union(&to);
        let pad = (PAD_DIP * scale) as i32
            + (PAD_FRACTION * union.width().max(union.height()) as f32) as i32;
        let bounds = clamp_to_desktop(union.inflate(pad));
        if bounds.is_empty() {
            return false;
        }
        win32::place_window(self.hwnd, bounds, true);
        win32::show_no_activate(self.hwnd);
        self.visible = true;
        self.generation = self.generation.wrapping_add(1);
        self.interrupt.store(self.generation, Ordering::Relaxed);
        let _ = self.tx.send(FlightCommand::Fly {
            token,
            generation: self.generation,
            image,
            from,
            to,
            origin: RectI::new(bounds.left, bounds.top, bounds.right, bounds.bottom),
            dpi,
        });
        true
    }

    /// Drops a card from `rect` (screen pixels) and hides the window afterwards.
    pub fn drop_card(
        &mut self,
        token: u64,
        image: ImageData,
        rect: RectI,
        tilt: f32,
        dpi: u32,
    ) -> bool {
        let scale = dpi.max(96) as f32 / 96.0;
        let travel = (DROP_DISTANCE * scale) as i32;
        let bounds = clamp_to_desktop(
            RectI::new(rect.left, rect.top, rect.right, rect.bottom).inflate(travel + 64),
        );
        logging::trace(format!(
            "drop card rect={rect:?} travel={travel} bounds={bounds:?}"
        ));
        if bounds.is_empty() {
            logging::error("drop_card: empty bounds");
            return false;
        }
        win32::place_window(self.hwnd, bounds, true);
        win32::show_no_activate(self.hwnd);
        self.visible = true;
        self.generation = self.generation.wrapping_add(1);
        self.interrupt.store(self.generation, Ordering::Relaxed);
        let _ = self.tx.send(FlightCommand::Drop {
            token,
            generation: self.generation,
            image,
            rect,
            tilt,
            origin: bounds,
            dpi,
        });
        true
    }

    /// Hides the window and gives its surface back.
    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        win32::hide_window(self.hwnd);
        // 2x2 releases most of the swap chain's memory while the window waits.
        win32::place_window(self.hwnd, RectI::new(0, 0, 2, 2), false);
    }

    pub fn shutdown(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.tx.send(FlightCommand::Stop);
            let _ = join.join();
        }
        // SAFETY: the render thread has been joined.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn clamp_to_desktop(rect: RectI) -> RectI {
    let desktop = win32::virtual_screen();
    RectI::new(
        rect.left.max(desktop.left),
        rect.top.max(desktop.top),
        rect.right.min(desktop.right),
        rect.bottom.min(desktop.bottom),
    )
}

fn flight_thread(
    hwnd: SendHwnd,
    rx: Receiver<FlightCommand>,
    ready: Sender<Result<(), String>>,
    on_event: Box<dyn Fn(FlightEvent) + Send>,
    interrupt: Arc<AtomicU64>,
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

    while let Ok(command) = rx.recv() {
        match command {
            FlightCommand::Stop => break,
            FlightCommand::Fly {
                token,
                generation,
                image,
                from,
                to,
                origin,
                dpi,
            } => {
                if let Err(error) = run_flight(
                    &mut renderer,
                    hwnd.0,
                    token,
                    generation,
                    &interrupt,
                    &image,
                    from,
                    to,
                    origin,
                    dpi,
                    &on_event,
                ) {
                    logging::error(format!("flight: {error}"));
                }
            }
            FlightCommand::Drop {
                token,
                generation,
                image,
                rect,
                tilt,
                origin,
                dpi,
            } => {
                if let Err(error) = run_drop(
                    &mut renderer,
                    hwnd.0,
                    token,
                    generation,
                    &interrupt,
                    &image,
                    rect,
                    tilt,
                    origin,
                    dpi,
                    &on_event,
                ) {
                    logging::error(format!("drop: {error}"));
                }
            }
        }
    }
    logging::trace("flight render thread stopped");
}

#[allow(clippy::too_many_arguments)]
fn run_flight(
    renderer: &mut Renderer,
    hwnd: HWND,
    token: u64,
    generation: u64,
    interrupt: &AtomicU64,
    image: &ImageData,
    from: RectI,
    to: RectI,
    origin: RectI,
    dpi: u32,
    on_event: &dyn Fn(FlightEvent),
) -> Result<(), String> {
    let width_px = origin.width().max(1) as u32;
    let height_px = origin.height().max(1) as u32;
    renderer.resize(width_px, height_px, dpi)?;
    let scale = dpi.max(96) as f32 / 96.0;
    let texture = renderer.upload(image)?;
    let _ = hwnd;

    let start = win32::now_seconds();
    let mut landed = false;
    let mut frames = 0u32;
    let mut draw_max_ms = 0.0f32;
    loop {
        // A newer capture takes over: leave without reporting a landing; the app
        // already made the old card visible again.
        if interrupt.load(Ordering::Relaxed) != generation {
            return Ok(());
        }
        let elapsed = (win32::now_seconds() - start) as f32;
        let t = (elapsed / FLIGHT_SECONDS).clamp(0.0, 1.0);
        let k = ease::in_out_cubic(t);

        let from_w = from.width() as f32 / scale;
        let from_h = from.height() as f32 / scale;
        let to_w = to.width() as f32 / scale;
        let to_h = to.height() as f32 / scale;
        let width = from_w + (to_w - from_w) * k;
        let height = from_h + (to_h - from_h) * k;
        let from_cx = (from.left + from.right) as f32 * 0.5;
        let from_cy = (from.top + from.bottom) as f32 * 0.5;
        let to_cx = (to.left + to.right) as f32 * 0.5;
        let to_cy = (to.top + to.bottom) as f32 * 0.5;
        let cx = (from_cx + (to_cx - from_cx) * k - origin.left as f32) / scale;
        let cy = (from_cy + (to_cy - from_cy) * k - origin.top as f32) / scale
            - (std::f32::consts::PI * k).sin() * ARC_DIP;

        let card = RectF::from_xywh(cx - width * 0.5, cy - height * 0.5, width, height);
        let chrome = ease::smooth(t, CHROME_FADE_START, CHROME_FADE_END);
        if !landed && t >= 1.0 {
            landed = true;
            on_event(FlightEvent::Landed {
                token,
                frames,
                elapsed,
                draw_max_ms,
            });
        }
        let alpha = if landed {
            (1.0 - (elapsed - FLIGHT_SECONDS) / LAND_FADE_SECONDS).clamp(0.0, 1.0)
        } else {
            1.0
        };

        let draw_start = win32::now_seconds();
        renderer.begin()?;
        draw_flight_card(renderer, &texture, card, chrome, alpha);
        renderer.end()?;
        let draw_ms = ((win32::now_seconds() - draw_start) * 1000.0) as f32;
        draw_max_ms = draw_max_ms.max(draw_ms);
        frames += 1;
        if renderer.present(true) == Present::DeviceLost {
            return Err("device lost during flight".to_string());
        }

        if landed && elapsed >= FLIGHT_SECONDS + LAND_FADE_SECONDS {
            on_event(FlightEvent::Finished(token));
            return Ok(());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_drop(
    renderer: &mut Renderer,
    _hwnd: HWND,
    token: u64,
    generation: u64,
    interrupt: &AtomicU64,
    image: &ImageData,
    rect: RectI,
    tilt: f32,
    origin: RectI,
    dpi: u32,
    on_event: &dyn Fn(FlightEvent),
) -> Result<(), String> {
    let width_px = origin.width().max(1) as u32;
    let height_px = origin.height().max(1) as u32;
    renderer.resize(width_px, height_px, dpi)?;
    let scale = dpi.max(96) as f32 / 96.0;
    let texture = renderer.upload(image)?;

    let width = rect.width() as f32 / scale;
    let height = rect.height() as f32 / scale;
    let cx = (rect.left + rect.right) as f32 * 0.5 / scale - origin.left as f32 / scale;
    let top = rect.top as f32 / scale - origin.top as f32 / scale;
    logging::trace(format!(
        "drop render start size={width:.1}x{height:.1} origin={origin:?}"
    ));

    let start = win32::now_seconds();
    let mut frames = 0u32;
    loop {
        if interrupt.load(Ordering::Relaxed) != generation {
            logging::trace(format!("drop interrupted after {frames} frames"));
            return Ok(());
        }
        let elapsed = (win32::now_seconds() - start) as f32;
        let t = (elapsed / DROP_SECONDS).clamp(0.0, 1.0);
        let e = t * t * t;
        let center = (cx, top + height * 0.5 + DROP_DISTANCE * e);
        let angle = tilt + (tilt * 7.0 + 20.0) * e;
        let opacity = 1.0 - e;

        let transform = windows_numerics::Matrix3x2::translation(
            center.0 - width * 0.5,
            center.1 - height * 0.5,
        ) * windows_numerics::Matrix3x2::rotation_around(
            angle,
            windows_numerics::Vector2 {
                X: width * 0.5,
                Y: 0.0,
            },
        );
        renderer.begin()?;
        renderer.set_transform(transform);
        // `push_opacity` returns false for a fully opaque group because it skips
        // the layer; the card must still be drawn in that case.
        let pushed =
            renderer.push_opacity(RectF::from_xywh(0.0, 0.0, width, height), opacity.max(0.02));
        draw_flight_card(
            renderer,
            &texture,
            RectF::from_xywh(0.0, 0.0, width, height),
            1.0,
            1.0,
        );
        if pushed {
            renderer.pop_opacity();
        }
        renderer.reset_transform();
        renderer.end()?;
        if renderer.present(true) == Present::DeviceLost {
            return Err("device lost during drop".to_string());
        }
        if t >= 1.0 {
            logging::trace(format!("drop finished after {frames} frames"));
            on_event(FlightEvent::Finished(token));
            return Ok(());
        }
        frames += 1;
    }
}

/// Draws the card: photo, then the glass frame and pin at `chrome` opacity.
fn draw_flight_card(
    renderer: &mut Renderer,
    texture: &windows::Win32::Graphics::Direct2D::ID2D1Bitmap1,
    card: RectF,
    chrome: f32,
    alpha: f32,
) {
    let width_dip = card.width();
    let inset = layout::PHOTO_INSET * chrome;
    let photo = RectF::new(
        card.left + inset,
        card.top + inset,
        card.right - inset,
        card.bottom - inset,
    );
    let opacity_ok = if alpha < 0.999 {
        renderer.push_opacity(card.inflated(24.0), alpha)
    } else {
        false
    };

    if chrome > 0.01 {
        // The frame is the whole card; only the photo shrinks inward as the
        // chrome fades in. (Shrinking the frame too made it hide behind the
        // photo, so a landing card looked like a bare screenshot.)
        let frame = card;
        if width_dip <= SHADOW_MAX_CARD {
            renderer.draw_shadow(frame, 0.18 * chrome, false);
        }
        // Draw the frame even when the group is fully opaque: push_opacity
        // skips the layer (and returns false) at opacity 1.
        let pushed = renderer.push_opacity(frame.inflated(8.0), chrome);
        renderer.fill_round(frame, layout::CARD_RADIUS, Brush::Glass);
        renderer.stroke_card(frame, layout::CARD_RADIUS, 0.75);
        if pushed {
            renderer.pop_opacity();
        }
    }
    renderer.draw_photo(texture, photo, layout::PHOTO_RADIUS);
    if chrome > 0.01 {
        let pin_left = card.left + width_dip * 0.5 - layout::PIN_WIDTH * 0.5;
        let pushed = renderer.push_opacity(
            RectF::from_xywh(
                pin_left - 4.0,
                card.top - 12.0,
                layout::PIN_WIDTH + 8.0,
                40.0,
            ),
            chrome,
        );
        renderer.draw_pin(pin_left, card.top - layout::PIN_ABOVE);
        if pushed {
            renderer.pop_opacity();
        }
    }
    if opacity_ok {
        renderer.pop_opacity();
    }
}

/// Window procedure: never take focus, never erase.
///
/// SAFETY: this only reads message parameters; the window is destroyed on the
/// UI thread after the render thread has stopped.
unsafe extern "system" fn flight_wndproc(
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
