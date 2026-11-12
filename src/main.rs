//! Pegline process entry point: one instance, one message loop, one tray icon.
//!
//! The thread model is the one in `docs/ARCHITECTURE.md` §2: this (UI) thread
//! owns every HWND, the pointer state machine and all file IO decisions, while
//! the overlay and the flight window each run a render thread that owns its
//! swap chain and frame clock.

#![windows_subsystem = "windows"]

use pegline::{app, logging, win32};

fn main() {
    win32::ensure_dpi_awareness();
    logging::init();
    // Uncaught panics must land in error.log; window procedures also catch and
    // log them so one bad gesture cannot take the tray icon down.
    std::panic::set_hook(Box::new(|info| logging::error(format!("panic: {info}"))));

    if already_running() && !second_instance_allowed() {
        logging::trace("second instance started; exiting");
        return;
    }

    let code = app::run();
    std::process::exit(code);
}

/// `PEGLINE_ALLOW_SECOND_INSTANCE=1` lets a second copy run beside the user's
/// instance. The scripted visual acceptance runs need it: the tray app is a
/// singleton, so every `--note-*-demo` would otherwise exit before it opens a
/// window. The override is only read here and never changes normal starts.
fn second_instance_allowed() -> bool {
    let allowed = second_instance_allowed_value(
        std::env::var("PEGLINE_ALLOW_SECOND_INSTANCE")
            .ok()
            .as_deref(),
    );
    if allowed {
        logging::trace("second instance allowed by PEGLINE_ALLOW_SECOND_INSTANCE");
    }
    allowed
}

fn second_instance_allowed_value(value: Option<&str>) -> bool {
    value == Some("1")
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
        // SAFETY: the handle is ours; we close it because this process exits.
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

#[cfg(test)]
mod tests {
    use super::second_instance_allowed_value;

    #[test]
    fn second_instance_needs_an_explicit_opt_in() {
        assert!(!second_instance_allowed_value(None));
        assert!(!second_instance_allowed_value(Some("0")));
        assert!(!second_instance_allowed_value(Some("true")));
        assert!(second_instance_allowed_value(Some("1")));
    }
}
