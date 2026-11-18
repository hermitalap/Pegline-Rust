//! The flight and drop window: the full-screen card that flies from where the
//! screenshot was taken to its place on the line, and the card that falls when
//! it is taken down.
//!
//! The window is pooled: it is created once, hidden between animations and
//! shrunk to 2x2 so it gives its surface back. Its render thread owns the swap
//! chain and the timeline; the UI thread only places, shows and hides it.

mod paint;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, MA_NOACTIVATE, SW_HIDE, ShowWindow, WM_ERASEBKGND,
    WM_MOUSEACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::capture::wic;
use crate::layout;
use crate::logging;
use crate::render::{Present, Renderer};
use crate::spring::{Spring, ease};
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;
use crate::window::SendHwnd;
use paint::{draw_flight_card, drop_pose, paint, paint_pre_roll};

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
/// The discarded note's tumble: how long it takes to shrink, how small it
/// gets, and the gravity that carries it off the screen.
const TUMBLE_SHRINK_SECONDS: f32 = 0.30;
const TUMBLE_SHRINK: f32 = 0.45;
const TUMBLE_GRAVITY: f32 = 2600.0;
/// Safety cap for the tumble, seconds; it normally ends when the note leaves
/// the window.
const TUMBLE_SECONDS: f32 = 2.0;
/// Arc height of the flight, DIP.
const ARC_DIP: f32 = 30.0;
/// Chrome fades in between these two fractions of the flight.
const CHROME_FADE_START: f32 = 0.10;
const CHROME_FADE_END: f32 = 0.55;
/// Where a note's glass frame leaves and rejoins the card, as a fraction of the
/// *path* (`k`, the eased progress), not of the clock: the frame belongs to the
/// line, so it has to be gone by the time the card is a third of the way to the
/// editor and back only once it is two thirds of the way home.
///
/// On the 0.65 s CSS-`ease` flight `k = 1/3` is reached at `t ≈ 0.215`
/// (0.14 s) and `k = 2/3` at `t ≈ 0.39` (0.25 s).
const NOTE_CHROME_LEAVE_END: f32 = 1.0 / 3.0;
const NOTE_CHROME_ARRIVE_START: f32 = 2.0 / 3.0;
/// How long a note card takes to dissolve into the editor view at the start of
/// its flight to the centre.
const ENTRY_CROSSFADE_SECONDS: f32 = 0.12;
/// Angular spring that brings a tilted hanging card upright before it flies to
/// an editor. A short, slightly under-damped spring so the turn accelerates and
/// eases out instead of snapping.
const ENTRY_SETTLE_RESPONSE: f32 = 0.16;
const ENTRY_SETTLE_DAMPING: f32 = 0.9;
/// Residual tilt that lets the annotation flight start moving, degrees. The
/// spring is far inside one pixel by then, and whatever is left finishes
/// during the travel instead of delaying it (O1).
const SETTLE_READY_DEG: f32 = 0.5;
/// Padding around the travel bounds: 60 DIP plus 5% of the longer side.
const PAD_DIP: f32 = 60.0;
const PAD_FRACTION: f32 = 0.05;
/// Largest frame delta the animation integrates, seconds. The overlay caps its
/// own scene delta at 0.05 s and the mirror springs that follow the landed card
/// must use the same cap, or one stalled frame desynchronises the two copies.
const MAX_FRAME_DT: f32 = 0.05;

/// How the clothespin behaves during an editor flight.
///
/// A pin belongs to the line, not to the editor card. Showing it for the whole
/// trip leaves a pin floating over the editor and makes it pop away only after
/// the flight lands, so editor flights either hide it or fade it at the end
/// that touches the rope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinMode {
    /// No pin at any point (a brand-new note flying from the hover card).
    Hidden,
    /// Pin from the first frame to the last (annotation editor and captures).
    Full,
    /// The card is leaving the line: the pin is present at take-off and fades
    /// out as the card detaches.
    Leave,
    /// The card is coming back: no pin while it travels, then it fades in as
    /// the card reaches its slot.
    Arrive,
}

/// How the glass frame around the card behaves during a flight.
///
/// The frame is the line's card, not the editor's paper: a note editor draws a
/// 22 DIP paper with no 4 DIP glass border, so a note's frame has to leave the
/// card on the way in and rejoin it on the way out. A capture and an annotation
/// flight both start and end on a real decorated card, so their frame is
/// opaque for the whole trip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChromeMode {
    /// Fade in over the first part of the flight (a capture arriving on the
    /// line).
    FadeIn,
    /// Opaque from the first frame to the last (annotation flights).
    Full,
    /// The card is leaving the line for the note editor: the frame is there at
    /// take-off and gone by [`NOTE_CHROME_LEAVE_END`] of the path.
    Leave,
    /// The card is coming home from the note editor: no frame while it
    /// travels, then it fades in over the last part of the path.
    Arrive,
}

/// The picture a flight carries.
///
/// A capture already owns its decoded flight image. An annotation flight
/// starts from a card whose bitmap is only card-sized, so the flight thread
/// decodes the file once at a resolution that survives the enlargement; the
/// card bitmap is the fallback when that fails.
pub enum FlightImage {
    Ready(ImageData),
    File {
        path: PathBuf,
        max_edge: u32,
        fallback: ImageData,
    },
}

impl FlightImage {
    /// Resolves the file-based variant on the flight thread, never on the UI
    /// thread (WIC decoding is not allowed there).
    fn resolve(self) -> ImageData {
        match self {
            Self::Ready(image) => image,
            Self::File {
                path,
                max_edge,
                fallback,
            } => match wic::decode_scaled(&path, max_edge) {
                Ok(image) => image,
                Err(error) => {
                    logging::error(format!(
                        "flight decode {} at {max_edge}px: {error}",
                        path.display()
                    ));
                    fallback
                }
            },
        }
    }
}

/// Messages from the render thread back to the UI thread.
#[derive(Clone, Copy, Debug)]
pub enum FlightEvent {
    /// The first frame is on screen, so whatever the flight took over from can
    /// be hidden without a gap.
    Started(u64),
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
        image: FlightImage,
        /// The card's own bitmap, shown at the start and dissolved into
        /// `image` before the travel begins (a note opening its editor).
        start_image: Option<ImageData>,
        /// The card's visual angle when the flight starts, degrees. When set,
        /// the flight first turns the card upright with an angular spring so a
        /// tilted hanging picture does not snap before flying.
        settle_angle: Option<f32>,
        /// Run the settle/cross-fade during the travel instead of first waiting
        /// at the slot.
        blend_entry: bool,
        /// How the glass frame behaves. The frame belongs to the line's card,
        /// so a note flight to or from the editor fades it along the path.
        chrome: ChromeMode,
        from: RectI,
        to: RectI,
        /// Fixed card tilt, degrees, applied increasingly during the flight.
        tilt: f32,
        /// How far the row glides after landing, DIP (normally -87, or 0 for
        /// the first card). The flight window mirrors the same spring.
        slide_dip: f32,
        /// How the clothespin behaves. A pin belongs to the line, not to the
        /// editor card, so editor flights hide it or fade it at the rope end.
        pin: PinMode,
        origin: RectI,
        dpi: u32,
    },
    Drop {
        token: u64,
        generation: u64,
        image: ImageData,
        rect: RectI,
        /// The card's visual angle on the line, degrees (swing + tilt + lean).
        angle: f32,
        /// The card's press/lift scale when it left the line.
        scale: f32,
        origin: RectI,
        dpi: u32,
    },
    /// A discarded note: it shrinks, spins and falls off the screen
    /// (NOTE-EDITOR.md §5.5).
    Tumble {
        token: u64,
        generation: u64,
        image: ImageData,
        /// The paper rectangle it starts from, screen pixels.
        from: RectI,
        /// Angular velocity in degrees per second; the sign is the spin
        /// direction.
        spin: f32,
        /// Initial velocity in DIP per second; gravity does the rest.
        velocity: (f32, f32),
        origin: RectI,
        dpi: u32,
    },
    Stop,
}

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
        // The window starts hidden and 2x2; the render thread places it.
        let hwnd = window::create_window(
            CLASS_NAME,
            WINDOW_TITLE,
            WS_POPUP,
            WS_EX_NOREDIRECTIONBITMAP
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE
                | WS_EX_TOPMOST
                | WS_EX_LAYERED
                | WS_EX_TRANSPARENT,
            RectI::new(0, 0, 2, 2),
            Some(flight_wndproc),
            "flight",
        )?;
        // SAFETY: our own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        let (tx, rx) = mpsc::channel();
        let send_hwnd = SendHwnd(hwnd);
        let interrupt = Arc::new(AtomicU64::new(0));
        let thread_interrupt = Arc::clone(&interrupt);
        let (join, ready_rx) =
            window::spawn_render_thread("pegline-flight", "flight", move |ready| {
                flight_thread(send_hwnd, rx, ready, on_event, thread_interrupt);
            })?;
        let join = window::await_render_ready(&ready_rx, join, || {}, hwnd, "flight")?;

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
    /// row glide after landing in DIP. `pin` says whether the clothespin is
    /// drawn (false for a new note on its way to the editor, which is not
    /// hanging on the line). Returns false when the animation could not be
    /// started.
    #[allow(clippy::too_many_arguments)]
    pub fn fly(
        &mut self,
        token: u64,
        image: ImageData,
        from: RectI,
        to: RectI,
        tilt: f32,
        slide_dip: f32,
        pin: PinMode,
        dpi: u32,
    ) -> bool {
        self.start_flight(
            token,
            FlightImage::Ready(image),
            None,
            None,
            ChromeMode::FadeIn,
            false,
            from,
            to,
            tilt,
            slide_dip,
            pin,
            dpi,
        )
    }

    /// Flies a card back onto the line with its glass frame and pin already
    /// drawn from the first frame. The editor behind it is still visible, so a
    /// chrome fade would read as the frame blinking away.
    ///
    /// `chrome` says how the frame behaves: an annotation flight keeps it
    /// opaque (`Full`, the editor's card carries the same frame), a note flight
    /// fades it in over the last part of the path (`Arrive`), because the note
    /// editor's paper has no glass border to hand over to.
    #[allow(clippy::too_many_arguments)]
    pub fn fly_decorated(
        &mut self,
        token: u64,
        image: ImageData,
        from: RectI,
        to: RectI,
        tilt: f32,
        slide_dip: f32,
        pin: PinMode,
        chrome: ChromeMode,
        dpi: u32,
    ) -> bool {
        self.start_flight(
            token,
            FlightImage::Ready(image),
            None,
            None,
            chrome,
            false,
            from,
            to,
            tilt,
            slide_dip,
            pin,
            dpi,
        )
    }

    /// Flies a hanging card into an editor.
    ///
    /// `settle_angle` is the card's current tilt; the flight turns it upright
    /// with an angular spring. `start_image` (the card's own bitmap) is
    /// dissolved into `image` (the editor's paper) at the start, so the
    /// enlarged text never flies across the screen. `blend` runs the
    /// cross-fade and the angular settle *during* the travel (note editor,
    /// where waiting would read as input lag); otherwise they happen at the
    /// slot first (annotation editor, which must be upright before it grows).
    /// `image` may be decoded from the file on the flight thread.
    ///
    /// `chrome` says how the glass frame behaves: a note flight leaves it on
    /// the line (`Leave`), an annotation flight keeps it (`Full`).
    #[allow(clippy::too_many_arguments)]
    pub fn fly_entry(
        &mut self,
        token: u64,
        image: FlightImage,
        start_image: Option<ImageData>,
        from: RectI,
        to: RectI,
        settle_angle: f32,
        pin: PinMode,
        chrome: ChromeMode,
        dpi: u32,
        blend: bool,
    ) -> bool {
        self.start_flight(
            token,
            image,
            start_image,
            Some(settle_angle),
            chrome,
            blend,
            from,
            to,
            0.0,
            0.0,
            pin,
            dpi,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_flight(
        &mut self,
        token: u64,
        image: FlightImage,
        start_image: Option<ImageData>,
        settle_angle: Option<f32>,
        chrome: ChromeMode,
        blend_entry: bool,
        from: RectI,
        to: RectI,
        tilt: f32,
        slide_dip: f32,
        pin: PinMode,
        dpi: u32,
    ) -> bool {
        let scale = dpi as f32 / 96.0;
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
            start_image,
            settle_angle,
            blend_entry,
            chrome,
            from,
            to,
            tilt,
            slide_dip,
            pin,
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
        angle: f32,
        scale: f32,
        dpi: u32,
    ) -> bool {
        let dpi_scale = dpi as f32 / 96.0;
        let travel = (DROP_DISTANCE * dpi_scale) as i32;
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
            angle,
            scale,
            origin: bounds,
            dpi,
        });
        true
    }

    /// Throws a discarded note away: it shrinks, spins in `spin`'s direction
    /// and falls with gravity until it leaves the screen.
    ///
    /// `from` is the paper rectangle in screen pixels, `spin` degrees per second
    /// and `velocity` the initial velocity in DIP per second. Returns false when
    /// the animation could not be started.
    pub fn tumble(
        &mut self,
        token: u64,
        image: ImageData,
        from: RectI,
        spin: f32,
        velocity: (f32, f32),
        dpi: u32,
    ) -> bool {
        let scale = dpi as f32 / 96.0;
        let pad = (PAD_DIP * scale) as i32;
        // The window has to hold the whole fall: the horizontal reach of the
        // throw on both sides, and the desktop bottom for the drop.
        let reach = (velocity.0.abs() * TUMBLE_SECONDS * scale).ceil() as i32 + pad;
        let desktop = win32::virtual_screen();
        let bounds = clamp_to_desktop(RectI::new(
            from.left - reach,
            from.top - pad,
            from.right + reach,
            desktop.bottom,
        ));
        logging::trace(format!(
            "tumble from={from:?} spin={spin:.0} velocity={velocity:?} bounds={bounds:?}"
        ));
        if bounds.is_empty() {
            logging::error("tumble: empty bounds");
            return false;
        }
        win32::place_window(self.hwnd, bounds, true);
        win32::show_no_activate(self.hwnd);
        self.visible = true;
        self.generation = self.generation.wrapping_add(1);
        self.interrupt.store(self.generation, Ordering::Relaxed);
        let _ = self.tx.send(FlightCommand::Tumble {
            token,
            generation: self.generation,
            image,
            from,
            spin,
            velocity,
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
        window::stop_render_thread(self.join.take(), || {
            let _ = self.tx.send(FlightCommand::Stop);
        });
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
                // The same curve `Scene::Card::step_swing` uses for the landing
                // nudge (F4): a mismatch would show as the flight copy and the
                // real card swinging apart during the 0.16 s fade.
                self.swing.value = self.nudge_from
                    + (self.nudge_to - self.nudge_from) * ease::smoothstep(self.nudge_t);
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
                start_image,
                settle_angle,
                blend_entry,
                chrome,
                from,
                to,
                tilt,
                slide_dip,
                pin,
                origin,
                dpi,
            } => {
                let request = FlightRequest {
                    image: image.resolve(),
                    start_image,
                    settle_angle,
                    blend_entry,
                    chrome,
                    from,
                    to,
                    tilt,
                    slide_dip,
                    pin,
                    origin,
                    dpi,
                };
                if let Err(error) = run_flight(
                    &mut renderer,
                    token,
                    generation,
                    &interrupt,
                    request,
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
                angle,
                scale,
                origin,
                dpi,
            } => {
                if let Err(error) = run_drop(
                    &mut renderer,
                    token,
                    generation,
                    &interrupt,
                    &image,
                    rect,
                    angle,
                    scale,
                    origin,
                    dpi,
                    &on_event,
                ) {
                    logging::error(format!("drop: {error}"));
                }
            }
            FlightCommand::Tumble {
                token,
                generation,
                image,
                from,
                spin,
                velocity,
                origin,
                dpi,
            } => {
                if let Err(error) = run_tumble(
                    &mut renderer,
                    token,
                    generation,
                    &interrupt,
                    &image,
                    from,
                    spin,
                    velocity,
                    origin,
                    dpi,
                    &on_event,
                ) {
                    logging::error(format!("tumble: {error}"));
                }
            }
        }
    }
    logging::trace("flight render thread stopped");
}

/// What one flight should do: the image it carries, the rectangles it travels
/// between and how its card is decorated.
///
/// Grouped into one value because the setup wants all of it at once; a dozen
/// positional parameters made the call unreadable.
struct FlightRequest {
    /// The picture that travels.
    image: ImageData,
    /// The card's own bitmap, shown at the start and dissolved into `image`
    /// (a note opening its editor).
    start_image: Option<ImageData>,
    /// The card's visual angle when the flight starts, degrees. When set, the
    /// flight first turns the card upright with an angular spring so a tilted
    /// hanging picture does not snap before flying.
    settle_angle: Option<f32>,
    /// Run the settle/cross-fade during the travel instead of first waiting at
    /// the slot.
    blend_entry: bool,
    /// How the glass frame behaves. The frame belongs to the line's card, so a
    /// note flight to or from the editor fades it along the path.
    chrome: ChromeMode,
    /// The card's starting rect, screen pixels.
    from: RectI,
    /// The rect it lands on, screen pixels.
    to: RectI,
    /// Fixed card tilt, degrees, applied increasingly during the flight.
    tilt: f32,
    /// How far the row glides after landing, DIP (normally -87, or 0 for the
    /// first card). The flight window mirrors the same spring.
    slide_dip: f32,
    /// How the clothespin behaves. A pin belongs to the line, not the editor
    /// card, so editor flights hide it or fade it at the rope end.
    pin: PinMode,
    /// The window rect, screen pixels: everything drawn is relative to it.
    origin: RectI,
    dpi: u32,
}

/// The images one flight draws with.
struct FlightTextures {
    /// The picture that travels.
    card: ID2D1Bitmap1,
    /// The card's own bitmap, dissolved into `card` at the start.
    start: Option<ID2D1Bitmap1>,
}

/// Everything one flight needs to know, built once before the frame loop.
///
/// The textures live in [`FlightTextures`] so this half holds no COM and
/// [`Self::step`] can be driven from a test without a device.
struct FlightPlan {
    /// The angular spring that turns a tilted card upright, when the flight
    /// starts from a tilt at all.
    settle: Option<Spring>,
    /// The row spring and landing sway the flight image mirrors after the
    /// hand-off.
    follow: LandingFollow,
    /// The card at its slot, where the annotation pre-roll turns it upright.
    pre_roll_card: RectF,
    from: RectI,
    to: RectI,
    /// The window rect: everything drawn is relative to it.
    origin: RectI,
    /// Device pixels per DIP.
    scale: f32,
    /// Fixed card tilt, degrees, applied increasingly during the flight.
    tilt: f32,
    pin: PinMode,
    /// How the glass frame behaves along the path.
    chrome: ChromeMode,
    /// The flight starts from a decorated card: no landing sway, and the pin
    /// follows `pin` only.
    entry: bool,
    /// The settle and the cross-fade run during the travel, not at the slot.
    blend: bool,
    /// A card bitmap is uploaded and dissolved into the travel image.
    has_start_image: bool,
    /// The annotation pre-roll is still running: the card turns upright at its
    /// slot before it moves.
    pre_roll: bool,
    /// The travel has reached the destination; the landing fade is running.
    landed: bool,
    /// The previous frame's travel time, so the follow springs advance by the
    /// same delta whether the flight is interpolated or handed over.
    last_elapsed: f32,
}

/// One travel frame, ready to draw.
struct FlightPose {
    /// The card rectangle in window DIP.
    card: RectF,
    /// Rotation about the pin top, as [`Renderer::set_transform`] wants it.
    transform: Matrix3x2,
    /// The glass frame's opacity: 0 for a bare screenshot, 1 for a card that
    /// is already decorated.
    chrome: f32,
    /// How far the picture is inset inside the card, DIP. A capture's photo
    /// grows to fill the card as the frame fades in; a note's paper keeps the
    /// line's inset so its rect never jumps.
    inset: f32,
    /// The picture's corner radius, DIP. A note flight carries the paper from
    /// the line card's radius to the note editor's sheet and back.
    radius: f32,
    /// The flight image's own opacity, which fades out after the hand-off.
    alpha: f32,
    /// Whether this copy still draws the shadow.
    shadow: bool,
    /// The clothespin's opacity.
    pin: f32,
    /// Progress into the start-image dissolve, 0..1; 1 when there is nothing
    /// left to dissolve.
    crossfade: f32,
    /// The travel reached the destination on this frame: report `Landed`.
    landed_now: bool,
    /// The landing fade is over: report `Finished`.
    finished: bool,
}

/// One pre-roll frame: the card at its slot, turning upright and dissolving
/// into the editor image before the travel starts.
struct PreRollPose {
    /// The card rectangle at the slot, in window DIP.
    card: RectF,
    /// Rotation about the pin top, as [`Renderer::set_transform`] wants it.
    transform: Matrix3x2,
    /// Progress into the start-image dissolve, 0..1.
    crossfade: f32,
    /// The clothespin's opacity; the card has not left the line yet.
    pin: f32,
    /// The card is upright and the dissolve is done: the travel can start.
    ready: bool,
}

/// Resizes the swap chain to the flight's bounding box, uploads its images and
/// builds the plan the frame loop steps through.
fn prepare(
    renderer: &mut Renderer,
    request: FlightRequest,
) -> Result<(FlightPlan, FlightTextures), String> {
    let origin = request.origin;
    renderer.resize(
        origin.width().max(1) as u32,
        origin.height().max(1) as u32,
        request.dpi,
    )?;
    let scale = request.dpi as f32 / 96.0;
    let texture = renderer.upload(&request.image)?;
    let start_texture = request
        .start_image
        .as_ref()
        .map(|image| renderer.upload(image))
        .transpose()?;

    let entry = request.settle_angle.is_some() || start_texture.is_some();
    // Notes start moving at once: the cross-fade and the settle run during the
    // first part of the flight instead of delaying it. The annotation editor
    // still waits for the card to be upright before it grows.
    let blend = request.blend_entry && entry;
    let settle = request.settle_angle.map(|angle| {
        let mut spring = Spring::new(ENTRY_SETTLE_RESPONSE, ENTRY_SETTLE_DAMPING, angle);
        spring.target = 0.0;
        spring
    });
    let pre_roll_card = {
        let width = request.from.width() as f32 / scale;
        let height = request.from.height() as f32 / scale;
        let cx = (request.from.left + request.from.right) as f32 * 0.5 / scale
            - origin.left as f32 / scale;
        let cy = (request.from.top + request.from.bottom) as f32 * 0.5 / scale
            - origin.top as f32 / scale;
        RectF::from_xywh(cx - width * 0.5, cy - height * 0.5, width, height)
    };
    let plan = FlightPlan {
        settle,
        follow: LandingFollow::new(request.slide_dip),
        pre_roll_card,
        from: request.from,
        to: request.to,
        origin,
        scale,
        tilt: request.tilt,
        pin: request.pin,
        chrome: request.chrome,
        entry,
        blend,
        has_start_image: start_texture.is_some(),
        pre_roll: entry && !blend,
        landed: false,
        last_elapsed: 0.0,
    };
    let textures = FlightTextures {
        card: texture,
        start: start_texture,
    };
    Ok((plan, textures))
}

impl FlightPlan {
    /// Turns the travel clock into this frame's pose.
    ///
    /// Pure arithmetic: no renderer, no Win32 and no clock of its own, so the
    /// easing and both springs can be driven at any frame rate from a test.
    fn step(&mut self, elapsed: f32, frame_dt: f32) -> FlightPose {
        let t = (elapsed / FLIGHT_SECONDS).clamp(0.0, 1.0);
        let k = ease::standard(t);

        // After the hand-off the flight image mirrors the real card's row
        // spring and landing sway, so the two never drift apart. An editor
        // flight lands on a still editor card, so it skips the sway.
        if self.landed {
            let dt = (elapsed - self.last_elapsed).clamp(0.0, MAX_FRAME_DT);
            self.follow.step(dt);
        }
        self.last_elapsed = elapsed;

        let (from, to, scale) = (self.from, self.to, self.scale);
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
        let cx = (from_cx + (to_cx - from_cx) * k - self.origin.left as f32) / scale;
        let cy = (from_cy + (to_cy - from_cy) * k - self.origin.top as f32) / scale
            - (std::f32::consts::PI * k).sin() * ARC_DIP;
        let x_offset = if self.landed {
            self.follow.x.value
        } else {
            0.0
        };
        let swing = if self.landed && !self.entry {
            self.follow.swing.value
        } else {
            0.0
        };
        // An entry that carries a settle spring keeps turning during the
        // travel, so the sub-degree of tilt left when it starts never snaps
        // back. Every other flight ramps its fixed tilt in over the trip.
        let angle = match self.settle.as_mut() {
            Some(spring) => {
                spring.step(frame_dt);
                spring.value
            }
            None => self.tilt * k,
        } + swing;
        let card = RectF::from_xywh(
            cx - width * 0.5 + x_offset,
            cy - height * 0.5,
            width,
            height,
        );
        let chrome = match self.chrome {
            ChromeMode::FadeIn => ease::smooth(t, CHROME_FADE_START, CHROME_FADE_END),
            ChromeMode::Full => 1.0,
            // The frame belongs to the line: it is gone a third of the way to
            // the editor and back only over the last third of the way home.
            ChromeMode::Leave => 1.0 - ease::smooth(k, 0.0, NOTE_CHROME_LEAVE_END),
            ChromeMode::Arrive => ease::smooth(k, NOTE_CHROME_ARRIVE_START, 1.0),
        };
        // A capture's photo grows to fill the card as the frame fades in, so
        // its inset follows `chrome`. A note's paper keeps the line's inset for
        // the whole trip: its rect must not jump, or the landing frame would
        // stop lining up with the editor's paper.
        let inset = match self.chrome {
            ChromeMode::FadeIn => layout::PHOTO_INSET * chrome,
            ChromeMode::Full | ChromeMode::Leave | ChromeMode::Arrive => layout::PHOTO_INSET,
        };
        // The corner radius is the other half of the same hand-off: the line's
        // card has a 12 DIP paper radius, the note editor's sheet 22 DIP. A
        // note flight carries the paper from one to the other along the path,
        // so the take-off frame matches the card and the landing frame the
        // editor. A capture keeps the card's radius throughout.
        let radius = match self.chrome {
            ChromeMode::FadeIn | ChromeMode::Full => layout::PHOTO_RADIUS,
            ChromeMode::Leave => {
                layout::PHOTO_RADIUS
                    + (crate::note_editor::display::RADIUS_DIP - layout::PHOTO_RADIUS) * k
            }
            ChromeMode::Arrive => {
                crate::note_editor::display::RADIUS_DIP
                    + (layout::PHOTO_RADIUS - crate::note_editor::display::RADIUS_DIP) * k
            }
        };
        // The hand-off: the travel is over and the real card underneath takes
        // over, but this copy still has to dissolve over it.
        let landed_now = !self.landed && t >= 1.0;
        if landed_now {
            self.landed = true;
        }
        let alpha = if self.landed {
            // The real card underneath is already fully opaque, so only this
            // copy has to dissolve. A smoothstep keeps the fade gentle; the
            // combined image stays at full opacity throughout.
            let progress = ((elapsed - FLIGHT_SECONDS) / LAND_FADE_SECONDS).clamp(0.0, 1.0);
            1.0 - ease::smooth(progress, 0.0, 1.0)
        } else {
            1.0
        };
        FlightPose {
            card,
            // The pin top is the real card's rotation centre; rotating about
            // the frame top would make the landing card jump.
            transform: Matrix3x2::rotation_around(
                angle,
                Vector2 {
                    X: card.left + width * 0.5,
                    Y: card.top - layout::CARD_OFFSET_BELOW_TOP,
                },
            ),
            chrome,
            inset,
            radius,
            alpha,
            shadow: !self.landed,
            pin: pin_alpha(self.pin, t),
            // Only a flight that dissolves the card into the editor image has
            // a start image to fade; every other flight skips the draw.
            crossfade: if self.blend {
                (elapsed / ENTRY_CROSSFADE_SECONDS).clamp(0.0, 1.0)
            } else {
                1.0
            },
            landed_now,
            finished: self.landed && elapsed >= FLIGHT_SECONDS + LAND_FADE_SECONDS,
        }
    }

    /// Turns the pre-roll clock into this frame's pose: the card turning
    /// upright at its slot and dissolving into the editor image.
    fn step_pre_roll(&mut self, elapsed: f64, frame_dt: f32) -> PreRollPose {
        let angle = match self.settle.as_mut() {
            Some(spring) => {
                spring.step(frame_dt);
                spring.value
            }
            None => 0.0,
        };
        // The pre-roll measures its dissolve from the flight's first frame,
        // not from the hand-off: it is what delays the travel.
        let crossfade = if self.has_start_image {
            ((elapsed / ENTRY_CROSSFADE_SECONDS as f64).clamp(0.0, 1.0)) as f32
        } else {
            1.0
        };
        let settled = self
            .settle
            .as_ref()
            .is_none_or(|spring| spring.value.abs() < SETTLE_READY_DEG);
        let card = self.pre_roll_card;
        PreRollPose {
            card,
            transform: Matrix3x2::rotation_around(
                angle,
                Vector2 {
                    X: card.left + card.width() * 0.5,
                    Y: card.top - layout::CARD_OFFSET_BELOW_TOP,
                },
            ),
            crossfade,
            pin: pin_alpha(self.pin, 0.0),
            ready: settled && crossfade >= 1.0,
        }
    }
}

/// Milliseconds since `start`, for the per-frame draw-time probe.
fn elapsed_ms(start: f64) -> f32 {
    ((win32::now_seconds() - start) * 1000.0) as f32
}

/// Runs one flight from the first frame to the end of the landing fade.
///
/// The loop is a sequence of steps: [`prepare`] once, then
/// [`FlightPlan::step`] for the timing and [`paint`] for the drawing, once per
/// frame.
fn run_flight(
    renderer: &mut Renderer,
    token: u64,
    generation: u64,
    interrupt: &AtomicU64,
    request: FlightRequest,
    on_event: &dyn Fn(FlightEvent),
) -> Result<(), String> {
    let (mut plan, textures) = prepare(renderer, request)?;

    let start = win32::now_seconds();
    // The travel clock; an annotation pre-roll hands over to it when the card
    // is upright, so the 0.65 s starts when the card actually moves.
    let mut flight_start = start;
    let mut last = start;
    let mut frames = 0u32;
    let mut draw_max_ms = 0.0f32;
    loop {
        // A newer capture takes over: leave without reporting a landing; the app
        // already made the old card visible again.
        if interrupt.load(Ordering::Relaxed) != generation {
            return Ok(());
        }
        let now = win32::now_seconds();
        let frame_dt = ((now - last) as f32).clamp(0.0, MAX_FRAME_DT);
        last = now;

        // ---- pre-roll (annotation editor): turn upright at the slot ----
        if plan.pre_roll {
            let pose = plan.step_pre_roll(now - start, frame_dt);
            let draw_start = win32::now_seconds();
            paint_pre_roll(renderer, &textures, &pose)?;
            draw_max_ms = draw_max_ms.max(elapsed_ms(draw_start));
            if renderer.present(true) == Present::DeviceLost {
                return Err("device lost during flight pre-roll".to_string());
            }
            if frames == 0 {
                on_event(FlightEvent::Started(token));
            }
            frames += 1;
            if pose.ready {
                plan.pre_roll = false;
                flight_start = now;
                last = now;
            }
            continue;
        }

        let elapsed = (now - flight_start) as f32;
        let pose = plan.step(elapsed, frame_dt);
        // Reported before the frame is drawn: the real card is already
        // underneath, so the landing is announced as soon as the travel ends.
        if pose.landed_now {
            on_event(FlightEvent::Landed {
                token,
                frames,
                elapsed,
                draw_max_ms,
            });
        }
        let draw_start = win32::now_seconds();
        paint(renderer, &textures, &pose)?;
        draw_max_ms = draw_max_ms.max(elapsed_ms(draw_start));
        if renderer.present(true) == Present::DeviceLost {
            return Err("device lost during flight".to_string());
        }
        if frames == 0 {
            // The first frame is on screen: whatever the flight took over from
            // can be hidden now without leaving an empty frame behind it.
            on_event(FlightEvent::Started(token));
        }
        frames += 1;

        if pose.finished {
            on_event(FlightEvent::Finished(token));
            return Ok(());
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn run_drop(
    renderer: &mut Renderer,
    token: u64,
    generation: u64,
    interrupt: &AtomicU64,
    image: &ImageData,
    rect: RectI,
    angle: f32,
    pose_scale: f32,
    origin: RectI,
    dpi: u32,
    on_event: &dyn Fn(FlightEvent),
) -> Result<(), String> {
    let width_px = origin.width().max(1) as u32;
    let height_px = origin.height().max(1) as u32;
    renderer.resize(width_px, height_px, dpi)?;
    let dpi_scale = dpi as f32 / 96.0;
    let texture = renderer.upload(image)?;

    let width = rect.width() as f32 / dpi_scale;
    let height = rect.height() as f32 / dpi_scale;
    let cx = (rect.left + rect.right) as f32 * 0.5 / dpi_scale - origin.left as f32 / dpi_scale;
    let top = rect.top as f32 / dpi_scale - origin.top as f32 / dpi_scale;
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
        let (transform, opacity) = drop_pose(t, cx - width * 0.5, top, width, angle, pose_scale);
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
            layout::PHOTO_INSET,
            layout::PHOTO_RADIUS,
            1.0,
            true,
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

/// A discarded note: it shrinks, spins and falls under gravity until the whole
/// paper is below the window, i.e. off the screen (NOTE-EDITOR.md §5.5).
///
/// The motion is integrated with the real frame `dt` rather than eased from a
/// clock, so the note accelerates like a falling object: a short throw, a slow
/// turn at the top, then a fast drop out of sight.
#[allow(clippy::too_many_arguments)]
fn run_tumble(
    renderer: &mut Renderer,
    token: u64,
    generation: u64,
    interrupt: &AtomicU64,
    image: &ImageData,
    from: RectI,
    spin: f32,
    velocity: (f32, f32),
    origin: RectI,
    dpi: u32,
    on_event: &dyn Fn(FlightEvent),
) -> Result<(), String> {
    let width_px = origin.width().max(1) as u32;
    let height_px = origin.height().max(1) as u32;
    renderer.resize(width_px, height_px, dpi)?;
    let scale = dpi as f32 / 96.0;
    let texture = renderer.upload(image)?;

    let width = from.width() as f32 / scale;
    let height = from.height() as f32 / scale;
    let mut pos = (
        (from.left + from.right) as f32 * 0.5 / scale - origin.left as f32 / scale,
        (from.top + from.bottom) as f32 * 0.5 / scale - origin.top as f32 / scale,
    );
    let mut vel = velocity;
    let mut angle = 0.0f32;
    let mut shrink;
    let window_height = (origin.bottom - origin.top) as f32 / scale;
    logging::trace(format!(
        "tumble render start size={width:.1}x{height:.1} origin={origin:?}"
    ));

    let start = win32::now_seconds();
    let mut last = start;
    let mut frames = 0u32;
    loop {
        if interrupt.load(Ordering::Relaxed) != generation {
            logging::trace(format!("tumble interrupted after {frames} frames"));
            return Ok(());
        }
        let now = win32::now_seconds();
        let dt = ((now - last) as f32).clamp(0.0, 0.05);
        last = now;
        let elapsed = (now - start) as f32;

        // Shrink first (the "grab and flick"), then keep that size.
        let shrink_t = (elapsed / TUMBLE_SHRINK_SECONDS).clamp(0.0, 1.0);
        shrink = 1.0 - (1.0 - TUMBLE_SHRINK) * ease::out_quad(shrink_t);
        vel.1 += TUMBLE_GRAVITY * dt;
        pos.0 += vel.0 * dt;
        pos.1 += vel.1 * dt;
        angle += spin * dt;

        // Done once the whole (shrunk) paper is below the window.
        let half_height = height * shrink * 0.5;
        if pos.1 - half_height > window_height || elapsed >= TUMBLE_SECONDS {
            logging::trace(format!(
                "tumble finished after {frames} frames, {elapsed:.2}s, angle={angle:.0}"
            ));
            on_event(FlightEvent::Finished(token));
            return Ok(());
        }

        // Rotate and scale about the paper's own centre, then place the centre
        // at `pos`: the note turns in place while it travels.
        let centre = Vector2 {
            X: width * 0.5,
            Y: height * 0.5,
        };
        let transform = Matrix3x2::rotation_around(angle, centre)
            * Matrix3x2::scale_around(shrink, shrink, centre)
            * Matrix3x2::translation(pos.0 - width * 0.5, pos.1 - height * 0.5);
        renderer.begin()?;
        renderer.set_transform(transform);
        // A discarded note is a bare sheet: no glass frame, no inset, and the
        // editor's own 22 DIP radius, so the first frame matches the paper the
        // editor was showing.
        draw_flight_card(
            renderer,
            &texture,
            RectF::from_xywh(0.0, 0.0, width, height),
            0.0,
            layout::PHOTO_INSET,
            crate::note_editor::display::RADIUS_DIP,
            1.0,
            true,
            0.0,
        );
        renderer.reset_transform();
        renderer.end()?;
        if renderer.present(true) == Present::DeviceLost {
            return Err("device lost during tumble".to_string());
        }
        if frames == 0 {
            // The note is on screen now; the editor behind it can go.
            on_event(FlightEvent::Started(token));
        }
        frames += 1;
    }
}

fn pin_alpha(mode: PinMode, t: f32) -> f32 {
    match mode {
        PinMode::Hidden => 0.0,
        PinMode::Full => 1.0,
        // The card is still at the line for the first frame; the pin fades
        // while it detaches instead of riding all the way to the editor.
        PinMode::Leave => 1.0 - ease::smooth(t, 0.0, 0.12),
        // The rope end: the pin is clipped back on just before landing.
        PinMode::Arrive => ease::smooth(t, 0.72, 1.0),
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
    // A panic may not unwind out of an `extern "system"` function: it aborts the
    // process. Contain it here.
    // SAFETY: USER32 passes `hwnd` and the message parameters for the flight window;
    // the body treats them as valid for this call (it writes through the
    // `WM_GETMINMAXINFO`/`WM_DPICHANGED` pointers USER32 supplied) and keeps
    // none of them past its return.
    crate::window::guard("flight_wndproc", || unsafe {
        flight_wndproc_inner(hwnd, message, wparam, lparam)
    })
}

/// The real handler; plain `unsafe fn` so a panic unwinds into the guard above.
unsafe fn flight_wndproc_inner(
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
        let (left, top, width, angle, scale) = (123.0, 41.0, 144.0, 2.0, 1.0);
        let cx = left + width * 0.5;
        for step in 0..=20 {
            let t = step as f32 / 20.0;
            let (transform, opacity) = drop_pose(t, left, top, width, angle, scale);
            // The pin is the rotation centre, so rotating never moves it and
            // the card falls in the column above its slot.
            let pin = apply(&transform, width * 0.5, -layout::CARD_OFFSET_BELOW_TOP);
            assert!((pin.0 - cx).abs() < 1e-3, "t={t} pin={pin:?}");
            let e = t * t;
            assert!(
                (pin.1 - (top - layout::CARD_OFFSET_BELOW_TOP + DROP_DISTANCE * e)).abs() < 1e-3,
                "t={t} pin={pin:?}"
            );
            assert!((opacity - (1.0 - e)).abs() < 1e-4);
        }
        // The timeline ends with the card one full drop below and invisible.
        let (transform, opacity) = drop_pose(1.0, 0.0, 10.0, 144.0, 0.0, 1.0);
        let pin = apply(&transform, 72.0, -layout::CARD_OFFSET_BELOW_TOP);
        assert!((pin.1 - (10.0 - layout::CARD_OFFSET_BELOW_TOP + DROP_DISTANCE)).abs() < 1e-3);
        assert!(opacity.abs() < 1e-4);
    }

    /// The first dropped frame must land exactly on the frame the line drew:
    /// same angle, same scale, same rotation origin (`draw_card`'s composition
    /// reproduced here, since the scene's painter is private).
    #[test]
    fn the_first_dropped_frame_matches_the_line_pose() {
        let (card_w, card_h) = (150.0f32, 100.0f32);
        let (left, top) = (320.0f32, 40.0f32);
        let angle = 12.5f32;
        let scale = 1.035f32;
        let (transform, opacity) = drop_pose(0.0, left, top, card_w, angle, scale);
        assert!((opacity - 1.0).abs() < 1e-6);

        let line = Matrix3x2::scale_around(
            scale,
            scale,
            Vector2 {
                X: card_w * 0.5,
                Y: 0.0,
            },
        ) * Matrix3x2::rotation_around(
            angle,
            Vector2 {
                X: card_w * 0.5,
                Y: -layout::CARD_OFFSET_BELOW_TOP,
            },
        ) * Matrix3x2::translation(left, top);

        for (x, y) in [(0.0, 0.0), (card_w, 0.0), (0.0, card_h), (card_w, card_h)] {
            let a = apply(&transform, x, y);
            let b = apply(&line, x, y);
            let distance = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
            assert!(distance < 1e-3, "corner ({x},{y}) moved {distance} DIP");
        }
    }

    #[test]
    fn the_pin_stays_at_the_rope_end() {
        // Leaving a note: present at take-off, gone while the editor is far
        // away, never lingering until the arrival.
        assert!((pin_alpha(PinMode::Leave, 0.0) - 1.0).abs() < 1e-4);
        assert!(pin_alpha(PinMode::Leave, 0.06) < 0.6);
        assert_eq!(pin_alpha(PinMode::Leave, 0.12), 0.0);
        assert_eq!(pin_alpha(PinMode::Leave, 1.0), 0.0);

        // Coming home: no pin during the trip, clipped on only near the slot.
        assert_eq!(pin_alpha(PinMode::Arrive, 0.0), 0.0);
        assert_eq!(pin_alpha(PinMode::Arrive, 0.72), 0.0);
        assert!(pin_alpha(PinMode::Arrive, 0.9) > 0.3);
        assert!((pin_alpha(PinMode::Arrive, 1.0) - 1.0).abs() < 1e-4);

        assert_eq!(pin_alpha(PinMode::Hidden, 0.5), 0.0);
        assert_eq!(pin_alpha(PinMode::Full, 0.5), 1.0);
    }

    /// A blended note-entry flight on a 120 Hz (2x) screen: the one `step`
    /// path that integrates a spring. The card leans 12° at take-off and
    /// travels from a screenshot rect to a slot 1100 DIP to the right.
    fn plan_for_step_tests() -> FlightPlan {
        let mut settle = Spring::new(ENTRY_SETTLE_RESPONSE, ENTRY_SETTLE_DAMPING, 12.0);
        settle.target = 0.0;
        FlightPlan {
            settle: Some(settle),
            follow: LandingFollow::new(0.0),
            pre_roll_card: RectF::default(),
            from: RectI::new(400, 300, 700, 520),
            to: RectI::new(1500, 120, 1772, 312),
            origin: RectI::new(100, 50, 3300, 1350),
            scale: 2.0,
            tilt: 0.0,
            pin: PinMode::Hidden,
            chrome: ChromeMode::FadeIn,
            entry: true,
            blend: true,
            has_start_image: true,
            pre_roll: false,
            landed: false,
            last_elapsed: 0.0,
        }
    }

    #[test]
    fn the_travel_ends_on_the_destination_rect() {
        let mut plan = plan_for_step_tests();
        let (to, origin, scale) = (plan.to, plan.origin, plan.scale);
        let pose = plan.step(FLIGHT_SECONDS, 1.0 / 60.0);
        // At the end of the travel the card sits where the real card will:
        // the destination rect, moved into the window and scaled to DIP.
        let left = (to.center_x() - origin.left as f32) / scale - to.width() as f32 / scale * 0.5;
        let top = (to.center_y() - origin.top as f32) / scale - to.height() as f32 / scale * 0.5;
        assert!(
            (pose.card.left - left).abs() < 1e-3,
            "left={} want={left}",
            pose.card.left
        );
        assert!(
            (pose.card.top - top).abs() < 1e-3,
            "top={} want={top}",
            pose.card.top
        );
        assert!((pose.card.width() - to.width() as f32 / scale).abs() < 1e-3);
        assert!((pose.card.height() - to.height() as f32 / scale).abs() < 1e-3);
        // It lands opaque, with the fade only about to start and the shadow
        // handed over to the real card underneath, which owns it from here.
        assert!(pose.landed_now, "the last travel frame reports the landing");
        assert!((pose.alpha - 1.0).abs() < 1e-4);
        assert!(!pose.shadow, "the landed copy no longer draws the shadow");
        assert!(!pose.finished, "the landing fade has not run yet");
    }

    /// A flight plan with the given chrome mode, everything else as
    /// [`plan_for_step_tests`].
    fn plan_with_chrome(chrome: ChromeMode) -> FlightPlan {
        let mut plan = plan_for_step_tests();
        plan.chrome = chrome;
        plan
    }

    #[test]
    fn note_frames_fade_on_the_editor_side_of_the_path() {
        // The glass frame belongs to the line's card, not to the editor's
        // paper: a note flight must be bare by the time the card is a third of
        // the way to the editor, and decorated only over the last third of the
        // way home. Sampled at 60 Hz across the whole travel.
        let frames = 60;
        let dt = FLIGHT_SECONDS / frames as f32;
        let mut leave = plan_with_chrome(ChromeMode::Leave);
        let mut arrive = plan_with_chrome(ChromeMode::Arrive);
        let mut previous_leave = f32::INFINITY;
        let mut previous_arrive = f32::NEG_INFINITY;
        let mut leave_gone = None;
        let mut arrive_back = None;
        for frame in 1..=frames {
            let elapsed = dt * frame as f32;
            let t = (elapsed / FLIGHT_SECONDS).clamp(0.0, 1.0);
            let k = ease::standard(t);
            let out = leave.step(elapsed, dt);
            let back = arrive.step(elapsed, dt);
            // Monotone: the frame only ever leaves, or only ever returns.
            assert!(out.chrome <= previous_leave + 1e-6, "leave rose at t={t}");
            assert!(
                back.chrome >= previous_arrive - 1e-6,
                "arrive fell at t={t}"
            );
            previous_leave = out.chrome;
            previous_arrive = back.chrome;
            // The paper rect never jumps: both note flights keep the line's
            // inset for the whole trip, so the landing frame still lines up
            // with the editor's paper and the take-off frame with the card.
            assert!((out.inset - layout::PHOTO_INSET).abs() < 1e-6);
            assert!((back.inset - layout::PHOTO_INSET).abs() < 1e-6);
            if leave_gone.is_none() && k >= NOTE_CHROME_LEAVE_END {
                leave_gone = Some((frame, elapsed, out.chrome));
            }
            if arrive_back.is_none() && k >= NOTE_CHROME_ARRIVE_START {
                arrive_back = Some((frame, elapsed, back.chrome));
            }
        }
        let (gone_frame, gone_at, gone_chrome) = leave_gone.expect("the frame never left");
        let (back_frame, back_at, back_chrome) = arrive_back.expect("the frame never returned");
        // A third of the path is reached around 0.14 s and two thirds around
        // 0.25 s, i.e. well before the 0.65 s travel ends.
        assert!(
            (0.12..0.17).contains(&gone_at),
            "the frame left at {gone_at}s (frame {gone_frame})"
        );
        assert!(
            (0.23..0.28).contains(&back_at),
            "the frame returned at {back_at}s (frame {back_frame})"
        );
        assert!(gone_chrome < 0.01, "chrome={gone_chrome} at the leave end");
        assert!(
            back_chrome < 0.01,
            "chrome={back_chrome} at the arrive start"
        );

        // The endpoints are the two real cards: a decorated card at take-off
        // and at the landing, a bare paper in between.
        let first = plan_with_chrome(ChromeMode::Leave).step(dt, dt);
        assert!((first.chrome - 1.0).abs() < 0.01, "chrome={}", first.chrome);
        let last = plan_with_chrome(ChromeMode::Leave).step(FLIGHT_SECONDS, dt);
        assert_eq!(last.chrome, 0.0);
        let first = plan_with_chrome(ChromeMode::Arrive).step(dt, dt);
        assert_eq!(first.chrome, 0.0);
        let last = plan_with_chrome(ChromeMode::Arrive).step(FLIGHT_SECONDS, dt);
        assert!((last.chrome - 1.0).abs() < 1e-6, "chrome={}", last.chrome);
    }

    #[test]
    fn a_capture_still_fades_its_frame_in_over_the_clock() {
        // The screenshot flight is unchanged: its frame fades in over the
        // clock (0.10..0.55) and its photo grows to fill the card as it does.
        let mut plan = plan_with_chrome(ChromeMode::FadeIn);
        let dt = 1.0 / 60.0;
        for frame in 1..=39 {
            let elapsed = dt * frame as f32;
            let t = (elapsed / FLIGHT_SECONDS).clamp(0.0, 1.0);
            let pose = plan.step(elapsed, dt);
            let want = ease::smooth(t, CHROME_FADE_START, CHROME_FADE_END);
            assert!((pose.chrome - want).abs() < 1e-6, "chrome at t={t}");
            assert!((pose.inset - layout::PHOTO_INSET * want).abs() < 1e-6);
            assert!((pose.radius - layout::PHOTO_RADIUS).abs() < 1e-6);
        }
    }

    #[test]
    fn a_note_paper_carries_the_editor_radius_across_the_path() {
        // The line's card paper is 12 DIP, the note editor's sheet 22 DIP. The
        // flight is the hand-off between them, so each end must match the real
        // surface it hands over to, and a capture must keep the card's radius.
        let dt = 1.0 / 60.0;
        let card = layout::PHOTO_RADIUS;
        let sheet = crate::note_editor::display::RADIUS_DIP;

        let first = plan_with_chrome(ChromeMode::Leave).step(0.0, dt);
        assert!((first.radius - card).abs() < 1e-4, "{}", first.radius);
        let last = plan_with_chrome(ChromeMode::Leave).step(FLIGHT_SECONDS, dt);
        assert!((last.radius - sheet).abs() < 1e-4, "{}", last.radius);

        let first = plan_with_chrome(ChromeMode::Arrive).step(0.0, dt);
        assert!((first.radius - sheet).abs() < 1e-4, "{}", first.radius);
        let last = plan_with_chrome(ChromeMode::Arrive).step(FLIGHT_SECONDS, dt);
        assert!((last.radius - card).abs() < 1e-4, "{}", last.radius);

        for chrome in [ChromeMode::FadeIn, ChromeMode::Full] {
            let mut plan = plan_with_chrome(chrome);
            for frame in 1..=39 {
                let pose = plan.step(dt * frame as f32, dt);
                assert!((pose.radius - card).abs() < 1e-6, "{chrome:?} moved");
            }
        }
    }

    #[test]
    fn the_settle_spring_advances_by_time_not_by_frames() {
        // One 60 Hz frame and two 120 Hz frames cover the same 1/60 s, so the
        // settle spring - integrated by real `dt` - must draw the same card.
        // A frame-counted integration would be a whole frame apart.
        let (end, frame_60, frame_120) = (0.30_f32, 1.0 / 60.0, 1.0 / 120.0);
        let mut sixty = plan_for_step_tests();
        let mut one_twenty = plan_for_step_tests();

        let pose_60 = sixty.step(end, frame_60);
        one_twenty.step(end - frame_120, frame_120);
        let pose_120 = one_twenty.step(end, frame_120);

        assert!((pose_60.card.left - pose_120.card.left).abs() < 1e-3);
        assert!((pose_60.card.top - pose_120.card.top).abs() < 1e-3);
        for (x, y) in [(0.0, 0.0), (136.0, 96.0)] {
            let a = apply(&pose_60.transform, x, y);
            let b = apply(&pose_120.transform, x, y);
            assert!(
                (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3,
                "60Hz={a:?} 120Hz={b:?}"
            );
        }

        // The spring really did advance, so the equality above is not vacuous:
        // held at its 12° start, the same flight draws a visibly different card.
        let held = plan_for_step_tests().step(end, 0.0);
        let moved = apply(&pose_60.transform, 0.0, 0.0);
        let still = apply(&held.transform, 0.0, 0.0);
        let travel = ((moved.0 - still.0).powi(2) + (moved.1 - still.1).powi(2)).sqrt();
        assert!(
            travel > 1.0,
            "the settle spring did not advance: {moved:?} vs {still:?}"
        );
    }
}
