//! A wallpaper-derived Mica material painted inside the existing Direct2D
//! surfaces.
//!
//! Windows' real Mica (and WinUI's `SystemBackdropElement`) is a window-level
//! system backdrop and is opaque; it cannot fill a transparent, click-through
//! card. This module therefore bakes the *look* instead: the desktop wallpaper
//! is decoded once, aligned to the virtual screen, blurred, tinted toward the
//! app's white glass and given a little grain. Each renderer uploads that small
//! texture and samples it in screen space, so the material stays anchored to
//! the desktop while cards move, lean and scale.
//!
//! The cache is process-wide: one wallpaper decode and one upload per renderer.
//! `WM_SETTINGCHANGE`/`SPI_SETDESKWALLPAPER` bumps the generation and the next
//! frame rebuilds the texture. If anything fails the caller falls back to the
//! old solid glass fill.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, POINT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_BRUSH_PROPERTIES1, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1,
    D2D1_EXTEND_MODE_CLAMP, D2D1_INTERPOLATION_MODE_LINEAR, ID2D1Bitmap1, ID2D1BitmapBrush1,
    ID2D1DeviceContext,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::w;
use windows_numerics::Matrix3x2;

use crate::types::{ImageData, RectF, RectI};

/// How many texture pixels one screen pixel gets. A sixth is enough for a
/// blurred material and keeps the 6K virtual screen to roughly 2.3 MB.
const TEXTURE_SCALE: f32 = 0.17;

/// The wallpaper is decoded at this size before the blur; the result is then
/// resampled to `TEXTURE_SCALE` of the virtual screen. The 6K texture is 1023
/// pixels wide, so decoding past 1024 only retains temporary memory.
const DECODE_SIDE: u32 = 1024;

/// One texture pixel in DIP at 96 DPI, used to map the texture to the screen.
/// The app's glass white, one channel (the fill is `#F7F7F7`).
const WHITE: u32 = 0xF7;

static GENERATION: AtomicU64 = AtomicU64::new(0);
static CACHE: OnceLock<Mutex<WallpaperCache>> = OnceLock::new();

/// A decoded, blurred and tinted wallpaper, aligned to the virtual screen.
pub struct MicaImage {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    virtual_rect: RectI,
}

struct WallpaperCache {
    generation: u64,
    image: Option<Arc<MicaImage>>,
}

/// Bumps the wallpaper generation; the next painted frame rebuilds the texture.
pub fn notify_changed() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Returns the shared wallpaper texture, building it on first use or after a
/// change. Loading holds the cache lock, so concurrent renderers wait once.
fn image() -> Option<Arc<MicaImage>> {
    let generation = generation();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(WallpaperCache {
            generation: u64::MAX,
            image: None,
        })
    });
    let mut cache = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if cache.generation == generation {
        return cache.image.clone();
    }
    let built = build().map(Arc::new);
    cache.generation = generation;
    cache.image = built.clone();
    built
}

/// The per-renderer Mica resources. Small and cheap to keep next to a Renderer.
pub struct Mica {
    hwnd: HWND,
    dpi: f32,
    origin: (i32, i32),
    generation: u64,
    bitmap: Option<ID2D1Bitmap1>,
    brush: Option<ID2D1BitmapBrush1>,
    image: Option<Arc<MicaImage>>,
    enabled: bool,
}

impl Mica {
    pub fn new(hwnd: HWND, dpi: f32) -> Self {
        Self {
            hwnd,
            dpi,
            origin: (0, 0),
            generation: u64::MAX,
            bitmap: None,
            brush: None,
            image: None,
            enabled: transparency_enabled(),
        }
    }

    /// Refreshes the screen origin every frame and rebuilds the texture when the
    /// wallpaper changed. Called from the renderer's `begin`.
    pub fn refresh(&mut self, context: &ID2D1DeviceContext) {
        if !self.enabled {
            return;
        }
        self.origin = client_origin(self.hwnd);
        let generation = generation();
        if self.generation == generation {
            return;
        }
        self.generation = generation;
        self.bitmap = None;
        self.brush = None;
        self.image = None;
        let Some(image) = image() else {
            return;
        };
        match upload(context, &image) {
            Ok((bitmap, brush)) => {
                self.bitmap = Some(bitmap);
                self.brush = Some(brush);
                self.image = Some(image);
            }
            Err(error) => crate::logging::error(format!("mica: upload: {error}")),
        }
    }

    /// Fills a rounded rectangle with the wallpaper material, anchored to the
    /// screen. Returns false when there is nothing to draw, so the caller can
    /// fall back to the old solid glass.
    pub fn fill(
        &self,
        context: &ID2D1DeviceContext,
        rect: RectF,
        radius: f32,
        current: Matrix3x2,
        lit: bool,
    ) -> bool {
        let (Some(brush), Some(image)) = (self.brush.as_ref(), self.image.as_ref()) else {
            return false;
        };
        let dip = 96.0 / self.dpi;
        let texture_px_to_dip = dip / TEXTURE_SCALE;
        let screen = Matrix3x2 {
            M11: texture_px_to_dip,
            M12: 0.0,
            M21: 0.0,
            M22: texture_px_to_dip,
            M31: (image.virtual_rect.left - self.origin.0) as f32 * dip,
            M32: (image.virtual_rect.top - self.origin.1) as f32 * dip,
        };
        // Direct2D applies the brush transform to geometry coordinates, so this
        // maps local DIPs through the current transform and then back into
        // texture pixels. `screen` itself maps texture pixels to screen DIPs.
        let inverse_screen = Matrix3x2 {
            M11: 1.0 / texture_px_to_dip,
            M12: 0.0,
            M21: 0.0,
            M22: 1.0 / texture_px_to_dip,
            M31: -screen.M31 / texture_px_to_dip,
            M32: -screen.M32 / texture_px_to_dip,
        };
        let transform = current * inverse_screen;
        let rounded = crate::render::rounded(rect, radius);
        // SAFETY: the brush and context are live; the transform is a plain copy.
        unsafe {
            brush.SetTransform(&transform);
            brush.SetOpacity(if lit { 0.95 } else { 0.82 });
            context.FillRoundedRectangle(&rounded, brush);
        }
        true
    }
}

fn transparency_enabled() -> bool {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: the output buffer is a live u32 and `size` describes it.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("EnableTransparency"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    if result == ERROR_SUCCESS {
        value != 0
    } else {
        true
    }
}

fn client_origin(hwnd: HWND) -> (i32, i32) {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `point` is a live POINT for the call.
    if unsafe { ClientToScreen(hwnd, &mut point) }.as_bool() {
        (point.x, point.y)
    } else {
        (0, 0)
    }
}

fn upload(
    context: &ID2D1DeviceContext,
    image: &MicaImage,
) -> Result<(ID2D1Bitmap1, ID2D1BitmapBrush1), String> {
    let properties = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: ManuallyDrop::new(None),
    };
    // SAFETY: the pixel slice is alive for the call and D2D copies it.
    let bitmap = unsafe {
        context.CreateBitmap(
            D2D_SIZE_U {
                width: image.width,
                height: image.height,
            },
            Some(image.bgra.as_ptr() as *const c_void),
            image.width * 4,
            &properties,
        )
    }
    .map_err(|error| format!("CreateBitmap: {error}"))?;
    let brush_properties = D2D1_BITMAP_BRUSH_PROPERTIES1 {
        extendModeX: D2D1_EXTEND_MODE_CLAMP,
        extendModeY: D2D1_EXTEND_MODE_CLAMP,
        interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
    };
    // SAFETY: the bitmap is alive and owned by the caller; the brush keeps its
    // own reference.
    let brush = unsafe { context.CreateBitmapBrush(&bitmap, Some(&brush_properties), None) }
        .map_err(|error| format!("CreateBitmapBrush: {error}"))?;
    Ok((bitmap, brush))
}

fn wallpaper_path() -> Option<PathBuf> {
    let mut buffer = [0u16; 32768];
    // SAFETY: the buffer is live and its capacity is passed in UTF-16 units.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buffer.len() as u32,
            Some(buffer.as_mut_ptr() as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if let Err(error) = result {
        crate::logging::error(format!("mica: SPI_GETDESKWALLPAPER: {error}"));
        return None;
    }
    let end = buffer.iter().position(|&unit| unit == 0)?;
    if end == 0 {
        return None;
    }
    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..end]));
    path.is_file().then_some(path)
}

fn build() -> Option<MicaImage> {
    let path = wallpaper_path()?;
    let source = match crate::capture::wic::decode_scaled(&path, DECODE_SIDE) {
        Ok(image) => image,
        Err(error) => {
            crate::logging::error(format!("mica: decode {}: {error}", path.display()));
            return None;
        }
    };
    let virtual_rect = crate::win32::virtual_screen();
    if virtual_rect.is_empty() {
        return None;
    }
    let width = ((virtual_rect.width() as f32 * TEXTURE_SCALE).round() as u32).max(2);
    let height = ((virtual_rect.height() as f32 * TEXTURE_SCALE).round() as u32).max(2);
    let resized = cover_resize(&source, width, height)?;
    let blurred = blur(&resized, width as usize, height as usize, 8);
    let bgra = tint(&blurred);
    Some(MicaImage {
        width,
        height,
        bgra,
        virtual_rect,
    })
}

fn cover_resize(source: &ImageData, width: u32, height: u32) -> Option<ImageData> {
    let source_width = source.width as f32;
    let source_height = source.height as f32;
    let destination_width = width as f32;
    let destination_height = height as f32;
    let scale = (destination_width / source_width).max(destination_height / source_height);
    let crop_width = destination_width / scale;
    let crop_height = destination_height / scale;
    let left = (source_width - crop_width) * 0.5;
    let top = (source_height - crop_height) * 0.5;
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        let sy = top + (y as f32 + 0.5) * crop_height / destination_height;
        for x in 0..width {
            let sx = left + (x as f32 + 0.5) * crop_width / destination_width;
            let color = sample(source, sx, sy);
            let index = ((y * width + x) * 4) as usize;
            bgra[index..index + 4].copy_from_slice(&[color[0], color[1], color[2], 255]);
        }
    }
    Some(ImageData {
        width,
        height,
        bgra,
    })
}

fn sample(source: &ImageData, x: f32, y: f32) -> [u8; 3] {
    let max_x = source.width.saturating_sub(1) as f32;
    let max_y = source.height.saturating_sub(1) as f32;
    let x = x.clamp(0.0, max_x);
    let y = y.clamp(0.0, max_y);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(source.width.saturating_sub(1));
    let y1 = (y0 + 1).min(source.height.saturating_sub(1));
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let mut result = [0u8; 3];
    for (channel, output) in result.iter_mut().enumerate() {
        let p00 = source.bgra[((y0 * source.width + x0) * 4) as usize + channel] as f32;
        let p10 = source.bgra[((y0 * source.width + x1) * 4) as usize + channel] as f32;
        let p01 = source.bgra[((y1 * source.width + x0) * 4) as usize + channel] as f32;
        let p11 = source.bgra[((y1 * source.width + x1) * 4) as usize + channel] as f32;
        let top = p00 + (p10 - p00) * fx;
        let bottom = p01 + (p11 - p01) * fx;
        *output = (top + (bottom - top) * fy).round().clamp(0.0, 255.0) as u8;
    }
    result
}

fn blur(source: &ImageData, width: usize, height: usize, radius: usize) -> Vec<u8> {
    let mut first = source.bgra.clone();
    let mut second = vec![0u8; first.len()];
    for _ in 0..3 {
        blur_horizontal(&first, &mut second, width, height, radius);
        blur_vertical(&second, &mut first, width, height, radius);
    }
    first
}

fn blur_horizontal(
    source: &[u8],
    destination: &mut [u8],
    width: usize,
    height: usize,
    radius: usize,
) {
    let window = (radius * 2 + 1) as u32;
    for y in 0..height {
        let row = y * width;
        let mut sum = [0u32; 4];
        for offset in -(radius as isize)..=(radius as isize) {
            let x = offset.clamp(0, width as isize - 1) as usize;
            for channel in 0..4 {
                sum[channel] += source[(row + x) * 4 + channel] as u32;
            }
        }
        for x in 0..width {
            let index = (row + x) * 4;
            for channel in 0..4 {
                destination[index + channel] = (sum[channel] / window) as u8;
            }
            let leaving = x.saturating_sub(radius);
            for channel in 0..4 {
                sum[channel] -= source[(row + leaving) * 4 + channel] as u32;
            }
            let entering = (x + radius + 1).min(width - 1);
            for channel in 0..4 {
                sum[channel] += source[(row + entering) * 4 + channel] as u32;
            }
        }
    }
}

fn blur_vertical(
    source: &[u8],
    destination: &mut [u8],
    width: usize,
    height: usize,
    radius: usize,
) {
    let window = (radius * 2 + 1) as u32;
    for x in 0..width {
        let mut sum = [0u32; 4];
        for offset in -(radius as isize)..=(radius as isize) {
            let y = offset.clamp(0, height as isize - 1) as usize;
            for channel in 0..4 {
                sum[channel] += source[(y * width + x) * 4 + channel] as u32;
            }
        }
        for y in 0..height {
            let index = (y * width + x) * 4;
            for channel in 0..4 {
                destination[index + channel] = (sum[channel] / window) as u8;
            }
            let leaving = y.saturating_sub(radius);
            for channel in 0..4 {
                sum[channel] -= source[(leaving * width + x) * 4 + channel] as u32;
            }
            let entering = (y + radius + 1).min(height - 1);
            for channel in 0..4 {
                sum[channel] += source[(entering * width + x) * 4 + channel] as u32;
            }
        }
    }
}

fn tint(source: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; source.len()];
    for (index, pixel) in source.chunks_exact(4).enumerate() {
        let noise = noise(index);
        for channel in 0..3 {
            let wallpaper = pixel[channel] as u32;
            let value = (WHITE * 72 + wallpaper * 28) / 100;
            result[index * 4 + channel] = (value as i32 + noise).clamp(0, 255) as u8;
        }
        result[index * 4 + 3] = 255;
    }
    result
}

fn noise(index: usize) -> i32 {
    let x = index as u32;
    let hash = x.wrapping_mul(0x9E37_79B9) ^ (x >> 13);
    // -2..=2 in 8-bit units; enough to break up a gradient without looking dirty.
    (hash % 5) as i32 - 2
}

/// Exposed for tests: the texture size for a virtual screen.
pub fn texture_size(virtual_width: i32, virtual_height: i32) -> (u32, u32) {
    (
        ((virtual_width as f32 * TEXTURE_SCALE).round() as u32).max(2),
        ((virtual_height as f32 * TEXTURE_SCALE).round() as u32).max(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_size_is_at_least_two() {
        assert_eq!(texture_size(6016, 3384), (1023, 575));
        assert_eq!(texture_size(1, 1), (2, 2));
    }

    #[test]
    fn tint_keeps_alpha_opaque() {
        let source = vec![0u8, 0, 0, 255, 255, 255, 255, 255];
        let tinted = tint(&source);
        assert_eq!(tinted[3], 255);
        assert_eq!(tinted[7], 255);
        assert!(tinted[0] > 0);
    }

    #[test]
    fn blur_keeps_size() {
        let image = ImageData {
            width: 3,
            height: 2,
            bgra: vec![10u8; 3 * 2 * 4],
        };
        let blurred = blur(&image, 3, 2, 1);
        assert_eq!(blurred.len(), 24);
        assert_eq!(blurred[0], 10);
    }

    #[test]
    fn cover_resize_crops_to_destination() {
        let source = ImageData {
            width: 4,
            height: 2,
            bgra: vec![200u8; 4 * 2 * 4],
        };
        let resized = cover_resize(&source, 2, 2).expect("resize");
        assert_eq!((resized.width, resized.height), (2, 2));
        assert_eq!(resized.bgra.len(), 16);
    }

    #[test]
    fn wallpaper_path_is_optional() {
        // Never assert a value: a locked-down test machine may have no wallpaper.
        let _ = wallpaper_path();
    }
}
