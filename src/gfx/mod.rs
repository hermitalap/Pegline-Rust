//! GPU device plumbing shared by the windows that present a composition swap
//! chain: the line ([`crate::render`]), the annotation editor
//! ([`crate::editor::window`]) and the note editor
//! ([`crate::note_editor::render`]).
//!
//! Only the creation constructors live here. Each window still keeps its own
//! `D3D11`/`D2D` stack and its own keep-alive fields: a lost device on one
//! window must not take the others down, so nothing is cached or shared at run
//! time.

pub(crate) mod device;

pub(crate) use device::{
    attach_content, begin_draw, bind_back_buffer, bitmap_target_props, create_composition,
    create_d2d, create_d3d_device, create_dwrite, create_swap_chain, create_text_format,
    create_text_layout, d2d_rect, end_draw, pixel_format, present, resize_swap_chain, rounded,
    solid, swap_chain_props, text_metrics, upload_bitmap,
};
