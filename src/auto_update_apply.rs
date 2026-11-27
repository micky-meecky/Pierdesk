//! Console-free pure-Rust self-update applier, health verifier, and automatic rollback.
//!
//! Works for portable single-file executables without PowerShell or external scripts:
//! 1. Copies the current executable into a temporary helper location under the per-user data dir.
//! 2. Spawns the helper in `self-update-apply` mode (directly with `CREATE_NO_WINDOW`, or via
//!    `runas` only when the install directory requires elevation and the user explicitly triggers
//!    the update).
//! 3. Waits for the parent tray process to exit cleanly, re-verifies the staged payload SHA-256,
//!    atomically swaps the target executable while preserving `pierdesk.previous.exe`, launches
//!    the new binary, and confirms its live tray window + startup identity receipt.
//! 4. Automatically rolls back to `pierdesk.previous.exe` and restarts the prior version if
//!    replacement or post-launch health verification fails.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LPARAM, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_SYNCHRONIZE, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, HWND_MESSAGE, PostMessageW, SMTO_ABORTIFHUNG, SW_HIDE,
    SendMessageTimeoutW, WM_APP, WM_NULL,
};
use windows_core::{PCWSTR, w};

use crate::app_settings::{AutoUpdateMode, AutoUpdateSettings};
use crate::auto_update::{
    AutoUpdateConfig, PendingStagedUpdate, hash_file_sha256, helper_dir, load_persistent_state,
    load_verified_pending, now_unix_secs, record_rolled_back_version, staged_dir,
    target_directory_is_writable, validate_sha256_hex,
};
use crate::device_store::DeviceStore;

const UPDATE_MUTEX_NAME: PCWSTR = w!("Local\\ClipFerry.Update.v1");
const TRAY_WINDOW_CLASS: PCWSTR = w!("PierdeskTrayWindow");
const TRAY_EXIT_MESSAGE: u32 = WM_APP + 3;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const PARENT_EXIT_GRACE: Duration = Duration::from_secs(5);
const PARENT_EXIT_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(25);
const HEALTH_STABLE_DURATION: Duration = Duration::from_secs(2);
const DEFAULT_IDLE_APPLY_THRESHOLD: Duration = Duration::from_mins(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyRequest {
    pub parent_pid: u32,
    pub target_exe: PathBuf,
    pub staged_exe: PathBuf,
    pub expected_sha256: String,
    pub target_version: String,
    pub store_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupApplyOutcome {
    ContinueNormalStartup,
    HandedOffToUpdater,
}

/// Returns how long Windows has seen no keyboard/mouse input in this desktop session.
#[must_use]
pub fn system_idle_duration() -> Duration {
    let mut info = LASTINPUTINFO {
        cbSize: u32::try_from(size_of::<LASTINPUTINFO>()).unwrap_or(0),
        dwTime: 0,
    };
    // SAFETY: `info` is properly sized and writable for `GetLastInputInfo`.
    let ok = unsafe { GetLastInputInfo(&raw mut info) }.as_bool();
    if !ok {
        return Duration::ZERO;
    }
    // SAFETY: `GetTickCount` returns the wrapping 32-bit tick counter matching `LASTINPUTINFO.dwTime`.
    let now = unsafe { GetTickCount() };
    Duration::from_millis(u64::from(now.wrapping_sub(info.dwTime)))
}

/// Returns the idle duration required before an automatic silent update swaps the running tray.
#[must_use]
pub const fn auto_apply_idle_threshold() -> Duration {
    DEFAULT_IDLE_APPLY_THRESHOLD
}

/// Checks during early tray startup whether a verified staged update is ready to be applied
/// silently before the tray finishes initializing (delivering the "next launch updates silently"
/// experience).
#[must_use]
pub fn try_apply_pending_on_startup(store: &DeviceStore, log_path: &Path) -> StartupApplyOutcome {
    let config = AutoUpdateConfig::for_current_process();
    if !config.is_configured() {
        return StartupApplyOutcome::ContinueNormalStartup;
    }
    let settings = AutoUpdateSettings::load_or_default(store).unwrap_or_default();
    if settings.mode != AutoUpdateMode::Silent {
        return StartupApplyOutcome::ContinueNormalStartup;
    }
    let Some(pending) = load_verified_pending(store.root(), &config) else {
        return StartupApplyOutcome::ContinueNormalStartup;
    };
    let state = load_persistent_state(store.root());
    if state
        .rolled_back_versions
        .iter()
        .any(|bad| bad == &pending.version)
    {
        return StartupApplyOutcome::ContinueNormalStartup;
    }
    if !target_directory_is_writable(&config.target_executable) {
        return StartupApplyOutcome::ContinueNormalStartup;
    }
    match launch_self_update(store, &pending, false) {
        Ok(()) => {
            let _ = crate::logging::append(
                log_path,
                &format!(
                    "auto_update_startup_handoff version={} pid={}",
                    pending.version,
                    std::process::id()
                ),
            );
            StartupApplyOutcome::HandedOffToUpdater
        }
        Err(error) => {
            let _ = crate::logging::append(
                log_path,
                &format!("auto_update_startup_handoff_failed error={error}"),
            );
            StartupApplyOutcome::ContinueNormalStartup
        }
    }
}

/// Copies the running executable to an isolated helper path and launches `self-update-apply`.
///
/// The caller should exit its tray loop immediately after this returns `Ok(())`.
///
/// # Errors
///
/// Returns an error if the helper cannot be copied or spawned (or if the user cancels UAC).
pub fn launch_self_update(
    store: &DeviceStore,
    pending: &PendingStagedUpdate,
    require_elevation: bool,
) -> io::Result<()> {
    validate_sha256_hex(&pending.sha256)?;
    let target_exe = std::env::current_exe()?;
    let staged_exe = PathBuf::from(&pending.staged_path);
    if !staged_exe.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "staged update executable is missing",
        ));
    }
    let helper_root = helper_dir(store.root());
    fs::create_dir_all(&helper_root)?;
    let helper_exe = helper_root.join(format!(
        "pierdesk-updater-{}-{}.exe",
        std::process::id(),
        now_unix_secs()
    ));
    fs::copy(&target_exe, &helper_exe)?;

    let parent_pid = std::process::id().to_string();
    let args = [
        "self-update-apply".to_owned(),
        "--parent-pid".to_owned(),
        parent_pid,
        "--target".to_owned(),
        target_exe.to_string_lossy().into_owned(),
        "--staged".to_owned(),
        staged_exe.to_string_lossy().into_owned(),
        "--sha256".to_owned(),
        pending.sha256.to_ascii_uppercase(),
        "--version".to_owned(),
        pending.version.clone(),
        "--store-root".to_owned(),
        store.root().to_string_lossy().into_owned(),
    ];

    if require_elevation {
        let joined = join_windows_command_line(&args);
        let exe_wide = wide_null(helper_exe.as_os_str());
        let params_wide = wide_null(OsStr::new(&joined));
        // SAFETY: strings are live, null-terminated UTF-16 buffers for the synchronous call.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("runas"),
                PCWSTR(exe_wide.as_ptr()),
                PCWSTR(params_wide.as_ptr()),
                None,
                SW_HIDE,
            )
        };
        if result.0 as isize <= 32 {
            let _ = fs::remove_file(&helper_exe);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "elevation was cancelled or failed (ShellExecute={})",
                    result.0 as isize
                ),
            ));
        }
        return Ok(());
    }

    let mut command = Command::new(&helper_exe);
    command
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Err(error) = command.spawn() {
        let _ = fs::remove_file(&helper_exe);
        return Err(error);
    }
    Ok(())
}

/// CLI entrypoint for `pierdesk self-update-apply ...`.
///
/// # Errors
///
/// Returns an error if arguments are invalid or the update cannot complete (after attempting
/// rollback when replacement had already started).
pub fn run_cli(arguments: &[String]) -> Result<(), String> {
    let request = parse_apply_args(arguments).map_err(|error| error.to_string())?;
    apply_update(&request).map_err(|error| error.to_string())
}

fn parse_apply_args(arguments: &[String]) -> io::Result<ApplyRequest> {
    let mut parent_pid: Option<u32> = None;
    let mut target_exe: Option<PathBuf> = None;
    let mut staged_exe: Option<PathBuf> = None;
    let mut expected_sha256: Option<String> = None;
    let mut target_version: Option<String> = None;
    let mut store_root: Option<PathBuf> = None;

    let mut index = 0;
    while index < arguments.len() {
        let flag = arguments[index].as_str();
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| invalid_input(format!("missing value for {flag}")))?;
        match flag {
            "--parent-pid" => {
                parent_pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| invalid_input("invalid --parent-pid"))?,
                );
            }
            "--target" => target_exe = Some(PathBuf::from(value)),
            "--staged" => staged_exe = Some(PathBuf::from(value)),
            "--sha256" => {
                validate_sha256_hex(value)?;
                expected_sha256 = Some(value.to_ascii_uppercase());
            }
            "--version" => target_version = Some(value.clone()),
            "--store-root" => store_root = Some(PathBuf::from(value)),
            _ => return Err(invalid_input(format!("unknown self-update flag: {flag}"))),
        }
        index += 2;
    }

    Ok(ApplyRequest {
        parent_pid: parent_pid.ok_or_else(|| invalid_input("missing --parent-pid"))?,
        target_exe: target_exe.ok_or_else(|| invalid_input("missing --target"))?,
        staged_exe: staged_exe.ok_or_else(|| invalid_input("missing --staged"))?,
        expected_sha256: expected_sha256.ok_or_else(|| invalid_input("missing --sha256"))?,
        target_version: target_version.ok_or_else(|| invalid_input("missing --version"))?,
        store_root: store_root.ok_or_else(|| invalid_input("missing --store-root"))?,
    })
}

struct UpdateMutexGuard {
    handle: HANDLE,
}

impl UpdateMutexGuard {
    fn acquire() -> io::Result<Self> {
        // SAFETY: constant named mutex with no security descriptor.
        let handle = unsafe { CreateMutexW(None, true, UPDATE_MUTEX_NAME) }
            .map_err(|error| io::Error::other(error.to_string()))?;
        // SAFETY: immediate GetLastError read on the same thread.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: close non-owned handle.
            let _ = unsafe { CloseHandle(handle) };
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another Pierdesk update is already running",
            ));
        }
        Ok(Self { handle })
    }
}

impl Drop for UpdateMutexGuard {
    fn drop(&mut self) {
        // SAFETY: this guard owns the mutex handle.
        let _ = unsafe { ReleaseMutex(self.handle) };
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

fn apply_update(request: &ApplyRequest) -> io::Result<()> {
    let _mutex = UpdateMutexGuard::acquire()?;
    let log_path = request.store_root.join("logs").join("clipferry.log");
    let _ = crate::logging::append(
        &log_path,
        &format!(
            "auto_update_apply_start version={} parent_pid={} target=\"{}\"",
            request.target_version,
            request.parent_pid,
            request.target_exe.display()
        ),
    );

    verify_pe_and_hash(&request.staged_exe, &request.expected_sha256)?;
    wait_for_parent_exit(request.parent_pid)?;

    let old_sha256 = hash_file_sha256(&request.target_exe)
        .map(|(hash, _)| hash)
        .unwrap_or_default();
    let backup_path = backup_path_for(&request.target_exe)?;

    let swap_result = replace_executable_atomically(
        &request.staged_exe,
        &request.target_exe,
        &backup_path,
        &request.expected_sha256,
    );
    if let Err(error) = swap_result {
        let _ = crate::logging::append(
            &log_path,
            &format!(
                "auto_update_replace_failed version={} error={error}",
                request.target_version
            ),
        );
        // Ensure the previous version is still running if we already stopped it.
        let _ = spawn_tray(&request.target_exe);
        write_update_receipt(
            &request.store_root,
            "failed",
            "replace_failed",
            request,
            &old_sha256,
            None,
            Some(&error.to_string()),
        );
        return Err(error);
    }

    match launch_and_verify_new_tray(request) {
        Ok(new_pid) => {
            let _ = fs::remove_dir_all(staged_dir(&request.store_root));
            let _ = crate::logging::append(
                &log_path,
                &format!(
                    "auto_update_apply_succeeded version={} new_pid={} backup=\"{}\"",
                    request.target_version,
                    new_pid,
                    backup_path.display()
                ),
            );
            write_update_receipt(
                &request.store_root,
                "succeeded",
                "updated",
                request,
                &old_sha256,
                Some(new_pid),
                None,
            );
            Ok(())
        }
        Err(error) => {
            let _ = crate::logging::append(
                &log_path,
                &format!(
                    "auto_update_health_failed version={} error={error} action=rollback",
                    request.target_version
                ),
            );
            let rollback_result = rollback_executable(&request.target_exe, &backup_path);
            let restarted_pid = if rollback_result.is_ok() {
                spawn_tray(&request.target_exe).ok().map(|child| child.id())
            } else {
                None
            };
            record_rolled_back_version(&request.store_root, &request.target_version);
            let _ = fs::remove_dir_all(staged_dir(&request.store_root));
            let outcome = if rollback_result.is_ok() {
                "rolled_back"
            } else {
                "rollback_failed"
            };
            write_update_receipt(
                &request.store_root,
                "failed",
                outcome,
                request,
                &old_sha256,
                restarted_pid,
                Some(&error.to_string()),
            );
            Err(error)
        }
    }
}

fn verify_pe_and_hash(path: &Path, expected_sha256: &str) -> io::Result<()> {
    let mut header = [0_u8; 2];
    let mut file = File::open(path)?;
    file.read_exact(&mut header)?;
    drop(file);
    if header != *b"MZ" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged update file does not have a valid Windows PE header",
        ));
    }
    let (actual_hash, _) = hash_file_sha256(path)?;
    if !actual_hash.eq_ignore_ascii_case(expected_sha256) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged update SHA-256 mismatch (expected {expected_sha256}, got {actual_hash})"
            ),
        ));
    }
    Ok(())
}

fn wait_for_parent_exit(parent_pid: u32) -> io::Result<()> {
    if parent_pid == 0 {
        return Ok(());
    }
    // SAFETY: request wait-only access to the parent process handle.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent_pid) }) else {
        // Parent has already exited.
        return Ok(());
    };
    let wait_once = |duration: Duration| -> bool {
        let ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `handle` is valid and owned here.
        let status = unsafe { WaitForSingleObject(handle, ms) };
        status == WAIT_OBJECT_0
    };
    let exited = if wait_once(PARENT_EXIT_GRACE) {
        true
    } else {
        post_tray_exit_for_pid(parent_pid);
        wait_once(PARENT_EXIT_TOTAL_TIMEOUT.saturating_sub(PARENT_EXIT_GRACE))
    };
    // SAFETY: close the handle opened above.
    let _ = unsafe { CloseHandle(handle) };
    if exited {
        // Give the OS loader a tiny breath to release the file mapping lock.
        thread::sleep(Duration::from_millis(150));
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "previous Pierdesk process did not exit in time for update",
        ))
    }
}

fn post_tray_exit_for_pid(expected_pid: u32) {
    // SAFETY: probing the hidden message-only tray window class is read-only.
    let Ok(window) = (unsafe { FindWindowExW(Some(HWND_MESSAGE), None, TRAY_WINDOW_CLASS, None) })
    else {
        return;
    };
    if window.0.is_null() {
        return;
    }
    let mut owner_pid = 0_u32;
    // SAFETY: `window` is a valid HWND and `owner_pid` is writable.
    let _ = unsafe { GetWindowThreadProcessId(window, Some(&raw mut owner_pid)) };
    if owner_pid == expected_pid {
        // SAFETY: posting our own exit message asks the tray loop to shut down cleanly.
        let _ = unsafe { PostMessageW(Some(window), TRAY_EXIT_MESSAGE, WPARAM(0), LPARAM(0)) };
    }
}

/// Computes the companion rollback backup path in the same directory as the target executable.
///
/// # Errors
///
/// Returns an error when `target_exe` has no parent directory.
pub fn backup_path_for(target_exe: &Path) -> io::Result<PathBuf> {
    let parent = target_exe
        .parent()
        .ok_or_else(|| invalid_input("target executable has no parent directory"))?;
    let stem = target_exe
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("pierdesk");
    Ok(parent.join(format!("{stem}.previous.exe")))
}

/// Atomically swaps `staged_exe` into `target_exe`, preserving the prior binary at `backup_path`.
///
/// # Errors
///
/// Returns an error if copying, checksum verification, or atomic replacement fails.
pub fn replace_executable_atomically(
    staged_exe: &Path,
    target_exe: &Path,
    backup_path: &Path,
    expected_sha256: &str,
) -> io::Result<()> {
    let parent = target_exe
        .parent()
        .ok_or_else(|| invalid_input("target executable has no parent directory"))?;
    let temp_target = parent.join(format!(".pierdesk.update.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temp_target);

    let result = (|| -> io::Result<()> {
        fs::copy(staged_exe, &temp_target)?;
        let (temp_hash, _) = hash_file_sha256(&temp_target)?;
        if !temp_hash.eq_ignore_ascii_case(expected_sha256) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "copied temporary executable checksum mismatch",
            ));
        }

        // Retry briefly in case antivirus or Windows loader still holds a transient lock.
        let mut last_error = None;
        for attempt in 0..10 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(250));
            }
            match atomic_swap_files(&temp_target, target_exe, backup_path) {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }
        if let Some(error) = last_error {
            return Err(error);
        }

        let (installed_hash, _) = hash_file_sha256(target_exe)?;
        if !installed_hash.eq_ignore_ascii_case(expected_sha256) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "installed executable checksum mismatch after swap",
            ));
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_target);
    }
    result
}

fn atomic_swap_files(replacement: &Path, target: &Path, backup: &Path) -> io::Result<()> {
    if target.exists() {
        let _ = fs::remove_file(backup);
        let target_w = wide_null(target.as_os_str());
        let replacement_w = wide_null(replacement.as_os_str());
        let backup_w = wide_null(backup.as_os_str());
        // SAFETY: all three UTF-16 paths are live and null-terminated for the call duration.
        unsafe {
            ReplaceFileW(
                PCWSTR(target_w.as_ptr()),
                PCWSTR(replacement_w.as_ptr()),
                PCWSTR(backup_w.as_ptr()),
                REPLACEFILE_IGNORE_MERGE_ERRORS,
                None,
                None,
            )
        }
        .map_err(|error| io::Error::other(format!("ReplaceFileW failed: {error}")))
    } else {
        let replacement_w = wide_null(replacement.as_os_str());
        let target_w = wide_null(target.as_os_str());
        // SAFETY: both UTF-16 paths are live and null-terminated.
        unsafe {
            MoveFileExW(
                PCWSTR(replacement_w.as_ptr()),
                PCWSTR(target_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| io::Error::other(format!("MoveFileExW failed: {error}")))
    }
}

/// Restores `backup_path` back onto `target_exe` after a failed launch or health verification.
///
/// # Errors
///
/// Returns an error if the backup does not exist or cannot replace the target.
pub fn rollback_executable(target_exe: &Path, backup_path: &Path) -> io::Result<()> {
    if !backup_path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "previous executable backup is missing",
        ));
    }
    let backup_w = wide_null(backup_path.as_os_str());
    let target_w = wide_null(target_exe.as_os_str());
    let mut last_error = None;
    for attempt in 0..10 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(250));
        }
        // SAFETY: both UTF-16 paths are live and null-terminated.
        let moved = unsafe {
            MoveFileExW(
                PCWSTR(backup_w.as_ptr()),
                PCWSTR(target_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        match moved {
            Ok(()) => {
                last_error = None;
                break;
            }
            Err(error) => {
                last_error = Some(io::Error::other(format!(
                    "rollback MoveFileExW failed: {error}"
                )));
            }
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    Ok(())
}

fn spawn_tray(executable: &Path) -> io::Result<Child> {
    let mut command = Command::new(executable);
    command
        .arg("tray")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn()
}

fn launch_and_verify_new_tray(request: &ApplyRequest) -> io::Result<u32> {
    let mut child = spawn_tray(&request.target_exe)?;
    let child_pid = child.id();
    let deadline = Instant::now() + HEALTH_CHECK_TIMEOUT;
    let mut healthy_since: Option<Instant> = None;

    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "updated process {child_pid} exited prematurely with {status}"
            )));
        }
        if is_tray_responsive_for_pid(child_pid)
            && verify_startup_receipt(&request.store_root, child_pid, request)
        {
            let since = *healthy_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= HEALTH_STABLE_DURATION {
                return Ok(child_pid);
            }
        } else {
            healthy_since = None;
        }
        thread::sleep(Duration::from_millis(250));
    }

    // Health check timed out: stop the unhealthy process before rolling back.
    post_tray_exit_for_pid(child_pid);
    thread::sleep(Duration::from_secs(2));
    if child.try_wait()?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("updated process {child_pid} did not pass tray/identity readiness check"),
    ))
}

fn is_tray_responsive_for_pid(expected_pid: u32) -> bool {
    // SAFETY: locating the hidden message-only tray window is read-only.
    let Ok(window) = (unsafe { FindWindowExW(Some(HWND_MESSAGE), None, TRAY_WINDOW_CLASS, None) })
    else {
        return false;
    };
    if window.0.is_null() {
        return false;
    }
    let mut owner_pid = 0_u32;
    // SAFETY: `window` is a live HWND returned by FindWindowExW.
    let _ = unsafe { GetWindowThreadProcessId(window, Some(&raw mut owner_pid)) };
    if owner_pid != expected_pid {
        return false;
    }
    let mut ignored = 0_usize;
    // SAFETY: WM_NULL with SMTO_ABORTIFHUNG probes message-loop responsiveness without side effects.
    let result = unsafe {
        SendMessageTimeoutW(
            window,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            1_000,
            Some(&raw mut ignored),
        )
    };
    result.0 != 0
}

fn verify_startup_receipt(store_root: &Path, child_pid: u32, request: &ApplyRequest) -> bool {
    let receipt_path = store_root
        .join("runtime")
        .join(format!("tray-{child_pid}.json"));
    let Ok(bytes) = fs::read(receipt_path) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let pid_ok = receipt["processId"].as_u64() == Some(u64::from(child_pid));
    let version_ok = receipt["version"].as_str() == Some(request.target_version.as_str());
    let hash_ok = receipt["sha256"]
        .as_str()
        .is_some_and(|hash| hash.eq_ignore_ascii_case(&request.expected_sha256));
    let path_ok = receipt["executablePath"].as_str().is_some_and(|recorded| {
        recorded.eq_ignore_ascii_case(&request.target_exe.to_string_lossy())
    });
    pid_ok && version_ok && hash_ok && path_ok
}

fn write_update_receipt(
    store_root: &Path,
    state: &str,
    outcome: &str,
    request: &ApplyRequest,
    old_sha256: &str,
    new_pid: Option<u32>,
    error: Option<&str>,
) {
    let history = store_root.join("update-history");
    if fs::create_dir_all(&history).is_err() {
        return;
    }
    let timestamp = now_unix_secs();
    let path = history.join(format!(
        "auto-update-{timestamp}-{}.json",
        std::process::id()
    ));
    let payload = serde_json::json!({
        "schemaVersion": 2,
        "source": "auto_update",
        "timeUnix": timestamp,
        "state": state,
        "outcome": outcome,
        "targetVersion": request.target_version,
        "targetPath": request.target_exe,
        "oldSha256": old_sha256,
        "expectedSha256": request.expected_sha256,
        "processId": new_pid,
        "error": error,
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&payload) {
        let _ = fs::write(path, bytes);
    }
}

fn join_windows_command_line(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|ch| matches!(ch, ' ' | '\t' | '"')) {
        return arg.to_owned();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0_usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            for _ in 0..=(backslashes * 2) {
                quoted.push('\\');
            }
            quoted.push('"');
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                quoted.push('\\');
            }
            backslashes = 0;
            quoted.push(ch);
        }
    }
    for _ in 0..(backslashes * 2) {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use sha2::{Digest as _, Sha256};

    use super::*;

    #[test]
    fn atomic_replace_creates_backup_and_rollback_restores_previous_binary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pierdesk.exe");
        let staged = dir.path().join("staged.exe");
        let backup = backup_path_for(&target).unwrap();

        let old_bytes = b"MZold-version-binary-content";
        let new_bytes = b"MZnew-version-binary-content-v2";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&staged, new_bytes).unwrap();
        let new_hash = format!("{:X}", Sha256::digest(new_bytes));

        replace_executable_atomically(&staged, &target, &backup, &new_hash).unwrap();
        assert_eq!(fs::read(&target).unwrap(), new_bytes);
        assert_eq!(fs::read(&backup).unwrap(), old_bytes);

        rollback_executable(&target, &backup).unwrap();
        assert_eq!(fs::read(&target).unwrap(), old_bytes);
        assert!(!backup.exists());
    }

    #[test]
    fn replace_rejects_mismatched_checksum_and_preserves_original_binary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pierdesk.exe");
        let staged = dir.path().join("staged.exe");
        let backup = backup_path_for(&target).unwrap();

        let old_bytes = b"MZoriginal-binary";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&staged, b"MZdifferent-binary").unwrap();
        let wrong_hash = "A".repeat(64);

        assert!(replace_executable_atomically(&staged, &target, &backup, &wrong_hash).is_err());
        assert_eq!(fs::read(&target).unwrap(), old_bytes);
        assert!(!backup.exists());
    }

    #[test]
    fn parse_apply_args_validates_all_required_fields() {
        let hash = "0123456789ABCDEF".repeat(4);
        let args = vec![
            "--parent-pid".into(),
            "1234".into(),
            "--target".into(),
            r"C:\Apps\pierdesk.exe".into(),
            "--staged".into(),
            r"C:\Temp\pierdesk-1.0.65.exe".into(),
            "--sha256".into(),
            hash.clone(),
            "--version".into(),
            "1.0.65".into(),
            "--store-root".into(),
            r"C:\Users\Test\AppData\Local\ClipFerry".into(),
        ];
        let parsed = parse_apply_args(&args).unwrap();
        assert_eq!(parsed.parent_pid, 1234);
        assert_eq!(parsed.expected_sha256, hash);
        assert_eq!(parsed.target_version, "1.0.65");

        // Missing required flag must fail.
        assert!(parse_apply_args(&args[..4]).is_err());
    }

    #[test]
    fn windows_argument_quoting_handles_spaces_and_quotes() {
        let args = vec![
            "self-update-apply".to_owned(),
            "--target".to_owned(),
            r"C:\Program Files\Pierdesk\pierdesk.exe".to_owned(),
        ];
        assert_eq!(
            join_windows_command_line(&args),
            r#"self-update-apply --target "C:\Program Files\Pierdesk\pierdesk.exe""#
        );
    }
}
