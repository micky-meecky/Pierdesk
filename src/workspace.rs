//! Shared integer geometry. Network links are supplied independently of screen edges.
use crate::cross_screen::PeerPlacement;

pub mod control;
pub mod control_host;
pub mod editor;
pub mod graph;
pub mod input_runtime;
pub mod persistence;
pub mod pointer;
mod pointer_gain;
pub mod precise_pointer;
pub mod precise_product;
pub mod product;
pub mod sync;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub type DeviceId = [u8; 32];
pub type Edge = (DeviceId, DeviceId);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridPoint {
    pub x: i8,
    pub y: i8,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutRevision {
    pub counter: u64,
    pub writer: DeviceId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    MemberCount,
    CoordinateBounds,
    Overlap,
    UnknownDevice,
    MissingTrustedEdge,
    Disconnected,
    InvalidIdentity,
    InvalidRevision,
    InvalidSchema,
    InvalidEncoding,
    DuplicateDevice,
    DigestMismatch,
    WorkspaceMismatch,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid workspace layout: {self:?}")
    }
}

impl std::error::Error for LayoutError {}

pub const MAX_LAYOUT_BYTES: usize = 16 * 1024;

/// Immutable validated snapshot. Decode never silently drops duplicate device records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceLayout {
    workspace_id: [u8; 16],
    revision: LayoutRevision,
    positions: BTreeMap<DeviceId, GridPoint>,
    digest: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutWire {
    schema: u32,
    workspace_id: [u8; 16],
    revision: LayoutRevision,
    positions: Vec<(DeviceId, GridPoint)>,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotOrder {
    Older,
    Equal,
    Newer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlacementChange {
    Move,
    Swap { displaced: DeviceId },
    Replace { removed: DeviceId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutEdit {
    pub candidate: WorkspaceLayout,
    pub change: PlacementChange,
    pub removed_edges: BTreeSet<Edge>,
    pub added_edges: BTreeSet<Edge>,
    /// Existing links whose direction changed also need a fresh handshake.
    pub redirected_edges: BTreeSet<Edge>,
}

impl WorkspaceLayout {
    /// Builds a canonical snapshot using independently authenticated connection edges.
    ///
    /// # Errors
    /// Rejects invalid identity, revision, membership or screen geometry.
    pub fn new(
        workspace_id: [u8; 16],
        revision: LayoutRevision,
        positions: &BTreeMap<DeviceId, GridPoint>,
        members: &BTreeSet<DeviceId>,
        trusted_edges: &BTreeSet<Edge>,
    ) -> Result<Self, LayoutError> {
        if workspace_id == [0; 16] || members.contains(&[0; 32]) {
            return Err(LayoutError::InvalidIdentity);
        }
        if revision.counter == u64::MAX || !members.contains(&revision.writer) {
            return Err(LayoutError::InvalidRevision);
        }
        let positions = normalize(positions, members, trusted_edges)?;
        let mut hash = Sha256::new();
        hash.update(b"ClipFerryWorkspaceLayoutV1\0");
        hash.update(workspace_id);
        hash.update(revision.counter.to_be_bytes());
        hash.update(revision.writer);
        for (id, point) in &positions {
            hash.update(id);
            hash.update(point.x.to_be_bytes());
            hash.update(point.y.to_be_bytes());
        }
        Ok(Self {
            workspace_id,
            revision,
            positions,
            digest: hash.finalize().into(),
        })
    }

    #[must_use]
    pub const fn workspace_id(&self) -> [u8; 16] {
        self.workspace_id
    }

    #[must_use]
    pub const fn revision(&self) -> LayoutRevision {
        self.revision
    }

    #[must_use]
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    #[must_use]
    pub fn positions(&self) -> &BTreeMap<DeviceId, GridPoint> {
        &self.positions
    }

    /// Compare whole snapshots; equal revisions with differing contents are a fault.
    ///
    /// # Errors
    /// Rejects foreign workspaces and conflicting digests at the same revision.
    pub fn compare(&self, incoming: &Self) -> Result<SnapshotOrder, LayoutError> {
        if self.workspace_id != incoming.workspace_id {
            return Err(LayoutError::WorkspaceMismatch);
        }
        match incoming.revision.cmp(&self.revision) {
            std::cmp::Ordering::Less => Ok(SnapshotOrder::Older),
            std::cmp::Ordering::Greater => Ok(SnapshotOrder::Newer),
            std::cmp::Ordering::Equal if incoming.digest == self.digest => Ok(SnapshotOrder::Equal),
            std::cmp::Ordering::Equal => Err(LayoutError::DigestMismatch),
        }
    }

    /// Serialize a complete, canonical snapshot.
    ///
    /// # Errors
    /// Returns an encoding error if serialization fails.
    pub fn encode(&self) -> Result<Vec<u8>, LayoutError> {
        serde_json::to_vec(&LayoutWire {
            schema: 1,
            workspace_id: self.workspace_id,
            revision: self.revision,
            positions: self
                .positions
                .iter()
                .map(|(id, point)| (*id, *point))
                .collect(),
            digest: self.digest,
        })
        .map_err(|_| LayoutError::InvalidEncoding)
    }

    /// Decode bounded untrusted input, checking every field and the canonical digest.
    ///
    /// # Errors
    /// Rejects oversized, malformed, duplicate, untrusted or inconsistent snapshots.
    pub fn decode(
        bytes: &[u8],
        members: &BTreeSet<DeviceId>,
        trusted_edges: &BTreeSet<Edge>,
    ) -> Result<Self, LayoutError> {
        if bytes.is_empty() || bytes.len() > MAX_LAYOUT_BYTES {
            return Err(LayoutError::InvalidEncoding);
        }
        let wire: LayoutWire =
            serde_json::from_slice(bytes).map_err(|_| LayoutError::InvalidEncoding)?;
        if wire.schema != 1 {
            return Err(LayoutError::InvalidSchema);
        }
        if wire.positions.len() > 5 {
            return Err(LayoutError::MemberCount);
        }
        let mut positions = BTreeMap::new();
        for (id, point) in wire.positions {
            if positions.insert(id, point).is_some() {
                return Err(LayoutError::DuplicateDevice);
            }
        }
        let layout = Self::new(
            wire.workspace_id,
            wire.revision,
            &positions,
            members,
            trusted_edges,
        )?;
        if layout.digest != wire.digest {
            return Err(LayoutError::DigestMismatch);
        }
        Ok(layout)
    }

    /// Prepare a move/swap/replacement without changing this snapshot.
    ///
    /// # Errors
    /// Rejects unknown devices, exhausted revisions and any invalid resulting geometry.
    pub fn place(
        &self,
        device: DeviceId,
        target: GridPoint,
        writer: DeviceId,
        members: &BTreeSet<DeviceId>,
        trusted_edges: &BTreeSet<Edge>,
    ) -> Result<LayoutEdit, LayoutError> {
        if !members.contains(&device) {
            return Err(LayoutError::UnknownDevice);
        }
        let mut positions = self.positions.clone();
        let old = positions.get(&device).copied();
        let occupant = positions
            .iter()
            .find_map(|(id, p)| (*id != device && *p == target).then_some(*id));
        let change = match (occupant, old) {
            (Some(displaced), Some(previous)) => {
                positions.insert(displaced, previous);
                PlacementChange::Swap { displaced }
            }
            (Some(removed), None) => {
                positions.remove(&removed);
                PlacementChange::Replace { removed }
            }
            (None, _) => PlacementChange::Move,
        };
        positions.insert(device, target);
        let revision = LayoutRevision {
            counter: self
                .revision
                .counter
                .checked_add(1)
                .ok_or(LayoutError::InvalidRevision)?,
            writer,
        };
        let candidate = Self::new(
            self.workspace_id,
            revision,
            &positions,
            members,
            trusted_edges,
        )?;
        let before = screen_edges(&self.positions);
        let after = screen_edges(&candidate.positions);
        let redirected_edges = before
            .intersection(&after)
            .filter(|(a, b)| self.direction(*a, *b) != candidate.direction(*a, *b))
            .copied()
            .collect();
        Ok(LayoutEdit {
            candidate,
            change,
            removed_edges: before.difference(&after).copied().collect(),
            added_edges: after.difference(&before).copied().collect(),
            redirected_edges,
        })
    }

    #[must_use]
    pub fn local_view(&self, local: DeviceId) -> Option<BTreeMap<DeviceId, (i16, i16)>> {
        let origin = self.positions.get(&local)?;
        Some(
            self.positions
                .iter()
                .map(|(id, p)| {
                    (
                        *id,
                        (
                            i16::from(p.x) - i16::from(origin.x),
                            i16::from(p.y) - i16::from(origin.y),
                        ),
                    )
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn direction(&self, from: DeviceId, to: DeviceId) -> Option<PeerPlacement> {
        let a = self.positions.get(&from)?;
        let b = self.positions.get(&to)?;
        match (
            i16::from(b.x) - i16::from(a.x),
            i16::from(b.y) - i16::from(a.y),
        ) {
            (1, 0) => Some(PeerPlacement::Right),
            (-1, 0) => Some(PeerPlacement::Left),
            (0, 1) => Some(PeerPlacement::Below),
            (0, -1) => Some(PeerPlacement::Above),
            _ => None,
        }
    }
}

/// Canonical endpoint order also determines the unique dialer (the first endpoint).
#[must_use]
pub fn edge(a: DeviceId, b: DeviceId) -> Edge {
    if a < b { (a, b) } else { (b, a) }
}

#[must_use]
pub fn edge_id(a: DeviceId, b: DeviceId) -> [u8; 32] {
    let (a, b) = edge(a, b);
    let mut hash = Sha256::new();
    hash.update(b"ClipFerryPeerEdgeV1\0");
    hash.update(a);
    hash.update(b);
    hash.finalize().into()
}

/// Derive geometry without interpreting additional network links as screen edges.
#[must_use]
pub fn screen_edges(positions: &BTreeMap<DeviceId, GridPoint>) -> BTreeSet<Edge> {
    let mut edges = BTreeSet::new();
    for (a, p) in positions {
        for (b, q) in positions.range(*a..) {
            if a != b
                && (i16::from(p.x) - i16::from(q.x)).abs() + (i16::from(p.y) - i16::from(q.y)).abs()
                    == 1
            {
                edges.insert((*a, *b));
            }
        }
    }
    edges
}

/// Normalize to the smallest fingerprint and validate the complete candidate.
///
/// # Errors
/// Rejects excess members, invalid coordinates, overlap, unknown identities,
/// untrusted screen edges and disconnected geometry.
pub fn normalize(
    positions: &BTreeMap<DeviceId, GridPoint>,
    members: &BTreeSet<DeviceId>,
    trusted_edges: &BTreeSet<Edge>,
) -> Result<BTreeMap<DeviceId, GridPoint>, LayoutError> {
    if positions.is_empty() || positions.len() > 5 || members.len() > 5 {
        return Err(LayoutError::MemberCount);
    }
    let anchor = positions.values().next().ok_or(LayoutError::MemberCount)?;
    let mut result = BTreeMap::new();
    let mut occupied = BTreeSet::new();
    for (id, point) in positions {
        if !(-8..=8).contains(&point.x) || !(-8..=8).contains(&point.y) {
            return Err(LayoutError::CoordinateBounds);
        }
        if !members.contains(id) {
            return Err(LayoutError::UnknownDevice);
        }
        let x = i16::from(point.x) - i16::from(anchor.x);
        let y = i16::from(point.y) - i16::from(anchor.y);
        if !(-8..=8).contains(&x) || !(-8..=8).contains(&y) {
            return Err(LayoutError::CoordinateBounds);
        }
        let point = GridPoint {
            x: i8::try_from(x).map_err(|_| LayoutError::CoordinateBounds)?,
            y: i8::try_from(y).map_err(|_| LayoutError::CoordinateBounds)?,
        };
        if !occupied.insert(point) {
            return Err(LayoutError::Overlap);
        }
        result.insert(*id, point);
    }
    let edges = screen_edges(&result);
    if !edges.is_subset(trusted_edges) {
        return Err(LayoutError::MissingTrustedEdge);
    }
    let mut reached = BTreeSet::from([*result.keys().next().ok_or(LayoutError::MemberCount)?]);
    loop {
        let before = reached.len();
        for (a, b) in &edges {
            if reached.contains(a) || reached.contains(b) {
                reached.insert(*a);
                reached.insert(*b);
            }
        }
        if reached.len() == before {
            break;
        }
    }
    if reached.len() != result.len() {
        return Err(LayoutError::Disconnected);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (WorkspaceLayout, BTreeSet<DeviceId>, BTreeSet<Edge>) {
        let positions = BTreeMap::from([
            ([1; 32], GridPoint { x: 0, y: 0 }),
            ([2; 32], GridPoint { x: 0, y: -1 }),
            ([3; 32], GridPoint { x: 1, y: 0 }),
        ]);
        let members = BTreeSet::from([[1; 32], [2; 32], [3; 32], [4; 32], [5; 32]]);
        let edges = members
            .iter()
            .flat_map(|a| {
                members
                    .iter()
                    .filter(move |b| a < *b)
                    .map(move |b| (*a, *b))
            })
            .collect();
        (
            WorkspaceLayout::new(
                [9; 16],
                LayoutRevision {
                    counter: 1,
                    writer: [1; 32],
                },
                &positions,
                &members,
                &edges,
            )
            .unwrap(),
            members,
            edges,
        )
    }

    #[test]
    fn swap_moves_both_devices_and_preserves_local_views() {
        let (layout, members, edges) = fixture();
        let edit = layout
            .place([2; 32], GridPoint { x: 1, y: 0 }, [1; 32], &members, &edges)
            .unwrap();
        assert_eq!(edit.change, PlacementChange::Swap { displaced: [3; 32] });
        assert_eq!(
            edit.candidate.direction([1; 32], [2; 32]),
            Some(PeerPlacement::Right)
        );
        assert_eq!(
            edit.candidate.direction([2; 32], [1; 32]),
            Some(PeerPlacement::Left)
        );
        assert_eq!(
            edit.candidate.local_view([2; 32]).unwrap()[&[3; 32]],
            (-1, -1)
        );
        assert_eq!(
            layout.direction([1; 32], [2; 32]),
            Some(PeerPlacement::Above)
        );
        assert_eq!(edit.candidate.revision.counter, 2);
        assert_eq!(edit.redirected_edges.len(), 2);
    }

    #[test]
    fn replacement_unplaces_occupant_and_reports_changed_edges() {
        let (layout, members, edges) = fixture();
        let edit = layout
            .place([4; 32], GridPoint { x: 1, y: 0 }, [1; 32], &members, &edges)
            .unwrap();
        assert_eq!(edit.change, PlacementChange::Replace { removed: [3; 32] });
        assert!(!edit.candidate.positions.contains_key(&[3; 32]));
        assert!(members.contains(&[3; 32]));
        assert_eq!(edit.removed_edges, BTreeSet::from([edge([1; 32], [3; 32])]));
        assert_eq!(edit.added_edges, BTreeSet::from([edge([1; 32], [4; 32])]));
    }

    #[test]
    fn invalid_move_does_not_modify_original() {
        let (layout, members, edges) = fixture();
        let original = layout.clone();
        assert_eq!(
            layout.place([2; 32], GridPoint { x: 4, y: 4 }, [1; 32], &members, &edges),
            Err(LayoutError::Disconnected)
        );
        assert_eq!(layout, original);
    }

    #[test]
    fn canonical_digest_survives_wire_order_and_translation() {
        let (layout, members, edges) = fixture();
        let bytes = layout.encode().unwrap();
        let mut wire: LayoutWire = serde_json::from_slice(&bytes).unwrap();
        wire.positions.reverse();
        for (_, p) in &mut wire.positions {
            p.x += 2;
            p.y += 2;
        }
        assert_eq!(
            WorkspaceLayout::decode(&serde_json::to_vec(&wire).unwrap(), &members, &edges).unwrap(),
            layout
        );
    }

    #[test]
    fn wire_rejects_duplicate_unknown_fields_and_tampering() {
        let (layout, members, edges) = fixture();
        let mut wire: LayoutWire = serde_json::from_slice(&layout.encode().unwrap()).unwrap();
        wire.positions.push(wire.positions[0]);
        assert_eq!(
            WorkspaceLayout::decode(&serde_json::to_vec(&wire).unwrap(), &members, &edges),
            Err(LayoutError::DuplicateDevice)
        );
        wire.positions.pop();
        wire.digest[0] ^= 1;
        assert_eq!(
            WorkspaceLayout::decode(&serde_json::to_vec(&wire).unwrap(), &members, &edges),
            Err(LayoutError::DigestMismatch)
        );
        let mut value: serde_json::Value =
            serde_json::from_slice(&layout.encode().unwrap()).unwrap();
        value["extra"] = true.into();
        assert_eq!(
            WorkspaceLayout::decode(&serde_json::to_vec(&value).unwrap(), &members, &edges),
            Err(LayoutError::InvalidEncoding)
        );
    }

    #[test]
    fn equal_revision_different_digest_is_a_fault_and_concurrent_winner_is_stable() {
        let (layout, members, edges) = fixture();
        let a = layout
            .place([2; 32], GridPoint { x: 1, y: 0 }, [1; 32], &members, &edges)
            .unwrap()
            .candidate;
        let b = layout
            .place(
                [2; 32],
                GridPoint { x: -1, y: 0 },
                [2; 32],
                &members,
                &edges,
            )
            .unwrap()
            .candidate;
        assert_eq!(a.compare(&b), Ok(SnapshotOrder::Newer));
        assert_eq!(b.compare(&a), Ok(SnapshotOrder::Older));
        let conflict = WorkspaceLayout::new(
            layout.workspace_id,
            a.revision,
            &layout.positions,
            &members,
            &edges,
        )
        .unwrap();
        assert_eq!(a.compare(&conflict), Err(LayoutError::DigestMismatch));
    }

    #[test]
    fn geometry_rejects_overlap_unknown_missing_trust_bounds_and_excess_members() {
        let (layout, members, edges) = fixture();
        let mut positions = layout.positions.clone();
        positions.insert([2; 32], GridPoint { x: 0, y: 0 });
        assert_eq!(
            normalize(&positions, &members, &edges),
            Err(LayoutError::Overlap)
        );
        assert_eq!(
            normalize(&layout.positions, &BTreeSet::from([[1; 32]]), &edges),
            Err(LayoutError::UnknownDevice)
        );
        assert_eq!(
            normalize(&layout.positions, &members, &BTreeSet::new()),
            Err(LayoutError::MissingTrustedEdge)
        );
        positions.insert([2; 32], GridPoint { x: 127, y: 0 });
        assert_eq!(
            normalize(&positions, &members, &edges),
            Err(LayoutError::CoordinateBounds)
        );
        let mut too_many = members.clone();
        too_many.insert([6; 32]);
        assert_eq!(
            normalize(&layout.positions, &too_many, &edges),
            Err(LayoutError::MemberCount)
        );
    }

    #[test]
    fn network_triangle_is_an_l_shaped_screen_graph() {
        let positions = BTreeMap::from([
            ([1; 32], GridPoint { x: 0, y: 0 }),
            ([2; 32], GridPoint { x: 0, y: -1 }),
            ([3; 32], GridPoint { x: 1, y: 0 }),
        ]);
        let trusted = BTreeSet::from([
            edge([1; 32], [2; 32]),
            edge([1; 32], [3; 32]),
            edge([2; 32], [3; 32]),
        ]);
        let normalized =
            normalize(&positions, &positions.keys().copied().collect(), &trusted).unwrap();
        assert_eq!(screen_edges(&normalized).len(), 2);
        assert!(!screen_edges(&normalized).contains(&edge([2; 32], [3; 32])));
    }

    #[test]
    fn rectangle_remains_a_legal_screen_cycle_after_translation() {
        let positions = BTreeMap::from([
            ([1; 32], GridPoint { x: 4, y: 4 }),
            ([2; 32], GridPoint { x: 5, y: 4 }),
            ([3; 32], GridPoint { x: 5, y: 5 }),
            ([4; 32], GridPoint { x: 4, y: 5 }),
        ]);
        let edges = screen_edges(&positions);
        assert_eq!(edges.len(), 4);
        let normalized =
            normalize(&positions, &positions.keys().copied().collect(), &edges).unwrap();
        assert_eq!(normalized[&[1; 32]], GridPoint { x: 0, y: 0 });
        assert_eq!(screen_edges(&normalized), edges);
    }

    #[test]
    fn concurrent_revision_has_a_total_order() {
        assert!(
            LayoutRevision {
                counter: 7,
                writer: [2; 32]
            } > LayoutRevision {
                counter: 7,
                writer: [1; 32]
            }
        );
        assert!(
            LayoutRevision {
                counter: 8,
                writer: [1; 32]
            } > LayoutRevision {
                counter: 7,
                writer: [2; 32]
            }
        );
    }
}
