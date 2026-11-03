//! Windows OpenSSH operations. Only locally initiated, fixed operations may elevate.
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(test)]
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
#[cfg(test)]
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
#[cfg(test)]
use windows_core::GUID;
use windows_core::{PCWSTR, PWSTR, w};

const NO_WINDOW: u32 = 0x0800_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const OPENSSH_BUNDLE: &[u8] = include_bytes!("../../assets/openssh/OpenSSH-Win64-10.0.0.0p2.zip");
const OPENSSH_BUNDLE_VERSION: &str = "10.0.0.0p2";
const OPENSSH_BUNDLE_SHA256: &str =
    "23F50F3458C4C5D0B12217C6A5DDFDE0137210A30FA870E98B29827F7B43ABA5";
const OPENSSH_SSH_SHA256: &str = "6890C128C86CC2C38AAD9FCB32A82B851FF3D38C714A1F656B8E445D7CD5E1C6";
const OPENSSH_KEYGEN_SHA256: &str =
    "B51FDD26BE0F7C83398D18E5354A0ACB0406A9DE25516791758FE63BBE3AE870";
const OPENSSH_CRYPTO_SHA256: &str =
    "4652E861C0335EE80A51306CEAB75AA35C8865B235F97CE7DD5A0FD9DAB44B5D";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)] // Independent observed Windows capabilities, not mutually exclusive states.
pub struct ServerInfo {
    pub client_installed: bool,
    pub firewall: String,
    #[serde(default)]
    pub loopback_only: bool,
    #[serde(default)]
    pub networks: Vec<serde_json::Value>,
    pub installed: bool,
    pub running: bool,
    pub port: u16,
    pub accounts: Vec<Account>,
    pub host_keys: Vec<String>,
    pub issue: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub sid: String,
    pub admin: bool,
}

pub fn ps_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        result.push(TABLE[(a >> 2) as usize] as char);
        result.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

pub fn encoded(script: &str) -> String {
    base64(
        &script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if length > 0 && length < buffer.len() {
        let path = PathBuf::from(OsString::from_wide(&buffer[..length]));
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(io::Error::other(
                "Windows returned a relative system directory",
            ))
        }
    } else {
        Err(if length == 0 {
            io::Error::last_os_error()
        } else {
            io::Error::other("Windows system directory path is too long")
        })
    }
}

#[cfg(test)]
fn known_folder(id: &GUID) -> io::Result<PathBuf> {
    let raw =
        unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.map_err(io::Error::other)?;
    let result = unsafe { raw.to_string() }
        .map(PathBuf::from)
        .map_err(io::Error::other);
    unsafe { CoTaskMemFree(Some(raw.as_ptr().cast())) };
    result
}

fn powershell() -> io::Result<PathBuf> {
    Ok(system_directory()?.join("WindowsPowerShell/v1.0/powershell.exe"))
}

fn client_open_ssh_root(ssh_root: &Path) -> PathBuf {
    ssh_root.join("runtime").join(OPENSSH_BUNDLE_VERSION)
}

fn expected_client_hash(name: &str) -> io::Result<&'static str> {
    match name {
        "ssh.exe" => Ok(OPENSSH_SSH_SHA256),
        "ssh-keygen.exe" => Ok(OPENSSH_KEYGEN_SHA256),
        "libcrypto.dll" => Ok(OPENSSH_CRYPTO_SHA256),
        _ => Err(io::Error::other("Unsupported bundled SSH client tool")),
    }
}

fn file_sha256(path: &Path) -> io::Result<Option<String>> {
    use sha2::{Digest, Sha256};
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(None);
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(Some(hex_upper(&digest.finalize())))
}

fn file_has_sha256(path: &Path, expected: &str) -> io::Result<bool> {
    Ok(file_sha256(path)?.as_deref() == Some(expected))
}

fn hex_upper(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(*byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(*byte & 0x0f)]));
    }
    result
}

fn plain_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            Ok(metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn bundled_client_ready(ssh_root: &Path) -> io::Result<bool> {
    let root = client_open_ssh_root(ssh_root);
    if !plain_directory(&root)? {
        return Ok(false);
    }
    for (name, expected) in [
        ("ssh.exe", OPENSSH_SSH_SHA256),
        ("ssh-keygen.exe", OPENSSH_KEYGEN_SHA256),
        ("libcrypto.dll", OPENSSH_CRYPTO_SHA256),
    ] {
        if !file_has_sha256(&root.join(name), expected)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn ssh_tool(ssh_root: &Path, name: &str) -> io::Result<PathBuf> {
    let bundled_root = client_open_ssh_root(ssh_root);
    let bundled = bundled_root.join(name);
    if plain_directory(&bundled_root)?
        && file_has_sha256(&bundled, expected_client_hash(name)?)?
        && file_has_sha256(&bundled_root.join("libcrypto.dll"), OPENSSH_CRYPTO_SHA256)?
    {
        return Ok(bundled);
    }
    #[cfg(test)]
    {
        let test_tool = test_open_ssh_root().join(name);
        if test_tool.is_file() {
            return Ok(test_tool);
        }
    }
    Err(io::Error::other(
        "Pierdesk OpenSSH runtime is missing. Use Set up access to install it.",
    ))
}

#[cfg(test)]
fn test_open_ssh_root() -> PathBuf {
    use std::sync::OnceLock;
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "pierdesk-openssh-test-{}",
            super::random_id().expect("random test runtime path")
        ));
        let archive = root.join("runtime.zip");
        fs::create_dir_all(&root).expect("create test runtime directory");
        fs::write(&archive, OPENSSH_BUNDLE).expect("write embedded test runtime");
        ps(&format!(
            "Expand-Archive -LiteralPath {} -DestinationPath {}",
            ps_literal(&archive.to_string_lossy()),
            ps_literal(&root.to_string_lossy())
        ))
        .expect("extract embedded test runtime");
        root.join("OpenSSH-Win64")
    })
    .clone()
}

pub fn output(mut command: Command, timeout: Duration) -> io::Result<String> {
    let mut child = command
        .creation_flags(NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("stderr unavailable"))?;
    let read = |mut stream: Box<dyn Read + Send>| {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            if bytes.len() < 131_072 {
                bytes.extend_from_slice(&buf[..n]);
            }
        }
        bytes
    };
    let out = thread::spawn(move || read(Box::new(stdout)));
    let err = thread::spawn(move || read(Box::new(stderr)));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Operation timed out; refresh to check actual state.",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let text = String::from_utf8_lossy(&stdout)
        .trim_start_matches('\u{feff}')
        .trim()
        .to_owned();
    let error = decode_elevated_error(&stderr).unwrap_or_else(|| {
        String::from_utf8_lossy(&stderr)
            .trim_start_matches('\u{feff}')
            .trim()
            .to_owned()
    });
    if status.success() {
        Ok(text)
    } else {
        Err(io::Error::other(if error.is_empty() {
            format!("Operation failed ({status}): {text}")
        } else {
            error
        }))
    }
}

const ELEVATED_ERROR_PREFIX: &str = "CLIPFERRY_ERROR_B64:";

fn decode_elevated_error(bytes: &[u8]) -> Option<String> {
    // Windows PowerShell 5.1 can encode Console.Error with the machine ANSI code page even when
    // OutputEncoding is UTF-8. Elevated operations therefore report their message as ASCII base64.
    // Search rather than requiring an exact prefix so harmless PowerShell host text cannot hide it.
    let ascii = std::str::from_utf8(bytes).ok()?;
    let encoded = ascii
        .split(ELEVATED_ERROR_PREFIX)
        .nth(1)?
        .split_whitespace()
        .next()?;
    String::from_utf8(decode_base64(encoded).ok()?).ok()
}

pub fn ps(script: &str) -> io::Result<String> {
    let mut command = Command::new(powershell()?);
    // PowerShell 7 may pass incompatible module paths to Windows PowerShell 5.1.
    command
        .env_remove("PSModulePath")
        .args(["-OutputFormat", "Text"]);
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded(&format!("$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); {script}"))]);
    output(command, Duration::from_secs(45))
}

fn elevate(script: &str) -> io::Result<()> {
    // Authority-bearing inputs stay in the immutable elevated command line. Results return over
    // a random, ACL-restricted named pipe; an elevated process never writes to a caller-writable
    // temporary path that could be replaced with a link.
    let pipe_name = format!("PierdeskElevated{}", super::random_id()?);
    let script = elevated_pipe_reporter(script, &pipe_name);
    let args = format!(
        "-NoLogo -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand {}",
        encoded(&script)
    );
    let powershell = powershell()?;
    let outer = elevated_launcher(&args, &pipe_name, &powershell);
    let mut command = Command::new(powershell);
    command
        .env_remove("PSModulePath")
        .args(["-OutputFormat", "Text"]);
    if outer.encode_utf16().count() > 30_000 {
        return Err(io::Error::other(
            "Administrator command exceeds Windows size limit",
        ));
    }
    command.args(["-NoProfile", "-NonInteractive", "-Command", &outer]);
    match output(command, Duration::from_mins(15)) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "管理员设置等待超时。请稍后刷新状态。",
        )),
        Err(error) => Err(error),
    }
}

fn elevated_pipe_reporter(script: &str, pipe_name: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false);$cfStep='管理员操作';$cfExit=0;$cfMessage='';try {{{script}}}catch{{$cfExit=1;$cfMessage=$cfStep+': '+$_.Exception.Message;if($cfMessage.Length -gt 4096){{$cfMessage=$cfMessage.Substring(0,4096)}}}}finally{{try{{$c=[IO.Pipes.NamedPipeClientStream]::new('.',{},[IO.Pipes.PipeDirection]::Out);$c.Connect(10000);$w=[IO.StreamWriter]::new($c,[Text.UTF8Encoding]::new($false));$w.Write($cfMessage);$w.Dispose();$c.Dispose()}}catch{{}}}};exit $cfExit",
        ps_literal(pipe_name)
    )
}

fn elevated_launcher(args: &str, pipe_name: &str, powershell: &Path) -> String {
    format!(
        "$ErrorActionPreference='Stop';$n={};$emit={{param([string]$v);$b=[Text.UTF8Encoding]::new($false).GetBytes($v);[Console]::Error.WriteLine('CLIPFERRY_ERROR_B64:'+[Convert]::ToBase64String($b))}};$sec=[IO.Pipes.PipeSecurity]::new();$right=[IO.Pipes.PipeAccessRights]::ReadWrite;$allow=[Security.AccessControl.AccessControlType]::Allow;foreach($sid in @([Security.Principal.WindowsIdentity]::GetCurrent().User,[Security.Principal.SecurityIdentifier]::new('S-1-5-18'),[Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))){{[void]$sec.AddAccessRule([IO.Pipes.PipeAccessRule]::new($sid,$right,$allow))}};$s=[IO.Pipes.NamedPipeServerStream]::new($n,[IO.Pipes.PipeDirection]::In,1,[IO.Pipes.PipeTransmissionMode]::Byte,[IO.Pipes.PipeOptions]::Asynchronous,4096,4096,$sec);$wait=$s.BeginWaitForConnection($null,$null);try{{$p=Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -WindowStyle Hidden -PassThru;$deadline=[DateTime]::UtcNow.AddMinutes(14);while(!$wait.IsCompleted){{if($p.HasExited){{throw ('管理员进程提前退出（代码 '+$p.ExitCode+'）')}};if([DateTime]::UtcNow -ge $deadline){{throw '管理员进程没有返回结果'}};[void]$wait.AsyncWaitHandle.WaitOne(200)}};$s.EndWaitForConnection($wait);$r=[IO.StreamReader]::new($s,[Text.UTF8Encoding]::new($false));$m=$r.ReadToEnd();$r.Dispose();$p.WaitForExit();if($p.ExitCode -ne 0){{& $emit $m;exit 1}};exit 0}}catch{{& $emit ('Windows 权限确认未完成：'+$_.Exception.Message);exit 1}}finally{{$s.Dispose()}}",
        ps_literal(pipe_name),
        ps_literal(&powershell.to_string_lossy()),
        ps_literal(args)
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_upper(&Sha256::digest(bytes))
}

fn elevate_hashed_payload(script: &str) -> io::Result<()> {
    // A setup script containing the embedded runtime installer is too large for Windows' command
    // line. The unelevated process writes it to a random file, while the small elevated loader
    // reads it once, verifies the SHA-256 passed in its immutable command line, and only then
    // creates a ScriptBlock from those verified in-memory bytes. This avoids a writable-script
    // elevation boundary and its time-of-check/time-of-use race.
    let token = super::random_id()?;
    let payload_path = std::env::temp_dir().join(format!("pierdesk-elevated-{token}.ps1"));
    let bytes = script.as_bytes();
    fs::write(&payload_path, bytes)?;
    let digest = sha256_hex(bytes);
    let loader_body = format!(
        "$b=[IO.File]::ReadAllBytes({});$h=[Security.Cryptography.SHA256]::Create();try{{$a=([BitConverter]::ToString($h.ComputeHash($b))).Replace('-','')}}finally{{$h.Dispose()}};if($a -cne {}){{throw 'Elevated payload integrity check failed'}};$t=[Text.UTF8Encoding]::new($false,$true).GetString($b);& ([ScriptBlock]::Create($t))",
        ps_literal(&payload_path.to_string_lossy()),
        ps_literal(&digest)
    );
    let pipe_name = format!("PierdeskElevated{}", super::random_id()?);
    let loader = elevated_pipe_reporter(&loader_body, &pipe_name);
    let args = format!(
        "-NoLogo -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand {}",
        encoded(&loader)
    );
    let powershell = powershell()?;
    let outer = elevated_launcher(&args, &pipe_name, &powershell);
    let mut command = Command::new(powershell);
    command
        .env_remove("PSModulePath")
        .args(["-OutputFormat", "Text"])
        .args(["-NoProfile", "-NonInteractive", "-Command", &outer]);
    let result = output(command, Duration::from_mins(15));
    let _ = fs::remove_file(payload_path);
    match result {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "管理员设置等待超时。安装可能仍在进行，请稍后刷新状态。",
        )),
        Err(error) => Err(error),
    }
}

fn install_bundled_open_ssh(script: &str) -> io::Result<()> {
    debug_assert_eq!(sha256_hex(OPENSSH_BUNDLE), OPENSSH_BUNDLE_SHA256);
    let token = super::random_id()?;
    let archive = std::env::temp_dir().join(format!("pierdesk-openssh-{token}.zip"));
    fs::write(&archive, OPENSSH_BUNDLE)?;
    let payload = format!(
        "$cfBundlePath={};$cfBundleHash={};$cfBundleVersion={};\n{}\n{}",
        ps_literal(&archive.to_string_lossy()),
        ps_literal(OPENSSH_BUNDLE_SHA256),
        ps_literal(OPENSSH_BUNDLE_VERSION),
        include_str!("install_bundle.ps1"),
        script
    );
    let result = elevate_hashed_payload(&payload);
    let _ = fs::remove_file(archive);
    result
}

pub fn inspect(ssh_root: &Path) -> io::Result<ServerInfo> {
    let raw = ps(&format!(
        "$cfGatewayProgram={}\n{}",
        ps_literal(&std::env::current_exe()?.to_string_lossy()),
        include_str!("inspect.ps1")
    ))?;
    let mut info: ServerInfo = serde_json::from_str(&raw).map_err(io::Error::other)?;
    info.accounts
        .retain(|account| visible_account(&account.name));
    let mut host_keys = Vec::with_capacity(info.host_keys.len());
    for key in &info.host_keys {
        let key = normalize_host_public_key(key)?;
        if !host_keys.contains(&key) {
            host_keys.push(key);
        }
    }
    info.host_keys = host_keys;
    info.client_installed = bundled_client_ready(ssh_root)?;
    Ok(info)
}

fn visible_account(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "aspnet"
            | "defaultaccount"
            | "guest"
            | "wdagutilityaccount"
            | "defaultuser0"
            | "codexsandboxoffline"
            | "codexsandboxonline"
    )
}

pub fn setup() -> io::Result<()> {
    install_bundled_open_ssh(&format!(
        "$cfGatewayProgram={}\n{}\n{}",
        ps_literal(&std::env::current_exe()?.to_string_lossy()),
        include_str!("setup_portable.ps1"),
        include_str!("publish.ps1")
    ))
}

pub fn setup_client(ssh_root: &Path) -> io::Result<()> {
    protect_directory(ssh_root)?;
    let token = super::random_id()?;
    let archive = ssh_root.join(format!(".client-bundle-{token}.zip"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&archive)?;
    file.write_all(OPENSSH_BUNDLE)?;
    file.sync_all()?;
    drop(file);
    let payload = format!(
        "$cfClientRoot={};$cfBundlePath={};$cfBundleHash={};$cfBundleVersion={};$cfSshHash={};$cfKeygenHash={};$cfCryptoHash={};\n{}",
        ps_literal(&ssh_root.to_string_lossy()),
        ps_literal(&archive.to_string_lossy()),
        ps_literal(OPENSSH_BUNDLE_SHA256),
        ps_literal(OPENSSH_BUNDLE_VERSION),
        ps_literal(OPENSSH_SSH_SHA256),
        ps_literal(OPENSSH_KEYGEN_SHA256),
        ps_literal(OPENSSH_CRYPTO_SHA256),
        include_str!("install_client.ps1")
    );
    let result = ps(&payload);
    let _ = fs::remove_file(&archive);
    result?;
    if bundled_client_ready(ssh_root)? {
        install_askpass_helper(ssh_root)?;
        Ok(())
    } else {
        Err(io::Error::other(
            "Pierdesk SSH connection tools failed their integrity check",
        ))
    }
}

fn replace_local_file(from: &Path, to: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        Ok(MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?)
    }
}

fn install_askpass_helper(ssh_root: &Path) -> io::Result<PathBuf> {
    // The SSH root is protected when the per-user client is installed. Reapplying its owner and
    // ACL here can require SeSecurityPrivilege on older Windows builds even when nothing changes.
    let source = std::env::current_exe()?;
    let source_hash = file_sha256(&source)?
        .ok_or_else(|| io::Error::other("Pierdesk executable is not a regular local file"))?;
    let helper = ssh_root.join("pierdesk-ssh-askpass.exe");
    if file_sha256(&helper)?.as_deref() == Some(source_hash.as_str()) {
        return Ok(helper);
    }

    let temporary = ssh_root.join(format!(
        ".pierdesk-ssh-askpass-{}.next",
        super::random_id()?
    ));
    let result = (|| {
        fs::copy(&source, &temporary)?;
        OpenOptions::new()
            .write(true)
            .open(&temporary)?
            .sync_all()?;
        if file_sha256(&temporary)?.as_deref() != Some(source_hash.as_str()) {
            return Err(io::Error::other(
                "Pierdesk SSH password helper failed its integrity check",
            ));
        }
        replace_local_file(&temporary, &helper)?;
        Ok(helper.clone())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

pub fn validate_public_key(value: &str) -> io::Result<String> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if value.contains(['\r', '\n'])
        || parts.len() < 2
        || parts.len() > 3
        || parts[0] != "ssh-ed25519"
        || parts[1].len() != 68
        || !parts[1]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c))
    {
        return Err(io::Error::other("Invalid Ed25519 public key"));
    }
    let bytes = decode_base64(parts[1])?;
    if bytes.len() != 51 || &bytes[..19] != b"\0\0\0\x0bssh-ed25519\0\0\0\x20" {
        return Err(io::Error::other("Invalid Ed25519 wire encoding"));
    }
    Ok(format!("{} {}", parts[0], parts[1]))
}

pub fn normalize_host_public_key(value: &str) -> io::Result<String> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(io::Error::other("Invalid SSH host public key"));
    }
    let mut parts = value.split_ascii_whitespace();
    let algorithm = parts
        .next()
        .ok_or_else(|| io::Error::other("Invalid SSH host public key"))?;
    let material = parts
        .next()
        .ok_or_else(|| io::Error::other("Invalid SSH host public key"))?;
    if !matches!(algorithm, "ssh-ed25519" | "ecdsa-sha2-nistp256" | "ssh-rsa")
        || material.is_empty()
        || material.len() > 8192
        || !material
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
    {
        return Err(io::Error::other("Invalid SSH host public key"));
    }
    let wire = decode_base64(material)?;
    let algorithm_length = wire
        .get(..4)
        .and_then(|prefix| <[u8; 4]>::try_from(prefix).ok())
        .map(u32::from_be_bytes)
        .and_then(|length| usize::try_from(length).ok())
        .ok_or_else(|| io::Error::other("Invalid SSH host key wire encoding"))?;
    if wire.get(4..4 + algorithm_length) != Some(algorithm.as_bytes()) {
        return Err(io::Error::other("Invalid SSH host key wire encoding"));
    }
    Ok(format!("{algorithm} {material}"))
}

fn decode_base64(value: &str) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for b in value.bytes() {
        let n = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err(io::Error::other("Invalid base64")),
        };
        bits = (bits << 6) | u32::from(n);
        count += 6;
        if count >= 8 {
            count -= 8;
            result.push(((bits >> count) & 255) as u8);
        }
    }
    if base64(&result) != value {
        return Err(io::Error::other("Noncanonical base64"));
    }
    Ok(result)
}

pub fn key_fingerprint(key: &str) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = decode_base64(
        key.split_whitespace()
            .nth(1)
            .ok_or_else(|| io::Error::other("Missing public key"))?,
    )?;
    Ok(format!(
        "SHA256:{}",
        base64(&Sha256::digest(bytes)).trim_end_matches('=')
    ))
}

pub fn protect_directory(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    ps(&format!(
        "$p={}; if((Get-Item -LiteralPath $p).Attributes -band [IO.FileAttributes]::ReparsePoint) {{ throw 'SSH directory cannot be a link' }}; $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User; $acl=[Security.AccessControl.DirectorySecurity]::new();$acl.SetAccessRuleProtection($true,$false);$acl.SetOwner($sid);foreach($s in @($sid,[Security.Principal.SecurityIdentifier]::new('S-1-5-18'))){{$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($s,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))}};Set-Acl -LiteralPath $p -AclObject $acl",
        ps_literal(&root.to_string_lossy())
    ))?;
    Ok(())
}

pub fn generate_key(path: &Path) -> io::Result<String> {
    let ssh_root = path
        .parent()
        .ok_or_else(|| io::Error::other("SSH key path has no parent directory"))?;
    if !path.exists() {
        let mut command = Command::new(ssh_tool(ssh_root, "ssh-keygen.exe")?);
        command
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "pierdesk"])
            .arg("-f")
            .arg(path);
        output(command, Duration::from_secs(20))?;
    }
    // A freshly generated key has no passphrase, so ssh-keygen never consults the helper here.
    let public = public_key(path, "", &keygen_askpass_program()?)?;
    validate_public_key(&public)
}

/// Switches the `Pierdesk` executable into the askpass role for one `ssh-keygen` invocation.
pub const KEYGEN_SECRET_ENV: &str = "PIERDESK_SSH_KEYGEN_SECRET";
const KEYGEN_SECRET_MAGIC: &str = "ClipFerry ssh-keygen secret v1";
const KEYGEN_SECRET_PREFIX: &str = ".keygen-";
const KEYGEN_SECRET_SUFFIX: &str = ".secret";
const KEYGEN_SECRET_LIMIT: u64 = 4096;

/// The program `ssh-keygen` launches to obtain passphrases: this `Pierdesk` executable.
pub fn keygen_askpass_program() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// Passphrases for one `ssh-keygen` invocation. They reach the askpass helper through this
/// DPAPI-protected file instead of `-P`/`-N`, because process-creation auditing and endpoint
/// security tools commonly persist command lines. Dropping it removes the file and its marker.
struct KeygenSecret {
    path: PathBuf,
}

impl KeygenSecret {
    fn create(directory: &Path, current: &str, next: &str) -> io::Result<Self> {
        if current.contains(['\r', '\n']) || next.contains(['\r', '\n']) {
            return Err(io::Error::other(
                "SSH key passphrases cannot contain line breaks",
            ));
        }
        let path = directory.join(format!(
            "{KEYGEN_SECRET_PREFIX}{}{KEYGEN_SECRET_SUFFIX}",
            super::random_id()?
        ));
        let protected = crate::device_store::protect_for_current_user(
            format!("{KEYGEN_SECRET_MAGIC}\n{current}\n{next}").as_bytes(),
        )?;
        let secret = Self { path };
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&secret.path)?
            .write_all(&protected)?;
        Ok(secret)
    }

    /// Whether the helper answered at least one prompt for this invocation.
    fn served(&self) -> bool {
        keygen_served_marker(&self.path).is_file()
    }
}

impl Drop for KeygenSecret {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(keygen_served_marker(&self.path));
    }
}

fn keygen_served_marker(secret: &Path) -> PathBuf {
    secret.with_extension("served")
}

/// Only `ssh-keygen -p` asks for the new passphrase (twice); every other prompt wants the current.
fn keygen_answer<'a>(prompt: &str, current: &'a str, next: &'a str) -> &'a str {
    if prompt.starts_with("Enter new passphrase") || prompt.starts_with("Enter same passphrase") {
        next
    } else {
        current
    }
}

/// Answers one `ssh-keygen` prompt from a `KeygenSecret` file and records that it was served.
fn keygen_askpass_answer(secret: &Path, prompt: &str) -> io::Result<String> {
    let name = secret
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    if !name.starts_with(KEYGEN_SECRET_PREFIX) || !name.ends_with(KEYGEN_SECRET_SUFFIX) {
        return Err(io::Error::other("Invalid ssh-keygen secret reference"));
    }
    let mut protected = Vec::new();
    fs::File::open(secret)?
        .take(KEYGEN_SECRET_LIMIT)
        .read_to_end(&mut protected)?;
    let cleartext = crate::device_store::unprotect_for_current_user(&protected)?;
    let text = std::str::from_utf8(&cleartext).map_err(io::Error::other)?;
    let mut parts = text.splitn(3, '\n');
    let (Some(KEYGEN_SECRET_MAGIC), Some(current), Some(next)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(io::Error::other("Invalid ssh-keygen secret"));
    };
    let answer = keygen_answer(prompt, current, next).to_owned();
    fs::write(keygen_served_marker(secret), b"")?;
    Ok(answer)
}

/// Entry point when `ssh-keygen` runs this executable as `SSH_ASKPASS`.
pub fn run_keygen_askpass() -> io::Result<()> {
    let secret = std::env::var_os(KEYGEN_SECRET_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("Missing ssh-keygen secret reference"))?;
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let answer = keygen_askpass_answer(&secret, &prompt)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(answer.as_bytes())?;
    stdout.flush()
}

fn keygen_command(ssh_root: &Path, askpass: &Path, secret: &KeygenSecret) -> io::Result<Command> {
    let mut command = Command::new(ssh_tool(ssh_root, "ssh-keygen.exe")?);
    command
        .env("SSH_ASKPASS", askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("DISPLAY", "pierdesk")
        .env(KEYGEN_SECRET_ENV, &secret.path)
        .env_remove("PIERDESK_SSH_ASKPASS_ID");
    Ok(command)
}

/// A wrong passphrase, as opposed to `ssh-keygen` or its helper failing to run.
#[derive(Debug)]
struct PassphraseMismatch;

impl std::fmt::Display for PassphraseMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SSH key passphrase does not match")
    }
}

impl std::error::Error for PassphraseMismatch {}

pub fn is_passphrase_mismatch(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<PassphraseMismatch>)
}

/// `ssh-keygen` prints "incorrect passphrase" both for a wrong answer and when its askpass helper
/// never ran (it then retries an empty passphrase), so only an answered prompt counts as a
/// mismatch. Anything else is a tool failure, e.g. processes that cannot start under memory
/// exhaustion, which used to be misreported as a credential that no longer unlocks the key.
fn classify_keygen<T>(result: io::Result<T>, secret: &KeygenSecret) -> io::Result<T> {
    result.map_err(|error| {
        if !secret.served() {
            io::Error::other(format!(
                "{error} (the Pierdesk passphrase helper did not answer)"
            ))
        } else if error.to_string().contains("incorrect passphrase") {
            io::Error::other(PassphraseMismatch)
        } else {
            error
        }
    })
}

pub fn public_key(path: &Path, passphrase: &str, askpass: &Path) -> io::Result<String> {
    let ssh_root = path
        .parent()
        .ok_or_else(|| io::Error::other("SSH key path has no parent directory"))?;
    let secret = KeygenSecret::create(ssh_root, passphrase, "")?;
    let mut command = keygen_command(ssh_root, askpass, &secret)?;
    command.arg("-y").arg("-f").arg(path);
    classify_keygen(output(command, Duration::from_secs(15)), &secret)
}

pub fn set_key_passphrase(path: &Path, old: &str, new: &str, askpass: &Path) -> io::Result<()> {
    let ssh_root = path
        .parent()
        .ok_or_else(|| io::Error::other("SSH key path has no parent directory"))?;
    let secret = KeygenSecret::create(ssh_root, old, new)?;
    let mut command = keygen_command(ssh_root, askpass, &secret)?;
    command.args(["-q", "-p", "-f"]).arg(path);
    classify_keygen(
        output(command, Duration::from_secs(30)).map(|_| ()),
        &secret,
    )
}

pub fn grant(sid: &str, public: &str, id: &str, revoke: bool) -> io::Result<()> {
    if !sid.starts_with("S-1-5-21-")
        || !sid
            .bytes()
            .all(|c| c.is_ascii_digit() || b"S-".contains(&c))
        || !super::valid_id(id)
    {
        return Err(io::Error::other("Invalid account or grant identity"));
    }
    let public = validate_public_key(public)?;
    elevate(&grant_script(sid, &public, id, revoke))
}

fn grant_script(sid: &str, public: &str, id: &str, revoke: bool) -> String {
    format!(
        "$accountSid={}; $publicKey={}; $grantId={}; $revoke=${};\n{}",
        ps_literal(sid),
        ps_literal(public),
        ps_literal(id),
        if revoke { "true" } else { "false" },
        include_str!("grant.ps1")
    )
}

fn command_text_for_tools(
    tool: &Path,
    config: &Path,
    alias: &str,
    askpass: &Path,
    id: &str,
) -> String {
    format!(
        "& {{ $pierdeskOldAskPass=$env:SSH_ASKPASS; $pierdeskOldAskPassRequire=$env:SSH_ASKPASS_REQUIRE; $pierdeskOldDisplay=$env:DISPLAY; $pierdeskOldAskPassId=$env:PIERDESK_SSH_ASKPASS_ID; try {{ $env:SSH_ASKPASS={}; $env:SSH_ASKPASS_REQUIRE='force'; $env:DISPLAY='pierdesk'; $env:PIERDESK_SSH_ASKPASS_ID={}; & {} -F {} {} }} finally {{ $env:SSH_ASKPASS=$pierdeskOldAskPass; $env:SSH_ASKPASS_REQUIRE=$pierdeskOldAskPassRequire; $env:DISPLAY=$pierdeskOldDisplay; $env:PIERDESK_SSH_ASKPASS_ID=$pierdeskOldAskPassId }} }}",
        ps_literal(&askpass.to_string_lossy()),
        ps_literal(id),
        ps_literal(&tool.to_string_lossy()),
        ps_literal(&config.to_string_lossy()),
        ps_literal(alias)
    )
}

pub fn command_text(config: &Path, alias: &str, id: &str) -> io::Result<String> {
    if !super::valid_id(id) {
        return Err(io::Error::other("Invalid SSH authorization identity"));
    }
    let ssh_root = config
        .parent()
        .ok_or_else(|| io::Error::other("SSH configuration path has no parent directory"))?;
    Ok(command_text_for_tools(
        &ssh_tool(ssh_root, "ssh.exe")?,
        config,
        alias,
        &install_askpass_helper(ssh_root)?,
        id,
    ))
}

pub fn test(config: &Path, alias: &str, sid: &str, id: &str) -> io::Result<serde_json::Value> {
    if !super::valid_id(id) {
        return Err(io::Error::other("Invalid SSH authorization identity"));
    }
    let script = "$i=[Security.Principal.WindowsIdentity]::GetCurrent(); $p=[Security.Principal.WindowsPrincipal]::new($i); [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); @{sid=$i.User.Value;account=$i.Name;hostName=$env:COMPUTERNAME;admin=$p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator);shell='Windows PowerShell'} | ConvertTo-Json -Compress";
    let ssh_root = config
        .parent()
        .ok_or_else(|| io::Error::other("SSH configuration path has no parent directory"))?;
    let mut command = Command::new(ssh_tool(ssh_root, "ssh.exe")?);
    command
        .env("SSH_ASKPASS", install_askpass_helper(ssh_root)?)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("DISPLAY", "pierdesk")
        .env("PIERDESK_SSH_ASKPASS_ID", id);
    command.arg("-F").arg(config).args([
        "-T",
        "-o",
        "BatchMode=no",
        "-o",
        "ConnectTimeout=8",
        alias,
        "powershell.exe",
        "-NoProfile",
        "-NonInteractive",
        "-EncodedCommand",
        &encoded(script),
    ]);
    let text = output(command, Duration::from_secs(25))?;
    let result: serde_json::Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    if result["sid"].as_str() != Some(sid) {
        return Err(io::Error::other(
            "Remote account identity does not match the approved account",
        ));
    }
    Ok(result)
}

pub fn terminal(config: &Path, alias: &str, id: &str) -> io::Result<()> {
    if !super::valid_id(id) {
        return Err(io::Error::other("Invalid SSH authorization identity"));
    }
    let ssh_root = config
        .parent()
        .ok_or_else(|| io::Error::other("SSH configuration path has no parent directory"))?;
    let ssh = ssh_tool(ssh_root, "ssh.exe")?;
    let script = terminal_script(&ssh, config, alias, &install_askpass_helper(ssh_root)?, id);
    let executable = wide_null(powershell()?.as_os_str());
    let parameters = wide_null(std::ffi::OsStr::new(&terminal_parameters(&script)));
    // ShellExecute starts the console independently from Pierdesk's GUI process. This lets
    // Windows assign the terminal real standard-input handles instead of inheriting the GUI's
    // empty handles, which previously made ssh exit as soon as the window appeared.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(executable.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        return Err(io::Error::other(format!(
            "Windows could not open the SSH terminal (ShellExecute={})",
            result.0 as isize
        )));
    }
    Ok(())
}

fn terminal_script(ssh: &Path, config: &Path, alias: &str, askpass: &Path, id: &str) -> String {
    format!(
        "$Host.UI.RawUI.WindowTitle={}; $env:SSH_ASKPASS={}; $env:SSH_ASKPASS_REQUIRE='force'; $env:DISPLAY='pierdesk'; $env:PIERDESK_SSH_ASKPASS_ID={}; Write-Host 'Pierdesk SSH 正在连接…' -ForegroundColor Cyan; & {} -F {} {}; $pierdeskExit=$LASTEXITCODE; Write-Host ''; if($pierdeskExit -eq 0){{Write-Host 'SSH 连接已结束。此窗口会保留，可输入 exit 关闭。' -ForegroundColor Yellow}}else{{Write-Host ('SSH 连接失败或已中断（退出代码 {{0}}）。此窗口会保留，请查看上方错误；输入 exit 关闭。' -f $pierdeskExit) -ForegroundColor Red}}",
        ps_literal(&format!("Pierdesk SSH - {alias}")),
        ps_literal(&askpass.to_string_lossy()),
        ps_literal(id),
        ps_literal(&ssh.to_string_lossy()),
        ps_literal(&config.to_string_lossy()),
        ps_literal(alias)
    )
}

fn terminal_parameters(script: &str) -> String {
    format!(
        "-NoLogo -NoProfile -NoExit -EncodedCommand {}",
        encoded(script)
    )
}

fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn process_image(process_id: u32) -> io::Result<PathBuf> {
    let process = ProcessHandle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
            .map_err(io::Error::other)?,
    );
    let mut buffer = vec![0_u16; 32_768];
    let mut length = u32::try_from(buffer.len()).map_err(io::Error::other)?;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut length,
        )
    }
    .map_err(io::Error::other)?;
    Ok(PathBuf::from(OsString::from_wide(
        &buffer[..usize::try_from(length).map_err(io::Error::other)?],
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keygen_test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pierdesk-keygen-{label}-{}",
            super::super::random_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn keygen_prompts_route_to_the_current_or_new_passphrase() {
        // Prompts observed from the bundled OpenSSH 10.0.0.0p2 ssh-keygen.
        for (prompt, expected) in [
            (r#"Enter passphrase for "C:\keys\k": "#, "old"),
            ("Enter old passphrase: ", "old"),
            ("Enter new passphrase (empty for no passphrase): ", "new"),
            ("Enter same passphrase again: ", "new"),
        ] {
            assert_eq!(keygen_answer(prompt, "old", "new"), expected, "{prompt}");
        }
    }

    #[test]
    fn keygen_secret_is_protected_answered_once_marked_and_removed() {
        let root = keygen_test_root("secret");
        let current = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let secret = KeygenSecret::create(&root, current, "").unwrap();
        let protected = fs::read(&secret.path).unwrap();
        assert!(
            !protected
                .windows(current.len())
                .any(|window| window == current.as_bytes())
        );
        assert!(!secret.served());
        assert_eq!(
            keygen_askpass_answer(&secret.path, "Enter old passphrase: ").unwrap(),
            current
        );
        assert!(secret.served());
        let (path, marker) = (secret.path.clone(), keygen_served_marker(&secret.path));
        drop(secret);
        assert!(!path.exists() && !marker.exists());

        assert!(KeygenSecret::create(&root, "a\nb", "").is_err());
        let foreign = root.join("credential.dpapi");
        fs::write(&foreign, b"x").unwrap();
        assert!(keygen_askpass_answer(&foreign, "Enter passphrase: ").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_an_answered_prompt_counts_as_a_wrong_passphrase() {
        let root = keygen_test_root("classify");
        let secret = KeygenSecret::create(&root, "", "").unwrap();
        let wrong = || -> io::Result<()> {
            Err(io::Error::other(
                "Load key \"k\": incorrect passphrase supplied to decrypt private key",
            ))
        };
        // ssh-keygen prints the same text when its helper never ran and it fell back to "".
        let unanswered = classify_keygen(wrong(), &secret).unwrap_err();
        assert!(!is_passphrase_mismatch(&unanswered));
        assert!(unanswered.to_string().contains("helper did not answer"));

        keygen_askpass_answer(&secret.path, "Enter passphrase: ").unwrap();
        assert!(is_passphrase_mismatch(
            &classify_keygen(wrong(), &secret).unwrap_err()
        ));
        let corrupt =
            classify_keygen::<()>(Err(io::Error::other("invalid format")), &secret).unwrap_err();
        assert!(!is_passphrase_mismatch(&corrupt));
        drop(secret);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quote_and_encode_are_literal() {
        assert_eq!(ps_literal("a'b;$x"), "'a''b;$x'");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(encoded("A"), "QQA=");
        assert_eq!(
            decode_elevated_error(b"CLIPFERRY_ERROR_B64:566h55CG5ZGY5pON5L2c77ya5rWL6K+V")
                .as_deref(),
            Some("管理员操作：测试")
        );
    }

    #[test]
    fn ssh_terminal_script_keeps_the_console_open_and_reports_exit_status() {
        let alias = "cf-terminal-test";
        let script = terminal_script(
            Path::new(r"C:\Program Files\Pierdesk\ssh.exe"),
            Path::new(r"C:\Users\Test User\connection.config"),
            alias,
            Path::new(r"C:\Program Files\Pierdesk\pierdesk.exe"),
            "0123456789abcdef0123456789abcdef",
        );
        let parameters = terminal_parameters(&script);
        assert!(parameters.contains("-NoExit"));
        assert!(script.contains("$LASTEXITCODE"));
        assert!(script.contains("此窗口会保留"));
        assert!(script.contains("'C:\\Users\\Test User\\connection.config'"));
        assert!(script.contains("PIERDESK_SSH_ASKPASS_ID"));
        let parse = format!(
            "$tokens=$null;$errors=$null;[void][Management.Automation.Language.Parser]::ParseInput({},[ref]$tokens,[ref]$errors);if($errors.Count){{throw $errors[0].Message}}",
            ps_literal(&script)
        );
        ps(&parse).unwrap();
    }

    #[test]
    fn elevated_error_transport_survives_windows_powershell_encoding() {
        let message = "管理员操作：无法生成 Pierdesk SSH 主机密钥";
        let script = format!(
            "[Console]::Error.WriteLine('{}{}'); exit 1",
            ELEVATED_ERROR_PREFIX,
            base64(message.as_bytes())
        );
        let mut command = Command::new(powershell().unwrap());
        command
            .env_remove("PSModulePath")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-OutputFormat",
                "Text",
            ])
            .args(["-EncodedCommand", &encoded(&script)]);
        assert_eq!(
            output(command, Duration::from_secs(10))
                .unwrap_err()
                .to_string(),
            message
        );
    }

    #[test]
    fn windows_powershell_51_preserves_the_empty_host_key_passphrase() {
        let root = std::env::temp_dir().join(format!(
            "pierdesk-ps51-host-key-test-{}",
            super::super::random_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let key = root.join("ssh_host_ed25519_key");
        let keygen = test_open_ssh_root().join("ssh-keygen.exe");
        ps(&format!(
            "& {} -q -t ed25519 -N '\"\"' -f {}; if($LASTEXITCODE){{throw 'host key generation failed'}}",
            ps_literal(&keygen.to_string_lossy()),
            ps_literal(&key.to_string_lossy())
        ))
        .unwrap();
        assert!(key.is_file());
        assert!(key.with_extension("pub").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn elevated_launcher_uses_the_run_as_parameter_set() {
        let launcher = elevated_launcher(
            "-NoProfile",
            "PierdeskElevatedTest",
            Path::new(r"D:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );
        assert!(launcher.contains("-Verb RunAs"));
        assert!(launcher.contains("-PassThru"));
        assert!(!launcher.contains("-Wait -PassThru"));
        assert!(launcher.contains("$p.WaitForExit()"));
        assert!(launcher.contains("while(!$wait.IsCompleted)"));
        assert!(launcher.contains("if($p.HasExited)"));
        assert!(launcher.contains("WaitOne(200)"));
        assert!(!launcher.contains("WaitOne(840000)"));
        assert!(!launcher.contains("RedirectStandard"));
        assert!(launcher.contains("NamedPipeServerStream"));
        assert!(launcher.contains("CLIPFERRY_ERROR_B64:"));
        let reporter = elevated_pipe_reporter("Write-Output ok", "PierdeskElevatedTest");
        assert!(reporter.contains("NamedPipeClientStream"));
        assert!(!reporter.contains("WriteAllText"));

        let parse = format!(
            "$tokens=$null;$errors=$null;[void][Management.Automation.Language.Parser]::ParseInput({},[ref]$tokens,[ref]$errors);if($errors.Count){{throw $errors[0].Message}}",
            ps_literal(&launcher)
        );
        ps(&parse).unwrap();
    }

    #[test]
    fn bundled_openssh_release_is_exactly_pinned() {
        assert_eq!(sha256_hex(OPENSSH_BUNDLE), OPENSSH_BUNDLE_SHA256);
        assert!(OPENSSH_BUNDLE.len() > 5_000_000);
        assert_eq!(OPENSSH_BUNDLE_VERSION, "10.0.0.0p2");
    }

    #[test]
    fn portable_install_uses_a_protected_copy_and_microsoft_signatures() {
        let install = include_str!("install_bundle.ps1");
        assert!(install.contains("$protectedBundle"));
        assert!(install.contains("Get-FileHash -LiteralPath $protectedBundle"));
        assert!(install.contains("Expand-Archive -LiteralPath $protectedBundle"));
        assert!(!install.contains("Expand-Archive -LiteralPath $cfBundlePath"));
        assert!(install.contains("Get-AuthenticodeSignature"));
        assert!(install.contains("SignerCertificate.Subject -notmatch 'Microsoft'"));
        assert!(install.contains("'clipferry-sshd.exe'"));
        assert!(install.contains("Copy-Item -LiteralPath (Join-Path $source 'sshd.exe')"));
        assert!(install.contains("Existing OpenSSH target link rejected"));
        assert!(install.contains("'sftp-server.exe'"));
        assert!(install.contains("Protect-CFOpenSshDirectory $target $true"));
        assert!(install.contains("Protect-CFOpenSshTree $target"));
        assert!(install.contains("if($ready)"));
        assert!(!install.contains("Add-WindowsCapability"));
        assert!(!install.contains("Remove-WindowsCapability"));
    }

    #[test]
    fn portable_service_is_independent_and_acl_scoped() {
        let setup = include_str!("setup_portable.ps1");
        assert!(setup.contains("$cfServiceName='ClipFerrySshd'"));
        assert!(setup.contains("$cfPort=45834"));
        assert!(setup.contains("$tool=Join-Path $cfToolRoot 'clipferry-sshd.exe'"));
        // Installed sshd configs start with this marker; changing it makes repair refuse them.
        assert!(setup.contains("# Managed by ClipFerry. This file is independent"));
        assert!(setup.contains("StartsWith('# Managed by ClipFerry.')"));
        assert!(setup.contains("AddressFamily inet"));
        assert!(setup.contains("ListenAddress 127.0.0.1"));
        assert!(!setup.contains("ListenAddress ::"));
        assert!(setup.contains("'ClipFerry\\ssh'"));
        assert!(setup.contains("accounts/%u/authorized_keys"));
        assert!(setup.contains("PidFile $pidFile"));
        assert!(setup.contains("DisableForwarding yes"));
        assert!(setup.contains("$configNext=$cfConfig+'.next-'"));
        assert!(setup.contains("& $tool -t -f $configNext"));
        assert!(setup.contains("[IO.File]::Replace($configNext,$cfConfig,$configBackup)"));
        assert!(setup.contains("[IO.File]::Replace($configBackup,$cfConfig,$rollbackDiscard)"));
        assert!(setup.contains("Restart-Service -Name $cfServiceName"));
        assert!(setup.contains("$current.Status -ne 'Running'"));
        assert!(setup.contains("'obj=' 'LocalSystem'"));
        assert!(setup.contains("$trustedSystemDirectory=[Environment]::SystemDirectory"));
        assert!(setup.contains("$sc=Join-Path $trustedSystemDirectory 'sc.exe'"));
        assert!(setup.contains("Test-Path -LiteralPath $sc -PathType Leaf"));
        assert!(!setup.contains("& sc.exe"));
        assert!(setup.contains("$serviceRecord.StartName -ne 'LocalSystem'"));
        assert!(setup.contains("RedirectionGuard"));
        assert!(setup.contains("Image File Execution Options\\clipferry-sshd.exe"));
        assert!(!setup.contains("Image File Execution Options\\sshd.exe'"));
        assert!(setup.contains("0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0x10"));
        assert!(setup.contains("if($null -eq $mitigation)"));
        assert!(setup.contains("S-1-5-32-545"));
        assert!(setup.contains("Protect-CFDirectory $cfDataRoot $true"));
        assert!(setup.contains("Protect-CFDirectory (Join-Path $cfDataRoot 'accounts') $false"));
        assert!(setup.contains("Protect-CFDirectory $cfLogRoot $false"));
        assert!(setup.contains("Protect-CFPublicFile ($hostKey+'.pub')"));
        assert!(setup.contains("@('-N','\"\"','-f',$candidate)"));
        assert!(!setup.contains("@('-N','','-f',$candidate)"));
        assert!(setup.contains("type='rsa'"));
        assert!(!setup.contains("Get-WindowsCapability"));
        assert!(!setup.contains("Set-Service sshd"));
        assert!(!setup.contains("Restart-Service sshd"));
    }

    #[test]
    fn per_user_client_install_never_elevates_and_uses_pinned_files() {
        let install = include_str!("install_client.ps1");
        assert!(!install.contains("Start-Process"));
        assert!(!install.contains("-Verb RunAs"));
        assert!(!install.contains("ProgramFiles"));
        assert!(!install.contains("System32"));
        assert!(install.contains("Get-FileHash"));
        assert!(install.contains("Get-AuthenticodeSignature"));

        let root = std::env::temp_dir().join(format!(
            "pierdesk-client-install-test-{}",
            super::super::random_id().unwrap()
        ));
        setup_client(&root).unwrap();
        assert!(bundled_client_ready(&root).unwrap());
        for name in ["ssh.exe", "ssh-keygen.exe"] {
            let tool = ssh_tool(&root, name).unwrap();
            assert!(tool.starts_with(&root));
            assert!(tool.is_file());
        }
        let helper = root.join("pierdesk-ssh-askpass.exe");
        assert!(helper.is_file());
        assert_eq!(
            file_sha256(&helper).unwrap(),
            file_sha256(&std::env::current_exe().unwrap()).unwrap()
        );
        let id = "0123456789abcdef0123456789abcdef";
        let command = command_text(&root.join("test.config"), &format!("cf-{id}"), id).unwrap();
        assert!(command.contains(&root.to_string_lossy().to_string()));
        assert!(command.contains("pierdesk-ssh-askpass.exe"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn public_metadata_is_readable_without_exposing_private_material() {
        let publish = include_str!("publish.ps1");
        assert!(publish.contains("S-1-5-32-545"));
        assert!(publish.contains("'Read'"));
        assert!(publish.contains("clipferry-public.json"));
        assert!(!publish.contains("ssh_host_ed25519_key'"));
    }

    #[test]
    fn copied_command_uses_the_bundled_client_and_automatic_key_unlock() {
        let command = command_text_for_tools(
            Path::new(r"C:\Program Files\Pierdesk\OpenSSH\ssh.exe"),
            Path::new(r"C:\Users\Example User\connection.config"),
            "cf-0123456789abcdef0123456789abcdef",
            Path::new(
                r"C:\Users\Example User\AppData\Local\ClipFerry\ssh\pierdesk-ssh-askpass.exe",
            ),
            "0123456789abcdef0123456789abcdef",
        );
        assert!(command.contains("'C:\\Program Files\\Pierdesk\\OpenSSH\\ssh.exe'"));
        assert!(command.contains("'C:\\Users\\Example User\\connection.config'"));
        assert!(command.contains("pierdesk-ssh-askpass.exe"));
        assert!(command.contains("PIERDESK_SSH_ASKPASS_ID"));
        assert!(command.contains("SSH_ASKPASS_REQUIRE='force'"));
        assert!(command.contains("finally"));
        let parse = format!(
            "$tokens=$null;$errors=$null;[void][Management.Automation.Language.Parser]::ParseInput({},[ref]$tokens,[ref]$errors);if($errors.Count){{throw $errors[0].Message}}",
            ps_literal(&command)
        );
        ps(&parse).unwrap();
    }

    #[test]
    fn grant_atomic_replace_uses_a_real_backup_path_on_windows_powershell() {
        let grant = include_str!("grant.ps1");
        assert!(grant.contains("[IO.File]::Replace($next,$path,$backup)"));
        assert!(!grant.contains("[IO.File]::Replace($next,$path,$null)"));
    }

    #[test]
    fn grant_is_an_atomic_dedicated_account_operation() {
        let grant = include_str!("grant.ps1");
        assert!(grant.contains("[Environment+SpecialFolder]::ProgramFiles"));
        assert!(grant.contains("[Environment+SpecialFolder]::CommonApplicationData"));
        assert!(grant.contains("'ClipFerry\\ssh'"));
        assert!(grant.contains("$accountDir=Join-Path $accountsRoot $name"));
        assert!(grant.contains("$entry=$publicKey+' clipferry:'+$grantId"));
        assert!(grant.contains("Protect-CFPath $accountDir $true $accountSid"));
        assert!(grant.contains("Protect-CFPath $keyPath $false $accountSid"));
        assert!(grant.contains("[IO.File]::Replace($next,$path,$backup)"));
        assert!(grant.contains("& $tool -T -f $config"));
        assert!(!grant.contains("System32\\OpenSSH"));
        assert!(!grant.contains("administrators_authorized_keys"));
        assert!(!grant.contains("Restart-Service"));
        assert!(!grant.contains("Set-Service"));
        assert!(grant.contains("^[\\p{L}\\p{Nd}_.-]+$"));
        assert!(!grant.contains("^[a-z0-9_.-]+$"));

        let payload = grant_script(
            "S-1-5-21-1",
            "ssh-ed25519 test",
            "0123456789abcdef0123456789abcdef",
            false,
        );
        assert!(!payload.contains("clipferry-public.json"));
        assert!(!payload.contains(include_str!("publish.ps1")));
    }

    #[test]
    fn authoritative_windows_paths_fail_closed_without_literal_fallbacks() {
        let source = include_str!("platform.rs");
        assert!(!source.contains("System32\\OpenSSH"));
        assert!(!source.contains("PathBuf::from(r\"C:\\Windows"));
        assert!(!source.contains("PathBuf::from(r\"C:\\Program Files"));
        assert!(!source.contains("PathBuf::from(r\"C:\\ProgramData"));
        assert!(system_directory().unwrap().is_absolute());
        assert!(known_folder(&FOLDERID_ProgramFiles).unwrap().is_absolute());
        assert!(known_folder(&FOLDERID_ProgramData).unwrap().is_absolute());
    }

    #[test]
    fn rejects_key_options_and_line_injection() {
        let mut wire = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        wire.extend_from_slice(&[1; 32]);
        let key = format!("ssh-ed25519 {}", base64(&wire));
        assert!(validate_public_key(&key).is_ok());
        assert!(validate_public_key(&format!("{key}\ncommand=x")).is_err());
        assert!(validate_public_key(&format!("command=x {key}")).is_err());
    }

    #[test]
    fn host_key_comments_are_removed_without_weakening_wire_validation() {
        let mut wire = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        wire.extend_from_slice(&[7; 32]);
        let key = format!("ssh-ed25519 {}", base64(&wire));
        assert_eq!(
            normalize_host_public_key(&format!("{key} user@LAPTOP-OFFICE")).unwrap(),
            key
        );
        assert!(normalize_host_public_key(&format!("{key}\nother-key")).is_err());
        assert!(
            normalize_host_public_key(&format!(
                "ssh-rsa {} misleading-comment",
                key.split_ascii_whitespace().nth(1).unwrap()
            ))
            .is_err()
        );
    }

    #[test]
    fn hides_service_and_codex_sandbox_accounts_from_ssh_approval() {
        for hidden in [
            "ASPNET",
            "DefaultAccount",
            "Guest",
            "WDAGUtilityAccount",
            "CodexSandboxOffline",
            "CodexSandboxOnline",
        ] {
            assert!(!visible_account(hidden));
        }
        assert!(visible_account("12345"));
        assert!(visible_account("Administrator"));
        assert!(visible_account("测试用户"));
    }

    #[test]
    fn inspected_accounts_apply_the_same_visibility_filter() {
        let inspect_script = include_str!("inspect.ps1");
        assert!(inspect_script.contains("$_.SID.Value -eq $currentSid"));
        assert!(!inspect_script.contains("Sort-Object"));
        let info = inspect(&std::env::temp_dir()).unwrap();
        assert!(info.accounts.len() <= 1);
        assert!(
            info.accounts
                .iter()
                .all(|account| visible_account(&account.name))
        );
    }
}
