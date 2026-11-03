//! One serialized input pump across the workspace hub, native owner and pointer.
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::time::{Duration, Instant};

use crate::clipboard::drag_live::DragBridge;
use crate::cross_screen::hub::PeerConnectionHub;
use crate::cross_screen::native_input::{
    NativeInputDiagnostics, NativeInputEvent, NativeInputRuntime,
};
use crate::cross_screen::{
    CompiledBoundaryLayout, InputEvent, InputLedger, KeyTransition, LedgerResult, MouseButton,
    PhysicalScreenSize,
};
use std::sync::Arc;

use super::WorkspaceLayout;
use super::control::{BoundaryMappingRef, ControlToken, CrossScreenCoordinator, Handoff};
use super::control_host::ControlHost;
use super::persistence::LocalBoundarySettings;
use super::pointer::WorkspacePointer;
use super::pointer_gain::{PointerGainCalibration, PointerGainModel};
use super::precise_pointer::{PrecisePointerCrossing, PreciseWorkspacePointer};
use super::{DeviceId, Edge};

/// Native operations used by the pump; deterministic backends exercise the same
/// control flow over actual authenticated hub connections without moving a user's cursor.
#[allow(clippy::missing_errors_doc)] // Backend implementations retain their native operation contracts.
pub trait InputBackend {
    fn screenshot_active(&self) -> bool {
        crate::screenshot::selection_active()
    }
    fn fullscreen(&self) -> io::Result<bool> {
        Ok(false)
    }
    fn screen(&self) -> PhysicalScreenSize;
    fn take_events(&self) -> io::Result<Vec<NativeInputEvent>>;
    fn inject(&self, events: &[InputEvent]) -> io::Result<()>;
    /// Ordered admission for ordinary remote input, rather than Win32 completion.
    fn enqueue_remote(&self, events: &[InputEvent]) -> io::Result<()> {
        self.inject(events)
    }
    /// Cancel old queued input and await actual native release before handoff.
    fn injection_barrier(&self) -> io::Result<()> {
        Ok(())
    }
    fn forwarding(&self, active: bool) -> io::Result<()>;
    fn target_capture(&self, active: bool) -> io::Result<()>;
    fn release_all(&self) -> io::Result<()>;
    fn diagnostics(&self) -> NativeInputDiagnostics {
        NativeInputDiagnostics::default()
    }
    fn note_target_takeover(&self) {}
    fn drain_cursor_probes(&self) -> Vec<String> {
        Vec::new()
    }
    fn set_event_notifier(&self, _notifier: Arc<dyn Fn() + Send + Sync>) {}
    fn shutdown(&mut self) -> io::Result<()> {
        self.release_all()
    }
}

impl InputBackend for NativeInputRuntime {
    fn drain_cursor_probes(&self) -> Vec<String> {
        self.drain_cursor_probes()
    }
    fn fullscreen(&self) -> io::Result<bool> {
        crate::cross_screen::foreground_covers_primary_screen()
    }
    fn screen(&self) -> PhysicalScreenSize {
        self.snapshot().screen
    }
    fn take_events(&self) -> io::Result<Vec<NativeInputEvent>> {
        self.take_events()
    }
    fn inject(&self, events: &[InputEvent]) -> io::Result<()> {
        self.inject(events)
    }
    fn enqueue_remote(&self, events: &[InputEvent]) -> io::Result<()> {
        self.enqueue_remote(events)
    }
    fn injection_barrier(&self) -> io::Result<()> {
        self.injection_barrier()
    }
    fn forwarding(&self, active: bool) -> io::Result<()> {
        self.set_forwarding(active)
    }
    fn target_capture(&self, active: bool) -> io::Result<()> {
        self.set_target_capture(active)
    }
    fn release_all(&self) -> io::Result<()> {
        self.release_all()
    }
    fn diagnostics(&self) -> NativeInputDiagnostics {
        self.snapshot().diagnostics
    }
    fn note_target_takeover(&self) {
        self.note_target_takeover();
    }
    fn set_event_notifier(&self, notifier: Arc<dyn Fn() + Send + Sync>) {
        self.set_event_notifier(notifier);
    }
    fn shutdown(&mut self) -> io::Result<()> {
        let result = self.stop()?;
        if !result.clean || result.remaining_inputs != 0 {
            return Err(io::Error::other(
                result
                    .last_error
                    .unwrap_or_else(|| "native input shutdown was incomplete".into()),
            ));
        }
        Ok(())
    }
}

/// How often the keep-warm policy is re-evaluated, and how long after the last local input the
/// user still counts as active.
const KEEP_WARM_REFRESH: Duration = Duration::from_millis(250);
const KEEP_WARM_USER_ACTIVE: Duration = Duration::from_mins(3);
/// See `arrival_edge_guard_until`.
const ARRIVAL_EDGE_GUARD: Duration = Duration::from_millis(150);

pub struct WorkspaceInputRuntime<B: InputBackend = NativeInputRuntime> {
    local: DeviceId,
    host: ControlHost,
    pointer: RefCell<PointerRouter>,
    backend: B,
    local_os: RefCell<InputLedger>,
    forwarding: Cell<bool>,
    capture: Cell<bool>,
    pointer_gain: RefCell<PointerGainCalibration>,
    forwarding_gain: Cell<PointerGainModel>,
    forwarding_residual_x: Cell<i64>,
    forwarding_residual_y: Cell<i64>,
    motion_probe_remaining: u8,
    entry_carry_pixels: Cell<Option<(i32, i32)>>,
    // An arrival is placed one pixel inside the boundary. Within milliseconds a single small
    // outward packet, raw sensor jitter or a stale absolute sample, crossed straight back while
    // the user kept moving away from the edge (2026-10-10 19:57:20, 19:58:33, 20:21:10). No local
    // input may select a crossing during this short window.
    arrival_edge_guard_until: Cell<Option<Instant>>,
    // Last local input that fed edge detection, and the arrival placement, for crossing traces.
    last_edge_input: Cell<(char, i32, i32)>,
    arrival_note: RefCell<Option<String>>,
    // CF-CS-007 keep-warm: last real local input, last policy refresh and per-peer state.
    last_local_input: Cell<Option<Instant>>,
    keep_warm_checked: Cell<Option<Instant>>,
    keep_warm_state: RefCell<BTreeMap<DeviceId, bool>>,
    target_takeover: RefCell<TargetTakeoverMotion>,
    drag_bridges: BTreeMap<DeviceId, Arc<DragBridge>>,
    active_drag_peer: Cell<Option<DeviceId>>,
    remote_left_down: Cell<bool>,
    paused: bool,
}

const TARGET_TAKEOVER_WINDOW: Duration = Duration::from_millis(150);
const TARGET_TAKEOVER_DISTANCE: u64 = 64;
const TARGET_TAKEOVER_SAMPLES: u8 = 4;
const TARGET_TAKEOVER_COHERENCE_NUMERATOR: u64 = 3;
const TARGET_TAKEOVER_COHERENCE_DENOMINATOR: u64 = 4;

#[derive(Debug, Default)]
struct TargetTakeoverMotion {
    started: Option<Instant>,
    samples: u8,
    path_distance: u64,
    net_x: i64,
    net_y: i64,
}

impl TargetTakeoverMotion {
    fn observe_raw(&mut self, delta_x: i32, delta_y: i32, now: Instant) -> bool {
        let Some(started) = self.started else {
            self.restart(delta_x, delta_y, now);
            return false;
        };
        if now.saturating_duration_since(started) > TARGET_TAKEOVER_WINDOW {
            self.restart(delta_x, delta_y, now);
            return false;
        }
        self.samples = self.samples.saturating_add(1);
        self.path_distance = self
            .path_distance
            .saturating_add(raw_distance(delta_x, delta_y));
        self.net_x = self.net_x.saturating_add(i64::from(delta_x));
        self.net_y = self.net_y.saturating_add(i64::from(delta_y));
        let net_distance = self
            .net_x
            .unsigned_abs()
            .saturating_add(self.net_y.unsigned_abs());
        self.samples >= TARGET_TAKEOVER_SAMPLES
            && net_distance >= TARGET_TAKEOVER_DISTANCE
            && net_distance.saturating_mul(TARGET_TAKEOVER_COHERENCE_DENOMINATOR)
                >= self
                    .path_distance
                    .saturating_mul(TARGET_TAKEOVER_COHERENCE_NUMERATOR)
    }

    fn restart(&mut self, delta_x: i32, delta_y: i32, now: Instant) {
        self.started = Some(now);
        self.samples = 1;
        self.path_distance = raw_distance(delta_x, delta_y);
        self.net_x = i64::from(delta_x);
        self.net_y = i64::from(delta_y);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn raw_distance(delta_x: i32, delta_y: i32) -> u64 {
    u64::from(delta_x.unsigned_abs()).saturating_add(u64::from(delta_y.unsigned_abs()))
}

enum PointerRouter {
    Grid(WorkspacePointer),
    Precise(PreciseWorkspacePointer),
}

#[derive(Debug)]
enum RoutedCrossing {
    Grid(super::pointer::PointerCrossing),
    Precise(PrecisePointerCrossing),
}

impl PointerRouter {
    #[cfg(test)]
    fn position(&self) -> crate::cross_screen::PhysicalPoint {
        match self {
            Self::Grid(pointer) => pointer.position(),
            Self::Precise(pointer) => pointer.position(),
        }
    }

    fn local_position(
        &mut self,
        x: i32,
        y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        match self {
            Self::Grid(pointer) => pointer.local_position(x, y, ready),
            Self::Precise(pointer) => pointer.local_position(x, y, ready),
        }
    }

    fn local_projected(
        &mut self,
        raw_x: i32,
        raw_y: i32,
        projected_x: i32,
        projected_y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<Option<(i32, i32)>> {
        match self {
            Self::Grid(pointer) => pointer.local_raw(raw_x, raw_y, ready).map(|()| None),
            Self::Precise(pointer) => Ok(pointer.local_projected(projected_x, projected_y, ready)),
        }
    }

    fn neighbor_peers(&self) -> Vec<DeviceId> {
        match self {
            Self::Grid(_) => Vec::new(),
            Self::Precise(pointer) => pointer.neighbor_peers(),
        }
    }

    fn peers_near_boundaries(&self) -> Vec<DeviceId> {
        match self {
            Self::Grid(_) => Vec::new(),
            Self::Precise(pointer) => pointer.peers_near_boundaries(),
        }
    }

    fn remote_delta(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<InputEvent> {
        match self {
            Self::Grid(pointer) => pointer.remote_delta(delta_x, delta_y, ready),
            Self::Precise(pointer) => pointer.remote_delta(delta_x, delta_y, ready),
        }
    }

    fn arrival(&mut self, handoff: &Handoff) -> io::Result<InputEvent> {
        match self {
            Self::Grid(pointer) => pointer.arrival(handoff),
            Self::Precise(pointer) => pointer.arrival(handoff),
        }
    }

    fn crossing(&self) -> Option<RoutedCrossing> {
        match self {
            Self::Grid(pointer) => pointer.crossing().map(RoutedCrossing::Grid),
            Self::Precise(pointer) => pointer.crossing().cloned().map(RoutedCrossing::Precise),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Grid(pointer) => pointer.reset(),
            Self::Precise(pointer) => pointer.reset(),
        }
    }

    fn rebase_local(&mut self) {
        match self {
            Self::Grid(pointer) => pointer.rebase_local(),
            Self::Precise(pointer) => pointer.rebase_local(),
        }
    }
}

impl<B: InputBackend> WorkspaceInputRuntime<B> {
    /// Attach the one native owner to a validated coordinator and layout.
    ///
    /// # Errors
    /// Rejects invalid physical geometry or boundary preferences.
    pub fn new(
        local: DeviceId,
        coordinator: CrossScreenCoordinator,
        layout: WorkspaceLayout,
        preferences: &LocalBoundarySettings,
        backend: B,
    ) -> io::Result<Self> {
        let pointer = PointerRouter::Grid(WorkspacePointer::new(
            local,
            layout,
            backend.screen(),
            preferences,
        )?);
        Ok(Self {
            local,
            host: ControlHost::new(local, coordinator),
            pointer: RefCell::new(pointer),
            backend,
            local_os: RefCell::new(InputLedger::default()),
            forwarding: Cell::new(false),
            capture: Cell::new(false),
            pointer_gain: RefCell::new(PointerGainCalibration::default()),
            forwarding_gain: Cell::new(PointerGainModel::default()),
            forwarding_residual_x: Cell::new(0),
            forwarding_residual_y: Cell::new(0),
            motion_probe_remaining: 0,
            entry_carry_pixels: Cell::new(None),
            arrival_edge_guard_until: Cell::new(None),
            last_edge_input: Cell::new(('-', 0, 0)),
            arrival_note: RefCell::new(None),
            last_local_input: Cell::new(None),
            keep_warm_checked: Cell::new(None),
            keep_warm_state: RefCell::new(BTreeMap::new()),
            target_takeover: RefCell::new(TargetTakeoverMotion::default()),
            drag_bridges: BTreeMap::new(),
            active_drag_peer: Cell::new(None),
            remote_left_down: Cell::new(false),
            paused: !preferences.enabled,
        })
    }

    /// Attach the native owner to the applied pair-scoped v3 boundary mappings.
    ///
    /// # Errors
    /// Rejects invalid compiled geometry or local input preferences.
    pub fn new_precise(
        local: DeviceId,
        coordinator: CrossScreenCoordinator,
        layout: CompiledBoundaryLayout,
        preferences: &LocalBoundarySettings,
        backend: B,
    ) -> io::Result<Self> {
        let pointer = PointerRouter::Precise(PreciseWorkspacePointer::new(
            layout,
            backend.screen(),
            preferences.require_second_push,
        )?);
        Ok(Self {
            local,
            host: ControlHost::new(local, coordinator),
            pointer: RefCell::new(pointer),
            backend,
            local_os: RefCell::new(InputLedger::default()),
            forwarding: Cell::new(false),
            capture: Cell::new(false),
            pointer_gain: RefCell::new(PointerGainCalibration::default()),
            forwarding_gain: Cell::new(PointerGainModel::default()),
            forwarding_residual_x: Cell::new(0),
            forwarding_residual_y: Cell::new(0),
            motion_probe_remaining: 0,
            entry_carry_pixels: Cell::new(None),
            arrival_edge_guard_until: Cell::new(None),
            last_edge_input: Cell::new(('-', 0, 0)),
            arrival_note: RefCell::new(None),
            last_local_input: Cell::new(None),
            keep_warm_checked: Cell::new(None),
            keep_warm_state: RefCell::new(BTreeMap::new()),
            target_takeover: RefCell::new(TargetTakeoverMotion::default()),
            drag_bridges: BTreeMap::new(),
            active_drag_peer: Cell::new(None),
            remote_left_down: Cell::new(false),
            paused: !preferences.enabled,
        })
    }

    #[must_use]
    pub fn token(&self) -> Option<&ControlToken> {
        self.host.token()
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.host.last_error()
    }

    #[must_use]
    pub const fn last_control_return_reason(&self) -> Option<&'static str> {
        self.host.last_control_return_reason()
    }

    #[must_use]
    pub fn screen(&self) -> PhysicalScreenSize {
        self.backend.screen()
    }

    pub(crate) fn fullscreen(&self) -> io::Result<bool> {
        self.backend.fullscreen()
    }

    pub(crate) fn set_event_notifier(&self, notifier: Arc<dyn Fn() + Send + Sync>) {
        self.backend.set_event_notifier(notifier);
    }

    #[must_use]
    pub(crate) fn diagnostics(&self) -> NativeInputDiagnostics {
        self.backend.diagnostics()
    }

    pub(crate) fn drain_probe_events(&mut self) -> Vec<String> {
        let mut events = self.host.drain_probe_events();
        events.extend(self.backend.drain_cursor_probes());
        events
    }

    pub(crate) fn register_drag_bridge(&mut self, peer: DeviceId, bridge: Arc<DragBridge>) {
        self.drag_bridges.insert(peer, bridge);
    }

    pub(crate) fn unregister_drag_bridge(&mut self, peer: DeviceId) {
        self.drag_bridges.remove(&peer);
        if self.active_drag_peer.get() == Some(peer) {
            self.active_drag_peer.set(None);
        }
    }

    /// CF-CS-007: a Wi-Fi peer's link stalls on TCP retransmission during the first seconds of
    /// dense traffic after a quiet period. Ask that peer's channel to ping densely while the
    /// local pointer is near its boundary, so the stall happens before the crossing.
    fn warm_links_near_pointer(
        &mut self,
        hub: &PeerConnectionHub,
        ready: &impl Fn(DeviceId) -> bool,
    ) {
        let peers = self.pointer.borrow().peers_near_boundaries();
        for peer in peers {
            if !ready(peer) {
                continue;
            }
            if hub
                .with_peer(peer, |channel| Ok(channel.request_warmup()))
                .unwrap_or(false)
            {
                self.host.note_pointer_diagnostic(&format!(
                    "phase=link_warmup peer={:02x}{:02x}{:02x}{:02x} reason=pointer_near_boundary",
                    peer[0], peer[1], peer[2], peer[3]
                ));
            }
        }
    }

    /// CF-CS-007: while the user is active on this machine (and it is not itself being remotely
    /// controlled), keep each neighbouring link at a denser heartbeat. The channel applies it
    /// only when this end is on AC power with the switch on and the peer advertises the same.
    fn refresh_keep_warm(
        &mut self,
        hub: &PeerConnectionHub,
        ready: &impl Fn(DeviceId) -> bool,
        now: Instant,
    ) {
        if self
            .keep_warm_checked
            .get()
            .is_some_and(|checked| now.saturating_duration_since(checked) < KEEP_WARM_REFRESH)
        {
            return;
        }
        self.keep_warm_checked.set(Some(now));
        let controlled = self
            .host
            .token()
            .is_some_and(|token| token.origin != self.local);
        let user_active = self
            .last_local_input
            .get()
            .is_some_and(|input| now.saturating_duration_since(input) < KEEP_WARM_USER_ACTIVE);
        let requested = user_active && !controlled && !self.paused;
        let peers = self.pointer.borrow().neighbor_peers();
        for peer in peers {
            let effective = ready(peer)
                && hub
                    .with_peer(peer, |channel| Ok(channel.set_keep_warm(requested)))
                    .unwrap_or(false);
            let changed =
                self.keep_warm_state.borrow_mut().insert(peer, effective) != Some(effective);
            if changed {
                self.host.note_pointer_diagnostic(&format!(
                    "phase=keep_warm peer={:02x}{:02x}{:02x}{:02x} effective={effective} requested={requested} local_allows={}",
                    peer[0],
                    peer[1],
                    peer[2],
                    peer[3],
                    crate::cross_screen::link_warmth::local_allows(),
                ));
            }
        }
    }

    fn arrival_edge_guarded(&self, now: Instant) -> bool {
        self.arrival_edge_guard_until
            .get()
            .is_some_and(|until| now < until)
    }

    #[must_use]
    pub(crate) fn physical_left_down(&self) -> bool {
        contains_left_down(&self.host.physical_held())
    }

    #[must_use]
    pub(crate) fn remote_left_down(&self) -> bool {
        self.remote_left_down.get()
    }

    pub(crate) fn shutdown(&mut self, hub: &PeerConnectionHub) -> io::Result<()> {
        let release = self.return_local(hub);
        let shutdown = self.backend.shutdown();
        release.and(shutdown)
    }

    /// Drain native input before advancing authenticated control messages. One
    /// caller serializes this with layout replication; callbacks never run hooks.
    ///
    /// # Errors
    /// Native or input failures synchronously attempt release and return an error.
    pub fn tick(
        &mut self,
        hub: &PeerConnectionHub,
        now: Instant,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        let result = self.tick_inner(hub, now, ready);
        if let Err(error) = result {
            let cleanup = self.return_local(hub);
            return Err(match cleanup {
                Ok(()) => error,
                Err(release) => {
                    io::Error::other(format!("{error}; input cleanup failed: {release}"))
                }
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // Keep the capture/commit/crossing ordering visible in one pump.
    fn tick_inner(
        &mut self,
        hub: &PeerConnectionHub,
        now: Instant,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        self.flush_entry_carry(hub)?;
        // Keep the target's lease and input pump running while the screenshot
        // selects/annotates at an edge. Only boundary handoffs are suspended.
        let selection_active = self.backend.screenshot_active();
        if selection_active {
            self.pointer.borrow_mut().reset();
        }
        for captured in self.backend.take_events()? {
            match captured {
                NativeInputEvent::EmergencyReturn => {
                    self.host.clear_physical_after_emergency();
                    self.return_local(hub)?;
                    // Do not replay a capture batch predating the emergency barrier.
                    return Ok(());
                }
                NativeInputEvent::Physical(event) | NativeInputEvent::Suppressed(event) => {
                    let suppressed = matches!(captured, NativeInputEvent::Suppressed(_));
                    self.last_local_input.set(Some(now));
                    if !suppressed {
                        record(&mut self.local_os.borrow_mut(), event)?;
                    }
                    let controlled = self
                        .host
                        .token()
                        .is_some_and(|token| token.origin != self.local);
                    if controlled {
                        // Decisive physical input on the target has priority. Pointer jitter is
                        // handled by the timestamped motion branch below.
                        self.target_takeover.borrow_mut().reset();
                        self.return_local(hub)?;
                    }
                    if !matches!(event, InputEvent::MouseMove { .. }) {
                        if let InputEvent::MouseButton {
                            button: MouseButton::Left,
                            transition,
                        } = event
                            && transition == KeyTransition::Up
                        {
                            // The original source can be local again with no control token
                            // while its OLE return preview is still running. Its physical
                            // release must end that preview, not wait for the timeout.
                            for drag in self.drag_bridges.values() {
                                drag.release_returned_source();
                                if !self.host.origin_forwarding() {
                                    // A reverse receiver presenting a peer's drag on this
                                    // computer ends only on this physical release.
                                    let _ = drag.release_local_target();
                                }
                            }
                            if self.host.origin_forwarding()
                                && let Some(drag) = self.active_source_drag()
                            {
                                drag.release_source();
                            }
                        }
                        self.host.physical_input(hub, event, &mut |events| {
                            self.backend.inject(events)
                        })?;
                    }
                    if self.host.origin_forwarding() {
                        // An event captured just before the latch may already have
                        // reached Windows. Remove that local held state explicitly.
                        self.release_local_os()?;
                    } else {
                        if suppressed
                            && !controlled
                            && matches!(event, InputEvent::MouseMove { .. })
                        {
                            // A source-confinement sample captured before return is
                            // not the user's intended local cursor position.
                            continue;
                        }
                        if suppressed {
                            self.backend.inject(&[event])?;
                            record(&mut self.local_os.borrow_mut(), event)?;
                        }
                        if let InputEvent::MouseMove { x, y } = event {
                            self.last_edge_input.set(('a', x, y));
                            let guarded = self.arrival_edge_guarded(now);
                            self.pointer.borrow_mut().local_position(x, y, &|peer| {
                                !selection_active && !guarded && ready(peer)
                            })?;
                        }
                    }
                }
                NativeInputEvent::PointerMotion {
                    x,
                    y,
                    message_time_ms,
                    suppressed,
                } => {
                    let controlled = self
                        .host
                        .token()
                        .is_some_and(|token| token.origin != self.local);
                    if controlled {
                        // Absolute hook coordinates can also move when Windows, OLE, or an
                        // accessibility utility repositions the cursor. Only raw device motion
                        // below is allowed to reclaim a remotely controlled target.
                        continue;
                    }
                    if self.host.origin_forwarding() || suppressed {
                        // Absolute hook samples are the confined source cursor while forwarding.
                        continue;
                    }
                    self.pointer_gain.borrow_mut().record_processed(
                        x,
                        y,
                        message_time_ms,
                        self.backend.screen(),
                    );
                    self.last_edge_input.set(('a', x, y));
                    let guarded = self.arrival_edge_guarded(now);
                    self.pointer.borrow_mut().local_position(x, y, &|peer| {
                        !selection_active && !guarded && ready(peer)
                    })?;
                }
                NativeInputEvent::RawMotion {
                    delta_x,
                    delta_y,
                    message_time_ms,
                } => {
                    self.last_local_input.set(Some(now));
                    let controlled = self
                        .host
                        .token()
                        .is_some_and(|token| token.origin != self.local);
                    if controlled {
                        if self
                            .target_takeover
                            .borrow_mut()
                            .observe_raw(delta_x, delta_y, now)
                        {
                            self.backend.note_target_takeover();
                            self.return_local(hub)?;
                        }
                    } else if self.host.origin_forwarding() {
                        let raw = (delta_x, delta_y);
                        let (delta_x, delta_y) = self.scale_forwarding(delta_x, delta_y);
                        if self.motion_probe_remaining > 0 {
                            self.motion_probe_remaining -= 1;
                            self.host.note_pointer_diagnostic(&format!(
                                "phase=motion token={:?} raw={},{} scaled={},{}",
                                self.host
                                    .token()
                                    .map(|token| (token.session_id, token.epoch)),
                                raw.0,
                                raw.1,
                                delta_x,
                                delta_y,
                            ));
                        }
                        self.host.physical_input(
                            hub,
                            InputEvent::MouseMove {
                                x: delta_x,
                                y: delta_y,
                            },
                            &mut |events| self.backend.inject(events),
                        )?;
                    } else if self.host.token().is_none() {
                        let (projected_x, projected_y, matched_processed) = {
                            let mut calibration = self.pointer_gain.borrow_mut();
                            if let Some(overflow) =
                                calibration.record_raw(delta_x, delta_y, message_time_ms)
                            {
                                (overflow.0, overflow.1, true)
                            } else {
                                let mut residual_x = 0;
                                let mut residual_y = 0;
                                let projected = calibration.model().scale(
                                    delta_x,
                                    delta_y,
                                    &mut residual_x,
                                    &mut residual_y,
                                );
                                (projected.0, projected.1, false)
                            }
                        };
                        if self.pointer.borrow().crossing().is_some() {
                            if matched_processed && (projected_x != 0 || projected_y != 0) {
                                self.entry_carry_pixels
                                    .set(Some((projected_x, projected_y)));
                            }
                        } else if let Some((x, y)) = {
                            self.last_edge_input.set(('r', projected_x, projected_y));
                            let guarded = self.arrival_edge_guarded(now);
                            self.pointer.borrow_mut().local_projected(
                                delta_x,
                                delta_y,
                                projected_x,
                                projected_y,
                                &|peer| !selection_active && !guarded && ready(peer),
                            )?
                        } && (x != 0 || y != 0)
                        {
                            self.entry_carry_pixels.set(Some((x, y)));
                        }
                    }
                }
            }
        }
        let paused = self.paused;
        let enabled = |peer| !paused && ready(peer);
        let pointer_enabled = |peer| !selection_active && enabled(peer);
        let origin_held = self.host.physical_held();
        let pointer = &self.pointer;
        let backend = &self.backend;
        let drag_bridges = &self.drag_bridges;
        let active_drag_peer = &self.active_drag_peer;
        let remote_left_down = &self.remote_left_down;
        let prepare_inputs = |events: &[InputEvent]| {
            let mut pointer = pointer.borrow_mut();
            events
                .iter()
                .filter_map(|event| match event {
                    InputEvent::MouseMove { x, y } => {
                        Some(pointer.remote_delta(*x, *y, &pointer_enabled))
                    }
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        transition,
                    } => {
                        remote_left_down.set(*transition == KeyTransition::Down);
                        let suppress = active_drag_peer
                            .get()
                            .and_then(|peer| drag_bridges.get(&peer))
                            .is_some_and(|drag| {
                                drag.observe_remote_source_left_event(
                                    *transition == KeyTransition::Down,
                                );
                                drag.target_pending() || drag.returned_source_pending()
                            });
                        (!suppress).then_some(Ok(*event))
                    }
                    _ => Some(Ok(*event)),
                })
                .collect::<io::Result<Vec<_>>>()
        };
        self.host.tick_with_delivery(
            hub,
            now,
            &enabled,
            &mut |events| {
                backend.injection_barrier()?;
                backend.inject(&prepare_inputs(events)?)
            },
            &mut |handoff: &Handoff| {
                backend.injection_barrier()?;
                backend.forwarding(false)?;
                self.forwarding.set(false);
                let capture = handoff.next.origin != self.local;
                backend.target_capture(capture)?;
                self.capture.set(capture);
                active_drag_peer.set(Some(handoff.previous.current));
                remote_left_down.set(contains_left_down(&handoff.held));
                let releases = self.local_os.borrow().release_events();
                backend.inject(&releases)?;
                *self.local_os.borrow_mut() = InputLedger::default();
                let arrival = pointer.borrow_mut().arrival(handoff)?;
                backend.inject(&[arrival])?;
                *self.arrival_note.borrow_mut() = Some(format!(
                    "phase=arrival point={arrival:?} edge_offset={} boundary={:?} origin_local={}",
                    handoff.edge_offset,
                    handoff
                        .boundary
                        .as_ref()
                        .map(|mapping| mapping.boundary_id.as_str()),
                    handoff.next.origin == self.local,
                ));
                self.arrival_edge_guard_until
                    .set(Some(Instant::now() + ARRIVAL_EDGE_GUARD));
                if let Some(drag) = drag_bridges.get(&handoff.previous.current) {
                    drag.target_control_arrived();
                    if !capture {
                        drag.reverse_control_arrived(contains_left_down(&origin_held));
                    }
                    if drag
                        .source_remote_controlled
                        .load(std::sync::atomic::Ordering::Acquire)
                        && drag.source_dragging()
                    {
                        let _ = drag.present_remote_source_after_peer_reentry();
                    } else if handoff.next.origin == self.local && drag.source_dragging() {
                        let _ = drag.present_source_after_return();
                    }
                }
                if !capture {
                    let restore = local_held_after_return(&origin_held, drag_bridges);
                    backend.inject(&restore)?;
                    for event in &restore {
                        record(&mut self.local_os.borrow_mut(), *event)?;
                    }
                }
                Ok(())
            },
            &mut |events| backend.enqueue_remote(&prepare_inputs(events)?),
        )?;
        if let Some(note) = self.arrival_note.borrow_mut().take() {
            self.host.note_pointer_diagnostic(&note);
        }
        if self.host.token().is_none() && !self.paused {
            self.warm_links_near_pointer(hub, ready);
        }
        self.refresh_keep_warm(hub, ready, now);
        self.flush_entry_carry(hub)?;
        if !self.paused && !self.host.handoff_pending() {
            let crossing = self.pointer.borrow().crossing();
            if let Some(crossing) = crossing {
                let (kind, input_x, input_y) = self.last_edge_input.get();
                self.host.note_pointer_diagnostic(&format!(
                    "phase=crossing_detected input={kind}:{input_x},{input_y} since_arrival_guard={:?} crossing={crossing:?}",
                    self.arrival_edge_guard_until
                        .get()
                        .map(|until| until.saturating_duration_since(Instant::now())),
                ));
                let edge_detected = Instant::now();
                let peer = match &crossing {
                    RoutedCrossing::Grid(crossing) => crossing.peer,
                    RoutedCrossing::Precise(crossing) => crossing.peer,
                };
                if let Some(drag) = self.drag_bridges.get(&peer) {
                    let left = if self
                        .host
                        .token()
                        .is_some_and(|token| token.origin != self.local)
                    {
                        self.remote_left_down.get()
                    } else {
                        self.physical_left_down()
                    };
                    let allowed = if self.host.token().is_none() {
                        drag.may_cross(left)
                    } else {
                        drag.may_return(left)
                    };
                    if !allowed {
                        return self.reconcile_modes();
                    }
                    if self.host.token().is_none() {
                        if drag.source_can_handoff() {
                            drag.source_active
                                .store(true, std::sync::atomic::Ordering::Release);
                            drag.activate_source();
                        }
                        // A reverse receiver presenting the peer's drag here yields while the
                        // held pointer re-enters the peer that owns the original IDataObject.
                        let _ = drag.suspend_reverse_target_for_source_reentry();
                    } else if self
                        .host
                        .token()
                        .is_some_and(|token| token.origin != self.local)
                    {
                        let _ = drag.suspend_target_for_source_return();
                        if drag
                            .source_remote_controlled
                            .load(std::sync::atomic::Ordering::Acquire)
                            && drag.source_dragging()
                        {
                            // End only the re-entry preview before this return releases the
                            // injected button; otherwise that release reads as a local drop.
                            let _ = drag.suspend_source_return_presentation();
                        }
                    }
                }
                if self.host.token().is_none() {
                    // Latch before prepare: physical events are then captured even
                    // while the origin waits for the target's acknowledgement.
                    let forwarding_started = Instant::now();
                    self.backend.forwarding(true)?;
                    let forwarding_us =
                        u64::try_from(forwarding_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    self.host
                        .note_local_edge(peer, edge_detected, forwarding_us);
                    self.forwarding.set(true);
                    self.forwarding_gain.set(self.pointer_gain.borrow().model());
                    self.motion_probe_remaining = 12;
                    self.host.note_pointer_diagnostic(&format!(
                        "phase=latch target={peer:?} screen={:?} {}",
                        self.backend.screen(),
                        self.pointer_gain.borrow().diagnostic(),
                    ));
                    self.forwarding_residual_x.set(0);
                    self.forwarding_residual_y.set(0);
                    self.release_local_os()?;
                }
                let result = match crossing {
                    RoutedCrossing::Grid(crossing) => {
                        self.host
                            .cross_at(hub, crossing.peer, crossing.edge_offset, now, &enabled)
                    }
                    RoutedCrossing::Precise(crossing) => self.host.cross_precise(
                        hub,
                        crossing.peer,
                        crossing.parameter,
                        crossing.boundary,
                        now,
                        &enabled,
                    ),
                };
                self.pointer.borrow_mut().reset();
                result?;
                self.flush_entry_carry(hub)?;
            }
        }
        self.reconcile_modes()
    }

    fn release_local_os(&self) -> io::Result<()> {
        let releases = self.local_os.borrow().release_events();
        if !releases.is_empty() {
            self.backend.inject(&releases)?;
        }
        *self.local_os.borrow_mut() = InputLedger::default();
        Ok(())
    }

    fn scale_forwarding(&self, delta_x: i32, delta_y: i32) -> (i32, i32) {
        let mut residual_x = self.forwarding_residual_x.get();
        let mut residual_y = self.forwarding_residual_y.get();
        let scaled =
            self.forwarding_gain
                .get()
                .scale(delta_x, delta_y, &mut residual_x, &mut residual_y);
        self.forwarding_residual_x.set(residual_x);
        self.forwarding_residual_y.set(residual_y);
        scaled
    }

    fn active_source_drag(&self) -> Option<&Arc<DragBridge>> {
        self.host
            .token()
            .filter(|token| token.origin == self.local && token.current != self.local)
            .and_then(|token| self.drag_bridges.get(&token.current))
            .filter(|drag| drag.source_dragging())
    }

    fn flush_entry_carry(&mut self, hub: &PeerConnectionHub) -> io::Result<()> {
        if !self.host.origin_forwarding() {
            return Ok(());
        }
        let Some((x, y)) = self.entry_carry_pixels.take() else {
            return Ok(());
        };
        // Queue the edge overflow as soon as source confinement starts. The control
        // host holds it behind the handoff commit, where later physical deltas can
        // coalesce in their original order instead of leaving a perceptible pause.
        self.host
            .physical_input(hub, InputEvent::MouseMove { x, y }, &mut |events| {
                self.backend.inject(events)
            })?;
        Ok(())
    }

    fn reconcile_modes(&self) -> io::Result<()> {
        let forwarding = self.host.origin_forwarding();
        let capture = self
            .host
            .token()
            .is_some_and(|token| token.origin != self.local);
        if self.forwarding.get() != forwarding {
            self.backend.forwarding(forwarding)?;
            self.forwarding.set(forwarding);
            if !forwarding {
                self.pointer.borrow_mut().rebase_local();
                self.pointer_gain.borrow_mut().rebase();
                self.entry_carry_pixels.set(None);
                self.forwarding_residual_x.set(0);
                self.forwarding_residual_y.set(0);
                self.active_drag_peer.set(None);
                let held = local_held_after_return(&self.host.physical_held(), &self.drag_bridges);
                self.backend.inject(&held)?;
                for event in held {
                    record(&mut self.local_os.borrow_mut(), event)?;
                }
            }
        }
        if self.capture.get() != capture {
            self.backend.target_capture(capture)?;
            self.capture.set(capture);
            self.target_takeover.borrow_mut().reset();
            if !capture {
                self.remote_left_down.set(false);
            }
        }
        Ok(())
    }

    /// Release the route before returning native input to the local desktop.
    ///
    /// # Errors
    /// Reports either protocol-ledger or native release failure.
    pub fn return_local(&mut self, hub: &PeerConnectionHub) -> io::Result<()> {
        let route = self
            .host
            .return_local(hub, &mut |events| self.backend.inject(events));
        let native = self.backend.release_all();
        self.pointer.borrow_mut().rebase_local();
        self.remote_left_down.set(false);
        self.active_drag_peer.set(None);
        route?;
        native?;
        self.reconcile_modes()
    }

    /// Pause cross-screen control without closing shared transport or SSH sessions.
    ///
    /// # Errors
    /// Propagates release failures before allowing further control.
    pub fn set_paused(&mut self, hub: &PeerConnectionHub, paused: bool) -> io::Result<()> {
        self.paused = paused;
        if paused {
            self.host
                .pause(hub, &mut |events| self.backend.inject(events))?;
            self.return_local(hub)
        } else {
            self.pointer.borrow_mut().rebase_local();
            self.host.resume();
            Ok(())
        }
    }

    /// Install a complete layout after the replication owner's release barrier.
    ///
    /// # Errors
    /// Rejects invalid layout/preferences and fails closed on native release errors.
    pub fn replace_layout(
        &mut self,
        hub: &PeerConnectionHub,
        layout: WorkspaceLayout,
        members: &BTreeSet<DeviceId>,
        edges: BTreeSet<Edge>,
        preferences: &LocalBoundarySettings,
    ) -> io::Result<()> {
        let pointer = WorkspacePointer::new(
            self.local,
            layout.clone(),
            self.backend.screen(),
            preferences,
        )?;
        self.host
            .replace_layout(hub, layout, members, edges, &mut |events| {
                self.backend.inject(events)
            })?;
        *self.pointer.borrow_mut() = PointerRouter::Grid(pointer);
        self.set_paused(hub, !preferences.enabled)?;
        self.reconcile_modes()
    }

    /// Install the pair-scoped layout of a running precise runtime.
    ///
    /// The active route is released first. Drag bridges of peers that are no longer mapped are
    /// unregistered; the bridges of the remaining peers are left untouched.
    ///
    /// # Errors
    /// Rejects invalid geometry or mappings and fails closed when the native release is incomplete.
    pub fn replace_precise_layout(
        &mut self,
        hub: &PeerConnectionHub,
        layout: CompiledBoundaryLayout,
        mappings: BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>,
        require_second_push: bool,
    ) -> io::Result<()> {
        let pointer =
            PreciseWorkspacePointer::new(layout, self.backend.screen(), require_second_push)?;
        let unmapped: Vec<DeviceId> = self
            .drag_bridges
            .keys()
            .copied()
            .filter(|peer| !mappings.contains_key(peer))
            .collect();
        self.host
            .replace_precise_mappings(hub, mappings, &mut |events| self.backend.inject(events))?;
        *self.pointer.borrow_mut() = PointerRouter::Precise(pointer);
        for peer in unmapped {
            self.unregister_drag_bridge(peer);
        }
        self.reconcile_modes()
    }
}

fn contains_left_down(events: &[InputEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Down,
            }
        )
    })
}

fn local_held_after_return(
    held: &[InputEvent],
    drag_bridges: &BTreeMap<DeviceId, Arc<DragBridge>>,
) -> Vec<InputEvent> {
    // A returned file drag is represented by its own preview, and a reverse receiver by its own
    // native drag. Replaying the still-pressed physical button into Explorer would start an
    // ordinary desktop marquee underneath it; after a failed reverse preparation it would turn
    // the abandoned drag into a marquee as well.
    // Consume every bridge's one-shot flag, not only the first set one.
    let mut failed_reverse = false;
    for drag in drag_bridges.values() {
        failed_reverse |= drag.take_failed_reverse_left_restore();
    }
    let preview_owns_left = failed_reverse
        || drag_bridges
            .values()
            .any(|drag| drag.returned_source_pending() || drag.reverse_target_pending());
    held.iter()
        .copied()
        .filter(|event| {
            !preview_owns_left
                || !matches!(
                    event,
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        transition: KeyTransition::Down,
                    }
                )
        })
        .collect()
}

fn record(ledger: &mut InputLedger, event: InputEvent) -> io::Result<()> {
    if ledger.apply(event) == LedgerResult::CapacityExceeded {
        return Err(io::Error::other(
            "local operating-system input ledger exceeded capacity",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::hub::tests::Network;
    use crate::cross_screen::{KeyTransition, MouseButton};
    use crate::workspace::{GridPoint, LayoutRevision, edge};
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    #[derive(Default)]
    struct Backend {
        captured: RefCell<Vec<NativeInputEvent>>,
        injected: RefCell<Vec<InputEvent>>,
        forwarding: Cell<bool>,
        target: Cell<bool>,
        releases: Cell<usize>,
        target_takeovers: Cell<u64>,
        defer_remote: Cell<bool>,
        pending_remote: RefCell<Vec<InputEvent>>,
        screenshot: Cell<bool>,
    }

    impl InputBackend for Backend {
        fn screenshot_active(&self) -> bool {
            self.screenshot.get()
        }
        fn screen(&self) -> PhysicalScreenSize {
            PhysicalScreenSize::new(1920, 1080).unwrap()
        }
        fn take_events(&self) -> io::Result<Vec<NativeInputEvent>> {
            Ok(self.captured.take())
        }
        fn inject(&self, events: &[InputEvent]) -> io::Result<()> {
            if self.defer_remote.get() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fake native call is still blocked",
                ));
            }
            self.injected.borrow_mut().extend_from_slice(events);
            Ok(())
        }
        fn enqueue_remote(&self, events: &[InputEvent]) -> io::Result<()> {
            if self.defer_remote.get() {
                self.pending_remote.borrow_mut().extend_from_slice(events);
                Ok(())
            } else {
                self.inject(events)
            }
        }
        fn forwarding(&self, active: bool) -> io::Result<()> {
            self.forwarding.set(active);
            Ok(())
        }
        fn target_capture(&self, active: bool) -> io::Result<()> {
            self.target.set(active);
            Ok(())
        }
        fn release_all(&self) -> io::Result<()> {
            self.releases.set(self.releases.get() + 1);
            self.forwarding.set(false);
            self.target.set(false);
            Ok(())
        }
        fn note_target_takeover(&self) {
            self.target_takeovers
                .set(self.target_takeovers.get().saturating_add(1));
        }
    }

    struct Rig {
        runtimes: Vec<WorkspaceInputRuntime<Backend>>,
        network: Network,
    }

    impl Rig {
        fn new() -> Self {
            let network = Network::new(2);
            network.wait_connected();
            let members: BTreeSet<_> = network.ids.iter().copied().collect();
            let edges = BTreeSet::from([edge(network.ids[0], network.ids[1])]);
            let positions = BTreeMap::from([
                (network.ids[0], GridPoint { x: 0, y: 0 }),
                (network.ids[1], GridPoint { x: 1, y: 0 }),
            ]);
            let layout = WorkspaceLayout::new(
                [7; 16],
                LayoutRevision {
                    counter: 1,
                    writer: network.ids[0],
                },
                &positions,
                &members,
                &edges,
            )
            .unwrap();
            let preferences = LocalBoundarySettings {
                schema: 2,
                enabled: true,
                corner_guard_enabled: false,
                corner_guard_pixels: 32,
                require_second_push: false,
                pause_on_fullscreen: false,
            };
            let runtimes = network
                .ids
                .iter()
                .map(|local| {
                    WorkspaceInputRuntime::new(
                        *local,
                        CrossScreenCoordinator::new(
                            *local,
                            layout.clone(),
                            &members,
                            edges.clone(),
                        )
                        .unwrap(),
                        layout.clone(),
                        &preferences,
                        Backend::default(),
                    )
                    .unwrap()
                })
                .collect();
            Self { runtimes, network }
        }

        fn until(&mut self, predicate: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                for (index, runtime) in self.runtimes.iter_mut().enumerate() {
                    runtime
                        .tick(&self.network.hubs[index], Instant::now(), &|_| true)
                        .unwrap();
                }
                if predicate(self) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "native pump did not converge: {:?}",
                    self.runtimes
                        .iter()
                        .map(|runtime| (runtime.token(), runtime.last_error()))
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn cross(&mut self) {
            self.runtimes[0].backend.captured.borrow_mut().extend([
                NativeInputEvent::Physical(InputEvent::MouseMove { x: 1000, y: 400 }),
                NativeInputEvent::Physical(InputEvent::MouseMove { x: 1919, y: 400 }),
                NativeInputEvent::RawMotion {
                    delta_x: 10,
                    delta_y: 0,
                    message_time_ms: 1,
                },
            ]);
            self.until(|rig| {
                rig.runtimes
                    .iter()
                    .all(|runtime| runtime.token().is_some() && !runtime.host.handoff_pending())
            });
        }
    }

    fn button(transition: KeyTransition) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            transition,
        }
    }

    #[test]
    fn screenshot_locks_target_edges_but_keeps_remote_motion_clicks_and_lease_alive() {
        let mut rig = Rig::new();
        rig.cross();
        let token = rig.runtimes[1].token().unwrap().clone();
        rig.runtimes[1].backend.screenshot.set(true);
        rig.runtimes[1].backend.injected.borrow_mut().clear();
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::RawMotion {
                delta_x: -10000,
                delta_y: 0,
                message_time_ms: 5,
            },
            NativeInputEvent::Suppressed(button(KeyTransition::Down)),
            NativeInputEvent::Suppressed(button(KeyTransition::Up)),
        ]);
        let until = Instant::now() + Duration::from_millis(2200);
        while Instant::now() < until {
            for (i, runtime) in rig.runtimes.iter_mut().enumerate() {
                runtime
                    .tick(&rig.network.hubs[i], Instant::now(), &|_| true)
                    .unwrap();
                assert_eq!(
                    runtime.token().map(|t| (t.session_id, t.epoch)),
                    Some((token.session_id, token.epoch))
                );
                assert!(runtime.last_error().is_none());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let delivered = rig.runtimes[1].backend.injected.borrow();
        assert!(delivered.contains(&button(KeyTransition::Down)));
        assert!(delivered.contains(&button(KeyTransition::Up)));
        assert!(
            delivered
                .iter()
                .any(|event| matches!(event, InputEvent::MouseMove { x: 0, .. }))
        );
        drop(delivered);
        rig.runtimes[1].backend.screenshot.set(false);
        rig.runtimes[0].return_local(&rig.network.hubs[0]).unwrap();
        rig.until(|rig| rig.runtimes.iter().all(|r| r.token().is_none()));
        rig.cross();
    }

    #[test]
    fn a_blocked_target_injector_does_not_stop_input_delivery_or_control_renewals() {
        let mut rig = Rig::new();
        rig.cross();
        let token = rig.runtimes[1].token().unwrap().clone();
        rig.runtimes[1].backend.defer_remote.set(true);
        let scroll = InputEvent::Wheel {
            axis: crate::cross_screen::WheelAxis::Vertical,
            delta: 120,
        };
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::Suppressed(scroll),
            NativeInputEvent::Suppressed(button(KeyTransition::Down)),
            NativeInputEvent::Suppressed(button(KeyTransition::Up)),
        ]);
        rig.until(|rig| rig.runtimes[1].backend.pending_remote.borrow().len() >= 3);
        assert_eq!(
            *rig.runtimes[1].backend.pending_remote.borrow(),
            [
                scroll,
                button(KeyTransition::Down),
                button(KeyTransition::Up)
            ]
        );
        // Beyond the two-second lease: actual TLS renewals must still be read
        // and processed while ordinary native input is awaiting completion.
        let deadline = Instant::now() + Duration::from_millis(2200);
        while Instant::now() < deadline {
            for (index, runtime) in rig.runtimes.iter_mut().enumerate() {
                runtime
                    .tick(&rig.network.hubs[index], Instant::now(), &|_| true)
                    .unwrap();
                assert_eq!(
                    runtime.token().map(|token| (token.session_id, token.epoch)),
                    Some((token.session_id, token.epoch))
                );
                assert!(runtime.last_error().is_none());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.runtimes[1].backend.defer_remote.set(false);
        rig.runtimes[0].return_local(&rig.network.hubs[0]).unwrap();
        rig.until(|rig| rig.runtimes.iter().all(|runtime| runtime.token().is_none()));
    }

    #[test]
    fn physical_left_up_ends_a_returned_source_preview_without_a_control_token() {
        let mut rig = Rig::new();
        let bridge = DragBridge::returned_source_for_test();
        rig.runtimes[0].register_drag_bridge(rig.network.ids[1], Arc::clone(&bridge));
        assert!(rig.runtimes[0].token().is_none());
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::Physical(button(KeyTransition::Up)));

        rig.runtimes[0]
            .tick(&rig.network.hubs[0], Instant::now(), &|_| true)
            .unwrap();

        assert!(bridge.return_release_requested_for_test());
    }

    #[test]
    fn return_preview_does_not_restore_a_desktop_left_down_beneath_the_file_image() {
        let mut rig = Rig::new();
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::Physical(button(KeyTransition::Down)));
        rig.cross();
        let bridge = DragBridge::returned_source_for_test();
        rig.runtimes[0].register_drag_bridge(rig.network.ids[1], bridge);
        rig.runtimes[0].backend.injected.borrow_mut().clear();
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::RawMotion {
                delta_x: -10_000,
                delta_y: 0,
                message_time_ms: 8,
            });

        rig.until(|rig| rig.runtimes.iter().all(|runtime| runtime.token().is_none()));

        assert!(rig.runtimes[0].physical_left_down());
        assert!(
            !rig.runtimes[0]
                .backend
                .injected
                .borrow()
                .contains(&button(KeyTransition::Down))
        );
    }

    #[test]
    fn real_tls_pump_projects_raw_once_and_restores_origin_held_state_on_return() {
        let mut rig = Rig::new();
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::Physical(button(KeyTransition::Down)));
        rig.cross();
        assert!(rig.runtimes[0].backend.forwarding.get());
        assert!(rig.runtimes[1].backend.target.get());
        assert!(
            rig.runtimes[0]
                .backend
                .injected
                .borrow()
                .contains(&button(KeyTransition::Up))
        );
        let arrival = rig.runtimes[1].pointer.borrow().position();
        rig.runtimes[1].backend.injected.borrow_mut().clear();
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::Suppressed(InputEvent::MouseMove { x: 1000, y: 500 }),
            NativeInputEvent::RawMotion {
                delta_x: 17,
                delta_y: 3,
                message_time_ms: 2,
            },
        ]);
        rig.until(|rig| !rig.runtimes[1].backend.injected.borrow().is_empty());
        assert_eq!(
            *rig.runtimes[1].backend.injected.borrow(),
            vec![InputEvent::MouseMove {
                x: i32::try_from(arrival.x).unwrap() + 17,
                y: i32::try_from(arrival.y).unwrap() + 3,
            }]
        );
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::RawMotion {
                delta_x: -10000,
                delta_y: 0,
                message_time_ms: 3,
            });
        rig.until(|rig| rig.runtimes.iter().all(|runtime| runtime.token().is_none()));
        assert!(!rig.runtimes[0].backend.forwarding.get());
        assert_eq!(
            rig.runtimes[0].backend.injected.borrow().last(),
            Some(&button(KeyTransition::Down))
        );
        assert_eq!(
            rig.runtimes[1].backend.injected.borrow().last(),
            Some(&button(KeyTransition::Up))
        );
    }

    #[test]
    fn projected_edge_carry_waits_behind_commit_without_waiting_for_commit() {
        let mut rig = Rig::new();
        // Simulate the distance projected beyond the clamped Windows edge by the
        // raw-input sample that selects the portal below.
        rig.runtimes[0].entry_carry_pixels.set(Some((13, 0)));
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::Physical(InputEvent::MouseMove { x: 1000, y: 400 }),
            NativeInputEvent::Physical(InputEvent::MouseMove { x: 1919, y: 400 }),
        ]);
        rig.runtimes[0]
            .tick(&rig.network.hubs[0], Instant::now(), &|_| true)
            .unwrap();
        assert!(rig.runtimes[0].host.handoff_pending());
        assert!(rig.runtimes[0].backend.forwarding.get());
        assert_eq!(rig.runtimes[0].entry_carry_pixels.get(), None);

        rig.until(|rig| {
            rig.runtimes
                .iter()
                .all(|runtime| runtime.token().is_some() && !runtime.host.handoff_pending())
                && rig.runtimes[1].pointer.borrow().position().x == 14
        });
    }

    #[test]
    fn emergency_and_pause_release_remote_control_without_restoring_emergency_keys() {
        let mut rig = Rig::new();
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::Physical(button(KeyTransition::Down)));
        rig.cross();
        rig.runtimes[0].backend.injected.borrow_mut().clear();
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::EmergencyReturn,
            NativeInputEvent::Suppressed(button(KeyTransition::Down)),
        ]);
        rig.until(|rig| rig.runtimes.iter().all(|runtime| runtime.token().is_none()));
        assert!(rig.runtimes[0].backend.injected.borrow().is_empty());
        rig.runtimes[0]
            .set_paused(&rig.network.hubs[0], true)
            .unwrap();
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::Physical(InputEvent::MouseMove { x: 1919, y: 400 }),
            NativeInputEvent::RawMotion {
                delta_x: 20,
                delta_y: 0,
                message_time_ms: 3,
            },
        ]);
        rig.until(|_| true);
        assert!(!rig.runtimes[0].host.handoff_pending());
        assert!(!rig.runtimes[0].backend.forwarding.get());
        rig.runtimes[0]
            .set_paused(&rig.network.hubs[0], false)
            .unwrap();
        rig.cross();
    }

    #[test]
    fn target_injection_recovery_returns_origin_and_allows_later_crossing() {
        let mut rig = Rig::new();
        rig.cross();
        assert!(rig.runtimes[0].backend.forwarding.get());
        assert!(rig.runtimes[1].backend.target.get());
        rig.runtimes[1]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::EmergencyReturn);
        rig.until(|rig| rig.runtimes.iter().all(|runtime| runtime.token().is_none()));
        assert!(!rig.runtimes[0].backend.forwarding.get());
        assert!(!rig.runtimes[1].backend.target.get());
        assert!(rig.runtimes[1].backend.releases.get() > 0);
        rig.cross();
    }

    #[test]
    fn target_physical_takeover_releases_remote_button_before_replaying_local_event() {
        let mut rig = Rig::new();
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::Physical(button(KeyTransition::Down)));
        rig.cross();
        rig.runtimes[1].backend.injected.borrow_mut().clear();
        rig.runtimes[1]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::Suppressed(button(KeyTransition::Down)));
        rig.until(|rig| rig.runtimes.iter().all(|runtime| runtime.token().is_none()));
        assert_eq!(
            *rig.runtimes[1].backend.injected.borrow(),
            vec![button(KeyTransition::Up), button(KeyTransition::Down)]
        );
        assert!(!rig.runtimes[1].backend.target.get());
    }

    #[test]
    fn controlled_target_uses_raw_device_motion_for_intentional_takeover() {
        let mut rig = Rig::new();
        rig.cross();

        for (x, time) in [(100, 10), (140, 11), (180, 12)] {
            rig.runtimes[1]
                .backend
                .captured
                .borrow_mut()
                .push(NativeInputEvent::PointerMotion {
                    x,
                    y: 100,
                    message_time_ms: time,
                    suppressed: true,
                });
            for (index, runtime) in rig.runtimes.iter_mut().enumerate() {
                runtime
                    .tick(&rig.network.hubs[index], Instant::now(), &|_| true)
                    .unwrap();
            }
            assert!(rig.runtimes[1].token().is_some());
        }

        rig.runtimes[1]
            .backend
            .captured
            .borrow_mut()
            .extend((13..17).map(|message_time_ms| NativeInputEvent::RawMotion {
                delta_x: 20,
                delta_y: 0,
                message_time_ms,
            }));
        rig.until(|rig| rig.runtimes.iter().all(|runtime| runtime.token().is_none()));
        assert_eq!(rig.runtimes[1].backend.target_takeovers.get(), 1);
    }

    #[test]
    fn controlled_target_ignores_oscillating_raw_device_noise() {
        let mut rig = Rig::new();
        rig.cross();
        rig.runtimes[1]
            .backend
            .captured
            .borrow_mut()
            .extend((20..36).map(|message_time_ms| NativeInputEvent::RawMotion {
                delta_x: if message_time_ms % 2 == 0 { 12 } else { -12 },
                delta_y: 0,
                message_time_ms,
            }));

        for (index, runtime) in rig.runtimes.iter_mut().enumerate() {
            runtime
                .tick(&rig.network.hubs[index], Instant::now(), &|_| true)
                .unwrap();
        }

        assert!(rig.runtimes.iter().all(|runtime| runtime.token().is_some()));
    }

    #[test]
    fn forwarded_click_stays_on_the_remote_target() {
        let mut rig = Rig::new();
        rig.cross();
        rig.runtimes[1].backend.injected.borrow_mut().clear();
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::Suppressed(button(KeyTransition::Down)),
            NativeInputEvent::Suppressed(button(KeyTransition::Up)),
        ]);
        rig.until(|rig| rig.runtimes[1].backend.injected.borrow().len() >= 2);
        assert_eq!(
            *rig.runtimes[1].backend.injected.borrow(),
            vec![button(KeyTransition::Down), button(KeyTransition::Up)]
        );
        assert!(rig.runtimes.iter().all(|runtime| runtime.token().is_some()));
    }

    #[test]
    fn learned_windows_pointer_speed_is_kept_after_handoff() {
        let mut rig = Rig::new();
        rig.runtimes[0].backend.captured.borrow_mut().extend([
            NativeInputEvent::PointerMotion {
                x: 100,
                y: 100,
                message_time_ms: 20,
                suppressed: false,
            },
            NativeInputEvent::PointerMotion {
                x: 120,
                y: 100,
                message_time_ms: 21,
                suppressed: false,
            },
            NativeInputEvent::RawMotion {
                delta_x: 10,
                delta_y: 0,
                message_time_ms: 21,
            },
        ]);
        for (index, runtime) in rig.runtimes.iter_mut().enumerate() {
            runtime
                .tick(&rig.network.hubs[index], Instant::now(), &|_| true)
                .unwrap();
        }
        rig.cross();
        let arrival = rig.runtimes[1].pointer.borrow().position();
        rig.runtimes[1].backend.injected.borrow_mut().clear();
        rig.runtimes[0]
            .backend
            .captured
            .borrow_mut()
            .push(NativeInputEvent::RawMotion {
                delta_x: 10,
                delta_y: 10,
                message_time_ms: 22,
            });
        rig.until(|rig| !rig.runtimes[1].backend.injected.borrow().is_empty());
        assert_eq!(
            rig.runtimes[1].backend.injected.borrow().last(),
            Some(&InputEvent::MouseMove {
                x: i32::try_from(arrival.x).unwrap() + 20,
                y: i32::try_from(arrival.y).unwrap() + 20,
            })
        );
    }
}
