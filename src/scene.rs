//! The line scene: what hangs on the rope and how it moves.
//!
//! The scene lives on the overlay's render thread. The UI thread sends
//! [`Command`]s; the render thread drains them once per frame, integrates the
//! springs with the real `dt` and draws. The UI thread reads a [`FrameSnapshot`]
//! for hit testing, so no rendering result is ever read back.

use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;

use crate::layout;
use crate::render::{ActionsLayout, Renderer, TextId};
use crate::spring::{Spring, ease};
use crate::types::{ActionKind, CardId, CardSpec, FrameSnapshot, HitCard, ImageData, RectF};

/// Time for the "Copied" label, seconds.
const COPIED_SECONDS: f64 = 1.2;
/// Time for a card to fade in, seconds (exponential time constant).
const FADE_SECONDS: f32 = 0.16;
/// Delay for the hide animation, seconds.
const HIDE_SECONDS: f32 = 0.22;

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
    /// The flight window carries this card; keep it invisible until it lands.
    SetFlying { id: CardId, flying: bool },
    /// Flash the "Copied" pill on a card.
    Copied(CardId),
    /// A gust of wind (also fired by the internal timer).
    Gust,
    /// Cap the frame rate to `1 / period` Hz; 0 follows the display.
    SetFrameCap(f64),
    /// Stop the render thread.
    Stop,
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
    image: Option<ID2D1Bitmap1>,
    note: bool,
    note_color: usize,
    /// Seconds left before this card's scheduled gust starts.
    nudge_delay: f32,
    nudge_from: f32,
    nudge_to: f32,
    /// -1 is idle, 0..1 is the ease-out progress of an active nudge.
    nudge_t: f32,
}

impl Card {
    fn new(spec: CardSpec, slot_x: f32) -> Self {
        let mut card = Self {
            id: spec.id,
            card_size: spec.card_size,
            tilt: spec.tilt,
            x: Spring::new(0.55, 0.78, slot_x),
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
            image: None,
            note: spec.note,
            note_color: 0,
            nudge_delay: 0.0,
            nudge_from: 0.0,
            nudge_to: 0.0,
            nudge_t: -1.0,
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
            RectF::from_xywh(left, top, actions.copy_width, actions.height),
            RectF::from_xywh(
                left + actions.copy_width + actions.gap,
                top,
                actions.open_width,
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

/// The line itself.
pub struct Scene {
    width_dip: f32,
    scale: f32,
    visible: bool,
    revealed: bool,
    slide: Spring,
    hide_t: f32,
    hide_from: f32,
    cards: Vec<Card>,
    hover: Option<CardId>,
    hover_action: Option<(CardId, ActionKind)>,
    pressed: Option<CardId>,
    dragging: Option<CardId>,
    copied: Option<CardId>,
    copied_until: f64,
    hint_alpha: f32,
    next_gust: f64,
    rng: Rng,
    actions: ActionsLayout,
    copied_height: f32,
    snapshot: Arc<Mutex<FrameSnapshot>>,
    dirty: bool,
}

impl Scene {
    pub fn new(
        width_px: u32,
        dpi: u32,
        hover_actions: ActionsLayout,
        copied_height: f32,
        snapshot: Arc<Mutex<FrameSnapshot>>,
    ) -> Self {
        let scale = dpi as f32 / 96.0;
        let width_dip = width_px as f32 / scale;
        let mut scene = Self {
            width_dip,
            scale,
            visible: false,
            revealed: false,
            slide: Spring::new(0.42, 0.82, layout::HIDDEN_SLIDE),
            hide_t: -1.0,
            hide_from: layout::HIDDEN_SLIDE,
            cards: Vec::new(),
            hover: None,
            hover_action: None,
            pressed: None,
            dragging: None,
            copied: None,
            copied_until: 0.0,
            hint_alpha: 0.0,
            next_gust: 7.0,
            rng: Rng(0x9E37_79B9),
            actions: hover_actions,
            copied_height,
            snapshot,
            dirty: true,
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
        if !self.visible || !self.revealed || self.cards.is_empty() || self.dragging.is_some() {
            return None;
        }
        Some((self.next_gust - now).max(0.001))
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
        self.dirty = true;
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        if !visible {
            self.hover = None;
            self.hover_action = None;
            self.pressed = None;
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
                        self.hide_t = -1.0;
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
                        self.hover_action = None;
                    }
                    self.dirty = true;
                }
            }
            Command::SetFlying { id, flying } => {
                if let Some(card) = self.cards.iter_mut().find(|card| card.id == id) {
                    if card.flying != flying {
                        card.flying = flying;
                        if !flying {
                            card.arrived = true;
                            card.nudge(2.2);
                        }
                        self.dirty = true;
                    }
                }
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
            Command::SetFrameCap(_) | Command::Stop => {}
        }
    }

    fn relayout_cards(&mut self) {
        let count = self.cards.len();
        for (index, card) in self.cards.iter_mut().enumerate() {
            card.x.target = layout::slot_x(index, count, self.width_dip);
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

        let hover = self.hover;
        let pressed = self.pressed;
        let dragging = self.dragging;
        let copied = self.copied;
        let count = self.cards.len();
        for (index, card) in self.cards.iter_mut().enumerate() {
            card.x.target = layout::slot_x(index, count, self.width_dip);
            let was = card.x.value;
            card.x.step(dt);
            card.drop.step(dt);
            let hovered = hover == Some(card.id) && dragging.is_none();
            let pressed_here = pressed == Some(card.id);
            let copying = copied == Some(card.id);

            card.step_swing(dt);

            if pressed_here {
                card.press.set_response(0.45, 1.0);
            } else {
                card.press.set_response(0.3, 0.6);
            }
            card.press.target = if pressed_here {
                0.95
            } else if hovered {
                1.035
            } else {
                1.0
            };
            card.press.step(dt);

            // Fade-in follows the drop; a card only becomes visible once it has
            // arrived (flying cards arrive through `arriving`).
            if !card.arrived {
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
                || (card.opacity - if card.flying { 0.0 } else { 1.0 }).abs() > 0.002
                || card.cross_alpha > 0.002
                || card.actions_alpha > 0.002
                || card.copied_alpha > 0.002
                || (card.x.value - was).abs() > 0.001;
            if (card.x.value - was).abs() > 0.01 {
                self.dirty = true;
            }
        }

        if animating || self.dirty {
            self.publish(now);
        }
        animating || self.dirty
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
        snapshot.cards.clear();
        snapshot.cards.reserve(self.cards.len());
        for (index, card) in self.cards.iter().enumerate() {
            let slot_center = layout::slot_x(index, self.cards.len(), self.width_dip);
            let slot_left = slot_center - layout::CARD_WIDTH * 0.5;
            // The published box is in window DIP: the slot origin plus the
            // rope height, the drop offset and the slide.
            let slot_top = layout::rope_y(slot_center, self.width_dip) - layout::PIN_ABOVE
                + card.drop.value
                + self.slide.value;
            let frame = card.frame_rect(slot_left).offset(0.0, slot_top);
            let hovered = self.hover == Some(card.id) && self.dragging.is_none();
            let actions = if hovered {
                card.actions_rect(slot_left, &self.actions)
                    .offset(0.0, slot_top)
            } else {
                RectF::default()
            };
            let (copy_button, open_button) = if hovered {
                let (copy, open) = card.button_rects(slot_left, &self.actions);
                (copy.offset(0.0, slot_top), open.offset(0.0, slot_top))
            } else {
                (RectF::default(), RectF::default())
            };
            let swatches = if card.note
                && self.hover == Some(card.id)
                && self.dragging.is_none()
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
                copy_button,
                open_button,
                note: card.note,
                swatches,
            });
        }
        self.dirty = false;
    }

    /// Draws the whole scene.
    pub fn draw(&mut self, renderer: &mut Renderer) {
        renderer.reset_transform();
        renderer.set_transform(windows_numerics::Matrix3x2::translation(
            0.0,
            self.slide.value,
        ));
        renderer.draw_rope(self.width_dip);

        if self.hint_alpha > 0.01 && self.cards.is_empty() {
            let center_x = self.width_dip * 0.5;
            let center_y = layout::rope_y(center_x, self.width_dip) + 34.0;
            let (text_w, text_h) = renderer.text_size(TextId::Hint);
            let bounds = RectF::from_xywh(
                center_x - text_w * 0.5 - 16.0,
                center_y - text_h * 0.5 - 8.0,
                text_w + 32.0,
                text_h + 16.0,
            );
            let pushed = renderer.push_opacity(bounds, self.hint_alpha);
            renderer.draw_hint(center_x, center_y);
            if pushed {
                renderer.pop_opacity();
            }
        }

        let count = self.cards.len();
        for index in 0..count {
            let slot_x = layout::slot_x(index, count, self.width_dip);
            let slot_left = slot_x - layout::CARD_WIDTH * 0.5;
            let hovered = self.hover == Some(self.cards[index].id) && self.dragging.is_none();
            self.draw_card(renderer, index, slot_left, hovered);
        }
        renderer.reset_transform();
    }

    fn draw_card(&mut self, renderer: &mut Renderer, index: usize, slot_left: f32, hovered: bool) {
        let (
            id,
            card_size,
            drop,
            x,
            tilt,
            swing,
            press,
            opacity,
            cross_alpha,
            actions_alpha,
            note,
            note_color,
        ) = {
            let card = &self.cards[index];
            (
                card.id,
                card.card_size,
                card.drop.value,
                card.x.value,
                card.tilt,
                card.swing.value,
                card.press.value,
                card.opacity,
                card.cross_alpha,
                card.actions_alpha,
                card.note,
                card.note_color,
            )
        };
        if opacity <= 0.01 {
            return;
        }
        let (card_w, card_h) = card_size;
        let slot_top = layout::rope_y(x, self.width_dip) - layout::PIN_ABOVE + drop;
        let angle = swing + tilt;
        // Row-vector matrices compose in application order: scale the frame,
        // then swing it around the pin, then move it to the slot (the slide is
        // part of the placement because this transform replaces the base one).
        let transform =
            windows_numerics::Matrix3x2::scale_around(
                press,
                press,
                windows_numerics::Vector2 {
                    X: layout::CARD_WIDTH * 0.5,
                    Y: layout::CARD_OFFSET_BELOW_TOP,
                },
            ) * windows_numerics::Matrix3x2::rotation_around(
                angle,
                windows_numerics::Vector2 {
                    X: layout::CARD_WIDTH * 0.5,
                    Y: 0.0,
                },
            ) * windows_numerics::Matrix3x2::translation(slot_left, slot_top + self.slide.value);
        let frame = RectF::from_xywh(
            (layout::CARD_WIDTH - card_w) * 0.5,
            layout::CARD_OFFSET_BELOW_TOP,
            card_w,
            card_h,
        );
        let bounds = RectF::from_xywh(
            (layout::CARD_WIDTH - card_w) * 0.5 - 30.0,
            layout::CARD_OFFSET_BELOW_TOP - 30.0,
            card_w + 60.0,
            card_h + 60.0 + layout::ACTIONS_HEIGHT + 20.0,
        );
        renderer.set_transform(transform);
        // A fully opaque card skips the layer; the card is still drawn.
        let pushed = renderer.push_opacity(bounds, opacity);

        renderer.draw_shadow(frame, if hovered { 0.26 } else { 0.18 }, hovered);
        renderer.fill_round(frame, layout::CARD_RADIUS, crate::render::Brush::Glass);
        if let Some(image) = self.cards[index].image.as_ref() {
            let photo = RectF::from_xywh(
                frame.left + layout::PHOTO_INSET,
                frame.top + layout::PHOTO_INSET,
                card_w - layout::PHOTO_INSET * 2.0,
                card_h - layout::PHOTO_INSET * 2.0,
            );
            renderer.draw_photo(image, photo, layout::PHOTO_RADIUS);
        }
        renderer.stroke_card(frame, layout::CARD_RADIUS, 0.75);
        renderer.draw_pin(layout::CARD_WIDTH * 0.5 - layout::PIN_WIDTH * 0.5, 0.0);
        renderer.draw_cross(
            frame.left + layout::CROSS_MARGIN,
            frame.top + layout::CROSS_MARGIN,
            cross_alpha,
        );

        // "Copied" takes the button row's place while it is visible.
        let copied_alpha = self.cards[index].copied_alpha;
        if copied_alpha > 0.01 {
            let (text_w, _) = renderer.text_size(TextId::Copied);
            let pill = RectF::from_xywh(
                (layout::CARD_WIDTH - (text_w + 20.0)) * 0.5,
                layout::copied_top(card_h, self.copied_height),
                text_w + 20.0,
                self.copied_height,
            );
            let pushed = renderer.push_opacity(pill.inflated(8.0), copied_alpha);
            renderer.draw_pill(pill, TextId::Copied, false);
            if pushed {
                renderer.pop_opacity();
            }
        }

        if actions_alpha > 0.01 {
            let a = self.actions;
            let left = (layout::CARD_WIDTH - a.total_width) * 0.5;
            // The row rises into place as it fades in, like the original's
            // -4 DIP translate.
            let lift = -4.0 * (1.0 - actions_alpha);
            let top = layout::actions_top_for(card_h, note) + lift;
            let copy = RectF::from_xywh(left, top, a.copy_width, a.height);
            let open = RectF::from_xywh(left + a.copy_width + a.gap, top, a.open_width, a.height);
            if note {
                // The colour dots sit above the buttons in their own glass pill.
                let pill = layout::swatch_pill(card_h).offset(0.0, lift);
                let pushed = renderer.push_opacity(pill.inflated(8.0), actions_alpha);
                renderer.fill_round(pill, pill.height() * 0.5, crate::render::Brush::Glass);
                renderer.stroke_card(pill, pill.height() * 0.5, 0.75);
                for dot in 0..4 {
                    renderer.draw_swatch(
                        layout::swatch_dot(dot, card_h).offset(0.0, lift),
                        dot,
                        dot == note_color,
                    );
                }
                if pushed {
                    renderer.pop_opacity();
                }
            }
            let bounds = RectF::from_xywh(left, top - 8.0, a.total_width, a.height + 16.0);
            let pushed = renderer.push_opacity(bounds, actions_alpha);
            renderer.draw_pill(
                copy,
                TextId::Copy,
                self.hover_action == Some((id, ActionKind::Copy)),
            );
            renderer.draw_pill(
                open,
                TextId::Open,
                self.hover_action == Some((id, ActionKind::Open)),
            );
            if pushed {
                renderer.pop_opacity();
            }
        }

        if pushed {
            renderer.pop_opacity();
        }
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
}
