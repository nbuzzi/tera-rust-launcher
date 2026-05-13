//! Persistent file logging for the launcher.
//!
//! Why: when users report "no me carga la lista de servers" or "se cuelga el
//! launcher", we have no record of what actually happened in the Win32
//! handshake path between us and TERA.exe. The frontend log stream is
//! ephemeral and gets cleared when the user closes the window. This module
//! writes every `log::*!` call to `%APPDATA%\teralaunch\launcher.log` with a
//! timestamp + level + target, rotating once the file exceeds ~1 MB.
//!
//! It wraps an inner `log::Log` (the teralib TeraLogger that feeds the
//! frontend) so behaviour for the existing UI log stream is unchanged.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Log, Metadata, Record};

const MAX_LOG_BYTES: u64 = 1_048_576; // 1 MB before rotation

/// A log::Log implementation that:
/// - Forwards records whose target starts with "teralib" to the inner logger
///   (which feeds the frontend via mpsc), to preserve existing UI behaviour.
/// - Writes ALL records (any target, any level <= max) to a file in APPDATA.
/// - Also mirrors to stderr in debug builds.
pub struct FileAndChannelLogger {
    inner: Box<dyn Log>,
    file: Mutex<Option<File>>,
    path: PathBuf,
}

impl FileAndChannelLogger {
    pub fn new(inner: Box<dyn Log>) -> Self {
        let path = log_file_path();
        // Make sure the directory exists.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Rotate if file is already too big from a previous run.
        rotate_if_needed(&path);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        // Write a session banner so each launch is easy to find in the log.
        if let Some(f) = file.as_ref() {
            let mut f = f;
            let _ = writeln!(
                f,
                "\n========== launcher session started {} ==========\n\
                 build: teralaunch (debug={}) pid={} os={}\n",
                format_timestamp(),
                cfg!(debug_assertions),
                std::process::id(),
                std::env::consts::OS
            );
        }
        FileAndChannelLogger {
            inner,
            file: Mutex::new(file),
            path,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
}

/// Returns true for log records we actually care about persisting to disk.
///
/// Why: Tauri/wry/tao use `tracing` and bridge to the `log` crate. Every IPC
/// roundtrip emits a span whose Debug repr serializes the *entire* Window
/// struct (icon bytes, plugin store, event listener map, etc.). One click can
/// produce ~800 KB on a single line with no newlines, which makes the file
/// useless for diagnosing launcher failures. We keep only our own crates'
/// records.
fn target_is_app(target: &str) -> bool {
    target.starts_with("teralib")
        || target.starts_with("teralaunch")
        || target.starts_with("launcher")
        || target.starts_with("teralauncher")
        || target.starts_with("file_log")
        || target.starts_with("self_update")
        || target.starts_with("language")
        || target.starts_with("optimizer")
}

/// Hard cap on a single formatted log line written to disk. Anything longer
/// is truncated with a marker. Prevents a single bad record from blowing
/// past the rotation budget in one go.
const MAX_LINE_BYTES: usize = 8 * 1024;

impl Log for FileAndChannelLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Accept Warn/Error from anything (so we catch unexpected failures from
        // tauri/wry/reqwest/etc.), but Info/Debug only from our own crates.
        match metadata.level() {
            log::Level::Error | log::Level::Warn => true,
            _ => target_is_app(metadata.target()),
        }
    }

    fn log(&self, record: &Record) {
        // Mirror to inner first so frontend keeps receiving teralib logs.
        if self.inner.enabled(record.metadata()) {
            self.inner.log(record);
        }
        if !self.enabled(record.metadata()) {
            return;
        }
        let raw = format!(
            "{ts} [{lvl:<5}] [{target}] {msg}",
            ts = format_timestamp(),
            lvl = record.level(),
            target = record.target(),
            msg = record.args()
        );
        let line = if raw.len() > MAX_LINE_BYTES {
            let mut s = raw;
            s.truncate(MAX_LINE_BYTES);
            s.push_str(" ...[truncated]");
            s
        } else {
            raw
        };
        // Mirror to stderr in debug.
        #[cfg(debug_assertions)]
        {
            eprintln!("{}", line);
        }
        if let Ok(mut guard) = self.file.lock() {
            // Decide if we need to rotate before doing anything else, so we
            // can drop the lock cleanly before the rotation work.
            let needs_rotate = guard
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map(|md| md.len() > MAX_LOG_BYTES)
                .unwrap_or(false);
            if needs_rotate {
                // Drop the current handle so Windows lets us rename the file.
                *guard = None;
                drop(guard);
                rotate_if_needed(&self.path);
                if let Ok(mut g2) = self.file.lock() {
                    *g2 = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)
                        .ok();
                    if let Some(f2) = g2.as_mut() {
                        let _ = writeln!(f2, "{}", line);
                        let _ = f2.flush();
                    }
                }
                return;
            }
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{}", line);
                let _ = f.flush();
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

/// Computes `%APPDATA%\teralaunch\launcher.log` (falls back to temp dir).
pub fn log_file_path() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        PathBuf::from(appdata).join("teralaunch").join("launcher.log")
    } else {
        std::env::temp_dir().join("teralaunch-launcher.log")
    }
}

/// Returns the directory holding launcher.log.
pub fn log_dir() -> PathBuf {
    log_file_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
}

fn rotate_if_needed(path: &PathBuf) {
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > MAX_LOG_BYTES {
            let backup = path.with_extension("log.1");
            let _ = std::fs::remove_file(&backup);
            let _ = std::fs::rename(path, &backup);
        }
    }
}

/// Minimal YYYY-MM-DD HH:MM:SS.sss formatter (no chrono dependency).
fn format_timestamp() -> String {
    let now = SystemTime::now();
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let total_secs = dur.as_secs();
    let millis = dur.subsec_millis();

    // Use local time offset from the system. We avoid pulling chrono in;
    // accuracy of a few hours due to DST is acceptable for diagnostics.
    // Get offset via WinAPI GetTimeZoneInformation; if it fails, log UTC.
    let offset_minutes = local_offset_minutes().unwrap_or(0);
    let adj = total_secs as i64 + (offset_minutes as i64) * 60;
    let (year, month, day, hour, minute, second) = seconds_to_ymdhms(adj);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        year, month, day, hour, minute, second, millis
    )
}

#[cfg(target_os = "windows")]
fn local_offset_minutes() -> Option<i32> {
    use std::mem::MaybeUninit;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(
            lpTimeZoneInformation: *mut TimeZoneInformation,
        ) -> u32;
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: [u16; 8],
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: [u16; 8],
        daylight_bias: i32,
    }
    unsafe {
        let mut tz = MaybeUninit::<TimeZoneInformation>::zeroed();
        let result = GetTimeZoneInformation(tz.as_mut_ptr());
        let tz = tz.assume_init();
        // result: 0=unknown, 1=standard, 2=daylight
        let total_bias = match result {
            2 => tz.bias + tz.daylight_bias,
            1 => tz.bias + tz.standard_bias,
            _ => tz.bias,
        };
        // Bias is in minutes WEST of UTC; offset east = -bias.
        Some(-total_bias)
    }
}

#[cfg(not(target_os = "windows"))]
fn local_offset_minutes() -> Option<i32> {
    None
}

fn seconds_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days_from_epoch = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;

    // Convert days since 1970-01-01 to YMD using the well-known algorithm.
    // 1970-01-01 was a Thursday; days_from_epoch=0 -> 1970-01-01.
    let z = days_from_epoch + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut year = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    if month <= 2 {
        year += 1;
    }
    (year, month, day, hour, minute, second)
}
