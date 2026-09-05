//! The core's `log` bridge: a rotating on-device file (`<log_dir>/baybo.log`,
//! 2 MiB × 3 — the exportable log bundle, whose format stays fixed so exported
//! sessions stay comparable across builds) plus a stderr mirror (visible in
//! Xcode's console on debug runs). Levels are Warn globally, Debug for our own
//! crates.
//!
//! Timestamps are seconds-precision UTC derived from `SystemTime` — enough to
//! sequence a session against gateway/relay logs without pulling a chrono/time
//! dependency into the core.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Metadata, Record};
use parking_lot::Mutex;

const LOG_FILE_NAME: &str = "baybo.log";
const LOG_FILE_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Rotated files kept beside the live one (`baybo.log.1`, `baybo.log.2`).
const LOG_FILES_KEPT: usize = 2;

/// What `adb logcat -s` filters on. Matches the log file's own name so a bug
/// report and a live capture read the same.
#[cfg(target_os = "android")]
const LOGCAT_TAG: &str = "baybo";

/// Crates logged at Debug; everything else at Warn.
const DEBUG_TARGETS: [&str; 2] = ["baybo_ffi", "baybo_ffi::core"];

struct FileState {
    file: File,
    written: u64,
}

struct CoreLogger {
    dir: Option<PathBuf>,
    file: Mutex<Option<FileState>>,
}

impl CoreLogger {
    fn open(dir: &PathBuf) -> Option<FileState> {
        std::fs::create_dir_all(dir).ok()?;
        let path = dir.join(LOG_FILE_NAME);
        let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(FileState { file, written })
    }

    /// Shift `baybo.log` → `.1` → `.2`, dropping the oldest, then reopen fresh.
    fn rotate(&self, dir: &PathBuf) -> Option<FileState> {
        let base = dir.join(LOG_FILE_NAME);
        let _ = std::fs::remove_file(dir.join(format!("{LOG_FILE_NAME}.{LOG_FILES_KEPT}")));
        for i in (1..=LOG_FILES_KEPT).rev() {
            let from = if i == 1 {
                base.clone()
            } else {
                dir.join(format!("{LOG_FILE_NAME}.{}", i - 1))
            };
            let _ = std::fs::rename(&from, dir.join(format!("{LOG_FILE_NAME}.{i}")));
        }
        Self::open(dir)
    }
}

/// The live mirror beside the rotating file: Xcode's console on Apple targets,
/// logcat on Android.
///
/// Android needs its own arm rather than the `eprint!` every other target uses,
/// and not for taste: an app process inherits the zygote's stdio, so fd 2 is
/// `/dev/null` and anything written there reaches nobody. Only the Java
/// `System.err` stream is rerouted, and this crate does not go through it.
#[cfg(not(target_os = "android"))]
fn mirror(_level: Level, line: &str) {
    eprint!("{line}");
}

#[cfg(target_os = "android")]
fn mirror(level: Level, line: &str) {
    use android_log_sys::LogPriority;
    use std::ffi::CString;

    let priority = match level {
        Level::Error => LogPriority::ERROR,
        Level::Warn => LogPriority::WARN,
        Level::Info => LogPriority::INFO,
        Level::Debug => LogPriority::DEBUG,
        Level::Trace => LogPriority::VERBOSE,
    };
    // logcat is line-oriented and NUL-terminated; a message carrying an interior
    // NUL is dropped rather than truncated at it.
    let (Ok(tag), Ok(message)) = (CString::new(LOGCAT_TAG), CString::new(line.trim_end())) else {
        return;
    };
    // SAFETY: both pointers are valid NUL-terminated C strings that outlive the
    // call, which is what liblog's contract asks for.
    unsafe {
        android_log_sys::__android_log_write(priority as i32, tag.as_ptr(), message.as_ptr());
    }
}

impl log::Log for CoreLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let max = if DEBUG_TARGETS
            .iter()
            .any(|t| metadata.target().starts_with(t))
        {
            Level::Debug
        } else {
            Level::Warn
        };
        metadata.level() <= max
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} [{}][{}] {}\n",
            utc_timestamp(),
            record.level(),
            record.target(),
            record.args()
        );
        mirror(record.level(), &line);
        let Some(dir) = &self.dir else { return };
        let mut guard = self.file.lock();
        if guard.is_none() {
            *guard = Self::open(dir);
        }
        let Some(state) = guard.as_mut() else { return };
        if state.written + line.len() as u64 > LOG_FILE_MAX_BYTES {
            match self.rotate(dir) {
                Some(fresh) => *state = fresh,
                None => return,
            }
        }
        if state.file.write_all(line.as_bytes()).is_ok() {
            state.written += line.len() as u64;
        }
    }

    fn flush(&self) {
        if let Some(state) = self.file.lock().as_mut() {
            let _ = state.file.flush();
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` from the civil-from-days algorithm (Howard Hinnant) —
/// exact for the Gregorian calendar, no external time crate.
fn utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Install the logger (idempotent — later calls are no-ops, matching the
/// process-global `log` facade).
pub(crate) fn install(log_dir: Option<String>) {
    let logger = CoreLogger {
        dir: log_dir.map(PathBuf::from),
        file: Mutex::new(None),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Debug);
    }
}
