//! Window classes and the message loop.
//!
//! The UI thread owns every window. Each `wndproc` recovers a raw pointer to
//! its owner from `GWLP_USERDATA`; the owner outlives the window because it is
//! boxed for exactly as long as the loop runs (see `app::run`). Messages are
//! caught with `catch_unwind`, so one broken gesture cannot take the tray icon
//! down with it.

use std::cell::{Cell, RefCell};
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
        // ERROR_CLASS_ALREADY_EXISTS (1410), already normalised by the API:
        // re-registering is expected because every helper window is created on
        // demand and lives in the same process.
        if error.code().0 as u32 != 0x8007_0582 {
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

thread_local! {
    /// Depth of nested modal loops (menus, message boxes, OLE drags, the
    /// shortcut recorder) entered on this thread.
    static MODAL_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Messages swallowed by those loops, re-posted when the outermost one exits.
    static MODAL_DEFERRED: RefCell<Vec<(HWND, u32, WPARAM, LPARAM)>> =
        const { RefCell::new(Vec::new()) };
}

/// Marks the scope of a nested modal message loop.
///
/// While one is on the stack, a message delivered to the hidden window must not
/// run `App::on_message`: the caller that entered the loop already holds
/// `&mut App`, and running the handler would create a second one — two live
/// mutable references to the same object, which is undefined behaviour. Timers
/// re-arm themselves; everything else is queued and re-posted on exit.
pub struct ModalGuard;

impl ModalGuard {
    /// Enters a modal scope; the loop is considered over when the guard drops.
    pub fn enter() -> Self {
        MODAL_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for ModalGuard {
    fn drop(&mut self) {
        MODAL_DEPTH.with(|depth| {
            let remaining = depth.get().saturating_sub(1);
            depth.set(remaining);
            if remaining != 0 {
                return;
            }
            // The outermost loop is gone, so no borrow of the app state is on
            // the stack: deliver what the nested loops swallowed.
            let queued: Vec<(HWND, u32, WPARAM, LPARAM)> =
                MODAL_DEFERRED.with(|queue| queue.borrow_mut().drain(..).collect());
            for (hwnd, message, wparam, lparam) in queued {
                // SAFETY: the handle belongs to this process and is alive; the
                // values are exactly the ones the window procedure received.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), message, wparam, lparam);
                }
            }
        });
    }
}

/// True while a modal loop is nested on this thread.
pub fn in_modal_loop() -> bool {
    MODAL_DEPTH.with(Cell::get) > 0
}

/// Queues a message that must not be handled while a modal loop is nested.
pub fn defer_message(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) {
    // Bounded: the messages that need this are rare, and a flood must not grow
    // the queue without limit.
    const LIMIT: usize = 32;
    MODAL_DEFERRED.with(|queue| {
        let mut queue = queue.borrow_mut();
        if queue.len() < LIMIT {
            queue.push((hwnd, message, wparam, lparam));
        }
    });
}

/// The hidden message window, stored as an integer because `HWND` is not
/// `Send` and the workers that call [`wake`] only need the value.
static WAKE_TARGET: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Records the window workers should wake. Called once, from `App::start`.
pub fn set_wake_target(hwnd: HWND) {
    WAKE_TARGET.store(hwnd.0 as isize, std::sync::atomic::Ordering::Release);
}

/// Wakes the UI thread so it can drain worker results immediately.
///
/// The message must be addressed to the hidden window: `PostMessageW` with a
/// null `HWND` posts to the *calling* thread's queue, and `DispatchMessageW`
/// drops hwnd-less messages, so the wake-up would never reach `on_message`.
pub fn wake() {
    let raw = WAKE_TARGET.load(std::sync::atomic::Ordering::Acquire);
    if raw == 0 {
        return;
    }
    // SAFETY: the handle was stored by `set_wake_target` and stays valid for the
    // life of the process; the message carries no pointer payload.
    unsafe {
        let _ = PostMessageW(
            Some(HWND(raw as *mut core::ffi::c_void)),
            WM_APP_WAKE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Ends the message loop.
pub fn quit() {
    // SAFETY: thread-local message queue operation.
    unsafe { PostQuitMessage(0) };
}
