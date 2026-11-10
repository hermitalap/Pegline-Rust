//! File logging: `%APPDATA%\Pegline\error.log` always, `trace.log` on demand.
//!
//! Logging must never take the app down: every IO error is swallowed. The
//! handle is opened per write, which is slower but keeps the log readable when
//! the process dies mid-session and avoids holding the file open forever.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::win32;

struct LogState {
    dir: PathBuf,
    trace: bool,
}

static STATE: OnceLock<LogState> = OnceLock::new();

/// Creates the log directory and decides whether tracing is on.
///
/// Tracing is enabled by `PEGLINE_TRACE=1` or by a `trace.on` file next to the
/// settings, so a user can turn it on without a console.
pub fn init() {
    let dir = win32::app_dir();
    if let Err(error) = fs::create_dir_all(&dir) {
        // There is nowhere to log this yet; the process still runs.
        let _ = error;
    }
    let trace =
        std::env::var("PEGLINE_TRACE").as_deref() == Ok("1") || dir.join("trace.on").exists();
    let _ = STATE.set(LogState { dir, trace });
}

fn path(name: &str) -> Option<PathBuf> {
    STATE.get().map(|state| state.dir.join(name))
}

fn trace_enabled() -> bool {
    STATE.get().map(|state| state.trace).unwrap_or(false)
}

/// Writes with the documented format: `2026-11-02T17:20:01.123+08:00 LEVEL msg`.
fn write_line(file: &str, level: &str, message: &str) {
    let Some(path) = path(file) else {
        return;
    };
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > 2 * 1024 * 1024 {
            let _ = fs::rename(&path, path.with_extension("old.log"));
        }
    }
    let Ok(mut handle) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let line = format!("{} {} {}\r\n", timestamp(), level, one_line(message));
    let _ = handle.write_all(line.as_bytes());
}

/// Collapses newlines so one event stays one line and the format is parseable.
fn one_line(message: &str) -> String {
    message.replace(['\r', '\n'], " ")
}

/// Records an unexpected condition. Never fails, never panics.
pub fn error(message: impl AsRef<str>) {
    write_line("error.log", "ERROR", message.as_ref());
}

/// Records a trace event. No-op unless tracing is enabled.
pub fn trace(message: impl AsRef<str>) {
    if trace_enabled() {
        write_line("trace.log", "TRACE", message.as_ref());
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmm+HH:MM` in the machine's local time zone.
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let offset = local_offset_seconds();
    let local = seconds + offset as i64;
    let days = local.div_euclid(86_400);
    let day_seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    let second = day_seconds % 60;
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.abs();
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}{sign}{:02}:{:02}",
        offset / 3600,
        (offset % 3600) / 60
    )
}

/// Days since the Unix epoch to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`, adapted). Kept local so the logging module has no deps.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current UTC offset in seconds, from the time-zone information.
fn local_offset_seconds() -> i32 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut info = TIME_ZONE_INFORMATION::default();
    // SAFETY: GetTimeZoneInformation fills the struct; the return value decides
    // whether the daylight-bias applies.
    let result = unsafe { GetTimeZoneInformation(&mut info) };
    let bias = match result {
        0 => info.Bias,
        1 => info.Bias + info.StandardBias,
        2 => info.Bias + info.DaylightBias,
        _ => info.Bias,
    };
    // The API reports minutes west of UTC.
    -(bias * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
