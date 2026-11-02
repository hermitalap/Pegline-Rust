//! Clipboard reading and writing through the classic Win32 clipboard API.
//!
//! The capture rules in `docs/BEHAVIOR.md` §9.2 need several formats inspected
//! inside one open, and the copy command publishes three formats at once, which
//! is why this module talks to `OpenClipboard`/`GetClipboardData`/
//! `SetClipboardData` instead of wrapping `arboard`.
//!
//! Reading follows the WPF reference (`archive/wpf-v1/src/Watchers.cs`): an entry
//! that also carries HTML, text, a file list or RTF is a browser or word
//! processor copy rather than a screenshot; the `PNG` format wins over the DIB;
//! and a 32-bit bitmap whose alpha channel is all zero is read as opaque (the
//! Win+Shift+S quirk).
//!
//! Opening the clipboard can wait up to ~30 ms for another application to let go,
//! so the UI thread should hand `read_screenshot` to a worker. No test in this
//! file touches the real clipboard.

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::BI_RGB;
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::DROPFILES;

use crate::types::ImageData;

/// Size of the `BITMAPINFOHEADER` at the start of every `BITMAPCOREHEADER`-era
/// DIB; `BITMAPV4HEADER` and `BITMAPV5HEADER` start with the same fields.
const BITMAPINFOHEADER_LEN: usize = 40;

/// Largest screenshot side accepted from the DIB path, in pixels. A 6K screen is
/// 6016 wide, so anything past this is not a capture we want to materialise.
const MAX_DIB_SIDE: u32 = 20_000;

/// `OpenClipboard` attempts and the pause between them, in place of the WPF
/// app's single 40 ms sleep.
const OPEN_ATTEMPTS: u32 = 4;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// How long after our own write a `WM_CLIPBOARDUPDATE` is ignored, in ms.
const SELF_WRITE_WINDOW_MS: u64 = 1_000;

/// When this process last wrote the clipboard, in ms since the Unix epoch.
static LAST_SELF_WRITE_MS: AtomicU64 = AtomicU64::new(0);

/// Registers `AddClipboardFormatListener` for the hidden message window.
pub fn install_listener(hwnd: HWND) -> Result<(), String> {
    // SAFETY: `hwnd` is the hidden message window owned by this process and stays
    // valid for the lifetime of the process; user32 only records the handle.
    unsafe { AddClipboardFormatListener(hwnd) }
        .map_err(|error| format!("AddClipboardFormatListener: {error}"))
}

/// Reads an image from the clipboard if it is a screenshot-like payload.
///
/// `Ok(None)` means the clipboard holds something that must not be captured
/// (HTML, text, file lists, RTF) or an image format this module does not read;
/// `Err` is a transient clipboard or decode failure worth logging.
pub fn read_screenshot() -> Result<Option<ImageData>, String> {
    let payload = read_payload()?;

    match payload {
        None => Ok(None),
        Some(Payload::Png(bytes)) => {
            let image = crate::capture::wic::decode_bytes(&bytes, u32::MAX)
                .map_err(|error| format!("decode clipboard PNG: {error}"))?;
            if !image.is_valid() {
                return Err(
                    "decode clipboard PNG: decoder returned an invalid BGRA buffer".to_string(),
                );
            }
            Ok(Some(image))
        }
        Some(Payload::Dib(bytes)) => Ok(dib_to_bgra(&bytes)),
    }
}

/// Reads plain text (used by notes in phase B).
pub fn read_text() -> Option<String> {
    open_clipboard(None).ok()?;
    let guard = ClipboardGuard::new();
    let bytes = read_format(u32::from(CF_UNICODETEXT.0)).ok();
    drop(guard);
    let bytes = bytes?;

    let mut units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if let Some(end) = units.iter().position(|unit| *unit == 0) {
        units.truncate(end);
    }
    let text = String::from_utf16_lossy(&units);
    if text.is_empty() { None } else { Some(text) }
}

/// Writes the image in all three formats the WPF app offered: `PNG`, `CF_DIB`
/// and a file drop list. Records the time so the resulting
/// `WM_CLIPBOARDUPDATE` is ignored.
pub fn write_image(
    hwnd: HWND,
    image: &ImageData,
    png: &[u8],
    file: Option<&Path>,
) -> Result<(), String> {
    if png.is_empty() {
        return Err("write_image: empty PNG payload".to_string());
    }
    // Build both payloads before opening the clipboard: the DIB copy is the
    // expensive part and other applications should not wait for it.
    let dib = bgra_to_dib(image)?;
    let drop_list = file.map(hdrop_payload);
    let formats = formats();
    if formats.png == 0 {
        return Err("RegisterClipboardFormatW(PNG) returned 0".to_string());
    }

    open_clipboard(Some(hwnd))?;
    let guard = ClipboardGuard::new();

    // SAFETY: the clipboard is open and owned by this thread; emptying it drops
    // the previous owner's data and makes our window the owner.
    unsafe { EmptyClipboard() }.map_err(|error| format!("EmptyClipboard: {error}"))?;

    set_clipboard_bytes(formats.png, png)?;
    set_clipboard_bytes(u32::from(CF_DIB.0), &dib)?;
    if let Some(drop_list) = drop_list.as_deref() {
        set_clipboard_bytes(u32::from(CF_HDROP.0), drop_list)?;
    }

    guard.close()?;
    mark_self_write();
    Ok(())
}

/// Marks "we just wrote the clipboard"; suppresses the notification for 1 s.
pub fn mark_self_write() {
    LAST_SELF_WRITE_MS.store(now_millis(), Ordering::Relaxed);
}

/// Whether our own clipboard write is still inside the 1 s suppression window.
pub fn self_write_recent() -> bool {
    let marked = LAST_SELF_WRITE_MS.load(Ordering::Relaxed);
    if marked == 0 {
        return false;
    }
    now_millis().saturating_sub(marked) < SELF_WRITE_WINDOW_MS
}

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
fn now_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as u64,
        Err(_) => 0,
    }
}

/// Session-wide ids of the string clipboard formats this module cares about.
///
/// `RegisterClipboardFormatW` returns the same id for a name for the life of the
/// process, so the three names are registered once.
#[derive(Clone, Copy)]
struct Formats {
    png: u32,
    html: u32,
    rtf: u32,
}

impl Formats {
    fn load() -> Self {
        // SAFETY: the names are NUL-terminated UTF-16 literals in this binary;
        // user32 only reads them and returns the session format ids.
        unsafe {
            Self {
                png: RegisterClipboardFormatW(windows::core::w!("PNG")),
                html: RegisterClipboardFormatW(windows::core::w!("HTML Format")),
                rtf: RegisterClipboardFormatW(windows::core::w!("Rich Text Format")),
            }
        }
    }
}

fn formats() -> Formats {
    static CACHE: OnceLock<Formats> = OnceLock::new();
    *CACHE.get_or_init(Formats::load)
}

/// The raw image data copied out of the clipboard while it was open.
enum Payload {
    Png(Vec<u8>),
    Dib(Vec<u8>),
}

/// Copies the raw image payload out of the clipboard and closes it again.
fn read_payload() -> Result<Option<Payload>, String> {
    open_clipboard(None)?;
    let guard = ClipboardGuard::new();
    let payload = read_payload_while_open();
    // Closing before the caller decodes keeps the lock short: a 6K PNG is ~40 ms
    // of work and other applications want the clipboard too.
    drop(guard);
    payload
}

/// Reads the payload from the clipboard this thread has open.
fn read_payload_while_open() -> Result<Option<Payload>, String> {
    let formats = formats();

    // A picture copied in a browser or a document comes with HTML, text, RTF or a
    // file list; only a bare image is a screenshot. Enumerating the whole list is
    // cheap and also tells us which image formats are available.
    let mut has_png = false;
    let mut has_dib = false;
    let mut has_dibv5 = false;
    let mut current = 0u32;
    loop {
        // SAFETY: EnumClipboardFormats is a read-only query on the clipboard this
        // thread has open; it returns 0 at the end of the list.
        current = unsafe { EnumClipboardFormats(current) };
        if current == 0 {
            break;
        }
        if current == formats.html
            || current == u32::from(CF_UNICODETEXT.0)
            || current == u32::from(CF_HDROP.0)
            || current == formats.rtf
        {
            return Ok(None);
        }
        if current == formats.png {
            has_png = true;
        } else if current == u32::from(CF_DIB.0) {
            has_dib = true;
        } else if current == u32::from(CF_DIBV5.0) {
            has_dibv5 = true;
        }
    }

    // A failed read of one format still leaves the next one worth trying; the
    // error is only reported when no format yields data.
    let mut last_error: Option<String> = None;

    // `PNG` first: it keeps the original pixels and is what the Snipping Tool
    // writes next to the DIB.
    if has_png {
        match read_format(formats.png) {
            Ok(bytes) if !bytes.is_empty() => return Ok(Some(Payload::Png(bytes))),
            Ok(_) => {}
            Err(error) => last_error = Some(error),
        }
    }

    // Otherwise the classic bitmap. `CF_DIB` is what the WPF app read through
    // `Clipboard.GetImage`; `CF_DIBV5` is only the fallback when it is absent.
    for (present, clipboard_format) in [
        (has_dib, u32::from(CF_DIB.0)),
        (has_dibv5, u32::from(CF_DIBV5.0)),
    ] {
        if !present {
            continue;
        }
        match read_format(clipboard_format) {
            Ok(bytes) if !bytes.is_empty() => return Ok(Some(Payload::Dib(bytes))),
            Ok(_) => {}
            Err(error) => last_error = Some(error),
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// Copies one clipboard format into a `Vec`. Only valid while the clipboard is
/// open on this thread.
fn read_format(format: u32) -> Result<Vec<u8>, String> {
    // SAFETY: the clipboard is open on this thread and `format` was listed by
    // EnumClipboardFormats; the returned handle is owned by the clipboard.
    let handle = unsafe { GetClipboardData(format) }
        .map_err(|error| format!("GetClipboardData({format}): {error}"))?;
    read_hglobal(handle)
}

/// Copies a clipboard `HGLOBAL` into a `Vec`.
fn read_hglobal(handle: HANDLE) -> Result<Vec<u8>, String> {
    let block = HGLOBAL(handle.0);
    // SAFETY: `handle` came from GetClipboardData while the clipboard is open, so
    // it is a valid HGLOBAL; GlobalSize only reads the block header.
    let size = unsafe { GlobalSize(block) };
    if size == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: as above; for a GMEM_MOVEABLE block GlobalLock returns a readable
    // buffer of `size` bytes or null on failure.
    let locked = unsafe { GlobalLock(block) };
    if locked.is_null() {
        return Err(format!("GlobalLock({size} bytes): returned null"));
    }
    // SAFETY: non-null and `size` bytes long by the contract of GlobalLock; the
    // copy is finished before the matching unlock below.
    let bytes = unsafe { std::slice::from_raw_parts(locked as *const u8, size) }.to_vec();
    // SAFETY: releases the lock taken above. Win32 reports the final unlock as
    // FALSE with no error, which the wrapper cannot express, so the result is
    // ignored.
    let _ = unsafe { GlobalUnlock(block) };
    Ok(bytes)
}

/// Opens the clipboard, retrying because another application may hold it.
///
/// A successful open must be paired with a [`ClipboardGuard`] on this thread.
fn open_clipboard(owner: Option<HWND>) -> Result<(), String> {
    let mut last = String::from("OpenClipboard: no attempt made");
    for attempt in 0..OPEN_ATTEMPTS {
        // SAFETY: opening the clipboard is a global operation; a successful open
        // is always paired with ClipboardGuard on this thread.
        match unsafe { OpenClipboard(owner) } {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = format!("OpenClipboard (attempt {}): {error}", attempt + 1);
                if attempt + 1 < OPEN_ATTEMPTS {
                    std::thread::sleep(OPEN_RETRY_DELAY);
                }
            }
        }
    }
    Err(last)
}

/// Owns an open clipboard and closes it on drop, including along error paths.
struct ClipboardGuard {
    open: bool,
}

impl ClipboardGuard {
    fn new() -> Self {
        Self { open: true }
    }

    /// Closes the clipboard now, reporting a failure to do so.
    fn close(mut self) -> Result<(), String> {
        // SAFETY: the guard is only constructed after OpenClipboard succeeded on
        // this thread, and it closes at most once on success because `open` is
        // cleared below.
        let result = unsafe { CloseClipboard() };
        let result = result.map_err(|error| format!("CloseClipboard: {error}"));
        if result.is_ok() {
            self.open = false;
        }
        result
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        // SAFETY: the clipboard is still open on this thread; nobody can act on a
        // failure here, so the result is dropped.
        let _ = unsafe { CloseClipboard() };
    }
}

/// A `GMEM_MOVEABLE` block that frees itself unless ownership is handed to the
/// clipboard with [`GlobalBlock::into_handle`].
struct GlobalBlock(HGLOBAL);

impl GlobalBlock {
    fn new(len: usize) -> Result<Self, String> {
        if len == 0 {
            return Err("GlobalAlloc: refusing a zero-length block".to_string());
        }
        // SAFETY: allocates `len` movable bytes; no pointer is involved.
        unsafe { GlobalAlloc(GMEM_MOVEABLE, len) }
            .map(Self)
            .map_err(|error| format!("GlobalAlloc({len} bytes): {error}"))
    }

    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        // SAFETY: `self.0` is a live GMEM_MOVEABLE block of at least
        // `bytes.len()` bytes, allocated by `GlobalBlock::new`.
        let target = unsafe { GlobalLock(self.0) };
        if target.is_null() {
            return Err("GlobalLock: returned null".to_string());
        }
        // SAFETY: the lock returned a writable buffer of GlobalSize(self.0)
        // bytes, which covers `bytes`, and nobody else can see the block yet.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target as *mut u8, bytes.len());
        }
        // SAFETY: releases the lock taken above; the block itself stays alive.
        let _ = unsafe { GlobalUnlock(self.0) };
        Ok(())
    }

    fn into_handle(self) -> HGLOBAL {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for GlobalBlock {
    fn drop(&mut self) {
        // SAFETY: the block was allocated by `GlobalBlock::new` and has not been
        // handed out, so it is ours to free. Win32 reports success as NULL, which
        // the wrapper turns into an error, hence the ignored result.
        let _ = unsafe { GlobalFree(Some(self.0)) };
    }
}

/// Hands a fresh `GMEM_MOVEABLE` block holding `bytes` to the clipboard.
fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
    let block = GlobalBlock::new(bytes.len())?;
    block.write(bytes)?;
    let handle = block.into_handle();
    // SAFETY: the clipboard is open and owned by this thread; `handle` is a live
    // movable block. On success the system takes ownership of it, which is why
    // the block was forgotten above and is only freed on the failure path.
    match unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) } {
        Ok(_) => Ok(()),
        Err(error) => {
            // SAFETY: the call failed, so ownership stayed with us; Win32 reports
            // success as NULL, hence the ignored result.
            let _ = unsafe { GlobalFree(Some(handle)) };
            Err(format!("SetClipboardData({format}): {error}"))
        }
    }
}

/// Bytes per DIB row, padded to a 4-byte boundary. `None` for impossible inputs.
fn dib_row_stride(width: u32, bit_count: u16) -> Option<usize> {
    if width == 0 || bit_count == 0 {
        return None;
    }
    let bits = u64::from(width).checked_mul(u64::from(bit_count))?;
    let bytes = bits.div_ceil(8);
    let stride = bytes.next_multiple_of(4);
    usize::try_from(stride).ok()
}

/// Total bytes of a `CF_DIB` payload for a plain bitmap: a 40-byte
/// `BITMAPINFOHEADER` plus 4-byte-aligned rows.
fn dib_len(width: u32, height: u32, bit_count: u16) -> Option<usize> {
    let stride = dib_row_stride(width, bit_count)?;
    let pixels = stride.checked_mul(usize::try_from(height).ok()?)?;
    pixels.checked_add(BITMAPINFOHEADER_LEN)
}

/// Builds the `CF_DIB` payload for an image: a `BITMAPINFOHEADER` followed by
/// bottom-up 32-bit BGRA rows, with the alpha channel written.
fn bgra_to_dib(image: &ImageData) -> Result<Vec<u8>, String> {
    if !image.is_valid() {
        return Err("CF_DIB: image is not valid BGRA".to_string());
    }
    if image.width > i32::MAX as u32 || image.height > i32::MAX as u32 {
        return Err("CF_DIB: image is too large for a BITMAPINFOHEADER".to_string());
    }
    let stride = dib_row_stride(image.width, 32)
        .ok_or_else(|| "CF_DIB: row stride overflowed".to_string())?;
    let len = dib_len(image.width, image.height, 32)
        .ok_or_else(|| "CF_DIB: payload size overflowed".to_string())?;
    let size_image = u32::try_from(len - BITMAPINFOHEADER_LEN)
        .map_err(|_| "CF_DIB: image is too large for a BITMAPINFOHEADER".to_string())?;

    let mut dib = vec![0u8; len];
    dib[0..4].copy_from_slice(&(BITMAPINFOHEADER_LEN as u32).to_le_bytes());
    dib[4..8].copy_from_slice(&(image.width as i32).to_le_bytes());
    // Positive height: the rows below are stored bottom-up.
    dib[8..12].copy_from_slice(&(image.height as i32).to_le_bytes());
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    // biCompression = BI_RGB; the pels-per-metre and palette fields stay zero.
    dib[20..24].copy_from_slice(&size_image.to_le_bytes());

    let height = image.height as usize;
    for row in 0..height {
        let source = (height - 1 - row) * stride;
        let target = BITMAPINFOHEADER_LEN + row * stride;
        dib[target..target + stride].copy_from_slice(&image.bgra[source..source + stride]);
    }
    Ok(dib)
}

/// Normalised `BITMAPINFOHEADER` fields of a `CF_DIB`/`CF_DIBV5` payload.
struct DibInfo {
    width: u32,
    height: u32,
    top_down: bool,
    bit_count: u16,
    /// Byte offset of the first pixel row.
    pixel_offset: usize,
}

/// Parses the header of a `CF_DIB`/`CF_DIBV5` payload.
///
/// `None` for anything that is not a plain RGB bitmap this module can convert:
/// compressed, paletted or otherwise unusual bitmaps, absurd sizes, or a payload
/// shorter than its own header claims.
fn parse_dib(dib: &[u8]) -> Option<DibInfo> {
    let header_len = usize::try_from(read_u32(dib, 0)?).ok()?;
    if header_len < BITMAPINFOHEADER_LEN || header_len > dib.len() {
        return None;
    }
    let width = read_i32(dib, 4)?;
    let raw_height = read_i32(dib, 8)?;
    let planes = read_u16(dib, 12)?;
    let bit_count = read_u16(dib, 14)?;
    let compression = read_u32(dib, 16)?;
    let palette_entries = read_u32(dib, 32)?;

    if planes != 1 || compression != BI_RGB.0 {
        return None;
    }
    if !matches!(bit_count, 24 | 32) {
        return None;
    }
    if width <= 0 || raw_height == 0 || raw_height == i32::MIN {
        return None;
    }

    let width = u32::try_from(width).ok()?;
    let height = raw_height.unsigned_abs();
    if width > MAX_DIB_SIDE || height > MAX_DIB_SIDE {
        return None;
    }

    let palette_bytes = usize::try_from(palette_entries).ok()?.checked_mul(4)?;
    let pixel_offset = header_len.checked_add(palette_bytes)?;
    let stride = dib_row_stride(width, bit_count)?;
    let pixel_bytes = stride.checked_mul(usize::try_from(height).ok()?)?;
    if pixel_offset.checked_add(pixel_bytes)? > dib.len() {
        return None;
    }

    Some(DibInfo {
        width,
        height,
        top_down: raw_height < 0,
        bit_count,
        pixel_offset,
    })
}

/// Converts a clipboard `CF_DIB`/`CF_DIBV5` payload into premultiplied BGRA.
///
/// Returns `None` when the payload is not a plain 24- or 32-bit RGB bitmap.
fn dib_to_bgra(dib: &[u8]) -> Option<ImageData> {
    let info = parse_dib(dib)?;
    let width = info.width as usize;
    let height = info.height as usize;
    let stride = dib_row_stride(info.width, info.bit_count)?;
    let byte_len = width.checked_mul(height)?.checked_mul(4)?;

    let mut bgra = Vec::new();
    bgra.try_reserve_exact(byte_len).ok()?;
    bgra.resize(byte_len, 0);

    // A 32-bit DIB copied by the Snipping Tool can carry an all-zero alpha
    // channel; read as transparent it would hang an invisible card, so it is
    // treated as opaque exactly like the WPF app did.
    let opaque = info.bit_count == 32 && !dib_has_alpha(dib, &info, stride);

    for y in 0..height {
        let source_row = if info.top_down { y } else { height - 1 - y };
        let source = info.pixel_offset + source_row * stride;
        let target = y * width * 4;
        if info.bit_count == 32 {
            for x in 0..width {
                let src = source + x * 4;
                let dst = target + x * 4;
                let alpha = if opaque { 255 } else { dib[src + 3] };
                if alpha == 0 {
                    // Premultiplied: a transparent pixel carries no colour.
                    continue;
                }
                if alpha == 255 {
                    // Opaque pixels are the common case; skip the multiplications.
                    bgra[dst] = dib[src];
                    bgra[dst + 1] = dib[src + 1];
                    bgra[dst + 2] = dib[src + 2];
                    bgra[dst + 3] = 255;
                    continue;
                }
                bgra[dst] = premultiply(dib[src], alpha);
                bgra[dst + 1] = premultiply(dib[src + 1], alpha);
                bgra[dst + 2] = premultiply(dib[src + 2], alpha);
                bgra[dst + 3] = alpha;
            }
        } else {
            for x in 0..width {
                let src = source + x * 3;
                let dst = target + x * 4;
                bgra[dst] = dib[src];
                bgra[dst + 1] = dib[src + 1];
                bgra[dst + 2] = dib[src + 2];
                bgra[dst + 3] = 255;
            }
        }
    }

    Some(ImageData::new(info.width, info.height, bgra))
}

/// Whether any pixel of a 32-bit DIB has a non-zero alpha byte.
///
/// Stops at the first opaque pixel: an ordinary screenshot has one in its first
/// row, so the common case never pays for a second full pass.
fn dib_has_alpha(dib: &[u8], info: &DibInfo, stride: usize) -> bool {
    for y in 0..info.height as usize {
        let row = info.pixel_offset + y * stride;
        for x in 0..info.width as usize {
            if dib[row + x * 4 + 3] != 0 {
                return true;
            }
        }
    }
    false
}

/// `channel * alpha / 255`, rounded to nearest.
fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((u32::from(channel) * u32::from(alpha) + 127) / 255) as u8
}

/// Builds the `CF_HDROP` payload: a `DROPFILES` header with `fWide = TRUE`, the
/// UTF-16 path, and a double NUL terminator.
fn hdrop_payload(file: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    let units: Vec<u16> = file.as_os_str().encode_wide().collect();
    let header_len = std::mem::size_of::<DROPFILES>();
    let mut bytes = vec![0u8; header_len + (units.len() + 2) * 2];

    let files_offset = header_len as u32;
    let offset = std::mem::offset_of!(DROPFILES, pFiles);
    bytes[offset..offset + 4].copy_from_slice(&files_offset.to_le_bytes());
    let offset = std::mem::offset_of!(DROPFILES, fWide);
    bytes[offset..offset + 4].copy_from_slice(&1i32.to_le_bytes());
    for (index, unit) in units.iter().enumerate() {
        let offset = header_len + index * 2;
        bytes[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    // The tail (the path's NUL plus the list's second NUL) is already zero.
    bytes
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset.checked_add(2)?)?;
    let array: [u8; 2] = raw.try_into().ok()?;
    Some(u16::from_le_bytes(array))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset.checked_add(4)?)?;
    let array: [u8; 4] = raw.try_into().ok()?;
    Some(u32::from_le_bytes(array))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    let raw = bytes.get(offset..offset.checked_add(4)?)?;
    let array: [u8; 4] = raw.try_into().ok()?;
    Some(i32::from_le_bytes(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `BITMAPINFOHEADER` plus pixel bytes for the DIB tests.
    fn make_dib(
        width: i32,
        height: i32,
        bit_count: u16,
        compression: u32,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut dib = vec![0u8; BITMAPINFOHEADER_LEN];
        dib[0..4].copy_from_slice(&(BITMAPINFOHEADER_LEN as u32).to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&bit_count.to_le_bytes());
        dib[16..20].copy_from_slice(&compression.to_le_bytes());
        dib.extend_from_slice(pixels);
        dib
    }

    #[test]
    fn dib_row_stride_pads_to_four_bytes() {
        assert_eq!(dib_row_stride(1, 24), Some(4));
        assert_eq!(dib_row_stride(2, 24), Some(8));
        assert_eq!(dib_row_stride(3, 24), Some(12));
        assert_eq!(dib_row_stride(4, 24), Some(12));
        assert_eq!(dib_row_stride(1, 32), Some(4));
        assert_eq!(dib_row_stride(1, 8), Some(4));
        assert_eq!(dib_row_stride(0, 24), None);
        assert_eq!(dib_row_stride(1, 0), None);
    }

    #[test]
    fn dib_len_adds_the_header() {
        assert_eq!(dib_len(3, 2, 24), Some(BITMAPINFOHEADER_LEN + 2 * 12));
        assert_eq!(dib_len(2, 2, 32), Some(BITMAPINFOHEADER_LEN + 2 * 8));
        assert_eq!(dib_len(0, 2, 32), None);
    }

    #[test]
    fn bgra_to_dib_writes_bottom_up_rows_and_header() {
        // Top row blue/green, bottom row red/white.
        let image = ImageData::new(
            2,
            2,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, // top row
                0, 0, 255, 255, 255, 255, 255, 255, // bottom row
            ],
        );
        let dib = bgra_to_dib(&image).expect("dib");
        assert_eq!(dib.len(), BITMAPINFOHEADER_LEN + 16);
        assert_eq!(read_u32(&dib, 0), Some(BITMAPINFOHEADER_LEN as u32));
        assert_eq!(read_i32(&dib, 4), Some(2));
        assert_eq!(read_i32(&dib, 8), Some(2));
        assert_eq!(read_u16(&dib, 12), Some(1));
        assert_eq!(read_u16(&dib, 14), Some(32));
        assert_eq!(read_u32(&dib, 16), Some(0));
        assert_eq!(read_u32(&dib, 20), Some(16));
        // Row 0 of the payload is the bottom row of the image.
        assert_eq!(
            &dib[BITMAPINFOHEADER_LEN..BITMAPINFOHEADER_LEN + 8],
            &[0, 0, 255, 255, 255, 255, 255, 255]
        );
        assert_eq!(
            &dib[BITMAPINFOHEADER_LEN + 8..],
            &[255, 0, 0, 255, 0, 255, 0, 255]
        );
    }

    #[test]
    fn bgra_to_dib_rejects_invalid_buffers() {
        assert!(bgra_to_dib(&ImageData::new(2, 2, vec![0; 15])).is_err());
    }

    #[test]
    fn dib_to_bgra_reads_padded_24_bit_bottom_up_rows() {
        // 2x2, stored bottom-up, with two bytes of padding per 6-byte row.
        let pixels = [
            1, 2, 3, 4, 5, 6, 0, 0, // bottom row (image row 1) plus padding
            7, 8, 9, 10, 11, 12, 0, 0, // top row (image row 0) plus padding
        ];
        let dib = make_dib(2, 2, 24, BI_RGB.0, &pixels);
        let image = dib_to_bgra(&dib).expect("image");
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(&image.bgra[0..4], &[7, 8, 9, 255]);
        assert_eq!(&image.bgra[4..8], &[10, 11, 12, 255]);
        assert_eq!(&image.bgra[8..12], &[1, 2, 3, 255]);
        assert_eq!(&image.bgra[12..16], &[4, 5, 6, 255]);
    }

    #[test]
    fn dib_to_bgra_reads_top_down_32_bit_rows() {
        let pixels = [10, 20, 30, 255, 40, 50, 60, 255];
        let dib = make_dib(2, -1, 32, BI_RGB.0, &pixels);
        let image = dib_to_bgra(&dib).expect("image");
        assert_eq!(&image.bgra, &[10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn dib_to_bgra_treats_zero_alpha_as_opaque() {
        let dib = make_dib(1, 1, 32, BI_RGB.0, &[1, 2, 3, 0]);
        let image = dib_to_bgra(&dib).expect("image");
        assert_eq!(&image.bgra, &[1, 2, 3, 255]);
    }

    #[test]
    fn dib_to_bgra_premultiplies_real_alpha() {
        let dib = make_dib(1, 1, 32, BI_RGB.0, &[100, 150, 200, 128]);
        let image = dib_to_bgra(&dib).expect("image");
        assert_eq!(&image.bgra, &[50, 75, 100, 128]);
    }

    #[test]
    fn dib_to_bgra_rejects_compressed_paletted_or_oversized_bitmaps() {
        // BI_BITFIELDS is not a plain RGB DIB.
        let compressed = make_dib(1, 1, 32, 3, &[0; 4]);
        assert!(dib_to_bgra(&compressed).is_none());
        let paletted = make_dib(1, 1, 8, BI_RGB.0, &[0; 4]);
        assert!(dib_to_bgra(&paletted).is_none());
        let oversized = make_dib(MAX_DIB_SIDE as i32 + 1, 1, 32, BI_RGB.0, &[]);
        assert!(dib_to_bgra(&oversized).is_none());
    }

    #[test]
    fn dib_to_bgra_rejects_truncated_pixels() {
        let dib = make_dib(2, 2, 32, BI_RGB.0, &[0; 4]);
        assert!(dib_to_bgra(&dib).is_none());
    }

    #[test]
    fn dib_to_bgra_honours_the_palette_offset() {
        // One palette entry (4 bytes) sits between the header and the pixels, so
        // `biClrUsed` moves the pixel data from offset 40 to offset 44.
        let mut dib = make_dib(1, 1, 24, BI_RGB.0, &[0xaa, 0xbb, 0xcc, 0xdd, 9, 8, 7, 0]);
        dib[32..36].copy_from_slice(&1u32.to_le_bytes());
        let image = dib_to_bgra(&dib).expect("image");
        assert_eq!(&image.bgra, &[9, 8, 7, 255]);
    }

    #[test]
    fn hdrop_payload_is_a_wide_dropfiles_list() {
        use std::os::windows::ffi::OsStrExt;

        let path = Path::new(r"C:\shot.png");
        let payload = hdrop_payload(path);
        let header_len = std::mem::size_of::<DROPFILES>();
        assert_eq!(read_u32(&payload, 0), Some(header_len as u32));
        let wide = std::mem::offset_of!(DROPFILES, fWide);
        assert_eq!(read_u32(&payload, wide), Some(1));

        let units: Vec<u16> = payload[header_len..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let mut expected: Vec<u16> = path.as_os_str().encode_wide().collect();
        expected.push(0);
        expected.push(0);
        assert_eq!(units, expected);
    }

    #[test]
    fn self_write_is_recent_after_marking() {
        mark_self_write();
        assert!(self_write_recent());
    }
}
