use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};

const TIME_ZONE_ID_UNKNOWN: u32 = 0;
const TIME_ZONE_ID_STANDARD: u32 = 1;
const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
const MAIN_LOG_FILE: &str = "clipferry.log";
const RETENTION: Duration = Duration::from_hours(30 * 24);

#[derive(Clone, Copy, Debug)]
struct SessionIdentity {
    start_hour: u16,
    start_minute: u16,
    start_second: u16,
    start_millisecond: u16,
    process_id: u32,
}

impl SessionIdentity {
    fn current() -> Self {
        // SAFETY: GetLocalTime returns a process-independent clock snapshot.
        let local_time = unsafe { GetLocalTime() };
        Self {
            start_hour: local_time.wHour,
            start_minute: local_time.wMinute,
            start_second: local_time.wSecond,
            start_millisecond: local_time.wMilliseconds,
            process_id: std::process::id(),
        }
    }
}

fn session_identity() -> SessionIdentity {
    static IDENTITY: OnceLock<SessionIdentity> = OnceLock::new();
    *IDENTITY.get_or_init(SessionIdentity::current)
}

/// Creates this process' current daily log and removes managed logs older than 30 days.
///
/// Calling this again after local midnight creates the next day's file while retaining the same
/// process-session suffix. Individual cleanup failures are deliberately ignored so a locked old
/// log can never prevent `Pierdesk` from starting or writing the current log.
pub(crate) fn ensure_current(path: &Path) -> io::Result<PathBuf> {
    let active = active_path(path);
    if let Some(parent) = active.parent() {
        fs::create_dir_all(parent)?;
        let _ = cleanup_expired_logs_at(parent, SystemTime::now());
    }
    OpenOptions::new().create(true).append(true).open(&active)?;
    Ok(active)
}

/// Resolves the legacy logical `clipferry.log` path to this process' current daily session log.
/// Explicit diagnostic paths keep their original names.
pub(crate) fn active_path(path: &Path) -> PathBuf {
    if path.file_name() != Some(OsStr::new(MAIN_LOG_FILE)) {
        return path.to_path_buf();
    }
    // SAFETY: GetLocalTime returns a process-independent clock snapshot.
    let local_time = unsafe { GetLocalTime() };
    active_path_at(path, local_time)
}

pub(crate) fn append(path: &Path, message: &str) -> io::Result<()> {
    // SAFETY: GetLocalTime returns a process-independent clock snapshot.
    let local_time = unsafe { GetLocalTime() };
    let active = active_path_at(path, local_time);
    if let Some(parent) = active.parent() {
        fs::create_dir_all(parent)?;
    }
    let unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let offset_minutes = current_utc_offset_minutes();
    let timestamp = format_timestamp(local_time, offset_minutes, unix_millis);
    let mut file = OpenOptions::new().create(true).append(true).open(active)?;
    // One append operation keeps concurrent worker records from interleaving.
    file.write_all(format!("{timestamp} {message}\n").as_bytes())
}

fn active_path_at(path: &Path, local_time: SYSTEMTIME) -> PathBuf {
    if path.file_name() == Some(OsStr::new(MAIN_LOG_FILE)) {
        session_log_path(path, local_time, session_identity())
    } else {
        path.to_path_buf()
    }
}

fn session_log_path(path: &Path, local_time: SYSTEMTIME, session: SessionIdentity) -> PathBuf {
    let file_name = format!(
        "clipferry-{:04}-{:02}-{:02}-s{:02}{:02}{:02}{:03}-p{}.log",
        local_time.wYear,
        local_time.wMonth,
        local_time.wDay,
        session.start_hour,
        session.start_minute,
        session.start_second,
        session.start_millisecond,
        session.process_id,
    );
    path.with_file_name(file_name)
}

fn cleanup_expired_logs_at(directory: &Path, now: SystemTime) -> io::Result<usize> {
    let Some(cutoff) = now.checked_sub(RETENTION) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !is_managed_log(&path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.modified().is_ok_and(|modified| modified >= cutoff) {
            continue;
        }
        if fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn is_managed_log(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    name == MAIN_LOG_FILE
        || (name.starts_with("clipferry-")
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("log")))
}

fn current_utc_offset_minutes() -> Option<i32> {
    let mut information = TIME_ZONE_INFORMATION::default();
    // SAFETY: `information` is writable for the duration of this synchronous API call.
    let state = unsafe { GetTimeZoneInformation(&raw mut information) };
    let additional_bias = match state {
        TIME_ZONE_ID_UNKNOWN => 0,
        TIME_ZONE_ID_STANDARD => information.StandardBias,
        TIME_ZONE_ID_DAYLIGHT => information.DaylightBias,
        _ => return None,
    };
    information
        .Bias
        .checked_add(additional_bias)
        .and_then(i32::checked_neg)
}

fn format_timestamp(
    local_time: SYSTEMTIME,
    offset_minutes: Option<i32>,
    unix_millis: u128,
) -> String {
    let timezone = offset_minutes.map_or_else(
        || " time_zone=unknown".to_owned(),
        |minutes| {
            let sign = if minutes < 0 { '-' } else { '+' };
            let absolute = minutes.unsigned_abs();
            format!("{sign}{:02}:{:02}", absolute / 60, absolute % 60)
        },
    );
    format!(
        "time_local={:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{timezone} time_unix_ms={unix_millis}",
        local_time.wYear,
        local_time.wMonth,
        local_time.wDay,
        local_time.wHour,
        local_time.wMinute,
        local_time.wSecond,
        local_time.wMilliseconds,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    fn sample_time() -> SYSTEMTIME {
        SYSTEMTIME {
            wYear: 2026,
            wMonth: 9,
            wDay: 2,
            wHour: 12,
            wMinute: 34,
            wSecond: 56,
            wMilliseconds: 789,
            ..SYSTEMTIME::default()
        }
    }

    fn test_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pierdesk-log-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn timestamp_contains_local_milliseconds_offset_and_unix_milliseconds() {
        assert_eq!(
            format_timestamp(sample_time(), Some(8 * 60), 1_788_327_296_789),
            "time_local=2026-09-02T12:34:56.789+08:00 time_unix_ms=1788327296789"
        );
        assert_eq!(
            format_timestamp(sample_time(), Some(-5 * 60 - 30), 42),
            "time_local=2026-09-02T12:34:56.789-05:30 time_unix_ms=42"
        );
        assert_eq!(
            format_timestamp(sample_time(), None, 42),
            "time_local=2026-09-02T12:34:56.789 time_zone=unknown time_unix_ms=42"
        );
    }

    #[test]
    fn session_path_changes_by_day_and_keeps_start_identity() {
        let logical = Path::new("C:/logs/clipferry.log");
        let session = SessionIdentity {
            start_hour: 23,
            start_minute: 59,
            start_second: 58,
            start_millisecond: 123,
            process_id: 42,
        };
        let mut next_day = sample_time();
        next_day.wDay = 3;
        assert_eq!(
            session_log_path(logical, sample_time(), session),
            PathBuf::from("C:/logs/clipferry-2026-09-02-s235958123-p42.log")
        );
        assert_eq!(
            session_log_path(logical, next_day, session),
            PathBuf::from("C:/logs/clipferry-2026-09-03-s235958123-p42.log")
        );
        let next_start = SessionIdentity {
            start_millisecond: 124,
            ..session
        };
        assert_ne!(
            session_log_path(logical, sample_time(), session),
            session_log_path(logical, sample_time(), next_start)
        );
    }

    #[test]
    fn append_writes_to_a_session_file_instead_of_the_legacy_log() {
        let directory = test_directory("append");
        let logical = directory.join(MAIN_LOG_FILE);
        append(&logical, "test_event clean=true").unwrap();
        let active = active_path(&logical);

        assert!(!logical.exists());
        let line = std::fs::read_to_string(&active).unwrap();
        assert!(line.starts_with("time_local=20"));
        assert!(line.contains(" time_unix_ms="));
        assert!(line.ends_with(" test_event clean=true\n"));

        std::fs::remove_file(active).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn cleanup_removes_only_managed_logs_older_than_thirty_days() {
        let directory = test_directory("retention");
        fs::create_dir_all(&directory).unwrap();
        let old = directory.join("clipferry-2026-07-01-s010203004-p1.log");
        let recent = directory.join("clipferry-2026-09-01-s010203004-p1.log");
        let unrelated = directory.join("notes.log");
        fs::write(&old, b"old").unwrap();
        fs::write(&recent, b"recent").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let old_time = now - RETENTION - Duration::from_secs(1);
        let recent_time = now - RETENTION + Duration::from_secs(1);
        OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old_time))
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&recent)
            .unwrap()
            .set_times(FileTimes::new().set_modified(recent_time))
            .unwrap();

        assert_eq!(cleanup_expired_logs_at(&directory, now).unwrap(), 1);
        assert!(!old.exists());
        assert!(recent.exists());
        assert!(unrelated.exists());

        fs::remove_dir_all(directory).unwrap();
    }
}
