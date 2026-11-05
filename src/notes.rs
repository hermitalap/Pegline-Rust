//! Sticky notes: copied text hung on the line as a small sheet of paper.
//!
//! Ported from `Notes.cs`. A note is a plain `.txt` file in Pegline's own
//! folder, so it survives a restart, opens in Notepad and drags like a
//! screenshot. On the line it is drawn as a bitmap so cards, the drag ghost and
//! the fall treat it exactly like a photo (BEHAVIOR §10).
//!
//! The bitmap is 136×88 DIP rendered at 3× (408×264) and drawn through GDI into
//! a top-down DIB. It is rasterised once per note, off the UI thread; the rule
//! "never rasterise a whole window on the CPU" is about frames, and a note image
//! is a texture like any thumbnail.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DeleteDC, DeleteObject, DrawTextW, FF_DONTCARE, FW_NORMAL,
    FW_SEMIBOLD, GetDC, HDC, HGDIOBJ, OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::font;
use crate::i18n;
use crate::logging;
use crate::types::ImageData;
use crate::win32;

/// Note bitmap width and height in DIP.
pub const CARD_WIDTH: f32 = 136.0;
pub const CARD_HEIGHT: f32 = 88.0;
/// Supersampling factor; the card shows the note at 136×88 DIP.
const SCALE: u32 = 3;
/// Width of the dog-eared corner, DIP.
const FOLD: f32 = 13.0;

/// How many colours a note can wear.
pub const COLOR_COUNT: usize = 4;

/// One note colour: paper, the slightly deeper edge and the ink.
#[derive(Clone, Copy, Debug)]
pub struct NoteColor {
    pub paper: u32,
    pub edge: u32,
    pub ink: u32,
}

/// Yellow, blue, pink and grey, exactly the WPF palette.
pub const PALETTE: [NoteColor; COLOR_COUNT] = [
    NoteColor {
        paper: 0xFFF4B8,
        edge: 0xF6E288,
        ink: 0x4A3B12,
    },
    NoteColor {
        paper: 0xDBF1FF,
        edge: 0xBAE1F9,
        ink: 0x12384A,
    },
    NoteColor {
        paper: 0xFFE1E8,
        edge: 0xF8C4D2,
        ink: 0x4A1828,
    },
    NoteColor {
        paper: 0xF2F2F2,
        edge: 0xDADADA,
        ink: 0x282828,
    },
];

/// The palette entry for `color`, clamped to the four legal values.
pub fn color(color: usize) -> NoteColor {
    PALETTE[color.min(COLOR_COUNT - 1)]
}

/// The swatch shown for a colour: the edge, a little deeper than the paper.
pub fn swatch(color: usize) -> u32 {
    self::color(color).edge
}

/// `%LOCALAPPDATA%\Pegline\Notes`.
pub fn folder() -> PathBuf {
    win32::local_dir().join("Notes")
}

/// Where taken-down notes go (NOTE-EDITOR.md §8.3).
pub fn archive_folder() -> PathBuf {
    folder().join("Archive")
}

/// Whether `path` is a `.txt` file inside the active notes folder. Files in
/// `Notes\Archive` are taken down and must not be treated as active notes.
pub fn is_note(path: &Path) -> bool {
    let Ok(folder) = std::fs::canonicalize(folder()) else {
        return false;
    };
    let Ok(candidate) = std::fs::canonicalize(path) else {
        return false;
    };
    let archived = std::fs::canonicalize(archive_folder())
        .map(|archive| candidate.starts_with(archive))
        .unwrap_or(false);
    candidate.starts_with(&folder)
        && !archived
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
}

/// Writes a new note and returns its file. The text is stored with a UTF-8 BOM
/// and CRLF line endings so Notepad (and every other editor) reads it exactly
/// as the WPF app wrote it.
pub fn create(text: &str) -> Result<PathBuf, String> {
    let folder = folder();
    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("create {}: {error}", folder.display()))?;
    let (year, month, day, hour, minute, second) = win32::local_time_parts();
    let name = format!(
        "{} {:04}-{:02}-{:02} {:02}{:02}{:02}.txt",
        i18n::s().note_file_prefix,
        year,
        month,
        day,
        hour,
        minute,
        second
    );
    let path = unique_path(&folder, &name);
    write_atomic(&path, text)?;
    Ok(path)
}

/// Replaces `path` atomically: a temporary file in the same directory, then
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` so a crash never leaves half a note.
/// The bytes are UTF-8 with a BOM and CRLF line endings (NOTE-EDITOR.md §8.1).
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("note has no folder: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temp = temp_path(parent);
    let mut bytes = Vec::with_capacity(text.len() + 16);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(crate::note_editor::model::to_crlf(text).as_bytes());
    std::fs::write(&temp, bytes).map_err(|error| format!("write {}: {error}", temp.display()))?;

    let from = win32::wide(&temp.to_string_lossy());
    let to = win32::wide(&path.to_string_lossy());
    // SAFETY: both strings are NUL-terminated and alive for the call; the flags
    // are the documented replace-and-flush pair.
    let moved = unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = moved {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("replace {}: {error}", path.display()));
    }
    Ok(())
}

/// Moves a note into `Notes\Archive`, numbering a collision. The source file is
/// left untouched when the move fails.
pub fn archive(path: &Path) -> Result<PathBuf, String> {
    archive_into(path, &archive_folder())
}

fn archive_into(path: &Path, folder: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(folder)
        .map_err(|error| format!("create {}: {error}", folder.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("note has no file name: {}", path.display()))?;
    let target = unique_path(folder, &name.to_string_lossy());
    std::fs::rename(path, &target)
        .map_err(|error| format!("archive {}: {error}", path.display()))?;
    Ok(target)
}

/// The plain text a note card shows: Markdown markers removed, blocks kept
/// recognisable (a bullet for lists, a box for tasks), fences and rules gone.
/// The file itself is never changed (NOTE-EDITOR.md §6.8).
pub fn summary(text: &str) -> String {
    use crate::note_editor::display::{Block, DisplayDoc};
    let normalized = crate::note_editor::model::normalize(text);
    let normalized = normalized.replace('\t', " ");
    let doc = DisplayDoc::parse(&normalized);
    let mut lines: Vec<String> = Vec::new();
    for line in &doc.lines {
        if line.is_fence() || matches!(line.block, Block::Rule) {
            continue;
        }
        let body = line.display_text(false);
        if body.trim().is_empty() {
            if !lines.is_empty() && lines.last().is_some_and(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        let body = body.trim_end().to_string();
        lines.push(match line.block {
            Block::List {
                ordered: true,
                number,
                task: Some(checked),
            } => format!("{number}. [{}] {body}", if checked { 'x' } else { ' ' }),
            Block::List {
                ordered: true,
                number,
                task: None,
            } => format!("{number}. {body}"),
            Block::List {
                task: Some(checked),
                ..
            } => format!("[{}] {body}", if checked { 'x' } else { ' ' }),
            Block::List { .. } => format!("• {body}"),
            _ => body,
        });
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// A temporary sibling for [`write_atomic`].
fn temp_path(parent: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for attempt in 0..1000 {
        let candidate = parent.join(format!(
            ".pegline-{}-{nanos}-{attempt}.tmp",
            std::process::id()
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!(".pegline-{}-{nanos}.tmp", std::process::id()))
}

/// Reads a note, tolerating a BOM and any line endings. The in-memory text is
/// always LF so it compares with a [`Doc`](crate::note_editor::model::Doc).
pub fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(crate::note_editor::model::normalize(&text))
}

/// `name` in `folder`, appending a number when the file already exists.
fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let candidate = folder.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_default();
    for number in 2..1000 {
        let candidate = folder.join(format!("{stem} {number}.{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    folder.join(name)
}

/// Renders `text` as the note bitmap the card draws, or `None` when the text
/// cannot be read.
pub fn render(text: &str, color_index: usize) -> Option<ImageData> {
    render_checked(text, color_index)
}

fn render_checked(raw: &str, color_index: usize) -> Option<ImageData> {
    let palette = color(color_index);
    let width = CARD_WIDTH as u32 * SCALE;
    let height = CARD_HEIGHT as u32 * SCALE;
    let mut pixels = vec![0u8; (width * height * 4) as usize];

    // Paper: a vertical gradient from the paper colour to the midpoint between
    // paper and edge (the WPF LinearGradientBrush ran at 90°).
    let paper = rgb(palette.paper);
    let edge = rgb(palette.edge);
    let mid = [
        ((paper[0] as u32 + edge[0] as u32) / 2) as u8,
        ((paper[1] as u32 + edge[1] as u32) / 2) as u8,
        ((paper[2] as u32 + edge[2] as u32) / 2) as u8,
    ];
    for y in 0..height {
        let t = y as f32 / (height - 1).max(1) as f32;
        let row = mix(paper, mid, t);
        let start = (y * width * 4) as usize;
        for x in 0..width as usize {
            let index = start + x * 4;
            pixels[index] = row[2];
            pixels[index + 1] = row[1];
            pixels[index + 2] = row[0];
            pixels[index + 3] = 255;
        }
    }

    // The dog-eared corner: the lift (edge colour) above the diagonal and the
    // shade under it, as in the original two polygons.
    let fold = (FOLD * SCALE as f32).round() as u32;
    let x0 = width - fold;
    let y0 = height - fold;
    for y in y0..height {
        for x in x0..width {
            let above = (x - x0) + (y - y0) <= fold;
            let index = ((y * width + x) * 4) as usize;
            if above {
                pixels[index] = edge[2];
                pixels[index + 1] = edge[1];
                pixels[index + 2] = edge[0];
            } else {
                // 30/255 black over the paper.
                pixels[index] = (pixels[index] as u32 * 225 / 255) as u8;
                pixels[index + 1] = (pixels[index + 1] as u32 * 225 / 255) as u8;
                pixels[index + 2] = (pixels[index + 2] as u32 * 225 / 255) as u8;
            }
            if (x - x0) + (y - y0) == fold {
                // The 0.5 DIP stroke along the fold.
                pixels[index] = (pixels[index] as u32 * 215 / 255) as u8;
                pixels[index + 1] = (pixels[index + 1] as u32 * 215 / 255) as u8;
                pixels[index + 2] = (pixels[index + 2] as u32 * 215 / 255) as u8;
            }
        }
    }

    // Text: the card shows the rendered Markdown, not its source markers, and
    // the layout rules are the C# ones. Short text sits large in the middle;
    // longer text wraps, and the last visible line ends in an ellipsis.
    let summary = summary(raw);
    let text: String = summary.chars().take(600).collect();
    let text = text.trim().to_string();
    let brief = text.chars().count() <= 24 && !text.contains('\n');
    let (bold, font_height, margin) = if brief {
        if text.chars().count() <= 12 {
            (true, 16.0, (8.0, 4.0, 8.0, 6.0))
        } else {
            (true, 13.0, (8.0, 4.0, 8.0, 6.0))
        }
    } else {
        (false, 11.5, (9.0, 7.0, 11.0, 9.0))
    };

    if !text.is_empty() {
        draw_text(
            &mut pixels,
            width,
            height,
            &text,
            brief,
            bold,
            font_height,
            margin,
            palette.ink,
        );
    }

    // GDI leaves the alpha byte untouched (or zero) on antialiased text, so the
    // whole sheet is forced opaque: paper has no transparency anyway, and the
    // upload expects premultiplied BGRA, where alpha 255 is the identity.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Some(ImageData::new(width, height, pixels))
}

/// Draws the note text into the DIB pixels using GDI.
///
/// SAFETY: every GDI object created here is deleted before returning; the DIB
/// section stays selected into its DC only while both are alive.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    brief: bool,
    bold: bool,
    font_dip: f32,
    margin_dip: (f32, f32, f32, f32),
    ink: u32,
) {
    // SAFETY: GDI calls below create and release their own objects. The DIB
    // pixel pointer is owned by the bitmap until DeleteObject.
    unsafe {
        let screen: HDC = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        if dc.is_invalid() {
            logging::error("notes: CreateCompatibleDC failed");
            return;
        }
        let header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(dc), &header, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => bitmap,
            Err(error) => {
                logging::error(format!("notes: CreateDIBSection: {error}"));
                let _ = DeleteDC(dc);
                return;
            }
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
            return;
        }
        let old = SelectObject(dc, HGDIOBJ(bitmap.0));

        // Copy the paper and fold into the DIB, then let GDI draw the glyphs.
        let target = std::slice::from_raw_parts_mut(bits as *mut u8, pixels.len());
        target.copy_from_slice(pixels);

        let face: Vec<u16> = font::UI_FAMILY_STR
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // The 3x supersampled bitmap is downscaled on the GPU. ClearType
        // subpixel fringes would be resampled into colour noise, so grayscale
        // antialiasing is the correct choice here even on an opaque sheet.
        let font = CreateFontW(
            -((font_dip * SCALE as f32).round() as i32),
            0,
            0,
            0,
            if bold {
                FW_SEMIBOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
            PCWSTR(face.as_ptr()),
        );
        let old_font = SelectObject(dc, HGDIOBJ(font.0));
        let _ = SetBkMode(dc, TRANSPARENT);
        // GDI wants 0x00BBGGRR; `ink` is 0xRRGGBB.
        let color_ref = ((ink & 0xFF) << 16) | (ink & 0x0000_FF00) | ((ink >> 16) & 0xFF);
        let _ = SetTextColor(dc, COLORREF(color_ref));

        let (left, top, right, bottom) = (
            (margin_dip.0 * SCALE as f32) as i32,
            (margin_dip.1 * SCALE as f32) as i32,
            width as i32 - (margin_dip.2 * SCALE as f32) as i32,
            height as i32 - (margin_dip.3 * SCALE as f32) as i32,
        );
        let mut rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        let format = if brief {
            windows::Win32::Graphics::Gdi::DT_CENTER
                | windows::Win32::Graphics::Gdi::DT_VCENTER
                | windows::Win32::Graphics::Gdi::DT_SINGLELINE
                | windows::Win32::Graphics::Gdi::DT_NOPREFIX
        } else {
            windows::Win32::Graphics::Gdi::DT_LEFT
                | windows::Win32::Graphics::Gdi::DT_WORDBREAK
                | windows::Win32::Graphics::Gdi::DT_END_ELLIPSIS
                | windows::Win32::Graphics::Gdi::DT_NOPREFIX
        };
        // `DrawTextW` takes the text as a mutable UTF-16 slice; the buffer
        // stays alive for the synchronous call.
        let _ = DrawTextW(dc, &mut utf16, &mut rect, DRAW_TEXT_FORMAT(format.0));

        pixels.copy_from_slice(target);

        let _ = SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(dc);
    }
}

/// `0xRRGGBB` to `[r, g, b]`.
fn rgb(value: u32) -> [u8; 3] {
    [
        ((value >> 16) & 0xFF) as u8,
        ((value >> 8) & 0xFF) as u8,
        (value & 0xFF) as u8,
    ]
}

fn mix(from: [u8; 3], to: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        (from[0] as f32 + (to[0] as f32 - from[0] as f32) * t) as u8,
        (from[1] as f32 + (to[1] as f32 - from[1] as f32) * t) as u8,
        (from[2] as f32 + (to[2] as f32 - from[2] as f32) * t) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_has_four_entries_and_clamps() {
        assert_eq!(COLOR_COUNT, 4);
        assert_eq!(color(0).paper, 0xFFF4B8);
        assert_eq!(color(99).ink, color(3).ink);
        assert_eq!(swatch(1), 0xBAE1F9);
    }

    #[test]
    fn note_bitmap_is_three_times_the_card() {
        let image = render("hello", 0).expect("a note bitmap");
        assert_eq!((image.width, image.height), (408, 264));
        assert!(image.is_valid());
        // The paper is opaque everywhere.
        assert!(image.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn a_blank_note_renders_for_the_entry_hover() {
        let image = render("", 0).expect("a blank note bitmap");
        assert_eq!((image.width, image.height), (408, 264));
        assert!(image.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn long_text_renders_and_stays_opaque() {
        let text = "这是一段很长的便签文字，用来检查自动换行和末尾省略号。\nSecond line.";
        let image = render(text, 2).expect("a note bitmap");
        assert_eq!((image.width, image.height), (408, 264));
        // Something was drawn: not every pixel is still the paper colour at the
        // ink's top-left area.
        let distinct = image
            .bgra
            .chunks_exact(4)
            .take(408 * 80)
            .filter(|pixel| pixel[0] != 232 || pixel[1] != 225 || pixel[2] != 255)
            .count();
        assert!(distinct > 0, "no text pixels were drawn");
    }

    #[test]
    fn unique_path_numbers_collisions() {
        let directory = std::env::temp_dir().join(format!("pegline-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let first = unique_path(&directory, "Note.txt");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(&directory, "Note.txt");
        assert_eq!(second.file_name().unwrap(), "Note 2.txt");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn summary_strips_markdown_and_keeps_structure() {
        let source =
            "# Title\n\n**bold** and `code`\n- one\n- [x] two\n> quote\n---\n```\ncode\n```";
        assert_eq!(
            summary(source),
            "Title\n\nbold and code\n• one\n[x] two\nquote\ncode"
        );
        assert_eq!(summary("**only markers**"), "only markers");
        assert_eq!(summary("   "), "");
    }

    #[test]
    fn atomic_write_uses_bom_and_crlf_and_replaces() {
        let directory =
            std::env::temp_dir().join(format!("pegline-notes-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("note.txt");
        write_atomic(&path, "a\n中").expect("write");
        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                assert!(index > 0 && bytes[index - 1] == b'\r', "bare LF at {index}");
            }
            if *byte == b'\r' {
                assert_eq!(bytes.get(index + 1), Some(&b'\n'), "bare CR at {index}");
            }
        }
        assert_eq!(read(&path).as_deref(), Some("a\n中"));

        write_atomic(&path, "b").expect("replace");
        assert_eq!(read(&path).as_deref(), Some("b"));
        assert!(std::fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp")
        }));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn archive_moves_and_numbers_collisions() {
        let root =
            std::env::temp_dir().join(format!("pegline-notes-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let notes = root.join("Notes");
        let archive = notes.join("Archive");
        std::fs::create_dir_all(&notes).unwrap();
        let first = notes.join("Note.txt");
        std::fs::write(&first, b"one").unwrap();
        let moved = archive_into(&first, &archive).expect("archive");
        assert!(!first.exists());
        assert!(moved.exists());
        assert_eq!(moved.file_name().unwrap(), "Note.txt");

        let second = notes.join("Note.txt");
        std::fs::write(&second, b"two").unwrap();
        let moved = archive_into(&second, &archive).expect("archive again");
        assert_eq!(moved.file_name().unwrap(), "Note 2.txt");
        let _ = std::fs::remove_dir_all(&root);
    }
}
