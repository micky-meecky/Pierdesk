use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::app_settings::{AppSettings, CrossScreenSettings};
use crate::device_store::DeviceStore;
use crate::discovery::DiscoveryView;
use crate::workspace::precise_product::{
    PreciseProductRuntime, PreciseProductSnapshot, PreciseProductState,
};

use super::RemoteBoundaryWorkspace;
use super::boundary::{PeerPlacement, PhysicalScreenSize};
use super::channel::ScreenMillimeterSize;
use super::live::{
    LiveBoundarySyncState, LiveControlDiagnosticReport, LiveControlStopReason, LiveSessionControl,
    LiveSessionPhase, LiveTransitionTraceBuffer, foreground_covers_primary_screen,
    run_live_control_product_session,
};
use super::native_input::NativeInputDiagnostics;
use super::runtime::{AutomaticChannelConfig, AutomaticChannelRole};

const PRODUCT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(100);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const PAUSE_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrossScreenProductState {
    Connecting,
    Connected,
    ControllingPeer,
    ControlledByPeer,
    Reconnecting,
    Paused,
    PausedFullscreen,
    LockedLocal,
    Stopped,
    Failed,
}

impl CrossScreenProductState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::ControllingPeer => "controlling_peer",
            Self::ControlledByPeer => "controlled_by_peer",
            Self::Reconnecting => "reconnecting",
            Self::Paused => "paused",
            Self::PausedFullscreen => "paused_fullscreen",
            Self::LockedLocal => "locked_local",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CrossScreenProductSnapshot {
    pub enabled: bool,
    pub state: CrossScreenProductState,
    pub role: AutomaticChannelRole,
    pub peer_label: String,
    pub peer_placement: PeerPlacement,
    pub local_screen: Option<PhysicalScreenSize>,
    pub peer_screen: Option<PhysicalScreenSize>,
    pub peer_screen_millimeters: Option<ScreenMillimeterSize>,
    pub remote_boundary_workspace: Option<RemoteBoundaryWorkspace>,
    pub connections: serde_json::Value,
    pub round_trip_micros: Option<u64>,
    pub session_id: Option<String>,
    pub channel_epoch: Option<u64>,
    pub last_enter_latency_micros: Option<u64>,
    pub connection_attempts: u64,
    pub successful_connections: u64,
    pub reconnects: u64,
    pub control_entries: u64,
    pub control_returns: u64,
    pub forwarded_events: u64,
    pub injected_events: u64,
    pub forced_releases: u64,
    pub native_input: NativeInputDiagnostics,
    pub clean_shutdown: bool,
    pub last_stop_reason: Option<LiveControlStopReason>,
    pub last_control_return_reason: Option<&'static str>,
    pub control_return_sequence: u64,
    pub last_transition_trace: Option<String>,
    pub transition_trace_sequence: u64,
    pub last_error: Option<String>,
}

struct ProductShared {
    stop: AtomicBool,
    paused: AtomicBool,
    locked_local: AtomicBool,
    active_session: Mutex<Option<Arc<LiveSessionControl>>>,
    transition_traces: Arc<LiveTransitionTraceBuffer>,
    boundary_sync: Arc<LiveBoundarySyncState>,
    snapshot: Mutex<CrossScreenProductSnapshot>,
    wake: Condvar,
}

impl ProductShared {
    fn update_snapshot(&self, update: impl FnOnce(&mut CrossScreenProductSnapshot)) {
        match self.snapshot.lock() {
            Ok(mut snapshot) => update(&mut snapshot),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    fn cancel_active_session(&self) {
        let session = self.active_session.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |session| session.clone(),
        );
        if let Some(session) = session {
            session.cancel();
        }
        self.wake.notify_all();
    }

    fn set_active_session(&self, session: Option<Arc<LiveSessionControl>>) {
        match self.active_session.lock() {
            Ok(mut active) => *active = session,
            Err(poisoned) => *poisoned.into_inner() = session,
        }
    }

    fn wait(&self, duration: Duration) {
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = self.wake.wait_timeout(snapshot, duration);
    }
}

pub struct CrossScreenProductRuntime {
    shared: Arc<ProductShared>,
    worker: Option<JoinHandle<()>>,
    precise: Option<PreciseProductRuntime>,
}

impl CrossScreenProductRuntime {
    /// Starts continuous cross-screen control for the currently selected trusted peer.
    ///
    /// # Errors
    ///
    /// Returns an error for disabled/corrupt settings, missing trust, invalid addressing, or
    /// worker creation. Transient network failures are retried in the background.
    pub fn start(
        store: DeviceStore,
        app_settings: &AppSettings,
        settings: CrossScreenSettings,
    ) -> io::Result<Self> {
        crate::license_runtime::require_access()?;
        if !settings.enabled {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cross-screen control is disabled",
            ));
        }
        let config = AutomaticChannelConfig::from_settings(&store, app_settings)?;
        let peer = store.load_peer(app_settings.active_peer)?;
        let local_is_listener = config.role == AutomaticChannelRole::Listener;
        let peer_placement = settings.local_peer_placement(local_is_listener);
        let snapshot = CrossScreenProductSnapshot {
            enabled: true,
            state: CrossScreenProductState::Connecting,
            role: config.role,
            peer_label: peer.label,
            peer_placement,
            local_screen: None,
            peer_screen: None,
            peer_screen_millimeters: None,
            remote_boundary_workspace: None,
            connections: serde_json::Value::Null,
            round_trip_micros: None,
            session_id: None,
            channel_epoch: None,
            last_enter_latency_micros: None,
            connection_attempts: 0,
            successful_connections: 0,
            reconnects: 0,
            control_entries: 0,
            control_returns: 0,
            forwarded_events: 0,
            injected_events: 0,
            forced_releases: 0,
            native_input: NativeInputDiagnostics::default(),
            clean_shutdown: false,
            last_stop_reason: None,
            last_control_return_reason: None,
            control_return_sequence: 0,
            last_transition_trace: None,
            transition_trace_sequence: 0,
            last_error: None,
        };
        let shared = Arc::new(ProductShared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            locked_local: AtomicBool::new(false),
            active_session: Mutex::new(None),
            transition_traces: Arc::new(LiveTransitionTraceBuffer::default()),
            boundary_sync: Arc::new(LiveBoundarySyncState::default()),
            snapshot: Mutex::new(snapshot),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("pierdesk-cross-screen-product".to_owned())
            .spawn(move || product_loop(&store, &settings, &worker_shared))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            precise: None,
        })
    }

    /// Starts the shared multi-peer runtime over the applied v3 logical boundaries.
    ///
    /// # Errors
    /// Rejects disabled settings, invalid active-peer settings, or precise runtime startup errors.
    pub fn start_precise(
        store: &DeviceStore,
        app_settings: &AppSettings,
        settings: &CrossScreenSettings,
        discovery: DiscoveryView,
    ) -> io::Result<Self> {
        crate::license_runtime::require_access()?;
        if !settings.enabled {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cross-screen control is disabled",
            ));
        }
        let config = AutomaticChannelConfig::from_settings(store, app_settings)?;
        let peer = store.load_peer(app_settings.active_peer)?;
        let local_is_listener = config.role == AutomaticChannelRole::Listener;
        let snapshot = CrossScreenProductSnapshot {
            enabled: true,
            state: CrossScreenProductState::Connecting,
            role: config.role,
            peer_label: peer.label,
            peer_placement: settings.local_peer_placement(local_is_listener),
            local_screen: None,
            peer_screen: None,
            peer_screen_millimeters: None,
            remote_boundary_workspace: None,
            connections: serde_json::Value::Null,
            round_trip_micros: None,
            session_id: None,
            channel_epoch: None,
            last_enter_latency_micros: None,
            connection_attempts: 0,
            successful_connections: 0,
            reconnects: 0,
            control_entries: 0,
            control_returns: 0,
            forwarded_events: 0,
            injected_events: 0,
            forced_releases: 0,
            native_input: NativeInputDiagnostics::default(),
            clean_shutdown: false,
            last_stop_reason: None,
            last_control_return_reason: None,
            control_return_sequence: 0,
            last_transition_trace: None,
            transition_trace_sequence: 0,
            last_error: None,
        };
        let shared = Arc::new(ProductShared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            locked_local: AtomicBool::new(false),
            active_session: Mutex::new(None),
            transition_traces: Arc::new(LiveTransitionTraceBuffer::default()),
            boundary_sync: Arc::new(LiveBoundarySyncState::default()),
            snapshot: Mutex::new(snapshot),
            wake: Condvar::new(),
        });
        let precise =
            PreciseProductRuntime::start(store, app_settings.clone(), settings, discovery)?;
        Ok(Self {
            shared,
            worker: None,
            precise: Some(precise),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> CrossScreenProductSnapshot {
        if let Some(precise) = &self.precise {
            return self.precise_snapshot(precise.snapshot());
        }
        let mut snapshot = self.shared.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |snapshot| snapshot.clone(),
        );
        let session = self.shared.active_session.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |session| session.clone(),
        );
        if let Some(session) = session {
            let status = session.snapshot();
            snapshot.state = match (status.phase, status.route) {
                (LiveSessionPhase::Connected, "controlling_peer") => {
                    CrossScreenProductState::ControllingPeer
                }
                (LiveSessionPhase::Connected, "controlled_by_peer") => {
                    CrossScreenProductState::ControlledByPeer
                }
                (LiveSessionPhase::Connected, _) => CrossScreenProductState::Connected,
                (LiveSessionPhase::Connecting, _) => {
                    if snapshot.state == CrossScreenProductState::Reconnecting {
                        CrossScreenProductState::Reconnecting
                    } else {
                        CrossScreenProductState::Connecting
                    }
                }
                (LiveSessionPhase::Stopping, _) => snapshot.state,
            };
            snapshot.local_screen = status.local_screen;
            snapshot.peer_screen = status.peer_screen;
            snapshot.peer_screen_millimeters = status.peer_screen_millimeters;
            snapshot.round_trip_micros = status.round_trip_micros;
            snapshot.session_id = status.session_id;
            snapshot.channel_epoch = status.channel_epoch;
            snapshot.last_enter_latency_micros = status.last_enter_latency_micros;
            snapshot.last_control_return_reason = status.last_control_return_reason;
            snapshot.control_return_sequence = status.control_return_sequence;
            snapshot.last_transition_trace = status.last_transition_trace;
            snapshot.transition_trace_sequence = status.transition_trace_sequence;
            if status.phase == LiveSessionPhase::Connected {
                snapshot.last_stop_reason = None;
                snapshot.last_error = None;
                snapshot.reconnects = snapshot
                    .reconnects
                    .saturating_add(u64::from(snapshot.successful_connections != 0));
                snapshot.successful_connections = snapshot.successful_connections.saturating_add(1);
            }
            if let Some(placement) = status.peer_placement {
                snapshot.peer_placement = placement;
            }
        }
        snapshot.remote_boundary_workspace = self.shared.boundary_sync.snapshot();
        snapshot
    }

    fn precise_snapshot(&self, precise: PreciseProductSnapshot) -> CrossScreenProductSnapshot {
        let mut snapshot = self.shared.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |snapshot| snapshot.clone(),
        );
        snapshot.state = match precise.state {
            PreciseProductState::Connecting => CrossScreenProductState::Connecting,
            PreciseProductState::Connected | PreciseProductState::Relaying => {
                CrossScreenProductState::Connected
            }
            PreciseProductState::ControllingPeer => CrossScreenProductState::ControllingPeer,
            PreciseProductState::ControlledByPeer => CrossScreenProductState::ControlledByPeer,
            PreciseProductState::Paused => CrossScreenProductState::Paused,
            PreciseProductState::PausedFullscreen => CrossScreenProductState::PausedFullscreen,
            PreciseProductState::LockedLocal => CrossScreenProductState::LockedLocal,
            PreciseProductState::Stopped => CrossScreenProductState::Stopped,
            PreciseProductState::Failed => CrossScreenProductState::Failed,
        };
        snapshot.peer_label = format!("{} 台设备", precise.peer_count);
        snapshot.local_screen = Some(precise.local_screen);
        snapshot.connections = precise
            .model
            .get("connections")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        snapshot.round_trip_micros = precise.round_trip_micros;
        snapshot.connection_attempts = u64::try_from(precise.peer_count).unwrap_or(u64::MAX);
        snapshot.successful_connections = u64::try_from(precise.online_count).unwrap_or(u64::MAX);
        snapshot.native_input = precise.native_input;
        snapshot.clean_shutdown = precise.clean_shutdown;
        snapshot.last_control_return_reason = precise.last_control_return_reason;
        snapshot.last_error = precise.last_error;
        snapshot
    }

    /// Queues the latest boundary preview or applied snapshot for the active authenticated peer.
    /// Newer previews replace older unsent previews so pointer traffic cannot be delayed by drag
    /// updates.
    pub fn queue_boundary_workspace(&self, payload: Vec<u8>) {
        self.shared.boundary_sync.queue(payload);
        self.shared.wake.notify_all();
    }

    /// Removes a preview only when it is still the exact message the settings UI handled.
    pub fn clear_remote_boundary_workspace(&self, sequence: u64) {
        self.shared.boundary_sync.clear_inbound(sequence);
    }

    pub(crate) fn drain_diagnostic_traces(&self) -> Vec<String> {
        if let Some(precise) = &self.precise {
            return precise.drain_diagnostic_traces();
        }
        let (events, dropped) = self.shared.transition_traces.drain();
        let mut traces = Vec::with_capacity(events.len() + usize::from(dropped != 0));
        if dropped != 0 {
            traces.push(format!("event=trace_overflow dropped={dropped}"));
        }
        traces.extend(events.into_iter().map(|event| {
            format!(
                "event_seq={} session_elapsed_us={} session={} epoch={} {}",
                event.sequence,
                event.session_elapsed_micros,
                event.session_id.as_deref().unwrap_or("none"),
                event.channel_epoch.unwrap_or(0),
                event.trace
            )
        }));
        traces
    }

    /// Switches one pair on or off. Only the multi-device runtime has per-pair state.
    ///
    /// # Errors
    /// Reports a legacy single-pair runtime, an offline peer, a refused change, or a failed release.
    pub fn set_pair_enabled(
        &self,
        peer: crate::workspace::DeviceId,
        enabled: bool,
    ) -> io::Result<()> {
        self.precise.as_ref().map_or_else(
            || {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "逐对跨屏开关需要已应用的多设备边界",
                ))
            },
            |precise| precise.set_pair_enabled(peer, enabled),
        )
    }

    pub fn pause(&self) {
        if let Some(precise) = &self.precise {
            precise.pause();
            return;
        }
        self.shared.paused.store(true, Ordering::Release);
        self.shared.cancel_active_session();
    }

    pub fn resume(&self) {
        if let Some(precise) = &self.precise {
            precise.resume();
            return;
        }
        self.shared.locked_local.store(false, Ordering::Release);
        self.shared.paused.store(false, Ordering::Release);
        self.shared.update_snapshot(|snapshot| {
            snapshot.last_stop_reason = None;
            snapshot.last_error = None;
        });
        self.shared.wake.notify_all();
    }

    pub fn return_local(&self) {
        if let Some(precise) = &self.precise {
            precise.return_local();
            return;
        }
        self.shared.locked_local.store(true, Ordering::Release);
        self.shared.cancel_active_session();
    }

    pub fn stop(&mut self) {
        if let Some(mut precise) = self.precise.take() {
            let _ = precise.stop();
            return;
        }
        if self.shared.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.cancel_active_session();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for CrossScreenProductRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn product_loop(store: &DeviceStore, settings: &CrossScreenSettings, shared: &ProductShared) {
    let mut reconnect_backoff = INITIAL_RECONNECT_BACKOFF;
    let mut connected_once = false;
    while !shared.stop.load(Ordering::Acquire) {
        if shared.paused.load(Ordering::Acquire) || shared.locked_local.load(Ordering::Acquire) {
            shared.update_snapshot(|snapshot| {
                snapshot.state = if shared.locked_local.load(Ordering::Acquire) {
                    if snapshot.last_error.is_some() {
                        CrossScreenProductState::Failed
                    } else {
                        CrossScreenProductState::LockedLocal
                    }
                } else {
                    CrossScreenProductState::Paused
                };
            });
            shared.wait(PAUSE_POLL_INTERVAL);
            continue;
        }
        if settings.pause_on_fullscreen && foreground_covers_primary_screen().unwrap_or(false) {
            shared.update_snapshot(|snapshot| {
                snapshot.state = CrossScreenProductState::PausedFullscreen;
            });
            shared.wait(PAUSE_POLL_INTERVAL);
            continue;
        }

        shared.update_snapshot(|snapshot| {
            snapshot.connection_attempts = snapshot.connection_attempts.saturating_add(1);
            snapshot.state = if connected_once {
                CrossScreenProductState::Reconnecting
            } else {
                CrossScreenProductState::Connecting
            };
        });
        let session = Arc::new(LiveSessionControl::with_boundary_sync(
            settings.pause_on_fullscreen,
            Arc::clone(&shared.transition_traces),
            Arc::clone(&shared.boundary_sync),
        ));
        shared.set_active_session(Some(Arc::clone(&session)));
        let result = run_live_control_product_session(store, PRODUCT_CONNECT_TIMEOUT, &session);
        shared.set_active_session(None);

        let retry_immediately = match result {
            Ok(report) => {
                record_report(shared, &report, connected_once);
                connected_once = true;
                reconnect_backoff = INITIAL_RECONNECT_BACKOFF;
                handle_stop_reason(shared, report.stop_reason)
            }
            Err(error) => {
                let role = shared.snapshot.lock().map_or_else(
                    |poisoned| poisoned.into_inner().role,
                    |snapshot| snapshot.role,
                );
                record_session_start_error(
                    shared,
                    role,
                    error.kind(),
                    error.to_string(),
                    connected_once,
                )
            }
        };
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if !retry_immediately {
            shared.wait(reconnect_backoff);
            reconnect_backoff = reconnect_backoff
                .saturating_mul(2)
                .min(MAX_RECONNECT_BACKOFF);
        }
    }
    shared.set_active_session(None);
    shared.update_snapshot(|snapshot| {
        snapshot.state = CrossScreenProductState::Stopped;
        if snapshot.successful_connections == 0 {
            snapshot.clean_shutdown = true;
        }
    });
}

fn record_session_start_error(
    shared: &ProductShared,
    role: AutomaticChannelRole,
    kind: io::ErrorKind,
    message: String,
    connected_once: bool,
) -> bool {
    if kind == io::ErrorKind::Interrupted {
        return true;
    }
    shared.update_snapshot(|snapshot| {
        snapshot.state = if kind == io::ErrorKind::TimedOut && !connected_once {
            CrossScreenProductState::Connecting
        } else {
            CrossScreenProductState::Reconnecting
        };
        // A listener merely reaching the end of one bounded accept window is normal while the
        // other computer sleeps. A connector timeout means an actual selected path failed and must
        // remain visible so adapter, firewall, and routing problems are not hidden as `error=none`.
        let idle_listener_timeout = kind == io::ErrorKind::TimedOut
            && role == AutomaticChannelRole::Listener
            && message.contains("listener timed out");
        snapshot.last_error = (!idle_listener_timeout).then(|| {
            if kind == io::ErrorKind::TimedOut && role == AutomaticChannelRole::Connector {
                format!(
                    "连接对端超时。请确认另一台电脑的 Pierdesk 正在运行、两台电脑位于可互通的局域网，并在 Windows 防火墙中允许专用网络。详细信息：{message}"
                )
            } else {
                message
            }
        });
    });
    false
}

fn record_report(
    shared: &ProductShared,
    report: &LiveControlDiagnosticReport,
    connected_once: bool,
) {
    let safe = report.remaining_forwarded_inputs == 0 && report.remaining_injected_inputs == 0;
    shared.update_snapshot(|snapshot| {
        snapshot.successful_connections = snapshot.successful_connections.saturating_add(1);
        snapshot.reconnects = snapshot
            .reconnects
            .saturating_add(u64::from(connected_once));
        snapshot.control_entries = snapshot
            .control_entries
            .saturating_add(report.control_entries);
        snapshot.control_returns = snapshot
            .control_returns
            .saturating_add(report.control_returns);
        snapshot.forwarded_events = snapshot
            .forwarded_events
            .saturating_add(report.forwarded.total_observed());
        snapshot.injected_events = snapshot
            .injected_events
            .saturating_add(report.injected_events);
        snapshot.forced_releases = snapshot
            .forced_releases
            .saturating_add(report.forced_injected_releases);
        snapshot.clean_shutdown = safe;
        snapshot.last_stop_reason = Some(report.stop_reason);
        snapshot.last_error.clone_from(&report.failure);
    });
}

fn handle_stop_reason(shared: &ProductShared, reason: LiveControlStopReason) -> bool {
    match reason {
        LiveControlStopReason::FullscreenPause => {
            shared.update_snapshot(|snapshot| {
                snapshot.state = CrossScreenProductState::PausedFullscreen;
            });
            true
        }
        LiveControlStopReason::DisplayChanged | LiveControlStopReason::DurationElapsed => {
            shared.update_snapshot(|snapshot| {
                snapshot.state = CrossScreenProductState::Reconnecting;
            });
            true
        }
        LiveControlStopReason::ExternalStop => false,
        LiveControlStopReason::ConnectionFailure => {
            shared.update_snapshot(|snapshot| {
                snapshot.state = CrossScreenProductState::Reconnecting;
            });
            false
        }
        LiveControlStopReason::EmergencyShortcut => {
            shared.locked_local.store(true, Ordering::Release);
            shared.update_snapshot(|snapshot| {
                snapshot.state = CrossScreenProductState::LockedLocal;
            });
            false
        }
        LiveControlStopReason::QueueFailure
        | LiveControlStopReason::InjectionFailure
        | LiveControlStopReason::MessageLoopFailure => {
            shared.locked_local.store(true, Ordering::Release);
            shared.update_snapshot(|snapshot| {
                snapshot.state = CrossScreenProductState::Failed;
            });
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_shared() -> ProductShared {
        ProductShared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            locked_local: AtomicBool::new(false),
            active_session: Mutex::new(None),
            transition_traces: Arc::new(LiveTransitionTraceBuffer::default()),
            boundary_sync: Arc::new(LiveBoundarySyncState::default()),
            snapshot: Mutex::new(CrossScreenProductSnapshot {
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
                round_trip_micros: None,
                session_id: None,
                channel_epoch: None,
                last_enter_latency_micros: None,
                connection_attempts: 1,
                successful_connections: 1,
                reconnects: 0,
                control_entries: 0,
                control_returns: 0,
                forwarded_events: 0,
                injected_events: 0,
                forced_releases: 0,
                native_input: NativeInputDiagnostics::default(),
                clean_shutdown: false,
                last_stop_reason: None,
                last_control_return_reason: None,
                control_return_sequence: 0,
                last_transition_trace: None,
                transition_trace_sequence: 0,
                last_error: None,
            }),
            wake: Condvar::new(),
        }
    }

    #[test]
    fn product_state_labels_are_stable() {
        assert_eq!(CrossScreenProductState::Connecting.label(), "connecting");
        assert_eq!(CrossScreenProductState::LockedLocal.label(), "locked_local");
        assert_eq!(
            CrossScreenProductState::PausedFullscreen.label(),
            "paused_fullscreen"
        );
    }

    #[test]
    fn product_drain_preserves_every_transition_trace() {
        let shared = Arc::new(test_shared());
        let control =
            LiveSessionControl::with_trace_buffer(false, Arc::clone(&shared.transition_traces));
        control.record_transition_trace("kind=edge_contact".to_owned());
        control.record_transition_trace("kind=enter_request".to_owned());
        let runtime = CrossScreenProductRuntime {
            shared,
            worker: None,
            precise: None,
        };

        let traces = runtime.drain_diagnostic_traces();

        assert_eq!(traces.len(), 2);
        assert!(traces[0].contains("event_seq=1"));
        assert!(traces[0].contains("session=none"));
        assert!(traces[0].contains("kind=edge_contact"));
        assert!(traces[1].contains("kind=enter_request"));
        assert!(runtime.drain_diagnostic_traces().is_empty());
    }

    #[test]
    fn stop_reasons_choose_safe_product_states() {
        let shared = test_shared();

        assert!(handle_stop_reason(
            &shared,
            LiveControlStopReason::FullscreenPause
        ));
        assert_eq!(
            shared.snapshot.lock().unwrap().state,
            CrossScreenProductState::PausedFullscreen
        );

        assert!(handle_stop_reason(
            &shared,
            LiveControlStopReason::DisplayChanged
        ));
        assert_eq!(
            shared.snapshot.lock().unwrap().state,
            CrossScreenProductState::Reconnecting
        );

        assert!(!handle_stop_reason(
            &shared,
            LiveControlStopReason::EmergencyShortcut
        ));
        assert!(shared.locked_local.load(Ordering::Acquire));
        assert_eq!(
            shared.snapshot.lock().unwrap().state,
            CrossScreenProductState::LockedLocal
        );
    }

    #[test]
    fn input_failures_lock_local_and_surface_failed_state() {
        for reason in [
            LiveControlStopReason::QueueFailure,
            LiveControlStopReason::InjectionFailure,
            LiveControlStopReason::MessageLoopFailure,
        ] {
            let shared = test_shared();
            assert!(!handle_stop_reason(&shared, reason));
            assert!(shared.locked_local.load(Ordering::Acquire));
            assert_eq!(
                shared.snapshot.lock().unwrap().state,
                CrossScreenProductState::Failed
            );
        }
    }

    #[test]
    fn idle_connection_timeouts_remain_waiting_without_a_false_error() {
        let shared = test_shared();

        assert!(!record_session_start_error(
            &shared,
            AutomaticChannelRole::Listener,
            io::ErrorKind::TimedOut,
            "listener timed out".to_owned(),
            false,
        ));
        let snapshot = shared.snapshot.lock().unwrap().clone();
        assert_eq!(snapshot.state, CrossScreenProductState::Connecting);
        assert!(snapshot.last_error.is_none());

        drop(snapshot);
        assert!(!record_session_start_error(
            &shared,
            AutomaticChannelRole::Listener,
            io::ErrorKind::TimedOut,
            "listener timed out".to_owned(),
            true,
        ));
        let snapshot = shared.snapshot.lock().unwrap();
        assert_eq!(snapshot.state, CrossScreenProductState::Reconnecting);
        assert!(snapshot.last_error.is_none());
    }

    #[test]
    fn connector_timeout_is_not_hidden_as_idle_waiting() {
        let shared = test_shared();

        assert!(!record_session_start_error(
            &shared,
            AutomaticChannelRole::Connector,
            io::ErrorKind::TimedOut,
            "timed out waiting for the paired device: connection timed out".to_owned(),
            false,
        ));
        let snapshot = shared.snapshot.lock().unwrap();
        assert_eq!(snapshot.state, CrossScreenProductState::Connecting);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some(
                "连接对端超时。请确认另一台电脑的 Pierdesk 正在运行、两台电脑位于可互通的局域网，并在 Windows 防火墙中允许专用网络。详细信息：timed out waiting for the paired device: connection timed out"
            )
        );
    }
}
