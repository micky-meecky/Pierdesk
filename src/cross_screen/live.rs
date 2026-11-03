use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClassNameW, GetClientRect,
    GetClipCursor, GetCursorPos, GetDesktopWindow, GetForegroundWindow, GetShellWindow, HCURSOR,
    HTCLIENT, HWND_TOPMOST, IsIconic, LWA_ALPHA, MA_NOACTIVATE, MSG, PM_NOREMOVE, PeekMessageW,
    PostMessageW, PostThreadMessageW, RegisterClassW, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SetCursor, SetLayeredWindowAttributes, SetWindowPos, ShowCursor,
    UnregisterClassW, WM_APP, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_QUIT, WM_SETCURSOR, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, IsWindowVisible, PM_REMOVE, SetCursorPos,
};
use windows_core::{PCWSTR, w};

use crate::app_settings::{AppSettings, CrossScreenSettings};
use crate::device_store::DeviceStore;

use super::boundary::{
    BoundaryPolicy, PhysicalPoint, PhysicalScreenSize, PointerSample, ScreenBoundary,
    map_normalized_boundary_position,
};
use super::boundary_map::{
    BoundaryMapConstraints, BoundaryMapV2, DEFAULT_MINIMUM_SEGMENT_PIXELS, PointerMotion,
    PreparedBoundaryMap,
};
use super::channel::{
    ChannelControlEvent, ChannelEvent, ChannelInputEvent, ChannelState, ControlTransitionAck,
    CrossScreenChannel, LayoutConfiguration, LogicalBoundaryTransition, NormalizedMousePosition,
    PhysicalPrimaryScreen, RemoteInputEvent, ScreenMillimeterSize, denormalize_physical_coordinate,
    normalize_physical_coordinate,
};
use super::control::{
    ControlCoordinator, ControlProtocolAction, ControlProtocolState, dispatch_control_actions,
};
use super::current_primary_screen_millimeters;
use super::diagnostic::{
    ActiveContextGuard, HookGuards, InputHookHandler, RawMouseInputWindow, ScreenExtent,
    SendInputEndpoint, ThreadDpiAwarenessGuard, run_hook_message_loop,
};
use super::runtime::{AutomaticChannelConfig, AutomaticChannelRole};
use super::state::{
    InputEvent, InputEventStats, InputStateMachine, KeyTransition, MouseButton, StateDecision,
};
#[cfg(test)]
use super::{BoundarySyncPhase, BoundarySyncSurface, encode_boundary_sync};
use super::{
    BoundaryWorkspaceV3, CompiledBoundaryLayout, PortalPoint, RemoteBoundaryWorkspace,
    decode_boundary_sync,
};

const LIVE_QUEUE_CAPACITY: usize = 256;
const INJECTION_QUEUE_CAPACITY: usize = 256;
// Preserve ordinary network bursts instead of reducing every adjacent pair to one point. A hard
// tail cap still prevents an exceptionally delayed receiver from replaying stale pointer history.
const TARGET_MOUSE_BURST_CAPACITY: usize = 16;
const MAX_PENDING_EMERGENCY_MODIFIERS: usize = 6;
const CONNECTION_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
// Local hooks and authenticated peer input both wake this worker directly. This timeout
// is only for maintenance and disconnect checks, not the pointer delivery cadence.
const NETWORK_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PASTE_FENCE_TIMEOUT: Duration = Duration::from_secs(2);
const INJECTION_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const TRUST_RECHECK_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LIFETIME: Duration = Duration::from_mins(5);
const MAX_CURSOR_VISIBILITY_ADJUSTMENTS: u32 = 64;
const CURSOR_VISIBILITY_REPAIR_INTERVAL: Duration = Duration::from_millis(500);
const SOURCE_CURSOR_REPAIR_INTERVAL: Duration = Duration::from_millis(100);
const LOCAL_TAKEOVER_MOVE_WINDOW: Duration = Duration::from_millis(160);
const LOCAL_TAKEOVER_MOVE_MIN_SAMPLES: u8 = 3;
const LOCAL_TAKEOVER_MOVE_DISTANCE: u32 = 24;
const POINTER_GAIN_SCALE: i64 = 1_024;
const POINTER_GAIN_MIN: u64 = 64;
const POINTER_GAIN_MAX: u64 = 8_192;
const POINTER_GAIN_BIN_COUNT: usize = 4;
const POINTER_GAIN_MIN_RAW_DISTANCE: u64 = 8;
const POINTER_GAIN_HISTORY_LIMIT: usize = 128;
const PENDING_ENTER_RAW_AXIS_LIMIT: i64 = 1_048_576;
const PENDING_ENTER_EDGE_OVERFLOW_MAX_AGE_MS: u32 = 100;
const SOURCE_CURSOR_SHIELD_ALPHA: u8 = 1;
const SOURCE_CURSOR_SHIELD_CLASS: PCWSTR = w!("PierdeskSourceCursorShieldWindow");
const SOURCE_CURSOR_SHIELD_SET_ACTIVE: u32 = WM_APP + 0x51;
const LIVE_TRANSITION_TRACE_CAPACITY: usize = 1_024;
const BOUNDARY_PROBE_CAPACITY: usize = 64;
const RAW_BOUNDARY_DIAGNOSTIC_DISTANCE: u32 = 512;
const RAW_BOUNDARY_DIAGNOSTIC_INTERVAL_MS: u32 = 50;

const VK_CONTROL_VALUE: u16 = 0x11;
const VK_MENU_VALUE: u16 = 0x12;
const VK_SHIFT_VALUE: u16 = 0x10;
const VK_ESCAPE_VALUE: u16 = 0x1b;
const VK_INSERT_VALUE: u16 = 0x2d;
const VK_V_VALUE: u16 = 0x56;
const VK_NUMLOCK_VALUE: u16 = 0x90;
const VK_LSHIFT_VALUE: u16 = 0xa0;
const VK_RSHIFT_VALUE: u16 = 0xa1;
const VK_LCONTROL_VALUE: u16 = 0xa2;
const VK_RCONTROL_VALUE: u16 = 0xa3;
const VK_LMENU_VALUE: u16 = 0xa4;
const VK_RMENU_VALUE: u16 = 0xa5;

const MODIFIER_LEFT_CONTROL: u16 = 1 << 0;
const MODIFIER_RIGHT_CONTROL: u16 = 1 << 1;
const MODIFIER_GENERIC_CONTROL: u16 = 1 << 2;
const MODIFIER_LEFT_ALT: u16 = 1 << 3;
const MODIFIER_RIGHT_ALT: u16 = 1 << 4;
const MODIFIER_GENERIC_ALT: u16 = 1 << 5;
const MODIFIER_LEFT_SHIFT: u16 = 1 << 6;
const MODIFIER_RIGHT_SHIFT: u16 = 1 << 7;
const MODIFIER_GENERIC_SHIFT: u16 = 1 << 8;
const CONTROL_MASK: u16 = MODIFIER_LEFT_CONTROL | MODIFIER_RIGHT_CONTROL | MODIFIER_GENERIC_CONTROL;
const ALT_MASK: u16 = MODIFIER_LEFT_ALT | MODIFIER_RIGHT_ALT | MODIFIER_GENERIC_ALT;
const SHIFT_MASK: u16 = MODIFIER_LEFT_SHIFT | MODIFIER_RIGHT_SHIFT | MODIFIER_GENERIC_SHIFT;

#[derive(Clone, Debug)]
pub struct LiveControlDiagnosticOptions {
    pub store: DeviceStore,
    pub connect_timeout: Duration,
    pub lifetime: Duration,
}

#[derive(Clone, Debug)]
struct LiveControlSessionOptions {
    store: DeviceStore,
    connect_timeout: Duration,
}

impl From<&LiveControlDiagnosticOptions> for LiveControlSessionOptions {
    fn from(options: &LiveControlDiagnosticOptions) -> Self {
        Self {
            store: options.store.clone(),
            connect_timeout: options.connect_timeout,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LiveSessionPhase {
    Connecting,
    Connected,
    Stopping,
}

#[derive(Clone, Debug)]
pub(super) struct LiveSessionStatus {
    pub phase: LiveSessionPhase,
    pub role: Option<AutomaticChannelRole>,
    pub local_screen: Option<PhysicalScreenSize>,
    pub peer_screen: Option<PhysicalScreenSize>,
    pub peer_screen_millimeters: Option<ScreenMillimeterSize>,
    pub peer_placement: Option<super::boundary::PeerPlacement>,
    pub route: &'static str,
    pub round_trip_micros: Option<u64>,
    pub session_id: Option<String>,
    pub channel_epoch: Option<u64>,
    pub last_enter_latency_micros: Option<u64>,
    pub last_control_return_reason: Option<&'static str>,
    pub control_return_sequence: u64,
    pub last_transition_trace: Option<String>,
    pub transition_trace_sequence: u64,
}

impl Default for LiveSessionStatus {
    fn default() -> Self {
        Self {
            phase: LiveSessionPhase::Connecting,
            role: None,
            local_screen: None,
            peer_screen: None,
            peer_screen_millimeters: None,
            peer_placement: None,
            route: "local",
            round_trip_micros: None,
            session_id: None,
            channel_epoch: None,
            last_enter_latency_micros: None,
            last_control_return_reason: None,
            control_return_sequence: 0,
            last_transition_trace: None,
            transition_trace_sequence: 0,
        }
    }
}

pub(super) struct LiveSessionControl {
    cancel: AtomicBool,
    pause_on_fullscreen: bool,
    status: Mutex<LiveSessionStatus>,
    started_at: Instant,
    traces: Arc<LiveTransitionTraceBuffer>,
    boundary_sync: Arc<LiveBoundarySyncState>,
}

#[derive(Debug, Default)]
pub(super) struct LiveBoundarySyncState {
    outbound: Mutex<Option<Vec<u8>>>,
    inbound: Mutex<Option<RemoteBoundaryWorkspace>>,
    next_inbound_sequence: AtomicU64,
}

impl LiveBoundarySyncState {
    pub(super) fn queue(&self, payload: Vec<u8>) {
        match self.outbound.lock() {
            Ok(mut outbound) => *outbound = Some(payload),
            Err(poisoned) => *poisoned.into_inner() = Some(payload),
        }
    }

    fn take_outbound(&self) -> Option<Vec<u8>> {
        self.outbound.lock().map_or_else(
            |poisoned| poisoned.into_inner().take(),
            |mut outbound| outbound.take(),
        )
    }

    fn restore_outbound(&self, payload: Vec<u8>) {
        let mut outbound = self
            .outbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if outbound.is_none() {
            *outbound = Some(payload);
        }
    }

    fn receive(
        &self,
        payload: &[u8],
        peer: crate::security::CertificateFingerprint,
        local: crate::security::CertificateFingerprint,
    ) -> io::Result<()> {
        let sequence = self
            .next_inbound_sequence
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let preview = decode_boundary_sync(payload, peer, local, sequence)?;
        match self.inbound.lock() {
            Ok(mut inbound) => *inbound = Some(preview),
            Err(poisoned) => *poisoned.into_inner() = Some(preview),
        }
        Ok(())
    }

    pub(super) fn snapshot(&self) -> Option<RemoteBoundaryWorkspace> {
        self.inbound.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |inbound| inbound.clone(),
        )
    }

    pub(super) fn clear_inbound(&self, sequence: u64) {
        let mut inbound = self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inbound
            .as_ref()
            .is_some_and(|value| value.sequence == sequence)
        {
            *inbound = None;
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LiveTransitionTraceEvent {
    pub sequence: u64,
    pub session_elapsed_micros: u64,
    pub session_id: Option<String>,
    pub channel_epoch: Option<u64>,
    pub trace: String,
}

#[derive(Debug, Default)]
pub(super) struct LiveTransitionTraceBuffer {
    events: Mutex<VecDeque<LiveTransitionTraceEvent>>,
    next_sequence: AtomicU64,
    dropped: AtomicU64,
}

impl LiveTransitionTraceBuffer {
    fn push(&self, mut event: LiveTransitionTraceEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        event.sequence = self
            .next_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if events.len() == LIVE_TRANSITION_TRACE_CAPACITY {
            events.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        events.push_back(event);
    }

    pub(super) fn drain(&self) -> (Vec<LiveTransitionTraceEvent>, u64) {
        let events = self.events.lock().map_or_else(
            |poisoned| poisoned.into_inner().drain(..).collect(),
            |mut events| events.drain(..).collect(),
        );
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        (events, dropped)
    }
}

impl LiveSessionControl {
    #[cfg(test)]
    pub(super) fn new(pause_on_fullscreen: bool) -> Self {
        Self::with_trace_buffer(
            pause_on_fullscreen,
            Arc::new(LiveTransitionTraceBuffer::default()),
        )
    }

    #[cfg(test)]
    pub(super) fn with_trace_buffer(
        pause_on_fullscreen: bool,
        traces: Arc<LiveTransitionTraceBuffer>,
    ) -> Self {
        Self::with_boundary_sync(
            pause_on_fullscreen,
            traces,
            Arc::new(LiveBoundarySyncState::default()),
        )
    }

    pub(super) fn with_boundary_sync(
        pause_on_fullscreen: bool,
        traces: Arc<LiveTransitionTraceBuffer>,
        boundary_sync: Arc<LiveBoundarySyncState>,
    ) -> Self {
        Self {
            cancel: AtomicBool::new(false),
            pause_on_fullscreen,
            status: Mutex::new(LiveSessionStatus::default()),
            started_at: Instant::now(),
            traces,
            boundary_sync,
        }
    }

    pub(super) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    pub(super) fn snapshot(&self) -> LiveSessionStatus {
        self.status.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |status| status.clone(),
        )
    }

    fn update(&self, update: impl FnOnce(&mut LiveSessionStatus)) {
        match self.status.lock() {
            Ok(mut status) => update(&mut status),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    fn record_control_return(&self, reason: &'static str) {
        self.update(|status| {
            status.last_control_return_reason = Some(reason);
            status.control_return_sequence = status.control_return_sequence.saturating_add(1);
        });
    }

    fn record_enter_latency(&self, elapsed: Duration) {
        self.update(|status| {
            status.last_enter_latency_micros =
                Some(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
        });
    }

    pub(super) fn record_transition_trace(&self, trace: String) {
        self.record_transition_trace_at(trace, Instant::now());
    }

    fn record_transition_trace_at(&self, trace: String, observed_at: Instant) {
        let (session_id, channel_epoch) = self.status.lock().map_or_else(
            |poisoned| {
                let mut status = poisoned.into_inner();
                status.last_transition_trace = Some(trace.clone());
                status.transition_trace_sequence =
                    status.transition_trace_sequence.saturating_add(1);
                (status.session_id.clone(), status.channel_epoch)
            },
            |mut status| {
                status.last_transition_trace = Some(trace.clone());
                status.transition_trace_sequence =
                    status.transition_trace_sequence.saturating_add(1);
                (status.session_id.clone(), status.channel_epoch)
            },
        );
        self.traces.push(LiveTransitionTraceEvent {
            sequence: 0,
            session_elapsed_micros: elapsed_micros(
                observed_at.saturating_duration_since(self.started_at),
            ),
            session_id,
            channel_epoch,
            trace,
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveControlStopReason {
    DurationElapsed,
    EmergencyShortcut,
    ConnectionFailure,
    QueueFailure,
    InjectionFailure,
    MessageLoopFailure,
    ExternalStop,
    FullscreenPause,
    DisplayChanged,
}

impl LiveControlStopReason {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DurationElapsed => "duration_elapsed",
            Self::EmergencyShortcut => "emergency_shortcut",
            Self::ConnectionFailure => "connection_failure",
            Self::QueueFailure => "queue_failure",
            Self::InjectionFailure => "injection_failure",
            Self::MessageLoopFailure => "message_loop_failure",
            Self::ExternalStop => "external_stop",
            Self::FullscreenPause => "fullscreen_pause",
            Self::DisplayChanged => "display_changed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LiveControlDiagnosticReport {
    pub role: AutomaticChannelRole,
    pub local_screen: PhysicalScreenSize,
    pub peer_screen: PhysicalScreenSize,
    pub stop_reason: LiveControlStopReason,
    pub clean_shutdown: bool,
    pub control_entries: u64,
    pub control_returns: u64,
    pub enter_latency_samples: u64,
    pub enter_latency_average_micros: u64,
    pub enter_latency_max_micros: u64,
    pub return_latency_samples: u64,
    pub return_latency_average_micros: u64,
    pub return_latency_max_micros: u64,
    pub rejected_input_epochs: u64,
    pub ignored_stale_acknowledgements: u64,
    pub forwarded: InputEventStats,
    pub injected_events: u64,
    pub forced_injected_releases: u64,
    pub remaining_forwarded_inputs: usize,
    pub remaining_injected_inputs: usize,
    pub queue_peak_depth: usize,
    pub queue_coalesced_moves: u64,
    pub queue_bypassed_moves: u64,
    pub channel: super::channel::ChannelSnapshot,
    pub failure: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum LiveRoute {
    Local = 0,
    ControllingPeer = 1,
    ControlledByPeer = 2,
    EnteringPeer = 3,
}

impl LiveRoute {
    fn load(value: u8) -> Self {
        match value {
            1 => Self::ControllingPeer,
            2 => Self::ControlledByPeer,
            3 => Self::EnteringPeer,
            _ => Self::Local,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ControllingPeer => "controlling_peer",
            Self::ControlledByPeer => "controlled_by_peer",
            Self::EnteringPeer => "entering_peer",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum LiveStopCode {
    Running = 0,
    DurationElapsed = 1,
    EmergencyShortcut = 2,
    ConnectionFailure = 3,
    QueueFailure = 4,
    InjectionFailure = 5,
    MessageLoopFailure = 6,
    ExternalStop = 7,
    FullscreenPause = 8,
    DisplayChanged = 9,
}

impl LiveStopCode {
    fn load(value: u8) -> Self {
        match value {
            1 => Self::DurationElapsed,
            2 => Self::EmergencyShortcut,
            3 => Self::ConnectionFailure,
            4 => Self::QueueFailure,
            5 => Self::InjectionFailure,
            6 => Self::MessageLoopFailure,
            7 => Self::ExternalStop,
            8 => Self::FullscreenPause,
            9 => Self::DisplayChanged,
            _ => Self::Running,
        }
    }

    const fn reason(self) -> LiveControlStopReason {
        match self {
            Self::Running | Self::ExternalStop => LiveControlStopReason::ExternalStop,
            Self::FullscreenPause => LiveControlStopReason::FullscreenPause,
            Self::DisplayChanged => LiveControlStopReason::DisplayChanged,
            Self::DurationElapsed => LiveControlStopReason::DurationElapsed,
            Self::EmergencyShortcut => LiveControlStopReason::EmergencyShortcut,
            Self::ConnectionFailure => LiveControlStopReason::ConnectionFailure,
            Self::QueueFailure => LiveControlStopReason::QueueFailure,
            Self::InjectionFailure => LiveControlStopReason::InjectionFailure,
            Self::MessageLoopFailure => LiveControlStopReason::MessageLoopFailure,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveSignal {
    Pointer {
        sample: PointerSample,
        remote_position: Option<NormalizedMousePosition>,
    },
    Input(RemoteInputEvent),
    LocalTakeover,
}

impl LiveSignal {
    const fn is_coalescible_pointer(self) -> bool {
        matches!(self, Self::Pointer { .. })
    }

    fn coalesced_after(self, previous: Self) -> Self {
        match (previous, self) {
            (
                Self::Pointer {
                    sample: previous, ..
                },
                Self::Pointer {
                    mut sample,
                    remote_position,
                },
            ) => {
                // A high-polling-rate mouse can emit another packet after Windows has already
                // clamped one axis to an edge while the other axis keeps moving. Preserve the
                // unconsumed direction independently for each stationary axis. Requiring the
                // entire point to remain unchanged lets tangential motion erase the outward push
                // and makes boundary entry depend on the physical mouse report rate.
                if sample.position.x == previous.position.x && sample.delta_x == 0 {
                    sample.delta_x = previous.delta_x;
                }
                if sample.position.y == previous.position.y && sample.delta_y == 0 {
                    sample.delta_y = previous.delta_y;
                }
                Self::Pointer {
                    sample,
                    remote_position,
                }
            }
            (_, latest) => latest,
        }
    }
}

#[derive(Debug)]
struct LiveQueueState {
    signals: VecDeque<LiveSignal>,
    tail_pointer_protected: bool,
    closed: bool,
    peak_depth: usize,
    network_ready: bool,
}

#[derive(Debug)]
struct LiveQueue {
    state: Mutex<LiveQueueState>,
    ready: Condvar,
    coalesced_moves: AtomicU64,
    bypassed_moves: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveEnqueueResult {
    Queued,
    Coalesced,
    MouseMoveBypassed,
    Closed,
    Fatal,
}

impl LiveEnqueueResult {
    const fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Coalesced => "coalesced",
            Self::MouseMoveBypassed => "bypassed",
            Self::Closed => "closed",
            Self::Fatal => "fatal",
        }
    }
}

impl LiveQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(LiveQueueState {
                signals: VecDeque::with_capacity(LIVE_QUEUE_CAPACITY),
                tail_pointer_protected: false,
                closed: false,
                peak_depth: 0,
                network_ready: false,
            }),
            ready: Condvar::new(),
            coalesced_moves: AtomicU64::new(0),
            bypassed_moves: AtomicU64::new(0),
        }
    }

    fn push_pointer(&self, signal: LiveSignal) -> LiveEnqueueResult {
        self.push_pointer_with_protection(signal, false)
    }

    fn push_transition_pointer(&self, signal: LiveSignal) -> LiveEnqueueResult {
        self.push_pointer_with_protection(signal, true)
    }

    fn push_pointer_with_protection(
        &self,
        signal: LiveSignal,
        protect_from_later_moves: bool,
    ) -> LiveEnqueueResult {
        debug_assert!(signal.is_coalescible_pointer());
        let Ok(mut state) = self.state.lock() else {
            return LiveEnqueueResult::Fatal;
        };
        if state.closed {
            return LiveEnqueueResult::Closed;
        }
        let tail_pointer_protected = state.tail_pointer_protected;
        if let Some(previous) = state.signals.back_mut()
            && previous.is_coalescible_pointer()
            && !tail_pointer_protected
        {
            *previous = signal.coalesced_after(*previous);
            state.tail_pointer_protected = protect_from_later_moves;
            self.coalesced_moves.fetch_add(1, Ordering::Relaxed);
            self.ready.notify_one();
            return LiveEnqueueResult::Coalesced;
        }
        if state.signals.len() >= LIVE_QUEUE_CAPACITY {
            self.bypassed_moves.fetch_add(1, Ordering::Relaxed);
            return LiveEnqueueResult::MouseMoveBypassed;
        }
        state.signals.push_back(signal);
        state.tail_pointer_protected = protect_from_later_moves;
        state.peak_depth = state.peak_depth.max(state.signals.len());
        self.ready.notify_one();
        LiveEnqueueResult::Queued
    }

    fn push_inputs(&self, events: &[RemoteInputEvent]) -> LiveEnqueueResult {
        let Ok(mut state) = self.state.lock() else {
            return LiveEnqueueResult::Fatal;
        };
        if state.closed {
            return LiveEnqueueResult::Closed;
        }
        if events.len() > LIVE_QUEUE_CAPACITY {
            return LiveEnqueueResult::Fatal;
        }
        if state.signals.len().saturating_add(events.len()) > LIVE_QUEUE_CAPACITY {
            return LiveEnqueueResult::Fatal;
        }
        state
            .signals
            .extend(events.iter().copied().map(LiveSignal::Input));
        if !events.is_empty() {
            state.tail_pointer_protected = false;
        }
        state.peak_depth = state.peak_depth.max(state.signals.len());
        self.ready.notify_one();
        LiveEnqueueResult::Queued
    }

    fn push_local_takeover(&self) -> LiveEnqueueResult {
        let Ok(mut state) = self.state.lock() else {
            return LiveEnqueueResult::Fatal;
        };
        if state.closed {
            return LiveEnqueueResult::Closed;
        }
        if state.signals.len() >= LIVE_QUEUE_CAPACITY {
            return LiveEnqueueResult::Fatal;
        }
        state.signals.push_back(LiveSignal::LocalTakeover);
        state.tail_pointer_protected = false;
        state.peak_depth = state.peak_depth.max(state.signals.len());
        self.ready.notify_one();
        LiveEnqueueResult::Queued
    }

    fn pop_until(&self, deadline: Instant) -> Option<LiveSignal> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.network_ready {
                state.network_ready = false;
                return None;
            }
            if let Some(signal) = state.signals.pop_front() {
                if state.signals.is_empty() {
                    state.tail_pointer_protected = false;
                }
                return Some(signal);
            }
            if state.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next, timeout) = self
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timeout.timed_out() {
                return None;
            }
        }
    }

    fn notify_network(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.network_ready = true;
        self.ready.notify_one();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.signals.clear();
        state.tail_pointer_protected = false;
        self.ready.notify_all();
    }

    fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.signals.clear();
        state.tail_pointer_protected = false;
    }

    fn peak_depth(&self) -> usize {
        self.state.lock().map_or_else(
            |poisoned| poisoned.into_inner().peak_depth,
            |state| state.peak_depth,
        )
    }

    fn diagnostic_depth(&self) -> Option<usize> {
        self.state.try_lock().ok().map(|state| state.signals.len())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundaryProbeKind {
    Contact,
    ProcessedOutward,
    RawOutward,
    RawPredicted,
    RawNearUnmatched,
}

impl BoundaryProbeKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Contact => "edge_contact",
            Self::ProcessedOutward => "edge_processed_outward",
            Self::RawOutward => "edge_raw_outward",
            Self::RawPredicted => "edge_raw_predicted",
            Self::RawNearUnmatched => "edge_raw_near_unmatched",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct BoundaryProbeEvent {
    kind: BoundaryProbeKind,
    attempt: u64,
    observed_at: Instant,
    message_time_ms: u32,
    position_x: i32,
    position_y: i32,
    delta_x: i32,
    delta_y: i32,
    corner_safe: bool,
    queue_result: Option<LiveEnqueueResult>,
    queue_depth: Option<usize>,
    coalesced_moves: u64,
    bypassed_moves: u64,
}

#[derive(Debug, Default)]
struct BoundaryProbeState {
    events: VecDeque<BoundaryProbeEvent>,
    attempt: u64,
    contact_active: bool,
    processed_outward_recorded: bool,
    raw_outward_recorded: bool,
    contact_started_at: Option<Instant>,
    processed_outward_at: Option<Instant>,
    raw_outward_at: Option<Instant>,
    dropped: u64,
}

impl BoundaryProbeState {
    fn reset_contact(&mut self) {
        self.contact_active = false;
        self.processed_outward_recorded = false;
        self.raw_outward_recorded = false;
        self.contact_started_at = None;
        self.processed_outward_at = None;
        self.raw_outward_at = None;
    }

    fn begin_contact(&mut self, observed_at: Instant) -> u64 {
        if !self.contact_active {
            self.attempt = self.attempt.saturating_add(1);
            self.contact_active = true;
            self.processed_outward_recorded = false;
            self.raw_outward_recorded = false;
            self.contact_started_at = Some(observed_at);
            self.processed_outward_at = None;
            self.raw_outward_at = None;
        }
        self.attempt
    }

    fn push(&mut self, event: BoundaryProbeEvent) {
        if self.events.len() == BOUNDARY_PROBE_CAPACITY {
            self.events.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.events.push_back(event);
    }
}

fn point_is_on_probe_boundary(
    x: i32,
    y: i32,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
) -> bool {
    match boundary {
        ScreenBoundary::Left => x == 0,
        ScreenBoundary::Right => {
            x == i32::try_from(screen.width - 1).expect("screen width fits i32")
        }
        ScreenBoundary::Top => y == 0,
        ScreenBoundary::Bottom => {
            y == i32::try_from(screen.height - 1).expect("screen height fits i32")
        }
    }
}

const fn probe_delta_is_outward(delta_x: i32, delta_y: i32, boundary: ScreenBoundary) -> bool {
    match boundary {
        ScreenBoundary::Left => delta_x < 0,
        ScreenBoundary::Right => delta_x > 0,
        ScreenBoundary::Top => delta_y < 0,
        ScreenBoundary::Bottom => delta_y > 0,
    }
}

fn distance_to_probe_boundary(
    x: i32,
    y: i32,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
) -> Option<u32> {
    let right = i32::try_from(screen.width - 1).ok()?;
    let bottom = i32::try_from(screen.height - 1).ok()?;
    let distance = match boundary {
        ScreenBoundary::Left => x,
        ScreenBoundary::Right => right.saturating_sub(x),
        ScreenBoundary::Top => y,
        ScreenBoundary::Bottom => bottom.saturating_sub(y),
    };
    u32::try_from(distance).ok()
}

fn probe_position_is_corner_safe(
    x: i32,
    y: i32,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
    guard: u32,
) -> bool {
    let offset = match boundary {
        ScreenBoundary::Left | ScreenBoundary::Right => y,
        ScreenBoundary::Top | ScreenBoundary::Bottom => x,
    };
    let extent = match boundary {
        ScreenBoundary::Left | ScreenBoundary::Right => screen.height,
        ScreenBoundary::Top | ScreenBoundary::Bottom => screen.width,
    };
    u32::try_from(offset).is_ok_and(|offset| {
        guard.saturating_mul(2) < extent && offset >= guard && offset < extent.saturating_sub(guard)
    })
}

#[derive(Clone, Copy, Debug)]
struct RemotePointerState {
    screen: PhysicalScreenSize,
    position: PhysicalPoint,
    entry_raw_time_cutoff: Option<u32>,
    gain: PointerGainModel,
    residual_x: i64,
    residual_y: i64,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessedPointerSample {
    message_time_ms: u32,
    delta_x: i32,
    delta_y: i32,
    x_clamped: bool,
    y_clamped: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EdgeOverflowCandidate {
    message_time_ms: u32,
    pixel_x: i32,
    pixel_y: i32,
}

#[derive(Clone, Copy, Debug, Default)]
struct AxisGainSample {
    raw_distance: u64,
    processed_distance: u64,
}

impl AxisGainSample {
    fn observe(&mut self, raw: i32, processed: i32, clamped: bool) {
        if clamped || raw == 0 || (processed != 0 && raw.signum() != processed.signum()) {
            return;
        }
        self.raw_distance = self
            .raw_distance
            .saturating_add(u64::from(raw.unsigned_abs()));
        self.processed_distance = self
            .processed_distance
            .saturating_add(u64::from(processed.unsigned_abs()));
        if self.raw_distance > 16_384 {
            self.raw_distance = self.raw_distance.div_ceil(2);
            self.processed_distance = self.processed_distance.div_ceil(2);
        }
    }

    fn gain(self) -> Option<u64> {
        (self.raw_distance >= POINTER_GAIN_MIN_RAW_DISTANCE).then(|| {
            self.processed_distance
                .saturating_mul(POINTER_GAIN_SCALE.cast_unsigned())
                .div_ceil(self.raw_distance)
                .clamp(POINTER_GAIN_MIN, POINTER_GAIN_MAX)
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct PointerGainModel {
    x: [u64; POINTER_GAIN_BIN_COUNT],
    y: [u64; POINTER_GAIN_BIN_COUNT],
}

impl Default for PointerGainModel {
    fn default() -> Self {
        Self {
            x: [POINTER_GAIN_SCALE.cast_unsigned(); POINTER_GAIN_BIN_COUNT],
            y: [POINTER_GAIN_SCALE.cast_unsigned(); POINTER_GAIN_BIN_COUNT],
        }
    }
}

impl PointerGainModel {
    fn scale_x(self, delta: i32, residual: &mut i64) -> i32 {
        scale_pointer_axis(delta, self.x[pointer_gain_bin(delta)], residual)
    }

    fn scale_y(self, delta: i32, residual: &mut i64) -> i32 {
        scale_pointer_axis(delta, self.y[pointer_gain_bin(delta)], residual)
    }
}

fn raw_boundary_transition_sample(
    current: (i32, i32),
    raw_delta: (i32, i32),
    gain: PointerGainModel,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
    corner_guard_pixels: u32,
) -> Option<PointerSample> {
    let (current_x, current_y) = current;
    let (raw_delta_x, raw_delta_y) = raw_delta;
    if !probe_delta_is_outward(raw_delta_x, raw_delta_y, boundary)
        || current_x < 0
        || current_y < 0
        || current_x >= i32::try_from(screen.width).ok()?
        || current_y >= i32::try_from(screen.height).ok()?
    {
        return None;
    }

    let mut residual_x = 0;
    let mut residual_y = 0;
    let scaled_x = gain.scale_x(raw_delta_x, &mut residual_x);
    let scaled_y = gain.scale_y(raw_delta_y, &mut residual_y);
    scaled_boundary_transition_sample(
        current_x,
        current_y,
        scaled_x,
        scaled_y,
        screen,
        boundary,
        corner_guard_pixels,
    )
}

fn prepared_boundary_transition_sample(
    current: (i32, i32),
    scaled_delta: (i32, i32),
    screen: PhysicalScreenSize,
    map: &PreparedBoundaryMap,
) -> Option<(PointerSample, ScreenBoundary)> {
    let motion = PointerMotion {
        from_x: i64::from(current.0),
        from_y: i64::from(current.1),
        to_x: i64::from(current.0).saturating_add(i64::from(scaled_delta.0)),
        to_y: i64::from(current.1).saturating_add(i64::from(scaled_delta.1)),
    };
    let hit = map.hit_test_motion(motion).ok().flatten()?;
    let sample = scaled_boundary_transition_sample(
        current.0,
        current.1,
        scaled_delta.0,
        scaled_delta.1,
        screen,
        hit.edge,
        0,
    )?;
    Some((sample, hit.edge))
}

fn raw_boundary_overflow(
    current: (i32, i32),
    raw_delta: (i32, i32),
    gain: PointerGainModel,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
    message_time_ms: u32,
) -> Option<EdgeOverflowCandidate> {
    let mut residual_x = 0;
    let mut residual_y = 0;
    let scaled_x = gain.scale_x(raw_delta.0, &mut residual_x);
    let scaled_y = gain.scale_y(raw_delta.1, &mut residual_y);
    let right = i32::try_from(screen.width - 1).ok()?;
    let bottom = i32::try_from(screen.height - 1).ok()?;
    let (pixel_x, pixel_y) = match boundary {
        ScreenBoundary::Left => (scaled_x.saturating_add(current.0), 0),
        ScreenBoundary::Right => (scaled_x.saturating_sub(right.saturating_sub(current.0)), 0),
        ScreenBoundary::Top => (0, scaled_y.saturating_add(current.1)),
        ScreenBoundary::Bottom => (0, scaled_y.saturating_sub(bottom.saturating_sub(current.1))),
    };
    probe_delta_is_outward(pixel_x, pixel_y, boundary).then_some(EdgeOverflowCandidate {
        message_time_ms,
        pixel_x,
        pixel_y,
    })
}

fn scaled_boundary_transition_sample(
    current_x: i32,
    current_y: i32,
    scaled_x: i32,
    scaled_y: i32,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
    corner_guard_pixels: u32,
) -> Option<PointerSample> {
    if !probe_delta_is_outward(scaled_x, scaled_y, boundary)
        || current_x < 0
        || current_y < 0
        || current_x >= i32::try_from(screen.width).ok()?
        || current_y >= i32::try_from(screen.height).ok()?
    {
        return None;
    }
    let right = i32::try_from(screen.width - 1).ok()?;
    let bottom = i32::try_from(screen.height - 1).ok()?;

    let (exit_distance, exit_delta, tangent_position, tangent_delta, horizontal) = match boundary {
        ScreenBoundary::Left => (current_x, scaled_x, current_y, scaled_y, true),
        ScreenBoundary::Right => (
            right.saturating_sub(current_x),
            scaled_x,
            current_y,
            scaled_y,
            true,
        ),
        ScreenBoundary::Top => (current_y, scaled_y, current_x, scaled_x, false),
        ScreenBoundary::Bottom => (
            bottom.saturating_sub(current_y),
            scaled_y,
            current_x,
            scaled_x,
            false,
        ),
    };
    let exit_magnitude = i64::from(exit_delta).abs();
    if exit_magnitude < i64::from(exit_distance) || exit_magnitude == 0 {
        return None;
    }

    // Map the tangential coordinate to the point where this raw packet intersects the boundary,
    // rather than to the packet's final projected point. This prevents a fast diagonal movement
    // from being shifted sideways or from bypassing the corner guard after the crossing.
    let tangent_at_crossing = i64::from(tangent_position).saturating_add(
        i64::from(tangent_delta)
            .saturating_mul(i64::from(exit_distance))
            .checked_div(exit_magnitude)
            .unwrap_or_default(),
    );
    let tangent_limit = i64::from(if horizontal { bottom } else { right });
    let tangent_at_crossing = i32::try_from(tangent_at_crossing.clamp(0, tangent_limit)).ok()?;
    let (x, y) = match boundary {
        ScreenBoundary::Left => (0, tangent_at_crossing),
        ScreenBoundary::Right => (right, tangent_at_crossing),
        ScreenBoundary::Top => (tangent_at_crossing, 0),
        ScreenBoundary::Bottom => (tangent_at_crossing, bottom),
    };
    if !probe_position_is_corner_safe(x, y, screen, boundary, corner_guard_pixels) {
        return None;
    }

    let mut delta_x = x.saturating_sub(current_x);
    let mut delta_y = y.saturating_sub(current_y);
    // When the processed cursor is already clamped on the edge, preserve the raw outward intent
    // with a one-pixel logical delta so the state machine still observes a crossing.
    match boundary {
        ScreenBoundary::Left if delta_x == 0 => delta_x = -1,
        ScreenBoundary::Right if delta_x == 0 => delta_x = 1,
        ScreenBoundary::Top if delta_y == 0 => delta_y = -1,
        ScreenBoundary::Bottom if delta_y == 0 => delta_y = 1,
        _ => {}
    }
    Some(PointerSample {
        position: PhysicalPoint {
            x: u32::try_from(x).ok()?,
            y: u32::try_from(y).ok()?,
        },
        delta_x,
        delta_y,
    })
}

#[derive(Debug, Default)]
struct PointerGainCalibration {
    pending_processed: VecDeque<ProcessedPointerSample>,
    x: [AxisGainSample; POINTER_GAIN_BIN_COUNT],
    y: [AxisGainSample; POINTER_GAIN_BIN_COUNT],
}

impl PointerGainCalibration {
    fn record_processed(&mut self, sample: ProcessedPointerSample) {
        if self.pending_processed.len() == POINTER_GAIN_HISTORY_LIMIT {
            self.pending_processed.pop_front();
        }
        self.pending_processed.push_back(sample);
    }

    fn record_raw_with_match(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        message_time_ms: u32,
    ) -> (bool, Option<EdgeOverflowCandidate>) {
        let Some(index) = self
            .pending_processed
            .iter()
            .position(|sample| message_time_ms.wrapping_sub(sample.message_time_ms) <= 2)
        else {
            return (false, None);
        };
        for _ in 0..index {
            self.pending_processed.pop_front();
        }
        let Some(processed) = self.pending_processed.pop_front() else {
            return (false, None);
        };
        let model = self.model();
        let pixel_x = edge_overflow_pixels(
            delta_x,
            processed.delta_x,
            processed.x_clamped,
            model.x[pointer_gain_bin(delta_x)],
        );
        let pixel_y = edge_overflow_pixels(
            delta_y,
            processed.delta_y,
            processed.y_clamped,
            model.y[pointer_gain_bin(delta_y)],
        );
        self.x[pointer_gain_bin(delta_x)].observe(delta_x, processed.delta_x, processed.x_clamped);
        self.y[pointer_gain_bin(delta_y)].observe(delta_y, processed.delta_y, processed.y_clamped);
        (
            true,
            (pixel_x != 0 || pixel_y != 0).then_some(EdgeOverflowCandidate {
                message_time_ms,
                pixel_x,
                pixel_y,
            }),
        )
    }

    fn model(&self) -> PointerGainModel {
        PointerGainModel {
            x: resolved_axis_gains(&self.x),
            y: resolved_axis_gains(&self.y),
        }
    }
}

fn edge_overflow_pixels(raw: i32, processed: i32, clamped: bool, gain: u64) -> i32 {
    if !clamped || raw == 0 {
        return 0;
    }
    let mut residual = 0;
    let overflow = scale_pointer_axis(raw, gain, &mut residual).saturating_sub(processed);
    if overflow != 0 && overflow.signum() == raw.signum() {
        overflow
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PendingEnterMotion {
    cutoff_ms: Option<u32>,
    boundary: Option<ScreenBoundary>,
    x_by_gain_bin: [i64; POINTER_GAIN_BIN_COUNT],
    y_by_gain_bin: [i64; POINTER_GAIN_BIN_COUNT],
    initial_pixel_x: i32,
    initial_pixel_y: i32,
    edge_overflow_applied: bool,
    packets: u32,
}

impl PendingEnterMotion {
    fn begin(&mut self, cutoff_ms: u32, boundary: ScreenBoundary) {
        *self = Self {
            cutoff_ms: Some(cutoff_ms),
            boundary: Some(boundary),
            ..Self::default()
        };
    }

    fn apply_edge_overflow(&mut self, candidate: EdgeOverflowCandidate) {
        let (Some(cutoff_ms), Some(boundary)) = (self.cutoff_ms, self.boundary) else {
            return;
        };
        if self.edge_overflow_applied
            || tick_is_strictly_after(candidate.message_time_ms, cutoff_ms)
            || cutoff_ms.wrapping_sub(candidate.message_time_ms)
                > PENDING_ENTER_EDGE_OVERFLOW_MAX_AGE_MS
        {
            return;
        }
        match boundary {
            ScreenBoundary::Left if candidate.pixel_x < 0 => {
                self.initial_pixel_x = candidate.pixel_x;
            }
            ScreenBoundary::Right if candidate.pixel_x > 0 => {
                self.initial_pixel_x = candidate.pixel_x;
            }
            ScreenBoundary::Top if candidate.pixel_y < 0 => {
                self.initial_pixel_y = candidate.pixel_y;
            }
            ScreenBoundary::Bottom if candidate.pixel_y > 0 => {
                self.initial_pixel_y = candidate.pixel_y;
            }
            _ => return,
        }
        self.edge_overflow_applied = true;
    }

    fn record(&mut self, delta_x: i32, delta_y: i32, message_time_ms: u32) {
        let Some(cutoff_ms) = self.cutoff_ms else {
            return;
        };
        if !tick_is_strictly_after(message_time_ms, cutoff_ms) {
            return;
        }
        let x_bin = pointer_gain_bin(delta_x);
        let y_bin = pointer_gain_bin(delta_y);
        self.x_by_gain_bin[x_bin] = self.x_by_gain_bin[x_bin]
            .saturating_add(i64::from(delta_x))
            .clamp(-PENDING_ENTER_RAW_AXIS_LIMIT, PENDING_ENTER_RAW_AXIS_LIMIT);
        self.y_by_gain_bin[y_bin] = self.y_by_gain_bin[y_bin]
            .saturating_add(i64::from(delta_y))
            .clamp(-PENDING_ENTER_RAW_AXIS_LIMIT, PENDING_ENTER_RAW_AXIS_LIMIT);
        self.packets = self.packets.saturating_add(1);
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }

    fn scale(
        self,
        gain: PointerGainModel,
        residual_x: &mut i64,
        residual_y: &mut i64,
    ) -> (i32, i32) {
        let mut delta_x = self.initial_pixel_x;
        let mut delta_y = self.initial_pixel_y;
        for index in 0..POINTER_GAIN_BIN_COUNT {
            let raw_x = i32::try_from(self.x_by_gain_bin[index])
                .expect("bounded pending x movement fits i32");
            let raw_y = i32::try_from(self.y_by_gain_bin[index])
                .expect("bounded pending y movement fits i32");
            delta_x = delta_x.saturating_add(scale_pointer_axis(raw_x, gain.x[index], residual_x));
            delta_y = delta_y.saturating_add(scale_pointer_axis(raw_y, gain.y[index], residual_y));
        }
        (delta_x, delta_y)
    }

    fn raw_totals(self) -> (i64, i64) {
        (
            self.x_by_gain_bin.into_iter().sum(),
            self.y_by_gain_bin.into_iter().sum(),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EnterCarryTrace {
    packets: u32,
    raw_x: i64,
    raw_y: i64,
    pixel_x: i32,
    pixel_y: i32,
}

fn pointer_gain_bin(delta: i32) -> usize {
    match delta.unsigned_abs() {
        0..=2 => 0,
        3..=6 => 1,
        7..=16 => 2,
        _ => 3,
    }
}

fn resolved_axis_gains(
    samples: &[AxisGainSample; POINTER_GAIN_BIN_COUNT],
) -> [u64; POINTER_GAIN_BIN_COUNT] {
    let mut gains = [POINTER_GAIN_SCALE.cast_unsigned(); POINTER_GAIN_BIN_COUNT];
    for (index, output) in gains.iter_mut().enumerate() {
        *output = samples[index]
            .gain()
            .or_else(|| {
                (1..POINTER_GAIN_BIN_COUNT).find_map(|distance| {
                    index
                        .checked_sub(distance)
                        .and_then(|candidate| samples[candidate].gain())
                        .or_else(|| {
                            samples
                                .get(index + distance)
                                .and_then(|sample| sample.gain())
                        })
                })
            })
            .unwrap_or(POINTER_GAIN_SCALE.cast_unsigned());
    }
    gains
}

fn scale_pointer_axis(delta: i32, gain: u64, residual: &mut i64) -> i32 {
    let scaled = i64::from(delta)
        .saturating_mul(i64::try_from(gain).expect("bounded pointer gain fits i64"))
        .saturating_add(*residual);
    let pixels = scaled / POINTER_GAIN_SCALE;
    *residual = scaled % POINTER_GAIN_SCALE;
    i32::try_from(pixels.clamp(i64::from(i32::MIN), i64::from(i32::MAX)))
        .expect("clamped scaled pointer delta fits i32")
}

#[derive(Debug, Default)]
struct LocalTakeoverMotion {
    origin: Option<(i32, i32, Instant)>,
    samples: u8,
    distance: u32,
}

impl LocalTakeoverMotion {
    fn observe(&mut self, x: i32, y: i32, now: Instant) -> bool {
        let Some((origin_x, origin_y, started_at)) = self.origin else {
            self.restart(x, y, now);
            return false;
        };
        if now.saturating_duration_since(started_at) > LOCAL_TAKEOVER_MOVE_WINDOW {
            self.restart(x, y, now);
            return false;
        }
        self.samples = self.samples.saturating_add(1);
        self.distance = self
            .distance
            .max(x.abs_diff(origin_x).saturating_add(y.abs_diff(origin_y)));
        let intentional = self.samples >= LOCAL_TAKEOVER_MOVE_MIN_SAMPLES
            && self.distance >= LOCAL_TAKEOVER_MOVE_DISTANCE;
        if intentional {
            self.reset();
        }
        intentional
    }

    fn restart(&mut self, x: i32, y: i32, now: Instant) {
        self.origin = Some((x, y, now));
        self.samples = 1;
        self.distance = 0;
    }

    fn reset(&mut self) {
        self.origin = None;
        self.samples = 0;
        self.distance = 0;
    }
}

#[derive(Debug, Default)]
struct TransitionLatency {
    samples: u64,
    total_micros: u64,
    max_micros: u64,
}

impl TransitionLatency {
    fn record(&mut self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.samples = self.samples.saturating_add(1);
        self.total_micros = self.total_micros.saturating_add(micros);
        self.max_micros = self.max_micros.max(micros);
    }

    fn average_micros(&self) -> u64 {
        self.total_micros.checked_div(self.samples).unwrap_or(0)
    }
}

#[derive(Debug, Default)]
struct LiveStats {
    control_entries: u64,
    control_returns: u64,
    rejected_input_epochs: u64,
    ignored_stale_acknowledgements: u64,
    enter_latency: TransitionLatency,
    return_latency: TransitionLatency,
}

#[derive(Debug, Default)]
struct ForwardingState {
    machine: InputStateMachine,
    pending_modifiers: Vec<InputEvent>,
}

struct ForwardingOutcome {
    events: Vec<RemoteInputEvent>,
    suppress_current: bool,
}

impl ForwardingState {
    fn prepare(&mut self, event: InputEvent) -> Result<ForwardingOutcome, StateDecision> {
        if is_modifier_key(event) {
            return self.prepare_modifier(event);
        }

        let mut events = std::mem::take(&mut self.pending_modifiers);
        events.push(event);
        self.forward_batch(&events)
    }

    fn prepare_modifier(&mut self, event: InputEvent) -> Result<ForwardingOutcome, StateDecision> {
        let InputEvent::Key {
            physical,
            transition,
            ..
        } = event
        else {
            unreachable!("modifier events are keyboard events");
        };
        match transition {
            KeyTransition::Down => {
                if self.pending_modifiers.iter().any(|pending| {
                    matches!(pending, InputEvent::Key { physical: pending_physical, .. } if *pending_physical == physical)
                }) {
                    return Ok(ForwardingOutcome {
                        events: Vec::new(),
                        suppress_current: true,
                    });
                }
                if self.pending_modifiers.len() >= MAX_PENDING_EMERGENCY_MODIFIERS {
                    return Err(StateDecision::CapacityExceeded);
                }
                self.pending_modifiers.push(event);
                Ok(ForwardingOutcome {
                    events: Vec::new(),
                    suppress_current: true,
                })
            }
            KeyTransition::Up => {
                if self.pending_modifiers.iter().any(|pending| {
                    matches!(pending, InputEvent::Key { physical: pending_physical, .. } if *pending_physical == physical)
                }) {
                    let mut events = std::mem::take(&mut self.pending_modifiers);
                    events.push(event);
                    self.forward_batch(&events)
                } else {
                    self.forward_batch(&[event])
                }
            }
        }
    }

    fn forward_batch(&mut self, events: &[InputEvent]) -> Result<ForwardingOutcome, StateDecision> {
        let mut remote = Vec::with_capacity(events.len());
        let mut suppress_current = false;
        for (index, event) in events.iter().copied().enumerate() {
            match self.machine.handle(event) {
                StateDecision::Forward => {
                    remote.push(
                        remote_event_from_input(event)
                            .expect("reliable forwarded input converts to a remote event"),
                    );
                    if index + 1 == events.len() {
                        suppress_current = true;
                    }
                }
                StateDecision::PassThrough => {}
                StateDecision::EmergencyReturn => return Err(StateDecision::EmergencyReturn),
                StateDecision::CapacityExceeded => return Err(StateDecision::CapacityExceeded),
            }
        }
        Ok(ForwardingOutcome {
            events: remote,
            suppress_current,
        })
    }

    fn release_all(&mut self) -> Vec<InputEvent> {
        self.pending_modifiers.clear();
        self.machine.release_all()
    }

    fn take_local_restore_keys(&mut self) -> Vec<InputEvent> {
        let mut restore = self.pending_modifiers.drain(..).collect::<Vec<_>>();
        let mut tracked = self
            .machine
            .release_all()
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Key {
                    physical,
                    virtual_key,
                    ..
                } if virtual_key != VK_NUMLOCK_VALUE => Some(InputEvent::Key {
                    physical,
                    virtual_key,
                    transition: KeyTransition::Down,
                }),
                InputEvent::Key { .. }
                | InputEvent::MouseButton { .. }
                | InputEvent::MouseMove { .. }
                | InputEvent::Wheel { .. } => None,
            })
            .collect::<Vec<_>>();
        tracked.reverse();
        restore.extend(tracked);
        restore
    }

    const fn stats(&self) -> InputEventStats {
        self.machine.stats()
    }

    const fn outstanding_count(&self) -> usize {
        self.machine.outstanding_count()
    }
}

#[derive(Clone, Debug)]
struct LogicalRuntimeTransfer {
    transition: LogicalBoundaryTransition,
    departure_boundary: ScreenBoundary,
}

fn portal_point_for_sample(
    sample: PointerSample,
    screen: PhysicalScreenSize,
    edge: ScreenBoundary,
) -> Option<PortalPoint> {
    let x = i32::try_from(sample.position.x).ok()?;
    let y = i32::try_from(sample.position.y).ok()?;
    if !point_is_on_probe_boundary(x, y, screen, edge)
        || !probe_delta_is_outward(sample.delta_x, sample.delta_y, edge)
    {
        return None;
    }
    let (coordinate, extent) = match edge {
        ScreenBoundary::Left | ScreenBoundary::Right => (y, i32::try_from(screen.height).ok()?),
        ScreenBoundary::Top | ScreenBoundary::Bottom => (x, i32::try_from(screen.width).ok()?),
    };
    Some(PortalPoint {
        edge,
        normalized_offset: u32::from(normalize_physical_coordinate(coordinate, extent)),
    })
}

fn portal_point_to_physical(point: PortalPoint, screen: PhysicalScreenSize) -> PhysicalPoint {
    map_normalized_boundary_position(
        u16::try_from(point.normalized_offset.min(u32::from(u16::MAX)))
            .expect("clamped portal offset fits u16"),
        point.edge,
        screen,
        0,
    )
}

struct LiveContext {
    drag: Option<Arc<crate::clipboard::drag_live::DragBridge>>,
    drag_retry: Mutex<Option<LiveSignal>>,
    suppress_drag_left_restore: AtomicBool,
    local_source_drag_return_eligible: AtomicBool,
    remote_left_down: AtomicBool,
    preserve_next_drag_release: AtomicBool,
    route: AtomicU8,
    active_epoch: AtomicU64,
    stop: AtomicU8,
    failure: Mutex<Option<String>>,
    hook_thread_id: u32,
    local_screen: PhysicalScreenSize,
    local_refresh_hz: u32,
    departure_boundary: ScreenBoundary,
    corner_guard_pixels: u32,
    boundary_map: Option<PreparedBoundaryMap>,
    logical_boundary_layout: Option<CompiledBoundaryLayout>,
    logical_peer_fingerprint: String,
    logical_peer_screen: Option<PhysicalScreenSize>,
    trace_control: Option<Arc<LiveSessionControl>>,
    previous_x: Mutex<i32>,
    previous_y: Mutex<i32>,
    last_local_x: AtomicI32,
    last_local_y: AtomicI32,
    boundary_probe: Mutex<BoundaryProbeState>,
    last_raw_boundary_diagnostic_tick: AtomicU32,
    local_transition_pending: AtomicBool,
    source_first_remote_move_pending: AtomicBool,
    source_first_remote_move_sent_pending: AtomicBool,
    target_first_remote_move_pending: AtomicBool,
    target_first_remote_move_injection_pending: AtomicBool,
    target_first_remote_move_received_at: Mutex<Option<Instant>>,
    pointer_gain: Mutex<PointerGainCalibration>,
    pending_enter_motion: Mutex<PendingEnterMotion>,
    recent_edge_overflow: Mutex<Option<EdgeOverflowCandidate>>,
    last_enter_carry: Mutex<EnterCarryTrace>,
    local_pointer_needs_baseline: AtomicBool,
    remote_pointer: Mutex<Option<RemotePointerState>>,
    local_resume: Mutex<Option<PhysicalPoint>>,
    return_requested_at: Mutex<Option<Instant>>,
    forwarded: Mutex<ForwardingState>,
    modifier_bits: AtomicU64,
    local_takeover_pending: AtomicBool,
    local_takeover_motion: Mutex<LocalTakeoverMotion>,
    local_button_mask: AtomicU8,
    inherited_remote_button_mask: AtomicU8,
    inherited_button_downs: AtomicU64,
    inherited_button_ups: AtomicU64,
    forwarded_mouse_moves: AtomicU64,
    target_injection_timing: TargetInjectionTiming,
    queue: LiveQueue,
    stats: Mutex<LiveStats>,
}

impl LiveContext {
    #[allow(clippy::too_many_arguments)]
    fn new_with_boundary_layout(
        hook_thread_id: u32,
        local_screen: PhysicalScreenSize,
        policy: BoundaryPolicy,
        trace_control: Option<Arc<LiveSessionControl>>,
        boundary_map: Option<PreparedBoundaryMap>,
        logical_boundary_layout: Option<CompiledBoundaryLayout>,
        logical_peer_fingerprint: String,
        logical_peer_screen: Option<PhysicalScreenSize>,
    ) -> io::Result<Self> {
        let cursor = PhysicalPrimaryScreen::current()?.sample_cursor()?.map_or(
            PhysicalPoint { x: 0, y: 0 },
            |position| PhysicalPoint {
                x: u32::try_from(denormalize_physical_coordinate(
                    position.x,
                    i32::try_from(local_screen.width).expect("screen width fits i32"),
                ))
                .expect("cursor x is nonnegative"),
                y: u32::try_from(denormalize_physical_coordinate(
                    position.y,
                    i32::try_from(local_screen.height).expect("screen height fits i32"),
                ))
                .expect("cursor y is nonnegative"),
            },
        );
        Ok(Self {
            drag: None,
            drag_retry: Mutex::new(None),
            suppress_drag_left_restore: AtomicBool::new(false),
            local_source_drag_return_eligible: AtomicBool::new(false),
            remote_left_down: AtomicBool::new(false),
            preserve_next_drag_release: AtomicBool::new(false),
            route: AtomicU8::new(LiveRoute::Local as u8),
            active_epoch: AtomicU64::new(0),
            stop: AtomicU8::new(LiveStopCode::Running as u8),
            failure: Mutex::new(None),
            hook_thread_id,
            local_screen,
            local_refresh_hz: ScreenExtent::current_primary_refresh_hz().unwrap_or(0),
            departure_boundary: policy.peer_placement.local_exit_boundary(),
            corner_guard_pixels: policy.corner_guard_pixels,
            boundary_map,
            logical_boundary_layout,
            logical_peer_fingerprint,
            logical_peer_screen,
            trace_control,
            previous_x: Mutex::new(i32::try_from(cursor.x).expect("cursor x fits i32")),
            previous_y: Mutex::new(i32::try_from(cursor.y).expect("cursor y fits i32")),
            last_local_x: AtomicI32::new(i32::try_from(cursor.x).expect("cursor x fits i32")),
            last_local_y: AtomicI32::new(i32::try_from(cursor.y).expect("cursor y fits i32")),
            boundary_probe: Mutex::new(BoundaryProbeState::default()),
            last_raw_boundary_diagnostic_tick: AtomicU32::new(0),
            local_transition_pending: AtomicBool::new(false),
            source_first_remote_move_pending: AtomicBool::new(false),
            source_first_remote_move_sent_pending: AtomicBool::new(false),
            target_first_remote_move_pending: AtomicBool::new(false),
            target_first_remote_move_injection_pending: AtomicBool::new(false),
            target_first_remote_move_received_at: Mutex::new(None),
            pointer_gain: Mutex::new(PointerGainCalibration::default()),
            pending_enter_motion: Mutex::new(PendingEnterMotion::default()),
            recent_edge_overflow: Mutex::new(None),
            last_enter_carry: Mutex::new(EnterCarryTrace::default()),
            local_pointer_needs_baseline: AtomicBool::new(false),
            remote_pointer: Mutex::new(None),
            local_resume: Mutex::new(None),
            return_requested_at: Mutex::new(None),
            forwarded: Mutex::new(ForwardingState::default()),
            modifier_bits: AtomicU64::new(0),
            local_takeover_pending: AtomicBool::new(false),
            local_takeover_motion: Mutex::new(LocalTakeoverMotion::default()),
            local_button_mask: AtomicU8::new(0),
            inherited_remote_button_mask: AtomicU8::new(0),
            inherited_button_downs: AtomicU64::new(0),
            inherited_button_ups: AtomicU64::new(0),
            forwarded_mouse_moves: AtomicU64::new(0),
            target_injection_timing: TargetInjectionTiming::default(),
            queue: LiveQueue::new(),
            stats: Mutex::new(LiveStats::default()),
        })
    }

    fn local_departure_for_sample(&self, sample: PointerSample) -> Option<ScreenBoundary> {
        if self.logical_boundary_layout.is_some() {
            return self
                .logical_transfer_for_sample(ControlProtocolState::Local, sample)
                .map(|transfer| transfer.departure_boundary);
        }
        if let Some(map) = &self.boundary_map {
            return map
                .hit_test_sample(sample)
                .ok()
                .flatten()
                .map(|hit| hit.edge);
        }
        let boundary = self.departure_boundary;
        (point_is_on_probe_boundary(
            i32::try_from(sample.position.x).ok()?,
            i32::try_from(sample.position.y).ok()?,
            self.local_screen,
            boundary,
        ) && probe_delta_is_outward(sample.delta_x, sample.delta_y, boundary)
            && probe_position_is_corner_safe(
                i32::try_from(sample.position.x).ok()?,
                i32::try_from(sample.position.y).ok()?,
                self.local_screen,
                boundary,
                self.corner_guard_pixels,
            ))
        .then_some(boundary)
    }

    fn logical_transfer_for_sample(
        &self,
        state: ControlProtocolState,
        sample: PointerSample,
    ) -> Option<LogicalRuntimeTransfer> {
        let layout = self.logical_boundary_layout.as_ref()?;
        let screen = match state {
            ControlProtocolState::Local => self.local_screen,
            ControlProtocolState::ControllingPeer => self.peer_screen_for_logical_layout()?,
            _ => return None,
        };
        for edge in [
            ScreenBoundary::Left,
            ScreenBoundary::Right,
            ScreenBoundary::Top,
            ScreenBoundary::Bottom,
        ] {
            let Some(point) = portal_point_for_sample(sample, screen, edge) else {
                continue;
            };
            for boundary in layout
                .boundaries()
                .iter()
                .filter(|boundary| boundary.peer_fingerprint() == self.logical_peer_fingerprint)
            {
                let transfer = match state {
                    ControlProtocolState::Local => boundary.source_to_target(point),
                    ControlProtocolState::ControllingPeer => boundary.target_to_source(point),
                    _ => None,
                };
                if let Some(transfer) = transfer {
                    return Some(LogicalRuntimeTransfer {
                        transition: LogicalBoundaryTransition {
                            boundary_id: boundary.boundary_id().to_owned(),
                            mapping_revision: boundary.mapping_revision(),
                            parameter: transfer.parameter,
                        },
                        departure_boundary: edge,
                    });
                }
            }
        }
        None
    }

    fn peer_screen_for_logical_layout(&self) -> Option<PhysicalScreenSize> {
        self.logical_peer_screen
    }

    fn logical_destination_for_request(
        &self,
        request: &super::channel::ControlTransitionRequest,
        entering_peer: bool,
    ) -> Option<PhysicalPoint> {
        let logical = request.logical_boundary.as_ref()?;
        let boundary = self
            .logical_boundary_layout
            .as_ref()?
            .boundary(&logical.boundary_id)?;
        if boundary.peer_fingerprint() != self.logical_peer_fingerprint
            || boundary.mapping_revision() != logical.mapping_revision
        {
            return None;
        }
        let parameter = if boundary.reversed() {
            u16::MAX - logical.parameter
        } else {
            logical.parameter
        };
        let point = if entering_peer {
            boundary.target().point_at_parameter(parameter)
        } else {
            boundary.source().point_at_parameter(parameter)
        };
        let screen = if entering_peer {
            self.peer_screen_for_logical_layout()?
        } else {
            self.local_screen
        };
        Some(portal_point_to_physical(point, screen))
    }

    fn logical_inbound_arrival(
        &self,
        request: &super::channel::ControlTransitionRequest,
    ) -> Option<PhysicalPoint> {
        let logical = request.logical_boundary.as_ref()?;
        let boundary = self
            .logical_boundary_layout
            .as_ref()?
            .boundary(&logical.boundary_id)?;
        if boundary.peer_fingerprint() != self.logical_peer_fingerprint
            || boundary.mapping_revision() != logical.mapping_revision
        {
            return None;
        }
        let parameter = if boundary.reversed() {
            u16::MAX - logical.parameter
        } else {
            logical.parameter
        };
        Some(portal_point_to_physical(
            boundary.source().point_at_parameter(parameter),
            self.local_screen,
        ))
    }

    fn logical_request_is_valid(&self, request: &super::channel::ControlTransitionRequest) -> bool {
        self.logical_inbound_arrival(request).is_some()
    }

    fn request_stop(&self, reason: LiveStopCode) {
        if self
            .stop
            .compare_exchange(
                LiveStopCode::Running as u8,
                reason as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.queue.close();
            // SAFETY: this id belongs to the live hook message thread and the message carries no
            // borrowed data.
            let _ =
                unsafe { PostThreadMessageW(self.hook_thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
    }

    fn fail(&self, reason: LiveStopCode, message: impl Into<String>) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(message.into());
        }
        self.request_stop(reason);
    }

    fn set_controlled_by_peer(&self) {
        self.cancel_pending_enter_capture();
        self.local_takeover_pending.store(false, Ordering::Release);
        self.reset_local_takeover_motion();
        self.active_epoch.store(0, Ordering::Release);
        self.target_first_remote_move_pending
            .store(true, Ordering::Release);
        self.target_first_remote_move_injection_pending
            .store(true, Ordering::Release);
        *self
            .target_first_remote_move_received_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.route
            .store(LiveRoute::ControlledByPeer as u8, Ordering::Release);
        if let Ok(mut stats) = self.stats.lock() {
            stats.control_entries += 1;
        }
    }

    fn begin_entering_peer(
        &self,
        epoch: u64,
        peer_screen: PhysicalScreenSize,
        peer_position: PhysicalPoint,
        local_resume: PhysicalPoint,
    ) -> io::Result<Vec<InputEvent>> {
        self.queue.clear();
        self.source_first_remote_move_pending
            .store(true, Ordering::Release);
        self.source_first_remote_move_sent_pending
            .store(true, Ordering::Release);
        let local_resume = inset_boundary_resume(local_resume, self.local_screen);
        let gain = self
            .pointer_gain
            .lock()
            .map_err(|_| io::Error::other("pointer gain calibration was poisoned"))?
            .model();
        let pending_motion = self
            .pending_enter_motion
            .lock()
            .map_err(|_| io::Error::other("pending enter movement was poisoned"))?
            .take();
        let (pending_raw_x, pending_raw_y) = pending_motion.raw_totals();
        let entry_raw_time_cutoff = pending_motion.cutoff_ms.or_else(|| {
            // SAFETY: GetTickCount has no preconditions and shares the WM_INPUT time basis.
            Some(unsafe { GetTickCount() })
        });
        let mut pointer = RemotePointerState {
            screen: peer_screen,
            position: peer_position,
            entry_raw_time_cutoff,
            gain,
            residual_x: 0,
            residual_y: 0,
        };
        let (pending_delta_x, pending_delta_y) =
            pending_motion.scale(gain, &mut pointer.residual_x, &mut pointer.residual_y);
        *self
            .last_enter_carry
            .lock()
            .map_err(|_| io::Error::other("enter carry trace was poisoned"))? = EnterCarryTrace {
            packets: pending_motion.packets,
            raw_x: pending_raw_x,
            raw_y: pending_raw_y,
            pixel_x: pending_delta_x,
            pixel_y: pending_delta_y,
        };
        let pending_signal = advance_remote_pointer(&mut pointer, pending_delta_x, pending_delta_y);
        *self
            .remote_pointer
            .lock()
            .map_err(|_| io::Error::other("remote pointer state was poisoned"))? = Some(pointer);
        *self
            .local_resume
            .lock()
            .map_err(|_| io::Error::other("local resume state was poisoned"))? = Some(local_resume);
        self.return_requested_at
            .lock()
            .map_err(|_| io::Error::other("return latency state was poisoned"))?
            .take();
        *self
            .forwarded
            .lock()
            .map_err(|_| io::Error::other("forwarded input state was poisoned"))? =
            ForwardingState::default();
        let inherited_mask = self.local_button_mask.load(Ordering::Acquire);
        let inherited_downs = button_events(inherited_mask, KeyTransition::Down);
        let remote_downs = inherited_downs
            .iter()
            .copied()
            .map(|event| {
                remote_event_from_input(event)
                    .expect("inherited mouse-button presses convert to remote input")
            })
            .collect::<Vec<_>>();
        if !remote_downs.is_empty() {
            match self.queue.push_inputs(&remote_downs) {
                LiveEnqueueResult::Queued => {}
                LiveEnqueueResult::Closed => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "live input queue closed while inheriting pressed mouse buttons",
                    ));
                }
                LiveEnqueueResult::Fatal => {
                    return Err(io::Error::other(
                        "live input queue could not inherit pressed mouse buttons",
                    ));
                }
                LiveEnqueueResult::Coalesced | LiveEnqueueResult::MouseMoveBypassed => {
                    unreachable!("reliable input enqueue cannot return a mouse-move result");
                }
            }
        }
        self.inherited_remote_button_mask
            .store(inherited_mask, Ordering::Release);
        self.inherited_button_downs
            .fetch_add(u64::from(inherited_mask.count_ones()), Ordering::Relaxed);
        self.enqueue_pending_enter_pointer(pending_signal)?;
        self.active_epoch.store(epoch, Ordering::Release);
        self.route
            .store(LiveRoute::EnteringPeer as u8, Ordering::Release);
        self.reconcile_inherited_button_releases();
        Ok(button_events(inherited_mask, KeyTransition::Up))
    }

    fn enqueue_pending_enter_pointer(&self, signal: Option<LiveSignal>) -> io::Result<()> {
        let Some(signal) = signal else {
            return Ok(());
        };
        match self.queue.push_pointer(signal) {
            LiveEnqueueResult::Queued | LiveEnqueueResult::Coalesced => {
                self.forwarded_mouse_moves.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            LiveEnqueueResult::Closed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "live input queue closed while applying pending enter movement",
            )),
            // Mouse movement is replaceable. If reliable inherited-button events filled the
            // bounded queue, the next fresh raw packet will carry the current absolute position.
            LiveEnqueueResult::MouseMoveBypassed => Ok(()),
            LiveEnqueueResult::Fatal => Err(io::Error::other(
                "live input queue could not apply pending enter movement",
            )),
        }
    }

    fn activate_controlling_peer(&self) -> io::Result<()> {
        self.route
            .compare_exchange(
                LiveRoute::EnteringPeer as u8,
                LiveRoute::ControllingPeer as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| invalid_data("live route left entering state before activation"))?;
        if let Ok(mut stats) = self.stats.lock() {
            stats.control_entries += 1;
        }
        Ok(())
    }

    fn force_local(&self) -> Option<PhysicalPoint> {
        self.force_local_with_drag(false)
    }

    fn force_local_with_drag(&self, preserve: bool) -> Option<PhysicalPoint> {
        if !preserve {
            self.preserve_next_drag_release
                .store(false, Ordering::Release);
            self.local_source_drag_return_eligible
                .store(false, Ordering::Release);
        }
        self.remote_left_down.store(false, Ordering::Release);
        if !preserve && let Some(drag) = &self.drag {
            drag.cancel_with_reason("control_forced_local");
        }
        if let Ok(mut retry) = self.drag_retry.lock() {
            *retry = None;
        }
        self.queue.clear();
        self.cancel_pending_enter_capture();
        self.local_takeover_pending.store(false, Ordering::Release);
        self.reset_local_takeover_motion();
        self.active_epoch.store(0, Ordering::Release);
        self.inherited_remote_button_mask
            .store(0, Ordering::Release);
        if let Ok(mut forwarded) = self.forwarded.lock() {
            let _ = forwarded.release_all();
        }
        if let Ok(mut remote) = self.remote_pointer.lock() {
            remote.take();
        }
        let resume = self
            .local_resume
            .lock()
            .map_or(None, |mut resume| resume.take());
        if let Some(resume) = resume {
            self.set_local_pointer_baseline(resume);
            self.local_pointer_needs_baseline
                .store(false, Ordering::Release);
        } else {
            // A target that has just stopped receiving remote input has no saved source-side
            // boundary point. Its first genuine local movement establishes a fresh baseline so a
            // large delta from the last pre-control sample cannot trigger a phantom crossing.
            self.local_pointer_needs_baseline
                .store(true, Ordering::Release);
        }
        // Publish Local only after the baseline and forwarding state are coherent. The low-level
        // hook can now never observe a locally routed pointer with stale pre-control coordinates.
        self.route.store(LiveRoute::Local as u8, Ordering::Release);
        resume
    }

    fn begin_pending_enter_capture(&self, boundary: ScreenBoundary) {
        // SAFETY: GetTickCount has no preconditions and shares the millisecond time basis used by
        // GetMessageTime for WM_INPUT packets.
        self.begin_pending_enter_capture_at(unsafe { GetTickCount() }, boundary);
    }

    fn begin_pending_enter_capture_at(&self, cutoff_ms: u32, boundary: ScreenBoundary) {
        let recent_overflow = self
            .recent_edge_overflow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut pending = self
            .pending_enter_motion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.begin(cutoff_ms, boundary);
        if let Some(candidate) = recent_overflow {
            pending.apply_edge_overflow(candidate);
        }
    }

    fn cancel_pending_enter_capture(&self) {
        *self
            .pending_enter_motion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PendingEnterMotion::default();
        self.recent_edge_overflow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.local_transition_pending
            .store(false, Ordering::Release);
    }

    fn take_local_restore_inputs(&self) -> io::Result<Vec<InputEvent>> {
        let mut restore = self
            .forwarded
            .lock()
            .map_err(|_| io::Error::other("forwarded input state was poisoned"))?
            .take_local_restore_keys();
        let mut buttons = self.local_button_mask.load(Ordering::Acquire);
        if self.suppress_drag_left_restore.load(Ordering::Acquire) {
            buttons &= !mouse_button_bit(MouseButton::Left);
        }
        restore.extend(button_events(buttons, KeyTransition::Down));
        Ok(restore)
    }

    fn reconcile_inherited_button_releases(&self) {
        let released = self.inherited_remote_button_mask.load(Ordering::Acquire)
            & !self.local_button_mask.load(Ordering::Acquire);
        for event in button_events(released, KeyTransition::Up) {
            let InputEvent::MouseButton { button, .. } = event else {
                unreachable!("button helper only creates mouse-button events");
            };
            let _ = self.forward_inherited_button_release(button);
        }
    }

    fn forward_inherited_button_release(&self, button: MouseButton) -> bool {
        let bit = mouse_button_bit(button);
        let previous = self
            .inherited_remote_button_mask
            .fetch_and(!bit, Ordering::AcqRel);
        if previous & bit == 0 {
            return false;
        }
        let event = RemoteInputEvent::MouseButton {
            button,
            transition: KeyTransition::Up,
        };
        match self.queue.push_inputs(&[event]) {
            LiveEnqueueResult::Queued => {
                self.inherited_button_ups.fetch_add(1, Ordering::Relaxed);
            }
            LiveEnqueueResult::Closed => {}
            LiveEnqueueResult::Fatal => {
                self.fail(
                    LiveStopCode::QueueFailure,
                    "live input queue could not release an inherited mouse button",
                );
            }
            LiveEnqueueResult::Coalesced | LiveEnqueueResult::MouseMoveBypassed => {
                unreachable!("reliable input enqueue cannot return a mouse-move result");
            }
        }
        true
    }

    fn record_control_return(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.control_returns += 1;
        }
    }

    fn record_enter_latency(&self, elapsed: Duration) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.enter_latency.record(elapsed);
        }
    }

    fn begin_return_latency(&self) -> io::Result<()> {
        *self
            .return_requested_at
            .lock()
            .map_err(|_| io::Error::other("return latency state was poisoned"))? =
            Some(Instant::now());
        Ok(())
    }

    fn record_return_latency(&self) {
        let requested_at = self
            .return_requested_at
            .lock()
            .map_or(None, |mut requested_at| requested_at.take());
        if let Some(requested_at) = requested_at
            && let Ok(mut stats) = self.stats.lock()
        {
            stats.return_latency.record(requested_at.elapsed());
        }
    }

    fn set_local_resume(&self, point: PhysicalPoint) -> io::Result<()> {
        let point = inset_boundary_resume(point, self.local_screen);
        *self
            .local_resume
            .lock()
            .map_err(|_| io::Error::other("local resume state was poisoned"))? = Some(point);
        Ok(())
    }

    fn set_local_pointer_baseline(&self, point: PhysicalPoint) {
        let x = i32::try_from(point.x).expect("local pointer x fits i32");
        let y = i32::try_from(point.y).expect("local pointer y fits i32");
        *self
            .previous_x
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = x;
        *self
            .previous_y
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = y;
        self.last_local_x.store(x, Ordering::Release);
        self.last_local_y.store(y, Ordering::Release);
        if let Ok(mut probe) = self.boundary_probe.try_lock() {
            probe.reset_contact();
        }
    }

    fn record_processed_boundary_probe(
        &self,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        message_time_ms: Option<u32>,
        queue_result: LiveEnqueueResult,
    ) {
        let previous_x = self.last_local_x.swap(x, Ordering::AcqRel);
        let previous_y = self.last_local_y.swap(y, Ordering::AcqRel);
        let current_is_on_edge =
            point_is_on_probe_boundary(x, y, self.local_screen, self.departure_boundary);
        if !current_is_on_edge {
            // Avoid taking even a non-blocking diagnostic mutex on ordinary high-rate movement.
            // Only the first packet that leaves the configured edge needs to re-arm the probe.
            if point_is_on_probe_boundary(
                previous_x,
                previous_y,
                self.local_screen,
                self.departure_boundary,
            ) && let Ok(mut probe) = self.boundary_probe.try_lock()
            {
                probe.reset_contact();
            }
            return;
        }
        let Ok(mut probe) = self.boundary_probe.try_lock() else {
            return;
        };
        let observed_at = Instant::now();
        let is_new_contact = !probe.contact_active;
        let attempt = probe.begin_contact(observed_at);
        let corner_safe = probe_position_is_corner_safe(
            x,
            y,
            self.local_screen,
            self.departure_boundary,
            self.corner_guard_pixels,
        );
        let queue_depth = self.queue.diagnostic_depth();
        let coalesced_moves = self.queue.coalesced_moves.load(Ordering::Relaxed);
        let bypassed_moves = self.queue.bypassed_moves.load(Ordering::Relaxed);
        let message_time_ms = message_time_ms.unwrap_or(0);
        if is_new_contact {
            probe.push(BoundaryProbeEvent {
                kind: BoundaryProbeKind::Contact,
                attempt,
                observed_at,
                message_time_ms,
                position_x: x,
                position_y: y,
                delta_x,
                delta_y,
                corner_safe,
                queue_result: Some(queue_result),
                queue_depth,
                coalesced_moves,
                bypassed_moves,
            });
        }
        if probe_delta_is_outward(delta_x, delta_y, self.departure_boundary)
            && !probe.processed_outward_recorded
        {
            probe.processed_outward_recorded = true;
            probe.processed_outward_at = Some(observed_at);
            probe.push(BoundaryProbeEvent {
                kind: BoundaryProbeKind::ProcessedOutward,
                attempt,
                observed_at,
                message_time_ms,
                position_x: x,
                position_y: y,
                delta_x,
                delta_y,
                corner_safe,
                queue_result: Some(queue_result),
                queue_depth,
                coalesced_moves,
                bypassed_moves,
            });
        }
    }

    fn record_raw_boundary_probe(&self, delta_x: i32, delta_y: i32, message_time_ms: u32) {
        if !probe_delta_is_outward(delta_x, delta_y, self.departure_boundary) {
            return;
        }
        let x = self.last_local_x.load(Ordering::Acquire);
        let y = self.last_local_y.load(Ordering::Acquire);
        if !point_is_on_probe_boundary(x, y, self.local_screen, self.departure_boundary) {
            return;
        }
        let observed_at = Instant::now();
        let Ok(mut probe) = self.boundary_probe.try_lock() else {
            return;
        };
        let attempt = probe.begin_contact(observed_at);
        if probe.raw_outward_recorded {
            return;
        }
        probe.raw_outward_recorded = true;
        probe.raw_outward_at = Some(observed_at);
        probe.push(BoundaryProbeEvent {
            kind: BoundaryProbeKind::RawOutward,
            attempt,
            observed_at,
            message_time_ms,
            position_x: x,
            position_y: y,
            delta_x,
            delta_y,
            corner_safe: probe_position_is_corner_safe(
                x,
                y,
                self.local_screen,
                self.departure_boundary,
                self.corner_guard_pixels,
            ),
            queue_result: None,
            queue_depth: self.queue.diagnostic_depth(),
            coalesced_moves: self.queue.coalesced_moves.load(Ordering::Relaxed),
            bypassed_moves: self.queue.bypassed_moves.load(Ordering::Relaxed),
        });
    }

    fn flush_boundary_probe_events(&self, control: Option<&LiveSessionControl>) {
        let (events, dropped) = self.boundary_probe.lock().map_or_else(
            |poisoned| {
                let mut probe = poisoned.into_inner();
                (
                    probe.events.drain(..).collect::<Vec<_>>(),
                    std::mem::take(&mut probe.dropped),
                )
            },
            |mut probe| {
                (
                    probe.events.drain(..).collect::<Vec<_>>(),
                    std::mem::take(&mut probe.dropped),
                )
            },
        );
        let Some(control) = control else {
            return;
        };
        if dropped != 0 {
            control.record_transition_trace(format!("kind=edge_probe_overflow,dropped={dropped}"));
        }
        for event in events {
            control.record_transition_trace_at(
                format!(
                    "kind={},attempt={},boundary={},position={},{} delta={},{} message_tick_ms={} corner_safe={} queue={} queue_depth={} coalesced={} bypassed={}",
                    event.kind.label(),
                    event.attempt,
                    self.departure_boundary.label(),
                    event.position_x,
                    event.position_y,
                    event.delta_x,
                    event.delta_y,
                    event.message_time_ms,
                    event.corner_safe,
                    event.queue_result.map_or("none", LiveEnqueueResult::label),
                    event.queue_depth.map_or_else(|| "busy".to_owned(), |depth| depth.to_string()),
                    event.coalesced_moves,
                    event.bypassed_moves,
                ),
                event.observed_at,
            );
        }
    }

    fn boundary_request_trace(&self, requested_at: Instant) -> String {
        let probe = self
            .boundary_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let elapsed = |observed_at: Option<Instant>| {
            observed_at.map_or_else(
                || "none".to_owned(),
                |observed_at| {
                    elapsed_micros(requested_at.saturating_duration_since(observed_at)).to_string()
                },
            )
        };
        format!(
            "attempt={},edge_to_request_us={},processed_to_request_us={},raw_to_request_us={}",
            probe.attempt,
            elapsed(probe.contact_started_at),
            elapsed(probe.processed_outward_at),
            elapsed(probe.raw_outward_at),
        )
    }

    fn record_trace(&self, trace: String) {
        if let Some(control) = self.trace_control.as_deref() {
            control.record_transition_trace(trace);
        }
    }

    fn record_source_first_remote_move(
        &self,
        raw_delta_x: i32,
        raw_delta_y: i32,
        scaled_delta_x: i32,
        scaled_delta_y: i32,
        position: PhysicalPoint,
        queue_result: LiveEnqueueResult,
    ) {
        if matches!(
            queue_result,
            LiveEnqueueResult::Queued | LiveEnqueueResult::Coalesced
        ) && self
            .source_first_remote_move_pending
            .swap(false, Ordering::AcqRel)
        {
            self.record_trace(format!(
                "kind=source_first_remote_move,raw_delta={},{} scaled_delta={},{} position={},{} queue={} queue_depth={}",
                raw_delta_x,
                raw_delta_y,
                scaled_delta_x,
                scaled_delta_y,
                position.x,
                position.y,
                queue_result.label(),
                self.queue
                    .diagnostic_depth()
                    .map_or_else(|| "busy".to_owned(), |depth| depth.to_string()),
            ));
        }
    }

    fn record_source_first_remote_move_sent(&self, sample: PointerSample) {
        if self
            .source_first_remote_move_sent_pending
            .swap(false, Ordering::AcqRel)
        {
            self.record_trace(format!(
                "kind=source_first_remote_move_sent,position={},{} delta={},{}",
                sample.position.x, sample.position.y, sample.delta_x, sample.delta_y,
            ));
        }
    }

    fn record_target_first_remote_move_received(&self) {
        if self
            .target_first_remote_move_pending
            .swap(false, Ordering::AcqRel)
        {
            *self
                .target_first_remote_move_received_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
            self.record_trace("kind=target_first_remote_move_received".to_owned());
        }
    }

    fn record_target_first_remote_move_injected(&self) {
        if self
            .target_first_remote_move_injection_pending
            .swap(false, Ordering::AcqRel)
        {
            let received_at = self
                .target_first_remote_move_received_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            self.record_trace(format!(
                "kind=target_first_remote_move_injected,receive_to_inject_us={}",
                received_at.map_or_else(
                    || "none".to_owned(),
                    |received_at| elapsed_micros(received_at.elapsed()).to_string(),
                )
            ));
        }
    }

    fn pointer_gain_trace(&self) -> String {
        let model = self.pointer_gain.lock().map_or_else(
            |poisoned| poisoned.into_inner().model(),
            |gain| gain.model(),
        );
        format!(
            "x={}/{}/{}/{},y={}/{}/{}/{}",
            model.x[0],
            model.x[1],
            model.x[2],
            model.x[3],
            model.y[0],
            model.y[1],
            model.y[2],
            model.y[3]
        )
    }

    fn enter_carry_trace(&self) -> String {
        let carry = self
            .last_enter_carry
            .lock()
            .map_or_else(|poisoned| *poisoned.into_inner(), |carry| *carry);
        format!(
            "carry_packets={},carry_raw={},{} carry_pixels={},{} display_hz={}",
            carry.packets,
            carry.raw_x,
            carry.raw_y,
            carry.pixel_x,
            carry.pixel_y,
            self.local_refresh_hz
        )
    }

    fn observe_emergency(&self, event: InputEvent) -> bool {
        let InputEvent::Key {
            virtual_key,
            transition,
            ..
        } = event
        else {
            return false;
        };
        if virtual_key == VK_ESCAPE_VALUE && transition == KeyTransition::Down {
            let bits =
                u16::try_from(self.modifier_bits.load(Ordering::Acquire)).unwrap_or_default();
            if bits & CONTROL_MASK != 0 && bits & ALT_MASK != 0 && bits & SHIFT_MASK != 0 {
                self.request_stop(LiveStopCode::EmergencyShortcut);
                return true;
            }
        }
        let Some(bit) = modifier_bit(virtual_key) else {
            return false;
        };
        match transition {
            KeyTransition::Down => {
                self.modifier_bits
                    .fetch_or(u64::from(bit), Ordering::AcqRel);
            }
            KeyTransition::Up => {
                self.modifier_bits
                    .fetch_and(!u64::from(bit), Ordering::AcqRel);
            }
        }
        false
    }

    fn request_local_takeover(&self) {
        if self
            .local_takeover_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        match self.queue.push_local_takeover() {
            LiveEnqueueResult::Queued => {}
            LiveEnqueueResult::Closed => {
                self.local_takeover_pending.store(false, Ordering::Release);
            }
            LiveEnqueueResult::Fatal => {
                self.local_takeover_pending.store(false, Ordering::Release);
                self.fail(
                    LiveStopCode::QueueFailure,
                    "live input queue could not request local physical-input takeover",
                );
            }
            LiveEnqueueResult::Coalesced | LiveEnqueueResult::MouseMoveBypassed => {
                unreachable!("local takeover enqueue cannot return a mouse-move result");
            }
        }
    }

    fn reset_local_takeover_motion(&self) {
        match self.local_takeover_motion.try_lock() {
            Ok(mut motion) => motion.reset(),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().reset(),
            Err(TryLockError::WouldBlock) => {}
        }
    }

    fn handle_controlled_target_input(&self, event: InputEvent) -> bool {
        if let InputEvent::MouseMove { x, y } = event {
            let intentional = match self.local_takeover_motion.try_lock() {
                Ok(mut motion) => motion.observe(x, y, Instant::now()),
                Err(TryLockError::Poisoned(poisoned)) => {
                    poisoned.into_inner().observe(x, y, Instant::now())
                }
                // Losing one optional sample must neither fail the service nor leak a target-side
                // cursor movement while the peer still owns control.
                Err(TryLockError::WouldBlock) => false,
            };
            if intentional {
                self.request_local_takeover();
            }
            return true;
        }
        self.reset_local_takeover_motion();
        self.request_local_takeover();
        false
    }

    fn claim_local_transition(&self) -> bool {
        self.local_transition_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release_local_transition_after_enqueue(&self, result: LiveEnqueueResult) {
        if !matches!(
            result,
            LiveEnqueueResult::Queued | LiveEnqueueResult::Coalesced
        ) {
            self.local_transition_pending
                .store(false, Ordering::Release);
        }
    }

    fn record_near_raw_boundary_motion(
        &self,
        current_x: i32,
        current_y: i32,
        raw_delta_x: i32,
        raw_delta_y: i32,
        message_time_ms: u32,
        matched_processed: bool,
    ) {
        // A matched raw packet has already moved the processed cursor and is not evidence of a
        // missing high-speed boundary event. Logging only unmatched packets keeps this probe
        // focused and prevents ordinary movement near the configured edge from growing the log.
        if matched_processed
            || !probe_delta_is_outward(raw_delta_x, raw_delta_y, self.departure_boundary)
            || distance_to_probe_boundary(
                current_x,
                current_y,
                self.local_screen,
                self.departure_boundary,
            )
            .is_none_or(|distance| distance > RAW_BOUNDARY_DIAGNOSTIC_DISTANCE)
        {
            return;
        }
        let previous = self
            .last_raw_boundary_diagnostic_tick
            .swap(message_time_ms, Ordering::AcqRel);
        if previous != 0
            && message_time_ms.wrapping_sub(previous) < RAW_BOUNDARY_DIAGNOSTIC_INTERVAL_MS
        {
            return;
        }
        let observed_at = Instant::now();
        let Ok(mut probe) = self.boundary_probe.try_lock() else {
            return;
        };
        probe.push(BoundaryProbeEvent {
            kind: BoundaryProbeKind::RawNearUnmatched,
            attempt: 0,
            observed_at,
            message_time_ms,
            position_x: current_x,
            position_y: current_y,
            delta_x: raw_delta_x,
            delta_y: raw_delta_y,
            corner_safe: probe_position_is_corner_safe(
                current_x,
                current_y,
                self.local_screen,
                self.departure_boundary,
                self.corner_guard_pixels,
            ),
            queue_result: None,
            queue_depth: self.queue.diagnostic_depth(),
            coalesced_moves: self.queue.coalesced_moves.load(Ordering::Relaxed),
            bypassed_moves: self.queue.bypassed_moves.load(Ordering::Relaxed),
        });
    }

    fn raw_boundary_sample(
        &self,
        current: (i32, i32),
        raw_delta: (i32, i32),
        gain: PointerGainModel,
        matched_processed: bool,
        edge_overflow: Option<EdgeOverflowCandidate>,
    ) -> Option<(PointerSample, ScreenBoundary)> {
        if self.logical_boundary_layout.is_some() {
            let scaled_delta = if matched_processed {
                edge_overflow.map(|overflow| (overflow.pixel_x, overflow.pixel_y))?
            } else {
                let mut residual_x = 0;
                let mut residual_y = 0;
                (
                    gain.scale_x(raw_delta.0, &mut residual_x),
                    gain.scale_y(raw_delta.1, &mut residual_y),
                )
            };
            for edge in [
                ScreenBoundary::Left,
                ScreenBoundary::Right,
                ScreenBoundary::Top,
                ScreenBoundary::Bottom,
            ] {
                let Some(sample) = scaled_boundary_transition_sample(
                    current.0,
                    current.1,
                    scaled_delta.0,
                    scaled_delta.1,
                    self.local_screen,
                    edge,
                    0,
                ) else {
                    continue;
                };
                if self
                    .logical_transfer_for_sample(ControlProtocolState::Local, sample)
                    .is_some()
                {
                    return Some((sample, edge));
                }
            }
            return None;
        }
        if let Some(map) = &self.boundary_map {
            if matched_processed {
                return edge_overflow.and_then(|overflow| {
                    prepared_boundary_transition_sample(
                        current,
                        (overflow.pixel_x, overflow.pixel_y),
                        self.local_screen,
                        map,
                    )
                });
            }
            let mut residual_x = 0;
            let mut residual_y = 0;
            let scaled_x = gain.scale_x(raw_delta.0, &mut residual_x);
            let scaled_y = gain.scale_y(raw_delta.1, &mut residual_y);
            return prepared_boundary_transition_sample(
                current,
                (scaled_x, scaled_y),
                self.local_screen,
                map,
            );
        }
        if matched_processed {
            return edge_overflow.and_then(|overflow| {
                scaled_boundary_transition_sample(
                    current.0,
                    current.1,
                    overflow.pixel_x,
                    overflow.pixel_y,
                    self.local_screen,
                    self.departure_boundary,
                    self.corner_guard_pixels,
                )
                .map(|sample| (sample, self.departure_boundary))
            });
        }
        raw_boundary_transition_sample(
            current,
            raw_delta,
            gain,
            self.local_screen,
            self.departure_boundary,
            self.corner_guard_pixels,
        )
        .map(|sample| (sample, self.departure_boundary))
    }

    fn enqueue_raw_boundary_transition(
        &self,
        raw_delta_x: i32,
        raw_delta_y: i32,
        message_time_ms: u32,
        gain: PointerGainModel,
        matched_processed: bool,
        edge_overflow: Option<EdgeOverflowCandidate>,
    ) {
        if self.local_pointer_needs_baseline.load(Ordering::Acquire) {
            return;
        }
        let current_x = self.last_local_x.load(Ordering::Acquire);
        let current_y = self.last_local_y.load(Ordering::Acquire);
        let boundary_sample = self.raw_boundary_sample(
            (current_x, current_y),
            (raw_delta_x, raw_delta_y),
            gain,
            matched_processed,
            edge_overflow,
        );
        let Some((sample, departure_boundary)) = boundary_sample else {
            self.record_near_raw_boundary_motion(
                current_x,
                current_y,
                raw_delta_x,
                raw_delta_y,
                message_time_ms,
                matched_processed,
            );
            return;
        };
        if !self.claim_local_transition() {
            return;
        }
        let predicted_overflow = (!matched_processed)
            .then(|| {
                raw_boundary_overflow(
                    (current_x, current_y),
                    (raw_delta_x, raw_delta_y),
                    gain,
                    self.local_screen,
                    departure_boundary,
                    message_time_ms,
                )
            })
            .flatten();
        if let Some(overflow) = predicted_overflow
            && let Ok(mut recent) = self.recent_edge_overflow.try_lock()
        {
            *recent = Some(overflow);
        }
        let result = self.queue.push_transition_pointer(LiveSignal::Pointer {
            sample,
            remote_position: None,
        });
        self.release_local_transition_after_enqueue(result);
        if !matches!(
            result,
            LiveEnqueueResult::Queued | LiveEnqueueResult::Coalesced
        ) && predicted_overflow.is_some()
            && let Ok(mut recent) = self.recent_edge_overflow.try_lock()
        {
            recent.take();
        }
        let observed_at = Instant::now();
        if let Ok(mut probe) = self.boundary_probe.try_lock() {
            let attempt = probe.begin_contact(observed_at);
            probe.raw_outward_recorded = true;
            probe.raw_outward_at = Some(observed_at);
            probe.push(BoundaryProbeEvent {
                kind: BoundaryProbeKind::RawPredicted,
                attempt,
                observed_at,
                message_time_ms,
                position_x: i32::try_from(sample.position.x).unwrap_or(i32::MAX),
                position_y: i32::try_from(sample.position.y).unwrap_or(i32::MAX),
                delta_x: raw_delta_x,
                delta_y: raw_delta_y,
                corner_safe: true,
                queue_result: Some(result),
                queue_depth: self.queue.diagnostic_depth(),
                coalesced_moves: self.queue.coalesced_moves.load(Ordering::Relaxed),
                bypassed_moves: self.queue.bypassed_moves.load(Ordering::Relaxed),
            });
        }
        if result == LiveEnqueueResult::Fatal {
            self.fail(
                LiveStopCode::QueueFailure,
                "live input queue was poisoned while predicting a raw boundary crossing",
            );
        }
    }

    fn handle_local_pointer(&self, x: i32, y: i32, message_time_ms: Option<u32>) -> bool {
        if self
            .local_pointer_needs_baseline
            .swap(false, Ordering::AcqRel)
        {
            self.set_local_pointer_baseline(PhysicalPoint {
                x: u32::try_from(x).expect("hook x is inside primary screen"),
                y: u32::try_from(y).expect("hook y is inside primary screen"),
            });
            return false;
        }
        let (delta_x, delta_y) = match (self.previous_x.try_lock(), self.previous_y.try_lock()) {
            (Ok(mut previous_x), Ok(mut previous_y)) => {
                let delta_x = x.saturating_sub(*previous_x);
                let delta_y = y.saturating_sub(*previous_y);
                *previous_x = x;
                *previous_y = y;
                (delta_x, delta_y)
            }
            _ => return false,
        };
        let sample = PointerSample {
            position: PhysicalPoint {
                x: u32::try_from(x).expect("hook x is inside primary screen"),
                y: u32::try_from(y).expect("hook y is inside primary screen"),
            },
            delta_x,
            delta_y,
        };
        if let Some(message_time_ms) = message_time_ms
            && let Ok(mut calibration) = self.pointer_gain.try_lock()
        {
            calibration.record_processed(ProcessedPointerSample {
                message_time_ms,
                delta_x,
                delta_y,
                x_clamped: x == 0
                    || x == i32::try_from(self.local_screen.width - 1)
                        .expect("screen width fits i32"),
                y_clamped: y == 0
                    || y == i32::try_from(self.local_screen.height - 1)
                        .expect("screen height fits i32"),
            });
        }
        let signal = LiveSignal::Pointer {
            sample,
            remote_position: None,
        };
        let transition_candidate = self.local_departure_for_sample(sample).is_some();
        // A qualifying edge sample is a control event, not disposable mouse motion. A faster
        // device can deliver a clamped or slightly recoiled packet before the network worker gets
        // scheduled; keeping this exact sample prevents the later packet from erasing the only
        // evidence that the user pushed through the configured boundary.
        let protected_transition = transition_candidate && self.claim_local_transition();
        let result = if protected_transition {
            self.queue.push_transition_pointer(signal)
        } else {
            self.queue.push_pointer(signal)
        };
        if protected_transition {
            self.release_local_transition_after_enqueue(result);
        }
        self.record_processed_boundary_probe(x, y, delta_x, delta_y, message_time_ms, result);
        if result == LiveEnqueueResult::Fatal {
            self.fail(
                LiveStopCode::QueueFailure,
                "live input queue was poisoned while sampling local mouse movement",
            );
        }
        false
    }

    fn handle_remote_pointer(&self, raw_delta_x: i32, raw_delta_y: i32, message_time_ms: u32) {
        let mut pointer = match self.remote_pointer.try_lock() {
            Ok(pointer) => pointer,
            // Mouse movement is replaceable. Losing one raw packet during a transition is safer
            // than blocking the hook/message thread or permanently failing the session.
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(_)) => {
                self.fail(
                    LiveStopCode::QueueFailure,
                    "remote pointer state was poisoned in the raw-input receiver",
                );
                return;
            }
        };
        let Some(pointer) = pointer.as_mut() else {
            self.fail(
                LiveStopCode::QueueFailure,
                "remote pointer state was missing while raw input was captured",
            );
            return;
        };
        if let Some(cutoff) = pointer.entry_raw_time_cutoff {
            if !tick_is_strictly_after(message_time_ms, cutoff) {
                return;
            }
            pointer.entry_raw_time_cutoff = None;
        }
        let delta_x = pointer.gain.scale_x(raw_delta_x, &mut pointer.residual_x);
        let delta_y = pointer.gain.scale_y(raw_delta_y, &mut pointer.residual_y);
        let Some(signal) = advance_remote_pointer(pointer, delta_x, delta_y) else {
            return;
        };
        let position = pointer.position;
        let result = self.queue.push_pointer(signal);
        self.record_source_first_remote_move(
            raw_delta_x,
            raw_delta_y,
            delta_x,
            delta_y,
            position,
            result,
        );
        match result {
            LiveEnqueueResult::Queued | LiveEnqueueResult::Coalesced => {
                self.forwarded_mouse_moves.fetch_add(1, Ordering::Relaxed);
            }
            LiveEnqueueResult::MouseMoveBypassed | LiveEnqueueResult::Closed => {}
            LiveEnqueueResult::Fatal => {
                self.fail(
                    LiveStopCode::QueueFailure,
                    "live input queue was poisoned while forwarding mouse movement",
                );
            }
        }
    }

    fn handle_remote_reliable(&self, event: InputEvent) -> bool {
        if let InputEvent::MouseButton {
            button,
            transition: KeyTransition::Up,
        } = event
            && self.forward_inherited_button_release(button)
        {
            return true;
        }
        let outcome = match self.forwarded.try_lock() {
            Ok(mut state) => state.prepare(event),
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                self.fail(
                    LiveStopCode::QueueFailure,
                    "forwarded input state was unavailable in the input hook",
                );
                return false;
            }
        };
        match outcome {
            Ok(outcome) => {
                if !outcome.events.is_empty() {
                    match self.queue.push_inputs(&outcome.events) {
                        LiveEnqueueResult::Queued => {}
                        LiveEnqueueResult::Closed => return true,
                        LiveEnqueueResult::Fatal => {
                            self.fail(
                                LiveStopCode::QueueFailure,
                                "bounded live input queue could not accept reliable input events",
                            );
                            return false;
                        }
                        LiveEnqueueResult::Coalesced | LiveEnqueueResult::MouseMoveBypassed => {
                            unreachable!(
                                "reliable input enqueue cannot return a mouse-move result"
                            );
                        }
                    }
                }
                // Num Lock must also reach the source OS so subsequent hook events describe
                // the physical keyboard's current digit/navigation mode and its LED follows
                // the user's key. The receiver preserves that meaning per keypad event.
                // Do not apply this exception to ordinary keys or shortcut modifiers.
                outcome.suppress_current
                    && !matches!(
                        event,
                        InputEvent::Key {
                            virtual_key: VK_NUMLOCK_VALUE,
                            ..
                        }
                    )
            }
            Err(StateDecision::EmergencyReturn) => {
                self.request_stop(LiveStopCode::EmergencyShortcut);
                true
            }
            Err(StateDecision::CapacityExceeded) => {
                self.fail(
                    LiveStopCode::QueueFailure,
                    "forwarded input state exceeded its fixed capacity",
                );
                false
            }
            Err(StateDecision::Forward | StateDecision::PassThrough) => {
                unreachable!("forwarding preparation only returns terminal state decisions")
            }
        }
    }

    fn handle_event(&self, event: InputEvent, mouse_time_ms: Option<u32>) -> bool {
        if matches!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Up
            }
        ) {
            self.local_source_drag_return_eligible
                .store(false, Ordering::Release);
            self.suppress_drag_left_restore
                .store(false, Ordering::Release);
            if let Some(drag) = &self.drag {
                let returned_source_pending = drag.returned_source_pending();
                let local_route =
                    LiveRoute::load(self.route.load(Ordering::Acquire)) == LiveRoute::Local;
                if local_route {
                    drag.release_returned_source();
                }
                // A rearmed reverse receiver remains PREPARING while the pointer is
                // on the source PC. Only the currently local receiver owns this Up.
                let released_local_target = local_route && drag.release_local_target();
                if should_cancel_local_drag_after_left_release(
                    released_local_target,
                    local_route,
                    returned_source_pending,
                ) {
                    drag.cancel_with_reason("local_left_release_without_target");
                }
                drag.clear_failed_reverse_left_restore();
            }
        }
        if matches!(
            event,
            InputEvent::Key {
                virtual_key: 0x1b,
                transition: KeyTransition::Down,
                ..
            }
        ) && let Some(drag) = &self.drag
        {
            self.local_source_drag_return_eligible
                .store(false, Ordering::Release);
            drag.cancel_with_reason("local_escape");
        }
        if let InputEvent::MouseButton { button, transition } = event {
            let bit = mouse_button_bit(button);
            match transition {
                KeyTransition::Down => {
                    self.local_button_mask.fetch_or(bit, Ordering::AcqRel);
                }
                KeyTransition::Up => {
                    self.local_button_mask.fetch_and(!bit, Ordering::AcqRel);
                }
            }
        }
        if self.observe_emergency(event) {
            return true;
        }
        match (LiveRoute::load(self.route.load(Ordering::Acquire)), event) {
            (LiveRoute::Local, InputEvent::MouseMove { x, y }) => {
                self.handle_local_pointer(x, y, mouse_time_ms)
            }
            (
                LiveRoute::ControllingPeer | LiveRoute::EnteringPeer,
                InputEvent::MouseMove { .. },
            ) => {
                hide_cursor_shape_on_hook_thread();
                true
            }
            (LiveRoute::ControllingPeer | LiveRoute::EnteringPeer, _) => {
                self.handle_remote_reliable(event)
            }
            (LiveRoute::ControlledByPeer, _) => self.handle_controlled_target_input(event),
            (LiveRoute::Local, _) => false,
        }
    }
}

const fn should_cancel_local_drag_after_left_release(
    released_local_target: bool,
    local_route: bool,
    returned_source_pending: bool,
) -> bool {
    !released_local_target && local_route && !returned_source_pending
}

impl InputHookHandler for LiveContext {
    fn screen_contains(&self, x: i32, y: i32) -> bool {
        x >= 0
            && y >= 0
            && x < i32::try_from(self.local_screen.width).expect("screen width fits i32")
            && y < i32::try_from(self.local_screen.height).expect("screen height fits i32")
    }

    fn handle_input(&self, event: InputEvent) -> bool {
        self.handle_event(event, None)
    }

    fn handle_mouse_input(&self, event: InputEvent, message_time_ms: u32) -> bool {
        self.handle_event(event, Some(message_time_ms))
    }

    fn handle_raw_mouse(&self, delta_x: i32, delta_y: i32, message_time_ms: u32) {
        match LiveRoute::load(self.route.load(Ordering::Acquire)) {
            LiveRoute::Local => {
                self.record_raw_boundary_probe(delta_x, delta_y, message_time_ms);
                let calibration = self.pointer_gain.try_lock().ok().map(|mut calibration| {
                    let gain = calibration.model();
                    let (matched_processed, edge_overflow) =
                        calibration.record_raw_with_match(delta_x, delta_y, message_time_ms);
                    (gain, matched_processed, edge_overflow)
                });
                if let Some((gain, matched_processed, edge_overflow)) = calibration {
                    self.enqueue_raw_boundary_transition(
                        delta_x,
                        delta_y,
                        message_time_ms,
                        gain,
                        matched_processed,
                        edge_overflow,
                    );
                }
                let edge_overflow = calibration.and_then(|(_, _, edge_overflow)| edge_overflow);
                if let Some(candidate) = edge_overflow {
                    if let Ok(mut recent) = self.recent_edge_overflow.try_lock() {
                        *recent = Some(candidate);
                    }
                    if let Ok(mut pending) = self.pending_enter_motion.try_lock() {
                        pending.apply_edge_overflow(candidate);
                    }
                }
                if let Ok(mut pending) = self.pending_enter_motion.try_lock() {
                    pending.record(delta_x, delta_y, message_time_ms);
                }
            }
            LiveRoute::ControllingPeer | LiveRoute::EnteringPeer => {
                self.handle_remote_pointer(delta_x, delta_y, message_time_ms);
            }
            LiveRoute::ControlledByPeer => {}
        }
    }
}

fn advance_remote_pointer(
    pointer: &mut RemotePointerState,
    delta_x: i32,
    delta_y: i32,
) -> Option<LiveSignal> {
    if delta_x == 0 && delta_y == 0 {
        return None;
    }
    // RAWMOUSE supplies hardware-relative movement independent of the source cursor's clamped
    // absolute position. Accumulating it in the peer coordinate space avoids speed-dependent edge
    // loss and never needs to move the real source cursor to a centre anchor.
    let next_x = i64::from(pointer.position.x) + i64::from(delta_x);
    let next_y = i64::from(pointer.position.y) + i64::from(delta_y);
    pointer.position.x = u32::try_from(next_x.clamp(0, i64::from(pointer.screen.width - 1)))
        .expect("clamped remote x fits u32");
    pointer.position.y = u32::try_from(next_y.clamp(0, i64::from(pointer.screen.height - 1)))
        .expect("clamped remote y fits u32");
    let remote_width = i32::try_from(pointer.screen.width).expect("remote width fits i32");
    let remote_height = i32::try_from(pointer.screen.height).expect("remote height fits i32");
    let remote_position = NormalizedMousePosition {
        x: normalize_physical_coordinate(
            i32::try_from(pointer.position.x).expect("remote x fits i32"),
            remote_width,
        ),
        y: normalize_physical_coordinate(
            i32::try_from(pointer.position.y).expect("remote y fits i32"),
            remote_height,
        ),
    };
    Some(LiveSignal::Pointer {
        sample: PointerSample {
            position: pointer.position,
            delta_x,
            delta_y,
        },
        remote_position: Some(remote_position),
    })
}

fn tick_is_strictly_after(candidate: u32, reference: u32) -> bool {
    let elapsed = candidate.wrapping_sub(reference);
    elapsed != 0 && elapsed < (1_u32 << 31)
}

fn hide_cursor_shape_on_hook_thread() {
    // SAFETY: NULL removes the current cursor shape and does not transfer ownership. Running this
    // after each suppressed physical move prevents a foreground window's delayed WM_SETCURSOR
    // from making the source-side boundary cursor visible again.
    let _ = unsafe { SetCursor(None) };
}

const fn modifier_bit(virtual_key: u16) -> Option<u16> {
    match virtual_key {
        VK_CONTROL_VALUE => Some(MODIFIER_GENERIC_CONTROL),
        VK_LCONTROL_VALUE => Some(MODIFIER_LEFT_CONTROL),
        VK_RCONTROL_VALUE => Some(MODIFIER_RIGHT_CONTROL),
        VK_MENU_VALUE => Some(MODIFIER_GENERIC_ALT),
        VK_LMENU_VALUE => Some(MODIFIER_LEFT_ALT),
        VK_RMENU_VALUE => Some(MODIFIER_RIGHT_ALT),
        VK_SHIFT_VALUE => Some(MODIFIER_GENERIC_SHIFT),
        VK_LSHIFT_VALUE => Some(MODIFIER_LEFT_SHIFT),
        VK_RSHIFT_VALUE => Some(MODIFIER_RIGHT_SHIFT),
        _ => None,
    }
}

const fn is_modifier_key(event: InputEvent) -> bool {
    matches!(
        event,
        InputEvent::Key { virtual_key, .. } if modifier_bit(virtual_key).is_some()
    )
}

const fn mouse_button_bit(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1 << 0,
        MouseButton::Right => 1 << 1,
        MouseButton::Middle => 1 << 2,
        MouseButton::X1 => 1 << 3,
        MouseButton::X2 => 1 << 4,
    }
}

fn button_events(mask: u8, transition: KeyTransition) -> Vec<InputEvent> {
    let mut ordered = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::X1,
        MouseButton::X2,
    ];
    if transition == KeyTransition::Up {
        ordered.reverse();
    }
    ordered
        .into_iter()
        .filter(|button| mask & mouse_button_bit(*button) != 0)
        .map(|button| InputEvent::MouseButton { button, transition })
        .collect()
}

fn remote_event_from_input(event: InputEvent) -> Option<RemoteInputEvent> {
    match event {
        InputEvent::Key {
            physical,
            virtual_key,
            transition,
        } => Some(RemoteInputEvent::Key {
            physical,
            virtual_key,
            transition,
        }),
        InputEvent::MouseButton { button, transition } => {
            Some(RemoteInputEvent::MouseButton { button, transition })
        }
        InputEvent::Wheel { axis, delta } => Some(RemoteInputEvent::Wheel { axis, delta }),
        InputEvent::MouseMove { .. } => None,
    }
}

enum InjectionCommand {
    Input(InputEvent, Instant),
    Position(PhysicalPoint, mpsc::SyncSender<io::Result<()>>),
    PositionAsync(PhysicalPoint),
    PrepareSource(Vec<InputEvent>, bool, mpsc::SyncSender<io::Result<()>>),
    RestoreSource(
        PhysicalPoint,
        Vec<InputEvent>,
        mpsc::SyncSender<io::Result<()>>,
    ),
    Visibility(bool, mpsc::SyncSender<io::Result<()>>),
    VisibilityAsync(bool),
}

enum InjectionQueueItem {
    ReleaseAll,
    RepairSourceCursor,
    Command(InjectionCommand),
}

struct InjectionQueueState {
    commands: VecDeque<InjectionCommand>,
    repair_source_cursor: bool,
    release_all: bool,
    closed: bool,
}

struct InjectionQueue {
    state: Mutex<InjectionQueueState>,
    ready: Condvar,
    coalesced_mouse_moves: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TargetInjectionTimingSnapshot {
    moves: u64,
    max_gap_micros: u64,
    gaps_over_12_ms: u64,
    gaps_over_20_ms: u64,
    gaps_over_33_ms: u64,
    gaps_over_50_ms: u64,
    gaps_over_100_ms: u64,
    queue_delay_total_micros: u64,
    queue_delay_max_micros: u64,
    queue_delays_over_2_ms: u64,
    queue_delays_over_8_ms: u64,
    queue_delays_over_16_ms: u64,
}

impl TargetInjectionTimingSnapshot {
    fn average_queue_delay_micros(self) -> u64 {
        self.queue_delay_total_micros
            .checked_div(self.moves)
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct TargetInjectionTiming {
    last_injected_micros: AtomicU64,
    moves: AtomicU64,
    max_gap_micros: AtomicU64,
    gaps_over_12_ms: AtomicU64,
    gaps_over_20_ms: AtomicU64,
    gaps_over_33_ms: AtomicU64,
    gaps_over_50_ms: AtomicU64,
    gaps_over_100_ms: AtomicU64,
    queue_delay_total_micros: AtomicU64,
    queue_delay_max_micros: AtomicU64,
    queue_delays_over_2_ms: AtomicU64,
    queue_delays_over_8_ms: AtomicU64,
    queue_delays_over_16_ms: AtomicU64,
}

impl TargetInjectionTiming {
    fn observe(&self, queued_at: Instant) {
        let now = injection_timing_micros();
        let previous = self.last_injected_micros.swap(now, Ordering::Relaxed);
        self.moves.fetch_add(1, Ordering::Relaxed);
        if previous != 0 {
            let gap = now.saturating_sub(previous);
            self.max_gap_micros.fetch_max(gap, Ordering::Relaxed);
            if gap > 12_000 {
                self.gaps_over_12_ms.fetch_add(1, Ordering::Relaxed);
            }
            if gap > 20_000 {
                self.gaps_over_20_ms.fetch_add(1, Ordering::Relaxed);
            }
            if gap > 33_000 {
                self.gaps_over_33_ms.fetch_add(1, Ordering::Relaxed);
            }
            if gap > 50_000 {
                self.gaps_over_50_ms.fetch_add(1, Ordering::Relaxed);
            }
            if gap > 100_000 {
                self.gaps_over_100_ms.fetch_add(1, Ordering::Relaxed);
            }
        }
        let queue_delay = elapsed_micros(queued_at.elapsed());
        self.queue_delay_total_micros
            .fetch_add(queue_delay, Ordering::Relaxed);
        self.queue_delay_max_micros
            .fetch_max(queue_delay, Ordering::Relaxed);
        if queue_delay > 2_000 {
            self.queue_delays_over_2_ms.fetch_add(1, Ordering::Relaxed);
        }
        if queue_delay > 8_000 {
            self.queue_delays_over_8_ms.fetch_add(1, Ordering::Relaxed);
        }
        if queue_delay > 16_000 {
            self.queue_delays_over_16_ms.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn take(&self) -> TargetInjectionTimingSnapshot {
        TargetInjectionTimingSnapshot {
            moves: self.moves.swap(0, Ordering::Relaxed),
            max_gap_micros: self.max_gap_micros.swap(0, Ordering::Relaxed),
            gaps_over_12_ms: self.gaps_over_12_ms.swap(0, Ordering::Relaxed),
            gaps_over_20_ms: self.gaps_over_20_ms.swap(0, Ordering::Relaxed),
            gaps_over_33_ms: self.gaps_over_33_ms.swap(0, Ordering::Relaxed),
            gaps_over_50_ms: self.gaps_over_50_ms.swap(0, Ordering::Relaxed),
            gaps_over_100_ms: self.gaps_over_100_ms.swap(0, Ordering::Relaxed),
            queue_delay_total_micros: self.queue_delay_total_micros.swap(0, Ordering::Relaxed),
            queue_delay_max_micros: self.queue_delay_max_micros.swap(0, Ordering::Relaxed),
            queue_delays_over_2_ms: self.queue_delays_over_2_ms.swap(0, Ordering::Relaxed),
            queue_delays_over_8_ms: self.queue_delays_over_8_ms.swap(0, Ordering::Relaxed),
            queue_delays_over_16_ms: self.queue_delays_over_16_ms.swap(0, Ordering::Relaxed),
        }
    }
}

fn injection_timing_micros() -> u64 {
    static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    elapsed_micros(STARTED.get_or_init(Instant::now).elapsed()).saturating_add(1)
}

impl InjectionQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(InjectionQueueState {
                commands: VecDeque::with_capacity(INJECTION_QUEUE_CAPACITY),
                repair_source_cursor: false,
                release_all: false,
                closed: false,
            }),
            ready: Condvar::new(),
            coalesced_mouse_moves: AtomicU64::new(0),
        }
    }

    fn push_input(&self, event: InputEvent) -> io::Result<()> {
        let queued_at = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("injection queue was poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injection queue is closed",
            ));
        }
        if event.is_mouse_move() {
            let pending_mouse_moves = state
                .commands
                .iter()
                .rev()
                .take_while(|command| {
                    matches!(command, InjectionCommand::Input(previous, _) if previous.is_mouse_move())
                })
                .count();
            if pending_mouse_moves >= TARGET_MOUSE_BURST_CAPACITY
                && let Some(InjectionCommand::Input(previous, previous_queued_at)) =
                    state.commands.back_mut()
            {
                *previous = event;
                *previous_queued_at = queued_at;
                self.coalesced_mouse_moves.fetch_add(1, Ordering::Relaxed);
                self.ready.notify_one();
                return Ok(());
            }
        }
        if state.commands.len() >= INJECTION_QUEUE_CAPACITY {
            return Err(io::Error::other(
                "injection queue reached its fixed capacity",
            ));
        }
        state
            .commands
            .push_back(InjectionCommand::Input(event, queued_at));
        self.ready.notify_one();
        Ok(())
    }

    fn take_coalesced_mouse_moves(&self) -> u64 {
        self.coalesced_mouse_moves.swap(0, Ordering::Relaxed)
    }

    fn request_release_all(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.commands.retain(|command| {
            !matches!(
                command,
                InjectionCommand::Input(_, _) | InjectionCommand::PositionAsync(_)
            )
        });
        state.repair_source_cursor = false;
        state.release_all = true;
        self.ready.notify_one();
    }

    fn position(&self, point: PhysicalPoint) -> io::Result<()> {
        self.synchronous(|completion| InjectionCommand::Position(point, completion))
    }

    fn position_async(&self, point: PhysicalPoint) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("injection queue was poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injection queue is closed",
            ));
        }
        if state.commands.len() >= INJECTION_QUEUE_CAPACITY {
            return Err(io::Error::other(
                "injection queue reached its fixed capacity",
            ));
        }
        state
            .commands
            .push_back(InjectionCommand::PositionAsync(point));
        self.ready.notify_one();
        Ok(())
    }

    fn repair_source_cursor_async(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("injection queue was poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injection queue is closed",
            ));
        }
        // Cursor-shape repair is replaceable state, not reliable input. A stalled worker retains
        // only one pending request and can never fill the bounded command lane.
        state.repair_source_cursor = true;
        self.ready.notify_one();
        Ok(())
    }

    fn prepare_source(&self, releases: Vec<InputEvent>, confine_cursor: bool) -> io::Result<()> {
        self.synchronous(|completion| {
            InjectionCommand::PrepareSource(releases, confine_cursor, completion)
        })
    }

    fn restore_source(&self, point: PhysicalPoint, presses: Vec<InputEvent>) -> io::Result<()> {
        self.synchronous(|completion| InjectionCommand::RestoreSource(point, presses, completion))
    }

    fn set_visible(&self, visible: bool) -> io::Result<()> {
        self.synchronous(|completion| InjectionCommand::Visibility(visible, completion))
    }

    fn set_visible_async(&self, visible: bool) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("injection queue was poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injection queue is closed",
            ));
        }
        if let Some(InjectionCommand::VisibilityAsync(previous)) = state.commands.back_mut() {
            *previous = visible;
        } else {
            if state.commands.len() >= INJECTION_QUEUE_CAPACITY {
                return Err(io::Error::other(
                    "injection queue reached its fixed capacity",
                ));
            }
            state
                .commands
                .push_back(InjectionCommand::VisibilityAsync(visible));
        }
        self.ready.notify_one();
        Ok(())
    }

    fn synchronous(
        &self,
        command: impl FnOnce(mpsc::SyncSender<io::Result<()>>) -> InjectionCommand,
    ) -> io::Result<()> {
        let (sender, receiver) = mpsc::sync_channel(0);
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("injection queue was poisoned"))?;
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injection queue is closed",
                ));
            }
            if state.commands.len() >= INJECTION_QUEUE_CAPACITY {
                return Err(io::Error::other(
                    "injection queue reached its fixed capacity",
                ));
            }
            state.commands.push_back(command(sender));
            self.ready.notify_one();
        }
        receiver
            .recv_timeout(INJECTION_CONTROL_TIMEOUT)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the injection worker",
                )
            })?
    }

    fn pop(&self) -> Option<InjectionQueueItem> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.release_all {
                state.release_all = false;
                return Some(InjectionQueueItem::ReleaseAll);
            }
            if let Some(command) = state.commands.pop_front() {
                return Some(InjectionQueueItem::Command(command));
            }
            if state.repair_source_cursor {
                state.repair_source_cursor = false;
                return Some(InjectionQueueItem::RepairSourceCursor);
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
}

#[derive(Debug, Default)]
struct InjectionOutcome {
    injected_events: u64,
    forced_releases: u64,
    remaining_inputs: usize,
    failure: Option<String>,
}

pub(super) struct SourceCursorShieldWindow {
    handle: HWND,
    instance: HINSTANCE,
}

impl SourceCursorShieldWindow {
    pub(super) fn create(screen: ScreenExtent, departure: ScreenBoundary) -> io::Result<Self> {
        // SAFETY: the current executable module remains loaded for the process lifetime.
        let module = unsafe { GetModuleHandleW(None) }.map_err(io::Error::other)?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(source_cursor_shield_window_procedure),
            hInstance: instance,
            // A null class cursor is essential: DefWindowProc otherwise restores the foreground
            // application's class cursor on every physical movement.
            hCursor: HCURSOR::default(),
            lpszClassName: SOURCE_CURSOR_SHIELD_CLASS,
            ..WNDCLASSW::default()
        };
        // SAFETY: the descriptor and static UTF-16 class name remain valid.
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let (x, y, width, height) = source_cursor_shield_bounds(screen, departure);
        let handle = unsafe {
            // SAFETY: this creates one hidden, non-activating top-level window owned by the hook
            // thread. While this computer controls its peer, the window covers the complete local
            // primary screen so a hidden source cursor cannot hover or click any local UI. The
            // departure argument is retained in the geometry helper to make layout independence
            // explicit and regression-test all four placements.
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                SOURCE_CURSOR_SHIELD_CLASS,
                w!("Pierdesk Source Cursor Shield"),
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
            // SAFETY: class registration succeeded above and no window was returned.
            let _ = unsafe { UnregisterClassW(SOURCE_CURSOR_SHIELD_CLASS, Some(instance)) };
            io::Error::other(error)
        })?;
        // Alpha zero is excluded from layered-window hit testing. Alpha one is visually
        // imperceptible but keeps this window as the cursor owner so WM_SETCURSOR can reliably
        // suppress the source cursor without altering the user's system cursor scheme.
        if let Err(error) = unsafe {
            // SAFETY: handle identifies the live layered window created above.
            SetLayeredWindowAttributes(handle, COLORREF(0), SOURCE_CURSOR_SHIELD_ALPHA, LWA_ALPHA)
        } {
            // SAFETY: handle and class are owned by this partially constructed guard.
            let _ = unsafe { DestroyWindow(handle) };
            let _ = unsafe { UnregisterClassW(SOURCE_CURSOR_SHIELD_CLASS, Some(instance)) };
            return Err(io::Error::other(error));
        }
        Ok(Self { handle, instance })
    }

    pub(super) fn controller(&self) -> SourceCursorShieldController {
        SourceCursorShieldController {
            handle: self.handle.0 as usize,
        }
    }
}

const fn source_cursor_shield_bounds(
    screen: ScreenExtent,
    _departure: ScreenBoundary,
) -> (i32, i32, i32, i32) {
    (0, 0, screen.width, screen.height)
}

impl Drop for SourceCursorShieldWindow {
    fn drop(&mut self) {
        // SAFETY: this guard owns the window and class and is dropped on their creating thread.
        let _ = unsafe { DestroyWindow(self.handle) };
        let _ = unsafe { UnregisterClassW(SOURCE_CURSOR_SHIELD_CLASS, Some(self.instance)) };
    }
}

#[derive(Debug, Default)]
pub(super) struct SourceCursorConfinement {
    previous_clip: Option<RECT>,
    anchor: Option<POINT>,
}

impl SourceCursorConfinement {
    pub(super) fn confine_current(&mut self) -> io::Result<POINT> {
        if let Some(anchor) = self.anchor {
            return Ok(anchor);
        }
        let mut previous_clip = RECT::default();
        unsafe { GetClipCursor(&raw mut previous_clip) }.map_err(io::Error::other)?;
        let mut anchor = POINT::default();
        unsafe { GetCursorPos(&raw mut anchor) }.map_err(io::Error::other)?;
        let confined = RECT {
            left: anchor.x,
            top: anchor.y,
            right: anchor.x.saturating_add(1),
            bottom: anchor.y.saturating_add(1),
        };
        unsafe { ClipCursor(Some(&raw const confined)) }.map_err(io::Error::other)?;
        self.previous_clip = Some(previous_clip);
        self.anchor = Some(anchor);
        Ok(anchor)
    }

    pub(super) fn release(&mut self) -> io::Result<bool> {
        let Some(previous_clip) = self.previous_clip else {
            self.anchor = None;
            return Ok(false);
        };
        if let Err(restore_error) = unsafe { ClipCursor(Some(&raw const previous_clip)) } {
            // Never leave the user's cursor trapped if the old clipping rectangle became invalid
            // after a display-mode change. Releasing all confinement is the safe fallback.
            unsafe { ClipCursor(None) }.map_err(|release_error| {
                io::Error::other(format!(
                    "could not restore cursor clipping ({restore_error}) or release it ({release_error})"
                ))
            })?;
        }
        self.previous_clip = None;
        self.anchor = None;
        Ok(true)
    }
}

impl Drop for SourceCursorConfinement {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SourceCursorShieldController {
    handle: usize,
}

impl SourceCursorShieldController {
    pub(super) fn set_active(self, active: bool) -> io::Result<()> {
        let handle = HWND(self.handle as *mut core::ffi::c_void);
        // Cross-thread SetWindowPos can synchronously wait hundreds of milliseconds for a busy
        // desktop thread. Post a pointer-free command instead; the hook/message thread owns the
        // shield and applies the visibility change without delaying a control acknowledgement.
        // SAFETY: the creating hook thread retains the window until after all workers join and the
        // private message contains only a Boolean value.
        unsafe {
            PostMessageW(
                Some(handle),
                SOURCE_CURSOR_SHIELD_SET_ACTIVE,
                WPARAM(usize::from(active)),
                LPARAM(0),
            )
        }
        .map_err(io::Error::other)
    }
}

fn apply_source_cursor_shield_visibility(window: HWND, active: bool) -> io::Result<()> {
    let visibility = if active {
        SWP_SHOWWINDOW
    } else {
        SWP_HIDEWINDOW
    };
    // SAFETY: this helper is called by the shield window procedure on its owning thread.
    unsafe {
        SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | visibility,
        )
    }
    .map_err(io::Error::other)
}

unsafe extern "system" fn source_cursor_shield_window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        SOURCE_CURSOR_SHIELD_SET_ACTIVE => {
            let _ = apply_source_cursor_shield_visibility(window, wparam.0 != 0);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // SAFETY: NULL is the documented cursor value for hiding a cursor whose window class
            // also has a null cursor. The active shield owns the complete local primary screen.
            let _ = unsafe { SetCursor(None) };
            LRESULT(1)
        }
        WM_MOUSEACTIVATE => LRESULT(isize::try_from(MA_NOACTIVATE).unwrap()),
        WM_NCHITTEST => LRESULT(isize::try_from(HTCLIENT).unwrap()),
        // SAFETY: unhandled messages retain the standard top-level window behavior.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[derive(Debug, Default)]
struct CursorVisibilityBalance {
    hide_calls: u32,
}

/// A thread-owned, balanced visibility guard for the workspace's native owner.
/// Clearing the cursor shape alone does not prevent a later shape change from
/// displaying a source cursor while input is forwarded to another computer.
#[derive(Debug, Default)]
pub(super) struct SourceCursorVisibility(CursorVisibilityBalance);

impl SourceCursorVisibility {
    pub(super) fn hide(&mut self) -> io::Result<()> {
        self.0.hide_with(|show| {
            // SAFETY: called on the native owner thread; every decrement is retained.
            unsafe { ShowCursor(show) }
        })
    }

    pub(super) fn restore(&mut self) {
        self.0.restore_with(|show| {
            // SAFETY: restore only our own decrements, on their originating thread.
            unsafe { ShowCursor(show) }
        });
    }
}

impl Drop for SourceCursorVisibility {
    fn drop(&mut self) {
        self.restore();
    }
}

impl CursorVisibilityBalance {
    fn hide_with(&mut self, mut adjust: impl FnMut(bool) -> i32) -> io::Result<()> {
        if self.hide_calls != 0 {
            return Ok(());
        }
        for _ in 0..MAX_CURSOR_VISIBILITY_ADJUSTMENTS {
            let display_count = adjust(false);
            self.hide_calls += 1;
            if display_count < 0 {
                return Ok(());
            }
        }
        Err(io::Error::other(
            "cursor display counter did not reach the hidden state",
        ))
    }

    fn restore_with(&mut self, mut adjust: impl FnMut(bool) -> i32) {
        for _ in 0..self.hide_calls {
            let _ = adjust(true);
        }
        self.hide_calls = 0;
    }

    fn ensure_visible_with(&mut self, mut adjust: impl FnMut(bool) -> i32) -> io::Result<()> {
        self.restore_with(&mut adjust);
        for _ in 0..MAX_CURSOR_VISIBILITY_ADJUSTMENTS {
            let display_count = adjust(true);
            if display_count == 0 {
                return Ok(());
            }
            if display_count > 0 {
                // The cursor was already visible. Undo the probe so repeated health checks do not
                // continuously inflate the thread's ShowCursor display counter.
                let _ = adjust(false);
                return Ok(());
            }
        }
        Err(io::Error::other(
            "cursor display counter did not reach the visible state",
        ))
    }
}

#[derive(Debug, Default)]
pub(super) struct CursorShapeCapture {
    previous: Option<HCURSOR>,
    active: bool,
}

impl CursorShapeCapture {
    pub(super) fn hide(&mut self) {
        // SAFETY: NULL removes the shared cursor shape without taking ownership of the returned
        // system cursor handle. Repeated calls repair foreground WM_SETCURSOR changes while the
        // physical source cursor stays fixed at the departure boundary.
        self.hide_with(|| unsafe { SetCursor(None) });
    }

    fn hide_with(&mut self, hide: impl FnOnce() -> HCURSOR) {
        let previous = hide();
        if self.previous.is_none() && !previous.is_invalid() {
            self.previous = Some(previous);
        }
        self.active = true;
    }

    pub(super) fn restore(&mut self) {
        self.restore_with(|previous| {
            // SAFETY: previous is a borrowed system cursor handle returned by SetCursor and
            // remains valid; ownership is not transferred or destroyed here.
            let _ = unsafe { SetCursor(Some(previous)) };
        });
    }

    fn restore_with(&mut self, restore: impl FnOnce(HCURSOR)) {
        if !self.active {
            return;
        }
        if let Some(previous) = self.previous.take() {
            restore(previous);
        }
        self.active = false;
    }
}

fn injection_loop(
    queue: &InjectionQueue,
    shared: &LiveContext,
    screen: ScreenExtent,
    source_cursor_shield: SourceCursorShieldController,
) -> InjectionOutcome {
    let mut endpoint = SendInputEndpoint::new(screen);
    let mut cursor_visibility = CursorVisibilityBalance::default();
    let mut cursor_shape = CursorShapeCapture::default();
    let mut source_cursor_confinement = SourceCursorConfinement::default();
    let mut next_visibility_repair = Instant::now();
    let mut outcome = InjectionOutcome::default();
    while let Some(command) = queue.pop() {
        let mut process_context = InjectionProcessContext {
            outcome: &mut outcome,
            source_cursor_shield,
            shared,
        };
        let result = process_injection_item(
            command,
            &mut endpoint,
            &mut cursor_visibility,
            &mut cursor_shape,
            &mut source_cursor_confinement,
            &mut next_visibility_repair,
            &mut process_context,
        );
        if let Err(error) = result {
            let message = format!("SendInput worker failed: {error}");
            outcome.failure = Some(message.clone());
            shared.fail(LiveStopCode::InjectionFailure, message);
            break;
        }
    }
    let (released, release_error) = endpoint.release_all();
    outcome.forced_releases += u64::try_from(released).unwrap_or(u64::MAX);
    let _ = source_cursor_shield.set_active(false);
    let _ = source_cursor_confinement.release();
    cursor_shape.restore();
    let _ = set_cursor_visibility(true, &mut cursor_visibility);
    if let Some(error) = release_error {
        let message = format!("SendInput release-all failed: {error}");
        outcome.failure.get_or_insert_with(|| message.clone());
        shared.fail(LiveStopCode::InjectionFailure, message);
    }
    outcome.injected_events = endpoint.injected_events();
    outcome.remaining_inputs = endpoint.outstanding_count();
    outcome
}

struct InjectionProcessContext<'a> {
    outcome: &'a mut InjectionOutcome,
    source_cursor_shield: SourceCursorShieldController,
    shared: &'a LiveContext,
}

fn spawn_injection_worker(
    queue: Arc<InjectionQueue>,
    shared: Arc<LiveContext>,
    screen: ScreenExtent,
    source_cursor_shield: SourceCursorShieldController,
) -> io::Result<JoinHandle<InjectionOutcome>> {
    std::thread::Builder::new()
        .name("pierdesk-live-input-injector".to_owned())
        .spawn(move || {
            #[cfg(not(test))]
            crate::traffic_control::promote_interactive_thread();
            injection_loop(&queue, &shared, screen, source_cursor_shield)
        })
}

fn process_injection_item(
    item: InjectionQueueItem,
    endpoint: &mut SendInputEndpoint,
    cursor_visibility: &mut CursorVisibilityBalance,
    cursor_shape: &mut CursorShapeCapture,
    source_cursor_confinement: &mut SourceCursorConfinement,
    next_visibility_repair: &mut Instant,
    context: &mut InjectionProcessContext<'_>,
) -> io::Result<()> {
    match item {
        InjectionQueueItem::ReleaseAll => {
            let (released, error) = endpoint.release_all();
            context.outcome.forced_releases += u64::try_from(released).unwrap_or(u64::MAX);
            error.map_or(Ok(()), Err)
        }
        // Foreground windows may replace the cursor shape. Repair it without moving the source
        // cursor or touching the balanced ShowCursor count.
        InjectionQueueItem::RepairSourceCursor => {
            context.source_cursor_shield.set_active(true)?;
            cursor_shape.hide();
            Ok(())
        }
        InjectionQueueItem::Command(command) => process_injection_command(
            command,
            endpoint,
            cursor_visibility,
            cursor_shape,
            source_cursor_confinement,
            next_visibility_repair,
            context,
        ),
    }
}

fn process_injection_command(
    command: InjectionCommand,
    endpoint: &mut SendInputEndpoint,
    cursor_visibility: &mut CursorVisibilityBalance,
    cursor_shape: &mut CursorShapeCapture,
    source_cursor_confinement: &mut SourceCursorConfinement,
    next_visibility_repair: &mut Instant,
    context: &mut InjectionProcessContext<'_>,
) -> io::Result<()> {
    match command {
        InjectionCommand::Input(event, queued_at) => {
            let is_mouse_move = event.is_mouse_move();
            let now = Instant::now();
            let result = if is_mouse_move && now >= *next_visibility_repair {
                *next_visibility_repair = now + CURSOR_VISIBILITY_REPAIR_INTERVAL;
                set_cursor_visibility(true, cursor_visibility).and_then(|()| endpoint.inject(event))
            } else {
                endpoint.inject(event)
            };
            if result.is_ok() && is_mouse_move {
                context.shared.target_injection_timing.observe(queued_at);
                context.shared.record_target_first_remote_move_injected();
            }
            result
        }
        InjectionCommand::Position(point, completion) => {
            let result = inject_point(endpoint, point);
            let _ = completion.send(clone_io_result(&result));
            result
        }
        InjectionCommand::PositionAsync(point) => {
            let started_at = Instant::now();
            let result = inject_point(endpoint, point);
            if result.is_ok() {
                context.shared.record_trace(format!(
                    "kind=target_cursor_positioned,position={},{} inject_us={}",
                    point.x,
                    point.y,
                    elapsed_micros(started_at.elapsed())
                ));
            }
            result
        }
        InjectionCommand::PrepareSource(releases, confine_cursor, completion) => {
            let result = releases
                .into_iter()
                .try_for_each(|event| endpoint.inject_stateless(event))
                .and_then(|()| {
                    if confine_cursor {
                        let anchor = source_cursor_confinement.confine_current()?;
                        context.shared.record_trace(format!(
                            "kind=source_cursor_confined,position={},{}",
                            anchor.x, anchor.y,
                        ));
                    }
                    Ok(())
                })
                .and_then(|()| context.source_cursor_shield.set_active(true))
                .map(|()| {
                    // ShowCursor and SetCursor remain best-effort fallbacks for systems that do
                    // not composite the shield immediately. The owned shield is authoritative.
                    let _ = set_cursor_visibility(false, cursor_visibility);
                    cursor_shape.hide();
                });
            if result.is_err() {
                let _ = source_cursor_confinement.release();
                let _ = context.source_cursor_shield.set_active(false);
                cursor_shape.restore();
                let _ = set_cursor_visibility(true, cursor_visibility);
            }
            let _ = completion.send(clone_io_result(&result));
            result
        }
        InjectionCommand::RestoreSource(point, presses, completion) => {
            let confinement_result = source_cursor_confinement.release();
            let shield_result = context.source_cursor_shield.set_active(false);
            cursor_shape.restore();
            let result = confinement_result
                .and(shield_result)
                .and_then(|()| set_cursor_visibility(true, cursor_visibility))
                .and_then(|()| inject_point(endpoint, point))
                .and_then(|()| {
                    presses
                        .into_iter()
                        .try_for_each(|event| endpoint.inject_stateless(event))
                });
            let _ = completion.send(clone_io_result(&result));
            result
        }
        InjectionCommand::Visibility(visible, completion) => {
            let result = apply_cursor_visibility(
                visible,
                cursor_visibility,
                cursor_shape,
                source_cursor_confinement,
                context.source_cursor_shield,
            );
            let _ = completion.send(clone_io_result(&result));
            result
        }
        InjectionCommand::VisibilityAsync(visible) => apply_cursor_visibility(
            visible,
            cursor_visibility,
            cursor_shape,
            source_cursor_confinement,
            context.source_cursor_shield,
        ),
    }
}

fn apply_cursor_visibility(
    visible: bool,
    cursor_visibility: &mut CursorVisibilityBalance,
    cursor_shape: &mut CursorShapeCapture,
    source_cursor_confinement: &mut SourceCursorConfinement,
    source_cursor_shield: SourceCursorShieldController,
) -> io::Result<()> {
    if visible {
        source_cursor_confinement.release()?;
        let _ = source_cursor_shield.set_active(false);
        cursor_shape.restore();
    }
    let result = source_cursor_shield
        .set_active(!visible)
        .and_then(|()| set_cursor_visibility(visible, cursor_visibility));
    if result.is_ok() && !visible {
        cursor_shape.hide();
    }
    result
}

fn set_cursor_visibility(visible: bool, balance: &mut CursorVisibilityBalance) -> io::Result<()> {
    if visible {
        balance.ensure_visible_with(|show| {
            // SAFETY: ShowCursor accepts a Boolean and returns the cursor display counter. Every
            // decrement made by this balance is restored on return and teardown. The visibility
            // probe is immediately balanced when the counter was already non-negative.
            unsafe { ShowCursor(show) }
        })
    } else {
        balance.hide_with(|show| {
            // SAFETY: same contract as above; hide_with records every successful decrement.
            unsafe { ShowCursor(show) }
        })
    }
}

fn inject_point(endpoint: &mut SendInputEndpoint, point: PhysicalPoint) -> io::Result<()> {
    endpoint.inject(InputEvent::MouseMove {
        x: i32::try_from(point.x).expect("physical point x fits i32"),
        y: i32::try_from(point.y).expect("physical point y fits i32"),
    })
}

const fn inset_boundary_resume(point: PhysicalPoint, screen: PhysicalScreenSize) -> PhysicalPoint {
    let mut inset = point;
    if inset.x == 0 {
        inset.x = 1;
    } else if inset.x == screen.width - 1 {
        inset.x = screen.width - 2;
    }
    if inset.y == 0 {
        inset.y = 1;
    } else if inset.y == screen.height - 1 {
        inset.y = screen.height - 2;
    }
    inset
}

fn clone_io_result(result: &io::Result<()>) -> io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(io::Error::new(error.kind(), format!("{error}"))),
    }
}

fn elapsed_micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy)]
struct PendingEnter {
    peer_position: PhysicalPoint,
    local_resume: PhysicalPoint,
    requested_at: Instant,
}

struct NetworkOutcome {
    channel: super::channel::ChannelSnapshot,
}

#[derive(Default)]
struct RemoteShortcutState {
    control_epoch: Option<u64>,
    control_depth: u8,
    alt_depth: u8,
    shift_depth: u8,
    blocked_paste: Option<super::state::PhysicalKey>,
}

impl RemoteShortcutState {
    fn should_send(
        &mut self,
        event: RemoteInputEvent,
        control_epoch: Option<u64>,
        wait_for_paste: impl FnOnce() -> crate::clipboard::paste_fence::PasteFenceDecision,
    ) -> bool {
        if self.control_epoch != control_epoch {
            self.control_epoch = control_epoch;
            self.control_depth = 0;
            self.alt_depth = 0;
            self.shift_depth = 0;
            self.blocked_paste = None;
        }
        let RemoteInputEvent::Key {
            physical,
            virtual_key,
            transition,
        } = event
        else {
            return true;
        };
        if self.blocked_paste == Some(physical) {
            if transition == KeyTransition::Up {
                self.blocked_paste = None;
            }
            return false;
        }
        if matches!(
            virtual_key,
            VK_CONTROL_VALUE | VK_LCONTROL_VALUE | VK_RCONTROL_VALUE
        ) {
            match transition {
                KeyTransition::Down => self.control_depth = self.control_depth.saturating_add(1),
                KeyTransition::Up => self.control_depth = self.control_depth.saturating_sub(1),
            }
            return true;
        }
        if matches!(virtual_key, VK_MENU_VALUE | VK_LMENU_VALUE | VK_RMENU_VALUE) {
            match transition {
                KeyTransition::Down => self.alt_depth = self.alt_depth.saturating_add(1),
                KeyTransition::Up => self.alt_depth = self.alt_depth.saturating_sub(1),
            }
            return true;
        }
        if matches!(
            virtual_key,
            VK_SHIFT_VALUE | VK_LSHIFT_VALUE | VK_RSHIFT_VALUE
        ) {
            match transition {
                KeyTransition::Down => self.shift_depth = self.shift_depth.saturating_add(1),
                KeyTransition::Up => self.shift_depth = self.shift_depth.saturating_sub(1),
            }
            return true;
        }
        let is_paste =
            (virtual_key == VK_V_VALUE && self.control_depth != 0 && self.alt_depth == 0)
                || (virtual_key == VK_INSERT_VALUE
                    && self.shift_depth != 0
                    && self.control_depth == 0
                    && self.alt_depth == 0);
        if is_paste
            && transition == KeyTransition::Down
            && wait_for_paste() == crate::clipboard::paste_fence::PasteFenceDecision::Block
        {
            self.blocked_paste = Some(physical);
            return false;
        }
        true
    }
}

struct NetworkLoopContext<'a> {
    policy: BoundaryPolicy,
    store: &'a DeviceStore,
    peer_fingerprint: crate::security::CertificateFingerprint,
    shared: &'a LiveContext,
    injection: &'a InjectionQueue,
    source_cursor_shield: SourceCursorShieldController,
    session_control: Option<&'a LiveSessionControl>,
}

#[allow(clippy::too_many_lines)]
fn network_loop(
    mut channel: CrossScreenChannel,
    mut coordinator: ControlCoordinator,
    context: &NetworkLoopContext<'_>,
) -> NetworkOutcome {
    let mut pending_enter = None;
    let mut next_trust_check = Instant::now() + TRUST_RECHECK_INTERVAL;
    let mut next_source_cursor_repair = Instant::now() + SOURCE_CURSOR_REPAIR_INTERVAL;
    let mut remote_shortcuts = RemoteShortcutState::default();
    let mut next_input_diagnostic = Instant::now() + Duration::from_secs(5);
    let mut previous_iteration = Instant::now();
    let mut max_iteration_gap_us = 0;
    #[cfg(not(test))]
    let interactive_session =
        crate::traffic_control::InteractiveSession::new(channel.snapshot().session_id);
    while LiveStopCode::load(context.shared.stop.load(Ordering::Acquire)) == LiveStopCode::Running {
        let now = Instant::now();
        max_iteration_gap_us =
            max_iteration_gap_us.max(elapsed_micros(now.duration_since(previous_iteration)));
        previous_iteration = now;
        #[cfg(not(test))]
        interactive_session.set_active(
            LiveRoute::load(context.shared.route.load(Ordering::Acquire)) != LiveRoute::Local,
        );
        if Instant::now() >= next_trust_check {
            if let Err(error) = context.store.load_peer(context.peer_fingerprint) {
                context.shared.fail(
                    LiveStopCode::ConnectionFailure,
                    format!("cross-screen peer trust was revoked: {error}"),
                );
                break;
            }
            next_trust_check = Instant::now() + TRUST_RECHECK_INTERVAL;
        }
        let snapshot = channel.snapshot();
        if now >= next_input_diagnostic {
            let cadence = channel.take_input_cadence();
            let injection = context.shared.target_injection_timing.take();
            if let Some(control) = context.session_control {
                control.record_transition_trace(format!(
                    "kind=input_health,sent_input={},received_input={},coalesced_input={},forwarded_mouse={},max_loop_gap_us={},rtt_us={:?},route={},latency_qos={},latency_qos_error={:?},wire_enqueue_moves={},wire_enqueue_gap_max_us={},wire_enqueue_gt12ms={},wire_enqueue_gt20ms={},wire_enqueue_gt33ms={},wire_enqueue_gt50ms={},wire_enqueue_gt100ms={},wire_tx_moves={},wire_tx_gap_max_us={},wire_tx_gt12ms={},wire_tx_gt20ms={},wire_tx_gt33ms={},wire_tx_gt50ms={},wire_tx_gt100ms={},wire_rx_moves={},wire_rx_gap_max_us={},wire_rx_gt12ms={},wire_rx_gt20ms={},wire_rx_gt33ms={},wire_rx_gt50ms={},wire_rx_gt100ms={},inject_moves={},inject_gap_max_us={},inject_gt12ms={},inject_gt20ms={},inject_gt33ms={},inject_gt50ms={},inject_gt100ms={},inject_queue_avg_us={},inject_queue_max_us={},inject_queue_gt2ms={},inject_queue_gt8ms={},inject_queue_gt16ms={},inject_queue_coalesced_moves={}",
                    snapshot.sent_input_events,
                    snapshot.received_input_events,
                    snapshot.coalesced_input_mouse_moves,
                    context.shared.forwarded_mouse_moves.load(Ordering::Relaxed),
                    max_iteration_gap_us,
                    snapshot.round_trip_micros,
                    LiveRoute::load(context.shared.route.load(Ordering::Acquire)).label(),
                    if snapshot.latency_qos_enabled {
                        "control"
                    } else {
                        "unavailable"
                    },
                    snapshot.latency_qos_error,
                    cadence.enqueued.moves,
                    cadence.enqueued.max_gap_micros,
                    cadence.enqueued.gaps_over_12_ms,
                    cadence.enqueued.gaps_over_20_ms,
                    cadence.enqueued.gaps_over_33_ms,
                    cadence.enqueued.gaps_over_50_ms,
                    cadence.enqueued.gaps_over_100_ms,
                    cadence.sent.moves,
                    cadence.sent.max_gap_micros,
                    cadence.sent.gaps_over_12_ms,
                    cadence.sent.gaps_over_20_ms,
                    cadence.sent.gaps_over_33_ms,
                    cadence.sent.gaps_over_50_ms,
                    cadence.sent.gaps_over_100_ms,
                    cadence.received.moves,
                    cadence.received.max_gap_micros,
                    cadence.received.gaps_over_12_ms,
                    cadence.received.gaps_over_20_ms,
                    cadence.received.gaps_over_33_ms,
                    cadence.received.gaps_over_50_ms,
                    cadence.received.gaps_over_100_ms,
                    injection.moves,
                    injection.max_gap_micros,
                    injection.gaps_over_12_ms,
                    injection.gaps_over_20_ms,
                    injection.gaps_over_33_ms,
                    injection.gaps_over_50_ms,
                    injection.gaps_over_100_ms,
                    injection.average_queue_delay_micros(),
                    injection.queue_delay_max_micros,
                    injection.queue_delays_over_2_ms,
                    injection.queue_delays_over_8_ms,
                    injection.queue_delays_over_16_ms,
                    context.injection.take_coalesced_mouse_moves(),
                ));
            }
            max_iteration_gap_us = 0;
            next_input_diagnostic = now + Duration::from_secs(5);
        }
        if let Some(drag) = &context.shared.drag {
            let left = context.shared.local_button_mask.load(Ordering::Acquire)
                & mouse_button_bit(MouseButton::Left)
                != 0;
            let local = coordinator.state() == ControlProtocolState::Local;
            let controlled = coordinator.state() == ControlProtocolState::ControlledByPeer;
            if drag.source_id.load(Ordering::Acquire) == 0
                && coordinator.state() != ControlProtocolState::AwaitingReturnAcknowledgement
            {
                context
                    .shared
                    .local_source_drag_return_eligible
                    .store(false, Ordering::Release);
            }
            if drag.source_id.load(Ordering::Acquire) == 0 {
                drag.classify_idle_source(controlled);
            }
            let remote_left = context.shared.remote_left_down.load(Ordering::Acquire);
            drag.boundary_visible.store(
                (local && left) || (controlled && remote_left),
                Ordering::Release,
            );
            if controlled
                && drag.source_remote_controlled.load(Ordering::Acquire)
                && drag.source_ready.load(Ordering::Acquire)
                && !drag.source_active.load(Ordering::Acquire)
            {
                drag.source_active.store(true, Ordering::Release);
                let prepared = context
                    .source_cursor_shield
                    .set_active(true)
                    .and_then(|()| {
                        context.injection.prepare_source(
                            vec![InputEvent::MouseButton {
                                button: MouseButton::Left,
                                transition: KeyTransition::Up,
                            }],
                            false,
                        )
                    });
                if let Err(error) = prepared {
                    drag.cancel_with_reason("remote_source_injection_failed");
                    context
                        .shared
                        .fail(LiveStopCode::InjectionFailure, error.to_string());
                    break;
                }
                drag.activate_source();
            }
            if local && !left {
                let _ = drag.may_cross(false);
            }
            drag.expire_remote_source_left_up();
            while let Some(message) = drag.poll() {
                if let Err(error) = channel.send_file_drag(message) {
                    context
                        .shared
                        .fail(LiveStopCode::ConnectionFailure, error.to_string());
                    break;
                }
            }
        }
        if let Some(control) = context.session_control {
            control.update(|status| {
                status.round_trip_micros = snapshot.round_trip_micros;
                status.route =
                    LiveRoute::load(context.shared.route.load(Ordering::Acquire)).label();
            });
            if let Some(payload) = control.boundary_sync.take_outbound()
                && let Err(error) = channel.send_boundary_workspace(payload.clone())
            {
                control.boundary_sync.restore_outbound(payload);
                context
                    .shared
                    .fail(LiveStopCode::ConnectionFailure, error.to_string());
                break;
            }
        }
        match snapshot.state {
            ChannelState::Connected => {}
            ChannelState::Closing | ChannelState::Stopped if snapshot.last_error.is_none() => {
                context.shared.request_stop(LiveStopCode::ExternalStop);
                break;
            }
            ChannelState::Closing | ChannelState::Stopped | ChannelState::Failed => {
                context.shared.fail(
                    LiveStopCode::ConnectionFailure,
                    snapshot
                        .last_error
                        .unwrap_or_else(|| "cross-screen channel disconnected".to_owned()),
                );
                break;
            }
        }
        if let Err(error) =
            pump_channel_events(&channel, &mut coordinator, context, &mut pending_enter)
        {
            context
                .shared
                .fail(LiveStopCode::ConnectionFailure, error.to_string());
            break;
        }
        context
            .shared
            .flush_boundary_probe_events(context.session_control);
        if let Err(error) =
            maintain_source_cursor(coordinator.state(), context, &mut next_source_cursor_repair)
        {
            context
                .shared
                .fail(LiveStopCode::InjectionFailure, error.to_string());
            break;
        }
        let retry = context
            .shared
            .drag_retry
            .lock()
            .ok()
            .and_then(|mut retry| retry.take());
        let signal = context
            .shared
            .queue
            .pop_until(
                Instant::now()
                    + if retry.is_some() {
                        Duration::from_millis(8)
                    } else {
                        NETWORK_POLL_INTERVAL
                    },
            )
            .or(retry);
        if let Some(signal) = signal
            && let Err(error) = handle_live_signal(
                signal,
                &channel,
                &mut coordinator,
                context,
                &mut pending_enter,
                &mut remote_shortcuts,
            )
        {
            context
                .shared
                .fail(LiveStopCode::ConnectionFailure, error.to_string());
            break;
        }
    }

    #[cfg(not(test))]
    interactive_session.set_active(false);
    context
        .shared
        .flush_boundary_probe_events(context.session_control);
    let resume = context.shared.force_local();
    if let Some(control) = context.session_control {
        control.update(|status| {
            status.phase = LiveSessionPhase::Stopping;
            status.route = LiveRoute::Local.label();
        });
    }
    channel.request_release_all();
    context.injection.request_release_all();
    let _ = context.injection.set_visible(true);
    if let Some(resume) = resume {
        let _ = context.injection.position(resume);
    }
    let actions = coordinator.disconnect();
    let _ = dispatch_control_actions(&channel, actions, || {
        context.injection.request_release_all();
        Ok(())
    });
    channel.stop();
    NetworkOutcome {
        channel: channel.snapshot(),
    }
}

fn maintain_source_cursor(
    state: ControlProtocolState,
    context: &NetworkLoopContext<'_>,
    next_capture: &mut Instant,
) -> io::Result<()> {
    let now = Instant::now();
    if state != ControlProtocolState::ControllingPeer {
        *next_capture = now + SOURCE_CURSOR_REPAIR_INTERVAL;
        return Ok(());
    }
    if now < *next_capture {
        return Ok(());
    }
    *next_capture = now + SOURCE_CURSOR_REPAIR_INTERVAL;
    context
        .injection
        .repair_source_cursor_async()
        .map_err(|error| {
            io::Error::other(format!(
                "could not queue source cursor-shape repair: {error}"
            ))
        })
}

struct LiveNetworkWorker {
    channel: CrossScreenChannel,
    coordinator: ControlCoordinator,
    policy: BoundaryPolicy,
    store: DeviceStore,
    peer_fingerprint: crate::security::CertificateFingerprint,
    shared: Arc<LiveContext>,
    injection: Arc<InjectionQueue>,
    source_cursor_shield: SourceCursorShieldController,
    control: Option<Arc<LiveSessionControl>>,
}

impl LiveNetworkWorker {
    fn spawn(self) -> io::Result<JoinHandle<NetworkOutcome>> {
        std::thread::Builder::new()
            .name("pierdesk-live-control-network".to_owned())
            .spawn(move || {
                #[cfg(not(test))]
                crate::traffic_control::promote_interactive_thread();
                let shared = Arc::downgrade(&self.shared);
                self.channel.set_event_notifier(Arc::new(move || {
                    if let Some(shared) = shared.upgrade() {
                        shared.queue.notify_network();
                    }
                }));
                network_loop(
                    self.channel,
                    self.coordinator,
                    &NetworkLoopContext {
                        policy: self.policy,
                        store: &self.store,
                        peer_fingerprint: self.peer_fingerprint,
                        shared: &self.shared,
                        injection: &self.injection,
                        source_cursor_shield: self.source_cursor_shield,
                        session_control: self.control.as_deref(),
                    },
                )
            })
    }
}

fn pump_channel_events(
    channel: &CrossScreenChannel,
    coordinator: &mut ControlCoordinator,
    context: &NetworkLoopContext<'_>,
    pending_enter: &mut Option<PendingEnter>,
) -> io::Result<()> {
    while let Some(event) = channel.try_receive_event()? {
        match event {
            ChannelEvent::Control(event) => {
                if let ChannelControlEvent::BoundaryWorkspace(payload) = &event {
                    let Some(control) = context.session_control else {
                        continue;
                    };
                    let local = context.store.load_identity()?.fingerprint();
                    control
                        .boundary_sync
                        .receive(payload, context.peer_fingerprint, local)?;
                    continue;
                }
                if let ChannelControlEvent::FileDrag(message) = &event {
                    if let Some(drag) = &context.shared.drag {
                        let state = coordinator.state();
                        drag.receive_for_route(
                            *message,
                            matches!(
                                state,
                                ControlProtocolState::Local | ControlProtocolState::ControllingPeer
                            ),
                            state == ControlProtocolState::ControllingPeer,
                        );
                    }
                    continue;
                }
                if context.shared.logical_boundary_layout.is_some() {
                    match &event {
                        ChannelControlEvent::EnterRequest(request)
                            if !context.shared.logical_request_is_valid(request) =>
                        {
                            channel.send_enter_acknowledgement(ControlTransitionAck {
                                control_epoch: request.control_epoch,
                                accepted: false,
                            })?;
                            continue;
                        }
                        ChannelControlEvent::ReturnRequest(request)
                            if !context.shared.logical_request_is_valid(request) =>
                        {
                            channel.send_return_acknowledgement(ControlTransitionAck {
                                control_epoch: request.control_epoch,
                                accepted: false,
                            })?;
                            continue;
                        }
                        _ => {}
                    }
                }
                if transition_acknowledgement_is_stale(
                    &event,
                    coordinator.state(),
                    coordinator.active_control_epoch(),
                ) {
                    if let Ok(mut stats) = context.shared.stats.lock() {
                        stats.ignored_stale_acknowledgements =
                            stats.ignored_stale_acknowledgements.saturating_add(1);
                    }
                    continue;
                }
                handle_control_event(&event, channel, coordinator, context, pending_enter)?;
            }
            ChannelEvent::Input(input) => {
                handle_input_event(input, coordinator, context.shared, context.injection)?;
            }
        }
    }
    Ok(())
}

fn transition_acknowledgement_is_stale(
    event: &ChannelControlEvent,
    state: ControlProtocolState,
    active_epoch: Option<u64>,
) -> bool {
    match event {
        ChannelControlEvent::EnterAcknowledgement(acknowledgement) => {
            state != ControlProtocolState::AwaitingEnterAcknowledgement
                || active_epoch != Some(acknowledgement.control_epoch)
        }
        ChannelControlEvent::ReturnAcknowledgement(acknowledgement) => {
            state != ControlProtocolState::AwaitingReturnAcknowledgement
                || active_epoch != Some(acknowledgement.control_epoch)
        }
        _ => false,
    }
}

fn handle_control_event(
    event: &ChannelControlEvent,
    channel: &CrossScreenChannel,
    coordinator: &mut ControlCoordinator,
    context: &NetworkLoopContext<'_>,
    pending_enter: &mut Option<PendingEnter>,
) -> io::Result<()> {
    let previous = coordinator.state();
    let request = match event {
        ChannelControlEvent::EnterRequest(request)
        | ChannelControlEvent::ReturnRequest(request) => Some(request),
        _ => None,
    };
    let release_all = matches!(event, ChannelControlEvent::ReleaseAll);
    let actions = coordinator.handle_channel_event(event.clone())?;
    let next = coordinator.state();
    if previous == ControlProtocolState::Local && next == ControlProtocolState::ControlledByPeer {
        let request = request.expect("accepted enter came from an enter request");
        begin_target_control(request, coordinator, context)?;
    } else if previous == ControlProtocolState::AwaitingEnterAcknowledgement
        && next == ControlProtocolState::ControllingPeer
    {
        let pending = pending_enter.take().ok_or_else(|| {
            invalid_data("accepted enter acknowledgement had no pending pointer mapping")
        })?;
        let acknowledgement_arrival = pending.requested_at.elapsed();
        let local_releases = context.shared.begin_entering_peer(
            coordinator
                .active_control_epoch()
                .ok_or_else(|| invalid_data("controlling-peer state has no control epoch"))?,
            coordinator.peer_screen(),
            pending.peer_position,
            pending.local_resume,
        )?;
        if let Some(drag) = &context.shared.drag {
            drag.suspend_source_return_presentation();
            drag.suspend_reverse_target_for_source_reentry();
        }
        let source_prepare_started = Instant::now();
        context.injection.prepare_source(local_releases, true)?;
        let source_prepare = source_prepare_started.elapsed();
        context.shared.activate_controlling_peer()?;
        if let Some(drag) = &context.shared.drag {
            drag.activate_source();
        }
        if let Some(control) = context.session_control {
            control.record_transition_trace(format!(
                "kind=enter_ack,ack_arrival_us={},source_prepare_us={},{}",
                elapsed_micros(acknowledgement_arrival),
                elapsed_micros(source_prepare),
                context.shared.enter_carry_trace()
            ));
        }
        let enter_latency = pending.requested_at.elapsed();
        context.shared.record_enter_latency(enter_latency);
        if let Some(control) = context.session_control {
            control.record_enter_latency(enter_latency);
        }
    } else if previous == ControlProtocolState::AwaitingEnterAcknowledgement
        && next == ControlProtocolState::Local
    {
        pending_enter.take();
        if let Some(drag) = &context.shared.drag {
            drag.cancel_with_reason("enter_request_rejected");
        }
        context
            .shared
            .local_source_drag_return_eligible
            .store(false, Ordering::Release);
        context.shared.cancel_pending_enter_capture();
        let _ = context.source_cursor_shield.set_active(false);
    } else if matches!(
        previous,
        ControlProtocolState::ControllingPeer | ControlProtocolState::AwaitingReturnAcknowledgement
    ) && next == ControlProtocolState::Local
    {
        finish_source_control_return(previous, event, context)?;
    } else if previous == ControlProtocolState::ControlledByPeer
        && next == ControlProtocolState::Local
    {
        finish_target_control_return(event, context)?;
    } else if release_all {
        release_peer_input(context)?;
    }
    dispatch_control_actions(channel, actions, || {
        context.injection.request_release_all();
        Ok(())
    })?;
    Ok(())
}

fn finish_target_control_return(
    event: &ChannelControlEvent,
    context: &NetworkLoopContext<'_>,
) -> io::Result<()> {
    let returning = matches!(event, ChannelControlEvent::ReturnRequest(_));
    if returning {
        context.shared.record_control_return();
        if let Some(control) = context.session_control {
            control.record_control_return("peer_boundary_return");
        }
    }
    let reverse_source = context.shared.drag.as_ref().is_some_and(|drag| {
        returning && drag.source_remote_controlled.load(Ordering::Acquire) && drag.source_dragging()
    });
    if reverse_source && let Some(drag) = &context.shared.drag {
        drag.suspend_source_return_presentation();
    }
    let returning_target = context
        .shared
        .drag
        .as_ref()
        .is_some_and(|drag| returning && drag.suspend_target_for_source_return());
    let preserve_drag = reverse_source || returning_target;
    context.shared.force_local_with_drag(preserve_drag);
    if let Some(drag) = &context.shared.drag {
        drag.classify_idle_source(false);
    }
    context
        .shared
        .preserve_next_drag_release
        .store(preserve_drag, Ordering::Release);
    context.injection.request_release_all();
    if preserve_drag {
        context.source_cursor_shield.set_active(false)?;
        context.injection.set_visible(true)?;
    }
    Ok(())
}

fn release_peer_input(context: &NetworkLoopContext<'_>) -> io::Result<()> {
    if let Some(control) = context.session_control {
        control.record_control_return("peer_release_all");
    }
    // The return acknowledgement sends one ReleaseAll for input, not for the
    // independent reverse file offer that was just handed to the receiver.
    let preserve = context
        .shared
        .preserve_next_drag_release
        .swap(false, Ordering::AcqRel)
        && context.shared.drag.as_ref().is_some_and(|drag| {
            (drag.source_remote_controlled.load(Ordering::Acquire) && drag.source_dragging())
                || drag.returned_target_pending()
        });
    let resume = context.shared.force_local_with_drag(preserve);
    context.injection.request_release_all();
    context.injection.set_visible(true)?;
    if let Some(resume) = resume {
        context.injection.position(resume)?;
    }
    Ok(())
}

fn begin_target_control(
    request: &super::channel::ControlTransitionRequest,
    coordinator: &ControlCoordinator,
    context: &NetworkLoopContext<'_>,
) -> io::Result<()> {
    let arrival = if context.shared.logical_boundary_layout.is_some() {
        context
            .shared
            .logical_inbound_arrival(request)
            .ok_or_else(|| {
                invalid_data("logical boundary enter request does not match the applied layout")
            })?
    } else {
        map_normalized_boundary_position(
            request.normalized_edge_offset,
            request.departure_boundary.opposite(),
            coordinator.local_screen(),
            context.policy.corner_guard_pixels,
        )
    };
    let target_prepare_started = Instant::now();
    context.shared.set_controlled_by_peer();
    if let Some(drag) = &context.shared.drag {
        // Publish the ownership before the injected arrival can start an Explorer drag.
        drag.classify_idle_source(true);
    }
    context.injection.request_release_all();
    let drag_target_pending = context
        .shared
        .drag
        .as_ref()
        .is_some_and(|drag| drag.target_pending());
    let remote_source_reentry = context.shared.drag.as_ref().is_some_and(|drag| {
        drag.source_remote_controlled.load(Ordering::Acquire) && drag.source_dragging()
    });
    let cursor_mode = if drag_target_pending || remote_source_reentry {
        // A native target drag performs its first Shell hit-test as soon as DoDragDrop starts.
        // Complete the arrival positioning before acknowledging this one transition, otherwise a
        // busy injector can briefly highlight the desktop icon under the stale local cursor.
        context.injection.set_visible(true)?;
        context.injection.position(arrival)?;
        "sync_drag"
    } else {
        // Normal pointer handoff keeps the low-latency asynchronous path. The injector applies
        // visibility before arrival and before any subsequently received remote input.
        context.injection.set_visible_async(true)?;
        context.injection.position_async(arrival)?;
        "async_pointer"
    };
    let remote_source_represented = remote_source_reentry
        && context
            .shared
            .drag
            .as_ref()
            .is_some_and(|drag| drag.present_remote_source_after_peer_reentry());
    if let Some(drag) = &context.shared.drag {
        drag.target_control_arrived();
    }
    if let Some(control) = context.session_control {
        let (drag_target_id, remote_left_down) =
            context.shared.drag.as_ref().map_or((0, false), |drag| {
                (
                    drag.target_debug_id(),
                    context.shared.remote_left_down.load(Ordering::Acquire),
                )
            });
        control.record_transition_trace(format!(
            "kind=peer_enter,from={},arrival={},{},target_queue_us={},cursor_mode={},drag_target_pending={},drag_target_id={},remote_left_down={},remote_source_reentry={},remote_source_represented={}",
            request.departure_boundary.label(),
            arrival.x,
            arrival.y,
            elapsed_micros(target_prepare_started.elapsed()),
            cursor_mode,
            drag_target_pending,
            drag_target_id,
            remote_left_down,
            remote_source_reentry,
            remote_source_represented,
        ));
    }
    Ok(())
}

fn finish_source_control_return(
    previous: ControlProtocolState,
    event: &ChannelControlEvent,
    context: &NetworkLoopContext<'_>,
) -> io::Result<()> {
    let completed_return = previous == ControlProtocolState::AwaitingReturnAcknowledgement;
    let reverse_target = completed_return
        && context
            .shared
            .drag
            .as_ref()
            .is_some_and(|drag| drag.reverse_handoff_ready());
    let return_eligible = context
        .shared
        .local_source_drag_return_eligible
        .swap(false, Ordering::AcqRel);
    let returning_local_source_drag = should_represent_returning_local_drag(
        completed_return && !reverse_target,
        context.shared.local_button_mask.load(Ordering::Acquire)
            & mouse_button_bit(MouseButton::Left)
            != 0,
        return_eligible,
    );
    let failed_reverse_target = completed_return
        && context
            .shared
            .drag
            .as_ref()
            .is_some_and(|drag| drag.take_failed_reverse_left_restore());
    if should_suppress_drag_left_restore(
        reverse_target,
        returning_local_source_drag,
        failed_reverse_target,
    ) {
        context
            .shared
            .suppress_drag_left_restore
            .store(true, Ordering::Release);
    }
    let restore_inputs = if completed_return {
        context.shared.take_local_restore_inputs()?
    } else {
        Vec::new()
    };
    if completed_return {
        context.shared.record_control_return();
        if let Some(control) = context.session_control {
            control.record_control_return("boundary_return_ack");
        }
    } else if matches!(event, ChannelControlEvent::ReleaseAll)
        && let Some(control) = context.session_control
    {
        control.record_control_return("peer_release_all");
    }
    // There are two valid drag sessions that must survive a control return:
    //
    // * a reverse target that is about to become active on this computer; and
    // * a local Explorer data object returning from the peer while the physical
    //   left button is still held. Its original DoDragDrop loop ended during the
    //   first handoff, so the dedicated OLE STA presents that retained object
    //   again instead of fabricating a second local button press.
    let preserve_drag =
        should_preserve_drag_on_control_return(reverse_target, returning_local_source_drag);
    let resume = context.shared.force_local_with_drag(preserve_drag);
    context.injection.request_release_all();
    if let Some(resume) = resume {
        if completed_return {
            context.injection.restore_source(resume, restore_inputs)?;
        } else {
            context.injection.set_visible(true)?;
            context.injection.position(resume)?;
        }
    } else {
        context.injection.set_visible(true)?;
    }
    if completed_return {
        context.shared.record_return_latency();
    }
    if reverse_target && let Some(drag) = &context.shared.drag {
        // A physical Up can arrive while the return acknowledgement is in flight.
        // The target was still PREPARING then, so the hook could not release it.
        // Consume that release now, after cursor positioning and activation.
        drag.complete_local_target_handoff(
            context.shared.local_button_mask.load(Ordering::Acquire)
                & mouse_button_bit(MouseButton::Left)
                != 0,
        );
    }
    if returning_local_source_drag {
        if let Some(drag) = &context.shared.drag
            && !drag.present_source_after_return()
        {
            drag.cancel_with_reason("source_return_presentation_unavailable");
        }
        if let Some(control) = context.session_control {
            control.record_transition_trace(
                "kind=source_drag_return_local,left_restore=false,native_drag_represent=true"
                    .to_owned(),
            );
        }
    }
    if failed_reverse_target && let Some(control) = context.session_control {
        control.record_transition_trace(
            "kind=failed_reverse_drag_return,left_restore=false,native_drag_represent=false"
                .to_owned(),
        );
    }
    Ok(())
}

const fn should_represent_returning_local_drag(
    returning_source_side: bool,
    physical_left_down: bool,
    return_eligible: bool,
) -> bool {
    returning_source_side && physical_left_down && return_eligible
}

const fn should_preserve_drag_on_control_return(
    reverse_target: bool,
    returning_local_source_drag: bool,
) -> bool {
    reverse_target || returning_local_source_drag
}

const fn should_suppress_drag_left_restore(
    reverse_target: bool,
    returning_local_source_drag: bool,
    failed_reverse_target: bool,
) -> bool {
    reverse_target || returning_local_source_drag || failed_reverse_target
}

fn handle_input_event(
    input: ChannelInputEvent,
    coordinator: &ControlCoordinator,
    shared: &LiveContext,
    injection: &InjectionQueue,
) -> io::Result<()> {
    if !coordinator.authorizes_remote_input(input.control_epoch) {
        if let Ok(mut stats) = shared.stats.lock() {
            stats.rejected_input_epochs += 1;
        }
        return Ok(());
    }
    let event = local_input_event(input.event, coordinator.local_screen());
    if let InputEvent::MouseButton {
        button: MouseButton::Left,
        transition,
    } = event
    {
        shared
            .remote_left_down
            .store(transition == KeyTransition::Down, Ordering::Release);
        if let Some(drag) = &shared.drag {
            drag.observe_remote_source_left_event(transition == KeyTransition::Down);
        }
    }
    if let Some(drag) = &shared.drag
        && (drag.target_pending() || drag.returned_source_pending())
        && matches!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                ..
            }
        )
    {
        return Ok(());
    }
    let is_mouse_move = event.is_mouse_move();
    if is_mouse_move {
        shared.record_target_first_remote_move_received();
    }
    injection.push_input(event)
}

fn begin_source_enter_request(
    request: &super::channel::ControlTransitionRequest,
    sample: PointerSample,
    coordinator: &ControlCoordinator,
    context: &NetworkLoopContext<'_>,
    pending_enter: &mut Option<PendingEnter>,
) -> io::Result<()> {
    // Arm the full-screen cursor shield while the acknowledgement is in flight. It does not
    // suppress the input hook, but prevents the hidden source cursor from hovering or clicking
    // local UI throughout the acknowledged handoff.
    if let Some(drag) = &context.shared.drag
        && drag.source_can_handoff()
    {
        drag.source_active.store(true, Ordering::Release);
        context
            .shared
            .local_source_drag_return_eligible
            .store(true, Ordering::Release);
        context
            .shared
            .suppress_drag_left_restore
            .store(true, Ordering::Release);
    }
    context.source_cursor_shield.set_active(true)?;
    context
        .shared
        .begin_pending_enter_capture(request.departure_boundary);
    let peer_position = if context.shared.logical_boundary_layout.is_some() {
        context
            .shared
            .logical_destination_for_request(request, true)
            .ok_or_else(|| {
                invalid_data("logical boundary enter request does not match the applied layout")
            })?
    } else {
        map_normalized_boundary_position(
            request.normalized_edge_offset,
            request.departure_boundary.opposite(),
            coordinator.peer_screen(),
            context.policy.corner_guard_pixels,
        )
    };
    let requested_at = Instant::now();
    *pending_enter = Some(PendingEnter {
        peer_position,
        local_resume: sample.position,
        requested_at,
    });
    if let Some(control) = context.session_control {
        let (drag_source_id, drag_ready, drag_active, drag_remote_controlled) = context
            .shared
            .drag
            .as_ref()
            .map_or((0, false, false, false), |drag| {
                (
                    drag.source_id.load(Ordering::Acquire),
                    drag.source_ready.load(Ordering::Acquire),
                    drag.source_active.load(Ordering::Acquire),
                    drag.source_remote_controlled.load(Ordering::Acquire),
                )
            });
        control.record_transition_trace_at(
            format!(
                "kind=enter_request,from={},source={},{},destination={},{},delta={},{},{},gain_q10={},local_button_mask={:#x},drag_source_id={},drag_ready={},drag_active={},drag_remote_controlled={}",
                request.departure_boundary.label(),
                sample.position.x,
                sample.position.y,
                peer_position.x,
                peer_position.y,
                sample.delta_x,
                sample.delta_y,
                context.shared.boundary_request_trace(requested_at),
                context.shared.pointer_gain_trace(),
                context.shared.local_button_mask.load(Ordering::Acquire),
                drag_source_id,
                drag_ready,
                drag_active,
                drag_remote_controlled,
            ),
            requested_at,
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn handle_live_signal(
    signal: LiveSignal,
    channel: &CrossScreenChannel,
    coordinator: &mut ControlCoordinator,
    context: &NetworkLoopContext<'_>,
    pending_enter: &mut Option<PendingEnter>,
    remote_shortcuts: &mut RemoteShortcutState,
) -> io::Result<()> {
    match signal {
        LiveSignal::Pointer {
            sample,
            remote_position,
        } => {
            let previous = coordinator.state();
            let logical_transfer = context.shared.logical_transfer_for_sample(previous, sample);
            let local_departure = (previous == ControlProtocolState::Local)
                .then(|| {
                    logical_transfer.as_ref().map_or_else(
                        || context.shared.local_departure_for_sample(sample),
                        |transfer| Some(transfer.departure_boundary),
                    )
                })
                .flatten();
            let peer_departure = (previous == ControlProtocolState::ControllingPeer)
                .then(|| {
                    logical_transfer
                        .as_ref()
                        .map(|transfer| transfer.departure_boundary)
                })
                .flatten();
            // A final local pointer sample can race with accepting an inbound EnterRequest. Once
            // the coordinator has granted the peer control, that stale sample must not be treated
            // as a contradictory boundary request; the next genuine local event is represented by
            // LocalTakeover instead.
            if previous == ControlProtocolState::ControlledByPeer {
                return Ok(());
            }
            if previous == ControlProtocolState::Local {
                let mut preview = coordinator.clone();
                if matches!(
                    preview.observe_pointer_at_boundaries(
                        sample,
                        local_departure,
                        peer_departure,
                    )?,
                    Some(ControlProtocolAction::SendEnterRequest(_))
                ) {
                    if let Some(drag) = &context.shared.drag {
                        let left = context.shared.local_button_mask.load(Ordering::Acquire)
                            & mouse_button_bit(MouseButton::Left)
                            != 0;
                        if !drag.may_cross(left) {
                            *context
                                .shared
                                .drag_retry
                                .lock()
                                .map_err(|_| io::Error::other("drag retry lock"))? = Some(signal);
                            return Ok(());
                        }
                    }
                    let sequence = unsafe { GetClipboardSequenceNumber() };
                    if !context
                        .shared
                        .drag
                        .as_ref()
                        .is_some_and(|drag| drag.source_ready.load(Ordering::Acquire))
                        && crate::clipboard::paste_fence::wait_for_install(
                            sequence,
                            PASTE_FENCE_TIMEOUT,
                        ) == crate::clipboard::paste_fence::PasteFenceDecision::Block
                    {
                        // Leave the real boundary state untouched. A later pointer sample retries
                        // after the receiver has installed the pending IDataObject, so mouse and
                        // context-menu paste can never reach the peer with its old clipboard live.
                        if let Some(control) = context.session_control {
                            control.record_transition_trace(format!(
                                "kind=enter_blocked,gate=paste_fence,local_button_mask={:#x},{}",
                                context.shared.local_button_mask.load(Ordering::Acquire),
                                crate::clipboard::paste_fence::diagnostic_state(sequence),
                            ));
                        }
                        return Ok(());
                    }
                }
            }
            if previous == ControlProtocolState::ControllingPeer
                && let Some(drag) = &context.shared.drag
            {
                let mut preview = coordinator.clone();
                if matches!(
                    preview.observe_pointer_at_boundaries(
                        sample,
                        local_departure,
                        peer_departure,
                    )?,
                    Some(ControlProtocolAction::SendReturnRequest(_))
                ) {
                    let left = context.shared.local_button_mask.load(Ordering::Acquire)
                        & mouse_button_bit(MouseButton::Left)
                        != 0;
                    if !drag.may_return(left) {
                        if let Some(position) = remote_position {
                            send_remote_input(
                                channel,
                                coordinator,
                                RemoteInputEvent::MouseMove(position),
                            )?;
                        }
                        *context
                            .shared
                            .drag_retry
                            .lock()
                            .map_err(|_| io::Error::other("drag retry lock"))? = Some(signal);
                        return Ok(());
                    }
                } else {
                    drag.reset_return_wait();
                }
            }
            if let Some(mut action) = coordinator.observe_pointer_at_boundaries(
                sample,
                local_departure,
                peer_departure,
            )? {
                if let Some(transfer) = &logical_transfer {
                    match &mut action {
                        ControlProtocolAction::SendEnterRequest(request)
                        | ControlProtocolAction::SendReturnRequest(request) => {
                            request.logical_boundary = Some(transfer.transition.clone());
                        }
                        _ => {}
                    }
                }
                if let ControlProtocolAction::SendEnterRequest(ref request) = action {
                    begin_source_enter_request(
                        request,
                        sample,
                        coordinator,
                        context,
                        pending_enter,
                    )?;
                } else if let ControlProtocolAction::SendReturnRequest(ref request) = action {
                    context.shared.begin_return_latency()?;
                    let local_resume = if context.shared.logical_boundary_layout.is_some() {
                        context
                            .shared
                            .logical_destination_for_request(request, false)
                            .ok_or_else(|| {
                                invalid_data(
                                    "logical boundary return request does not match the applied layout",
                                )
                            })?
                    } else {
                        map_normalized_boundary_position(
                            request.normalized_edge_offset,
                            request.departure_boundary.opposite(),
                            coordinator.local_screen(),
                            context.policy.corner_guard_pixels,
                        )
                    };
                    context.shared.set_local_resume(local_resume)?;
                    if let Some(control) = context.session_control {
                        control.record_transition_trace(format!(
                            "kind=return_request,from={},source={},{},destination={},{},delta={},{},{}",
                            request.departure_boundary.label(),
                            sample.position.x,
                            sample.position.y,
                            local_resume.x,
                            local_resume.y,
                            sample.delta_x,
                            sample.delta_y,
                            context.shared.enter_carry_trace()
                        ));
                    }
                }
                dispatch_control_actions(channel, [action], || Ok(()))?;
            } else if previous == ControlProtocolState::ControllingPeer
                && coordinator.state() == ControlProtocolState::ControllingPeer
                && let Some(position) = remote_position
            {
                send_remote_input(channel, coordinator, RemoteInputEvent::MouseMove(position))?;
                context.shared.record_source_first_remote_move_sent(sample);
            }
        }
        LiveSignal::Input(event) => {
            if let Some(drag) = &context.shared.drag
                && drag.source_dragging()
            {
                match event {
                    RemoteInputEvent::MouseButton {
                        button: MouseButton::Left,
                        transition: KeyTransition::Down,
                    } => {
                        return Ok(());
                    }
                    RemoteInputEvent::MouseButton {
                        button: MouseButton::Left,
                        transition: KeyTransition::Up,
                    } => {
                        drag.release_source();
                        return Ok(());
                    }
                    RemoteInputEvent::Key {
                        virtual_key: 0x1b,
                        transition: KeyTransition::Down,
                        ..
                    } => {
                        drag.cancel_with_reason("remote_escape");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            let should_send =
                remote_shortcuts.should_send(event, coordinator.active_control_epoch(), || {
                    let sequence = unsafe { GetClipboardSequenceNumber() };
                    crate::clipboard::paste_fence::wait_for_sequence(sequence, PASTE_FENCE_TIMEOUT)
                });
            if should_send {
                send_remote_input(channel, coordinator, event)?;
            }
        }
        LiveSignal::LocalTakeover => {
            context
                .shared
                .local_takeover_pending
                .store(false, Ordering::Release);
            if coordinator.state() == ControlProtocolState::ControlledByPeer {
                let actions = coordinator.disconnect();
                context.shared.force_local();
                context.shared.record_control_return();
                if let Some(control) = context.session_control {
                    control.record_control_return("local_physical_takeover");
                }
                dispatch_control_actions(channel, actions, || {
                    context.injection.request_release_all();
                    context.injection.set_visible(true)
                })?;
            }
        }
    }
    context.shared.active_epoch.store(
        coordinator.active_control_epoch().unwrap_or(0),
        Ordering::Release,
    );
    Ok(())
}

fn send_remote_input(
    channel: &CrossScreenChannel,
    coordinator: &ControlCoordinator,
    event: RemoteInputEvent,
) -> io::Result<()> {
    if coordinator.state() != ControlProtocolState::ControllingPeer {
        return Ok(());
    }
    let control_epoch = coordinator
        .active_control_epoch()
        .ok_or_else(|| invalid_data("remote input has no active control epoch"))?;
    channel.send_input_event(ChannelInputEvent {
        control_epoch,
        event,
    })
}

fn local_input_event(event: RemoteInputEvent, screen: PhysicalScreenSize) -> InputEvent {
    match event {
        RemoteInputEvent::Key {
            physical,
            virtual_key,
            transition,
        } => InputEvent::Key {
            physical,
            virtual_key,
            transition,
        },
        RemoteInputEvent::MouseMove(position) => InputEvent::MouseMove {
            x: denormalize_physical_coordinate(
                position.x,
                i32::try_from(screen.width).expect("screen width fits i32"),
            ),
            y: denormalize_physical_coordinate(
                position.y,
                i32::try_from(screen.height).expect("screen height fits i32"),
            ),
        },
        RemoteInputEvent::MouseButton { button, transition } => {
            InputEvent::MouseButton { button, transition }
        }
        RemoteInputEvent::Wheel { axis, delta } => InputEvent::Wheel { axis, delta },
    }
}

fn establish_channel(
    store: &DeviceStore,
    config: AutomaticChannelConfig,
    timeout: Duration,
    control: Option<&LiveSessionControl>,
) -> io::Result<CrossScreenChannel> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid_data("live control connection timeout overflow"))?;
    let mut last_error = None;
    loop {
        if control.is_some_and(LiveSessionControl::is_cancelled) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cross-screen connection was cancelled",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                last_error.map_or_else(
                    || "timed out waiting for the paired device".to_owned(),
                    |error: io::Error| format!("timed out waiting for the paired device: {error}"),
                ),
            ));
        }
        let attempt_timeout = remaining.min(CONNECTION_ATTEMPT_TIMEOUT);
        let result = match config.role {
            AutomaticChannelRole::Listener => CrossScreenChannel::listen_once(
                store,
                config.peer_fingerprint,
                config.attempt_endpoint(),
                attempt_timeout,
            ),
            AutomaticChannelRole::Connector => CrossScreenChannel::connect(
                store,
                config.peer_fingerprint,
                config.attempt_endpoint(),
                attempt_timeout,
            ),
        };
        match result {
            Ok(channel) => return Ok(channel),
            Err(error) => last_error = Some(error),
        }
        for _ in 0..10 {
            if control.is_some_and(LiveSessionControl::is_cancelled) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "cross-screen connection was cancelled",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn exchange_session_configuration(
    channel: &CrossScreenChannel,
    local: PhysicalScreenSize,
    local_millimeters: Option<ScreenMillimeterSize>,
    local_layout: LayoutConfiguration,
    role: AutomaticChannelRole,
    timeout: Duration,
    control: Option<&LiveSessionControl>,
) -> io::Result<(
    PhysicalScreenSize,
    Option<ScreenMillimeterSize>,
    LayoutConfiguration,
)> {
    channel.send_screen_info(local)?;
    channel.send_screen_millimeters(local_millimeters)?;
    channel.send_layout_info(local_layout)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid_data("screen handshake timeout overflow"))?;
    let mut peer_screen = None;
    let mut peer_millimeters = None;
    let mut peer_millimeters_received = false;
    let mut peer_layout = None;
    loop {
        if control.is_some_and(LiveSessionControl::is_cancelled) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cross-screen session configuration was cancelled",
            ));
        }
        while let Some(event) = channel.try_receive_control_event()? {
            match event {
                ChannelControlEvent::ScreenInfo(peer) if peer_screen.is_none() => {
                    peer_screen = Some(peer);
                }
                ChannelControlEvent::ScreenMillimeters(size) if !peer_millimeters_received => {
                    peer_millimeters = size;
                    peer_millimeters_received = true;
                }
                ChannelControlEvent::LayoutInfo(layout) if peer_layout.is_none() => {
                    peer_layout = Some(layout);
                }
                _ => {
                    return Err(invalid_data(
                        "cross-screen control event arrived before session configuration",
                    ));
                }
            }
        }
        if let (Some(peer_screen), true, Some(peer_layout)) =
            (peer_screen, peer_millimeters_received, peer_layout)
        {
            let layout = resolve_layout_configuration(local_layout, peer_layout, role);
            return Ok((peer_screen, peer_millimeters, layout));
        }
        let snapshot = channel.snapshot();
        if snapshot.state != ChannelState::Connected {
            return Err(io::Error::other(snapshot.last_error.unwrap_or_else(|| {
                "channel stopped during screen handshake".to_owned()
            })));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for peer screen metadata and layout information",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

const fn resolve_layout_configuration(
    local: LayoutConfiguration,
    peer: LayoutConfiguration,
    role: AutomaticChannelRole,
) -> LayoutConfiguration {
    if peer.revision > local.revision
        || (peer.revision == local.revision && matches!(role, AutomaticChannelRole::Connector))
    {
        peer
    } else {
        local
    }
}

fn live_policy(
    role: AutomaticChannelRole,
    layout: LayoutConfiguration,
    local_corner_guard_enabled: bool,
    local_corner_guard_pixels: u32,
) -> BoundaryPolicy {
    BoundaryPolicy {
        peer_placement: match role {
            AutomaticChannelRole::Listener => layout.listener_peer_placement,
            AutomaticChannelRole::Connector => layout.listener_peer_placement.opposite(),
        },
        corner_guard_pixels: if local_corner_guard_enabled {
            local_corner_guard_pixels
        } else {
            0
        },
        require_second_push: layout.require_second_push,
    }
}

pub(crate) fn foreground_covers_primary_screen() -> io::Result<bool> {
    let _dpi_guard = ThreadDpiAwarenessGuard::enter_per_monitor()?;
    foreground_covers_screen(ScreenExtent::current_primary_display_mode()?)
}

fn foreground_covers_screen(screen: ScreenExtent) -> io::Result<bool> {
    const TOLERANCE: i32 = 2;
    // SAFETY: this reads the current foreground window handle without taking ownership.
    let window = unsafe { GetForegroundWindow() };
    if window == HWND::default() {
        return Ok(false);
    }
    // Our fullscreen capture overlay must keep receiving remote input even
    // when the user's automatic fullscreen pause preference is enabled.
    let mut screenshot_class = [0_u16; 128];
    let length = unsafe { GetClassNameW(window, &mut screenshot_class) };
    if usize::try_from(length).ok().is_some_and(|length| {
        screenshot_class[..length]
            .iter()
            .copied()
            .eq("PierdeskScreenshotWindow".encode_utf16())
    }) {
        return Ok(false);
    }
    if foreground_is_shell_desktop(window)? {
        return Ok(false);
    }
    // SAFETY: IsIconic is a read-only query for the non-null foreground window.
    if unsafe { IsIconic(window) }.as_bool() {
        return Ok(false);
    }
    let mut client = RECT::default();
    // SAFETY: client owns writable storage and the foreground handle remains system-owned.
    unsafe { GetClientRect(window, &raw mut client) }.map_err(io::Error::other)?;
    let mut top_left = POINT {
        x: client.left,
        y: client.top,
    };
    let mut bottom_right = POINT {
        x: client.right,
        y: client.bottom,
    };
    // SAFETY: both points own writable storage and the window handle is system-owned.
    if !unsafe { ClientToScreen(window, &raw mut top_left) }.as_bool() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same invariants as the preceding ClientToScreen call.
    if !unsafe { ClientToScreen(window, &raw mut bottom_right) }.as_bool() {
        return Err(io::Error::last_os_error());
    }
    Ok(top_left.x <= TOLERANCE
        && top_left.y <= TOLERANCE
        && bottom_right.x >= screen.width.saturating_sub(TOLERANCE)
        && bottom_right.y >= screen.height.saturating_sub(TOLERANCE))
}

fn foreground_is_shell_desktop(window: HWND) -> io::Result<bool> {
    // SAFETY: both functions return system-owned handles and transfer no ownership.
    if window == unsafe { GetDesktopWindow() } || window == unsafe { GetShellWindow() } {
        return Ok(true);
    }
    let mut class_name = [0_u16; 256];
    // SAFETY: window is a system-owned foreground handle and class_name is writable storage.
    let length = unsafe { GetClassNameW(window, &mut class_name) };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = usize::try_from(length).map_err(io::Error::other)?;
    Ok(is_shell_desktop_class_name(&class_name[..length]))
}

fn is_shell_desktop_class_name(class_name: &[u16]) -> bool {
    class_name.iter().copied().eq("Progman".encode_utf16())
        || class_name.iter().copied().eq("WorkerW".encode_utf16())
}

struct PreparedLiveSession {
    _dpi_guard: ThreadDpiAwarenessGuard,
    screen_extent: ScreenExtent,
    config: AutomaticChannelConfig,
    channel: CrossScreenChannel,
    local_screen: PhysicalScreenSize,
    peer_screen: PhysicalScreenSize,
    policy: BoundaryPolicy,
    boundary_map: Option<PreparedBoundaryMap>,
    logical_boundary_layout: Option<CompiledBoundaryLayout>,
    coordinator: ControlCoordinator,
    effective_dpi: u32,
}

fn load_compiled_logical_boundary_layout(
    store: &DeviceStore,
    local_fingerprint: crate::security::CertificateFingerprint,
    peer_fingerprint: crate::security::CertificateFingerprint,
    local_screen: PhysicalScreenSize,
    peer_screen: PhysicalScreenSize,
) -> io::Result<Option<CompiledBoundaryLayout>> {
    let workspace = match BoundaryWorkspaceV3::load(store) {
        Ok(workspace) => workspace,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(applied) = workspace.applied.as_ref() else {
        return Ok(None);
    };
    let local = local_fingerprint.to_string();
    let peer = peer_fingerprint.to_string();
    if applied.layout.local_fingerprint != local {
        return Err(invalid_data(
            "applied logical boundary layout does not belong to this device",
        ));
    }
    if !applied
        .layout
        .boundaries
        .iter()
        .any(|boundary| boundary.peer_fingerprint == peer)
    {
        return Ok(None);
    }
    let stored_local = applied
        .layout
        .screens
        .iter()
        .find(|screen| screen.device_fingerprint == local)
        .ok_or_else(|| invalid_data("applied logical boundary layout is missing the local screen"))?
        .oriented_pixel_size()?;
    let stored_peer = applied
        .layout
        .screens
        .iter()
        .find(|screen| screen.device_fingerprint == peer)
        .ok_or_else(|| invalid_data("applied logical boundary layout is missing the peer screen"))?
        .oriented_pixel_size()?;
    if stored_local != local_screen || stored_peer != peer_screen {
        return Err(invalid_data(format!(
            "applied logical boundary screen sizes changed: local={}x{} expected={}x{}, peer={}x{} expected={}x{}",
            stored_local.width,
            stored_local.height,
            local_screen.width,
            local_screen.height,
            stored_peer.width,
            stored_peer.height,
            peer_screen.width,
            peer_screen.height,
        )));
    }
    workspace.compile_applied()
}

fn load_prepared_boundary_map(
    store: &DeviceStore,
    peer: crate::security::CertificateFingerprint,
    local_screen: PhysicalScreenSize,
    legacy_departure: ScreenBoundary,
    settings: &CrossScreenSettings,
) -> io::Result<Option<PreparedBoundaryMap>> {
    if !settings.boundary_map_v2_enabled {
        return Ok(None);
    }
    let constraints = BoundaryMapConstraints {
        screen: local_screen,
        corner_guard_enabled: settings.corner_guard_enabled,
        corner_guard_pixels: settings.corner_guard_pixels,
        minimum_segment_pixels: DEFAULT_MINIMUM_SEGMENT_PIXELS,
    };
    let map = match BoundaryMapV2::load(store) {
        Ok(map) => map,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let legacy = BoundaryMapV2::legacy_full_edge(peer, legacy_departure, constraints)?;
            match BoundaryMapV2::replace(store, legacy, constraints) {
                Ok(saved) => saved,
                // A settings window may have won the first-write race. Its complete map is the
                // authority; never overwrite it with the migration default.
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    BoundaryMapV2::load(store)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    map.prepare_for_peer(peer, constraints).map(Some)
}

fn current_local_screen_metadata(
    screen_extent: ScreenExtent,
) -> io::Result<(PhysicalScreenSize, Option<ScreenMillimeterSize>)> {
    let pixels = PhysicalScreenSize::new(
        u32::try_from(screen_extent.width).map_err(|_| invalid_data("screen width is negative"))?,
        u32::try_from(screen_extent.height)
            .map_err(|_| invalid_data("screen height is negative"))?,
    )?;
    let millimeters = current_primary_screen_millimeters()
        .ok()
        .and_then(|(width, height)| ScreenMillimeterSize::new(width, height).ok());
    Ok((pixels, millimeters))
}

#[allow(clippy::too_many_lines)]
fn prepare_live_session(
    options: &LiveControlSessionOptions,
    control: Option<&LiveSessionControl>,
) -> io::Result<PreparedLiveSession> {
    if options.connect_timeout.is_zero() || options.connect_timeout > MAX_LIFETIME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "live control connection timeout must be between 1 millisecond and 300 seconds",
        ));
    }
    let dpi_guard = ThreadDpiAwarenessGuard::enter_per_monitor()?;
    let screen_extent = ScreenExtent::system_metrics()?;
    let display_extent = ScreenExtent::current_primary_display_mode()?;
    let effective_dpi = ScreenExtent::effective_dpi()?;
    if screen_extent != display_extent {
        return Err(invalid_data(format!(
            "primary physical screen mismatch: per-monitor metrics={}x{}, display mode={}x{}",
            screen_extent.width, screen_extent.height, display_extent.width, display_extent.height
        )));
    }
    let (local_screen, local_millimeters) = current_local_screen_metadata(screen_extent)?;
    let settings = AppSettings::load(&options.store)?;
    let config = AutomaticChannelConfig::from_settings(&options.store, &settings)?;
    if let Some(control) = control {
        control.update(|status| status.role = Some(config.role));
    }
    let mut cross_screen_settings = CrossScreenSettings::load_or_default(&options.store)?;
    let local_layout = LayoutConfiguration {
        listener_peer_placement: cross_screen_settings.listener_peer_placement,
        revision: cross_screen_settings.layout_revision,
        // Kept zero on the wire for compatibility with the existing layout frame. Corner
        // protection is a local outbound-input preference and is no longer synchronized.
        corner_guard_enabled: false,
        corner_guard_pixels: 0,
        require_second_push: cross_screen_settings.require_second_push,
    };
    let mut channel = establish_channel(&options.store, config, options.connect_timeout, control)?;
    let (peer_screen, peer_screen_millimeters, resolved_layout) =
        match exchange_session_configuration(
            &channel,
            local_screen,
            local_millimeters,
            local_layout,
            config.role,
            options.connect_timeout,
            control,
        ) {
            Ok(configuration) => configuration,
            Err(error) => {
                channel.stop();
                return Err(error);
            }
        };
    if resolved_layout != local_layout {
        cross_screen_settings.listener_peer_placement = resolved_layout.listener_peer_placement;
        cross_screen_settings.layout_revision = resolved_layout.revision;
        cross_screen_settings.require_second_push = resolved_layout.require_second_push;
        cross_screen_settings.save(&options.store)?;
    }
    let policy = live_policy(
        config.role,
        resolved_layout,
        cross_screen_settings.corner_guard_enabled,
        cross_screen_settings.corner_guard_pixels,
    );
    let logical_boundary_layout = load_compiled_logical_boundary_layout(
        &options.store,
        config.local_fingerprint,
        config.peer_fingerprint,
        local_screen,
        peer_screen,
    )?;
    let boundary_map = if logical_boundary_layout.is_none() {
        load_prepared_boundary_map(
            &options.store,
            config.peer_fingerprint,
            local_screen,
            policy.peer_placement.local_exit_boundary(),
            &cross_screen_settings,
        )?
    } else {
        None
    };
    // Reconnecting the control channel is transport state, not a user edit. Boundary workspace
    // updates are queued only by explicit layout, boundary, or confirmation actions so an idle
    // peer cannot make another settings page jump by replaying an old snapshot.
    let coordinator = if logical_boundary_layout.is_some() {
        ControlCoordinator::new_logical(policy, local_screen, peer_screen)?
    } else if boundary_map.is_some() {
        ControlCoordinator::new_dynamic(policy, local_screen, peer_screen)?
    } else {
        ControlCoordinator::new(policy, local_screen, peer_screen)?
    };
    if let Some(control) = control {
        let snapshot = channel.snapshot();
        control.update(|status| {
            status.phase = LiveSessionPhase::Connected;
            status.local_screen = Some(local_screen);
            status.peer_screen = Some(peer_screen);
            status.peer_screen_millimeters = peer_screen_millimeters;
            status.peer_placement = Some(policy.peer_placement);
            status.round_trip_micros = snapshot.round_trip_micros;
            status.session_id = Some(snapshot.session_id);
            status.channel_epoch = Some(snapshot.channel_epoch);
        });
    }
    Ok(PreparedLiveSession {
        _dpi_guard: dpi_guard,
        screen_extent,
        config,
        channel,
        local_screen,
        peer_screen,
        policy,
        boundary_map,
        logical_boundary_layout,
        coordinator,
        effective_dpi,
    })
}

fn build_live_report(
    shared: &LiveContext,
    config: AutomaticChannelConfig,
    local_screen: PhysicalScreenSize,
    peer_screen: PhysicalScreenSize,
    network: NetworkOutcome,
    injection: &InjectionOutcome,
) -> io::Result<LiveControlDiagnosticReport> {
    let stop = LiveStopCode::load(shared.stop.load(Ordering::Acquire));
    let failure = shared.failure.lock().map_or_else(
        |poisoned| poisoned.into_inner().clone(),
        |failure| failure.clone(),
    );
    let forwarded = shared
        .forwarded
        .lock()
        .map_err(|_| io::Error::other("forwarded input state was poisoned"))?;
    let remaining_forwarded_inputs = forwarded.outstanding_count();
    let mut forwarded_stats = forwarded.stats();
    forwarded_stats.mouse_moves = shared.forwarded_mouse_moves.load(Ordering::Relaxed);
    forwarded_stats.mouse_button_downs = forwarded_stats
        .mouse_button_downs
        .saturating_add(shared.inherited_button_downs.load(Ordering::Relaxed));
    forwarded_stats.mouse_button_ups = forwarded_stats
        .mouse_button_ups
        .saturating_add(shared.inherited_button_ups.load(Ordering::Relaxed));
    drop(forwarded);
    let stats = shared
        .stats
        .lock()
        .map_err(|_| io::Error::other("live control statistics were poisoned"))?;
    let clean_shutdown = failure.is_none()
        && network.channel.clean_shutdown
        && network.channel.state == ChannelState::Stopped
        && remaining_forwarded_inputs == 0
        && injection.remaining_inputs == 0;
    Ok(LiveControlDiagnosticReport {
        role: config.role,
        local_screen,
        peer_screen,
        stop_reason: stop.reason(),
        clean_shutdown,
        control_entries: stats.control_entries,
        control_returns: stats.control_returns,
        enter_latency_samples: stats.enter_latency.samples,
        enter_latency_average_micros: stats.enter_latency.average_micros(),
        enter_latency_max_micros: stats.enter_latency.max_micros,
        return_latency_samples: stats.return_latency.samples,
        return_latency_average_micros: stats.return_latency.average_micros(),
        return_latency_max_micros: stats.return_latency.max_micros,
        rejected_input_epochs: stats.rejected_input_epochs,
        ignored_stale_acknowledgements: stats.ignored_stale_acknowledgements,
        forwarded: forwarded_stats,
        injected_events: injection.injected_events,
        forced_injected_releases: injection.forced_releases,
        remaining_forwarded_inputs,
        remaining_injected_inputs: injection.remaining_inputs,
        queue_peak_depth: shared.queue.peak_depth(),
        queue_coalesced_moves: shared.queue.coalesced_moves.load(Ordering::Relaxed),
        queue_bypassed_moves: shared.queue.bypassed_moves.load(Ordering::Relaxed),
        channel: network.channel,
        failure,
    })
}

/// Runs one explicit two-machine live-control diagnostic using the existing pairing and trust.
///
/// The command is intentionally not wired into tray startup. It suppresses input only after the
/// strict boundary transfer is acknowledged, reserves Ctrl+Alt+Shift+Esc locally, and releases all
/// injected input on every stop path.
///
/// # Errors
///
/// Returns an error for invalid bounds, unavailable physical display information, missing pairing
/// state, TLS or protocol failure, hook/injection startup failure, or a worker panic.
pub fn run_live_control_diagnostic(
    options: &LiveControlDiagnosticOptions,
) -> io::Result<LiveControlDiagnosticReport> {
    if options.lifetime.is_zero() || options.lifetime > MAX_LIFETIME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "live control lifetime must be between 1 millisecond and 300 seconds",
        ));
    }
    run_live_control_session(&options.into(), None, Some(options.lifetime))
}

pub(super) fn run_live_control_product_session(
    store: &DeviceStore,
    connect_timeout: Duration,
    control: &Arc<LiveSessionControl>,
) -> io::Result<LiveControlDiagnosticReport> {
    run_live_control_session(
        &LiveControlSessionOptions {
            store: store.clone(),
            connect_timeout,
        },
        Some(control),
        None,
    )
}

fn initialize_hook_thread() -> u32 {
    #[cfg(not(test))]
    crate::traffic_control::promote_interactive_thread();
    // SAFETY: this reads the identifier of the current CLI hook thread.
    let hook_thread_id = unsafe { GetCurrentThreadId() };
    let mut bootstrap_message = MSG::default();
    // SAFETY: a no-remove peek creates this thread's message queue without consuming messages.
    let _ = unsafe { PeekMessageW(&raw mut bootstrap_message, None, 0, 0, PM_NOREMOVE) };
    hook_thread_id
}

#[allow(clippy::too_many_lines)]
fn run_live_control_session(
    options: &LiveControlSessionOptions,
    session_control: Option<&Arc<LiveSessionControl>>,
    lifetime: Option<Duration>,
) -> io::Result<LiveControlDiagnosticReport> {
    let PreparedLiveSession {
        _dpi_guard,
        screen_extent,
        config,
        mut channel,
        local_screen,
        peer_screen,
        policy,
        boundary_map,
        logical_boundary_layout,
        coordinator,
        effective_dpi,
    } = prepare_live_session(options, session_control.map(Arc::as_ref))?;

    let hook_thread_id = initialize_hook_thread();
    let snapshot = channel.snapshot();
    let (drag, drag_thread) = crate::clipboard::drag_live::DragBridge::start(
        options.store.clone(),
        config.peer_fingerprint,
        snapshot.local_address,
        snapshot.peer_address,
        policy.peer_placement.local_exit_boundary(),
    )?;
    let _drag_worker =
        crate::clipboard::drag_live::DragWorker(Arc::clone(&drag), Some(drag_thread));
    let mut live_context = LiveContext::new_with_boundary_layout(
        hook_thread_id,
        local_screen,
        policy,
        session_control.cloned(),
        boundary_map,
        logical_boundary_layout,
        config.peer_fingerprint.to_string(),
        Some(peer_screen),
    )?;
    live_context.drag = Some(drag);
    let shared = Arc::new(live_context);
    let _active_context = ActiveContextGuard::register(Arc::clone(&shared))?;
    let hooks = HookGuards::install()?;
    let raw_mouse_window = RawMouseInputWindow::create()?;
    let departure = policy.peer_placement.local_exit_boundary();
    let source_cursor_shield_window = SourceCursorShieldWindow::create(screen_extent, departure)?;
    let source_cursor_shield = source_cursor_shield_window.controller();
    let injection = Arc::new(InjectionQueue::new());

    let injector = spawn_injection_worker(
        Arc::clone(&injection),
        Arc::clone(&shared),
        screen_extent,
        source_cursor_shield,
    );
    let injector = match injector {
        Ok(injector) => injector,
        Err(error) => {
            drop(hooks);
            channel.stop();
            return Err(error);
        }
    };

    let network = LiveNetworkWorker {
        channel,
        coordinator,
        policy,
        store: options.store.clone(),
        peer_fingerprint: config.peer_fingerprint,
        shared: Arc::clone(&shared),
        injection: Arc::clone(&injection),
        source_cursor_shield,
        control: session_control.cloned(),
    }
    .spawn();
    let network = match network {
        Ok(network) => network,
        Err(error) => {
            shared.request_stop(LiveStopCode::ExternalStop);
            injection.close();
            let _ = injector.join();
            drop(hooks);
            return Err(error);
        }
    };

    let (watchdog_cancel, watchdog_receiver) = mpsc::channel();
    let watchdog = spawn_live_watchdog(
        Arc::clone(&shared),
        watchdog_receiver,
        lifetime,
        session_control.cloned(),
        screen_extent,
        effective_dpi,
    );
    let watchdog = match watchdog {
        Ok(watchdog) => watchdog,
        Err(error) => {
            shared.request_stop(LiveStopCode::ExternalStop);
            drop(hooks);
            let _ = network.join();
            injection.close();
            let _ = injector.join();
            return Err(error);
        }
    };

    if let Err(error) = run_hook_message_loop() {
        shared.fail(
            LiveStopCode::MessageLoopFailure,
            format!("live input hook message loop failed: {error}"),
        );
    }
    if LiveStopCode::load(shared.stop.load(Ordering::Acquire)) == LiveStopCode::Running {
        shared.request_stop(LiveStopCode::ExternalStop);
    }
    drop(raw_mouse_window);
    drop(hooks);
    let _ = watchdog_cancel.send(());
    join_thread(watchdog, "live control watchdog")?;
    let network = join_thread(network, "live control network")?;
    injection.close();
    let injection = join_thread(injector, "live input injector")?;
    drop(source_cursor_shield_window);

    build_live_report(
        &shared,
        config,
        local_screen,
        peer_screen,
        network,
        &injection,
    )
}

fn spawn_live_watchdog(
    shared: Arc<LiveContext>,
    cancel: mpsc::Receiver<()>,
    lifetime: Option<Duration>,
    control: Option<Arc<LiveSessionControl>>,
    screen: ScreenExtent,
    effective_dpi: u32,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pierdesk-live-control-watchdog".to_owned())
        .spawn(move || {
            let _dpi_guard = ThreadDpiAwarenessGuard::enter_per_monitor().ok();
            let started = Instant::now();
            loop {
                let remaining = lifetime.map_or(Duration::from_millis(100), |lifetime| {
                    lifetime.saturating_sub(started.elapsed())
                });
                if lifetime.is_some() && remaining.is_zero() {
                    shared.request_stop(LiveStopCode::DurationElapsed);
                    return;
                }
                if cancel
                    .recv_timeout(remaining.min(Duration::from_millis(100)))
                    .is_ok()
                {
                    return;
                }
                if let Some(control) = control.as_deref() {
                    if control.is_cancelled() {
                        shared.request_stop(LiveStopCode::ExternalStop);
                        return;
                    }
                    if control.pause_on_fullscreen
                        && foreground_covers_screen(screen).unwrap_or(false)
                    {
                        shared.request_stop(LiveStopCode::FullscreenPause);
                        return;
                    }
                }
                let display_changed = ScreenExtent::current_primary_display_mode()
                    .is_ok_and(|current| current != screen)
                    || ScreenExtent::effective_dpi().is_ok_and(|current| current != effective_dpi);
                if display_changed {
                    shared.request_stop(LiveStopCode::DisplayChanged);
                    return;
                }
            }
        })
}

fn join_thread<T>(thread: JoinHandle<T>, label: &str) -> io::Result<T> {
    thread
        .join()
        .map_err(|_| io::Error::other(format!("{label} thread panicked")))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::super::boundary::PeerPlacement;
    use super::super::state::{MouseButton, PhysicalKey, WheelAxis};
    use super::*;

    static BOUNDARY_DIRECTORY_SERIAL: AtomicU64 = AtomicU64::new(1);

    struct BoundaryTestDirectory(std::path::PathBuf);

    impl BoundaryTestDirectory {
        fn new() -> Self {
            let serial = BOUNDARY_DIRECTORY_SERIAL.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pierdesk-live-boundary-{}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for BoundaryTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn prepared_right_boundary() -> PreparedBoundaryMap {
        let peer = crate::security::CertificateFingerprint::from_bytes([7; 32]);
        let constraints = BoundaryMapConstraints {
            screen: PhysicalScreenSize::new(1_920, 1_080).unwrap(),
            corner_guard_enabled: true,
            corner_guard_pixels: 32,
            minimum_segment_pixels: DEFAULT_MINIMUM_SEGMENT_PIXELS,
        };
        BoundaryMapV2 {
            schema_version: super::super::boundary_map::BOUNDARY_MAP_SCHEMA_VERSION,
            revision: 1,
            segments: vec![super::super::boundary_map::BoundarySegment {
                id: "right-middle".to_owned(),
                peer_fingerprint: peer.to_string(),
                edge: ScreenBoundary::Right,
                start: 16_000,
                end: 49_000,
                group_id: None,
                corner: None,
            }],
        }
        .prepare_for_peer(peer, constraints)
        .unwrap()
    }

    fn test_context() -> LiveContext {
        LiveContext {
            drag: None,
            drag_retry: Mutex::new(None),
            suppress_drag_left_restore: AtomicBool::new(false),
            local_source_drag_return_eligible: AtomicBool::new(false),
            remote_left_down: AtomicBool::new(false),
            preserve_next_drag_release: AtomicBool::new(false),
            route: AtomicU8::new(LiveRoute::Local as u8),
            active_epoch: AtomicU64::new(0),
            stop: AtomicU8::new(LiveStopCode::Running as u8),
            failure: Mutex::new(None),
            hook_thread_id: 0,
            local_screen: PhysicalScreenSize::new(1_920, 1_080).unwrap(),
            local_refresh_hz: 60,
            departure_boundary: ScreenBoundary::Top,
            corner_guard_pixels: 32,
            boundary_map: None,
            logical_boundary_layout: None,
            logical_peer_fingerprint: String::new(),
            logical_peer_screen: None,
            trace_control: None,
            previous_x: Mutex::new(100),
            previous_y: Mutex::new(100),
            last_local_x: AtomicI32::new(100),
            last_local_y: AtomicI32::new(100),
            boundary_probe: Mutex::new(BoundaryProbeState::default()),
            last_raw_boundary_diagnostic_tick: AtomicU32::new(0),
            local_transition_pending: AtomicBool::new(false),
            source_first_remote_move_pending: AtomicBool::new(false),
            source_first_remote_move_sent_pending: AtomicBool::new(false),
            target_first_remote_move_pending: AtomicBool::new(false),
            target_first_remote_move_injection_pending: AtomicBool::new(false),
            target_first_remote_move_received_at: Mutex::new(None),
            pointer_gain: Mutex::new(PointerGainCalibration::default()),
            pending_enter_motion: Mutex::new(PendingEnterMotion::default()),
            recent_edge_overflow: Mutex::new(None),
            last_enter_carry: Mutex::new(EnterCarryTrace::default()),
            local_pointer_needs_baseline: AtomicBool::new(false),
            remote_pointer: Mutex::new(None),
            local_resume: Mutex::new(None),
            return_requested_at: Mutex::new(None),
            forwarded: Mutex::new(ForwardingState::default()),
            modifier_bits: AtomicU64::new(0),
            local_takeover_pending: AtomicBool::new(false),
            local_takeover_motion: Mutex::new(LocalTakeoverMotion::default()),
            local_button_mask: AtomicU8::new(0),
            inherited_remote_button_mask: AtomicU8::new(0),
            inherited_button_downs: AtomicU64::new(0),
            inherited_button_ups: AtomicU64::new(0),
            forwarded_mouse_moves: AtomicU64::new(0),
            target_injection_timing: TargetInjectionTiming::default(),
            queue: LiveQueue::new(),
            stats: Mutex::new(LiveStats::default()),
        }
    }

    fn logical_runtime_layout(
        local: crate::security::CertificateFingerprint,
        peer: crate::security::CertificateFingerprint,
    ) -> super::super::logical_boundary::BoundaryLayoutV3 {
        use super::super::logical_boundary::{
            BoundaryArcSegment, BoundaryPortal, BoundaryScreen, LOGICAL_BOUNDARY_SCHEMA_VERSION,
            LogicalBoundary,
        };

        super::super::logical_boundary::BoundaryLayoutV3 {
            schema_version: LOGICAL_BOUNDARY_SCHEMA_VERSION,
            revision: 7,
            local_fingerprint: local.to_string(),
            screens: vec![
                BoundaryScreen {
                    device_fingerprint: local.to_string(),
                    pixel_width: 1_920,
                    pixel_height: 1_080,
                    physical_width_millimeters: 520,
                    physical_height_millimeters: 290,
                    rotation_quarter_turns: 0,
                    canvas_x_millimeters: 0,
                    canvas_y_millimeters: 0,
                },
                BoundaryScreen {
                    device_fingerprint: peer.to_string(),
                    pixel_width: 2_560,
                    pixel_height: 1_440,
                    physical_width_millimeters: 600,
                    physical_height_millimeters: 340,
                    rotation_quarter_turns: 0,
                    canvas_x_millimeters: 520,
                    canvas_y_millimeters: -340,
                },
            ],
            boundaries: vec![LogicalBoundary {
                id: "right-to-top".to_owned(),
                peer_fingerprint: peer.to_string(),
                mapping_revision: 3,
                local_confirmed_revision: 3,
                peer_confirmed_revision: 3,
                reversed: true,
                source: BoundaryPortal {
                    segments: vec![BoundaryArcSegment {
                        edge: ScreenBoundary::Right,
                        start: 10_000,
                        end: 55_000,
                    }],
                },
                target: BoundaryPortal {
                    segments: vec![BoundaryArcSegment {
                        edge: ScreenBoundary::Top,
                        start: 5_000,
                        end: 50_000,
                    }],
                },
            }],
        }
    }

    #[test]
    fn logical_runtime_maps_exact_boundary_revision_and_parameter_on_both_devices() {
        let local = crate::security::CertificateFingerprint::from_bytes([21; 32]);
        let peer = crate::security::CertificateFingerprint::from_bytes([22; 32]);
        let layout = logical_runtime_layout(local, peer);
        let compiled = layout.compile(32, 64).unwrap();
        let mut source = test_context();
        source.logical_boundary_layout = Some(compiled.clone());
        source.logical_peer_fingerprint = peer.to_string();
        source.logical_peer_screen = Some(PhysicalScreenSize::new(2_560, 1_440).unwrap());

        let sample = PointerSample {
            position: PhysicalPoint { x: 1_919, y: 540 },
            delta_x: 12,
            delta_y: 0,
        };
        let transfer = source
            .logical_transfer_for_sample(ControlProtocolState::Local, sample)
            .unwrap();
        assert_eq!(transfer.departure_boundary, ScreenBoundary::Right);
        assert_eq!(transfer.transition.boundary_id, "right-to-top");
        assert_eq!(transfer.transition.mapping_revision, 3);
        let request = super::super::channel::ControlTransitionRequest {
            control_epoch: 77,
            departure_boundary: transfer.departure_boundary,
            normalized_edge_offset: 0,
            logical_boundary: Some(transfer.transition.clone()),
        };
        let peer_position = source
            .logical_destination_for_request(&request, true)
            .unwrap();
        assert_eq!(peer_position.y, 0);

        let payload = encode_boundary_sync(
            &layout,
            peer,
            BoundarySyncPhase::Applied,
            BoundarySyncSurface::Complete,
        )
        .unwrap()
        .unwrap();
        let mirrored = decode_boundary_sync(&payload, local, peer, 1).unwrap();
        let mut target = test_context();
        target.local_screen = PhysicalScreenSize::new(2_560, 1_440).unwrap();
        target.logical_boundary_layout = Some(mirrored.layout.compile(32, 64).unwrap());
        target.logical_peer_fingerprint = local.to_string();
        target.logical_peer_screen = Some(PhysicalScreenSize::new(1_920, 1_080).unwrap());
        assert_eq!(
            target.logical_inbound_arrival(&request),
            Some(peer_position)
        );

        let mut stale = request;
        stale.logical_boundary.as_mut().unwrap().mapping_revision += 1;
        assert!(!target.logical_request_is_valid(&stale));
    }

    #[test]
    fn applied_logical_workspace_is_loaded_only_for_matching_live_screens() {
        let directory = BoundaryTestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let local = store
            .load_or_create_identity()
            .unwrap()
            .identity
            .fingerprint();
        let peer = crate::security::CertificateFingerprint::from_bytes([23; 32]);
        BoundaryWorkspaceV3::initialize(&store, logical_runtime_layout(local, peer)).unwrap();
        let online = std::collections::HashSet::from([peer]);
        BoundaryWorkspaceV3::apply(&store, 0, &online, 32, 64).unwrap();

        let loaded = load_compiled_logical_boundary_layout(
            &store,
            local,
            peer,
            PhysicalScreenSize::new(1_920, 1_080).unwrap(),
            PhysicalScreenSize::new(2_560, 1_440).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded.revision(), 7);
        assert!(loaded.boundary("right-to-top").is_some());
        assert!(
            load_compiled_logical_boundary_layout(
                &store,
                local,
                peer,
                PhysicalScreenSize::new(1_920, 1_080).unwrap(),
                PhysicalScreenSize::new(2_560, 1_600).unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn v2_runtime_migrates_once_but_preserves_an_explicit_empty_map() {
        let directory = BoundaryTestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let peer = crate::security::CertificateFingerprint::from_bytes([4; 32]);
        let screen = PhysicalScreenSize::new(1_920, 1_080).unwrap();
        let settings = CrossScreenSettings::default();

        let migrated =
            load_prepared_boundary_map(&store, peer, screen, ScreenBoundary::Top, &settings)
                .unwrap()
                .unwrap();
        assert_eq!(migrated.segment_count(), 1);
        let current = BoundaryMapV2::load(&store).unwrap();
        let empty = BoundaryMapV2::replace(
            &store,
            BoundaryMapV2 {
                revision: current.revision,
                ..BoundaryMapV2::default()
            },
            BoundaryMapConstraints {
                screen,
                corner_guard_enabled: true,
                corner_guard_pixels: 32,
                minimum_segment_pixels: DEFAULT_MINIMUM_SEGMENT_PIXELS,
            },
        )
        .unwrap();
        assert!(empty.segments.is_empty());
        assert!(
            load_prepared_boundary_map(&store, peer, screen, ScreenBoundary::Top, &settings,)
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn v2_runtime_blocks_unassigned_edges_and_predicts_fast_assigned_crossing() {
        let mut context = test_context();
        context.boundary_map = Some(prepared_right_boundary());
        assert_eq!(
            context.local_departure_for_sample(PointerSample {
                position: PhysicalPoint { x: 960, y: 0 },
                delta_x: 0,
                delta_y: -8,
            }),
            None
        );
        assert_eq!(
            context.local_departure_for_sample(PointerSample {
                position: PhysicalPoint { x: 1_919, y: 540 },
                delta_x: 8,
                delta_y: 0,
            }),
            Some(ScreenBoundary::Right)
        );

        context.set_local_pointer_baseline(PhysicalPoint { x: 1_000, y: 540 });
        context.handle_raw_mouse(1_200, 0, 100);
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 1_919, y: 540 },
                    delta_x: 919,
                    delta_y: 0,
                },
                ..
            })
        ));
    }

    #[test]
    fn returning_cancelled_file_drag_does_not_repress_source_left_button() {
        let context = test_context();
        context
            .local_button_mask
            .store(mouse_button_bit(MouseButton::Left), Ordering::Release);
        context
            .suppress_drag_left_restore
            .store(true, Ordering::Release);
        context
            .local_source_drag_return_eligible
            .store(true, Ordering::Release);
        assert!(context.take_local_restore_inputs().unwrap().is_empty());
        context.handle_event(
            InputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Up,
            },
            None,
        );
        assert!(!context.suppress_drag_left_restore.load(Ordering::Acquire));
        assert!(
            !context
                .local_source_drag_return_eligible
                .load(Ordering::Acquire)
        );
    }

    #[test]
    fn target_injection_timing_reports_and_resets_aggregate_queue_delay() {
        let timing = TargetInjectionTiming::default();
        timing.observe(
            Instant::now()
                .checked_sub(Duration::from_millis(3))
                .unwrap(),
        );
        timing.observe(
            Instant::now()
                .checked_sub(Duration::from_millis(10))
                .unwrap(),
        );
        let snapshot = timing.take();
        assert_eq!(snapshot.moves, 2);
        assert!(snapshot.queue_delay_max_micros >= 8_000);
        assert_eq!(snapshot.queue_delays_over_2_ms, 2);
        assert_eq!(snapshot.queue_delays_over_8_ms, 1);
        assert_eq!(timing.take().moves, 0);
    }

    #[test]
    fn returned_source_release_is_left_to_the_preview_cancellation_loop() {
        assert!(should_cancel_local_drag_after_left_release(
            false, true, false
        ));
        assert!(!should_cancel_local_drag_after_left_release(
            false, true, true
        ));
        assert!(!should_cancel_local_drag_after_left_release(
            true, true, false
        ));
        assert!(!should_cancel_local_drag_after_left_release(
            false, false, false
        ));
    }

    #[test]
    fn only_a_held_local_source_drag_is_represented_on_boundary_return() {
        assert!(should_represent_returning_local_drag(true, true, true));
        assert!(!should_represent_returning_local_drag(false, true, true));
        assert!(!should_represent_returning_local_drag(true, false, true));
        assert!(!should_represent_returning_local_drag(true, true, false));
    }

    #[test]
    fn control_return_preserves_both_reverse_and_returning_source_drags() {
        assert!(should_preserve_drag_on_control_return(true, false));
        assert!(should_preserve_drag_on_control_return(false, true));
        assert!(should_preserve_drag_on_control_return(true, true));
        assert!(!should_preserve_drag_on_control_return(false, false));
        assert!(should_suppress_drag_left_restore(true, false, false));
        assert!(should_suppress_drag_left_restore(false, true, false));
        assert!(should_suppress_drag_left_restore(false, false, true));
        assert!(!should_suppress_drag_left_restore(false, false, false));
    }

    #[test]
    fn shell_desktop_classes_never_count_as_fullscreen_applications() {
        assert!(is_shell_desktop_class_name(
            &"Progman".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(is_shell_desktop_class_name(
            &"WorkerW".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!is_shell_desktop_class_name(
            &"Chrome_WidgetWin_1".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!is_shell_desktop_class_name(
            &"WorkerWindow".encode_utf16().collect::<Vec<_>>()
        ));
    }

    #[test]
    fn modifier_tracker_requires_all_three_modifiers_and_never_depends_on_forwarding_state() {
        assert_eq!(modifier_bit(VK_LCONTROL_VALUE), Some(MODIFIER_LEFT_CONTROL));
        assert_eq!(modifier_bit(VK_RMENU_VALUE), Some(MODIFIER_RIGHT_ALT));
        assert_eq!(modifier_bit(VK_LSHIFT_VALUE), Some(MODIFIER_LEFT_SHIFT));
        assert_eq!(modifier_bit(0x41), None);
    }

    #[test]
    fn session_status_records_each_control_return_reason() {
        let control = LiveSessionControl::new(false);

        control.record_control_return("local_physical_takeover");
        control.record_control_return("boundary_return_ack");
        control.record_transition_trace("kind=enter_request,from=top".to_owned());

        let status = control.snapshot();
        assert_eq!(
            status.last_control_return_reason,
            Some("boundary_return_ack")
        );
        assert_eq!(status.control_return_sequence, 2);
        assert_eq!(
            status.last_transition_trace.as_deref(),
            Some("kind=enter_request,from=top")
        );
        assert_eq!(status.transition_trace_sequence, 1);
    }

    #[test]
    fn transition_trace_buffer_retains_intermediate_events_between_status_polls() {
        let control = LiveSessionControl::new(false);
        control.update(|status| {
            status.session_id = Some("SESSION".to_owned());
            status.channel_epoch = Some(42);
        });

        control.record_transition_trace("kind=edge_contact".to_owned());
        control.record_transition_trace("kind=enter_request".to_owned());
        control.record_transition_trace("kind=enter_ack".to_owned());

        let (events, dropped) = control.traces.drain();
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].trace, "kind=enter_request");
        assert_eq!(events[2].session_id.as_deref(), Some("SESSION"));
        assert_eq!(events[2].channel_epoch, Some(42));
    }

    #[test]
    fn edge_probe_distinguishes_processed_and_raw_outward_motion_after_coalescing() {
        let context = test_context();
        let control = LiveSessionControl::new(false);

        assert!(!context.handle_event(InputEvent::MouseMove { x: 100, y: 1 }, Some(10)));
        assert!(!context.handle_event(InputEvent::MouseMove { x: 100, y: 0 }, Some(11)));
        context.handle_raw_mouse(0, -20, 11);
        context.flush_boundary_probe_events(Some(&control));

        let (events, dropped) = control.traces.drain();
        assert_eq!(dropped, 0);
        let traces = events
            .into_iter()
            .map(|event| event.trace)
            .collect::<Vec<_>>();
        assert!(traces.iter().any(|trace| {
            trace.contains("kind=edge_contact") && trace.contains("queue=coalesced")
        }));
        assert!(
            traces
                .iter()
                .any(|trace| trace.contains("kind=edge_processed_outward"))
        );
        assert!(
            traces
                .iter()
                .any(|trace| trace.contains("kind=edge_raw_outward"))
        );
    }

    #[test]
    fn local_hook_keeps_a_qualifying_edge_sample_ahead_of_fast_recoil() {
        let context = test_context();

        assert!(!context.handle_event(InputEvent::MouseMove { x: 100, y: 1 }, Some(10)));
        assert!(!context.handle_event(InputEvent::MouseMove { x: 100, y: 0 }, Some(11)));
        assert!(!context.handle_event(InputEvent::MouseMove { x: 101, y: 1 }, Some(12)));

        let first = context.queue.pop_until(Instant::now()).unwrap();
        let second = context.queue.pop_until(Instant::now()).unwrap();
        assert!(matches!(
            first,
            LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 100, y: 0 },
                    delta_y: -1,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            second,
            LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 101, y: 1 },
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn cursor_visibility_reaches_hidden_state_and_restores_its_exact_balance() {
        let display_count = std::cell::Cell::new(2_i32);
        let calls = std::cell::Cell::new(0_u32);
        let adjust = |show: bool| {
            calls.set(calls.get() + 1);
            let next = display_count.get() + if show { 1 } else { -1 };
            display_count.set(next);
            next
        };
        let mut balance = CursorVisibilityBalance::default();

        balance.hide_with(adjust).unwrap();
        assert_eq!(display_count.get(), -1);
        assert_eq!(balance.hide_calls, 3);
        assert_eq!(calls.get(), 3);

        balance.hide_with(adjust).unwrap();
        assert_eq!(calls.get(), 3);

        balance.restore_with(adjust);
        assert_eq!(display_count.get(), 2);
        assert_eq!(balance.hide_calls, 0);
        assert_eq!(calls.get(), 6);
    }

    #[test]
    fn cursor_visibility_hide_is_bounded_and_still_recoverable() {
        let calls = std::cell::Cell::new(0_u32);
        let adjust = |show: bool| {
            calls.set(calls.get() + 1);
            i32::from(show)
        };
        let mut balance = CursorVisibilityBalance::default();

        let error = balance.hide_with(adjust).unwrap_err();
        assert_eq!(
            error.to_string(),
            "cursor display counter did not reach the hidden state"
        );
        assert_eq!(balance.hide_calls, MAX_CURSOR_VISIBILITY_ADJUSTMENTS);
        balance.restore_with(adjust);
        assert_eq!(calls.get(), MAX_CURSOR_VISIBILITY_ADJUSTMENTS * 2);
        assert_eq!(balance.hide_calls, 0);
    }

    #[test]
    fn cursor_visibility_repair_recovers_negative_count_without_positive_drift() {
        let display_count = std::cell::Cell::new(-3_i32);
        let adjust = |show: bool| {
            let next = display_count.get() + if show { 1 } else { -1 };
            display_count.set(next);
            next
        };
        let mut balance = CursorVisibilityBalance::default();

        balance.ensure_visible_with(adjust).unwrap();
        assert_eq!(display_count.get(), 0);

        for _ in 0..10 {
            balance.ensure_visible_with(adjust).unwrap();
        }
        assert_eq!(display_count.get(), 0);
    }

    #[test]
    fn cursor_visibility_repair_preserves_an_existing_positive_count() {
        let display_count = std::cell::Cell::new(4_i32);
        let adjust = |show: bool| {
            let next = display_count.get() + if show { 1 } else { -1 };
            display_count.set(next);
            next
        };
        let mut balance = CursorVisibilityBalance::default();

        balance.ensure_visible_with(adjust).unwrap();
        assert_eq!(display_count.get(), 4);
        balance.hide_with(adjust).unwrap();
        assert_eq!(display_count.get(), -1);
        balance.ensure_visible_with(adjust).unwrap();
        assert_eq!(display_count.get(), 4);
    }

    #[test]
    fn cursor_shape_capture_restores_the_first_non_null_shape_exactly_once() {
        let original = HCURSOR(std::ptr::dangling_mut());
        let mut capture = CursorShapeCapture::default();

        capture.hide_with(|| original);
        capture.hide_with(HCURSOR::default);
        assert!(capture.active);
        assert_eq!(capture.previous, Some(original));

        let restored = std::cell::Cell::new(None);
        capture.restore_with(|cursor| restored.set(Some(cursor)));
        assert_eq!(restored.get(), Some(original));
        assert!(!capture.active);
        assert_eq!(capture.previous, None);

        capture.restore_with(|_| panic!("inactive cursor shape capture restored twice"));
    }

    #[test]
    fn source_cursor_visibility_repeated_sessions_preserve_the_original_count() {
        for initial in [-3, 0, 4] {
            let count = std::cell::Cell::new(initial);
            let adjust = |show| {
                count.set(count.get() + if show { 1 } else { -1 });
                count.get()
            };
            let mut guard = SourceCursorVisibility::default();
            for _ in 0..20 {
                guard.0.hide_with(adjust).unwrap();
                let hidden = count.get();
                assert!(hidden < 0);
                // Repeated forwarding commands / shape repairs must not stack hides.
                guard.0.hide_with(adjust).unwrap();
                assert_eq!(count.get(), hidden);
                guard.0.restore_with(adjust);
                guard.0.restore_with(adjust);
                assert_eq!(count.get(), initial);
            }
        }
    }

    #[test]
    fn source_cursor_shape_repair_does_not_replace_the_return_cursor_with_busy() {
        let mut original_token = 0_u8;
        let mut busy_token = 0_u8;
        let original = HCURSOR(std::ptr::from_mut(&mut original_token).cast());
        let busy = HCURSOR(std::ptr::from_mut(&mut busy_token).cast());
        let mut capture = CursorShapeCapture::default();
        capture.hide_with(|| original);
        capture.hide_with(|| busy);
        capture.restore_with(|cursor| assert_eq!(cursor, original));
        capture.restore_with(|_| panic!("double restore"));
    }

    #[test]
    fn source_cursor_shield_window_can_be_shown_and_hidden_without_activation() {
        let window = SourceCursorShieldWindow::create(
            ScreenExtent {
                width: 2,
                height: 2,
            },
            ScreenBoundary::Top,
        )
        .unwrap();
        let controller = window.controller();
        let handle = HWND(controller.handle as *mut core::ffi::c_void);

        assert!(!unsafe { IsWindowVisible(handle) }.as_bool());
        controller.set_active(true).unwrap();
        let mut message = MSG::default();
        assert!(
            unsafe {
                PeekMessageW(
                    &raw mut message,
                    Some(handle),
                    SOURCE_CURSOR_SHIELD_SET_ACTIVE,
                    SOURCE_CURSOR_SHIELD_SET_ACTIVE,
                    PM_REMOVE,
                )
            }
            .as_bool()
        );
        // SAFETY: PeekMessageW returned one initialized message for this owned test window.
        unsafe { DispatchMessageW(&raw const message) };
        assert!(unsafe { IsWindowVisible(handle) }.as_bool());
        controller.set_active(false).unwrap();
        assert!(
            unsafe {
                PeekMessageW(
                    &raw mut message,
                    Some(handle),
                    SOURCE_CURSOR_SHIELD_SET_ACTIVE,
                    SOURCE_CURSOR_SHIELD_SET_ACTIVE,
                    PM_REMOVE,
                )
            }
            .as_bool()
        );
        // SAFETY: same initialized-message invariant as the show command above.
        unsafe { DispatchMessageW(&raw const message) };
        assert!(!unsafe { IsWindowVisible(handle) }.as_bool());
    }

    #[test]
    fn source_cursor_shield_covers_the_full_primary_screen_for_every_layout() {
        let screen = ScreenExtent {
            width: 3_840,
            height: 2_160,
        };
        for boundary in [
            ScreenBoundary::Left,
            ScreenBoundary::Right,
            ScreenBoundary::Top,
            ScreenBoundary::Bottom,
        ] {
            assert_eq!(
                source_cursor_shield_bounds(screen, boundary),
                (0, 0, 3_840, 2_160),
                "source UI isolation must not depend on the configured boundary"
            );
        }
    }

    #[test]
    #[ignore = "briefly moves and restores the real cursor to verify the Win32 confinement contract"]
    fn source_cursor_confinement_pins_both_axes_and_restores_the_previous_clip() {
        let mut original_cursor = POINT::default();
        unsafe { GetCursorPos(&raw mut original_cursor) }.unwrap();
        let mut confinement = SourceCursorConfinement::default();
        let probe = (|| -> io::Result<(POINT, POINT)> {
            let anchor = confinement.confine_current()?;
            unsafe { SetCursorPos(anchor.x.saturating_add(97), anchor.y.saturating_add(53)) }
                .map_err(io::Error::other)?;
            let mut observed = POINT::default();
            unsafe { GetCursorPos(&raw mut observed) }.map_err(io::Error::other)?;
            Ok((anchor, observed))
        })();
        let release = confinement.release();
        let restore = unsafe { SetCursorPos(original_cursor.x, original_cursor.y) };

        let (anchor, observed) = probe.unwrap();
        assert_eq!(observed, anchor);
        assert!(release.unwrap());
        restore.unwrap();
    }

    #[test]
    fn live_path_ignores_only_stale_or_mismatched_transition_acknowledgements() {
        use super::super::channel::ControlTransitionAck;

        let enter = ChannelControlEvent::EnterAcknowledgement(ControlTransitionAck {
            control_epoch: 41,
            accepted: true,
        });
        let returned = ChannelControlEvent::ReturnAcknowledgement(ControlTransitionAck {
            control_epoch: 73,
            accepted: true,
        });

        assert!(!transition_acknowledgement_is_stale(
            &enter,
            ControlProtocolState::AwaitingEnterAcknowledgement,
            Some(41)
        ));
        assert!(transition_acknowledgement_is_stale(
            &enter,
            ControlProtocolState::Local,
            None
        ));
        assert!(transition_acknowledgement_is_stale(
            &enter,
            ControlProtocolState::AwaitingEnterAcknowledgement,
            Some(40)
        ));
        assert!(!transition_acknowledgement_is_stale(
            &returned,
            ControlProtocolState::AwaitingReturnAcknowledgement,
            Some(73)
        ));
        assert!(transition_acknowledgement_is_stale(
            &returned,
            ControlProtocolState::Local,
            None
        ));
        assert!(!transition_acknowledgement_is_stale(
            &ChannelControlEvent::ReleaseAll,
            ControlProtocolState::Local,
            None
        ));
    }

    #[test]
    fn target_injection_queue_preserves_an_ordinary_mouse_burst() {
        let queue = InjectionQueue::new();
        for coordinate in 0..4 {
            queue
                .push_input(InputEvent::MouseMove {
                    x: coordinate,
                    y: coordinate + 10,
                })
                .unwrap();
        }
        queue.close();

        for coordinate in 0..4 {
            assert!(matches!(
                queue.pop(),
                Some(InjectionQueueItem::Command(InjectionCommand::Input(
                    InputEvent::MouseMove { x, y },
                    _
                ))) if x == coordinate && y == coordinate + 10
            ));
        }
        assert!(queue.pop().is_none());
        assert_eq!(queue.take_coalesced_mouse_moves(), 0);
    }

    #[test]
    fn target_injection_queue_bounds_extreme_mouse_bursts_without_reordering_buttons() {
        let queue = InjectionQueue::new();
        let total_moves = TARGET_MOUSE_BURST_CAPACITY + 4;
        for coordinate in 0..total_moves {
            queue
                .push_input(InputEvent::MouseMove {
                    x: i32::try_from(coordinate).unwrap(),
                    y: 0,
                })
                .unwrap();
        }
        queue
            .push_input(InputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Up,
            })
            .unwrap();
        queue.close();

        let mut moves = Vec::new();
        while let Some(item) = queue.pop() {
            match item {
                InjectionQueueItem::Command(InjectionCommand::Input(
                    InputEvent::MouseMove { x, .. },
                    _,
                )) => moves.push(x),
                InjectionQueueItem::Command(InjectionCommand::Input(
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        transition: KeyTransition::Up,
                    },
                    _,
                )) => break,
                _ => panic!("unexpected injection queue item"),
            }
        }
        assert_eq!(moves.len(), TARGET_MOUSE_BURST_CAPACITY);
        assert_eq!(
            moves.last().copied(),
            Some(i32::try_from(total_moves - 1).unwrap())
        );
        assert_eq!(queue.take_coalesced_mouse_moves(), 4);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn release_all_discards_queued_remote_input_and_async_arrival_position() {
        let queue = InjectionQueue::new();
        queue
            .push_input(InputEvent::MouseMove { x: 400, y: 300 })
            .unwrap();
        queue
            .position_async(PhysicalPoint { x: 10, y: 20 })
            .unwrap();

        queue.request_release_all();
        assert!(matches!(queue.pop(), Some(InjectionQueueItem::ReleaseAll)));
        queue.close();
        assert!(queue.pop().is_none());
    }

    #[test]
    fn target_cursor_preparation_is_async_and_ordered_before_arrival() {
        let queue = InjectionQueue::new();
        let arrival = PhysicalPoint { x: 640, y: 1 };

        queue.request_release_all();
        queue.set_visible_async(true).unwrap();
        queue.position_async(arrival).unwrap();

        assert!(matches!(queue.pop(), Some(InjectionQueueItem::ReleaseAll)));
        assert!(matches!(
            queue.pop(),
            Some(InjectionQueueItem::Command(
                InjectionCommand::VisibilityAsync(true)
            ))
        ));
        assert!(matches!(
            queue.pop(),
            Some(InjectionQueueItem::Command(
                InjectionCommand::PositionAsync(point)
            )) if point == arrival
        ));
        queue.close();
        assert!(queue.pop().is_none());
    }

    #[test]
    fn source_cursor_repairs_coalesce_and_release_all_discards_them() {
        let queue = InjectionQueue::new();
        for _ in 0..(INJECTION_QUEUE_CAPACITY * 4) {
            queue.repair_source_cursor_async().unwrap();
        }

        assert!(matches!(
            queue.pop(),
            Some(InjectionQueueItem::RepairSourceCursor)
        ));
        queue.repair_source_cursor_async().unwrap();
        queue.request_release_all();
        assert!(matches!(queue.pop(), Some(InjectionQueueItem::ReleaseAll)));
        queue.close();
        assert!(queue.pop().is_none());
    }

    #[test]
    fn boundary_resume_is_inset_one_physical_pixel_on_every_edge() {
        let screen = PhysicalScreenSize::new(1_920, 1_080).unwrap();
        assert_eq!(
            inset_boundary_resume(PhysicalPoint { x: 0, y: 500 }, screen),
            PhysicalPoint { x: 1, y: 500 }
        );
        assert_eq!(
            inset_boundary_resume(PhysicalPoint { x: 1_919, y: 500 }, screen),
            PhysicalPoint { x: 1_918, y: 500 }
        );
        assert_eq!(
            inset_boundary_resume(PhysicalPoint { x: 500, y: 0 }, screen),
            PhysicalPoint { x: 500, y: 1 }
        );
        assert_eq!(
            inset_boundary_resume(PhysicalPoint { x: 500, y: 1_079 }, screen),
            PhysicalPoint { x: 500, y: 1_078 }
        );
        assert_eq!(
            inset_boundary_resume(PhysicalPoint { x: 500, y: 500 }, screen),
            PhysicalPoint { x: 500, y: 500 }
        );
    }

    #[test]
    fn remote_input_conversion_preserves_scan_buttons_wheels_and_normalized_pixels() {
        let screen = PhysicalScreenSize::new(3_840, 2_160).unwrap();
        let key = RemoteInputEvent::Key {
            physical: PhysicalKey {
                scan_code: 0x1d,
                extended: true,
            },
            virtual_key: VK_RCONTROL_VALUE,
            transition: KeyTransition::Down,
        };
        assert_eq!(
            local_input_event(key, screen),
            InputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 0x1d,
                    extended: true,
                },
                virtual_key: VK_RCONTROL_VALUE,
                transition: KeyTransition::Down,
            }
        );
        assert_eq!(
            local_input_event(
                RemoteInputEvent::MouseMove(NormalizedMousePosition { x: u16::MAX, y: 0 }),
                screen,
            ),
            InputEvent::MouseMove { x: 3_839, y: 0 }
        );
        assert_eq!(
            remote_event_from_input(InputEvent::MouseButton {
                button: MouseButton::X1,
                transition: KeyTransition::Up,
            }),
            Some(RemoteInputEvent::MouseButton {
                button: MouseButton::X1,
                transition: KeyTransition::Up,
            })
        );
        assert_eq!(
            remote_event_from_input(InputEvent::Wheel {
                axis: WheelAxis::Horizontal,
                delta: -120,
            }),
            Some(RemoteInputEvent::Wheel {
                axis: WheelAxis::Horizontal,
                delta: -120,
            })
        );
    }

    #[test]
    fn live_queue_coalesces_only_adjacent_pointer_samples() {
        let queue = LiveQueue::new();
        let pointer = |x| LiveSignal::Pointer {
            sample: PointerSample {
                position: PhysicalPoint { x, y: 100 },
                delta_x: 1,
                delta_y: 0,
            },
            remote_position: None,
        };
        assert_eq!(queue.push_pointer(pointer(1)), LiveEnqueueResult::Queued);
        assert_eq!(queue.push_pointer(pointer(2)), LiveEnqueueResult::Coalesced);
        assert_eq!(
            queue.push_inputs(&[RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Down,
            }]),
            LiveEnqueueResult::Queued
        );
        assert_eq!(queue.push_pointer(pointer(3)), LiveEnqueueResult::Queued);
        assert_eq!(queue.pop_until(Instant::now()), Some(pointer(2)));
        assert!(matches!(
            queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(_))
        ));
        assert_eq!(queue.pop_until(Instant::now()), Some(pointer(3)));
        assert_eq!(queue.coalesced_moves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stationary_edge_packet_cannot_erase_a_coalesced_outward_push() {
        let queue = LiveQueue::new();
        let edge = PhysicalPoint { x: 1_919, y: 540 };
        let outward = LiveSignal::Pointer {
            sample: PointerSample {
                position: edge,
                delta_x: 7,
                delta_y: 0,
            },
            remote_position: None,
        };
        let clamped = LiveSignal::Pointer {
            sample: PointerSample {
                position: edge,
                delta_x: 0,
                delta_y: 0,
            },
            remote_position: None,
        };

        assert_eq!(queue.push_pointer(outward), LiveEnqueueResult::Queued);
        assert_eq!(queue.push_pointer(clamped), LiveEnqueueResult::Coalesced);
        assert_eq!(queue.pop_until(Instant::now()), Some(outward));
    }

    #[test]
    fn qualifying_boundary_packet_survives_a_later_recoil_packet() {
        let queue = LiveQueue::new();
        let outward = LiveSignal::Pointer {
            sample: PointerSample {
                position: PhysicalPoint { x: 1_919, y: 540 },
                delta_x: 24,
                delta_y: 0,
            },
            remote_position: None,
        };
        let recoil = LiveSignal::Pointer {
            sample: PointerSample {
                position: PhysicalPoint { x: 1_918, y: 541 },
                delta_x: -1,
                delta_y: 1,
            },
            remote_position: None,
        };

        assert_eq!(
            queue.push_transition_pointer(outward),
            LiveEnqueueResult::Queued
        );
        assert_eq!(queue.push_pointer(recoil), LiveEnqueueResult::Queued);
        assert_eq!(queue.pop_until(Instant::now()), Some(outward));
        assert_eq!(queue.pop_until(Instant::now()), Some(recoil));
        assert_eq!(queue.coalesced_moves.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn raw_motion_predicts_crossing_for_each_configured_boundary() {
        let screen = PhysicalScreenSize::new(1_920, 1_080).unwrap();
        let gain = PointerGainModel::default();
        let cases = [
            (
                ScreenBoundary::Left,
                (40, 540),
                (-50, 0),
                PhysicalPoint { x: 0, y: 540 },
                (-40, 0),
            ),
            (
                ScreenBoundary::Right,
                (1_870, 540),
                (60, 0),
                PhysicalPoint { x: 1_919, y: 540 },
                (49, 0),
            ),
            (
                ScreenBoundary::Top,
                (960, 40),
                (0, -50),
                PhysicalPoint { x: 960, y: 0 },
                (0, -40),
            ),
            (
                ScreenBoundary::Bottom,
                (960, 1_030),
                (0, 60),
                PhysicalPoint { x: 960, y: 1_079 },
                (0, 49),
            ),
        ];
        for (boundary, current, raw, position, delta) in cases {
            let sample =
                raw_boundary_transition_sample(current, raw, gain, screen, boundary, 32).unwrap();
            assert_eq!(sample.position, position);
            assert_eq!((sample.delta_x, sample.delta_y), delta);
        }
    }

    #[test]
    fn raw_prediction_uses_the_boundary_intersection_and_preserves_corner_guard() {
        let screen = PhysicalScreenSize::new(1_920, 1_080).unwrap();
        let sample = raw_boundary_transition_sample(
            (100, 100),
            (100, -200),
            PointerGainModel::default(),
            screen,
            ScreenBoundary::Top,
            32,
        )
        .unwrap();
        assert_eq!(sample.position, PhysicalPoint { x: 150, y: 0 });
        assert_eq!((sample.delta_x, sample.delta_y), (50, -100));

        assert!(
            raw_boundary_transition_sample(
                (10, 40),
                (0, -80),
                PointerGainModel::default(),
                screen,
                ScreenBoundary::Top,
                32,
            )
            .is_none()
        );
        assert!(
            raw_boundary_transition_sample(
                (960, 100),
                (0, -99),
                PointerGainModel::default(),
                screen,
                ScreenBoundary::Top,
                32,
            )
            .is_none()
        );
        assert!(
            raw_boundary_transition_sample(
                (960, 100),
                (0, 200),
                PointerGainModel::default(),
                screen,
                ScreenBoundary::Top,
                32,
            )
            .is_none()
        );
    }

    #[test]
    fn raw_predicted_crossing_is_single_and_survives_processed_recoil() {
        let context = test_context();
        context.set_local_pointer_baseline(PhysicalPoint { x: 960, y: 80 });

        context.handle_raw_mouse(0, -120, 100);
        assert!(context.local_transition_pending.load(Ordering::Acquire));
        assert!(!context.handle_mouse_input(InputEvent::MouseMove { x: 960, y: 1 }, 101));
        context.handle_raw_mouse(0, -30, 102);

        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 960, y: 0 },
                    delta_x: 0,
                    delta_y: -80,
                },
                ..
            })
        ));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 960, y: 1 },
                    ..
                },
                ..
            })
        ));
        assert_eq!(context.queue.pop_until(Instant::now()), None);

        context.cancel_pending_enter_capture();
        assert!(!context.local_transition_pending.load(Ordering::Acquire));
    }

    #[test]
    fn raw_predicted_crossing_carries_only_distance_beyond_the_source_edge() {
        let context = test_context();
        context.set_local_pointer_baseline(PhysicalPoint { x: 960, y: 80 });
        context.handle_raw_mouse(0, -120, 100);

        context.begin_pending_enter_capture_at(100, ScreenBoundary::Top);
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();

        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 1_120, y: 1_359 },
                    delta_x: 0,
                    delta_y: -40,
                },
                ..
            })
        ));
    }

    #[test]
    fn raw_prediction_does_not_apply_a_processed_packet_twice() {
        let context = test_context();
        context.set_local_pointer_baseline(PhysicalPoint { x: 960, y: 50 });
        context
            .pointer_gain
            .lock()
            .unwrap()
            .record_processed(ProcessedPointerSample {
                message_time_ms: 100,
                delta_x: 0,
                delta_y: -50,
                x_clamped: false,
                y_clamped: false,
            });

        context.handle_raw_mouse(0, -100, 100);

        assert_eq!(context.queue.pop_until(Instant::now()), None);
        assert!(!context.local_transition_pending.load(Ordering::Acquire));
    }

    #[test]
    fn raw_overflow_recovers_a_clamped_packet_without_processed_outward_delta() {
        let context = test_context();
        context.set_local_pointer_baseline(PhysicalPoint { x: 960, y: 0 });
        context
            .pointer_gain
            .lock()
            .unwrap()
            .record_processed(ProcessedPointerSample {
                message_time_ms: 100,
                delta_x: 0,
                delta_y: 0,
                x_clamped: false,
                y_clamped: true,
            });

        context.handle_raw_mouse(0, -80, 100);

        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 960, y: 0 },
                    delta_x: 0,
                    delta_y: -1,
                },
                ..
            })
        ));
        assert!(context.local_transition_pending.load(Ordering::Acquire));
    }

    #[test]
    fn near_edge_raw_diagnostics_are_bounded_by_distance_and_time() {
        let context = test_context();
        context.set_local_pointer_baseline(PhysicalPoint { x: 960, y: 400 });

        context.handle_raw_mouse(0, -1, 100);
        context.handle_raw_mouse(0, -1, 110);
        context.handle_raw_mouse(0, -1, 160);
        context.set_local_pointer_baseline(PhysicalPoint { x: 960, y: 700 });
        context.handle_raw_mouse(0, -1, 220);

        let probe = context.boundary_probe.lock().unwrap();
        let near = probe
            .events
            .iter()
            .filter(|event| event.kind == BoundaryProbeKind::RawNearUnmatched)
            .count();
        assert_eq!(near, 2);
    }

    #[test]
    fn tangential_edge_motion_cannot_erase_the_outward_axis() {
        let cases = [
            (
                PhysicalPoint { x: 0, y: 500 },
                PointerSample {
                    position: PhysicalPoint { x: 0, y: 500 },
                    delta_x: -19,
                    delta_y: 4,
                },
                PointerSample {
                    position: PhysicalPoint { x: 0, y: 524 },
                    delta_x: 0,
                    delta_y: 24,
                },
                (-19, 24),
            ),
            (
                PhysicalPoint { x: 1_919, y: 500 },
                PointerSample {
                    position: PhysicalPoint { x: 1_919, y: 500 },
                    delta_x: 31,
                    delta_y: -3,
                },
                PointerSample {
                    position: PhysicalPoint { x: 1_919, y: 471 },
                    delta_x: 0,
                    delta_y: -29,
                },
                (31, -29),
            ),
            (
                PhysicalPoint { x: 900, y: 0 },
                PointerSample {
                    position: PhysicalPoint { x: 900, y: 0 },
                    delta_x: 2,
                    delta_y: -47,
                },
                PointerSample {
                    position: PhysicalPoint { x: 963, y: 0 },
                    delta_x: 63,
                    delta_y: 0,
                },
                (63, -47),
            ),
            (
                PhysicalPoint { x: 900, y: 1_079 },
                PointerSample {
                    position: PhysicalPoint { x: 900, y: 1_079 },
                    delta_x: -5,
                    delta_y: 53,
                },
                PointerSample {
                    position: PhysicalPoint { x: 824, y: 1_079 },
                    delta_x: -76,
                    delta_y: 0,
                },
                (-76, 53),
            ),
        ];

        for (edge, outward, tangential, expected_delta) in cases {
            let queue = LiveQueue::new();
            assert_eq!(outward.position, edge);
            assert_eq!(
                queue.push_pointer(LiveSignal::Pointer {
                    sample: outward,
                    remote_position: None,
                }),
                LiveEnqueueResult::Queued
            );
            assert_eq!(
                queue.push_pointer(LiveSignal::Pointer {
                    sample: tangential,
                    remote_position: None,
                }),
                LiveEnqueueResult::Coalesced
            );
            let Some(LiveSignal::Pointer { sample, .. }) = queue.pop_until(Instant::now()) else {
                panic!("coalesced pointer was missing");
            };
            assert_eq!((sample.delta_x, sample.delta_y), expected_delta);
            assert_eq!(sample.position, tangential.position);
        }
    }

    #[test]
    fn repeated_high_rate_tangential_packets_preserve_top_edge_push() {
        let queue = LiveQueue::new();
        assert_eq!(
            queue.push_pointer(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 1_000, y: 0 },
                    delta_x: 18,
                    delta_y: -120,
                },
                remote_position: None,
            }),
            LiveEnqueueResult::Queued
        );
        for x in 1_001..1_064 {
            assert_eq!(
                queue.push_pointer(LiveSignal::Pointer {
                    sample: PointerSample {
                        position: PhysicalPoint { x, y: 0 },
                        delta_x: 1,
                        delta_y: 0,
                    },
                    remote_position: None,
                }),
                LiveEnqueueResult::Coalesced
            );
        }

        let Some(LiveSignal::Pointer { sample, .. }) = queue.pop_until(Instant::now()) else {
            panic!("coalesced pointer was missing");
        };
        assert_eq!(sample.position, PhysicalPoint { x: 1_063, y: 0 });
        assert_eq!((sample.delta_x, sample.delta_y), (1, -120));
    }

    #[test]
    fn network_notification_is_remembered_before_wait_and_wakes_a_sleeping_worker() {
        let queue = Arc::new(LiveQueue::new());
        queue.notify_network();
        let start = Instant::now();
        assert!(queue.pop_until(start + Duration::from_secs(2)).is_none());
        assert!(start.elapsed() < Duration::from_millis(100));
        let waiting = Arc::clone(&queue);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = waiting.pop_until(Instant::now() + Duration::from_secs(5));
            tx.send(result.is_none()).unwrap();
        });
        queue.notify_network();
        assert!(rx.recv_timeout(Duration::from_millis(500)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn transient_live_queue_lock_contention_waits_instead_of_reporting_failure() {
        let queue = Arc::new(LiveQueue::new());
        let held = queue.state.lock().unwrap();
        let worker_queue = Arc::clone(&queue);
        let (attempting_sender, attempting_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            attempting_sender.send(()).unwrap();
            let result = worker_queue.push_pointer(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 10, y: 20 },
                    delta_x: 1,
                    delta_y: 0,
                },
                remote_position: None,
            });
            result_sender.send(result).unwrap();
        });

        attempting_receiver.recv().unwrap();
        assert_eq!(
            result_receiver.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        drop(held);
        assert_eq!(result_receiver.recv().unwrap(), LiveEnqueueResult::Queued);
        worker.join().unwrap();
    }

    #[test]
    fn full_live_queue_bypasses_new_pointer_without_reordering_reliable_input() {
        let queue = LiveQueue::new();
        let pointer = |x| LiveSignal::Pointer {
            sample: PointerSample {
                position: PhysicalPoint { x, y: 100 },
                delta_x: 1,
                delta_y: 0,
            },
            remote_position: Some(NormalizedMousePosition {
                x: u16::try_from(x).unwrap(),
                y: 100,
            }),
        };
        let reliable = RemoteInputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        };

        assert_eq!(queue.push_pointer(pointer(1)), LiveEnqueueResult::Queued);
        assert_eq!(
            queue.push_inputs(&vec![reliable; LIVE_QUEUE_CAPACITY - 1]),
            LiveEnqueueResult::Queued
        );
        assert_eq!(
            queue.push_pointer(pointer(2)),
            LiveEnqueueResult::MouseMoveBypassed
        );
        assert_eq!(queue.pop_until(Instant::now()), Some(pointer(1)));
        for _ in 0..LIVE_QUEUE_CAPACITY - 1 {
            assert!(matches!(
                queue.pop_until(Instant::now()),
                Some(LiveSignal::Input(_))
            ));
        }
        assert_eq!(queue.pop_until(Instant::now()), None);
        assert_eq!(queue.coalesced_moves.load(Ordering::Relaxed), 0);
        assert_eq!(queue.bypassed_moves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reliable_input_never_evicts_a_pointer_across_an_ordering_barrier() {
        let queue = LiveQueue::new();
        let pointer = LiveSignal::Pointer {
            sample: PointerSample {
                position: PhysicalPoint { x: 10, y: 20 },
                delta_x: 1,
                delta_y: 0,
            },
            remote_position: None,
        };
        let reliable = RemoteInputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        };

        assert_eq!(queue.push_pointer(pointer), LiveEnqueueResult::Queued);
        assert_eq!(
            queue.push_inputs(&vec![reliable; LIVE_QUEUE_CAPACITY - 1]),
            LiveEnqueueResult::Queued
        );
        assert_eq!(
            queue.push_inputs(&[RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Up,
            }]),
            LiveEnqueueResult::Fatal
        );
        assert_eq!(queue.pop_until(Instant::now()), Some(pointer));
        assert_eq!(queue.coalesced_moves.load(Ordering::Relaxed), 0);
        assert_eq!(queue.peak_depth(), LIVE_QUEUE_CAPACITY);
    }

    #[test]
    fn mouse_move_is_bypassed_without_closing_a_queue_full_of_reliable_input() {
        let queue = LiveQueue::new();
        let reliable = RemoteInputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        };
        assert_eq!(
            queue.push_inputs(&vec![reliable; LIVE_QUEUE_CAPACITY]),
            LiveEnqueueResult::Queued
        );
        assert_eq!(
            queue.push_pointer(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 10, y: 20 },
                    delta_x: 1,
                    delta_y: 0,
                },
                remote_position: None,
            }),
            LiveEnqueueResult::MouseMoveBypassed
        );
        assert_eq!(queue.bypassed_moves.load(Ordering::Relaxed), 1);
        assert_eq!(queue.peak_depth(), LIVE_QUEUE_CAPACITY);
    }

    #[test]
    fn bypassed_remote_mouse_move_keeps_live_control_running() {
        let context = test_context();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 100, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let cutoff = context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .entry_raw_time_cutoff
            .unwrap();
        let reliable = RemoteInputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        };
        assert_eq!(
            context
                .queue
                .push_inputs(&vec![reliable; LIVE_QUEUE_CAPACITY]),
            LiveEnqueueResult::Queued
        );

        context.handle_raw_mouse(1, 0, cutoff.wrapping_add(1));
        assert_eq!(
            LiveStopCode::load(context.stop.load(Ordering::Acquire)),
            LiveStopCode::Running
        );
        assert!(context.failure.lock().unwrap().is_none());
        assert_eq!(context.queue.bypassed_moves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn raw_pointer_packets_accumulate_without_moving_the_hidden_source_cursor() {
        let context = test_context();
        let initial_remote = PhysicalPoint { x: 100, y: 700 };
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                initial_remote,
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let cutoff = context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .entry_raw_time_cutoff
            .unwrap();

        // The low-level hook now only suppresses legacy cursor motion. Its absolute screen point
        // must never be interpreted as a relative packet or forwarded.
        assert!(context.handle_input(InputEvent::MouseMove { x: 1_700, y: 240 }));
        assert_eq!(context.queue.pop_until(Instant::now()), None);

        context.handle_raw_mouse(10, 0, cutoff.wrapping_add(1));
        context.handle_raw_mouse(10, 0, cutoff.wrapping_add(2));

        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 120, y: 700 },
                    delta_x: 10,
                    delta_y: 0,
                },
                ..
            })
        ));
        assert_eq!(
            context
                .remote_pointer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .position,
            PhysicalPoint { x: 120, y: 700 }
        );
    }

    #[test]
    fn raw_motion_during_source_preparation_is_not_lost() {
        let context = test_context();
        context.begin_pending_enter_capture_at(500, ScreenBoundary::Top);
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();
        assert_eq!(
            LiveRoute::load(context.route.load(Ordering::Acquire)),
            LiveRoute::EnteringPeer
        );

        context.handle_raw_mouse(0, -75, 501);
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 1_120, y: 1_324 },
                    delta_x: 0,
                    delta_y: -75,
                },
                ..
            })
        ));
        context.activate_controlling_peer().unwrap();
        assert_eq!(
            LiveRoute::load(context.route.load(Ordering::Acquire)),
            LiveRoute::ControllingPeer
        );
    }

    fn raw_position_after_packets(packets: &[(i32, i32, u32)]) -> PhysicalPoint {
        let context = test_context();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 100, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let cutoff = context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .entry_raw_time_cutoff
            .unwrap();
        for &(delta_x, delta_y, elapsed_ms) in packets {
            context.handle_raw_mouse(delta_x, delta_y, cutoff.wrapping_add(elapsed_ms));
        }
        context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .position
    }

    #[test]
    fn raw_pointer_result_is_independent_of_mouse_report_rate_and_message_spacing() {
        let one_high_speed_packet = [(800, -320, 1)];
        let packets_125_hz = (0..8)
            .map(|index| (100, -40, 1 + index * 8))
            .collect::<Vec<_>>();
        let packets_500_hz = (0..40)
            .map(|index| (20, -8, 1 + index * 2))
            .collect::<Vec<_>>();
        let packets_1000_hz = (0..80).map(|index| (10, -4, 1 + index)).collect::<Vec<_>>();
        let expected = PhysicalPoint { x: 900, y: 380 };

        assert_eq!(raw_position_after_packets(&one_high_speed_packet), expected);
        assert_eq!(raw_position_after_packets(&packets_125_hz), expected);
        assert_eq!(raw_position_after_packets(&packets_500_hz), expected);
        assert_eq!(raw_position_after_packets(&packets_1000_hz), expected);
    }

    #[test]
    fn fast_motion_during_enter_ack_is_carried_into_the_peer_screen() {
        let context = test_context();
        context.begin_pending_enter_capture_at(1_000, ScreenBoundary::Top);
        context.handle_raw_mouse(12, -180, 1_001);
        context.handle_raw_mouse(8, -40, 1_002);

        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();

        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 1_140, y: 1_179 },
                    delta_x: 20,
                    delta_y: -220,
                },
                ..
            })
        ));
        assert_eq!(
            context
                .remote_pointer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .position,
            PhysicalPoint { x: 1_140, y: 1_179 }
        );
        assert_eq!(
            context.enter_carry_trace(),
            "carry_packets=2,carry_raw=20,-220 carry_pixels=20,-220 display_hz=60"
        );
    }

    #[test]
    fn boundary_packet_carries_only_the_unconsumed_outward_pixels() {
        assert_eq!(
            edge_overflow_pixels(-30, -10, true, POINTER_GAIN_SCALE.cast_unsigned()),
            -20
        );
        assert_eq!(
            edge_overflow_pixels(30, 10, true, POINTER_GAIN_SCALE.cast_unsigned()),
            20
        );
        assert_eq!(
            edge_overflow_pixels(-30, -30, true, POINTER_GAIN_SCALE.cast_unsigned()),
            0
        );
        assert_eq!(
            edge_overflow_pixels(-30, -10, false, POINTER_GAIN_SCALE.cast_unsigned()),
            0
        );
    }

    #[test]
    fn delayed_boundary_raw_packet_is_applied_once_after_enter_ack() {
        let context = test_context();
        context
            .pointer_gain
            .lock()
            .unwrap()
            .record_processed(ProcessedPointerSample {
                message_time_ms: 100,
                delta_x: 0,
                delta_y: -10,
                x_clamped: false,
                y_clamped: true,
            });
        context.begin_pending_enter_capture_at(100, ScreenBoundary::Top);

        // WM_INPUT for the edge-causing packet can be dispatched after the network thread has
        // already created the EnterRequest. Its timestamp equals the cutoff, so the full packet
        // is not accumulated; only the -20 pixels Windows could not consume before clamping are.
        context.handle_raw_mouse(0, -30, 100);
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();

        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 1_120, y: 1_379 },
                    delta_x: 0,
                    delta_y: -20,
                },
                ..
            })
        ));
        assert_eq!(
            context.enter_carry_trace(),
            "carry_packets=0,carry_raw=0,0 carry_pixels=0,-20 display_hz=60"
        );
    }

    #[test]
    fn pending_entry_uses_only_fresh_overflow_from_the_requested_boundary() {
        let mut pending = PendingEnterMotion::default();
        pending.begin(1_000, ScreenBoundary::Top);
        pending.apply_edge_overflow(EdgeOverflowCandidate {
            message_time_ms: 999,
            pixel_x: -80,
            pixel_y: -30,
        });
        let gain = PointerGainModel::default();
        assert_eq!(pending.scale(gain, &mut 0, &mut 0), (0, -30));

        pending.begin(1_000, ScreenBoundary::Top);
        pending.apply_edge_overflow(EdgeOverflowCandidate {
            message_time_ms: 899,
            pixel_x: 0,
            pixel_y: -90,
        });
        assert_eq!(pending.scale(gain, &mut 0, &mut 0), (0, 0));

        pending.begin(1_000, ScreenBoundary::Bottom);
        pending.apply_edge_overflow(EdgeOverflowCandidate {
            message_time_ms: 999,
            pixel_x: 0,
            pixel_y: -90,
        });
        assert_eq!(pending.scale(gain, &mut 0, &mut 0), (0, 0));

        let direction_cases = [
            (ScreenBoundary::Left, -25, 0, (-25, 0)),
            (ScreenBoundary::Right, 25, 0, (25, 0)),
            (ScreenBoundary::Top, 0, -25, (0, -25)),
            (ScreenBoundary::Bottom, 0, 25, (0, 25)),
        ];
        for (boundary, pixel_x, pixel_y, expected) in direction_cases {
            pending.begin(1_000, boundary);
            pending.apply_edge_overflow(EdgeOverflowCandidate {
                message_time_ms: 999,
                pixel_x,
                pixel_y,
            });
            assert_eq!(pending.scale(gain, &mut 0, &mut 0), expected);
        }
    }

    fn pending_entry_position(packets: &[(i32, i32, u32)]) -> PhysicalPoint {
        let context = test_context();
        context.begin_pending_enter_capture_at(10_000, ScreenBoundary::Top);
        for &(delta_x, delta_y, message_time_ms) in packets {
            context.handle_raw_mouse(delta_x, delta_y, message_time_ms);
        }
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .position
    }

    #[test]
    fn pending_enter_carry_is_independent_of_report_rate_and_ack_spacing() {
        let one_fast_packet = [(120, -400, 10_001)];
        let packets_125_hz = (0..8)
            .map(|index| (15, -50, 10_001 + index * 8))
            .collect::<Vec<_>>();
        let packets_500_hz = (0..40)
            .map(|index| (3, -10, 10_001 + index * 2))
            .collect::<Vec<_>>();
        let packets_1000_hz = (0..80)
            .map(|index| (if index % 2 == 0 { 2 } else { 1 }, -5, 10_001 + index))
            .collect::<Vec<_>>();
        let expected = PhysicalPoint { x: 1_240, y: 999 };

        assert_eq!(pending_entry_position(&one_fast_packet), expected);
        assert_eq!(pending_entry_position(&packets_125_hz), expected);
        assert_eq!(pending_entry_position(&packets_500_hz), expected);
        assert_eq!(pending_entry_position(&packets_1000_hz), expected);
    }

    #[test]
    fn pending_enter_capture_ignores_pre_request_input_and_handles_tick_rollover() {
        let context = test_context();
        context.begin_pending_enter_capture_at(u32::MAX, ScreenBoundary::Top);
        context.handle_raw_mouse(50, -50, u32::MAX);
        context.handle_raw_mouse(20, -30, 0);
        let pending = context.pending_enter_motion.lock().unwrap().take();

        assert_eq!(pending.packets, 1);
        assert_eq!(pending.raw_totals(), (20, -30));
    }

    #[test]
    fn reversing_during_enter_ack_cancels_the_carried_distance() {
        let context = test_context();
        context.begin_pending_enter_capture_at(100, ScreenBoundary::Top);
        context.handle_raw_mouse(0, -80, 101);
        context.handle_raw_mouse(0, 80, 102);
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();

        assert_eq!(context.queue.pop_until(Instant::now()), None);
        assert_eq!(
            context
                .remote_pointer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .position,
            PhysicalPoint { x: 1_120, y: 1_399 }
        );
    }

    #[test]
    fn rejected_or_cancelled_enter_discards_pending_motion() {
        let context = test_context();
        context.begin_pending_enter_capture_at(100, ScreenBoundary::Top);
        context.handle_raw_mouse(0, -300, 101);
        context.cancel_pending_enter_capture();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();

        assert_eq!(context.queue.pop_until(Instant::now()), None);
        assert_eq!(
            context
                .remote_pointer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .position,
            PhysicalPoint { x: 1_120, y: 1_399 }
        );
    }

    #[test]
    fn pending_enter_accumulator_is_fixed_size_and_saturating() {
        let mut pending = PendingEnterMotion::default();
        pending.begin(100, ScreenBoundary::Top);
        for timestamp in 101..201 {
            pending.record(i32::MAX, i32::MIN, timestamp);
        }
        assert_eq!(
            pending.raw_totals(),
            (PENDING_ENTER_RAW_AXIS_LIMIT, -PENDING_ENTER_RAW_AXIS_LIMIT)
        );
        assert_eq!(pending.packets, 100);
    }

    #[test]
    fn pointer_gain_learns_windows_processed_speed_without_boundary_clamping() {
        let mut calibration = PointerGainCalibration::default();
        for sequence in 0..8_u32 {
            let message_time_ms = 1_000 + sequence * 4;
            calibration.record_processed(ProcessedPointerSample {
                message_time_ms,
                delta_x: 20,
                delta_y: if sequence % 2 == 0 { 0 } else { -1 },
                x_clamped: false,
                y_clamped: false,
            });
            let _ = calibration.record_raw_with_match(10, -1, message_time_ms + 1);
        }
        // X learned 2x acceleration. Y includes zero-pixel packets and retains their fractional
        // contribution instead of incorrectly rounding every raw count to a pixel.
        let model = calibration.model();
        assert_eq!(model.x, [2_048; POINTER_GAIN_BIN_COUNT]);
        assert_eq!(model.y, [512; POINTER_GAIN_BIN_COUNT]);
        let mut residual = 0;
        assert_eq!(model.scale_x(10, &mut residual), 20);
        assert_eq!(residual, 0);
        assert_eq!(model.scale_y(-1, &mut residual), 0);
        assert_eq!(model.scale_y(-1, &mut residual), -1);
    }

    #[test]
    fn boundary_clamped_samples_cannot_corrupt_learned_pointer_gain() {
        let mut calibration = PointerGainCalibration::default();
        for sequence in 0..8_u32 {
            calibration.record_processed(ProcessedPointerSample {
                message_time_ms: sequence,
                delta_x: 1,
                delta_y: 0,
                x_clamped: true,
                y_clamped: false,
            });
            let _ = calibration.record_raw_with_match(100, 0, sequence);
        }
        assert_eq!(
            calibration.model().x,
            [POINTER_GAIN_SCALE.cast_unsigned(); POINTER_GAIN_BIN_COUNT]
        );
    }

    #[test]
    fn continuing_fast_motion_after_entry_moves_inward_not_to_the_return_edge() {
        let context = test_context();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 1_120, y: 1_399 },
                PhysicalPoint { x: 960, y: 0 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let cutoff = context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .entry_raw_time_cutoff
            .unwrap();

        // A very fast upward movement continues away from the peer's bottom return boundary.
        context.handle_raw_mouse(240, -900, cutoff.wrapping_add(1));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 1_360, y: 499 },
                    delta_x: 240,
                    delta_y: -900,
                },
                ..
            })
        ));
    }

    #[test]
    fn entry_capture_drops_queued_boundary_packets_before_forwarding_fresh_motion() {
        let context = test_context();
        let initial_remote = PhysicalPoint { x: 100, y: 700 };
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                initial_remote,
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let cutoff = context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .entry_raw_time_cutoff
            .unwrap();

        // A raw packet timestamped before source capture completed is stale.
        context.handle_raw_mouse(10, 0, cutoff);
        assert_eq!(context.queue.pop_until(Instant::now()), None);

        // The first post-capture packet arms normal 1:1 forwarding without imposing a velocity
        // threshold; an extreme but fresh physical movement must never require slowing down.
        context.handle_raw_mouse(700, 0, cutoff.wrapping_add(1));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    position: PhysicalPoint { x: 800, y: 700 },
                    delta_x: 700,
                    delta_y: 0,
                },
                ..
            })
        ));
        assert!(
            context
                .remote_pointer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .entry_raw_time_cutoff
                .is_none()
        );
    }

    #[test]
    fn input_tick_ordering_handles_equal_values_and_u32_rollover() {
        assert!(!tick_is_strictly_after(100, 100));
        assert!(!tick_is_strictly_after(99, 100));
        assert!(tick_is_strictly_after(101, 100));
        assert!(tick_is_strictly_after(0, u32::MAX));
        assert!(!tick_is_strictly_after(u32::MAX, 0));
    }

    #[test]
    fn transient_remote_pointer_lock_contention_suppresses_one_move_without_failing() {
        let context = test_context();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 100, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let cutoff = context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .entry_raw_time_cutoff
            .unwrap();
        let held = context.remote_pointer.lock().unwrap();

        context.handle_raw_mouse(4, 0, cutoff.wrapping_add(1));
        assert_eq!(
            LiveStopCode::load(context.stop.load(Ordering::Acquire)),
            LiveStopCode::Running
        );
        assert!(context.failure.lock().unwrap().is_none());
        assert_eq!(context.queue.pop_until(Instant::now()), None);
        drop(held);
    }

    #[test]
    fn hook_suppression_starts_only_after_control_and_force_local_clears_everything() {
        let context = test_context();
        assert!(!context.handle_input(InputEvent::MouseMove { x: 101, y: 100 }));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                remote_position: None,
                ..
            })
        ));

        let resume = PhysicalPoint { x: 1_919, y: 540 };
        let releases = context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                resume,
            )
            .unwrap();
        assert!(releases.is_empty());
        context.activate_controlling_peer().unwrap();
        let cutoff = context
            .remote_pointer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .entry_raw_time_cutoff
            .unwrap();
        assert!(context.handle_input(InputEvent::MouseMove { x: 1_700, y: 500 }));
        assert_eq!(context.queue.pop_until(Instant::now()), None);
        context.handle_raw_mouse(12, -4, cutoff.wrapping_add(1));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                remote_position: Some(_),
                ..
            })
        ));
        assert!(context.handle_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        }));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(RemoteInputEvent::MouseButton { .. }))
        ));
        let inset_resume = PhysicalPoint { x: 1_918, y: 540 };
        assert_eq!(context.force_local(), Some(inset_resume));
        assert_eq!(
            LiveRoute::load(context.route.load(Ordering::Acquire)),
            LiveRoute::Local
        );
        assert_eq!(*context.previous_x.lock().unwrap(), 1_918);
        assert_eq!(*context.previous_y.lock().unwrap(), 540);
        assert!(!context.local_pointer_needs_baseline.load(Ordering::Acquire));
        assert_eq!(context.active_epoch.load(Ordering::Acquire), 0);
        assert_eq!(context.forwarded.lock().unwrap().outstanding_count(), 0);
        assert!(!context.handle_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Up,
        }));
    }

    #[test]
    fn force_local_without_saved_source_position_rebaselines_first_physical_move() {
        let context = test_context();
        context.set_controlled_by_peer();
        assert_eq!(context.force_local(), None);
        assert!(context.local_pointer_needs_baseline.load(Ordering::Acquire));

        assert!(!context.handle_input(InputEvent::MouseMove { x: 900, y: 700 }));
        assert_eq!(*context.previous_x.lock().unwrap(), 900);
        assert_eq!(*context.previous_y.lock().unwrap(), 700);
        assert_eq!(context.queue.pop_until(Instant::now()), None);

        assert!(!context.handle_input(InputEvent::MouseMove { x: 900, y: 699 }));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer {
                sample: PointerSample {
                    delta_x: 0,
                    delta_y: -1,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn controlled_target_mouse_noise_is_suppressed_until_movement_is_intentional() {
        let context = test_context();
        context.set_controlled_by_peer();

        assert!(context.handle_input(InputEvent::MouseMove { x: 101, y: 100 }));
        assert!(!context.local_takeover_pending.load(Ordering::Acquire));
        assert_eq!(context.queue.pop_until(Instant::now()), None);
        assert!(context.handle_input(InputEvent::MouseMove { x: 109, y: 100 }));
        assert!(!context.local_takeover_pending.load(Ordering::Acquire));
        assert!(context.handle_input(InputEvent::MouseMove { x: 125, y: 100 }));
        assert!(context.local_takeover_pending.load(Ordering::Acquire));
        assert_eq!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::LocalTakeover)
        );

        context.force_local();
        assert!(!context.local_takeover_pending.load(Ordering::Acquire));
        assert_eq!(
            LiveRoute::load(context.route.load(Ordering::Acquire)),
            LiveRoute::Local
        );
    }

    #[test]
    fn controlled_target_repeated_stationary_jitter_never_steals_remote_control() {
        let mut motion = LocalTakeoverMotion::default();
        let started = Instant::now();

        for sample in 0..32_u64 {
            let offset = i32::try_from(sample % 3).unwrap() - 1;
            assert!(!motion.observe(
                100 + offset,
                100 - offset,
                started + Duration::from_millis(sample * 4)
            ));
        }
        assert!(motion.distance <= 4);
    }

    #[test]
    fn controlled_target_decisive_input_requests_immediate_local_takeover() {
        let context = test_context();
        context.set_controlled_by_peer();

        assert!(!context.handle_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        }));
        assert_eq!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::LocalTakeover)
        );
    }

    #[test]
    fn controlled_target_motion_debounce_expires_between_isolated_samples() {
        let mut motion = LocalTakeoverMotion::default();
        let started = Instant::now();
        assert!(!motion.observe(100, 100, started));
        assert!(!motion.observe(
            200,
            200,
            started + LOCAL_TAKEOVER_MOVE_WINDOW + Duration::from_millis(1)
        ));
        assert_eq!(motion.samples, 1);
        assert_eq!(motion.distance, 0);
    }

    #[test]
    fn controlled_by_peer_transition_preserves_takeover_after_a_stale_pointer() {
        let context = test_context();
        assert!(!context.handle_input(InputEvent::MouseMove { x: 101, y: 100 }));
        context.set_controlled_by_peer();
        assert!(!context.handle_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        }));

        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Pointer { .. })
        ));
        assert_eq!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::LocalTakeover)
        );
        assert_eq!(
            LiveRoute::load(context.route.load(Ordering::Acquire)),
            LiveRoute::ControlledByPeer
        );
    }

    #[test]
    fn emergency_chord_is_consumed_locally_even_before_remote_control() {
        let context = test_context();
        let key = |virtual_key, scan_code| InputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition: KeyTransition::Down,
        };
        assert!(!context.handle_input(key(VK_LCONTROL_VALUE, 0x1d)));
        assert!(!context.handle_input(key(VK_LMENU_VALUE, 0x38)));
        assert!(!context.handle_input(key(VK_LSHIFT_VALUE, 0x2a)));
        assert!(context.handle_input(key(VK_ESCAPE_VALUE, 0x01)));
        assert_eq!(
            LiveStopCode::load(context.stop.load(Ordering::Acquire)),
            LiveStopCode::EmergencyShortcut
        );
    }

    #[test]
    fn remote_emergency_modifiers_are_never_queued_or_injected() {
        let context = test_context();
        let releases = context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        assert!(releases.is_empty());
        context.activate_controlling_peer().unwrap();
        let key = |virtual_key, scan_code| InputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition: KeyTransition::Down,
        };
        for event in [
            key(VK_LCONTROL_VALUE, 0x1d),
            key(VK_LMENU_VALUE, 0x38),
            key(VK_LSHIFT_VALUE, 0x2a),
        ] {
            assert!(context.handle_input(event));
            assert_eq!(context.queue.pop_until(Instant::now()), None);
        }
        assert!(context.handle_input(key(VK_ESCAPE_VALUE, 0x01)));
        assert_eq!(
            LiveStopCode::load(context.stop.load(Ordering::Acquire)),
            LiveStopCode::EmergencyShortcut
        );
        assert_eq!(context.queue.pop_until(Instant::now()), None);
        let mut forwarded = context.forwarded.lock().unwrap();
        assert_eq!(forwarded.stats().key_downs, 0);
        assert!(forwarded.release_all().is_empty());
        assert_eq!(forwarded.outstanding_count(), 0);
    }

    #[test]
    fn deferred_modifier_preserves_normal_remote_shortcut_order() {
        let context = test_context();
        let releases = context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        assert!(releases.is_empty());
        context.activate_controlling_peer().unwrap();
        let key = |virtual_key, scan_code, transition| InputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition,
        };
        let control_down = key(VK_LCONTROL_VALUE, 0x1d, KeyTransition::Down);
        let a_down = key(0x41, 0x1e, KeyTransition::Down);
        let a_up = key(0x41, 0x1e, KeyTransition::Up);
        let control_up = key(VK_LCONTROL_VALUE, 0x1d, KeyTransition::Up);

        assert!(context.handle_input(control_down));
        assert_eq!(context.queue.pop_until(Instant::now()), None);
        assert!(context.handle_input(a_down));
        for expected in [control_down, a_down] {
            assert_eq!(
                context.queue.pop_until(Instant::now()),
                Some(LiveSignal::Input(
                    remote_event_from_input(expected).unwrap()
                ))
            );
        }
        assert!(context.handle_input(a_up));
        assert_eq!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(remote_event_from_input(a_up).unwrap()))
        );
        assert!(context.handle_input(control_up));
        assert_eq!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(
                remote_event_from_input(control_up).unwrap()
            ))
        );
        assert_eq!(context.forwarded.lock().unwrap().outstanding_count(), 0);
    }

    #[test]
    fn first_paste_fence_blocks_stale_v_down_and_its_matching_release() {
        let key = |virtual_key, scan_code, transition| RemoteInputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition,
        };
        let control_down = key(VK_LCONTROL_VALUE, 0x1d, KeyTransition::Down);
        let control_up = key(VK_LCONTROL_VALUE, 0x1d, KeyTransition::Up);
        let v_down = key(VK_V_VALUE, 0x2f, KeyTransition::Down);
        let v_up = key(VK_V_VALUE, 0x2f, KeyTransition::Up);
        let mut state = RemoteShortcutState::default();

        assert!(state.should_send(control_down, Some(7), || {
            crate::clipboard::paste_fence::PasteFenceDecision::Block
        }));
        assert!(!state.should_send(v_down, Some(7), || {
            crate::clipboard::paste_fence::PasteFenceDecision::Block
        }));
        assert!(!state.should_send(v_down, Some(7), || {
            crate::clipboard::paste_fence::PasteFenceDecision::Send
        }));
        assert!(!state.should_send(v_up, Some(7), || {
            crate::clipboard::paste_fence::PasteFenceDecision::Send
        }));
        assert!(state.should_send(control_up, Some(7), || {
            crate::clipboard::paste_fence::PasteFenceDecision::Block
        }));

        // A new control epoch cannot inherit a held Control key from the previous session.
        assert!(state.should_send(v_down, Some(8), || {
            crate::clipboard::paste_fence::PasteFenceDecision::Block
        }));
    }

    #[test]
    fn ready_first_paste_preserves_the_complete_control_v_sequence() {
        let key = |virtual_key, scan_code, transition| RemoteInputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition,
        };
        let mut state = RemoteShortcutState::default();
        assert!(state.should_send(
            key(VK_RCONTROL_VALUE, 0x1d, KeyTransition::Down),
            Some(9),
            || crate::clipboard::paste_fence::PasteFenceDecision::Send,
        ));
        assert!(
            state.should_send(key(VK_V_VALUE, 0x2f, KeyTransition::Down), Some(9), || {
                crate::clipboard::paste_fence::PasteFenceDecision::Send
            },)
        );
        assert!(
            state.should_send(key(VK_V_VALUE, 0x2f, KeyTransition::Up), Some(9), || {
                crate::clipboard::paste_fence::PasteFenceDecision::Block
            },)
        );
        assert!(state.should_send(
            key(VK_RCONTROL_VALUE, 0x1d, KeyTransition::Up),
            Some(9),
            || crate::clipboard::paste_fence::PasteFenceDecision::Block,
        ));
    }

    #[test]
    fn right_shift_is_forwarded_before_letters_and_released_after_them() {
        let mut state = ForwardingState::default();
        let shift = InputEvent::Key {
            physical: PhysicalKey {
                scan_code: 0x36,
                extended: false,
            },
            virtual_key: VK_RSHIFT_VALUE,
            transition: KeyTransition::Down,
        };
        let letter = InputEvent::Key {
            physical: PhysicalKey {
                scan_code: 0x1e,
                extended: false,
            },
            virtual_key: 0x41,
            transition: KeyTransition::Down,
        };
        assert!(state.prepare(shift).unwrap().events.is_empty());
        assert_eq!(
            state.prepare(letter).unwrap().events,
            vec![
                remote_event_from_input(shift).unwrap(),
                remote_event_from_input(letter).unwrap()
            ]
        );
        let releases = state.release_all();
        assert!(matches!(
            releases[0],
            InputEvent::Key {
                virtual_key: 0x41,
                transition: KeyTransition::Up,
                ..
            }
        ));
        assert!(matches!(
            releases[1],
            InputEvent::Key {
                virtual_key: VK_RSHIFT_VALUE,
                transition: KeyTransition::Up,
                ..
            }
        ));
        assert_eq!(state.outstanding_count(), 0);
    }

    #[test]
    fn numlock_is_forwarded_but_not_suppressed_on_the_physical_keyboard() {
        let context = test_context();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        for transition in [KeyTransition::Down, KeyTransition::Up] {
            let event = InputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 0x45,
                    extended: true,
                },
                virtual_key: VK_NUMLOCK_VALUE,
                transition,
            };
            assert!(!context.handle_input(event));
            assert_eq!(
                context.queue.pop_until(Instant::now()),
                Some(LiveSignal::Input(remote_event_from_input(event).unwrap()))
            );
        }
        assert_eq!(context.forwarded.lock().unwrap().outstanding_count(), 0);
    }

    #[test]
    fn returning_control_never_replays_a_locally_passed_numlock_press() {
        let mut state = ForwardingState::default();
        let num_lock = InputEvent::Key {
            physical: PhysicalKey {
                scan_code: 0x45,
                extended: true,
            },
            virtual_key: VK_NUMLOCK_VALUE,
            transition: KeyTransition::Down,
        };
        assert_eq!(state.prepare(num_lock).unwrap().events.len(), 1);
        assert!(state.take_local_restore_keys().is_empty());
        assert_eq!(state.outstanding_count(), 0);
    }

    #[test]
    fn releasing_one_deferred_modifier_flushes_every_press_in_original_order() {
        let mut state = ForwardingState::default();
        let key = |virtual_key, scan_code, transition| InputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition,
        };
        let control_down = key(VK_LCONTROL_VALUE, 0x1d, KeyTransition::Down);
        let alt_down = key(VK_LMENU_VALUE, 0x38, KeyTransition::Down);
        let control_up = key(VK_LCONTROL_VALUE, 0x1d, KeyTransition::Up);
        assert!(state.prepare(control_down).unwrap().events.is_empty());
        assert!(state.prepare(alt_down).unwrap().events.is_empty());
        assert_eq!(
            state.prepare(control_up).unwrap().events,
            [control_down, alt_down, control_up]
                .map(|event| remote_event_from_input(event).unwrap())
        );
        assert_eq!(state.outstanding_count(), 1);
    }

    #[test]
    fn held_mouse_button_crosses_as_paired_remote_drag_and_releases_during_entry() {
        let context = test_context();
        let left_down = InputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        };
        let left_up = InputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Up,
        };
        assert!(!context.handle_input(left_down));
        let local_releases = context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        assert_eq!(local_releases, vec![left_up]);
        assert_eq!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(
                remote_event_from_input(left_down).unwrap()
            ))
        );

        assert!(context.handle_input(left_up));
        assert_eq!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(remote_event_from_input(left_up).unwrap()))
        );
        context.activate_controlling_peer().unwrap();
        assert_eq!(
            context.inherited_remote_button_mask.load(Ordering::Acquire),
            0
        );
        assert_eq!(context.local_button_mask.load(Ordering::Acquire), 0);
        assert_eq!(context.inherited_button_downs.load(Ordering::Relaxed), 1);
        assert_eq!(context.inherited_button_ups.load(Ordering::Relaxed), 1);
        assert_eq!(
            context.force_local(),
            Some(PhysicalPoint { x: 1_918, y: 540 })
        );
    }

    #[test]
    fn every_held_mouse_button_is_inherited_and_released_in_reverse_order() {
        let context = test_context();
        let buttons = [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::X1,
            MouseButton::X2,
        ];
        for button in buttons {
            assert!(!context.handle_input(InputEvent::MouseButton {
                button,
                transition: KeyTransition::Down,
            }));
        }

        let local_releases = context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        assert_eq!(
            local_releases,
            buttons
                .into_iter()
                .rev()
                .map(|button| InputEvent::MouseButton {
                    button,
                    transition: KeyTransition::Up,
                })
                .collect::<Vec<_>>()
        );
        for button in buttons {
            assert_eq!(
                context.queue.pop_until(Instant::now()),
                Some(LiveSignal::Input(RemoteInputEvent::MouseButton {
                    button,
                    transition: KeyTransition::Down,
                }))
            );
        }

        for button in buttons.into_iter().rev() {
            assert!(context.handle_input(InputEvent::MouseButton {
                button,
                transition: KeyTransition::Up,
            }));
            assert_eq!(
                context.queue.pop_until(Instant::now()),
                Some(LiveSignal::Input(RemoteInputEvent::MouseButton {
                    button,
                    transition: KeyTransition::Up,
                }))
            );
        }
        assert_eq!(
            context.inherited_remote_button_mask.load(Ordering::Acquire),
            0
        );
        assert_eq!(context.inherited_button_downs.load(Ordering::Relaxed), 5);
        assert_eq!(context.inherited_button_ups.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn inherited_button_release_is_forwarded_at_most_once() {
        let context = test_context();
        let down = InputEvent::MouseButton {
            button: MouseButton::X2,
            transition: KeyTransition::Down,
        };
        let up = InputEvent::MouseButton {
            button: MouseButton::X2,
            transition: KeyTransition::Up,
        };
        assert!(!context.handle_input(down));
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(RemoteInputEvent::MouseButton {
                button: MouseButton::X2,
                transition: KeyTransition::Down,
            }))
        ));
        assert!(context.handle_input(up));
        assert!(!context.handle_input(up));
        assert!(matches!(
            context.queue.pop_until(Instant::now()),
            Some(LiveSignal::Input(RemoteInputEvent::MouseButton {
                button: MouseButton::X2,
                transition: KeyTransition::Up,
            }))
        ));
        assert_eq!(context.queue.pop_until(Instant::now()), None);
        assert_eq!(context.inherited_button_ups.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn newer_layout_wins_and_equal_revision_is_resolved_by_listener() {
        let local = LayoutConfiguration {
            listener_peer_placement: PeerPlacement::Right,
            revision: 10,
            corner_guard_enabled: true,
            corner_guard_pixels: 32,
            require_second_push: false,
        };
        let peer = LayoutConfiguration {
            listener_peer_placement: PeerPlacement::Below,
            revision: 11,
            corner_guard_enabled: true,
            corner_guard_pixels: 48,
            require_second_push: true,
        };
        assert_eq!(
            resolve_layout_configuration(local, peer, AutomaticChannelRole::Listener),
            peer
        );
        let equal_peer = LayoutConfiguration {
            revision: local.revision,
            ..peer
        };
        assert_eq!(
            resolve_layout_configuration(local, equal_peer, AutomaticChannelRole::Listener),
            local
        );
        assert_eq!(
            resolve_layout_configuration(local, equal_peer, AutomaticChannelRole::Connector),
            equal_peer
        );
    }

    #[test]
    fn resolved_layout_opens_reciprocal_edges_with_independent_local_guards() {
        let layout = LayoutConfiguration {
            listener_peer_placement: PeerPlacement::Above,
            revision: 1,
            corner_guard_enabled: false,
            corner_guard_pixels: 96,
            require_second_push: true,
        };
        let listener = live_policy(AutomaticChannelRole::Listener, layout, true, 512);
        let connector = live_policy(AutomaticChannelRole::Connector, layout, false, 24);
        assert_eq!(listener.peer_placement, PeerPlacement::Above);
        assert_eq!(connector.peer_placement, PeerPlacement::Below);
        assert_eq!(listener.corner_guard_pixels, 512);
        assert_eq!(connector.corner_guard_pixels, 0);
        assert!(listener.require_second_push);
        assert!(connector.require_second_push);
    }

    #[test]
    fn normal_return_restores_held_remote_keys_and_drag_locally_in_press_order() {
        let context = test_context();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let key = |virtual_key, scan_code| InputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition: KeyTransition::Down,
        };
        let control = key(VK_LCONTROL_VALUE, 0x1d);
        let a = key(0x41, 0x1e);
        let left = InputEvent::MouseButton {
            button: MouseButton::Left,
            transition: KeyTransition::Down,
        };
        assert!(context.handle_input(control));
        assert!(context.handle_input(a));
        assert!(context.handle_input(left));
        while context.queue.pop_until(Instant::now()).is_some() {}

        assert_eq!(
            context.take_local_restore_inputs().unwrap(),
            vec![control, a, left]
        );
        assert_eq!(context.forwarded.lock().unwrap().outstanding_count(), 0);
        assert_eq!(
            context.force_local(),
            Some(PhysicalPoint { x: 1_918, y: 540 })
        );
    }

    #[test]
    fn a_deferred_modifier_is_restored_locally_even_if_never_sent_remote() {
        let context = test_context();
        context
            .begin_entering_peer(
                77,
                PhysicalScreenSize::new(2_240, 1_400).unwrap(),
                PhysicalPoint { x: 0, y: 700 },
                PhysicalPoint { x: 1_919, y: 540 },
            )
            .unwrap();
        context.activate_controlling_peer().unwrap();
        let control = InputEvent::Key {
            physical: PhysicalKey {
                scan_code: 0x1d,
                extended: false,
            },
            virtual_key: VK_LCONTROL_VALUE,
            transition: KeyTransition::Down,
        };
        assert!(context.handle_input(control));
        assert_eq!(context.queue.pop_until(Instant::now()), None);
        assert_eq!(context.take_local_restore_inputs().unwrap(), vec![control]);
    }
}
