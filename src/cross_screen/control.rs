use std::fs;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::device_store::DeviceStore;

use super::boundary::{
    BoundaryControlState, BoundaryDecision, BoundaryPolicy, ControlSurface, PeerPlacement,
    PhysicalPoint, PhysicalScreenSize, PointerSample, ScreenBoundary, StrictBoundaryEngine,
};
use super::channel::{
    ChannelControlEvent, ChannelInputEvent, ChannelState, ControlTransitionAck,
    ControlTransitionRequest, CrossScreenChannel, NormalizedMousePosition, RemoteInputEvent,
    accept_from_listener,
};
use super::state::{KeyTransition, MouseButton, PhysicalKey, WheelAxis};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlProtocolState {
    Local,
    AwaitingEnterAcknowledgement,
    ControllingPeer,
    AwaitingReturnAcknowledgement,
    ControlledByPeer,
}

impl ControlProtocolState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::AwaitingEnterAcknowledgement => "awaiting_enter_ack",
            Self::ControllingPeer => "controlling_peer",
            Self::AwaitingReturnAcknowledgement => "awaiting_return_ack",
            Self::ControlledByPeer => "controlled_by_peer",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlProtocolAction {
    SendScreenInfo(PhysicalScreenSize),
    SendEnterRequest(ControlTransitionRequest),
    SendEnterAcknowledgement(ControlTransitionAck),
    SendReturnRequest(ControlTransitionRequest),
    SendReturnAcknowledgement(ControlTransitionAck),
    ReleaseLocalInput,
    SendReleaseAll,
}

#[derive(Clone, Debug)]
pub struct ControlCoordinator {
    policy: BoundaryPolicy,
    local_screen: PhysicalScreenSize,
    peer_screen: PhysicalScreenSize,
    boundary: StrictBoundaryEngine,
    state: ControlProtocolState,
    active_epoch: Option<u64>,
    last_local_epoch: Option<u64>,
    last_remote_epoch: Option<u64>,
    dynamic_boundaries: bool,
    logical_boundaries: bool,
    inbound_return_boundary: Option<ScreenBoundary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlProtocolDiagnosticReport {
    pub screen_info_exchanges: u32,
    pub enter_round_trips: u32,
    pub return_round_trips: u32,
    pub rejected_wrong_boundaries: u32,
    pub rejected_replays: u32,
    pub rejected_stale_acknowledgements: u32,
    pub input_events_round_tripped: u32,
    pub rejected_input_epochs: u32,
    pub local_release_requests: u32,
    pub control_epoch: u64,
    pub first_channel_epoch: u64,
    pub second_channel_epoch: u64,
    pub clean: bool,
}

impl ControlCoordinator {
    /// Creates one strict, acknowledgement-gated two-machine control coordinator.
    ///
    /// # Errors
    ///
    /// Returns an error when the boundary policy is invalid for either physical screen.
    pub fn new(
        policy: BoundaryPolicy,
        local_screen: PhysicalScreenSize,
        peer_screen: PhysicalScreenSize,
    ) -> io::Result<Self> {
        Ok(Self {
            policy,
            local_screen,
            peer_screen,
            boundary: StrictBoundaryEngine::new(policy, local_screen, peer_screen)?,
            state: ControlProtocolState::Local,
            active_epoch: None,
            last_local_epoch: None,
            last_remote_epoch: None,
            dynamic_boundaries: false,
            logical_boundaries: false,
            inbound_return_boundary: None,
        })
    }

    /// Creates a coordinator whose local departure edge is selected by a validated V2 map.
    ///
    /// # Errors
    ///
    /// Returns an error when the fallback policy is invalid for either physical screen.
    pub fn new_dynamic(
        policy: BoundaryPolicy,
        local_screen: PhysicalScreenSize,
        peer_screen: PhysicalScreenSize,
    ) -> io::Result<Self> {
        let mut coordinator = Self::new(policy, local_screen, peer_screen)?;
        coordinator.dynamic_boundaries = true;
        Ok(coordinator)
    }

    /// Creates a coordinator whose entry and return edges come from a compiled logical mapping.
    ///
    /// # Errors
    ///
    /// Returns an error when the fallback policy is invalid for either physical screen.
    pub fn new_logical(
        mut policy: BoundaryPolicy,
        local_screen: PhysicalScreenSize,
        peer_screen: PhysicalScreenSize,
    ) -> io::Result<Self> {
        // The compiler has already removed the 32-pixel corner guards from every portal.
        policy.corner_guard_pixels = 0;
        let mut coordinator = Self::new(policy, local_screen, peer_screen)?;
        coordinator.dynamic_boundaries = true;
        coordinator.logical_boundaries = true;
        Ok(coordinator)
    }

    #[must_use]
    pub const fn state(&self) -> ControlProtocolState {
        self.state
    }

    #[must_use]
    pub const fn active_control_epoch(&self) -> Option<u64> {
        self.active_epoch
    }

    #[must_use]
    pub const fn peer_screen(&self) -> PhysicalScreenSize {
        self.peer_screen
    }

    #[must_use]
    pub const fn local_screen(&self) -> PhysicalScreenSize {
        self.local_screen
    }

    /// Returns true only while this endpoint is controlled by the authenticated peer under the
    /// exact active control epoch.
    #[must_use]
    pub fn authorizes_remote_input(&self, control_epoch: u64) -> bool {
        control_epoch != 0
            && self.state == ControlProtocolState::ControlledByPeer
            && self.active_epoch == Some(control_epoch)
    }

    #[must_use]
    pub const fn initial_action(&self) -> ControlProtocolAction {
        ControlProtocolAction::SendScreenInfo(self.local_screen)
    }

    /// Observes one physical pointer sample and produces at most one reliable transfer request.
    ///
    /// The boundary state remains pending until the exact acknowledgement is handled.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid sample, a contradictory protocol state, or unavailable
    /// cryptographic randomness.
    pub fn observe_pointer(
        &mut self,
        sample: PointerSample,
    ) -> io::Result<Option<ControlProtocolAction>> {
        self.observe_pointer_at_boundary(sample, None)
    }

    /// Observes a pointer using the local edge selected by the V2 boundary map.
    ///
    /// Legacy coordinators ignore `local_departure`. Dynamic coordinators fail closed while local
    /// when it is `None`; return from the peer remains bound to the edge used for entry.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pointer samples, contradictory state, or epoch generation.
    pub fn observe_pointer_at_boundary(
        &mut self,
        sample: PointerSample,
        local_departure: Option<ScreenBoundary>,
    ) -> io::Result<Option<ControlProtocolAction>> {
        self.observe_pointer_at_boundaries(sample, local_departure, None)
    }

    /// Observes a pointer with independently selected compiled departure edges.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pointer geometry, contradictory state, or epoch generation.
    pub fn observe_pointer_at_boundaries(
        &mut self,
        sample: PointerSample,
        local_departure: Option<ScreenBoundary>,
        peer_departure: Option<ScreenBoundary>,
    ) -> io::Result<Option<ControlProtocolAction>> {
        if self.state == ControlProtocolState::ControlledByPeer {
            return Err(invalid_data(
                "local pointer cannot request a boundary while controlled by the peer",
            ));
        }
        let decision = if self.logical_boundaries {
            self.boundary
                .observe_with_boundaries(sample, local_departure, peer_departure)?
        } else if self.dynamic_boundaries {
            self.boundary
                .observe_with_local_boundary(sample, local_departure)?
        } else {
            self.boundary.observe(sample)?
        };
        let BoundaryDecision::TransitionRequested(transition) = decision else {
            return Ok(None);
        };
        match (self.state, transition.from, transition.to) {
            (ControlProtocolState::Local, ControlSurface::Local, ControlSurface::Peer) => {
                let control_epoch = self.generate_control_epoch()?;
                self.active_epoch = Some(control_epoch);
                self.state = ControlProtocolState::AwaitingEnterAcknowledgement;
                Ok(Some(ControlProtocolAction::SendEnterRequest(
                    ControlTransitionRequest {
                        control_epoch,
                        departure_boundary: transition.departure_boundary,
                        normalized_edge_offset: transition.normalized_edge_offset,
                        logical_boundary: None,
                    },
                )))
            }
            (
                ControlProtocolState::ControllingPeer,
                ControlSurface::Peer,
                ControlSurface::Local,
            ) => {
                let control_epoch = self.active_epoch.ok_or_else(|| {
                    invalid_data("peer control state has no active control epoch")
                })?;
                self.state = ControlProtocolState::AwaitingReturnAcknowledgement;
                Ok(Some(ControlProtocolAction::SendReturnRequest(
                    ControlTransitionRequest {
                        control_epoch,
                        departure_boundary: transition.departure_boundary,
                        normalized_edge_offset: transition.normalized_edge_offset,
                        logical_boundary: None,
                    },
                )))
            }
            _ => {
                self.force_local();
                Err(invalid_data(
                    "boundary transition contradicts the control protocol state",
                ))
            }
        }
    }

    /// Applies one authenticated, ordered channel event and returns bounded follow-up actions.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale or mismatched acknowledgement or an invalid screen change.
    pub fn handle_channel_event(
        &mut self,
        event: ChannelControlEvent,
    ) -> io::Result<Vec<ControlProtocolAction>> {
        match event {
            ChannelControlEvent::FileDrag(_) | ChannelControlEvent::BoundaryWorkspace(_) => {
                Ok(Vec::new())
            }
            ChannelControlEvent::ScreenInfo(screen) => self.apply_peer_screen(screen),
            ChannelControlEvent::ScreenMillimeters(_) | ChannelControlEvent::LayoutInfo(_) => {
                Err(invalid_data(
                    "screen metadata and layout information must be resolved before control coordination",
                ))
            }
            ChannelControlEvent::EnterRequest(request) => Ok(self.handle_enter_request(&request)),
            ChannelControlEvent::EnterAcknowledgement(acknowledgement) => {
                self.handle_enter_acknowledgement(acknowledgement)
            }
            ChannelControlEvent::ReturnRequest(request) => Ok(self.handle_return_request(&request)),
            ChannelControlEvent::ReturnAcknowledgement(acknowledgement) => {
                self.handle_return_acknowledgement(acknowledgement)
            }
            ChannelControlEvent::ReleaseAll => {
                self.force_local();
                Ok(vec![ControlProtocolAction::ReleaseLocalInput])
            }
        }
    }

    /// Atomically applies a local physical-screen change and returns control local if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error without changing the coordinator if the new screen is invalid for the
    /// configured physical corner guard.
    pub fn update_local_screen(
        &mut self,
        screen: PhysicalScreenSize,
    ) -> io::Result<Vec<ControlProtocolAction>> {
        PhysicalScreenSize::new(screen.width, screen.height)?;
        if screen == self.local_screen {
            return Ok(Vec::new());
        }
        let release_required = self.state != ControlProtocolState::Local;
        self.boundary
            .reconfigure(self.policy, screen, self.peer_screen)?;
        self.local_screen = screen;
        self.state = ControlProtocolState::Local;
        self.active_epoch = None;
        Ok(if release_required {
            vec![
                ControlProtocolAction::ReleaseLocalInput,
                ControlProtocolAction::SendReleaseAll,
            ]
        } else {
            Vec::new()
        })
    }

    /// Returns to local control after a disconnect, stop, trust change, or runtime pause.
    #[must_use]
    pub fn disconnect(&mut self) -> [ControlProtocolAction; 2] {
        self.force_local();
        [
            ControlProtocolAction::ReleaseLocalInput,
            ControlProtocolAction::SendReleaseAll,
        ]
    }

    fn apply_peer_screen(
        &mut self,
        screen: PhysicalScreenSize,
    ) -> io::Result<Vec<ControlProtocolAction>> {
        PhysicalScreenSize::new(screen.width, screen.height)?;
        if screen == self.peer_screen {
            return Ok(Vec::new());
        }
        let release_required = self.state != ControlProtocolState::Local;
        self.boundary
            .reconfigure(self.policy, self.local_screen, screen)?;
        self.peer_screen = screen;
        self.state = ControlProtocolState::Local;
        self.active_epoch = None;
        Ok(if release_required {
            vec![
                ControlProtocolAction::ReleaseLocalInput,
                ControlProtocolAction::SendReleaseAll,
            ]
        } else {
            Vec::new()
        })
    }

    fn handle_enter_request(
        &mut self,
        request: &ControlTransitionRequest,
    ) -> Vec<ControlProtocolAction> {
        let expected = self.policy.peer_placement.peer_return_boundary();
        let accepted = request.control_epoch != 0
            && if self.logical_boundaries {
                request.logical_boundary.is_some()
            } else {
                request.logical_boundary.is_none()
                    && (self.dynamic_boundaries || request.departure_boundary == expected)
            }
            && self.state == ControlProtocolState::Local
            && self.boundary.state() == BoundaryControlState::Local
            && self.remote_epoch_is_expected(request.control_epoch);
        if accepted {
            self.last_remote_epoch = Some(request.control_epoch);
            self.active_epoch = Some(request.control_epoch);
            self.state = ControlProtocolState::ControlledByPeer;
            self.inbound_return_boundary = Some(request.departure_boundary.opposite());
            vec![
                ControlProtocolAction::ReleaseLocalInput,
                ControlProtocolAction::SendEnterAcknowledgement(ControlTransitionAck {
                    control_epoch: request.control_epoch,
                    accepted: true,
                }),
            ]
        } else {
            vec![ControlProtocolAction::SendEnterAcknowledgement(
                ControlTransitionAck {
                    control_epoch: request.control_epoch,
                    accepted: false,
                },
            )]
        }
    }

    fn handle_enter_acknowledgement(
        &mut self,
        acknowledgement: ControlTransitionAck,
    ) -> io::Result<Vec<ControlProtocolAction>> {
        if self.state != ControlProtocolState::AwaitingEnterAcknowledgement
            || self.active_epoch != Some(acknowledgement.control_epoch)
        {
            return Err(invalid_data(
                "stale or mismatched cross-screen enter acknowledgement",
            ));
        }
        if acknowledgement.accepted {
            self.boundary.confirm_pending()?;
            self.state = ControlProtocolState::ControllingPeer;
        } else {
            self.boundary.reject_pending();
            self.state = ControlProtocolState::Local;
            self.active_epoch = None;
        }
        Ok(Vec::new())
    }

    fn handle_return_request(
        &mut self,
        request: &ControlTransitionRequest,
    ) -> Vec<ControlProtocolAction> {
        let expected = if self.dynamic_boundaries {
            self.inbound_return_boundary
        } else {
            Some(self.policy.peer_placement.local_exit_boundary())
        };
        let accepted = request.control_epoch != 0
            && if self.logical_boundaries {
                request.logical_boundary.is_some()
            } else {
                request.logical_boundary.is_none() && Some(request.departure_boundary) == expected
            }
            && self.state == ControlProtocolState::ControlledByPeer
            && self.active_epoch == Some(request.control_epoch);
        if accepted {
            self.force_local();
            vec![
                ControlProtocolAction::ReleaseLocalInput,
                ControlProtocolAction::SendReturnAcknowledgement(ControlTransitionAck {
                    control_epoch: request.control_epoch,
                    accepted: true,
                }),
            ]
        } else {
            vec![ControlProtocolAction::SendReturnAcknowledgement(
                ControlTransitionAck {
                    control_epoch: request.control_epoch,
                    accepted: false,
                },
            )]
        }
    }

    fn handle_return_acknowledgement(
        &mut self,
        acknowledgement: ControlTransitionAck,
    ) -> io::Result<Vec<ControlProtocolAction>> {
        if self.state != ControlProtocolState::AwaitingReturnAcknowledgement
            || self.active_epoch != Some(acknowledgement.control_epoch)
        {
            return Err(invalid_data(
                "stale or mismatched cross-screen return acknowledgement",
            ));
        }
        if acknowledgement.accepted {
            self.boundary.confirm_pending()?;
        } else {
            self.boundary.force_local();
        }
        self.state = ControlProtocolState::Local;
        self.active_epoch = None;
        Ok(vec![
            ControlProtocolAction::ReleaseLocalInput,
            ControlProtocolAction::SendReleaseAll,
        ])
    }

    fn generate_control_epoch(&mut self) -> io::Result<u64> {
        let epoch = if let Some(previous) = self.last_local_epoch {
            following_epoch(previous)
        } else {
            let mut bytes = [0_u8; 8];
            getrandom::fill(&mut bytes).map_err(|error| {
                io::Error::other(format!("control epoch randomness failed: {error}"))
            })?;
            let random = u64::from_le_bytes(bytes);
            if random == 0 { 1 } else { random }
        };
        self.last_local_epoch = Some(epoch);
        Ok(epoch)
    }

    fn remote_epoch_is_expected(&self, epoch: u64) -> bool {
        epoch != 0
            && self
                .last_remote_epoch
                .is_none_or(|previous| epoch == following_epoch(previous))
    }

    fn force_local(&mut self) {
        self.boundary.force_local();
        self.state = ControlProtocolState::Local;
        self.active_epoch = None;
        self.inbound_return_boundary = None;
    }
}

const fn following_epoch(previous: u64) -> u64 {
    if previous == u64::MAX {
        1
    } else {
        previous + 1
    }
}

/// Enqueues the network portion of protocol actions without allowing network workers to decide
/// input suppression. Local input release is performed synchronously, at its exact ordered action
/// position, through the supplied callback.
///
/// # Errors
///
/// Returns an error if the channel is closing or its bounded reliable queue is full.
pub fn dispatch_control_actions(
    channel: &CrossScreenChannel,
    actions: impl IntoIterator<Item = ControlProtocolAction>,
    mut release_local_input: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    for action in actions {
        match action {
            ControlProtocolAction::SendScreenInfo(screen) => channel.send_screen_info(screen)?,
            ControlProtocolAction::SendEnterRequest(request) => {
                channel.send_enter_request(request)?;
            }
            ControlProtocolAction::SendEnterAcknowledgement(acknowledgement) => {
                channel.send_enter_acknowledgement(acknowledgement)?;
            }
            ControlProtocolAction::SendReturnRequest(request) => {
                channel.send_return_request(request)?;
            }
            ControlProtocolAction::SendReturnAcknowledgement(acknowledgement) => {
                channel.send_return_acknowledgement(acknowledgement)?;
            }
            ControlProtocolAction::ReleaseLocalInput => release_local_input()?,
            ControlProtocolAction::SendReleaseAll => channel.request_release_all(),
        }
    }
    Ok(())
}

struct DiagnosticDirectory(PathBuf);

impl DiagnosticDirectory {
    fn new(label: &str) -> io::Result<Self> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| io::Error::other(format!("diagnostic randomness failed: {error}")))?;
        let path = std::env::temp_dir().join(format!(
            "pierdesk-control-protocol-{label}-{}-{}",
            std::process::id(),
            u64::from_le_bytes(random)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for DiagnosticDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs one self-contained mutual-TLS control transfer on loopback without suppressing or
/// injecting any user input.
///
/// # Errors
///
/// Returns an error if screen exchange, acknowledgement gating, wrong-boundary rejection,
/// replay rejection, stale-acknowledgement rejection, disconnect release, or clean shutdown
/// does not behave exactly as required.
pub fn run_control_protocol_diagnostic() -> io::Result<ControlProtocolDiagnosticReport> {
    let mut diagnostic = ProtocolDiagnostic::new()?;
    diagnostic.exchange_screen_info()?;
    diagnostic.reject_wrong_boundary()?;
    let enter_request = diagnostic.complete_enter()?;
    diagnostic.exchange_input_events(enter_request.control_epoch)?;
    diagnostic.complete_return()?;
    diagnostic.reject_replay(&enter_request)?;
    diagnostic.reject_stale_acknowledgement(enter_request.control_epoch)?;
    diagnostic.verify_disconnect_release()?;
    diagnostic.finish(enter_request.control_epoch)
}

struct ProtocolDiagnostic {
    _first_directory: DiagnosticDirectory,
    _second_directory: DiagnosticDirectory,
    first_channel: CrossScreenChannel,
    second_channel: CrossScreenChannel,
    first: ControlCoordinator,
    second: ControlCoordinator,
    first_screen: PhysicalScreenSize,
    second_screen: PhysicalScreenSize,
    local_release_requests: u32,
    input_events_round_tripped: u32,
    rejected_input_epochs: u32,
}

impl ProtocolDiagnostic {
    fn new() -> io::Result<Self> {
        let first_directory = DiagnosticDirectory::new("first")?;
        let second_directory = DiagnosticDirectory::new("second")?;
        let first_store = DeviceStore::new(&first_directory.0);
        let second_store = DeviceStore::new(&second_directory.0);
        let first_identity = first_store.load_or_create_identity()?.identity;
        let second_identity = second_store.load_or_create_identity()?.identity;
        let first_fingerprint = first_identity.fingerprint();
        let second_fingerprint = second_identity.fingerprint();
        first_store.trust_peer(
            second_identity.certificate_der().to_vec(),
            second_fingerprint,
            "control-diagnostic-second",
        )?;
        second_store.trust_peer(
            first_identity.certificate_der().to_vec(),
            first_fingerprint,
            "control-diagnostic-first",
        )?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let second_worker = std::thread::spawn(move || {
            accept_from_listener(
                &second_store,
                first_fingerprint,
                &listener,
                Duration::from_secs(5),
            )
        });
        let first_channel = CrossScreenChannel::connect(
            &first_store,
            second_fingerprint,
            address,
            Duration::from_secs(5),
        )?;
        let second_channel = second_worker
            .join()
            .map_err(|_| io::Error::other("control diagnostic listener panicked"))??;
        let first_screen = PhysicalScreenSize::new(3_840, 2_160)?;
        let second_screen = PhysicalScreenSize::new(2_560, 1_440)?;
        let placeholder = PhysicalScreenSize::new(1_024, 768)?;
        let first = ControlCoordinator::new(
            diagnostic_policy(PeerPlacement::Right),
            first_screen,
            placeholder,
        )?;
        let second = ControlCoordinator::new(
            diagnostic_policy(PeerPlacement::Left),
            second_screen,
            placeholder,
        )?;
        Ok(Self {
            _first_directory: first_directory,
            _second_directory: second_directory,
            first_channel,
            second_channel,
            first,
            second,
            first_screen,
            second_screen,
            local_release_requests: 0,
            input_events_round_tripped: 0,
            rejected_input_epochs: 0,
        })
    }

    fn exchange_screen_info(&mut self) -> io::Result<()> {
        self.dispatch_first([self.first.initial_action()])?;
        self.dispatch_second([self.second.initial_action()])?;
        let first_event = wait_for_control_event(&self.first_channel, Duration::from_secs(3))?;
        let second_event = wait_for_control_event(&self.second_channel, Duration::from_secs(3))?;
        let first_actions = self.first.handle_channel_event(first_event)?;
        self.dispatch_first(first_actions)?;
        let second_actions = self.second.handle_channel_event(second_event)?;
        self.dispatch_second(second_actions)?;
        if self.first.peer_screen() != self.second_screen
            || self.second.peer_screen() != self.first_screen
        {
            return Err(invalid_data(
                "control diagnostic did not exchange physical screen information",
            ));
        }
        Ok(())
    }

    fn reject_wrong_boundary(&mut self) -> io::Result<()> {
        let request = ControlTransitionRequest {
            control_epoch: 91,
            departure_boundary: ScreenBoundary::Left,
            normalized_edge_offset: 30_000,
            logical_boundary: None,
        };
        self.first_channel.send_enter_request(request.clone())?;
        let event = wait_for_control_event(&self.second_channel, Duration::from_secs(3))?;
        let actions = self.second.handle_channel_event(event)?;
        self.dispatch_second(actions)?;
        let acknowledgement = wait_for_control_event(&self.first_channel, Duration::from_secs(3))?;
        let expected = ChannelControlEvent::EnterAcknowledgement(ControlTransitionAck {
            control_epoch: request.control_epoch,
            accepted: false,
        });
        if acknowledgement != expected
            || self.first.state() != ControlProtocolState::Local
            || self.second.state() != ControlProtocolState::Local
        {
            return Err(invalid_data(
                "control diagnostic accepted a request from the wrong boundary",
            ));
        }
        Ok(())
    }

    fn complete_enter(&mut self) -> io::Result<ControlTransitionRequest> {
        let action = self
            .first
            .observe_pointer(diagnostic_pointer_sample(
                ScreenBoundary::Right,
                self.first_screen.height / 2,
                self.first_screen,
            ))?
            .ok_or_else(|| invalid_data("control diagnostic did not request peer entry"))?;
        let ControlProtocolAction::SendEnterRequest(ref request) = action else {
            return Err(invalid_data(
                "control diagnostic produced the wrong enter action",
            ));
        };
        let request = request.clone();
        self.dispatch_first([action])?;
        let event = wait_for_control_event(&self.second_channel, Duration::from_secs(3))?;
        let actions = self.second.handle_channel_event(event)?;
        self.dispatch_second(actions)?;
        let acknowledgement = wait_for_control_event(&self.first_channel, Duration::from_secs(3))?;
        let actions = self.first.handle_channel_event(acknowledgement)?;
        self.dispatch_first(actions)?;
        if self.first.state() != ControlProtocolState::ControllingPeer
            || self.second.state() != ControlProtocolState::ControlledByPeer
            || self.first.active_control_epoch() != Some(request.control_epoch)
            || self.second.active_control_epoch() != Some(request.control_epoch)
        {
            return Err(invalid_data(
                "control diagnostic changed state without an exact enter acknowledgement",
            ));
        }
        Ok(request)
    }

    fn complete_return(&mut self) -> io::Result<()> {
        let action = self
            .first
            .observe_pointer(diagnostic_pointer_sample(
                ScreenBoundary::Left,
                self.second_screen.height / 2,
                self.second_screen,
            ))?
            .ok_or_else(|| invalid_data("control diagnostic did not request local return"))?;
        self.dispatch_first([action])?;
        let event = wait_for_control_event(&self.second_channel, Duration::from_secs(3))?;
        let actions = self.second.handle_channel_event(event)?;
        self.dispatch_second(actions)?;
        let acknowledgement = wait_for_control_event(&self.first_channel, Duration::from_secs(3))?;
        let actions = self.first.handle_channel_event(acknowledgement)?;
        self.dispatch_first(actions)?;
        let release = wait_for_control_event(&self.second_channel, Duration::from_secs(3))?;
        let actions = self.second.handle_channel_event(release)?;
        self.dispatch_second(actions)?;
        if self.first.state() != ControlProtocolState::Local
            || self.second.state() != ControlProtocolState::Local
        {
            return Err(invalid_data(
                "control diagnostic did not restore local state after return acknowledgement",
            ));
        }
        Ok(())
    }

    fn exchange_input_events(&mut self, control_epoch: u64) -> io::Result<()> {
        let stale = ChannelInputEvent {
            control_epoch: following_epoch(control_epoch),
            event: RemoteInputEvent::MouseButton {
                button: MouseButton::Right,
                transition: KeyTransition::Down,
            },
        };
        self.first_channel.send_input_event(stale)?;
        let received = wait_for_input_event(&self.second_channel, Duration::from_secs(3))?;
        if self.second.authorizes_remote_input(received.control_epoch) {
            return Err(invalid_data(
                "control diagnostic authorized input from the wrong control epoch",
            ));
        }
        self.rejected_input_epochs += 1;

        let events = [
            RemoteInputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 0x1d,
                    extended: false,
                },
                virtual_key: 0xa2,
                transition: KeyTransition::Down,
            },
            RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Down,
            },
            RemoteInputEvent::MouseMove(NormalizedMousePosition {
                x: 40_000,
                y: 20_000,
            }),
            RemoteInputEvent::Wheel {
                axis: WheelAxis::Vertical,
                delta: 120,
            },
            RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Up,
            },
            RemoteInputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 0x1d,
                    extended: false,
                },
                virtual_key: 0xa2,
                transition: KeyTransition::Up,
            },
        ];
        for event in events {
            self.first_channel.send_input_event(ChannelInputEvent {
                control_epoch,
                event,
            })?;
        }
        for expected in events {
            let received = wait_for_input_event(&self.second_channel, Duration::from_secs(3))?;
            if received
                != (ChannelInputEvent {
                    control_epoch,
                    event: expected,
                })
                || !self.second.authorizes_remote_input(received.control_epoch)
            {
                return Err(invalid_data(
                    "control diagnostic changed an ordered control-epoch-bound input event",
                ));
            }
            self.input_events_round_tripped += 1;
        }
        Ok(())
    }

    fn reject_replay(&mut self, request: &ControlTransitionRequest) -> io::Result<()> {
        self.first_channel.send_enter_request(request.clone())?;
        let event = wait_for_control_event(&self.second_channel, Duration::from_secs(3))?;
        let actions = self.second.handle_channel_event(event)?;
        self.dispatch_second(actions)?;
        let acknowledgement = wait_for_control_event(&self.first_channel, Duration::from_secs(3))?;
        let expected = ChannelControlEvent::EnterAcknowledgement(ControlTransitionAck {
            control_epoch: request.control_epoch,
            accepted: false,
        });
        if acknowledgement != expected {
            return Err(invalid_data(
                "control diagnostic did not reject a replayed control epoch",
            ));
        }
        Ok(())
    }

    fn reject_stale_acknowledgement(&mut self, control_epoch: u64) -> io::Result<()> {
        let result = self
            .first
            .handle_channel_event(ChannelControlEvent::ReturnAcknowledgement(
                ControlTransitionAck {
                    control_epoch,
                    accepted: true,
                },
            ));
        if result.is_ok() || self.first.state() != ControlProtocolState::Local {
            return Err(invalid_data(
                "control diagnostic accepted a stale acknowledgement",
            ));
        }
        Ok(())
    }

    fn verify_disconnect_release(&mut self) -> io::Result<()> {
        let pending = self
            .first
            .observe_pointer(diagnostic_pointer_sample(
                ScreenBoundary::Right,
                self.first_screen.height / 2,
                self.first_screen,
            ))?
            .ok_or_else(|| invalid_data("disconnect probe did not enter pending state"))?;
        if !matches!(pending, ControlProtocolAction::SendEnterRequest(_)) {
            return Err(invalid_data("disconnect probe produced an invalid action"));
        }
        let disconnect_actions = self.first.disconnect();
        self.dispatch_first(disconnect_actions)?;
        if self.first.state() != ControlProtocolState::Local
            || self.first.active_control_epoch().is_some()
        {
            return Err(invalid_data(
                "disconnect probe did not force control back to local",
            ));
        }
        let release = wait_for_control_event(&self.second_channel, Duration::from_secs(3))?;
        let actions = self.second.handle_channel_event(release)?;
        self.dispatch_second(actions)
    }

    fn finish(mut self, control_epoch: u64) -> io::Result<ControlProtocolDiagnosticReport> {
        let first_channel_epoch = self.first_channel.snapshot().channel_epoch;
        let second_channel_epoch = self.second_channel.snapshot().channel_epoch;
        self.first_channel.stop();
        self.second_channel.stop();
        let first_snapshot = self.first_channel.snapshot();
        let second_snapshot = self.second_channel.snapshot();
        let clean = first_snapshot.state == ChannelState::Stopped
            && second_snapshot.state == ChannelState::Stopped
            && first_snapshot.clean_shutdown
            && second_snapshot.clean_shutdown
            && first_channel_epoch == second_channel_epoch;
        if !clean {
            return Err(io::Error::other(format!(
                "control diagnostic channels did not stop cleanly: first={:?}, second={:?}",
                first_snapshot.last_error, second_snapshot.last_error
            )));
        }
        Ok(ControlProtocolDiagnosticReport {
            screen_info_exchanges: 2,
            enter_round_trips: 1,
            return_round_trips: 1,
            rejected_wrong_boundaries: 1,
            rejected_replays: 1,
            rejected_stale_acknowledgements: 1,
            input_events_round_tripped: self.input_events_round_tripped,
            rejected_input_epochs: self.rejected_input_epochs,
            local_release_requests: self.local_release_requests,
            control_epoch,
            first_channel_epoch,
            second_channel_epoch,
            clean,
        })
    }

    fn dispatch_first(
        &mut self,
        actions: impl IntoIterator<Item = ControlProtocolAction>,
    ) -> io::Result<()> {
        dispatch_diagnostic_actions(
            &self.first_channel,
            actions,
            &mut self.local_release_requests,
        )
    }

    fn dispatch_second(
        &mut self,
        actions: impl IntoIterator<Item = ControlProtocolAction>,
    ) -> io::Result<()> {
        dispatch_diagnostic_actions(
            &self.second_channel,
            actions,
            &mut self.local_release_requests,
        )
    }
}

const fn diagnostic_policy(peer_placement: PeerPlacement) -> BoundaryPolicy {
    BoundaryPolicy {
        peer_placement,
        corner_guard_pixels: 32,
        require_second_push: false,
    }
}

fn diagnostic_pointer_sample(
    boundary: ScreenBoundary,
    offset: u32,
    screen: PhysicalScreenSize,
) -> PointerSample {
    match boundary {
        ScreenBoundary::Left => PointerSample {
            position: PhysicalPoint { x: 0, y: offset },
            delta_x: -1,
            delta_y: 0,
        },
        ScreenBoundary::Right => PointerSample {
            position: PhysicalPoint {
                x: screen.width - 1,
                y: offset,
            },
            delta_x: 1,
            delta_y: 0,
        },
        ScreenBoundary::Top => PointerSample {
            position: PhysicalPoint { x: offset, y: 0 },
            delta_x: 0,
            delta_y: -1,
        },
        ScreenBoundary::Bottom => PointerSample {
            position: PhysicalPoint {
                x: offset,
                y: screen.height - 1,
            },
            delta_x: 0,
            delta_y: 1,
        },
    }
}

fn wait_for_control_event(
    channel: &CrossScreenChannel,
    timeout: Duration,
) -> io::Result<ChannelControlEvent> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid_data("control diagnostic timeout overflow"))?;
    loop {
        if let Some(event) = channel.try_receive_control_event()? {
            return Ok(event);
        }
        let snapshot = channel.snapshot();
        if snapshot.state == ChannelState::Failed {
            return Err(io::Error::other(snapshot.last_error.unwrap_or_else(|| {
                "control diagnostic channel failed without an error".to_owned()
            })));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "control diagnostic timed out waiting for an event",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_input_event(
    channel: &CrossScreenChannel,
    timeout: Duration,
) -> io::Result<ChannelInputEvent> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid_data("input diagnostic timeout overflow"))?;
    loop {
        if let Some(event) = channel.try_receive_input_event()? {
            return Ok(event);
        }
        let snapshot = channel.snapshot();
        if snapshot.state == ChannelState::Failed {
            return Err(io::Error::other(snapshot.last_error.unwrap_or_else(|| {
                "control diagnostic channel failed without an error".to_owned()
            })));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "control diagnostic timed out waiting for an input event",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn dispatch_diagnostic_actions(
    channel: &CrossScreenChannel,
    actions: impl IntoIterator<Item = ControlProtocolAction>,
    local_release_requests: &mut u32,
) -> io::Result<()> {
    dispatch_control_actions(channel, actions, || {
        *local_release_requests = local_release_requests.saturating_add(1);
        Ok(())
    })
}

#[cfg(test)]
const fn opposite_placement(placement: PeerPlacement) -> PeerPlacement {
    match placement {
        PeerPlacement::Left => PeerPlacement::Right,
        PeerPlacement::Right => PeerPlacement::Left,
        PeerPlacement::Above => PeerPlacement::Below,
        PeerPlacement::Below => PeerPlacement::Above,
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::{PhysicalPoint, ScreenBoundary};

    fn screen(width: u32, height: u32) -> PhysicalScreenSize {
        PhysicalScreenSize::new(width, height).unwrap()
    }

    fn policy(peer_placement: PeerPlacement) -> BoundaryPolicy {
        BoundaryPolicy {
            peer_placement,
            corner_guard_pixels: 32,
            require_second_push: false,
        }
    }

    fn boundary_sample(
        boundary: ScreenBoundary,
        offset: u32,
        screen: PhysicalScreenSize,
    ) -> PointerSample {
        match boundary {
            ScreenBoundary::Left => PointerSample {
                position: PhysicalPoint { x: 0, y: offset },
                delta_x: -1,
                delta_y: 0,
            },
            ScreenBoundary::Right => PointerSample {
                position: PhysicalPoint {
                    x: screen.width - 1,
                    y: offset,
                },
                delta_x: 1,
                delta_y: 0,
            },
            ScreenBoundary::Top => PointerSample {
                position: PhysicalPoint { x: offset, y: 0 },
                delta_x: 0,
                delta_y: -1,
            },
            ScreenBoundary::Bottom => PointerSample {
                position: PhysicalPoint {
                    x: offset,
                    y: screen.height - 1,
                },
                delta_x: 0,
                delta_y: 1,
            },
        }
    }

    fn request_from(action: ControlProtocolAction) -> ControlTransitionRequest {
        match action {
            ControlProtocolAction::SendEnterRequest(request)
            | ControlProtocolAction::SendReturnRequest(request) => request,
            _ => panic!("expected transition request"),
        }
    }

    fn ack_from(actions: &[ControlProtocolAction]) -> ControlTransitionAck {
        actions
            .iter()
            .find_map(|action| match action {
                ControlProtocolAction::SendEnterAcknowledgement(acknowledgement)
                | ControlProtocolAction::SendReturnAcknowledgement(acknowledgement) => {
                    Some(*acknowledgement)
                }
                _ => None,
            })
            .expect("expected acknowledgement")
    }

    #[test]
    fn every_layout_is_acknowledgement_gated_and_returns_on_the_only_inverse_edge() {
        let local = screen(3_840, 2_160);
        let peer = screen(2_560, 1_440);
        for placement in [
            PeerPlacement::Left,
            PeerPlacement::Right,
            PeerPlacement::Above,
            PeerPlacement::Below,
        ] {
            let mut origin = ControlCoordinator::new(policy(placement), local, peer).unwrap();
            let mut receiver =
                ControlCoordinator::new(policy(opposite_placement(placement)), peer, local)
                    .unwrap();
            let exit = placement.local_exit_boundary();
            let exit_extent = match exit {
                ScreenBoundary::Left | ScreenBoundary::Right => local.height,
                ScreenBoundary::Top | ScreenBoundary::Bottom => local.width,
            };
            let enter = request_from(
                origin
                    .observe_pointer(boundary_sample(exit, exit_extent / 2, local))
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(
                origin.state(),
                ControlProtocolState::AwaitingEnterAcknowledgement
            );
            let receiver_actions = receiver
                .handle_channel_event(ChannelControlEvent::EnterRequest(enter.clone()))
                .unwrap();
            let enter_ack = ack_from(&receiver_actions);
            assert!(enter_ack.accepted);
            assert_eq!(receiver.state(), ControlProtocolState::ControlledByPeer);
            origin
                .handle_channel_event(ChannelControlEvent::EnterAcknowledgement(enter_ack))
                .unwrap();
            assert_eq!(origin.state(), ControlProtocolState::ControllingPeer);

            let return_edge = placement.peer_return_boundary();
            let return_extent = match return_edge {
                ScreenBoundary::Left | ScreenBoundary::Right => peer.height,
                ScreenBoundary::Top | ScreenBoundary::Bottom => peer.width,
            };
            let return_request = request_from(
                origin
                    .observe_pointer(boundary_sample(return_edge, return_extent / 2, peer))
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(return_request.control_epoch, enter.control_epoch);
            let receiver_actions = receiver
                .handle_channel_event(ChannelControlEvent::ReturnRequest(return_request))
                .unwrap();
            let return_ack = ack_from(&receiver_actions);
            assert!(return_ack.accepted);
            origin
                .handle_channel_event(ChannelControlEvent::ReturnAcknowledgement(return_ack))
                .unwrap();
            assert_eq!(origin.state(), ControlProtocolState::Local);
            assert_eq!(receiver.state(), ControlProtocolState::Local);
        }
    }

    #[test]
    fn dynamic_boundary_uses_the_selected_entry_edge_and_its_exact_inverse_return() {
        let local = screen(1_920, 1_080);
        let peer = screen(2_560, 1_440);
        let mut origin =
            ControlCoordinator::new_dynamic(policy(PeerPlacement::Above), local, peer).unwrap();
        let mut receiver =
            ControlCoordinator::new_dynamic(policy(PeerPlacement::Below), peer, local).unwrap();

        assert!(
            origin
                .observe_pointer_at_boundary(
                    boundary_sample(ScreenBoundary::Top, 960, local),
                    None,
                )
                .unwrap()
                .is_none()
        );
        let enter = request_from(
            origin
                .observe_pointer_at_boundary(
                    boundary_sample(ScreenBoundary::Right, 540, local),
                    Some(ScreenBoundary::Right),
                )
                .unwrap()
                .unwrap(),
        );
        assert_eq!(enter.departure_boundary, ScreenBoundary::Right);
        let enter_ack = ack_from(
            &receiver
                .handle_channel_event(ChannelControlEvent::EnterRequest(enter))
                .unwrap(),
        );
        assert!(enter_ack.accepted);
        origin
            .handle_channel_event(ChannelControlEvent::EnterAcknowledgement(enter_ack))
            .unwrap();

        assert!(
            origin
                .observe_pointer_at_boundary(
                    boundary_sample(ScreenBoundary::Top, 1_280, peer),
                    None,
                )
                .unwrap()
                .is_none()
        );
        let return_request = request_from(
            origin
                .observe_pointer_at_boundary(boundary_sample(ScreenBoundary::Left, 720, peer), None)
                .unwrap()
                .unwrap(),
        );
        assert_eq!(return_request.departure_boundary, ScreenBoundary::Left);
        let return_ack = ack_from(
            &receiver
                .handle_channel_event(ChannelControlEvent::ReturnRequest(return_request))
                .unwrap(),
        );
        assert!(return_ack.accepted);
        origin
            .handle_channel_event(ChannelControlEvent::ReturnAcknowledgement(return_ack))
            .unwrap();
        assert_eq!(origin.state(), ControlProtocolState::Local);
        assert_eq!(receiver.state(), ControlProtocolState::Local);
    }

    #[test]
    fn logical_boundary_allows_an_independent_peer_return_edge_and_requires_metadata() {
        let local = screen(1_920, 1_080);
        let peer = screen(2_560, 1_440);
        let mut origin =
            ControlCoordinator::new_logical(policy(PeerPlacement::Right), local, peer).unwrap();
        let mut receiver =
            ControlCoordinator::new_logical(policy(PeerPlacement::Left), peer, local).unwrap();

        let mut enter = request_from(
            origin
                .observe_pointer_at_boundaries(
                    boundary_sample(ScreenBoundary::Right, 540, local),
                    Some(ScreenBoundary::Right),
                    None,
                )
                .unwrap()
                .unwrap(),
        );
        assert!(
            !ack_from(
                &receiver
                    .handle_channel_event(ChannelControlEvent::EnterRequest(enter.clone()))
                    .unwrap()
            )
            .accepted
        );
        enter.logical_boundary = Some(super::super::channel::LogicalBoundaryTransition {
            boundary_id: "right-to-top".to_owned(),
            mapping_revision: 3,
            parameter: 30_000,
        });
        let enter_ack = ack_from(
            &receiver
                .handle_channel_event(ChannelControlEvent::EnterRequest(enter))
                .unwrap(),
        );
        assert!(enter_ack.accepted);
        origin
            .handle_channel_event(ChannelControlEvent::EnterAcknowledgement(enter_ack))
            .unwrap();

        let mut returned = request_from(
            origin
                .observe_pointer_at_boundaries(
                    boundary_sample(ScreenBoundary::Top, 1_280, peer),
                    None,
                    Some(ScreenBoundary::Top),
                )
                .unwrap()
                .unwrap(),
        );
        assert_eq!(returned.departure_boundary, ScreenBoundary::Top);
        returned.logical_boundary = Some(super::super::channel::LogicalBoundaryTransition {
            boundary_id: "right-to-top".to_owned(),
            mapping_revision: 3,
            parameter: 35_000,
        });
        let return_ack = ack_from(
            &receiver
                .handle_channel_event(ChannelControlEvent::ReturnRequest(returned))
                .unwrap(),
        );
        assert!(return_ack.accepted);
        origin
            .handle_channel_event(ChannelControlEvent::ReturnAcknowledgement(return_ack))
            .unwrap();
        assert_eq!(origin.state(), ControlProtocolState::Local);
        assert_eq!(receiver.state(), ControlProtocolState::Local);
    }

    #[test]
    fn wrong_boundary_replay_and_stale_acknowledgement_are_rejected() {
        let local = screen(1_920, 1_080);
        let peer = screen(2_560, 1_440);
        let mut receiver =
            ControlCoordinator::new(policy(PeerPlacement::Left), local, peer).unwrap();
        let wrong = ControlTransitionRequest {
            control_epoch: 41,
            departure_boundary: ScreenBoundary::Left,
            normalized_edge_offset: 12_000,
            logical_boundary: None,
        };
        assert!(
            !ack_from(
                &receiver
                    .handle_channel_event(ChannelControlEvent::EnterRequest(wrong.clone()))
                    .unwrap()
            )
            .accepted
        );
        assert_eq!(receiver.state(), ControlProtocolState::Local);

        let valid = ControlTransitionRequest {
            departure_boundary: ScreenBoundary::Right,
            ..wrong
        };
        assert!(
            ack_from(
                &receiver
                    .handle_channel_event(ChannelControlEvent::EnterRequest(valid.clone()))
                    .unwrap()
            )
            .accepted
        );
        let _ = receiver.disconnect();
        assert!(
            !ack_from(
                &receiver
                    .handle_channel_event(ChannelControlEvent::EnterRequest(valid))
                    .unwrap()
            )
            .accepted
        );

        let stale = ControlTransitionAck {
            control_epoch: 999,
            accepted: true,
        };
        assert!(
            receiver
                .handle_channel_event(ChannelControlEvent::EnterAcknowledgement(stale))
                .is_err()
        );
        assert_eq!(receiver.state(), ControlProtocolState::Local);
    }

    #[test]
    fn simultaneous_enter_requests_are_both_rejected_without_transferring_control() {
        let first_screen = screen(1_920, 1_080);
        let second_screen = screen(2_560, 1_440);
        let mut first =
            ControlCoordinator::new(policy(PeerPlacement::Right), first_screen, second_screen)
                .unwrap();
        let mut second =
            ControlCoordinator::new(policy(PeerPlacement::Left), second_screen, first_screen)
                .unwrap();
        let first_request = request_from(
            first
                .observe_pointer(boundary_sample(
                    ScreenBoundary::Right,
                    first_screen.height / 2,
                    first_screen,
                ))
                .unwrap()
                .unwrap(),
        );
        let second_request = request_from(
            second
                .observe_pointer(boundary_sample(
                    ScreenBoundary::Left,
                    second_screen.height / 2,
                    second_screen,
                ))
                .unwrap()
                .unwrap(),
        );
        let first_ack = ack_from(
            &second
                .handle_channel_event(ChannelControlEvent::EnterRequest(first_request))
                .unwrap(),
        );
        let second_ack = ack_from(
            &first
                .handle_channel_event(ChannelControlEvent::EnterRequest(second_request))
                .unwrap(),
        );
        assert!(!first_ack.accepted);
        assert!(!second_ack.accepted);
        first
            .handle_channel_event(ChannelControlEvent::EnterAcknowledgement(first_ack))
            .unwrap();
        second
            .handle_channel_event(ChannelControlEvent::EnterAcknowledgement(second_ack))
            .unwrap();
        assert_eq!(first.state(), ControlProtocolState::Local);
        assert_eq!(second.state(), ControlProtocolState::Local);
    }

    #[test]
    fn remote_control_epochs_remain_strictly_monotonic_beyond_replay_window_sizes() {
        let local = screen(1_920, 1_080);
        let peer = screen(2_560, 1_440);
        let mut receiver =
            ControlCoordinator::new(policy(PeerPlacement::Left), local, peer).unwrap();
        for epoch in 100..200 {
            let enter = ControlTransitionRequest {
                control_epoch: epoch,
                departure_boundary: ScreenBoundary::Right,
                normalized_edge_offset: 20_000,
                logical_boundary: None,
            };
            assert!(
                ack_from(
                    &receiver
                        .handle_channel_event(ChannelControlEvent::EnterRequest(enter.clone()))
                        .unwrap()
                )
                .accepted
            );
            let return_request = ControlTransitionRequest {
                departure_boundary: ScreenBoundary::Left,
                ..enter
            };
            assert!(
                ack_from(
                    &receiver
                        .handle_channel_event(ChannelControlEvent::ReturnRequest(return_request))
                        .unwrap()
                )
                .accepted
            );
        }
        let replay = ControlTransitionRequest {
            control_epoch: 100,
            departure_boundary: ScreenBoundary::Right,
            normalized_edge_offset: 20_000,
            logical_boundary: None,
        };
        assert!(
            !ack_from(
                &receiver
                    .handle_channel_event(ChannelControlEvent::EnterRequest(replay))
                    .unwrap()
            )
            .accepted
        );
        assert_eq!(receiver.state(), ControlProtocolState::Local);
    }

    #[test]
    fn wrong_return_epoch_and_boundary_leave_imported_control_unchanged() {
        let local = screen(1_920, 1_080);
        let peer = screen(2_560, 1_440);
        let mut receiver =
            ControlCoordinator::new(policy(PeerPlacement::Left), local, peer).unwrap();
        let enter = ControlTransitionRequest {
            control_epoch: 501,
            departure_boundary: ScreenBoundary::Right,
            normalized_edge_offset: 20_000,
            logical_boundary: None,
        };
        assert!(
            ack_from(
                &receiver
                    .handle_channel_event(ChannelControlEvent::EnterRequest(enter.clone()))
                    .unwrap()
            )
            .accepted
        );
        for invalid in [
            ControlTransitionRequest {
                control_epoch: 502,
                departure_boundary: ScreenBoundary::Left,
                ..enter.clone()
            },
            ControlTransitionRequest {
                departure_boundary: ScreenBoundary::Right,
                ..enter.clone()
            },
        ] {
            assert!(
                !ack_from(
                    &receiver
                        .handle_channel_event(ChannelControlEvent::ReturnRequest(invalid))
                        .unwrap()
                )
                .accepted
            );
            assert_eq!(receiver.state(), ControlProtocolState::ControlledByPeer);
            assert_eq!(receiver.active_control_epoch(), Some(enter.control_epoch));
        }
    }

    #[test]
    fn disconnect_release_and_screen_change_always_force_local_control() {
        let local = screen(1_920, 1_080);
        let peer = screen(2_560, 1_440);
        let mut coordinator =
            ControlCoordinator::new(policy(PeerPlacement::Right), local, peer).unwrap();
        let request = request_from(
            coordinator
                .observe_pointer(boundary_sample(
                    ScreenBoundary::Right,
                    local.height / 2,
                    local,
                ))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            coordinator.disconnect(),
            [
                ControlProtocolAction::ReleaseLocalInput,
                ControlProtocolAction::SendReleaseAll,
            ]
        );
        assert_eq!(coordinator.state(), ControlProtocolState::Local);
        assert_eq!(coordinator.active_control_epoch(), None);

        let imported_epoch = following_epoch(request.control_epoch);
        let actions = coordinator
            .handle_channel_event(ChannelControlEvent::EnterRequest(
                ControlTransitionRequest {
                    control_epoch: imported_epoch,
                    departure_boundary: ScreenBoundary::Left,
                    normalized_edge_offset: request.normalized_edge_offset,
                    logical_boundary: None,
                },
            ))
            .unwrap();
        assert!(ack_from(&actions).accepted);
        let changed = screen(3_000, 2_000);
        assert_eq!(
            coordinator
                .handle_channel_event(ChannelControlEvent::ScreenInfo(changed))
                .unwrap(),
            [
                ControlProtocolAction::ReleaseLocalInput,
                ControlProtocolAction::SendReleaseAll,
            ]
        );
        assert_eq!(coordinator.peer_screen(), changed);
        assert_eq!(coordinator.state(), ControlProtocolState::Local);

        let next_enter = ControlTransitionRequest {
            control_epoch: following_epoch(imported_epoch),
            departure_boundary: ScreenBoundary::Left,
            normalized_edge_offset: request.normalized_edge_offset,
            logical_boundary: None,
        };
        assert!(
            ack_from(
                &coordinator
                    .handle_channel_event(ChannelControlEvent::EnterRequest(next_enter))
                    .unwrap()
            )
            .accepted
        );
        let changed_local = screen(2_880, 1_800);
        assert_eq!(
            coordinator.update_local_screen(changed_local).unwrap(),
            [
                ControlProtocolAction::ReleaseLocalInput,
                ControlProtocolAction::SendReleaseAll,
            ]
        );
        assert_eq!(coordinator.local_screen(), changed_local);
        assert_eq!(coordinator.state(), ControlProtocolState::Local);

        let invalid_for_guard = screen(100, 64);
        assert!(coordinator.update_local_screen(invalid_for_guard).is_err());
        assert_eq!(coordinator.local_screen(), changed_local);
    }

    #[test]
    fn loopback_tls_control_protocol_diagnostic_is_clean() {
        let report = run_control_protocol_diagnostic().unwrap();
        assert!(report.clean);
        assert_eq!(report.screen_info_exchanges, 2);
        assert_eq!(report.enter_round_trips, 1);
        assert_eq!(report.return_round_trips, 1);
        assert_eq!(report.rejected_wrong_boundaries, 1);
        assert_eq!(report.rejected_replays, 1);
        assert_eq!(report.rejected_stale_acknowledgements, 1);
        assert!(report.local_release_requests >= 4);
        assert_ne!(report.control_epoch, 0);
        assert_eq!(report.first_channel_epoch, report.second_channel_epoch);
    }
}
