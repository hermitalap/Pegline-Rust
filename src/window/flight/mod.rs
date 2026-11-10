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
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, MA_NOACTIVATE, SW_HIDE, ShowWindow,
    WM_ERASEBKGND, WM_MOUSEACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
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
/// How long a note card takes to dissolve into the editor view at the start of
/// its flight to the centre.
const ENTRY_CROSSFADE_SECONDS: f32 = 0.12;
/// Angular spring that brings a tilted hanging card upright before it flies to
/// an editor. A short, slightly under-damped spring so the turn accelerates and
/// eases out instead of snapping.
const ENTRY_SETTLE_RESPONSE: f32 = 0.16;
const ENTRY_SETTLE_DAMPING: f32 = 0.9;
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
        /// Keep the glass frame and pin fully opaque from the first frame:
        /// editor flights start from a real, fully decorated card.
        chrome_full: bool,
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
        tilt: f32,
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
            false,
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
        dpi: u32,
    ) -> bool {
        self.start_flight(
            token,
            FlightImage::Ready(image),
            None,
            None,
            true,
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
        dpi: u32,
        blend: bool,
    ) -> bool {
        self.start_flight(
            token,
            image,
            start_image,
            Some(settle_angle),
            true,
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
        chrome_full: bool,
        blend_entry: bool,
        from: RectI,
        to: RectI,
        tilt: f32,
        slide_dip: f32,
        pin: PinMode,
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
            start_image,
            settle_angle,
            blend_entry,
            chrome_full,
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
        let scale = dpi.max(96) as f32 / 96.0;
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
                start_image,
                settle_angle,
                blend_entry,
                chrome_full,
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
                    chrome_full,
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
                    hwnd.0,
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
    /// Keep the glass frame and pin fully opaque from the first frame: editor
    /// flights start from a real, fully decorated card.
    chrome_full: bool,
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
    /// The glass frame and pin are opaque from the first frame.
    chrome_full: bool,
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
    /// The hand-off happened on an earlier frame, so the follow springs move.
    following: bool,
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
    let scale = request.dpi.max(96) as f32 / 96.0;
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
        chrome_full: request.chrome_full,
        entry,
        blend,
        has_start_image: start_texture.is_some(),
        pre_roll: entry && !blend,
        landed: false,
        following: false,
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
        let k = ease::in_out_cubic(t);

        // After the hand-off the flight image mirrors the real card's row
        // spring and landing sway, so the two never drift apart. An editor
        // flight lands on a still editor card, so it skips the sway.
        if self.landed && self.following {
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
        // A blended note entry keeps its starting lean and settles while it
        // travels; the annotation pre-roll has already brought it to zero.
        let settle_now = if self.blend {
            self.settle.as_mut().map_or(0.0, |spring| {
                spring.step(frame_dt);
                spring.value
            })
        } else {
            0.0
        };
        let angle = if self.blend {
            settle_now
        } else {
            self.tilt * k
        } + swing;
        let card = RectF::from_xywh(
            cx - width * 0.5 + x_offset,
            cy - height * 0.5,
            width,
            height,
        );
        let chrome = if self.chrome_full {
            1.0
        } else {
            ease::smooth(t, CHROME_FADE_START, CHROME_FADE_END)
        };
        // The hand-off: the travel is over and the real card underneath takes
        // over, but this copy still has to dissolve over it.
        let landed_now = !self.landed && t >= 1.0;
        if landed_now {
            self.landed = true;
        }
        self.following = self.landed;
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
        let settled = self.settle.as_ref().is_none_or(|spring| spring.settled());
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
    hwnd: HWND,
    token: u64,
    generation: u64,
    interrupt: &AtomicU64,
    request: FlightRequest,
    on_event: &dyn Fn(FlightEvent),
) -> Result<(), String> {
    let (mut plan, textures) = prepare(renderer, request)?;
    let _ = hwnd;

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
    let scale = dpi.max(96) as f32 / 96.0;
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
        draw_flight_card(
            renderer,
            &texture,
            RectF::from_xywh(0.0, 0.0, width, height),
            1.0,
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
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        flight_wndproc_inner(hwnd, message, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            crate::logging::error("panic in flight_wndproc");
            LRESULT(0)
        }
    }
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
            chrome_full: false,
            entry: true,
            blend: true,
            has_start_image: true,
            pre_roll: false,
            landed: false,
            following: false,
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
