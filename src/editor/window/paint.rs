//! Direct2D drawing for the annotation editor window (BEHAVIOR §11).
//!
//! The drawing half of the editor: the opaque card bitmap (photo, marks and the
//! text box), the glass chrome and pills around it, and the off-screen renders
//! that feed the save and the return flights. This is a child of the window
//! module, so it reads the editor's private state and adds its methods in its
//! own `impl` block; nothing here becomes part of the window's public API.
//!
//! The draw calls are the parent's painting slice moved verbatim: same calls,
//! same order, same transforms, same layer push/pop, same antialias modes.

use std::mem::ManuallyDrop;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LAYER_OPTIONS_NONE, D2D1_LAYER_PARAMETERS,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, ID2D1Bitmap1, ID2D1Geometry, ID2D1RenderTarget,
    ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::IDWriteTextLayout;
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppPBGRA, IWICPalette, WICBitmapCacheOnLoad,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICRect,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows_numerics::{Matrix3x2, Vector2};

use super::{
    Assets, CHROME_ACCENT, CHROME_ACCENT_BORDER, CHROME_ACCENT_HOVER, CHROME_BORDER_ALPHA,
    CHROME_GLASS, CHROME_GLASS_ALPHA, CHROME_GLASS_LIT, CHROME_GLASS_LIT_ALPHA, CHROME_INK,
    CHROME_INK_DIM, Drag, ERASER_RADIUS_DIP, ICON_BOX, ICON_DESIGN, ICON_STROKE, Label, TOOLS,
    TextEntry, WM_APP_IME_CARET, arrow_head, build_mosaic, composition_caret_x, d2d_rect,
    drag_rect, ime_client_px, mosaic_key, pixel_format, rect_of, rounded, solid, text_metrics,
    wic_factory,
};
use crate::editor::layout::{self, Hit};
use crate::editor::model::{COLORS, Mark, Tool, WIDTHS};
use crate::types::{ImageData, RectF};

impl super::EditorState {
    /// Paints the floating window: the card's baked shadow, the glass frame and
    /// its picture, the clothespin and the five toolbar pills.
    pub(super) fn paint_chrome(&self) {
        let rt = &self.assets.rt;
        // SAFETY: the window target is in a draw and every brush is ours.
        unsafe {
            rt.SetTransform(&Matrix3x2::identity());
        }
        if let Some(shadow) = self.surface.shadow.as_ref() {
            crate::render::draw_shadow(&self.surface.context, shadow, self.layout.card, 0.18);
        }
        let card = self.layout.card;
        let frame = rounded(card, layout::CARD_RADIUS);
        if !self.fill_glass(card, layout::CARD_RADIUS, false) {
            // SAFETY: the brushes and the render target are alive.
            unsafe {
                rt.FillRoundedRectangle(&frame, &self.chrome.glass);
            }
        }
        if let Some(bitmap) = self.surface.card.as_ref() {
            self.paint_card_bitmap(bitmap);
        }
        // SAFETY: as above.
        unsafe {
            rt.DrawRoundedRectangle(&frame, &self.chrome.border, 0.75, None);
        }
        self.paint_pin();
        for pill in self.layout.top_pills {
            self.paint_pill(pill);
        }
        for pill in self.layout.bottom_pills {
            self.paint_pill(pill);
        }
        self.paint_tools();
        self.paint_colours();
        self.paint_widths();
        self.paint_actions();
    }

    /// Draws the opaque photo bitmap inside the rounded photo corners.
    fn paint_card_bitmap(&self, bitmap: &ID2D1Bitmap1) {
        let Some(mask) = self.photo_mask.as_ref() else {
            return;
        };
        let photo = self.layout.photo;
        let parameters = D2D1_LAYER_PARAMETERS {
            contentBounds: d2d_rect(photo),
            geometricMask: ManuallyDrop::new(Some(mask.clone())),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS_NONE,
        };
        let destination = d2d_rect(photo);
        let source = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: self.layout.photo.width(),
            bottom: self.layout.photo.height(),
        };
        // SAFETY: the layer parameters and the rounded mask are alive for the
        // calls; an internal layer is used because none is passed.
        unsafe {
            self.assets.rt.PushLayer(&parameters, None);
            self.assets.rt.DrawBitmap(
                bitmap,
                Some(&destination),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                Some(&source),
            );
            self.assets.rt.PopLayer();
        }
    }

    fn paint_pill(&self, pill: RectF) {
        let rounded = rounded(pill, layout::PILL_RADIUS);
        if !self.fill_glass(pill, layout::PILL_RADIUS, false) {
            // SAFETY: the brushes and the render target are alive.
            unsafe {
                self.assets
                    .rt
                    .FillRoundedRectangle(&rounded, &self.chrome.glass);
            }
        }
        // SAFETY: the border brush and the render target are alive.
        unsafe {
            self.assets
                .rt
                .DrawRoundedRectangle(&rounded, &self.chrome.border, 0.75, None);
        }
    }

    /// Tries the wallpaper Mica; false means the caller keeps its solid glass.
    fn fill_glass(&self, rect: RectF, radius: f32, lit: bool) -> bool {
        self.surface.mica.borrow().fill(
            &self.surface.context,
            rect,
            radius,
            Matrix3x2::identity(),
            lit,
        )
    }

    fn paint_pin(&self) {
        let pin = self.layout.pin;
        let body = rounded(pin, 3.5);
        // SAFETY: the brushes and the render target are alive.
        unsafe {
            self.assets
                .rt
                .FillRoundedRectangle(&body, &self.chrome.metal);
            self.assets
                .rt
                .DrawRoundedRectangle(&body, &self.chrome.metal_edge, 0.6, None);
        }
        let slot = RectF::from_xywh(pin.left + 2.0, pin.top + 8.0, pin.width() - 4.0, 1.4);
        self.fill_round(slot, 0.7, &self.chrome.pin_slot);
    }

    fn fill_round(&self, rect: RectF, radius: f32, brush: &ID2D1SolidColorBrush) {
        // SAFETY: the brush and the render target are alive.
        unsafe {
            self.assets
                .rt
                .FillRoundedRectangle(&rounded(rect, radius), brush);
        }
    }

    fn paint_tools(&self) {
        for (index, rect) in self.layout.tools.iter().enumerate() {
            let selected = TOOLS[index] == self.tool;
            let hovered = self.hover == Some(Hit::Tool(index));
            self.paint_button(*rect, selected, hovered);
            let brush = if selected {
                &self.chrome.white
            } else {
                &self.chrome.ink
            };
            self.paint_tool_icon(index, *rect, brush);
        }
    }

    fn paint_colours(&self) {
        for (index, rect) in self.layout.colours.iter().enumerate() {
            let rgb = COLORS[index].0;
            let center = rect.center();
            // The dots are round, concentric with the pill's end radius.
            let disc = D2D1_ELLIPSE {
                point: Vector2 {
                    X: center.0,
                    Y: center.1,
                },
                radiusX: rect.width() * 0.5,
                radiusY: rect.height() * 0.5,
            };
            if let Some(brush) = self.assets.brush(rgb) {
                // SAFETY: the brush and the render target are alive.
                unsafe {
                    self.assets.rt.FillEllipse(&disc, brush);
                }
            }
            let selected = rgb == self.color;
            let hovered = self.hover == Some(Hit::Colour(index));
            // The hairline keeps a white dot readable on the glass; the accent
            // ring sits just outside the dot so it never eats into the colour.
            // SAFETY: the brush and the render target are alive.
            unsafe {
                self.assets
                    .rt
                    .DrawEllipse(&disc, &self.chrome.ink_dim, 1.0, None);
            }
            if selected || hovered {
                let ring = D2D1_ELLIPSE {
                    radiusX: disc.radiusX + 1.6,
                    radiusY: disc.radiusY + 1.6,
                    ..disc
                };
                // SAFETY: the brush and the render target are alive.
                unsafe {
                    self.assets.rt.DrawEllipse(
                        &ring,
                        &self.chrome.accent,
                        if selected { 2.0 } else { 1.5 },
                        None,
                    );
                }
            }
        }
    }

    fn paint_widths(&self) {
        for (index, rect) in self.layout.widths.iter().enumerate() {
            let selected = (WIDTHS[index] - self.width).abs() < 0.01;
            let hovered = self.hover == Some(Hit::Width(index));
            // Keep the button unfilled when selected so the dot's real size
            // stays readable; the accent ring marks the choice instead.
            self.paint_button(*rect, false, hovered);
            if selected {
                let ring = D2D1_ELLIPSE {
                    point: Vector2 {
                        X: rect.center().0,
                        Y: rect.center().1,
                    },
                    radiusX: rect.width() * 0.5 - 1.2,
                    radiusY: rect.height() * 0.5 - 1.2,
                };
                // SAFETY: the brush and the render target are alive.
                unsafe {
                    self.assets
                        .rt
                        .DrawEllipse(&ring, &self.chrome.accent, 2.0, None);
                }
            }
            // A dot whose diameter follows the stroke weight reads at a glance:
            // 4/8/12 source pixels, scaled 1.25x so the 4 px dot stays visible.
            let diameter = (WIDTHS[index] * 1.25).min(rect.height() - 10.0);
            let disc = D2D1_ELLIPSE {
                point: Vector2 {
                    X: rect.center().0,
                    Y: rect.center().1,
                },
                radiusX: diameter * 0.5,
                radiusY: diameter * 0.5,
            };
            let brush = self.assets.brush(self.color).unwrap_or(&self.chrome.ink);
            let outline = if selected {
                &self.chrome.accent
            } else {
                &self.chrome.ink_dim
            };
            // SAFETY: the render target is in a draw and the brushes are alive.
            unsafe {
                self.assets.rt.FillEllipse(&disc, brush);
                self.assets.rt.DrawEllipse(&disc, outline, 1.0, None);
            }
        }
    }

    fn paint_actions(&self) {
        let undo_enabled = self.doc.as_ref().map(|doc| doc.can_undo()) == Some(true);
        let redo_enabled = self.doc.as_ref().map(|doc| doc.can_redo()) == Some(true);
        self.paint_action(
            self.layout.undo,
            &self.labels.undo,
            undo_enabled,
            false,
            self.hover == Some(Hit::Undo),
        );
        self.paint_action(
            self.layout.redo,
            &self.labels.redo,
            redo_enabled,
            false,
            self.hover == Some(Hit::Redo),
        );
        self.paint_action(
            self.layout.cancel,
            &self.labels.cancel,
            true,
            false,
            self.hover == Some(Hit::Cancel),
        );
        self.paint_action(
            self.layout.save,
            &self.labels.save,
            true,
            true,
            self.hover == Some(Hit::Save),
        );
    }

    /// Paints the picture and its marks into the opaque card bitmap. The
    /// transform maps source pixels to the bitmap's DIP space, whose origin is
    /// the photo's top-left corner.
    pub(super) fn paint_photo(&self) {
        let rt = &self.assets.rt;
        let base = Matrix3x2::scale(self.canvas.scale, self.canvas.scale);
        // SAFETY: the card target is in a draw and the bitmap is alive.
        unsafe {
            rt.SetTransform(&base);
        }
        if let Some(bitmap) = self.assets.source.as_ref() {
            let full = d2d_rect(RectF::from_xywh(
                0.0,
                0.0,
                self.source_size.0 as f32,
                self.source_size.1 as f32,
            ));
            let source = D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: self.source_size.0 as f32,
                bottom: self.source_size.1 as f32,
            };
            // SAFETY: the bitmap covers the whole destination.
            unsafe {
                rt.DrawBitmap(
                    bitmap,
                    Some(&full),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    Some(&source),
                );
            }
        }
        let painter = Painter {
            rt,
            assets: &self.assets,
            stroke: &self.surface.stroke_round,
            dash: &self.surface.stroke_dash,
            triangle: &self.surface.triangle,
            base,
        };
        // WPF's text tool showed a white TextBox with a caret while typing;
        // without it the glyphs float on a busy picture with no insertion
        // point. It sits above older marks and below the text being typed, and
        // is on-screen only: `save()` renders the marks alone.
        if let Some(doc) = self.doc.as_ref() {
            match (doc.typing_index(), doc.typing_mark()) {
                (
                    Some(index),
                    Some(Mark::Text {
                        origin,
                        text,
                        color,
                        ..
                    }),
                ) => {
                    painter.marks_except(doc.marks(), &self.texts, index);
                    // While the IME is composing, the box, the text and the
                    // caret all follow `committed + composition`: the pinyin is
                    // drawn inside the white box, underlined, instead of by the
                    // input method in a transparent floating window.
                    let composed = self
                        .composing
                        .as_ref()
                        .filter(|composed| composed.committed == *text);
                    let (text_w, text_h) = match composed {
                        Some(composed) => (composed.width, composed.height),
                        None => self
                            .texts
                            .iter()
                            .find(|entry| entry.text == *text)
                            .map(|entry| {
                                let metrics = text_metrics(&entry.layout);
                                (metrics.width, metrics.height)
                            })
                            .unwrap_or((8.0, 38.0)),
                    };
                    // The caret sits after the composition cursor, not after
                    // the committed text, so an IME that edits inside its
                    // composition keeps the caret where the pinyin is.
                    let caret_x = composed
                        .and_then(|composed| {
                            let composition = doc.composition()?;
                            composition_caret_x(
                                &composed.layout,
                                text,
                                composition.text(),
                                composition.cursor(),
                            )
                        })
                        .unwrap_or(text_w);
                    let pad = 4.0;
                    let box_rect = RectF::from_xywh(
                        origin.0 - pad,
                        origin.1 - pad,
                        text_w.max(caret_x).max(8.0) + pad * 2.0,
                        text_h.max(24.0) + pad * 2.0,
                    );
                    let rounded_box = rounded(box_rect, 3.0);
                    let pen = self.assets.brush(*color);
                    // SAFETY: the render target is in a draw; brushes are live.
                    unsafe {
                        rt.FillRoundedRectangle(&rounded_box, &self.chrome.text_box);
                        if let Some(pen) = pen {
                            rt.DrawRoundedRectangle(&rounded_box, pen, 1.0, None);
                            let caret = d2d_rect(RectF::from_xywh(
                                origin.0 + caret_x.max(0.0) + 1.0,
                                origin.1,
                                2.0,
                                text_h.max(18.0),
                            ));
                            rt.FillRectangle(&caret, pen);
                        }
                    }
                    // Publish the insertion point and the box bottom in client
                    // pixels: the composition window of input methods that draw
                    // their own sits at the caret, and the candidate list opens
                    // under the box instead of on top of the glyphs.
                    let (caret_px, candidate_px) =
                        ime_client_px(self.canvas, *origin, caret_x + 1.0, text_h, self.dpi);
                    let previous = (
                        self.shared.caret_x.load(Ordering::Relaxed),
                        self.shared.caret_y.load(Ordering::Relaxed),
                        self.shared.candidate_x.load(Ordering::Relaxed),
                        self.shared.candidate_y.load(Ordering::Relaxed),
                    );
                    let next = (caret_px.0, caret_px.1, candidate_px.0, candidate_px.1);
                    if previous != next {
                        self.shared.caret_x.store(next.0, Ordering::Relaxed);
                        self.shared.caret_y.store(next.1, Ordering::Relaxed);
                        self.shared.candidate_x.store(next.2, Ordering::Relaxed);
                        self.shared.candidate_y.store(next.3, Ordering::Relaxed);
                        // The composition string changes faster than a frame:
                        // each pinyin key widens the caret before the IME asks
                        // for its position. Re-anchor it once the layout has
                        // been measured, while the composition is still open.
                        if self.shared.composing.load(Ordering::Acquire) {
                            // SAFETY: posting to the window this editor owns;
                            // a failed post only leaves the list a frame late.
                            unsafe {
                                let _ = PostMessageW(
                                    Some(self.surface.hwnd),
                                    WM_APP_IME_CARET,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                            }
                        }
                    }
                    match composed {
                        Some(composed) => {
                            painter.text_layout(*origin, &composed.layout, *color);
                        }
                        None => {
                            if let Some(mark) = doc.marks().get(index) {
                                painter.mark(mark, &self.texts);
                            }
                        }
                    }
                }
                _ => painter.marks(doc.marks(), &self.texts),
            }
        }
        if let Some(drag) = self.drag.as_ref() {
            painter.drag(self.tool, self.color, self.width, self.canvas.scale, drag);
        }
        // SAFETY: restores the identity transform for the next target.
        unsafe {
            rt.SetTransform(&Matrix3x2::identity());
        }
    }

    /// Radius that keeps a button concentric with the pill around it: half its
    /// short side, so the tool buttons and the colour dots are circles and the
    /// wide action buttons are stadiums.
    fn button_radius(rect: RectF) -> f32 {
        rect.width().min(rect.height()) * 0.5
    }

    fn paint_button(&self, rect: RectF, selected: bool, hovered: bool) {
        let rounded = rounded(rect, Self::button_radius(rect));
        if selected {
            let fill = if hovered {
                &self.chrome.accent_hover
            } else {
                &self.chrome.accent
            };
            // SAFETY: the brushes and the render target are alive.
            unsafe {
                self.assets.rt.FillRoundedRectangle(&rounded, fill);
                self.assets.rt.DrawRoundedRectangle(
                    &rounded,
                    &self.chrome.accent_border,
                    1.0,
                    None,
                );
            }
        } else if hovered && !self.fill_glass(rect, Self::button_radius(rect), true) {
            // SAFETY: the brush and the render target are alive.
            unsafe {
                self.assets
                    .rt
                    .FillRoundedRectangle(&rounded, &self.chrome.glass_lit);
            }
        }
    }

    /// Draws one of the four Lucide tool icons (ISC/MIT; the SVG sources and
    /// license are in `assets/icons/`). Each icon lives on Lucide's 24x24 grid
    /// with a 2 px stroke; here it is centred in the button at 18 DIP and the
    /// stroke is scaled to match, so all four share one optical weight.
    fn paint_tool_icon(&self, index: usize, rect: RectF, brush: &ID2D1SolidColorBrush) {
        let (cx, cy) = rect.center();
        let rt = &self.assets.rt;
        let stroke = &self.surface.stroke_round;
        // The arrow glyph sits in a 14x14 part of Lucide's 24x24 grid, so it is
        // grown a little to share the optical size of the rectangle and grid;
        // the filled eraser is compact, so it gets a slightly bigger box too.
        let design_scale = match index {
            0 => 1.12,
            3 => 1.2,
            _ => 1.0,
        };
        let scale = ICON_BOX / ICON_DESIGN * design_scale;
        let width = ICON_STROKE * ICON_BOX / ICON_DESIGN;
        let point = |x: f32, y: f32| Vector2 {
            X: cx + (x - 12.0) * scale,
            Y: cy + (y - 12.0) * scale,
        };
        let box_rect = |x: f32, y: f32, w: f32, h: f32| {
            RectF::from_xywh(
                cx + (x - 12.0) * scale,
                cy + (y - 12.0) * scale,
                w * scale,
                h * scale,
            )
        };
        let line = |from: (f32, f32), to: (f32, f32)| {
            // SAFETY: the render target is in a draw, the brush is alive and
            // the stroke style belongs to the surface.
            unsafe {
                rt.DrawLine(
                    point(from.0, from.1),
                    point(to.0, to.1),
                    brush,
                    width,
                    Some(stroke),
                );
            }
        };
        match index {
            // Lucide `move-up-right`: M13 5H19V11 and M19 5L5 19.
            0 => {
                line((13.0, 5.0), (19.0, 5.0));
                line((19.0, 5.0), (19.0, 11.0));
                line((19.0, 5.0), (5.0, 19.0));
            }
            // Lucide `rectangle-horizontal`: rect x=2 y=6 w=20 h=12 rx=2.
            1 => {
                let frame = box_rect(2.0, 6.0, 20.0, 12.0);
                // SAFETY: the render target is in a draw and the brush is alive.
                unsafe {
                    rt.DrawRoundedRectangle(
                        &rounded(frame, 2.0 * scale),
                        brush,
                        width,
                        Some(stroke),
                    );
                }
            }
            // Lucide `type`: M12 4v16, M9 20h6 and the rounded top bar
            // M4 7V5a1 1 0 0 1 1-1h14a1 1 0 0 1 1 1v2.
            2 => {
                line((12.0, 4.0), (12.0, 20.0));
                line((9.0, 20.0), (15.0, 20.0));
                line((4.0, 7.0), (4.0, 5.0));
                line((4.0, 5.0), (5.0, 4.0));
                line((5.0, 4.0), (19.0, 4.0));
                line((19.0, 4.0), (20.0, 5.0));
                line((20.0, 5.0), (20.0, 7.0));
            }
            // Lucide `eye-off` for the mosaic/redaction tool: an eye with a
            // slash, drawn as an oval, a pupil and the slash.
            4 => {
                let eye = D2D1_ELLIPSE {
                    point: point(12.0, 12.0),
                    radiusX: 9.2 * scale,
                    radiusY: 5.8 * scale,
                };
                let pupil = D2D1_ELLIPSE {
                    point: point(12.0, 12.0),
                    radiusX: 2.4 * scale,
                    radiusY: 2.4 * scale,
                };
                // SAFETY: the render target is in a draw and the brush is alive.
                unsafe {
                    rt.DrawEllipse(&eye, brush, width, Some(stroke));
                    rt.DrawEllipse(&pupil, brush, width, Some(stroke));
                }
                line((2.0, 2.0), (22.0, 22.0));
            }
            // Lucide `eraser`, drawn with the same outline and sleeve stroke.
            3 => {
                let transform = Matrix3x2::scale(scale, scale)
                    * Matrix3x2::translation(cx - 12.0 * scale, cy - 12.0 * scale);
                // SAFETY: the render target is in a draw and the brush is alive.
                unsafe {
                    rt.SetTransform(&transform);
                    rt.DrawGeometry(
                        &self.surface.eraser,
                        brush,
                        width / scale.max(0.0001),
                        Some(stroke),
                    );
                    rt.SetTransform(&Matrix3x2::identity());
                }
            }
            _ => {}
        }
    }

    fn paint_action(
        &self,
        rect: RectF,
        label: &Label,
        enabled: bool,
        primary: bool,
        hovered: bool,
    ) {
        self.paint_button(rect, primary, hovered);
        let brush = if primary {
            &self.chrome.white
        } else if enabled {
            &self.chrome.ink
        } else {
            &self.chrome.ink_dim
        };
        self.paint_label(label, rect, brush);
    }

    fn paint_label(&self, label: &Label, rect: RectF, brush: &ID2D1SolidColorBrush) {
        let x = rect.left + (rect.width() - label.width) * 0.5;
        let y = rect.top + (rect.height() - label.height) * 0.5 - 1.0;
        // SAFETY: the layout, the brush and the render target are alive.
        unsafe {
            self.assets.rt.DrawTextLayout(
                Vector2 { X: x, Y: y },
                &label.layout,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    /// Renders the source picture plus marks into an image of `width` x
    /// `height` pixels. `scale` maps source pixels to those pixels: 1.0 for the
    /// saved file, a fraction for the flight preview. Runs on the render
    /// thread, never on the UI thread.
    pub(super) fn render_document(
        &mut self,
        width: u32,
        height: u32,
        scale: f32,
        smooth: bool,
        marks: bool,
    ) -> Result<ImageData, String> {
        let Some(doc) = self.doc.as_ref() else {
            return Err("there is no picture to save".to_string());
        };
        let source = doc
            .source()
            .ok_or_else(|| "there is no picture to save".to_string())?;
        if !source.is_valid() {
            return Err(format!(
                "invalid source image ({}x{}, {} bytes)",
                source.width,
                source.height,
                source.bgra.len()
            ));
        }
        let (source_width, source_height) = (source.width, source.height);
        let factory = wic_factory()?;
        // SAFETY: the factory is live, the size is at least one pixel and the
        // pixel format is a static WIC GUID.
        let target = unsafe {
            factory.CreateBitmap(
                width,
                height,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnLoad,
            )
        }
        .map_err(|error| format!("CreateBitmap({width}x{height}): {error}"))?;
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: pixel_format(D2D1_ALPHA_MODE_PREMULTIPLIED),
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        // SAFETY: the factory and the WIC bitmap are live; the target keeps a
        // reference to the bitmap for as long as it exists.
        let rt = unsafe {
            self.surface
                .factory
                .CreateWicBitmapRenderTarget(&target, &properties)
        }
        .map_err(|error| format!("CreateWicBitmapRenderTarget: {error}"))?;

        let mut assets = Assets::new(rt.clone())?;
        assets.set_source(source)?;
        if marks {
            for mark in doc.marks() {
                let Mark::Mosaic { rect, block } = mark else {
                    continue;
                };
                let Some(key) = mosaic_key(*rect, *block, (source_width, source_height)) else {
                    continue;
                };
                if assets.has_mosaic(key) {
                    continue;
                }
                if let Some(image) = build_mosaic(source, key) {
                    assets.upload_mosaic(key, &image)?;
                }
            }
        }

        let transform = Matrix3x2::scale(scale, scale);
        let full = d2d_rect(RectF::from_xywh(
            0.0,
            0.0,
            source_width as f32,
            source_height as f32,
        ));
        // SAFETY: the target is a fresh WIC render target on this thread.
        unsafe {
            rt.SetDpi(96.0, 96.0);
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            // The file has an alpha channel; subpixel ClearType would both be
            // unsupported and leave coloured fringes in it.
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            rt.SetTransform(&Matrix3x2::identity());
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            rt.SetTransform(&transform);
        }
        if let Some(bitmap) = assets.source.as_ref() {
            // SAFETY: `bitmap` is alive and covers the full destination.
            unsafe {
                rt.DrawBitmap(
                    bitmap,
                    Some(&full),
                    1.0,
                    if smooth {
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR
                    } else {
                        D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
                    },
                    None,
                );
            }
        }
        let painter = Painter {
            rt: &rt,
            assets: &assets,
            stroke: &self.surface.stroke_round,
            dash: &self.surface.stroke_dash,
            triangle: &self.surface.triangle,
            base: transform,
        };
        if marks {
            painter.marks(doc.marks(), &self.texts);
        }
        // SAFETY: `BeginDraw` was called above on this thread.
        unsafe { rt.EndDraw(None, None) }.map_err(|error| format!("EndDraw(save): {error}"))?;

        // The render target holds premultiplied pixels; PNG wants straight
        // alpha, so the bitmap is converted before it is copied out.
        // SAFETY: the factory is live.
        let converter = unsafe { factory.CreateFormatConverter() }
            .map_err(|error| format!("CreateFormatConverter: {error}"))?;
        // SAFETY: the converter is fresh and the source bitmap is alive.
        unsafe {
            converter.Initialize(
                &target,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
        }
        .map_err(|error| format!("IWICFormatConverter::Initialize: {error}"))?;
        let stride = width * 4;
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let rect = WICRect {
            X: 0,
            Y: 0,
            Width: width as i32,
            Height: height as i32,
        };
        // SAFETY: `bgra` is exactly `stride * height` bytes for the size above.
        unsafe { converter.CopyPixels(&rect, stride, &mut bgra) }
            .map_err(|error| format!("CopyPixels({width}x{height}): {error}"))?;
        Ok(ImageData::new(width, height, bgra))
    }
}

/// Draws marks in source coordinates through the current base transform.
struct Painter<'a> {
    rt: &'a ID2D1RenderTarget,
    assets: &'a Assets,
    stroke: &'a ID2D1StrokeStyle,
    dash: &'a ID2D1StrokeStyle,
    triangle: &'a ID2D1Geometry,
    base: Matrix3x2,
}

impl<'a> Painter<'a> {
    fn marks(&self, marks: &[Mark], texts: &[TextEntry]) {
        for mark in marks {
            self.mark(mark, texts);
        }
    }

    /// Draws every mark except `skip` (the text being typed, which goes above
    /// its input box).
    fn marks_except(&self, marks: &[Mark], texts: &[TextEntry], skip: usize) {
        for (index, mark) in marks.iter().enumerate() {
            if index != skip {
                self.mark(mark, texts);
            }
        }
    }

    fn mark(&self, mark: &Mark, texts: &[TextEntry]) {
        match mark {
            Mark::Arrow {
                from,
                to,
                color,
                width,
            } => self.arrow(*from, *to, *color, *width),
            Mark::Rectangle { rect, color, width } => {
                self.rectangle(*rect, *color, *width);
            }
            Mark::Text {
                origin,
                text,
                color,
                ..
            } => self.text(*origin, text, *color, texts),
            Mark::Mosaic { rect, block } => self.mosaic(*rect, *block),
        }
    }

    fn text(&self, origin: (f32, f32), text: &str, color: u32, texts: &[TextEntry]) {
        let Some(entry) = texts.iter().find(|entry| entry.text == text) else {
            return;
        };
        self.text_layout(origin, &entry.layout, color);
    }

    /// Draws an already laid-out run. The composing text uses the same call;
    /// only its layout carries the composition underline.
    fn text_layout(&self, origin: (f32, f32), layout: &IDWriteTextLayout, color: u32) {
        let Some(brush) = self.assets.brush(color) else {
            return;
        };
        // SAFETY: the layout, the brush and the target are alive.
        unsafe {
            self.rt.DrawTextLayout(
                Vector2 {
                    X: origin.0,
                    Y: origin.1,
                },
                layout,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    fn arrow(&self, from: (f32, f32), to: (f32, f32), color: u32, width: f32) {
        let Some(brush) = self.assets.brush(color) else {
            return;
        };
        let Some((shaft, transform)) = arrow_head(from, to, width, self.base) else {
            return;
        };
        // SAFETY: the brush, the triangle and the target are alive; the head is
        // a transformed fill of the cached unit triangle.
        unsafe {
            self.rt.DrawLine(
                Vector2 {
                    X: from.0,
                    Y: from.1,
                },
                Vector2 {
                    X: shaft.0,
                    Y: shaft.1,
                },
                brush,
                width,
                Some(self.stroke),
            );
            self.rt.SetTransform(&transform);
            self.rt.FillGeometry(self.triangle, brush, None);
            self.rt.SetTransform(&self.base);
        }
    }

    fn rectangle(&self, rect: (f32, f32, f32, f32), color: u32, width: f32) {
        if let Some(brush) = self.assets.brush(color) {
            let d = d2d_rect(rect_of(rect));
            // SAFETY: the brush and the target are alive.
            unsafe {
                self.rt.DrawRectangle(&d, brush, width, Some(self.stroke));
            }
        }
    }

    fn mosaic(&self, rect: (f32, f32, f32, f32), block: u32) {
        let Some(key) = mosaic_key(rect, block, self.assets.source_size) else {
            return;
        };
        let Some(bitmap) = self.assets.find_mosaic(key) else {
            return;
        };
        let (blocks_x, blocks_y) = key.blocks();
        let destination = d2d_rect(rect_of((
            key.x as f32,
            key.y as f32,
            key.width as f32,
            key.height as f32,
        )));
        let source = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: blocks_x as f32,
            bottom: blocks_y as f32,
        };
        // SAFETY: the bitmap is alive; nearest-neighbour keeps the 16-pixel
        // blocks square, which is what a mosaic is.
        unsafe {
            self.rt.DrawBitmap(
                bitmap,
                Some(&destination),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                Some(&source),
            );
        }
    }

    /// The in-progress gesture, drawn exactly like the mark it will become.
    fn drag(&self, tool: Tool, color: u32, width: f32, scale: f32, drag: &Drag) {
        match tool {
            Tool::Arrow => self.arrow(drag.start, drag.current, color, width),
            Tool::Rectangle => {
                self.rectangle(drag_rect(drag.start, drag.current), color, width);
            }
            Tool::Mosaic => {
                // Dashed white outline: no pixels are hidden before the drop.
                let Some(brush) = self.assets.brush(0xFF_FF_FF) else {
                    return;
                };
                let outline = (1.5 / scale.max(0.0001)).max(1.0);
                let d = d2d_rect(rect_of(drag_rect(drag.start, drag.current)));
                // SAFETY: the brush and the target are alive.
                unsafe {
                    self.rt.DrawRectangle(&d, brush, outline, Some(self.dash));
                }
            }
            Tool::Eraser => {
                // A dashed eraser disc at the pointer. A dark ring under the
                // white dashes keeps it visible on light and dark pictures.
                let radius = ERASER_RADIUS_DIP / scale.max(0.0001);
                let disc = D2D1_ELLIPSE {
                    point: Vector2 {
                        X: drag.current.0,
                        Y: drag.current.1,
                    },
                    radiusX: radius,
                    radiusY: radius,
                };
                let width = (1.5 / scale.max(0.0001)).max(0.8);
                // SAFETY: the brushes and the target are alive.
                unsafe {
                    if let Some(dark) = self.assets.brush(0x00_00_00) {
                        self.rt.DrawEllipse(&disc, dark, width * 2.0, None);
                    }
                    if let Some(white) = self.assets.brush(0xFF_FF_FF) {
                        self.rt.DrawEllipse(&disc, white, width, Some(self.dash));
                    }
                }
            }
            Tool::Text => {}
        }
    }
}

/// Solid brushes for the toolbar chrome, created once per surface.
pub(super) struct Chrome {
    /// The same white glass a hanging card wears.
    glass: ID2D1SolidColorBrush,
    /// Hover fill inside a pill.
    glass_lit: ID2D1SolidColorBrush,
    /// Hairline around a pill or the card.
    border: ID2D1SolidColorBrush,
    ink: ID2D1SolidColorBrush,
    ink_dim: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    accent_hover: ID2D1SolidColorBrush,
    accent_border: ID2D1SolidColorBrush,
    white: ID2D1SolidColorBrush,
    /// The translucent sheet behind the text being typed (WPF's TextBox).
    text_box: ID2D1SolidColorBrush,
    /// The clothespin.
    metal: ID2D1SolidColorBrush,
    metal_edge: ID2D1SolidColorBrush,
    pin_slot: ID2D1SolidColorBrush,
}

impl Chrome {
    pub(super) fn new(rt: &ID2D1RenderTarget) -> Result<Self, String> {
        Ok(Self {
            glass: solid(rt, CHROME_GLASS, CHROME_GLASS_ALPHA)?,
            glass_lit: solid(rt, CHROME_GLASS_LIT, CHROME_GLASS_LIT_ALPHA)?,
            border: solid(rt, 0xFF_FF_FF, CHROME_BORDER_ALPHA)?,
            ink: solid(rt, CHROME_INK, 1.0)?,
            ink_dim: solid(rt, CHROME_INK_DIM, 1.0)?,
            accent: solid(rt, CHROME_ACCENT, 1.0)?,
            accent_hover: solid(rt, CHROME_ACCENT_HOVER, 1.0)?,
            accent_border: solid(rt, CHROME_ACCENT_BORDER, 1.0)?,
            white: solid(rt, 0xFF_FF_FF, 1.0)?,
            text_box: solid(rt, 0xFF_FF_FF, 0.78)?,
            metal: solid(rt, 0xD8_D8_D8, 1.0)?,
            metal_edge: solid(rt, 0x8A_8A_8A, 0.9)?,
            pin_slot: solid(rt, 0x00_00_00, 0.32)?,
        })
    }
}
