//! The DIB↔BGRA codec used by the clipboard reader and writer.
//!
//! `clipboard.rs` moves bytes; this module only turns a `CF_DIB`/`CF_DIBV5`
//! payload into premultiplied BGRA and an [`ImageData`] back into a
//! `BITMAPINFOHEADER` payload. Apart from `ImageData` and the `BI_RGB` constant
//! the work is plain arithmetic, so nothing here opens the clipboard, touches a
//! window, or allocates past the buffers it returns.

use windows::Win32::Graphics::Gdi::BI_RGB;

use crate::types::ImageData;

/// Size of the `BITMAPINFOHEADER` at the start of every `BITMAPCOREHEADER`-era
/// DIB; `BITMAPV4HEADER` and `BITMAPV5HEADER` start with the same fields.
const BITMAPINFOHEADER_LEN: usize = 40;

/// Largest screenshot side accepted from the DIB path, in pixels. A 6K screen is
/// 6016 wide, so anything past this is not a capture we want to materialise.
const MAX_DIB_SIDE: u32 = 20_000;

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
pub(crate) fn bgra_to_dib(image: &ImageData) -> Result<Vec<u8>, String> {
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
pub(crate) fn dib_to_bgra(dib: &[u8]) -> Option<ImageData> {
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

// Little-endian header readers used by the header parser. `read_u32` is the one
// also used from outside this module: the clipboard module's `CF_HDROP` test
// reads the header it wrote.

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset.checked_add(2)?)?;
    let array: [u8; 2] = raw.try_into().ok()?;
    Some(u16::from_le_bytes(array))
}

pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
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
}
