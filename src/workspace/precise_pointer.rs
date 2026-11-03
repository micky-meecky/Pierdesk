//! Pointer routing over the applied v3 logical-boundary segments.

use std::io;
use std::str::FromStr as _;

use crate::cross_screen::{
    CompiledBoundaryLayout, CompiledLogicalBoundary, InputEvent, PhysicalPoint, PhysicalScreenSize,
    PortalPoint, ScreenBoundary,
};
use crate::security::CertificateFingerprint;

use super::DeviceId;
use super::control::{BoundaryMappingRef, Handoff};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrecisePointerCrossing {
    pub peer: DeviceId,
    pub parameter: u16,
    pub boundary: BoundaryMappingRef,
}

#[derive(Clone)]
struct NeighborBoundary {
    peer: DeviceId,
    boundary: CompiledLogicalBoundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrossingSelection {
    Unavailable,
    Armed,
    Selected,
}

pub struct PreciseWorkspacePointer {
    layout: CompiledBoundaryLayout,
    screen: PhysicalScreenSize,
    position: PhysicalPoint,
    local_baseline: bool,
    require_second_push: bool,
    armed_boundary: Option<String>,
    boundaries: Vec<NeighborBoundary>,
    pending: Option<PrecisePointerCrossing>,
}

impl PreciseWorkspacePointer {
    /// Builds a router for every confirmed local source portal.
    ///
    /// # Errors
    /// Rejects invalid screens, peer fingerprints, or an empty compiled layout.
    pub fn new(
        layout: CompiledBoundaryLayout,
        screen: PhysicalScreenSize,
        require_second_push: bool,
    ) -> io::Result<Self> {
        PhysicalScreenSize::new(screen.width, screen.height)?;
        let boundaries = layout
            .boundaries()
            .iter()
            .map(|boundary| {
                let peer = CertificateFingerprint::from_str(boundary.peer_fingerprint())?.bytes();
                Ok(NeighborBoundary {
                    peer,
                    boundary: boundary.clone(),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        // An empty layout is valid while every pair is switched off: the pointer then never crosses.
        Ok(Self {
            layout,
            screen,
            position: PhysicalPoint {
                x: screen.width / 2,
                y: screen.height / 2,
            },
            local_baseline: false,
            require_second_push,
            armed_boundary: None,
            boundaries,
            pending: None,
        })
    }

    #[must_use]
    pub fn crossing(&self) -> Option<&PrecisePointerCrossing> {
        self.pending.as_ref()
    }

    #[must_use]
    pub const fn position(&self) -> PhysicalPoint {
        self.position
    }

    /// Records the latest physical local cursor position and checks source portals.
    ///
    /// # Errors
    /// Rejects coordinates outside the current primary screen.
    pub fn local_position(
        &mut self,
        x: i32,
        y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        let point = self.point(x, y)?;
        if !self.local_baseline {
            self.position = point;
            self.local_baseline = true;
            self.armed_boundary = None;
            return Ok(());
        }
        let delta_x = x.saturating_sub(i32::try_from(self.position.x).map_err(io::Error::other)?);
        let delta_y = y.saturating_sub(i32::try_from(self.position.y).map_err(io::Error::other)?);
        self.position = point;
        self.observe(delta_x, delta_y, ready);
        Ok(())
    }

    /// Projects one raw packet from the last processed cursor position. This catches a fast
    /// crossing even when `WM_INPUT` arrives before Windows publishes the clamped edge position.
    /// Returns only the normal-axis distance beyond the source edge for post-handoff carry.
    pub fn local_projected(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> Option<(i32, i32)> {
        if !self.local_baseline || self.pending.is_some() {
            return None;
        }
        let horizontal_first = delta_x.unsigned_abs() >= delta_y.unsigned_abs();
        let edges = if horizontal_first {
            [
                ScreenBoundary::Left,
                ScreenBoundary::Right,
                ScreenBoundary::Top,
                ScreenBoundary::Bottom,
            ]
        } else {
            [
                ScreenBoundary::Top,
                ScreenBoundary::Bottom,
                ScreenBoundary::Left,
                ScreenBoundary::Right,
            ]
        };
        for edge in edges {
            let Some((point, overflow)) = projected_portal_point_for_motion(
                self.position,
                delta_x,
                delta_y,
                self.screen,
                edge,
            ) else {
                continue;
            };
            match self.select_crossing(point, ready) {
                CrossingSelection::Selected => return Some(overflow),
                CrossingSelection::Armed => return None,
                CrossingSelection::Unavailable => {}
            }
        }
        self.armed_boundary = None;
        None
    }

    /// Applies relative remote motion and observes any subsequent local source portal.
    ///
    /// # Errors
    /// Reports integer conversion failures for unsupported screen coordinates.
    pub fn remote_delta(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<InputEvent> {
        let x = i64::from(self.position.x)
            .saturating_add(i64::from(delta_x))
            .clamp(0, i64::from(self.screen.width - 1));
        let y = i64::from(self.position.y)
            .saturating_add(i64::from(delta_y))
            .clamp(0, i64::from(self.screen.height - 1));
        self.position = PhysicalPoint {
            x: u32::try_from(x).map_err(io::Error::other)?,
            y: u32::try_from(y).map_err(io::Error::other)?,
        };
        self.observe(delta_x, delta_y, ready);
        Ok(InputEvent::MouseMove {
            x: i32::try_from(x).map_err(io::Error::other)?,
            y: i32::try_from(y).map_err(io::Error::other)?,
        })
    }

    /// Maps a committed pair-scoped boundary parameter onto this screen.
    ///
    /// # Errors
    /// Rejects missing, stale, or peer-mismatched boundary identities.
    pub fn arrival(&mut self, handoff: &Handoff) -> io::Result<InputEvent> {
        let mapping = handoff
            .boundary
            .as_ref()
            .ok_or_else(|| invalid("precise pointer arrival is missing its boundary mapping"))?;
        let boundary = self
            .layout
            .boundary(&mapping.boundary_id)
            .filter(|boundary| {
                boundary.mapping_revision() == mapping.mapping_revision
                    && CertificateFingerprint::from_str(boundary.peer_fingerprint())
                        .is_ok_and(|peer| peer.bytes() == handoff.previous.current)
            })
            .ok_or_else(|| invalid("pointer arrival does not match a confirmed local boundary"))?;
        let parameter = if boundary.reversed() {
            u16::MAX - handoff.edge_offset
        } else {
            handoff.edge_offset
        };
        let mut point =
            portal_point_to_physical(boundary.source().point_at_parameter(parameter), self.screen);
        match boundary.source().point_at_parameter(parameter).edge {
            ScreenBoundary::Left => point.x = 1,
            ScreenBoundary::Right => point.x = self.screen.width - 2,
            ScreenBoundary::Top => point.y = 1,
            ScreenBoundary::Bottom => point.y = self.screen.height - 2,
        }
        self.position = point;
        self.local_baseline = true;
        self.reset();
        Ok(InputEvent::MouseMove {
            x: i32::try_from(point.x).map_err(io::Error::other)?,
            y: i32::try_from(point.y).map_err(io::Error::other)?,
        })
    }

    pub fn reset(&mut self) {
        self.pending = None;
        self.armed_boundary = None;
    }

    /// Every peer reachable through a confirmed local boundary.
    #[must_use]
    pub fn neighbor_peers(&self) -> Vec<DeviceId> {
        let mut peers = Vec::new();
        for neighbor in &self.boundaries {
            if !peers.contains(&neighbor.peer) {
                peers.push(neighbor.peer);
            }
        }
        peers
    }

    /// Peers whose boundary edge lies within a third of the screen of the local pointer. Used to
    /// warm an idle link shortly before a likely crossing (CF-CS-007).
    #[must_use]
    pub fn peers_near_boundaries(&self) -> Vec<DeviceId> {
        if !self.local_baseline {
            return Vec::new();
        }
        let mut peers = Vec::new();
        for neighbor in &self.boundaries {
            let edge = neighbor.boundary.source().point_at_parameter(0).edge;
            let (distance, extent) = match edge {
                ScreenBoundary::Top => (self.position.y, self.screen.height),
                ScreenBoundary::Bottom => (
                    self.screen.height.saturating_sub(1 + self.position.y),
                    self.screen.height,
                ),
                ScreenBoundary::Left => (self.position.x, self.screen.width),
                ScreenBoundary::Right => (
                    self.screen.width.saturating_sub(1 + self.position.x),
                    self.screen.width,
                ),
            };
            if distance <= extent / 3 && !peers.contains(&neighbor.peer) {
                peers.push(neighbor.peer);
            }
        }
        peers
    }

    pub fn rebase_local(&mut self) {
        self.local_baseline = false;
        self.reset();
    }

    fn point(&self, x: i32, y: i32) -> io::Result<PhysicalPoint> {
        let x = u32::try_from(x).map_err(io::Error::other)?;
        let y = u32::try_from(y).map_err(io::Error::other)?;
        if x >= self.screen.width || y >= self.screen.height {
            return Err(invalid("pointer is outside the primary screen"));
        }
        Ok(PhysicalPoint { x, y })
    }

    fn observe(&mut self, delta_x: i32, delta_y: i32, ready: &impl Fn(DeviceId) -> bool) {
        if self.pending.is_some() {
            return;
        }
        let horizontal_first = delta_x.unsigned_abs() >= delta_y.unsigned_abs();
        let edges = if horizontal_first {
            [
                ScreenBoundary::Left,
                ScreenBoundary::Right,
                ScreenBoundary::Top,
                ScreenBoundary::Bottom,
            ]
        } else {
            [
                ScreenBoundary::Top,
                ScreenBoundary::Bottom,
                ScreenBoundary::Left,
                ScreenBoundary::Right,
            ]
        };
        for edge in edges {
            let Some(point) =
                portal_point_for_motion(self.position, delta_x, delta_y, self.screen, edge)
            else {
                continue;
            };
            match self.select_crossing(point, ready) {
                CrossingSelection::Selected | CrossingSelection::Armed => return,
                CrossingSelection::Unavailable => {}
            }
        }
        self.armed_boundary = None;
    }

    fn select_crossing(
        &mut self,
        point: PortalPoint,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> CrossingSelection {
        for neighbor in &self.boundaries {
            if !ready(neighbor.peer) {
                continue;
            }
            let Some(transfer) = neighbor.boundary.source_to_target(point) else {
                continue;
            };
            let boundary_id = neighbor.boundary.boundary_id().to_owned();
            if self.require_second_push
                && self.armed_boundary.as_deref() != Some(boundary_id.as_str())
            {
                self.armed_boundary = Some(boundary_id);
                return CrossingSelection::Armed;
            }
            self.pending = Some(PrecisePointerCrossing {
                peer: neighbor.peer,
                parameter: transfer.parameter,
                boundary: BoundaryMappingRef {
                    boundary_id,
                    mapping_revision: neighbor.boundary.mapping_revision(),
                },
            });
            return CrossingSelection::Selected;
        }
        CrossingSelection::Unavailable
    }
}

fn projected_portal_point_for_motion(
    position: PhysicalPoint,
    delta_x: i32,
    delta_y: i32,
    screen: PhysicalScreenSize,
    edge: ScreenBoundary,
) -> Option<(PortalPoint, (i32, i32))> {
    let current_x = i32::try_from(position.x).ok()?;
    let current_y = i32::try_from(position.y).ok()?;
    let right = i32::try_from(screen.width - 1).ok()?;
    let bottom = i32::try_from(screen.height - 1).ok()?;
    let (distance, outward_delta, tangent, tangent_delta, tangent_extent) = match edge {
        ScreenBoundary::Left if delta_x < 0 => {
            (current_x, delta_x, current_y, delta_y, screen.height)
        }
        ScreenBoundary::Right if delta_x > 0 => (
            right.saturating_sub(current_x),
            delta_x,
            current_y,
            delta_y,
            screen.height,
        ),
        ScreenBoundary::Top if delta_y < 0 => {
            (current_y, delta_y, current_x, delta_x, screen.width)
        }
        ScreenBoundary::Bottom if delta_y > 0 => (
            bottom.saturating_sub(current_y),
            delta_y,
            current_x,
            delta_x,
            screen.width,
        ),
        _ => return None,
    };
    let magnitude = i64::from(outward_delta).abs();
    if magnitude < i64::from(distance) || magnitude == 0 {
        return None;
    }
    let tangent_at_crossing = i64::from(tangent).saturating_add(
        i64::from(tangent_delta)
            .saturating_mul(i64::from(distance))
            .checked_div(magnitude)
            .unwrap_or_default(),
    );
    let tangent_limit = i64::from(tangent_extent.saturating_sub(1));
    let coordinate = u32::try_from(tangent_at_crossing.clamp(0, tangent_limit)).ok()?;
    let overflow = match edge {
        ScreenBoundary::Left => (delta_x.saturating_add(distance), 0),
        ScreenBoundary::Right => (delta_x.saturating_sub(distance), 0),
        ScreenBoundary::Top => (0, delta_y.saturating_add(distance)),
        ScreenBoundary::Bottom => (0, delta_y.saturating_sub(distance)),
    };
    Some((
        PortalPoint {
            edge,
            normalized_offset: normalize(coordinate, tangent_extent),
        },
        overflow,
    ))
}

fn portal_point_for_motion(
    position: PhysicalPoint,
    delta_x: i32,
    delta_y: i32,
    screen: PhysicalScreenSize,
    edge: ScreenBoundary,
) -> Option<PortalPoint> {
    let on_edge = match edge {
        ScreenBoundary::Left => position.x == 0,
        ScreenBoundary::Right => position.x == screen.width - 1,
        ScreenBoundary::Top => position.y == 0,
        ScreenBoundary::Bottom => position.y == screen.height - 1,
    };
    let outward = match edge {
        ScreenBoundary::Left => delta_x < 0,
        ScreenBoundary::Right => delta_x > 0,
        ScreenBoundary::Top => delta_y < 0,
        ScreenBoundary::Bottom => delta_y > 0,
    };
    if !on_edge || !outward {
        return None;
    }
    let (coordinate, extent) = match edge {
        ScreenBoundary::Left | ScreenBoundary::Right => (position.y, screen.height),
        ScreenBoundary::Top | ScreenBoundary::Bottom => (position.x, screen.width),
    };
    Some(PortalPoint {
        edge,
        normalized_offset: normalize(coordinate, extent),
    })
}

fn normalize(coordinate: u32, extent: u32) -> u32 {
    if extent <= 1 {
        return 0;
    }
    u32::try_from(u64::from(coordinate) * u64::from(u16::MAX) / u64::from(extent.saturating_sub(1)))
        .unwrap_or(u32::from(u16::MAX))
}

fn portal_point_to_physical(point: PortalPoint, screen: PhysicalScreenSize) -> PhysicalPoint {
    let coordinate = |normalized: u32, extent: u32| {
        u32::try_from(
            u64::from(normalized.min(u32::from(u16::MAX))) * u64::from(extent.saturating_sub(1))
                / u64::from(u16::MAX),
        )
        .unwrap_or_default()
    };
    match point.edge {
        ScreenBoundary::Left => PhysicalPoint {
            x: 0,
            y: coordinate(point.normalized_offset, screen.height),
        },
        ScreenBoundary::Right => PhysicalPoint {
            x: screen.width - 1,
            y: coordinate(point.normalized_offset, screen.height),
        },
        ScreenBoundary::Top => PhysicalPoint {
            x: coordinate(point.normalized_offset, screen.width),
            y: 0,
        },
        ScreenBoundary::Bottom => PhysicalPoint {
            x: coordinate(point.normalized_offset, screen.width),
            y: screen.height - 1,
        },
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::{
        BoundaryArcSegment, BoundaryLayoutV3, BoundaryPortal, BoundaryScreen,
        LOGICAL_BOUNDARY_SCHEMA_VERSION, LogicalBoundary,
    };

    fn fingerprint(value: u8) -> String {
        CertificateFingerprint::from_bytes([value; 32]).to_string()
    }

    fn compiled() -> CompiledBoundaryLayout {
        BoundaryLayoutV3 {
            schema_version: LOGICAL_BOUNDARY_SCHEMA_VERSION,
            revision: 4,
            local_fingerprint: fingerprint(1),
            screens: vec![
                BoundaryScreen {
                    device_fingerprint: fingerprint(1),
                    pixel_width: 1000,
                    pixel_height: 800,
                    physical_width_millimeters: 300,
                    physical_height_millimeters: 200,
                    rotation_quarter_turns: 0,
                    canvas_x_millimeters: 0,
                    canvas_y_millimeters: 0,
                },
                BoundaryScreen {
                    device_fingerprint: fingerprint(2),
                    pixel_width: 600,
                    pixel_height: 500,
                    physical_width_millimeters: 240,
                    physical_height_millimeters: 180,
                    rotation_quarter_turns: 0,
                    canvas_x_millimeters: 300,
                    canvas_y_millimeters: 0,
                },
                BoundaryScreen {
                    device_fingerprint: fingerprint(3),
                    pixel_width: 700,
                    pixel_height: 450,
                    physical_width_millimeters: 260,
                    physical_height_millimeters: 170,
                    rotation_quarter_turns: 0,
                    canvas_x_millimeters: 0,
                    canvas_y_millimeters: -170,
                },
            ],
            boundaries: vec![
                LogicalBoundary {
                    id: "right-peer".into(),
                    peer_fingerprint: fingerprint(2),
                    mapping_revision: 7,
                    local_confirmed_revision: 7,
                    peer_confirmed_revision: 7,
                    reversed: false,
                    source: BoundaryPortal {
                        segments: vec![BoundaryArcSegment {
                            edge: ScreenBoundary::Right,
                            start: 0,
                            end: 65_536,
                        }],
                    },
                    target: BoundaryPortal {
                        segments: vec![BoundaryArcSegment {
                            edge: ScreenBoundary::Left,
                            start: 0,
                            end: 65_536,
                        }],
                    },
                },
                LogicalBoundary {
                    id: "top-peer".into(),
                    peer_fingerprint: fingerprint(3),
                    mapping_revision: 11,
                    local_confirmed_revision: 11,
                    peer_confirmed_revision: 11,
                    reversed: false,
                    source: BoundaryPortal {
                        segments: vec![BoundaryArcSegment {
                            edge: ScreenBoundary::Top,
                            start: 0,
                            end: 65_536,
                        }],
                    },
                    target: BoundaryPortal {
                        segments: vec![BoundaryArcSegment {
                            edge: ScreenBoundary::Bottom,
                            start: 0,
                            end: 65_536,
                        }],
                    },
                },
            ],
        }
        .compile(0, 1)
        .unwrap()
    }

    #[test]
    fn exact_boundary_routes_to_its_peer() {
        let mut pointer = PreciseWorkspacePointer::new(
            compiled(),
            PhysicalScreenSize::new(1000, 800).unwrap(),
            false,
        )
        .unwrap();
        pointer.local_position(999, 400, &|_| true).unwrap();
        pointer.local_projected(10, 0, &|_| true);
        let crossing = pointer.crossing().unwrap();
        assert_eq!(crossing.peer, [2; 32]);
        assert_eq!(crossing.boundary.boundary_id, "right-peer");
        assert_eq!(crossing.boundary.mapping_revision, 7);
    }

    #[test]
    fn top_and_right_boundaries_select_different_peers() {
        let screen = PhysicalScreenSize::new(1000, 800).unwrap();
        let mut right = PreciseWorkspacePointer::new(compiled(), screen, false).unwrap();
        right.local_position(999, 400, &|_| true).unwrap();
        right.local_projected(4, 0, &|_| true);
        assert_eq!(right.crossing().unwrap().peer, [2; 32]);

        let mut top = PreciseWorkspacePointer::new(compiled(), screen, false).unwrap();
        top.local_position(400, 0, &|_| true).unwrap();
        top.local_projected(0, -4, &|_| true);
        assert_eq!(top.crossing().unwrap().peer, [3; 32]);
        assert_eq!(top.crossing().unwrap().boundary.boundary_id, "top-peer");
    }

    #[test]
    fn fast_raw_packet_crosses_from_before_the_edge_and_carries_only_overflow() {
        let screen = PhysicalScreenSize::new(1000, 800).unwrap();
        let mut pointer = PreciseWorkspacePointer::new(compiled(), screen, false).unwrap();
        pointer.local_position(940, 400, &|_| true).unwrap();

        let carry = pointer.local_projected(100, 20, &|_| true);

        assert_eq!(carry, Some((41, 0)));
        let crossing = pointer.crossing().unwrap();
        assert_eq!(crossing.peer, [2; 32]);
        assert_eq!(crossing.boundary.boundary_id, "right-peer");
        assert!(crossing.parameter > u16::MAX / 2);
    }

    #[test]
    fn raw_packet_that_does_not_reach_an_edge_stays_local() {
        let screen = PhysicalScreenSize::new(1000, 800).unwrap();
        let mut pointer = PreciseWorkspacePointer::new(compiled(), screen, false).unwrap();
        pointer.local_position(940, 400, &|_| true).unwrap();

        assert_eq!(pointer.local_projected(50, 0, &|_| true), None);
        assert!(pointer.crossing().is_none());
    }
}
