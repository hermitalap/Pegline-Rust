//! The painting half of the note editor window: every draw call that writes the
//! paper bitmap and the glass swap-chain target.
//!
//! A child module of its parent, so it reads `EditorState`'s private fields and
//! adds its own `impl super::EditorState` block instead of widening any field;
//! only the two entry points the parent calls are `pub(super)`. The code is the
//! parent's painting slice moved verbatim: same calls, same order, same
//! transforms, same antialias modes and the same layer push/pop.

use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_INTERPOLATION_MODE_LINEAR,
    D2D1_LAYER_OPTIONS1_NONE, D2D1_LAYER_PARAMETERS1, D2D1_ROUNDED_RECT, ID2D1RenderTarget,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_HIT_TEST_METRICS, DWRITE_MEASURING_MODE_NATURAL,
};
use windows_numerics::{Matrix3x2, Vector2};

use super::{
    BORDER_DIP, LaidLine, RADIUS_DIP, color_of, d2d_rect, paper_rect, shadow_slices, utf16_offset,
};
use crate::i18n;
use crate::note_editor::display::{self, Block, PAD_BOTTOM, PAD_LEFT, PAD_TOP, ResizeZone, Style};

impl super::EditorState {
    /// Paints the paper (gradient, dog-ear, text) into `context`. The same code
    /// renders the on-screen paper bitmap and the cached WIC bitmap a flight
    /// carries, so the editor can look identical in both.
    pub(super) fn paint_paper_on(&mut self, context: &ID2D1RenderTarget) {
        // The paper is opaque, so the whole bitmap can simply be filled.
        let palette = crate::notes::color(self.color);
        let content_width = self.paper.content_width();
        // SAFETY: the context is drawing to the paper target.
        unsafe {
            context.Clear(Some(&color_of(palette.paper, 1.0)));
            if let Some(brush) = self.paper_brush.as_ref() {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: self.paper.width,
                        bottom: self.paper.height,
                    },
                    radiusX: RADIUS_DIP,
                    radiusY: RADIUS_DIP,
                };
                context.FillRoundedRectangle(&rounded, brush);
            }
        }
        // Dog-ear.
        // SAFETY: the brushes and geometries are alive.
        unsafe {
            if let Some(geometry) = self.fold_edge.as_ref() {
                context.FillGeometry(geometry, &self.assets.fold_edge, None);
            }
            if let Some(geometry) = self.fold_shade.as_ref() {
                context.FillGeometry(geometry, &self.assets.fold_shade, None);
            }
        }
        // Block decoration behind the text.
        let origin_x = PAD_LEFT;
        for line in &self.lines {
            // A 200-line note paints the ~13 lines that can land on the paper.
            if !in_view(line, self.scroll, self.view_height) {
                continue;
            }
            let top = PAD_TOP + line.text_top - self.scroll;
            let height = line.text_height;
            if !line.active {
                match line.block {
                    Block::Code { fence: false } => {
                        // SAFETY: drawing on the paper target.
                        unsafe {
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x + 2.0,
                                    top,
                                    right: origin_x + content_width,
                                    bottom: top + height,
                                },
                                &self.assets.code_background,
                            );
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x,
                                    top,
                                    right: origin_x + 2.0,
                                    bottom: top + height,
                                },
                                &self.assets.quote_bar,
                            );
                        }
                    }
                    Block::Quote => {
                        // SAFETY: drawing on the paper target.
                        unsafe {
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x,
                                    top,
                                    right: origin_x + 2.0,
                                    bottom: top + height,
                                },
                                &self.assets.quote_bar,
                            );
                        }
                    }
                    Block::Rule => {
                        // SAFETY: drawing on the paper target.
                        unsafe {
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x,
                                    top: top + height * 0.5,
                                    right: origin_x + content_width,
                                    bottom: top + height * 0.5 + 1.0,
                                },
                                &self.assets.rule,
                            );
                        }
                    }
                    Block::List {
                        ordered,
                        number,
                        task,
                    } => self.paint_list_marker(
                        context,
                        ordered,
                        number,
                        task,
                        origin_x,
                        top,
                        height,
                        line.font_size,
                        line.indent,
                    ),
                    _ => {}
                }
            }
            // Inline code background.
            if !line.active {
                self.paint_inline_code(context, line, origin_x, top);
            }
        }
        // Selection goes behind the glyphs.
        self.paint_selection(context, origin_x);
        // Text.
        for line in &self.lines {
            if !in_view(line, self.scroll, self.view_height) {
                continue;
            }
            let Some(layout) = line.layout.as_ref() else {
                continue;
            };
            let brush = if matches!(line.block, Block::Quote) {
                &self.assets.quote
            } else {
                &self.assets.ink
            };
            let origin = Vector2 {
                X: origin_x + line.indent,
                Y: PAD_TOP + line.text_top - self.scroll,
            };
            // SAFETY: the layout, brush and context are alive.
            unsafe {
                context.DrawTextLayout(origin, layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }
        self.paint_caret(context, origin_x);
        self.paint_placeholder(context, origin_x);
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_list_marker(
        &self,
        context: &ID2D1RenderTarget,
        ordered: bool,
        number: u32,
        task: Option<bool>,
        origin_x: f32,
        top: f32,
        height: f32,
        font_size: f32,
        indent: f32,
    ) {
        // Centre the bullet and the task box on the glyphs, not on the line
        // box. The first baseline sits at 0.8 em (see [`Assets::layout`]) and
        // the spare room of the 1.55 line spacing lies below it, so the box
        // centre would drop the bullet onto the descenders. 0.33 em above the
        // baseline is the optical centre of the bullet glyph and of mixed
        // CJK/Latin text (measured with Microsoft YaHei UI).
        let center_y = top + font_size * (0.8 - 0.33);
        // SAFETY: drawing on the paper target with live brushes.
        unsafe {
            match (ordered, task) {
                (_, Some(checked)) => {
                    let box_rect = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: origin_x + 1.0,
                            top: center_y - 6.0,
                            right: origin_x + 13.0,
                            bottom: center_y + 6.0,
                        },
                        radiusX: 3.0,
                        radiusY: 3.0,
                    };
                    context.DrawRoundedRectangle(&box_rect, &self.assets.quote, 1.2, None);
                    if checked {
                        context.DrawLine(
                            Vector2 {
                                X: origin_x + 4.0,
                                Y: center_y,
                            },
                            Vector2 {
                                X: origin_x + 6.5,
                                Y: center_y + 2.5,
                            },
                            &self.assets.ink,
                            1.4,
                            None,
                        );
                        context.DrawLine(
                            Vector2 {
                                X: origin_x + 6.5,
                                Y: center_y + 2.5,
                            },
                            Vector2 {
                                X: origin_x + 11.0,
                                Y: center_y - 3.0,
                            },
                            &self.assets.ink,
                            1.4,
                            None,
                        );
                    }
                }
                (false, _) => {
                    let dot = windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                        point: Vector2 {
                            X: origin_x + 8.0,
                            Y: center_y,
                        },
                        radiusX: 2.0,
                        radiusY: 2.0,
                    };
                    context.FillEllipse(&dot, &self.assets.ink);
                }
                (true, None) => {
                    // A stack buffer keeps the number layout allocation-free
                    // even though the draw runs on every caret blink.
                    let mut digits = [0u16; 10];
                    let mut count = 0usize;
                    let mut value = number.max(1);
                    while value > 0 && count < digits.len() {
                        digits[count] = b'0' as u16 + (value % 10) as u16;
                        value /= 10;
                        count += 1;
                    }
                    let mut wide = [0u16; 11];
                    for index in 0..count {
                        wide[index] = digits[count - 1 - index];
                    }
                    wide[count] = b'.' as u16;
                    let len = count + 1;
                    let rect = D2D_RECT_F {
                        left: origin_x + indent - 22.0,
                        top,
                        right: origin_x + indent - 4.0,
                        bottom: top + height,
                    };
                    context.DrawText(
                        &wide[..len],
                        &self.assets.number,
                        &rect,
                        &self.assets.quote,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }
        }
    }

    fn paint_inline_code(
        &self,
        context: &ID2D1RenderTarget,
        line: &LaidLine,
        origin_x: f32,
        top: f32,
    ) {
        let Some(layout) = line.layout.as_ref() else {
            return;
        };
        let display = &self.display.lines[line.index];
        let mut offset = 0usize;
        for run in display.runs.iter().filter(|run| !run.marker) {
            if run.style == Style::Code {
                let start = utf16_offset(&line.text, offset);
                let length = run.text.encode_utf16().count() as u32;
                if length > 0 {
                    let mut metrics = [DWRITE_HIT_TEST_METRICS::default(); 8];
                    let mut count = 0u32;
                    // SAFETY: live layout, buffer and out-param.
                    unsafe {
                        let _ = layout.HitTestTextRange(
                            start,
                            length,
                            0.0,
                            0.0,
                            Some(&mut metrics[..]),
                            &mut count,
                        );
                    }
                    // SAFETY: drawing on the paper target.
                    unsafe {
                        for metric in metrics.iter().take(count as usize) {
                            let rect = D2D1_ROUNDED_RECT {
                                rect: D2D_RECT_F {
                                    left: origin_x + line.indent + metric.left - 2.0,
                                    top: top + metric.top,
                                    right: origin_x
                                        + line.indent
                                        + metric.left
                                        + metric.width
                                        + 2.0,
                                    bottom: top + metric.top + metric.height,
                                },
                                radiusX: 3.0,
                                radiusY: 3.0,
                            };
                            context.FillRoundedRectangle(&rect, &self.assets.code_background);
                        }
                    }
                }
            }
            offset += run.text.len();
        }
    }

    fn paint_selection(&self, context: &ID2D1RenderTarget, origin_x: f32) {
        let Some(selection) = self.doc.selection() else {
            return;
        };
        for line in &self.lines {
            if !in_view(line, self.scroll, self.view_height) {
                continue;
            }
            let Some(layout) = line.layout.as_ref() else {
                continue;
            };
            let display = &self.display.lines[line.index];
            for segment in display.segments(selection.clone(), line.active) {
                let mut start = segment.start;
                let mut end = segment.end;
                if let Some(composition) = &line.composition {
                    if start >= composition.end {
                        start -= composition.len();
                        end -= composition.len();
                    } else if start > composition.start {
                        start = composition.start;
                    }
                    if end > composition.start && end <= composition.end {
                        end = composition.start;
                    }
                }
                if start >= end {
                    continue;
                }
                let from = utf16_offset(&line.text, start);
                let length = (utf16_offset(&line.text, end) - from).max(1);
                let mut metrics = [DWRITE_HIT_TEST_METRICS::default(); 16];
                let mut count = 0u32;
                // SAFETY: live layout, buffer and out-param.
                unsafe {
                    let _ = layout.HitTestTextRange(
                        from,
                        length,
                        0.0,
                        0.0,
                        Some(&mut metrics[..]),
                        &mut count,
                    );
                }
                // SAFETY: drawing on the paper target.
                unsafe {
                    for metric in metrics.iter().take(count as usize) {
                        let rect = D2D_RECT_F {
                            left: origin_x + line.indent + metric.left,
                            top: PAD_TOP + line.text_top - self.scroll + metric.top,
                            right: origin_x + line.indent + metric.left + metric.width,
                            bottom: PAD_TOP + line.text_top - self.scroll
                                + metric.top
                                + metric.height,
                        };
                        context.FillRectangle(&rect, &self.assets.selection);
                    }
                }
            }
        }
    }

    fn paint_caret(&self, context: &ID2D1RenderTarget, origin_x: f32) {
        if !self.focused || !self.enabled || !self.caret_on {
            return;
        }
        let Some((x, y, height)) = self.caret_position() else {
            return;
        };
        let rect = D2D_RECT_F {
            left: origin_x + x,
            top: PAD_TOP + y - self.scroll,
            right: origin_x + x + 1.5,
            bottom: PAD_TOP + y - self.scroll + height,
        };
        // SAFETY: drawing on the paper target.
        unsafe {
            context.FillRectangle(&rect, &self.assets.ink);
        }
    }

    fn paint_placeholder(&self, context: &ID2D1RenderTarget, origin_x: f32) {
        if !self.doc.is_empty() || self.composition.is_some() {
            return;
        }
        let text = i18n::s().note_placeholder;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let rect = D2D_RECT_F {
            left: origin_x,
            top: PAD_TOP,
            right: origin_x + self.paper.content_width(),
            bottom: PAD_TOP + display::BODY_SIZE * display::BODY_SPACING,
        };
        // SAFETY: drawing on the paper target with the body format.
        unsafe {
            context.DrawText(
                &wide,
                &self.assets.body,
                &rect,
                &self.assets.marker,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    pub(super) fn paint_window(&self) {
        let context = &self.surface.context;
        // SAFETY: the swap-chain target is set and transparent.
        unsafe {
            context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            if let Some(shadow) = self.shadow.as_ref() {
                // Nine stretches: four corners 1:1, edges and middle scaled. Each
                // destination is whole device pixels, so no seam can open at a
                // slice boundary.
                for slice in shadow_slices(
                    self.width_px,
                    self.height_px,
                    shadow.corner_px,
                    shadow.middle_px,
                ) {
                    let destination = slice.dest.to_dip(self.scale);
                    let source = slice.src.to_dip(self.scale);
                    context.DrawBitmap(
                        &shadow.bitmap,
                        Some(&destination),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        Some(&source),
                        None,
                    );
                }
            }
            let paper = paper_rect(self.paper);
            let Some(geometry) = self.clip.as_ref() else {
                return;
            };
            let layer = D2D1_LAYER_PARAMETERS1 {
                contentBounds: D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: self.width_px as f32 / self.scale,
                    bottom: self.height_px as f32 / self.scale,
                },
                geometricMask: ManuallyDrop::new(Some(geometry.clone())),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: Matrix3x2::identity(),
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            context.PushLayer(
                &layer,
                None::<&windows::Win32::Graphics::Direct2D::ID2D1Layer>,
            );
            if let Some(paper_bitmap) = self.surface.paper.as_ref() {
                let destination = d2d_rect(paper);
                context.DrawBitmap(
                    paper_bitmap,
                    Some(&destination),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            }
            // The resize affordance: a 2 DIP highlight on the hit edge. Drawn
            // on the window layer (clipped to the rounded paper), never into
            // the paper bitmap, so ClearType and the commit image are intact.
            if let Some(zone) = self.hover {
                let grip = display::RESIZE_GRIP_DIP;
                let right = paper.right;
                let bottom = paper.bottom;
                let strips = match zone {
                    ResizeZone::Right => [
                        D2D_RECT_F {
                            left: right - grip,
                            top: paper.top,
                            right,
                            bottom,
                        },
                        D2D_RECT_F::default(),
                    ],
                    ResizeZone::Bottom => [
                        D2D_RECT_F {
                            left: paper.left,
                            top: bottom - grip,
                            right,
                            bottom,
                        },
                        D2D_RECT_F::default(),
                    ],
                    ResizeZone::Corner => [
                        D2D_RECT_F {
                            left: right - grip,
                            top: paper.top,
                            right,
                            bottom,
                        },
                        D2D_RECT_F {
                            left: paper.left,
                            top: bottom - grip,
                            right,
                            bottom,
                        },
                    ],
                };
                for strip in strips {
                    if strip.right > strip.left && strip.bottom > strip.top {
                        context.FillRectangle(&strip, &self.assets.hover);
                    }
                }
            }
            context.PopLayer();
            let rounded = D2D1_ROUNDED_RECT {
                rect: d2d_rect(paper),
                radiusX: RADIUS_DIP,
                radiusY: RADIUS_DIP,
            };
            let _ = geometry;
            if self.now < self.error_until {
                context.DrawRoundedRectangle(&rounded, &self.assets.error, BORDER_DIP, None);
            } else if let Some(brush) = self.border_brush.as_ref() {
                context.DrawRoundedRectangle(&rounded, brush, BORDER_DIP, None);
            }
        }
    }
}

/// True when any part of `line` can land on the paper and therefore be seen.
///
/// `line.top` is content-relative and `scroll` is the content top, which puts
/// the paper's top edge at `-PAD_TOP` and its bottom at `view_height +
/// PAD_BOTTOM`; everything outside the paper bitmap is clipped away when the
/// paper is composited. A line's box contains everything painted for it, so a
/// box outside that band draws no pixel at all.
pub(super) fn in_view(line: &LaidLine, scroll: f32, view_height: f32) -> bool {
    let top = line.top - scroll;
    top < view_height + PAD_BOTTOM && top + line.box_height > -PAD_TOP
}
