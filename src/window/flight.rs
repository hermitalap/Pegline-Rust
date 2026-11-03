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
use windows_numerics::{Matrix3x2, Vector2};

use crate::layout;
use crate::logging;
use crate::render::{Brush, Present, Renderer};
use crate::spring::{Spring, ease};
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;

const CLASS_NAME: PCWSTR = w!("PeglineFlight");
const WINDOW_TITLE: PCWSTR = w!("Pegline");

/// Flight duration, seconds.
const FLIGHT_SECONDS: f32 = 0.65;
/// Fade-out after landing, seconds. Only the flight window fades; the real
/// card is already fully opaque underneath it.
const LAND_FADE_SECONDS: f32 = 0.16;
/// Drop duration, seconds.
const DROP_SECONDS: f32 = 0.32;
/// How far a dropped card falls, DIP. The card is mostly faded by the time it
/// has travelled this far, so the fall reads as "gone" without crossing the
/// whole desktop.
const DROP_DISTANCE: f32 = 200.0;
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
    /// The card reached the line; the real card is opaque underneath it.
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
        /// Fixed card tilt, degrees, applied increasingly during the flight.
        tilt: f32,
        /// How far the row glides after landing, DIP (normally -87, or 0 for
        /// the first card). The flight window mirrors the same spring.
        slide_dip: f32,
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
    /// `from` is the capture rectangle in screen pixels, `to` the landing
    /// rectangle, `tilt` the card's fixed tilt in degrees and `slide_dip` the
    /// row glide after landing in DIP. Returns false when the animation could
    /// not be started.
    #[allow(clippy::too_many_arguments)]
    pub fn fly(
        &mut self,
        token: u64,
        image: ImageData,
        from: RectI,
        to: RectI,
        tilt: f32,
        slide_dip: f32,
        dpi: u32,
    ) -> bool {
        let scale = dpi.max(96) as f32 / 96.0;
        // The landing card follows the real card's row glide during the fade;
        // the window must already cover the final slot as well.
        let slide_px = (slide_dip * scale).round() as i32;
        let settle = RectI::new(to.left + slide_px, to.top, to.right + slide_px, to.bottom);
        let union = from.union(&to).union(&settle);
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
            tilt,
            slide_dip,
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

/// The motion the landed card performs in the scene: the row spring that
/// carries it from the landing point to its final slot, plus the 2.2° landing
/// sway. The flight window mirrors this while it fades out, so the fading
/// flight image and the ordinary card never drift apart.
struct LandingFollow {
    x: Spring,
    swing: Spring,
    nudge_from: f32,
    nudge_to: f32,
    nudge_t: f32,
}

impl LandingFollow {
    fn new(slide_dip: f32) -> Self {
        let mut x = Spring::new(0.55, 0.78, 0.0);
        x.target = slide_dip;
        Self {
            x,
            swing: Spring::new(1.0, 1.0, 0.0),
            nudge_from: 0.0,
            nudge_to: 2.2,
            nudge_t: 0.0,
        }
    }

    fn step(&mut self, dt: f32) {
        self.x.step(dt);
        if self.nudge_t >= 0.0 {
            self.nudge_t += dt / 0.3;
            if self.nudge_t >= 1.0 {
                self.nudge_t = -1.0;
                self.swing.value = self.nudge_to;
                self.swing.velocity = 0.0;
                self.swing.set_stiffness(38.0, 2.4);
            } else {
                self.swing.value = self.nudge_from
                    + (self.nudge_to - self.nudge_from) * ease::out_quad(self.nudge_t);
            }
        } else {
            self.swing.step(dt);
        }
    }
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
                tilt,
                slide_dip,
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
                    tilt,
                    slide_dip,
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
    tilt: f32,
    slide_dip: f32,
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
    let mut following = false;
    let mut last_elapsed = 0.0f32;
    let mut follow = LandingFollow::new(slide_dip);
    // The overlay caps its scene delta at 0.05 s; the mirror spring must use
    // the same cap or a stalled frame desynchronises the two copies.
    let max_follow_dt = 0.05;
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

        // After the hand-off the flight image mirrors the real card's row
        // spring and landing sway, so the two never drift apart.
        if landed && following {
            let dt = (elapsed - last_elapsed).clamp(0.0, max_follow_dt);
            follow.step(dt);
        }
        last_elapsed = elapsed;

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
        let x_offset = if landed { follow.x.value } else { 0.0 };
        let swing = if landed { follow.swing.value } else { 0.0 };
        let angle = tilt * k + swing;
        let card = RectF::from_xywh(
            cx - width * 0.5 + x_offset,
            cy - height * 0.5,
            width,
            height,
        );
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
        following = landed;
        let alpha = if landed {
            // The real card underneath is already fully opaque, so only this
            // copy has to dissolve. A smoothstep keeps the fade gentle; the
            // combined image stays at full opacity throughout.
            let progress = ((elapsed - FLIGHT_SECONDS) / LAND_FADE_SECONDS).clamp(0.0, 1.0);
            1.0 - ease::smooth(progress, 0.0, 1.0)
        } else {
            1.0
        };

        let rotation = Matrix3x2::rotation_around(
            angle,
            Vector2 {
                X: card.left + width * 0.5,
                // The pin top is the real card's rotation centre; rotating
                // about the frame top would make the landing card jump.
                Y: card.top - layout::CARD_OFFSET_BELOW_TOP,
            },
        );
        let draw_start = win32::now_seconds();
        renderer.begin()?;
        renderer.set_transform(rotation);
        // Once landed, the real card underneath owns the shadow; drawing the
        // flight shadow too would double its alpha and make it pulse as the
        // flight fades.
        draw_flight_card(renderer, &texture, card, chrome, alpha, !landed);
        renderer.reset_transform();
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
        let (transform, opacity) = drop_pose(t, cx - width * 0.5, top, width, tilt);
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
            true,
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

/// The transform and group opacity of a falling card at progress `t` (0..1).
///
/// The card only travels down and fades. Its fixed tilt is applied about its
/// own top centre (the pin) *before* the placement: rotating after the
/// placement, around a point in window space, is what used to swing the card
/// out to the left while it fell (`BEHAVIOR.md` §6.4).
fn drop_pose(t: f32, left: f32, top: f32, width: f32, tilt: f32) -> (Matrix3x2, f32) {
    let t = t.clamp(0.0, 1.0);
    let e = t * t * t;
    let transform = Matrix3x2::rotation_around(
        tilt,
        Vector2 {
            X: width * 0.5,
            Y: 0.0,
        },
    ) * Matrix3x2::translation(left, top + DROP_DISTANCE * e);
    (transform, 1.0 - e)
}

/// Draws the card: photo, then the glass frame and pin at `chrome` opacity.
fn draw_flight_card(
    renderer: &mut Renderer,
    texture: &windows::Win32::Graphics::Direct2D::ID2D1Bitmap1,
    card: RectF,
    chrome: f32,
    alpha: f32,
    shadow: bool,
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
        if shadow && width_dip <= SHADOW_MAX_CARD {
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
        // The card rotates about this point, exactly like the real card on the
        // line: the pin top, not the frame top (which is 14 DIP lower).
        let pin_left = card.left + width_dip * 0.5 - layout::PIN_WIDTH * 0.5;
        let pin_top = card.top - layout::CARD_OFFSET_BELOW_TOP;
        let pushed = renderer.push_opacity(
            RectF::from_xywh(
                pin_left - 4.0,
                pin_top - 12.0,
                layout::PIN_WIDTH + 8.0,
                40.0,
            ),
            chrome,
        );
        renderer.draw_pin(pin_left, pin_top);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Row-vector transform, the convention Direct2D and `Matrix3x2` use.
    fn apply(matrix: &Matrix3x2, x: f32, y: f32) -> (f32, f32) {
        (
            x * matrix.M11 + y * matrix.M21 + matrix.M31,
            x * matrix.M12 + y * matrix.M22 + matrix.M32,
        )
    }

    #[test]
    fn a_falling_card_only_moves_down() {
        let (left, top, width, tilt) = (123.0, 41.0, 144.0, 2.0);
        let cx = left + width * 0.5;
        for step in 0..=20 {
            let t = step as f32 / 20.0;
            let (transform, opacity) = drop_pose(t, left, top, width, tilt);
            // The pin is the rotation centre, so rotating never moves it and
            // the card falls in the column above its slot.
            let pin = apply(&transform, width * 0.5, 0.0);
            assert!((pin.0 - cx).abs() < 1e-3, "t={t} pin={pin:?}");
            let e = t * t * t;
            assert!((pin.1 - (top + DROP_DISTANCE * e)).abs() < 1e-3);
            assert!((opacity - (1.0 - e)).abs() < 1e-4);
        }
    }

    #[test]
    fn a_falling_card_is_gone_when_the_timeline_ends() {
        let (transform, opacity) = drop_pose(1.0, 0.0, 10.0, 144.0, 0.0);
        let pin = apply(&transform, 72.0, 0.0);
        assert!((pin.1 - (10.0 + DROP_DISTANCE)).abs() < 1e-3);
        assert!(opacity.abs() < 1e-4);
    }
}
