//! The line scene: what hangs on the rope and how it moves.
//!
//! The scene lives on the overlay's render thread. The UI thread sends
//! [`Command`]s; the render thread drains them once per frame, integrates the
//! springs with the real `dt` and draws. The UI thread reads a [`FrameSnapshot`]
//! for hit testing, so no rendering result is ever read back.

mod paint;
mod pendulum;
mod trace;

use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;

use crate::layout;
use crate::render::{ActionsLayout, Renderer};
use crate::spring::{Spring, ease};
use crate::types::{ActionKind, CardId, CardSpec, FrameSnapshot, HitCard, ImageData, RectF};

use pendulum::Pendulum;
use trace::{PaperTrace, TraceRow};

/// Time for the "Copied" label, seconds.
const COPIED_SECONDS: f64 = 1.2;
/// Time for a card to fade in, seconds (exponential time constant).
const FADE_SECONDS: f32 = 0.16;
/// Delay for the hide animation, seconds.
const HIDE_SECONDS: f32 = 0.22;
/// Response of the row's horizontal spring, seconds.
const ROW_RESPONSE: f32 = 0.55;
/// Damping ratio of the row's horizontal spring. The note entry uses the same
/// curve, so it reflows exactly with the last hanging card.
const ROW_DAMPING: f32 = 0.78;

// Pin drag. The follow spring is deliberately quick and only lightly damped:
// the card should trail the pointer by a few DIP, not lag behind it.
const GRAB_RESPONSE: f32 = 0.075;
const GRAB_DAMPING: f32 = 0.85;
const GRAB_DROP_RESPONSE: f32 = 0.30;
const GRAB_DROP_DAMPING: f32 = 0.55;
/// Release spring: softer than the follow, so the card settles with a visible
/// elastic overshoot instead of stopping dead.
const RELEASE_RESPONSE: f32 = 0.46;
const RELEASE_DAMPING: f32 = 0.80;
/// Travel before a pin press becomes a drag, in DIP.
const GRAB_DRAG_DIP: f32 = 4.0;
/// The Y rubber band: free travel, then a compressed slope and a hard cap.
const RUBBER_FREE_DIP: f32 = 18.0;
const RUBBER_SLOPE: f32 = 0.22;
const RUBBER_MAX_DIP: f32 = 26.0;
/// How far the first and second neighbours may be pushed, in DIP.
const SQUEEZE_CAP: f32 = 34.0;
const SQUEEZE_CHAIN_CAP: f32 = 10.0;
/// The neighbour's lean at full push, degrees.
const SQUEEZE_ANGLE: f32 = 3.0;
/// Pre-collision penetration that arms a swap, its dwell and its cooldown.
const SWAP_PENETRATION: f32 = 28.0;
const SWAP_DWELL: f64 = 0.06;
const SWAP_COOLDOWN: f64 = 0.18;
/// After a swap, the dragged card must retreat this far before the reverse
/// swap arms. Without it the two cards would swap back and forth forever while
/// the dragged one still overlaps its neighbour.
const SWAP_REARM_DIP: f32 = 43.0;
const SWAP_HOP_SECONDS: f32 = 0.20;
const SWAP_HOP_DIP: f32 = 6.0;
const SWAP_POP_VELOCITY: f32 = 260.0;
const SWAP_RECOIL_VELOCITY: f32 = 160.0;

/// Effective gravity at the pin, DIP/s². The pendulum's natural frequency is
/// `sqrt(g_eff / L_eff)`; a pin accelerating downwards makes the card lighter
/// and slows its swing.
const GRAVITY_DIP: f32 = 35_500.0;
/// A card behaves like a uniform rectangle. Keep its effective length in a
/// sane range for the smallest note and the widest screenshot.
const PENDULUM_L_MIN: f32 = 60.0;
const PENDULUM_L_MAX: f32 = 220.0;
/// Safety limits for the pin acceleration fed into the pendulum.
const PENDULUM_ACCEL_CLAMP: f32 = 6000.0;
const PENDULUM_MAX_DEG: f32 = 6.0;
const PENDULUM_DEADZONE: f32 = 300.0;
/// The bottom edge lags the rigid rotation by a small shear: response,
/// damping, degrees per degree/second of swing, and the clamp.
const FLEX_RESPONSE: f32 = 0.12;
const FLEX_DAMPING: f32 = 0.55;
const FLEX_GAIN: f32 = 0.006;
const FLEX_MAX_DEG: f32 = 1.2;

// Paper coefficients. Air drag grows with the card's aspect ratio: a tall
// strip sweeps a longer arc and loses its swing sooner than a wide screenshot.
// The relative-wind torque uses the card's own linear velocity, so a card
// dragged at constant speed keeps a small backward tilt instead of hanging
// perfectly vertical like a block on a hinge.
// The “stronger air” tuner preset, converted to effective coefficients:
// air 1.55 x (damping 0.72 + 0.28*tall, drag 0.18 + 0.20*tall, wind 2.8e-5).
// The drag aspect slider is 0.20; after the 1.55x air multiplier it is 0.31 here.
const PAPER_DAMPING_BASE: f32 = 1.116;
const PAPER_DAMPING_ASPECT: f32 = 0.434;
const PAPER_DRAG_BASE: f32 = 0.279;
const PAPER_DRAG_ASPECT: f32 = 0.31;
const PAPER_WIND_BASE: f32 = 4.34e-5;
const PAPER_ASPECT_NORM: f32 = 1.2;

// Pin visual states: scale and glow opacity.
const PIN_HOVER_SCALE: f32 = 1.12;
const PIN_HOVER_GLOW: f32 = 0.22;
const PIN_PRESS_SCALE: f32 = 0.94;
const PIN_PRESS_GLOW: f32 = 0.26;
const PIN_DRAG_SCALE: f32 = 1.08;
const PIN_DRAG_GLOW: f32 = 0.20;

/// The Y rubber band: linear to [`RUBBER_FREE_DIP`], then compressed, capped.
pub(crate) fn rubber_y(raw: f32) -> f32 {
    let raw = raw.max(0.0);
    let stretched = if raw <= RUBBER_FREE_DIP {
        raw
    } else {
        RUBBER_FREE_DIP + (raw - RUBBER_FREE_DIP) * RUBBER_SLOPE
    };
    stretched.min(RUBBER_MAX_DIP)
}

/// The pre-collision penetration the next card feels, in DIP.
pub(crate) fn squeeze_push(prev_edge: f32, slot: f32, width: f32, cap: f32) -> f32 {
    (prev_edge - (slot - width * 0.5)).clamp(0.0, cap)
}

/// Effective length of a uniform rectangular card pivoting at its top edge:
/// `(w²/12 + h²/3) / (h/2)`, clamped for the smallest note and the widest
/// screenshot.
fn effective_length((w, h): (f32, f32)) -> f32 {
    let h = h.max(1.0);
    (((w * w) / 12.0 + (h * h) / 3.0) / (h * 0.5)).clamp(PENDULUM_L_MIN, PENDULUM_L_MAX)
}

/// Tuning values read once from the environment, so the trace tool can try a
/// damping or wind strength without a rebuild. The defaults are the shipped
/// paper model.
#[derive(Clone, Copy)]
struct PaperTuning {
    damping_base: f32,
    damping_aspect: f32,
    drag_base: f32,
    drag_aspect: f32,
    wind_base: f32,
    aspect_norm: f32,
}

impl PaperTuning {
    fn from_env() -> Self {
        fn value(name: &str, default: f32) -> f32 {
            std::env::var(name)
                .ok()
                .and_then(|text| text.trim().parse::<f32>().ok())
                .filter(|number| number.is_finite() && *number >= 0.0)
                .unwrap_or(default)
        }
        Self {
            damping_base: value("PEGLINE_PAPER_DAMPING", PAPER_DAMPING_BASE),
            damping_aspect: PAPER_DAMPING_ASPECT,
            drag_base: value("PEGLINE_PAPER_DRAG", PAPER_DRAG_BASE),
            drag_aspect: PAPER_DRAG_ASPECT,
            wind_base: value("PEGLINE_PAPER_WIND", PAPER_WIND_BASE),
            aspect_norm: value("PEGLINE_PAPER_ASPECT", PAPER_ASPECT_NORM).max(0.1),
        }
    }
}

fn paper_tuning() -> &'static PaperTuning {
    static TUNING: OnceLock<PaperTuning> = OnceLock::new();
    TUNING.get_or_init(PaperTuning::from_env)
}

#[derive(Clone, Copy)]
struct PaperCoefficients {
    damping: f32,
    drag: f32,
    wind_gain: f32,
}

/// Per-card paper constants. `tall` is 0 for a wide card and 1 for an aspect
/// ratio 1.2 above square (or higher); the wind gain also grows with it.
fn paper_coefficients((width, height): (f32, f32)) -> PaperCoefficients {
    let tuning = paper_tuning();
    let aspect = (height / width.max(1.0)).max(0.01);
    let tall = ((aspect - 1.0) / tuning.aspect_norm).clamp(0.0, 1.0);
    PaperCoefficients {
        damping: (tuning.damping_base + tuning.damping_aspect * tall).max(0.0),
        drag: (tuning.drag_base + tuning.drag_aspect * tall).max(0.0),
        wind_gain: (tuning.wind_base * (0.7 + 0.6 * tall)).max(0.0),
    }
}

/// The relative-wind angular acceleration at the card's centre of pressure.
/// Positive is clockwise (bottom left): a card dragged right leaves its bottom
/// behind, and a card dragged left does the mirror image. `height` is the card
/// height in DIP; the centre of pressure is halfway down.
fn relative_wind(angle_deg: f32, omega_deg: f32, vx: f32, vy: f32, height: f32, gain: f32) -> f32 {
    let theta = angle_deg.to_radians();
    let omega = omega_deg.to_radians();
    let radius = (height * 0.5).max(1.0);
    let (sin, cos) = theta.sin_cos();
    let vx_point = vx - radius * omega * cos;
    let vy_point = vy - radius * omega * sin;
    let speed = (vx_point * vx_point + vy_point * vy_point).sqrt();
    (gain * speed * (vx_point * cos + vy_point * sin) * radius)
        .clamp(-PENDULUM_ACCEL_CLAMP, PENDULUM_ACCEL_CLAMP)
}

/// What the UI thread asks the line to do.
#[derive(Debug)]
pub enum Command {
    /// New window size and DPI (the window itself was already moved by the UI).
    Layout {
        width_px: u32,
        height_px: u32,
        dpi: u32,
    },
    /// The window became visible or hidden; a hidden line runs no frames.
    SetVisible(bool),
    /// Start sliding the line down (`true`) or up (`false`).
    Reveal(bool),
    /// Put a card on the line.
    AddCard(CardSpec),
    /// A thumbnail finished decoding after the card was added.
    SetImage { id: CardId, image: ImageData },
    /// A note wears another colour; redraw its swatch ring.
    SetNoteColor { id: CardId, color: usize },
    /// Take a card off the line immediately (the fall, if any, is drawn by the
    /// flight window).
    RemoveCard { id: CardId },
    /// Which card the pointer is over.
    Hover(Option<CardId>),
    /// Which hover button the pointer is over, if any.
    HoverAction(Option<(CardId, ActionKind)>),
    /// Which card is being held down.
    Press(Option<CardId>),
    /// Which card is being dragged (its frame goes translucent).
    Dragging(Option<CardId>),
    /// Begin a pin drag. `pointer` is the current cursor position in window
    /// DIP; the render thread updates it every frame afterwards.
    Grab { id: CardId, pointer: (f32, f32) },
    /// End the pin drag: settle the card into its (possibly new) slot.
    EndGrab,
    /// Which clothespin the pointer is over.
    PinHover(Option<CardId>),
    /// The flight window carries this card; keep it invisible until it lands.
    SetFlying { id: CardId, flying: bool },
    /// The flight window carries an existing card back to its slot. Unlike
    /// [`Command::SetFlying`] the row must not reflow: the card keeps its place
    /// and only goes invisible under the flight.
    SetRehang { id: CardId, flying: bool },
    /// The entry note's peeking corner: `true` slides its top slice down from
    /// the slot, `false` takes it back up.
    NotePeek(bool),
    /// Drop the peeking corner at once: the full note is taking over its slot
    /// and a shrinking copy behind it would read as a second card.
    HideNotePeek,
    /// Flash the "Copied" pill on a card.
    Copied(CardId),
    /// A gust of wind (also fired by the internal timer).
    Gust,
    /// The desktop wallpaper changed; repaint so the Mica texture is rebuilt.
    WallpaperChanged,
    /// Cap the frame rate to `1 / period` Hz; 0 follows the display.
    SetFrameCap(f64),
    /// Stop the render thread.
    Stop,
}

/// Horizontal targets for a row of `count` cards.
///
/// A card that is still flying is not on the line yet: it waits at the slot it
/// will land on — half a spacing to the right of the cards already hanging —
/// while those cards keep their old places. Once it lands, `flying` is `None`
/// and every card gets the centred layout, so the row glides left under the
/// new card on the horizontal spring (BEHAVIOR §5.2).
fn row_targets(count: usize, flying: Option<usize>, width: f32) -> Vec<f32> {
    let visible = if flying.is_some() {
        count.saturating_sub(1)
    } else {
        count
    };
    let mut targets = Vec::with_capacity(count);
    let mut index = 0;
    for slot in 0..count {
        if flying == Some(slot) {
            targets.push(layout::flight_slot_x(visible, visible + 1, width));
        } else {
            targets.push(layout::slot_x(index, visible, width));
            index += 1;
        }
    }
    targets
}

/// One card's animated state.
struct Card {
    id: CardId,
    card_size: (f32, f32),
    tilt: f32,
    x: Spring,
    drop: Spring,
    swing: Spring,
    press: Spring,
    opacity: f32,
    cross_alpha: f32,
    actions_alpha: f32,
    copied_alpha: f32,
    arriving: bool,
    arrived: bool,
    flying: bool,
    /// Hidden under the flight that is carrying an edited note back to its
    /// slot; the row layout is untouched (unlike `flying`).
    rehang: bool,
    image: Option<ID2D1Bitmap1>,
    note: bool,
    note_color: usize,
    /// Seconds left before this card's scheduled gust starts.
    nudge_delay: f32,
    nudge_from: f32,
    nudge_to: f32,
    /// -1 is idle, 0..1 is the ease-out progress of an active nudge.
    nudge_t: f32,
    /// The hanging card's inertial sway: a damped pendulum with the card's
    /// own effective length and quadratic air drag.
    pendulum: Pendulum,
    /// Effective pendulum length for this card, DIP.
    pendulum_length: f32,
    /// Per-card relative-wind gain, degrees/s² per (DIP/s)² at the pin.
    wind_gain: f32,
    /// The previous frame's X velocity, for the pendulum's acceleration.
    last_vx: f32,
    /// The previous frame's drop velocity, for the vertical excitation.
    last_vy: f32,
    /// A small shear of the lower card, degrees; the bottom edge lags the pin.
    flex: Spring,
    /// The neighbour's extra rotation while the dragged card squeezes it.
    squeeze: Spring,
    /// The clothespin's scale, glow and hot-metal cross-fade.
    pin_scale: Spring,
    pin_glow: Spring,
    pin_hot: Spring,
    /// -1 is idle, otherwise 0..1 of the 20 ms swap hop.
    hop_t: f32,
    /// True between a release and the moment the row spring has settled, so
    /// the faster release response can be swapped back to the row response.
    release_spring: bool,
}

impl Card {
    fn new(spec: CardSpec, slot_x: f32) -> Self {
        let pendulum_length = effective_length(spec.card_size);
        let paper = paper_coefficients(spec.card_size);
        let mut card = Self {
            id: spec.id,
            card_size: spec.card_size,
            tilt: spec.tilt,
            x: Spring::new(ROW_RESPONSE, ROW_DAMPING, slot_x),
            drop: Spring::new(0.42, 0.72, -46.0),
            swing: Spring::new(1.0, 1.0, 0.0),
            press: Spring::new(0.3, 0.6, 1.0),
            opacity: 0.0,
            cross_alpha: 0.0,
            actions_alpha: 0.0,
            copied_alpha: 0.0,
            arriving: spec.arriving,
            arrived: false,
            flying: false,
            rehang: false,
            image: None,
            note: spec.note,
            note_color: 0,
            nudge_delay: 0.0,
            nudge_from: 0.0,
            nudge_to: 0.0,
            nudge_t: -1.0,
            pendulum: Pendulum::new_with(
                (GRAVITY_DIP / pendulum_length).sqrt(),
                paper.damping,
                paper.drag,
            ),
            pendulum_length,
            wind_gain: paper.wind_gain,
            last_vx: 0.0,
            last_vy: 0.0,
            flex: Spring::new(FLEX_RESPONSE, FLEX_DAMPING, 0.0),
            squeeze: Spring::new(0.22, 0.80, 0.0),
            pin_scale: Spring::new(0.16, 0.85, 1.0),
            pin_glow: Spring::new(0.16, 0.85, 0.0),
            pin_hot: Spring::new(0.18, 0.90, 0.0),
            hop_t: -1.0,
            release_spring: false,
        };
        card.x.target = slot_x;
        if spec.arriving {
            // The flight already did the travelling; the card starts in place
            // and only gets the little landing sway.
            card.arrived = true;
            card.drop.snap(0.0);
            card.opacity = 1.0;
            card.nudge(2.2);
        } else {
            // Start leaning, then let the spring swing back to 0. The original
            // set the value, not the target.
            card.swing.value = 16.0;
            card.swing.set_stiffness(46.0, 2.6);
            // The card drops onto the line from 46 DIP above.
            card.drop.target = 0.0;
        }
        card
    }

    fn nudge(&mut self, degrees: f32) {
        self.nudge_from = self.swing.value;
        self.nudge_to = degrees;
        self.nudge_t = 0.0;
        self.nudge_delay = 0.0;
    }

    /// Schedules a gust for this card: it keeps swinging for `delay` seconds,
    /// then is eased out to `degrees` and released back to the rope.
    fn schedule_gust(&mut self, degrees: f32, delay: f32) {
        self.nudge_to = degrees;
        if delay <= 0.0 {
            // No stagger for this card: start now, from where it hangs.
            self.nudge_from = self.swing.value;
            self.nudge_t = 0.0;
            self.nudge_delay = 0.0;
        } else {
            self.nudge_delay = delay;
            self.nudge_t = -1.0;
        }
    }

    /// Whether the swing is visually at rest. The generic spring threshold
    /// (0.001) is far below one pixel for a rotation, and a lightly damped
    /// spring takes many seconds to reach its velocity; stopping the frame loop
    /// at a twentieth of a degree is what matters here.
    fn swing_at_rest(&self) -> bool {
        self.nudge_delay <= 0.0
            && self.nudge_t < 0.0
            && self.swing.value.abs() < 0.02
            && self.swing.velocity.abs() < 0.1
    }

    /// Advances the swing by `dt`: a pending gust counts down, an active gust
    /// eases the card out, and the rest of the time the damped spring pulls it
    /// back to the rope.
    fn step_swing(&mut self, dt: f32) {
        if self.nudge_delay > 0.0 {
            // Wait for this card's turn, but do not freeze it: the spring keeps
            // running so the gust starts from wherever the card is.
            self.nudge_delay -= dt;
            self.swing.step(dt);
            if self.nudge_delay <= 0.0 {
                self.nudge_delay = 0.0;
                self.nudge_from = self.swing.value;
                self.nudge_t = 0.0;
            }
            return;
        }
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
            return;
        }
        self.swing.step(dt);
    }

    /// The swap hop's vertical offset: a half sine, up and back down in
    /// [`SWAP_HOP_SECONDS`]. Zero when no hop is running.
    fn hop_offset(&self) -> f32 {
        if self.hop_t < 0.0 {
            0.0
        } else {
            -SWAP_HOP_DIP * (std::f32::consts::PI * self.hop_t).sin()
        }
    }

    /// Top of the card frame in slot-local DIP (before rotation).
    fn frame_top(&self) -> f32 {
        layout::CARD_OFFSET_BELOW_TOP
    }

    fn frame_rect(&self, slot_left: f32) -> RectF {
        let (card_w, card_h) = self.card_size;
        RectF::from_xywh(
            slot_left + (layout::CARD_WIDTH - card_w) * 0.5,
            self.frame_top(),
            card_w,
            card_h,
        )
    }

    fn actions_rect(&self, slot_left: f32, actions: &ActionsLayout) -> RectF {
        let left = slot_left + (layout::CARD_WIDTH - actions.total_width) * 0.5;
        RectF::from_xywh(
            left,
            layout::actions_top_for(self.card_size.1, self.note),
            actions.total_width,
            actions.height,
        )
    }

    /// The copy and open pills, in slot-local DIP.
    fn button_rects(&self, slot_left: f32, actions: &ActionsLayout) -> (RectF, RectF) {
        let left = slot_left + (layout::CARD_WIDTH - actions.total_width) * 0.5;
        let top = layout::actions_top_for(self.card_size.1, self.note);
        (
            RectF::from_xywh(left, top, actions.left_width, actions.height),
            RectF::from_xywh(
                left + actions.left_width + actions.gap,
                top,
                actions.right_width,
                actions.height,
            ),
        )
    }

    /// The swatch pill and its four dots, in slot-local DIP.
    fn swatch_rects(&self, slot_left: f32) -> [RectF; 4] {
        let mut rects = [RectF::default(); 4];
        for (index, rect) in rects.iter_mut().enumerate() {
            *rect = layout::swatch_dot(index, self.card_size.1).offset(slot_left, 0.0);
        }
        rects
    }
}

/// A small deterministic RNG for tilt and wind; no dependency needed.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> f32 {
        // xorshift32
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x as f32) / (u32::MAX as f32)
    }

    fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.next()
    }
}

/// The active clothespin drag: which card, where the pointer grabbed it and
/// the last sample the render thread took.
struct Grab {
    id: CardId,
    /// Pin anchor minus pointer at press time, in DIP. The anchor is the card's
    /// top-centre, the point the card swings around.
    offset: (f32, f32),
    press: (f32, f32),
    pointer: (f32, f32),
    /// The last horizontal direction the pointer moved, +1 right and -1 left.
    /// The squeeze follows the drag, not which side of the press it is on, so
    /// pulling back after a swap still squeezes the card on that side.
    move_dir: i32,
    moved: bool,
    /// The last swap direction, +1 right and -1 left, and where the dragged
    /// card was when it happened. The reverse swap only arms after the card
    /// has retreated [`SWAP_REARM_DIP`] from this anchor.
    swap_dir: i32,
    swap_anchor: f32,
    swap_since: Option<f64>,
    cooldown_until: f64,
}

/// The line itself.
pub struct Scene {
    width_dip: f32,
    scale: f32,
    visible: bool,
    revealed: bool,
    slide: Spring,
    hide_t: f32,
    hide_from: f32,
    /// 0 = the entry note is hidden, 1 = its 16 DIP top corner is showing.
    note_peek: Spring,
    /// Horizontal centre of the entry's peeking corner, in window DIP. It
    /// follows the row on the same spring, so removing a card slides the
    /// corner instead of teleporting it to the new slot.
    note_peek_x: Spring,
    cards: Vec<Card>,
    hover: Option<CardId>,
    hover_action: Option<(CardId, ActionKind)>,
    pressed: Option<CardId>,
    dragging: Option<CardId>,
    /// The clothespin the pointer is over. Kept separate from `hover`: the pin
    /// is outside the card frame and needs its own highlight.
    pin_hover: Option<CardId>,
    /// The active pin drag. It is not `dragging`: `dragging` is the OLE drag
    /// out of a card and must keep the frame loop asleep behind its modal
    /// loop, while a pin drag runs at display rate and never blocks.
    grab: Option<Grab>,
    copied: Option<CardId>,
    copied_until: f64,
    hint_alpha: f32,
    next_gust: f64,
    rng: Rng,
    actions: ActionsLayout,
    /// Note cards keep the original Copy + Open row; pictures replace Copy
    /// with Annotate.
    note_actions: ActionsLayout,
    copied_height: f32,
    snapshot: Arc<Mutex<FrameSnapshot>>,
    dirty: bool,
    /// Optional CSV trace, opened only when `PEGLINE_PAPER_TRACE` is set.
    trace: Option<PaperTrace>,
}

impl Scene {
    pub fn new(
        width_px: u32,
        dpi: u32,
        hover_actions: ActionsLayout,
        note_hover_actions: ActionsLayout,
        copied_height: f32,
        snapshot: Arc<Mutex<FrameSnapshot>>,
    ) -> Self {
        let scale = dpi as f32 / 96.0;
        let width_dip = width_px as f32 / scale;
        // The entry starts at the empty line's slot, right of the centre where
        // the first card will hang.
        let note_peek_x = layout::note_hotspot(0, width_dip, layout::NOTE_CARD_HEIGHT, true)
            .map(|hotspot| hotspot.center().0)
            .unwrap_or(width_dip * 0.5 + layout::SPACING * 0.5);
        let mut scene = Self {
            width_dip,
            scale,
            visible: false,
            revealed: false,
            slide: Spring::new(0.42, 0.82, layout::HIDDEN_SLIDE),
            hide_t: -1.0,
            hide_from: layout::HIDDEN_SLIDE,
            note_peek: Spring::new(0.22, 1.0, 0.0),
            note_peek_x: Spring::new(ROW_RESPONSE, ROW_DAMPING, note_peek_x),
            cards: Vec::new(),
            hover: None,
            hover_action: None,
            pressed: None,
            dragging: None,
            pin_hover: None,
            grab: None,
            copied: None,
            copied_until: 0.0,
            hint_alpha: 0.0,
            next_gust: 7.0,
            rng: Rng(0x9E37_79B9),
            actions: hover_actions,
            note_actions: note_hover_actions,
            copied_height,
            snapshot,
            dirty: true,
            trace: PaperTrace::open(),
        };
        scene.publish(0.0);
        scene
    }

    pub fn card_count(&self) -> usize {
        self.cards.len()
    }

    pub fn slide_value(&self) -> f32 {
        self.slide.value
    }

    /// Seconds until the next automatic gust, when the frame loop may sleep
    /// until then. `None` means no timer is pending (hidden line, no cards, or
    /// a drag in progress).
    pub fn next_wake_in(&self, now: f64) -> Option<f64> {
        if !self.visible || !self.revealed {
            return None;
        }
        // A pin drag runs at display rate: the pointer is sampled every frame
        // and the card follows it, so the loop must never sleep until the
        // grab ends. `Some(0.0)` asks for the next frame immediately.
        if self.grab.is_some() {
            return Some(0.0);
        }
        if self.cards.is_empty() || self.dragging.is_some() {
            return None;
        }
        Some((self.next_gust - now).max(0.001))
    }

    /// True while a pin drag owns the pointer. The render thread samples the
    /// cursor while this holds.
    pub fn grabbing(&self) -> bool {
        self.grab.is_some()
    }

    /// Feeds the latest cursor sample, in window DIP, into the active drag.
    /// Called from the render thread; it must not allocate or log.
    pub fn set_pointer(&mut self, x: f32, y: f32, _now: f64) {
        let Some(grab) = self.grab.as_mut() else {
            return;
        };
        let step = x - grab.pointer.0;
        if step.abs() > 0.5 {
            let dir = if step > 0.0 { 1 } else { -1 };
            if dir != grab.move_dir {
                grab.move_dir = dir;
                grab.swap_since = None;
            }
        }
        grab.pointer = (x, y);
        if !grab.moved {
            let dx = x - grab.press.0;
            let dy = y - grab.press.1;
            if dx.abs() + dy.abs() > GRAB_DRAG_DIP {
                grab.moved = true;
            }
        }
    }

    /// Debug summary of the cards, for the trace log.
    pub fn debug_cards(&self) -> String {
        self.cards
            .iter()
            .map(|card| {
                format!(
                    "id{} x={:.1} top={:.1} drop={:.1} swing={:.1} op={:.3}",
                    card.id,
                    card.x.value,
                    layout::rope_y(card.x.value, self.width_dip) - layout::PIN_ABOVE
                        + card.drop.value
                        + self.slide.value
                        + layout::CARD_OFFSET_BELOW_TOP,
                    card.drop.value,
                    card.swing.value,
                    card.opacity
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn set_layout(&mut self, width_px: u32, height_px: u32, dpi: u32) {
        let scale = dpi as f32 / 96.0;
        self.scale = scale;
        self.width_dip = width_px as f32 / scale;
        let _ = height_px;
        // The slots depend on the line width, so they move with the window.
        self.relayout_cards();
        self.dirty = true;
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        if !visible {
            // A hide must not leave the drag half alive: settle the card into
            // its slot and let the release spring run when the line returns.
            self.end_grab(0.0);
            self.hover = None;
            self.hover_action = None;
            self.pressed = None;
            self.pin_hover = None;
            self.note_peek.snap(0.0);
            self.dirty = true;
        }
    }

    pub fn apply(&mut self, command: Command, renderer: &Renderer, now: f64) {
        match command {
            Command::Layout {
                width_px,
                height_px,
                dpi,
            } => self.set_layout(width_px, height_px, dpi),
            Command::SetVisible(visible) => self.set_visible(visible),
            Command::Reveal(revealed) => {
                if self.revealed != revealed {
                    self.revealed = revealed;
                    if !revealed {
                        self.end_grab(now);
                        self.hide_t = -1.0;
                        // The peeking corner belongs to a line that is down; it
                        // must not pop back when the strip returns.
                        self.note_peek.snap(0.0);
                        // Let the spring start from wherever it is and keep the
                        // velocity: the quadratic ease-out takes over below.
                    } else if now >= self.next_gust {
                        // Do not fire a stale gust the instant the line drops;
                        // start the breeze clock again.
                        self.next_gust = now + self.rng.range(7.0, 16.0) as f64;
                    }
                    self.dirty = true;
                }
            }
            Command::AddCard(mut spec) => {
                let index = self.cards.len();
                let x = layout::slot_x(index, index + 1, self.width_dip);
                let image = spec.image.take();
                let mut card = Card::new(spec, x);
                if let Some(image) = image.as_ref() {
                    match renderer.upload(image) {
                        Ok(bitmap) => card.image = Some(bitmap),
                        Err(error) => crate::logging::error(format!("upload card: {error}")),
                    }
                }
                self.cards.push(card);
                self.relayout_cards();
                self.dirty = true;
            }
            Command::SetImage { id, image } => {
                if let Some(card) = self.cards.iter_mut().find(|card| card.id == id) {
                    match renderer.upload(&image) {
                        Ok(bitmap) => card.image = Some(bitmap),
                        Err(error) => crate::logging::error(format!("upload card: {error}")),
                    }
                    self.dirty = true;
                }
            }
            Command::SetNoteColor { id, color } => {
                if let Some(card) = self.cards.iter_mut().find(|card| card.id == id) {
                    if card.note && card.note_color != color.min(3) {
                        card.note_color = color.min(3);
                        self.dirty = true;
                    }
                }
            }
            Command::RemoveCard { id } => {
                let before = self.cards.len();
                self.cards.retain(|card| card.id != id);
                if self.cards.len() != before {
                    if self.hover == Some(id) {
                        self.hover = None;
                    }
                    if self.hover_action.is_some_and(|(hovered, _)| hovered == id) {
                        self.hover_action = None;
                    }
                    if self.pressed == Some(id) {
                        self.pressed = None;
                    }
                    if self.dragging == Some(id) {
                        self.dragging = None;
                    }
                    if self.copied == Some(id) {
                        self.copied = None;
                    }
                    if self.pin_hover == Some(id) {
                        self.pin_hover = None;
                    }
                    if self.grab.as_ref().is_some_and(|grab| grab.id == id) {
                        self.grab = None;
                    }
                    self.relayout_cards();
                    self.dirty = true;
                }
            }
            Command::Hover(id) => {
                if self.hover != id {
                    self.hover = id;
                    self.hover_action = None;
                    self.dirty = true;
                }
            }
            Command::HoverAction(action) => {
                if self.hover_action != action {
                    self.hover_action = action;
                    self.dirty = true;
                }
            }
            Command::Press(id) => {
                if self.pressed != id {
                    self.pressed = id;
                    self.dirty = true;
                }
            }
            Command::Dragging(id) => {
                if self.dragging != id {
                    self.dragging = id;
                    if id.is_some() {
                        // An OLE drag-out owns the pointer; a pin drag cannot
                        // also be running.
                        self.end_grab(now);
                        self.hover_action = None;
                    }
                    self.dirty = true;
                }
            }
            Command::Grab { id, pointer } => self.begin_grab(id, pointer, now),
            Command::EndGrab => self.end_grab(now),
            Command::PinHover(id) => {
                if self.pin_hover != id {
                    self.pin_hover = id;
                    self.dirty = true;
                }
            }
            Command::SetFlying { id, flying } => {
                // Where a flight lands: one spacing beyond the current last
                // card, so the invisible scene card is already there when the
                // flight window hands over.
                let visible = self.cards.len().saturating_sub(1);
                let landing_x = layout::flight_slot_x(visible, visible + 1, self.width_dip);
                let mut changed = false;
                if let Some(card) = self.cards.iter_mut().find(|card| card.id == id) {
                    if card.flying != flying {
                        card.flying = flying;
                        card.arrived = true;
                        if flying {
                            // The flight window is drawing the card until it
                            // lands; the one on the line stays invisible
                            // underneath it (BEHAVIOR §5.2). Its swing starts
                            // from rest so the flight window can mirror the
                            // same landing sway from a known state.
                            card.opacity = 0.0;
                            card.swing.snap(0.0);
                            card.pendulum.snap(0.0);
                            card.flex.snap(0.0);
                            card.last_vx = 0.0;
                            card.last_vy = 0.0;
                            card.nudge_t = -1.0;
                            card.x.value = landing_x;
                            card.x.velocity = 0.0;
                            card.x.target = landing_x;
                        } else {
                            // Landing starts the row glide immediately. The
                            // real card is already opaque: the flight window,
                            // which mirrors the same row spring and 2.2° sway,
                            // is still on top and fades out over it. Two
                            // identical, aligned copies therefore composite at
                            // full opacity — no double-exposure, no mid-fade
                            // transparency dip.
                            card.opacity = 1.0;
                            card.x.velocity = 0.0;
                            card.last_vx = 0.0;
                            card.last_vy = 0.0;
                            card.flex.snap(0.0);
                            card.nudge(2.2);
                        }
                        changed = true;
                    }
                }
                if changed {
                    self.dirty = true;
                    self.relayout_cards();
                }
            }
            Command::SetRehang { id, flying } => self.set_rehang(id, flying),
            Command::NotePeek(on) => {
                let target = if on { 1.0 } else { 0.0 };
                if (self.note_peek.target - target).abs() > f32::EPSILON {
                    self.note_peek.target = target;
                    self.dirty = true;
                }
            }
            Command::HideNotePeek => {
                self.note_peek.snap(0.0);
                self.dirty = true;
            }
            Command::Copied(id) => {
                self.copied = Some(id);
                self.copied_until = now + COPIED_SECONDS;
                if let Some(card) = self.cards.iter_mut().find(|card| card.id == id) {
                    card.nudge(3.0);
                }
                self.dirty = true;
            }
            Command::Gust => self.gust(now),
            Command::WallpaperChanged => self.dirty = true,
            Command::SetFrameCap(_) | Command::Stop => {}
        }
    }

    /// The pin anchor in window DIP: the slot centre and the rope height at
    /// the current x, plus the card's drop and the line's slide.
    fn pin_window_pos(&self, card: &Card) -> (f32, f32) {
        (
            card.x.value,
            layout::rope_y(card.x.value, self.width_dip) - layout::PIN_ABOVE
                + card.drop.value
                + self.slide.value,
        )
    }

    fn begin_grab(&mut self, id: CardId, pointer: (f32, f32), now: f64) {
        let Some(index) = self.cards.iter().position(|card| card.id == id) else {
            return;
        };
        {
            let card = &self.cards[index];
            if card.flying || card.rehang || !card.arrived || card.hop_t >= 0.0 {
                return;
            }
        }
        let pin = self.pin_window_pos(&self.cards[index]);
        // A new grab supersedes any stale one; there is no animation to run
        // because the old card never left the vector.
        self.grab = None;
        let card = &mut self.cards[index];
        card.x.set_response(GRAB_RESPONSE, GRAB_DAMPING);
        card.x.target = card.x.value;
        card.drop
            .set_response(GRAB_DROP_RESPONSE, GRAB_DROP_DAMPING);
        card.pendulum.snap(0.0);
        card.flex.snap(0.0);
        card.last_vx = card.x.velocity;
        card.last_vy = card.drop.velocity;
        card.press.set_response(0.12, 1.0);
        card.press.target = 1.0;
        card.release_spring = false;
        self.grab = Some(Grab {
            id,
            offset: (pin.0 - pointer.0, pin.1 - pointer.1),
            press: pointer,
            pointer,
            move_dir: 0,
            moved: false,
            swap_dir: 0,
            swap_anchor: card.x.value,
            swap_since: None,
            cooldown_until: now,
        });
        self.pin_hover = Some(id);
        self.hover = None;
        self.hover_action = None;
        self.dirty = true;
    }

    pub fn end_grab(&mut self, _now: f64) {
        let Some(grab) = self.grab.take() else {
            return;
        };
        // A swap may have changed the vector order; the row settles into the
        // new slots on the release spring. This is also the cancel path, so it
        // must not rely on the pointer still being over anything.
        self.relayout_cards();
        if let Some(card) = self.cards.iter_mut().find(|card| card.id == grab.id) {
            card.x.set_response(RELEASE_RESPONSE, RELEASE_DAMPING);
            card.x.velocity = card.x.velocity.clamp(-600.0, 600.0);
            card.drop.target = 0.0;
            card.drop.set_response(0.42, 0.72);
            card.press.set_response(0.3, 0.6);
            card.press.target = 1.0;
            // Keep the current pendulum angle and angular velocity: the
            // release deceleration is what makes the card swing back.
            card.pendulum.target = 0.0;
            card.release_spring = true;
        }
        self.pin_hover = None;
        self.dirty = true;
    }

    /// Turns the latest pointer sample into the dragged card's targets. The
    /// pointer may not move the card outside the window: the card's half width
    /// is the clamp, and the rubber band caps the drop at 26 DIP.
    fn apply_grab_targets(&mut self, grab: &Grab) {
        let half = layout::CARD_WIDTH * 0.5;
        let target_x = (grab.pointer.0 + grab.offset.0).clamp(half, self.width_dip - half);
        // `offset` was taken from the pin's window position, which includes the
        // whole line's slide; cancel that here so a grab during the reveal does
        // not add the slide to the card's own drop.
        let rest_y =
            layout::rope_y(target_x, self.width_dip) - layout::PIN_ABOVE + self.slide.value;
        let raw = (grab.pointer.1 + grab.offset.1 - rest_y).max(0.0);
        let drop_target = rubber_y(raw);
        if let Some(card) = self.cards.iter_mut().find(|card| card.id == grab.id) {
            card.x.target = target_x;
            card.drop.target = drop_target;
        }
    }

    /// Pushes the cards ahead of the dragged one and arms the swap when the
    /// pre-collision penetration is held past the threshold.
    fn update_contacts(&mut self, grab: &mut Grab, index: usize, now: f64) {
        for card in &mut self.cards {
            card.squeeze.target = 0.0;
        }
        let dir = grab.move_dir;
        // Count the visible cards without collecting them: this runs every
        // frame of a drag, so it must not allocate.
        let visible = |card: &Card| !card.flying && !card.rehang;
        if !visible(&self.cards[index]) {
            grab.swap_since = None;
            return;
        }
        let count = self.cards.iter().filter(|card| visible(card)).count();
        let vpos = self.cards[..index]
            .iter()
            .filter(|card| visible(card))
            .count();
        if dir == 0 {
            grab.swap_since = None;
            return;
        }
        let (a_x, a_half) = (
            self.cards[index].x.value,
            self.cards[index].card_size.0 * 0.5,
        );
        // The front edge of the card just pushed; the next card tests against
        // it. The first neighbour gets the full cap, the second a soft one.
        let mut front = if dir > 0 { a_x + a_half } else { a_x - a_half };
        let mut first_pen = 0.0;
        let mut first_neighbour = None;
        let mut cursor = index;
        for hop in 0..2 {
            let next = if dir > 0 {
                (cursor + 1..self.cards.len()).find(|&i| visible(&self.cards[i]))
            } else {
                (0..cursor).rev().find(|&i| visible(&self.cards[i]))
            };
            let Some(j) = next else {
                break;
            };
            cursor = j;
            if hop == 0 {
                first_neighbour = Some(j);
            }
            let card = &mut self.cards[j];
            let position = if dir > 0 {
                vpos + hop + 1
            } else {
                vpos - hop - 1
            };
            let slot = layout::slot_x(position, count, self.width_dip);
            let cap = if hop == 0 {
                SQUEEZE_CAP
            } else {
                SQUEEZE_CHAIN_CAP
            };
            let pen = if dir > 0 {
                squeeze_push(front, slot, card.card_size.0, cap)
            } else {
                // Mirrored: the neighbour's right edge is in front of the
                // dragged card's left edge, so penetration is right minus left.
                (slot + card.card_size.0 * 0.5 - front).clamp(0.0, cap)
            };
            if hop == 0 {
                first_pen = pen;
            }
            if dir > 0 {
                card.x.target = slot + pen;
                front = slot + pen + card.card_size.0 * 0.5;
            } else {
                card.x.target = slot - pen;
                front = slot - pen - card.card_size.0 * 0.5;
            }
            // The neighbour leans away from the squeeze: a right push turns
            // it counter-clockwise (negative degrees), a left push clockwise.
            card.squeeze.target = -(pen / SQUEEZE_CAP) * SQUEEZE_ANGLE * dir as f32;
        }

        let Some(neighbour) = first_neighbour else {
            grab.swap_since = None;
            return;
        };
        if first_pen < SWAP_PENETRATION {
            grab.swap_since = None;
            return;
        }
        if now < grab.cooldown_until {
            return;
        }
        // The reverse swap must wait until the dragged card has retreated far
        // enough from the anchor of the previous swap; otherwise the two cards
        // would trade places on every frame around the threshold.
        if grab.swap_dir != 0 && dir == -grab.swap_dir {
            let retreat = (a_x - grab.swap_anchor) * grab.swap_dir as f32;
            if retreat > -SWAP_REARM_DIP {
                return;
            }
        }
        let since = grab.swap_since.get_or_insert(now);
        if now - *since >= SWAP_DWELL {
            self.do_swap(grab, index, neighbour, dir, now);
        }
    }

    fn do_swap(&mut self, grab: &mut Grab, a_index: usize, b_index: usize, dir: i32, now: f64) {
        let visible = |card: &Card| !card.flying && !card.rehang;
        let count = self.cards.iter().filter(|card| visible(card)).count();
        let vpos = self.cards[..a_index]
            .iter()
            .filter(|card| visible(card))
            .count();
        let slot = layout::slot_x(vpos, count, self.width_dip);
        let a_x = self.cards[a_index].x.value;
        let a_velocity = self.cards[a_index].x.velocity;
        self.cards.swap(a_index, b_index);
        // The neighbour takes the dragged card's old slot, pops towards it and
        // hops a little; the dragged card keeps following the pointer and
        // gets a recoil away from the card that just left.
        let neighbour = &mut self.cards[a_index];
        // Capture the pre-pop velocity so the next update's acceleration
        // includes the whole impulse, not just the spring's own step.
        neighbour.last_vx = neighbour.x.velocity;
        neighbour.x.set_response(0.28, 0.75);
        neighbour.x.target = slot;
        neighbour.x.velocity = -(SWAP_POP_VELOCITY) * dir as f32;
        neighbour.hop_t = 0.0;
        // The pop uses a faster spring than the row; restore the row response
        // once it has settled so later relayouts feel like every other card.
        neighbour.release_spring = true;
        let dragged = &mut self.cards[b_index];
        dragged.last_vx = a_velocity;
        dragged.x.velocity = a_velocity - SWAP_RECOIL_VELOCITY * dir as f32;
        grab.swap_anchor = a_x;
        grab.swap_dir = dir;
        grab.swap_since = None;
        grab.cooldown_until = now + SWAP_COOLDOWN;
        for card in &mut self.cards {
            card.squeeze.target = 0.0;
        }
        self.dirty = true;
    }

    fn relayout_cards(&mut self) {
        let flying = self.cards.iter().position(|card| card.flying);
        let targets = row_targets(self.cards.len(), flying, self.width_dip);
        for (card, target) in self.cards.iter_mut().zip(targets) {
            card.x.target = target;
        }
        // The entry hangs off the last card: when the row re-centres after a
        // removal, its corner must glide on the same spring, not jump. While
        // the corner is hidden there is nothing to see, so snap to the new
        // slot and keep the frame loop asleep.
        if let Some(center) = self.note_entry_center() {
            self.note_peek_x.target = center;
            if self.note_peek.value <= 0.001 {
                self.note_peek_x.snap(center);
            }
        }
    }

    /// Horizontal centre of the invisible note entry in its target slot, in
    /// window DIP. `None` when the entry is clamped off the screen.
    fn note_entry_center(&self) -> Option<f32> {
        let (card_height, note) = self
            .cards
            .last()
            .map(|card| (card.card_size.1, card.note))
            .unwrap_or((layout::NOTE_CARD_HEIGHT, true));
        layout::note_hotspot(self.cards.len(), self.width_dip, card_height, note)
            .map(|hotspot| hotspot.center().0)
    }

    /// Hides an existing card under the flight that is bringing it back, or
    /// shows it again on landing. The row layout is deliberately left alone.
    fn set_rehang(&mut self, id: CardId, flying: bool) {
        if let Some(card) = self.cards.iter_mut().find(|card| card.id == id) {
            if card.rehang != flying {
                card.rehang = flying;
                card.arrived = true;
                if flying {
                    // The flight draws the note from the editor back to this
                    // slot; the card waits invisible in place and the rest of
                    // the row never moves.
                    card.opacity = 0.0;
                    card.swing.snap(0.0);
                    card.pendulum.snap(0.0);
                    card.flex.snap(0.0);
                    card.last_vx = 0.0;
                    card.last_vy = 0.0;
                    card.nudge_t = -1.0;
                } else {
                    card.opacity = 1.0;
                    card.last_vx = 0.0;
                    card.last_vy = 0.0;
                    card.flex.snap(0.0);
                    card.nudge(2.2);
                }
                self.dirty = true;
            }
        }
    }

    fn gust(&mut self, now: f64) {
        for card in &mut self.cards {
            let delay = self.rng.range(0.0, 0.35);
            let amount = self.rng.range(1.6, 3.4);
            // The original used a timer per card; a countdown on the card has
            // the same stagger without extra timers.
            card.schedule_gust(amount, delay);
        }
        self.next_gust = now + self.rng.range(7.0, 16.0) as f64;
        self.dirty = true;
    }

    /// Advances the scene by `dt` seconds. Returns true while anything moves.
    pub fn update(&mut self, dt: f32, now: f64) -> bool {
        if !self.visible {
            return false;
        }
        let mut animating = false;

        // The entry note's peeking corner runs on the scene clock, so the hint
        // slides in/out at display rate instead of the UI tick.
        let peek_before = self.note_peek.value;
        self.note_peek.step(dt);
        animating |= !self.note_peek.settled();
        if (self.note_peek.value - peek_before).abs() > 0.0001 {
            self.dirty = true;
        }
        if self.note_peek.value > 0.001 {
            // The corner is visible (or still retracting): let it glide along
            // with the row instead of snapping to the new slot.
            let x_before = self.note_peek_x.value;
            self.note_peek_x.step(dt);
            if (self.note_peek_x.value - x_before).abs() > 0.0001 {
                self.dirty = true;
            }
            animating |= !self.note_peek_x.settled();
        } else {
            // Nothing is drawn; start the next peek from the right slot.
            self.note_peek_x.snap(self.note_peek_x.target);
        }

        // Slide: a spring when opening, a 0.22 s quadratic ease when closing.
        if self.revealed {
            self.hide_t = -1.0;
            self.slide.target = 0.0;
            self.slide.step(dt);
            animating |= !self.slide.settled();
        } else {
            if self.hide_t < 0.0 {
                self.hide_t = 0.0;
                self.hide_from = self.slide.value;
            }
            if self.hide_t < 1.0 {
                animating = true;
                self.hide_t = (self.hide_t + dt / HIDE_SECONDS).min(1.0);
                self.slide.value = self.hide_from
                    + (layout::HIDDEN_SLIDE - self.hide_from) * self.hide_t * self.hide_t;
                self.slide.velocity = 0.0;
                self.dirty = true;
            }
        }

        if !self.revealed && self.hide_t >= 1.0 {
            // The line is fully tucked away: freeze the cards, stop the breeze
            // clock and let the render thread block until the line comes back.
            if self.dirty {
                self.publish(now);
            }
            return false;
        }

        // Hint fades with the card count.
        let hint_target = if self.cards.is_empty() { 1.0 } else { 0.0 };
        if (self.hint_alpha - hint_target).abs() > 0.001 {
            self.hint_alpha += (hint_target - self.hint_alpha) * (dt / 0.3 * 2.0).min(1.0);
            animating = true;
            self.dirty = true;
        }

        if self.copied.is_some() && now >= self.copied_until {
            self.copied = None;
            self.dirty = true;
        }

        if now >= self.next_gust && !self.cards.is_empty() && self.dragging.is_none() {
            self.gust(now);
        }

        // The pin drag is integrated before the cards: its follow target comes
        // from the render thread's latest sample, the squeeze targets from the
        // row's current spring values, and a swap may reorder the vector before
        // the cards step.
        if let Some(mut grab) = self.grab.take() {
            if let Some(index) = self.cards.iter().position(|card| card.id == grab.id) {
                self.apply_grab_targets(&grab);
                self.update_contacts(&mut grab, index, now);
                self.grab = Some(grab);
            }
        }
        let grab_id = self.grab.as_ref().map(|grab| grab.id);
        let grab_moved = self.grab.as_ref().is_some_and(|grab| grab.moved);
        let grab_dir = self.grab.as_ref().map(|grab| grab.swap_dir).unwrap_or(0);
        let pin_hover = self.pin_hover;
        let hover = self.hover;
        let pressed = self.pressed;
        let dragging = self.dragging;
        let copied = self.copied;
        // Take the trace out of `self` so the card loop can hold `cards`
        // mutably without a second borrow of the scene.
        let mut trace = self.trace.take();
        for card in self.cards.iter_mut() {
            let was = card.x.value;
            card.x.step(dt);
            // Once the release spring has arrived, hand the card back to the
            // slower row spring so later reflows feel like the rest of the row.
            if card.release_spring && card.x.settled() {
                card.x.set_response(ROW_RESPONSE, ROW_DAMPING);
                card.release_spring = false;
            }
            card.drop.step(dt);
            let last_vy = card.last_vy;
            card.last_vy = card.drop.velocity;
            let ay = if dt >= 0.001 {
                ((card.drop.velocity - last_vy) / dt)
                    .clamp(-PENDULUM_ACCEL_CLAMP, PENDULUM_ACCEL_CLAMP)
            } else {
                0.0
            };
            let grabbed_here = grab_id == Some(card.id);
            let hovered = hover == Some(card.id) && dragging.is_none() && grab_id.is_none();
            let pressed_here = pressed == Some(card.id);
            let copying = copied == Some(card.id);

            card.step_swing(dt);

            if pressed_here {
                card.press.set_response(0.45, 1.0);
            } else {
                card.press.set_response(0.3, 0.6);
            }
            card.press.target = if grabbed_here {
                1.0
            } else if pressed_here {
                0.95
            } else if hovered {
                1.035
            } else {
                1.0
            };
            card.press.step(dt);

            // The clothespin's own state. The card follows the pointer at full
            // opacity while the pin shrinks slightly under the press, grows
            // and glows on hover and settles at 1.08 while dragging.
            let (pin_scale, pin_glow) = if grabbed_here {
                if grab_moved {
                    (PIN_DRAG_SCALE, PIN_DRAG_GLOW)
                } else {
                    (PIN_PRESS_SCALE, PIN_PRESS_GLOW)
                }
            } else if pin_hover == Some(card.id) {
                (PIN_HOVER_SCALE, PIN_HOVER_GLOW)
            } else {
                (1.0, 0.0)
            };
            if grabbed_here {
                card.pin_scale.set_response(0.12, 1.0);
            } else {
                card.pin_scale.set_response(0.16, 0.85);
            }
            card.pin_scale.target = pin_scale;
            card.pin_glow.target = pin_glow;
            card.pin_hot.target = if pin_hover == Some(card.id) || grabbed_here {
                1.0
            } else {
                0.0
            };
            card.pin_scale.step(dt);
            card.pin_glow.step(dt);
            card.pin_hot.step(dt);

            // The card is a damped pendulum hanging from the pin. Its natural
            // frequency comes from its own effective length and the effective
            // gravity at the pin: a pin accelerating downwards makes the card
            // lighter, while the drop's vertical motion also excites a swing.
            // The X spring's velocity difference keeps this frame-rate
            // independent and needs no extra simulation state.
            let last_vx = card.last_vx;
            card.last_vx = card.x.velocity;
            let ax = if dt >= 0.001 {
                ((card.x.velocity - last_vx) / dt)
                    .clamp(-PENDULUM_ACCEL_CLAMP, PENDULUM_ACCEL_CLAMP)
            } else {
                0.0
            };
            let g_eff = (GRAVITY_DIP - ay).clamp(GRAVITY_DIP * 0.25, GRAVITY_DIP * 1.75);
            card.pendulum
                .set_omega((g_eff / card.pendulum_length).sqrt());
            let swinging = grabbed_here || card.hop_t >= 0.0 || card.release_spring;
            card.pendulum.target = if swinging
                && !card.flying
                && !card.rehang
                && card.arrived
                && ax.abs() > PENDULUM_DEADZONE
            {
                (ax / g_eff)
                    .atan()
                    .to_degrees()
                    .clamp(-PENDULUM_MAX_DEG, PENDULUM_MAX_DEG)
            } else {
                0.0
            };
            // Still air resists the card's own translation: the point halfway
            // down the card moves with the pin plus the rotation, and the drag
            // there is a torque about the pin. It is a force, not a target, so
            // a card dragged at constant speed keeps a small backward tilt.
            let wind = if swinging && !card.flying && !card.rehang && card.arrived {
                relative_wind(
                    card.pendulum.angle,
                    card.pendulum.velocity,
                    card.x.velocity,
                    card.drop.velocity,
                    card.card_size.1,
                    card.wind_gain,
                )
            } else {
                0.0
            };
            card.pendulum.step_with(dt, wind);
            // A paper card's bottom edge lags the pin by a small shear. This
            // is visual only; hit testing still uses the rigid angle.
            card.flex.target = if card.flying || card.rehang {
                0.0
            } else {
                (card.pendulum.velocity * FLEX_GAIN).clamp(-FLEX_MAX_DEG, FLEX_MAX_DEG)
            };
            card.flex.step(dt);
            if trace.is_some() {
                let row = TraceRow {
                    t: now,
                    id: card.id,
                    grabbed: grabbed_here,
                    hop: card.hop_t >= 0.0,
                    release: card.release_spring,
                    x: card.x.value,
                    vx: card.x.velocity,
                    drop: card.drop.value,
                    vy: card.drop.velocity,
                    angle: card.pendulum.angle,
                    omega: card.pendulum.velocity,
                    target: card.pendulum.target,
                    ax,
                    ay,
                    g_eff,
                    wind,
                    flex: card.flex.value,
                    swap_dir: grab_dir,
                };
                let failed = if let Some(writer) = trace.as_mut() {
                    writer.row(&row).is_err()
                } else {
                    false
                };
                if failed {
                    trace = None;
                }
            }
            // The squeeze target was written by `update_contacts`; the spring
            // advances here so the neighbour lags the push by ~80 ms.
            card.squeeze.step(dt);
            if card.hop_t >= 0.0 {
                card.hop_t = (card.hop_t + dt / SWAP_HOP_SECONDS).min(1.0);
                if card.hop_t >= 1.0 {
                    card.hop_t = -1.0;
                }
            }

            // Fade-in follows the drop; a card only becomes visible once it has
            // arrived, and a card in flight is drawn by the flight window.
            if card.flying || card.rehang {
                card.opacity = 0.0;
            } else if !card.arrived {
                card.opacity = 0.0;
                if !card.arriving || card.drop.value > -0.5 {
                    card.arrived = true;
                }
            } else {
                card.opacity += (1.0 - card.opacity) * (dt / FADE_SECONDS * 2.0).min(1.0);
                if card.drop.value < -0.5 {
                    card.opacity = (1.0 + card.drop.value / 46.0).clamp(0.0, 1.0);
                }
            }

            card.cross_alpha +=
                ((if hovered { 1.0 } else { 0.0 }) - card.cross_alpha) * (dt * 12.0).min(1.0);
            card.copied_alpha +=
                ((if copying { 1.0 } else { 0.0 }) - card.copied_alpha) * (dt * 12.0).min(1.0);
            let show_actions = hovered && !copying && !pressed_here && card.arrived;
            card.actions_alpha += ((if show_actions { 1.0 } else { 0.0 }) - card.actions_alpha)
                * (dt * 14.0).min(1.0);

            animating |= !card.x.settled()
                || !card.drop.settled()
                || !card.swing_at_rest()
                || (card.press.value - card.press.target).abs() > 0.002
                || (card.opacity - if card.flying || card.rehang { 0.0 } else { 1.0 }).abs()
                    > 0.002
                // Compare against the target, not zero: these are approached
                // asymptotically, so `alpha > 0.002` stays true forever once a
                // card is hovered and the thread would never go idle again.
                || (card.cross_alpha - if hovered { 1.0 } else { 0.0 }).abs() > 0.002
                || (card.actions_alpha - if show_actions { 1.0 } else { 0.0 }).abs() > 0.002
                || (card.copied_alpha - if copying { 1.0 } else { 0.0 }).abs() > 0.002
                || !card.pin_scale.settled()
                || !card.pin_glow.settled()
                || !card.pin_hot.settled()
                || !card.pendulum.settled()
                || !card.flex.settled()
                || !card.squeeze.settled()
                || card.hop_t >= 0.0
                || (card.x.value - was).abs() > 0.001;
            if (card.x.value - was).abs() > 0.01 {
                self.dirty = true;
            }
        }

        if animating || self.dirty {
            self.publish(now);
        }
        self.trace = trace;
        animating || self.dirty
    }

    /// The button row a card of this kind publishes and draws.
    fn actions_for(&self, note: bool) -> &ActionsLayout {
        if note {
            &self.note_actions
        } else {
            &self.actions
        }
    }

    /// Rebuilds the hit boxes and publishes them to the UI thread.
    fn publish(&mut self, _now: f64) {
        let mut snapshot = match self.snapshot.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        snapshot.scale = self.scale;
        snapshot.slide = self.slide.value;
        snapshot.width_dip = self.width_dip;
        // The UI keeps the entry hotspot under the drawn corner; the corner
        // glides with the row while a card is removed or added.
        snapshot.note_entry_x = self.note_entry_center().map(|_| self.note_peek_x.value);
        snapshot.grabbing = self.grab.is_some();
        snapshot.order.clear();
        snapshot.order.extend(self.cards.iter().map(|card| card.id));
        snapshot.cards.clear();
        snapshot.cards.reserve(self.cards.len());
        for card in &self.cards {
            // A card in flight is drawn by the flight window and must not take
            // clicks on the line under it.
            if card.flying || card.rehang {
                continue;
            }
            let slot_center = card.x.value;
            let slot_left = slot_center - layout::CARD_WIDTH * 0.5;
            // The published box is in window DIP: the slot origin plus the
            // rope height, the drop offset and the slide.
            let slot_top = layout::rope_y(slot_center, self.width_dip) - layout::PIN_ABOVE
                + card.drop.value
                + card.hop_offset()
                + self.slide.value;
            let frame = card.frame_rect(slot_left).offset(0.0, slot_top);
            // The card is drawn rotated about its pin, the clothespin at the
            // top of the frame: `draw_card`'s transform puts that origin at the
            // slot centre, `slot_top` down. The published boxes stay unrotated
            // and are in the same frame, so hit testing undoes the rotation
            // about this point before testing them.
            let pin = (slot_center, slot_top);
            let grabbed = self.grab.as_ref().is_some_and(|grab| grab.id == card.id);
            let hovered =
                self.hover == Some(card.id) && self.dragging.is_none() && self.grab.is_none();
            let actions_layout = self.actions_for(card.note);
            let actions = if hovered {
                card.actions_rect(slot_left, actions_layout)
                    .offset(0.0, slot_top)
            } else {
                RectF::default()
            };
            let (primary_button, open_button) = if hovered {
                let (primary, open) = card.button_rects(slot_left, actions_layout);
                (primary.offset(0.0, slot_top), open.offset(0.0, slot_top))
            } else {
                (RectF::default(), RectF::default())
            };
            let primary_action = if card.note {
                ActionKind::Copy
            } else {
                ActionKind::Annotate
            };
            let swatches = if card.note
                && self.hover == Some(card.id)
                && self.dragging.is_none()
                && self.grab.is_none()
                && card.actions_alpha > 0.01
            {
                card.swatch_rects(slot_left)
                    .map(|rect| rect.offset(0.0, slot_top))
            } else {
                [RectF::default(); 4]
            };
            snapshot.cards.push(HitCard {
                id: card.id,
                frame,
                actions,
                primary_button,
                primary_action,
                open_button,
                note: card.note,
                angle: card.swing.value + card.tilt + card.pendulum.angle + card.squeeze.value,
                pin,
                pin_box: RectF::new(pin.0 - 10.0, pin.1 - 4.0, pin.0 + 10.0, pin.1 + 30.0),
                // A card in flight is drawn by the flight window, and the card
                // already in hand must not swallow a second press.
                pin_ready: card.arrived && !grabbed && card.hop_t < 0.0,
                dragged: grabbed,
                swatches,
            });
        }
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_stays_in_range() {
        let mut rng = Rng(12345);
        for _ in 0..1000 {
            let value = rng.range(1.6, 3.4);
            assert!((1.6..=3.4).contains(&value));
        }
    }

    #[test]
    fn the_published_pin_is_the_rotation_origin_the_card_draws_around() {
        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::types::FrameSnapshot::default()));
        let mut scene = Scene::new(
            1920,
            96,
            crate::render::ActionsLayout::default(),
            crate::render::ActionsLayout::default(),
            32.0,
            std::sync::Arc::clone(&snapshot),
        );
        // The line is part-way up, the card swings: the pin must carry the
        // slot, the drop and the slide, and the angle the pendulum and the
        // swing, or the hit boxes drift away from the card.
        scene.slide.value = -37.0;
        let mut item = card();
        item.tilt = 2.5;
        item.swing.value = 4.0;
        item.x.value = 786.0;
        item.x.target = 786.0;
        item.drop.snap(0.0);
        scene.cards.push(item);
        scene.publish(0.0);

        let frame = snapshot.lock().expect("snapshot");
        let hit = frame.cards.first().expect("one published card");
        assert!((hit.angle - 6.5).abs() < 1e-6, "angle={}", hit.angle);

        // The transform `draw_card` builds, with the local rotation origin
        // pushed through it, has to land on the published pin: that is the
        // point the UI thread turns the pointer back around.
        let slot_left = 786.0 - layout::CARD_WIDTH * 0.5;
        let slot_top =
            layout::rope_y(786.0, scene.width_dip) - layout::PIN_ABOVE + scene.cards[0].drop.value;
        let press = scene.cards[0].press.value;
        let draw =
            windows_numerics::Matrix3x2::scale_around(
                press,
                press,
                windows_numerics::Vector2 {
                    X: layout::CARD_WIDTH * 0.5,
                    Y: layout::CARD_OFFSET_BELOW_TOP,
                },
            ) * windows_numerics::Matrix3x2::rotation_around(
                hit.angle,
                windows_numerics::Vector2 {
                    X: layout::CARD_WIDTH * 0.5,
                    Y: 0.0,
                },
            ) * windows_numerics::Matrix3x2::translation(slot_left, slot_top + scene.slide.value);
        let (origin_x, origin_y) = (layout::CARD_WIDTH * 0.5, 0.0);
        let x = origin_x * draw.M11 + origin_y * draw.M21 + draw.M31;
        let y = origin_x * draw.M12 + origin_y * draw.M22 + draw.M32;
        assert!((x - hit.pin.0).abs() < 1e-4, "pin x {x} vs {}", hit.pin.0);
        assert!((y - hit.pin.1).abs() < 1e-4, "pin y {y} vs {}", hit.pin.1);
    }

    #[test]
    fn a_landing_is_opaque_under_the_flight_and_glides() {
        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::types::FrameSnapshot::default()));
        let mut scene = Scene::new(
            1920,
            96,
            crate::render::ActionsLayout::default(),
            crate::render::ActionsLayout::default(),
            32.0,
            snapshot,
        );
        scene.visible = true;
        scene.revealed = true;
        scene.slide.value = 0.0;

        // A card that has just been handed over by the flight window: it is
        // 87 DIP right of its final slot, already fully opaque (the flight
        // window is still on top of it), and both the row spring and the 2.2°
        // landing sway start on this frame.
        let mut item = card();
        item.flying = false;
        item.arrived = true;
        item.opacity = 1.0;
        item.swing.snap(0.0);
        item.nudge_t = -1.0;
        item.nudge(2.2);
        item.x.value = 1047.0;
        item.x.target = 960.0;
        item.x.velocity = 0.0;
        item.drop.snap(0.0);
        scene.cards.push(item);

        let dt = 1.0 / 60.0;
        let mut now = 0.0;
        for _ in 0..6 {
            now += f64::from(dt);
            scene.update(dt, now);
        }
        // The real card moves and sways immediately and stays opaque, so the
        // flight window can never reveal a transparency dip underneath.
        assert!((scene.cards[0].x.target - 960.0).abs() < 0.01);
        assert!(
            scene.cards[0].x.value < 1040.0,
            "x={} did not glide",
            scene.cards[0].x.value
        );
        assert_eq!(scene.cards[0].opacity, 1.0);
        assert!(scene.cards[0].nudge_t >= 0.0);
    }

    #[test]
    fn the_peek_card_only_shows_its_bottom_right_corner() {
        let entry = 1933.0;
        let (cx, cy) = layout::note_peek_center(entry);
        let angle = layout::NOTE_PEEK_ANGLE_DEG;
        let transform = windows_numerics::Matrix3x2::rotation(angle)
            * windows_numerics::Matrix3x2::translation(cx, cy);
        let apply = |x: f32, y: f32| {
            (
                x * transform.M11 + y * transform.M21 + transform.M31,
                x * transform.M12 + y * transform.M22 + transform.M32,
            )
        };
        let (card_w, card_h) = layout::note_card_size();
        let corners = [
            (-card_w * 0.5, -card_h * 0.5),
            (card_w * 0.5, -card_h * 0.5),
            (-card_w * 0.5, card_h * 0.5),
            (card_w * 0.5, card_h * 0.5),
        ];
        let screen: Vec<(f32, f32)> = corners.iter().map(|&(x, y)| apply(x, y)).collect();
        let bottom_right = screen[3];
        assert!(
            (bottom_right.0 - entry).abs() < 1e-3,
            "br={bottom_right:?} screen={screen:?}"
        );
        assert!(
            (bottom_right.1 - layout::NOTE_PEEK_VISIBLE_DIP).abs() < 1e-3,
            "br={bottom_right:?} screen={screen:?}"
        );
        for corner in &screen[..3] {
            assert!(corner.1 < 0.0, "corner {corner:?} is above the top edge");
        }
    }

    #[test]
    fn a_removed_card_glides_the_entry_instead_of_teleporting_it() {
        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::types::FrameSnapshot::default()));
        let mut scene = Scene::new(
            1920,
            96,
            crate::render::ActionsLayout::default(),
            crate::render::ActionsLayout::default(),
            32.0,
            std::sync::Arc::clone(&snapshot),
        );
        scene.visible = true;
        scene.revealed = true;
        scene.slide.snap(0.0);
        for id in 1..=3 {
            let mut item = card();
            item.id = id;
            scene.cards.push(item);
        }
        for (index, item) in scene.cards.iter_mut().enumerate() {
            item.x.snap(layout::slot_x(index, 3, 1920.0));
        }
        scene.relayout_cards();
        scene.note_peek.snap(1.0); // the pointer is over the strip
        let old_entry = layout::note_hotspot(3, 1920.0, 112.0, false)
            .expect("entry")
            .center()
            .0;
        assert!((scene.note_peek_x.value - old_entry).abs() < 0.01);

        // Removing the leading card re-centres the row: the last card and its
        // entry move half a spacing to the left.
        scene.cards.retain(|item| item.id != 1);
        scene.relayout_cards();
        let new_entry = layout::note_hotspot(2, 1920.0, 112.0, false)
            .expect("entry")
            .center()
            .0;
        assert!((scene.note_peek_x.target - new_entry).abs() < 0.01);
        assert!(
            (scene.note_peek_x.value - old_entry).abs() < 0.01,
            "relayout teleported the visible corner"
        );

        scene.update(1.0 / 60.0, 0.0);
        let value = scene.note_peek_x.value;
        assert!(
            value < old_entry - 0.01,
            "the corner did not start gliding: {value}"
        );
        assert!(
            value > new_entry,
            "the corner snapped to the new slot: {value} vs {new_entry}"
        );
        assert_eq!(
            snapshot.lock().expect("snapshot").note_entry_x,
            Some(value),
            "the UI hotspot must follow the animated corner"
        );

        // Both run the same spring from the same offset, so the entry keeps
        // exactly the last card's distance for the whole reflow.
        let offset = old_entry - layout::slot_x(2, 3, 1920.0);
        for _ in 0..180 {
            scene.update(1.0 / 60.0, 0.0);
            let last = scene.cards.last().expect("last card").x.value;
            assert!(
                (scene.note_peek_x.value - last - offset).abs() < 0.01,
                "entry lagged the row: {} vs {}",
                scene.note_peek_x.value,
                last
            );
        }
        assert!((scene.note_peek_x.value - new_entry).abs() < 0.01);
    }

    #[test]
    fn a_hidden_entry_snaps_to_the_new_slot() {
        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::types::FrameSnapshot::default()));
        let mut scene = Scene::new(
            1920,
            96,
            crate::render::ActionsLayout::default(),
            crate::render::ActionsLayout::default(),
            32.0,
            snapshot,
        );
        for id in 1..=3 {
            let mut item = card();
            item.id = id;
            scene.cards.push(item);
        }
        scene.relayout_cards();
        let old_entry = scene.note_peek_x.value;
        scene.cards.retain(|item| item.id != 1);
        scene.relayout_cards();
        assert!(scene.note_peek_x.value < old_entry);
        assert_eq!(scene.note_peek_x.value, scene.note_peek_x.target);
    }

    #[test]
    fn a_rehang_keeps_the_row_in_place_and_hides_the_card() {
        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::types::FrameSnapshot::default()));
        let mut scene = Scene::new(
            1920,
            96,
            crate::render::ActionsLayout::default(),
            crate::render::ActionsLayout::default(),
            32.0,
            std::sync::Arc::clone(&snapshot),
        );
        scene.visible = true;
        scene.revealed = true;
        scene.slide.value = 0.0;
        let mut first = card();
        first.id = 1;
        first.x.value = 786.0;
        first.x.target = 786.0;
        let mut second = card();
        second.id = 2;
        second.note = true;
        second.x.value = 960.0;
        second.x.target = 960.0;
        scene.cards.push(first);
        scene.cards.push(second);
        scene.relayout_cards();
        let targets: Vec<f32> = scene.cards.iter().map(|card| card.x.target).collect();

        scene.set_rehang(2, true);
        assert!(scene.cards[1].rehang);
        assert_eq!(scene.cards[1].opacity, 0.0);
        // The other cards must not close the gap while the note is away.
        let after: Vec<f32> = scene.cards.iter().map(|card| card.x.target).collect();
        assert_eq!(after, targets);
        scene.publish(0.0);
        assert_eq!(
            snapshot.lock().unwrap().cards.len(),
            1,
            "the returning note must not take clicks while in flight"
        );

        scene.set_rehang(2, false);
        assert!(!scene.cards[1].rehang);
        assert_eq!(scene.cards[1].opacity, 1.0);
        assert!(scene.cards[1].nudge_t >= 0.0, "landing must sway");
        scene.publish(0.0);
        assert_eq!(snapshot.lock().unwrap().cards.len(), 2);
    }

    fn card() -> Card {
        Card::new(
            CardSpec {
                id: 1,
                card_size: (144.0, 112.0),
                tilt: 0.0,
                arriving: true,
                image: None,
                note: false,
            },
            100.0,
        )
    }

    #[test]
    fn slot_order_follows_the_card_list() {
        // Three cards in a 1920 DIP line: 786 / 960 / 1134.
        assert!((layout::slot_x(0, 3, 1920.0) - 786.0).abs() < 1e-3);
        assert!((layout::slot_x(1, 3, 1920.0) - 960.0).abs() < 1e-3);
        assert!((layout::slot_x(2, 3, 1920.0) - 1134.0).abs() < 1e-3);
    }

    #[test]
    fn a_flying_card_waits_at_the_right_end_until_it_lands() {
        let width = 1920.0;
        // Four cards, the last one still in the air. The three cards already
        // hanging keep the slots they had, and the flier waits a whole spacing
        // to the right of the third card, clear of it.
        let held = row_targets(4, Some(3), width);
        for (index, target) in held.iter().enumerate().take(3) {
            assert!((target - layout::slot_x(index, 3, width)).abs() < 1e-4);
        }
        assert!((held[3] - layout::flight_slot_x(3, 4, width)).abs() < 1e-4);
        assert!((held[3] - layout::slot_x(3, 4, width) - 87.0).abs() < 1e-4);

        // Landing slides the whole row left by half a spacing: the flier moves
        // from its landing spot to its final slot, the others by the same 87.
        let landed = row_targets(4, None, width);
        for (index, target) in landed.iter().enumerate() {
            assert!((target - layout::slot_x(index, 4, width)).abs() < 1e-4);
        }
        for (target, old) in landed.iter().zip(&held) {
            assert!((target - old + 87.0).abs() < 1e-3);
        }
    }

    #[test]
    fn no_flight_means_the_plain_centred_row() {
        let width = 1920.0;
        let targets = row_targets(3, None, width);
        assert_eq!(targets.len(), 3);
        for (index, target) in targets.iter().enumerate() {
            assert!((target - layout::slot_x(index, 3, width)).abs() < 1e-4);
        }
    }

    #[test]
    fn gust_eases_out_and_settles_instead_of_repeating() {
        let mut card = card();
        card.schedule_gust(3.0, 0.25);
        let mut max_angle = 0.0f32;
        // Ten seconds at 60 Hz: a 0.25 s delay, a 0.3 s ease-out and then the
        // damped spring. The old state machine restarted the nudge every 0.3 s
        // and never settled (the visible "twitch").
        for _ in 0..600 {
            card.step_swing(1.0 / 60.0);
            max_angle = max_angle.max(card.swing.value.abs());
        }
        assert!(
            card.swing_at_rest(),
            "swing did not settle: value={} velocity={} nudge_t={}",
            card.swing.value,
            card.swing.velocity,
            card.nudge_t
        );
        assert!(
            max_angle < 20.0,
            "gust produced an implausible {max_angle} degrees"
        );
        assert_eq!(card.nudge_t, -1.0);
    }

    #[test]
    fn a_pending_gust_keeps_the_spring_running() {
        let mut card = card();
        // Give the spring somewhere to go, so "did it move" is observable.
        card.swing.value = 10.0;
        card.schedule_gust(3.0, 0.3);
        let before = card.swing.value;
        // During the delay the card must keep moving, not freeze.
        card.step_swing(0.1);
        assert!(card.nudge_delay < 0.3);
        assert!(card.nudge_t < 0.0);
        assert!((card.swing.value - before).abs() > 1e-4);
    }

    #[test]
    fn a_gust_swings_past_zero_once_and_decays() {
        let mut card = card();
        card.schedule_gust(3.0, 0.0);
        // Run out the ease first.
        while card.nudge_t >= 0.0 {
            card.step_swing(1.0 / 60.0);
        }
        // The spring is underdamped, so it crosses the rope once and comes back
        // with a smaller overshoot. It must never look like a jump cut.
        let mut minimum = 0.0f32;
        for _ in 0..600 {
            card.step_swing(1.0 / 60.0);
            minimum = minimum.min(card.swing.value);
        }
        assert!(
            (-2.5..-0.5).contains(&minimum),
            "first overshoot was {minimum} degrees"
        );
        assert!(
            card.swing_at_rest(),
            "value={} velocity={}",
            card.swing.value,
            card.swing.velocity
        );
    }

    #[test]
    fn a_finished_gust_releases_the_spring_back_to_zero() {
        let mut card = card();
        card.schedule_gust(3.0, 0.0);
        for _ in 0..30 {
            card.step_swing(1.0 / 60.0);
            if card.nudge_t < 0.0 {
                break;
            }
        }
        assert_eq!(card.nudge_t, -1.0);
        assert!(
            (card.swing.value - 3.0).abs() < 0.2,
            "value={} nudge_t={} delay={}",
            card.swing.value,
            card.nudge_t,
            card.nudge_delay
        );
        for _ in 0..300 {
            card.step_swing(1.0 / 60.0);
        }
        assert!(
            card.swing_at_rest(),
            "value={} velocity={}",
            card.swing.value,
            card.swing.velocity
        );
    }

    #[test]
    fn pictures_publish_annotate_while_notes_keep_copy() {
        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::types::FrameSnapshot::default()));
        let mut scene = Scene::new(
            1920,
            96,
            ActionsLayout {
                left_width: 74.0,
                right_width: 58.0,
                gap: 6.0,
                total_width: 138.0,
                height: 26.0,
            },
            ActionsLayout {
                left_width: 58.0,
                right_width: 58.0,
                gap: 6.0,
                total_width: 122.0,
                height: 26.0,
            },
            32.0,
            std::sync::Arc::clone(&snapshot),
        );
        scene.visible = true;
        scene.revealed = true;
        scene.slide.snap(0.0);
        let mut picture = card();
        picture.id = 1;
        picture.x.value = 786.0;
        picture.x.target = 786.0;
        let mut note = card();
        note.id = 2;
        note.note = true;
        note.x.value = 960.0;
        note.x.target = 960.0;
        scene.cards.push(picture);
        scene.cards.push(note);

        scene.hover = Some(1);
        scene.publish(0.0);
        let frame = snapshot.lock().expect("snapshot");
        let picture = frame.cards.iter().find(|card| card.id == 1).unwrap();
        assert_eq!(picture.primary_action, ActionKind::Annotate);
        assert!(!picture.primary_button.is_empty());
        assert_eq!(
            frame.action_at(
                picture.primary_button.center().0,
                picture.primary_button.center().1
            ),
            Some((1, ActionKind::Annotate))
        );

        drop(frame);
        scene.hover = Some(2);
        scene.publish(0.0);
        let frame = snapshot.lock().expect("snapshot");
        let note = frame.cards.iter().find(|card| card.id == 2).unwrap();
        assert_eq!(note.primary_action, ActionKind::Copy);
        assert_eq!(
            frame.action_at(
                note.primary_button.center().0,
                note.primary_button.center().1
            ),
            Some((2, ActionKind::Copy))
        );
    }

    #[test]
    fn the_rubber_band_is_linear_then_soft_and_capped() {
        assert_eq!(rubber_y(-4.0), 0.0);
        assert_eq!(rubber_y(0.0), 0.0);
        assert_eq!(rubber_y(18.0), 18.0);
        assert!((rubber_y(28.0) - 20.2).abs() < 1e-3);
        assert_eq!(rubber_y(400.0), 26.0);
    }

    #[test]
    fn squeeze_push_is_the_penetration_clamped_to_the_cap() {
        // The neighbour's left edge is at 1059; the dragged card's right edge
        // at 1000 leaves a gap, so there is nothing to push.
        assert_eq!(squeeze_push(1000.0, 1134.0, 150.0, 34.0), 0.0);
        // 10 DIP of penetration pushes by exactly 10.
        assert_eq!(squeeze_push(1069.0, 1134.0, 150.0, 34.0), 10.0);
        // Deep penetration is capped instead of shoving the row off screen.
        assert_eq!(squeeze_push(1135.0, 1134.0, 150.0, 34.0), 34.0);
    }

    fn three_cards() -> (
        Scene,
        std::sync::Arc<std::sync::Mutex<crate::types::FrameSnapshot>>,
    ) {
        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::types::FrameSnapshot::default()));
        let mut scene = Scene::new(
            1920,
            96,
            crate::render::ActionsLayout::default(),
            crate::render::ActionsLayout::default(),
            32.0,
            std::sync::Arc::clone(&snapshot),
        );
        scene.visible = true;
        scene.revealed = true;
        scene.slide.snap(0.0);
        for (index, id) in [1u64, 2, 3].into_iter().enumerate() {
            let mut item = card();
            item.id = id;
            item.x.value = layout::slot_x(index, 3, 1920.0);
            item.x.target = item.x.value;
            item.drop.snap(0.0);
            scene.cards.push(item);
        }
        (scene, snapshot)
    }

    #[test]
    fn a_grab_during_the_reveal_does_not_add_the_slide_to_the_drop() {
        let (mut scene, _) = three_cards();
        scene.slide.value = -30.0;
        scene.cards[1].drop.value = 0.0;
        let pin = scene.pin_window_pos(&scene.cards[1]);
        scene.begin_grab(2, pin, 0.0);
        let grab = scene.grab.take().expect("grab");
        scene.apply_grab_targets(&grab);
        assert_eq!(
            scene.cards[1].drop.target, 0.0,
            "the line's slide is not part of the card's drop"
        );
    }

    #[test]
    fn a_right_pull_squeezes_then_swaps_and_publishes_the_new_order() {
        let (mut scene, snapshot) = three_cards();
        scene.begin_grab(2, (960.0, 30.0), 0.0);
        scene.set_pointer(1060.0, 30.0, 0.0);
        let mut grab = scene.grab.take().expect("grab");
        grab.pointer = (1060.0, 30.0);
        scene.cards[1].x.value = 1060.0;

        scene.update_contacts(&mut grab, 1, 100.0);
        assert_eq!(scene.cards[1].id, 2, "the 60 ms dwell has not elapsed");
        assert!(
            scene.cards[2].x.target > 1134.0,
            "the right neighbour is pushed, target={}",
            scene.cards[2].x.target
        );

        scene.update_contacts(&mut grab, 1, 100.07);
        scene.grab = Some(grab);
        scene.publish(100.1);

        assert_eq!(scene.cards[1].id, 3);
        assert_eq!(scene.cards[2].id, 2);
        assert!(scene.cards[1].hop_t >= 0.0, "the neighbour hops off");
        assert!(scene.cards[2].x.velocity < 0.0, "the dragged card recoils");
        assert_eq!(snapshot.lock().unwrap().order, vec![1, 3, 2]);

        // The pop is a real velocity impulse: the neighbour must swing, not
        // just slide. The acceleration is read from the X spring after the
        // swap, so one update is enough to seed the pendulum.
        scene.update(1.0 / 60.0, 100.12);
        let neighbour = &scene.cards[1];
        assert!(
            neighbour.pendulum.angle.abs() > 0.05 || neighbour.pendulum.velocity.abs() > 0.05,
            "the popped neighbour swings: angle={} velocity={}",
            neighbour.pendulum.angle,
            neighbour.pendulum.velocity
        );
    }

    #[test]
    fn a_left_pull_squeezes_the_left_neighbour_and_swaps() {
        let (mut scene, _) = three_cards();
        scene.begin_grab(2, (960.0, 30.0), 0.0);
        scene.set_pointer(860.0, 30.0, 0.0);
        let mut grab = scene.grab.take().expect("grab");
        grab.pointer = (860.0, 30.0);
        scene.cards[1].x.value = 860.0;

        scene.update_contacts(&mut grab, 1, 100.0);
        assert_eq!(scene.cards[0].id, 1);
        assert!(
            scene.cards[0].x.target < 786.0,
            "the left neighbour is pushed, target={}",
            scene.cards[0].x.target
        );

        scene.update_contacts(&mut grab, 1, 100.07);
        scene.grab = Some(grab);

        assert_eq!(scene.cards[0].id, 2);
        assert_eq!(scene.cards[1].id, 1);
        assert!(scene.cards[1].x.velocity > 0.0, "the neighbour pops right");
    }

    #[test]
    fn the_pendulum_swings_with_acceleration_and_settles() {
        let (mut scene, _) = three_cards();
        scene.begin_grab(2, (960.0, 30.0), 0.0);
        scene.set_pointer(1000.0, 30.0, 0.0);
        scene.update(1.0 / 60.0, 0.016);
        assert!(
            scene.cards[1].pendulum.angle > 0.0,
            "rightward acceleration tips the bottom left, angle={}",
            scene.cards[1].pendulum.angle
        );
        for frame in 1..6 {
            scene.update(1.0 / 60.0, 0.016 * (frame + 1) as f64);
        }
        let card = &scene.cards[1];
        assert!(
            card.pendulum.angle.abs() > 0.05 || card.pendulum.velocity.abs() > 0.05,
            "the grab acceleration should tip the card: angle={} velocity={}",
            card.pendulum.angle,
            card.pendulum.velocity
        );
        assert!(
            card.flex.value.abs() > 0.01,
            "the bottom edge lags the swing, flex={}",
            card.flex.value
        );

        scene.end_grab(1.0);
        for frame in 0..240 {
            scene.update(1.0 / 60.0, 1.0 + frame as f64 / 60.0);
        }
        assert!(
            scene.cards[1].pendulum.settled(),
            "the swing damps out, angle={} velocity={}",
            scene.cards[1].pendulum.angle,
            scene.cards[1].pendulum.velocity
        );
        assert!(
            scene.cards[1].flex.settled(),
            "the shear settles with the swing, flex={}",
            scene.cards[1].flex.value
        );
    }

    #[test]
    fn a_downward_drop_makes_the_pendulum_lighter_and_slower() {
        let (mut level, _) = three_cards();
        let (mut dropped, _) = three_cards();
        for scene in [&mut level, &mut dropped] {
            scene.begin_grab(2, (960.0, 30.0), 0.0);
            let card = &mut scene.cards[1];
            card.pendulum.angle = 3.0;
            card.pendulum.velocity = 0.0;
            card.drop.value = 0.0;
            card.drop.velocity = 0.0;
            card.drop.target = 0.0;
        }
        // The card above the rope accelerates downwards; the pin's effective
        // gravity is lower, so the same 3° displacement restores more slowly.
        dropped.cards[1].drop.value = -20.0;
        level.update(1.0 / 60.0, 0.016);
        dropped.update(1.0 / 60.0, 0.016);
        assert!(
            dropped.cards[1].pendulum.angle.abs() > level.cards[1].pendulum.angle.abs(),
            "the dropping card swings slower: dropped={} level={}",
            dropped.cards[1].pendulum.angle,
            level.cards[1].pendulum.angle
        );
    }

    #[test]
    fn the_swap_hop_is_a_small_six_dip_bump() {
        assert_eq!(SWAP_HOP_DIP, 6.0);
        let mut item = card();
        item.hop_t = 0.5;
        assert!(
            (item.hop_offset() + 6.0).abs() < 1e-4,
            "peak={}",
            item.hop_offset()
        );
    }

    #[test]
    fn a_tall_card_has_more_paper_drag_than_a_wide_one() {
        let wide = paper_coefficients((144.0, 96.0));
        let tall = paper_coefficients((80.0, 140.0));
        assert!(
            tall.damping > wide.damping,
            "tall damping={} wide={}",
            tall.damping,
            wide.damping
        );
        assert!(
            tall.drag > wide.drag,
            "tall drag={} wide={}",
            tall.drag,
            wide.drag
        );
        assert!(
            tall.wind_gain > wide.wind_gain,
            "tall wind={} wide={}",
            tall.wind_gain,
            wide.wind_gain
        );
    }

    #[test]
    fn relative_wind_follows_the_direction_of_travel() {
        let right = relative_wind(0.0, 0.0, 500.0, 0.0, 100.0, 1.0e-5);
        let left = relative_wind(0.0, 0.0, -500.0, 0.0, 100.0, 1.0e-5);
        assert!(right > 0.0, "rightward wind={right}");
        assert!(left < 0.0, "leftward wind={left}");
        assert_eq!(relative_wind(0.0, 0.0, 0.0, 0.0, 100.0, 1.0e-5), 0.0);
    }
}
