//! Reviewable layout edits bind the preview to the exact original snapshot.
use std::collections::BTreeSet;

use crate::cross_screen::PeerPlacement;
use crate::security::CertificateFingerprint;

use super::{DeviceId, Edge, GridPoint, LayoutEdit, LayoutError, PlacementChange, WorkspaceLayout};

pub struct LayoutPreview {
    base: WorkspaceLayout,
    edit: LayoutEdit,
}

impl LayoutPreview {
    /// Prepare a move in the local centered view without changing global coordinates.
    ///
    /// # Errors
    /// Rejects absent local devices and invalid resulting layouts.
    pub fn at_neighbor(
        base: &WorkspaceLayout,
        local: DeviceId,
        device: DeviceId,
        direction: PeerPlacement,
        members: &BTreeSet<DeviceId>,
        edges: &BTreeSet<Edge>,
    ) -> Result<Self, LayoutError> {
        if local == device {
            return Err(LayoutError::UnknownDevice);
        }
        let center = base
            .positions()
            .get(&local)
            .ok_or(LayoutError::UnknownDevice)?;
        let (dx, dy) = match direction {
            PeerPlacement::Left => (-1, 0),
            PeerPlacement::Right => (1, 0),
            PeerPlacement::Above => (0, -1),
            PeerPlacement::Below => (0, 1),
        };
        let target = GridPoint {
            x: center
                .x
                .checked_add(dx)
                .ok_or(LayoutError::CoordinateBounds)?,
            y: center
                .y
                .checked_add(dy)
                .ok_or(LayoutError::CoordinateBounds)?,
        };
        Ok(Self {
            base: base.clone(),
            edit: base.place(device, target, local, members, edges)?,
        })
    }

    /// Revalidate at confirmation time; never reinterpret an old preview against a new map.
    ///
    /// # Errors
    /// Rejects changed base snapshots or trust revoked since preview generation.
    pub fn confirm(
        &self,
        current: &WorkspaceLayout,
        members: &BTreeSet<DeviceId>,
        edges: &BTreeSet<Edge>,
    ) -> Result<WorkspaceLayout, LayoutError> {
        if current != &self.base {
            return Err(LayoutError::InvalidRevision);
        }
        WorkspaceLayout::decode(&self.edit.candidate.encode()?, members, edges)
    }

    #[must_use]
    pub fn settings_model(&self) -> serde_json::Value {
        let fingerprint = |id| CertificateFingerprint::from_bytes(id).to_string();
        let (operation, displaced) = match self.edit.change {
            PlacementChange::Move => ("move", None),
            PlacementChange::Swap { displaced } => ("swap", Some(fingerprint(displaced))),
            PlacementChange::Replace { removed } => ("replace", Some(fingerprint(removed))),
        };
        let describe = |layout: &WorkspaceLayout, edges: &BTreeSet<Edge>| {
            edges.iter().map(|(a,b)| serde_json::json!({"from":fingerprint(*a),"to":fingerprint(*b),"direction":layout.direction(*a,*b).map(PeerPlacement::label)})).collect::<Vec<_>>()
        };
        let removed: BTreeSet<_> = self
            .edit
            .removed_edges
            .union(&self.edit.redirected_edges)
            .copied()
            .collect();
        let added: BTreeSet<_> = self
            .edit
            .added_edges
            .union(&self.edit.redirected_edges)
            .copied()
            .collect();
        serde_json::json!({
            "operation":operation,"displaced":displaced,
            "baseRevision":self.base.revision().counter,"baseDigest":self.base.digest(),
            "removedEdges":describe(&self.base,&removed),"addedEdges":describe(&self.edit.candidate,&added),
            "positions": self.edit.candidate.positions().iter().map(|(id,p)| serde_json::json!({"fingerprint":fingerprint(*id),"x":p.x,"y":p.y})).collect::<Vec<_>>()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{LayoutRevision, edge};
    use std::collections::BTreeMap;

    fn fixture() -> (WorkspaceLayout, BTreeSet<DeviceId>, BTreeSet<Edge>) {
        let members = BTreeSet::from([[1; 32], [2; 32], [3; 32], [4; 32]]);
        let edges = members
            .iter()
            .flat_map(|a| {
                members
                    .iter()
                    .filter(move |b| a < *b)
                    .map(move |b| edge(*a, *b))
            })
            .collect();
        let positions = BTreeMap::from([
            ([1; 32], GridPoint { x: 0, y: 0 }),
            ([2; 32], GridPoint { x: 0, y: -1 }),
            ([3; 32], GridPoint { x: 1, y: 0 }),
        ]);
        (
            WorkspaceLayout::new(
                [1; 16],
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
    fn swap_preview_reports_redirected_edges_and_confirmation_is_exact() {
        let (base, members, edges) = fixture();
        let preview = LayoutPreview::at_neighbor(
            &base,
            [1; 32],
            [2; 32],
            PeerPlacement::Right,
            &members,
            &edges,
        )
        .unwrap();
        let model = preview.settings_model();
        assert_eq!(model["operation"], "swap");
        assert_eq!(model["removedEdges"].as_array().unwrap().len(), 2);
        assert_eq!(model["addedEdges"].as_array().unwrap().len(), 2);
        let changed = preview.confirm(&base, &members, &edges).unwrap();
        assert_eq!(
            changed.direction([1; 32], [2; 32]),
            Some(PeerPlacement::Right)
        );
        assert_eq!(
            preview.confirm(&changed, &members, &edges),
            Err(LayoutError::InvalidRevision)
        );
    }

    #[test]
    fn replacement_preview_explicitly_identifies_the_removed_device() {
        let (base, members, edges) = fixture();
        let preview = LayoutPreview::at_neighbor(
            &base,
            [1; 32],
            [4; 32],
            PeerPlacement::Right,
            &members,
            &edges,
        )
        .unwrap();
        assert_eq!(preview.settings_model()["operation"], "replace");
        assert_eq!(
            preview.settings_model()["displaced"],
            CertificateFingerprint::from_bytes([3; 32]).to_string()
        );
        assert!(
            !preview
                .confirm(&base, &members, &edges)
                .unwrap()
                .positions()
                .contains_key(&[3; 32])
        );
        assert!(members.contains(&[3; 32]));
    }

    #[test]
    fn trust_changes_between_preview_and_confirmation_are_revalidated() {
        let (base, members, mut edges) = fixture();
        let preview = LayoutPreview::at_neighbor(
            &base,
            [1; 32],
            [2; 32],
            PeerPlacement::Right,
            &members,
            &edges,
        )
        .unwrap();
        edges.remove(&edge([1; 32], [2; 32]));
        assert_eq!(
            preview.confirm(&base, &members, &edges),
            Err(LayoutError::MissingTrustedEdge)
        );
    }
}
