#[cfg(test)]
use super::transfer_speed::RateSample as TransferSample;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CLIPBRD_E_CANT_OPEN, E_UNEXPECTED, HINSTANCE, HWND, LPARAM, LRESULT, S_OK, WPARAM,
};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber, RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    OleFlushClipboard, OleInitialize, OleSetClipboard, OleUninitialize,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::IDataObjectAsyncCapability;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    KillTimer, MSG, PostThreadMessageW, RegisterClassW, SetTimer, TranslateMessage,
    UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_QUIT, WM_TIMER,
    WNDCLASSW,
};
use windows_core::{Error, HRESULT, Interface, Result as WindowsResult, w};

use super::data_object::{SourceFactory, VirtualFileDataObject, VirtualFileEntry};
use super::local_file::{
    CaptureError, CaptureFormats, ClipboardCapture, FileTreeSnapshot, LocalOfferRegistry,
    capture_files_from_clipboard,
};
use super::probe::ProbeState;
use super::runtime::{ole_flush_clipboard_with_retry, ole_set_clipboard_with_retry};
use super::secure_transfer::{
    OfferManifest, RemoteTransferRegistry, SecureOfferClient, SecureOfferServer, SecureOfferedFile,
    SecureRecoveryPolicy,
};
use super::shared_data_object::{PendingSharedText, SharedTextDataObject};
use super::shared_protocol::{
    SHARED_FAILED_ACK, SHARED_INSTALLED_ACK, SHARED_MAGIC, SHARED_READY_ACK, SHARED_REJECTED_ACK,
    SharedTextBegin, SharedTextFinish, read_begin_after_magic, read_finish, write_begin,
    write_failed, write_ready,
};
use super::snapshot::{
    ClipboardEventId, SharedCaptureFormats, SharedCapturePolicy, SharedClipboardCapture,
    SharedClipboardSnapshot, SharedFormatKind, capture_shared_clipboard_isolated,
};
use super::source::ReadAtSource;
use crate::app_settings::{AppSettings, SharedClipboardSettings, validate_private_endpoint};
use crate::cross_screen::{RemoteBoundaryWorkspace, decode_boundary_sync};
use crate::device_store::DeviceStore;
use crate::security::{
    AuthenticatedServerConnection, CertificateFingerprint, PinnedTlsClient, TrustedTlsServer,
};

const PRODUCT_WINDOW_CLASS: windows_core::PCWSTR = w!("PierdeskProductClipboardWindow");
const PRODUCT_EVENT_MESSAGE: u32 = WM_APP + 40;
const PRODUCT_COMMAND_MESSAGE: u32 = WM_APP + 41;
const PRODUCT_TIMER_ID: usize = 0x4346;
const PRODUCT_TIMER_MS: u32 = 1000;
const OFFER_MAGIC: &[u8; 8] = b"CFOFFER1";
const OFFER_ACK: &[u8; 8] = b"CFACK001";
const DOWNLINK_MAGIC: &[u8; 8] = b"CFDOWN01";
const DOWNLINK_ACK: &[u8; 8] = b"CFDNACK1";
const REVERSE_TUNNEL_MAGIC: &[u8; 8] = b"CFREVR01";
const REVERSE_TUNNEL_ACK: &[u8; 8] = b"CFRVACK1";
const DRAG_REVERSE_TUNNEL_MAGIC: &[u8; 8] = b"CFDRGR01";
const DRAG_REVERSE_TUNNEL_ACK: &[u8; 8] = b"CFDGACK1";
const REVERSE_TUNNEL_START: u8 = 1;
const OFFER_FRAME_SIZE: usize = 8 + 1 + 16 + 2;
const TLS_TIMEOUT: Duration = Duration::from_secs(10);
const OFFER_TTL: Duration = Duration::from_mins(15);
const TRANSFER_TTL: Duration = Duration::from_hours(1);
const RECOVERY_WINDOW: Duration = Duration::from_mins(3);
const MAX_LIVE_SOURCE_OFFERS: usize = 16;
const REVERSE_TUNNEL_LANES: usize = 2;
const IDLE_FILE_CHANNEL_REFRESH: Duration = Duration::from_secs(30);
const SHARED_APPLY_TIMEOUT: Duration = Duration::from_secs(3);
const SHARED_FINISH_TIMEOUT: Duration = Duration::from_secs(7);
const MAX_SEEN_SHARED_EVENTS: usize = 256;
const MAX_LIVE_REMOTE_TASKS: usize = 16;
pub(crate) const BOUNDARY_WORKSPACE_MAGIC: &[u8; 8] = b"CFBNDY01";
pub(crate) const BOUNDARY_WORKSPACE_ACK: &[u8; 8] = b"CFBNDACK";
pub(crate) const BOUNDARY_WORKSPACE_PAYLOAD_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileChannelRole {
    Listener,
    Connector,
}

impl FileChannelRole {
    pub(crate) fn resolve(store: &DeviceStore, settings: &AppSettings) -> io::Result<Self> {
        let local = store.load_identity()?.fingerprint();
        if local == settings.active_peer {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file channel peer must differ from the local device",
            ));
        }
        Ok(if local.bytes() < settings.active_peer.bytes() {
            Self::Listener
        } else {
            Self::Connector
        })
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Listener => "listener",
            Self::Connector => "connector",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductTransferState {
    AwaitingPaste,
    Running,
    Paused,
    Cancelled,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProductTransferDirection {
    Sending,
    Receiving,
}

impl ProductTransferDirection {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sending => "发送",
            Self::Receiving => "接收",
        }
    }

    #[must_use]
    pub const fn arrow(self) -> &'static str {
        match self {
            Self::Sending => "↑",
            Self::Receiving => "↓",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProductTransferId {
    direction: ProductTransferDirection,
    offer_id: [u8; 16],
}

impl ProductTransferId {
    pub(crate) const fn new(direction: ProductTransferDirection, offer_id: [u8; 16]) -> Self {
        Self {
            direction,
            offer_id,
        }
    }

    #[must_use]
    pub const fn direction(self) -> ProductTransferDirection {
        self.direction
    }

    #[must_use]
    pub(crate) const fn offer_id(self) -> [u8; 16] {
        self.offer_id
    }

    #[must_use]
    pub fn short_label(self) -> String {
        format!(
            "{:02x}{:02x}{:02x}{:02x}",
            self.offer_id[0], self.offer_id[1], self.offer_id[2], self.offer_id[3]
        )
    }
}

impl ProductTransferState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AwaitingPaste => "等待粘贴",
            Self::Running => "传输中",
            Self::Paused => "已暂停",
            Self::Cancelled => "已取消",
            Self::Completed => "已完成",
            Self::Failed => "失败",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductManifestSummary {
    pub direction: &'static str,
    pub primary_name: String,
    pub items: usize,
    pub files: usize,
    pub directories: usize,
    pub total_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductTransferSnapshot {
    pub state: ProductTransferState,
    pub transferred: u64,
    pub total_size: u64,
    pub bytes_per_second: u64,
    pub average_bytes_per_second: u64,
    pub started_files: usize,
    pub total_files: usize,
    pub current_file_name: Option<String>,
    pub current_file_transferred: u64,
    pub current_file_size: u64,
    pub reconnect_attempts: u64,
    pub recovered_commands: u64,
    pub recovery_active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductTransferTaskSnapshot {
    pub id: ProductTransferId,
    pub generation: u64,
    pub manifest: ProductManifestSummary,
    pub transfer: ProductTransferSnapshot,
    pub files: Vec<ProductFileSnapshot>,
    pub source_location: String,
    pub target_location: String,
    pub started_unix_ms: u64,
    pub completed_unix_ms: u64,
}

impl ProductTransferTaskSnapshot {
    /// Active tasks can be controlled from the sending or receiving device.
    #[must_use]
    pub fn locally_controllable(&self) -> bool {
        local_task_control_supported(self.id.direction(), self.transfer.state)
    }
}

fn local_task_control_supported(
    direction: ProductTransferDirection,
    state: ProductTransferState,
) -> bool {
    let _ = direction;
    matches!(
        state,
        ProductTransferState::AwaitingPaste
            | ProductTransferState::Running
            | ProductTransferState::Paused
    )
}

fn require_receiving_task(id: ProductTransferId) -> io::Result<()> {
    if id.direction() == ProductTransferDirection::Sending {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Cancel selected files on the receiving device.",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductFileSnapshot {
    pub id: [u8; 16],
    pub name: String,
    pub size: u64,
    pub transferred: u64,
    pub bytes_per_second: u64,
    pub state: ProductTransferState,
    pub is_directory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProductSnapshot {
    pub listener_running: bool,
    pub local_endpoint: SocketAddr,
    pub active_peer_label: String,
    pub active_peer: CertificateFingerprint,
    pub auto_receive: bool,
    pub pending_offer: bool,
    pub last_manifest: Option<ProductManifestSummary>,
    pub transfer: Option<ProductTransferSnapshot>,
    pub transfer_generation: u64,
    pub transfer_tasks: Vec<ProductTransferTaskSnapshot>,
    pub shared_text_enabled: bool,
    pub shared_text_generation: u64,
    pub last_shared_text: Option<ProductSharedTextSummary>,
    pub remote_boundary_workspace: Option<Arc<RemoteBoundaryWorkspace>>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSharedTextSummary {
    pub direction: &'static str,
    pub utf16_units: usize,
    pub bytes: usize,
    pub formats: String,
}

impl ProductSnapshot {
    fn initial(
        settings: &AppSettings,
        shared_settings: SharedClipboardSettings,
        peer_label: String,
    ) -> Self {
        Self {
            listener_running: false,
            local_endpoint: settings.local_endpoint,
            active_peer_label: peer_label,
            active_peer: settings.active_peer,
            auto_receive: settings.auto_receive,
            pending_offer: false,
            last_manifest: None,
            transfer: None,
            transfer_generation: 0,
            transfer_tasks: Vec::new(),
            shared_text_enabled: shared_settings.any_enabled(),
            shared_text_generation: 0,
            last_shared_text: None,
            remote_boundary_workspace: None,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductCommand {
    AcceptPending,
    Pause,
    Resume,
    Cancel,
    PauseTask(ProductTransferId),
    ResumeTask(ProductTransferId),
    CancelTask(ProductTransferId),
    CancelFiles(ProductTransferId, Vec<[u8; 16]>),
}

pub struct ProductRuntime {
    rates: Mutex<super::transfer_speed::SnapshotRates>,
    snapshot: Arc<Mutex<ProductSnapshot>>,
    command_sender: mpsc::Sender<WorkerCommand>,
    worker_thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

impl ProductRuntime {
    /// Starts the authenticated offer inbox and the OLE clipboard worker.
    ///
    /// # Errors
    ///
    /// Returns an error when settings, trust, TLS, listener, COM, or worker startup fails.
    pub fn start(store: DeviceStore, settings: AppSettings) -> io::Result<Self> {
        crate::license_runtime::require_access()?;
        let peer = store.load_peer(settings.active_peer)?;
        let shared_settings = SharedClipboardSettings::load_or_default(&store)?;
        let snapshot = Arc::new(Mutex::new(ProductSnapshot::initial(
            &settings,
            shared_settings,
            peer.label.clone(),
        )));
        let (command_sender, command_receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread_snapshot = Arc::clone(&snapshot);
        let worker = std::thread::Builder::new()
            .name("pierdesk-product-clipboard".to_owned())
            .spawn(move || {
                let result = run_worker(
                    store,
                    settings,
                    shared_settings,
                    &thread_snapshot,
                    &command_receiver,
                    &ready_sender,
                );
                if let Err(error) = result {
                    if let Ok(mut snapshot) = thread_snapshot.lock() {
                        snapshot.listener_running = false;
                        snapshot.last_error = Some(format!("后台服务已停止：{error}"));
                    }
                    let _ = ready_sender.try_send(Err(error));
                }
            })?;
        let worker_thread_id = match ready_receiver.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(error);
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "product clipboard worker did not start",
                ));
            }
        };
        Ok(Self {
            snapshot,
            rates: Mutex::new(super::transfer_speed::SnapshotRates::default()),
            command_sender,
            worker_thread_id,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> ProductSnapshot {
        let mut snapshot = self.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |value| value.clone(),
        );
        super::drag_live::merge_ui_snapshot(&mut snapshot);
        self.rates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .apply(&mut snapshot, Instant::now());
        snapshot
    }

    /// Queues a bounded product control command on the clipboard STA.
    ///
    /// # Errors
    ///
    /// Returns an error if the worker stopped or its message queue cannot be woken.
    pub fn command(&self, command: ProductCommand) -> io::Result<()> {
        self.command_sender
            .send(WorkerCommand::Control(command))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "product worker stopped"))?;
        post_worker_message(self.worker_thread_id, PRODUCT_COMMAND_MESSAGE)
    }

    /// Removes an authenticated boundary update only when it is still the item the UI handled.
    pub fn clear_remote_boundary_workspace(&self, sequence: u64) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if snapshot
            .remote_boundary_workspace
            .as_ref()
            .is_some_and(|workspace| workspace.sequence == sequence)
        {
            snapshot.remote_boundary_workspace = None;
        }
    }

    pub fn stop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let _ = self.command_sender.send(WorkerCommand::Stop);
        let _ = post_worker_message(self.worker_thread_id, PRODUCT_COMMAND_MESSAGE);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ProductRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Copy, Debug)]
struct OfferAnnouncement {
    endpoint: SocketAddr,
    peer: CertificateFingerprint,
}

enum WorkerEvent {
    RemoteOffer(OfferAnnouncement),
    LocalTextCapture(LocalTextCaptureEvent),
    RemoteSharedPending(RemoteSharedPendingEvent),
    RemoteSharedReady(RemoteSharedReadyEvent),
    RemoteSharedFailed {
        event_id: ClipboardEventId,
    },
    SharedInstalled {
        event_id: ClipboardEventId,
    },
    SharedSendResult {
        event_id: ClipboardEventId,
        elapsed_millis: u128,
        result: io::Result<()>,
    },
    RemoteBoundaryWorkspace {
        peer: CertificateFingerprint,
        payload: Vec<u8>,
        completion: mpsc::SyncSender<io::Result<()>>,
    },
}

struct LocalTextCaptureEvent {
    generation: u64,
    begin: SharedTextBegin,
    elapsed_millis: u128,
    result: Result<SharedClipboardCapture, String>,
}

struct RemoteSharedPendingEvent {
    begin: SharedTextBegin,
    peer: CertificateFingerprint,
    observed_local_generation: u64,
    observed_sequence: u32,
    completion: mpsc::SyncSender<io::Result<PendingSharedText>>,
}

struct RemoteSharedReadyEvent {
    snapshot: SharedClipboardSnapshot,
    elapsed_millis: u128,
    completion: mpsc::SyncSender<io::Result<()>>,
}

enum WorkerCommand {
    Control(ProductCommand),
    Stop,
}

struct ProductWindow {
    handle: HWND,
    instance: HINSTANCE,
}

impl ProductWindow {
    fn create() -> WindowsResult<Self> {
        // SAFETY: retrieving the current module does not transfer ownership.
        let module = unsafe { GetModuleHandleW(None) }?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(product_window_procedure),
            hInstance: instance,
            lpszClassName: PRODUCT_WINDOW_CLASS,
            ..Default::default()
        };
        // SAFETY: the class definition and constant name remain valid for the call.
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            return Err(Error::from_thread());
        }
        // SAFETY: the registered class creates a message-only window owned by this thread.
        let handle = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PRODUCT_WINDOW_CLASS,
                w!("Pierdesk Product Clipboard"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
        }?;
        // SAFETY: the live message-only window can receive clipboard notifications.
        if let Err(error) = unsafe { AddClipboardFormatListener(handle) } {
            let _ = unsafe { DestroyWindow(handle) };
            let _ = unsafe { UnregisterClassW(PRODUCT_WINDOW_CLASS, Some(instance)) };
            return Err(error);
        }
        Ok(Self { handle, instance })
    }
}

impl Drop for ProductWindow {
    fn drop(&mut self) {
        // SAFETY: this guard owns the listener registration, window and class.
        let _ = unsafe { RemoveClipboardFormatListener(self.handle) };
        let _ = unsafe { DestroyWindow(self.handle) };
        let _ = unsafe { UnregisterClassW(PRODUCT_WINDOW_CLASS, Some(self.instance)) };
    }
}

unsafe extern "system" fn product_window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: all messages are intentionally handled by the owning message loop.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

struct OleApartment;

impl OleApartment {
    fn initialize() -> WindowsResult<Self> {
        // SAFETY: the worker is a dedicated STA and balances this call in Drop.
        unsafe { OleInitialize(None) }?;
        Ok(Self)
    }
}

impl Drop for OleApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful initialization on the same worker thread.
        unsafe { OleUninitialize() };
    }
}

struct PasteFenceConfiguration;

impl Drop for PasteFenceConfiguration {
    fn drop(&mut self) {
        super::paste_fence::configure(false, unsafe { GetClipboardSequenceNumber() });
    }
}

struct ProductRemoteLease {
    object: IDataObject,
    task: Arc<RemoteTransferTask>,
    _probe: Arc<ProbeState>,
}

struct RemoteTransferTask {
    registry: Arc<RemoteTransferRegistry>,
    manifest: OfferManifest,
    generation: u64,
    clipboard_available: AtomicBool,
}

impl RemoteTransferTask {
    fn has_started_and_is_active(&self) -> bool {
        let progress = self.registry.local_progress();
        progress.started_transfers != 0 && !progress.cancelled && !self.files_terminal()
    }

    fn is_terminal_or_stale(&self) -> bool {
        let progress = self.registry.local_progress();
        progress.cancelled
            || self.files_terminal()
            || (progress.started_transfers == 0
                && !self.clipboard_available.load(Ordering::Acquire))
    }

    fn files_terminal(&self) -> bool {
        let files = self.registry.file_progress(&self.manifest);
        files.iter().any(|file| !file.is_directory)
            && files.iter().filter(|file| !file.is_directory).all(|file| {
                matches!(
                    file.state,
                    ProductTransferState::Completed
                        | ProductTransferState::Cancelled
                        | ProductTransferState::Failed
                )
            })
    }
}

impl ProductRemoteLease {
    fn register(client: &SecureOfferClient, generation: u64) -> WindowsResult<Self> {
        let manifest = client.fetch_manifest().map_err(io_to_windows)?;
        let registry = Arc::new(RemoteTransferRegistry::default());
        registry.bind_task(client.clone(), manifest.offer_id);
        let entries = manifest
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
                let source_factory: SourceFactory = Arc::new(move || {
                    let source = registry.create_source_for_entry(
                        client.clone(),
                        manifest.clone(),
                        entry.clone(),
                    );
                    let source: Arc<dyn ReadAtSource> = source;
                    source
                });
                VirtualFileEntry::file(descriptor, source_factory)
            })
            .collect();
        let probe = Arc::new(ProbeState::quiet());
        let object = VirtualFileDataObject::create_with_entries(
            entries,
            Arc::clone(&probe),
            manifest.origin_payload(),
        )?;
        let capability: IDataObjectAsyncCapability = object.cast()?;
        // SAFETY: the object implements the capability and remains alive in this lease.
        unsafe { capability.SetAsyncMode(true) }?;
        ole_set_clipboard_with_retry(&object)?;
        let task = Arc::new(RemoteTransferTask {
            registry,
            manifest,
            generation,
            clipboard_available: AtomicBool::new(true),
        });
        Ok(Self {
            object,
            task,
            _probe: probe,
        })
    }

    fn is_current(&self) -> bool {
        self.current_status() == S_OK
    }

    fn current_status(&self) -> HRESULT {
        // SAFETY: the COM pointer remains owned by this lease.
        unsafe { ole_is_current_clipboard_raw(Interface::as_raw(&self.object)) }
    }
}

#[link(name = "ole32")]
unsafe extern "system" {
    #[link_name = "OleIsCurrentClipboard"]
    fn ole_is_current_clipboard_raw(data_object: *mut c_void) -> HRESULT;
}

impl Drop for ProductRemoteLease {
    fn drop(&mut self) {
        self.task
            .clipboard_available
            .store(false, Ordering::Release);
        if self.is_current() {
            // SAFETY: only clear the clipboard while this lease still owns the current object.
            let _ = unsafe { OleSetClipboard(None::<&IDataObject>) };
        }
    }
}

struct ProductTextLease {
    object: IDataObject,
    event_id: ClipboardEventId,
    pending: PendingSharedText,
    flushed: bool,
    flush_deferred_since: Option<Instant>,
    flush_attempts: u32,
}

impl ProductTextLease {
    fn register_pending(begin: &SharedTextBegin) -> WindowsResult<Self> {
        let (object, pending) = SharedTextDataObject::create_pending(begin)?;
        ole_set_clipboard_with_retry(&object)?;
        Ok(Self {
            object,
            event_id: begin.event_id,
            pending,
            flushed: false,
            flush_deferred_since: None,
            flush_attempts: 0,
        })
    }

    fn is_current(&self) -> bool {
        // SAFETY: the COM pointer remains owned by this STA-local lease.
        (unsafe { ole_is_current_clipboard_raw(Interface::as_raw(&self.object)) }) == S_OK
    }
}

impl Drop for ProductTextLease {
    fn drop(&mut self) {
        self.pending.fail();
        if !self.flushed && self.is_current() {
            // Never reveal the target's pre-transfer clipboard after a failed/superseded pending
            // value. Clear only while this exact object still owns the clipboard.
            let _ = unsafe { OleSetClipboard(None::<&IDataObject>) };
        }
    }
}

struct LiveSourceOffer {
    _reverse_tunnel: ReverseOfferTunnel,
    server: SecureOfferServer,
    manifest: OfferManifest,
    generation: u64,
}

#[derive(Clone)]
struct TextCaptureRequest {
    generation: u64,
    begin: SharedTextBegin,
    started_at: Instant,
}

#[derive(Default)]
struct TextCaptureMailbox {
    pending: Option<TextCaptureRequest>,
    stopping: bool,
}

struct TextCaptureWorker {
    mailbox: Arc<(Mutex<TextCaptureMailbox>, Condvar)>,
    cancelled: Arc<AtomicBool>,
    latest_generation: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl TextCaptureWorker {
    fn start(
        event_sender: mpsc::Sender<WorkerEvent>,
        worker_thread_id: u32,
        policy: SharedCapturePolicy,
    ) -> io::Result<Self> {
        let mailbox = Arc::new((Mutex::new(TextCaptureMailbox::default()), Condvar::new()));
        let thread_mailbox = Arc::clone(&mailbox);
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let latest_generation = Arc::new(AtomicU64::new(0));
        let thread_latest_generation = Arc::clone(&latest_generation);
        let worker = std::thread::Builder::new()
            .name("pierdesk-shared-text-capture".to_owned())
            .spawn(move || {
                loop {
                    let request = {
                        let (lock, ready) = &*thread_mailbox;
                        let mut state = lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while state.pending.is_none() && !state.stopping {
                            state = ready
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        if state.stopping {
                            break;
                        }
                        state.pending.take().expect("capture request was checked")
                    };
                    let result = capture_shared_clipboard_isolated(
                        &thread_cancelled,
                        &thread_latest_generation,
                        request.generation,
                        policy,
                    );
                    if event_sender
                        .send(WorkerEvent::LocalTextCapture(LocalTextCaptureEvent {
                            generation: request.generation,
                            begin: request.begin,
                            elapsed_millis: request.started_at.elapsed().as_millis(),
                            result,
                        }))
                        .is_err()
                    {
                        break;
                    }
                    let _ = post_worker_message(worker_thread_id, PRODUCT_EVENT_MESSAGE);
                }
            })?;
        Ok(Self {
            mailbox,
            cancelled,
            latest_generation,
            worker: Some(worker),
        })
    }

    fn request(&self, request: TextCaptureRequest) {
        let (lock, ready) = &*self.mailbox;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.stopping {
            // Latest wins while a delayed clipboard owner is still rendering an older value.
            self.latest_generation
                .store(request.generation, Ordering::Release);
            state.pending = Some(request);
            ready.notify_one();
        }
    }

    fn observe_generation(&self, generation: u64) {
        self.latest_generation.store(generation, Ordering::Release);
    }

    fn stop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let (lock, ready) = &*self.mailbox;
        {
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopping = true;
            state.pending = None;
            ready.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for TextCaptureWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct SharedOutboundMailbox {
    latest: Option<Arc<SharedOutboundJob>>,
    stopping: bool,
}

struct SharedOutboundJob {
    trace: super::shared_diagnostics::SharedTrace,
    begin: SharedTextBegin,
    result: Mutex<Option<Result<SharedClipboardSnapshot, String>>>,
    changed: Condvar,
    cancelled: AtomicBool,
}

impl SharedOutboundJob {
    fn cancel(&self) {
        self.trace.mark("send_cancel", "reason=replaced_or_stopped");
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    fn wait_for_capture(&self) -> io::Result<SharedClipboardSnapshot> {
        let mut result = self
            .result
            .lock()
            .map_err(|_| io::Error::other("shared clipboard capture result was poisoned"))?;
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "a newer clipboard value replaced this send",
                ));
            }
            if let Some(result) = result.take() {
                return result.map_err(io::Error::other);
            }
            result = self
                .changed
                .wait(result)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

struct SharedOutbound {
    mailbox: Arc<(Mutex<SharedOutboundMailbox>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

enum SharedSendTransport {
    Direct {
        tls: PinnedTlsClient,
        peer_endpoint: SocketAddr,
    },
    Listener {
        downlink: Arc<OfferDownlink>,
        tls: PinnedTlsClient,
        peer_endpoint: SocketAddr,
    },
}

impl SharedSendTransport {
    fn send(
        &self,
        job: &SharedOutboundJob,
        event_sender: &mpsc::Sender<WorkerEvent>,
        worker_thread_id: u32,
    ) -> io::Result<()> {
        match self {
            Self::Direct { tls, peer_endpoint } => {
                send_shared_job(tls, *peer_endpoint, job, event_sender, worker_thread_id)
            }
            Self::Listener {
                downlink,
                tls,
                peer_endpoint,
            } => {
                // Reuse the established reverse channel when it exists. A listener can also be a
                // secondary workspace peer, however, and the legacy active peer is the only one
                // that owns such a channel. Send directly in that case instead of waiting two
                // seconds for a downlink that can never appear.
                if downlink.is_available() {
                    if job
                        .trace
                        .io("downlink_send", || {
                            downlink.send_shared(job, event_sender, worker_thread_id)
                        })
                        .is_ok()
                    {
                        return Ok(());
                    }
                    job.trace.mark("direct_fallback", "reason=downlink_failed");
                }
                send_shared_job(tls, *peer_endpoint, job, event_sender, worker_thread_id)
            }
        }
    }
}

impl SharedOutbound {
    fn start(
        store: &DeviceStore,
        settings: &AppSettings,
        file_role: FileChannelRole,
        downlink: Arc<OfferDownlink>,
        event_sender: mpsc::Sender<WorkerEvent>,
        worker_thread_id: u32,
    ) -> io::Result<Self> {
        let transport = match file_role {
            FileChannelRole::Connector => SharedSendTransport::Direct {
                tls: build_pinned_tls(store, settings)?,
                peer_endpoint: settings.peer_endpoint,
            },
            FileChannelRole::Listener => SharedSendTransport::Listener {
                downlink,
                tls: build_pinned_tls(store, settings)?,
                peer_endpoint: settings.peer_endpoint,
            },
        };
        let mailbox = Arc::new((Mutex::new(SharedOutboundMailbox::default()), Condvar::new()));
        let thread_mailbox = Arc::clone(&mailbox);
        let worker = std::thread::Builder::new()
            .name("pierdesk-shared-text-send".to_owned())
            .spawn(move || {
                loop {
                    let job = {
                        let (lock, ready) = &*thread_mailbox;
                        let mut state = lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while state.latest.is_none() && !state.stopping {
                            state = ready
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        if state.stopping {
                            break;
                        }
                        Arc::clone(state.latest.as_ref().expect("shared job was checked"))
                    };
                    let event_id = job.begin.event_id;
                    job.trace.mark("send_dequeued", "state=begin");
                    let started_at = Instant::now();
                    let result = job.trace.io("send_transport", || {
                        transport.send(&job, &event_sender, worker_thread_id)
                    });
                    if event_sender
                        .send(WorkerEvent::SharedSendResult {
                            event_id,
                            elapsed_millis: started_at.elapsed().as_millis(),
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                    let _ = post_worker_message(worker_thread_id, PRODUCT_EVENT_MESSAGE);
                    let (lock, _) = &*thread_mailbox;
                    let mut state = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state
                        .latest
                        .as_ref()
                        .is_some_and(|latest| latest.begin.event_id == event_id)
                    {
                        state.latest = None;
                    }
                }
            })?;
        Ok(Self {
            mailbox,
            worker: Some(worker),
        })
    }

    fn begin(&self, begin: SharedTextBegin) -> io::Result<()> {
        let (lock, ready) = &*self.mailbox;
        let mut state = lock
            .lock()
            .map_err(|_| io::Error::other("shared clipboard sender was poisoned"))?;
        if state.stopping {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "shared clipboard sender stopped",
            ));
        }
        if let Some(previous) = state.latest.replace(Arc::new(SharedOutboundJob {
            trace: super::shared_diagnostics::SharedTrace::new(&begin, "send"),
            begin,
            result: Mutex::new(None),
            changed: Condvar::new(),
            cancelled: AtomicBool::new(false),
        })) {
            // A clipboard is a current-value register, not a history queue. A new local copy
            // cancels the previous in-flight value; reconnect never replays offline history.
            previous.cancel();
        }
        ready.notify_one();
        Ok(())
    }

    fn complete(
        &self,
        event_id: ClipboardEventId,
        result: Result<SharedClipboardSnapshot, String>,
    ) -> io::Result<()> {
        let (lock, _) = &*self.mailbox;
        let state = lock
            .lock()
            .map_err(|_| io::Error::other("shared clipboard sender was poisoned"))?;
        let job = state
            .latest
            .as_ref()
            .filter(|job| job.begin.event_id == event_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "shared clipboard capture was superseded",
                )
            })?;
        let mut capture = job
            .result
            .lock()
            .map_err(|_| io::Error::other("shared clipboard capture result was poisoned"))?;
        job.trace.mark(
            "capture_result",
            if result.is_ok() {
                "state=ready"
            } else {
                "state=failed"
            },
        );
        *capture = Some(result);
        job.changed.notify_all();
        Ok(())
    }

    fn cancel_current(&self) {
        let (lock, ready) = &*self.mailbox;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(job) = state.latest.take() {
            job.cancel();
        }
        ready.notify_all();
    }

    fn stop(&mut self) {
        let (lock, ready) = &*self.mailbox;
        {
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopping = true;
            if let Some(job) = state.latest.take() {
                job.cancel();
            }
            ready.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for SharedOutbound {
    fn drop(&mut self) {
        self.stop();
    }
}

fn send_shared_job(
    tls: &PinnedTlsClient,
    peer_endpoint: SocketAddr,
    job: &SharedOutboundJob,
    event_sender: &mpsc::Sender<WorkerEvent>,
    worker_thread_id: u32,
) -> io::Result<()> {
    let mut stream = job.trace.io("connect", || {
        tls.connect_with_timeout(peer_endpoint, Duration::from_secs(2))
    })?;
    stream.sock.set_read_timeout(Some(SHARED_APPLY_TIMEOUT))?;
    stream.sock.set_write_timeout(Some(SHARED_APPLY_TIMEOUT))?;
    let result = exchange_shared_job(&mut stream, job, event_sender, worker_thread_id);
    let _ = stream.sock.shutdown(Shutdown::Both);
    result
}

fn exchange_shared_job(
    stream: &mut (impl io::Read + io::Write),
    job: &SharedOutboundJob,
    event_sender: &mpsc::Sender<WorkerEvent>,
    worker_thread_id: u32,
) -> io::Result<()> {
    job.trace
        .io("begin_write", || write_begin(&mut *stream, &job.begin))?;
    job.trace.io("installed_ack_read", || {
        read_shared_ack(&mut *stream, *SHARED_INSTALLED_ACK, "installation")
    })?;
    event_sender
        .send(WorkerEvent::SharedInstalled {
            event_id: job.begin.event_id,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard worker stopped"))?;
    post_worker_message(worker_thread_id, PRODUCT_EVENT_MESSAGE)?;

    match job.trace.io("capture_wait", || job.wait_for_capture()) {
        Ok(snapshot) => {
            let snapshot_begin = SharedTextBegin::from_snapshot(&snapshot)?;
            if snapshot_begin != job.begin {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "shared clipboard capture does not match its announced event",
                ));
            }
            job.trace
                .io("payload_write", || write_ready(&mut *stream, &snapshot))?;
            job.trace.io("ready_ack_read", || {
                read_shared_ack(&mut *stream, *SHARED_READY_ACK, "ready")
            })?;
        }
        Err(error) => {
            write_failed(&mut *stream)?;
            read_shared_ack(&mut *stream, *SHARED_FAILED_ACK, "failure")?;
            return Err(error);
        }
    }
    Ok(())
}

fn read_shared_ack(stream: &mut impl io::Read, expected: [u8; 8], phase: &str) -> io::Result<()> {
    let mut acknowledgement = [0_u8; 8];
    stream.read_exact(&mut acknowledgement)?;
    if acknowledgement == expected {
        return Ok(());
    }
    if acknowledgement == *SHARED_REJECTED_ACK {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("peer rejected shared clipboard during {phase}"),
        ));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("peer returned an invalid shared clipboard {phase} acknowledgement"),
    ))
}

struct ProductSession {
    store: DeviceStore,
    settings: AppSettings,
    formats: CaptureFormats,
    shared_formats: Option<SharedCaptureFormats>,
    shared_settings: SharedClipboardSettings,
    text_capture: Option<TextCaptureWorker>,
    shared_outbound: Option<SharedOutbound>,
    snapshot: Arc<Mutex<ProductSnapshot>>,
    registry: LocalOfferRegistry,
    sources: Vec<LiveSourceOffer>,
    pending: Option<OfferAnnouncement>,
    remote: Option<ProductRemoteLease>,
    remote_tasks: VecDeque<Arc<RemoteTransferTask>>,
    remote_text: Option<ProductTextLease>,
    active_direction: Option<TransferDirection>,
    last_clipboard_sequence: Option<u32>,
    local_generation: Arc<std::sync::atomic::AtomicU64>,
    shared_revision: u64,
    local_fingerprint: CertificateFingerprint,
    active_outbound_shared: Option<ActiveOutboundShared>,
    seen_shared_events: VecDeque<ClipboardEventId>,
    last_outbound_shared: Option<(ClipboardEventId, u64)>,
    log_path: std::path::PathBuf,
    next_transfer_generation: u64,
    next_boundary_sync_sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferDirection {
    Source,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClipboardOrder {
    revision: u64,
    source: CertificateFingerprint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveOutboundShared {
    event_id: ClipboardEventId,
    generation: u64,
    order: ClipboardOrder,
}

impl ClipboardOrder {
    fn is_newer_than(self, other: Self) -> bool {
        (self.revision, self.source.bytes()) > (other.revision, other.source.bytes())
    }
}

impl ProductSession {
    fn allocate_transfer_generation(&mut self) -> u64 {
        self.next_transfer_generation = self.next_transfer_generation.wrapping_add(1);
        if self.next_transfer_generation == 0 {
            self.next_transfer_generation = 1;
        }
        self.next_transfer_generation
    }

    fn handle_clipboard_update(&mut self, window: HWND) {
        // SAFETY: this reads the process-wide clipboard sequence without taking ownership.
        let observed_sequence = unsafe { GetClipboardSequenceNumber() };
        // Claim the notification before any clipboard read. Successful captures, explicit policy
        // rejections, and transient read failures must all be deduplicated uniformly.
        if !claim_clipboard_sequence(&mut self.last_clipboard_sequence, observed_sequence) {
            return;
        }
        let mut generation = self
            .local_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        if generation == 0 {
            generation = 1;
            self.local_generation.store(generation, Ordering::Release);
        }
        super::paste_fence::begin(observed_sequence, generation);
        if let Some(capture) = self.text_capture.as_ref() {
            capture.observe_generation(generation);
        }
        if let Some(outbound) = self.shared_outbound.as_ref() {
            outbound.cancel_current();
        }
        self.active_outbound_shared = None;
        if !self.shared_settings.any_enabled() {
            super::paste_fence::mark_not_shared(generation);
        }
        if self
            .remote
            .as_ref()
            .is_some_and(ProductRemoteLease::is_current)
            || self
                .remote_text
                .as_ref()
                .is_some_and(ProductTextLease::is_current)
        {
            super::paste_fence::observe_remote(observed_sequence);
            return;
        }
        self.retire_replaced_remote_file();
        self.shared_revision = self.shared_revision.saturating_add(1).max(1);
        self.remote_text.take();
        match capture_files_from_clipboard(window, self.formats) {
            Ok(ClipboardCapture::Candidates { paths, sequence }) => {
                super::paste_fence::mark_not_shared(generation);
                if sequence != 0 {
                    self.last_clipboard_sequence = Some(sequence);
                }
                if let Err(error) = self.publish_paths(&paths) {
                    let message = format!("本机文件清单发布失败：{error}");
                    if error.kind() == io::ErrorKind::Unsupported {
                        self.set_background_error(message);
                    } else {
                        self.set_error(message);
                    }
                }
            }
            Ok(ClipboardCapture::Rejected(reason)) => {
                super::paste_fence::mark_not_shared(generation);
                self.set_background_error(format!(
                    "本机文件剪贴板未发送：{}",
                    reason.user_message()
                ));
            }
            Ok(ClipboardCapture::NotFileClipboard) => {
                self.begin_local_shared_capture(generation);
            }
            Ok(ClipboardCapture::PrivateOffer) => {
                super::paste_fence::mark_not_shared(generation);
            }
            Err(error) => {
                super::paste_fence::mark_failed(generation);
                self.set_error(format!("读取本机剪贴板失败：{error}"));
            }
        }
    }

    fn begin_local_shared_capture(&mut self, generation: u64) {
        let policy = shared_capture_policy(self.shared_settings);
        let kinds = self
            .shared_formats
            .map_or_else(|| Arc::from([]), |formats| formats.announced_kinds(policy));
        if kinds.is_empty() {
            super::paste_fence::mark_not_shared(generation);
            return;
        }
        let event_id = match ClipboardEventId::random() {
            Ok(event_id) => event_id,
            Err(error) => {
                super::paste_fence::mark_failed(generation);
                self.set_background_error(format!("无法创建共享剪贴板事件：{error}"));
                return;
            }
        };
        let begin = match SharedTextBegin::from_kinds(event_id, self.shared_revision, &kinds) {
            Ok(begin) => begin,
            Err(error) => {
                super::paste_fence::mark_failed(generation);
                self.set_background_error(format!("无法创建共享剪贴板事件：{error}"));
                return;
            }
        };
        let started = self
            .shared_outbound
            .as_ref()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "shared clipboard sender is not running",
                )
            })
            .and_then(|outbound| outbound.begin(begin.clone()));
        if let Err(error) = started {
            super::paste_fence::mark_failed(generation);
            self.set_background_error(format!("共享剪贴板未送达：{error}"));
            return;
        }
        self.last_outbound_shared = Some((event_id, generation));
        self.active_outbound_shared = Some(ActiveOutboundShared {
            event_id,
            generation,
            order: ClipboardOrder {
                revision: begin.revision,
                source: self.local_fingerprint,
            },
        });
        let format_kinds = begin
            .formats
            .iter()
            .map(|format| format.kind.label())
            .collect::<Vec<_>>()
            .join("+");
        let _ = append_product_log(
            &self.log_path,
            &format!(
                "shared_clipboard phase=send_begin revision={} generation={generation} formats={} format_kinds={format_kinds}",
                begin.revision,
                begin.formats.len(),
            ),
        );
        if let Some(capture) = self.text_capture.as_ref() {
            capture.request(TextCaptureRequest {
                generation,
                begin,
                started_at: Instant::now(),
            });
        }
    }

    fn handle_local_text_capture(&mut self, event: LocalTextCaptureEvent) {
        if event.generation != self.local_generation.load(Ordering::Acquire) {
            return;
        }
        match event.result {
            Ok(SharedClipboardCapture::Snapshot { formats, sequence }) => {
                // A newer local clipboard value can be visible before its WM_CLIPBOARDUPDATE is
                // dispatched. Compare both the OS sequence and our claimed sequence before
                // publishing so an older capture can never overwrite/send the newer value.
                let current_sequence = unsafe { GetClipboardSequenceNumber() };
                if current_sequence != sequence
                    || (sequence != 0 && self.last_clipboard_sequence != Some(sequence))
                {
                    return;
                }
                let snapshot = SharedClipboardSnapshot {
                    event_id: event.begin.event_id,
                    revision: event.begin.revision,
                    sequence,
                    formats,
                };
                let summary = ProductSharedTextSummary {
                    direction: "发送",
                    utf16_units: snapshot.unicode_units(),
                    bytes: snapshot.total_bytes(),
                    formats: snapshot.format_labels(),
                };
                let result = self
                    .shared_outbound
                    .as_ref()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotConnected,
                            "shared clipboard sender is not running",
                        )
                    })
                    .and_then(|outbound| outbound.complete(event.begin.event_id, Ok(snapshot)));
                if let Err(error) = result {
                    super::paste_fence::mark_failed(event.generation);
                    self.set_background_error(format!("共享剪贴板未送达：{error}"));
                    return;
                }
                let _ = append_product_log(
                    &self.log_path,
                    &format!(
                        "shared_clipboard phase=capture_ready revision={} formats={} bytes={} elapsed_ms={}",
                        event.begin.revision, summary.formats, summary.bytes, event.elapsed_millis,
                    ),
                );
                self.update_snapshot(|state| {
                    state.shared_text_generation = state.shared_text_generation.wrapping_add(1);
                    state.last_shared_text = Some(summary);
                    state.last_error = None;
                });
            }
            Ok(
                SharedClipboardCapture::NotShareable
                | SharedClipboardCapture::PrivateShared
                | SharedClipboardCapture::Excluded
                | SharedClipboardCapture::ChangedDuringRead,
            ) => {
                let error = "共享剪贴板在读取期间失效或被替换".to_owned();
                if let Some(outbound) = self.shared_outbound.as_ref() {
                    let _ = outbound.complete(event.begin.event_id, Err(error));
                }
                super::paste_fence::mark_failed(event.generation);
            }
            Err(error) => {
                if let Some(outbound) = self.shared_outbound.as_ref() {
                    let _ = outbound.complete(event.begin.event_id, Err(error.clone()));
                }
                super::paste_fence::mark_failed(event.generation);
                self.set_background_error(format!("读取本机共享剪贴板失败：{error}"));
            }
        }
    }

    fn install_remote_shared(
        &mut self,
        event: &RemoteSharedPendingEvent,
    ) -> io::Result<PendingSharedText> {
        if !self.shared_settings.any_enabled() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "shared text is disabled on this device",
            ));
        }
        if self.seen_shared_events.contains(&event.begin.event_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "shared clipboard event was already applied",
            ));
        }
        self.shared_revision = self.shared_revision.max(event.begin.revision);
        let incoming_order = ClipboardOrder {
            revision: event.begin.revision,
            source: event.peer,
        };
        if let Some(active) = self.active_outbound_shared {
            if !incoming_order.is_newer_than(active.order) {
                let _ = append_product_log(
                    &self.log_path,
                    &format!(
                        "shared_clipboard phase=receive_conflict decision=keep_local incoming_revision={} local_revision={} local_generation={}",
                        incoming_order.revision, active.order.revision, active.generation,
                    ),
                );
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a simultaneous deterministically winning local clipboard event is active",
                ));
            }
            if let Some(outbound) = self.shared_outbound.as_ref() {
                outbound.cancel_current();
            }
            super::paste_fence::mark_failed(active.generation);
            self.active_outbound_shared = None;
            let _ = append_product_log(
                &self.log_path,
                &format!(
                    "shared_clipboard phase=receive_conflict decision=accept_remote incoming_revision={} local_revision={} local_generation={}",
                    incoming_order.revision, active.order.revision, active.generation,
                ),
            );
        }
        let current_generation = self.local_generation.load(Ordering::Acquire);
        let current_sequence = unsafe { GetClipboardSequenceNumber() };
        if current_generation != event.observed_local_generation
            || current_sequence != event.observed_sequence
            || (current_sequence != 0 && self.last_clipboard_sequence != Some(current_sequence))
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a newer local clipboard value won the shared clipboard race",
            ));
        }

        let remote_text =
            ProductTextLease::register_pending(&event.begin).map_err(windows_to_io)?;
        let pending = remote_text.pending.clone();
        self.retire_replaced_remote_file();
        self.remote_text = Some(remote_text);
        let installed_sequence = unsafe { GetClipboardSequenceNumber() };
        if installed_sequence != 0 {
            self.last_clipboard_sequence = Some(installed_sequence);
        }
        super::paste_fence::observe_remote(installed_sequence);
        let _ = append_product_log(
            &self.log_path,
            &format!(
                "shared_text phase=receive_installed revision={} sequence={installed_sequence}",
                event.begin.revision
            ),
        );
        Ok(pending)
    }

    fn retire_replaced_remote_file(&mut self) {
        let unstarted = self
            .remote
            .as_ref()
            .is_some_and(|remote| remote.task.registry.local_progress().started_transfers == 0);
        if unstarted && let Some(remote) = self.remote.as_ref() {
            let _ = remote.task.registry.cancel_all();
        }
        self.remote.take();
        if unstarted
            && !self
                .remote_tasks
                .iter()
                .any(|task| task.has_started_and_is_active())
        {
            self.active_direction = self
                .sources
                .iter()
                .any(|source| source.server.metrics().begun_transfers != 0)
                .then_some(TransferDirection::Source);
            self.update_snapshot(|snapshot| {
                if let Some(transfer) = snapshot.transfer.as_mut() {
                    transfer.state = ProductTransferState::Cancelled;
                    transfer.bytes_per_second = 0;
                }
            });
        }
    }

    fn finalize_remote_shared(&mut self, event: &RemoteSharedReadyEvent) -> io::Result<()> {
        let lease = self.remote_text.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "pending shared clipboard object is no longer installed",
            )
        })?;
        if lease.event_id != event.snapshot.event_id || !lease.is_current() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a newer local clipboard value won before shared text became ready",
            ));
        }
        // The inbox thread has already completed this object's synchronized text state. Flushing
        // renders the complete value into a static clipboard payload so it survives Pierdesk
        // exiting. Clipboard listeners can briefly hold the Win32 clipboard immediately after the
        // pending object was installed, so contention is retried and then deferred rather than
        // turning an already-usable remote value into a failed network transfer.
        let flush_started_at = Instant::now();
        match ole_flush_clipboard_with_retry() {
            Ok(report) => {
                lease.flushed = true;
                lease.flush_attempts = report.attempts();
                if report.attempts() > 1 {
                    let _ = append_product_log(
                        &self.log_path,
                        &format!(
                            "shared_clipboard phase=receive_flush_retried revision={} attempts={} elapsed_ms={}",
                            event.snapshot.revision,
                            report.attempts(),
                            report.elapsed().as_millis(),
                        ),
                    );
                }
            }
            Err(failure) if failure.is_clipboard_busy() => {
                lease.flush_deferred_since = Some(flush_started_at);
                lease.flush_attempts = failure.attempts();
                let _ = append_product_log(
                    &self.log_path,
                    &format!(
                        "shared_clipboard phase=receive_flush_deferred revision={} retry_ms={PRODUCT_TIMER_MS} error={failure}",
                        event.snapshot.revision,
                    ),
                );
            }
            Err(failure) => return Err(io::Error::other(failure.to_string())),
        }
        let installed_sequence = unsafe { GetClipboardSequenceNumber() };
        if installed_sequence != 0 {
            self.last_clipboard_sequence = Some(installed_sequence);
        }
        super::paste_fence::observe_remote(installed_sequence);
        let event_id = event.snapshot.event_id;
        if self.seen_shared_events.len() == MAX_SEEN_SHARED_EVENTS {
            self.seen_shared_events.pop_front();
        }
        self.seen_shared_events.push_back(event_id);
        let summary = ProductSharedTextSummary {
            direction: "接收",
            utf16_units: event.snapshot.unicode_units(),
            bytes: event.snapshot.total_bytes(),
            formats: event.snapshot.format_labels(),
        };
        self.update_snapshot(|state| {
            state.shared_text_generation = state.shared_text_generation.wrapping_add(1);
            state.last_shared_text = Some(summary);
            state.last_error = None;
        });
        let _ = append_product_log(
            &self.log_path,
            &format!(
                "shared_clipboard phase=receive_ready revision={} formats={} bytes={} elapsed_ms={}",
                event.snapshot.revision,
                event.snapshot.format_labels(),
                event.snapshot.total_bytes(),
                event.elapsed_millis,
            ),
        );
        Ok(())
    }

    fn fail_remote_shared(&mut self, event_id: ClipboardEventId) {
        if self
            .remote_text
            .as_ref()
            .is_some_and(|lease| lease.event_id == event_id)
        {
            self.remote_text.take();
            let sequence = unsafe { GetClipboardSequenceNumber() };
            if sequence != 0 {
                self.last_clipboard_sequence = Some(sequence);
            }
        }
    }

    fn retry_deferred_remote_text_flush(&mut self) {
        let Some(lease) = self.remote_text.as_mut() else {
            return;
        };
        let Some(deferred_since) = lease.flush_deferred_since else {
            return;
        };
        if !lease.is_current() {
            lease.flush_deferred_since = None;
            return;
        }
        lease.flush_attempts = lease.flush_attempts.saturating_add(1);
        match unsafe { OleFlushClipboard() } {
            Ok(()) => {
                lease.flushed = true;
                lease.flush_deferred_since = None;
                let _ = append_product_log(
                    &self.log_path,
                    &format!(
                        "shared_clipboard phase=receive_flush_recovered attempts={} elapsed_ms={}",
                        lease.flush_attempts,
                        deferred_since.elapsed().as_millis(),
                    ),
                );
            }
            Err(error) if error.code() == CLIPBRD_E_CANT_OPEN => {}
            Err(error) => {
                lease.flush_deferred_since = None;
                let _ = append_product_log(
                    &self.log_path,
                    &format!(
                        "shared_clipboard phase=receive_flush_abandoned attempts={} elapsed_ms={} error={} ({:#010X})",
                        lease.flush_attempts,
                        deferred_since.elapsed().as_millis(),
                        error,
                        error.code().0.cast_unsigned(),
                    ),
                );
            }
        }
    }

    fn handle_timer(&mut self) {
        self.retry_deferred_remote_text_flush();
        self.refresh_progress();
    }

    fn publish_paths(&mut self, paths: &[std::path::PathBuf]) -> io::Result<()> {
        self.prepare_source_offer_slot()?;
        let tree = FileTreeSnapshot::capture(paths).map_err(|error| match error {
            CaptureError::Rejected(reason) => {
                io::Error::new(io::ErrorKind::Unsupported, reason.user_message())
            }
            CaptureError::Windows(error) => io::Error::other(error),
        })?;
        let offer = self
            .registry
            .publish_tree(&tree, OFFER_TTL)
            .map_err(windows_to_io)?;
        let offered = SecureOfferedFile::from_local_offer(&offer)?;
        let (server, reverse_tunnel) =
            start_clipboard_source_offer(&self.store, &self.settings, offered)?;
        let manifest = server.manifest();
        let summary = summarize_manifest("发送", &manifest);
        let total_size = summary.total_size;
        let total_files = summary.files;
        let endpoint = server.address();
        let generation = self.allocate_transfer_generation();
        self.sources.push(LiveSourceOffer {
            _reverse_tunnel: reverse_tunnel,
            server,
            manifest,
            generation,
        });
        self.active_direction = Some(TransferDirection::Source);
        let _ = append_product_log(
            &self.log_path,
            &format!(
                "offer_published endpoint={} route=reverse_tunnel items={} files={} total_size={}",
                endpoint, summary.items, summary.files, summary.total_size
            ),
        );
        self.update_snapshot(|snapshot| {
            snapshot.last_manifest = Some(summary);
            snapshot.transfer_generation = generation;
            snapshot.transfer = Some(ProductTransferSnapshot {
                state: ProductTransferState::AwaitingPaste,
                transferred: 0,
                total_size,
                bytes_per_second: 0,
                average_bytes_per_second: 0,
                started_files: 0,
                total_files,
                current_file_name: None,
                current_file_transferred: 0,
                current_file_size: 0,
                reconnect_attempts: 0,
                recovered_commands: 0,
                recovery_active: false,
            });
            snapshot.last_error = None;
        });
        self.refresh_progress();
        Ok(())
    }

    fn prepare_source_offer_slot(&mut self) -> io::Result<()> {
        while self.sources.len() >= MAX_LIVE_SOURCE_OFFERS {
            let Some(index) = self.sources.iter().position(|source| {
                let metrics = source.server.metrics();
                let total = summarize_manifest("发送", &source.manifest).total_size;
                metrics.begun_transfers == 0
                    || metrics.source_cancelled
                    || metrics.cancelled_transfers != 0
                    || (total != 0 && metrics.unique_bytes >= total)
            }) else {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "同时活动的发送文件任务过多，请先完成部分任务",
                ));
            };
            self.sources.remove(index);
        }
        Ok(())
    }

    fn handle_remote_boundary_workspace(
        &mut self,
        peer: CertificateFingerprint,
        payload: &[u8],
        completion: &mpsc::SyncSender<io::Result<()>>,
    ) {
        self.next_boundary_sync_sequence = self.next_boundary_sync_sequence.wrapping_add(1).max(1);
        let result = decode_boundary_sync(
            payload,
            peer,
            self.local_fingerprint,
            self.next_boundary_sync_sequence,
        )
        .map(|workspace| {
            self.update_snapshot(|snapshot| {
                snapshot.remote_boundary_workspace = Some(Arc::new(workspace));
            });
        });
        let _ = completion.send(result);
    }

    fn handle_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::RemoteOffer(announcement) => {
                if announcement.peer != self.settings.active_peer {
                    self.set_error("收到非活动设备的 Offer，已拒绝".to_owned());
                    return;
                }
                self.pending = Some(announcement);
                self.update_snapshot(|snapshot| snapshot.pending_offer = true);
                if self.settings.auto_receive
                    && let Err(error) = self.accept_pending()
                {
                    self.set_background_error(remote_offer_failure_message(
                        announcement.endpoint,
                        &error,
                    ));
                }
            }
            WorkerEvent::LocalTextCapture(event) => self.handle_local_text_capture(event),
            WorkerEvent::RemoteSharedPending(event) => {
                let result = self.install_remote_shared(&event);
                if let Err(error) = &result {
                    self.set_background_error(format!("安装待接收共享剪贴板失败：{error}"));
                }
                let _ = event.completion.send(result);
            }
            WorkerEvent::RemoteSharedReady(event) => {
                let result = self.finalize_remote_shared(&event);
                if let Err(error) = &result {
                    self.fail_remote_shared(event.snapshot.event_id);
                    self.set_background_error(format!("完成接收共享剪贴板失败：{error}"));
                }
                let _ = event.completion.send(result);
            }
            WorkerEvent::RemoteSharedFailed { event_id } => {
                self.fail_remote_shared(event_id);
            }
            WorkerEvent::SharedInstalled { event_id } => {
                if let Some((current_event, generation)) = self.last_outbound_shared
                    && current_event == event_id
                {
                    super::paste_fence::mark_installed(generation);
                    let _ = append_product_log(
                        &self.log_path,
                        &format!("shared_text phase=peer_installed generation={generation}"),
                    );
                }
            }
            WorkerEvent::SharedSendResult {
                event_id,
                elapsed_millis,
                result,
            } => {
                if self
                    .active_outbound_shared
                    .is_some_and(|active| active.event_id == event_id)
                {
                    self.active_outbound_shared = None;
                }
                if let Some((current_event, generation)) = self.last_outbound_shared
                    && current_event == event_id
                {
                    match result {
                        Ok(()) => {
                            super::paste_fence::mark_ready(generation);
                            let _ = append_product_log(
                                &self.log_path,
                                &format!(
                                    "shared_clipboard phase=send_ready generation={generation} elapsed_ms={elapsed_millis}"
                                ),
                            );
                            self.update_snapshot(|state| state.last_error = None);
                        }
                        Err(error) => {
                            super::paste_fence::mark_failed(generation);
                            self.set_background_error(format!(
                                "共享剪贴板未送达：{error}。网络恢复后请重新复制；不会回放离线历史。"
                            ));
                        }
                    }
                }
            }
            WorkerEvent::RemoteBoundaryWorkspace {
                peer,
                payload,
                completion,
            } => self.handle_remote_boundary_workspace(peer, &payload, &completion),
        }
    }

    fn handle_command(&mut self, command: ProductCommand) {
        use super::secure_transfer::RemoteTransferState;
        let result = match command {
            ProductCommand::AcceptPending => self.accept_pending(),
            ProductCommand::Pause => self.control_tasks(None, RemoteTransferState::Paused),
            ProductCommand::Resume => self.control_tasks(None, RemoteTransferState::Running),
            ProductCommand::Cancel => self.control_tasks(None, RemoteTransferState::Cancelled),
            ProductCommand::PauseTask(id) => {
                self.control_tasks(Some(id), RemoteTransferState::Paused)
            }
            ProductCommand::ResumeTask(id) => {
                self.control_tasks(Some(id), RemoteTransferState::Running)
            }
            ProductCommand::CancelTask(id) => {
                self.control_tasks(Some(id), RemoteTransferState::Cancelled)
            }
            ProductCommand::CancelFiles(id, ids) => self.cancel_task_files(id, &ids),
        };
        if let Err(error) = result {
            self.set_error(format!("传输控制失败：{error}"));
        }
        self.refresh_progress();
    }

    fn accept_pending(&mut self) -> io::Result<()> {
        let announcement = self
            .pending
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "当前没有待接收清单"))?;
        validate_private_endpoint(announcement.endpoint)?;
        self.prepare_remote_task_slot()?;
        let client = build_offer_client(&self.store, &self.settings, announcement.endpoint)?;
        let generation = self.allocate_transfer_generation();
        let remote = ProductRemoteLease::register(&client, generation).map_err(windows_to_io)?;
        let summary = summarize_manifest("接收", &remote.task.manifest);
        let total_size = summary.total_size;
        let total_files = summary.files;
        self.remote_tasks.push_back(Arc::clone(&remote.task));
        self.remote_text.take();
        self.retire_replaced_remote_file();
        self.remote = Some(remote);
        let installed_sequence = unsafe { GetClipboardSequenceNumber() };
        if installed_sequence != 0 {
            self.last_clipboard_sequence = Some(installed_sequence);
        }
        self.active_direction = Some(TransferDirection::Remote);
        let _ = append_product_log(
            &self.log_path,
            &format!(
                "offer_received endpoint={} items={} files={} total_size={} auto_receive={}",
                announcement.endpoint,
                summary.items,
                summary.files,
                summary.total_size,
                self.settings.auto_receive
            ),
        );
        self.pending = None;
        self.update_snapshot(|snapshot| {
            snapshot.pending_offer = false;
            snapshot.last_manifest = Some(summary);
            snapshot.transfer_generation = generation;
            snapshot.transfer = Some(ProductTransferSnapshot {
                state: ProductTransferState::AwaitingPaste,
                transferred: 0,
                total_size,
                bytes_per_second: 0,
                average_bytes_per_second: 0,
                started_files: 0,
                total_files,
                current_file_name: None,
                current_file_transferred: 0,
                current_file_size: 0,
                reconnect_attempts: 0,
                recovered_commands: 0,
                recovery_active: false,
            });
            snapshot.last_error = None;
        });
        self.refresh_progress();
        Ok(())
    }

    fn prepare_remote_task_slot(&mut self) -> io::Result<()> {
        while self.remote_tasks.len() >= MAX_LIVE_REMOTE_TASKS {
            let Some(index) = self
                .remote_tasks
                .iter()
                .position(|candidate| candidate.is_terminal_or_stale())
            else {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "同时活动的接收文件任务过多，请先完成或取消一个任务",
                ));
            };
            self.remote_tasks.remove(index);
        }
        Ok(())
    }

    fn control_tasks(
        &mut self,
        id: Option<ProductTransferId>,
        state: super::secure_transfer::RemoteTransferState,
    ) -> io::Result<()> {
        if let Some(id) = id {
            let _ = crate::logging::append(
                &self.log_path,
                &format!(
                    "transfer_control task={} direction={:?} requested={state:?}",
                    id.short_label(),
                    id.direction()
                ),
            );
        }
        let mut found = false;
        for task in &self.remote_tasks {
            if id.is_none_or(|id| remote_task_id(task) == id) {
                found = true;
                if !task.is_terminal_or_stale() {
                    task.registry.set_local_control(state);
                }
            }
        }
        for source in &self.sources {
            if id.is_none_or(|id| source_task_id(source) == id) {
                let metrics = source.server.metrics();
                let total = summarize_manifest("发送", &source.manifest).total_size;
                if !metrics.source_cancelled
                    && metrics.cancelled_transfers == 0
                    && (total == 0 || metrics.unique_bytes < total)
                {
                    source.server.control_handle().set(state);
                }
                found = true;
            }
        }
        found |= super::drag_live::control_ui_tasks(id, state);
        if id.is_some() && !found {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "传输任务已结束或不存在",
            ));
        }
        Ok(())
    }

    fn cancel_task_files(&self, id: ProductTransferId, ids: &[[u8; 16]]) -> io::Result<()> {
        require_receiving_task(id)?;
        let task = self
            .remote_tasks
            .iter()
            .find(|task| remote_task_id(task) == id);
        let Some(task) = task else {
            return super::drag_live::cancel_ui_files(id, ids)
                .then_some(())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "传输任务已结束或不存在"));
        };
        let valid =
            ids.iter()
                .filter(|id| {
                    task.manifest.entries.iter().any(|entry| {
                        entry.file_id.bytes() == **id && !entry.descriptor.is_directory()
                    })
                })
                .copied()
                .collect::<Vec<_>>();
        task.registry.cancel_files(&valid);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn refresh_progress(&mut self) {
        if self.sources.is_empty() && self.remote_tasks.is_empty() {
            return;
        }
        let current_remote_id = self
            .remote
            .as_ref()
            .map(|remote| remote_task_id(&remote.task));
        let current_source_id = (self.active_direction == Some(TransferDirection::Source))
            .then(|| self.sources.last().map(source_task_id))
            .flatten();
        let mut tasks = Vec::new();

        for source in &self.sources {
            let id = source_task_id(source);
            let metrics = source.server.metrics();
            if metrics.begun_transfers == 0 && current_source_id != Some(id) {
                continue;
            }
            let manifest =
                summarize_manifest(ProductTransferDirection::Sending.label(), &source.manifest);
            let transferred = metrics.unique_bytes.min(manifest.total_size);
            let state = if metrics.source_cancelled || metrics.cancelled_transfers != 0 {
                ProductTransferState::Cancelled
            } else if manifest.total_size != 0 && transferred >= manifest.total_size {
                ProductTransferState::Completed
            } else if metrics.source_paused {
                ProductTransferState::Paused
            } else if metrics.begun_transfers != 0 {
                ProductTransferState::Running
            } else {
                ProductTransferState::AwaitingPaste
            };
            tasks.push(ProductTransferTaskSnapshot {
                id,
                generation: source.generation,
                files: source.server.file_progress(),
                source_location: source.manifest.source_location.to_string(),
                target_location: String::new(),
                started_unix_ms: 0,
                completed_unix_ms: 0,
                transfer: ProductTransferSnapshot {
                    state,
                    transferred,
                    total_size: manifest.total_size,
                    bytes_per_second: 0,
                    average_bytes_per_second: 0,
                    started_files: usize::try_from(metrics.begun_transfers).unwrap_or(usize::MAX),
                    total_files: manifest.files,
                    current_file_name: None,
                    current_file_transferred: 0,
                    current_file_size: 0,
                    reconnect_attempts: 0,
                    recovered_commands: 0,
                    recovery_active: false,
                },
                manifest,
            });
        }

        for remote in &self.remote_tasks {
            let id = remote_task_id(remote);
            let progress = remote.registry.local_progress();
            if progress.started_transfers == 0
                && current_remote_id != Some(id)
                && !progress.cancelled
            {
                continue;
            }
            let manifest = summarize_manifest(
                ProductTransferDirection::Receiving.label(),
                &remote.manifest,
            );
            let files = remote.registry.file_progress(&remote.manifest);
            let ordinary = files
                .iter()
                .filter(|file| !file.is_directory)
                .collect::<Vec<_>>();
            let all_terminal = !ordinary.is_empty()
                && ordinary.iter().all(|file| {
                    matches!(
                        file.state,
                        ProductTransferState::Completed
                            | ProductTransferState::Cancelled
                            | ProductTransferState::Failed
                    )
                });
            let all_complete = all_terminal
                && ordinary
                    .iter()
                    .all(|file| file.state == ProductTransferState::Completed);
            let state = if progress.cancelled {
                ProductTransferState::Cancelled
            } else if all_complete {
                ProductTransferState::Completed
            } else if all_terminal {
                if ordinary
                    .iter()
                    .any(|file| file.state == ProductTransferState::Failed)
                {
                    ProductTransferState::Failed
                } else {
                    ProductTransferState::Cancelled
                }
            } else if progress.paused {
                ProductTransferState::Paused
            } else if progress.started_transfers != 0 {
                ProductTransferState::Running
            } else {
                ProductTransferState::AwaitingPaste
            };
            let (current_file_name, current_file_transferred, current_file_size) = remote
                .registry
                .current_file_progress()
                .map_or((None, 0, 0), |(name, transferred, size)| {
                    (Some(name), transferred, size)
                });
            let recovery = remote.registry.recovery_snapshot();
            tasks.push(ProductTransferTaskSnapshot {
                id,
                generation: remote.generation,
                source_location: remote.manifest.source_location.to_string(),
                target_location: String::new(),
                started_unix_ms: 0,
                completed_unix_ms: 0,
                transfer: ProductTransferSnapshot {
                    state,
                    transferred: ordinary.iter().map(|file| file.transferred).sum(),
                    total_size: manifest.total_size,
                    bytes_per_second: 0,
                    average_bytes_per_second: 0,
                    started_files: progress.started_transfers,
                    total_files: manifest.files,
                    current_file_name,
                    current_file_transferred,
                    current_file_size,
                    reconnect_attempts: recovery.reconnect_attempts,
                    recovered_commands: recovery.recovered_commands,
                    recovery_active: recovery.active_recoveries != 0,
                },
                manifest,
                files,
            });
        }

        tasks.sort_by_key(|task| task.generation);
        let selected = tasks
            .iter()
            .rev()
            .find(|task| {
                task.id.direction() == ProductTransferDirection::Receiving
                    && matches!(
                        task.transfer.state,
                        ProductTransferState::Running | ProductTransferState::Paused
                    )
            })
            .or_else(|| {
                tasks.iter().rev().find(|task| {
                    matches!(
                        task.transfer.state,
                        ProductTransferState::Running | ProductTransferState::Paused
                    )
                })
            })
            .or_else(|| tasks.last())
            .cloned();
        self.update_snapshot(|snapshot| {
            snapshot.transfer_tasks = tasks;
            if let Some(selected) = selected {
                snapshot.transfer_generation = selected.generation;
                snapshot.last_manifest = Some(selected.manifest);
                snapshot.transfer = Some(selected.transfer);
            }
        });
    }

    fn set_error(&self, error: String) {
        self.record_error(error);
        let _ = crate::tray::show_runtime_error_existing();
    }

    fn set_background_error(&self, error: String) {
        self.record_error(error);
    }

    fn record_error(&self, error: String) {
        let _ = append_product_log(&self.log_path, &format!("product_error error={error}"));
        self.update_snapshot(|snapshot| snapshot.last_error = Some(error));
    }

    fn update_snapshot(&self, update: impl FnOnce(&mut ProductSnapshot)) {
        match self.snapshot.lock() {
            Ok(mut snapshot) => update(&mut snapshot),
            Err(mut poisoned) => update(poisoned.get_mut()),
        }
    }
}

fn claim_clipboard_sequence(last_sequence: &mut Option<u32>, observed_sequence: u32) -> bool {
    if observed_sequence == 0 {
        return true;
    }
    if *last_sequence == Some(observed_sequence) {
        return false;
    }
    *last_sequence = Some(observed_sequence);
    true
}

const fn shared_capture_policy(settings: SharedClipboardSettings) -> SharedCapturePolicy {
    SharedCapturePolicy {
        text: settings.enabled,
        rich_text: settings.enabled,
        image: settings.enabled,
    }
}

#[allow(clippy::too_many_lines)]
fn run_worker(
    store: DeviceStore,
    settings: AppSettings,
    shared_settings: SharedClipboardSettings,
    snapshot: &Arc<Mutex<ProductSnapshot>>,
    command_receiver: &mpsc::Receiver<WorkerCommand>,
    ready_sender: &mpsc::SyncSender<io::Result<u32>>,
) -> io::Result<()> {
    let _apartment = OleApartment::initialize().map_err(windows_to_io)?;
    let window = ProductWindow::create().map_err(windows_to_io)?;
    let formats = CaptureFormats::register().map_err(windows_to_io)?;
    let shared_policy = shared_capture_policy(shared_settings);
    let shared_formats = shared_policy
        .any()
        .then(SharedCaptureFormats::register)
        .transpose()
        .map_err(windows_to_io)?;
    // SAFETY: this call reads the current dedicated worker thread identifier.
    let thread_id = unsafe { GetCurrentThreadId() };
    let (event_sender, event_receiver) = mpsc::channel();
    let log_path = store.root().join("logs").join("clipferry.log");
    super::shared_diagnostics::initialize(log_path.clone());
    let local_fingerprint = store.load_identity()?.fingerprint();
    let file_role = FileChannelRole::resolve(&store, &settings)?;
    let downlink = Arc::new(OfferDownlink::default());
    let reverse_proxies = Arc::new(Mutex::new(VecDeque::new()));
    let local_generation = Arc::new(AtomicU64::new(0));
    let inbox_services = OfferInboxServices {
        event_sender: event_sender.clone(),
        worker_thread_id: thread_id,
        downlink: Arc::clone(&downlink),
        reverse_proxies: Arc::clone(&reverse_proxies),
        shared_text_enabled: shared_settings.any_enabled(),
        shared_policy,
        local_generation: Arc::clone(&local_generation),
    };
    let mut inbox = OfferInbox::start(store.clone(), &settings, inbox_services.clone())?;
    let mut outbound_link = (file_role == FileChannelRole::Connector)
        .then(|| OutboundOfferLink::start(&store, &settings, inbox_services))
        .transpose()?;
    let text_capture = shared_formats
        .map(|_| TextCaptureWorker::start(event_sender.clone(), thread_id, shared_policy))
        .transpose()?;
    let shared_outbound = shared_settings
        .any_enabled()
        .then(|| {
            SharedOutbound::start(
                &store,
                &settings,
                file_role,
                Arc::clone(&downlink),
                event_sender.clone(),
                thread_id,
            )
        })
        .transpose()?;
    let initial_clipboard_sequence = unsafe { GetClipboardSequenceNumber() };
    super::paste_fence::configure(shared_settings.any_enabled(), initial_clipboard_sequence);
    let _paste_fence_configuration = PasteFenceConfiguration;
    if let Ok(mut value) = snapshot.lock() {
        value.listener_running = true;
    }
    let _ = append_product_log(
        &log_path,
        &format!(
            "product_start listener={} peer={} auto_receive={} file_role={} shared_text={}",
            settings.local_endpoint,
            settings.active_peer,
            settings.auto_receive,
            file_role.label(),
            shared_settings.any_enabled(),
        ),
    );
    ready_sender
        .send(Ok(thread_id))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "tray abandoned worker startup"))?;
    // SAFETY: a thread timer posts WM_TIMER into this worker's queue.
    let timer = unsafe { SetTimer(None, PRODUCT_TIMER_ID, PRODUCT_TIMER_MS, None) };
    if timer == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut session = ProductSession {
        store,
        settings,
        formats,
        shared_formats,
        shared_settings,
        text_capture,
        shared_outbound,
        snapshot: Arc::clone(snapshot),
        registry: LocalOfferRegistry::default(),
        sources: Vec::new(),
        pending: None,
        remote: None,
        remote_tasks: VecDeque::new(),
        remote_text: None,
        active_direction: None,
        last_clipboard_sequence: Some(initial_clipboard_sequence).filter(|value| *value != 0),
        local_generation,
        shared_revision: 0,
        local_fingerprint,
        active_outbound_shared: None,
        seen_shared_events: VecDeque::new(),
        last_outbound_shared: None,
        log_path: log_path.clone(),
        next_transfer_generation: 0,
        next_boundary_sync_sequence: 0,
    };
    let mut stopping = false;
    while !stopping {
        let mut message = MSG::default();
        // SAFETY: the worker owns its message queue and the writable MSG structure.
        let status = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
        if status == -1 {
            return Err(io::Error::last_os_error());
        }
        if status == 0 || message.message == WM_QUIT {
            break;
        }
        match message.message {
            WM_CLIPBOARDUPDATE if message.hwnd == window.handle => {
                session.handle_clipboard_update(window.handle);
            }
            PRODUCT_EVENT_MESSAGE => {
                for event in event_receiver.try_iter() {
                    session.handle_event(event);
                }
            }
            PRODUCT_COMMAND_MESSAGE => {
                for command in command_receiver.try_iter() {
                    match command {
                        WorkerCommand::Control(command) => session.handle_command(command),
                        WorkerCommand::Stop => {
                            stopping = true;
                            break;
                        }
                    }
                }
            }
            WM_TIMER if message.wParam.0 == timer => session.handle_timer(),
            _ => unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            },
        }
    }
    for remote in &session.remote_tasks {
        let _ = remote.registry.cancel_all();
    }
    if let Some(capture) = session.text_capture.as_mut() {
        capture.stop();
    }
    if let Some(outbound) = session.shared_outbound.as_mut() {
        outbound.stop();
    }
    session.remote_text.take();
    session.remote.take();
    session.sources.clear();
    if let Some(link) = outbound_link.as_mut() {
        link.stop();
    }
    inbox.stop();
    // SAFETY: balances the live timer on this worker thread.
    let _ = unsafe { KillTimer(None, timer) };
    if let Ok(mut value) = snapshot.lock() {
        value.listener_running = false;
    }
    let _ = append_product_log(&log_path, "product_stop clean=true");
    Ok(())
}

#[derive(Default)]
struct OfferDownlink {
    connection: Mutex<Option<AuthenticatedServerConnection>>,
    ready: Condvar,
}

impl OfferDownlink {
    fn is_available(&self) -> bool {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn install(&self, connection: AuthenticatedServerConnection) {
        let mut slot = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = slot.replace(connection) {
            let _ = previous.stream.sock.shutdown(Shutdown::Both);
        }
        self.ready.notify_all();
    }

    /// Test-only: production sources no longer announce direct LAN endpoints; they publish
    /// through the reverse tunnel. The receiver still accepts this frame from older peers.
    #[cfg(test)]
    fn send_recovering(&self, endpoint: SocketAddr, window: Duration) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(window)
            .unwrap_or_else(Instant::now);
        let frame = encode_announcement(endpoint)?;
        loop {
            let mut slot = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while slot.is_none() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "outbound file channel from the paired device was not available",
                    ));
                }
                let waited = self
                    .ready
                    .wait_timeout(slot, remaining.min(Duration::from_secs(1)))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slot = waited.0;
            }
            let connection = slot.as_mut().expect("downlink was checked above");
            let result = connection
                .stream
                .write_all(&frame)
                .and_then(|()| connection.stream.flush())
                .and_then(|()| {
                    let mut acknowledgement = [0_u8; OFFER_ACK.len()];
                    connection.stream.read_exact(&mut acknowledgement)?;
                    (acknowledgement == *OFFER_ACK)
                        .then_some(())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "peer returned an invalid downlink offer acknowledgement",
                            )
                        })
                });
            if result.is_ok() {
                return Ok(());
            }
            if let Some(failed) = slot.take() {
                let _ = failed.stream.sock.shutdown(Shutdown::Both);
            }
            drop(slot);
            if Instant::now() >= deadline {
                return result;
            }
        }
    }

    fn send_shared(
        &self,
        job: &SharedOutboundJob,
        event_sender: &mpsc::Sender<WorkerEvent>,
        worker_thread_id: u32,
    ) -> io::Result<()> {
        let channel_wait = job.trace.stage("downlink_acquire");
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);
        let mut slot = self
            .connection
            .lock()
            .map_err(|_| io::Error::other("shared clipboard downlink was poisoned"))?;
        while slot.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "reverse shared clipboard channel was not available",
                ));
            }
            let waited = self
                .ready
                .wait_timeout(slot, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot = waited.0;
        }
        let connection = slot.as_mut().expect("shared downlink was checked above");
        channel_wait.finish("ok");
        connection
            .stream
            .sock
            .set_read_timeout(Some(SHARED_APPLY_TIMEOUT))?;
        connection
            .stream
            .sock
            .set_write_timeout(Some(SHARED_APPLY_TIMEOUT))?;
        let result =
            exchange_shared_job(&mut connection.stream, job, event_sender, worker_thread_id);
        if result.is_err()
            && let Some(failed) = slot.take()
        {
            let _ = failed.stream.sock.shutdown(Shutdown::Both);
        }
        result
    }
}

struct OutboundOfferLink {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl OutboundOfferLink {
    fn start(
        store: &DeviceStore,
        settings: &AppSettings,
        services: OfferInboxServices,
    ) -> io::Result<Self> {
        let tls = build_pinned_tls(store, settings)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let peer = settings.active_peer;
        let peer_endpoint = settings.peer_endpoint;
        let worker = std::thread::Builder::new()
            .name("pierdesk-file-downlink".to_owned())
            .spawn(move || {
                let mut delay = Duration::from_millis(100);
                while !thread_stop.load(Ordering::Acquire) {
                    let result =
                        run_outbound_offer_link(&tls, peer_endpoint, peer, &services, &thread_stop);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if result.is_ok() {
                        delay = Duration::from_millis(100);
                    }
                    sleep_until_stopped(&thread_stop, delay);
                    delay = delay.saturating_mul(2).min(Duration::from_secs(2));
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for OutboundOfferLink {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_outbound_offer_link(
    tls: &PinnedTlsClient,
    peer_endpoint: SocketAddr,
    peer: CertificateFingerprint,
    services: &OfferInboxServices,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut stream = tls.connect_with_timeout(peer_endpoint, Duration::from_secs(2))?;
    stream.write_all(DOWNLINK_MAGIC)?;
    stream.flush()?;
    let mut acknowledgement = [0_u8; DOWNLINK_ACK.len()];
    stream.read_exact(&mut acknowledgement)?;
    if acknowledgement != *DOWNLINK_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer returned an invalid file downlink acknowledgement",
        ));
    }
    stream
        .sock
        .set_read_timeout(Some(Duration::from_millis(500)))?;
    let refresh_at = Instant::now()
        .checked_add(IDLE_FILE_CHANNEL_REFRESH)
        .unwrap_or_else(Instant::now);
    while !stop.load(Ordering::Acquire) {
        let mut magic = [0_u8; OFFER_MAGIC.len()];
        if !read_exact_polling_until(&mut stream, &mut magic, stop, Some(refresh_at))? {
            break;
        }
        if magic == *SHARED_MAGIC {
            stream.sock.set_read_timeout(Some(SHARED_FINISH_TIMEOUT))?;
            handle_shared_stream(&mut stream, peer, services)?;
            stream
                .sock
                .set_read_timeout(Some(Duration::from_millis(500)))?;
            continue;
        }
        if magic != *OFFER_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid message received on reverse clipboard channel",
            ));
        }
        let endpoint = read_announcement_after_magic(&mut stream)?;
        services
            .event_sender
            .send(WorkerEvent::RemoteOffer(OfferAnnouncement {
                endpoint,
                peer,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard worker stopped"))?;
        stream.write_all(OFFER_ACK)?;
        stream.flush()?;
        let _ = post_worker_message(services.worker_thread_id, PRODUCT_EVENT_MESSAGE);
    }
    let _ = stream.sock.shutdown(Shutdown::Both);
    Ok(())
}

fn read_exact_polling_until(
    stream: &mut impl io::Read,
    buffer: &mut [u8],
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> io::Result<bool> {
    let mut offset = 0;
    while offset < buffer.len() && !stop.load(Ordering::Acquire) {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            if offset == 0 {
                return Ok(false);
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "file channel refresh interrupted a partial frame",
            ));
        }
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file downlink closed",
                ));
            }
            Ok(read) => offset += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(offset == buffer.len())
}

struct ReverseProxyEntry {
    offer_id: [u8; 16],
    proxy: Arc<ReverseProxyServer>,
}

struct ReverseProxyServer {
    address: SocketAddr,
    outer_sender: mpsc::SyncSender<AuthenticatedServerConnection>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ReverseProxyServer {
    fn start() -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let (outer_sender, outer_receiver) = mpsc::sync_channel(REVERSE_TUNNEL_LANES * 2);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("pierdesk-file-reverse-proxy".to_owned())
            .spawn(move || reverse_proxy_loop(&listener, &outer_receiver, &thread_stop))?;
        Ok(Self {
            address,
            outer_sender,
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }

    fn add_outer(&self, connection: AuthenticatedServerConnection) -> io::Result<()> {
        self.outer_sender
            .try_send(connection)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "reverse file proxy lane queue is full",
                ),
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "reverse file proxy stopped")
                }
            })
    }
}

impl Drop for ReverseProxyServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn reverse_proxy_loop(
    listener: &TcpListener,
    outer_receiver: &mpsc::Receiver<AuthenticatedServerConnection>,
    stop: &Arc<AtomicBool>,
) {
    let mut inner_connections: VecDeque<TcpStream> = VecDeque::new();
    let mut outer_connections: VecDeque<AuthenticatedServerConnection> = VecDeque::new();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((connection, _)) => {
                let _ = connection.set_nodelay(true);
                if inner_connections.len() < REVERSE_TUNNEL_LANES * 2 {
                    inner_connections.push_back(connection);
                } else {
                    let _ = connection.shutdown(Shutdown::Both);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        loop {
            match outer_receiver.try_recv() {
                Ok(connection) => {
                    if outer_connections.len() == REVERSE_TUNNEL_LANES
                        && let Some(stale) = outer_connections.pop_front()
                    {
                        let _ = stale.stream.sock.shutdown(Shutdown::Both);
                    }
                    outer_connections.push_back(connection);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        while !inner_connections.is_empty() && !outer_connections.is_empty() {
            let inner = inner_connections
                .pop_front()
                .expect("reverse proxy checked the inner queue");
            let outer = outer_connections
                .pop_front()
                .expect("reverse proxy checked the outer queue");
            let relay_stop = Arc::clone(stop);
            let _ = std::thread::Builder::new()
                .name("pierdesk-file-reverse-relay".to_owned())
                .spawn(move || relay_receiver_side(inner, outer, &relay_stop));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Publish a clipboard file source through the receiving peer's authenticated fixed-port inbox.
/// Both file-channel roles use this route, so the receiver never needs to reach a random
/// source port on the LAN. The offer server listens on loopback only.
fn start_clipboard_source_offer(
    store: &DeviceStore,
    settings: &AppSettings,
    offered: SecureOfferedFile,
) -> io::Result<(SecureOfferServer, ReverseOfferTunnel)> {
    let identity = store.load_identity()?;
    let server = SecureOfferServer::start_trusted(
        SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        TrustedTlsServer::new(identity, store.clone(), TLS_TIMEOUT)?,
        settings.active_peer,
        offered,
        TRANSFER_TTL,
        TLS_TIMEOUT,
    )?;
    let tunnel = ReverseOfferTunnel::start(
        &build_pinned_tls(store, settings)?,
        settings.peer_endpoint,
        server.address(),
        server.manifest().offer_id,
        OFFER_TTL,
    )?;
    Ok((server, tunnel))
}

/// Publish a drag source through the receiving peer's authenticated fixed-port inbox.
/// Both file-channel roles use this route: no random source port is exposed on the LAN.
pub(crate) fn start_drag_source_offer(
    store: &DeviceStore,
    peer: CertificateFingerprint,
    peer_endpoint: SocketAddr,
    offered: SecureOfferedFile,
) -> io::Result<(SecureOfferServer, ReverseOfferTunnel, u16)> {
    let identity = store.load_identity()?;
    let trusted = store.load_peer(peer)?;
    let client = PinnedTlsClient::new(
        &identity,
        trusted.certificate_der().to_vec(),
        peer,
        Duration::from_secs(3),
    )?;
    let server = SecureOfferServer::start_trusted(
        SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        TrustedTlsServer::new(identity, store.clone(), Duration::from_secs(3))?,
        peer,
        offered,
        super::secure_transfer::TRANSFER_IDLE_TIMEOUT,
        Duration::from_secs(3),
    )?;
    let (tunnel, port) = ReverseOfferTunnel::start_drag(
        &client,
        peer_endpoint,
        server.address(),
        server.manifest().offer_id,
        OFFER_TTL,
    )?;
    Ok((server, tunnel, port))
}

pub(crate) struct ReverseOfferTunnel {
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl ReverseOfferTunnel {
    fn start(
        tls: &PinnedTlsClient,
        peer_endpoint: SocketAddr,
        source_endpoint: SocketAddr,
        offer_id: super::secure_transfer::ProtocolId,
        lifetime: Duration,
    ) -> io::Result<Self> {
        Self::start_inner(
            tls,
            peer_endpoint,
            source_endpoint,
            offer_id,
            lifetime,
            false,
        )
        .map(|(tunnel, _)| tunnel)
    }

    pub(crate) fn start_drag(
        tls: &PinnedTlsClient,
        peer_endpoint: SocketAddr,
        source_endpoint: SocketAddr,
        offer_id: super::secure_transfer::ProtocolId,
        lifetime: Duration,
    ) -> io::Result<(Self, u16)> {
        let (tunnel, port) = Self::start_inner(
            tls,
            peer_endpoint,
            source_endpoint,
            offer_id,
            lifetime,
            true,
        )?;
        port.map_or_else(
            || {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "drag reverse tunnel did not become ready",
                ))
            },
            |port| Ok((tunnel, port)),
        )
    }

    fn start_inner(
        tls: &PinnedTlsClient,
        peer_endpoint: SocketAddr,
        source_endpoint: SocketAddr,
        offer_id: super::secure_transfer::ProtocolId,
        lifetime: Duration,
        drag: bool,
    ) -> io::Result<(Self, Option<u16>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now()
            .checked_add(lifetime)
            .unwrap_or_else(Instant::now);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let mut workers = Vec::with_capacity(REVERSE_TUNNEL_LANES);
        for lane in 0..REVERSE_TUNNEL_LANES {
            let thread_tls = tls.clone();
            let thread_stop = Arc::clone(&stop);
            let offer_id = offer_id.bytes();
            let ready_sender = ready_sender.clone();
            workers.push(
                std::thread::Builder::new()
                    .name(format!("pierdesk-file-reverse-{lane}"))
                    .spawn(move || {
                        while !thread_stop.load(Ordering::Acquire)
                            && (drag || Instant::now() < deadline)
                        {
                            let result = run_reverse_offer_lane(
                                &thread_tls,
                                peer_endpoint,
                                source_endpoint,
                                offer_id,
                                &thread_stop,
                                drag,
                                &ready_sender,
                            );
                            if thread_stop.load(Ordering::Acquire) {
                                break;
                            }
                            sleep_until_stopped(
                                &thread_stop,
                                if result.is_ok() {
                                    Duration::from_millis(50)
                                } else {
                                    Duration::from_millis(250)
                                },
                            );
                        }
                    })?,
            );
        }
        drop(ready_sender);
        let tunnel = Self { stop, workers };
        let port = if drag {
            Some(
                ready_receiver
                    .recv_timeout(Duration::from_secs(4))
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "drag reverse tunnel handshake timed out",
                        )
                    })?,
            )
        } else {
            None
        };
        Ok((tunnel, port))
    }
}

impl Drop for ReverseOfferTunnel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn run_reverse_offer_lane(
    tls: &PinnedTlsClient,
    peer_endpoint: SocketAddr,
    source_endpoint: SocketAddr,
    offer_id: [u8; 16],
    stop: &AtomicBool,
    drag: bool,
    ready: &mpsc::SyncSender<u16>,
) -> io::Result<()> {
    let mut outer = tls.connect_with_timeout(peer_endpoint, Duration::from_secs(2))?;
    outer.write_all(if drag {
        DRAG_REVERSE_TUNNEL_MAGIC
    } else {
        REVERSE_TUNNEL_MAGIC
    })?;
    outer.write_all(&offer_id)?;
    outer.flush()?;
    let mut acknowledgement = [0_u8; REVERSE_TUNNEL_ACK.len()];
    outer.read_exact(&mut acknowledgement)?;
    let expected = if drag {
        *DRAG_REVERSE_TUNNEL_ACK
    } else {
        *REVERSE_TUNNEL_ACK
    };
    if acknowledgement != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer returned an invalid reverse tunnel acknowledgement",
        ));
    }
    if drag {
        let mut encoded_port = [0_u8; 2];
        outer.read_exact(&mut encoded_port)?;
        let port = u16::from_be_bytes(encoded_port);
        if port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid drag reverse proxy port",
            ));
        }
        let _ = ready.try_send(port);
    }
    outer
        .sock
        .set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut start = [0_u8; 1];
    let refresh_at = Instant::now()
        .checked_add(IDLE_FILE_CHANNEL_REFRESH)
        .unwrap_or_else(Instant::now);
    if !read_exact_polling_until(&mut outer, &mut start, stop, Some(refresh_at))? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "idle reverse file tunnel is being refreshed",
        ));
    }
    if start[0] != REVERSE_TUNNEL_START {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer returned an invalid reverse tunnel start signal",
        ));
    }
    let inner = TcpStream::connect_timeout(&source_endpoint, Duration::from_secs(2))?;
    inner.set_nodelay(true)?;
    relay_sender_side(inner, outer, stop);
    Ok(())
}

fn relay_sender_side(
    mut inner: TcpStream,
    mut outer: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    stop: &AtomicBool,
) {
    let _ = inner.set_nonblocking(true);
    let _ = outer.sock.set_nonblocking(true);
    outer.conn.set_buffer_limit(Some(64 * 1024));
    let _ = super::file_relay::relay(&mut outer, &mut inner, stop);
    let _ = inner.shutdown(Shutdown::Both);
    let _ = outer.sock.shutdown(Shutdown::Both);
}

fn relay_receiver_side(
    mut inner: TcpStream,
    mut outer: AuthenticatedServerConnection,
    stop: &AtomicBool,
) {
    if outer
        .stream
        .write_all(&[REVERSE_TUNNEL_START])
        .and_then(|()| outer.stream.flush())
        .is_err()
    {
        let _ = inner.shutdown(Shutdown::Both);
        let _ = outer.stream.sock.shutdown(Shutdown::Both);
        return;
    }
    let _ = inner.set_nonblocking(true);
    let _ = outer.stream.sock.set_nonblocking(true);
    outer.stream.conn.set_buffer_limit(Some(64 * 1024));
    let _ = super::file_relay::relay(&mut outer.stream, &mut inner, stop);
    let _ = inner.shutdown(Shutdown::Both);
    let _ = outer.stream.sock.shutdown(Shutdown::Both);
}

fn sleep_until_stopped(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now);
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone)]
struct OfferInboxServices {
    event_sender: mpsc::Sender<WorkerEvent>,
    worker_thread_id: u32,
    downlink: Arc<OfferDownlink>,
    reverse_proxies: Arc<Mutex<VecDeque<ReverseProxyEntry>>>,
    shared_text_enabled: bool,
    shared_policy: SharedCapturePolicy,
    local_generation: Arc<AtomicU64>,
}

fn handle_boundary_workspace_connection(
    connection: &mut AuthenticatedServerConnection,
    services: &OfferInboxServices,
) -> io::Result<()> {
    let mut length = [0_u8; 4];
    connection.stream.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_be_bytes(length)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid boundary payload length",
        )
    })?;
    if length == 0 || length > BOUNDARY_WORKSPACE_PAYLOAD_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "boundary workspace payload is outside its fixed limit",
        ));
    }
    let mut payload = vec![0_u8; length];
    connection.stream.read_exact(&mut payload)?;
    let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
    services
        .event_sender
        .send(WorkerEvent::RemoteBoundaryWorkspace {
            peer: connection.peer_fingerprint,
            payload,
            completion: completion_sender,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard worker stopped"))?;
    post_worker_message(services.worker_thread_id, PRODUCT_EVENT_MESSAGE)?;
    completion_receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "boundary workspace validation timed out",
            )
        })??;
    connection.stream.write_all(BOUNDARY_WORKSPACE_ACK)?;
    connection.stream.flush()
}

fn handle_offer_inbox_connection(
    mut connection: AuthenticatedServerConnection,
    services: &OfferInboxServices,
    active_peer: CertificateFingerprint,
) -> io::Result<()> {
    let mut magic = [0_u8; 8];
    connection.stream.read_exact(&mut magic)?;
    if magic == *crate::screen_info_sync::SCREEN_INFO_MAGIC {
        return crate::screen_info_sync::write_local_screen_info_response(&mut connection.stream);
    }
    if magic == *BOUNDARY_WORKSPACE_MAGIC {
        return handle_boundary_workspace_connection(&mut connection, services);
    }
    if magic == *SHARED_MAGIC {
        // Mutual TLS already identifies a paired device. Shared clipboard conflicts are ordered
        // by the authenticated sender fingerprint, so every trusted workspace peer may deliver
        // its current value even when the legacy file-transfer runtime selected another peer.
        return handle_shared_inbox_connection(&mut connection, services);
    }
    if connection.peer_fingerprint != active_peer {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "inactive peer",
        ));
    }
    if magic == *OFFER_MAGIC {
        let endpoint = read_announcement_after_magic(&mut connection.stream)?;
        services
            .event_sender
            .send(WorkerEvent::RemoteOffer(OfferAnnouncement {
                endpoint,
                peer: connection.peer_fingerprint,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard worker stopped"))?;
        connection.stream.write_all(OFFER_ACK)?;
        connection.stream.flush()?;
        return Ok(());
    }
    if magic == *DOWNLINK_MAGIC {
        connection.stream.write_all(DOWNLINK_ACK)?;
        connection.stream.flush()?;
        connection
            .stream
            .sock
            .set_read_timeout(Some(Duration::from_secs(2)))?;
        services.downlink.install(connection);
        return Ok(());
    }
    if magic == *REVERSE_TUNNEL_MAGIC || magic == *DRAG_REVERSE_TUNNEL_MAGIC {
        let drag = magic == *DRAG_REVERSE_TUNNEL_MAGIC;
        let mut offer_id = [0_u8; 16];
        connection.stream.read_exact(&mut offer_id)?;
        let peer = connection.peer_fingerprint;
        let (proxy, created) = {
            let mut entries = services
                .reverse_proxies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = entries.iter().find(|entry| entry.offer_id == offer_id) {
                (Arc::clone(&existing.proxy), false)
            } else {
                let proxy = Arc::new(ReverseProxyServer::start()?);
                if entries.len() == MAX_LIVE_SOURCE_OFFERS {
                    entries.pop_front();
                }
                entries.push_back(ReverseProxyEntry {
                    offer_id,
                    proxy: Arc::clone(&proxy),
                });
                (proxy, true)
            }
        };
        connection.stream.write_all(if drag {
            DRAG_REVERSE_TUNNEL_ACK
        } else {
            REVERSE_TUNNEL_ACK
        })?;
        if drag {
            connection
                .stream
                .write_all(&proxy.address.port().to_be_bytes())?;
        }
        connection.stream.flush()?;
        connection.stream.sock.set_read_timeout(None)?;
        proxy.add_outer(connection)?;
        if created && !drag {
            services
                .event_sender
                .send(WorkerEvent::RemoteOffer(OfferAnnouncement {
                    endpoint: proxy.address,
                    peer,
                }))
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "clipboard worker stopped")
                })?;
            post_worker_message(services.worker_thread_id, PRODUCT_EVENT_MESSAGE)?;
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "unknown file inbox connection type",
    ))
}

fn handle_shared_inbox_connection(
    connection: &mut AuthenticatedServerConnection,
    services: &OfferInboxServices,
) -> io::Result<()> {
    connection
        .stream
        .sock
        .set_read_timeout(Some(SHARED_FINISH_TIMEOUT))?;
    let result = handle_shared_stream(
        &mut connection.stream,
        connection.peer_fingerprint,
        services,
    );
    if result.is_ok() {
        // The final acknowledgement must be followed by an authenticated TLS shutdown. Dropping
        // the socket immediately after `flush` can race the peer's final acknowledgement read and
        // rustls then reports `UnexpectedEof`, even though the clipboard was already installed.
        connection.stream.conn.send_close_notify();
        connection.stream.flush()?;
    }
    result
}

#[allow(clippy::too_many_lines)] // Keep the two-phase protocol and its timing boundaries together.
fn handle_shared_stream(
    stream: &mut (impl io::Read + io::Write),
    peer: CertificateFingerprint,
    services: &OfferInboxServices,
) -> io::Result<()> {
    let started_at = Instant::now();
    if !services.shared_text_enabled {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "shared text is disabled on this device",
        ));
    }
    let begin = read_begin_after_magic(&mut *stream)?;
    let trace = super::shared_diagnostics::SharedTrace::new(&begin, "receive");
    let receive = trace.stage("receive_total");
    if !shared_begin_allowed(&begin, services.shared_policy) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "one or more shared clipboard categories are disabled on this device",
        ));
    }
    let observed_local_generation = services.local_generation.load(Ordering::Acquire);
    let observed_sequence = unsafe { GetClipboardSequenceNumber() };
    let (completion, installed) = mpsc::sync_channel(1);
    let install = trace.stage("install_dispatch_wait");
    services
        .event_sender
        .send(WorkerEvent::RemoteSharedPending(RemoteSharedPendingEvent {
            begin: begin.clone(),
            peer,
            observed_local_generation,
            observed_sequence,
            completion,
        }))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard worker stopped"))?;
    let _ = post_worker_message(services.worker_thread_id, PRODUCT_EVENT_MESSAGE);
    let pending = match installed.recv_timeout(SHARED_APPLY_TIMEOUT) {
        Ok(Ok(pending)) => pending,
        Ok(Err(error)) => {
            write_shared_rejection(&mut *stream)?;
            return Err(error);
        }
        Err(_) => {
            write_shared_rejection(&mut *stream)?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "shared clipboard pending-object installation timed out",
            ));
        }
    };
    install.finish("ok");
    trace.io("installed_ack_write", || {
        stream.write_all(SHARED_INSTALLED_ACK)?;
        stream.flush()
    })?;
    let finish = match trace.io("finish_read_validate", || read_finish(&mut *stream, &begin)) {
        Ok(finish) => finish,
        Err(error) => {
            pending.fail();
            notify_remote_shared_failed(services, begin.event_id);
            return Err(error);
        }
    };
    match finish {
        SharedTextFinish::Failed => {
            receive.finish("sender_failed");
            pending.fail();
            notify_remote_shared_failed(services, begin.event_id);
            stream.write_all(SHARED_FAILED_ACK)?;
            stream.flush()
        }
        SharedTextFinish::Ready(snapshot) => {
            if let Err(error) = trace.io("pending_complete", || {
                pending.complete(Arc::clone(&snapshot.formats))
            }) {
                notify_remote_shared_failed(services, begin.event_id);
                return Err(error);
            }
            let (completion, applied) = mpsc::sync_channel(1);
            let apply = trace.stage("ready_dispatch_wait");
            services
                .event_sender
                .send(WorkerEvent::RemoteSharedReady(RemoteSharedReadyEvent {
                    snapshot,
                    elapsed_millis: started_at.elapsed().as_millis(),
                    completion,
                }))
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "clipboard worker stopped")
                })?;
            let _ = post_worker_message(services.worker_thread_id, PRODUCT_EVENT_MESSAGE);
            let outcome = match applied.recv_timeout(SHARED_APPLY_TIMEOUT) {
                Ok(Ok(())) => {
                    apply.finish("ok");
                    trace.io("ready_ack_write", || {
                        stream.write_all(SHARED_READY_ACK)?;
                        stream.flush()
                    })
                }
                Ok(Err(error)) => write_shared_rejection(&mut *stream).and(Err(error)),
                Err(_) => write_shared_rejection(&mut *stream).and(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "shared clipboard ready acknowledgement timed out",
                ))),
            };
            if outcome.is_err() {
                notify_remote_shared_failed(services, begin.event_id);
            }
            receive.finish(if outcome.is_ok() { "ok" } else { "failed" });
            outcome
        }
    }
}

fn write_shared_rejection(stream: &mut impl io::Write) -> io::Result<()> {
    stream.write_all(SHARED_REJECTED_ACK)?;
    stream.flush()
}

fn shared_begin_allowed(begin: &SharedTextBegin, policy: SharedCapturePolicy) -> bool {
    let has_rich = begin
        .formats
        .iter()
        .any(|format| matches!(format.kind, SharedFormatKind::Html | SharedFormatKind::Rtf));
    begin.formats.iter().all(|format| match format.kind {
        SharedFormatKind::UnicodeText => policy.text || (policy.rich_text && has_rich),
        SharedFormatKind::Html | SharedFormatKind::Rtf => policy.rich_text,
        SharedFormatKind::Png | SharedFormatKind::DibV5 | SharedFormatKind::Dib => policy.image,
    })
}

fn notify_remote_shared_failed(services: &OfferInboxServices, event_id: ClipboardEventId) {
    let _ = services
        .event_sender
        .send(WorkerEvent::RemoteSharedFailed { event_id });
    let _ = post_worker_message(services.worker_thread_id, PRODUCT_EVENT_MESSAGE);
}

struct OfferInbox {
    #[cfg(test)]
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    active_socket: Arc<Mutex<Option<TcpStream>>>,
    worker: Option<JoinHandle<()>>,
}

impl OfferInbox {
    fn start(
        store: DeviceStore,
        settings: &AppSettings,
        services: OfferInboxServices,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(settings.local_endpoint)?;
        listener.set_nonblocking(true)?;
        #[cfg(test)]
        let address = listener.local_addr()?;
        let identity = store.load_identity()?;
        let tls = TrustedTlsServer::new(identity, store, TLS_TIMEOUT)?;
        let active_peer = settings.active_peer;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let active_socket = Arc::new(Mutex::new(None));
        let worker_socket = Arc::clone(&active_socket);
        let worker = std::thread::Builder::new()
            .name("pierdesk-offer-inbox".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    let Ok((socket, _)) = listener.accept() else {
                        // The old interface may disappear during a Wi-Fi/hotspot switch.
                        // Never require a connection to that address in order to stop.
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    };
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if socket.set_nonblocking(false).is_err() {
                        continue;
                    }
                    {
                        let mut active = worker_socket
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *active = socket.try_clone().ok();
                    }
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let result =
                        tls.accept_cancellable(socket, &thread_stop)
                            .and_then(|connection| {
                                handle_offer_inbox_connection(connection, &services, active_peer)
                            });
                    worker_socket
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if result.is_ok() {
                        let _ =
                            post_worker_message(services.worker_thread_id, PRODUCT_EVENT_MESSAGE);
                    }
                }
            })?;
        Ok(Self {
            #[cfg(test)]
            address,
            stop,
            active_socket,
            worker: Some(worker),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(socket) = self
            .active_socket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for OfferInbox {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
fn send_announcement(
    store: &DeviceStore,
    settings: &AppSettings,
    endpoint: SocketAddr,
) -> io::Result<()> {
    let mut stream = build_pinned_tls(store, settings)?.connect(settings.peer_endpoint)?;
    stream.write_all(&encode_announcement(endpoint)?)?;
    stream.flush()?;
    let mut acknowledgement = [0_u8; OFFER_ACK.len()];
    stream.read_exact(&mut acknowledgement)?;
    if &acknowledgement != OFFER_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer returned an invalid offer acknowledgement",
        ));
    }
    let _ = stream.sock.shutdown(Shutdown::Both);
    Ok(())
}

fn remote_offer_failure_message(offer_endpoint: SocketAddr, error: &io::Error) -> String {
    if offer_endpoint.ip().is_loopback() {
        format!(
            "已收到对端文件清单，但反向文件数据隧道尚未就绪（{error}）。Pierdesk 会保留该清单并继续恢复隧道；跨屏控制使用独立连接，不受此次失败影响。"
        )
    } else {
        format!(
            "已收到对端文件清单，但无法连接对端文件数据入口 {offer_endpoint}（{error}）。请确认两台电脑都运行相同的新版 Pierdesk；跨屏控制使用独立连接，不受此次失败影响。该清单仍保持待接收状态。"
        )
    }
}

fn build_offer_client(
    store: &DeviceStore,
    settings: &AppSettings,
    endpoint: SocketAddr,
) -> io::Result<SecureOfferClient> {
    let tls = build_pinned_tls(store, settings)?;
    Ok(SecureOfferClient::with_recovery_policy(
        endpoint,
        tls,
        SecureRecoveryPolicy {
            max_elapsed: RECOVERY_WINDOW,
            connect_attempt_timeout: Duration::from_secs(2),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
        },
    ))
}

fn build_pinned_tls(store: &DeviceStore, settings: &AppSettings) -> io::Result<PinnedTlsClient> {
    let identity = store.load_identity()?;
    let peer = store.load_peer(settings.active_peer)?;
    PinnedTlsClient::new(
        &identity,
        peer.into_certificate_der(),
        settings.active_peer,
        TLS_TIMEOUT,
    )
}

#[cfg(test)]
fn encode_announcement(endpoint: SocketAddr) -> io::Result<[u8; OFFER_FRAME_SIZE]> {
    validate_private_endpoint(endpoint)?;
    let mut frame = [0_u8; OFFER_FRAME_SIZE];
    frame[..8].copy_from_slice(OFFER_MAGIC);
    match endpoint.ip() {
        IpAddr::V4(ip) => {
            frame[8] = 4;
            frame[9..13].copy_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            frame[8] = 6;
            frame[9..25].copy_from_slice(&ip.octets());
        }
    }
    frame[25..27].copy_from_slice(&endpoint.port().to_be_bytes());
    Ok(frame)
}

#[cfg(test)]
fn read_announcement(stream: &mut impl io::Read) -> io::Result<SocketAddr> {
    let mut magic = [0_u8; OFFER_MAGIC.len()];
    stream.read_exact(&mut magic)?;
    if magic != *OFFER_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid offer announcement magic",
        ));
    }
    read_announcement_after_magic(stream)
}

fn read_announcement_after_magic(stream: &mut impl io::Read) -> io::Result<SocketAddr> {
    let mut frame = [0_u8; OFFER_FRAME_SIZE - 8];
    stream.read_exact(&mut frame)?;
    let ip = match frame[0] {
        4 => IpAddr::V4(std::net::Ipv4Addr::new(
            frame[1], frame[2], frame[3], frame[4],
        )),
        6 => {
            let octets: [u8; 16] = frame[1..17]
                .try_into()
                .map_err(|_| io::Error::other("invalid IPv6 offer endpoint"))?;
            IpAddr::V6(std::net::Ipv6Addr::from(octets))
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid offer endpoint address family",
            ));
        }
    };
    let port = u16::from_be_bytes([frame[17], frame[18]]);
    let endpoint = SocketAddr::new(ip, port);
    validate_private_endpoint(endpoint)?;
    Ok(endpoint)
}

fn summarize_manifest(direction: &'static str, manifest: &OfferManifest) -> ProductManifestSummary {
    let files = manifest
        .entries
        .iter()
        .filter(|entry| !entry.descriptor.is_directory())
        .count();
    let total_size = manifest
        .entries
        .iter()
        .map(|entry| entry.descriptor.size)
        .sum();
    let primary_name = manifest.entries.first().map_or_else(
        || "（空清单）".to_owned(),
        |entry| entry.descriptor.file_name.to_string(),
    );
    ProductManifestSummary {
        direction,
        primary_name,
        items: manifest.entries.len(),
        files,
        directories: manifest.entries.len().saturating_sub(files),
        total_size,
    }
}

fn remote_task_id(task: &RemoteTransferTask) -> ProductTransferId {
    ProductTransferId::new(
        ProductTransferDirection::Receiving,
        task.manifest.offer_id.bytes(),
    )
}

fn source_task_id(source: &LiveSourceOffer) -> ProductTransferId {
    ProductTransferId::new(
        ProductTransferDirection::Sending,
        source.manifest.offer_id.bytes(),
    )
}

fn post_worker_message(thread_id: u32, message: u32) -> io::Result<()> {
    // SAFETY: private messages carry no pointers and target the known product worker thread.
    unsafe { PostThreadMessageW(thread_id, message, WPARAM(0), LPARAM(0)) }
        .map_err(|error| io::Error::other(error.to_string()))
}

fn append_product_log(path: &std::path::Path, message: &str) -> io::Result<()> {
    crate::logging::append(path, message)
}

#[allow(clippy::needless_pass_by_value)]
fn windows_to_io(error: Error) -> io::Error {
    io::Error::other(format!(
        "{} ({:#010X})",
        error,
        error.code().0.cast_unsigned()
    ))
}

#[allow(clippy::needless_pass_by_value)]
fn io_to_windows(error: io::Error) -> Error {
    if let Some(code) = error.raw_os_error() {
        Error::from_hresult(HRESULT::from_win32(code.cast_unsigned()))
    } else {
        Error::new(E_UNEXPECTED, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::snapshot::SharedFormatData;
    use std::sync::atomic::AtomicU64;

    static NEXT_TEST_STORE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn task_controls_support_both_live_directions() {
        for state in [
            ProductTransferState::AwaitingPaste,
            ProductTransferState::Running,
            ProductTransferState::Paused,
            ProductTransferState::Completed,
            ProductTransferState::Cancelled,
            ProductTransferState::Failed,
        ] {
            assert_eq!(
                local_task_control_supported(ProductTransferDirection::Sending, state),
                local_task_control_supported(ProductTransferDirection::Receiving, state)
            );
            assert_eq!(
                local_task_control_supported(ProductTransferDirection::Receiving, state),
                matches!(
                    state,
                    ProductTransferState::AwaitingPaste
                        | ProductTransferState::Running
                        | ProductTransferState::Paused
                )
            );
        }
        let sending = ProductTransferId::new(ProductTransferDirection::Sending, [1; 16]);
        let receiving = ProductTransferId::new(ProductTransferDirection::Receiving, [1; 16]);
        assert_eq!(
            require_receiving_task(sending).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(require_receiving_task(receiving).is_ok());
    }

    fn ensure_current_thread_message_queue() {
        let mut message = MSG::default();
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &raw mut message,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_NOREMOVE,
            )
        };
    }

    #[test]
    fn average_speed_excludes_pause_and_freezes_after_completion() {
        let now = Instant::now();
        let mut transfer = ProductTransferSnapshot {
            state: ProductTransferState::Running,
            transferred: 100,
            total_size: 1000,
            bytes_per_second: 0,
            average_bytes_per_second: 0,
            started_files: 1,
            total_files: 1,
            current_file_name: None,
            current_file_transferred: 0,
            current_file_size: 1000,
            reconnect_attempts: 0,
            recovered_commands: 0,
            recovery_active: false,
        };
        let mut sample = TransferSample::new(now, 0, ProductTransferState::Running);
        transfer.state = ProductTransferState::Paused;
        sample.update(&mut transfer, now + Duration::from_secs(1));
        assert_eq!(transfer.bytes_per_second, 0);
        assert_eq!(transfer.average_bytes_per_second, 100);
        sample.update(&mut transfer, now + Duration::from_mins(1));
        assert_eq!(transfer.average_bytes_per_second, 100);
        transfer.state = ProductTransferState::Running;
        sample.update(&mut transfer, now + Duration::from_secs(61));
        transfer.transferred = 200;
        transfer.state = ProductTransferState::Completed;
        sample.update(&mut transfer, now + Duration::from_secs(62));
        assert_eq!(transfer.average_bytes_per_second, 100);
        sample.update(&mut transfer, now + Duration::from_mins(2));
        assert_eq!(transfer.average_bytes_per_second, 100);
    }

    struct TestStore {
        store: DeviceStore,
        root: std::path::PathBuf,
    }

    impl TestStore {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_STORE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "pierdesk-product-{label}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self {
                store: DeviceStore::new(&root),
                root,
            }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const fn enabled_shared_policy() -> SharedCapturePolicy {
        SharedCapturePolicy {
            text: true,
            rich_text: true,
            image: true,
        }
    }

    fn received_text_snapshot(
        event_id: ClipboardEventId,
        revision: u64,
        units: &[u16],
    ) -> SharedClipboardSnapshot {
        let bytes = units
            .iter()
            .flat_map(|unit| unit.to_le_bytes())
            .collect::<Vec<_>>();
        let format =
            SharedFormatData::new(SharedFormatKind::UnicodeText, Arc::from(bytes)).unwrap();
        SharedClipboardSnapshot::received(event_id, revision, Arc::from([format])).unwrap()
    }

    fn begin_with_kinds(kinds: &[SharedFormatKind]) -> SharedTextBegin {
        SharedTextBegin::from_kinds(ClipboardEventId([4; 16]), 1, kinds).unwrap()
    }

    #[test]
    fn simultaneous_clipboard_orders_converge_by_revision_then_fingerprint() {
        let lower = ClipboardOrder {
            revision: 7,
            source: CertificateFingerprint::from_bytes([1; 32]),
        };
        let higher_peer = ClipboardOrder {
            revision: 7,
            source: CertificateFingerprint::from_bytes([2; 32]),
        };
        let later = ClipboardOrder {
            revision: 8,
            source: CertificateFingerprint::from_bytes([0; 32]),
        };
        assert!(higher_peer.is_newer_than(lower));
        assert!(!lower.is_newer_than(higher_peer));
        assert!(later.is_newer_than(higher_peer));
    }

    #[test]
    fn receiver_category_policy_keeps_plain_rich_and_image_independent() {
        let rich_only = SharedCapturePolicy {
            text: false,
            rich_text: true,
            image: false,
        };
        assert!(!shared_begin_allowed(
            &begin_with_kinds(&[SharedFormatKind::UnicodeText]),
            rich_only,
        ));
        assert!(shared_begin_allowed(
            &begin_with_kinds(&[SharedFormatKind::UnicodeText, SharedFormatKind::Html]),
            rich_only,
        ));
        let text_only = SharedCapturePolicy {
            text: true,
            rich_text: false,
            image: false,
        };
        assert!(shared_begin_allowed(
            &begin_with_kinds(&[SharedFormatKind::UnicodeText]),
            text_only,
        ));
        assert!(!shared_begin_allowed(
            &begin_with_kinds(&[SharedFormatKind::UnicodeText, SharedFormatKind::Rtf]),
            text_only,
        ));
        assert!(!shared_begin_allowed(
            &begin_with_kinds(&[SharedFormatKind::Png]),
            text_only,
        ));
    }

    #[test]
    fn offer_announcement_round_trips_private_ipv4_and_ipv6() {
        for endpoint in ["192.168.1.9:49152", "[fd00::9]:49152"] {
            let endpoint: SocketAddr = endpoint.parse().unwrap();
            let encoded = encode_announcement(endpoint).unwrap();
            assert_eq!(
                read_announcement(&mut encoded.as_slice()).unwrap(),
                endpoint
            );
        }
    }

    #[test]
    fn offer_announcement_rejects_public_zero_port_and_bad_magic() {
        assert!(encode_announcement("8.8.8.8:45233".parse().unwrap()).is_err());
        assert!(encode_announcement("192.168.1.9:0".parse().unwrap()).is_err());
        let mut frame = encode_announcement("192.168.1.9:45233".parse().unwrap()).unwrap();
        frame[0] ^= 0xff;
        assert!(read_announcement(&mut frame.as_slice()).is_err());
    }

    #[test]
    fn remote_offer_timeout_names_the_unreachable_data_endpoint_and_remains_pending() {
        let message = remote_offer_failure_message(
            "192.168.1.11:64465".parse().unwrap(),
            &io::Error::new(io::ErrorKind::TimedOut, "connection timed out"),
        );

        assert!(message.contains("192.168.1.11:64465"));
        assert!(message.contains("文件数据入口"));
        assert!(message.contains("跨屏控制使用独立连接"));
        assert!(message.contains("仍保持待接收状态"));

        let reverse_message = remote_offer_failure_message(
            "127.0.0.1:64465".parse().unwrap(),
            &io::Error::new(io::ErrorKind::TimedOut, "connection timed out"),
        );
        assert!(reverse_message.contains("反向文件数据隧道"));
        assert!(reverse_message.contains("继续恢复隧道"));
    }

    #[test]
    fn public_snapshot_layout_stays_small() {
        assert!(std::mem::size_of::<ProductSnapshot>() < 512);
    }

    #[test]
    fn inbox_stop_does_not_depend_on_the_old_interface_or_a_tls_peer() {
        for stalled_handshake in [false, true].into_iter().cycle().take(20) {
            let store = TestStore::new("inbox-network-switch");
            store.store.load_or_create_identity().unwrap();
            let peer_store = TestStore::new("inbox-network-switch-peer");
            let identity = peer_store.store.load_or_create_identity().unwrap().identity;
            store
                .store
                .trust_peer(
                    identity.certificate_der().to_vec(),
                    identity.fingerprint(),
                    "peer",
                )
                .unwrap();
            let settings = AppSettings {
                local_endpoint: "127.0.0.1:0".parse().unwrap(),
                active_peer: identity.fingerprint(),
                peer_endpoint: "127.0.0.1:9".parse().unwrap(),
                auto_receive: true,
            };
            let (event_sender, _events) = mpsc::channel();
            let mut inbox = OfferInbox::start(
                store.store.clone(),
                &settings,
                OfferInboxServices {
                    event_sender,
                    worker_thread_id: unsafe { GetCurrentThreadId() },
                    downlink: Arc::new(OfferDownlink::default()),
                    reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                    shared_text_enabled: true,
                    shared_policy: enabled_shared_policy(),
                    local_generation: Arc::new(AtomicU64::new(0)),
                },
            )
            .unwrap();
            let _client = stalled_handshake.then(|| TcpStream::connect(inbox.address).unwrap());
            if stalled_handshake {
                let deadline = Instant::now() + Duration::from_secs(2);
                while inbox.active_socket.lock().unwrap().is_none() {
                    assert!(
                        Instant::now() < deadline,
                        "listener did not accept the stalled peer"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            // Simulate an address no longer usable as a wake-up route after disconnect.
            inbox.address = "192.0.2.1:9".parse().unwrap();
            let (done, result) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                inbox.stop();
                let _ = done.send(());
            });
            assert!(
                result.recv_timeout(Duration::from_secs(2)).is_ok(),
                "network switch must not hang stop (stalled_handshake={stalled_handshake})"
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn clipboard_sequence_is_claimed_before_success_rejection_or_read_failure() {
        let mut last = None;
        assert!(claim_clipboard_sequence(&mut last, 41));
        assert_eq!(last, Some(41));
        assert!(!claim_clipboard_sequence(&mut last, 41));
        assert!(claim_clipboard_sequence(&mut last, 42));
        assert_eq!(last, Some(42));
        assert!(claim_clipboard_sequence(&mut last, 0));
    }

    #[test]
    fn shared_rejection_ack_is_not_misreported_as_a_network_failure() {
        let error = read_shared_ack(
            &mut std::io::Cursor::new(*SHARED_REJECTED_ACK),
            *SHARED_READY_ACK,
            "ready",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("peer rejected"));
    }

    #[test]
    fn receiver_writes_rejection_ack_when_pending_installation_fails() {
        let begin = SharedTextBegin::from_kinds(
            ClipboardEventId([0xB7; 16]),
            23,
            &[SharedFormatKind::UnicodeText],
        )
        .unwrap();
        let mut bytes = Vec::new();
        write_begin(&mut bytes, &begin).unwrap();
        let mut stream = std::io::Cursor::new(bytes[SHARED_MAGIC.len()..].to_vec());
        let input_length = stream.get_ref().len();
        let (event_sender, event_receiver) = mpsc::channel();
        let services = OfferInboxServices {
            event_sender,
            // SAFETY: this test only uses the id for a best-effort wake message.
            worker_thread_id: unsafe { GetCurrentThreadId() },
            downlink: Arc::new(OfferDownlink::default()),
            reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
            shared_text_enabled: true,
            shared_policy: enabled_shared_policy(),
            local_generation: Arc::new(AtomicU64::new(0)),
        };
        let responder = std::thread::spawn(move || {
            let WorkerEvent::RemoteSharedPending(event) = event_receiver.recv().unwrap() else {
                panic!("expected pending shared clipboard event");
            };
            event
                .completion
                .send(Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "simulated clipboard contention",
                )))
                .unwrap();
        });

        let error = handle_shared_stream(
            &mut stream,
            CertificateFingerprint::from_bytes([0x39; 32]),
            &services,
        )
        .unwrap_err();
        responder.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            &stream.into_inner()[input_length..],
            SHARED_REJECTED_ACK.as_slice()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn authenticated_inactive_peer_shared_text_is_acknowledged_after_apply() {
        let sender_store = TestStore::new("shared-sender");
        let receiver_store = TestStore::new("shared-receiver");
        let active_store = TestStore::new("shared-active");
        let sender_identity = sender_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        let receiver_identity = receiver_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        let active_identity = active_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        sender_store
            .store
            .trust_peer(
                receiver_identity.certificate_der().to_vec(),
                receiver_identity.fingerprint(),
                "receiver",
            )
            .unwrap();
        receiver_store
            .store
            .trust_peer(
                sender_identity.certificate_der().to_vec(),
                sender_identity.fingerprint(),
                "sender",
            )
            .unwrap();
        receiver_store
            .store
            .trust_peer(
                active_identity.certificate_der().to_vec(),
                active_identity.fingerprint(),
                "active",
            )
            .unwrap();

        let receiver_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: active_identity.fingerprint(),
            peer_endpoint: "127.0.0.1:9".parse().unwrap(),
            auto_receive: true,
        };
        let (event_sender, event_receiver) = mpsc::channel();
        let sender_events = event_sender.clone();
        let local_generation = Arc::new(AtomicU64::new(7));
        let mut inbox = OfferInbox::start(
            receiver_store.store.clone(),
            &receiver_settings,
            OfferInboxServices {
                event_sender,
                worker_thread_id: unsafe { GetCurrentThreadId() },
                downlink: Arc::new(OfferDownlink::default()),
                reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                shared_text_enabled: true,
                shared_policy: enabled_shared_policy(),
                local_generation: Arc::clone(&local_generation),
            },
        )
        .unwrap();
        let sender_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: receiver_identity.fingerprint(),
            peer_endpoint: inbox.address,
            auto_receive: true,
        };
        let expected = received_text_snapshot(
            ClipboardEventId([0xA5; 16]),
            11,
            &[0x4F60_u16, 0x597D, 0xD83D, 0xDE80],
        );
        let sender_device_store = sender_store.store.clone();
        let sent_snapshot = expected.clone();
        let sender_thread_id = unsafe { GetCurrentThreadId() };
        let sender_worker = std::thread::spawn(move || {
            let tls = build_pinned_tls(&sender_device_store, &sender_settings).unwrap();
            let job = SharedOutboundJob {
                trace: super::super::shared_diagnostics::SharedTrace::disabled(),
                begin: SharedTextBegin::from_snapshot(&sent_snapshot).unwrap(),
                result: Mutex::new(Some(Ok(sent_snapshot))),
                changed: Condvar::new(),
                cancelled: AtomicBool::new(false),
            };
            SharedSendTransport::Listener {
                downlink: Arc::new(OfferDownlink::default()),
                tls,
                peer_endpoint: sender_settings.peer_endpoint,
            }
            .send(&job, &sender_events, sender_thread_id)
        });

        let event = event_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let WorkerEvent::RemoteSharedPending(event) = event else {
            panic!("expected a pending shared clipboard event");
        };
        assert_eq!(event.begin.event_id, expected.event_id);
        assert_eq!(event.begin.revision, expected.revision);
        assert_eq!(event.peer, sender_identity.fingerprint());
        assert_eq!(event.observed_local_generation, 7);
        let (_, pending) = SharedTextDataObject::create_pending(&event.begin).unwrap();
        event.completion.send(Ok(pending)).unwrap();

        let mut saw_installed = false;
        let mut saw_ready = false;
        while !saw_installed || !saw_ready {
            match event_receiver.recv_timeout(Duration::from_secs(2)).unwrap() {
                WorkerEvent::SharedInstalled { event_id } => {
                    assert_eq!(event_id, expected.event_id);
                    saw_installed = true;
                }
                WorkerEvent::RemoteSharedReady(event) => {
                    assert_eq!(event.snapshot, expected);
                    event.completion.send(Ok(())).unwrap();
                    saw_ready = true;
                }
                _ => panic!("unexpected shared clipboard event"),
            }
        }
        sender_worker.join().unwrap().unwrap();
        inbox.stop();
    }

    #[test]
    fn authenticated_offer_inbox_keeps_file_offers_scoped_to_the_selected_peer() {
        let first = TestStore::new("first");
        let second = TestStore::new("second");
        let first_identity = first.store.load_or_create_identity().unwrap().identity;
        let second_identity = second.store.load_or_create_identity().unwrap().identity;
        first
            .store
            .trust_peer(
                second_identity.certificate_der().to_vec(),
                second_identity.fingerprint(),
                "second",
            )
            .unwrap();
        second
            .store
            .trust_peer(
                first_identity.certificate_der().to_vec(),
                first_identity.fingerprint(),
                "first",
            )
            .unwrap();
        let receiver_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: first_identity.fingerprint(),
            peer_endpoint: "127.0.0.1:45234".parse().unwrap(),
            auto_receive: true,
        };
        let (sender, receiver) = mpsc::channel();
        // SAFETY: the numeric id is used only for an ignored best-effort wake message in this test.
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut inbox = OfferInbox::start(
            second.store.clone(),
            &receiver_settings,
            OfferInboxServices {
                event_sender: sender,
                worker_thread_id: thread_id,
                downlink: Arc::new(OfferDownlink::default()),
                reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                shared_text_enabled: true,
                shared_policy: enabled_shared_policy(),
                local_generation: Arc::new(AtomicU64::new(0)),
            },
        )
        .unwrap();
        let sender_settings = AppSettings {
            local_endpoint: "127.0.0.1:45234".parse().unwrap(),
            active_peer: second_identity.fingerprint(),
            peer_endpoint: inbox.address,
            auto_receive: true,
        };
        let source_tls = TrustedTlsServer::new(
            first.store.load_identity().unwrap(),
            first.store.clone(),
            TLS_TIMEOUT,
        )
        .unwrap();
        let offered =
            SecureOfferedFile::generated(Arc::<str>::from("product-smoke.bin"), 4096, OFFER_TTL)
                .unwrap();
        let mut source_server = SecureOfferServer::start_trusted(
            "127.0.0.1:0".parse().unwrap(),
            source_tls,
            second_identity.fingerprint(),
            offered,
            TRANSFER_TTL,
            TLS_TIMEOUT,
        )
        .unwrap();
        let transfer_endpoint = source_server.address();
        send_announcement(&first.store, &sender_settings, transfer_endpoint).unwrap();
        let event = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let WorkerEvent::RemoteOffer(announcement) = event else {
            panic!("expected a remote offer event");
        };
        assert_eq!(announcement.endpoint, transfer_endpoint);
        assert_eq!(announcement.peer, first_identity.fingerprint());
        let client =
            build_offer_client(&second.store, &receiver_settings, announcement.endpoint).unwrap();
        let manifest = client.fetch_manifest().unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(
            manifest.entries[0].descriptor.file_name.as_ref(),
            "product-smoke.bin"
        );
        assert_eq!(manifest.entries[0].descriptor.size, 4096);
        let source = super::super::secure_transfer::SecureRemoteSource::new(client, manifest);
        let mut bytes = [0_u8; 32];
        assert_eq!(source.read_at(1024, &mut bytes).unwrap(), bytes.len());
        assert!(bytes.iter().any(|byte| *byte != 0));
        inbox.stop();
        source_server.stop();
    }

    #[test]
    fn authenticated_inactive_peer_can_send_a_boundary_workspace_event() {
        ensure_current_thread_message_queue();
        let receiver = TestStore::new("boundary-receiver");
        let active = TestStore::new("boundary-active");
        let editor = TestStore::new("boundary-editor");
        let receiver_identity = receiver.store.load_or_create_identity().unwrap().identity;
        let active_identity = active.store.load_or_create_identity().unwrap().identity;
        let editor_identity = editor.store.load_or_create_identity().unwrap().identity;
        receiver
            .store
            .trust_peer(
                active_identity.certificate_der().to_vec(),
                active_identity.fingerprint(),
                "active",
            )
            .unwrap();
        receiver
            .store
            .trust_peer(
                editor_identity.certificate_der().to_vec(),
                editor_identity.fingerprint(),
                "editor",
            )
            .unwrap();
        editor
            .store
            .trust_peer(
                receiver_identity.certificate_der().to_vec(),
                receiver_identity.fingerprint(),
                "receiver",
            )
            .unwrap();
        let settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: active_identity.fingerprint(),
            peer_endpoint: "127.0.0.1:9".parse().unwrap(),
            auto_receive: true,
        };
        let (event_sender, event_receiver) = mpsc::channel();
        let mut inbox = OfferInbox::start(
            receiver.store.clone(),
            &settings,
            OfferInboxServices {
                event_sender,
                worker_thread_id: unsafe { GetCurrentThreadId() },
                downlink: Arc::new(OfferDownlink::default()),
                reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                shared_text_enabled: false,
                shared_policy: SharedCapturePolicy {
                    text: false,
                    rich_text: false,
                    image: false,
                },
                local_generation: Arc::new(AtomicU64::new(0)),
            },
        )
        .unwrap();
        let endpoint = inbox.address;
        let editor_store = editor.store.clone();
        let expected = b"authenticated-boundary-payload".to_vec();
        let sent = expected.clone();
        let client = std::thread::spawn(move || {
            let tls = PinnedTlsClient::new(
                &editor_store.load_identity().unwrap(),
                editor_store
                    .load_peer(receiver_identity.fingerprint())
                    .unwrap()
                    .into_certificate_der(),
                receiver_identity.fingerprint(),
                Duration::from_secs(2),
            )
            .unwrap();
            let mut stream = tls.connect_with_timeout(endpoint, Duration::from_secs(2))?;
            stream.write_all(BOUNDARY_WORKSPACE_MAGIC)?;
            stream.write_all(&u32::try_from(sent.len()).unwrap().to_be_bytes())?;
            stream.write_all(&sent)?;
            stream.flush()?;
            let mut acknowledgement = [0_u8; 8];
            stream.read_exact(&mut acknowledgement)?;
            assert_eq!(acknowledgement, *BOUNDARY_WORKSPACE_ACK);
            Ok::<(), io::Error>(())
        });
        let WorkerEvent::RemoteBoundaryWorkspace {
            peer,
            payload,
            completion,
        } = event_receiver.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected a boundary workspace event");
        };
        assert_eq!(peer, editor_identity.fingerprint());
        assert_eq!(payload, expected);
        completion.send(Ok(())).unwrap();
        client.join().unwrap().unwrap();
        inbox.stop();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn connector_downlink_delivers_listener_offer_without_connector_inbound_access() {
        let listener_store = TestStore::new("downlink-listener");
        let connector_store = TestStore::new("downlink-connector");
        let listener_identity = listener_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        let connector_identity = connector_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        listener_store
            .store
            .trust_peer(
                connector_identity.certificate_der().to_vec(),
                connector_identity.fingerprint(),
                "connector",
            )
            .unwrap();
        connector_store
            .store
            .trust_peer(
                listener_identity.certificate_der().to_vec(),
                listener_identity.fingerprint(),
                "listener",
            )
            .unwrap();

        let listener_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: connector_identity.fingerprint(),
            peer_endpoint: "127.0.0.1:9".parse().unwrap(),
            auto_receive: true,
        };
        let (listener_sender, listener_receiver) = mpsc::channel();
        let thread_id = unsafe { GetCurrentThreadId() };
        let downlink = Arc::new(OfferDownlink::default());
        let mut inbox = OfferInbox::start(
            listener_store.store.clone(),
            &listener_settings,
            OfferInboxServices {
                event_sender: listener_sender.clone(),
                worker_thread_id: thread_id,
                downlink: Arc::clone(&downlink),
                reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                shared_text_enabled: true,
                shared_policy: enabled_shared_policy(),
                local_generation: Arc::new(AtomicU64::new(0)),
            },
        )
        .unwrap();
        let connector_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: listener_identity.fingerprint(),
            peer_endpoint: inbox.address,
            auto_receive: true,
        };
        let (connector_sender, connector_receiver) = mpsc::channel();
        let mut link = OutboundOfferLink::start(
            &connector_store.store,
            &connector_settings,
            OfferInboxServices {
                event_sender: connector_sender,
                worker_thread_id: thread_id,
                downlink: Arc::new(OfferDownlink::default()),
                reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                shared_text_enabled: true,
                shared_policy: enabled_shared_policy(),
                local_generation: Arc::new(AtomicU64::new(0)),
            },
        )
        .unwrap();

        let endpoint: SocketAddr = "127.0.0.1:54321".parse().unwrap();
        downlink
            .send_recovering(endpoint, Duration::from_secs(5))
            .unwrap();
        let WorkerEvent::RemoteOffer(announcement) = connector_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
        else {
            panic!("expected downlink offer");
        };
        assert_eq!(announcement.endpoint, endpoint);
        assert_eq!(announcement.peer, listener_identity.fingerprint());

        let expected = received_text_snapshot(
            ClipboardEventId([0xD2; 16]),
            17,
            &[0x53CD_u16, 0x5411, 0x6587, 0x5B57],
        );
        let downlink_for_shared = Arc::clone(&downlink);
        let sent_snapshot = expected.clone();
        let shared_sender = std::thread::spawn(move || {
            let job = SharedOutboundJob {
                trace: super::super::shared_diagnostics::SharedTrace::disabled(),
                begin: SharedTextBegin::from_snapshot(&sent_snapshot).unwrap(),
                result: Mutex::new(Some(Ok(sent_snapshot))),
                changed: Condvar::new(),
                cancelled: AtomicBool::new(false),
            };
            downlink_for_shared.send_shared(&job, &listener_sender, thread_id)
        });
        let WorkerEvent::RemoteSharedPending(pending_event) = connector_receiver
            .recv_timeout(Duration::from_secs(4))
            .unwrap()
        else {
            panic!("expected shared text pending event over downlink");
        };
        assert_eq!(pending_event.begin.event_id, expected.event_id);
        let (_, pending) = SharedTextDataObject::create_pending(&pending_event.begin).unwrap();
        pending_event.completion.send(Ok(pending)).unwrap();

        let WorkerEvent::SharedInstalled { event_id } = listener_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
        else {
            panic!("expected shared text installation acknowledgement");
        };
        assert_eq!(event_id, expected.event_id);
        let WorkerEvent::RemoteSharedReady(ready_event) = connector_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
        else {
            panic!("expected shared text ready event over downlink");
        };
        assert_eq!(ready_event.snapshot, expected);
        ready_event.completion.send(Ok(())).unwrap();
        shared_sender.join().unwrap().unwrap();
        link.stop();
        inbox.stop();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn both_file_channel_roles_publish_drag_sources_only_on_loopback() {
        let first = TestStore::new("drag-both-first");
        let second = TestStore::new("drag-both-second");
        let first_identity = first.store.load_or_create_identity().unwrap().identity;
        let second_identity = second.store.load_or_create_identity().unwrap().identity;
        for (store, identity) in [
            (&first.store, &second_identity),
            (&second.store, &first_identity),
        ] {
            store
                .trust_peer(
                    identity.certificate_der().to_vec(),
                    identity.fingerprint(),
                    "peer",
                )
                .unwrap();
        }
        let mut roles = Vec::new();
        for (source, target, source_id, target_id) in [
            (
                &first.store,
                &second.store,
                first_identity.fingerprint(),
                second_identity.fingerprint(),
            ),
            (
                &second.store,
                &first.store,
                second_identity.fingerprint(),
                first_identity.fingerprint(),
            ),
        ] {
            let target_settings = AppSettings {
                local_endpoint: "127.0.0.1:0".parse().unwrap(),
                active_peer: source_id,
                peer_endpoint: "127.0.0.1:9".parse().unwrap(),
                auto_receive: true,
            };
            let (events, receiver) = mpsc::channel();
            let mut inbox = OfferInbox::start(
                target.clone(),
                &target_settings,
                OfferInboxServices {
                    event_sender: events,
                    worker_thread_id: unsafe { GetCurrentThreadId() },
                    downlink: Arc::new(OfferDownlink::default()),
                    reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                    shared_text_enabled: false,
                    shared_policy: SharedCapturePolicy {
                        text: false,
                        rich_text: false,
                        image: false,
                    },
                    local_generation: Arc::new(AtomicU64::new(0)),
                },
            )
            .unwrap();
            let source_settings = AppSettings {
                local_endpoint: "127.0.0.1:0".parse().unwrap(),
                active_peer: target_id,
                peer_endpoint: inbox.address,
                auto_receive: true,
            };
            roles.push(FileChannelRole::resolve(source, &source_settings).unwrap());
            let offered =
                SecureOfferedFile::generated(Arc::<str>::from("drag-both.bin"), 65_537, OFFER_TTL)
                    .unwrap();
            let (mut server, tunnel, port) =
                start_drag_source_offer(source, target_id, inbox.address, offered).unwrap();
            assert!(server.address().ip().is_loopback());
            let client = build_offer_client(
                target,
                &target_settings,
                SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
            )
            .unwrap();
            let manifest = client.fetch_manifest().unwrap();
            assert_eq!(manifest.offer_id, server.manifest().offer_id);
            let remote = super::super::secure_transfer::SecureRemoteSource::new(client, manifest);
            let mut bytes = vec![0; 65_537];
            assert_eq!(remote.read_at(0, &mut bytes).unwrap(), bytes.len());
            assert_eq!(
                bytes,
                (0..65_537)
                    .map(super::super::transfer::generated_byte)
                    .collect::<Vec<_>>()
            );
            // A drag tunnel must not also announce an ordinary clipboard transfer.
            assert!(receiver.try_recv().is_err());
            drop(remote);
            drop(tunnel);
            server.stop();
            inbox.stop();
        }
        assert!(roles.contains(&FileChannelRole::Listener));
        assert!(roles.contains(&FileChannelRole::Connector));
    }

    #[test]
    fn listener_role_clipboard_offer_is_published_through_loopback_reverse_tunnel() {
        let source_store = TestStore::new("clipboard-listener-source");
        let receiver_store = TestStore::new("clipboard-listener-receiver");
        let source_identity = source_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        let receiver_identity = receiver_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        source_store
            .store
            .trust_peer(
                receiver_identity.certificate_der().to_vec(),
                receiver_identity.fingerprint(),
                "receiver",
            )
            .unwrap();
        receiver_store
            .store
            .trust_peer(
                source_identity.certificate_der().to_vec(),
                source_identity.fingerprint(),
                "source",
            )
            .unwrap();

        let (events, receiver) = mpsc::channel();
        let receiver_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: source_identity.fingerprint(),
            peer_endpoint: "127.0.0.1:9".parse().unwrap(),
            auto_receive: true,
        };
        let mut inbox = OfferInbox::start(
            receiver_store.store.clone(),
            &receiver_settings,
            OfferInboxServices {
                event_sender: events,
                worker_thread_id: unsafe { GetCurrentThreadId() },
                downlink: Arc::new(OfferDownlink::default()),
                reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                shared_text_enabled: false,
                shared_policy: SharedCapturePolicy {
                    text: false,
                    rich_text: false,
                    image: false,
                },
                local_generation: Arc::new(AtomicU64::new(0)),
            },
        )
        .unwrap();
        let source_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: receiver_identity.fingerprint(),
            peer_endpoint: inbox.address,
            auto_receive: true,
        };
        let offered = SecureOfferedFile::generated(
            Arc::<str>::from("listener-route.bin"),
            65_537,
            OFFER_TTL,
        )
        .unwrap();
        let (mut server, tunnel) =
            start_clipboard_source_offer(&source_store.store, &source_settings, offered).unwrap();
        // The source must not expose a LAN endpoint that the receiver has to reach directly.
        assert!(server.address().ip().is_loopback());

        let WorkerEvent::RemoteOffer(announcement) =
            receiver.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected reverse tunnel offer");
        };
        assert!(announcement.endpoint.ip().is_loopback());
        assert_eq!(announcement.peer, source_identity.fingerprint());
        let client = build_offer_client(
            &receiver_store.store,
            &receiver_settings,
            announcement.endpoint,
        )
        .unwrap();
        let manifest = client.fetch_manifest().unwrap();
        assert_eq!(manifest.offer_id, server.manifest().offer_id);

        drop(client);
        drop(tunnel);
        server.stop();
        inbox.stop();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn connector_source_streams_through_reverse_tls_tunnel_without_inbound_port() {
        let listener_store = TestStore::new("reverse-listener");
        let connector_store = TestStore::new("reverse-connector");
        let listener_identity = listener_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        let connector_identity = connector_store
            .store
            .load_or_create_identity()
            .unwrap()
            .identity;
        listener_store
            .store
            .trust_peer(
                connector_identity.certificate_der().to_vec(),
                connector_identity.fingerprint(),
                "connector",
            )
            .unwrap();
        connector_store
            .store
            .trust_peer(
                listener_identity.certificate_der().to_vec(),
                listener_identity.fingerprint(),
                "listener",
            )
            .unwrap();

        let listener_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: connector_identity.fingerprint(),
            peer_endpoint: "127.0.0.1:9".parse().unwrap(),
            auto_receive: true,
        };
        let (event_sender, event_receiver) = mpsc::channel();
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut inbox = OfferInbox::start(
            listener_store.store.clone(),
            &listener_settings,
            OfferInboxServices {
                event_sender,
                worker_thread_id: thread_id,
                downlink: Arc::new(OfferDownlink::default()),
                reverse_proxies: Arc::new(Mutex::new(VecDeque::new())),
                shared_text_enabled: false,
                shared_policy: SharedCapturePolicy {
                    text: false,
                    rich_text: false,
                    image: false,
                },
                local_generation: Arc::new(AtomicU64::new(0)),
            },
        )
        .unwrap();
        let connector_settings = AppSettings {
            local_endpoint: "127.0.0.1:0".parse().unwrap(),
            active_peer: listener_identity.fingerprint(),
            peer_endpoint: inbox.address,
            auto_receive: true,
        };
        let source_tls = TrustedTlsServer::new(
            connector_store.store.load_identity().unwrap(),
            connector_store.store.clone(),
            TLS_TIMEOUT,
        )
        .unwrap();
        let offered = SecureOfferedFile::generated(
            Arc::<str>::from("reverse-smoke.bin"),
            4 * 1024 * 1024,
            OFFER_TTL,
        )
        .unwrap();
        let mut source_server = SecureOfferServer::start_trusted(
            "127.0.0.1:0".parse().unwrap(),
            source_tls,
            listener_identity.fingerprint(),
            offered,
            TRANSFER_TTL,
            TLS_TIMEOUT,
        )
        .unwrap();
        let manifest = source_server.manifest();
        let tunnel = ReverseOfferTunnel::start(
            &build_pinned_tls(&connector_store.store, &connector_settings).unwrap(),
            inbox.address,
            source_server.address(),
            manifest.offer_id,
            Duration::from_secs(15),
        )
        .unwrap();
        let WorkerEvent::RemoteOffer(announcement) =
            event_receiver.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected reverse tunnel offer");
        };
        assert!(announcement.endpoint.ip().is_loopback());
        assert_eq!(announcement.peer, connector_identity.fingerprint());
        let client = build_offer_client(
            &listener_store.store,
            &listener_settings,
            announcement.endpoint,
        )
        .unwrap();
        let received_manifest = client.fetch_manifest().unwrap();
        assert_eq!(received_manifest.offer_id, manifest.offer_id);
        let source =
            super::super::secure_transfer::SecureRemoteSource::new(client, received_manifest);
        let mut bytes = vec![0_u8; 256 * 1024];
        let mut total = 0_u64;
        let transfer_clock = Instant::now();
        while total < 4 * 1024 * 1024 {
            let read = source.read_at(total, &mut bytes).unwrap();
            assert_eq!(read, bytes.len());
            assert!(bytes.iter().enumerate().all(|(i, byte)| *byte
                == super::super::transfer::generated_byte(total + u64::try_from(i).unwrap())));
            total += u64::try_from(read).unwrap();
        }
        assert_eq!(total, 4 * 1024 * 1024);
        println!(
            "FILE_TUNNEL_BENCH bytes={total} elapsed_ms={} (includes byte verification, loopback nested TLS)",
            transfer_clock.elapsed().as_millis()
        );

        drop(source);
        drop(tunnel);

        let drag_offered = SecureOfferedFile::generated(
            Arc::<str>::from("reverse-drag-smoke.bin"),
            4096,
            OFFER_TTL,
        )
        .unwrap();
        let mut drag_server = SecureOfferServer::start_trusted(
            "127.0.0.1:0".parse().unwrap(),
            TrustedTlsServer::new(
                connector_store.store.load_identity().unwrap(),
                connector_store.store.clone(),
                TLS_TIMEOUT,
            )
            .unwrap(),
            listener_identity.fingerprint(),
            drag_offered,
            TRANSFER_TTL,
            TLS_TIMEOUT,
        )
        .unwrap();
        let drag_manifest = drag_server.manifest();
        let (drag_tunnel, proxy_port) = ReverseOfferTunnel::start_drag(
            &build_pinned_tls(&connector_store.store, &connector_settings).unwrap(),
            inbox.address,
            drag_server.address(),
            drag_manifest.offer_id,
            Duration::from_secs(15),
        )
        .unwrap();
        assert!(
            event_receiver
                .recv_timeout(Duration::from_millis(200))
                .is_err()
        );
        let drag_client = build_offer_client(
            &listener_store.store,
            &listener_settings,
            SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, proxy_port)),
        )
        .unwrap();
        assert_eq!(
            drag_client.fetch_manifest().unwrap().offer_id,
            drag_manifest.offer_id
        );
        assert!(event_receiver.try_recv().is_err());

        drop(drag_client);
        drop(drag_tunnel);
        drag_server.stop();
        inbox.stop();
        source_server.stop();
    }

    #[test]
    fn product_runtime_starts_and_stops_its_listener_and_clipboard_sta() {
        let first = TestStore::new("runtime-first");
        let second = TestStore::new("runtime-second");
        let first_identity = first.store.load_or_create_identity().unwrap().identity;
        let second_identity = second.store.load_or_create_identity().unwrap().identity;
        second
            .store
            .trust_peer(
                first_identity.certificate_der().to_vec(),
                first_identity.fingerprint(),
                "first",
            )
            .unwrap();
        first
            .store
            .trust_peer(
                second_identity.certificate_der().to_vec(),
                second_identity.fingerprint(),
                "second",
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let local_endpoint = listener.local_addr().unwrap();
        drop(listener);
        let settings = AppSettings {
            local_endpoint,
            active_peer: first_identity.fingerprint(),
            peer_endpoint: "127.0.0.1:45234".parse().unwrap(),
            auto_receive: false,
        };
        let mut runtime = ProductRuntime::start(second.store.clone(), settings).unwrap();
        let snapshot = runtime.snapshot();
        assert!(snapshot.listener_running);
        assert_eq!(snapshot.active_peer, first_identity.fingerprint());
        assert!(!snapshot.pending_offer);
        runtime.stop();
        assert!(!runtime.snapshot().listener_running);
    }
}
