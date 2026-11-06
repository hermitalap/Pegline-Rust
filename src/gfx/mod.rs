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
    create_composition, create_d2d, create_d3d_device, create_dwrite, create_swap_chain,
};
