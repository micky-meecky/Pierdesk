use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetLastError, HANDLE,
    HINSTANCE, HWND, LPARAM, LRESULT, POINT, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::Console::FreeConsole;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyW, RegDeleteValueW,
    RegGetValueW, RegSetValueExW,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
    ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    IsUserAnAdmin, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, SHCNE_ASSOCCHANGED,
    SHCNE_UPDATEITEM, SHCNF_FLUSH, SHCNF_IDLIST, SHCNF_PATHW, SHChangeNotify, Shell_NotifyIconW,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowExW,
    GWLP_USERDATA, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON,
    HWND_MESSAGE, IDI_APPLICATION, IDI_INFORMATION, IDYES, KillTimer, LR_DEFAULTCOLOR, LoadIconW,
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_YESNO,
    MENU_ITEM_FLAGS, MF_CHECKED, MF_DISABLED, MF_SEPARATOR, MF_STRING, MSG, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
    SM_CYSMICON, SW_SHOWNORMAL, SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, UnregisterClassW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_COMMAND, WM_DESTROY, WM_ENDSESSION, WM_HOTKEY,
    WM_LBUTTONDBLCLK, WM_NCCREATE, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
};
use windows_core::{PCWSTR, w};

use crate::app_settings::{
    AppSettings, CrossScreenSettings, MAX_UI_FONT_SCALE_PERCENT, MIN_UI_FONT_SCALE_PERCENT,
    SharedClipboardSettings, TransferWindowSettings, UiLanguage, UiPreferences, UiTheme,
};
use crate::clipboard::{
    ProductCommand, ProductRuntime, ProductSnapshot, ProductTransferDirection, ProductTransferId,
    ProductTransferState,
};
use crate::cross_screen::{
    BoundaryLayoutV3, BoundaryMapConstraints, BoundaryMapV2, BoundarySyncPhase,
    BoundarySyncSurface, BoundaryWorkspaceV3, CrossScreenProductRuntime,
    CrossScreenProductSnapshot, CrossScreenProductState, DEFAULT_MINIMUM_SEGMENT_PIXELS,
    LOGICAL_BOUNDARY_CORNER_GUARD_PIXELS, PeerPlacement, RemoteBoundaryWorkspace,
    SynchronizedScreenCatalog, SynchronizedScreenInfo, accept_remote_preview,
    current_physical_primary_screen_size, current_primary_screen_millimeters, encode_boundary_sync,
    encode_boundary_sync_with_initiator, initialize_from_remote_preview,
    merge_matching_confirmations, remote_pair_geometry_matches,
};
use crate::device_store::DeviceStore;
use crate::discovery::{
    DiscoveryRuntime, DiscoveryView, NetworkPathIssue, PAIRING_PORT, activate_discovered_peer,
    validate_saved_network_path,
};
use crate::pairing::{PendingPairing, connect_for_pairing, listen_for_pairing};
use crate::screen_info_sync::ScreenInfoSyncRuntime;
use crate::settings_window::{SettingsWindow, SettingsWindowCommand, SettingsWindowCommands};
use crate::transfer_window::{TransferWindow, TransferWindowCommands, persisted_view_preferences};

const TRAY_WINDOW_CLASS: PCWSTR = w!("PierdeskTrayWindow");
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;
const TRAY_SHOW_STATUS_MESSAGE: u32 = WM_APP + 2;
const TRAY_EXIT_MESSAGE: u32 = WM_APP + 3;
const TRAY_RELOAD_MESSAGE: u32 = WM_APP + 4;
const TRAY_RUNTIME_ERROR_MESSAGE: u32 = WM_APP + 5;
const TRAY_QUERY_ELEVATED_MESSAGE: u32 = WM_APP + 6;
const TRAY_ICON_ID: u32 = 1;
const TRAY_REFRESH_TIMER_ID: usize = 0x4346_5452;
const TRAY_REFRESH_TIMER_MS: u32 = 500;
const TRAY_ICON_HEALTH_INTERVAL: Duration = Duration::from_secs(5);
const TRAY_ICON_ADD_RETRY_LOG_INTERVAL: Duration = Duration::from_mins(1);
const SINGLE_INSTANCE_NAME: PCWSTR = w!("Local\\ClipFerry.Tray.v1");
const SINGLE_INSTANCE_HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("ClipFerry");
static AUTOSTART_CACHE: std::sync::Mutex<Option<(Instant, AutostartState)>> =
    std::sync::Mutex::new(None);
const ICON_BYTES: &[u8] = include_bytes!("../assets/brand/pierdesk.ico");
const SHELL_MAX_PATH: usize = 260;

const COMMAND_STATUS: usize = 1001;
const COMMAND_PAIR: usize = 1002;
const COMMAND_MANAGE_PEERS: usize = 1003;
const COMMAND_PAUSE: usize = 1004;
const COMMAND_RESUME: usize = 1005;
const COMMAND_CANCEL: usize = 1006;
const COMMAND_AUTOSTART: usize = 1007;
const COMMAND_LOG: usize = 1008;
const COMMAND_EXIT: usize = 1009;
const COMMAND_SETTINGS: usize = 1010;
const COMMAND_ACCEPT_PENDING: usize = 1011;
const COMMAND_TRANSFER_WINDOW: usize = 1012;
const COMMAND_ABOUT: usize = 1013;
const COMMAND_TRANSFER_DISPATCH: usize = 1014;
const COMMAND_SETTINGS_DISPATCH: usize = 1015;
const COMMAND_CROSS_SCREEN_ENABLE: usize = 1020;
const COMMAND_CROSS_SCREEN_PAUSE: usize = 1021;
const COMMAND_CROSS_SCREEN_RESUME: usize = 1022;
const COMMAND_CROSS_SCREEN_RETURN_LOCAL: usize = 1023;
const COMMAND_CROSS_SCREEN_LEFT: usize = 1024;
const COMMAND_CROSS_SCREEN_RIGHT: usize = 1025;
const COMMAND_CROSS_SCREEN_ABOVE: usize = 1026;
const COMMAND_CROSS_SCREEN_BELOW: usize = 1027;
const COMMAND_CROSS_SCREEN_CORNER_GUARD: usize = 1028;
const COMMAND_CROSS_SCREEN_CORNER_SIZE: usize = 1029;
const COMMAND_CROSS_SCREEN_SECOND_PUSH: usize = 1030;
const COMMAND_CROSS_SCREEN_FULLSCREEN_PAUSE: usize = 1031;
const COMMAND_CROSS_SCREEN_ADMIN_RESTART: usize = 1032;
const COMMAND_CROSS_SCREEN_SETTINGS: usize = 1033;
const COMMAND_SHARED_CLIPBOARD_ENABLE: usize = 1040;
const COMMAND_TRANSFER_WINDOW_AUTO_POPUP: usize = 1041;
const COMMAND_SCREENSHOT: usize = 1042;

/// Notifies Windows Shell that the running executable changed in place.
///
/// This invalidates Shell's icon cache before targeting the current executable. It lets Desktop
/// and File Explorer discard a stale system-image-list entry after a portable same-name upgrade.
pub fn notify_shell_current_executable_updated() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let wide_path: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if wide_path.len() > SHELL_MAX_PATH {
        return;
    }

    // SAFETY: SHCNE_ASSOCCHANGED takes no item pointers with SHCNF_IDLIST. Microsoft documents it
    // as the notification that invalidates Shell's icon and thumbnail cache. SHCNF_FLUSH waits
    // until affected Shell components receive the invalidation.
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSH, None, None);
    }

    // SAFETY: `wide_path` is a live, null-terminated absolute UTF-16 path. After invalidating the
    // cached image, the targeted update makes Desktop and File Explorer reload this exact item.
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEITEM,
            SHCNF_PATHW | SHCNF_FLUSH,
            Some(wide_path.as_ptr().cast()),
            None,
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutostartState {
    Disabled,
    Enabled,
    Stale,
}

impl AutostartState {
    const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Stale => "stale",
        }
    }
}

struct SingleInstance {
    handle: HANDLE,
    owned: bool,
}

enum SingleInstanceAcquisition {
    Primary(SingleInstance),
    Existing(HANDLE),
}

impl SingleInstance {
    fn acquire() -> io::Result<SingleInstanceAcquisition> {
        Self::acquire_named(SINGLE_INSTANCE_NAME)
    }

    fn acquire_named(name: PCWSTR) -> io::Result<SingleInstanceAcquisition> {
        // The primary process owns this mutex for its entire tray lifetime. Ownership, rather
        // than the mere existence of a kernel handle, lets a replacement wait for an exiting
        // instance to finish its worker teardown without racing the already-destroyed tray
        // window.
        // SAFETY: the mutex uses a process-lifetime constant name and no security descriptor.
        let handle = unsafe { CreateMutexW(None, true, name) }.map_err(windows_error)?;
        // SAFETY: GetLastError reads thread-local status immediately after CreateMutexW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Ok(SingleInstanceAcquisition::Existing(handle));
        }
        Ok(SingleInstanceAcquisition::Primary(Self {
            handle,
            owned: true,
        }))
    }

    fn wait_for_exiting_instance(handle: HANDLE) -> io::Result<Self> {
        let timeout = u32::try_from(SINGLE_INSTANCE_HANDOFF_TIMEOUT.as_millis())
            .expect("single-instance handoff timeout fits u32");
        // SAFETY: handle refers to the existing named mutex and remains owned by this function.
        let wait = unsafe { WaitForSingleObject(handle, timeout) };
        match wait {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self {
                handle,
                owned: true,
            }),
            WAIT_TIMEOUT => {
                // SAFETY: the wait timed out without transferring mutex ownership.
                let _ = unsafe { CloseHandle(handle) };
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "previous Pierdesk instance did not finish exiting within 5 seconds",
                ))
            }
            _ => {
                let error = io::Error::last_os_error();
                // SAFETY: the failed wait did not transfer mutex ownership.
                let _ = unsafe { CloseHandle(handle) };
                Err(error)
            }
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: this guard acquired and still owns the mutex on the current thread.
            let _ = unsafe { ReleaseMutex(self.handle) };
            self.owned = false;
        }
        // SAFETY: the guard uniquely owns the mutex handle.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

struct PairingRuntime {
    stop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    event_sender: std::sync::mpsc::Sender<PairingEvent>,
    events: std::sync::mpsc::Receiver<PairingEvent>,
    worker: Option<JoinHandle<()>>,
}

struct PendingPairingRequest {
    pending: PendingPairing,
    discovered: crate::discovery::DiscoveredPeer,
}

enum PairingEvent {
    Pending(Box<PendingPairingRequest>),
    Completed(io::Result<String>),
}

impl PairingRuntime {
    fn start(store: DeviceStore, discovery: DiscoveryView, log_path: PathBuf) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        let (event_sender, events) = std::sync::mpsc::channel();
        let worker_stop = Arc::clone(&stop);
        let worker_busy = Arc::clone(&busy);
        let worker_events = event_sender.clone();
        let worker = thread::Builder::new()
            .name("pierdesk-pairing-listener".to_owned())
            .spawn(move || {
                pairing_listener_loop(
                    &store,
                    &discovery,
                    &log_path,
                    &worker_stop,
                    &worker_busy,
                    &worker_events,
                );
            })?;
        Ok(Self {
            stop,
            busy,
            event_sender,
            events,
            worker: Some(worker),
        })
    }

    fn connect(
        &self,
        store: DeviceStore,
        discovery: DiscoveryView,
        peer: crate::discovery::DiscoveredPeer,
        log_path: PathBuf,
    ) -> io::Result<()> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "已有配对确认正在进行",
            ));
        }
        let busy = Arc::clone(&self.busy);
        let events = self.event_sender.clone();
        thread::Builder::new()
            .name("pierdesk-pairing-connector".to_owned())
            .spawn(move || {
                let result = connect_for_pairing(
                    store.clone(),
                    peer.pairing_endpoint,
                    Duration::from_mins(2),
                )
                .and_then(|pending| prepare_pairing(&store, &discovery, pending, Some(peer)));
                match result {
                    Ok(pending) => {
                        if events
                            .send(PairingEvent::Pending(Box::new(pending)))
                            .is_err()
                        {
                            busy.store(false, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        let _ = append_log(
                            &log_path,
                            &format!("pairing_prepare completed=false error={error}"),
                        );
                        let _ = events.send(PairingEvent::Completed(Err(error)));
                        busy.store(false, Ordering::Release);
                    }
                }
            })?;
        Ok(())
    }

    fn take_events(&self) -> Vec<PairingEvent> {
        self.events.try_iter().collect()
    }

    fn confirm(
        &self,
        store: DeviceStore,
        request: PendingPairingRequest,
        approved: bool,
        log_path: PathBuf,
    ) -> io::Result<()> {
        let busy = Arc::clone(&self.busy);
        let completion_busy = Arc::clone(&busy);
        let events = self.event_sender.clone();
        let spawn = thread::Builder::new()
            .name("pierdesk-pairing-confirmation".to_owned())
            .spawn(move || {
                let label = request.discovered.label.clone();
                let result = if approved {
                    request.pending.confirm(true, &label).and_then(|peer| {
                        activate_discovered_peer(&store, &request.discovered)?;
                        Ok(format!("已与“{}”安全配对", peer.label))
                    })
                } else {
                    let _ = request.pending.confirm(false, &label);
                    Ok(format!("已拒绝“{label}”的配对请求"))
                };
                let message = match &result {
                    Ok(message) => format!("pairing_confirmation completed=true message={message}"),
                    Err(error) => format!("pairing_confirmation completed=false error={error}"),
                };
                let _ = append_log(&log_path, &message);
                let _ = events.send(PairingEvent::Completed(result));
                completion_busy.store(false, Ordering::Release);
            });
        match spawn {
            Ok(_) => Ok(()),
            Err(error) => {
                busy.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
}

impl Drop for PairingRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, PAIRING_PORT)),
            Duration::from_millis(200),
        );
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn pairing_listener_loop(
    store: &DeviceStore,
    discovery: &DiscoveryView,
    log_path: &Path,
    stop: &AtomicBool,
    busy: &AtomicBool,
    events: &std::sync::mpsc::Sender<PairingEvent>,
) {
    let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, PAIRING_PORT));
    while !stop.load(Ordering::Acquire) {
        let result = listen_for_pairing(store.clone(), address, Duration::from_mins(2));
        if stop.load(Ordering::Acquire) {
            break;
        }
        match result {
            Ok(pending) => {
                if busy
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    let _ = pending.confirm(false, "设备");
                    continue;
                }
                match prepare_pairing(store, discovery, pending, None) {
                    Ok(pending) => {
                        if events
                            .send(PairingEvent::Pending(Box::new(pending)))
                            .is_err()
                        {
                            busy.store(false, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        let _ = append_log(
                            log_path,
                            &format!("pairing_prepare completed=false error={error}"),
                        );
                        let _ = events.send(PairingEvent::Completed(Err(error)));
                        busy.store(false, Ordering::Release);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => {
                let _ = append_log(log_path, &format!("pairing_listener_error error={error}"));
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn prepare_pairing(
    store: &DeviceStore,
    discovery: &DiscoveryView,
    pending: PendingPairing,
    known_peer: Option<crate::discovery::DiscoveredPeer>,
) -> io::Result<PendingPairingRequest> {
    let fingerprint = pending.peer_fingerprint();
    let peer_address = pending.peer_address();
    let discovered = known_peer
        .filter(|peer| peer.fingerprint == fingerprint)
        .or_else(|| discovery.peer(fingerprint));
    let Some(discovered) = discovered else {
        let _ = pending.confirm(false, "未发现设备");
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "对端身份与局域网发现记录不匹配，已拒绝配对",
        ));
    };
    if discovered.service_endpoint.ip() != peer_address.ip() {
        let _ = pending.confirm(false, "地址不匹配的设备");
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "对端连接地址与局域网发现记录不匹配，已拒绝配对",
        ));
    }
    if store.load_peer(fingerprint).is_ok() {
        let _ = pending.confirm(false, &discovered.label);
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "该设备已经配对，无需重复确认",
        ));
    }
    Ok(PendingPairingRequest {
        pending,
        discovered,
    })
}

struct OwnedIcon {
    handle: HICON,
    owned: bool,
}

impl OwnedIcon {
    fn load() -> io::Result<Self> {
        if let Ok(image) = best_icon_image(ICON_BYTES) {
            // SAFETY: `image` is a validated, in-bounds ICO image resource and remains live for
            // the duration of the call. Windows creates an independent HICON on success.
            if let Ok(handle) = unsafe {
                CreateIconFromResourceEx(
                    image,
                    true,
                    0x0003_0000,
                    GetSystemMetrics(SM_CXSMICON),
                    GetSystemMetrics(SM_CYSMICON),
                    LR_DEFAULTCOLOR,
                )
            } {
                return Ok(Self {
                    handle,
                    owned: true,
                });
            }
        }
        // SAFETY: IDI_APPLICATION is a predefined shared system resource.
        let handle = unsafe { LoadIconW(None, IDI_APPLICATION) }.map_err(windows_error)?;
        Ok(Self {
            handle,
            owned: false,
        })
    }
}

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: owned icons are returned by CreateIconFromResourceEx exactly once.
            let _ = unsafe { DestroyIcon(self.handle) };
        }
    }
}

#[derive(Clone, Copy)]
enum BoundaryWorkspaceTransport {
    DeviceService,
    ActiveControlChannel,
}

struct TrayState {
    screenshot: crate::screenshot::Manager,
    license: crate::license_runtime::Runtime,
    cursor_locator: Option<crate::cursor_locator::CursorLocatorRuntime>,
    ssh: Option<crate::ssh::Runtime>,
    ssh_error: String,
    store: DeviceStore,
    log_path: PathBuf,
    active_log_path: PathBuf,
    icon: OwnedIcon,
    taskbar_created: u32,
    discovery: Option<DiscoveryRuntime>,
    discovery_error: Option<String>,
    screen_info_sync: Option<ScreenInfoSyncRuntime>,
    pairing: Option<PairingRuntime>,
    pending_pairing: Option<PendingPairingRequest>,
    pairing_message: Option<String>,
    runtime: Option<ProductRuntime>,
    runtime_error: Option<String>,
    cross_screen: Option<CrossScreenProductRuntime>,
    cross_screen_error: Option<String>,
    active_peer: Option<crate::security::CertificateFingerprint>,
    transfer_window: Option<TransferWindow>,
    transfer_window_settings: TransferWindowSettings,
    ui_language: UiLanguage,
    ui_theme: UiTheme,
    ui_font_scale_percent: u16,
    transfer_window_error: Option<String>,
    settings_window: Option<SettingsWindow>,
    settings_window_error: Option<String>,
    last_tooltip: String,
    last_icon_health_check: Instant,
    last_log_rotation_check: Instant,
    tray_icon_startup_retry_pending: bool,
    /// Set while the startup `NIM_ADD` has failed and the icon is not yet in the notification area.
    tray_icon_deferred_at: Option<Instant>,
    tray_icon_add_attempts: u32,
    last_icon_add_retry_log: Option<Instant>,
    last_cross_screen_log: String,
    last_network_path_log: String,
    last_network_notification: String,
    last_boundary_sync_sequence: u64,
    last_product_boundary_sync_sequence: u64,
    notified_terminal_transfers: HashSet<ProductTransferId>,
    notified_started_transfers: HashSet<ProductTransferId>,
    notified_ssh_requests: HashSet<String>,
    ssh_attention_icon: HICON,
    ssh_attention_active: bool,
    ssh_attention_phase: bool,
}

impl TrayState {
    fn confirm_ssh_exit(&self) -> bool {
        let count = self
            .ssh
            .as_ref()
            .map_or(0, crate::ssh::Runtime::active_session_count);
        count == 0
            || ask_yes_no(
                localized(
                    self.ui_language,
                    "仍有 SSH 会话。退出 Pierdesk 会断开这些连接，远端命令可能中断。确定退出？",
                    "SSH sessions are active. Quitting Pierdesk disconnects them and may interrupt remote commands. Quit?",
                ),
                "Pierdesk",
            )
    }
    #[allow(clippy::too_many_lines)] // Initialize owned runtimes together for predictable teardown.
    fn new() -> io::Result<Self> {
        let license = crate::license_runtime::Runtime::start()?;
        let store = DeviceStore::current_user()?;
        let stored = store.load_or_create_identity()?;
        let log_path = store.root().join("logs").join("clipferry.log");
        let active_log_path = crate::logging::ensure_current(&log_path)?;
        let qos_marking = crate::cross_screen::link_warmth::load_qos_marking(&store);
        let _ = append_log(
            &log_path,
            &format!("cross_screen_qos_marking enabled={qos_marking}"),
        );
        match crate::cross_screen::link_warmth::LinkWarmthSettings::load_or_default(&store) {
            Ok(settings) => crate::cross_screen::link_warmth::set_enabled(settings.keep_link_warm),
            Err(error) => {
                let _ = append_log(
                    &log_path,
                    &format!("link_warmth_settings_load_failed error={error} action=default_on"),
                );
            }
        }
        append_log(
            &log_path,
            &format!(
                "tray_start version={} pid={} elevated={} executable=\"{}\" created_identity={} fingerprint={} uptime_ms={} parent={}",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                is_process_elevated(),
                std::env::current_exe()?.display(),
                stored.created,
                stored.identity.fingerprint(),
                uptime_ms(),
                parent_process_name()
            ),
        )?;
        if stored.created {
            match write_autostart(true) {
                Ok(()) => {
                    let _ = append_log(&log_path, "first_run_defaults autostart=true");
                }
                Err(error) => {
                    let _ = append_log(
                        &log_path,
                        &format!("first_run_defaults autostart_error={error}"),
                    );
                }
            }
        }
        if !stored.created && is_process_elevated() {
            // Migrate only this exact, enabled legacy entry. A disabled task/StartupApproved
            // entry or another executable's Run entry is an explicit choice, not a retry signal.
            let migration = (|| -> io::Result<()> {
                if should_migrate_autostart(
                    query_legacy_autostart()?,
                    crate::autostart::query(&std::env::current_exe()?)?,
                ) {
                    write_autostart(true)?;
                    append_log(
                        &log_path,
                        "autostart_migration backend=interactive_elevated_task success=true",
                    )?;
                }
                Ok(())
            })();
            if let Err(error) = migration {
                let _ = append_log(
                    &log_path,
                    &format!("autostart_migration success=false error={error}"),
                );
            }
        }
        let last_run = crate::autostart::last_run().ok().flatten();
        let _ = append_log(
            &log_path,
            &format!(
                "autostart_health state={} elevated={} last_run={} last_result={}",
                autostart_status().map_or_else(|error| format!("error:{error}"), str::to_owned),
                is_process_elevated(),
                last_run
                    .as_ref()
                    .and_then(|run| run.time.as_deref())
                    .unwrap_or("none"),
                last_run
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |run| format!("{:#010x}", run.result))
            ),
        );
        // SAFETY: the static string is valid for the duration of the registration call.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        if taskbar_created == 0 {
            return Err(io::Error::last_os_error());
        }
        let (discovery, discovery_error) = match DiscoveryRuntime::start(store.clone()) {
            Ok(runtime) => (Some(runtime), None),
            Err(error) => (None, Some(format!("局域网自动发现启动失败：{error}"))),
        };
        let pairing = discovery.as_ref().and_then(|runtime| {
            PairingRuntime::start(store.clone(), runtime.view(), log_path.clone())
                .map_err(|error| {
                    append_log(&log_path, &format!("pairing_listener_start error={error}")).ok();
                    error
                })
                .ok()
        });
        let (runtime, runtime_error) = start_product_runtime(&store);
        let screen_info_sync = discovery.as_ref().and_then(|discovery_runtime| {
            ScreenInfoSyncRuntime::start(store.clone(), discovery_runtime.view())
                .map_err(|error| {
                    let _ = append_log(&log_path, &format!("screen_info_sync_start error={error}"));
                })
                .ok()
        });
        let (cross_screen, cross_screen_error) =
            start_cross_screen_runtime(&store, discovery.as_ref().map(DiscoveryRuntime::view));
        let cursor_locator = match crate::cursor_locator::CursorLocatorRuntime::start() {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                let _ = append_log(&log_path, &format!("cursor_locator_start error={error}"));
                None
            }
        };
        let active_peer = AppSettings::load(&store)
            .ok()
            .map(|settings| settings.active_peer);
        let transfer_window_settings = TransferWindowSettings::load_or_default(&store)
            .unwrap_or_else(|error| {
                let _ = append_log(
                    &log_path,
                    &format!("transfer_window_settings_load error={error} default=quiet"),
                );
                TransferWindowSettings::default()
            });
        let ui_language = UiPreferences::load_or_default(&store).unwrap_or_else(|error| {
            let _ = append_log(
                &log_path,
                &format!("ui_preferences_load error={error} default=en"),
            );
            UiPreferences::default()
        });
        let (ssh, ssh_error) = match crate::ssh::Runtime::start(
            store.clone(),
            discovery.as_ref().map(DiscoveryRuntime::view),
        ) {
            Ok(runtime) => (Some(runtime), String::new()),
            Err(error) => (None, error.to_string()),
        };
        // SAFETY: IDI_INFORMATION is a predefined shared system resource.
        let ssh_attention_icon =
            unsafe { LoadIconW(None, IDI_INFORMATION) }.map_err(windows_error)?;
        Ok(Self {
            screenshot: crate::screenshot::Manager::new(store.root()),
            license,
            cursor_locator,
            ssh,
            ssh_error,
            store,
            log_path,
            active_log_path,
            icon: OwnedIcon::load()?,
            taskbar_created,
            discovery,
            discovery_error,
            screen_info_sync,
            pairing,
            pending_pairing: None,
            pairing_message: None,
            runtime,
            runtime_error,
            cross_screen,
            cross_screen_error,
            active_peer,
            transfer_window: None,
            transfer_window_settings,
            ui_language: ui_language.language,
            ui_theme: ui_language.theme,
            ui_font_scale_percent: ui_language.font_scale_percent,
            transfer_window_error: None,
            settings_window: None,
            settings_window_error: None,
            last_tooltip: String::new(),
            last_icon_health_check: Instant::now(),
            last_log_rotation_check: Instant::now(),
            tray_icon_startup_retry_pending: true,
            tray_icon_deferred_at: None,
            tray_icon_add_attempts: 0,
            last_icon_add_retry_log: None,
            last_cross_screen_log: String::new(),
            last_network_path_log: String::new(),
            last_network_notification: String::new(),
            last_boundary_sync_sequence: 0,
            last_product_boundary_sync_sequence: 0,
            notified_terminal_transfers: HashSet::new(),
            notified_started_transfers: HashSet::new(),
            notified_ssh_requests: HashSet::new(),
            ssh_attention_icon,
            ssh_attention_active: false,
            ssh_attention_phase: false,
        })
    }

    fn shutdown(&mut self) {
        self.screenshot.shutdown();
        self.ssh.take();
        if let Some(mut cursor_locator) = self.cursor_locator.take() {
            cursor_locator.stop();
        }
        let started = Instant::now();
        let _ = append_log(&self.log_path, "tray_shutdown phase=begin");
        // Stop input control first so hooks, injected state, and the dedicated TLS channel are
        // released before the tray window disappears. Every field is taken explicitly, making
        // this idempotent when partial startup or a later Drop path calls it again.
        if let Some(mut cross_screen) = self.cross_screen.take() {
            cross_screen.stop();
            for trace in cross_screen.drain_diagnostic_traces() {
                let _ = append_log(&self.log_path, &format!("cross_screen_trace {trace}"));
            }
        }
        let cross_screen_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let _ = append_log(
            &self.log_path,
            &format!("tray_shutdown phase=cross_screen_done elapsed_ms={cross_screen_ms}"),
        );
        if let Some(mut runtime) = self.screen_info_sync.take() {
            runtime.stop();
        }
        if let Some(mut runtime) = self.runtime.take() {
            runtime.stop();
        }
        self.settings_window.take();
        self.transfer_window.take();
        self.pairing.take();
        self.discovery.take();
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let _ = append_log(
            &self.log_path,
            &format!("tray_workers_stop clean=true elapsed_ms={elapsed_ms}"),
        );
    }

    fn active_network_issue(&self) -> Option<NetworkPathIssue> {
        self.discovery
            .as_ref()?
            .view()
            .network_issue(self.active_peer?)
    }

    #[allow(clippy::too_many_lines)]
    fn snapshot(&self) -> io::Result<String> {
        let english = self.ui_language == UiLanguage::English;
        let identity = self.store.load_identity()?;
        let peers = self.store.list_peers()?;
        let shared_settings = SharedClipboardSettings::load_or_default(&self.store)?;
        let peer_lines = if peers.is_empty() {
            localized(
                self.ui_language,
                "暂无（可从托盘菜单开始配对）",
                "None (start pairing from the tray menu)",
            )
            .to_owned()
        } else {
            peers
                .iter()
                .map(|peer| format!("• {}  {}", peer.label, peer.fingerprint))
                .collect::<Vec<_>>()
                .join("\r\n")
        };
        let autostart = query_autostart()?.label();
        let (connection, shared_text, recent_manifest, active_transfer, runtime_error) =
            if let Some(runtime) = self.runtime.as_ref() {
                let product = runtime.snapshot();
                let connection = if english {
                    format!(
                        "{} · {} · listening on {} · auto receive {}",
                        if product.listener_running {
                            "Online"
                        } else {
                            "Stopping"
                        },
                        product.active_peer_label,
                        product.local_endpoint,
                        if product.auto_receive { "on" } else { "off" }
                    )
                } else {
                    format!(
                        "{} · {} · 监听 {} · 自动接收 {}",
                        if product.listener_running {
                            "在线"
                        } else {
                            "正在停止"
                        },
                        product.active_peer_label,
                        product.local_endpoint,
                        if product.auto_receive {
                            "开启"
                        } else {
                            "关闭"
                        }
                    )
                };
                let recent_manifest = product.last_manifest.as_ref().map_or_else(
                    || localized(self.ui_language, "暂无", "None").to_owned(),
                    |manifest| {
                        if english {
                            return format!(
                                "{} · {} · {} files/{} folders · {}",
                                localized_direction(manifest.direction, self.ui_language),
                                manifest.primary_name,
                                manifest.files,
                                manifest.directories,
                                format_bytes(manifest.total_size)
                            );
                        }
                        format!(
                            "{} · {} · {} 个文件/{} 个目录 · {}",
                            manifest.direction,
                            manifest.primary_name,
                            manifest.files,
                            manifest.directories,
                            format_bytes(manifest.total_size)
                        )
                    },
                );
                let active_transfer = product.transfer.as_ref().map_or_else(
                    || {
                        if product.pending_offer {
                            localized(
                                self.ui_language,
                                "有待确认的远端文件清单",
                                "A remote file list is awaiting approval",
                            )
                            .to_owned()
                        } else {
                            localized(self.ui_language, "暂无", "None").to_owned()
                        }
                    },
                    |transfer| {
                        let percentage_tenths = transfer
                            .transferred
                            .saturating_mul(1000)
                            .checked_div(transfer.total_size)
                            .unwrap_or(1000);
                        format!(
                            "{} · {}.{}% · {} / {} · {}/s · {} {}/{}",
                            localized_transfer_state(transfer.state, self.ui_language),
                            percentage_tenths / 10,
                            percentage_tenths % 10,
                            format_bytes(transfer.transferred),
                            format_bytes(transfer.total_size),
                            format_bytes(transfer.bytes_per_second),
                            localized(self.ui_language, "文件", "files"),
                            transfer.started_files,
                            transfer.total_files
                        )
                    },
                );
                let shared_text = if product.shared_text_enabled {
                    product.last_shared_text.as_ref().map_or_else(
                        || {
                            if english {
                                "Enabled (text, rich text, and images) · waiting for copied content"
                                    .to_owned()
                            } else {
                                format!(
                                    "已开启（{}）· 等待新的复制内容",
                                    shared_clipboard_categories(shared_settings)
                                )
                            }
                        },
                        |shared| {
                            if english {
                                return format!(
                                    "Enabled (text, rich text, and images) · last {} {} ({}, {} UTF-16 units)",
                                    localized_direction(shared.direction, self.ui_language),
                                    shared.formats,
                                    format_bytes(u64::try_from(shared.bytes).unwrap_or(u64::MAX)),
                                    shared.utf16_units
                                );
                            }
                            format!(
                                "已开启（{}）· 最近{} {}（{}，{} 个 UTF-16 单元）",
                                shared_clipboard_categories(shared_settings),
                                shared.direction,
                                shared.formats,
                                format_bytes(u64::try_from(shared.bytes).unwrap_or(u64::MAX)),
                                shared.utf16_units
                            )
                        },
                    )
                } else {
                    localized(self.ui_language, "未开启", "Disabled").to_owned()
                };
                (
                    connection,
                    shared_text,
                    recent_manifest,
                    active_transfer,
                    product.last_error.map_or_else(
                        || localized(self.ui_language, "无", "None").to_owned(),
                        |error| localized_error_detail(&error, self.ui_language),
                    ),
                )
            } else {
                let connection = match AppSettings::load(&self.store) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => localized(
                        self.ui_language,
                        "尚未配置（请打开“连接设置”）",
                        "Not configured (open Connection settings)",
                    )
                    .to_owned(),
                    Ok(_) => localized(
                        self.ui_language,
                        "配置已保存，但后台服务未启动",
                        "Configuration is saved, but the background service is not running",
                    )
                    .to_owned(),
                    Err(error) => localized_operation_error(
                        self.ui_language,
                        "配置不可用",
                        "Configuration unavailable",
                        &error.to_string(),
                    ),
                };
                (
                    connection,
                    if shared_settings.any_enabled() {
                        if english {
                            "Enabled (text, rich text, and images), but the background service is not running".to_owned()
                        } else {
                            format!(
                                "已开启（{}），后台服务未启动",
                                shared_clipboard_categories(shared_settings)
                            )
                        }
                    } else {
                        localized(self.ui_language, "未开启", "Disabled").to_owned()
                    },
                    localized(self.ui_language, "暂无", "None").to_owned(),
                    localized(self.ui_language, "暂无", "None").to_owned(),
                    self.runtime_error.as_deref().map_or_else(
                        || localized(self.ui_language, "无", "None").to_owned(),
                        |error| localized_error_detail(error, self.ui_language),
                    ),
                )
            };
        let network_issue = self.active_network_issue();
        let discovery = self.discovery.as_ref().map_or_else(
            || {
                self.discovery_error.as_deref().map_or_else(
                    || localized(self.ui_language, "不可用", "Unavailable").to_owned(),
                    |error| localized_error_detail(error, self.ui_language),
                )
            },
            |runtime| {
                network_issue.as_ref().map_or_else(
                    || {
                        if english {
                            format!(
                                "Running, {} device(s) discovered",
                                runtime.view().peers().len()
                            )
                        } else {
                            format!("运行中，当前发现 {} 台设备", runtime.view().peers().len())
                        }
                    },
                    |issue| {
                        format!(
                            "{}: {}",
                            localized(
                                self.ui_language,
                                "检测到网络地址冲突",
                                "Network address conflict detected"
                            ),
                            localized_network_issue(issue, self.ui_language)
                        )
                    },
                )
            },
        );
        let cross_screen = self.cross_screen_status();
        if english {
            Ok(format!(
                "Pierdesk is running in the background\r\n\r\nThis device\r\n{}\r\n\r\nPaired devices ({})\r\n{}\r\n\r\nLAN discovery: {}\r\nFile clipboard: {}\r\nShared clipboard: {}\r\nCross-screen control: {}\r\nLatest remote list: {}\r\nActive transfer: {}\r\nLatest error: {}\r\nStart with Windows: {}",
                identity.fingerprint(),
                peers.len(),
                peer_lines,
                discovery,
                connection,
                shared_text,
                cross_screen,
                recent_manifest,
                active_transfer,
                runtime_error,
                autostart
            ))
        } else {
            Ok(format!(
                "Pierdesk 正在后台运行\r\n\r\n本机设备\r\n{}\r\n\r\n已配对设备（{}）\r\n{}\r\n\r\n局域网发现：{}\r\n文件剪贴板：{}\r\n共享剪贴板：{}\r\n跨屏控制：{}\r\n最近远端清单：{}\r\n活动传输：{}\r\n最近错误：{}\r\n开机自启动：{}",
                identity.fingerprint(),
                peers.len(),
                peer_lines,
                discovery,
                connection,
                shared_text,
                cross_screen,
                recent_manifest,
                active_transfer,
                runtime_error,
                autostart
            ))
        }
    }

    #[allow(clippy::too_many_lines)]
    fn settings_model_json(&mut self) -> io::Result<String> {
        let identity = self.store.load_identity()?;
        let trusted = self.store.list_peers()?;
        let discovered = self
            .discovery
            .as_ref()
            .map(|runtime| runtime.view().peers())
            .unwrap_or_default();
        let app_settings = AppSettings::load(&self.store).ok();
        let product = self.runtime.as_ref().map(ProductRuntime::snapshot);
        let shared_settings = SharedClipboardSettings::load_or_default(&self.store)?;
        let cross_settings = CrossScreenSettings::load_or_default(&self.store)?;
        let boundary_constraints = current_boundary_constraints(&cross_settings)?;
        let (boundary_map, boundary_map_adjusted, boundary_map_error) =
            match BoundaryMapV2::load_or_default(&self.store) {
                Ok(stored) => match stored.clipped_to_screen(boundary_constraints) {
                    Ok(effective) => {
                        let adjusted = effective.segments != stored.segments;
                        (effective, adjusted, String::new())
                    }
                    Err(error) => (stored, false, error.to_string()),
                },
                Err(error) => (BoundaryMapV2::default(), false, error.to_string()),
            };
        let (boundary_workspace_v3, boundary_workspace_v3_error) =
            match BoundaryWorkspaceV3::load(&self.store) {
                Ok(workspace) => (Some(workspace), String::new()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (None, String::new()),
                Err(error) => (None, error.to_string()),
            };
        let boundary_workspace_v3_applied =
            boundary_workspace_v3.as_ref().is_some_and(|workspace| {
                !workspace.draft.boundaries.is_empty() && !workspace.has_unapplied_changes()
            });
        let local_physical_millimeters = current_primary_screen_millimeters().ok();
        let cross_snapshot = self
            .cross_screen
            .as_ref()
            .map(CrossScreenProductRuntime::snapshot);
        let boundary_remote_workspace = product
            .as_ref()
            .and_then(|snapshot| snapshot.remote_boundary_workspace.as_deref())
            .or_else(|| {
                cross_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.remote_boundary_workspace.as_ref())
            });
        let active = app_settings.as_ref().map(|settings| settings.active_peer);
        if let Some(workspace) = boundary_workspace_v3.as_ref() {
            for screen in workspace
                .draft
                .screens
                .iter()
                .filter(|screen| screen.device_fingerprint != workspace.draft.local_fingerprint)
            {
                let Ok(fingerprint) =
                    crate::security::CertificateFingerprint::from_str(&screen.device_fingerprint)
                else {
                    continue;
                };
                let Ok(info) = SynchronizedScreenInfo::new(
                    screen.pixel_width,
                    screen.pixel_height,
                    screen.physical_width_millimeters,
                    screen.physical_height_millimeters,
                ) else {
                    continue;
                };
                let _ = SynchronizedScreenCatalog::remember_latest(&self.store, fingerprint, info);
            }
        }
        if let (Some(active), Some(snapshot)) = (active, cross_snapshot.as_ref())
            && let (Some(screen), Some(millimeters)) =
                (snapshot.peer_screen, snapshot.peer_screen_millimeters)
            && let Ok(info) = SynchronizedScreenInfo::new(
                screen.width,
                screen.height,
                millimeters.width,
                millimeters.height,
            )
        {
            let _ = SynchronizedScreenCatalog::remember_latest(&self.store, active, info);
        }
        let screen_catalog =
            SynchronizedScreenCatalog::load_or_default(&self.store).unwrap_or_default();
        let trusted_fingerprints = trusted
            .iter()
            .map(|peer| peer.fingerprint)
            .collect::<HashSet<_>>();
        let pairable = discovered
            .iter()
            .filter(|peer| !trusted_fingerprints.contains(&peer.fingerprint))
            .map(|peer| {
                serde_json::json!({
                    "fingerprint": peer.fingerprint.to_string(),
                    "label": peer.label,
                    "address": peer.service_endpoint.ip().to_string(),
                    "pairable": true,
                })
            })
            .collect::<Vec<_>>();
        let peers = trusted
            .iter()
            .map(|peer| {
                let seen = discovered
                    .iter()
                    .find(|candidate| candidate.fingerprint == peer.fingerprint);
                let endpoint = seen
                    .map(|candidate| candidate.service_endpoint.to_string())
                    .or_else(|| {
                        app_settings
                            .as_ref()
                            .filter(|settings| settings.active_peer == peer.fingerprint)
                            .map(|settings| settings.peer_endpoint.to_string())
                    })
                    .unwrap_or_default();
                let fingerprint = peer.fingerprint.to_string();
                let stored_screen = boundary_workspace_v3.as_ref().and_then(|workspace| {
                    workspace
                        .draft
                        .screens
                        .iter()
                        .find(|screen| screen.device_fingerprint == fingerprint)
                });
                let cached_screen = screen_catalog.get(peer.fingerprint);
                let screen = if active == Some(peer.fingerprint) {
                    cross_snapshot.as_ref().and_then(|snapshot| {
                        snapshot.peer_screen.map(|screen| {
                            let millimeters = snapshot.peer_screen_millimeters.map_or_else(
                                || {
                                    stored_screen.map(|stored| {
                                        (
                                            stored.physical_width_millimeters,
                                            stored.physical_height_millimeters,
                                        )
                                    })
                                },
                                |size| Some((size.width, size.height)),
                            );
                            let mut value = serde_json::json!({
                                "pixelWidth": screen.width,
                                "pixelHeight": screen.height,
                            });
                            if let Some((width, height)) = millimeters
                                && let Some(object) = value.as_object_mut()
                            {
                                object.insert(
                                    "physicalWidthMillimeters".to_owned(),
                                    serde_json::json!(width),
                                );
                                object.insert(
                                    "physicalHeightMillimeters".to_owned(),
                                    serde_json::json!(height),
                                );
                            }
                            value
                        })
                    })
                } else {
                    None
                }
                .or_else(|| {
                    stored_screen.map(|screen| {
                        serde_json::json!({
                            "pixelWidth": screen.pixel_width,
                            "pixelHeight": screen.pixel_height,
                            "physicalWidthMillimeters": screen.physical_width_millimeters,
                            "physicalHeightMillimeters": screen.physical_height_millimeters,
                        })
                    })
                })
                .or_else(|| {
                    cached_screen.map(|screen| {
                        serde_json::json!({
                            "pixelWidth": screen.pixel_width,
                            "pixelHeight": screen.pixel_height,
                            "physicalWidthMillimeters": screen.physical_width_millimeters,
                            "physicalHeightMillimeters": screen.physical_height_millimeters,
                        })
                    })
                });
                serde_json::json!({
                    "fingerprint": fingerprint,
                    "label": peer.label,
                    "endpoint": endpoint,
                    "online": seen.is_some(),
                    "current": active == Some(peer.fingerprint),
                    "screen": screen,
                })
            })
            .collect::<Vec<_>>();
        let (history_limit, row_height) = self.transfer_window.as_ref().map_or_else(
            || persisted_view_preferences(&self.store.root().join("transfer-history.v1")),
            TransferWindow::view_preferences,
        );
        let transfer_tasks = product.as_ref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .transfer_tasks
                .iter()
                .filter(|task| !terminal_transfer(task.transfer.state))
                .map(|task| {
                    let tenths = transfer_percentage_tenths(
                        task.transfer.transferred,
                        task.transfer.total_size,
                    );
                    let direction_detail = match task.id.direction() {
                        ProductTransferDirection::Sending => {
                            format!("本机 → {}", snapshot.active_peer_label)
                        }
                        ProductTransferDirection::Receiving => {
                            format!("{} → 本机", snapshot.active_peer_label)
                        }
                    };
                    serde_json::json!({
                        "key": settings_task_key(task.id),
                        "name": task.manifest.primary_name,
                        "files": task.manifest.files,
                        "directories": task.manifest.directories,
                        "direction": task.id.direction().label(),
                        "directionDetail": direction_detail,
                        "state": task.transfer.state.label(),
                        "stateCode": settings_transfer_state_code(task.transfer.state),
                        "controllable": task.locally_controllable(),
                        "receiving": task.id.direction() == ProductTransferDirection::Receiving,
                        "progress": f64::from(u32::try_from(tenths).unwrap_or(u32::MAX)) / 10.0,
                        "transferred": format_bytes(task.transfer.transferred),
                        "total": format_bytes(task.transfer.total_size),
                        "speed": crate::transfer_window::transfer_speed_label(task.transfer.state, task.transfer.bytes_per_second),
                    })
                })
                .collect()
        });
        let recent_manifest = product
            .as_ref()
            .and_then(|snapshot| snapshot.last_manifest.as_ref())
            .map(|manifest| {
                serde_json::json!({
                    "name": manifest.primary_name,
                    "files": manifest.files,
                    "directories": manifest.directories,
                    "size": format_bytes(manifest.total_size),
                })
            });
        let last_shared = product
            .as_ref()
            .and_then(|snapshot| snapshot.last_shared_text.as_ref());
        let placement = cross_snapshot
            .as_ref()
            .map_or(cross_settings.listener_peer_placement, |snapshot| {
                snapshot.peer_placement
            });
        let latency = cross_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.round_trip_micros)
            .map_or_else(
                || "等待中".to_owned(),
                |micros| {
                    format!(
                        "{:.1} ms",
                        Duration::from_micros(micros).as_secs_f64() * 1_000.0
                    )
                },
            );
        let cross_state = cross_snapshot.as_ref().map_or_else(
            || {
                if cross_settings.enabled {
                    "等待启动".to_owned()
                } else {
                    "未开启".to_owned()
                }
            },
            |snapshot| cross_screen_state_text(snapshot.state).to_owned(),
        );
        let peer_label = product
            .as_ref()
            .map(|snapshot| snapshot.active_peer_label.clone())
            .or_else(|| {
                active.and_then(|fingerprint| {
                    trusted
                        .iter()
                        .find(|peer| peer.fingerprint == fingerprint)
                        .map(|peer| peer.label.clone())
                })
            })
            .unwrap_or_else(|| "尚未选择设备".to_owned());
        let network_issue = self
            .active_network_issue()
            .map(|issue| localized_network_issue(&issue, self.ui_language));
        let pairing = self.pending_pairing.as_ref().map(|request| {
            serde_json::json!({
                "fingerprint": request.pending.peer_fingerprint().to_string(),
                "label": request.discovered.label,
                "code": request.pending.code().to_string(),
            })
        });
        let last_error = product
            .as_ref()
            .and_then(|snapshot| snapshot.last_error.clone())
            .or_else(|| self.runtime_error.clone())
            .or_else(|| {
                cross_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.last_error.clone())
            })
            .or_else(|| self.cross_screen_error.clone())
            .unwrap_or_default();
        let last_notice = cross_snapshot.as_ref().map_or("", |snapshot| {
            match snapshot.last_control_return_reason {
                Some("destination_busy") => localized(
                    self.ui_language,
                    "对端正在使用，控制已返回本机",
                    "The peer is in use; control returned to this computer",
                ),
                _ => "",
            }
        });
        let online_peers = discovered
            .iter()
            .map(|peer| peer.fingerprint)
            .collect::<HashSet<_>>();
        let pair_switches = pair_switch_rows(
            &self.store,
            boundary_workspace_v3
                .as_ref()
                .and_then(|workspace| workspace.applied.as_ref()),
            &online_peers,
        );
        let mut model = serde_json::json!({
            "license": self.license.snapshot,
            "ssh": self.ssh.as_ref().map(crate::ssh::Runtime::snapshot),
            "sshError": self.ssh_error,
            "type": "model",
            "version": env!("CARGO_PKG_VERSION"),
            "language": self.ui_language.key(),
            "theme": self.ui_theme.key(),
            "fontScale": self.ui_font_scale_percent,
            "screenshotShortcut": self.screenshot.shortcut,
            "screenshotError": self.screenshot.error,
            "connected": cross_snapshot.as_ref().is_some_and(|snapshot| matches!(
                snapshot.state,
                CrossScreenProductState::Connected | CrossScreenProductState::ControllingPeer | CrossScreenProductState::ControlledByPeer
            )) || (!cross_settings.enabled
                && product.as_ref().is_some_and(|snapshot| snapshot.listener_running)
                && active.is_some_and(|fingerprint| discovered.iter().any(|peer| peer.fingerprint == fingerprint))),
            "peerLabel": peer_label,
            "localLabel": std::env::var("COMPUTERNAME").unwrap_or_else(|_| "本机".to_owned()),
            "localFingerprint": identity.fingerprint().to_string(),
            "autostart": query_autostart()? == AutostartState::Enabled,
            "autostartState": query_autostart()?.label(),
            "discovery": {
                "running": self.discovery.is_some(),
                "issue": network_issue.as_deref().unwrap_or_default(),
                "peersFound": discovered.len(),
            },
            "discovered": pairable,
            "peers": peers,
            "pairing": pairing,
            "pairingMessage": self.pairing_message.as_deref().unwrap_or_default(),
            "connection": {
                "configured": app_settings.is_some(),
                "activePeer": app_settings.as_ref().map_or_else(String::new, |settings| settings.active_peer.to_string()),
                "localEndpoint": app_settings.as_ref().map_or_else(String::new, |settings| settings.local_endpoint.to_string()),
                "peerEndpoint": app_settings.as_ref().map_or_else(String::new, |settings| settings.peer_endpoint.to_string()),
                "autoReceive": app_settings.as_ref().is_some_and(|settings| settings.auto_receive),
                "valid": network_issue.is_none(),
            },
            "cross": {
                "enabled": cross_settings.enabled,
                "boundaryMapV2Enabled": cross_settings.boundary_map_v2_enabled,
                "state": cross_state,
                "placement": placement_key(placement),
                "cornerGuardEnabled": cross_settings.corner_guard_enabled,
                "cornerGuardPixels": cross_settings.corner_guard_pixels,
                "requireSecondPush": cross_settings.require_second_push,
                "pauseOnFullscreen": cross_settings.pause_on_fullscreen,
                "keepLinkWarm": crate::cross_screen::link_warmth::enabled(),
                "latency": latency,
                "admin": is_process_elevated(),
                "connections": cross_snapshot.as_ref().map_or(serde_json::Value::Null, |snapshot| snapshot.connections.clone()),
            },
            "boundaryMap": boundary_map,
            "boundaryMapAdjusted": boundary_map_adjusted,
            "boundaryMapError": boundary_map_error,
            "boundaryWorkspaceV3": boundary_workspace_v3,
            "boundaryWorkspaceV3Applied": boundary_workspace_v3_applied,
            "boundaryWorkspaceV3Error": boundary_workspace_v3_error,
            "boundaryRemoteWorkspace": boundary_remote_workspace,
            "boundaryWorkspaceV3RuntimeReady": boundary_workspace_v3_runtime_ready(),
            "boundaryMultiPeerRuntimeReady": boundary_multi_peer_runtime_ready(),
            "boundaryScreen": {
                "width": boundary_constraints.screen.width,
                "height": boundary_constraints.screen.height,
                "physicalWidthMillimeters": local_physical_millimeters.map(|size| size.0),
                "physicalHeightMillimeters": local_physical_millimeters.map(|size| size.1),
                "normalizedEdgeLength": crate::cross_screen::NORMALIZED_EDGE_LENGTH,
                "minimumSegmentPixels": boundary_constraints.minimum_segment_pixels,
            },
            "shared": {
                "enabled": shared_settings.enabled,
                "generation": product.as_ref().map_or(0, |snapshot| snapshot.shared_text_generation),
                "lastDirection": last_shared.map_or("", |value| value.direction),
                "lastFormats": last_shared.map_or("", |value| value.formats.as_str()),
                "lastBytes": last_shared.map_or_else(String::new, |value| format_bytes(u64::try_from(value.bytes).unwrap_or(u64::MAX))),
                "lastUnits": last_shared.map_or(0, |value| value.utf16_units),
            },
            "transfer": {
                "generation": product.as_ref().map_or(0, |snapshot| snapshot.transfer_generation),
                "pendingOffer": product.as_ref().is_some_and(|snapshot| snapshot.pending_offer),
                "autoPopup": self.transfer_window_settings.auto_popup,
                "historyLimit": history_limit,
                "rowHeight": row_height,
                "tasks": transfer_tasks,
            },
            "recentManifest": recent_manifest,
            "lastError": last_error,
            "lastNotice": last_notice,
            "logPath": self.active_log_path.to_string_lossy(),
        });
        // Added outside the macro, which is at its recursion limit.
        let (last_result, last_run_time) = last_run_summary();
        model["autostartLastResult"] = last_result.into();
        model["autostartLastRun"] = last_run_time.into();
        model["pairSwitches"] = pair_switches;
        model["licenseEnabled"] = crate::license_runtime::ENABLED.into();
        serde_json::to_string(&model).map_err(io::Error::other)
    }

    fn cross_screen_status(&self) -> String {
        let english = self.ui_language == UiLanguage::English;
        if let Some(issue) = self.active_network_issue() {
            return format!(
                "{}: {}",
                if english { "Paused" } else { "已暂停" },
                localized_network_issue(&issue, self.ui_language)
            );
        }
        if let Some(runtime) = self.cross_screen.as_ref() {
            let snapshot = runtime.snapshot();
            if let Some(error) = snapshot.last_error.as_deref() {
                if english {
                    return format!(
                        "{} · peer {} · {} · Last connection error: {}",
                        localized_cross_state(snapshot.state, self.ui_language),
                        localized_placement(snapshot.peer_placement, self.ui_language),
                        snapshot.peer_label,
                        localized_error_detail(error, self.ui_language)
                    );
                }
                return format!(
                    "{} · 对端在{} · {} · 最近连接错误：{}",
                    cross_screen_state_text(snapshot.state),
                    placement_text(snapshot.peer_placement),
                    snapshot.peer_label,
                    error
                );
            }
            let latency = snapshot.round_trip_micros.map_or_else(
                || localized(self.ui_language, "延迟等待中", "Waiting for latency").to_owned(),
                |micros| {
                    let tenths = micros / 100;
                    format!(
                        "{} {}.{} ms",
                        localized(self.ui_language, "延迟", "Latency"),
                        tenths / 10,
                        tenths % 10
                    )
                },
            );
            if english {
                return format!(
                    "{} · peer {} · {} · {}",
                    localized_cross_state(snapshot.state, self.ui_language),
                    localized_placement(snapshot.peer_placement, self.ui_language),
                    snapshot.peer_label,
                    latency
                );
            }
            return format!(
                "{} · 对端在{} · {} · {}",
                cross_screen_state_text(snapshot.state),
                placement_text(snapshot.peer_placement),
                snapshot.peer_label,
                latency
            );
        }
        match CrossScreenSettings::load_or_default(&self.store) {
            Ok(settings) if !settings.enabled => {
                localized(self.ui_language, "未开启", "Disabled").to_owned()
            }
            Ok(_) => self.cross_screen_error.as_ref().map_or_else(
                || {
                    localized(
                        self.ui_language,
                        "已开启，正在等待启动",
                        "Enabled, waiting to start",
                    )
                    .to_owned()
                },
                |error| {
                    localized_operation_error(self.ui_language, "启动失败", "Start failed", error)
                },
            ),
            Err(error) => localized_operation_error(
                self.ui_language,
                "设置不可用",
                "Settings unavailable",
                &error.to_string(),
            ),
        }
    }

    fn add_icon(&self, window: HWND) -> io::Result<()> {
        #[cfg(debug_assertions)]
        if let Some(remaining) = debug_defer_remaining() {
            return Err(io::Error::other(format!(
                "debug-deferred tray icon ({} ms left)",
                remaining.as_millis()
            )));
        }
        let data = self.notify_data(window, &self.tooltip_text());
        // SAFETY: data is fully initialized and references a live hidden window and icon.
        if unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool() {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn delete_icon(&self, window: HWND) {
        let data = self.notify_data(window, "");
        // SAFETY: deleting a missing icon is harmless; the data identifies our own tray icon.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) };
    }

    fn notify_data(&self, window: HWND, tooltip: &str) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(u32::MAX),
            hWnd: window,
            uID: TRAY_ICON_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY_CALLBACK_MESSAGE,
            hIcon: self.icon.handle,
            ..Default::default()
        };
        copy_wide_to_fixed(tooltip, &mut data.szTip);
        data
    }

    #[allow(clippy::too_many_lines)]
    fn tooltip_text(&self) -> String {
        let english = self.ui_language == UiLanguage::English;
        if let Some(issue) = self.active_network_issue() {
            return format!(
                "Pierdesk · {}\r\n{}",
                if english {
                    "Network address conflict"
                } else {
                    "网络地址冲突"
                },
                truncate_text(&localized_network_issue(&issue, self.ui_language), 88)
            );
        }
        let cross_screen = self
            .cross_screen
            .as_ref()
            .map(CrossScreenProductRuntime::snapshot);
        let Some(runtime) = self.runtime.as_ref() else {
            if let Some(cross_screen) = cross_screen.as_ref() {
                return localized_cross_screen_tooltip(cross_screen, self.ui_language);
            }
            return self.runtime_error.as_ref().map_or_else(
                || {
                    localized(
                        self.ui_language,
                        "Pierdesk · 尚未配置",
                        "Pierdesk · Not configured",
                    )
                    .to_owned()
                },
                |error| {
                    format!(
                        "Pierdesk · {}\r\n{}",
                        localized(
                            self.ui_language,
                            "后台服务未启动",
                            "Background service is not running"
                        ),
                        truncate_text(&localized_error_detail(error, self.ui_language), 72)
                    )
                },
            );
        };
        let snapshot = runtime.snapshot();
        let has_active_transfer = snapshot.transfer.as_ref().is_some_and(|transfer| {
            matches!(
                transfer.state,
                ProductTransferState::Running | ProductTransferState::Paused
            )
        });
        if !has_active_transfer && let Some(error) = snapshot.last_error.as_deref() {
            return format!(
                "Pierdesk · {}\r\n{}",
                localized(
                    self.ui_language,
                    "最近一次剪贴板未发送",
                    "Last clipboard was not sent"
                ),
                truncate_text(&localized_error_detail(error, self.ui_language), 88)
            );
        }
        if let (Some(manifest), Some(transfer)) =
            (snapshot.last_manifest.as_ref(), snapshot.transfer.as_ref())
        {
            let tenths = transfer_percentage_tenths(transfer.transferred, transfer.total_size);
            let cross_line = cross_screen.as_ref().map_or_else(String::new, |cross| {
                format!(
                    "\r\n{}{} · {}{}",
                    localized(self.ui_language, "跨屏", "Cross-screen: "),
                    localized_cross_state(cross.state, self.ui_language),
                    localized(self.ui_language, "对端在", "peer "),
                    localized_placement(cross.peer_placement, self.ui_language)
                )
            });
            return format!(
                "Pierdesk · {} {}.{}%\r\n{}/s · {} / {}\r\n{} · {}{}",
                localized_direction(manifest.direction, self.ui_language),
                tenths / 10,
                tenths % 10,
                format_bytes(transfer.bytes_per_second),
                format_bytes(transfer.transferred),
                format_bytes(transfer.total_size),
                truncate_text(&snapshot.active_peer_label, 24),
                localized_transfer_state(transfer.state, self.ui_language),
                cross_line,
            );
        }
        if let Some(cross_screen) = cross_screen.as_ref() {
            return localized_cross_screen_tooltip(cross_screen, self.ui_language);
        }
        if english {
            format!(
                "Pierdesk · {} online\r\nAuto receive {} · No transfers",
                truncate_text(&snapshot.active_peer_label, 36),
                if snapshot.auto_receive { "on" } else { "off" }
            )
        } else {
            format!(
                "Pierdesk · {}在线\r\n自动接收{} · 暂无传输",
                truncate_text(&snapshot.active_peer_label, 36),
                if snapshot.auto_receive {
                    "开启"
                } else {
                    "关闭"
                }
            )
        }
    }

    fn tray_icon_added(&self) -> bool {
        self.tray_icon_deferred_at.is_none()
    }

    fn retry_deferred_icon(&mut self, window: HWND, tooltip: String) {
        self.last_icon_health_check = Instant::now();
        self.tray_icon_add_attempts += 1;
        match self.add_icon(window) {
            Ok(()) => {
                self.tray_icon_startup_retry_pending = false;
                self.last_tooltip = tooltip;
                let waited_ms = self
                    .tray_icon_deferred_at
                    .take()
                    .map_or(0, |at| at.elapsed().as_millis());
                let _ = append_log(
                    &self.log_path,
                    &format!(
                        "tray_icon_recovered reason=startup_retry attempts={} waited_ms={waited_ms}",
                        self.tray_icon_add_attempts
                    ),
                );
            }
            Err(error) => {
                if self
                    .last_icon_add_retry_log
                    .is_none_or(|at| at.elapsed() >= TRAY_ICON_ADD_RETRY_LOG_INTERVAL)
                {
                    self.last_icon_add_retry_log = Some(Instant::now());
                    let _ = append_log(
                        &self.log_path,
                        &format!(
                            "tray_icon_add_retry error={error} attempts={}",
                            self.tray_icon_add_attempts
                        ),
                    );
                }
            }
        }
    }

    fn update_icon_tip(&mut self, window: HWND) -> io::Result<()> {
        let tooltip = self.tooltip_text();
        let health_due = self.last_icon_health_check.elapsed() >= TRAY_ICON_HEALTH_INTERVAL;
        if tooltip == self.last_tooltip && !health_due {
            return Ok(());
        }
        if !self.tray_icon_added() {
            // The notification area was not ready at startup; only the throttled retry may touch it.
            if health_due {
                self.retry_deferred_icon(window, tooltip);
            }
            return Ok(());
        }
        let data = self.notify_data(window, &tooltip);
        if self.tray_icon_startup_retry_pending && health_due {
            self.last_icon_health_check = Instant::now();
            self.delete_icon(window);
            self.add_icon(window)?;
            self.tray_icon_startup_retry_pending = false;
            self.last_tooltip = tooltip;
            let _ = append_log(&self.log_path, "tray_icon_recovered reason=startup_retry");
            return Ok(());
        }
        // SAFETY: data identifies the live tray icon and contains only owned fixed-size fields.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
            let modify_error = io::Error::last_os_error();
            self.delete_icon(window);
            self.add_icon(window).map_err(|add_error| {
                io::Error::other(format!(
                    "托盘图标自修复失败（修改：{modify_error}；重新添加：{add_error}）"
                ))
            })?;
            let _ = append_log(&self.log_path, "tray_icon_recovered action=delete_add");
        }
        self.last_tooltip = tooltip;
        self.last_icon_health_check = Instant::now();
        Ok(())
    }

    fn ensure_transfer_window(&mut self, command_target: HWND) -> io::Result<&mut TransferWindow> {
        let language = self.ui_language;
        let theme = self.ui_theme;
        if self.transfer_window.is_none() {
            let window = TransferWindow::create(
                command_target,
                self.icon.handle,
                TransferWindowCommands {
                    pause: COMMAND_PAUSE,
                    resume: COMMAND_RESUME,
                    cancel: COMMAND_CANCEL,
                    dispatch: COMMAND_TRANSFER_DISPATCH,
                },
                self.store.root().join("transfer-history.v1"),
                self.transfer_window_settings.auto_popup,
            )?;
            self.transfer_window = Some(window);
        }
        let window = self
            .transfer_window
            .as_mut()
            .ok_or_else(|| io::Error::other("传输窗口初始化失败"))?;
        window.set_language(language);
        window.set_theme(theme);
        window.set_auto_popup(self.transfer_window_settings.auto_popup);
        Ok(window)
    }

    fn ensure_settings_window(&mut self, command_target: HWND) -> io::Result<&mut SettingsWindow> {
        if self.settings_window.is_none() {
            let window = SettingsWindow::create(
                command_target,
                self.icon.handle,
                SettingsWindowCommands {
                    dispatch: COMMAND_SETTINGS_DISPATCH,
                },
                self.store.root(),
            )?;
            self.settings_window = Some(window);
        }
        self.settings_window
            .as_mut()
            .ok_or_else(|| io::Error::other("设置窗口初始化失败"))
    }

    fn show_settings(&mut self, command_target: HWND, page: &str) -> io::Result<()> {
        let page = if page == "status" && !self.license.snapshot.active {
            "license"
        } else {
            page
        };
        self.clear_ssh_attention(command_target);
        let model = self.settings_model_json()?;
        let window = self.ensure_settings_window(command_target)?;
        window.update_json(model)?;
        window.show(page)
    }

    fn refresh_settings_window(&mut self) {
        if self.settings_window.is_none() {
            return;
        }
        let result = self.settings_model_json().and_then(|model| {
            self.settings_window
                .as_ref()
                .ok_or_else(|| io::Error::other("设置窗口不可用"))?
                .update_json(model)
        });
        match result {
            Ok(()) => self.settings_window_error = None,
            Err(error) => {
                let message = error.to_string();
                if self.settings_window_error.as_deref() != Some(&message) {
                    let _ = append_log(
                        &self.log_path,
                        &format!("settings_window_update error={error}"),
                    );
                    self.settings_window_error = Some(message);
                }
            }
        }
    }

    fn validate_boundary_workspace_layout(
        &self,
        value: serde_json::Value,
    ) -> io::Result<BoundaryLayoutV3> {
        let mut proposed: BoundaryLayoutV3 = serde_json::from_value(value).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("逻辑边界草稿格式无效：{error}"),
            )
        })?;
        proposed.canonicalize_fingerprints()?;
        proposed.validate_schema()?;
        let local = self.store.load_identity()?.fingerprint();
        if proposed.local_fingerprint != local.to_string() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "逻辑边界草稿不属于本机",
            ));
        }
        let trusted = self
            .store
            .list_peers()?
            .into_iter()
            .map(|peer| peer.fingerprint)
            .collect::<HashSet<_>>();
        for screen in proposed
            .screens
            .iter()
            .filter(|screen| screen.device_fingerprint != proposed.local_fingerprint)
        {
            let fingerprint =
                crate::security::CertificateFingerprint::from_str(&screen.device_fingerprint)?;
            if !trusted.contains(&fingerprint) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "逻辑边界草稿引用了未配对设备",
                ));
            }
        }
        Ok(proposed)
    }

    fn queue_boundary_workspace_for_peers(
        &self,
        layout: &BoundaryLayoutV3,
        phase: BoundarySyncPhase,
        surface: BoundarySyncSurface,
    ) -> io::Result<()> {
        let initiator = self.store.load_identity()?.fingerprint();
        self.queue_boundary_workspace_for_peers_with_initiator(layout, phase, surface, initiator)
    }

    fn queue_boundary_workspace_for_peers_with_initiator(
        &self,
        layout: &BoundaryLayoutV3,
        phase: BoundarySyncPhase,
        surface: BoundarySyncSurface,
        initiator: crate::security::CertificateFingerprint,
    ) -> io::Result<()> {
        if let Some(runtime) = self.screen_info_sync.as_ref() {
            runtime.queue_boundary_workspace(layout, phase, surface, initiator);
            return Ok(());
        }
        let Some(runtime) = self.cross_screen.as_ref() else {
            return Ok(());
        };
        let Ok(settings) = AppSettings::load(&self.store) else {
            return Ok(());
        };
        if let Some(payload) = encode_boundary_sync_with_initiator(
            layout,
            settings.active_peer,
            phase,
            surface,
            initiator,
        )? {
            runtime.queue_boundary_workspace(payload);
        }
        Ok(())
    }

    fn queue_boundary_workspace_for_peer_with_initiator(
        &self,
        layout: &BoundaryLayoutV3,
        peer: crate::security::CertificateFingerprint,
        phase: BoundarySyncPhase,
        surface: BoundarySyncSurface,
        initiator: crate::security::CertificateFingerprint,
    ) -> io::Result<()> {
        if let Some(runtime) = self.screen_info_sync.as_ref() {
            runtime.queue_boundary_workspace_for_peer(peer, layout, phase, surface, initiator);
            return Ok(());
        }
        let Some(runtime) = self.cross_screen.as_ref() else {
            return Ok(());
        };
        let Ok(settings) = AppSettings::load(&self.store) else {
            return Ok(());
        };
        if settings.active_peer != peer {
            return Ok(());
        }
        if let Some(payload) =
            encode_boundary_sync_with_initiator(layout, peer, phase, surface, initiator)?
        {
            runtime.queue_boundary_workspace(payload);
        }
        Ok(())
    }

    fn remote_boundary_workspace(
        &self,
    ) -> Option<(BoundaryWorkspaceTransport, RemoteBoundaryWorkspace)> {
        if let Some(remote) = self
            .runtime
            .as_ref()
            .map(ProductRuntime::snapshot)
            .and_then(|snapshot| snapshot.remote_boundary_workspace)
        {
            return Some((BoundaryWorkspaceTransport::DeviceService, (*remote).clone()));
        }
        self.cross_screen
            .as_ref()
            .map(CrossScreenProductRuntime::snapshot)
            .and_then(|snapshot| snapshot.remote_boundary_workspace)
            .map(|remote| (BoundaryWorkspaceTransport::ActiveControlChannel, remote))
    }

    fn clear_remote_boundary_workspace(
        &self,
        transport: BoundaryWorkspaceTransport,
        sequence: u64,
    ) {
        match transport {
            BoundaryWorkspaceTransport::DeviceService => {
                if let Some(runtime) = self.runtime.as_ref() {
                    runtime.clear_remote_boundary_workspace(sequence);
                }
            }
            BoundaryWorkspaceTransport::ActiveControlChannel => {
                if let Some(runtime) = self.cross_screen.as_ref() {
                    runtime.clear_remote_boundary_workspace(sequence);
                }
            }
        }
    }

    fn remote_boundary_peer(
        remote: &RemoteBoundaryWorkspace,
    ) -> io::Result<crate::security::CertificateFingerprint> {
        remote
            .layout
            .screens
            .iter()
            .find(|screen| screen.device_fingerprint != remote.layout.local_fingerprint)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "远端边界草稿没有包含已认证的对端屏幕",
                )
            })
            .and_then(|screen| {
                crate::security::CertificateFingerprint::from_str(&screen.device_fingerprint)
            })
    }

    fn apply_confirmed_boundary_pair_if_ready(
        &mut self,
        current: BoundaryWorkspaceV3,
        peer: crate::security::CertificateFingerprint,
        initiator: crate::security::CertificateFingerprint,
    ) -> io::Result<BoundaryWorkspaceV3> {
        let peer_text = peer.to_string();
        let pair = current
            .draft
            .boundaries
            .iter()
            .filter(|boundary| boundary.peer_fingerprint == peer_text)
            .collect::<Vec<_>>();
        if pair.is_empty()
            || pair.iter().any(|boundary| {
                boundary.local_confirmed_revision != boundary.mapping_revision
                    || boundary.peer_confirmed_revision != boundary.mapping_revision
            })
        {
            return Ok(current);
        }
        let draft_pair = encode_boundary_sync(
            &current.draft,
            peer,
            BoundarySyncPhase::Preview,
            BoundarySyncSurface::Boundary,
        )?;
        let applied_pair = current
            .applied
            .as_ref()
            .map(|applied| {
                encode_boundary_sync(
                    &applied.layout,
                    peer,
                    BoundarySyncPhase::Preview,
                    BoundarySyncSurface::Boundary,
                )
            })
            .transpose()?
            .flatten();
        if applied_pair == draft_pair && applied_pair.is_some() {
            return Ok(current);
        }
        let (saved, compiled) = BoundaryWorkspaceV3::apply_confirmed_pair(
            &self.store,
            current.storage_revision,
            peer,
            LOGICAL_BOUNDARY_CORNER_GUARD_PIXELS,
            DEFAULT_MINIMUM_SEGMENT_PIXELS,
        )?;
        let applied_layout = &saved
            .applied
            .as_ref()
            .expect("confirmed pair application always creates an applied snapshot")
            .layout;
        self.queue_boundary_workspace_for_peer_with_initiator(
            applied_layout,
            peer,
            BoundarySyncPhase::Applied,
            BoundarySyncSurface::Complete,
            initiator,
        )?;
        self.reload_cross_screen_runtime();
        append_log(
            &self.log_path,
            &format!(
                "boundary_workspace_v3 auto_applied storage_revision={} layout_revision={} boundaries={}",
                saved.storage_revision,
                compiled.revision(),
                compiled.boundaries().len()
            ),
        )?;
        Ok(saved)
    }

    fn apply_confirmed_boundary_pairs_if_ready(
        &mut self,
        mut current: BoundaryWorkspaceV3,
        initiator: crate::security::CertificateFingerprint,
    ) -> io::Result<BoundaryWorkspaceV3> {
        let mut peers = Vec::new();
        for boundary in &current.draft.boundaries {
            let peer =
                crate::security::CertificateFingerprint::from_str(&boundary.peer_fingerprint)?;
            if !peers.contains(&peer) {
                peers.push(peer);
            }
        }
        for peer in peers {
            current = self.apply_confirmed_boundary_pair_if_ready(current, peer, initiator)?;
        }
        Ok(current)
    }

    fn accept_remote_boundary_workspace(&mut self, sequence: u64) -> io::Result<()> {
        let (transport, remote) = self
            .remote_boundary_workspace()
            .filter(|(_, remote)| remote.sequence == sequence)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "远端布局预览已经更新，请检查最新版本",
                )
            })?;
        let peer = Self::remote_boundary_peer(&remote)?;
        let initiator =
            crate::security::CertificateFingerprint::from_str(&remote.initiator_fingerprint)?;
        let saved = match BoundaryWorkspaceV3::load(&self.store) {
            Ok(current) => {
                let proposed = accept_remote_preview(&current.draft, &remote)?;
                BoundaryWorkspaceV3::replace_draft_from_authenticated_peer(
                    &self.store,
                    current.storage_revision,
                    peer,
                    proposed,
                )?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                BoundaryWorkspaceV3::initialize(
                    &self.store,
                    initialize_from_remote_preview(&remote)?,
                )?
            }
            Err(error) => return Err(error),
        };
        match transport {
            BoundaryWorkspaceTransport::DeviceService => {
                self.last_product_boundary_sync_sequence = sequence;
            }
            BoundaryWorkspaceTransport::ActiveControlChannel => {
                self.last_boundary_sync_sequence = sequence;
            }
        }
        self.clear_remote_boundary_workspace(transport, sequence);
        self.queue_boundary_workspace_for_peer_with_initiator(
            &saved.draft,
            peer,
            BoundarySyncPhase::Preview,
            BoundarySyncSurface::Boundary,
            initiator,
        )?;
        let saved = self.apply_confirmed_boundary_pair_if_ready(saved, peer, initiator)?;
        append_log(
            &self.log_path,
            &format!(
                "boundary_workspace_v3 remote_preview_accepted sequence={} storage_revision={} draft_revision={}",
                sequence, saved.storage_revision, saved.draft.revision
            ),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn reconcile_remote_boundary_workspace(&mut self) -> io::Result<()> {
        let Some((transport, remote)) = self.remote_boundary_workspace() else {
            return Ok(());
        };
        let last_sequence = match transport {
            BoundaryWorkspaceTransport::DeviceService => self.last_product_boundary_sync_sequence,
            BoundaryWorkspaceTransport::ActiveControlChannel => self.last_boundary_sync_sequence,
        };
        if remote.sequence <= last_sequence {
            return Ok(());
        }
        match transport {
            BoundaryWorkspaceTransport::DeviceService => {
                self.last_product_boundary_sync_sequence = remote.sequence;
            }
            BoundaryWorkspaceTransport::ActiveControlChannel => {
                self.last_boundary_sync_sequence = remote.sequence;
            }
        }
        let peer = Self::remote_boundary_peer(&remote)?;
        if remote.phase == BoundarySyncPhase::Applied {
            let mut current = match BoundaryWorkspaceV3::load(&self.store) {
                Ok(current) => current,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    BoundaryWorkspaceV3::initialize(
                        &self.store,
                        initialize_from_remote_preview(&remote)?,
                    )?
                }
                Err(error) => return Err(error),
            };
            if !remote_pair_geometry_matches(&current.draft, &remote)? {
                return Ok(());
            }
            if let Some(proposed) = merge_matching_confirmations(&current.draft, &remote)? {
                current = BoundaryWorkspaceV3::replace_draft(
                    &self.store,
                    current.storage_revision,
                    proposed,
                )?;
            }
            if !current.has_unapplied_changes() {
                self.clear_remote_boundary_workspace(transport, remote.sequence);
                return Ok(());
            }
            let (saved, compiled) = BoundaryWorkspaceV3::apply_confirmed_pair(
                &self.store,
                current.storage_revision,
                peer,
                LOGICAL_BOUNDARY_CORNER_GUARD_PIXELS,
                DEFAULT_MINIMUM_SEGMENT_PIXELS,
            )?;
            self.clear_remote_boundary_workspace(transport, remote.sequence);
            self.reload_cross_screen_runtime();
            append_log(
                &self.log_path,
                &format!(
                    "boundary_workspace_v3 remote_applied sequence={} storage_revision={} layout_revision={} boundaries={}",
                    remote.sequence,
                    saved.storage_revision,
                    compiled.revision(),
                    compiled.boundaries().len()
                ),
            )?;
            return Ok(());
        }
        let current = match BoundaryWorkspaceV3::load(&self.store) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let remote_peer = remote
            .layout
            .screens
            .iter()
            .find(|screen| screen.device_fingerprint != remote.layout.local_fingerprint)
            .map(|screen| screen.device_fingerprint.as_str());
        let pair_has_boundaries = remote_peer.is_some_and(|peer| {
            current
                .draft
                .boundaries
                .iter()
                .any(|boundary| boundary.peer_fingerprint == peer)
        });
        if current.applied.is_none() && remote.layout.boundaries.is_empty() && !pair_has_boundaries
        {
            return Ok(());
        }
        if !remote_pair_geometry_matches(&current.draft, &remote)? {
            return Ok(());
        }
        let Some(proposed) = merge_matching_confirmations(&current.draft, &remote)? else {
            let peer_text = peer.to_string();
            let pair = current
                .draft
                .boundaries
                .iter()
                .filter(|boundary| boundary.peer_fingerprint == peer_text)
                .collect::<Vec<_>>();
            let local_confirmed = !pair.is_empty()
                && pair
                    .iter()
                    .all(|boundary| boundary.local_confirmed_revision == boundary.mapping_revision);
            let local = self.store.load_identity()?.fingerprint();
            let initiator =
                crate::security::CertificateFingerprint::from_str(&remote.initiator_fingerprint)?;
            if local_confirmed && initiator != local {
                self.queue_boundary_workspace_for_peer_with_initiator(
                    &current.draft,
                    peer,
                    BoundarySyncPhase::Preview,
                    remote.surface,
                    initiator,
                )?;
            }
            self.clear_remote_boundary_workspace(transport, remote.sequence);
            return Ok(());
        };
        let saved =
            BoundaryWorkspaceV3::replace_draft(&self.store, current.storage_revision, proposed)?;
        self.clear_remote_boundary_workspace(transport, remote.sequence);
        let initiator =
            crate::security::CertificateFingerprint::from_str(&remote.initiator_fingerprint)?;
        self.queue_boundary_workspace_for_peer_with_initiator(
            &saved.draft,
            peer,
            BoundarySyncPhase::Preview,
            remote.surface,
            initiator,
        )?;
        let saved = self.apply_confirmed_boundary_pair_if_ready(saved, peer, initiator)?;
        append_log(
            &self.log_path,
            &format!(
                "boundary_workspace_v3 peer_confirmation_merged sequence={} storage_revision={} draft_revision={}",
                remote.sequence, saved.storage_revision, saved.draft.revision
            ),
        )
    }

    fn refresh_ui(&mut self, command_target: HWND) {
        self.screenshot.poll();
        if self.license.poll() {
            if self.license.snapshot.active {
                self.reload_runtime();
                match crate::ssh::Runtime::start(
                    self.store.clone(),
                    self.discovery.as_ref().map(DiscoveryRuntime::view),
                ) {
                    Ok(runtime) => {
                        self.ssh = Some(runtime);
                        self.ssh_error.clear();
                    }
                    Err(error) => self.ssh_error = error.to_string(),
                }
            } else {
                if let Some(mut runtime) = self.cross_screen.take() {
                    runtime.stop();
                }
                if let Some(mut runtime) = self.runtime.take() {
                    runtime.stop();
                }
                self.ssh.take();
                self.runtime_error = Some("请联网验证授权 / License verification required".into());
            }
        }
        if self.last_log_rotation_check.elapsed() >= Duration::from_secs(30) {
            self.last_log_rotation_check = Instant::now();
            if let Ok(active) = crate::logging::ensure_current(&self.log_path)
                && active != self.active_log_path
            {
                self.active_log_path = active;
                let _ = append_log(&self.log_path, "log_rollover reason=local_midnight");
            }
        }
        self.poll_pairing_events(command_target);
        if let Some(message) = crate::traffic_control::diagnostic() {
            let _ = append_log(&self.log_path, &message);
        }
        self.update_network_notification(command_target);
        self.update_drag_failure_notification(command_target);
        self.update_ssh_notifications(command_target);
        self.log_network_path_status();
        self.log_cross_screen_status();
        if let Err(error) = self.update_icon_tip(command_target) {
            let _ = append_log(
                &self.log_path,
                &format!("tray_tooltip_update error={error}"),
            );
        }
        self.update_ssh_attention_icon(command_target);
        if let Err(error) = self.reconcile_remote_boundary_workspace() {
            let _ = append_log(
                &self.log_path,
                &format!("boundary_workspace_v3 reconcile_remote error={error}"),
            );
        }
        self.refresh_settings_window();
        let Some(snapshot) = self.runtime.as_ref().map(ProductRuntime::snapshot) else {
            return;
        };
        self.update_transfer_notifications(command_target, &snapshot);
        if !has_transfer(&snapshot) && self.transfer_window.is_none() {
            return;
        }
        match self
            .ensure_transfer_window(command_target)
            .and_then(|window| window.update(&snapshot))
        {
            Ok(()) => self.transfer_window_error = None,
            Err(error) => {
                let message = error.to_string();
                if self.transfer_window_error.as_deref() != Some(&message) {
                    let _ = append_log(
                        &self.log_path,
                        &format!("transfer_window_update error={error}"),
                    );
                    self.transfer_window_error = Some(message);
                }
            }
        }
    }

    fn poll_pairing_events(&mut self, command_target: HWND) {
        let events = self
            .pairing
            .as_ref()
            .map(PairingRuntime::take_events)
            .unwrap_or_default();
        for event in events {
            let toast = match event {
                PairingEvent::Pending(request) => {
                    let label = request.discovered.label.clone();
                    self.pending_pairing = Some(*request);
                    self.pairing_message = Some(format!(
                        "请在两台设备上核对验证码，然后确认是否配对“{label}”"
                    ));
                    Some("收到安全配对请求，请核对验证码".to_owned())
                }
                PairingEvent::Completed(result) => {
                    self.pending_pairing = None;
                    match result {
                        Ok(message) => {
                            self.pairing_message = Some(message.clone());
                            self.reload_runtime();
                            self.reload_cross_screen_runtime();
                            Some(message)
                        }
                        Err(error) => {
                            let message = format!("配对未完成：{error}");
                            self.pairing_message = Some(message.clone());
                            Some(message)
                        }
                    }
                }
            };
            if let Err(error) = self.show_settings(command_target, "devices") {
                let _ = append_log(
                    &self.log_path,
                    &format!("pairing_settings_open error={error}"),
                );
            }
            if let (Some(message), Some(settings)) = (toast, self.settings_window.as_ref()) {
                settings.toast(&message);
            }
        }
    }

    fn log_cross_screen_status(&mut self) {
        if let Some(runtime) = self.cross_screen.as_ref() {
            for trace in runtime.drain_diagnostic_traces() {
                let _ = append_log(&self.log_path, &format!("cross_screen_trace {trace}"));
            }
        }
        let status = self.cross_screen.as_ref().map_or_else(
            || {
                self.cross_screen_error.as_ref().map_or_else(
                    || "state=disabled".to_owned(),
                    |error| format!("state=failed error={error}"),
                )
            },
            |runtime| {
                let snapshot = runtime.snapshot();
                cross_screen_log_status(&snapshot)
            },
        );
        if status == self.last_cross_screen_log {
            return;
        }
        let _ = append_log(&self.log_path, &format!("cross_screen {status}"));
        self.last_cross_screen_log = status;
    }

    fn log_network_path_status(&mut self) {
        let status = self.active_network_issue().map_or_else(
            || "state=available_or_discovering".to_owned(),
            |issue| issue.log_fields(),
        );
        if status == self.last_network_path_log {
            return;
        }
        let _ = append_log(&self.log_path, &format!("network_path {status}"));
        self.last_network_path_log = status;
    }

    fn update_drag_failure_notification(&mut self, window: HWND) {
        let Some(message) = crate::clipboard::drag_live::take_connection_failure_notification()
        else {
            return;
        };
        if let Some(settings) = self.settings_window.as_ref() {
            settings.toast(&message);
        }
        let mut data = self.notify_data(window, &self.tooltip_text());
        data.uFlags |= NIF_INFO;
        copy_wide_to_fixed(
            localized(
                self.ui_language,
                "Pierdesk 文件拖拽未开始",
                "Pierdesk file drag could not start",
            ),
            &mut data.szInfoTitle,
        );
        copy_wide_to_fixed(
            localized(
                self.ui_language,
                "无法建立接收电脑的文件通道。请打开设置中心，查看“最近错误”中的对端名称和防火墙处理步骤；处理后重新拖拽。",
                "Cannot open the receiving PC's file channel. Open Settings and read Latest error for the receiving PC and firewall steps, then drag again.",
            ),
            &mut data.szInfo,
        );
        data.dwInfoFlags = NIIF_WARNING;
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
            let _ = append_log(
                &self.log_path,
                &format!(
                    "drag_failure_notification error={}",
                    io::Error::last_os_error()
                ),
            );
        }
    }

    fn update_network_notification(&mut self, window: HWND) {
        let issue = self.active_network_issue();
        let signature = issue
            .as_ref()
            .map_or_else(String::new, NetworkPathIssue::log_fields);
        if signature == self.last_network_notification {
            return;
        }
        self.last_network_notification = signature;
        let Some(issue) = issue else {
            return;
        };
        let mut data = self.notify_data(window, &self.tooltip_text());
        data.uFlags |= NIF_INFO;
        copy_wide_to_fixed(
            localized(
                self.ui_language,
                "Pierdesk 检测到网络地址冲突",
                "Pierdesk detected a network address conflict",
            ),
            &mut data.szInfoTitle,
        );
        copy_wide_to_fixed(
            &localized_network_issue(&issue, self.ui_language),
            &mut data.szInfo,
        );
        data.dwInfoFlags = NIIF_WARNING;
        // SAFETY: data identifies the live tray icon and all notification text is stored in its
        // fixed-size owned buffers for the duration of this synchronous call.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
            let _ = append_log(
                &self.log_path,
                &format!(
                    "network_path_notification error={}",
                    io::Error::last_os_error()
                ),
            );
        }
    }

    fn update_ssh_notifications(&mut self, window: HWND) {
        let settings_visible = self
            .settings_window
            .as_ref()
            .is_some_and(SettingsWindow::is_visible);
        let pending = self
            .ssh
            .as_ref()
            .map(crate::ssh::Runtime::pending_incoming_requests)
            .unwrap_or_default();
        let fresh = fresh_ssh_request_notifications(&mut self.notified_ssh_requests, &pending);
        if fresh.is_empty() {
            return;
        }

        let mut data = self.notify_data(window, &self.tooltip_text());
        data.uFlags |= NIF_INFO;
        copy_wide_to_fixed(
            localized(
                self.ui_language,
                "新的 SSH 访问请求",
                "New SSH access request",
            ),
            &mut data.szInfoTitle,
        );
        let body = if fresh.len() == 1 {
            format!(
                "{} {}",
                truncate_text(&fresh[0].1, 42),
                localized(
                    self.ui_language,
                    "请求访问这台电脑。打开“远程工作”进行审核。",
                    "is requesting access to this computer. Open Remote work to review it.",
                )
            )
        } else {
            format!(
                "{} {}",
                fresh.len(),
                localized(
                    self.ui_language,
                    "台设备正在等待审核。打开“远程工作”进行处理。",
                    "devices are waiting for review. Open Remote work to respond.",
                )
            )
        };
        copy_wide_to_fixed(&body, &mut data.szInfo);
        // NIIF_INFO deliberately leaves sound enabled so Windows uses the user's notification
        // sound and accessibility preferences.
        data.dwInfoFlags = NIIF_INFO;
        // SAFETY: data identifies the live tray icon and owns all fixed-size notification text.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
            let _ = append_log(
                &self.log_path,
                &format!(
                    "ssh_request_notification error={}",
                    io::Error::last_os_error()
                ),
            );
        }
        if settings_visible {
            // The global request banner is already visible in the open settings window, so the
            // request has been presented. Keep the one-time Windows sound/notification but do not
            // begin a tray flash that would otherwise continue until the window is opened again.
            self.clear_ssh_attention(window);
        } else {
            self.ssh_attention_active = true;
            self.ssh_attention_phase = false;
        }
        let _ = append_log(
            &self.log_path,
            &format!("ssh_request_notification count={}", fresh.len()),
        );
    }

    fn update_ssh_attention_icon(&mut self, window: HWND) {
        if !self.ssh_attention_active {
            return;
        }
        self.ssh_attention_phase = !self.ssh_attention_phase;
        let mut data = self.notify_data(window, &self.tooltip_text());
        data.uFlags = NIF_ICON;
        data.hIcon = if self.ssh_attention_phase {
            self.ssh_attention_icon
        } else {
            self.icon.handle
        };
        // SAFETY: both icon handles are live shared/owned resources and data names our tray icon.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
            let _ = append_log(
                &self.log_path,
                &format!("ssh_attention_icon error={}", io::Error::last_os_error()),
            );
        }
    }

    fn clear_ssh_attention(&mut self, window: HWND) {
        if !self.ssh_attention_active && !self.ssh_attention_phase {
            return;
        }
        self.ssh_attention_active = false;
        self.ssh_attention_phase = false;
        let mut data = self.notify_data(window, &self.tooltip_text());
        data.uFlags = NIF_ICON;
        data.hIcon = self.icon.handle;
        // SAFETY: the application icon remains live for the lifetime of TrayState.
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) };
        let _ = append_log(
            &self.log_path,
            "ssh_request_attention acknowledged=settings_opened",
        );
    }

    fn update_transfer_notifications(&mut self, window: HWND, snapshot: &ProductSnapshot) {
        let live = snapshot
            .transfer_tasks
            .iter()
            .map(|task| task.id)
            .collect::<HashSet<_>>();
        self.notified_terminal_transfers
            .retain(|id| live.contains(id));
        self.notified_started_transfers
            .retain(|id| live.contains(id));
        for task in &snapshot.transfer_tasks {
            // Announce the moment file data really starts moving (drag drop or remote paste),
            // regardless of the transfer window's popup preference.
            if should_notify_transfer_start(task.transfer.started_files, task.transfer.state)
                && self.notified_started_transfers.insert(task.id)
            {
                let notification = transfer_start_notification(
                    task.id.direction(),
                    &task.manifest.primary_name,
                    task.transfer.total_size,
                    &snapshot.active_peer_label,
                    self.ui_language,
                );
                self.show_transfer_balloon(window, &notification, task.id);
            }
            if task.transfer.started_files == 0 || !terminal_transfer(task.transfer.state) {
                continue;
            }
            let first_terminal = self.notified_terminal_transfers.insert(task.id);
            if !should_notify_transfer_completion(
                self.transfer_window_settings.auto_popup,
                task.transfer.started_files,
                task.transfer.state,
                first_terminal,
            ) {
                continue;
            }
            let Some(notification) = transfer_completion_notification(
                task.id.direction(),
                task.transfer.state,
                &task.manifest.primary_name,
                task.transfer.total_size,
                &snapshot.active_peer_label,
                self.ui_language,
            ) else {
                continue;
            };
            self.show_transfer_balloon(window, &notification, task.id);
        }
    }

    fn show_transfer_balloon(
        &self,
        window: HWND,
        notification: &TransferCompletionNotification,
        id: ProductTransferId,
    ) {
        let mut data = self.notify_data(window, &self.tooltip_text());
        data.uFlags |= NIF_INFO;
        copy_wide_to_fixed(&notification.title, &mut data.szInfoTitle);
        copy_wide_to_fixed(&notification.body, &mut data.szInfo);
        data.dwInfoFlags = if notification.warning {
            NIIF_WARNING
        } else {
            windows::Win32::UI::Shell::NIIF_INFO
        };
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
            let _ = append_log(
                &self.log_path,
                &format!(
                    "transfer_notification error={} id={}",
                    io::Error::last_os_error(),
                    id.short_label(),
                ),
            );
        }
    }

    fn show_transfer_window(&mut self, command_target: HWND) -> io::Result<()> {
        let snapshot = self
            .runtime
            .as_ref()
            .map(ProductRuntime::snapshot)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "文件传输服务尚未启动"))?;
        let window = self.ensure_transfer_window(command_target)?;
        window.update(&snapshot)?;
        window.show()
    }

    fn toggle_transfer_window_auto_popup(&mut self) -> io::Result<()> {
        let next = TransferWindowSettings {
            auto_popup: !self.transfer_window_settings.auto_popup,
        };
        next.save(&self.store)?;
        self.transfer_window_settings = next;
        if let Some(window) = &self.transfer_window {
            window.set_auto_popup(next.auto_popup);
        }
        append_log(
            &self.log_path,
            &format!("transfer_window_auto_popup enabled={}", next.auto_popup),
        )
    }

    fn handle_command(&mut self, window: HWND, command: usize) {
        let result = match command {
            COMMAND_STATUS => self.show_settings(window, "status"),
            COMMAND_SCREENSHOT => self.screenshot.launch(self.ui_language.key()),
            COMMAND_PAIR | COMMAND_MANAGE_PEERS => self.show_settings(window, "devices"),
            COMMAND_SETTINGS => self.show_settings(window, "connection"),
            COMMAND_ACCEPT_PENDING => self.product_command(ProductCommand::AcceptPending),
            COMMAND_TRANSFER_WINDOW => self.show_transfer_window(window),
            COMMAND_TRANSFER_WINDOW_AUTO_POPUP => self.toggle_transfer_window_auto_popup(),
            COMMAND_PAUSE => self.product_command(ProductCommand::Pause),
            COMMAND_RESUME => self.product_command(ProductCommand::Resume),
            COMMAND_CANCEL => self.cancel_all_transfers(),
            COMMAND_TRANSFER_DISPATCH => self.dispatch_transfer_commands(window),
            COMMAND_SETTINGS_DISPATCH => {
                self.dispatch_settings_commands(window);
                Ok(())
            }
            COMMAND_AUTOSTART => self.toggle_autostart(window),
            COMMAND_LOG => self.open_log(),
            COMMAND_ABOUT => self.show_settings(window, "about"),
            COMMAND_CROSS_SCREEN_ENABLE => self.toggle_cross_screen_enabled(),
            COMMAND_CROSS_SCREEN_PAUSE => {
                self.cross_screen_command("pause", CrossScreenProductRuntime::pause)
            }
            COMMAND_CROSS_SCREEN_RESUME => {
                self.cross_screen_command("resume", CrossScreenProductRuntime::resume)
            }
            COMMAND_CROSS_SCREEN_RETURN_LOCAL => {
                self.cross_screen_command("return_local", CrossScreenProductRuntime::return_local)
            }
            COMMAND_CROSS_SCREEN_LEFT => self.set_cross_screen_placement(PeerPlacement::Left),
            COMMAND_CROSS_SCREEN_RIGHT => self.set_cross_screen_placement(PeerPlacement::Right),
            COMMAND_CROSS_SCREEN_ABOVE => self.set_cross_screen_placement(PeerPlacement::Above),
            COMMAND_CROSS_SCREEN_BELOW => self.set_cross_screen_placement(PeerPlacement::Below),
            COMMAND_CROSS_SCREEN_CORNER_GUARD => self.update_cross_screen_settings(|settings| {
                settings.corner_guard_enabled = !settings.corner_guard_enabled;
            }),
            COMMAND_CROSS_SCREEN_CORNER_SIZE => self.show_corner_guard_slider(window),
            COMMAND_CROSS_SCREEN_SECOND_PUSH => self.update_cross_screen_settings(|settings| {
                settings.require_second_push = !settings.require_second_push;
                settings.layout_revision = next_layout_revision(settings.layout_revision);
            }),
            COMMAND_CROSS_SCREEN_FULLSCREEN_PAUSE => {
                self.update_cross_screen_settings(|settings| {
                    settings.pause_on_fullscreen = !settings.pause_on_fullscreen;
                })
            }
            COMMAND_CROSS_SCREEN_ADMIN_RESTART => Self::restart_elevated(),
            COMMAND_CROSS_SCREEN_SETTINGS => self.show_settings(window, "cross"),
            COMMAND_SHARED_CLIPBOARD_ENABLE => self.toggle_shared_clipboard_enabled(),
            COMMAND_EXIT => {
                if !self.confirm_ssh_exit() {
                    return;
                }
                let _ = append_log(&self.log_path, "tray_exit requested=true");
                // SAFETY: this ends the message loop owned by the current tray thread.
                unsafe { PostQuitMessage(0) };
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            let _ = append_log(&self.log_path, &format!("tray_command_error error={error}"));
            show_error(
                window,
                &localized_operation_error(
                    self.ui_language,
                    "操作失败",
                    "The operation failed",
                    &error.to_string(),
                ),
            );
        }
    }

    fn show_status(&self, window: HWND) -> io::Result<()> {
        let snapshot = self.snapshot()?;
        show_information(
            window,
            &snapshot,
            localized(self.ui_language, "Pierdesk 状态", "Pierdesk Status"),
        );
        Ok(())
    }

    fn show_runtime_error(&self, window: HWND) {
        let error = self
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.snapshot().last_error)
            .or_else(|| self.runtime_error.clone())
            .or_else(|| {
                self.cross_screen
                    .as_ref()
                    .and_then(|runtime| runtime.snapshot().last_error)
            })
            .or_else(|| self.cross_screen_error.clone())
            .unwrap_or_else(|| {
                localized(
                    self.ui_language,
                    "后台传输发生未知错误",
                    "An unknown background operation failed",
                )
                .to_owned()
            });
        show_error(
            window,
            &localized_operation_error(
                self.ui_language,
                "后台操作失败",
                "The background operation failed",
                &error,
            ),
        );
    }

    fn start_pairing_candidate(&self, fingerprint: &str) -> io::Result<()> {
        let fingerprint = crate::security::CertificateFingerprint::from_str(fingerprint)?;
        let discovery = self.discovery.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                self.discovery_error
                    .as_deref()
                    .unwrap_or("局域网自动发现尚未启动"),
            )
        })?;
        let pairing = self
            .pairing
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "自动配对监听器尚未启动"))?;
        let peer = discovery.view().peer(fingerprint).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "这台设备已离开局域网，请刷新后重试",
            )
        })?;
        if self.store.load_peer(fingerprint).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "该设备已经配对",
            ));
        }
        pairing.connect(
            self.store.clone(),
            discovery.view(),
            peer,
            self.log_path.clone(),
        )
    }

    fn command_settings_task(&self, key: &str, pause: bool) -> io::Result<()> {
        let task = self
            .runtime
            .as_ref()
            .map(ProductRuntime::snapshot)
            .and_then(|snapshot| {
                snapshot
                    .transfer_tasks
                    .into_iter()
                    .find(|task| settings_task_key(task.id) == key)
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "传输任务已结束或不存在"))?;
        let _ = append_log(
            &self.log_path,
            &format!(
                "transfer_control_ui task={} direction={:?} toggle_pause={pause} state={:?} supported={}",
                settings_task_key(task.id),
                task.id.direction(),
                task.transfer.state,
                task.locally_controllable()
            ),
        );
        if !task.locally_controllable() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "This transfer has ended and can no longer be controlled.",
            ));
        }
        let command = if pause {
            if task.transfer.state == ProductTransferState::Paused {
                ProductCommand::ResumeTask(task.id)
            } else {
                ProductCommand::PauseTask(task.id)
            }
        } else {
            ProductCommand::CancelTask(task.id)
        };
        self.product_command(command)
    }

    fn product_command(&self, command: ProductCommand) -> io::Result<()> {
        self.runtime
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "后台传输服务尚未启动"))?
            .command(command)
    }

    fn dispatch_transfer_commands(&mut self, window: HWND) -> io::Result<()> {
        let commands = self
            .transfer_window
            .as_ref()
            .map(TransferWindow::take_commands)
            .unwrap_or_default();
        for command in commands {
            self.product_command(command)?;
        }
        self.refresh_ui(window);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch_settings_commands(&mut self, window: HWND) {
        let commands = self
            .settings_window
            .as_ref()
            .map(SettingsWindow::take_commands)
            .unwrap_or_default();
        let mut boundary_save_result = None;
        for command in commands {
            let boundary_save = matches!(
                &command,
                SettingsWindowCommand::SaveBoundaryWorkspaceV3 { .. }
            );
            let success_toast = settings_command_success_toast(&command);
            // Keep command failures inside this iteration. Several arms use `?`; without this
            // closure those errors escaped `dispatch_settings_commands` before the WebView
            // received its save receipt, leaving the boundary editor permanently locked in its
            // `saving` state and surfacing an unrelated native error dialog.
            let result: io::Result<()> = (|| match command {
                #[cfg(feature = "licensing")]
                SettingsWindowCommand::LicenseActivate(code) => self
                    .license
                    .command(crate::license_runtime::Command::Activate(code.0)),
                #[cfg(feature = "licensing")]
                SettingsWindowCommand::LicenseRefresh => self
                    .license
                    .command(crate::license_runtime::Command::Refresh),
                #[cfg(feature = "licensing")]
                SettingsWindowCommand::LicenseRelease => self
                    .license
                    .command(crate::license_runtime::Command::Release),
                SettingsWindowCommand::Refresh => Ok(()),
                SettingsWindowCommand::Pair(fingerprint) => {
                    self.start_pairing_candidate(&fingerprint)
                }
                SettingsWindowCommand::ApprovePairing(approved) => {
                    let request = self.pending_pairing.take().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "当前没有等待确认的配对请求")
                    })?;
                    let pairing = self.pairing.as_ref().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotConnected, "自动配对监听器尚未启动")
                    })?;
                    self.pairing_message = Some(if approved {
                        "已批准，正在等待另一台设备确认…".to_owned()
                    } else {
                        "正在安全拒绝配对请求…".to_owned()
                    });
                    pairing.confirm(self.store.clone(), request, approved, self.log_path.clone())
                }
                SettingsWindowCommand::Revoke(fingerprint) => {
                    if let Some(ssh) = &self.ssh {
                        ssh.ensure_can_unpair(&fingerprint)?;
                    }
                    let fingerprint =
                        crate::security::CertificateFingerprint::from_str(&fingerprint)?;
                    self.store.revoke_peer(fingerprint)?;
                    self.reload_runtime();
                    self.reload_cross_screen_runtime();
                    Ok(())
                }
                SettingsWindowCommand::SelectPeer(fingerprint) => {
                    self.activate_settings_peer(&fingerprint)
                }
                SettingsWindowCommand::Ssh(command) => self
                    .ssh
                    .as_ref()
                    .ok_or_else(|| io::Error::other("SSH runtime unavailable"))?
                    .command(command),
                SettingsWindowCommand::SetLayout {
                    fingerprint,
                    placement,
                } => {
                    self.activate_settings_peer(&fingerprint)?;
                    self.set_cross_screen_placement(parse_settings_placement(&placement)?)
                }
                SettingsWindowCommand::SaveBoundaryMap(value) => {
                    let proposed: BoundaryMapV2 =
                        serde_json::from_value(value).map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("边界地图格式无效：{error}"),
                            )
                        })?;
                    let trusted = self
                        .store
                        .list_peers()?
                        .into_iter()
                        .map(|peer| peer.fingerprint)
                        .collect::<HashSet<_>>();
                    for segment in &proposed.segments {
                        let fingerprint = crate::security::CertificateFingerprint::from_str(
                            &segment.peer_fingerprint,
                        )?;
                        if !trusted.contains(&fingerprint) {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "边界地图引用了未配对设备",
                            ));
                        }
                    }
                    let settings = CrossScreenSettings::load_or_default(&self.store)?;
                    let saved = BoundaryMapV2::replace(
                        &self.store,
                        proposed,
                        current_boundary_constraints(&settings)?,
                    )?;
                    append_log(
                        &self.log_path,
                        &format!(
                            "boundary_map saved_revision={} segments={}",
                            saved.revision,
                            saved.segments.len()
                        ),
                    )?;
                    self.reload_cross_screen_runtime();
                    Ok(())
                }
                SettingsWindowCommand::PreviewBoundaryWorkspaceV3 { layout } => {
                    let proposed = self.validate_boundary_workspace_layout(layout)?;
                    self.queue_boundary_workspace_for_peers(
                        &proposed,
                        BoundarySyncPhase::Preview,
                        BoundarySyncSurface::Spatial,
                    )
                }
                SettingsWindowCommand::AcceptBoundaryWorkspacePreview { sequence } => {
                    self.accept_remote_boundary_workspace(sequence)
                }
                SettingsWindowCommand::SaveBoundaryWorkspaceV3 {
                    expected_storage_revision,
                    layout,
                    surface,
                } => {
                    let surface = match surface.as_str() {
                        "spatial" => BoundarySyncSurface::Spatial,
                        "boundary" => BoundarySyncSurface::Boundary,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "设置窗口提交了未知的边界编辑页面",
                            ));
                        }
                    };
                    let proposed = self.validate_boundary_workspace_layout(layout)?;
                    let saved = match BoundaryWorkspaceV3::load(&self.store) {
                        Ok(_) => BoundaryWorkspaceV3::replace_draft(
                            &self.store,
                            expected_storage_revision,
                            proposed,
                        )?,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            if expected_storage_revision != 0 {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "逻辑边界草稿基于已过期的工作区修订",
                                ));
                            }
                            BoundaryWorkspaceV3::initialize(&self.store, proposed)?
                        }
                        Err(error) => return Err(error),
                    };
                    self.queue_boundary_workspace_for_peers(
                        &saved.draft,
                        BoundarySyncPhase::Preview,
                        surface,
                    )?;
                    let initiator = self.store.load_identity()?.fingerprint();
                    let saved = self.apply_confirmed_boundary_pairs_if_ready(saved, initiator)?;
                    append_log(
                        &self.log_path,
                        &format!(
                            "boundary_workspace_v3 saved_storage_revision={} draft_revision={} boundaries={}",
                            saved.storage_revision,
                            saved.draft.revision,
                            saved.draft.boundaries.len()
                        ),
                    )
                }
                SettingsWindowCommand::ApplyBoundaryWorkspaceV3 {
                    expected_storage_revision,
                } => {
                    if !boundary_workspace_v3_runtime_ready() {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "P2.5 双端映射协议和稳定运行时入口尚未完成",
                        ));
                    }
                    let online = self
                        .discovery
                        .as_ref()
                        .map(|runtime| {
                            runtime
                                .view()
                                .peers()
                                .into_iter()
                                .map(|peer| peer.fingerprint)
                                .collect::<HashSet<_>>()
                        })
                        .unwrap_or_default();
                    let (saved, compiled) = BoundaryWorkspaceV3::apply(
                        &self.store,
                        expected_storage_revision,
                        &online,
                        LOGICAL_BOUNDARY_CORNER_GUARD_PIXELS,
                        DEFAULT_MINIMUM_SEGMENT_PIXELS,
                    )?;
                    if let Some(applied) = saved.applied.as_ref() {
                        self.queue_boundary_workspace_for_peers(
                            &applied.layout,
                            BoundarySyncPhase::Applied,
                            BoundarySyncSurface::Complete,
                        )?;
                    }
                    self.reload_cross_screen_runtime();
                    append_log(
                        &self.log_path,
                        &format!(
                            "boundary_workspace_v3 applied_storage_revision={} layout_revision={} boundaries={}",
                            saved.storage_revision,
                            compiled.revision(),
                            compiled.boundaries().len()
                        ),
                    )
                }
                SettingsWindowCommand::SaveConnection {
                    active_peer,
                    local_endpoint,
                    peer_endpoint,
                    auto_receive,
                } => {
                    let settings = AppSettings {
                        active_peer: crate::security::CertificateFingerprint::from_str(
                            &active_peer,
                        )?,
                        local_endpoint: local_endpoint.parse().map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "本机服务地址格式无效")
                        })?,
                        peer_endpoint: peer_endpoint.parse().map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "对端服务地址格式无效")
                        })?,
                        auto_receive,
                    };
                    settings.save(&self.store)?;
                    self.active_peer = Some(settings.active_peer);
                    self.reload_runtime();
                    self.reload_cross_screen_runtime();
                    Ok(())
                }
                SettingsWindowCommand::SetCrossScreenEnabled(enabled) => {
                    let current = CrossScreenSettings::load_or_default(&self.store)?.enabled;
                    if current != enabled {
                        self.toggle_cross_screen_enabled()?;
                    }
                    Ok(())
                }
                SettingsWindowCommand::SetPairEnabled {
                    fingerprint,
                    enabled,
                } => self.set_pair_enabled(&fingerprint, enabled),
                SettingsWindowCommand::SetBoundaryMapV2Enabled(enabled) => self
                    .update_cross_screen_settings(|settings| {
                        settings.boundary_map_v2_enabled = enabled;
                    }),
                SettingsWindowCommand::SetPlacement(placement) => {
                    self.set_cross_screen_placement(parse_settings_placement(&placement)?)
                }
                SettingsWindowCommand::SetCornerGuardEnabled(enabled) => self
                    .update_cross_screen_settings(|settings| {
                        settings.corner_guard_enabled = enabled;
                    }),
                SettingsWindowCommand::SetCornerGuardPixels(pixels) => {
                    if pixels > 512 {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "四角保护范围必须在 0–512 个物理像素之间",
                        ))
                    } else {
                        self.update_cross_screen_settings(|settings| {
                            settings.corner_guard_pixels = pixels;
                        })
                    }
                }
                SettingsWindowCommand::SetSecondPush(enabled) => {
                    self.update_cross_screen_settings(|settings| {
                        settings.require_second_push = enabled;
                        settings.layout_revision = next_layout_revision(settings.layout_revision);
                    })
                }
                SettingsWindowCommand::SetFullscreenPause(enabled) => self
                    .update_cross_screen_settings(|settings| {
                        settings.pause_on_fullscreen = enabled;
                    }),
                SettingsWindowCommand::SetKeepLinkWarm(enabled) => self.set_keep_link_warm(enabled),
                SettingsWindowCommand::PauseCrossScreen => {
                    self.cross_screen_command("pause", CrossScreenProductRuntime::pause)
                }
                SettingsWindowCommand::ResumeCrossScreen => {
                    self.cross_screen_command("resume", CrossScreenProductRuntime::resume)
                }
                SettingsWindowCommand::ReturnLocal => self
                    .cross_screen_command("return_local", CrossScreenProductRuntime::return_local),
                SettingsWindowCommand::RestartElevated => Self::restart_elevated(),
                SettingsWindowCommand::ShowCompatibility => self.show_cross_screen_settings(window),
                SettingsWindowCommand::SetSharedClipboardEnabled(enabled) => {
                    let current = SharedClipboardSettings::load_or_default(&self.store)?.enabled;
                    if current != enabled {
                        self.toggle_shared_clipboard_enabled()?;
                    }
                    Ok(())
                }
                SettingsWindowCommand::Screenshot => self.screenshot.launch(self.ui_language.key()),
                SettingsWindowCommand::SetScreenshotShortcut(value) => {
                    self.screenshot.change(window, self.store.root(), &value)
                }
                SettingsWindowCommand::OpenTransfer { history } => {
                    let snapshot = self
                        .runtime
                        .as_ref()
                        .map(ProductRuntime::snapshot)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::NotConnected, "文件传输服务尚未启动")
                        })?;
                    let transfer = self.ensure_transfer_window(window)?;
                    transfer.update(&snapshot)?;
                    if history {
                        transfer.show_history()
                    } else {
                        transfer.show()
                    }
                }
                SettingsWindowCommand::SetTransferAutoPopup(enabled) => {
                    if self.transfer_window_settings.auto_popup != enabled {
                        self.toggle_transfer_window_auto_popup()?;
                    }
                    Ok(())
                }
                SettingsWindowCommand::AcceptPending => {
                    self.product_command(ProductCommand::AcceptPending)
                }
                SettingsWindowCommand::PauseAll => self.product_command(ProductCommand::Pause),
                SettingsWindowCommand::ResumeAll => self.product_command(ProductCommand::Resume),
                SettingsWindowCommand::CancelAll => self.product_command(ProductCommand::Cancel),
                SettingsWindowCommand::PauseTask(key) => self.command_settings_task(&key, true),
                SettingsWindowCommand::CancelTask(key) => self.command_settings_task(&key, false),
                SettingsWindowCommand::SetHistoryLimit(limit) => self
                    .ensure_transfer_window(window)?
                    .set_history_limit(limit),
                SettingsWindowCommand::SetHistoryRowHeight(height) => self
                    .ensure_transfer_window(window)?
                    .set_history_row_height(height),
                SettingsWindowCommand::SetAutostart(enabled) => write_autostart(enabled),
                SettingsWindowCommand::SetLanguage(language) => {
                    let language = match language.as_str() {
                        "en" => UiLanguage::English,
                        "zh-CN" => UiLanguage::ChineseSimplified,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "不支持的界面语言",
                            ));
                        }
                    };
                    UiPreferences {
                        language,
                        theme: self.ui_theme,
                        font_scale_percent: self.ui_font_scale_percent,
                    }
                    .save(&self.store)?;
                    self.ui_language = language;
                    if let Some(window) = self.transfer_window.as_ref() {
                        window.set_language(language);
                    }
                    Ok(())
                }
                SettingsWindowCommand::SetTheme(theme) => {
                    let theme = match theme.as_str() {
                        "light" => UiTheme::Light,
                        "dark" => UiTheme::Dark,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "不支持的界面主题",
                            ));
                        }
                    };
                    UiPreferences {
                        language: self.ui_language,
                        theme,
                        font_scale_percent: self.ui_font_scale_percent,
                    }
                    .save(&self.store)?;
                    self.ui_theme = theme;
                    if let Some(window) = self.transfer_window.as_ref() {
                        window.set_theme(theme);
                    }
                    Ok(())
                }
                SettingsWindowCommand::SetFontScale(font_scale_percent) => {
                    if !(MIN_UI_FONT_SCALE_PERCENT..=MAX_UI_FONT_SCALE_PERCENT)
                        .contains(&font_scale_percent)
                        || !font_scale_percent.is_multiple_of(5)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "字体缩放必须为 50% 到 125% 之间的 5% 整数步进",
                        ));
                    }
                    UiPreferences {
                        language: self.ui_language,
                        theme: self.ui_theme,
                        font_scale_percent,
                    }
                    .save(&self.store)?;
                    self.ui_font_scale_percent = font_scale_percent;
                    Ok(())
                }
                SettingsWindowCommand::OpenLog => self.open_log(),
                SettingsWindowCommand::OpenLogFolder => self.open_log_folder(),
                SettingsWindowCommand::OpenSource => Self::open_source(),
                SettingsWindowCommand::Exit => {
                    if !self.confirm_ssh_exit() {
                        return Ok(());
                    }
                    unsafe { PostQuitMessage(0) };
                    Ok(())
                }
            })();
            match result {
                Ok(()) => {
                    if let (Some(settings), Some(message)) =
                        (self.settings_window.as_ref(), success_toast)
                    {
                        if boundary_save {
                            boundary_save_result = Some((true, message.to_owned()));
                        } else {
                            settings.toast(message);
                        }
                    }
                }
                Err(error) => {
                    if let Some(settings) = self.settings_window.as_ref() {
                        let message = format!("操作未完成：{error}");
                        if boundary_save {
                            boundary_save_result = Some((false, message));
                        } else {
                            settings.toast(&message);
                        }
                    }
                    let _ = append_log(
                        &self.log_path,
                        &format!("settings_window_command error={error}"),
                    );
                }
            }
        }
        self.refresh_settings_window();
        if let (Some(settings), Some((success, message))) =
            (self.settings_window.as_ref(), boundary_save_result)
        {
            settings.boundary_save_result(success, &message);
        }
    }

    fn cancel_all_transfers(&self) -> io::Result<()> {
        if ask_yes_no(
            "取消全部传输任务？\r\n\r\n所有正在传输、已暂停和等待粘贴的发送及接收任务都将被取消，此操作不能继续。已完成的文件不受影响。",
            "Pierdesk · 确认全部取消",
        ) {
            self.product_command(ProductCommand::Cancel)?;
        }
        Ok(())
    }

    fn reload_runtime(&mut self) {
        self.runtime.take();
        self.cross_screen.take();
        self.last_boundary_sync_sequence = 0;
        self.last_product_boundary_sync_sequence = 0;
        let (runtime, error) = start_product_runtime(&self.store);
        let (cross_screen, cross_screen_error) = start_cross_screen_runtime(
            &self.store,
            self.discovery.as_ref().map(DiscoveryRuntime::view),
        );
        self.runtime = runtime;
        self.runtime_error = error;
        self.cross_screen = cross_screen;
        self.cross_screen_error = cross_screen_error;
        self.active_peer = AppSettings::load(&self.store)
            .ok()
            .map(|settings| settings.active_peer);
        let message = self.runtime_error.as_ref().map_or_else(
            || "settings_reload success=true".to_owned(),
            |error| format!("settings_reload success=false error={error}"),
        );
        let _ = append_log(&self.log_path, &message);
    }

    fn reload_cross_screen_runtime(&mut self) {
        self.cross_screen.take();
        self.last_boundary_sync_sequence = 0;
        let (runtime, error) = start_cross_screen_runtime(
            &self.store,
            self.discovery.as_ref().map(DiscoveryRuntime::view),
        );
        self.cross_screen = runtime;
        self.cross_screen_error = error;
    }

    fn cross_screen_command(
        &self,
        action: &str,
        command: impl FnOnce(&CrossScreenProductRuntime),
    ) -> io::Result<()> {
        let runtime = self
            .cross_screen
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "跨屏控制服务尚未启动"))?;
        command(runtime);
        let _ = append_log(
            &self.log_path,
            &format!("cross_screen_command action={action} requested=true"),
        );
        Ok(())
    }

    /// Switches one computer pair. The runtime commits only after the peer acknowledges the change.
    fn set_pair_enabled(&mut self, fingerprint: &str, enabled: bool) -> io::Result<()> {
        let peer = crate::security::CertificateFingerprint::from_str(fingerprint)?;
        let runtime = self
            .cross_screen
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "跨屏控制服务尚未启动"))?;
        runtime.set_pair_enabled(peer.bytes(), enabled)?;
        let _ = append_log(
            &self.log_path,
            &format!("pair_switch peer={peer} enabled={enabled} committed=true"),
        );
        Ok(())
    }

    fn toggle_cross_screen_enabled(&mut self) -> io::Result<()> {
        self.update_cross_screen_settings(|settings| settings.enabled = !settings.enabled)
    }

    fn toggle_shared_clipboard_enabled(&mut self) -> io::Result<()> {
        self.update_shared_clipboard_settings(|settings| {
            settings.enabled = !settings.enabled;
        })
    }

    fn update_shared_clipboard_settings(
        &mut self,
        update: impl FnOnce(&mut SharedClipboardSettings),
    ) -> io::Result<()> {
        self.runtime.take();
        let result =
            SharedClipboardSettings::load_or_default(&self.store).and_then(|mut settings| {
                update(&mut settings);
                settings.save(&self.store)
            });
        let (runtime, error) = start_product_runtime(&self.store);
        self.runtime = runtime;
        self.runtime_error = error;
        result
    }

    fn set_cross_screen_placement(&mut self, placement: PeerPlacement) -> io::Result<()> {
        let app = AppSettings::load(&self.store)?;
        let local = self.store.load_identity()?.fingerprint();
        let peer = self.store.load_peer(app.active_peer)?.fingerprint;
        let local_is_listener = local.bytes() < peer.bytes();
        self.update_cross_screen_settings(|settings| {
            let revision = next_layout_revision(settings.layout_revision);
            settings.set_local_peer_placement(local_is_listener, placement, revision);
        })
    }

    fn activate_settings_peer(&mut self, fingerprint: &str) -> io::Result<()> {
        let fingerprint = crate::security::CertificateFingerprint::from_str(fingerprint)?;
        if self.active_peer == Some(fingerprint) {
            return Ok(());
        }
        let previous_auto_receive =
            AppSettings::load(&self.store).map_or(true, |settings| settings.auto_receive);
        let discovered = self
            .discovery
            .as_ref()
            .and_then(|runtime| runtime.view().peer(fingerprint))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "该历史设备当前离线；设备重新上线后即可快速切换",
                )
            })?;
        activate_discovered_peer(&self.store, &discovered)?;
        let mut settings = AppSettings::load(&self.store)?;
        settings.auto_receive = previous_auto_receive;
        settings.save(&self.store)?;
        self.active_peer = Some(fingerprint);
        self.reload_runtime();
        self.reload_cross_screen_runtime();
        Ok(())
    }

    fn show_corner_guard_slider(&mut self, owner: HWND) -> io::Result<()> {
        let settings = CrossScreenSettings::load_or_default(&self.store)?;
        if let Some(pixels) = crate::corner_guard_window::show(owner, settings.corner_guard_pixels)?
        {
            self.update_cross_screen_settings(|settings| {
                settings.corner_guard_pixels = pixels;
                settings.corner_guard_enabled = pixels != 0;
            })?;
        }
        Ok(())
    }

    fn local_cross_screen_placement(&self, settings: &CrossScreenSettings) -> PeerPlacement {
        let Ok(app) = AppSettings::load(&self.store) else {
            return settings.listener_peer_placement;
        };
        let Ok(local) = self.store.load_identity() else {
            return settings.listener_peer_placement;
        };
        let Ok(peer) = self.store.load_peer(app.active_peer) else {
            return settings.listener_peer_placement;
        };
        settings.local_peer_placement(local.fingerprint().bytes() < peer.fingerprint.bytes())
    }

    fn set_keep_link_warm(&self, enabled: bool) -> io::Result<()> {
        crate::cross_screen::link_warmth::LinkWarmthSettings {
            keep_link_warm: enabled,
        }
        .save(&self.store)?;
        // The channel writers and input pump read this live; no runtime restart is needed.
        crate::cross_screen::link_warmth::set_enabled(enabled);
        let _ = append_log(
            &self.log_path,
            &format!("link_warmth_setting enabled={enabled}"),
        );
        Ok(())
    }

    fn update_cross_screen_settings(
        &mut self,
        update: impl FnOnce(&mut CrossScreenSettings),
    ) -> io::Result<()> {
        self.cross_screen.take();
        let result = CrossScreenSettings::load_or_default(&self.store).and_then(|mut settings| {
            update(&mut settings);
            settings.save(&self.store)
        });
        self.reload_cross_screen_runtime();
        result
    }

    fn restart_elevated() -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let executable = wide_null(executable.as_os_str());
        let parameters = wide_null(OsStr::new(&format!("tray-replace {}", std::process::id())));
        // SAFETY: strings are live, null-terminated, and ShellExecuteW copies them before return.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("runas"),
                PCWSTR(executable.as_ptr()),
                PCWSTR(parameters.as_ptr()),
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            return Err(io::Error::other(format!(
                "Windows 未能以管理员身份启动 Pierdesk（ShellExecute={}）",
                result.0 as isize
            )));
        }
        // SAFETY: the elevated replacement was launched; ending this message loop triggers
        // orderly runtime drops and releases all cross-screen input before process exit.
        unsafe { PostQuitMessage(0) };
        Ok(())
    }

    fn show_cross_screen_settings(&self, window: HWND) -> io::Result<()> {
        let settings = CrossScreenSettings::load_or_default(&self.store)?;
        let placement = self
            .cross_screen
            .as_ref()
            .map_or(settings.listener_peer_placement, |runtime| {
                runtime.snapshot().peer_placement
            });
        let privilege = if unsafe { IsUserAnAdmin() }.as_bool() {
            "管理员权限"
        } else {
            "普通权限"
        };
        show_information(
            window,
            &format!(
                "启用跨屏控制：{}\r\n对端位置：{}\r\n屏幕四角防误触：{}（{} 个物理像素）\r\n二次顶边确认：{}\r\n全屏应用运行时暂停：{}\r\n当前权限：{}\r\n\r\n可在托盘菜单直接修改以上设置。屏幕布局会通过已配对的双向 TLS 通道自动同步；四角保护范围是本机独立设置，只影响鼠标从本机跨向对端。\r\n\r\n普通权限运行时不能控制管理员权限窗口；需要时可从托盘选择“以管理员身份重启”，也可以自行设置 EXE 兼容性管理员启动。管理员权限仍不能控制 UAC 安全桌面、登录界面或 Ctrl+Alt+Del。\r\n\r\n跨屏控制面向 Windows 桌面应用，不针对游戏、反作弊程序、Raw Input、独占全屏或远程桌面进行兼容优化。跨设备文件移动请继续使用 Pierdesk 文件剪贴板。",
                enabled_text(settings.enabled),
                placement_text(placement),
                enabled_text(settings.corner_guard_enabled),
                settings.corner_guard_pixels,
                enabled_text(settings.require_second_push),
                enabled_text(settings.pause_on_fullscreen),
                privilege,
            ),
            "Pierdesk 跨屏控制设置",
        );
        Ok(())
    }

    fn toggle_autostart(&self, window: HWND) -> io::Result<()> {
        let enable = query_autostart()? != AutostartState::Enabled;
        write_autostart(enable)?;
        append_log(
            &self.log_path,
            if enable {
                "autostart enabled=true"
            } else {
                "autostart enabled=false"
            },
        )?;
        show_information(
            window,
            if enable {
                "已启用当前 Windows 用户登录后自动启动；管理员模式使用交互登录计划任务。"
            } else {
                "已关闭开机自启动。"
            },
            "Pierdesk",
        );
        Ok(())
    }

    fn open_log(&self) -> io::Result<()> {
        append_log(&self.log_path, "diagnostics_open requested=true")?;
        let active = crate::logging::active_path(&self.log_path);
        let path = wide_null(active.as_os_str());
        // SAFETY: path is null terminated and ShellExecuteW copies it during the call.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            return Err(io::Error::other(format!(
                "Windows 无法打开诊断日志（ShellExecute={}）",
                result.0 as isize
            )));
        }
        Ok(())
    }

    fn open_log_folder(&self) -> io::Result<()> {
        let folder = self
            .log_path
            .parent()
            .ok_or_else(|| io::Error::other("诊断日志目录无效"))?;
        let folder = wide_null(folder.as_os_str());
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(folder.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            return Err(io::Error::other(format!(
                "Windows 无法打开诊断日志文件夹（ShellExecute={}）",
                result.0 as isize
            )));
        }
        Ok(())
    }

    fn open_source() -> io::Result<()> {
        let address = wide_null(OsStr::new("https://github.com/micky-meecky/Pierdesk"));
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(address.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            return Err(io::Error::other(format!(
                "Windows 无法打开项目主页（ShellExecute={}）",
                result.0 as isize
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn append_cross_screen_menu(&self, menu: &PopupMenu, language: UiLanguage) -> io::Result<()> {
        let settings = CrossScreenSettings::load_or_default(&self.store)?;
        let snapshot = self
            .cross_screen
            .as_ref()
            .map(CrossScreenProductRuntime::snapshot);
        let state = snapshot.as_ref().map(|snapshot| snapshot.state);
        let placement = snapshot.as_ref().map_or_else(
            || self.local_cross_screen_placement(&settings),
            |snapshot| snapshot.peer_placement,
        );
        menu.append(
            localized(language, "跨屏控制", "Cross-screen control"),
            0,
            MF_STRING | MF_DISABLED,
        )?;
        menu.append(
            localized(language, "开启跨屏控制", "Enable cross-screen control"),
            COMMAND_CROSS_SCREEN_ENABLE,
            checked_menu_flags(settings.enabled),
        )?;
        menu.append(
            localized(language, "暂时锁定在本机", "Temporarily lock to this PC"),
            COMMAND_CROSS_SCREEN_PAUSE,
            enabled_menu_flags(
                settings.enabled
                    && !matches!(
                        state,
                        Some(
                            CrossScreenProductState::Paused
                                | CrossScreenProductState::LockedLocal
                                | CrossScreenProductState::PausedFullscreen
                                | CrossScreenProductState::Stopped
                                | CrossScreenProductState::Failed
                        )
                    ),
            ),
        )?;
        menu.append(
            localized(language, "恢复跨屏控制", "Resume cross-screen control"),
            COMMAND_CROSS_SCREEN_RESUME,
            enabled_menu_flags(matches!(
                state,
                Some(
                    CrossScreenProductState::Paused
                        | CrossScreenProductState::LockedLocal
                        | CrossScreenProductState::Failed
                )
            )),
        )?;
        menu.append(
            localized(language, "立即切回本机", "Return to this PC now"),
            COMMAND_CROSS_SCREEN_RETURN_LOCAL,
            enabled_menu_flags(self.cross_screen.is_some()),
        )?;
        for (chinese, english, command, item) in [
            (
                "布局：对端在左侧",
                "Layout: peer on the left",
                COMMAND_CROSS_SCREEN_LEFT,
                PeerPlacement::Left,
            ),
            (
                "布局：对端在右侧",
                "Layout: peer on the right",
                COMMAND_CROSS_SCREEN_RIGHT,
                PeerPlacement::Right,
            ),
            (
                "布局：对端在上方",
                "Layout: peer above",
                COMMAND_CROSS_SCREEN_ABOVE,
                PeerPlacement::Above,
            ),
            (
                "布局：对端在下方",
                "Layout: peer below",
                COMMAND_CROSS_SCREEN_BELOW,
                PeerPlacement::Below,
            ),
        ] {
            menu.append(
                localized(language, chinese, english),
                command,
                placement_menu_flags(placement, item),
            )?;
        }
        menu.append(
            localized(language, "屏幕四角防误触", "Screen-corner guard"),
            COMMAND_CROSS_SCREEN_CORNER_GUARD,
            checked_menu_flags(settings.corner_guard_enabled),
        )?;
        let corner_range = if language == UiLanguage::English {
            format!(
                "Corner guard: {} pixels (select to adjust)",
                settings.corner_guard_pixels
            )
        } else {
            format!(
                "四角保护范围：{} 像素（点击拖动设置）",
                settings.corner_guard_pixels
            )
        };
        menu.append(&corner_range, COMMAND_CROSS_SCREEN_CORNER_SIZE, MF_STRING)?;
        menu.append(
            localized(language, "跨越前需要二次顶边", "Require a second edge push"),
            COMMAND_CROSS_SCREEN_SECOND_PUSH,
            checked_menu_flags(settings.require_second_push),
        )?;
        menu.append(
            localized(
                language,
                "全屏应用运行时暂停跨屏控制",
                "Pause while a full-screen app is running",
            ),
            COMMAND_CROSS_SCREEN_FULLSCREEN_PAUSE,
            checked_menu_flags(settings.pause_on_fullscreen),
        )?;
        menu.append(
            localized(language, "以管理员身份重启", "Restart as administrator"),
            COMMAND_CROSS_SCREEN_ADMIN_RESTART,
            MF_STRING,
        )?;
        menu.append(
            localized(
                language,
                "跨屏控制设置与兼容性说明…",
                "Cross-screen settings and compatibility…",
            ),
            COMMAND_CROSS_SCREEN_SETTINGS,
            MF_STRING,
        )?;
        menu.separator()
    }

    #[allow(clippy::too_many_lines)]
    fn show_menu(&mut self, window: HWND) -> io::Result<()> {
        let language = self.ui_language;
        let menu = PopupMenu::new()?;
        menu.append(
            localized(language, "Pierdesk · 后台运行", "Pierdesk · Running"),
            0,
            MF_STRING | MF_DISABLED,
        )?;
        menu.separator()?;
        menu.append(
            localized(language, "查看状态", "View status"),
            COMMAND_STATUS,
            MF_STRING,
        )?;
        menu.append(
            localized(language, "配对新设备…", "Pair new device…"),
            COMMAND_PAIR,
            MF_STRING,
        )?;
        menu.append(
            localized(language, "管理已配对设备…", "Manage paired devices…"),
            COMMAND_MANAGE_PEERS,
            MF_STRING,
        )?;
        menu.append(
            localized(language, "高级连接设置…", "Advanced connection settings…"),
            COMMAND_SETTINGS,
            MF_STRING,
        )?;
        menu.append(
            &format!(
                "{} ({})",
                localized(language, "截图…", "Screenshot…"),
                self.screenshot.shortcut
            ),
            COMMAND_SCREENSHOT,
            MF_STRING,
        )?;
        menu.separator()?;
        self.append_cross_screen_menu(&menu, language)?;
        let shared = SharedClipboardSettings::load_or_default(&self.store)?;
        menu.append(
            localized(language, "共享剪贴板", "Shared clipboard"),
            0,
            MF_STRING | MF_DISABLED,
        )?;
        menu.append(
            localized(language, "开启共享剪贴板", "Enable shared clipboard"),
            COMMAND_SHARED_CLIPBOARD_ENABLE,
            checked_menu_flags(shared.enabled),
        )?;
        menu.separator()?;
        let product = self.runtime.as_ref().map(ProductRuntime::snapshot);
        let pending = product.as_ref().is_some_and(|value| value.pending_offer);
        let receiving = product
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .transfer_tasks
                    .iter()
                    .filter(|task| task.id.direction() == ProductTransferDirection::Receiving)
                    .map(|task| task.transfer.state)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        menu.append(
            localized(
                language,
                "接收待确认的文件剪贴板",
                "Accept pending file clipboard",
            ),
            COMMAND_ACCEPT_PENDING,
            enabled_menu_flags(pending),
        )?;
        menu.append(
            localized(language, "显示传输窗口", "Show transfer center"),
            COMMAND_TRANSFER_WINDOW,
            enabled_menu_flags(product.is_some()),
        )?;
        menu.append(
            localized(
                language,
                "收到新传输时自动弹出窗口",
                "Open automatically for new transfers",
            ),
            COMMAND_TRANSFER_WINDOW_AUTO_POPUP,
            checked_menu_flags(self.transfer_window_settings.auto_popup),
        )?;
        menu.append(
            localized(language, "全部暂停接收", "Pause all receives"),
            COMMAND_PAUSE,
            enabled_menu_flags(receiving.iter().any(|state| {
                matches!(
                    state,
                    ProductTransferState::Running | ProductTransferState::AwaitingPaste
                )
            })),
        )?;
        menu.append(
            localized(language, "全部继续接收", "Resume all receives"),
            COMMAND_RESUME,
            enabled_menu_flags(receiving.contains(&ProductTransferState::Paused)),
        )?;
        menu.append(
            localized(language, "全部取消接收…", "Cancel all receives…"),
            COMMAND_CANCEL,
            enabled_menu_flags(receiving.iter().any(|state| {
                matches!(
                    state,
                    ProductTransferState::AwaitingPaste
                        | ProductTransferState::Running
                        | ProductTransferState::Paused
                )
            })),
        )?;
        menu.separator()?;
        let autostart_flags = if query_autostart()? == AutostartState::Enabled {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        menu.append(
            localized(language, "随 Windows 启动", "Start with Windows"),
            COMMAND_AUTOSTART,
            autostart_flags,
        )?;
        menu.append(
            localized(language, "打开诊断日志", "Open diagnostic log"),
            COMMAND_LOG,
            MF_STRING,
        )?;
        menu.append(
            localized(language, "关于 Pierdesk…", "About Pierdesk…"),
            COMMAND_ABOUT,
            MF_STRING,
        )?;
        menu.separator()?;
        menu.append(
            localized(language, "退出 Pierdesk", "Quit Pierdesk"),
            COMMAND_EXIT,
            MF_STRING,
        )?;

        let mut point = POINT::default();
        // SAFETY: point is a live writable structure.
        unsafe { GetCursorPos(&raw mut point) }.map_err(windows_error)?;
        // SAFETY: the hidden window owns the popup-menu interaction.
        let _ = unsafe { SetForegroundWindow(window) };
        // SAFETY: menu, owner window and cursor coordinates remain valid during the call.
        let selected = unsafe {
            TrackPopupMenu(
                menu.handle,
                TPM_RIGHTBUTTON | TPM_RETURNCMD,
                point.x,
                point.y,
                None,
                window,
                None,
            )
        };
        let command = usize::try_from(selected.0).unwrap_or_default();
        if command != 0 {
            self.handle_command(window, command);
        }
        Ok(())
    }
}

fn fresh_ssh_request_notifications(
    notified: &mut HashSet<String>,
    pending: &[(String, String)],
) -> Vec<(String, String)> {
    let live = pending
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    notified.retain(|id| live.contains(id));
    pending
        .iter()
        .filter(|(id, _)| notified.insert(id.clone()))
        .cloned()
        .collect()
}

fn start_product_runtime(store: &DeviceStore) -> (Option<ProductRuntime>, Option<String>) {
    let settings = match AppSettings::load(store) {
        Ok(settings) => settings,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (None, None),
        Err(error) => return (None, Some(format!("连接设置不可用：{error}"))),
    };
    if let Err(error) = validate_saved_network_path(&settings) {
        return (None, Some(format!("网络路径暂不可用：{error}")));
    }
    match ProductRuntime::start(store.clone(), settings) {
        Ok(runtime) => (Some(runtime), None),
        Err(error) => (None, Some(format!("后台服务启动失败：{error}"))),
    }
}

/// One row per applied peer for the cross-screen device-connection card. The switch state is the
/// mirrored pair-switch record; "online" means the peer is currently discovered on the LAN.
fn pair_switch_rows(
    store: &DeviceStore,
    applied: Option<&crate::cross_screen::AppliedBoundaryLayoutV3>,
    online: &HashSet<crate::security::CertificateFingerprint>,
) -> serde_json::Value {
    let Some(applied) = applied else {
        return serde_json::Value::Array(Vec::new());
    };
    // A corrupt switch file hides the rows instead of showing every pair as enabled; the runtime
    // reports the same file error on its own status line.
    let Ok(switches) = crate::cross_screen::pair_switch::PairSwitches::load_or_default(store)
    else {
        return serde_json::Value::Array(Vec::new());
    };
    let mut seen = std::collections::BTreeSet::new();
    let rows = applied
        .layout
        .boundaries
        .iter()
        .filter_map(|boundary| {
            let peer =
                crate::security::CertificateFingerprint::from_str(&boundary.peer_fingerprint)
                    .ok()?;
            if !seen.insert(peer.bytes()) {
                return None;
            }
            let record = switches.record(peer.bytes());
            let label = store
                .load_peer(peer)
                .map_or_else(|_| peer.to_string(), |trusted| trusted.label);
            Some(serde_json::json!({
                "fingerprint": peer.to_string(),
                "label": label,
                "online": online.contains(&peer),
                "enabled": record.enabled,
                "revision": record.revision,
            }))
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(rows)
}

fn start_cross_screen_runtime(
    store: &DeviceStore,
    discovery: Option<crate::discovery::DiscoveryView>,
) -> (Option<CrossScreenProductRuntime>, Option<String>) {
    let settings = match CrossScreenSettings::load_or_default(store) {
        Ok(settings) if settings.enabled => settings,
        Ok(_) => return (None, None),
        Err(error) => return (None, Some(format!("跨屏控制设置不可用：{error}"))),
    };
    let app_settings = match AppSettings::load(store) {
        Ok(settings) => settings,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (None, None),
        Err(error) => {
            return (
                None,
                Some(format!("跨屏控制无法读取已配对设备设置：{error}")),
            );
        }
    };
    if let Err(error) = validate_saved_network_path(&app_settings) {
        return (None, Some(format!("跨屏控制网络路径暂不可用：{error}")));
    }
    let precise = BoundaryWorkspaceV3::load(store).is_ok_and(|workspace| {
        workspace
            .applied
            .as_ref()
            .is_some_and(|applied| !applied.layout.boundaries.is_empty())
    });
    let result = if precise {
        discovery.map_or_else(
            || {
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "多设备跨屏控制正在等待局域网自动发现服务",
                ))
            },
            |discovery| {
                CrossScreenProductRuntime::start_precise(store, &app_settings, &settings, discovery)
            },
        )
    } else {
        CrossScreenProductRuntime::start(store.clone(), &app_settings, settings)
    };
    match result {
        Ok(runtime) => (Some(runtime), None),
        Err(error) => (None, Some(format!("跨屏控制启动失败：{error}"))),
    }
}

fn enabled_menu_flags(enabled: bool) -> MENU_ITEM_FLAGS {
    if enabled {
        MF_STRING
    } else {
        MF_STRING | MF_DISABLED
    }
}

fn checked_menu_flags(checked: bool) -> MENU_ITEM_FLAGS {
    if checked {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    }
}

fn placement_menu_flags(current: PeerPlacement, item: PeerPlacement) -> MENU_ITEM_FLAGS {
    checked_menu_flags(current == item)
}

const fn enabled_text(enabled: bool) -> &'static str {
    if enabled { "开启" } else { "关闭" }
}

fn shared_clipboard_categories(settings: SharedClipboardSettings) -> String {
    if settings.enabled {
        "纯文字、网页/富文本、截图/图片".to_owned()
    } else {
        "无".to_owned()
    }
}

fn settings_task_key(id: ProductTransferId) -> String {
    let prefix = match id.direction() {
        ProductTransferDirection::Sending => 's',
        ProductTransferDirection::Receiving => 'r',
    };
    let bytes = id.offer_id();
    let encoded = bytes
        .iter()
        .fold(String::with_capacity(32), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        });
    format!("{prefix}:{encoded}")
}

const fn settings_transfer_state_code(state: ProductTransferState) -> &'static str {
    match state {
        ProductTransferState::AwaitingPaste => "awaiting",
        ProductTransferState::Running => "running",
        ProductTransferState::Paused => "paused",
        ProductTransferState::Completed => "completed",
        ProductTransferState::Cancelled => "cancelled",
        ProductTransferState::Failed => "failed",
    }
}

const fn placement_key(placement: PeerPlacement) -> &'static str {
    match placement {
        PeerPlacement::Left => "left",
        PeerPlacement::Right => "right",
        PeerPlacement::Above => "above",
        PeerPlacement::Below => "below",
    }
}

fn parse_settings_placement(value: &str) -> io::Result<PeerPlacement> {
    match value {
        "left" => Ok(PeerPlacement::Left),
        "right" => Ok(PeerPlacement::Right),
        "above" => Ok(PeerPlacement::Above),
        "below" => Ok(PeerPlacement::Below),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "未知的屏幕布局",
        )),
    }
}

const fn placement_text(placement: PeerPlacement) -> &'static str {
    match placement {
        PeerPlacement::Left => "左侧",
        PeerPlacement::Right => "右侧",
        PeerPlacement::Above => "上方",
        PeerPlacement::Below => "下方",
    }
}

const fn cross_screen_state_text(state: CrossScreenProductState) -> &'static str {
    match state {
        CrossScreenProductState::Connecting => "正在连接",
        CrossScreenProductState::Connected => "已连接",
        CrossScreenProductState::ControllingPeer => "正在控制对端",
        CrossScreenProductState::ControlledByPeer => "正由对端控制",
        CrossScreenProductState::Reconnecting => "正在重连",
        CrossScreenProductState::Paused => "已暂停",
        CrossScreenProductState::PausedFullscreen => "全屏应用暂停中",
        CrossScreenProductState::LockedLocal => "已锁定本机",
        CrossScreenProductState::Stopped => "已停止",
        CrossScreenProductState::Failed => "失败",
    }
}

fn cross_screen_tooltip(snapshot: &CrossScreenProductSnapshot) -> String {
    if let Some(error) = snapshot.last_error.as_deref() {
        return format!(
            "Pierdesk · 跨屏{}\r\n{}",
            cross_screen_state_text(snapshot.state),
            truncate_text(error, 88)
        );
    }
    let latency = snapshot.round_trip_micros.map_or_else(
        || "延迟等待中".to_owned(),
        |micros| {
            let tenths = micros / 100;
            format!("延迟 {}.{} ms", tenths / 10, tenths % 10)
        },
    );
    format!(
        "Pierdesk · 跨屏{}\r\n{} · 对端在{}\r\n{}",
        cross_screen_state_text(snapshot.state),
        truncate_text(&snapshot.peer_label, 32),
        placement_text(snapshot.peer_placement),
        latency,
    )
}

fn localized_placement(placement: PeerPlacement, language: UiLanguage) -> &'static str {
    if language != UiLanguage::English {
        return placement_text(placement);
    }
    match placement {
        PeerPlacement::Left => "on the left",
        PeerPlacement::Right => "on the right",
        PeerPlacement::Above => "above",
        PeerPlacement::Below => "below",
    }
}

fn localized_cross_state(state: CrossScreenProductState, language: UiLanguage) -> &'static str {
    if language != UiLanguage::English {
        return cross_screen_state_text(state);
    }
    match state {
        CrossScreenProductState::Connecting => "Connecting",
        CrossScreenProductState::Connected => "Connected",
        CrossScreenProductState::ControllingPeer => "Controlling peer",
        CrossScreenProductState::ControlledByPeer => "Controlled by peer",
        CrossScreenProductState::Reconnecting => "Reconnecting",
        CrossScreenProductState::Paused => "Paused",
        CrossScreenProductState::PausedFullscreen => "Paused for full-screen app",
        CrossScreenProductState::LockedLocal => "Locked to this PC",
        CrossScreenProductState::Stopped => "Stopped",
        CrossScreenProductState::Failed => "Failed",
    }
}

fn localized_transfer_state(state: ProductTransferState, language: UiLanguage) -> &'static str {
    if language != UiLanguage::English {
        return state.label();
    }
    match state {
        ProductTransferState::AwaitingPaste => "Awaiting paste",
        ProductTransferState::Running => "Transferring",
        ProductTransferState::Paused => "Paused",
        ProductTransferState::Cancelled => "Cancelled",
        ProductTransferState::Completed => "Completed",
        ProductTransferState::Failed => "Failed",
    }
}

fn localized_direction(direction: &str, language: UiLanguage) -> &str {
    if language != UiLanguage::English {
        return direction;
    }
    match direction {
        "发送" => "Sending",
        "接收" => "Receiving",
        _ => direction,
    }
}

fn localized_cross_screen_tooltip(
    snapshot: &CrossScreenProductSnapshot,
    language: UiLanguage,
) -> String {
    if language != UiLanguage::English {
        return cross_screen_tooltip(snapshot);
    }
    if let Some(error) = snapshot.last_error.as_deref() {
        return format!(
            "Pierdesk · Cross-screen {}\r\n{}",
            localized_cross_state(snapshot.state, language),
            truncate_text(&localized_error_detail(error, language), 88)
        );
    }
    let latency = snapshot.round_trip_micros.map_or_else(
        || "Waiting for latency".to_owned(),
        |micros| {
            let tenths = micros / 100;
            format!("Latency {}.{} ms", tenths / 10, tenths % 10)
        },
    );
    format!(
        "Pierdesk · Cross-screen {}\r\n{} · peer {}\r\n{}",
        localized_cross_state(snapshot.state, language),
        truncate_text(&snapshot.peer_label, 32),
        localized_placement(snapshot.peer_placement, language),
        latency,
    )
}

fn cross_screen_log_status(snapshot: &CrossScreenProductSnapshot) -> String {
    format!(
        "state={} role={} placement={} session={} epoch={} enter_ack_us={} stop={} return_reason={} return_seq={} transition={} transition_seq={} native_queue_peak={} native_motion_dropped={} native_recovery={} native_recovery_seq={} native_contention_returns={} native_overflow_returns={} target_raw_filtered={} target_raw_after_grace={} target_raw_without_injection={} target_raw_after_grace_max_us={} target_takeover_returns={} native_inject_slow={} native_inject_max_us={} injection_queue_peak={} injection_moves_coalesced={} injection_moves_dropped={} injection_wheels_dropped={} injection_wheels_failed={} source_cursor_visible={} source_cursor_visible_after_repair={} source_cursor_probe_failures={} error={}",
        snapshot.state.label(),
        snapshot.role.label(),
        snapshot.peer_placement.label(),
        snapshot.session_id.as_deref().unwrap_or("none"),
        snapshot.channel_epoch.unwrap_or(0),
        snapshot.last_enter_latency_micros.unwrap_or(0),
        snapshot
            .last_stop_reason
            .map_or("none", |reason| reason.label()),
        snapshot.last_control_return_reason.unwrap_or("none"),
        snapshot.control_return_sequence,
        snapshot.last_transition_trace.as_deref().unwrap_or("none"),
        snapshot.transition_trace_sequence,
        snapshot.native_input.peak_queued_events,
        snapshot.native_input.dropped_motion_events,
        snapshot
            .native_input
            .last_recovery
            .map_or("none", |reason| reason.label()),
        snapshot.native_input.recovery_returns,
        snapshot.native_input.contention_returns,
        snapshot.native_input.overflow_returns,
        snapshot.native_input.target_raw_echo_filtered,
        snapshot.native_input.target_raw_after_grace,
        snapshot.native_input.target_raw_without_injection,
        snapshot.native_input.max_target_raw_after_grace_micros,
        snapshot.native_input.target_takeover_returns,
        snapshot.native_input.slow_injection_commands,
        snapshot.native_input.max_injection_command_micros,
        snapshot.native_input.injection_queue_peak,
        snapshot.native_input.coalesced_injection_moves,
        snapshot.native_input.dropped_injection_moves,
        snapshot.native_input.dropped_injection_wheels,
        snapshot.native_input.failed_injection_wheels,
        snapshot.native_input.source_cursor_visible_samples,
        snapshot.native_input.source_cursor_visible_after_repair,
        snapshot.native_input.source_cursor_probe_failures,
        snapshot.last_error.as_deref().unwrap_or("none"),
    )
}

fn current_boundary_constraints(
    settings: &CrossScreenSettings,
) -> io::Result<BoundaryMapConstraints> {
    Ok(BoundaryMapConstraints {
        screen: current_physical_primary_screen_size()?,
        corner_guard_enabled: settings.corner_guard_enabled,
        corner_guard_pixels: settings.corner_guard_pixels,
        minimum_segment_pixels: DEFAULT_MINIMUM_SEGMENT_PIXELS,
    })
}

const fn next_layout_revision(previous: u64) -> u64 {
    previous.saturating_add(1)
}

fn has_transfer(snapshot: &ProductSnapshot) -> bool {
    !snapshot.transfer_tasks.is_empty()
}

const fn terminal_transfer(state: ProductTransferState) -> bool {
    matches!(
        state,
        ProductTransferState::Completed
            | ProductTransferState::Cancelled
            | ProductTransferState::Failed
    )
}

const fn should_notify_transfer_completion(
    auto_popup: bool,
    started_files: usize,
    state: ProductTransferState,
    first_terminal: bool,
) -> bool {
    !auto_popup && started_files != 0 && terminal_transfer(state) && first_terminal
}

const fn should_notify_transfer_start(started_files: usize, state: ProductTransferState) -> bool {
    // AwaitingPaste is an offer, not a transfer; a pasted or dropped offer becomes Running.
    started_files != 0 && matches!(state, ProductTransferState::Running)
}

fn transfer_start_notification(
    direction: ProductTransferDirection,
    primary_name: &str,
    total_size: u64,
    peer_label: &str,
    language: UiLanguage,
) -> TransferCompletionNotification {
    let title = if language == UiLanguage::English {
        match direction {
            ProductTransferDirection::Sending => "Pierdesk · Sending file",
            ProductTransferDirection::Receiving => "Pierdesk · Receiving file",
        }
        .to_owned()
    } else {
        format!("Pierdesk · 开始{}文件", direction.label())
    };
    TransferCompletionNotification {
        title,
        body: format!(
            "{primary_name} · {} · {peer_label}",
            format_bytes(total_size)
        ),
        warning: false,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct TransferCompletionNotification {
    title: String,
    body: String,
    warning: bool,
}

fn transfer_completion_notification(
    direction: ProductTransferDirection,
    state: ProductTransferState,
    primary_name: &str,
    total_size: u64,
    peer_label: &str,
    language: UiLanguage,
) -> Option<TransferCompletionNotification> {
    let outcome = match state {
        ProductTransferState::Completed => "完成",
        ProductTransferState::Cancelled => "已取消",
        ProductTransferState::Failed => "失败",
        ProductTransferState::AwaitingPaste
        | ProductTransferState::Running
        | ProductTransferState::Paused => return None,
    };
    let title = if language == UiLanguage::English {
        let operation = match (direction, state) {
            (ProductTransferDirection::Sending, ProductTransferState::Completed) => "File sent",
            (ProductTransferDirection::Receiving, ProductTransferState::Completed) => {
                "File received"
            }
            (ProductTransferDirection::Sending, ProductTransferState::Cancelled) => {
                "File send cancelled"
            }
            (ProductTransferDirection::Receiving, ProductTransferState::Cancelled) => {
                "File receive cancelled"
            }
            (ProductTransferDirection::Sending, ProductTransferState::Failed) => "File send failed",
            (ProductTransferDirection::Receiving, ProductTransferState::Failed) => {
                "File receive failed"
            }
            _ => return None,
        };
        format!("Pierdesk · {operation}")
    } else {
        format!("Pierdesk · 文件{}{outcome}", direction.label())
    };
    Some(TransferCompletionNotification {
        title,
        body: format!(
            "{primary_name} · {} · {peer_label}",
            format_bytes(total_size)
        ),
        warning: state == ProductTransferState::Failed,
    })
}

fn transfer_percentage_tenths(transferred: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    transferred
        .min(total)
        .saturating_mul(1000)
        .checked_div(total)
        .unwrap_or_default()
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut characters = text.chars();
    let prefix = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn localized<'a>(language: UiLanguage, chinese: &'a str, english: &'a str) -> &'a str {
    if language == UiLanguage::English {
        english
    } else {
        chinese
    }
}

fn contains_han(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
        )
    })
}

fn localized_error_detail(error: &str, language: UiLanguage) -> String {
    if language != UiLanguage::English || !contains_han(error) {
        return error.to_owned();
    }
    "An internal operation failed. Open the diagnostic log for the complete error.".to_owned()
}

fn localized_operation_error(
    language: UiLanguage,
    chinese_summary: &str,
    english_summary: &str,
    error: &str,
) -> String {
    if language != UiLanguage::English {
        return format!("{chinese_summary}：{error}");
    }
    if contains_han(error) {
        format!("{english_summary}. Open the diagnostic log for details.")
    } else {
        format!("{english_summary}: {error}")
    }
}

fn localized_network_issue(issue: &NetworkPathIssue, language: UiLanguage) -> String {
    if language != UiLanguage::English {
        return issue.user_message();
    }
    format!(
        "The peer address {} is also assigned to a local network adapter. Pierdesk preserved the existing settings and paused this conflicting path. Turn off the conflicting hotspot or virtual adapter, or connect both computers to the same LAN. The path resumes automatically after the conflict is resolved.",
        issue.peer_endpoint.ip()
    )
}

const fn boundary_workspace_v3_runtime_ready() -> bool {
    true
}

const fn boundary_multi_peer_runtime_ready() -> bool {
    true
}

const fn settings_command_success_toast(command: &SettingsWindowCommand) -> Option<&'static str> {
    match command {
        #[cfg(feature = "licensing")]
        SettingsWindowCommand::LicenseActivate(_)
        | SettingsWindowCommand::LicenseRefresh
        | SettingsWindowCommand::LicenseRelease => None,
        SettingsWindowCommand::Ssh(_)
        | SettingsWindowCommand::PreviewBoundaryWorkspaceV3 { .. } => None,
        SettingsWindowCommand::Refresh => Some("状态已刷新"),
        SettingsWindowCommand::SaveBoundaryWorkspaceV3 { .. } => Some("人工边界草稿已保存"),
        SettingsWindowCommand::AcceptBoundaryWorkspacePreview { .. } => {
            Some("已采用并确认对端发送的布局")
        }
        SettingsWindowCommand::ApplyBoundaryWorkspaceV3 { .. } => {
            Some("P2.5 边界快照已通过校验并保存")
        }
        _ => Some("设置已应用"),
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_scaled_bytes(bytes, GIB, 2, "GiB")
    } else if bytes >= MIB {
        format_scaled_bytes(bytes, MIB, 1, "MiB")
    } else if bytes >= KIB {
        format_scaled_bytes(bytes, KIB, 1, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_scaled_bytes(bytes: u64, unit: u64, decimals: u32, suffix: &str) -> String {
    let factor = 10_u64.pow(decimals);
    let fraction = u128::from(bytes % unit) * u128::from(factor) / u128::from(unit);
    format!(
        "{}.{:0width$} {suffix}",
        bytes / unit,
        fraction,
        width = decimals as usize
    )
}

struct PopupMenu {
    handle: windows::Win32::UI::WindowsAndMessaging::HMENU,
}

impl PopupMenu {
    fn new() -> io::Result<Self> {
        // SAFETY: no parameters and ownership is transferred to the returned guard.
        let handle = unsafe { CreatePopupMenu() }.map_err(windows_error)?;
        Ok(Self { handle })
    }

    fn append(&self, label: &str, command: usize, flags: MENU_ITEM_FLAGS) -> io::Result<()> {
        let label = wide_null(label.as_ref());
        // SAFETY: the menu is live and the null-terminated label remains valid during the call.
        unsafe { AppendMenuW(self.handle, flags, command, PCWSTR(label.as_ptr())) }
            .map_err(windows_error)
    }

    fn separator(&self) -> io::Result<()> {
        // SAFETY: separators ignore the identifier and label.
        unsafe { AppendMenuW(self.handle, MF_SEPARATOR, 0, PCWSTR::null()) }.map_err(windows_error)
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // SAFETY: the guard uniquely owns the popup menu.
        let _ = unsafe { DestroyMenu(self.handle) };
    }
}

struct TrayWindow {
    handle: HWND,
    instance: HINSTANCE,
    state: Box<TrayState>,
    class_registered: bool,
}

impl TrayWindow {
    fn create() -> io::Result<Self> {
        let mut state = Box::new(TrayState::new().map_err(|e| stage_error("tray_state", e))?);
        // SAFETY: retrieving the current module does not transfer ownership.
        let module = unsafe { GetModuleHandleW(None) }.map_err(windows_error)?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_window_procedure),
            hInstance: instance,
            hIcon: state.icon.handle,
            lpszClassName: TRAY_WINDOW_CLASS,
            ..Default::default()
        };
        // SAFETY: the class structure and constant class name are valid.
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            return Err(stage_error("window", io::Error::last_os_error()));
        }
        let state_pointer = (&raw mut *state).cast::<c_void>();
        // SAFETY: the boxed state has a stable address until after the window is destroyed.
        let handle = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                TRAY_WINDOW_CLASS,
                w!("Pierdesk"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                Some(state_pointer.cast_const()),
            )
        }
        .map_err(|error| {
            // SAFETY: registration succeeded in this function.
            let _ = unsafe { UnregisterClassW(TRAY_WINDOW_CLASS, Some(instance)) };
            stage_error("window", windows_error(error))
        })?;
        match state.add_icon(handle) {
            Ok(()) => {}
            Err(error) => {
                // Explorer's notification area may not exist yet at logon; keep running and retry.
                let _ = append_log(
                    &state.log_path,
                    &format!(
                        "tray_icon_add_deferred error={error} uptime_ms={}",
                        uptime_ms()
                    ),
                );
                let now = Instant::now();
                state.tray_icon_deferred_at = Some(now);
                state.last_icon_health_check =
                    now.checked_sub(TRAY_ICON_HEALTH_INTERVAL).unwrap_or(now);
            }
        }
        state.screenshot.bind(handle);
        // SAFETY: the hidden tray window is live and receives periodic pointer-free WM_TIMER.
        if unsafe {
            SetTimer(
                Some(handle),
                TRAY_REFRESH_TIMER_ID,
                TRAY_REFRESH_TIMER_MS,
                None,
            )
        } == 0
        {
            if state.tray_icon_added() {
                state.delete_icon(handle);
            }
            // SAFETY: handle and class are owned by this partially constructed window.
            let _ = unsafe { DestroyWindow(handle) };
            let _ = unsafe { UnregisterClassW(TRAY_WINDOW_CLASS, Some(instance)) };
            return Err(stage_error("timer", io::Error::last_os_error()));
        }
        Ok(Self {
            handle,
            instance,
            state,
            class_registered: true,
        })
    }

    fn run_loop(&self) -> io::Result<()> {
        debug_assert!(!self.handle.0.is_null());
        loop {
            let mut message = MSG::default();
            // SAFETY: message is writable and the thread owns this message loop.
            let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Ok(());
            }
            if self
                .state
                .transfer_window
                .as_ref()
                .is_some_and(|window| window.is_dialog_message(&message))
                || self
                    .state
                    .settings_window
                    .as_ref()
                    .is_some_and(|window| window.is_dialog_message(&message))
            {
                continue;
            }
            // SAFETY: GetMessageW initialized the message.
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }
}

impl Drop for TrayWindow {
    fn drop(&mut self) {
        self.state.shutdown();
        self.state.screenshot.unbind(self.handle);
        // SAFETY: balances the fixed timer created for this live hidden window.
        let _ = unsafe { KillTimer(Some(self.handle), TRAY_REFRESH_TIMER_ID) };
        self.state.delete_icon(self.handle);
        // SAFETY: the window was created by this guard and state stays live through destruction.
        let _ = unsafe { DestroyWindow(self.handle) };
        if self.class_registered {
            // SAFETY: this guard registered the class and no window remains after DestroyWindow.
            let _ = unsafe { UnregisterClassW(TRAY_WINDOW_CLASS, Some(self.instance)) };
            self.class_registered = false;
        }
        let _ = append_log(&self.state.log_path, "tray_stop clean=true");
    }
}

#[allow(clippy::too_many_lines)]
unsafe extern "system" fn tray_window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            // SAFETY: WM_NCCREATE guarantees lParam points to CREATESTRUCTW, whose lpCreateParams
            // is the stable TrayState pointer passed to CreateWindowExW.
            let state = unsafe { (*create).lpCreateParams } as isize;
            // SAFETY: window creation is in progress and GWLP_USERDATA stores the pointer value.
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, state) };
        }
    }
    // SAFETY: reading the application-owned pointer does not dereference it yet.
    let state_pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut TrayState;
    if !state_pointer.is_null() {
        // SAFETY: the pointer belongs to the boxed TrayState and outlives the window.
        let state = unsafe { &mut *state_pointer };
        if message == state.taskbar_created {
            state.last_tooltip.clear();
            match state.add_icon(window) {
                Ok(()) => {
                    state.last_icon_health_check = Instant::now();
                    state.tray_icon_startup_retry_pending = false;
                    state.tray_icon_deferred_at = None;
                    let _ = append_log(
                        &state.log_path,
                        "tray_icon_recovered reason=taskbar_created",
                    );
                }
                Err(error) => {
                    let _ =
                        append_log(&state.log_path, &format!("tray_icon_restore error={error}"));
                }
            }
            return LRESULT(0);
        }
        if message == TRAY_CALLBACK_MESSAGE {
            let event = u32::try_from(lparam.0).unwrap_or_default();
            match event {
                WM_RBUTTONUP => {
                    if let Err(error) = state.show_menu(window) {
                        show_error(
                            window,
                            &localized_operation_error(
                                state.ui_language,
                                "无法打开托盘菜单",
                                "Could not open the tray menu",
                                &error.to_string(),
                            ),
                        );
                    }
                }
                WM_LBUTTONDBLCLK => {
                    if let Err(error) = state.show_settings(window, "status") {
                        let _ = append_log(
                            &state.log_path,
                            &format!("settings_window_open error={error} fallback=native_status"),
                        );
                        if let Err(fallback) = state.show_status(window) {
                            show_error(
                                window,
                                &localized_operation_error(
                                    state.ui_language,
                                    "无法读取状态",
                                    "Could not read the status",
                                    &fallback.to_string(),
                                ),
                            );
                        }
                    }
                }
                NIN_BALLOONUSERCLICK => {
                    if let Err(error) = state.show_settings(window, "remote") {
                        let _ = append_log(
                            &state.log_path,
                            &format!("ssh_notification_open error={error}"),
                        );
                    }
                }
                _ => {}
            }
            return LRESULT(0);
        }
        if message == TRAY_SHOW_STATUS_MESSAGE {
            if let Err(error) = state.show_settings(window, "status") {
                let _ = append_log(
                    &state.log_path,
                    &format!("settings_window_open error={error} fallback=native_status"),
                );
                if let Err(fallback) = state.show_status(window) {
                    show_error(
                        window,
                        &localized_operation_error(
                            state.ui_language,
                            "无法读取状态",
                            "Could not read the status",
                            &fallback.to_string(),
                        ),
                    );
                }
            }
            return LRESULT(0);
        }
        if message == TRAY_EXIT_MESSAGE {
            let _ = append_log(&state.log_path, "tray_exit requested=external");
            // SAFETY: this ends the message loop owned by the tray thread.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        if message == TRAY_RELOAD_MESSAGE {
            state.reload_runtime();
            return LRESULT(0);
        }
        if message == TRAY_RUNTIME_ERROR_MESSAGE {
            state.show_runtime_error(window);
            return LRESULT(0);
        }
        if message == TRAY_QUERY_ELEVATED_MESSAGE {
            return LRESULT(is_process_elevated().into());
        }
        if message == WM_TIMER && wparam.0 == TRAY_REFRESH_TIMER_ID {
            state.refresh_ui(window);
            return LRESULT(0);
        }
        if message == WM_HOTKEY && state.screenshot.matches(wparam.0) {
            state.handle_command(window, COMMAND_SCREENSHOT);
            return LRESULT(0);
        }
        if message == WM_COMMAND {
            state.handle_command(window, wparam.0 & 0xffff);
            return LRESULT(0);
        }
    }
    match message {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION if wparam.0 != 0 => {
            // SAFETY: this requests orderly termination of the current thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: the window is ending, so stop the owning message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // SAFETY: unhandled messages are delegated to the system default procedure.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
    }
}

/// Runs the per-user native tray process.
///
/// # Errors
///
/// Returns a user-facing error when identity initialization, single-instance acquisition,
/// hidden-window setup, tray registration, or the message loop fails.
pub fn run(detach_console: bool) -> Result<(), String> {
    notify_shell_current_executable_updated();
    if detach_console {
        // SAFETY: detaching the inherited console is process-local and expected for tray mode.
        let _ = unsafe { FreeConsole() };
    }
    let result = run_inner();
    if let Err(error) = &result {
        log_start_failure(error);
        show_error(
            HWND::default(),
            &localized_operation_error(
                UiLanguage::English,
                "Pierdesk 无法启动",
                "Pierdesk could not start",
                &error.to_string(),
            ),
        );
    }
    result.map_err(|error| error.to_string())
}

/// Waits for a previous unelevated tray process to finish before taking over as its elevated
/// replacement. This avoids racing the single-instance mutex during `runas` startup.
///
/// # Errors
///
/// Returns an error when the previous process does not finish within 30 seconds or tray startup
/// fails.
pub fn run_after_process(previous_process_id: u32) -> Result<(), String> {
    // SAFETY: detaching the temporary runas console is process-local.
    let _ = unsafe { FreeConsole() };
    // SAFETY: this requests wait-only access to the user-selected predecessor process id.
    if let Ok(process) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, previous_process_id) } {
        // SAFETY: process is a live waitable handle owned by this function.
        let wait = unsafe { WaitForSingleObject(process, 30_000) };
        // SAFETY: process was returned by OpenProcess and is closed exactly once.
        let _ = unsafe { CloseHandle(process) };
        if wait != windows::Win32::Foundation::WAIT_OBJECT_0 {
            return Err("等待原 Pierdesk 进程退出超时".to_owned());
        }
    }
    run(false)
}

fn run_inner() -> io::Result<()> {
    let _single_instance =
        match SingleInstance::acquire().map_err(|e| stage_error("single_instance", e))? {
            SingleInstanceAcquisition::Primary(instance) => instance,
            SingleInstanceAcquisition::Existing(handle) => {
                if is_process_elevated() && !existing_tray_is_elevated() {
                    post_to_existing(TRAY_EXIT_MESSAGE)?;
                    SingleInstance::wait_for_exiting_instance(handle)?
                } else {
                    match post_to_existing(TRAY_SHOW_STATUS_MESSAGE) {
                        Ok(()) => {
                            // SAFETY: CreateMutexW returned a non-owning handle to the live primary mutex.
                            let _ = unsafe { CloseHandle(handle) };
                            return Ok(());
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            // The old process has already left its message loop but may still be releasing
                            // cross-screen and clipboard workers. Wait for its owned mutex instead of
                            // reporting the misleading "tray is not running" startup failure.
                            SingleInstance::wait_for_exiting_instance(handle)?
                        }
                        Err(error) => {
                            // SAFETY: the current process never acquired this existing mutex.
                            let _ = unsafe { CloseHandle(handle) };
                            return Err(error);
                        }
                    }
                }
            }
        };
    let tray = TrayWindow::create()?;
    let root = DeviceStore::current_user().map_err(|e| stage_error("identity", e))?;
    let identity = crate::update_support::record_identity(root.root(), is_process_elevated())
        .map_err(|e| stage_error("identity", e))?;
    let _ = append_log(
        &tray.state.log_path,
        &format!(
            "tray_ready identity_recorded=true icon={}",
            if tray.state.tray_icon_added() {
                "added"
            } else {
                "deferred"
            }
        ),
    );
    let result = tray.run_loop();
    let _ = std::fs::remove_file(identity);
    result
}

/// Asks the already-running tray instance to show its native status dialog.
///
/// # Errors
///
/// Returns an error if no instance window exists or the message cannot be posted.
pub fn show_existing_status() -> Result<(), String> {
    post_to_existing(TRAY_SHOW_STATUS_MESSAGE).map_err(|error| error.to_string())
}

/// Requests an orderly exit from the already-running tray instance.
///
/// # Errors
///
/// Returns an error if no instance window exists or the message cannot be posted.
pub fn exit_existing() -> Result<(), String> {
    post_to_existing(TRAY_EXIT_MESSAGE).map_err(|error| error.to_string())
}

/// Notifies the running tray instance that persisted settings changed.
///
/// # Errors
///
/// Returns an error if no instance exists or the private notification cannot be posted.
pub fn reload_existing() -> Result<(), String> {
    post_to_existing(TRAY_RELOAD_MESSAGE).map_err(|error| error.to_string())
}

/// Asks the tray to surface the current product-runtime error in a recoverable native dialog.
///
/// # Errors
///
/// Returns an error if no tray instance exists or the private notification cannot be posted.
pub fn show_runtime_error_existing() -> Result<(), String> {
    post_to_existing(TRAY_RUNTIME_ERROR_MESSAGE).map_err(|error| error.to_string())
}

fn post_to_existing(message: u32) -> io::Result<()> {
    // SAFETY: the search is restricted to message-only windows with our private class name.
    let window =
        unsafe { FindWindowExW(Some(HWND_MESSAGE), None, TRAY_WINDOW_CLASS, PCWSTR::null()) }
            .map_err(|_| {
                io::Error::new(io::ErrorKind::NotFound, "Pierdesk tray is not running")
            })?;
    // SAFETY: the message contains no pointers and targets the discovered private window.
    unsafe { PostMessageW(Some(window), message, WPARAM(0), LPARAM(0)) }.map_err(windows_error)
}

fn existing_tray_is_elevated() -> bool {
    let Ok(window) =
        (unsafe { FindWindowExW(Some(HWND_MESSAGE), None, TRAY_WINDOW_CLASS, PCWSTR::null()) })
    else {
        return false;
    };
    unsafe {
        SendMessageW(
            window,
            TRAY_QUERY_ELEVATED_MESSAGE,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        )
        .0 != 0
    }
}

/// Layout of `PROCESS_BASIC_INFORMATION` (the windows crate gates it behind an unused feature).
#[repr(C)]
#[derive(Default)]
struct ProcessBasicInfo {
    exit_status: i32,
    peb_base_address: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    // The windows crate only exposes this behind the Wdk feature set.
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        info: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;

#[derive(Debug)]
struct StageError {
    stage: &'static str,
    source: io::Error,
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for StageError {}

fn stage_error(stage: &'static str, source: io::Error) -> io::Error {
    io::Error::new(source.kind(), StageError { stage, source })
}

/// Best-effort: the failure is otherwise only visible in a modal dialog.
fn log_start_failure(error: &io::Error) {
    let stage = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<StageError>())
        .map_or("startup", |inner| inner.stage);
    if let Ok(store) = DeviceStore::current_user() {
        let _ = append_log(
            &store.root().join("logs").join("clipferry.log"),
            &format!("tray_start_failed stage={stage} error={error}"),
        );
    }
}

fn uptime_ms() -> u64 {
    // SAFETY: GetTickCount64 has no preconditions.
    unsafe { GetTickCount64() }
}

/// Image name of the process that launched us (scheduler host, Explorer, updater...).
fn parent_process_name() -> String {
    (|| -> windows_core::Result<String> {
        let mut info = ProcessBasicInfo::default();
        // SAFETY: pseudo-handle plus a correctly sized, writable ProcessBasicInfo.
        let status = unsafe {
            NtQueryInformationProcess(
                GetCurrentProcess(),
                PROCESS_BASIC_INFORMATION_CLASS,
                (&raw mut info).cast(),
                u32::try_from(size_of::<ProcessBasicInfo>()).unwrap_or(0),
                std::ptr::null_mut(),
            )
        };
        if status < 0 {
            return Err(windows_core::Error::from_hresult(windows_core::HRESULT(
                status,
            )));
        }
        let parent_id = u32::try_from(info.inherited_from_unique_process_id).unwrap_or(0);
        // SAFETY: limited query handle is closed below; the buffer length is passed in/out.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, parent_id)?;
            let mut buffer = [0u16; 520];
            let mut length = 520u32;
            let result = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows_core::PWSTR(buffer.as_mut_ptr()),
                &raw mut length,
            );
            let _ = CloseHandle(process);
            result?;
            let path = String::from_utf16_lossy(&buffer[..length as usize]);
            Ok(path.rsplit('\\').next().unwrap_or("unknown").to_owned())
        }
    })()
    .unwrap_or_else(|_| "unknown".to_owned())
}

/// (`autostartLastResult`, `autostartLastRun`) for the settings page; cached like the task state.
fn last_run_summary() -> (String, String) {
    static CACHE: std::sync::Mutex<Option<(Instant, (String, String))>> =
        std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().expect("autostart last-run cache");
    if let Some((at, value)) = &*cache
        && at.elapsed() < Duration::from_secs(10)
    {
        return value.clone();
    }
    let value = match crate::autostart::last_run() {
        Ok(Some(run)) => (
            crate::autostart::result_label(run.result),
            run.time.unwrap_or_default(),
        ),
        Ok(None) => ("never".to_owned(), String::new()),
        Err(_) => ("unknown".to_owned(), String::new()),
    };
    *cache = Some((Instant::now(), value.clone()));
    value
}

#[cfg(debug_assertions)]
fn debug_defer_remaining() -> Option<Duration> {
    // Debug-only simulation of a notification area that is not ready yet.
    static UNTIL: std::sync::OnceLock<Option<Instant>> = std::sync::OnceLock::new();
    let until = (*UNTIL.get_or_init(|| {
        std::env::var("PIERDESK_TEST_DEFER_TRAY_ICON_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|ms| Instant::now() + Duration::from_millis(ms))
    }))?;
    until.checked_duration_since(Instant::now())
}

fn is_process_elevated() -> bool {
    unsafe { IsUserAnAdmin() }.as_bool()
}

/// Returns an externally stable autostart state label.
///
/// # Errors
///
/// Returns an error if the current executable or per-user Run registry value cannot be read.
pub fn autostart_status() -> Result<&'static str, String> {
    query_autostart()
        .map(AutostartState::label)
        .map_err(|error| error.to_string())
}

/// Enables or disables current-user startup, retaining the current privilege mode.
///
/// # Errors
///
/// Returns an error if the current executable cannot be resolved or HKCU cannot be modified.
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    write_autostart(enabled).map_err(|error| error.to_string())
}

fn query_autostart() -> io::Result<AutostartState> {
    // Settings refreshes frequently. Do not make scheduler RPC on every UI tick.
    let mut cache = AUTOSTART_CACHE.lock().expect("autostart query cache");
    if let Some((at, state)) = *cache
        && at.elapsed() < Duration::from_secs(3)
    {
        return Ok(state);
    }
    let state = query_autostart_uncached()?;
    *cache = Some((Instant::now(), state));
    Ok(state)
}

fn query_autostart_uncached() -> io::Result<AutostartState> {
    use crate::autostart::TaskState;
    Ok(match crate::autostart::query(&std::env::current_exe()?)? {
        TaskState::Valid => AutostartState::Enabled,
        TaskState::Disabled => AutostartState::Disabled,
        TaskState::Stale => AutostartState::Stale,
        TaskState::Missing => {
            let legacy = query_legacy_autostart()?;
            if legacy == AutostartState::Enabled && is_process_elevated() {
                AutostartState::Stale
            } else {
                legacy
            }
        }
    })
}

fn should_migrate_autostart(legacy: AutostartState, task: crate::autostart::TaskState) -> bool {
    legacy == AutostartState::Enabled && task == crate::autostart::TaskState::Missing
}

fn legacy_startup_allowed() -> io::Result<bool> {
    let mut bytes = [0_u8; 12];
    let mut length = 12;
    // SAFETY: fixed-size output buffer for the current user's single approval entry.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run"),
            RUN_VALUE,
            windows::Win32::System::Registry::RRF_RT_REG_BINARY,
            None,
            Some(bytes.as_mut_ptr().cast()),
            Some(&raw mut length),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(true);
    }
    check_win32(result)?;
    Ok(matches!(bytes[0], 2 | 6))
}

fn query_legacy_autostart() -> io::Result<AutostartState> {
    let expected = autostart_command()?;
    let mut bytes = 0_u32;
    // SAFETY: querying with no data buffer asks Windows for the required REG_SZ size.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&raw mut bytes),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(AutostartState::Disabled);
    }
    check_win32(status)?;
    if bytes == 0 || bytes > 64 * 1024 || !bytes.is_multiple_of(2) {
        return Ok(AutostartState::Stale);
    }
    let mut data = vec![0_u8; usize::try_from(bytes).map_err(io::Error::other)?];
    // SAFETY: the buffer has exactly the size Windows reported and remains live through the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&raw mut bytes),
        )
    };
    check_win32(status)?;
    let units = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    let actual = String::from_utf16(&units)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "autostart value is not UTF-16"))?;
    Ok(if actual == expected {
        if legacy_startup_allowed()? {
            AutostartState::Enabled
        } else {
            AutostartState::Disabled
        }
    } else {
        AutostartState::Stale
    })
}

fn write_autostart(enabled: bool) -> io::Result<()> {
    *AUTOSTART_CACHE.lock().expect("autostart query cache") = None;
    if enabled && is_process_elevated() {
        // Register and verify before removing the legacy entry, so a failure is not destructive.
        crate::autostart::enable(&std::env::current_exe()?)?;
        write_legacy_autostart(false)
    } else {
        crate::autostart::disable()?;
        write_legacy_autostart(enabled)
    }
}

fn write_legacy_autostart(enabled: bool) -> io::Result<()> {
    if enabled && !legacy_startup_allowed()? {
        return Err(io::Error::other(
            "Windows disabled this startup entry. Enable Pierdesk in Task Manager, or reopen Pierdesk as administrator to configure a logon task.",
        ));
    }
    let mut key = HKEY::default();
    // SAFETY: this opens/creates the current user's standard Run key with no custom security.
    check_win32(unsafe { RegCreateKeyW(HKEY_CURRENT_USER, RUN_KEY, &raw mut key) })?;
    let key = RegistryKey(key);
    if enabled {
        let command = wide_null(autostart_command()?.as_ref());
        // SAFETY: command is valid UTF-16 including its terminating NUL; bytes borrow it only for
        // the registry call.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                command.as_ptr().cast::<u8>(),
                command.len().saturating_mul(size_of::<u16>()),
            )
        };
        // SAFETY: key is live and owned, value name is static, and data is a complete REG_SZ.
        check_win32(unsafe { RegSetValueExW(key.0, RUN_VALUE, None, REG_SZ, Some(bytes)) })
    } else {
        // SAFETY: key is live and deleting a missing value is normalized below.
        let status = unsafe { RegDeleteValueW(key.0, RUN_VALUE) };
        if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            check_win32(status)
        }
    }
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: the guard uniquely owns the registry key returned by RegCreateKeyW.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn autostart_command() -> io::Result<String> {
    let executable = std::env::current_exe()?;
    quote_tray_command(&executable)
}

fn quote_tray_command(executable: &Path) -> io::Result<String> {
    let text = executable.as_os_str().to_string_lossy();
    if text.contains('"') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the executable path contains a quote",
        ));
    }
    Ok(format!("\"{text}\" tray"))
}

fn append_log(path: &Path, message: &str) -> io::Result<()> {
    crate::logging::append(path, message)
}

fn best_icon_image(bytes: &[u8]) -> io::Result<&[u8]> {
    if bytes.len() < 6 || bytes[0..2] != [0, 0] || bytes[2..4] != [1, 0] {
        return Err(invalid_data("invalid ICO header"));
    }
    let count = usize::from(u16::from_le_bytes([bytes[4], bytes[5]]));
    if count == 0 || count > 256 || bytes.len() < 6 + count * 16 {
        return Err(invalid_data("invalid ICO directory"));
    }
    let mut best = None;
    for index in 0..count {
        let entry = 6 + index * 16;
        let width = if bytes[entry] == 0 {
            256_u32
        } else {
            u32::from(bytes[entry])
        };
        let height = if bytes[entry + 1] == 0 {
            256_u32
        } else {
            u32::from(bytes[entry + 1])
        };
        let size = usize::try_from(u32::from_le_bytes([
            bytes[entry + 8],
            bytes[entry + 9],
            bytes[entry + 10],
            bytes[entry + 11],
        ]))
        .map_err(io::Error::other)?;
        let offset = usize::try_from(u32::from_le_bytes([
            bytes[entry + 12],
            bytes[entry + 13],
            bytes[entry + 14],
            bytes[entry + 15],
        ]))
        .map_err(io::Error::other)?;
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid_data("ICO image is out of bounds"))?;
        let score = width.saturating_mul(height);
        if best.is_none_or(|(best_score, _, _)| score > best_score) {
            best = Some((score, offset, end));
        }
    }
    let (_, start, end) = best.ok_or_else(|| invalid_data("ICO has no image"))?;
    Ok(&bytes[start..end])
}

fn copy_wide_to_fixed(text: &str, output: &mut [u16]) {
    output.fill(0);
    let content_length = output.len().saturating_sub(1);
    for (target, value) in output
        .iter_mut()
        .take(content_length)
        .zip(text.encode_utf16())
    {
        *target = value;
    }
}

fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn show_information(window: HWND, message: &str, title: &str) {
    show_message(window, message, title, MB_OK | MB_ICONINFORMATION);
}

fn show_error(window: HWND, message: &str) {
    show_message(window, message, "Pierdesk", MB_OK | MB_ICONERROR);
}

fn ask_yes_no(message: &str, title: &str) -> bool {
    let message = wide_null(message.as_ref());
    let title = wide_null(title.as_ref());
    // SAFETY: both strings are null terminated and remain live for the modal call. The dialog is
    // deliberately unowned so it has an independent taskbar entry and cannot become stranded
    // behind the message-only tray window.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

fn show_message(
    _window: HWND,
    message: &str,
    title: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) {
    let message = wide_null(message.as_ref());
    let title = wide_null(title.as_ref());
    // SAFETY: both strings are null terminated and remain live for the modal call. The tray
    // window is message-only; making it an owner hides the dialog from the taskbar and can make
    // an occluded modal impossible to recover. An unowned foreground dialog gets its own taskbar
    // entry and remains independently closable.
    let _ = unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            style | MB_SETFOREGROUND,
        )
    };
}

fn check_win32(status: windows::Win32::Foundation::WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(
            i32::try_from(status.0).unwrap_or(i32::MAX),
        ))
    }
}

fn windows_error(error: windows_core::Error) -> io::Error {
    io::Error::other(error)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Exercises deterministic tray assets and state formatting without creating UI or mutating HKCU.
///
/// # Errors
///
/// Returns an error if the embedded icon or command-line quoting invariant is invalid.
pub fn self_test() -> Result<(), String> {
    let icon = best_icon_image(ICON_BYTES).map_err(|error| error.to_string())?;
    if icon.len() < 40 {
        return Err("embedded icon image is unexpectedly short".to_owned());
    }
    let quoted = quote_tray_command(Path::new(r"C:\Program Files\Pierdesk\pierdesk.exe"))
        .map_err(|error| error.to_string())?;
    if quoted != r#""C:\Program Files\Pierdesk\pierdesk.exe" tray"# {
        return Err("autostart command quoting is invalid".to_owned());
    }
    if next_layout_revision(100) != 101 {
        return Err("cross-screen layout revision is not monotonic".to_owned());
    }
    let about = crate::about::dialog_text();
    let hidden_ticket = crate::about::ferry_ticket();
    if !about.contains(crate::about::AUTHOR)
        || about.contains(crate::about::FERRY_TICKET_CODE)
        || !hidden_ticket.contains(crate::about::FERRY_TICKET_CODE)
    {
        return Err("about authorship metadata is incomplete".to_owned());
    }
    println!(
        "TRAY_SELFTEST passed=true icon_bytes={} autostart_command={quoted:?} cross_screen_menu=true corner_guard_slider=0..512 about_author={:?} ferry_ticket={}",
        icon.len(),
        crate::about::AUTHOR,
        crate::about::FERRY_TICKET_CODE,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ICON_BYTES, SettingsWindowCommand, SingleInstance, SingleInstanceAcquisition,
        best_icon_image, copy_wide_to_fixed, cross_screen_log_status, cross_screen_tooltip,
        fresh_ssh_request_notifications, localized, localized_error_detail,
        localized_operation_error, quote_tray_command, settings_command_success_toast,
        should_notify_transfer_completion, should_notify_transfer_start,
        transfer_completion_notification, transfer_start_notification,
    };
    use crate::app_settings::UiLanguage;
    use crate::clipboard::{ProductTransferDirection, ProductTransferState};
    use crate::cross_screen::{
        AutomaticChannelRole, CrossScreenProductSnapshot, CrossScreenProductState, PeerPlacement,
    };
    use std::path::Path;

    #[test]
    fn refresh_toasts_describe_state_and_ssh_operations_use_the_live_page() {
        assert_eq!(
            settings_command_success_toast(&SettingsWindowCommand::Refresh),
            Some("状态已刷新")
        );
        assert_eq!(
            settings_command_success_toast(&SettingsWindowCommand::Ssh(
                crate::ssh::Command::Refresh
            )),
            None
        );
    }

    #[test]
    fn native_error_text_never_leaks_backend_chinese_in_english_mode() {
        assert_eq!(
            localized_error_detail("连接对端超时，请检查防火墙", UiLanguage::English),
            "An internal operation failed. Open the diagnostic log for the complete error."
        );
        assert_eq!(
            localized_error_detail("Access denied", UiLanguage::English),
            "Access denied"
        );
        assert_eq!(
            localized_operation_error(
                UiLanguage::English,
                "操作失败",
                "The operation failed",
                "内部错误",
            ),
            "The operation failed. Open the diagnostic log for details."
        );
        assert_eq!(
            localized_operation_error(
                UiLanguage::ChineseSimplified,
                "操作失败",
                "The operation failed",
                "内部错误",
            ),
            "操作失败：内部错误"
        );
    }

    #[test]
    fn single_instance_handoff_waits_for_the_exiting_owner() {
        let name = format!("Local\\ClipFerry.Tray.Test.{}", std::process::id());
        let primary_name = name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let contender_name = primary_name.clone();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let primary = std::thread::spawn(move || {
            let acquisition =
                SingleInstance::acquire_named(windows_core::PCWSTR(primary_name.as_ptr())).unwrap();
            let SingleInstanceAcquisition::Primary(instance) = acquisition else {
                panic!("first test process did not own its unique mutex");
            };
            ready_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            drop(instance);
        });
        ready_receiver.recv().unwrap();

        let acquisition =
            SingleInstance::acquire_named(windows_core::PCWSTR(contender_name.as_ptr())).unwrap();
        let SingleInstanceAcquisition::Existing(handle) = acquisition else {
            panic!("contender bypassed the live primary mutex");
        };
        release_sender.send(()).unwrap();
        let replacement = SingleInstance::wait_for_exiting_instance(handle).unwrap();
        primary.join().unwrap();

        let acquisition =
            SingleInstance::acquire_named(windows_core::PCWSTR(contender_name.as_ptr())).unwrap();
        let SingleInstanceAcquisition::Existing(handle) = acquisition else {
            panic!("replacement did not retain mutex ownership");
        };
        // SAFETY: this contender never owned the mutex and closes only its own kernel handle.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
        drop(replacement);
    }

    #[test]
    fn embedded_ico_directory_selects_an_in_bounds_image() {
        let image = best_icon_image(ICON_BYTES).unwrap();
        assert!(image.len() >= 40);
        assert!(
            ICON_BYTES
                .windows(image.len())
                .any(|window| window == image)
        );
    }

    #[test]
    fn autostart_command_quotes_the_executable_and_uses_tray_mode() {
        let command =
            quote_tray_command(Path::new(r"C:\Program Files\Pierdesk\pierdesk.exe")).unwrap();
        assert_eq!(
            command,
            r#""C:\Program Files\Pierdesk\pierdesk.exe" tray"#
        );
        assert!(quote_tray_command(Path::new("C:\\bad\"path\\pierdesk.exe")).is_err());
    }

    #[test]
    fn autostart_migration_preserves_disabled_stale_and_existing_tasks() {
        use super::{AutostartState, should_migrate_autostart};
        use crate::autostart::TaskState;
        for legacy in [
            AutostartState::Enabled,
            AutostartState::Disabled,
            AutostartState::Stale,
        ] {
            for task in [
                TaskState::Missing,
                TaskState::Valid,
                TaskState::Disabled,
                TaskState::Stale,
            ] {
                assert_eq!(
                    should_migrate_autostart(legacy, task),
                    legacy == AutostartState::Enabled && task == TaskState::Missing
                );
            }
        }
    }

    #[test]
    fn fixed_wide_text_is_null_terminated_and_truncated() {
        let mut output = [0xFFFF; 5];
        copy_wide_to_fixed("Pierdesk", &mut output);
        assert_eq!(output, ['P' as u16, 'i' as u16, 'e' as u16, 'r' as u16, 0]);
    }

    #[test]
    fn native_menu_labels_follow_the_persisted_ui_language() {
        assert_eq!(
            localized(UiLanguage::English, "查看状态", "View status"),
            "View status"
        );
        assert_eq!(
            localized(UiLanguage::ChineseSimplified, "查看状态", "View status"),
            "查看状态"
        );
    }

    #[test]
    fn incoming_ssh_request_notification_is_emitted_once_until_request_disappears() {
        let request = vec![("request-1".to_owned(), "LAPTOP".to_owned())];
        let mut notified = std::collections::HashSet::new();
        assert_eq!(
            fresh_ssh_request_notifications(&mut notified, &request),
            request
        );
        assert!(fresh_ssh_request_notifications(&mut notified, &request).is_empty());
        assert!(fresh_ssh_request_notifications(&mut notified, &[]).is_empty());
        assert_eq!(
            fresh_ssh_request_notifications(&mut notified, &request),
            request
        );
    }

    #[test]
    fn completion_balloon_is_only_for_quiet_first_terminal_transition() {
        assert!(should_notify_transfer_completion(
            false,
            1,
            ProductTransferState::Completed,
            true
        ));
        assert!(should_notify_transfer_completion(
            false,
            1,
            ProductTransferState::Failed,
            true
        ));
        assert!(!should_notify_transfer_completion(
            true,
            1,
            ProductTransferState::Completed,
            true
        ));
        assert!(!should_notify_transfer_completion(
            false,
            1,
            ProductTransferState::Completed,
            false
        ));
    }

    #[test]
    fn completion_balloon_describes_both_directions_and_terminal_outcomes() {
        let sent = transfer_completion_notification(
            ProductTransferDirection::Sending,
            ProductTransferState::Completed,
            "report.zip",
            1536,
            "LAPTOP",
            UiLanguage::ChineseSimplified,
        )
        .unwrap();
        assert_eq!(sent.title, "Pierdesk · 文件发送完成");
        assert_eq!(sent.body, "report.zip · 1.5 KiB · LAPTOP");
        assert!(!sent.warning);

        let received = transfer_completion_notification(
            ProductTransferDirection::Receiving,
            ProductTransferState::Completed,
            "photo.png",
            300,
            "DESKTOP",
            UiLanguage::ChineseSimplified,
        )
        .unwrap();
        assert_eq!(received.title, "Pierdesk · 文件接收完成");
        assert_eq!(received.body, "photo.png · 300 B · DESKTOP");
        assert!(!received.warning);

        let failed = transfer_completion_notification(
            ProductTransferDirection::Receiving,
            ProductTransferState::Failed,
            "folder",
            0,
            "DESKTOP",
            UiLanguage::ChineseSimplified,
        )
        .unwrap();
        assert_eq!(failed.title, "Pierdesk · 文件接收失败");
        assert!(failed.warning);

        let english_failed = transfer_completion_notification(
            ProductTransferDirection::Receiving,
            ProductTransferState::Failed,
            "folder",
            0,
            "DESKTOP",
            UiLanguage::English,
        )
        .unwrap();
        assert_eq!(english_failed.title, "Pierdesk · File receive failed");
        assert!(english_failed.warning);

        let cancelled = transfer_completion_notification(
            ProductTransferDirection::Sending,
            ProductTransferState::Cancelled,
            "folder",
            0,
            "LAPTOP",
            UiLanguage::ChineseSimplified,
        )
        .unwrap();
        assert_eq!(cancelled.title, "Pierdesk · 文件发送已取消");
        assert!(!cancelled.warning);
    }

    #[test]
    fn start_balloon_fires_only_once_real_data_moves() {
        assert!(should_notify_transfer_start(
            1,
            ProductTransferState::Running
        ));
        assert!(!should_notify_transfer_start(
            0,
            ProductTransferState::Running
        ));
        for state in [
            ProductTransferState::AwaitingPaste,
            ProductTransferState::Paused,
            ProductTransferState::Completed,
            ProductTransferState::Cancelled,
            ProductTransferState::Failed,
        ] {
            assert!(!should_notify_transfer_start(1, state));
        }
        let received = transfer_start_notification(
            ProductTransferDirection::Receiving,
            "video.mp4",
            300,
            "DESKTOP",
            UiLanguage::ChineseSimplified,
        );
        assert_eq!(received.title, "Pierdesk · 开始接收文件");
        assert_eq!(received.body, "video.mp4 · 300 B · DESKTOP");
        let sent = transfer_start_notification(
            ProductTransferDirection::Sending,
            "video.mp4",
            300,
            "LAPTOP",
            UiLanguage::English,
        );
        assert_eq!(sent.title, "Pierdesk · Sending file");
    }

    #[test]
    fn completion_balloon_rejects_nonterminal_states() {
        for state in [
            ProductTransferState::AwaitingPaste,
            ProductTransferState::Running,
            ProductTransferState::Paused,
        ] {
            assert!(
                transfer_completion_notification(
                    ProductTransferDirection::Sending,
                    state,
                    "report.zip",
                    1536,
                    "LAPTOP",
                    UiLanguage::English,
                )
                .is_none()
            );
        }
    }

    #[test]
    fn cross_screen_log_deduplication_ignores_heartbeat_rtt_churn() {
        let mut snapshot = CrossScreenProductSnapshot {
            enabled: true,
            state: CrossScreenProductState::Connected,
            role: AutomaticChannelRole::Listener,
            peer_label: "peer".to_owned(),
            peer_placement: PeerPlacement::Right,
            local_screen: None,
            peer_screen: None,
            peer_screen_millimeters: None,
            remote_boundary_workspace: None,
            connections: serde_json::Value::Null,
            round_trip_micros: Some(1_000),
            session_id: Some("session".to_owned()),
            channel_epoch: Some(42),
            last_enter_latency_micros: None,
            connection_attempts: 1,
            successful_connections: 1,
            reconnects: 0,
            control_entries: 0,
            control_returns: 0,
            forwarded_events: 0,
            injected_events: 0,
            forced_releases: 0,
            native_input: crate::cross_screen::native_input::NativeInputDiagnostics::default(),
            clean_shutdown: true,
            last_stop_reason: None,
            last_control_return_reason: None,
            control_return_sequence: 0,
            last_transition_trace: None,
            transition_trace_sequence: 0,
            last_error: None,
        };
        let before = cross_screen_log_status(&snapshot);
        snapshot.round_trip_micros = Some(29_999);
        assert_eq!(cross_screen_log_status(&snapshot), before);

        snapshot.last_transition_trace = Some(
            "kind=enter_request,from=top,source=960,0,destination=1120,1399,delta=0,-7".to_owned(),
        );
        snapshot.transition_trace_sequence = 1;
        assert_ne!(cross_screen_log_status(&snapshot), before);

        snapshot.native_input.recovery_returns = 1;
        snapshot.native_input.contention_returns = 1;
        snapshot.native_input.last_recovery =
            Some(crate::cross_screen::native_input::NativeInputRecoveryReason::QueueContended);
        let recovered = cross_screen_log_status(&snapshot);
        assert!(recovered.contains("native_recovery=queue_contended"));
        assert!(recovered.contains("native_recovery_seq=1"));

        snapshot.state = CrossScreenProductState::Reconnecting;
        snapshot.last_error = Some("连接对端超时，请检查防火墙".to_owned());
        let tooltip = cross_screen_tooltip(&snapshot);
        assert!(tooltip.contains("跨屏正在重连"));
        assert!(tooltip.contains("请检查防火墙"));
    }
}
