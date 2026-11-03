use std::io;

use serde::{Deserialize, Serialize};

const NORMALIZED_EDGE_MAX: u32 = u16::MAX as u32;
const MAX_SCREEN_EXTENT: u32 = i32::MAX as u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerPlacement {
    Left,
    Right,
    Above,
    Below,
}

impl PeerPlacement {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Above => "above",
            Self::Below => "below",
        }
    }

    #[must_use]
    pub const fn local_exit_boundary(self) -> ScreenBoundary {
        match self {
            Self::Left => ScreenBoundary::Left,
            Self::Right => ScreenBoundary::Right,
            Self::Above => ScreenBoundary::Top,
            Self::Below => ScreenBoundary::Bottom,
        }
    }

    #[must_use]
    pub const fn peer_return_boundary(self) -> ScreenBoundary {
        self.local_exit_boundary().opposite()
    }

    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Above => Self::Below,
            Self::Below => Self::Above,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenBoundary {
    Left,
    Right,
    Top,
    Bottom,
}

impl ScreenBoundary {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }

    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    const fn edge_extent(self, screen: PhysicalScreenSize) -> u32 {
        match self {
            Self::Left | Self::Right => screen.height,
            Self::Top | Self::Bottom => screen.width,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalScreenSize {
    pub width: u32,
    pub height: u32,
}

impl PhysicalScreenSize {
    /// Creates one primary-screen size expressed only in physical pixels.
    ///
    /// # Errors
    ///
    /// Returns an error for degenerate or non-Windows coordinate extents.
    pub fn new(width: u32, height: u32) -> io::Result<Self> {
        if width < 2 || height < 2 || width > MAX_SCREEN_EXTENT || height > MAX_SCREEN_EXTENT {
            return Err(invalid_data(
                "physical screen dimensions must be between 2 and i32::MAX pixels",
            ));
        }
        Ok(Self { width, height })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalPoint {
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerSample {
    pub position: PhysicalPoint,
    pub delta_x: i32,
    pub delta_y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundaryPolicy {
    pub peer_placement: PeerPlacement,
    pub corner_guard_pixels: u32,
    pub require_second_push: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlSurface {
    Local,
    Peer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryControlState {
    Local,
    EnterPending,
    Peer,
    ReturnPending,
}

impl BoundaryControlState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::EnterPending => "enter_pending",
            Self::Peer => "peer",
            Self::ReturnPending => "return_pending",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundaryTransition {
    pub from: ControlSurface,
    pub to: ControlSurface,
    pub departure_boundary: ScreenBoundary,
    pub arrival_boundary: ScreenBoundary,
    pub normalized_edge_offset: u16,
    pub destination: PhysicalPoint,
    pub destination_offset_clamped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryDecision {
    None,
    Primed(ScreenBoundary),
    Pending,
    TransitionRequested(BoundaryTransition),
}

#[derive(Clone, Debug)]
pub struct StrictBoundaryEngine {
    policy: BoundaryPolicy,
    local_screen: PhysicalScreenSize,
    peer_screen: PhysicalScreenSize,
    state: BoundaryControlState,
    primed_boundary: Option<ScreenBoundary>,
    pending: Option<BoundaryTransition>,
    local_exit_requires_rearm: bool,
    active_local_departure: ScreenBoundary,
}

impl StrictBoundaryEngine {
    /// Creates one two-screen, single-open-boundary state machine.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured physical corner guard consumes either open edge.
    pub fn new(
        policy: BoundaryPolicy,
        local_screen: PhysicalScreenSize,
        peer_screen: PhysicalScreenSize,
    ) -> io::Result<Self> {
        validate_configuration(policy, local_screen, peer_screen)?;
        Ok(Self {
            policy,
            local_screen,
            peer_screen,
            state: BoundaryControlState::Local,
            primed_boundary: None,
            pending: None,
            local_exit_requires_rearm: false,
            active_local_departure: policy.peer_placement.local_exit_boundary(),
        })
    }

    #[must_use]
    pub const fn state(&self) -> BoundaryControlState {
        self.state
    }

    #[must_use]
    pub const fn pending_transition(&self) -> Option<BoundaryTransition> {
        self.pending
    }

    /// Atomically applies a confirmed layout or physical display change and returns control local.
    ///
    /// # Errors
    ///
    /// Returns an error without changing the current engine if the new corner guard consumes an
    /// open edge.
    pub fn reconfigure(
        &mut self,
        policy: BoundaryPolicy,
        local_screen: PhysicalScreenSize,
        peer_screen: PhysicalScreenSize,
    ) -> io::Result<()> {
        validate_configuration(policy, local_screen, peer_screen)?;
        self.policy = policy;
        self.local_screen = local_screen;
        self.peer_screen = peer_screen;
        self.active_local_departure = policy.peer_placement.local_exit_boundary();
        self.force_local();
        Ok(())
    }

    /// Observes one physical pointer sample on the currently controlled screen.
    ///
    /// Only the configured local exit edge and its exact opposite peer return edge are eligible.
    /// A request remains pending until `confirm_pending` or `reject_pending` is called.
    ///
    /// # Errors
    ///
    /// Returns an error when the sample is outside the active primary screen.
    pub fn observe(&mut self, sample: PointerSample) -> io::Result<BoundaryDecision> {
        self.observe_with_local_boundary(
            sample,
            Some(self.policy.peer_placement.local_exit_boundary()),
        )
    }

    /// Observes a pointer while allowing the caller's validated V2 map to select the local edge.
    ///
    /// `local_departure` is consulted only while control is local. `None` means that the sample did
    /// not hit an assigned boundary and therefore cannot transfer control. Once control is on the
    /// peer, return is always limited to the exact opposite of the edge used for entry.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-screen sample or a guard that consumes the selected edge.
    pub fn observe_with_local_boundary(
        &mut self,
        sample: PointerSample,
        local_departure: Option<ScreenBoundary>,
    ) -> io::Result<BoundaryDecision> {
        self.observe_with_boundaries(
            sample,
            local_departure,
            Some(self.active_local_departure.opposite()),
        )
    }

    /// Observes a pointer with independently selected local and peer departure edges.
    ///
    /// This is used by compiled logical portals, where the return edge is part of the confirmed
    /// mapping and does not need to be the geometric opposite of the local exit edge.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-screen sample or a guard that consumes the selected edge.
    pub fn observe_with_boundaries(
        &mut self,
        sample: PointerSample,
        local_departure: Option<ScreenBoundary>,
        peer_departure: Option<ScreenBoundary>,
    ) -> io::Result<BoundaryDecision> {
        let (surface, screen, departure, destination_screen) = match self.state {
            BoundaryControlState::Local => (
                ControlSurface::Local,
                self.local_screen,
                local_departure,
                self.peer_screen,
            ),
            BoundaryControlState::Peer => (
                ControlSurface::Peer,
                self.peer_screen,
                peer_departure,
                self.local_screen,
            ),
            BoundaryControlState::EnterPending | BoundaryControlState::ReturnPending => {
                return Ok(BoundaryDecision::Pending);
            }
        };
        validate_sample(sample, screen)?;
        if surface == ControlSurface::Local && self.local_exit_requires_rearm {
            if point_is_on_boundary(sample.position, screen, self.active_local_departure) {
                self.primed_boundary = None;
                return Ok(BoundaryDecision::None);
            }
            self.local_exit_requires_rearm = false;
        }
        let Some(departure) = departure else {
            self.primed_boundary = None;
            return Ok(BoundaryDecision::None);
        };
        if !qualifies_for_transition(sample, screen, departure, self.policy.corner_guard_pixels) {
            self.primed_boundary = None;
            return Ok(BoundaryDecision::None);
        }
        if self.policy.require_second_push && self.primed_boundary != Some(departure) {
            self.primed_boundary = Some(departure);
            return Ok(BoundaryDecision::Primed(departure));
        }
        self.primed_boundary = None;
        validate_guard(
            self.policy.corner_guard_pixels,
            departure.edge_extent(screen),
        )?;
        validate_guard(
            self.policy.corner_guard_pixels,
            departure.opposite().edge_extent(destination_screen),
        )?;
        let transition = build_transition(
            surface,
            sample.position,
            screen,
            destination_screen,
            departure,
            self.policy.corner_guard_pixels,
        );
        self.pending = Some(transition);
        if surface == ControlSurface::Local {
            self.active_local_departure = departure;
        }
        self.state = match surface {
            ControlSurface::Local => BoundaryControlState::EnterPending,
            ControlSurface::Peer => BoundaryControlState::ReturnPending,
        };
        Ok(BoundaryDecision::TransitionRequested(transition))
    }

    /// Confirms the exact pending transition after the peer acknowledgement arrives.
    ///
    /// # Errors
    ///
    /// Returns an error when no switch is pending.
    pub fn confirm_pending(&mut self) -> io::Result<BoundaryTransition> {
        let transition = self
            .pending
            .take()
            .ok_or_else(|| invalid_data("no cross-screen boundary transition is pending"))?;
        self.state = match transition.to {
            ControlSurface::Local => BoundaryControlState::Local,
            ControlSurface::Peer => BoundaryControlState::Peer,
        };
        self.primed_boundary = None;
        Ok(transition)
    }

    /// Rejects a pending transition and keeps control on its departure screen.
    pub fn reject_pending(&mut self) -> Option<BoundaryTransition> {
        let transition = self.pending.take()?;
        self.state = match transition.from {
            ControlSurface::Local => BoundaryControlState::Local,
            ControlSurface::Peer => BoundaryControlState::Peer,
        };
        self.primed_boundary = None;
        Some(transition)
    }

    /// Cancels pending state and returns control to the local screen after a disconnect or stop.
    pub fn force_local(&mut self) {
        let requires_rearm =
            self.local_exit_requires_rearm || self.state != BoundaryControlState::Local;
        self.pending = None;
        self.primed_boundary = None;
        self.state = BoundaryControlState::Local;
        self.local_exit_requires_rearm = requires_rearm;
    }
}

fn validate_configuration(
    policy: BoundaryPolicy,
    local_screen: PhysicalScreenSize,
    peer_screen: PhysicalScreenSize,
) -> io::Result<()> {
    validate_guard(
        policy.corner_guard_pixels,
        policy
            .peer_placement
            .local_exit_boundary()
            .edge_extent(local_screen),
    )?;
    validate_guard(
        policy.corner_guard_pixels,
        policy
            .peer_placement
            .peer_return_boundary()
            .edge_extent(peer_screen),
    )
}

fn validate_guard(guard: u32, edge_extent: u32) -> io::Result<()> {
    if guard.saturating_mul(2) >= edge_extent {
        return Err(invalid_data(
            "corner guard consumes the complete cross-screen boundary",
        ));
    }
    Ok(())
}

fn validate_sample(sample: PointerSample, screen: PhysicalScreenSize) -> io::Result<()> {
    if sample.position.x >= screen.width || sample.position.y >= screen.height {
        return Err(invalid_data(
            "physical pointer sample is outside the active primary screen",
        ));
    }
    Ok(())
}

fn qualifies_for_transition(
    sample: PointerSample,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
    guard: u32,
) -> bool {
    let at_boundary = match boundary {
        ScreenBoundary::Left => sample.position.x == 0 && sample.delta_x < 0,
        ScreenBoundary::Right => sample.position.x == screen.width - 1 && sample.delta_x > 0,
        ScreenBoundary::Top => sample.position.y == 0 && sample.delta_y < 0,
        ScreenBoundary::Bottom => sample.position.y == screen.height - 1 && sample.delta_y > 0,
    };
    at_boundary
        && offset_is_outside_corners(
            edge_offset(sample.position, boundary),
            screen,
            boundary,
            guard,
        )
}

const fn point_is_on_boundary(
    point: PhysicalPoint,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
) -> bool {
    match boundary {
        ScreenBoundary::Left => point.x == 0,
        ScreenBoundary::Right => point.x == screen.width - 1,
        ScreenBoundary::Top => point.y == 0,
        ScreenBoundary::Bottom => point.y == screen.height - 1,
    }
}

fn offset_is_outside_corners(
    offset: u32,
    screen: PhysicalScreenSize,
    boundary: ScreenBoundary,
    guard: u32,
) -> bool {
    let extent = boundary.edge_extent(screen);
    offset >= guard && offset < extent - guard
}

fn edge_offset(point: PhysicalPoint, boundary: ScreenBoundary) -> u32 {
    match boundary {
        ScreenBoundary::Left | ScreenBoundary::Right => point.y,
        ScreenBoundary::Top | ScreenBoundary::Bottom => point.x,
    }
}

fn build_transition(
    from: ControlSurface,
    source_point: PhysicalPoint,
    source_screen: PhysicalScreenSize,
    destination_screen: PhysicalScreenSize,
    departure: ScreenBoundary,
    guard: u32,
) -> BoundaryTransition {
    let arrival = departure.opposite();
    let source_extent = departure.edge_extent(source_screen);
    let destination_extent = arrival.edge_extent(destination_screen);
    let normalized = normalize_edge_offset(edge_offset(source_point, departure), source_extent);
    let mapped = denormalize_edge_offset(normalized, destination_extent);
    let minimum = guard;
    let maximum = destination_extent - 1 - guard;
    let safe_mapped = mapped.clamp(minimum, maximum);
    let destination = point_on_boundary(arrival, safe_mapped, destination_screen);
    BoundaryTransition {
        from,
        to: match from {
            ControlSurface::Local => ControlSurface::Peer,
            ControlSurface::Peer => ControlSurface::Local,
        },
        departure_boundary: departure,
        arrival_boundary: arrival,
        normalized_edge_offset: normalized,
        destination,
        destination_offset_clamped: safe_mapped != mapped,
    }
}

fn normalize_edge_offset(offset: u32, extent: u32) -> u16 {
    u16::try_from(u64::from(offset) * u64::from(NORMALIZED_EDGE_MAX) / u64::from(extent - 1))
        .expect("normalized edge offset fits in u16")
}

fn denormalize_edge_offset(normalized: u16, extent: u32) -> u32 {
    let numerator =
        u64::from(normalized) * u64::from(extent - 1) + u64::from(NORMALIZED_EDGE_MAX / 2);
    u32::try_from(numerator / u64::from(NORMALIZED_EDGE_MAX))
        .expect("physical edge offset fits in u32")
}

fn point_on_boundary(
    boundary: ScreenBoundary,
    offset: u32,
    screen: PhysicalScreenSize,
) -> PhysicalPoint {
    match boundary {
        ScreenBoundary::Left => PhysicalPoint { x: 0, y: offset },
        ScreenBoundary::Right => PhysicalPoint {
            x: screen.width - 1,
            y: offset,
        },
        ScreenBoundary::Top => PhysicalPoint { x: offset, y: 0 },
        ScreenBoundary::Bottom => PhysicalPoint {
            x: offset,
            y: screen.height - 1,
        },
    }
}

/// Maps a normalized edge offset onto one physical boundary and clamps it outside the configured
/// corner guards.
#[must_use]
pub fn map_normalized_boundary_position(
    normalized: u16,
    boundary: ScreenBoundary,
    screen: PhysicalScreenSize,
    corner_guard_pixels: u32,
) -> PhysicalPoint {
    let extent = boundary.edge_extent(screen);
    let mapped = denormalize_edge_offset(normalized, extent);
    let safe = mapped.clamp(corner_guard_pixels, extent - 1 - corner_guard_pixels);
    point_on_boundary(boundary, safe, screen)
}

fn sample_on_boundary(
    boundary: ScreenBoundary,
    offset: u32,
    screen: PhysicalScreenSize,
) -> PointerSample {
    let position = point_on_boundary(boundary, offset, screen);
    let (delta_x, delta_y) = match boundary {
        ScreenBoundary::Left => (-1, 0),
        ScreenBoundary::Right => (1, 0),
        ScreenBoundary::Top => (0, -1),
        ScreenBoundary::Bottom => (0, 1),
    };
    PointerSample {
        position,
        delta_x,
        delta_y,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundaryDiagnosticReport {
    pub layouts: u32,
    pub dpi_profiles: u32,
    pub resolution_pairs: u32,
    pub transition_round_trips: u32,
    pub rejected_wrong_edges: u32,
    pub blocked_corner_samples: u32,
    pub maximum_mapping_error_pixels: u32,
    pub clean: bool,
}

/// Runs a deterministic, network-free boundary and physical-pixel mapping matrix.
///
/// # Errors
///
/// Returns an error if any layout opens a wrong edge, accepts a protected corner, changes control
/// before confirmation, or maps an unprotected proportional landing by more than one pixel.
pub fn run_boundary_diagnostic() -> io::Result<BoundaryDiagnosticReport> {
    let profiles = diagnostic_physical_screens()?;
    let placements = [
        PeerPlacement::Left,
        PeerPlacement::Right,
        PeerPlacement::Above,
        PeerPlacement::Below,
    ];
    let mut report = BoundaryDiagnosticReport {
        layouts: 4,
        dpi_profiles: 5,
        resolution_pairs: 0,
        transition_round_trips: 0,
        rejected_wrong_edges: 0,
        blocked_corner_samples: 0,
        maximum_mapping_error_pixels: 0,
        clean: false,
    };
    for placement in placements {
        verify_closed_edges(placement, profiles[0], profiles[1], &mut report)?;
        verify_corner_guard(placement, profiles[0], profiles[1], &mut report)?;
        for local in profiles {
            for peer in profiles {
                for numerator in [1, 2, 3] {
                    verify_round_trip(placement, local, peer, numerator, 4, &mut report)?;
                }
                report.resolution_pairs += 1;
            }
        }
    }
    report.clean = true;
    Ok(report)
}

fn diagnostic_physical_screens() -> io::Result<[PhysicalScreenSize; 5]> {
    Ok([
        physical_screen_from_logical(1920, 1080, 96)?,
        physical_screen_from_logical(1536, 864, 120)?,
        physical_screen_from_logical(2560, 1440, 96)?,
        physical_screen_from_logical(2560, 1440, 144)?,
        physical_screen_from_logical(1600, 900, 120)?,
    ])
}

fn physical_screen_from_logical(
    width: u32,
    height: u32,
    dpi: u32,
) -> io::Result<PhysicalScreenSize> {
    let physical_width = width
        .checked_mul(dpi)
        .ok_or_else(|| invalid_data("diagnostic physical width overflow"))?
        / 96;
    let physical_height = height
        .checked_mul(dpi)
        .ok_or_else(|| invalid_data("diagnostic physical height overflow"))?
        / 96;
    PhysicalScreenSize::new(physical_width, physical_height)
}

fn diagnostic_policy(placement: PeerPlacement) -> BoundaryPolicy {
    BoundaryPolicy {
        peer_placement: placement,
        corner_guard_pixels: 32,
        require_second_push: false,
    }
}

fn verify_closed_edges(
    placement: PeerPlacement,
    local: PhysicalScreenSize,
    peer: PhysicalScreenSize,
    report: &mut BoundaryDiagnosticReport,
) -> io::Result<()> {
    let open = placement.local_exit_boundary();
    for boundary in [
        ScreenBoundary::Left,
        ScreenBoundary::Right,
        ScreenBoundary::Top,
        ScreenBoundary::Bottom,
    ] {
        if boundary == open {
            continue;
        }
        let mut engine = StrictBoundaryEngine::new(diagnostic_policy(placement), local, peer)?;
        let offset = boundary.edge_extent(local) / 2;
        if engine.observe(sample_on_boundary(boundary, offset, local))? != BoundaryDecision::None {
            return Err(invalid_data(
                "a closed screen boundary accepted a transition",
            ));
        }
        report.rejected_wrong_edges += 1;
    }
    let return_boundary = placement.peer_return_boundary();
    for boundary in [
        ScreenBoundary::Left,
        ScreenBoundary::Right,
        ScreenBoundary::Top,
        ScreenBoundary::Bottom,
    ] {
        if boundary == return_boundary {
            continue;
        }
        let mut engine = StrictBoundaryEngine::new(diagnostic_policy(placement), local, peer)?;
        enter_peer(&mut engine, placement, local)?;
        let offset = boundary.edge_extent(peer) / 2;
        if engine.observe(sample_on_boundary(boundary, offset, peer))? != BoundaryDecision::None {
            return Err(invalid_data(
                "a closed peer return boundary accepted a transition",
            ));
        }
        report.rejected_wrong_edges += 1;
    }
    Ok(())
}

fn enter_peer(
    engine: &mut StrictBoundaryEngine,
    placement: PeerPlacement,
    local: PhysicalScreenSize,
) -> io::Result<()> {
    let departure = placement.local_exit_boundary();
    let offset = departure.edge_extent(local) / 2;
    if !matches!(
        engine.observe(sample_on_boundary(departure, offset, local))?,
        BoundaryDecision::TransitionRequested(_)
    ) {
        return Err(invalid_data("valid local edge did not request peer entry"));
    }
    engine.confirm_pending()?;
    Ok(())
}

fn verify_corner_guard(
    placement: PeerPlacement,
    local: PhysicalScreenSize,
    peer: PhysicalScreenSize,
    report: &mut BoundaryDiagnosticReport,
) -> io::Result<()> {
    let boundary = placement.local_exit_boundary();
    let extent = boundary.edge_extent(local);
    for offset in [0, 31, extent - 32, extent - 1] {
        let mut engine = StrictBoundaryEngine::new(diagnostic_policy(placement), local, peer)?;
        if engine.observe(sample_on_boundary(boundary, offset, local))? != BoundaryDecision::None {
            return Err(invalid_data(
                "a protected screen corner accepted a transition",
            ));
        }
        report.blocked_corner_samples += 1;
    }
    Ok(())
}

fn verify_round_trip(
    placement: PeerPlacement,
    local: PhysicalScreenSize,
    peer: PhysicalScreenSize,
    numerator: u32,
    denominator: u32,
    report: &mut BoundaryDiagnosticReport,
) -> io::Result<()> {
    let mut engine = StrictBoundaryEngine::new(diagnostic_policy(placement), local, peer)?;
    let departure = placement.local_exit_boundary();
    let source_extent = departure.edge_extent(local);
    let source_offset = (source_extent - 1) * numerator / denominator;
    let requested = engine.observe(sample_on_boundary(departure, source_offset, local))?;
    let BoundaryDecision::TransitionRequested(enter) = requested else {
        return Err(invalid_data(
            "valid local open boundary did not request entry",
        ));
    };
    if engine.state() != BoundaryControlState::EnterPending
        || engine.observe(sample_on_boundary(departure, source_offset, local))?
            != BoundaryDecision::Pending
    {
        return Err(invalid_data(
            "boundary engine changed control before entry acknowledgement",
        ));
    }
    report.maximum_mapping_error_pixels = report.maximum_mapping_error_pixels.max(mapping_error(
        source_offset,
        source_extent,
        edge_offset(enter.destination, enter.arrival_boundary),
        enter.arrival_boundary.edge_extent(peer),
    ));
    engine.confirm_pending()?;
    let return_offset = edge_offset(enter.destination, enter.arrival_boundary);
    let requested = engine.observe(sample_on_boundary(
        enter.arrival_boundary,
        return_offset,
        peer,
    ))?;
    let BoundaryDecision::TransitionRequested(back) = requested else {
        return Err(invalid_data(
            "peer opposite boundary did not request return",
        ));
    };
    if back.arrival_boundary != departure || back.to != ControlSurface::Local {
        return Err(invalid_data(
            "return transition used a non-adjacent boundary",
        ));
    }
    engine.confirm_pending()?;
    if engine.state() != BoundaryControlState::Local {
        return Err(invalid_data("round trip did not restore local control"));
    }
    if report.maximum_mapping_error_pixels > 1 {
        return Err(invalid_data(
            "physical edge proportional mapping exceeded one pixel of error",
        ));
    }
    report.transition_round_trips += 1;
    Ok(())
}

fn mapping_error(
    source_offset: u32,
    source_extent: u32,
    destination_offset: u32,
    destination_extent: u32,
) -> u32 {
    let expected_numerator = u64::from(source_offset) * u64::from(destination_extent - 1);
    let rounded =
        (expected_numerator + u64::from((source_extent - 1) / 2)) / u64::from(source_extent - 1);
    u32::try_from(rounded.abs_diff(u64::from(destination_offset))).expect("mapping error fits u32")
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(width: u32, height: u32) -> PhysicalScreenSize {
        PhysicalScreenSize::new(width, height).unwrap()
    }

    fn policy(placement: PeerPlacement, guard: u32, second_push: bool) -> BoundaryPolicy {
        BoundaryPolicy {
            peer_placement: placement,
            corner_guard_pixels: guard,
            require_second_push: second_push,
        }
    }

    #[test]
    fn every_layout_opens_only_its_real_adjacent_boundary() {
        let local = screen(1920, 1080);
        let peer = screen(2560, 1440);
        for placement in [
            PeerPlacement::Left,
            PeerPlacement::Right,
            PeerPlacement::Above,
            PeerPlacement::Below,
        ] {
            let expected = placement.local_exit_boundary();
            for boundary in [
                ScreenBoundary::Left,
                ScreenBoundary::Right,
                ScreenBoundary::Top,
                ScreenBoundary::Bottom,
            ] {
                let mut engine =
                    StrictBoundaryEngine::new(policy(placement, 32, false), local, peer).unwrap();
                let sample = sample_on_boundary(boundary, boundary.edge_extent(local) / 2, local);
                let decision = engine.observe(sample).unwrap();
                assert_eq!(
                    matches!(decision, BoundaryDecision::TransitionRequested(_)),
                    boundary == expected
                );
            }
        }
    }

    #[test]
    fn physical_corner_guard_has_exact_inclusive_boundaries() {
        let local = screen(1920, 1080);
        let peer = screen(3840, 2160);
        let boundary = ScreenBoundary::Right;
        for offset in [0, 31, 1048, 1079] {
            let mut engine =
                StrictBoundaryEngine::new(policy(PeerPlacement::Right, 32, false), local, peer)
                    .unwrap();
            assert_eq!(
                engine
                    .observe(sample_on_boundary(boundary, offset, local))
                    .unwrap(),
                BoundaryDecision::None
            );
        }
        for offset in [32, 1047] {
            let mut engine =
                StrictBoundaryEngine::new(policy(PeerPlacement::Right, 32, false), local, peer)
                    .unwrap();
            assert!(matches!(
                engine
                    .observe(sample_on_boundary(boundary, offset, local))
                    .unwrap(),
                BoundaryDecision::TransitionRequested(_)
            ));
        }
    }

    #[test]
    fn second_push_must_be_consecutive_on_the_configured_edge() {
        let local = screen(1920, 1080);
        let peer = screen(2560, 1440);
        let boundary = ScreenBoundary::Right;
        let valid = sample_on_boundary(boundary, 540, local);
        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Right, 32, true), local, peer).unwrap();
        assert_eq!(
            engine.observe(valid).unwrap(),
            BoundaryDecision::Primed(boundary)
        );
        assert!(matches!(
            engine.observe(valid).unwrap(),
            BoundaryDecision::TransitionRequested(_)
        ));

        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Right, 32, true), local, peer).unwrap();
        assert_eq!(
            engine.observe(valid).unwrap(),
            BoundaryDecision::Primed(boundary)
        );
        let neutral = PointerSample {
            position: valid.position,
            delta_x: 0,
            delta_y: 1,
        };
        assert_eq!(engine.observe(neutral).unwrap(), BoundaryDecision::None);
        assert_eq!(
            engine.observe(valid).unwrap(),
            BoundaryDecision::Primed(boundary)
        );
    }

    #[test]
    fn transition_waits_for_acknowledgement_and_rejection_is_local() {
        let local = screen(1920, 1080);
        let peer = screen(3840, 2160);
        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Right, 32, false), local, peer)
                .unwrap();
        let sample = sample_on_boundary(ScreenBoundary::Right, 540, local);
        let requested = engine.observe(sample).unwrap();
        assert!(matches!(
            requested,
            BoundaryDecision::TransitionRequested(_)
        ));
        assert_eq!(engine.state(), BoundaryControlState::EnterPending);
        assert_eq!(engine.observe(sample).unwrap(), BoundaryDecision::Pending);
        assert!(engine.reject_pending().is_some());
        assert_eq!(engine.state(), BoundaryControlState::Local);
        assert!(engine.confirm_pending().is_err());
    }

    #[test]
    fn resolution_mapping_round_trip_stays_within_one_physical_pixel() {
        let report = run_boundary_diagnostic().unwrap();
        assert!(report.clean);
        assert_eq!(report.layouts, 4);
        assert_eq!(report.dpi_profiles, 5);
        assert_eq!(report.resolution_pairs, 100);
        assert_eq!(report.transition_round_trips, 300);
        assert_eq!(report.rejected_wrong_edges, 24);
        assert_eq!(report.blocked_corner_samples, 16);
        assert!(report.maximum_mapping_error_pixels <= 1);
    }

    #[test]
    fn destination_is_clamped_out_of_its_physical_corner_guard() {
        let local = screen(3840, 2160);
        let peer = screen(1920, 1080);
        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Right, 32, false), local, peer)
                .unwrap();
        let decision = engine
            .observe(sample_on_boundary(ScreenBoundary::Right, 32, local))
            .unwrap();
        let BoundaryDecision::TransitionRequested(transition) = decision else {
            panic!("expected transition");
        };
        assert_eq!(transition.destination, PhysicalPoint { x: 0, y: 32 });
        assert!(transition.destination_offset_clamped);
    }

    #[test]
    fn force_local_clears_peer_or_pending_control_state() {
        let local = screen(1920, 1080);
        let peer = screen(2560, 1440);
        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Below, 32, false), local, peer)
                .unwrap();
        let sample = sample_on_boundary(ScreenBoundary::Bottom, 960, local);
        engine.observe(sample).unwrap();
        engine.confirm_pending().unwrap();
        assert_eq!(engine.state(), BoundaryControlState::Peer);
        engine.force_local();
        assert_eq!(engine.state(), BoundaryControlState::Local);
        assert!(engine.pending_transition().is_none());
    }

    #[test]
    fn forced_local_control_must_leave_the_open_edge_before_reentering() {
        let local = screen(1920, 1080);
        let peer = screen(2560, 1440);
        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Below, 32, false), local, peer)
                .unwrap();
        let boundary = sample_on_boundary(ScreenBoundary::Bottom, 960, local);
        assert!(matches!(
            engine.observe(boundary).unwrap(),
            BoundaryDecision::TransitionRequested(_)
        ));
        engine.confirm_pending().unwrap();
        engine.force_local();

        assert_eq!(engine.observe(boundary).unwrap(), BoundaryDecision::None);
        assert_eq!(engine.observe(boundary).unwrap(), BoundaryDecision::None);
        assert_eq!(
            engine
                .observe(PointerSample {
                    position: PhysicalPoint { x: 960, y: 1078 },
                    delta_x: 0,
                    delta_y: -1,
                })
                .unwrap(),
            BoundaryDecision::None
        );
        assert!(matches!(
            engine.observe(boundary).unwrap(),
            BoundaryDecision::TransitionRequested(_)
        ));
    }

    #[test]
    fn physical_display_or_layout_change_is_atomic_and_returns_control_local() {
        let local = screen(1920, 1080);
        let peer = screen(2560, 1440);
        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Below, 32, false), local, peer)
                .unwrap();
        engine
            .observe(sample_on_boundary(ScreenBoundary::Bottom, 960, local))
            .unwrap();
        engine.confirm_pending().unwrap();
        assert_eq!(engine.state(), BoundaryControlState::Peer);

        let resized_local = screen(3840, 2160);
        let resized_peer = screen(1920, 1080);
        engine
            .reconfigure(
                policy(PeerPlacement::Right, 32, false),
                resized_local,
                resized_peer,
            )
            .unwrap();
        assert_eq!(engine.state(), BoundaryControlState::Local);
        assert_eq!(
            engine
                .observe(sample_on_boundary(
                    ScreenBoundary::Bottom,
                    1920,
                    resized_local
                ))
                .unwrap(),
            BoundaryDecision::None
        );
        assert!(matches!(
            engine
                .observe(sample_on_boundary(
                    ScreenBoundary::Right,
                    1080,
                    resized_local
                ))
                .unwrap(),
            BoundaryDecision::TransitionRequested(_)
        ));
    }

    #[test]
    fn invalid_geometry_and_out_of_bounds_samples_fail_closed() {
        assert!(PhysicalScreenSize::new(1, 1080).is_err());
        assert!(PhysicalScreenSize::new(1920, 1).is_err());
        let local = screen(100, 100);
        let peer = screen(100, 100);
        assert!(
            StrictBoundaryEngine::new(policy(PeerPlacement::Right, 50, false), local, peer)
                .is_err()
        );
        let mut engine =
            StrictBoundaryEngine::new(policy(PeerPlacement::Right, 0, false), local, peer).unwrap();
        assert!(
            engine
                .observe(PointerSample {
                    position: PhysicalPoint { x: 100, y: 50 },
                    delta_x: 1,
                    delta_y: 0,
                })
                .is_err()
        );
        assert_eq!(engine.state(), BoundaryControlState::Local);
    }
}
