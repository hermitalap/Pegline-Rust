//! WIC decoding and PNG encoding, off the UI thread.
//!
//! The decoder is asked for the final size directly (`IWICBitmapScaler`) so a 6K
//! screenshot never produces a full-size CPU copy on its way to the GPU.
//!
//! Every entry point creates its own `IWICImagingFactory`; these calls run on the
//! decoder workers and on the clipboard worker, never on the UI thread. The
//! calling thread must have initialized COM: a `CO_E_NOTINITIALIZED` failure is
//! reported to the caller instead of being hidden behind a local `CoInitializeEx`
//! (the apartment belongs to the thread's owner, not to this module).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CO_E_NOTINITIALIZED, GENERIC_READ, GENERIC_WRITE, HGLOBAL};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, CLSID_WICImagingFactory2, GUID_ContainerFormatPng,
    GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppPBGRA, IWICBitmapDecoder,
    IWICBitmapFrameDecode, IWICBitmapSource, IWICImagingFactory, IWICPalette,
    WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand, WICRect,
};
use windows::Win32::System::Com::StructuredStorage::{
    CreateStreamOnHGlobal, GetHGlobalFromStream, IPropertyBag2,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, IStream, STATFLAG_NONAME, STATSTG,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::{GUID, IUnknown, PCWSTR};

use crate::types::ImageData;

/// Pixel size of an image without decoding its pixels.
pub fn pixel_size(path: &Path) -> Result<(u32, u32), String> {
    let factory = imaging_factory()?;
    let decoder = decoder_from_filename(&factory, path)?;
    let frame = first_frame(&decoder)?;
    let (width, height) = source_size(&frame)?;
    if width == 0 || height == 0 {
        return Err(format!(
            "WIC frame is empty ({width}x{height}): {}",
            path.display()
        ));
    }
    Ok((width, height))
}

/// Decodes an image so that its longest side is at most `max_side`, preserving
/// aspect. Never upscales. Output is premultiplied BGRA, top-down.
pub fn decode_scaled(path: &Path, max_side: u32) -> Result<ImageData, String> {
    let factory = imaging_factory()?;
    let decoder = decoder_from_filename(&factory, path)?;
    let frame = first_frame(&decoder)?;
    decode_frame(&factory, &frame, max_side).map_err(|error| format!("{}: {error}", path.display()))
}

/// Decodes at the source resolution.
pub fn decode_full(path: &Path) -> Result<ImageData, String> {
    decode_scaled(path, u32::MAX)
}

/// Decodes an encoded image held in memory (the clipboard `PNG` format).
///
/// The slice is borrowed for the whole decode: the WIC stream reads it in place
/// instead of copying it, so it must outlive the stream, the decoder and the
/// frame produced here.
pub fn decode_bytes(bytes: &[u8], max_side: u32) -> Result<ImageData, String> {
    let factory = imaging_factory()?;
    // SAFETY: the factory is a live WIC factory; no memory is copied and
    // `bytes` stays borrowed until this function returns, after every WIC
    // object that refers to it has been dropped.
    let stream =
        unsafe { factory.CreateStream() }.map_err(|error| format!("WIC CreateStream: {error}"))?;
    // SAFETY: `stream` is a live WIC stream and `bytes` outlives it; the
    // buffer is only read while the stream is alive.
    unsafe { stream.InitializeFromMemory(bytes) }
        .map_err(|error| format!("WIC InitializeFromMemory({} bytes): {error}", bytes.len()))?;
    // SAFETY: `stream` is initialized from the live memory above and stays
    // alive until after the frame has been copied; vendor selection is unused.
    let decoder = unsafe {
        factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
    }
    .map_err(|error| format!("WIC CreateDecoderFromStream: {error}"))?;
    let frame = first_frame(&decoder)?;
    decode_frame(&factory, &frame, max_side)
}

/// Encodes premultiplied BGRA to a PNG file, writing a temporary file first.
pub fn encode_png(path: &Path, image: &ImageData) -> Result<(), String> {
    let temp_path = temp_path_for(path);
    if let Err(error) = encode_png_file(&temp_path, image) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    // The WIC stream has been dropped, so the temporary file is closed. Rename
    // replaces the target atomically, so the watcher never sees a partial PNG.
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "std::fs::rename({} -> {}): {error}",
            temp_path.display(),
            path.display()
        )
    })
}

/// Encodes to a PNG byte stream (for the `PNG` clipboard format).
pub fn encode_png_bytes(image: &ImageData) -> Result<Vec<u8>, String> {
    let factory = imaging_factory()?;
    // SAFETY: a null `HGLOBAL` asks COM to allocate the memory itself and
    // `true` makes the stream free it when the last reference is released.
    let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }
        .map_err(|error| format!("CreateStreamOnHGlobal: {error}"))?;
    write_png(&factory, &stream, image)?;

    // SAFETY: `stream` is a live stream created by `CreateStreamOnHGlobal`, so
    // it has exactly one memory handle; the returned handle is owned by the
    // stream and remains valid until it is dropped below.
    let hglobal = unsafe { GetHGlobalFromStream(&stream) }
        .map_err(|error| format!("GetHGlobalFromStream: {error}"))?;
    if hglobal.is_invalid() {
        return Err("GetHGlobalFromStream returned an invalid handle".to_string());
    }
    let mut stat = STATSTG::default();
    // SAFETY: `stat` is a live local and `STATFLAG_NONAME` asks WIC not to
    // allocate a name string into it.
    unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }
        .map_err(|error| format!("IStream::Stat: {error}"))?;
    let len = usize::try_from(stat.cbSize)
        .map_err(|_| format!("PNG stream is larger than usize ({} bytes)", stat.cbSize))?;
    if len == 0 {
        return Err("PNG encoder produced an empty stream".to_string());
    }
    // SAFETY: `hglobal` is the handle behind the live stream above.
    let capacity = unsafe { GlobalSize(hglobal) };
    if len > capacity {
        return Err(format!(
            "PNG stream length {len} exceeds its {capacity}-byte allocation"
        ));
    }
    // SAFETY: `hglobal` is a live global memory block owned by the stream.
    let pointer = unsafe { GlobalLock(hglobal) };
    if pointer.is_null() {
        return Err("GlobalLock returned a null pointer for the PNG stream".to_string());
    }
    let mut bytes = vec![0u8; len];
    // SAFETY: `pointer` points to at least `capacity` initialized bytes,
    // `len <= capacity`, and the freshly allocated `bytes` cannot overlap it.
    unsafe { std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), bytes.as_mut_ptr(), len) };
    // The final unlock returns a zero BOOL, which is success; the windows
    // wrapper maps that to Err, so the result is deliberately ignored.
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(bytes)
}

/// True when two files have (nearly) the same pixels.
pub fn same_picture(a: &Path, b: &Path) -> Result<bool, String> {
    let left = decode_scaled(a, super::COMPARE_MAX)?;
    let right = decode_scaled(b, super::COMPARE_MAX)?;
    if left.width == 0 || left.height == 0 || right.width == 0 || right.height == 0 {
        return Ok(false);
    }
    if (left.width, left.height) != (right.width, right.height) {
        return Ok(false);
    }
    if left.bgra.len() != right.bgra.len() {
        return Ok(false);
    }
    let total = u64::from(left.width) * u64::from(left.height);
    if total == 0 {
        return Ok(false);
    }
    let mut differing = 0u64;
    for (left_pixel, right_pixel) in left.bgra.chunks_exact(4).zip(right.bgra.chunks_exact(4)) {
        if left_pixel
            .iter()
            .zip(right_pixel)
            .any(|(left_channel, right_channel)| left_channel.abs_diff(*right_channel) > 8)
        {
            differing += 1;
            // Stop as soon as the picture is known to be different.
            if differing * 200 > total {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Extensions the watcher accepts, lowercase, without the dot.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif", "tif", "tiff"];

/// Whether a path looks like an image the watcher should pick up.
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
        .unwrap_or(false)
}

/// Fits `width x height` inside a `max_side` square, preserving aspect.
///
/// Integer arithmetic, never upscales, and every side is at least one pixel.
/// A zero-sized input is returned unchanged; callers reject empty frames before
/// asking for a scaled size.
fn fit(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let longest = width.max(height);
    let limit = max_side.max(1);
    if longest <= limit {
        return (width, height);
    }
    (
        scale_side(width, longest, limit),
        scale_side(height, longest, limit),
    )
}

/// Scales one side of an aspect-preserving fit and clamps it to `1..=side`.
fn scale_side(side: u32, longest: u32, limit: u32) -> u32 {
    // u64 keeps the intermediate product of two u32 values from overflowing.
    let scaled = (u64::from(side) * u64::from(limit) + u64::from(longest) / 2) / u64::from(longest);
    scaled.clamp(1, u64::from(side)) as u32
}

/// Creates a WIC factory, preferring the Windows 8+ factory when available.
fn imaging_factory() -> Result<IWICImagingFactory, String> {
    match create_factory(&CLSID_WICImagingFactory2) {
        Ok(factory) => Ok(factory),
        Err(error) if error.code() == CO_E_NOTINITIALIZED => Err(format!(
            "CoCreateInstance(CLSID_WICImagingFactory2): COM is not initialized on this thread: {error}"
        )),
        Err(first_error) => create_factory(&CLSID_WICImagingFactory).map_err(|second_error| {
            format!(
                "CoCreateInstance(CLSID_WICImagingFactory2): {first_error}; \
                 CoCreateInstance(CLSID_WICImagingFactory): {second_error}"
            )
        }),
    }
}

fn create_factory(clsid: &GUID) -> windows::core::Result<IWICImagingFactory> {
    // SAFETY: `clsid` points to one of the two static WIC factory class ids,
    // COM is expected to be initialized by the calling worker thread, and no
    // aggregation is requested.
    unsafe { CoCreateInstance(clsid, None::<&IUnknown>, CLSCTX_INPROC_SERVER) }
}

fn decoder_from_filename(
    factory: &IWICImagingFactory,
    path: &Path,
) -> Result<IWICBitmapDecoder, String> {
    let wide = wide_path(path);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that stays alive for the
    // call, and `factory` is a live WIC factory.
    unsafe {
        factory
            .CreateDecoderFromFilename(
                PCWSTR(wide.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .map_err(|error| format!("WIC CreateDecoderFromFilename({}): {error}", path.display()))
    }
}

fn first_frame(decoder: &IWICBitmapDecoder) -> Result<IWICBitmapFrameDecode, String> {
    // SAFETY: `decoder` is a live WIC decoder; WIC returns an error cleanly
    // when the container has no first frame.
    unsafe { decoder.GetFrame(0) }.map_err(|error| format!("WIC GetFrame(0): {error}"))
}

fn source_size(source: &IWICBitmapSource) -> Result<(u32, u32), String> {
    let mut width = 0u32;
    let mut height = 0u32;
    // SAFETY: `source` is a live WIC bitmap source and both out-pointers refer
    // to initialized local storage for the duration of the call.
    unsafe { source.GetSize(&mut width, &mut height) }
        .map_err(|error| format!("WIC IWICBitmapSource::GetSize: {error}"))?;
    Ok((width, height))
}

/// Converts one decoded frame to the tightly packed premultiplied BGRA the
/// renderer uploads, optionally through a scaler.
fn decode_frame(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    max_side: u32,
) -> Result<ImageData, String> {
    let (width, height) = source_size(frame)?;
    if width == 0 || height == 0 {
        return Err(format!("WIC frame is empty ({width}x{height})"));
    }
    let (target_width, target_height) = fit(width, height, max_side);

    let scaler = if (target_width, target_height) != (width, height) {
        // SAFETY: `factory` is a live WIC factory.
        let scaler = unsafe { factory.CreateBitmapScaler() }
            .map_err(|error| format!("WIC CreateBitmapScaler: {error}"))?;
        // SAFETY: `scaler` is a fresh scaler and `frame` outlives it; the
        // requested size is at least 1x1 and no larger than the source.
        unsafe {
            scaler.Initialize(
                frame,
                target_width,
                target_height,
                WICBitmapInterpolationModeFant,
            )
        }
        .map_err(|error| {
            format!("WIC IWICBitmapScaler::Initialize({target_width}x{target_height}): {error}")
        })?;
        Some(scaler)
    } else {
        None
    };

    let source: &IWICBitmapSource = match &scaler {
        Some(scaler) => scaler,
        None => frame,
    };

    // SAFETY: `factory` is a live WIC factory.
    let converter = unsafe { factory.CreateFormatConverter() }
        .map_err(|error| format!("WIC CreateFormatConverter: {error}"))?;
    // SAFETY: `converter` is fresh, `source` outlives the call and stays alive
    // through `CopyPixels`, and the destination GUID is a static WIC format id.
    unsafe {
        converter.Initialize(
            source,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None::<&IWICPalette>,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
    }
    .map_err(|error| format!("WIC IWICFormatConverter::Initialize(32bppPBGRA): {error}"))?;

    let stride = target_width
        .checked_mul(4)
        .ok_or_else(|| format!("WIC stride overflows for {target_width}x{target_height}"))?;
    let width_px = usize::try_from(target_width)
        .map_err(|_| format!("WIC width {target_width} does not fit in usize"))?;
    let height_px = usize::try_from(target_height)
        .map_err(|_| format!("WIC height {target_height} does not fit in usize"))?;
    let len = width_px
        .checked_mul(height_px)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            format!("WIC pixel buffer size overflows for {target_width}x{target_height}")
        })?;
    let rect = WICRect {
        X: 0,
        Y: 0,
        Width: i32::try_from(target_width)
            .map_err(|_| format!("WIC width {target_width} does not fit in i32"))?,
        Height: i32::try_from(target_height)
            .map_err(|_| format!("WIC height {target_height} does not fit in i32"))?,
    };
    let mut bgra = vec![0u8; len];
    // SAFETY: the converter was initialized successfully, `rect` describes the
    // full converted source, and `bgra` is exactly `stride * height` bytes.
    unsafe { converter.CopyPixels(&rect, stride, &mut bgra) }.map_err(|error| {
        format!("WIC IWICBitmapSource::CopyPixels({target_width}x{target_height}): {error}")
    })?;

    Ok(ImageData::new(target_width, target_height, bgra))
}

fn encode_png_file(temp_path: &Path, image: &ImageData) -> Result<(), String> {
    let factory = imaging_factory()?;
    // SAFETY: `factory` is a live WIC factory.
    let stream =
        unsafe { factory.CreateStream() }.map_err(|error| format!("WIC CreateStream: {error}"))?;
    let wide = wide_path(temp_path);
    // SAFETY: `stream` is a fresh WIC stream, `wide` is a NUL-terminated UTF-16
    // path that stays alive for the call, and `GENERIC_WRITE` is the access
    // right the encoder needs.
    unsafe { stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), GENERIC_WRITE.0) }.map_err(
        |error| {
            format!(
                "WIC InitializeFromFilename({}): {error}",
                temp_path.display()
            )
        },
    )?;
    write_png(&factory, &stream, image)
    // `stream` is dropped here, closing the file before the caller renames it.
}

fn write_png(
    factory: &IWICImagingFactory,
    stream: &IStream,
    image: &ImageData,
) -> Result<(), String> {
    if !image.is_valid() {
        return Err(format!(
            "WIC encode: invalid image ({}x{}, {} bytes)",
            image.width,
            image.height,
            image.bgra.len()
        ));
    }
    let stride = image
        .width
        .checked_mul(4)
        .ok_or_else(|| format!("WIC encode: stride overflows for width {}", image.width))?;
    // The windows wrapper converts the slice length to u32 with an internal
    // unwrap, so a buffer that cannot be described by the API is rejected here.
    u32::try_from(image.bgra.len())
        .map_err(|_| "WIC encode: pixel buffer is larger than 4 GiB".to_string())?;

    // SAFETY: `factory` is a live WIC factory and the PNG container GUID is a
    // static WIC format id; no vendor is selected.
    let encoder = unsafe { factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()) }
        .map_err(|error| format!("WIC CreateEncoder(PNG): {error}"))?;
    // SAFETY: `encoder` and `stream` are live COM objects and the stream stays
    // alive until the encoder has been committed and dropped.
    unsafe { encoder.Initialize(stream, WICBitmapEncoderNoCache) }
        .map_err(|error| format!("WIC IWICBitmapEncoder::Initialize: {error}"))?;

    let mut frame = None;
    // SAFETY: `frame` is a live local out-parameter; no encoder options are
    // requested, so a null property-bag pointer is passed.
    unsafe { encoder.CreateNewFrame(&mut frame, std::ptr::null_mut()) }
        .map_err(|error| format!("WIC CreateNewFrame: {error}"))?;
    let frame = frame.ok_or_else(|| "WIC CreateNewFrame returned no frame".to_string())?;

    // SAFETY: `frame` is a live frame encoder and a null options bag means the
    // encoder defaults.
    unsafe { frame.Initialize(None::<&IPropertyBag2>) }
        .map_err(|error| format!("WIC IWICBitmapFrameEncode::Initialize: {error}"))?;
    // SAFETY: the size comes from a validated `ImageData`.
    unsafe { frame.SetSize(image.width, image.height) }
        .map_err(|error| format!("WIC SetSize({}x{}): {error}", image.width, image.height))?;
    let mut pixel_format = GUID_WICPixelFormat32bppBGRA;
    // SAFETY: the format GUID is a live local and the encoder may replace it
    // with the closest format it supports.
    unsafe { frame.SetPixelFormat(&mut pixel_format) }
        .map_err(|error| format!("WIC SetPixelFormat(32bppBGRA): {error}"))?;
    // SAFETY: `image.bgra` holds exactly `stride * height` bytes (`is_valid`
    // above) and the encoder was told the matching 32bppBGRA layout.
    unsafe { frame.WritePixels(image.height, stride, &image.bgra) }
        .map_err(|error| format!("WIC WritePixels({}x{}): {error}", image.width, image.height))?;
    // SAFETY: the frame encoder above is live and has all of its pixels.
    unsafe { frame.Commit() }
        .map_err(|error| format!("WIC IWICBitmapFrameEncode::Commit: {error}"))?;
    // SAFETY: `encoder` is live, its only frame has been committed, and the
    // stream stays alive until the encoder is dropped.
    unsafe { encoder.Commit() }
        .map_err(|error| format!("WIC IWICBitmapEncoder::Commit: {error}"))?;
    Ok(())
}

/// Appends `.tmp` to the target path so the temporary file lands in the same
/// directory and the final rename can never cross a volume.
fn temp_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// NUL-terminated UTF-16 copy of a path, kept alive for the duration of a call.
fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_paths_are_recognized_case_insensitively() {
        assert!(is_image_path(Path::new(r"C:\shots\shot.png")));
        assert!(is_image_path(Path::new(r"C:\shots\shot.JPG")));
        assert!(is_image_path(Path::new("a.jpeg")));
        assert!(is_image_path(Path::new("a.TIFF")));
        assert!(!is_image_path(Path::new(r"C:\shots\notes.txt")));
        assert!(!is_image_path(Path::new(r"C:\shots\shot.webp")));
        assert!(!is_image_path(Path::new("no-extension")));
    }

    #[test]
    fn fit_never_upscales_and_preserves_aspect() {
        assert_eq!(fit(100, 50, 200), (100, 50));
        assert_eq!(fit(100, 50, 100), (100, 50));
        assert_eq!(fit(100, 50, 50), (50, 25));
        assert_eq!(fit(50, 100, 50), (25, 50));
        assert_eq!(fit(1920, 1080, 480), (480, 270));
        assert_eq!(fit(100, 50, 10), (10, 5));
    }

    #[test]
    fn fit_clamps_small_sides_to_one_pixel() {
        assert_eq!(fit(1000, 1, 10), (10, 1));
        assert_eq!(fit(3, 1, 2), (2, 1));
        assert_eq!(fit(1, 1, 0), (1, 1));
    }

    #[test]
    fn wic_png_round_trip() {
        let _com = crate::window::ComGuard::init(windows::Win32::System::Com::COINIT_MULTITHREADED)
            .expect("COM initialization for the WIC test");

        let mut bgra = Vec::with_capacity(4 * 4 * 4);
        for y in 0..4u8 {
            for x in 0..4u8 {
                bgra.extend_from_slice(&[x * 20, y * 20, 40, 255]);
            }
        }
        let image = ImageData::new(4, 4, bgra);
        let path =
            std::env::temp_dir().join(format!("pegline-wic-test-{}.png", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let result = (|| -> Result<(), String> {
            encode_png(&path, &image)?;
            assert_eq!(pixel_size(&path)?, (4, 4));

            let decoded = decode_full(&path)?;
            assert_eq!((decoded.width, decoded.height), (4, 4));
            assert_eq!(decoded.bgra, image.bgra);

            let thumb = decode_scaled(&path, 2)?;
            assert_eq!((thumb.width, thumb.height), (2, 2));

            // The annotation editor overwrites an existing file, so the
            // temporary file must replace the target, not fail on it.
            let mut changed = image.clone();
            changed.bgra[0] = 7;
            encode_png(&path, &changed)?;
            assert_eq!(decode_full(&path)?.bgra, changed.bgra);

            let bytes = encode_png_bytes(&image)?;
            assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
            let from_bytes = decode_bytes(&bytes, 480)?;
            assert_eq!((from_bytes.width, from_bytes.height), (4, 4));
            assert_eq!(from_bytes.bgra, image.bgra);

            assert!(same_picture(&path, &path)?);
            Ok(())
        })();

        let _ = std::fs::remove_file(&path);
        result.expect("WIC round trip");
    }
}
