//! Pegline process entry point: one instance, one message loop, one tray icon.
//!
//! The thread model is the one in `docs/ARCHITECTURE.md` §2: this (UI) thread
//! owns every HWND, the pointer state machine and all file IO decisions, while
//! the overlay and the flight window each run a render thread that owns its
//! swap chain and frame clock.

#![windows_subsystem = "windows"]
// Phase A is still landing: several helpers are used by the flight window and
// the probe, which are the next milestone. Remove this before the v0.1 tag.
#![allow(dead_code)]

mod app;
mod capture;
mod dragdrop;
mod i18n;
mod layout;
mod logging;
mod render;
mod scene;
mod settings;
mod shortcut;
mod spring;
mod tray;
mod types;
mod win32;
mod window;

fn main() {
    win32::ensure_dpi_awareness();
    logging::init();

    if already_running() {
        logging::trace("second instance started; exiting");
        return;
    }

    let code = app::run();
    std::process::exit(code);
}

/// Holds the named mutex for the lifetime of the process.
///
/// The handle is deliberately leaked: the process is a singleton and the OS
/// releases the mutex when the process exits, so there is nothing to clean up.
fn already_running() -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::w;

    // SAFETY: no security attributes, an owned handle and a static name.
    let handle = unsafe { CreateMutexW(None, true, w!("Local\\Pegline.SingleInstance")) };
    let Ok(handle) = handle else {
        // If the mutex cannot be created we let the app run: refusing to start
        // is worse than two tray icons.
        logging::error("CreateMutexW failed; continuing without single-instance guard");
        return false;
    };
    // SAFETY: GetLastError reads the calling thread's last error.
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        // SAFETY: the handle is ours; close it because this process exits.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
    } else {
        // Never closed: the mutex must outlive this function for the whole
        // process. Dropping a copy of the handle value does not close it.
        let _ = handle;
    }
    already
}
