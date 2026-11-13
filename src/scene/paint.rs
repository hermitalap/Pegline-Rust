//! Painting for the line scene: the rope, the hint, the peeking entry note
//! and every hanging card.
//!
//! This is a child module of `scene`, so it reads `Scene`'s private fields and
//! hangs its methods off it from here; drawing never writes anything back.

use super::Card;
use crate::layout;
use crate::render::{Renderer, TextId};
use crate::types::{ActionKind, RectF};

impl super::Scene {
    /// Draws the whole scene.
    pub fn draw(&self, renderer: &mut Renderer) {
        renderer.reset_transform();
        renderer.set_transform(windows_numerics::Matrix3x2::translation(
            0.0,
            self.slide.value,
        ));
        renderer.draw_rope(self.width_dip);

        if self.note_peek.value > 0.001 && self.note_entry_center().is_some() {
            self.draw_note_peek(renderer, self.note_peek_x.value);
        }

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

        let grab_id = self.grab.as_ref().map(|grab| grab.id);
        // The row first, then the card in hand on top of it: the card being
        // reordered must never be painted under a neighbour it passes.
        for card in &self.cards {
            if grab_id == Some(card.id) {
                continue;
            }
            let slot_left = card.x.value - layout::CARD_WIDTH * 0.5;
            let hovered =
                self.hover == Some(card.id) && self.dragging.is_none() && grab_id.is_none();
            self.draw_card(card, renderer, slot_left, hovered, false);
        }
        if let Some(card) = grab_id.and_then(|id| self.cards.iter().find(|card| card.id == id)) {
            let slot_left = card.x.value - layout::CARD_WIDTH * 0.5;
            self.draw_card(card, renderer, slot_left, false, true);
        }
        renderer.reset_transform();
    }

    /// The entry note's peeking corner: a tilted card almost entirely above the
    /// top edge, with only its bottom-right corner showing.
    ///
    /// The line strip's window starts at the top edge, so the target clips the
    /// rest of the card without a bitmap mask. The spring slides the card down
    /// from fully hidden to [`layout::NOTE_PEEK_VISIBLE_DIP`], and `entry_x`
    /// is the animated slot centre that lets it reflow with the row.
    fn draw_note_peek(&self, renderer: &mut Renderer, entry_x: f32) {
        let reveal = self.note_peek.value.clamp(0.0, 1.0);
        if reveal <= 0.001 {
            return;
        }
        let (card_w, card_h) = layout::note_card_size();
        let (cx, base_cy) = layout::note_peek_center(entry_x);
        let cy = base_cy - (1.0 - reveal) * layout::NOTE_PEEK_VISIBLE_DIP;
        // `Matrix3x2::rotation` takes degrees, like the rest of the scene.
        let angle = layout::NOTE_PEEK_ANGLE_DEG;
        renderer.set_transform(
            windows_numerics::Matrix3x2::rotation(angle)
                * windows_numerics::Matrix3x2::translation(cx, cy),
        );
        let frame = RectF::from_xywh(-card_w * 0.5, -card_h * 0.5, card_w, card_h);
        renderer.draw_glass(frame, layout::CARD_RADIUS, false);
        let paper = RectF::new(
            frame.left + layout::PHOTO_INSET,
            frame.top + layout::PHOTO_INSET,
            frame.right - layout::PHOTO_INSET,
            frame.bottom - layout::PHOTO_INSET,
        );
        renderer.fill_round(paper, layout::PHOTO_RADIUS, crate::render::Brush::NotePaper);
        renderer.stroke_card(frame, layout::CARD_RADIUS, 0.75);
        renderer.set_transform(windows_numerics::Matrix3x2::translation(
            0.0,
            self.slide.value,
        ));
    }

    /// Draws one card in the slot starting at `slot_left`. The card is scaled
    /// by its press spring and swung about its pin by `swing + tilt`; nothing
    /// on it is written back.
    fn draw_card(
        &self,
        card: &Card,
        renderer: &mut Renderer,
        slot_left: f32,
        hovered: bool,
        lifted: bool,
    ) {
        if card.opacity <= 0.01 {
            return;
        }
        let (card_w, card_h) = card.card_size;
        let slot_top =
            layout::rope_y(card.x.value, self.width_dip) - layout::PIN_ABOVE + card.drop.value;
        let angle = card.swing.value + card.tilt + card.lean.value;
        // Row-vector matrices compose in application order: lift the frame,
        // swing it about the pin, then move it to the slot (the slide is part
        // of the placement because this transform replaces the base one).
        let lift = if lifted { super::DRAG_LIFT_SCALE } else { 1.0 };
        let scale = card.press.value * lift;
        let transform =
            windows_numerics::Matrix3x2::scale_around(
                scale,
                scale,
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
        let pushed = renderer.push_opacity(bounds, card.opacity);

        // The lifted card gets the deepest shadow; it reads as being held
        // above its neighbours even before the pointer moves.
        renderer.draw_shadow(
            frame,
            if lifted {
                0.30
            } else if hovered {
                0.26
            } else {
                0.18
            },
            hovered,
        );
        renderer.draw_glass(frame, layout::CARD_RADIUS, false);
        if let Some(image) = card.image.as_ref() {
            let photo = RectF::from_xywh(
                frame.left + layout::PHOTO_INSET,
                frame.top + layout::PHOTO_INSET,
                card_w - layout::PHOTO_INSET * 2.0,
                card_h - layout::PHOTO_INSET * 2.0,
            );
            renderer.draw_photo(image, photo, layout::PHOTO_RADIUS);
        }
        renderer.stroke_card(frame, layout::CARD_RADIUS, 0.75);
        renderer.draw_pin(
            layout::CARD_WIDTH * 0.5 - layout::PIN_WIDTH * 0.5,
            0.0,
            card.pin_scale.value,
            card.pin_glow.value,
            card.pin_hot.value,
        );
        renderer.draw_cross(
            frame.left + layout::CROSS_MARGIN,
            frame.top + layout::CROSS_MARGIN,
            card.cross_alpha,
        );

        // "Copied" takes the button row's place while it is visible.
        if card.copied_alpha > 0.01 {
            let (text_w, _) = renderer.text_size(TextId::Copied);
            let pill = RectF::from_xywh(
                (layout::CARD_WIDTH - (text_w + 20.0)) * 0.5,
                layout::copied_top(card_h, self.copied_height),
                text_w + 20.0,
                self.copied_height,
            );
            let pushed = renderer.push_opacity(pill.inflated(8.0), card.copied_alpha);
            renderer.draw_pill(pill, TextId::Copied, false);
            if pushed {
                renderer.pop_opacity();
            }
        }

        if card.actions_alpha > 0.01 {
            let a = if card.note {
                self.note_actions
            } else {
                self.actions
            };
            let left = (layout::CARD_WIDTH - a.total_width) * 0.5;
            // The row rises into place as it fades in, like the original's
            // -4 DIP translate.
            let lift = -4.0 * (1.0 - card.actions_alpha);
            let top = layout::actions_top_for(card_h, card.note) + lift;
            let primary = RectF::from_xywh(left, top, a.left_width, a.height);
            let open = RectF::from_xywh(left + a.left_width + a.gap, top, a.right_width, a.height);
            if card.note {
                // The colour dots sit above the buttons in their own glass pill.
                let pill = layout::swatch_pill(card_h).offset(0.0, lift);
                let pushed = renderer.push_opacity(pill.inflated(8.0), card.actions_alpha);
                renderer.draw_glass(pill, pill.height() * 0.5, false);
                renderer.stroke_card(pill, pill.height() * 0.5, 0.75);
                for dot in 0..4 {
                    renderer.draw_swatch(
                        layout::swatch_dot(dot, card_h).offset(0.0, lift),
                        dot,
                        dot == card.note_color,
                    );
                }
                if pushed {
                    renderer.pop_opacity();
                }
            }
            let bounds = RectF::from_xywh(left, top - 8.0, a.total_width, a.height + 16.0);
            let pushed = renderer.push_opacity(bounds, card.actions_alpha);
            let primary_text = if card.note {
                TextId::Copy
            } else {
                TextId::Annotate
            };
            let primary_action = if card.note {
                ActionKind::Copy
            } else {
                ActionKind::Annotate
            };
            renderer.draw_pill(
                primary,
                primary_text,
                self.hover_action == Some((card.id, primary_action)),
            );
            renderer.draw_pill(
                open,
                TextId::Open,
                self.hover_action == Some((card.id, ActionKind::Open)),
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
