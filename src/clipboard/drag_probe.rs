#![allow(
    clippy::arc_with_non_send_sync,
    clippy::inline_always,
    clippy::ref_as_ptr
)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_POINTER,
    HINSTANCE, HWND, LPARAM, LRESULT, POINT, POINTL, S_OK, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber, RegisterClipboardFormatW};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl,
    IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    CFSTR_FILEDESCRIPTORW, CLSID_DragDropHelper, DragQueryFileW, HDROP, IDataObjectAsyncCapability,
    IDragSourceHelper, SHDRAGIMAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, KillTimer, LWA_ALPHA, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
    PostThreadMessageW, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNOACTIVATE,
    SetLayeredWindowAttributes, SetTimer, ShowWindow, TranslateMessage, UnregisterClassW,
    WM_MOUSEMOVE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows_core::{BOOL, Error, HRESULT, Interface, Ref, Result as WindowsResult, implement, w};

use super::data_object::{SourceFactory, VirtualFileDataObject, VirtualFileEntry, format_etc};
use super::local_file::{CaptureError, FileTreeSnapshot, LocalFileOffer, LocalOfferRegistry};
use super::probe::ProbeState;
use super::secure_transfer::{SecureOfferClient, SecureRemoteSource};
use super::source::ReadAtSource;
use super::{catch_com_hresult, catch_com_result};

const SOURCE_WINDOW_CLASS: windows_core::PCWSTR = w!("PierdeskD0BoundaryDropTarget");
const SOURCE_TIMER_ID: usize = 0x4346_4430;
const SOURCE_TIMER_MS: u32 = 50;
const CORNER_GUARD_PIXELS: i32 = 32;
const MAX_SOURCE_THICKNESS: u32 = 128;
const DRAG_IMAGE_EDGE: usize = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DragProbeEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl DragProbeEdge {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SourceDragProbeOptions {
    pub edge: DragProbeEdge,
    pub thickness: u32,
    pub lifetime: Duration,
    pub settle_delay: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SourceDragProbeReport {
    pub event: &'static str,
    pub hdrop_available: bool,
    pub file_descriptor_available: bool,
    pub item_count: usize,
    pub all_paths_absolute: bool,
    pub held_object_still_readable: bool,
    pub clipboard_unchanged: bool,
}

#[derive(Clone, Debug)]
pub struct TargetDragProbeOptions {
    pub file: PathBuf,
    pub lifetime: Duration,
    pub auto_cancel: bool,
}

#[derive(Clone)]
pub struct RemoteTargetDragProbeOptions {
    pub client: SecureOfferClient,
    pub lifetime: Duration,
    pub auto_cancel: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetDragProbeReport {
    pub result: &'static str,
    pub effect: u32,
    pub drag_image_initialized: bool,
    pub private_format_writes: usize,
    pub async_operation_finished: bool,
    pub content_read_calls: u64,
    pub content_bytes_read: u64,
    pub clipboard_unchanged: bool,
}

struct OleApartment;

impl OleApartment {
    fn initialize() -> WindowsResult<Self> {
        unsafe { OleInitialize(None) }?;
        Ok(Self)
    }
}

impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceEvent {
    Entered,
    Left,
    Dropped,
}

impl SourceEvent {
    const fn label(self) -> &'static str {
        match self {
            Self::Entered => "drag-enter",
            Self::Left => "drag-leave",
            Self::Dropped => "drop",
        }
    }

    const fn terminal(self) -> bool {
        matches!(self, Self::Left | Self::Dropped)
    }
}

#[derive(Default)]
struct BoundaryDropState {
    held: Mutex<Option<IDataObject>>,
    events: Mutex<VecDeque<SourceEvent>>,
    key_states: Mutex<Vec<u32>>,
    accepts_hdrop: AtomicBool,
}

impl BoundaryDropState {
    fn record(&self, event: SourceEvent, object: Option<IDataObject>, accepts_hdrop: bool) {
        if let Some(object) = object
            && let Ok(mut held) = self.held.lock()
        {
            *held = Some(object);
        }
        self.accepts_hdrop.store(accepts_hdrop, Ordering::Release);
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    fn next_event(&self) -> Option<SourceEvent> {
        self.events.lock().ok()?.pop_front()
    }

    fn object(&self) -> Option<IDataObject> {
        self.held.lock().ok()?.clone()
    }

    fn record_key_state(&self, key_state: MODIFIERKEYS_FLAGS) {
        if let Ok(mut key_states) = self.key_states.lock() {
            key_states.push(key_state.0);
        }
    }
}

#[implement(IDropTarget)]
struct BoundaryDropTarget {
    state: Arc<BoundaryDropState>,
}

impl IDropTarget_Impl for BoundaryDropTarget_Impl {
    fn DragEnter(
        &self,
        data_object: Ref<IDataObject>,
        key_state: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WindowsResult<()> {
        catch_com_result(|| {
            self.state.record_key_state(key_state);
            if effect.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            let object = data_object
                .cloned()
                .ok_or_else(|| Error::from_hresult(E_POINTER))?;
            let accepts = supports_hdrop(&object);
            let offered = unsafe { effect.read() };
            unsafe {
                effect.write(if accepts && offered.0 & DROPEFFECT_COPY.0 != 0 {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                });
            }
            self.state
                .record(SourceEvent::Entered, Some(object), accepts);
            Ok(())
        })
    }

    fn DragOver(
        &self,
        key_state: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WindowsResult<()> {
        catch_com_result(|| {
            self.state.record_key_state(key_state);
            if effect.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            let offered = unsafe { effect.read() };
            let accepts = self.state.accepts_hdrop.load(Ordering::Acquire);
            unsafe {
                effect.write(if accepts && offered.0 & DROPEFFECT_COPY.0 != 0 {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                });
            }
            Ok(())
        })
    }

    fn DragLeave(&self) -> WindowsResult<()> {
        catch_com_result(|| {
            self.state.record(SourceEvent::Left, None, false);
            Ok(())
        })
    }

    fn Drop(
        &self,
        data_object: Ref<IDataObject>,
        key_state: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WindowsResult<()> {
        catch_com_result(|| {
            self.state.record_key_state(key_state);
            if effect.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            let object = data_object
                .cloned()
                .ok_or_else(|| Error::from_hresult(E_POINTER))?;
            let accepts = supports_hdrop(&object);
            let offered = unsafe { effect.read() };
            unsafe {
                effect.write(if accepts && offered.0 & DROPEFFECT_COPY.0 != 0 {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                });
            }
            self.state
                .record(SourceEvent::Dropped, Some(object), accepts);
            Ok(())
        })
    }
}

pub(super) struct RegisteredBoundaryWindow {
    pub(super) handle: HWND,
    instance: HINSTANCE,
    registered: bool,
}

impl RegisteredBoundaryWindow {
    pub(super) fn create(
        edge: DragProbeEdge,
        thickness: u32,
        target: &IDropTarget,
    ) -> io::Result<Self> {
        Self::create_with_visibility(edge, thickness, target, true)
    }

    pub(super) fn create_with_visibility(
        edge: DragProbeEdge,
        thickness: u32,
        target: &IDropTarget,
        visible: bool,
    ) -> io::Result<Self> {
        let module = unsafe { GetModuleHandleW(None) }.map_err(io::Error::other)?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(source_window_procedure),
            hInstance: instance,
            lpszClassName: SOURCE_WINDOW_CLASS,
            ..WNDCLASSW::default()
        };
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let thickness = i32::try_from(thickness).map_err(io::Error::other)?;
        let (x, y, width, height) = boundary_window_bounds(
            edge,
            screen_width,
            screen_height,
            thickness,
            CORNER_GUARD_PIXELS,
        )?;
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                SOURCE_WINDOW_CLASS,
                w!("Pierdesk D0 Boundary Drop Target"),
                WS_POPUP,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|error| {
            let _ = unsafe { UnregisterClassW(SOURCE_WINDOW_CLASS, Some(instance)) };
            io::Error::other(error)
        })?;
        if let Err(error) = unsafe {
            SetLayeredWindowAttributes(
                handle,
                COLORREF(0),
                if visible { 112 } else { 1 },
                LWA_ALPHA,
            )
        } {
            let _ = unsafe { DestroyWindow(handle) };
            let _ = unsafe { UnregisterClassW(SOURCE_WINDOW_CLASS, Some(instance)) };
            return Err(io::Error::other(error));
        }
        if let Err(error) = unsafe { RegisterDragDrop(handle, target) } {
            let _ = unsafe { DestroyWindow(handle) };
            let _ = unsafe { UnregisterClassW(SOURCE_WINDOW_CLASS, Some(instance)) };
            return Err(io::Error::other(error));
        }
        if visible {
            let _ = unsafe { ShowWindow(handle, SW_SHOWNOACTIVATE) };
        }
        if unsafe { SetTimer(Some(handle), SOURCE_TIMER_ID, SOURCE_TIMER_MS, None) } == 0 {
            let _ = unsafe { RevokeDragDrop(handle) };
            let _ = unsafe { DestroyWindow(handle) };
            let _ = unsafe { UnregisterClassW(SOURCE_WINDOW_CLASS, Some(instance)) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle,
            instance,
            registered: true,
        })
    }
}

impl Drop for RegisteredBoundaryWindow {
    fn drop(&mut self) {
        let _ = unsafe { KillTimer(Some(self.handle), SOURCE_TIMER_ID) };
        if self.registered {
            let _ = unsafe { RevokeDragDrop(self.handle) };
        }
        let _ = unsafe { DestroyWindow(self.handle) };
        let _ = unsafe { UnregisterClassW(SOURCE_WINDOW_CLASS, Some(self.instance)) };
    }
}

unsafe extern "system" fn source_window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn boundary_window_bounds(
    edge: DragProbeEdge,
    screen_width: i32,
    screen_height: i32,
    thickness: i32,
    corner_guard: i32,
) -> io::Result<(i32, i32, i32, i32)> {
    if screen_width <= corner_guard.saturating_mul(2)
        || screen_height <= corner_guard.saturating_mul(2)
        || thickness <= 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid primary-screen geometry for drag probe",
        ));
    }
    Ok(match edge {
        DragProbeEdge::Left => (0, corner_guard, thickness, screen_height - corner_guard * 2),
        DragProbeEdge::Right => (
            screen_width - thickness,
            corner_guard,
            thickness,
            screen_height - corner_guard * 2,
        ),
        DragProbeEdge::Top => (corner_guard, 0, screen_width - corner_guard * 2, thickness),
        DragProbeEdge::Bottom => (
            corner_guard,
            screen_height - thickness,
            screen_width - corner_guard * 2,
            thickness,
        ),
    })
}

/// Runs the D0 source-side OLE target probe on the calling STA.
///
/// # Errors
///
/// Returns an error for invalid limits, OLE/window registration failures, malformed source data,
/// a message-loop failure, or when the probe times out before a terminal drag event.
pub fn run_source_drag_probe(options: SourceDragProbeOptions) -> io::Result<SourceDragProbeReport> {
    if options.thickness == 0
        || options.thickness > MAX_SOURCE_THICKNESS
        || options.lifetime.is_zero()
        || options.settle_delay.is_zero()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid source drag probe limits",
        ));
    }
    let _ole = OleApartment::initialize().map_err(io::Error::other)?;
    let clipboard_before = unsafe { GetClipboardSequenceNumber() };
    let state = Arc::new(BoundaryDropState::default());
    let target: IDropTarget = BoundaryDropTarget {
        state: Arc::clone(&state),
    }
    .into();
    let window = RegisteredBoundaryWindow::create(options.edge, options.thickness, &target)?;
    println!(
        "FILE_DRAG_SOURCE_PROBE ready=true edge={} thickness={} lifetime_ms={} clipboard_sequence={} window={:#x}",
        options.edge.label(),
        options.thickness,
        options.lifetime.as_millis(),
        clipboard_before,
        window.handle.0 as usize,
    );

    let started = Instant::now();
    let mut terminal = None;
    let mut first_inspection = None;
    loop {
        let mut message = MSG::default();
        let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        if result == 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
        while let Some(event) = state.next_event() {
            println!("FILE_DRAG_SOURCE_PROBE event={}", event.label());
            if event.terminal() && terminal.is_none() {
                let object = state.object().ok_or_else(|| {
                    io::Error::other("drop target did not retain the source IDataObject")
                })?;
                first_inspection = Some(inspect_source_object(&object)?);
                terminal = Some((event, Instant::now()));
            }
        }
        if let Some((event, terminal_at)) = terminal
            && terminal_at.elapsed() >= options.settle_delay
        {
            let object = state.object().ok_or_else(|| {
                io::Error::other("retained source IDataObject disappeared before settle check")
            })?;
            let settled = inspect_source_object(&object)?;
            let first = first_inspection
                .take()
                .ok_or_else(|| io::Error::other("source object was not inspected"))?;
            let clipboard_after = unsafe { GetClipboardSequenceNumber() };
            return Ok(SourceDragProbeReport {
                event: event.label(),
                hdrop_available: first.hdrop_available,
                file_descriptor_available: first.file_descriptor_available,
                item_count: first.item_count,
                all_paths_absolute: first.all_paths_absolute,
                held_object_still_readable: settled.hdrop_available
                    && settled.item_count == first.item_count
                    && settled.all_paths_absolute,
                clipboard_unchanged: clipboard_before == clipboard_after,
            });
        }
        if started.elapsed() >= options.lifetime {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "source drag probe timed out before a drop or drag cancellation",
            ));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "source drag probe message loop ended",
    ))
}

#[derive(Clone, Copy, Debug)]
struct SourceObjectInspection {
    hdrop_available: bool,
    file_descriptor_available: bool,
    item_count: usize,
    all_paths_absolute: bool,
}

fn inspect_source_object(object: &IDataObject) -> io::Result<SourceObjectInspection> {
    let hdrop_available = supports_hdrop(object);
    let descriptor_format = unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) };
    let file_descriptor_available = descriptor_format != 0
        && query_hglobal_format(object, u16::try_from(descriptor_format).unwrap_or_default());
    if !hdrop_available {
        return Ok(SourceObjectInspection {
            hdrop_available,
            file_descriptor_available,
            item_count: 0,
            all_paths_absolute: false,
        });
    }
    let format = format_etc(
        windows::Win32::System::Ole::CF_HDROP.0,
        -1,
        TYMED_HGLOBAL.0.cast_unsigned(),
    );
    let medium = unsafe { object.GetData(&raw const format) }.map_err(io::Error::other)?;
    let guard = StgMediumGuard(medium);
    if guard.0.tymed != TYMED_HGLOBAL.0.cast_unsigned() {
        return Err(io::Error::other(
            "CF_HDROP was not returned as TYMED_HGLOBAL",
        ));
    }
    let drop = HDROP(unsafe { guard.0.u.hGlobal }.0);
    let count = usize::try_from(unsafe { DragQueryFileW(drop, u32::MAX, None) })
        .map_err(io::Error::other)?;
    let mut all_absolute = count != 0;
    for index in 0..count {
        let index = u32::try_from(index).map_err(io::Error::other)?;
        let length = usize::try_from(unsafe { DragQueryFileW(drop, index, None) })
            .map_err(io::Error::other)?;
        if length == 0 {
            all_absolute = false;
            continue;
        }
        let mut buffer = vec![0_u16; length + 1];
        let copied = usize::try_from(unsafe { DragQueryFileW(drop, index, Some(&mut buffer)) })
            .map_err(io::Error::other)?;
        let path = PathBuf::from(String::from_utf16_lossy(&buffer[..copied]));
        all_absolute &= copied == length && path.is_absolute();
    }
    Ok(SourceObjectInspection {
        hdrop_available,
        file_descriptor_available,
        item_count: count,
        all_paths_absolute: all_absolute,
    })
}

fn supports_hdrop(object: &IDataObject) -> bool {
    query_hglobal_format(object, windows::Win32::System::Ole::CF_HDROP.0)
}

fn query_hglobal_format(object: &IDataObject, format: u16) -> bool {
    let format = format_etc(format, -1, TYMED_HGLOBAL.0.cast_unsigned());
    (unsafe { object.QueryGetData(&raw const format) }) == S_OK
}

struct StgMediumGuard(STGMEDIUM);

impl Drop for StgMediumGuard {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&raw mut self.0) };
    }
}

#[implement(IDropSource)]
struct ProbeDropSource {
    armed: AtomicBool,
    started_at: Instant,
    lifetime: Duration,
    auto_cancel: bool,
}

impl IDropSource_Impl for ProbeDropSource_Impl {
    fn QueryContinueDrag(&self, escape_pressed: BOOL, key_state: MODIFIERKEYS_FLAGS) -> HRESULT {
        catch_com_hresult(|| {
            if self.auto_cancel
                || escape_pressed.as_bool()
                || self.started_at.elapsed() >= self.lifetime
            {
                return DRAGDROP_S_CANCEL;
            }
            if key_state.0 & MK_LBUTTON.0 != 0 {
                self.armed.store(true, Ordering::Release);
                return S_OK;
            }
            if self.armed.load(Ordering::Acquire) {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        })
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Starts a target-side native OLE drag with a virtual file object and no clipboard publication.
///
/// # Errors
///
/// Returns an error for invalid input, OLE/data-object construction failures, drag-image setup
/// failures, or an unexpected result from the native drag loop.
pub fn run_target_drag_probe(
    options: &TargetDragProbeOptions,
) -> io::Result<TargetDragProbeReport> {
    if options.lifetime.is_zero() || !options.file.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target drag probe requires an absolute file and non-zero lifetime",
        ));
    }
    let _ole = OleApartment::initialize().map_err(io::Error::other)?;
    let (object, probe, offer) = create_local_virtual_object(&options.file)?;
    run_prepared_target_drag(
        &object,
        &probe,
        options.lifetime,
        options.auto_cancel,
        offer.total_size(),
        &|| offer.read_calls(),
        &|| offer.bytes_read(),
    )
}

/// Starts a target-side native OLE drag backed by the existing authenticated remote range source.
///
/// # Errors
///
/// Returns an error for manifest/TLS validation failures, a non-single-file offer, COM setup
/// failures, or an unexpected result from the native drag loop.
pub fn run_remote_target_drag_probe(
    options: &RemoteTargetDragProbeOptions,
) -> io::Result<TargetDragProbeReport> {
    if options.lifetime.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote target drag probe requires a non-zero lifetime",
        ));
    }
    let manifest = options.client.fetch_manifest()?;
    if manifest.entries.len() != 1 || manifest.entries[0].descriptor.is_directory() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "D0 remote target drag probe requires exactly one ordinary file",
        ));
    }
    let entry = manifest.entries[0].clone();
    let total_size = entry.descriptor.size;
    let remote = Arc::new(SecureRemoteSource::new_for_entry(
        options.client.clone(),
        manifest.clone(),
        entry.clone(),
    ));
    let remote_for_factory = Arc::clone(&remote);
    let source_factory: SourceFactory = Arc::new(move || {
        let source: Arc<dyn ReadAtSource> = remote_for_factory.clone();
        source
    });
    let probe = Arc::new(ProbeState::default());
    let object = VirtualFileDataObject::create_with_entries(
        vec![VirtualFileEntry::file(entry.descriptor, source_factory)],
        Arc::clone(&probe),
        manifest.origin_payload(),
    )
    .map_err(io::Error::other)?;
    let _ole = OleApartment::initialize().map_err(io::Error::other)?;
    let async_capability: IDataObjectAsyncCapability = object.cast().map_err(io::Error::other)?;
    unsafe { async_capability.SetAsyncMode(true) }.map_err(io::Error::other)?;
    run_prepared_target_drag(
        &object,
        &probe,
        options.lifetime,
        options.auto_cancel,
        total_size,
        &|| remote.read_calls(),
        &|| remote.bytes_read(),
    )
}

fn run_prepared_target_drag(
    object: &IDataObject,
    probe: &Arc<ProbeState>,
    lifetime: Duration,
    auto_cancel: bool,
    total_size: u64,
    read_calls: &dyn Fn() -> u64,
    bytes_read: &dyn Fn() -> u64,
) -> io::Result<TargetDragProbeReport> {
    let clipboard_before = unsafe { GetClipboardSequenceNumber() };
    let _drag_image = initialize_drag_image(object)?;
    let drag_image_initialized = true;
    let source: IDropSource = ProbeDropSource {
        armed: AtomicBool::new(false),
        started_at: Instant::now(),
        lifetime,
        auto_cancel,
    }
    .into();
    println!(
        "FILE_DRAG_TARGET_PROBE ready=true file_count=1 lifetime_ms={} clipboard_sequence={} drag_image={}",
        lifetime.as_millis(),
        clipboard_before,
        drag_image_initialized,
    );
    let auto_cancel_done = Arc::new(AtomicBool::new(false));
    let auto_cancel_wake = auto_cancel.then(|| {
        // DoDragDrop asks QueryContinueDrag in response to input messages rather than on a timer.
        // Wake only this STA's queue so the non-interactive probe can exercise its cancel branch;
        // this does not alter the system cursor or synthesize global keyboard/mouse input.
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut message = MSG::default();
        unsafe {
            let _ = PeekMessageW(&raw mut message, None, 0, 0, PM_NOREMOVE);
        }
        let done = Arc::clone(&auto_cancel_done);
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(25));
                let _ =
                    unsafe { PostThreadMessageW(thread_id, WM_MOUSEMOVE, WPARAM(0), LPARAM(0)) };
            }
        })
    });
    let drag_started = Instant::now();
    let mut effect = DROPEFFECT_NONE;
    let result = unsafe { DoDragDrop(object, &source, DROPEFFECT_COPY, &raw mut effect) };
    auto_cancel_done.store(true, Ordering::Release);
    if let Some(wake) = auto_cancel_wake {
        let _ = wake.join();
    }
    let result_label = if result == DRAGDROP_S_DROP {
        "drop"
    } else if result == DRAGDROP_S_CANCEL {
        "cancel"
    } else {
        result.ok().map_err(io::Error::other)?;
        "completed"
    };
    let async_operation_finished = if result == DRAGDROP_S_DROP && effect == DROPEFFECT_COPY {
        wait_for_target_consumption(
            probe,
            total_size,
            bytes_read,
            lifetime.saturating_sub(drag_started.elapsed()),
        )?
    } else {
        true
    };
    let clipboard_after = unsafe { GetClipboardSequenceNumber() };
    let events = probe.events();
    Ok(TargetDragProbeReport {
        result: result_label,
        effect: effect.0,
        drag_image_initialized,
        private_format_writes: events
            .iter()
            .filter(|event| event.contains("private_format="))
            .count(),
        async_operation_finished,
        content_read_calls: read_calls(),
        content_bytes_read: bytes_read(),
        clipboard_unchanged: clipboard_before == clipboard_after,
    })
}

fn wait_for_target_consumption(
    probe: &ProbeState,
    total_size: u64,
    bytes_read: &dyn Fn() -> u64,
    timeout: Duration,
) -> io::Result<bool> {
    let started = Instant::now();
    let mut content_complete_at = None;
    loop {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
        let events = probe.events();
        let async_started = events
            .iter()
            .any(|event| event.contains("IDataObjectAsyncCapability::StartOperation"));
        let async_finished = !async_started
            || events
                .iter()
                .any(|event| event.contains("IDataObjectAsyncCapability::EndOperation"));
        if bytes_read() >= total_size {
            let complete_at = content_complete_at.get_or_insert_with(Instant::now);
            if async_finished || complete_at.elapsed() >= Duration::from_millis(500) {
                return Ok(async_finished);
            }
        }
        if started.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn create_local_virtual_object(
    path: &PathBuf,
) -> io::Result<(IDataObject, Arc<ProbeState>, Arc<LocalFileOffer>)> {
    let tree = FileTreeSnapshot::capture(std::slice::from_ref(path)).map_err(capture_error)?;
    let mut registry = LocalOfferRegistry::default();
    let offer = registry
        .publish_tree(&tree, Duration::from_mins(5))
        .map_err(io::Error::other)?;
    if offer.file_count() != 1 || offer.directory_count() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "D0 target drag probe accepts exactly one ordinary file",
        ));
    }
    let entries = offer
        .entries()
        .iter()
        .map(|entry| {
            let descriptor = entry.descriptor();
            let source = entry
                .source()
                .expect("the D0 target probe has one ordinary file");
            let source_factory: SourceFactory = Arc::new(move || {
                let source: Arc<dyn ReadAtSource> = source.clone();
                source
            });
            VirtualFileEntry::file(descriptor, source_factory)
        })
        .collect();
    let probe = Arc::new(ProbeState::default());
    let object = VirtualFileDataObject::create_with_entries(
        entries,
        Arc::clone(&probe),
        offer.origin_payload(),
    )
    .map_err(io::Error::other)?;
    let async_capability: IDataObjectAsyncCapability = object.cast().map_err(io::Error::other)?;
    unsafe { async_capability.SetAsyncMode(true) }.map_err(io::Error::other)?;
    Ok((object, probe, offer))
}

fn capture_error(error: CaptureError) -> io::Error {
    match error {
        CaptureError::Windows(error) => io::Error::other(error),
        CaptureError::Rejected(reason) => {
            io::Error::new(io::ErrorKind::Unsupported, reason.to_string())
        }
    }
}

pub(super) struct DragImage(HBITMAP);
impl Drop for DragImage {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(self.0.into()) };
    }
}

pub(super) fn initialize_drag_image(object: &IDataObject) -> io::Result<IDragSourceHelper> {
    let bitmap = DragImage(create_drag_bitmap()?);
    let image = SHDRAGIMAGE {
        sizeDragImage: SIZE {
            cx: i32::try_from(DRAG_IMAGE_EDGE).unwrap_or(48),
            cy: i32::try_from(DRAG_IMAGE_EDGE).unwrap_or(48),
        },
        ptOffset: POINT { x: 12, y: 12 },
        hbmpDragImage: bitmap.0,
        crColorKey: COLORREF(0),
    };
    let helper: IDragSourceHelper =
        unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }
            .map_err(io::Error::other)?;
    unsafe { helper.InitializeFromBitmap(&raw const image, object) }.map_err(io::Error::other)?;
    // The Shell owns the bitmap after successful initialization.
    std::mem::forget(bitmap);
    Ok(helper)
}

fn create_drag_bitmap() -> io::Result<windows::Win32::Graphics::Gdi::HBITMAP> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(u32::MAX),
            biWidth: i32::try_from(DRAG_IMAGE_EDGE).unwrap_or(48),
            biHeight: -i32::try_from(DRAG_IMAGE_EDGE).unwrap_or(48),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: u32::try_from(DRAG_IMAGE_EDGE * DRAG_IMAGE_EDGE * 4).unwrap_or(u32::MAX),
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut bits = std::ptr::null_mut::<c_void>();
    let bitmap = unsafe {
        CreateDIBSection(
            None,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
    }
    .map_err(io::Error::other)?;
    if bits.is_null() {
        let _ = unsafe { DeleteObject(bitmap.into()) };
        return Err(io::Error::other("drag bitmap has no pixel storage"));
    }
    let pixels = unsafe {
        std::slice::from_raw_parts_mut(bits.cast::<u32>(), DRAG_IMAGE_EDGE * DRAG_IMAGE_EDGE)
    };
    pixels.fill(0);
    for y in 4..44 {
        for x in 7..39 {
            let border = x == 7 || x == 38 || y == 4 || y == 43;
            let fold = x >= 30 && y <= 12;
            let pixel = if border {
                0xe0_2d_73_d9
            } else if fold {
                0xe0_b9_d2_f4
            } else {
                0xe0_ff_ff_ff
            };
            pixels[y * DRAG_IMAGE_EDGE + x] = pixel;
        }
    }
    Ok(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetKeyboardState, SetKeyboardState, VK_LBUTTON,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    struct ProbeInputStateGuard {
        cursor: POINT,
        keyboard: [u8; 256],
    }

    impl ProbeInputStateGuard {
        fn capture() -> io::Result<Self> {
            let mut cursor = POINT::default();
            unsafe { GetCursorPos(&raw mut cursor) }.map_err(io::Error::other)?;
            let mut keyboard = [0_u8; 256];
            unsafe { GetKeyboardState(&mut keyboard) }.map_err(io::Error::other)?;
            Ok(Self { cursor, keyboard })
        }
    }

    impl Drop for ProbeInputStateGuard {
        fn drop(&mut self) {
            let _ = unsafe { SetKeyboardState(&self.keyboard) };
            let _ = unsafe { SetCursorPos(self.cursor.x, self.cursor.y) };
        }
    }

    #[implement(IDropSource)]
    struct TimedCancelDropSource {
        started_at: Instant,
        lifetime: Duration,
    }

    impl IDropSource_Impl for TimedCancelDropSource_Impl {
        fn QueryContinueDrag(
            &self,
            _escape_pressed: BOOL,
            _key_state: MODIFIERKEYS_FLAGS,
        ) -> HRESULT {
            if self.started_at.elapsed() >= self.lifetime {
                DRAGDROP_S_CANCEL
            } else {
                S_OK
            }
        }

        fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    #[test]
    fn boundary_probe_bounds_keep_all_four_edges_corner_safe() {
        assert_eq!(
            boundary_window_bounds(DragProbeEdge::Left, 1920, 1080, 8, 32).unwrap(),
            (0, 32, 8, 1016)
        );
        assert_eq!(
            boundary_window_bounds(DragProbeEdge::Right, 1920, 1080, 8, 32).unwrap(),
            (1912, 32, 8, 1016)
        );
        assert_eq!(
            boundary_window_bounds(DragProbeEdge::Top, 1920, 1080, 8, 32).unwrap(),
            (32, 0, 1856, 8)
        );
        assert_eq!(
            boundary_window_bounds(DragProbeEdge::Bottom, 1920, 1080, 8, 32).unwrap(),
            (32, 1072, 1856, 8)
        );
    }

    #[test]
    fn invalid_probe_geometry_is_rejected() {
        assert!(boundary_window_bounds(DragProbeEdge::Left, 64, 64, 8, 32).is_err());
        assert!(boundary_window_bounds(DragProbeEdge::Left, 1920, 1080, 0, 32).is_err());
    }

    #[test]
    #[ignore = "moves the cursor briefly to prove how Windows OLE observes thread-local button state"]
    fn ole_target_observes_thread_local_left_button_without_global_click() {
        let _ole = OleApartment::initialize().unwrap();
        let guard = ProbeInputStateGuard::capture().unwrap();
        let state = Arc::new(BoundaryDropState::default());
        let target: IDropTarget = BoundaryDropTarget {
            state: Arc::clone(&state),
        }
        .into();
        let _window = RegisteredBoundaryWindow::create(DragProbeEdge::Top, 128, &target).unwrap();
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        unsafe { SetCursorPos(width / 2, 64) }.unwrap();

        let mut virtual_keyboard = guard.keyboard;
        virtual_keyboard[usize::from(VK_LBUTTON.0)] |= 0x80;
        unsafe { SetKeyboardState(&virtual_keyboard) }.unwrap();
        let async_left_after_thread_press = unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) };

        let path = std::env::current_exe().unwrap();
        let (object, _probe, _offer) = create_local_virtual_object(&path).unwrap();
        let source: IDropSource = TimedCancelDropSource {
            started_at: Instant::now(),
            lifetime: Duration::from_millis(250),
        }
        .into();
        let thread_id = unsafe { GetCurrentThreadId() };
        let done = Arc::new(AtomicBool::new(false));
        let wake_done = Arc::clone(&done);
        let wake = std::thread::spawn(move || {
            while !wake_done.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
                let _ =
                    unsafe { PostThreadMessageW(thread_id, WM_MOUSEMOVE, WPARAM(0), LPARAM(0)) };
            }
        });
        let mut effect = DROPEFFECT_NONE;
        let result = unsafe { DoDragDrop(&object, &source, DROPEFFECT_COPY, &raw mut effect) };
        done.store(true, Ordering::Release);
        wake.join().unwrap();

        let key_states = state.key_states.lock().unwrap().clone();
        println!(
            "OLE_THREAD_KEYSTATE_PROBE result={:#x} effect={} callbacks={} states={key_states:?} async_left_after_thread_press={async_left_after_thread_press:#x}",
            result.0,
            effect.0,
            key_states.len(),
        );
        assert_eq!(result, DRAGDROP_S_CANCEL);
        assert!(!key_states.is_empty());
        assert!(
            key_states
                .iter()
                .all(|key_state| key_state & MK_LBUTTON.0 != 0)
        );
        assert_eq!(async_left_after_thread_press & i16::MIN, 0);
    }
}
