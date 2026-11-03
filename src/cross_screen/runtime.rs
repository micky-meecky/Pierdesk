use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::app_settings::AppSettings;
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;

use super::channel::{
    ChannelControlEvent, ChannelSnapshot, ChannelState, ControlTransitionAck, CrossScreenChannel,
    MOUSE_SAMPLE_INTERVAL, NormalizedMousePosition, PhysicalPrimaryScreen,
};
use super::control::{
    ControlCoordinator, ControlProtocolAction, ControlProtocolState, dispatch_control_actions,
};
use super::{BoundaryPolicy, PeerPlacement, PhysicalScreenSize};

pub const AUTO_CHANNEL_PORT: u16 = 45_234;

const CONNECTION_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(100);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const CONNECTED_STATUS_INTERVAL: Duration = Duration::from_millis(100);
const SCREEN_INFO_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_DIAGNOSTIC_LIFETIME: Duration = Duration::from_mins(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomaticChannelRole {
    Listener,
    Connector,
}

impl AutomaticChannelRole {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Listener => "listener",
            Self::Connector => "connector",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrossScreenRuntimeState {
    WaitingForPeer,
    Connecting,
    Connected,
    Reconnecting,
    Stopped,
    Failed,
}

impl CrossScreenRuntimeState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WaitingForPeer => "waiting_for_peer",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossScreenRuntimeSnapshot {
    pub state: CrossScreenRuntimeState,
    pub role: AutomaticChannelRole,
    pub local_fingerprint: CertificateFingerprint,
    pub peer_fingerprint: CertificateFingerprint,
    pub local_endpoint: SocketAddr,
    pub peer_endpoint: SocketAddr,
    pub connection_attempts: u64,
    pub successful_connections: u64,
    pub reconnects: u64,
    pub queued_mouse_positions: u64,
    pub runtime_coalesced_mouse_positions: u64,
    pub clean_shutdown: bool,
    pub last_error: Option<String>,
    pub channel: Option<ChannelSnapshot>,
    pub control: AutomaticControlSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomaticControlSnapshot {
    pub enabled: bool,
    pub ready: bool,
    pub state: ControlProtocolState,
    pub local_screen: Option<PhysicalScreenSize>,
    pub peer_screen: Option<PhysicalScreenSize>,
    pub screen_info_sent: u64,
    pub screen_info_received: u64,
    pub successful_handshakes: u64,
    pub rejected_transition_requests: u64,
    pub release_all_observed: u64,
    pub forced_local_resets: u64,
}

impl AutomaticControlSnapshot {
    const fn disabled() -> Self {
        Self {
            enabled: false,
            ready: false,
            state: ControlProtocolState::Local,
            local_screen: None,
            peer_screen: None,
            screen_info_sent: 0,
            screen_info_received: 0,
            successful_handshakes: 0,
            rejected_transition_requests: 0,
            release_all_observed: 0,
            forced_local_resets: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AutomaticChannelDiagnosticOptions {
    pub store: DeviceStore,
    pub connect_timeout: Duration,
    pub lifetime: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomaticChannelDiagnosticReport {
    pub sampled_mouse_positions: u64,
    pub snapshot: CrossScreenRuntimeSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AutomaticChannelConfig {
    pub(super) role: AutomaticChannelRole,
    pub(super) local_fingerprint: CertificateFingerprint,
    pub(super) peer_fingerprint: CertificateFingerprint,
    pub(super) local_endpoint: SocketAddr,
    pub(super) peer_endpoint: SocketAddr,
}

impl AutomaticChannelConfig {
    pub(super) fn from_settings(store: &DeviceStore, settings: &AppSettings) -> io::Result<Self> {
        validate_source_endpoint(settings.local_endpoint)?;
        validate_source_endpoint(settings.peer_endpoint)?;
        if settings.local_endpoint.ip().is_loopback() != settings.peer_endpoint.ip().is_loopback() {
            return Err(invalid_data(
                "automatic cross-screen endpoints must use the same network scope",
            ));
        }
        let identity = store.load_identity()?;
        let local_fingerprint = identity.fingerprint();
        let peer = store.load_peer(settings.active_peer)?;
        if local_fingerprint == peer.fingerprint {
            return Err(invalid_data(
                "automatic cross-screen peer must differ from the local device",
            ));
        }
        let role = select_role(local_fingerprint, peer.fingerprint);
        Ok(Self {
            role,
            local_fingerprint,
            peer_fingerprint: peer.fingerprint,
            local_endpoint: SocketAddr::new(settings.local_endpoint.ip(), AUTO_CHANNEL_PORT),
            peer_endpoint: SocketAddr::new(settings.peer_endpoint.ip(), AUTO_CHANNEL_PORT),
        })
    }

    pub(super) const fn attempt_endpoint(self) -> SocketAddr {
        match self.role {
            AutomaticChannelRole::Listener => self.local_endpoint,
            AutomaticChannelRole::Connector => self.peer_endpoint,
        }
    }
}

#[derive(Default)]
struct RuntimeSignals {
    pending_mouse: Option<NormalizedMousePosition>,
    release_all: bool,
}

struct RuntimeShared {
    stop: AtomicBool,
    signals: Mutex<RuntimeSignals>,
    wake: Condvar,
    snapshot: Mutex<CrossScreenRuntimeSnapshot>,
}

impl RuntimeShared {
    fn snapshot(&self) -> CrossScreenRuntimeSnapshot {
        self.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |snapshot| snapshot.clone(),
        )
    }

    fn update_snapshot(&self, update: impl FnOnce(&mut CrossScreenRuntimeSnapshot)) {
        match self.snapshot.lock() {
            Ok(mut snapshot) => update(&mut snapshot),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    fn clear_pending_input(&self) {
        match self.signals.lock() {
            Ok(mut signals) => *signals = RuntimeSignals::default(),
            Err(poisoned) => *poisoned.into_inner() = RuntimeSignals::default(),
        }
    }

    fn wait(&self, duration: Duration) {
        let signals = match self.signals.lock() {
            Ok(signals) => signals,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.stop.load(Ordering::Acquire)
            || signals.pending_mouse.is_some()
            || signals.release_all
        {
            return;
        }
        let _ = self.wake.wait_timeout(signals, duration);
    }
}

pub struct CrossScreenRuntime {
    shared: Arc<RuntimeShared>,
    worker: Option<JoinHandle<()>>,
}

impl CrossScreenRuntime {
    /// Starts an automatically addressed input channel for the already-selected trusted peer.
    ///
    /// Both devices deterministically choose opposite roles from their certificate fingerprints,
    /// so callers never supply an IP address, port, role, or peer fingerprint.
    ///
    /// # Errors
    ///
    /// Returns an error when existing product settings, identity, trust, endpoints, or worker
    /// creation are invalid. Network failures after startup are retried in the background.
    pub fn start(store: DeviceStore, settings: &AppSettings) -> io::Result<Self> {
        Self::start_with_attempt_timeout(store, settings, CONNECTION_ATTEMPT_TIMEOUT)
    }

    fn start_with_attempt_timeout(
        store: DeviceStore,
        settings: &AppSettings,
        attempt_timeout: Duration,
    ) -> io::Result<Self> {
        Self::start_internal(store, settings, attempt_timeout, None)
    }

    fn start_with_control_attempt_timeout(
        store: DeviceStore,
        settings: &AppSettings,
        attempt_timeout: Duration,
        policy: BoundaryPolicy,
    ) -> io::Result<Self> {
        Self::start_internal(store, settings, attempt_timeout, Some(policy))
    }

    fn start_internal(
        store: DeviceStore,
        settings: &AppSettings,
        attempt_timeout: Duration,
        control_policy: Option<BoundaryPolicy>,
    ) -> io::Result<Self> {
        if attempt_timeout.is_zero() || attempt_timeout > Duration::from_secs(30) {
            return Err(invalid_data(
                "automatic cross-screen attempt timeout must be between 1 millisecond and 30 seconds",
            ));
        }
        let config = AutomaticChannelConfig::from_settings(&store, settings)?;
        let initial_state = match config.role {
            AutomaticChannelRole::Listener => CrossScreenRuntimeState::WaitingForPeer,
            AutomaticChannelRole::Connector => CrossScreenRuntimeState::Connecting,
        };
        let shared = Arc::new(RuntimeShared {
            stop: AtomicBool::new(false),
            signals: Mutex::new(RuntimeSignals::default()),
            wake: Condvar::new(),
            snapshot: Mutex::new(CrossScreenRuntimeSnapshot {
                state: initial_state,
                role: config.role,
                local_fingerprint: config.local_fingerprint,
                peer_fingerprint: config.peer_fingerprint,
                local_endpoint: config.local_endpoint,
                peer_endpoint: config.peer_endpoint,
                connection_attempts: 0,
                successful_connections: 0,
                reconnects: 0,
                queued_mouse_positions: 0,
                runtime_coalesced_mouse_positions: 0,
                clean_shutdown: false,
                last_error: None,
                channel: None,
                control: control_policy.map_or_else(AutomaticControlSnapshot::disabled, |_| {
                    AutomaticControlSnapshot {
                        enabled: true,
                        ..AutomaticControlSnapshot::disabled()
                    }
                }),
            }),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("pierdesk-cross-screen-runtime".to_owned())
            .spawn(move || {
                run_worker(
                    &store,
                    config,
                    attempt_timeout,
                    control_policy,
                    &worker_shared,
                );
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Queues only the newest normalized mouse position while the automatic channel is connected.
    ///
    /// # Errors
    ///
    /// Returns `NotConnected` before a channel is authenticated or after it disconnects.
    pub fn send_mouse_position(&self, position: NormalizedMousePosition) -> io::Result<()> {
        if self.snapshot().state != CrossScreenRuntimeState::Connected {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "automatic cross-screen channel is not connected",
            ));
        }
        let coalesced = {
            let mut signals = self
                .shared
                .signals
                .lock()
                .map_err(|_| io::Error::other("cross-screen runtime queue was poisoned"))?;
            signals.pending_mouse.replace(position).is_some()
        };
        self.shared.update_snapshot(|snapshot| {
            snapshot.queued_mouse_positions += 1;
            if coalesced {
                snapshot.runtime_coalesced_mouse_positions += 1;
            }
        });
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Requests an idempotent release-all frame when a channel is currently active.
    pub fn request_release_all(&self) {
        if self.snapshot().state != CrossScreenRuntimeState::Connected {
            return;
        }
        match self.shared.signals.lock() {
            Ok(mut signals) => signals.release_all = true,
            Err(poisoned) => poisoned.into_inner().release_all = true,
        }
        self.shared.wake.notify_one();
    }

    #[must_use]
    pub fn snapshot(&self) -> CrossScreenRuntimeSnapshot {
        self.shared.snapshot()
    }

    pub fn stop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if worker.join().is_err() {
            self.shared.update_snapshot(|snapshot| {
                snapshot.state = CrossScreenRuntimeState::Failed;
                snapshot.clean_shutdown = false;
                snapshot.last_error = Some("cross-screen runtime worker panicked".to_owned());
            });
        }
    }
}

impl Drop for CrossScreenRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_worker(
    store: &DeviceStore,
    config: AutomaticChannelConfig,
    attempt_timeout: Duration,
    control_policy: Option<BoundaryPolicy>,
    shared: &RuntimeShared,
) {
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    while !shared.stop.load(Ordering::Acquire) {
        let mut retry_delay = backoff;
        shared.update_snapshot(|snapshot| {
            snapshot.state = if snapshot.successful_connections == 0 {
                match config.role {
                    AutomaticChannelRole::Listener => CrossScreenRuntimeState::WaitingForPeer,
                    AutomaticChannelRole::Connector => CrossScreenRuntimeState::Connecting,
                }
            } else {
                CrossScreenRuntimeState::Reconnecting
            };
            snapshot.connection_attempts += 1;
        });
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
            Ok(mut channel) => {
                backoff = INITIAL_RECONNECT_BACKOFF;
                retry_delay = match config.role {
                    AutomaticChannelRole::Listener => Duration::ZERO,
                    AutomaticChannelRole::Connector => INITIAL_RECONNECT_BACKOFF,
                };
                shared.clear_pending_input();
                let initial = channel.snapshot();
                shared.update_snapshot(|snapshot| {
                    snapshot.successful_connections += 1;
                    snapshot.reconnects = snapshot.successful_connections.saturating_sub(1);
                    snapshot.state = CrossScreenRuntimeState::Connected;
                    snapshot.last_error = None;
                    snapshot.channel = Some(initial);
                });
                drive_channel(
                    shared,
                    store,
                    config.peer_fingerprint,
                    control_policy,
                    &mut channel,
                );
                let runtime_error = shared.snapshot().last_error;
                channel.stop();
                let final_snapshot = channel.snapshot();
                let channel_error = final_snapshot.last_error.clone();
                shared.clear_pending_input();
                shared.update_snapshot(|snapshot| {
                    snapshot.channel = Some(final_snapshot);
                    if !shared.stop.load(Ordering::Acquire) {
                        snapshot.state = CrossScreenRuntimeState::Reconnecting;
                        snapshot.last_error = runtime_error.or(channel_error);
                    }
                });
            }
            Err(error) => {
                if config.role == AutomaticChannelRole::Listener
                    && error.kind() == io::ErrorKind::TimedOut
                {
                    backoff = INITIAL_RECONNECT_BACKOFF;
                    retry_delay = Duration::ZERO;
                }
                shared.clear_pending_input();
                shared.update_snapshot(|snapshot| snapshot.last_error = Some(error.to_string()));
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if !retry_delay.is_zero() {
            shared.wait(retry_delay);
            backoff = (backoff.saturating_mul(2)).min(MAX_RECONNECT_BACKOFF);
        }
    }
    shared.clear_pending_input();
    shared.update_snapshot(|snapshot| {
        snapshot.state = CrossScreenRuntimeState::Stopped;
        snapshot.clean_shutdown = true;
    });
}

struct RuntimeControlSession {
    policy: BoundaryPolicy,
    local_screen: PhysicalScreenSize,
    coordinator: Option<ControlCoordinator>,
    next_screen_refresh: Instant,
}

impl RuntimeControlSession {
    fn start(
        policy: BoundaryPolicy,
        channel: &CrossScreenChannel,
        shared: &RuntimeShared,
    ) -> io::Result<Self> {
        let local_screen = PhysicalPrimaryScreen::current()?.size();
        channel.send_screen_info(local_screen)?;
        shared.update_snapshot(|snapshot| {
            snapshot.control.enabled = true;
            snapshot.control.ready = false;
            snapshot.control.state = ControlProtocolState::Local;
            snapshot.control.local_screen = Some(local_screen);
            snapshot.control.peer_screen = None;
            snapshot.control.screen_info_sent += 1;
        });
        Ok(Self {
            policy,
            local_screen,
            coordinator: None,
            next_screen_refresh: Instant::now() + SCREEN_INFO_REFRESH_INTERVAL,
        })
    }

    fn pump(&mut self, channel: &CrossScreenChannel, shared: &RuntimeShared) -> io::Result<()> {
        self.refresh_local_screen(channel, shared)?;
        while let Some(event) = channel.try_receive_control_event()? {
            match event {
                ChannelControlEvent::FileDrag(_) | ChannelControlEvent::BoundaryWorkspace(_) => {}
                ChannelControlEvent::ScreenInfo(peer_screen) => {
                    self.handle_screen_info(peer_screen, channel, shared)?;
                }
                ChannelControlEvent::ScreenMillimeters(_) | ChannelControlEvent::LayoutInfo(_) => {
                    return Err(invalid_data(
                        "unexpected session metadata in diagnostic runtime",
                    ));
                }
                ChannelControlEvent::EnterRequest(request) => {
                    channel.send_enter_acknowledgement(ControlTransitionAck {
                        control_epoch: request.control_epoch,
                        accepted: false,
                    })?;
                    shared.update_snapshot(|snapshot| {
                        snapshot.control.rejected_transition_requests += 1;
                    });
                }
                ChannelControlEvent::ReturnRequest(request) => {
                    channel.send_return_acknowledgement(ControlTransitionAck {
                        control_epoch: request.control_epoch,
                        accepted: false,
                    })?;
                    shared.update_snapshot(|snapshot| {
                        snapshot.control.rejected_transition_requests += 1;
                    });
                }
                ChannelControlEvent::ReleaseAll => {
                    if let Some(coordinator) = self.coordinator.as_mut() {
                        let actions = coordinator.handle_channel_event(event)?;
                        dispatch_runtime_actions(channel, shared, actions)?;
                        shared.update_snapshot(|snapshot| {
                            snapshot.control.state = coordinator.state();
                        });
                    } else {
                        shared.update_snapshot(|snapshot| {
                            snapshot.control.forced_local_resets += 1;
                        });
                    }
                    shared.update_snapshot(|snapshot| {
                        snapshot.control.release_all_observed += 1;
                    });
                }
                ChannelControlEvent::EnterAcknowledgement(_)
                | ChannelControlEvent::ReturnAcknowledgement(_) => {
                    return Err(invalid_data(
                        "cross-screen acknowledgement arrived without a local transition",
                    ));
                }
            }
        }
        Ok(())
    }

    fn refresh_local_screen(
        &mut self,
        channel: &CrossScreenChannel,
        shared: &RuntimeShared,
    ) -> io::Result<()> {
        if Instant::now() < self.next_screen_refresh {
            return Ok(());
        }
        self.next_screen_refresh = Instant::now() + SCREEN_INFO_REFRESH_INTERVAL;
        let current = PhysicalPrimaryScreen::current()?.size();
        if current == self.local_screen {
            return Ok(());
        }
        if let Some(coordinator) = self.coordinator.as_mut() {
            let actions = coordinator.update_local_screen(current)?;
            dispatch_runtime_actions(channel, shared, actions)?;
        }
        self.local_screen = current;
        channel.send_screen_info(current)?;
        let state = self
            .coordinator
            .as_ref()
            .map_or(ControlProtocolState::Local, ControlCoordinator::state);
        shared.update_snapshot(|snapshot| {
            snapshot.control.state = state;
            snapshot.control.local_screen = Some(current);
            snapshot.control.screen_info_sent += 1;
        });
        Ok(())
    }

    fn handle_screen_info(
        &mut self,
        peer_screen: PhysicalScreenSize,
        channel: &CrossScreenChannel,
        shared: &RuntimeShared,
    ) -> io::Result<()> {
        if let Some(coordinator) = self.coordinator.as_mut() {
            let actions =
                coordinator.handle_channel_event(ChannelControlEvent::ScreenInfo(peer_screen))?;
            dispatch_runtime_actions(channel, shared, actions)?;
        } else {
            self.coordinator = Some(ControlCoordinator::new(
                self.policy,
                self.local_screen,
                peer_screen,
            )?);
        }
        let state = self
            .coordinator
            .as_ref()
            .map_or(ControlProtocolState::Local, ControlCoordinator::state);
        shared.update_snapshot(|snapshot| {
            if !snapshot.control.ready {
                snapshot.control.successful_handshakes += 1;
            }
            snapshot.control.ready = true;
            snapshot.control.state = state;
            snapshot.control.peer_screen = Some(peer_screen);
            snapshot.control.screen_info_received += 1;
        });
        Ok(())
    }

    fn force_local(&mut self, channel: &CrossScreenChannel, shared: &RuntimeShared) {
        if let Some(coordinator) = self.coordinator.as_mut() {
            let actions = coordinator.disconnect();
            if let Err(error) = dispatch_runtime_actions(channel, shared, actions) {
                shared.update_snapshot(|snapshot| {
                    snapshot.last_error.get_or_insert_with(|| error.to_string());
                });
            }
        } else {
            channel.request_release_all();
            shared.update_snapshot(|snapshot| snapshot.control.forced_local_resets += 1);
        }
        shared.update_snapshot(|snapshot| {
            snapshot.control.ready = false;
            snapshot.control.state = ControlProtocolState::Local;
        });
    }
}

fn dispatch_runtime_actions(
    channel: &CrossScreenChannel,
    shared: &RuntimeShared,
    actions: impl IntoIterator<Item = ControlProtocolAction>,
) -> io::Result<()> {
    dispatch_control_actions(channel, actions, || {
        shared.update_snapshot(|snapshot| snapshot.control.forced_local_resets += 1);
        Ok(())
    })
}

fn drive_channel(
    shared: &RuntimeShared,
    store: &DeviceStore,
    peer_fingerprint: CertificateFingerprint,
    control_policy: Option<BoundaryPolicy>,
    channel: &mut CrossScreenChannel,
) {
    let mut control = match control_policy {
        Some(policy) => match RuntimeControlSession::start(policy, channel, shared) {
            Ok(control) => Some(control),
            Err(error) => {
                shared.update_snapshot(|snapshot| snapshot.last_error = Some(error.to_string()));
                return;
            }
        },
        None => None,
    };
    while !shared.stop.load(Ordering::Acquire) {
        let channel_snapshot = channel.snapshot();
        shared.update_snapshot(|snapshot| snapshot.channel = Some(channel_snapshot.clone()));
        if channel_snapshot.state != ChannelState::Connected {
            break;
        }
        if let Err(error) = store.load_peer(peer_fingerprint) {
            channel.request_release_all();
            shared.update_snapshot(|snapshot| {
                snapshot.last_error = Some(format!(
                    "cross-screen peer trust was revoked while connected: {error}"
                ));
            });
            break;
        }
        if let Some(control) = control.as_mut()
            && let Err(error) = control.pump(channel, shared)
        {
            shared.update_snapshot(|snapshot| snapshot.last_error = Some(error.to_string()));
            break;
        }
        let (pending_mouse, release_all) = {
            let mut signals = match shared.signals.lock() {
                Ok(signals) => signals,
                Err(poisoned) => poisoned.into_inner(),
            };
            (
                signals.pending_mouse.take(),
                std::mem::take(&mut signals.release_all),
            )
        };
        if release_all {
            channel.request_release_all();
        }
        if let Some(position) = pending_mouse
            && let Err(error) = channel.send_mouse_position(position)
        {
            shared.update_snapshot(|snapshot| snapshot.last_error = Some(error.to_string()));
            break;
        }
        shared.wait(CONNECTED_STATUS_INTERVAL);
    }
    if let Some(control) = control.as_mut() {
        control.force_local(channel, shared);
    }
}

fn select_role(
    local_fingerprint: CertificateFingerprint,
    peer_fingerprint: CertificateFingerprint,
) -> AutomaticChannelRole {
    if local_fingerprint.bytes() < peer_fingerprint.bytes() {
        AutomaticChannelRole::Listener
    } else {
        AutomaticChannelRole::Connector
    }
}

fn validate_source_endpoint(endpoint: SocketAddr) -> io::Result<()> {
    if endpoint.port() == 0 || !is_private_or_loopback(endpoint.ip()) {
        return Err(invalid_data(
            "automatic cross-screen source endpoint must be private and use a non-zero port",
        ));
    }
    Ok(())
}

fn is_private_or_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.is_loopback() || address.is_unique_local(),
    }
}

/// Runs the automatic trusted-peer channel diagnostic without suppressing or injecting input.
///
/// Existing product settings provide both endpoints and the active trusted peer. The diagnostic
/// waits for the other device, samples the local physical cursor after mutual TLS succeeds, and
/// never passes received positions to `SendInput`.
///
/// # Errors
///
/// Returns an error for invalid bounds, missing product settings, trust or display failures, a
/// connection deadline, or local queue failure.
pub fn run_automatic_channel_diagnostic(
    options: &AutomaticChannelDiagnosticOptions,
) -> io::Result<AutomaticChannelDiagnosticReport> {
    if options.connect_timeout.is_zero() || options.connect_timeout > MAX_DIAGNOSTIC_LIFETIME {
        return Err(invalid_data(
            "automatic cross-screen connect timeout must be between 1 millisecond and 300 seconds",
        ));
    }
    if options.lifetime.is_zero() || options.lifetime > MAX_DIAGNOSTIC_LIFETIME {
        return Err(invalid_data(
            "automatic cross-screen lifetime must be between 1 millisecond and 300 seconds",
        ));
    }
    let settings = AppSettings::load(&options.store)?;
    let config = AutomaticChannelConfig::from_settings(&options.store, &settings)?;
    let policy = automatic_diagnostic_control_policy(config.role);
    let mut runtime = CrossScreenRuntime::start_with_control_attempt_timeout(
        options.store.clone(),
        &settings,
        CONNECTION_ATTEMPT_TIMEOUT,
        policy,
    )?;
    if let Err(error) = wait_for_control_ready(&runtime, options.connect_timeout) {
        runtime.stop();
        return Err(error);
    }
    let sample_result = sample_mouse_positions(&runtime, options.lifetime);
    runtime.stop();
    let sampled_mouse_positions = sample_result?;
    let snapshot = runtime.snapshot();
    if snapshot.successful_connections == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "automatic cross-screen channel never authenticated",
        ));
    }
    Ok(AutomaticChannelDiagnosticReport {
        sampled_mouse_positions,
        snapshot,
    })
}

fn wait_for_control_ready(runtime: &CrossScreenRuntime, timeout: Duration) -> io::Result<()> {
    let started = Instant::now();
    loop {
        let snapshot = runtime.snapshot();
        if snapshot.state == CrossScreenRuntimeState::Connected && snapshot.control.ready {
            return Ok(());
        }
        if snapshot.state == CrossScreenRuntimeState::Failed {
            return Err(io::Error::other(snapshot.last_error.unwrap_or_else(|| {
                "automatic cross-screen runtime failed".to_owned()
            })));
        }
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                snapshot.last_error.map_or_else(
                    || {
                        "timed out waiting for the paired device's automatic cross-screen control handshake"
                            .to_owned()
                    },
                    |error| format!("timed out waiting for paired device: {error}"),
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

const fn automatic_diagnostic_control_policy(role: AutomaticChannelRole) -> BoundaryPolicy {
    BoundaryPolicy {
        peer_placement: match role {
            AutomaticChannelRole::Listener => PeerPlacement::Right,
            AutomaticChannelRole::Connector => PeerPlacement::Left,
        },
        corner_guard_pixels: 32,
        require_second_push: false,
    }
}

fn sample_mouse_positions(runtime: &CrossScreenRuntime, lifetime: Duration) -> io::Result<u64> {
    let mut screen = PhysicalPrimaryScreen::current()?;
    let started = Instant::now();
    let mut next_screen_refresh = started + Duration::from_secs(1);
    let mut previous = None;
    let mut sampled_mouse_positions = 0_u64;
    while started.elapsed() < lifetime {
        if Instant::now() >= next_screen_refresh {
            screen = PhysicalPrimaryScreen::current()?;
            next_screen_refresh = Instant::now() + Duration::from_secs(1);
        }
        if runtime.snapshot().state == CrossScreenRuntimeState::Connected {
            match screen.sample_cursor() {
                Ok(Some(position)) if previous != Some(position) => {
                    match runtime.send_mouse_position(position) {
                        Ok(()) => {
                            previous = Some(position);
                            sampled_mouse_positions += 1;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                            previous = None;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(_) => {}
                Err(error) => return Err(error),
            }
        } else {
            previous = None;
        }
        std::thread::sleep(MOUSE_SAMPLE_INTERVAL.min(lifetime.saturating_sub(started.elapsed())));
    }
    Ok(sampled_mouse_positions)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
    static NETWORK_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pierdesk-cross-screen-runtime-{label}-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn trusted_pair() -> (
        TemporaryDirectory,
        DeviceStore,
        AppSettings,
        TemporaryDirectory,
        DeviceStore,
        AppSettings,
    ) {
        let first_directory = TemporaryDirectory::new("first");
        let second_directory = TemporaryDirectory::new("second");
        let first_store = DeviceStore::new(&first_directory.0);
        let second_store = DeviceStore::new(&second_directory.0);
        let first = first_store.load_or_create_identity().unwrap().identity;
        let second = second_store.load_or_create_identity().unwrap().identity;
        let first_fingerprint = first.fingerprint();
        let second_fingerprint = second.fingerprint();
        first_store
            .trust_peer(
                second.certificate_der().to_vec(),
                second_fingerprint,
                "second",
            )
            .unwrap();
        second_store
            .trust_peer(first.certificate_der().to_vec(), first_fingerprint, "first")
            .unwrap();
        let first_settings = AppSettings {
            local_endpoint: "127.0.0.1:45233".parse().unwrap(),
            active_peer: second_fingerprint,
            peer_endpoint: "127.0.0.1:45233".parse().unwrap(),
            auto_receive: true,
        };
        let second_settings = AppSettings {
            local_endpoint: "127.0.0.1:45233".parse().unwrap(),
            active_peer: first_fingerprint,
            peer_endpoint: "127.0.0.1:45233".parse().unwrap(),
            auto_receive: true,
        };
        (
            first_directory,
            first_store,
            first_settings,
            second_directory,
            second_store,
            second_settings,
        )
    }

    fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
        let started = Instant::now();
        while !predicate() {
            assert!(started.elapsed() < timeout, "condition timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const fn control_policy(peer_placement: PeerPlacement) -> BoundaryPolicy {
        BoundaryPolicy {
            peer_placement,
            corner_guard_pixels: 32,
            require_second_push: false,
        }
    }

    #[test]
    fn paired_devices_choose_opposite_roles_and_matching_endpoints() {
        let (
            _first_directory,
            first_store,
            first_settings,
            _second_directory,
            second_store,
            second_settings,
        ) = trusted_pair();
        let first = AutomaticChannelConfig::from_settings(&first_store, &first_settings).unwrap();
        let second =
            AutomaticChannelConfig::from_settings(&second_store, &second_settings).unwrap();
        assert_ne!(first.role, second.role);
        assert_eq!(first.local_endpoint, second.peer_endpoint);
        assert_eq!(first.peer_endpoint, second.local_endpoint);
        assert_eq!(first.local_endpoint.port(), AUTO_CHANNEL_PORT);
        assert_eq!(first.peer_fingerprint, second.local_fingerprint);
        assert_eq!(first.local_fingerprint, second.peer_fingerprint);
    }

    #[test]
    fn automatic_runtime_connects_bidirectionally_without_manual_identity_or_address() {
        let _network_guard = NETWORK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (
            _first_directory,
            first_store,
            first_settings,
            _second_directory,
            second_store,
            second_settings,
        ) = trusted_pair();
        let mut first = CrossScreenRuntime::start_with_attempt_timeout(
            first_store,
            &first_settings,
            Duration::from_secs(1),
        )
        .unwrap();
        let mut second = CrossScreenRuntime::start_with_attempt_timeout(
            second_store,
            &second_settings,
            Duration::from_secs(1),
        )
        .unwrap();
        wait_until(Duration::from_secs(10), || {
            first.snapshot().state == CrossScreenRuntimeState::Connected
                && second.snapshot().state == CrossScreenRuntimeState::Connected
        });
        first
            .send_mouse_position(NormalizedMousePosition { x: 123, y: 456 })
            .unwrap();
        second
            .send_mouse_position(NormalizedMousePosition { x: 789, y: 987 })
            .unwrap();
        wait_until(Duration::from_secs(5), || {
            first
                .snapshot()
                .channel
                .as_ref()
                .is_some_and(|channel| channel.received_mouse_moves >= 1)
                && second
                    .snapshot()
                    .channel
                    .as_ref()
                    .is_some_and(|channel| channel.received_mouse_moves >= 1)
        });
        let first_stop = std::thread::spawn(move || {
            first.stop();
            first.snapshot()
        });
        let second_stop = std::thread::spawn(move || {
            second.stop();
            second.snapshot()
        });
        let first = first_stop.join().unwrap();
        let second = second_stop.join().unwrap();
        assert_eq!(first.state, CrossScreenRuntimeState::Stopped);
        assert_eq!(second.state, CrossScreenRuntimeState::Stopped);
        assert!(first.clean_shutdown);
        assert!(second.clean_shutdown);
        let first_channel = first.channel.unwrap();
        let second_channel = second.channel.unwrap();
        assert!(first_channel.clean_shutdown);
        assert!(second_channel.clean_shutdown);
        assert!(first_channel.sent_release_all >= 1);
        assert!(second_channel.sent_release_all >= 1);
        assert!(first_channel.received_release_all >= 1);
        assert!(second_channel.received_release_all >= 1);
        assert_eq!(first_channel.session_id, second_channel.session_id);
        assert_eq!(first_channel.channel_epoch, second_channel.channel_epoch);
    }

    #[test]
    fn automatic_runtime_exchanges_physical_screens_and_resets_control_on_stop() {
        let _network_guard = NETWORK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (
            _first_directory,
            first_store,
            first_settings,
            _second_directory,
            second_store,
            second_settings,
        ) = trusted_pair();
        let mut first = CrossScreenRuntime::start_with_control_attempt_timeout(
            first_store,
            &first_settings,
            Duration::from_secs(1),
            control_policy(PeerPlacement::Right),
        )
        .unwrap();
        let mut second = CrossScreenRuntime::start_with_control_attempt_timeout(
            second_store,
            &second_settings,
            Duration::from_secs(1),
            control_policy(PeerPlacement::Left),
        )
        .unwrap();
        wait_until(Duration::from_secs(10), || {
            first.snapshot().control.ready && second.snapshot().control.ready
        });
        let first_connected = first.snapshot();
        let second_connected = second.snapshot();
        assert_eq!(first_connected.control.state, ControlProtocolState::Local);
        assert_eq!(second_connected.control.state, ControlProtocolState::Local);
        assert_eq!(
            first_connected.control.peer_screen,
            second_connected.control.local_screen
        );
        assert_eq!(
            second_connected.control.peer_screen,
            first_connected.control.local_screen
        );
        assert_eq!(first_connected.control.successful_handshakes, 1);
        assert_eq!(second_connected.control.successful_handshakes, 1);

        let first_stop = std::thread::spawn(move || {
            first.stop();
            first.snapshot()
        });
        let second_stop = std::thread::spawn(move || {
            second.stop();
            second.snapshot()
        });
        let first_stopped = first_stop.join().unwrap();
        let second_stopped = second_stop.join().unwrap();
        for snapshot in [&first_stopped, &second_stopped] {
            assert!(!snapshot.control.ready);
            assert_eq!(snapshot.control.state, ControlProtocolState::Local);
            assert!(snapshot.control.forced_local_resets >= 1);
            let channel = snapshot.channel.as_ref().unwrap();
            assert!(channel.sent_control_messages >= 1);
            assert!(channel.received_control_messages >= 1);
        }
    }

    #[test]
    fn automatic_runtime_reconnects_with_a_fresh_bound_session() {
        let _network_guard = NETWORK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (
            _first_directory,
            first_store,
            first_settings,
            _second_directory,
            second_store,
            second_settings,
        ) = trusted_pair();
        let second_restart_store = second_store.clone();
        let second_restart_settings = second_settings.clone();
        let mut first = CrossScreenRuntime::start_with_control_attempt_timeout(
            first_store,
            &first_settings,
            Duration::from_secs(1),
            control_policy(PeerPlacement::Right),
        )
        .unwrap();
        let mut second = CrossScreenRuntime::start_with_control_attempt_timeout(
            second_store,
            &second_settings,
            Duration::from_secs(1),
            control_policy(PeerPlacement::Left),
        )
        .unwrap();
        wait_until(Duration::from_secs(10), || {
            first.snapshot().state == CrossScreenRuntimeState::Connected
                && second.snapshot().state == CrossScreenRuntimeState::Connected
                && first.snapshot().control.ready
                && second.snapshot().control.ready
        });
        let original = first.snapshot().channel.unwrap();
        second.stop();
        second = CrossScreenRuntime::start_with_control_attempt_timeout(
            second_restart_store,
            &second_restart_settings,
            Duration::from_secs(1),
            control_policy(PeerPlacement::Left),
        )
        .unwrap();
        let reconnect_started = Instant::now();
        loop {
            let first_snapshot = first.snapshot();
            let second_snapshot = second.snapshot();
            if first_snapshot.state == CrossScreenRuntimeState::Connected
                && first_snapshot.successful_connections >= 2
                && first_snapshot.control.ready
                && first_snapshot.control.successful_handshakes >= 2
                && second_snapshot.state == CrossScreenRuntimeState::Connected
                && second_snapshot.control.ready
            {
                break;
            }
            assert!(
                reconnect_started.elapsed() < Duration::from_secs(10),
                "reconnect timed out: first={first_snapshot:?} second={second_snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let reconnected = first.snapshot().channel.unwrap();
        assert_ne!(original.session_id, reconnected.session_id);
        assert_ne!(original.channel_epoch, reconnected.channel_epoch);
        assert_eq!(first.snapshot().reconnects, 1);

        let first_stop = std::thread::spawn(move || {
            first.stop();
            first.snapshot()
        });
        let second_stop = std::thread::spawn(move || {
            second.stop();
            second.snapshot()
        });
        assert!(first_stop.join().unwrap().clean_shutdown);
        assert!(second_stop.join().unwrap().clean_shutdown);
    }

    #[test]
    fn revoking_trust_ends_the_active_channel_and_prevents_reconnect() {
        let _network_guard = NETWORK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (
            _first_directory,
            first_store,
            first_settings,
            _second_directory,
            second_store,
            second_settings,
        ) = trusted_pair();
        let first_control = first_store.clone();
        let revoked_peer = first_settings.active_peer;
        let mut first = CrossScreenRuntime::start_with_control_attempt_timeout(
            first_store,
            &first_settings,
            Duration::from_secs(1),
            control_policy(PeerPlacement::Right),
        )
        .unwrap();
        let mut second = CrossScreenRuntime::start_with_control_attempt_timeout(
            second_store,
            &second_settings,
            Duration::from_secs(1),
            control_policy(PeerPlacement::Left),
        )
        .unwrap();
        wait_until(Duration::from_secs(10), || {
            first.snapshot().state == CrossScreenRuntimeState::Connected
                && second.snapshot().state == CrossScreenRuntimeState::Connected
                && first.snapshot().control.ready
                && second.snapshot().control.ready
        });
        first_control.revoke_peer(revoked_peer).unwrap();
        wait_until(Duration::from_secs(5), || {
            first.snapshot().state != CrossScreenRuntimeState::Connected
                && second.snapshot().state != CrossScreenRuntimeState::Connected
        });
        std::thread::sleep(Duration::from_millis(500));
        assert_ne!(first.snapshot().state, CrossScreenRuntimeState::Connected);
        assert_eq!(first.snapshot().successful_connections, 1);
        assert!(!first.snapshot().control.ready);
        assert_eq!(first.snapshot().control.state, ControlProtocolState::Local);
        assert!(first.snapshot().control.forced_local_resets >= 1);

        let first_stop = std::thread::spawn(move || {
            first.stop();
            first.snapshot()
        });
        let second_stop = std::thread::spawn(move || {
            second.stop();
            second.snapshot()
        });
        assert!(first_stop.join().unwrap().clean_shutdown);
        assert!(second_stop.join().unwrap().clean_shutdown);
    }
}
