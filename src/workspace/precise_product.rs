//! Product lifecycle for the applied v3 logical boundaries and all direct peer links.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io;
use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, mpsc::sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::app_settings::{AppSettings, CrossScreenSettings};
use crate::clipboard::drag_live::{DragBridge, DragWorker};
use crate::cross_screen::hub::PeerConnectionHub;
use crate::cross_screen::native_input::{
    NativeInputDiagnostics, NativeInputRecoveryReason, NativeInputRuntime,
};
use crate::cross_screen::pair_switch::{
    PairSwitchMessage, PairSwitches, decode_message, encode_message,
};
use crate::cross_screen::{
    AUTO_CHANNEL_PORT, ApplicationProtocol, BoundaryWorkspaceV3, ChannelState,
    CompiledBoundaryLayout, PhysicalScreenSize, ScreenBoundary,
};
use crate::device_store::DeviceStore;
use crate::discovery::DiscoveryView;
use crate::security::CertificateFingerprint;

use super::DeviceId;
use super::control::{BoundaryMappingRef, CrossScreenCoordinator};
use super::input_runtime::{InputBackend, WorkspaceInputRuntime};
use super::persistence::{
    ConnectionSettings, LocalBoundarySettings, PeerConnectionSettings, reserve_control_epoch_range,
};

const POLL: Duration = Duration::from_millis(5);
const DISCOVERY_REFRESH: Duration = Duration::from_millis(250);
const FULLSCREEN_REFRESH: Duration = Duration::from_millis(250);
const DRAG_RECONNECT_GRACE: Duration = Duration::from_secs(5);
const CONTROL_PATH_PROBE_INTERVAL: Duration = Duration::from_secs(1);
const CONTROL_PATH_PROBE_DURATION: Duration = Duration::from_secs(5);
/// How long a pair switch waits for the peer's acknowledgement before both ends keep their state.
const PAIR_ACK_DEADLINE: Duration = Duration::from_secs(2);
/// Interval of the periodic record exchange that converges a missed acknowledgement.
const PAIR_STATUS_INTERVAL: Duration = Duration::from_secs(2);
const MAX_PAIR_MESSAGES_PER_TICK: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreciseProductState {
    Connecting,
    Connected,
    ControllingPeer,
    ControlledByPeer,
    Relaying,
    Paused,
    PausedFullscreen,
    LockedLocal,
    Stopped,
    Failed,
}

#[derive(Clone, Debug)]
pub struct PreciseProductSnapshot {
    pub state: PreciseProductState,
    pub local_screen: PhysicalScreenSize,
    pub peer_count: usize,
    pub online_count: usize,
    pub round_trip_micros: Option<u64>,
    pub native_input: NativeInputDiagnostics,
    pub clean_shutdown: bool,
    pub last_error: Option<String>,
    pub last_control_return_reason: Option<&'static str>,
    pub model: serde_json::Value,
}

struct Session<B: InputBackend> {
    local: DeviceId,
    store: DeviceStore,
    app: AppSettings,
    discovery: DiscoveryView,
    /// The applied layout before per-pair switches; the source for every hot re-filtering.
    base_layout: CompiledBoundaryLayout,
    /// Every applied peer: links stay up for these even while a pair is switched off.
    link_peers: BTreeSet<DeviceId>,
    /// The mirrored switch records of this device, as last committed.
    pairs: PairSwitches,
    pending_switch: Option<PendingPairSwitch>,
    next_request_id: u64,
    next_status: Instant,
    desired: BTreeSet<DeviceId>,
    hub: Arc<PeerConnectionHub>,
    input: WorkspaceInputRuntime<B>,
    drag_edges: BTreeMap<DeviceId, ScreenBoundary>,
    drag_workers: BTreeMap<DeviceId, PeerDragWorker>,
    behavior: LocalBoundarySettings,
    requested_pause: bool,
    locked_local: bool,
    fullscreen: bool,
    next_discovery: Instant,
    next_fullscreen: Instant,
}

/// One pair-switch change requested from the UI thread and answered once it is committed.
struct PairSwitchRequest {
    peer: DeviceId,
    enabled: bool,
    reply: SyncSender<io::Result<()>>,
}

/// A switch this device initiated and is waiting to see acknowledged by the peer.
struct PendingPairSwitch {
    peer: DeviceId,
    request_id: u64,
    deadline: Instant,
    reply: SyncSender<io::Result<()>>,
}

struct PeerDragWorker {
    generation: u64,
    worker: DragWorker,
    reconnect_deadline: Option<Instant>,
}

struct ControlPathProbe {
    session: [u8; 16],
    epoch: u64,
    role: &'static str,
    started: Instant,
    last_emitted: Instant,
    max_loop_gap_us: u64,
    max_tick_us: u64,
}

#[derive(Default)]
struct ControlPathMonitor {
    observed_control: Option<([u8; 16], u64)>,
    probe: Option<ControlPathProbe>,
    previous_loop_started: Option<Instant>,
}

impl ControlPathMonitor {
    fn begin_iteration(&mut self) -> u64 {
        let started = Instant::now();
        let gap_us = self.previous_loop_started.map_or(0, |previous| {
            duration_us(started.saturating_duration_since(previous))
        });
        self.previous_loop_started = Some(started);
        gap_us
    }

    fn after_tick<B: InputBackend>(
        &mut self,
        session: &Session<B>,
        shared: &Shared,
        loop_gap_us: u64,
        tick_us: u64,
    ) {
        let now = Instant::now();
        let active_token = session
            .input
            .token()
            .filter(|token| token.origin != session.local || token.current != session.local);
        let identity = active_token.map(|token| (token.session_id, token.epoch));
        if identity != self.observed_control {
            if let Some(mut probe) = self.probe.take() {
                emit_control_path_probe(session, shared, &mut probe, now);
            }
            self.observed_control = identity;
            if let Some(token) = active_token {
                for link in session.hub.snapshots() {
                    let _ = session.hub.with_peer(link.peer, |channel| {
                        channel.reset_control_cadence();
                        Ok(())
                    });
                }
                self.probe = Some(ControlPathProbe {
                    session: token.session_id,
                    epoch: token.epoch,
                    role: if token.origin == session.local {
                        "source"
                    } else if token.current == session.local {
                        "target"
                    } else {
                        "relay"
                    },
                    started: now,
                    last_emitted: now,
                    max_loop_gap_us: 0,
                    max_tick_us: 0,
                });
            }
        }
        if let Some(probe) = self.probe.as_mut() {
            probe.max_loop_gap_us = probe.max_loop_gap_us.max(loop_gap_us);
            probe.max_tick_us = probe.max_tick_us.max(tick_us);
            if now.saturating_duration_since(probe.last_emitted) >= CONTROL_PATH_PROBE_INTERVAL
                || now.saturating_duration_since(probe.started) >= CONTROL_PATH_PROBE_DURATION
            {
                emit_control_path_probe(session, shared, probe, now);
            }
            if now.saturating_duration_since(probe.started) >= CONTROL_PATH_PROBE_DURATION {
                self.probe = None;
            }
        }
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn emit_control_path_probe<B: InputBackend>(
    session: &Session<B>,
    shared: &Shared,
    probe: &mut ControlPathProbe,
    now: Instant,
) {
    let at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis());
    for link in session.hub.snapshots() {
        let Ok((cadence, reader, tcp, tcp_probe_us, qos)) =
            session.hub.with_peer(link.peer, |channel| {
                let cadence = channel.take_control_cadence();
                let reader = channel.take_reader_stage();
                let started = Instant::now();
                let tcp = channel.tcp_path_snapshot();
                let snapshot = channel.snapshot();
                let qos = if snapshot.latency_qos_enabled {
                    "control".to_owned()
                } else {
                    snapshot
                        .latency_qos_error
                        .unwrap_or_else(|| "none".to_owned())
                        .replace(' ', "_")
                };
                Ok((cadence, reader, tcp, duration_us(started.elapsed()), qos))
            })
        else {
            continue;
        };
        // A complete receive stall has zero control messages in this window.
        // Still sample the active sockets so TCP and reader-thread evidence is
        // preserved at the exact point where the older cadence probe went quiet.
        let peer = u64::from_le_bytes(
            link.peer[..8]
                .try_into()
                .expect("device id has eight bytes"),
        );
        let tcp_detail = match tcp {
            Ok(tcp) => format!(
                "tcp_info=ok tcp_bytes_out={} tcp_bytes_in={} tcp_bytes_retrans={} tcp_timeout_episodes={} tcp_rtt_us={} tcp_bytes_in_flight={} tcp_receive_window={} tcp_pending_bytes={:?}",
                tcp.bytes_out,
                tcp.bytes_in,
                tcp.bytes_retrans,
                tcp.timeout_episodes,
                tcp.rtt_us,
                tcp.bytes_in_flight,
                tcp.receive_window,
                tcp.pending_bytes,
            ),
            Err(error) => format!("tcp_info=error:{}", error.raw_os_error().unwrap_or(-1)),
        };
        let event = format!(
            "event=control_path_probe at_unix_ms={at_unix_ms} session={:032x} epoch={} role={} peer={peer:016x} elapsed_ms={} window_ms={} enqueue_count={} enqueue_gap_max_us={} write_count={} write_gap_max_us={} read_count={} read_gap_max_us={} dequeue_count={} dequeue_gap_max_us={} owner_loop_gap_max_us={} owner_tick_max_us={} reader_socket_reads={} reader_socket_timeouts={} reader_socket_bytes={} reader_socket_wait_max_us={} reader_outside_read_max_us={} reader_tls_max_us={} reader_dispatch_max_us={} tcp_probe_us={} qos={qos} {tcp_detail}",
            u128::from_le_bytes(probe.session),
            probe.epoch,
            probe.role,
            now.saturating_duration_since(probe.started).as_millis(),
            now.saturating_duration_since(probe.last_emitted)
                .as_millis(),
            cadence.enqueued.moves,
            cadence.enqueued.max_gap_micros,
            cadence.sent.moves,
            cadence.sent.max_gap_micros,
            cadence.received.moves,
            cadence.received.max_gap_micros,
            cadence.dequeued.moves,
            cadence.dequeued.max_gap_micros,
            probe.max_loop_gap_us,
            probe.max_tick_us,
            reader.socket_reads,
            reader.socket_timeouts,
            reader.socket_bytes,
            reader.socket_wait_max_us,
            reader.outside_read_max_us,
            reader.tls_max_us,
            reader.dispatch_max_us,
            tcp_probe_us,
        );
        let mut queue = shared
            .probe_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.len() == 128 {
            queue.pop_front();
        }
        queue.push_back(event);
    }
    probe.last_emitted = now;
    probe.max_loop_gap_us = 0;
    probe.max_tick_us = 0;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragWorkerReconciliation {
    Keep,
    Rebind(u64),
    WaitUntil(Instant),
    Drop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragControlRoute {
    Local,
    ControllingPeer,
    ControlledByPeer,
    Relaying,
}

impl<B: InputBackend> Session<B> {
    fn new(
        store: &DeviceStore,
        app: AppSettings,
        settings: &CrossScreenSettings,
        discovery: DiscoveryView,
        layout: CompiledBoundaryLayout,
        backend: B,
    ) -> io::Result<Self> {
        let local = store.load_identity()?.fingerprint().bytes();
        let base_layout = layout.clone();
        let pairs = PairSwitches::load_or_default(store)?;
        let layout = base_layout.without_peers(&disabled_set(&pairs.disabled_peers()));
        let mappings = boundary_mappings(&layout)?;
        let drag_edges = drag_edges(&layout)?;
        let desired = mappings.keys().copied().collect::<BTreeSet<_>>();
        // Every applied peer keeps its authenticated link, even while switched off, so that the
        // pair can still be switched back on. Routing uses `desired`, never this set.
        let link_peers = base_layout_peers(&base_layout)?;
        let behavior = LocalBoundarySettings {
            schema: 2,
            enabled: settings.enabled,
            corner_guard_enabled: settings.corner_guard_enabled,
            corner_guard_pixels: settings.corner_guard_pixels,
            require_second_push: settings.require_second_push,
            pause_on_fullscreen: settings.pause_on_fullscreen,
        };
        let connection_settings = ConnectionSettings {
            schema: 2,
            local_endpoint: app.local_endpoint,
            auto_receive: app.auto_receive,
            peers: initial_peers(&app, &discovery, &link_peers),
        };
        let hub = Arc::new(PeerConnectionHub::start(
            store.clone(),
            &connection_settings,
        )?);
        let epoch_range = reserve_control_epoch_range(store)?;
        let coordinator = CrossScreenCoordinator::new_precise_with_epoch_range(
            local,
            mappings,
            epoch_range.floor,
            epoch_range.limit,
        )?;
        let input =
            WorkspaceInputRuntime::new_precise(local, coordinator, layout, &behavior, backend)?;
        Ok(Self {
            local,
            store: store.clone(),
            app,
            discovery,
            base_layout,
            link_peers,
            pairs,
            pending_switch: None,
            next_request_id: 0,
            next_status: Instant::now(),
            desired,
            hub,
            input,
            drag_edges,
            drag_workers: BTreeMap::new(),
            behavior,
            requested_pause: false,
            locked_local: false,
            fullscreen: false,
            next_discovery: Instant::now(),
            next_fullscreen: Instant::now(),
        })
    }

    /// Applies a new set of switched-off pairs to the running session.
    ///
    /// A switched-off pair leaves the coordinator, the pointer router and the drag bridges, and
    /// releases control first if it was in the route. Its authenticated link stays up for the
    /// switch protocol. Other pairs keep their links, epochs and sessions. The applied boundary
    /// layout itself is never changed here.
    ///
    /// # Errors
    /// Rejects a pair that is dragging right now, and reports a failed native release.
    fn apply_disabled_pairs(&mut self, disabled: BTreeSet<DeviceId>) -> io::Result<()> {
        let layout = self.base_layout.without_peers(&disabled_set(&disabled));
        let mappings = boundary_mappings(&layout)?;
        let drag_edges = drag_edges(&layout)?;
        let desired = mappings.keys().copied().collect::<BTreeSet<_>>();
        if self
            .desired
            .difference(&desired)
            .any(|peer| self.drag_active(*peer))
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "这台设备正在拖拽文件，稍后再切换跨屏开关",
            ));
        }
        let require_second_push = self.behavior.require_second_push;
        self.input
            .replace_precise_layout(&self.hub, layout, mappings, require_second_push)?;
        self.drag_workers.retain(|peer, _| desired.contains(peer));
        self.desired = desired;
        self.drag_edges = drag_edges;
        Ok(())
    }

    /// Commits a complete new switch record: the running session first, then the file.
    ///
    /// A failed runtime change leaves both the session and the file unchanged. A failed save
    /// restores the previous runtime shape.
    fn commit_pairs(&mut self, next: PairSwitches) -> io::Result<()> {
        self.apply_disabled_pairs(next.disabled_peers())?;
        if let Err(error) = next.save(&self.store) {
            let _ = self.apply_disabled_pairs(self.pairs.disabled_peers());
            return Err(error);
        }
        self.pairs = next;
        Ok(())
    }

    /// True only while a drag with this peer is live. Idle drag workers exist for every connected
    /// peer with a drag edge, so their presence alone does not mean a drag is in progress.
    fn drag_active(&self, peer: DeviceId) -> bool {
        self.drag_workers
            .get(&peer)
            .is_some_and(|worker| worker.worker.0.has_live_session())
    }

    fn peer_connected(&self, peer: DeviceId) -> bool {
        self.hub.snapshots().iter().any(|link| {
            link.peer == peer
                && link
                    .channel
                    .as_ref()
                    .is_some_and(|channel| channel.state == ChannelState::Connected)
        })
    }

    fn send_pair_message(&self, peer: DeviceId, message: &PairSwitchMessage) -> io::Result<()> {
        let bytes = encode_message(message)?;
        self.hub.with_peer(peer, |channel| {
            channel.send_application(ApplicationProtocol::PairSwitch, bytes)
        })
    }

    /// Starts a switch initiated on this device. Nothing is committed until the peer acknowledges.
    fn begin_pair_switch(&mut self, request: PairSwitchRequest, now: Instant) {
        let PairSwitchRequest {
            peer,
            enabled,
            reply,
        } = request;
        let current = self.pairs.record(peer);
        if current.enabled == enabled {
            let _ = reply.send(Ok(()));
            return;
        }
        match self.start_pair_switch(peer, enabled, current.revision) {
            Ok(request_id) => {
                self.pending_switch = Some(PendingPairSwitch {
                    peer,
                    request_id,
                    deadline: now + PAIR_ACK_DEADLINE,
                    reply,
                });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn start_pair_switch(
        &mut self,
        peer: DeviceId,
        enabled: bool,
        revision: u64,
    ) -> io::Result<u64> {
        if self.pending_switch.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "已有一个跨屏开关正在切换，请稍后再试",
            ));
        }
        if !self.link_peers.contains(&peer) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "这对电脑没有已应用的跨屏边界",
            ));
        }
        if !self.peer_connected(peer) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "对端离线，无法切换；请让两台电脑同时在线",
            ));
        }
        if self.drag_active(peer) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "这台设备正在拖拽文件，稍后再切换跨屏开关",
            ));
        }
        if self
            .input
            .token()
            .is_some_and(|token| token.route.contains(&peer))
        {
            self.input.return_local(&self.hub)?;
        }
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let request_id = self.next_request_id;
        self.send_pair_message(
            peer,
            &PairSwitchMessage::Request {
                request_id,
                enabled,
                expected_revision: revision,
            },
        )?;
        Ok(request_id)
    }

    /// Drains bounded inbound switch messages, answers requests, and keeps records aligned.
    fn pump_pair_switches(&mut self, now: Instant) {
        if self
            .pending_switch
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
            && let Some(pending) = self.pending_switch.take()
        {
            let _ = pending.reply.send(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "对端没有及时确认跨屏开关；稍后查看状态，两端会自动同步到同一结果",
            )));
        }
        let peers = self.link_peers.iter().copied().collect::<Vec<_>>();
        for peer in &peers {
            for _ in 0..MAX_PAIR_MESSAGES_PER_TICK {
                let Ok(Some(bytes)) = self.hub.with_peer(*peer, |channel| {
                    channel.try_receive_application(ApplicationProtocol::PairSwitch)
                }) else {
                    break;
                };
                if let Ok(message) = decode_message(&bytes) {
                    self.handle_pair_message(*peer, message);
                }
            }
        }
        if now >= self.next_status {
            self.next_status = now + PAIR_STATUS_INTERVAL;
            for peer in peers.into_iter().filter(|peer| self.peer_connected(*peer)) {
                let record = self.pairs.record(peer);
                let _ = self.send_pair_message(peer, &PairSwitchMessage::Status { record });
            }
        }
    }

    fn handle_pair_message(&mut self, peer: DeviceId, message: PairSwitchMessage) {
        match message {
            PairSwitchMessage::Request {
                request_id,
                enabled,
                expected_revision,
            } => {
                let busy = self
                    .pending_switch
                    .as_ref()
                    .is_some_and(|pending| pending.peer == peer);
                let committed = if busy {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
                } else {
                    self.accept_pair_request(peer, enabled, expected_revision)
                };
                let record = self.pairs.record(peer);
                let _ = self.send_pair_message(
                    peer,
                    &PairSwitchMessage::Ack {
                        request_id,
                        accepted: committed.is_ok(),
                        record,
                    },
                );
            }
            PairSwitchMessage::Ack {
                request_id,
                accepted,
                record,
            } => {
                let matches = self.pending_switch.as_ref().is_some_and(|pending| {
                    pending.peer == peer && pending.request_id == request_id
                });
                if !matches {
                    return;
                }
                let Some(pending) = self.pending_switch.take() else {
                    return;
                };
                let outcome = if accepted {
                    let mut next = self.pairs.clone();
                    next.adopt_if_newer(peer, record)
                        .and_then(|_| self.commit_pairs(next))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "对端拒绝了这次跨屏开关",
                    ))
                };
                let _ = pending.reply.send(outcome);
            }
            PairSwitchMessage::Status { record } => {
                if record.revision > self.pairs.record(peer).revision {
                    let mut next = self.pairs.clone();
                    if next.adopt_if_newer(peer, record).unwrap_or(false) {
                        let _ = self.commit_pairs(next);
                    }
                }
            }
        }
    }

    /// Applies a request from the peer, but only against the revision this device stores.
    fn accept_pair_request(
        &mut self,
        peer: DeviceId,
        enabled: bool,
        expected_revision: u64,
    ) -> io::Result<()> {
        if !self.link_peers.contains(&peer) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "这对电脑没有已应用的跨屏边界",
            ));
        }
        let mut next = self.pairs.clone();
        next.set_enabled(peer, enabled, expected_revision)?;
        self.commit_pairs(next)
    }
}

fn base_layout_peers(layout: &CompiledBoundaryLayout) -> io::Result<BTreeSet<DeviceId>> {
    layout
        .boundaries()
        .iter()
        .map(|boundary| Ok(CertificateFingerprint::from_str(boundary.peer_fingerprint())?.bytes()))
        .collect()
}

impl<B: InputBackend> Session<B> {
    fn tick(&mut self, requested_pause: bool, return_local: bool) -> io::Result<()> {
        let now = Instant::now();
        if return_local {
            self.locked_local = true;
            self.input.return_local(&self.hub)?;
        }
        if now >= self.next_discovery {
            self.hub.upsert_peers(discovered_endpoints(
                &self.app,
                &self.discovery,
                &self.link_peers,
            ))?;
            self.next_discovery = now.checked_add(DISCOVERY_REFRESH).unwrap_or(now);
        }
        let links = self.hub.snapshots();
        self.reconcile_drag_workers(&links)?;
        if now >= self.next_fullscreen {
            self.fullscreen = self.behavior.pause_on_fullscreen && self.input.fullscreen()?;
            self.next_fullscreen = now.checked_add(FULLSCREEN_REFRESH).unwrap_or(now);
        }
        let paused =
            requested_pause || self.locked_local || self.fullscreen || !self.behavior.enabled;
        if self.requested_pause != paused {
            self.input.set_paused(&self.hub, paused)?;
            self.requested_pause = paused;
        }
        let ready_peers = links
            .iter()
            .filter_map(|link| {
                link.channel
                    .as_ref()
                    .is_some_and(|channel| channel.state == ChannelState::Connected)
                    .then_some(link.peer)
            })
            .collect::<BTreeSet<_>>();
        let ready = |peer| ready_peers.contains(&peer);
        if let Err(error) = self.input.tick(&self.hub, now, &ready)
            && !peer_connection_changed(&error)
        {
            return Err(error);
        }
        self.pump_drag_bridges()
    }

    fn reconcile_drag_workers(
        &mut self,
        links: &[crate::cross_screen::hub::PeerLinkSnapshot],
    ) -> io::Result<()> {
        let now = Instant::now();
        let connected_generations = links
            .iter()
            .filter_map(|link| {
                link.channel
                    .as_ref()
                    .filter(|channel| channel.state == ChannelState::Connected)
                    .map(|_| (link.peer, link.generation))
            })
            .collect::<BTreeMap<_, _>>();
        let mut stale = Vec::new();
        for (peer, worker) in &mut self.drag_workers {
            let connected_generation = connected_generations.get(peer).copied();
            let bridge = &worker.worker.0;
            match reconcile_drag_worker(
                worker.generation,
                connected_generation,
                bridge.has_live_session(),
                bridge.is_stopped(),
                worker.reconnect_deadline,
                now,
            ) {
                DragWorkerReconciliation::Keep => {
                    worker.reconnect_deadline = None;
                }
                DragWorkerReconciliation::Rebind(generation) => {
                    worker.generation = generation;
                    worker.reconnect_deadline = None;
                }
                DragWorkerReconciliation::WaitUntil(deadline) => {
                    worker.reconnect_deadline = Some(deadline);
                }
                DragWorkerReconciliation::Drop => stale.push(*peer),
            }
        }
        for peer in stale {
            self.input.unregister_drag_bridge(peer);
            self.drag_workers.remove(&peer);
        }
        for link in links {
            if self.drag_workers.contains_key(&link.peer) {
                continue;
            }
            let Some(edge) = self.drag_edges.get(&link.peer).copied() else {
                continue;
            };
            let Some(channel) = link
                .channel
                .clone()
                .filter(|channel| channel.state == ChannelState::Connected)
            else {
                continue;
            };
            let (bridge, worker) = DragBridge::start(
                self.store.clone(),
                CertificateFingerprint::from_bytes(link.peer),
                channel.local_address,
                channel.peer_address,
                edge,
            )?;
            self.input
                .register_drag_bridge(link.peer, Arc::clone(&bridge));
            self.drag_workers.insert(
                link.peer,
                PeerDragWorker {
                    generation: link.generation,
                    worker: DragWorker(bridge, Some(worker)),
                    reconnect_deadline: None,
                },
            );
        }
        Ok(())
    }

    fn pump_drag_bridges(&self) -> io::Result<()> {
        let token = self.input.token().cloned();
        let physical_left = self.input.physical_left_down();
        let remote_left = self.input.remote_left_down();
        for (peer, worker) in &self.drag_workers {
            let bridge = &worker.worker.0;
            let local_route = token.is_none();
            let controlling_this = token
                .as_ref()
                .is_some_and(|token| token.origin == self.local && token.current == *peer);
            let controlled_by_this = token
                .as_ref()
                .is_some_and(|token| token.origin == *peer && token.current == self.local);
            let control_route = if local_route {
                DragControlRoute::Local
            } else if controlling_this {
                DragControlRoute::ControllingPeer
            } else if controlled_by_this {
                DragControlRoute::ControlledByPeer
            } else {
                DragControlRoute::Relaying
            };
            if bridge.source_id.load(Ordering::Acquire) == 0 {
                bridge.classify_idle_source(controlled_by_this);
            }
            bridge.boundary_visible.store(
                drag_boundary_visible(
                    control_route,
                    physical_left,
                    remote_left,
                    bridge.retains_active_source(),
                ),
                Ordering::Release,
            );
            bridge.boundary_needs_motion.store(
                drag_boundary_needs_motion(control_route, bridge.retains_active_source()),
                Ordering::Release,
            );
            let transfer = self.hub.with_peer(*peer, |channel| {
                while let Some(message) = channel.try_receive_file_drag()? {
                    bridge.receive_for_route(
                        message,
                        local_route || controlling_this,
                        controlling_this,
                    );
                }
                while let Some(message) = bridge.poll() {
                    channel.send_file_drag(message)?;
                }
                Ok(())
            });
            if let Err(error) = transfer {
                if peer_connection_changed(&error) {
                    continue;
                }
                return Err(error);
            }
            if !controlled_by_this {
                bridge.discard_handed_off_remote_left_up();
            }
            if local_route && !physical_left {
                let _ = bridge.may_cross(false);
            }
            bridge.expire_remote_source_left_up();
        }
        Ok(())
    }

    fn resume(&mut self) {
        self.locked_local = false;
    }

    fn snapshot(&self, clean_shutdown: bool, last_error: Option<String>) -> PreciseProductSnapshot {
        let links = self.hub.snapshots();
        let online = links
            .iter()
            .filter(|link| {
                link.channel
                    .as_ref()
                    .is_some_and(|channel| channel.state == ChannelState::Connected)
            })
            .count();
        let round_trip_micros = links
            .iter()
            .filter_map(|link| link.channel.as_ref()?.round_trip_micros)
            .min();
        let token = self.input.token();
        let state = if last_error.is_some() {
            PreciseProductState::Failed
        } else if self.locked_local {
            PreciseProductState::LockedLocal
        } else if self.fullscreen {
            PreciseProductState::PausedFullscreen
        } else if self.requested_pause {
            PreciseProductState::Paused
        } else if let Some(token) = token {
            if token.origin == self.local {
                PreciseProductState::ControllingPeer
            } else if token.current == self.local {
                PreciseProductState::ControlledByPeer
            } else {
                PreciseProductState::Relaying
            }
        } else if online == 0 {
            PreciseProductState::Connecting
        } else {
            PreciseProductState::Connected
        };
        let fingerprint = |id| CertificateFingerprint::from_bytes(id).to_string();
        let native_input = self.input.diagnostics();
        PreciseProductSnapshot {
            state,
            local_screen: self.input.screen(),
            peer_count: self.desired.len(),
            online_count: online,
            round_trip_micros,
            native_input,
            clean_shutdown,
            last_error: last_error
                .clone()
                .or_else(|| self.input.last_error().map(str::to_owned)),
            last_control_return_reason: self.input.last_control_return_reason(),
            model: serde_json::json!({
                "state": format!("{state:?}").to_ascii_lowercase(),
                "connections": self.hub.settings_model(),
                "control": token.map(|token| serde_json::json!({
                    "origin": fingerprint(token.origin),
                    "current": fingerprint(token.current),
                    "epoch": token.epoch,
                    "route": token.route.iter().copied().map(fingerprint).collect::<Vec<_>>(),
                })),
                "lastError": last_error.or_else(|| self.input.last_error().map(str::to_owned)),
                "lastControlReturnReason": self.input.last_control_return_reason(),
                "nativeInput": {
                    "peakQueuedEvents": native_input.peak_queued_events,
                    "droppedMotionEvents": native_input.dropped_motion_events,
                    "recoveryReturns": native_input.recovery_returns,
                    "contentionReturns": native_input.contention_returns,
                    "overflowReturns": native_input.overflow_returns,
                    "targetRawEchoFiltered": native_input.target_raw_echo_filtered,
                    "targetRawAfterGrace": native_input.target_raw_after_grace,
                    "targetRawWithoutInjection": native_input.target_raw_without_injection,
                    "maxTargetRawAfterGraceMicros": native_input.max_target_raw_after_grace_micros,
                    "targetTakeoverReturns": native_input.target_takeover_returns,
                    "slowInjectionCommands": native_input.slow_injection_commands,
                    "maxInjectionCommandMicros": native_input.max_injection_command_micros,
                    "sourceCursorVisibleSamples": native_input.source_cursor_visible_samples,
                    "sourceCursorVisibleAfterRepair": native_input.source_cursor_visible_after_repair,
                    "sourceCursorProbeFailures": native_input.source_cursor_probe_failures,
                    "lastRecovery": native_input
                        .last_recovery
                        .map(NativeInputRecoveryReason::label),
                },
            }),
        }
    }
}

fn peer_connection_changed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
    )
}

fn reconcile_drag_worker(
    worker_generation: u64,
    connected_generation: Option<u64>,
    has_live_session: bool,
    stopped: bool,
    reconnect_deadline: Option<Instant>,
    now: Instant,
) -> DragWorkerReconciliation {
    if connected_generation == Some(worker_generation) {
        return DragWorkerReconciliation::Keep;
    }
    if !has_live_session || stopped {
        return DragWorkerReconciliation::Drop;
    }
    if let Some(generation) = connected_generation {
        return DragWorkerReconciliation::Rebind(generation);
    }
    let deadline =
        reconnect_deadline.unwrap_or_else(|| now.checked_add(DRAG_RECONNECT_GRACE).unwrap_or(now));
    if now < deadline {
        DragWorkerReconciliation::WaitUntil(deadline)
    } else {
        DragWorkerReconciliation::Drop
    }
}

const fn drag_boundary_visible(
    control_route: DragControlRoute,
    physical_left: bool,
    remote_left: bool,
    retained_source_active: bool,
) -> bool {
    match control_route {
        DragControlRoute::Local => physical_left,
        // The physical pointer is confined at the source edge while controlling a peer.
        // Showing an idle OLE receiver there steals cursor ownership from the source
        // shield on every remote click. Only an actual retained cross-screen drag
        // needs to keep its source receiver visible; ordinary clicks do not.
        DragControlRoute::ControllingPeer => physical_left && retained_source_active,
        DragControlRoute::ControlledByPeer => remote_left,
        DragControlRoute::Relaying => false,
    }
}

/// A controlled peer's boundary window only appears once the press has moved, so a plain
/// remote click on the taskbar cannot reveal the topmost receiver. A retained real drag keeps
/// it immediately, because its return path must already be ready.
const fn drag_boundary_needs_motion(
    control_route: DragControlRoute,
    retained_source_active: bool,
) -> bool {
    matches!(control_route, DragControlRoute::ControlledByPeer) && !retained_source_active
}

struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    return_local: AtomicBool,
    resume: AtomicBool,
    snapshot: Mutex<PreciseProductSnapshot>,
    probe_events: Mutex<VecDeque<String>>,
    pair_switches: Mutex<VecDeque<PairSwitchRequest>>,
    wake_generation: Mutex<u64>,
    wake: Condvar,
}

/// How long the UI thread waits for the workspace thread to apply one pair switch.
const PAIR_SWITCH_TIMEOUT: Duration = Duration::from_secs(3);

impl Shared {
    fn signal(&self) {
        let mut generation = self
            .wake_generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *generation = generation.wrapping_add(1);
        drop(generation);
        self.wake.notify_all();
    }
}

pub struct PreciseProductRuntime {
    shared: Arc<Shared>,
    hub: Arc<PeerConnectionHub>,
    worker: Option<JoinHandle<()>>,
}

impl PreciseProductRuntime {
    /// Starts one shared multi-peer connection hub and one native input owner.
    ///
    /// # Errors
    /// Rejects missing applied boundaries, invalid peer settings, or native startup failures.
    pub fn start(
        store: &DeviceStore,
        app: AppSettings,
        settings: &CrossScreenSettings,
        discovery: DiscoveryView,
    ) -> io::Result<Self> {
        let workspace = BoundaryWorkspaceV3::load(store)?;
        let applied = workspace
            .applied
            .as_ref()
            .ok_or_else(|| invalid("logical boundary workspace has not been applied"))?;
        let local = store.load_identity()?.fingerprint().to_string();
        if applied.layout.local_fingerprint != local {
            return Err(invalid(
                "applied logical boundary layout belongs to another device",
            ));
        }
        let stored_local_screen = applied
            .layout
            .screens
            .iter()
            .find(|screen| screen.device_fingerprint == local)
            .ok_or_else(|| invalid("applied logical boundary layout is missing the local screen"))?
            .oriented_pixel_size()?;
        let layout = workspace
            .compile_applied()?
            .ok_or_else(|| invalid("logical boundary workspace has not been applied"))?;
        let mut native = NativeInputRuntime::start()?;
        if native.snapshot().screen != stored_local_screen {
            let current = native.snapshot().screen;
            let _ = native.stop();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "applied logical boundary screen size changed: stored={}x{}, current={}x{}",
                    stored_local_screen.width,
                    stored_local_screen.height,
                    current.width,
                    current.height,
                ),
            ));
        }
        let session = Session::new(store, app, settings, discovery, layout, native)?;
        Self::from_session(session)
    }

    fn from_session<B: InputBackend + Send + 'static>(session: Session<B>) -> io::Result<Self> {
        let hub = Arc::clone(&session.hub);
        let initial = session.snapshot(false, None);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            return_local: AtomicBool::new(false),
            resume: AtomicBool::new(false),
            snapshot: Mutex::new(initial),
            probe_events: Mutex::new(VecDeque::new()),
            pair_switches: Mutex::new(VecDeque::new()),
            wake_generation: Mutex::new(0),
            wake: Condvar::new(),
        });
        let native_wake = Arc::clone(&shared);
        session
            .input
            .set_event_notifier(Arc::new(move || native_wake.signal()));
        let network_wake = Arc::clone(&shared);
        session
            .hub
            .set_event_notifier(Arc::new(move || network_wake.signal()));
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("pierdesk-precise-workspace".into())
            .spawn(move || {
                #[cfg(not(test))]
                crate::traffic_control::promote_interactive_thread();
                run(session, &worker_shared);
            })?;
        Ok(Self {
            shared,
            hub,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> PreciseProductSnapshot {
        self.shared
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn drain_diagnostic_traces(&self) -> Vec<String> {
        self.shared
            .probe_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::Release);
        self.shared.signal();
    }

    pub fn resume(&self) {
        self.shared.resume.store(true, Ordering::Release);
        self.shared.paused.store(false, Ordering::Release);
        self.shared.signal();
    }

    pub fn return_local(&self) {
        self.shared.return_local.store(true, Ordering::Release);
        self.shared.signal();
    }

    /// Switches one pair on or off. Both computers must be online; the change is committed on both
    /// ends only after the peer acknowledges it.
    ///
    /// Blocks the caller until the change is committed or refused, so the UI can report the result.
    ///
    /// # Errors
    /// Reports an offline peer, a dragging pair, a switch already in progress, a peer that refused
    /// or did not acknowledge in time, and failed local release or persistence.
    pub fn set_pair_enabled(&self, peer: DeviceId, enabled: bool) -> io::Result<()> {
        let (reply, answer) = sync_channel(1);
        self.shared
            .pair_switches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(PairSwitchRequest {
                peer,
                enabled,
                reply,
            });
        self.shared.signal();
        answer
            .recv_timeout(PAIR_SWITCH_TIMEOUT)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "跨屏控制没有及时响应开关变更"))?
    }

    /// Stops input capture after a synchronous release barrier.
    ///
    /// # Errors
    /// Reports worker panic or incomplete native input release.
    pub fn stop(&mut self) -> io::Result<()> {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.signal();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("precise workspace worker panicked"))?;
        }
        self.hub.shutdown();
        let snapshot = self.snapshot();
        if !snapshot.clean_shutdown {
            return Err(io::Error::other(snapshot.last_error.unwrap_or_else(|| {
                "precise workspace shutdown was incomplete".into()
            })));
        }
        Ok(())
    }
}

impl Drop for PreciseProductRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run<B: InputBackend>(mut session: Session<B>, shared: &Shared) {
    let mut failure = None;
    let mut path_monitor = ControlPathMonitor::default();
    while !shared.stop.load(Ordering::Acquire) {
        let loop_gap_us = path_monitor.begin_iteration();
        let observed_generation = *shared
            .wake_generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shared.resume.swap(false, Ordering::AcqRel) {
            session.resume();
        }
        let pair_requests = std::mem::take(
            &mut *shared
                .pair_switches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for request in pair_requests {
            session.begin_pair_switch(request, Instant::now());
        }
        session.pump_pair_switches(Instant::now());
        let tick_started = Instant::now();
        let result = session.tick(
            shared.paused.load(Ordering::Acquire),
            shared.return_local.swap(false, Ordering::AcqRel),
        );
        let tick_us = duration_us(tick_started.elapsed());
        let events = session.input.drain_probe_events();
        if !events.is_empty() {
            let mut queue = shared
                .probe_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for event in events {
                if queue.len() == 128 {
                    queue.pop_front();
                }
                queue.push_back(event);
            }
        }
        if let Err(error) = result {
            failure = Some(error.to_string());
            break;
        }
        path_monitor.after_tick(&session, shared, loop_gap_us, tick_us);
        let snapshot = session.snapshot(false, None);
        {
            let mut guard = shared
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = snapshot;
        }
        let wake_guard = shared
            .wake_generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *wake_guard == observed_generation {
            let _ = shared.wake.wait_timeout(wake_guard, POLL);
        }
    }
    shared.stop.store(true, Ordering::Release);
    let release = session.input.shutdown(&session.hub);
    let release_error = release.err().map(|error| error.to_string());
    let final_error = match (failure, release_error) {
        (Some(primary), Some(cleanup)) => {
            Some(format!("{primary}; shutdown cleanup failed: {cleanup}"))
        }
        (Some(primary), None) => Some(primary),
        (None, Some(cleanup)) => Some(cleanup),
        (None, None) => None,
    };
    let mut snapshot = session.snapshot(final_error.is_none(), final_error.clone());
    snapshot.state = if final_error.is_none() {
        PreciseProductState::Stopped
    } else {
        PreciseProductState::Failed
    };
    snapshot.last_error = final_error.or(snapshot.last_error);
    *shared
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
}

fn boundary_mappings(
    layout: &CompiledBoundaryLayout,
) -> io::Result<BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>> {
    let mut mappings = BTreeMap::<DeviceId, BTreeSet<BoundaryMappingRef>>::new();
    for boundary in layout.boundaries() {
        let peer = CertificateFingerprint::from_str(boundary.peer_fingerprint())?.bytes();
        mappings
            .entry(peer)
            .or_default()
            .insert(BoundaryMappingRef {
                boundary_id: boundary.boundary_id().to_owned(),
                mapping_revision: boundary.mapping_revision(),
            });
    }
    Ok(mappings)
}

fn drag_edges(layout: &CompiledBoundaryLayout) -> io::Result<BTreeMap<DeviceId, ScreenBoundary>> {
    let mut edges = BTreeMap::new();
    let mut unsupported = BTreeSet::new();
    for boundary in layout.boundaries() {
        let peer = CertificateFingerprint::from_str(boundary.peer_fingerprint())?.bytes();
        let mut segments = boundary.source().segments().iter();
        let Some(edge) = segments.next().map(|segment| segment.edge) else {
            continue;
        };
        if segments.any(|segment| segment.edge != edge)
            || edges.get(&peer).is_some_and(|existing| *existing != edge)
        {
            unsupported.insert(peer);
            continue;
        }
        edges.insert(peer, edge);
    }
    for peer in unsupported {
        edges.remove(&peer);
    }
    Ok(edges)
}

fn disabled_set(disabled: &BTreeSet<DeviceId>) -> HashSet<DeviceId> {
    disabled.iter().copied().collect()
}

fn initial_peers(
    app: &AppSettings,
    discovery: &DiscoveryView,
    desired: &BTreeSet<DeviceId>,
) -> Vec<PeerConnectionSettings> {
    discovered_endpoints(app, discovery, desired)
        .into_iter()
        .map(|(peer, endpoint)| PeerConnectionSettings {
            peer,
            last_endpoint: endpoint,
            manual_endpoint: (peer == app.active_peer.bytes()).then_some(app.peer_endpoint),
            enabled: true,
        })
        .collect()
}

fn discovered_endpoints(
    app: &AppSettings,
    discovery: &DiscoveryView,
    desired: &BTreeSet<DeviceId>,
) -> BTreeMap<DeviceId, std::net::SocketAddr> {
    let mut peers = discovery
        .peers()
        .into_iter()
        .filter(|peer| desired.contains(&peer.fingerprint.bytes()))
        .map(|peer| {
            (
                peer.fingerprint.bytes(),
                std::net::SocketAddr::new(peer.service_endpoint.ip(), AUTO_CHANNEL_PORT),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let active = app.active_peer.bytes();
    if desired.contains(&active) {
        peers.entry(active).or_insert_with(|| {
            std::net::SocketAddr::new(app.peer_endpoint.ip(), AUTO_CHANNEL_PORT)
        });
    }
    peers
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_drag_survives_a_brief_disconnect_and_rebinds_after_reconnect() {
        let now = Instant::now();
        let wait = reconcile_drag_worker(7, None, true, false, None, now);
        let DragWorkerReconciliation::WaitUntil(deadline) = wait else {
            panic!("active drag should wait for a reconnect");
        };
        assert_eq!(
            reconcile_drag_worker(7, Some(8), true, false, Some(deadline), now),
            DragWorkerReconciliation::Rebind(8)
        );
        assert_eq!(
            reconcile_drag_worker(7, None, true, false, Some(deadline), deadline),
            DragWorkerReconciliation::Drop
        );
        assert_eq!(
            reconcile_drag_worker(7, Some(8), false, false, None, now),
            DragWorkerReconciliation::Drop
        );
    }

    #[test]
    fn source_drag_remains_visible_while_control_is_on_the_target() {
        assert!(drag_boundary_visible(
            DragControlRoute::ControllingPeer,
            true,
            false,
            true
        ));
        assert!(drag_boundary_visible(
            DragControlRoute::ControlledByPeer,
            false,
            true,
            false
        ));
        assert!(drag_boundary_visible(
            DragControlRoute::Local,
            true,
            false,
            false
        ));
        assert!(!drag_boundary_visible(
            DragControlRoute::Relaying,
            true,
            true,
            true
        ));
    }

    #[test]
    fn remote_clicks_never_show_an_idle_source_receiver() {
        for _click in 0..20 {
            for held in [true, false] {
                assert!(!drag_boundary_visible(
                    DragControlRoute::ControllingPeer,
                    held,
                    false,
                    false,
                ));
            }
        }
    }

    #[test]
    fn drag_receiver_visibility_follows_route_and_release() {
        assert!(drag_boundary_visible(
            DragControlRoute::Local,
            true,
            false,
            false
        ));
        assert!(!drag_boundary_visible(
            DragControlRoute::ControllingPeer,
            true,
            false,
            false
        ));
        assert!(drag_boundary_visible(
            DragControlRoute::ControllingPeer,
            true,
            false,
            true
        ));
        assert!(!drag_boundary_visible(
            DragControlRoute::ControllingPeer,
            false,
            false,
            true
        ));
        // A completed/cancelled retained source must not reopen on the next click.
        assert!(!drag_boundary_visible(
            DragControlRoute::ControllingPeer,
            true,
            false,
            false
        ));
        assert!(drag_boundary_visible(
            DragControlRoute::ControlledByPeer,
            false,
            true,
            false
        ));
        assert!(!drag_boundary_visible(
            DragControlRoute::ControlledByPeer,
            true,
            false,
            true
        ));
        assert!(!drag_boundary_visible(
            DragControlRoute::Relaying,
            true,
            true,
            true
        ));
    }

    #[test]
    fn boundary_needs_motion_only_for_controlled_peer_without_active_source() {
        assert!(drag_boundary_needs_motion(
            DragControlRoute::ControlledByPeer,
            false
        ));
        assert!(!drag_boundary_needs_motion(
            DragControlRoute::ControlledByPeer,
            true
        ));
        assert!(!drag_boundary_needs_motion(
            DragControlRoute::ControllingPeer,
            false
        ));
        assert!(!drag_boundary_needs_motion(DragControlRoute::Local, false));
        assert!(!drag_boundary_needs_motion(
            DragControlRoute::Relaying,
            false
        ));
    }

    #[test]
    fn peer_disconnect_errors_do_not_fail_the_workspace_runtime() {
        for kind in [
            io::ErrorKind::NotConnected,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::TimedOut,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(peer_connection_changed(&io::Error::new(kind, "test")));
        }
        assert!(!peer_connection_changed(&io::Error::new(
            io::ErrorKind::InvalidData,
            "test"
        )));
    }
}
