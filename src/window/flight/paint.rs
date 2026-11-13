//! The painting half of the flight window: every draw call that writes the
//! flight swap chain.
//!
//! A child module of `flight`, so it reads the textures and the poses the frame
//! loop computes without widening a single field; only the entry points
//! `mod.rs` calls are `pub(super)`. The code is the parent's painting slice
//! moved verbatim: same draw calls, same order, same layer push/pop, same
//! transforms and the same opacity ramps.

use windows_numerics::{Matrix3x2, Vector2};

use crate::layout;
use crate::render::Renderer;
use crate::spring::ease;
use crate::types::RectF;

use super::{DROP_DISTANCE, FlightPose, FlightTextures, PreRollPose};

/// Smallest card that still gets a shadow, DIP.
const SHADOW_MAX_CARD: f32 = 300.0;

/// Draws one travel frame.
pub(super) fn paint(
    renderer: &mut Renderer,
    textures: &FlightTextures,
    pose: &FlightPose,
) -> Result<(), String> {
    renderer.begin()?;
    renderer.set_transform(pose.transform);
    // Once landed, the real card underneath owns the shadow; drawing the
    // flight shadow too would double its alpha and make it pulse as the
    // flight fades.
    draw_flight_card(
        renderer,
        &textures.card,
        pose.card,
        pose.chrome,
        pose.alpha,
        pose.shadow,
        pose.pin,
    );
    paint_start_image(renderer, textures, pose);
    renderer.reset_transform();
    renderer.end()
}

/// Draws the card's own paper still lying under the pointer for the first
/// fraction of the travel, dissolving into the editor view while the card is
/// already moving.
fn paint_start_image(renderer: &mut Renderer, textures: &FlightTextures, pose: &FlightPose) {
    // Only a flight that carries a card bitmap has one, and the photo is gone
    // once the dissolve has finished.
    let Some(start_texture) = textures.start.as_ref() else {
        return;
    };
    if pose.crossfade >= 1.0 {
        return;
    }
    let inset = layout::PHOTO_INSET * pose.chrome;
    let photo = RectF::new(
        pose.card.left + inset,
        pose.card.top + inset,
        pose.card.right - inset,
        pose.card.bottom - inset,
    );
    let start_alpha = (1.0 - ease::smooth(pose.crossfade, 0.0, 1.0)).max(0.01);
    let pushed = renderer.push_opacity(photo, start_alpha);
    renderer.draw_photo(start_texture, photo, layout::PHOTO_RADIUS);
    if pushed {
        renderer.pop_opacity();
    }
}

/// Draws one pre-roll frame: the card still at its slot, turning upright, with
/// the card's own bitmap showing under the editor image.
pub(super) fn paint_pre_roll(
    renderer: &mut Renderer,
    textures: &FlightTextures,
    pose: &PreRollPose,
) -> Result<(), String> {
    renderer.begin()?;
    renderer.set_transform(pose.transform);
    draw_flight_card(
        renderer,
        &textures.card,
        pose.card,
        1.0,
        1.0,
        true,
        pose.pin,
    );
    if let Some(start_texture) = textures.start.as_ref() {
        let inset = layout::PHOTO_INSET;
        let photo = RectF::new(
            pose.card.left + inset,
            pose.card.top + inset,
            pose.card.right - inset,
            pose.card.bottom - inset,
        );
        // Unlike the travel copy this keeps drawing at the 1 % floor, which is
        // what the frame that ends the pre-roll draws.
        let alpha = 1.0 - ease::smooth(pose.crossfade, 0.0, 1.0);
        let pushed = renderer.push_opacity(photo, alpha.max(0.01));
        renderer.draw_photo(start_texture, photo, layout::PHOTO_RADIUS);
        if pushed {
            renderer.pop_opacity();
        }
    }
    renderer.reset_transform();
    renderer.end()
}

/// The transform and group opacity of a falling card at progress `t` (0..1).
///
/// The card only travels down and fades. Its fixed tilt is applied about its
/// own top centre (the pin) *before* the placement: rotating after the
/// placement, around a point in window space, is what used to swing the card
/// out to the left while it fell (`BEHAVIOR.md` §6.4).
pub(super) fn drop_pose(t: f32, left: f32, top: f32, width: f32, tilt: f32) -> (Matrix3x2, f32) {
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

/// Draws the card: photo, then the glass frame and (when `pin` is set) the
/// clothespin at `chrome` opacity.
pub(super) fn draw_flight_card(
    renderer: &mut Renderer,
    texture: &windows::Win32::Graphics::Direct2D::ID2D1Bitmap1,
    card: RectF,
    chrome: f32,
    alpha: f32,
    shadow: bool,
    pin: f32,
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
        renderer.draw_glass(frame, layout::CARD_RADIUS, false);
        renderer.stroke_card(frame, layout::CARD_RADIUS, 0.75);
        if pushed {
            renderer.pop_opacity();
        }
    }
    renderer.draw_photo(texture, photo, layout::PHOTO_RADIUS);
    if pin > 0.01 && chrome > 0.01 {
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
            pin * chrome,
        );
        renderer.draw_pin(pin_left, pin_top, 1.0, 0.0, 0.0);
        if pushed {
            renderer.pop_opacity();
        }
    }
    if opacity_ok {
        renderer.pop_opacity();
    }
}
