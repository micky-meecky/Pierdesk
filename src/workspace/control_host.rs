//! Bounded product pump for the single control coordinator and all hub links.
//! No socket listener, hook or injector thread is duplicated per peer.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::control::{
    self, BoundaryMappingRef, ControlMessage, ControlToken, CrossScreenCoordinator, Handoff,
    HandoffAck,
};
use super::{DeviceId, Edge, WorkspaceLayout};
use crate::cross_screen::hub::PeerConnectionHub;
use crate::cross_screen::{ChannelState, InputEvent, InputLedger, LedgerResult};

const ORIGIN_TIMEOUT: Duration = Duration::from_secs(2);
const RENEW_INTERVAL: Duration = Duration::from_millis(250);
const MAX_INPUT_QUEUE: usize = 256;
const MAX_CONTROL_MESSAGES_PER_PEER: usize = 64;
const DESTINATION_BUSY_RETURN_REASON: &str = "destination_busy";
const MAX_PROBE_EVENTS: usize = 128;
const PROBE_MOTION_WINDOW: Duration = Duration::from_secs(3);

struct CrossingProbe {
    session: [u8; 16],
    epoch: u64,
    started: Instant,
    first_motion: bool,
    first_motion_at: Option<Instant>,
    last_motion_at: Option<Instant>,
    motion_count: u32,
    max_motion_gap_us: u64,
    max_dispatch_us: u64,
    window_emitted: bool,
}

impl CrossingProbe {
    fn new(session: [u8; 16], epoch: u64, started: Instant) -> Self {
        Self {
            session,
            epoch,
            started,
            first_motion: false,
            first_motion_at: None,
            last_motion_at: None,
            motion_count: 0,
            max_motion_gap_us: 0,
            max_dispatch_us: 0,
            window_emitted: false,
        }
    }

    fn matches(&self, session: [u8; 16], epoch: u64) -> bool {
        self.session == session && self.epoch == epoch
    }

    fn record_motion(&mut self, now: Instant, dispatch_us: u64) {
        if self.first_motion_at.is_none() {
            self.first_motion_at = Some(now);
        }
        if let Some(previous) = self.last_motion_at {
            let gap = u64::try_from(now.saturating_duration_since(previous).as_micros())
                .unwrap_or(u64::MAX);
            self.max_motion_gap_us = self.max_motion_gap_us.max(gap);
        }
        self.last_motion_at = Some(now);
        self.motion_count = self.motion_count.saturating_add(1);
        self.max_dispatch_us = self.max_dispatch_us.max(dispatch_us);
    }

    fn window_detail(&self) -> String {
        format!(
            "window_ms={} motion_count={} max_motion_gap_us={} max_dispatch_us={} first_motion={}",
            self.first_motion_at
                .map_or(0, |first| first.elapsed().as_millis()),
            self.motion_count,
            self.max_motion_gap_us,
            self.max_dispatch_us,
            self.first_motion,
        )
    }
}

struct OriginHandoff {
    previous: ControlToken,
    target: DeviceId,
    edge_offset: u16,
    boundary: Option<BoundaryMappingRef>,
    authorized: bool,
    deadline: Instant,
}

struct Outgoing {
    handoff: Handoff,
    ready_ack: Option<HandoffAck>,
    awaiting_commit: bool,
}

pub struct ControlHost {
    local: DeviceId,
    coordinator: CrossScreenCoordinator,
    physical: InputLedger,
    input_queue: VecDeque<InputEvent>,
    links: BTreeMap<DeviceId, u64>,
    outgoing: Option<Outgoing>,
    origin_handoff: Option<OriginHandoff>,
    cross_requested: Option<(DeviceId, Instant)>,
    recent_commit: Option<Handoff>,
    input_sequence: u64,
    renewal_sequence: u64,
    next_renewal: Instant,
    paused: bool,
    last_error: Option<String>,
    last_control_return_reason: Option<&'static str>,
    last_crossing_started: Option<Instant>,
    source_probe: Option<CrossingProbe>,
    target_probe: Option<CrossingProbe>,
    probe_events: VecDeque<String>,
    pending_edge: Option<(DeviceId, Instant, u64)>,
}

impl ControlHost {
    #[must_use]
    pub fn new(local: DeviceId, coordinator: CrossScreenCoordinator) -> Self {
        Self {
            local,
            coordinator,
            physical: InputLedger::default(),
            input_queue: VecDeque::new(),
            links: BTreeMap::new(),
            outgoing: None,
            origin_handoff: None,
            cross_requested: None,
            recent_commit: None,
            input_sequence: 0,
            renewal_sequence: 0,
            next_renewal: Instant::now(),
            paused: false,
            last_error: None,
            last_control_return_reason: None,
            last_crossing_started: None,
            source_probe: None,
            target_probe: None,
            probe_events: VecDeque::new(),
            pending_edge: None,
        }
    }

    pub(crate) fn drain_probe_events(&mut self) -> Vec<String> {
        self.probe_events.drain(..).collect()
    }

    pub(crate) fn note_pointer_diagnostic(&mut self, detail: &str) {
        if self.probe_events.len() == MAX_PROBE_EVENTS {
            self.probe_events.pop_front();
        }
        self.probe_events
            .push_back(format!("event=pointer_gain {detail}"));
    }

    fn probe(&mut self, phase: &str, session: [u8; 16], epoch: u64, detail: &str) {
        let at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_millis());
        if self.probe_events.len() == MAX_PROBE_EVENTS {
            self.probe_events.pop_front();
        }
        self.probe_events.push_back(format!(
            "event=idle_cross_probe phase={phase} at_unix_ms={at_unix_ms} session={:032x} epoch={epoch} {detail}",
            u128::from_le_bytes(session),
        ));
    }

    pub(crate) fn note_local_edge(
        &mut self,
        target: DeviceId,
        detected: Instant,
        forwarding_us: u64,
    ) {
        self.pending_edge = Some((target, detected, forwarding_us));
    }

    fn flush_probe_window(&mut self, source: bool, force: bool) {
        let probe = if source {
            self.source_probe.as_mut()
        } else {
            self.target_probe.as_mut()
        };
        let event = probe.and_then(|probe| {
            let elapsed = probe.first_motion_at.map(|first| first.elapsed());
            if probe.window_emitted
                || !(force || elapsed.is_some_and(|elapsed| elapsed >= PROBE_MOTION_WINDOW))
            {
                return None;
            }
            probe.window_emitted = true;
            Some((probe.session, probe.epoch, probe.window_detail()))
        });
        if let Some((session, epoch, detail)) = event {
            self.probe(
                if source {
                    "source_motion_window"
                } else {
                    "target_motion_window"
                },
                session,
                epoch,
                &detail,
            );
        }
    }

    #[must_use]
    pub fn token(&self) -> Option<&ControlToken> {
        self.coordinator.token()
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    #[must_use]
    pub const fn last_control_return_reason(&self) -> Option<&'static str> {
        self.last_control_return_reason
    }

    #[must_use]
    pub fn origin_forwarding(&self) -> bool {
        self.origin_handoff.is_some()
            || self
                .token()
                .is_some_and(|token| token.origin == self.local && token.current != self.local)
    }

    #[must_use]
    pub fn physical_held(&self) -> Vec<InputEvent> {
        self.physical.press_events()
    }

    pub(crate) fn clear_physical_after_emergency(&mut self) {
        self.physical = InputLedger::default();
    }

    /// Replace validated geometry after ending this device's active control route.
    ///
    /// # Errors
    /// Rejects invalid geometry or an incomplete native release.
    pub fn replace_layout(
        &mut self,
        hub: &PeerConnectionHub,
        layout: WorkspaceLayout,
        members: &BTreeSet<DeviceId>,
        edges: BTreeSet<Edge>,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        CrossScreenCoordinator::new(self.local, layout.clone(), members, edges.clone())?;
        self.return_local(hub, inject)?;
        self.coordinator
            .replace_layout(layout, members, edges, inject)
    }

    /// Replace pair-scoped mappings. Only a route that uses a removed pair is ended first.
    ///
    /// # Errors
    /// Rejects invalid mappings or an incomplete native release; the old mappings remain otherwise.
    pub fn replace_precise_mappings(
        &mut self,
        hub: &PeerConnectionHub,
        mappings: BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        CrossScreenCoordinator::validate_precise_mappings(self.local, &mappings)?;
        if self.coordinator.route_touches_removed(&mappings) {
            self.return_local(hub, inject)?;
        }
        self.coordinator.replace_precise_mappings(mappings, inject)
    }

    #[must_use]
    pub fn handoff_pending(&self) -> bool {
        self.outgoing.is_some()
            || self.origin_handoff.is_some()
            || self.cross_requested.is_some()
            || self.coordinator.awaiting_handoff()
    }

    /// Begin a real boundary crossing. A remote target first asks the physical
    /// origin to freeze input and provide its authoritative held-key snapshot.
    ///
    /// # Errors
    /// Rejects paused/busy control, unavailable or unsynchronized edges, and send failure.
    pub fn cross(
        &mut self,
        hub: &PeerConnectionHub,
        target: DeviceId,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        self.cross_at(hub, target, u16::MAX / 2, now, edge_ready)
    }

    /// Cross at the measured normalized edge offset, independent of either display DPI.
    ///
    /// # Errors
    /// Rejects paused, busy or unsynchronized crossings and transport failures.
    pub fn cross_at(
        &mut self,
        hub: &PeerConnectionHub,
        target: DeviceId,
        edge_offset: u16,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        self.cross_with_mapping(hub, target, edge_offset, None, now, edge_ready)
    }

    /// Cross through one exact confirmed logical boundary.
    ///
    /// # Errors
    /// Rejects paused, busy, offline, or unsynchronized crossings.
    pub fn cross_precise(
        &mut self,
        hub: &PeerConnectionHub,
        target: DeviceId,
        parameter: u16,
        boundary: BoundaryMappingRef,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        self.cross_with_mapping(hub, target, parameter, Some(boundary), now, edge_ready)
    }

    fn cross_with_mapping(
        &mut self,
        hub: &PeerConnectionHub,
        target: DeviceId,
        edge_offset: u16,
        boundary: Option<BoundaryMappingRef>,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        if self.paused || self.handoff_pending() || !edge_ready(target) {
            return Err(invalid("crossing is paused, busy or not synchronized"));
        }
        if let Some(token) = self.token().cloned() {
            if token.current != self.local {
                return Err(invalid("boundary is not on the current device"));
            }
            if token.origin != self.local {
                if target != token.origin {
                    self.send_back(
                        hub,
                        &token,
                        ControlMessage::CrossRequest {
                            session: token.session_id,
                            epoch: token.epoch,
                            target,
                            edge_offset,
                            boundary,
                        },
                    )?;
                    self.cross_requested = Some((target, now + ORIGIN_TIMEOUT));
                    return Ok(());
                }
                // The controlled target already owns the exact injected press ledger. Start the
                // return handoff directly instead of asking the physical origin to send the same
                // snapshot back through two extra network legs. The origin still validates this
                // proposal against its authoritative physical ledger in `Prepare`; any in-flight
                // press change automatically takes the existing refresh path.
                return self.begin(
                    hub,
                    target,
                    self.coordinator.injected_press_events(),
                    edge_offset,
                    boundary,
                    now,
                );
            }
        }
        self.begin(
            hub,
            target,
            self.physical.press_events(),
            edge_offset,
            boundary,
            now,
        )
    }

    /// Called only for physical input, never for events carrying the native
    /// injection marker. Returns whether the hook should suppress the local event.
    ///
    /// # Errors
    /// Overflow aborts control and synchronously releases the retained native ledger.
    pub fn physical_input(
        &mut self,
        hub: &PeerConnectionHub,
        event: InputEvent,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<bool> {
        if self.physical.apply(event) == LedgerResult::CapacityExceeded {
            self.return_local(hub, inject)?;
            return Err(invalid("physical input ledger capacity exceeded"));
        }
        let remote_origin = self
            .token()
            .is_some_and(|token| token.origin == self.local && token.current != self.local);
        let suppress = remote_origin || self.origin_handoff.is_some();
        if !suppress {
            return Ok(false);
        }
        // Until authorization the latest held snapshot supersedes intermediate key
        // transitions. Pointer motion is different: Windows has already confined the
        // source cursor at the edge, so every physical delta produced while the target
        // prepares must be carried into the committed target instead of disappearing.
        // Consecutive motion is coalesced below and remains blocked behind commit.
        let awaiting_authorization = self
            .origin_handoff
            .as_ref()
            .is_some_and(|pending| !pending.authorized);
        if awaiting_authorization && !matches!(event, InputEvent::MouseMove { .. }) {
            return Ok(true);
        }
        if let InputEvent::MouseMove { x, y } = event
            && let Some(InputEvent::MouseMove {
                x: previous_x,
                y: previous_y,
            }) = self.input_queue.back_mut()
        {
            // Workspace mouse packets carry physical deltas. Coalescing must add
            // displacement, unlike the legacy absolute-position transport.
            *previous_x = previous_x.saturating_add(x);
            *previous_y = previous_y.saturating_add(y);
            return Ok(true);
        }
        if self.input_queue.len() == MAX_INPUT_QUEUE {
            self.return_local(hub, inject)?;
            return Err(invalid("reliable physical input queue overflow"));
        }
        self.input_queue.push_back(event);
        Ok(true)
    }

    /// Process bounded work on every peer, then flush input and renew the route.
    /// The same owner invokes this alongside layout replication and the native hook.
    ///
    /// # Errors
    /// Native release failures remain visible and are retried on subsequent ticks.
    pub fn tick(
        &mut self,
        hub: &PeerConnectionHub,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.tick_with_entry(hub, now, edge_ready, inject, &mut |_| Ok(()))
    }

    /// Pump control with a native arrival barrier before held input is installed.
    ///
    /// # Errors
    /// Propagates release or arrival failures and invalidates the affected control route.
    pub fn tick_with_entry(
        &mut self,
        hub: &PeerConnectionHub,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
        enter: &mut impl FnMut(&Handoff) -> io::Result<()>,
    ) -> io::Result<()> {
        let inject = std::cell::RefCell::new(inject);
        self.tick_with_delivery(
            hub,
            now,
            edge_ready,
            &mut |events| inject.borrow_mut()(events),
            enter,
            &mut |events| inject.borrow_mut()(events),
        )
    }

    /// Ordinary input may acknowledge ordered queue admission. Control transitions
    /// still use the synchronous injection/release and arrival barriers above.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tick_with_delivery(
        &mut self,
        hub: &PeerConnectionHub,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
        enter: &mut impl FnMut(&Handoff) -> io::Result<()>,
        deliver: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        let links: BTreeMap<_, _> = hub
            .snapshots()
            .into_iter()
            .filter_map(|peer| {
                peer.channel
                    .filter(|channel| channel.state == ChannelState::Connected)
                    .map(|_| (peer.peer, peer.generation))
            })
            .collect();
        let lost: Vec<_> = self
            .links
            .iter()
            .filter(|(peer, generation)| links.get(*peer) != Some(*generation))
            .map(|(peer, _)| *peer)
            .collect();
        for peer in lost {
            if self.participants().contains(&peer) {
                self.return_local(hub, inject)?;
            }
        }
        self.links = links;
        let route_unready = self.token().is_some_and(|token| {
            [
                route_neighbor(token, self.local, false),
                route_neighbor(token, self.local, true),
            ]
            .into_iter()
            .flatten()
            .any(|peer| !edge_ready(peer))
        });
        if route_unready {
            self.return_local(hub, inject)?;
        }
        let participants = self.participants();
        let identity = self.control_identity();
        let expired = self.coordinator.tick(now, inject)?
            || self
                .origin_handoff
                .as_ref()
                .is_some_and(|pending| now >= pending.deadline)
            || self
                .cross_requested
                .is_some_and(|(_, deadline)| now >= deadline);
        if expired {
            if let Some((session, epoch, detail)) = self.source_probe.as_ref().map(|probe| {
                (
                    probe.session,
                    probe.epoch,
                    format!("since_prepare_us={}", probe.started.elapsed().as_micros()),
                )
            }) {
                self.probe("handoff_expired", session, epoch, &detail);
            }
            if let Some((session, epoch)) = identity {
                for peer in &participants {
                    let _ = control::send(hub, *peer, ControlMessage::Release { session, epoch });
                }
            }
            self.return_local(hub, inject)?;
        }
        let peers: Vec<_> = self.links.keys().copied().collect();
        for peer in peers {
            for _ in 0..MAX_CONTROL_MESSAGES_PER_PEER {
                let message = match control::receive(hub, peer) {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(error) => {
                        self.last_error = Some(error.to_string());
                        break;
                    }
                };
                if let Err(error) =
                    self.dispatch(hub, peer, message, now, edge_ready, inject, enter, deliver)
                {
                    self.last_error = Some(error.to_string());
                    if self.participants().contains(&peer) {
                        self.return_local(hub, inject)?;
                    }
                }
            }
        }
        // Keep the lease ahead of high-frequency pointer traffic. Input backpressure is
        // recoverable; it must not starve the authority renewal that keeps the route alive.
        if let Err(error) = self
            .renew(hub, now)
            .and_then(|()| self.flush_input(hub, now))
        {
            self.last_error = Some(error.to_string());
            self.return_local(hub, inject)?;
        }
        self.flush_probe_window(true, false);
        self.flush_probe_window(false, false);
        Ok(())
    }

    /// Release this token along its route, preserving independent peer sessions.
    ///
    /// # Errors
    /// Propagates native release failure; potentially injected presses remain tracked.
    pub fn return_local(
        &mut self,
        hub: &PeerConnectionHub,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.flush_probe_window(true, true);
        self.flush_probe_window(false, true);
        if let Some(probe) = self.source_probe.take() {
            let detail = format!(
                "elapsed_us={} first_motion={}",
                probe.started.elapsed().as_micros(),
                probe.first_motion
            );
            self.probe("return_local", probe.session, probe.epoch, &detail);
        }
        self.target_probe = None;
        self.pending_edge = None;
        let participants = self.participants();
        self.release_to(hub, &participants);
        self.coordinator.return_local(inject)?;
        self.input_queue.clear();
        self.outgoing = None;
        self.origin_handoff = None;
        self.cross_requested = None;
        Ok(())
    }

    /// Pause retains transport links while synchronously giving up control.
    ///
    /// # Errors
    /// Propagates native release failure.
    pub fn pause(
        &mut self,
        hub: &PeerConnectionHub,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.paused = true;
        self.return_local(hub, inject)
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    fn begin(
        &mut self,
        hub: &PeerConnectionHub,
        target: DeviceId,
        held: Vec<InputEvent>,
        edge_offset: u16,
        boundary: Option<BoundaryMappingRef>,
        now: Instant,
    ) -> io::Result<()> {
        self.last_control_return_reason = None;
        let epoch = channel_epoch(hub, target)?;
        let handoff = if let Some(outgoing) = &self.outgoing {
            if outgoing.handoff.next.current != target
                || outgoing.handoff.edge_offset != edge_offset
                || outgoing.handoff.boundary != boundary
                || outgoing.awaiting_commit
            {
                return Err(invalid("another handoff is committing"));
            }
            self.coordinator.update_handoff_held(held, now)?
        } else {
            match boundary.clone() {
                Some(mapping) => self.coordinator.begin_precise_handoff(
                    target,
                    epoch,
                    held,
                    edge_offset,
                    mapping,
                    now,
                )?,
                None => self
                    .coordinator
                    .begin_handoff_at(target, epoch, held, edge_offset, now)?,
            }
        };
        if handoff.previous.origin == self.local && self.origin_handoff.is_none() {
            self.origin_handoff = Some(OriginHandoff {
                previous: handoff.previous.clone(),
                target,
                edge_offset,
                boundary,
                authorized: false,
                deadline: now + ORIGIN_TIMEOUT,
            });
        }
        self.outgoing = Some(Outgoing {
            handoff: handoff.clone(),
            ready_ack: None,
            awaiting_commit: false,
        });
        self.cross_requested = None;
        let sent = control::send(
            hub,
            target,
            ControlMessage::Prepare {
                handoff: Box::new(handoff.clone()),
            },
        );
        let queued_at = Instant::now();
        if sent.is_ok() && handoff.previous.origin == self.local {
            let session = handoff.next.session_id;
            let epoch = handoff.next.epoch;
            self.flush_probe_window(true, true);
            let elapsed_since_previous_ms = self
                .last_crossing_started
                .map(|previous| queued_at.saturating_duration_since(previous).as_millis());
            self.last_crossing_started = Some(queued_at);
            self.source_probe = Some(CrossingProbe::new(session, epoch, queued_at));
            let link = hub.snapshots().into_iter().find(|link| link.peer == target);
            let edge = self
                .pending_edge
                .take()
                .filter(|(peer, _, _)| *peer == target);
            let detail = format!(
                "target={:016x} since_previous_cross_ms={elapsed_since_previous_ms:?} edge_to_prepare_us={:?} forwarding_us={:?} link_generation={:?} rtt_us={:?} sent_hb={:?} recv_hb={:?}",
                u64::from_le_bytes(target[..8].try_into().expect("device id has eight bytes")),
                edge.as_ref().map(|(_, detected, _)| {
                    queued_at.saturating_duration_since(*detected).as_micros()
                }),
                edge.as_ref().map(|(_, _, elapsed)| elapsed),
                link.as_ref().map(|link| link.generation),
                link.as_ref()
                    .and_then(|link| link.channel.as_ref()?.round_trip_micros),
                link.as_ref()
                    .and_then(|link| link.channel.as_ref().map(|channel| channel.sent_heartbeats)),
                link.as_ref().and_then(|link| link
                    .channel
                    .as_ref()
                    .map(|channel| channel.received_heartbeats)),
            );
            self.probe("prepare_queued", session, epoch, &detail);
        }
        sent
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)] // All authenticated control transitions share one serialized dispatcher.
    fn dispatch(
        &mut self,
        hub: &PeerConnectionHub,
        peer: DeviceId,
        message: ControlMessage,
        now: Instant,
        edge_ready: &impl Fn(DeviceId) -> bool,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
        enter: &mut impl FnMut(&Handoff) -> io::Result<()>,
        deliver: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        match message {
            ControlMessage::CrossRequest {
                session,
                epoch,
                target,
                edge_offset,
                boundary,
            } => {
                let Some(token) = self.route_message(peer, session, epoch, false)? else {
                    return Ok(());
                };
                if token.origin != self.local {
                    return self.send_back(
                        hub,
                        &token,
                        ControlMessage::CrossRequest {
                            session,
                            epoch,
                            target,
                            edge_offset,
                            boundary,
                        },
                    );
                }
                if self.origin_handoff.is_some() {
                    return Ok(());
                }
                self.origin_handoff = Some(OriginHandoff {
                    previous: token.clone(),
                    target,
                    edge_offset,
                    boundary: boundary.clone(),
                    authorized: false,
                    deadline: now + ORIGIN_TIMEOUT,
                });
                self.input_queue.clear();
                self.send_forward(
                    hub,
                    &token,
                    ControlMessage::BeginHandoff {
                        session,
                        epoch,
                        target,
                        edge_offset,
                        boundary,
                        held: self.physical.press_events(),
                    },
                )
            }
            ControlMessage::BeginHandoff {
                session,
                epoch,
                target,
                edge_offset,
                boundary,
                held,
            } => {
                let Some(token) = self.route_message(peer, session, epoch, true)? else {
                    return Ok(());
                };
                if token.current != self.local {
                    return self.send_forward(
                        hub,
                        &token,
                        ControlMessage::BeginHandoff {
                            session,
                            epoch,
                            target,
                            edge_offset,
                            boundary,
                            held,
                        },
                    );
                }
                if !edge_ready(target) || self.paused {
                    return Err(invalid("destination edge unavailable"));
                }
                self.begin(hub, target, held, edge_offset, boundary, now)
            }
            ControlMessage::Prepare { handoff } => {
                let ack = handoff.acknowledgement();
                if self.paused || !edge_ready(peer) {
                    return control::send(hub, peer, ControlMessage::Busy { ack });
                }
                let handoff = *handoff;
                if handoff.next.current == self.local {
                    self.flush_probe_window(false, true);
                    let session = handoff.next.session_id;
                    let epoch = handoff.next.epoch;
                    self.target_probe = Some(CrossingProbe::new(session, epoch, Instant::now()));
                    self.probe("prepare_received", session, epoch, "role=target");
                }
                let fast_return =
                    handoff.previous.origin == self.local && handoff.next.current == self.local;
                if fast_return
                    && let Some(pending) = &self.origin_handoff
                    && (pending.previous != handoff.previous
                        || pending.target != handoff.next.current
                        || pending.edge_offset != handoff.edge_offset
                        || pending.boundary != handoff.boundary)
                {
                    return Err(invalid("prepare belongs to another origin crossing"));
                }
                match self.coordinator.prepare(
                    peer,
                    channel_epoch(hub, peer)?,
                    handoff.clone(),
                    now,
                ) {
                    Ok(ack) if fast_return => {
                        if self.origin_handoff.is_none() {
                            self.origin_handoff = Some(OriginHandoff {
                                previous: handoff.previous.clone(),
                                target: handoff.next.current,
                                edge_offset: handoff.edge_offset,
                                boundary: handoff.boundary.clone(),
                                authorized: false,
                                deadline: now + ORIGIN_TIMEOUT,
                            });
                            self.input_queue.clear();
                        }
                        if self.physical.press_events() == handoff.held {
                            if let Some(origin) = &mut self.origin_handoff {
                                origin.authorized = true;
                            }
                            // The Prepare may use a direct authenticated origin link, but control
                            // authorization still follows the existing token route so every
                            // intermediate participant validates the path and observes the later
                            // commit.
                            self.send_forward(
                                hub,
                                &handoff.previous,
                                ControlMessage::Authorize { ack },
                            )
                        } else {
                            // A press changed while Prepare was in flight. Acknowledged makes the
                            // old target send Ready, after which the established authoritative
                            // held-state refresh updates this reservation before commit.
                            control::send(hub, peer, ControlMessage::Acknowledged { ack })
                        }
                    }
                    Ok(ack) => control::send(hub, peer, ControlMessage::Acknowledged { ack }),
                    Err(_) => control::send(hub, peer, ControlMessage::Busy { ack }),
                }
            }
            ControlMessage::Acknowledged { ack } => {
                let Some(outgoing) = &mut self.outgoing else {
                    return Ok(());
                };
                if peer != outgoing.handoff.next.current
                    || ack != outgoing.handoff.acknowledgement()
                    || outgoing.awaiting_commit
                {
                    return Ok(());
                }
                outgoing.ready_ack = Some(ack.clone());
                let handoff = outgoing.handoff.clone();
                if handoff.previous.origin == self.local {
                    if let Some(probe) = self
                        .source_probe
                        .as_ref()
                        .filter(|probe| probe.matches(ack.session_id, ack.epoch))
                    {
                        let detail =
                            format!("since_prepare_us={}", probe.started.elapsed().as_micros());
                        self.probe("ack_received", ack.session_id, ack.epoch, &detail);
                    }
                    if self.physical.press_events() != handoff.held {
                        return self.begin(
                            hub,
                            peer,
                            self.physical.press_events(),
                            handoff.edge_offset,
                            handoff.boundary.clone(),
                            now,
                        );
                    }
                    if let Some(origin) = &mut self.origin_handoff {
                        origin.authorized = true;
                    }
                    self.authorize(hub, &ack, now, inject)
                } else {
                    self.send_back(
                        hub,
                        &handoff.previous,
                        ControlMessage::Ready {
                            handoff: Box::new(handoff.clone()),
                            ack,
                        },
                    )
                }
            }
            ControlMessage::Ready { handoff, ack } => {
                let Some(token) = self.route_message(
                    peer,
                    handoff.previous.session_id,
                    handoff.previous.epoch,
                    false,
                )?
                else {
                    return Ok(());
                };
                if ack != handoff.acknowledgement() {
                    return Err(invalid("ready acknowledgement differs from held snapshot"));
                }
                if token.origin != self.local {
                    return self.send_back(hub, &token, ControlMessage::Ready { handoff, ack });
                }
                let pending = self
                    .origin_handoff
                    .as_mut()
                    .ok_or_else(|| invalid("origin did not request this crossing"))?;
                if pending.target != handoff.next.current
                    || pending.edge_offset != handoff.edge_offset
                    || pending.boundary != handoff.boundary
                    || pending.previous != handoff.previous
                {
                    return Err(invalid("ready belongs to another crossing"));
                }
                if pending.authorized {
                    return Ok(());
                }
                if self.physical.press_events() != handoff.held {
                    return self.send_forward(
                        hub,
                        &token,
                        ControlMessage::BeginHandoff {
                            session: token.session_id,
                            epoch: token.epoch,
                            target: handoff.next.current,
                            edge_offset: handoff.edge_offset,
                            boundary: handoff.boundary.clone(),
                            held: self.physical.press_events(),
                        },
                    );
                }
                pending.authorized = true;
                self.send_forward(hub, &token, ControlMessage::Authorize { ack })
            }
            ControlMessage::Authorize { ack } => {
                let Some(token) =
                    self.route_message(peer, ack.session_id, ack.epoch.saturating_sub(1), true)?
                else {
                    return Ok(());
                };
                if token.current != self.local {
                    return self.send_forward(hub, &token, ControlMessage::Authorize { ack });
                }
                if let Some(outgoing) = &mut self.outgoing
                    && outgoing.ready_ack.is_none()
                    && !outgoing.awaiting_commit
                    && outgoing.handoff.acknowledgement() == ack
                {
                    // Fast return-to-origin authorization replaces the otherwise redundant
                    // Acknowledged -> Ready round trip. Authentication, epoch, mapping and held
                    // input are still covered by the exact handoff acknowledgement.
                    outgoing.ready_ack = Some(ack.clone());
                }
                self.authorize(hub, &ack, now, inject)
            }
            ControlMessage::Commit { handoff } => {
                if handoff.next.current == self.local {
                    let detail = self
                        .target_probe
                        .as_ref()
                        .filter(|probe| probe.matches(handoff.next.session_id, handoff.next.epoch))
                        .map_or_else(
                            || "since_prepare_us=unknown".to_owned(),
                            |probe| {
                                format!("since_prepare_us={}", probe.started.elapsed().as_micros())
                            },
                        );
                    self.probe(
                        "commit_received",
                        handoff.next.session_id,
                        handoff.next.epoch,
                        &detail,
                    );
                }
                let commit_started = Instant::now();
                if self.recent_commit.as_ref() != Some(&handoff) {
                    self.coordinator.commit_with_entry(
                        peer,
                        channel_epoch(hub, peer)?,
                        &handoff,
                        now,
                        inject,
                        enter,
                    )?;
                    self.recent_commit = Some((*handoff).clone());
                } else if peer != handoff.previous.current
                    || channel_epoch(hub, peer)? != handoff.channel_epoch
                {
                    return Err(invalid("duplicate commit arrived on another connection"));
                }
                if handoff.next.current == self.local {
                    let detail =
                        format!("commit_apply_us={}", commit_started.elapsed().as_micros());
                    self.probe(
                        "commit_applied",
                        handoff.next.session_id,
                        handoff.next.epoch,
                        &detail,
                    );
                }
                control::send(
                    hub,
                    peer,
                    ControlMessage::Committed {
                        ack: handoff.acknowledgement(),
                    },
                )?;
                if handoff.next.origin == self.local {
                    self.finish_origin();
                }
                self.last_error = None;
                Ok(())
            }
            ControlMessage::Committed { ack } => {
                let Some(outgoing) = &self.outgoing else {
                    return Ok(());
                };
                if !outgoing.awaiting_commit
                    || peer != outgoing.handoff.next.current
                    || ack != outgoing.handoff.acknowledgement()
                {
                    return Ok(());
                }
                let handoff = outgoing.handoff.clone();
                self.recent_commit = Some(handoff.clone());
                self.outgoing = None;
                if handoff.next.current == handoff.next.origin && handoff.previous.route.len() == 2
                {
                    self.last_error = None;
                    return Ok(());
                }
                if handoff.previous.origin == self.local {
                    self.finish_origin();
                    self.last_error = None;
                    return Ok(());
                }
                self.send_back(
                    hub,
                    &handoff.previous,
                    ControlMessage::RouteCommit {
                        handoff: Box::new(handoff.clone()),
                    },
                )?;
                self.last_error = None;
                Ok(())
            }
            ControlMessage::RouteCommit { handoff } => {
                let expected = route_neighbor(&handoff.previous, self.local, true);
                if expected != Some(peer) {
                    return Err(invalid(
                        "committed route update has wrong authenticated next hop",
                    ));
                }
                if self.recent_commit.as_ref() != Some(&handoff) {
                    self.coordinator.observe_commit(peer, &handoff, now)?;
                    self.recent_commit = Some((*handoff).clone());
                }
                if handoff.previous.origin == self.local {
                    self.finish_origin();
                    self.last_error = None;
                    return Ok(());
                }
                self.send_back(
                    hub,
                    &handoff.previous,
                    ControlMessage::RouteCommit {
                        handoff: Box::new((*handoff).clone()),
                    },
                )?;
                self.last_error = None;
                Ok(())
            }
            ControlMessage::Busy { ack } => {
                if self.outgoing.as_ref().is_some_and(|pending| {
                    peer == pending.handoff.next.current && ack == pending.handoff.acknowledgement()
                }) {
                    self.last_control_return_reason = Some(DESTINATION_BUSY_RETURN_REASON);
                    self.return_local(hub, inject)?;
                }
                Ok(())
            }
            ControlMessage::Input {
                session,
                epoch,
                sequence,
                event,
            } => {
                // Release and queued input travel independently. Once authority is gone,
                // in-flight input cannot be valid or revive it, so discard the stale frame.
                if self.token().is_none() {
                    return Ok(());
                }
                if self
                    .token()
                    .is_some_and(|token| token.session_id == session && epoch < token.epoch)
                {
                    return Ok(());
                }
                if self
                    .token()
                    .is_some_and(|token| token.current == self.local)
                {
                    // The origin may still have pre-freeze input in flight. Preparing
                    // targets discard it; the origin's authoritative snapshot follows.
                    if self.coordinator.awaiting_handoff() {
                        return Ok(());
                    }
                    let first_motion = matches!(event, InputEvent::MouseMove { .. })
                        && self.target_probe.as_ref().is_some_and(|probe| {
                            probe.matches(session, epoch) && !probe.first_motion
                        });
                    if first_motion {
                        let detail = self
                            .target_probe
                            .as_ref()
                            .map(|probe| {
                                format!(
                                    "since_prepare_us={} sequence={sequence}",
                                    probe.started.elapsed().as_micros()
                                )
                            })
                            .unwrap_or_default();
                        self.probe("first_motion_received", session, epoch, &detail);
                    }
                    let received = Instant::now();
                    let result = self
                        .coordinator
                        .receive_input(peer, session, epoch, sequence, event, now, deliver);
                    let dispatch_us =
                        u64::try_from(received.elapsed().as_micros()).unwrap_or(u64::MAX);
                    if result.is_ok()
                        && matches!(event, InputEvent::MouseMove { .. })
                        && let Some(probe) = self
                            .target_probe
                            .as_mut()
                            .filter(|probe| probe.matches(session, epoch))
                    {
                        probe.record_motion(received, dispatch_us);
                    }
                    if first_motion && result.is_ok() {
                        if let Some(probe) = self.target_probe.as_mut() {
                            probe.first_motion = true;
                        }
                        let detail = format!("dispatch_us={dispatch_us} sequence={sequence}");
                        self.probe("first_motion_dispatched", session, epoch, &detail);
                    }
                    result
                } else {
                    let next = self
                        .coordinator
                        .relay_input(peer, session, epoch, sequence, now)?;
                    control::send(
                        hub,
                        next,
                        ControlMessage::Input {
                            session,
                            epoch,
                            sequence,
                            event,
                        },
                    )
                }
            }
            ControlMessage::Renew {
                session,
                epoch,
                sequence,
            } => {
                // Release and renewal travel independently. A renewal already queued on the
                // authenticated link may arrive just after the matching token was released; it
                // cannot revive authority and is safe to discard.
                if self.token().is_none() {
                    return Ok(());
                }
                if self
                    .token()
                    .is_some_and(|token| token.session_id == session && epoch < token.epoch)
                {
                    return Ok(());
                }
                self.coordinator
                    .renew(peer, session, epoch, sequence, now)?;
                if let Some(token) = self.token()
                    && let Some(next) = route_neighbor(token, self.local, true)
                {
                    control::send(
                        hub,
                        next,
                        ControlMessage::Renew {
                            session,
                            epoch,
                            sequence,
                        },
                    )?;
                }
                Ok(())
            }
            ControlMessage::Release { session, epoch } => {
                let matches =
                    self.control_identity()
                        .is_some_and(|(current_session, current_epoch)| {
                            session == current_session && epoch >= current_epoch
                        });
                if matches && self.participants().contains(&peer) {
                    self.return_local(hub, inject)?;
                }
                Ok(())
            }
        }
    }

    fn authorize(
        &mut self,
        hub: &PeerConnectionHub,
        ack: &HandoffAck,
        now: Instant,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        let pending = self
            .outgoing
            .as_ref()
            .ok_or_else(|| invalid("authorization without outgoing handoff"))?;
        if pending.ready_ack.as_ref() != Some(ack) || pending.awaiting_commit {
            return Err(invalid(
                "authorization is not the latest ready acknowledgement",
            ));
        }
        let target = pending.handoff.next.current;
        let commit =
            self.coordinator
                .acknowledge(target, channel_epoch(hub, target)?, ack, now, inject)?;
        self.outgoing = Some(Outgoing {
            handoff: commit.clone(),
            ready_ack: Some(ack.clone()),
            awaiting_commit: true,
        });
        let sent = control::send(
            hub,
            target,
            ControlMessage::Commit {
                handoff: Box::new(commit),
            },
        );
        if sent.is_ok()
            && let Some(probe) = self
                .source_probe
                .as_ref()
                .filter(|probe| probe.matches(ack.session_id, ack.epoch))
        {
            let detail = format!("since_prepare_us={}", probe.started.elapsed().as_micros());
            self.probe("commit_queued", ack.session_id, ack.epoch, &detail);
        }
        sent
    }

    fn finish_origin(&mut self) {
        self.origin_handoff = None;
        if self.token().is_none() {
            self.flush_probe_window(true, true);
            self.input_queue.clear();
            if let Some(probe) = self.source_probe.take() {
                let detail = format!(
                    "elapsed_us={} first_motion={}",
                    probe.started.elapsed().as_micros(),
                    probe.first_motion
                );
                self.probe("origin_restored", probe.session, probe.epoch, &detail);
            }
        }
        self.next_renewal = Instant::now();
    }

    fn route_message(
        &self,
        peer: DeviceId,
        session: [u8; 16],
        epoch: u64,
        upstream: bool,
    ) -> io::Result<Option<ControlToken>> {
        // Route-scoped messages already queued before Release cannot establish authority on
        // their own. Treat the tokenless case as a recovered ordering race, while retaining
        // strict validation whenever a live route exists.
        let Some(token) = self.token() else {
            return Ok(None);
        };
        if token.session_id != session
            || token.epoch != epoch
            || route_neighbor(token, self.local, !upstream) != Some(peer)
        {
            return Err(invalid(
                "route message does not match session, epoch or authenticated path",
            ));
        }
        Ok(Some(token.clone()))
    }

    fn send_back(
        &self,
        hub: &PeerConnectionHub,
        token: &ControlToken,
        message: ControlMessage,
    ) -> io::Result<()> {
        let peer = route_neighbor(token, self.local, false)
            .ok_or_else(|| invalid("route has no upstream peer"))?;
        control::send(hub, peer, message)
    }

    fn send_forward(
        &self,
        hub: &PeerConnectionHub,
        token: &ControlToken,
        message: ControlMessage,
    ) -> io::Result<()> {
        let peer = route_neighbor(token, self.local, true)
            .ok_or_else(|| invalid("route has no downstream peer"))?;
        control::send(hub, peer, message)
    }

    fn flush_input(&mut self, hub: &PeerConnectionHub, now: Instant) -> io::Result<()> {
        if let Some(pending) = &self.origin_handoff {
            // For a direct departure from the physical origin, acknowledge() has
            // already installed the new token and queued Commit on this same ordered
            // channel. Input may follow it immediately; waiting for Committed costs
            // another half round trip and makes the pointer pause at the edge.
            let commit_queued_to_direct_target = pending.authorized
                && self.token().is_some_and(|token| {
                    token.origin == self.local && token.current == pending.target
                });
            if !commit_queued_to_direct_target {
                return Ok(());
            }
        }
        let Some(token) = self.token().cloned() else {
            self.input_queue.clear();
            return Ok(());
        };
        if token.origin != self.local {
            return Ok(());
        }
        let Some(peer) = self.coordinator.input_destination(now)? else {
            return Ok(());
        };
        for _ in 0..4 {
            let Some(event) = self.input_queue.front().copied() else {
                break;
            };
            let sequence = self
                .input_sequence
                .checked_add(1)
                .ok_or_else(|| invalid("origin input sequence exhausted"))?;
            match control::send(
                hub,
                peer,
                ControlMessage::Input {
                    session: token.session_id,
                    epoch: token.epoch,
                    sequence,
                    event,
                },
            ) {
                Ok(()) => {
                    self.input_sequence = sequence;
                    self.input_queue.pop_front();
                    if matches!(event, InputEvent::MouseMove { .. }) {
                        let elapsed = self.source_probe.as_mut().and_then(|probe| {
                            if !probe.matches(token.session_id, token.epoch) {
                                return None;
                            }
                            probe.record_motion(Instant::now(), 0);
                            if probe.first_motion {
                                return None;
                            }
                            probe.first_motion = true;
                            Some(probe.started.elapsed().as_micros())
                        });
                        if let Some(elapsed) = elapsed {
                            let detail = format!(
                                "since_prepare_us={elapsed} sequence={sequence} queued_after={}",
                                self.input_queue.len()
                            );
                            self.probe(
                                "first_motion_queued",
                                token.session_id,
                                token.epoch,
                                &detail,
                            );
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn renew(&mut self, hub: &PeerConnectionHub, now: Instant) -> io::Result<()> {
        if self.origin_handoff.is_some() || now < self.next_renewal {
            return Ok(());
        }
        let Some(token) = self
            .token()
            .filter(|token| token.origin == self.local)
            .cloned()
        else {
            return Ok(());
        };
        let sequence = self
            .renewal_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("renewal sequence exhausted"))?;
        let send = self.send_forward(
            hub,
            &token,
            ControlMessage::Renew {
                session: token.session_id,
                epoch: token.epoch,
                sequence,
            },
        );
        if let Err(error) = send {
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
        self.renewal_sequence = sequence;
        self.coordinator.renew_origin(now)?;
        self.next_renewal = now + RENEW_INTERVAL;
        Ok(())
    }

    fn participants(&self) -> BTreeSet<DeviceId> {
        let mut result = BTreeSet::new();
        if let Some(token) = self.token() {
            result.extend(&token.route);
        }
        if let Some(handoff) = self.coordinator.pending_handoff() {
            result.extend(&handoff.previous.route);
            result.extend(&handoff.next.route);
        }
        if let Some(pending) = &self.outgoing {
            result.extend(&pending.handoff.previous.route);
            result.extend(&pending.handoff.next.route);
        }
        if let Some(pending) = &self.origin_handoff {
            result.extend(&pending.previous.route);
            result.insert(pending.target);
        }
        result.remove(&self.local);
        result
    }

    fn control_identity(&self) -> Option<([u8; 16], u64)> {
        if let Some(pending) = &self.origin_handoff {
            // A cancellation must also reach a destination already reserved at the
            // next epoch, before its commit notification has reached the origin.
            return Some((
                pending.previous.session_id,
                pending.previous.epoch.saturating_add(1),
            ));
        }
        self.outgoing
            .as_ref()
            .map(|pending| &pending.handoff.next)
            .or_else(|| {
                self.coordinator
                    .pending_handoff()
                    .map(|handoff| &handoff.next)
            })
            .or_else(|| self.token())
            .map(|token| (token.session_id, token.epoch))
    }

    fn release_to(&self, hub: &PeerConnectionHub, peers: &BTreeSet<DeviceId>) {
        if let Some((session, epoch)) = self.control_identity() {
            for peer in peers {
                let _ = control::send(hub, *peer, ControlMessage::Release { session, epoch });
            }
        }
    }
}

fn route_neighbor(token: &ControlToken, local: DeviceId, forward: bool) -> Option<DeviceId> {
    let index = token.route.iter().position(|id| *id == local)?;
    token
        .route
        .get(if forward {
            index + 1
        } else {
            index.checked_sub(1)?
        })
        .copied()
}

fn channel_epoch(hub: &PeerConnectionHub, peer: DeviceId) -> io::Result<u64> {
    hub.with_peer(peer, |channel| {
        let snapshot = channel.snapshot();
        if snapshot.state != ChannelState::Connected {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "peer disconnected",
            ));
        }
        Ok(snapshot.channel_epoch)
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::hub::tests::Network;
    use crate::cross_screen::{KeyTransition, MouseButton};
    use crate::workspace::{GridPoint, LayoutRevision, WorkspaceLayout, edge};

    struct Rig {
        hosts: Vec<ControlHost>,
        events: Vec<Vec<InputEvent>>,
        network: Network,
    }

    impl Rig {
        fn new(count: usize) -> Self {
            Self::from_network(Network::new(count))
        }

        fn from_network(network: Network) -> Self {
            let count = network.ids.len();
            network.wait_connected();
            let members: BTreeSet<_> = network.ids.iter().copied().collect();
            let edges: BTreeSet<_> = network
                .hubs
                .iter()
                .enumerate()
                .flat_map(|(index, hub)| {
                    let local = network.ids[index];
                    hub.snapshots()
                        .into_iter()
                        .map(move |peer| edge(local, peer.peer))
                })
                .collect();
            let points = [
                GridPoint { x: 0, y: 0 },
                GridPoint { x: 1, y: 0 },
                GridPoint { x: 1, y: 1 },
                GridPoint { x: 0, y: 1 },
            ];
            let positions = network.ids.iter().copied().zip(points).collect();
            let layout = WorkspaceLayout::new(
                [3; 16],
                LayoutRevision {
                    counter: 1,
                    writer: network.ids[0],
                },
                &positions,
                &members,
                &edges,
            )
            .unwrap();
            let hosts = network
                .ids
                .iter()
                .map(|local| {
                    ControlHost::new(
                        *local,
                        CrossScreenCoordinator::new(
                            *local,
                            layout.clone(),
                            &members,
                            edges.clone(),
                        )
                        .unwrap(),
                    )
                })
                .collect();
            Self {
                hosts,
                events: vec![vec![]; count],
                network,
            }
        }

        fn precise_star() -> Self {
            let network = Network::new(3);
            network.wait_connected();
            let mapping = |id: &str, revision| BoundaryMappingRef {
                boundary_id: id.to_owned(),
                mapping_revision: revision,
            };
            let authorities = [
                BTreeMap::from([
                    (network.ids[1], BTreeSet::from([mapping("host-top", 4)])),
                    (network.ids[2], BTreeSet::from([mapping("host-right", 9)])),
                ]),
                BTreeMap::from([(network.ids[0], BTreeSet::from([mapping("host-top", 4)]))]),
                BTreeMap::from([(network.ids[0], BTreeSet::from([mapping("host-right", 9)]))]),
            ];
            let hosts = network
                .ids
                .iter()
                .zip(authorities)
                .map(|(local, mappings)| {
                    ControlHost::new(
                        *local,
                        CrossScreenCoordinator::new_precise(*local, mappings).unwrap(),
                    )
                })
                .collect();
            Self {
                hosts,
                events: vec![vec![]; 3],
                network,
            }
        }

        fn pump(&mut self) {
            for (index, host) in self.hosts.iter_mut().enumerate() {
                host.tick(
                    &self.network.hubs[index],
                    Instant::now(),
                    &|_| true,
                    &mut |events| {
                        self.events[index].extend_from_slice(events);
                        Ok(())
                    },
                )
                .unwrap();
            }
        }

        fn until(&mut self, predicate: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                self.pump();
                if predicate(self) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "host convergence failed: {:?}",
                    self.hosts
                        .iter()
                        .map(|host| (host.last_error(), host.token(), host.handoff_pending()))
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn cross(&mut self, from: usize, to: usize) {
            self.hosts[from]
                .cross(
                    &self.network.hubs[from],
                    self.network.ids[to],
                    Instant::now(),
                    &|_| true,
                )
                .unwrap();
            let origin = self.hosts[from]
                .token()
                .map_or(self.network.ids[from], |token| token.origin);
            self.until(|rig| {
                rig.hosts.iter().all(|host| !host.handoff_pending())
                    && if origin == rig.network.ids[to] {
                        rig.hosts.iter().all(|host| host.token().is_none())
                    } else {
                        rig.hosts[to]
                            .token()
                            .is_some_and(|token| token.current == rig.network.ids[to])
                            && rig
                                .hosts
                                .iter()
                                .filter_map(ControlHost::token)
                                .all(|token| token.current == rig.network.ids[to])
                    }
            });
        }

        fn physical(&mut self, index: usize, event: InputEvent) -> bool {
            self.hosts[index]
                .physical_input(&self.network.hubs[index], event, &mut |events| {
                    self.events[index].extend_from_slice(events);
                    Ok(())
                })
                .unwrap()
        }
    }

    #[test]
    fn first_crossing_probe_correlates_handoff_and_first_motion_on_both_hosts() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);
        let motion = InputEvent::MouseMove { x: 9, y: 4 };
        assert!(rig.physical(0, motion));
        rig.until(|rig| rig.events[1].contains(&motion));

        let source_cadence = rig.network.hubs[0]
            .with_peer(rig.network.ids[1], |channel| {
                Ok(channel.take_control_cadence())
            })
            .unwrap();
        let target_cadence = rig.network.hubs[1]
            .with_peer(rig.network.ids[0], |channel| {
                Ok(channel.take_control_cadence())
            })
            .unwrap();
        assert!(source_cadence.enqueued.moves > 0);
        assert!(source_cadence.sent.moves > 0);
        assert!(target_cadence.received.moves > 0);
        assert!(target_cadence.dequeued.moves > 0);

        let source = rig.hosts[0].drain_probe_events();
        let target = rig.hosts[1].drain_probe_events();
        for phase in [
            "prepare_queued",
            "ack_received",
            "commit_queued",
            "first_motion_queued",
        ] {
            assert!(
                source
                    .iter()
                    .any(|event| event.contains(&format!("phase={phase} "))),
                "missing {phase}: {source:?}"
            );
        }
        for phase in [
            "prepare_received",
            "commit_received",
            "commit_applied",
            "first_motion_received",
            "first_motion_dispatched",
        ] {
            assert!(
                target
                    .iter()
                    .any(|event| event.contains(&format!("phase={phase} "))),
                "missing {phase}: {target:?}"
            );
        }
        let token = rig.hosts[0].token().unwrap();
        let identity = format!(
            "session={:032x} epoch={}",
            u128::from_le_bytes(token.session_id),
            token.epoch
        );
        assert!(source.iter().all(|event| event.contains(&identity)));
        assert!(target.iter().all(|event| event.contains(&identity)));
        assert!(rig.hosts[0].drain_probe_events().is_empty());
    }

    #[test]
    fn crossing_probe_emits_one_bounded_three_second_motion_summary() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);
        let motion = InputEvent::MouseMove { x: 9, y: 4 };
        assert!(rig.physical(0, motion));
        rig.until(|rig| rig.events[1].contains(&motion));
        for host in &mut rig.hosts {
            let source = host.source_probe.is_some();
            let probe = if source {
                host.source_probe.as_mut().unwrap()
            } else {
                host.target_probe.as_mut().unwrap()
            };
            probe.first_motion_at = Instant::now().checked_sub(Duration::from_secs(3));
            host.flush_probe_window(source, false);
            let events = host.drain_probe_events();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.contains("_motion_window "))
                    .count(),
                1
            );
            assert!(events.iter().any(|event| event.contains("motion_count=1")));
            host.flush_probe_window(source, false);
            assert!(host.drain_probe_events().is_empty());
        }
    }

    #[test]
    fn precise_star_routes_two_host_edges_with_one_control_token() {
        let mut rig = Rig::precise_star();
        rig.hosts[0]
            .cross_precise(
                &rig.network.hubs[0],
                rig.network.ids[1],
                20_000,
                BoundaryMappingRef {
                    boundary_id: "host-top".into(),
                    mapping_revision: 4,
                },
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        rig.until(|rig| {
            rig.hosts[1]
                .token()
                .is_some_and(|token| token.current == rig.network.ids[1])
                && rig.hosts.iter().all(|host| !host.handoff_pending())
        });
        let first_session = rig.hosts[1].token().unwrap().session_id;

        rig.hosts[1]
            .cross_precise(
                &rig.network.hubs[1],
                rig.network.ids[0],
                20_000,
                BoundaryMappingRef {
                    boundary_id: "host-top".into(),
                    mapping_revision: 4,
                },
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        rig.until(|rig| rig.hosts.iter().all(|host| host.token().is_none()));

        rig.hosts[0]
            .cross_precise(
                &rig.network.hubs[0],
                rig.network.ids[2],
                45_000,
                BoundaryMappingRef {
                    boundary_id: "host-right".into(),
                    mapping_revision: 9,
                },
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        rig.until(|rig| {
            rig.hosts[2]
                .token()
                .is_some_and(|token| token.current == rig.network.ids[2])
                && rig.hosts.iter().all(|host| !host.handoff_pending())
        });
        assert_ne!(rig.hosts[2].token().unwrap().session_id, first_session);
        assert!(rig.hosts[1].token().is_none());
    }

    #[test]
    fn direct_return_to_physical_origin_skips_the_cross_request_round_trip() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);

        rig.hosts[1]
            .cross(
                &rig.network.hubs[1],
                rig.network.ids[0],
                Instant::now(),
                &|_| true,
            )
            .unwrap();

        assert!(rig.hosts[1].outgoing.is_some());
        assert!(rig.hosts[1].cross_requested.is_none());
        assert!(rig.hosts[0].origin_handoff.is_none());
        rig.until(|rig| {
            rig.hosts
                .iter()
                .all(|host| host.token().is_none() && !host.handoff_pending())
        });
    }

    #[test]
    fn destination_busy_returns_local_as_notice_without_recording_an_error() {
        let mut rig = Rig::new(2);
        rig.hosts[1]
            .pause(&rig.network.hubs[1], &mut |events| {
                rig.events[1].extend_from_slice(events);
                Ok(())
            })
            .unwrap();
        rig.hosts[0]
            .cross(
                &rig.network.hubs[0],
                rig.network.ids[1],
                Instant::now(),
                &|_| true,
            )
            .unwrap();

        rig.until(|rig| {
            !rig.hosts[0].handoff_pending()
                && rig.hosts[0].last_control_return_reason() == Some(DESTINATION_BUSY_RETURN_REASON)
        });

        assert!(rig.hosts.iter().all(|host| host.token().is_none()));
        assert!(rig.hosts.iter().all(|host| host.last_error().is_none()));
    }

    #[test]
    fn direct_return_refreshes_a_press_changed_while_prepare_is_in_flight() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);
        assert!(rig.physical(0, left(KeyTransition::Down)));
        rig.until(|rig| rig.events[1].last() == Some(&left(KeyTransition::Down)));

        rig.hosts[1]
            .cross(
                &rig.network.hubs[1],
                rig.network.ids[0],
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        assert!(rig.physical(0, left(KeyTransition::Up)));

        rig.until(|rig| {
            rig.hosts
                .iter()
                .all(|host| host.token().is_none() && !host.handoff_pending())
        });
        assert_eq!(rig.events[1].last(), Some(&left(KeyTransition::Up)));
        assert!(
            rig.hosts.iter().all(|host| host.last_error().is_none()),
            "unexpected host errors: {:?}",
            rig.hosts
                .iter()
                .map(ControlHost::last_error)
                .collect::<Vec<_>>()
        );
    }

    fn left(transition: KeyTransition) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            transition,
        }
    }

    #[test]
    fn a_sparse_three_device_route_relays_input_without_origin_target_trust() {
        let mut rig = Rig::from_network(Network::with_links(3, |a, b| a.abs_diff(b) == 1));
        assert!(
            rig.network.hubs[0]
                .with_peer(rig.network.ids[2], |_| Ok(()))
                .is_err()
        );
        rig.cross(0, 1);
        rig.cross(1, 2);
        let intermediate_events = rig.events[1].len();
        rig.physical(0, left(KeyTransition::Down));
        rig.until(|rig| rig.events[2].last() == Some(&left(KeyTransition::Down)));
        rig.physical(0, left(KeyTransition::Up));
        rig.until(|rig| rig.events[2].last() == Some(&left(KeyTransition::Up)));
        assert_eq!(
            rig.events[1].len(),
            intermediate_events,
            "relay must not inject input locally"
        );
        rig.cross(2, 1);
        assert_eq!(
            rig.hosts[0].token().unwrap().route,
            vec![rig.network.ids[0], rig.network.ids[1]]
        );
        assert!(rig.hosts[2].token().is_none());
    }

    #[test]
    fn four_real_tls_hosts_complete_a_rectangle_with_one_token_and_live_input() {
        let mut rig = Rig::new(4);
        rig.cross(0, 1);
        let session = rig.hosts[0].token().unwrap().session_id;
        assert!(rig.physical(0, left(KeyTransition::Down)));
        rig.until(|rig| rig.events[1].contains(&left(KeyTransition::Down)));
        rig.cross(1, 2);
        assert_eq!(rig.hosts[2].token().unwrap().session_id, session);
        assert_eq!(rig.events[1].last(), Some(&left(KeyTransition::Up)));
        assert_eq!(rig.events[2].last(), Some(&left(KeyTransition::Down)));
        rig.cross(2, 3);
        assert_eq!(rig.hosts[3].token().unwrap().session_id, session);
        assert!(rig.physical(0, left(KeyTransition::Up)));
        rig.until(|rig| rig.events[3].last() == Some(&left(KeyTransition::Up)));
        rig.cross(3, 0);
        rig.until(|rig| {
            rig.hosts
                .iter()
                .all(|host| host.token().is_none() && !host.handoff_pending())
        });
        assert!(
            rig.hosts.iter().all(|host| host.last_error().is_none()),
            "unexpected host errors: {:?}",
            rig.hosts
                .iter()
                .map(ControlHost::last_error)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn origin_refreshes_a_release_before_authorizing_the_third_device() {
        let mut rig = Rig::new(3);
        rig.cross(0, 1);
        rig.physical(0, left(KeyTransition::Down));
        rig.until(|rig| rig.events[1].last() == Some(&left(KeyTransition::Down)));
        rig.hosts[1]
            .cross(
                &rig.network.hubs[1],
                rig.network.ids[2],
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        rig.until(|rig| rig.hosts[0].origin_handoff.is_some());
        rig.physical(0, left(KeyTransition::Up));
        rig.until(|rig| {
            rig.hosts.iter().all(|host| !host.handoff_pending()) && rig.hosts[2].token().is_some()
        });
        assert!(!rig.events[2].contains(&left(KeyTransition::Down)));
        assert_eq!(rig.events[1].last(), Some(&left(KeyTransition::Up)));
    }

    #[test]
    fn routed_renewals_keep_all_participants_alive_and_pause_releases_the_route() {
        let mut rig = Rig::new(3);
        rig.cross(0, 1);
        rig.cross(1, 2);
        rig.physical(0, left(KeyTransition::Down));
        rig.until(|rig| rig.events[2].last() == Some(&left(KeyTransition::Down)));
        let until = Instant::now() + Duration::from_millis(2200);
        while Instant::now() < until {
            rig.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(rig.hosts.iter().all(|host| host.token().is_some()));
        rig.hosts[1]
            .pause(&rig.network.hubs[1], &mut |events| {
                rig.events[1].extend_from_slice(events);
                Ok(())
            })
            .unwrap();
        rig.until(|rig| rig.hosts.iter().all(|host| host.token().is_none()));
        assert_eq!(rig.events[2].last(), Some(&left(KeyTransition::Up)));
    }

    #[test]
    fn adjacent_relative_motion_is_summed_without_crossing_button_order() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);
        rig.events[1].clear();
        assert!(rig.physical(0, InputEvent::MouseMove { x: 7, y: -3 }));
        assert!(rig.physical(0, InputEvent::MouseMove { x: 11, y: 5 }));
        assert!(rig.physical(0, left(KeyTransition::Down)));
        assert!(rig.physical(0, InputEvent::MouseMove { x: -2, y: 4 }));
        rig.until(|rig| rig.events[1].len() >= 3);
        assert_eq!(
            rig.events[1],
            vec![
                InputEvent::MouseMove { x: 18, y: 2 },
                left(KeyTransition::Down),
                InputEvent::MouseMove { x: -2, y: 4 },
            ]
        );
    }

    #[test]
    fn motion_during_initial_handoff_is_carried_into_the_target() {
        let mut rig = Rig::new(2);
        rig.hosts[0]
            .cross(
                &rig.network.hubs[0],
                rig.network.ids[1],
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        assert!(rig.hosts[0].origin_handoff.is_some());

        assert!(rig.physical(0, InputEvent::MouseMove { x: 7, y: -3 }));
        assert!(rig.physical(0, InputEvent::MouseMove { x: 11, y: 5 }));

        rig.until(|rig| {
            rig.hosts.iter().all(|host| !host.handoff_pending())
                && rig.events[1].contains(&InputEvent::MouseMove { x: 18, y: 2 })
        });
    }

    #[test]
    fn direct_target_receives_carried_motion_before_commit_round_trip_finishes() {
        let mut rig = Rig::new(2);
        rig.hosts[0]
            .cross(
                &rig.network.hubs[0],
                rig.network.ids[1],
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        assert!(rig.physical(0, InputEvent::MouseMove { x: 9, y: 4 }));

        let deadline = Instant::now() + Duration::from_secs(4);
        while !rig.hosts[0]
            .outgoing
            .as_ref()
            .is_some_and(|pending| pending.awaiting_commit)
        {
            rig.hosts[1]
                .tick(
                    &rig.network.hubs[1],
                    Instant::now(),
                    &|_| true,
                    &mut |events| {
                        rig.events[1].extend_from_slice(events);
                        Ok(())
                    },
                )
                .unwrap();
            rig.hosts[0]
                .tick(
                    &rig.network.hubs[0],
                    Instant::now(),
                    &|_| true,
                    &mut |events| {
                        rig.events[0].extend_from_slice(events);
                        Ok(())
                    },
                )
                .unwrap();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }

        while !rig.events[1].contains(&InputEvent::MouseMove { x: 9, y: 4 }) {
            rig.hosts[1]
                .tick(
                    &rig.network.hubs[1],
                    Instant::now(),
                    &|_| true,
                    &mut |events| {
                        rig.events[1].extend_from_slice(events);
                        Ok(())
                    },
                )
                .unwrap();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(rig.hosts[0].origin_handoff.is_some());
    }

    #[test]
    fn short_target_scheduler_stall_does_not_drop_live_control() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);
        let origin = 0;
        let target = 1;

        // A busy Windows target can leave its 5 ms product pump unscheduled for a few
        // tens of milliseconds. Keep producing realistic pointer/button traffic while
        // only the source pump runs; transport buffering must not turn that temporary
        // scheduling delay into a connection failure and forced return-to-origin.
        for index in 0..24 {
            assert!(
                rig.physical(
                    origin,
                    InputEvent::MouseMove {
                        x: 3 + index,
                        y: -2,
                    },
                ),
                "source stopped forwarding at iteration {index}: token={:?}, error={:?}, origin_links={:?}, target_links={:?}",
                rig.hosts[origin].token(),
                rig.hosts[origin].last_error(),
                rig.network.hubs[origin].snapshots(),
                rig.network.hubs[target].snapshots(),
            );
            if index % 4 == 0 {
                assert!(rig.physical(origin, left(KeyTransition::Down)));
                assert!(rig.physical(origin, left(KeyTransition::Up)));
            }
            rig.hosts[origin]
                .tick(
                    &rig.network.hubs[origin],
                    Instant::now(),
                    &|_| true,
                    &mut |events| {
                        rig.events[origin].extend_from_slice(events);
                        Ok(())
                    },
                )
                .unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }

        rig.until(|rig| {
            rig.events[target]
                .iter()
                .any(|event| matches!(event, InputEvent::MouseMove { .. }))
        });
        assert!(rig.hosts[origin].token().is_some());
        assert!(rig.hosts[target].token().is_some());
        assert!(rig.network.hubs.iter().all(|hub| {
            hub.snapshots().iter().all(|peer| {
                peer.channel
                    .as_ref()
                    .is_some_and(|channel| channel.state == ChannelState::Connected)
            })
        }));
    }

    #[test]
    fn unrelated_connection_loss_does_not_cancel_a_live_origin_target_pair() {
        let mut rig = Rig::new(3);
        rig.cross(0, 1);
        rig.network.hubs[2].stop();
        rig.until(|rig| !rig.hosts[0].links.contains_key(&rig.network.ids[2]));
        assert!(rig.hosts[0].token().is_some());
        assert!(rig.hosts[1].token().is_some());
        rig.physical(0, left(KeyTransition::Down));
        rig.until(|rig| rig.events[1].last() == Some(&left(KeyTransition::Down)));
        rig.network.hubs[0].stop();
        rig.until(|rig| rig.hosts[1].token().is_none());
        assert_eq!(rig.events[1].last(), Some(&left(KeyTransition::Up)));
    }

    #[test]
    fn queued_renewal_after_release_cannot_fault_or_revive_control() {
        let mut rig = Rig::new(2);
        control::send(
            &rig.network.hubs[0],
            rig.network.ids[1],
            ControlMessage::Renew {
                session: [9; 16],
                epoch: 7,
                sequence: 1,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            rig.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(rig.hosts.iter().all(|host| host.token().is_none()));
        assert!(rig.hosts.iter().all(|host| host.last_error().is_none()));
    }

    #[test]
    fn queued_input_after_release_cannot_fault_or_revive_control() {
        let mut rig = Rig::new(2);
        control::send(
            &rig.network.hubs[0],
            rig.network.ids[1],
            ControlMessage::Input {
                session: [8; 16],
                epoch: 6,
                sequence: 1,
                event: InputEvent::MouseMove { x: 7, y: -3 },
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            rig.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(rig.hosts.iter().all(|host| host.token().is_none()));
        assert!(rig.hosts.iter().all(|host| host.last_error().is_none()));
    }

    #[test]
    fn queued_route_request_after_release_cannot_fault_or_revive_control() {
        let mut rig = Rig::new(2);
        control::send(
            &rig.network.hubs[0],
            rig.network.ids[1],
            ControlMessage::CrossRequest {
                session: [7; 16],
                epoch: 5,
                target: rig.network.ids[0],
                edge_offset: 24_000,
                boundary: None,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            rig.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(rig.hosts.iter().all(|host| host.token().is_none()));
        assert!(rig.hosts.iter().all(|host| host.last_error().is_none()));
    }

    #[test]
    fn successful_handoff_clears_recovered_control_errors() {
        let mut rig = Rig::new(2);
        for host in &mut rig.hosts {
            host.last_error = Some("previous control failure".to_owned());
        }
        rig.cross(0, 1);
        assert!(rig.hosts.iter().all(|host| host.last_error().is_none()));
    }

    #[test]
    fn mismatched_input_during_live_control_remains_an_error() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);
        let token = rig.hosts[1].token().unwrap().clone();
        control::send(
            &rig.network.hubs[0],
            rig.network.ids[1],
            ControlMessage::Input {
                session: [0xA5; 16],
                epoch: token.epoch,
                sequence: 1,
                event: InputEvent::MouseMove { x: 1, y: 1 },
            },
        )
        .unwrap();
        rig.until(|rig| rig.hosts[1].last_error().is_some());
        assert_eq!(
            rig.hosts[1].last_error(),
            Some("input does not match active target, epoch or route")
        );
    }

    #[test]
    fn transport_heartbeats_cannot_keep_an_abandoned_control_lease_alive() {
        let mut rig = Rig::new(2);
        rig.cross(0, 1);
        rig.physical(0, left(KeyTransition::Down));
        rig.until(|rig| rig.events[1].last() == Some(&left(KeyTransition::Down)));
        let deadline = Instant::now() + Duration::from_secs(3);
        while rig.hosts[1].token().is_some() {
            rig.hosts[1]
                .tick(
                    &rig.network.hubs[1],
                    Instant::now(),
                    &|_| true,
                    &mut |events| {
                        rig.events[1].extend_from_slice(events);
                        Ok(())
                    },
                )
                .unwrap();
            assert!(
                Instant::now() < deadline,
                "lease survived without origin renewals"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(rig.events[1].last(), Some(&left(KeyTransition::Up)));
        assert!(rig.network.hubs[1].snapshots().iter().all(|peer| {
            peer.channel
                .as_ref()
                .is_some_and(|channel| channel.state == ChannelState::Connected)
        }));
        rig.pump();
        assert!(rig.hosts[0].token().is_none());
    }

    #[test]
    fn unprocessed_prepare_cannot_restart_control_after_origin_timeout() {
        let mut rig = Rig::new(2);
        rig.hosts[0]
            .cross(
                &rig.network.hubs[0],
                rig.network.ids[1],
                Instant::now(),
                &|_| true,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while rig.hosts[0].handoff_pending() {
            rig.hosts[0]
                .tick(&rig.network.hubs[0], Instant::now(), &|_| true, &mut |_| {
                    Ok(())
                })
                .unwrap();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.until(|rig| rig.hosts.iter().all(|host| !host.handoff_pending()));
        assert!(rig.hosts.iter().all(|host| host.token().is_none()));
        assert!(rig.events.iter().all(Vec::is_empty));
        // A subsequent crossing uses a fresh epoch and is not poisoned by the old ack.
        rig.cross(0, 1);
    }
}
