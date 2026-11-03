use std::collections::VecDeque;
use std::io;
use std::mem::size_of;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, GetThreadDpiAwarenessContext,
    IsValidDpiAwarenessContext, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE_NOCOALESCE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageTime, GetMessageW, GetSystemMetrics, HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT,
    LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, UnregisterClassW, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDBLCLK, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
    XBUTTON1, XBUTTON2,
};
use windows_core::{PCWSTR, w};

use super::state::{
    InputEvent, InputEventStats, InputLedger, InputStateMachine, KeyTransition, LedgerResult,
    MouseButton, PhysicalKey, StateDecision, WheelAxis,
};

const INPUT_QUEUE_CAPACITY: usize = 256;
const MAX_DIAGNOSTIC_LIFETIME: Duration = Duration::from_mins(5);
const RAW_INJECTION_TAG: u32 = 0x4346_4353;
const INJECTION_TAG: usize = RAW_INJECTION_TAG as usize;

static ACTIVE_HOOK_CONTEXT: Mutex<Option<Arc<dyn InputHookHandler>>> = Mutex::new(None);
static RAW_MOTION_NORMALIZER: Mutex<RawMotionNormalizer> = Mutex::new(RawMotionNormalizer::new());
const RAW_MOUSE_WINDOW_CLASS: PCWSTR = w!("PierdeskRawMouseInputWindow");
const RAW_ABSOLUTE_DEVICE_LIMIT: usize = 16;

#[derive(Clone, Copy)]
struct RawAbsolutePosition {
    device: usize,
    x: i32,
    y: i32,
}

struct RawMotionNormalizer {
    absolute_positions: VecDeque<RawAbsolutePosition>,
}

fn is_pierdesk_injected_raw_mouse(extra_information: u32) -> bool {
    extra_information == RAW_INJECTION_TAG
}

impl RawMotionNormalizer {
    const fn new() -> Self {
        Self {
            absolute_positions: VecDeque::new(),
        }
    }

    fn normalize(&mut self, device: usize, absolute: bool, x: i32, y: i32) -> Option<(i32, i32)> {
        if !absolute {
            return (x != 0 || y != 0).then_some((x, y));
        }
        if let Some(previous) = self
            .absolute_positions
            .iter_mut()
            .find(|position| position.device == device)
        {
            let delta = (x.saturating_sub(previous.x), y.saturating_sub(previous.y));
            previous.x = x;
            previous.y = y;
            return (delta != (0, 0)).then_some(delta);
        }
        if self.absolute_positions.len() == RAW_ABSOLUTE_DEVICE_LIMIT {
            self.absolute_positions.pop_front();
        }
        self.absolute_positions
            .push_back(RawAbsolutePosition { device, x, y });
        None
    }

    fn clear(&mut self) {
        self.absolute_positions.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputDiagnosticOptions {
    pub lifetime: Duration,
}

impl Default for InputDiagnosticOptions {
    fn default() -> Self {
        Self {
            lifetime: Duration::from_secs(15),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticStopReason {
    DurationElapsed,
    EmergencyShortcut,
    ExternalStop,
    QueueFailure,
    InjectionFailure,
    MessageLoopFailure,
}

impl DiagnosticStopReason {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DurationElapsed => "duration_elapsed",
            Self::EmergencyShortcut => "emergency_shortcut",
            Self::ExternalStop => "external_stop",
            Self::QueueFailure => "queue_failure",
            Self::InjectionFailure => "injection_failure",
            Self::MessageLoopFailure => "message_loop_failure",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticDpiMode {
    PerMonitorV2,
    PerMonitorV1,
}

impl DiagnosticDpiMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PerMonitorV2 => "per_monitor_v2",
            Self::PerMonitorV1 => "per_monitor_v1",
        }
    }
}

#[derive(Clone, Debug)]
pub struct InputDiagnosticReport {
    pub stop_reason: DiagnosticStopReason,
    pub captured: InputEventStats,
    pub injected_events: u64,
    pub forced_capture_releases: usize,
    pub forced_injected_releases: usize,
    pub remaining_captured_inputs: usize,
    pub remaining_injected_keys: usize,
    pub remaining_injected_buttons: usize,
    pub queue_capacity: usize,
    pub queue_peak_depth: usize,
    pub coalesced_mouse_moves: u64,
    pub bypassed_mouse_moves: u64,
    pub stale_mouse_moves: u64,
    pub dpi_mode: DiagnosticDpiMode,
    pub effective_dpi: u32,
    pub initial_metrics_width: i32,
    pub initial_metrics_height: i32,
    pub primary_physical_width: i32,
    pub primary_physical_height: i32,
    pub clean_shutdown: bool,
    pub failure: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ScreenExtent {
    pub(super) width: i32,
    pub(super) height: i32,
}

impl ScreenExtent {
    pub(super) fn system_metrics() -> io::Result<Self> {
        // SAFETY: GetSystemMetrics is a read-only process-global query.
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        // SAFETY: GetSystemMetrics is a read-only process-global query.
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        if width <= 1 || height <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "primary display dimensions are unavailable",
            ));
        }
        Ok(Self { width, height })
    }

    pub(super) fn current_primary_display_mode() -> io::Result<Self> {
        let mut mode = DEVMODEW {
            dmSize: u16::try_from(size_of::<DEVMODEW>()).expect("DEVMODEW size fits in u16"),
            ..DEVMODEW::default()
        };
        // SAFETY: the null device selects the primary display and mode owns writable storage of
        // the declared size for the duration of the query.
        if !unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &raw mut mode) }
            .as_bool()
        {
            return Err(io::Error::last_os_error());
        }
        let width = i32::try_from(mode.dmPelsWidth)
            .map_err(|_| io::Error::other("primary display width exceeds i32"))?;
        let height = i32::try_from(mode.dmPelsHeight)
            .map_err(|_| io::Error::other("primary display height exceeds i32"))?;
        if width <= 1 || height <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "primary display mode dimensions are unavailable",
            ));
        }
        Ok(Self { width, height })
    }

    pub(super) fn current_primary_refresh_hz() -> io::Result<u32> {
        let mut mode = DEVMODEW {
            dmSize: u16::try_from(size_of::<DEVMODEW>()).expect("DEVMODEW size fits in u16"),
            ..DEVMODEW::default()
        };
        // SAFETY: the null device selects the primary display and mode owns writable storage of
        // the declared size for the duration of the query.
        if !unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &raw mut mode) }
            .as_bool()
        {
            return Err(io::Error::last_os_error());
        }
        if mode.dmDisplayFrequency <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "primary display refresh rate is unavailable",
            ));
        }
        Ok(mode.dmDisplayFrequency)
    }

    pub(super) const fn contains(self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.width && y < self.height
    }

    pub(super) fn effective_dpi() -> io::Result<u32> {
        // SAFETY: GetDpiForSystem is a read-only query interpreted under this thread's awareness.
        let dpi = unsafe { GetDpiForSystem() };
        if dpi == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(dpi)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScreenGeometry {
    physical: ScreenExtent,
    initial_metrics: ScreenExtent,
    effective_dpi: u32,
    dpi_mode: DiagnosticDpiMode,
}

impl ScreenGeometry {
    fn primary(initial_metrics: ScreenExtent, dpi_mode: DiagnosticDpiMode) -> io::Result<Self> {
        let physical = ScreenExtent::system_metrics()?;
        let display_mode = ScreenExtent::current_primary_display_mode()?;
        validate_physical_extents(physical, display_mode)?;
        // SAFETY: the hook thread is currently in a per-monitor-aware context, so this query
        // returns the effective system DPI instead of the DPI-unaware compatibility value.
        let effective_dpi = unsafe { GetDpiForSystem() };
        if effective_dpi == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            physical,
            initial_metrics,
            effective_dpi,
            dpi_mode,
        })
    }
}

fn validate_physical_extents(metrics: ScreenExtent, display_mode: ScreenExtent) -> io::Result<()> {
    if metrics == display_mode {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "primary physical screen mismatch: per-monitor metrics={}x{}, display mode={}x{}",
                metrics.width, metrics.height, display_mode.width, display_mode.height
            ),
        ))
    }
}

pub(crate) struct ThreadDpiAwarenessGuard {
    previous: DPI_AWARENESS_CONTEXT,
    mode: DiagnosticDpiMode,
}

impl ThreadDpiAwarenessGuard {
    pub(crate) fn enter_per_monitor() -> io::Result<Self> {
        for (target, mode) in [
            (
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                DiagnosticDpiMode::PerMonitorV2,
            ),
            (
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
                DiagnosticDpiMode::PerMonitorV1,
            ),
        ] {
            // SAFETY: these are documented predefined awareness-context handles.
            if !unsafe { IsValidDpiAwarenessContext(target) }.as_bool() {
                continue;
            }
            // SAFETY: this changes only the current CLI/hook thread and returns its prior context.
            let previous = unsafe { SetThreadDpiAwarenessContext(target) };
            if previous.0.is_null() {
                continue;
            }
            // SAFETY: both calls query the current thread and compare predefined opaque handles.
            let applied = unsafe {
                AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), target).as_bool()
            };
            if applied {
                return Ok(Self { previous, mode });
            }
            // SAFETY: previous was returned by the successful context switch above.
            let _ = unsafe { SetThreadDpiAwarenessContext(previous) };
        }
        Err(io::Error::other(
            "Windows did not provide a usable per-monitor DPI awareness context",
        ))
    }
}

impl Drop for ThreadDpiAwarenessGuard {
    fn drop(&mut self) {
        // SAFETY: previous belongs to this same thread and was returned by Windows when the guard
        // entered its temporary per-monitor context.
        let _ = unsafe { SetThreadDpiAwarenessContext(self.previous) };
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopCode {
    Running = 0,
    DurationElapsed = 1,
    EmergencyShortcut = 2,
    ExternalStop = 3,
    QueueFailure = 4,
    InjectionFailure = 5,
    MessageLoopFailure = 6,
}

impl StopCode {
    fn load(value: u8) -> Self {
        match value {
            1 => Self::DurationElapsed,
            2 => Self::EmergencyShortcut,
            3 => Self::ExternalStop,
            4 => Self::QueueFailure,
            5 => Self::InjectionFailure,
            6 => Self::MessageLoopFailure,
            _ => Self::Running,
        }
    }

    const fn report_reason(self) -> Option<DiagnosticStopReason> {
        match self {
            Self::Running => None,
            Self::DurationElapsed => Some(DiagnosticStopReason::DurationElapsed),
            Self::EmergencyShortcut => Some(DiagnosticStopReason::EmergencyShortcut),
            Self::ExternalStop => Some(DiagnosticStopReason::ExternalStop),
            Self::QueueFailure => Some(DiagnosticStopReason::QueueFailure),
            Self::InjectionFailure => Some(DiagnosticStopReason::InjectionFailure),
            Self::MessageLoopFailure => Some(DiagnosticStopReason::MessageLoopFailure),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct QueuedInput {
    event: InputEvent,
    move_generation: u64,
}

#[derive(Debug)]
struct QueueState {
    items: VecDeque<QueuedInput>,
    closed: bool,
    peak_depth: usize,
}

#[derive(Debug)]
struct BoundedInputQueue {
    capacity: usize,
    state: Mutex<QueueState>,
    ready: Condvar,
    coalesced_mouse_moves: AtomicU64,
    bypassed_mouse_moves: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnqueueResult {
    Queued,
    Coalesced,
    MouseMoveBypassed,
    Fatal,
}

impl BoundedInputQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(QueueState {
                items: VecDeque::with_capacity(capacity),
                closed: false,
                peak_depth: 0,
            }),
            ready: Condvar::new(),
            coalesced_mouse_moves: AtomicU64::new(0),
            bypassed_mouse_moves: AtomicU64::new(0),
        }
    }

    fn try_push(&self, queued: QueuedInput) -> EnqueueResult {
        let is_move = queued.event.is_mouse_move();
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) if is_move => {
                self.bypassed_mouse_moves.fetch_add(1, Ordering::Relaxed);
                return EnqueueResult::MouseMoveBypassed;
            }
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                return EnqueueResult::Fatal;
            }
        };
        if state.closed {
            return if is_move {
                self.bypassed_mouse_moves.fetch_add(1, Ordering::Relaxed);
                EnqueueResult::MouseMoveBypassed
            } else {
                EnqueueResult::Fatal
            };
        }
        if is_move
            && let Some(last) = state.items.back_mut()
            && last.event.is_mouse_move()
        {
            *last = queued;
            self.coalesced_mouse_moves.fetch_add(1, Ordering::Relaxed);
            self.ready.notify_one();
            return EnqueueResult::Coalesced;
        }
        if state.items.len() == self.capacity {
            if is_move {
                self.bypassed_mouse_moves.fetch_add(1, Ordering::Relaxed);
                return EnqueueResult::MouseMoveBypassed;
            }
            return EnqueueResult::Fatal;
        }
        state.items.push_back(queued);
        state.peak_depth = state.peak_depth.max(state.items.len());
        self.ready.notify_one();
        EnqueueResult::Queued
    }

    fn pop(&self, stop: &AtomicU8) -> Option<QueuedInput> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if StopCode::load(stop.load(Ordering::Acquire)) != StopCode::Running {
                state.items.clear();
                return None;
            }
            if let Some(event) = state.items.pop_front() {
                return Some(event);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        self.ready.notify_all();
    }

    fn wake(&self) {
        self.ready.notify_all();
    }

    fn peak_depth(&self) -> usize {
        self.state.lock().map_or_else(
            |poisoned| poisoned.into_inner().peak_depth,
            |state| state.peak_depth,
        )
    }
}

#[derive(Debug)]
struct HookContext {
    state_machine: Mutex<InputStateMachine>,
    queue: BoundedInputQueue,
    stop: AtomicU8,
    failure: Mutex<Option<String>>,
    hook_thread_id: u32,
    geometry: ScreenGeometry,
    latest_move_generation: AtomicU64,
}

pub(super) trait InputHookHandler: Send + Sync {
    fn screen_contains(&self, x: i32, y: i32) -> bool;
    fn handle_input(&self, event: InputEvent) -> bool;

    fn handle_mouse_input(&self, event: InputEvent, _message_time_ms: u32) -> bool {
        self.handle_input(event)
    }

    fn handle_raw_mouse(&self, _delta_x: i32, _delta_y: i32, _message_time_ms: u32) {}
}

impl InputHookHandler for HookContext {
    fn screen_contains(&self, x: i32, y: i32) -> bool {
        self.geometry.physical.contains(x, y)
    }

    fn handle_input(&self, event: InputEvent) -> bool {
        self.handle(event)
    }
}

impl HookContext {
    fn new(hook_thread_id: u32, geometry: ScreenGeometry) -> Self {
        Self {
            state_machine: Mutex::new(InputStateMachine::default()),
            queue: BoundedInputQueue::new(INPUT_QUEUE_CAPACITY),
            stop: AtomicU8::new(StopCode::Running as u8),
            failure: Mutex::new(None),
            hook_thread_id,
            geometry,
            latest_move_generation: AtomicU64::new(0),
        }
    }

    fn request_stop(&self, reason: StopCode) {
        if self
            .stop
            .compare_exchange(
                StopCode::Running as u8,
                reason as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.queue.wake();
            // SAFETY: the target id belongs to the live hook message thread; failure is harmless
            // because its next queued event or explicit teardown still observes the stop flag.
            let _ =
                unsafe { PostThreadMessageW(self.hook_thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
    }

    fn fail(&self, reason: StopCode, message: impl Into<String>) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(message.into());
        }
        self.request_stop(reason);
    }

    fn handle(&self, event: InputEvent) -> bool {
        let decision = if let Ok(mut machine) = self.state_machine.lock() {
            machine.handle(event)
        } else {
            self.fail(
                StopCode::QueueFailure,
                "input state machine lock was poisoned",
            );
            return false;
        };
        match decision {
            StateDecision::PassThrough => false,
            StateDecision::EmergencyReturn => {
                self.request_stop(StopCode::EmergencyShortcut);
                true
            }
            StateDecision::CapacityExceeded => {
                self.fail(
                    StopCode::QueueFailure,
                    "input state exceeded the bounded pressed-key capacity",
                );
                false
            }
            StateDecision::Forward => {
                let move_generation = if event.is_mouse_move() {
                    self.latest_move_generation.fetch_add(1, Ordering::AcqRel) + 1
                } else {
                    0
                };
                match self.queue.try_push(QueuedInput {
                    event,
                    move_generation,
                }) {
                    EnqueueResult::Queued | EnqueueResult::Coalesced => true,
                    EnqueueResult::MouseMoveBypassed => false,
                    EnqueueResult::Fatal => {
                        self.fail(
                            StopCode::QueueFailure,
                            "bounded input queue could not accept a reliable event",
                        );
                        false
                    }
                }
            }
        }
    }
}

pub(super) struct ActiveContextGuard {
    context: Arc<dyn InputHookHandler>,
}

impl ActiveContextGuard {
    pub(super) fn register<T>(context: Arc<T>) -> io::Result<Self>
    where
        T: InputHookHandler + 'static,
    {
        let context: Arc<dyn InputHookHandler> = context;
        let mut active = ACTIVE_HOOK_CONTEXT
            .lock()
            .map_err(|_| io::Error::other("input hook registry lock was poisoned"))?;
        if active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an input diagnostic is already active in this process",
            ));
        }
        *active = Some(Arc::clone(&context));
        Ok(Self { context })
    }
}

impl Drop for ActiveContextGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_HOOK_CONTEXT.lock()
            && active
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.context))
        {
            active.take();
        }
    }
}

pub(super) struct HookGuards {
    keyboard: HHOOK,
    mouse: HHOOK,
}

impl HookGuards {
    pub(super) fn install() -> io::Result<Self> {
        // SAFETY: the current executable module remains loaded for the process lifetime.
        let module = unsafe { GetModuleHandleW(None) }.map_err(|error| windows_to_io(&error))?;
        let instance = HINSTANCE(module.0);
        // SAFETY: both callbacks use the system ABI and remain resident in this executable.
        let keyboard =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(instance), 0) }
                .map_err(|error| windows_to_io(&error))?;
        // SAFETY: the callback uses the system ABI and remains resident in this executable.
        let mouse =
            match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(instance), 0) } {
                Ok(mouse) => mouse,
                Err(error) => {
                    // SAFETY: keyboard was installed successfully and is still owned here.
                    let _ = unsafe { UnhookWindowsHookEx(keyboard) };
                    return Err(windows_to_io(&error));
                }
            };
        Ok(Self { keyboard, mouse })
    }
}

impl Drop for HookGuards {
    fn drop(&mut self) {
        // SAFETY: each hook handle is unhooked once during deterministic teardown.
        let _ = unsafe { UnhookWindowsHookEx(self.mouse) };
        // SAFETY: each hook handle is unhooked once during deterministic teardown.
        let _ = unsafe { UnhookWindowsHookEx(self.keyboard) };
    }
}

pub(super) struct RawMouseInputWindow {
    handle: HWND,
    instance: HINSTANCE,
}

impl RawMouseInputWindow {
    pub(super) fn create() -> io::Result<Self> {
        RAW_MOTION_NORMALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        // SAFETY: the current executable module remains loaded for the process lifetime.
        let module = unsafe { GetModuleHandleW(None) }.map_err(|error| windows_to_io(&error))?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(raw_mouse_window_procedure),
            hInstance: instance,
            lpszClassName: RAW_MOUSE_WINDOW_CLASS,
            ..Default::default()
        };
        // SAFETY: the class descriptor and static class name remain valid.
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: this creates a message-only window owned by the current hook thread.
        let handle = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                RAW_MOUSE_WINDOW_CLASS,
                w!("Pierdesk Raw Mouse Input"),
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
        }
        .map_err(|error| {
            // SAFETY: registration succeeded in this function.
            let _ = unsafe { UnregisterClassW(RAW_MOUSE_WINDOW_CLASS, Some(instance)) };
            windows_to_io(&error)
        })?;
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: handle,
        };
        // SAFETY: device is a valid generic-desktop mouse registration and the target window
        // remains alive until Drop unregisters it.
        if let Err(error) = unsafe {
            RegisterRawInputDevices(
                &[device],
                u32::try_from(size_of::<RAWINPUTDEVICE>()).unwrap(),
            )
        } {
            // SAFETY: handle and class are owned by this partially constructed instance.
            let _ = unsafe { DestroyWindow(handle) };
            let _ = unsafe { UnregisterClassW(RAW_MOUSE_WINDOW_CLASS, Some(instance)) };
            return Err(windows_to_io(&error));
        }
        Ok(Self { handle, instance })
    }
}

impl Drop for RawMouseInputWindow {
    fn drop(&mut self) {
        let remove = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        };
        // SAFETY: RIDEV_REMOVE with a null target unregisters this process's mouse sink.
        let _ = unsafe {
            RegisterRawInputDevices(
                &[remove],
                u32::try_from(size_of::<RAWINPUTDEVICE>()).unwrap(),
            )
        };
        // SAFETY: the window and class are owned by this guard and are destroyed once.
        let _ = unsafe { DestroyWindow(self.handle) };
        let _ = unsafe { UnregisterClassW(RAW_MOUSE_WINDOW_CLASS, Some(self.instance)) };
        RAW_MOTION_NORMALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

unsafe extern "system" fn raw_mouse_window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_INPUT {
        let mut input = RAWINPUT::default();
        let mut input_size = u32::try_from(size_of::<RAWINPUT>()).unwrap();
        // SAFETY: WM_INPUT supplies a valid HRAWINPUT for the duration of the call, and input is
        // writable storage of the advertised size.
        let copied = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as *mut core::ffi::c_void),
                RID_INPUT,
                Some((&raw mut input).cast()),
                &raw mut input_size,
                u32::try_from(size_of::<RAWINPUTHEADER>()).unwrap(),
            )
        };
        if copied != u32::MAX
            && copied >= u32::try_from(size_of::<RAWINPUTHEADER>()).unwrap()
            && input.header.dwType == RIM_TYPEMOUSE.0
        {
            // SAFETY: dwType identified the active RAWINPUT union member as RAWMOUSE.
            let mouse = unsafe { input.data.mouse };
            // SendInput also produces WM_INPUT on Windows. Pierdesk tags every injected
            // packet, so discard that exact feedback before it can be mistaken for the
            // target computer's physical mouse and revoke the active control token.
            if !is_pierdesk_injected_raw_mouse(mouse.ulExtraInformation) {
                let absolute = mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0;
                let device = input.header.hDevice.0 as usize;
                let delta = RAW_MOTION_NORMALIZER
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .normalize(device, absolute, mouse.lLastX, mouse.lLastY);
                if let Some((delta_x, delta_y)) = delta
                    && let Some(context) = active_context()
                {
                    // SAFETY: this is called while dispatching WM_INPUT and has no preconditions.
                    let message_time = unsafe { GetMessageTime() };
                    context.handle_raw_mouse(
                        delta_x,
                        delta_y,
                        u32::from_ne_bytes(message_time.to_ne_bytes()),
                    );
                }
            }
        }
    }
    // SAFETY: WM_INPUT requires the default window procedure to perform system cleanup.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == i32::try_from(HC_ACTION).unwrap_or_default() && lparam.0 != 0 {
        // SAFETY: HC_ACTION for WH_KEYBOARD_LL supplies a valid KBDLLHOOKSTRUCT pointer for the
        // duration of this callback.
        let data = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if !data.flags.contains(LLKHF_INJECTED)
            && let Some(event) = normalize_keyboard_event(wparam.0, data)
            && active_context().is_some_and(|context| context.handle_input(event))
        {
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding preserves the hook chain contract and does not retain callback data.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == i32::try_from(HC_ACTION).unwrap_or_default() && lparam.0 != 0 {
        // SAFETY: HC_ACTION for WH_MOUSE_LL supplies a valid MSLLHOOKSTRUCT pointer for the
        // duration of this callback.
        let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if data.flags & LLMHF_INJECTED == 0
            && let Some(context) = active_context()
            && context.screen_contains(data.pt.x, data.pt.y)
            && let Some(event) = normalize_mouse_event(wparam.0, data)
            && context.handle_mouse_input(event, data.time)
        {
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding preserves the hook chain contract and does not retain callback data.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn active_context() -> Option<Arc<dyn InputHookHandler>> {
    ACTIVE_HOOK_CONTEXT
        .lock()
        .ok()
        .and_then(|active| active.as_ref().map(Arc::clone))
}

fn normalize_keyboard_event(message: usize, data: &KBDLLHOOKSTRUCT) -> Option<InputEvent> {
    let transition = match u32::try_from(message).ok()? {
        WM_KEYDOWN | WM_SYSKEYDOWN => KeyTransition::Down,
        WM_KEYUP | WM_SYSKEYUP => KeyTransition::Up,
        _ => return None,
    };
    let virtual_key = u16::try_from(data.vkCode).ok()?;
    let physical = PhysicalKey {
        scan_code: u16::try_from(data.scanCode)
            .ok()
            .filter(|scan| *scan != 0)?,
        extended: data.flags.contains(LLKHF_EXTENDED),
    };
    Some(InputEvent::Key {
        physical: canonical_physical_key(physical, virtual_key),
        virtual_key,
        transition,
    })
}

fn canonical_physical_key(mut physical: PhysicalKey, virtual_key: u16) -> PhysicalKey {
    // WH_KEYBOARD_LL can mark right Shift as extended even though its actual Set 1 scan is
    // 0x36, not E0 36. Replaying that hook flag verbatim makes SendInput lose the modifier.
    // Normalize at capture AND injection, so a receiver also tolerates an older sender.
    if virtual_key == 0xa1 && physical.scan_code == 0x36 {
        physical.extended = false;
    }
    physical
}

const fn keypad_uses_virtual_key(physical: PhysicalKey, virtual_key: u16) -> bool {
    // Preserve the source's Num Lock/Shift interpretation instead of letting the target's
    // independent Num Lock state turn a requested digit into navigation (or vice versa).
    // Other keys remain scan based, including the separate extended navigation cluster.
    matches!(virtual_key, 0x60..=0x6f | 0x90)
        || (!physical.extended
            && matches!(
                (physical.scan_code, virtual_key),
                (0x47, 0x24) // keypad Home
                    | (0x48, 0x26) // keypad Up
                    | (0x49, 0x21) // keypad Page Up
                    | (0x4b, 0x25) // keypad Left
                    | (0x4c, 0x0c) // keypad Clear
                    | (0x4d, 0x27) // keypad Right
                    | (0x4f, 0x23) // keypad End
                    | (0x50, 0x28) // keypad Down
                    | (0x51, 0x22) // keypad Page Down
                    | (0x52, 0x2d) // keypad Insert
                    | (0x53, 0x2e) // keypad Delete
            ))
}

fn normalize_mouse_event(message: usize, data: &MSLLHOOKSTRUCT) -> Option<InputEvent> {
    let message = u32::try_from(message).ok()?;
    let button = match message {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => Some((MouseButton::Left, KeyTransition::Down)),
        WM_LBUTTONUP => Some((MouseButton::Left, KeyTransition::Up)),
        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => Some((MouseButton::Right, KeyTransition::Down)),
        WM_RBUTTONUP => Some((MouseButton::Right, KeyTransition::Up)),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => Some((MouseButton::Middle, KeyTransition::Down)),
        WM_MBUTTONUP => Some((MouseButton::Middle, KeyTransition::Up)),
        WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => Some((x_button(data.mouseData)?, KeyTransition::Down)),
        WM_XBUTTONUP => Some((x_button(data.mouseData)?, KeyTransition::Up)),
        _ => None,
    };
    if let Some((button, transition)) = button {
        return Some(InputEvent::MouseButton { button, transition });
    }
    match message {
        WM_MOUSEMOVE => Some(InputEvent::MouseMove {
            x: data.pt.x,
            y: data.pt.y,
        }),
        WM_MOUSEWHEEL => Some(InputEvent::Wheel {
            axis: WheelAxis::Vertical,
            delta: signed_high_word(data.mouseData),
        }),
        WM_MOUSEHWHEEL => Some(InputEvent::Wheel {
            axis: WheelAxis::Horizontal,
            delta: signed_high_word(data.mouseData),
        }),
        _ => None,
    }
}

fn x_button(mouse_data: u32) -> Option<MouseButton> {
    match u16::try_from(mouse_data >> 16).ok()? {
        XBUTTON1 => Some(MouseButton::X1),
        XBUTTON2 => Some(MouseButton::X2),
        _ => None,
    }
}

fn signed_high_word(value: u32) -> i16 {
    i16::from_ne_bytes(u16::try_from(value >> 16).unwrap_or_default().to_ne_bytes())
}

#[derive(Debug, Default)]
struct InjectionOutcome {
    injected_events: u64,
    forced_releases: usize,
    remaining_keys: usize,
    remaining_buttons: usize,
    stale_mouse_moves: u64,
}

pub(super) struct SendInputEndpoint {
    screen: ScreenExtent,
    ledger: InputLedger,
    injected_events: u64,
}

impl SendInputEndpoint {
    pub(super) fn new(screen: ScreenExtent) -> Self {
        Self {
            screen,
            ledger: InputLedger::default(),
            injected_events: 0,
        }
    }

    pub(super) fn inject(&mut self, event: InputEvent) -> io::Result<()> {
        let event = self.ledger.pair_key_event(event);
        let next_ledger = if matches!(
            event,
            InputEvent::Key { .. } | InputEvent::MouseButton { .. }
        ) {
            let mut next = self.ledger.clone();
            let result = next.apply(event);
            validate_ledger_result(result)?;
            debug_assert_ne!(result, LedgerResult::Unchanged);
            Some(next)
        } else {
            None
        };
        send_input(event, self.screen)?;
        if let Some(next) = next_ledger {
            self.ledger = next;
        }
        self.injected_events += 1;
        Ok(())
    }

    pub(super) fn inject_stateless(&mut self, event: InputEvent) -> io::Result<()> {
        send_input(event, self.screen)?;
        self.injected_events += 1;
        Ok(())
    }

    pub(super) fn release_all(&mut self) -> (usize, Option<io::Error>) {
        let releases = self.ledger.release_events();
        let mut release_count = 0;
        let mut first_error = None;
        for event in releases {
            match send_input(event, self.screen) {
                Ok(()) => {
                    let result = self.ledger.apply(event);
                    debug_assert_eq!(result, LedgerResult::Released);
                    release_count += 1;
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        (release_count, first_error)
    }

    pub(super) fn outstanding_count(&self) -> usize {
        self.ledger.outstanding_count()
    }

    pub(super) const fn injected_events(&self) -> u64 {
        self.injected_events
    }
}

fn validate_ledger_result(result: LedgerResult) -> io::Result<()> {
    match result {
        LedgerResult::CapacityExceeded => {
            return Err(io::Error::other(
                "SendInput endpoint exceeded the pressed-key capacity",
            ));
        }
        // ReleaseAll is deliberately allowed to overtake queued input during a control handoff.
        // A physical Up already in flight can therefore arrive after its tracked Down was
        // released or discarded. Windows button/key releases are idempotent; injecting that Up
        // is safer than killing the entire cross-screen session or leaving global input stuck.
        LedgerResult::UntrackedRelease
        | LedgerResult::Pressed
        | LedgerResult::Repeated
        | LedgerResult::Released
        | LedgerResult::Unchanged => {}
    }
    Ok(())
}

fn send_input(event: InputEvent, screen: ScreenExtent) -> io::Result<()> {
    let input = input_for_event(event, screen);
    let input_size = i32::try_from(size_of::<INPUT>()).expect("INPUT size fits in i32");
    // SAFETY: the one-element slice contains a fully initialized INPUT union matching its type.
    let sent = unsafe { SendInput(&[input], input_size) };
    if sent == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn input_for_event(event: InputEvent, screen: ScreenExtent) -> INPUT {
    match event {
        InputEvent::Key {
            physical,
            virtual_key,
            transition,
        } => {
            let physical = canonical_physical_key(physical, virtual_key);
            let use_virtual_key = keypad_uses_virtual_key(physical, virtual_key);
            let mut flags = if use_virtual_key {
                KEYBD_EVENT_FLAGS::default()
            } else {
                KEYEVENTF_SCANCODE
            };
            if physical.extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if transition == KeyTransition::Up {
                flags |= KEYEVENTF_KEYUP;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(if use_virtual_key { virtual_key } else { 0 }),
                        wScan: physical.scan_code,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECTION_TAG,
                    },
                },
            }
        }
        InputEvent::MouseMove { x, y } => INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: normalize_coordinate(x, screen.width),
                    dy: normalize_coordinate(y, screen.height),
                    mouseData: 0,
                    // Coalesce like a physical mouse so an application that repaints per move
                    // (Explorer's desktop marquee) cannot fall behind the cursor, except while a
                    // Pierdesk OLE drag loop runs here: see `OleDragLoopGuard`.
                    dwFlags: mouse_move_flags(super::ole_drag_loop_active()),
                    time: 0,
                    dwExtraInfo: INJECTION_TAG,
                },
            },
        },
        InputEvent::MouseButton { button, transition } => {
            let (flags, mouse_data) = match (button, transition) {
                (MouseButton::Left, KeyTransition::Down) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, KeyTransition::Up) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, KeyTransition::Down) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, KeyTransition::Up) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, KeyTransition::Down) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, KeyTransition::Up) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::X1, KeyTransition::Down) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
                (MouseButton::X1, KeyTransition::Up) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
                (MouseButton::X2, KeyTransition::Down) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
                (MouseButton::X2, KeyTransition::Up) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
            };
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: mouse_data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECTION_TAG,
                    },
                },
            }
        }
        InputEvent::Wheel { axis, delta } => INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: u32::from_ne_bytes(i32::from(delta).to_ne_bytes()),
                    dwFlags: match axis {
                        WheelAxis::Vertical => MOUSEEVENTF_WHEEL,
                        WheelAxis::Horizontal => MOUSEEVENTF_HWHEEL,
                    },
                    time: 0,
                    dwExtraInfo: INJECTION_TAG,
                },
            },
        },
    }
}

fn mouse_move_flags(ole_drag_loop_active: bool) -> MOUSE_EVENT_FLAGS {
    if ole_drag_loop_active {
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE_NOCOALESCE
    } else {
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE
    }
}

fn normalize_coordinate(coordinate: i32, extent: i32) -> i32 {
    super::normalized_absolute_coordinate(coordinate, extent)
}

fn injection_loop(context: &HookContext) -> InjectionOutcome {
    let mut endpoint = SendInputEndpoint::new(context.geometry.physical);
    let mut outcome = InjectionOutcome::default();
    while let Some(queued) = context.queue.pop(&context.stop) {
        if queued.event.is_mouse_move()
            && queued.move_generation < context.latest_move_generation.load(Ordering::Acquire)
        {
            outcome.stale_mouse_moves += 1;
            continue;
        }
        if let Err(error) = endpoint.inject(queued.event) {
            context.fail(
                StopCode::InjectionFailure,
                format!("SendInput injection failed: {error}"),
            );
            break;
        }
    }
    let (forced_releases, release_error) = endpoint.release_all();
    if let Some(error) = release_error {
        context.fail(
            StopCode::InjectionFailure,
            format!("SendInput release-all failed: {error}"),
        );
    }
    outcome.injected_events = endpoint.injected_events;
    outcome.forced_releases = forced_releases;
    outcome.remaining_keys = endpoint.ledger.pressed_key_count();
    outcome.remaining_buttons = endpoint.ledger.pressed_button_count();
    outcome
}

/// Runs the local low-level-hook to bounded-queue to `SendInput` diagnostic.
///
/// Physical keyboard, button, and wheel events on the primary display are suppressed only after
/// they are accepted by the reliable queue, then immediately reinjected locally. Accepted mouse
/// movement is also suppressed and the newest queued physical-pixel position is reinjected;
/// movement bypasses the hook only if the queue cannot accept it. Injected events are never
/// recaptured. `Ctrl+Alt+Shift+Esc` is reserved for immediate emergency teardown.
///
/// # Errors
///
/// Returns an error when the duration is invalid, the primary display cannot be queried, hooks or
/// diagnostic threads cannot start, or the injector worker panics.
pub fn run_input_diagnostic(options: InputDiagnosticOptions) -> io::Result<InputDiagnosticReport> {
    if options.lifetime.is_zero() || options.lifetime > MAX_DIAGNOSTIC_LIFETIME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic lifetime must be between 1 millisecond and 300 seconds",
        ));
    }
    let initial_metrics = ScreenExtent::system_metrics()?;
    let dpi_guard = ThreadDpiAwarenessGuard::enter_per_monitor()?;
    let geometry = ScreenGeometry::primary(initial_metrics, dpi_guard.mode)?;
    // SAFETY: this reads the identifier of the current CLI diagnostic thread.
    let hook_thread_id = unsafe { GetCurrentThreadId() };
    let mut bootstrap_message = MSG::default();
    // SAFETY: a no-remove peek creates this thread's message queue without consuming messages.
    let _ = unsafe { PeekMessageW(&raw mut bootstrap_message, None, 0, 0, PM_NOREMOVE) };

    let context = Arc::new(HookContext::new(hook_thread_id, geometry));
    let _active_context = ActiveContextGuard::register(Arc::clone(&context))?;
    let hooks = HookGuards::install()?;

    let injector_context = Arc::clone(&context);
    let injector = std::thread::Builder::new()
        .name("pierdesk-input-injector".to_owned())
        .spawn(move || injection_loop(&injector_context));
    let injector = match injector {
        Ok(injector) => injector,
        Err(error) => {
            drop(hooks);
            context.request_stop(StopCode::ExternalStop);
            context.queue.close();
            return Err(error);
        }
    };

    let (watchdog_cancel, watchdog_receiver) = mpsc::channel();
    let watchdog_context = Arc::clone(&context);
    let lifetime = options.lifetime;
    let watchdog = std::thread::Builder::new()
        .name("pierdesk-input-watchdog".to_owned())
        .spawn(move || {
            if watchdog_receiver.recv_timeout(lifetime).is_err() {
                watchdog_context.request_stop(StopCode::DurationElapsed);
            }
        });
    let watchdog = match watchdog {
        Ok(watchdog) => watchdog,
        Err(error) => {
            context.request_stop(StopCode::ExternalStop);
            drop(hooks);
            context.queue.close();
            let _ = injector.join();
            return Err(error);
        }
    };

    if let Err(error) = run_hook_message_loop() {
        context.fail(
            StopCode::MessageLoopFailure,
            format!("input hook message loop failed: {error}"),
        );
    }
    if StopCode::load(context.stop.load(Ordering::Acquire)) == StopCode::Running {
        context.request_stop(StopCode::ExternalStop);
    }

    drop(hooks);
    let _ = watchdog_cancel.send(());
    join_thread(watchdog, "input watchdog")?;
    context.queue.close();
    let injection = join_thread(injector, "input injector")?;

    build_report(&context, &injection)
}

fn build_report(
    context: &HookContext,
    injection: &InjectionOutcome,
) -> io::Result<InputDiagnosticReport> {
    let (captured, forced_capture_releases, remaining_captured_inputs) = {
        let mut machine = context
            .state_machine
            .lock()
            .map_err(|_| io::Error::other("input state machine lock was poisoned"))?;
        let captured = machine.stats();
        let forced_capture_releases = machine.release_all().len();
        (
            captured,
            forced_capture_releases,
            machine.outstanding_count(),
        )
    };
    let stop_code = StopCode::load(context.stop.load(Ordering::Acquire));
    let stop_reason = stop_code
        .report_reason()
        .unwrap_or(DiagnosticStopReason::ExternalStop);
    let failure = context.failure.lock().map_or_else(
        |poisoned| poisoned.into_inner().clone(),
        |value| value.clone(),
    );
    let clean_shutdown = failure.is_none()
        && remaining_captured_inputs == 0
        && injection.remaining_keys == 0
        && injection.remaining_buttons == 0;
    Ok(InputDiagnosticReport {
        stop_reason,
        captured,
        injected_events: injection.injected_events,
        forced_capture_releases,
        forced_injected_releases: injection.forced_releases,
        remaining_captured_inputs,
        remaining_injected_keys: injection.remaining_keys,
        remaining_injected_buttons: injection.remaining_buttons,
        queue_capacity: INPUT_QUEUE_CAPACITY,
        queue_peak_depth: context.queue.peak_depth(),
        coalesced_mouse_moves: context.queue.coalesced_mouse_moves.load(Ordering::Relaxed),
        bypassed_mouse_moves: context.queue.bypassed_mouse_moves.load(Ordering::Relaxed),
        stale_mouse_moves: injection.stale_mouse_moves,
        dpi_mode: context.geometry.dpi_mode,
        effective_dpi: context.geometry.effective_dpi,
        initial_metrics_width: context.geometry.initial_metrics.width,
        initial_metrics_height: context.geometry.initial_metrics.height,
        primary_physical_width: context.geometry.physical.width,
        primary_physical_height: context.geometry.physical.height,
        clean_shutdown,
        failure,
    })
}

pub(super) fn run_hook_message_loop() -> io::Result<()> {
    loop {
        let mut message = MSG::default();
        // SAFETY: the current thread owns its queue and message storage.
        let status = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
        if status == -1 {
            return Err(io::Error::last_os_error());
        }
        if status == 0 || message.message == WM_QUIT {
            return Ok(());
        }
        // SAFETY: the message was initialized by GetMessageW and is used synchronously.
        unsafe {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

fn join_thread<T>(thread: JoinHandle<T>, name: &str) -> io::Result<T> {
    thread
        .join()
        .map_err(|_| io::Error::other(format!("{name} thread panicked")))
}

fn windows_to_io(error: &windows_core::Error) -> io::Error {
    io::Error::other(format!(
        "{} ({:#010X})",
        error,
        error.code().0.cast_unsigned()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_cleanup_accepts_an_idempotent_late_release() {
        assert!(validate_ledger_result(LedgerResult::UntrackedRelease).is_ok());
    }

    fn geometry(width: i32, height: i32, effective_dpi: u32) -> ScreenGeometry {
        let physical = ScreenExtent { width, height };
        ScreenGeometry {
            physical,
            initial_metrics: physical,
            effective_dpi,
            dpi_mode: DiagnosticDpiMode::PerMonitorV2,
        }
    }

    fn move_event(x: i32) -> QueuedInput {
        QueuedInput {
            event: InputEvent::MouseMove { x, y: 10 },
            move_generation: u64::try_from(x).unwrap(),
        }
    }

    fn key_event(transition: KeyTransition) -> QueuedInput {
        QueuedInput {
            event: InputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 0x1e,
                    extended: false,
                },
                virtual_key: 0x41,
                transition,
            },
            move_generation: 0,
        }
    }

    #[test]
    fn bounded_queue_coalesces_only_adjacent_mouse_moves() {
        let queue = BoundedInputQueue::new(3);
        assert_eq!(queue.try_push(move_event(1)), EnqueueResult::Queued);
        assert_eq!(queue.try_push(move_event(2)), EnqueueResult::Coalesced);
        assert_eq!(
            queue.try_push(key_event(KeyTransition::Down)),
            EnqueueResult::Queued
        );
        assert_eq!(queue.try_push(move_event(3)), EnqueueResult::Queued);
        assert_eq!(queue.peak_depth(), 3);
        assert_eq!(queue.coalesced_mouse_moves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reliable_queue_overflow_is_fatal_but_mouse_move_overflow_is_bypassed() {
        let queue = BoundedInputQueue::new(1);
        assert_eq!(
            queue.try_push(key_event(KeyTransition::Down)),
            EnqueueResult::Queued
        );
        assert_eq!(
            queue.try_push(key_event(KeyTransition::Up)),
            EnqueueResult::Fatal
        );
        assert_eq!(
            queue.try_push(move_event(4)),
            EnqueueResult::MouseMoveBypassed
        );
        assert_eq!(queue.bypassed_mouse_moves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn raw_mouse_sink_can_be_registered_removed_and_registered_again() {
        let first = RawMouseInputWindow::create().unwrap();
        drop(first);
        let second = RawMouseInputWindow::create().unwrap();
        drop(second);
    }

    #[test]
    fn raw_motion_normalizer_preserves_relative_devices_and_deltas_absolute_devices() {
        let mut normalizer = RawMotionNormalizer::new();
        assert_eq!(normalizer.normalize(1, false, 15, -4), Some((15, -4)));
        assert_eq!(normalizer.normalize(1, false, 0, 0), None);

        assert_eq!(normalizer.normalize(7, true, 30_000, 20_000), None);
        assert_eq!(
            normalizer.normalize(7, true, 30_125, 19_950),
            Some((125, -50))
        );
        // Every absolute pointing device has an independent baseline.
        assert_eq!(normalizer.normalize(8, true, 10_000, 12_000), None);
        assert_eq!(
            normalizer.normalize(7, true, 30_100, 20_000),
            Some((-25, 50))
        );
    }

    #[test]
    fn raw_mouse_filter_rejects_only_pierdesk_injected_motion() {
        assert!(is_pierdesk_injected_raw_mouse(RAW_INJECTION_TAG));
        assert!(!is_pierdesk_injected_raw_mouse(0));
        assert!(!is_pierdesk_injected_raw_mouse(0x1234_5678));
    }

    #[test]
    fn accepted_mouse_move_is_suppressed_for_true_loopback() {
        let context = HookContext::new(1, geometry(3840, 2160, 120));
        assert!(context.handle(InputEvent::MouseMove { x: 3839, y: 2159 }));
        assert_eq!(context.queue.peak_depth(), 1);
    }

    #[test]
    fn physical_metrics_must_match_current_display_mode() {
        let physical = ScreenExtent {
            width: 3840,
            height: 2160,
        };
        assert!(validate_physical_extents(physical, physical).is_ok());
        let logical = ScreenExtent {
            width: 3072,
            height: 1728,
        };
        let error = validate_physical_extents(logical, physical).unwrap_err();
        assert!(error.to_string().contains("3072x1728"));
        assert!(error.to_string().contains("3840x2160"));
    }

    #[test]
    fn send_input_keyboard_payload_preserves_scan_and_extended_flags() {
        let input = input_for_event(
            InputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 0x1d,
                    extended: true,
                },
                virtual_key: 0xa3,
                transition: KeyTransition::Up,
            },
            ScreenExtent {
                width: 1920,
                height: 1080,
            },
        );
        assert_eq!(input.r#type, INPUT_KEYBOARD);
        // SAFETY: INPUT_KEYBOARD makes the ki union member active.
        let keyboard = unsafe { input.Anonymous.ki };
        assert_eq!(keyboard.wVk, VIRTUAL_KEY(0));
        assert_eq!(keyboard.wScan, 0x1d);
        assert!(keyboard.dwFlags.contains(KEYEVENTF_SCANCODE));
        assert!(keyboard.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
        assert!(keyboard.dwFlags.contains(KEYEVENTF_KEYUP));
        assert_eq!(keyboard.dwExtraInfo, INJECTION_TAG);
    }

    #[test]
    fn right_shift_hook_extension_is_removed_without_changing_right_control() {
        for (virtual_key, scan_code, hook_extended, expected_extended) in [
            (0xa1, 0x36, true, false),
            (0xa1, 0x36, false, false),
            (0xa0, 0x2a, false, false),
            (0xa3, 0x1d, true, true),
            (0xa5, 0x38, true, true),
        ] {
            for (message, transition) in [
                (WM_KEYDOWN, KeyTransition::Down),
                (WM_KEYUP, KeyTransition::Up),
            ] {
                let data = KBDLLHOOKSTRUCT {
                    vkCode: virtual_key,
                    scanCode: scan_code,
                    flags: if hook_extended {
                        LLKHF_EXTENDED
                    } else {
                        windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT_FLAGS::default()
                    },
                    ..Default::default()
                };
                assert_eq!(
                    normalize_keyboard_event(message as usize, &data),
                    Some(InputEvent::Key {
                        physical: PhysicalKey {
                            scan_code: u16::try_from(scan_code).unwrap(),
                            extended: expected_extended,
                        },
                        virtual_key: u16::try_from(virtual_key).unwrap(),
                        transition,
                    })
                );
            }
        }
    }

    #[test]
    fn right_shift_injection_accepts_legacy_extended_sender_events() {
        for transition in [KeyTransition::Down, KeyTransition::Up] {
            let input = input_for_event(
                InputEvent::Key {
                    physical: PhysicalKey {
                        scan_code: 0x36,
                        extended: true,
                    },
                    virtual_key: 0xa1,
                    transition,
                },
                ScreenExtent {
                    width: 1920,
                    height: 1080,
                },
            );
            // SAFETY: the event above produces an INPUT_KEYBOARD union.
            let keyboard = unsafe { input.Anonymous.ki };
            assert_eq!(keyboard.wScan, 0x36);
            assert!(keyboard.dwFlags.contains(KEYEVENTF_SCANCODE));
            assert!(!keyboard.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
            assert_eq!(
                keyboard.dwFlags.contains(KEYEVENTF_KEYUP),
                transition == KeyTransition::Up
            );
        }
    }

    #[test]
    fn keypad_injection_preserves_digits_navigation_decimal_and_operators() {
        for (scan_code, numeric, navigation) in [
            (0x52, 0x60, 0x2d),
            (0x4f, 0x61, 0x23),
            (0x50, 0x62, 0x28),
            (0x51, 0x63, 0x22),
            (0x4b, 0x64, 0x25),
            (0x4c, 0x65, 0x0c),
            (0x4d, 0x66, 0x27),
            (0x47, 0x67, 0x24),
            (0x48, 0x68, 0x26),
            (0x49, 0x69, 0x21),
            (0x53, 0x6e, 0x2e),
            (0x37, 0x6a, 0x6a),
            (0x4e, 0x6b, 0x6b),
            (0x4a, 0x6d, 0x6d),
        ] {
            for virtual_key in [numeric, navigation] {
                for transition in [KeyTransition::Down, KeyTransition::Up] {
                    let input = input_for_event(
                        InputEvent::Key {
                            physical: PhysicalKey {
                                scan_code,
                                extended: false,
                            },
                            virtual_key,
                            transition,
                        },
                        ScreenExtent {
                            width: 1920,
                            height: 1080,
                        },
                    );
                    // SAFETY: a key event produces an INPUT_KEYBOARD union.
                    let keyboard = unsafe { input.Anonymous.ki };
                    assert_eq!(keyboard.wVk, VIRTUAL_KEY(virtual_key));
                    assert_eq!(keyboard.wScan, scan_code);
                    assert!(!keyboard.dwFlags.contains(KEYEVENTF_SCANCODE));
                    assert!(!keyboard.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
                    assert_eq!(
                        keyboard.dwFlags.contains(KEYEVENTF_KEYUP),
                        transition == KeyTransition::Up
                    );
                }
            }
        }
        // The separate navigation cluster and keypad Enter must retain their physical identity.
        for (scan_code, virtual_key) in [(0x4b, 0x25), (0x1c, 0x0d)] {
            assert!(!keypad_uses_virtual_key(
                PhysicalKey {
                    scan_code,
                    extended: true
                },
                virtual_key
            ));
        }
        assert!(keypad_uses_virtual_key(
            PhysicalKey {
                scan_code: 0x35,
                extended: true
            },
            0x6f
        ));
    }

    #[test]
    fn send_input_absolute_mouse_moves_disable_a_second_os_coalescing_pass() {
        let input = input_for_event(
            InputEvent::MouseMove { x: 960, y: 540 },
            ScreenExtent {
                width: 1920,
                height: 1080,
            },
        );
        assert_eq!(input.r#type, INPUT_MOUSE);
        // SAFETY: INPUT_MOUSE makes the mi union member active.
        let mouse = unsafe { input.Anonymous.mi };
        assert!(mouse.dwFlags.contains(MOUSEEVENTF_MOVE));
        assert!(mouse.dwFlags.contains(MOUSEEVENTF_ABSOLUTE));
        assert!(!mouse.dwFlags.contains(MOUSEEVENTF_MOVE_NOCOALESCE));
        assert!(mouse_move_flags(true).contains(MOUSEEVENTF_MOVE_NOCOALESCE));
        assert!(!mouse_move_flags(false).contains(MOUSEEVENTF_MOVE_NOCOALESCE));
        assert_eq!(mouse.dwExtraInfo, INJECTION_TAG);
    }

    #[test]
    fn windows_translates_keypad_digits_with_either_target_numlock_state() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, ToUnicodeEx};
        // SAFETY: read only the current layout; ToUnicodeEx uses our private keyboard-state
        // buffer and flag 4 prevents modifying the system's dead-key state. No input is sent.
        let layout = unsafe { GetKeyboardLayout(0) };
        for num_lock in [0, 1] {
            let mut state = [0_u8; 256];
            state[0x90] = num_lock;
            for digit in 0..=9_u16 {
                let mut text = [0_u16; 8];
                // SAFETY: all buffers are correctly sized and no handles are retained.
                let count = unsafe {
                    ToUnicodeEx(
                        u32::from(0x60 + digit),
                        0,
                        &state,
                        &mut text,
                        4,
                        Some(layout),
                    )
                };
                assert_eq!(count, 1);
                assert_eq!(text[0], u16::from(b'0') + digit);
            }
        }
    }

    #[test]
    fn send_input_mouse_payload_has_distinct_paired_button_flags() {
        let screen = ScreenExtent {
            width: 1920,
            height: 1080,
        };
        for (button, down_flags, up_flags, mouse_data) in [
            (
                MouseButton::Left,
                MOUSEEVENTF_LEFTDOWN,
                MOUSEEVENTF_LEFTUP,
                0,
            ),
            (
                MouseButton::Right,
                MOUSEEVENTF_RIGHTDOWN,
                MOUSEEVENTF_RIGHTUP,
                0,
            ),
            (
                MouseButton::Middle,
                MOUSEEVENTF_MIDDLEDOWN,
                MOUSEEVENTF_MIDDLEUP,
                0,
            ),
            (
                MouseButton::X1,
                MOUSEEVENTF_XDOWN,
                MOUSEEVENTF_XUP,
                u32::from(XBUTTON1),
            ),
            (
                MouseButton::X2,
                MOUSEEVENTF_XDOWN,
                MOUSEEVENTF_XUP,
                u32::from(XBUTTON2),
            ),
        ] {
            for (transition, expected_flags) in [
                (KeyTransition::Down, down_flags),
                (KeyTransition::Up, up_flags),
            ] {
                let input = input_for_event(InputEvent::MouseButton { button, transition }, screen);
                assert_eq!(input.r#type, INPUT_MOUSE);
                // SAFETY: INPUT_MOUSE makes the mi union member active.
                let mouse = unsafe { input.Anonymous.mi };
                assert_eq!(mouse.dwFlags, expected_flags);
                assert_eq!(mouse.mouseData, mouse_data);
                assert_eq!(mouse.dwExtraInfo, INJECTION_TAG);
            }
        }
    }

    #[test]
    fn signed_wheel_delta_and_primary_coordinate_mapping_are_exact() {
        assert_eq!(signed_high_word(0x0078_0000), 120);
        assert_eq!(signed_high_word(0xff88_0000), -120);
        assert_eq!(normalize_coordinate(0, 1920), 0);
        assert_eq!(
            normalize_coordinate(5000, 1920),
            normalize_coordinate(1919, 1920)
        );
    }

    #[test]
    fn physical_coordinate_mapping_is_stable_across_resolution_and_dpi_matrix() {
        for (width, height) in [
            (1024, 768),
            (1366, 768),
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
            (5120, 2880),
            (7680, 4320),
        ] {
            for dpi in [96_i64, 120, 144, 168, 192, 240] {
                let logical_width = i32::try_from(i64::from(width) * 96 / dpi).unwrap();
                let logical_height = i32::try_from(i64::from(height) * 96 / dpi).unwrap();
                if dpi == 96 {
                    assert_eq!((logical_width, logical_height), (width, height));
                } else {
                    assert!(logical_width < width);
                    assert!(logical_height < height);
                }

                for (coordinate, extent) in [
                    (0, width),
                    (1, width),
                    (width / 4, width),
                    (width / 2, width),
                    (width - 1, width),
                    (0, height),
                    (1, height),
                    (height / 4, height),
                    (height / 2, height),
                    (height - 1, height),
                ] {
                    let normalized = i64::from(normalize_coordinate(coordinate, extent));
                    assert!(normalized <= 65_535);
                    // Exact, not within one pixel: an edge arrival must never land on the edge.
                    for divisor in [65_536, 65_535] {
                        let recovered = normalized * i64::from(extent) / divisor;
                        assert_eq!(
                            recovered,
                            i64::from(coordinate),
                            "{width}x{height} at {dpi} DPI: {coordinate}/{extent} /{divisor}"
                        );
                    }
                }

                assert_eq!(normalize_coordinate(0, width), 0);
                assert_eq!(normalize_coordinate(0, height), 0);
            }
        }
    }
}
