//! Startup identity and console-free update helper. Never injects desktop input.
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest as _, Sha256};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

/// Attach a CLI-only invocation to its parent's console, preserving redirected I/O.
pub fn attach_cli_console() {
    // SAFETY: handle inspection and optional parent-console attachment are process-local.
    unsafe {
        let redirected = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .iter()
            .any(|kind| GetStdHandle(*kind).is_ok_and(|handle| !handle.is_invalid()));
        if !redirected {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Launch the adjacent deployment script without allocating a console, waiting only
/// for that helper (not its long-lived tray child).
///
/// # Errors
/// Returns an error for invalid arguments, a missing staged script, or helper failure.
pub fn run_apply(mut arguments: Vec<String>) -> Result<(), String> {
    let force = arguments.last().is_some_and(|value| value == "--force");
    if force {
        arguments.pop();
    }
    if !(4..=5).contains(&arguments.len()) {
        return Err(
            "update-apply requires staged EXE, SHA-256, status, hostname, and optional target"
                .into(),
        );
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let script = exe
        .parent()
        .ok_or("missing update directory")?
        .join("remote-apply.ps1");
    if !script.is_file() {
        return Err("adjacent remote-apply.ps1 is missing".into());
    }
    let status = Path::new(&arguments[2]);
    let directory = status.parent().ok_or("missing status directory")?;
    let shell = PathBuf::from(std::env::var_os("SystemRoot").ok_or("SystemRoot is missing")?)
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut command = Command::new(shell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        // Some Windows PowerShell 5.1 hosts exit -1 before executing this advanced
        // script via -File. Invoke the same file/policy via the command host.
        // Values stay in child-local environment entries, never executable text.
        .arg("$ErrorActionPreference='Stop'; $p=@{StagedExe=$env:CF_UPDATE_EXE; ExpectedSha256=$env:CF_UPDATE_HASH; StatusPath=$env:CF_UPDATE_STATUS; ExpectedHostname=$env:CF_UPDATE_HOST}; if($env:CF_UPDATE_TARGET){$p.TargetExe=$env:CF_UPDATE_TARGET}; if($env:CF_UPDATE_FORCE -eq '1'){$p.Force=$true}; & $env:CF_UPDATE_SCRIPT @p")
        .env("CF_UPDATE_SCRIPT", script)
        .env("CF_UPDATE_EXE", &arguments[0])
        .env("CF_UPDATE_HASH", &arguments[1])
        .env("CF_UPDATE_STATUS", &arguments[2])
        .env("CF_UPDATE_HOST", &arguments[3])
        .env("CF_UPDATE_TARGET", arguments.get(4).map_or("", String::as_str))
        .env("CF_UPDATE_FORCE", if force { "1" } else { "0" })
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW; no hide-after-launch flash.
        // A closed pipe gives the noninteractive host unambiguous EOF.
        .stdin(Stdio::piped())
        .stdout(File::create(directory.join("helper.stdout.log")).map_err(|e| e.to_string())?)
        .stderr(File::create(directory.join("helper.stderr.log")).map_err(|e| e.to_string())?);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    drop(child.stdin.take());
    let result = child.wait().map_err(|error| error.to_string())?;
    if result.success() {
        Ok(())
    } else {
        if !status.exists() {
            let failure = serde_json::json!({
                "state": "failed", "outcome": "helper_start_failed",
                "error": format!("PowerShell exited before update status: {result}"),
                "helperExitCode": result.code(),
            });
            fs::write(
                status,
                serde_json::to_vec(&failure).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
        Err(format!("update helper failed: {result}"))
    }
}

/// Record the identity of the actual tray image, not merely a file on disk.
///
/// # Errors
/// Returns file/hash/serialization errors. A failure prevents false update readiness.
pub fn record_identity(root: &Path, elevated: bool) -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let mut source = File::open(&executable)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let directory = root.join("runtime");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("tray-{}.json", std::process::id()));
    let payload = serde_json::json!({
        "schemaVersion": 1, "processId": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"), "executablePath": executable,
        "sha256": format!("{:X}", hash.finalize()), "elevated": elevated,
    });
    fs::write(&path, serde_json::to_vec(&payload)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_receipt_identifies_this_image_and_privilege() {
        let root = tempfile::tempdir().unwrap();
        let path = record_identity(root.path(), false).unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(receipt["processId"], std::process::id());
        assert_eq!(receipt["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(receipt["elevated"], false);
        let executable = std::env::current_exe().unwrap();
        assert_eq!(receipt["executablePath"], executable.to_str().unwrap());
        let hash = Sha256::digest(fs::read(executable).unwrap());
        assert_eq!(receipt["sha256"], format!("{hash:X}"));
    }

    #[test]
    fn invalid_update_arguments_do_not_start_helpers() {
        assert!(run_apply(Vec::new()).is_err());
        assert!(run_apply(vec!["--force".into()]).is_err());
        assert!(run_apply(vec!["extra".into(); 6]).is_err());
    }
}
