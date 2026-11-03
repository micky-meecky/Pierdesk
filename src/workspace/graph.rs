//! Connection trust is independent of screen geometry and current reachability.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{DeviceId, Edge, LayoutError, edge};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustAdvertisement {
    pub schema: u32,
    pub revision: u64,
    pub trusted_members: Vec<DeviceId>,
}

#[derive(Clone)]
struct Claim {
    generation: u64,
    revision: u64,
    neighbors: BTreeSet<DeviceId>,
}

/// Remote statements are accepted only from their own authenticated connection.
/// No statement grants this computer trust in another computer.
pub struct ConnectionGraph {
    local: DeviceId,
    members: BTreeSet<DeviceId>,
    local_trust: BTreeSet<DeviceId>,
    claims: BTreeMap<DeviceId, Claim>,
}

impl ConnectionGraph {
    /// Create a workspace graph using locally stored, explicit pairing records.
    ///
    /// # Errors
    /// Rejects invalid members, self trust, and trust outside this workspace.
    pub fn new(
        local: DeviceId,
        members: BTreeSet<DeviceId>,
        local_trust: BTreeSet<DeviceId>,
    ) -> Result<Self, LayoutError> {
        if members.is_empty() || members.len() > 5 {
            return Err(LayoutError::MemberCount);
        }
        if local == [0; 32] || members.contains(&[0; 32]) {
            return Err(LayoutError::InvalidIdentity);
        }
        if !members.contains(&local)
            || !local_trust.is_subset(&members)
            || local_trust.contains(&local)
        {
            return Err(LayoutError::UnknownDevice);
        }
        Ok(Self {
            local,
            members,
            local_trust,
            claims: BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn advertisement(&self, revision: u64) -> TrustAdvertisement {
        TrustAdvertisement {
            schema: 1,
            revision,
            trusted_members: self.local_trust.iter().copied().collect(),
        }
    }

    /// Record the authenticated sender's own statement for this connection generation.
    /// The caller supplies the TLS peer identity; the message cannot nominate its author.
    ///
    /// # Errors
    /// Rejects unpaired senders, stale generations/revisions, duplicate members and
    /// inconsistent statements at the same generation and revision.
    pub fn observe(
        &mut self,
        authenticated_peer: DeviceId,
        generation: u64,
        message: &TrustAdvertisement,
    ) -> Result<(), LayoutError> {
        if !self.local_trust.contains(&authenticated_peer) {
            return Err(LayoutError::UnknownDevice);
        }
        if message.schema != 1 {
            return Err(LayoutError::InvalidSchema);
        }
        if generation == 0 || message.revision == u64::MAX {
            return Err(LayoutError::InvalidRevision);
        }
        if message.trusted_members.len() > 4 {
            return Err(LayoutError::MemberCount);
        }
        let neighbors: BTreeSet<_> = message.trusted_members.iter().copied().collect();
        if neighbors.len() != message.trusted_members.len() {
            return Err(LayoutError::DuplicateDevice);
        }
        if neighbors.contains(&authenticated_peer) || !neighbors.is_subset(&self.members) {
            return Err(LayoutError::UnknownDevice);
        }
        if let Some(previous) = self.claims.get(&authenticated_peer) {
            if generation < previous.generation
                || (generation == previous.generation && message.revision < previous.revision)
            {
                return Err(LayoutError::InvalidRevision);
            }
            if generation == previous.generation
                && message.revision == previous.revision
                && neighbors != previous.neighbors
            {
                return Err(LayoutError::DigestMismatch);
            }
        }
        self.claims.insert(
            authenticated_peer,
            Claim {
                generation,
                revision: message.revision,
                neighbors,
            },
        );
        Ok(())
    }

    /// Revoke one local pairing without changing unrelated remote statements.
    pub fn revoke(&mut self, peer: DeviceId) {
        self.local_trust.remove(&peer);
        self.claims.remove(&peer);
    }

    /// Refresh only this computer's pairing records; preserve other valid statements.
    ///
    /// # Errors
    /// Rejects self trust or a device outside the workspace.
    pub fn replace_local_trust(
        &mut self,
        trusted: BTreeSet<DeviceId>,
    ) -> Result<bool, LayoutError> {
        if trusted.contains(&self.local) || !trusted.is_subset(&self.members) {
            return Err(LayoutError::UnknownDevice);
        }
        let changed = self.local_trust != trusted;
        self.claims.retain(|id, _| trusted.contains(id));
        self.local_trust = trusted;
        Ok(changed)
    }

    /// Only reciprocal statements produce an edge. Offline devices may retain
    /// their last authenticated statement; transport liveness gates crossing separately.
    #[must_use]
    pub fn trusted_edges(&self) -> BTreeSet<Edge> {
        let mut claims = BTreeMap::from([(self.local, &self.local_trust)]);
        claims.extend(
            self.claims
                .iter()
                .map(|(id, claim)| (*id, &claim.neighbors)),
        );
        let mut edges = BTreeSet::new();
        for (a, neighbors) in &claims {
            for b in *neighbors {
                if claims.get(b).is_some_and(|reverse| reverse.contains(a)) {
                    edges.insert(edge(*a, *b));
                }
            }
        }
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> ConnectionGraph {
        ConnectionGraph::new(
            [1; 32],
            BTreeSet::from([[1; 32], [2; 32], [3; 32]]),
            BTreeSet::from([[2; 32], [3; 32]]),
        )
        .unwrap()
    }

    fn claim(neighbors: Vec<DeviceId>) -> TrustAdvertisement {
        TrustAdvertisement {
            schema: 1,
            revision: 1,
            trusted_members: neighbors,
        }
    }

    #[test]
    fn pairing_two_peers_does_not_invent_their_mutual_trust() {
        let mut graph = graph();
        assert!(graph.trusted_edges().is_empty());
        graph
            .observe([2; 32], 1, &claim(vec![[1; 32], [3; 32]]))
            .unwrap();
        graph.observe([3; 32], 1, &claim(vec![[1; 32]])).unwrap();
        assert_eq!(
            graph.trusted_edges(),
            BTreeSet::from([edge([1; 32], [2; 32]), edge([1; 32], [3; 32])])
        );
        graph
            .observe(
                [3; 32],
                1,
                &TrustAdvertisement {
                    revision: 2,
                    ..claim(vec![[1; 32], [2; 32]])
                },
            )
            .unwrap();
        assert_eq!(graph.trusted_edges().len(), 3);
    }

    #[test]
    fn stale_and_contradictory_claims_leave_the_last_valid_graph_unchanged() {
        let mut graph = graph();
        let statement = claim(vec![[1; 32]]);
        graph.observe([2; 32], 2, &statement).unwrap();
        let before = graph.trusted_edges();
        assert_eq!(
            graph.observe([2; 32], 1, &statement),
            Err(LayoutError::InvalidRevision)
        );
        assert_eq!(
            graph.observe([2; 32], 2, &claim(vec![[1; 32], [3; 32]])),
            Err(LayoutError::DigestMismatch)
        );
        assert_eq!(graph.trusted_edges(), before);
        graph.observe([2; 32], 3, &claim(vec![])).unwrap();
        assert!(graph.trusted_edges().is_empty());
    }

    #[test]
    fn claims_cannot_introduce_unknown_peers_or_duplicate_members() {
        let mut graph = graph();
        assert_eq!(
            graph.observe([4; 32], 1, &claim(vec![[1; 32]])),
            Err(LayoutError::UnknownDevice)
        );
        assert_eq!(
            graph.observe([2; 32], 1, &claim(vec![[1; 32], [4; 32]])),
            Err(LayoutError::UnknownDevice)
        );
        assert_eq!(
            graph.observe([2; 32], 1, &claim(vec![[1; 32], [1; 32]])),
            Err(LayoutError::DuplicateDevice)
        );
        assert!(graph.trusted_edges().is_empty());
    }

    #[test]
    fn revocation_removes_that_peers_evidence_and_preserves_unrelated_trust() {
        let mut graph = graph();
        graph
            .observe([2; 32], 1, &claim(vec![[1; 32], [3; 32]]))
            .unwrap();
        graph
            .observe([3; 32], 1, &claim(vec![[1; 32], [2; 32]]))
            .unwrap();
        graph.revoke([2; 32]);
        assert_eq!(
            graph.trusted_edges(),
            BTreeSet::from([edge([1; 32], [3; 32])])
        );
        assert!(graph.observe([2; 32], 2, &claim(vec![[1; 32]])).is_err());
    }
}
