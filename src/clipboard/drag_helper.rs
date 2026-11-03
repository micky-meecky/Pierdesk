#![allow(clippy::inline_always, clippy::ref_as_ptr)]

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{
    CloseHandle, E_POINTER, ERROR_INSUFFICIENT_BUFFER, HANDLE, POINTL,
};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_CONNECTIONS,
};
use windows::Win32::Networking::WinSock::AF_INET;
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, CoCreateInstance, IDataObject, IDispatch, IServiceProvider,
};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl, OleInitialize,
    OleUninitialize,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::System::Threading::{
    CreateProcessW, GetStartupInfoW, OpenProcess, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    STARTF_FORCEOFFFEEDBACK, STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows, IsUserAnAdmin,
    SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE,
    ShowWindow,
};
use windows_core::{BSTR, Error, Interface, PWSTR, Ref, Result, implement};

use super::catch_com_result;
use super::drag_live::{drag_source_diagnostic, real_paths};
use super::drag_probe::{DragProbeEdge, RegisteredBoundaryWindow};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const HELPER_START_TIMEOUT: Duration = Duration::from_secs(5);
const HELPER_IO_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize, Serialize)]
enum HelperEvent {
    Hello {
        token: String,
        elevated: bool,
        process_id: u32,
        bounds: [i32; 4],
        startup_flags: u32,
    },
    DragEnter {
        paths_utf16: Vec<Vec<u16>>,
    },
    CaptureRejected {
        reason: String,
    },
    VisibilityApplied {
        visible: bool,
    },
}

#[derive(Debug, Deserialize, Serialize)]
enum HelperCommand {
    Visibility { visible: bool },
    VisibilityProbe { visible: bool },
    Clear,
    Stop,
}

struct FrameReader {
    stream: TcpStream,
    buffered: Vec<u8>,
}

impl FrameReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffered: Vec::new(),
        }
    }

    fn poll<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Option<T>> {
        let mut chunk = [0_u8; 8192];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "drag helper disconnected",
                    ));
                }
                Ok(read) => {
                    self.buffered.extend_from_slice(&chunk[..read]);
                    if self.buffered.len() > MAX_FRAME_BYTES + 4 {
                        return Err(invalid_data("drag helper frame exceeds limit"));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        decode_buffered_frame(&mut self.buffered)
    }
}

fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> io::Result<()> {
    let encoded = serde_json::to_vec(value).map_err(invalid_data)?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(invalid_data("drag helper frame exceeds limit"));
    }
    let length = u32::try_from(encoded.len()).map_err(invalid_data)?;
    let mut frame = Vec::with_capacity(encoded.len() + 4);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&encoded);
    stream.write_all(&frame)
}

fn read_frame_blocking<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> io::Result<T> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_le_bytes(length)).map_err(invalid_data)?;
    if length > MAX_FRAME_BYTES {
        return Err(invalid_data("drag helper frame exceeds limit"));
    }
    let mut encoded = vec![0_u8; length];
    stream.read_exact(&mut encoded)?;
    serde_json::from_slice(&encoded).map_err(invalid_data)
}

fn decode_buffered_frame<T: for<'de> Deserialize<'de>>(
    buffered: &mut Vec<u8>,
) -> io::Result<Option<T>> {
    if buffered.len() < 4 {
        return Ok(None);
    }
    let length = usize::try_from(u32::from_le_bytes(
        buffered[..4].try_into().expect("four-byte prefix"),
    ))
    .map_err(invalid_data)?;
    if length > MAX_FRAME_BYTES {
        return Err(invalid_data("drag helper frame exceeds limit"));
    }
    if buffered.len() < length + 4 {
        return Ok(None);
    }
    let value = serde_json::from_slice(&buffered[4..length + 4]).map_err(invalid_data)?;
    buffered.drain(..length + 4);
    Ok(Some(value))
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub(super) struct DragCaptureHelper {
    writer: TcpStream,
    reader: FrameReader,
    pub(super) process_id: u32,
    pub(super) elevated: bool,
    pub(super) bounds: [i32; 4],
    pub(super) startup_flags: u32,
}

pub(super) enum DragCaptureEvent {
    Paths(Vec<PathBuf>),
    Rejected(String),
}

impl DragCaptureHelper {
    #[allow(clippy::too_many_lines)]
    pub(super) fn start(
        runtime_root: &Path,
        edge: DragProbeEdge,
        thickness: u32,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let token = nonce
            .iter()
            .fold(String::with_capacity(64), |mut text, byte| {
                use std::fmt::Write as _;
                let _ = write!(text, "{byte:02x}");
                text
            });
        let (helper_path, helper_digest) = prepare_helper_copy(runtime_root)?;
        launch_through_desktop_shell(
            &helper_path,
            &format!(
                "file-drag-edge-launcher --port {} --token {} --edge {} --thickness {thickness}",
                address.port(),
                token,
                edge.label(),
            ),
        )?;

        let deadline = Instant::now() + HELPER_START_TIMEOUT;
        let mut last_rejection = None;
        loop {
            match listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        last_rejection = Some("non-loopback client".to_owned());
                        continue;
                    }
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(HELPER_IO_TIMEOUT))?;
                    stream.set_write_timeout(Some(HELPER_IO_TIMEOUT))?;
                    let (received, elevated, process_id, bounds, startup_flags) =
                        match read_frame_blocking::<HelperEvent>(&mut stream) {
                            Ok(HelperEvent::Hello {
                                token,
                                elevated,
                                process_id,
                                bounds,
                                startup_flags,
                            }) => (token, elevated, process_id, bounds, startup_flags),
                            Ok(other) => {
                                last_rejection =
                                    Some(format!("unexpected helper hello frame: {other:?}"));
                                continue;
                            }
                            Err(error) => {
                                last_rejection = Some(format!("invalid helper hello: {error}"));
                                continue;
                            }
                        };
                    if received != token {
                        last_rejection = Some("helper token mismatch".to_owned());
                        continue;
                    }
                    if startup_flags & STARTF_FORCEOFFFEEDBACK.0 == 0 {
                        last_rejection =
                            Some("helper GUI startup feedback was not disabled".to_owned());
                        continue;
                    }
                    let owner_process_id = tcp_client_process_id(&stream)?;
                    if owner_process_id != process_id {
                        last_rejection = Some(format!(
                            "helper socket PID {owner_process_id} did not match hello PID {process_id}"
                        ));
                        continue;
                    }
                    if !process_image_matches(process_id, &helper_path)? {
                        last_rejection = Some("helper process image did not match".to_owned());
                        continue;
                    }
                    if hash_file(&helper_path)? != helper_digest {
                        last_rejection = Some("helper process image hash did not match".to_owned());
                        continue;
                    }
                    // The helper is launched by Explorer's desktop automation object so it has
                    // the same integrity level as the interactive shell. On machines where UAC is
                    // disabled (or Explorer itself is elevated), that legitimate desktop token is
                    // also elevated. Elevation alone therefore cannot be treated as an authentication
                    // failure; the nonce, loopback socket owner PID, exact image path and image hash
                    // above bind this connection to the helper copy we launched.
                    stream.set_read_timeout(None)?;
                    stream.set_write_timeout(Some(HELPER_IO_TIMEOUT))?;
                    stream.set_nonblocking(true)?;
                    let reader = FrameReader::new(stream.try_clone()?);
                    return Ok(Self {
                        writer: stream,
                        reader,
                        process_id,
                        elevated,
                        bounds,
                        startup_flags,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            last_rejection.map_or_else(
                                || "desktop drag helper did not connect".to_owned(),
                                |reason| format!("desktop drag helper was rejected: {reason}"),
                            ),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn set_visible(&mut self, visible: bool) -> io::Result<()> {
        write_frame(&mut self.writer, &HelperCommand::Visibility { visible })
    }

    pub(super) fn clear(&mut self) -> io::Result<()> {
        write_frame(&mut self.writer, &HelperCommand::Clear)
    }

    pub(super) fn poll(&mut self) -> io::Result<Option<DragCaptureEvent>> {
        match self.reader.poll::<HelperEvent>()? {
            Some(HelperEvent::DragEnter { paths_utf16 }) => paths_utf16
                .into_iter()
                .map(|path| {
                    let path = PathBuf::from(OsString::from_wide(&path));
                    if path.is_absolute() {
                        Ok(path)
                    } else {
                        Err(invalid_data("drag helper returned a relative path"))
                    }
                })
                .collect::<io::Result<Vec<_>>>()
                .map(|paths| Some(DragCaptureEvent::Paths(paths))),
            Some(HelperEvent::CaptureRejected { reason }) => {
                Ok(Some(DragCaptureEvent::Rejected(reason)))
            }
            Some(HelperEvent::Hello { .. }) => Err(invalid_data("duplicate drag helper hello")),
            Some(HelperEvent::VisibilityApplied { .. }) => Err(invalid_data(
                "unexpected diagnostic visibility acknowledgement",
            )),
            None => Ok(None),
        }
    }
}

impl Drop for DragCaptureHelper {
    fn drop(&mut self) {
        let _ = self.writer.set_nonblocking(false);
        let _ = write_frame(&mut self.writer, &HelperCommand::Stop);
    }
}

fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    Ok(Sha256::digest(fs::read(path)?).into())
}

fn prepare_helper_copy(runtime_root: &Path) -> io::Result<(PathBuf, [u8; 32])> {
    let executable = std::env::current_exe()?;
    let digest = hash_file(&executable)?;
    let suffix = digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        });
    let directory = runtime_root.join("runtime");
    fs::create_dir_all(&directory)?;
    let helper = directory.join(format!("pierdesk-drag-helper-{suffix}.exe"));
    let valid = hash_file(&helper).is_ok_and(|existing| existing == digest);
    if !valid {
        fs::copy(executable, &helper)?;
    }
    prune_old_helpers(&directory, &helper);
    Ok((helper, digest))
}

fn prune_old_helpers(directory: &Path, current: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_helper = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("pierdesk-drag-helper-")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            });
        if is_helper && path != current {
            // A still-running helper keeps its image locked. It will be reclaimed on a later start.
            let _ = fs::remove_file(path);
        }
    }
}

fn tcp_client_process_id(stream: &TcpStream) -> io::Result<u32> {
    let server_port = stream.local_addr()?.port();
    let client_port = stream.peer_addr()?.port();
    for _ in 0..10 {
        if let Some(process_id) = tcp_owner_pid(client_port, server_port)? {
            return Ok(process_id);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "could not authenticate the drag helper socket owner",
    ))
}

fn tcp_owner_pid(local_port: u16, remote_port: u16) -> io::Result<Option<u32>> {
    let mut size = 0_u32;
    let status = unsafe {
        GetExtendedTcpTable(
            None,
            &raw mut size,
            false,
            u32::from(AF_INET.0),
            TCP_TABLE_OWNER_PID_CONNECTIONS,
            0,
        )
    };
    if status != 0 && status != ERROR_INSUFFICIENT_BUFFER.0 {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }
    let words = usize::try_from(size)
        .map_err(invalid_data)?
        .div_ceil(std::mem::size_of::<u32>());
    let mut table = vec![0_u32; words.max(1)];
    let status = unsafe {
        GetExtendedTcpTable(
            Some(table.as_mut_ptr().cast()),
            &raw mut size,
            false,
            u32::from(AF_INET.0),
            TCP_TABLE_OWNER_PID_CONNECTIONS,
            0,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }
    let count = usize::try_from(table[0]).map_err(invalid_data)?;
    let required = std::mem::size_of::<u32>()
        .checked_add(
            count
                .checked_mul(std::mem::size_of::<MIB_TCPROW_OWNER_PID>())
                .ok_or_else(|| invalid_data("TCP owner table size overflow"))?,
        )
        .ok_or_else(|| invalid_data("TCP owner table size overflow"))?;
    if required > usize::try_from(size).map_err(invalid_data)? {
        return Err(invalid_data("truncated TCP owner table"));
    }
    let rows = unsafe {
        std::slice::from_raw_parts(table.as_ptr().add(1).cast::<MIB_TCPROW_OWNER_PID>(), count)
    };
    Ok(rows
        .iter()
        .find(|row| {
            decode_mib_port(row.dwLocalPort) == Some(local_port)
                && decode_mib_port(row.dwRemotePort) == Some(remote_port)
        })
        .map(|row| row.dwOwningPid))
}

fn decode_mib_port(value: u32) -> Option<u16> {
    u16::try_from(value).ok().map(u16::from_be)
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn process_image_matches(process_id: u32, expected: &Path) -> io::Result<bool> {
    Ok(normalized_path(&process_image(process_id)?) == normalized_path(expected))
}

pub(super) fn process_image(process_id: u32) -> io::Result<PathBuf> {
    let process = ProcessHandle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
            .map_err(io::Error::other)?,
    );
    let mut buffer = vec![0_u16; 32_768];
    let mut length = u32::try_from(buffer.len()).map_err(invalid_data)?;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut length,
        )
    }
    .map_err(io::Error::other)?;
    let actual = OsString::from_wide(&buffer[..usize::try_from(length).map_err(invalid_data)?]);
    Ok(PathBuf::from(actual))
}

pub(super) fn foreground_process_diagnostic() -> String {
    let window = unsafe { GetForegroundWindow() };
    let mut process_id = 0_u32;
    let thread_id = unsafe { GetWindowThreadProcessId(window, Some(&raw mut process_id)) };
    let process = process_image(process_id)
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unavailable".to_owned())
        .replace([' ', '\t', '\r', '\n'], "_");
    format!(
        "foreground_thread={thread_id} foreground_pid={process_id} foreground_process={process}"
    )
}

fn normalized_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn launch_through_desktop_shell(executable: &Path, arguments: &str) -> io::Result<()> {
    // Resolve the automation object from Explorer's desktop view. Calling the in-process
    // Shell.Application class from an administrator process would inherit that elevation;
    // Explorer's out-of-process view instead launches with the interactive desktop token. That
    // token can itself be elevated when UAC is disabled, which is still the correct token for OLE
    // drag/drop on that desktop.
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|error| io::Error::other(format!("create ShellWindows: {error}")))?;
    let empty = VARIANT::default();
    let mut window = 0_i32;
    let desktop = unsafe {
        windows.FindWindowSW(
            &raw const empty,
            &raw const empty,
            SWC_DESKTOP,
            &raw mut window,
            SWFO_NEEDDISPATCH,
        )
    }
    .map_err(|error| io::Error::other(format!("find desktop shell window: {error}")))?;
    let provider: IServiceProvider = desktop
        .cast()
        .map_err(|error| io::Error::other(format!("query desktop service provider: {error}")))?;
    let top_level_browser = SID_STopLevelBrowser;
    let browser: IShellBrowser = unsafe { provider.QueryService(&raw const top_level_browser) }
        .map_err(|error| io::Error::other(format!("query desktop shell browser: {error}")))?;
    let view = unsafe { browser.QueryActiveShellView() }
        .map_err(|error| io::Error::other(format!("query active desktop view: {error}")))?;
    let background: IDispatch = unsafe { view.GetItemObject(SVGIO_BACKGROUND) }
        .map_err(|error| io::Error::other(format!("query desktop folder view: {error}")))?;
    let folder: IShellFolderViewDual = background
        .cast()
        .map_err(|error| io::Error::other(format!("query desktop folder automation: {error}")))?;
    let shell: IShellDispatch2 = unsafe { folder.Application() }
        .and_then(|application| application.cast())
        .map_err(|error| io::Error::other(format!("query desktop shell automation: {error}")))?;
    let executable = BSTR::from(executable.to_string_lossy().as_ref());
    let arguments = VARIANT::from(BSTR::from(arguments));
    let hidden = VARIANT::from(SW_HIDE.0);
    unsafe { shell.ShellExecute(&executable, &arguments, &empty, &empty, &hidden) }
        .map_err(|error| io::Error::other(format!("desktop ShellExecute: {error}")))
}

struct OleApartment;

impl OleApartment {
    fn new() -> io::Result<Self> {
        unsafe { OleInitialize(None) }.map_err(io::Error::other)?;
        Ok(Self)
    }
}

impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

#[implement(IDropTarget)]
struct HelperDropTarget {
    writer: Arc<Mutex<TcpStream>>,
    retained: Rc<RefCell<Option<IDataObject>>>,
}

impl HelperDropTarget {
    fn send(&self, event: &HelperEvent) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = write_frame(&mut writer, event);
        }
    }
}

impl IDropTarget_Impl for HelperDropTarget_Impl {
    fn DragEnter(
        &self,
        object: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        catch_com_result(|| {
            if effect.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            unsafe { effect.write(DROPEFFECT_NONE) };
            let object = object
                .cloned()
                .ok_or_else(|| Error::from_hresult(E_POINTER))?;
            match real_paths(&object) {
                Ok(paths) => {
                    let paths_utf16 = paths
                        .iter()
                        .map(|path| path.as_os_str().encode_wide().collect())
                        .collect();
                    *self.retained.borrow_mut() = Some(object);
                    self.send(&HelperEvent::DragEnter { paths_utf16 });
                    unsafe { effect.write(DROPEFFECT_COPY) };
                }
                Err(error) => self.send(&HelperEvent::CaptureRejected {
                    reason: format!(
                        "{error} {} {}",
                        drag_source_diagnostic(&object),
                        foreground_process_diagnostic()
                    ),
                }),
            }
            Ok(())
        })
    }

    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        if effect.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        unsafe {
            effect.write(if self.retained.borrow().is_some() {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            });
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        _: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        if effect.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        unsafe { effect.write(DROPEFFECT_NONE) };
        Ok(())
    }
}

/// Runs the native edge receiver used by an elevated `Pierdesk` parent.
///
/// # Errors
///
/// Returns an error when OLE/window setup fails, the authenticated parent cannot be reached, or
/// the local IPC channel closes unexpectedly.
pub fn run_drag_edge_helper(
    port: u16,
    token: String,
    edge: DragProbeEdge,
    thickness: u32,
) -> io::Result<()> {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_data("invalid drag helper token"));
    }
    let _ole = OleApartment::new()?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
        HELPER_START_TIMEOUT,
    )?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(Some(HELPER_IO_TIMEOUT))?;
    let retained = Rc::new(RefCell::new(None));
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let target: IDropTarget = HelperDropTarget {
        writer,
        retained: Rc::clone(&retained),
    }
    .into();
    let window = RegisteredBoundaryWindow::create_with_visibility(edge, thickness, &target, false)?;
    unsafe {
        let _ = ShowWindow(window.handle, SW_HIDE);
    }
    let bounds = super::drag_live::window_bounds(window.handle);
    let mut startup = STARTUPINFOW::default();
    unsafe { GetStartupInfoW(&raw mut startup) };
    write_frame(
        &mut stream,
        &HelperEvent::Hello {
            token,
            elevated: unsafe { IsUserAnAdmin() }.as_bool(),
            process_id: std::process::id(),
            bounds,
            startup_flags: startup.dwFlags.0,
        },
    )?;
    stream.set_nonblocking(true)?;
    let mut reader = FrameReader::new(stream);
    loop {
        super::drag_live::pump_messages();
        while let Some(command) = reader.poll::<HelperCommand>()? {
            match command {
                HelperCommand::Visibility { visible } => unsafe {
                    let _ = ShowWindow(
                        window.handle,
                        if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
                    );
                },
                HelperCommand::VisibilityProbe { visible } => {
                    unsafe {
                        let _ = ShowWindow(
                            window.handle,
                            if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
                        );
                    }
                    write_frame(
                        &mut reader.stream,
                        &HelperEvent::VisibilityApplied {
                            visible: unsafe { IsWindowVisible(window.handle) }.as_bool(),
                        },
                    )?;
                }
                HelperCommand::Clear => {
                    retained.borrow_mut().take();
                }
                HelperCommand::Stop => return Ok(()),
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Start the background receiver with the desktop token but without Shell's GUI
/// startup cursor feedback. The short-lived launcher itself creates no window.
///
/// # Errors
/// Returns executable lookup or native process creation errors.
pub fn launch_drag_edge_helper(
    port: u16,
    token: &str,
    edge: DragProbeEdge,
    thickness: u32,
) -> io::Result<()> {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_data("invalid drag helper token"));
    }
    let executable = std::env::current_exe()?;
    let application: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut command = OsString::from("\"");
    command.push(&executable);
    command.push(format!(
        "\" file-drag-edge-helper --port {port} --token {token} --edge {} --thickness {thickness}",
        edge.label()
    ));
    let mut command: Vec<u16> = command.encode_wide().chain(Some(0)).collect();
    let startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>()).map_err(invalid_data)?,
        dwFlags: STARTF_FORCEOFFFEEDBACK | STARTF_USESHOWWINDOW,
        wShowWindow: 0,
        ..STARTUPINFOW::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // No inherited handles or changed token. Nonce / PID / image authentication
    // in the parent still applies to the actual receiver, not the launcher.
    unsafe {
        CreateProcessW(
            windows_core::PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            PROCESS_CREATION_FLAGS(0),
            None,
            None,
            &raw const startup,
            &raw mut process,
        )
    }
    .map_err(io::Error::other)?;
    let _thread = ProcessHandle(process.hThread);
    let _process = ProcessHandle(process.hProcess);
    Ok(())
}

/// Launches the native edge helper and verifies its authenticated IPC handshake.
///
/// # Errors
///
/// Returns an error when the helper cannot be copied, launched through the interactive desktop,
/// authenticated, or controlled through its loopback channel.
pub fn run_drag_helper_self_test(runtime_root: &Path) -> io::Result<(u32, bool, [i32; 4])> {
    let _ole = OleApartment::new()?;
    let mut helper = DragCaptureHelper::start(runtime_root, DragProbeEdge::Top, 8)?;
    helper.set_visible(false)?;
    // Exercise Explorer-launched visibility, not merely the IPC handshake. This
    // explicit diagnostic never moves the pointer or synthesizes button input.
    for _ in 0..12 {
        for visible in [true, false] {
            write_frame(
                &mut helper.writer,
                &HelperCommand::VisibilityProbe { visible },
            )?;
            let deadline = Instant::now() + HELPER_IO_TIMEOUT;
            loop {
                match helper.reader.poll::<HelperEvent>()? {
                    Some(HelperEvent::VisibilityApplied { visible: actual })
                        if actual == visible =>
                    {
                        break;
                    }
                    Some(_) => return Err(invalid_data("helper visibility verification failed")),
                    None if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(8));
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "helper visibility verification timed out",
                        ));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(180));
        }
    }
    let result = (helper.process_id, helper.elevated, helper.bounds);
    drop(helper);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_rejects_invalid_nonce_before_spawning() {
        assert!(launch_drag_edge_helper(1, "invalid", DragProbeEdge::Top, 8).is_err());
    }

    #[test]
    fn framed_protocol_handles_partial_and_multiple_messages() {
        let first = HelperCommand::Visibility { visible: true };
        let second = HelperCommand::Clear;
        let mut bytes = Vec::new();
        for command in [&first, &second] {
            let encoded = serde_json::to_vec(command).unwrap();
            bytes.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(&encoded);
        }
        let suffix = bytes.split_off(3);
        assert!(
            decode_buffered_frame::<HelperCommand>(&mut bytes)
                .unwrap()
                .is_none()
        );
        bytes.extend_from_slice(&suffix);
        assert!(matches!(
            decode_buffered_frame::<HelperCommand>(&mut bytes).unwrap(),
            Some(HelperCommand::Visibility { visible: true })
        ));
        assert!(matches!(
            decode_buffered_frame::<HelperCommand>(&mut bytes).unwrap(),
            Some(HelperCommand::Clear)
        ));
    }

    #[test]
    fn helper_rejects_relative_paths_from_ipc() {
        let event = HelperEvent::DragEnter {
            paths_utf16: vec![OsString::from("relative.txt").encode_wide().collect()],
        };
        let encoded = serde_json::to_vec(&event).unwrap();
        let decoded: HelperEvent = serde_json::from_slice(&encoded).unwrap();
        let HelperEvent::DragEnter { paths_utf16 } = decoded else {
            panic!("unexpected event");
        };
        assert!(!PathBuf::from(OsString::from_wide(&paths_utf16[0])).is_absolute());
    }

    #[test]
    fn helper_connection_is_bound_to_its_real_process() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let connector = std::thread::spawn(move || TcpStream::connect(address).unwrap());
        let (stream, _) = listener.accept().unwrap();
        let client = connector.join().unwrap();
        assert_eq!(tcp_client_process_id(&stream).unwrap(), std::process::id());
        assert!(
            process_image_matches(std::process::id(), &std::env::current_exe().unwrap()).unwrap()
        );
        drop(client);
    }
}
