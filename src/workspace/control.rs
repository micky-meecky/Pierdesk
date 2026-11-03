//! One control lease per computer, independent of the number of transport links.
//!
//! The host serializes calls and supplies the authenticated channel epoch. Input
//! callbacks are synchronous: a handoff cannot commit before the old injector
//! acknowledges release. This module does not install another input hook.
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::cross_screen::hub::PeerConnectionHub;
use crate::cross_screen::{
    ApplicationProtocol, InputEvent, InputLedger, KeyTransition, LedgerResult,
};

use super::{DeviceId, Edge, LayoutRevision, WorkspaceLayout, edge};

const LEASE: Duration = Duration::from_secs(2);
const HANDOFF_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_HELD: usize = 261;
const MAX_MESSAGE: usize = 16 * 1024 - 1;

/// Identifies the exact confirmed portal mapping used by one handoff.
///
/// This is pair-scoped on purpose: every device can validate the boundary it shares with the
/// authenticated neighbor without requiring every computer to store an identical global layout.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryMappingRef {
    pub boundary_id: String,
    pub mapping_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ControlMessage {
    CrossRequest {
        session: [u8; 16],
        epoch: u64,
        target: DeviceId,
        edge_offset: u16,
        boundary: Option<BoundaryMappingRef>,
    },
    BeginHandoff {
        session: [u8; 16],
        epoch: u64,
        target: DeviceId,
        edge_offset: u16,
        boundary: Option<BoundaryMappingRef>,
        #[serde(with = "held_wire")]
        held: Vec<InputEvent>,
    },
    Ready {
        handoff: Box<Handoff>,
        ack: HandoffAck,
    },
    Authorize {
        ack: HandoffAck,
    },
    Prepare {
        handoff: Box<Handoff>,
    },
    Acknowledged {
        ack: HandoffAck,
    },
    Busy {
        ack: HandoffAck,
    },
    Commit {
        handoff: Box<Handoff>,
    },
    Committed {
        ack: HandoffAck,
    },
    RouteCommit {
        handoff: Box<Handoff>,
    },
    Input {
        session: [u8; 16],
        epoch: u64,
        sequence: u64,
        event: InputEvent,
    },
    Renew {
        session: [u8; 16],
        epoch: u64,
        sequence: u64,
    },
    Release {
        session: [u8; 16],
        epoch: u64,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    schema: u32,
    channel_epoch: u64,
    message: ControlMessage,
}

/// Queue control on the existing authenticated hub link, bound to this connection.
///
/// # Errors
/// Rejects absent links, oversized payloads and bounded-queue backpressure.
pub fn send(hub: &PeerConnectionHub, peer: DeviceId, message: ControlMessage) -> io::Result<()> {
    hub.with_peer(peer, |channel| {
        let packet = Packet {
            schema: 1,
            channel_epoch: channel.snapshot().channel_epoch,
            message,
        };
        let bytes = serde_json::to_vec(&packet).map_err(invalid)?;
        if bytes.len() > MAX_MESSAGE {
            return Err(invalid("control message exceeds wire bound"));
        }
        channel.send_application(ApplicationProtocol::Control, bytes)
    })
}

/// Read one control packet. The caller supplies the peer from the hub, never from
/// untrusted JSON; reconnection invalidates every old connection-bound packet.
///
/// # Errors
/// Rejects malformed packets, unsupported schemas and stale channel epochs.
pub fn receive(hub: &PeerConnectionHub, peer: DeviceId) -> io::Result<Option<ControlMessage>> {
    hub.with_peer(peer, |channel| {
        let Some(bytes) = channel.try_receive_application(ApplicationProtocol::Control)? else {
            return Ok(None);
        };
        if bytes.len() > MAX_MESSAGE {
            return Err(invalid("control message exceeds wire bound"));
        }
        let packet: Packet = serde_json::from_slice(&bytes).map_err(invalid)?;
        if packet.schema != 1 || packet.channel_epoch != channel.snapshot().channel_epoch {
            return Err(invalid(
                "control packet belongs to another protocol or connection epoch",
            ));
        }
        Ok(Some(packet.message))
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlToken {
    pub session_id: [u8; 16],
    pub origin: DeviceId,
    pub current: DeviceId,
    pub route: Vec<DeviceId>,
    pub epoch: u64,
    pub layout_revision: LayoutRevision,
    pub layout_digest: [u8; 32],
    pub held_input_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    pub transaction: [u8; 16],
    pub held_revision: u64,
    pub edge_offset: u16,
    pub boundary: Option<BoundaryMappingRef>,
    pub channel_epoch: u64,
    pub previous: ControlToken,
    pub next: ControlToken,
    #[serde(with = "held_wire")]
    pub held: Vec<InputEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffAck {
    pub transaction: [u8; 16],
    pub held_revision: u64,
    pub edge_offset: u16,
    pub boundary: Option<BoundaryMappingRef>,
    pub channel_epoch: u64,
    pub session_id: [u8; 16],
    pub epoch: u64,
    pub held_input_digest: [u8; 32],
}

impl Handoff {
    #[must_use]
    pub fn acknowledgement(&self) -> HandoffAck {
        HandoffAck {
            transaction: self.transaction,
            held_revision: self.held_revision,
            edge_offset: self.edge_offset,
            boundary: self.boundary.clone(),
            channel_epoch: self.channel_epoch,
            session_id: self.next.session_id,
            epoch: self.next.epoch,
            held_input_digest: self.next.held_input_digest,
        }
    }
}

struct Reservation {
    handoff: Handoff,
    deadline: Instant,
    outgoing: bool,
}

/// Local monotonic deadlines are deliberately absent from wire tokens: clocks on
/// different computers need not agree. Only a fresh origin renewal extends a lease.
pub struct CrossScreenCoordinator {
    local: DeviceId,
    authority: LayoutAuthority,
    edges: BTreeSet<Edge>,
    active: Option<ControlToken>,
    reservation: Option<Reservation>,
    deadline: Option<Instant>,
    last_epochs: BTreeMap<DeviceId, u64>,
    local_epoch_limit: u64,
    last_input_sequence: u64,
    last_renewal: u64,
    injected: InputLedger,
    faulted: bool,
}

enum LayoutAuthority {
    Global(WorkspaceLayout),
    Precise(BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>),
}

impl CrossScreenCoordinator {
    /// Create the sole input owner after validating the workspace geometry.
    ///
    /// # Errors
    /// Rejects an unknown local computer or invalid trust/geometry.
    pub fn new(
        local: DeviceId,
        layout: WorkspaceLayout,
        members: &BTreeSet<DeviceId>,
        edges: BTreeSet<Edge>,
    ) -> io::Result<Self> {
        WorkspaceLayout::decode(&layout.encode().map_err(invalid)?, members, &edges)
            .map_err(invalid)?;
        if !members.contains(&local) {
            return Err(invalid("local computer is not a workspace member"));
        }
        Self::new_with_epoch_range(local, layout, members, edges, 0, u64::MAX)
    }

    pub(crate) fn new_with_epoch_range(
        local: DeviceId,
        layout: WorkspaceLayout,
        members: &BTreeSet<DeviceId>,
        edges: BTreeSet<Edge>,
        epoch_floor: u64,
        epoch_limit: u64,
    ) -> io::Result<Self> {
        WorkspaceLayout::decode(&layout.encode().map_err(invalid)?, members, &edges)
            .map_err(invalid)?;
        if !members.contains(&local) || epoch_floor >= epoch_limit {
            return Err(invalid("invalid local computer or control epoch range"));
        }
        Ok(Self {
            local,
            authority: LayoutAuthority::Global(layout),
            edges,
            active: None,
            reservation: None,
            deadline: None,
            last_epochs: BTreeMap::from([(local, epoch_floor)]),
            local_epoch_limit: epoch_limit,
            last_input_sequence: 0,
            last_renewal: 0,
            injected: InputLedger::default(),
            faulted: false,
        })
    }

    /// Create a coordinator from pair-scoped confirmed logical boundaries.
    ///
    /// Each authenticated neighbor supplies the exact boundary ids and mapping revisions shared
    /// by that pair. Route segments outside this computer remain opaque and are validated by the
    /// computers that own those segments.
    ///
    /// # Errors
    /// Rejects local, empty, malformed, or excess neighbor mappings.
    pub fn new_precise(
        local: DeviceId,
        mappings: BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>,
    ) -> io::Result<Self> {
        Self::new_precise_with_epoch_range(local, mappings, 0, u64::MAX)
    }

    pub(crate) fn new_precise_with_epoch_range(
        local: DeviceId,
        mappings: BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>,
        epoch_floor: u64,
        epoch_limit: u64,
    ) -> io::Result<Self> {
        if epoch_floor >= epoch_limit {
            return Err(invalid("invalid precise boundary authority"));
        }
        Self::validate_precise_mappings(local, &mappings)?;
        let edges = mappings.keys().map(|peer| edge(local, *peer)).collect();
        Ok(Self {
            local,
            authority: LayoutAuthority::Precise(mappings),
            edges,
            active: None,
            reservation: None,
            deadline: None,
            last_epochs: BTreeMap::from([(local, epoch_floor)]),
            local_epoch_limit: epoch_limit,
            last_input_sequence: 0,
            last_renewal: 0,
            injected: InputLedger::default(),
            faulted: false,
        })
    }

    #[must_use]
    pub fn token(&self) -> Option<&ControlToken> {
        self.active.as_ref()
    }

    #[must_use]
    pub const fn faulted(&self) -> bool {
        self.faulted
    }

    /// Returns the pressed input state currently installed on this controlled target.
    ///
    /// A remote target uses this only to propose a fast return to the physical origin. The
    /// origin still compares the snapshot with its authoritative physical ledger before it
    /// authorizes the handoff, so in-flight press changes fall back to the normal refresh path.
    #[must_use]
    pub fn injected_press_events(&self) -> Vec<InputEvent> {
        self.injected.press_events()
    }

    #[must_use]
    pub fn awaiting_handoff(&self) -> bool {
        self.reservation.is_some()
    }

    /// Reserve a crossing from this computer, preserving the existing token on
    /// later hops. The host freezes ordinary forwarding until this finishes.
    ///
    /// # Errors
    /// Rejects a busy, expired, faulted, non-adjacent or non-current caller.
    pub fn begin_handoff(
        &mut self,
        next: DeviceId,
        channel_epoch: u64,
        held: Vec<InputEvent>,
        now: Instant,
    ) -> io::Result<Handoff> {
        self.begin_handoff_at(next, channel_epoch, held, u16::MAX / 2, now)
    }

    /// Reserve a crossing with its exact normalized position along the shared edge.
    ///
    /// # Errors
    /// Rejects the same invalid/busy states as `begin_handoff`.
    pub fn begin_handoff_at(
        &mut self,
        next: DeviceId,
        channel_epoch: u64,
        held: Vec<InputEvent>,
        edge_offset: u16,
        now: Instant,
    ) -> io::Result<Handoff> {
        self.begin_handoff_with_mapping(next, channel_epoch, held, edge_offset, None, now)
    }

    /// Reserve a handoff through one exact confirmed logical boundary.
    ///
    /// # Errors
    /// Rejects stale control state, an unavailable neighbor, or an unknown mapping.
    pub fn begin_precise_handoff(
        &mut self,
        next: DeviceId,
        channel_epoch: u64,
        held: Vec<InputEvent>,
        parameter: u16,
        boundary: BoundaryMappingRef,
        now: Instant,
    ) -> io::Result<Handoff> {
        self.begin_handoff_with_mapping(next, channel_epoch, held, parameter, Some(boundary), now)
    }

    fn begin_handoff_with_mapping(
        &mut self,
        next: DeviceId,
        channel_epoch: u64,
        held: Vec<InputEvent>,
        edge_offset: u16,
        boundary: Option<BoundaryMappingRef>,
        now: Instant,
    ) -> io::Result<Handoff> {
        self.check_running(now)?;
        if self.reservation.is_some() || channel_epoch == 0 {
            return Err(invalid("handoff is busy or channel epoch is invalid"));
        }
        self.check_edge(self.local, next)?;
        self.check_boundary(self.local, next, boundary.as_ref())?;
        let digest = held_digest(&held)?;
        let previous = if let Some(token) = &self.active {
            if token.current != self.local {
                return Err(invalid("only the current device can cross a screen edge"));
            }
            token.clone()
        } else {
            ControlToken {
                session_id: random_id()?,
                origin: self.local,
                current: self.local,
                route: vec![self.local],
                epoch: self.last_epochs.get(&self.local).copied().unwrap_or(0) + 1,
                layout_revision: self.layout_revision(),
                layout_digest: self.layout_digest(),
                held_input_digest: digest,
            }
        };
        let mut candidate = previous.clone();
        candidate.epoch = previous
            .epoch
            .checked_add(1)
            .filter(|value| *value != u64::MAX)
            .ok_or_else(|| invalid("control epoch exhausted"))?;
        if candidate.origin == self.local && candidate.epoch >= self.local_epoch_limit {
            return Err(invalid("local control epoch reservation exhausted"));
        }
        candidate.current = next;
        candidate.route = folded_route(&previous.route, next);
        candidate.held_input_digest = digest;
        self.validate_token(&candidate)?;
        let handoff = Handoff {
            transaction: random_id()?,
            held_revision: 1,
            edge_offset,
            boundary,
            channel_epoch,
            previous,
            next: candidate,
            held,
        };
        self.last_epochs
            .insert(handoff.next.origin, handoff.next.epoch);
        self.reservation = Some(Reservation {
            handoff: handoff.clone(),
            deadline: now + HANDOFF_TIMEOUT,
            outgoing: true,
        });
        Ok(handoff)
    }

    /// Refresh presses released or acquired while waiting at the boundary. Earlier
    /// acknowledgements cannot commit this new held state, even after press/release/press.
    ///
    /// # Errors
    /// Rejects expired, incoming or absent handoffs and invalid held input.
    pub fn update_handoff_held(
        &mut self,
        held: Vec<InputEvent>,
        now: Instant,
    ) -> io::Result<Handoff> {
        self.check_running(now)?;
        let digest = held_digest(&held)?;
        let pending = self
            .reservation
            .as_mut()
            .ok_or_else(|| invalid("no pending handoff"))?;
        if !pending.outgoing || now >= pending.deadline {
            return Err(invalid("handoff is not live and outgoing"));
        }
        pending.handoff.held_revision = pending
            .handoff
            .held_revision
            .checked_add(1)
            .ok_or_else(|| invalid("held input revision exhausted"))?;
        pending.handoff.held = held;
        pending.handoff.next.held_input_digest = digest;
        Ok(pending.handoff.clone())
    }

    /// Reserve the new target without injecting held or ordinary input.
    ///
    /// # Errors
    /// Rejects another token, wrong sender/connection, replay or invalid layout.
    pub fn prepare(
        &mut self,
        sender: DeviceId,
        channel_epoch: u64,
        handoff: Handoff,
        now: Instant,
    ) -> io::Result<HandoffAck> {
        self.check_running(now)?;
        self.validate_handoff(sender, channel_epoch, &handoff)?;
        if handoff.next.current != self.local {
            return Err(invalid("handoff targets another computer"));
        }
        if let Some(reservation) = &mut self.reservation {
            if !reservation.outgoing && reservation.handoff == handoff && now < reservation.deadline
            {
                return Ok(handoff.acknowledgement());
            }
            if !reservation.outgoing
                && now < reservation.deadline
                && reservation.handoff.transaction == handoff.transaction
                && reservation.handoff.channel_epoch == handoff.channel_epoch
                && reservation.handoff.edge_offset == handoff.edge_offset
                && reservation.handoff.previous == handoff.previous
                && same_control_position(&reservation.handoff.next, &handoff.next)
                && handoff.held_revision > reservation.handoff.held_revision
            {
                let ack = handoff.acknowledgement();
                reservation.handoff = handoff;
                return Ok(ack);
            }
            return Err(invalid("another handoff already holds the reservation"));
        }
        if let Some(active) = &self.active
            && !same_control_position(active, &handoff.previous)
        {
            return Err(invalid("computer is busy with another committed token"));
        }
        if self
            .last_epochs
            .get(&handoff.next.origin)
            .is_some_and(|epoch| *epoch >= handoff.next.epoch)
        {
            return Err(invalid("stale control epoch"));
        }
        // Burn the epoch at reservation, including rejected/expired transactions.
        self.last_epochs
            .insert(handoff.next.origin, handoff.next.epoch);
        let ack = handoff.acknowledgement();
        self.reservation = Some(Reservation {
            handoff,
            deadline: now + HANDOFF_TIMEOUT,
            outgoing: false,
        });
        Ok(ack)
    }

    /// Commit at the old target only after an exact acknowledgement. The caller
    /// sends the returned commit to the new target and then along the old route.
    ///
    /// # Errors
    /// Wrong or expired acknowledgements preserve state; release errors fault the
    /// coordinator and retain its ledger for a subsequent `ReleaseAll` retry.
    pub fn acknowledge(
        &mut self,
        sender: DeviceId,
        channel_epoch: u64,
        ack: &HandoffAck,
        now: Instant,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<Handoff> {
        self.check_running(now)?;
        let pending = self
            .reservation
            .as_ref()
            .ok_or_else(|| invalid("no pending handoff"))?;
        if !pending.outgoing
            || now >= pending.deadline
            || sender != pending.handoff.next.current
            || channel_epoch != pending.handoff.channel_epoch
            || *ack != pending.handoff.acknowledgement()
        {
            return Err(invalid("acknowledgement does not match the live handoff"));
        }
        let commit = pending.handoff.clone();
        self.release_injected(inject)?;
        self.reservation = None;
        self.install_token(commit.next.clone(), now);
        Ok(commit)
    }

    /// Activate a prepared target. Held input is installed only at commit.
    ///
    /// # Errors
    /// Rejects a missing/stale reservation or partial injection failure.
    pub fn commit(
        &mut self,
        sender: DeviceId,
        channel_epoch: u64,
        commit: &Handoff,
        now: Instant,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.commit_with_entry(sender, channel_epoch, commit, now, inject, &mut |_| Ok(()))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_with_entry(
        &mut self,
        sender: DeviceId,
        channel_epoch: u64,
        commit: &Handoff,
        now: Instant,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
        enter: &mut impl FnMut(&Handoff) -> io::Result<()>,
    ) -> io::Result<()> {
        self.check_running(now)?;
        self.validate_handoff(sender, channel_epoch, commit)?;
        let reservation = self
            .reservation
            .as_ref()
            .ok_or_else(|| invalid("commit without reservation"))?;
        if reservation.outgoing || reservation.handoff != *commit || now >= reservation.deadline {
            return Err(invalid("commit does not match the reserved handoff"));
        }
        self.release_injected(inject)?;
        if let Err(error) = enter(commit) {
            self.faulted = true;
            return Err(error);
        }
        if self.local != commit.next.origin {
            // Track before the native call: a partially successful SendInput still
            // needs every possibly injected press released on failure.
            for event in &commit.held {
                let _ = self.injected.apply(*event);
            }
            if let Err(error) = inject(&commit.held) {
                self.faulted = true;
                return Err(error);
            }
        }
        self.reservation = None;
        self.install_token(commit.next.clone(), now);
        Ok(())
    }

    /// Relay a committed route update backwards toward the physical origin.
    /// The authenticated sender must be the next hop on the previously active route.
    ///
    /// # Errors
    /// Rejects unrelated senders, skipped epochs, changed layouts or token replacement.
    pub fn observe_commit(
        &mut self,
        sender: DeviceId,
        commit: &Handoff,
        now: Instant,
    ) -> io::Result<()> {
        self.check_running(now)?;
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| invalid("route update without active token"))?;
        let index = active
            .route
            .iter()
            .position(|id| *id == self.local)
            .ok_or_else(|| invalid("local device is outside active route"))?;
        if active.route.get(index + 1) != Some(&sender)
            || !same_control_position(active, &commit.previous)
            || self.reservation.is_some()
        {
            return Err(invalid(
                "route update does not follow the committed predecessor path",
            ));
        }
        self.validate_handoff(commit.previous.current, commit.channel_epoch, commit)?;
        self.install_token(commit.next.clone(), now);
        Ok(())
    }

    /// Accept input only for this committed target, in increasing origin sequence.
    /// An origin may send directly over a trusted link; otherwise only the previous
    /// route hop can relay it. The runtime also checks the current channel generation.
    ///
    /// # Errors
    /// Rejects ordinary input during prepare, replay, wrong route or expired lease.
    #[allow(clippy::too_many_arguments)]
    pub fn receive_input(
        &mut self,
        sender: DeviceId,
        session: [u8; 16],
        epoch: u64,
        sequence: u64,
        event: InputEvent,
        now: Instant,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.check_running(now)?;
        let token = self
            .active
            .as_ref()
            .ok_or_else(|| invalid("input without a committed token"))?;
        if self.reservation.is_some()
            || token.current != self.local
            || token.origin == self.local
            || token.session_id != session
            || token.epoch != epoch
            || sequence == 0
            || sequence <= self.last_input_sequence
            || !self.valid_upstream(sender, token)
        {
            return Err(invalid(
                "input does not match active target, epoch or route",
            ));
        }
        if self.injected.apply(event) == LedgerResult::CapacityExceeded {
            self.faulted = true;
            return Err(invalid("held input capacity exceeded"));
        }
        self.last_input_sequence = sequence;
        if let Err(error) = inject(&[event]) {
            self.faulted = true;
            return Err(error);
        }
        Ok(())
    }

    /// Extend a lease only for a fresh renewal flowing from its origin.
    ///
    /// # Errors
    /// Rejects late renewals, replay and unrelated authenticated peers.
    pub fn renew(
        &mut self,
        sender: DeviceId,
        session: [u8; 16],
        epoch: u64,
        sequence: u64,
        now: Instant,
    ) -> io::Result<()> {
        self.check_running(now)?;
        let token = self
            .active
            .as_ref()
            .ok_or_else(|| invalid("renewal without token"))?;
        if token.session_id != session
            || token.epoch != epoch
            || sequence == 0
            || sequence <= self.last_renewal
            || !self.valid_upstream(sender, token)
        {
            return Err(invalid("invalid lease renewal"));
        }
        self.last_renewal = sequence;
        self.deadline = Some(now + LEASE);
        Ok(())
    }

    pub(crate) fn renew_origin(&mut self, now: Instant) -> io::Result<()> {
        self.check_running(now)?;
        if self
            .active
            .as_ref()
            .is_none_or(|token| token.origin != self.local)
        {
            return Err(invalid("only the physical origin can originate a renewal"));
        }
        self.deadline = Some(now + LEASE);
        Ok(())
    }

    pub(crate) fn relay_input(
        &mut self,
        sender: DeviceId,
        session: [u8; 16],
        epoch: u64,
        sequence: u64,
        now: Instant,
    ) -> io::Result<DeviceId> {
        self.check_running(now)?;
        let token = self
            .active
            .as_ref()
            .ok_or_else(|| invalid("relay without token"))?;
        if token.current == self.local
            || token.origin == self.local
            || self.reservation.is_some()
            || token.session_id != session
            || token.epoch != epoch
            || sequence == 0
            || sequence <= self.last_input_sequence
            || !self.valid_upstream(sender, token)
        {
            return Err(invalid("input relay does not match the committed route"));
        }
        let next = self
            .input_destination(now)?
            .ok_or_else(|| invalid("relay has no destination"))?;
        self.last_input_sequence = sequence;
        Ok(next)
    }

    pub(crate) fn pending_handoff(&self) -> Option<&Handoff> {
        self.reservation
            .as_ref()
            .map(|reservation| &reservation.handoff)
    }

    /// Synchronous safety barrier used by layout commits, pause, disconnect and exit.
    ///
    /// # Errors
    /// Retains potentially held input and refuses new control until release succeeds.
    pub fn return_local(
        &mut self,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.release_injected(inject)?;
        self.active = None;
        self.reservation = None;
        self.deadline = None;
        self.faulted = false;
        Ok(())
    }

    /// Expire abandoned handoffs and leases without depending on network progress.
    ///
    /// # Errors
    /// Propagates native release failures so the host can retry its retained ledger.
    pub fn tick(
        &mut self,
        now: Instant,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<bool> {
        if self.faulted
            || self.deadline.is_some_and(|deadline| now >= deadline)
            || self
                .reservation
                .as_ref()
                .is_some_and(|pending| now >= pending.deadline)
        {
            self.return_local(inject)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Unrelated peer failures leave the current token intact.
    ///
    /// # Errors
    /// Propagates synchronous release failure for a member of the active route.
    pub fn peer_lost(
        &mut self,
        peer: DeviceId,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<bool> {
        let affected = self
            .active
            .as_ref()
            .is_some_and(|token| token.route.contains(&peer))
            || self.reservation.as_ref().is_some_and(|pending| {
                pending.handoff.previous.route.contains(&peer)
                    || pending.handoff.next.route.contains(&peer)
            });
        if affected {
            self.return_local(inject)?;
        }
        Ok(affected)
    }

    /// Install a layout only after the old injected state is synchronously released.
    ///
    /// # Errors
    /// Invalid layouts and failed native release leave the previous geometry intact.
    pub fn replace_layout(
        &mut self,
        layout: WorkspaceLayout,
        members: &BTreeSet<DeviceId>,
        edges: BTreeSet<Edge>,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        let validated = Self::new(self.local, layout, members, edges)?;
        self.return_local(inject)?;
        self.authority = validated.authority;
        self.edges = validated.edges;
        Ok(())
    }

    /// Replace the pair-scoped mappings of a running precise coordinator.
    ///
    /// The per-computer control epochs are kept, so a replacement never rewinds them (CF-CS-009).
    /// Control is released only when the active route or a pending handoff touches a peer that the
    /// new mappings no longer contain; other routes continue untouched.
    ///
    /// # Errors
    /// Rejects invalid mappings or a grid-authority coordinator, and fails closed when the native
    /// release does not complete; the previous mappings stay in place in every error case.
    pub fn replace_precise_mappings(
        &mut self,
        mappings: BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        Self::validate_precise_mappings(self.local, &mappings)?;
        if !matches!(self.authority, LayoutAuthority::Precise(_)) {
            return Err(invalid(
                "coordinator does not use precise boundary mappings",
            ));
        }
        if self.route_touches_removed(&mappings) {
            self.return_local(inject)?;
        }
        self.edges = mappings
            .keys()
            .map(|peer| edge(self.local, *peer))
            .collect();
        self.authority = LayoutAuthority::Precise(mappings);
        Ok(())
    }

    /// True when the active route or a pending handoff includes a peer absent from `mappings`.
    pub(crate) fn route_touches_removed(
        &self,
        mappings: &BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>,
    ) -> bool {
        let removed = |peer: &DeviceId| *peer != self.local && !mappings.contains_key(peer);
        self.active
            .as_ref()
            .is_some_and(|token| token.route.iter().any(removed))
            || self.reservation.as_ref().is_some_and(|pending| {
                pending
                    .handoff
                    .previous
                    .route
                    .iter()
                    .chain(pending.handoff.next.route.iter())
                    .any(removed)
            })
    }

    pub(crate) fn validate_precise_mappings(
        local: DeviceId,
        mappings: &BTreeMap<DeviceId, BTreeSet<BoundaryMappingRef>>,
    ) -> io::Result<()> {
        if mappings.len() > 4
            || mappings.contains_key(&local)
            || mappings.values().any(|entries| {
                entries.is_empty()
                    || entries.iter().any(|entry| {
                        entry.boundary_id.is_empty()
                            || entry.boundary_id.len() > 64
                            || entry.mapping_revision == 0
                            || entry.mapping_revision == u64::MAX
                    })
            })
        {
            return Err(invalid("invalid precise boundary authority"));
        }
        Ok(())
    }

    /// Resolve the next input hop without flooding other links or extending a route.
    ///
    /// # Errors
    /// Refuses expired/preparing tokens and computers outside their committed route.
    pub fn input_destination(&self, now: Instant) -> io::Result<Option<DeviceId>> {
        self.check_running(now)?;
        if self.reservation.is_some() {
            return Err(invalid("input forwarding is frozen during handoff"));
        }
        let Some(token) = &self.active else {
            return Ok(None);
        };
        if token.current == self.local {
            return Ok(None);
        }
        if token.origin == self.local && self.edges.contains(&edge(self.local, token.current)) {
            return Ok(Some(token.current));
        }
        let index = token
            .route
            .iter()
            .position(|id| *id == self.local)
            .ok_or_else(|| invalid("local computer is outside the control route"))?;
        token
            .route
            .get(index + 1)
            .copied()
            .map(Some)
            .ok_or_else(|| invalid("control route has no next hop"))
    }

    fn install_token(&mut self, token: ControlToken, now: Instant) {
        self.last_epochs
            .entry(token.origin)
            .and_modify(|epoch| *epoch = (*epoch).max(token.epoch))
            .or_insert(token.epoch);
        self.last_input_sequence = 0;
        self.last_renewal = 0;
        if token.current == token.origin || !token.route.contains(&self.local) {
            self.active = None;
            self.deadline = None;
        } else {
            self.active = Some(token);
            self.deadline = Some(now + LEASE);
        }
    }

    fn valid_upstream(&self, sender: DeviceId, token: &ControlToken) -> bool {
        if !self.edges.contains(&edge(sender, self.local)) {
            return false;
        }
        sender == token.origin
            || token
                .route
                .iter()
                .position(|id| *id == self.local)
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| token.route.get(index))
                == Some(&sender)
    }

    fn release_injected(
        &mut self,
        inject: &mut impl FnMut(&[InputEvent]) -> io::Result<()>,
    ) -> io::Result<()> {
        let releases = self.injected.release_events();
        // Even an empty ledger needs a native barrier: queued moves/wheels and
        // an in-flight press must not survive an acknowledged control handoff.
        if let Err(error) = inject(&releases) {
            self.faulted = true;
            return Err(error);
        }
        self.injected.release_all();
        Ok(())
    }

    fn check_running(&self, now: Instant) -> io::Result<()> {
        if self.faulted || self.deadline.is_some_and(|deadline| now >= deadline) {
            return Err(invalid(
                "control lease expired or native input requires release",
            ));
        }
        Ok(())
    }

    fn check_edge(&self, from: DeviceId, to: DeviceId) -> io::Result<()> {
        let valid = match &self.authority {
            LayoutAuthority::Global(layout) => {
                layout.direction(from, to).is_some() && self.edges.contains(&edge(from, to))
            }
            LayoutAuthority::Precise(_) => {
                if from == self.local || to == self.local {
                    self.edges.contains(&edge(from, to))
                } else {
                    // A relay does not need the geometry owned by another authenticated pair.
                    true
                }
            }
        };
        if !valid {
            return Err(invalid("crossing is not a trusted screen edge"));
        }
        Ok(())
    }

    fn check_boundary(
        &self,
        from: DeviceId,
        to: DeviceId,
        boundary: Option<&BoundaryMappingRef>,
    ) -> io::Result<()> {
        match &self.authority {
            LayoutAuthority::Global(_) => {
                if boundary.is_some() {
                    return Err(invalid(
                        "grid handoff unexpectedly contains a logical boundary",
                    ));
                }
            }
            LayoutAuthority::Precise(mappings) => {
                let peer = if from == self.local {
                    Some(to)
                } else if to == self.local {
                    Some(from)
                } else {
                    None
                };
                if let Some(peer) = peer {
                    let boundary = boundary.ok_or_else(|| {
                        invalid("precise handoff is missing its boundary mapping")
                    })?;
                    if !mappings
                        .get(&peer)
                        .is_some_and(|entries| entries.contains(boundary))
                    {
                        return Err(invalid("precise handoff uses an unknown boundary mapping"));
                    }
                }
            }
        }
        Ok(())
    }

    fn layout_revision(&self) -> LayoutRevision {
        match &self.authority {
            LayoutAuthority::Global(layout) => layout.revision(),
            LayoutAuthority::Precise(_) => LayoutRevision {
                counter: 0,
                writer: [0; 32],
            },
        }
    }

    fn layout_digest(&self) -> [u8; 32] {
        match &self.authority {
            LayoutAuthority::Global(layout) => layout.digest(),
            LayoutAuthority::Precise(_) => {
                Sha256::digest(b"ClipFerryPreciseBoundaryControlV1\0").into()
            }
        }
    }

    fn validate_token(&self, token: &ControlToken) -> io::Result<()> {
        if token.session_id == [0; 16]
            || token.epoch == 0
            || token.epoch == u64::MAX
            || token.route.is_empty()
            || token.route.len() > 5
            || token.route.first() != Some(&token.origin)
            || token.route.last() != Some(&token.current)
            || token.layout_revision != self.layout_revision()
            || token.layout_digest != self.layout_digest()
            || token.route.iter().copied().collect::<BTreeSet<_>>().len() != token.route.len()
        {
            return Err(invalid("invalid control token identity, route or layout"));
        }
        if let LayoutAuthority::Global(layout) = &self.authority
            && token
                .route
                .iter()
                .any(|id| !layout.positions().contains_key(id))
        {
            return Err(invalid("control token route contains an unknown device"));
        }
        for pair in token.route.windows(2) {
            self.check_edge(pair[0], pair[1])?;
        }
        Ok(())
    }

    fn validate_handoff(
        &self,
        sender: DeviceId,
        channel_epoch: u64,
        handoff: &Handoff,
    ) -> io::Result<()> {
        self.validate_token(&handoff.previous)?;
        self.validate_token(&handoff.next)?;
        self.check_edge(handoff.previous.current, handoff.next.current)?;
        self.check_boundary(
            handoff.previous.current,
            handoff.next.current,
            handoff.boundary.as_ref(),
        )?;
        if sender != handoff.previous.current
            || channel_epoch == 0
            || channel_epoch != handoff.channel_epoch
            || handoff.transaction == [0; 16]
            || handoff.held_revision == 0
            || handoff.previous.session_id != handoff.next.session_id
            || handoff.previous.origin != handoff.next.origin
            || handoff.previous.epoch.checked_add(1) != Some(handoff.next.epoch)
            || handoff.next.route != folded_route(&handoff.previous.route, handoff.next.current)
            || held_digest(&handoff.held)? != handoff.next.held_input_digest
        {
            return Err(invalid(
                "handoff identity, connection, route or held inputs do not match",
            ));
        }
        Ok(())
    }
}

fn same_control_position(left: &ControlToken, right: &ControlToken) -> bool {
    left.session_id == right.session_id
        && left.origin == right.origin
        && left.current == right.current
        && left.route == right.route
        && left.epoch == right.epoch
        && left.layout_revision == right.layout_revision
        && left.layout_digest == right.layout_digest
}

fn folded_route(route: &[DeviceId], next: DeviceId) -> Vec<DeviceId> {
    route.iter().position(|id| *id == next).map_or_else(
        || route.iter().copied().chain(std::iter::once(next)).collect(),
        |index| route[..=index].to_vec(),
    )
}

fn held_digest(held: &[InputEvent]) -> io::Result<[u8; 32]> {
    if held.len() > MAX_HELD {
        return Err(invalid("held input list exceeds limit"));
    }
    let mut ledger = InputLedger::default();
    for event in held {
        if !matches!(
            event,
            InputEvent::Key {
                transition: KeyTransition::Down,
                ..
            } | InputEvent::MouseButton {
                transition: KeyTransition::Down,
                ..
            }
        ) || ledger.apply(*event) != LedgerResult::Pressed
        {
            return Err(invalid("held inputs must be unique presses"));
        }
    }
    let mut hash = Sha256::new();
    hash.update(b"ClipFerryHeldInputV1\0");
    hash.update(serde_json::to_vec(held).map_err(invalid)?);
    Ok(hash.finalize().into())
}

// A full 256-key ledger must fit the same bounded frame as ordinary handoffs.
// Explicit four-integer tuples avoid repeating verbose event/field names per key.
mod held_wire {
    use super::{Deserialize, InputEvent, KeyTransition, MAX_HELD, Serialize};
    use crate::cross_screen::{MouseButton, PhysicalKey};

    pub fn serialize<S: serde::Serializer>(
        events: &[InputEvent],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let compact = events
            .iter()
            .map(|event| match event {
                InputEvent::Key {
                    physical,
                    virtual_key,
                    transition: KeyTransition::Down,
                } => Ok([
                    0,
                    physical.scan_code,
                    u16::from(physical.extended),
                    *virtual_key,
                ]),
                InputEvent::MouseButton {
                    button,
                    transition: KeyTransition::Down,
                } => Ok([
                    1,
                    match button {
                        MouseButton::Left => 0,
                        MouseButton::Right => 1,
                        MouseButton::Middle => 2,
                        MouseButton::X1 => 3,
                        MouseButton::X2 => 4,
                    },
                    0,
                    0,
                ]),
                _ => Err(serde::ser::Error::custom(
                    "held state contains a non-press event",
                )),
            })
            .collect::<Result<Vec<[u16; 4]>, S::Error>>()?;
        compact.serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<InputEvent>, D::Error> {
        let compact = Vec::<[u16; 4]>::deserialize(deserializer)?;
        if compact.len() > MAX_HELD {
            return Err(serde::de::Error::custom("held state exceeds bound"));
        }
        compact
            .into_iter()
            .map(|item| match item {
                [0, scan_code, extended @ 0..=1, virtual_key] => Ok(InputEvent::Key {
                    physical: PhysicalKey {
                        scan_code,
                        extended: extended == 1,
                    },
                    virtual_key,
                    transition: KeyTransition::Down,
                }),
                [1, button @ 0..=4, 0, 0] => Ok(InputEvent::MouseButton {
                    button: match button {
                        0 => MouseButton::Left,
                        1 => MouseButton::Right,
                        2 => MouseButton::Middle,
                        3 => MouseButton::X1,
                        _ => MouseButton::X2,
                    },
                    transition: KeyTransition::Down,
                }),
                _ => Err(serde::de::Error::custom("invalid held-state tuple")),
            })
            .collect()
    }
}

fn random_id() -> io::Result<[u8; 16]> {
    let mut id = [0; 16];
    getrandom::fill(&mut id).map_err(invalid)?;
    if id == [0; 16] {
        return Err(invalid("random identifier was zero"));
    }
    Ok(id)
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::{MouseButton, PhysicalKey};
    use crate::workspace::GridPoint;

    #[test]
    fn precise_mappings_are_replaced_atomically_and_only_when_valid() {
        let local = [1; 32];
        let first = [2; 32];
        let second = [3; 32];
        let mapping = |id: &str| BoundaryMappingRef {
            boundary_id: id.to_owned(),
            mapping_revision: 4,
        };
        let mut coordinator = CrossScreenCoordinator::new_precise(
            local,
            BTreeMap::from([(first, BTreeSet::from([mapping("pair-b")]))]),
        )
        .unwrap();
        let mut inject = |_: &[InputEvent]| Ok(());
        assert!(coordinator.edges.contains(&edge(local, first)));

        coordinator
            .replace_precise_mappings(
                BTreeMap::from([(second, BTreeSet::from([mapping("pair-c")]))]),
                &mut inject,
            )
            .unwrap();
        assert!(!coordinator.edges.contains(&edge(local, first)));
        assert!(coordinator.edges.contains(&edge(local, second)));
        // With no active route or handoff, removing a pair touches no control state.
        assert!(!coordinator.route_touches_removed(&BTreeMap::new()));

        // A rejected replacement keeps the previous pair mapping in place.
        assert!(
            coordinator
                .replace_precise_mappings(
                    BTreeMap::from([(local, BTreeSet::from([mapping("self")]))]),
                    &mut inject,
                )
                .is_err()
        );
        assert!(coordinator.edges.contains(&edge(local, second)));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Exercise the actual ordered wire exchange and stale connection rejection.
    fn handoff_and_epoch_bound_input_use_the_existing_mutual_tls_hub() {
        use crate::cross_screen::hub::tests::Network;
        fn wait(network: &Network, receiver: usize, sender: usize) -> ControlMessage {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(message) =
                    receive(&network.hubs[receiver], network.ids[sender]).unwrap()
                {
                    return message;
                }
                assert!(Instant::now() < deadline, "control packet timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let network = Network::new(2);
        network.wait_connected();
        let members = network.ids.iter().copied().collect();
        let edges = BTreeSet::from([edge(network.ids[0], network.ids[1])]);
        let layout = WorkspaceLayout::new(
            [8; 16],
            LayoutRevision {
                counter: 1,
                writer: network.ids[0],
            },
            &BTreeMap::from([
                (network.ids[0], GridPoint { x: 0, y: 0 }),
                (network.ids[1], GridPoint { x: 1, y: 0 }),
            ]),
            &members,
            &edges,
        )
        .unwrap();
        let mut origin =
            CrossScreenCoordinator::new(network.ids[0], layout.clone(), &members, edges.clone())
                .unwrap();
        let mut target =
            CrossScreenCoordinator::new(network.ids[1], layout, &members, edges).unwrap();
        let channel_epoch = network.hubs[0]
            .with_peer(network.ids[1], |channel| {
                Ok(channel.snapshot().channel_epoch)
            })
            .unwrap();
        let handoff = origin
            .begin_handoff(network.ids[1], channel_epoch, vec![], Instant::now())
            .unwrap();
        send(
            &network.hubs[0],
            network.ids[1],
            ControlMessage::Prepare {
                handoff: Box::new(handoff),
            },
        )
        .unwrap();
        let ControlMessage::Prepare { handoff } = wait(&network, 1, 0) else {
            panic!("prepare expected")
        };
        let ack = target
            .prepare(network.ids[0], channel_epoch, *handoff, Instant::now())
            .unwrap();
        send(
            &network.hubs[1],
            network.ids[0],
            ControlMessage::Acknowledged { ack },
        )
        .unwrap();
        let ControlMessage::Acknowledged { ack } = wait(&network, 0, 1) else {
            panic!("ack expected")
        };
        let commit = origin
            .acknowledge(
                network.ids[1],
                channel_epoch,
                &ack,
                Instant::now(),
                &mut |_| Ok(()),
            )
            .unwrap();
        send(
            &network.hubs[0],
            network.ids[1],
            ControlMessage::Commit {
                handoff: Box::new(commit.clone()),
            },
        )
        .unwrap();
        let ControlMessage::Commit { handoff } = wait(&network, 1, 0) else {
            panic!("commit expected")
        };
        target
            .commit(
                network.ids[0],
                channel_epoch,
                &handoff,
                Instant::now(),
                &mut |_| Ok(()),
            )
            .unwrap();
        let token = commit.next;
        send(
            &network.hubs[0],
            network.ids[1],
            ControlMessage::Input {
                session: token.session_id,
                epoch: token.epoch,
                sequence: 1,
                event: button(KeyTransition::Down),
            },
        )
        .unwrap();
        let ControlMessage::Input {
            session,
            epoch,
            sequence,
            event,
        } = wait(&network, 1, 0)
        else {
            panic!("input expected")
        };
        let mut injected = vec![];
        target
            .receive_input(
                network.ids[0],
                session,
                epoch,
                sequence,
                event,
                Instant::now(),
                &mut |events| {
                    injected.extend_from_slice(events);
                    Ok(())
                },
            )
            .unwrap();
        target
            .return_local(&mut |events| {
                injected.extend_from_slice(events);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            injected,
            vec![button(KeyTransition::Down), button(KeyTransition::Up)]
        );
        // Even a valid JSON packet cannot be replayed onto another channel epoch.
        let stale = serde_json::to_vec(&Packet {
            schema: 1,
            channel_epoch: channel_epoch.wrapping_add(1),
            message: ControlMessage::Release { session, epoch },
        })
        .unwrap();
        network.hubs[0]
            .with_peer(network.ids[1], |channel| {
                channel.send_application(ApplicationProtocol::Control, stale)
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match receive(&network.hubs[1], network.ids[0]) {
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                    break;
                }
                Ok(None) => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(Some(_)) => panic!("stale packet accepted"),
            }
        }
    }

    fn id(index: usize) -> DeviceId {
        [u8::try_from(index + 1).unwrap(); 32]
    }

    #[test]
    fn maximum_held_ledger_round_trips_within_the_control_frame_bound() {
        let mut nodes = computers();
        let held: Vec<_> = (1..=256)
            .map(|scan_code| InputEvent::Key {
                physical: PhysicalKey {
                    scan_code,
                    extended: true,
                },
                virtual_key: 255,
                transition: KeyTransition::Down,
            })
            .chain(
                [
                    MouseButton::Left,
                    MouseButton::Right,
                    MouseButton::Middle,
                    MouseButton::X1,
                    MouseButton::X2,
                ]
                .map(|button| InputEvent::MouseButton {
                    button,
                    transition: KeyTransition::Down,
                }),
            )
            .collect();
        let handoff = nodes[0]
            .begin_handoff(id(1), 17, held, Instant::now())
            .unwrap();
        let packet = Packet {
            schema: 1,
            channel_epoch: 17,
            message: ControlMessage::Prepare {
                handoff: Box::new(handoff),
            },
        };
        let bytes = serde_json::to_vec(&packet).unwrap();
        assert!(
            bytes.len() <= MAX_MESSAGE,
            "maximum ledger exceeds channel bound: {}",
            bytes.len()
        );
        let decoded: Packet = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.message, packet.message);
    }

    #[test]
    fn input_prefers_direct_origin_link_and_otherwise_uses_only_the_active_route() {
        let mut nodes = computers();
        let now = Instant::now();
        transfer(&mut nodes, 0, 1, vec![], now);
        transfer(&mut nodes, 1, 2, vec![], now);
        assert_eq!(nodes[0].input_destination(now).unwrap(), Some(id(2)));
        nodes[0].edges.remove(&edge(id(0), id(2)));
        assert_eq!(nodes[0].input_destination(now).unwrap(), Some(id(1)));
        assert_eq!(nodes[1].input_destination(now).unwrap(), Some(id(2)));
        assert_eq!(nodes[2].input_destination(now).unwrap(), None);
        assert_eq!(nodes[3].input_destination(now).unwrap(), None);
    }

    #[test]
    fn layout_change_waits_for_release_and_never_reuses_the_old_input_epoch() {
        let mut nodes = computers();
        let now = Instant::now();
        let commit = transfer(&mut nodes, 0, 1, vec![button(KeyTransition::Down)], now);
        let members = (0..4).map(id).collect();
        let old_layout = match &nodes[1].authority {
            LayoutAuthority::Global(layout) => layout.clone(),
            LayoutAuthority::Precise(_) => panic!("fixture uses the global layout"),
        };
        let edit = old_layout
            .place(
                id(1),
                GridPoint { x: 0, y: 1 },
                id(0),
                &members,
                &nodes[1].edges,
            )
            .unwrap();
        let changed = edit.candidate;
        let edges = nodes[1].edges.clone();
        assert!(
            nodes[1]
                .replace_layout(changed.clone(), &members, edges.clone(), &mut |_| Err(
                    io::Error::other("release failed")
                ))
                .is_err()
        );
        assert!(
            matches!(&nodes[1].authority, LayoutAuthority::Global(layout) if *layout == old_layout)
        );
        nodes[1]
            .replace_layout(changed.clone(), &members, edges, &mut |_| Ok(()))
            .unwrap();
        assert!(
            matches!(&nodes[1].authority, LayoutAuthority::Global(layout) if *layout == changed)
        );
        assert!(nodes[1].token().is_none());
        assert!(
            nodes[1]
                .receive_input(
                    id(0),
                    commit.next.session_id,
                    commit.next.epoch,
                    1,
                    button(KeyTransition::Down),
                    now,
                    &mut |_| panic!("old layout must never inject")
                )
                .is_err()
        );
    }

    fn computers() -> Vec<CrossScreenCoordinator> {
        let members = (0..4).map(id).collect();
        let edges: BTreeSet<_> = (0..4)
            .flat_map(|a| (a + 1..4).map(move |b| edge(id(a), id(b))))
            .collect();
        let positions = BTreeMap::from([
            (id(0), GridPoint { x: 0, y: 0 }),
            (id(1), GridPoint { x: 1, y: 0 }),
            (id(2), GridPoint { x: 1, y: 1 }),
            (id(3), GridPoint { x: 0, y: 1 }),
        ]);
        let layout = WorkspaceLayout::new(
            [7; 16],
            LayoutRevision {
                counter: 1,
                writer: id(0),
            },
            &positions,
            &members,
            &edges,
        )
        .unwrap();
        (0..4)
            .map(|index| {
                CrossScreenCoordinator::new(id(index), layout.clone(), &members, edges.clone())
                    .unwrap()
            })
            .collect()
    }

    fn button(transition: KeyTransition) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            transition,
        }
    }

    fn transfer(
        nodes: &mut [CrossScreenCoordinator],
        from: usize,
        to: usize,
        held: Vec<InputEvent>,
        now: Instant,
    ) -> Handoff {
        let handoff = nodes[from].begin_handoff(id(to), 17, held, now).unwrap();
        let ack = nodes[to].prepare(id(from), 17, handoff, now).unwrap();
        let commit = nodes[from]
            .acknowledge(id(to), 17, &ack, now, &mut |_| Ok(()))
            .unwrap();
        nodes[to]
            .commit(id(from), 17, &commit, now, &mut |_| Ok(()))
            .unwrap();
        for pair in commit.previous.route.windows(2).rev() {
            let index = usize::from(pair[0][0] - 1);
            if index != to {
                nodes[index].observe_commit(pair[1], &commit, now).unwrap();
            }
        }
        commit
    }

    #[test]
    fn rectangle_round_trip_preserves_one_token_and_returns_every_node_local() {
        let mut nodes = computers();
        let now = Instant::now();
        let first = transfer(&mut nodes, 0, 1, vec![], now);
        let second = transfer(&mut nodes, 1, 2, vec![], now);
        let third = transfer(&mut nodes, 2, 3, vec![], now);
        let last = transfer(&mut nodes, 3, 0, vec![], now);
        for commit in [&second, &third, &last] {
            assert_eq!(commit.next.session_id, first.next.session_id);
        }
        assert_eq!(last.next.route, vec![id(0)]);
        assert_eq!(last.next.epoch, first.next.epoch + 3);
        assert!(nodes.iter().all(|node| node.token().is_none()));
    }

    #[test]
    fn crossing_back_to_middle_folds_the_route_and_releases_departed_participant() {
        let mut nodes = computers();
        let now = Instant::now();
        transfer(&mut nodes, 0, 1, vec![], now);
        transfer(&mut nodes, 1, 2, vec![], now);
        let commit = transfer(&mut nodes, 2, 1, vec![], now);
        assert_eq!(commit.next.route, vec![id(0), id(1)]);
        assert!(nodes[2].token().is_none());
        assert_eq!(nodes[0].token(), nodes[1].token());
    }

    #[test]
    fn prepared_target_does_not_inject_and_committed_token_wins_competition() {
        let mut nodes = computers();
        let now = Instant::now();
        let handoff = nodes[0]
            .begin_handoff(id(1), 17, vec![button(KeyTransition::Down)], now)
            .unwrap();
        nodes[1].prepare(id(0), 17, handoff.clone(), now).unwrap();
        assert_eq!(nodes[1].injected.outstanding_count(), 0);
        assert!(
            nodes[1]
                .receive_input(
                    id(0),
                    handoff.next.session_id,
                    handoff.next.epoch,
                    1,
                    button(KeyTransition::Down),
                    now,
                    &mut |_| panic!("prepare must not inject")
                )
                .is_err()
        );
        let competing = nodes[2].begin_handoff(id(1), 19, vec![], now).unwrap();
        assert!(nodes[1].prepare(id(2), 19, competing.clone(), now).is_err());
        let commit = nodes[0]
            .acknowledge(id(1), 17, &handoff.acknowledgement(), now, &mut |_| Ok(()))
            .unwrap();
        nodes[1]
            .commit(id(0), 17, &commit, now, &mut |_| Ok(()))
            .unwrap();
        assert!(nodes[1].prepare(id(2), 19, competing, now).is_err());
        assert_eq!(nodes[1].token(), Some(&commit.next));
    }

    #[test]
    fn release_during_prepare_invalidates_ack_and_never_represses_released_button() {
        let mut nodes = computers();
        let now = Instant::now();
        let handoff = nodes[0]
            .begin_handoff(id(1), 17, vec![button(KeyTransition::Down)], now)
            .unwrap();
        let stale = nodes[1].prepare(id(0), 17, handoff, now).unwrap();
        let updated = nodes[0].update_handoff_held(vec![], now).unwrap();
        let ack = nodes[1].prepare(id(0), 17, updated, now).unwrap();
        assert!(
            nodes[0]
                .acknowledge(id(1), 17, &stale, now, &mut |_| Ok(()))
                .is_err()
        );
        let commit = nodes[0]
            .acknowledge(id(1), 17, &ack, now, &mut |_| Ok(()))
            .unwrap();
        let mut injected = vec![];
        nodes[1]
            .commit(id(0), 17, &commit, now, &mut |events| {
                injected.extend_from_slice(events);
                Ok(())
            })
            .unwrap();
        assert!(injected.is_empty());
    }

    #[test]
    fn old_target_releases_before_new_target_inherits_and_release_failure_is_retryable() {
        let mut nodes = computers();
        let now = Instant::now();
        let held = vec![
            InputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 42,
                    extended: false,
                },
                virtual_key: 0xa0,
                transition: KeyTransition::Down,
            },
            button(KeyTransition::Down),
        ];
        transfer(&mut nodes, 0, 1, held.clone(), now);
        let handoff = nodes[1]
            .begin_handoff(id(2), 17, held.clone(), now)
            .unwrap();
        let ack = nodes[2].prepare(id(1), 17, handoff, now).unwrap();
        assert!(
            nodes[1]
                .acknowledge(id(2), 17, &ack, now, &mut |_| Err(io::Error::other(
                    "injector unavailable"
                )))
                .is_err()
        );
        assert!(nodes[1].faulted());
        assert_eq!(nodes[1].injected.outstanding_count(), 2);
        let mut released = vec![];
        nodes[1]
            .return_local(&mut |events| {
                released.extend_from_slice(events);
                Ok(())
            })
            .unwrap();
        assert_eq!(released.len(), 2);
        assert_eq!(released[0], button(KeyTransition::Up));
        assert_eq!(nodes[1].injected.outstanding_count(), 0);
        assert!(!nodes[1].faulted());
        // Failed release emitted no commit, so the target reservation simply expires.
        assert!(
            nodes[2]
                .tick(now + HANDOFF_TIMEOUT, &mut |_| Ok(()))
                .unwrap()
        );
        assert!(nodes[2].token().is_none());
    }

    #[test]
    fn abandoned_ack_burns_epoch_and_late_commit_cannot_revive_control() {
        let mut nodes = computers();
        let now = Instant::now();
        let first = nodes[0].begin_handoff(id(1), 17, vec![], now).unwrap();
        nodes[1].prepare(id(0), 17, first.clone(), now).unwrap();
        let later = now + HANDOFF_TIMEOUT;
        assert!(nodes[0].tick(later, &mut |_| Ok(())).unwrap());
        assert!(nodes[1].tick(later, &mut |_| Ok(())).unwrap());
        assert!(
            nodes[1]
                .commit(id(0), 17, &first, later, &mut |_| Ok(()))
                .is_err()
        );
        assert!(nodes[1].prepare(id(0), 17, first.clone(), later).is_err());
        let second = nodes[0].begin_handoff(id(1), 18, vec![], later).unwrap();
        assert!(second.next.epoch > first.next.epoch);
        assert!(nodes[1].prepare(id(0), 18, second, later).is_ok());
    }

    #[test]
    fn durable_epoch_range_allows_origin_restart_without_target_restart() {
        const BLOCK: u64 = 1 << 32;
        let mut nodes = computers();
        nodes[0].last_epochs.insert(id(0), BLOCK);
        nodes[0].local_epoch_limit = BLOCK * 2;
        let now = Instant::now();
        let before_restart = nodes[0].begin_handoff(id(1), 17, vec![], now).unwrap();
        assert_eq!(before_restart.next.epoch, BLOCK + 2);
        nodes[1].prepare(id(0), 17, before_restart, now).unwrap();
        nodes[0]
            .tick(now + HANDOFF_TIMEOUT, &mut |_| Ok(()))
            .unwrap();
        nodes[1]
            .tick(now + HANDOFF_TIMEOUT, &mut |_| Ok(()))
            .unwrap();

        let mut stale_restart = computers().remove(0);
        let stale = stale_restart
            .begin_handoff(id(1), 18, vec![], now + HANDOFF_TIMEOUT)
            .unwrap();
        assert!(
            nodes[1]
                .prepare(id(0), 18, stale, now + HANDOFF_TIMEOUT)
                .is_err()
        );

        let mut durable_restart = computers().remove(0);
        durable_restart.last_epochs.insert(id(0), BLOCK * 2);
        durable_restart.local_epoch_limit = BLOCK * 3;
        let fresh = durable_restart
            .begin_handoff(id(1), 19, vec![], now + HANDOFF_TIMEOUT)
            .unwrap();
        assert_eq!(fresh.next.epoch, BLOCK * 2 + 2);
        assert!(
            nodes[1]
                .prepare(id(0), 19, fresh, now + HANDOFF_TIMEOUT)
                .is_ok()
        );
    }

    #[test]
    fn stale_input_renewal_and_wrong_peer_do_not_extend_a_lease() {
        let mut nodes = computers();
        let now = Instant::now();
        let commit = transfer(&mut nodes, 0, 1, vec![], now);
        let token = &commit.next;
        nodes[1]
            .receive_input(
                id(0),
                token.session_id,
                token.epoch,
                1,
                button(KeyTransition::Down),
                now,
                &mut |_| Ok(()),
            )
            .unwrap();
        assert!(
            nodes[1]
                .receive_input(
                    id(0),
                    token.session_id,
                    token.epoch,
                    1,
                    button(KeyTransition::Down),
                    now,
                    &mut |_| Ok(())
                )
                .is_err()
        );
        assert!(
            nodes[1]
                .receive_input(
                    id(2),
                    token.session_id,
                    token.epoch,
                    2,
                    button(KeyTransition::Up),
                    now,
                    &mut |_| Ok(())
                )
                .is_err()
        );
        assert!(
            nodes[1]
                .renew(id(2), token.session_id, token.epoch, 1, now)
                .is_err()
        );
        nodes[1]
            .renew(id(0), token.session_id, token.epoch, 1, now)
            .unwrap();
        assert!(
            nodes[1]
                .renew(
                    id(0),
                    token.session_id,
                    token.epoch,
                    1,
                    now + Duration::from_secs(1)
                )
                .is_err()
        );
        let mut releases = vec![];
        assert!(
            nodes[1]
                .tick(now + LEASE, &mut |events| {
                    releases.extend_from_slice(events);
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!(releases, vec![button(KeyTransition::Up)]);
        assert!(
            nodes[1]
                .renew(id(0), token.session_id, token.epoch, 2, now + LEASE)
                .is_err()
        );
    }

    #[test]
    fn native_partial_press_failure_retains_release_ledger_and_disconnect_is_scoped() {
        let mut nodes = computers();
        let now = Instant::now();
        let handoff = nodes[0]
            .begin_handoff(id(1), 17, vec![button(KeyTransition::Down)], now)
            .unwrap();
        let ack = nodes[1].prepare(id(0), 17, handoff, now).unwrap();
        let commit = nodes[0]
            .acknowledge(id(1), 17, &ack, now, &mut |_| Ok(()))
            .unwrap();
        assert!(
            nodes[1]
                .commit(id(0), 17, &commit, now, &mut |events| {
                    // The empty release barrier succeeds; fail only the actual
                    // held-press batch, whose cleanup ledger must be retained.
                    if events.is_empty() {
                        Ok(())
                    } else {
                        Err(io::Error::other("partial native injection"))
                    }
                })
                .is_err()
        );
        assert!(nodes[1].faulted());
        assert!(!nodes[0].peer_lost(id(3), &mut |_| Ok(())).unwrap());
        let mut releases = vec![];
        assert!(
            nodes[1]
                .peer_lost(id(0), &mut |events| {
                    releases.extend_from_slice(events);
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!(releases, vec![button(KeyTransition::Up)]);
        assert!(nodes[1].token().is_none());
    }

    #[test]
    fn an_empty_held_ledger_still_requires_a_native_release_barrier_before_acknowledgement() {
        let mut nodes = computers();
        let now = Instant::now();
        let handoff = nodes[0].begin_handoff(id(1), 17, vec![], now).unwrap();
        let ack = nodes[1].prepare(id(0), 17, handoff, now).unwrap();
        let mut calls = 0;
        let result = nodes[0].acknowledge(id(1), 17, &ack, now, &mut |events| {
            assert!(events.is_empty());
            calls += 1;
            Err(io::Error::other("old native queue is still blocked"))
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
        assert!(nodes[0].faulted());
    }

    #[test]
    fn a_failed_empty_release_barrier_prevents_target_entry_and_commit() {
        let mut nodes = computers();
        let now = Instant::now();
        let handoff = nodes[0].begin_handoff(id(1), 17, vec![], now).unwrap();
        let ack = nodes[1].prepare(id(0), 17, handoff, now).unwrap();
        let commit = nodes[0]
            .acknowledge(id(1), 17, &ack, now, &mut |_| Ok(()))
            .unwrap();
        let mut entered = false;
        let result = nodes[1].commit_with_entry(
            id(0),
            17,
            &commit,
            now,
            &mut |events| {
                assert!(events.is_empty());
                Err(io::Error::other("native release incomplete"))
            },
            &mut |_| {
                entered = true;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!entered);
        assert!(nodes[1].token().is_none());
        assert!(nodes[1].faulted());
    }

    #[test]
    fn malformed_routes_layouts_held_lists_and_connection_epochs_are_rejected() {
        let mut nodes = computers();
        let now = Instant::now();
        let handoff = nodes[0].begin_handoff(id(1), 17, vec![], now).unwrap();
        assert!(nodes[1].prepare(id(0), 18, handoff.clone(), now).is_err());
        assert!(nodes[1].prepare(id(2), 17, handoff.clone(), now).is_err());
        let mut bad = handoff.clone();
        bad.next.route = vec![id(0), id(1), id(0), id(1)];
        assert!(nodes[1].prepare(id(0), 17, bad, now).is_err());
        let mut bad = handoff.clone();
        bad.next.layout_digest[0] ^= 1;
        assert!(nodes[1].prepare(id(0), 17, bad, now).is_err());
        let mut bad = handoff;
        bad.held = vec![button(KeyTransition::Down); 2];
        assert!(nodes[1].prepare(id(0), 17, bad, now).is_err());
        assert!(nodes[3].begin_handoff(id(1), 17, vec![], now).is_err());
    }
}
