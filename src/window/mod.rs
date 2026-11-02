//! Window classes and the message loop.
//!
//! The UI thread owns every window. Each `wndproc` recovers a raw pointer to
//! its owner from `GWLP_USERDATA`; the owner outlives the window because it is
//! boxed for exactly as long as the loop runs (see `app::run`). Messages are
//! caught with `catch_unwind`, so one broken gesture cannot take the tray icon
//! down with it.

use std::panic::AssertUnwindSafe;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    COINIT, COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, GetMessageW, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, TranslateMessage, WM_APP, WM_QUIT, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::PCWSTR;

pub mod flight;
pub mod ghost;
pub mod overlay;
pub mod preview;
pub mod shortcuts;

use crate::logging;

/// Tray icon callback: `lParam` is the mouse message, `wParam` the icon id.
pub const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
/// The app asks itself to quit (used by workers and menus).
pub const WM_APP_QUIT: u32 = WM_APP + 2;
/// Forces a tick even if the 33 ms timer is not due yet (captures, clipboard).
pub const WM_APP_WAKE: u32 = WM_APP + 3;

/// COM initialisation for one thread, undone on drop.
pub struct ComGuard {
    kind: ComKind,
}

enum ComKind {
    Co,
    Ole,
}

impl ComGuard {
    /// Enters the apartment this thread asked for.
    pub fn init(mode: COINIT) -> Result<Self, String> {
        // SAFETY: CoInitializeEx is per-thread and may be called more than once;
        // the matching CoUninitialize runs in Drop.
        let result = unsafe { CoInitializeEx(None, mode) };
        if result.is_err() {
            return Err(format!("CoInitializeEx: {result:?}"));
        }
        Ok(Self { kind: ComKind::Co })
    }

    /// STA for the UI thread (clipboard, OLE menus, shell).
    pub fn apartment() -> Result<Self, String> {
        Self::init(COINIT_APARTMENTTHREADED)
    }

    /// OLE apartment for the UI thread. `OleInitialize` also initialises COM as
    /// STA and is required before `DoDragDrop`.
    pub fn ole() -> Result<Self, String> {
        // SAFETY: per-thread OLE initialisation; the matching OleUninitialize
        // runs in Drop.
        unsafe { windows::Win32::System::Ole::OleInitialize(None) }
            .map_err(|e| format!("OleInitialize: {e}"))?;
        Ok(Self { kind: ComKind::Ole })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        match self.kind {
            // SAFETY: balances the successful CoInitializeEx above.
            ComKind::Co => unsafe { CoUninitialize() },
            // SAFETY: balances the successful OleInitialize above.
            ComKind::Ole => unsafe { windows::Win32::System::Ole::OleUninitialize() },
        }
    }
}

/// Registers a window class once. Re-registering returns the existing atom.
pub fn register_class(
    name: PCWSTR,
    wndproc: windows::Win32::UI::WindowsAndMessaging::WNDPROC,
) -> Result<(), String> {
    // SAFETY: the class name is a static wide string and the procedure lives for
    // the process lifetime.
    let instance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let class = WNDCLASSW {
        style: windows::Win32::UI::WindowsAndMessaging::CS_HREDRAW
            | windows::Win32::UI::WindowsAndMessaging::CS_VREDRAW,
        lpfnWndProc: wndproc,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(instance.0),
        hIcon: Default::default(),
        hCursor: Default::default(),
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: name,
    };
    // SAFETY: RegisterClassW copies the class description.
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        let error = windows::core::Error::from_thread();
        // 1410 = class already registered, which is fine on a second window.
        if error.code().0 as u32 != 0x8000_0582 {
            // Not fatal: the window may still be created if the class exists.
            logging::error(format!("RegisterClassW failed: {error}"));
        }
    }
    Ok(())
}

/// Creates a hidden message-only window owned by `owner`.
///
/// `owner` is stored in `GWLP_USERDATA` and must stay alive until the window is
/// destroyed.
pub fn create_message_window(
    class: PCWSTR,
    title: PCWSTR,
    wndproc: windows::Win32::UI::WindowsAndMessaging::WNDPROC,
    owner: *mut (),
) -> Result<HWND, String> {
    register_class(class, wndproc)?;
    let instance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    // SAFETY: class registered above; no parent (message-only via HWND_MESSAGE
    // would suppress broadcasts we need for TaskbarCreated, so a tool window is
    // used instead and simply never shown).
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            title,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW({class:?}): {e}"))?;
    // SAFETY: the window was just created and belongs to this thread.
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            owner as isize,
        );
    }
    Ok(hwnd)
}

/// Runs the message loop until `WM_QUIT`. Returns the quit code.
///
/// `DispatchMessageW` is wrapped in `catch_unwind`: a panic in a window
/// procedure is logged and the loop continues, which is what keeps a tray app
/// usable after one bad frame.
pub fn message_loop() -> i32 {
    let mut message = MSG::default();
    loop {
        // SAFETY: a standard message loop; `message` is a live stack value.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            logging::error("GetMessageW failed");
            break;
        }
        if !result.as_bool() {
            break;
        }
        if message.message == WM_QUIT {
            return message.wParam.0 as i32;
        }
        // SAFETY: messages are dispatched to this thread's windows.
        let dispatched = std::panic::catch_unwind(AssertUnwindSafe(|| unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }));
        if dispatched.is_err() {
            logging::error("panic while dispatching a window message");
        }
    }
    0
}

/// Wakes the UI thread so it can drain worker results immediately.
pub fn wake(hwnd: HWND) {
    // SAFETY: posting to a window this process owns; the queue is bounded by the
    // 33 ms timer so a lost wake-up only delays a tick.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// Ends the message loop.
pub fn quit() {
    // SAFETY: thread-local message queue operation.
    unsafe { PostQuitMessage(0) };
}
