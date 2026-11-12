//! Development and acceptance-check scaffolding.
//!
//! Every `--*-demo` flag and its `PEGLINE_*` environment override lives here,
//! separated from the product state so the shipping paths in [`super`] stay
//! readable. The state is always compiled — the flags have to be parseable in a
//! release build, because the visual acceptance runs use the released exe — but
//! [`Dev::enabled`] is false for an ordinary launch, which lets the frame loop
//! skip the whole block after one comparison.
//!
//! Nothing here may affect a run that passes no flag: each state machine is
//! gated on its own flag in [`super::App::run_dev`].

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use super::{
    CardSpec, Command, EditingNote, Event, Item, NoteEditor, post_note_text, pseudo_random,
};
use crate::capture::wic;
use crate::note_editor::display::{PaperSize, ResizeZone};
use crate::types::{CardId, ImageData};
use crate::{layout, logging, notes, win32};

/// State for the scripted demos. Only `App::run_dev` reads it.
pub(crate) struct Dev {
    /// `--demo`: hang a few generated cards instead of restoring the user's.
    pub(crate) demo: bool,
    /// `--panic-test`: panic once inside the message handler to prove recovery.
    pub(crate) panic_test: bool,
    pub(crate) panicked: bool,
    /// `--editor-demo`: open the annotation editor on a generated picture and
    /// close it after a few seconds.
    pub(crate) editor_demo: bool,
    /// `--editor-hold` keeps that editor open for interactive checks.
    pub(crate) editor_hold: bool,
    pub(crate) editor_demo_at: Option<f64>,
    /// `--annotate-demo`: hang one demo card and open the real annotate flow.
    pub(crate) annotate_demo: bool,
    pub(crate) annotate_demo_stage: u8,
    pub(crate) annotate_demo_at: f64,
    /// `--note-editor-demo`: open the WYSIWYG note editor with sample text.
    pub(crate) note_demo: bool,
    pub(crate) note_demo_opened: bool,
    /// `--note-hover-demo`: keep the line down and park a synthetic pointer on
    /// the entry hotspot.
    pub(crate) note_hover_demo: bool,
    /// Where that synthetic pointer sits, DIP from the hotspot centre, from
    /// `PEGLINE_NOTE_DEMO_POINTER` (`x,y`).
    pub(crate) note_demo_pointer: (f32, f32),
    /// `PEGLINE_NOTE_PEEK_POINTER`: an absolute screen point for the hover demo
    /// that is outside the hotspot.
    pub(crate) note_peek_pointer: Option<(i32, i32)>,
    /// Slow-motion factor for the hover fly-in/out, from `PEGLINE_NOTE_SLOW`.
    pub(crate) note_slow: f32,
    /// `--note-flow-demo`: open a new note, type into it, commit, let it land.
    pub(crate) note_flow_demo: bool,
    pub(crate) note_flow_stage: u8,
    pub(crate) note_flow_at: f64,
    /// `--note-discard-demo`: open an empty new note and discard it.
    pub(crate) note_discard_demo: bool,
    pub(crate) note_discard_stage: u8,
    pub(crate) note_discard_at: f64,
    /// `--note-storage-demo`: create a temporary note, edit and save it through
    /// the real card editor path, then archive it. No user note is touched.
    pub(crate) note_storage_demo: bool,
    pub(crate) note_storage_stage: u8,
    pub(crate) note_storage_at: f64,
    pub(crate) note_storage_id: Option<CardId>,
    pub(crate) note_storage_path: Option<PathBuf>,
    pub(crate) note_storage_archived: Option<PathBuf>,
    /// `--note-preview-demo`: hang one note and park a synthetic pointer on its
    /// card so the note hover preview can be captured deterministically.
    pub(crate) note_preview_demo: bool,
    pub(crate) note_preview_demo_card: Option<CardId>,
    /// `--note-resize-demo`: open the note editor at a paper size chosen by
    /// `PEGLINE_NOTE_RESIZE_PAPER` (`WxH`, DIP) so the resize result can be
    /// photographed without a mouse.
    pub(crate) note_resize_demo: bool,
    pub(crate) note_resize_paper: PaperSize,
    /// `PEGLINE_NOTE_RESIZE_HOVER` (`right`, `bottom`, `corner`) highlights one
    /// resize edge in that demo.
    pub(crate) note_resize_hover: Option<ResizeZone>,
    /// When the highlight was last re-asserted; the real pointer can enter the
    /// window and clear it, so the demo keeps asking.
    pub(crate) note_resize_hover_at: f64,
}

impl Dev {
    /// Parses the command line and the `PEGLINE_*` environment once.
    pub(crate) fn from_env() -> Self {
        let arg = |flag: &str| std::env::args().any(|value| value == flag);
        Self {
            demo: arg("--demo"),
            panic_test: arg("--panic-test"),
            panicked: false,
            editor_demo: arg("--editor-demo"),
            editor_hold: arg("--editor-hold"),
            editor_demo_at: None,
            annotate_demo: arg("--annotate-demo"),
            annotate_demo_stage: 0,
            annotate_demo_at: 0.0,
            note_demo: arg("--note-editor-demo"),
            note_demo_opened: false,
            note_hover_demo: arg("--note-hover-demo"),
            note_demo_pointer: std::env::var("PEGLINE_NOTE_DEMO_POINTER")
                .ok()
                .and_then(|value| parse_pointer(&value))
                .unwrap_or((78.0, 80.0)),
            note_peek_pointer: std::env::var("PEGLINE_NOTE_PEEK_POINTER")
                .ok()
                .and_then(|value| parse_pointer(&value))
                .map(|(x, y)| (x as i32, y as i32)),
            note_slow: std::env::var("PEGLINE_NOTE_SLOW")
                .ok()
                .and_then(|value| value.parse::<f32>().ok())
                .unwrap_or(1.0)
                .clamp(1.0, 10.0),
            note_flow_demo: arg("--note-flow-demo"),
            note_flow_stage: 0,
            note_flow_at: 0.0,
            note_discard_demo: arg("--note-discard-demo"),
            note_discard_stage: 0,
            note_discard_at: 0.0,
            note_storage_demo: arg("--note-storage-demo"),
            note_storage_stage: 0,
            note_storage_at: 0.0,
            note_storage_id: None,
            note_storage_path: None,
            note_storage_archived: None,
            note_preview_demo: arg("--note-preview-demo"),
            note_preview_demo_card: None,
            note_resize_demo: arg("--note-resize-demo"),
            note_resize_paper: std::env::var("PEGLINE_NOTE_RESIZE_PAPER")
                .ok()
                .and_then(|value| crate::note_editor::display::parse_note_size(&value))
                .unwrap_or(crate::note_editor::display::MAX_PAPER),
            note_resize_hover: std::env::var("PEGLINE_NOTE_RESIZE_HOVER")
                .ok()
                .and_then(|value| parse_zone(&value)),
            note_resize_hover_at: 0.0,
        }
    }

    /// True when any scripted demo is running. A plain launch returns false, and
    /// the frame loop then costs one comparison per tick.
    pub(crate) fn enabled(&self) -> bool {
        self.demo
            || self.panic_test
            || self.editor_demo
            || self.editor_demo_at.is_some()
            || self.annotate_demo
            || self.note_demo
            || self.note_hover_demo
            || self.note_flow_demo
            || self.note_discard_demo
            || self.note_storage_demo
            || self.note_preview_demo
            || self.note_resize_demo
    }
}

/// `right`, `bottom` or `corner`, as `PEGLINE_NOTE_RESIZE_HOVER` passes it.
fn parse_zone(text: &str) -> Option<ResizeZone> {
    match text.trim().to_ascii_lowercase().as_str() {
        "right" => Some(ResizeZone::Right),
        "bottom" => Some(ResizeZone::Bottom),
        "corner" => Some(ResizeZone::Corner),
        _ => None,
    }
}

/// `"x,y"` in DIP, as the visual acceptance scripts pass it.
///
/// A malformed value keeps the default pointer instead of failing the launch.
fn parse_pointer(text: &str) -> Option<(f32, f32)> {
    let (x, y) = text.split_once(',')?;
    let x = x.trim().parse::<f32>().ok()?;
    let y = y.trim().parse::<f32>().ok()?;
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// The demo entry points the frame loop and startup call.
///
/// These live in this module rather than in the parent because they drive real
/// product paths on purpose: an acceptance run has to exercise the same code a
/// user does, not a parallel copy of it.
impl super::App {
    /// Runs the scripted acceptance demos (`--*-demo`).
    ///
    /// A plain launch returns after one flag test, so this costs nothing when
    /// no demo was asked for. The state lives in [`dev::Dev`]; the logic stays
    /// here because it drives the real product paths on purpose (that is the
    /// point of the acceptance runs).
    pub(super) fn run_dev(&mut self, now: f64) {
        if !self.dev.enabled() {
            return;
        }
        if self.dev.note_demo && !self.dev.note_demo_opened && now - self.start_time > 1.0 {
            self.dev.note_demo_opened = true;
            self.open_note_demo(self.dev.note_resize_paper, None);
        }
        if self.dev.note_resize_demo && !self.dev.note_demo_opened && now - self.start_time > 1.0 {
            self.dev.note_demo_opened = true;
            self.open_note_demo(self.dev.note_resize_paper, self.dev.note_resize_hover);
        }
        if self.dev.note_resize_demo
            && self.dev.note_demo_opened
            && now - self.dev.note_resize_hover_at > 0.5
        {
            // A real pointer entering the window sets its own highlight; the
            // scripted screenshot wants the one it asked for.
            if let (Some(zone), Some(editor)) = (
                self.dev.note_resize_hover,
                self.windows.note_editor.as_ref(),
            ) {
                self.dev.note_resize_hover_at = now;
                editor.hover(Some(zone));
            }
        }
        if self.dev.note_hover_demo {
            self.state.keep_open = true;
            // The synthetic pointer, not the real one, decides whether the
            // entry is hovered; keep the strip down even while the real mouse
            // is elsewhere on the desktop.
            self.state.pinned = true;
            if !self.state.is_present {
                self.present();
            }
            if !self.state.is_revealed {
                self.reveal(false, 0.0);
            }
        }
        if self.dev.note_preview_demo {
            // Same idea: a synthetic pointer sits on the demo note, so the
            // strip has to stay down while the real mouse is elsewhere.
            self.state.keep_open = true;
            self.state.pinned = true;
            if !self.state.is_present {
                self.present();
            }
            if !self.state.is_revealed {
                self.reveal(false, 0.0);
            }
        }
        if self.dev.note_flow_demo {
            self.state.keep_open = true;
            if !self.state.is_present {
                self.present();
            }
            if !self.state.is_revealed {
                self.reveal(false, 0.0);
            }
            match self.dev.note_flow_stage {
                0 if now - self.start_time > 1.0 => {
                    self.open_new_note_editor();
                    self.dev.note_flow_stage = 1;
                    self.dev.note_flow_at = now;
                }
                1 if now - self.dev.note_flow_at > 0.8 => {
                    if let Some(editor) = self.windows.note_editor.as_ref() {
                        // The list line's newline now continues "- " by itself,
                        // so the task line must not repeat the bullet.
                        post_note_text(editor.hwnd(), "Demo note\n- one\n[x] two");
                    }
                    self.dev.note_flow_stage = 2;
                    self.dev.note_flow_at = now;
                }
                2 if now - self.dev.note_flow_at > 0.8 => {
                    if let Some(editor) = self.windows.note_editor.as_mut() {
                        editor.commit();
                    }
                    self.dev.note_flow_stage = 3;
                    self.dev.note_flow_at = now;
                }
                _ => {}
            }
        }
        if self.dev.note_discard_demo {
            self.state.keep_open = true;
            if !self.state.is_present {
                self.present();
            }
            if !self.state.is_revealed {
                self.reveal(false, 0.0);
            }
            match self.dev.note_discard_stage {
                0 if now - self.start_time > 1.0 => {
                    self.open_new_note_editor();
                    self.dev.note_discard_stage = 1;
                    self.dev.note_discard_at = now;
                }
                1 if now - self.dev.note_discard_at > 1.5 => {
                    // An empty note: this is the discard path.
                    if let Some(editor) = self.windows.note_editor.as_mut() {
                        editor.commit();
                    }
                    self.dev.note_discard_stage = 2;
                    self.dev.note_discard_at = now;
                }
                _ => {}
            }
        }
        if self.dev.note_storage_demo {
            self.state.keep_open = true;
            if !self.state.is_present {
                self.present();
            }
            if !self.state.is_revealed {
                self.reveal(false, 0.0);
            }
            match self.dev.note_storage_stage {
                0 if now - self.start_time > 1.2 => {
                    self.start_note_storage_demo();
                    self.dev.note_storage_stage = if self.dev.note_storage_id.is_some() {
                        1
                    } else {
                        99
                    };
                    self.dev.note_storage_at = now;
                }
                1 if now - self.dev.note_storage_at > 1.2 => {
                    if let Some(editor) = self.windows.note_editor.as_ref() {
                        post_note_text(editor.hwnd(), " edited");
                    }
                    self.dev.note_storage_stage = 2;
                    self.dev.note_storage_at = now;
                }
                2 if now - self.dev.note_storage_at > 0.8 => {
                    if let Some(editor) = self.windows.note_editor.as_mut() {
                        editor.commit();
                    }
                    self.dev.note_storage_stage = 3;
                    self.dev.note_storage_at = now;
                }
                3 if now - self.dev.note_storage_at > 1.5 => {
                    self.verify_note_storage_save();
                    if let Some(id) = self.dev.note_storage_id {
                        self.discard(id);
                    }
                    self.dev.note_storage_stage = 4;
                    self.dev.note_storage_at = now;
                }
                4 if now - self.dev.note_storage_at > 1.5 => {
                    self.finish_note_storage_demo();
                    self.dev.note_storage_stage = 5;
                }
                _ => {}
            }
        }
        if self.dev.annotate_demo {
            if self.dev.annotate_demo_stage == 0 {
                self.dev.annotate_demo_stage = 1;
                self.dev.annotate_demo_at = now;
            }
            self.state.keep_open = true;
            if !self.state.is_present {
                self.present();
            }
            if !self.state.is_revealed {
                self.reveal(false, 0.0);
            }
            match self.dev.annotate_demo_stage {
                1 if now - self.dev.annotate_demo_at > 1.2 => {
                    if self.demo_card_id().is_none() {
                        self.install_demo_cards();
                    }
                    self.dev.annotate_demo_stage = 2;
                    self.dev.annotate_demo_at = now;
                }
                2 if now - self.dev.annotate_demo_at > 1.2 => {
                    // Only ever annotate a demo card: a restored user picture
                    // must not be rewritten by a development switch.
                    if let Some(id) = self.demo_card_id() {
                        self.annotate_item(id);
                    }
                    self.dev.annotate_demo_stage = 3;
                }
                _ => {}
            }
        }
    }

    /// `--note-storage-demo`: create a temporary note, edit it through the real
    /// card editor, save it, archive it through `discard`, and clean up.
    pub(super) fn start_note_storage_demo(&mut self) {
        let text = "storage demo\nline two".to_string();
        let path = match notes::create(&text) {
            Ok(path) => path,
            Err(error) => {
                logging::error(format!("storage demo create: {error}"));
                return;
            }
        };
        let Some(image) = notes::render(&text, 0) else {
            let _ = std::fs::remove_file(&path);
            logging::error("storage demo render failed");
            return;
        };
        let id = self.alloc_id();
        self.add_item(
            id,
            path.clone(),
            true,
            (notes::CARD_WIDTH as u32, notes::CARD_HEIGHT as u32),
            image,
            None,
            true,
            false,
            true,
            0,
            Some(text),
        );
        self.dev.note_storage_id = Some(id);
        self.dev.note_storage_path = Some(path.clone());
        logging::trace(format!("storage demo: created {}", path.display()));
        self.open_note_editor(id);
    }

    /// `--note-preview-demo`: one note on an otherwise empty line, with a
    /// synthetic pointer parked on its card. It makes the note hover preview of
    /// BEHAVIOR §7 reproducible by hand and by the visual acceptance run.
    pub(super) fn start_note_preview_demo(&mut self) {
        let directory = win32::local_dir().join("demo");
        if let Err(error) = std::fs::create_dir_all(&directory) {
            logging::error(format!("note preview demo dir: {error}"));
            return;
        }
        let text = "# 便签预览\n\n- 第一项\n- [x] 已完成\n- [ ] 待办\n\n1. 有序一项\n2. 有序二项\n\n> 引用 `代码` 和 **粗体**。\n\nA longer line of English text so the preview wraps inside the sheet."
            .to_string();
        let path = directory.join("note-preview-demo.txt");
        if let Err(error) = std::fs::write(&path, text.as_bytes()) {
            logging::error(format!("note preview demo write: {error}"));
            return;
        }
        let Some(image) = notes::render(&text, 0) else {
            logging::error("note preview demo render failed");
            return;
        };
        let id = self.alloc_id();
        self.dev.note_preview_demo_card = Some(id);
        self.add_item(
            id,
            path,
            false,
            (notes::CARD_WIDTH as u32, notes::CARD_HEIGHT as u32),
            image,
            None,
            false,
            false,
            true,
            0,
            Some(text),
        );
        self.present();
        self.reveal(true, 0.0);
        logging::trace("note preview demo: card hung");
    }

    pub(super) fn verify_note_storage_save(&self) {
        let Some(id) = self.dev.note_storage_id else {
            return;
        };
        let item = self.line.items.iter().find(|item| item.id == id);
        let text = item.and_then(|item| item.text.clone()).unwrap_or_default();
        let bytes = self
            .dev
            .note_storage_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .unwrap_or_default();
        let has_bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
        let has_crlf = bytes.windows(2).any(|pair| pair == b"\r\n");
        logging::trace(format!(
            "storage demo save: has_card={} text_has_edit={} bytes={} bom={has_bom} crlf={has_crlf}",
            item.is_some(),
            text.contains("edited"),
            bytes.len()
        ));
    }

    pub(super) fn finish_note_storage_demo(&mut self) {
        if let Some(archived) = self.dev.note_storage_archived.take() {
            logging::trace(format!(
                "storage demo archive: exists={} path={}",
                archived.exists(),
                archived.display()
            ));
            let _ = std::fs::remove_file(&archived);
        }
        if let Some(path) = self.dev.note_storage_path.take() {
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
        }
        if let Some(id) = self.dev.note_storage_id.take() {
            if self.line.items.iter().any(|item| item.id == id) {
                self.remove_item(id);
            }
        }
        self.save_pegged();
        logging::trace("storage demo complete");
    }

    /// `--note-editor-demo`: show the editor with a document that exercises the
    /// v1 Markdown subset and the active-line reveal.
    pub(super) fn open_note_demo(&mut self, paper: PaperSize, hover: Option<ResizeZone>) {
        if self.windows.note_editor.is_some() {
            return;
        }
        let Some(monitor) = self.screen.or_else(|| self.pointer_screen()) else {
            return;
        };
        let text = "# Pegline\n\nA note that edits **in place**: the caret line shows its source and the rest is Markdown.\n\n## Lists\n- first item\n- [x] done\n- [ ] to do\n1. one\n2. two\n\n> A quote.\n\n`inline code` and [a link](https://example.com).\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\n中文便签：光标所在行显示原文，其余行实时渲染。"
            .to_string();
        let tx = self.events_tx.clone();
        let notify = Box::new(move |event| {
            let _ = tx.send(Event::NoteEditor(event));
        });
        match NoteEditor::open(text.clone(), 0, paper, monitor.dpi, monitor, notify) {
            Ok(editor) => {
                if let Some(zone) = hover {
                    // The render thread owns the highlight; the screenshot run
                    // asks for it without a pointer.
                    editor.hover(Some(zone));
                }
                self.windows.note_editor = Some(editor);
                self.sessions.note_editing = Some(EditingNote {
                    path: None,
                    card: None,
                    original: text,
                    color: 0,
                });
                self.sessions.note_committing = false;
                self.present();
                self.reveal(true, 0.0);
            }
            Err(error) => logging::error(format!("note editor demo: {error}")),
        }
    }

    /// `--editor-demo`: a generated picture in the real annotation editor, so
    /// the window, the GPU stack and the toolbar can be checked without a real
    /// capture.
    /// `--editor-hold` keeps it open for interactive (manual or scripted)
    /// checks instead of the four-second smoke test.
    pub(super) fn install_editor_demo(&mut self) {
        let (w, h) = (900u32, 600u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let index = ((y * w + x) * 4) as usize;
                pixels[index] = (40 + x * 180 / w) as u8;
                pixels[index + 1] = (60 + y * 150 / h) as u8;
                pixels[index + 2] = 200u8.saturating_sub((x * 120 / w) as u8);
                pixels[index + 3] = 255;
            }
        }
        let image = ImageData::new(w, h, pixels);
        let encoded = match wic::encode_png_bytes(&image) {
            Ok(bytes) => bytes,
            Err(error) => {
                logging::error(format!("editor demo encode: {error}"));
                return;
            }
        };
        let directory = win32::local_dir();
        if let Err(error) = std::fs::create_dir_all(&directory) {
            logging::error(format!("editor demo dir: {error}"));
            return;
        }
        let path = directory.join("editor-demo.png");
        if let Err(error) = wic::write_png_bytes(&path, &encoded) {
            logging::error(format!("editor demo write: {error}"));
            return;
        }
        self.open_editor_for(path, false, None, (w, h));
        let hold = self.dev.editor_hold;
        self.dev.editor_demo_at = if hold {
            None
        } else {
            Some(win32::now_seconds() + 4.0)
        };
    }

    /// The first card that belongs to the development demo folder, if any.
    /// `--annotate-demo` only ever edits one of these, never a user picture.
    pub(super) fn demo_card_id(&self) -> Option<CardId> {
        let directory = win32::local_dir().join("demo");
        self.line
            .items
            .iter()
            .find(|item| item.path.starts_with(&directory))
            .map(|item| item.id)
    }

    pub(super) fn install_demo_cards(&mut self) {
        let directory = win32::local_dir().join("demo");
        if let Err(error) = std::fs::create_dir_all(&directory) {
            logging::error(format!("demo dir: {error}"));
        }
        for index in 0..3u32 {
            let (w, h) = match index {
                0 => (480, 270),
                1 => (270, 480),
                _ => (300, 300),
            };
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    pixels[i] = (40 + (x * 180 / w)) as u8;
                    pixels[i + 1] = (80 + (y * 120 / h)) as u8;
                    pixels[i + 2] = 180 + (index * 20) as u8;
                    pixels[i + 3] = 255;
                }
            }
            let id = self.alloc_id();
            let size_px = (w, h);
            let card_size = layout::card_size(w as f32, h as f32);
            let tilt = pseudo_random(id) * 5.0 - 2.5;
            // Demo cards use real files so the hover preview, copy and open
            // exercise the same decode path as a real capture. `own` stays
            // false: the files belong to the demo folder, not the user.
            let file = directory.join(format!("demo-{index}.png"));
            let path = match wic::encode_png_bytes(&ImageData::new(w, h, pixels.clone()))
                .and_then(|bytes| wic::write_png_bytes(&file, &bytes))
            {
                Ok(()) => file,
                Err(error) => {
                    logging::error(format!("demo card {index}: {error}"));
                    PathBuf::from(format!("demo://card{index}"))
                }
            };
            self.line.items.push(Item {
                id,
                path,
                own: false,
                note: false,
                color: 0,
                text: None,
                size_px,
                card_size,
                tilt,
                thumb: ImageData::new(w, h, pixels.clone()),
                missing: 0,
                last_write: UNIX_EPOCH,
                revision: 1,
                captured_at: 0.0,
            });
            self.send_scene(Command::AddCard(CardSpec {
                id,
                card_size,
                tilt,
                arriving: false,
                image: Some(ImageData::new(w, h, pixels)),
                note: false,
            }));
        }
        self.present();
        self.state.keep_open = true;
        self.reveal(true, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_pointer;

    #[test]
    fn the_pointer_parses_only_two_finite_numbers() {
        assert_eq!(parse_pointer("78,80"), Some((78.0, 80.0)));
        assert_eq!(parse_pointer(" -78 , -80 "), Some((-78.0, -80.0)));
        assert_eq!(parse_pointer("78"), None);
        assert_eq!(parse_pointer("a,80"), None);
        assert_eq!(parse_pointer("78,NaN"), None);
    }
}
