//! `ReadDirectoryChangesW` watcher for the screenshots folder.
//!
//! The watcher owns one thread, debounces bursts by 250 ms and reports files
//! that are new since the watcher started (files present at startup count as
//! known, per `docs/BEHAVIOR.md` §9.1). A missing folder is retried every 3 s.
//!
//! A notification is only a trigger: the folder is rescanned with
//! `FindFirstFileW`, so a burst that overflows the notification buffer still
//! reports every file and a screenshot that is still being written is picked up
//! by the next scan.

use std::collections::HashSet;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NO_MORE_FILES,
    ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FindClose, FindFirstFileW, FindNextFileW, OPEN_EXISTING,
    ReadDirectoryChangesW, WIN32_FIND_DATAW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::capture::wic::IMAGE_EXTENSIONS;
use crate::logging;
use crate::win32::werr;

/// How long a burst of notifications settles before the folder is rescanned.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// How often a folder that does not exist yet is retried (`BEHAVIOR.md` §9.1).
const FOLDER_RETRY: Duration = Duration::from_secs(3);
/// Bound on how long a cancelled read may take before its buffer is abandoned.
const CANCEL_TIMEOUT_MS: u32 = 5_000;
/// Space for change records, in `u32` words so the buffer is DWORD-aligned.
/// The size is not critical: the rescan is what decides what is new, so records
/// that do not fit only cost one extra scan.
const CHANGE_BUFFER_WORDS: usize = 2048;
/// Names Windows and tools drop into folders; they are never screenshots.
const IGNORED_NAMES: &[&str] = &["thumbs.db", "desktop.ini", ".ds_store"];

/// A running watcher; dropping it stops the thread.
pub struct Watcher {
    /// The stop event as a raw value, so the watch can move between threads;
    /// the handle is closed by `stop` once the thread has joined.
    stop_event: usize,
    thread: Option<JoinHandle<()>>,
}

/// Starts watching `folder`. `on_new` runs on the watcher thread and must not
/// block on the UI thread; the app passes a channel sender.
pub fn start(folder: PathBuf, on_new: Box<dyn Fn(Vec<PathBuf>) + Send>) -> Result<Watcher, String> {
    // Manual reset: once stopped, every wait on the event must stay stopped.
    // SAFETY: no security attributes, no name, and the handle is owned by this
    // function (and closed below if the thread cannot be spawned).
    let stop_event = unsafe { CreateEventW(None, true, false, None) }
        .map_err(|error| werr("CreateEventW", error))?;
    let stop_raw = stop_event.0 as usize;

    let thread = std::thread::Builder::new()
        .name("pegline-watcher".to_string())
        .spawn(move || run(folder, on_new, stop_raw))
        .map_err(|error| {
            close_handle(stop_event);
            format!("spawning the watcher thread: {error}")
        })?;

    Ok(Watcher {
        stop_event: stop_raw,
        thread: Some(thread),
    })
}

impl Watcher {
    /// Stops the thread and waits for it. Safe to call twice.
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let stop_event = HANDLE(self.stop_event as *mut c_void);
        // SAFETY: the event was created by `start` and is still open; setting a
        // manual-reset event that is already set is harmless.
        unsafe {
            let _ = SetEvent(stop_event);
        }
        if thread.join().is_err() {
            logging::error("watcher: thread panicked");
        }
        close_handle(stop_event);
        self.stop_event = 0;
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The watcher thread: keeps a watch on the folder, retrying the whole setup
/// every 3 s while the folder is missing or the watch cannot be re-established.
fn run(folder: PathBuf, on_new: Box<dyn Fn(Vec<PathBuf>) + Send>, stop_raw: usize) {
    let stop_event = HANDLE(stop_raw as *mut c_void);
    let mut known: HashSet<String> = HashSet::new();
    let mut missing_logged = false;
    let mut last_error: Option<String> = None;

    loop {
        let dir = match open_folder(&folder) {
            Ok(dir) => {
                missing_logged = false;
                dir
            }
            Err(DirectoryError::Missing) => {
                // Windows only creates the Screenshots folder on the first
                // Win+PrtScn, so a missing folder is not an error worth a line.
                if !missing_logged {
                    logging::trace(format!(
                        "watcher: {} does not exist yet; retrying every 3 s",
                        folder.display()
                    ));
                    missing_logged = true;
                }
                if !wait_retry(stop_event, FOLDER_RETRY) {
                    return;
                }
                continue;
            }
            Err(DirectoryError::Failed(error)) => {
                log_error_once(
                    &mut last_error,
                    format!("watcher: cannot open {}: {error}", folder.display()),
                );
                if !wait_retry(stop_event, FOLDER_RETRY) {
                    return;
                }
                continue;
            }
        };

        match watch_until_stopped(dir, &folder, on_new.as_ref(), stop_event, &mut known) {
            WatchEnd::Stopped => return,
            WatchEnd::Failed(error) => log_error_once(
                &mut last_error,
                format!("watcher: watching {} failed: {error}", folder.display()),
            ),
        }
        if !wait_retry(stop_event, FOLDER_RETRY) {
            return;
        }
    }
}

/// Why the folder could not be opened.
enum DirectoryError {
    /// The folder is not there yet; keep polling.
    Missing,
    /// Anything else, worth a log line.
    Failed(String),
}

/// Opens the folder for asynchronous change notifications.
fn open_folder(folder: &Path) -> Result<HANDLE, DirectoryError> {
    let wide = wide_path(folder);
    // FILE_FLAG_OVERLAPPED is what the change event relies on, and
    // FILE_FLAG_BACKUP_SEMANTICS is what lets CreateFileW open a directory.
    // SAFETY: `wide` is NUL-terminated and alive for the call; no security
    // attributes and no template file are used.
    let result = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };
    match result {
        Ok(handle) => Ok(handle),
        Err(error) => {
            // SAFETY: GetLastError reads the calling thread's last error, which
            // the failed CreateFileW call has just set.
            let code = unsafe { GetLastError() };
            if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
                Err(DirectoryError::Missing)
            } else {
                Err(DirectoryError::Failed(werr("CreateFileW", error)))
            }
        }
    }
}

/// How a watch session ended.
enum WatchEnd {
    /// The stop event was set.
    Stopped,
    /// The folder or the directory handle went away.
    Failed(String),
}

/// Watches an open folder handle until stopped or broken. The handle is closed
/// before returning.
fn watch_until_stopped(
    dir: HANDLE,
    folder: &Path,
    on_new: &(dyn Fn(Vec<PathBuf>) + Send),
    stop_event: HANDLE,
    known: &mut HashSet<String>,
) -> WatchEnd {
    // Auto-reset: one request completes at most once, so a wait consumes the
    // signal, and the kernel resets it when the next request starts.
    // SAFETY: no security attributes and no name; the handle is owned here and
    // closed before returning.
    let change_event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(handle) => handle,
        Err(error) => {
            close_handle(dir);
            return WatchEnd::Failed(werr("CreateEventW", error));
        }
    };

    let end = read_and_scan(dir, change_event, folder, on_new, stop_event, known);

    close_handle(change_event);
    close_handle(dir);
    end
}

/// Issues change notifications, debounces them and rescans the folder.
fn read_and_scan(
    dir: HANDLE,
    change_event: HANDLE,
    folder: &Path,
    on_new: &(dyn Fn(Vec<PathBuf>) + Send),
    stop_event: HANDLE,
    known: &mut HashSet<String>,
) -> WatchEnd {
    // Boxed so it can be leaked if a cancelled request never completes: the
    // kernel must never write its completion into memory that is already gone.
    let mut context = Box::new(ChangeBuffer {
        data: [0; CHANGE_BUFFER_WORDS],
        overlapped: OVERLAPPED {
            hEvent: change_event,
            ..Default::default()
        },
    });

    let mut next_scan: Option<Instant> = None;
    if let Err(error) = begin_read(dir, &mut context) {
        return WatchEnd::Failed(error);
    }
    let mut pending = true;

    // Seed after the read is queued and before the first scan: everything the
    // listing sees is pre-existing and never reported, while a screenshot saved
    // during the listing still triggers the pending read. The window is one
    // directory read, and the reference has the same one.
    seed_known(folder, known);

    loop {
        if !pending {
            if let Err(error) = begin_read(dir, &mut context) {
                return WatchEnd::Failed(error);
            }
            pending = true;
        }

        let timeout = match next_scan {
            Some(due) => wait_ms(due.saturating_duration_since(Instant::now())),
            None => INFINITE,
        };
        let handles = [stop_event, change_event];
        // SAFETY: both handles are open for the duration of the wait; the change
        // event belongs to the outstanding request, which is still alive.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, timeout) };

        if wait == WAIT_OBJECT_0 {
            if pending && !cancel_read(dir, change_event, &context.overlapped) {
                // Cancellation itself hung; the kernel may still write into the
                // buffer, so it is leaked rather than freed under the request.
                logging::error("watcher: cancelled read did not complete; leaking its buffer");
                std::mem::forget(context);
            }
            return WatchEnd::Stopped;
        }

        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            match complete_read(dir, &context.overlapped) {
                Ok(true) => {
                    pending = false;
                    next_scan = Some(Instant::now() + DEBOUNCE);
                }
                // A stale signal: the request is still running, wait again.
                Ok(false) => {}
                Err(error) => return WatchEnd::Failed(error),
            }
            continue;
        }

        if wait == WAIT_TIMEOUT {
            if next_scan.take().is_none() {
                // No scan was due; an INFINITE wait cannot time out.
                continue;
            }
            let outcome = rescan(folder, known);
            if !outcome.new_files.is_empty() {
                report(on_new, outcome.new_files);
            }
            if outcome.deferred {
                // A new file was still empty; look again after the next debounce.
                next_scan = Some(Instant::now() + DEBOUNCE);
            }
            continue;
        }

        // WAIT_FAILED, or a handle index that was not requested.
        // SAFETY: GetLastError reads the calling thread's last error.
        let code = unsafe { GetLastError() };
        return WatchEnd::Failed(format!("WaitForMultipleObjects: {code:?}"));
    }
}

/// The buffer and `OVERLAPPED` of one pending `ReadDirectoryChangesW` request.
struct ChangeBuffer {
    data: [u32; CHANGE_BUFFER_WORDS],
    overlapped: OVERLAPPED,
}

/// Result of queuing one change notification.
///
/// A nonzero return from `ReadDirectoryChangesW` only means the request was
/// accepted; with an `OVERLAPPED` structure and no completion routine, the
/// change event is what reports that it has something to report. `Ok` here
/// therefore means "outstanding", never "completed".
fn begin_read(dir: HANDLE, context: &mut ChangeBuffer) -> Result<(), String> {
    let event = context.overlapped.hEvent;
    context.overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    let bytes = std::mem::size_of_val(&context.data) as u32;
    // SAFETY: `dir` is an open directory handle; `context` is boxed and stays
    // alive until the request completes (or is deliberately leaked); the event
    // in the overlapped structure is owned by the caller; no completion routine
    // is used, so the request is observed through that event.
    let result = unsafe {
        ReadDirectoryChangesW(
            dir,
            context.data.as_mut_ptr() as *mut c_void,
            bytes,
            false,
            FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE,
            None,
            Some(&mut context.overlapped as *mut OVERLAPPED),
            None,
        )
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            // SAFETY: GetLastError reads the calling thread's last error, which
            // the failed call has just set. ERROR_IO_PENDING is the documented
            // "queued asynchronously" answer; anything else is a real failure.
            let code = unsafe { GetLastError() };
            if code == ERROR_IO_PENDING {
                Ok(())
            } else {
                Err(werr("ReadDirectoryChangesW", error))
            }
        }
    }
}

/// Collects the result of the outstanding request. `Ok(false)` means the event
/// was stale and the request is in fact still pending.
fn complete_read(dir: HANDLE, overlapped: &OVERLAPPED) -> Result<bool, String> {
    let mut bytes = 0u32;
    // SAFETY: `dir` is the handle the request was issued on and `overlapped` is
    // the structure passed with it; `false` keeps the caller from blocking on a
    // request that is still running (`ERROR_IO_INCOMPLETE`).
    match unsafe { GetOverlappedResult(dir, overlapped, &mut bytes, false) } {
        Ok(()) => Ok(true),
        Err(error) => {
            // SAFETY: GetLastError reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            if code == ERROR_IO_INCOMPLETE {
                Ok(false)
            } else {
                Err(werr("GetOverlappedResult", error))
            }
        }
    }
}

/// Cancels the outstanding request and waits for its completion, so the kernel
/// can no longer touch the buffer. False when the completion never arrived.
fn cancel_read(dir: HANDLE, change_event: HANDLE, overlapped: &OVERLAPPED) -> bool {
    // The stop event can win the race against a notification that has already
    // completed, so look for a result before cancelling anything: waiting for a
    // completion that already happened would stall `stop` for the full timeout.
    let mut bytes = 0u32;
    // SAFETY: `dir` is the handle the request was issued on and `overlapped` is
    // the structure passed with it; `false` only polls.
    let completed = match unsafe { GetOverlappedResult(dir, overlapped, &mut bytes, false) } {
        Ok(()) => true,
        Err(_) => {
            // SAFETY: GetLastError reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            code != ERROR_IO_INCOMPLETE
        }
    };
    if completed {
        return true;
    }

    // SAFETY: `dir` is an open directory handle and `overlapped` is the structure
    // of the request being cancelled.
    let _ = unsafe { CancelIoEx(dir, Some(overlapped as *const OVERLAPPED)) };
    // The bounded wait keeps `stop` from hanging on a cancellation that never
    // completes.
    // SAFETY: the event belongs to the request; waiting on it is how the
    // cancellation is observed.
    let _ = unsafe { WaitForSingleObject(change_event, CANCEL_TIMEOUT_MS) };
    let mut bytes = 0u32;
    // SAFETY: after the wait the request is complete or abandoned; reading its
    // result touches nothing but the OVERLAPPED.
    match unsafe { GetOverlappedResult(dir, overlapped, &mut bytes, false) } {
        Ok(()) => true,
        Err(_) => {
            // SAFETY: GetLastError reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            code != ERROR_IO_INCOMPLETE
        }
    }
}

/// Waits on the stop event. False means the watcher was asked to stop.
fn wait_retry(stop_event: HANDLE, timeout: Duration) -> bool {
    // SAFETY: the stop event is open for the whole lifetime of the thread.
    let wait = unsafe { WaitForSingleObject(stop_event, wait_ms(timeout)) };
    wait == WAIT_TIMEOUT
}

/// One directory entry as `FindFirstFileW` reports it.
struct FoundFile {
    name: String,
    path: PathBuf,
    attributes: u32,
    size: u64,
    created: u64,
}

/// Lists the folder, oldest first. The listing is not filtered; the caller
/// decides what counts as a screenshot.
fn list_folder(folder: &Path) -> Result<Vec<FoundFile>, String> {
    let pattern = folder.join("*");
    let wide = wide_path(&pattern);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `wide` is NUL-terminated and alive for the call; `data` is a valid
    // out-parameter and the returned handle is closed below.
    let handle = unsafe { FindFirstFileW(PCWSTR(wide.as_ptr()), &mut data) }
        .map_err(|error| werr("FindFirstFileW", error))?;

    let mut files = Vec::new();
    collect_entry(folder, &data, &mut files);
    let mut result = Ok(());
    loop {
        // SAFETY: `handle` came from FindFirstFileW above and stays open until
        // FindClose below; `data` is a valid out-parameter.
        match unsafe { FindNextFileW(handle, &mut data) } {
            Ok(()) => collect_entry(folder, &data, &mut files),
            Err(error) => {
                // SAFETY: GetLastError reads the calling thread's last error.
                let code = unsafe { GetLastError() };
                if code != ERROR_NO_MORE_FILES {
                    result = Err(werr("FindNextFileW", error));
                }
                break;
            }
        }
    }
    // SAFETY: `handle` is the find handle from above and is not used again.
    unsafe {
        let _ = FindClose(handle);
    }
    result?;
    // Oldest first, so cards hang in the order the screenshots were taken.
    files.sort_by_key(|file| file.created);
    Ok(files)
}

/// Converts one `WIN32_FIND_DATAW` into a `FoundFile`, skipping `.` and `..`.
fn collect_entry(folder: &Path, data: &WIN32_FIND_DATAW, files: &mut Vec<FoundFile>) {
    let length = data
        .cFileName
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(data.cFileName.len());
    if length == 0 {
        return;
    }
    let name = String::from_utf16_lossy(&data.cFileName[..length]);
    if name == "." || name == ".." {
        return;
    }
    files.push(FoundFile {
        path: folder.join(&name),
        name,
        attributes: data.dwFileAttributes,
        size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
        created: (u64::from(data.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(data.ftCreationTime.dwLowDateTime),
    });
}

/// Marks everything currently in the folder as known, so files that were there
/// before the watch started are never reported. Called while a read is queued.
fn seed_known(folder: &Path, known: &mut HashSet<String>) {
    let Ok(entries) = list_folder(folder) else {
        // A listing that fails right after the handle opened is not fatal: the
        // next notification rescans anyway.
        return;
    };
    *known = entries
        .into_iter()
        .filter(|file| is_candidate(&file.name, file.attributes))
        .map(|file| path_key(&file.path))
        .collect();
}

/// What one debounced rescan found.
struct ScanOutcome {
    /// Files that are new, complete and reportable, oldest first.
    new_files: Vec<PathBuf>,
    /// A new file had no content yet; scan again after the next debounce.
    deferred: bool,
}

/// Rescans the folder. Files that are new to `known` and ready are added to it
/// and returned; a zero-length file stays unknown so a later scan can report it
/// once it has been written.
fn rescan(folder: &Path, known: &mut HashSet<String>) -> ScanOutcome {
    let entries = match list_folder(folder) {
        Ok(entries) => entries,
        Err(error) => {
            logging::trace(format!(
                "watcher: cannot list {}: {error}",
                folder.display()
            ));
            return ScanOutcome {
                new_files: Vec::new(),
                deferred: false,
            };
        }
    };

    let mut outcome = ScanOutcome {
        new_files: Vec::new(),
        deferred: false,
    };
    let mut present = HashSet::with_capacity(entries.len());
    for entry in entries {
        if !is_candidate(&entry.name, entry.attributes) {
            continue;
        }
        let key = path_key(&entry.path);
        present.insert(key.clone());
        if known.contains(&key) {
            continue;
        }
        if !is_ready(entry.size) {
            outcome.deferred = true;
            continue;
        }
        known.insert(key);
        outcome.new_files.push(entry.path);
    }
    // Forget files that are gone, so a screenshot deleted from the folder and
    // saved again under the same name is reported again, as in the reference.
    known.retain(|key| present.contains(key));
    outcome
}

/// Calls the application callback with one batch and keeps a panicking callback
/// from silently killing the watcher thread. No lock is held at this point.
fn report(on_new: &(dyn Fn(Vec<PathBuf>) + Send), files: Vec<PathBuf>) {
    let names = files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let result = catch_unwind(AssertUnwindSafe(|| on_new(files)));
    if let Err(payload) = result {
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        logging::error(format!(
            "watcher: on_new panicked while reporting {names}: {message}"
        ));
    }
}

/// Logs a failure at error level unless it repeats the previous one, so a folder
/// that cannot be watched does not flood `error.log` every 3 s.
fn log_error_once(last: &mut Option<String>, message: String) {
    if last.as_deref() != Some(message.as_str()) {
        logging::error(&message);
        *last = Some(message);
    } else {
        logging::trace(&message);
    }
}

/// Whether an entry could be a screenshot at all: a plain, visible file with an
/// image extension that is not one of Windows' own thumbnail files.
fn is_candidate(name: &str, attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        && attributes & FILE_ATTRIBUTE_HIDDEN.0 == 0
        && has_image_extension(name)
        && !is_excluded_name(name)
}

/// Whether the name has one of the extensions WIC can decode, ignoring case.
fn has_image_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Whether the name is one of the files Windows and tools keep in a folder.
fn is_excluded_name(name: &str) -> bool {
    IGNORED_NAMES
        .iter()
        .any(|excluded| name.eq_ignore_ascii_case(excluded))
}

/// A file with no content may still be being written; it stays unknown so the
/// next debounce scans it again.
fn is_ready(size: u64) -> bool {
    size > 0
}

/// Windows compares paths case-insensitively, so the known set is keyed by a
/// lowercased full path.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// Milliseconds to pass to a wait, rounded up so a deadline is never missed by a
/// sub-millisecond remainder and a zero-length wait cannot spin the thread.
fn wait_ms(remaining: Duration) -> u32 {
    let millis = remaining.as_nanos().div_ceil(1_000_000);
    millis.min(u128::from(u32::MAX) - 1) as u32
}

/// NUL-terminated UTF-16 for a Win32 call.
fn wide_path(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

/// Closes a handle this module owns. There is nothing useful to do about a
/// handle that cannot be closed.
fn close_handle(handle: HANDLE) {
    // SAFETY: `handle` was returned by a Win32 call in this module and is not
    // used again after this call.
    unsafe {
        let _ = CloseHandle(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_extensions_are_matched_without_case() {
        assert!(has_image_extension("shot.png"));
        assert!(has_image_extension("SHOT.PNG"));
        assert!(has_image_extension("shot.JpEg"));
        assert!(has_image_extension("scan.tiff"));
        assert!(!has_image_extension("shot.txt"));
        assert!(!has_image_extension("png"));
        assert!(!has_image_extension("shot.png.txt"));
        assert!(!has_image_extension(""));
    }

    #[test]
    fn thumbnail_files_are_excluded_without_case() {
        assert!(is_excluded_name("thumbs.db"));
        assert!(is_excluded_name("ThUmBs.Db"));
        assert!(is_excluded_name("desktop.ini"));
        assert!(is_excluded_name(".DS_STORE"));
        assert!(!is_excluded_name("thumbs.db.png"));
        assert!(!is_excluded_name("my desktop.ini.png"));
    }

    #[test]
    fn directories_and_hidden_files_are_not_candidates() {
        let plain = 0u32;
        let hidden = FILE_ATTRIBUTE_HIDDEN.0;
        let directory = FILE_ATTRIBUTE_DIRECTORY.0;

        assert!(is_candidate("shot.png", plain));
        assert!(!is_candidate("shot.png", hidden));
        assert!(!is_candidate("shots.png", directory));
        assert!(!is_candidate("shot.png", hidden | directory));
        assert!(!is_candidate("thumbs.db", plain));
        assert!(!is_candidate("notes.txt", plain));
    }

    #[test]
    fn a_file_still_being_written_is_not_ready() {
        assert!(!is_ready(0));
        assert!(is_ready(1));
        assert!(is_ready(6 * 1024 * 1024));
    }

    #[test]
    fn the_debounce_deadline_is_rounded_up() {
        assert_eq!(wait_ms(Duration::ZERO), 0);
        assert_eq!(wait_ms(Duration::from_millis(250)), 250);
        assert_eq!(wait_ms(DEBOUNCE), 250);
        assert_eq!(wait_ms(Duration::from_micros(1)), 1);
        assert_eq!(wait_ms(Duration::from_micros(1_400)), 2);
        assert_eq!(wait_ms(FOLDER_RETRY), 3000);
    }

    #[test]
    fn known_paths_ignore_case() {
        assert_eq!(
            path_key(Path::new("C:\\Shots\\A.PNG")),
            path_key(Path::new("c:\\shots\\a.png"))
        );
        assert_ne!(
            path_key(Path::new("C:\\Shots\\a.png")),
            path_key(Path::new("C:\\Shots\\b.png"))
        );
    }
}
