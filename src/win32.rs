//! Thin, allocation-light wrappers around the Win32 calls the shell needs.
//!
//! Everything here is called from the UI thread except where noted. Each
//! function converts a Win32 error into `Err(String)` with the call name, so the
//! caller can log it and degrade instead of unwrapping.

use std::ffi::c_void;
use std::path::PathBuf;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForWindow,
    MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    FO_DELETE, FOLDERID_Screenshots, KF_FLAG_DEFAULT, SHFILEOPSTRUCTW, SHFileOperationW,
    SHGetKnownFolderPath,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetClassNameW, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowRect, HWND_TOPMOST, IsIconic, IsWindowVisible, SM_CXICON,
    SM_CXSMICON, SM_CYICON, SM_CYSMICON, SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, WS_EX_LAYERED, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
};
use windows::core::{BOOL, GUID, HSTRING, PCWSTR};

/// `MONITORINFOF_PRIMARY`; the constant lives in a feature we do not enable.
const MONITORINFOF_PRIMARY: u32 = 1;

use crate::types::RectI;

const RUN_KEY: PCWSTR = windows::core::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = windows::core::w!("Pegline");

/// Outcome of a Win32 call, with the function name prefixed for the log.
pub type WinResult<T> = Result<T, String>;

/// Turns a `windows` crate error into something worth logging.
pub fn werr(call: &str, error: windows::core::Error) -> String {
    format!("{call}: {error}")
}

/// Declares the process DPI aware. The manifest already does this; the call is
/// a belt-and-braces fallback for hosts that strip the manifest.
pub fn ensure_dpi_awareness() {
    // SAFETY: user32 reads no memory here; the context is a documented constant.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// One monitor, with both the desktop bounds and the work area in device pixels.
#[derive(Clone, Copy, Debug)]
pub struct Monitor {
    pub handle: HMONITOR,
    pub bounds: RectI,
    pub work: RectI,
    pub dpi: u32,
    pub primary: bool,
}

impl Monitor {
    pub fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }
}

fn rect_from_win32(rect: &RECT) -> RectI {
    RectI {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// Enumerates the active monitors. The callback only pushes into a local Vec,
/// which is why it can be a plain `extern "system"` function.
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the pointer to the Vec passed to EnumDisplayMonitors
        // below; it outlives every callback for the duration of that call.
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            // SAFETY: shcore fills the two out-params; failure leaves the 96
            // fallback in place.
            unsafe {
                let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            }
            list.push(Monitor {
                handle: monitor,
                bounds: rect_from_win32(&info.rcMonitor),
                work: rect_from_win32(&info.rcWork),
                dpi: dpi_x.max(96),
                primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        BOOL::from(true)
    }
    let mut list: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only touches `list`, which is alive for the call.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut list as *mut Vec<Monitor> as isize),
        )
    };
    list.sort_by_key(|m| (m.bounds.left, m.bounds.top));
    list
}

/// The monitor containing a point, falling back to the nearest one.
pub fn monitor_at(x: i32, y: i32) -> Option<Monitor> {
    // SAFETY: MonitorFromPoint takes a point by value and returns a handle.
    let handle = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
    monitors().into_iter().find(|m| m.handle == handle)
}

/// The cursor position in device pixels. Returns `None` if the call fails,
/// which only happens in exotic desktop states.
pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    // SAFETY: GetCursorPos writes one POINT.
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return None;
    }
    Some((point.x, point.y))
}

/// Foreground window rectangle in device pixels.
pub fn foreground_rect() -> Option<(HWND, RectI)> {
    // SAFETY: both calls are queries on the current desktop.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    Some((hwnd, rect_from_win32(&rect)))
}

pub fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: the buffer is ours and GetClassNameW clamps to its length.
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..len as usize])
}

/// True when a foreground window covers the whole monitor: maximised games,
/// video players and full-screen browsers. Shell windows are excluded because
/// the desktop and the taskbar are "full screen" by geometry but are not apps.
pub fn foreground_is_fullscreen_on(bounds: RectI) -> bool {
    let Some((hwnd, rect)) = foreground_rect() else {
        return false;
    };
    // SAFETY: IsIconic/IsWindowVisible are pure queries.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() || unsafe { IsIconic(hwnd) }.as_bool() {
        return false;
    }
    if rect != bounds {
        return false;
    }
    let class = window_class(hwnd);
    !matches!(
        class.as_str(),
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Windows.UI.Core.CoreWindow"
    ) && class != "PeglineOverlay"
}

/// Converts a `Path`-ish string into a NUL-terminated UTF-16 buffer.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The application directory: `%APPDATA%\Pegline`.
pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Pegline")
}

/// The local directory: `%LOCALAPPDATA%\Pegline`.
pub fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Pegline")
}

/// `%LOCALAPPDATA%\Pegline\Screenshots`: the inbox for captures we take over.
pub fn inbox_dir() -> PathBuf {
    local_dir().join("Screenshots")
}

/// The real Windows screenshot folder (Win+PrtScn target), resolved through the
/// known-folder API so a moved folder is still found.
pub fn screenshots_folder() -> Option<PathBuf> {
    // SAFETY: SHGetKnownFolderPath allocates the string with CoTaskMemAlloc; we
    // copy it out and free it with the matching allocator immediately.
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Screenshots, KF_FLAG_DEFAULT, None) }.ok()?;
    if raw.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated by contract of the API.
    let text = unsafe { raw.to_string() }.ok();
    // SAFETY: the pointer came from SHGetKnownFolderPath's allocator.
    unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };
    text.map(PathBuf::from)
}

/// Opens a file or folder with its default handler.
pub fn shell_open(path: &std::path::Path) -> WinResult<()> {
    let target = HSTRING::from(path.as_os_str());
    // SAFETY: ShellExecuteW only reads the NUL-terminated strings; no out-params.
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    if result.0 as usize <= 32 {
        return Err(format!("ShellExecuteW returned {}", result.0 as usize));
    }
    Ok(())
}

/// Opens Explorer with the file selected.
pub fn reveal_in_explorer(path: &std::path::Path) -> WinResult<()> {
    let arg = format!("/select,\"{}\"", path.display());
    let file = HSTRING::from("explorer.exe");
    let params = HSTRING::from(arg);
    // SAFETY: as above; both strings are alive for the call.
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            PCWSTR::null(),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    if result.0 as usize <= 32 {
        return Err(format!(
            "ShellExecuteW(explorer) returned {}",
            result.0 as usize
        ));
    }
    Ok(())
}

/// Moves files to the recycle bin. Explorer does the work, so the call must not
/// run on the UI thread for large sets; the caller uses a worker.
pub fn recycle_files(paths: &[PathBuf]) -> WinResult<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut from = String::new();
    for path in paths {
        from.push_str(&path.to_string_lossy());
        from.push('\0');
    }
    from.push('\0');
    let from_wide = wide(&from);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from_wide.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (windows::Win32::UI::Shell::FOF_ALLOWUNDO.0
            | windows::Win32::UI::Shell::FOF_NOCONFIRMATION.0
            | windows::Win32::UI::Shell::FOF_SILENT.0
            | windows::Win32::UI::Shell::FOF_NOERRORUI.0) as u16,
        fAnyOperationsAborted: BOOL::from(false),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: PCWSTR::null(),
    };
    // SAFETY: `op` and the double-NUL-terminated `from_wide` stay alive for the
    // synchronous call; no pointers are retained.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.fAnyOperationsAborted.as_bool() {
        return Err(format!("SHFileOperationW returned {code}"));
    }
    Ok(())
}

fn run_key(access: u32) -> WinResult<HKEY> {
    let mut key = HKEY::default();
    // SAFETY: the pre-defined root and a NUL-terminated subkey; `key` is written
    // on success and closed by the caller.
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            Some(0),
            windows::Win32::System::Registry::REG_SAM_FLAGS(access),
            &mut key,
        )
    };
    if result.is_err() {
        return Err(format!("RegOpenKeyExW: {result:?}"));
    }
    Ok(key)
}

/// Whether Pegline is registered to start with Windows.
pub fn autostart_enabled() -> bool {
    let mut buffer = [0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    let mut kind = REG_VALUE_TYPE::default();
    // SAFETY: the out-buffer is stack-allocated with its byte size passed in.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            windows::Win32::System::Registry::RRF_RT_REG_SZ,
            Some(&mut kind),
            Some(buffer.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    result == windows::Win32::Foundation::ERROR_SUCCESS && size > 2
}

/// Adds or removes the `HKCU\...\Run` entry. Uses the running executable's path.
pub fn set_autostart(enabled: bool) -> WinResult<()> {
    let key = run_key(KEY_SET_VALUE.0)?;
    let result = if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let quoted = format!("\"{}\"", exe.display());
        let data = wide(&quoted);
        // SAFETY: `key` is open for writing; the value data is a live UTF-16
        // buffer and the byte length counts the terminating NUL.
        unsafe {
            RegSetValueExW(
                key,
                RUN_VALUE,
                Some(0),
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    data.as_ptr() as *const u8,
                    data.len() * 2,
                )),
            )
        }
    } else {
        // SAFETY: `key` is open for writing; deleting a missing value is fine.
        unsafe { RegDeleteValueW(key, RUN_VALUE) }
    };
    // SAFETY: the key came from RegOpenKeyExW/RegCreateKeyExW.
    unsafe {
        let _ = RegCloseKey(key);
    }
    if result.is_err() {
        return Err(format!("autostart write: {result:?}"));
    }
    Ok(())
}

/// Creates the Run key if the user has never had one (unlikely, but cheap).
pub fn ensure_run_key() {
    let mut key = HKEY::default();
    // SAFETY: standard key creation with no security attributes requested.
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if result.is_ok() {
        // SAFETY: the handle was created above.
        unsafe {
            let _ = RegCloseKey(key);
        }
    }
}

/// Makes the window topmost without activating it.
pub fn set_topmost(hwnd: HWND) {
    // SAFETY: the handle belongs to this process; no size/move flags.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn show_no_activate(hwnd: HWND) {
    // SAFETY: showing a window we own.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

pub fn hide_window(hwnd: HWND) {
    // SAFETY: hiding a window we own.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// Enables or disables cross-process click-through.
///
/// Both `WS_EX_TRANSPARENT` and `WS_EX_LAYERED` are required on Windows 11 (M0
/// finding); `WS_EX_LAYERED` is only an input flag here, there is no redirection
/// bitmap and no `UpdateLayeredWindow`. `SWP_FRAMECHANGED` makes the hit test
/// pick the change up immediately.
pub fn set_click_through(hwnd: HWND, enabled: bool) {
    // SAFETY: reading and writing the extended style of our own window.
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let flags = WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0;
        let updated = if enabled {
            current | flags
        } else {
            current & !flags
        };
        if updated != current {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, updated as isize);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

pub fn is_topmost(hwnd: HWND) -> bool {
    // SAFETY: querying our own window's style.
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 }
}

/// Places a window in device pixels without activating it.
pub fn place_window(hwnd: HWND, rect: RectI, topmost: bool) {
    // SAFETY: moving a window we own; the handle is valid for the process life.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            if topmost { Some(HWND_TOPMOST) } else { None },
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            SWP_NOACTIVATE,
        );
    }
}

/// Window size in device pixels as the OS sees it.
pub fn window_rect(hwnd: HWND) -> Option<RectI> {
    let mut rect = RECT::default();
    // SAFETY: GetWindowRect writes one RECT.
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    Some(rect_from_win32(&rect))
}

pub fn window_dpi(hwnd: HWND) -> u32 {
    // SAFETY: pure query; 0 is not possible for a valid window, but clamp anyway.
    (unsafe { GetDpiForWindow(hwnd) }).max(96)
}

/// Standard icon sizes, in device pixels, for the current DPI.
pub fn small_icon_size() -> i32 {
    // SAFETY: metric query.
    unsafe { GetSystemMetrics(SM_CXSMICON).max(16) }
}

pub fn large_icon_size() -> i32 {
    // SAFETY: metric query.
    unsafe { GetSystemMetrics(SM_CXICON).max(32) }
}

/// Loads icon resource 1 (see `assets/pegline.rc`) at the requested size.
#[allow(clippy::manual_dangling_ptr)]
pub fn load_app_icon(size: i32) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    // SAFETY: the module handle is this process; resource 1 is embedded by
    // build.rs and `LoadImageW` returns null on failure.
    let instance = unsafe { GetModuleHandleW(None) }.ok()?;
    let icon = unsafe {
        windows::Win32::UI::WindowsAndMessaging::LoadImageW(
            Some(windows::Win32::Foundation::HINSTANCE(instance.0)),
            // SAFETY: MAKEINTRESOURCE(1): the integer is the resource id.
            #[allow(clippy::manual_dangling_ptr)]
            PCWSTR(1 as *const u16),
            windows::Win32::UI::WindowsAndMessaging::IMAGE_ICON,
            size,
            size,
            windows::Win32::UI::WindowsAndMessaging::LR_DEFAULTCOLOR,
        )
    }
    .ok()?;
    Some(windows::Win32::UI::WindowsAndMessaging::HICON(icon.0))
}

/// `GetSystemMetrics` convenience for the virtual desktop.
pub fn virtual_screen() -> RectI {
    // SAFETY: metric queries.
    unsafe {
        RectI::new(
            GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN),
            GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN),
            GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN)
                + GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN),
            GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN)
                + GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN),
        )
    }
}

pub fn icon_metrics() -> (i32, i32) {
    // SAFETY: metric queries.
    unsafe { (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)) }
}

pub fn small_icon_metrics() -> (i32, i32) {
    // SAFETY: metric queries.
    unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) }
}

/// QueryPerformanceCounter frequency, cached after the first call.
pub fn qpc_frequency() -> f64 {
    use std::sync::OnceLock;
    static FREQUENCY: OnceLock<f64> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut frequency = 0i64;
        // SAFETY: writes one LARGE_INTEGER.
        unsafe {
            let _ = QueryPerformanceFrequency(&mut frequency);
        }
        if frequency <= 0 {
            1.0
        } else {
            frequency as f64
        }
    })
}

/// Monotonic seconds since boot, from the high-resolution counter.
pub fn now_seconds() -> f64 {
    let mut ticks = 0i64;
    // SAFETY: writes one LARGE_INTEGER.
    unsafe {
        let _ = QueryPerformanceCounter(&mut ticks);
    }
    ticks as f64 / qpc_frequency()
}

/// A GUID from the shell namespace, used only for the screenshots folder.
pub fn known_folder_guid() -> GUID {
    FOLDERID_Screenshots
}

/// Handle type used by clipboard and drag-and-drop code.
pub type WinHandle = HANDLE;

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// Manual integration check: touches `HKCU\...\Run`, so it is ignored in
    /// the default test run.
    #[test]
    #[ignore = "writes the user's Run key; run with --ignored"]
    fn autostart_round_trip() {
        let before = autostart_enabled();
        assert!(set_autostart(true).is_ok());
        assert!(autostart_enabled());
        assert!(set_autostart(false).is_ok());
        assert!(!autostart_enabled());
        if before {
            assert!(set_autostart(true).is_ok());
        }
    }
}
