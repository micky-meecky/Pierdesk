#![allow(clippy::inline_always, clippy::ref_as_ptr)]

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::mem::size_of;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::{
    COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_POINTER,
    HINSTANCE, HWND, LPARAM, LRESULT, POINT, POINTL, RECT, S_OK, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, RDW_ALLCHILDREN,
    RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, ScreenToClient,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CoCreateInstance, CoTaskMemFree, DATADIR_GET,
    FORMATETC, IDataObject, IServiceProvider, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{
    GetClipboardFormatNameW, GetClipboardSequenceNumber, RegisterClipboardFormatW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource,
    IDropSource_Impl, IDropSourceNotify, IDropSourceNotify_Impl, IDropTarget, IDropTarget_Impl,
    OleInitialize, OleUninitialize, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
    MOUSEINPUT, ReleaseCapture, SendInput, SetCapture, VK_LBUTTON,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CLSID_DragDropHelper, DragQueryFileW,
    FOLDERID_Desktop, FWF_AUTOARRANGE, Folder2, HDROP, IDataObjectAsyncCapability,
    IDropTargetHelper, IFolderView, IShellBrowser, IShellFolder, IShellFolderViewDual2,
    IShellWindows, IWebBrowser2, IsUserAnAdmin, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    SID_STopLevelBrowser, SVGIO_ALLVIEW, SVSI_POSITIONITEM, SWC_DESKTOP, SWFO_NEEDDISPATCH,
    ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW, GA_ROOT,
    GetAncestor, GetClassNameW, GetCursorPos, GetParent, GetSystemMetrics, GetWindowRect,
    GetWindowThreadProcessId, HWND_TOPMOST, LWA_ALPHA, MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW,
    PostThreadMessageW, RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SendMessageW, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    TranslateMessage, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_SETREDRAW,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WindowFromPoint,
};
use windows_core::{BOOL, Error, HRESULT, Interface, PCWSTR, Ref, Result, implement, w};

use super::data_object::{
    HdropGate, HdropProvider, SourceFactory, VirtualFileDataObject, VirtualFileEntry, format_etc,
};
use super::drag_helper::{
    DragCaptureEvent, DragCaptureHelper, foreground_process_diagnostic, process_image,
};
use super::drag_probe::{DragProbeEdge, RegisteredBoundaryWindow};
use super::local_file::{FileTreeSnapshot, LocalOfferRegistry};
use super::probe::{ProbeState, ShellTransferSnapshot};
use super::product::{ReverseOfferTunnel, start_drag_source_offer};
use super::secure_transfer::{
    RemoteTransferRegistry, SecureMetricsSnapshot, SecureOfferClient, SecureOfferServer,
    SecureOfferedFile,
};
use super::source::ReadAtSource;
use super::{catch_com_hresult, catch_com_result};
use crate::cross_screen::{DragMessage, ScreenBoundary};
use crate::device_store::DeviceStore;
use crate::security::{CertificateFingerprint, PinnedTlsClient};

const PREPARING: u8 = 0;
const ACTIVE: u8 = 1;
const RELEASED: u8 = 2;
const CANCELLED: u8 = 3;
const TRANSFERRING: u8 = 4;
const MAX_UI_TASKS: usize = 16;
const SOURCE_PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_SOURCE_RELEASE_GRACE: Duration = Duration::from_secs(2);
const SHELL_PENDING: u8 = 0;
const SHELL_SUCCEEDED: u8 = 1;
const SHELL_CANCELLED: u8 = 2;
const SHELL_FAILED: u8 = 3;
const FILE_DRAG_EDGE_THICKNESS: u32 = 8;
const FILE_DRAG_TASKBAR_THICKNESS: u32 = 96;
const LOCAL_DROP_CACHE_RETENTION: Duration = Duration::from_hours(168);
const LOCAL_DROP_READ_BUFFER: usize = 1024 * 1024;
// A target that received CF_HDROP already owns complete local files when DoDragDrop returns.
// Some call StartOperation without ever calling EndOperation, so only give late GetData calls a
// short window instead of holding the transfer open for the five-minute virtual-stream limit.
const LOCAL_DROP_SHELL_SETTLE: Duration = Duration::from_secs(5);
// The bootstrap Down normally reaches the anchor within a few milliseconds. While the anchor
// still provably owns the hit-test point and capture, a later Down can only land on the anchor,
// so a longer bounded wait is safe and avoids cancelling the drag on a scheduling hiccup.
const DRAG_INPUT_ACK_TIMEOUT: Duration = Duration::from_millis(120);
const DRAG_INPUT_OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
// The boundary-overlay trace was useful while stabilising handoff ordering, but it can toggle
// repeatedly during ordinary mouse use and synchronously write a large amount of unrelated data.
// Keep the probe available for a future regression without enabling it in normal diagnostic logs.
const VERBOSE_BOUNDARY_WINDOW_DIAGNOSTICS: bool = false;
/// Pointer travel from the press point before a motion-gated boundary window appears.
/// Larger than hardware jitter and the system drag threshold, so stationary clicks never arm it.
const BOUNDARY_ARM_DISTANCE_PX: i32 = 16;

struct TransferWatchdog {
    last_progress: Instant,
    bytes: u64,
}
impl TransferWatchdog {
    fn new(now: Instant) -> Self {
        Self {
            last_progress: now,
            bytes: 0,
        }
    }
    fn stalled(
        &mut self,
        progress: super::secure_transfer::TransferLocalProgress,
        now: Instant,
    ) -> bool {
        if progress.paused || progress.bytes_read != self.bytes {
            self.last_progress = now;
            self.bytes = progress.bytes_read;
        }
        now.saturating_duration_since(self.last_progress)
            > super::secure_transfer::TRANSFER_IDLE_TIMEOUT
    }
}

fn classify_shell_outcome(shell: ShellTransferSnapshot, network_complete: bool) -> u8 {
    let shell_effect = shell
        .logical_performed_effect
        .or(shell.performed_effect)
        .unwrap_or(shell.async_effects);
    let copy_effect = shell_effect & DROPEFFECT_COPY.0 != 0;
    let paste_succeeded = shell
        .paste_succeeded
        .is_none_or(|effect| effect & DROPEFFECT_COPY.0 != 0);
    let succeeded = if shell.async_started {
        shell.async_ended
            && shell.async_result == S_OK.0.cast_unsigned()
            && copy_effect
            && paste_succeeded
            && network_complete
    } else {
        // Synchronous targets finish all extraction before DoDragDrop returns. Some Shell
        // targets do not send a feedback format for this ordinary copy path.
        network_complete && paste_succeeded && (shell_effect == 0 || copy_effect)
    };

    if succeeded {
        SHELL_SUCCEEDED
    } else if shell.async_result == S_OK.0.cast_unsigned() {
        SHELL_CANCELLED
    } else {
        SHELL_FAILED
    }
}

struct TargetReturnPending {
    returning: bool,
    state: u8,
}

impl Default for TargetReturnPending {
    fn default() -> Self {
        Self {
            returning: false,
            state: PREPARING,
        }
    }
}

struct DragUiTask {
    manifest: super::secure_transfer::OfferManifest,
    registry: Arc<RemoteTransferRegistry>,
    generation: u64,
    created: Instant,
    accepted: AtomicBool,
    cancelled: AtomicBool,
    shell_outcome: AtomicU8,
    target_location: Mutex<String>,
}

static UI_TASKS: OnceLock<Mutex<Vec<Arc<DragUiTask>>>> = OnceLock::new();
static SOURCE_UI_TASKS: OnceLock<Mutex<Vec<Arc<DragSourceUiTask>>>> = OnceLock::new();
static NEXT_UI_GENERATION: AtomicU64 = AtomicU64::new(1_u64 << 63);
static CONNECTION_FAILURE: Mutex<Option<DragConnectionFailure>> = Mutex::new(None);

struct DragConnectionFailure {
    peer: CertificateFingerprint,
    message: String,
    created: Instant,
    pending: bool,
}

fn drag_connection_failure_message(
    label: &str,
    endpoint: SocketAddr,
    details: &str,
    language: crate::app_settings::UiLanguage,
) -> String {
    match language {
        crate::app_settings::UiLanguage::ChineseSimplified => format!(
            "无法建立到“{label}”({endpoint})的文件拖拽通道，原因未确认。请确认两端新版 Pierdesk 正在运行；在接收电脑的 Windows 安全中心 → 防火墙和网络保护 → 允许应用通过防火墙中，允许当前 Pierdesk 访问实际使用的网络类别。受管理的电脑请联系管理员。处理后重新拖拽。错误：{details}"
        ),
        crate::app_settings::UiLanguage::English => format!(
            "Cannot prepare file dragging to {label} ({endpoint}); cause unconfirmed. Keep Pierdesk updated and running on both PCs. On the receiving PC, open Windows Security > Firewall & network protection > Allow an app through firewall, and allow the running Pierdesk on the active network profile. Contact your administrator on managed PCs, then drag again. Error: {details}"
        ),
    }
}

fn report_drag_connection_failure(
    store: &DeviceStore,
    peer: CertificateFingerprint,
    endpoint: SocketAddr,
    error: &io::Error,
) {
    let label = store
        .load_peer(peer)
        .map_or_else(|_| peer.to_string(), |p| p.label);
    let language = crate::app_settings::UiPreferences::load_or_default(store).map_or_else(
        |_| crate::app_settings::UiLanguage::default(),
        |p| p.language,
    );
    let message = drag_connection_failure_message(&label, endpoint, &error.to_string(), language);
    let mut failure = CONNECTION_FAILURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failure.as_ref().is_some_and(|old| {
        old.message == message && old.created.elapsed() < Duration::from_secs(30)
    }) {
        return;
    }
    *failure = Some(DragConnectionFailure {
        peer,
        message,
        created: Instant::now(),
        pending: true,
    });
}

pub(crate) fn take_connection_failure_notification() -> Option<String> {
    let mut failure = CONNECTION_FAILURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let failure = failure.as_mut().filter(|failure| failure.pending)?;
    failure.pending = false;
    Some(failure.message.clone())
}

fn clear_connection_failure(peer: CertificateFingerprint) {
    let mut failure = CONNECTION_FAILURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failure.as_ref().is_some_and(|failure| failure.peer == peer) {
        *failure = None;
    }
}

struct DragSourceUiTask {
    manifest: super::secure_transfer::OfferManifest,
    gesture_id: u64,
    generation: u64,
    created: Instant,
    metrics: Mutex<SecureMetricsSnapshot>,
    file_progress: Mutex<Vec<super::product::ProductFileSnapshot>>,
    target_location: Mutex<String>,
    terminal: AtomicU8,
    control: super::secure_transfer::SourceTaskControl,
}

fn ui_tasks() -> &'static Mutex<Vec<Arc<DragUiTask>>> {
    UI_TASKS.get_or_init(|| Mutex::new(Vec::new()))
}

fn source_ui_tasks() -> &'static Mutex<Vec<Arc<DragSourceUiTask>>> {
    SOURCE_UI_TASKS.get_or_init(|| Mutex::new(Vec::new()))
}

fn register_source_ui_task(
    gesture_id: u64,
    manifest: &super::secure_transfer::OfferManifest,
    control: super::secure_transfer::SourceTaskControl,
) -> Arc<DragSourceUiTask> {
    let task = Arc::new(DragSourceUiTask {
        manifest: manifest.clone(),
        gesture_id,
        generation: NEXT_UI_GENERATION.fetch_add(1, Ordering::Relaxed),
        created: Instant::now(),
        metrics: Mutex::new(SecureMetricsSnapshot::default()),
        file_progress: Mutex::new(Vec::new()),
        target_location: Mutex::new(String::new()),
        terminal: AtomicU8::new(SHELL_PENDING),
        control,
    });
    let mut tasks = source_ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if tasks.len() == MAX_UI_TASKS {
        tasks.remove(0);
    }
    tasks.push(Arc::clone(&task));
    task
}

fn finish_source_ui_task(gesture_id: u64, completed: bool) {
    if let Some(task) = source_ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|task| task.gesture_id == gesture_id)
    {
        task.terminal.store(
            if completed {
                SHELL_SUCCEEDED
            } else {
                SHELL_CANCELLED
            },
            Ordering::Release,
        );
    }
}

fn register_ui_task(
    manifest: &super::secure_transfer::OfferManifest,
    registry: &Arc<RemoteTransferRegistry>,
) -> Arc<DragUiTask> {
    let task = Arc::new(DragUiTask {
        manifest: manifest.clone(),
        registry: Arc::clone(registry),
        generation: NEXT_UI_GENERATION.fetch_add(1, Ordering::Relaxed),
        created: Instant::now(),
        accepted: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        shell_outcome: AtomicU8::new(SHELL_PENDING),
        target_location: Mutex::new(String::new()),
    });
    let mut tasks = ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tasks.retain(|existing| existing.manifest.offer_id != manifest.offer_id);
    if tasks.len() == MAX_UI_TASKS {
        tasks.remove(0);
    }
    tasks.push(Arc::clone(&task));
    task
}

#[allow(clippy::too_many_lines)]
fn ui_task_snapshot(task: &DragUiTask) -> super::product::ProductTransferTaskSnapshot {
    use super::product::{
        ProductManifestSummary, ProductTransferDirection, ProductTransferId,
        ProductTransferSnapshot, ProductTransferState, ProductTransferTaskSnapshot,
    };
    let files = task.registry.file_progress(&task.manifest);
    let ordinary = files
        .iter()
        .filter(|file| !file.is_directory)
        .collect::<Vec<_>>();
    let progress = task.registry.local_progress();
    let all_terminal = !ordinary.is_empty()
        && ordinary.iter().all(|file| {
            matches!(
                file.state,
                ProductTransferState::Completed
                    | ProductTransferState::Cancelled
                    | ProductTransferState::Failed
            )
        });
    let shell_outcome = task.shell_outcome.load(Ordering::Acquire);
    let state = if shell_outcome == SHELL_FAILED {
        ProductTransferState::Failed
    } else if task.cancelled.load(Ordering::Acquire)
        || progress.cancelled
        || shell_outcome == SHELL_CANCELLED
    {
        ProductTransferState::Cancelled
    } else if shell_outcome == SHELL_FAILED
        || ordinary
            .iter()
            .any(|file| file.state == ProductTransferState::Failed)
    {
        ProductTransferState::Failed
    } else if shell_outcome == SHELL_SUCCEEDED
        && ((all_terminal
            && ordinary
                .iter()
                .all(|file| file.state == ProductTransferState::Completed))
            || ordinary.iter().all(|file| file.size == 0))
    {
        ProductTransferState::Completed
    } else if progress.paused {
        ProductTransferState::Paused
    } else if progress.started_transfers != 0 || task.accepted.load(Ordering::Acquire) {
        ProductTransferState::Running
    } else {
        ProductTransferState::AwaitingPaste
    };
    let first = task.manifest.entries.first().map_or_else(
        || "拖拽文件".to_owned(),
        |entry| entry.descriptor.file_name.to_string(),
    );
    let manifest = ProductManifestSummary {
        direction: ProductTransferDirection::Receiving.label(),
        primary_name: first,
        items: task
            .manifest
            .entries
            .iter()
            .filter(|entry| !entry.descriptor.file_name.contains('\\'))
            .count(),
        files: ordinary.len(),
        directories: files.iter().filter(|file| file.is_directory).count(),
        total_size: ordinary.iter().map(|file| file.size).sum(),
    };
    let (current_file_name, current_file_transferred, current_file_size) = task
        .registry
        .current_file_progress()
        .map_or((None, 0, 0), |(name, transferred, size)| {
            (Some(name), transferred, size)
        });
    let recovery = task.registry.recovery_snapshot();
    ProductTransferTaskSnapshot {
        id: ProductTransferId::new(
            ProductTransferDirection::Receiving,
            task.manifest.offer_id.bytes(),
        ),
        generation: task.generation,
        manifest: manifest.clone(),
        source_location: task.manifest.source_location.to_string(),
        target_location: task
            .target_location
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        started_unix_ms: 0,
        completed_unix_ms: 0,
        transfer: ProductTransferSnapshot {
            state,
            transferred: ordinary.iter().map(|file| file.transferred).sum(),
            total_size: manifest.total_size,
            bytes_per_second: 0,
            average_bytes_per_second: 0,
            started_files: progress
                .started_transfers
                .max(usize::from(task.accepted.load(Ordering::Acquire))),
            total_files: manifest.files,
            current_file_name,
            current_file_transferred,
            current_file_size,
            reconnect_attempts: recovery.reconnect_attempts,
            recovered_commands: recovery.recovered_commands,
            recovery_active: recovery.active_recoveries != 0,
        },
        files,
    }
}

#[allow(clippy::too_many_lines)]
fn source_ui_task_snapshot(task: &DragSourceUiTask) -> super::product::ProductTransferTaskSnapshot {
    use super::product::{
        ProductFileSnapshot, ProductManifestSummary, ProductTransferDirection, ProductTransferId,
        ProductTransferSnapshot, ProductTransferState, ProductTransferTaskSnapshot,
    };
    let metrics = *task
        .metrics
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let terminal = task.terminal.load(Ordering::Acquire);
    let state = match terminal {
        SHELL_SUCCEEDED => ProductTransferState::Completed,
        SHELL_CANCELLED => ProductTransferState::Cancelled,
        SHELL_FAILED => ProductTransferState::Failed,
        _ if task.control.state() == super::secure_transfer::RemoteTransferState::Cancelled => {
            ProductTransferState::Cancelled
        }
        _ if task.control.state() == super::secure_transfer::RemoteTransferState::Paused => {
            ProductTransferState::Paused
        }
        _ if metrics.begun_transfers != 0 => ProductTransferState::Running,
        _ => ProductTransferState::AwaitingPaste,
    };
    let ordinary = task
        .manifest
        .entries
        .iter()
        .filter(|entry| !entry.descriptor.is_directory())
        .collect::<Vec<_>>();
    let total_size = ordinary.iter().map(|entry| entry.descriptor.size).sum();
    let files = task
        .manifest
        .entries
        .iter()
        .map(|entry| {
            let size = entry.descriptor.size;
            let transferred = if state == ProductTransferState::Completed {
                size
            } else if ordinary.len() == 1 && !entry.descriptor.is_directory() {
                metrics.unique_bytes.min(size)
            } else {
                0
            };
            ProductFileSnapshot {
                id: entry.file_id.bytes(),
                name: entry.descriptor.file_name.to_string(),
                size,
                transferred,
                bytes_per_second: 0,
                state,
                is_directory: entry.descriptor.is_directory(),
            }
        })
        .collect();
    let measured_files = task
        .file_progress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let files = if measured_files.is_empty() {
        files
    } else {
        measured_files
            .iter()
            .cloned()
            .map(|mut file| {
                if state == ProductTransferState::Completed {
                    file.transferred = file.size;
                    file.state = state;
                }
                file
            })
            .collect()
    };
    let primary_name = task.manifest.entries.first().map_or_else(
        || "拖拽文件".to_owned(),
        |entry| entry.descriptor.file_name.to_string(),
    );
    let manifest = ProductManifestSummary {
        direction: ProductTransferDirection::Sending.label(),
        primary_name,
        items: task
            .manifest
            .entries
            .iter()
            .filter(|entry| !entry.descriptor.file_name.contains('\\'))
            .count(),
        files: ordinary.len(),
        directories: task
            .manifest
            .entries
            .iter()
            .filter(|entry| entry.descriptor.is_directory())
            .count(),
        total_size,
    };
    ProductTransferTaskSnapshot {
        id: ProductTransferId::new(
            ProductTransferDirection::Sending,
            task.manifest.offer_id.bytes(),
        ),
        generation: task.generation,
        manifest,
        transfer: ProductTransferSnapshot {
            state,
            transferred: metrics.unique_bytes.min(total_size),
            total_size,
            bytes_per_second: 0,
            average_bytes_per_second: 0,
            started_files: usize::try_from(metrics.begun_transfers)
                .unwrap_or(usize::MAX)
                .max(usize::from(terminal != SHELL_PENDING)),
            total_files: ordinary.len(),
            current_file_name: None,
            current_file_transferred: 0,
            current_file_size: 0,
            reconnect_attempts: 0,
            recovered_commands: 0,
            recovery_active: false,
        },
        files,
        source_location: task.manifest.source_location.to_string(),
        target_location: task
            .target_location
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        started_unix_ms: 0,
        completed_unix_ms: 0,
    }
}

pub(crate) fn merge_ui_snapshot(snapshot: &mut super::product::ProductSnapshot) {
    if snapshot.last_error.is_none() {
        snapshot.last_error = CONNECTION_FAILURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|failure| failure.message.clone());
    }
    let mut tasks = ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tasks.retain(|task| {
        !matches!(
            ui_task_snapshot(task).transfer.state,
            super::product::ProductTransferState::Completed
                | super::product::ProductTransferState::Cancelled
                | super::product::ProductTransferState::Failed
        ) || task.created.elapsed() < Duration::from_mins(10)
    });
    let external = tasks
        .iter()
        .map(|task| ui_task_snapshot(task))
        .collect::<Vec<_>>();
    drop(tasks);
    let mut source_tasks = source_ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    source_tasks.retain(|task| {
        !matches!(
            source_ui_task_snapshot(task).transfer.state,
            super::product::ProductTransferState::Completed
                | super::product::ProductTransferState::Cancelled
                | super::product::ProductTransferState::Failed
        ) || task.created.elapsed() < Duration::from_mins(10)
    });
    let mut external = external;
    external.extend(
        source_tasks
            .iter()
            .map(|task| source_ui_task_snapshot(task)),
    );
    drop(source_tasks);
    for task in external {
        if snapshot
            .transfer_tasks
            .iter()
            .all(|existing| existing.id != task.id)
        {
            snapshot.transfer_tasks.push(task);
        }
    }
    snapshot.transfer_tasks.sort_by_key(|task| task.generation);
    if let Some(selected) = snapshot.transfer_tasks.last().cloned() {
        snapshot.transfer_generation = selected.generation;
        snapshot.last_manifest = Some(selected.manifest);
        snapshot.transfer = Some(selected.transfer);
    }
}

pub(crate) fn control_ui_tasks(
    id: Option<super::product::ProductTransferId>,
    state: super::secure_transfer::RemoteTransferState,
) -> bool {
    let tasks = ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut found = false;
    for task in tasks.iter() {
        let task_id = super::product::ProductTransferId::new(
            super::product::ProductTransferDirection::Receiving,
            task.manifest.offer_id.bytes(),
        );
        if id.is_none_or(|id| id == task_id) {
            found = true;
            if !ui_task_snapshot(task).locally_controllable() {
                continue;
            }
            task.registry.set_local_control(state);
            if state == super::secure_transfer::RemoteTransferState::Cancelled {
                task.cancelled.store(true, Ordering::Release);
            }
        }
    }
    drop(tasks);
    for task in source_ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
    {
        let task_id = super::product::ProductTransferId::new(
            super::product::ProductTransferDirection::Sending,
            task.manifest.offer_id.bytes(),
        );
        if id.is_none_or(|id| id == task_id) {
            if source_ui_task_snapshot(task).locally_controllable() {
                task.control.set(state);
            }
            found = true;
        }
    }
    found
}

pub(crate) fn cancel_ui_files(id: super::product::ProductTransferId, ids: &[[u8; 16]]) -> bool {
    let tasks = ui_tasks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(task) = tasks.iter().find(|task| {
        super::product::ProductTransferId::new(
            super::product::ProductTransferDirection::Receiving,
            task.manifest.offer_id.bytes(),
        ) == id
    }) else {
        return false;
    };
    task.registry.cancel_files(ids);
    true
}

/// Shared state contains no COM pointers. Both OLE endpoints live on the dedicated STA.
pub(crate) struct DragBridge {
    stop: AtomicBool,
    pub local_candidate: AtomicBool,
    pub boundary_visible: AtomicBool,
    pub boundary_needs_motion: AtomicBool,
    pub source_id: AtomicU64,
    pub source_ready: AtomicBool,
    pub source_active: AtomicBool,
    pub source_remote_controlled: AtomicBool,
    target_reverse: AtomicBool,
    target_tunnel: AtomicBool,
    target_prepared: AtomicBool,
    target_armed: AtomicBool,
    reverse_arrival_pending: AtomicBool,
    target_returning: AtomicBool,
    target_control_arrived: AtomicBool,
    suppress_failed_reverse_left_restore: AtomicBool,
    target_return_pending: Mutex<TargetReturnPending>,
    remote_source_left_up: Mutex<Option<(u64, Instant)>>,
    return_wait: Mutex<Option<Instant>>,
    peer_candidate: Mutex<Option<(u64, Instant, bool)>>,
    source_capture: Mutex<()>,
    source_image: Mutex<Option<(u64, Vec<u8>)>>,
    source_released: AtomicBool,
    target_id: AtomicU64,
    target_state: AtomicU8,
    target_release_point: Mutex<Option<POINT>>,
    target_request: Mutex<Option<DragMessage>>,
    captured: Mutex<Option<Vec<PathBuf>>>,
    next_id: AtomicU64,
    last_peer_id: AtomicU64,
    edge_wait: Mutex<Option<Instant>>,
    source_gate_logged: AtomicBool,
    source_return_requested: AtomicBool,
    source_return_presenting: AtomicBool,
    source_return_remote_controlled: AtomicBool,
    source_return_handoff: AtomicBool,
    // Set when a remotely controlled source hands its re-entry preview back to the peer's
    // reverse receiver; the receiver's next READY must then re-arm it with ACTIVATE.
    remote_rearm_on_ready: AtomicBool,
    source_return_drop_requested: AtomicBool,
    outgoing: mpsc::SyncSender<DragMessage>,
    incoming: Mutex<mpsc::Receiver<DragMessage>>,
    log_path: PathBuf,
}

impl DragBridge {
    pub(crate) fn start(
        store: DeviceStore,
        peer: CertificateFingerprint,
        local: SocketAddr,
        remote: SocketAddr,
        boundary: ScreenBoundary,
    ) -> io::Result<(Arc<Self>, JoinHandle<()>)> {
        let bridge = Arc::new(Self::new(store.root().join("file-drag.log")));
        let worker_bridge = Arc::clone(&bridge);
        let worker = std::thread::Builder::new()
            .name("pierdesk-file-drag-sta".into())
            .spawn(move || {
                if let Err(error) = run_sta(&worker_bridge, &store, peer, local, remote, boundary) {
                    worker_bridge.log(&format!("event=worker_error error={error}"));
                    worker_bridge.stop.store(true, Ordering::Release);
                }
            })?;
        Ok((bridge, worker))
    }

    fn new(log_path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::sync_channel(32);
        Self {
            stop: AtomicBool::new(false),
            local_candidate: AtomicBool::new(false),
            boundary_visible: AtomicBool::new(false),
            boundary_needs_motion: AtomicBool::new(false),
            source_id: AtomicU64::new(0),
            source_ready: AtomicBool::new(false),
            source_active: AtomicBool::new(false),
            source_remote_controlled: AtomicBool::new(false),
            target_reverse: AtomicBool::new(false),
            target_tunnel: AtomicBool::new(false),
            target_prepared: AtomicBool::new(false),
            target_armed: AtomicBool::new(false),
            reverse_arrival_pending: AtomicBool::new(false),
            target_returning: AtomicBool::new(false),
            target_control_arrived: AtomicBool::new(false),
            suppress_failed_reverse_left_restore: AtomicBool::new(false),
            target_return_pending: Mutex::new(TargetReturnPending::default()),
            remote_source_left_up: Mutex::new(None),
            return_wait: Mutex::new(None),
            peer_candidate: Mutex::new(None),
            source_capture: Mutex::new(()),
            source_image: Mutex::new(None),
            source_released: AtomicBool::new(false),
            target_id: AtomicU64::new(0),
            target_state: AtomicU8::new(PREPARING),
            target_release_point: Mutex::new(None),
            target_request: Mutex::new(None),
            captured: Mutex::new(None),
            next_id: AtomicU64::new(1),
            last_peer_id: AtomicU64::new(0),
            edge_wait: Mutex::new(None),
            source_gate_logged: AtomicBool::new(false),
            source_return_requested: AtomicBool::new(false),
            source_return_presenting: AtomicBool::new(false),
            source_return_remote_controlled: AtomicBool::new(false),
            source_return_handoff: AtomicBool::new(false),
            remote_rearm_on_ready: AtomicBool::new(false),
            source_return_drop_requested: AtomicBool::new(false),
            outgoing: sender,
            incoming: Mutex::new(receiver),
            log_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn returned_source_for_test() -> Arc<Self> {
        let bridge = Arc::new(Self::new(PathBuf::new()));
        bridge.source_id.store(1, Ordering::Release);
        bridge
            .source_return_requested
            .store(true, Ordering::Release);
        bridge
    }

    #[cfg(test)]
    pub(crate) fn return_release_requested_for_test(&self) -> bool {
        self.source_return_drop_requested.load(Ordering::Acquire)
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.target_state.store(CANCELLED, Ordering::Release);
    }

    pub(crate) fn has_live_session(&self) -> bool {
        self.source_id.load(Ordering::Acquire) != 0
            || self.target_id.load(Ordering::Acquire) != 0
            || self.local_candidate.load(Ordering::Acquire)
            || self
                .peer_candidate
                .lock()
                .expect("candidate lock")
                .is_some()
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub(crate) fn poll(&self) -> Option<DragMessage> {
        self.incoming.lock().ok()?.try_recv().ok()
    }

    pub(crate) fn classify_idle_source(&self, remote_controlled: bool) {
        // Ownership is frozen from DragEnter until the offer reaches a protocol terminal state.
        // Route changes during a multi-crossing drag must never reclassify its retained object.
        let _capture = self.source_capture.lock().expect("source capture lock");
        if self.source_id.load(Ordering::Acquire) == 0 {
            self.source_remote_controlled
                .store(remote_controlled, Ordering::Release);
        }
    }

    pub(crate) fn retains_active_source(&self) -> bool {
        let _capture = self.source_capture.lock().expect("source capture lock");
        self.source_id.load(Ordering::Acquire) != 0
            && self.source_active.load(Ordering::Acquire)
            && !self.source_released.load(Ordering::Acquire)
    }

    fn send(&self, id: u64, kind: u8, port: u16) {
        if self
            .outgoing
            .try_send(DragMessage { id, kind, port })
            .is_err()
        {
            self.stop();
        }
    }

    #[cfg(test)]
    fn receive(&self, message: DragMessage, locally_controlled: bool) {
        self.receive_for_route(message, locally_controlled, false);
    }

    pub(crate) fn receive_for_route(
        &self,
        message: DragMessage,
        locally_controlled: bool,
        reverse: bool,
    ) {
        self.log_receive(message, locally_controlled, reverse);
        match message.kind {
            DragMessage::CANDIDATE => {
                self.accept_peer_candidate(message.id, locally_controlled, reverse);
            }
            DragMessage::PREPARE | DragMessage::PREPARE_TUNNEL => {
                if self.stop.load(Ordering::Acquire)
                    || !locally_controlled
                    || self.local_candidate.load(Ordering::Acquire)
                    || self.source_id.load(Ordering::Acquire) != 0
                    || self.target_id.load(Ordering::Acquire) != 0
                    || message.id <= self.last_peer_id.load(Ordering::Acquire)
                {
                    if reverse {
                        self.mark_failed_reverse_target(message.id, "prepare_rejected");
                    }
                    self.send(message.id, DragMessage::CANCEL, 0);
                    return;
                }
                self.last_peer_id.store(message.id, Ordering::Release);
                self.peer_candidate.lock().expect("candidate lock").take();
                self.target_reverse.store(reverse, Ordering::Release);
                self.target_tunnel.store(
                    message.kind == DragMessage::PREPARE_TUNNEL,
                    Ordering::Release,
                );
                self.target_prepared.store(false, Ordering::Release);
                self.target_armed.store(false, Ordering::Release);
                self.reverse_arrival_pending.store(false, Ordering::Release);
                self.target_returning.store(false, Ordering::Release);
                self.target_control_arrived.store(false, Ordering::Release);
                *self
                    .target_return_pending
                    .lock()
                    .expect("target return lock") = TargetReturnPending::default();
                self.target_state.store(PREPARING, Ordering::Release);
                self.target_id.store(message.id, Ordering::Release);
                self.target_release_point
                    .lock()
                    .expect("target release point lock")
                    .take();
                *self.target_request.lock().expect("drag request lock") = Some(message);
            }
            DragMessage::READY if message.id == self.source_id.load(Ordering::Acquire) => {
                self.accept_source_ready(message.id);
            }
            DragMessage::ACTIVATE | DragMessage::RELEASE | DragMessage::CANCEL
                if message.id == self.target_id.load(Ordering::Acquire) =>
            {
                if message.kind == DragMessage::CANCEL
                    && self.target_reverse.load(Ordering::Acquire)
                {
                    self.mark_failed_reverse_target(message.id, "peer_cancelled_target");
                }
                if self.queue_returning_target_message(message) {
                    return;
                }
                if message.kind == DragMessage::ACTIVATE
                    && self.target_reverse.load(Ordering::Acquire)
                {
                    self.arm_reverse_target(message.id);
                    return;
                }
                let next = match message.kind {
                    DragMessage::ACTIVATE => ACTIVE,
                    DragMessage::RELEASE => RELEASED,
                    _ => CANCELLED,
                };
                let _ =
                    self.target_state
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                            (old < next).then_some(if old == PREPARING && next == RELEASED {
                                CANCELLED
                            } else {
                                next
                            })
                        });
            }
            DragMessage::CANCEL | DragMessage::FINISHED
                if message.id == self.source_id.load(Ordering::Acquire) =>
            {
                finish_source_ui_task(message.id, message.kind == DragMessage::FINISHED);
                self.clear_source();
            }
            DragMessage::RELEASE
                if message.id == self.source_id.load(Ordering::Acquire)
                    && self.source_remote_controlled.load(Ordering::Acquire) =>
            {
                self.accept_remote_source_release();
            }
            DragMessage::CANCEL => {
                self.cancel_peer_candidate(message.id, reverse);
            }
            _ => {}
        }
    }

    fn accept_peer_candidate(&self, id: u64, locally_controlled: bool, reverse: bool) {
        if locally_controlled
            && !self.stop.load(Ordering::Acquire)
            && self.source_id.load(Ordering::Acquire) == 0
            && self.target_id.load(Ordering::Acquire) == 0
            && id > self.last_peer_id.load(Ordering::Acquire)
        {
            if reverse {
                self.suppress_failed_reverse_left_restore
                    .store(false, Ordering::Release);
            }
            *self.peer_candidate.lock().expect("candidate lock") =
                Some((id, Instant::now(), reverse));
        } else {
            if reverse {
                self.mark_failed_reverse_target(id, "candidate_rejected");
            }
            self.send(id, DragMessage::CANCEL, 0);
        }
    }

    fn log_receive(&self, message: DragMessage, locally_controlled: bool, reverse: bool) {
        self.log(&format!(
            "event=receive id={} kind={} available={locally_controlled} reverse={reverse}",
            message.id, message.kind,
        ));
    }

    fn accept_source_ready(&self, id: u64) {
        self.source_ready.store(true, Ordering::Release);
        self.log(&format!(
            "event=source_ready id={id} local_candidate={} boundary_visible={} remote_controlled={}",
            self.local_candidate.load(Ordering::Acquire),
            self.boundary_visible.load(Ordering::Acquire),
            self.source_remote_controlled.load(Ordering::Acquire),
        ));
        // A remotely controlled source keeps its reverse receiver on the peer; READY there means
        // that receiver re-armed, possibly after this computer's re-entry preview already ended.
        if self.source_active.load(Ordering::Acquire)
            && (self.source_return_requested.load(Ordering::Acquire)
                || self.source_return_presenting.load(Ordering::Acquire)
                || self.remote_rearm_on_ready.swap(false, Ordering::AcqRel))
        {
            self.log(&format!("event=source_return_target_rearmed id={id}"));
            self.send(id, DragMessage::ACTIVATE, 0);
        }
    }

    fn begin_source_capture(&self, id: u64) {
        // `network_loop` classifies the computer that owns this Explorer drag before the edge
        // target receives DragEnter.  Keep that classification intact: when this computer is
        // being controlled by its peer, clearing it here makes the first outbound crossing work
        // but sends a still-held return through the physical-source path, so the native drag
        // image disappears on re-entry.
        let _capture = self.source_capture.lock().expect("source capture lock");
        self.local_candidate.store(true, Ordering::Release);
        self.source_ready.store(false, Ordering::Release);
        self.source_active.store(false, Ordering::Release);
        self.source_released.store(false, Ordering::Release);
        self.source_return_drop_requested
            .store(false, Ordering::Release);
        self.source_return_requested.store(false, Ordering::Release);
        self.source_return_presenting
            .store(false, Ordering::Release);
        self.source_return_remote_controlled
            .store(false, Ordering::Release);
        self.source_return_handoff.store(false, Ordering::Release);
        self.remote_rearm_on_ready.store(false, Ordering::Release);
        self.source_gate_logged.store(false, Ordering::Release);
        // Publish the id last. While it is zero the network loop may still refresh
        // `source_remote_controlled` from the authoritative control route.
        self.source_id.store(id, Ordering::Release);
    }

    fn capture_helper_paths(&self, paths: Vec<PathBuf>) {
        if paths.is_empty()
            || self.stop.load(Ordering::Acquire)
            || self.target_pending()
            || self.source_id.load(Ordering::Acquire) != 0
        {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        self.begin_source_capture(id);
        self.log(&format!(
            "event=drag_enter id={id} paths={} capture=desktop_shell_helper boundary_visible={} remote_controlled={}",
            paths.len(),
            self.boundary_visible.load(Ordering::Acquire),
            self.source_remote_controlled.load(Ordering::Acquire),
        ));
        self.send(id, DragMessage::CANDIDATE, 0);
        *self.captured.lock().expect("drag capture lock") = Some(paths);
    }

    fn cancel_peer_candidate(&self, id: u64, reverse: bool) {
        let mut candidate = self.peer_candidate.lock().expect("candidate lock");
        if candidate.is_some_and(|(candidate_id, _, _)| candidate_id == id) {
            candidate.take();
            if reverse {
                drop(candidate);
                self.mark_failed_reverse_target(id, "peer_cancelled_candidate");
            }
        }
    }

    fn mark_failed_reverse_target(&self, id: u64, reason: &str) {
        self.suppress_failed_reverse_left_restore
            .store(true, Ordering::Release);
        self.log(&format!(
            "event=reverse_target_failed id={id} reason={reason} suppress_left_restore=true"
        ));
    }

    pub(crate) fn take_failed_reverse_left_restore(&self) -> bool {
        self.suppress_failed_reverse_left_restore
            .swap(false, Ordering::AcqRel)
    }

    pub(crate) fn clear_failed_reverse_left_restore(&self) {
        self.suppress_failed_reverse_left_restore
            .store(false, Ordering::Release);
    }

    fn capture_target_release_point(&self) {
        let mut release = self
            .target_release_point
            .lock()
            .expect("target release point lock");
        if release.is_some() {
            return;
        }
        let point = cursor_position();
        *release = Some(point);
        self.log(&format!(
            "event=target_release_point id={} cursor={},{}",
            self.target_id.load(Ordering::Acquire),
            point.x,
            point.y,
        ));
    }

    fn accept_remote_source_release(&self) {
        self.remote_source_left_up
            .lock()
            .expect("remote source release lock")
            .take();
        self.source_released.store(true, Ordering::Release);
    }

    fn queue_returning_target_message(&self, message: DragMessage) -> bool {
        if self.target_reverse.load(Ordering::Acquire) {
            return false;
        }
        let mut pending = self
            .target_return_pending
            .lock()
            .expect("target return lock");
        if !pending.returning {
            return false;
        }
        match message.kind {
            DragMessage::ACTIVATE if pending.state < ACTIVE => {
                pending.state = ACTIVE;
            }
            DragMessage::RELEASE => {
                if pending.state < RELEASED {
                    pending.state = if pending.state == PREPARING {
                        CANCELLED
                    } else {
                        RELEASED
                    };
                }
            }
            DragMessage::CANCEL if pending.state < CANCELLED => {
                pending.state = CANCELLED;
            }
            DragMessage::ACTIVATE | DragMessage::CANCEL => {}
            _ => return false,
        }
        let state = pending.state;
        drop(pending);
        self.log(&format!(
            "event=target_return_message_queued id={} kind={} state={state}",
            message.id, message.kind,
        ));
        true
    }

    fn clear_source(&self) {
        let _capture = self.source_capture.lock().expect("source capture lock");
        let id = self.source_id.load(Ordering::Acquire);
        if id != 0 {
            self.log(&format!(
                "event=source_clear id={id} ready={} active={} released={} boundary_visible={}",
                self.source_ready.load(Ordering::Acquire),
                self.source_active.load(Ordering::Acquire),
                self.source_released.load(Ordering::Acquire),
                self.boundary_visible.load(Ordering::Acquire),
            ));
        }
        self.source_released.store(false, Ordering::Release);
        self.source_active.store(false, Ordering::Release);
        self.source_ready.store(false, Ordering::Release);
        self.source_id.store(0, Ordering::Release);
        self.local_candidate.store(false, Ordering::Release);
        self.source_remote_controlled
            .store(false, Ordering::Release);
        self.remote_source_left_up
            .lock()
            .expect("remote source release lock")
            .take();
        self.captured.lock().expect("drag capture lock").take();
        self.source_image.lock().expect("source image lock").take();
        *self.edge_wait.lock().expect("edge wait lock") = None;
        self.source_gate_logged.store(false, Ordering::Release);
        self.source_return_requested.store(false, Ordering::Release);
        self.source_return_remote_controlled
            .store(false, Ordering::Release);
        self.source_return_handoff.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn cancel(&self) {
        self.cancel_with_reason("unspecified");
    }

    pub(crate) fn cancel_with_reason(&self, reason: &str) {
        let source_id = self.source_id.load(Ordering::Acquire);
        let target_id = self.target_id.load(Ordering::Acquire);
        let local_candidate = self.local_candidate.load(Ordering::Acquire);
        let peer_candidate = self
            .peer_candidate
            .lock()
            .expect("candidate lock")
            .is_some();
        if source_id != 0 || target_id != 0 || local_candidate || peer_candidate {
            self.log(&format!(
                "event=cancel reason={reason} source_id={source_id} source_ready={} source_active={} source_released={} local_candidate={local_candidate} boundary_visible={} target_id={target_id} target_state={} peer_candidate={peer_candidate}",
                self.source_ready.load(Ordering::Acquire),
                self.source_active.load(Ordering::Acquire),
                self.source_released.load(Ordering::Acquire),
                self.boundary_visible.load(Ordering::Acquire),
                self.target_state.load(Ordering::Acquire),
            ));
        }
        if let Some((id, _, _)) = self.peer_candidate.lock().expect("candidate lock").take() {
            self.send(id, DragMessage::CANCEL, 0);
        }
        *self.return_wait.lock().expect("return wait") = None;
        self.target_returning.store(false, Ordering::Release);
        *self
            .target_return_pending
            .lock()
            .expect("target return lock") = TargetReturnPending::default();
        let id = self.source_id.load(Ordering::Acquire);
        if !self.source_released.load(Ordering::Acquire) {
            if id != 0 {
                self.send(id, DragMessage::CANCEL, 0);
                finish_source_ui_task(id, false);
            }
            self.clear_source();
        }
        let cancelled_target =
            self.target_state
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                    (state < CANCELLED).then_some(CANCELLED)
                });
        if cancelled_target.is_ok() && self.target_id.load(Ordering::Acquire) != 0 {
            self.send(
                self.target_id.load(Ordering::Acquire),
                DragMessage::CANCEL,
                0,
            );
        }
    }

    pub(crate) fn activate_source(&self) {
        let id = self.source_id.load(Ordering::Acquire);
        if id != 0 && self.source_can_handoff() {
            self.source_active.store(true, Ordering::Release);
            self.log(&format!(
                "event=source_activate id={id} ready={} remote_controlled={} boundary_visible={}",
                self.source_ready.load(Ordering::Acquire),
                self.source_remote_controlled.load(Ordering::Acquire),
                self.boundary_visible.load(Ordering::Acquire),
            ));
            self.send(id, DragMessage::ACTIVATE, 0);
        }
    }

    /// Requests a click-through image preview of the retained source after the
    /// pointer returns to the computer where the drag started.
    pub(crate) fn present_source_after_return(&self) -> bool {
        self.request_source_return_presentation(false, "physical_source_return")
    }

    /// Requests the same retained Explorer object when a remotely controlled drag returns to
    /// its source computer. This is the reverse-direction counterpart of
    /// `present_source_after_return`.
    pub(crate) fn present_remote_source_after_peer_reentry(&self) -> bool {
        self.request_source_return_presentation(true, "controlled_source_reentry")
    }

    fn request_source_return_presentation(&self, remote_controlled: bool, route: &str) -> bool {
        let id = self.source_id.load(Ordering::Acquire);
        let ready = self.source_ready.load(Ordering::Acquire);
        let local_candidate = self.local_candidate.load(Ordering::Acquire);
        let released = self.source_released.load(Ordering::Acquire);
        let active = self.source_active.load(Ordering::Acquire);
        let actual_remote_controlled = self.source_remote_controlled.load(Ordering::Acquire);
        if id == 0
            || !ready
            || !local_candidate
            || released
            || !active
            || actual_remote_controlled != remote_controlled
        {
            self.log(&format!(
                "event=source_return_presentation_rejected id={id} route={route} ready={ready} local_candidate={local_candidate} active={active} released={released} remote_controlled={actual_remote_controlled} expected_remote_controlled={remote_controlled}"
            ));
            return false;
        }
        self.source_return_handoff.store(false, Ordering::Release);
        self.source_return_remote_controlled
            .store(remote_controlled, Ordering::Release);
        self.source_return_requested.store(true, Ordering::Release);
        self.log(&format!(
            "event=source_return_presentation_requested id={id} route={route}"
        ));
        if ready {
            // READY and the control-return acknowledgement travel through independent event
            // paths. If READY won that race, no later callback would otherwise activate the
            // rearmed peer target and the next boundary crossing would lose its drag image.
            // A later READY may deliberately send ACTIVATE again; the target transition is
            // idempotent and that second send closes the opposite ordering of the same race.
            self.log(&format!(
                "event=source_return_target_activate id={id} reason=ready_before_request"
            ));
            self.send(id, DragMessage::ACTIVATE, 0);
        }
        true
    }

    /// Ends only the source-side presentation before the same live offer crosses to the peer
    /// again. The retained data object and transfer server deliberately remain alive.
    pub(crate) fn suspend_source_return_presentation(&self) -> bool {
        if !self.source_return_requested.load(Ordering::Acquire)
            && !self.source_return_presenting.load(Ordering::Acquire)
        {
            return false;
        }
        let id = self.source_id.load(Ordering::Acquire);
        self.source_return_handoff.store(true, Ordering::Release);
        self.remote_rearm_on_ready.store(
            self.source_remote_controlled.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.source_return_requested.store(false, Ordering::Release);
        self.log(&format!(
            "event=source_return_presentation_suspended id={id}"
        ));
        true
    }

    fn take_source_return_presentation(&self) -> Option<u64> {
        if !self.source_return_requested.swap(false, Ordering::AcqRel) {
            return None;
        }
        let id = self.source_id.load(Ordering::Acquire);
        if id == 0
            || !self.source_can_handoff()
            || !self.source_active.load(Ordering::Acquire)
            || self.source_remote_controlled.load(Ordering::Acquire)
                != self.source_return_remote_controlled.load(Ordering::Acquire)
            || self.source_return_handoff.load(Ordering::Acquire)
        {
            return None;
        }
        self.source_return_presenting.store(true, Ordering::Release);
        Some(id)
    }

    fn finish_source_return_presentation(&self, id: u64) {
        self.source_return_presenting
            .store(false, Ordering::Release);
        let source_is_current = self.source_id.load(Ordering::Acquire) == id;
        if self.source_return_handoff.swap(false, Ordering::AcqRel) && source_is_current {
            self.log(&format!(
                "event=source_return_presentation_ended id={id} reason=peer_handoff"
            ));
            return;
        }
        if source_is_current {
            self.cancel_with_reason("source_return_presentation_ended_locally");
        } else {
            self.log(&format!(
                "event=source_return_presentation_ended id={id} reason=source_terminal"
            ));
        }
    }

    pub(crate) fn source_dragging(&self) -> bool {
        self.source_active.load(Ordering::Acquire) && !self.source_released.load(Ordering::Acquire)
    }

    pub(crate) fn returned_source_pending(&self) -> bool {
        self.source_return_requested.load(Ordering::Acquire)
            || self.source_return_presenting.load(Ordering::Acquire)
    }

    // OLE's key state includes synthetic release-all events used for input handoff.
    // Only the physical hook / authenticated input stream can end a returned drag.
    pub(crate) fn release_returned_source(&self) {
        if self.returned_source_pending() && !self.source_return_handoff.load(Ordering::Acquire) {
            self.source_return_drop_requested
                .store(true, Ordering::Release);
            self.log(&format!(
                "event=source_return_real_release id={}",
                self.source_id.load(Ordering::Acquire)
            ));
        }
    }

    pub(crate) fn source_can_handoff(&self) -> bool {
        self.source_ready.load(Ordering::Acquire)
            && self.local_candidate.load(Ordering::Acquire)
            && !self.source_released.load(Ordering::Acquire)
    }

    pub(crate) fn observe_remote_source_left_event(&self, left_down: bool) {
        // The physical release also causes the reverse target to send an ordered drag RELEASE.
        // The ordinary input frame can win that cross-thread race by a few milliseconds, so it
        // is diagnostic only: cancelling here used to tear down the source before the legitimate
        // RELEASE/FINISHED pair arrived and made the first drop intermittently disappear.
        let mut pending = self
            .remote_source_left_up
            .lock()
            .expect("remote source release lock");
        if left_down {
            pending.take();
            return;
        }
        if self.returned_source_pending() {
            self.release_returned_source();
            return;
        }
        let id = self.source_id.load(Ordering::Acquire);
        if self.source_remote_controlled.load(Ordering::Acquire) && id != 0 {
            *pending = Some((id, Instant::now()));
            drop(pending);
            self.log(&format!(
                "event=remote_source_left_up_deferred id={id} awaiting=drag_protocol_terminal"
            ));
        }
    }

    /// A remotely controlled source whose offer was activated for the peer's reverse target no
    /// longer owns the drag terminal once control leaves this computer. The release injected by
    /// that control handoff is not the user's drop and must not start the cancellation grace.
    pub(crate) fn discard_handed_off_remote_left_up(&self) {
        if !self.source_remote_controlled.load(Ordering::Acquire)
            || !self.source_active.load(Ordering::Acquire)
            || self.returned_source_pending()
        {
            return;
        }
        let discarded = self
            .remote_source_left_up
            .lock()
            .expect("remote source release lock")
            .take();
        if let Some((id, _)) = discarded {
            self.log(&format!(
                "event=remote_source_left_up_handed_off id={id} owner=reverse_target"
            ));
        }
    }

    pub(crate) fn expire_remote_source_left_up(&self) {
        let mut pending = self
            .remote_source_left_up
            .lock()
            .expect("remote source release lock");
        let Some((id, started)) = *pending else {
            return;
        };
        if id != self.source_id.load(Ordering::Acquire) {
            pending.take();
            return;
        }
        if started.elapsed() < REMOTE_SOURCE_RELEASE_GRACE {
            return;
        }
        pending.take();
        drop(pending);
        self.cancel_with_reason("remote_left_up_without_drag_terminal");
    }

    pub(crate) fn release_source(&self) {
        let id = self.source_id.load(Ordering::Acquire);
        if id != 0 && !self.source_released.swap(true, Ordering::AcqRel) {
            self.log(&format!(
                "event=source_release id={id} ready={} active={} boundary_visible={}",
                self.source_ready.load(Ordering::Acquire),
                self.source_active.load(Ordering::Acquire),
                self.boundary_visible.load(Ordering::Acquire),
            ));
            self.send(id, DragMessage::RELEASE, 0);
        }
    }

    pub(crate) fn target_pending(&self) -> bool {
        self.target_id.load(Ordering::Acquire) != 0
            && self.target_state.load(Ordering::Acquire) < TRANSFERRING
    }

    pub(crate) fn target_debug_id(&self) -> u64 {
        self.target_id.load(Ordering::Acquire)
    }

    /// Ends only the receiver-side synthetic OLE loop while a held source drag returns to its
    /// original computer. The authenticated source offer deliberately remains alive so the same
    /// `IDataObject` can be presented again if the pointer crosses back to this computer.
    pub(crate) fn suspend_target_for_source_return(&self) -> bool {
        self.suspend_target_presentation(false, "source_return")
    }

    /// Stops the reverse synthetic target while control goes back to the remote source
    /// computer. The offer is kept so the same target can be armed again on the next crossing.
    pub(crate) fn suspend_reverse_target_for_source_reentry(&self) -> bool {
        self.suspend_target_presentation(true, "controlled_source_reentry")
    }

    fn suspend_target_presentation(&self, reverse: bool, route: &str) -> bool {
        if self.target_id.load(Ordering::Acquire) == 0
            || self.target_reverse.load(Ordering::Acquire) != reverse
        {
            return false;
        }
        if reverse {
            self.target_prepared.store(false, Ordering::Release);
            self.target_armed.store(false, Ordering::Release);
            self.reverse_arrival_pending.store(false, Ordering::Release);
        }
        {
            let mut pending = self
                .target_return_pending
                .lock()
                .expect("target return lock");
            pending.returning = true;
            pending.state = PREPARING;
            self.target_returning.store(true, Ordering::Release);
        }
        loop {
            let state = self.target_state.load(Ordering::Acquire);
            if state >= RELEASED {
                *self
                    .target_return_pending
                    .lock()
                    .expect("target return lock") = TargetReturnPending::default();
                self.target_returning.store(false, Ordering::Release);
                return false;
            }
            if self
                .target_state
                .compare_exchange(state, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.target_control_arrived.store(false, Ordering::Release);
                self.log(&format!(
                    "event=target_suspend_for_source_return id={} previous_state={state} route={route}",
                    self.target_id.load(Ordering::Acquire),
                ));
                return true;
            }
        }
    }

    pub(crate) fn returned_target_pending(&self) -> bool {
        self.target_id.load(Ordering::Acquire) != 0
            && !self.target_reverse.load(Ordering::Acquire)
            && (self.target_returning.load(Ordering::Acquire)
                || self.target_state.load(Ordering::Acquire) == PREPARING
                || (self.target_state.load(Ordering::Acquire) == ACTIVE
                    && !self.target_control_arrived.load(Ordering::Acquire)))
    }

    /// ACTIVATE can rearm the offer while the pointer is still on the source PC.
    /// Only the control-enter path, after positioning the cursor, permits native hit-testing.
    pub(crate) fn target_control_arrived(&self) {
        self.target_control_arrived.store(true, Ordering::Release);
    }

    fn target_waiting_for_activation(&self) -> bool {
        let state = self.target_state.load(Ordering::Acquire);
        state == PREPARING
            || ((state == ACTIVE || state == RELEASED)
                && !self.target_reverse.load(Ordering::Acquire)
                && !self.target_control_arrived.load(Ordering::Acquire))
    }

    fn begin_target_presentation(&self) -> bool {
        let mut pending = self
            .target_return_pending
            .lock()
            .expect("target return lock");
        if !pending.returning {
            return true;
        }
        let state = pending.state;
        *pending = TargetReturnPending::default();
        self.target_state.store(state, Ordering::Release);
        self.target_returning.store(false, Ordering::Release);
        drop(pending);
        self.log(&format!(
            "event=target_return_messages_applied id={} state={state}",
            self.target_id.load(Ordering::Acquire),
        ));
        state != CANCELLED
    }

    pub(crate) fn reverse_handoff_ready(&self) -> bool {
        self.target_pending()
            && self.target_reverse.load(Ordering::Acquire)
            && self.target_prepared.load(Ordering::Acquire)
            && self.target_armed.load(Ordering::Acquire)
            && self.target_state.load(Ordering::Acquire) == PREPARING
    }

    fn active_reverse_target(&self) -> bool {
        self.target_pending()
            && self.target_reverse.load(Ordering::Acquire)
            && self.target_prepared.load(Ordering::Acquire)
            && self.target_armed.load(Ordering::Acquire)
            && !self.target_returning.load(Ordering::Acquire)
            && self.target_state.load(Ordering::Acquire) == ACTIVE
    }

    fn arm_reverse_target(&self, id: u64) {
        if self.target_state.load(Ordering::Acquire) != PREPARING {
            return;
        }
        self.target_armed.store(true, Ordering::Release);
        if self.reverse_arrival_pending.swap(false, Ordering::AcqRel) {
            self.activate_local_target();
            self.log(&format!(
                "event=reverse_target_arrival_completed id={id} reason=activate_after_control"
            ));
        }
    }

    pub(crate) fn reverse_target_pending(&self) -> bool {
        self.target_pending() && self.target_reverse.load(Ordering::Acquire)
    }

    /// Control has returned to the computer hosting a reverse receiver. ACTIVATE travels on the
    /// file-drag channel and can trail the control commit, so remember the arrival and complete
    /// the activation when it lands.
    pub(crate) fn reverse_control_arrived(&self, left_down: bool) {
        if !self.reverse_target_pending() {
            return;
        }
        let id = self.target_id.load(Ordering::Acquire);
        if self.target_returning.load(Ordering::Acquire) {
            // The previous presentation is still leaving its OLE loop. Dropping this arrival left
            // the target unarmed, and the next crossing was blocked as a held pointer without a
            // drag (2026-10-11 21:49). The re-armed target activates when ACTIVATE arrives.
            if left_down {
                self.reverse_arrival_pending.store(true, Ordering::Release);
                self.log(&format!(
                    "event=reverse_target_arrival_deferred id={id} awaiting=presentation_resume"
                ));
            }
            return;
        }
        if self.reverse_handoff_ready() {
            self.complete_local_target_handoff(left_down);
            self.log(&format!(
                "event=reverse_target_arrival_completed id={id} reason=control_arrived left_down={left_down}"
            ));
        } else if left_down && self.target_state.load(Ordering::Acquire) == PREPARING {
            self.reverse_arrival_pending.store(true, Ordering::Release);
            self.log(&format!(
                "event=reverse_target_arrival_deferred id={id} awaiting=activate"
            ));
        } else if !left_down {
            let _ = self.release_local_target();
        }
    }

    /// A drag that began on this computer while it was controlled by a peer must be handed to the
    /// peer's reverse receiver before the held pointer returns there. Returns `None` when this is
    /// not such a drag.
    fn remote_source_return_gate(&self, left: bool) -> Option<bool> {
        if !left
            || !self.source_remote_controlled.load(Ordering::Acquire)
            || self.source_id.load(Ordering::Acquire) == 0
            || !self.local_candidate.load(Ordering::Acquire)
            || self.source_released.load(Ordering::Acquire)
            || self.source_active.load(Ordering::Acquire)
        {
            return None;
        }
        if !self.source_ready.load(Ordering::Acquire) {
            let mut wait = self.edge_wait.lock().expect("edge wait lock");
            if wait.get_or_insert_with(Instant::now).elapsed() > SOURCE_PREPARE_TIMEOUT {
                *wait = None;
                drop(wait);
                self.cancel_with_reason("remote_source_prepare_timeout");
            }
            return Some(false);
        }
        *self.edge_wait.lock().expect("edge wait lock") = None;
        // ACTIVATE arms the reverse receiver while it is still PREPARING; the receiver starts its
        // native drag only after control arrives there.
        self.activate_source();
        Some(self.source_active.load(Ordering::Acquire))
    }

    pub(crate) fn may_return(&self, left: bool) -> bool {
        if let Some(allowed) = self.remote_source_return_gate(left) {
            return allowed;
        }
        let mut candidate = self.peer_candidate.lock().expect("candidate lock");
        let timed_out_candidate = if let Some((id, started, reverse)) = *candidate {
            if started.elapsed() < SOURCE_PREPARE_TIMEOUT {
                return false;
            }
            self.send(id, DragMessage::CANCEL, 0);
            candidate.take();
            Some((id, reverse))
        } else {
            None
        };
        drop(candidate);
        if let Some((id, true)) = timed_out_candidate {
            self.mark_failed_reverse_target(id, "candidate_timeout");
        }
        if left
            && self
                .suppress_failed_reverse_left_restore
                .load(Ordering::Acquire)
        {
            // Preparation failed while Explorer still owns the original drag. Keep the
            // pointer on its source until release instead of crossing with no file offer.
            return false;
        }
        if self.target_pending()
            && !self.target_reverse.load(Ordering::Acquire)
            && self.target_state.load(Ordering::Acquire) == ACTIVE
        {
            // A normal synthetic target is the receiver half of the original drag. The held
            // pointer must be allowed to cross back to its source; the control-enter path then
            // suspends only this presentation while retaining the authenticated file offer.
            return true;
        }
        if self.target_pending() {
            return self.reverse_handoff_ready();
        }
        let mut wait = self.return_wait.lock().expect("return wait");
        if !left {
            *wait = None;
            return true;
        }
        wait.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(200)
    }

    pub(crate) fn reset_return_wait(&self) {
        *self.return_wait.lock().expect("return wait") = None;
    }

    pub(crate) fn activate_local_target(&self) {
        if self.reverse_handoff_ready() {
            self.target_state.store(ACTIVE, Ordering::Release);
            self.log("event=reverse_target_activated");
        }
    }

    pub(crate) fn complete_local_target_handoff(&self, left_down: bool) {
        self.activate_local_target();
        if !left_down {
            self.release_local_target();
        }
    }

    pub(crate) fn release_local_target(&self) -> bool {
        // While the pointer is back on the remote source, its native OLE loop owns
        // the release. A suspended receiver must not send CANCEL ahead of that input.
        if self.target_returning.load(Ordering::Acquire) {
            return false;
        }
        if !self.target_reverse.load(Ordering::Acquire) || !self.target_pending() {
            return false;
        }
        if self.target_state.load(Ordering::Acquire) == RELEASED {
            return true;
        }
        if self
            .target_state
            .compare_exchange(ACTIVE, RELEASED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.send(
                self.target_id.load(Ordering::Acquire),
                DragMessage::RELEASE,
                0,
            );
            return true;
        }
        self.cancel_with_reason("reverse_target_release_before_activation");
        true
    }

    pub(crate) fn may_cross(&self, left_down: bool) -> bool {
        if !left_down {
            *self.edge_wait.lock().expect("edge wait lock") = None;
            if !self.source_active.load(Ordering::Acquire)
                && self.local_candidate.load(Ordering::Acquire)
            {
                self.cancel_with_reason("left_released_before_handoff");
            }
            self.source_gate_logged.store(false, Ordering::Release);
            return true;
        }
        if self.stop.load(Ordering::Acquire) {
            return false;
        }
        if self.active_reverse_target() {
            // A remotely controlled source has already crossed back to this computer and is now
            // represented by our synthetic OLE target.  It is a proven file drag even though this
            // computer has no local source IDataObject.  Permit the still-held pointer to enter
            // the source computer again; the enter acknowledgement suspends this target and
            // re-presents the retained source object there.
            *self.edge_wait.lock().expect("edge wait lock") = None;
            self.source_gate_logged.store(false, Ordering::Release);
            return true;
        }
        if self.source_released.load(Ordering::Acquire) {
            self.source_gate_logged.store(false, Ordering::Release);
            return true;
        }
        if self.local_candidate.load(Ordering::Acquire) {
            if self
                .edge_wait
                .lock()
                .expect("edge wait lock")
                .is_some_and(|started| started.elapsed() > SOURCE_PREPARE_TIMEOUT)
            {
                self.cancel_with_reason("source_prepare_timeout");
                return false;
            }
            let ready = self.source_ready.load(Ordering::Acquire);
            if !ready && !self.source_gate_logged.swap(true, Ordering::AcqRel) {
                self.log(&format!(
                    "event=cross_gate decision=wait_for_ready id={} boundary_visible={} remote_controlled={}",
                    self.source_id.load(Ordering::Acquire),
                    self.boundary_visible.load(Ordering::Acquire),
                    self.source_remote_controlled.load(Ordering::Acquire),
                ));
            }
            if ready {
                self.source_gate_logged.store(false, Ordering::Release);
            }
            return ready;
        }
        self.edge_wait
            .lock()
            .expect("edge wait lock")
            .get_or_insert_with(Instant::now);
        if !self.source_gate_logged.swap(true, Ordering::AcqRel) {
            self.log(&format!(
                "event=cross_gate decision=block_held_pointer_without_drag source_id={} local_candidate=false boundary_visible={} remote_controlled={}",
                self.source_id.load(Ordering::Acquire),
                self.boundary_visible.load(Ordering::Acquire),
                self.source_remote_controlled.load(Ordering::Acquire),
            ));
        }
        false
    }

    fn log(&self, text: &str) {
        if self.log_path.as_os_str().is_empty() {
            return;
        }
        let _ = crate::logging::append(&self.log_path, text);
        if let Some(root) = self.log_path.parent() {
            let _ = crate::logging::append(
                &root.join("logs/clipferry.log"),
                &format!("file_drag pid={} {text}", std::process::id()),
            );
        }
    }
}

pub(crate) struct DragWorker(pub Arc<DragBridge>, pub Option<JoinHandle<()>>);
impl Drop for DragWorker {
    fn drop(&mut self) {
        self.0.stop();
        if let Some(worker) = self.1.take() {
            let _ = worker.join();
        }
    }
}

struct Apartment;
impl Apartment {
    fn new() -> io::Result<Self> {
        unsafe { OleInitialize(None) }.map_err(io::Error::other)?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

#[implement(IDropTarget)]
struct EdgeTarget {
    bridge: Arc<DragBridge>,
    helper: IDropTargetHelper,
    window: Rc<Cell<windows::Win32::Foundation::HWND>>,
    retained_source: Rc<RefCell<Option<RetainedSourceObject>>>,
}
impl IDropTarget_Impl for EdgeTarget_Impl {
    fn DragEnter(
        &self,
        object: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        catch_com_result(|| {
            if effect.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            // Reject Move even if Shift is held. This target only stages a remote copy.
            unsafe {
                effect.write(DROPEFFECT_NONE);
            }
            if self.bridge.source_return_presenting.load(Ordering::Acquire)
                && self.bridge.source_id.load(Ordering::Acquire) != 0
            {
                self.bridge.log(&format!(
                    "event=source_return_boundary_reenter id={} x={} y={}",
                    self.bridge.source_id.load(Ordering::Acquire),
                    point.x,
                    point.y,
                ));
                unsafe {
                    effect.write(DROPEFFECT_COPY);
                    let point = POINT {
                        x: point.x,
                        y: point.y,
                    };
                    if let Some(object) = object.cloned() {
                        let _ = self.helper.DragEnter(
                            self.window.get(),
                            &object,
                            &raw const point,
                            DROPEFFECT_COPY,
                        );
                    }
                }
                return Ok(());
            }
            if self.bridge.target_pending() || self.bridge.source_id.load(Ordering::Acquire) != 0 {
                return Ok(());
            }
            let object = object
                .cloned()
                .ok_or_else(|| Error::from_hresult(E_POINTER))?;
            match real_paths(&object) {
                Ok(paths) => {
                    let id = self.bridge.next_id.fetch_add(1, Ordering::AcqRel);
                    self.bridge.begin_source_capture(id);
                    self.bridge.log(&format!(
                        "event=drag_enter id={id} x={} y={} paths={} boundary_visible={} remote_controlled={}",
                        point.x,
                        point.y,
                        paths.len(),
                        self.bridge.boundary_visible.load(Ordering::Acquire),
                        self.bridge.source_remote_controlled.load(Ordering::Acquire),
                    ));
                    self.bridge.send(id, DragMessage::CANDIDATE, 0);
                    *self.retained_source.borrow_mut() = Some(RetainedSourceObject {
                        id,
                        _object: object.clone(),
                        image: Vec::new(),
                    });
                    *self.bridge.captured.lock().expect("drag capture lock") = Some(paths);
                    unsafe {
                        effect.write(DROPEFFECT_COPY);
                        let point = POINT {
                            x: point.x,
                            y: point.y,
                        };
                        let _ = self.helper.DragEnter(
                            self.window.get(),
                            &object,
                            &raw const point,
                            DROPEFFECT_COPY,
                        );
                    }
                }
                Err(error) => self.bridge.log(&format!(
                    "event=capture_rejected capture=in_process reason={error} {}",
                    drag_source_diagnostic(&object)
                )),
            }
            Ok(())
        })
    }
    fn DragOver(
        &self,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        if effect.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        unsafe {
            effect.write(if self.bridge.local_candidate.load(Ordering::Acquire) {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            });
            let point = POINT {
                x: point.x,
                y: point.y,
            };
            let _ = self.helper.DragOver(&raw const point, effect.read());
        }
        Ok(())
    }
    fn DragLeave(&self) -> Result<()> {
        let _ = unsafe { self.helper.DragLeave() };
        let active = self.bridge.source_active.load(Ordering::Acquire);
        let boundary_visible = self.bridge.boundary_visible.load(Ordering::Acquire);
        let deferred = !active && boundary_visible;
        let point = cursor_position();
        self.bridge.log(&format!(
            "event=drag_leave id={} ready={} active={active} boundary_visible={boundary_visible} deferred={deferred} cursor={},{}",
            self.bridge.source_id.load(Ordering::Acquire),
            self.bridge.source_ready.load(Ordering::Acquire),
            point.x,
            point.y,
        ));
        // OLE can briefly hand the cursor back to the taskbar while the remote virtual object is
        // still being prepared. Keep this candidate for the lifetime of the held left button;
        // may_cross(false) cancels it as soon as the physical drag actually ends.
        if should_cancel_after_drag_leave(active, boundary_visible) {
            self.bridge
                .cancel_with_reason("drag_leave_after_button_release");
        }
        Ok(())
    }
    fn Drop(
        &self,
        _: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        if effect.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        unsafe {
            effect.write(DROPEFFECT_NONE);
            let _ = self.helper.DragLeave();
        }
        self.bridge.log(&format!(
            "event=boundary_drop id={} x={} y={} ready={} active={} boundary_visible={}",
            self.bridge.source_id.load(Ordering::Acquire),
            point.x,
            point.y,
            self.bridge.source_ready.load(Ordering::Acquire),
            self.bridge.source_active.load(Ordering::Acquire),
            self.bridge.boundary_visible.load(Ordering::Acquire),
        ));
        if !self.bridge.source_active.load(Ordering::Acquire) {
            self.bridge
                .cancel_with_reason("boundary_drop_before_handoff");
        }
        Ok(())
    }
}

struct RetainedSourceObject {
    id: u64,
    _object: IDataObject,
    image: Vec<u8>,
}

pub(super) fn real_paths(object: &IDataObject) -> io::Result<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    struct Medium(STGMEDIUM);
    impl Drop for Medium {
        fn drop(&mut self) {
            unsafe {
                ReleaseStgMedium(&raw mut self.0);
            }
        }
    }
    let format = format_etc(CF_HDROP.0, -1, TYMED_HGLOBAL.0.cast_unsigned());
    let medium = unsafe { object.GetData(&raw const format) }.map_err(|error| {
        io::Error::other(format!(
            "stage=get_cf_hdrop hresult={:#010X} error={error}",
            error.code().0.cast_unsigned()
        ))
    })?;
    let medium = Medium(medium);
    if medium.0.tymed != TYMED_HGLOBAL.0.cast_unsigned() {
        return Err(io::Error::other("unsupported drag medium"));
    }
    let hdrop = HDROP(unsafe { medium.0.u.hGlobal }.0);
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    if count == 0 || count > 4_096 {
        return Err(io::Error::other("invalid drag path count"));
    }
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let length = unsafe { DragQueryFileW(hdrop, index, None) };
        if length == 0 || length > 32767 {
            return Err(io::Error::other("invalid drag path length"));
        }
        let mut path = vec![0; length as usize + 1];
        if unsafe { DragQueryFileW(hdrop, index, Some(&mut path)) } != length {
            return Err(io::Error::other("truncated drag path"));
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]));
        if !path.is_absolute() || (!path.is_file() && !path.is_dir()) {
            return Err(io::Error::other(
                "drag item must be a real file or directory",
            ));
        }
        paths.push(path);
    }
    Ok(paths)
}

const MAX_DRAG_FORMAT_DIAGNOSTICS: usize = 24;
const MAX_DRAG_DIAGNOSTIC_BYTES: usize = 4 * 1024;

pub(super) fn drag_source_diagnostic(object: &IDataObject) -> String {
    let hdrop = query_drag_format(object, CF_HDROP.0, -1, TYMED_HGLOBAL.0.cast_unsigned());
    let descriptor = register_and_query_drag_format(
        object,
        CFSTR_FILEDESCRIPTORW,
        -1,
        TYMED_HGLOBAL.0.cast_unsigned(),
    );
    let contents = register_and_query_drag_format(
        object,
        CFSTR_FILECONTENTS,
        0,
        TYMED_ISTREAM.0.cast_unsigned(),
    );
    let mut diagnostic = format!(
        "hdrop_query={hdrop:#010X} file_descriptor_w_query={descriptor} file_contents_0_query={contents}"
    );
    match unsafe { object.EnumFormatEtc(DATADIR_GET.0.cast_unsigned()) } {
        Ok(enumerator) => {
            let mut formats = Vec::new();
            let mut truncated = false;
            for _ in 0..MAX_DRAG_FORMAT_DIAGNOSTICS {
                let mut format = FORMATETC::default();
                let mut fetched = 0;
                let result = unsafe {
                    enumerator.Next(std::slice::from_mut(&mut format), Some(&raw mut fetched))
                };
                if fetched == 0 {
                    if result != S_OK {
                        formats.push(format!("enum_end={:#010X}", result.0.cast_unsigned()));
                    }
                    break;
                }
                formats.push(format!(
                    "{}#{}:tymed=0x{:X}:aspect={}:index={}",
                    drag_format_name(format.cfFormat),
                    format.cfFormat,
                    format.tymed,
                    format.dwAspect,
                    format.lindex
                ));
                if !format.ptd.is_null() {
                    unsafe { CoTaskMemFree(Some(format.ptd.cast())) };
                }
                if formats.len() == MAX_DRAG_FORMAT_DIAGNOSTICS {
                    truncated = true;
                }
            }
            diagnostic.push_str(" formats=[");
            diagnostic.push_str(&formats.join("|"));
            if truncated {
                diagnostic.push_str("|truncated");
            }
            diagnostic.push(']');
        }
        Err(error) => {
            let _ = write!(
                diagnostic,
                " enum_error={:#010X}",
                error.code().0.cast_unsigned()
            );
        }
    }
    truncate_utf8(&mut diagnostic, MAX_DRAG_DIAGNOSTIC_BYTES);
    diagnostic
}

fn query_drag_format(object: &IDataObject, format: u16, index: i32, medium: u32) -> u32 {
    let format = format_etc(format, index, medium);
    unsafe { object.QueryGetData(&raw const format) }
        .0
        .cast_unsigned()
}

fn register_and_query_drag_format(
    object: &IDataObject,
    name: PCWSTR,
    index: i32,
    medium: u32,
) -> String {
    let registered = unsafe { RegisterClipboardFormatW(name) };
    if registered == 0 {
        return "register_failed".to_owned();
    }
    u16::try_from(registered).map_or_else(
        |_| "register_failed".to_owned(),
        |format| format!("{:#010X}", query_drag_format(object, format, index, medium)),
    )
}

fn drag_format_name(format: u16) -> String {
    if format == CF_HDROP.0 {
        return "CF_HDROP".to_owned();
    }
    let mut name = [0_u16; 128];
    let length = unsafe { GetClipboardFormatNameW(u32::from(format), &mut name) };
    if length <= 0 {
        return "standard".to_owned();
    }
    let length = usize::try_from(length).unwrap_or(0).min(name.len());
    String::from_utf16_lossy(&name[..length]).replace([' ', '\t', '\r', '\n'], "_")
}

fn truncate_utf8(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit.saturating_sub("[truncated]".len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.truncate(end);
    text.push_str("[truncated]");
}

pub(super) fn pump_messages() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

pub(super) fn window_bounds(window: HWND) -> [i32; 4] {
    let mut bounds = RECT::default();
    let _ = unsafe { GetWindowRect(window, &raw mut bounds) };
    [
        bounds.left,
        bounds.top,
        bounds.right.saturating_sub(bounds.left),
        bounds.bottom.saturating_sub(bounds.top),
    ]
}

const fn capture_thickness(edge: DragProbeEdge) -> u32 {
    match edge {
        DragProbeEdge::Bottom => FILE_DRAG_TASKBAR_THICKNESS,
        DragProbeEdge::Left | DragProbeEdge::Right | DragProbeEdge::Top => FILE_DRAG_EDGE_THICKNESS,
    }
}

fn cursor_position() -> POINT {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&raw mut point);
    }
    point
}

/// Keeps a motion-gated boundary window hidden until the press has really moved.
/// Latched for the rest of the press, so a drag that returns near its start point stays open.
#[derive(Default)]
struct BoundaryMotionArm {
    press: Option<(i32, i32)>,
    armed: bool,
}

impl BoundaryMotionArm {
    fn update(&mut self, requested: bool, needs_motion: bool, point: (i32, i32)) -> bool {
        if !requested {
            *self = Self::default();
            return false;
        }
        let press = *self.press.get_or_insert(point);
        if !needs_motion {
            self.armed = true;
        } else if !self.armed {
            let dx = point.0 - press.0;
            let dy = point.1 - press.1;
            self.armed = dx * dx + dy * dy >= BOUNDARY_ARM_DISTANCE_PX * BOUNDARY_ARM_DISTANCE_PX;
        }
        self.armed
    }
}

const fn should_cancel_after_drag_leave(source_active: bool, boundary_visible: bool) -> bool {
    !source_active && !boundary_visible
}

struct LiveSourceOffer {
    server: SecureOfferServer,
    _tunnel: Option<ReverseOfferTunnel>,
    task: Arc<DragSourceUiTask>,
}

#[allow(clippy::too_many_lines)]
fn run_sta(
    bridge: &Arc<DragBridge>,
    store: &DeviceStore,
    peer: CertificateFingerprint,
    _local: SocketAddr,
    remote: SocketAddr,
    boundary: ScreenBoundary,
) -> io::Result<()> {
    let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
    let _ole = Apartment::new()?;
    let edge = match boundary {
        ScreenBoundary::Left => DragProbeEdge::Left,
        ScreenBoundary::Right => DragProbeEdge::Right,
        ScreenBoundary::Top => DragProbeEdge::Top,
        ScreenBoundary::Bottom => DragProbeEdge::Bottom,
    };
    let elevated = unsafe { IsUserAnAdmin() }.as_bool();
    let thickness = capture_thickness(edge);
    let window_handle = Rc::new(Cell::new(windows::Win32::Foundation::HWND::default()));
    let retained_source = Rc::new(RefCell::new(None));
    let mut capture_helper = if elevated {
        match DragCaptureHelper::start(store.root(), edge, thickness) {
            Ok(helper) => Some(helper),
            Err(error) => {
                // A missing desktop helper must not terminate the entire file-drag worker. The
                // in-process target can still serve elevated Explorer windows and gives the user a
                // useful fallback while keeping future reconnects alive.
                bridge.log(&format!(
                    "event=capture_helper_failed action=in_process_fallback error={error}"
                ));
                None
            }
        }
    } else {
        None
    };
    let window = if capture_helper.is_none() {
        let helper: IDropTargetHelper =
            unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }
                .map_err(io::Error::other)?;
        let target: IDropTarget = EdgeTarget {
            bridge: Arc::clone(bridge),
            helper,
            window: Rc::clone(&window_handle),
            retained_source: Rc::clone(&retained_source),
        }
        .into();
        // Only the bottom edge needs to cover a scaled taskbar. The other edges stay narrow so a
        // browser can rearrange tabs and favorites without an invisible window covering its UI.
        let window =
            RegisteredBoundaryWindow::create_with_visibility(edge, thickness, &target, false)?;
        window_handle.set(window.handle);
        unsafe {
            let _ = ShowWindow(window.handle, SW_HIDE);
        }
        Some(window)
    } else {
        None
    };
    let bounds = capture_helper.as_ref().map_or_else(
        || window_bounds(window.as_ref().expect("local drag window").handle),
        |helper| helper.bounds,
    );
    let point = cursor_position();
    let mut visible = false;
    let mut arm = BoundaryMotionArm::default();
    let mut helper_source_id = 0;
    let mut offers: Vec<LiveSourceOffer> = Vec::new();
    bridge.log(&format!(
        "event=ready mode=file_tree_copy boundary={} elevated={} capture={} helper_pid={} helper_elevated={} window={},{},{},{} cursor={},{} helper_startup_flags={:#x}",
        boundary.label(),
        elevated,
        if capture_helper.is_some() {
            "desktop_shell_helper"
        } else if elevated {
            "in_process_fallback"
        } else {
            "in_process"
        },
        capture_helper.as_ref().map_or(0, |helper| helper.process_id),
        capture_helper.as_ref().is_some_and(|helper| helper.elevated),
        bounds[0],
        bounds[1],
        bounds[2],
        bounds[3],
        point.x,
        point.y,
        capture_helper.as_ref().map_or(0, |helper| helper.startup_flags),
    ));
    while !bridge.stop.load(Ordering::Acquire) {
        pump_messages();
        if let Some(helper) = capture_helper.as_mut() {
            while let Some(event) = helper.poll()? {
                match event {
                    DragCaptureEvent::Paths(paths) => bridge.capture_helper_paths(paths),
                    DragCaptureEvent::Rejected(reason) => bridge.log(&format!(
                        "event=capture_rejected capture=desktop_shell_helper reason={reason}"
                    )),
                }
            }
            let source_id = bridge.source_id.load(Ordering::Acquire);
            if helper_source_id != 0 && source_id == 0 {
                helper.clear()?;
            }
            helper_source_id = source_id;
        }
        let requested = bridge.boundary_visible.load(Ordering::Acquire);
        let needs_motion = bridge.boundary_needs_motion.load(Ordering::Acquire);
        let cursor = cursor_position();
        let show =
            arm.update(requested, needs_motion, (cursor.x, cursor.y)) && !bridge.target_pending();
        if show != visible {
            if let Some(helper) = capture_helper.as_mut() {
                helper.set_visible(show)?;
            } else if let Some(window) = &window {
                unsafe {
                    if show {
                        let _ = SetWindowPos(
                            window.handle,
                            Some(HWND_TOPMOST),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        );
                    } else {
                        let _ = ShowWindow(window.handle, SW_HIDE);
                    }
                }
            }
            visible = show;
            if VERBOSE_BOUNDARY_WINDOW_DIAGNOSTICS {
                let point = cursor_position();
                bridge.log(&format!(
                    "event=boundary_window visible={show} cursor={},{} source_id={} ready={} active={} local_candidate={} target_id={} target_state={}",
                    point.x,
                    point.y,
                    bridge.source_id.load(Ordering::Acquire),
                    bridge.source_ready.load(Ordering::Acquire),
                    bridge.source_active.load(Ordering::Acquire),
                    bridge.local_candidate.load(Ordering::Acquire),
                    bridge.target_id.load(Ordering::Acquire),
                    bridge.target_state.load(Ordering::Acquire),
                ));
            }
        }
        if let Some(id) = bridge.take_source_return_presentation() {
            let object = bridge
                .source_image
                .lock()
                .expect("source image lock")
                .as_ref()
                .filter(|(source_id, _)| *source_id == id)
                .map(|(_, image)| image.clone());
            if let Some(image) = object {
                present_returned_source(bridge, &image, id);
            } else {
                bridge.log(&format!("event=source_return_presentation_missing id={id}"));
                bridge.finish_source_return_presentation(id);
            }
        }
        let current_source_id = bridge.source_id.load(Ordering::Acquire);
        let source_released = bridge.source_released.load(Ordering::Acquire);
        let release_retained = retained_source.borrow().as_ref().is_some_and(|retained| {
            should_release_retained_source(
                retained.id,
                current_source_id,
                source_released,
                bridge.source_return_presenting.load(Ordering::Acquire),
            )
        });
        if release_retained && let Some(retained) = retained_source.borrow_mut().take() {
            bridge.log(&format!(
                "event=source_object_released id={} reason={}",
                retained.id,
                if source_released {
                    "remote_release"
                } else {
                    "source_terminal"
                },
            ));
        }
        let paths = bridge.captured.lock().expect("drag capture lock").take();
        if let Some(paths) = paths.filter(|_| bridge.local_candidate.load(Ordering::Acquire)) {
            if offers.len() >= 4 {
                bridge.cancel_with_reason("source_offer_capacity");
                continue;
            }
            let prepared =
                (|| -> io::Result<(SecureOfferServer, ReverseOfferTunnel, u16, usize)> {
                    let tree = FileTreeSnapshot::capture(&paths)
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    let mut registry = LocalOfferRegistry::default();
                    let offer = registry
                        .publish_tree(&tree, Duration::from_mins(15))
                        .map_err(io::Error::other)?;
                    let image = super::drag_image::capture(&paths[0], paths.len())?;
                    *bridge.source_image.lock().expect("source image lock") =
                        Some((bridge.source_id.load(Ordering::Acquire), image.clone()));
                    if let Some(retained) = retained_source.borrow_mut().as_mut() {
                        retained.image.clone_from(&image);
                    }
                    bridge.log(&format!(
                        "event=source_image bytes={} source=shell_icon",
                        image.len()
                    ));
                    let offered =
                        SecureOfferedFile::from_local_offer(&offer)?.with_drag_image(image)?;
                    let manifest = offered.manifest();
                    let endpoint = SocketAddr::new(remote.ip(), crate::discovery::SERVICE_PORT);
                    let (server, tunnel, route_port) =
                        start_drag_source_offer(store, peer, endpoint, offered).inspect_err(
                            |error| report_drag_connection_failure(store, peer, endpoint, error),
                        )?;
                    clear_connection_failure(peer);
                    Ok((server, tunnel, route_port, manifest.entries.len()))
                })();
            match prepared {
                Ok((server, tunnel, route_port, item_count)) => {
                    if !bridge.local_candidate.load(Ordering::Acquire) {
                        continue;
                    }
                    let id = bridge.source_id.load(Ordering::Acquire);
                    if id == 0 {
                        continue;
                    }
                    bridge.send(id, DragMessage::PREPARE_TUNNEL, route_port);
                    bridge.log(&format!(
                        "event=prepare id={id} items={item_count} route=reverse_tunnel"
                    ));
                    let ui_task =
                        register_source_ui_task(id, &server.manifest(), server.control_handle());
                    offers.push(LiveSourceOffer {
                        server,
                        _tunnel: Some(tunnel),
                        task: ui_task,
                    });
                }
                Err(error) => {
                    bridge.log(&format!(
                        "event=prepare_failed kind={:?} error={error}",
                        error.kind()
                    ));
                    bridge.cancel_with_reason("source_prepare_failed");
                }
            }
        }
        let request = bridge
            .target_request
            .lock()
            .expect("drag request lock")
            .take();
        if let Some(request) = request {
            let tunnel = bridge.target_tunnel.load(Ordering::Acquire);
            let reverse = bridge.target_reverse.load(Ordering::Acquire);
            let target = SocketAddr::new(
                if tunnel {
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                } else {
                    remote.ip()
                },
                request.port,
            );
            bridge.log(&format!(
                "event=target_connect id={} transport={} endpoint={target}",
                request.id,
                if tunnel { "reverse_tunnel" } else { "direct" },
            ));
            let completed = match target_drag(bridge, store, peer, target, request.id) {
                Ok(completed) => completed,
                Err(error) => {
                    bridge.log(&format!(
                        "event=target_failed id={} error={error}",
                        request.id
                    ));
                    false
                }
            };
            if reverse && !completed {
                bridge.mark_failed_reverse_target(request.id, "target_presentation_failed");
            } else if reverse {
                bridge.clear_failed_reverse_left_restore();
            }
            bridge.send(
                request.id,
                if completed {
                    DragMessage::FINISHED
                } else {
                    DragMessage::CANCEL
                },
                0,
            );
            bridge.target_id.store(0, Ordering::Release);
            bridge.target_reverse.store(false, Ordering::Release);
            bridge.target_tunnel.store(false, Ordering::Release);
            bridge.target_prepared.store(false, Ordering::Release);
            bridge.target_armed.store(false, Ordering::Release);
            bridge
                .reverse_arrival_pending
                .store(false, Ordering::Release);
            bridge.target_returning.store(false, Ordering::Release);
            bridge
                .target_release_point
                .lock()
                .expect("target release point lock")
                .take();
            *bridge
                .target_return_pending
                .lock()
                .expect("target return lock") = TargetReturnPending::default();
        }
        offers.retain(|offer| {
            *offer
                .task
                .file_progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = offer.server.file_progress();
            *offer
                .task
                .metrics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = offer.server.metrics();
            offer.server.target_location().clone_into(
                &mut offer
                    .task
                    .target_location
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let terminal = offer.task.terminal.load(Ordering::Acquire) != SHELL_PENDING
                || offer.task.control.state()
                    == super::secure_transfer::RemoteTransferState::Cancelled;
            !offer.server.is_idle()
                && (!terminal || offer.task.control.idle_for() < Duration::from_secs(5))
        });
        std::thread::sleep(Duration::from_millis(8));
    }
    bridge.log("event=stopped");
    Ok(())
}

fn source_return_preview_active(bridge: &DragBridge, id: u64) -> bool {
    !bridge.stop.load(Ordering::Acquire)
        && bridge.source_id.load(Ordering::Acquire) == id
        && !bridge.source_released.load(Ordering::Acquire)
        && !bridge.source_return_handoff.load(Ordering::Acquire)
        && !bridge.source_return_drop_requested.load(Ordering::Acquire)
}

fn present_returned_source(bridge: &Arc<DragBridge>, image: &[u8], id: u64) {
    bridge.log(&format!("event=source_return_presentation_started id={id}"));
    let started = Instant::now();
    let mut preview = None;
    let mut attempt = 0_u32;
    while source_return_preview_active(bridge, id) {
        pump_messages();
        if started.elapsed() > Duration::from_mins(2) {
            bridge.log(&format!(
                "event=source_return_preview_timeout id={id} elapsed_ms={}",
                started.elapsed().as_millis()
            ));
            break;
        }

        if preview.is_none() {
            attempt = attempt.saturating_add(1);
            match super::drag_image::ReturnedDragPreview::create(image, cursor_position()) {
                Ok(created) => {
                    bridge.log(&format!(
                        "event=source_return_preview_visible id={id} attempt={attempt} release_action=cancel"
                    ));
                    preview = Some(created);
                }
                Err(error) => {
                    // A transient compositor or window-creation failure must not cancel the
                    // authenticated drag offer. Keep the protocol session alive and recreate the
                    // visual preview while the pointer is still held.
                    bridge.log(&format!(
                        "event=source_return_preview_create_failed id={id} attempt={attempt} action=retry error={error}"
                    ));
                    std::thread::sleep(Duration::from_millis(32));
                    continue;
                }
            }
        }

        if let Some(active_preview) = preview.as_ref()
            && let Err(error) = active_preview.follow(cursor_position())
        {
            // The preview is only visual feedback. Recreate it instead of translating a display
            // hiccup into DragMessage::CANCEL, which used to make Explorer lose the drag image.
            bridge.log(&format!(
                "event=source_return_preview_move_failed id={id} attempt={attempt} action=recreate error={error}"
            ));
            preview.take();
            std::thread::sleep(Duration::from_millis(16));
            continue;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    drop(preview);
    bridge.finish_source_return_presentation(id);
}

const fn should_release_retained_source(
    retained_id: u64,
    current_id: u64,
    source_released: bool,
    source_presenting: bool,
) -> bool {
    !source_presenting && (retained_id != current_id || source_released)
}

#[derive(Default)]
struct NativeDragDiagnostics {
    target: Cell<HWND>,
    effect: Cell<Option<u32>>,
    changes: Cell<u32>,
    desktop_redraw_window: Cell<HWND>,
}

impl NativeDragDiagnostics {
    fn record(&self, bridge: &DragBridge, event: &str) {
        let point = cursor_position();
        let hit = unsafe { WindowFromPoint(point) };
        bridge.log(&format!(
            "event={event} id={} cursor={},{} ole_target={:?} ole_class={} hit={:?} hit_class={} effect={:?} async_left_down={} reverse={} returning={}",
            bridge.target_debug_id(), point.x, point.y, self.target.get(),
            window_class(self.target.get()), hit, window_class(hit), self.effect.get(),
            async_left_button_down(),
            bridge.target_reverse.load(Ordering::Acquire), bridge.target_returning.load(Ordering::Acquire),
        ));
    }

    fn changed(&self, bridge: &DragBridge, event: &str) {
        let count = self.changes.get();
        self.changes.set(count.saturating_add(1));
        if count < 32 {
            self.record(bridge, event);
        }
    }

    fn suspend_desktop_redraw(&self, bridge: &DragBridge) {
        if !self.desktop_redraw_window.get().0.is_null() {
            return;
        }
        let point = cursor_position();
        if !resolve_drop_target(point).is_some_and(|target| target.desktop) {
            return;
        }
        let Ok((shell_view, _)) = desktop_shell_view() else {
            bridge.log("event=desktop_redraw_suspend_failed reason=query_view");
            return;
        };
        let Ok(view_window) = (unsafe { shell_view.GetWindow() }) else {
            bridge.log("event=desktop_redraw_suspend_failed reason=query_window");
            return;
        };
        // On the classic Windows desktop IShellView::GetWindow returns SHELLDLL_DefView, while
        // SysListView32 is the child that actually paints and rearranges the icons. Disabling only
        // the parent still leaks Explorer's temporary first-free-slot frame to the screen.
        let window =
            unsafe { FindWindowExW(Some(view_window), None, w!("SysListView32"), PCWSTR::null()) }
                .unwrap_or(view_window);
        // Keep Explorer's asynchronous virtual-file insertion off-screen until the original
        // layout and the final drop position can be applied in one batch.
        unsafe {
            let _ = SendMessageW(window, WM_SETREDRAW, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        self.desktop_redraw_window.set(window);
        bridge.log(&format!(
            "event=desktop_redraw_suspended view_window={view_window:?} paint_window={window:?} paint_class={}",
            window_class(window)
        ));
    }

    fn resume_desktop_redraw(&self) -> Option<HWND> {
        let window = self.desktop_redraw_window.replace(HWND::default());
        if window.0.is_null() {
            return None;
        }
        unsafe {
            let _ = SendMessageW(window, WM_SETREDRAW, Some(WPARAM(1)), Some(LPARAM(0)));
            let _ = RedrawWindow(
                Some(window),
                None,
                None,
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
        }
        Some(window)
    }
}

impl Drop for NativeDragDiagnostics {
    fn drop(&mut self) {
        let _ = self.resume_desktop_redraw();
    }
}

#[implement(IDropSource, IDropSourceNotify)]
struct RemoteDropSource {
    bridge: Arc<DragBridge>,
    diagnostics: Rc<NativeDragDiagnostics>,
}

impl IDropSourceNotify_Impl for RemoteDropSource_Impl {
    fn DragEnterTarget(&self, window: HWND) -> Result<()> {
        catch_com_result(|| {
            self.diagnostics.target.set(window);
            self.diagnostics
                .changed(&self.bridge, "native_target_enter");
            Ok(())
        })
    }
    fn DragLeaveTarget(&self) -> Result<()> {
        catch_com_result(|| {
            self.diagnostics
                .changed(&self.bridge, "native_target_leave");
            self.diagnostics.target.set(HWND::default());
            Ok(())
        })
    }
}
impl IDropSource_Impl for RemoteDropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, _: MODIFIERKEYS_FLAGS) -> HRESULT {
        catch_com_hresult(|| {
            if escape.as_bool() || self.bridge.stop.load(Ordering::Acquire) {
                return DRAGDROP_S_CANCEL;
            }
            match self.bridge.target_state.load(Ordering::Acquire) {
                CANCELLED => DRAGDROP_S_CANCEL,
                RELEASED => {
                    // Release and activation can arrive before OLE has done its first hit-test.
                    // Give it one feedback cycle instead of dropping with no target/effect yet.
                    if self.diagnostics.effect.get().is_none() {
                        return S_OK;
                    }
                    self.diagnostics.suspend_desktop_redraw(&self.bridge);
                    self.bridge.capture_target_release_point();
                    self.diagnostics
                        .record(&self.bridge, "native_target_release");
                    DRAGDROP_S_DROP
                }
                _ => S_OK,
            }
        })
    }
    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        catch_com_hresult(|| {
            if self.diagnostics.effect.replace(Some(effect.0)) != Some(effect.0) {
                self.diagnostics
                    .changed(&self.bridge, "native_target_feedback");
            }
            DRAGDROP_S_USEDEFAULTCURSORS
        })
    }
}

const DRAG_INPUT_ANCHOR_CLASS: PCWSTR = w!("PierdeskRemoteDragInputAnchor");
const DRAG_INPUT_TAG: usize = 0x4346_4444;
static DRAG_INPUT_ANCHOR_DOWNS: AtomicU64 = AtomicU64::new(0);

/// Owns the synthetic system-wide left-button state required by native targets whose drag policy
/// is evaluated outside the OLE source STA (notably Chromium/Electron and QQ).
///
/// `SetKeyboardState` changes only one thread's input table. It can make the `grfKeyState` passed
/// by OLE look correct while `GetAsyncKeyState(VK_LBUTTON)` in another process still reports Up.
/// A real `SendInput` press closes that split. A nearly transparent no-activate anchor covers the
/// virtual desktop while the press is delivered. A small anchor could lose a delayed Down to the
/// underlying Explorer desktop as forwarded pointer motion moved beyond its bounds. `DoDragDrop`
/// must not begin until the anchor actually acknowledges the Down. The matching Up is paired
/// before the OLE modal loop ends.
#[derive(Clone)]
struct NativeDragButton {
    active: Arc<AtomicBool>,
}

impl NativeDragButton {
    fn press_at(point: POINT) -> io::Result<Self> {
        if async_left_button_down() {
            return Err(io::Error::other(
                "destination left button was already down before native drag activation",
            ));
        }
        let (dx, dy) = normalized_virtual_point(point)?;
        let inputs = [
            mouse_input(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                dx,
                dy,
            ),
            mouse_input(MOUSEEVENTF_LEFTDOWN, 0, 0),
        ];
        let mut last_error = None;
        for _ in 0..3 {
            match send_native_mouse_inputs(&inputs) {
                Ok(()) if wait_for_async_left_button(true, Duration::from_millis(100)) => {
                    return Ok(Self {
                        active: Arc::new(AtomicBool::new(true)),
                    });
                }
                Ok(()) => {
                    last_error = Some(io::Error::other(
                        "native drag press did not establish the system left-button state",
                    ));
                }
                Err(error) => last_error = Some(error),
            }
            // SendInput can report a partial insertion, and the asynchronous key state can lag
            // behind a successful call. Pair every unsuccessful attempt before retrying so a
            // late Down can never escape the owned capture window.
            let _ = send_native_mouse_inputs(&[mouse_input(MOUSEEVENTF_LEFTUP, 0, 0)]);
            let _ = wait_for_async_left_button(false, Duration::from_millis(100));
        }
        Err(last_error.unwrap_or_else(|| io::Error::other("native drag press failed")))
    }

    fn release(&self) -> io::Result<bool> {
        if self
            .active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(false);
        }
        let mut last_error = None;
        for _ in 0..3 {
            match send_native_mouse_inputs(&[mouse_input(MOUSEEVENTF_LEFTUP, 0, 0)]) {
                Ok(()) if wait_for_async_left_button(false, Duration::from_millis(100)) => {
                    return Ok(true);
                }
                Ok(()) => {
                    last_error = Some(io::Error::other(
                        "native drag release did not clear the system left-button state",
                    ));
                }
                Err(error) => last_error = Some(error),
            }
        }
        self.active.store(true, Ordering::Release);
        Err(last_error.unwrap_or_else(|| io::Error::other("native drag release failed")))
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

struct NativeDragInputAnchor {
    handle: HWND,
}

impl NativeDragInputAnchor {
    fn create(point: POINT) -> io::Result<Self> {
        let module = unsafe { GetModuleHandleW(None) }.map_err(io::Error::other)?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(drag_input_anchor_procedure),
            hInstance: instance,
            lpszClassName: DRAG_INPUT_ANCHOR_CLASS,
            ..WNDCLASSW::default()
        };
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(1410) {
                return Err(error);
            }
        }
        let (left, top, width, height) = drag_input_anchor_bounds(point)?;
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                DRAG_INPUT_ANCHOR_CLASS,
                w!("Pierdesk drag input anchor"),
                WS_POPUP,
                left,
                top,
                width,
                height,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(io::Error::other)?;
        let anchor = Self { handle };
        unsafe { SetLayeredWindowAttributes(handle, COLORREF(0), 1, LWA_ALPHA) }
            .map_err(io::Error::other)?;
        anchor.show_at(point)?;
        Ok(anchor)
    }

    fn show_at(&self, point: POINT) -> io::Result<()> {
        let (left, top, width, height) = drag_input_anchor_bounds(point)?;
        unsafe {
            SetWindowPos(
                self.handle,
                Some(HWND_TOPMOST),
                left,
                top,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        }
        .map_err(io::Error::other)?;
        let _ = unsafe { ShowWindow(self.handle, SW_SHOWNOACTIVATE) };
        if unsafe { WindowFromPoint(point) } != self.handle {
            return Err(io::Error::other(
                "native drag input anchor did not own the activation point",
            ));
        }
        Ok(())
    }

    fn hide(&self) {
        let _ = unsafe { ShowWindow(self.handle, SW_HIDE) };
    }

    fn capture_mouse(&self) -> io::Result<()> {
        // Capture supplements the full-screen hit-test while other forwarded motion is queued.
        // It is not proof of delivery: a background window cannot capture input outside its
        // visible bounds, so start() also requires an actual WndProc acknowledgement.
        let _ = unsafe { SetCapture(self.handle) };
        if unsafe { GetCapture() } == self.handle {
            Ok(())
        } else {
            Err(io::Error::other(
                "native drag input anchor could not capture the mouse",
            ))
        }
    }

    fn release_mouse_capture(&self) {
        if unsafe { GetCapture() } == self.handle {
            let _ = unsafe { ReleaseCapture() };
        }
    }

    fn owns_point(&self, point: POINT) -> bool {
        unsafe { WindowFromPoint(point) == self.handle && GetCapture() == self.handle }
    }

    fn routing_diagnostic(&self) -> String {
        let point = cursor_position();
        let hit = unsafe { WindowFromPoint(point) };
        let capture = unsafe { GetCapture() };
        format!(
            "cursor={},{} hit={hit:?} hit_class={} hit_process={} anchor_hit={} anchor_capture={} {}",
            point.x,
            point.y,
            window_class(hit),
            window_process_name(hit).unwrap_or_else(|| "unavailable".to_owned()),
            hit == self.handle,
            capture == self.handle,
            foreground_process_diagnostic(),
        )
    }
}

impl Drop for NativeDragInputAnchor {
    fn drop(&mut self) {
        self.release_mouse_capture();
        let _ = unsafe { DestroyWindow(self.handle) };
    }
}

struct NativeDragInput {
    button: NativeDragButton,
    anchor: NativeDragInputAnchor,
    press_acknowledged: bool,
}

impl NativeDragInput {
    fn start(point: POINT) -> io::Result<Self> {
        let anchor = NativeDragInputAnchor::create(point)?;
        anchor.capture_mouse()?;
        let down_count = DRAG_INPUT_ANCHOR_DOWNS.load(Ordering::Acquire);
        let button = NativeDragButton::press_at(point)?;
        let started = Instant::now();
        let acknowledged = || DRAG_INPUT_ANCHOR_DOWNS.load(Ordering::Acquire) != down_count;
        loop {
            // Hardware input is retrieved in queue order. A window-filtered PeekMessage cannot
            // remove an earlier input message addressed to another window on this thread, which
            // would leave the anchor's Down queued behind it, so pump the whole thread queue.
            pump_messages();
            if acknowledged() {
                break;
            }
            let elapsed = started.elapsed();
            if elapsed >= DRAG_INPUT_OWNED_ACK_TIMEOUT
                || (elapsed >= DRAG_INPUT_ACK_TIMEOUT && !anchor.owns_point(cursor_position()))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let press_acknowledged = acknowledged();
        if !press_acknowledged {
            // SendInput reports stream insertion, not which window received the click. Continuing
            // here can create a desktop marquee underneath a valid OLE drag and displace icons.
            let waited_ms = started.elapsed().as_millis();
            let routing = anchor.routing_diagnostic();
            let release = button.release();
            pump_messages();
            let late = acknowledged();
            return Err(io::Error::other(format!(
                "native drag bootstrap press was not acknowledged {} waited_ms={waited_ms} late_ack={late} {routing}",
                if release.is_ok() {
                    "by its anchor"
                } else {
                    "and release failed"
                }
            )));
        }
        anchor.release_mouse_capture();
        anchor.hide();
        Ok(Self {
            button,
            anchor,
            press_acknowledged,
        })
    }

    fn controller(&self) -> NativeDragButton {
        self.button.clone()
    }

    const fn press_acknowledged(&self) -> bool {
        self.press_acknowledged
    }

    fn release_after_native_end(&self) -> io::Result<bool> {
        if !self.button.is_active() {
            return Ok(false);
        }
        let point = cursor_position();
        let show_result = self.anchor.show_at(point);
        let capture_result = show_result.and_then(|()| self.anchor.capture_mouse());
        let release_result = self.button.release();
        pump_messages();
        self.anchor.release_mouse_capture();
        self.anchor.hide();
        release_result.and(capture_result).map(|()| true)
    }
}

fn drag_input_anchor_bounds(point: POINT) -> io::Result<(i32, i32, i32, i32)> {
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).map_err(io::Error::other)?,
        ..MONITORINFO::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        return Err(io::Error::other(
            "native drag anchor monitor is unavailable",
        ));
    }
    anchor_rect_within_monitor(info.rcMonitor, point)
        .ok_or_else(|| io::Error::other("invalid native drag anchor geometry"))
}

/// A non-foreground topmost window covering a whole monitor is treated by Explorer as a
/// full-screen ("rude") window, and the taskbar raises itself back above it. Entry from a lower
/// neighbour lands on the taskbar strip, so the bootstrap Down then reached the taskbar instead
/// of the anchor. Leave out the one monitor row farthest from the activation point.
fn anchor_rect_within_monitor(monitor: RECT, point: POINT) -> Option<(i32, i32, i32, i32)> {
    let width = monitor.right.saturating_sub(monitor.left);
    let height = monitor.bottom.saturating_sub(monitor.top);
    if width <= 0 || height <= 1 {
        return None;
    }
    let middle = monitor.top.saturating_add(height / 2);
    let top = if point.y >= middle {
        monitor.top.saturating_add(1)
    } else {
        monitor.top
    };
    Some((monitor.left, top, width, height - 1))
}

impl Drop for NativeDragInput {
    fn drop(&mut self) {
        let _ = self.release_after_native_end();
    }
}

fn async_left_button_down() -> bool {
    (unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) }) < 0
}

fn wait_for_async_left_button(expected_down: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if async_left_button_down() == expected_down {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn normalized_virtual_point(point: POINT) -> io::Result<(i32, i32)> {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 1 || height <= 1 {
        return Err(io::Error::other("invalid virtual-screen geometry"));
    }
    let normalize = |coordinate: i32, origin: i32, extent: i32| {
        crate::cross_screen::normalized_absolute_coordinate(
            coordinate.saturating_sub(origin),
            extent,
        )
    };
    Ok((
        normalize(point.x, left, width),
        normalize(point.y, top, height),
    ))
}

fn mouse_input(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    dx: i32,
    dy: i32,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: DRAG_INPUT_TAG,
            },
        },
    }
}

fn send_native_mouse_inputs(inputs: &[INPUT]) -> io::Result<()> {
    let size = i32::try_from(size_of::<INPUT>()).expect("INPUT size fits i32");
    let sent = unsafe { SendInput(inputs, size) };
    if usize::try_from(sent).unwrap_or_default() == inputs.len() {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

unsafe extern "system" fn drag_input_anchor_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => LRESULT(isize::try_from(MA_NOACTIVATE).unwrap_or(3)),
        WM_LBUTTONDOWN => {
            DRAG_INPUT_ANCHOR_DOWNS.fetch_add(1, Ordering::Release);
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_MOUSEMOVE => LRESULT(0),
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

struct LocalDropMaterializer {
    cache_root: PathBuf,
    directory_name: String,
    entries: Arc<[VirtualFileEntry]>,
    materialized: Mutex<Option<Arc<[PathBuf]>>>,
    served: AtomicBool,
    bridge: Arc<DragBridge>,
}

impl LocalDropMaterializer {
    fn new(
        store: &DeviceStore,
        manifest: &super::secure_transfer::OfferManifest,
        entries: Vec<VirtualFileEntry>,
        bridge: &Arc<DragBridge>,
    ) -> Self {
        Self {
            cache_root: store.root().join("drag-cache"),
            directory_name: format!("drop-{}-p{}", manifest.offer_id, std::process::id()),
            entries: Arc::from(entries),
            materialized: Mutex::new(None),
            served: AtomicBool::new(false),
            bridge: Arc::clone(bridge),
        }
    }

    // `CF_HDROP` for a target. Before the drop it never downloads: hovering over an application
    // that requests local paths during DragEnter (Chromium/WebView2, including Pierdesk's own
    // windows) used to download the whole offer inside the OLE modal loop, and a background
    // prefetch still surfaced as a transfer before the user released. Only a released drop
    // fetches the files.
    fn hdrop_paths(&self, dropping: bool) -> io::Result<Arc<[PathBuf]>> {
        let paths = if dropping {
            self.paths()?
        } else {
            let cached = match self.materialized.try_lock() {
                Ok(cached) => cached.as_ref().map(Arc::clone),
                Err(std::sync::TryLockError::WouldBlock) => None,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(io::Error::other("local drop materializer was poisoned"));
                }
            };
            let Some(paths) = cached else {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "local drop files are fetched only after the drop",
                ));
            };
            paths
        };
        self.served.store(true, Ordering::Release);
        Ok(paths)
    }

    fn paths(&self) -> io::Result<Arc<[PathBuf]>> {
        let mut cached = self
            .materialized
            .lock()
            .map_err(|_| io::Error::other("local drop materializer was poisoned"))?;
        if let Some(paths) = cached.as_ref() {
            return Ok(Arc::clone(paths));
        }

        fs::create_dir_all(&self.cache_root)?;
        cleanup_local_drop_cache(&self.cache_root);
        let root = self.allocate_root()?;
        let started = Instant::now();
        let total_bytes = self
            .entries
            .iter()
            .filter(|entry| !entry.descriptor.is_directory())
            .map(|entry| entry.descriptor.size)
            .sum::<u64>();
        self.bridge.log(&format!(
            "event=local_drop_materialize_start files={} bytes={} root={}",
            self.entries
                .iter()
                .filter(|entry| !entry.descriptor.is_directory())
                .count(),
            total_bytes,
            root.display()
        ));

        let result = self.materialize_into(&root);
        let paths = match result {
            Ok(paths) => paths,
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                self.bridge.log(&format!(
                    "event=local_drop_materialize_failed elapsed_ms={} error={error}",
                    started.elapsed().as_millis()
                ));
                return Err(error);
            }
        };
        self.bridge.log(&format!(
            "event=local_drop_materialize_complete files={} bytes={} elapsed_ms={} paths={}",
            self.entries
                .iter()
                .filter(|entry| !entry.descriptor.is_directory())
                .count(),
            total_bytes,
            started.elapsed().as_millis(),
            paths.len()
        ));
        let paths: Arc<[PathBuf]> = Arc::from(paths);
        *cached = Some(Arc::clone(&paths));
        Ok(paths)
    }

    /// True only when a target actually received local paths, not merely when a background
    /// prefetch completed; an Explorer virtual-file drop must keep its STA alive regardless.
    fn served(&self) -> bool {
        self.served.load(Ordering::Acquire)
    }

    fn allocate_root(&self) -> io::Result<PathBuf> {
        for suffix in 0..100_u32 {
            let name = if suffix == 0 {
                self.directory_name.clone()
            } else {
                format!("{}-{suffix}", self.directory_name)
            };
            let path = self.cache_root.join(name);
            match fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate an isolated local drop cache directory",
        ))
    }

    fn materialize_into(&self, root: &Path) -> io::Result<Vec<PathBuf>> {
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.descriptor.is_directory())
        {
            fs::create_dir_all(root.join(entry.descriptor.file_name.as_ref()))?;
        }
        for entry in self
            .entries
            .iter()
            .filter(|entry| !entry.descriptor.is_directory())
        {
            let path = root.join(entry.descriptor.file_name.as_ref());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            let source_factory = entry.source_factory.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file entry has no content source",
                )
            })?;
            let source = source_factory();
            if source.len() != entry.descriptor.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local drop source size does not match its authenticated manifest",
                ));
            }
            let mut offset = 0_u64;
            let mut buffer = vec![0_u8; LOCAL_DROP_READ_BUFFER];
            while offset < entry.descriptor.size {
                let remaining = entry.descriptor.size - offset;
                let requested = usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(buffer.len());
                let read = source
                    .read_at(offset, &mut buffer[..requested])
                    .map_err(|error| io::Error::other(error.to_string()))?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "remote file ended before its authenticated size",
                    ));
                }
                file.write_all(&buffer[..read])?;
                offset = offset
                    .checked_add(u64::try_from(read).map_err(io::Error::other)?)
                    .ok_or_else(|| io::Error::other("local drop file offset overflow"))?;
            }
            file.flush()?;
            let mut eof_probe = [0_u8; 1];
            let eof = source
                .read_at(entry.descriptor.size, &mut eof_probe)
                .map_err(|error| io::Error::other(error.to_string()))?;
            if eof != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote file exceeded its authenticated size",
                ));
            }
        }

        let mut seen = HashSet::new();
        let mut paths = Vec::new();
        for entry in self.entries.iter() {
            let top = entry
                .descriptor
                .file_name
                .split('\\')
                .next()
                .ok_or_else(|| io::Error::other("empty local drop path"))?;
            let key = top.to_lowercase();
            if seen.insert(key) {
                paths.push(root.join(top));
            }
        }
        if paths.is_empty() || paths.iter().any(|path| !path.exists()) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "local drop paths did not materialize completely",
            ));
        }
        Ok(paths)
    }
}

fn cleanup_local_drop_cache(cache_root: &Path) {
    let cutoff = SystemTime::now()
        .checked_sub(LOCAL_DROP_CACHE_RETENTION)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(entries) = fs::read_dir(cache_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let managed = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("drop-"));
        let expired = entry
            .metadata()
            .ok()
            .filter(std::fs::Metadata::is_dir)
            .and_then(|metadata| metadata.modified().ok())
            .is_some_and(|modified| modified < cutoff);
        if managed && expired {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn target_drag(
    bridge: &Arc<DragBridge>,
    store: &DeviceStore,
    peer: CertificateFingerprint,
    address: SocketAddr,
    id: u64,
) -> io::Result<bool> {
    let clipboard = unsafe { GetClipboardSequenceNumber() };
    let identity = store.load_identity()?;
    let trusted = store.load_peer(peer)?;
    let tls = PinnedTlsClient::new(
        &identity,
        trusted.certificate_der().to_vec(),
        peer,
        Duration::from_secs(3),
    )?;
    let client = SecureOfferClient::new(address, tls);
    let manifest = client.fetch_manifest()?;
    let preexisting_desktop_names = desktop_preexisting_names(&manifest);
    // Explorer initially inserts an asynchronously materialised virtual file at slot zero before
    // it learns the final drop position. Moving only the new item afterwards leaves a hole because
    // every old desktop icon has already shifted. Preserve the complete pre-drop layout so those
    // existing icons can be put back exactly where they were.
    let desktop_layout = match capture_desktop_layout() {
        Ok(layout) => Some(layout),
        Err(error) => {
            bridge.log(&format!(
                "event=desktop_layout_capture_failed id={id} error={error}"
            ));
            None
        }
    };
    let image = client.fetch_drag_image(manifest.offer_id)?;
    let registry = Arc::new(RemoteTransferRegistry::default());
    registry.bind_task(client.clone(), manifest.offer_id);
    let entries: Vec<VirtualFileEntry> = manifest
        .entries
        .iter()
        .map(|entry| {
            let descriptor = entry.descriptor.clone();
            if descriptor.is_directory() {
                return VirtualFileEntry::directory(descriptor);
            }
            let client = client.clone();
            let manifest = manifest.clone();
            let entry = entry.clone();
            let registry = Arc::clone(&registry);
            let factory: SourceFactory = Arc::new(move || {
                let source = registry.create_source_for_entry(
                    client.clone(),
                    manifest.clone(),
                    entry.clone(),
                );
                let source: Arc<dyn ReadAtSource> = source;
                source
            });
            VirtualFileEntry::file(descriptor, factory)
        })
        .collect();
    let probe = Arc::new(ProbeState::quiet());
    let materializer = Arc::new(LocalDropMaterializer::new(
        store,
        &manifest,
        entries.clone(),
        bridge,
    ));
    let provider_materializer = Arc::clone(&materializer);
    let provider_bridge = Arc::clone(bridge);
    let hdrop_provider: HdropProvider = Arc::new(move || {
        let state = provider_bridge.target_state.load(Ordering::Acquire);
        provider_materializer.hdrop_paths(state == RELEASED || state == TRANSFERRING)
    });
    let gate_bridge = Arc::clone(bridge);
    let last_policy = Arc::new(AtomicU8::new(0));
    let hdrop_gate: HdropGate = Arc::new(move || {
        // Explorer (desktop, folder windows, taskbar) consumes FileGroupDescriptorW/FileContents
        // natively: it streams after the drop with its own progress UI and reliably calls
        // EndOperation. Given CF_HDROP it instead requests the paths during DragEnter, which
        // downloads the whole offer inside the OLE modal loop, and then never ends the async
        // operation. Keep the local-path form for applications that need it.
        let process = window_process_name(unsafe { WindowFromPoint(cursor_position()) });
        let offered = !process
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"));
        let policy = if offered { 1 } else { 2 };
        if last_policy.swap(policy, Ordering::AcqRel) != policy {
            gate_bridge.log(&format!(
                "event=hdrop_policy id={id} offered={offered} target_process={}",
                process.as_deref().unwrap_or("unavailable")
            ));
        }
        offered
    });
    let object = VirtualFileDataObject::create_with_entries_and_hdrop(
        entries,
        Arc::clone(&probe),
        manifest.origin_payload(),
        hdrop_provider,
        hdrop_gate,
    )
    .map_err(io::Error::other)?;
    let capability: IDataObjectAsyncCapability = object.cast().map_err(io::Error::other)?;
    unsafe { capability.SetAsyncMode(true) }.map_err(io::Error::other)?;
    let ui_task = register_ui_task(&manifest, &registry);
    let mut presentation = 0_u32;
    let (result, effect, drop_point, diagnostics) = loop {
        presentation = presentation.saturating_add(1);
        if !bridge.begin_target_presentation() {
            ui_task.cancelled.store(true, Ordering::Release);
            return Ok(false);
        }
        let _drag_image = super::drag_image::initialize(&object, &image)?;
        bridge.log(&format!(
            "event=target_ready id={id} image_bytes={} presentation={presentation}",
            image.len()
        ));
        bridge.send(id, DragMessage::READY, 0);
        bridge.target_prepared.store(true, Ordering::Release);
        let activation_started = Instant::now();
        while bridge.target_waiting_for_activation() {
            if bridge.stop.load(Ordering::Acquire)
                || activation_started.elapsed() > Duration::from_mins(2)
            {
                ui_task.cancelled.store(true, Ordering::Release);
                return Err(io::Error::other("drag activation timed out"));
            }
            pump_messages();
            std::thread::sleep(Duration::from_millis(4));
        }
        if bridge.target_state.load(Ordering::Acquire) == CANCELLED {
            if bridge.target_returning.load(Ordering::Acquire) {
                bridge.target_state.store(PREPARING, Ordering::Release);
                bridge.log(&format!(
                    "event=target_resume_wait id={id} presentation={presentation} phase=before_native_drag"
                ));
                continue;
            }
            ui_task.cancelled.store(true, Ordering::Release);
            return Ok(false);
        }
        let point = cursor_position();
        bridge.log(&format!(
            "event=target_activate id={id} cursor={},{} reverse={} tunnel={} target_state={} presentation={presentation}",
            point.x,
            point.y,
            bridge.target_reverse.load(Ordering::Acquire),
            bridge.target_tunnel.load(Ordering::Acquire),
            bridge.target_state.load(Ordering::Acquire),
        ));
        let diagnostics = Rc::new(NativeDragDiagnostics::default());
        let native_input = match NativeDragInput::start(point) {
            Ok(input) => input,
            Err(error) if bridge.target_returning.load(Ordering::Acquire) => {
                // Control can cross back while the system-wide bootstrap press is still being
                // acknowledged. This is a suspended presentation, not a failed file offer. Keep
                // the authenticated data object alive and let the next loop iteration rearm it.
                bridge.log(&format!(
                    "event=target_resume_wait id={id} presentation={presentation} phase=native_input_interrupted error={error}"
                ));
                continue;
            }
            Err(error) => return Err(error),
        };
        bridge.log(&format!(
            "event=ole_native_drag_button id={id} state=down scope=system anchor=owned anchor_acknowledged={}",
            native_input.press_acknowledged(),
        ));
        let source: IDropSource = RemoteDropSource {
            bridge: Arc::clone(bridge),
            diagnostics: Rc::clone(&diagnostics),
        }
        .into();
        let done = Arc::new(AtomicBool::new(false));
        let done_worker = Arc::clone(&done);
        let wake_bridge = Arc::clone(bridge);
        let wake_button = native_input.controller();
        let thread_id = unsafe { GetCurrentThreadId() };
        let native_started = Instant::now();
        let wake = std::thread::spawn(move || {
            let mut previous_state = wake_bridge.target_state.load(Ordering::Acquire);
            let mut initial_wake = true;
            while !done_worker.load(Ordering::Acquire) {
                if native_started.elapsed() > Duration::from_mins(2) {
                    wake_bridge.target_state.store(CANCELLED, Ordering::Release);
                }
                let state = wake_bridge.target_state.load(Ordering::Acquire);
                if initial_wake || state != previous_state {
                    if matches!(state, RELEASED | CANCELLED) {
                        match wake_button.release() {
                            Ok(true) => wake_bridge.log(&format!(
                                "event=ole_native_drag_button id={id} state=up reason={} scope=system",
                                if state == RELEASED {
                                    "remote_release"
                                } else {
                                    "remote_cancel"
                                }
                            )),
                            Ok(false) => {}
                            Err(error) => wake_bridge.log(&format!(
                                "event=ole_native_drag_button_release_failed id={id} reason=remote_terminal error={error}"
                            )),
                        }
                    }
                    // This is a thread message (hWnd == NULL), so it has no client coordinate
                    // space. Supplying physical screen coordinates in lParam made OLE targets
                    // interpret a second, offset pointer and highlight UI away from the visible
                    // drag image. The message merely wakes DoDragDrop so QueryContinueDrag can
                    // observe the bridge state; OLE reads the real cursor position separately.
                    unsafe {
                        let _ = PostThreadMessageW(thread_id, WM_MOUSEMOVE, WPARAM(0), LPARAM(0));
                    }
                    initial_wake = false;
                    previous_state = state;
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        });
        let mut effect = DROPEFFECT_NONE;
        let ole_loop = crate::cross_screen::OleDragLoopGuard::enter();
        let result = unsafe { DoDragDrop(&object, &source, DROPEFFECT_COPY, &raw mut effect) };
        drop(ole_loop);
        match native_input.release_after_native_end() {
            Ok(true) => bridge.log(&format!(
                "event=ole_native_drag_button id={id} state=up reason=native_end_guard scope=system"
            )),
            Ok(false) => {}
            Err(error) => bridge.log(&format!(
                "event=ole_native_drag_button_release_failed id={id} reason=native_end_guard error={error}"
            )),
        }
        let native_end_point = cursor_position();
        done.store(true, Ordering::Release);
        let _ = wake.join();
        if bridge.target_returning.load(Ordering::Acquire) {
            let _ = diagnostics.resume_desktop_redraw();
            bridge.target_state.store(PREPARING, Ordering::Release);
            bridge.log(&format!(
                "event=target_resume_wait id={id} presentation={presentation} phase=after_native_drag hr={:#x}",
                result.0,
            ));
            continue;
        }
        break (result, effect, native_end_point, diagnostics);
    };
    let release_point = bridge
        .target_release_point
        .lock()
        .expect("target release point lock")
        .take();
    let captured_at_release = release_point.is_some();
    let drop_point = release_point.unwrap_or(drop_point);
    bridge.log(&format!(
        "event=drop_result id={id} hr={:#x} effect={} clipboard_unchanged={} cursor={},{} cursor_source={}",
        result.0,
        effect.0,
        clipboard == unsafe { GetClipboardSequenceNumber() },
        drop_point.x,
        drop_point.y,
        if captured_at_release { "ole_release" } else { "native_end" },
    ));
    if result != DRAGDROP_S_DROP || effect != DROPEFFECT_COPY {
        let events = probe.events();
        bridge.log(&format!(
            "event=ole_probe_summary id={id} retained={} total={} dropped={}",
            events.len(),
            probe.event_count(),
            probe.dropped_events(),
        ));
        for (index, event) in events.iter().enumerate() {
            bridge.log(&format!(
                "event=ole_probe id={id} index={index} detail={event}"
            ));
        }
    }
    let mut desktop_drop = false;
    if result == DRAGDROP_S_DROP && effect == DROPEFFECT_COPY {
        ui_task.accepted.store(true, Ordering::Release);
        if let Some(location) = resolve_drop_target(drop_point) {
            desktop_drop = location.desktop;
            location.path.clone_into(
                &mut ui_task
                    .target_location
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            bridge.log(&format!(
                "event=target_location id={id} path={} desktop={}",
                location.path, location.desktop
            ));
            if let Err(error) = client.report_target_location(manifest.offer_id, &location.path) {
                bridge.log(&format!(
                    "event=target_location_report_failed id={id} error={error}"
                ));
            }
        } else {
            bridge.log(&format!(
                "event=target_location_unresolved id={id} cursor={},{}",
                drop_point.x, drop_point.y
            ));
        }
    } else {
        ui_task.cancelled.store(true, Ordering::Release);
        ui_task
            .shell_outcome
            .store(SHELL_CANCELLED, Ordering::Release);
    }
    // Keep the STA and data object alive for Explorer's asynchronous reads after DoDragDrop.
    let transfer_started = Instant::now();
    let mut watchdog = TransferWatchdog::new(transfer_started);
    bridge.target_state.store(TRANSFERRING, Ordering::Release);
    if desktop_drop {
        match restore_and_position_received_desktop_items(
            &manifest,
            drop_point,
            &preexisting_desktop_names,
            desktop_layout.as_ref(),
        ) {
            Ok((restored, positioned)) => bridge.log(&format!(
                "event=desktop_layout_stabilized id={id} restored={restored} positioned={positioned} cursor={},{}",
                drop_point.x, drop_point.y
            )),
            Err(error) => bridge.log(&format!(
                "event=desktop_layout_stabilize_deferred id={id} cursor={},{} error={error}",
                drop_point.x, drop_point.y
            )),
        }
    }
    // The namespace item normally appears while Explorer begins its asynchronous copy. Repaint as
    // soon as that first atomic layout pass completes instead of freezing the desktop for the
    // duration of a large transfer. The final pass below handles late namespace convergence.
    if let Some(window) = diagnostics.resume_desktop_redraw() {
        bridge.log(&format!(
            "event=desktop_redraw_resumed window={window:?} phase=initial"
        ));
    }
    if result == DRAGDROP_S_DROP && effect == DROPEFFECT_COPY {
        let mut timed_out = false;
        let mut desktop_position_checks = 0_u8;
        loop {
            pump_messages();
            if desktop_drop
                && desktop_position_checks < 3
                && transfer_started.elapsed()
                    >= match desktop_position_checks {
                        0 => Duration::from_millis(500),
                        1 => Duration::from_millis(1500),
                        _ => Duration::from_secs(3),
                    }
            {
                desktop_position_checks += 1;
                match check_received_desktop_position(
                    &manifest,
                    drop_point,
                    &preexisting_desktop_names,
                    desktop_layout.as_ref(),
                ) {
                    Ok(DesktopPositionCheck::Displaced { actual, expected }) => {
                        bridge.log(&format!(
                            "event=desktop_layout_displaced id={id} check={desktop_position_checks} actual={},{} expected={},{}",
                            actual.x, actual.y, expected.x, expected.y,
                        ));
                        if let Err(error) = restore_and_position_received_desktop_items(
                            &manifest,
                            drop_point,
                            &preexisting_desktop_names,
                            desktop_layout.as_ref(),
                        ) {
                            bridge.log(&format!(
                                "event=desktop_layout_retry_failed id={id} check={desktop_position_checks} error={error}"
                            ));
                        }
                    }
                    Ok(DesktopPositionCheck::Settled) => {}
                    Ok(DesktopPositionCheck::AutoArrange) => {
                        bridge.log(&format!(
                            "event=desktop_layout_auto_arrange id={id} action=respect_user_view"
                        ));
                        desktop_position_checks = 3;
                    }
                    Err(error) => bridge.log(&format!(
                        "event=desktop_layout_check_failed id={id} check={desktop_position_checks} error={error}"
                    )),
                }
            }
            let in_operation = unsafe { capability.InOperation() }
                .map_err(io::Error::other)?
                .as_bool();
            let shell = probe.shell_transfer();
            // After DoDragDrop returns, the target can perform additional reads only if it called
            // StartOperation. InOperation=false is therefore the Shell's terminal signal; waiting
            // for every virtual stream to be opened deadlocks skipped/conflicting files.
            if !in_operation {
                break;
            }
            if bridge.stop.load(Ordering::Acquire)
                || watchdog.stalled(registry.local_progress(), Instant::now())
            {
                bridge.log(&format!(
                    "event=transfer_stalled id={id} bytes={} idle_ms={} stopped={}",
                    registry.local_progress().bytes_read,
                    watchdog.last_progress.elapsed().as_millis(),
                    bridge.stop.load(Ordering::Acquire)
                ));
                registry.set_local_control(super::secure_transfer::RemoteTransferState::Cancelled);
                ui_task.cancelled.store(true, Ordering::Release);
                ui_task.shell_outcome.store(SHELL_FAILED, Ordering::Release);
                timed_out = true;
                break;
            }
            if shell.async_ended {
                // Be defensive against a target that reports EndOperation before its observable
                // InOperation flag converges. The callback itself is terminal by contract.
                break;
            }
            if materializer.served() && transfer_started.elapsed() >= LOCAL_DROP_SHELL_SETTLE {
                bridge.log(&format!(
                    "event=local_drop_shell_settled id={id} reason=no_end_operation elapsed_ms={}",
                    transfer_started.elapsed().as_millis()
                ));
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        if !timed_out {
            let mut shell = probe.shell_transfer();
            if materializer.served() && !shell.async_ended {
                // The target copied from complete local files; judge it like a synchronous
                // target instead of waiting for an EndOperation that will not arrive.
                shell.async_started = false;
            }
            let progress = registry.local_progress();
            let ordinary_files = manifest
                .entries
                .iter()
                .filter(|entry| !entry.descriptor.is_directory())
                .count();
            let total_size = manifest
                .entries
                .iter()
                .filter(|entry| !entry.descriptor.is_directory())
                .map(|entry| entry.descriptor.size)
                .sum::<u64>();
            let network_complete = ordinary_files == 0
                || total_size == 0
                || progress.completed_transfers >= ordinary_files;
            ui_task.shell_outcome.store(
                classify_shell_outcome(shell, network_complete),
                Ordering::Release,
            );
        }
    }
    if desktop_drop && ui_task.shell_outcome.load(Ordering::Acquire) == SHELL_SUCCEEDED {
        match restore_and_position_received_desktop_items(
            &manifest,
            drop_point,
            &preexisting_desktop_names,
            desktop_layout.as_ref(),
        ) {
            Ok((restored, positioned)) => bridge.log(&format!(
                "event=desktop_layout_repaired id={id} restored={restored} positioned={positioned} cursor={},{}",
                drop_point.x, drop_point.y
            )),
            Err(error) => bridge.log(&format!(
                "event=desktop_layout_repair_failed id={id} cursor={},{} error={error}",
                drop_point.x, drop_point.y
            )),
        }
    }
    if let Some(window) = diagnostics.resume_desktop_redraw() {
        bridge.log(&format!("event=desktop_redraw_resumed window={window:?}"));
    }
    let shell = probe.shell_transfer();
    bridge.log(&format!(
        "event=transfer_end id={id} read_calls={} bytes={} completed_transfers={} async_started={} async_ended={} async_result={:#x} async_effects={} performed_effect={:?} logical_effect={:?} paste_succeeded={:?} shell_outcome={}",
        registry.local_progress().read_calls,
        registry.local_progress().bytes_read,
        registry.local_progress().completed_transfers,
        shell.async_started,
        shell.async_ended,
        shell.async_result,
        shell.async_effects,
        shell.performed_effect,
        shell.logical_performed_effect,
        shell.paste_succeeded,
        ui_task.shell_outcome.load(Ordering::Acquire),
    ));
    Ok(ui_task.shell_outcome.load(Ordering::Acquire) == SHELL_SUCCEEDED)
}

struct DropTargetLocation {
    path: String,
    desktop: bool,
}

fn resolve_drop_target(point: POINT) -> Option<DropTargetLocation> {
    let hit = unsafe { WindowFromPoint(point) };
    let mut current = hit;
    let mut desktop = false;
    while !current.0.is_null() {
        let class = window_class(current);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "SHELLDLL_DefView") {
            desktop = true;
            break;
        }
        current = unsafe { GetParent(current) }.unwrap_or_default();
    }
    if desktop {
        return known_desktop_path().map(|path| DropTargetLocation {
            path,
            desktop: true,
        });
    }
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    if window_class(root) != "CabinetWClass" {
        return None;
    }
    shell_window_path(root).map(|path| DropTargetLocation {
        path,
        desktop: false,
    })
}

struct OwnedItemIdList(*mut ITEMIDLIST);

impl Drop for OwnedItemIdList {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

struct DesktopItemPosition {
    item: OwnedItemIdList,
    position: POINT,
}

struct DesktopLayoutSnapshot {
    shell_view: windows::Win32::UI::Shell::IShellView,
    folder_view: IFolderView,
    items: Vec<DesktopItemPosition>,
}

fn desktop_top_level_names(manifest: &super::secure_transfer::OfferManifest) -> Vec<&str> {
    let mut names = Vec::new();
    for entry in manifest.entries.iter() {
        let Some(name) = entry.descriptor.file_name.split('\\').next() else {
            continue;
        };
        if !name.is_empty()
            && !names
                .iter()
                .any(|existing: &&str| existing.eq_ignore_ascii_case(name))
        {
            names.push(name);
        }
    }
    names
}

fn desktop_preexisting_names(manifest: &super::secure_transfer::OfferManifest) -> Vec<String> {
    let Some(path) = known_desktop_path().map(PathBuf::from) else {
        return Vec::new();
    };
    desktop_top_level_names(manifest)
        .into_iter()
        .filter(|name| path.join(name).exists())
        .map(str::to_lowercase)
        .collect()
}

fn desktop_shell_view() -> io::Result<(windows::Win32::UI::Shell::IShellView, IFolderView)> {
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
    let shell_view = unsafe { browser.QueryActiveShellView() }
        .map_err(|error| io::Error::other(format!("query active desktop view: {error}")))?;
    let folder_view = shell_view
        .cast::<IFolderView>()
        .map_err(|error| io::Error::other(format!("query desktop folder view: {error}")))?;
    Ok((shell_view, folder_view))
}

fn capture_desktop_layout() -> io::Result<DesktopLayoutSnapshot> {
    let (shell_view, folder_view) = desktop_shell_view()?;
    let count = unsafe { folder_view.ItemCount(SVGIO_ALLVIEW) }
        .map_err(|error| io::Error::other(format!("count desktop items: {error}")))?;
    let mut items = Vec::with_capacity(usize::try_from(count.max(0)).unwrap_or_default());
    for index in 0..count {
        let item = unsafe { folder_view.Item(index) }.map_err(|error| {
            io::Error::other(format!("enumerate desktop item {index}: {error}"))
        })?;
        if item.is_null() {
            continue;
        }
        let item = OwnedItemIdList(item);
        let position =
            unsafe { folder_view.GetItemPosition(item.0.cast_const()) }.map_err(|error| {
                io::Error::other(format!("read desktop item {index} position: {error}"))
            })?;
        items.push(DesktopItemPosition { item, position });
    }
    Ok(DesktopLayoutSnapshot {
        shell_view,
        folder_view,
        items,
    })
}

fn parse_desktop_item(folder: &IShellFolder, name: &str) -> io::Result<OwnedItemIdList> {
    let wide = name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut item = std::ptr::null_mut();
    let mut attributes = 0_u32;
    unsafe {
        folder.ParseDisplayName(
            HWND::default(),
            None::<&windows::Win32::System::Com::IBindCtx>,
            PCWSTR(wide.as_ptr()),
            None,
            &raw mut item,
            &raw mut attributes,
        )
    }
    .map_err(|error| io::Error::other(format!("resolve desktop item {name:?}: {error}")))?;
    if item.is_null() {
        return Err(io::Error::other(format!(
            "desktop item {name:?} resolved to a null identifier"
        )));
    }
    Ok(OwnedItemIdList(item))
}

struct ReceivedDesktopPositions {
    items: Vec<OwnedItemIdList>,
    positions: Vec<POINT>,
}

enum DesktopPositionCheck {
    Settled,
    Displaced { actual: POINT, expected: POINT },
    AutoArrange,
}

fn desktop_position_displaced(actual: POINT, expected: POINT, spacing: POINT) -> bool {
    let x_delta = (i64::from(actual.x) - i64::from(expected.x)).abs();
    let y_delta = (i64::from(actual.y) - i64::from(expected.y)).abs();
    x_delta > i64::from(spacing.x.max(64)) || y_delta > i64::from(spacing.y.max(64))
}

fn check_received_desktop_position(
    manifest: &super::secure_transfer::OfferManifest,
    screen_point: POINT,
    preexisting_names: &[String],
    layout: Option<&DesktopLayoutSnapshot>,
) -> io::Result<DesktopPositionCheck> {
    let detached;
    let (shell_view, folder_view) = if let Some(layout) = layout {
        (&layout.shell_view, &layout.folder_view)
    } else {
        detached = desktop_shell_view()?;
        (&detached.0, &detached.1)
    };
    let settings = unsafe { shell_view.GetCurrentInfo() }
        .map_err(|error| io::Error::other(format!("read desktop view settings: {error}")))?;
    if settings.fFlags & FWF_AUTOARRANGE.0.cast_unsigned() != 0 {
        return Ok(DesktopPositionCheck::AutoArrange);
    }
    let received = received_desktop_positions(
        manifest,
        screen_point,
        preexisting_names,
        shell_view,
        folder_view,
    )?;
    let spacing = unsafe { folder_view.GetDefaultSpacing() }.unwrap_or(POINT { x: 96, y: 96 });
    for (item, expected) in received.items.iter().zip(&received.positions) {
        let actual =
            unsafe { folder_view.GetItemPosition(item.0.cast_const()) }.map_err(|error| {
                io::Error::other(format!("read received desktop position: {error}"))
            })?;
        if desktop_position_displaced(actual, *expected, spacing) {
            return Ok(DesktopPositionCheck::Displaced {
                actual,
                expected: *expected,
            });
        }
    }
    Ok(DesktopPositionCheck::Settled)
}

fn received_desktop_positions(
    manifest: &super::secure_transfer::OfferManifest,
    screen_point: POINT,
    preexisting_names: &[String],
    shell_view: &windows::Win32::UI::Shell::IShellView,
    folder_view: &IFolderView,
) -> io::Result<ReceivedDesktopPositions> {
    let names = desktop_top_level_names(manifest)
        .into_iter()
        .filter(|name| {
            !preexisting_names
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
        })
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Ok(ReceivedDesktopPositions {
            items: Vec::new(),
            positions: Vec::new(),
        });
    }
    let folder: IShellFolder = unsafe { folder_view.GetFolder() }
        .map_err(|error| io::Error::other(format!("query desktop folder: {error}")))?;

    // Explorer can report EndOperation just before its namespace view observes the new item.
    // Retry the lookup briefly while pumping this OLE STA instead of falling back to slot zero.
    let deadline = Instant::now() + Duration::from_millis(750);
    let items = loop {
        let parsed = names
            .iter()
            .map(|name| parse_desktop_item(&folder, name))
            .collect::<io::Result<Vec<_>>>();
        match parsed {
            Ok(items) => break items,
            Err(_error) if Instant::now() < deadline => {
                pump_messages();
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    };

    let view_window = unsafe { shell_view.GetWindow() }
        .map_err(|error| io::Error::other(format!("query desktop view window: {error}")))?;
    let mut client_point = screen_point;
    if !unsafe { ScreenToClient(view_window, &raw mut client_point) }.as_bool() {
        return Err(io::Error::last_os_error());
    }
    let spacing = unsafe { folder_view.GetDefaultSpacing() }.unwrap_or(POINT { x: 96, y: 96 });
    let positions = (0..items.len())
        .map(|index| POINT {
            x: client_point.x.saturating_add(
                spacing
                    .x
                    .max(1)
                    .saturating_mul(i32::try_from(index).unwrap_or(i32::MAX)),
            ),
            y: client_point.y,
        })
        .collect::<Vec<_>>();
    Ok(ReceivedDesktopPositions { items, positions })
}

fn apply_desktop_positions(
    folder_view: &IFolderView,
    identifiers: &[*const ITEMIDLIST],
    positions: &[POINT],
) -> io::Result<()> {
    debug_assert_eq!(identifiers.len(), positions.len());
    if identifiers.is_empty() {
        return Ok(());
    }
    unsafe {
        folder_view.SelectAndPositionItems(
            u32::try_from(identifiers.len())
                .map_err(|_| io::Error::other("too many desktop items to position"))?,
            identifiers.as_ptr(),
            Some(positions.as_ptr()),
            SVSI_POSITIONITEM.0.cast_unsigned(),
        )
    }
    .map_err(|error| io::Error::other(format!("position desktop items: {error}")))
}

fn position_received_desktop_items(
    manifest: &super::secure_transfer::OfferManifest,
    screen_point: POINT,
    preexisting_names: &[String],
    shell_view: &windows::Win32::UI::Shell::IShellView,
    folder_view: &IFolderView,
) -> io::Result<usize> {
    let received = received_desktop_positions(
        manifest,
        screen_point,
        preexisting_names,
        shell_view,
        folder_view,
    )?;
    let identifiers = received
        .items
        .iter()
        .map(|item| item.0.cast_const())
        .collect::<Vec<_>>();
    apply_desktop_positions(folder_view, &identifiers, &received.positions)?;
    Ok(received.items.len())
}

fn restore_and_position_received_desktop_items(
    manifest: &super::secure_transfer::OfferManifest,
    screen_point: POINT,
    preexisting_names: &[String],
    layout: Option<&DesktopLayoutSnapshot>,
) -> io::Result<(usize, usize)> {
    let Some(layout) = layout else {
        let (shell_view, folder_view) = desktop_shell_view()?;
        let positioned = position_received_desktop_items(
            manifest,
            screen_point,
            preexisting_names,
            &shell_view,
            &folder_view,
        )?;
        return Ok((0, positioned));
    };

    let received = received_desktop_positions(
        manifest,
        screen_point,
        preexisting_names,
        &layout.shell_view,
        &layout.folder_view,
    )?;
    // IFolderView applies positioning asynchronously. Two consecutive calls (old layout first,
    // new item second) allow the second request to supersede the first on Explorer's view thread.
    // Submit one atomic batch containing both sets so the old layout and final drop location are
    // committed together.
    let identifiers = layout
        .items
        .iter()
        .map(|item| item.item.0.cast_const())
        .chain(received.items.iter().map(|item| item.0.cast_const()))
        .collect::<Vec<_>>();
    let positions = layout
        .items
        .iter()
        .map(|item| item.position)
        .chain(received.positions.iter().copied())
        .collect::<Vec<_>>();
    apply_desktop_positions(&layout.folder_view, &identifiers, &positions)?;
    Ok((layout.items.len(), received.items.len()))
}

fn window_process_name(window: HWND) -> Option<String> {
    if window.0.is_null() {
        return None;
    }
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut process_id)) };
    if process_id == 0 {
        return None;
    }
    let image = process_image(process_id).ok()?;
    image
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn window_class(window: windows::Win32::Foundation::HWND) -> String {
    let mut buffer = [0_u16; 128];
    let length = unsafe { GetClassNameW(window, &mut buffer) };
    String::from_utf16_lossy(&buffer[..usize::try_from(length).unwrap_or(0)])
}

fn known_desktop_path() -> Option<String> {
    let desktop = FOLDERID_Desktop;
    let path = unsafe { SHGetKnownFolderPath(&raw const desktop, KF_FLAG_DEFAULT, None) }.ok()?;
    let result = unsafe { path.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(path.as_ptr().cast())) };
    result
}

fn shell_window_path(root: windows::Win32::Foundation::HWND) -> Option<String> {
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }.ok()?;
    let count = unsafe { windows.Count() }.ok()?;
    for index in 0..count {
        let variant = variant_i32(index);
        let Ok(dispatch) = (unsafe { windows.Item(&variant) }) else {
            continue;
        };
        let Ok(browser) = dispatch.cast::<IWebBrowser2>() else {
            continue;
        };
        let Ok(browser_window) = (unsafe { browser.HWND() }) else {
            continue;
        };
        if browser_window.0 != root.0 as isize {
            continue;
        }
        let Ok(document) = (unsafe { browser.Document() }) else {
            continue;
        };
        let Ok(view) = document.cast::<IShellFolderViewDual2>() else {
            continue;
        };
        let Ok(folder) = (unsafe { view.Folder() }) else {
            continue;
        };
        let Ok(folder) = folder.cast::<Folder2>() else {
            continue;
        };
        let Ok(folder_item) = (unsafe { folder.Self_() }) else {
            continue;
        };
        let Ok(path) = (unsafe { folder_item.Path() }) else {
            continue;
        };
        let path = path.to_string();
        if PathBuf::from(&path).is_absolute() {
            return Some(path);
        }
    }
    None
}

fn variant_i32(value: i32) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_I4;
        (*variant.Anonymous.Anonymous).Anonymous.lVal = value;
    }
    variant
}

#[cfg(test)]
mod tests {
    use super::super::product::{ProductTransferDirection, ProductTransferState};
    use super::*;
    use crate::clipboard::data_object::VirtualFileDescriptor;
    use crate::clipboard::source::MemorySource;

    fn bridge() -> DragBridge {
        DragBridge::new(PathBuf::new())
    }
    fn message(id: u64, kind: u8) -> DragMessage {
        DragMessage {
            id,
            kind,
            port: if matches!(kind, DragMessage::PREPARE | DragMessage::PREPARE_TUNNEL) {
                45239
            } else {
                0
            },
        }
    }

    #[test]
    fn inactivity_watchdog_allows_long_transfers_and_pauses() {
        let start = Instant::now();
        let mut watchdog = super::TransferWatchdog::new(start);
        let mut p = super::super::secure_transfer::TransferLocalProgress {
            started_transfers: 1,
            completed_transfers: 0,
            read_calls: 0,
            bytes_read: 0,
            paused: false,
            cancelled: false,
        };
        for minute in 1..90 {
            p.bytes_read += 1024;
            assert!(!watchdog.stalled(p, start + Duration::from_secs(minute * 60)));
        }
        p.paused = true;
        assert!(!watchdog.stalled(p, start + Duration::from_hours(3)));
        p.paused = false;
        assert!(!watchdog.stalled(p, start + Duration::from_hours(3) + Duration::from_secs(1)));
        assert!(watchdog.stalled(p, start + Duration::from_hours(3) + Duration::from_mins(11)));
    }

    #[test]
    fn boundary_arm_ignores_stationary_press() {
        let mut arm = BoundaryMotionArm::default();
        for _ in 0..20 {
            assert!(!arm.update(true, true, (1000, 1080)));
        }
    }

    #[test]
    fn boundary_arm_ignores_jitter_below_threshold() {
        let mut arm = BoundaryMotionArm::default();
        assert!(!arm.update(true, true, (1000, 1080)));
        assert!(!arm.update(true, true, (1003, 1077)));
        assert!(!arm.update(true, true, (997, 1084)));
    }

    #[test]
    fn boundary_arm_latches_after_real_movement_until_release() {
        let mut arm = BoundaryMotionArm::default();
        assert!(!arm.update(true, true, (1000, 1080)));
        assert!(arm.update(true, true, (1020, 1080)));
        assert!(arm.update(true, true, (1000, 1080)));
        assert!(!arm.update(false, true, (1000, 1080)));
        assert!(!arm.update(true, true, (1000, 1080)));
    }

    #[test]
    fn boundary_arm_is_immediate_without_motion_requirement() {
        let mut arm = BoundaryMotionArm::default();
        assert!(arm.update(true, false, (1000, 1080)));
    }

    #[test]
    fn absolute_input_coordinates_map_back_to_the_same_pixel() {
        for extent in [768, 1080, 1400, 1440, 2160, 3840, 5120] {
            for pixel in 0..extent {
                let value = i64::from(crate::cross_screen::normalized_absolute_coordinate(
                    pixel, extent,
                ));
                assert!(value <= 65_535);
                // Both conversions Windows has been observed to use.
                assert_eq!(value * i64::from(extent) / 65_536, i64::from(pixel));
                assert_eq!(value * i64::from(extent) / 65_535, i64::from(pixel));
            }
        }
    }

    #[test]
    fn drag_anchor_never_covers_a_whole_monitor_but_covers_the_point() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 2176,
            bottom: 1400,
        };
        for y in [0, 1, 699, 700, 1395, 1399] {
            let point = POINT { x: 1177, y };
            let (left, top, width, height) =
                super::anchor_rect_within_monitor(monitor, point).unwrap();
            assert_eq!(height, 1399);
            assert!(width == 2176 && left == 0);
            assert!((top..top + height).contains(&y), "y={y} top={top}");
        }
    }

    #[test]
    fn virtual_only_drag_source_reproduces_the_historical_format_rejection() {
        let _apartment = Apartment::new().unwrap();
        let probe = Arc::new(ProbeState::quiet());
        let source = Arc::new(MemorySource::new(&b"virtual content"[..]));
        let object =
            VirtualFileDataObject::create(Arc::from("virtual.txt"), source, Arc::clone(&probe))
                .unwrap();

        let error = real_paths(&object).unwrap_err().to_string();
        assert!(error.contains("0x80040064"), "unexpected error: {error}");
        assert!(error.contains("stage=get_cf_hdrop"));
        let diagnostic = drag_source_diagnostic(&object);
        assert!(diagnostic.contains("hdrop_query=0x80040064"));
        assert!(diagnostic.contains("file_descriptor_w_query=0x00000000"));
        assert!(diagnostic.contains("file_contents_0_query=0x00000000"));
        assert!(diagnostic.contains("FileGroupDescriptorW"));
        assert!(diagnostic.contains("FileContents"));
        assert!(diagnostic.len() <= MAX_DRAG_DIAGNOSTIC_BYTES);
        assert_eq!(
            probe.read_calls(),
            0,
            "diagnostics must not read file content"
        );
        assert!(probe.events().iter().any(|event| {
            event.contains("IDataObject::GetData") && event.contains("format=15")
        }));
    }

    #[test]
    fn local_drop_materializer_writes_a_complete_tree_once_and_returns_top_level_paths() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let store_root = std::env::temp_dir().join(format!(
            "pierdesk-local-drop-test-{}-{nonce}",
            std::process::id()
        ));
        let store = DeviceStore::new(&store_root);
        let alpha: Arc<dyn ReadAtSource> = Arc::new(MemorySource::new(&b"alpha"[..]));
        let beta: Arc<dyn ReadAtSource> = Arc::new(MemorySource::new(&b"beta"[..]));
        let offered = SecureOfferedFile::new_tree(
            vec![
                (
                    VirtualFileDescriptor {
                        file_name: Arc::from("资料"),
                        size: 0,
                        attributes: windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0,
                        creation_time: None,
                        last_access_time: None,
                        last_write_time: None,
                    },
                    None,
                ),
                (
                    VirtualFileDescriptor::basic(Arc::from("资料\\甲.txt"), 5),
                    Some(Arc::clone(&alpha)),
                ),
                (
                    VirtualFileDescriptor::basic(Arc::from("乙.txt"), 4),
                    Some(Arc::clone(&beta)),
                ),
            ],
            Duration::from_secs(30),
        )
        .unwrap();
        let entries = vec![
            VirtualFileEntry::directory(offered.manifest().entries[0].descriptor.clone()),
            VirtualFileEntry::file(
                offered.manifest().entries[1].descriptor.clone(),
                Arc::new(move || Arc::clone(&alpha)),
            ),
            VirtualFileEntry::file(
                offered.manifest().entries[2].descriptor.clone(),
                Arc::new(move || Arc::clone(&beta)),
            ),
        ];
        let materializer =
            LocalDropMaterializer::new(&store, &offered.manifest(), entries, &Arc::new(bridge()));

        let first = materializer.paths().unwrap();
        let second = materializer.paths().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.len(), 2);
        let root = first[0].parent().unwrap();
        assert_eq!(fs::read(root.join("资料\\甲.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(root.join("乙.txt")).unwrap(), b"beta");

        fs::remove_dir_all(store_root).unwrap();
    }

    #[test]
    fn hovering_hdrop_request_never_downloads_and_drop_fetches() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let store_root = std::env::temp_dir().join(format!(
            "pierdesk-local-drop-prefetch-{}-{nonce}",
            std::process::id()
        ));
        let store = DeviceStore::new(&store_root);
        let source: Arc<dyn ReadAtSource> = Arc::new(MemorySource::new(&b"payload"[..]));
        let offered = SecureOfferedFile::new_tree(
            vec![(
                VirtualFileDescriptor::basic(Arc::from("视频.mp4"), 7),
                Some(Arc::clone(&source)),
            )],
            Duration::from_secs(30),
        )
        .unwrap();
        let entries = vec![VirtualFileEntry::file(
            offered.manifest().entries[0].descriptor.clone(),
            Arc::new(move || Arc::clone(&source)),
        )];
        let materializer = Arc::new(LocalDropMaterializer::new(
            &store,
            &offered.manifest(),
            entries,
            &Arc::new(bridge()),
        ));

        // While the button is held a request neither waits nor starts a download.
        let error = materializer.hdrop_paths(false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(!materializer.served());
        std::thread::sleep(Duration::from_millis(50));
        assert!(materializer.materialized.lock().unwrap().is_none());
        // Only the drop fetches the files; later requests reuse them.
        let dropped = materializer.hdrop_paths(true).unwrap();
        assert!(materializer.served());
        assert_eq!(fs::read(&dropped[0]).unwrap(), b"payload");
        let hovered = materializer.hdrop_paths(false).unwrap();
        assert!(Arc::ptr_eq(&dropped, &hovered));

        fs::remove_dir_all(store_root).unwrap();
    }

    #[test]
    fn transient_drag_leave_keeps_candidate_until_the_held_drag_ends() {
        assert!(!should_cancel_after_drag_leave(false, true));
        assert!(!should_cancel_after_drag_leave(true, false));
        assert!(should_cancel_after_drag_leave(false, false));
    }

    #[test]
    fn retained_explorer_object_is_released_as_soon_as_return_is_impossible() {
        assert!(!should_release_retained_source(7, 7, false, false));
        assert!(should_release_retained_source(7, 7, true, false));
        assert!(should_release_retained_source(7, 0, false, false));
        assert!(!should_release_retained_source(7, 0, true, true));
    }

    #[test]
    fn drag_ui_task_uses_the_existing_receiver_window_model() {
        let offered = SecureOfferedFile::generated(
            Arc::<str>::from("drag-window.bin"),
            1234,
            Duration::from_secs(30),
        )
        .unwrap();
        let task = DragUiTask {
            manifest: offered.manifest(),
            registry: Arc::new(RemoteTransferRegistry::default()),
            generation: 77,
            created: Instant::now(),
            accepted: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            shell_outcome: AtomicU8::new(SHELL_PENDING),
            target_location: Mutex::new(String::new()),
        };
        let snapshot = ui_task_snapshot(&task);
        assert_eq!(snapshot.manifest.primary_name, "drag-window.bin");
        assert_eq!(snapshot.manifest.files, 1);
        assert_eq!(snapshot.transfer.total_size, 1234);
        assert_eq!(
            snapshot.transfer.state,
            super::super::product::ProductTransferState::AwaitingPaste
        );
        task.cancelled.store(true, Ordering::Release);
        assert_eq!(
            ui_task_snapshot(&task).transfer.state,
            super::super::product::ProductTransferState::Cancelled
        );
    }

    #[test]
    fn desktop_positioning_uses_each_top_level_manifest_item_once() {
        let directory = |name: &'static str| {
            let mut descriptor =
                super::super::data_object::VirtualFileDescriptor::basic(Arc::<str>::from(name), 0);
            descriptor.attributes = windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0;
            (descriptor, None)
        };
        let offered = SecureOfferedFile::new_tree(
            vec![
                directory("资料"),
                directory("资料\\子目录"),
                directory("另一项"),
            ],
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(
            desktop_top_level_names(&offered.manifest()),
            vec!["资料", "另一项"]
        );
    }

    #[test]
    fn desktop_position_check_distinguishes_grid_snap_from_top_left_displacement() {
        let expected = POINT { x: 1604, y: 1056 };
        let spacing = POINT { x: 112, y: 112 };
        assert!(!desktop_position_displaced(expected, expected, spacing));
        assert!(!desktop_position_displaced(
            POINT { x: 1552, y: 1012 },
            expected,
            spacing,
        ));
        assert!(desktop_position_displaced(
            POINT { x: 0, y: 0 },
            expected,
            spacing,
        ));
    }

    #[test]
    fn source_drag_task_is_exposed_as_sending_history_with_server_progress() {
        let offered = SecureOfferedFile::generated(
            Arc::<str>::from("sent-by-drag.bin"),
            4096,
            Duration::from_secs(30),
        )
        .unwrap()
        .with_source_location(r"C:\Users\Local\Desktop".to_owned())
        .unwrap();
        let task = DragSourceUiTask {
            manifest: offered.manifest(),
            gesture_id: 91,
            generation: 79,
            created: Instant::now(),
            file_progress: Mutex::new(Vec::new()),
            metrics: Mutex::new(SecureMetricsSnapshot {
                begun_transfers: 1,
                unique_bytes: 2048,
                ..SecureMetricsSnapshot::default()
            }),
            target_location: Mutex::new(r"C:\Users\Peer\Desktop".to_owned()),
            terminal: AtomicU8::new(SHELL_PENDING),
            control: super::super::secure_transfer::SourceTaskControl::default(),
        };
        let snapshot = source_ui_task_snapshot(&task);
        assert_eq!(snapshot.id.direction(), ProductTransferDirection::Sending);
        assert_eq!(snapshot.transfer.state, ProductTransferState::Running);
        assert_eq!(snapshot.transfer.transferred, 2048);
        assert_eq!(snapshot.source_location, r"C:\Users\Local\Desktop");
        assert_eq!(snapshot.target_location, r"C:\Users\Peer\Desktop");

        let mut measured = snapshot.files[0].clone();
        measured.transferred = 1024;
        *task.file_progress.lock().unwrap() = vec![measured];
        assert_eq!(source_ui_task_snapshot(&task).files[0].transferred, 1024);
        task.terminal.store(SHELL_SUCCEEDED, Ordering::Release);
        let completed = source_ui_task_snapshot(&task);
        assert_eq!(completed.transfer.state, ProductTransferState::Completed);
        assert_eq!(completed.files[0].transferred, 4096);
    }

    #[test]
    fn drag_ui_completion_waits_for_shell_success_not_only_stream_reads() {
        let mut descriptor = super::super::data_object::VirtualFileDescriptor::basic(
            Arc::<str>::from("empty-folder"),
            0,
        );
        descriptor.attributes = windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0;
        let offered =
            SecureOfferedFile::new_tree(vec![(descriptor, None)], Duration::from_secs(30)).unwrap();
        let task = DragUiTask {
            manifest: offered.manifest(),
            registry: Arc::new(RemoteTransferRegistry::default()),
            generation: 78,
            created: Instant::now(),
            accepted: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            shell_outcome: AtomicU8::new(SHELL_PENDING),
            target_location: Mutex::new(String::new()),
        };

        assert_eq!(
            ui_task_snapshot(&task).transfer.state,
            super::super::product::ProductTransferState::Running
        );
        task.shell_outcome.store(SHELL_CANCELLED, Ordering::Release);
        assert_eq!(
            ui_task_snapshot(&task).transfer.state,
            super::super::product::ProductTransferState::Cancelled
        );
        task.shell_outcome.store(SHELL_SUCCEEDED, Ordering::Release);
        assert_eq!(
            ui_task_snapshot(&task).transfer.state,
            super::super::product::ProductTransferState::Completed
        );
    }

    #[test]
    fn shell_outcome_requires_a_real_successful_copy() {
        use super::super::probe::ShellTransferSnapshot;

        let copied = ShellTransferSnapshot {
            async_started: true,
            async_ended: true,
            async_result: S_OK.0.cast_unsigned(),
            async_effects: DROPEFFECT_COPY.0,
            paste_succeeded: Some(DROPEFFECT_COPY.0),
            ..ShellTransferSnapshot::default()
        };
        assert_eq!(super::classify_shell_outcome(copied, true), SHELL_SUCCEEDED);
        assert_eq!(
            super::classify_shell_outcome(copied, false),
            SHELL_CANCELLED
        );

        let skipped_conflict = ShellTransferSnapshot {
            paste_succeeded: Some(DROPEFFECT_NONE.0),
            ..copied
        };
        assert_eq!(
            super::classify_shell_outcome(skipped_conflict, true),
            SHELL_CANCELLED
        );

        let failed = ShellTransferSnapshot {
            async_result: windows::Win32::Foundation::E_FAIL.0.cast_unsigned(),
            ..copied
        };
        assert_eq!(super::classify_shell_outcome(failed, true), SHELL_FAILED);

        let synchronous_without_feedback = ShellTransferSnapshot::default();
        assert_eq!(
            super::classify_shell_outcome(synchronous_without_feedback, true),
            SHELL_SUCCEEDED
        );
    }

    #[test]
    fn target_cancel_is_terminal_and_stale_release_cannot_revive_it() {
        let bridge = bridge();
        bridge.receive(message(1, DragMessage::PREPARE), true);
        bridge.receive(message(1, DragMessage::ACTIVATE), false);
        assert_eq!(bridge.target_state.load(Ordering::Acquire), ACTIVE);
        bridge.receive(message(2, DragMessage::RELEASE), false);
        assert_eq!(bridge.target_state.load(Ordering::Acquire), ACTIVE);
        bridge.receive(message(1, DragMessage::CANCEL), false);
        bridge.receive(message(1, DragMessage::RELEASE), false);
        bridge.receive(message(1, DragMessage::ACTIVATE), false);
        assert_eq!(bridge.target_state.load(Ordering::Acquire), CANCELLED);
    }

    #[test]
    fn release_without_activation_cannot_drop_and_native_source_honors_terminal_state() {
        let bridge = Arc::new(bridge());
        bridge.receive(message(1, DragMessage::PREPARE), true);
        bridge.receive(message(1, DragMessage::RELEASE), false);
        assert_eq!(bridge.target_state.load(Ordering::Acquire), CANCELLED);
        let source: IDropSource = RemoteDropSource {
            bridge: Arc::clone(&bridge),
            diagnostics: Rc::default(),
        }
        .into();
        assert_eq!(
            unsafe { source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)) },
            DRAGDROP_S_CANCEL
        );
        bridge.target_state.store(ACTIVE, Ordering::Release);
        assert_eq!(
            unsafe { source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)) },
            S_OK
        );
        bridge.target_state.store(RELEASED, Ordering::Release);
        assert_eq!(
            unsafe { source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)) },
            S_OK
        );
        assert_eq!(
            unsafe { source.GiveFeedback(DROPEFFECT_COPY) },
            DRAGDROP_S_USEDEFAULTCURSORS
        );
        assert_eq!(
            unsafe { source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)) },
            DRAGDROP_S_DROP
        );
        assert!(bridge.target_release_point.lock().unwrap().is_some());
        assert_eq!(
            unsafe { source.QueryContinueDrag(true, MODIFIERKEYS_FLAGS(0)) },
            DRAGDROP_S_CANCEL
        );
    }

    #[test]
    #[ignore = "injects a paired mouse press into an owned window to validate native drag input"]
    fn native_drag_input_is_anchored_and_restores_the_global_button() {
        assert!(!async_left_button_down());
        let point = cursor_position();
        let input = NativeDragInput::start(point).unwrap();
        assert!(async_left_button_down());
        assert!(input.press_acknowledged());
        assert_ne!(unsafe { GetCapture() }, input.anchor.handle);
        assert_ne!(unsafe { WindowFromPoint(point) }, input.anchor.handle);
        assert!(input.release_after_native_end().unwrap());
        assert!(!async_left_button_down());
        assert!(!input.release_after_native_end().unwrap());
    }

    #[test]
    #[ignore = "temporarily moves the pointer to the top boundary to validate edge activation"]
    fn native_drag_input_capture_consumes_the_press_at_the_top_boundary() {
        struct CursorPositionRestore(POINT);

        impl Drop for CursorPositionRestore {
            fn drop(&mut self) {
                if let Ok((dx, dy)) = normalized_virtual_point(self.0) {
                    let _ = send_native_mouse_inputs(&[mouse_input(
                        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        dx,
                        dy,
                    )]);
                }
            }
        }

        assert!(!async_left_button_down());
        let original = cursor_position();
        let _restore = CursorPositionRestore(original);
        let point = POINT {
            x: original.x,
            y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        };
        for _ in 0..8 {
            let input = NativeDragInput::start(point).unwrap();
            assert!(async_left_button_down());
            assert!(input.press_acknowledged());
            assert_ne!(unsafe { GetCapture() }, input.anchor.handle);
            assert_ne!(unsafe { WindowFromPoint(point) }, input.anchor.handle);
            assert!(input.release_after_native_end().unwrap());
            assert!(!async_left_button_down());
        }
    }

    #[test]
    fn normal_target_rearm_waits_for_cursor_arrival_on_every_crossing() {
        let bridge = bridge();
        bridge.receive(message(91, DragMessage::PREPARE), true);
        bridge.receive(message(91, DragMessage::ACTIVATE), false);
        assert!(bridge.target_waiting_for_activation());
        for _ in 0..8 {
            bridge.target_control_arrived();
            assert!(!bridge.target_waiting_for_activation());
            assert!(bridge.suspend_target_for_source_return());
            bridge.receive(message(91, DragMessage::ACTIVATE), false);
            assert!(bridge.begin_target_presentation());
            assert!(bridge.target_waiting_for_activation());
            // The trailing ReleaseAll from the last control return must preserve this offer
            // even if ACTIVATE has already rearmed it before that input message arrives.
            assert!(bridge.returned_target_pending());
        }
        bridge.receive(message(91, DragMessage::RELEASE), false);
        assert!(bridge.target_waiting_for_activation());
        bridge.target_control_arrived();
        assert!(!bridge.target_waiting_for_activation());
        assert_eq!(bridge.target_state.load(Ordering::Acquire), RELEASED);
    }

    #[test]
    fn prepare_rejects_busy_replayed_and_stopped_sessions() {
        let bridge = bridge();
        bridge.local_candidate.store(true, Ordering::Release);
        bridge.receive(message(1, DragMessage::PREPARE), true);
        assert_eq!(bridge.poll(), Some(message(1, DragMessage::CANCEL)));
        bridge.local_candidate.store(false, Ordering::Release);
        bridge.receive(message(1, DragMessage::PREPARE), false);
        assert_eq!(bridge.poll(), Some(message(1, DragMessage::CANCEL)));
        bridge.receive(message(1, DragMessage::PREPARE), true);
        bridge.target_id.store(0, Ordering::Release);
        bridge.receive(message(1, DragMessage::PREPARE), true);
        assert_eq!(bridge.poll(), Some(message(1, DragMessage::CANCEL)));
        bridge.stop();
        bridge.receive(message(2, DragMessage::PREPARE), true);
        assert_eq!(bridge.poll(), Some(message(2, DragMessage::CANCEL)));
    }

    #[test]
    fn source_release_is_once_and_return_does_not_revoke_dropped_offer() {
        let bridge = bridge();
        bridge.source_id.store(7, Ordering::Release);
        bridge.local_candidate.store(true, Ordering::Release);
        bridge.receive(message(6, DragMessage::READY), true);
        assert!(!bridge.source_ready.load(Ordering::Acquire));
        bridge.receive(message(7, DragMessage::READY), true);
        bridge.activate_source();
        assert_eq!(bridge.poll(), Some(message(7, DragMessage::ACTIVATE)));
        assert!(bridge.source_dragging());
        bridge.release_source();
        bridge.release_source();
        assert_eq!(bridge.poll(), Some(message(7, DragMessage::RELEASE)));
        assert!(!bridge.source_dragging());
        bridge.activate_source();
        assert_eq!(bridge.poll(), None);
        bridge.cancel();
        assert_eq!(bridge.poll(), None);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 7);
        bridge.receive(message(6, DragMessage::FINISHED), false);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 7);
        bridge.receive(message(7, DragMessage::FINISHED), false);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn only_an_active_unreleased_source_keeps_the_remote_origin_receiver_visible() {
        let bridge = bridge();
        assert!(!bridge.retains_active_source());
        bridge.begin_source_capture(73);
        assert!(!bridge.retains_active_source());
        bridge.source_ready.store(true, Ordering::Release);
        assert!(!bridge.retains_active_source());
        bridge.source_active.store(true, Ordering::Release);
        assert!(bridge.retains_active_source());
        bridge.accept_remote_source_release();
        assert!(!bridge.retains_active_source());
        bridge.clear_source();
        assert!(!bridge.retains_active_source());
        // An incomplete/stale flag without an offer is not a retained drag.
        bridge.source_active.store(true, Ordering::Release);
        assert!(!bridge.retains_active_source());
    }

    #[test]
    fn returned_source_can_be_presented_and_suspended_repeatedly_without_losing_offer() {
        let bridge = Arc::new(bridge());
        bridge.source_id.store(17, Ordering::Release);
        bridge.local_candidate.store(true, Ordering::Release);
        bridge.source_ready.store(true, Ordering::Release);
        bridge.source_active.store(true, Ordering::Release);

        for _ in 0..4 {
            assert!(bridge.present_source_after_return());
            assert_eq!(bridge.poll(), Some(message(17, DragMessage::ACTIVATE)));
            assert_eq!(bridge.take_source_return_presentation(), Some(17));
            assert!(source_return_preview_active(&bridge, 17));

            assert!(bridge.suspend_source_return_presentation());
            assert!(!source_return_preview_active(&bridge, 17));
            bridge.finish_source_return_presentation(17);
            assert_eq!(bridge.source_id.load(Ordering::Acquire), 17);
            assert!(bridge.source_dragging());
        }

        assert!(bridge.present_source_after_return());
        assert_eq!(bridge.poll(), Some(message(17, DragMessage::ACTIVATE)));
        assert_eq!(bridge.take_source_return_presentation(), Some(17));
        assert!(source_return_preview_active(&bridge, 17));
        bridge.release_returned_source();
        assert!(!source_return_preview_active(&bridge, 17));
        bridge.finish_source_return_presentation(17);
        assert_eq!(bridge.poll(), Some(message(17, DragMessage::CANCEL)));
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn remote_controlled_source_is_only_represented_after_peer_reentry() {
        let bridge = bridge();
        bridge.classify_idle_source(true);
        bridge.begin_source_capture(18);
        assert!(bridge.source_remote_controlled.load(Ordering::Acquire));
        bridge.classify_idle_source(false);
        assert!(bridge.source_remote_controlled.load(Ordering::Acquire));
        bridge.source_ready.store(true, Ordering::Release);
        bridge.source_active.store(true, Ordering::Release);

        assert!(!bridge.present_source_after_return());
        assert!(bridge.present_remote_source_after_peer_reentry());
        assert_eq!(bridge.poll(), Some(message(18, DragMessage::ACTIVATE)));
        assert_eq!(bridge.take_source_return_presentation(), Some(18));
        bridge.receive(message(18, DragMessage::READY), false);
        assert_eq!(bridge.poll(), Some(message(18, DragMessage::ACTIVATE)));
    }

    #[test]
    fn captured_source_ownership_is_frozen_until_the_protocol_terminal_state() {
        let bridge = bridge();
        bridge.classify_idle_source(true);
        bridge.begin_source_capture(71);
        bridge.classify_idle_source(false);
        assert!(bridge.source_remote_controlled.load(Ordering::Acquire));

        bridge.clear_source();
        bridge.classify_idle_source(false);
        bridge.begin_source_capture(72);
        bridge.classify_idle_source(true);
        assert!(!bridge.source_remote_controlled.load(Ordering::Acquire));
    }

    #[test]
    fn cancelled_source_never_activates_on_late_ready() {
        let bridge = bridge();
        bridge.source_id.store(1, Ordering::Release);
        bridge.cancel();
        assert_eq!(bridge.poll(), Some(message(1, DragMessage::CANCEL)));
        bridge.receive(message(1, DragMessage::READY), true);
        bridge.activate_source();
        assert!(!bridge.source_dragging());
        assert_eq!(bridge.poll(), None);
    }

    #[test]
    fn remote_left_up_cannot_race_the_reverse_target_terminal_messages() {
        let bridge = bridge();
        bridge.source_id.store(9, Ordering::Release);
        bridge.local_candidate.store(true, Ordering::Release);
        bridge.source_ready.store(true, Ordering::Release);
        bridge.source_active.store(true, Ordering::Release);
        bridge
            .source_remote_controlled
            .store(true, Ordering::Release);

        bridge.observe_remote_source_left_event(true);

        assert_eq!(bridge.poll(), None);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 9);

        bridge.observe_remote_source_left_event(false);

        assert_eq!(bridge.poll(), None);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 9);
        bridge.receive(message(9, DragMessage::RELEASE), false);
        assert!(bridge.source_released.load(Ordering::Acquire));
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 9);
        bridge.receive(message(9, DragMessage::FINISHED), false);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 0);
        assert!(!bridge.local_candidate.load(Ordering::Acquire));
    }

    #[test]
    fn remote_left_up_without_a_drag_terminal_event_expires_safely() {
        let bridge = bridge();
        bridge.source_id.store(10, Ordering::Release);
        bridge.local_candidate.store(true, Ordering::Release);
        bridge.source_ready.store(true, Ordering::Release);
        bridge.source_active.store(true, Ordering::Release);
        bridge
            .source_remote_controlled
            .store(true, Ordering::Release);
        bridge.observe_remote_source_left_event(false);
        *bridge.remote_source_left_up.lock().unwrap() = Instant::now()
            .checked_sub(REMOTE_SOURCE_RELEASE_GRACE + Duration::from_millis(1))
            .map(|started| (10, started));

        bridge.expire_remote_source_left_up();

        assert_eq!(bridge.poll(), Some(message(10, DragMessage::CANCEL)));
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 0);
        assert!(!bridge.local_candidate.load(Ordering::Acquire));
    }

    #[test]
    fn crossing_requires_ready_and_readiness_timeout_cancels() {
        let bridge = bridge();
        assert!(!bridge.may_cross(true));
        *bridge.edge_wait.lock().unwrap() = Instant::now().checked_sub(Duration::from_millis(500));
        assert!(
            !bridge.may_cross(true),
            "a held pointer without a recognized file offer must never cross"
        );
        bridge.local_candidate.store(true, Ordering::Release);
        bridge.source_id.store(1, Ordering::Release);
        assert!(!bridge.may_cross(true));
        bridge.receive(message(1, DragMessage::READY), true);
        assert!(bridge.may_cross(true));
        *bridge.edge_wait.lock().unwrap() =
            Instant::now().checked_sub(SOURCE_PREPARE_TIMEOUT + Duration::from_secs(1));
        assert!(!bridge.may_cross(true));
        assert_eq!(bridge.poll(), Some(message(1, DragMessage::CANCEL)));
    }

    #[test]
    fn only_the_taskbar_edge_uses_a_wide_drag_capture_window() {
        assert_eq!(capture_thickness(DragProbeEdge::Top), 8);
        assert_eq!(capture_thickness(DragProbeEdge::Left), 8);
        assert_eq!(capture_thickness(DragProbeEdge::Right), 8);
        assert_eq!(capture_thickness(DragProbeEdge::Bottom), 96);
    }

    #[test]
    fn transferring_target_no_longer_intercepts_mouse_or_accepts_late_cancel() {
        let bridge = bridge();
        bridge.target_id.store(1, Ordering::Release);
        bridge.target_state.store(TRANSFERRING, Ordering::Release);
        assert!(!bridge.target_pending());
        bridge.cancel();
        bridge.receive(message(1, DragMessage::CANCEL), false);
        assert_eq!(bridge.target_state.load(Ordering::Acquire), TRANSFERRING);
    }

    #[test]
    fn normal_target_can_suspend_and_reactivate_without_cancelling_the_source_offer() {
        let bridge = bridge();
        bridge.receive(message(51, DragMessage::PREPARE), true);
        bridge.receive(message(51, DragMessage::ACTIVATE), false);

        assert!(bridge.suspend_target_for_source_return());
        assert_eq!(bridge.target_state.load(Ordering::Acquire), CANCELLED);
        assert!(bridge.target_returning.load(Ordering::Acquire));
        // ReleaseAll can race ahead of the STA loop that converts CANCELLED back to PREPARING.
        // The independent drag offer must already be considered preservable in that window.
        assert!(bridge.returned_target_pending());
        assert_eq!(bridge.poll(), None);

        for _ in 0..4 {
            bridge.target_state.store(PREPARING, Ordering::Release);
            assert!(bridge.begin_target_presentation());
            assert!(bridge.returned_target_pending());
            bridge.receive(message(51, DragMessage::ACTIVATE), false);
            assert_eq!(bridge.target_state.load(Ordering::Acquire), ACTIVE);
            assert!(bridge.suspend_target_for_source_return());
            assert_eq!(bridge.poll(), None);
        }
    }

    #[test]
    fn active_normal_target_allows_a_held_pointer_to_return_to_its_source() {
        let bridge = bridge();
        bridge.receive(message(52, DragMessage::PREPARE), true);
        bridge.receive(message(52, DragMessage::ACTIVATE), false);

        assert!(bridge.may_return(true));
        assert!(bridge.suspend_target_for_source_return());
        assert!(bridge.returned_target_pending());
        assert_eq!(bridge.poll(), None);
    }

    #[test]
    fn rapid_reentry_messages_survive_the_returning_ole_loop() {
        let active = bridge();
        active.receive(message(53, DragMessage::PREPARE), true);
        active.receive(message(53, DragMessage::ACTIVATE), false);
        assert!(active.suspend_target_for_source_return());

        active.receive(message(53, DragMessage::ACTIVATE), false);
        active.receive(message(53, DragMessage::RELEASE), false);
        assert_eq!(active.target_state.load(Ordering::Acquire), CANCELLED);
        active.target_state.store(PREPARING, Ordering::Release);
        assert!(active.begin_target_presentation());
        assert_eq!(active.target_state.load(Ordering::Acquire), RELEASED);
        assert_eq!(active.poll(), None);

        let cancelled = bridge();
        cancelled.receive(message(54, DragMessage::PREPARE), true);
        cancelled.receive(message(54, DragMessage::ACTIVATE), false);
        assert!(cancelled.suspend_target_for_source_return());
        cancelled.receive(message(54, DragMessage::CANCEL), false);
        cancelled.receive(message(54, DragMessage::ACTIVATE), false);
        cancelled.target_state.store(PREPARING, Ordering::Release);
        assert!(!cancelled.begin_target_presentation());
        assert_eq!(cancelled.target_state.load(Ordering::Acquire), CANCELLED);

        let released_before_activation = bridge();
        released_before_activation.receive(message(55, DragMessage::PREPARE), true);
        released_before_activation.receive(message(55, DragMessage::ACTIVATE), false);
        assert!(released_before_activation.suspend_target_for_source_return());
        released_before_activation.receive(message(55, DragMessage::RELEASE), false);
        released_before_activation.receive(message(55, DragMessage::ACTIVATE), false);
        released_before_activation
            .target_state
            .store(PREPARING, Ordering::Release);
        assert!(!released_before_activation.begin_target_presentation());
        assert_eq!(
            released_before_activation
                .target_state
                .load(Ordering::Acquire),
            CANCELLED
        );
    }

    #[test]
    fn reverse_and_terminal_targets_are_never_suspended_as_round_trip_targets() {
        let bridge = bridge();
        bridge.receive_for_route(message(52, DragMessage::PREPARE), true, true);
        assert!(!bridge.suspend_target_for_source_return());
        bridge.target_reverse.store(false, Ordering::Release);
        bridge.target_state.store(RELEASED, Ordering::Release);
        assert!(!bridge.suspend_target_for_source_return());
    }

    #[test]
    fn reverse_target_can_pause_for_source_reentry_and_rearm_repeatedly() {
        let bridge = bridge();
        bridge.receive_for_route(message(56, DragMessage::PREPARE), true, true);
        bridge.target_prepared.store(true, Ordering::Release);
        bridge.receive_for_route(message(56, DragMessage::ACTIVATE), false, true);
        bridge.activate_local_target();
        assert!(bridge.active_reverse_target());
        assert!(bridge.may_cross(true));

        for _ in 0..4 {
            assert!(bridge.suspend_reverse_target_for_source_reentry());
            assert!(!bridge.active_reverse_target());
            assert!(!bridge.may_cross(true));
            assert_eq!(bridge.target_state.load(Ordering::Acquire), CANCELLED);
            assert!(bridge.target_returning.load(Ordering::Acquire));
            assert!(!bridge.target_prepared.load(Ordering::Acquire));
            assert!(!bridge.target_armed.load(Ordering::Acquire));

            bridge.target_state.store(PREPARING, Ordering::Release);
            assert!(bridge.begin_target_presentation());
            bridge.target_prepared.store(true, Ordering::Release);
            bridge.receive_for_route(message(56, DragMessage::ACTIVATE), false, true);
            assert!(bridge.reverse_handoff_ready());
            bridge.activate_local_target();
            assert!(bridge.active_reverse_target());
            assert!(bridge.may_cross(true));
            assert_eq!(bridge.target_state.load(Ordering::Acquire), ACTIVE);
        }
        assert_eq!(bridge.poll(), None);
    }

    #[test]
    fn release_during_return_ack_is_delivered_after_target_activation() {
        let source = bridge();
        source.classify_idle_source(true);
        source.begin_source_capture(57);
        source.source_ready.store(true, Ordering::Release);
        source.source_active.store(true, Ordering::Release);
        let target = bridge();
        target.receive_for_route(message(57, DragMessage::PREPARE), true, true);
        target.target_prepared.store(true, Ordering::Release);
        target.receive_for_route(message(57, DragMessage::ACTIVATE), true, true);
        // Up crossed the network before the return ack. It cannot end the offer yet.
        source.observe_remote_source_left_event(false);
        assert!(target.reverse_handoff_ready());
        target.complete_local_target_handoff(false);
        assert_eq!(target.target_state.load(Ordering::Acquire), RELEASED);
        let release = target
            .poll()
            .expect("handoff must deliver the pending physical release");
        assert_eq!(release, message(57, DragMessage::RELEASE));
        source.receive(release, false);
        assert!(source.source_released.load(Ordering::Acquire));
        assert!(source.remote_source_left_up.lock().unwrap().is_none());
        source.receive(message(57, DragMessage::FINISHED), false);
        assert_eq!(source.source_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn reverse_arrival_during_presentation_teardown_is_deferred_not_dropped() {
        // 2026-10-11 21:49: control returned while the suspended presentation was still leaving
        // its OLE loop; the arrival was dropped and the next crossing was blocked.
        let target = bridge();
        let id = 59;
        target.receive_for_route(message(id, DragMessage::PREPARE_TUNNEL), true, true);
        assert!(target.begin_target_presentation());
        target.target_prepared.store(true, Ordering::Release);
        target.receive_for_route(message(id, DragMessage::ACTIVATE), false, true);
        target.reverse_control_arrived(true);
        assert_eq!(target.target_state.load(Ordering::Acquire), ACTIVE);

        assert!(target.suspend_reverse_target_for_source_reentry());
        assert!(target.target_returning.load(Ordering::Acquire));
        target.reverse_control_arrived(true);
        assert!(target.reverse_arrival_pending.load(Ordering::Acquire));

        target.target_state.store(PREPARING, Ordering::Release);
        assert!(target.begin_target_presentation());
        target.target_prepared.store(true, Ordering::Release);
        target.receive_for_route(message(id, DragMessage::ACTIVATE), false, true);
        assert_eq!(target.target_state.load(Ordering::Acquire), ACTIVE);
    }

    #[test]
    fn controlled_source_hands_off_before_return_and_survives_its_injected_release() {
        // 2026-10-10 17:48: the multi-device runtime returned control before activating the
        // controlled source; the release injected by the return then cancelled the offer.
        let source = bridge();
        let target = bridge();
        let id = 58;
        source.classify_idle_source(true);
        source.begin_source_capture(id);
        target.receive_for_route(message(id, DragMessage::PREPARE_TUNNEL), true, true);
        assert!(target.begin_target_presentation());
        target.target_prepared.store(true, Ordering::Release);

        // Held at the edge before the reverse receiver is READY: wait, never return.
        assert!(!source.may_return(true));
        target.send(id, DragMessage::READY, 0);
        source.receive_for_route(target.poll().unwrap(), false, false);
        assert!(source.may_return(true));
        assert!(source.source_dragging());
        let activate = source.poll().unwrap();
        assert_eq!(activate, message(id, DragMessage::ACTIVATE));

        // Control commit releases the injected button on the source and reaches the target
        // before ACTIVATE does.
        source.observe_remote_source_left_event(false);
        source.discard_handed_off_remote_left_up();
        assert!(source.remote_source_left_up.lock().unwrap().is_none());
        assert!(source.may_cross(false));
        assert_eq!(source.poll(), None);
        target.reverse_control_arrived(true);
        assert_eq!(target.target_state.load(Ordering::Acquire), PREPARING);
        target.receive_for_route(activate, false, true);
        assert_eq!(target.target_state.load(Ordering::Acquire), ACTIVE);

        for _ in 0..3 {
            // 18:43: re-enter the source computer, then cross back without releasing. The target
            // must yield, and the source preview must end before the return's injected release.
            assert!(target.suspend_reverse_target_for_source_reentry());
            assert!(source.present_remote_source_after_peer_reentry());
            assert_eq!(source.take_source_return_presentation(), Some(id));
            assert_eq!(source.poll(), Some(message(id, DragMessage::ACTIVATE)));
            target.target_state.store(PREPARING, Ordering::Release);
            assert!(target.begin_target_presentation());
            target.target_prepared.store(true, Ordering::Release);
            assert!(source.suspend_source_return_presentation());
            source.observe_remote_source_left_event(false);
            source.finish_source_return_presentation(id);
            source.discard_handed_off_remote_left_up();
            assert!(!source.source_return_drop_requested.load(Ordering::Acquire));
            assert!(source.source_dragging());
            // READY lands after the preview ended; it alone must re-arm the receiver.
            target.send(id, DragMessage::READY, 0);
            source.receive_for_route(target.poll().unwrap(), false, false);
            target.receive_for_route(source.poll().unwrap(), false, true);
            target.reverse_control_arrived(true);
            assert_eq!(target.target_state.load(Ordering::Acquire), ACTIVE);
        }

        // The physical release on the target computer is the drop.
        assert!(target.release_local_target());
        source.receive_for_route(target.poll().unwrap(), false, false);
        assert!(source.source_released.load(Ordering::Acquire));
    }

    #[test]
    fn controlled_source_and_reverse_target_complete_a_multi_crossing_round_trip() {
        let source = bridge();
        let target = bridge();
        let id = 57;
        source.classify_idle_source(true);
        source.begin_source_capture(id);
        source.source_active.store(true, Ordering::Release);
        assert!(source.source_remote_controlled.load(Ordering::Acquire));
        target.receive_for_route(message(id, DragMessage::PREPARE_TUNNEL), true, true);
        assert!(target.begin_target_presentation());
        target.target_prepared.store(true, Ordering::Release);
        target.send(id, DragMessage::READY, 0);
        source.receive_for_route(target.poll().unwrap(), false, false);
        source.activate_source();
        target.receive_for_route(source.poll().unwrap(), false, true);
        target.activate_local_target();
        assert_eq!(target.target_state.load(Ordering::Acquire), ACTIVE);

        for _ in 0..4 {
            // The physical controller leaves the synthetic target and re-enters the computer
            // that owns the original Explorer IDataObject.
            assert!(target.suspend_reverse_target_for_source_reentry());
            assert!(source.present_remote_source_after_peer_reentry());
            assert_eq!(source.take_source_return_presentation(), Some(id));
            // Model ACTIVATE winning the race. It reaches the still-cancelling target first and
            // is intentionally ignored; the READY callback below must trigger a second ACTIVATE.
            target.receive_for_route(source.poll().unwrap(), false, true);
            assert!(!target.target_armed.load(Ordering::Acquire));
            target.target_state.store(PREPARING, Ordering::Release);
            assert!(target.begin_target_presentation());
            target.target_prepared.store(true, Ordering::Release);
            target.send(id, DragMessage::READY, 0);
            source.receive_for_route(target.poll().unwrap(), false, false);
            target.receive_for_route(source.poll().unwrap(), false, true);
            assert!(target.reverse_handoff_ready());

            // Crossing back suspends only the temporary source presentation. The authenticated
            // offer survives and the already rearmed target becomes the active OLE drag again.
            assert!(source.suspend_source_return_presentation());
            source.finish_source_return_presentation(id);
            assert!(source.source_dragging());
            target.activate_local_target();
            assert_eq!(target.target_state.load(Ordering::Acquire), ACTIVE);
        }

        assert!(target.release_local_target());
        source.receive_for_route(target.poll().unwrap(), false, false);
        assert!(source.source_released.load(Ordering::Acquire));
        target.send(id, DragMessage::FINISHED, 0);
        source.receive_for_route(target.poll().unwrap(), false, false);
        assert_eq!(source.source_id.load(Ordering::Acquire), 0);
        assert_eq!(source.poll(), None);
        assert_eq!(target.poll(), None);
    }

    #[test]
    fn reverse_drag_waits_for_source_arm_and_control_return_before_native_activation() {
        let bridge = bridge();
        bridge.receive_for_route(message(11, DragMessage::PREPARE), true, true);
        bridge.target_prepared.store(true, Ordering::Release);
        assert!(!bridge.may_return(true));
        bridge.receive_for_route(message(11, DragMessage::ACTIVATE), false, true);
        assert!(bridge.may_return(true));
        assert_eq!(bridge.target_state.load(Ordering::Acquire), PREPARING);
        bridge.activate_local_target();
        assert_eq!(bridge.target_state.load(Ordering::Acquire), ACTIVE);
        assert!(bridge.release_local_target());
        assert!(bridge.release_local_target());
        assert_eq!(bridge.poll(), Some(message(11, DragMessage::RELEASE)));
        assert_eq!(bridge.poll(), None);
        assert_eq!(bridge.target_state.load(Ordering::Acquire), RELEASED);
    }

    #[test]
    fn reverse_early_release_cancels_instead_of_leaving_an_armed_drag() {
        let bridge = bridge();
        bridge.receive_for_route(message(11, DragMessage::PREPARE), true, true);
        assert!(bridge.release_local_target());
        bridge.receive_for_route(message(11, DragMessage::ACTIVATE), false, true);
        bridge.activate_local_target();
        assert_eq!(bridge.target_state.load(Ordering::Acquire), CANCELLED);
        assert_eq!(bridge.poll(), Some(message(11, DragMessage::CANCEL)));
    }

    #[test]
    fn returned_remote_source_cancels_on_authenticated_release_without_local_copy() {
        let bridge = Arc::new(bridge());
        bridge.classify_idle_source(true);
        bridge.begin_source_capture(81);
        bridge.source_ready.store(true, Ordering::Release);
        bridge.local_candidate.store(true, Ordering::Release);
        bridge.source_active.store(true, Ordering::Release);
        assert!(bridge.present_remote_source_after_peer_reentry());
        assert_eq!(bridge.take_source_return_presentation(), Some(81));
        assert!(source_return_preview_active(&bridge, 81));
        bridge.observe_remote_source_left_event(false);
        assert!(!source_return_preview_active(&bridge, 81));
        assert!(bridge.remote_source_left_up.lock().unwrap().is_none());
        bridge.finish_source_return_presentation(81);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 0);
        assert_eq!(bridge.poll(), Some(message(81, DragMessage::ACTIVATE)));
        assert_eq!(bridge.poll(), Some(message(81, DragMessage::CANCEL)));
        bridge.begin_source_capture(82);
        assert!(!bridge.source_return_drop_requested.load(Ordering::Acquire));
    }

    #[test]
    fn suspended_reverse_receiver_does_not_cancel_remote_source_release() {
        let bridge = bridge();
        bridge.receive_for_route(message(83, DragMessage::PREPARE), true, true);
        bridge.target_prepared.store(true, Ordering::Release);
        bridge.receive_for_route(message(83, DragMessage::ACTIVATE), false, true);
        bridge.activate_local_target();
        assert!(bridge.suspend_reverse_target_for_source_reentry());
        assert!(!bridge.release_local_target());
        assert_eq!(bridge.poll(), None);
        assert_eq!(bridge.target_id.load(Ordering::Acquire), 83);
    }

    #[test]
    fn remote_source_release_keeps_offer_until_receiver_finishes() {
        let bridge = bridge();
        bridge.source_id.store(8, Ordering::Release);
        bridge
            .source_remote_controlled
            .store(true, Ordering::Release);
        bridge.receive(message(8, DragMessage::RELEASE), false);
        bridge.cancel();
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 8);
        assert_eq!(bridge.poll(), None);
        bridge.receive(message(8, DragMessage::FINISHED), false);
        assert_eq!(bridge.source_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn remote_candidate_holds_return_while_source_icon_is_still_preparing() {
        let bridge = bridge();
        bridge.receive_for_route(message(21, DragMessage::CANDIDATE), true, true);
        *bridge.return_wait.lock().unwrap() = Instant::now().checked_sub(Duration::from_secs(1));
        assert!(!bridge.may_return(true));
        bridge.receive_for_route(message(21, DragMessage::PREPARE), true, true);
        assert!(!bridge.may_return(true));
        bridge.target_prepared.store(true, Ordering::Release);
        bridge.receive_for_route(message(21, DragMessage::ACTIVATE), false, true);
        assert!(bridge.may_return(true));
    }

    #[test]
    fn drag_connection_failure_explains_the_receiving_pc_and_recovery_in_both_languages() {
        use crate::app_settings::UiLanguage;
        let endpoint = "192.168.1.13:45233".parse().unwrap();
        let chinese = drag_connection_failure_message(
            "receiver",
            endpoint,
            "timeout",
            UiLanguage::ChineseSimplified,
        );
        assert!(chinese.contains("receiver"));
        assert!(chinese.contains("192.168.1.13:45233"));
        assert!(chinese.contains("原因未确认"));
        assert!(chinese.contains("接收电脑"));
        assert!(chinese.contains("实际使用的网络类别"));
        assert!(chinese.contains("重新拖拽"));
        let english =
            drag_connection_failure_message("receiver", endpoint, "timeout", UiLanguage::English);
        assert!(english.contains("cause unconfirmed"));
        assert!(english.contains("receiving PC"));
        assert!(english.contains("active network profile"));
        assert!(english.contains("drag again"));
    }

    #[test]
    fn drag_failure_notification_is_once_and_only_its_peer_can_clear_it() {
        let peer = CertificateFingerprint::from_certificate(b"failed-drag-peer");
        *CONNECTION_FAILURE.lock().unwrap() = Some(DragConnectionFailure {
            peer,
            message: "actionable failure".to_owned(),
            created: Instant::now(),
            pending: true,
        });
        assert_eq!(
            take_connection_failure_notification().as_deref(),
            Some("actionable failure")
        );
        assert!(take_connection_failure_notification().is_none());
        clear_connection_failure(CertificateFingerprint::from_certificate(b"other-peer"));
        assert!(CONNECTION_FAILURE.lock().unwrap().is_some());
        clear_connection_failure(peer);
        assert!(CONNECTION_FAILURE.lock().unwrap().is_none());
    }

    #[test]
    fn offer_transport_is_explicit_and_independent_from_pointer_direction() {
        let bridge = bridge();
        bridge.receive_for_route(message(31, DragMessage::PREPARE_TUNNEL), true, false);
        assert!(bridge.target_tunnel.load(Ordering::Acquire));
        assert!(!bridge.target_reverse.load(Ordering::Acquire));

        bridge.target_id.store(0, Ordering::Release);
        bridge.receive_for_route(message(32, DragMessage::PREPARE), true, true);
        assert!(!bridge.target_tunnel.load(Ordering::Acquire));
        assert!(bridge.target_reverse.load(Ordering::Acquire));
    }

    #[test]
    fn cancelling_clears_peer_candidate_and_wait_debouncers() {
        let bridge = bridge();
        bridge.receive_for_route(message(41, DragMessage::CANDIDATE), true, true);
        *bridge.return_wait.lock().unwrap() = Some(Instant::now());
        *bridge.edge_wait.lock().unwrap() = Some(Instant::now());

        bridge.cancel();

        assert!(bridge.peer_candidate.lock().unwrap().is_none());
        assert!(bridge.return_wait.lock().unwrap().is_none());
        assert_eq!(bridge.poll(), Some(message(41, DragMessage::CANCEL)));
    }

    #[test]
    fn failed_reverse_offer_suppresses_exactly_one_returned_left_restore() {
        let bridge = bridge();
        bridge.receive_for_route(message(51, DragMessage::CANDIDATE), true, true);
        bridge.receive_for_route(message(51, DragMessage::CANCEL), false, true);

        assert!(!bridge.may_return(true));
        assert!(bridge.may_return(false));

        assert!(bridge.take_failed_reverse_left_restore());
        assert!(!bridge.take_failed_reverse_left_restore());

        bridge.receive_for_route(message(52, DragMessage::CANDIDATE), true, true);
        bridge.receive_for_route(message(52, DragMessage::CANCEL), false, true);
        bridge.receive_for_route(message(53, DragMessage::CANDIDATE), true, true);
        assert!(!bridge.take_failed_reverse_left_restore());
    }

    #[test]
    fn timed_out_reverse_candidate_also_suppresses_returned_left_restore() {
        let bridge = bridge();
        bridge.receive_for_route(message(61, DragMessage::CANDIDATE), true, true);
        *bridge.peer_candidate.lock().unwrap() = Some((
            61,
            Instant::now()
                .checked_sub(SOURCE_PREPARE_TIMEOUT + Duration::from_millis(1))
                .unwrap(),
            true,
        ));

        assert!(!bridge.may_return(true));
        assert_eq!(bridge.poll(), Some(message(61, DragMessage::CANCEL)));
        assert!(bridge.take_failed_reverse_left_restore());
    }
}
