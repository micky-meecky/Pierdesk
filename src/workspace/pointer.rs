//! Four local screen boundaries and target-physical pointer projection.
//! Workspace mouse packets carry relative pixel deltas; arrival positions use
//! a normalized edge offset so monitors with different sizes remain aligned.
use std::collections::BTreeMap;
use std::io;

use crate::cross_screen::native_input::NativeInputRuntime;
use crate::cross_screen::{
    BoundaryDecision, BoundaryPolicy, InputEvent, PeerPlacement, PhysicalPoint, PhysicalScreenSize,
    PointerSample, ScreenBoundary, StrictBoundaryEngine, map_normalized_boundary_position,
};

use super::control::Handoff;
use super::persistence::LocalBoundarySettings;
use super::{DeviceId, WorkspaceLayout};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerCrossing {
    pub peer: DeviceId,
    pub direction: PeerPlacement,
    pub edge_offset: u16,
}

struct Neighbor {
    direction: PeerPlacement,
    boundary: StrictBoundaryEngine,
}

pub struct WorkspacePointer {
    local: DeviceId,
    layout: WorkspaceLayout,
    screen: PhysicalScreenSize,
    position: PhysicalPoint,
    local_baseline: bool,
    neighbors: BTreeMap<DeviceId, Neighbor>,
    pending: Option<PointerCrossing>,
}

impl WorkspacePointer {
    /// Build all local outgoing edges from one complete global layout.
    /// An unplaced member has no crossing edges and remains locally usable.
    ///
    /// # Errors
    /// Rejects invalid physical screen sizes or local boundary preferences.
    pub fn new(
        local: DeviceId,
        layout: WorkspaceLayout,
        screen: PhysicalScreenSize,
        preferences: &LocalBoundarySettings,
    ) -> io::Result<Self> {
        PhysicalScreenSize::new(screen.width, screen.height)?;
        if preferences.schema != 2 || preferences.corner_guard_pixels > 512 {
            return Err(invalid("invalid local pointer preferences"));
        }
        let guard = if preferences.corner_guard_enabled {
            preferences
                .corner_guard_pixels
                .min((screen.width.min(screen.height) - 2) / 2)
        } else {
            0
        };
        let mut neighbors = BTreeMap::new();
        for peer in layout.positions().keys() {
            if !preferences.enabled {
                break;
            }
            if let Some(direction) = layout.direction(local, *peer) {
                neighbors.insert(
                    *peer,
                    Neighbor {
                        direction,
                        boundary: StrictBoundaryEngine::new(
                            BoundaryPolicy {
                                peer_placement: direction,
                                corner_guard_pixels: guard,
                                require_second_push: preferences.require_second_push,
                            },
                            screen,
                            screen,
                        )?,
                    },
                );
            }
        }
        Ok(Self {
            local,
            layout,
            screen,
            position: PhysicalPoint {
                x: screen.width / 2,
                y: screen.height / 2,
            },
            local_baseline: false,
            neighbors,
            pending: None,
        })
    }

    #[must_use]
    pub const fn position(&self) -> PhysicalPoint {
        self.position
    }

    #[must_use]
    pub const fn crossing(&self) -> Option<PointerCrossing> {
        self.pending
    }

    /// Observe the actual local cursor position. Raw input is not added here.
    ///
    /// # Errors
    /// Rejects points outside the physical primary screen.
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
            // A fresh interior sample rearms boundaries after an explicit return.
            // Zero motion cannot itself request a crossing at the edge.
            return self.observe(0, 0, ready);
        }
        let delta_x = i32::try_from(point.x).map_err(io::Error::other)?
            - i32::try_from(self.position.x).map_err(io::Error::other)?;
        let delta_y = i32::try_from(point.y).map_err(io::Error::other)?
            - i32::try_from(self.position.y).map_err(io::Error::other)?;
        self.position = point;
        self.observe(delta_x, delta_y, ready)
    }

    /// Supply raw outward intent at a Windows-clamped edge without adding that
    /// motion to the already processed local cursor position a second time.
    ///
    /// # Errors
    /// Propagates boundary validation failure.
    pub fn local_raw(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        if !self.local_baseline {
            return Ok(());
        }
        self.observe(delta_x, delta_y, ready)
    }

    /// Convert an authenticated relative packet to the target's physical position.
    ///
    /// # Errors
    /// Propagates boundary validation failure.
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
        self.observe(delta_x, delta_y, ready)?;
        Ok(InputEvent::MouseMove {
            x: i32::try_from(x).map_err(io::Error::other)?,
            y: i32::try_from(y).map_err(io::Error::other)?,
        })
    }

    /// Map a committed arrival before any inherited buttons can begin a drag.
    ///
    /// # Errors
    /// Rejects another target, a changed layout or non-adjacent departure.
    pub fn arrival(&mut self, handoff: &Handoff) -> io::Result<InputEvent> {
        if handoff.next.current != self.local
            || handoff.next.layout_digest != self.layout.digest()
            || handoff.next.layout_revision != self.layout.revision()
        {
            return Err(invalid("pointer arrival does not match this target layout"));
        }
        let edge = self
            .layout
            .direction(self.local, handoff.previous.current)
            .ok_or_else(|| invalid("pointer arrival is not from an adjacent screen"))?
            .local_exit_boundary();
        let mut point = map_normalized_boundary_position(handoff.edge_offset, edge, self.screen, 0);
        // Inset only the normal axis; the orthogonal normalized coordinate stays
        // intact, including at the ends of an edge with corner protection disabled.
        match edge {
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

    /// Adapt the control host's relative packets to the existing native injector.
    ///
    /// # Errors
    /// Propagates geometry, cancellation and Win32 injection failures.
    pub fn inject_native(
        &mut self,
        native: &NativeInputRuntime,
        events: &[InputEvent],
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        let mut physical = Vec::with_capacity(events.len());
        for event in events {
            physical.push(match event {
                InputEvent::MouseMove { x, y } => self.remote_delta(*x, *y, ready)?,
                _ => *event,
            });
        }
        native.inject(&physical)
    }

    /// Restore source confinement before placing a returning origin's pointer.
    ///
    /// # Errors
    /// Propagates native cursor restoration and injection errors.
    pub fn enter_native(
        &mut self,
        native: &NativeInputRuntime,
        handoff: &Handoff,
    ) -> io::Result<()> {
        let position = self.arrival(handoff)?;
        native.set_forwarding(false)?;
        native.inject(&[position])
    }

    pub fn reset(&mut self) {
        self.pending = None;
        for neighbor in self.neighbors.values_mut() {
            neighbor.boundary.force_local();
        }
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

    fn observe(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        ready: &impl Fn(DeviceId) -> bool,
    ) -> io::Result<()> {
        if self.pending.is_some() {
            return Ok(());
        }
        let horizontal_first = delta_x.unsigned_abs() >= delta_y.unsigned_abs();
        let mut peers: Vec<_> = self.neighbors.keys().copied().collect();
        peers.sort_by_key(|peer| {
            let horizontal = matches!(
                self.neighbors[peer].direction,
                PeerPlacement::Left | PeerPlacement::Right
            );
            (horizontal != horizontal_first, *peer)
        });
        for peer in peers {
            let neighbor = self
                .neighbors
                .get_mut(&peer)
                .expect("collected neighbor exists");
            if !ready(peer) {
                neighbor.boundary.force_local();
                continue;
            }
            if let BoundaryDecision::TransitionRequested(transition) =
                neighbor.boundary.observe(PointerSample {
                    position: self.position,
                    delta_x,
                    delta_y,
                })?
            {
                self.pending = Some(PointerCrossing {
                    peer,
                    direction: neighbor.direction,
                    edge_offset: transition.normalized_edge_offset,
                });
                break;
            }
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::{KeyTransition, MouseButton};
    use crate::workspace::control::CrossScreenCoordinator;
    use crate::workspace::{GridPoint, LayoutRevision, edge};
    use std::collections::BTreeSet;
    use std::time::Instant;

    fn id(value: u8) -> DeviceId {
        [value; 32]
    }

    fn fixture() -> (
        WorkspaceLayout,
        BTreeSet<DeviceId>,
        BTreeSet<super::super::Edge>,
        LocalBoundarySettings,
    ) {
        let members = (1..=5).map(id).collect();
        let edges = (2..=5).map(|peer| edge(id(1), id(peer))).collect();
        let layout = WorkspaceLayout::new(
            [9; 16],
            LayoutRevision {
                counter: 1,
                writer: id(1),
            },
            &BTreeMap::from([
                (id(1), GridPoint { x: 0, y: 0 }),
                (id(2), GridPoint { x: -1, y: 0 }),
                (id(3), GridPoint { x: 1, y: 0 }),
                (id(4), GridPoint { x: 0, y: -1 }),
                (id(5), GridPoint { x: 0, y: 1 }),
            ]),
            &members,
            &edges,
        )
        .unwrap();
        (
            layout,
            members,
            edges,
            LocalBoundarySettings {
                schema: 2,
                enabled: true,
                corner_guard_enabled: false,
                corner_guard_pixels: 32,
                require_second_push: false,
                pause_on_fullscreen: false,
            },
        )
    }

    #[test]
    fn every_neighbor_maps_the_same_edge_fraction_onto_a_different_physical_screen() {
        let (layout, members, edges, preferences) = fixture();
        for (peer, x, y, direction) in [
            (2, 0, 270, PeerPlacement::Left),
            (3, 1919, 270, PeerPlacement::Right),
            (4, 480, 0, PeerPlacement::Above),
            (5, 480, 1079, PeerPlacement::Below),
        ] {
            let mut source = WorkspacePointer::new(
                id(1),
                layout.clone(),
                PhysicalScreenSize::new(1920, 1080).unwrap(),
                &preferences,
            )
            .unwrap();
            source.local_position(960, 540, &|_| true).unwrap();
            source.local_position(x, y, &|_| true).unwrap();
            let crossing = source.crossing().unwrap();
            assert_eq!(crossing.peer, id(peer));
            assert_eq!(crossing.direction, direction);
            let mut control =
                CrossScreenCoordinator::new(id(1), layout.clone(), &members, edges.clone())
                    .unwrap();
            let handoff = control
                .begin_handoff_at(id(peer), 1, vec![], crossing.edge_offset, Instant::now())
                .unwrap();
            let mut target = WorkspacePointer::new(
                id(peer),
                layout.clone(),
                PhysicalScreenSize::new(2560, 1440).unwrap(),
                &preferences,
            )
            .unwrap();
            let InputEvent::MouseMove { x, y } = target.arrival(&handoff).unwrap() else {
                panic!("arrival must be a pointer event")
            };
            match direction {
                PeerPlacement::Left => {
                    assert_eq!(x, 2558);
                    assert!((y - 360).abs() <= 1);
                }
                PeerPlacement::Right => {
                    assert_eq!(x, 1);
                    assert!((y - 360).abs() <= 1);
                }
                PeerPlacement::Above => {
                    assert_eq!(y, 1438);
                    assert!((x - 640).abs() <= 1);
                }
                PeerPlacement::Below => {
                    assert_eq!(y, 1);
                    assert!((x - 640).abs() <= 1);
                }
            }
            assert!(target.crossing().is_none());
        }
    }

    #[test]
    fn local_raw_intent_does_not_double_apply_processed_motion_and_requires_a_baseline() {
        let (layout, _, _, preferences) = fixture();
        let mut pointer = WorkspacePointer::new(
            id(1),
            layout,
            PhysicalScreenSize::new(1920, 1080).unwrap(),
            &preferences,
        )
        .unwrap();
        pointer.local_position(1919, 500, &|_| true).unwrap();
        assert!(pointer.crossing().is_none());
        pointer.local_raw(4, 0, &|_| true).unwrap();
        assert_eq!(pointer.crossing().unwrap().peer, id(3));
        pointer.rebase_local();
        pointer.local_position(1000, 500, &|_| true).unwrap();
        pointer.local_position(1004, 500, &|_| true).unwrap();
        pointer.local_raw(4, 0, &|_| true).unwrap();
        assert_eq!(pointer.position(), PhysicalPoint { x: 1004, y: 500 });
    }

    #[test]
    fn disabled_edges_corner_guard_and_second_push_gate_crossings() {
        let (layout, _, _, mut preferences) = fixture();
        preferences.corner_guard_enabled = true;
        preferences.require_second_push = true;
        let mut pointer = WorkspacePointer::new(
            id(1),
            layout,
            PhysicalScreenSize::new(1920, 1080).unwrap(),
            &preferences,
        )
        .unwrap();
        pointer.local_position(1919, 2, &|_| true).unwrap();
        pointer.local_raw(10, 0, &|_| true).unwrap();
        assert!(pointer.crossing().is_none());
        pointer.local_position(1919, 500, &|_| false).unwrap();
        pointer.local_raw(10, 0, &|_| false).unwrap();
        assert!(pointer.crossing().is_none());
        pointer.local_raw(10, 0, &|_| true).unwrap();
        assert!(pointer.crossing().is_none());
        pointer.local_raw(10, 0, &|_| true).unwrap();
        assert_eq!(pointer.crossing().unwrap().peer, id(3));
    }

    #[test]
    fn disabled_local_setting_prevents_crossing_even_when_links_are_ready() {
        let (layout, _, _, mut preferences) = fixture();
        preferences.enabled = false;
        let mut pointer = WorkspacePointer::new(
            id(1),
            layout,
            PhysicalScreenSize::new(1920, 1080).unwrap(),
            &preferences,
        )
        .unwrap();
        pointer.local_position(1919, 500, &|_| true).unwrap();
        for _ in 0..4 {
            pointer.local_raw(10, 0, &|_| true).unwrap();
        }
        assert!(pointer.crossing().is_none());
        pointer.remote_delta(10000, 0, &|_| true).unwrap();
        assert!(pointer.crossing().is_none());
    }

    #[test]
    fn target_motion_uses_its_own_physical_pixels_and_clamps_overflow_safely() {
        let (layout, _, _, preferences) = fixture();
        let mut pointer = WorkspacePointer::new(
            id(1),
            layout,
            PhysicalScreenSize::new(3840, 2160).unwrap(),
            &preferences,
        )
        .unwrap();
        assert_eq!(
            pointer.remote_delta(10, -20, &|_| true).unwrap(),
            InputEvent::MouseMove { x: 1930, y: 1060 }
        );
        assert_eq!(
            pointer
                .remote_delta(i32::MAX, i32::MIN, &|_| false)
                .unwrap(),
            InputEvent::MouseMove { x: 3839, y: 0 }
        );
        assert!(pointer.crossing().is_none());
    }

    #[test]
    fn committed_arrival_is_before_held_buttons_and_offset_tampering_never_enters() {
        let (layout, members, edges, _) = fixture();
        let mut origin =
            CrossScreenCoordinator::new(id(1), layout.clone(), &members, edges.clone()).unwrap();
        let mut target = CrossScreenCoordinator::new(id(3), layout, &members, edges).unwrap();
        let now = Instant::now();
        let handoff = origin
            .begin_handoff_at(
                id(3),
                1,
                vec![InputEvent::MouseButton {
                    button: MouseButton::Left,
                    transition: KeyTransition::Down,
                }],
                10000,
                now,
            )
            .unwrap();
        let ack = target.prepare(id(1), 1, handoff, now).unwrap();
        let mut bad = ack.clone();
        bad.edge_offset += 1;
        assert!(
            origin
                .acknowledge(id(3), 1, &bad, now, &mut |_| Ok(()))
                .is_err()
        );
        let commit = origin
            .acknowledge(id(3), 1, &ack, now, &mut |_| Ok(()))
            .unwrap();
        let order = std::cell::RefCell::new(vec![]);
        target
            .commit_with_entry(
                id(1),
                1,
                &commit,
                now,
                &mut |_| {
                    order.borrow_mut().push("held");
                    Ok(())
                },
                &mut |handoff| {
                    assert_eq!(handoff.edge_offset, 10000);
                    order.borrow_mut().push("arrival");
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(*order.borrow(), vec!["arrival", "held"]);
    }
}
