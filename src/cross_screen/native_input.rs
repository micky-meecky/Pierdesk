//! Connection-independent ownership of the single Win32 input hook and injector.
//! The workspace coordinator consumes events and chooses their peer; this owner
//! never opens a transport connection or creates a per-peer hook.
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DispatchMessageW, GetClipCursor, GetCursorInfo,
    GetForegroundWindow, GetWindowThreadProcessId, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    QS_ALLINPUT, TranslateMessage, WM_APP, WM_QUIT, WindowFromPoint,
};

use super::diagnostic::{
    ActiveContextGuard, HookGuards, InputHookHandler, RawMouseInputWindow, ScreenExtent,
    SendInputEndpoint, ThreadDpiAwarenessGuard,
};
use super::live::{
    CursorShapeCapture, SourceCursorConfinement, SourceCursorShieldWindow, SourceCursorVisibility,
};
use super::{InputEvent, InputStateMachine, PhysicalScreenSize, ScreenBoundary, StateDecision};

mod injection;
use injection::InjectionQueue;

const EVENT_CAPACITY: usize = 256;
const RELIABLE_EVENT_RESERVE: usize = 64;
const LOSSY_EVENT_LIMIT: usize = EVENT_CAPACITY - RELIABLE_EVENT_RESERVE;
const COMMAND_CAPACITY: usize = 16;
const MAX_BATCH: usize = 512;
const COMMAND_DEADLINE: Duration = Duration::from_millis(500);
const COMMAND_WAIT: Duration = Duration::from_secs(1);
const INJECTED_RAW_ECHO_GRACE: Duration = Duration::from_millis(40);
const SLOW_INJECTION_COMMAND: Duration = Duration::from_millis(40);
const SOURCE_CURSOR_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const WAKE: u32 = WM_APP + 0x74;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInputEvent {
    Physical(InputEvent),
    /// Captured while the hook actually suppressed delivery to the local OS.
    Suppressed(InputEvent),
    /// Absolute Windows pointer motion paired with its hook timestamp. Keeping the
    /// timestamp lets the workspace runtime correlate processed pixels with raw
    /// device units and preserve the user's configured Windows pointer speed.
    PointerMotion {
        x: i32,
        y: i32,
        message_time_ms: u32,
        suppressed: bool,
    },
    RawMotion {
        delta_x: i32,
        delta_y: i32,
        message_time_ms: u32,
    },
    EmergencyReturn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInputRecoveryReason {
    QueueContended,
    QueueOverflow,
    InjectionBlocked,
}

impl NativeInputRecoveryReason {
    const fn code(self) -> u32 {
        match self {
            Self::QueueContended => 1,
            Self::QueueOverflow => 2,
            Self::InjectionBlocked => 3,
        }
    }

    const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::QueueContended),
            2 => Some(Self::QueueOverflow),
            3 => Some(Self::InjectionBlocked),
            _ => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::QueueContended => "queue_contended",
            Self::QueueOverflow => "queue_overflow",
            Self::InjectionBlocked => "injection_blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeInputDiagnostics {
    pub peak_queued_events: u64,
    pub dropped_motion_events: u64,
    pub recovery_returns: u64,
    pub contention_returns: u64,
    pub overflow_returns: u64,
    pub target_raw_echo_filtered: u64,
    pub target_raw_after_grace: u64,
    pub target_raw_without_injection: u64,
    pub max_target_raw_after_grace_micros: u64,
    pub target_takeover_returns: u64,
    pub slow_injection_commands: u64,
    pub max_injection_command_micros: u64,
    pub injection_queue_peak: u64,
    pub coalesced_injection_moves: u64,
    pub dropped_injection_moves: u64,
    pub dropped_injection_wheels: u64,
    pub failed_injection_wheels: u64,
    pub source_cursor_visible_samples: u64,
    pub source_cursor_visible_after_repair: u64,
    pub source_cursor_probe_failures: u64,
    pub last_recovery: Option<NativeInputRecoveryReason>,
}

#[derive(Clone, Debug)]
pub struct NativeInputSnapshot {
    pub screen: PhysicalScreenSize,
    pub running: bool,
    pub forwarding: bool,
    pub target_capture: bool,
    pub queued_events: usize,
    pub diagnostics: NativeInputDiagnostics,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NativeInputShutdown {
    pub clean: bool,
    pub remaining_inputs: usize,
    pub injected_events: u64,
    pub last_error: Option<String>,
}

struct Shared {
    cursor_probes: Mutex<VecDeque<String>>,
    events: Mutex<VecDeque<NativeInputEvent>>,
    event_notifier: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    injected_raw_echo_deadline: Mutex<Option<Instant>>,
    injected_pointer_in_flight: AtomicBool,
    physical: Mutex<InputStateMachine>,
    stop: AtomicBool,
    faulted: AtomicBool,
    forwarding: AtomicBool,
    target_capture: AtomicBool,
    release_requested: AtomicBool,
    emergency: AtomicBool,
    generation: AtomicU64,
    thread_id: AtomicU32,
    queued_events: AtomicU64,
    peak_queued_events: AtomicU64,
    dropped_motion_events: AtomicU64,
    recovery_returns: AtomicU64,
    contention_returns: AtomicU64,
    overflow_returns: AtomicU64,
    target_raw_echo_filtered: AtomicU64,
    target_raw_after_grace: AtomicU64,
    target_raw_without_injection: AtomicU64,
    max_target_raw_after_grace_micros: AtomicU64,
    target_takeover_returns: AtomicU64,
    slow_injection_commands: AtomicU64,
    max_injection_command_micros: AtomicU64,
    injection_queue_peak: AtomicU64,
    coalesced_injection_moves: AtomicU64,
    dropped_injection_moves: AtomicU64,
    dropped_injection_wheels: AtomicU64,
    failed_injection_wheels: AtomicU64,
    source_cursor_visible_samples: AtomicU64,
    source_cursor_visible_after_repair: AtomicU64,
    source_cursor_probe_failures: AtomicU64,
    last_recovery: AtomicU32,
    failure: Mutex<Option<String>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            cursor_probes: Mutex::new(VecDeque::new()),
            events: Mutex::new(VecDeque::new()),
            event_notifier: Mutex::new(None),
            injected_raw_echo_deadline: Mutex::new(None),
            injected_pointer_in_flight: AtomicBool::new(false),
            physical: Mutex::new(InputStateMachine::default()),
            stop: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
            forwarding: AtomicBool::new(false),
            target_capture: AtomicBool::new(false),
            release_requested: AtomicBool::new(false),
            emergency: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            thread_id: AtomicU32::new(0),
            queued_events: AtomicU64::new(0),
            peak_queued_events: AtomicU64::new(0),
            dropped_motion_events: AtomicU64::new(0),
            recovery_returns: AtomicU64::new(0),
            contention_returns: AtomicU64::new(0),
            overflow_returns: AtomicU64::new(0),
            target_raw_echo_filtered: AtomicU64::new(0),
            target_raw_after_grace: AtomicU64::new(0),
            target_raw_without_injection: AtomicU64::new(0),
            max_target_raw_after_grace_micros: AtomicU64::new(0),
            target_takeover_returns: AtomicU64::new(0),
            slow_injection_commands: AtomicU64::new(0),
            max_injection_command_micros: AtomicU64::new(0),
            injection_queue_peak: AtomicU64::new(0),
            coalesced_injection_moves: AtomicU64::new(0),
            dropped_injection_moves: AtomicU64::new(0),
            dropped_injection_wheels: AtomicU64::new(0),
            failed_injection_wheels: AtomicU64::new(0),
            source_cursor_visible_samples: AtomicU64::new(0),
            source_cursor_visible_after_repair: AtomicU64::new(0),
            source_cursor_probe_failures: AtomicU64::new(0),
            last_recovery: AtomicU32::new(0),
            failure: Mutex::new(None),
        }
    }

    fn invalidate_injection(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.forwarding.store(false, Ordering::Release);
        self.target_capture.store(false, Ordering::Release);
        self.release_requested.store(true, Ordering::Release);
    }

    fn fail(&self, message: impl Into<String>) {
        self.faulted.store(true, Ordering::Release);
        if let Ok(mut failure) = self.failure.try_lock() {
            failure.get_or_insert(message.into());
        }
        self.invalidate_injection();
        self.stop.store(true, Ordering::Release);
    }

    fn error(&self) -> Option<String> {
        self.failure.lock().map_or_else(
            |_| Some("native failure record was poisoned".into()),
            |failure| {
                failure.clone().or_else(|| {
                    self.faulted
                        .load(Ordering::Acquire)
                        .then(|| "native input fault; hook returned to local pass-through".into())
                })
            },
        )
    }

    fn diagnostics(&self) -> NativeInputDiagnostics {
        NativeInputDiagnostics {
            peak_queued_events: self.peak_queued_events.load(Ordering::Relaxed),
            dropped_motion_events: self.dropped_motion_events.load(Ordering::Relaxed),
            recovery_returns: self.recovery_returns.load(Ordering::Relaxed),
            contention_returns: self.contention_returns.load(Ordering::Relaxed),
            overflow_returns: self.overflow_returns.load(Ordering::Relaxed),
            target_raw_echo_filtered: self.target_raw_echo_filtered.load(Ordering::Relaxed),
            target_raw_after_grace: self.target_raw_after_grace.load(Ordering::Relaxed),
            target_raw_without_injection: self.target_raw_without_injection.load(Ordering::Relaxed),
            max_target_raw_after_grace_micros: self
                .max_target_raw_after_grace_micros
                .load(Ordering::Relaxed),
            target_takeover_returns: self.target_takeover_returns.load(Ordering::Relaxed),
            slow_injection_commands: self.slow_injection_commands.load(Ordering::Relaxed),
            max_injection_command_micros: self.max_injection_command_micros.load(Ordering::Relaxed),
            injection_queue_peak: self.injection_queue_peak.load(Ordering::Relaxed),
            coalesced_injection_moves: self.coalesced_injection_moves.load(Ordering::Relaxed),
            dropped_injection_moves: self.dropped_injection_moves.load(Ordering::Relaxed),
            dropped_injection_wheels: self.dropped_injection_wheels.load(Ordering::Relaxed),
            failed_injection_wheels: self.failed_injection_wheels.load(Ordering::Relaxed),
            source_cursor_visible_samples: self
                .source_cursor_visible_samples
                .load(Ordering::Relaxed),
            source_cursor_visible_after_repair: self
                .source_cursor_visible_after_repair
                .load(Ordering::Relaxed),
            source_cursor_probe_failures: self.source_cursor_probe_failures.load(Ordering::Relaxed),
            last_recovery: NativeInputRecoveryReason::from_code(
                self.last_recovery.load(Ordering::Relaxed),
            ),
        }
    }

    fn recover_local(&self, reason: NativeInputRecoveryReason) {
        self.invalidate_injection();
        if !self.emergency.swap(true, Ordering::AcqRel) {
            self.recovery_returns.fetch_add(1, Ordering::Relaxed);
            match reason {
                NativeInputRecoveryReason::QueueContended => {
                    self.contention_returns.fetch_add(1, Ordering::Relaxed);
                }
                NativeInputRecoveryReason::QueueOverflow => {
                    self.overflow_returns.fetch_add(1, Ordering::Relaxed);
                }
                NativeInputRecoveryReason::InjectionBlocked => {}
            }
            self.last_recovery.store(reason.code(), Ordering::Relaxed);
        }
        self.notify_event();
        self.wake();
    }

    fn push(&self, event: NativeInputEvent) -> CaptureEnqueueResult {
        let lossy_motion = event.is_lossy_motion();
        let mut queue = match self.events.try_lock() {
            Ok(queue) => queue,
            Err(TryLockError::WouldBlock) if lossy_motion => {
                // The Win32 hook must never wait behind status snapshots, batch drains or
                // forwarding cleanup. A later pointer sample carries the current position (or
                // accumulates fresh raw motion), so dropping this one sample is recoverable.
                self.dropped_motion_events.fetch_add(1, Ordering::Relaxed);
                return CaptureEnqueueResult::DroppedMotion;
            }
            Err(TryLockError::WouldBlock) => {
                // A reliable transition may never be dropped while remote input remains active.
                // Return control locally and release the remote ledger instead of turning a
                // microsecond-scale queue collision into a permanent product failure.
                self.recover_local(NativeInputRecoveryReason::QueueContended);
                return CaptureEnqueueResult::RecoveredLocal;
            }
            Err(TryLockError::Poisoned(_)) => {
                self.fail("native capture queue was poisoned");
                return CaptureEnqueueResult::Fatal;
            }
        };
        if lossy_motion && NativeInputEvent::coalesce_trailing_motion(&mut queue, event) {
            drop(queue);
            self.notify_event();
            return CaptureEnqueueResult::Coalesced;
        }
        if lossy_motion && queue.len() >= LOSSY_EVENT_LIMIT {
            self.dropped_motion_events.fetch_add(1, Ordering::Relaxed);
            return CaptureEnqueueResult::DroppedMotion;
        }
        if queue.len() == EVENT_CAPACITY {
            drop(queue);
            self.recover_local(NativeInputRecoveryReason::QueueOverflow);
            return CaptureEnqueueResult::RecoveredLocal;
        }
        queue.push_back(event);
        let depth = u64::try_from(queue.len()).unwrap_or(u64::MAX);
        self.queued_events.store(depth, Ordering::Relaxed);
        self.peak_queued_events.fetch_max(depth, Ordering::Relaxed);
        drop(queue);
        self.notify_event();
        CaptureEnqueueResult::Queued
    }

    fn take_events(&self) -> io::Result<Vec<NativeInputEvent>> {
        if let Some(error) = self.error() {
            return Err(io::Error::other(error));
        }
        let emergency = self.emergency.swap(false, Ordering::AcqRel);
        let mut captured = VecDeque::with_capacity(EVENT_CAPACITY);
        {
            let mut queue = self
                .events
                .lock()
                .map_err(|_| io::Error::other("native event queue poisoned"))?;
            std::mem::swap(&mut *queue, &mut captured);
            self.queued_events.store(0, Ordering::Relaxed);
        }
        if emergency {
            return Ok(vec![NativeInputEvent::EmergencyReturn]);
        }
        Ok(captured.into())
    }

    fn notify_event(&self) {
        let notifier = self
            .event_notifier
            .try_lock()
            .ok()
            .and_then(|notifier| notifier.clone());
        if let Some(notifier) = notifier {
            notifier();
        }
    }

    fn wake(&self) {
        let thread = self.thread_id.load(Ordering::Acquire);
        if thread != 0 {
            // SAFETY: a pointer-free wake is posted to the owned hook thread. A
            // timed native wait also observes stop if the thread has just exited.
            let _ = unsafe { PostThreadMessageW(thread, WAKE, WPARAM(0), LPARAM(0)) };
        }
    }

    fn mark_injected_pointer(&self) {
        *self
            .injected_raw_echo_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Instant::now().checked_add(INJECTED_RAW_ECHO_GRACE);
    }

    fn injected_pointer_echo_is_recent(&self) -> bool {
        if !self.target_capture.load(Ordering::Acquire) {
            return false;
        }
        if self.injected_pointer_in_flight.load(Ordering::Acquire) {
            self.target_raw_echo_filtered
                .fetch_add(1, Ordering::Relaxed);
            return true;
        }
        let deadline = self
            .injected_raw_echo_deadline
            .try_lock()
            .ok()
            .and_then(|deadline| *deadline);
        let Some(deadline) = deadline else {
            self.target_raw_without_injection
                .fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let now = Instant::now();
        if now <= deadline {
            self.target_raw_echo_filtered
                .fetch_add(1, Ordering::Relaxed);
            return true;
        }
        self.target_raw_after_grace.fetch_add(1, Ordering::Relaxed);
        let age = INJECTED_RAW_ECHO_GRACE.saturating_add(now.duration_since(deadline));
        self.max_target_raw_after_grace_micros.fetch_max(
            u64::try_from(age.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        false
    }

    fn note_target_takeover(&self) {
        self.target_takeover_returns.fetch_add(1, Ordering::Relaxed);
    }

    fn observe_injection_command(&self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.max_injection_command_micros
            .fetch_max(micros, Ordering::Relaxed);
        if elapsed > SLOW_INJECTION_COMMAND {
            self.slow_injection_commands.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn clear_injected_pointer_echo(&self) {
        *self
            .injected_raw_echo_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureEnqueueResult {
    Queued,
    Coalesced,
    DroppedMotion,
    RecoveredLocal,
    Fatal,
}

impl NativeInputEvent {
    const fn is_lossy_motion(self) -> bool {
        matches!(
            self,
            Self::PointerMotion { .. }
                | Self::RawMotion { .. }
                | Self::Physical(InputEvent::MouseMove { .. })
                | Self::Suppressed(InputEvent::MouseMove { .. })
        )
    }

    fn coalesce_trailing_motion(queue: &mut VecDeque<Self>, next: Self) -> bool {
        queue
            .back_mut()
            .is_some_and(|previous| previous.coalesce(next))
    }

    fn coalesce(&mut self, next: Self) -> bool {
        match (self, next) {
            (
                Self::PointerMotion {
                    x,
                    y,
                    message_time_ms,
                    suppressed,
                },
                Self::PointerMotion {
                    x: next_x,
                    y: next_y,
                    message_time_ms: next_time,
                    suppressed: next_suppressed,
                },
            ) if *suppressed == next_suppressed => {
                *x = next_x;
                *y = next_y;
                *message_time_ms = next_time;
                true
            }
            (
                Self::RawMotion {
                    delta_x,
                    delta_y,
                    message_time_ms,
                },
                Self::RawMotion {
                    delta_x: next_x,
                    delta_y: next_y,
                    message_time_ms: next_time,
                },
            ) => {
                *delta_x = delta_x.saturating_add(next_x);
                *delta_y = delta_y.saturating_add(next_y);
                *message_time_ms = next_time;
                true
            }
            (
                Self::Physical(InputEvent::MouseMove { x, y }),
                Self::Physical(InputEvent::MouseMove {
                    x: next_x,
                    y: next_y,
                }),
            )
            | (
                Self::Suppressed(InputEvent::MouseMove { x, y }),
                Self::Suppressed(InputEvent::MouseMove {
                    x: next_x,
                    y: next_y,
                }),
            ) => {
                *x = next_x;
                *y = next_y;
                true
            }
            _ => false,
        }
    }
}

struct CaptureContext {
    shared: Arc<Shared>,
    screen: ScreenExtent,
}

impl InputHookHandler for CaptureContext {
    fn screen_contains(&self, x: i32, y: i32) -> bool {
        self.screen.contains(x, y)
    }

    fn handle_input(&self, event: InputEvent) -> bool {
        self.handle(event, None)
    }

    fn handle_mouse_input(&self, event: InputEvent, message_time_ms: u32) -> bool {
        self.handle(event, Some(message_time_ms))
    }

    fn handle_raw_mouse(&self, delta_x: i32, delta_y: i32, message_time_ms: u32) {
        // Some Windows input stacks do not preserve SendInput's extra-information tag in
        // WM_INPUT. Correlate raw packets with the target-side injection window as a second
        // barrier so our own remote cursor movement cannot be mistaken for physical takeover.
        if !self.shared.stop.load(Ordering::Acquire)
            && !self.shared.injected_pointer_echo_is_recent()
        {
            let _ = self.shared.push(NativeInputEvent::RawMotion {
                delta_x,
                delta_y,
                message_time_ms,
            });
        }
    }
}

impl CaptureContext {
    fn handle(&self, event: InputEvent, message_time_ms: Option<u32>) -> bool {
        if self.shared.stop.load(Ordering::Acquire) {
            return false;
        }
        let Ok(mut physical) = self.shared.physical.try_lock() else {
            self.shared
                .fail("native physical ledger could not be acquired without blocking");
            return false;
        };
        let decision = physical.handle(event);
        if decision == StateDecision::EmergencyReturn {
            *physical = InputStateMachine::default();
            drop(physical);
            self.shared.invalidate_injection();
            self.shared.emergency.store(true, Ordering::Release);
            self.shared.notify_event();
            return true;
        }
        drop(physical);
        if decision == StateDecision::CapacityExceeded {
            self.shared.fail("native physical key ledger overflowed");
            return false;
        }
        let suppressed = self.shared.forwarding.load(Ordering::Acquire)
            || self.shared.target_capture.load(Ordering::Acquire);
        let enqueued = match (event, message_time_ms) {
            (InputEvent::MouseMove { x, y }, Some(message_time_ms)) => {
                self.shared.push(NativeInputEvent::PointerMotion {
                    x,
                    y,
                    message_time_ms,
                    suppressed,
                })
            }
            _ if suppressed => self.shared.push(NativeInputEvent::Suppressed(event)),
            _ => self.shared.push(NativeInputEvent::Physical(event)),
        };
        suppressed
            && enqueued != CaptureEnqueueResult::RecoveredLocal
            && !self.shared.stop.load(Ordering::Acquire)
    }
}

enum Action {
    Forwarding(bool),
    TargetCapture(bool),
    Release,
}

struct Command {
    action: Action,
    generation: u64,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    reply: mpsc::SyncSender<io::Result<()>>,
}

impl Command {
    fn valid(&self, shared: &Shared) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && Instant::now() < self.deadline
            && self.generation == shared.generation.load(Ordering::Acquire)
            && !shared.stop.load(Ordering::Acquire)
    }
}

pub struct NativeInputRuntime {
    shared: Arc<Shared>,
    screen: PhysicalScreenSize,
    commands: mpsc::SyncSender<Command>,
    injections: Arc<InjectionQueue>,
    worker: Option<JoinHandle<io::Result<NativeInputShutdown>>>,
    shutdown: Option<NativeInputShutdown>,
}

impl NativeInputRuntime {
    pub(crate) fn drain_cursor_probes(&self) -> Vec<String> {
        self.shared
            .cursor_probes
            .lock()
            .map(|mut probes| probes.drain(..).collect())
            .unwrap_or_default()
    }
    /// Start one process-wide hook and message-only raw-input window. Capture
    /// starts in pass-through mode until the origin explicitly reserves a crossing.
    ///
    /// # Errors
    /// Returns native startup errors, including an already-owned hook registry.
    pub fn start() -> io::Result<Self> {
        let shared = Arc::new(Shared::new());
        let injections = Arc::new(InjectionQueue::new());
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (ready, startup) = mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let thread_injections = Arc::clone(&injections);
        let worker = std::thread::Builder::new()
            .name("pierdesk-workspace-input".into())
            .spawn(move || {
                #[cfg(not(test))]
                crate::traffic_control::promote_interactive_thread();
                let result = run(&thread_shared, &thread_injections, &receiver, &ready);
                if let Err(error) = &result {
                    thread_shared.fail(error.to_string());
                    let _ = ready.try_send(Err(io::Error::new(error.kind(), error.to_string())));
                }
                result
            })?;
        match startup.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(screen)) => Ok(Self {
                shared,
                screen,
                commands,
                injections,
                worker: Some(worker),
                shutdown: None,
            }),
            result => {
                shared.stop.store(true, Ordering::Release);
                shared.wake();
                let _ = worker.join();
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => io::Error::new(
                        io::ErrorKind::TimedOut,
                        "native input startup did not complete",
                    ),
                })
            }
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> NativeInputSnapshot {
        NativeInputSnapshot {
            screen: self.screen,
            running: !self.shared.stop.load(Ordering::Acquire)
                && self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()),
            forwarding: self.shared.forwarding.load(Ordering::Acquire),
            target_capture: self.shared.target_capture.load(Ordering::Acquire),
            queued_events: usize::try_from(self.shared.queued_events.load(Ordering::Relaxed))
                .unwrap_or(usize::MAX),
            diagnostics: self.shared.diagnostics(),
            last_error: self
                .shutdown
                .as_ref()
                .and_then(|shutdown| shutdown.last_error.clone())
                .or_else(|| self.shared.error()),
        }
    }

    /// Drain one bounded capture batch. Emergency return precedes queued inputs.
    ///
    /// # Errors
    /// A native fault requires the product owner to stop forwarding immediately.
    pub fn take_events(&self) -> io::Result<Vec<NativeInputEvent>> {
        self.shared.take_events()
    }

    /// Wake the serialized product pump as soon as native input is available.
    pub(crate) fn set_event_notifier(&self, notifier: Arc<dyn Fn() + Send + Sync>) {
        *self
            .shared
            .event_notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(notifier);
    }

    /// Await actual injection of a local replay or handoff batch. Ordinary remote
    /// input uses `enqueue_remote` so a slow Win32 call cannot block lease renewal.
    ///
    /// # Errors
    /// Rejects oversized, cancelled, stale or backpressured batches and native failures.
    pub fn inject(&self, events: &[InputEvent]) -> io::Result<()> {
        self.injections.inject(&self.shared, events)
    }

    /// Accept ordinary target input into the bounded, ordered native queue.
    /// Success handles the batch under the stateless drop/coalescing policy,
    /// not Win32 completion. Native failures surface
    /// through `take_events`; a wheel failure alone is diagnostic and non-fatal.
    ///
    /// # Errors
    /// Rejects stopped owners, oversized batches and reliable queue exhaustion.
    pub fn enqueue_remote(&self, events: &[InputEvent]) -> io::Result<()> {
        self.injections.enqueue(&self.shared, events)
    }

    /// Cancel old queued input and await release of the actual native ledger,
    /// including any press whose Win32 call was already in flight.
    ///
    /// # Errors
    /// An incomplete release never acknowledges a handoff or permits stale replay.
    pub fn injection_barrier(&self) -> io::Result<()> {
        self.injections.barrier(&self.shared)
    }

    /// Suppress origin input and protect its cursor during reservation and control.
    ///
    /// # Errors
    /// Returns confinement/window errors or a stopped owner.
    pub fn set_forwarding(&self, active: bool) -> io::Result<()> {
        self.request(Action::Forwarding(active))
    }

    /// Capture target-side physical takeover input without hiding or confining its cursor.
    ///
    /// # Errors
    /// Returns a stopped owner or an expired native command.
    pub fn set_target_capture(&self, active: bool) -> io::Result<()> {
        self.request(Action::TargetCapture(active))
    }

    /// Invalidate pending injections immediately, then await native key/button release.
    ///
    /// # Errors
    /// Returns incomplete native release or a stopped owner; failure never validates an old batch.
    pub fn release_all(&self) -> io::Result<()> {
        self.shared.invalidate_injection();
        self.shared.wake();
        let cursor = self.request(Action::Release);
        let injection = self.injection_barrier();
        cursor.and(injection)
    }

    pub(crate) fn note_target_takeover(&self) {
        self.shared.note_target_takeover();
    }

    /// Stop capture, release injected state and join the sole native owner.
    ///
    /// # Errors
    /// Reports worker failure or panic rather than claiming clean teardown.
    pub fn stop(&mut self) -> io::Result<NativeInputShutdown> {
        if let Some(shutdown) = &self.shutdown {
            return Ok(shutdown.clone());
        }
        self.shared.invalidate_injection();
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake();
        self.injections.wake();
        let worker = self
            .worker
            .take()
            .ok_or_else(|| io::Error::other("native input worker previously failed"))?;
        let shutdown = worker
            .join()
            .map_err(|_| io::Error::other("native input owner panicked"))??;
        self.shutdown = Some(shutdown.clone());
        Ok(shutdown)
    }

    fn request(&self, action: Action) -> io::Result<()> {
        if self.shared.stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native input owner stopped",
            ));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .try_send(Command {
                action,
                generation: self.shared.generation.load(Ordering::Acquire),
                deadline: Instant::now() + COMMAND_DEADLINE,
                cancelled: Arc::clone(&cancelled),
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "native command queue full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "native command queue closed")
                }
            })?;
        self.shared.wake();
        if let Ok(result) = response.recv_timeout(COMMAND_WAIT) {
            result
        } else {
            cancelled.store(true, Ordering::Release);
            self.shared.invalidate_injection();
            self.shared.wake();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native command was cancelled before a late completion",
            ))
        }
    }
}

impl Drop for NativeInputRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct NativeOwner {
    cursor_probe_budget: u8,
    confinement: SourceCursorConfinement,
    cursor: CursorShapeCapture,
    visibility: SourceCursorVisibility,
    shield: SourceCursorShieldWindow,
}

impl NativeOwner {
    fn repair_source_cursor(&mut self, shared: &Shared) {
        if shared.forwarding.load(Ordering::Acquire) {
            // Repair synchronously on the owner: never hide the target's cursor or
            // apply an asynchronously queued source repair after returning local.
            let before = observe_source_cursor(shared, false);
            self.cursor.hide();
            let after = observe_source_cursor(shared, true);
            if let Some(info) = after
                && self.cursor_probe_budget > 0
            {
                self.cursor_probe_budget -= 1;
                record_cursor_probe(shared, before, info);
            }
        }
    }

    fn forwarding(&mut self, shared: &Shared, active: bool) -> io::Result<()> {
        if active {
            self.cursor_probe_budget = 8;
            self.confinement.confine_current()?;
            if let Err(error) = self.shield.controller().set_active(true) {
                let _ = self.confinement.release();
                return Err(error);
            }
            if let Err(error) = self.visibility.hide() {
                self.visibility.restore();
                let _ = self.shield.controller().set_active(false);
                let _ = self.confinement.release();
                return Err(error);
            }
            self.cursor.hide();
            shared.forwarding.store(true, Ordering::Release);
        } else {
            shared.forwarding.store(false, Ordering::Release);
            // Samples from the confined source must not move the returning cursor.
            // Keep every reliable key/button transition captured across this barrier.
            let mut events = shared
                .events
                .lock()
                .map_err(|_| io::Error::other("native capture queue poisoned"))?;
            events.retain(|event| {
                !matches!(
                    event,
                    NativeInputEvent::RawMotion { .. } | NativeInputEvent::PointerMotion { .. }
                )
            });
            let depth = events.len();
            shared
                .queued_events
                .store(u64::try_from(depth).unwrap_or(u64::MAX), Ordering::Relaxed);
            drop(events);
            let confinement = self.confinement.release();
            let shield = self.shield.controller().set_active(false);
            self.cursor.restore();
            self.visibility.restore();
            confinement?;
            shield?;
        }
        Ok(())
    }

    fn release(&mut self, shared: &Shared) -> io::Result<()> {
        self.forwarding(shared, false)
    }

    fn execute(&mut self, shared: &Shared, command: &Command) -> io::Result<()> {
        if !command.valid(shared) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "stale or cancelled native command",
            ));
        }
        match &command.action {
            Action::Forwarding(active) => self.forwarding(shared, *active),
            Action::TargetCapture(active) => {
                shared.target_capture.store(*active, Ordering::Release);
                if !*active {
                    shared.clear_injected_pointer_echo();
                }
                Ok(())
            }
            Action::Release => self.release(shared),
        }
    }
}

fn run(
    shared: &Arc<Shared>,
    injections: &Arc<InjectionQueue>,
    commands: &mpsc::Receiver<Command>,
    ready: &mpsc::SyncSender<io::Result<PhysicalScreenSize>>,
) -> io::Result<NativeInputShutdown> {
    let _dpi = ThreadDpiAwarenessGuard::enter_per_monitor()?;
    let screen = ScreenExtent::system_metrics()?;
    if screen != ScreenExtent::current_primary_display_mode()? {
        return Err(io::Error::other(
            "native physical screen metrics disagree with display mode",
        ));
    }
    let dpi = ScreenExtent::effective_dpi()?;
    let mut message = MSG::default();
    // SAFETY: the no-remove peek creates this thread's queue before it is published.
    let _ = unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: this is the current native owner thread identifier.
    shared
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let context = Arc::new(CaptureContext {
        shared: Arc::clone(shared),
        screen,
    });
    let _context = ActiveContextGuard::register(context)?;
    let hooks = HookGuards::install()?;
    let raw = RawMouseInputWindow::create()?;
    let mut owner = NativeOwner {
        cursor_probe_budget: 0,
        confinement: SourceCursorConfinement::default(),
        cursor: CursorShapeCapture::default(),
        visibility: SourceCursorVisibility::default(),
        shield: SourceCursorShieldWindow::create(screen, ScreenBoundary::Left)?,
    };
    let physical = PhysicalScreenSize::new(
        u32::try_from(screen.width).map_err(io::Error::other)?,
        u32::try_from(screen.height).map_err(io::Error::other)?,
    )?;
    let injector = injection::start(Arc::clone(shared), Arc::clone(injections), move || {
        SendInputEndpoint::new(screen)
    })?;
    let _ = ready.send(Ok(physical));
    let mut next_display_check = Instant::now() + Duration::from_millis(250);
    let mut next_source_cursor_check = Instant::now();
    while !shared.stop.load(Ordering::Acquire) {
        // SAFETY: message is owned writable storage; dispatch stays on its owner thread.
        for _ in 0..64 {
            if !unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                shared.stop.store(true, Ordering::Release);
                break;
            }
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
        if shared.release_requested.swap(false, Ordering::AcqRel)
            && let Err(error) = owner.release(shared)
        {
            shared.fail(error.to_string());
        }
        for _ in 0..8 {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            let result = owner.execute(shared, &command);
            let _ = command.reply.send(result);
        }
        if Instant::now() >= next_source_cursor_check {
            next_source_cursor_check = Instant::now() + SOURCE_CURSOR_CHECK_INTERVAL;
            owner.repair_source_cursor(shared);
        }
        if Instant::now() >= next_display_check {
            if ScreenExtent::current_primary_display_mode().is_ok_and(|current| current != screen)
                || ScreenExtent::effective_dpi().is_ok_and(|current| current != dpi)
            {
                shared.fail("native display geometry changed; input owner must restart");
            }
            next_display_check = Instant::now() + Duration::from_millis(250);
        }
        // SAFETY: no kernel handles are supplied; posted commands and input messages
        // wake the owner, while the timeout bounds stop/display checks if posting fails.
        let wait =
            unsafe { MsgWaitForMultipleObjectsEx(None, 20, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        if wait.0 == u32::MAX {
            shared.fail(io::Error::last_os_error().to_string());
        }
    }
    shared.forwarding.store(false, Ordering::Release);
    drop(raw);
    drop(hooks);
    finish_native(shared, injections, owner, injector)
}

fn finish_native(
    shared: &Shared,
    injections: &InjectionQueue,
    mut owner: NativeOwner,
    injector: JoinHandle<NativeInputShutdown>,
) -> io::Result<NativeInputShutdown> {
    let mut release_error = None;
    for _ in 0..3 {
        match owner.release(shared) {
            Ok(()) => {
                release_error = None;
                break;
            }
            Err(error) => release_error = Some(error.to_string()),
        }
    }
    injections.wake();
    let injected = injector
        .join()
        .map_err(|_| io::Error::other("native injector panicked"))?;
    let last_error = release_error
        .or(injected.last_error)
        .or_else(|| shared.error());
    Ok(NativeInputShutdown {
        clean: injected.remaining_inputs == 0 && last_error.is_none(),
        remaining_inputs: injected.remaining_inputs,
        injected_events: injected.injected_events,
        last_error,
    })
}

fn observe_source_cursor(shared: &Shared, after_repair: bool) -> Option<CURSORINFO> {
    let mut info = CURSORINFO {
        cbSize: u32::try_from(std::mem::size_of::<CURSORINFO>()).unwrap(),
        ..CURSORINFO::default()
    };
    // SAFETY: info has the required size and writable storage. This is a read-only probe.
    if unsafe { GetCursorInfo(&raw mut info) }.is_err() {
        shared
            .source_cursor_probe_failures
            .fetch_add(1, Ordering::Relaxed);
    } else if info.flags.0 & CURSOR_SHOWING.0 != 0 {
        let counter = if after_repair {
            &shared.source_cursor_visible_after_repair
        } else {
            &shared.source_cursor_visible_samples
        };
        counter.fetch_add(1, Ordering::Relaxed);
        return Some(info);
    }
    None
}

fn record_cursor_probe(shared: &Shared, before: Option<CURSORINFO>, after: CURSORINFO) {
    // Read-only, at most eight anomalies per crossing. No titles or application content.
    let mut hit_pid = 0;
    let mut foreground_pid = 0;
    let mut clip = RECT::default();
    // SAFETY: read-only handles/metadata; output buffers are valid local storage.
    let (hit, foreground, clip_ok) = unsafe {
        let hit = WindowFromPoint(after.ptScreenPos);
        let foreground = GetForegroundWindow();
        GetWindowThreadProcessId(hit, Some(&raw mut hit_pid));
        GetWindowThreadProcessId(foreground, Some(&raw mut foreground_pid));
        (hit, foreground, GetClipCursor(&raw mut clip).is_ok())
    };
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis());
    if let Ok(mut probes) = shared.cursor_probes.try_lock() {
        if probes.len() == 32 {
            probes.pop_front();
        }
        probes.push_back(format!(
            "event=source_cursor_anomaly at_unix_ms={time} owner_thread={} before_cursor={:?} after_cursor={:?} position={},{} hit={hit:?} hit_pid={hit_pid} foreground={foreground:?} foreground_pid={foreground_pid} clip_ok={clip_ok} clip={},{},{},{}",
            shared.thread_id.load(Ordering::Acquire), before.map(|info| info.hCursor), after.hCursor,
            after.ptScreenPos.x, after.ptScreenPos.y, clip.left, clip.top, clip.right, clip.bottom,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::{KeyTransition, PhysicalKey};

    fn key(scan_code: u16, virtual_key: u16) -> InputEvent {
        InputEvent::Key {
            physical: PhysicalKey {
                scan_code,
                extended: false,
            },
            virtual_key,
            transition: KeyTransition::Down,
        }
    }

    #[test]
    fn a_contended_failure_record_cannot_hide_the_atomic_native_fault() {
        let shared = Shared::new();
        let record = shared.failure.lock().unwrap();
        shared.fail("simulated hook failure while status is being read");
        assert!(shared.faulted.load(Ordering::Acquire));
        assert!(shared.stop.load(Ordering::Acquire));
        drop(record);
        assert!(shared.error().is_some());
    }

    #[test]
    fn native_owner_registers_once_and_releases_all_owned_resources_on_stop() {
        let mut runtime = NativeInputRuntime::start().unwrap();
        assert!(runtime.snapshot().running);
        assert!(!runtime.snapshot().forwarding);
        let duplicate = NativeInputRuntime::start()
            .err()
            .expect("a second process-wide input owner must be refused");
        assert_eq!(duplicate.kind(), io::ErrorKind::AlreadyExists);
        assert!(runtime.snapshot().running);
        // Empty batches exercise the real message-thread acknowledgement without
        // moving the user's cursor or injecting a physical key/button.
        runtime.inject(&[]).unwrap();
        runtime.release_all().unwrap();
        let report = runtime.stop().unwrap();
        assert!(report.clean, "{report:?}");
        assert_eq!(report.remaining_inputs, 0);
        assert_eq!(runtime.stop().unwrap().clean, report.clean);
        assert!(!runtime.snapshot().running);
        assert!(runtime.inject(&[]).is_err());
        let mut restarted = NativeInputRuntime::start().unwrap();
        assert!(restarted.stop().unwrap().clean);
    }

    #[test]
    fn capture_coalesces_adjacent_motion_without_reordering_reliable_events() {
        let shared = Arc::new(Shared::new());
        shared.push(NativeInputEvent::PointerMotion {
            x: 1,
            y: 2,
            message_time_ms: 10,
            suppressed: false,
        });
        shared.push(NativeInputEvent::PointerMotion {
            x: 3,
            y: 4,
            message_time_ms: 11,
            suppressed: false,
        });
        let press = NativeInputEvent::Physical(key(30, 65));
        shared.push(press);
        shared.push(NativeInputEvent::RawMotion {
            delta_x: 2,
            delta_y: -1,
            message_time_ms: 1,
        });
        shared.push(NativeInputEvent::RawMotion {
            delta_x: 4,
            delta_y: -3,
            message_time_ms: 2,
        });
        assert_eq!(
            *shared.events.lock().unwrap(),
            VecDeque::from([
                NativeInputEvent::PointerMotion {
                    x: 3,
                    y: 4,
                    message_time_ms: 11,
                    suppressed: false,
                },
                press,
                NativeInputEvent::RawMotion {
                    delta_x: 6,
                    delta_y: -4,
                    message_time_ms: 2
                },
            ])
        );
    }

    #[test]
    fn capture_notifier_runs_for_queued_and_coalesced_motion() {
        let shared = Shared::new();
        let notifications = Arc::new(AtomicU32::new(0));
        let observed = Arc::clone(&notifications);
        *shared.event_notifier.lock().unwrap() = Some(Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }));

        shared.push(NativeInputEvent::RawMotion {
            delta_x: 3,
            delta_y: 4,
            message_time_ms: 10,
        });
        shared.push(NativeInputEvent::RawMotion {
            delta_x: 5,
            delta_y: 6,
            message_time_ms: 11,
        });

        assert_eq!(notifications.load(Ordering::Relaxed), 2);
        assert_eq!(shared.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn transient_motion_contention_never_stops_the_native_input_owner() {
        let shared = Shared::new();
        shared.forwarding.store(true, Ordering::Release);
        let queue = shared.events.lock().unwrap();

        shared.push(NativeInputEvent::PointerMotion {
            x: 120,
            y: 80,
            message_time_ms: 42,
            suppressed: true,
        });

        assert!(!shared.stop.load(Ordering::Acquire));
        assert!(!shared.faulted.load(Ordering::Acquire));
        assert!(shared.forwarding.load(Ordering::Acquire));
        assert!(queue.is_empty());
    }

    #[test]
    fn transient_reliable_contention_returns_local_without_stopping_the_owner() {
        let shared = Arc::new(Shared::new());
        let context = CaptureContext {
            shared: Arc::clone(&shared),
            screen: ScreenExtent {
                width: 1920,
                height: 1080,
            },
        };
        shared.forwarding.store(true, Ordering::Release);
        let queue = shared.events.lock().unwrap();

        assert!(!context.handle_input(key(30, 65)));

        assert!(!shared.stop.load(Ordering::Acquire));
        assert!(!shared.faulted.load(Ordering::Acquire));
        assert!(!shared.forwarding.load(Ordering::Acquire));
        assert!(shared.emergency.load(Ordering::Acquire));
        assert!(shared.release_requested.load(Ordering::Acquire));
        assert_eq!(shared.diagnostics().contention_returns, 1);
        assert_eq!(
            shared.diagnostics().last_recovery,
            Some(NativeInputRecoveryReason::QueueContended)
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn alternating_processed_and_raw_motion_cannot_fill_the_reliable_queue() {
        let shared = Shared::new();
        for sample in 0..EVENT_CAPACITY {
            shared.push(NativeInputEvent::PointerMotion {
                x: i32::try_from(sample).unwrap(),
                y: 20,
                message_time_ms: u32::try_from(sample).unwrap(),
                suppressed: false,
            });
            shared.push(NativeInputEvent::RawMotion {
                delta_x: 1,
                delta_y: 0,
                message_time_ms: u32::try_from(sample).unwrap(),
            });
        }

        assert_eq!(shared.events.lock().unwrap().len(), LOSSY_EVENT_LIMIT);
        assert!(shared.diagnostics().dropped_motion_events > 0);
        assert!(!shared.stop.load(Ordering::Acquire));
        assert!(!shared.faulted.load(Ordering::Acquire));
    }

    #[test]
    fn lossy_motion_stops_before_the_reliable_event_reserve() {
        let shared = Shared::new();
        for sample in 0..(LOSSY_EVENT_LIMIT / 2) {
            shared.push(NativeInputEvent::Physical(key(30, 65)));
            shared.push(NativeInputEvent::PointerMotion {
                x: i32::try_from(sample).unwrap(),
                y: 20,
                message_time_ms: u32::try_from(sample).unwrap(),
                suppressed: false,
            });
        }
        assert_eq!(shared.events.lock().unwrap().len(), LOSSY_EVENT_LIMIT);

        shared.push(NativeInputEvent::RawMotion {
            delta_x: 4,
            delta_y: 0,
            message_time_ms: 999,
        });
        assert_eq!(shared.events.lock().unwrap().len(), LOSSY_EVENT_LIMIT);
        assert_eq!(shared.diagnostics().dropped_motion_events, 1);

        shared.push(NativeInputEvent::Physical(key(31, 66)));
        assert_eq!(shared.events.lock().unwrap().len(), LOSSY_EVENT_LIMIT + 1);
        assert_eq!(shared.diagnostics().recovery_returns, 0);
    }

    #[test]
    fn full_capture_queue_drops_motion_without_losing_reliable_transitions() {
        let shared = Shared::new();
        for _ in 0..EVENT_CAPACITY {
            shared.push(NativeInputEvent::Physical(key(30, 65)));
        }

        shared.push(NativeInputEvent::RawMotion {
            delta_x: 12,
            delta_y: -4,
            message_time_ms: 44,
        });

        assert!(!shared.stop.load(Ordering::Acquire));
        assert!(!shared.faulted.load(Ordering::Acquire));
        assert_eq!(shared.events.lock().unwrap().len(), EVENT_CAPACITY);
    }

    #[test]
    fn target_capture_marks_suppression_without_source_confinement() {
        let shared = Arc::new(Shared::new());
        let context = CaptureContext {
            shared: Arc::clone(&shared),
            screen: ScreenExtent {
                width: 1920,
                height: 1080,
            },
        };
        assert!(!context.handle_mouse_input(InputEvent::MouseMove { x: 10, y: 20 }, 40));
        shared.target_capture.store(true, Ordering::Release);
        assert!(context.handle_mouse_input(InputEvent::MouseMove { x: 11, y: 20 }, 41));
        assert!(!shared.forwarding.load(Ordering::Acquire));
        assert_eq!(
            *shared.events.lock().unwrap(),
            VecDeque::from([
                NativeInputEvent::PointerMotion {
                    x: 10,
                    y: 20,
                    message_time_ms: 40,
                    suppressed: false,
                },
                NativeInputEvent::PointerMotion {
                    x: 11,
                    y: 20,
                    message_time_ms: 41,
                    suppressed: true,
                },
            ])
        );
        shared.invalidate_injection();
        assert!(!shared.target_capture.load(Ordering::Acquire));
        assert!(!context.handle_input(key(30, 65)));
    }

    #[test]
    fn target_capture_discards_raw_echo_immediately_after_pointer_injection() {
        let shared = Arc::new(Shared::new());
        let context = CaptureContext {
            shared: Arc::clone(&shared),
            screen: ScreenExtent {
                width: 1920,
                height: 1080,
            },
        };
        shared.target_capture.store(true, Ordering::Release);
        shared.mark_injected_pointer();

        context.handle_raw_mouse(80, 0, 50);
        assert!(shared.events.lock().unwrap().is_empty());
        assert_eq!(shared.diagnostics().target_raw_echo_filtered, 1);

        shared.clear_injected_pointer_echo();
        context.handle_raw_mouse(80, 0, 51);
        assert_eq!(
            *shared.events.lock().unwrap(),
            VecDeque::from([NativeInputEvent::RawMotion {
                delta_x: 80,
                delta_y: 0,
                message_time_ms: 51,
            }])
        );
        assert_eq!(shared.diagnostics().target_raw_without_injection, 1);
    }

    #[test]
    fn target_capture_records_raw_motion_arriving_after_the_echo_grace() {
        let shared = Arc::new(Shared::new());
        let context = CaptureContext {
            shared: Arc::clone(&shared),
            screen: ScreenExtent {
                width: 1920,
                height: 1080,
            },
        };
        shared.target_capture.store(true, Ordering::Release);
        *shared.injected_raw_echo_deadline.lock().unwrap() =
            Instant::now().checked_sub(Duration::from_millis(1));

        context.handle_raw_mouse(80, 0, 52);

        let diagnostics = shared.diagnostics();
        assert_eq!(diagnostics.target_raw_after_grace, 1);
        assert!(
            diagnostics.max_target_raw_after_grace_micros
                >= u64::try_from(INJECTED_RAW_ECHO_GRACE.as_micros()).unwrap()
        );
        assert_eq!(shared.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn target_takeover_and_slow_injection_are_diagnostic_only() {
        let shared = Shared::new();
        shared.note_target_takeover();
        shared.observe_injection_command(SLOW_INJECTION_COMMAND + Duration::from_millis(1));

        let diagnostics = shared.diagnostics();
        assert_eq!(diagnostics.target_takeover_returns, 1);
        assert_eq!(diagnostics.slow_injection_commands, 1);
        assert!(diagnostics.max_injection_command_micros >= 41_000);
        assert!(!shared.stop.load(Ordering::Acquire));
        assert!(!shared.faulted.load(Ordering::Acquire));
    }

    #[test]
    fn reliable_capture_overflow_returns_local_without_stopping_the_owner() {
        let shared = Arc::new(Shared::new());
        shared.forwarding.store(true, Ordering::Release);
        let generation = shared.generation.load(Ordering::Acquire);
        for _ in 0..=EVENT_CAPACITY {
            shared.push(NativeInputEvent::Physical(key(30, 65)));
        }
        assert!(!shared.stop.load(Ordering::Acquire));
        assert!(!shared.faulted.load(Ordering::Acquire));
        assert!(!shared.forwarding.load(Ordering::Acquire));
        assert!(shared.release_requested.load(Ordering::Acquire));
        assert!(shared.generation.load(Ordering::Acquire) > generation);
        assert_eq!(shared.events.lock().unwrap().len(), EVENT_CAPACITY);
        assert_eq!(shared.diagnostics().overflow_returns, 1);
        assert_eq!(
            shared.diagnostics().last_recovery,
            Some(NativeInputRecoveryReason::QueueOverflow)
        );
        assert_eq!(
            shared.take_events().unwrap(),
            vec![NativeInputEvent::EmergencyReturn]
        );
        assert_eq!(shared.queued_events.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn emergency_chord_is_consumed_locally_and_invalidates_old_batches_immediately() {
        let shared = Arc::new(Shared::new());
        let context = CaptureContext {
            shared: Arc::clone(&shared),
            screen: ScreenExtent {
                width: 1920,
                height: 1080,
            },
        };
        for event in [key(29, 0xa2), key(56, 0xa4), key(42, 0xa0)] {
            assert!(!context.handle_input(event));
        }
        shared.forwarding.store(true, Ordering::Release);
        let generation = shared.generation.load(Ordering::Acquire);
        assert!(context.handle_input(key(1, 0x1b)));
        assert!(shared.emergency.load(Ordering::Acquire));
        assert!(!shared.forwarding.load(Ordering::Acquire));
        assert!(shared.generation.load(Ordering::Acquire) > generation);
        assert!(shared.release_requested.load(Ordering::Acquire));
    }

    #[test]
    fn cancelled_expired_and_pre_release_commands_never_become_valid_again() {
        let shared = Shared::new();
        let (reply, _) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut command = Command {
            action: Action::TargetCapture(true),
            generation: 1,
            deadline: Instant::now() + Duration::from_secs(1),
            cancelled: Arc::clone(&cancelled),
            reply,
        };
        assert!(command.valid(&shared));
        cancelled.store(true, Ordering::Release);
        assert!(!command.valid(&shared));
        cancelled.store(false, Ordering::Release);
        command.deadline = Instant::now();
        assert!(!command.valid(&shared));
        command.deadline = Instant::now() + Duration::from_secs(1);
        shared.invalidate_injection();
        assert!(!command.valid(&shared));
    }
}
